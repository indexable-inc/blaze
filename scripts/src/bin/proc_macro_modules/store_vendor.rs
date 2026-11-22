//! `vendor_from` (D112 N2): the fetch step from blaze's fetch store, in place of `cargo vendor`.
//! No network: every package is written from a verified record, and a package the store does
//! not hold is a refusal naming it.
//!
//! What is written, under `<work>/vendor/<name>-<version>/` (cargo's directory source, the
//! layout `cargo vendor --versioned-dirs` makes): the files of the package's record, found at
//! `<store>/records/<key>` by `SourceId::Crate { name, version, sha256 }.key()` (the key of
//! blaze-fetch's plan's `Unit::key()`, from the lock's text alone) and opened by
//! `PackView::open_source`, which refuses a record that is not that `.crate`'s files; and the
//! `.cargo-checksum.json` cargo's directory source checks: `{"files":{<path>:<SHA-256 of its
//! bytes>,..},"package":"<the lock's checksum>"}`, compact, the files in the record's order
//! (sorted by path bytes), as `cargo vendor` writes it.
//!
//! From the checkout's lock, vendor each registry root and the lock closure of every
//! dependency retained in its generated member manifest. From std's own lock
//! (`library/Cargo.lock`, what `-Zbuild-std` builds), vendor every crates.io package. A
//! workspace, path or git package is not vendored, as with `cargo vendor` before (the build's
//! source replacement covers crates.io only; a git root's files come from the store by
//! [`super::git_origin`]).
//!
//! Refused, every one named before anything is written: a dependency the lock pins no version
//! of; a registry package blaze-fetch keys no record for (no checksum, another registry); a
//! package whose record the store does not hold; an optional dependency deeper down that the
//! build's requested features turn on and the lock does not pin among its package's
//! dependencies. Each vendored checkout package's manifest goes through the member's decision
//! ([`member_dependencies::MemberDependencies::select`]) with the features its edges request,
//! so an optional dependency outside that closure is not in it and Cargo never asks for it.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use blaze_fetch::{Mode, PackView, Skip, SourceId, Unit};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{ASortedSet, AString, AVec, Alloc, CollectIn};
use blaze_text::Shown;

use super::{Failure, Origin, Plan, SCRIPT, Wanted, hex, io, member_dependencies, read, sha256, write};

/// Which lock pinned a package.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Which {
    /// The checkout's `Cargo.lock`.
    Checkout,
    /// The toolchain's `library/Cargo.lock`: std's own dependencies.
    Std,
}

impl fmt::Display for Which {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Which::Checkout => "the checkout's lock",
            Which::Std => "std's lock (library/Cargo.lock)",
        })
    }
}

/// A package the build needs that cannot be vendored from the store, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Missing {
    /// A crates.io package the lock pins whose record (`records/<key>`) the store does not hold.
    NotStored { which: Which, name: String, version: String, key: String },
    /// A dependency the crate `of` (`name version`) declares that the lock pins no version of.
    NotPinned { name: String, of: String },
    /// A registry package the lock pins that blaze-fetch keys no record for: its words.
    NoKey { which: Which, name: String, version: String, why: String },
    /// An optional dependency `name` of the package `of` (`name version`) that the features
    /// this build requests turn on (by its feature `feature`), and that the checkout's lock does
    /// not pin among `of`'s dependencies: Cargo's resolution would enable it, so the lock is
    /// not this build's.
    EnabledNotPinned { name: String, of: String, feature: String },
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Missing::NotStored { which, name, version, key } => {
                write!(f, "{name} {version} (pinned by {which}): the store holds no record {key}")
            }
            Missing::NotPinned { name, of } => {
                write!(f, "{name}: a dependency {of} declares, of which the checkout's lock pins no version, so no record can be keyed")
            }
            Missing::NoKey { which, name, version, why } => write!(f, "{name} {version} (pinned by {which}): no record is keyed for it: {why}"),
            Missing::EnabledNotPinned { name, of, feature } => write!(
                f,
                "{name}: an optional dependency {of} declares, enabled by its feature {feature:?} in this build, of which the checkout's lock pins no version among {of}'s dependencies"
            ),
        }
    }
}

/// A crate the run builds, as the store fetch starts from it.
pub(super) struct Root<'r> {
    pub(super) name: &'r str,
    pub(super) version: &'r str,
    /// Features the member asks Cargo to resolve for this package. `default` is explicit when
    /// the input omitted its feature list, matching the build workspace.
    pub(super) features: Vec<String>,
    /// A workspace or git package's manifest (what it inherits written in); `None` for a
    /// registry crate, whose manifest is read from its record.
    pub(super) manifest: Option<&'r str>,
}

/// A lock as given: where it was read (for errors) and its text.
#[derive(Clone, Copy)]
pub(super) struct LockText<'t> {
    pub(super) path: &'t Path,
    pub(super) text: &'t str,
}

/// What [`from_store`] wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Vendored {
    /// Package folders written.
    pub(super) packages: usize,
    /// Of them, pinned by std's lock only.
    pub(super) std_only: usize,
}

/// One package to write: the lock's pin.
struct Pinned {
    name: String,
    version: String,
    sha256: [u8; 32],
    which: Which,
    /// Its index in the lock that pinned it.
    at: usize,
    /// Pinned by the checkout's lock and std's both: one folder serves `-Zbuild-std` too.
    with_std: bool,
}

impl Pinned {
    fn source(&self) -> SourceId<'_> {
        SourceId::Crate { name: &self.name, version: &self.version, sha256: self.sha256 }
    }
}

impl Plan<'_> {
    /// Step one with `vendor_from`: `<work>/vendor` written from the fetch store `store`, and
    /// `<work>/fetch/Cargo.lock` (the lock each build's own is made from) the checkout's.
    /// `cut` is the part of a crate's manifest its build's member takes: all of it for a macro
    /// crate, the build-dependency tables for a script. No network, no program started.
    pub(super) fn vendor_from(&self, store: &Path, cut: fn(&str) -> String, alloc: Alloc<'_>) -> Result<(), Failure> {
        write(&self.work.join("fetch").join("Cargo.lock"), &self.lock)?;
        let roots: Vec<Root<'_>> = self.located().map(|(_, wanted, origin)| root_of(wanted, origin)).collect();
        let checkout_path = self.args.checkout.join("Cargo.lock");
        let std_path = self.tool.library().join("Cargo.lock");
        let std_text = read(&std_path)?;
        let vendored = from_store(
            store,
            &self.work.join("vendor"),
            LockText { path: &checkout_path, text: &self.lock },
            LockText { path: &std_path, text: &std_text },
            &roots,
            &[],
            cut,
            alloc,
        )?;
        println!(
            "{SCRIPT}: vendored {} packages from the fetch store {} ({} pinned by std's lock only); no network",
            vendored.packages,
            store.display(),
            vendored.std_only
        );
        Ok(())
    }

    /// The offline fetch for one crate. A missing record refuses that crate while later roots
    /// still get a chance to vendor and build.
    pub(super) fn vendor_from_one(&self, store: &Path, at: usize, cut: fn(&str) -> String, alloc: Alloc<'_>) -> Result<(), Failure> {
        write(&self.work.join("fetch").join("Cargo.lock"), &self.lock)?;
        let wanted = &self.args.crates[at];
        let origin = self.origin(at)?;
        let root = root_of(wanted, origin);
        // The vendor folder is shared by every crate of the run, so each vendored manifest is
        // decided by what all of them request; the others' own refusals are their calls'.
        let others: Vec<Root<'_>> = self.located().filter(|(other, _, _)| *other != at).map(|(_, wanted, origin)| root_of(wanted, origin)).collect();
        let checkout_path = self.args.checkout.join("Cargo.lock");
        let std_path = self.tool.library().join("Cargo.lock");
        let std_text = read(&std_path)?;
        let vendored = from_store(
            store,
            &self.work.join("vendor"),
            LockText { path: &checkout_path, text: &self.lock },
            LockText { path: &std_path, text: &std_text },
            &[root],
            &others,
            cut,
            alloc,
        )?;
        println!("{SCRIPT}: vendored {} packages for {} {} from the fetch store {} ({} pinned by std's lock only); no network", vendored.packages, wanted.name, wanted.version, store.display(), vendored.std_only);
        Ok(())
    }
}

/// A crate the run builds as the fetch starts from it: its requested features (`default`
/// when the input gave none) and, for a workspace or git package, its manifest.
fn root_of<'r>(wanted: &'r Wanted, origin: &'r Origin) -> Root<'r> {
    Root {
        name: &wanted.name,
        version: &wanted.version,
        features: wanted.features.clone().unwrap_or_else(|| vec!["default".to_owned()]),
        manifest: match origin {
            Origin::Registry { .. } => None,
            Origin::Path { manifest, .. } | Origin::Git { manifest, .. } => Some(manifest.as_str()),
        },
    }
}

/// The packages `roots` reach in `checkout` and every crates.io package of `std`, each written
/// under `vendor` from its record in `store` with its `.cargo-checksum.json`. Every package
/// that cannot be is named in one [`Failure::Unvendored`] before anything is written.
///
/// A checkout package's manifest is written through the member's decision
/// ([`member_dependencies::rewritten_manifest`]) with the features the build's edges request
/// of it ([`Requests`]): an optional dependency no enabled feature turns on is not in it, so
/// Cargo's offline resolve never asks for a package the lock does not pin. `others` are the
/// run's other crates sharing the vendor folder: their requests count, their refusals are
/// their own calls'.
#[allow(clippy::too_many_arguments)]
pub(super) fn from_store<'a>(
    store: &Path,
    vendor: &Path,
    checkout: LockText<'_>,
    std: LockText<'_>,
    roots: &[Root<'_>],
    others: &[Root<'_>],
    cut: fn(&str) -> String,
    alloc: Alloc<'a>,
) -> Result<Vendored, Failure> {
    let refused = |path: &Path, why: String| Failure::Manifest { path: path.to_owned(), why };
    let lock = Lock::read(checkout.text.as_bytes(), alloc).map_err(|why| refused(checkout.path, why.to_string()))?;
    let plan = blaze_fetch::plan::plan(&lock, alloc).map_err(|why| refused(checkout.path, Shown(&why).to_string()))?;
    let std_lock = Lock::read(std.text.as_bytes(), alloc).map_err(|why| refused(std.path, why.to_string()))?;
    let std_plan = blaze_fetch::plan::plan(&std_lock, alloc).map_err(|why| refused(std.path, Shown(&why).to_string()))?;
    let unit_of = units_by_package(&lock, &plan.units);
    let std_unit_of = units_by_package(&std_lock, &std_plan.units);
    let mut missing: Vec<Missing> = Vec::new();

    // The checkout's packages the builds reach, walked with a work list.
    let mut todo: Vec<LockId> = Vec::new();
    let mut registry_roots = blaze_own::AVec::new_in(alloc);
    for root in roots {
        let entry = format!("{} {}", root.name, root.version);
        let id = lock.entry(&entry).ok_or_else(|| Failure::NotLocked { name: root.name.to_owned(), version: root.version.to_owned() })?;
        let manifest = match root.manifest {
            Some(manifest) => manifest.to_owned(),
            None => {
                registry_roots.push(id);
                // A registry root's manifest is in its record; with no record (or no key) the
                // walk below names it, and its declared dependencies wait for it.
                let Some(unit) = unit_of[id.index()].map(|at| &plan.units[at]) else { continue };
                let Some(manifest) = manifest_in_record(store, unit)? else { continue };
                manifest
            }
        };
        let declared = &lock.package(id).deps;
        let cut_manifest = cut(&manifest);
        let manifest_path = Path::new(&entry).join("Cargo.toml");
        let doc = member_dependencies::parse(&cut_manifest, &manifest_path, alloc)?;
        let selected = member_dependencies::MemberDependencies::select(&doc, &root.features, alloc);
        for name in selected.packages() {
            let locked: Vec<LockId> = declared.iter().copied().filter(|dep| lock.package(*dep).name.as_str() == name).collect();
            if !locked.is_empty() {
                todo.extend(locked);
                continue;
            }
            let named: Vec<LockId> = lock.named(name).collect();
            if named.is_empty() {
                missing.push(Missing::NotPinned { name: name.to_owned(), of: entry.clone() });
            }
            todo.extend(named);
        }
    }
    let mut seen = vec![false; lock.packages.len()];
    while let Some(id) = todo.pop() {
        if std::mem::replace(&mut seen[id.index()], true) {
            continue;
        }
        todo.extend(lock.package(id).deps.iter().copied());
    }
    // Vendor a registry root's source. Its direct edges were selected above. A root also
    // reached as a transitive dependency has already had its ordinary lock closure visited.
    for id in registry_roots {
        seen[id.index()] = true;
    }

    // The features the build's edges request of each checkout package. The roots' walks come
    // first and from nothing, so each of their refusals is seen; the others' only add requests
    // (a crate the vendor folder also serves), and what refuses one of them is named by its
    // own call, so their refusals here are not this call's.
    let mut requests = Requests::new(&lock, alloc);
    for root in roots {
        requests.walk(&lock, &plan, &unit_of, store, root, cut, &mut missing, alloc)?;
    }
    for root in others {
        let mut theirs: Vec<Missing> = Vec::new();
        match requests.walk(&lock, &plan, &unit_of, store, root, cut, &mut theirs, alloc) {
            Ok(()) | Err(_) => {}
        }
    }
    let unknown = requests.unknown_below(&lock, alloc);

    let mut pinned: Vec<Pinned> = Vec::new();
    for (at, reached) in seen.iter().enumerate() {
        if *reached {
            pin(&lock, &plan, &unit_of, at, Which::Checkout, &mut pinned, &mut missing);
        }
    }
    for at in 0..std_lock.packages.len() {
        pin(&std_lock, &std_plan, &std_unit_of, at, Which::Std, &mut pinned, &mut missing);
    }
    // One folder per name and version: a stable sort keeps the checkout's pin first.
    pinned.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    let mut kept: Vec<Pinned> = Vec::with_capacity(pinned.len());
    for package in pinned {
        match kept.last_mut() {
            Some(held) if held.name == package.name && held.version == package.version => {
                held.with_std |= package.which == Which::Std;
                if held.sha256 != package.sha256 {
                    return Err(refused(
                        std.path,
                        format!(
                            "{} {} is pinned by {} to {} and by {} to {}: one folder, two sources",
                            package.name,
                            package.version,
                            held.which,
                            hex(&held.sha256),
                            package.which,
                            hex(&package.sha256)
                        ),
                    ));
                }
            }
            _ => kept.push(package),
        }
    }
    let std_only = kept.iter().filter(|it| it.which == Which::Std).count();

    let records = store.join("records");
    for package in &kept {
        let key = hex(&package.source().key().0);
        let record = records.join(&key);
        match std::fs::metadata(&record) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(Missing::NotStored { which: package.which, name: package.name.clone(), version: package.version.clone(), key });
            }
            Err(error) => return Err(Failure::Io { path: record, error }),
        }
    }
    if !missing.is_empty() {
        missing.sort_by_key(|it| it.to_string());
        missing.dedup();
        return Err(Failure::Unvendored { store: store.to_owned(), missing });
    }

    for package in &kept {
        let record = records.join(hex(&package.source().key().0));
        let bytes = std::fs::read(&record).map_err(io(&record))?;
        let view = PackView::open_source(&package.source(), &bytes).map_err(|why| {
            refused(&record, format!("the record of {} {}: {}", package.name, package.version, Shown(&why)))
        })?;
        // The checkout's request of it decides its manifest. Not decided here: a folder std's
        // lock pins too (what `-Zbuild-std` requests of it is std's, not read here), and a
        // package below a manifest the walk could not read (what reaches it is not known).
        let decided = package.which == Which::Checkout && !package.with_std && !unknown[package.at];
        let requested = if decided { requests.features[package.at].as_ref() } else { None };
        let manifest = match requested {
            Some(requested) => {
                let file = view.find("Cargo.toml").ok_or_else(|| refused(&record, "the crate's record holds no Cargo.toml".to_owned()))?;
                let text = std::str::from_utf8(file.bytes).map_err(|error| refused(&record, format!("its Cargo.toml is not UTF-8: {error}")))?;
                let path = Path::new(&format!("{}-{}", package.name, package.version)).join("Cargo.toml");
                member_dependencies::rewritten_manifest(text, &path, requested.as_slice(), alloc)?
            }
            None => None,
        };
        write_package(&vendor.join(format!("{}-{}", package.name, package.version)), &view, manifest.as_ref().map(|text| text.as_str()), &hex(&package.sha256), &record)?;
    }
    Ok(Vendored { packages: kept.len(), std_only })
}

/// Each lock package's crate unit, by the package's index.
fn units_by_package(lock: &Lock<'_>, units: &[Unit<'_, '_>]) -> Vec<Option<usize>> {
    let mut out = vec![None; lock.packages.len()];
    for (at, unit) in units.iter().enumerate() {
        if let Unit::Crate { package, .. } = unit {
            out[package.index()] = Some(at);
        }
    }
    out
}

/// The features the build's dependency edges request of each checkout package: Cargo's
/// feature resolution walked over the lock's graph. A package's request is the union, over
/// the edges that reach it, of the edge's `features`, `default` unless `default-features =
/// false`, and what its dependent's enabled closure forwards to it; its edges are those the
/// one decision ([`member_dependencies::MemberDependencies::select`]) keeps from its request.
struct Requests<'a> {
    /// By lock index: the features requested; `None` while no edge reaches the package.
    features: AVec<'a, Option<ASortedSet<'a, AString<'a>>>>,
    /// By lock index: the manifest from the package's record once read; `Some(None)` when it
    /// has none (a path or git package, a record the store does not hold).
    manifests: AVec<'a, Option<Option<String>>>,
    /// By lock index: reached, but no manifest was read, so its own edges are not known.
    unread: AVec<'a, bool>,
}

impl<'a> Requests<'a> {
    fn new(lock: &Lock<'_>, alloc: Alloc<'a>) -> Self {
        let count = lock.packages.len();
        Requests {
            features: (0..count).map(|_| None).collect_in(alloc),
            manifests: (0..count).map(|_| None).collect_in(alloc),
            unread: (0..count).map(|_| false).collect_in(alloc),
        }
    }

    /// Adds `features` to the `at`th package's request: `true` when it was first reached or
    /// the request grew, so its edges are walked (again).
    fn request<'f>(&mut self, at: usize, features: impl Iterator<Item = &'f str>, alloc: Alloc<'a>) -> bool {
        let mut grew = self.features[at].is_none();
        let set = self.features[at].get_or_insert_with(|| ASortedSet::new_in(alloc));
        for feature in features {
            if !set.contains(feature) {
                set.insert(AString::from_str_in(feature, alloc));
                grew = true;
            }
        }
        grew
    }

    /// The `at`th package's manifest from its record, read once.
    fn manifest(&mut self, plan: &blaze_fetch::Plan<'_, '_>, unit_of: &[Option<usize>], store: &Path, at: usize) -> Result<Option<String>, Failure> {
        if self.manifests[at].is_none() {
            let read = match unit_of[at] {
                Some(unit) => manifest_in_record(store, &plan.units[unit])?,
                None => None,
            };
            self.manifests[at] = Some(read);
        }
        Ok(self.manifests[at].clone().flatten())
    }

    /// Walks from `root` with its requested features to a fixed point: each package reached is
    /// (re)read with its whole request, and its kept edges add to their packages' requests.
    /// A kept edge the lock does not pin among the package's dependencies is named in
    /// `missing`: an optional one as [`Missing::EnabledNotPinned`] with the feature that turned
    /// it on. The root's own edges are named by the fetch walk, by the same rule.
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        lock: &Lock<'_>,
        plan: &blaze_fetch::Plan<'_, '_>,
        unit_of: &[Option<usize>],
        store: &Path,
        root: &Root<'_>,
        cut: fn(&str) -> String,
        missing: &mut Vec<Missing>,
        alloc: Alloc<'a>,
    ) -> Result<(), Failure> {
        let entry = format!("{} {}", root.name, root.version);
        let start = lock.entry(&entry).ok_or_else(|| Failure::NotLocked { name: root.name.to_owned(), version: root.version.to_owned() })?;
        let mut todo: AVec<'a, LockId> = AVec::new_in(alloc);
        if self.request(start.index(), root.features.iter().map(String::as_str), alloc) {
            todo.push(start);
        }
        while let Some(id) = todo.pop() {
            let at = id.index();
            let is_root = at == start.index();
            // The root's manifest is its build's member's: `cut` of what it has.
            let text = match (is_root, root.manifest) {
                (true, Some(manifest)) => Some(cut(manifest)),
                (true, None) => self.manifest(plan, unit_of, store, at)?.map(|whole| cut(&whole)),
                (false, _) => self.manifest(plan, unit_of, store, at)?,
            };
            let Some(text) = text else {
                self.unread[at] = true;
                continue;
            };
            let package = lock.package(id);
            let of = format!("{} {}", package.name.as_str(), package.version);
            let requested: AVec<'a, AString<'a>> =
                self.features[at].iter().flat_map(|set| set.iter()).map(|feature| AString::from_str_in(feature.as_str(), alloc)).collect_in(alloc);
            let doc = member_dependencies::parse(&text, &Path::new(&of).join("Cargo.toml"), alloc)?;
            let selected = member_dependencies::MemberDependencies::select(&doc, requested.as_slice(), alloc);
            for edge in selected.edges(alloc).iter() {
                let mut targets: AVec<'a, LockId> =
                    package.deps.iter().copied().filter(|dep| lock.package(*dep).name.as_str() == edge.package).collect_in(alloc);
                if is_root && targets.is_empty() {
                    // As the fetch walk takes the root's edges (it names one no entry pins).
                    targets.extend(lock.named(edge.package));
                }
                if targets.is_empty() {
                    if !is_root {
                        missing.push(match edge.enabled_by {
                            Some(feature) => Missing::EnabledNotPinned { name: edge.package.to_owned(), of: of.clone(), feature: feature.to_owned() },
                            None => Missing::NotPinned { name: edge.package.to_owned(), of: of.clone() },
                        });
                    }
                    continue;
                }
                for target in targets.iter() {
                    if self.request(target.index(), edge.features.iter().copied(), alloc) {
                        todo.push(*target);
                    }
                }
            }
        }
        Ok(())
    }

    /// By lock index: whether the package is at or below one whose manifest was not read, in
    /// the lock's graph: what is requested of it is then not known.
    fn unknown_below(&self, lock: &Lock<'_>, alloc: Alloc<'a>) -> AVec<'a, bool> {
        let mut below: AVec<'a, bool> = self.unread.iter().copied().collect_in(alloc);
        let mut todo: AVec<'a, LockId> = (0..below.len()).filter(|&at| below[at]).map(|at| LockId(at as u32)).collect_in(alloc);
        while let Some(id) = todo.pop() {
            for &dep in lock.package(id).deps.iter() {
                if !std::mem::replace(&mut below[dep.index()], true) {
                    todo.push(dep);
                }
            }
        }
        below
    }
}

/// The `at`th package of `lock` as a pin to write, or why it cannot be one; a workspace, path
/// or git package is none. `plan` is blaze-fetch's of `lock`, `unit_of` its units by package.
fn pin(
    lock: &Lock<'_>,
    plan: &blaze_fetch::Plan<'_, '_>,
    unit_of: &[Option<usize>],
    at: usize,
    which: Which,
    pinned: &mut Vec<Pinned>,
    missing: &mut Vec<Missing>,
) {
    let package = &lock.packages[at];
    if !matches!(package.source, LockSource::Registry { .. }) {
        return;
    }
    let skipped: &[Skip<'_>] = &plan.skipped;
    match unit_of[at].map(|unit| &plan.units[unit]) {
        Some(Unit::Crate { name, version, sha256, .. }) => {
            pinned.push(Pinned { name: (*name).to_owned(), version: version.as_str().to_owned(), sha256: *sha256, which, at, with_std: false });
        }
        Some(Unit::Git { .. }) | None => {
            let name = package.name.as_str();
            let why = skipped
                .iter()
                .find(|skip| match skip {
                    Skip::NoChecksum { name: skipped } | Skip::OtherRegistry { name: skipped, .. } | Skip::GitNotHttps { name: skipped, .. } => {
                        *skipped == name
                    }
                })
                .map_or_else(|| "blaze-fetch's plan of the lock has no unit for it".to_owned(), |skip| Shown(skip).to_string());
            missing.push(Missing::NoKey { which, name: name.to_owned(), version: package.version.to_string(), why });
        }
    }
}

/// A registry crate's `Cargo.toml` from its record; `None` when the store holds no record (the
/// walk names it).
fn manifest_in_record(store: &Path, unit: &Unit<'_, '_>) -> Result<Option<String>, Failure> {
    let record = store.join("records").join(hex(&unit.key().0));
    let bytes = match std::fs::read(&record) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Failure::Io { path: record, error }),
    };
    let refuse = |why: String| Failure::Manifest { path: record.clone(), why };
    let view = PackView::open_source(&unit.source(), &bytes).map_err(|why| refuse(format!("{}", Shown(&why))))?;
    let file = view.find("Cargo.toml").ok_or_else(|| refuse("the crate's record holds no Cargo.toml".to_owned()))?;
    let text = std::str::from_utf8(file.bytes).map_err(|error| refuse(format!("its Cargo.toml is not UTF-8: {error}")))?;
    Ok(Some(text.to_owned()))
}

/// One package's folder, written afresh from `view` (anything there before is removed: the
/// folder is the run's own), and its `.cargo-checksum.json` with `checksum` as `package`.
/// `manifest`, when given, is written as its `Cargo.toml` in place of the record's, and is
/// what the checksum file hashes.
fn write_package(dir: &Path, view: &PackView<'_>, manifest: Option<&str>, checksum: &str, record: &Path) -> Result<(), Failure> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::Io { path: dir.to_owned(), error }),
    }
    let mut files = String::new();
    for file in view.files() {
        let path: PathBuf = dir.join(file.path);
        let bytes = match manifest {
            Some(manifest) if file.path == "Cargo.toml" => manifest.as_bytes(),
            _ => file.bytes,
        };
        match file.mode {
            Mode::File => write(&path, bytes)?,
            Mode::Exec => {
                use std::os::unix::fs::PermissionsExt as _;
                write(&path, bytes)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(io(&path))?;
            }
            Mode::Link => {
                return Err(Failure::Manifest {
                    path: record.to_owned(),
                    why: format!("{} is a link: a crate's link is not vendored yet (cargo's directory source checks files)", file.path),
                });
            }
        }
        if !files.is_empty() {
            files.push(',');
        }
        let quoted = serde_json::to_string(file.path).map_err(|error| Failure::Manifest { path: record.to_owned(), why: error.to_string() })?;
        files.push_str(&format!("{quoted}:\"{}\"", sha256(bytes)));
    }
    write(&dir.join(".cargo-checksum.json"), format!("{{\"files\":{{{files}}},\"package\":\"{checksum}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock's checksum of the fake crate's `.crate` (any 64 hex digits: no `.crate` is read).
    const CHECKSUM: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const MANIFEST: &str = "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n";
    /// The fixture: what cargo's directory source checks, compact, `files` (each file's SHA-256,
    /// by `shasum -a 256` of the bytes above) before `package` (the lock's checksum).
    const CHECKSUM_JSON: &str = "{\"files\":{\"Cargo.toml\":\"1ea0298919290d9b20cb2aa15ca55459619b1333b0cdeeea530ace925789e34b\",\"src/lib.rs\":\"1afe9973bc88e19065efa92c68f873d5c8436c344948d1b05c0c4b2bb42ae4cd\"},\"package\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"}";
    const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";
    const NO_STD: &str = "version = 4\n";

    fn sha(text: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (at, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).expect("hex");
        }
        out
    }

    /// A fresh folder of the test's own.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-store-vendor-{}-{name}", std::process::id()));
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{}: {error}", dir.display()),
        }
        dir
    }

    /// A store holding one record: `tiny 0.1.0`'s two files, made by blaze-fetch's builder.
    fn store_with_tiny(store: &Path, alloc: Alloc<'_>) {
        store_with_tiny_manifest(store, MANIFEST, alloc);
    }

    fn store_with_tiny_manifest(store: &Path, manifest: &str, alloc: Alloc<'_>) {
        store_crate(store, "tiny", "0.1.0", manifest, alloc);
    }

    /// A record of `name version` (the lock's checksum [`CHECKSUM`]) with `manifest` and [`LIB`].
    fn store_crate(store: &Path, name: &str, version: &str, manifest: &str, alloc: Alloc<'_>) {
        let limits = blaze_fetch::pack::Limits { most_files: 16, most_bytes: 1 << 20, most_path: 256 };
        let mut builder = blaze_fetch::pack::Builder::new(blaze_fetch::pack::Kind::Crate, sha(CHECKSUM), limits, 0, alloc).expect("a builder");
        builder.file("Cargo.toml", Mode::File, manifest.as_bytes()).expect("Cargo.toml");
        builder.file("src/lib.rs", Mode::File, LIB.as_bytes()).expect("src/lib.rs");
        let record = builder.finish().expect("the record");
        let key = SourceId::Crate { name, version, sha256: sha(CHECKSUM) }.key();
        let path = store.join("records").join(hex(&key.0));
        std::fs::create_dir_all(store.join("records")).expect("records/");
        std::fs::write(&path, &record[..]).expect("the record written");
    }

    fn package(name: &str, version: &str, checksum: &str, deps: &[&str]) -> String {
        let mut out = format!("\n[[package]]\nname = {name:?}\nversion = {version:?}\n");
        if !checksum.is_empty() {
            out.push_str(&format!("source = {REGISTRY:?}\nchecksum = {checksum:?}\n"));
        }
        if !deps.is_empty() {
            out.push_str("dependencies = [\n");
            for dep in deps {
                out.push_str(&format!(" {dep:?},\n"));
            }
            out.push_str("]\n");
        }
        out
    }

    /// N2's fixture test: a store with one record, and the vendored folder and its
    /// `.cargo-checksum.json` byte-equal to what cargo's directory source wants. Prints
    /// "vendored from the store: tiny 0.1.0, byte-equal to the fixture".
    #[test]
    fn a_stored_crate_is_vendored_byte_equal_to_the_fixture() {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = folder("fixture");
        let (store, vendor) = (dir.join("store"), dir.join("work/vendor"));
        store_with_tiny(&store, alloc);
        let lock = format!("version = 4\n{}", package("tiny", "0.1.0", CHECKSUM, &[]));
        let roots = [Root { name: "tiny", version: "0.1.0", features: vec!["default".to_owned()], manifest: None }];
        let vendored = match from_store(
            &store,
            &vendor,
            LockText { path: Path::new("Cargo.lock"), text: &lock },
            LockText { path: Path::new("library/Cargo.lock"), text: NO_STD },
            &roots,
            &[],
            |manifest| manifest.to_owned(),
            alloc,
        ) {
            Ok(vendored) => vendored,
            Err(failure) => panic!("refused: {failure}"),
        };
        assert_eq!(vendored, Vendored { packages: 1, std_only: 0 });
        let folder = vendor.join("tiny-0.1.0");
        let text = |path: &str| std::fs::read_to_string(folder.join(path)).unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(text("Cargo.toml"), MANIFEST);
        assert_eq!(text("src/lib.rs"), LIB);
        assert_eq!(text(".cargo-checksum.json"), CHECKSUM_JSON);
        println!("vendored from the store: tiny 0.1.0, byte-equal to the fixture");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the store lacks is refused by name, and nothing is written: `gone 1.0.0` (under
    /// `tiny`, no record) and `absent` (declared by the workspace member, pinned by no lock
    /// entry). Prints "refused by name: gone 1.0.0, absent".
    #[test]
    fn a_package_missing_from_the_store_is_refused_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = folder("missing");
        let (store, vendor) = (dir.join("store"), dir.join("work/vendor"));
        store_with_tiny(&store, alloc);
        let gone = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let lock = format!(
            "version = 4\n{}{}{}",
            package("gone", "1.0.0", gone, &[]),
            package("member", "0.1.0", "", &["tiny"]),
            package("tiny", "0.1.0", CHECKSUM, &["gone"]),
        );
        let member = "[package]\nname = \"member\"\nversion = \"0.1.0\"\n\n[dependencies]\ntiny = \"0.1\"\nabsent = { version = \"1\", optional = true }\n\n[features]\nneeds-absent = [\"dep:absent\"]\n\n[dev-dependencies]\nnot-built = \"1\"\n";
        let roots = [Root { name: "member", version: "0.1.0", features: vec!["needs-absent".to_owned()], manifest: Some(member) }];
        let result = from_store(
            &store,
            &vendor,
            LockText { path: Path::new("Cargo.lock"), text: &lock },
            LockText { path: Path::new("library/Cargo.lock"), text: NO_STD },
            &roots,
            &[],
            |manifest| manifest.to_owned(),
            alloc,
        );
        let Err(Failure::Unvendored { missing, .. }) = result else { panic!("a store without gone 1.0.0 was not refused") };
        let named: Vec<String> = missing.iter().map(|it| it.to_string()).collect();
        assert_eq!(named.len(), 2, "{named:?}");
        assert!(named.iter().any(|it| it.starts_with("gone 1.0.0 (pinned by the checkout's lock)")), "{named:?}");
        assert!(named.iter().any(|it| it.starts_with("absent: a dependency member 0.1.0 declares")), "{named:?}");
        assert!(!vendor.join("tiny-0.1.0").exists(), "a refused fetch wrote tiny");
        println!("refused by name: gone 1.0.0, absent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tables read, renames by `package`, and `dev-dependencies` left out.
    #[test]
    fn dependency_tables_are_read_by_package() {
        let manifest = "[package]\nname = \"m\"\n\n[dependencies]\nserde = \"1\"\nrenamed = { package = \"real\", version = \"2\" }\n\n[dependencies.tabled]\nversion = \"3\"\npackage = \"tabled-real\"\n\n[target.'cfg(target_os = \"linux\")'.build-dependencies]\ncc = \"1\"\n\n[dev-dependencies]\nproptest = \"1\"\n";
        let alloc = blaze_own::testing::heap().alloc();
        let doc = member_dependencies::parse(manifest, Path::new("Cargo.toml"), alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let selected = member_dependencies::MemberDependencies::select::<String>(&doc, &[], alloc);
        let mut packages: Vec<_> = selected.packages().collect();
        packages.sort_unstable();
        assert_eq!(packages, ["cc", "real", "serde", "tabled-real"]);
    }

    #[test]
    fn registry_root_follows_only_selected_lock_edges() {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = folder("registry-edges");
        let (store, vendor) = (dir.join("store"), dir.join("vendor"));
        let manifest = r#"
[package]
name = "tiny"
version = "0.1.0"
[features]
"use-alias" = ["alias/syntax"]
[target.'cfg(unix)'.build-dependencies.alias]
package = "real"
version = "1"
optional = true
[dev-dependencies.dev-only]
version = "1"
"#;
        store_with_tiny_manifest(&store, manifest, alloc);
        let lock = format!("version = 4\n{}{}{}",
            package("tiny", "0.1.0", CHECKSUM, &["real", "dev-only"]),
            package("real", "1.0.0", CHECKSUM, &[]),
            package("dev-only", "1.0.0", CHECKSUM, &[]));
        let vendor_with = |features| from_store(
            &store, &vendor, LockText { path: Path::new("Cargo.lock"), text: &lock },
            LockText { path: Path::new("library/Cargo.lock"), text: NO_STD },
            &[Root { name: "tiny", version: "0.1.0", features, manifest: None }],
            &[], str::to_owned, alloc,
        );
        assert_eq!(vendor_with(vec![]).unwrap_or_else(|failure| panic!("{failure}")), Vendored { packages: 1, std_only: 0 });
        assert!(vendor.join("tiny-0.1.0/Cargo.toml").is_file());
        let Err(Failure::Unvendored { missing, .. }) = vendor_with(vec!["use-alias".to_owned()]) else { panic!("expected the missing real package") };
        assert!(matches!(&missing[..], [Missing::NotStored { name, .. }] if name == "real"), "{missing:?}");
        println!("registry root: disabled alias vendors only tiny; enabled alias demands real");
        std::fs::remove_dir_all(dir).expect("remove registry fixture");
    }

    /// A vendored dependency (not the member) declaring an optional dependency only its
    /// feature `include-exclude` turns on, in `[dependencies.NAME]` table form.
    const UTILS: &str = "[package]\nname = \"utils\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies.globset]\nversion = \"0.4\"\noptional = true\n\n[features]\ninclude-exclude = [\"globset\"]\n";
    const GLOBSET: &str = "[package]\nname = \"globset\"\nversion = \"0.4.0\"\nedition = \"2021\"\n";

    /// The member `m` (a workspace root) depending on `utils`, with `request` as its declaration.
    fn vendor_utils(name: &str, request: &str, lock_pins_globset: bool) -> (PathBuf, Result<Vendored, Failure>) {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = folder(name);
        let (store, vendor) = (dir.join("store"), dir.join("work/vendor"));
        store_crate(&store, "utils", "0.1.0", UTILS, alloc);
        store_crate(&store, "globset", "0.4.0", GLOBSET, alloc);
        let lock = if lock_pins_globset {
            format!(
                "version = 4\n{}{}{}",
                package("globset", "0.4.0", CHECKSUM, &[]),
                package("m", "0.1.0", "", &["utils"]),
                package("utils", "0.1.0", CHECKSUM, &["globset"]),
            )
        } else {
            format!("version = 4\n{}{}", package("m", "0.1.0", "", &["utils"]), package("utils", "0.1.0", CHECKSUM, &[]))
        };
        let member = format!("[package]\nname = \"m\"\nversion = \"0.1.0\"\n\n[dependencies]\nutils = {request}\n");
        let roots = [Root { name: "m", version: "0.1.0", features: vec!["default".to_owned()], manifest: Some(member.as_str()) }];
        let result = from_store(
            &store,
            &vendor,
            LockText { path: Path::new("Cargo.lock"), text: &lock },
            LockText { path: Path::new("library/Cargo.lock"), text: NO_STD },
            &roots,
            &[],
            |manifest| manifest.to_owned(),
            alloc,
        );
        (dir, result)
    }

    /// Cargo's feature resolution: an optional dependency is built only when an enabled
    /// feature of its package turns it on, and `m` requests no feature of `utils` (`default`
    /// only, which `utils` does not declare). So the vendored `utils` declares no `globset`
    /// dependency, its feature keeps its name with no entry, `globset`'s implicit feature is
    /// declared empty, and the checksum file hashes the bytes
    /// written. Prints "vendored utils without globset".
    #[test]
    fn an_optional_dependency_of_a_vendored_package_outside_its_closure_is_removed() {
        let (dir, result) = vendor_utils("utils-off", "\"0.1\"", false);
        assert_eq!(result.unwrap_or_else(|failure| panic!("refused: {failure}")), Vendored { packages: 1, std_only: 0 });
        let folder = dir.join("work/vendor/utils-0.1.0");
        let manifest = std::fs::read_to_string(folder.join("Cargo.toml")).expect("utils' Cargo.toml");
        assert!(!manifest.contains("[dependencies.globset]"), "{manifest}");
        let alloc = blaze_own::testing::heap().alloc();
        let doc = member_dependencies::parse(&manifest, Path::new("Cargo.toml"), alloc).unwrap_or_else(|failure| panic!("{failure}"));
        // globset's implicit feature stays, declared empty (member_dependencies::implicit_features).
        let implicit = doc.root().get("features").and_then(|table| table.get("globset")).unwrap_or_else(|| panic!("the implicit feature is gone: {manifest}"));
        assert_eq!(implicit.children().count(), 0, "{manifest}");
        let feature = doc.root().get("features").and_then(|table| table.get("include-exclude")).unwrap_or_else(|| panic!("the feature is gone: {manifest}"));
        assert_eq!(feature.children().count(), 0, "{manifest}");
        let checksums = std::fs::read_to_string(folder.join(".cargo-checksum.json")).expect("the checksum file");
        assert!(checksums.contains(&format!("\"Cargo.toml\":\"{}\"", sha256(manifest.as_bytes()))), "{checksums}");
        println!("vendored utils without globset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `m` requests `include-exclude` of `utils`, which turns `globset` on: with the lock
    /// pinning it, `utils` is written as its record has it. Prints "vendored utils with globset".
    #[test]
    fn an_optional_dependency_its_requested_feature_enables_stays() {
        let (dir, result) = vendor_utils("utils-on", "{ version = \"0.1\", features = [\"include-exclude\"] }", true);
        assert_eq!(result.unwrap_or_else(|failure| panic!("refused: {failure}")), Vendored { packages: 2, std_only: 0 });
        let manifest = std::fs::read_to_string(dir.join("work/vendor/utils-0.1.0/Cargo.toml")).expect("utils' Cargo.toml");
        assert_eq!(manifest, UTILS);
        println!("vendored utils with globset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lock is the oracle: the closure turns `globset` on, and a lock that does not pin it
    /// among `utils`' dependencies cannot be this build's. Refused by name: the package, the
    /// dependency and the feature. Prints "refused: globset enabled by include-exclude".
    #[test]
    fn an_enabled_optional_dependency_the_lock_lacks_is_refused() {
        let (dir, result) = vendor_utils("utils-unpinned", "{ version = \"0.1\", features = [\"include-exclude\"] }", false);
        let Err(Failure::Unvendored { missing, .. }) = result else { panic!("an enabled globset the lock lacks was not refused") };
        assert_eq!(
            missing,
            [Missing::EnabledNotPinned { name: "globset".to_owned(), of: "utils 0.1.0".to_owned(), feature: "include-exclude".to_owned() }],
        );
        assert!(!dir.join("work/vendor/utils-0.1.0").exists(), "a refused fetch wrote utils");
        println!("refused: globset enabled by include-exclude");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
