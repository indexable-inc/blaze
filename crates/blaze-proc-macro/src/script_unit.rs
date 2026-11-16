//! A unit graph's build scripts, run for the units they serve: each script's module record
//! found by its package and exact feature set, its environment made from the unit, its outputs
//! from the record or from a run in the sandbox ([`ScriptRuntime::run`]).
//!
//! This is the step a driver's build-script task takes, and what feeds the outputs' readers:
//! the world's cfg of a crate (blaze-world's `ScriptOutputs`, by the served unit) and
//! `blaze_build_script::cx::ScriptCx` for `env!("OUT_DIR")` and `include!`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{
    Env, Outputs,
    env::{DepMetadata, Given, PackageFacts},
    key::{CMode, RunLimits},
    probe::Row,
};
use blaze_own::Alloc;
use blaze_unit_graph::{Unit, UnitGraph, UnitId, UnitKind};

use crate::{
    script::{LoadedScript, ScriptError, ScriptRun, ScriptRuntime},
    script_sandbox::{Script, Tree, owned_env},
};

/// What every script of one graph shares.
pub struct Setting<'s> {
    /// The store holding `build-script/modules` (the bootstrap's module records).
    pub store: &'s Path,
    pub target: &'s str,
    pub host: &'s str,
    /// The probe table's rows (`probe::pinned_rows`).
    pub table: Arc<Vec<Row>>,
    pub c_mode: CMode,
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
            UnitScriptError::ReadModule { path, error } => write!(f, "{}: {error}", path.display()),
            UnitScriptError::NoFiles { package } => write!(f, "no files of {package} to run its build script over"),
            UnitScriptError::Script(error) => write!(f, "{error}"),
            UnitScriptError::Dependency { package, dependency, links } => write!(
                f,
                "the build script of {package} needs the metadata of {dependency} (`links = \"{links}\"`, its DEP_* variables), whose build script gave no outputs"
            ),
            UnitScriptError::LinksCycle { package } => write!(f, "the build script of {package} is in a cycle of `links` metadata"),
        }
    }
}

impl std::error::Error for UnitScriptError {}

/// The module record (`<store>/build-script/modules/<key>`) of `name version` compiled with
/// exactly `features`: the one whose identity names all three.
pub fn module_of(store: &Path, name: &str, version: &str, features: &[&str]) -> Option<PathBuf> {
    let mut sorted = features.to_vec();
    sorted.sort_unstable();
    let lines = [format!("name {name}"), format!("version {version}"), format!("features {}", sorted.join(","))];
    for entry in std::fs::read_dir(store.join("build-script/modules")).ok()?.flatten() {
        let identity = std::fs::read_to_string(entry.path().join("identity")).unwrap_or_default();
        if lines.iter().all(|want| identity.lines().any(|l| l == want)) {
            return Some(entry.path());
        }
    }
    None
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

/// `script`'s module loaded and its inputs made: `tree` and `folder` are its package's files
/// (`script_sandbox::dir_tree` for a workspace member with its folder, `mem_tree` for a
/// fetched package with an empty folder). `deps` is the `links` metadata of the served unit's
/// direct dependencies ([`dep_metadata`] of each of [`links_inputs`]); it is in the
/// environment, so in the run's key.
#[allow(clippy::too_many_arguments)]
pub fn prepare<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    folder: String,
    deps: &[DepMetadata<'_>],
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
    let module = module_of(setting.store, unit.name.as_str(), unit.version.as_str(), &features).ok_or_else(|| UnitScriptError::NoModule {
        package: package.clone(),
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        store: setting.store.to_owned(),
    })?;
    let path = module.join("module.wasm");
    let wasm = std::fs::read(&path).map_err(|error| UnitScriptError::ReadModule { path, error })?;
    let loaded = runtime.load(&package, &wasm).map_err(UnitScriptError::Script)?;
    let facts = PackageFacts { name: unit.name.as_str(), version: unit.version.as_str(), links: unit.links.as_ref().map(|l| l.as_str()), ..PackageFacts::default() };
    let given = Given { package: facts, folder: &folder, features: &unit.features, target_cfg: &serves.cfg, target: setting.target, host: setting.host, deps };
    let env = blaze_build_script::env(&given, alloc);
    let script = Arc::new(Script {
        package,
        tree,
        folder,
        env: owned_env(&env),
        target: setting.target.to_owned(),
        host: setting.host.to_owned(),
        c_mode: setting.c_mode,
        limits: RunLimits::STANDARD,
        table: Arc::clone(&setting.table),
    });
    Ok(Prepared { loaded, script, env })
}

/// `script`'s outputs, from its record when one answers, else run and kept.
#[allow(clippy::too_many_arguments)]
pub fn outputs<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    folder: String,
    deps: &[DepMetadata<'_>],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let prepared = prepare(runtime, setting, graph, script, tree, folder, deps, alloc)?;
    runtime.run(&prepared.loaded, prepared.script, &prepared.env, alloc).map_err(UnitScriptError::Script)
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
/// `files` gives a package's tree and folder, or `None`. A script whose `links` input gave no
/// outputs is not run: it fails naming that dependency.
pub fn outputs_by_served<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    mut files: impl FnMut(&Unit<'_>) -> Option<(Tree, String)>,
    alloc: Alloc<'a>,
) -> (Vec<(UnitId, ScriptRun<'a>)>, Vec<(UnitId, UnitScriptError)>) {
    let (mut done, mut failed): (Vec<(UnitId, ScriptRun<'a>)>, Vec<(UnitId, UnitScriptError)>) = (Vec::new(), Vec::new());
    let order = match links_order(graph) {
        Ok(order) => order,
        Err(error) => return (done, vec![(UnitId(u32::MAX), error)]),
    };
    // Each script's run, by the script unit, for its dependents' metadata.
    let mut ran_by_script: Vec<(UnitId, usize)> = Vec::new();
    for script in order {
        let unit = &graph.units[script.index()];
        let Some(serves) = served(graph, script) else {
            failed.push((script, UnitScriptError::ServesNothing { package: package(unit) }));
            continue;
        };
        let mut deps = Vec::new();
        let mut missing = None;
        for (input, links) in links_inputs(graph, script) {
            match ran_by_script.iter().find(|(s, _)| *s == input) {
                Some((_, at)) => deps.extend(dep_metadata(links, &done[*at].1.outputs)),
                None => {
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
        let Some((tree, folder)) = files(unit) else {
            failed.push((serves, UnitScriptError::NoFiles { package: package(unit) }));
            continue;
        };
        match outputs(runtime, setting, graph, script, tree, folder, &deps, alloc) {
            Ok(ran) => {
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
            folder: "",
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
