//! `cargo metadata --format-version 1` with its `resolve`, made by blaze with no cargo: the
//! workspace read by blaze-manifest, every package `Cargo.lock` locks fetched by
//! blaze-registry where cargo would have it and read from its manifest, and the two resolves
//! of cargo's `resolve_ws_with_opts` made by blaze-resolve, guided by the lock file. Either
//! the whole workspace's graph, as `cargo metadata` prints it, or that of one build
//! ([`Build`]), whose features are a build's: unified over the members it selects, or over
//! every member with `resolver.feature-unification = "workspace"` (docs/self-contained.md,
//! "Resolution and fetching").
//!
//! A registry package the lock file does not pin is chosen from the registry's index, as
//! cargo chooses it, and a lock file that is missing or that the manifests no longer agree
//! with is written again, as cargo writes it. What it does not do yet is an error: a git or
//! vendored-directory package the lock file does not pin, a registry with a git index, and a
//! dependency of a registry package whose `.crate` cannot be fetched.

mod graph;
mod summary;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use blaze_manifest::config::RegistryConfig;
use blaze_manifest::config::schema::{
    CargoResolverConfig, FeatureUnification, IncompatibleRustVersions,
};
use blaze_manifest::manifest::{EitherManifest, Manifest, read_manifest};
use blaze_manifest::metadata::{SerializedPackage, export, export_package, root_manifest};
use blaze_manifest::{Config, Context, Workspace};
use blaze_resolve::lockfile::{self, PathDeps};
use blaze_resolve::metadata::{MetadataPackages, resolve_graph};
use blaze_resolve::resolver::{
    CliFeatures, CompileKind, FeatureOpts, FeatureResolver, ForceAllTargets, HasDevUnits, Members,
    PackageTargets, ResolveBehavior, VersionOrdering, members_with_features,
};
use blaze_resolve::workspace_resolve::{PackageRegistry, resolve_ws};
use blaze_resolve::{
    Cfg, Dependency, IndexSummary, InternedString, MemRegistry, PackageId, PackageIdSpec,
    PartialVersion, Platform, QueryKind, Registry, ResolveVersion, RustVersion, SourceId, Summary,
};
use rustc_hash::FxHashMap;

pub use crate::summary::Read;

/// What is asked of the workspace, as `cargo metadata` or `cargo build` is asked.
pub struct Asked<'a> {
    /// The directory cargo would be run in, and its variables.
    pub cwd: &'a Path,
    pub env: Vec<(String, String)>,
    pub manifest_path: Option<&'a Path>,
    /// `--config` arguments.
    pub config: &'a [String],
    pub features: &'a [String],
    pub all_features: bool,
    pub no_default_features: bool,
    /// The build whose graph is wanted; none for `cargo metadata`'s.
    pub build: Option<Build<'a>>,
    /// `--filter-platform`: the target's triple and its `cfg`.
    pub platform: Option<TargetPlatform<'a>>,
    pub offline: bool,
    /// blaze's store, which keeps `.crate` files by their sha256.
    pub store: Option<Arc<dyn blaze_registry::Blobs>>,
    /// The release of the rustc that builds, as `rustc -vV` says it (`1.101.0-nightly`): the
    /// Rust version a resolve that honours `rust-version` prefers when no member names one.
    pub rustc_version: Option<&'a str>,
}

/// The members a `cargo build` builds, as its flags select them.
pub struct Build<'a> {
    /// `-p`: the members by name.
    pub packages: &'a [String],
    pub workspace: bool,
    pub excludes: &'a [String],
    /// Whether their tests, examples and benchmarks are built too, and so their
    /// dev-dependencies.
    pub dev_units: bool,
}

pub struct TargetPlatform<'a> {
    pub triple: &'a str,
    pub cfg: &'a [Cfg],
}

impl TargetPlatform<'_> {
    fn activates(&self, platform: &Platform) -> bool {
        platform.matches(self.triple, self.cfg)
    }
}

/// A package read for the metadata: where its manifest is, the manifest, and the source the
/// lock file pins it to when it is not a path package.
struct Package {
    manifest_path: PathBuf,
    manifest: Box<Manifest>,
    locked_source: Option<String>,
}

/// The root manifest of the workspace the manifest at `manifest_path` is in, as
/// `cargo locate-project --workspace` finds it from `cwd` with the variables `env`.
pub fn workspace_root(
    cwd: &Path,
    env: Vec<(String, String)>,
    manifest_path: &Path,
) -> Result<PathBuf> {
    let ctx = Context::new(Config::load(cwd, env, &[])?);
    let ws = Workspace::new(&root_manifest(Some(manifest_path), &ctx)?, &ctx)?;
    Ok(ws.root_manifest().to_owned())
}

/// A program a `cargo build` or `cargo test` links: a binary or an example of a member it
/// builds, a `cdylib` or `staticlib` of its library, or a test of one.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Program {
    /// The id of its package, as `cargo metadata` spells it.
    pub package: String,
    pub name: String,
    /// The root file of its crate.
    pub root: PathBuf,
    pub kind: Kind,
    /// Built as a test, which `cargo test` runs: a library's or a binary's unit tests, with
    /// `cfg(test)`, or an integration test.
    pub test: bool,
    /// Whether a test starts in libtest's harness: not with `harness = false`.
    pub harness: bool,
}

/// What kind of target a program is built of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Lib,
    Bin,
    Example,
    Test,
    /// The `cdylib` of a library, which `cargo build` links as a shared library (#262).
    Cdylib,
    /// The `staticlib` of a library, an archive `cargo build` writes.
    Staticlib,
    /// The `dylib` of a library, a shared library of Rust's own ABI `cargo build` links.
    Dylib,
}

impl Kind {
    /// Whether it is a library's linked output, a `cdylib`, `staticlib` or `dylib`, and not an
    /// executable.
    pub fn linked_library(self) -> bool {
        matches!(self, Kind::Cdylib | Kind::Staticlib | Kind::Dylib)
    }
}

/// The programs a build names with `--bin`, `--example` or `--lib`, or none for every binary
/// and the `cdylib` and `staticlib` of each library; of `cargo test` (`test`), also the targets
/// `--lib` and `--test` name.
#[derive(Clone, Copy, Default)]
pub struct Targets<'a> {
    pub bin: Option<&'a str>,
    pub example: Option<&'a str>,
    /// `--lib` of a build: the library's `cdylib` and `staticlib`.
    pub lib: bool,
    pub test: Option<Tests<'a>>,
}

/// The targets of `cargo test` that `--lib` and `--test` name.
#[derive(Clone, Copy, Default)]
pub struct Tests<'a> {
    pub lib: bool,
    pub test: Option<&'a str>,
}

impl Targets<'_> {
    fn named(&self) -> bool {
        let test = self.test.is_some_and(|it| it.lib || it.test.is_some());
        self.bin.is_some() || self.example.is_some() || test
    }

    /// Whether the target `name` of `kind`, which `cargo test` tests when `tested`, is built,
    /// and whether as a test.
    fn picks(&self, kind: Kind, name: &str, tested: bool) -> Option<bool> {
        let Some(tests) = self.test else {
            let named = match self {
                Targets { bin: None, example: None, lib: false, .. } => {
                    kind == Kind::Bin || kind.linked_library()
                }
                _ => {
                    (kind == Kind::Bin && self.bin == Some(name))
                        || (kind == Kind::Example && self.example == Some(name))
                        || (kind.linked_library() && self.lib)
                }
            };
            return named.then_some(false);
        };
        if !self.named() {
            // cargo's default: what is tested, and the examples built to see that they compile.
            return match kind {
                Kind::Example if !tested => Some(false),
                _ => tested.then_some(true),
            };
        }
        let named = match kind {
            Kind::Lib => tests.lib,
            Kind::Bin => self.bin == Some(name),
            Kind::Example => self.example == Some(name),
            Kind::Test => tests.test == Some(name),
            Kind::Cdylib | Kind::Staticlib | Kind::Dylib => false,
        };
        named.then_some(kind != Kind::Example || tested)
    }
}

/// The JSON `cargo metadata --format-version 1` prints for `asked`.
pub fn metadata(asked: &Asked<'_>) -> Result<String> {
    Ok(made(asked)?.json)
}

/// The JSON `cargo metadata --format-version 1 --no-deps` prints for `asked`: the members
/// alone, nothing resolved or fetched.
pub fn metadata_no_deps(asked: &Asked<'_>) -> Result<String> {
    let mut config = Config::load(asked.cwd, asked.env.clone(), &[])?;
    config.start("metadata", asked.config)?;
    let ctx = Context::new(config);
    let ws = Workspace::new(&root_manifest(asked.manifest_path, &ctx)?, &ctx)?;
    Ok(serde_json::to_string(&blaze_manifest::metadata::export_no_deps(&ws))?)
}

/// [`metadata`] of a workspace whose lock file is never written, as `cargo metadata
/// --locked` reads one: a lock file the manifests no longer agree with is cargo's
/// `--locked` error. The standard library's workspace in a toolchain is read so.
pub fn metadata_locked(asked: &Asked<'_>) -> Result<String> {
    let relock =
        Relock { command: "metadata", locked: Some("--locked"), respect_source_config: true };
    let made = resolving_with(asked, None, Some(relock), |resolved, serialized, ids, _| {
        described(asked, &resolved, serialized, ids)
    })?;
    Ok(made.json)
}

/// The metadata of `asked`'s build, and the programs it links of `targets`, as cargo's unit
/// graph has them: of each member the build selects, the binaries whose `required-features`
/// it is built with, or the one target named; of `cargo test`'s, the targets it tests and
/// the examples it builds, or those named. A test of a member's integration tests links its
/// binaries too, which the tests may run.
pub fn programs(asked: &Asked<'_>, targets: Targets<'_>) -> Result<(String, Vec<Program>)> {
    anyhow::ensure!(asked.build.is_some(), "the programs are of a build");
    let Made { json, roots, unharnessed } = made(asked)?;
    let metadata: serde_json::Value = serde_json::from_str(&json)?;
    let nodes = metadata["resolve"]["nodes"].as_array().into_iter().flatten();
    let features: FxHashMap<&str, Vec<&str>> = nodes
        .filter_map(|node| {
            let on = node["features"].as_array()?.iter().filter_map(|it| it.as_str());
            Some((node["id"].as_str()?, on.collect()))
        })
        .collect();
    let packages = metadata["packages"].as_array().into_iter().flatten();
    let mut programs = Vec::new();
    for package in packages {
        let Some(id) = package["id"].as_str().filter(|id| roots.iter().any(|it| it == id)) else {
            continue;
        };
        let on = features.get(id).map(Vec::as_slice).unwrap_or_default();
        let mut integration = false;
        let mut bins = Vec::new();
        for target in package["targets"].as_array().into_iter().flatten() {
            let of = |key: &str, it: &str| {
                target[key].as_array().is_some_and(|all| all.iter().any(|kind| kind == it))
            };
            let only_bin = target["crate_types"].as_array().is_some_and(|it| it == &["bin"]);
            let kind = match () {
                _ if of("kind", "bin") && only_bin => Kind::Bin,
                _ if of("kind", "example") && only_bin => Kind::Example,
                _ if of("kind", "test") => Kind::Test,
                _ if of("kind", "bench") || of("kind", "example") => continue,
                _ if of("kind", "custom-build") => continue,
                _ => Kind::Lib,
            };
            // A build links a library's `cdylib`, `staticlib` and `dylib`, each a program of
            // its own; its `rlib` is linked into the others.
            let linked =
                [("cdylib", Kind::Cdylib), ("staticlib", Kind::Staticlib), ("dylib", Kind::Dylib)];
            let kinds: Vec<Kind> = match (kind, targets.test) {
                (Kind::Lib, None) => linked
                    .into_iter()
                    .filter(|(it, _)| of("crate_types", it))
                    .map(|(_, kind)| kind)
                    .collect(),
                _ => vec![kind],
            };
            let name = target["name"].as_str().unwrap_or_default();
            let tested = target["test"].as_bool().unwrap_or(false);
            let root = PathBuf::from(target["src_path"].as_str().unwrap_or_default());
            let required = target["required-features"].as_array().into_iter().flatten();
            // A feature of a dependency (`dep/feature`) is taken to be on.
            let missing: Vec<&str> = required
                .filter_map(|it| it.as_str())
                .filter(|it| !it.contains('/') && !on.contains(it))
                .collect();
            if kind == Kind::Bin && missing.is_empty() {
                bins.push((name, root.clone()));
            }
            for kind in kinds {
                let Some(test) = targets.picks(kind, name, tested) else { continue };
                if !missing.is_empty() && targets.named() {
                    let listed: Vec<String> = missing.iter().map(|it| format!("`{it}`")).collect();
                    bail!(
                        "target `{name}` in package `{}` requires the features: {}\nConsider enabling them by passing, e.g., `--features=\"{}\"`",
                        package["name"].as_str().unwrap_or_default(),
                        listed.join(", "),
                        missing.join(" ")
                    );
                }
                if missing.is_empty() {
                    integration |= kind == Kind::Test;
                    let harness = test && !unharnessed.contains(&root);
                    let (package, name, root) = (id.to_owned(), name.to_owned(), root.clone());
                    programs.push(Program { package, name, root, kind, test, harness });
                }
            }
        }
        for (name, root) in bins.into_iter().filter(|_| integration) {
            let (package, name) = (id.to_owned(), name.to_owned());
            let (kind, test, harness) = (Kind::Bin, false, false);
            programs.push(Program { package, name, root, kind, test, harness });
        }
    }
    Ok((json, programs))
}

/// The unit graph of a build, as `cargo build --unit-graph` prints it: which of its targets
/// and how, as [`blaze_units::Request`] says.
pub struct Units<'a> {
    pub intent: blaze_units::Intent,
    pub filter: &'a blaze_units::Filter,
    /// `--release`.
    pub release: bool,
    /// `--profile`.
    pub profile: Option<&'a str>,
    /// The host's triple and cfg.
    pub host: &'a blaze_units::Platformed,
    /// `--target`'s, when the build is not for the host.
    pub target: Option<&'a blaze_units::Platformed>,
}

/// The unit graph of `asked`'s build (`asked.build` selects its members, and has to want the
/// dev-dependencies exactly when the filter's targets do), made by blaze-units from blaze's
/// own resolve: what `cargo <command> --unit-graph` prints.
pub fn units(asked: &Asked<'_>, units: &Units<'_>) -> Result<blaze_units::UnitGraph> {
    let build = asked.build.as_ref().context("a unit graph is of a build")?;
    let has_dev_units = units.filter.need_dev_deps(units.intent);
    anyhow::ensure!(
        build.dev_units == has_dev_units,
        "the build {} the dev-dependencies but its targets {}",
        if build.dev_units { "resolves" } else { "does not resolve" },
        if has_dev_units { "need them" } else { "do not" },
    );
    let requested = blaze_manifest::profiles::requested_profile(
        units.intent.default_profile(),
        units.release,
        units.profile,
    )?;
    resolved(asked, None, |resolved| {
        let FeatureRoots { specs, members: member_features } = resolved.one_feature_resolve()?;
        let Resolved { ws, resolve, behavior, packages: reads, .. } = resolved;
        let profiles = blaze_manifest::profiles::Profiles::new(
            ws.profiles(),
            ws.unstable_features(),
            resolved.config,
            &requested,
            &units.host.triple,
        )?;
        let host = TargetPlatform { triple: &units.host.triple, cfg: &units.host.cfg };
        let target = units.target.map(|it| TargetPlatform { triple: &it.triple, cfg: &it.cfg });
        let packages =
            Packages { reads, platform: Some(target.as_ref().unwrap_or(&host)), host: Some(&host) };
        let kinds = match units.target {
            Some(target) => vec![CompileKind::Target(InternedString::new(&target.triple))],
            None => vec![CompileKind::Host],
        };
        let dev = if has_dev_units { HasDevUnits::Yes } else { HasDevUnits::No };
        let opts = FeatureOpts::new(behavior, dev, ForceAllTargets::No);
        let features =
            FeatureResolver::resolve(&packages, resolve, &member_features, &kinds, opts)?;
        let roots = resolved.selected_ids(&specs);
        let mut by_id = FxHashMap::default();
        for id in resolve.iter() {
            let manifest = resolved
                .manifests
                .get(&id)
                .with_context(|| format!("no manifest of `{id}` was read"))?;
            let is_member = resolved.member_ids.contains(&id);
            by_id.insert(id, blaze_units::Package { manifest, is_member });
        }
        blaze_units::unit_graph(&blaze_units::Request {
            packages: &by_id,
            resolve,
            features: &features,
            profiles: &profiles,
            roots: &roots,
            intent: units.intent,
            filter: units.filter,
            host: units.host,
            target: units.target,
            has_dev_units,
        })
    })
}

/// The workspace of an [`Asked`] resolved as cargo's `resolve_ws_with_opts` resolves it, before
/// the feature resolver: what `cargo metadata` and `cargo tree` print from.
pub struct Resolved<'a> {
    pub ws: &'a Workspace<'a>,
    /// The resolve of the packages `specs` select, guided by the lock file.
    pub resolve: &'a blaze_resolve::Resolve,
    pub members: Members<'a>,
    /// The members' ids, in the workspace's order.
    pub member_ids: &'a [PackageId],
    /// The packages selected: those built, or those whose trees are printed.
    pub specs: &'a [PackageIdSpec],
    pub cli_features: &'a CliFeatures,
    pub behavior: ResolveBehavior,
    pub has_dev_units: HasDevUnits,
    /// `resolver.feature-unification`: which members the features are resolved for.
    pub unification: FeatureUnification,
    /// What was read of every package the lock file names.
    pub packages: &'a FxHashMap<PackageId, Read>,
    /// The manifest of every package the lock file names.
    pub manifests: &'a FxHashMap<PackageId, &'a Manifest>,
    /// The configuration the workspace was read with.
    pub config: &'a Config,
}

/// One feature resolve of a build: the packages it selects, and the members it resolves the
/// features of, with theirs from the command line.
pub struct FeatureRoots<'a> {
    pub specs: Vec<PackageIdSpec>,
    pub members: Vec<(&'a Summary, CliFeatures)>,
}

impl<'a> Resolved<'a> {
    /// The feature resolves of the build, as cargo's `resolve_ws_with_opts` makes them under
    /// `resolver.feature-unification`: one of the selected packages, which resolves the
    /// features of every member under `workspace`; one per selected package under `package`.
    pub fn feature_resolves(&self) -> Result<Vec<FeatureRoots<'a>>> {
        let all;
        let resolved_for = match self.unification {
            FeatureUnification::Workspace => {
                all = self.member_ids.iter().map(|id| id.to_spec()).collect::<Vec<_>>();
                &all[..]
            }
            FeatureUnification::Selected | FeatureUnification::Package => self.specs,
        };
        let members = members_with_features(&self.members, resolved_for, self.cli_features)?;
        if !matches!(self.unification, FeatureUnification::Package) {
            return Ok(vec![FeatureRoots { specs: self.specs.to_vec(), members }]);
        }
        // Each package gets only the features the command line gives it, so that `-p a -p b
        // -F a/x,b/y` does not ask `b` for `x`.
        let one = |spec: &PackageIdSpec| -> Result<FeatureRoots<'a>> {
            let mut narrowed = self.cli_features.clone();
            let enabled = members.iter().filter(|(member, _)| spec.matches(member.package_id()));
            narrowed.features =
                std::rc::Rc::new(enabled.flat_map(|(_, it)| it.features.iter().cloned()).collect());
            let specs = vec![spec.clone()];
            let members = members_with_features(&self.members, &specs, &narrowed)?;
            Ok(FeatureRoots { specs, members })
        };
        self.specs.iter().map(one).collect()
    }

    /// The build's one feature resolve. `resolver.feature-unification = "package"` makes one
    /// per package, and so a package built for two of them with other features each; blaze
    /// builds one copy of each package per build, so that is refused for several packages.
    fn one_feature_resolve(&self) -> Result<FeatureRoots<'a>> {
        let mut resolves = self.feature_resolves()?;
        if resolves.len() > 1 {
            let named: Vec<String> =
                self.specs.iter().map(|it| format!("`{}`", it.name())).collect();
            bail!(
                "blaze does not build several packages with `resolver.feature-unification = \
                 \"package\"` yet: it resolves the features of {} apart, and blaze builds each \
                 package once per build; build them one at a time with `-p`",
                named.join(", ")
            );
        }
        resolves.pop().context("the build selects no package")
    }

    /// The members `specs` select, in the workspace's order.
    fn selected_ids(&self, specs: &[PackageIdSpec]) -> Vec<PackageId> {
        let selected = |id: &&PackageId| specs.iter().any(|spec| spec.matches(**id));
        self.member_ids.iter().filter(selected).copied().collect()
    }
}

/// The packages a command selects of the workspace, from its members' ids.
pub type Pick<'a> = &'a dyn Fn(&Workspace<'_>, &[PackageId]) -> Result<Vec<PackageIdSpec>>;

/// Resolves the workspace of `asked` for the packages `pick` selects (none: as `asked.build`
/// selects them, or every member), and hands it to `then`.
pub fn resolved<R>(
    asked: &Asked<'_>,
    pick: Option<Pick<'_>>,
    then: impl FnOnce(Resolved<'_>) -> Result<R>,
) -> Result<R> {
    resolving(asked, pick, |it, _, _| then(it))
}

/// The packages the workspace's lock file names, read with nothing fetched, and the member
/// whose manifest is the current one (none in a virtual workspace's root): what `cargo pkgid`
/// reads. None when there is no lock file.
pub struct Locked {
    pub ids: Vec<PackageId>,
    pub current: Option<PackageId>,
}

pub fn locked(asked: &Asked<'_>) -> Result<Option<Locked>> {
    with_lock(asked, "pkgid", |_, ws, lock| {
        let current = ws.members().find(|(path, _)| *path == ws.current_manifest());
        let current = current.map(|(_, m)| {
            let dir = m.package_id.source_id.local_path().context("a member's directory")?;
            let version = m.package_id.version.to_string();
            PackageId::try_new(m.package_id.name.as_str(), &version, SourceId::for_path(&dir)?)
        });
        Ok(Locked { ids: lock.iter().collect(), current: current.transpose()? })
    })
}

/// Fetches every package the workspace's lock file names that is not a path package, where
/// cargo would have it: `cargo fetch` for every platform. Returns how many there are; none
/// when there is no lock file.
pub fn fetch_locked(asked: &Asked<'_>) -> Result<Option<usize>> {
    with_lock(asked, "fetch", |ctx, _, lock| {
        let offline = asked.offline || ctx.config.net_config()?.offline.unwrap_or(false);
        let sources = sources(ctx, offline, asked.store.clone(), true, None)?;
        Ok(fetch(&locked_ids(lock), lock.checksums(), ctx, &sources)?.len())
    })
}

/// The workspace of `asked` and its lock file read, as `command` reads them, handed to `then`;
/// none when there is no lock file.
fn with_lock<R>(
    asked: &Asked<'_>,
    command: &str,
    then: impl FnOnce(&Context, &Workspace<'_>, &blaze_resolve::Resolve) -> Result<R>,
) -> Result<Option<R>> {
    let mut config = Config::load(asked.cwd, asked.env.clone(), &[])?;
    config.start(command, asked.config)?;
    let ctx = Context::new(config);
    let ws = Workspace::new(&root_manifest(asked.manifest_path, &ctx)?, &ctx)?;
    let root = ws.root().to_owned();
    let Ok(lock_text) = std::fs::read_to_string(root.join("Cargo.lock")) else {
        return Ok(None);
    };
    let path_packages = path_packages(&ws, &root, &ctx)?;
    let mut path_deps = PathDeps::default();
    let members = ws.members().map(|(_, m)| m);
    for m in members.chain(path_packages.iter().map(|it| &*it.manifest)) {
        let dir = m.package_id.source_id.local_path().context("a path package's directory")?;
        let entry = path_deps.entry(m.package_id.name.clone()).or_default();
        entry.insert(m.package_id.version.clone(), SourceId::for_path(&dir)?);
    }
    let lock = lockfile::into_resolve(lockfile::parse(&lock_text)?, &lock_text, &path_deps)?;
    then(&ctx, &ws, &lock).map(Some)
}

/// The metadata of `asked`, and what of it a build of its members reads ([`Made`]).
fn made(asked: &Asked<'_>) -> Result<Made> {
    resolving(asked, None, |resolved, serialized, ids| described(asked, &resolved, serialized, ids))
}

/// Resolves the workspace of `asked` again after its manifests changed, as `cargo remove`
/// does, every version it can keep being the one the lock file locks; writes the lock file
/// if the resolve changed it, unless `locked` (`--locked` or `--frozen`) forbids it. Returns
/// the packages `[patch]` names that the resolve did not use.
pub fn relock(asked: &Asked<'_>, command: &str, locked: Option<&str>) -> Result<Vec<PackageId>> {
    let relock = Relock { command, locked, respect_source_config: true };
    resolving_with(asked, None, Some(relock), |_, _, _, unused| Ok(unused.to_vec()))
}

/// What a resolve that may change the lock file was asked by.
#[derive(Clone, Copy)]
struct Relock<'a> {
    command: &'a str,
    locked: Option<&'a str>,
    /// Whether `[source]` replaces the sources it names; `cargo vendor` reads around it.
    respect_source_config: bool,
}

fn resolving<R>(
    asked: &Asked<'_>,
    pick: Option<Pick<'_>>,
    then: impl FnOnce(Resolved<'_>, Vec<SerializedPackage<'_>>, &[PackageId]) -> Result<R>,
) -> Result<R> {
    resolving_with(asked, pick, None, |it, serialized, ids, _| then(it, serialized, ids))
}

fn resolving_with<R>(
    asked: &Asked<'_>,
    pick: Option<Pick<'_>>,
    relock: Option<Relock<'_>>,
    then: impl FnOnce(Resolved<'_>, Vec<SerializedPackage<'_>>, &[PackageId], &[PackageId]) -> Result<R>,
) -> Result<R> {
    let mut config = Config::load(asked.cwd, asked.env.clone(), &[])?;
    config.start(relock.map_or("metadata", |it| it.command), asked.config)?;
    let offline = asked.offline || config.net_config()?.offline.unwrap_or(false);
    let ctx = Context::new(config);
    let ws = Workspace::new(&root_manifest(asked.manifest_path, &ctx)?, &ctx)?;
    let root = ws.root().to_owned();
    let replace = summary::replace_table(&root)?;
    let lock_path = root.join("Cargo.lock");
    let lock_text = match std::fs::read_to_string(&lock_path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", lock_path.display())),
    };

    let path_packages = path_packages(&ws, &root, &ctx)?;
    let mut path_deps = PathDeps::default();
    let members = ws.members().map(|(_, m)| m);
    for m in members.chain(path_packages.iter().map(|it| &*it.manifest)) {
        let dir = m.package_id.source_id.local_path().context("a path package's directory")?;
        let entry = path_deps.entry(m.package_id.name.clone()).or_default();
        entry.insert(m.package_id.version.clone(), SourceId::for_path(&dir)?);
    }
    let lock = match &lock_text {
        Some(text) => Some(lockfile::into_resolve(lockfile::parse(text)?, text, &path_deps)?),
        None => None,
    };
    let respect = relock.is_none_or(|it| it.respect_source_config);
    let sources = sources(&ctx, offline, asked.store.clone(), respect, None)?;
    let fetched = match &lock {
        Some(lock) => fetch(&locked_ids(lock), lock.checksums(), &ctx, &sources)?,
        None => Vec::new(),
    };
    // The registry packages the resolve picks that the lock file does not pin.
    let picked: Vec<Package>;

    let mut serialized: Vec<SerializedPackage<'_>> =
        ws.members().map(|(path, m)| export_package(path, m)).collect();
    let member_count = serialized.len();
    for package in path_packages.iter().chain(&fetched) {
        let mut it = export_package(&package.manifest_path, &package.manifest);
        if let Some(source) = &package.locked_source {
            it.set_source(source.clone());
        }
        serialized.push(it);
    }
    let mut reads = FxHashMap::default();
    let mut ids = Vec::with_capacity(serialized.len());
    for package in &serialized {
        let mut read = summary::read(&serde_json::to_value(package)?)?;
        let id = read.summary.package_id();
        if let Some(Some(checksum)) = lock.as_ref().and_then(|it| it.checksums().get(&id)) {
            read.summary.set_checksum(checksum.clone());
        }
        ids.push(id);
        reads.insert(id, read);
    }

    let mut registry = MemRegistry::new();
    for read in reads.values() {
        registry.add(IndexSummary::Candidate(read.summary.clone()));
    }
    let patch = summary::patch_tables(&root)?;
    let by_path: FxHashMap<SourceId, Summary> = reads
        .values()
        .filter(|it| it.summary.source_id().is_path())
        .map(|it| (it.summary.source_id(), it.summary.clone()))
        .collect();
    let member_ids = ids[..member_count].to_vec();
    let member_summaries: Vec<Summary> =
        member_ids.iter().map(|id| reads[id].summary.clone()).collect();
    let members: Vec<&Summary> = member_summaries.iter().collect();
    let current =
        ws.members().position(|(path, _)| path == ws.current_manifest()).map(|at| member_ids[at]);
    let behavior = match ws.resolve_behavior() {
        blaze_manifest::features::ResolveBehavior::V1 => ResolveBehavior::V1,
        blaze_manifest::features::ResolveBehavior::V2 => ResolveBehavior::V2,
        blaze_manifest::features::ResolveBehavior::V3 => ResolveBehavior::V3,
    };
    let new_cli_feature_behavior = behavior != ResolveBehavior::V1 || ws.is_virtual();
    let members_of = || Members { members: &members, current, new_cli_feature_behavior };
    let member_rust_versions: Vec<RustVersion> = ws
        .members()
        .filter_map(|(_, m)| m.metadata.rust_version.as_ref())
        .map(|it| it.to_string().parse())
        .collect::<Result<_>>()?;
    let lowest_rust_version = member_rust_versions.iter().min().cloned();
    let resolving = blaze_resolve::workspace_resolve::Workspace {
        members: members_of(),
        path_packages: &by_path,
        patch: &patch,
        replace: &replace,
        require_optional_deps: true,
        version_ordering: VersionOrdering::MaximumVersionsFirst,
        rust_versions: rust_versions(asked, &ctx, behavior, &member_rust_versions)?,
        resolve_version: ResolveVersion::with_rust_version(lowest_rust_version.as_ref()),
    };
    let specs: Vec<PackageIdSpec> = match (pick, &asked.build) {
        (Some(pick), _) => pick(&ws, &member_ids)?,
        (None, None) => member_ids.iter().map(|id| id.to_spec()).collect(),
        (None, Some(build)) => selected(&ws, &member_ids, build)?,
    };
    let cli_features = CliFeatures::from_command_line(
        asked.features,
        asked.all_features,
        !asked.no_default_features,
    )?;
    let has_dev_units = match &asked.build {
        Some(build) if !build.dev_units => HasDevUnits::No,
        _ => HasDevUnits::Yes,
    };
    let unification = feature_unification(&ctx.config)?;
    let every_member: Vec<PackageIdSpec>;
    let resolved_for = match unification {
        FeatureUnification::Workspace => {
            every_member = member_ids.iter().map(|id| id.to_spec()).collect();
            &every_member
        }
        FeatureUnification::Selected | FeatureUnification::Package => &specs,
    };
    let (mut whole, targeted) = resolve_ws(
        &mut PackageRegistry::new(Pinned(blaze_registry::IndexRegistry::new(&sources, registry))),
        &resolving,
        lock.as_ref(),
        &cli_features,
        resolved_for,
        has_dev_units,
    )?;
    if let Some(lock) = &lock {
        whole.merge_from(lock)?;
    }
    let unread: Vec<PackageId> =
        locked_ids(&whole).into_iter().filter(|id| !reads.contains_key(id)).collect();
    picked = fetch(&unread, whole.checksums(), &ctx, &sources)?;
    for package in &picked {
        let mut it = export_package(&package.manifest_path, &package.manifest);
        if let Some(source) = &package.locked_source {
            it.set_source(source.clone());
        }
        let mut read = summary::read(&serde_json::to_value(&it)?)?;
        let id = read.summary.package_id();
        if let Some(Some(checksum)) = whole.checksums().get(&id) {
            read.summary.set_checksum(checksum.clone());
        }
        serialized.push(it);
        ids.push(id);
        reads.insert(id, read);
    }
    let locked = relock.and_then(|it| it.locked).map(|flag| lockfile::Locked {
        flag,
        path: &lock_path,
        path_deps: &path_deps,
    });
    if let Some(text) = lockfile::lockfile_to_write(
        &mut whole,
        lock_text.as_deref(),
        lowest_rust_version.as_ref(),
        locked,
    )? {
        std::fs::write(&lock_path, text)
            .with_context(|| format!("failed to write {}", lock_path.display()))?;
    }
    let unused_patches = whole.unused_patches().to_vec();
    let manifests: FxHashMap<PackageId, &Manifest> = ws
        .members()
        .map(|(_, m)| m)
        .chain(path_packages.iter().chain(&fetched).chain(&picked).map(|it| &*it.manifest))
        .zip(&ids)
        .map(|(m, id)| (*id, m))
        .collect();
    let resolved = Resolved {
        ws: &ws,
        resolve: &targeted,
        members: members_of(),
        member_ids: &member_ids,
        specs: &specs,
        cli_features: &cli_features,
        behavior,
        has_dev_units,
        unification,
        packages: &reads,
        manifests: &manifests,
        config: &ctx.config,
    };
    then(resolved, serialized, &ids, &unused_patches)
}

/// `resolver.feature-unification`, which nightly cargo reads only with `-Zfeature-unification`
/// (`unstable.feature-unification`); `selected` otherwise.
fn feature_unification(config: &Config) -> Result<FeatureUnification> {
    let resolver = config.get::<CargoResolverConfig>("resolver")?;
    let unstable = config.get::<Option<bool>>("unstable.feature-unification")?;
    Ok(match (unstable, resolver.feature_unification) {
        (Some(true), Some(it)) => it,
        _ => FeatureUnification::Selected,
    })
}

/// What `cargo metadata` prints of `resolved`, and what of it a build of its members reads.
fn described(
    asked: &Asked<'_>,
    resolved: &Resolved<'_>,
    serialized: Vec<SerializedPackage<'_>>,
    ids: &[PackageId],
) -> Result<Made> {
    let Resolved {
        ws,
        resolve: targeted,
        member_ids,
        behavior,
        has_dev_units,
        packages: reads,
        ..
    } = *resolved;
    let current = resolved.members.current;

    let kinds = match &asked.platform {
        Some(platform) => vec![CompileKind::Target(InternedString::new(platform.triple))],
        None => vec![CompileKind::Host],
    };
    let packages = Packages { reads, platform: asked.platform.as_ref(), host: None };
    let mut roots = Vec::new();
    let resolve = match &asked.build {
        None => resolve_graph(targeted, member_ids, current, &packages, &kinds)?,
        Some(build) => {
            let FeatureRoots { specs, members: member_features } =
                resolved.one_feature_resolve()?;
            let opts = FeatureOpts::new(behavior, has_dev_units, ForceAllTargets::No);
            let features =
                FeatureResolver::resolve(&packages, targeted, &member_features, &kinds, opts)?;
            roots = resolved.selected_ids(&specs);
            let activated = |platform: &Platform| packages.activates(platform);
            graph::of_build(
                targeted,
                &features,
                &roots,
                build.dev_units,
                reads,
                &activated,
                current,
            )?
        }
    };

    // The packages the graph reaches, in the order of their ids, as cargo lists them.
    let reached: FxHashMap<PackageId, usize> =
        ids.iter().enumerate().map(|(at, id)| (*id, at)).collect();
    let mut listed: Vec<(PackageId, usize)> = Vec::new();
    for node in &resolve.nodes {
        let id = ids.iter().find(|id| id.to_spec() == node.id).copied();
        let id = id.with_context(|| format!("no package {} was read", node.id))?;
        listed.push((id, reached[&id]));
    }
    listed.sort();
    let mut slots: Vec<Option<SerializedPackage<'_>>> = serialized.into_iter().map(Some).collect();
    let packages: Vec<SerializedPackage<'_>> =
        listed.iter().filter_map(|(_, at)| slots[*at].take()).collect();
    let json = serde_json::to_string(&export(ws, packages, Some(resolve)))?;
    let targets = ws.members().flat_map(|(_, it)| &it.targets);
    let unharnessed = targets.filter(|it| !it.harness).filter_map(|it| it.src_path.clone());
    let roots = roots.iter().map(|it| it.to_spec().to_string()).collect();
    Ok(Made { json, roots, unharnessed: unharnessed.collect() })
}

/// The metadata of a workspace, as JSON, the ids of the members its build selects, and the
/// root files of the members' targets built without libtest's harness (`harness = false`),
/// which the metadata does not say.
struct Made {
    json: String,
    roots: Vec<String>,
    unharnessed: Vec<PathBuf>,
}

/// What the resolve graph and the feature resolver read of the packages.
struct Packages<'a> {
    reads: &'a FxHashMap<PackageId, Read>,
    platform: Option<&'a TargetPlatform<'a>>,
    /// The host's platform, for what is built for it, when it is not `platform`.
    host: Option<&'a TargetPlatform<'a>>,
}

impl Packages<'_> {
    fn activates(&self, platform: &Platform) -> bool {
        self.platform.is_none_or(|it| it.activates(platform))
    }

    fn activates_for(&self, platform: &Platform, kind: CompileKind) -> bool {
        match (kind, self.host) {
            (CompileKind::Host, Some(host)) => host.activates(platform),
            _ => self.activates(platform),
        }
    }
}

impl MetadataPackages for Packages<'_> {
    fn package_id(&self, id: PackageId) -> PackageId {
        id
    }

    fn lib_crate_name(&self, id: PackageId) -> Option<InternedString> {
        self.reads.get(&id).and_then(|it| it.lib)
    }

    fn platform_activated(&self, platform: &Platform, _kind: CompileKind) -> bool {
        self.activates(platform)
    }
}

impl PackageTargets for Packages<'_> {
    fn has_any_proc_macro(&self, package_id: PackageId) -> bool {
        self.reads.get(&package_id).is_some_and(|it| it.any_proc_macro)
    }

    fn has_proc_macro_lib(&self, package_id: PackageId) -> bool {
        self.reads.get(&package_id).is_some_and(|it| it.proc_macro_lib)
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        self.activates_for(platform, kind)
    }
}

/// The members `build` selects, as cargo's `Packages::from_flags` and `to_package_id_specs`
/// select them.
fn selected(
    ws: &Workspace<'_>,
    member_ids: &[PackageId],
    build: &Build<'_>,
) -> Result<Vec<PackageIdSpec>> {
    let named = |name: &str| member_ids.iter().find(|id| id.name().as_str() == name).copied();
    let ids: Vec<PackageId> = if build.workspace {
        let excluded = |id: &PackageId| build.excludes.iter().any(|it| it == id.name().as_str());
        member_ids.iter().filter(|id| !excluded(id)).copied().collect()
    } else if !build.packages.is_empty() {
        let found = build.packages.iter().map(|name| {
            named(name).with_context(|| format!("package `{name}` is not a member blaze knows"))
        });
        found.collect::<Result<_>>()?
    } else {
        let defaults = ws.default_members().map(|(_, m)| named(&m.package_id.name));
        defaults.collect::<Option<_>>().context("a default member that is not a member")?
    };
    Ok(ids.iter().map(|id| id.to_spec()).collect())
}

/// The path packages that are not members: those the members and the root's `[patch]` and
/// `[replace]` name by path, and theirs in turn.
fn path_packages(ws: &Workspace<'_>, root: &Path, ctx: &Context) -> Result<Vec<Package>> {
    let members: Vec<PathBuf> =
        ws.members().filter_map(|(path, _)| path.parent().map(Path::to_owned)).collect();
    let mut queue = summary::patch_paths(root)?;
    queue.extend(summary::replace_paths(&summary::replace_table(root)?));
    for (path, m) in ws.members() {
        queue.extend(path_dependencies(&serde_json::to_value(export_package(path, m))?));
    }
    let mut seen: Vec<PathBuf> = members.clone();
    let mut read = Vec::new();
    while let Some(dir) = queue.pop() {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        let manifest_path = dir.join("Cargo.toml");
        let source = blaze_manifest::source::SourceId::for_path(&dir)?;
        let EitherManifest::Real(manifest) = read_manifest(&manifest_path, source, ctx)? else {
            bail!("{} is a path dependency with no package", manifest_path.display());
        };
        let json = serde_json::to_value(export_package(&manifest_path, &manifest))?;
        queue.extend(path_dependencies(&json));
        read.push(Package { manifest_path, manifest, locked_source: None });
    }
    Ok(read)
}

fn path_dependencies(package: &serde_json::Value) -> Vec<PathBuf> {
    let deps = package["dependencies"].as_array().into_iter().flatten();
    deps.filter(|it| it["source"].is_null())
        .filter_map(|it| it["path"].as_str())
        .map(|it| summary::normalize(Path::new(it)))
        .collect()
}

/// Every package of `lock` that is not a path package, the `[[patch.unused]]` entries too,
/// which the resolve has to see to keep them unused.
fn locked_ids(lock: &blaze_resolve::Resolve) -> Vec<PackageId> {
    let ids = lock.sort().into_iter().chain(lock.unused_patches().iter().copied());
    ids.filter(|id| !id.source_id().is_path()).collect()
}

/// The packages `ids`, fetched where cargo would have them and read from their manifests,
/// with `checksums` those of a lock file or resolve.
fn fetch(
    ids: &[PackageId],
    checksums: &FxHashMap<PackageId, Option<String>>,
    ctx: &Context,
    sources: &blaze_registry::Sources,
) -> Result<Vec<Package>> {
    let mut locked = Vec::new();
    let mut texts = Vec::new();
    for &id in ids {
        let source = id.source_id();
        let source_text = source.as_url().to_string();
        let from = sources.map().replacement(source)?;
        let mut checksum = checksums.get(&id).cloned().flatten().unwrap_or_default();
        // A lock file of cargo before 1.53 may name no checksum: the index's.
        if checksum.is_empty() && from.is_remote_registry() {
            checksum = sources.registry(from)?.checksum(id.name().as_str(), id.version())?;
        }
        let package = blaze_registry::Package {
            name: id.name().to_string(),
            version: id.version().clone(),
            checksum,
        };
        locked.push(blaze_registry::Locked { source, package });
        texts.push(source_text);
    }
    let roots = sources.fetch(&locked)?;
    let mut read = Vec::with_capacity(texts.len());
    for (source_text, (dir, _)) in texts.into_iter().zip(roots) {
        let manifest_path = dir.join("Cargo.toml");
        let source = manifest_source(&source_text, &ctx.config)?;
        let EitherManifest::Real(manifest) = read_manifest(&manifest_path, source, ctx)? else {
            bail!("{} has no package", manifest_path.display());
        };
        read.push(Package { manifest_path, manifest, locked_source: Some(source_text) });
    }
    Ok(read)
}

/// The registries' indexes behind the packages the lock file pins, read from their fetched
/// manifests so that a locked build reads no index. Git checkouts and vendored directories,
/// which blaze does not list yet, answer only at the versions the lock file pins.
struct Pinned<'a>(blaze_registry::IndexRegistry<'a, MemRegistry>);

impl Registry for Pinned<'_> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> Result<()> {
        let source = dep.source_id();
        let registry = self.0.registry_of(source)?;
        if kind == QueryKind::Exact && (registry.is_none() || dep.version_req().is_locked()) {
            let mut found = false;
            self.0
                .others()
                .query(dep, kind, &mut |it| {
                    found = true;
                    f(it)
                })
                .await?;
            if found || source.is_path() {
                return Ok(());
            }
            if registry.is_none() {
                bail!(
                    "`{}` of {} is not pinned by Cargo.lock, and blaze reads that source only \
                     at the versions Cargo.lock pins",
                    dep.package_name(),
                    self.describe_source(source)
                );
            }
        }
        self.0.query(dep, kind, f).await
    }

    fn describe_source(&self, source: SourceId) -> String {
        self.0.describe_source(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        self.0.is_replaced(source)
    }
}

/// The Rust versions the resolve prefers packages to support, as cargo's `resolve_with_previous`
/// gives them: when `resolver.incompatible-rust-versions` is `fallback`, or unset with resolver
/// 3, the members' `rust-version`s, or rustc's release if none names one.
fn rust_versions(
    asked: &Asked<'_>,
    ctx: &Context,
    behavior: ResolveBehavior,
    members: &[RustVersion],
) -> Result<Option<Vec<PartialVersion>>> {
    let config: CargoResolverConfig = ctx.config.get("resolver")?;
    let honours = match config.incompatible_rust_versions {
        Some(IncompatibleRustVersions::Allow) => false,
        Some(IncompatibleRustVersions::Fallback) => true,
        None => behavior == ResolveBehavior::V3,
    };
    if !honours {
        return Ok(None);
    }
    if members.is_empty() {
        let rustc = asked.rustc_version.context(
            "the resolve prefers packages that support the workspace's Rust version, no member \
             names a `rust-version`, and the release of the rustc that builds is not known",
        )?;
        return Ok(Some(vec![rustc.parse()?]));
    }
    Ok(Some(members.iter().map(RustVersion::to_partial).collect()))
}

/// The sources packages are fetched from, `[source]` replacing those it names unless not
/// `respect_source_config`.
fn sources(
    ctx: &Context,
    offline: bool,
    store: Option<Arc<dyn blaze_registry::Blobs>>,
    respect_source_config: bool,
    auth: Option<Arc<blaze_registry::auth::Auth>>,
) -> Result<blaze_registry::Sources> {
    let mut context = blaze_registry::Context::new(
        ctx.config.home().to_owned(),
        blaze_registry::http::HyperHttp::shared()?,
    );
    context.net.offline = offline;
    context.store = store;
    context.auth = match auth {
        Some(auth) => Some(auth),
        None => Some(Arc::new(blaze_registry::auth::Auth::new(
            ctx.config.reload()?,
            Box::new(blaze_registry::auth::Stderr),
        ))),
    };
    let mut config = source_config(&ctx.config)?;
    if !respect_source_config {
        config.sources.clear();
    }
    blaze_registry::Sources::new(&config, Arc::new(context))
}

/// The sources a registry's web API command finds its registry in: cargo's `SourceConfigMap`
/// of the configuration (`[source]` replacing others only with `respect_source_config`, as
/// `ops::registry` reads it both ways), offline as `net.offline` says, with the command's
/// credentials when it has them.
pub fn registry_sources(
    ctx: &Context,
    respect_source_config: bool,
    auth: Option<Arc<blaze_registry::auth::Auth>>,
) -> Result<blaze_registry::Sources> {
    let offline = ctx.config.net_config()?.offline.unwrap_or(false);
    sources(ctx, offline, None, respect_source_config, auth)
}

/// A package of the lock file as `cargo vendor` reads it.
pub struct Vendored {
    pub id: PackageId,
    /// The checksum the lock file has of it.
    pub checksum: Option<String>,
    /// Its source: unpacked from its registry, or a path package's directory.
    pub root: PathBuf,
    /// The `.crate` of a registry's package; none for a path package.
    pub tarball: Option<PathBuf>,
}

/// Every package of the workspace's lock file, as `cargo vendor` reads them: the workspace
/// resolved again first, writing the lock file if that changes it (unless `locked`, the flag
/// that forbids it), and each registry package fetched, `[source]` replacing none unless
/// `respect_source_config`. A git package is refused: it is vendored with its manifest as
/// `cargo package` writes it, which blaze does not write yet.
pub fn vendored(
    asked: &Asked<'_>,
    respect_source_config: bool,
    locked: Option<&str>,
) -> Result<Vec<Vendored>> {
    let relock = Relock { command: "vendor", locked, respect_source_config };
    resolving_with(asked, None, Some(relock), |_, _, _, _| Ok(()))?;
    let read = with_lock(asked, "vendor", |ctx, _, lock| {
        let offline = asked.offline || ctx.config.net_config()?.offline.unwrap_or(false);
        let sources = sources(ctx, offline, asked.store.clone(), respect_source_config, None)?;
        let mut out = Vec::new();
        let mut locked = Vec::new();
        let mut fetched = Vec::new();
        for id in lock.sort() {
            let checksum = lock.checksums().get(&id).cloned().flatten();
            if id.source_id().is_path() {
                let root = id.source_id().local_path().context("a path package's directory")?;
                out.push(Vendored { id, checksum, root, tarball: None });
                continue;
            }
            let source = id.source_id();
            let from = sources.map().replacement(source)?;
            match from.kind() {
                blaze_resolve::SourceKind::Registry
                | blaze_resolve::SourceKind::SparseRegistry
                | blaze_resolve::SourceKind::LocalRegistry => {}
                blaze_resolve::SourceKind::Git(_) => bail!(
                    "blaze does not vendor `{id}` yet: a git package is vendored with its \
                     manifest as `cargo package` writes it, which blaze does not write yet"
                ),
                _ => bail!("blaze does not vendor `{id}` yet: it is read from {from}"),
            }
            let registry = sources.registry(from)?;
            let mut sum = checksum.clone().unwrap_or_default();
            if sum.is_empty() && from.is_remote_registry() {
                sum = registry.checksum(id.name().as_str(), id.version())?;
            }
            let package = blaze_registry::Package {
                name: id.name().to_string(),
                version: id.version().clone(),
                checksum: sum,
            };
            let tarball = registry.crate_file(id.name().as_str(), id.version());
            locked.push(blaze_registry::Locked { source, package });
            fetched.push(out.len());
            out.push(Vendored { id, checksum, root: PathBuf::new(), tarball: Some(tarball) });
        }
        for (at, (dir, _)) in fetched.into_iter().zip(sources.fetch(&locked)?) {
            out[at].root = dir;
        }
        Ok(out)
    })?;
    read.context("a lock file the resolve just wrote")
}

/// The source of a package the lock file pins to `source`, as blaze-manifest names it.
fn manifest_source(source: &str, config: &Config) -> Result<blaze_manifest::source::SourceId> {
    use blaze_manifest::source::{CRATES_IO_INDEX, SourceId};
    use cargo_util_schemas::core::GitReference;
    let (kind, rest) = source.split_once('+').context("a source with no kind")?;
    match kind {
        "registry" if rest == CRATES_IO_INDEX => SourceId::crates_io(config),
        "registry" => Ok(SourceId::for_registry(&rest.parse()?)),
        "sparse" => Ok(SourceId::for_registry(&source.parse()?)),
        "git" => {
            let mut url: url::Url = rest.parse()?;
            let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
            let reference = match (query.get("branch"), query.get("tag"), query.get("rev")) {
                (Some(branch), _, _) => GitReference::Branch(branch.clone()),
                (_, Some(tag), _) => GitReference::Tag(tag.clone()),
                (_, _, Some(rev)) => GitReference::Rev(rev.clone()),
                _ => GitReference::DefaultBranch,
            };
            url.set_fragment(None);
            url.set_query(None);
            Ok(SourceId::for_git(&url, reference))
        }
        _ => bail!("the source {source} is not one blaze reads yet"),
    }
}

/// `[source]` and `[registries]` as blaze-registry takes them.
fn source_config(config: &Config) -> Result<blaze_registry::SourceConfig> {
    use blaze_manifest::source::SourceId;
    use cargo_util_schemas::core::{GitReference, SourceKind};
    let mut out = blaze_registry::SourceConfig::default();
    for (name, def) in config.source_configs()? {
        let (replace_with, definition) = def.replace_with.clone().unzip();
        let mut it = blaze_registry::SourceDef {
            replace_with,
            definition: definition.unwrap_or_default(),
            ..Default::default()
        };
        if name == blaze_resolve::CRATES_IO_REGISTRY {
            if it.replace_with.is_some() {
                out.sources.push((name, it));
            }
            continue;
        }
        let SourceId { kind, url, .. } = &def.id;
        match kind {
            SourceKind::Registry | SourceKind::SparseRegistry => {
                it.registry = Some(url.to_string());
            }
            SourceKind::LocalRegistry => it.local_registry = url.to_file_path().ok(),
            SourceKind::Directory => it.directory = url.to_file_path().ok(),
            SourceKind::Git(reference) => {
                it.git = Some(url.to_string());
                match reference {
                    GitReference::Branch(b) => it.branch = Some(b.clone()),
                    GitReference::Tag(t) => it.tag = Some(t.clone()),
                    GitReference::Rev(r) => it.rev = Some(r.clone()),
                    GitReference::DefaultBranch => {}
                }
            }
            _ => bail!("`source.{name}` is not a source blaze reads yet"),
        }
        out.sources.push((name, it));
    }
    let registries: Option<BTreeMap<String, RegistryConfig>> = config.get("registries")?;
    for name in registries.into_iter().flatten().map(|(name, _)| name) {
        if name == blaze_resolve::CRATES_IO_REGISTRY {
            continue;
        }
        let index = config.registry_index(&name)?;
        out.registries.insert(name, index.to_string());
    }
    out.crates_io_git =
        config.get_string("registries.crates-io.protocol")?.is_some_and(|it| it.val == "git");
    Ok(out)
}
