//! The sealed layer: the records of a sealed tree, kept once a machine and read in place by
//! every home's store (/tmp/plan/7-cold-disk.md, task A; docs/v2/plan/design/end-state.md).
//!
//! **Sealed** is one rule, for any crate: its sources cannot change. A crate is sealed exactly
//! when it is read from a tree named by a digest that pins its files ([`pin`], the one place
//! that decides it): a tree the toolchain ships (`library/DIGEST`, bstd's tree), a registry
//! package at the checksum its `.crate` was checked against, a git package at the whole commit
//! its lock file pins. A workspace member and a path dependency never are, whatever their
//! origin. Nothing here names a crate. What the toolchain ships is a list its driver makes
//! (`cli::store::shipped`). The planner writes the pin on each crate of the package
//! (`base_db::CrateData::sealed`, through `blaze_lower::Lower::sealed`), and every other
//! question of sealed asks that field (`hir_def::deps::sealed`) or this rule.
//!
//! **How a sealed crate is told from one that is edited.** By where it is read from, never by
//! its name: a crate whose manifest is under a placed tree (`<machine>/<tree>/<digest>/`, made
//! by blaze and checked file by file against the digest it is named by) is sealed; the same
//! package at a path a person edits (a path dependency into a checkout, `crates/blaze-std` of
//! blaze's own tree, a workspace member) is not, whatever it holds, and its crates are built
//! and kept in its home like any other path crate's. [`Placed::sealed_at`] is that test. A
//! tree a build reads through a mount of blaze's own is told by that mount too
//! ([`Placed::mounted`]): std's crates are named `/rustc/<commit>/library/...` in every plan,
//! never by the placed directory.
//!
//! **The cover.** A sealed tree's records are found by the key each asker already computes;
//! the [`Cover`] says which pack holds them and which layer it is installed as: the hash of
//! the tree's digest, the covers of the sealed trees it depends on, what makes the records
//! (`serve::RecordMakers::id`) and the host. A cover that matches nothing installed is a
//! miss: the tree's crates are built in the home that asked, said once by name. An installed
//! layer whose bytes do not verify is a refusal of the store that reads it (every value is
//! checked against its hash), never a quiet build from source (AGENTS.md rule 3).
//!
//! **Where a layer is.** One shared directory a machine and maker (`<machine>/std/<makers>`,
//! its `store`), one writer directory in it a cover (`w-<writer(cover)>`, [`writer`]): what
//! blaze-cas's shared stores already are, so a home's store reads every installed cover as a
//! layer behind its own records and never writes it.
//!
//! **What a build puts into the layer, and when another home sees it.** A sealed unit a home
//! builds (every crate of std, by [`pin`]) goes into that daemon's writer of the layer alone
//! (`World::std`), never its home: its outputs as the unit ends (`Compiled::share`, put on the
//! loop), its record and slot after the build's answer (`work::PutInto::Layer`, filed by the
//! after-answer task), and its crate's front-end records with its kept front end. One writer,
//! put in that order, so any commit that holds a record holds every output it names (and a
//! reader takes a record only with its outputs there: `unit::compile`'s `found`). Nothing on
//! the path that answers the build waits for the layer: the job's end flushes it, and its
//! tickets settle after the answer (`Jobs`' `Settling::layer`, checked with the layer's own
//! `durable`); the build's certificate is written only once they have, so the daemon says a
//! build kept only after its layer records are durable. Another home reads only what a commit
//! made durable (a writer's head names its packs; its own daemon alone reads it live,
//! `Store::read_live`): while the first home is still committing, a second home's lookup is a
//! miss and it builds the unit itself, putting equal bytes under the same key into its own
//! writer; never a torn record. Other bytes under a key the layer holds are the store's typed
//! refusal (`blaze_cas::Mismatch`). Proved by the simulation
//! `two_homes_building_std_at_once_leave_a_second_home_every_record_they_reported_kept`
//! (blaze-cas `tests/layers.rs`).
//!
//! **One lookup, in order** ([`Install`]): the layer (one `exists` of its head); then, unless
//! prebuilt records are turned off (`"prebuilt": false` in `packs.json`), each configured
//! source by the cover's pin ([`pin`]), its pack checked against its signed manifest record by
//! record and imported into the layer, not a home; then nothing: the crates are built here.
//! Two installs of one cover on one machine import once: the second takes the cover's lock
//! after the first and finds its head.

use std::path::PathBuf;

use blaze_ids::{ContentId, Hasher};

use crate::serve::packs::Base;

/// The domain of [`cover`].
pub const COVER_DOMAIN: &str = "blaze v2 sealed cover 1";
/// The domain of [`writer`].
pub const WRITER_DOMAIN: &str = "blaze v2 sealed writer 1";

/// Which records a sealed tree's pack holds and which layer they are installed as.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Cover(pub ContentId);

/// What a [`Cover`] is made of.
#[derive(Clone, Copy, Debug)]
pub struct CoverOf<'a> {
    /// The tree's digest as the tree states it (`library/DIGEST`, a crate's checksum).
    pub tree: &'a str,
    /// The covers of the sealed trees its crates depend on, in any order.
    pub on: &'a [Cover],
    /// What makes the records ([`crate::serve::RecordMakers::id`]).
    pub makers: ContentId,
    /// The host's target triple.
    pub host: &'a str,
}

/// The cover of `of`: each field length first, in order; the dependencies sorted, so their
/// order in a manifest moves nothing.
pub fn cover(of: &CoverOf<'_>) -> Cover {
    let mut on: Vec<Cover> = of.on.to_vec(); // alloc: once a cover, a tree's few dependencies
    on.sort_unstable();
    on.dedup();
    let mut hasher = Hasher::new(COVER_DOMAIN);
    hasher.part(of.tree.as_bytes());
    hasher.update(&(on.len() as u64).to_le_bytes());
    for it in &on {
        hasher.id(&it.0);
    }
    hasher.id(&of.makers).part(of.host.as_bytes());
    Cover(hasher.finish())
}

/// The pin a cover's pack is published and imported under.
pub fn pack_pin(cover: Cover) -> String {
    format!("sealed-{:x}", cover.0) // alloc: a name, once a cover
}

/// The writer of the shared directory a cover's layer is ([`blaze_cas::SharedConfig::writer`]).
pub fn writer(cover: Cover) -> ContentId {
    ContentId::of(WRITER_DOMAIN, &cover.0.0)
}

/// The directory of a writer in a shared store's directory, as blaze-cas names it.
pub fn writer_dir(shared: &std::path::Path, cover: Cover) -> PathBuf {
    shared.join("store").join(format!("w-{:x}", writer(cover)))
}

/// A sealed tree placed on this machine: its directory, where builds name its crates when
/// that is not it, the digest it is named by and its cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The tree's directory, `<machine>/<tree>/<digest>`.
    pub dir: PathBuf,
    /// Where a build names the tree's crates when it reads them through a mount of blaze's
    /// own rather than from [`Placed::dir`] ([`Tree::mounted`]): the library, mounted at
    /// `/rustc/<commit>/library` from its texts file, so every crate of std is named there.
    pub mounted: Option<PathBuf>,
    /// The digest it is named by, as [`Tree::digest`] states it.
    pub digest: String,
    pub cover: Cover,
}

/// The domain of [`Pin::digest`].
pub const PIN_DOMAIN: &str = "blaze v2 sealed pin 1";

/// What pins a sealed crate's sources: the tree it is read from, named by a digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin<'a> {
    /// Its manifest is under a tree this machine placed and checked against its digest.
    Shipped(&'a Placed),
    /// A registry package at the checksum its lock file names, which its `.crate` was checked
    /// against as it was unpacked.
    Registry(&'a [u8; 32]),
    /// A package of a git source, read from the checkout of the whole commit its lock file
    /// pins (`<sources>/git/checkouts/<ident>/<commit>`).
    Git(&'a str),
}

impl Pin<'_> {
    /// The digest of the tree as text: what a [`CoverOf::tree`] is, each kind under its own
    /// word, so no two kinds' digests are one text.
    pub fn tree(&self) -> String {
        match self {
            // clone: once a pin, a digest's text
            Pin::Shipped(placed) => placed.digest.clone(),
            Pin::Registry(sum) => format!("registry-{}", blaze_format::hex::encode(&sum[..])), // alloc: as above
            Pin::Git(commit) => format!("git-{commit}"), // alloc: as above
        }
    }

    /// What the planner writes on each crate of the package (`base_db::CrateData::sealed`).
    pub fn digest(&self) -> ContentId {
        let mut hasher = Hasher::new(PIN_DOMAIN);
        hasher.part(self.tree().as_bytes());
        hasher.finish()
    }
}

/// What pins `package`'s sources, or none: the one rule of what is sealed (AGENTS.md rule 30).
/// `sources` is where this home keeps fetched packages (`BuildConfig::sources`); `shipped`
/// lists the trees the toolchain ships, asked only for a package no checksum or commit pins
/// and for a workspace member.
/// A workspace member never is, but one read through a shipped tree's mount ([`Placed::mounted`]);
/// then, in order, a package whose manifest is under a shipped tree (its directory or its
/// mount), one with a registry checksum, one of a git source read from the checkout of a whole
/// commit. Any other (a path dependency, whatever its origin, a directory source) is not.
///
/// The member that is sealed: the library is loaded as one more workspace (`-Zbuild-std`'s
/// graph, `blaze_units::stdlib`), so `std`, `sysroot` and `blaze_inproc_rt`, members of
/// `library/Cargo.toml`, are members in the plan (`blaze_lower::crates::proc_macro_cwd`
/// relies on it). They are read through blaze's mount of the placed library, which no person
/// edits (its files are the texts file's, checked against the digest as the tree was placed),
/// so they are sealed as `core` and `alloc` are. Until 2026-11-13 no crate of std was sealed:
/// the plan names them under the mount (`/rustc/<commit>/library/...`) and the test compared
/// the placed directory alone, so every std unit's record and outputs went into each home and
/// a second home built std again (a hello-world's cold build, 7.6 s of 7.7, in every new home).
/// A member under a tree's placed directory (a person building in it) is still not sealed.
pub fn pin<'a, 's: 'a, E>(
    package: &'a blaze_units::query::PackagePlan,
    sources: &std::path::Path,
    shipped: impl FnOnce() -> Result<&'s [Placed], E>,
) -> Result<Option<Pin<'a>>, E> {
    let manifest = std::path::Path::new(&package.manifest_path);
    let dir = manifest.parent().unwrap_or(std::path::Path::new(""));
    if package.is_member {
        return Ok(Placed::mounted_at(shipped()?, dir).map(Pin::Shipped));
    }
    if let Some(sum) = &package.checksum {
        return Ok(Some(Pin::Registry(sum)));
    }
    let git = package.source.as_deref().is_some_and(|it| it.contains("://"));
    if git && let Some(commit) = checkout_commit(sources, dir) {
        return Ok(Some(Pin::Git(commit)));
    }
    Ok(Placed::sealed_at(shipped()?, dir).map(Pin::Shipped))
}

/// The commit whose checkout `dir` is in, `<sources>/git/checkouts/<ident>/<commit>/...`, when
/// the commit is whole (40 or 64 hex digits, as the load names checkouts): a prefix pins
/// nothing.
fn checkout_commit<'d>(sources: &std::path::Path, dir: &'d std::path::Path) -> Option<&'d str> {
    let rest = dir.strip_prefix(sources.join("git").join("checkouts")).ok()?;
    let mut parts = rest.components();
    let (_ident, commit) = (parts.next()?, parts.next()?);
    let commit = commit.as_os_str().to_str()?;
    let whole = matches!(commit.len(), 40 | 64) && commit.bytes().all(|it| it.is_ascii_hexdigit());
    whole.then_some(commit)
}

impl Placed {
    /// The cover of the sealed tree the crate whose manifest is in `dir` is read from, or none:
    /// the crate is not sealed (a path a person edits), and is built and kept in its home.
    /// `dir` is under the tree's directory or where builds mount it ([`Placed::mounted`]).
    pub fn sealed_at<'p>(placed: &'p [Placed], dir: &std::path::Path) -> Option<&'p Placed> {
        placed.iter().find(|it| dir.starts_with(&it.dir) || it.mounts(dir))
    }

    /// The sealed tree whose mount `dir` is under, or none: of a workspace member, the one
    /// way it is sealed ([`pin`]).
    pub fn mounted_at<'p>(placed: &'p [Placed], dir: &std::path::Path) -> Option<&'p Placed> {
        placed.iter().find(|it| it.mounts(dir))
    }

    fn mounts(&self, dir: &std::path::Path) -> bool {
        self.mounted.as_deref().is_some_and(|at| dir.starts_with(at))
    }
}

/// A sealed tree as its driver lists it: where it is placed, the digest it is named by and
/// checked against when it was placed, and the trees of the same list its crates depend on.
/// The toolchain's shipped trees and a lock file's registry packages are each such a list, and
/// one [`placed`] makes the covers of either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// `<machine>/<tree>/<digest>` for a shipped tree; a registry package's unpacked directory,
    /// checked against its lock file's checksum as it was unpacked.
    pub dir: PathBuf,
    /// Where builds name its crates when they read them through a mount of blaze's own, not
    /// at `dir`: the library blaze placed, mounted at `/rustc/<commit>/library`
    /// (`blaze_units::stdlib::library_at`, as `serve::unit`'s `library` mounts it). None for
    /// any other tree, and for a library blaze did not place (one `BLAZE_LIBRARY` names: a
    /// developer's tree, never sealed by its mount).
    pub mounted: Option<PathBuf>,
    /// `library/DIGEST`, a shipped tree's digest, a registry package's `.crate` checksum.
    pub digest: String,
    /// Indexes, in the same list, of the trees its crates depend on: each before it.
    pub on: Vec<usize>,
}

/// Each of `trees` placed with its cover, in order: a tree's cover is on the covers of the
/// trees it depends on, so a list is made dependencies first. A tree that names one not
/// before it is refused, naming both.
pub fn placed(trees: &[Tree], makers: ContentId, host: &str) -> Result<Vec<Placed>, String> {
    let mut out: Vec<Placed> = Vec::with_capacity(trees.len()); // alloc: once a list, a tree each
    let mut on = Vec::new(); // alloc: once a list, reused
    for (at, tree) in trees.iter().enumerate() {
        on.clear();
        for &dep in &tree.on {
            match out.get(dep) {
                Some(it) => on.push(it.cover),
                None => {
                    return Err(format!(
                        "the sealed tree {} (number {at}) depends on number {dep}, which is not listed before it",
                        tree.dir.display()
                    ));
                }
            }
        }
        let cover = cover(&CoverOf { tree: &tree.digest, on: &on, makers, host });
        // clone: once a tree, the list owns its directories and digest
        out.push(Placed { dir: tree.dir.clone(), mounted: tree.mounted.clone(), digest: tree.digest.clone(), cover });
    }
    Ok(out)
}

/// A package a lock file pins at a checksum: a registry package, whose sources are its
/// `.crate`'s, checked against the checksum as they were unpacked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locked {
    pub name: String,
    pub version: String,
    pub checksum: String,
    /// Its dependencies as the lock file names them: `name`, or `name version`, or `name
    /// version (source)`.
    pub dependencies: Vec<String>,
}

/// The registry packages of `locked` as sealed trees, dependencies first, each on the trees
/// `base` (indexes of the list they will follow, such as the library's) and on the packages of
/// `locked` it depends on; `dir_of` says where each is unpacked. A dependency the list does not
/// hold (a path or git package, which has no checksum) is not a sealed tree and adds nothing
/// to the cover: the records' own keys still hold every input. A cycle among them is refused,
/// naming a package of it. `first` is the index the first returned tree will have.
pub fn registry(
    locked: &[Locked],
    base: &[usize],
    first: usize,
    dir_of: impl Fn(&Locked) -> Result<PathBuf, String>,
) -> Result<Vec<Tree>, String> {
    // The package a dependency names, by name and, when the lock file says it, version.
    let named = |dep: &str| {
        let mut words = dep.split_whitespace();
        let (name, version) = (words.next().unwrap_or(""), words.next());
        locked.iter().position(|it| it.name == name && version.is_none_or(|version| it.version == version))
    };
    let deps: Vec<Vec<usize>> =
        locked.iter().map(|it| it.dependencies.iter().filter_map(|dep| named(dep)).collect()).collect(); // alloc: once a lock file
    // Each package's index in the output once placed; a package is placed once all it depends
    // on are. Bounded: each pass places at least one, or the rest is a cycle.
    let mut index: Vec<Option<usize>> = vec![None; locked.len()]; // alloc: once a lock file
    let mut trees = Vec::with_capacity(locked.len()); // alloc: once a lock file, a tree a package
    for _ in 0..locked.len() {
        let before = trees.len();
        for (at, it) in locked.iter().enumerate() {
            if index[at].is_some() || deps[at].iter().any(|dep| index[*dep].is_none()) {
                continue;
            }
            let mut on = base.to_vec(); // alloc: once a package, its few dependencies
            on.extend(deps[at].iter().filter_map(|dep| index[*dep]));
            index[at] = Some(first + trees.len());
            // clone: once a package, the tree owns its digest
            trees.push(Tree { dir: dir_of(it)?, mounted: None, digest: it.checksum.clone(), on });
        }
        if trees.len() == locked.len() {
            break;
        }
        if trees.len() == before {
            let stuck = locked.iter().zip(&index).find(|(_, it)| it.is_none()).map(|(it, _)| it);
            let stuck = stuck.map_or_else(String::new, |it| format!("{} {}", it.name, it.version));
            return Err(format!("the lock file's registry packages depend on each other in a cycle, through {stuck}"));
        }
    }
    Ok(trees)
}

/// The crates of `crates` (name, manifest directory) that are resolved from source: those no
/// placed tree whose layer `installed` says is installed covers.
pub fn from_source<'c>(
    crates: &[(&'c str, &std::path::Path)],
    placed: &[Placed],
    installed: impl Fn(Cover) -> bool,
) -> Vec<&'c str> {
    crates
        .iter()
        .filter(|(_, dir)| !Placed::sealed_at(placed, dir).is_some_and(|it| installed(it.cover)))
        .map(|(name, _)| *name)
        .collect() // alloc: a short list, once a question
}

/// What an [`Install`] asks its driver to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Whether the layer's head is there: one `exists`.
    Stat { head: PathBuf },
    /// Take the cover's lock, a file in the shared directory, held until [`Step::Done`]: one
    /// install of a cover at a time on the machine.
    Lock { at: PathBuf },
    /// Import the pack of `pin` at `base` into the layer: every record checked against its
    /// signed manifest before it is put, then the layer committed.
    Import { base: Base, pin: String },
    Done(Installed),
}

/// What the driver found for the last [`Step`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Head(bool),
    Locked,
    /// Records put and their bytes, or why the base refused (it could not be read, a record
    /// did not hash to its manifest, the head was not signed by a trusted key).
    Imported(Result<(u64, u64), String>),
    /// The base has no pack of the pin.
    Absent,
}

/// How an install ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The layer was there.
    Already,
    Now { records: u64, bytes: u64 },
    /// No base had the cover's pack (each named), or prebuilt records are off (none looked
    /// at): the tree's crates are built in the home that asks.
    NoPack { looked: Vec<String> },
    /// A base that is set refused: the command's error, naming it (rule 3).
    Refused { base: String, why: String },
}

/// One install of a cover's layer, as a state machine: no IO of its own.
#[derive(Debug)]
pub struct Install {
    cover: Cover,
    std: PathBuf,
    bases: Vec<Base>,
    prebuilt: bool,
    at: At,
    looked: Vec<String>,
    /// A planted bug for the oracle's control: the stat under the lock is skipped.
    #[cfg(test)]
    pub(crate) plant_skip_stat: bool,
    /// A planted bug for the control of the prebuilt test: the setting is ignored.
    #[cfg(test)]
    pub(crate) plant_ignore_prebuilt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    Start,
    /// The first stat, before the lock.
    Looked,
    Locking,
    /// The stat under the lock.
    Checked,
    /// Importing from base `n`.
    Importing(usize),
    Done,
}

impl Install {
    /// The install of `cover`'s layer in the shared directory `std` (`<machine>/std/<makers>`),
    /// from `bases` in order (`packs.json`'s `install` and sources, then the engine's own
    /// `share/blaze/sealed`), asked only when `prebuilt`.
    pub fn new(cover: Cover, std: PathBuf, bases: Vec<Base>, prebuilt: bool) -> Install {
        Install {
            cover,
            std,
            bases,
            prebuilt,
            at: At::Start,
            looked: Vec::new(), // alloc: empty, allocates nothing
            #[cfg(test)]
            plant_skip_stat: false,
            #[cfg(test)]
            plant_ignore_prebuilt: false,
        }
    }

    fn head(&self) -> PathBuf {
        writer_dir(&self.std, self.cover).join("head")
    }

    fn base(&mut self, from: usize) -> Step {
        #[cfg(test)]
        let prebuilt = self.prebuilt || self.plant_ignore_prebuilt;
        #[cfg(not(test))]
        let prebuilt = self.prebuilt;
        match self.bases.get(from).filter(|_| prebuilt) {
            Some(base) => {
                self.at = At::Importing(from);
                // clone: once a base asked, a path or an address
                Step::Import { base: base.clone(), pin: pack_pin(self.cover) }
            }
            None => {
                let looked = std::mem::take(&mut self.looked);
                self.done(Installed::NoPack { looked })
            }
        }
    }

    fn done(&mut self, installed: Installed) -> Step {
        self.at = At::Done;
        Step::Done(installed)
    }

    /// The next step, given what the last one found (`None` for the first). An outcome that is
    /// not the answer to the step asked is a bug of the driver, and panics naming both.
    pub fn step(&mut self, done: Option<Outcome>) -> Step {
        match (self.at, done) {
            (At::Start, None) => {
                self.at = At::Looked;
                Step::Stat { head: self.head() }
            }
            (At::Looked | At::Checked, Some(Outcome::Head(true))) => self.done(Installed::Already),
            (At::Looked, Some(Outcome::Head(false))) => {
                self.at = At::Locking;
                Step::Lock { at: self.std.join("store").join(format!(".sealed-{:x}.lock", self.cover.0)) }
            }
            (At::Locking, Some(Outcome::Locked)) => {
                #[cfg(test)]
                if self.plant_skip_stat {
                    return self.base(0);
                }
                self.at = At::Checked;
                Step::Stat { head: self.head() }
            }
            (At::Checked, Some(Outcome::Head(false))) => self.base(0),
            (At::Importing(_), Some(Outcome::Imported(Ok((records, bytes))))) => {
                self.done(Installed::Now { records, bytes })
            }
            (At::Importing(from), Some(Outcome::Imported(Err(why)))) => {
                let base = self.bases[from].named();
                self.done(Installed::Refused { base, why })
            }
            (At::Importing(from), Some(Outcome::Absent)) => {
                let named = self.bases[from].named();
                self.looked.push(named);
                self.base(from + 1)
            }
            (at, done) => panic!("the sealed install of {} at {at:?} was answered {done:?}", pack_pin(self.cover)),
        }
    }
}

#[cfg(test)]
mod tests;
