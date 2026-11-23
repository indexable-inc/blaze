//! The command's store on blaze-cas's plain store (D114 piece (b); drive.md, "D114 rulings",
//! "The store under the command"): ONE store a user of each form, at `<home>/.blaze/`
//! [`STORE_DIR`] (`v2/store/f5s3`, D160), read and written by every checkout and worktree;
//! records read IN PLACE.
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
use std::alloc::Allocator;

use blaze_cas::Key as CasKey;
use blaze_own::Alloc;
use blaze_cas::plain::{Driven, Lent, LentCounts, LentError, Liveness, PlaceName, PlainConfig, Probes, ROOT_LIFE_NS, Settings, Snapshot, SnapshotError, Visibility};
use blaze_core::{Driver, FileName};
use blaze_drive::Key;
use blaze_keyed::kind::{Namespace, ReservedTag, Tag};
use blaze_keyed::store::StoreError;
use bstd::cx::Cx;

use super::roots::{Answer, Rooted};

/// The allocator the command's store lives in: the process's heap, as the handle `main` was
/// handed (`heap.alloc()` of its `&'static blaze_own::Heap`, D120), passed down to [`open`].
/// The `Lent`'s arena and every scratch list about the store are made in it.
pub type StoreAlloc = Alloc<'static>;

/// Stores of other forms beside this blaze's: the note said once, and what cleanup removes.
#[path = "forms.rs"]
pub(crate) mod forms;

/// The root of every store's folder under blaze's folder in the home (drive.md ruling 1): it
/// holds blaze's cache directory tag, the store of each form a blaze on this machine made
/// ([`STORE_DIR`]), and, from format 4 and before, that store's files directly (the legacy
/// layout, [`forms`]).
pub(crate) const STORE_ROOT: &str = "v2/store";

/// THE PLACE OF THIS BLAZE'S STORE (D160): `v2/store/<forms>`, the forms every blaze must agree
/// on to read it, named by blaze-cas alone ([`PlaceName::THIS`], `f5s3` today: the store's
/// files' format and its segments'). Nothing else names the store's folder: every reader of it
/// goes through this constant, and a bump of either form moves it, so a blaze of a new form
/// makes its own folder beside the old, and none stops because another wrote a store.
//
// Stable const code, no unsafe: `STORE_DIR_JOINED.bytes` is a constant of a type with no
// `Drop` and no interior mutability, so a borrow of it is promoted to `'static` (RFC 1414);
// `split_at` and `str::from_utf8` are `const fn`, and a name that is not UTF-8 fails the build.
pub(crate) const STORE_DIR: &str = match core::str::from_utf8(STORE_DIR_JOINED.bytes.split_at(STORE_DIR_JOINED.len).0) {
    Ok(it) => it,
    Err(_) => panic!("the store's place is UTF-8"),
};

const STORE_DIR_JOINED: Joined = Joined::of(STORE_ROOT, PlaceName::THIS.as_str());

/// Two names joined by a `/`, made at compile time (no allocator: rule 11).
struct Joined {
    bytes: [u8; 64],
    len: usize,
}

impl Joined {
    const fn of(root: &str, leaf: &str) -> Joined {
        let (root, leaf) = (root.as_bytes(), leaf.as_bytes());
        assert!(root.len() + 1 + leaf.len() <= 64, "a store's place is short");
        let mut bytes = [0u8; 64];
        let mut i = 0;
        while i < root.len() {
            bytes[i] = root[i];
            i += 1;
        }
        bytes[i] = b'/';
        let mut j = 0;
        while j < leaf.len() {
            bytes[i + 1 + j] = leaf[j];
            j += 1;
        }
        Joined { bytes, len: root.len() + 1 + leaf.len() }
    }
}

// Every drive record is filed under [`ReservedTag::Drive`] in the plain store's index (the
// drive record's own head says which drive kind it is), and an expansion record (blaze-scope
// expansion_records.rs: a macro call's text and origin column, keyed by their bytes, put after
// its crate's `CrateScope` record) under [`ReservedTag::Expansion`]: a family apart from the
// drive records', read only when a body reads a text a macro wrote. Both are blaze-keyed's
// reservations, the one registry of reserved tags.

/// The store's effect tags on the lent driver (`PlainConfig::tags`).
const STORE_TAGS: u64 = 1 << 50;

/// Past this many uncommitted bytes in the arena, a put commits first.
/// 64 MiB bounds the transient `Arena + lay_out` commit spike to ~128 MiB on large cold builds
/// while incremental edits (a few KB to a few MB) still commit once after the answer.
pub(crate) const COMMIT_BYTES: u64 = 64 << 20;

/// THE PUT PATH'S SEAM (blaze-cas plain/arena.rs, rule 78): where a reader finds a record this
/// command put and has not committed. `AfterCommit` (today's): each phase commits its puts
/// (`CommandStore::commit_puts`) before the next phase's workers read them from a segment.
/// `InArena`: the one copy a put makes is read in place by every reader and snapshot; a phase
/// boundary is a checkpoint ([`PlainPlace::checkpoint`]: a commit only past [`COMMIT_BYTES`] or
/// [`CHECKPOINT_AGE_NS`]), and the commit after the answer writes the rest.
pub(crate) const VISIBILITY: Visibility = Visibility::InArena { shared: blaze_mem::Kept(blaze_mem::Tag::Store) };

/// Under `Visibility::InArena`, a phase boundary commits what is put when the oldest of it has
/// waited this long (first value, accepted 2026-11-21): a command killed after a long phase
/// keeps at most this much of its work uncommitted, while a command of a few seconds (an edit's
/// check) writes ONE segment a namespace, after its answer. Only where a segment boundary falls
/// depends on it, never a record's bytes or key (blaze-cas plain/arena_tests.rs
/// `checkpoints_at_any_points_give_the_same_records_and_answer_pointer`).
pub(crate) const CHECKPOINT_AGE_NS: u64 = 30_000_000_000;

/// The checkpoint rule, alone (`checkpoint_tests` below): `uncommitted` bytes put
/// and not committed, the oldest seen `waited_ns` ago at a phase boundary.
pub(crate) fn checkpoint_due(uncommitted: u64, waited_ns: u64) -> bool {
    uncommitted > COMMIT_BYTES || (uncommitted > 0 && waited_ns >= CHECKPOINT_AGE_NS)
}

/// Why the store refused, by name (rule 19).
#[derive(Debug)]
pub enum PlaceError {
    /// The plain store would not open: its own typed error.
    Open { why: StoreError },
    /// The store's owner refused (two values, a namespace not writable, the arena, the store).
    Lent(LentError),
    /// A snapshot of the committed segments could not be taken (blaze-cas plain/snapshot.rs).
    Snapshot(SnapshotError),
    /// The calling thread could not be held at the after-answer class ([`after_answer`]).
    #[cfg(target_os = "macos")]
    Qos(blaze_core::threads::qos::QosError),
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaceError::Open { why } => write!(f, "the store under the home's .blaze/{STORE_DIR} would not open: {why}"),
            PlaceError::Lent(error) => write!(f, "the store under the home's .blaze/{STORE_DIR}: {error}"),
            PlaceError::Snapshot(error) => write!(f, "the store under the home's .blaze/{STORE_DIR}: {error}"),
            #[cfg(target_os = "macos")]
            PlaceError::Qos(error) => write!(f, "the store's work after the answer under the home's .blaze/{STORE_DIR}: {error}"),
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

/// The plain store's configuration for the folder `dir` (as the driver names it), its lists in
/// `alloc`, the store's.
pub(crate) fn config(dir: FileName, settings: &Settings, facts: Facts, alloc: StoreAlloc) -> PlainConfig<StoreAlloc> {
    PlainConfig {
        dir,
        tags: STORE_TAGS,
        cap_bytes: settings.cap_bytes(),
        // No boot fact: the nonce, a boot no segment was written in.
        boot: facts.boot.unwrap_or(facts.nonce),
        nonce: facts.nonce,
        durable: settings.durable,
        // The caller adds the checkout's and the home's spellings ([`PlainPlace::machine`]).
        machine: Vec::new_in(alloc),
        writes: settings.writes(alloc),
        reads: settings.reads(alloc),
        // Reachability GC (`Liveness::Reached`, runtime.md C5): keeps what unexpired roots reach
        // (store/roots.rs: program root, toolchain root, answer `.pages` root and expansion
        // indexes, dependency-heads root, rows-form roots) and what any running command's lease
        // keeps, sweeping superseded/expired records in post-answer compaction slices. The old
        // drive path is deleted.
        liveness: Liveness::Reached,
        // The jittered doubling, its jitter the nonce (drawn from the granted `Rng`: the
        // simulation's draw under it); `Fixed` is a test's control only.
        lock_step: blaze_cas::plain::LockStep::Backoff,
    }
}

/// The store opened lazily over `driver` (nothing mapped: a segment is mapped at its first
/// probe), owned by a [`Lent`] whose arena is in `alloc`, the caller's (rule 75).
pub fn open<D: Driver>(driver: D, cfg: PlainConfig<StoreAlloc>, alloc: StoreAlloc) -> Result<Lent<D, StoreAlloc>, PlaceError> {
    // The root path is read only by the native pread path, which this module never takes.
    let store = Driven::with(driver, std::path::Path::new("/"), cfg, alloc).map_err(|why| PlaceError::Open { why })?;
    Ok(Lent::with_visibility(store, alloc, VISIBILITY))
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

impl<'s> Checked<'s> {
    /// A snapshot read's bytes with their proof as this crate's [`Checked`] (way 1 above): the
    /// store's [`blaze_cas::plain::Proof`] is made only from a segment proved this boot, so the
    /// `Proved` it rides in is the evidence, and the `Checked` lives exactly as long as the bytes
    /// it proves (`'s`, the snapshot's borrow). The way a pool worker's read
    /// (`Snapshot::get_proved`) reaches the engine's `StoreProof`.
    pub fn of_snapshot(proved: blaze_cas::plain::Proved<'s>) -> (&'s [u8], Checked<'s>) {
        (proved.bytes, Checked { _private: PhantomData })
    }
}

/// The record under `key` in `snapshot`, in place for the snapshot's borrow `'s`, with the
/// store's proof beside its bytes ([`Checked::of_snapshot`]): a pool worker's [`PlainPlace::get`]
/// (a `Snapshot` is `Send + Sync`, the `Lent` is not), so a reader hands the proof on (the
/// engine's `Sealed::by_store`) instead of hashing the bytes again (rule 74). A snapshot lists
/// committed segments alone, so `place` is never the arena's copy: only a misaligned payload
/// (which the store's layout never writes) is not in place. `None`: absent.
pub(crate) fn snapshot_get<'s, A: Allocator>(snapshot: &'s Snapshot<A>, key: Key) -> Result<Option<Held<'s>>, SnapshotError> {
    Ok(snapshot.get_proved(&CasKey(key.0))?.map(|proved| {
        let (bytes, proof) = Checked::of_snapshot(proved);
        Held { bytes, proof, place: blaze_cas::plain::Aligned16::new(bytes).map_err(blaze_cas::plain::NotInPlace::Misaligned) }
    }))
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
    lent: &'m Lent<D, StoreAlloc>,
    /// Whether this machine may write [`Namespace::PUBLIC`] (a trusted builder).
    public_writer: bool,
    plant: PlacePlant,
    commits: u32,
    segments_written: u32,
    /// `Visibility::InArena`: the wall time (the store's driver's clock) of the first phase
    /// boundary that saw uncommitted puts, from which [`CHECKPOINT_AGE_NS`] runs; cleared by
    /// each commit.
    aging_since: Option<u64>,
}

impl<'m, D: Driver> PlainPlace<'m, D> {
    /// A phase boundary under `Visibility::InArena`: a commit when what is put and not committed
    /// is past [`COMMIT_BYTES`], or has waited [`CHECKPOINT_AGE_NS`] since the first boundary
    /// that saw it; else nothing (the phase's records are read from the shared arena). Whether
    /// it committed. Under `AfterCommit`, every boundary commits (today's).
    pub fn checkpoint(&mut self) -> Result<bool, PlaceError> {
        let uncommitted = self.lent.uncommitted_bytes();
        if self.lent.visibility() == Visibility::AfterCommit {
            self.commit()?;
            return Ok(true);
        }
        if uncommitted == 0 {
            return Ok(false);
        }
        let now = self.lent.wall_ns();
        let since = *self.aging_since.get_or_insert(now);
        if checkpoint_due(uncommitted, now.saturating_sub(since)) {
            self.commit()?;
            return Ok(true);
        }
        Ok(false)
    }
    /// `public_writer`: whether the settings name this machine a trusted writer.
    pub fn new(lent: &'m Lent<D, StoreAlloc>, public_writer: bool) -> PlainPlace<'m, D> {
        PlainPlace::planted(lent, public_writer, PlacePlant::None)
    }

    pub(crate) fn planted(lent: &'m Lent<D, StoreAlloc>, public_writer: bool, plant: PlacePlant) -> PlainPlace<'m, D> {
        PlainPlace { lent, public_writer, plant, commits: 0, segments_written: 0, aging_since: None }
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
        let lent: &'m Lent<D, StoreAlloc> = self.lent;
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
        self.lent.put_parts(&CasKey(key.0), self.namespace(all_names_public), ReservedTag::Drive.tag(), parts)?;
        Ok(())
    }

    /// A record keyed by its own bytes (an expansion record: `tag` [`ReservedTag::Expansion`]) that many
    /// owners of either namespace may name: equal bytes already held under `key`, this
    /// command's or committed, in any namespace and under any tag (a record a remote fetch
    /// filed under [`ReservedTag::Drive`]), is [`Put::Same`], so a toolchain crate's public record and a
    /// workspace crate's local one share it; else put in the namespace `all_names_public` (its
    /// owner's) says. Other bytes under `key` are refused (rule 21).
    pub fn put_record(&mut self, key: Key, all_names_public: bool, tag: Tag, bytes: &[u8]) -> Result<blaze_keyed::store::Put, PlaceError> {
        // Asked by comparison, never by a get: a get of this command's own put keeps its arena
        // chunk, and every record put beside it, out of place after the commit (blaze-cas
        // lent.rs `holds`). An expansion record two units of one wave both put did that to
        // the names records beside it (clap, tempo, 2026-11-17).
        if self.lent.holds(&CasKey(key.0), bytes)? {
            return Ok(blaze_keyed::store::Put::Same);
        }
        if self.lent.uncommitted_bytes() > COMMIT_BYTES {
            self.commit()?;
        }
        Ok(self.lent.put_parts(&CasKey(key.0), self.namespace(all_names_public), tag, &[bytes])?)
    }

    /// The record the root `name` is, in place (its bytes the 32-byte keys it names, in order):
    /// a root is a name in the newest manifest, not a probe (drive.md ruling 7). `None`: no
    /// root of that name. Uses the root's creation generation as a segment-locality hint so warm
    /// reads avoid mapping older/larger segments.
    pub fn root(&self, name: &str) -> Result<Option<Held<'m>>, PlaceError> {
        let lent: &'m Lent<D, StoreAlloc> = self.lent;
        Ok(lent.get_root(name)?.map(|lend| Held { bytes: lend.bytes, proof: Checked { _private: PhantomData }, place: lend.in_place() }))
    }

    /// Keeps `keys` under the root `name` from the next commit on, for blaze-cas
    /// `ROOT_LIFE_NS` from it (each commit that names them again renews it): cleanup keeps
    /// every record an unexpired root names (a running program's code records; a checkout's
    /// last answer for each request shape: drive.md ruling 3), and nothing else past a
    /// command's lease. The one function of the [`Answer`] class (store/roots.rs): its
    /// [`Rooted`] says the root expires with the checkout.
    pub(crate) fn pin(&self, name: &str, keys: &[Key]) -> Result<Rooted<Answer>, PlaceError> {
        if self.plant == PlacePlant::ForgetsPin {
            return Ok(Rooted::new(CasKey(blaze_ids::ContentId([0; 32]))));
        }
        let mut cas = Vec::with_capacity_in(keys.len(), *self.lent.alloc());
        cas.extend(keys.iter().map(|key| CasKey(key.0)));
        Ok(Rooted::new(self.lent.root_for(name, &cas, ROOT_LIFE_NS)?))
    }

    /// [`PlainPlace::pin`] through index records: the root `name` names one index of `keys`
    /// and the index filed beside each of `owners` that this store holds ([`PlainPlace::beside`]),
    /// so reach keeps `keys` and every record an owner was put with (a names record's expansion
    /// records: engine_store.rs `put_all`), with no read of the owners. An owner with no index
    /// beside it was put with none.
    pub(crate) fn pin_through(&self, name: &str, keys: &[Key], owners: &[Key]) -> Result<Rooted<Answer>, PlaceError> {
        if self.plant == PlacePlant::ForgetsPin {
            return Ok(Rooted::new(CasKey(blaze_ids::ContentId([0; 32]))));
        }
        let mut cas = Vec::with_capacity_in(keys.len(), *self.lent.alloc());
        cas.extend(keys.iter().map(|key| CasKey(key.0)));
        let mut indexes = Vec::with_capacity_in(owners.len() + 1, *self.lent.alloc());
        indexes.push(self.lent.index(&cas)?);
        for owner in owners {
            if let Some(beside) = self.beside(*owner)? {
                indexes.push(beside);
            }
        }
        Ok(Rooted::new(self.lent.root_through(name, &indexes, ROOT_LIFE_NS)?))
    }

    /// The unexpired root `name` renewed in O(1) for `life_ns` from the next commit without
    /// reading, probing, sorting, or hashing the keys it holds (`Lent::renew_root`): a command
    /// that answered from what the root reaches without writing it again (`engine-check` whose
    /// certificate stood) still renews it, so a checkout built every day never sees its answer
    /// expire. Answers whether there was such a live root (an expired one is never named again: a
    /// slice may have swept what it named). Within `TOUCH_NS` of its full life nothing is queued
    /// and no flip is owed.
    pub(crate) fn renew(&self, name: &str, life_ns: u64) -> Result<bool, PlaceError> {
        Ok(self.lent.renew_root(name, life_ns).is_live())
    }

    /// O(segments + roots + retired) check of whether compaction or GC housekeeping is due at
    /// `now_ns` (`Lent::needs_housekeeping`).
    pub fn needs_housekeeping(&self, now_ns: u64) -> bool {
        self.lent.needs_housekeeping(now_ns)
    }

    /// The index of the records `owner` was put with, filed beside it (blaze-cas
    /// `Plain::index_beside`): put with its owner, before any root names it.
    pub(crate) fn index_beside(&self, owner: Key, keys: &[Key]) -> Result<CasKey, PlaceError> {
        let mut cas = Vec::with_capacity_in(keys.len(), *self.lent.alloc());
        cas.extend(keys.iter().map(|key| CasKey(key.0)));
        Ok(self.lent.index_beside(CasKey(owner.0), &cas)?)
    }

    /// The index filed beside `owner` when this store holds one (an index record there, by
    /// its tag): `None` when `owner` was put with no records beside it.
    pub(crate) fn beside(&self, owner: Key) -> Result<Option<CasKey>, PlaceError> {
        let key = blaze_cas::plain::beside_key(CasKey(owner.0));
        Ok(self.lent.get(&key)?.filter(|held| ReservedTag::Index.is(held.tag)).map(|_| key))
    }

    /// The store's owner, for a class root of store/roots.rs written beside the engine's
    /// (the toolchain's, [`super::roots::keep_toolchain`]).
    pub(crate) fn lent(&self) -> &'m Lent<D, StoreAlloc> {
        self.lent
    }

    /// Every segment named mapped, and every one parked checked: asked once, when the
    /// request's first pool task is about to start (blaze-cas `Driven::map_all`).
    pub fn map_all(&self) -> Result<(), PlaceError> {
        Ok(self.lent.map_all()?)
    }

    /// What is not yet committed, laid out as one segment per namespace, and one flip.
    pub fn commit(&mut self) -> Result<(), PlaceError> {
        let committed = self.lent.commit()?;
        self.aging_since = None;
        self.commits += 1;
        self.segments_written += u32::try_from(committed.segments.len()).unwrap_or(u32::MAX);
        Ok(())
    }

    /// A `Send + Sync` read-only snapshot of what is committed now, its list in `alloc`
    /// (blaze-cas `Lent::snapshot_in`: every segment mapped first). The arena's puts not yet
    /// committed are not in it.
    pub fn snapshot_in<B: Allocator>(&self, alloc: B) -> Result<Snapshot<B>, PlaceError> {
        self.lent.snapshot_in(alloc).map_err(PlaceError::Snapshot)
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
    /// Cleanup slices this command's flips named (at most blaze-cas `CLEANUP_SLICES_MOST`).
    pub compaction_slices: u32,
    /// The cleanup line: roots set, keys named, what the slice swept, carried, leases.
    pub cleanup: super::roots::CleanupLine,
    /// The calling thread's QoS class around it (the measure of [`AFTER_ANSWER_QOS`]).
    pub qos: QosSeen,
}

/// The seam (rule 78) of the class the work after the answer runs at (thoughts.md 2.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AfterAnswerQos {
    /// The calling thread as it is: blaze-cas lowers it to `utility` inside `fold_slice` and
    /// `seal` (`lower_this_thread`) and never gives it back, so a commit with no slice folded
    /// before it runs at the caller's class, and the caller's thread stays lowered after. The
    /// control.
    AsCalled,
    /// The whole stretch held at most at `blaze_core::threads::qos::AFTER_ANSWER` (`utility`),
    /// the caller's class given back after on every way out, a panic's included
    /// (`blaze_core::threads::qos::Lowered`). macOS only: Linux has no class, and its lowering
    /// (nice 19, the idle IO class) cannot be given back by an unprivileged thread, so there it
    /// is `AsCalled` (blaze_core::threads::qos's module doc).
    Scoped,
}

/// The choice [`after_answer`] makes: scoped (2026-11-21), `AsCalled` kept as the tested
/// control.
pub const AFTER_ANSWER_QOS: AfterAnswerQos = AfterAnswerQos::Scoped;

/// The calling thread's QoS class (a `qos_class_t`; 0 off macOS, which has no classes) as
/// [`after_answer`] was called and as its last step ended.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct QosSeen {
    pub called_at: u32,
    pub ran_at: u32,
}

/// The calling thread's class, as [`QosSeen`] holds it.
fn qos_now() -> Result<u32, PlaceError> {
    #[cfg(target_os = "macos")]
    return Ok(blaze_core::threads::qos::this_thread().map_err(PlaceError::Qos)?.0.raw());
    #[cfg(not(target_os = "macos"))]
    return Ok(0);
}

/// After the answer is printed, never waited on by it (the command's outer function calls it
/// once every handle over `lent` is gone): with `slices` past zero, one cleanup slice written
/// with no lock (pruning and sweeping bounded by blaze-cas `SLICE_BYTES`; a size-tiered merge,
/// `Tiers`, not cut by it, its bytes amortised to at most 16 a byte committed and 1.25 MiB a
/// commit, and after it a namespace of `S` bytes names at most `segments_most(S)` segments;
/// every copy a k-way merge streamed within blaze-cas `merge_holds_most`, 8 MiB of records,
/// 1 MiB of index and 20 bytes an input at most at once); this
/// command's lease let go of (its answer's roots, set before, keep what it made); the commit of
/// what is left as one segment a namespace and ONE flip, which names the slice too; then, while
/// blaze-cas `Plain::cleanup_goes_on` says so (only merges go on: a pruning slice ends the
/// cleanup, and so does a sweep unless a namespace's merge is due next, which sweep mode plans
/// before any sweep; so a cold build's dead records cost each command one slice),
/// one more slice a namespace still over its bound,
/// each named by a flip of its own, within the work budget (the bytes they read at most the
/// amortisation's bound of what this command committed, and one slice more; the rest is the
/// next command's); then the exit tail's seal. No lock is taken for writing cleanup, so no
/// command waits on it (blaze-cas `Driven::fold_slice`); its flips hold the lock for
/// milliseconds each, as a commit's.
///
/// THE PROFILE (rules 16, 17): each step is a span under `cx`, and a span's row carries the
/// meter's numbers of its stretch (bstd telemetry/brief.rs: `hashed` the bytes fed to BLAKE3,
/// `calls` and `in calls` the system calls and their time, `allocs` and `heap`), so the row
/// names which step took the time, which hashed the segment digest's bytes and which made the
/// syncs. Where each number comes from, as the code reads (blaze-cas plain/core.rs):
/// - the fold: a size-tiered merge's copy (`stream_segment`), every byte of it hashed for its
///   footer's digest as it is written, records already committed included; its `map_all`
///   first (a parked segment of an earlier boot checked whole: a digest of every byte);
/// - the commit: the arena's puts copied into one `Batch` a namespace (`Lent::commit`, a
///   CRC-32C a record) and again into the layout (`lay_out`), the new segment's digest over
///   all of it (`write_segment`, format.rs `seal_parts`), its writes, one flip; no sync
///   unless durable;
/// - more slices: each a merge's copy (hashed as the fold's) and a flip of its own;
/// - the seal: ONE `Effect::Barrier` (`F_BARRIERFSYNC`, Linux `fdatasync`) on EACH local
///   segment of this boot not sealed yet (`Plain::seal`), this command's phase commits'
///   (store.rs `commit_puts`) and a merge's copy among them, then one flip: the "fsync" calls
///   of a store that is not durable are these.
///
/// At [`AFTER_ANSWER_QOS`]: the class it ran at and the caller's are in [`AfterAnswer::qos`].
pub fn after_answer<D: Driver>(cx: &Cx, lent: &Lent<D, StoreAlloc>, slices: u32) -> Result<AfterAnswer, PlaceError> {
    after_answer_at(cx, lent, slices, AFTER_ANSWER_QOS)
}

/// [`after_answer`] at the class choice `qos`: the seam's two sides, for its test.
///
/// When `slices == 0` (foreground CLI commit-and-exit where background `blaze gc --background`
/// handles compaction and exit-tail sealing asynchronously), the calling thread stays at its
/// foreground QoS class (never throttled onto E-cores or behind background GC) and performs ONE
/// commit flip with ZERO redundant `F_BARRIERFSYNC` / second flip. When `slices > 0` (inline
/// compaction + seal), `qos` scopes the lowering to `AFTER_ANSWER` (`utility`) and restores the
/// caller's class on every exit.
pub(crate) fn after_answer_at<D: Driver>(cx: &Cx, lent: &Lent<D, StoreAlloc>, slices: u32, qos: AfterAnswerQos) -> Result<AfterAnswer, PlaceError> {
    let called_at = qos_now()?;
    #[cfg(target_os = "macos")]
    let _held = if slices > 0 {
        match qos {
            AfterAnswerQos::Scoped => {
                Some(blaze_core::threads::qos::Lowered::at_most(blaze_core::threads::qos::AFTER_ANSWER).map_err(PlaceError::Qos)?)
            }
            AfterAnswerQos::AsCalled => None,
        }
    } else {
        None
    };
    #[cfg(not(target_os = "macos"))]
    let (AfterAnswerQos::AsCalled | AfterAnswerQos::Scoped) = qos;
    let mut done = after_answer_steps(cx, lent, slices)?;
    done.qos = QosSeen { called_at, ran_at: qos_now()? };
    Ok(done)
}

/// [`after_answer`]'s steps, on the calling thread at the class it has.
fn after_answer_steps<D: Driver>(cx: &Cx, lent: &Lent<D, StoreAlloc>, slices: u32) -> Result<AfterAnswer, PlaceError> {
    let cx = cx.span("store after answer");
    // The put path's tally up to here: the phases' commits and every copy before the answer.
    let before = lent.put_tally();
    if slices > 0 {
        let _cx = cx.span("store after answer: fold a cleanup slice (map, merge copy hashed whole)");
        lent.fold_slice()?;
    }
    lent.release_lease();
    let committed = {
        let _cx = cx.span("store after answer: commit (batch copy, layout copy, segment digest, flip)");
        lent.commit()?
    };
    let first = lent.take_sliced();
    let (slice, swept) = if slices > 0 {
        let _cx = cx.span("store after answer: more cleanup slices (each a merge copy hashed whole, a flip)");
        lent.clean_on(first)?
    } else {
        (first, 0)
    };
    // Only run the exit-tail barrier + second manifest flip (`lent.seal()`) when running inline
    // maintenance (`slices > 0`). On the foreground exit path (`slices == 0`), `lent.commit()`
    // already persisted the new segment atomically with this boot's id (`proved == boot`, zero
    // re-hashing on subsequent same-boot reads), while `blaze gc --background` seals segments
    // asynchronously for cross-reboot fast-open (and across a power cut, blaze-cas verifies any
    // unsealed segment whole on first read, preserving 100% crash durability).
    let sealed = if slices > 0 {
        let _cx = cx.span("store after answer: seal (a barrier a segment of this boot unsealed, a flip)");
        lent.seal()?
    } else {
        false
    };
    let mut cleanup = {
        let _cx = cx.span("store after answer: the cleanup line");
        super::roots::CleanupLine::of(lent, slice)
    };
    if slices > 0 {
        cleanup.slices = swept;
    }
    cleanup.put = super::roots::PutLine { before_answer: before, after_answer: lent.put_tally().since(before) };
    Ok(AfterAnswer {
        segments_written: u32::try_from(committed.segments.len()).unwrap_or(u32::MAX),
        sealed,
        compaction_slices: cleanup.slices,
        cleanup,
        qos: QosSeen::default(),
    })
}

/// The tests' context for [`after_answer`] (its spans): a simulated world's root, its own
/// world beside the test's `Sim` store driver. The one home of it for every test of the
/// store's commit after the answer.
#[cfg(test)]
pub(crate) fn test_cx() -> Cx {
    let io = bstd::io::Io::simulated(bstd::io::SimWorld::new(1, bstd::io::Limits::USUAL));
    io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context")
}

#[cfg(test)]
#[path = "plain_place_tests.rs"]
mod tests;

/// The checkpoint rule's bounds (`checkpoint_due`): nothing put is never due; past
/// `COMMIT_BYTES` always; at `CHECKPOINT_AGE_NS` waited, any bytes; below both, not due. Under
/// the simulation, a store's clock is the sim's (`Lent::wall_ns`), so `PlainPlace::checkpoint`
/// falls where its tape says.
#[cfg(test)]
mod checkpoint_tests {
    use super::{CHECKPOINT_AGE_NS, COMMIT_BYTES, checkpoint_due};

    #[test]
    fn a_checkpoint_is_due_past_the_bytes_or_the_age_and_never_with_nothing_put() {
        assert!(!checkpoint_due(0, u64::MAX), "nothing put: nothing to commit");
        assert!(checkpoint_due(COMMIT_BYTES + 1, 0), "past the bytes, at once");
        assert!(!checkpoint_due(COMMIT_BYTES, CHECKPOINT_AGE_NS - 1), "at the bytes and under the age: not yet");
        assert!(checkpoint_due(1, CHECKPOINT_AGE_NS), "one byte, waited the age");
    }
}

/// The after-answer class's seam, on the simulation's store: with `slices > 0` (inline cleanup +
/// seal), scoped runs at `utility` and restores the caller's class while `AsCalled` leaves it
/// lowered; with `slices == 0` (foreground CLI commit-and-exit), it runs at full foreground QoS
/// (`UserInitiated`) with 1 commit and 0 seal barriers/flips.
#[cfg(all(test, target_os = "macos"))]
mod qos_tests {
    use blaze_cas::plain::Settings;
    use blaze_core::{
        FileName,
        sim::{Faults, Sim},
        threads::qos::{self, Qos},
    };
    use blaze_drive::Key;
    use blaze_ids::ContentId;

    use super::{AfterAnswerQos, Facts, PlainPlace, after_answer_at, config, open, test_cx};

    fn at(slices: u32, choice: AfterAnswerQos) -> (super::AfterAnswer, Qos) {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    qos::set_this_thread(Qos::UserInitiated, 0).expect("the caller's class");
                    let alloc = blaze_own::testing::heap().alloc();
                    let cfg = config(FileName::new("store").expect("a name"), &Settings::default(), Facts { boot: Some(1), nonce: 4242 }, alloc);
                    let lent = open(Sim::new(1, Faults::default()), cfg, alloc).expect("the store opens");
                    let mut place = PlainPlace::new(&lent, false);
                    place.put(Key(ContentId::of("qos test key", b"v1")), false, b"payload").expect("put");
                    drop(place);
                    let done = after_answer_at(&test_cx(), &lent, slices, choice).expect("the work after the answer");
                    (done, qos::this_thread().expect("the class after").0)
                })
                .join()
                .expect("the caller's thread")
        })
    }

    #[test]
    fn scoped_the_work_after_the_answer_runs_at_utility_and_the_caller_gets_its_class_back() {
        let (done, after) = at(1, AfterAnswerQos::Scoped);
        assert_eq!(done.qos, super::QosSeen { called_at: Qos::UserInitiated.raw(), ran_at: qos::AFTER_ANSWER.raw() });
        assert_eq!(after, Qos::UserInitiated, "the caller's class given back");
        assert!(done.sealed, "inline maintenance (`slices > 0`) seals the exit tail");
        let (done, after) = at(1, AfterAnswerQos::AsCalled);
        assert_eq!(done.qos.called_at, Qos::UserInitiated.raw());
        assert_eq!(after, Qos::Utility, "as called with inline maintenance, blaze-cas's lowering stays");
    }

    #[test]
    fn foreground_zero_slice_commit_keeps_foreground_qos_and_skips_second_seal_barrier() {
        let (done, after) = at(0, AfterAnswerQos::Scoped);
        assert_eq!(done.segments_written, 1, "one segment committed");
        assert!(!done.sealed, "zero-slice foreground commit does not run a redundant second seal flip");
        assert_eq!(done.cleanup.put.after_answer.disk.barriers, 0, "zero foreground barriers on commit");
        assert_eq!(done.qos, super::QosSeen { called_at: Qos::UserInitiated.raw(), ran_at: Qos::UserInitiated.raw() });
        assert_eq!(after, Qos::UserInitiated, "foreground commit never throttles the calling thread onto E-cores");
    }
}
