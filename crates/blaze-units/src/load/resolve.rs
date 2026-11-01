//! The workspace resolved as cargo's `resolve_ws_with_opts` resolves it (blaze-metadata's
//! `resolving_with`, every file read through the load's [`Snap`]): the lock file read, each
//! package it pins located where cargo keeps it ([`Sources::locate`], a fetch when it is not
//! mounted), the whole workspace resolved guided by the lock file and the build's packages by
//! that, the lock file as cargo would write it, and the packages the resolve picked that the
//! lock file did not pin located in turn.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use blaze_manifest::config::schema::FeatureUnification;
use blaze_manifest::files::Files;
use blaze_manifest::manifest::{EitherManifest, Manifest, read_manifest};
use blaze_manifest::metadata::export_package;
use blaze_manifest::{Context, Workspace};
use blaze_query::Cancelled;
use blaze_resolve::lockfile::{self, PathDeps};
use blaze_resolve::resolver::{CliFeatures, HasDevUnits, Members, ResolveBehavior, VersionOrdering, members_with_features};
use blaze_resolve::workspace_resolve::{PackageRegistry, resolve_ws};
use blaze_resolve::{
    Dependency, IndexSummary, MemRegistry, PackageId, PackageIdSpec, QueryKind, Registry, Resolve,
    ResolveVersion, RustVersion, SourceId, Summary,
};
use blaze_hash::HashMap;

use super::files::Snap;
use super::source::{Index, Located, Sources};
use super::{Select, Spec, Staged, load_error};
use crate::query::{Fetch, LockWrite, LoadError};
use crate::read::{
    self as summary, Package, Read, feature_unification, manifest_source, path_packages, rust_versions,
};

/// The workspace resolved, as the unit graph is made from it.
pub(crate) struct Resolved<'a> {
    /// The resolve of the build (cargo's `targeted_resolve`).
    pub resolve: Resolve,
    /// The members' ids, in the workspace's order.
    pub member_ids: Vec<PackageId>,
    /// The packages selected (`-p`, `--workspace`, the default members), as specs.
    pub specs: Vec<PackageIdSpec>,
    /// The members whose features are resolved, with theirs from the command line.
    pub feature_members: Vec<(Summary, CliFeatures)>,
    pub cli_features: CliFeatures,
    pub unification: FeatureUnification,
    /// The member whose manifest is the current one, and cargo's
    /// `allows_new_cli_feature_behavior`: what [`Members`] is made of with the members'
    /// summaries.
    pub current: Option<PackageId>,
    pub new_cli_feature_behavior: bool,
    pub behavior: ResolveBehavior,
    pub has_dev_units: HasDevUnits,
    pub reads: HashMap<PackageId, Read>,
    /// The manifest of every package of the resolve, and where it is.
    pub manifests: HashMap<PackageId, (&'a Path, &'a Manifest)>,
    pub lock: Option<LockWrite>,
    /// What cargo warns of while selecting the packages.
    pub warnings: Vec<String>,
}

/// Resolves the workspace `ws` as `spec` asks and hands the result to `then`. Every file is
/// read through `ctx`'s [`Snap`], and every query asked through its `Cx`.
pub(crate) fn resolved<'r, 'db, R>(
    ctx: &Context<Snap<'r, 'db>>,
    ws: &Workspace<'_, Snap<'r, 'db>>,
    spec: &Spec,
    then: impl FnOnce(Resolved<'_>) -> Result<Staged<R>, Cancelled>,
) -> Result<Staged<R>, Cancelled> {
    let files = *ctx.files();
    let at = Some(ws.root_manifest().display().to_string());
    let fail = |error: anyhow::Error| Staged::Failed(load_error(&error, at.clone()));
    let root = ws.root().to_owned();
    let root_text = match files.read(&root.join("Cargo.toml")).map(|bytes| String::from_utf8(bytes.into_owned())) {
        Ok(Ok(text)) => text,
        Ok(Err(error)) => return Ok(fail(anyhow::Error::new(error).context(format!("failed to read `{}`", ws.root_manifest().display())))),
        Err(error) => return Ok(fail(anyhow::Error::new(error).context(format!("failed to read `{}`", ws.root_manifest().display())))),
    };
    let root_table = match summary::Root::new(&root, &root_text) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let lock_path = root.join("Cargo.lock");
    let lock_text = match files.read(&lock_path) {
        Ok(bytes) => match String::from_utf8(bytes.into_owned()) {
            Ok(text) => Some(text),
            Err(error) => {
                return Ok(fail(anyhow::Error::new(error).context(format!("failed to read `{}`", lock_path.display()))));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Ok(fail(anyhow::Error::new(error).context(format!("failed to read `{}`", lock_path.display()))));
        }
    };
    let prepared = match prepare(ws, &root_table, ctx, lock_text.as_deref()) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let Prepared { replace, patch, path_packages, path_deps, lock } = prepared;
    let offline = match ctx.config.net_config() {
        Ok(net) => spec.offline.or(net.offline.unwrap_or(false).then_some("--offline")),
        Err(error) => return Ok(fail(error)),
    };
    let sources = match Sources::new(&ctx.config, offline) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };

    // The packages the lock file pins, located and read.
    let mut owned: Vec<Package> = path_packages;
    if let Some(lock) = &lock {
        match locate_all(ctx, &sources, &locked_ids(lock), lock.checksums())? {
            Staged::Done(mut read) => owned.append(&mut read),
            Staged::Needs(fetches) => return Ok(Staged::Needs(fetches)),
            Staged::Failed(error) => return Ok(Staged::Failed(error)),
        }
    }
    let members: Vec<(&Path, &Manifest)> = ws.members().collect();
    let known = members.len() + owned.len();
    let mut reads: HashMap<PackageId, Read> = HashMap::with_capacity_and_hasher(known, Default::default());
    // Of each of `members` then `owned`, in order, its id.
    let mut ids: Vec<PackageId> = Vec::with_capacity(known);
    let all = members.iter().map(|(path, m)| (*path, *m, None)).chain(
        owned.iter().map(|it| (it.manifest_path.as_path(), &*it.manifest, it.locked_source.as_deref())),
    );
    for (path, manifest, source) in all {
        match read_of(path, manifest, source, lock.as_ref()) {
            Ok(read) => {
                ids.push(read.summary.package_id());
                reads.insert(read.summary.package_id(), read);
            }
            Err(error) => return Ok(fail(error)),
        }
    }
    let member_ids = ids[..members.len()].to_vec();

    let mut pinned = MemRegistry::new();
    for read in reads.values() {
        pinned.add(IndexSummary::Candidate(read.summary.clone()));
    }
    let by_path: HashMap<SourceId, Summary> = reads
        .values()
        .filter(|it| it.summary.source_id().is_path())
        .map(|it| (it.summary.source_id(), it.summary.clone()))
        .collect();
    let member_summaries: Vec<Summary> = member_ids.iter().map(|id| reads[id].summary.clone()).collect();
    let member_refs: Vec<&Summary> = member_summaries.iter().collect();
    let current = members.iter().position(|(path, _)| *path == ws.current_manifest()).map(|at| member_ids[at]);
    let behavior = ws.resolve_behavior();
    let new_cli_feature_behavior = behavior != ResolveBehavior::V1 || ws.is_virtual();
    let members_of = || Members { members: &member_refs, current, new_cli_feature_behavior };
    let member_rust_versions: Vec<RustVersion> = match members
        .iter()
        .filter_map(|(_, m)| m.metadata.rust_version.as_ref())
        .map(|it| it.to_string().parse())
        .collect::<anyhow::Result<_>>()
    {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let lowest_rust_version = member_rust_versions.iter().min().cloned();
    let rust_versions = match rust_versions(ctx, behavior, &member_rust_versions, Some(spec.rustc_version.as_str())) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let resolving = blaze_resolve::workspace_resolve::Workspace {
        members: members_of(),
        path_packages: &by_path,
        patch: &patch,
        replace: &replace,
        require_optional_deps: spec.require_optional_deps,
        version_ordering: VersionOrdering::MaximumVersionsFirst,
        rust_versions,
        resolve_version: ResolveVersion::with_rust_version(lowest_rust_version.as_ref()),
        offline_flag: offline,
    };
    let mut warnings = Vec::new();
    let specs = match selected(ws, &member_ids, &spec.select, &mut warnings) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let cli_features = match CliFeatures::from_command_line(&spec.features, spec.all_features, !spec.no_default_features) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let has_dev_units = match spec.filter.need_dev_deps(spec.intent) {
        true => HasDevUnits::Yes,
        false => HasDevUnits::No,
    };
    let unification = match feature_unification(&ctx.config) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let every_member: Vec<PackageIdSpec> = member_ids.iter().map(|id| id.to_spec()).collect();
    let resolved_for = match unification {
        FeatureUnification::Workspace => &every_member,
        FeatureUnification::Selected | FeatureUnification::Package => &specs,
    };

    let (result, index_fetches) = files.with_cx(|cx| {
        let index = Index::new(&sources, cx, pinned);
        let result = resolve_ws(
            &mut PackageRegistry::new(Through(&index)),
            &resolving,
            lock.as_ref(),
            &cli_features,
            resolved_for,
            has_dev_units,
        );
        index.finish().map(|fetches| (result, fetches))
    })?;
    if !index_fetches.is_empty() {
        return Ok(Staged::Needs(index_fetches));
    }
    let (mut whole, targeted) = match result {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    if let Some(lock) = &lock
        && let Err(error) = whole.merge_from(lock)
    {
        return Ok(fail(error));
    }

    // The registry packages the resolve picked that the lock file does not pin.
    let unread: Vec<PackageId> = locked_ids(&whole).into_iter().filter(|id| !reads.contains_key(id)).collect();
    match locate_all(ctx, &sources, &unread, whole.checksums())? {
        Staged::Done(picked) => {
            for package in &picked {
                match read_of(&package.manifest_path, &package.manifest, package.locked_source.as_deref(), Some(&whole)) {
                    Ok(read) => {
                        ids.push(read.summary.package_id());
                        reads.insert(read.summary.package_id(), read);
                    }
                    Err(error) => return Ok(fail(error)),
                }
            }
            owned.extend(picked);
        }
        Staged::Needs(fetches) => return Ok(Staged::Needs(fetches)),
        Staged::Failed(error) => return Ok(Staged::Failed(error)),
    }

    let locked = spec.locked.map(|flag| lockfile::Locked { flag, path: &lock_path, path_deps: &path_deps });
    let lock_write = match lockfile::lockfile_to_write(&mut whole, lock_text.as_deref(), lowest_rust_version.as_ref(), locked) {
        Ok(text) => text.map(|text| LockWrite { path: lock_path.display().to_string(), text }),
        Err(error) => return Ok(fail(error)),
    };

    let feature_members = match feature_roots(&members_of(), &member_ids, &specs, &cli_features, unification) {
        Ok(it) => it,
        Err(error) => return Ok(fail(error)),
    };
    let all = members.iter().map(|(path, m)| (*path, *m)).chain(owned.iter().map(|it| (it.manifest_path.as_path(), &*it.manifest)));
    let manifests: HashMap<PackageId, (&Path, &Manifest)> = ids.iter().copied().zip(all).collect();
    if let Some(missing) = targeted.iter().find(|id| !manifests.contains_key(id)) {
        return Ok(fail(anyhow::anyhow!("no manifest of `{missing}` was read")));
    }
    then(Resolved {
        resolve: targeted,
        member_ids,
        specs,
        feature_members,
        cli_features,
        unification,
        current,
        new_cli_feature_behavior,
        behavior,
        has_dev_units,
        reads,
        manifests,
        lock: lock_write,
        warnings,
    })
}

/// A [`Registry`] by reference: the resolve borrows the index, which is finished after it.
struct Through<'i, R>(&'i R);

impl<R: Registry> Registry for Through<'_, R> {
    async fn query(&self, dep: &Dependency, kind: QueryKind, f: &mut impl FnMut(IndexSummary)) -> anyhow::Result<()> {
        self.0.query(dep, kind, f).await
    }

    fn describe_source(&self, source: SourceId) -> String {
        self.0.describe_source(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        self.0.is_replaced(source)
    }
}

struct Prepared {
    replace: Vec<(PackageIdSpec, Dependency)>,
    patch: Vec<(url::Url, Vec<blaze_resolve::workspace_resolve::Patch>)>,
    path_packages: Vec<Package>,
    path_deps: PathDeps,
    lock: Option<Resolve>,
}

fn prepare<F: Files>(
    ws: &Workspace<'_, F>,
    root: &summary::Root<'_>,
    ctx: &Context<F>,
    lock_text: Option<&str>,
) -> anyhow::Result<Prepared> {
    let replace = summary::replace_table(root)?;
    let patch = summary::patch_tables(root)?;
    let path_packages = path_packages(ws, root, &replace, ctx)?;
    let mut path_deps = PathDeps::default();
    for m in ws.members().map(|(_, m)| m).chain(path_packages.iter().map(|it| &*it.manifest)) {
        let dir = m.package_id.source_id.local_path().context("a path package's directory")?;
        let entry = path_deps.entry(m.package_id.name.to_string()).or_default();
        entry.insert(m.package_id.version.clone(), SourceId::for_path(&dir)?);
    }
    let lock = match lock_text {
        Some(text) => Some(lockfile::into_resolve(lockfile::parse(text)?, text, &path_deps)?),
        None => None,
    };
    Ok(Prepared { replace, patch, path_packages, path_deps, lock })
}

/// What the resolve reads of a package, its checksum the lock file's.
fn read_of(path: &Path, manifest: &Manifest, source: Option<&str>, lock: Option<&Resolve>) -> anyhow::Result<Read> {
    let mut package = export_package(path, manifest);
    if let Some(source) = source {
        package.set_source(source.to_owned());
    }
    let mut read = summary::read(&package)?;
    let id = read.summary.package_id();
    if let Some(Some(checksum)) = lock.and_then(|it| it.checksums().get(&id)) {
        read.summary.set_checksum(checksum.clone());
    }
    Ok(read)
}

/// Every package of `lock` that is not a path package, the `[[patch.unused]]` entries too,
/// which the resolve has to see to keep them unused.
fn locked_ids(lock: &Resolve) -> Vec<PackageId> {
    let ids = lock.sort().into_iter().chain(lock.unused_patches().iter().copied());
    ids.filter(|id| !id.source_id().is_path()).collect()
}

/// The packages `ids` located where cargo keeps them and read from their manifests; every one
/// not mounted is a fetch. Each manifest is its `ManifestQuery`, parsed once whichever step
/// reads it.
fn locate_all(
    ctx: &Context<Snap<'_, '_>>,
    sources: &Sources,
    ids: &[PackageId],
    checksums: &HashMap<PackageId, Option<String>>,
) -> Result<Staged<Vec<Package>>, Cancelled> {
    let files = *ctx.files();
    let mut here = Vec::new();
    let mut needs: Vec<Fetch> = Vec::new();
    for &id in ids {
        let checksum = checksums.get(&id).and_then(Option::as_deref);
        match files.with_cx(|cx| sources.locate(cx, id, checksum))? {
            Located::Here { manifest } => here.push((id, manifest)),
            Located::Needs(fetch) => needs.push(fetch),
            Located::Failed(error) => return Ok(Staged::Failed(error)),
        }
    }
    if !needs.is_empty() {
        return Ok(Staged::Needs(needs));
    }
    let mut read = Vec::with_capacity(here.len());
    for (id, manifest_path) in here {
        let source_text = id.source_id().as_url().to_string();
        let manifest_path = PathBuf::from(manifest_path);
        let at = Some(manifest_path.display().to_string());
        let outcome = manifest_source(&source_text, &ctx.config)
            .and_then(|source| read_manifest(&manifest_path, source, ctx));
        match outcome {
            Ok(EitherManifest::Real(manifest)) => {
                read.push(Package { manifest_path, manifest, locked_source: Some(source_text) });
            }
            Ok(EitherManifest::Virtual(_)) => {
                let error = anyhow::anyhow!("{} has no package", manifest_path.display());
                return Ok(Staged::Failed(load_error(&error, at)));
            }
            Err(error) => return Ok(Staged::Failed(load_error(&error, at))),
        }
    }
    Ok(Staged::Done(read))
}

/// The members `select` selects, as cargo's `Packages::from_flags` and
/// `Packages::to_package_id_specs` select them.
fn selected(
    ws: &Workspace<'_, Snap>,
    member_ids: &[PackageId],
    select: &Select,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<PackageIdSpec>> {
    let pattern = |name: &str| name.contains(['*', '?', '[', ']']);
    let specs: Vec<PackageIdSpec> = match select {
        Select::All => member_ids.iter().map(|id| id.to_spec()).collect(),
        Select::OptOut(excludes) => {
            if let Some(it) = excludes.iter().find(|it| pattern(it)) {
                anyhow::bail!("blaze does not read package patterns in `--exclude` yet (`{it}`)");
            }
            let ids: Vec<PackageIdSpec> =
                excludes.iter().map(|it| PackageIdSpec::parse(it)).collect::<anyhow::Result<_>>()?;
            let not_found: Vec<String> = ids
                .iter()
                .filter(|spec| !member_ids.iter().any(|id| spec.matches(*id)))
                .map(|spec| spec.to_string())
                .collect();
            if !not_found.is_empty() {
                warnings.push(format!(
                    "excluded package(s) `{}` not found in workspace `{}`",
                    not_found.join(", "),
                    ws.root().display()
                ));
            }
            let excluded = |id: &PackageId| ids.iter().any(|spec| spec.matches(*id));
            member_ids.iter().filter(|id| !excluded(id)).map(|id| id.to_spec()).collect()
        }
        Select::Packages(names) => {
            if let Some(it) = names.iter().find(|it| pattern(it)) {
                anyhow::bail!("blaze does not read package patterns in `--package` yet (`{it}`)");
            }
            let mut specs: Vec<(String, PackageIdSpec)> = names
                .iter()
                .map(|name| PackageIdSpec::parse(name).map(|spec| (spec.to_string(), spec)))
                .collect::<anyhow::Result<_>>()?;
            specs.sort_by(|a, b| a.0.cmp(&b.0));
            specs.dedup_by(|a, b| a.0 == b.0);
            specs.into_iter().map(|(_, spec)| spec).collect()
        }
        Select::Default => {
            let named = |name: &str| member_ids.iter().find(|id| id.name().as_str() == name).copied();
            let defaults = ws.default_members().map(|(_, m)| named(&m.package_id.name));
            let ids: Vec<PackageId> = defaults.collect::<Option<_>>().context("a default member that is not a member")?;
            ids.iter().map(|id| id.to_spec()).collect()
        }
    };
    if specs.is_empty() {
        if ws.is_virtual() {
            anyhow::bail!(
                "manifest path `{}` contains no package: The manifest is virtual, and the workspace has no members.",
                ws.root().display()
            );
        }
        anyhow::bail!("no packages to compile");
    }
    Ok(specs)
}

/// The members the build resolves features for, with theirs from the command line, as cargo's
/// `resolve_ws_with_opts` makes its one feature resolve. `resolver.feature-unification =
/// "package"` makes one per package, so a package built for two of them with other features
/// each; blaze builds one copy of each package per build, so that is refused for several.
fn feature_roots(
    members: &Members<'_>,
    member_ids: &[PackageId],
    specs: &[PackageIdSpec],
    cli_features: &CliFeatures,
    unification: FeatureUnification,
) -> anyhow::Result<Vec<(Summary, CliFeatures)>> {
    let every: Vec<PackageIdSpec>;
    let resolved_for = match unification {
        FeatureUnification::Workspace => {
            every = member_ids.iter().map(|id| id.to_spec()).collect();
            &every[..]
        }
        FeatureUnification::Selected => specs,
        FeatureUnification::Package if specs.len() == 1 => specs,
        FeatureUnification::Package => {
            let named: Vec<String> = specs.iter().map(|it| format!("`{}`", it.name())).collect();
            anyhow::bail!(
                "blaze does not build several packages with `resolver.feature-unification = \
                 \"package\"` yet: it resolves the features of {} apart, and blaze builds each \
                 package once per build; build them one at a time with `-p`",
                named.join(", ")
            );
        }
    };
    let found = members_with_features(members, resolved_for, cli_features)?;
    Ok(found.into_iter().map(|(summary, features)| (summary.clone(), features)).collect())
}