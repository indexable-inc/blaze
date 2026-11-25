//! Where a needed package's facts come from: its sources' record in the store, read in place
//! (docs/v2/plan/design/units.md, "Where manifests come from").
//!
//! [`Resolution::pass`](crate::Resolution::pass) asks for packages by [`LockId`];
//! [`StoreSources::package`] answers one from the record blaze-fetch put for it (a crates.io
//! crate by its name, version and SHA-256; a git package by its commit), opened through the
//! reader the caller passes ([`Open`]: `PackView::open_source_put` for records this machine
//! fetched, `PackView::open_source` for any other), and read with blaze-manifest's `Layout` over
//! the record's files: the manifest and the directory listings the layout asks for are slices
//! of the record. Nothing is read from cargo's caches and nothing is guessed: a package whose
//! record the store lacks is [`SourceError::NotFetched`], naming it and its key.
//!
//! Workspace members are not asked for here: the caller reads them from the checkout and hands
//! them to [`Resolution::member`](crate::Resolution::member). A path package that is not a
//! member is [`SourceError::PathOutsideWorkspace`]: the lock does not say where it is.
//!
//! A git package is found as cargo finds it in a checkout (every `Cargo.toml` of the commit
//! whose `package.name` is the one asked): first among the members of the layout started at
//! the commit's root, then by that scan. A name with the lock's version in two directories is
//! [`SourceError::TwoInCommit`]. Each commit's layouts are kept for the commit's next packages
//! (tempo's reth commit holds 110), so a workspace is laid out once.

#![blaze::sans_io]

use core::fmt;

use blaze_fetch::{PackRefused, PackView, Skip, SourceId, plan};
use blaze_git_fetch::Oid;
use blaze_manifest::layout::{Entry, Layout, LayoutError, Need, Pass as LayoutPass, Units};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc, String, Vec};

use crate::{Package, Text};

/// The reader a record is opened with: `PackView::open_source_put` (a record this machine
/// fetched and put, its digest checked then) or `PackView::open_source` (any other record:
/// every byte hashed once).
pub type Open<'b> = for<'k> fn(&SourceId<'k>, &'b [u8]) -> Result<PackView<'b>, PackRefused>;

/// The store, as the caller holds it: the record of `source` (found at `source.key()`), mapped
/// for `'b`, or `None` when the store holds none.
pub trait Records<'b> {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'b [u8]>;
}

/// Placeholder [`Records`] for a [`StoreSources`] whose records are supplied per call through
/// [`StoreSources::package_with`], allowing a stateful discovery session to keep its parsed plan,
/// opened views, and git layouts across waves without borrowing the driver's record store between
/// waves.
#[derive(Clone, Copy, Default)]
pub struct ExternalRecords;

impl<'b> Records<'b> for ExternalRecords {
    fn record(&mut self, _source: &SourceId<'_>) -> Option<&'b [u8]> {
        None
    }
}

/// The layout gives up after this many passes (its depth: the root above, globs, targets).
const LAYOUT_PASSES: u32 = 64;

/// How a locked package's sources are found.
#[derive(Clone, Copy)]
enum Where {
    /// A workspace member or another path package: not in the store.
    Path,
    /// The plan's unit at this index.
    Unit(u32),
    /// The plan's skip at this index: no source blaze can verify.
    Skipped(u32),
}

/// One git commit's packages, as laid out so far.
struct Commit<'a> {
    commit: Oid,
    /// Each layout run in this commit (the first from its root).
    layouts: AVec<'a, Units<'a>>,
    /// Whether the layout at the root was tried.
    root_tried: bool,
    /// Every `(package.name, directory)` of the commit, made when first needed.
    scan: Option<AVec<'a, (Text<'a>, Text<'a>)>>,
}

/// The facts of locked packages, from their records in the store.
pub struct StoreSources<'l, 'b, 'a, R: Records<'b>> {
    lock: &'l Lock<'a>,
    plan: plan::Plan<'l, 'a>,
    /// Per locked package.
    place: AVec<'a, Where>,
    records: R,
    open: Open<'b>,
    /// Per plan unit, its record once opened.
    views: AVec<'a, Option<PackView<'b>>>,
    /// Per locked package, its located directory in its pack (`""` for a crate; subdirectory for
    /// a git workspace member), cached on first lookup or seeded from an earlier discovery wave.
    dirs: AVec<'a, Option<Text<'a>>>,
    commits: AVec<'a, Commit<'a>>,
    alloc: Alloc<'a>,
    _bytes: core::marker::PhantomData<&'b [u8]>,
}

/// Why a package's facts could not be read: each names the package (name and version).
#[derive(Debug)]
pub enum SourceError<'a> {
    /// The store holds no record of the package's sources: fetch the lock first.
    NotFetched { package: Text<'a>, version: Text<'a>, source: Text<'a>, key: [u8; 32] },
    /// The lock gives no source blaze can fetch and verify (blaze-fetch's `Skip`).
    NotFetchable { package: Text<'a>, version: Text<'a>, why: Text<'a> },
    /// The lock cannot be made into a fetch plan (a name, version, checksum or commit that is
    /// not one).
    Plan { why: Text<'a> },
    /// The record found is refused by the reader.
    Refused { package: Text<'a>, version: Text<'a>, why: PackRefused },
    /// A path package that is not a workspace member: the lock does not say where it is.
    PathOutsideWorkspace { package: Text<'a>, version: Text<'a> },
    /// No manifest of the package (that name and version) in its record.
    NoManifest { package: Text<'a>, version: Text<'a> },
    /// Two directories of one git commit hold the package at the lock's version.
    TwoInCommit { package: Text<'a>, version: Text<'a>, first: Text<'a>, second: Text<'a> },
    /// A manifest of the record is not UTF-8.
    NotUtf8 { package: Text<'a>, path: Text<'a> },
    /// The layout refused the package's manifests.
    Layout { package: Text<'a>, version: Text<'a>, error: LayoutError<'a> },
    /// The layout did not end within [`LAYOUT_PASSES`] passes.
    LayoutPasses { package: Text<'a>, version: Text<'a> },
}

impl fmt::Display for SourceError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::NotFetched { package, version, source, key } => {
                write!(f, "{package} {version}: source not fetched ({source}); the store holds no record ")?;
                for b in key {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
            SourceError::NotFetchable { package, version, why } => write!(f, "{package} {version}: no source blaze can verify: {why}"),
            SourceError::Plan { why } => write!(f, "the lock cannot be fetched: {why}"),
            SourceError::Refused { package, version, why } => {
                write!(f, "{package} {version}: its record is refused: {}", blaze_text::Shown(why.clone()))
            }
            SourceError::PathOutsideWorkspace { package, version } => {
                write!(f, "{package} {version}: a path package outside the workspace's members (the lock does not say where)")
            }
            SourceError::NoManifest { package, version } => write!(f, "{package} {version}: no manifest of it in its record"),
            SourceError::TwoInCommit { package, version, first, second } => {
                write!(f, "{package} {version}: two directories of its git commit hold it: `{first}` and `{second}`")
            }
            SourceError::NotUtf8 { package, path } => write!(f, "{package}: `{path}` in its record is not UTF-8"),
            SourceError::Layout { package, version, error } => write!(f, "{package} {version}: {error}"),
            SourceError::LayoutPasses { package, version } => write!(f, "{package} {version}: no layout after {LAYOUT_PASSES} passes"),
        }
    }
}

impl<'l, 'b, 'a, R: Records<'b>> StoreSources<'l, 'b, 'a, R> {
    /// The sources of `lock`'s packages in `records`, each record opened with `open`.
    pub fn new(lock: &'l Lock<'a>, records: R, open: Open<'b>, alloc: Alloc<'a>) -> Result<StoreSources<'l, 'b, 'a, R>, SourceError<'a>> {
        let plan = plan::plan(lock, alloc).map_err(|why| SourceError::Plan { why: shown(&why, alloc) })?;
        let mut place = Vec::with_capacity_in(lock.packages.len(), alloc);
        place.resize(lock.packages.len(), Where::Path);
        for (at, unit) in plan.units.iter().enumerate() {
            match unit {
                plan::Unit::Crate { package, .. } => place[package.index()] = Where::Unit(at as u32),
                plan::Unit::Git { commit, .. } => {
                    for (id, p) in lock.packages.iter().enumerate() {
                        if let LockSource::Git { commit: c, .. } = &p.source
                            && Oid::parse_hex(c.as_bytes()) == Some(*commit)
                        {
                            place[id] = Where::Unit(at as u32);
                        }
                    }
                }
            }
        }
        for (at, skip) in plan.skipped.iter().enumerate() {
            let name = match skip {
                Skip::NoChecksum { name } | Skip::OtherRegistry { name, .. } | Skip::GitNotHttps { name, .. } => *name,
            };
            // A skip names its package by name; of two packages of one name, the one not
            // otherwise placed (a crates.io or https git package is never skipped).
            for id in lock.named(name) {
                if !matches!(lock.package(id).source, LockSource::Path) && matches!(place[id.index()], Where::Path) {
                    place[id.index()] = Where::Skipped(at as u32);
                }
            }
        }
        let mut views = Vec::with_capacity_in(plan.units.len(), alloc);
        views.resize(plan.units.len(), None);
        let mut dirs = Vec::with_capacity_in(lock.packages.len(), alloc);
        dirs.resize(lock.packages.len(), None);
        Ok(StoreSources { lock, plan, place, records, open, views, dirs, commits: Vec::new_in(alloc), alloc, _bytes: core::marker::PhantomData })
    }

    /// The store, as handed in.
    pub fn records(&self) -> &R {
        &self.records
    }

    /// Seeds the located directory of locked package `id` inside its pack (e.g. from a discovery
    /// session that already laid out its git commit workspace) so [`Self::files`] returns it
    /// without re-running `Layout`.
    pub fn seed_dir(&mut self, id: LockId, dir: &str) {
        if let Some(slot) = self.dirs.get_mut(id.index()) {
            *slot = Some(own(dir, self.alloc));
        }
    }

    /// Every non-empty located package directory discovered so far (git workspace subdirectories),
    /// as `(LockId, &str)`.
    pub fn located_git_dirs(&self) -> impl Iterator<Item = (LockId, &str)> {
        self.dirs.iter().enumerate().filter_map(|(i, dir)| {
            let id = u32::try_from(i).ok().map(LockId)?;
            dir.as_deref().filter(|d| !d.is_empty()).map(|d| (id, d))
        })
    }

    /// The facts of locked package `id`, from its record.
    pub fn package(&mut self, id: LockId) -> Result<Package<'a>, SourceError<'a>> {
        let StoreSources { lock, plan, place, records, open, views, dirs, commits, alloc, .. } = self;
        locate_inner(lock, plan, place, records, *open, views, dirs, commits, *alloc, id).map(|(_, package, _)| package)
    }

    /// As [`Self::package`], reading any not-yet-opened pack record from `records` instead of the
    /// stored `R` (for stateful multi-wave discovery sessions).
    pub fn package_with<R2: Records<'b>>(&mut self, id: LockId, records: &mut R2) -> Result<Package<'a>, SourceError<'a>> {
        let StoreSources { lock, plan, place, open, views, dirs, commits, alloc, .. } = self;
        locate_inner(lock, plan, place, records, *open, views, dirs, commits, *alloc, id).map(|(_, package, _)| package)
    }

    /// Where locked package `id`'s files are: its record, opened, and the package's directory
    /// in it (`""` for a crate; a git package's directory in its commit, as [`Self::package`]
    /// found it). What a runner serves as the package's sources.
    pub fn files(&mut self, id: LockId) -> Result<(PackView<'b>, Text<'a>), SourceError<'a>> {
        let StoreSources { lock, plan, place, records, open, views, dirs, commits, alloc, .. } = self;
        let (at, view) = open_view_inner(lock, plan, place, records, *open, views, *alloc, id)?;
        // Fast path: directory already located (by an earlier `package`/`files` call or seeded
        // from the unit-graph discovery session) — skip re-parsing `Cargo.toml` and `Layout`.
        if let Some(Some(dir)) = dirs.get(id.index()) {
            return Ok((view, own(dir.as_str(), *alloc)));
        }
        // Fast path for crates.io archives: a `.crate` pack always has its package at `""`.
        // Verify `Cargo.toml` exists in the pack index without re-running full manifest layout.
        if matches!(plan.units.get(at), Some(plan::Unit::Crate { .. })) {
            let p = lock.package(id);
            if view.find_entry("Cargo.toml").is_some() {
                if let Some(slot) = dirs.get_mut(id.index()) {
                    *slot = Some(Text::new_in(*alloc));
                }
                return Ok((view, Text::new_in(*alloc)));
            }
            return Err(SourceError::NoManifest { package: own(p.name.as_str(), *alloc), version: version_text(lock, id, *alloc) });
        }
        locate_inner(lock, plan, place, records, *open, views, dirs, commits, *alloc, id).map(|(view, _, dir)| (view, dir))
    }
}

/// Resolves `id`'s plan unit and opens (or reuses) its [`PackView`].
#[expect(clippy::too_many_arguments, reason = "internal split-borrow helper shared by package, package_with, and files")]
fn open_view_inner<'l, 'b, 'a, R: Records<'b>>(
    lock: &'l Lock<'a>,
    plan: &plan::Plan<'l, 'a>,
    place: &[Where],
    records: &mut R,
    open: Open<'b>,
    views: &mut [Option<PackView<'b>>],
    alloc: Alloc<'a>,
    id: LockId,
) -> Result<(usize, PackView<'b>), SourceError<'a>> {
    let p = lock.package(id);
    let version = version_text(lock, id, alloc);
    let at = match place.get(id.index()).copied().unwrap_or(Where::Path) {
        Where::Path => return Err(SourceError::PathOutsideWorkspace { package: own(p.name.as_str(), alloc), version }),
        Where::Skipped(at) => {
            let why = match plan.skipped.get(usize::try_from(at).unwrap_or(usize::MAX)) {
                Some(skip) => shown(skip, alloc),
                None => own("skipped", alloc),
            };
            return Err(SourceError::NotFetchable { package: own(p.name.as_str(), alloc), version, why });
        }
        Where::Unit(at) => usize::try_from(at).unwrap_or(usize::MAX),
    };
    let Some(unit) = plan.units.get(at) else {
        return Err(SourceError::NoManifest { package: own(p.name.as_str(), alloc), version });
    };
    // A record is mapped and opened once (a git commit's serves all its packages).
    let view = match views.get(at).copied().flatten() {
        Some(view) => view,
        None => {
            let source = unit.source();
            let Some(bytes) = records.record(&source) else {
                return Err(SourceError::NotFetched {
                    package: own(p.name.as_str(), alloc),
                    version,
                    source: own(p.source_text.as_str(), alloc),
                    key: source.key().0,
                });
            };
            let view = (open)(&source, bytes).map_err(|why| SourceError::Refused { package: own(p.name.as_str(), alloc), version, why })?;
            if let Some(slot) = views.get_mut(at) {
                *slot = Some(view);
            }
            view
        }
    };
    Ok((at, view))
}

/// Package `id`'s record, its facts and its directory in the record: the one lookup
/// [`StoreSources::package`], [`StoreSources::package_with`], and [`StoreSources::files`] share.
#[expect(clippy::too_many_arguments, reason = "internal split-borrow helper shared by package, package_with, and files")]
fn locate_inner<'l, 'b, 'a, R: Records<'b>>(
    lock: &'l Lock<'a>,
    plan: &plan::Plan<'l, 'a>,
    place: &[Where],
    records: &mut R,
    open: Open<'b>,
    views: &mut [Option<PackView<'b>>],
    dirs: &mut [Option<Text<'a>>],
    commits: &mut AVec<'a, Commit<'a>>,
    alloc: Alloc<'a>,
    id: LockId,
) -> Result<(PackView<'b>, Package<'a>, Text<'a>), SourceError<'a>> {
    let (at, view) = open_view_inner(lock, plan, place, records, open, views, alloc, id)?;
    let p = lock.package(id);
    let name = p.name.as_str();
    let version = version_text(lock, id, alloc);
    match plan.units.get(at) {
        Some(plan::Unit::Crate { .. }) => {
            let units = lay_out(&view, "", name, &version, alloc)?;
            match units.members.iter().find(|m| m.name.as_str() == name) {
                Some(member) => {
                    if let Some(slot) = dirs.get_mut(id.index()) {
                        *slot = Some(own(member.dir.as_str(), alloc));
                    }
                    Ok((view, Package::from_member(member, alloc), own(member.dir.as_str(), alloc)))
                }
                None => Err(SourceError::NoManifest { package: own(p.name.as_str(), alloc), version }),
            }
        }
        Some(plan::Unit::Git { commit, .. }) => {
            let commit = *commit;
            let c_at = match commits.iter().position(|c| c.commit == commit) {
                Some(c_at) => c_at,
                None => {
                    commits.push(Commit { commit, layouts: Vec::new_in(alloc), root_tried: false, scan: None });
                    commits.len() - 1
                }
            };
            let Some(c) = commits.get_mut(c_at) else {
                return Err(SourceError::NoManifest { package: own(p.name.as_str(), alloc), version });
            };
            let (package, dir) = git_package(c, &view, name, &version, alloc)?;
            if let Some(slot) = dirs.get_mut(id.index()) {
                *slot = Some(own(dir.as_str(), alloc));
            }
            Ok((view, package, dir))
        }
        None => Err(SourceError::NoManifest { package: own(p.name.as_str(), alloc), version }),
    }
}

/// A git package `name` at `version`: from a layout already run in its commit, else from the
/// layout at the commit's root, else from the layout started at the one directory the scan
/// finds it in. With its facts, its directory in the commit.
fn git_package<'a>(c: &mut Commit<'a>, view: &PackView<'_>, name: &str, version: &Text<'a>, alloc: Alloc<'a>) -> Result<(Package<'a>, Text<'a>), SourceError<'a>> {
    let found = |layouts: &AVec<'a, Units<'a>>| {
        layouts.iter().find_map(|units| {
            units.members.iter().find(|m| m.name.as_str() == name && member_version(m) == version.as_str()).map(|m| (Package::from_member(m, alloc), m.dir.clone()))
        })
    };
    if let Some(package) = found(&c.layouts) {
        return Ok(package);
    }
    if !c.root_tried {
        c.root_tried = true;
        // The root may hold no manifest, or one that is no workspace: then only the scan finds.
        if view.find_entry("Cargo.toml").is_some() {
            c.layouts.push(lay_out(view, "", name, version, alloc)?);
            if let Some(package) = found(&c.layouts) {
                return Ok(package);
            }
        }
    }
    if c.scan.is_none() {
        c.scan = Some(scan(view, name, version, alloc)?);
    }
    let mut dir: Option<&str> = None;
    for (n, d) in c.scan.iter().flat_map(|s| s.iter()) {
        if n.as_str() != name {
            continue;
        }
        let units = lay_out(view, d, name, version, alloc)?;
        let fits = units.members.iter().any(|m| m.name.as_str() == name && member_version(m) == version.as_str() && m.dir.as_str() == d.as_str());
        if !fits {
            continue;
        }
        if let Some(first) = dir {
            return Err(SourceError::TwoInCommit { package: own(name, alloc), version: version.clone(), first: own(first, alloc), second: d.clone() });
        }
        dir = Some(d.as_str());
        c.layouts.push(units);
    }
    match dir.is_some() {
        true => found(&c.layouts).ok_or_else(|| SourceError::NoManifest { package: own(name, alloc), version: version.clone() }),
        false => Err(SourceError::NoManifest { package: own(name, alloc), version: version.clone() }),
    }
}

/// A member's version as cargo reads it: `0.0.0` when none is written.
fn member_version<'m>(m: &'m blaze_manifest::layout::Member<'_>) -> &'m str {
    m.version.as_ref().map_or("0.0.0", |v| v.as_str())
}

/// Every `(package.name, directory)` of a commit's record, sorted. A manifest that does not
/// parse names no package here (cargo skips a nested manifest it cannot read the same way); a
/// package that lives only in one is then [`SourceError::NoManifest`], never guessed.
fn scan<'a>(view: &PackView<'_>, package: &str, version: &Text<'a>, alloc: Alloc<'a>) -> Result<AVec<'a, (Text<'a>, Text<'a>)>, SourceError<'a>> {
    let mut out = Vec::new_in(alloc);
    for entry in view.entries() {
        if entry.mode == blaze_fetch::Mode::Submodule {
            continue;
        }
        let dir = match entry.path {
            "Cargo.toml" => "",
            path => match path.strip_suffix("/Cargo.toml") {
                Some(dir) => dir,
                None => continue,
            },
        };
        let bytes = entry
            .read_in(alloc)
            .map_err(|why| SourceError::Refused { package: own(package, alloc), version: own(version.as_str(), alloc), why })?;
        let text = core::str::from_utf8(bytes.as_slice())
            .map_err(|_| SourceError::NotUtf8 { package: own(package, alloc), path: own(entry.path, alloc) })?;
        let Ok(doc) = blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, alloc) else { continue };
        let Some(name) = doc.root().get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) else { continue };
        out.push((own(name, alloc), own(dir, alloc)));
    }
    out.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    Ok(out)
}

/// The layout of the record's files, started at `start` (the record's root is the anchor).
fn lay_out<'a>(view: &PackView<'_>, start: &str, package: &str, version: &Text<'a>, alloc: Alloc<'a>) -> Result<Units<'a>, SourceError<'a>> {
    let refused = |error| SourceError::Layout { package: own(package, alloc), version: version.clone(), error };
    let mut reader = Layout::new(start, alloc).map_err(refused)?;
    for _ in 0..LAYOUT_PASSES {
        let needs = match reader.pass().map_err(refused)? {
            LayoutPass::Done(units) => return Ok(units),
            LayoutPass::Need(needs) => needs,
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let loaded = view
                        .read(path, alloc)
                        .map_err(|why| SourceError::Refused { package: own(package, alloc), version: own(version.as_str(), alloc), why })?;
                    let text = match loaded.as_ref() {
                        Some(file) => Some(
                            core::str::from_utf8(file.bytes.as_slice())
                                .map_err(|_| SourceError::NotUtf8 { package: own(package, alloc), path: path.clone() })?,
                        ),
                        None => None,
                    };
                    reader.manifest(path, text).map_err(refused)?;
                }
                Need::Listing(dir) => {
                    let entries = listing(view, dir, alloc)
                        .map_err(|why| SourceError::Refused { package: own(package, alloc), version: own(version.as_str(), alloc), why })?;
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    Err(SourceError::LayoutPasses { package: own(package, alloc), version: version.clone() })
}

/// The entries of directory `dir` of a record (`""` is its root): the first path segment after
/// `dir/` of each file under it. Files are sorted by path, so the files under one entry are
/// together; `None` when no file is under `dir`.
fn listing<'v, 'a>(view: &PackView<'v>, dir: &str, alloc: Alloc<'a>) -> Result<Option<AVec<'a, Entry<'v>>>, blaze_fetch::PackRefused> {
    if !dir.is_empty()
        && let Some(sub) = view.submodule_covering(dir)
    {
        return Err(blaze_fetch::PackRefused::Submodule {
            path: blaze_fetch::unpack::Cut::of(dir.as_bytes()),
            submodule: blaze_fetch::unpack::Cut::of(sub.path.as_bytes()),
            commit: sub.submodule_commit().unwrap_or([0; 20]),
        });
    }
    let mut prefix: String<Alloc<'a>> = String::new_in(alloc);
    if !dir.is_empty() {
        prefix.push_str(dir);
        prefix.push('/');
    }
    // The first file at or after the prefix, by halving.
    let (mut lo, mut hi) = (0usize, view.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match view.entry(mid).path < prefix.as_str() {
            true => lo = mid + 1,
            false => hi = mid,
        }
    }
    let mut entries: AVec<'a, Entry<'v>> = Vec::new_in(alloc);
    for at in lo..view.len() {
        let entry = view.entry(at);
        let Some(rest) = entry.path.strip_prefix(prefix.as_str()) else { break };
        let (name, is_dir) = match rest.find('/') {
            Some(cut) => (&rest[..cut], true),
            None => (rest, entry.mode == blaze_fetch::Mode::Submodule),
        };
        if entries.last().is_some_and(|e| e.name == name) {
            continue;
        }
        entries.push(Entry { name, is_dir });
    }
    Ok(match entries.is_empty() && !dir.is_empty() {
        true => None,
        false => Some(entries),
    })
}

fn own<'a>(s: &str, alloc: Alloc<'a>) -> Text<'a> {
    String::from_str_in(s, alloc)
}

fn version_text<'a>(lock: &Lock<'_>, id: LockId, alloc: Alloc<'a>) -> Text<'a> {
    let mut v = String::new_in(alloc);
    let _ = core::fmt::Write::write_fmt(&mut v, format_args!("{}", lock.package(id).version));
    v
}

fn shown<'a, T: blaze_text::Show + Clone>(value: &T, alloc: Alloc<'a>) -> Text<'a> {
    let mut v = String::new_in(alloc);
    let _ = core::fmt::Write::write_fmt(&mut v, format_args!("{}", blaze_text::Shown(value.clone())));
    v
}
