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
use std::alloc::{Allocator, Global};

use blaze_core::Driver;
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_keyed::store::{Put, StoreError};

use super::core::{Batch, Committed, POINTER_LEN, POINTER_TAG, Probes, Report, Sliced};
use super::driven::Driven;
use crate::Key;

/// One arena chunk's blocks (1 MiB); a larger record gets a chunk of its own size.
const CHUNK_BLOCKS: usize = 1 << 16;

/// Why a [`Lent`] refused, by name (rule 19).
#[derive(Clone, Debug)]
pub enum LentError {
    /// The plain store stopped while `doing`: its own typed error.
    Store { doing: &'static str, why: StoreError },
    /// Two values under one key (rule 21): the key misses an input of its value. The held
    /// record (`held_own`: this command's own put, else a committed one) and the refused put,
    /// each by namespace, tag and length, and the offset of their first differing byte (`None`
    /// when the bytes are equal and the tag or namespace is not; the shorter's length when one
    /// is a prefix of the other).
    TwoValues { key: Key, namespace: Namespace, tag: Tag, len: usize, held_namespace: Namespace, held_tag: Tag, held_len: usize, held_own: bool, differs_at: Option<usize> },
    /// The arena's allocator refused a record of this many bytes.
    Arena { bytes: usize },
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
            LentError::TwoValues { key, namespace, tag, len, held_namespace, held_tag, held_len, held_own, differs_at } => {
                let held = if *held_own { "this command's" } else { "committed" };
                write!(
                    f,
                    "two values under one key {:?} (rule 21: the key misses an input of its value): {held} record of {held_len} bytes (namespace {}, tag {}), put of {len} bytes (namespace {}, tag {})",
                    key.0, held_namespace.0, held_tag.0, namespace.0, tag.0
                )?;
                match differs_at {
                    Some(at) => write!(f, ", first differing byte at {at}"),
                    None => f.write_str(", bytes equal"),
                }
            }
            LentError::Arena { bytes } => write!(f, "this command's records could not take {bytes} more bytes: the allocator refused"),
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
        LentError::TwoValues {
            key: *key,
            namespace,
            tag,
            len: bytes.len(),
            held_namespace: self.namespace,
            held_tag: self.tag,
            held_len: self.bytes.len(),
            held_own: self.own,
            differs_at: first_difference(self.bytes, bytes),
        }
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

/// A record lent in place for `'l`, a borrow of the [`Lent`].
#[derive(Clone, Copy, Debug)]
pub struct Lend<'l> {
    pub tag: Tag,
    pub namespace: Namespace,
    pub bytes: &'l [u8],
    /// This command's own put (from the arena), not a committed record.
    pub own: bool,
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
    pub arena_bytes: u64,
    pub chunks: u32,
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
}

/// Sixteen bytes at sixteen-byte alignment: a put's payload starts as a segment's does.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

#[derive(Clone, Copy)]
struct PutAt {
    chunk: u32,
    /// In blocks.
    block: u32,
    len: u32,
    tag: Tag,
    namespace: Namespace,
    /// Its place in `Inner::order`: a pointer pointed again before the commit keeps it, one
    /// pointed again after it takes a new one.
    order: u32,
}

struct Inner<D: Driver, A: Allocator + Clone> {
    store: Driven<D>,
    /// Each chunk made at its capacity and never pushed past it: its buffer never moves.
    chunks: Vec<Vec<Block, A>, A>,
    puts: Map<[u8; 32], PutAt, BuildKeyHasher, A>,
    /// The puts in the order they came: a commit lays out `order[committed..]`.
    order: Vec<[u8; 32], A>,
    committed: usize,
    uncommitted_bytes: u64,
    counts: LentCounts,
}

/// The command's one owner of every byte a get lends: see the module's docs.
pub struct Lent<D: Driver, A: Allocator + Clone> {
    inner: RefCell<Inner<D, A>>,
    alloc: A,
    plant: LentPlant,
}

/// `len` bytes at `ptr`, lent for `'l`.
///
/// # Safety
///
/// The one assumption the lend rests on. `ptr..ptr + len` lies in a segment mapping held by
/// the [`Driven`] inside a [`Lent`], or in one of that `Lent`'s arena chunks. The store never
/// unmaps a segment while it is open; a retired file may be unlinked and its mapping stands.
/// A chunk is never moved (made at its capacity, pushed only within it, never freed before the
/// `Lent`). The caller borrows that `Lent` for `'l`, so it lives, with nothing of it dropped,
/// for `'l`; no `&mut` to those bytes is ever made (a mapping is read-only, a block is written
/// once, before its first lend).
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
    pub fn new(store: Driven<D>, alloc: A) -> Lent<D, A> {
        Lent::planted(store, alloc, LentPlant::None)
    }

    pub(crate) fn planted(store: Driven<D>, alloc: A, plant: LentPlant) -> Lent<D, A> {
        let inner = Inner {
            store,
            chunks: Vec::new_in(alloc.clone()),
            puts: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: alloc.clone(), capacity: None }),
            order: Vec::new_in(alloc.clone()),
            committed: 0,
            uncommitted_bytes: 0,
            counts: LentCounts::default(),
        };
        Lent { inner: RefCell::new(inner), alloc, plant }
    }

    /// The record under `key`, in place, for the life of this borrow: this command's own put,
    /// else the committed record of the first namespace read that holds it (a segment mapped
    /// at its first probe, `Driven::get`). `None`: absent.
    pub fn get(&self, key: &Key) -> Result<Option<Lend<'_>>, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        inner.counts.gets += 1;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            if at.tag == POINTER_TAG {
                return Err(LentError::PointerTag { key: *key });
            }
            let bytes = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
            let (ptr, len) = (bytes.as_ptr(), bytes.len());
            inner.counts.from_puts += 1;
            // SAFETY: `ptr..ptr + len` is in this `Lent`'s arena chunk `at.chunk`, written by
            // the put before this lend; the lend is for `&self` (`lend`'s contract).
            return Ok(Some(Lend { tag: at.tag, namespace: at.namespace, bytes: unsafe { lend(ptr, len) }, own: true }));
        }
        let Some(found) = inner.store.get(key).map_err(stopped("reading a record"))? else {
            inner.counts.misses += 1;
            return Ok(None);
        };
        let got = inner.store.got(found);
        if got.tag == POINTER_TAG {
            // Found in the largest-first order, which may be an older value of it.
            return Err(LentError::PointerTag { key: *key });
        }
        let (ptr, len, tag, namespace) = (got.bytes.as_ptr(), got.bytes.len(), got.tag, got.namespace);
        inner.counts.from_segments += 1;
        // SAFETY: the store answers from a proved segment's mapping held by the `Driven` inside
        // this `Lent`, or from a record held uncommitted by it (only roots are put into the
        // `Driven` itself, and they are never read through a `Lent`); the lend is for `&self`
        // (`lend`'s contract).
        Ok(Some(Lend { tag, namespace, bytes: unsafe { lend(ptr, len) }, own: false }))
    }

    /// `bytes` under `key` in `namespace`, copied once into the arena; written by the next
    /// commit. A key held with equal bytes is left as it is (rule 25); with other bytes it is
    /// refused (rule 21). The store is asked too (it maps at a first probe, as a get does).
    pub fn put(&self, key: &Key, namespace: Namespace, tag: Tag, bytes: &[u8]) -> Result<Put, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if !inner.store.core().writes().contains(&namespace) {
            return Err(LentError::NotWritable { namespace });
        }
        if tag == POINTER_TAG {
            return Err(LentError::PointerTag { key: *key });
        }
        inner.counts.puts += 1;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            let kept = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
            if kept == bytes && at.namespace == namespace && at.tag == tag {
                inner.counts.puts_same += 1;
                return Ok(Put::Same);
            }
            let held = Held { namespace: at.namespace, tag: at.tag, bytes: kept, own: true };
            return Err(held.against(key, namespace, tag, bytes));
        }
        if let Some(found) = inner.store.get(key).map_err(stopped("checking a put against the store"))? {
            let got = inner.store.got(found);
            if got.bytes == bytes && got.tag == tag {
                inner.counts.puts_same += 1;
                return Ok(Put::Same);
            }
            let held = Held { namespace: got.namespace, tag: got.tag, bytes: got.bytes, own: false };
            return Err(held.against(key, namespace, tag, bytes));
        }
        let (chunk, block, len) = self.copy_in(inner, bytes)?;
        let at = PutAt { chunk, block, len, tag, namespace, order: inner.order.len() as u32 };
        inner.puts.insert(key.0.0, at);
        inner.order.push(key.0.0);
        inner.uncommitted_bytes += bytes.len() as u64;
        Ok(Put::Written)
    }

    /// `bytes` copied into the arena, within a chunk's capacity (`lend`'s contract), with room
    /// reserved for one more put and its place in the order: `(chunk, block, len)`.
    fn copy_in(&self, inner: &mut Inner<D, A>, bytes: &[u8]) -> Result<(u32, u32, u32), LentError> {
        let need = bytes.len().div_ceil(16).max(1);
        let chunk_at = match self.plant {
            LentPlant::OneBuffer => {
                if inner.chunks.is_empty() {
                    inner.chunks.try_reserve(1).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
                    inner.chunks.push(Vec::new_in(self.alloc.clone()));
                }
                // The planted bug: one buffer, grown in place or moved.
                inner.chunks[0].try_reserve(need).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
                0
            }
            LentPlant::None | LentPlant::RootFirst | LentPlant::PointerFirst => {
                let fits = inner.chunks.last().is_some_and(|chunk| chunk.capacity() - chunk.len() >= need);
                if !fits {
                    let mut chunk: Vec<Block, A> = Vec::new_in(self.alloc.clone());
                    chunk.try_reserve_exact(need.max(CHUNK_BLOCKS)).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
                    inner.chunks.try_reserve(1).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
                    inner.chunks.push(chunk);
                    inner.counts.chunks += 1;
                }
                inner.chunks.len() - 1
            }
        };
        inner.puts.try_reserve(1).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
        inner.order.try_reserve(1).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
        let len = u32::try_from(bytes.len()).map_err(|_| LentError::Arena { bytes: bytes.len() })?;
        let chunk = &mut inner.chunks[chunk_at];
        let block = chunk.len();
        // Within the capacity reserved above: the buffer does not move (`lend`'s contract).
        if bytes.is_empty() {
            chunk.push(Block([0; 16]));
        }
        for piece in bytes.chunks(16) {
            let mut one = Block([0; 16]);
            one.0[..piece.len()].copy_from_slice(piece);
            chunk.push(one);
        }
        inner.counts.arena_bytes += (need * 16) as u64;
        Ok((chunk_at as u32, block as u32, len))
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
        let (chunk, block, len) = self.copy_in(inner, &target.0.0)?;
        let order = kept.unwrap_or(inner.order.len());
        inner.puts.insert(name.0.0, PutAt { chunk, block, len, tag: POINTER_TAG, namespace: Namespace::LOCAL, order: order as u32 });
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
            if at.tag != POINTER_TAG {
                return Err(LentError::PointerTag { key: *name });
            }
            // Not committed yet: newer than any row. A committed one is read from the rows,
            // where another process's later commit may stand after it.
            if at.order as usize >= inner.committed {
                return decode(block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize)).map(Some);
            }
        }
        let Some(found) = inner.store.newest(name).map_err(stopped("reading a pointer"))? else { return Ok(None) };
        let got = inner.store.got(found);
        if got.tag != POINTER_TAG {
            return Err(LentError::PointerTag { key: *name });
        }
        decode(got.bytes).map(Some)
    }

    /// Bytes put and not yet committed: the caller commits between tasks past its bound.
    pub fn uncommitted_bytes(&self) -> u64 {
        self.inner.borrow().uncommitted_bytes
    }

    /// Keeps `keys` under the root `name` from the next commit on (`Driven::root`): a running
    /// program's records, a checkout's leaf table and its last answer for each request shape.
    pub fn root(&self, name: &str, keys: &[Key]) -> Result<Key, LentError> {
        self.inner.borrow_mut().store.root(name, keys).map_err(stopped("setting a root"))
    }

    /// The roots the generation this store holds names: `(name, record key)`.
    pub fn roots(&self) -> Vec<(String, Key)> {
        self.inner.borrow().store.core().roots()
    }

    /// What is not yet committed, one segment per namespace, one flip (a whole store: the
    /// commit maps what is left first). The arena keeps every byte: what it lent stays lent.
    pub fn commit(&self) -> Result<Committed, LentError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if self.plant == LentPlant::RootFirst {
            inner.store.commit_with(&[]).map_err(stopped("committing the roots first (planted)"))?;
        }
        let upto = inner.order.len();
        let mut writes: Vec<Namespace, Global> = Vec::with_capacity_in(inner.store.core().writes().len(), Global);
        writes.extend_from_slice(inner.store.core().writes());
        let mut batches: Vec<Batch, Global> = Vec::with_capacity_in(writes.len(), Global);
        batches.extend(writes.iter().map(|namespace| Batch::new(*namespace)));
        let mut first = Batch::new(Namespace::LOCAL);
        for key in &inner.order[inner.committed..upto] {
            let Some(at) = inner.puts.get(key).copied() else { continue };
            let bytes = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
            if self.plant == LentPlant::PointerFirst && at.tag == POINTER_TAG {
                // The planted bug: the pointers laid apart, for a commit before their targets'.
                first.put(Key(ContentId(*key)), at.tag, bytes).map_err(|why| LentError::Store { doing: "laying out the pointers first (planted)", why: why.into_store(at.tag) })?;
                continue;
            }
            let Some(batch) = batches.iter_mut().find(|batch| batch.namespace() == at.namespace) else {
                return Err(LentError::NotWritable { namespace: at.namespace });
            };
            batch.put(Key(ContentId(*key)), at.tag, bytes).map_err(|why| LentError::Store { doing: "laying out a commit", why: why.into_store(at.tag) })?;
        }
        if !first.is_empty() {
            inner.store.commit_with(&[&first]).map_err(stopped("committing the pointers first (planted)"))?;
        }
        let mut refs: Vec<&Batch, Global> = Vec::with_capacity_in(batches.len(), Global);
        refs.extend(batches.iter().filter(|batch| !batch.is_empty()));
        let committed = inner.store.commit_with(&refs).map_err(stopped("committing"))?;
        inner.committed = upto;
        inner.uncommitted_bytes = 0;
        Ok(committed)
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

    /// After the answer: at most `slices` compaction slices (`Driven::compact`).
    pub fn compact(&self, slices: u32) -> Result<Vec<Sliced>, LentError> {
        self.inner.borrow_mut().store.compact(slices).map_err(stopped("compacting"))
    }

    /// Adds what names this machine to the scan of every put into a shared namespace.
    pub fn machine(&self, paths: &[&std::path::Path]) {
        self.inner.borrow_mut().store.machine(paths);
    }

    /// `(segments mapped, segments named)`: `--stats` "segments mapped N of M named".
    pub fn mapped_of_named(&self) -> (u32, u32) {
        self.inner.borrow().store.core().mapped_of_named()
    }

    /// Probes per get: `--stats` "probes per get (mean, most)".
    pub fn probes(&self) -> Probes {
        self.inner.borrow().store.core().probes()
    }

    pub fn counts(&self) -> LentCounts {
        self.inner.borrow().counts
    }

    /// The open's report (and losses found at first probes since), for the profile.
    pub fn report_with<T>(&self, read: impl FnOnce(&Report) -> T) -> T {
        read(self.inner.borrow().store.report())
    }

    /// A test's power cut on the store inside (`Driven::cut`).
    #[cfg(test)]
    pub(crate) fn cut(&self, at: Option<super::driven::Cut>) {
        self.inner.borrow_mut().store.cut(at);
    }

    /// The store back, every lend ended (by value: no borrow of `self` is alive).
    pub fn into_store(self) -> Driven<D> {
        self.inner.into_inner().store
    }
}

#[cfg(test)]
#[path = "lent_tests.rs"]
mod tests;
