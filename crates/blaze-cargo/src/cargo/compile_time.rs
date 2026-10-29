//! The build scripts and proc macros of a load, built and run by blaze in place of
//! rust-analyzer's `cargo check --compile-time-deps` (docs/no-rustc.md, steps I, J and K). What
//! cargo did there is scheduling and bookkeeping over its unit graph; the compiles were
//! already blaze's (`own_script.rs`, `own_macro.rs`) and the runs already went through
//! `blaze-engine build-script` (`script.rs`). Here the same compiles and runs are driven from the
//! graph (`plan.rs` `UnitGraph`):
//!
//! - each library a build script or proc macro links is described from its unit: its root,
//!   edition, `feature` cfgs, `debug_assertions` from its profile, the `--cfg`s of the flags
//!   cargo would pass, and what its own build script printed (`rustc-cfg`, `rustc-env`,
//!   `OUT_DIR`), with the variables cargo sets for rustc of it;
//! - each build script is compiled from that description and run as cargo runs it: the
//!   environment is `blaze-build-script`'s port of cargo's, its `DEP_<links>_<key>` from the
//!   scripts of the dependencies it runs after, and what it printed is read as cargo reads it;
//! - each proc macro is linked as a shared library for blaze's proc-macro server.
//!
//! What a package's script printed and where it wrote, and its proc macro's library, are what
//! the load reads (`project_model::OwnBuildScripts`). Nothing falls back to cargo: what blaze
//! cannot build or run fails the load, saying what and why. `harness/resolve.py ctdeps` holds
//! the result to cargo's.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::SystemTime,
};

use anyhow::Context as _;
use blaze_build_script as script;
use ide_db::base_db::TargetKind;
use project_model::{CargoWorkspace, OwnBuildAsked, OwnBuildOutput, OwnBuildScripts, Package};
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest as _, Sha256};

use super::own_script::{self, Described, Made};
use crate::plan::{Selection, Unit, UnitGraph};

/// The variables a load names to the cargo it would run, which nothing here runs.
const FOR_CARGO: [&str; 3] =
    ["CARGO_UNSTABLE_CHECKSUM_FRESHNESS", super::own_macro::OWN_TARGET, super::own_macro::SERVER];

/// The crate name cargo gives every build script.
const SCRIPT: &str = "build_script_build";

/// The hook that builds a load's build scripts and proc macros for the build `selection`
/// names.
pub fn own(selection: &Selection) -> OwnBuildScripts {
    let selection = selection.clone();
    OwnBuildScripts(Arc::new(move |cargo, asked| build(cargo, asked, &selection)))
}

/// Builds and runs the build scripts and proc macros of the build `selection` names of
/// `cargo`: what each package's script printed and where it wrote, and its proc macro.
pub fn build(
    cargo: &CargoWorkspace,
    asked: &OwnBuildAsked<'_>,
    selection: &Selection,
) -> anyhow::Result<Vec<(Package, OwnBuildOutput)>> {
    let root: &Path = cargo.workspace_root().as_ref();
    let native = match super::own_macro::wasm() {
        true => Some(super::own_macro::native_proc_macros(root).map_err(anyhow::Error::msg)?),
        false => None,
    };
    if native.is_none() && super::own_macro::server().is_none() {
        anyhow::bail!("{}", super::own_macro::missing_server());
    }
    let config = asked.config;
    let manifest: &Path = cargo.manifest_path().as_ref();
    let toolchain: PathBuf = asked.sysroot.root().context("the load has no toolchain")?.into();
    let host = host_of()?;
    let triple = selection.target.as_deref().unwrap_or(&host);
    let for_graph = crate::plan::Asked { triple, cfg: asked.rustc_cfg, env: &config.extra_env };
    let graph = UnitGraph::asked(manifest, selection, &for_graph)?;
    let own = config
        .target_dir_config
        .target_dir(Some(cargo.target_directory().as_ref()))
        .context("the load has no target directory of its own")?;
    let own = PathBuf::from(own.as_str()).join("compile-time");
    let mut driver =
        Driver::new(cargo, &graph, &toolchain, host, &own, manifest, selection, config)?;
    driver.native = native;
    let ran = driver.run()?;
    Ok(driver.outputs(&ran))
}

/// What the units of one platform are built with.
struct Platform {
    /// `TARGET`.
    triple: String,
    /// What `rustc --print cfg` says of it with `rustflags`, a line each.
    cfg: Vec<String>,
    /// The flags cargo passes rustc for its units.
    rustflags: Vec<String>,
    linker: Option<PathBuf>,
}

/// One step of the build: compiling a build script, running one, or linking a proc macro,
/// each by its unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Task {
    Script(usize),
    Run(usize),
    Macro(usize),
}

/// A run of a build script that succeeded.
struct Ran {
    out_dir: PathBuf,
    output: script::Output,
}

/// What the steps made, by unit.
#[derive(Default)]
struct Made_ {
    scripts: FxHashMap<usize, PathBuf>,
    runs: FxHashMap<usize, Arc<Ran>>,
    macros: FxHashMap<usize, PathBuf>,
}

struct Driver<'a> {
    cargo: &'a CargoWorkspace,
    graph: &'a UnitGraph,
    /// The workspace's package of each unit.
    packages: Vec<Option<Package>>,
    /// The key of each unit: what it is and what it depends on, for where it is kept.
    keys: Vec<String>,
    toolchain: &'a Path,
    own: &'a Path,
    host: String,
    /// The platform of the units without one, and of those with `--target`'s.
    platforms: FxHashMap<Option<String>, Platform>,
    /// The environment cargo would have been started with, as its scripts inherit it.
    inherited: Vec<(OsString, OsString)>,
    config_env: Vec<script::unit::ConfigEnv>,
    tools: script::unit::Tools,
    jobs: u32,
    made: Mutex<Made_>,
    /// With proc macros built as WebAssembly modules (`own_macro::wasm`), the crates of those
    /// the workspace expands natively (`own_macro::NATIVE_KEY`); `None` when every one is
    /// linked for blaze's server.
    native: Option<Vec<String>>,
    /// The load's manifest, build and variables, as its unit graph was asked for.
    manifest: PathBuf,
    selection: Selection,
    extra_env: FxHashMap<String, Option<String>>,
    /// The unit graph of the build with [`own_macro::WASM_TARGET`] as its host, made once a
    /// module links a crate with dependencies of that platform alone.
    ///
    /// [`own_macro::WASM_TARGET`]: super::own_macro::WASM_TARGET
    wasm_graph: OnceLock<Result<Arc<UnitGraph>, String>>,
    /// Whether the package of each unit has dependencies [`own_macro::WASM_TARGET`] has and
    /// the host has not.
    ///
    /// [`own_macro::WASM_TARGET`]: super::own_macro::WASM_TARGET
    wasm_only: Mutex<FxHashMap<usize, bool>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Unit {
    fn is_run(&self) -> bool {
        self.mode == "run-custom-build"
    }

    fn is_script(&self) -> bool {
        !self.is_run() && self.target.kind.iter().any(|it| it == "custom-build")
    }

    fn is_proc_macro(&self) -> bool {
        self.mode == "build" && self.target.kind.iter().any(|it| it == "proc-macro")
    }

    /// Whether a crate that depends on it links it: a library or a proc macro.
    fn is_linked(&self) -> bool {
        !self.is_run()
            && !self.is_script()
            && (self.is_library() || self.target.kind.iter().any(|it| it == "proc-macro"))
    }

    fn crate_name(&self) -> String {
        self.target.name.replace('-', "_")
    }
}

impl<'a> Driver<'a> {
    fn new(
        cargo: &'a CargoWorkspace,
        graph: &'a UnitGraph,
        toolchain: &'a Path,
        host: String,
        own: &'a Path,
        manifest: &Path,
        selection: &Selection,
        config: &project_model::CargoConfig,
    ) -> anyhow::Result<Driver<'a>> {
        let dir = manifest.parent().unwrap_or(Path::new("/"));
        let by_id: FxHashMap<String, Package> = cargo
            .packages()
            .map(|it| (crate::plan::package_named(&cargo[it].id.repr), it))
            .collect();
        let packages = graph
            .units
            .iter()
            .map(|unit| by_id.get(&crate::plan::package_named(&unit.pkg_id)).copied())
            .collect();
        // cargo's own environment: this process's, with what the load asks cargo to be run
        // with, as libgit2 leaves it once cargo started.
        let mut vars: BTreeMap<String, String> = std::env::vars().collect();
        for (name, value) in &config.extra_env {
            if FOR_CARGO.contains(&name.as_str()) {
                continue;
            }
            match value {
                Some(value) => vars.insert(name.clone(), value.clone()),
                None => vars.remove(name),
            };
        }
        let started: Vec<(OsString, OsString)> =
            vars.iter().map(|(k, v)| (k.into(), v.into())).collect();
        let inherited = script::env::cargo_started(&started, &host);
        let settings = blaze_manifest::config::Config::load(dir, vars, &selection.config)?;
        use blaze_manifest::config::target::{CompileKind, Flags};
        let requested = match &selection.target {
            Some(triple) => vec![CompileKind::Target(triple.clone())],
            None => vec![CompileKind::Host],
        };
        let mut platforms = FxHashMap::default();
        for platform in graph.units.iter().map(|it| it.platform.clone()) {
            if platforms.contains_key(&platform) {
                continue;
            }
            let triple = platform.clone().unwrap_or_else(|| host.clone());
            let kind = match &platform {
                Some(triple) => CompileKind::Target(triple.clone()),
                None => CompileKind::Host,
            };
            let mut ask = vec!["--print", "cfg"];
            if platform.is_some() {
                ask.extend(["--target", &triple]);
            }
            let plain = cfgs(&said(&ask)?);
            let rustflags =
                settings.extra_args(&requested, &host, Some(&plain), &kind, Flags::Rust)?;
            let linker = settings.target_linker(&kind, &host, &plain)?;
            ask.extend(rustflags.iter().map(String::as_str));
            let cfg = said(&ask)?.lines().map(str::to_owned).collect();
            platforms.insert(platform, Platform { triple, cfg, rustflags, linker });
        }
        let config_env = settings
            .env_config()?
            .into_iter()
            .map(|(name, value)| script::unit::ConfigEnv {
                name,
                value: value.to_string_lossy().into_owned(),
                // Already only what cargo's environment lacks, or forced.
                force: true,
            })
            .collect();
        let bin = toolchain.join("bin");
        let exe = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let tools = script::unit::Tools {
            cargo: exe("cargo"),
            rustc: exe("rustc"),
            rustdoc: exe("rustdoc"),
            rustc_wrapper: None,
            rustc_workspace_wrapper: None,
            linker: None,
        };
        let jobs = std::thread::available_parallelism().map_or(1, |it| it.get());
        Ok(Driver {
            cargo,
            graph,
            packages,
            keys: keys(graph),
            toolchain,
            own,
            host,
            platforms,
            inherited,
            config_env,
            tools,
            jobs: u32::try_from(jobs).unwrap_or(1),
            made: Mutex::new(Made_::default()),
            native: None,
            manifest: manifest.to_owned(),
            selection: selection.clone(),
            extra_env: config.extra_env.clone(),
            wasm_graph: OnceLock::new(),
            wasm_only: Mutex::new(FxHashMap::default()),
        })
    }

    fn unit(&self, at: usize) -> &'a Unit {
        &self.graph.units[at]
    }

    fn package(&self, at: usize) -> anyhow::Result<&'a project_model::PackageData> {
        let cargo = self.cargo;
        match self.packages[at] {
            Some(package) => Ok(&cargo[package]),
            None => anyhow::bail!(
                "the unit graph's `{}` is no package of the workspace",
                self.unit(at).pkg_id
            ),
        }
    }

    fn platform(&self, at: usize) -> &Platform {
        &self.platforms[&self.unit(at).platform]
    }

    /// The units `at` depends on.
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        self.unit(at)
            .dependencies
            .iter()
            .map(|it| it.index)
            .filter(|&it| it < self.graph.units.len())
    }

    /// The run of the build script of `at`'s package that `at` is compiled with.
    fn own_run(&self, at: usize) -> Option<usize> {
        let unit = self.unit(at);
        self.deps(at).find(|&it| self.unit(it).is_run() && self.unit(it).pkg_id == unit.pkg_id)
    }

    /// The steps of the build and what each waits for.
    fn tasks(&self) -> Vec<(Task, Vec<Task>)> {
        // A proc macro nothing depends on is one cargo's check only checks.
        let depended: FxHashSet<usize> =
            (0..self.graph.units.len()).flat_map(|it| self.deps(it)).collect();
        let mut tasks = Vec::new();
        for (at, unit) in self.graph.units.iter().enumerate() {
            let task = if unit.is_run() {
                Task::Run(at)
            } else if unit.is_script() {
                Task::Script(at)
            } else if unit.is_proc_macro() && depended.contains(&at) {
                Task::Macro(at)
            } else {
                continue;
            };
            let waits = match task {
                Task::Run(_) => self
                    .deps(at)
                    .filter_map(|it| {
                        let dep = self.unit(it);
                        if dep.is_script() {
                            Some(Task::Script(it))
                        } else if dep.is_run() {
                            Some(Task::Run(it))
                        } else {
                            None
                        }
                    })
                    .collect(),
                Task::Script(_) | Task::Macro(_) => {
                    // Every library it is described with, and what those are described with.
                    let mut waits = Vec::new();
                    let mut seen = FxHashSet::default();
                    let mut queue: Vec<usize> = vec![at];
                    while let Some(it) = queue.pop() {
                        if !seen.insert(it) {
                            continue;
                        }
                        if let Some(run) = self.own_run(it) {
                            waits.push(Task::Run(run));
                        }
                        if it != at && self.unit(it).is_proc_macro() {
                            waits.push(Task::Macro(it));
                        }
                        queue.extend(self.deps(it).filter(|&dep| self.unit(dep).is_linked()));
                    }
                    waits
                }
            };
            tasks.push((task, waits));
        }
        tasks
    }

    /// Does every step as a node of a build graph (docs/driver.md), as many at once as there
    /// are workers, each once what it waits for is done, and says each node and its moves.
    /// The first failure fails the build once the steps under way end.
    fn run(&self) -> anyhow::Result<()> {
        use blaze_graph::{Config, Machine, NodeId, Pool, Spec, exec};
        use blaze_protocol::events::NodeKind;
        let tasks = self.tasks();
        let index: FxHashMap<Task, usize> =
            tasks.iter().enumerate().map(|(at, (task, _))| (*task, at)).collect();
        let first = exec::ids(tasks.len());
        let id = |at: usize| NodeId(first + at as u32);
        let nodes = tasks
            .iter()
            .enumerate()
            .map(|(at, (task, waits))| {
                let mut deps: Vec<NodeId> =
                    waits.iter().filter_map(|it| index.get(it)).map(|&it| id(it)).collect();
                deps.sort();
                deps.dedup();
                let (unit, kind) = match *task {
                    Task::Script(unit) => (unit, NodeKind::Script),
                    Task::Run(unit) => (unit, NodeKind::ScriptRun),
                    Task::Macro(unit) => (unit, NodeKind::Macro),
                };
                let name = self.package(unit).map_or_else(
                    |_| self.unit(unit).pkg_id.clone(),
                    |it| format!("{} v{}", it.name, it.version),
                );
                // One pool for compiles and runs alike: a run can be a C build that takes cores
                // of its own, and together they are held to `workers` as before.
                Spec { id: id(at), kind, name, deps, pool: Pool::Work, cost: 1 }
            })
            .collect();
        let workers =
            std::thread::available_parallelism().map_or(1, |it| it.get()).div_ceil(4).max(4);
        let config = Config { io: 1, work: workers, keep_going: false };
        let mut machine = Machine::new(config)?;
        let slots = workers.min(tasks.len().max(1));
        let counting = crate::events::Counting::new(
            blaze_protocol::events::Step::BuildScripts,
            tasks.len(),
            slots,
        );
        // The slot a step under way says what it works on in: at most `slots` are under way.
        let free: Mutex<Vec<usize>> = Mutex::new((0..slots).rev().collect());
        let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        let work = |node: NodeId| {
            let at = (node.0 - first) as usize;
            let slot = lock(&free).pop().unwrap_or_default();
            counting.on(slot, || self.shown(tasks[at].0));
            let result = self.step(tasks[at].0);
            counting.done(slot);
            lock(&free).push(slot);
            match result {
                Ok(()) => exec::Did::built(),
                Err(error) => {
                    let why = format!("{error:#}");
                    lock(&failed).get_or_insert(error);
                    exec::Did::failed(why)
                }
            }
        };
        // A step that panicked failed with no error of its own: the machine has why.
        let mut first_why = None;
        let mut said = |record: &blaze_protocol::events::Record| {
            if let blaze_protocol::events::Event::Moved {
                state: blaze_protocol::events::NodeState::Failed { why },
                ..
            } = &record.event
            {
                first_why.get_or_insert_with(|| why.clone());
            }
            crate::events::say(record.event.clone());
        };
        let ok = exec::drive(&mut machine, nodes, &work, &mut said, None)?;
        if let Some(error) = failed.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(error);
        }
        match (ok, first_why) {
            (true, _) => Ok(()),
            (false, why) => anyhow::bail!(
                "a step of the build failed: {}",
                why.as_deref().unwrap_or("it said no more")
            ),
        }
    }

    /// A step as the build's progress names it: `serde (build script)`.
    fn shown(&self, task: Task) -> String {
        let (at, what) = match task {
            Task::Script(at) => (at, "compiling its build script"),
            Task::Run(at) => (at, "running its build script"),
            Task::Macro(at) => (at, "linking the proc macro"),
        };
        let name = self
            .package(at)
            .map_or_else(|_| self.unit(at).target.name.clone(), |it| it.name.clone());
        format!("{name}, {what}")
    }

    fn step(&self, task: Task) -> anyhow::Result<()> {
        let (at, what) = match task {
            Task::Script(at) => (at, "compile the build script"),
            Task::Run(at) => (at, "run the build script"),
            Task::Macro(at) => (at, "link the proc macro"),
        };
        let done = match task {
            Task::Script(at) => {
                self.script(at).map(|exe| _ = lock(&self.made).scripts.insert(at, exe))
            }
            Task::Run(at) => {
                self.run_script(at).map(|ran| _ = lock(&self.made).runs.insert(at, Arc::new(ran)))
            }
            Task::Macro(at) => {
                self.proc_macro(at).map(|lib| _ = lock(&self.made).macros.insert(at, lib))
            }
        };
        let unit = self.unit(at);
        done.with_context(|| {
            let name = self
                .package(at)
                .map_or(unit.pkg_id.clone(), |it| format!("{} v{}", it.name, it.version));
            format!("blaze could not {what} of `{name}` for the load")
        })
    }

    /// The directory of a unit's files: `build/<package>-<key>` for a build script's, as
    /// cargo's first layout has it.
    fn unit_dir(&self, at: usize) -> PathBuf {
        let unit = self.unit(at);
        let name = self.package(at).map_or_else(|_| unit.target.name.clone(), |it| it.name.clone());
        self.own.join("build").join(format!("{name}-{}", self.keys[at]))
    }

    /// The `--cfg`s rustc compiles the unit with beside the target's: its features,
    /// `debug_assertions` as its profile has it, and those of the flags.
    fn cfg(&self, at: usize) -> Vec<String> {
        unit_cfg(self.unit(at), &self.platform(at).rustflags)
    }

    /// Adds to `crates`, the description of the module of the proc macro of unit `at` with
    /// where each unit is in it (`linked`), the libraries its crates link only when built for
    /// [`own_macro::WASM_TARGET`]: a dependency of `cfg(target_os = "wasi")` (getrandom 0.2's
    /// `wasi`), which the load's graph, of the host, has not. They are taken from the graph of
    /// the build for that target, made only when a crate of the module has such dependencies.
    ///
    /// [`own_macro::WASM_TARGET`]: super::own_macro::WASM_TARGET
    fn link_for_wasm(
        &self,
        at: usize,
        crates: &mut Vec<Described>,
        linked: &FxHashMap<usize, usize>,
    ) -> anyhow::Result<()> {
        let mut units: Vec<usize> = linked.keys().copied().collect();
        units.sort_unstable();
        let mut wanted = false;
        for &it in &units {
            wanted |= self.has_wasm_only(it)?;
        }
        if !wanted {
            return Ok(());
        }
        let wasm = self.wasm_graph()?;
        let unit = self.unit(at);
        let same = |host: &Unit, wasm: &Unit| {
            host.pkg_id == wasm.pkg_id
                && host.target.name == wasm.target.name
                && host.is_proc_macro() == wasm.is_proc_macro()
                && wasm.is_linked()
        };
        let counterpart = wasm.units.iter().enumerate().filter(|(_, it)| same(unit, it));
        let counterpart = counterpart.max_by_key(|(_, it)| it.features == unit.features);
        let (first, _) = counterpart.with_context(|| {
            format!("the graph of the build for wasm32-wasip1 has no proc macro `{}`", unit.pkg_id)
        })?;
        // Each unit of the host's graph in the module with its counterpart in `wasm`, and the
        // units of `wasm` described here, by where they are in `crates`.
        let mut queue = vec![(at, first)];
        let mut paired = FxHashSet::default();
        let mut described: FxHashMap<usize, usize> = FxHashMap::default();
        while let Some((host, built)) = queue.pop() {
            if !paired.insert(host) {
                continue;
            }
            let host_deps: Vec<usize> =
                self.deps(host).filter(|&it| self.unit(it).is_linked()).collect();
            for dep in &wasm.units[built].dependencies {
                let Some(it) = wasm.units.get(dep.index).filter(|it| it.is_linked()) else {
                    continue;
                };
                if let Some(&mate) = host_deps.iter().find(|&&h| same(self.unit(h), it)) {
                    queue.push((mate, dep.index));
                    continue;
                }
                let name = match dep.extern_crate_name.is_empty() {
                    true => it.crate_name(),
                    false => dep.extern_crate_name.clone(),
                };
                let module = &unit.pkg_id;
                let index =
                    self.describe_wasm(&wasm, dep.index, module, crates, linked, &mut described)?;
                crates[linked[&host]].deps.push((name, index));
            }
        }
        Ok(())
    }

    /// Adds unit `at` of `wasm`, the graph for [`own_macro::WASM_TARGET`], to `crates`
    /// described, as a library the module of the proc macro of the package `module` links
    /// there alone, with what it links: where it is in them. What is in the host's graph too
    /// is described as `linked` has it; what is not as `described` has it.
    ///
    /// [`own_macro::WASM_TARGET`]: super::own_macro::WASM_TARGET
    fn describe_wasm(
        &self,
        wasm: &UnitGraph,
        at: usize,
        module: &str,
        crates: &mut Vec<Described>,
        linked: &FxHashMap<usize, usize>,
        described: &mut FxHashMap<usize, usize>,
    ) -> anyhow::Result<usize> {
        if let Some(&it) = described.get(&at) {
            return Ok(it);
        }
        let unit = &wasm.units[at];
        let host = linked.keys().find(|&&it| {
            let it = self.unit(it);
            it.pkg_id == unit.pkg_id && it.target.name == unit.target.name
        });
        if let Some(host) = host {
            return Ok(linked[host]);
        }
        let scripted = unit.dependencies.iter().any(|it| {
            wasm.units.get(it.index).is_some_and(|it| it.is_run() && it.pkg_id == unit.pkg_id)
        });
        if scripted || unit.is_proc_macro() {
            anyhow::bail!(
                "the proc macro `{module}` links `{}` only when built for wasm32-wasip1, and it \
                 is a proc macro or has a build script, which blaze builds for the host alone",
                unit.pkg_id
            );
        }
        let crate_name = unit.crate_name();
        let index = crates.len();
        crates.push(Described {
            name: crate_name.clone(),
            root: unit.target.src_path.clone(),
            edition: unit.target.edition.clone(),
            cfg: unit_cfg(unit, &[]),
            env: self.manifest_env(&unit.target.src_path, &crate_name)?,
            deps: Vec::new(),
            proc_macro: None,
            compiled_macro: false,
            inputs: None,
        });
        described.insert(at, index);
        for dep in &unit.dependencies {
            let Some(it) = wasm.units.get(dep.index).filter(|it| it.is_linked()) else {
                continue;
            };
            let name = match dep.extern_crate_name.is_empty() {
                true => it.crate_name(),
                false => dep.extern_crate_name.clone(),
            };
            let found = self.describe_wasm(wasm, dep.index, module, crates, linked, described)?;
            crates[index].deps.push((name, found));
        }
        Ok(index)
    }

    /// Whether the manifest of unit `at`'s package has a `[target.'cfg(..)'.dependencies]`
    /// table that [`own_macro::WASM_TARGET`] has and the unit's platform has not.
    ///
    /// [`own_macro::WASM_TARGET`]: super::own_macro::WASM_TARGET
    fn has_wasm_only(&self, at: usize) -> anyhow::Result<bool> {
        if let Some(&it) = lock(&self.wasm_only).get(&at) {
            return Ok(it);
        }
        let manifest = &self.package(at)?.manifest;
        let text =
            std::fs::read_to_string(manifest).with_context(|| format!("reading {manifest}"))?;
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("reading {manifest}"))?;
        let wasm_cfg = cfgs(&said(&["--print", "cfg", "--target", super::own_macro::WASM_TARGET])?);
        let platform = self.platform(at);
        let host_cfg = cfgs(&platform.cfg.join("\n"));
        let tables = table.get("target").and_then(toml::Value::as_table).into_iter().flatten();
        let mut found = false;
        for (key, value) in tables {
            let Ok(target) = key.parse::<cargo_platform::Platform>() else { continue };
            let deps = value.get("dependencies").and_then(toml::Value::as_table);
            if deps.is_some_and(|it| !it.is_empty())
                && target.matches(super::own_macro::WASM_TARGET, &wasm_cfg)
                && !target.matches(&platform.triple, &host_cfg)
            {
                found = true;
                break;
            }
        }
        lock(&self.wasm_only).insert(at, found);
        Ok(found)
    }

    /// The unit graph of the load's build with [`own_macro::WASM_TARGET`] as its host, the
    /// platform its proc macros and what they link are built for.
    ///
    /// [`own_macro::WASM_TARGET`]: super::own_macro::WASM_TARGET
    fn wasm_graph(&self) -> anyhow::Result<Arc<UnitGraph>> {
        let made = self.wasm_graph.get_or_init(|| {
            let target = super::own_macro::WASM_TARGET;
            let printed =
                said(&["--print", "cfg", "--target", target]).map_err(|it| format!("{it:#}"))?;
            let cfg = atoms(&printed);
            let selection = Selection { target: None, ..self.selection.clone() };
            let asked = crate::plan::Asked { triple: target, cfg: &cfg, env: &self.extra_env };
            UnitGraph::asked(&self.manifest, &selection, &asked).map_err(|it| format!("{it:#}"))
        });
        made.clone().map_err(|why| {
            anyhow::anyhow!("making the build's unit graph for wasm32-wasip1: {why}")
        })
    }

    /// The variables cargo sets for rustc of the crate `crate_name` whose root is `root`, of
    /// the package whose manifest is the nearest above it: a package of a registry, which no
    /// build script of the load ran for.
    fn manifest_env(
        &self,
        root: &Path,
        crate_name: &str,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let manifest = root
            .ancestors()
            .skip(1)
            .map(|it| it.join("Cargo.toml"))
            .find(|it| it.is_file())
            .with_context(|| format!("no Cargo.toml above {}", root.display()))?;
        let text = std::fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?;
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
        let package = table.get("package").and_then(toml::Value::as_table);
        let package =
            package.with_context(|| format!("{} has no [package]", manifest.display()))?;
        let field = |name: &str| match package.get(name) {
            Some(toml::Value::String(it)) => it.clone(),
            Some(toml::Value::Array(it)) => {
                it.iter().filter_map(toml::Value::as_str).collect::<Vec<_>>().join(":")
            }
            _ => String::new(),
        };
        let version = semver::Version::parse(&field("version"))
            .with_context(|| format!("the version of {}", manifest.display()))?;
        let mut env = BTreeMap::new();
        let mut set = |name: &str, value: String| _ = env.insert(name.to_owned(), value);
        set("CARGO", self.tools.cargo.display().to_string());
        let dir = manifest.parent().unwrap_or(Path::new("/"));
        set("CARGO_MANIFEST_DIR", dir.display().to_string());
        set("CARGO_MANIFEST_PATH", manifest.display().to_string());
        set("CARGO_PKG_VERSION", version.to_string());
        set("CARGO_PKG_VERSION_MAJOR", version.major.to_string());
        set("CARGO_PKG_VERSION_MINOR", version.minor.to_string());
        set("CARGO_PKG_VERSION_PATCH", version.patch.to_string());
        set("CARGO_PKG_VERSION_PRE", version.pre.to_string());
        set("CARGO_PKG_NAME", field("name"));
        set("CARGO_PKG_AUTHORS", field("authors"));
        set("CARGO_PKG_DESCRIPTION", field("description"));
        set("CARGO_PKG_HOMEPAGE", field("homepage"));
        set("CARGO_PKG_REPOSITORY", field("repository"));
        set("CARGO_PKG_LICENSE", field("license"));
        set("CARGO_PKG_LICENSE_FILE", field("license-file"));
        set("CARGO_PKG_RUST_VERSION", field("rust-version"));
        set("CARGO_PKG_README", field("readme"));
        set("CARGO_CRATE_NAME", crate_name.to_owned());
        Ok(env)
    }

    /// The variables cargo sets for rustc of the unit's crate that its source can read with
    /// `env!`, with what its package's script printed (`ran`).
    fn crate_env(
        &self,
        at: usize,
        crate_name: &str,
        ran: Option<&Ran>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let package = self.package(at)?;
        let mut env = BTreeMap::new();
        let mut set = |name: &str, value: String| _ = env.insert(name.to_owned(), value);
        let dir = package.manifest.parent();
        set("CARGO", self.tools.cargo.display().to_string());
        set("CARGO_MANIFEST_DIR", dir.to_string());
        set("CARGO_MANIFEST_PATH", package.manifest.to_string());
        let version = &package.version;
        set("CARGO_PKG_VERSION", version.to_string());
        set("CARGO_PKG_VERSION_MAJOR", version.major.to_string());
        set("CARGO_PKG_VERSION_MINOR", version.minor.to_string());
        set("CARGO_PKG_VERSION_PATCH", version.patch.to_string());
        set("CARGO_PKG_VERSION_PRE", version.pre.to_string());
        set("CARGO_PKG_NAME", package.name.clone());
        set("CARGO_PKG_AUTHORS", package.authors.join(":"));
        let field = |it: &Option<String>| it.clone().unwrap_or_default();
        set("CARGO_PKG_DESCRIPTION", field(&package.description));
        set("CARGO_PKG_HOMEPAGE", field(&package.homepage));
        set("CARGO_PKG_REPOSITORY", field(&package.repository));
        set("CARGO_PKG_LICENSE", field(&package.license));
        set(
            "CARGO_PKG_LICENSE_FILE",
            package.license_file.as_ref().map(ToString::to_string).unwrap_or_default(),
        );
        set("CARGO_PKG_RUST_VERSION", package.rust_version_as_written().unwrap_or_default());
        set(
            "CARGO_PKG_README",
            package.readme.as_ref().map(ToString::to_string).unwrap_or_default(),
        );
        set("CARGO_CRATE_NAME", crate_name.to_owned());
        if let Some(ran) = ran {
            set("OUT_DIR", ran.out_dir.display().to_string());
            for (name, value) in &ran.output.env {
                set(name, value.clone());
            }
        }
        Ok(env)
    }

    fn ran(&self, at: usize) -> anyhow::Result<Arc<Ran>> {
        let ran = lock(&self.made).runs.get(&at).cloned();
        ran.with_context(|| format!("the build script of `{}` has not run", self.unit(at).pkg_id))
    }

    /// Adds the library of unit `at` to `crates`, described, and what it links: where it is
    /// in them. `linked` has where each unit already is.
    fn describe(
        &self,
        at: usize,
        crates: &mut Vec<Described>,
        linked: &mut FxHashMap<usize, usize>,
    ) -> anyhow::Result<usize> {
        if let Some(&it) = linked.get(&at) {
            return Ok(it);
        }
        let unit = self.unit(at);
        let ran = self.own_run(at).map(|it| self.ran(it)).transpose()?;
        let crate_name = unit.crate_name();
        let mut cfg = self.cfg(at);
        cfg.extend(ran.iter().flat_map(|it| it.output.cfgs.iter().cloned()));
        let proc_macro = match unit.is_proc_macro() {
            true => Some(lock(&self.made).macros.get(&at).cloned().with_context(|| {
                format!("the proc macro `{}` is not linked yet", unit.target.name)
            })?),
            false => None,
        };
        let described = Described {
            name: crate_name.clone(),
            root: unit.target.src_path.clone(),
            edition: unit.target.edition.clone(),
            cfg,
            env: self.crate_env(at, &crate_name, ran.as_deref())?,
            deps: Vec::new(),
            proc_macro,
            compiled_macro: false,
            inputs: Some((self.files_of(at)?, PathBuf::from("/"))),
        };
        let index = crates.len();
        crates.push(described);
        linked.insert(at, index);
        let deps = self.linked_deps(at, crates, linked)?;
        crates[index].deps = deps;
        Ok(index)
    }

    /// Each library `at` links, by the name it links it under, described in `crates`.
    fn linked_deps(
        &self,
        at: usize,
        crates: &mut Vec<Described>,
        linked: &mut FxHashMap<usize, usize>,
    ) -> anyhow::Result<Vec<(String, usize)>> {
        let mut deps = Vec::new();
        for dep in &self.unit(at).dependencies {
            if dep.index < self.graph.units.len() && self.unit(dep.index).is_linked() {
                let name = match dep.extern_crate_name.is_empty() {
                    true => self.unit(dep.index).crate_name(),
                    false => dep.extern_crate_name.clone(),
                };
                deps.push((name, self.describe(dep.index, crates, linked)?));
            }
        }
        Ok(deps)
    }

    /// A dep-info that names every Rust file of the unit's crate, as the wrapper writes for a
    /// library it only describes: a proc macro's library is kept in the store while they are
    /// what they were.
    fn files_of(&self, at: usize) -> anyhow::Result<PathBuf> {
        let unit = self.unit(at);
        let info =
            self.own.join("deps").join(format!("{}-{}.files.d", unit.crate_name(), self.keys[at]));
        let root = unit.target.src_path.parent().context("a crate root without a directory")?;
        let manifest = AsRef::<Path>::as_ref(self.package(at)?.manifest.parent()).to_owned();
        let mut files = vec![unit.target.src_path.clone()];
        super::load_wrapper::rust_files(root, Some(&manifest), &mut files)
            .with_context(|| format!("reading {}", root.display()))?;
        files.sort();
        files.dedup();
        let files: Vec<String> =
            files.iter().map(|it| crate::dep_info::escaped(&it.display().to_string())).collect();
        let text = format!(
            "{}: {}\n",
            crate::dep_info::escaped(&info.display().to_string()),
            files.join(" ")
        );
        if std::fs::read_to_string(&info).ok().as_deref() != Some(&text) {
            write(&info, text.as_bytes())?;
        }
        Ok(info)
    }

    /// Compiles the build script of unit `at`: where its executable is.
    fn script(&self, at: usize) -> anyhow::Result<PathBuf> {
        let unit = self.unit(at);
        let dir = self.unit_dir(at);
        let exe = dir.join(format!("build-script-build{}", std::env::consts::EXE_SUFFIX));
        let mut crates = vec![Described {
            name: SCRIPT.to_owned(),
            root: unit.target.src_path.clone(),
            edition: unit.target.edition.clone(),
            cfg: self.cfg(at),
            env: self.crate_env(at, SCRIPT, None)?,
            deps: Vec::new(),
            proc_macro: None,
            compiled_macro: false,
            inputs: None,
        }];
        let mut linked = FxHashMap::default();
        crates[0].deps = self.linked_deps(at, &mut crates, &mut linked)?;
        let made = Made {
            name: SCRIPT,
            proc_macro: false,
            wasi: false,
            wasm: false,
            dep_info: exe.with_extension("d"),
            exe: exe.clone(),
        };
        self.compile(&crates, &made, &dir.join("blaze"))?;
        Ok(exe)
    }

    /// Builds the proc macro of unit `at` as a WebAssembly module, or links it for blaze's
    /// server when the workspace expands it natively: where the module or library is.
    fn proc_macro(&self, at: usize) -> anyhow::Result<PathBuf> {
        let unit = self.unit(at);
        let name = unit.crate_name();
        let ran = self.own_run(at).map(|it| self.ran(it)).transpose()?;
        let mut cfg = self.cfg(at);
        cfg.extend(ran.iter().flat_map(|it| it.output.cfgs.iter().cloned()));
        let mut crates = vec![Described {
            name: name.clone(),
            root: unit.target.src_path.clone(),
            edition: unit.target.edition.clone(),
            cfg,
            env: self.crate_env(at, &name, ran.as_deref())?,
            deps: Vec::new(),
            proc_macro: None,
            compiled_macro: true,
            inputs: None,
        }];
        let mut linked = FxHashMap::default();
        linked.insert(at, 0);
        crates[0].deps = self.linked_deps(at, &mut crates, &mut linked)?;
        let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
        let deps = self.own.join("deps");
        let stem = format!("{name}-{}", self.keys[at]);
        let wasm = self.native.as_ref().is_some_and(|native| !native.contains(&name));
        if !wasm && super::own_macro::server().is_none() {
            anyhow::bail!("`{name}`: {}", super::own_macro::missing_server());
        }
        if wasm {
            self.link_for_wasm(at, &mut crates, &linked)?;
        }
        let made = Made {
            name: &name,
            proc_macro: true,
            wasi: false,
            wasm,
            exe: match wasm {
                true => deps.join(format!("{stem}.wasm")),
                false => deps.join(format!("{prefix}{stem}{suffix}")),
            },
            dep_info: deps.join(format!("{stem}.d")),
        };
        let mut compiled = self.compile(&crates, &made, &deps.join(&stem));
        // Compiled for this host from here, beside the other units, rather than in the load,
        // which loads one module after another.
        if wasm && compiled.is_ok() {
            compiled = blaze_macro_wasm::prepare_soon(&made.exe, own_script::store())
                .map_err(anyhow::Error::msg);
        }
        if let (true, Err(why)) = (wasm, &compiled) {
            let root: &Path = self.cargo.workspace_root().as_ref();
            anyhow::bail!(super::own_macro::refusal(&name, &format!("{why:#}"), root));
        }
        compiled?;
        Ok(made.exe)
    }

    /// Builds the first of `crates` as `made` says, describing them in `dir`, unless what it
    /// made last is of the same description and no file it was made from changed since.
    fn compile(&self, crates: &[Described], made: &Made<'_>, dir: &Path) -> anyhow::Result<()> {
        let described = own_script::description(self.toolchain, crates);
        let stamp = made.exe.with_extension("described");
        let mut digest = Sha256::new();
        digest.update(described.as_bytes());
        if made.wasm {
            digest.update(super::own_macro::MODULE_CFG.as_bytes());
        }
        let digest = format!("{:x}", digest.finalize());
        let inputs =
            crates.iter().filter_map(|it| it.inputs.as_ref().map(|(info, _)| info.as_path()));
        if fresh(&stamp, &digest, std::iter::once(made.dep_info.as_path()).chain(inputs)) {
            return Ok(());
        }
        let env = &crates[0].env;
        let rustc_had = |name: &str| env.get(name).cloned().or_else(|| std::env::var(name).ok());
        own_script::build_described(self.toolchain, crates, made, dir, &rustc_had)
            .map_err(|why| anyhow::anyhow!(why))?;
        write(&stamp, digest.as_bytes())
    }

    /// Runs the build script of unit `at`, a `run-custom-build`, as cargo runs it.
    fn run_script(&self, at: usize) -> anyhow::Result<Ran> {
        let unit = self.unit(at);
        let package = self.package(at)?;
        let compiled = self.deps(at).find(|&it| self.unit(it).is_script());
        let compiled = compiled.context("the unit graph has no compile of the build script")?;
        let exe = lock(&self.made).scripts.get(&compiled).cloned();
        let exe = exe.context("the build script is not compiled yet")?;
        let mut dependencies = Vec::new();
        for dep in self.deps(at).filter(|&it| self.unit(it).is_run()) {
            let ran = self.ran(dep)?;
            let of = self.package(dep)?;
            dependencies.push(script::unit::Dependency::of(
                &of.name,
                of.links.as_deref(),
                &ran.output,
            ));
        }
        let platform = self.platform(at);
        let profile = &unit.profile;
        let out_dir = self.unit_dir(at).join("out");
        let described = script::Unit {
            package: self.described_package(at)?,
            features: unit.features.clone(),
            profile: script::unit::Profile {
                name: profile.name.clone(),
                release: matches!(profile.name.as_str(), "release" | "bench"),
                opt_level: profile.opt_level.clone(),
                debuginfo: profile.debuginfo(),
                debug_assertions: profile.debug_assertions,
            },
            target: platform.triple.clone(),
            host: self.host.clone(),
            target_cfgs: platform.cfg.clone(),
            rustflags: platform.rustflags.iter().chain(profile.rustflags.iter()).cloned().collect(),
            jobs: self.jobs,
            script: exe.clone(),
            out_dir: out_dir.clone(),
            tools: script::unit::Tools { linker: platform.linker.clone(), ..self.tools.clone() },
            workspace_member: package.is_member,
            dependencies,
            config_env: self.config_env.clone(),
            dylib_search: script::unit::DylibSearch {
                // A check writes nothing to the artifact directory, and has none.
                host_root: None,
                host_deps: vec![self.own.join("deps")],
                sysroot_libdir: self.toolchain.join("lib/rustlib").join(&self.host).join("lib"),
                plugin_search: Vec::new(),
            },
            jobserver: None,
        };
        let env = script::env(&described, &self.inherited).map_err(|it| anyhow::anyhow!(it.0))?;
        // `blaze-engine build-script` runs it in the sandbox, or has a run the store kept stand for
        // it, as for the runs cargo starts (`script.rs`).
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("build-script");
        env.apply(&mut command);
        command.env(super::script::REAL, &exe).current_dir(package.manifest.parent());
        let root: &Path = self.cargo.workspace_root().as_ref();
        command.env(super::script_sandbox::ROOT, root);
        let ran = script::run::run(&described, command).map_err(|it| anyhow::anyhow!(it.0))?;
        let cx = script::output::Context::of(&described, &self.inherited);
        let output = ran.finish(&described, &cx).map_err(|failed| {
            let logged = failed.log_messages.iter().map(|(_, it)| format!("\n  {it}"));
            anyhow::anyhow!("{}{}", failed.error.0, logged.collect::<String>())
        })?;
        Ok(Ran { out_dir, output })
    }

    /// The package of unit `at` as a run of its script reads it.
    fn described_package(&self, at: usize) -> anyhow::Result<script::unit::Package> {
        let package = self.package(at)?;
        let cargo = self.cargo;
        let targets = package
            .targets
            .iter()
            .map(|&it| {
                let kind = match cargo[it].kind {
                    TargetKind::Lib { is_proc_macro: true } => "proc-macro",
                    TargetKind::Lib { is_proc_macro: false } => "lib",
                    TargetKind::Bin => "bin",
                    TargetKind::Example => "example",
                    TargetKind::Test => "test",
                    TargetKind::Bench => "bench",
                    TargetKind::BuildScript => "custom-build",
                    TargetKind::Other => "other",
                };
                script::unit::Target {
                    name: cargo[it].name.clone(),
                    kind: vec![kind.to_owned()],
                    crate_types: vec![if kind == "lib" { "lib" } else { kind }.to_owned()],
                }
            })
            .collect();
        let text = |it: &Option<paths::Utf8PathBuf>| it.as_ref().map(ToString::to_string);
        Ok(script::unit::Package {
            name: package.name.clone(),
            version: package.version.to_string(),
            descr: None,
            manifest_dir: AsRef::<Path>::as_ref(package.manifest.parent()).to_owned(),
            manifest_path: AsRef::<Path>::as_ref(&package.manifest).to_owned(),
            authors: package.authors.clone(),
            description: package.description.clone(),
            homepage: package.homepage.clone(),
            repository: package.repository.clone(),
            license: package.license.clone(),
            license_file: text(&package.license_file),
            readme: text(&package.readme),
            rust_version: package.rust_version_as_written(),
            links: package.links.clone(),
            targets,
        })
    }

    /// What the load reads of each package: what its script printed and where it wrote, for
    /// the run the program is built with when the package's script runs for the host too, and
    /// its proc macro's library.
    fn outputs(&self, _: &()) -> Vec<(Package, OwnBuildOutput)> {
        let made = lock(&self.made);
        let for_target = self.graph.for_target();
        let mut chosen: FxHashMap<Package, (bool, OwnBuildOutput)> = FxHashMap::default();
        for (&at, ran) in &made.runs {
            let Some(package) = self.packages[at] else { continue };
            // A run is the program's when a library built into it is compiled with it.
            let ours = for_target.iter().any(|&it| self.deps(it).any(|dep| dep == at));
            if chosen.get(&package).is_some_and(|(theirs, _)| *theirs || !ours) {
                continue;
            }
            let out = OwnBuildOutput {
                cfgs: ran.output.cfgs.clone(),
                envs: ran.output.env.clone(),
                out_dir: Some(paths::AbsPathBuf::assert_utf8(ran.out_dir.clone())),
                proc_macro_dylib: None,
            };
            chosen.insert(package, (ours, out));
        }
        for (&at, library) in &made.macros {
            let Some(package) = self.packages[at] else { continue };
            let entry = chosen.entry(package).or_default();
            entry.1.proc_macro_dylib = Some(paths::AbsPathBuf::assert_utf8(library.clone()));
        }
        chosen.into_iter().map(|(package, (_, out))| (package, out)).collect()
    }
}

/// What rustc says to `args`, as blaze answers it for its release: no rustc runs, and a
/// question blaze has no answer to fails the load.
fn said(args: &[&str]) -> anyhow::Result<String> {
    crate::answers::ours_said(args)
}

/// The host blaze's rustc builds for.
fn host_of() -> anyhow::Result<String> {
    let told = said(&["-vV"])?;
    let host = told.lines().find_map(|it| it.strip_prefix("host: "));
    Ok(host.context("`rustc -vV` names no host")?.trim().to_owned())
}

/// `value` as a string literal of a cfg.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The cfgs of `rustc --print cfg`'s lines.
fn cfgs(printed: &str) -> Vec<cargo_platform::Cfg> {
    printed.lines().filter_map(|it| it.parse().ok()).collect()
}

/// The cfgs of `rustc --print cfg`'s lines, as rust-analyzer has them.
fn atoms(printed: &str) -> Vec<cfg::CfgAtom> {
    let symbol = intern::Symbol::intern;
    let atom = |line: &str| match line.split_once('=') {
        Some((key, value)) => {
            cfg::CfgAtom::KeyValue { key: symbol(key), value: symbol(value.trim_matches('"')) }
        }
        None => cfg::CfgAtom::Flag(symbol(line)),
    };
    printed.lines().filter(|it| !it.is_empty()).map(atom).collect()
}

/// The `--cfg`s rustc compiles `unit` with beside the target's, for a platform whose units
/// cargo passes `rustflags`: its features, `debug_assertions` as its profile has it, and
/// those of the flags.
fn unit_cfg(unit: &Unit, rustflags: &[String]) -> Vec<String> {
    let mut cfg: Vec<String> =
        unit.features.iter().map(|it| format!("feature={}", quoted(it))).collect();
    if unit.profile.debug_assertions {
        cfg.push("debug_assertions".to_owned());
    }
    let mut flags = rustflags.iter().chain(unit.profile.rustflags.iter()).map(String::as_str);
    while let Some(flag) = flags.next() {
        if flag == "--cfg" {
            cfg.extend(flags.next().map(str::to_owned));
        } else if let Some(it) = flag.strip_prefix("--cfg=") {
            cfg.push(it.to_owned());
        }
    }
    cfg
}

/// The key of each unit of `graph`: what it is and the keys of what it depends on.
fn keys(graph: &UnitGraph) -> Vec<String> {
    fn key(graph: &UnitGraph, at: usize, keys: &mut Vec<Option<String>>) -> String {
        if let Some(it) = &keys[at] {
            return it.clone();
        }
        let unit = &graph.units[at];
        let mut hash = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        };
        field(unit.pkg_id.as_bytes());
        field(unit.target.name.as_bytes());
        field(unit.target.kind.join(",").as_bytes());
        field(unit.mode.as_bytes());
        field(unit.features.join(",").as_bytes());
        field(format!("{:?}", unit.profile).as_bytes());
        field(unit.platform.as_deref().unwrap_or("").as_bytes());
        let deps: Vec<(String, usize)> = unit
            .dependencies
            .iter()
            .filter(|it| it.index < graph.units.len())
            .map(|it| (it.extern_crate_name.clone(), it.index))
            .collect();
        let mut deps: Vec<(String, String)> =
            deps.into_iter().map(|(name, it)| (name, key(graph, it, keys))).collect();
        deps.sort();
        for (name, key) in deps {
            hash.update(name.as_bytes());
            hash.update(key.as_bytes());
        }
        let key: String = format!("{:x}", hash.finalize())[..16].to_owned();
        keys[at] = Some(key.clone());
        key
    }
    let mut keys = vec![None; graph.units.len()];
    (0..graph.units.len()).map(|at| key(graph, at, &mut keys)).collect()
}

/// Whether what `stamp` was written beside was made from what `digest` names, and no file the
/// dep-infos `infos` name is newer than it.
fn fresh<'a>(stamp: &Path, digest: &str, infos: impl Iterator<Item = &'a Path>) -> bool {
    let Ok(written) = std::fs::read_to_string(stamp) else { return false };
    let Ok(when) = std::fs::metadata(stamp).and_then(|it| it.modified()) else { return false };
    if written != digest {
        return false;
    }
    let newer = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|it| it.modified())
            .map_or(true, |it: SystemTime| it > when)
    };
    for info in infos {
        let Ok(text) = std::fs::read_to_string(info) else { return false };
        let from = info.parent().unwrap_or(Path::new("/"));
        let parsed = crate::dep_info::DepInfo::parse(&text);
        if parsed.files.iter().any(|file| newer(&from.join(file))) {
            return false;
        }
    }
    true
}

fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("a file at the root")?;
    std::fs::create_dir_all(parent).with_context(|| format!("making {}", parent.display()))?;
    // The workers write one library's files at once when two steps describe it.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nth = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let new = path.with_extension(format!("{}-{nth}.new", std::process::id()));
    std::fs::write(&new, bytes)
        .and_then(|()| std::fs::rename(&new, path))
        .with_context(|| format!("writing {}", path.display()))
}
