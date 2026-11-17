//! The command's store on blaze-cas's plain store (D114 piece (b); drive.md, "D114 rulings",
//! "The store under the command"): ONE store a user, one root (`<home>/.blaze/v2/store`), read
//! and written by every checkout and worktree; records read IN PLACE.
//!
//! Two types, by lifetime:
//! - [`Maps`]: the command's one append-only owner of what a get lends: the plain store
//!   ([`Driven`], whose segment mappings are never unmapped or moved while it lives) and this
//!   command's own puts (an arena of 16-aligned blocks that never moves or frees a byte before
//!   the command ends). Made before the store handle, dropped after every reader.
//! - [`PlainPlace`]: the store handle over `&'m Maps`. A get returns [`Held<'m>`], whose bytes
//!   borrow `Maps`, never the handle: a put, a commit or a compaction may run while a reader
//!   keeps the bytes of an earlier get.
//!
//! WRITES. A put is copied once into the arena and indexed there; nothing touches the disk on
//! the command's path. [`Maps::commit`] (after the answer, or when [`Maps::commit_due`] says
//! the arena passed [`COMMIT_BYTES`]) lays out what is not yet committed as one segment per
//! namespace and flips the head (blaze-cas plain.rs, "A commit, in order"). In the default mode
//! nothing is synced: a process crash anywhere leaves the store as it was or with the whole
//! commit (the flip is the commit; a segment no generation names is swept a grace period
//! later); a power cut may take a commit not yet sealed, never part of one. [`Maps::after_answer`]
//! commits, seals the exit tail (one barrier a segment of this boot, then one flip: the next
//! boot proves them by row, with no byte read) and runs a bounded compaction. The arena is
//! never freed at a commit: its slices are lent until the command ends.
//!
//! THE OLD HOME. The stand-in's folders (`v2/shared`, `v2/own` beside this store; a checkout's
//! `.blaze/v2/packs`, which the caller looks for through bstd) are an old form: looked for
//! once in a home's life (a root records it), reported by name by the caller, never read,
//! migrated or deleted; their records are made again (rule 26).
//!
//! Generic over the driver (rule 78): the simulation in the tests, the driver bstd lends in the
//! command (one driver a process: drive.md ruling 5).

use core::cell::{Cell, RefCell};
use core::fmt;
use core::marker::PhantomData;
use core::time::Duration;
use std::alloc::Global;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Batch, Driven, PlainConfig, Report, Settings};
use blaze_core::wait::Deadline;
use blaze_core::{Completion, Done, Driver, Effect, FileName, Tag as EffectTag};
use blaze_drive::Key;
use blaze_hash::ByHash;
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_keyed::store::StoreError;
use blaze_own::Alloc;

/// The store's folder under the driver's root (blaze's folder in the home): drive.md ruling 1.
pub(crate) const STORE_DIR: &str = "v2/store";

/// The stand-in's folders under the same root: an old home when either is there.
pub(crate) const OLD_FOLDERS: [&str; 2] = ["v2/shared", "v2/own"];

/// The root that says the old home was looked for: set by the first command on this store, so
/// no later command looks again (no stat on any later command's path).
const OLD_LOOKED: &str = "blaze.old-home-looked";

/// The kind every drive record is filed under in the plain store's index. The drive record's
/// own head says which drive kind it is; the store needs one number apart from its roots'
/// ([`blaze_cas::plain::ROOT_TAG`], `u16::MAX`) and the startup image's (`u16::MAX - 1`).
pub(crate) const DRIVE_TAG: Tag = Tag(u16::MAX - 2);

/// The store's effect tags on the shared driver (`PlainConfig::tags`), and this module's own
/// (the old home's look) apart from them.
const STORE_TAGS: u64 = 1 << 50;
const OWN_TAGS: u64 = 3 << 50;

/// Past this many uncommitted bytes in the arena, [`Maps::commit_due`] says to commit.
pub(crate) const COMMIT_BYTES: u64 = 256 << 20;

/// One arena chunk's blocks (1 MiB); a larger record gets a chunk of its own size.
const CHUNK_BLOCKS: usize = 1 << 16;

/// How long the old home's look waits for the driver.
const LOOK_WAIT: Duration = Duration::from_secs(10);

/// Why the store refused, by name (rule 19).
#[derive(Debug)]
pub enum PlaceError {
    /// The plain store stopped: its own typed error.
    Store { doing: &'static str, why: StoreError },
    /// Two values under one key (rule 21): the key misses an input of its value.
    TwoValues { key: Key },
    /// The arena could not take a record of this many bytes: the allocator refused.
    Arena { bytes: usize },
    /// The old home's look got no answer for `file` within [`LOOK_WAIT`].
    LookLate { file: &'static str },
    /// The old home's look of `file` failed in the driver.
    LookFailed { file: &'static str },
    /// A completion of a tag this module never asked arrived while it waited alone.
    Stray { tag: u64 },
    /// A namespace this machine may not write (a trusted builder's).
    NotWritable { namespace: Namespace },
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaceError::Store { doing, why } => write!(f, "the store under the home's .blaze/{STORE_DIR} stopped while {doing}: {why}"),
            PlaceError::TwoValues { key } => write!(f, "two values under one key {:?} (rule 21: the key misses an input of its value)", key.0),
            PlaceError::Arena { bytes } => write!(f, "this command's records could not take {bytes} more bytes: the allocator refused"),
            PlaceError::LookLate { file } => write!(f, "looking for the old home's .blaze/{file} took past {} s", LOOK_WAIT.as_secs()),
            PlaceError::LookFailed { file } => write!(f, "looking for the old home's .blaze/{file} failed in the driver"),
            PlaceError::Stray { tag } => write!(f, "the store's driver answered tag {tag:#x}, which nothing asked"),
            PlaceError::NotWritable { namespace } => write!(f, "namespace {} is a trusted builder's; this machine writes only its own", namespace.0),
        }
    }
}

fn stopped(doing: &'static str) -> impl FnOnce(StoreError) -> PlaceError {
    move |why| PlaceError::Store { doing, why }
}

/// The facts the store is opened with, passed in from the command's start (rule 75).
#[derive(Clone, Debug)]
pub struct Facts {
    /// blaze's folder in the home as the driver names it (the lent driver's `dir`), which the
    /// store's folder and the old home's are beneath; `None`: the driver's root itself.
    pub home: Option<FileName>,
    /// The machine's boot (the driver's fact, hashed to 64 bits), or `None`: then every
    /// segment is taken as of another boot and checked once (blaze-cas `PlainConfig::boot`).
    pub boot: Option<u64>,
    /// Drawn at random by the caller: two processes never name a segment alike.
    pub nonce: u64,
}

/// The proof that [`Held::bytes`] are the bytes filed under the key asked (rule 74). Only this
/// module makes one. It is earned in one of three ways:
/// 1. the bytes lie in a segment proved once this boot: by its manifest row (written or checked
///    this boot), by its seal (a barrier ordered it before the flip that sealed it), or by the
///    full check of its length, layout and digest the first time it is reached after a restart;
/// 2. the bytes are this command's own put, copied into [`Maps`]' arena by this tool;
/// 3. the bytes were fetched, checked against their signed statement and digest at the fetch,
///    and adopted by a flip (from then on, case 1).
///
/// It proves the bytes, not the form of the record they hold: a reader's form check is its own
/// and is made once, from this proof.
#[derive(Clone, Copy, Debug)]
pub struct Checked<'m> {
    _private: PhantomData<&'m ()>,
}

/// A record lent in place for `'m`, the life of the [`Maps`] borrowed.
#[derive(Clone, Copy, Debug)]
pub struct Held<'m> {
    pub bytes: &'m [u8],
    pub proof: Checked<'m>,
}

/// Sixteen bytes at sixteen-byte alignment: the arena's unit, so a put's payload starts as a
/// segment's does (blaze-keyed `store::ALIGN`).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

/// Where a put lies in the arena.
#[derive(Clone, Copy)]
struct PutAt {
    chunk: u32,
    /// In blocks.
    block: u32,
    len: u32,
    namespace: Namespace,
}

/// What the store did this command, for `--stats`.
#[derive(Clone, Copy, Default, Debug)]
pub struct Counts {
    pub gets: u64,
    /// Answered from this command's arena.
    pub from_puts: u64,
    /// Answered in place from a segment.
    pub from_segments: u64,
    pub misses: u64,
    pub puts: u64,
    /// A put of a key already held with equal bytes: nothing written.
    pub puts_same: u64,
    pub arena_bytes: u64,
    pub commits: u32,
    pub segments_written: u32,
    /// Whether the exit tail's seal flipped.
    pub sealed: bool,
    pub compaction_slices: u32,
}

/// A planted bug of the tests (`plain_place_tests.rs`): each one the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PlacePlant {
    None,
    /// A commit lays out only the last put since the one before (a walk of the put list that
    /// starts in the wrong place): a commit that returned `Ok` lost records.
    DropsEarlierPuts,
    /// A put skips the store's check for the key: other bytes under a committed key are kept
    /// beside it, two values under one key.
    SkipsStoreCheck,
    /// A pin does nothing: compaction past the cap lets a running program's records go.
    ForgetsPin,
}

struct Inner<'a, D: Driver> {
    store: Driven<D>,
    /// Each chunk made at its capacity and never pushed past it, so its buffer never moves.
    chunks: Vec<Vec<Block, Alloc<'a>>, Alloc<'a>>,
    puts: ByHash<[u8; 32], PutAt, Alloc<'a>>,
    /// The puts in the order they came: a commit lays out `order[committed..]`.
    order: Vec<[u8; 32], Alloc<'a>>,
    committed: usize,
    uncommitted_bytes: u64,
    /// A root set and not yet committed (the old home's mark, a pin).
    roots_pending: bool,
    counts: Counts,
}

/// The command's one owner of every byte a get lends (drive.md ruling 6). Not `Sync`: the
/// command's store thread uses it; the bytes it lends are plain shared slices, sent anywhere.
pub struct Maps<'a, D: Driver> {
    inner: RefCell<Inner<'a, D>>,
    alloc: Alloc<'a>,
    writes: Vec<Namespace, Global>,
    plant: PlacePlant,
    /// The old home's folders this open found ([`OLD_FOLDERS`], by index).
    old: [bool; 2],
    /// Set once an old home's report has been taken by the caller.
    old_taken: Cell<bool>,
}

/// `len` bytes at `ptr`, lent for `'m`.
///
/// # Safety
///
/// The one assumption the store's lending rests on (drive.md ruling 6): `ptr..ptr + len` lies
/// either in a segment mapping held by the [`Driven`] inside a [`Maps`] (blaze-cas plain.rs,
/// "Reads": each segment is mapped once a process and never unmapped during a run; a
/// [`blaze_core::Mapping`]'s bytes stay as they are while any clone lives; a retired segment's
/// file may be unlinked, its mapping stands), or in one of that `Maps`' arena chunks (each a
/// `Vec` made at its capacity and pushed only within it, so its buffer is never reallocated,
/// moved or freed while `Maps` lives, and a push writes only past the blocks already lent). The
/// caller borrows that `Maps` (or, in `block_bytes`, the chunk's blocks themselves) for `'m`,
/// so it lives, with nothing of it dropped, for `'m`. No
/// `&mut` to those bytes is ever made: a mapping is read-only and an arena block is written
/// once, before its first lend.
unsafe fn lend<'m>(ptr: *const u8, len: usize) -> &'m [u8] {
    // SAFETY: the function's contract, above.
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

/// `leaf` beneath `home` (or the driver's root), joined on the stack: one name made, no other
/// allocation. `None` past 512 bytes or for a name that is not one.
fn beneath(home: Option<&FileName>, leaf: &str) -> Option<FileName> {
    let Some(home) = home else { return FileName::new(leaf) };
    let mut buf = [0u8; 512];
    let (head, tail) = (home.as_str().as_bytes(), leaf.as_bytes());
    let len = head.len() + 1 + tail.len();
    if len > buf.len() {
        return None;
    }
    buf[..head.len()].copy_from_slice(head);
    buf[head.len()] = b'/';
    buf[head.len() + 1..len].copy_from_slice(tail);
    FileName::new(core::str::from_utf8(&buf[..len]).ok()?)
}

/// The proof, made here only.
fn checked<'m>() -> Checked<'m> {
    Checked { _private: PhantomData }
}

/// Runs `effects` (tags `OWN_TAGS + index`) on `driver` alone until each has its completion or
/// `wait` passes: the completions by index. `what` names each effect for an error.
fn drive_alone<D: Driver>(driver: &mut D, mut effects: Vec<Effect, Global>, what: &[&'static str], wait: Duration) -> Result<Vec<Done, Global>, PlaceError> {
    let count = effects.len();
    let mut got: Vec<Option<Done>, Global> = Vec::with_capacity_in(count, Global);
    got.resize_with(count, || None);
    driver.submit(&mut effects);
    let deadline = Deadline::after(driver.now(), wait);
    let mut done: Vec<Completion, Global> = Vec::with_capacity_in(count, Global);
    while let Some(at) = got.iter().position(Option::is_none) {
        if driver.poll(&mut done, deadline) == 0 && deadline.passed(driver.now()) {
            return Err(PlaceError::LookLate { file: what[at] });
        }
        for completion in done.drain(..) {
            let index = completion.tag.0.checked_sub(OWN_TAGS).and_then(|it| usize::try_from(it).ok()).filter(|it| *it < count);
            match index {
                Some(index) => got[index] = Some(completion.done),
                None => return Err(PlaceError::Stray { tag: completion.tag.0 }),
            }
        }
    }
    let mut out = Vec::with_capacity_in(count, Global);
    out.extend(got.into_iter().flatten());
    Ok(out)
}

impl<'a, D: Driver> Maps<'a, D> {
    /// The store at [`STORE_DIR`] under `driver`'s root, made if missing, and opened (blaze-cas
    /// plain core.rs `open`). On a store that never looked, the old home is looked for once
    /// ([`Maps::old_home`]).
    pub fn open(driver: D, settings: &Settings, facts: Facts, alloc: Alloc<'a>) -> Result<Maps<'a, D>, PlaceError> {
        Maps::open_planted(driver, settings, facts, alloc, PlacePlant::None)
    }

    pub(crate) fn open_planted(driver: D, settings: &Settings, facts: Facts, alloc: Alloc<'a>, plant: PlacePlant) -> Result<Maps<'a, D>, PlaceError> {
        let dir = beneath(facts.home.as_ref(), STORE_DIR).ok_or(PlaceError::Store { doing: "naming its folder", why: StoreError::Other { what: "the store's folder is not a relative name" } })?;
        let cfg = PlainConfig {
            dir,
            tags: STORE_TAGS,
            cap_bytes: settings.cap_bytes(),
            boot: facts.boot.unwrap_or(facts.nonce),
            nonce: facts.nonce,
            durable: settings.durable,
            // The caller adds the checkout's and the home's spellings ([`Maps::machine`]).
            machine: Vec::new_in(Global),
            writes: settings.writes(),
            reads: settings.reads(),
        };
        // The driver's root path is read only by the native pread path, which this module
        // never takes (a get is in place).
        let mut store = Driven::with(driver, std::path::Path::new("/"), cfg).map_err(stopped("opening"))?;
        let looked = store.core().roots().iter().any(|(name, _)| name == OLD_LOOKED);
        let mut old = [false; 2];
        if !looked {
            let mut effects = Vec::with_capacity_in(OLD_FOLDERS.len(), Global);
            for (at, folder) in OLD_FOLDERS.iter().enumerate() {
                let file = beneath(facts.home.as_ref(), folder).ok_or(PlaceError::LookFailed { file: folder })?;
                effects.push(Effect::Stat { file, tag: EffectTag(OWN_TAGS + at as u64) });
            }
            let dones = drive_alone(store.driver(), effects, &OLD_FOLDERS, LOOK_WAIT)?;
            for (at, done) in dones.into_iter().enumerate() {
                old[at] = match done {
                    Done::Stat(meta) => meta.is_some(),
                    _ => return Err(PlaceError::LookFailed { file: OLD_FOLDERS[at] }),
                };
            }
            // Recorded with this command's first commit (after the answer): no later command
            // looks. Nothing is kept under it.
            store.root(OLD_LOOKED, &[]).map_err(stopped("marking the old home looked for"))?;
        }
        let inner = Inner {
            store,
            chunks: Vec::new_in(alloc),
            puts: ByHash::new_in(alloc),
            order: Vec::new_in(alloc),
            committed: 0,
            uncommitted_bytes: 0,
            roots_pending: !looked,
            counts: Counts::default(),
        };
        let mut writes = Vec::with_capacity_in(3, Global);
        writes.extend(settings.writes());
        Ok(Maps { inner: RefCell::new(inner), alloc, writes, plant, old, old_taken: Cell::new(false) })
    }

    /// The old home's folders this open found, by name, once: the caller prints one line for
    /// each ("an old store under the home's .blaze/v2/own is not read by this blaze: its
    /// records are made again; delete it when no older blaze needs it").
    pub fn old_home(&self) -> impl Iterator<Item = &'static str> + '_ {
        let first = !self.old_taken.replace(true);
        OLD_FOLDERS.iter().zip(self.old).filter(move |(_, found)| first && *found).map(|(folder, _)| *folder)
    }

    /// Adds what names this machine (the checkout's and the home's paths, in each spelling the
    /// caller knows) to the scan of every put into a shared namespace (rule 25).
    pub fn machine(&self, paths: &[&std::path::Path]) {
        self.inner.borrow_mut().store.machine(paths);
    }

    /// The record under `key`, in place, for the life of this `Maps`: this command's own put,
    /// else the committed record of the first namespace read that holds it. `None`: absent.
    pub fn get(&self, key: Key) -> Option<Held<'_>> {
        let mut inner = self.inner.borrow_mut();
        inner.counts.gets += 1;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            let (ptr, len) = {
                let bytes = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
                (bytes.as_ptr(), bytes.len())
            };
            inner.counts.from_puts += 1;
            // SAFETY: `ptr..ptr + len` is in this `Maps`' arena chunk `at.chunk`, written by
            // the put before this lend; the lend is for `&self`, a borrow of this `Maps`
            // (`lend`'s contract).
            return Some(Held { bytes: unsafe { lend(ptr, len) }, proof: checked() });
        }
        let found = inner.store.get(&CasKey(key.0)).map(|got| (got.bytes.as_ptr(), got.bytes.len()));
        match found {
            Some((ptr, len)) => {
                inner.counts.from_segments += 1;
                // SAFETY: the store answers only from a proved segment's mapping (no put goes
                // into the `Driven` itself but roots, which carry another tag and domain), held
                // by the `Driven` inside this `Maps`; the lend is for `&self` (`lend`'s contract).
                Some(Held { bytes: unsafe { lend(ptr, len) }, proof: checked() })
            }
            None => {
                inner.counts.misses += 1;
                None
            }
        }
    }

    /// Whether `key` is held: this command's put, or committed in a namespace read.
    pub fn has(&self, key: Key) -> bool {
        let inner = self.inner.borrow();
        inner.puts.get(&key.0.0).is_some() || inner.store.has(&CasKey(key.0))
    }

    /// `bytes` under `key` in `namespace`, copied once into the arena; written to disk by the
    /// next commit. A key held with equal bytes is left as it is (rule 25: two builders of one
    /// key write equal bytes); with other bytes it is refused (rule 21).
    pub fn put(&self, key: Key, namespace: Namespace, bytes: &[u8]) -> Result<(), PlaceError> {
        if !self.writes.contains(&namespace) {
            return Err(PlaceError::NotWritable { namespace });
        }
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        inner.counts.puts += 1;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            let blocks = &inner.chunks[at.chunk as usize][at.block as usize..];
            if block_bytes(blocks, at.len as usize) == bytes && at.namespace == namespace {
                inner.counts.puts_same += 1;
                return Ok(());
            }
            return Err(PlaceError::TwoValues { key });
        }
        if self.plant != PlacePlant::SkipsStoreCheck
            && let Some(got) = inner.store.get(&CasKey(key.0))
        {
            if got.bytes == bytes {
                inner.counts.puts_same += 1;
                return Ok(());
            }
            return Err(PlaceError::TwoValues { key });
        }
        let need = bytes.len().div_ceil(16);
        let fits = inner.chunks.last().is_some_and(|chunk| chunk.capacity() - chunk.len() >= need);
        if !fits {
            let mut chunk: Vec<Block, Alloc<'a>> = Vec::new_in(self.alloc);
            chunk.try_reserve_exact(need.max(CHUNK_BLOCKS)).map_err(|_| PlaceError::Arena { bytes: bytes.len() })?;
            inner.chunks.try_reserve(1).map_err(|_| PlaceError::Arena { bytes: bytes.len() })?;
            inner.chunks.push(chunk);
        }
        inner.puts.try_reserve(1).map_err(|_| PlaceError::Arena { bytes: bytes.len() })?;
        inner.order.try_reserve(1).map_err(|_| PlaceError::Arena { bytes: bytes.len() })?;
        let chunk_at = inner.chunks.len() - 1;
        let chunk = &mut inner.chunks[chunk_at];
        let block = chunk.len();
        // Within the capacity reserved above: the buffer does not move (`lend`'s contract).
        for piece in bytes.chunks(16) {
            let mut one = Block([0; 16]);
            one.0[..piece.len()].copy_from_slice(piece);
            chunk.push(one);
        }
        let len = u32::try_from(bytes.len()).map_err(|_| PlaceError::Arena { bytes: bytes.len() })?;
        let at = PutAt { chunk: chunk_at as u32, block: block as u32, len, namespace };
        inner.puts.insert(key.0.0, at);
        inner.order.push(key.0.0);
        inner.uncommitted_bytes += bytes.len() as u64;
        inner.counts.arena_bytes += (need * 16) as u64;
        Ok(())
    }

    /// Whether the arena holds more than [`COMMIT_BYTES`] uncommitted: the caller commits at
    /// its next step between tasks.
    pub fn commit_due(&self) -> bool {
        self.inner.borrow().uncommitted_bytes > COMMIT_BYTES
    }

    /// Keeps `keys` under the root `name` from the next commit on: compaction keeps every
    /// record a root names (a running program's code records; a checkout's leaf table and its
    /// last answer for each request shape, under one root named by a digest of the checkout's
    /// path: drive.md ruling 3).
    pub fn pin(&self, name: &str, keys: &[Key]) -> Result<(), PlaceError> {
        if self.plant == PlacePlant::ForgetsPin {
            return Ok(());
        }
        let mut cas = Vec::with_capacity_in(keys.len(), Global);
        cas.extend(keys.iter().map(|key| CasKey(key.0)));
        let mut inner = self.inner.borrow_mut();
        inner.store.root(name, &cas).map_err(stopped("setting a root"))?;
        inner.roots_pending = true;
        Ok(())
    }

    /// What is not yet committed, laid out as one segment per namespace, and one flip
    /// (blaze-cas plain.rs, "A commit, in order"). The arena keeps every byte: what it lent
    /// stays lent.
    pub fn commit(&self) -> Result<(), PlaceError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        let upto = inner.order.len();
        if inner.committed == upto && !inner.roots_pending {
            return Ok(());
        }
        let mut batches: Vec<Batch, Global> = Vec::with_capacity_in(self.writes.len(), Global);
        batches.extend(self.writes.iter().map(|namespace| Batch::new(*namespace)));
        let from = match self.plant {
            PlacePlant::DropsEarlierPuts => upto.saturating_sub(1).max(inner.committed),
            _ => inner.committed,
        };
        for key in &inner.order[from..upto] {
            let Some(at) = inner.puts.get(key).copied() else { continue };
            let bytes = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
            let Some(batch) = batches.iter_mut().find(|batch| batch.namespace() == at.namespace) else {
                return Err(PlaceError::NotWritable { namespace: at.namespace });
            };
            batch.put(CasKey(ContentId(*key)), DRIVE_TAG, bytes).map_err(|why| PlaceError::Store { doing: "laying out a commit", why: why.into_store(DRIVE_TAG) })?;
        }
        let mut refs: Vec<&Batch, Global> = Vec::with_capacity_in(batches.len(), Global);
        refs.extend(batches.iter().filter(|batch| !batch.is_empty()));
        let committed = inner.store.commit_with(&refs).map_err(stopped("committing"))?;
        inner.committed = upto;
        inner.uncommitted_bytes = 0;
        inner.roots_pending = false;
        inner.counts.commits += 1;
        inner.counts.segments_written += u32::try_from(committed.segments.len()).unwrap_or(u32::MAX);
        Ok(())
    }

    /// After the answer is printed, never waited on by it: the commit, the exit tail's seal,
    /// then at most `slices` compaction slices (each bounded by blaze-cas `SLICE_BYTES`), on
    /// the calling thread lowered (blaze-cas `lower_this_thread`).
    pub fn after_answer(&self, slices: u32) -> Result<Counts, PlaceError> {
        self.commit()?;
        let mut inner = self.inner.borrow_mut();
        inner.counts.sealed = inner.store.seal().map_err(stopped("sealing the exit tail"))?;
        if slices > 0 {
            let done = inner.store.compact(slices).map_err(stopped("compacting"))?;
            inner.counts.compaction_slices += u32::try_from(done.len()).unwrap_or(u32::MAX);
        }
        Ok(inner.counts)
    }

    /// This command's counts so far.
    pub fn counts(&self) -> Counts {
        self.inner.borrow().counts
    }

    /// The open's report (segments proved by row, checked whole, lost), for the profile.
    pub fn report_with<T>(&self, read: impl FnOnce(&Report) -> T) -> T {
        read(self.inner.borrow().store.report())
    }

    /// The driver back, the store let go of: a test's simulation, crashed and opened again.
    pub fn into_driver(self) -> D {
        self.inner.into_inner().store.into_driver()
    }
}

/// The first `len` bytes of `blocks` (at most all of them), borrowed as long as `blocks`.
fn block_bytes(blocks: &[Block], len: usize) -> &[u8] {
    let len = len.min(blocks.len() * 16);
    // SAFETY: a `Block` is `repr(C)` over one `[u8; 16]` (size 16, no padding), so `blocks`
    // covers `16 * blocks.len()` initialized bytes, at least `len`; they are an arena chunk's,
    // borrowed here for the result's life, which is `blocks`' (`lend`'s contract, its second
    // case, for that borrow).
    unsafe { lend(blocks.as_ptr().cast::<u8>(), len) }
}

/// The store handle the command's `Store` seam is implemented on (the switch batch): the
/// namespace a record goes to is decided here (drive.md ruling 2), every byte lives in `Maps`.
pub struct PlainPlace<'m, 'a, D: Driver> {
    maps: &'m Maps<'a, D>,
    /// Whether this machine may write [`Namespace::PUBLIC`] (a trusted builder).
    public_writer: bool,
}

impl<'m, 'a, D: Driver> PlainPlace<'m, 'a, D> {
    pub fn new(maps: &'m Maps<'a, D>) -> PlainPlace<'m, 'a, D> {
        let public_writer = maps.writes.contains(&Namespace::PUBLIC);
        PlainPlace { maps, public_writer }
    }

    /// The record's namespace (drive.md ruling 2): `PUBLIC` only when every name in it is
    /// public AND this machine is a trusted builder; else `LOCAL`.
    pub fn namespace(&self, all_names_public: bool) -> Namespace {
        if all_names_public && self.public_writer { Namespace::PUBLIC } else { Namespace::LOCAL }
    }

    pub fn get(&self, key: Key) -> Option<Held<'m>> {
        self.maps.get(key)
    }

    pub fn put(&self, key: Key, all_names_public: bool, bytes: &[u8]) -> Result<(), PlaceError> {
        self.maps.put(key, self.namespace(all_names_public), bytes)
    }

    pub fn maps(&self) -> &'m Maps<'a, D> {
        self.maps
    }
}

#[cfg(test)]
#[path = "plain_place_tests.rs"]
mod tests;
