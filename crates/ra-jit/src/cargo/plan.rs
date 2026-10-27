//! What the daemon is asked to build a program from: the crates of the workspace that are in
//! it, the files rustc built to read symbols from, and rustc's link line with the daemon's
//! objects ahead of rustc's.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::{package, units::Record};
use crate::weaken;

/// What an ELF linker is told so that the first definition of a name is the one that stays.
const FIRST_DEFINITION_STAYS: &str = "-Wl,--allow-multiple-definition";

/// What Apple's linker is told so that the loader finds what it fixes up in tables, as the
/// splice reads and writes them, and not in chains through the pointers themselves. The
/// linker makes chains for a deployment target of macOS 12 or later, as nix's shell sets, and
/// `cc` gives a line that names none the version of the Mac it runs on. The links of
/// [`crate::link`] ask for it too.
pub(crate) const NO_FIXUP_CHAINS: &str = "-Wl,-no_fixup_chains";

/// The room Apple's linker is told to leave after the load commands when nothing says more,
/// so that a splice has room there for its two segments and for the unwind table, which
/// makes way for the patch's frames in `__TEXT`: that of a program of some hundred thousand
/// functions. A program that needs more is linked again with what it needs, which is kept in
/// [`headerpad_file`].
const HEADERPAD: u64 = 0x100000;

/// The same for rustc's links of a program, which nothing links again with more: the first
/// edits are spliced into them. Room for the unwind table of a program of some four hundred
/// thousand functions, as nushell is twice over; one of more is built in full for its first
/// edit. The room is zeros the program never touches: it costs its size on disk.
const RUSTC_HEADERPAD: u64 = 0x400000;

/// How the room is asked for.
const HEADERPAD_FLAG: &str = "-Wl,-headerpad,";

/// What a driver of the `cc` kind is told to take out code nothing reaches with, alone or
/// passed to the linker: `--gc-sections` of an ELF linker, `-dead_strip` of Apple's.
const STRIPS_UNREACHED: [&str; 2] = ["--gc-sections", "-dead_strip"];

/// How a driver of the `cc` kind passes the linker arguments, separated by commas.
const TO_THE_LINKER: &str = "-Wl,";

/// How `link.exe` is told what to take out, in options separated by commas: `REF` takes out
/// code nothing reaches, `NOREF` keeps it.
const OPT: &str = "/OPT:";

/// The same for `link.exe`, which warns of every name it does so for unless told not to.
const FIRST_DEFINITION_STAYS_MSVC: [&str; 2] = ["/FORCE:MULTIPLE", "/IGNORE:4006"];

/// What a build script says to have `link.exe` fail on a warning. It warns of every image
/// it writes with `/FORCE` and can't be told not to, so the daemon's link leaves this out.
const WARNINGS_FAIL: &str = "/WX";

/// How `link.exe` is told where to write.
const OUT: &str = "/OUT:";

/// The stack of the first thread, when the line asks for none. `link.exe` gives it 1 MiB,
/// an eighth of what it has elsewhere, and a function compiled here can have a frame twice
/// that of rustc's: one of clap's that rustc gives 480 KiB overflows it at once.
const STACK: &str = "/STACK:";
const STACK_OF_MAIN: &str = "/STACK:8388608";

/// rustc has the executable name its symbols by file name alone, which finds them next
/// to it. The daemon's are not next to where the executable is installed.
const SYMBOLS_BY_NAME: &str = "/PDBALTPATH:%_PDB%";

/// The crates of the standard library whose symbols the daemon reads.
const LIBRARY: [&str; 3] = ["core", "alloc", "std"];

/// A program as the daemon builds it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Plan {
    /// The root files of the crates of the workspace that ra-jit compiles, the program's own
    /// first.
    pub(crate) crates: Vec<PathBuf>,
    /// The object with all of ra-jit's code for the program.
    pub(crate) object: PathBuf,
    /// The object with what an edit changed.
    pub(crate) patch: PathBuf,
    /// The executable the daemon links and splices.
    pub(crate) binary: PathBuf,
    /// The executable rustc linked, which the daemon's first edits are patches of.
    #[serde(default)]
    pub(crate) rustc_binary: Option<PathBuf>,
    /// The command that links `binary`, for a shell.
    pub(crate) link_cmd: String,
    pub(crate) symbol_files: Vec<PathBuf>,
    /// Whether the program's own crate is compiled with `cfg(test)` and is not a test target:
    /// the unit tests of a library or a binary.
    pub(crate) unit_test: bool,
    /// The source of the libraries of the workspace a unit test is linked with as rustc built
    /// them, without `cfg(test)`: a change to their tests alone leaves rustc's copy as it is.
    pub(crate) linked_as_built: Vec<PathBuf>,
    /// The copies of rustc's output the plan names, which have to stay.
    pub(crate) copies: Vec<PathBuf>,
    /// The package rustc wrote of what a debugger reads of the program it linked, when the
    /// profile keeps that in one (`split-debuginfo = "packed"`).
    #[serde(default)]
    pub(crate) package: Option<PathBuf>,
    /// The newest record the plan was made from.
    pub(crate) stamp: u128,
    /// Whether the program is built whole by the daemon, every crate of it compiled and
    /// `binary` linked by blaze, with nothing of rustc's (docs/no-rustc.md, step K).
    #[serde(default)]
    pub(crate) whole: bool,
}

/// The library of the workspace among `records` that wrote the file a link line names. The
/// file is told by what it is and not by its name: a target directory reached through a
/// link has two names for it.
fn member_that_wrote<'a>(records: &'a [Record], file: &Path) -> Option<&'a Record> {
    if file.extension().is_none_or(|it| it != "rlib") {
        return None;
    }
    let is_file = |it: &PathBuf| it == file || same_file::is_same_file(it, file).unwrap_or(false);
    records.iter().find(|it| it.is_rlib() && it.outputs().iter().any(is_file))
}

impl Plan {
    /// Plans the program `unit` built, with the daemon's files for it in `dir`. `records` are
    /// the units of the workspace. `Err` is why the program is left to cargo.
    pub(crate) fn of(
        unit: &Record,
        records: &[Record],
        dir: &Path,
        unit_test: bool,
    ) -> anyhow::Result<Result<Plan, String>> {
        if let Some(why) = unsupported(unit) {
            return Ok(Err(why));
        }
        let line = unit.link_line.as_ref().context("a linked unit without a link line")?;
        let line =
            std::fs::read_to_string(line).with_context(|| format!("reading {}", line.display()))?;
        let Some(mut link) = LinkLine::parse(&line) else {
            let why = "the link line is not one of a linker driven by `cc` or of `link.exe`";
            return Ok(Err(why.to_owned()));
        };
        let mut patched_as_built = !unit_test && !unit.is_test() && calls_by_symbol(unit);
        let mut plan = Plan {
            crates: vec![unit.source.clone()],
            object: dir.join("ra-jit.o"),
            patch: dir.join("patch.o"),
            binary: dir.join("bin"),
            rustc_binary: None,
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            unit_test,
            linked_as_built: Vec::new(),
            copies: Vec::new(),
            package: (unit.codegen("split-debuginfo") == Some("packed"))
                .then(|| link.output().map(|it| package::beside(Path::new(it))))
                .flatten(),
            stamp: unit.stamp,
            whole: false,
        };
        // rustc's object of the names the program keeps is moved to its copy, which stays when
        // the target directory is cleaned. GNU ld takes the panic runtime by it.
        link.args.retain(|it| {
            !is_symbols(it) || unit.copy_of(Path::new(it)).is_some() || Path::new(it).exists()
        });
        for arg in &mut link.args {
            let path = PathBuf::from(&*arg);
            let member = member_that_wrote(records, &path);
            let Some(owner) = member.or_else(|| unit.copy_of(&path).map(|_| unit)) else {
                continue;
            };
            if let Some(why) = unsupported(owner) {
                return Ok(Err(why));
            }
            let Some(copy) = owner.copy_of(&path) else {
                return Ok(Err(format!("no copy of {} was kept", path.display())));
            };
            plan.copies.push(copy.to_owned());
            plan.stamp = plan.stamp.max(owner.stamp);
            *arg = copy.display().to_string();
            if member.is_none() {
                continue;
            }
            patched_as_built &= calls_by_symbol(owner);
            // The libraries of a unit test are rustc's until an edit reaches one, which the
            // daemon then takes over, built as rustc built it, without `cfg(test)`.
            match unit_test {
                true => plan.linked_as_built.extend(owner.inputs().files),
                false => plan.crates.push(owner.source.clone()),
            }
        }
        plan.symbol_files = link.rustc_files();
        plan.rustc_binary = link.output().filter(|_| patched_as_built).map(PathBuf::from);
        std::fs::create_dir_all(dir)?;
        match unit.is_test() {
            true => std::fs::write(dir.join(TESTS), harness_alone(unit).unwrap_or_default())?,
            false => _ = std::fs::remove_file(dir.join(TESTS)),
        }
        link.give_way(&plan.copies)?;
        let apple = link.flavor == Flavor::Apple;
        plan.link_cmd =
            link.command(&[&plan.patch, &plan.object], &plan.binary, &dir.join("link.args"))?;
        if let Some(entitlements) = entitlements(&unit.source).filter(|_| apple) {
            plan.link_cmd = format!(
                "{} && codesign --force --sign - --entitlements {} {}",
                plan.link_cmd,
                shlex::try_quote(&entitlements.display().to_string())?,
                shlex::try_quote(&plan.binary.display().to_string())?,
            );
        }
        Ok(Ok(plan))
    }
}

/// The entitlements the package of the crate with root file `source` asks its executable to
/// be signed with, as `[package.metadata.ra-jit] entitlements = "<file>"` in its manifest.
/// The executable the daemon links is its own, which cargo copies over the one in the target
/// directory, so a program that has to be signed is signed by the link.
fn entitlements(source: &Path) -> Option<PathBuf> {
    let package = source.ancestors().skip(1).find(|it| it.join("Cargo.toml").is_file())?;
    let manifest = std::fs::read_to_string(package.join("Cargo.toml")).ok()?;
    let manifest: toml::Table = toml::from_str(&manifest).ok()?;
    let file =
        manifest.get("package")?.get("metadata")?.get("ra-jit")?.get("entitlements")?.as_str()?;
    Some(package.join(file))
}

/// Whether rustc compiled `unit` with one copy of each function that is not generic or
/// `#[inline]`, which every call goes to: no inlining, and nothing made private to an object.
fn calls_by_symbol(unit: &Record) -> bool {
    unit.codegen("opt-level").unwrap_or("0") == "0"
        && unit.codegen("incremental").is_some()
        && !unit.stands_in()
}

/// The file in a program's directory that says it is a program of tests, which the daemon
/// starts in ra-jit's test harness. It holds why the daemon builds it only with that harness,
/// when it does: [`harness_alone`].
pub(crate) const TESTS: &str = "tests";

/// The file in a program of tests' directory that says `cargo ra-jit test` runs only the
/// tests an edit reaches, for the daemon to write [`REACH`] at each build of it.
pub(crate) const REACH_WANTED: &str = "tests-reach-wanted";

/// What the daemon wrote of each test of the program at its last build, one a line: a hash of
/// what decides how the test ends, a tab and libtest's name of it; or `-`, the name, a tab,
/// why the test is run every time and, when calls lead to that, a tab and the calls from the
/// test, joined by ` -> `; or `*`, a tab and why every test is. A test that reaches the world
/// outside the program has `~` before its hash, then why and the calls as `-` has them: what
/// it read there, which a sandbox records, decides the rest. Written whole, or not there.
pub(crate) const REACH: &str = "tests-reach";

/// Why the tests of `unit` are only right as rustc built them under rustc's harness, if they
/// are. rustc's harness, where it stays in a test executable, calls the tests and their
/// helpers, so each has to be a symbol the daemon's copy can take the place of, called the
/// way the source says. ra-jit's harness calls the daemon's copies.
fn harness_alone(unit: &Record) -> Option<String> {
    let name = &unit.crate_name;
    if unit.codegen("incremental").is_none() {
        return Some(format!(
            "the tests of `{name}` are built without incremental compilation, where rustc \
             keeps them private to its object"
        ));
    }
    let level = unit.codegen("opt-level").unwrap_or("0");
    (level != "0").then(|| {
        format!(
            "the tests of `{name}` are built with opt-level {level}, where rustc inlines \
             them into its harness and changes how it calls their helpers"
        )
    })
}

/// Why ra-jit has no part in a crate that was compiled with these flags, when it has none.
/// What a profile sets (the level, the checks, the panic strategy) the daemon reads from
/// cargo itself, and refuses there what it does not build.
fn unsupported(unit: &Record) -> Option<String> {
    let name = &unit.crate_name;
    let off = |value: Option<&str>| matches!(value, None | Some("off" | "no" | "n" | "false"));
    if !off(unit.codegen("lto")) || unit.codegen("linker-plugin-lto").is_some() {
        return Some(format!("`{name}` is built with LTO"));
    }
    for flag in ["instrument-coverage", "profile-generate", "profile-use"] {
        if unit.codegen(flag).is_some() {
            return Some(format!("`{name}` is built with -C {flag}"));
        }
    }
    if let Some(why) = crate::unstable::refusal(&unit.args) {
        return Some(format!("`{name}` is built with {why}"));
    }
    None
}

/// The line rustc ran the linker with: the variables it set, the driver, the arguments.
#[derive(Clone)]
pub(crate) struct LinkLine {
    /// What the line starts with to run the driver without some variables: `env -u NAME`.
    without: Vec<String>,
    env: Vec<(String, String)>,
    linker: String,
    args: Vec<String>,
    flavor: Flavor,
}

/// The kinds of linker the daemon can put its objects ahead of rustc's for.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Flavor {
    /// An ELF linker driven by `cc`.
    Cc,
    /// Apple's linker driven by `cc`, which tells it its architecture.
    Apple,
    /// `link.exe`, or one that takes its arguments.
    Msvc,
}

impl Flavor {
    fn of(args: &[String]) -> Option<Flavor> {
        if args.iter().any(|it| it == "/NOLOGO") {
            return args.iter().any(|it| it.starts_with(OUT)).then_some(Flavor::Msvc);
        }
        let of_cc = args.iter().any(|it| it == "-o") && !args.iter().any(|it| it == "-flavor");
        let flavor = match args.iter().any(|it| it == "-arch") {
            true => Flavor::Apple,
            false => Flavor::Cc,
        };
        of_cc.then_some(flavor)
    }
}

impl LinkLine {
    /// `None` for a line that is of no linker the daemon knows.
    pub(crate) fn parse(line: &str) -> Option<LinkLine> {
        let words = shlex::split(line.lines().find(|it| it.contains('"'))?)?;
        let unsets = match words.first().map(String::as_str) {
            Some("env") => words[1..].chunks(2).take_while(|it| it[0] == "-u").count(),
            _ => 0,
        };
        let (without, words) = words.split_at(if unsets == 0 { 0 } else { 1 + 2 * unsets });
        let first = words.iter().position(|it| !is_assignment(it))?;
        let env = words[..first]
            .iter()
            .filter_map(|it| it.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        let mut args: Vec<String> = words[first + 1..].to_vec();
        let flavor = Flavor::of(&args)?;
        if flavor == Flavor::Apple {
            // The wrapper's come after what cargo asked.
            for added in rustc_link_args() {
                if let Some(at) = args.iter().rposition(|it| *it == added) {
                    args.remove(at);
                }
            }
        }
        let (without, linker) = (without.to_vec(), words[first].clone());
        Some(LinkLine { without, env, linker, args, flavor })
    }

    /// Has what rustc built of the workspace give way to the daemon's by being weak, where
    /// the linker has no flag for the first definition staying: Apple's. Either way the
    /// statics rustc kept private to an object are made ones the daemon's code can refer to
    /// (`symbols::StaticCopy`): on an ELF system in copies of the files that have any.
    fn give_way(&mut self, copies: &[PathBuf]) -> anyhow::Result<()> {
        let made = match self.flavor {
            Flavor::Apple => weaken::copy_of,
            Flavor::Cc => weaken::shared,
            Flavor::Msvc => return Ok(()),
        };
        for arg in &mut self.args {
            // rustc's object of names defines none.
            if !is_symbols(arg) && copies.iter().any(|it| it == Path::new(arg)) {
                *arg = made(Path::new(arg))?.display().to_string();
            }
        }
        Ok(())
    }

    /// The line without rustc's object of the names the program keeps once it is gone:
    /// `-Csave-temps` keeps rustc's directory, and nothing else does.
    #[cfg(test)]
    fn present(mut self) -> LinkLine {
        self.args.retain(|it| !is_symbols(it) || Path::new(it).exists());
        self
    }

    /// The executable the line links.
    fn output(&self) -> Option<&str> {
        let at = self.args.iter().position(|it| it == "-o")?;
        self.args.get(at + 1).map(|it| &**it)
    }

    /// What rustc built that is linked, and the standard library's crates next to it.
    pub(crate) fn rustc_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> =
            self.args.iter().filter(|it| is_rustc_file(it)).map(PathBuf::from).collect();
        let library = files.iter().find_map(|it| {
            it.components().any(|it| it.as_os_str() == "rustlib").then(|| it.parent())?
        });
        let entries = library.and_then(|it| std::fs::read_dir(it).ok()).into_iter().flatten();
        for path in entries.flatten().map(|it| it.path()) {
            let name = path.file_name().and_then(|it| it.to_str()).unwrap_or_default();
            let of_library = LIBRARY.iter().any(|it| {
                name.strip_prefix("lib")
                    .and_then(|rest| rest.strip_prefix(it))
                    .is_some_and(|rest| rest.starts_with('-'))
            });
            if of_library && name.ends_with(".rlib") && !files.contains(&path) {
                files.push(path);
            }
        }
        files
    }

    /// The command that links `binary` with the objects `ahead` before everything rustc
    /// built, with its arguments in the file at `response`: a line with all of them is longer
    /// than a process may be given. Next to it is the file of the line that takes out the
    /// code nothing reaches, as rustc's does ([`reached_only`]).
    pub(crate) fn command(
        mut self,
        ahead: &[&Path],
        binary: &Path,
        response: &Path,
    ) -> anyhow::Result<String> {
        if self.flavor == Flavor::Msvc {
            self.args.retain(|it| !it.eq_ignore_ascii_case(WARNINGS_FAIL));
        }
        self.clone().command_of(ahead, binary, &reached_file(response))?;
        self.keep_unreached();
        self.command_of(ahead, binary, response)
    }

    fn command_of(self, ahead: &[&Path], binary: &Path, response: &Path) -> anyhow::Result<String> {
        let first = self
            .args
            .iter()
            .position(|it| is_rustc_file(it))
            .context("nothing of rustc's is linked")?;
        let ours = ahead.iter().map(|it| it.display().to_string()).collect();
        match self.flavor {
            Flavor::Cc | Flavor::Apple => self.command_of_cc(ours, first, binary, response),
            Flavor::Msvc => self.command_of_msvc(ours, first, binary, response),
        }
    }

    /// Has the linker keep the code nothing in the executable reaches, which rustc's line
    /// has it take out. A patch calls the code of the executable by name, also code the
    /// executable's own did not reach when it was linked (an instance of a generic function
    /// for a type first used in an edit, a function its callers no longer call), and a
    /// splice declines a patch that calls what is not there, so the edit is linked in full.
    fn keep_unreached(&mut self) {
        match self.flavor {
            Flavor::Cc | Flavor::Apple => self.args.retain_mut(|arg| {
                if STRIPS_UNREACHED.contains(&arg.as_str()) {
                    return false;
                }
                let Some(passed) = arg.strip_prefix(TO_THE_LINKER) else { return true };
                let kept: Vec<&str> =
                    passed.split(',').filter(|it| !STRIPS_UNREACHED.contains(it)).collect();
                let any = !kept.is_empty();
                *arg = format!("{TO_THE_LINKER}{}", kept.join(","));
                any
            }),
            Flavor::Msvc => {
                for arg in &mut self.args {
                    let named = arg.get(..OPT.len()).filter(|it| it.eq_ignore_ascii_case(OPT));
                    let Some(options) = named.map(|_| &arg[OPT.len()..]) else { continue };
                    let options: Vec<&str> = (options.split(','))
                        .map(|it| if it.eq_ignore_ascii_case("REF") { "NOREF" } else { it })
                        .collect();
                    *arg = format!("{OPT}{}", options.join(","));
                }
            }
        }
    }

    fn command_of_cc(
        mut self,
        ours: Vec<String>,
        first: usize,
        binary: &Path,
        response: &Path,
    ) -> anyhow::Result<String> {
        let flag = match self.flavor {
            Flavor::Apple => NO_FIXUP_CHAINS,
            _ => FIRST_DEFINITION_STAYS,
        };
        self.args.splice(first..first, ours.into_iter().chain([flag.to_owned()]));
        let output =
            self.args.iter().position(|it| it == "-o").context("the link names no output")?;
        *self.args.get_mut(output + 1).context("the link names no output")? =
            binary.display().to_string();
        let quoted: Vec<String> = self
            .args
            .iter()
            .map(|it| shlex::try_quote(it).map(|it| it.into_owned()))
            .collect::<Result<_, _>>()
            .context("an argument of the link can't be quoted")?;
        std::fs::write(response, quoted.join("\n"))?;
        let mut command = self.without;
        for (name, value) in &self.env {
            command.push(format!("{name}={}", shlex::try_quote(value)?));
        }
        command.push(shlex::try_quote(&self.linker)?.into_owned());
        command.push(shlex::try_quote(&format!("@{}", response.display()))?.into_owned());
        if self.flavor == Flavor::Apple {
            // Last, so that it is what the linker takes: it is at least what the line asks.
            let asked = self.args.iter().filter_map(|it| headerpad_in(it)).last();
            command.push(shlex::try_quote(&headerpad_read(binary, asked)?)?.into_owned());
        }
        Ok(command.join(" "))
    }

    /// rustc tells `link.exe` where the libraries of the system are through `LIB`, which
    /// is not in the line; here the line says it.
    fn command_of_msvc(
        mut self,
        ours: Vec<String>,
        first: usize,
        binary: &Path,
        response: &Path,
    ) -> anyhow::Result<String> {
        let stays = FIRST_DEFINITION_STAYS_MSVC.map(str::to_owned);
        self.args.splice(first..first, ours.into_iter().chain(stays));
        let binary = binary.display();
        for arg in &mut self.args {
            if arg.starts_with(OUT) {
                *arg = format!("{OUT}{binary}");
            } else if arg == SYMBOLS_BY_NAME {
                *arg = format!("/PDBALTPATH:{binary}.pdb");
            }
        }
        if !self.args.iter().any(|it| it.starts_with(STACK)) {
            self.args.push(STACK_OF_MAIN.to_owned());
        }
        self.args.extend(system_libraries().iter().map(|it| format!("/LIBPATH:{}", it.display())));
        if self.args.iter().any(|it| it.contains('"')) {
            anyhow::bail!("an argument of the link can't be quoted");
        }
        // A backslash before the quote that closes an argument would make it part of it.
        let quoted = |arg: &String| {
            let closing = arg.len() - arg.trim_end_matches('\\').len();
            format!("\"{arg}{}\"", "\\".repeat(closing))
        };
        // lld takes its flavour from the command line, which a response file does not give it.
        let flavor: String = match self.args.iter().position(|it| it == "-flavor") {
            Some(at) => self.args.drain(at..at + 2).map(|it| format!("\"{it}\" ")).collect(),
            None => String::new(),
        };
        let quoted: Vec<String> = self.args.iter().map(quoted).collect();
        std::fs::write(response, quoted.join("\n"))?;
        // rustc runs the `rust-lld` a target names from its toolchain, not from the PATH.
        let linker = match self.linker.as_str() {
            "rust-lld" => super::toolchain::rust_lld(),
            _ => PathBuf::from(&self.linker),
        };
        Ok(format!("\"{}\" {flavor}\"@{}\"", linker.display(), response.display()))
    }
}

/// The directories `link.exe` finds the libraries of the compiler and of the system in.
#[cfg(windows)]
fn system_libraries() -> Vec<PathBuf> {
    let Some(linker) = find_msvc_tools::find_tool(std::env::consts::ARCH, "link.exe") else {
        return Vec::new();
    };
    let listed = linker.env().into_iter().find(|(name, _)| name == "LIB");
    let Some((_, dirs)) = listed else { return Vec::new() };
    // A list that ends in its separator has an empty last entry.
    std::env::split_paths(dirs).filter(|it| !it.as_os_str().is_empty()).collect()
}

#[cfg(not(windows))]
fn system_libraries() -> Vec<PathBuf> {
    Vec::new()
}

/// Whether a word of the link line sets a variable for the linker's driver.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'_')
    })
}

fn is_rustc_file(arg: &str) -> bool {
    arg.ends_with(".rlib") || arg.ends_with(".rcgu.o")
}

/// Whether `arg` is the object rustc links first in a program: one that only names what the
/// program has to keep (the panic runtime, the allocator's shims, `#[used]` statics). A
/// linker that reads each archive once, as GNU ld does, needs it to take the panic runtime,
/// which comes before the standard library that calls it.
pub(crate) fn is_symbols(arg: &str) -> bool {
    arg.ends_with("symbols.o")
}

/// The response file next to the link's at `response` of the line that takes out the code
/// nothing reaches, as rustc's does.
fn reached_file(response: &Path) -> PathBuf {
    response.with_extension("reached.args")
}

/// The link command `cmd` with the response file that takes out the code nothing reaches, as
/// rustc's line does, when [`LinkLine::command`] wrote one. The link that keeps it fails when
/// some of it uses what nothing defines, as zed's `webrtc` does what X11's `libXtst` would,
/// which rustc's line never asks for. The executable is linked without that code then, and
/// an edit that calls into what was taken out is linked in full.
pub(crate) fn reached_only(cmd: &str) -> Option<String> {
    let args = shlex::split(cmd)?;
    let (response, reached) = args.iter().find_map(|it| {
        let response = Path::new(it.strip_prefix('@')?);
        let reached = reached_file(response);
        reached.is_file().then_some((response.display().to_string(), reached))
    })?;
    // The response file as the line has it: quoted for a shell after `cc` (where a Windows
    // path's `\` is escaped), in double quotes as it is after `link.exe`.
    let quotings: [fn(&str) -> Option<String>; 2] =
        [|it| Some(shlex::try_quote(it).ok()?.into_owned()), |it| Some(format!("\"{it}\""))];
    quotings.iter().find_map(|quote| {
        let response = quote(&format!("@{response}"))?;
        let reached = quote(&format!("@{}", reached.display()))?;
        cmd.contains(&response).then(|| cmd.replacen(&response, &reached, 1))
    })
}

/// The file the link of the Apple executable at `binary` reads the room after its load
/// commands from: a response file of the one argument that asks for it. The daemon makes it
/// more when a link leaves a splice too little, and the links after start from it.
pub(crate) fn headerpad_file(binary: &Path) -> PathBuf {
    let mut file = binary.as_os_str().to_owned();
    file.push(".headerpad");
    PathBuf::from(file)
}

/// The room the link of the executable at `binary` asks for, when it reads it from a file.
pub(crate) fn headerpad_of(binary: &Path) -> Option<u64> {
    headerpad_in(std::fs::read_to_string(headerpad_file(binary)).ok()?.trim())
}

/// The last argument of a link of the Apple executable at `binary`, which has it read the room
/// from [`headerpad_file`]: what the line `asked` for, or [`HEADERPAD`], and no less than the
/// file says already.
pub(crate) fn headerpad_read(binary: &Path, asked: Option<u64>) -> std::io::Result<String> {
    let kept = headerpad_of(binary);
    let pad = asked.unwrap_or(HEADERPAD).max(kept.unwrap_or(0));
    if kept != Some(pad) {
        write_headerpad(binary, pad)?;
    }
    Ok(format!("@{}", headerpad_file(binary).display()))
}

/// What rustc's link of a program for Apple is told besides what cargo asked, for the
/// daemon's first edits to be spliced into it: the room after the load commands, an address
/// slot of each function of another library (which ld64 makes of each lazy one when
/// everything is bound at load), and fixups in tables. The daemon's own links leave these out
/// of the line.
pub(crate) fn rustc_link_args() -> [String; 3] {
    [
        format!("{HEADERPAD_FLAG}{RUSTC_HEADERPAD:#x}"),
        "-Wl,-bind_at_load".to_owned(),
        NO_FIXUP_CHAINS.to_owned(),
    ]
}

/// What rustc's link of a program for Linux is told besides what cargo asked, and the daemon's
/// links with it: a GNU build ID, by which a program that takes edits while it runs is known
/// (`ra-jit-live`). Not every C compiler has the linker write one (NixOS's does not).
pub(crate) const BUILD_ID: &str = "-Wl,--build-id";

/// What rustc's link of a program for Linux is told to keep of compiler-builtins, for what
/// the daemon compiles to call there: the functions of [`hir_ty_codegen::BUILTINS`], which
/// the linker leaves out when rustc's code calls none of them. A name the target has no
/// function of is no error.
pub(crate) fn kept_builtins() -> String {
    let kept: Vec<String> =
        hir_ty_codegen::BUILTINS.iter().map(|it| format!("--undefined={it}")).collect();
    format!("{TO_THE_LINKER}{}", kept.join(","))
}

pub(crate) fn write_headerpad(binary: &Path, pad: u64) -> std::io::Result<()> {
    std::fs::write(headerpad_file(binary), format!("{HEADERPAD_FLAG}{pad:#x}\n"))
}

/// The room an argument of the driver asks Apple's linker for, which reads it in hexadecimal.
fn headerpad_in(arg: &str) -> Option<u64> {
    let digits = arg.strip_prefix(HEADERPAD_FLAG)?;
    let digits = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")).unwrap_or(digits);
    u64::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::super::units::Copied;
    use super::*;

    const LINE: &str = r#"LC_ALL="C" PATH="/t/bin:/usr/bin" "cc" "-m64" "/t/deps/rustc1/symbols.o" "/t/deps/a.1.rcgu.o" "-Wl,-Bstatic" "/t/deps/libb-2.rlib" "-o" "/t/deps/a-1" "-pie""#;

    #[test]
    fn link_line_of_cc() {
        let link = LinkLine::parse(LINE).unwrap();
        assert_eq!(
            link.env,
            [
                ("LC_ALL".to_owned(), "C".to_owned()),
                ("PATH".to_owned(), "/t/bin:/usr/bin".to_owned())
            ]
        );
        assert_eq!(link.linker, "cc");
        assert_eq!(link.args[..3], ["-m64", "/t/deps/rustc1/symbols.o", "/t/deps/a.1.rcgu.o"]);
        assert_eq!(
            link.rustc_files(),
            ["/t/deps/a.1.rcgu.o", "/t/deps/libb-2.rlib"].map(PathBuf::from)
        );
        assert_eq!(link.present().args[..2], ["-m64", "/t/deps/a.1.rcgu.o"]);
    }

    /// The wrapper moves rustc's object of the names the program keeps out of rustc's
    /// directory, and the daemon links its copy: without it GNU ld does not take the panic
    /// runtime, which comes before the standard library that calls it.
    #[test]
    fn the_names_a_program_keeps_are_linked_from_their_copy() {
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-symbols-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (line, copy) = (dir.join("link.line"), dir.join("symbols.o"));
        std::fs::write(&line, LINE).unwrap();
        std::fs::write(&copy, b"").unwrap();
        let mut program = unit(&["--crate-type", "bin"]);
        program.link_line = Some(line);
        program.copies =
            vec![Copied { original: "/t/deps/rustc1/symbols.o".into(), copy: copy.clone() }];
        let plan = Plan::of(&program, &[], &dir, false).unwrap().unwrap();
        let args = std::fs::read_to_string(dir.join("link.args")).unwrap();
        let args: Vec<String> = args.lines().flat_map(|it| shlex::split(it).unwrap()).collect();
        assert!(args.contains(&copy.display().to_string()), "{args:?}");
        assert!(plan.copies.contains(&copy));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn unit(args: &[&str]) -> Record {
        Record {
            id: "a-1".to_owned(),
            crate_name: "a".to_owned(),
            cwd: PathBuf::from("/ws"),
            args: args.iter().map(|it| it.to_string()).collect(),
            source: PathBuf::from("/ws/src/lib.rs"),
            stamp: 1,
            copies: Vec::new(),
            link_line: None,
            env: Default::default(),
            crate_env: Default::default(),
        }
    }

    #[test]
    fn unstable_flags_that_change_nothing() {
        assert_eq!(unsupported(&unit(&["-Z", "embed-metadata=no"])), None);
        assert_eq!(unsupported(&unit(&["-Zembed-metadata=no", "-Zthreads=8"])), None);
        let threads = unsupported(&unit(&["-Z", "embed-metadata=no", "-Z", "share-generics=y"]));
        assert!(threads.is_some_and(|why| why.contains("-Z share-generics=y")));
        assert!(unsupported(&unit(&["-Zlocation-detail=none"])).is_some());
    }

    #[test]
    fn tests_the_harness_reaches_by_symbol_only() {
        let incremental = "incremental=/t/debug/incremental";
        assert_eq!(harness_alone(&unit(&["--test", "-C", incremental])), None);
        assert_eq!(harness_alone(&unit(&["--test", "-C", incremental, "-C", "opt-level=0"])), None);
        let private = harness_alone(&unit(&["--test"]));
        assert!(private.is_some_and(|why| why.contains("without incremental")));
        let inlined = harness_alone(&unit(&["--test", "-C", incremental, "-C", "opt-level=2"]));
        assert!(inlined.is_some_and(|why| why.contains("opt-level 2")));
        // ra-jit's harness calls the daemon's copies: the program is not refused for them.
        assert_eq!(unsupported(&unit(&["--test"])), None);
        assert_eq!(unsupported(&unit(&["-C", "opt-level=3"])), None);
    }

    #[test]
    fn link_lines_that_are_not_cc_for_elf() {
        assert!(LinkLine::parse("").is_none());
        assert!(LinkLine::parse("no quotes here").is_none());
        assert!(LinkLine::parse(r#""link.exe" "/NOLOGO" "-o""#).is_none());
        assert!(LinkLine::parse(r#""rust-lld" "-flavor" "wasm" "-o" "a.wasm""#).is_none());
        assert!(LinkLine::parse(r#"A="1" "cc" "unterminated"#).is_none());
    }

    #[test]
    fn link_line_of_cc_for_mach_o() {
        let line = r#"env -u IPHONEOS_DEPLOYMENT_TARGET -u TVOS_DEPLOYMENT_TARGET LC_ALL="C" "cc" "/t/deps/a.1.rcgu.o" "-arch" "arm64" "-o" "/t/deps/a-1""#;
        let link = LinkLine::parse(line).unwrap();
        assert_eq!(
            link.without,
            ["env", "-u", "IPHONEOS_DEPLOYMENT_TARGET", "-u", "TVOS_DEPLOYMENT_TARGET"]
        );
        assert_eq!(link.env, [("LC_ALL".to_owned(), "C".to_owned())]);
        assert_eq!(link.linker, "cc");
        assert_eq!(link.flavor, Flavor::Apple);
        assert_eq!(LinkLine::parse(LINE).unwrap().flavor, Flavor::Cc);
    }

    #[test]
    fn rustcs_copies_give_way_for_apple() {
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-apple-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("a.1.rcgu.o");
        std::fs::write(&copy, b"not an object").unwrap();
        let line = format!(
            r#"env -u IPHONEOS_DEPLOYMENT_TARGET LC_ALL="C" "cc" "{}" "/t/libstd-3.rlib" "-arch" "arm64" "-o" "/t/deps/a-1""#,
            copy.display()
        );
        let mut link = LinkLine::parse(&line).unwrap();
        link.give_way(std::slice::from_ref(&copy)).unwrap();
        let (response, binary) = (dir.join("link.args"), dir.join("bin"));
        let command = link.command(&[Path::new("/p/ra-jit.o")], &binary, &response).unwrap();
        assert_eq!(
            shlex::split(&command).unwrap(),
            [
                "env",
                "-u",
                "IPHONEOS_DEPLOYMENT_TARGET",
                "LC_ALL=C",
                "cc",
                &format!("@{}", response.display()),
                &format!("@{}", headerpad_file(&binary).display()),
            ]
        );
        let pad = std::fs::read_to_string(headerpad_file(&binary)).unwrap();
        assert_eq!(pad, "-Wl,-headerpad,0x100000\n");
        let weak = dir.join(weaken::DIR).join("a.1.rcgu.o");
        assert!(weak.exists());
        let args = std::fs::read_to_string(&response).unwrap();
        let args: Vec<String> = args.lines().flat_map(|it| shlex::split(it).unwrap()).collect();
        assert_eq!(
            args,
            [
                "/p/ra-jit.o",
                NO_FIXUP_CHAINS,
                &weak.display().to_string(),
                "/t/libstd-3.rlib",
                "-arch",
                "arm64",
                "-o",
                &binary.display().to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The room after the load commands is what the line asks for, or a default, and never
    /// less than a link of the executable was found to need.
    #[test]
    fn headerpad_of_an_apple_link() {
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-pad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (response, binary) = (dir.join("link.args"), dir.join("bin"));
        let line =
            |pad: &str| format!(r#""cc" "/t/a.1.rcgu.o" "-arch" "arm64" "-o" "/t/a-1" {pad}"#);
        let pad = |line: &str| {
            LinkLine::parse(line).unwrap().command(&[], &binary, &response).unwrap();
            headerpad_of(&binary)
        };
        assert_eq!(pad(&line("")), Some(HEADERPAD));
        std::fs::remove_file(headerpad_file(&binary)).unwrap();
        assert_eq!(pad(&line(r#""-Wl,-headerpad,0x800000""#)), Some(0x800000));
        std::fs::remove_file(headerpad_file(&binary)).unwrap();
        // What the wrapper added to rustc's link is not the daemon's to ask for.
        let wrapper = rustc_link_args().map(|it| format!("{it:?}")).join(" ");
        assert_eq!(pad(&line(&format!(r#""-Wl,-headerpad,0x100" {wrapper}"#))), Some(0x100));
        let args = std::fs::read_to_string(&response).unwrap();
        assert!(!args.contains("bind_at_load") && !args.contains("0x400000"), "{args}");
        std::fs::remove_file(headerpad_file(&binary)).unwrap();
        // Hexadecimal, as ld64 reads it, with or without `0x`; the last one is the one.
        assert_eq!(pad(&line(r#""-Wl,-headerpad,10" "-Wl,-headerpad,200""#)), Some(0x200));
        // What was found to be needed stays, over a line that asks for less.
        write_headerpad(&binary, 0x8000).unwrap();
        assert_eq!(pad(&line(r#""-Wl,-headerpad,0x0""#)), Some(0x8000));
        assert_eq!(pad(&line("")), Some(HEADERPAD));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A patch calls the base object's instances by name, also one the base's own code no
    /// longer reaches, so no link takes out what nothing reaches: not Apple's, an ELF
    /// linker's or `link.exe`.
    #[test]
    fn links_keep_what_a_patch_may_call() {
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-strip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (response, binary) = (dir.join("link.args"), dir.join("bin"));
        let args = |line: &str| {
            LinkLine::parse(line).unwrap().command(&[], &binary, &response).unwrap();
            let args = std::fs::read_to_string(&response).unwrap();
            args.lines().flat_map(|it| shlex::split(it).unwrap()).collect::<Vec<_>>()
        };
        let apple = args(
            r#""cc" "/t/a.1.rcgu.o" "-arch" "arm64" "-Wl,-dead_strip" "-dead_strip" "-o" "/t/a-1""#,
        );
        assert!(!apple.iter().any(|it| it.contains("dead_strip")), "{apple:?}");
        let elf = args(
            r#""cc" "/t/a.1.rcgu.o" "-Wl,--as-needed" "-Wl,--gc-sections" "-Wl,-O1,--gc-sections,-z,now" "-o" "/t/a-1""#,
        );
        let expected = [FIRST_DEFINITION_STAYS, "-Wl,--as-needed", "-Wl,-O1,-z,now"];
        assert_eq!(elf.iter().filter(|it| it.starts_with("-Wl,")).collect::<Vec<_>>(), expected);
        let msvc = r#""link.exe" "/NOLOGO" "C:\\t\\a.1.rcgu.o" "/OPT:REF,NOICF" "/opt:ref" "/OUT:C:\\t\\a.exe""#;
        LinkLine::parse(msvc).unwrap().command(&[], &binary, &response).unwrap();
        let msvc = std::fs::read_to_string(&response).unwrap();
        let opt: Vec<&str> =
            msvc.lines().filter(|it| it.to_ascii_uppercase().contains("/OPT:")).collect();
        assert_eq!(opt, [r#""/OPT:NOREF,NOICF""#, r#""/OPT:NOREF""#]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The line as rustc had it, which takes out what nothing reaches, is next to the one that
    /// keeps it, for a link that fails on what only that code uses.
    #[test]
    fn the_line_without_what_nothing_reaches_is_kept_too() {
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-reached-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (response, binary) = (dir.join("link.args"), dir.join("bin"));
        let line = r#""cc" "/t/a.1.rcgu.o" "-Wl,--gc-sections" "-o" "/t/a-1""#;
        let command = LinkLine::parse(line).unwrap().command(&[], &binary, &response).unwrap();
        let reached = reached_only(&command).unwrap();
        assert_eq!(reached, command.replace("link.args", "link.reached.args"));
        let args = std::fs::read_to_string(reached_file(&response)).unwrap();
        assert!(args.contains("--gc-sections") && args.contains(FIRST_DEFINITION_STAYS), "{args}");
        assert!(!std::fs::read_to_string(&response).unwrap().contains("--gc-sections"));
        std::fs::remove_file(reached_file(&response)).unwrap();
        assert_eq!(reached_only(&command), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ours_go_ahead_of_rustcs_for_link_exe() {
        const LINE: &str = r#""C:\\v c\\link.exe" "/NOLOGO" "C:\\t\\rustc1\\symbols.o" "C:\\t\\a.1.rcgu.o" "C:\\t\\libb-2.rlib" "kernel32.lib" "/WX" "/LIBPATH:C:\\l\\" "/OUT:C:\\t\\a.exe" "/DEBUG" "/PDBALTPATH:%_PDB%""#;
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-msvc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = Plan {
            crates: Vec::new(),
            object: PathBuf::from("P:/ra-jit.o"),
            patch: PathBuf::from("P:/patch.o"),
            binary: PathBuf::from("P:/bin"),
            rustc_binary: None,
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            unit_test: false,
            linked_as_built: Vec::new(),
            copies: Vec::new(),
            stamp: 0,
            package: None,
            whole: false,
        };
        let response = dir.join("link.args");
        let link = LinkLine::parse(LINE).unwrap().present();
        assert_eq!(link.flavor, Flavor::Msvc);
        let command = link.command(&[&plan.patch, &plan.object], &plan.binary, &response).unwrap();
        assert_eq!(command, format!(r#""C:\v c\link.exe" "@{}""#, response.display()));
        let args = std::fs::read_to_string(&response).unwrap();
        let args: Vec<&str> = args
            .lines()
            .filter(|it| !it.starts_with("\"/LIBPATH:") || it.contains(r"C:\l"))
            .collect();
        assert_eq!(
            args,
            [
                r#""/NOLOGO""#,
                r#""P:/patch.o""#,
                r#""P:/ra-jit.o""#,
                r#""/FORCE:MULTIPLE""#,
                r#""/IGNORE:4006""#,
                r#""C:\t\a.1.rcgu.o""#,
                r#""C:\t\libb-2.rlib""#,
                r#""kernel32.lib""#,
                r#""/LIBPATH:C:\l\\""#,
                r#""/OUT:P:/bin""#,
                r#""/DEBUG""#,
                r#""/PDBALTPATH:P:/bin.pdb""#,
                r#""/STACK:8388608""#,
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The repository's cargo configuration links MSVC programs with `rust-lld`, which only the
    /// toolchain has; the daemon runs it by its path.
    #[cfg(windows)]
    #[test]
    fn rust_lld_is_run_from_the_toolchain() {
        const LINE: &str = r#""rust-lld" "-flavor" "link" "/NOLOGO" "C:\\t\\rustc1\\symbols.o" "C:\\t\\a.1.rcgu.o" "/OUT:C:\\t\\a.exe""#;
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-lld-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = LinkLine::parse(LINE).unwrap().present();
        let lld = crate::cargo::toolchain::rust_lld();
        assert!(lld.is_file(), "{}", lld.display());
        let command = link
            .command(&[Path::new("P:/ra-jit.o")], Path::new("P:/bin"), &dir.join("link.args"))
            .unwrap();
        assert!(
            command.starts_with(&format!(r#""{}" "-flavor" "link" "@"#, lld.display())),
            "{command}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ours_go_ahead_of_rustcs() {
        let dir = std::env::temp_dir().join(format!("ra-jit-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = Plan {
            crates: Vec::new(),
            object: PathBuf::from("/p/ra-jit.o"),
            patch: PathBuf::from("/p/patch.o"),
            binary: PathBuf::from("/p/bin"),
            rustc_binary: None,
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            unit_test: false,
            linked_as_built: Vec::new(),
            copies: Vec::new(),
            package: None,
            stamp: 0,
            whole: false,
        };
        let response = dir.join("link.args");
        let command = LinkLine::parse(LINE)
            .unwrap()
            .present()
            .command(&[&plan.patch, &plan.object], &plan.binary, &response)
            .unwrap();
        let response_arg = format!("@{}", response.display());
        assert_eq!(
            shlex::split(&command).unwrap(),
            ["LC_ALL=C", "PATH=/t/bin:/usr/bin", "cc", response_arg.as_str()]
        );
        // As the driver reads the file: quoted where a shell would need it.
        let args = shlex::split(&std::fs::read_to_string(&response).unwrap()).unwrap();
        assert_eq!(
            args,
            [
                "-m64",
                "/p/patch.o",
                "/p/ra-jit.o",
                FIRST_DEFINITION_STAYS,
                "/t/deps/a.1.rcgu.o",
                "-Wl,-Bstatic",
                "/t/deps/libb-2.rlib",
                "-o",
                "/p/bin",
                "-pie"
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
