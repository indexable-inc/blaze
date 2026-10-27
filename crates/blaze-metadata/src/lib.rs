//! `cargo metadata --format-version 1` with its `resolve`, made by blaze with no cargo: the
//! workspace read by blaze-manifest, every package `Cargo.lock` locks fetched by
//! blaze-registry where cargo would have it and read from its manifest, and the two resolves
//! of cargo's `resolve_ws_with_opts` made by blaze-resolve, guided by the lock file. Either
//! the whole workspace's graph, as `cargo metadata` prints it, or that of one build
//! ([`Build`]), whose features are a build's and not unified over every member
//! (docs/self-contained.md, "Resolution and fetching").
//!
//! A lock file that is missing or that the manifests no longer agree with is written again,
//! as cargo writes it, when the resolve needs no version the lock file does not have. What
//! it does not do yet is an error: a registry package at a version the lock file does not
//! name (choosing versions needs the index, #321), `[replace]`, a registry with a git
//! index, and a dependency of a registry package whose `.crate` cannot be fetched.

mod graph;
mod summary;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use blaze_manifest::config::RegistryConfig;
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
    Cfg, IndexSummary, InternedString, MemRegistry, PackageId, PackageIdSpec, Platform,
    ResolveVersion, SourceId, Summary,
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Kind {
    Lib,
    Bin,
    Example,
    Test,
    /// The `cdylib` of a library, which `cargo build` links as a shared library (#262).
    Cdylib,
    /// The `staticlib` of a library, an archive `cargo build` writes.
    Staticlib,
}

impl Kind {
    /// Whether it is a library's linked output, a `cdylib` or a `staticlib`, and not an
    /// executable.
    pub fn linked_library(self) -> bool {
        matches!(self, Kind::Cdylib | Kind::Staticlib)
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
            Kind::Cdylib | Kind::Staticlib => false,
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
            // A build links a library's `cdylib` and `staticlib`, each a program of its own;
            // its `rlib` is linked into the others.
            let kinds: Vec<Kind> = match (kind, targets.test) {
                (Kind::Lib, None) => [("cdylib", Kind::Cdylib), ("staticlib", Kind::Staticlib)]
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

/// The workspace of an [`Asked`] resolved as cargo's `resolve_ws_with_opts` resolves it, before
/// the feature resolver: what `cargo metadata` and `cargo tree` print from.
pub struct Resolved<'a> {
    pub ws: &'a Workspace<'a>,
    /// The resolve of the packages `specs` select, guided by the lock file.
    pub resolve: &'a blaze_resolve::Resolve,
    pub members: Members<'a>,
    /// The members' ids, in the workspace's order.
    pub member_ids: &'a [PackageId],
    pub specs: &'a [PackageIdSpec],
    pub cli_features: &'a CliFeatures,
    pub behavior: ResolveBehavior,
    pub has_dev_units: HasDevUnits,
    /// What was read of every package the lock file names.
    pub packages: &'a FxHashMap<PackageId, Read>,
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
        Ok(fetch(lock, ctx, offline, asked.store.clone(), true)?.len())
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
    if summary::has_replace(&root)? {
        bail!("`[replace]` is not read by blaze yet");
    }
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
    let fetched = match &lock {
        Some(lock) => fetch(lock, &ctx, offline, asked.store.clone(), respect)?,
        None => Vec::new(),
    };

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
    let patch = summary::patch_tables(&root, &registry)?;
    let by_path: FxHashMap<SourceId, Summary> = reads
        .values()
        .filter(|it| it.summary.source_id().is_path())
        .map(|it| (it.summary.source_id(), it.summary.clone()))
        .collect();
    let member_ids = &ids[..member_count];
    let members: Vec<&Summary> = member_ids.iter().map(|id| &reads[id].summary).collect();
    let current =
        ws.members().position(|(path, _)| path == ws.current_manifest()).map(|at| member_ids[at]);
    let behavior = match ws.resolve_behavior() {
        blaze_manifest::features::ResolveBehavior::V1 => ResolveBehavior::V1,
        blaze_manifest::features::ResolveBehavior::V2 => ResolveBehavior::V2,
        blaze_manifest::features::ResolveBehavior::V3 => ResolveBehavior::V3,
    };
    let new_cli_feature_behavior = behavior != ResolveBehavior::V1 || ws.is_virtual();
    let members_of = || Members { members: &members, current, new_cli_feature_behavior };
    let resolving = blaze_resolve::workspace_resolve::Workspace {
        members: members_of(),
        path_packages: &by_path,
        patch: &patch,
        replace: &[],
        require_optional_deps: true,
        version_ordering: VersionOrdering::MaximumVersionsFirst,
        rust_versions: None,
        resolve_version: ResolveVersion::default(),
    };
    let specs: Vec<PackageIdSpec> = match (pick, &asked.build) {
        (Some(pick), _) => pick(&ws, member_ids)?,
        (None, None) => member_ids.iter().map(|id| id.to_spec()).collect(),
        (None, Some(build)) => selected(&ws, member_ids, build)?,
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
    let (mut whole, targeted) = resolve_ws(
        &mut PackageRegistry::new(registry),
        &resolving,
        lock.as_ref(),
        &cli_features,
        &specs,
        has_dev_units,
    )
    .context("blaze picks a registry package only at the version Cargo.lock has (#321)")?;
    if let Some(lock) = &lock {
        whole.merge_from(lock)?;
    }
    match relock {
        None => {
            // A new lock file has the newest encoding, whatever the members' `rust-version`.
            if let Some(text) =
                lockfile::lockfile_to_write(&mut whole, lock_text.as_deref(), None, None)?
            {
                std::fs::write(&lock_path, text)
                    .with_context(|| format!("writing {}", lock_path.display()))?;
            }
        }
        Some(relock) => {
            let lowest = ws
                .members()
                .filter_map(|(_, m)| m.metadata.rust_version.as_ref())
                .filter_map(|it| it.to_string().parse::<blaze_resolve::RustVersion>().ok())
                .min();
            let locked = relock.locked.map(|flag| lockfile::Locked {
                flag,
                path: &lock_path,
                path_deps: &path_deps,
            });
            if let Some(text) = lockfile::lockfile_to_write(
                &mut whole,
                lock_text.as_deref(),
                lowest.as_ref(),
                locked,
            )? {
                std::fs::write(&lock_path, text)
                    .with_context(|| format!("failed to write {}", lock_path.display()))?;
            }
        }
    }
    let unused_patches = whole.unused_patches().to_vec();
    let resolved = Resolved {
        ws: &ws,
        resolve: &targeted,
        members: members_of(),
        member_ids,
        specs: &specs,
        cli_features: &cli_features,
        behavior,
        has_dev_units,
        packages: &reads,
    };
    then(resolved, serialized, &ids, &unused_patches)
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
        specs,
        cli_features,
        behavior,
        has_dev_units,
        packages: reads,
        ..
    } = *resolved;
    let current = resolved.members.current;
    let members_of = || resolved.members;

    let kinds = match &asked.platform {
        Some(platform) => vec![CompileKind::Target(InternedString::new(platform.triple))],
        None => vec![CompileKind::Host],
    };
    let packages = Packages { reads, platform: asked.platform.as_ref() };
    let mut roots = Vec::new();
    let resolve = match &asked.build {
        None => resolve_graph(targeted, member_ids, current, &packages, &kinds)?,
        Some(build) => {
            let member_features = members_with_features(&members_of(), specs, cli_features)?;
            let opts = FeatureOpts::new(behavior, has_dev_units, ForceAllTargets::No);
            let features =
                FeatureResolver::resolve(&packages, targeted, &member_features, &kinds, opts)?;
            roots = member_features.iter().map(|(it, _)| it.package_id()).collect();
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
}

impl Packages<'_> {
    fn activates(&self, platform: &Platform) -> bool {
        self.platform.is_none_or(|it| it.activates(platform))
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

    fn platform_activated(&self, platform: &Platform, _kind: CompileKind) -> bool {
        self.activates(platform)
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

/// The path packages that are not members: those the members and the root's `[patch]` name
/// by path, and theirs in turn.
fn path_packages(ws: &Workspace<'_>, root: &Path, ctx: &Context) -> Result<Vec<Package>> {
    let members: Vec<PathBuf> =
        ws.members().filter_map(|(path, _)| path.parent().map(Path::to_owned)).collect();
    let mut queue = summary::patch_paths(root)?;
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

/// Every package of `lock` that is not a path package, fetched where cargo would have it and
/// read from its manifest.
fn fetch(
    lock: &blaze_resolve::Resolve,
    ctx: &Context,
    offline: bool,
    store: Option<Arc<dyn blaze_registry::Blobs>>,
    respect_source_config: bool,
) -> Result<Vec<Package>> {
    let sources = sources(ctx, offline, store, respect_source_config)?;
    let mut locked = Vec::new();
    let mut ids = Vec::new();
    // The `[[patch.unused]]` entries too, which the resolve has to see to keep them unused.
    for id in lock.sort().into_iter().chain(lock.unused_patches().iter().copied()) {
        if id.source_id().is_path() {
            continue;
        }
        let source_text = id.source_id().as_url().to_string();
        let (source, precise) = blaze_registry::SourceId::from_url(&source_text)?;
        let from = sources.map().replacement(&source, precise.is_some())?;
        if from.is_remote_registry() && !from.is_sparse() {
            bail!("{source_text} is read from a git index, which blaze does not read yet");
        }
        let mut checksum = lock.checksums().get(&id).cloned().flatten().unwrap_or_default();
        // A lock file of cargo before 1.53 may name no checksum: the index's.
        if checksum.is_empty() && from.is_remote_registry() {
            checksum = sources.registry(&from)?.checksum(id.name().as_str(), id.version())?;
        }
        let package = blaze_registry::Package {
            name: id.name().to_string(),
            version: id.version().clone(),
            checksum,
        };
        locked.push(blaze_registry::Locked { source, precise, package });
        ids.push((id, source_text));
    }
    let roots = sources.fetch(&locked)?;
    let mut read = Vec::with_capacity(ids.len());
    for ((_, source_text), (dir, _)) in ids.into_iter().zip(roots) {
        let manifest_path = dir.join("Cargo.toml");
        let source = manifest_source(&source_text, &ctx.config)?;
        let EitherManifest::Real(manifest) = read_manifest(&manifest_path, source, ctx)? else {
            bail!("{} has no package", manifest_path.display());
        };
        read.push(Package { manifest_path, manifest, locked_source: Some(source_text) });
    }
    Ok(read)
}

/// The sources packages are fetched from, `[source]` replacing those it names unless not
/// `respect_source_config`.
fn sources(
    ctx: &Context,
    offline: bool,
    store: Option<Arc<dyn blaze_registry::Blobs>>,
    respect_source_config: bool,
) -> Result<blaze_registry::Sources> {
    let mut context = blaze_registry::Context::new(
        ctx.config.home().to_owned(),
        blaze_registry::http::HyperHttp::shared()?,
    );
    context.net.offline = offline;
    context.store = store;
    let mut config = source_config(&ctx.config)?;
    if !respect_source_config {
        config.sources.clear();
    }
    blaze_registry::Sources::new(&config, Arc::new(context))
}

/// The sources a registry's web API command finds its registry in: cargo's `SourceConfigMap`
/// of the configuration (`[source]` replacing others only with `respect_source_config`, as
/// `ops::registry` reads it both ways), offline as `net.offline` says.
pub fn registry_sources(
    ctx: &Context,
    respect_source_config: bool,
) -> Result<blaze_registry::Sources> {
    let offline = ctx.config.net_config()?.offline.unwrap_or(false);
    sources(ctx, offline, None, respect_source_config)
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
        let sources = sources(ctx, offline, asked.store.clone(), respect_source_config)?;
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
            let source_text = id.source_id().as_url().to_string();
            let (source, precise) = blaze_registry::SourceId::from_url(&source_text)?;
            let from = sources.map().replacement(&source, precise.is_some())?;
            use blaze_registry::SourceKind;
            match from.kind() {
                SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry => {}
                SourceKind::Git(_) => bail!(
                    "blaze does not vendor `{id}` yet: a git package is vendored with its \
                     manifest as `cargo package` writes it, which blaze does not write yet"
                ),
                _ => bail!("blaze does not vendor `{id}` yet: it is read from {from}"),
            }
            if from.is_remote_registry() && !from.is_sparse() {
                bail!("{source_text} is read from a git index, which blaze does not read yet");
            }
            let registry = sources.registry(&from)?;
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
            locked.push(blaze_registry::Locked { source, precise, package });
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
        if name == blaze_registry::CRATES_IO_REGISTRY {
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
        if name == blaze_registry::CRATES_IO_REGISTRY {
            continue;
        }
        let index = config.registry_index(&name)?;
        out.registries.insert(name, index.to_string());
    }
    out.crates_io_git =
        config.get_string("registries.crates-io.protocol")?.is_some_and(|it| it.val == "git");
    Ok(out)
}
