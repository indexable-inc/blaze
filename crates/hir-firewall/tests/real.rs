//! The harness over real crates: the harness projects (`harness/apps/*`, and the repositories
//! `harness/apps/apps.json` names, checked out), each lowered as a build lowers it, every
//! function of the named packages edited in turn.
//!
//! `BLAZE_FIREWALL` is one JSON value, inline or a path to a file holding it:
//!
//! ```json
//! {"projects": [{"dir": "/abs/harness/apps/ecs", "cargo_home": "/abs/cargo-home",
//!                "packages": ["ecs"], "stride": 1}]}
//! ```
//!
//! Each project is built once with `blaze build` first, so its build scripts have run, and its
//! registry packages are fetched into `cargo_home` (`cargo fetch --locked`). The test is
//! `#[ignore]`d, as it reads the machine's checkouts: the harness runs it with `--ignored`.

use std::{
    path::Path,
    sync::{Arc, OnceLock},
};

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_query::{Config, Database, MemTree};
use blaze_units::{
    query::{self, BuildConfig, Command, FetchKind, Invocation, Mount, Origin, Plan, Planned, Units},
    schema::Mode,
    stdlib,
};
use blaze_vfs::View;
use hir_firewall::{Codegen, Harness, Report};
use blaze_lower::{Lower, Lowered, Ran};
use serde::Deserialize;

const LIBRARY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../library");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    projects: Vec<Project>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    /// The workspace's directory, absolute.
    dir: String,
    /// Where its registry packages were fetched.
    cargo_home: String,
    /// The packages whose functions are edited.
    packages: Vec<String>,
    /// Every `stride`th function is edited.
    #[serde(default = "one")]
    stride: usize,
}

fn one() -> usize {
    1
}

#[test]
#[ignore = "reads the harness projects' checkouts: run with --ignored and BLAZE_FIREWALL set"]
fn an_edit_of_each_function_of_the_harness_projects_reaches_only_it() {
    let input = std::env::var("BLAZE_FIREWALL")
        .expect("BLAZE_FIREWALL names the projects: see the head of crates/hir-firewall/tests/real.rs");
    let text = if input.trim_start().starts_with('{') {
        input
    } else {
        std::fs::read_to_string(&input).unwrap_or_else(|error| panic!("BLAZE_FIREWALL: {input}: {error}"))
    };
    let input: Input = serde_json::from_str(&text).unwrap_or_else(|error| panic!("BLAZE_FIREWALL: {error}"));
    let mut dirty = Vec::new();
    for project in &input.projects {
        let mut lowered = lower(&project.dir, &project.cargo_home).unwrap_or_else(|error| panic!("{error}"));
        for package in &project.packages {
            let krate = krate(&lowered.1, &lowered.0, package).unwrap_or_else(|error| panic!("{error}"));
            let names = Arc::clone(lowered.0.paths());
            let codegen = Codegen::dev(package, names).unwrap_or_else(|error| panic!("{error}"));
            let report = harness(lowered.0.served_db(), krate, codegen, project.stride)
                .unwrap_or_else(|error| panic!("{}: `{package}`: {error}", project.dir));
            eprintln!("{}: `{package}`: {report}", project.dir);
            if !report.is_clean() {
                dirty.push(format!("{}: `{package}`", project.dir));
            }
        }
    }
    assert!(dirty.is_empty(), "edits reached past their function in {dirty:?}: the reports are above");
}

fn harness(db: &mut Db, krate: Crate, codegen: Codegen, stride: usize) -> Result<Report, String> {
    let mut harness = Harness::new(db, krate, Some(codegen))?;
    harness.run(db, stride)
}

/// The crate of package `package`'s library, or of its first other target that is no build
/// script.
fn krate(units: &Units, lowered: &Lowered, package: &str) -> Result<Crate, String> {
    let id = units
        .packages
        .iter()
        .find(|it| it.name == package)
        .map(|it| it.id.as_str())
        .ok_or_else(|| format!("no package `{package}`"))?;
    let of_package = |it: &&blaze_units::schema::Unit| -> bool {
        it.pkg_id == id && !it.is_std && it.mode == Mode::Build && !it.target.is_custom_build()
    };
    let units = &units.graph.units;
    let unit = (units.iter().position(|it| of_package(&it) && it.target.is_lib()))
        .or_else(|| units.iter().position(|it| of_package(&it)))
        .ok_or_else(|| format!("package `{package}` has no unit to edit"))?;
    lowered.krate(unit).ok_or_else(|| format!("unit {unit} of `{package}` has no crate"))
}

/// Every root the test reads, in one tree.
fn tree() -> &'static Arc<MemTree> {
    static TREE: OnceLock<Arc<MemTree>> = OnceLock::new();
    TREE.get_or_init(|| Arc::new(MemTree::default()))
}

struct Library {
    root: ContentId,
    commit: String,
}

/// This checkout's `library/` as the daemon mounts it.
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

/// Every file under `dir` that `keep` keeps, by its `/`-separated path, but under `.git` and
/// `skipped`.
fn read_tree(dir: &Path, keep: &dyn Fn(&str) -> bool, skipped: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut dirs = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, under)) = dirs.pop() {
        let entries = std::fs::read_dir(&at).map_err(|error| format!("{}: {error}", at.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?;
            let name = entry.file_name().into_string().map_err(|it| format!("{}: {it:?} is not UTF-8", at.display()))?;
            let path = if under.is_empty() { name.clone() } else { format!("{under}/{name}") };
            let metadata = std::fs::metadata(entry.path()).map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if metadata.is_dir() {
                if name != ".git" && !skipped.contains(&path.as_str()) {
                    dirs.push((entry.path(), path));
                }
            } else if keep(&path) {
                let bytes = std::fs::read(entry.path()).map_err(|error| format!("{}: {error}", entry.path().display()))?;
                files.push((path, bytes));
            }
        }
    }
    Ok(files)
}

fn root_of(files: &[(String, Vec<u8>)]) -> ContentId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
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
        (arch, os, _) => Err(format!("the firewall test knows no triple of {arch} {os}")),
    }
}

/// The units of the workspace at `dir` as `blaze build` plans them, each package it needs
/// mounted from where cargo fetched it.
fn plan(db: &Database, dir: &str, view: ContentId, cargo_home: &str) -> Result<(Units, BuildConfig), String> {
    let library = library()?;
    let home = std::env::var("HOME").map_err(|_| "HOME is not set, and cargo reads it".to_owned())?;
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
            std: "/s/std".to_owned(),
        };
        let opened = db.open_with(View { root: view }, &config.encode());
        let planned = db.pin().get::<Plan>(opened, &()).map_err(|error| format!("planning {dir}: {error:?}"))?.get();
        let needs = match planned {
            Planned::Ready(units) => return Ok((units, config)),
            Planned::Failed(error) => return Err(format!("planning {dir}: {}", error.what)),
            Planned::Needs(needs) => needs,
        };
        for fetch in needs {
            if fetched.contains(&fetch.at) {
                return Err(format!("planning {dir} needs {} again once it is mounted at {}", fetch.what, fetch.at));
            }
            if matches!(fetch.kind, FetchKind::IndexFile) {
                return Err(format!("planning {dir} reads the index file {}: its Cargo.lock does not pin {}", fetch.url, fetch.what));
            }
            let at = Path::new(&fetch.at);
            if !at.is_dir() {
                return Err(format!(
                    "{} is not at {}: `cargo fetch --locked --manifest-path {dir}/Cargo.toml` with CARGO_HOME={cargo_home} first",
                    fetch.what, fetch.at
                ));
            }
            let files = read_tree(at, &|_: &str| true, &[])?;
            mounts.push(Mount { at: fetch.at.clone(), origin: Origin::Root(root_of(&files)), under: String::new() });
            fetched.push(fetch.at);
        }
    }
}

/// The run of build-script unit `unit` as `blaze build` left it: `output` beside its
/// `OUT_DIR`, and the files in `OUT_DIR`.
fn ran(units: &Units, unit: usize) -> Result<Ran, String> {
    let out_dir = units.plans[unit].out_dir.clone().ok_or_else(|| format!("build script run {unit} has no OUT_DIR"))?;
    let output_at = Path::new(&out_dir).parent().map(|it| it.join("output")).ok_or_else(|| format!("{out_dir} has no parent"))?;
    let output = std::fs::read(&output_at).map_err(|error| {
        format!("no build script's run at {}: {error}; build the project with `blaze build` first", output_at.display())
    })?;
    let mut ran = Ran { out_dir: out_dir.clone(), ..Ran::default() };
    for line in String::from_utf8_lossy(&output).lines() {
        let Some(directive) = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:")) else { continue };
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
    ran.files = read_tree(Path::new(&out_dir), &|_: &str| true, &[])?;
    Ok(ran)
}

/// The workspace at `dir` lowered into a logged database ([`Db::logged`]) as a build lowers
/// it, each build script's run and each proc-macro crate's macros applied.
fn lower(dir: &str, cargo_home: &str) -> Result<(Lowered, Units), String> {
    let tree = tree();
    let files = read_tree(Path::new(dir), &|_: &str| true, &["target"])?;
    let view = root_of(&files);
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    query::register(&mut db);
    let (units, config) = plan(&db, dir, view, cargo_home)?;
    let lower = Lower { units: &units, config: &config, files: &**tree, root: view, host_cpu: None };
    let mut lowered = blaze_lower::lower_into(Db::logged(), lower).map_err(|error| format!("lowering {dir}: {error}"))?;
    for (unit, it) in units.graph.units.iter().enumerate() {
        if it.mode == Mode::RunCustomBuild {
            let run = ran(&units, unit)?;
            lowered.ran(unit, &run).map_err(|error| format!("applying build script run {unit} of {dir}: {error}"))?;
        }
    }
    let toolchain = ContentId::of("blaze v2 firewall toolchain 1", library()?.commit.as_bytes());
    for unit in 0..units.graph.units.len() {
        if !lowered.crates().is_proc_macro(unit) {
            continue;
        }
        let name = &units.graph.units[unit].target.name;
        let krate = lowered.krate(unit).ok_or_else(|| format!("proc macro `{name}` has no crate"))?;
        let macros = blaze_macro_eval::load(lowered.db(), krate, toolchain);
        lowered.macros(unit, macros).map_err(|error| format!("the macros of `{name}`: {error}"))?;
    }
    Ok((lowered, units))
}
