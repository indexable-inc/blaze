//! The resolve made into units (blaze-metadata's `units`, then what `-Zbuild-std` adds): the
//! profiles, the feature resolve, the unit graph ([`crate::unit_graph`]), the standard
//! library's units merged in as cargo's `attach_std_deps` merges them, and each unit's plan
//! ([`super::plan`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use blaze_hash::{HashMap, HashSet};
use blaze_manifest::manifest::Manifest;
use blaze_manifest::profiles::{Profiles, requested_profile};
use blaze_manifest::targets::TargetKind;
use blaze_manifest::{Context, Workspace};
use blaze_query::Cancelled;
use blaze_resolve::resolver::{
    CompileKind, FeatureOpts, FeatureResolver, ForceAllTargets, HasDevUnits, PackageTargets,
};
use blaze_resolve::{Cfg, InternedString, PackageId, Platform};
use cargo_util_schemas::core::PartialVersion;
use cargo_util_schemas::manifest::{ProfileName, RustVersion, TomlProfile, TomlProfiles};

use super::files::Snap;
use super::manifest::WorkspaceRead;
use super::resolve::Resolved;
use super::sandbox::PackageOptOut;
use super::{ForeignAsk, Spec, Staged, load_error};
use crate::Read;
use crate::query::{
    LoadError, LockWrite, PackagePlan, StdGraph, StdKey, StdPlanned, StdUnits, TestSandbox,
    UnitPlan,
};
use crate::schema::{self, Mode, Unit, UnitDep};
use crate::stdlib::target::{self as platform, CfgArgs};
use crate::stdlib::{STD_OUT, default_crates, default_features};
use crate::{Filter, Intent, LibRule, Package, Platformed, Request, Rule};

/// A build's units as the load made them: [`crate::query::Units`] less what only the view's
/// build has, or a [`StdGraph`]'s.
pub(crate) struct Built {
    pub graph: schema::UnitGraph,
    pub plans: Vec<UnitPlan>,
    pub packages: Vec<PackagePlan>,
    pub lock: Option<LockWrite>,
    pub warnings: Vec<String>,
    pub target_dir: String,
    pub build_dir: String,
    pub finished: String,
    /// A foreign workspace's roots by name, with their units' indexes.
    pub roots: Vec<(String, usize)>,
    /// The workspace's `build-script-link-dirs`, absolute or `${NAME}/rest` for a run to expand.
    pub link_dirs: Vec<String>,
    /// The workspace's `sandboxed-tests` experiment ([`crate::query::Units::sandboxed_tests`]).
    pub sandboxed_tests: bool,
}

/// The units of `resolved`, as `spec` asks for them.
pub(crate) fn units(
    ctx: &Context<Snap<'_, '_>>,
    ws: &Workspace<'_, Snap<'_, '_>>,
    spec: &Spec,
    read: &WorkspaceRead,
    resolved: Resolved<'_>,
) -> Result<Staged<Built>, Cancelled> {
    let at = Some(ws.root_manifest().display().to_string());
    let fail = |error: anyhow::Error| Staged::Failed(load_error(&error, at.clone()));
    let config = &ctx.config;
    let platforms = match platforms(config, spec) {
        Ok(it) => it,
        Err(error) => return Ok(Staged::Failed(error)),
    };
    let Platforms { host, host_flags, target, target_flags } = &platforms;
    let requested = match &spec.foreign {
        Some(ask) => ask.profile.root.clone(),
        None => match requested_profile(spec.intent.default_profile(), spec.release, spec.profile.as_deref()) {
            Ok(it) => it,
            Err(error) => return Ok(fail(error)),
        },
    };
    let manifest_profiles = match &spec.foreign {
        Some(ask) => match foreign_profiles(ws.profiles(), ask) {
            Ok(it) => Some(it),
            Err(error) => return Ok(fail(error)),
        },
        None => ws.profiles().cloned(),
    };
    let profiles = match Profiles::new(manifest_profiles.as_ref(), ws.unstable_features(), config, &requested, &host.triple) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let mut warnings: Vec<String> = read.config.warnings.clone();
    warnings.extend(read.warnings.iter().cloned());
    warnings.extend(resolved.warnings.iter().cloned());
    {
        let pkgs: Vec<&blaze_manifest::manifest::PackageId> =
            resolved.resolve.iter().filter_map(|id| resolved.manifests.get(&id)).map(|(_, m)| &m.package_id).collect();
        if let Err(error) = profiles.validate_packages(manifest_profiles.as_ref(), &pkgs, &mut warnings) {
            return Ok(fail(error));
        }
    }

    let filter = match run_filter(spec, &resolved) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let targets = Targets { reads: &resolved.reads, host, target: target.as_ref() };
    let kinds = match target {
        Some(target) => vec![CompileKind::Target(InternedString::new(&target.triple))],
        None => vec![CompileKind::Host],
    };
    let opts = FeatureOpts::new(resolved.behavior, resolved.has_dev_units, ForceAllTargets::No);
    let members: Vec<_> = resolved.feature_members.iter().map(|(summary, features)| (summary, features.clone())).collect();
    let features = match FeatureResolver::resolve(&targets, &resolved.resolve, &members, &kinds, opts) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let all_ids: Vec<PackageId> = resolved.resolve.iter().collect();
    // A foreign workspace's roots are the packages it names (cargo's `generate_std_roots`),
    // which need not be the packages it resolves for.
    let root_specs: Vec<blaze_resolve::PackageIdSpec> = match &spec.foreign {
        Some(ask) => match ask.roots.iter().map(|name| blaze_resolve::PackageIdSpec::parse(name)).collect() {
            Ok(it) => it,
            Err(error) => return Ok(fail(error)),
        },
        None => resolved.specs.clone(),
    };
    let roots = match root_specs.iter().map(|spec| spec.query(&all_ids)).collect::<anyhow::Result<Vec<_>>>() {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let members: HashSet<PackageId> = resolved.member_ids.iter().copied().collect();
    let mut by_id: HashMap<PackageId, Package<'_>> = HashMap::with_capacity_and_hasher(all_ids.len(), Default::default());
    let mut packages_by_spec: BySpec<'_> = HashMap::with_capacity_and_hasher(all_ids.len(), Default::default());
    for id in &all_ids {
        let Some((_, manifest)) = resolved.manifests.get(id) else { continue };
        let package = Package { manifest, is_member: members.contains(id) };
        by_id.insert(*id, package);
        // alloc: the spec is how a unit names its package (`Unit::pkg_id`), once per package.
        packages_by_spec.insert(id.to_spec().to_string(), (*id, package));
    }
    let opt_out = match super::sandbox::workspace(ws.custom_metadata(), ws.root_manifest()) {
        Ok(it) => it,
        Err(error) => return Ok(Staged::Failed(error)),
    };
    let mut unsandboxed: HashSet<&str> = HashSet::default();
    // A member's tests keys, by id: a dependency's own table is not read (it can't opt out).
    let mut test_sandbox: HashMap<PackageId, TestSandbox> = HashMap::default();
    for (spec, (id, package)) in &packages_by_spec {
        let own = match package.is_member {
            true => match super::sandbox::package(package.manifest.custom_metadata.as_ref(), resolved.manifests[id].0) {
                Ok(it) => it,
                Err(error) => return Ok(Staged::Failed(error)),
            },
            false => PackageOptOut::default(),
        };
        if own.build_script_outside || opt_out.unsandboxed(id.name().as_str()) {
            unsandboxed.insert(spec.as_str());
        }
        if own.tests != TestSandbox::default() {
            test_sandbox.insert(*id, own.tests);
        }
    }
    let request = Request {
        packages: &by_id,
        resolve: &resolved.resolve,
        features: &features,
        profiles: &profiles,
        roots: &roots,
        intent: spec.intent,
        filter: &filter,
        host,
        target: target.as_ref(),
        has_dev_units: resolved.has_dev_units == HasDevUnits::Yes,
    };
    let mut graph = match crate::unit_graph(&request) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    if let Some(error) = artifact_refusal(&graph, &packages_by_spec, &resolved) {
        return Ok(Staged::Failed(error));
    }
    if spec.honor_rust_version
        && let Err(error) = rust_version_refusal(&graph, &packages_by_spec, &spec.rustc_version)
    {
        return Ok(fail(error));
    }
    if spec.run {
        if let Err(error) = run_one(&graph, &filter) {
            return Ok(fail(error));
        }
    }

    let target_dir = match &spec.target_dir {
        Some(dir) => PathBuf::from(dir),
        None => ws.target_dir(),
    };
    let build_dir = match config.build_dir(ws.root_manifest()) {
        Ok(Some(dir)) => dir,
        Ok(None) => target_dir.clone(),
        Err(error) => return Ok(fail(error)),
    };
    let dir_name = profiles.get_dir_name().to_owned();

    // The standard library's units: of each platform a unit is built for, merged in.
    let named_roots: Vec<(String, usize)> = match &spec.foreign {
        Some(ask) => {
            let mut named = Vec::new();
            for (name, id) in ask.roots.iter().zip(&roots) {
                let pkg_id = id.to_spec().to_string();
                let found = graph.roots.iter().copied().find(|at| {
                    let unit = &graph.units[*at];
                    unit.pkg_id == pkg_id && unit.target.is_lib() && unit.mode == Mode::Build
                });
                match found {
                    Some(at) => named.push((name.clone(), at)),
                    None => return Ok(fail(anyhow::anyhow!("package `{name}` has no library unit among the roots"))),
                }
            }
            named
        }
        None => Vec::new(),
    };
    let mut std_plans: Vec<Option<UnitPlan>> = vec![None; graph.units.len()];
    let mut std_packages: Vec<PackagePlan> = Vec::new();
    match &spec.foreign {
        Some(ask) => foreign_units(&mut graph, &packages_by_spec, ask),
        None => {
            let base = match schema_profile(&profiles) {
                Ok(it) => it,
                Err(error) => return Ok(fail(error)),
            };
            let mut platforms_used: Vec<Option<String>> = graph.units.iter().map(|it| it.platform.clone()).collect();
            platforms_used.sort();
            platforms_used.dedup();
            for platform in platforms_used {
                let triple = platform.clone().unwrap_or_else(|| host.triple.clone());
                let rustflags = match &platform {
                    Some(_) => target_flags.clone(),
                    None => host_flags.clone(),
                };
                let key = match std_key(spec, &graph, &packages_by_spec, &base, &profiles, &triple, rustflags) {
                    Ok(it) => it,
                    Err(error) => return Ok(Staged::Failed(error)),
                };
                let std = match ctx.files().with_cx(|cx| cx.get::<StdUnits>(&key).map(|it| it.get()))? {
                    StdPlanned::Ready(std) => std,
                    StdPlanned::Failed(error) => return Ok(Staged::Failed(error)),
                };
                // Named by its key alone, never by the workspace: every workspace on the daemon
                // asking for the same std names the same outputs, and builds it once.
                if !Path::new(&spec.std).is_absolute() {
                    return Ok(fail(anyhow::anyhow!(
                        "the daemon names `{}`, no absolute directory, for the standard library's builds",
                        spec.std
                    )));
                }
                let out = format!("{}/{:x}", spec.std, key.id());
                merge_std(&mut graph, &mut std_plans, &mut std_packages, std, &platform, &out);
            }
        }
    }

    let (out_dir, out_build) = match &spec.foreign {
        Some(_) => (PathBuf::from(FOREIGN_TARGET), PathBuf::from(FOREIGN_TARGET)),
        None => (target_dir.clone(), build_dir.clone()),
    };
    let jobs = match jobs(config, spec.jobs) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    // A standard library's packages are hashed against the library's root, so their hashes do
    // not depend on where it is named.
    let sysroot = spec.foreign.as_ref().and_then(|ask| Path::new(&ask.manifest).parent());
    let base_profile = profiles.base_profile();
    let input = super::plan::PlanInput {
        units: &graph.units,
        std_plans: &std_plans,
        packages: &packages_by_spec,
        manifests: &resolved.manifests,
        roots: &graph.roots,
        config,
        intent: spec.intent,
        lints: spec.lints,
        build_std: spec.build_std.is_some(),
        ws_root: ws.root(),
        ws_manifest: ws.root_manifest(),
        ws_metadata: ws.custom_metadata(),
        sysroot,
        target_dir: &out_dir,
        build_dir: &out_build,
        host: &host.triple,
        target: target.as_ref().map(|it| it.triple.as_str()),
        requested_profile: &requested,
        profile_dir: &dir_name,
        base_profile: &base_profile,
        jobs,
        unsandboxed: &unsandboxed,
    };
    let planned = match super::plan::plan(&input) {
        Ok(it) => it,
        Err(error) => return Ok(Staged::Failed(error)),
    };
    let mut plans: Vec<UnitPlan> = Vec::with_capacity(planned.plans.len());
    for (here, std) in planned.plans.into_iter().zip(std_plans) {
        match here.or(std) {
            Some(plan) => plans.push(plan),
            None => return Ok(Staged::Failed(LoadError { what: "a unit has no plan".to_owned(), at: at.clone() })),
        }
    }
    if let Some(ask) = &spec.foreign {
        let ours = |at: &Path| at.display().to_string();
        let prefixes = [
            ours(&Path::new(FOREIGN_TARGET).join(&ask.triple).join(&dir_name)),
            ours(&Path::new(FOREIGN_TARGET).join(&dir_name)),
        ];
        for plan in &mut plans {
            rebase(plan, &prefixes, &ask.out);
        }
    }

    let checksums = resolved.resolve.checksums();
    let const_macros_on = super::sandbox::const_macros_on(ws.custom_metadata());
    let mut packages: Vec<PackagePlan> = Vec::with_capacity(by_id.len());
    // The first by manifest, whatever order the packages are walked in.
    let mut failed: Option<LoadError> = None;
    for (id, package) in &by_id {
        let checksum = checksums.get(id).and_then(Option::as_deref);
        let tests = test_sandbox.remove(id).unwrap_or_default();
        let manifest_path = resolved.manifests[id].0;
        let metadata = package.manifest.custom_metadata.as_ref();
        match super::sandbox::const_macros(metadata, manifest_path, ws.root_manifest(), const_macros_on) {
            Ok(const_macros) => packages.push(package_plan(
                *id,
                package.manifest,
                manifest_path,
                package.is_member,
                checksum,
                tests,
                const_macros,
            )),
            Err(error) if failed.as_ref().is_none_or(|it| error.at < it.at) => failed = Some(error),
            Err(_) => {}
        }
    }
    if let Some(error) = failed {
        return Ok(Staged::Failed(error));
    }
    packages.extend(std_packages);
    packages.sort_by(|a, b| a.id.cmp(&b.id));
    packages.dedup_by(|a, b| a.id == b.id);
    Ok(Staged::Done(Built {
        graph,
        plans,
        packages,
        lock: resolved.lock,
        warnings,
        target_dir: target_dir.display().to_string(),
        build_dir: build_dir.display().to_string(),
        finished: planned.finished,
        roots: named_roots,
        link_dirs: opt_out.link_dirs,
        sandboxed_tests: opt_out.sandboxed_tests,
    }))
}

/// `-j` or `build.jobs` as cargo's `JobsConfig` reads it, when it names a number of jobs; none
/// for cargo's default, the machine's parallelism (less `-N` of it), which the daemon has.
fn jobs(config: &blaze_manifest::Config<Snap>, asked: Option<u32>) -> anyhow::Result<Option<u32>> {
    use blaze_manifest::config::schema::JobsConfig;
    if asked == Some(0) {
        anyhow::bail!("jobs may not be 0");
    }
    if asked.is_some() {
        return Ok(asked);
    }
    Ok(match &config.build_config()?.jobs {
        None => None,
        Some(JobsConfig::Integer(0)) => anyhow::bail!("jobs may not be 0"),
        Some(JobsConfig::Integer(n)) if *n > 0 => Some(*n as u32),
        Some(JobsConfig::Integer(_)) => None,
        Some(JobsConfig::String(it)) if it == "default" => None,
        Some(JobsConfig::String(it)) => match it.parse::<i32>() {
            Ok(0) => anyhow::bail!("jobs may not be 0"),
            Ok(n) if n > 0 => Some(n as u32),
            Ok(_) => None,
            Err(_) => anyhow::bail!("could not parse `{it}`. Number of parallel jobs should be `default` or a number."),
        },
    })
}

/// Where a foreign workspace's plans are made before their directory is put as
/// [`ForeignAsk::out`].
const FOREIGN_TARGET: &str = "/@blaze-foreign-target";

/// Each path of `plan` under one of `prefixes` put under `out` instead.
fn rebase<P: AsRef<str>>(plan: &mut UnitPlan, prefixes: &[P], out: &str) {
    let rebased = |path: &mut String| {
        for prefix in prefixes {
            let prefix = prefix.as_ref();
            if let Some(rest) = path.strip_prefix(prefix)
                && (rest.is_empty() || rest.starts_with('/'))
            {
                path.replace_range(..prefix.len(), out);
                return;
            }
        }
    };
    for output in &mut plan.outputs {
        rebased(output);
    }
    if let Some(dir) = &mut plan.out_dir {
        rebased(dir);
    }
    for (name, value) in &mut plan.env {
        if name == "OUT_DIR" {
            rebased(value);
        }
    }
}

/// The platforms a build is for: the host, and `--target`'s or `build.target`'s when there is
/// one, each with its cfg and the rustflags cargo passes its units.
pub(crate) struct Platforms {
    pub host: Platformed,
    pub host_flags: Vec<String>,
    pub target: Option<Platformed>,
    pub target_flags: Vec<String>,
}

fn platforms(config: &blaze_manifest::Config<Snap>, spec: &Spec) -> Result<Platforms, LoadError> {
    let error = |what: String| LoadError { what, at: None };
    if let Some(ask) = &spec.foreign {
        let parse = |cfg: &[String]| -> Result<Vec<Cfg>, LoadError> {
            cfg.iter().map(|it| it.parse::<Cfg>().map_err(|e| error(format!("the cfg `{it}`: {e}")))).collect()
        };
        return Ok(Platforms {
            host: Platformed { triple: ask.host.clone(), cfg: parse(&ask.host_cfg)? },
            host_flags: Vec::new(),
            target: Some(Platformed { triple: ask.triple.clone(), cfg: parse(&ask.cfg)? }),
            target_flags: ask.rustflags.clone(),
        });
    }
    let target = match &spec.target {
        Some(it) => Some(it.clone()),
        None => {
            let build = config.build_config().map_err(|e| load_error(&e, None))?;
            let named = build.target.as_ref().map(|it| it.values(config.cwd())).unwrap_or_default();
            match named.as_slice() {
                [] => None,
                [one] => Some(one.clone()),
                _ => {
                    return Err(error(
                        "blaze builds for one target at a time, and `build.target` names several".to_owned(),
                    ));
                }
            }
        }
    };
    let platform = |triple: &str| -> Result<(Platformed, Vec<String>), LoadError> {
        let rustflags = super::plan::platform_rustflags(config, triple, &spec.host, target.as_deref())?;
        let args = CfgArgs::from_flags(&rustflags).map_err(error)?;
        let cfg = platform::target_cfgs(triple, &CfgArgs { proc_macro: true, ..args }).map_err(error)?;
        Ok((Platformed { triple: triple.to_owned(), cfg }, rustflags))
    };
    let (host, host_flags) = platform(&spec.host)?;
    let (target, target_flags) = match target.as_deref().map(platform).transpose()? {
        Some((it, flags)) => (Some(it), flags),
        None => (None, Vec::new()),
    };
    Ok(Platforms { host, host_flags, target, target_flags })
}

/// What the feature resolver reads of the packages.
struct Targets<'a> {
    reads: &'a HashMap<PackageId, Read>,
    host: &'a Platformed,
    target: Option<&'a Platformed>,
}

impl PackageTargets for Targets<'_> {
    fn has_any_proc_macro(&self, package_id: PackageId) -> bool {
        self.reads.get(&package_id).is_some_and(|it| it.any_proc_macro)
    }

    fn has_proc_macro_lib(&self, package_id: PackageId) -> bool {
        self.reads.get(&package_id).is_some_and(|it| it.proc_macro_lib)
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        match kind {
            CompileKind::Host => self.host.activates(platform),
            CompileKind::Target(_) => self.target.unwrap_or(self.host).activates(platform),
        }
    }
}

/// `cargo run`'s targets: the one `default-run` names when no target flag does (cargo's
/// `commands/run.rs`), else the command's filter.
fn run_filter(spec: &Spec, resolved: &Resolved<'_>) -> anyhow::Result<Filter> {
    if !spec.run || !matches!(spec.filter, Filter::Default { .. }) {
        return Ok(spec.filter.clone());
    }
    let selected = resolved.member_ids.iter().filter(|id| resolved.specs.iter().any(|spec| spec.matches(**id)));
    let default_runs: Vec<&str> = selected
        .filter_map(|id| resolved.manifests.get(id))
        .filter_map(|(_, manifest)| manifest.default_run.as_deref())
        .collect();
    Ok(match default_runs.as_slice() {
        [bin] => Filter::Only {
            all_targets: false,
            lib: LibRule::False,
            bins: Rule::Just(vec![(*bin).to_owned()]),
            examples: Rule::none(),
            tests: Rule::none(),
            benches: Rule::none(),
        },
        _ => Filter::Default { required_features_filterable: false },
    })
}

/// cargo's `ops::run`: exactly one executable among the graph's roots.
fn run_one(graph: &schema::UnitGraph, filter: &Filter) -> anyhow::Result<()> {
    let specific = matches!(filter, Filter::Only { .. });
    let runnable: Vec<&Unit> = graph
        .roots
        .iter()
        .map(|it| &graph.units[*it])
        .filter(|unit| unit.target.kind.iter().any(|kind| kind == "bin" || kind == "example"))
        .collect();
    match runnable.as_slice() {
        [] if !specific => anyhow::bail!("a bin target must be available for `cargo run`"),
        [] => Ok(()),
        [one] if one.target.kind.iter().any(|it| it == "example") && !one.target.crate_types.iter().any(|it| it == "bin") => {
            anyhow::bail!("example target `{}` is a library and cannot be executed", one.target.name)
        }
        [_] => Ok(()),
        _ if !specific => {
            let mut names: Vec<&str> = runnable.iter().map(|it| it.target.name.as_str()).collect();
            names.sort();
            anyhow::bail!(
                "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a binary, or the `default-run` manifest key.\navailable binaries: {}",
                names.join(", ")
            )
        }
        _ => anyhow::bail!("`cargo run` can run at most one executable, but multiple were specified"),
    }
}

/// The packages by the spec a unit's `pkg_id` names them by.
type BySpec<'a> = HashMap<String, (PackageId, Package<'a>)>;

/// The first unit that depends on an artifact dependency (`-Z bindeps`, #601), which blaze
/// does not build: refused, naming both. Units that depend on none build.
fn artifact_refusal(graph: &schema::UnitGraph, packages: &BySpec<'_>, resolved: &Resolved<'_>) -> Option<LoadError> {
    for unit in &graph.units {
        let Some((id, package)) = packages.get(unit.pkg_id.as_str()) else { continue };
        if !package.manifest.dependencies.iter().any(|dep| dep.artifact.is_some()) {
            continue;
        }
        let script = unit.target.is_custom_build() && unit.mode != Mode::RunCustomBuild;
        let dev = matches!(unit.mode, Mode::Test) || unit.target.kind.iter().any(|it| matches!(it.as_str(), "test" | "bench" | "example"));
        let used = |kind: blaze_manifest::dependency::DepKind| match kind {
            blaze_manifest::dependency::DepKind::Build => script,
            blaze_manifest::dependency::DepKind::Development => dev && !script,
            blaze_manifest::dependency::DepKind::Normal => !script && unit.mode != Mode::RunCustomBuild,
        };
        for dep in &package.manifest.dependencies {
            let Some(artifact) = &dep.artifact else { continue };
            if !used(dep.kind) || !resolved.resolve.deps(*id).any(|(it, _)| it.name().as_str() == dep.name.as_str()) {
                continue;
            }
            return Some(LoadError {
                what: format!(
                    "blaze does not build artifact dependencies yet (`-Z bindeps`, #601): {} of `{}` depends on `{}` with `artifact = {:?}`",
                    unit_name(unit),
                    package.manifest.package_id.name,
                    dep.name,
                    artifact.kinds
                ),
                at: Some(package.manifest.package_id.source_id.to_string()),
            });
        }
    }
    None
}

/// cargo's check in `create_bcx` that rustc meets the `rust-version` of each unit's package,
/// a nightly counting as its release: a line for each unit that it doesn't, by package.
fn rust_version_refusal(graph: &schema::UnitGraph, packages: &BySpec<'_>, rustc: &str) -> anyhow::Result<()> {
    let release = rustc.split(' ').next().unwrap_or(rustc);
    let rustc_version: PartialVersion = semver::Version::parse(release)
        .with_context(|| format!("blaze's rustc version `{rustc}` is not a version"))?
        .into();
    let mut incompatible: Vec<(PackageId, &RustVersion)> = Vec::new();
    let mut local_incompatible = false;
    for unit in &graph.units {
        let Some((id, package)) = packages.get(&unit.pkg_id) else {
            anyhow::bail!("a unit of `{}` has no package in the resolve", unit.pkg_id);
        };
        let Some(msrv) = &package.manifest.metadata.rust_version else { continue };
        if msrv.is_compatible_with(&rustc_version) {
            continue;
        }
        local_incompatible |= id.source_id().is_path();
        incompatible.push((*id, msrv));
    }
    if incompatible.is_empty() {
        return Ok(());
    }
    use std::fmt::Write as _;
    let plural = if incompatible.len() == 1 { "" } else { "s" };
    let mut message = format!("rustc {rustc_version} is not supported by the following package{plural}:\n");
    incompatible.sort_by(|a, b| (a.0.name().as_str(), a.0.version()).cmp(&(b.0.name().as_str(), b.0.version())));
    for (id, msrv) in incompatible {
        _ = writeln!(message, "  {}@{} requires rustc {msrv}", id.name(), id.version());
    }
    if !local_incompatible {
        _ = writeln!(
            message,
            "Either upgrade rustc or select compatible dependency versions with
`cargo update <name>@<current-ver> --precise <compatible-ver>`
where `<compatible-ver>` is the latest version supporting rustc {rustc_version}",
        );
    }
    Err(anyhow::Error::msg(message))
}

fn unit_name(unit: &Unit) -> String {
    let kind = unit.target.kind.first().map(String::as_str).unwrap_or("lib");
    match unit.mode {
        Mode::RunCustomBuild => "the build script's run".to_owned(),
        _ if unit.target.is_custom_build() => "the build script".to_owned(),
        _ => format!("the {kind} target `{}`", unit.target.name),
    }
}

/// The base profile (no package's overrides) in the unit graph's form: what [`StdKey`] takes
/// of the user's profile.
fn schema_profile(profiles: &Profiles) -> anyhow::Result<schema::Profile> {
    serde_json::from_value(serde_json::to_value(profiles.base_profile())?).context("a profile in cargo's form")
}

fn debuginfo_text(debuginfo: &schema::DebugInfo) -> String {
    match debuginfo {
        schema::DebugInfo::Level(level) => level.to_string(),
        schema::DebugInfo::Named(name) => name.clone(),
    }
}

/// The standard library's key for the units built for `triple`.
fn std_key(
    spec: &Spec,
    graph: &schema::UnitGraph,
    packages: &BySpec<'_>,
    base: &schema::Profile,
    profiles: &Profiles,
    triple: &str,
    rustflags: Vec<String>,
) -> Result<StdKey, LoadError> {
    let library = spec.library.clone().ok_or_else(|| LoadError {
        what: format!("the standard library is mounted nowhere: no mount is at `{}`", spec.library_at),
        at: None,
    })?;
    let mut crates = match &spec.build_std {
        Some(crates) => crates.clone(),
        None => default_crates(triple).map_err(|what| LoadError { what, at: None })?,
    };
    // cargo's `std_crates`: `test` when a unit is a test in libtest's harness.
    let harnessed = graph.units.iter().any(|unit| {
        unit.mode == Mode::Test && harness_of(unit, packages)
    });
    if crates.iter().any(|it| it == "std") && harnessed && !crates.iter().any(|it| it == "test") {
        crates.push("test".to_owned());
    }
    crates.sort();
    crates.dedup();
    let root = match profiles.base_profile().root {
        blaze_manifest::profiles::ProfileRoot::Release => "release",
        blaze_manifest::profiles::ProfileRoot::Debug => "dev",
    };
    Ok(StdKey {
        library,
        triple: triple.to_owned(),
        profile: base.name.clone(),
        panic: base.panic.clone(),
        opt_level: base.opt_level.clone(),
        debug_assertions: base.debug_assertions,
        overflow_checks: base.overflow_checks,
        debuginfo: debuginfo_text(&base.debuginfo),
        crates,
        features: spec.build_std_features.clone().unwrap_or_else(default_features),
        host: spec.host.clone(),
        rustflags,
        profile_root: root.to_owned(),
    })
}

/// Whether the test `unit` runs in libtest's harness: its target's `harness`.
fn harness_of(unit: &Unit, packages: &BySpec<'_>) -> bool {
    let Some((_, package)) = packages.get(unit.pkg_id.as_str()) else { return false };
    package.manifest.targets.iter().any(|target| {
        target.name == unit.target.name && target.harness && kind_name(&target.kind) == unit.target.kind.first().map(String::as_str)
    })
}

fn kind_name(kind: &TargetKind) -> Option<&'static str> {
    Some(match kind {
        TargetKind::Lib(_) => return None,
        TargetKind::Bin => "bin",
        TargetKind::Test => "test",
        TargetKind::Bench => "bench",
        TargetKind::ExampleLib(_) | TargetKind::ExampleBin => "example",
        TargetKind::CustomBuild => "custom-build",
    })
}

/// `std`'s units for `platform` merged into `graph` as cargo's `attach_std_deps` attaches
/// them: every unit built for the platform but a build script's run depends on each root,
/// `noprelude` (named by `--extern noprelude:std=...`), public and not reported unused. blaze
/// has no sysroot, so a host unit (a build script, a proc macro) is given the host's std the
/// same way. std's own units, merged after the build's, are given none: its build scripts are
/// never compiled, their output blaze's port of each ([`crate::stdlib::scripts`]).
fn merge_std(
    graph: &mut schema::UnitGraph,
    plans: &mut Vec<Option<UnitPlan>>,
    packages: &mut Vec<PackagePlan>,
    std: StdGraph,
    platform: &Option<String>,
    out: &str,
) {
    let offset = graph.units.len();
    let users: Vec<usize> = (0..offset)
        .filter(|at| &graph.units[*at].platform == platform && graph.units[*at].mode != Mode::RunCustomBuild)
        .collect();
    graph.units.reserve(std.units.len());
    for mut unit in std.units {
        for dep in &mut unit.dependencies {
            dep.index += offset;
        }
        // The library was planned with `--target` its platform's triple: its target units are
        // of the build's `platform` (none for the host's), its build scripts the host's.
        if unit.platform.is_some() {
            unit.platform.clone_from(platform);
        }
        graph.units.push(unit);
    }
    plans.reserve(std.plans.len());
    for mut plan in std.plans {
        rebase(&mut plan, &[STD_OUT], out);
        plans.push(Some(plan));
    }
    for at in users {
        graph.units[at].dependencies.reserve(std.roots.len());
        for (name, index) in &std.roots {
            graph.units[at].dependencies.push(UnitDep {
                index: index + offset,
                extern_crate_name: name.clone(),
                public: Some(true),
                noprelude: Some(true),
                nounused: Some(true),
            });
        }
    }
    packages.extend(std.packages);
}

/// The units of a foreign workspace as its ask says: every one `is_std` when asked, the
/// packages that always abort built to abort, a standard library `dylib` built as an `rlib`,
/// each unit's profile named as the build's.
fn foreign_units(graph: &mut schema::UnitGraph, packages: &BySpec<'_>, ask: &ForeignAsk) {
    for unit in &mut graph.units {
        unit.is_std = ask.is_std;
        unit.profile.name.clone_from(&ask.profile.name);
        let script = unit.mode == Mode::RunCustomBuild || unit.target.is_custom_build();
        let aborts = packages
            .get(unit.pkg_id.as_str())
            .is_some_and(|(id, _)| ask.panic_abort.iter().any(|it| it.as_str() == id.name().as_str()));
        if aborts && !script {
            unit.profile.panic = "abort".to_owned();
        }
        // cargo's `UnitInterner::intern`: a standard library unit whose library is also a
        // `dylib` (library/std's `crate-type = ["dylib", "rlib"]`) is built as an `rlib` only.
        let dylib = unit.target.is_lib() && unit.target.kind.iter().any(|it| it == "dylib");
        if ask.is_std && dylib {
            // alloc: once per standard library load, the one target that names a `dylib`
            unit.target.kind = vec!["rlib".to_owned()];
            // alloc: as above
            unit.target.crate_types = vec!["rlib".to_owned()];
        }
    }
}

/// The workspace's own profiles, the root `ask.profile.root` given the build's settings.
fn foreign_profiles(profiles: Option<&TomlProfiles>, ask: &ForeignAsk) -> anyhow::Result<TomlProfiles> {
    let mut profiles = profiles.cloned().unwrap_or_else(|| TomlProfiles(BTreeMap::new()));
    let quoted = |it: &str| match it.parse::<u32>() {
        Ok(number) => number.to_string(),
        Err(_) => format!("{it:?}"),
    };
    let text = format!(
        "opt-level = {}\ndebug-assertions = {}\noverflow-checks = {}\ndebug = {}\npanic = {:?}\n",
        quoted(&ask.profile.opt_level),
        ask.profile.debug_assertions,
        ask.profile.overflow_checks,
        quoted(&ask.profile.debuginfo),
        ask.profile.panic,
    );
    let settings: TomlProfile = toml::from_str(&text).with_context(|| format!("the standard library's profile: {text}"))?;
    let name = ProfileName::new(ask.profile.root.clone())?;
    profiles.0.entry(name).or_default().merge(&settings);
    Ok(profiles)
}

/// A package as the build names it. `checksum` is the resolve's, in hex: kept only for a
/// crates.io package, whose `.crate` it is the SHA-256 of.
fn package_plan(
    id: PackageId,
    manifest: &Manifest,
    manifest_path: &Path,
    is_member: bool,
    checksum: Option<&str>,
    tests: TestSandbox,
    const_macros: Option<String>,
) -> PackagePlan {
    let named = format!("{} v{}", id.name(), id.version());
    let full = id.to_string();
    let source = full.strip_prefix(&named).map(str::trim).filter(|it| !it.is_empty()).map(str::to_owned);
    PackagePlan {
        id: id.to_spec().to_string(),
        name: manifest.package_id.name.to_string(),
        version: manifest.package_id.version.to_string(),
        manifest_path: manifest_path.display().to_string(),
        source,
        is_member,
        default_run: manifest.default_run.clone(),
        checksum: checksum.filter(|_| id.source_id().is_crates_io()).and_then(blaze_format::hex::decode::<32>),
        tests,
        const_macros,
    }
}
