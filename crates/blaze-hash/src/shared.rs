//! The tables shared between threads ([`crate::SharedMap`], [`crate::SharedSet`],
//! [`crate::SharedByHash`]): a fixed number of shards, each an open-addressed table of pointers
//! to boxed entries, read with no lock and no store to shared memory, written under the
//! shard's lock, with what a write unlinks freed by `blaze_reclaim` once no reader can hold it.
//!
//! **Reads.** [`Map::pin`] enters a `blaze_reclaim` task; a lookup loads the shard's table
//! (Acquire), probes it linearly from the hash (each slot Acquire) and compares hash, then key.
//! A value is lent for as long as the pin lives: an entry is boxed once and never moved, and
//! one that is replaced or removed is retired, not freed, so it stays readable until every task
//! that could have seen it has ended. A reader never waits and writes nothing shared beyond its
//! own thread's epoch slot.
//!
//! **Writes.** Insert, replace and remove take the shard's lock (one per [`SHARDS`]th of the
//! keys, by the hash's top bits), change one slot with a Release store, and retire what they
//! unlinked into the shard's limbo. A table that would pass three quarters full (tombstones
//! counted) is copied into one of twice the live entries, published with a Release store, and
//! the old one retired; entries are pointers, so a copy moves no value. A slot goes from empty
//! to an entry, from an entry to another or to a tombstone, and from a tombstone to an entry; a
//! table always keeps an empty slot, so every probe ends.
//!
//! **Orderings, in one line:** entry published Release / loaded Acquire, table published
//! Release / loaded Acquire, lengths Relaxed (a count, never a guard), retirement by
//! `blaze_reclaim`'s epoch (its SeqCst fences).
//!
//! Under `cfg(blaze_check)` the atomics and the lock are the model checker's and each entry's
//! bytes are tracked, so a model reports a read that a publish does not order, and a read of an
//! entry freed under a reader.

use core::{alloc::Allocator, fmt, marker::PhantomData, ptr};

use blaze_own::{
    Box, Vec,
    planted::{NoPlant, Planted, Plants},
};
use blaze_reclaim::{Guard, Horizon, Limbo};

use crate::{Equivalent, KeyHash};

/// Keys a batch of [`Pinned::get_many_into`] prefetches before it probes: two dependent
/// misses a key (its slot, its boxed entry), so deeper than the owning map's pipeline.
/// Chosen by blaze-hashcons' `examples/batch_cost.rs`.
pub const SHARED_BATCH: usize = 32;

#[cfg(test)]
mod tests;

/// How many shards a table has: a power of two, fixed so that a table is laid out the same on
/// every machine; 64 keeps two writers on one shard rare at 128 threads, and an empty table at
/// 64 lines.
pub const SHARDS: usize = 64;

/// Retired values a shard holds before a write asks which may be freed: the ask scans every
/// thread's epoch slot, so it is made once per this many retirements, not per write.
const COLLECT_AT: usize = 32;

/// The smallest table a shard allocates.
const SMALLEST: usize = 8;

mod sync {
    #[cfg(not(blaze_check))]
    pub(super) use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    #[cfg(blaze_check)]
    pub(super) use blaze_check::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    /// The bytes at `at` are written: a model checks every earlier access happens before.
    #[inline(always)]
    pub(super) fn wrote<T>(at: *const T) {
        #[cfg(blaze_check)]
        blaze_check::cell::write(at.cast(), size_of::<T>());
        _ = at;
    }

    /// The bytes at `at` are read: a model checks their last write happens before.
    #[inline(always)]
    pub(super) fn read<T>(at: *const T) {
        #[cfg(blaze_check)]
        blaze_check::cell::read(at.cast(), size_of::<T>());
        _ = at;
    }

    /// The bytes at `at` are freed: a model reports a later read of them.
    #[inline(always)]
    pub(super) fn freed<T>(at: *const T) {
        #[cfg(blaze_check)]
        blaze_check::cell::free(at.cast(), size_of::<T>());
        _ = at;
    }
}

use sync::{AtomicPtr, AtomicUsize, Ordering};

/// The blocking lock a shard's writers take, given by the crate that instantiates the map
/// (decision D64): blaze-core's named lock (`blaze_core::SharedMap`), whose wait is a deadline
/// wait with a span, which this crate below blaze-core cannot name; blaze-check's lock under the
/// model. A family rather than one lock type, so the map's private writer state is never named
/// in a public bound. No reader takes it: only writers of one shard.
///
/// # Safety
///
/// The map's soundness rests on it: while a [`ShardLock::Guard`] lives, no other guard of the
/// same lock does (mutual exclusion), and the value it lends is the one [`ShardLock::new`] was
/// given, with every write under an earlier guard happening before the next guard is handed out.
pub unsafe trait ShardLock {
    /// A lock holding a `T`.
    type Lock<T>;
    /// Its hold: lends the value and lets the lock go when it drops.
    type Guard<'a, T: 'a>: core::ops::DerefMut<Target = T>;
    /// A lock named `name` (what a wait on it is called in the profile) holding `value`.
    fn new<T>(name: &'static str, value: T) -> Self::Lock<T>;
    /// The lock, waiting for it while another writer holds it.
    fn lock<T>(lock: &Self::Lock<T>) -> Self::Guard<'_, T>;
}

/// A bug a model test plants, to show its model catches it: through the seam
/// (`blaze_own::planted`), so a write takes `impl Planted<Plant>` and shipped code passes
/// [`NoPlant`], which folds every check to `false`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// No bug: the control run.
    #[cfg_attr(not(all(test, blaze_check)), expect(dead_code, reason = "the model tests' control"))]
    None,
    /// An entry's slot stored Relaxed: a reader that loads it is not ordered after the
    /// entry's bytes.
    RelaxedPublish,
    /// A replaced entry freed at once, not retired: a reader in it reads freed bytes.
    FreeAtOnce,
}

impl Plants for Plant {}

/// One key and its value, boxed once and never moved, with the key's hash.
struct Entry<K, V> {
    hash: u64,
    key: K,
    value: V,
}

impl<K, V> Drop for Entry<K, V> {
    fn drop(&mut self) {
        sync::freed(ptr::from_ref(self));
    }
}

/// A shard's slots: a power of two of them, each empty (null), an entry, or a tombstone. In the
/// map's allocator, as its entries are.
struct Table<K, V, A: Allocator> {
    mask: usize,
    slots: Box<[AtomicPtr<Entry<K, V>>], A>,
}

/// What a removed entry's slot holds: never dereferenced, never an allocation's address.
fn tombstone<K, V>() -> *mut Entry<K, V> {
    ptr::without_provenance_mut(1)
}

/// A slot that holds an entry.
fn live<K, V>(at: *mut Entry<K, V>) -> bool {
    !at.is_null() && at != tombstone()
}

impl<K, V, A: Allocator> Table<K, V, A> {
    fn new(capacity: usize, alloc: A) -> Table<K, V, A> {
        debug_assert!(capacity.is_power_of_two());
        let mut slots = Vec::with_capacity_in(capacity, alloc);
        slots.resize_with(capacity, || AtomicPtr::new(ptr::null_mut()));
        Table { mask: capacity - 1, slots: slots.into_boxed_slice() }
    }

    /// The entry of `key`, read with no lock.
    fn find<Q: ?Sized + Equivalent<K>>(&self, hash: u64, key: &Q) -> Option<&Entry<K, V>> {
        let mut at = hash as usize & self.mask;
        loop {
            // Acquire: the entry's bytes, written before its Release publish.
            let slot = self.slots[at].load(Ordering::Acquire);
            if slot.is_null() {
                return None;
            }
            if slot != tombstone() {
                sync::read(slot.cast_const());
                // SAFETY: a live slot points at a boxed entry, freed only once retired and its
                // grace passed, which the caller's pin (a `blaze_reclaim` task) holds back.
                let entry = unsafe { &*slot };
                if entry.hash == hash && key.equivalent(&entry.key) {
                    return Some(entry);
                }
            }
            at = (at + 1) & self.mask;
        }
    }

    /// Starts loading the slot a [`Table::find`] of `hash` reads first.
    #[inline]
    fn prefetch_slot(&self, hash: u64) {
        blaze_swiss::prefetch(ptr::from_ref(&self.slots[hash as usize & self.mask]));
    }

    /// Starts loading the entry the first slot of `hash` points at: a batch's second step,
    /// once [`Table::prefetch_slot`] brought the slot in. A hint: the pointer is not read
    /// through.
    #[inline]
    fn prefetch_entry(&self, hash: u64) {
        // Relaxed: the pointer only names a line to load; `find` loads it again, Acquire.
        let slot = self.slots[hash as usize & self.mask].load(Ordering::Relaxed);
        if live(slot) {
            blaze_swiss::prefetch(slot.cast_const());
        }
    }

    /// Under the shard's lock: the slot of `key`, or else the slot an insert of it takes (the
    /// first tombstone of its run, or the empty slot that ends it), and whether it is the key's.
    fn place<Q: ?Sized + Equivalent<K>>(&self, hash: u64, key: &Q) -> (usize, bool) {
        let mut at = hash as usize & self.mask;
        let mut free = None;
        loop {
            // Relaxed: under the lock, every store to this table is this thread's or ordered
            // before it by the lock.
            let slot = self.slots[at].load(Ordering::Relaxed);
            if slot.is_null() {
                return (free.unwrap_or(at), false);
            }
            if slot == tombstone() {
                free.get_or_insert(at);
            } else {
                // SAFETY: a live slot of the current table, under the lock: nothing frees it.
                let entry = unsafe { &*slot };
                if entry.hash == hash && key.equivalent(&entry.key) {
                    return (at, true);
                }
            }
            at = (at + 1) & self.mask;
        }
    }
}

/// A shard's writer state, behind its lock.
struct Writer {
    /// Slots that are not empty: entries and tombstones.
    used: usize,
    /// What writes unlinked: entries and tables, freed once no reader can hold them.
    limbo: Limbo,
}

/// One part of a table, by the top bits of the hash, on lines of its own.
#[repr(align(128))]
struct Shard<K, V, A: Allocator, L: ShardLock> {
    table: AtomicPtr<Table<K, V, A>>,
    len: AtomicUsize,
    /// How many values `limbo` holds, read before the lock to skip the scan of a collect.
    held: AtomicUsize,
    writer: L::Lock<Writer>,
}

impl<K, V, A: Allocator, L: ShardLock> Shard<K, V, A, L> {
    fn new(name: &'static str) -> Shard<K, V, A, L> {
        Shard {
            table: AtomicPtr::new(ptr::null_mut()),
            len: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
            writer: L::new(name, Writer { used: 0, limbo: Limbo::new() }),
        }
    }

    /// The current table, for a reader inside a pin: `None` before the first insert.
    fn current(&self) -> Option<&Table<K, V, A>> {
        // Acquire: the table's slots, written before its Release publish.
        let table = self.table.load(Ordering::Acquire);
        // SAFETY: a published table is freed only once retired and its grace passed, which
        // the caller's pin holds back.
        (!table.is_null()).then(|| unsafe { &*table })
    }
}

/// A table shared between threads; see the module docs. Made by [`Map::new`] from a
/// [`MapOpts`] (D126; the hasher is named as a value, D125), in the allocator the caller passes:
/// the shards, every table and
/// every entry are in it. A write retires what it unlinks into a limbo that may outlive the
/// call, so the allocator is a cloneable handle that lives as long as the program
/// (`Send + Sync + 'static` for a write): the global heap, or a region kept for the run.
pub struct Map<K, V, S, A: Allocator + Clone, L: ShardLock> {
    shards: Box<[Shard<K, V, A, L>], A>,
    hasher: S,
    alloc: A,
    _owns: PhantomData<Box<Entry<K, V>, A>>,
}

// SAFETY: the map owns its keys and values (boxed entries) and hands them to whichever thread
// drops the map or an entry's grace passes on, so they are `Send`; any thread may read them
// through a pin, so they are `Sync`. The rest is atomics, a lock and the allocator handle,
// which every thread that writes or drops uses.
unsafe impl<K: Send + Sync, V: Send + Sync, S: Send, A: Allocator + Clone + Send + Sync, L: ShardLock> Send for Map<K, V, S, A, L> {}

// SAFETY: as for `Send`: a shared map lends `&K` and `&V` to every thread and moves them to
// the thread whose write unlinks them; any thread's write allocates through `&A`.
unsafe impl<K: Send + Sync, V: Send + Sync, S: Sync, A: Allocator + Clone + Send + Sync, L: ShardLock> Sync for Map<K, V, S, A, L> {}

/// What a shared [`Map`] is made from ([`Map::new`], rule 83 options-struct): every field is
/// written at every use, the hasher too (D125):
/// `Map::new(MapOpts { name: "cas.packs", hasher: FixedState, alloc: Global, capacity: None })`.
///
/// There is no shard count: it is [`SHARDS`], a constant so that a table is laid out the same on
/// every machine, and the read path finds a key's shard by a shift of that constant; one value
/// is right everywhere, so it is not a field.
///
/// A call's named arguments: write it as a literal in the call that takes it (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct MapOpts<S, A> {
    /// What a wait on one of the shards' writer locks is called in the profile: say what the
    /// table holds (`"cas.packs"`).
    pub name: &'static str,
    /// The hasher, as a value: `FixedState` for ordinary keys, `BuildKeyHasher::default()` for
    /// keys that are hashes already, a `SipKey` for keys an outsider picks.
    pub hasher: S,
    /// The allocator the shards, every table and every entry are in: a cloneable handle that
    /// lives as long as the table (see [`Map`]).
    pub alloc: A,
    /// Entries room is made for at once: `None` makes each shard's table at its first insert;
    /// `Some(n)` makes every shard's table now with room for its even part of `n` (`n / SHARDS`
    /// rounded up, within three quarters full), so no shard grows while the keys spread evenly.
    /// A shard that gets more than its part grows as it would have.
    pub capacity: Option<usize>,
}

impl<K, V, S, A: Allocator + Clone, L: ShardLock> Map<K, V, S, A, L> {
    /// The empty table `opts` describe: the shards, and their tables if `opts.capacity` asks for
    /// room. A table of an iterator's pairs is `new`, then an insert of each through one
    /// [`Map::pin`].
    pub fn new(opts: MapOpts<S, A>) -> Map<K, V, S, A, L> {
        let mut shards = Vec::with_capacity_in(SHARDS, opts.alloc.clone());
        shards.resize_with(SHARDS, || Shard::new(opts.name));
        let map = Map {
            shards: shards.into_boxed_slice(),
            hasher: opts.hasher,
            alloc: opts.alloc,
            _owns: PhantomData,
        };
        let capacity = opts.capacity.unwrap_or(0);
        if capacity == 0 {
            return map;
        }
        // `room` grows a table once `(used + 1) * 4 > slots * 3`: this many slots hold the
        // shard's part with no growth.
        let part = capacity.div_ceil(SHARDS);
        let slots = (part * 4).div_ceil(3).next_power_of_two().max(SMALLEST);
        for shard in &*map.shards {
            let table = Table::new(slots, map.alloc.clone());
            let (table, _) = Box::into_raw_with_allocator(Box::new_in(table, map.alloc.clone()));
            // Relaxed: the map is this thread's alone until it is returned; whatever shares it
            // after (a spawn, a channel, a `static`'s init) orders this store before any read.
            shard.table.store(table, Ordering::Relaxed);
        }
        map
    }
}

impl<K, V, S: Default, A: Allocator + Clone, L: ShardLock> Map<K, V, S, A, L> {
    /// An empty table on `S::default()` in `alloc`, its writer locks named `name`.
    ///
    /// D126: goes at the sweep; write `Map::new(MapOpts { name, hasher, alloc, capacity: None })`.
    pub fn new_in(name: &'static str, alloc: A) -> Map<K, V, S, A, L> {
        Map::new(MapOpts { name, hasher: S::default(), alloc, capacity: None })
    }
}

impl<K, V, S, A: Allocator + Clone, L: ShardLock> Map<K, V, S, A, L> {
    /// An empty table on `hasher` in `alloc`, its writer locks named `name`.
    ///
    /// D126: goes at the sweep; write `Map::new(MapOpts { name, hasher, alloc, capacity: None })`.
    pub fn with_hasher_in(name: &'static str, hasher: S, alloc: A) -> Map<K, V, S, A, L> {
        Map::new(MapOpts { name, hasher, alloc, capacity: None })
    }
}

impl<K, V, S, A: Allocator + Clone, L: ShardLock> Map<K, V, S, A, L> {
    /// Lends the table to this thread's task: what it reads stays valid until the pin drops.
    /// The pin may be held across a wait (`blaze_reclaim::enter`).
    pub fn pin(&self) -> Pinned<'_, K, V, S, A, L> {
        Pinned { map: self, _task: blaze_reclaim::enter() }
    }

    /// How many entries it holds: a sum of the shards' counts, each read on its own.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.len.load(Ordering::Relaxed)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn hasher(&self) -> &S {
        &self.hasher
    }
}

impl<K, V, S, A: Allocator + Clone, L: ShardLock> Drop for Map<K, V, S, A, L> {
    fn drop(&mut self) {
        for shard in &*self.shards {
            // Relaxed: `&mut self`, so every store to it happened before.
            let table = shard.table.load(Ordering::Relaxed);
            if table.is_null() {
                continue;
            }
            // SAFETY: the map is dropped, so no pin of it lives: the current table and its
            // live entries are this drop's alone (retired ones are the limbo's, dropped with it).
            let table = unsafe { Box::from_raw_in(table, self.alloc.clone()) };
            for slot in &*table.slots {
                let entry = slot.load(Ordering::Relaxed);
                if live(entry) {
                    // SAFETY: as above; each live entry is in one slot of the current table, in
                    // the map's allocator.
                    drop(unsafe { Box::from_raw_in(entry, self.alloc.clone()) });
                }
            }
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S, A: Allocator + Clone, L: ShardLock> fmt::Debug for Map<K, V, S, A, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pinned = self.pin();
        f.debug_map().entries(pinned.iter()).finish()
    }
}

/// A [`Map`] lent to this thread's task ([`Map::pin`]); what it lends lives as long as it does.
pub struct Pinned<'m, K, V, S, A: Allocator + Clone, L: ShardLock> {
    map: &'m Map<K, V, S, A, L>,
    _task: Guard,
}

/// A [`Pinned::try_insert`] of a key the table holds: what it holds, and the value not put.
pub struct OccupiedError<'g, V> {
    pub current: &'g V,
    pub not_inserted: V,
}

impl<V: fmt::Debug> fmt::Debug for OccupiedError<'_, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OccupiedError")
            .field("current", self.current)
            .field("not_inserted", &self.not_inserted)
            .finish()
    }
}

impl<K, V, S, A: Allocator + Clone, L: ShardLock> Pinned<'_, K, V, S, A, L> {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every entry, shard by shard: each one present for the whole walk is met once; one
    /// inserted or removed during it may be met or not.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.map.shards.iter().filter_map(Shard::current).flat_map(|table| {
            table.slots.iter().filter_map(|slot| {
                let entry = slot.load(Ordering::Acquire);
                if !live(entry) {
                    return None;
                }
                sync::read(entry.cast_const());
                // SAFETY: a live slot's entry, held back from freeing by this pin.
                let entry = unsafe { &*entry };
                Some((&entry.key, &entry.value))
            })
        })
    }

    /// The keys, as [`Pinned::iter`].
    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(key, _)| key)
    }
}

impl<K, V, S, A: Allocator + Clone, L: ShardLock> Pinned<'_, K, V, S, A, L> {
    fn shard_of(&self, hash: u64) -> &Shard<K, V, A, L> {
        &self.map.shards[(hash >> (u64::BITS - SHARDS.trailing_zeros())) as usize]
    }

    /// `key`'s hash, by the one seam every table hashes at ([`KeyHash`]): a `PlainKey` on
    /// `FixedState` by its bytes, a `DigestKey` on `DigestState` its word.
    #[inline(always)]
    fn hash_of<Q: ?Sized>(&self, key: &Q) -> u64
    where
        S: KeyHash<Q>,
    {
        <S as KeyHash<Q>>::hash_key(&self.map.hasher, key)
    }

    fn entry<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<&Entry<K, V>>
    where
        S: KeyHash<Q>,
    {
        let hash = self.hash_of(key);
        self.shard_of(hash).current()?.find(hash, key)
    }

    /// The value of `key`, read with no lock.
    pub fn get<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<&V>
    where
        S: KeyHash<Q>,
    {
        self.entry(key).map(|entry| &entry.value)
    }

    /// Starts loading the slot a [`Pinned::get`] of `key` reads first, for a caller that
    /// interleaves its own lookups. A hint: it reads no entry.
    pub fn prefetch<Q: ?Sized>(&self, key: &Q)
    where
        S: KeyHash<Q>,
    {
        let hash = self.hash_of(key);
        if let Some(table) = self.shard_of(hash).current() {
            table.prefetch_slot(hash);
        }
    }

    /// The entry of each of the `count` keys `key(at)` names, handed to `answer(at, entry)` in
    /// order, by group prefetching `CHUNK` keys at a time in three steps: every key's first
    /// slot loaded, then the entry each points at, then each probed. A lookup here is two
    /// dependent misses (the slot, then its boxed entry), so a batch on tables larger than the
    /// cache waits about two misses in all rather than two per key.
    fn each_entry<'s, 'k, Q: ?Sized + Equivalent<K> + 'k, const CHUNK: usize>(
        &'s self,
        count: usize,
        key: impl Fn(usize) -> &'k Q,
        mut answer: impl FnMut(usize, Option<&'s Entry<K, V>>),
    ) where
        S: KeyHash<Q>,
    {
        let mut start = 0;
        while start < count {
            let len = (count - start).min(CHUNK);
            let mut hashes = [0u64; CHUNK];
            let mut tables = [None; CHUNK];
            for at in 0..len {
                hashes[at] = self.hash_of(key(start + at));
                tables[at] = self.shard_of(hashes[at]).current();
                if let Some(table) = tables[at] {
                    table.prefetch_slot(hashes[at]);
                }
            }
            for at in 0..len {
                if let Some(table) = tables[at] {
                    table.prefetch_entry(hashes[at]);
                }
            }
            for at in 0..len {
                let entry = tables[at].and_then(|table| table.find(hashes[at], key(start + at)));
                answer(start + at, entry);
            }
            start += len;
        }
    }

    /// [`Pinned::get`] of each of `keys`, as one batch ([`Pinned::each_entry`]'s prefetching);
    /// duplicates and absent keys are answered as `get` answers them.
    pub fn get_many<Q: ?Sized + Equivalent<K>, const N: usize>(
        &self,
        keys: [&Q; N],
    ) -> [Option<&V>; N]
    where
        S: KeyHash<Q>,
    {
        let mut out = [None; N];
        self.each_entry::<Q, N>(
            N,
            |at| keys[at],
            |at, entry| out[at] = entry.map(|entry| &entry.value),
        );
        out
    }

    /// [`Pinned::get_many`] of a slice, [`SHARED_BATCH`] keys a batch: `out[at]` is the
    /// value of `keys[at]`. Panics unless `out` is as long as `keys`.
    pub fn get_many_into<'a, Q: Equivalent<K>>(
        &'a self,
        keys: &[Q],
        out: &mut [Option<&'a V>],
    ) where
        S: KeyHash<Q>,
    {
        assert_eq!(keys.len(), out.len(), "Pinned::get_many_into: one answer per key");
        self.each_entry::<Q, SHARED_BATCH>(
            keys.len(),
            |at| &keys[at],
            |at, entry| out[at] = entry.map(|entry| &entry.value),
        );
    }

    /// The key and value of `key`, read with no lock.
    pub fn get_key_value<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<(&K, &V)>
    where
        S: KeyHash<Q>,
    {
        self.entry(key).map(|entry| (&entry.key, &entry.value))
    }

    pub fn contains_key<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> bool
    where
        S: KeyHash<Q>,
    {
        self.entry(key).is_some()
    }
}

/// What a write does to the slot of a key the table holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Leaves it: the write's value is handed back.
    Keep,
    /// Replaces it, retiring the old entry.
    Replace,
}

/// What [`Pinned::write`] did.
enum Wrote<'g, K, V> {
    /// The key was not held; its new entry.
    Inserted(&'g Entry<K, V>),
    /// The key was held and replaced: the old entry, retired and still readable in this pin.
    Replaced(&'g Entry<K, V>),
    /// The key was held and kept: its entry, and the value not put.
    Kept(&'g Entry<K, V>, V),
}

impl<K: Eq + Send + Sync + 'static, V: Send + Sync + 'static, S: KeyHash<K>, A: Allocator + Clone + Send + Sync + 'static, L: ShardLock>
    Pinned<'_, K, V, S, A, L>
{
    /// Puts `value` for `key`; the value it replaced, readable while this pin lives.
    pub fn insert(&self, key: K, value: V) -> Option<&V> {
        match self.write(key, value, Held::Replace, NoPlant) {
            Wrote::Replaced(old) => Some(&old.value),
            Wrote::Inserted(_) | Wrote::Kept(..) => None,
        }
    }

    /// Puts `value` for `key` unless it is held; else what is held, and `value` back.
    pub fn try_insert(&self, key: K, value: V) -> Result<&V, OccupiedError<'_, V>> {
        match self.write(key, value, Held::Keep, NoPlant) {
            Wrote::Inserted(entry) => Ok(&entry.value),
            Wrote::Kept(entry, not_inserted) => Err(OccupiedError { current: &entry.value, not_inserted }),
            Wrote::Replaced(_) => unreachable!("a write that keeps what is held replaced it"),
        }
    }

    /// The value of `key`, or else `value`, put now. Of two threads that put one key at once,
    /// both get the first's.
    pub fn get_or_insert(&self, key: K, value: V) -> &V {
        if let Some(held) = self.get(&key) {
            return held;
        }
        match self.try_insert(key, value) {
            Ok(put) => put,
            Err(occupied) => occupied.current,
        }
    }

    /// The value of `key`, or else the value of the entry `make(key)` builds, put now: the
    /// owned key and the value are made only on a miss, and `key` is hashed once. `make` runs
    /// outside the shard's lock; what it built is dropped if another thread put the key
    /// first, and both get the first's. `make` must return a key equal to `key` (checked in
    /// debug builds). A miss probes twice, without and then under the lock: another writer
    /// may have put the key between. Never allocates on a hit.
    pub fn get_or_insert_with<Q: ?Sized + Equivalent<K>>(
        &self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> &V
    where
        S: KeyHash<Q>,
    {
        let hash = self.hash_of(key);
        if let Some(entry) = self.shard_of(hash).current().and_then(|table| table.find(hash, key)) {
            return &entry.value;
        }
        let (owned, value) = make(key);
        debug_assert!(
            key.equivalent(&owned) && self.hash_of::<K>(&owned) == hash,
            "get_or_insert_with: `make` built a key that is not equal to the one asked"
        );
        match self.write_hashed(hash, owned, value, Held::Keep, NoPlant) {
            Wrote::Inserted(entry) | Wrote::Kept(entry, _) => &entry.value,
            Wrote::Replaced(_) => unreachable!("a write that keeps what is held replaced it"),
        }
    }

    /// Takes `key` out; its value, readable while this pin lives.
    pub fn remove<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<&V>
    where
        S: KeyHash<Q>,
    {
        self.remove_where(key, |_, _| true).ok().flatten().map(|(_, value)| value)
    }

    /// Takes `key` out if `should(key, value)`: `Ok` with what was taken (or `None` when the
    /// key is not held), `Err` with what stays.
    pub fn remove_if<Q: ?Sized + Equivalent<K>>(
        &self,
        key: &Q,
        should: impl FnOnce(&K, &V) -> bool,
    ) -> Result<Option<(&K, &V)>, (&K, &V)>
    where
        S: KeyHash<Q>,
    {
        self.remove_where(key, should)
    }

    /// Keeps only the entries `keep` says to, shard by shard under each one's lock.
    pub fn retain(&self, mut keep: impl FnMut(&K, &V) -> bool) {
        for shard in &*self.map.shards {
            let horizon = self.horizon(shard);
            let passed = {
                let mut writer = lock(shard);
                if let Some(table) = shard.current() {
                    for slot in &*table.slots {
                        let entry = slot.load(Ordering::Relaxed);
                        if !live(entry) {
                            continue;
                        }
                        // SAFETY: a live slot of the current table, under the lock.
                        let held = unsafe { &*entry };
                        if !keep(&held.key, &held.value) {
                            slot.store(tombstone(), Ordering::Release);
                            shard.len.fetch_sub(1, Ordering::Relaxed);
                            retire_entry(shard, &mut writer, &self.map.alloc, entry, NoPlant);
                        }
                    }
                }
                horizon.map(|horizon| collect(shard, &mut writer, horizon))
            };
            drop(passed);
        }
    }

    /// Takes every entry out.
    pub fn clear(&self) {
        for shard in &*self.map.shards {
            let mut writer = lock(shard);
            let table = shard.table.swap(ptr::null_mut(), Ordering::AcqRel);
            if table.is_null() {
                continue;
            }
            // SAFETY: the table just unlinked, under the lock: its slots are this write's to
            // walk, and it is retired below, never freed here.
            for slot in unsafe { &*table }.slots.iter() {
                let entry = slot.load(Ordering::Relaxed);
                if live(entry) {
                    retire_entry(shard, &mut writer, &self.map.alloc, entry, NoPlant);
                }
            }
            shard.len.store(0, Ordering::Relaxed);
            writer.used = 0;
            retire_table(shard, &mut writer, &self.map.alloc, table);
        }
    }

    /// The scan of every thread's epoch slot, taken before the lock (`blaze_reclaim::scans`),
    /// when the shard holds enough retired values to free.
    fn horizon(&self, shard: &Shard<K, V, A, L>) -> Option<Horizon> {
        (shard.held.load(Ordering::Relaxed) >= COLLECT_AT).then(Horizon::now)
    }

    fn remove_where<Q: ?Sized + Equivalent<K>>(
        &self,
        key: &Q,
        should: impl FnOnce(&K, &V) -> bool,
    ) -> Result<Option<(&K, &V)>, (&K, &V)>
    where
        S: KeyHash<Q>,
    {
        let hash = self.hash_of(key);
        let shard = self.shard_of(hash);
        if shard.current().is_none_or(|table| table.find(hash, key).is_none()) {
            return Ok(None);
        }
        let horizon = self.horizon(shard);
        let (taken, passed) = {
            let mut writer = lock(shard);
            let taken = shard.current().and_then(|table| {
                let (at, held) = table.place(hash, key);
                held.then(|| (table, at))
            });
            let taken = match taken {
                None => Ok(None),
                Some((table, at)) => {
                    let entry = table.slots[at].load(Ordering::Relaxed);
                    // SAFETY: a live slot of the current table, under the lock; once retired
                    // below it stays readable while this pin lives.
                    let held: &Entry<K, V> = unsafe { &*entry };
                    if should(&held.key, &held.value) {
                        table.slots[at].store(tombstone(), Ordering::Release);
                        shard.len.fetch_sub(1, Ordering::Relaxed);
                        retire_entry(shard, &mut writer, &self.map.alloc, entry, NoPlant);
                        Ok(Some((&held.key, &held.value)))
                    } else {
                        Err((&held.key, &held.value))
                    }
                }
            };
            (taken, horizon.map(|horizon| collect(shard, &mut writer, horizon)))
        };
        drop(passed);
        taken
    }

    /// Puts `key` and `value` under the shard's lock, doing `held` to a slot that holds the key.
    fn write(&self, key: K, value: V, held: Held, plant: impl Planted<Plant>) -> Wrote<'_, K, V> {
        let hash = self.hash_of::<K>(&key);
        self.write_hashed(hash, key, value, held, plant)
    }

    /// [`Pinned::write`] of a key whose hash the caller has: `hash` is `key`'s.
    fn write_hashed(
        &self,
        hash: u64,
        key: K,
        value: V,
        held: Held,
        plant: impl Planted<Plant>,
    ) -> Wrote<'_, K, V> {
        let shard = self.shard_of(hash);
        let horizon = self.horizon(shard);
        let (wrote, passed) = {
            let mut writer = lock(shard);
            let table = room(shard, &mut writer, &self.map.alloc);
            let (at, holds) = table.place(hash, &key);
            let wrote = if holds && held == Held::Keep {
                let entry = table.slots[at].load(Ordering::Relaxed);
                // SAFETY: a live slot of the current table, under the lock; this pin keeps it.
                Wrote::Kept(unsafe { &*entry }, value)
            } else {
                let (fresh, _) = Box::into_raw_with_allocator(Box::new_in(Entry { hash, key, value }, self.map.alloc.clone()));
                sync::wrote(fresh.cast_const());
                let publish = if plant.has(Plant::RelaxedPublish) { Ordering::Relaxed } else { Ordering::Release };
                let old = table.slots[at].swap(fresh, publish);
                // SAFETY: just published, and freed only once retired: this pin keeps it.
                let fresh_ref = unsafe { &*fresh };
                if live(old) {
                    retire_entry(shard, &mut writer, &self.map.alloc, old, plant);
                    // SAFETY: retired, not freed (except under the planted bug): readable while
                    // this pin lives, as every task that began before its grace.
                    Wrote::Replaced(unsafe { &*old })
                } else {
                    if old.is_null() {
                        writer.used += 1;
                    }
                    shard.len.fetch_add(1, Ordering::Relaxed);
                    Wrote::Inserted(fresh_ref)
                }
            };
            (wrote, horizon.map(|horizon| collect(shard, &mut writer, horizon)))
        };
        drop(passed);
        wrote
    }

    /// [`Pinned::insert`] with a planted bug, for the model tests.
    #[cfg(test)]
    pub(crate) fn insert_planted(&self, key: K, value: V, plant: impl Planted<Plant>) -> Option<&V> {
        match self.write(key, value, Held::Replace, plant) {
            Wrote::Replaced(old) => Some(&old.value),
            Wrote::Inserted(_) | Wrote::Kept(..) => None,
        }
    }
}

/// Takes `shard`'s writers' lock. A lock a panicking writer left is whole as it was left (a
/// writer changes a slot with one store, after anything that can panic), so a family that
/// poisons takes a poisoned lock as it is.
fn lock<K, V, A: Allocator, L: ShardLock>(shard: &Shard<K, V, A, L>) -> L::Guard<'_, Writer> {
    L::lock(&shard.writer)
}

/// Under the lock: the current table, made or grown in `alloc` so that one more slot can be used.
fn room<'s, K: Send + Sync + 'static, V: Send + Sync + 'static, A: Allocator + Clone + Send + Sync + 'static, L: ShardLock>(
    shard: &'s Shard<K, V, A, L>,
    writer: &mut Writer,
    alloc: &A,
) -> &'s Table<K, V, A> {
    let current = shard.table.load(Ordering::Relaxed);
    // SAFETY: the current table, under the lock: only this writer can retire it.
    let table = (!current.is_null()).then(|| unsafe { &*current });
    let capacity = table.map_or(0, |it| it.slots.len());
    if (writer.used + 1) * 4 <= capacity * 3 {
        return table.expect("a table with room");
    }
    let len = shard.len.load(Ordering::Relaxed);
    let grown = Table::new(((len + 1) * 2).next_power_of_two().max(SMALLEST), alloc.clone());
    if let Some(table) = table {
        for slot in &*table.slots {
            let entry = slot.load(Ordering::Relaxed);
            if live(entry) {
                // SAFETY: a live slot of the current table, under the lock.
                let hash = unsafe { &*entry }.hash;
                let mut at = hash as usize & grown.mask;
                while !grown.slots[at].load(Ordering::Relaxed).is_null() {
                    at = (at + 1) & grown.mask;
                }
                // Relaxed: the table is unpublished; its Release publish below orders these.
                grown.slots[at].store(entry, Ordering::Relaxed);
            }
        }
    }
    writer.used = len;
    let (grown, _) = Box::into_raw_with_allocator(Box::new_in(grown, alloc.clone()));
    // Release: the slots above, before a reader that loads the table reads them.
    shard.table.store(grown, Ordering::Release);
    if !current.is_null() {
        retire_table(shard, writer, alloc, current);
    }
    // SAFETY: just published, retired only by a later writer under the lock.
    unsafe { &*grown }
}

/// Under the lock: `entry`, unlinked just now, held until no reader can be in it.
fn retire_entry<K: Send + Sync + 'static, V: Send + Sync + 'static, A: Allocator + Clone + Send + Sync + 'static, L: ShardLock>(
    shard: &Shard<K, V, A, L>,
    writer: &mut Writer,
    alloc: &A,
    entry: *mut Entry<K, V>,
    plant: impl Planted<Plant>,
) {
    // SAFETY: unlinked under the lock, by this writer alone: the limbo is its one owner now.
    // Boxed in the map's allocator, which `alloc` is a handle of.
    let entry = unsafe { Box::from_raw_in(entry, alloc.clone()) };
    // The planted bug frees the entry at once as far as the model can tell (its bytes marked
    // freed), and leaks the box, so the test reads no memory that is really freed.
    if plant.has(Plant::FreeAtOnce) {
        sync::freed(ptr::from_ref(&*entry));
        core::mem::forget(entry);
        return;
    }
    writer.limbo.retire(entry);
    shard.held.fetch_add(1, Ordering::Relaxed);
}

/// Under the lock: `table`, unlinked just now, held until no reader can be in it. Its slots are
/// pointers: the entries are the map's or the limbo's, never the table's.
fn retire_table<K: Send + Sync + 'static, V: Send + Sync + 'static, A: Allocator + Clone + Send + Sync + 'static, L: ShardLock>(
    shard: &Shard<K, V, A, L>,
    writer: &mut Writer,
    alloc: &A,
    table: *mut Table<K, V, A>,
) {
    // SAFETY: unlinked under the lock, by this writer alone; boxed in the map's allocator.
    let table = unsafe { Box::from_raw_in(table, alloc.clone()) };
    writer.limbo.retire(table);
    shard.held.fetch_add(1, Ordering::Relaxed);
}

/// Under the lock: what passed its grace by `horizon`, taken out to be dropped once the lock is
/// let go.
fn collect<K, V, A: Allocator, L: ShardLock>(shard: &Shard<K, V, A, L>, writer: &mut Writer, horizon: Horizon) -> blaze_reclaim::Passed {
    let passed = writer.limbo.collect_passed(horizon);
    shard.held.store(writer.limbo.len(), Ordering::Relaxed);
    passed
}

/// A set shared between threads: a [`Map`] to `()`.
pub struct Set<K, S, A: Allocator + Clone, L: ShardLock>(Map<K, (), S, A, L>);

/// What a shared [`Set`] is made from ([`Set::new`]), as [`MapOpts`] for a map:
/// `Set::new(SetOpts { name: "names.seen", hasher: FixedState, alloc: Global, capacity: None })`.
///
/// A call's named arguments: write it as a literal in the call that takes it (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct SetOpts<S, A> {
    /// What a wait on one of the shards' writer locks is called: say what the set holds.
    pub name: &'static str,
    /// The hasher, as a value (D125).
    pub hasher: S,
    /// The allocator, as [`MapOpts::alloc`].
    pub alloc: A,
    /// Keys room is made for at once, as [`MapOpts::capacity`].
    pub capacity: Option<usize>,
}

impl<K, S, A: Allocator + Clone, L: ShardLock> Set<K, S, A, L> {
    /// The empty set `opts` describe, as [`Map::new`].
    pub fn new(opts: SetOpts<S, A>) -> Set<K, S, A, L> {
        Set(Map::new(MapOpts {
            name: opts.name,
            hasher: opts.hasher,
            alloc: opts.alloc,
            capacity: opts.capacity,
        }))
    }
}

impl<K, S: Default, A: Allocator + Clone, L: ShardLock> Set<K, S, A, L> {
    /// An empty set on `S::default()` in `alloc`, its writer locks named `name`.
    ///
    /// D126: goes at the sweep; write `Set::new(SetOpts { name, hasher, alloc, capacity: None })`.
    pub fn new_in(name: &'static str, alloc: A) -> Set<K, S, A, L> {
        Set::new(SetOpts { name, hasher: S::default(), alloc, capacity: None })
    }
}

impl<K, S, A: Allocator + Clone, L: ShardLock> Set<K, S, A, L> {
    /// An empty set on `hasher` in `alloc`, its writer locks named `name`.
    ///
    /// D126: goes at the sweep; write `Set::new(SetOpts { name, hasher, alloc, capacity: None })`.
    pub fn with_hasher_in(name: &'static str, hasher: S, alloc: A) -> Set<K, S, A, L> {
        Set::new(SetOpts { name, hasher, alloc, capacity: None })
    }

    pub fn pin(&self) -> PinnedSet<'_, K, S, A, L> {
        PinnedSet(self.0.pin())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K: fmt::Debug, S, A: Allocator + Clone, L: ShardLock> fmt::Debug for Set<K, S, A, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pinned = self.pin();
        f.debug_set().entries(pinned.iter()).finish()
    }
}

/// A [`Set`] lent to this thread's task.
pub struct PinnedSet<'m, K, S, A: Allocator + Clone, L: ShardLock>(Pinned<'m, K, (), S, A, L>);

impl<K, S, A: Allocator + Clone, L: ShardLock> PinnedSet<'_, K, S, A, L> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &K> + '_ {
        self.0.keys()
    }
}

impl<K, S, A: Allocator + Clone, L: ShardLock> PinnedSet<'_, K, S, A, L> {
    pub fn contains<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> bool
    where
        S: KeyHash<Q>,
    {
        self.0.contains_key(key)
    }

    pub fn get<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<&K>
    where
        S: KeyHash<Q>,
    {
        self.0.get_key_value(key).map(|(key, ())| key)
    }

    /// Starts loading the slot a [`PinnedSet::get`] of `key` reads first, as
    /// [`Pinned::prefetch`].
    pub fn prefetch<Q: ?Sized>(&self, key: &Q)
    where
        S: KeyHash<Q>,
    {
        self.0.prefetch(key);
    }

    /// [`PinnedSet::get`] of each of `keys`, as [`Pinned::get_many`] batches them.
    pub fn get_many<Q: ?Sized + Equivalent<K>, const N: usize>(
        &self,
        keys: [&Q; N],
    ) -> [Option<&K>; N]
    where
        S: KeyHash<Q>,
    {
        let mut out = [None; N];
        self.0.each_entry::<Q, N>(
            N,
            |at| keys[at],
            |at, entry| out[at] = entry.map(|entry| &entry.key),
        );
        out
    }
}

impl<K: Eq + Send + Sync + 'static, S: KeyHash<K>, A: Allocator + Clone + Send + Sync + 'static, L: ShardLock>
    PinnedSet<'_, K, S, A, L>
{
    /// Puts `key`; whether it was not held.
    pub fn insert(&self, key: K) -> bool {
        self.0.try_insert(key, ()).is_ok()
    }

    /// Takes `key` out; whether it was held.
    pub fn remove<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> bool
    where
        S: KeyHash<Q>,
    {
        self.0.remove(key).is_some()
    }

    pub fn retain(&self, mut keep: impl FnMut(&K) -> bool) {
        self.0.retain(|key, ()| keep(key));
    }

    pub fn clear(&self) {
        self.0.clear();
    }
}
