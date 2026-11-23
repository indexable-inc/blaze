//! [`Lent`]: the command's one append-only owner of every byte a get lends (drive.md, "The
//! store under the command", ruling 6; home decided by the coordinator 2026-11-16: blaze-cas,
//! beside the guarantee the lend rests on).
//!
//! It owns the plain store ([`Driven`], whose segment mappings are never unmapped while it is
//! open) and an arena of this command's own puts: a LIST of chunks, each made at its capacity
//! and never pushed past it, so no chunk's buffer is ever moved or freed while the `Lent`
//! lives (never one growing buffer). A get returns [`Lend<'l>`], whose bytes borrow the `Lent`
//! (`'l` is a borrow of `&self`): a later get that maps more segments, a put that adds a
//! chunk, a commit, a seal or a compaction may run while a reader keeps the bytes of an
//! earlier get, and nothing outlives the `Lent` by construction. No caller casts a lifetime.
//!
//! Not `Sync`: the command's main thread uses it. Pool tasks read through the lock-free
//! handle after [`Lent::map_all`] (probe.rs, "WHO CALLS WHAT").
//!
//! The `&mut Driven` is never handed out: a caller that could replace the store could drop its
//! mappings under a lend. Each store operation the command needs is a method here.

use core::cell::RefCell;
use core::fmt;
use std::alloc::Allocator;

use blaze_core::Driver;
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::{ContentId, crc32c};
use blaze_keyed::kind::{Namespace, ReservedTag, Tag};
use blaze_keyed::store::{Holder, Put, StoreError, TwoValues};
use blaze_mem::Kept;
use blaze_own::Arc;

use super::arena::{Arena, ArenaView, RunEntry, Visibility};
use super::core::{Batch, Committed, GcPlan, LentRecord, Named, POINTER_LEN, Probes, PutPath, Renewed, Report, Sliced};
use super::driven::{Driven, HeadIdentity};
use super::format::record_crc;
use crate::Key;

/// One arena chunk's blocks (1 MiB); a larger record gets a chunk of its own size.
const CHUNK_BLOCKS: usize = 1 << 16;

/// Why a [`Lent`] refused, by name (rule 19).
#[derive(Clone, Debug)]
pub enum LentError {
    /// The plain store stopped while `doing`: its own typed error.
    Store { doing: &'static str, why: StoreError },
    /// Two values under one key (rule 21): the key misses an input of its value. The store's one
    /// shape ([`TwoValues`]: the held record, this command's own put or a committed one, and the
    /// refused put, each by kind and length, and their first differing byte: `None` when the
    /// bytes are equal and the kind or namespace is not), and each side's namespace.
    TwoValues { two: TwoValues, namespace: Namespace, held_namespace: Namespace },
    /// The arena's allocator refused a record of this many bytes.
    Arena { bytes: usize },
    /// The shared arena (`Visibility::InArena`, arena.rs) could not take `wanted` more of
    /// `what`: a reservation used up, or memory the kernel would not commit.
    ArenaReserve { what: &'static str, wanted: usize },
    /// A namespace this store does not write (a trusted builder's).
    NotWritable { namespace: Namespace },
    /// A pointer met where a record was asked, or a record where a pointer was: a pointer is
    /// written by [`Lent::point`] and read by [`Lent::pointed`] alone.
    PointerTag { key: Key },
    /// [`Lent::point`] to a target this store does not hold: a pointer is put after its target.
    PointsAtNothing { name: Key, target: Key },
    /// The pointer `name` names a target the store no longer holds (compaction let it go over
    /// the cap): typed, never a guess at another.
    TargetGone { name: Key, target: Key },
    /// The pointer's target is a record of another tag than the one asked.
    TargetTag { name: Key, target: Key, want: Tag, found: Tag },
    /// A pointer record whose payload is not one key.
    PointerForm { name: Key, len: usize },
}

impl fmt::Display for LentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LentError::Store { doing, why } => write!(f, "the plain store stopped while {doing}: {why}"),
            LentError::TwoValues { two, namespace, held_namespace } => {
                write!(f, "{two} (held in namespace {}, put in namespace {})", held_namespace.0, namespace.0)?;
                match two.differs_at {
                    Some(_) => Ok(()),
                    None => f.write_str(", bytes equal"),
                }
            }
            LentError::Arena { bytes } => write!(f, "this command's records could not take {bytes} more bytes: the allocator refused"),
            LentError::ArenaReserve { what, wanted } => write!(f, "this command's shared arena could not take {wanted} more {what} (a reservation used up, or memory not committed)"),
            LentError::NotWritable { namespace } => write!(f, "namespace {} is a trusted builder's; this machine writes only its own", namespace.0),
            LentError::PointerTag { key } => write!(f, "{:?}: a pointer met as a record, or a record as a pointer (a pointer is written by point and read by pointed alone)", key.0),
            LentError::PointsAtNothing { name, target } => write!(f, "pointer {:?}: its target {:?} is not in the store (a pointer is put after its target)", name.0, target.0),
            LentError::TargetGone { name, target } => write!(f, "pointer {:?}: its target {:?} is no longer in the store", name.0, target.0),
            LentError::TargetTag { name, target, want, found } => write!(f, "pointer {:?}: its target {:?} is a record of tag {} where tag {} was asked", name.0, target.0, found.0, want.0),
            LentError::PointerForm { name, len } => write!(f, "pointer {:?}: its record holds {len} bytes, not one 32-byte key", name.0),
        }
    }
}

fn stopped(doing: &'static str) -> impl FnOnce(StoreError) -> LentError {
    move |why| LentError::Store { doing, why }
}

/// The record a key holds when a put of other bytes meets it.
struct Held<'b> {
    namespace: Namespace,
    tag: Tag,
    bytes: &'b [u8],
    own: bool,
}

impl Held<'_> {
    /// The refusal of a put of `bytes` (in `namespace`, of `tag`) under `key`, which holds this.
    fn against(&self, key: &Key, namespace: Namespace, tag: Tag, bytes: &[u8]) -> LentError {
        let holder = if self.own { Holder::ThisProcess } else { Holder::Committed };
        LentError::TwoValues { two: two_values(key, holder, self.tag, self.bytes, tag, bytes), namespace, held_namespace: self.namespace }
    }
}

/// The store's two-values refusal of `bytes` (kind `tag`) under `key`, which holds `held_bytes`
/// (kind `held_tag`, put by `held`): both lengths and their first differing byte (rule 19). The
/// one place a raise site with both values at hand makes it.
pub(crate) fn two_values(key: &Key, held: Holder, held_tag: Tag, held_bytes: &[u8], tag: Tag, bytes: &[u8]) -> TwoValues {
    TwoValues {
        key: key.0,
        tag,
        len: Some(bytes.len() as u64),
        held_tag,
        held_len: held_bytes.len() as u64,
        held,
        differs_at: first_difference(held_bytes, bytes).map(|at| at as u64),
    }
}

/// The offset of the first byte where `a` and `b` differ: the shorter's length when one is a
/// prefix of the other, `None` when they are equal.
pub fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => Some(at),
        None if a.len() == b.len() => None,
        None => Some(a.len().min(b.len())),
    }
}

/// Whether `bytes` are the concatenation of `parts`, read in place.
fn parts_equal(bytes: &[u8], parts: &[&[u8]]) -> bool {
    let mut rest = bytes;
    for part in parts {
        match rest.split_at_checked(part.len()) {
            Some((head, tail)) if head == *part => rest = tail,
            _ => return false,
        }
    }
    rest.is_empty()
}

/// The concatenation of `parts`, in the `Lent`'s allocator: only for a refusal, which names the
/// first byte where a put differs from what its key holds (rule 19).
fn joined<A: Allocator>(parts: &[&[u8]], alloc: A) -> Vec<u8, A> {
    let mut all = Vec::new_in(alloc);
    for part in parts {
        all.extend_from_slice(part);
    }
    all
}

/// A record lent in place for `'l`, a borrow of the [`Lent`].
#[derive(Clone, Copy, Debug)]
pub struct Lend<'l> {
    pub tag: Tag,
    pub namespace: Namespace,
    pub bytes: &'l [u8],
    /// This command's own put (from the arena), not a committed record.
    pub own: bool,
    /// An own put in the shared arena (`Visibility::InArena`, arena.rs): the one copy every
    /// reader reads, in place for `'l` as a segment's bytes are.
    pub shared: bool,
}

impl<'l> Lend<'l> {
    /// The record as bytes of a segment's mapping, or of the shared arena, for a reader that
    /// reads them where they lie for `'l` (blaze-cli engine_records.rs): refused when it is this
    /// command's own put in the private arena (`Visibility::AfterCommit`'s copy, which a get
    /// keeps in the heap to the `Lent`'s end) or when it does not start at a multiple of
    /// [`ALIGN`](blaze_keyed::store::ALIGN), which the layout gives every record (core.rs
    /// `lay_out`, format.rs `check_segment`) and the shared arena every put (a block boundary).
    /// Never a copy in its place (rule 3).
    pub fn in_place(self) -> Result<Aligned16<'l>, NotInPlace> {
        if self.own && !self.shared {
            return Err(NotInPlace::Arena { len: self.bytes.len() });
        }
        Aligned16::new(self.bytes).map_err(NotInPlace::Misaligned)
    }
}

/// The alignment every record of a segment starts at, and every section a reader cuts at a
/// multiple of it inside one (blaze-drive record.rs `PAYLOAD_ALIGN`, blaze-cli names/sections.rs).
const ALIGN_BYTES: usize = blaze_keyed::store::ALIGN as usize;

/// Bytes that start at a multiple of sixteen (rule 76: the property the store gives, said by a
/// type): a reader whose views read words in place takes one, so a misaligned slice is refused
/// where it is made, once, and never copied to an aligned place behind the reader's back. An
/// empty slice is aligned: no byte of it is read.
#[derive(Clone, Copy, Debug)]
pub struct Aligned16<'a>(&'a [u8]);

/// Bytes refused as [`Aligned16`]: how far past a multiple of sixteen they start, and their length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Misaligned {
    pub by: usize,
    pub len: usize,
}

impl fmt::Display for Misaligned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes that start {} past a multiple of {ALIGN_BYTES} (rule 76: read in place only when aligned)", self.len, self.by)
    }
}

impl core::error::Error for Misaligned {}

/// Why a record is not lent as bytes of a mapping ([`Lend::in_place`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotInPlace {
    /// This command's own put: the arena's copy in the heap, not yet committed.
    Arena { len: usize },
    Misaligned(Misaligned),
}

impl fmt::Display for NotInPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotInPlace::Arena { len } => write!(f, "a record of {len} bytes read from this command's arena, not from a segment's mapping (rule 51: read in place after its commit)"),
            NotInPlace::Misaligned(why) => write!(f, "a record of a segment: {why}"),
        }
    }
}

impl core::error::Error for NotInPlace {}

impl<'a> Aligned16<'a> {
    /// `bytes`, when they start at a multiple of sixteen.
    pub fn new(bytes: &'a [u8]) -> Result<Aligned16<'a>, Misaligned> {
        let by = bytes.as_ptr().addr() % ALIGN_BYTES;
        if by != 0 && !bytes.is_empty() {
            return Err(Misaligned { by, len: bytes.len() });
        }
        Ok(Aligned16(bytes))
    }

    /// `bytes[at..at + len]`, when `at` is a multiple of sixteen and the range is inside;
    /// `None` past the end. A cut at another place is [`Misaligned`] (`Ok(None)` never hides it).
    pub fn cut(self, at: usize, len: usize) -> Result<Option<Aligned16<'a>>, Misaligned> {
        let Some(part) = at.checked_add(len).and_then(|end| self.0.get(at..end)) else { return Ok(None) };
        Aligned16::new(part).map(Some)
    }

    pub fn bytes(self) -> &'a [u8] {
        self.0
    }

    pub fn len(self) -> usize {
        self.0.len()
    }

    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }
}

/// What the lends of one command did, for `--stats`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct LentCounts {
    pub gets: u64,
    pub from_puts: u64,
    pub from_segments: u64,
    pub misses: u64,
    pub puts: u64,
    /// A put of a key already held with equal bytes: nothing kept.
    pub puts_same: u64,
    /// Bytes copied into the arena (each put's, rounded to its blocks), ever.
    pub arena_bytes: u64,
    /// Puts and points copied into the arena: each one a copy of its bytes in this process's
    /// heap (rule 51's measure; blaze-cli engine_store.rs `Ledger`).
    pub copied: u64,
    /// Bytes of the arena's puts alive now: a commit frees every chunk that lent nothing.
    pub held_bytes: u64,
    /// Chunks freed by commits.
    pub freed_chunks: u32,
    pub chunks: u32,
    /// Records a commit carried forward because a cleanup slice swept their segment under it
    /// (`Plain::carried`): work kept, not refused.
    pub carried: u64,
    /// Bytes [`Lent::commit`] copied out of the arena into its batches (each with its CRC-32C).
    pub batch_bytes: u64,
    /// [`Lent::commit`]s, and the wall nanoseconds the calling thread spent in them.
    pub commits: u32,
    pub commit_ns: u64,
}

/// THE PUT PATH'S TALLY (rule 16: measured before it is changed; rule 67, its floor): what one
/// command's puts cost from the arena to a mapped segment, each number counted where its step
/// runs. The floor this is held against: `arena_bytes` once (the one copy, worker's bytes into
/// the arena), `batch_bytes` and `laid_bytes` 0 (no copy out of the arena: the layout is the
/// arena's slices, written by a vectored write), no commit on the command thread before the
/// answer, one segment and one barrier a namespace a command. `put - before` of two tallies is
/// the stretch between ([`PutTally::since`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PutTally {
    /// Records copied into the arena (puts and points), and their bytes rounded to blocks.
    pub copied: u64,
    pub arena_bytes: u64,
    /// Bytes copied out of the arena into the commits' batches.
    pub batch_bytes: u64,
    /// Commits, and the wall nanoseconds the committing thread spent in them.
    pub commits: u32,
    pub commit_ns: u64,
    /// The disk side, from the plain store ([`PutPath`]).
    pub disk: PutPath,
}

impl PutTally {
    /// What happened after `before` was taken: each count less `before`'s.
    pub fn since(self, before: PutTally) -> PutTally {
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let n = |a: u32, b: u32| a.saturating_sub(b);
        let (now, was) = (self.disk, before.disk);
        PutTally {
            copied: d(self.copied, before.copied),
            arena_bytes: d(self.arena_bytes, before.arena_bytes),
            batch_bytes: d(self.batch_bytes, before.batch_bytes),
            commits: n(self.commits, before.commits),
            commit_ns: d(self.commit_ns, before.commit_ns),
            disk: PutPath {
                laid_commits: n(now.laid_commits, was.laid_commits),
                laid_bytes: d(now.laid_bytes, was.laid_bytes),
                digested_bytes: d(now.digested_bytes, was.digested_bytes),
                segments_written: n(now.segments_written, was.segments_written),
                merged_bytes: d(now.merged_bytes, was.merged_bytes),
                barriers: n(now.barriers, was.barriers),
                syncs: n(now.syncs, was.syncs),
                root_probes: d(now.root_probes, was.root_probes),
            },
        }
    }
}

impl fmt::Display for PutTally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let disk = &self.disk;
        write!(
            f,
            "{} records copied into the arena ({} bytes); {} commits, {:.3} ms in them; copied out {} bytes to batches, {} to layouts; digested {} bytes; {} segments written, {} bytes merged; {} barriers, {} syncs; {} root names looked for",
            self.copied,
            self.arena_bytes,
            self.commits,
            self.commit_ns as f64 / 1e6,
            self.batch_bytes,
            disk.laid_bytes,
            disk.digested_bytes,
            disk.segments_written,
            disk.merged_bytes,
            disk.barriers,
            disk.syncs,
            disk.root_probes,
        )
    }
}

/// The planted bug of `lent_tests` (an arena that moves what it lent); outside tests, none.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LentPlant {
    None,
    /// The arena as ONE growing buffer: a put past its capacity reallocates it, moving every
    /// byte lent before (the address check of the test catches the move).
    OneBuffer,
    /// The commit flips the roots in a commit of their own BEFORE the records' (root_commit_tests:
    /// a cut between the two leaves a root naming records no generation holds).
    RootFirst,
    /// The commit makes the pointers durable in a commit of their own BEFORE the records they
    /// name (pointer_tests: a cut between the two leaves a pointer to a target no generation
    /// holds).
    PointerFirst,
    /// The commit frees every chunk, a chunk a get lent from too (lent_tests: the held bytes
    /// after a commit under a live lend show it; the freed lend is never read).
    FreesLent,
    /// The commit leaves the keys of a chunk a get lent from in `puts`, as it did until
    /// 2026-11-18: a later get of any key of that chunk is answered from the arena, which a
    /// reader in place refuses (aligned_tests: the key beside the one lent shows it).
    KeepsLentKeys,
}

/// Sixteen bytes at sixteen-byte alignment: a put's payload starts as a segment's does.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

/// Where an own put's bytes lie.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// In the private arena's chunk of this index (`Visibility::AfterCommit`).
    Chunk(u32),
    /// In the current shared arena (`Inner::shared`, `Visibility::InArena`), with its record's
    /// CRC (`format::record_crc`), made at the put while its parts were in cache: the commit's
    /// layout takes it and reads the bytes once, to copy them (core.rs `LentRecord`).
    Shared { crc: u32 },
}

#[derive(Clone, Copy)]
struct PutAt {
    place: Place,
    /// In blocks, from the start of its chunk or of the shared arena.
    block: u32,
    len: u32,
    tag: Tag,
    namespace: Namespace,
    /// Its place in `Inner::order`: a pointer pointed again before the commit keeps it, one
    /// pointed again after it takes a new one.
    order: u32,
}

struct Inner<D: Driver, A: Allocator + Clone> {
    store: Driven<D, A>,
    /// Each chunk made at its capacity and never pushed past it: its buffer never moves.
    chunks: Vec<Vec<Block, A>, A>,
    /// Each chunk's: whether a get lent bytes of it. A chunk that lent is kept to the `Lent`'s
    /// end and takes no more puts; one that lent nothing is freed by the commit that writes its
    /// puts ([`Lent::commit`]), so no put's bytes stay in the heap past their commit (rule 51).
    lent: Vec<bool, A>,
    puts: Map<[u8; 32], PutAt, BuildKeyHasher, A>,
    /// The puts in the order they came: a commit lays out `order[committed..]`.
    order: Vec<[u8; 32], A>,
    committed: usize,
    uncommitted_bytes: u64,
    counts: LentCounts,
    /// `Visibility::InArena`: the shared arena of the puts since the last commit (made at the
    /// first put after it), whether a get lent from it, and `order`'s index below which its
    /// puts are sealed into runs (each snapshot take seals the rest: [`Lent::seal_run`]).
    shared: Option<Arc<Arena, Kept>>,
    shared_lent: bool,
    sealed: usize,
    /// Arenas a get lent from, kept to the `Lent`'s end (`lend`'s contract), in the arena's
    /// shared allocator: a commit drops the current one otherwise.
    kept_arenas: Vec<Arc<Arena, Kept>, A>,
}

impl<D: Driver, A: Allocator + Clone> Inner<D, A> {
    /// The bytes of the own put `at`, for the borrow of the `Inner`.
    fn kept(&self, at: &PutAt) -> &[u8] {
        match at.place {
            Place::Chunk(chunk) => block_bytes(&self.chunks[chunk as usize][at.block as usize..], at.len as usize),
            // A shared put is in the current arena, published whole before its key entered
            // `puts` (`Lent::copy_in`), and every key leaves `puts` at the commit that drops it.
            Place::Shared { .. } => self
                .shared
                .as_ref()
                .and_then(|arena| arena.bytes(u32::MAX, at.block, at.len))
                .expect("an own put in the shared arena lies in the current arena, published whole (lent.rs copy_in)"),
        }
    }
}

/// The command's one owner of every byte a get lends: see the module's docs.
pub struct Lent<D: Driver, A: Allocator + Clone> {
    inner: RefCell<Inner<D, A>>,
    alloc: A,
    plant: LentPlant,
    /// Where readers find this command's own puts (arena.rs, the put path's seam).
    visibility: Visibility,
}

/// `len` bytes at `ptr`, lent for `'l`.
///
/// # Safety
///
/// The one assumption the lend rests on. `ptr..ptr + len` lies in a segment mapping held by
/// the [`Driven`] inside a [`Lent`], or in one of that `Lent`'s arena chunks. The store never
/// unmaps a segment while it is open; a retired file may be unlinked and its mapping stands.
/// A chunk is never moved (made at its capacity, pushed only within it), and a chunk any lend
/// was made from is never freed before the `Lent`: [`Lent::get`], the one place arena bytes
/// are lent beyond a call, marks the chunk (`Inner::lent`) before it lends, and a commit frees
/// only an unmarked chunk (whose bytes no reference outlives: every other read of a chunk, a
/// put's compare, a commit's layout, a pointer's decode, ends inside its call), and takes every
/// key out of `Inner::puts`, so no later get reaches a freed chunk. Or `ptr..ptr + len` lies in
/// that `Lent`'s current shared arena (arena.rs: `AppendVec`s, whose reservations never move
/// and whose published slots are never written again): a get that lends from it marks it
/// (`Inner::shared_lent`) and the commit that drops it moves it into `Inner::kept_arenas`
/// instead, to the `Lent`'s end. The caller borrows that `Lent` for `'l`,
/// so it lives, with nothing lent of it dropped, for `'l`; no `&mut` to those bytes is ever made
/// (a mapping is read-only, a block is written once, before its first lend).
unsafe fn lend<'l>(ptr: *const u8, len: usize) -> &'l [u8] {
    // SAFETY: the function's contract, above.
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

/// The first `len` bytes of `blocks`, as plain bytes.
fn block_bytes(blocks: &[Block], len: usize) -> &[u8] {
    let len = len.min(blocks.len() * 16);
    // SAFETY: a `Block` is `repr(C)` over one `[u8; 16]` (size 16, no padding), so `blocks`
    // covers `16 * blocks.len()` initialized bytes, at least `len`, borrowed for `blocks`' life
    // (`lend`'s contract, its chunk case, for that borrow).
    unsafe { lend(blocks.as_ptr().cast::<u8>(), len) }
}

impl<D: Driver, A: Allocator + Clone> Lent<D, A> {
    /// The owner over an opened store (lazy: [`Driven::with`]); the arena in `alloc`.
    pub fn new(store: Driven<D, A>, alloc: A) -> Lent<D, A> {
        Lent::planted(store, alloc, LentPlant::None)
    }

    /// [`Lent::new`] with its put path's [`Visibility`] chosen (the seam; arena.rs).
    pub fn with_visibility(store: Driven<D, A>, alloc: A, visibility: Visibility) -> Lent<D, A> {
        Lent::planted_in(store, alloc, LentPlant::None, visibility)
    }

    pub(crate) fn planted(store: Driven<D, A>, alloc: A, plant: LentPlant) -> Lent<D, A> {
        Lent::planted_in(store, alloc, plant, Visibility::AfterCommit)
    }

    pub(crate) fn planted_in(store: Driven<D, A>, alloc: A, plant: LentPlant, visibility: Visibility) -> Lent<D, A> {
        let inner = Inner {
            store,
            chunks: Vec::new_in(alloc.clone()),
            lent: Vec::new_in(alloc.clone()),
            puts: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: alloc.clone(), capacity: None }),
            order: Vec::new_in(alloc.clone()),
            committed: 0,
            uncommitted_bytes: 0,
            counts: LentCounts::default(),
            shared: None,
            shared_lent: false,
            sealed: 0,
            kept_arenas: Vec::new_in(alloc.clone()),
        };
        Lent { inner: RefCell::new(inner), alloc, plant, visibility }
    }

    /// Where readers find this command's own puts.
    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    /// The store's driver's wall clock (rule 75: the clock the command handed the store; the
    /// simulation's under a test), for a caller's checkpoint by age.
    pub fn wall_ns(&self) -> u64 {
        self.inner.borrow_mut().store.driver().wall_ns()
    }

    /// The record under `key`, for the life of this borrow: this command's own put while it
    /// is not yet committed (from the arena: not in place), else the committed record of the
    /// first namespace read that holds it (a segment mapped at its first probe, `Driven::get`),
    /// in place. `None`: absent.
    pub fn get(&self, key: &Key) -> Result<Option<Lend<'_>>, LentError> {
        self.get_hinted(key, None)
    }

    /// Zero-allocation lookup of manifest root `name` and its root record, using the root's
    /// creation generation as a segment-locality hint (avoids mapping older/larger segments on
    /// warm reads).
    pub fn get_root(&self, name: &str) -> Result<Option<Lend<'_>>, LentError> {
        let target = self.inner.borrow().store.core().root_target(name);
        let Some((key, generation)) = target else {
            return Ok(None);
        };
        self.get_hinted(&key, Some(generation))
    }

    fn get_hinted(&self, key: &Key, hint_gen: Option<u64>) -> Result<Option<Lend<'_>>, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        inner.counts.gets += 1;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            if at.tag == ReservedTag::Pointer.tag() {
                return Err(LentError::PointerTag { key: *key });
            }
            let bytes = inner.kept(&at);
            let (ptr, len) = (bytes.as_ptr(), bytes.len());
            inner.counts.from_puts += 1;
            // Marked before the lend: this chunk, or this shared arena, is kept to the `Lent`'s
            // end (`lend`'s contract).
            match at.place {
                Place::Chunk(chunk) => inner.lent[chunk as usize] = true,
                Place::Shared { .. } => inner.shared_lent = true,
            }
            // SAFETY: `ptr..ptr + len` is in this `Lent`'s arena chunk `at.place` names, or in
            // its current shared arena, written by the put before this lend, marked lent just
            // now; the lend is for `&self` (`lend`'s contract).
            let shared = matches!(at.place, Place::Shared { .. });
            return Ok(Some(Lend { tag: at.tag, namespace: at.namespace, bytes: unsafe { lend(ptr, len) }, own: true, shared }));
        }
        let Some(found) = inner.store.get_hinted(key, hint_gen).map_err(stopped("reading a record"))? else {
            inner.counts.misses += 1;
            return Ok(None);
        };
        let got = inner.store.got(found);
        if got.tag == ReservedTag::Pointer.tag() {
            // Found in the largest-first order, which may be an older value of it.
            return Err(LentError::PointerTag { key: *key });
        }
        let (ptr, len, tag, namespace) = (got.bytes.as_ptr(), got.bytes.len(), got.tag, got.namespace);
        inner.counts.from_segments += 1;
        // SAFETY: the store answers from a proved segment's mapping held by the `Driven` inside
        // this `Lent`, or from a record held uncommitted by it (only roots are put into the
        // `Driven` itself, and they are never read through a `Lent`); the lend is for `&self`
        // (`lend`'s contract).
        Ok(Some(Lend { tag, namespace, bytes: unsafe { lend(ptr, len) }, own: false, shared: false }))
    }

    /// Whether `key` is held with exactly `bytes`, in any namespace: this command's own put,
    /// else the committed record (the store is asked as a get asks it). A comparison alone: it
    /// lends nothing, so it keeps no arena chunk past its commit. A [`Lent::get`] of an own put
    /// does (`lend`'s contract): its whole chunk stays allocated to the `Lent`'s end, though
    /// after the commit every key of it is read from its segment like any other. So "is this
    /// record here already" is asked here, never by a get (blaze-cli plain_place.rs
    /// `put_record`; aligned_tests.rs shows both).
    pub fn holds(&self, key: &Key, bytes: &[u8]) -> Result<bool, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            if at.tag == ReservedTag::Pointer.tag() {
                return Err(LentError::PointerTag { key: *key });
            }
            let kept = inner.kept(&at);
            return Ok(kept == bytes);
        }
        let Some(found) = inner.store.get(key).map_err(stopped("checking whether a record is held"))? else {
            return Ok(false);
        };
        let got = inner.store.got(found);
        if got.tag == ReservedTag::Pointer.tag() {
            return Err(LentError::PointerTag { key: *key });
        }
        Ok(got.bytes == bytes)
    }

    /// `bytes` under `key` in `namespace`, copied once into the arena; written by the next
    /// commit. A key held with equal bytes is left as it is (rule 25); with other bytes it is
    /// refused (rule 21). The store is asked too (it maps at a first probe, as a get does).
    pub fn put(&self, key: &Key, namespace: Namespace, tag: Tag, bytes: &[u8]) -> Result<Put, LentError> {
        self.put_parts(key, namespace, tag, &[bytes])
    }

    /// [`Lent::put`] of the concatenation of `parts`, never joined first: each part copied
    /// once, in order, into the arena (a record's head and its payload, blaze-cli
    /// engine_store.rs). Only a refusal joins them, to name the first differing byte.
    pub fn put_parts(&self, key: &Key, namespace: Namespace, tag: Tag, parts: &[&[u8]]) -> Result<Put, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if !inner.store.core().writes().contains(&namespace) {
            return Err(LentError::NotWritable { namespace });
        }
        if tag == ReservedTag::Pointer.tag() {
            return Err(LentError::PointerTag { key: *key });
        }
        inner.counts.puts += 1;
        let total: usize = parts.iter().map(|part| part.len()).sum();
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            let kept = inner.kept(&at);
            if parts_equal(kept, parts) && at.namespace == namespace && at.tag == tag {
                inner.counts.puts_same += 1;
                return Ok(Put::Same);
            }
            let held = Held { namespace: at.namespace, tag: at.tag, bytes: kept, own: true };
            return Err(held.against(key, namespace, tag, &joined(parts, &self.alloc)));
        }
        if let Some(found) = inner.store.get(key).map_err(stopped("checking a put against the store"))? {
            let got = inner.store.got(found);
            if parts_equal(got.bytes, parts) && got.tag == tag {
                inner.counts.puts_same += 1;
                return Ok(Put::Same);
            }
            let held = Held { namespace: got.namespace, tag: got.tag, bytes: got.bytes, own: false };
            return Err(held.against(key, namespace, tag, &joined(parts, &self.alloc)));
        }
        let (place, block, len) = self.copy_in(inner, key, tag, parts)?;
        let at = PutAt { place, block, len, tag, namespace, order: inner.order.len() as u32 };
        inner.puts.insert(key.0.0, at);
        inner.order.push(key.0.0);
        inner.uncommitted_bytes += total as u64;
        Ok(Put::Written)
    }

    /// The concatenation of `parts` copied into the arena, within a chunk's capacity (`lend`'s
    /// contract), with room reserved for one more put and its place in the order:
    /// `(place, block, len)`. Under `Visibility::InArena`, into the shared arena (made at the
    /// first put after a commit), whole, with one publish (arena.rs `Arena::append`).
    fn copy_in(&self, inner: &mut Inner<D, A>, key: &Key, tag: Tag, parts: &[&[u8]]) -> Result<(Place, u32, u32), LentError> {
        let total: usize = parts.iter().map(|part| part.len()).sum();
        let need = total.div_ceil(16).max(1);
        if let Visibility::InArena { shared } = self.visibility {
            // The CRC at the put (rule 77): over the parts as they are copied in, so the commit
            // reads the record once (its layout's copy) and never computes it again.
            let crc = parts.iter().fold(record_crc(&key.0, tag, &[]), |crc, part| crc32c::extend(crc, part));
            let len = u32::try_from(total).map_err(|_| LentError::Arena { bytes: total })?;
            inner.puts.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
            inner.order.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
            if inner.shared.is_none() {
                inner.kept_arenas.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
                let arena = Arc::try_new_in(Arena::new()?, shared).map_err(|_| LentError::Arena { bytes: total })?;
                inner.shared = Some(arena);
                inner.shared_lent = false;
                inner.counts.chunks += 1;
            }
            let arena = inner.shared.as_ref().expect("made just above");
            let block = arena.append(parts, total)?;
            inner.counts.arena_bytes += (need * 16) as u64;
            inner.counts.held_bytes += (need * 16) as u64;
            inner.counts.copied += 1;
            return Ok((Place::Shared { crc }, block, len));
        }
        let chunk_at = match self.plant {
            LentPlant::OneBuffer => {
                if inner.chunks.is_empty() {
                    inner.chunks.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
                    inner.lent.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
                    inner.chunks.push(Vec::new_in(self.alloc.clone()));
                    inner.lent.push(false);
                }
                // The planted bug: one buffer, grown in place or moved.
                inner.chunks[0].try_reserve(need).map_err(|_| LentError::Arena { bytes: total })?;
                0
            }
            LentPlant::None | LentPlant::RootFirst | LentPlant::PointerFirst | LentPlant::FreesLent | LentPlant::KeepsLentKeys => {
                // A chunk that lent takes no more puts: they would be kept with it.
                let fits = inner.chunks.last().is_some_and(|chunk| chunk.capacity() - chunk.len() >= need) && inner.lent.last() == Some(&false);
                if !fits {
                    let mut chunk: Vec<Block, A> = Vec::new_in(self.alloc.clone());
                    chunk.try_reserve_exact(need.max(CHUNK_BLOCKS)).map_err(|_| LentError::Arena { bytes: total })?;
                    inner.chunks.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
                    inner.lent.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
                    inner.chunks.push(chunk);
                    inner.lent.push(false);
                    inner.counts.chunks += 1;
                }
                inner.chunks.len() - 1
            }
        };
        inner.puts.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
        inner.order.try_reserve(1).map_err(|_| LentError::Arena { bytes: total })?;
        let len = u32::try_from(total).map_err(|_| LentError::Arena { bytes: total })?;
        let chunk = &mut inner.chunks[chunk_at];
        let block = chunk.len();
        // Within the capacity reserved above: the buffer does not move (`lend`'s contract). A
        // block may take the end of one part and the start of the next.
        let mut one = Block([0; 16]);
        let mut fill = 0usize;
        for part in parts {
            let mut rest: &[u8] = part;
            while !rest.is_empty() {
                let take = (16 - fill).min(rest.len());
                one.0[fill..fill + take].copy_from_slice(&rest[..take]);
                fill += take;
                rest = &rest[take..];
                if fill == 16 {
                    chunk.push(one);
                    one = Block([0; 16]);
                    fill = 0;
                }
            }
        }
        if fill > 0 || total == 0 {
            chunk.push(one);
        }
        inner.counts.arena_bytes += (need * 16) as u64;
        inner.counts.held_bytes += (need * 16) as u64;
        inner.counts.copied += 1;
        Ok((Place::Chunk(chunk_at as u32), block as u32, len))
    }

    /// The pointer `name` set to `target`, a record this store holds under `tag` (this
    /// command's own put, or committed): written by the next commit, the SAME commit as the
    /// records put before it, so no generation names a pointer whose target it lacks (a commit
    /// made between the target's put and this one carries the target alone). Pointing a name
    /// again replaces its value: the newest commit's wins ([`Lent::pointed`]). A name this
    /// command already pointed at `target`, not yet committed, is [`Put::Same`]; every other
    /// point is written, so of two processes the later commit wins.
    pub fn point(&self, tag: Tag, name: &Key, target: &Key) -> Result<Put, LentError> {
        match self.get(target)? {
            None => return Err(LentError::PointsAtNothing { name: *name, target: *target }),
            Some(lend) if lend.tag != tag => return Err(LentError::TargetTag { name: *name, target: *target, want: tag, found: lend.tag }),
            Some(_) => {}
        }
        let now = self.newest_target(name)?;
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        inner.counts.puts += 1;
        // Pointed again before the commit: the one place in the order it holds is enough.
        let kept = inner.puts.get(&name.0.0).map(|was| was.order as usize).filter(|&at| at >= inner.committed);
        // Only a value not yet committed is known to be the newest: a committed one (this
        // process's or read from the store) may be older than another process's commit since,
        // so pointing at it again is written, and this commit's row makes it the newest.
        if kept.is_some() && now == Some(*target) {
            inner.counts.puts_same += 1;
            return Ok(Put::Same);
        }
        let (place, block, len) = self.copy_in(inner, name, ReservedTag::Pointer.tag(), &[&target.0.0])?;
        let order = kept.unwrap_or(inner.order.len());
        inner.puts.insert(name.0.0, PutAt { place, block, len, tag: ReservedTag::Pointer.tag(), namespace: Namespace::LOCAL, order: order as u32 });
        if kept.is_none() {
            inner.order.push(name.0.0);
        }
        inner.uncommitted_bytes += POINTER_LEN as u64;
        Ok(Put::Written)
    }

    /// The target the pointer `name` names, held under `tag`: `None` when no pointer of that
    /// name was ever committed (or put by this command). Its value is this command's own last
    /// [`Lent::point`], else the one in the newest committed row holding the name
    /// ([`Driven::newest`]). A target the store no longer holds is
    /// [`LentError::TargetGone`]; one of another tag [`LentError::TargetTag`].
    pub fn pointed(&self, tag: Tag, name: &Key) -> Result<Option<Key>, LentError> {
        let Some(target) = self.newest_target(name)? else { return Ok(None) };
        match self.get(&target)? {
            None => Err(LentError::TargetGone { name: *name, target }),
            Some(lend) if lend.tag != tag => Err(LentError::TargetTag { name: *name, target, want: tag, found: lend.tag }),
            Some(_) => Ok(Some(target)),
        }
    }

    /// The pointer `name`'s newest value, its target unchecked.
    fn newest_target(&self, name: &Key) -> Result<Option<Key>, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        let decode = |bytes: &[u8]| -> Result<Key, LentError> {
            let key: [u8; POINTER_LEN] = bytes.try_into().map_err(|_| LentError::PointerForm { name: *name, len: bytes.len() })?;
            Ok(Key(ContentId(key)))
        };
        if let Some(at) = inner.puts.get(&name.0.0).copied() {
            if at.tag != ReservedTag::Pointer.tag() {
                return Err(LentError::PointerTag { key: *name });
            }
            // Not committed yet: newer than any row. A committed one is read from the rows,
            // where another process's later commit may stand after it.
            if at.order as usize >= inner.committed {
                return decode(inner.kept(&at)).map(Some);
            }
        }
        let Some(found) = inner.store.newest(name).map_err(stopped("reading a pointer"))? else { return Ok(None) };
        let got = inner.store.got(found);
        if got.tag != ReservedTag::Pointer.tag() {
            return Err(LentError::PointerTag { key: *name });
        }
        decode(got.bytes).map(Some)
    }

    /// The allocator the caller handed this `Lent` (its arena's): what a caller's scratch about
    /// this store is made in, so nothing about it reaches for another heap.
    pub fn alloc(&self) -> &A {
        &self.alloc
    }

    /// Bytes put and not yet committed: the caller commits between tasks past its bound.
    pub fn uncommitted_bytes(&self) -> u64 {
        self.inner.borrow().uncommitted_bytes
    }

    /// Keeps `keys` under the root `name` from the next commit on (`Driven::root`): a running
    /// program's records, a checkout's leaf table and its last answer for each request shape.
    pub fn root(&self, name: &str, keys: &[Key]) -> Result<Key, LentError> {
        self.root_for(name, keys, super::format::NEVER)
    }

    /// The roots the generation this store holds names: `(name, record key)`.
    pub fn roots(&self) -> Vec<(String, Key), A> {
        self.inner.borrow().store.core().roots()
    }

    /// Whether the current manifest names a root called `name`, without allocating.
    pub fn has_root(&self, name: &str) -> bool {
        self.inner.borrow().store.core().root_target(name).is_some()
    }

    /// What is not yet committed, one segment per namespace, one flip (a whole store: the
    /// commit maps what is left first). From then on every put so far is read from its segment,
    /// in place; every arena chunk no get lent from is freed, and what a get lent stays lent
    /// (`lend`'s contract: its chunk is kept, holding no key).
    pub fn commit(&self) -> Result<Committed<A>, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if self.plant == LentPlant::RootFirst {
            inner.store.commit_with(&[]).map_err(stopped("committing the roots first (planted)"))?;
        }
        let upto = inner.order.len();
        if self.plant == LentPlant::None && inner.committed == upto && !inner.store.core().needs_commit() {
            let committed = inner.store.commit_records(&[], &[]).map_err(stopped("committing nothing"))?;
            inner.counts.commits += 1;
            return Ok(committed);
        }
        // The measure (rule 16): the calling thread's time in the commit, from its layout to its
        // flip and the new segment's map (the driver's wall clock: the simulation's in a test).
        let began = inner.store.driver().wall_ns();
        // THE SEAM (rule 78): under `InArena` the puts are lent to the layout where they lie,
        // no batch between (copy 2 gone, the CRC made at the put); under `AfterCommit` each is
        // copied into its namespace's batch first, as before. The same segments either way
        // (arena_tests.rs `the_two_visibilities_write_the_same_segments`).
        let (committed, batch_bytes) = if matches!(self.visibility, Visibility::InArena { .. }) {
            (self.commit_shared(inner, upto)?, 0)
        } else {
            let mut writes: Vec<Namespace, &A> = Vec::with_capacity_in(inner.store.core().writes().len(), &self.alloc);
            writes.extend_from_slice(inner.store.core().writes());
            let mut batches: Vec<Batch, &A> = Vec::with_capacity_in(writes.len(), &self.alloc);
            batches.extend(writes.iter().map(|namespace| Batch::new(*namespace)));
            let mut first = Batch::new(Namespace::LOCAL);
            for key in &inner.order[inner.committed..upto] {
                let Some(at) = inner.puts.get(key).copied() else { continue };
                let bytes = inner.kept(&at);
                if self.plant == LentPlant::PointerFirst && at.tag == ReservedTag::Pointer.tag() {
                    // The planted bug: the pointers laid apart, for a commit before their targets'.
                    first.put(Key(ContentId(*key)), at.tag, bytes).map_err(|why| LentError::Store { doing: "laying out the pointers first (planted)", why: why.into_store(at.tag) })?;
                    continue;
                }
                let Some(batch) = batches.iter_mut().find(|batch| batch.namespace() == at.namespace) else {
                    return Err(LentError::NotWritable { namespace: at.namespace });
                };
                batch.put(Key(ContentId(*key)), at.tag, bytes).map_err(|why| LentError::Store { doing: "laying out a commit", why: why.into_store(at.tag) })?;
            }
            let batch_bytes = batches.iter().map(Batch::bytes).sum::<u64>() + first.bytes();
            if !first.is_empty() {
                inner.store.commit_with(&[&first]).map_err(stopped("committing the pointers first (planted)"))?;
            }
            let mut refs: Vec<&Batch, &A> = Vec::with_capacity_in(batches.len(), &self.alloc);
            refs.extend(batches.iter().filter(|batch| !batch.is_empty()));
            (inner.store.commit_with(&refs).map_err(stopped("committing"))?, batch_bytes)
        };
        inner.committed = upto;
        inner.uncommitted_bytes = 0;
        inner.counts.batch_bytes += batch_bytes;
        inner.counts.commits += 1;
        inner.counts.commit_ns += inner.store.driver().wall_ns().saturating_sub(began);
        // Every put is in a segment now and is read from its mapping from here on: EVERY key
        // leaves `puts`, so a later get reads the segment in place whichever chunk held the put.
        // (Until 2026-11-18 the keys of a chunk that lent stayed: one get of an own put before
        // the commit, and every record put beside it was answered from the arena, refused in
        // place, to the `Lent`'s end. The first `blaze run` in a fresh home ended so.) Each
        // chunk no get lent from is freed; a chunk that lent stays to the `Lent`'s end, holding
        // no key: its lends are read until then (`lend`'s contract), and nothing new reaches it.
        let free_lent = self.plant == LentPlant::FreesLent;
        let keep_lent_keys = self.plant == LentPlant::KeepsLentKeys;
        let Inner { chunks, lent, puts, counts, shared, shared_lent, sealed, kept_arenas, .. } = inner;
        for (chunk, lent) in chunks.iter_mut().zip(lent.iter()) {
            if (!*lent || free_lent) && chunk.capacity() > 0 {
                counts.held_bytes = counts.held_bytes.saturating_sub(chunk.len() as u64 * 16);
                counts.freed_chunks += 1;
                *chunk = Vec::new_in(self.alloc.clone());
            }
        }
        // The shared arena's puts are in segments now: the next put starts a new arena. This
        // one lives on in every snapshot that holds it, and to the `Lent`'s end when a get
        // lent from it (`lend`'s contract; room reserved when it was made).
        // (`LentPlant::FreesLent` is the private arena's control alone: a shared arena that lent
        // is always kept, as its drop would free what a lend still reads.)
        if let Some(arena) = shared.take() {
            if *shared_lent {
                kept_arenas.push(arena);
            } else {
                counts.held_bytes = counts.held_bytes.saturating_sub(u64::from(arena.blocks()) * 16);
                counts.freed_chunks += 1;
            }
        }
        *shared_lent = false;
        *sealed = upto;
        puts.retain(|_, at| match at.place {
            Place::Chunk(chunk) => keep_lent_keys && chunks[chunk as usize].capacity() > 0,
            Place::Shared { .. } => false,
        });
        Ok(committed)
    }

    /// `Visibility::InArena`'s commit of the puts `order[committed..upto]`: each lent to the
    /// layout from the shared arena where it lies, with the CRC its put made (core.rs
    /// `LentRecord`, `Plain::commit_records`): its bytes read once, by the layout's one copy
    /// into the segment's piece. The list of records is in the handed allocator.
    /// (`LentPlant::PointerFirst`, planted: the pointers committed alone first, as the batch
    /// path's control does.)
    fn commit_shared(&self, inner: &mut Inner<D, A>, upto: usize) -> Result<Committed<A>, LentError> {
        let Inner { store, shared, puts, order, committed, .. } = inner;
        let from = (*committed).min(upto);
        let mut records: Vec<LentRecord<'_>, &A> = Vec::new_in(&self.alloc);
        records.try_reserve_exact(upto - from).map_err(|_| LentError::Arena { bytes: (upto - from) * core::mem::size_of::<LentRecord<'_>>() })?;
        for key in &order[from..upto] {
            let Some(at) = puts.get(key) else { continue };
            let Place::Shared { crc } = at.place else {
                // Under `InArena` every own put is the shared arena's (`copy_in`); a chunk's here
                // is this file's bug, named, never laid out from another place (rule 3).
                panic!("lent.rs commit_shared: an own put of the private arena under Visibility::InArena (copy_in places every put in the shared arena)");
            };
            let bytes = shared
                .as_ref()
                .and_then(|arena| arena.bytes(u32::MAX, at.block, at.len))
                .expect("an own put in the shared arena lies in the current arena, published whole (lent.rs copy_in)");
            records.push(LentRecord { key: Key(ContentId(*key)), tag: at.tag, namespace: at.namespace, crc, bytes });
        }
        if self.plant == LentPlant::PointerFirst && records.iter().any(|it| it.tag == ReservedTag::Pointer.tag()) {
            // The planted bug: the pointers laid apart, for a commit before their targets'.
            let mut first: Vec<LentRecord<'_>, &A> = Vec::new_in(&self.alloc);
            first.extend(records.iter().filter(|it| it.tag == ReservedTag::Pointer.tag()).copied());
            records.retain(|it| it.tag != ReservedTag::Pointer.tag());
            store.commit_records(&[], &first).map_err(stopped("committing the pointers first (planted)"))?;
        }
        store.commit_records(&[], &records).map_err(stopped("committing"))
    }

    /// `Visibility::InArena`: the puts since the last take sealed into one run of the current
    /// shared arena (sorted by key; arena.rs `Arena::seal`), and the view a snapshot takes of
    /// it: every run and block published now. `None`: no shared arena (no put since the last
    /// commit, or `Visibility::AfterCommit`). A pointer pointed again after its name was sealed
    /// keeps its sealed entry: a snapshot never reads a pointer (`SnapshotError::PointerTag`).
    pub(super) fn seal_run(&self) -> Result<Option<ArenaView>, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        let Some(arena) = inner.shared.as_ref() else { return Ok(None) };
        let from = inner.sealed.max(inner.committed);
        let mut run: Vec<RunEntry, &A> = Vec::new_in(&self.alloc);
        let count = inner.order.len().saturating_sub(from);
        run.try_reserve_exact(count).map_err(|_| LentError::Arena { bytes: count * core::mem::size_of::<RunEntry>() })?;
        for key in &inner.order[from.min(inner.order.len())..] {
            let Some(at) = inner.puts.get(key) else { continue };
            if matches!(at.place, Place::Shared { .. }) {
                run.push(RunEntry { key: *key, block: at.block, len: at.len, tag: at.tag, namespace: at.namespace });
            }
        }
        // Sorted by key: a run is searched by bisection, and the commit's layout merges runs.
        run.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        arena.seal(&run)?;
        inner.sealed = inner.order.len();
        Ok(Some(ArenaView::of(arena)))
    }

    /// Every segment mapped and every parked one checked (probe.rs): asked once, when the
    /// request's first pool task is about to start.
    pub fn map_all(&self) -> Result<(), LentError> {
        self.inner.borrow_mut().store.map_all().map_err(stopped("mapping every segment named"))
    }

    /// After the answer: the exit tail sealed (`Driven::seal`). Whether a flip was made.
    pub fn seal(&self) -> Result<bool, LentError> {
        self.inner.borrow_mut().store.seal().map_err(stopped("sealing the exit tail"))
    }

    /// Foreground exit-tail seal at the calling thread's current priority (`Driven::seal_foreground`).
    pub fn seal_foreground(&self) -> Result<bool, LentError> {
        self.inner.borrow_mut().store.seal_foreground().map_err(stopped("sealing the exit tail"))
    }

    /// After the answer: at most `slices` compaction slices (`Driven::compact`).
    pub fn compact(&self, slices: u32) -> Result<Vec<Sliced, A>, LentError> {
        self.inner.borrow_mut().store.compact(slices).map_err(stopped("compacting"))
    }

    /// Foreground compaction at the calling thread's current priority (`Driven::compact_foreground`).
    pub fn compact_foreground(&self, slices: u32) -> Result<Vec<Sliced, A>, LentError> {
        self.inner.borrow_mut().store.compact_foreground(slices).map_err(stopped("compacting"))
    }

    /// After the answer, before its commit: one cleanup slice written with no lock, which the
    /// next [`Lent::commit`]'s flip names (`Driven::fold_slice`). Whether one is parked.
    pub fn fold_slice(&self) -> Result<bool, LentError> {
        self.inner.borrow_mut().store.fold_slice().map_err(stopped("writing a cleanup slice"))
    }

    /// The last slice's outcome, once: a folded one's after the commit that named it.
    pub fn take_sliced(&self) -> Option<Sliced> {
        self.inner.borrow_mut().store.take_sliced()
    }

    /// After the answer's commit, whose flip named the cleanup's first slice (`first`, its
    /// outcome): more slices, each folded and named by a commit of nothing else, while
    /// `Plain::cleanup_goes_on` says so (the last was a tiered merge that replaced something, or in sweep mode a merge is due next after a sweep or after a first slice the lease left empty, at most
    /// `CLEANUP_SLICES_MOST`, the bytes read under this process's work budget). The slices
    /// summed (`Sliced::and`) and how many replaced something.
    pub fn clean_on(&self, first: Option<Sliced>) -> Result<(Option<Sliced>, u32), LentError> {
        let Some(mut so_far) = first else { return Ok((None, 0)) };
        let (mut last, mut slices) = (so_far, u32::from(so_far.segments > 0 && !so_far.aborted));
        while self.inner.borrow().store.core().cleanup_goes_on(&so_far, slices, &last) {
            if !self.fold_slice()? {
                break;
            }
            self.commit()?;
            let Some(next) = self.take_sliced() else { break };
            slices += u32::from(next.segments > 0 && !next.aborted);
            so_far = so_far.and(next);
            last = next;
        }
        Ok((Some(so_far), slices))
    }

    /// [`Lent::root`], kept for `life_ns` from the commit that sets it (`Plain::root_for`).
    pub fn root_for(&self, name: &str, keys: &[Key], life_ns: u64) -> Result<Key, LentError> {
        let mut inner = self.inner.borrow_mut();
        let Inner { store, puts, .. } = &mut *inner;
        // This command's puts are in its arena, not yet the store's: D200's set-time guard
        // takes them as put (`Driven::root_names_held`).
        store.root_for_owned(name, keys, life_ns, &|key| puts.get(&key.0.0).is_some()).map_err(stopped("setting a root"))
    }

    /// An index of `keys` for the next commit (`Plain::index`): its key, which an indexed root
    /// names ([`Lent::root_through`]).
    pub fn index(&self, keys: &[Key]) -> Result<Key, LentError> {
        self.inner.borrow_mut().store.index(keys).map_err(stopped("putting an index"))
    }

    /// The root `name` naming the indexes `indexes` from the next commit on, for `life_ns`
    /// (`Plain::root_through`): reach keeps each index and every key it holds.
    pub fn root_through(&self, name: &str, indexes: &[Key], life_ns: u64) -> Result<Key, LentError> {
        let mut inner = self.inner.borrow_mut();
        let Inner { store, puts, .. } = &mut *inner;
        store.root_through_owned(name, indexes, life_ns, &|key| puts.get(&key.0.0).is_some()).map_err(stopped("setting an indexed root"))
    }

    /// An index of `keys` filed beside the record `owner` (`Plain::index_beside`).
    pub fn index_beside(&self, owner: Key, keys: &[Key]) -> Result<Key, LentError> {
        self.inner.borrow_mut().store.index_beside(owner, keys).map_err(stopped("putting an index beside a record"))
    }

    /// Every root the generation this store holds names, with its expiry (wall ns): the
    /// command's cleanup line counts the unexpired ones (`Plain::roots_expiring`).
    pub fn roots_expiring(&self) -> Vec<(String, Key, u64), A> {
        self.inner.borrow().store.core().roots_expiring()
    }

    /// The leases the generation this store holds names, `(holder, expires wall ns)`
    /// (`Plain::leases`).
    pub fn leases(&self) -> Vec<(u64, u64), A> {
        self.inner.borrow().store.core().leases()
    }

    /// Number of active leases named by the current manifest, without allocating.
    pub fn leases_len(&self) -> usize {
        self.inner.borrow().store.core().leases_len()
    }

    /// Whether [`Lent::commit`] has any uncommitted puts, pending roots, open stamps, or a
    /// folded slice to write/flip.
    pub fn needs_commit(&self) -> bool {
        let inner = self.inner.borrow();
        inner.committed < inner.order.len() || inner.store.core().needs_commit()
    }

    /// Whether [`Lent::seal`] has any unsealed segments of this boot or pending open stamps.
    pub fn needs_seal(&self) -> bool {
        self.inner.borrow().store.core().needs_seal()
    }

    /// Records this process's commits carried forward (`Plain::carried`).
    pub fn carried(&self) -> u64 {
        self.inner.borrow().store.core().carried()
    }

    /// The record the root `name` names unexpired (`Plain::root_named`).
    pub fn root_named(&self, name: &str) -> Option<Key> {
        self.inner.borrow().store.core().root_named(name)
    }

    /// O(1) lease renewal for an already-live root `name` ([`Plain::renew_root`]): refreshes the
    /// lease without reading, probing, sorting, or hashing the keys it holds.
    pub fn renew_root(&self, name: &str, life_ns: u64) -> Renewed {
        self.inner.borrow_mut().store.renew_root(name, life_ns)
    }

    /// O(segments + roots + retired) check of whether compaction or GC housekeeping is due at
    /// `now_ns` ([`Plain::needs_housekeeping`]).
    pub fn needs_housekeeping(&self, now_ns: u64) -> bool {
        self.inner.borrow().store.needs_housekeeping(now_ns)
    }

    /// Read-only inspection of what a GC pass would reclaim now ([`Driven::plan_gc`]).
    pub fn plan_gc(&self) -> Result<GcPlan, LentError> {
        self.inner.borrow_mut().store.plan_gc().map_err(stopped("planning gc"))
    }

    /// Whether total committed segment bytes exceed the high watermark of a finite budget.
    pub fn over_budget(&self) -> bool {
        self.inner.borrow().store.over_budget()
    }

    /// Total committed segment bytes.
    pub fn total_bytes(&self) -> u64 {
        self.inner.borrow().store.total_bytes()
    }

    /// Configured store byte budget (`u64::MAX` = unlimited).
    pub fn cap_bytes(&self) -> u64 {
        self.inner.borrow().store.cap_bytes()
    }

    /// Overrides the store byte budget for this handle (`u64::MAX` = unlimited).
    pub fn set_cap(&self, cap_bytes: u64) {
        self.inner.borrow_mut().store.set_cap(cap_bytes);
    }

    /// The roots this process set and the keys they name (`Plain::named`).
    pub fn named(&self) -> Named {
        self.inner.borrow().store.core().named()
    }

    /// This command's lease let go of at the next commit: its answer roots its records
    /// (`Plain::release_lease`).
    pub fn release_lease(&self) {
        self.inner.borrow_mut().store.release_lease()
    }

    /// Drops a named root at the next commit (`Plain::unroot`).
    pub fn unroot(&self, name: &str) {
        self.inner.borrow_mut().store.unroot(name)
    }

    /// Re-enables automatic lease acquisition for the next request after [`Lent::release_lease`]
    /// (`Plain::rearm_lease`), so a resident daemon holds zero leases while idle yet acquires
    /// fresh lease protection on the next multi-phase request.
    pub fn rearm_lease(&self) {
        self.inner.borrow_mut().store.rearm_lease()
    }

    /// The generation this open store holds (`Plain::generation`).
    pub fn generation(&self) -> u64 {
        self.inner.borrow().store.core().generation()
    }

    /// Reads the reset-safe [`HeadIdentity`] of `head` on disk (`Driven::head_identity_on_disk`,
    /// bounded to `HEAD_LEN + 1` bytes plus path-based `Stat` `(dev, ino)`).
    pub fn head_identity_on_disk(&self) -> Result<Option<HeadIdentity>, LentError> {
        self.inner.borrow_mut().store.head_identity_on_disk().map_err(stopped("reading the head identity"))
    }

    /// Reads only the fixed head file on disk (`Driven::head_generation_on_disk`, zero segment
    /// opens or mmaps) and returns the committed generation on disk.
    pub fn head_generation_on_disk(&self) -> Result<u64, LentError> {
        self.inner.borrow_mut().store.head_generation_on_disk().map_err(stopped("reading the head generation"))
    }

    /// Reclaims arenas and lent private chunks held past commit once all [`Lend`] borrows have
    /// ended (proven statically by `&mut self`) and no uncommitted puts remain. Mapped segments
    /// stay open and warm inside the underlying store. Shared arenas still retained by a live
    /// [`Snapshot`] are kept until their last owner drops so `held_bytes` stays accurate.
    pub fn reclaim_between_requests(&mut self) -> usize {
        let inner = self.inner.get_mut();
        if inner.uncommitted_bytes > 0 || inner.committed < inner.order.len() || !inner.puts.is_empty() {
            return 0;
        }
        let mut reclaimed = 0usize;
        let mut i = 0usize;
        while i < inner.kept_arenas.len() {
            let arena = inner.kept_arenas.swap_remove(i);
            match Arc::try_unwrap(arena) {
                Ok(owned) => {
                    inner.counts.held_bytes = inner.counts.held_bytes.saturating_sub(u64::from(owned.blocks()) * 16);
                    inner.counts.freed_chunks = inner.counts.freed_chunks.saturating_add(1);
                    reclaimed += 1;
                }
                Err(shared) => {
                    inner.kept_arenas.push(shared);
                    let last = inner.kept_arenas.len() - 1;
                    inner.kept_arenas.swap(i, last);
                    i += 1;
                }
            }
        }
        if inner.kept_arenas.capacity() > 16 && inner.kept_arenas.len() <= 16 {
            inner.kept_arenas.shrink_to_fit();
        }
        if let Some(arena) = inner.shared.take() {
            inner.shared_lent = false;
            match Arc::try_unwrap(arena) {
                Ok(owned) => {
                    inner.counts.held_bytes = inner.counts.held_bytes.saturating_sub(u64::from(owned.blocks()) * 16);
                    inner.counts.freed_chunks = inner.counts.freed_chunks.saturating_add(1);
                    reclaimed += 1;
                }
                Err(shared) => inner.kept_arenas.push(shared),
            }
        }
        while let Some(chunk) = inner.chunks.pop() {
            if chunk.capacity() > 0 {
                let bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX).saturating_mul(16);
                inner.counts.held_bytes = inner.counts.held_bytes.saturating_sub(bytes);
                inner.counts.freed_chunks = inner.counts.freed_chunks.saturating_add(1);
                reclaimed += 1;
            }
        }
        inner.lent.clear();
        if inner.chunks.capacity() > 16 {
            inner.chunks.shrink_to_fit();
            inner.lent.shrink_to_fit();
        }
        inner.order.clear();
        if inner.order.capacity() > 1024 {
            inner.order.shrink_to_fit();
        }
        inner.committed = 0;
        inner.sealed = 0;
        reclaimed
    }

    /// Adds what names this machine to the scan of every put into a shared namespace.
    pub fn machine(&self, paths: &[&std::path::Path]) {
        self.inner.borrow_mut().store.machine(paths);
    }

    /// `(segments mapped, segments named)`: `--stats` "segments mapped N of M named".
    pub fn mapped_of_named(&self) -> (u32, u32) {
        self.inner.borrow().store.core().mapped_of_named()
    }

    /// The most segments the generation may name once cleanup has run (`Plain::named_most`):
    /// what [`Lent::mapped_of_named`]'s named count is held against.
    pub fn named_most(&self) -> u32 {
        self.inner.borrow().store.core().named_most()
    }

    /// Probes per get: `--stats` "probes per get (mean, most)".
    pub fn probes(&self) -> Probes {
        self.inner.borrow().store.core().probes()
    }

    pub fn counts(&self) -> LentCounts {
        let inner = self.inner.borrow();
        LentCounts { carried: inner.store.core().carried(), ..inner.counts }
    }

    /// The put path's tally so far ([`PutTally`]): this `Lent`'s copies and commits, and the
    /// plain store's disk side.
    pub fn put_tally(&self) -> PutTally {
        let inner = self.inner.borrow();
        let counts = &inner.counts;
        PutTally { copied: counts.copied, arena_bytes: counts.arena_bytes, batch_bytes: counts.batch_bytes, commits: counts.commits, commit_ns: counts.commit_ns, disk: inner.store.core().put_path() }
    }

    /// The open's report (and losses found at first probes since), for the profile.
    pub fn report_with<T>(&self, read: impl FnOnce(&Report) -> T) -> T {
        read(self.inner.borrow().store.report())
    }

    /// The store lent to `read` by shared reference for the call alone: what a snapshot
    /// (snapshot.rs, `Lent::snapshot_in`) is taken from. Never `&mut`: `read` cannot replace the
    /// store or drop a mapping (the module's docs), and nothing it gets outlives the call.
    pub(crate) fn store_with<T>(&self, read: impl FnOnce(&Driven<D, A>) -> T) -> T {
        read(&self.inner.borrow().store)
    }

    /// A test's power cut on the store inside (`Driven::cut`).
    #[cfg(test)]
    pub(crate) fn cut(&self, at: Option<super::driven::Cut>) {
        self.inner.borrow_mut().store.cut(at);
    }

    /// The store back, every lend ended (by value: no borrow of `self` is alive).
    pub fn into_store(self) -> Driven<D, A> {
        self.inner.into_inner().store
    }
}

#[cfg(test)]
#[path = "lent_tests.rs"]
mod tests;
