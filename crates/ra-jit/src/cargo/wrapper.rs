//! ra-jit as the wrapper cargo runs rustc through for the crates of a workspace, under
//! `cargo ra-jit` and, once `ra-jit enable` named it in cargo's configuration, under plain
//! cargo. What rustc built of a crate is recorded for the daemon; a crate that is linked is
//! also asked for its link line and for the objects it was linked from. Under plain cargo
//! a program the daemon has loaded is the daemon's to build when cargo asks for it again,
//! and one it has not is rustc's, after which the daemon loads the build.

use std::{
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread::JoinHandle,
};

use anyhow::Context as _;

use super::{
    ancestor::Cargo,
    cli,
    daemon::{self, NotBuilt},
    own_macro, own_script, say, script,
    state::{State, place},
    switch::Enabled,
    units::{Copied, Record, stamp_now},
};
use crate::{
    dep_info,
    socket::{private_dir, private_file},
    stub::VALUED,
};

/// The variable `cargo ra-jit` sets for the cargo it runs: the wrapper records what rustc
/// builds and leaves the rest to the command that drives cargo.
pub(crate) const DRIVEN: &str = "RA_JIT_CARGO_DRIVES";

/// The variable `cargo ra-jit` sets in the first build of a workspace: the roots of the
/// crates that are the daemon's to compile, as a list of paths.
pub(crate) const STAGED: &str = "RA_JIT_STAGED_CRATES";

/// The variable that says cargo runs every crate's rustc through this wrapper, and the
/// workspace's through it twice.
pub(crate) const EVERY_CRATE: &str = "RA_JIT_EVERY_CRATE";
/// The file in the record of a unit that says rustc built what stands in for the crate.
pub(crate) const STANDS_IN: &str = "stands-in";

/// What the variables that name the programs of a build start with.
const PROGRAM_OF: &str = "CARGO_BIN_EXE_";
/// The variable that names the directory cargo gives the integration tests of a build.
const SCRATCH: &str = "CARGO_TARGET_TMPDIR";

/// The file in the daemon's directory that has a line for every program plain cargo asked
/// for: who linked it, and why when it was rustc.
const TALLY: &str = "plain-cargo.log";
/// Why rustc links a program the daemon was not asked for: the first build of it.
const NOT_LOADED: &str = "the daemon had not loaded the build yet";
/// The directory there with a file for every cargo whose build is being loaded.
const SETTLING: &str = "settling";
/// The codegen option cargo names rustc's incremental directory with, as its own argument
/// after `-C`.
const INCREMENTAL: &str = "incremental";
/// The directory, in cargo's incremental one, of the checks that go with the daemon's builds.
const CHECKS: &str = "ra-jit-check";

/// Builds what cargo asked rustc for and ends as rustc would have.
pub(crate) fn command(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let mut rustc = args.next().context("cargo names rustc first")?;
    // A cargo that asks through ra-jit as both wrappers names it twice.
    let mut twice = None;
    if super::switch::is_wrapper(Path::new(&rustc)) {
        twice = Some(rustc);
        rustc = args.next().context("cargo names rustc after the wrapper")?;
    }
    let args: Vec<OsString> = args.collect();
    if let Some(replied) = crate::answers::reply(&rustc, &args) {
        std::process::exit(replied?);
    }
    let only_asks = std::env::var_os(crate::answers::ONLY_ASKS).is_some();
    // A crate cargo runs through this wrapper alone, when it is cargo's `RUSTC_WRAPPER` too,
    // is not the workspace's.
    let outside = twice.is_none() && !only_asks && std::env::var_os(EVERY_CRATE).is_some();
    // The wrapper the user had for the workspace's crates still runs rustc.
    let mut compiler = match Enabled::read().and_then(|it| it.chained) {
        // What a cargo that only asks through ra-jit compiles is compiled as it would be
        // without it, by the workspace's wrapper when there is one.
        _ if only_asks => {
            let mut command = Command::new(twice.as_ref().unwrap_or(&rustc));
            command.args(twice.as_ref().map(|_| &rustc)).env_remove(crate::answers::ONLY_ASKS);
            command
        }
        Some(chained) if !outside => {
            let mut command = Command::new(chained);
            command.arg(&rustc);
            command
        }
        _ => rustc_of(&rustc),
    };
    compiler.args(&args);
    if only_asks {
        std::process::exit(for_load(compiler, &rustc, &args)?);
    }
    if outside {
        std::process::exit(self::outside(compiler, &rustc, &args)?);
    }
    let Some(unit) = Unit::of(&args) else {
        std::process::exit(compiler.status()?.code().unwrap_or(1));
    };
    let plain = unit.linked && std::env::var_os(DRIVEN).is_none();
    let dir = unit.records.join(&unit.id);
    if unit.script && std::env::var_os(own_script::BY_RUSTC).is_none() {
        match unit.by_ra_jit(&rustc, &dir) {
            Ok(()) => std::process::exit(0),
            Err(why) => unit.script_to_rustc(&why),
        }
    }
    if unit.is_staged() {
        match unit.stood_in(&rustc, &dir) {
            Ok(code) => std::process::exit(code),
            // rustc compiles the crate as cargo asked, and the daemon links against that.
            Err(why) => eprintln!("warning: rustc compiles `{}`: {why:#}", unit.id),
        }
    }
    let known = if plain { State::of_unit(&dir) } else { Vec::new() };
    if !known.is_empty() {
        match unit.by_daemon(&dir, &known, &rustc) {
            Ok(()) => {
                tally("daemon", &unit.crate_name);
                std::process::exit(0);
            }
            Err(why) => unit.left_to_rustc(&why),
        }
    }
    let stamp = stamp_now();
    let copies = dir.join(stamp.to_string());
    let line = copies.join("link.line");
    if unit.linked {
        std::fs::create_dir_all(&copies)?;
        compiler.arg(format!("--print=link-args={}", line.display())).arg("-Csave-temps");
        if unit.for_apple() {
            for arg in super::plan::rustc_link_args() {
                compiler.arg(format!("-Clink-arg={arg}"));
            }
        }
        if unit.for_linux() {
            compiler.arg(format!("-Clink-arg={}", super::plan::BUILD_ID));
            compiler.arg(format!("-Clink-arg={}", super::plan::kept_builtins()));
        }
    }
    let status =
        compiler.status().with_context(|| format!("running {}", Path::new(&rustc).display()))?;
    if status.success()
        && let Err(error) = unit.record(stamp, &copies, unit.linked.then_some(line), &unit.source)
    {
        // The build is good; the daemon has no part in this crate until it is built again.
        eprintln!("warning: ra-jit did not record `{}`: {error:#}", unit.id);
    }
    if plain && status.success() {
        // The program is rustc's now: the builds that had it are loaded again from this one.
        if known.is_empty() {
            tally("rustc", NOT_LOADED);
        }
        State::forget(&known);
        let _ = std::fs::remove_file(dir.join(State::STATES));
        if let Err(why) = loaded_later() {
            unit.left_to_rustc(&why);
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

/// The compiler cargo named, `rustc`, for a crate cargo asked it for: run then, or, for a
/// proc macro ra-jit linked, when a crate of rustc's that links it is compiled
/// ([`own_macro::rustc_first`]).
pub(crate) fn rustc_of(rustc: &OsStr) -> Command {
    Command::new(rustc)
}

/// Builds a crate of a package outside the workspace as cargo asked: its build script by
/// ra-jit where it can be, the rest by `compiler`, rustc, whose libraries and proc macros are
/// described for the scripts that link them. The code to end with.
fn outside(mut compiler: Command, rustc: &OsString, args: &[OsString]) -> anyhow::Result<i32> {
    let unit = Unit::of(args);
    if let Some(unit) = unit.as_ref().filter(|it| it.script)
        && std::env::var_os(own_script::BY_RUSTC).is_none()
    {
        match unit.by_ra_jit(rustc, &unit.records.join(&unit.id)) {
            Ok(()) => return Ok(0),
            Err(why) => unit.script_to_rustc(&why),
        }
    }
    let status = compiler.status()?;
    if let Some(unit) = unit.filter(|it| status.success() && !it.script) {
        let record = unit.as_record(stamp_now(), Vec::new(), None);
        if record.is_rlib() || record.runs_in_build() {
            record.write_described(&unit.records)?;
        }
    }
    Ok(status.code().unwrap_or(1))
}

/// Builds a crate for rust-analyzer's `cargo check --compile-time-deps` as `compiler`, rustc,
/// does without ra-jit, but a proc macro when it is ra-jit's ([`own_macro::wanted`]): the
/// libraries rustc compiles are then described for the proc macros that link them. The code
/// to end with.
fn for_load(mut compiler: Command, rustc: &OsString, args: &[OsString]) -> anyhow::Result<i32> {
    if !own_macro::wanted() {
        return Ok(compiler.status()?.code().unwrap_or(1));
    }
    let unit = Unit::of(args);
    let record = unit.as_ref().map(|it| it.as_record(stamp_now(), Vec::new(), None));
    if let Some((unit, record)) =
        unit.as_ref().zip(record.as_ref()).filter(|(_, it)| it.is_proc_macro())
    {
        let built = own_macro::made(record).ok_or("cargo names no place for it".to_owned());
        let dir = unit.records.join(&unit.id).join(record.stamp.to_string());
        let built = built.and_then(|made| {
            own_script::compile_as(rustc, &unit.args, &unit.cwd, &unit.source, &made, &dir)
        });
        match built {
            Ok(()) => {
                record.write_described(&unit.records)?;
                return Ok(0);
            }
            Err(why) => {
                let then = format!("rustc compiles the proc macro `{}`", unit.crate_name);
                say::fallback(&why, &then);
            }
        }
    }
    let text: Vec<String> = args.iter().filter_map(|it| it.to_str().map(str::to_owned)).collect();
    own_macro::rustc_first(rustc, &text, &std::env::current_dir()?)?;
    let status = compiler.status()?;
    if let Some((unit, record)) = unit.as_ref().zip(record.as_ref())
        && status.success()
        && (record.is_rlib() || record.runs_in_build())
    {
        record.write_described(&unit.records)?;
    }
    Ok(status.code().unwrap_or(1))
}

/// Starts a process that has the daemon load the build of the cargo above this one, when
/// none was started for that cargo yet. `Err` is why the build is not one the daemon loads.
fn loaded_later() -> Result<(), String> {
    let cargo = Cargo::above()?;
    let typed = cargo.args.join(" ");
    let asked =
        cli::of_cargo(&cargo.args).ok_or(format!("`cargo {typed}` is not a build ra-jit loads"))?;
    if let Some(why) = asked.only_cargo() {
        return Err(why);
    }
    let said = |it: &dyn std::fmt::Display| format!("the build is not loaded: {it:#}");
    let home = daemon::home().map_err(|it| said(&it))?;
    let settling = home.join(SETTLING);
    private_dir(&settling).map_err(|it| said(&it))?;
    let ours = settling.join(cargo.pid.to_string());
    match private_file(OpenOptions::new().write(true).create_new(true), &ours) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(said(&error)),
    }
    let log = private_file(OpenOptions::new().create(true).append(true), &home.join("settle.log"))
        .map_err(|it| said(&it))?;
    let mut settle = Command::new(std::env::current_exe().map_err(|it| said(&it))?);
    settle.args(["cargo", super::SETTLE]).arg(cargo.pid.to_string()).args(&cargo.args);
    settle.env_clear().envs(cargo.env).env("CARGO", &cargo.executable).current_dir(&cargo.cwd);
    settle.stdin(Stdio::null()).stdout(log.try_clone().map_err(|it| said(&it))?).stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut settle, 0);
    settle.spawn().map(drop).map_err(|it| {
        settled(cargo.pid);
        said(&it)
    })
}

/// The build of the cargo `cargo` is loaded, or is not going to be.
pub(crate) fn settled(cargo: u32) {
    if let Ok(home) = daemon::home() {
        let _ = std::fs::remove_file(home.join(SETTLING).join(cargo.to_string()));
    }
}

/// Writes down that `who` linked a program for plain cargo, and `what` of it: its name
/// when it was the daemon, why it was not when it was rustc. `error` is the daemon's too:
/// it found an error in the source, which no rustc ran to say.
fn tally(who: &str, what: &str) {
    let log = daemon::home().and_then(|home| {
        private_dir(&home)?;
        Ok(private_file(OpenOptions::new().create(true).append(true), &home.join(TALLY))?)
    });
    if let Ok(mut log) = log {
        let _ = writeln!(log, "{who}\t{}", what.replace('\n', " "));
    }
}

/// The lines of `ra-jit status` that count the programs plain cargo asked for.
pub(crate) fn tallied() -> Vec<String> {
    let log = daemon::home().ok().and_then(|it| std::fs::read_to_string(it.join(TALLY)).ok());
    let Some(log) = log else { return Vec::new() };
    let mut reasons = std::collections::BTreeMap::<&str, u64>::new();
    let (mut daemon, mut rustc) = (0u64, 0u64);
    for line in log.lines() {
        match line.split_once('\t') {
            Some(("daemon" | "error", _)) => daemon += 1,
            Some((_, why)) => {
                rustc += 1;
                *reasons.entry(why).or_default() += 1;
            }
            None => {}
        }
    }
    let mut lines = vec![format!("  {daemon} programs linked by the daemon, {rustc} by rustc")];
    lines.extend(reasons.iter().map(|(why, count)| format!("    {count} x {why}")));
    lines
}

/// A run of rustc that compiles a crate, as cargo's arguments describe it.
struct Unit {
    id: String,
    crate_name: String,
    args: Vec<String>,
    source: PathBuf,
    cwd: PathBuf,
    records: PathBuf,
    /// Whether rustc links a program of the workspace from it.
    linked: bool,
    /// Whether rustc links a build script from it.
    script: bool,
}

impl Unit {
    /// `None` for a run that compiles nothing to record: a question to rustc (`--print`,
    /// `-vV`), source on standard input, or arguments that are not text.
    fn of(args: &[OsString]) -> Option<Unit> {
        let args: Vec<String> =
            args.iter().map(|it| it.to_str().map(str::to_owned)).collect::<Option<_>>()?;
        let value = |flag: &str| {
            let at = args.iter().position(|it| it == flag)?;
            args.get(at + 1).cloned()
        };
        let crate_name = value("--crate-name")?;
        let out_dir = value("--out-dir")?;
        if args.iter().any(|it| it == "-" || it.starts_with("--print")) {
            return None;
        }
        // The one argument that is a path to a Rust file and not the value of a flag.
        let source = (0..args.len()).find(|&at| {
            args[at].ends_with(".rs") && !(at > 0 && VALUED.contains(&args[at - 1].as_str()))
        })?;
        let cwd = std::env::current_dir().ok()?;
        let mut unit = Unit {
            id: String::new(),
            crate_name,
            source: cwd.join(&args[source]),
            records: Record::dir_for(&cwd.join(&out_dir)),
            args,
            cwd,
            linked: false,
            script: false,
        };
        let record = unit.as_record(0, Vec::new(), None);
        let emits_link = unit.args.iter().any(|it| {
            it.strip_prefix("--emit=").is_some_and(|kinds| kinds.split(',').any(|it| it == "link"))
        });
        // A unit that is only checked leaves nothing the daemon reads.
        if !emits_link {
            return None;
        }
        unit.script = record.is_build_script();
        unit.linked = record.is_linked() && !record.runs_in_build();
        let extra = record.codegen("extra-filename").unwrap_or_default();
        unit.id = Record::id_of(&unit.crate_name, extra, &unit.cwd.join(&out_dir));
        Some(unit)
    }

    /// Has the daemon build the program and puts it where rustc would have, with the
    /// dep-info rustc would have written. `known` are the builds that have the program, and
    /// `dir` is where the unit is recorded. `Err` is why the program is rustc's to link.
    fn by_daemon(&self, dir: &Path, known: &[String], rustc: &OsString) -> Result<(), String> {
        let mut read = known.iter().map(|key| State::read(key).map_err(|why| (key, why)));
        let mut state = match read.find_map(Result::ok) {
            Some(state) => state,
            None => {
                let why = known.iter().find_map(|it| State::read(it).err().flatten());
                return Err(why.unwrap_or("the build is not one the daemon has loaded").to_owned());
            }
        };
        if let Some(why) = state.refusal.clone().or_else(|| state.broken_guard()) {
            return Err(why);
        }
        let (checkout, at) = state.asked_for(dir)?;
        let deps = self.dep_info(rustc);
        let answer = super::built(checkout).and_then(|programs| {
            let said = programs.get(at).cloned();
            said.unwrap_or(Err("the daemon said nothing of it".to_owned()))
                .map_err(NotBuilt::Failed)
        });
        let read = deps.join().map_err(|_| "rustc did not run".to_owned());
        // An error in the source fails the unit, as ra-jit writes it, and cargo says the
        // crate did not compile. What the daemon did not build otherwise, rustc does.
        match answer {
            Ok(()) => {}
            Err(NotBuilt::Error(said)) => {
                tally("error", &self.crate_name);
                eprintln!("{said}");
                std::process::exit(super::FAILED);
            }
            Err(why) => return Err(why.why()),
        }
        let read = read?.map_err(|it| format!("rustc did not run: {it}"))?;
        if !read.status.success() {
            let _ = std::io::stderr().write_all(&read.stderr);
            std::process::exit(super::FAILED);
        }
        let binary = &checkout.programs[at].binary;
        // Cargo starts the program with no word to the daemon, which may still be writing it.
        let _unwritten = crate::splice::unwritten(binary);
        let record = self.as_record(0, Vec::new(), None);
        let linked = record.outputs().into_iter().next().ok_or("rustc names no executable")?;
        place(binary, &linked).map_err(|it| format!("installing {}: {it}", linked.display()))?;
        state.cargo_has(dir, binary).map_err(|it| format!("keeping {}: {it}", linked.display()))?;
        // Every other build with the program kept the one cargo had before.
        State::forget(known.iter().filter(|it| **it != state.key));
        let _ = std::io::stderr().write_all(&read.stderr);
        Ok(())
    }

    /// Has ra-jit compile and link the build script, where rustc would have put it, and
    /// records it as [`Unit::record`] does a script of rustc's. `dir` is where the unit is
    /// recorded. `Err` is why rustc is to compile it.
    fn by_ra_jit(&self, rustc: &OsString, dir: &Path) -> Result<(), String> {
        let stamp = stamp_now();
        let copies = dir.join(stamp.to_string());
        let record = self.as_record(0, Vec::new(), None);
        let exe = record.outputs().into_iter().next().ok_or("rustc names no executable")?;
        own_script::compile(rustc, &self.args, &self.cwd, &self.source, &exe, &copies)?;
        self.record(stamp, &copies, None, &self.source).map_err(|it| format!("{it:#}"))
    }

    /// Says in one line why rustc compiles the build script and not ra-jit.
    fn script_to_rustc(&self, why: &str) {
        let package = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
        say::fallback(why, &format!("rustc compiles the build script of `{package}`"));
    }

    /// Has rustc write the dep-info cargo reads to know what the program was compiled
    /// from, while the daemon builds it. Rustc stops once it has read the source: it
    /// checks nothing, and the daemon builds what rust-analyzer passes.
    fn dep_info(&self, rustc: &OsString) -> JoinHandle<std::io::Result<Output>> {
        let args = self.args.iter().map(|it| match it.split_once('=') {
            Some(("--emit", _)) => "--emit=dep-info".to_owned(),
            Some((INCREMENTAL, dir)) => format!("{INCREMENTAL}={dir}/{CHECKS}"),
            _ => it.clone(),
        });
        let mut command = Command::new(rustc);
        command.args(args).stdin(Stdio::null());
        std::thread::spawn(move || command.output())
    }

    /// The target rustc is told to compile the crate for, when another than the host.
    fn target(&self) -> Option<&str> {
        let at = self.args.iter().position(|it| it == "--target");
        let target = at.and_then(|at| self.args.get(at + 1)).map(String::as_str);
        target.or_else(|| self.args.iter().find_map(|it| it.strip_prefix("--target=")))
    }

    /// Whether rustc compiles the crate for an Apple platform, whose linker is told how much
    /// room to leave after the load commands.
    fn for_apple(&self) -> bool {
        self.target().map_or(cfg!(target_vendor = "apple"), |it| it.contains("-apple-"))
    }

    /// Whether rustc compiles the crate for Linux and links it through a C compiler, which is
    /// told to have the linker give the program a build ID ([`super::plan::BUILD_ID`]). A
    /// linker run itself takes no `-Wl,`.
    fn for_linux(&self) -> bool {
        let codegen = |name: &str| {
            let flag = format!("{name}=");
            let mut args = self.args.iter();
            let mut found = None;
            while let Some(arg) = args.next() {
                let value = match arg.strip_prefix("-C") {
                    Some("") => args.next().and_then(|it| it.strip_prefix(&flag)),
                    Some(rest) => rest.strip_prefix(&flag),
                    None => None,
                };
                found = value.or(found);
            }
            found
        };
        // `ld`, `ld.lld`, `x86_64-linux-gnu-ld`, `rust-lld`, `mold`.
        let by_itself = codegen("linker").is_some_and(|it| {
            let name = Path::new(it).file_stem().and_then(|it| it.to_str()).unwrap_or(it);
            ["ld", "lld", "mold"].iter().any(|ld| name == *ld || name.ends_with(&format!("-{ld}")))
        });
        let by_itself = by_itself || codegen("linker-flavor").is_some_and(|it| !it.contains("cc"));
        self.target().map_or(cfg!(target_os = "linux"), |it| it.contains("-linux-")) && !by_itself
    }

    /// Whether the crate is one the first build of the workspace leaves to the daemon.
    fn is_staged(&self) -> bool {
        let Some(staged) = std::env::var_os(STAGED) else { return false };
        let lists = std::env::split_paths(&staged).any(|it| it == self.source);
        lists && !self.script && !self.args.iter().any(|it| it == "--test")
    }

    /// Has rustc do of the crate what it does in a build by the daemon: check it, and
    /// build what stands in for it, which is what cargo links and has no code. `dir` is
    /// where the unit is recorded. `Ok` is the status rustc's check ended with; `Err` is
    /// why nothing stands in for the crate.
    ///
    /// What stands in is built first and from the libraries that stand in for the crates
    /// it depends on, and the check reads the metadata of the checks of those. Cargo
    /// starts the units that depend on this one when the check has written the metadata,
    /// which is when both are there.
    fn stood_in(&self, rustc: &OsString, dir: &Path) -> anyhow::Result<i32> {
        let program = crate::stub::is_program(&self.args).context("not a library or a program")?;
        let stamp = stamp_now();
        let copies = dir.join(stamp.to_string());
        std::fs::create_dir_all(&copies)?;
        let line = copies.join("link.line");
        let extra = self.as_record(0, Vec::new(), None);
        let stem =
            format!("{}{}", self.crate_name, extra.codegen("extra-filename").unwrap_or_default());
        let source = copies.join(format!("{stem}.ra-jit.rs"));
        std::fs::write(&source, crate::stub::text_of(&self.args, program, &self.source))?;

        let mut stand_in = Command::new(rustc);
        let mut args = self.args.iter().peekable();
        while let Some(arg) = args.next() {
            let incremental = |it: &&String| it.starts_with(&format!("{INCREMENTAL}="));
            if arg == "-C" && args.peek().is_some_and(incremental) {
                args.next();
            } else if self.cwd.join(arg) == self.source {
                stand_in.arg(&source);
            } else if arg.starts_with("--emit=") {
                stand_in.arg("--emit=link");
            } else {
                stand_in.arg(self.standing_in(arg).unwrap_or_else(|| arg.clone()));
            }
        }
        if program {
            stand_in.arg(format!("--print=link-args={}", line.display())).arg("-Csave-temps");
            if self.for_linux() {
                stand_in.arg(format!("-Clink-arg={}", super::plan::BUILD_ID));
                stand_in.arg(format!("-Clink-arg={}", super::plan::kept_builtins()));
            }
        }
        let built = stand_in.stdin(Stdio::null()).output().context("running rustc")?;
        anyhow::ensure!(
            built.status.success(),
            "what stands in for it does not build: {}",
            String::from_utf8_lossy(&built.stderr).trim_end()
        );
        std::fs::write(dir.join(STANDS_IN), "")?;

        // The crate is not checked by rustc: the daemon's front end is what checks it, and a
        // second compile beside the daemon's would only compete with it.
        self.record(stamp, &copies, program.then_some(line), &source)?;
        Ok(0)
    }

    /// `--extern`'s value `named` with the library that stands in for the crate, when the
    /// value is the metadata of a crate that has one.
    fn standing_in(&self, named: &str) -> Option<String> {
        let (name, path) = named.split_once('=')?;
        let id = Record::id_of_file(&self.cwd.join(path), "lib", ".rmeta")?;
        self.records
            .join(id)
            .join(STANDS_IN)
            .exists()
            .then(|| format!("{name}={}", Path::new(path).with_extension("rlib").display()))
    }

    /// `--extern`'s value `named` with the metadata rustc's check wrote of the crate, when
    /// the value is the library that stands in for one.
    fn checked_against(&self, named: &str) -> Option<String> {
        let (name, path) = named.split_once('=')?;
        let id = Record::id_of_file(&self.cwd.join(path), "lib", ".rlib")?;
        self.records
            .join(id)
            .join(STANDS_IN)
            .exists()
            .then(|| format!("{name}={}", Path::new(path).with_extension("rmeta").display()))
    }

    /// Says in one line why rustc links the program and not the daemon, and counts it.
    fn left_to_rustc(&self, why: &str) {
        say::fallback(why, &format!("rustc links `{}`", self.crate_name));
        tally("rustc", why);
    }

    fn as_record(&self, stamp: u128, copies: Vec<Copied>, link_line: Option<PathBuf>) -> Record {
        Record {
            id: self.id.clone(),
            crate_name: self.crate_name.clone(),
            cwd: self.cwd.clone(),
            args: self.args.clone(),
            source: self.source.clone(),
            stamp,
            copies,
            link_line,
            env: std::env::vars()
                .filter(|(name, _)| name.starts_with(PROGRAM_OF) || name == SCRATCH)
                .collect(),
            crate_env: std::env::vars()
                .filter(|(name, _)| super::own_script::read_by_crates(name))
                .collect(),
        }
    }

    /// Copies what a later build of the unit writes over and writes the record.
    /// `compiled` is the root of the source rustc was given.
    fn record(
        &self,
        stamp: u128,
        to: &Path,
        link_line: Option<PathBuf>,
        compiled: &Path,
    ) -> anyhow::Result<()> {
        let record = self.as_record(stamp, Vec::new(), link_line.clone());
        // The executable is the daemon's to link; of the outputs it reads the libraries.
        let libraries =
            record.outputs().into_iter().filter(|it| it.extension().is_some_and(|it| it == "rlib"));
        let mut originals: Vec<(PathBuf, bool)> =
            libraries.filter(|it| it.exists()).map(|it| (it, false)).collect();
        if let Some(line) = &link_line {
            let line = std::fs::read_to_string(line)
                .with_context(|| format!("rustc wrote no link line to {}", line.display()))?;
            let args = shlex::split(&line).context("the link line is not quoted as a shell's")?;
            let objects = args
                .into_iter()
                .filter(|it| it.ends_with(".rcgu.o") || super::plan::is_symbols(it))
                .map(PathBuf::from);
            // The objects are rustc's to delete once it has linked, and kept for the daemon
            // alone: they are moved rather than copied, as copying those of a large program
            // holds up the build. On Apple's systems the program's debug info stays in them,
            // where a debugger looks for it.
            let moved = !self.for_apple();
            originals.extend(objects.filter(|it| it.exists()).map(|it| (it, moved)));
        }
        std::fs::create_dir_all(to)?;
        let mut copies = Vec::new();
        for (original, moved) in originals {
            let name = original.file_name().context("an output without a name")?;
            let copy = to.join(name);
            // Another file system takes a copy.
            if !(moved && std::fs::rename(&original, &copy).is_ok()) {
                std::fs::copy(&original, &copy)
                    .with_context(|| format!("copying {}", original.display()))?;
            }
            copies.push(Copied { original, copy });
        }
        // The daemon tells which crate a file of rustc's is by the dep-info next to it, of
        // which it reads the first source: the root of the crate.
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        let root = dep_info::escaped(&compiled.display().to_string());
        std::fs::write(to.join(format!("{stem}.d")), format!("{stem}: {root}\n"))?;
        if self.script
            && let Some(linked) = record.outputs().first()
        {
            script::stand_in_front(linked)?;
        }
        self.as_record(stamp, copies, link_line).write(&self.records)
    }
}
