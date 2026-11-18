//! The command's store on blaze-cas's plain store (D114 piece (b); drive.md, "D114 rulings",
//! "The store under the command"): ONE store a user, one root (`<home>/.blaze/v2/store`), read
//! and written by every checkout and worktree; records read IN PLACE.
//!
//! Every byte a get lends is owned by blaze-cas's [`Lent`] (the plain store's segment mappings,
//! never unmapped while it is open, and this command's puts in an arena of chunks that never
//! move): its one `unsafe` lives there (plain/lent.rs), so this crate keeps
//! `#![forbid(unsafe_code)]`. [`PlainPlace`] is the handle over `&'m Lent`: a get returns
//! [`Held<'m>`], whose bytes borrow the `Lent`, never the handle, so a later get (which may map
//! another segment), a put (which may add a chunk) or a commit leaves them as they are.
//!
//! WRITES. A put is copied once into the arena; nothing touches the disk on the command's path,
//! unless the arena passes [`COMMIT_BYTES`] (then one commit, between tasks). After the answer is
//! printed, [`after_answer`] commits what is left as one segment a namespace and one
//! flip, seals the exit tail and runs a bounded compaction. A crash before the commit loses this
//! command's records (made again by the next), never part of one: the flip is the commit.
//!
//! Generic over the driver (rule 78): the simulation in the tests, bstd's lent driver in the
//! command (one driver a process: drive.md ruling 5).

use core::fmt;
use core::marker::PhantomData;
use std::alloc::Global;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Driven, Lent, LentCounts, LentError, PlainConfig, Probes, Settings};
use blaze_core::{Driver, FileName};
use blaze_drive::Key;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_keyed::store::StoreError;

/// The store's folder under blaze's folder in the home (drive.md ruling 1).
pub(crate) const STORE_DIR: &str = "v2/store";

/// The kind every drive record is filed under in the plain store's index. The drive record's
/// own head says which drive kind it is; the store needs one number apart from its roots'
/// ([`blaze_cas::plain::ROOT_TAG`], `u16::MAX`) and the startup image's (`u16::MAX - 1`).
pub(crate) const DRIVE_TAG: Tag = Tag(u16::MAX - 2);

/// The store's effect tags on the lent driver (`PlainConfig::tags`).
const STORE_TAGS: u64 = 1 << 50;

/// Past this many uncommitted bytes in the arena, a put commits first.
pub(crate) const COMMIT_BYTES: u64 = 256 << 20;

/// Past this many segments named, compaction is behind: the open says so, naming the root
/// (main's Q3 answer; blaze-cas compacts to [`blaze_cas::plain::SEGMENTS_MOST`]).
pub(crate) const SEGMENTS_NAMED_MOST: u32 = 64;

/// Why the store refused, by name (rule 19).
#[derive(Debug)]
pub enum PlaceError {
    /// The plain store would not open: its own typed error.
    Open { why: StoreError },
    /// The store's owner refused (two values, a namespace not writable, the arena, the store).
    Lent(LentError),
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaceError::Open { why } => write!(f, "the store under the home's .blaze/{STORE_DIR} would not open: {why}"),
            PlaceError::Lent(error) => write!(f, "the store under the home's .blaze/{STORE_DIR}: {error}"),
        }
    }
}

impl From<LentError> for PlaceError {
    fn from(error: LentError) -> PlaceError {
        PlaceError::Lent(error)
    }
}

/// The facts the store is opened with, passed in from the command's start (rule 75).
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// The machine's boot (the driver's fact, hashed to 64 bits), or `None`: then every
    /// segment is taken as of another boot and checked once (blaze-cas `PlainConfig::boot`).
    pub boot: Option<u64>,
    /// Drawn at random by the caller: two processes never name a segment alike.
    pub nonce: u64,
}

/// The plain store's configuration for the folder `dir` (as the driver names it).
pub(crate) fn config(dir: FileName, settings: &Settings, facts: Facts) -> PlainConfig {
    PlainConfig {
        dir,
        tags: STORE_TAGS,
        cap_bytes: settings.cap_bytes(),
        // No boot fact: the nonce, a boot no segment was written in.
        boot: facts.boot.unwrap_or(facts.nonce),
        nonce: facts.nonce,
        durable: settings.durable,
        // The caller adds the checkout's and the home's spellings ([`PlainPlace::machine`]).
        machine: Vec::new_in(Global),
        writes: settings.writes(),
        reads: settings.reads(),
    }
}

/// The store opened lazily over `driver` (nothing mapped: a segment is mapped at its first
/// probe), owned by a [`Lent`] whose arena is on the global heap, named (rule 11).
pub fn open<D: Driver>(driver: D, cfg: PlainConfig) -> Result<Lent<D, Global>, PlaceError> {
    // The root path is read only by the native pread path, which this module never takes.
    let store = Driven::with(driver, std::path::Path::new("/"), cfg).map_err(|why| PlaceError::Open { why })?;
    Ok(Lent::new(store, Global))
}

/// The proof that [`Held::bytes`] are the bytes filed under the key asked (rule 74). Only this
/// module makes one. It is earned in one of three ways:
/// 1. the bytes lie in a segment proved once this boot: by its manifest row (written or checked
///    this boot), by its seal, or by the full check of its length, layout and digest at its
///    first hit after a restart (blaze-cas plain probe.rs);
/// 2. the bytes are this command's own put, copied into the `Lent`'s arena by this tool;
/// 3. the bytes were fetched, checked against their signed statement and digest at the fetch,
///    and adopted by a flip (from then on, case 1).
///
/// It proves the bytes, not the form of the record they hold: a reader's form check is its own.
#[derive(Clone, Copy, Debug)]
pub struct Checked<'m> {
    _private: PhantomData<&'m ()>,
}

/// A record lent in place for `'m`, the life of the [`Lent`] borrowed.
#[derive(Clone, Copy, Debug)]
pub struct Held<'m> {
    pub bytes: &'m [u8],
    pub proof: Checked<'m>,
    /// The same bytes as a segment's mapping, aligned, for a reader that reads them in place
    /// (engine_records.rs); or why they are not: this command's own put, the arena's copy
    /// (blaze-cas lent.rs `Lend::in_place`).
    pub place: Result<blaze_cas::plain::Aligned16<'m>, blaze_cas::plain::NotInPlace>,
}

/// A planted bug of the tests (`plain_place_tests.rs`): each one the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PlacePlant {
    None,
    /// A pin does nothing: compaction past the cap lets a running program's records go.
    ForgetsPin,
}

/// What the store did this command, for `--stats` and the profile.
#[derive(Clone, Copy, Default, Debug)]
pub struct Counts {
    pub lent: LentCounts,
    pub probes: Probes,
    /// Segments mapped of those the head names.
    pub mapped: u32,
    pub named: u32,
    /// Commits made on the command's path (the arena past [`COMMIT_BYTES`]).
    pub commits: u32,
    pub segments_written: u32,
}

/// The store handle the command's `Store` seam is implemented on: the namespace a record goes
/// to is decided here (drive.md ruling 2), every byte lives in the `Lent`.
pub struct PlainPlace<'m, D: Driver> {
    lent: &'m Lent<D, Global>,
    /// Whether this machine may write [`Namespace::PUBLIC`] (a trusted builder).
    public_writer: bool,
    plant: PlacePlant,
    commits: u32,
    segments_written: u32,
}

impl<'m, D: Driver> PlainPlace<'m, D> {
    /// `public_writer`: whether the settings name this machine a trusted writer.
    pub fn new(lent: &'m Lent<D, Global>, public_writer: bool) -> PlainPlace<'m, D> {
        PlainPlace::planted(lent, public_writer, PlacePlant::None)
    }

    pub(crate) fn planted(lent: &'m Lent<D, Global>, public_writer: bool, plant: PlacePlant) -> PlainPlace<'m, D> {
        PlainPlace { lent, public_writer, plant, commits: 0, segments_written: 0 }
    }

    /// The record's namespace (drive.md ruling 2): `PUBLIC` only when every name in it is
    /// public AND this machine is a trusted builder; else `LOCAL`.
    pub fn namespace(&self, all_names_public: bool) -> Namespace {
        if all_names_public && self.public_writer { Namespace::PUBLIC } else { Namespace::LOCAL }
    }

    /// Adds what names this machine (the checkout's and the home's paths, in each spelling the
    /// caller knows) to the scan of every put into a shared namespace (rule 25).
    pub fn machine(&self, paths: &[&std::path::Path]) {
        self.lent.machine(paths);
    }

    /// The record under `key`, in place, for `'m`: this command's own put, else the committed
    /// record of the first namespace read that holds it. `None`: absent.
    pub fn get(&self, key: Key) -> Result<Option<Held<'m>>, PlaceError> {
        let lent: &'m Lent<D, Global> = self.lent;
        Ok(lent.get(&CasKey(key.0))?.map(|lend| Held { bytes: lend.bytes, proof: Checked { _private: PhantomData }, place: lend.in_place() }))
    }

    /// `bytes` under `key`, copied once into the arena. A key held with equal bytes is left as
    /// it is (rule 25); with other bytes it is refused (rule 21). Past [`COMMIT_BYTES`]
    /// uncommitted, one commit first.
    pub fn put(&mut self, key: Key, all_names_public: bool, bytes: &[u8]) -> Result<(), PlaceError> {
        self.put_parts(key, all_names_public, &[bytes])
    }

    /// [`PlainPlace::put`] of the concatenation of `parts`, never joined (blaze-cas
    /// `Lent::put_parts`).
    pub fn put_parts(&mut self, key: Key, all_names_public: bool, parts: &[&[u8]]) -> Result<(), PlaceError> {
        if self.lent.uncommitted_bytes() > COMMIT_BYTES {
            self.commit()?;
        }
        self.lent.put_parts(&CasKey(key.0), self.namespace(all_names_public), DRIVE_TAG, parts)?;
        Ok(())
    }

    /// The record the root `name` is, in place (its bytes the 32-byte keys it names, in order):
    /// a root is a name in the newest manifest, not a probe (drive.md ruling 7). `None`: no
    /// root of that name.
    pub fn root(&self, name: &str) -> Result<Option<Held<'m>>, PlaceError> {
        let Some((_, at)) = self.lent.roots().into_iter().find(|(it, _)| it == name) else { return Ok(None) };
        self.get(Key(at.0))
    }

    /// Keeps `keys` under the root `name` from the next commit on: compaction keeps every
    /// record a root names (a running program's code records; a checkout's last answer for each
    /// request shape: drive.md ruling 3).
    pub fn pin(&self, name: &str, keys: &[Key]) -> Result<(), PlaceError> {
        if self.plant == PlacePlant::ForgetsPin {
            return Ok(());
        }
        let mut cas = Vec::with_capacity_in(keys.len(), Global);
        cas.extend(keys.iter().map(|key| CasKey(key.0)));
        self.lent.root(name, &cas)?;
        Ok(())
    }

    /// Every segment named mapped, and every one parked checked: asked once, when the
    /// request's first pool task is about to start (blaze-cas `Driven::map_all`).
    pub fn map_all(&self) -> Result<(), PlaceError> {
        Ok(self.lent.map_all()?)
    }

    /// What is not yet committed, laid out as one segment per namespace, and one flip.
    pub fn commit(&mut self) -> Result<(), PlaceError> {
        let committed = self.lent.commit()?;
        self.commits += 1;
        self.segments_written += u32::try_from(committed.segments.len()).unwrap_or(u32::MAX);
        Ok(())
    }

    /// This command's counts so far (before the commit after the answer: [`after_answer`]).
    pub fn counts(&self) -> Counts {
        let (mapped, named) = self.lent.mapped_of_named();
        Counts { lent: self.lent.counts(), probes: self.lent.probes(), mapped, named, commits: self.commits, segments_written: self.segments_written }
    }
}

/// What the store did after the answer was printed ([`after_answer`]).
#[derive(Clone, Copy, Default, Debug)]
pub struct AfterAnswer {
    pub segments_written: u32,
    /// Whether the exit tail's seal flipped.
    pub sealed: bool,
    pub compaction_slices: u32,
}

/// After the answer is printed, never waited on by it (the command's outer function calls it
/// once every handle over `lent` is gone): the commit of what is left as one segment a
/// namespace and one flip, the exit tail's seal, then at most `slices` compaction slices (each
/// bounded by blaze-cas `SLICE_BYTES`).
pub fn after_answer<D: Driver>(lent: &Lent<D, Global>, slices: u32) -> Result<AfterAnswer, PlaceError> {
    let committed = lent.commit()?;
    let sealed = lent.seal()?;
    let compaction_slices = match slices {
        0 => 0,
        _ => u32::try_from(lent.compact(slices)?.len()).unwrap_or(u32::MAX),
    };
    Ok(AfterAnswer { segments_written: u32::try_from(committed.segments.len()).unwrap_or(u32::MAX), sealed, compaction_slices })
}

#[cfg(test)]
#[path = "plain_place_tests.rs"]
mod tests;
