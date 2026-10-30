//! blaze as the `RUSTC_WRAPPER` of the cargo rust-analyzer runs to load a workspace
//! (`cargo check --compile-time-deps`): it answers what that cargo asks rustc, and has blaze
//! link the proc macros it is to expand with ([`own_macro`]). What else that cargo compiles is
//! rustc's until blaze compiles and runs the build scripts and proc macros of a load itself
//! (docs/no-rustc.md, steps I and J). It is no wrapper of any other cargo: plain cargo is blaze
//! after `blaze enable`, and a build is blaze's whole.

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::Command,
};

use anyhow::Context as _;

use super::{
    own_macro, own_script, say, script,
    units::{Copied, Record, stamp_now},
};

/// The flags of rustc that cargo passes with their value as the next argument.
pub(crate) const VALUED: &[&str] = &[
    "-C",
    "-L",
    "-Z",
    "-l",
    "-A",
    "-W",
    "-D",
    "-F",
    "--cfg",
    "--check-cfg",
    "--extern",
    "--out-dir",
    "--target",
    "--crate-name",
    "--crate-type",
    "--cap-lints",
    "--emit",
    "--edition",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
];

/// What a build runs the wrapper with: cargo's rustc and its arguments. Ends as rustc would.
pub(crate) fn command(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let rustc = args.next().context("cargo names rustc first")?;
    let args: Vec<OsString> = args.collect();
    if let Some(replied) = crate::answers::reply(&rustc, &args) {
        std::process::exit(replied?);
    }
    if std::env::var_os(crate::answers::ONLY_ASKS).is_none() {
        say::error(
            "blaze is no wrapper of cargo's builds: plain `cargo` is blaze after `blaze enable`, \
             and an older `enable` named this wrapper in cargo's configuration; `blaze enable` \
             again takes it out",
        );
        std::process::exit(super::FAILED);
    }
    let mut compiler = rustc_of(&rustc);
    compiler.args(&args).env_remove(crate::answers::ONLY_ASKS);
    std::process::exit(for_load(compiler, &rustc, &args)?);
}

/// The compiler cargo named, `rustc`, for a crate cargo asked it for: run then, or, for a
/// proc macro ra-jit linked, when a crate of rustc's that links it is compiled
/// ([`own_macro::rustc_first`]).
pub(crate) fn rustc_of(rustc: &OsStr) -> Command {
    Command::new(rustc)
}

/// Builds a crate for rust-analyzer's `cargo check --compile-time-deps` as `compiler`, rustc,
/// does without ra-jit, but a proc macro when it is ra-jit's ([`own_macro::wanted`]): the
/// libraries rustc compiles are then described for the proc macros that link them. The code
/// to end with.
fn for_load(mut compiler: Command, rustc: &OsString, args: &[OsString]) -> anyhow::Result<i32> {
    let unit = Unit::of(args);
    if !own_macro::wanted() {
        let status = compiler.status()?;
        stand_in_front_of_rustcs(unit.as_ref(), status)?;
        return Ok(status.code().unwrap_or(1));
    }
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
    stand_in_front_of_rustcs(unit.as_ref(), status)?;
    if let Some((unit, record)) = unit.as_ref().zip(record.as_ref())
        && status.success()
        && (record.is_rlib() || record.runs_in_build())
    {
        record.write_described(&unit.records)?;
    }
    Ok(status.code().unwrap_or(1))
}

/// Has cargo run a build script rustc linked, `unit` when it is one and `status` says rustc
/// did, through `ra-jit build-script`: every build script runs in the sandbox, whoever
/// compiled it.
fn stand_in_front_of_rustcs(
    unit: Option<&Unit>,
    status: std::process::ExitStatus,
) -> anyhow::Result<()> {
    let Some(unit) = unit.filter(|it| it.script && status.success()) else { return Ok(()) };
    let record = unit.as_record(0, Vec::new(), None);
    match record.outputs().first() {
        Some(linked) => script::stand_in_front(linked),
        None => Ok(()),
    }
}

/// A run of rustc that compiles a crate, as cargo's arguments describe it.
struct Unit {
    id: String,
    crate_name: String,
    args: Vec<String>,
    source: PathBuf,
    cwd: PathBuf,
    records: PathBuf,
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
        let extra = record.codegen("extra-filename").unwrap_or_default();
        unit.id = Record::id_of(&unit.crate_name, extra, &unit.cwd.join(&out_dir));
        Some(unit)
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
}

/// What the variables that name the programs of a build start with.
const PROGRAM_OF: &str = "CARGO_BIN_EXE_";
/// The variable that names the directory cargo gives the integration tests of a build.
const SCRATCH: &str = "CARGO_TARGET_TMPDIR";
