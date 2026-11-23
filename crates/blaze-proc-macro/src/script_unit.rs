//! A unit graph's build scripts, run for the units they serve: each script's module record
//! found by its package and exact feature set, its environment made from the unit, its outputs
//! from a run in the sandbox ([`ScriptRuntime::run`]; the command's `Script` task record is the
//! only cache of a run, D115).
//!
//! This is the step a driver's build-script task takes, and what feeds the outputs' readers:
//! the world's cfg of a crate (blaze-world's `ScriptOutputs`, by the served unit) and
//! `blaze_build_script::cx::ScriptCx` for `env!("OUT_DIR")` and `include!`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{
    Env, Outputs, OutputsRef,
    env::{DepMetadata, Given, PackageFacts},
    key::{CMode, RunLimits},
    native::Runner,
    probe::Row,
    toolchain::Mode,
};
use blaze_own::Alloc;
use blaze_unit_graph::{CfgAtom, Unit, UnitGraph, UnitId, UnitKind};

use crate::{
    native_unit::{NativeSetting, NativeUnitError, native_outputs, placed},
    script::{LoadedScript, ScriptError, ScriptRun, ScriptRuntime},
    script_sandbox::{
        Script, ScriptRoot, Tree,
        dep_mount::{self, DepProducer, DepViewError, DepViews},
        owned_env,
    },
};

/// What every script of one graph shares.
pub struct Setting<'s> {
    /// The store holding `build-script/modules` (the bootstrap's module records).
    pub store: &'s Path,
    pub target: &'s str,
    pub host: &'s str,
    /// The probe table's rows (`probe::pinned_rows`).
    pub table: Arc<Vec<Row>>,
    /// The probe checker and its records when the command's setting turns it on
    /// (script_probes.rs); `None`: the table answers the probes.
    pub probes: Option<crate::script_probes::ScriptProbes>,
    pub c_mode: CMode,
    /// What the command asked for: with each script's closure, where it runs (D69 (3),
    /// [`crate::native_unit::placed`]).
    pub mode: Mode,
    /// The pinned C toolchain and SDK native scripts run with; `None` where the command has
    /// none (a native script is then refused by name, never run as wasm).
    pub native: Option<NativeSetting<'s>>,
    /// What the command's setting says about spawns, chosen once per command
    /// (`crate::fact_pins::command_spawn_setting`): every script of the graph gets it.
    pub spawn_setting: Arc<crate::spawn_route::SpawnSetting>,
}

/// A script ready to run: its loaded module, its sandboxed inputs, its environment.
pub struct Prepared<'a> {
    pub loaded: LoadedScript,
    pub script: Arc<Script>,
    pub env: Env<'a>,
}

/// Why a unit's script gave no outputs.
#[derive(Debug)]
pub enum UnitScriptError {
    /// The unit is not a build script.
    NotAScript { unit: UnitId },
    /// No unit names it as its build script.
    ServesNothing { package: String },
    /// No module record of the package with exactly these features.
    NoModule { package: String, features: Vec<String>, store: PathBuf },
    /// The module store could not answer the lookup.
    Lookup(ModuleLookupError),
    /// The module record's bytes could not be read.
    ReadModule { path: PathBuf, error: std::io::Error },
    /// The caller has no files for the package.
    NoFiles { package: String },
    /// The script's own failure: a load, a limit, a refusal, an exit, the records.
    Script(ScriptError),
    /// A direct dependency with `links` gave no outputs, so its metadata (`DEP_<LINKS>_*`)
    /// cannot be given: the script is not run without it.
    Dependency { package: String, dependency: String, links: String },
    /// The scripts' `links` inputs form a cycle through this script.
    LinksCycle { package: String },
    /// A native script's run (D65): its setting, binary, key, record or run.
    Native(NativeUnitError),
    /// A `links` dependency's metadata names a path the script cannot be served (brief S1).
    DepView { package: String, why: DepViewError },
    /// A script placed native (D112) asked to run for a side's cfg (`target`): the native
    /// runner builds for the machine, never for that side (ruling D115 (2), rule 3).
    NativeOnSide { package: String, target: String },
}

impl std::fmt::Display for UnitScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnitScriptError::NotAScript { unit } => write!(f, "unit {} is not a build script", unit.index()),
            UnitScriptError::ServesNothing { package } => write!(f, "the build script of {package} is no unit's build script"),
            UnitScriptError::NoModule { package, features, store } => write!(
                f,
                "no build-script module of {package} with features [{}] in {}/build-script/modules: build it with proc_macro_modules' build-script mode",
                features.join(","),
                store.display()
            ),
            UnitScriptError::Lookup(error) => write!(f, "{error}"),
            UnitScriptError::ReadModule { path, error } => write!(f, "{}: {error}", path.display()),
            UnitScriptError::NoFiles { package } => write!(f, "no files of {package} to run its build script over"),
            UnitScriptError::Script(error) => write!(f, "{error}"),
            UnitScriptError::Dependency { package, dependency, links } => write!(
                f,
                "the build script of {package} needs the metadata of {dependency} (`links = \"{links}\"`, its DEP_* variables), whose build script gave no outputs"
            ),
            UnitScriptError::LinksCycle { package } => write!(f, "the build script of {package} is in a cycle of `links` metadata"),
            UnitScriptError::Native(error) => write!(f, "{error}"),
            UnitScriptError::DepView { package, why } => write!(f, "the build script of {package}: {why}"),
            UnitScriptError::NativeOnSide { package, target } => write!(
                f,
                "the build script of {package} runs native (D112), and a native run builds for the machine, not for `{target}`: it is not run for that side (D115 (2))"
            ),
        }
    }
}

impl std::error::Error for UnitScriptError {}

/// The module record (`<store>/build-script/modules/<key>`) of `name version` compiled with
/// exactly `features`: the one whose identity names all three. A native script's record (its
/// identity has a `target` line, D69) is never it: that one holds `script.bin`, not a module.
/// `Ok(None)`: the store holds no such record (no `modules` folder at all included).
pub fn module_of(store: &Path, name: &str, version: &str, features: &[&str]) -> Result<Option<PathBuf>, ModuleLookupError> {
    module_where(store, name, version, features, None)
}

/// The native binary record (`<store>/build-script/modules/<key>`, holding `script.bin`) of
/// `name version` built for `host` with exactly `features` (D69: the interim builder's
/// `native` mode, whose identity adds `target <host>`).
pub fn module_of_native(store: &Path, name: &str, version: &str, features: &[&str], host: &str) -> Result<Option<PathBuf>, ModuleLookupError> {
    module_where(store, name, version, features, Some(host))
}

/// Why the module store could not answer a lookup: each names the path (rule 19), and none is
/// taken as "no record" (rule 3).
#[derive(Debug)]
pub enum ModuleLookupError {
    /// The store's `build-script/modules` folder could not be listed (other than absent).
    List { folder: PathBuf, error: std::io::Error },
    /// A record's `identity` could not be read: a record without one is not a record.
    Identity { path: PathBuf, error: std::io::Error },
    /// Two records answer one ask (a toolchain or closure each, same name, version, features
    /// and runner): which is meant is not decided by the store's listing order (rules 21, 25).
    Ambiguous { first: PathBuf, second: PathBuf },
}

impl std::fmt::Display for ModuleLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleLookupError::List { folder, error } => write!(f, "{}: the module store could not be listed: {error}", folder.display()),
            ModuleLookupError::Identity { path, error } => write!(f, "{}: a module record's identity could not be read: {error}", path.display()),
            ModuleLookupError::Ambiguous { first, second } => write!(
                f,
                "{} and {}: two module records answer one ask (name, version, features, runner); remove the stale one",
                first.display(),
                second.display()
            ),
        }
    }
}

impl std::error::Error for ModuleLookupError {}

/// Each record of the store as (its folder, its identity's text), sorted by folder; none when
/// the store has no `modules` folder.
///
/// A listing and one read a record: the interim builder names a record by the SHA-256 of its
/// whole identity (scripts proc_macro_modules build_scripts.rs: origin, the lock's closure,
/// toolchain, shim), which an ask (name, version, features, runner) cannot compute, so the
/// folder cannot be joined from the ask until the builder also writes an ask-named pointer.
fn records(store: &Path) -> Result<Vec<(PathBuf, String)>, ModuleLookupError> {
    let folder = store.join("build-script/modules");
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ModuleLookupError::List { folder, error }),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ModuleLookupError::List { folder: folder.clone(), error })?;
        let path = entry.path().join("identity");
        let identity = std::fs::read_to_string(&path).map_err(|error| ModuleLookupError::Identity { path, error })?;
        found.push((entry.path(), identity));
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Every record of the store whose identity names `name` and `version`, whatever its features
/// and runner: (its folder, its identity's text), sorted by folder. What a refusal of a lookup
/// shows beside the asked identity (rule 19): which line of the nearest record differs.
pub fn records_named(store: &Path, name: &str, version: &str) -> Result<Vec<(PathBuf, String)>, ModuleLookupError> {
    let (name, version) = (format!("name {name}"), format!("version {version}"));
    let mut found = records(store)?;
    found.retain(|(_, identity)| identity.lines().any(|l| l == name) && identity.lines().any(|l| l == version));
    Ok(found)
}

/// The process shim this binary's build-script host answers (guest/script_process.rs, as built
/// into it): the bootstrap builder writes `shim <its SHA-256>` into a wasm module's identity
/// (scripts proc_macro_modules build_scripts.rs `library`), the same digest of the same file.
const SHIM: &[u8] = include_bytes!("../guest/script_process.rs");

/// The identity line a wasm module must carry to be run by this host: `shim <sha256 hex>`.
pub fn shim_line() -> String {
    format!("shim {}", crate::module::sha256_hex(SHIM))
}

/// The record whose identity names `name`, `version`, `features` and, for `host`, `target
/// <host>`; for `None`, one with no `target` line and this host's [`shim_line`] (a wasm module).
/// Two such records are a typed refusal, never the first in the listing's order.
fn module_where(store: &Path, name: &str, version: &str, features: &[&str], host: Option<&str>) -> Result<Option<PathBuf>, ModuleLookupError> {
    let mut sorted = features.to_vec();
    sorted.sort_unstable();
    let lines = [format!("name {name}"), format!("version {version}"), format!("features {}", sorted.join(","))];
    let target = host.map(|host| format!("target {host}"));
    let shim = shim_line();
    let mut fits = records(store)?.into_iter().filter(|(_, identity)| {
        // A wasm module answers only with the shim this host speaks to: one built with another
        // is not this ask's record, so a stale module is a named miss (`NoModule`, whose words
        // show the differing `shim` line), never a run that misreads a reply, and never a second
        // record beside its rebuild (`Ambiguous`).
        let runner_fits = match &target {
            Some(want) => identity.lines().any(|l| l == want),
            None => !identity.lines().any(|l| l.starts_with("target ")) && identity.lines().any(|l| l == shim),
        };
        runner_fits && lines.iter().all(|want| identity.lines().any(|l| l == want))
    });
    let Some((first, _)) = fits.next() else { return Ok(None) };
    match fits.next() {
        Some((second, _)) => Err(ModuleLookupError::Ambiguous { first, second }),
        None => Ok(Some(first)),
    }
}

/// The unit build-script unit `script` serves: the library (or proc macro) whose
/// `build_script` it is, else the first such unit.
pub fn served(graph: &UnitGraph<'_>, script: UnitId) -> Option<UnitId> {
    let mut first = None;
    for (at, unit) in graph.units.iter().enumerate() {
        if unit.build_script != Some(script) {
            continue;
        }
        let id = UnitId(at as u32);
        if matches!(unit.kind, UnitKind::Lib | UnitKind::ProcMacro) {
            return Some(id);
        }
        first.get_or_insert(id);
    }
    first
}

fn package(unit: &Unit<'_>) -> String {
    format!("{} {}", unit.name.as_str(), unit.version.as_str())
}

/// `script`'s module loaded and its inputs made: `tree` and `root` are its package's files
/// and what it sees (`script_sandbox::dir_tree` of the workspace with `ScriptRoot::member(dir)`
/// for a member, `mem_tree` with `ScriptRoot::package()` for a fetched package; D96). `deps` is
/// the `links` metadata of the served unit's
/// direct dependencies ([`dep_metadata`] of each of [`links_inputs`]); it is in the
/// environment, so in the run's key.
#[allow(clippy::too_many_arguments)]
pub fn prepare<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    alloc: Alloc<'a>,
) -> Result<Prepared<'a>, UnitScriptError> {
    prepare_viewing(runtime, setting, graph, script, tree, root, deps, DepViews::none(), alloc)
}

/// [`prepare`] with `views`, the read-only mounts of the script's `links` dependencies
/// ([`dep_mount::view`], brief S1); `deps` are then the values as the script sees them
/// ([`dep_mount::Viewed::values`]).
#[allow(clippy::too_many_arguments)]
pub fn prepare_viewing<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    views: DepViews,
    alloc: Alloc<'a>,
) -> Result<Prepared<'a>, UnitScriptError> {
    prepare_viewing_on(runtime, setting, graph, script, None, tree, root, deps, views, alloc)
}

/// [`prepare_viewing`] for the side whose cfg is `side_cfg` (sorted; `CARGO_CFG_*` and the
/// `OUT_DIR` configuration are made from it): the cfg of the crate of the list the run is for
/// (ruling D115 (2): a macro-side run sees wasm32-wasip1's atoms, with `setting.target` that
/// triple). `None`: the served unit's cfg as the graph holds it.
#[allow(clippy::too_many_arguments)]
pub fn prepare_viewing_on<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    side_cfg: Option<&[CfgAtom<'_>]>,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    views: DepViews,
    alloc: Alloc<'a>,
) -> Result<Prepared<'a>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    if unit.kind != UnitKind::BuildScript {
        return Err(UnitScriptError::NotAScript { unit: script });
    }
    let package = package(unit);
    let serves = served(graph, script).ok_or_else(|| UnitScriptError::ServesNothing { package: package.clone() })?;
    let serves = &graph.units[serves.index()];
    let features: Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    let module = module_of(setting.store, unit.name.as_str(), unit.version.as_str(), &features).map_err(UnitScriptError::Lookup)?.ok_or_else(|| UnitScriptError::NoModule {
        package: package.clone(),
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        store: setting.store.to_owned(),
    })?;
    let path = module.join("module.wasm");
    let wasm = std::fs::read(&path).map_err(|error| UnitScriptError::ReadModule { path, error })?;
    let loaded = runtime.load(&package, &wasm).map_err(UnitScriptError::Script)?;
    let facts = PackageFacts {
        name: unit.name.as_str(),
        version: unit.version.as_str(),
        meta: graph.meta_of(unit.package).map(|meta| meta.vars()),
        links: unit.links.as_ref().map(|l| l.as_str()),
    };
    let target_cfg: &[CfgAtom<'_>] = match side_cfg {
        Some(cfg) => cfg,
        None => &serves.cfg,
    };
    let given = Given { package: facts, seen: root.seen(), features: &unit.features, target_cfg, target: setting.target, host: setting.host, deps };
    let env = blaze_build_script::env(&given, alloc);
    let script = Arc::new(Script {
        package,
        tree,
        root,
        env: owned_env(&env, alloc),
        target: setting.target.to_owned(),
        host: setting.host.to_owned(),
        c_mode: setting.c_mode,
        limits: RunLimits::STANDARD,
        table: Arc::clone(&setting.table),
        probes: setting.probes.as_ref().map(blaze_own::Share::share),
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::clone(&setting.spawn_setting),
        deps: Arc::new(views),
    });
    Ok(Prepared { loaded, script, env })
}

/// `script`'s outputs, run as wasm, or native when [`placed`] says so for the setting's mode
/// (D65, D69). The choice is made here, before
/// anything runs, and is in the run's key; a failure either way is returned, never tried the
/// other way (rule 3).
#[allow(clippy::too_many_arguments)]
pub fn outputs<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    if unit.kind != UnitKind::BuildScript {
        return Err(UnitScriptError::NotAScript { unit: script });
    }
    match crate::native_unit::placed_in_store(setting.store, graph, script, setting.mode, setting.host, alloc) {
        Runner::Wasm => {
            let prepared = prepare(runtime, setting, graph, script, tree, root, deps, alloc)?;
            runtime.run(&prepared.loaded, prepared.script, &prepared.env, alloc).map_err(UnitScriptError::Script)
        }
        Runner::Native { because } => {
            let serves = served(graph, script).ok_or_else(|| UnitScriptError::ServesNothing { package: package(unit) })?;
            // No producers given: a `links` value is refused by name (brief N8), never passed as written.
            native_outputs(setting, graph, script, serves, because, tree, root, deps, &[], alloc).map_err(UnitScriptError::Native)
        }
    }
}

/// [`outputs`] for a script whose `links` dependencies' runs are `producers` (each with its
/// `links`), so the paths in `deps` are served (brief S1): a wasm script is given the values
/// as it sees them and the producers' files read-only ([`dep_mount::view`]); a native script
/// (D112) the same values as IT sees them, the producers' trees laid read-only under its own
/// folder ([`dep_mount::native::lay`], brief N8): one resolver, two sights.
#[allow(clippy::too_many_arguments)]
pub fn outputs_viewing<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    producers: &[(&str, &DepProducer)],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    outputs_viewing_on(runtime, setting, graph, script, None, tree, root, deps, producers, alloc)
}

/// [`outputs_viewing`] for the side whose cfg is `side_cfg` ([`prepare_viewing_on`]). A script
/// placed native is refused when given a side's cfg ([`UnitScriptError::NativeOnSide`]): the
/// native runner builds for the machine. `None`: as [`outputs_viewing`].
#[allow(clippy::too_many_arguments)]
pub fn outputs_viewing_on<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    side_cfg: Option<&[CfgAtom<'_>]>,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    producers: &[(&str, &DepProducer)],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    if unit.kind != UnitKind::BuildScript {
        return Err(UnitScriptError::NotAScript { unit: script });
    }
    match crate::native_unit::placed_in_store(setting.store, graph, script, setting.mode, setting.host, alloc) {
        Runner::Wasm => {
            let viewed = dep_mount::view(&tree, producers, deps, alloc).map_err(|why| UnitScriptError::DepView { package: package(unit), why })?;
            let seen: Vec<DepMetadata<'_>> =
                deps.iter().zip(viewed.values.iter()).map(|(dep, value)| DepMetadata { links: dep.links, key: dep.key, value: value.as_str() }).collect();
            let prepared = prepare_viewing_on(runtime, setting, graph, script, side_cfg, tree, root, &seen, viewed.views, alloc)?;
            runtime.run(&prepared.loaded, prepared.script, &prepared.env, alloc).map_err(UnitScriptError::Script)
        }
        Runner::Native { .. } if side_cfg.is_some() && setting.target != setting.host => {
            Err(UnitScriptError::NativeOnSide { package: package(unit), target: setting.target.to_owned() })
        }
        Runner::Native { because } => {
            let serves = served(graph, script).ok_or_else(|| UnitScriptError::ServesNothing { package: package(unit) })?;
            native_outputs(setting, graph, script, serves, because, tree, root, deps, producers, alloc).map_err(UnitScriptError::Native)
        }
    }
}

/// One run a reading script is served of: a script of its `links` closure ([`links_closure`]),
/// handed as its outputs RECORD (read in place), with the tree its package was served and the
/// root it saw it at. The command hands the `Script` records the drive read for it; an example
/// hands the records of the runs it made; both through [`outputs_given`], the one function that
/// turns them into the reader's `DEP_<LINKS>_*` and what its run is served (a wasm run's mounts,
/// a native run's laid folders), so the two never drift (rule 30).
pub struct GivenRun<'r> {
    pub script: UnitId,
    /// The `links` of the unit it serves.
    pub links: &'r str,
    pub outputs: OutputsRef<'r>,
    pub tree: Tree,
    pub root: ScriptRoot,
    /// A `links` input of the reader itself (its metadata is the reader's `DEP_<LINKS>_*`), not
    /// only upstream of one (whose files serve a forwarded path, brief S1b).
    pub direct: bool,
}

/// What a reading script is given of its closure's runs ([`served_links`]): its `DEP_<LINKS>_*`
/// pairs as the producers' records hold them, and each direct producer, with its own closure
/// upstream, by its `links`.
pub struct ServedLinks<'g> {
    pub deps: Vec<DepMetadata<'g>>,
    pub producers: Vec<(&'g str, DepProducer)>,
}

/// The scripts whose runs `script` is served of: each of its [`links_inputs`] and, walked from
/// those, each of theirs (the transitive closure brief S1b serves a forwarded path from), with
/// the `links` each serves and whether it is a direct input. A work list, no recursion; sorted
/// by unit, each once (a script reached directly and upstream is direct).
pub fn links_closure<'g>(graph: &'g UnitGraph<'_>, script: UnitId) -> Vec<(UnitId, &'g str, bool)> {
    let mut found: Vec<(UnitId, &'g str, bool)> = Vec::new();
    let mut stack: Vec<(UnitId, &'g str, bool)> = links_inputs(graph, script).into_iter().map(|(unit, links)| (unit, links, true)).collect();
    while let Some((unit, links, direct)) = stack.pop() {
        match found.iter_mut().find(|(seen, _, _)| *seen == unit) {
            Some(seen) => seen.2 |= direct,
            None => {
                found.push((unit, links, direct));
                stack.extend(links_inputs(graph, unit).into_iter().map(|(up, links)| (up, links, false)));
            }
        }
    }
    found.sort_unstable_by_key(|(unit, _, _)| *unit);
    found
}

/// The `links` metadata and producers build-script unit `script` is served from `given`, the
/// runs of its [`links_closure`]: each producer made from its record upstream first, so each
/// takes its own inputs' closures ([`DepProducer::closure`], one flatten); the metadata of each
/// direct one is a pair of the reader's environment. A direct `links` input of `script` that
/// `given` lacks is refused naming it ([`UnitScriptError::Dependency`]): a script is never run
/// without a `DEP_<LINKS>_*` cargo gives it (rule 3).
pub fn served_links<'g, 'r: 'g>(graph: &UnitGraph<'_>, script: UnitId, given: &'g [GivenRun<'r>]) -> Result<ServedLinks<'g>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    for (input, links) in links_inputs(graph, script) {
        if !given.iter().any(|run| run.script == input && run.direct) {
            return Err(UnitScriptError::Dependency { package: package(unit), dependency: package(&graph.units[input.index()]), links: links.to_owned() });
        }
    }
    let mut ids: Vec<UnitId> = given.iter().map(|run| run.script).collect();
    ids.sort_unstable();
    ids.dedup();
    let inputs: Vec<Vec<UnitId>> = ids.iter().map(|id| links_inputs(graph, *id).into_iter().map(|(up, _)| up).collect()).collect();
    let order = order_after(&ids, &inputs).map_err(|_| UnitScriptError::LinksCycle { package: package(unit) })?;
    let mut deps: Vec<DepMetadata<'g>> = Vec::new();
    let mut made: Vec<(UnitId, DepProducer)> = Vec::with_capacity(order.len());
    for id in order {
        let Some(run) = given.iter().find(|run| run.script == id) else { continue };
        if run.direct {
            deps.extend(run.outputs.metadata().map(|(key, value)| DepMetadata { links: run.links, key, value }));
        }
        let ups: Vec<(&str, &DepProducer)> =
            links_inputs(graph, id).into_iter().filter_map(|(up, links)| made.iter().find(|(s, _)| *s == up).map(|(_, producer)| (links, producer))).collect();
        let upstream = DepProducer::closure(&ups);
        made.push((id, DepProducer::of_record(blaze_own::Share::share(&run.tree), blaze_own::Share::share(&run.root), &run.outputs).with_upstream(upstream)));
    }
    let producers = given
        .iter()
        .filter(|run| run.direct)
        .filter_map(|run| made.iter().find(|(s, _)| *s == run.script).map(|(_, producer)| (run.links, producer.clone())))
        .collect();
    Ok(ServedLinks { deps, producers })
}

/// `script`'s outputs for the side whose cfg is `side_cfg` ([`outputs_viewing_on`]), served the
/// runs `given` of its `links` closure ([`served_links`]): THE run a command's `Script` task and
/// the census's exporter both make, wasm or native as [`placed`] says, the producers' files
/// served to either alike.
#[allow(clippy::too_many_arguments)]
pub fn outputs_given<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    side_cfg: Option<&[CfgAtom<'_>]>,
    tree: Tree,
    root: ScriptRoot,
    given: &[GivenRun<'_>],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let served = served_links(graph, script, given)?;
    let producers: Vec<(&str, &DepProducer)> = served.producers.iter().map(|(links, producer)| (*links, producer)).collect();
    outputs_viewing_on(runtime, setting, graph, script, side_cfg, tree, root, &served.deps, &producers, alloc)
}

/// The build-script units whose metadata `script` is given: for each direct dependency of the
/// unit it serves that has `links` and a build script, that script, with the dependency's
/// `links` (cargo: a package's `cargo::metadata` reaches the scripts of the packages that
/// depend on it directly, as `DEP_<LINKS>_<KEY>`). Sorted by unit, each once.
pub fn links_inputs<'g>(graph: &'g UnitGraph<'_>, script: UnitId) -> Vec<(UnitId, &'g str)> {
    let Some(serves) = served(graph, script) else { return Vec::new() };
    let mut inputs: Vec<(UnitId, &'g str)> = graph.units[serves.index()]
        .deps
        .iter()
        .filter_map(|dep| {
            let unit = &graph.units[dep.unit.index()];
            Some((unit.build_script?, unit.links.as_ref()?.as_str()))
        })
        .collect();
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

/// The build-script units of `graph` in an order where each comes after every script of its
/// [`links_inputs`]: a work list over those edges (Kahn's), ties by unit id, no recursion.
/// A script left over is in a cycle, refused by name.
pub fn links_order(graph: &UnitGraph<'_>) -> Result<Vec<UnitId>, UnitScriptError> {
    let scripts: Vec<UnitId> = (0..graph.units.len() as u32).map(UnitId).filter(|id| graph.units[id.index()].kind == UnitKind::BuildScript).collect();
    let inputs: Vec<Vec<UnitId>> = scripts.iter().map(|s| links_inputs(graph, *s).into_iter().map(|(u, _)| u).collect()).collect();
    order_after(&scripts, &inputs).map_err(|at| UnitScriptError::LinksCycle { package: package(&graph.units[scripts[at].index()]) })
}

/// `items` ordered so each comes after its `inputs` (those of them among `items`; others are
/// not waited on), ties in the order given. `Err` names the index of an item in a cycle.
fn order_after(items: &[UnitId], inputs: &[Vec<UnitId>]) -> Result<Vec<UnitId>, usize> {
    let at = |id: UnitId| items.binary_search(&id).ok();
    let mut waiting: Vec<usize> = inputs.iter().map(|i| i.iter().filter(|u| at(**u).is_some()).count()).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    for (i, ins) in inputs.iter().enumerate() {
        for d in ins.iter().filter_map(|u| at(*u)) {
            dependents[d].push(i);
        }
    }
    let mut ready: std::collections::BTreeSet<usize> = (0..items.len()).filter(|i| waiting[*i] == 0).collect();
    let mut order = Vec::with_capacity(items.len());
    while let Some(i) = ready.pop_first() {
        order.push(items[i]);
        for &d in &dependents[i] {
            waiting[d] -= 1;
            if waiting[d] == 0 {
                ready.insert(d);
            }
        }
    }
    match waiting.iter().position(|w| *w != 0) {
        Some(stuck) => Err(stuck),
        None => Ok(order),
    }
}

/// A dependency's `links` metadata as its dependents' scripts take it: each `cargo::metadata`
/// (or old `cargo:K=V`) pair of its outputs, under its `links`.
pub fn dep_metadata<'o>(links: &'o str, outputs: &'o Outputs<'_>) -> impl Iterator<Item = DepMetadata<'o>> {
    outputs.metadata.iter().map(move |(key, value)| DepMetadata { links, key: key.as_str(), value: value.as_str() })
}

/// Every build-script unit of `graph` run for the unit it serves, in [`links_order`], keyed by
/// that unit: what a reader of the outputs by library takes (blaze-world's `ScriptOutputs`).
/// `files` gives a package's tree and what its script sees (D96), or `None`. A script whose `links` input gave no
/// outputs is not run: it fails naming that dependency.
pub fn outputs_by_served<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    mut files: impl FnMut(&Unit<'_>) -> Option<(Tree, ScriptRoot)>,
    alloc: Alloc<'a>,
) -> (Vec<(UnitId, ScriptRun<'a>)>, Vec<(UnitId, UnitScriptError)>) {
    let (mut done, mut failed): (Vec<(UnitId, ScriptRun<'a>)>, Vec<(UnitId, UnitScriptError)>) = (Vec::new(), Vec::new());
    let order = match links_order(graph) {
        Ok(order) => order,
        Err(error) => return (done, vec![(UnitId(u32::MAX), error)]),
    };
    // Each script's run, by the script unit, for its dependents' metadata; and, for a script
    // whose served unit has `links`, what its dependents' scripts are served of it (brief S1).
    let mut ran_by_script: Vec<(UnitId, usize)> = Vec::new();
    let mut producers: Vec<(UnitId, DepProducer)> = Vec::new();
    for script in order {
        let unit = &graph.units[script.index()];
        let Some(serves) = served(graph, script) else {
            failed.push((script, UnitScriptError::ServesNothing { package: package(unit) }));
            continue;
        };
        let mut deps = Vec::new();
        let mut given: Vec<(&str, &DepProducer)> = Vec::new();
        let mut missing = None;
        for (input, links) in links_inputs(graph, script) {
            let producer = producers.iter().find(|(s, _)| *s == input).map(|(_, producer)| producer);
            match (ran_by_script.iter().find(|(s, _)| *s == input), producer) {
                (Some((_, at)), Some(producer)) => {
                    deps.extend(dep_metadata(links, &done[*at].1.outputs));
                    given.push((links, producer));
                }
                _ => {
                    let dependency = package(&graph.units[input.index()]);
                    missing = Some(UnitScriptError::Dependency { package: package(unit), dependency, links: links.to_owned() });
                    break;
                }
            }
        }
        if let Some(error) = missing {
            failed.push((serves, error));
            continue;
        }
        let Some((tree, root)) = files(unit) else {
            failed.push((serves, UnitScriptError::NoFiles { package: package(unit) }));
            continue;
        };
        // Brief S1b: what is upstream of this script, for its readers (a forwarded path).
        let upstream = DepProducer::closure(&given);
        let ran = outputs_viewing(runtime, setting, graph, script, blaze_own::Share::share(&tree), blaze_own::Share::share(&root), &deps, &given, alloc);
        match ran {
            Ok(ran) => {
                if graph.units[serves.index()].links.is_some() {
                    producers.push((script, DepProducer::of(tree, root, &ran.outputs).with_upstream(upstream)));
                }
                ran_by_script.push((script, done.len()));
                done.push((serves, ran));
            }
            Err(error) => failed.push((serves, error)),
        }
    }
    (done, failed)
}

#[cfg(test)]
mod tests {
    use blaze_build_script::{Given, Outputs, env::PackageFacts, instr::OutputLimits};
    use blaze_own::Alloc;
    use blaze_unit_graph::UnitId;

    use super::{dep_metadata, order_after};

    /// Two packages: `sys` has `links = "aws-lc"` and prints its include folder both ways;
    /// `user` depends on it directly. `user`'s script runs after `sys`'s and is given
    /// `DEP_AWS_LC_INCLUDE` and `DEP_AWS_LC_CONF`, and a changed value changes its environment
    /// (so its key). The control: without the dependency's metadata neither variable is there.
    #[test]
    fn a_links_dependencys_metadata_reaches_its_dependents_script() {
        let alloc = Alloc::global();
        let printed = b"cargo:include=/workspace/include\ncargo::metadata=conf=fips\ncargo:rustc-cfg=x\n";
        let sys = Outputs::read(printed, "/target/debug/build/sys-0123456789abcdef/out", OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        let deps: Vec<_> = dep_metadata("aws-lc", &sys).collect();
        let given = |deps| Given {
            package: PackageFacts { name: "user", version: "1.0.0", ..PackageFacts::default() },
            seen: blaze_expand::seen::SeenRoot::Package,
            features: &[],
            target_cfg: &[],
            target: "aarch64-apple-darwin",
            host: "aarch64-apple-darwin",
            deps,
        };
        let env = blaze_build_script::env(&given(&deps), alloc);
        assert_eq!(env.get("DEP_AWS_LC_INCLUDE"), Some("/workspace/include"));
        assert_eq!(env.get("DEP_AWS_LC_CONF"), Some("fips"));
        let bare = blaze_build_script::env(&given(&[]), alloc);
        assert_eq!(bare.get("DEP_AWS_LC_INCLUDE"), None);
        let other = Outputs::read(b"cargo:include=/workspace/other\n", "/target/debug/build/sys-0123456789abcdef/out", OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        let changed: Vec<_> = dep_metadata("aws-lc", &other).collect();
        assert_ne!(blaze_build_script::env(&given(&changed), alloc), env);
        // The order: `user` (unit 3) waits on `sys` (unit 7), though its id is lower; a third
        // script with no inputs keeps its place by id.
        let items = [UnitId(1), UnitId(3), UnitId(7)];
        let inputs = [vec![], vec![UnitId(7)], vec![]];
        assert_eq!(order_after(&items, &inputs), Ok(vec![UnitId(1), UnitId(7), UnitId(3)]));
        // A cycle is refused, never run in some order.
        let cycle = [vec![UnitId(3)], vec![UnitId(1)], vec![]];
        assert!(order_after(&items, &cycle).is_err());
    }
}
