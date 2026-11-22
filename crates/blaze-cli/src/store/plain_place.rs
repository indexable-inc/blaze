//! The command's store on blaze-cas's plain store (D114 piece (b); drive.md, "D114 rulings",
//! "The store under the command"): ONE store a user of each form, at `<home>/.blaze/`
//! [`STORE_DIR`] (`v2/store/f5s2`, D160), read and written by every checkout and worktree;
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
use blaze_cas::plain::{Driven, Lent, LentCounts, LentError, Liveness, PlaceName, PlainConfig, Probes, ROOT_LIFE_NS, Settings, Snapshot, SnapshotError};
use blaze_core::{Driver, FileName};
use blaze_drive::Key;
use blaze_keyed::kind::{Namespace, Tag};
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
/// on to read it, named by blaze-cas alone ([`PlaceName::THIS`], `f5s2` today: the store's
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

/// The kind every drive record is filed under in the plain store's index. The drive record's
/// own head says which drive kind it is; the store needs one number apart from its roots'
/// ([`blaze_cas::plain::ROOT_TAG`], `u16::MAX`) and the startup image's (`u16::MAX - 1`).
pub(crate) const DRIVE_TAG: Tag = Tag(u16::MAX - 2);

/// The kind an expansion record is filed under (blaze-scope expansion_records.rs: a macro
/// call's text and origin column, keyed by their bytes, put after its crate's `CrateScope`
/// record): a family apart from the drive records', read only when a body reads a text a macro
/// wrote. `u16::MAX - 3` is [`crate::code_store::CODE_TAG`], `- 4` blaze-cas's pointers, `- 5`
/// [`crate::start_list::START_LIST_TAG`].
pub(crate) const EXPANSION_TAG: Tag = Tag(u16::MAX - 6);

/// The store's effect tags on the lent driver (`PlainConfig::tags`).
const STORE_TAGS: u64 = 1 << 50;

/// Past this many uncommitted bytes in the arena, a put commits first.
pub(crate) const COMMIT_BYTES: u64 = 256 << 20;

/// Why the store refused, by name (rule 19).
#[derive(Debug)]
pub enum PlaceError {
    /// The plain store would not open: its own typed error.
    Open { why: StoreError },
    /// The store's owner refused (two values, a namespace not writable, the arena, the store).
    Lent(LentError),
    /// A snapshot of the committed segments could not be taken (blaze-cas plain/snapshot.rs).
    Snapshot(SnapshotError),
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaceError::Open { why } => write!(f, "the store under the home's .blaze/{STORE_DIR} would not open: {why}"),
            PlaceError::Lent(error) => write!(f, "the store under the home's .blaze/{STORE_DIR}: {error}"),
            PlaceError::Snapshot(error) => write!(f, "the store under the home's .blaze/{STORE_DIR}: {error}"),
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
        // Everything, until the switch to the engine path (plan row D5) turns the sweep on:
        // `Reached`, which keeps what the roots reach and what a running command's lease keeps.
        // On the engine path every record a later command reads by key is reached now
        // (store/roots.rs, the census): the program root (code entries, pointers, the start list,
        // the `run_ids` and `run_names` tables of its certificate), the toolchain root (std's
        // code, std's names records and their expansion records), the answer's `.pages` root
        // (every standing row's record, and each names record's expansion records through the
        // index beside it), the dependency-heads root and the rows-form answer's roots. The old
        // drive answer graph under the sealed last answer (`set_last` roots only the sealed
        // record, not its `cert`, page table, pages or rows' records) is old-path work, not
        // rooted by decision: it goes with the old path, and a sweep before the switch would
        // drop it after each command.
        liveness: Liveness::Everything,
    }
}

/// The store opened lazily over `driver` (nothing mapped: a segment is mapped at its first
/// probe), owned by a [`Lent`] whose arena is in `alloc`, the caller's (rule 75).
pub fn open<D: Driver>(driver: D, cfg: PlainConfig<StoreAlloc>, alloc: StoreAlloc) -> Result<Lent<D, StoreAlloc>, PlaceError> {
    // The root path is read only by the native pread path, which this module never takes.
    let store = Driven::with(driver, std::path::Path::new("/"), cfg, alloc).map_err(|why| PlaceError::Open { why })?;
    Ok(Lent::new(store, alloc))
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
}

impl<'m, D: Driver> PlainPlace<'m, D> {
    /// `public_writer`: whether the settings name this machine a trusted writer.
    pub fn new(lent: &'m Lent<D, StoreAlloc>, public_writer: bool) -> PlainPlace<'m, D> {
        PlainPlace::planted(lent, public_writer, PlacePlant::None)
    }

    pub(crate) fn planted(lent: &'m Lent<D, StoreAlloc>, public_writer: bool, plant: PlacePlant) -> PlainPlace<'m, D> {
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
        self.lent.put_parts(&CasKey(key.0), self.namespace(all_names_public), DRIVE_TAG, parts)?;
        Ok(())
    }

    /// A record keyed by its own bytes (an expansion record: `tag` [`EXPANSION_TAG`]) that many
    /// owners of either namespace may name: equal bytes already held under `key`, this
    /// command's or committed, in any namespace and under any tag (a record a remote fetch
    /// filed under [`DRIVE_TAG`]), is [`Put::Same`], so a toolchain crate's public record and a
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
    /// root of that name.
    pub fn root(&self, name: &str) -> Result<Option<Held<'m>>, PlaceError> {
        let Some((_, at)) = self.lent.roots().into_iter().find(|(it, _)| it == name) else { return Ok(None) };
        self.get(Key(at.0))
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

    /// The unexpired root `name` set again, naming what it names, for `life_ns` from the next
    /// commit: a command that answered from what the root reaches without writing it again
    /// (`engine-check` whose certificate stood) still renews it, so a checkout built every day
    /// never sees its answer expire. Answers whether there was such a root (an expired one is
    /// never named again: a slice may have swept what it named). Equal bytes within a day of the
    /// last renewal write nothing (blaze-cas `set_root`).
    pub(crate) fn renew(&self, name: &str, life_ns: u64) -> Result<bool, PlaceError> {
        let Some(key) = self.lent.root_named(name) else { return Ok(false) };
        let Some(held) = self.lent.get(&key)? else { return Ok(false) };
        let indexed = held.tag == blaze_cas::plain::INDEXED_ROOT_TAG;
        let mut named: Vec<CasKey, StoreAlloc> = Vec::with_capacity_in(held.bytes.len() / 32, *self.lent.alloc());
        named.extend(held.bytes.chunks_exact(32).map(|it| CasKey(blaze_ids::ContentId(it.try_into().expect("32 bytes")))));
        if indexed {
            self.lent.root_through(name, &named, life_ns)?;
        } else {
            self.lent.root_for(name, &named, life_ns)?;
        }
        Ok(true)
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
        Ok(self.lent.get(&key)?.filter(|held| held.tag == blaze_cas::plain::INDEX_TAG).map(|_| key))
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
pub fn after_answer<D: Driver>(cx: &Cx, lent: &Lent<D, StoreAlloc>, slices: u32) -> Result<AfterAnswer, PlaceError> {
    let cx = cx.span("store after answer");
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
    let sealed = {
        let _cx = cx.span("store after answer: seal (a barrier a segment of this boot unsealed, a flip)");
        lent.seal()?
    };
    let mut cleanup = {
        let _cx = cx.span("store after answer: the cleanup line");
        super::roots::CleanupLine::of(lent, slice)
    };
    if slices > 0 {
        cleanup.slices = swept;
    }
    Ok(AfterAnswer { segments_written: u32::try_from(committed.segments.len()).unwrap_or(u32::MAX), sealed, compaction_slices: cleanup.slices, cleanup })
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
