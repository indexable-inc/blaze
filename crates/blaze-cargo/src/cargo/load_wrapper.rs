//! blaze as the `RUSTC_WRAPPER` of a cargo that a load of blaze's runs
//! ([`crate::answers::ready_cargo`]): cargo runs `blaze-rustc rustc ...`, which becomes
//! `blaze-engine rustc-wrapper rustc ...`. What cargo asks rustc is answered by blaze
//! ([`crate::answers`]), and what rust-analyzer's `cargo check --compile-time-deps` builds
//! for the load is built by [`for_load`]. No other cargo runs blaze as rustc.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context as _;

use super::{
    inert_script, own_macro, own_script, script,
    units::{Record, stamp_now},
    wasi_script,
};
use crate::{dep_info, stub::VALUED};

/// `blaze-rustc`, installed next to this blaze.
pub fn path() -> std::io::Result<PathBuf> {
    let this = std::env::current_exe()?;
    Ok(this.with_file_name(format!("blaze-rustc{}", std::env::consts::EXE_SUFFIX)))
}

/// What the variables that name the programs of a build start with.
const PROGRAM_OF: &str = "CARGO_BIN_EXE_";
/// The variable that names the directory cargo gives the integration tests of a build.
const SCRATCH: &str = "CARGO_TARGET_TMPDIR";

/// Whether the variable is one cargo sets to say where a program of the build is, or its
/// tests' directory.
pub fn names_a_place(name: &str) -> bool {
    name.starts_with(PROGRAM_OF) || name == SCRATCH
}

/// Builds what cargo asked rustc for and ends as rustc would have.
pub fn command(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let rustc = args.next().context("cargo names rustc first")?;
    let args: Vec<OsString> = args.collect();
    if let Some(replied) = crate::answers::reply(&rustc, &args) {
        std::process::exit(replied?);
    }
    anyhow::ensure!(
        std::env::var_os(crate::answers::ONLY_ASKS).is_some(),
        "blaze-rustc builds only for blaze's loads, and plain cargo no longer builds through \
         blaze: `blaze build` does"
    );
    let mut compiler = Command::new(&rustc);
    compiler.args(&args).env_remove(crate::answers::ONLY_ASKS);
    std::process::exit(for_load(compiler, &args)?);
}

/// Builds a crate for rust-analyzer's `cargo check --compile-time-deps`. Into blaze's own
/// target directory, which cargo's builds never read ([`own_macro::OWN_TARGET`]), nothing is
/// rustc's: a build script is compiled and linked by blaze, a proc macro is linked by blaze
/// for its server ([`own_macro`]), and a library is only described for those that link it
/// ([`Unit::described_only`]), since every crate that links one there is blaze's and is
/// compiled from the descriptions. One blaze cannot build fails the load with why. Into
/// cargo's target directory, or with blaze's proc macros turned off for the harness's
/// reference builds ([`own_macro::turned_off`]), `compiler`, rustc, builds it as without
/// blaze. The code to end with.
fn for_load(compiler: Command, args: &[OsString]) -> anyhow::Result<i32> {
    let own = std::env::var_os(own_macro::OWN_TARGET).is_some() && !own_macro::turned_off();
    let unit = match Unit::of(args) {
        Some(unit) if own => unit,
        unit => {
            if let Some(unit) = unit.as_ref().filter(|it| it.script) {
                unit.for_wasi(&unit.records.join(&unit.id))?;
            }
            return by_rustc(compiler, unit.as_ref());
        }
    };
    let record = unit.as_record(stamp_now());
    let package = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let failed = |what: String, why: String| {
        anyhow::anyhow!("blaze did not build {what} of `{package}` for the load: {why}")
    };
    if unit.script {
        let dir = unit.records.join(&unit.id);
        unit.by_blaze(&dir).map_err(|why| failed("the build script".to_owned(), why))?;
        return Ok(0);
    }
    let named = || format!("`{}`", unit.crate_name);
    if record.is_proc_macro() {
        let wasm = own_macro::wasm() && own_macro::runs_as_wasm(&unit.crate_name)?;
        if !wasm && std::env::var_os(own_macro::SERVER).is_none() {
            let why = own_macro::missing_server();
            return Err(failed(format!("the proc macro {}", named()), why));
        }
        let built = own_macro::made(&record, wasm).ok_or("cargo names no place for it".to_owned());
        let dir = unit.records.join(&unit.id).join(record.stamp.to_string());
        built
            .and_then(|made| {
                own_script::compile_as(&unit.args, &unit.cwd, &unit.source, &made, &dir)
            })
            .map_err(|why| failed(format!("the proc macro {}", named()), why))?;
        record.write_described(&unit.records)?;
        return Ok(0);
    }
    if !record.is_rlib() {
        let why = "it is neither a library, a proc macro nor a build script".to_owned();
        return Err(failed(named(), why));
    }
    record.write_described(&unit.records)?;
    unit.described_only(&record).map_err(|why| failed(named(), format!("{why:#}")))?;
    Ok(0)
}

/// Has `compiler`, rustc, build `unit` as cargo asked. A build script it linked runs as one
/// of blaze's does, through `blaze-engine build-script`, in the sandbox. The code to end with.
fn by_rustc(mut compiler: Command, unit: Option<&Unit>) -> anyhow::Result<i32> {
    let status = compiler.status()?;
    if let Some(unit) = unit.filter(|it| it.script && status.success())
        && let Some(linked) = unit.as_record(0).outputs().first()
    {
        script::stand_in_front(linked)?;
    }
    Ok(status.code().unwrap_or(1))
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
        let record = unit.as_record(0);
        let emits_link = unit.args.iter().any(|it| {
            it.strip_prefix("--emit=").is_some_and(|kinds| kinds.split(',').any(|it| it == "link"))
        });
        // A unit that is only checked leaves nothing the load reads.
        if !emits_link {
            return None;
        }
        unit.script = record.is_build_script();
        let extra = record.codegen("extra-filename").unwrap_or_default();
        unit.id = Record::id_of(&unit.crate_name, extra, &unit.cwd.join(&out_dir));
        Some(unit)
    }

    /// Has blaze compile and link the build script where rustc would have, and records it;
    /// or, for one that only prints ([`inert_script`]), write what stands for it there.
    /// `dir` is where the unit is recorded. `Err` is why blaze did not.
    fn by_blaze(&self, dir: &Path) -> Result<(), String> {
        let stamp = stamp_now();
        let copies = dir.join(stamp.to_string());
        let record = self.as_record(0);
        let exe = record.outputs().into_iter().next().ok_or("rustc names no executable")?;
        if !inert_script::elide(&self.args, &self.source, &exe)? {
            self.for_wasi(dir).map_err(|it| format!("{it:#}"))?;
            own_script::compile(&self.args, &self.cwd, &self.source, &exe, &copies)?;
        }
        self.record(stamp, &copies).map_err(|it| format!("{it:#}"))
    }

    /// Builds the build script for wasm32-wasip1 beside where it is linked, when its package
    /// runs its script under WASI ([`wasi_script::mode`]), whoever builds the executable.
    fn for_wasi(&self, dir: &Path) -> anyhow::Result<()> {
        let package = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
        let record = self.as_record(0);
        let out_dir = record.out_dir();
        let Some(mode) = wasi_script::mode(&package, &self.cwd, out_dir.as_deref())? else {
            return Ok(());
        };
        let exe = record.outputs().into_iter().next().context("rustc names no executable")?;
        let dir = dir.join("wasi");
        wasi_script::compile(mode, &package, &self.args, &self.cwd, &self.source, &exe, &dir)
    }

    /// Gives cargo what rustc would have written of the library `record` is of, which the
    /// load reads nothing of but its record: an empty file for each it was to emit, and the
    /// dep-info, which names every Rust file of the crate's directory (no package nested in
    /// it, nothing hidden, no `target`), so that cargo builds the crate again, and so the
    /// scripts and proc macros that link it, when one of those changes. Each is written in
    /// place of what is there, which may be a link to cargo's own (`seed_own_target`).
    fn described_only(&self, record: &Record) -> anyhow::Result<()> {
        let out_dir = record.out_dir().context("cargo names no `--out-dir`")?;
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        let emits = self.args.iter().filter_map(|it| it.strip_prefix("--emit="));
        let mut outputs = Vec::new();
        for kind in emits.flat_map(|it| it.split(',')) {
            match kind {
                "link" => outputs.push(out_dir.join(format!("lib{stem}.rlib"))),
                "metadata" => outputs.push(out_dir.join(format!("lib{stem}.rmeta"))),
                "dep-info" => {}
                other => {
                    anyhow::bail!("cargo asks for `--emit={other}`, which blaze does not write")
                }
            }
        }
        let info = out_dir.join(format!("{stem}.d"));
        let root = self.source.parent().context("a crate root without a directory")?;
        let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
        let mut files = vec![self.source.clone()];
        rust_files(root, manifest.as_deref(), &mut files)
            .with_context(|| format!("reading {}", root.display()))?;
        files.sort();
        files.dedup();
        let files: Vec<String> =
            files.iter().map(|it| dep_info::escaped(&it.display().to_string())).collect();
        let mut text = String::new();
        for target in outputs.iter().chain([&info]) {
            let target = dep_info::escaped(&target.display().to_string());
            text.push_str(&format!("{target}: {}\n\n", files.join(" ")));
        }
        for file in &files {
            text.push_str(&format!("{file}:\n"));
        }
        let write = |path: &Path, bytes: &[u8]| {
            let new = path.with_extension(format!("{}.new", std::process::id()));
            std::fs::write(&new, bytes)
                .and_then(|()| std::fs::rename(&new, path))
                .with_context(|| format!("writing {}", path.display()))
        };
        for output in &outputs {
            write(output, b"")?;
        }
        write(&info, text.as_bytes())
    }

    fn as_record(&self, stamp: u128) -> Record {
        Record {
            id: self.id.clone(),
            crate_name: self.crate_name.clone(),
            cwd: self.cwd.clone(),
            args: self.args.clone(),
            source: self.source.clone(),
            stamp,
            copies: Vec::new(),
            link_line: None,
            env: std::env::vars().filter(|(name, _)| names_a_place(name)).collect(),
            crate_env: super::own_script::crate_env(),
        }
    }

    /// Writes the record of the build script blaze built into `to`, with the dep-info the
    /// daemon tells its crate by, and has cargo run it through blaze.
    fn record(&self, stamp: u128, to: &Path) -> anyhow::Result<()> {
        let record = self.as_record(stamp);
        std::fs::create_dir_all(to)?;
        // The daemon tells which crate a file is by the dep-info next to it, of which it
        // reads the first source: the root of the crate.
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        let root = dep_info::escaped(&self.source.display().to_string());
        std::fs::write(to.join(format!("{stem}.d")), format!("{stem}: {root}\n"))?;
        if let Some(linked) = record.outputs().first() {
            script::stand_in_front(linked)?;
        }
        record.write(&self.records)
    }
}

/// Adds to `files` every Rust file below `dir` but those of a package nested in it (a
/// directory with a manifest that is not `manifest`), of a hidden directory or of `target`.
pub fn rust_files(
    dir: &Path,
    manifest: Option<&Path>,
    files: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if entry.file_type()?.is_dir() {
            let nested = path.join("Cargo.toml").is_file() && Some(&*path) != manifest;
            if !(name.to_string_lossy().starts_with('.') || name == "target" || nested) {
                rust_files(&path, manifest, files)?;
            }
        } else if path.extension().is_some_and(|it| it == "rs") {
            files.push(path);
        }
    }
    Ok(())
}
