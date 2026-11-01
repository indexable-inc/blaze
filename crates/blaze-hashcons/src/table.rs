//! The interner (docs/v2/plan/design/concurrency.md): [`blaze_ids::shard_count`] shards, each
//! an append-only arena of nodes in chunks that never move and one open-addressed table from
//! fingerprint bits to the node's place in the arena.
//!
//! - A node (content ID, index, value) is set once in its slot, under its shard's lock, before
//!   the table entry naming it is published with one release store; a reader that loads the
//!   entry with acquire sees the node whole.
//! - The fingerprint picks the shard (its bits 0..32) and is the node's hash in the shard's
//!   `blaze_mem::swiss::Index` (bits 64..128: the probe's start and tag).
//! - A full index is copied into one twice its size under the lock and published with one
//!   release store. The tables it outgrew stay until the interner is dropped (a reader may
//!   still be probing one), so they cost at most what the last one does.
//! - An [`Idx`] is the node's place in its shard's arena shifted past the shard bits, ored with
//!   the shard: an index to a node is arithmetic and two acquire loads.

use std::{
    any::type_name,
    fmt, hash,
    ops::Deref,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_ids::{ContentId, Fingerprint, Hasher, Idx, Interner};
use blaze_mem::swiss::Index;

use crate::{Consed, content_of};

/// Chunk `k` of a shard's arena holds `FIRST << k` nodes, so a place's chunk and slot are
/// arithmetic and 27 chunks hold every `u32`.
pub(crate) const FIRST: usize = 64;
pub(crate) const CHUNKS: usize = 27;

/// One interned value, with its content ID and the index it was given.
struct Node<T> {
    content: ContentId,
    raw: u32,
    value: T,
}

type Chunk<T> = Box<[OnceLock<Node<T>>]>;

/// A shard's writer lock: held to look again, append one node and publish its entry. It runs
/// no code of `T` (its equality, hash and drop run after it is let go of) and calls nothing
/// that could intern, so it is never held across a call into the database or another
/// interner.
struct ShardLock {
    #[allow(clippy::disallowed_types)]
    inner: parking_lot::Mutex<()>,
}

#[allow(clippy::disallowed_types)]
impl ShardLock {
    fn new() -> ShardLock {
        ShardLock { inner: parking_lot::Mutex::new(()) }
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.inner.lock()
    }
}

/// One cache line pair per shard, so writers to neighbouring shards don't share a line.
#[repr(align(128))]
struct Shard<T> {
    lock: ShardLock,
    /// How many nodes the arena has: written under `lock`, with release after each node is
    /// set.
    len: AtomicU32,
    /// The shard's nodes by their fingerprint's bits 64..128: a find is one control word and
    /// one slot, then the node's fingerprint compared only on a tag match.
    index: Index,
    nodes: [OnceLock<Chunk<T>>; CHUNKS],
}

pub(crate) fn chunk_of(local: u32) -> (usize, usize) {
    let biased = local as usize / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, local as usize - FIRST * ((1 << chunk) - 1))
}

/// Where a fingerprint goes: its shard and its hash in the shard's index.
#[derive(Clone, Copy)]
struct Key {
    fingerprint: Fingerprint,
    shard: usize,
    hash: u64,
}

impl Key {
    fn of(content: &ContentId, mask: usize) -> Key {
        let fingerprint = content.fingerprint();
        let bits = fingerprint.0;
        Key { fingerprint, shard: bits as u32 as usize & mask, hash: (bits >> 64) as u64 }
    }
}

impl<T> Shard<T> {
    fn new() -> Shard<T> {
        Shard {
            lock: ShardLock::new(),
            len: AtomicU32::new(0),
            index: Index::new(),
            nodes: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    fn node(&self, local: u32) -> Option<&Node<T>> {
        let (chunk, at) = chunk_of(local);
        self.nodes[chunk].get()?.get(at)?.get()
    }

    /// The node whose fingerprint is `key`'s, taking no lock.
    fn find(&self, key: &Key) -> Option<&Node<T>> {
        let is = |local: u32| {
            self.node(local).expect("an index entry before its node").content.fingerprint() == key.fingerprint
        };
        self.node(self.index.find(key.hash, is)?)
    }
}

/// The values of `T`, each stored once.
///
/// [`HashCons::intern`] hashes its value once and probes for it without a lock; only a value
/// not interned yet locks its one shard, to look again and append it. [`HashCons::get`],
/// [`HashCons::handle`] and [`HashCons::lookup`] take no lock and write nothing. A value
/// never moves or changes once interned, so its handle lives as long as the interner.
pub struct HashCons<T> {
    shards: Box<[Shard<T>]>,
    /// `shards.len() - 1`.
    mask: usize,
    /// How many low bits of an index name its shard.
    shift: u32,
}

impl<T: Consed> HashCons<T> {
    pub fn new() -> HashCons<T> {
        let count = blaze_ids::shard_count();
        debug_assert!(count.is_power_of_two() && count >= 4);
        HashCons {
            shards: (0..count).map(|_| Shard::new()).collect(),
            mask: count - 1,
            shift: count.trailing_zeros(),
        }
    }

    /// The handle of the value equal to `value`, interning it if there is none. Its content
    /// ID is computed here, once; an equal value already interned is compared with `value`
    /// once, and `value` dropped.
    pub fn intern(&self, value: T) -> Interned<'_, T> {
        let content = content_of(&value);
        self.intern_content(content, value)
    }

    /// [`HashCons::intern`] of a value whose content ID the caller has just computed with
    /// [`content_of`].
    pub(crate) fn intern_content(&self, content: ContentId, value: T) -> Interned<'_, T> {
        let key = Key::of(&content, self.mask);
        let shard = &self.shards[key.shard];
        if let Some(node) = shard.find(&key) {
            return matched(node, &content, value);
        }
        let held = shard.lock.lock();
        if let Some(node) = shard.find(&key) {
            drop(held);
            return matched(node, &content, value);
        }
        let node = self.append(shard, &key, content, value);
        drop(held);
        Interned { node }
    }

    /// Appends a node to `shard`'s arena and publishes its entry. Called with the shard
    /// locked and `key` absent from its index.
    fn append<'s>(&self, shard: &'s Shard<T>, key: &Key, content: ContentId, value: T) -> &'s Node<T> {
        let local = shard.len.load(Ordering::Relaxed);
        assert!(
            local < u32::MAX >> self.shift,
            "a hash-cons shard of {} holds {local} values: indices are u32",
            type_name::<T>()
        );
        let raw = (local << self.shift) | key.shard as u32;
        let (chunk, at) = chunk_of(local);
        let nodes = shard.nodes[chunk].get_or_init(|| (0..FIRST << chunk).map(|_| OnceLock::new()).collect());
        if nodes[at].set(Node { content, raw, value }).is_err() {
            unreachable!("index {raw} handed out twice");
        }
        let node = nodes[at].get().expect("a node just set");
        let hash_of = |local: u32| {
            (shard.node(local).expect("an index entry before its node").content.fingerprint().0 >> 64) as u64
        };
        shard.index.insert(local as usize, key.hash, local, hash_of);
        shard.len.store(local + 1, Ordering::Release);
        node
    }

    fn node(&self, raw: u32) -> Option<&Node<T>> {
        self.shards[raw as usize & self.mask].node(raw >> self.shift)
    }

    /// The handle of the value `idx` names. Panics for an index this interner did not hand
    /// out.
    pub fn handle(&self, idx: Idx<T>) -> Interned<'_, T> {
        let node = self.node(idx.raw()).expect("an index this hash-cons did not hand out");
        Interned { node }
    }

    pub fn get(&self, idx: Idx<T>) -> &T {
        self.handle(idx).get()
    }

    /// The handle of the value whose content ID is `content`, if it is interned.
    pub fn lookup(&self, content: &ContentId) -> Option<Interned<'_, T>> {
        let key = Key::of(content, self.mask);
        let node = self.shards[key.shard].find(&key)?;
        (node.content == *content).then_some(Interned { node })
    }

    /// How many values are interned.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.len.load(Ordering::Relaxed) as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Above every index handed out so far: what a column indexed by [`Idx`] is sized to.
    pub fn end(&self) -> usize {
        let longest = self.shards.iter().map(|shard| shard.len.load(Ordering::Acquire)).max().unwrap_or(0);
        (longest as usize) << self.shift
    }

    /// Every value interned, in content ID order: the same order in every run, whatever order
    /// threads interned in.
    pub fn sorted(&self) -> Vec<Interned<'_, T>> {
        let mut all = Vec::with_capacity(self.len());
        for shard in self.shards.iter() {
            let len = shard.len.load(Ordering::Acquire);
            all.extend((0..len).map(|local| Interned {
                node: shard.node(local).expect("a node below its shard's length"),
            }));
        }
        all.sort_unstable_by(|a, b| a.node.content.cmp(&b.node.content));
        all
    }
}

/// The handle of `node`, found for `value` by its fingerprint: the same value, or a collision
/// that would alias two values in every memo that compares them by fingerprint.
fn matched<'a, T: Consed>(node: &'a Node<T>, content: &ContentId, value: T) -> Interned<'a, T> {
    if node.content != *content {
        panic!(
            "two values of {} share fingerprint {:?} (content IDs {:x} and {:x}): a 128-bit \
             BLAKE3 collision, which early cutoff can't tell apart",
            type_name::<T>(),
            node.content.fingerprint(),
            node.content,
            content
        );
    }
    if node.value != value {
        panic!(
            "two unequal values of {} share content ID {:x}: its Consed::hash_content does not \
             feed everything its Eq compares",
            type_name::<T>(),
            content
        );
    }
    Interned { node }
}

impl<T: Consed> Default for HashCons<T> {
    fn default() -> HashCons<T> {
        HashCons::new()
    }
}

/// `blaze-ids`' interner contract over a hash-cons: `value` runs only when `id` is not
/// interned, and its content ID is checked against `id` before it is.
impl<T: Consed> Interner<T> for HashCons<T> {
    fn intern(&self, id: ContentId, value: impl FnOnce() -> T) -> Idx<T> {
        if let Some(found) = self.lookup(&id) {
            return found.idx();
        }
        let value = value();
        let content = content_of(&value);
        assert!(
            content == id,
            "a value of {} interned as content ID {id:x} has content ID {content:x}",
            type_name::<T>()
        );
        self.intern_content(content, value).idx()
    }

    fn get(&self, idx: Idx<T>) -> &T {
        self.handle(idx).get()
    }

    fn content(&self, idx: Idx<T>) -> ContentId {
        self.handle(idx).content()
    }

    fn find(&self, id: &ContentId) -> Option<Idx<T>> {
        self.lookup(id).map(Interned::idx)
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.len.load(Ordering::Relaxed) as usize).sum()
    }
}

/// An interned value: one pointer. Two handles of one interner are equal exactly when their
/// values are. It hashes as its fingerprint, so a map or a query's hash of it is the
/// same in every run; it has no `Ord`, as its address and index are not (sort by
/// [`Interned::content`]).
pub struct Interned<'a, T> {
    node: &'a Node<T>,
}

impl<'a, T> Interned<'a, T> {
    pub fn get(self) -> &'a T {
        &self.node.value
    }

    /// The value's dense index in its interner: for columns of this run, never for a hash, a
    /// key or an output.
    pub fn idx(self) -> Idx<T> {
        Idx::from_raw(self.node.raw)
    }

    pub fn content(self) -> ContentId {
        self.node.content
    }

    /// The value's fingerprint, computed when it was interned.
    pub fn fingerprint(self) -> Fingerprint {
        self.node.content.fingerprint()
    }

    /// Feeds the value to the content hash of a value it is part of: by its content ID.
    pub fn hash_into(self, hasher: &mut Hasher) {
        hasher.id(&self.node.content);
    }
}

impl<T> Clone for Interned<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Interned<'_, T> {}

impl<T> PartialEq for Interned<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.node, other.node)
    }
}

impl<T> Eq for Interned<'_, T> {}

impl<T> hash::Hash for Interned<'_, T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        state.write_u128(self.node.content.fingerprint().0);
    }
}

impl<T> Deref for Interned<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.node.value
    }
}

impl<T: fmt::Debug> fmt::Debug for Interned<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Interned").field(&self.node.content).field(&self.node.value).finish()
    }
}