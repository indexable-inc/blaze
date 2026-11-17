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
use blaze_hash::ByHash;
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_keyed::store::{Put, StoreError};

use super::core::{Batch, Committed, Probes, Report, Sliced};
use super::driven::Driven;
use crate::Key;

/// One arena chunk's blocks (1 MiB); a larger record gets a chunk of its own size.
const CHUNK_BLOCKS: usize = 1 << 16;

/// Why a [`Lent`] refused, by name (rule 19).
#[derive(Clone, Debug)]
pub enum LentError {
    /// The plain store stopped while `doing`: its own typed error.
    Store { doing: &'static str, why: StoreError },
    /// Two values under one key (rule 21): the key misses an input of its value.
    TwoValues { key: Key },
    /// The arena's allocator refused a record of this many bytes.
    Arena { bytes: usize },
    /// A namespace this store does not write (a trusted builder's).
    NotWritable { namespace: Namespace },
}

impl fmt::Display for LentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LentError::Store { doing, why } => write!(f, "the plain store stopped while {doing}: {why}"),
            LentError::TwoValues { key } => write!(f, "two values under one key {:?} (rule 21: the key misses an input of its value)", key.0),
            LentError::Arena { bytes } => write!(f, "this command's records could not take {bytes} more bytes: the allocator refused"),
            LentError::NotWritable { namespace } => write!(f, "namespace {} is a trusted builder's; this machine writes only its own", namespace.0),
        }
    }
}

fn stopped(doing: &'static str) -> impl FnOnce(StoreError) -> LentError {
    move |why| LentError::Store { doing, why }
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
}

struct Inner<D: Driver, A: Allocator + Clone> {
    store: Driven<D>,
    /// Each chunk made at its capacity and never pushed past it: its buffer never moves.
    chunks: Vec<Vec<Block, A>, A>,
    puts: ByHash<[u8; 32], PutAt, A>,
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
            puts: ByHash::new_in(alloc.clone()),
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
        inner.counts.puts += 1;
        if let Some(at) = inner.puts.get(&key.0.0).copied() {
            let kept = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
            if kept == bytes && at.namespace == namespace && at.tag == tag {
                inner.counts.puts_same += 1;
                return Ok(Put::Same);
            }
            return Err(LentError::TwoValues { key: *key });
        }
        if let Some(found) = inner.store.get(key).map_err(stopped("checking a put against the store"))? {
            let got = inner.store.got(found);
            if got.bytes == bytes && got.tag == tag {
                inner.counts.puts_same += 1;
                return Ok(Put::Same);
            }
            return Err(LentError::TwoValues { key: *key });
        }
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
            LentPlant::None | LentPlant::RootFirst => {
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
        let at = PutAt { chunk: chunk_at as u32, block: block as u32, len, tag, namespace };
        inner.puts.insert(key.0.0, at);
        inner.order.push(key.0.0);
        inner.uncommitted_bytes += bytes.len() as u64;
        inner.counts.arena_bytes += (need * 16) as u64;
        Ok(Put::Written)
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
        for key in &inner.order[inner.committed..upto] {
            let Some(at) = inner.puts.get(key).copied() else { continue };
            let bytes = block_bytes(&inner.chunks[at.chunk as usize][at.block as usize..], at.len as usize);
            let Some(batch) = batches.iter_mut().find(|batch| batch.namespace() == at.namespace) else {
                return Err(LentError::NotWritable { namespace: at.namespace });
            };
            batch.put(Key(ContentId(*key)), at.tag, bytes).map_err(|why| LentError::Store { doing: "laying out a commit", why: why.into_store(at.tag) })?;
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
