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
use std::alloc::Global;

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
use crate::schema::{self, BuiltFor, Mode, Unit, UnitDep};
use crate::stdlib::target::{self as platform, CfgArgs};
use crate::stdlib::{DEV_RUNTIME, STD_OUT, default_crates, default_features, has_dev_runtime};
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
    /// The pinned native inputs the graph's build script runs are given ([`crate::query::Units::native`]).
    pub native: Vec<crate::query::NativeInput>,
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
    let Platforms { host, host_flags, target, target_flags, macro_wasm } = &platforms;
    let requested = match &spec.foreign {
        Some(ask) => ask.profile.root.clone(),
        None => match requested_profile(
            spec.intent.default_profile(),
            spec.release,
            spec.profile.as_deref(),
        ) {
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
    let mut profiles = match Profiles::new(
        manifest_profiles.as_ref(),
        ws.unstable_features(),
        config,
        &requested,
        &host.triple,
    ) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    // blaze's `contract-checks` (docs/v2/plan/design/contracts.md, part 1), which cargo's
    // profile tables do not carry. A foreign load builds another build's dependencies with
    // the profile it asks for, whose root default stands.
    if spec.foreign.is_none() {
        let set = blaze_manifest::profiles::read_contract_checks(ctx.files(), ws.root_manifest())
            .and_then(|keys| profiles.set_contract_checks(&keys));
        if let Err(error) = set {
            return Ok(fail(error));
        }
    }
    let mut warnings: Vec<String> = read.config.warnings.clone();
    warnings.extend(read.warnings.iter().cloned());
    warnings.extend(resolved.warnings.iter().cloned());
    {
        let pkgs: Vec<&blaze_manifest::manifest::PackageId> = resolved
            .resolve
            .iter()
            .filter_map(|id| resolved.manifests.get(&id))
            .map(|(_, m)| &m.package_id)
            .collect();
        if let Err(error) =
            profiles.validate_packages(manifest_profiles.as_ref(), &pkgs, &mut warnings)
        {
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
    let members: Vec<_> = resolved
        .feature_members
        .iter()
        .map(|(summary, features)| (summary, features.clone()))
        .collect();
    let features =
        match FeatureResolver::resolve(&targets, &resolved.resolve, &members, &kinds, opts) {
            Ok(it) => it,
            Err(error) => return Ok(fail(error)),
        };
    let all_ids: Vec<PackageId> = resolved.resolve.iter().collect();
    // A foreign workspace's roots are the packages it names (cargo's `generate_std_roots`),
    // which need not be the packages it resolves for.
    let root_specs: Vec<blaze_resolve::PackageIdSpec> = match &spec.foreign {
        Some(ask) => {
            match ask.roots.iter().map(|name| blaze_resolve::PackageIdSpec::parse(name)).collect() {
                Ok(it) => it,
                Err(error) => return Ok(fail(error)),
            }
        }
        None => resolved.specs.clone(),
    };
    let roots = match root_specs
        .iter()
        .map(|spec| spec.query(&all_ids))
        .collect::<anyhow::Result<Vec<_>>>()
    {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let members: HashSet<PackageId, Global> = resolved.member_ids.iter().copied().collect();
    let mut by_id: HashMap<PackageId, Package<'_>, Global> =
        HashMap::with_capacity_in(all_ids.len(), Global);
    let mut packages_by_spec: BySpec<'_> = HashMap::with_capacity_in(all_ids.len(), Global);
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
    let mut unsandboxed: HashSet<&str, Global> = HashSet::new_in(Global);
    // A member's tests keys, by id: a dependency's own table is not read (it can't opt out).
    let mut test_sandbox: HashMap<PackageId, TestSandbox, Global> = HashMap::new_in(Global);
    for (spec, (id, package)) in &packages_by_spec {
        let own = match package.is_member {
            true => match super::sandbox::package(
                package.manifest.custom_metadata.as_ref(),
                resolved.manifests[id].0,
            ) {
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
        macro_wasm: macro_wasm.as_ref(),
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
    // A wasm macro aborts on a panic: the guest's hook hands the host the message, and
    // wasm32-wasip1 has no unwinding. Before planning, so the units' hashes say so.
    let macro_platform = macro_wasm.as_ref().map(|it| it.triple.as_str());
    for unit in graph.units.iter_mut().filter(|it| it.macro_wasm) {
        "abort".clone_into(&mut unit.profile.panic);
    }
    // cargo passes `-C panic` only for a profile's `abort`, so a unit whose profile says
    // `unwind` (cargo's default) is built by rustc with its target's own strategy: `abort`
    // where the target has no unwinding (`--target wasm32-wasip1`). Its profile says so, for
    // its cfg, its hashes, its standard library and its code alike.
    if spec.foreign.is_none() {
        let host_aborts = match target_aborts(&host.triple) {
            Ok(it) => it,
            Err(error) => return Ok(Staged::Failed(error)),
        };
        let target_aborts = match target.as_ref().map(|it| target_aborts(&it.triple)).transpose() {
            Ok(it) => it,
            Err(error) => return Ok(Staged::Failed(error)),
        };
        for unit in
            graph.units.iter_mut().filter(|it| !it.macro_wasm && it.profile.panic == "unwind")
        {
            let aborts = match (&unit.platform, &target) {
                (None, _) => host_aborts,
                (Some(triple), Some(target)) if *triple == target.triple => {
                    target_aborts == Some(true)
                }
                (Some(triple), _) => {
                    return Ok(fail(anyhow::anyhow!(
                        "the unit graph has a unit of `{}` ({}) for {triple}, which the build is not for",
                        unit.pkg_id,
                        unit.target.name
                    )));
                }
            };
            if aborts {
                "abort".clone_into(&mut unit.profile.panic);
            }
        }
    }

    // `--target-dir`, else `CARGO_TARGET_DIR` or `build.target-dir`, else the workspace's
    // directory under blaze's root: never in the checkout. A foreign workspace's units are
    // placed under `FOREIGN_TARGET` below, whatever this says.
    let target_dir = match (&spec.target_dir, ws.explicit_target_dir()) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(dir)) => dir.to_path_buf(),
        (None, None) if spec.foreign.is_some() => PathBuf::from(FOREIGN_TARGET),
        (None, None) if Path::new(&spec.builds).is_absolute() => {
            blaze_manifest::home::build_dir(Path::new(&spec.builds), ws.root())
        }
        (None, None) => {
            return Ok(fail(anyhow::anyhow!(
                "the daemon names `{}`, no absolute directory, for the build directory of the workspace at {}",
                spec.builds,
                ws.root().display()
            )));
        }
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
                    None => {
                        return Ok(fail(anyhow::anyhow!(
                            "package `{name}` has no library unit among the roots"
                        )));
                    }
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
            // By what each unit is built for, of the build's platforms: the host, the target and
            // the wasm macros (a `--target wasm32-wasip1` build has a std for its target and
            // another for its macros, of one triple), in `BuiltFor`'s order.
            let mut platforms_used: Vec<BuiltFor<'_>> = [
                Some(BuiltFor { platform: None, macro_wasm: false }),
                target
                    .as_ref()
                    .map(|it| BuiltFor { platform: Some(it.triple.as_str()), macro_wasm: false }),
                macro_platform.map(|triple| BuiltFor { platform: Some(triple), macro_wasm: true }),
            ]
            .into_iter()
            .flatten()
            .filter(|it| graph.units.iter().any(|unit| unit.built_for() == *it))
            .collect(); // alloc: at most three, once per load
            platforms_used.sort();
            if let Some(unit) =
                graph.units.iter().find(|unit| !platforms_used.contains(&unit.built_for()))
            {
                return Ok(fail(anyhow::anyhow!(
                    "the unit graph has a unit of `{}` ({}) built for {}, which is none of the build's \
                     platforms: the host, its target {:?} and its wasm macros' {macro_platform:?}",
                    unit.pkg_id,
                    unit.target.name,
                    unit.built_for(),
                    target.as_ref().map(|it| it.triple.as_str())
                )));
            }
            // Each std merged so far, by its key's ID, with its roots in the graph: a platform
            // whose std has a key already merged (the host's and a `--target` of the host's own
            // triple in a release build, alike in profile, rustflags and where they run) shares
            // those units, as every workspace asking for one std shares its outputs. Two sets of
            // units of one key would build and place the same files twice.
            let mut merged: Vec<(blaze_ids::ContentId, Vec<(String, usize)>)> = Vec::new(); // alloc: a few platforms, once per load
            let links_runtime = links_dev_runtime(&graph, &packages_by_spec);
            for built_for in platforms_used {
                let triple = built_for.platform.unwrap_or(host.triple.as_str());
                // The wasm macros' std is planned as their units are: no rustflags, std's
                // default crates, aborting.
                let rustflags = match built_for.platform {
                    _ if built_for.macro_wasm => Vec::new(),
                    Some(_) => target_flags.clone(),
                    None => host_flags.clone(),
                };
                let key = match std_key(
                    spec,
                    &graph,
                    &packages_by_spec,
                    &base,
                    &profiles,
                    triple,
                    rustflags,
                    built_for,
                ) {
                    Ok(it) => it,
                    Err(error) => return Ok(Staged::Failed(error)),
                };
                let id = key.id();
                if let Some((_, roots)) = merged.iter().find(|(it, _)| *it == id) {
                    if let Err(error) = runs_as_keyed(&graph, roots, &key, id, built_for) {
                        return Ok(Staged::Failed(error));
                    }
                    attach_std(&mut graph, roots, built_for, &links_runtime);
                    continue;
                }
                let std =
                    match ctx.files().with_cx(|cx| cx.get::<StdUnits>(&key).map(|it| it.get()))? {
                        StdPlanned::Ready(std) => std,
                        StdPlanned::Failed(error) => return Ok(Staged::Failed(error)),
                    };
                // Named by its key alone, never by the workspace: every workspace on the daemon
                // asking for the same std names the same outputs, and builds it once.
                if !Path::new(&spec.std).has_root() {
                    return Ok(fail(anyhow::anyhow!(
                        "the daemon names `{}`, no absolute directory, for the standard library's builds",
                        spec.std
                    )));
                }
                let out = format!("{}/{:x}", spec.std, id);
                let roots = merge_std(
                    &mut graph,
                    &mut std_plans,
                    &mut std_packages,
                    std,
                    built_for,
                    &out,
                    &links_runtime,
                );
                if let Err(error) = runs_as_keyed(&graph, &roots, &key, id, built_for) {
                    return Ok(Staged::Failed(error));
                }
                merged.push((id, roots));
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
        macro_wasm: macro_platform,
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
            None => {
                return Ok(Staged::Failed(LoadError {
                    what: "a unit has no plan".to_owned(),
                    at: at.clone(),
                }));
            }
        }
    }
    if let Some(ask) = &spec.foreign {
        let ours = |at: &Path| at.display().to_string();
        let prefixes = [
            ours(&Path::new(FOREIGN_TARGET).join(&ask.triple).join(&dir_name)),
            ours(
                &Path::new(FOREIGN_TARGET)
                    .join(super::plan::macro_dir(&ask.triple))
                    .join(&dir_name),
            ),
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
        match super::sandbox::const_macros(
            metadata,
            manifest_path,
            ws.root_manifest(),
            const_macros_on,
        ) {
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
    // A foreign load builds another build's dependencies (the standard library's), whose
    // scripts are given nothing of this workspace's.
    let native = match &spec.foreign {
        Some(_) => Vec::new(),
        None => {
            // alloc: each build script run of the graph, once a load
            let runs: Vec<super::native::ScriptRun<'_>> = graph
                .units
                .iter()
                .filter(|unit| unit.mode == Mode::RunCustomBuild && !unit.is_std)
                .filter_map(|unit| {
                    let (id, _) = packages_by_spec.get(&unit.pkg_id)?;
                    Some(super::native::ScriptRun {
                        package: id.name().as_str(),
                        platform: unit.platform.as_deref(),
                        features: &unit.features,
                    })
                })
                .collect();
            match super::native::needed(
                ctx.files(),
                ws.custom_metadata(),
                ws.root_manifest(),
                &host.triple,
                &runs,
            ) {
                Ok(it) => it,
                Err(error) => return Ok(Staged::Failed(error)),
            }
        }
    };
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
        native,
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
            Err(_) => anyhow::bail!(
                "could not parse `{it}`. Number of parallel jobs should be `default` or a number."
            ),
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
    /// [`super::MACRO_WASM`]'s, which proc macros are built for ([`Spec::macro_wasm`]): it
    /// may be the target's triple too, whose units [`schema::Unit::macro_wasm`] tells apart.
    pub macro_wasm: Option<Platformed>,
}

fn platforms(config: &blaze_manifest::Config<Snap>, spec: &Spec) -> Result<Platforms, LoadError> {
    let error = |what: String| LoadError { what, at: None };
    if let Some(ask) = &spec.foreign {
        let parse = |cfg: &[String]| -> Result<Vec<Cfg>, LoadError> {
            cfg.iter()
                .map(|it| it.parse::<Cfg>().map_err(|e| error(format!("the cfg `{it}`: {e}"))))
                .collect()
        };
        return Ok(Platforms {
            host: Platformed { triple: ask.host.clone(), cfg: parse(&ask.host_cfg)? },
            host_flags: Vec::new(),
            target: Some(Platformed { triple: ask.triple.clone(), cfg: parse(&ask.cfg)? }),
            target_flags: ask.rustflags.clone(),
            // The wasm macros' std: its units are planned under the macros' kind.
            macro_wasm: match ask.macro_wasm {
                true => Some(Platformed { triple: ask.triple.clone(), cfg: parse(&ask.cfg)? }),
                false => None,
            },
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
                        "blaze builds for one target at a time, and `build.target` names several"
                            .to_owned(),
                    ));
                }
            }
        }
    };
    let platform = |triple: &str| -> Result<(Platformed, Vec<String>), LoadError> {
        let rustflags =
            super::plan::platform_rustflags(config, triple, &spec.host, target.as_deref())?;
        let args = CfgArgs::from_flags(&rustflags).map_err(error)?;
        let cfg =
            platform::target_cfgs(triple, &CfgArgs { proc_macro: true, ..args }).map_err(error)?;
        Ok((Platformed { triple: triple.to_owned(), cfg }, rustflags))
    };
    let (host, host_flags) = platform(&spec.host)?;
    let (target, target_flags) = match target.as_deref().map(platform).transpose()? {
        Some((it, flags)) => (Some(it), flags),
        None => (None, Vec::new()),
    };
    // With no rustflags, as `plan::flags::macro_wasm_info` plans its units. A build for
    // `--target wasm32-wasip1` has both: its units and the macros' are told apart by
    // `schema::Unit::macro_wasm`, never by their triple.
    let macro_wasm = match spec.macro_wasm {
        true => {
            let cfg = platform::target_cfgs(
                super::MACRO_WASM,
                &CfgArgs { proc_macro: true, ..CfgArgs::default() },
            )
            .map_err(error)?;
            Some(Platformed { triple: super::MACRO_WASM.to_owned(), cfg })
        }
        false => None,
    };
    Ok(Platforms { host, host_flags, target, target_flags, macro_wasm })
}

/// What the feature resolver reads of the packages.
struct Targets<'a> {
    reads: &'a HashMap<PackageId, Read, Global>,
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
    let selected = resolved
        .member_ids
        .iter()
        .filter(|id| resolved.specs.iter().any(|spec| spec.matches(**id)));
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
        [one]
            if one.target.kind.iter().any(|it| it == "example")
                && !one.target.crate_types.iter().any(|it| it == "bin") =>
        {
            anyhow::bail!(
                "example target `{}` is a library and cannot be executed",
                one.target.name
            )
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
        _ => {
            anyhow::bail!("`cargo run` can run at most one executable, but multiple were specified")
        }
    }
}

/// The packages by the spec a unit's `pkg_id` names them by.
type BySpec<'a> = HashMap<String, (PackageId, Package<'a>), Global>;

/// The first unit that depends on an artifact dependency (`-Z bindeps`, #601), which blaze
/// does not build: refused, naming both. Units that depend on none build.
fn artifact_refusal(
    graph: &schema::UnitGraph,
    packages: &BySpec<'_>,
    resolved: &Resolved<'_>,
) -> Option<LoadError> {
    for unit in &graph.units {
        let Some((id, package)) = packages.get(unit.pkg_id.as_str()) else { continue };
        if !package.manifest.dependencies.iter().any(|dep| dep.artifact.is_some()) {
            continue;
        }
        let script = unit.target.is_custom_build() && unit.mode != Mode::RunCustomBuild;
        let dev = matches!(unit.mode, Mode::Test)
            || unit
                .target
                .kind
                .iter()
                .any(|it| matches!(it.as_str(), "test" | "bench" | "example"));
        let used = |kind: blaze_manifest::dependency::DepKind| match kind {
            blaze_manifest::dependency::DepKind::Build => script,
            blaze_manifest::dependency::DepKind::Development => dev && !script,
            blaze_manifest::dependency::DepKind::Normal => {
                !script && unit.mode != Mode::RunCustomBuild
            }
        };
        for dep in &package.manifest.dependencies {
            let Some(artifact) = &dep.artifact else { continue };
            if !used(dep.kind)
                || !resolved
                    .resolve
                    .deps(*id)
                    .any(|(it, _)| it.name().as_str() == dep.name.as_str())
            {
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
fn rust_version_refusal(
    graph: &schema::UnitGraph,
    packages: &BySpec<'_>,
    rustc: &str,
) -> anyhow::Result<()> {
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
    let mut message =
        format!("rustc {rustc_version} is not supported by the following package{plural}:\n");
    incompatible.sort_by(|a, b| {
        (a.0.name().as_str(), a.0.version()).cmp(&(b.0.name().as_str(), b.0.version()))
    });
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
    serde_json::from_value(serde_json::to_value(profiles.base_profile())?)
        .context("a profile in cargo's form")
}

fn debuginfo_text(debuginfo: &schema::DebugInfo) -> String {
    match debuginfo {
        schema::DebugInfo::Level(level) => level.to_string(),
        schema::DebugInfo::Named(name) => name.clone(),
    }
}

/// Whether rustc builds for `triple` with `panic = "abort"` when given no `-C panic`.
fn target_aborts(triple: &str) -> Result<bool, LoadError> {
    let facts =
        crate::stdlib::target::target_facts(triple).map_err(|what| LoadError { what, at: None })?;
    Ok(facts.panic == "abort")
}

/// The standard library's key for the units built for `built_for`, of `triple`. The wasm
/// macros' std is one `-Zbuild-std` and the build's tests do not shape: std's default crates
/// and features, aborting, and a key of its own.
fn std_key(
    spec: &Spec,
    graph: &schema::UnitGraph,
    packages: &BySpec<'_>,
    base: &schema::Profile,
    profiles: &Profiles,
    triple: &str,
    rustflags: Vec<String>,
    built_for: BuiltFor<'_>,
) -> Result<StdKey, LoadError> {
    let for_macros = built_for.macro_wasm;
    let library = spec.library.clone().ok_or_else(|| LoadError {
        what: format!(
            "the standard library is mounted nowhere: no mount is at `{}`",
            spec.library_at
        ),
        at: None,
    })?;
    let mut crates = match &spec.build_std {
        Some(crates) if !for_macros => crates.clone(),
        _ => default_crates(triple).map_err(|what| LoadError { what, at: None })?,
    };
    // cargo's `std_crates`: `test` when a unit is compiled with `--test` in libtest's harness
    // (`CompileMode::is_rustc_test`: a test, or a check of one, `check --tests` and the
    // `--all-targets` of `check --fix`, whose crate's root has the harness and its `extern crate
    // test` appended all the same), or a library's doctests, whose runner is a libtest harness
    // blaze writes (docs/v2/plan/design/doctests.md).
    let harnessed = !for_macros
        && graph.units.iter().any(|unit| {
            let rustc_test =
                unit.mode == Mode::Test || (unit.mode == Mode::Check && unit.checked_as_test);
            (rustc_test && harness_of(unit, packages)) || unit.mode == Mode::Doctest
        });
    if crates.iter().any(|it| it == "std") && harnessed && !crates.iter().any(|it| it == "test") {
        crates.push("test".to_owned());
    }
    // blaze's dev runtime ([`DEV_RUNTIME`]) when an executable of this platform may run on the
    // daemon's code memory: a build of this machine's own target, on a platform that has the
    // runtime, under a profile a live process is allowed under (`blaze_lazy::allow`: not
    // `release` or `bench`, at opt-level 0 or 1), with a binary or a test among its units.
    let dev_run = !for_macros
        && triple == spec.host
        && has_dev_runtime(triple)
        && !matches!(base.name.as_str(), "release" | "bench")
        && matches!(base.opt_level.as_str(), "0" | "1")
        && graph.units.iter().any(|unit| unit.built_for() == built_for && dev_executable(unit));
    if crates.iter().any(|it| it == "std") && dev_run && !crates.iter().any(|it| it == DEV_RUNTIME)
    {
        crates.push(DEV_RUNTIME.to_owned());
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
        // Built to unwind, as the toolchain's sysroot is, whatever the profile says: a program
        // that aborts links it with `panic_abort` (rustc's `RequiredPanicStrategy` lets a crate
        // built to unwind into one), and the build's tests, benches and build scripts, which
        // unwind under any profile (cargo's `PanicSetting::AlwaysUnwind`), link the same std.
        // A std built to abort under a profile's `abort` was linked by an unwinding test, which
        // hir-ty-codegen's `linked_crates` refuses (#1611). Only where nothing unwinds is it
        // built to abort: the wasm macros, and a target that aborts. A `-Zbuild-std` with
        // neither `std` nor `panic_unwind` (`core`, `alloc`) is built with the profile's
        // `panic`, as cargo builds it: no crate of it defines `rust_eh_personality`, which a
        // `core` built to unwind names from its landing pads (#1611 round 3, L9). The key names
        // it, so a std built to abort is never another's (rule 21).
        panic: if for_macros || target_aborts(triple)? {
            "abort".to_owned()
        } else if !crates.iter().any(|it| it == "std" || it == "panic_unwind") {
            base.panic.clone()
        } else {
            "unwind".to_owned()
        },
        opt_level: base.opt_level.clone(),
        debug_assertions: base.debug_assertions,
        overflow_checks: base.overflow_checks,
        debuginfo: debuginfo_text(&base.debuginfo),
        crates,
        features: match &spec.build_std_features {
            Some(features) if !for_macros => features.clone(),
            _ => default_features(),
        },
        host: spec.host.clone(),
        rustflags,
        profile_root: root.to_owned(),
        macro_wasm: for_macros,
        // As each of its units is compiled: std's units carry the build's base profile's name
        // (`foreign_units`) and, merged, `built_for` (`merge_std`).
        runs_here: schema::runs_here(built_for, &base.name),
        // clone: the key owns its strings
        codegen_backend: base.codegen_backend.clone(),
    })
}

/// Refuses std's `roots` for `built_for` when one of them runs elsewhere than `key`, of ID
/// `id`, says: a key that missed where std's code runs would give the target a std compiled
/// for this machine's CPU, or the host one for the baseline (rule 21).
fn runs_as_keyed(
    graph: &schema::UnitGraph,
    roots: &[(String, usize)],
    key: &StdKey,
    id: blaze_ids::ContentId,
    built_for: BuiltFor<'_>,
) -> Result<(), LoadError> {
    let at =
        |runs_here: bool| if runs_here { "on the machine that builds it" } else { "elsewhere" };
    match roots.iter().find(|(_, index)| graph.units[*index].runs_here() != key.runs_here) {
        None => Ok(()),
        Some((name, index)) => Err(LoadError {
            what: format!(
                "the standard library's `{name}` for {built_for} runs {}, yet its key {id:x} says it runs {}: \
                 one std key would name two builds of std",
                at(graph.units[*index].runs_here()),
                at(key.runs_here)
            ),
            at: None,
        }),
    }
}

/// Whether the test `unit` runs in libtest's harness: its target's `harness`. A library's unit
/// is the package's one library target, whatever crate types its unit names.
fn harness_of(unit: &Unit, packages: &BySpec<'_>) -> bool {
    let Some((_, package)) = packages.get(unit.pkg_id.as_str()) else { return false };
    package.manifest.targets.iter().any(|target| {
        target.name == unit.target.name
            && target.harness
            && match kind_name(&target.kind) {
                None => unit.target.is_lib(),
                Some(kind) => unit.target.kind.first().map(String::as_str) == Some(kind),
            }
    })
}

/// The unit graph's kind of a target, or `None` for a library, whose kinds are its crate types.
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
/// `built_for` is what its users are built for: for the wasm macros' platform, std's target
/// units are wasm macros' units ([`schema::Unit::macro_wasm`]) as their users are, and their
/// plans say [`UnitPlan::macro_wasm`]. Gives std's roots by name, with their units' indexes.
fn merge_std(
    graph: &mut schema::UnitGraph,
    plans: &mut Vec<Option<UnitPlan>>,
    packages: &mut Vec<PackagePlan>,
    std: StdGraph,
    built_for: BuiltFor<'_>,
    out: &str,
    links_runtime: &[bool],
) -> Vec<(String, usize)> {
    let offset = graph.units.len();
    // alloc: the triple, once per platform's std.
    let platform: Option<String> = built_for.platform.map(str::to_owned);
    graph.units.reserve(std.units.len());
    for mut unit in std.units {
        for dep in &mut unit.dependencies {
            dep.index += offset;
        }
        // The library was planned with `--target` its platform's triple: its target units are
        // of the build's `platform` (none for the host's), its build scripts the host's.
        if unit.platform.is_some() {
            unit.platform.clone_from(&platform);
            unit.macro_wasm = built_for.macro_wasm;
        }
        graph.units.push(unit);
    }
    plans.reserve(std.plans.len());
    for (at, mut plan) in std.plans.into_iter().enumerate() {
        rebase(&mut plan, &[STD_OUT], out);
        plan.macro_wasm = graph.units[offset + at].macro_wasm;
        plans.push(Some(plan));
    }
    // alloc: std's few roots, once per platform's std.
    let roots: Vec<(String, usize)> =
        std.roots.into_iter().map(|(name, index)| (name, index + offset)).collect();
    attach_std(graph, &roots, built_for, links_runtime);
    packages.extend(std.packages);
    roots
}

/// Every unit of the build's own built for `built_for` but a build script's run made to depend
/// on each of std's `roots` (by name, at their indexes in `graph`), as [`merge_std`] says. The
/// dev runtime ([`DEV_RUNTIME`]), when a root, only for a dev executable ([`dev_executable`])
/// that does not link it itself (`links_runtime`, by unit, [`links_dev_runtime`]): two would
/// be two start hooks, and two runtimes whose program asks one that the hook never attached.
fn attach_std(
    graph: &mut schema::UnitGraph,
    roots: &[(String, usize)],
    built_for: BuiltFor<'_>,
    links_runtime: &[bool],
) {
    for (at, unit) in graph.units.iter_mut().enumerate() {
        if unit.is_std || unit.built_for() != built_for || unit.mode == Mode::RunCustomBuild {
            continue;
        }
        let runtime = dev_executable(unit) && !links_runtime.get(at).copied().unwrap_or(false);
        unit.dependencies.reserve(roots.len());
        for (name, index) in roots {
            if name == DEV_RUNTIME && !runtime {
                continue;
            }
            unit.dependencies.push(UnitDep {
                index: *index,
                extern_crate_name: name.clone(),
                public: Some(true),
                noprelude: Some(true),
                nounused: Some(true),
                dev: false,
            });
        }
    }
}

/// Whether `unit` builds a program that blaze runs on the daemon's code memory in a dev build,
/// and so gets the dev runtime ([`DEV_RUNTIME`]): a test, in libtest's harness or not, or a
/// binary or example program. Never a build script, which blaze runs from its file.
fn dev_executable(unit: &Unit) -> bool {
    !unit.is_std
        && match unit.mode {
            Mode::Test => true,
            Mode::Build => {
                !unit.target.is_custom_build()
                    && unit.target.crate_types.iter().any(|it| it == "bin")
            }
            _ => false,
        }
}

/// By unit of `graph` (the build's own, before std is merged): whether the program it builds
/// links blaze's dev runtime itself, a package of the build's named as [`DEV_RUNTIME`] is
/// (`blaze-inproc-rt`, which `blaze-inproc` and `blaze-live` depend on) among the units it
/// links. Build scripts and proc macros are not linked into it, so their dependencies are not
/// followed. A unit graph has no cycle; one would be followed once.
fn links_dev_runtime(graph: &schema::UnitGraph, packages: &BySpec<'_>) -> Vec<bool> {
    #[derive(Clone, Copy, PartialEq)]
    enum Seen {
        Not,
        On,
        Done(bool),
    }
    let is_runtime = |name: &str| {
        name.len() == DEV_RUNTIME.len()
            && name
                .bytes()
                .zip(DEV_RUNTIME.bytes())
                .all(|(it, want)| it == want || (it == b'-' && want == b'_'))
    };
    let linked = |unit: &Unit| {
        unit.mode != Mode::RunCustomBuild
            && !unit.target.is_custom_build()
            && !unit.target.is_proc_macro()
    };
    let units = &graph.units;
    // alloc: one mark a unit, once per load
    let mut seen = vec![Seen::Not; units.len()];
    // alloc: the walk's path, once per load
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for start in 0..units.len() {
        if seen[start] != Seen::Not {
            continue;
        }
        seen[start] = Seen::On;
        stack.push((start, 0));
        while let Some(&(at, next)) = stack.last() {
            let unit = &units[at];
            if let Some(dep) = unit.dependencies.get(next) {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                if seen[dep.index] == Seen::Not && linked(&units[dep.index]) {
                    seen[dep.index] = Seen::On;
                    stack.push((dep.index, 0));
                }
                continue;
            }
            let own = !unit.is_std
                && packages
                    .get(unit.pkg_id.as_str())
                    .is_some_and(|(id, _)| is_runtime(id.name().as_str()));
            let reached = own
                || unit
                    .dependencies
                    .iter()
                    .any(|dep| linked(&units[dep.index]) && seen[dep.index] == Seen::Done(true));
            seen[at] = Seen::Done(reached);
            stack.pop();
        }
    }
    seen.into_iter().map(|it| it == Seen::Done(true)).collect()
}

/// The units of a foreign workspace as its ask says: every one `is_std` when asked, the
/// packages that always abort built to abort, a standard library `dylib` built as an `rlib`,
/// each unit's profile named as the build's.
fn foreign_units(graph: &mut schema::UnitGraph, packages: &BySpec<'_>, ask: &ForeignAsk) {
    for unit in &mut graph.units {
        unit.is_std = ask.is_std;
        // The wasm macros' std: each unit for the triple is theirs, as `plan` keys and places it.
        unit.macro_wasm = ask.macro_wasm && unit.platform.is_some();
        unit.profile.name.clone_from(&ask.profile.name);
        // Not a setting of the workspace's tables, which would need the `codegen-backend`
        // cargo feature in library/Cargo.toml: the build's own, on every unit as it asked.
        unit.profile.codegen_backend.clone_from(&ask.profile.codegen_backend);
        let script = unit.mode == Mode::RunCustomBuild || unit.target.is_custom_build();
        let aborts = packages.get(unit.pkg_id.as_str()).is_some_and(|(id, _)| {
            ask.panic_abort.iter().any(|it| it.as_str() == id.name().as_str())
        });
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
fn foreign_profiles(
    profiles: Option<&TomlProfiles>,
    ask: &ForeignAsk,
) -> anyhow::Result<TomlProfiles> {
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
    let settings: TomlProfile =
        toml::from_str(&text).with_context(|| format!("the standard library's profile: {text}"))?;
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
    let source =
        full.strip_prefix(&named).map(str::trim).filter(|it| !it.is_empty()).map(str::to_owned);
    PackagePlan {
        id: id.to_spec().to_string(),
        name: manifest.package_id.name.to_string(),
        version: manifest.package_id.version.to_string(),
        manifest_path: manifest_path.display().to_string(),
        source,
        is_member,
        default_run: manifest.default_run.clone(),
        checksum: checksum
            .filter(|_| id.source_id().is_crates_io())
            .and_then(blaze_format::hex::decode::<32>),
        tests,
        const_macros,
    }
}
