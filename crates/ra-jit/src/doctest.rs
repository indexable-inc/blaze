//! ra-jit as cargo's `RUSTDOC` for `cargo test --doc`: the examples in the documentation of a
//! library are taken out as rustdoc takes them, and each is a program of its own, a crate
//! ra-jit adds to the graph, compiles with everything it links over the standard library's
//! unit and links itself, with nothing of rustc's. One rust-analyzer finds an error in, one
//! marked `compile_fail` and one ra-jit cannot build whole is rustc's. What they print and
//! how they end is told as rustdoc's test harness tells it.
//!
//! Anything else rustdoc is asked for, and a run of the tests that asks for what is not done
//! here, is rustdoc's: the reason is said and rustdoc runs.

mod extract;
mod program;

use std::{
    ffi::OsString,
    io::{IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use anyhow::Context as _;
use ide_db::{RootDatabase, base_db::SourceDatabase};
use syntax::Edition;

use crate::plan::Selection;
use extract::{Example, Marks};
use program::{Frame, Program};

/// The variable that makes `ra-jit` stand in for rustdoc. Its value is the rustdoc to run for
/// what is left to it.
pub(crate) const ENABLED: &str = "RA_JIT_RUSTDOC";

/// The variable that tells ra-jit as rustdoc the build `cargo test --doc` was asked for, as
/// JSON: cargo builds the libraries the examples link against with its features, which a
/// package shares with the others it selects, not with those of a build of the package alone.
pub(crate) const BUILD: &str = "RA_JIT_DOCTEST_BUILD";

/// The variable that has ra-jit as rustdoc write down how cargo ran it, in the directory it
/// names, and end there: the runs are made once cargo is done, over one load of the workspace.
pub(crate) const RECORD: &str = "RA_JIT_DOCTEST_RECORD";

/// The argument that has ra-jit make the runs written down under [`RECORD`], before a
/// [`Replay`] as JSON.
const REPLAY: &str = "--replay-doctests";

/// The code a test harness ends with when a test failed.
const FAILED: i32 = 101;

/// A run of the examples of one library, as cargo asks rustdoc for it.
pub(crate) struct Asked {
    crate_name: String,
    /// The root file of the library, as cargo names it from the package's directory.
    source: PathBuf,
    edition: String,
    /// Where rustdoc was run, and with what environment: what an example reads of either is
    /// what it reads under rustdoc.
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
    /// Where the examples run.
    run_in: PathBuf,
    /// The arguments rustc compiles every example with.
    rustc_args: Vec<String>,
    target: Option<String>,
    features: Vec<String>,
    /// The build cargo was asked for, when ra-jit asked it.
    build: Option<Selection>,
    wanted: Wanted,
    /// Where the programs are built.
    dir: PathBuf,
}

/// Which examples the arguments after `--` ask for, and how the harness shows them.
#[derive(Default)]
struct Wanted {
    filters: Vec<String>,
    skipped: Vec<String>,
    exact: bool,
    /// libtest's `--format=terse`: a character for each example that does not fail.
    terse: bool,
}

impl Wanted {
    /// `Err` is the argument that asks for something not done here.
    fn parse(args: &[String]) -> Result<Wanted, String> {
        let mut wanted = Wanted::default();
        let (mut quiet, mut format) = (false, None);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (flag, joined) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => (arg.as_str(), None),
            };
            let mut value = || joined.or_else(|| args.next().map(String::as_str));
            match flag {
                "--exact" => wanted.exact = true,
                "--skip" => wanted.skipped.extend(value().map(str::to_owned)),
                // The examples run as many at a time as there are processors either way.
                "--test-threads" => _ = value(),
                "-q" | "--quiet" => quiet = true,
                "--format" => match value() {
                    Some(it @ ("pretty" | "terse")) => format = Some(it == "terse"),
                    _ => return Err(arg.clone()),
                },
                // Nothing is shown in color, which is what `auto` is when the output is not
                // a terminal.
                "--color" => match value() {
                    Some("never") => {}
                    Some("auto") if !std::io::stdout().is_terminal() => {}
                    _ => return Err(arg.clone()),
                },
                _ if arg.starts_with('-') => return Err(arg.clone()),
                _ => wanted.filters.push(arg.clone()),
            }
        }
        wanted.terse = format.unwrap_or(quiet);
        Ok(wanted)
    }

    fn has(&self, name: &str) -> bool {
        let is = |it: &String| if self.exact { name == it } else { name.contains(it.as_str()) };
        (self.filters.is_empty() || self.filters.iter().any(is)) && !self.skipped.iter().any(is)
    }
}

/// The flags of rustdoc that take a value and are passed on to rustc for every example.
const TO_RUSTC: [&str; 6] = ["--cfg", "--check-cfg", "-L", "--extern", "-C", "--cap-lints"];

/// The flags of rustdoc that take a value and say nothing of how an example is built. The
/// levels of lints are of the documentation itself: rustdoc compiles no example with them.
const UNUSED: [&str; 12] = [
    "--crate-type",
    "--color",
    "--error-format",
    "--diagnostic-width",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "-A",
    "-W",
    "-D",
    "-F",
];

impl Asked {
    /// Reads the arguments of a run of rustdoc. `Err` is why the run is rustdoc's.
    fn read(recorded: &Recorded) -> Result<Asked, String> {
        let words: Vec<String> = recorded
            .args
            .iter()
            .map(|it| it.to_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or("an argument is not text")?;
        let cwd = recorded.cwd.clone();
        let (mut test, mut source, mut crate_name) = (false, None, None);
        let (mut edition, mut run_in, mut target) = ("2015".to_owned(), cwd.clone(), None);
        let (mut rustc_args, mut test_args, mut features) = (Vec::new(), Vec::new(), Vec::new());
        let mut words = words.into_iter();
        while let Some(word) = words.next() {
            // A flag and its value are one word with `=` between them, or two.
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => {
                    (flag.to_owned(), Some(value.to_owned()))
                }
                _ => (word.clone(), None),
            };
            let mut value =
                || joined.clone().or_else(|| words.next()).ok_or(format!("{flag} has no value"));
            match flag.as_str() {
                "--test" => test = true,
                "--edition" => edition = value()?,
                "--crate-name" => crate_name = Some(value()?),
                "--test-run-directory" => run_in = cwd.join(value()?),
                "--test-args" => {
                    let value = value()?;
                    test_args.extend(
                        shlex::split(&value).ok_or("`--test-args` is not quoted as a shell's")?,
                    );
                }
                "--target" => {
                    let triple = value()?;
                    rustc_args.extend(["--target".to_owned(), triple.clone()]);
                    target = Some(triple);
                }
                _ if TO_RUSTC.contains(&flag.as_str()) => {
                    let value = value()?;
                    if let Some(feature) = feature_of(&flag, &value) {
                        features.push(feature);
                    }
                    rustc_args.extend([flag.clone(), value]);
                }
                _ if UNUSED.contains(&flag.as_str()) => _ = value()?,
                _ if flag.starts_with('-') => return Err(format!("rustdoc is run with `{flag}`")),
                _ if source.is_none() => source = Some(PathBuf::from(word)),
                _ => return Err(format!("rustdoc is given `{word}` too")),
            }
        }
        if !test {
            return Err("rustdoc is not asked to run tests".to_owned());
        }
        let wanted =
            Wanted::parse(&test_args).map_err(|arg| format!("the tests are run with `{arg}`"))?;
        let build = match recorded.var(BUILD) {
            Some(build) => {
                Some(serde_json::from_str(build).map_err(|it| format!("{BUILD}: {it}"))?)
            }
            None => None,
        };
        Ok(Asked {
            crate_name: crate_name.ok_or("rustdoc is not told the name of the crate")?,
            source: source.ok_or("rustdoc is given no source file")?,
            edition,
            cwd,
            env: recorded.env.clone(),
            run_in,
            rustc_args,
            target,
            features,
            build,
            wanted,
            dir: std::env::temp_dir().join(format!("ra-jit-doctest-{}", std::process::id())),
        })
    }

    /// The arguments of the run of ra-jit that loads the library the examples are of, with
    /// nothing of rustc's: the examples are built over the standard library's unit.
    fn build_args(&self) -> anyhow::Result<Vec<String>> {
        std::fs::create_dir_all(&self.dir)?;
        // The package is where cargo runs the examples, not where it runs rustdoc: that is
        // the workspace's root, whose package is another one or none.
        let mut args = vec![
            self.run_in.display().to_string(),
            "--crate".to_owned(),
            self.root_file(),
            "--out".to_owned(),
            self.object().display().to_string(),
            "--dev-deps".to_owned(),
        ];
        match &self.build {
            // Built as cargo built it: a dependency of this package and of another one cargo
            // selected has the features both ask for, and its types are laid out with them.
            Some(build) => {
                args.extend(Selection { target: None, ..build.clone() }.flags());
                if let Some(asked_in) = &build.asked_in {
                    args.extend(["--asked-in".to_owned(), asked_in.display().to_string()]);
                }
            }
            // The build scripts run from the workspace's root, whose package is another one:
            // the features are this package's once it is the one selected. `package/feature`
            // would name a dependency of the root's, which this package need not be.
            None => {
                args.push("--no-default-features".to_owned());
                let package = self.var("CARGO_PKG_NAME").filter(|it| !it.is_empty());
                if let Some(package) = package {
                    args.extend(["--package".to_owned(), package.to_owned()]);
                }
                if !self.features.is_empty() {
                    args.extend(["--features".to_owned(), self.features.join(",")]);
                }
            }
        }
        if let Some(target) = &self.target {
            args.extend(["--target".to_owned(), target.clone()]);
        }
        Ok(args)
    }

    /// The root file of the library in full, which names its crate to ra-jit.
    fn root_file(&self) -> String {
        self.cwd.join(&self.source).display().to_string()
    }

    /// Where the run's `--out` is: nothing is built there, the standard library's unit is
    /// written beside it.
    fn object(&self) -> PathBuf {
        self.dir.join("ra-jit.o")
    }

    /// The variable of rustdoc's environment called `name`, when it is text.
    fn var(&self, name: &str) -> Option<&str> {
        var_in(&self.env, name)
    }

    /// A command that runs where rustdoc was run and with its environment.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        in_place(as_asked(program), &self.cwd, &self.env)
    }

    /// Has rustc compile `source` into a program, or finds the program it compiled of it in an
    /// earlier run, when nothing it was compiled from has changed since. The directory with
    /// what rustc wrote is `scratch` or the one it was remembered in.
    fn compile(
        &self,
        source: &str,
        scratch: &Path,
        edition: &str,
        env: &[(&str, String)],
        links: bool,
    ) -> anyhow::Result<(Compiled, PathBuf)> {
        let memo = self.memo().map(|memo| memo.join(self.key(source, edition, env, links)));
        if let Some(dir) = &memo {
            if let Some(compiled) = Remembered::read(dir, env, self) {
                return Ok((compiled, dir.clone()));
            }
            _ = std::fs::remove_dir_all(dir);
        }
        let memo = memo.filter(|dir| {
            dir.parent().is_some_and(|it| std::fs::create_dir_all(it).is_ok())
                && std::fs::create_dir(dir).is_ok()
        });
        let dir = memo.as_deref().unwrap_or(scratch);
        std::fs::create_dir_all(dir)?;
        let rustc = self.env.iter().find(|(name, _)| name == "RUSTC");
        let mut rustc = self.command(rustc.map_or_else(|| "rustc".into(), |(_, it)| it.clone()));
        rustc.args(&self.rustc_args).args(["--edition", edition, "--color", "never"]);
        rustc.arg("--crate-type=bin").arg("--out-dir").arg(dir);
        // Each output is named: with `-o` and more than one, rustc warns that it names them.
        let (out, deps) = (dir.join("rust_out"), dir.join("rust_out.d"));
        let emit = if links { "link" } else { "metadata" };
        rustc.arg(format!("--emit={emit}={},dep-info={}", out.display(), deps.display()));
        rustc.arg("-").envs(env.iter().map(|(name, value)| (name, value)));
        rustc.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut rustc = rustc.spawn().context("running rustc")?;
        rustc.stdin.take().context("rustc takes no input")?.write_all(source.as_bytes())?;
        let ran = rustc.wait_with_output()?;
        let compiled = Compiled {
            success: ran.status.success(),
            stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
        };
        if memo.is_some() {
            Remembered::write(dir, &compiled);
        }
        Ok((compiled, dir.to_owned()))
    }

    /// Where rustc's programs of the examples stay from run to run: beside what cargo built
    /// them against, in `<target>/<profile>/ra-jit-doctest`. Cargo names the libraries'
    /// directory, `deps` in its first layout and one unit's `build/<package>/<hash>/out` in
    /// its second ([`crate::cargo::layout`]).
    fn memo(&self) -> Option<PathBuf> {
        // In cargo's second layout of the build directory a library of no dependencies is
        // documented with no `-L dependency=`: the directory of the library itself then.
        let dependency = self.rustc_args.windows(2).find_map(|it| match it[0] == "-L" {
            true => it[1].strip_prefix("dependency=").map(Path::new),
            false => None,
        });
        let library = || {
            let externs = self.rustc_args.windows(2).filter(|it| it[0] == "--extern");
            externs.filter_map(|it| Path::new(it[1].split_once('=')?.1).parent()).next()
        };
        let deps = self.cwd.join(dependency.or_else(library)?);
        let profile = match crate::cargo::layout::unit_dir(&deps) {
            Some(unit) => unit.profile()?.to_owned(),
            None => deps.parent()?.to_owned(),
        };
        Some(profile.join("ra-jit-doctest"))
    }

    /// What tells a program rustc compiles apart from another: everything it is given, and
    /// the libraries named with `--extern` as they are now. Cargo builds a library again when
    /// one it is built against changes, so those name every library the program reads.
    fn key(&self, source: &str, edition: &str, env: &[(&str, String)], links: bool) -> String {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        let mut add = |it: &[u8]| {
            hasher.update((it.len() as u64).to_le_bytes());
            hasher.update(it);
        };
        add(env!("CARGO_PKG_VERSION").as_bytes());
        for name in ["RUSTC", "RUSTUP_TOOLCHAIN"] {
            add(self.var(name).unwrap_or_default().as_bytes());
        }
        for arg in &self.rustc_args {
            add(arg.as_bytes());
        }
        let externs = self.rustc_args.windows(2).filter(|it| it[0] == "--extern");
        for (_, file) in externs.filter_map(|it| it[1].split_once('=')) {
            add(format!("{:?}", stamp(Path::new(file))).as_bytes());
        }
        for (name, value) in env {
            add(name.as_bytes());
            add(value.as_bytes());
        }
        add(edition.as_bytes());
        add(&[u8::from(links)]);
        add(source.as_bytes());
        let digest = hasher.finalize();
        digest[..16].iter().map(|it| format!("{it:02x}")).collect()
    }

    /// The path of `file` as rustdoc shows it: from the directory it runs in.
    fn shown(&self, file: &Path) -> String {
        file.strip_prefix(&self.cwd).unwrap_or(file).display().to_string()
    }
}

/// What rustc said of a program it compiled.
#[derive(serde::Serialize, serde::Deserialize)]
struct Compiled {
    success: bool,
    stderr: String,
}

/// A program rustc compiled in an earlier run, in the directory it compiled it in, with what
/// it read: its dep-info names the files and the variables of the environment.
#[derive(serde::Serialize, serde::Deserialize)]
struct Remembered {
    compiled: Compiled,
    files: Vec<(PathBuf, Option<(u64, u128)>)>,
    env: Vec<(String, Option<String>)>,
}

impl Remembered {
    const FILE: &str = "rustc.json";

    /// What rustc said of the program in `dir`, when it read nothing since that is not as it
    /// was. `env` is what the program is compiled with beside rustdoc's environment.
    fn read(dir: &Path, env: &[(&str, String)], asked: &Asked) -> Option<Compiled> {
        let text = std::fs::read(dir.join(Self::FILE)).ok()?;
        let it: Remembered = serde_json::from_slice(&text).ok()?;
        let same_file = |(file, was): &(PathBuf, _)| stamp(file) == *was;
        let same_variable = |(name, was): &(String, Option<String>)| {
            let given = env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str());
            given.or_else(|| asked.var(name)) == was.as_deref()
        };
        (it.files.iter().all(same_file) && it.env.iter().all(same_variable)).then_some(it.compiled)
    }

    /// Remembers what rustc said of the program it compiled in `dir`, when it wrote what it
    /// read. A program it said nothing of is compiled again next time.
    fn write(dir: &Path, compiled: &Compiled) {
        let Ok(text) = std::fs::read_to_string(dir.join("rust_out.d")) else { return };
        let read = crate::dep_info::DepInfo::parse(&text);
        // The source is given on standard input, which dep-info names `-`.
        let files = read.files.iter().filter(|it| *it != "-").map(PathBuf::from);
        let files = files.map(|file| (stamp(&file), file)).map(|(was, file)| (file, was));
        let it = Remembered {
            compiled: Compiled { success: compiled.success, stderr: compiled.stderr.clone() },
            files: files.collect(),
            env: read.env,
        };
        let Ok(text) = serde_json::to_vec(&it) else { return };
        let written = dir.join(format!("{}.{}", Self::FILE, std::process::id()));
        if std::fs::write(&written, text).is_ok() {
            _ = std::fs::rename(&written, dir.join(Self::FILE));
        }
    }
}

/// A file's length and when it was written, `None` when there is none.
fn stamp(file: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(file).ok()?;
    let written = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((meta.len(), written.as_nanos()))
}

/// The feature a `--cfg feature="name"` turns on.
fn feature_of(flag: &str, value: &str) -> Option<String> {
    let name = value.strip_prefix("feature=").filter(|_| flag == "--cfg")?;
    Some(name.trim_matches('"').to_owned())
}

/// A command that runs where ra-jit was asked and with its environment as it is.
#[expect(clippy::disallowed_methods, reason = "`toolchain::command` adds to the environment")]
pub(crate) fn as_asked(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
}

/// `command`, run in `cwd` with `env` and nothing else of ra-jit's environment.
fn in_place(mut command: Command, cwd: &Path, env: &[(OsString, OsString)]) -> Command {
    command.current_dir(cwd).env_clear().envs(env.iter().map(|(name, value)| (name, value)));
    command
}

/// The variable of `env` called `name`, when it is text.
fn var_in<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a str> {
    env.iter().find(|(it, _)| it == name).and_then(|(_, value)| value.to_str())
}

/// A run of rustdoc as cargo asked for it: its arguments, where it ran and its environment.
#[derive(serde::Serialize, serde::Deserialize)]
struct Recorded {
    args: Vec<OsString>,
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Recorded {
    /// The run this process is.
    fn own(args: Vec<OsString>) -> anyhow::Result<Recorded> {
        let env = std::env::vars_os().filter(|(name, _)| name != RECORD).collect();
        Ok(Recorded { args, cwd: std::env::current_dir()?, env })
    }

    fn var(&self, name: &str) -> Option<&str> {
        var_in(&self.env, name)
    }

    /// The crate the run tests the examples of, as cargo names it after `Doc-tests`.
    fn crate_name(&self) -> &str {
        let mut args = self.args.iter().filter_map(|it| it.to_str());
        while let Some(arg) = args.next() {
            match arg.strip_prefix("--crate-name") {
                Some("") => return args.next().unwrap_or_default(),
                Some(joined) if joined.starts_with('=') => return &joined[1..],
                _ => {}
            }
        }
        ""
    }

    /// Says why and has rustdoc make the run. The code it ended with.
    fn to_rustdoc(&self, why: Option<&str>) -> i32 {
        if let Some(why) = why {
            eprintln!("ra-jit: the doctests are left to rustdoc: {why}");
        }
        let rustdoc = self.env.iter().find(|(name, _)| name == ENABLED).map(|(_, it)| it.clone());
        let rustdoc = rustdoc.filter(|it| !it.is_empty()).unwrap_or_else(|| "rustdoc".into());
        let mut rustdoc = in_place(as_asked(rustdoc), &self.cwd, &self.env);
        match rustdoc.args(&self.args).status() {
            Ok(status) => crate::cargo::run::exit_code(status),
            Err(error) => {
                eprintln!("error: rustdoc did not run: {error}");
                1
            }
        }
    }

    /// Writes the run down in `dir`, under a name that sorts after those of the runs before.
    fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        let file = dir.join(format!("{:039}-{}.json", since.as_nanos(), std::process::id()));
        std::fs::write(file, serde_json::to_vec(self)?)?;
        Ok(())
    }

    /// The runs written down in `dir`, in the order cargo asked for them.
    fn all_in(dir: &Path) -> anyhow::Result<Vec<Recorded>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            files.push(entry?.path());
        }
        files.retain(|it| it.extension().is_some_and(|it| it == "json"));
        files.sort();
        let read = |file: &PathBuf| Ok(serde_json::from_slice(&std::fs::read(file)?)?);
        files.iter().map(read).collect()
    }
}

/// Says why and runs rustdoc as it was asked, ending as it does.
fn leave_to_rustdoc(args: &[OsString], why: Option<&str>) -> ! {
    let code = match Recorded::own(args.to_vec()) {
        Ok(recorded) => recorded.to_rustdoc(why),
        Err(error) => {
            eprintln!("error: rustdoc did not run: {error}");
            1
        }
    };
    std::process::exit(code)
}

/// The runs of rustdoc cargo had ra-jit write down in `dir`, and how cargo was asked to run
/// them.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Replay {
    pub(crate) dir: PathBuf,
    /// Whether cargo was asked to go on past a library whose examples failed.
    pub(crate) no_fail_fast: bool,
    pub(crate) quiet: bool,
}

impl Replay {
    /// The run of ra-jit that makes the runs, where cargo made the first and with its
    /// environment. `None` when cargo made none.
    pub(crate) fn command(&self) -> anyhow::Result<Option<Command>> {
        let Some(first) = Recorded::all_in(&self.dir)?.into_iter().next() else {
            return Ok(None);
        };
        let ra_jit = std::env::current_exe().context("finding ra-jit itself")?;
        let mut ra_jit = in_place(as_asked(ra_jit), &first.cwd, &first.env);
        ra_jit.arg(REPLAY).arg(serde_json::to_string(self)?);
        Ok(Some(ra_jit))
    }
}

/// The libraries whose examples a run of ra-jit tests over one load of their workspace, in
/// the order cargo ran rustdoc for them.
pub(crate) struct Libraries {
    runs: Vec<(Recorded, Result<Asked, String>)>,
    /// How cargo was asked to run them, when ra-jit runs them after cargo is done: cargo ran
    /// rustdoc for none of them and said nothing of them.
    replay: Option<Replay>,
}

impl Libraries {
    /// Loads the workspace of the first library ra-jit can test the examples of and tests
    /// them all, ending as cargo would after them. Each is rustdoc's when the workspace does
    /// not load.
    fn load(&self) -> anyhow::Result<()> {
        let first = self.runs.iter().find_map(|(_, asked)| asked.as_ref().ok());
        let loaded = match first {
            Some(first) => first
                .build_args()
                .and_then(|args| crate::run(crate::Args::parse(args.into_iter())?, Some(self))),
            None => Ok(()),
        };
        let why = match (first, loaded) {
            (Some(_), Ok(())) => return Ok(()),
            (None, _) => None,
            (Some(_), Err(why)) => Some(format!("{why:#}")),
        };
        self.remove_dirs();
        let code = self.each(|recorded, asked| {
            recorded.to_rustdoc(Some(asked.err().or(why.as_deref()).unwrap_or_default()))
        });
        std::process::exit(code)
    }

    /// Tests each library with `test`, which is given its run and what ra-jit makes of it,
    /// until one fails where cargo would stop. The code to end with.
    fn each(&self, mut test: impl FnMut(&Recorded, Result<&Asked, &str>) -> i32) -> i32 {
        let mut test = |(recorded, asked): &(Recorded, Result<Asked, String>)| {
            test(recorded, asked.as_ref().map_err(String::as_str))
        };
        let Some(replay) = &self.replay else {
            return self.runs.iter().map(&mut test).fold(0, |code, it| code.max(it));
        };
        use crate::cargo::say;
        let mut failed = Vec::new();
        for run in &self.runs {
            say::status(replay.quiet, "Doc-tests", run.0.crate_name());
            if test(run) == 0 {
                continue;
            }
            let rerun = format!("-p {} --doc", run.0.var("CARGO_PKG_NAME").unwrap_or_default());
            say::error(&format!("doctest failed, to rerun pass `{rerun}`"));
            if !replay.no_fail_fast {
                return FAILED;
            }
            failed.push(rerun);
        }
        if failed.is_empty() {
            return 0;
        }
        // Cargo lists what failed again when it went on past the first.
        let targets = if failed.len() == 1 { "target" } else { "targets" };
        say::error(&format!("{} {targets} failed:", failed.len()));
        failed.iter().for_each(|it| eprintln!("    `{it}`"));
        FAILED
    }

    /// Removes the directories the programs of the examples were built in.
    fn remove_dirs(&self) {
        for (_, asked) in &self.runs {
            if let Ok(asked) = asked {
                _ = std::fs::remove_dir_all(&asked.dir);
            }
        }
    }
}

/// ra-jit as rustdoc: runs the examples when that is what is asked and ra-jit can, and has
/// rustdoc do as it was asked otherwise. Under [`RECORD`] the run is written down for
/// [`replay`] instead.
pub(crate) fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let all: Vec<OsString> = args.collect();
    if !all.iter().any(|it| it == "--test") {
        leave_to_rustdoc(&all, None);
    }
    let recorded = match Recorded::own(all.clone()) {
        Ok(recorded) => recorded,
        Err(why) => leave_to_rustdoc(&all, Some(&format!("{why:#}"))),
    };
    if let Some(dir) = std::env::var_os(RECORD) {
        if let Err(why) = recorded.write(Path::new(&dir)) {
            std::process::exit(recorded.to_rustdoc(Some(&format!("{why:#}"))));
        }
        return Ok(());
    }
    let asked = Asked::read(&recorded);
    Libraries { runs: vec![(recorded, asked)], replay: None }.load()
}

/// Makes the runs of rustdoc written down under [`RECORD`] when ra-jit is run with [`REPLAY`]:
/// the examples of every library, over one load of their workspace.
pub(crate) fn replay() -> Option<anyhow::Result<()>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some(REPLAY) {
        return None;
    }
    let replay: Replay = match serde_json::from_str(&args.next().unwrap_or_default()) {
        Ok(replay) => replay,
        Err(why) => return Some(Err(anyhow::anyhow!("{REPLAY}: {why}"))),
    };
    let runs = match Recorded::all_in(&replay.dir) {
        Ok(runs) => runs,
        Err(why) => return Some(Err(why)),
    };
    let runs = runs.into_iter().map(|it| (Asked::read(&it), it)).map(|(asked, it)| (it, asked));
    Some(Libraries { runs: runs.collect(), replay: Some(replay) }.load())
}

/// What the crate asks of every example with `#![doc(test(..))]`.
#[derive(Default)]
struct OfTheCrate {
    attrs: Vec<String>,
    no_crate_inject: bool,
}

impl OfTheCrate {
    fn read(db: &RootDatabase, krate: hir::Crate) -> OfTheCrate {
        use syntax::{AstNode, SyntaxKind, ast};
        let root = krate.root_file(db);
        let file = ast::SourceFile::parse(db.file_text(root).text(db), Edition::CURRENT).tree();
        let mut of_the_crate = OfTheCrate::default();
        let trees = file.syntax().descendants().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
        // `test(..)` directly inside `doc(..)` of an attribute of the file itself.
        for tree in trees {
            let before = |node: &syntax::SyntaxNode| {
                let mut prev = node.prev_sibling_or_token();
                while prev.as_ref().is_some_and(|it| it.kind().is_trivia()) {
                    prev = prev.and_then(|it| it.prev_sibling_or_token());
                }
                prev.map(|it| it.to_string())
            };
            let outer = tree.parent().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
            let in_doc = outer.as_ref().is_some_and(|it| before(it).as_deref() == Some("doc"));
            if before(&tree).as_deref() != Some("test") || !in_doc {
                continue;
            }
            for inner in tree.children().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE) {
                if before(&inner).as_deref() == Some("attr") {
                    let text = inner.text().to_string();
                    of_the_crate.attrs.extend(split_attrs(&text[1..text.len() - 1]));
                }
            }
            let words = tree.children_with_tokens().filter_map(|it| it.into_token());
            of_the_crate.no_crate_inject |=
                words.into_iter().any(|it| it.text() == "no_crate_inject");
        }
        of_the_crate
    }
}

/// The attributes in a list separated by commas, which their own arguments have too.
fn split_attrs(list: &str) -> Vec<String> {
    let (mut attrs, mut depth, mut start) = (Vec::new(), 0, 0);
    for (at, byte) in list.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                attrs.push(list[start..at].trim().to_owned());
                start = at + 1;
            }
            _ => {}
        }
    }
    attrs.push(list[start..].trim().to_owned());
    attrs.retain(|it| !it.is_empty());
    attrs
}

/// How many examples' programs ra-jit built and how many rustc did, and how many of ra-jit's
/// were taken from an earlier run.
#[derive(Default)]
struct Counted {
    ra_jit: AtomicUsize,
    rustc: AtomicUsize,
    remembered: AtomicUsize,
}

/// How an example ended.
enum Ended {
    Passed,
    Ignored,
    /// With what the harness shows of it.
    Failed(String),
}

/// One example ready to build: its name as the harness shows it and its program.
struct Test {
    name: String,
    marks: Marks,
    program: Program,
    /// The path and line rustc is to show for the first line of the example.
    shown_path: String,
    line: usize,
    edition: Edition,
    /// Whether rustdoc builds the example in one program with the others, as it does from
    /// edition 2024: the test harness of that program names it.
    in_one: bool,
}

/// What ra-jit built of an example.
enum Ours {
    /// Nothing, the example being only to compile, and rust-analyzer finds no error in it.
    Checked,
    /// Its object and the standard library's unit, to link in `dir` with `line`. A `kept`
    /// directory is the memo's, where the program stays for the next run.
    Compiled { objects: [PathBuf; 2], dir: PathBuf, line: crate::link::Line, kept: bool },
    /// The program, linked in an earlier run from what is unchanged since.
    Linked(PathBuf),
}

impl Ours {
    /// Links the program, which calls nothing ra-jit could not compile.
    fn link(&self) -> anyhow::Result<PathBuf> {
        let (objects, dir, line, kept) = match self {
            Ours::Compiled { objects, dir, line, kept } => (objects, dir, line, *kept),
            Ours::Linked(binary) => return Ok(binary.clone()),
            Ours::Checked => anyhow::bail!("nothing to link"),
        };
        let binary = dir.join("rust_out");
        let linked = line.link(&[&objects[0], &objects[1]], &binary).and_then(|_| {
            let stand_ins = crate::link::stand_ins(&binary)?;
            match stand_ins.first() {
                Some(first) => anyhow::bail!(
                    "it calls {} functions ra-jit could not compile: {first}",
                    stand_ins.len()
                ),
                None => Ok(()),
            }
        });
        if kept {
            _ = std::fs::remove_file(&objects[0]);
            if linked.is_err() {
                _ = std::fs::remove_dir_all(dir);
            }
        }
        linked.map(|()| binary)
    }

    /// The directory to remove once the program ran: one that is not the memo's.
    fn scratch(&self) -> Option<&Path> {
        match self {
            Ours::Compiled { dir, kept: false, .. } => Some(dir),
            _ => None,
        }
    }
}

impl Test {
    fn of(asked: &Asked, example: Example, frame: &Frame<'_>) -> Test {
        let shown_path = asked.shown(&example.file);
        let item =
            if example.item.is_empty() { String::new() } else { format!("{} ", example.item) };
        let name = format!("{shown_path} - {item}(line {})", example.line);
        let id: String = format!("{shown_path}_{}_0", example.line)
            .chars()
            .map(|it| if it.is_ascii_alphanumeric() { it } else { '_' })
            .collect();
        let edition = example.marks.edition.as_deref().unwrap_or(&asked.edition);
        let edition = edition.parse().unwrap_or(Edition::CURRENT);
        let program = Program::of(&example.code, frame, &id, edition);
        let in_one = edition.at_least_2024();
        Test {
            name,
            marks: example.marks,
            program,
            shown_path,
            line: example.line,
            edition,
            in_one,
        }
    }

    /// The lines before the program's first in a file of it whose lines are numbered as
    /// rustdoc has rustc show them: from the line of the example in its documentation.
    fn padding(&self) -> usize {
        self.line.saturating_sub(self.program.lines_before)
    }

    /// The name with what the harness adds for how the example is tested.
    fn shown_name(&self) -> String {
        let how = match &self.marks {
            it if it.compile_fail => " - compile fail",
            it if it.no_run => " - compile",
            it if it.should_panic && self.in_one => " - should panic",
            _ => "",
        };
        format!("{}{how}", self.name)
    }

    fn is_ignored(&self, target: &str) -> bool {
        self.marks.ignore || self.marks.ignored_on.iter().any(|it| target.contains(it.as_str()))
    }

    /// Links and runs what ra-jit built of the example, or has rustc build it in `dir` when
    /// ra-jit built nothing or cannot link it, and runs that.
    fn run(
        &self,
        asked: &Asked,
        dir: &Path,
        ours: Option<&Ours>,
        target: &str,
        counted: &Counted,
    ) -> anyhow::Result<Ended> {
        if self.is_ignored(target) {
            return Ok(Ended::Ignored);
        }
        let marks = &self.marks;
        let linked = match ours {
            Some(Ours::Checked) => {
                counted.ra_jit.fetch_add(1, Ordering::Relaxed);
                return Ok(Ended::Passed);
            }
            Some(ours) => ours.link().inspect_err(|why| {
                eprintln!("ra-jit: {} is rustc's: {why:#}", self.name);
            }),
            None => Err(anyhow::anyhow!("not built")),
        };
        let binary = match linked {
            Ok(binary) => {
                counted.ra_jit.fetch_add(1, Ordering::Relaxed);
                binary
            }
            Err(_) => {
                counted.rustc.fetch_add(1, Ordering::Relaxed);
                match self.compiled(asked, dir)? {
                    Ok(binary) => binary,
                    Err(ended) => return Ok(ended),
                }
            }
        };
        let ran = match asked.command(&binary).current_dir(&asked.run_in).output() {
            Ok(ran) => ran,
            Err(error) => return Ok(Ended::Failed(format!("Couldn't run the test: {error}"))),
        };
        Ok(match (ran.status.success(), marks.should_panic) {
            (true, false) | (false, true) => Ended::Passed,
            (true, true) => Ended::Failed(
                "Test executable succeeded, but it's marked `should_panic`.".to_owned(),
            ),
            (false, false) => Ended::Failed(failure(&ran)),
        })
    }

    /// Has rustc compile the example in `dir`: the program to run, or how the example ended
    /// when there is none to.
    fn compiled(&self, asked: &Asked, dir: &Path) -> anyhow::Result<Result<PathBuf, Ended>> {
        let marks = &self.marks;
        let edition = marks.edition.as_deref().unwrap_or(&asked.edition);
        let line = self.line as i64 - self.program.lines_before as i64;
        let env = [
            ("UNSTABLE_RUSTDOC_TEST_PATH", self.shown_path.clone()),
            ("UNSTABLE_RUSTDOC_TEST_LINE", line.to_string()),
        ];
        let runs = !marks.no_run && !marks.compile_fail;
        let (built, made) = asked.compile(&self.program.source, dir, edition, &env, runs)?;
        let errors = &built.stderr;
        let ended = match (built.success, marks.compile_fail) {
            (true, true) => Ended::Failed(
                "Test compiled successfully, but it's marked `compile_fail`.".to_owned(),
            ),
            (false, false) => Ended::Failed(format!("{errors}Couldn't compile the test.")),
            (false, true) => {
                let missing: Vec<&String> =
                    marks.error_codes.iter().filter(|it| !errors.contains(it.as_str())).collect();
                match missing.is_empty() {
                    true => Ended::Passed,
                    false => Ended::Failed(format!(
                        "{errors}Some expected error codes were not found: {missing:?}"
                    )),
                }
            }
            (true, false) if !runs => Ended::Passed,
            (true, false) => return Ok(Ok(made.join("rust_out"))),
        };
        Ok(Err(ended))
    }
}

/// libtest's line of the results of `--format=terse` ends after this many of them.
const TERSE_COLUMNS: usize = 88;

/// What the harness has shown of the examples that ended so far.
#[derive(Default)]
struct Progress {
    terse: bool,
    total: usize,
    count: usize,
    column: usize,
}

impl Progress {
    /// Shows how `test` ended as libtest's pretty or terse output does.
    fn ended(
        &mut self,
        out: &mut impl std::io::Write,
        test: &Test,
        result: &anyhow::Result<Ended>,
    ) -> std::io::Result<()> {
        if !self.terse {
            let word = match result {
                Ok(Ended::Passed) => "ok",
                Ok(Ended::Ignored) => "ignored",
                Ok(Ended::Failed(_)) | Err(_) => "FAILED",
            };
            return writeln!(out, "test {} ... {word}", test.shown_name());
        }
        let short = match result {
            Ok(Ended::Passed) => ".",
            Ok(Ended::Ignored) => "i",
            Ok(Ended::Failed(_)) | Err(_) => {
                // A failure is on a line of its own, after the progress of the one before.
                if self.column != 0 {
                    self.progress(out)?;
                }
                self.count += 1;
                return writeln!(out, "{} --- FAILED", test.name);
            }
        };
        out.write_all(short.as_bytes())?;
        self.count += 1;
        self.column += 1;
        if self.column % TERSE_COLUMNS == TERSE_COLUMNS - 1 {
            self.progress(out)?;
        }
        out.flush()
    }

    fn progress(&mut self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        self.column = 0;
        writeln!(out, " {}/{}", self.count, self.total)
    }
}

/// What the harness shows of an example that ran and failed.
fn failure(ran: &std::process::Output) -> String {
    let (stdout, stderr) =
        (String::from_utf8_lossy(&ran.stdout), String::from_utf8_lossy(&ran.stderr));
    let mut shown = format!("Test executable failed ({}).", ran.status);
    if !stdout.is_empty() || !stderr.is_empty() {
        shown.push('\n');
        if !stdout.is_empty() {
            shown.push_str(&format!("\nstdout:\n{stdout}"));
        }
        if !stderr.is_empty() {
            shown.push_str(&format!("\nstderr:\n{stderr}"));
        }
    }
    shown
}

/// Has `krate` be read as rustdoc reads a crate whose examples it tests, with `cfg(doc)` and
/// `cfg(doctest)`: documentation that is only there for them has examples too.
fn document(db: &mut RootDatabase, krate: hir::Crate) {
    use ide_db::base_db::salsa::Setter as _;
    let krate: ide_db::base_db::Crate = krate.into();
    let mut cfg = krate.cfg_options(db).clone();
    cfg.insert_atom(intern::Symbol::intern("doc"));
    cfg.insert_atom(intern::Symbol::intern("doctest"));
    krate.set_cfg_options(db).to(cfg);
}

/// The examples of the library, each as the program rustdoc makes of it, and how many the
/// arguments leave out.
fn tests(db: &RootDatabase, asked: &Asked, krate: hir::Crate) -> (Vec<Test>, usize) {
    let of_the_crate = OfTheCrate::read(db, krate);
    let frame = Frame {
        crate_name: &asked.crate_name,
        attrs: &of_the_crate.attrs,
        no_crate_inject: of_the_crate.no_crate_inject,
    };
    let mut tests: Vec<Test> =
        extract::examples(db, krate).into_iter().map(|it| Test::of(asked, it, &frame)).collect();
    let all = tests.len();
    tests.retain(|it| asked.wanted.has(&it.name));
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    let left_out = all - tests.len();
    (tests, left_out)
}

/// How an example is built.
enum Plan {
    /// By rustc.
    Rustc,
    /// Not at all: it is only to compile, and rust-analyzer finds no error in it.
    Checked,
    /// By ra-jit, from its crate of this name.
    Ours(String),
}

/// How each of `tests` is built, by its place, and the standard library's unit the ones
/// ra-jit builds link: by ra-jit when rust-analyzer finds no error in it, else by rustc, and
/// why is said.
fn plan(
    db: &mut RootDatabase,
    building: &Building<'_>,
    asked: &Asked,
    krate: hir::Crate,
    tests: &[Test],
    target: &str,
) -> (Vec<Plan>, Option<PathBuf>) {
    let mut plans: Vec<Plan> = tests.iter().map(|_| Plan::Rustc).collect();
    let wanted: Vec<usize> = (0..tests.len())
        .filter(|&at| !tests[at].is_ignored(target) && !tests[at].marks.compile_fail)
        .collect();
    if wanted.is_empty() {
        return (plans, None);
    }
    let unit = match crate::std_unit(db, building.table, building.args, building.listed) {
        Ok(Some(unit)) => unit,
        Ok(None) => {
            eprintln!("ra-jit: the examples are rustc's: the standard library is not built apart");
            return (plans, None);
        }
        Err(error) => {
            eprintln!("ra-jit: the examples are rustc's: {error:#}");
            return (plans, None);
        }
    };
    let mixing = with_another_copy(db, krate);
    for (at, example) in add_crates(db, asked, krate, tests, &wanted) {
        let test = &tests[at];
        let source = &test.program.source;
        let names = |name: &String| {
            source.contains(&format!("{name}::")) || source.contains(&format!("crate {name}"))
        };
        if let Some(name) = mixing.iter().find(|it| names(it)) {
            eprintln!(
                "ra-jit: {} is rustc's: `{name}` links another copy of the library",
                test.name
            );
        } else if has_errors(db, example.root_file, denies_lints(source)) {
            eprintln!("ra-jit: {} is rustc's: rust-analyzer finds an error in it", test.name);
        } else if test.marks.no_run {
            plans[at] = Plan::Checked;
        } else {
            plans[at] = Plan::Ours(example.name);
        }
    }
    (plans, Some(unit))
}

/// Whether rust-analyzer finds an error of any kind in the example's file: rustc, which says
/// what, compiles it then. An example rustc does not compile fails, and one that does not
/// name what it uses compiles to a program that runs. Where the program denies lints, as
/// `#![doc(test(attr(deny(warnings))))]` has it do, a warning or a use of something
/// deprecated is one too: rustc has lints rust-analyzer does not.
fn has_errors(db: &RootDatabase, file: vfs::FileId, denies_lints: bool) -> bool {
    use ide_db::Severity;
    let config = ide_diagnostics::DiagnosticsConfig::test_sample();
    let resolve = ide_db::assists::AssistResolveStrategy::None;
    hir::attach_db(db, || {
        let syntax = ide_diagnostics::syntax_diagnostics(db, &config, file);
        let semantic = ide_diagnostics::semantic_diagnostics(db, &config, &resolve, file);
        let mut found = syntax.into_iter().chain(semantic);
        found.any(|it| {
            it.severity == Severity::Error || denies_lints && it.severity == Severity::Warning
        }) || denies_lints && uses_deprecated(db, file)
    })
}

/// Whether the program has an inner attribute that denies or forbids lints.
fn denies_lints(source: &str) -> bool {
    let attrs = source.lines().map(str::trim_start);
    attrs.into_iter().any(|it| it.starts_with("#![deny") || it.starts_with("#![forbid"))
}

/// Whether the file names an item, or calls a method, marked `#[deprecated]`, the arguments
/// of its macros (`assert_eq!`) included.
fn uses_deprecated(db: &RootDatabase, file: vfs::FileId) -> bool {
    use syntax::{AstNode as _, ast};
    let sema = hir::Semantics::new(db);
    let mut trees = vec![sema.parse_guess_edition(file).syntax().clone()];
    while let Some(tree) = trees.pop() {
        for node in tree.descendants() {
            let used = if let Some(call) = ast::MacroCall::cast(node.clone()) {
                trees.extend(sema.expand_macro_call(&call).map(|it| it.value));
                None
            } else if let Some(path) = ast::Path::cast(node.clone()) {
                match sema.resolve_path(&path) {
                    Some(hir::PathResolution::Def(def)) => Some(def),
                    _ => None,
                }
            } else {
                ast::MethodCallExpr::cast(node)
                    .and_then(|call| sema.resolve_method_call(&call))
                    .map(hir::ModuleDef::Function)
            };
            if used.and_then(|it| it.attrs(db)).is_some_and(|it| it.is_deprecated()) {
                return true;
            }
        }
    }
    false
}

/// The names of the dependencies of the library that link another copy of it, as a
/// dev-dependency on a crate that depends on it from crates.io does. Its types and the
/// library's are not each other's, which rustc says where an example mixes them, and
/// rust-analyzer's checks do not.
fn with_another_copy(db: &RootDatabase, library: hir::Crate) -> Vec<String> {
    let library: ide_db::base_db::Crate = library.into();
    let name = |krate: ide_db::base_db::Crate| {
        hir::Crate::from(krate).display_name(db).map(|it| it.crate_name().as_str().to_owned())
    };
    let Some(own) = name(library) else { return Vec::new() };
    let dependencies = library.data(db).dependencies.iter();
    let mixing = dependencies.filter(|dependency| {
        let linked = dependency.crate_id.transitive_deps(db);
        linked.into_iter().any(|it| it != library && name(it).as_deref() == Some(own.as_str()))
    });
    mixing.map(|it| it.name.as_str().to_owned()).collect()
}

/// The crate of an example in the graph.
struct Added {
    name: String,
    root_file: vfs::FileId,
}

/// The first of the files and source roots of the examples' crates, counted down from: ids no
/// file or source root of the workspace has. A file's is the highest a span can name.
const FIRST_FILE: u32 = 0x007f_ffff;
const FIRST_SOURCE_ROOT: u32 = u32::MAX;

/// Adds a crate for each example of `tests` at `wanted`: a binary whose one file has the
/// program of the example, its lines numbered as rustdoc has rustc number them, under the path
/// of the file the example is in, and that depends on the library and on what the library
/// depends on, with its cfgs and variables.
fn add_crates(
    db: &mut RootDatabase,
    asked: &Asked,
    library: hir::Crate,
    tests: &[Test],
    wanted: &[usize],
) -> Vec<(usize, Added)> {
    use ide_db::base_db::{
        self, AbsPathBuf, BuiltCrateData, BuiltDependency, CrateDisplayName, CrateName,
        CrateOrigin, ExtraCrateData, FileSet, SourceRoot, SourceRootId, TargetKind, VfsPath,
        salsa::Durability,
    };
    let library: base_db::Crate = library.into();
    let data = library.data(db).clone();
    let named = CrateName::normalize_dashes(&asked.crate_name);
    let dependencies: Vec<BuiltDependency> = std::iter::once(BuiltDependency::new(named, library))
        .chain(data.dependencies.iter().cloned())
        .collect();
    let (workspace, cfg, env) = (
        library.workspace_data(db).clone(),
        library.cfg_options(db).clone(),
        library.env(db).clone(),
    );
    let (mut added, mut crates) = (Vec::new(), Vec::new());
    for (n, &at) in wanted.iter().enumerate() {
        let test = &tests[at];
        let path = asked.cwd.join(&test.shown_path);
        let Ok(vfs_path) = AbsPathBuf::try_from(path.to_string_lossy().as_ref()) else {
            continue;
        };
        let file = vfs::FileId::from_raw(FIRST_FILE - n as u32);
        let root = SourceRootId(FIRST_SOURCE_ROOT - n as u32);
        let text = format!("{}{}", "\n".repeat(test.padding()), test.program.source);
        db.set_file_text_with_durability(file, &text, Durability::LOW);
        db.set_file_source_root_with_durability(file, root, Durability::LOW);
        let mut files = FileSet::default();
        files.insert(file, VfsPath::from(vfs_path));
        let source_root = triomphe::Arc::new(SourceRoot::new_local(files));
        db.set_source_root_with_durability(root, source_root, Durability::LOW);
        let name = format!("rust_out_{at}");
        let data = BuiltCrateData {
            root_file_id: file,
            edition: test.edition,
            dependencies: dependencies.clone(),
            origin: CrateOrigin::Local { repo: None, name: None },
            crate_attrs: Box::default(),
            target_kind: TargetKind::Bin,
            proc_macro_cwd: data.proc_macro_cwd.clone(),
        };
        let extra = ExtraCrateData {
            version: None,
            display_name: Some(CrateDisplayName::from_canonical_name(&name)),
            potential_cfg_options: None,
        };
        let krate =
            base_db::Crate::builder(data, extra, workspace.clone(), cfg.clone(), env.clone())
                .durability(Durability::MEDIUM)
                .new(db);
        crates.push(krate);
        added.push((at, Added { name, root_file: file }));
    }
    let all: Vec<base_db::Crate> = base_db::all_crates(db).iter().copied().chain(crates).collect();
    base_db::set_all_crates_with_durability(db, all, Durability::MEDIUM);
    added
}

/// Compiles the example whose crate is `name` with everything it links over the standard
/// library's `unit`, into an object in a directory of its own, to link with nothing of
/// rustc's. The program ra-jit linked of it in an earlier run is taken again while what it
/// was built from is unchanged, as rustc's are ([`Asked::key`]), and while ra-jit is.
fn compile(
    db: &RootDatabase,
    building: &Building<'_>,
    asked: &Asked,
    test: &Test,
    name: &str,
    unit: &Path,
) -> anyhow::Result<Ours> {
    // What a program reads while it is compiled, besides the libraries, is named by rustc's
    // dep-info only, which ra-jit does not write.
    let reads = ["include!", "include_str!", "include_bytes!", "env!"];
    let reads = reads.iter().any(|it| test.program.source.contains(it));
    let ra_jit = std::env::current_exe().ok().and_then(|it| stamp(&it));
    let env = [("RA_JIT", format!("{ra_jit:?}"))];
    let edition = format!("{:?}", test.edition);
    let memo = asked.memo().filter(|_| !reads && ra_jit.is_some()).map(|memo| {
        memo.join(format!("ra-jit-{}", asked.key(&test.program.source, &edition, &env, true)))
    });
    if let Some(binary) = memo.as_ref().map(|it| it.join("rust_out")).filter(|it| it.is_file()) {
        return Ok(Ours::Linked(binary));
    }
    let kept = memo.as_ref().is_some_and(|it| std::fs::create_dir_all(it).is_ok());
    let dir = memo.filter(|_| kept).unwrap_or_else(|| asked.dir.join(name));
    std::fs::create_dir_all(&dir)?;
    let args = crate::Args {
        krate: name.to_owned(),
        batch: Vec::new(),
        widened: Vec::new(),
        out: dir.join("example.o"),
        exe: None,
        report: None,
        prebuilt: Some(unit.to_owned()),
        library: false,
        ..building.args.clone()
    };
    let built = || {
        let mut known = crate::Rejections::default();
        let built = crate::build_unheld(db, building.table, &args, &mut known, building.listed)?;
        if let Some(unset) = built.unset {
            anyhow::bail!(unset);
        }
        let example = crate::crates_named(db, name, &args)?;
        crate::link::Line::of(db, &example, building.sysroot, false)
    };
    match built() {
        Ok(line) => {
            Ok(Ours::Compiled { objects: [args.out.clone(), unit.to_owned()], dir, line, kept })
        }
        Err(why) => {
            if kept {
                _ = std::fs::remove_dir_all(&dir);
            }
            Err(why)
        }
    }
}

/// What the examples are built with: the run of ra-jit that loaded the library, with nothing
/// of rustc's.
pub(crate) struct Building<'a> {
    pub(crate) table: &'a crate::SymbolTable,
    pub(crate) args: &'a crate::Args,
    pub(crate) listed: &'a crate::Listed,
    pub(crate) sysroot: Option<&'a Path>,
}

/// Runs the examples of each library for `target` over the workspace `building` loaded, as
/// rustdoc would have one after another. The code to end with.
pub(crate) fn run_all(
    db: &mut RootDatabase,
    building: &Building<'_>,
    libraries: &Libraries,
    target: &str,
) -> i32 {
    use ide_db::base_db::{self, salsa::Durability};
    let code = libraries.each(|recorded, asked| {
        let asked = match asked {
            Ok(asked) => asked,
            Err(why) => return recorded.to_rustdoc(Some(why)),
        };
        let args = crate::Args {
            krate: asked.root_file(),
            batch: Vec::new(),
            widened: Vec::new(),
            ..building.args.clone()
        };
        let building = Building { args: &args, ..*building };
        let crates: Vec<base_db::Crate> = base_db::all_crates(db).iter().copied().collect();
        let ran = crate::crates_named(db, &args.krate, &args).and_then(|found| {
            let krate = *found.first().context("no such crate")?;
            run(db, &building, asked, krate, target)
        });
        // The examples of the next library are crates of the same names.
        base_db::set_all_crates_with_durability(db, crates, Durability::MEDIUM);
        match ran {
            Ok(code) => code,
            Err(why) => recorded.to_rustdoc(Some(&format!("{why:#}"))),
        }
    });
    libraries.remove_dirs();
    code
}

/// Runs the examples of `krate` for `target` and says how they went as the test harness does.
/// The code to end with.
fn run(
    db: &mut RootDatabase,
    building: &Building<'_>,
    asked: &Asked,
    krate: hir::Crate,
    target: &str,
) -> anyhow::Result<i32> {
    use ide_db::base_db::salsa::Setter as _;
    let start = Instant::now();
    // The examples are taken out of the library as rustdoc reads it, and linked with it as
    // rustc compiles it.
    let library: ide_db::base_db::Crate = krate.into();
    let cfg = library.cfg_options(db).clone();
    document(db, krate);
    let (tests, left_out) = hir::attach_db(db, || tests(db, asked, krate));
    library.set_cfg_options(db).to(cfg);
    let (plans, unit) = plan(db, building, asked, krate, &tests, target);
    let db: &RootDatabase = db;
    let out = std::io::stdout();
    writeln!(&out, "\nrunning {} test{}", tests.len(), if tests.len() == 1 { "" } else { "s" })?;
    let shown = Mutex::new(Progress {
        terse: asked.wanted.terse,
        total: tests.len(),
        ..Default::default()
    });
    let ended: Mutex<Vec<(usize, anyhow::Result<Ended>)>> = Mutex::new(Vec::new());
    let counted = Counted::default();
    // The examples rustc builds run while ra-jit compiles its, a few at a time on snapshots
    // of the database (a small program's build leaves most cores idle), and each of those is
    // linked, run and removed as soon as it is compiled.
    let (ready, next) = std::sync::mpsc::channel::<(usize, Option<Ours>)>();
    let next = Mutex::new(next);
    let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
    let threads = cores.min(tests.len());
    let ours: Vec<(usize, &String)> = match &unit {
        Some(_) => plans
            .iter()
            .enumerate()
            .filter_map(|(at, plan)| match plan {
                Plan::Ours(name) => Some((at, name)),
                _ => None,
            })
            .collect(),
        None => Vec::new(),
    };
    let compilers = (cores / 16).clamp(1, 4).min(ours.len());
    let ours = Mutex::new(ours.into_iter());
    let compiling = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let job = next.lock().unwrap_or_else(|it| it.into_inner()).recv();
                    let Ok((at, ours)) = job else { break };
                    let test = &tests[at];
                    let dir = asked.dir.join(at.to_string());
                    let result = test.run(asked, &dir, ours.as_ref(), target, &counted);
                    _ = std::fs::remove_dir_all(&dir);
                    if let Some(dir) = ours.as_ref().and_then(Ours::scratch) {
                        _ = std::fs::remove_dir_all(dir);
                    }
                    let mut progress = shown.lock().unwrap_or_else(|it| it.into_inner());
                    _ = progress.ended(&mut out.lock(), test, &result);
                    drop(progress);
                    ended.lock().unwrap_or_else(|it| it.into_inner()).push((at, result));
                }
            });
        }
        for (at, plan) in plans.iter().enumerate() {
            match plan {
                Plan::Rustc => _ = ready.send((at, None)),
                Plan::Checked => _ = ready.send((at, Some(Ours::Checked))),
                Plan::Ours(_) => {}
            }
        }
        let compilers: Vec<_> = (0..compilers)
            .map(|_| {
                let (db, ready, ours) = (db.clone(), ready.clone(), &ours);
                let unit = unit.as_deref();
                let (tests, counted) = (&tests, &counted);
                scope.spawn(move || {
                    let Some(unit) = unit else { return };
                    loop {
                        let job = ours.lock().unwrap_or_else(|it| it.into_inner()).next();
                        let Some((at, name)) = job else { break };
                        let compiled = compile(&db, building, asked, &tests[at], name, unit);
                        let compiled = compiled.inspect_err(|why| {
                            eprintln!("ra-jit: {} is rustc's: {why:#}", tests[at].name);
                        });
                        if let Ok(Ours::Linked(_)) = &compiled {
                            counted.remembered.fetch_add(1, Ordering::Relaxed);
                        }
                        _ = ready.send((at, compiled.ok()));
                    }
                })
            })
            .collect();
        drop(ready);
        for compiler in compilers {
            _ = compiler.join();
        }
    });
    let compiling = compiling.elapsed();
    let remembered = counted.remembered.load(Ordering::Relaxed);
    let remembered = match remembered {
        0 => String::new(),
        n => format!(" ({n} unchanged since the last run)"),
    };
    eprintln!(
        "doctests: {} built by ra-jit, {} by rustc; compiling took {compiling:.1?}{remembered}",
        counted.ra_jit.load(Ordering::Relaxed),
        counted.rustc.load(Ordering::Relaxed),
    );
    let mut ended = ended.into_inner().unwrap_or_else(|it| it.into_inner());
    ended.sort_by_key(|(at, _)| *at);
    let (mut passed, mut ignored, mut failed) = (0, 0, Vec::new());
    for (at, result) in ended {
        match result {
            Ok(Ended::Passed) => passed += 1,
            Ok(Ended::Ignored) => ignored += 1,
            // libtest lists a failure by the name alone, without how it is tested.
            Ok(Ended::Failed(shown)) => failed.push((tests[at].name.clone(), shown)),
            Err(error) => failed.push((tests[at].name.clone(), format!("ra-jit: {error:#}"))),
        }
    }
    if !failed.is_empty() {
        writeln!(&out, "\nfailures:\n")?;
        for (name, shown) in &failed {
            writeln!(&out, "---- {name} stdout ----\n{shown}\n")?;
        }
        writeln!(&out, "\nfailures:")?;
        for (name, _) in &failed {
            writeln!(&out, "    {name}")?;
        }
    }
    writeln!(
        &out,
        "\ntest result: {}. {passed} passed; {} failed; {ignored} ignored; 0 measured; {left_out} filtered out; finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        start.elapsed().as_secs_f64(),
    )?;
    Ok(if failed.is_empty() { 0 } else { FAILED })
}
