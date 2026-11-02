//! Each unit's plan: what the pinned nightly's cargo (`cad07cf8e597397fff5d0f6cb79c9f870c0b1dd2`)
//! hashes, passes rustc, sets and writes for it, computed as cargo computes it. Nothing here
//! reads a file but `cargo clippy`'s configuration files, through the config's view: the
//! manifests, the config and the standard library's facts come in.

mod env;
mod flags;
mod hash;
mod outputs;
#[cfg(test)]
mod tests;

use std::collections::hash_map::Entry;
use std::path::Path;

use blaze_manifest::Config;
use blaze_manifest::files::Files;
use blaze_manifest::config::CompileKind;
use blaze_manifest::manifest::Manifest;
use blaze_manifest::profiles::{Profile as BaseProfile, ProfileRoot};
use blaze_manifest::targets::TargetKind;
use blaze_resolve::PackageId;
use blaze_hash::{HashMap, HashSet};

use self::flags::KindInfo;
use self::hash::{CompileMode, HashedProfile, LtoUnit, Meta, MetaInput, RustcVersion, TrimPaths};
use self::outputs::{CompileFiles, Layout};
use crate::load::load_error;
use crate::query::{Clippy, LoadError, UnitPlan};
use crate::schema::Unit;
use crate::stdlib::target as platform;
use crate::{Intent, Package};

fn error(what: String) -> LoadError {
    LoadError { what, at: None }
}

pub(crate) struct PlanInput<'a, F: Files> {
    pub units: &'a [Unit],
    /// The standard library's units' plans, made by its own load; `None` for every other unit.
    pub std_plans: &'a [Option<UnitPlan>],
    /// Every package of the units, by the unit's `pkg_id`.
    pub packages: &'a HashMap<String, (PackageId, Package<'a>)>,
    /// Every package's manifest path, by its id.
    pub manifests: &'a HashMap<PackageId, (&'a Path, &'a Manifest)>,
    /// The graph's roots, as indexes of `units`.
    pub roots: &'a [usize],
    pub config: &'a Config<F>,
    pub intent: Intent,
    /// `check`'s: the workspace's crates linted, each given `--cfg clippy` and the
    /// configuration file clippy finds for it.
    pub lints: bool,
    /// Whether the standard library is built (`-Zbuild-std`): only then do its units' hashes
    /// go into their dependents'.
    pub build_std: bool,
    pub ws_root: &'a Path,
    pub ws_manifest: &'a Path,
    /// The root manifest's `[workspace.metadata]`, whose blaze lints every member gets.
    pub ws_metadata: Option<&'a toml::Value>,
    /// The sysroot, which the standard library's packages are hashed against.
    pub sysroot: Option<&'a Path>,
    pub target_dir: &'a Path,
    pub build_dir: &'a Path,
    pub host: &'a str,
    pub target: Option<&'a str>,
    /// The profile asked for, by name (`dev`), and its directory's name (`debug`).
    pub requested_profile: &'a str,
    pub profile_dir: &'a str,
    /// `Profiles::base_profile`: the requested profile, for its root and the `Finished` text.
    pub base_profile: &'a BaseProfile,
    pub jobs: Option<u32>,
    /// The packages, by the unit's `pkg_id`, whose build scripts run outside the sandbox
    /// (`UnitPlan::unsandboxed`).
    pub unsandboxed: &'a HashSet<&'a str>,
}

pub(crate) struct PlanOutput {
    /// Of each unit, in the units' order; `None` for one planned elsewhere, whose plan is its
    /// `std_plans`' (moved in by the caller, which owns them).
    pub plans: Vec<Option<UnitPlan>>,
    /// What cargo prints after `Finished`: "`dev` profile [unoptimized + debuginfo]".
    pub finished: String,
}

/// The rustflags of every unit for `triple` in a build for `target`.
pub(crate) fn platform_rustflags<F: Files>(config: &Config<F>, triple: &str, host: &str, target: Option<&str>) -> Result<Vec<String>, LoadError> {
    let kind = match target {
        Some(target) if target == triple => Some(triple),
        _ if triple == host => None,
        _ => return Err(error(format!("a build for {target:?} on `{host}` has no units for `{triple}`"))),
    };
    Ok(flags::kind_info(config, host, target, kind)?.rustflags)
}

/// cargo's `Finished` text of the profile asked for as `requested`.
fn finished(requested: &str, opt_level: &str, debuginfo: bool) -> String {
    let opt = if opt_level == "0" { "unoptimized" } else { "optimized" };
    let debug = if debuginfo { " + debuginfo" } else { "" };
    format!("`{requested}` profile [{opt}{debug}]")
}

pub(crate) fn plan<F: Files>(input: &PlanInput<'_, F>) -> Result<PlanOutput, LoadError> {
    plan_units(input).map_err(|mut error| {
        if error.at.is_none() {
            error.at = Some(input.ws_manifest.display().to_string());
        }
        error
    })
}

/// A unit's package as the plan uses it.
struct Pkg<'a> {
    id: PackageId,
    manifest: &'a Manifest,
    manifest_path: &'a Path,
    is_member: bool,
}

/// What every unit planned here has worked out before its hashes.
struct Facts<'a> {
    unit: &'a Unit,
    pkg: Pkg<'a>,
    /// 0 the host, 1 the target.
    kind: usize,
    target_kind: TargetKind,
    mode: CompileMode,
    profile: HashedProfile<'a>,
}

/// Each unit's dependencies in one column: unit `at`'s are
/// `edges[starts[at] as usize..starts[at + 1] as usize]`, 4 bytes an edge and 4 a unit.
struct Csr {
    starts: Vec<u32>,
    edges: Vec<u32>,
}

impl Csr {
    fn with_capacity(units: usize, edges: usize) -> Csr {
        let mut starts = Vec::with_capacity(units + 1);
        starts.push(0);
        Csr { starts, edges: Vec::with_capacity(edges) }
    }

    /// Ends the unit whose edges were pushed since the last.
    fn close(&mut self) {
        self.starts.push(self.edges.len() as u32);
    }

    fn of(&self, at: usize) -> &[u32] {
        &self.edges[self.starts[at] as usize..self.starts[at + 1] as usize]
    }
}

/// What every unit of a package planned here shares, made once per package.
struct PackageFlags {
    env: Vec<(String, String)>,
    /// `[lints]`' flags: a local package's only.
    lints: Vec<String>,
    /// What follows each `--check-cfg`: the features', then `[lints]`'.
    check_cfg: Vec<String>,
}

/// What is the same for every unit.
struct Shared<'a, F: Files> {
    input: &'a PlanInput<'a, F>,
    kinds: Vec<KindInfo>,
    kind_hashes: Vec<u64>,
    rustc: RustcVersion,
    default_lib_metadata: Option<&'a str>,
    new_layout: bool,
}

/// cargo's `should_use_metadata` (`use_extra_filename`) and `use_pkg_dir`, of a unit.
fn use_metadata<F: Files>(shared: &Shared<'_, F>, facts: &Facts<'_>) -> (bool, bool) {
    let mode = facts.mode;
    if mode.is_any_test() || mode.is_check() {
        return (true, true);
    }
    let kind = &facts.target_kind;
    if shared.new_layout {
        if kind.is_custom_build() {
            return (false, true);
        }
        if (kind.is_dylib() || kind.is_cdylib() || kind.is_executable()) && shared.default_lib_metadata.is_none() {
            return (false, true);
        }
        return (true, true);
    }
    let triple = shared.kinds[facts.kind].triple.as_str();
    let exe = kind.is_executable();
    let named = kind.is_dylib() || kind.is_cdylib() || (exe && triple == "wasm32-unknown-emscripten") || (exe && triple.contains("msvc"));
    if named && facts.pkg.id.source_id().is_path() && shared.default_lib_metadata.is_none() {
        return (false, false);
    }
    (true, true)
}

/// The target of the manifest a unit builds, for its `bin_name` and its kind.
fn manifest_target<'a>(manifest: &'a Manifest, kind: &TargetKind, name: &str) -> Result<&'a blaze_manifest::targets::Target, LoadError> {
    let same = |theirs: &TargetKind| match (kind, theirs) {
        (TargetKind::Lib(_), TargetKind::Lib(_))
        | (TargetKind::Bin, TargetKind::Bin)
        | (TargetKind::Test, TargetKind::Test)
        | (TargetKind::Bench, TargetKind::Bench)
        | (TargetKind::ExampleLib(_) | TargetKind::ExampleBin, TargetKind::ExampleLib(_) | TargetKind::ExampleBin)
        | (TargetKind::CustomBuild, TargetKind::CustomBuild) => true,
        _ => false,
    };
    manifest
        .targets
        .iter()
        .find(|it| it.name == name && same(&it.kind))
        .ok_or_else(|| error(format!("`{}` has no {kind:?} target `{name}`", manifest.package_id.name)))
}

/// The configuration file clippy's driver finds for the package of `manifest_path`, with its
/// text (`clippy_config::lookup_conf_file`): `.clippy.toml`, then `clippy.toml`, in
/// `CLIPPY_CONF_DIR` (from the workspace's root, where cargo runs it, when relative) or the
/// package's directory, then in each directory above.
fn clippy_conf<F: Files>(input: &PlanInput<'_, F>, manifest_path: &Path) -> Result<Option<(String, String)>, LoadError> {
    const NAMES: [&str; 2] = [".clippy.toml", "clippy.toml"];
    let files = input.config.files();
    let start = match input.config.env("CLIPPY_CONF_DIR") {
        Some(dir) => input.ws_root.join(dir),
        None => manifest_path.parent().unwrap_or(manifest_path).to_owned(),
    };
    let start = files
        .canonicalize(&start)
        .map_err(|why| error(format!("clippy's configuration is looked for from `{}`: {why}", start.display())))?;
    for dir in start.ancestors() {
        for name in NAMES {
            let path = dir.join(name);
            if !files.is_file(&path) {
                continue;
            }
            let shown = path.display().to_string();
            let bytes = files.read(&path).map_err(|why| error(format!("error reading `{shown}`: {why}")))?;
            let text = String::from_utf8(bytes.into_owned())
                .map_err(|_| error(format!("error reading `{shown}`: stream did not contain valid UTF-8")))?;
            return Ok(Some((shown, text)));
        }
    }
    Ok(None)
}

fn plan_units<F: Files>(input: &PlanInput<'_, F>) -> Result<PlanOutput, LoadError> {
    let units = input.units;
    if input.std_plans.len() != units.len() {
        return Err(error(format!("{} standard library plans for {} units", input.std_plans.len(), units.len())));
    }
    if let Some(root) = input.roots.iter().find(|&&at| at >= units.len()) {
        return Err(error(format!("the unit graph's root {root} is no unit")));
    }
    let config = input.config;
    let workspace_lints = flags::workspace_lints(input.ws_metadata, input.ws_manifest)?;

    let wrapper = match config.env("RUSTC_WORKSPACE_WRAPPER") {
        Some(it) => !it.is_empty(),
        None => config.build_config().map_err(|e| load_error(&e, None))?.rustc_workspace_wrapper.is_some(),
    };
    if wrapper && input.packages.values().any(|(_, package)| package.is_member) {
        return Err(error("blaze runs no rustc wrapper: `RUSTC_WORKSPACE_WRAPPER` (`build.rustc-workspace-wrapper`) is set".to_owned()));
    }

    let mut kinds = vec![flags::kind_info(config, input.host, input.target, None)?];
    let mut kind_hashes = vec![hash::kind_fingerprint_hash(None)?];
    let mut layouts = vec![Layout::new(input.target_dir, input.build_dir, None, input.profile_dir)];
    if let Some(target) = input.target {
        kinds.push(flags::kind_info(config, input.host, input.target, Some(target))?);
        kind_hashes.push(hash::kind_fingerprint_hash(Some(target))?);
        layouts.push(Layout::new(input.target_dir, input.build_dir, Some(target), input.profile_dir));
    }
    if let Intent::Doc { open } = input.intent {
        flags::doc_unhonored(config, input.host, input.target, &kinds, open)?;
    }
    let mut linkers = Vec::with_capacity(kinds.len());
    let mut runners = Vec::with_capacity(kinds.len());
    for (at, info) in kinds.iter().enumerate() {
        let kind = if at == 0 { CompileKind::Host } else { CompileKind::Target(info.triple.clone()) };
        let linker = config.target_linker(&kind, input.host, &info.cfgs).map_err(|e| load_error(&e, None))?;
        linkers.push(linker.map(|it| it.display().to_string()));
        let runner = config.target_runner(&kind, input.host, &info.cfgs).map_err(|e| load_error(&e, None))?;
        runners.push(match runner {
            Some((program, args)) => std::iter::once(program.display().to_string()).chain(args).collect(),
            None => Vec::new(),
        });
    }

    let mut config_env = Vec::new();
    for (name, value) in config.env_config().map_err(|e| load_error(&e, None))? {
        match value.into_string() {
            Ok(value) => config_env.push((name, value)),
            Err(_) => return Err(error(format!("the `[env]` variable `{name}` is not UTF-8"))),
        }
    }

    let shared = Shared {
        input,
        kinds,
        kind_hashes,
        rustc: RustcVersion::of(platform::rustc_version(), || platform::rustc_verbose_version(input.host))?,
        default_lib_metadata: config.env("__CARGO_DEFAULT_LIB_METADATA"),
        new_layout: config.env("__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT") != Some("1"),
    };
    let stop_rlib_uplift = shared.rustc.is_nightly() && config.env("__CARGO_TEMPORARY_STOP_RLIB_UPLIFT_OPT_OUT") != Some("1");

    // What each unit planned here is, and the dependencies its hashes take.
    if u32::try_from(units.len()).is_err() {
        return Err(error(format!("{} units are more than a unit's index holds", units.len())));
    }
    let mut facts: Vec<Option<Facts<'_>>> = Vec::with_capacity(units.len());
    let mut deps = Csr::with_capacity(units.len(), units.iter().map(|it| it.dependencies.len()).sum());
    for (at, unit) in units.iter().enumerate() {
        for dep in &unit.dependencies {
            let to = units
                .get(dep.index)
                .ok_or_else(|| error(format!("unit {at} depends on unit {}, which there is not", dep.index)))?;
            if !to.is_std || unit.is_std || input.build_std {
                deps.edges.push(dep.index as u32);
            }
        }
        deps.close();
        if input.std_plans[at].is_some() {
            facts.push(None);
            continue;
        }
        let (id, package) = input
            .packages
            .get(&unit.pkg_id)
            .ok_or_else(|| error(format!("the unit graph's package `{}` is not in the resolve", unit.pkg_id)))?;
        let (manifest_path, manifest) = *input
            .manifests
            .get(id)
            .ok_or_else(|| error(format!("the package `{}` has no manifest", unit.pkg_id)))?;
        let kind = match unit.platform.as_deref() {
            None => 0,
            Some(triple) if Some(triple) == input.target => 1,
            Some(triple) => {
                return Err(error(format!(
                    "the unit graph has a unit of `{}` for `{triple}` in a build for {:?}",
                    unit.pkg_id, input.target
                )));
            }
        };
        let profile = HashedProfile::of(&unit.profile)?;
        if profile.trim_paths != TrimPaths::None {
            return Err(error(format!("blaze does not trim paths (`trim-paths` of `{}`'s profile)", unit.pkg_id)));
        }
        facts.push(Some(Facts {
            unit,
            pkg: Pkg { id: *id, manifest, manifest_path, is_member: package.is_member },
            kind,
            target_kind: hash::target_kind(&unit.target),
            mode: CompileMode::of(unit)?,
            profile,
        }));
    }

    // `lto::generate`, over the units planned here.
    let mut lto_deps = Csr::with_capacity(units.len(), deps.edges.len());
    for at in 0..units.len() {
        lto_deps.edges.extend(deps.of(at).iter().copied().filter(|&to| facts[to as usize].is_some()));
        lto_deps.close();
    }
    let mut lto_units: Vec<Option<LtoUnit<'_>>> = Vec::with_capacity(units.len());
    for (at, it) in facts.iter().enumerate() {
        lto_units.push(it.as_ref().map(|it| LtoUnit {
            kind: &it.target_kind,
            mode: it.mode,
            lto: &it.profile.lto,
            deps: lto_deps.of(at),
        }));
    }
    let ltos = hash::generate(&lto_units, input.roots);

    // `compute_metadata`, each unit after its dependencies.
    let mut metas: Vec<Option<Meta>> = vec![None; units.len()];
    for at in 0..units.len() {
        meta_of(&shared, &facts, &deps, &ltos, &mut metas, at, 0)?;
    }
    let metas: Vec<Meta> = metas.into_iter().map(|it| it.expect("every unit's metadata is computed")).collect();

    // cargo's `pkg_dir` of each unit planned here.
    let mut pkg_dirs: Vec<String> = Vec::with_capacity(units.len());
    for (at, it) in facts.iter().enumerate() {
        let Some(it) = it else {
            pkg_dirs.push(String::new());
            continue;
        };
        let hash = if metas[at].pkg_dir {
            format!("{:016x}", metas[at].unit_id)
        } else {
            hash::target_short_hash(it.pkg.id, input.ws_root)?
        };
        let separator = if shared.new_layout { '/' } else { '-' };
        pkg_dirs.push(format!("{}{separator}{hash}", it.pkg.id.name()));
    }
    let run_out_dirs: Vec<Option<String>> = facts
        .iter()
        .zip(&pkg_dirs)
        .map(|(it, pkg_dir)| match it {
            Some(it) if it.mode == CompileMode::RunCustomBuild => {
                Some(outputs::run_files(&layouts[it.kind], shared.new_layout, pkg_dir).0)
            }
            _ => None,
        })
        .collect();

    let mut roots = vec![false; units.len()];
    for &at in input.roots {
        roots[at] = true;
    }
    let primary: HashSet<&str> = input.roots.iter().map(|&at| units[at].pkg_id.as_str()).collect();
    let mut per_package: HashMap<&str, PackageFlags> =
        HashMap::with_capacity_and_hasher(input.packages.len(), Default::default());
    let artifact_dir = !matches!(input.intent, Intent::Check { .. } | Intent::Doc { .. });

    let mut plans = Vec::with_capacity(units.len());
    for (at, it) in facts.iter().enumerate() {
        let Some(it) = it else {
            plans.push(None);
            continue;
        };
        let unit = it.unit;
        let meta = &metas[at];
        let info = &shared.kinds[it.kind];
        let layout = &layouts[it.kind];
        let package = match per_package.entry(unit.pkg_id.as_str()) {
            Entry::Occupied(found) => found.into_mut(),
            Entry::Vacant(slot) => {
                let env = env::package_env(it.pkg.id, it.pkg.manifest, it.pkg.manifest_path)?;
                // A package's units are all the standard library's or all not.
                let (mut lints, lint_check_cfg) = match it.pkg.id.source_id().is_path() && !unit.is_std {
                    true => flags::lints(it.pkg.manifest)?,
                    false => (Vec::new(), Vec::new()),
                };
                if it.pkg.is_member && !unit.is_std {
                    lints.splice(0..0, workspace_lints.iter().cloned());
                }
                let mut check_cfg = flags::check_cfg(it.pkg.manifest);
                check_cfg.extend(lint_check_cfg);
                slot.insert(PackageFlags { env, lints, check_cfg })
            }
        };
        let package_env = &package.env;
        let links = it.pkg.manifest.metadata.links.clone();
        let unsandboxed = input.unsandboxed.contains(unit.pkg_id.as_str());
        let target = manifest_target(it.pkg.manifest, &it.target_kind, &unit.target.name)?;

        if it.mode == CompileMode::RunCustomBuild {
            let (out_dir, files) = outputs::run_files(layout, shared.new_layout, &pkg_dirs[at]);
            let profile_root = match input.base_profile.root {
                ProfileRoot::Release => "release",
                ProfileRoot::Debug => "debug",
            };
            let env = env::run_env(
                env::RunEnv {
                    package: package_env,
                    out_dir: &out_dir,
                    jobs: input.jobs,
                    target: &info.triple,
                    host: input.host,
                    debug: it.profile.debuginfo.is_turned_on(),
                    opt_level: &unit.profile.opt_level,
                    profile: profile_root,
                    linker: linkers[it.kind].as_deref(),
                    links: links.as_deref(),
                    features: &unit.features,
                    debug_assertions: unit.profile.debug_assertions,
                    cfg_lines: &info.cfg_lines,
                    rustflags: &info.rustflags,
                },
                &config_env,
            );
            plans.push(Some(UnitPlan {
                checked_as_test: unit.checked_as_test,
                metadata: meta.metadata(),
                extra_filename: meta.extra_filename(),
                unit_id: meta.unit_id(),
                cfgs: Vec::new(),
                check_cfg: Vec::new(),
                rustflags: Vec::new(),
                lints: Vec::new(),
                env,
                outputs: files,
                out_dir: Some(out_dir),
                links,
                unsandboxed,
                runner: Vec::new(),
                harness: target.harness,
                clippy: None,
            }));
            continue;
        }

        let crate_name = unit.target.name.replace('-', "_");
        // clone: each plan owns its flags (`UnitPlan`), made once per package.
        let (lints, check_cfg) = (package.lints.clone(), package.check_cfg.clone());

        let out_dir = unit
            .dependencies
            .iter()
            .filter_map(|dep| run_out_dirs[dep.index].as_deref())
            .next_back();
        let test_or_bench = it.target_kind.is_test_or_bench();
        let mut bin_exes = Vec::new();
        if test_or_bench {
            for bin in it.pkg.manifest.targets.iter().filter(|it| it.is_bin()) {
                let name = bin.bin_name.as_deref().unwrap_or(&bin.name);
                let path = if artifact_dir {
                    outputs::bin_link(layout, &info.triple, &bin.name, bin.bin_name.as_deref())?.display().to_string()
                } else {
                    format!("placeholder:{name}")
                };
                bin_exes.push((format!("CARGO_BIN_EXE_{name}"), path));
            }
        }
        let env = env::compile_env(
            env::CompileEnv {
                package: package_env,
                crate_name: &crate_name,
                bin_name: it.target_kind.is_executable().then(|| target.bin_name.as_deref().unwrap_or(&target.name)),
                primary: primary.contains(unit.pkg_id.as_str()),
                tmpdir: test_or_bench.then(|| layout.tmp().display().to_string()),
                out_dir,
                bin_exes,
            },
            &config_env,
        );

        let extra = meta.extra_hex();
        let files = outputs::compile_outputs(&CompileFiles {
            mode: it.mode,
            kind: &it.target_kind,
            name: &unit.target.name,
            crate_name: &crate_name,
            bin_name: target.bin_name.as_deref(),
            package: it.pkg.id.name().as_str(),
            triple: &info.triple,
            extra: extra.as_deref(),
            pkg_dir: &pkg_dirs[at],
            layout,
            host_layout: &layouts[0],
            new_layout: shared.new_layout,
            artifact_dir,
            stop_rlib_uplift,
            is_root: roots[at],
        })?;

        let name = it.pkg.id.name();
        let rustflags = flags::unit_rustflags(&unit.profile.rustflags, &unit.profile.panic, name.as_str(), &info.rustflags)?;
        let mut cfgs = flags::cfgs(&unit.profile.rustflags, &unit.features, &info.rustflags);
        // As cargo-clippy wraps the workspace's rustc (`RUSTC_WORKSPACE_WRAPPER`): the
        // workspace's crates, not the registry's.
        let clippy = match input.lints && it.pkg.is_member && !unit.is_std {
            true => {
                cfgs.push("clippy".to_owned());
                Some(Clippy { conf: clippy_conf(input, it.pkg.manifest_path)? })
            }
            false => None,
        };
        plans.push(Some(UnitPlan {
            checked_as_test: unit.checked_as_test,
            metadata: meta.metadata(),
            extra_filename: meta.extra_filename(),
            unit_id: meta.unit_id(),
            cfgs,
            check_cfg,
            rustflags,
            lints,
            env,
            outputs: files,
            out_dir: None,
            links,
            unsandboxed,
            runner: match it.mode {
                CompileMode::Test => runners[it.kind].clone(),
                CompileMode::Build if it.target_kind.is_executable() => runners[it.kind].clone(),
                _ => Vec::new(),
            },
            harness: target.harness,
            clippy,
        }));
    }

    let base = input.base_profile;
    Ok(PlanOutput { plans, finished: finished(input.requested_profile, &base.opt_level, base.debuginfo.is_turned_on()) })
}

/// The unit's `Metadata`, its dependencies' first.
fn meta_of<F: Files>(
    shared: &Shared<'_, F>,
    facts: &[Option<Facts<'_>>],
    deps: &Csr,
    ltos: &[Option<hash::Lto>],
    metas: &mut [Option<Meta>],
    at: usize,
    depth: usize,
) -> Result<Meta, LoadError> {
    if let Some(meta) = metas[at] {
        return Ok(meta);
    }
    let input = shared.input;
    let unit = &input.units[at];
    if depth > input.units.len() {
        return Err(error(format!("the unit graph has a cycle through `{}`'s `{}`", unit.pkg_id, unit.target.name)));
    }
    let meta = match &facts[at] {
        None => {
            let plan = input.std_plans[at].as_ref().expect("a unit planned elsewhere has its plan");
            let what = format!("the standard library's unit of `{}`", unit.pkg_id);
            Meta::read(&plan.metadata, &plan.unit_id, &plan.extra_filename, &what)?
        }
        Some(it) => {
            let mut dep_metas = Vec::with_capacity(deps.of(at).len());
            for &dep in deps.of(at) {
                dep_metas.push(meta_of(shared, facts, deps, ltos, metas, dep as usize, depth + 1)?);
            }
            let lto = ltos[at].as_ref().ok_or_else(|| {
                error(format!("`{}`'s `{}` is reached from no root of the unit graph", unit.pkg_id, unit.target.name))
            })?;
            let root = if unit.is_std {
                input.sysroot.ok_or_else(|| error(format!("`{}` is a standard library unit, and no sysroot was given", unit.pkg_id)))?
            } else {
                input.ws_root
            };
            let (c_extra_filename, pkg_dir) = use_metadata(shared, it);
            hash::compute_metadata(
                &MetaInput {
                    package: it.pkg.id,
                    root,
                    features: &unit.features,
                    profile: &it.profile,
                    mode: it.mode,
                    lto,
                    kind_hash: shared.kind_hashes[it.kind],
                    host_unit: it.kind == 0,
                    target_name: &unit.target.name,
                    target_kind: &it.target_kind,
                    rustc: &shared.rustc,
                    host: input.host,
                    default_lib_metadata: shared.default_lib_metadata,
                    is_std: unit.is_std,
                    rustflags: &shared.kinds[it.kind].rustflags,
                    c_extra_filename,
                    pkg_dir,
                },
                &dep_metas,
            )?
        }
    };
    metas[at] = Some(meta);
    Ok(meta)
}
