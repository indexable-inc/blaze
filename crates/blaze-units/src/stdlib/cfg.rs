//! A unit's cfg, the one source: what blaze-command's graph gives every unit and what the
//! tests that key std's real crates (blaze-unitkey's std test, hir-def's std census) give std's.
//!
//! - [`unit_cfg`]: the profile's codegen flags, the plan's `--cfg`s (its features, as the load
//!   resolved them) and its rustflags, through the target's own cfg (`target::target_cfg`), and
//!   `test` for a test: the lines rustc would be given.
//! - [`std_cfg`]: a std unit's [`unit_cfg`] with its ported build script's `rustc-cfg`s
//!   ([`super::scripts`], never the script), and the script's `rustc-env`s.
//! - [`std_graph`]: std's graph as the load resolves it (the [`StdUnits`] query, `-Zbuild-std`'s
//!   resolution), read from the library's files: for a caller with no load of its own.
//!
//! blaze's own contract atom is not here: it is no line rustc is given, and its home is the
//! command's graph, which adds it when the profile checks contracts.
//!
//! Floor: one target cfg per unit (a table lookup and the flags' parse) and one script port's
//! run per std crate with a script; here the same. [`std_graph`] is the load's own resolution,
//! at its floor once the library's manifests are read.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blaze_query::{Database, MemTree};
use blaze_vfs::View;

use crate::query::{Mount, PackagePlan, StdGraph, StdKey, StdPlanned, StdUnits, UnitPlan};
use crate::schema::{self, DebugInfo, Mode};
use crate::stdlib::{scripts, target};

/// Unit `unit`'s cfg lines (`flag`, `key="value"`) as rustc is given them for `triple`, its plan
/// `plan`: the profile's opt-level, debug assertions, overflow checks and panic strategy, a
/// proc macro's crate type, the plan's `--cfg`s (`feature="x"`) and rustflags, through the
/// target's own cfg; `test` for a test.
pub fn unit_cfg(unit: &schema::Unit, plan: &UnitPlan, triple: &str) -> Result<Vec<String>, String> {
    let test = unit.mode == Mode::Test || plan.checked_as_test;
    let profile = &unit.profile;
    let on = |yes: bool| if yes { "on" } else { "off" };
    let mut flags: Vec<String> = Vec::with_capacity(10 + 2 * plan.cfgs.len() + plan.rustflags.len());
    flags.extend(["-C".to_owned(), format!("opt-level={}", profile.opt_level)]);
    flags.extend(["-C".to_owned(), format!("debug-assertions={}", on(profile.debug_assertions))]);
    flags.extend(["-C".to_owned(), format!("overflow-checks={}", on(profile.overflow_checks))]);
    if profile.panic != "unwind" {
        flags.extend(["-C".to_owned(), format!("panic={}", profile.panic)]);
    }
    if unit.target.is_proc_macro() && !test {
        flags.extend(["--crate-type".to_owned(), "proc-macro".to_owned()]);
    }
    for cfg in &plan.cfgs {
        flags.extend(["--cfg".to_owned(), cfg.clone()]);
    }
    flags.extend(plan.rustflags.iter().cloned());
    let args = target::CfgArgs::from_flags(&flags)?;
    let mut lines = target::target_cfg(triple, &args)?;
    if test {
        lines.push("test".to_owned());
    }
    Ok(lines)
}

/// What a std script's run reads beside its plan: where the library is named
/// ([`super::library_at`]) and is on disk, the machine scripts run on, blaze's stand-ins for
/// `rustc` and `rustdoc` (named in the script's environment, never run), the command's own
/// environment (after cargo's variables, the first of each name kept), and the jobs it names.
pub struct ScriptRun<'a> {
    pub library: &'a str,
    pub dir: &'a Path,
    pub host: &'a str,
    pub tools: &'a Path,
    pub inherited: &'a [(String, String)],
    pub jobs: u32,
}

/// A std unit's cfg lines and its script's variables.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StdCfg {
    /// Sorted, each once.
    pub lines: Vec<String>,
    /// The script's `rustc-env`s, sorted by name.
    pub env: Vec<(String, String)>,
}

/// Why [`on_disk`], [`std_cfg`] or [`std_graph`] refused: each names the unit, path or step it
/// is about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CfgError {
    /// The std path `named` is not under the library's name `library`.
    NotUnder { named: String, library: String },
    /// Unit `at` is past the plan's `units` units.
    PastUnits { at: usize, units: usize },
    /// Unit or script run `at` has no plan.
    NoPlan { at: usize },
    /// Unit `name` is not std's: its build script runs, it is not ported.
    NotStd { name: String },
    /// The plan names package `package` and has no plan of it.
    NoPackage { package: String },
    /// The script sets variable `name` to `first` and to `second`.
    SetTwice { name: String, first: String, second: String },
    /// The script run lacks what its port needs (`what`: its compiled script, its OUT_DIR, ...).
    Run { what: &'static str },
    /// A step of the port or of std's graph refused (`step` names it): its own message.
    Step { step: &'static str, why: String },
}

impl std::fmt::Display for CfgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CfgError::NotUnder { named, library } => write!(f, "{named} is not under {library}"),
            CfgError::PastUnits { at, units } => write!(f, "unit {at} is past the plan's {units} units"),
            CfgError::NoPlan { at } => write!(f, "unit {at} has no plan"),
            CfgError::NotStd { name } => {
                write!(f, "`{name}` is no unit of the standard library: its build script runs, it is not ported")
            }
            CfgError::NoPackage { package } => write!(f, "the plan names package {package} and has no plan of it"),
            CfgError::SetTwice { name, first, second } => {
                write!(f, "its script sets `{name}` to `{first}` and to `{second}`")
            }
            CfgError::Run { what } => write!(f, "the script run {what}"),
            CfgError::Step { step, why } => write!(f, "{step}: {why}"),
        }
    }
}

/// The std path `named` (`<library>/<rest>`) where the library is on disk.
pub fn on_disk(named: &str, run: &ScriptRun<'_>) -> Result<PathBuf, CfgError> {
    let rest = named
        .strip_prefix(run.library)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .ok_or_else(|| CfgError::NotUnder { named: named.to_owned(), library: run.library.to_owned() })?;
    let mut path = run.dir.as_os_str().to_owned();
    path.push(rest);
    Ok(PathBuf::from(path))
}

/// Std unit `at` of a plan's `units` (with their `plans` and `packages`, as [`StdGraph`] and
/// the load's `Units` both hold them) for `triple`: [`unit_cfg`] and, when it depends on a
/// script run, that run's ported output (blaze-cargo's `std_printed`: the run described as a
/// `blaze_build_script::Unit`, its environment by `blaze_build_script::env`, its output by
/// [`scripts::printed`], read by `blaze_build_script::parse`). A unit that is not std's is
/// refused: its script runs, it is not ported.
pub fn std_cfg(
    units: &[schema::Unit],
    plans: &[UnitPlan],
    packages: &[PackagePlan],
    at: usize,
    triple: &str,
    run: &ScriptRun<'_>,
) -> Result<StdCfg, CfgError> {
    let unit = units.get(at).ok_or(CfgError::PastUnits { at, units: units.len() })?;
    let plan = plans.get(at).ok_or(CfgError::NoPlan { at })?;
    if !unit.is_std {
        return Err(CfgError::NotStd { name: unit.target.name.clone() });
    }
    let lines = unit_cfg(unit, plan, triple).map_err(|why| CfgError::Step { step: "the unit's cfg", why })?;
    let mut cfg = StdCfg { lines, env: Vec::new() };
    let script = unit.dependencies.iter().map(|it| it.index).find(|&dep| units.get(dep).is_some_and(|it| it.mode == Mode::RunCustomBuild));
    if let Some(script) = script {
        let package = packages
            .iter()
            .find(|it| it.id == unit.pkg_id)
            .ok_or_else(|| CfgError::NoPackage { package: unit.pkg_id.to_string() })?;
        ported(units, plans, script, package, triple, run, &mut cfg)?;
    }
    cfg.lines.sort();
    cfg.lines.dedup();
    cfg.env.sort();
    if let Some(pair) = cfg.env.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(CfgError::SetTwice { name: pair[0].0.clone(), first: pair[0].1.clone(), second: pair[1].1.clone() });
    }
    Ok(cfg)
}

/// What std's script run `script` prints that its crate is given, added to `into`: its
/// `rustc-cfg`s to the lines, its `rustc-env`s as the variables.
fn ported(
    units: &[schema::Unit],
    plans: &[UnitPlan],
    script: usize,
    package: &PackagePlan,
    triple: &str,
    run: &ScriptRun<'_>,
    into: &mut StdCfg,
) -> Result<(), CfgError> {
    let run_unit = &units[script];
    let run_plan = plans.get(script).ok_or(CfgError::NoPlan { at: script })?;
    let compile = run_unit
        .dependencies
        .iter()
        .map(|it| it.index)
        .find(|at| units.get(*at).is_some_and(|it| it.target.is_custom_build() && it.mode != Mode::RunCustomBuild))
        .ok_or(CfgError::Run { what: "has no compiled script among its dependencies" })?;
    let script_root =
        units[compile].target.src_path.to_str().ok_or(CfgError::Run { what: "names its script's file not in UTF-8" })?;
    let read = |path: &str| {
        let disk = on_disk(path, run).map_err(|why| why.to_string())?;
        std::fs::read(&disk).map_err(|error| format!("{}: {error}", disk.display()))
    };
    let which = scripts::identify(&package.name, script_root, read)
        .map_err(|why| CfgError::Step { step: "identifying the script", why })?;
    let manifest_path = on_disk(&package.manifest_path, run)?;
    let manifest_dir =
        manifest_path.parent().map(Path::to_path_buf).ok_or(CfgError::Run { what: "has a manifest path with no directory" })?;
    let out_dir = run_plan.out_dir.clone().ok_or(CfgError::Run { what: "has no OUT_DIR" })?;
    let program = plans
        .get(compile)
        .and_then(|it| it.outputs.first())
        .ok_or(CfgError::Run { what: "has a compiled script that names no program" })?;
    let var = |name: &str| run_plan.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    let profile = &run_unit.profile;
    let debuginfo = !matches!(&profile.debuginfo, DebugInfo::Level(0))
        && !matches!(&profile.debuginfo, DebugInfo::Named(name) if name == "none" || name == "false");
    let cfg_args = target::CfgArgs::from_flags(&run_plan.rustflags)
        .map_err(|why| CfgError::Step { step: "the run's rustflags", why })?;
    let target_cfgs =
        target::target_cfg(triple, &cfg_args).map_err(|why| CfgError::Step { step: "the target's cfg", why })?;
    let tool = |name: &str| run.tools.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let described = blaze_build_script::Unit {
        package: blaze_build_script::unit::Package {
            name: package.name.clone(),
            version: package.version.clone(),
            descr: None,
            manifest_dir,
            manifest_path,
            links: run_plan.links.clone(),
            ..Default::default()
        },
        features: run_unit.features.clone(),
        profile: blaze_build_script::unit::Profile {
            name: profile.name.clone(),
            release: var("PROFILE") == Some("release"),
            opt_level: profile.opt_level.clone(),
            debuginfo,
            debug_assertions: profile.debug_assertions,
        },
        target: triple.to_owned(),
        host: run.host.to_owned(),
        target_cfgs,
        rustflags: run_plan.rustflags.clone(),
        jobs: run.jobs,
        script: PathBuf::from(program),
        out_dir: PathBuf::from(out_dir),
        tools: blaze_build_script::unit::Tools { cargo: tool("cargo"), rustc: tool("rustc"), rustdoc: tool("rustdoc"), ..Default::default() },
        workspace_member: false,
        ..Default::default()
    };
    // cargo's variables of the run first, then the caller's environment, the first of each name
    // kept: the daemon's `inherited`, without its sandbox's temporary directory (no script runs).
    let mut inherited: Vec<(OsString, OsString)> = Vec::with_capacity(run_plan.env.len() + run.inherited.len());
    inherited.extend(run_plan.env.iter().chain(run.inherited.iter()).map(|(name, value)| (OsString::from(name), OsString::from(value))));
    inherited.sort_by(|a, b| a.0.cmp(&b.0));
    inherited.dedup_by(|later, first| later.0 == first.0);
    let vars = blaze_build_script::env(&described, &inherited)
        .map_err(|error| CfgError::Step { step: "the script's environment", why: error.0 })?
        .vars;
    let printed = scripts::printed(which, &vars, &described.package.manifest_dir)
        .map_err(|why| CfgError::Step { step: "the ported script's output", why })?;
    let context = blaze_build_script::output::Context::of(&described, &inherited);
    let output = blaze_build_script::parse(&printed, &context)
        .map_err(|error| CfgError::Step { step: "reading the script's output", why: error.0 })?;
    into.lines.extend(output.cfgs);
    into.env = output.env;
    Ok(())
}

/// std's graph as the load resolves it ([`StdUnits`]), from the library's files `files` (every
/// file of `library/` a load reads, by its path under it), under the key `key` makes of the
/// library's mount ([`super::library_mount`] at [`super::library_at`] of the commit
/// `library/UPSTREAM` names, checked by [`super::library_commit`]).
pub fn std_graph(files: &[(&str, &[u8])], key: impl FnOnce(Mount) -> StdKey) -> Result<StdGraph, CfgError> {
    let upstream = files
        .iter()
        .find(|(path, _)| *path == "UPSTREAM")
        .ok_or_else(|| CfgError::Step { step: "the library", why: "it has no UPSTREAM".to_owned() })?;
    let commit = super::library_commit(&String::from_utf8_lossy(upstream.1))
        .map_err(|why| CfgError::Step { step: "the library's UPSTREAM", why })?;
    let tree = Arc::new(MemTree::default());
    let root = tree.root(files);
    let mut db = Database::new(blaze_query::Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    crate::query::register(&mut db);
    // The std graph reads nothing of a view: an empty one is opened for the query to run in.
    let view = db.open(View { root: tree.root(&[]) });
    let key = key(super::library_mount(root, &commit));
    let planned = db
        .pin()
        .get::<StdUnits>(view, &key)
        .map_err(|cancelled| CfgError::Step { step: "the std graph's query", why: format!("{cancelled:?}") })?
        .get();
    match planned {
        StdPlanned::Ready(graph) => Ok(graph),
        StdPlanned::Failed(error) => Err(CfgError::Step { step: "the standard library did not load", why: error.what }),
    }
}

/// The key of std for a dev-profile build on `triple` whose host is `triple` (`-Zbuild-std`'s
/// defaults: `std`, [`super::default_features`]): what a `blaze check` of a plain project asks.
pub fn dev_key(library: Mount, triple: &str) -> StdKey {
    StdKey {
        library,
        triple: triple.to_owned(),
        host: triple.to_owned(),
        profile: "dev".to_owned(),
        profile_root: "dev".to_owned(),
        macro_wasm: false,
        runs_here: true,
        panic: "unwind".to_owned(),
        opt_level: "0".to_owned(),
        debug_assertions: true,
        overflow_checks: true,
        debuginfo: "2".to_owned(),
        crates: vec!["std".to_owned()],
        features: super::default_features(),
        rustflags: Vec::new(),
        codegen_backend: None,
    }
}
