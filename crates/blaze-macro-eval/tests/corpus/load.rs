//! A fixture workspace in the front end as a build has it: planned by blaze-units' `Plan` over
//! its files on disk, the registry's sources cargo fetched and this checkout's `library/`;
//! lowered by `blaze_lower::lower`; each build script's run applied from what `blaze build`
//! of the workspace left beside its `OUT_DIR` (blaze's own runs, read as the daemon applies
//! them); and each proc-macro crate's expanders those `blaze_macro_eval::load` gives, as the
//! caller wraps them.
//!
//! The plan is every build's: its proc macros, and what they depend on, are built for
//! `wasm32-wasip1` to run as modules, so each macro crate here is the one its module is built
//! from, with wasm32's cfg, and the evaluator runs that crate. The tests compare engines on one
//! input (rule 10: the evaluator is the modules' reference), so what a host cfg would change in a
//! macro is not theirs to see. Its build scripts' runs are those that build left for the
//! macros' kind (`wasm32-wasip1+macros`).

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use base_db::Crate;
use blaze_ids::ContentId;
use blaze_lower::{Lower, Lowered, Ran};
use blaze_query::{Config, Database, MemTree};
use blaze_units::{
    query::{
        self, BuildConfig, Command, FetchKind, Invocation, Mount, Origin, Plan, Planned, Units,
    },
    schema::Mode,
    stdlib,
};
use blaze_vfs::View;
use hir_expand::proc_macro::ProcMacro;

use crate::config::Homes;

const LIBRARY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../library");

/// A workspace lowered with its runs and macros applied.
pub(crate) struct Workspace {
    pub(crate) dir: String,
    pub(crate) units: Units,
    pub(crate) lowered: Lowered,
}

impl Workspace {
    /// The crate of the library of package `package`.
    pub(crate) fn krate(&self, package: &str) -> Result<Crate, String> {
        let id = self
            .units
            .packages
            .iter()
            .find(|it| it.name == package)
            .map(|it| it.id.as_str())
            .ok_or_else(|| format!("{} has no package `{package}`", self.dir))?;
        let unit = self
            .units
            .graph
            .units
            .iter()
            .position(|it| {
                it.pkg_id == id && !it.is_std && it.mode == Mode::Build && it.target.is_lib()
            })
            .ok_or_else(|| format!("package `{package}` of {} has no library unit", self.dir))?;
        self.lowered.krate(unit).ok_or_else(|| format!("the library of `{package}` has no crate"))
    }
}

/// The ID of the toolchain the macros' expansions are keyed under: the library's commit, as
/// the daemon's toolchain names the release.
pub(crate) fn toolchain() -> Result<ContentId, String> {
    Ok(ContentId::of("blaze v2 macro-eval corpus toolchain 1", library()?.commit.as_bytes()))
}

/// The commit of the library the macros' crates are built against.
pub(crate) fn commit() -> Result<&'static str, String> {
    Ok(&library()?.commit)
}

/// Every root the tests read, in one tree: each database of a test is opened over it.
fn tree() -> &'static Arc<MemTree> {
    static TREE: OnceLock<Arc<MemTree>> = OnceLock::new();
    TREE.get_or_init(|| Arc::new(MemTree::default()))
}

struct Library {
    root: ContentId,
    commit: String,
}

/// This checkout's `library/` as the daemon mounts it: every file but blaze's own.
fn library() -> Result<&'static Library, String> {
    static READ: OnceLock<Result<Library, String>> = OnceLock::new();
    let read = READ.get_or_init(|| {
        let dir = Path::new(LIBRARY);
        let upstream = std::fs::read_to_string(dir.join("UPSTREAM"))
            .map_err(|error| format!("{}/UPSTREAM: {error}", dir.display()))?;
        let commit = stdlib::library_commit(&upstream)?;
        let files = read_tree(dir, &stdlib::in_library, &[])?;
        Ok(Library { root: root_of(&files), commit })
    });
    read.as_ref().map_err(Clone::clone)
}

/// Every file under `dir` by its `/`-separated path under it that `keep` keeps, but those under
/// a directory named `.git` or one of `skipped` (paths under `dir`).
fn read_tree(
    dir: &Path,
    keep: &dyn Fn(&str) -> bool,
    skipped: &[&str],
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut dirs = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, under)) = dirs.pop() {
        let entries =
            std::fs::read_dir(&at).map_err(|error| format!("{}: {error}", at.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|it| format!("{}: {it:?} is not UTF-8", at.display()))?;
            let path = if under.is_empty() { name.clone() } else { format!("{under}/{name}") };
            let metadata = std::fs::metadata(entry.path())
                .map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if metadata.is_dir() {
                if name != ".git" && !skipped.contains(&path.as_str()) {
                    dirs.push((entry.path(), path));
                }
            } else if keep(&path) {
                let bytes = std::fs::read(entry.path())
                    .map_err(|error| format!("{}: {error}", entry.path().display()))?;
                files.push((path, bytes));
            }
        }
    }
    Ok(files)
}

fn root_of(files: &[(String, Vec<u8>)]) -> ContentId {
    let files: Vec<(&str, &[u8])> =
        files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
    tree().root(&files)
}

fn host() -> Result<&'static str, String> {
    let musl = cfg!(target_env = "musl");
    match (std::env::consts::ARCH, std::env::consts::OS, musl) {
        ("x86_64", "linux", false) => Ok("x86_64-unknown-linux-gnu"),
        ("x86_64", "linux", true) => Ok("x86_64-unknown-linux-musl"),
        ("aarch64", "linux", false) => Ok("aarch64-unknown-linux-gnu"),
        ("aarch64", "linux", true) => Ok("aarch64-unknown-linux-musl"),
        ("aarch64", "macos", _) => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos", _) => Ok("x86_64-apple-darwin"),
        (arch, os, _) => Err(format!("the corpus knows no triple of {arch} {os}")),
    }
}

/// The units of the workspace at `dir` (whose files are the view `view`), as `blaze build`
/// plans them there, each package it needs mounted from where cargo fetched it.
/// `homes` is where the daemon that built it placed build directories and the standard
/// library's, so that each build script's `OUT_DIR` in the plan is where that build left its run.
fn plan(
    db: &Database,
    dir: &str,
    view: ContentId,
    cargo_home: &str,
    homes: Homes<'_>,
) -> Result<(Units, BuildConfig), String> {
    let library = library()?;
    let home =
        std::env::var("HOME").map_err(|_| "HOME is not set, and cargo reads it".to_owned())?;
    let mut env = vec![("CARGO_HOME".to_owned(), cargo_home.to_owned()), ("HOME".to_owned(), home)];
    env.sort();
    let config_toml = format!("{cargo_home}/config.toml");
    let files = match std::fs::read(&config_toml) {
        Ok(bytes) => vec![(config_toml, bytes)],
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("{config_toml}: {error}")),
    };
    let mut mounts = vec![
        Mount { at: dir.to_owned(), origin: Origin::View, under: String::new() },
        stdlib::library_mount(library.root, &library.commit),
    ];
    let mut fetched: Vec<String> = Vec::new();
    loop {
        mounts.sort();
        let config = BuildConfig {
            invocation: Invocation {
                command: Command::Build,
                args: Vec::new(),
                root: dir.to_owned(),
                cwd: dir.to_owned(),
                env: env.clone(),
                files: files.clone(),
            },
            mounts: mounts.clone(),
            host: host()?.to_owned(),
            library: stdlib::library_at(&library.commit),
            std: homes.std.to_owned(),
            builds: homes.builds.to_owned(),
            // The corpus is fetched by `cargo fetch` into `cargo_home`, laid out as blaze keeps its sources.
            sources: cargo_home.to_owned(),
            blaze: String::new(),
        };
        let opened = db.open_with(View { root: view }, &config.encode());
        let planned = db
            .pin()
            .get::<Plan>(opened, &())
            .map_err(|error| format!("planning {dir}: {error:?}"))?
            .get();
        let needs = match planned {
            Planned::Ready(units) => return Ok((units, config)),
            Planned::Failed(error) => return Err(format!("planning {dir}: {}", error.what)),
            Planned::Needs(needs) => needs,
        };
        for fetch in needs {
            if fetched.contains(&fetch.at) {
                return Err(format!(
                    "planning {dir} needs {} again once it is mounted at {}",
                    fetch.what, fetch.at
                ));
            }
            if matches!(fetch.kind, FetchKind::IndexFile) {
                return Err(format!(
                    "planning {dir} reads the index file {}: its Cargo.lock does not pin {}",
                    fetch.url, fetch.what
                ));
            }
            let at = Path::new(&fetch.at);
            if !at.is_dir() {
                return Err(format!(
                    "{} is not at {}: `cargo fetch --locked --manifest-path {dir}/Cargo.toml` with CARGO_HOME={cargo_home} first",
                    fetch.what, fetch.at
                ));
            }
            let files = read_tree(at, &|_: &str| true, &[])?;
            mounts.push(Mount {
                at: fetch.at.clone(),
                origin: Origin::Root(root_of(&files)),
                under: String::new(),
            });
            fetched.push(fetch.at);
        }
    }
}

/// The run of build-script unit `unit` as `blaze build` left it: its stdout where the plan
/// names it (the first of the run's outputs: `run/stdout` beside `out/` in the new layout,
/// `output` beside `OUT_DIR` in cargo's first), and the files in `OUT_DIR`. A run of the
/// standard library's is read where the daemon placed it under `std` ([`std_placed`]), its
/// `OUT_DIR` still the plan's.
fn ran(units: &Units, unit: usize, std: &str) -> Result<Ran, String> {
    let plan = &units.plans[unit];
    let out_dir =
        plan.out_dir.as_deref().ok_or_else(|| format!("build script run {unit} has no OUT_DIR"))?;
    let output_at =
        plan.outputs.first().ok_or_else(|| format!("build script run {unit} names no stdout"))?;
    let (read_from, output_at) = match std_placed(output_at, std)? {
        Some((planned, placed)) => {
            let under = |path: &str| {
                path.strip_prefix(planned.as_str())
                    .map(|rest| format!("{}{rest}", placed.display()))
                    .ok_or_else(|| format!("build script run {unit}: {path} is not under {planned}, where its stdout is"))
            };
            (under(out_dir)?, PathBuf::from(under(output_at)?))
        }
        None => (out_dir.to_owned(), PathBuf::from(output_at)),
    };
    let output = std::fs::read(&output_at).map_err(|error| {
        format!(
            "no build script's run at {}: {error}; the harness builds the workspace with `blaze build -p corpus-calls` first, which plans it as this does",
            output_at.display()
        )
    })?;
    let mut ran = Ran { out_dir: out_dir.to_owned(), ..Ran::default() };
    for line in String::from_utf8_lossy(&output).lines() {
        let Some(directive) = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"))
        else {
            continue;
        };
        let Some((key, value)) = directive.split_once('=') else { continue };
        match key {
            "rustc-cfg" => ran.cfgs.push(value.to_owned()),
            "rustc-env" => {
                let (name, value) = value
                    .split_once('=')
                    .ok_or_else(|| format!("{}: `{line}` sets no variable", output_at.display()))?;
                ran.env.push((name.to_owned(), value.to_owned()));
            }
            _ => {}
        }
    }
    ran.files = read_tree(Path::new(&read_from), &|_: &str| true, &[])?;
    Ok(ran)
}

/// For `path` of the plan under `std/<key>` (the standard library's directory, its key the
/// daemon's `std_key`), that directory as planned and the one of `std` the daemon left the
/// file in. This plan's key covers the library as the test mounts it, which is not the
/// daemon's mount byte for byte, so the file is looked for under every key and found in
/// exactly one: none, or two, is an error naming them. None when `path` is not under `std`.
fn std_placed(path: &str, std: &str) -> Result<Option<(String, PathBuf)>, String> {
    let Some(rest) = path.strip_prefix(std).and_then(|it| it.strip_prefix('/')) else {
        return Ok(None);
    };
    let (key, below) =
        rest.split_once('/').ok_or_else(|| format!("{path} names no file under a key of {std}"))?;
    let entries = std::fs::read_dir(std)
        .map_err(|error| format!("the standard library's directory {std}: {error}"))?;
    let mut found = Vec::new();
    for entry in entries {
        let dir = entry.map_err(|error| format!("{std}: {error}"))?.path();
        if dir.join(below).is_file() {
            found.push(dir);
        }
    }
    found.sort();
    match found.as_slice() {
        [placed] => Ok(Some((format!("{std}/{key}"), placed.clone()))),
        [] => Err(format!(
            "no build script's run of the standard library at {below} under any key of {std} (planned under {key}): \
             the corpus reads the std the daemon that built the workspace shared"
        )),
        more => Err(format!(
            "the standard library's run at {below} is under {} keys of {std}, {}: the corpus can't tell which the \
             daemon's build of the workspace used; a fresh `std` names one",
            more.len(),
            more.iter().map(|it| it.display().to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The workspace at `dir` (absolute) in the front end, its packages fetched into `cargo_home`
/// and built by a daemon that placed its outputs in `homes`: each proc-macro crate's
/// macros are what `wrap` makes of those `blaze_macro_eval::load` gives with `macros`, given
/// the crate's name.
pub(crate) fn lower(
    dir: &str,
    cargo_home: &str,
    homes: Homes<'_>,
    macros: &std::sync::Arc<blaze_macro_eval::Macros>,
    mut wrap: impl FnMut(&str, Vec<ProcMacro>) -> Result<Vec<ProcMacro>, String>,
) -> Result<Workspace, String> {
    lower_with(dir, cargo_home, homes, |db, krate, toolchain, name| {
        let macros = blaze_macro_eval::load(db, krate, toolchain, macros)
            .map_err(|error| format!("blaze_macro_eval::load of `{name}`: {error}"))?;
        wrap(name, macros)
    })
}

/// [`lower`], each proc-macro crate's macros what `load` gives of the database, the crate, the
/// toolchain and the crate's name.
pub(crate) fn lower_with(
    dir: &str,
    cargo_home: &str,
    homes: Homes<'_>,
    mut load: impl FnMut(&base_db::Db, Crate, ContentId, &str) -> Result<Vec<ProcMacro>, String>,
) -> Result<Workspace, String> {
    let tree = tree();
    let files = read_tree(Path::new(dir), &|_: &str| true, &["target"])?;
    let view = root_of(&files);
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    query::register(&mut db);
    let (units, config) = plan(&db, dir, view, cargo_home, homes)?;
    let mut lowered = blaze_lower::lower(Lower {
        units: &units,
        config: &config,
        files: &**tree,
        root: view,
        host_cpu: None,
    })
    .map_err(|error| format!("lowering {dir}: {error}"))?;
    for (unit, it) in units.graph.units.iter().enumerate() {
        if it.mode == Mode::RunCustomBuild {
            let run = ran(&units, unit, homes.std)?;
            lowered
                .ran(unit, &run)
                .map_err(|error| format!("applying build script run {unit} of {dir}: {error}"))?;
        }
    }
    let toolchain = toolchain()?;
    for unit in 0..units.graph.units.len() {
        if !lowered.crates().is_proc_macro(unit) {
            continue;
        }
        let name = units.graph.units[unit].target.name.clone();
        let krate =
            lowered.krate(unit).ok_or_else(|| format!("proc macro `{name}` has no crate"))?;
        let macros = load(lowered.db(), krate, toolchain, &name)?;
        lowered
            .macros(unit, Ok(macros))
            .map_err(|error| format!("the macros of `{name}`: {error}"))?;
    }
    Ok(Workspace { dir: dir.to_owned(), units, lowered })
}
