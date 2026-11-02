//! Resolving a workspace as cargo 1.99.0 (5f94df4789) does around its resolver: the
//! `PackageRegistry` of `src/workspace/registry.rs` over sources given as a [`Registry`],
//! which applies `[patch]` and the locks of a previous resolve to the summaries it hands out,
//! and `resolve_with_previous` and the two resolves of `resolve_ws_with_opts`, from
//! `src/ops/resolve.rs`.
//!
//! Path overrides (`paths` in `.cargo/config.toml`) are not here, and the warnings cargo
//! prints along the way (unused patches, fields a patch or `[replace]` ignores) are not
//! written.

use anyhow::{Context as _, Result, anyhow};
use blaze_hash::{HashMap, HashSet};
use tracing::{debug, trace};
use url::Url;

use crate::dependency::Dependency;
use crate::interning::InternedString;
use crate::package_id::PackageId;
use crate::package_id_spec::PackageIdSpec;
use crate::poll::block_on;
use crate::registry::{IndexSummary, QueryKind, Registry};
use crate::resolve::{Resolve, ResolveVersion};
use crate::resolver::{
    self, CliFeatures, HasDevUnits, Members, ResolveOpts, ResolveSettings, VersionOrdering,
    VersionPreferences, features::RequestedFeatures, members_with_features,
};
use crate::source_id::{CanonicalUrl, GitReference, SourceId};
use crate::summary::Summary;
use crate::version::PartialVersion;

/// One entry of a `[patch]` table.
#[derive(Clone, Debug)]
pub struct Patch {
    pub dep: Dependency,
    /// The manifest it is written in, for messages.
    pub loc: String,
}

/// Argument to [`PackageRegistry::patch`] which is information about a `[patch]`
/// directive that we found in a lockfile, if present.
pub struct LockedPatchDependency {
    /// The original `Dependency` directive, except "locked" so it's version
    /// requirement is Locked to `foo` and its `SourceId` has a "precise" listed.
    pub dependency: Dependency,
    /// The `PackageId` that was previously found in a lock file which
    /// `dependency` matches.
    pub package_id: PackageId,
    /// Something only used for backwards compatibility with the v2 lock file
    /// format where `branch=master` is considered the same as `DefaultBranch`.
    /// For more comments on this see the code in `ops/resolve.rs`.
    pub alt_package_id: Option<PackageId>,
}

/// This tuple is an argument to [`PackageRegistry::patch`].
///
/// * The first element is the patch definition straight from the manifest.
/// * The second element is an optional variant where the patch has been locked.
///   It is the patch locked to a specific version found in Cargo.lock.
///   This will be `None` if `Cargo.lock` doesn't exist,
///   or the patch did not match any existing entries in `Cargo.lock`.
pub type PatchDependency<'a> = (&'a Patch, Option<LockedPatchDependency>);

/// A map of all "locked packages" which is filled in when parsing a lock file
/// and is used to guide dependency resolution by altering summaries as they're
/// queried from this source.
///
/// This map can be thought of as a glorified `Vec<MySummary>` where `MySummary`
/// has a `PackageId` for which package it represents as well as a list of
/// `PackageId` for the resolved dependencies. The hash map is otherwise
/// structured though for easy access throughout this registry.
type LockedMap = HashMap<
    // The first level of key-ing done in this hash map is the source that
    // dependencies come from, identified by a `SourceId`.
    // The next level is keyed by the name of the package...
    (SourceId, InternedString),
    // ... and the value here is a list of tuples. The first element of each
    // tuple is a package which has the source/name used to get to this
    // point. The second element of each tuple is the list of locked
    // dependencies that the first element has.
    Vec<(PackageId, Vec<PackageId>)>,
>;

/// The sources with `[patch]` and the locks of a previous resolve applied: what the resolver
/// queries.
pub struct PackageRegistry<R> {
    sources: R,
    /// This is constructed via [`PackageRegistry::register_lock`].
    /// See also [`LockedMap`].
    locked: LockedMap,
    /// Patches registered during calls to [`PackageRegistry::patch`].
    ///
    /// These are available for `query` after calling [`PackageRegistry::lock_patches`],
    /// which `lock`s them all to specific versions.
    patches: HashMap<CanonicalUrl, Vec<Summary>>,
    /// Whether patches are locked. That is, they are available to resolution.
    ///
    /// See [`PackageRegistry::lock_patches`] and [`PackageRegistry::patch`] for more.
    patches_locked: bool,
    /// Patches available for each source.
    ///
    /// This is for determining whether a dependency entry from a lockfile
    /// happened through `[patch]`, during calls to [`lock`] to rewrite
    /// summaries to point directly at these patched entries.
    ///
    /// This is constructed during calls to [`PackageRegistry::patch`],
    /// along with the `patches` field, thoough these entries never get locked.
    patches_available: HashMap<CanonicalUrl, Vec<PackageId>>,
}

impl<R: Registry> PackageRegistry<R> {
    pub fn new(sources: R) -> PackageRegistry<R> {
        PackageRegistry {
            sources,
            locked: HashMap::default(),
            patches: HashMap::default(),
            patches_locked: false,
            patches_available: HashMap::default(),
        }
    }

    /// remove all residual state from previous lock files.
    pub fn clear_lock(&mut self) {
        trace!("clear_lock");
        self.locked = HashMap::default();
    }

    /// Registers one "locked package" to the registry, for guiding the
    /// dependency resolution. See [`LockedMap`] for more.
    pub fn register_lock(&mut self, id: PackageId, deps: Vec<PackageId>) {
        trace!("register_lock: {}", id);
        for dep in deps.iter() {
            trace!("\t-> {}", dep);
        }
        let sub_vec = self.locked.entry((id.source_id(), id.name())).or_default();
        sub_vec.push((id, deps));
    }

    /// Insert a `[patch]` section into this registry.
    ///
    /// This method will insert a `[patch]` section for the `url` specified,
    /// with the given list of dependencies. The `url` specified is the URL of
    /// the source to patch (for example this is `crates-io` in the manifest).
    /// The `deps` is an array of all the entries in the `[patch]` section of
    /// the manifest.
    ///
    /// Here the `patch_deps` will be resolved to a precise version and stored
    /// internally for future calls to `query` below.
    ///
    /// Note that the patch list specified here *will not* be available to
    /// [`Registry::query`] until [`PackageRegistry::lock_patches`] is called
    /// below, which should be called once all patches have been added.
    ///
    /// The return value is a `Vec` of patches that should *not* be locked.
    /// This happens when the patch is locked, but the patch has been updated
    /// so the locked value is no longer correct.
    pub fn patch(
        &mut self,
        url: &Url,
        patch_deps: &[PatchDependency<'_>],
    ) -> Result<Vec<(Patch, PackageId)>> {
        let canonical = CanonicalUrl::new(url)?;

        // Return value of patches that shouldn't be locked.
        let mut unlock_patches = Vec::new();

        // First up we need to actually resolve each `patch_deps` specification
        // to precisely one summary. We're not using the `query` method below
        // as it internally uses maps we're building up as part of this method
        // (`patches_available` and `patches`). Instead we're going straight to
        // the source to load information from it.
        //
        // Remember that each dependency listed in `[patch]` has to resolve to
        // precisely one package, so that's why we're just creating a flat list
        // of summaries which should be the same length as `deps` above.
        let mut unlocked_summaries = Vec::new();
        for (orig_patch, locked) in patch_deps {
            // Use the locked patch if it exists, otherwise use the original.
            let dep = match locked {
                Some(lock) => &lock.dependency,
                None => &orig_patch.dep,
            };
            debug!("registering a patch for `{}` with `{}`", url, dep.package_name());

            let summaries = block_on(self.sources.query_vec(dep, QueryKind::Exact))
                .with_context(|| format!("unable to update {}", dep.source_id()))
                .with_context(|| {
                    format!("failed to load source for dependency `{}`", dep.package_name())
                })?
                .into_iter()
                .filter_map(|s| match s {
                    IndexSummary::Candidate(s) => Some(s),
                    _ => None,
                })
                .collect();
            let (summary, should_unlock) =
                summary_for_patch(orig_patch, url, locked, summaries, &self.sources)?;
            debug!("patch summary is {:?} should_unlock={:?}", summary, should_unlock);
            if let Some(unlock_id) = should_unlock {
                unlock_patches.push(((*orig_patch).clone(), unlock_id));
            }

            if *summary.package_id().source_id().canonical_url() == canonical {
                return Err(anyhow!(
                    "patch for `{}` points to the same source, but patches must point to different sources\n\
                    help: check `{}` patch definition for `{}` in `{}`",
                    dep.package_name(),
                    dep.package_name(),
                    url,
                    orig_patch.loc
                ));
            }
            unlocked_summaries.push(summary);
        }

        let mut name_and_version = HashSet::default();
        for summary in unlocked_summaries.iter() {
            let name = summary.package_id().name();
            let version = summary.package_id().version();
            if !name_and_version.insert((name, version)) {
                let mut locations: Vec<String> = Vec::new();
                for (patch, _) in patch_deps.iter().filter(|p| p.0.dep.package_name() == name) {
                    let loc = format!("`{}`", patch.loc);
                    if !locations.contains(&loc) {
                        locations.push(loc);
                    }
                }
                return Err(anyhow!(
                    "several `[patch]` entries resolving to same version `{} v{}`\n\
                    help: check `{}` patch definitions for `{}` in {}",
                    name,
                    version,
                    name,
                    url,
                    locations.join(", ")
                ));
            }
        }

        // Calculate a list of all patches available for this source.
        let mut ids = Vec::new();
        for (summary, (_, lock)) in unlocked_summaries.iter().zip(patch_deps) {
            ids.push(summary.package_id());
            // This is subtle where the list of `ids` for a canonical URL is
            // extend with possibly two ids per summary. This is done to handle
            // the transition from the v2->v3 lock file format where in v2
            // DefaultBranch was either DefaultBranch or Branch("master") for
            // git dependencies. In this case if `summary.package_id()` is
            // Branch("master") then alt_package_id will be DefaultBranch. This
            // signifies that there's a patch available for either of those
            // dependency directives if we see them in the dependency graph.
            if let Some(lock) = lock {
                ids.extend(lock.alt_package_id);
            }
        }
        self.patches_available.insert(canonical.clone(), ids);

        // Note that we do not use `lock` here to lock summaries! That step
        // happens later once `lock_patches` is invoked. In the meantime though
        // we want to fill in the `patches_available` map (later used in the
        // `lock` method) and otherwise store the unlocked summaries in
        // `patches` to get locked in a future call to `lock_patches`.
        self.patches.insert(canonical, unlocked_summaries);

        Ok(unlock_patches)
    }

    /// Lock all patch summaries added via [`patch`](Self::patch),
    /// making them available to resolution via [`Registry::query`].
    pub fn lock_patches(&mut self) {
        assert!(!self.patches_locked);
        for summaries in self.patches.values_mut() {
            for summary in summaries {
                debug!("locking patch {:?}", summary);
                *summary = lock(&self.locked, &self.patches_available, summary.clone());
            }
        }
        self.patches_locked = true;
    }

    /// Gets all patches grouped by the source URLs they are going to patch.
    ///
    /// These patches are mainly collected from [`patch`](Self::patch).
    /// They might not be the same as patches actually used during dependency resolving.
    pub fn patches(&self) -> &HashMap<CanonicalUrl, Vec<Summary>> {
        &self.patches
    }

    pub fn lock(&self, summary: Summary) -> Summary {
        assert!(self.patches_locked);
        lock(&self.locked, &self.patches_available, summary)
    }
}

impl<R: Registry> Registry for PackageRegistry<R> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<()> {
        assert!(self.patches_locked);

        // Next up on our list of candidates is to check the `[patch]` section
        // of the manifest. Here we look through all patches relevant to the
        // source that `dep` points to, and then we match name/version. Note
        // that we don't use `dep.matches(..)` because the patches, by definition,
        // come from a different source. This means that `dep.matches(..)` will
        // always return false, when what we really care about is the name/version match.
        let mut patches = Vec::<Summary>::new();
        if let Some(extra) = self.patches.get(dep.source_id().canonical_url()) {
            patches.extend(
                extra.iter().filter(|s| dep.matches_ignoring_source(s.package_id())).cloned(),
            );
        }

        // A crucial feature of the `[patch]` feature is that we don't query the
        // actual registry if we have a "locked" dependency. A locked dep basically
        // just means a version constraint of `=a.b.c`, and because patches take
        // priority over the actual source then if we have a candidate we're done.
        if patches.len() == 1 && dep.is_locked() {
            f(IndexSummary::Candidate(patches.remove(0)));
            return Ok(());
        }

        if !patches.is_empty() {
            debug!(
                "found {} patches with an unlocked dep on `{}` at {} \
                     with `{}`, \
                     looking at sources",
                patches.len(),
                dep.package_name(),
                dep.source_id(),
                dep.version_req()
            );
        }

        for patch in patches.iter() {
            f(IndexSummary::Candidate(patch.clone()));
        }

        // Our sources shouldn't ever come back to us with two summaries
        // that have the same version. We could, however, have an `[patch]`
        // section which is in use to override a version in the registry.
        // This means that if our `summary` in this loop has the same
        // version as something in `patches` that we've already selected,
        // then we skip this `summary`.
        let locked = &self.locked;
        let all_patches = &self.patches_available;
        let callback = &mut |summary: IndexSummary| {
            for patch in patches.iter() {
                let patch = patch.package_id().version();
                if summary.package_id().version() == patch {
                    return;
                }
            }
            let summary = summary.map_summary(|summary| lock(locked, all_patches, summary));
            f(summary)
        };
        self.sources
            .query(dep, kind, callback)
            .await
            .with_context(|| format!("unable to update {}", dep.source_id()))
            .with_context(|| {
                format!("failed to load source for dependency `{}`", dep.package_name())
            })
    }

    fn describe_source(&self, source: SourceId) -> String {
        self.sources.describe_source(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        self.sources.is_replaced(source)
    }
}

/// See [`PackageRegistry::lock`].
fn lock(
    locked: &LockedMap,
    patches: &HashMap<CanonicalUrl, Vec<PackageId>>,
    summary: Summary,
) -> Summary {
    let pair = locked
        .get(&(summary.source_id(), summary.name()))
        .and_then(|vec| vec.iter().find(|&&(id, _)| id == summary.package_id()));

    trace!("locking summary of {}", summary.package_id());

    // Lock the summary's ID if possible
    let summary = match pair {
        Some((precise, _)) => summary.override_id(*precise),
        None => summary,
    };
    summary.map_dependencies(|dep| {
        trace!("\t{}/{}/{}", dep.package_name(), dep.version_req(), dep.source_id());

        // If we've got a known set of overrides for this summary, then
        // one of a few cases can arise:
        //
        // 1. We have a lock entry for this dependency from the same
        //    source as it's listed as coming from. In this case we make
        //    sure to lock to precisely the given package ID.
        //
        // 2. We have a lock entry for this dependency, but it's from a
        //    different source than what's listed, or the version
        //    requirement has changed. In this case we must discard the
        //    locked version because the dependency needs to be
        //    re-resolved.
        //
        // 3. We have a lock entry for this dependency, but it's from a
        //    different source than what's listed. This lock though happens
        //    through `[patch]`, so we want to preserve it.
        //
        // 4. We don't have a lock entry for this dependency, in which
        //    case it was likely an optional dependency which wasn't
        //    included previously so we just pass it through anyway.
        //
        // Cases 1/2 are handled by `matches_id`, case 3 is handled specially,
        // and case 4 is handled by falling through to the logic below.
        if let Some((_, locked_deps)) = pair {
            let locked = locked_deps.iter().find(|&&id| {
                // If the dependency matches the package id exactly then we've
                // found a match, this is the id the dependency was previously
                // locked to.
                if dep.matches_id(id) {
                    return true;
                }

                // If the name/version doesn't match, then we definitely don't
                // have a match whatsoever. Otherwise we need to check
                // `[patch]`...
                if !dep.matches_ignoring_source(id) {
                    return false;
                }

                // ... so here we look up the dependency url in the patches
                // map, and we see if `id` is contained in the list of patches
                // for that url. If it is then this lock is still valid,
                // otherwise the lock is no longer valid.
                match patches.get(dep.source_id().canonical_url()) {
                    Some(list) => list.contains(&id),
                    None => false,
                }
            });

            if let Some(&locked) = locked {
                trace!("\tfirst hit on {}", locked);
                let mut dep = dep;

                // If we found a locked version where the sources match, then
                // we can `lock_to` to get an exact lock on this dependency.
                // Otherwise we got a lock via `[patch]` so we only lock the
                // version requirement, not the source.
                if locked.source_id() == dep.source_id() {
                    dep.lock_to(locked);
                } else {
                    dep.lock_version(locked.version());
                }
                return dep;
            }
        }

        // If this dependency did not have a locked version, then we query
        // all known locked packages to see if they match this dependency.
        // If anything does then we lock it to that and move on.
        let v = locked
            .get(&(dep.source_id(), dep.package_name()))
            .and_then(|vec| vec.iter().find(|&&(id, _)| dep.matches_id(id)));
        if let Some(&(id, _)) = v {
            trace!("\tsecond hit on {}", id);
            let mut dep = dep;
            dep.lock_to(id);
            return dep;
        }

        trace!("\tnope, unlocked");
        dep
    })
}

/// A helper for selecting the summary, or generating a helpful error message.
///
/// Returns a tuple that the first element is the summary selected. The second
/// is a package ID indicating that the patch entry should be unlocked. This
/// happens when a match cannot be found with the `locked` one, but found one
/// via the original patch, so we need to inform the resolver to "unlock" it.
fn summary_for_patch(
    original_patch: &Patch,
    orig_patch_url: &Url,
    locked: &Option<LockedPatchDependency>,
    mut summaries: Vec<Summary>,
    source: &impl Registry,
) -> Result<(Summary, Option<PackageId>)> {
    if summaries.len() == 1 {
        return Ok((summaries.pop().unwrap(), None));
    }
    if summaries.len() > 1 {
        // TODO: In the future, it might be nice to add all of these
        // candidates so that version selection would just pick the
        // appropriate one. However, as this is currently structured, if we
        // added these all as patches, the unselected versions would end up in
        // the "unused patch" listing, and trigger a warning. It might take a
        // fair bit of restructuring to make that work cleanly, and there
        // isn't any demand at this time to support that.
        let mut vers: Vec<_> = summaries.iter().map(|summary| summary.version()).collect();
        vers.sort();
        let versions: Vec<_> = vers.into_iter().map(|v| v.to_string()).collect();
        return Err(anyhow!(
            "patch for `{}` in `{}` resolved to more than one candidate\n\
            note: found versions: {}\n\
            help: check `{}` patch definition for `{}` in `{}`\n\
            help: select only one package using `version = \"={}\"`",
            original_patch.dep.package_name(),
            original_patch.dep.source_id(),
            versions.join(", "),
            original_patch.dep.package_name(),
            orig_patch_url,
            original_patch.loc,
            versions.last().unwrap()
        ));
    }
    assert!(summaries.is_empty());
    let candidates = |dep: &Dependency| -> Vec<Summary> {
        block_on(source.query_vec(dep, QueryKind::Exact))
            .unwrap_or_else(|e| {
                tracing::warn!("could not query {:?}: {:?}", dep, e);
                Vec::new()
            })
            .into_iter()
            .filter_map(|s| match s {
                IndexSummary::Candidate(s) => Some(s),
                _ => None,
            })
            .collect()
    };
    // No summaries found, try to help the user figure out what is wrong.
    if let Some(locked) = locked {
        // Since the locked patch did not match anything, try the unlocked one.
        let orig_matches = candidates(&original_patch.dep);
        let summary =
            summary_for_patch(original_patch, orig_patch_url, &None, orig_matches, source)?;
        return Ok((summary.0, Some(locked.package_id)));
    }
    // Try checking if there are *any* packages that match this by name.
    let name_only_dep =
        Dependency::new_override(original_patch.dep.package_name(), original_patch.dep.source_id());
    let mut vers = candidates(&name_only_dep).iter().map(|s| s.version()).collect::<Vec<_>>();
    let found = match vers.len() {
        0 => String::new(),
        1 => format!("version `{}`", vers[0]),
        _ => {
            vers.sort();
            let strs: Vec<_> = vers.into_iter().map(|v| v.to_string()).collect();
            format!("versions `{}`", strs.join(", "))
        }
    };
    Err(if found.is_empty() {
        anyhow!(
            "patch location `{}` does not contain packages matching `{}`\n\
            help: check `{}` patch definition for `{}` in `{}`",
            original_patch.dep.source_id(),
            original_patch.dep.package_name(),
            original_patch.dep.package_name(),
            orig_patch_url,
            original_patch.loc
        )
    } else {
        anyhow!(
            "patch `{}` version mismatch\n\
            note: patch location contains {}, but patch definition requires `{}`\n\
            help: check patch location `{}`\n\
            help: check `{}` patch definition for `{}` in `{}`",
            original_patch.dep.package_name(),
            found,
            original_patch.dep.version_req(),
            original_patch.dep.source_id(),
            original_patch.dep.package_name(),
            orig_patch_url,
            original_patch.loc
        )
    })
}

/// The workspace, as far as resolving it goes.
pub struct Workspace<'a> {
    pub members: Members<'a>,
    /// Every package with a path source the workspace can reach (its members, their path
    /// dependencies and those of `[patch]` and `[replace]`), by source: cargo reads their
    /// manifests to tell what changed since the previous resolve.
    pub path_packages: &'a HashMap<SourceId, Summary>,
    /// The root manifest's `[patch]`, by the URL patched.
    pub patch: &'a [(Url, Vec<Patch>)],
    /// The root manifest's `[replace]`.
    pub replace: &'a [(PackageIdSpec, Dependency)],
    /// False only where optional dependencies need not be resolved (`cargo install`).
    pub require_optional_deps: bool,
    /// `-Zminimal-versions` is `MinimumVersionsFirst`.
    pub version_ordering: VersionOrdering,
    /// The Rust versions a version must support to be preferred, where the workspace asks for
    /// that (`resolver = "3"` or `incompatible-rust-versions = "fallback"`): the members'
    /// `rust-version`s, or rustc's version if none has one.
    pub rust_versions: Option<Vec<PartialVersion>>,
    /// The encoding of a new lock file, from the members' lowest `rust-version`.
    pub resolve_version: ResolveVersion,
    /// `--offline`, `--frozen`, or `net.offline` as `--offline`: a failed resolve then says
    /// offline mode may be why.
    pub offline_flag: Option<&'a str>,
}

impl Workspace<'_> {
    fn is_member(&self, id: PackageId) -> bool {
        self.members.members.iter().any(|m| m.package_id() == id)
    }
}

/// The two resolves cargo makes of a workspace (`resolve_ws_with_opts`, without
/// `--ignore-lock`): first every member with all its features and dev-dependencies, the
/// resolve `Cargo.lock` is written from, guided by `lock` (the lock file read, if any); then
/// the one asked for with `specs` and `cli_features`, guided by the first. Returns both.
pub fn resolve_ws<R: Registry>(
    registry: &mut PackageRegistry<R>,
    ws: &Workspace<'_>,
    lock: Option<&Resolve>,
    cli_features: &CliFeatures,
    specs: &[PackageIdSpec],
    has_dev_units: HasDevUnits,
) -> Result<(Resolve, Resolve)> {
    let resolve = resolve_with_previous(
        registry,
        ws,
        &CliFeatures::new_all(true),
        HasDevUnits::Yes,
        lock,
        &[],
        true,
    )?;
    let resolved_with_overrides = resolve_with_previous(
        registry,
        ws,
        cli_features,
        has_dev_units,
        Some(&resolve),
        specs,
        false,
    )?;
    Ok((resolve, resolved_with_overrides))
}

/// Resolves all dependencies for a package using an optional previous instance
/// of resolve to guide the resolution process. Every package of the previous
/// resolve is kept locked to it, but for those [`register_previous_locks`] and
/// [`register_patch_entries`] find changed (cargo's `keep_previous` without
/// `cargo update`).
///
/// The previous resolve normally comes from a lock file. This function does not
/// read or write lock files from the filesystem.
///
/// `specs` may be empty, which indicates it should resolve all workspace
/// members. In this case, `opts.all_features` must be `true`.
///
/// If `register_patches` is true, then entries from the `[patch]` table in
/// the manifest will be added to the given `PackageRegistry`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_with_previous<R: Registry>(
    registry: &mut PackageRegistry<R>,
    ws: &Workspace<'_>,
    cli_features: &CliFeatures,
    has_dev_units: HasDevUnits,
    previous: Option<&Resolve>,
    specs: &[PackageIdSpec],
    register_patches: bool,
) -> Result<Resolve> {
    // While registering patches, we will record preferences for particular versions
    // of various packages.
    let mut version_prefs = VersionPreferences::default();
    version_prefs.version_ordering(ws.version_ordering);
    if let Some(rust_versions) = &ws.rust_versions {
        version_prefs.rust_versions(rust_versions.clone());
    }

    let avoid_patch_ids = if register_patches {
        register_patch_entries(registry, ws, previous, &mut version_prefs)?
    } else {
        HashSet::default()
    };

    // Keep all of the previous resolve but the patches that should avoid locking.
    let keep = |p: &PackageId| !avoid_patch_ids.contains(p);

    let dev_deps = ws.require_optional_deps || has_dev_units == HasDevUnits::Yes;

    if let Some(r) = previous {
        trace!("previous: {:?}", r.iter().collect::<Vec<_>>());

        // In the case where a previous instance of resolve is available, we
        // want to lock as many packages as possible to the previous version
        // without disturbing the graph structure.
        register_previous_locks(ws, registry, r, &keep, dev_deps);

        // Prefer to use anything in the previous lock file, aka we want to have conservative updates.
        for id in r.iter().filter(keep) {
            debug!("attempting to prefer {}", id);
            version_prefs.prefer_package_id(id);
        }
    }

    if register_patches {
        registry.lock_patches();
    }

    let summaries: Vec<(Summary, ResolveOpts)> =
        members_with_features(&ws.members, specs, cli_features)?
            .into_iter()
            .map(|(member, features)| {
                let summary = registry.lock(member.clone());
                (summary, ResolveOpts::new(dev_deps, RequestedFeatures::CliFeatures(features)))
            })
            .collect();

    let replace = lock_replacements(ws, previous, &keep);

    let mut resolved = resolver::resolve(
        &summaries,
        &replace,
        &*registry,
        &version_prefs,
        ws.resolve_version,
        &ResolveSettings { offline_flag: ws.offline_flag, ..ResolveSettings::default() },
    )?;

    let patches = registry.patches().values().flat_map(|v| v.iter());
    resolved.register_used_patches(patches);

    if let Some(previous) = previous {
        resolved.merge_from(previous)?;
    }
    Ok(resolved)
}

/// In this function we're responsible for informing the `registry` of all
/// locked dependencies from the previous lock file we had, `resolve`.
///
/// This gets particularly tricky for a couple of reasons. The first is that we
/// want all updates to be conservative, so we actually want to take the
/// `resolve` into account (and avoid unnecessary registry updates and such).
/// the second, however, is that we want to be resilient to updates of
/// manifests. For example if a dependency is added or a version is changed we
/// want to make sure that we properly re-resolve (conservatively) instead of
/// providing an opaque error.
///
/// The logic here is somewhat subtle, but there should be more comments below to
/// clarify things.
///
/// Note that this function, at the time of this writing, is basically the
/// entire fix for issue #4127.
fn register_previous_locks<R: Registry>(
    ws: &Workspace<'_>,
    registry: &mut PackageRegistry<R>,
    resolve: &Resolve,
    keep: &impl Fn(&PackageId) -> bool,
    dev_deps: bool,
) {
    let path_pkg = |id: SourceId| {
        if !id.is_path() {
            return None;
        }
        ws.path_packages.get(&id)
    };

    // Ok so we've been passed in a `keep` function which basically says "if I
    // return `true` then this package wasn't listed for an update on the command
    // line". That is, if we run `cargo update foo` then `keep(bar)` will return
    // `true`, whereas `keep(foo)` will return `false` (roughly speaking).
    //
    // This isn't actually quite what we want, however. Instead we want to
    // further refine this `keep` function with *all transitive dependencies* of
    // the packages we're not keeping. For example, consider a case like this:
    //
    // * There's a crate `log`.
    // * There's a crate `serde` which depends on `log`.
    //
    // Let's say we then run `cargo update serde`. This may *also* want to
    // update the `log` dependency as our newer version of `serde` may have a
    // new minimum version required for `log`. Now this isn't always guaranteed
    // to work. What'll happen here is we *won't* lock the `log` dependency nor
    // the `log` crate itself, but we will inform the registry "please prefer
    // this version of `log`". That way if our newer version of serde works with
    // the older version of `log`, we conservatively won't update `log`. If,
    // however, nothing else in the dependency graph depends on `log` and the
    // newer version of `serde` requires a new version of `log` it'll get pulled
    // in (as we didn't accidentally lock it to an old version).
    let mut avoid_locking = HashSet::default();
    for node in resolve.iter() {
        if !keep(&node) {
            add_deps(resolve, node, &mut avoid_locking);
        }
    }

    // Ok, but the above loop isn't the entire story! Updates to the dependency
    // graph can come from two locations, the `cargo update` command or
    // manifests themselves. For example a manifest on the filesystem may
    // have been updated to have an updated version requirement on `serde`. In
    // this case both `keep(serde)` and `keep(log)` return `true` (the `keep`
    // that's an argument to this function). We, however, don't want to keep
    // either of those! Otherwise we'll get obscure resolve errors about locked
    // versions.
    //
    // To solve this problem we iterate over all packages with path sources
    // (aka ones with manifests that are changing) and take a look at all of
    // their dependencies. If any dependency does not match something in the
    // previous lock file, then we're guaranteed that the main resolver will
    // update the source of this dependency no matter what. Knowing this we
    // poison all packages from the same source, forcing them all to get
    // updated.
    //
    // This may seem like a heavy hammer, and it is! It means that if you change
    // anything from crates.io then all of crates.io becomes unlocked. Note,
    // however, that we still want conservative updates. This currently happens
    // because the first candidate the resolver picks is the previously locked
    // version, and only if that fails to activate to we move on and try
    // a different version. (giving the guise of conservative updates)
    //
    // For example let's say we had `serde = "0.1"` written in our lock file.
    // When we later edit this to `serde = "0.1.3"` we don't want to lock serde
    // at its old version, 0.1.1. Instead we want to allow it to update to
    // `0.1.3` and update its own dependencies (like above). To do this *all
    // crates from crates.io* are not locked (aka added to `avoid_locking`).
    // For dependencies like `log` their previous version in the lock file will
    // come up first before newer version, if newer version are available.
    {
        let mut path_deps: Vec<&Summary> = ws.members.members.to_vec();
        let mut visited = HashSet::default();
        while let Some(member) = path_deps.pop() {
            if !visited.insert(member.package_id()) {
                continue;
            }
            let is_ws_member = ws.is_member(member.package_id());
            for dep in member.dependencies() {
                // If this dependency didn't match anything special then we may want
                // to poison the source as it may have been added. If this path
                // dependencies is **not** a workspace member, however, and it's an
                // optional/non-transitive dependency then it won't be necessarily
                // be in our lock file. If this shows up then we avoid poisoning
                // this source as otherwise we'd repeatedly update the registry.
                //
                // TODO: this breaks adding an optional dependency in a
                // non-workspace member and then simultaneously editing the
                // dependency on that crate to enable the feature. For now,
                // this bug is better than the always-updating registry though.
                if !is_ws_member && (dep.is_optional() || !dep.is_transitive()) {
                    continue;
                }

                // If dev-dependencies aren't being resolved, skip them.
                if !dep.is_transitive() && !dev_deps {
                    continue;
                }

                // If this is a path dependency, then try to push it onto our
                // worklist.
                if let Some(pkg) = path_pkg(dep.source_id()) {
                    path_deps.push(pkg);
                    continue;
                }

                // If we match *anything* in the dependency graph then we consider
                // ourselves all ok, and assume that we'll resolve to that.
                if resolve.iter().any(|id| dep.matches_ignoring_source(id)) {
                    continue;
                }

                // Ok if nothing matches, then we poison the source of these
                // dependencies and the previous lock file.
                debug!(
                    "poisoning {} because {} looks like it changed {}",
                    dep.source_id(),
                    member.package_id(),
                    dep.package_name()
                );
                for id in resolve.iter().filter(|id| id.source_id() == dep.source_id()) {
                    add_deps(resolve, id, &mut avoid_locking);
                }
            }
        }
    }

    // Additionally, here we process all path dependencies listed in the previous
    // resolve. They can not only have their dependencies change but also
    // the versions of the package change as well. If this ends up happening
    // then we want to make sure we don't lock a package ID node that doesn't
    // actually exist. Note that we don't do transitive visits of all the
    // package's dependencies here as that'll be covered below to poison those
    // if they changed.
    //
    // This must come after all other `add_deps` calls to ensure it recursively walks the tree when
    // called.
    for node in resolve.iter() {
        if let Some(pkg) = path_pkg(node.source_id())
            && pkg.package_id() != node
        {
            avoid_locking.insert(node);
        }
    }

    // Alright now that we've got our new, fresh, shiny, and refined `keep`
    // function let's put it to action. Take a look at the previous lock file,
    // filter everything by this callback, and then shove everything else into
    // the registry as a locked dependency.
    let keep = |id: &PackageId| keep(id) && !avoid_locking.contains(id);

    registry.clear_lock();
    for node in resolve.iter().filter(keep) {
        let deps = resolve.deps_not_replaced(node).map(|p| p.0).filter(keep).collect::<Vec<_>>();

        // In the v2 lockfile format and prior the `branch=master` dependency
        // directive was serialized the same way as the no-branch-listed
        // directive. Nowadays in Cargo, however, these two directives are
        // considered distinct and are no longer represented the same way. To
        // maintain compatibility with older lock files we register locked nodes
        // for *both* the master branch and the default branch.
        //
        // Note that this is only applicable for loading older resolves now at
        // this point. All new lock files are encoded as v3-or-later, so this is
        // just compat for loading an old lock file successfully.
        if let Some(node) = master_branch_git_source(node, resolve) {
            registry.register_lock(node, deps.clone());
        }

        registry.register_lock(node, deps);
    }

    /// Recursively add `node` and all its transitive dependencies to `set`.
    fn add_deps(resolve: &Resolve, node: PackageId, set: &mut HashSet<PackageId>) {
        if !set.insert(node) {
            return;
        }
        debug!("ignoring any lock pointing directly at {}", node);
        for (dep, _) in resolve.deps_not_replaced(node) {
            add_deps(resolve, dep, set);
        }
    }
}

fn master_branch_git_source(id: PackageId, resolve: &Resolve) -> Option<PackageId> {
    if resolve.version() <= ResolveVersion::V2 {
        let source = id.source_id();
        if let Some(GitReference::DefaultBranch) = source.git_reference() {
            let new_source =
                SourceId::for_git(source.url(), GitReference::Branch("master".to_owned()))
                    .unwrap()
                    .with_precise_from(source);
            return Some(id.with_source_id(new_source));
        }
    }
    None
}

/// Informs `registry` and `version_pref` that `[patch]` entries are available
/// and preferable for the dependency resolution.
///
/// This returns a set of PackageIds of `[patch]` entries, and some related
/// locked PackageIds, for which locking should be avoided (but which will be
/// preferred when searching dependencies, via [`VersionPreferences::prefer_dependency`]).
fn register_patch_entries<R: Registry>(
    registry: &mut PackageRegistry<R>,
    ws: &Workspace<'_>,
    previous: Option<&Resolve>,
    version_prefs: &mut VersionPreferences,
) -> Result<HashSet<PackageId>> {
    let mut avoid_patch_ids = HashSet::default();
    for (url, patches) in ws.patch.iter() {
        for patch in patches {
            version_prefs.prefer_dependency(patch.dep.clone());
        }
        let Some(previous) = previous else {
            let patches: Vec<_> = patches.iter().map(|p| (p, None)).collect();
            let unlock_ids = registry.patch(url, &patches)?;
            // Since nothing is locked, this shouldn't possibly return anything.
            assert!(unlock_ids.is_empty());
            continue;
        };

        // This is a list of pairs where the first element of the pair is
        // the raw `Dependency` which matches what's listed in `Cargo.toml`.
        // The second element is, if present, the "locked" version of
        // the `Dependency` as well as the `PackageId` that it previously
        // resolved to. This second element is calculated by looking at the
        // previous resolve graph, which is primarily what's done here to
        // build the `registrations` list.
        let mut registrations = Vec::new();
        for patch in patches {
            let dep = &patch.dep;
            let candidates = || previous.iter().chain(previous.unused_patches().iter().cloned());

            let lock = match candidates().find(|id| dep.matches_id(*id)) {
                // If we found an exactly matching candidate in our list of
                // candidates, then that's the one to use.
                Some(package_id) => {
                    let mut locked_dep = dep.clone();
                    locked_dep.lock_to(package_id);
                    Some(LockedPatchDependency {
                        dependency: locked_dep,
                        package_id,
                        alt_package_id: None,
                    })
                }
                None => {
                    // If the candidate does not have a matching source id
                    // then we may still have a lock candidate. If we're
                    // loading a v2-encoded resolve graph and `dep` is a
                    // git dep with `branch = 'master'`, then this should
                    // also match candidates without `branch = 'master'`
                    // (which is now treated separately in Cargo).
                    //
                    // In this scenario we try to convert candidates located
                    // in the resolve graph to explicitly having the
                    // `master` branch (if they otherwise point to
                    // `DefaultBranch`). If this works and our `dep`
                    // matches that then this is something we'll lock to.
                    match candidates().find(|&id| match master_branch_git_source(id, previous) {
                        Some(id) => dep.matches_id(id),
                        None => false,
                    }) {
                        Some(id_using_default) => {
                            let id_using_master = id_using_default.with_source_id(
                                dep.source_id().with_precise_from(id_using_default.source_id()),
                            );

                            let mut locked_dep = dep.clone();
                            locked_dep.lock_to(id_using_master);
                            Some(LockedPatchDependency {
                                dependency: locked_dep,
                                package_id: id_using_master,
                                // Note that this is where the magic
                                // happens, where the resolve graph
                                // probably has locks pointing to
                                // DefaultBranch sources, and by including
                                // this here those will get transparently
                                // rewritten to Branch("master") which we
                                // have a lock entry for.
                                alt_package_id: Some(id_using_default),
                            })
                        }

                        // No locked candidate was found
                        None => None,
                    }
                }
            };

            registrations.push((patch, lock));
        }

        let canonical = CanonicalUrl::new(url)?;
        for (orig_patch, unlock_id) in registry.patch(url, &registrations)? {
            // Avoid the locked patch ID.
            avoid_patch_ids.insert(unlock_id);
            // Also avoid the thing it is patching.
            avoid_patch_ids.extend(previous.iter().filter(|id| {
                orig_patch.dep.matches_ignoring_source(*id)
                    && *id.source_id().canonical_url() == canonical
            }));
        }
    }

    Ok(avoid_patch_ids)
}

/// Locks each `[replace]` entry to a specific Package ID
/// if the lockfile contains any corresponding previous replacement.
fn lock_replacements(
    ws: &Workspace<'_>,
    previous: Option<&Resolve>,
    keep: &impl Fn(&PackageId) -> bool,
) -> Vec<(PackageIdSpec, Dependency)> {
    match previous {
        Some(r) => ws
            .replace
            .iter()
            .map(|(spec, dep)| {
                for (&key, &val) in r.replacements().iter() {
                    if spec.matches(key) && dep.matches_id(val) && keep(&val) {
                        let mut dep = dep.clone();
                        dep.lock_to(val);
                        return (spec.clone(), dep);
                    }
                }
                (spec.clone(), dep.clone())
            })
            .collect::<Vec<_>>(),
        None => ws.replace.to_vec(),
    }
}
