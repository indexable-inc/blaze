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

use core::{
    fmt,
    hash::{BuildHasher, Hash},
    marker::PhantomData,
    ptr,
};
use std::{alloc::Global, boxed::Box, vec::Vec};

use blaze_reclaim::{Guard, Horizon, Limbo};

use crate::Equivalent;

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

    #[cfg(not(blaze_check))]
    #[expect(
        clippy::disallowed_types,
        reason = "below blaze-core: blaze-hash is a dependency of blaze-core, whose named locks it cannot take; a shard's writers' lock, which no reader takes"
    )]
    pub(super) type Lock<T> = std::sync::Mutex<T>;

    #[cfg(blaze_check)]
    pub(super) type Lock<T> = blaze_check::sync::Mutex<T>;

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

use sync::{AtomicPtr, AtomicUsize, Lock, Ordering};

/// A bug a model test plants, to show its model catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// An entry's slot stored Relaxed: a reader that loads it is not ordered after the
    /// entry's bytes.
    #[cfg(test)]
    RelaxedPublish,
    /// A replaced entry freed at once, not retired: a reader in it reads freed bytes.
    #[cfg(test)]
    FreeAtOnce,
}

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

/// A shard's slots: a power of two of them, each empty (null), an entry, or a tombstone.
struct Table<K, V> {
    mask: usize,
    slots: Box<[AtomicPtr<Entry<K, V>>], Global>,
}

/// What a removed entry's slot holds: never dereferenced, never an allocation's address.
fn tombstone<K, V>() -> *mut Entry<K, V> {
    ptr::without_provenance_mut(1)
}

/// A slot that holds an entry.
fn live<K, V>(at: *mut Entry<K, V>) -> bool {
    !at.is_null() && at != tombstone()
}

impl<K, V> Table<K, V> {
    fn new(capacity: usize) -> Table<K, V> {
        debug_assert!(capacity.is_power_of_two());
        let mut slots = Vec::with_capacity_in(capacity, Global);
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
struct Shard<K, V> {
    table: AtomicPtr<Table<K, V>>,
    len: AtomicUsize,
    /// How many values `limbo` holds, read before the lock to skip the scan of a collect.
    held: AtomicUsize,
    writer: Lock<Writer>,
}

impl<K, V> Shard<K, V> {
    fn new() -> Shard<K, V> {
        Shard {
            table: AtomicPtr::new(ptr::null_mut()),
            len: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
            writer: Lock::new(Writer { used: 0, limbo: Limbo::new() }),
        }
    }

    /// The current table, for a reader inside a pin: `None` before the first insert.
    fn current(&self) -> Option<&Table<K, V>> {
        // Acquire: the table's slots, written before its Release publish.
        let table = self.table.load(Ordering::Acquire);
        // SAFETY: a published table is freed only once retired and its grace passed, which
        // the caller's pin holds back.
        (!table.is_null()).then(|| unsafe { &*table })
    }
}

/// A table shared between threads; see the module docs. Made by `Default` (`SharedMap::default()`)
/// or [`Map::with_hasher`].
pub struct Map<K, V, S> {
    shards: Box<[Shard<K, V>], Global>,
    hasher: S,
    _owns: PhantomData<Box<Entry<K, V>>>,
}

// SAFETY: the map owns its keys and values (boxed entries) and hands them to whichever thread
// drops the map or an entry's grace passes on, so they are `Send`; any thread may read them
// through a pin, so they are `Sync`. The rest is atomics and a lock.
unsafe impl<K: Send + Sync, V: Send + Sync, S: Send> Send for Map<K, V, S> {}

// SAFETY: as for `Send`: a shared map lends `&K` and `&V` to every thread and moves them to
// the thread whose write unlinks them.
unsafe impl<K: Send + Sync, V: Send + Sync, S: Sync> Sync for Map<K, V, S> {}

impl<K, V, S: Default> Default for Map<K, V, S> {
    fn default() -> Map<K, V, S> {
        Map::with_hasher(S::default())
    }
}

impl<K, V, S> Map<K, V, S> {
    /// An empty table on `hasher`: the shards, and no slot until a first insert.
    pub fn with_hasher(hasher: S) -> Map<K, V, S> {
        let mut shards = Vec::with_capacity_in(SHARDS, Global);
        shards.resize_with(SHARDS, Shard::new);
        Map { shards: shards.into_boxed_slice(), hasher, _owns: PhantomData }
    }

    /// Lends the table to this thread's task: what it reads stays valid until the pin drops.
    /// The pin may be held across a wait (`blaze_reclaim::enter`).
    pub fn pin(&self) -> Pinned<'_, K, V, S> {
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

impl<K, V, S> Drop for Map<K, V, S> {
    fn drop(&mut self) {
        for shard in &*self.shards {
            // Relaxed: `&mut self`, so every store to it happened before.
            let table = shard.table.load(Ordering::Relaxed);
            if table.is_null() {
                continue;
            }
            // SAFETY: the map is dropped, so no pin of it lives: the current table and its
            // live entries are this drop's alone (retired ones are the limbo's, dropped with it).
            let table = unsafe { Box::from_raw_in(table, Global) };
            for slot in &*table.slots {
                let entry = slot.load(Ordering::Relaxed);
                if live(entry) {
                    // SAFETY: as above; each live entry is in one slot of the current table.
                    drop(unsafe { Box::from_raw_in(entry, Global) });
                }
            }
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S> fmt::Debug for Map<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pinned = self.pin();
        f.debug_map().entries(pinned.iter()).finish()
    }
}

/// A [`Map`] lent to this thread's task ([`Map::pin`]); what it lends lives as long as it does.
pub struct Pinned<'m, K, V, S> {
    map: &'m Map<K, V, S>,
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

impl<K, V, S> Pinned<'_, K, V, S> {
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

impl<K, V, S: BuildHasher> Pinned<'_, K, V, S> {
    fn shard_of(&self, hash: u64) -> &Shard<K, V> {
        &self.map.shards[(hash >> (u64::BITS - SHARDS.trailing_zeros())) as usize]
    }

    fn entry<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<&Entry<K, V>> {
        let hash = self.map.hasher.hash_one(key);
        self.shard_of(hash).current()?.find(hash, key)
    }

    /// The value of `key`, read with no lock.
    pub fn get<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<&V> {
        self.entry(key).map(|entry| &entry.value)
    }

    /// The key and value of `key`, read with no lock.
    pub fn get_key_value<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<(&K, &V)> {
        self.entry(key).map(|entry| (&entry.key, &entry.value))
    }

    pub fn contains_key<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> bool {
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

impl<K: Hash + Eq + Send + Sync + 'static, V: Send + Sync + 'static, S: BuildHasher> Pinned<'_, K, V, S> {
    /// Puts `value` for `key`; the value it replaced, readable while this pin lives.
    pub fn insert(&self, key: K, value: V) -> Option<&V> {
        match self.write(key, value, Held::Replace, Plant::None) {
            Wrote::Replaced(old) => Some(&old.value),
            Wrote::Inserted(_) | Wrote::Kept(..) => None,
        }
    }

    /// Puts `value` for `key` unless it is held; else what is held, and `value` back.
    pub fn try_insert(&self, key: K, value: V) -> Result<&V, OccupiedError<'_, V>> {
        match self.write(key, value, Held::Keep, Plant::None) {
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

    /// The value of `key`, or else `make()`'s, put now; `make` runs outside the shard's lock,
    /// and its value is dropped if another thread put the key first.
    pub fn get_or_insert_with(&self, key: K, make: impl FnOnce() -> V) -> &V {
        if let Some(held) = self.get(&key) {
            return held;
        }
        self.get_or_insert(key, make())
    }

    /// Takes `key` out; its value, readable while this pin lives.
    pub fn remove<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<&V> {
        self.remove_where(key, |_, _| true).ok().flatten().map(|(_, value)| value)
    }

    /// Takes `key` out if `should(key, value)`: `Ok` with what was taken (or `None` when the
    /// key is not held), `Err` with what stays.
    pub fn remove_if<Q: ?Sized + Hash + Equivalent<K>>(
        &self,
        key: &Q,
        should: impl FnOnce(&K, &V) -> bool,
    ) -> Result<Option<(&K, &V)>, (&K, &V)> {
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
                            retire_entry(shard, &mut writer, entry, Plant::None);
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
                    retire_entry(shard, &mut writer, entry, Plant::None);
                }
            }
            shard.len.store(0, Ordering::Relaxed);
            writer.used = 0;
            retire_table(shard, &mut writer, table);
        }
    }

    /// The scan of every thread's epoch slot, taken before the lock (`blaze_reclaim::scans`),
    /// when the shard holds enough retired values to free.
    fn horizon(&self, shard: &Shard<K, V>) -> Option<Horizon> {
        (shard.held.load(Ordering::Relaxed) >= COLLECT_AT).then(Horizon::now)
    }

    fn remove_where<Q: ?Sized + Hash + Equivalent<K>>(
        &self,
        key: &Q,
        should: impl FnOnce(&K, &V) -> bool,
    ) -> Result<Option<(&K, &V)>, (&K, &V)> {
        let hash = self.map.hasher.hash_one(key);
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
                        retire_entry(shard, &mut writer, entry, Plant::None);
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
    fn write(&self, key: K, value: V, held: Held, plant: Plant) -> Wrote<'_, K, V> {
        let hash = self.map.hasher.hash_one(&key);
        let shard = self.shard_of(hash);
        let horizon = self.horizon(shard);
        let (wrote, passed) = {
            let mut writer = lock(shard);
            let table = room(shard, &mut writer);
            let (at, holds) = table.place(hash, &key);
            let wrote = if holds && held == Held::Keep {
                let entry = table.slots[at].load(Ordering::Relaxed);
                // SAFETY: a live slot of the current table, under the lock; this pin keeps it.
                Wrote::Kept(unsafe { &*entry }, value)
            } else {
                let fresh = Box::into_raw(Box::new_in(Entry { hash, key, value }, Global));
                sync::wrote(fresh.cast_const());
                let publish = match plant {
                    #[cfg(test)]
                    Plant::RelaxedPublish => Ordering::Relaxed,
                    _ => Ordering::Release,
                };
                let old = table.slots[at].swap(fresh, publish);
                // SAFETY: just published, and freed only once retired: this pin keeps it.
                let fresh_ref = unsafe { &*fresh };
                if live(old) {
                    retire_entry(shard, &mut writer, old, plant);
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
    pub(crate) fn insert_planted(&self, key: K, value: V, plant: Plant) -> Option<&V> {
        match self.write(key, value, Held::Replace, plant) {
            Wrote::Replaced(old) => Some(&old.value),
            Wrote::Inserted(_) | Wrote::Kept(..) => None,
        }
    }
}

/// Takes `shard`'s writers' lock; one a panicking writer left is taken as it was left, which
/// is whole: a writer changes a slot with one store, after anything that can panic.
fn lock<K, V>(shard: &Shard<K, V>) -> impl core::ops::DerefMut<Target = Writer> + '_ {
    shard.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Under the lock: the current table, made or grown so that one more slot can be used.
fn room<'s, K: Send + Sync + 'static, V: Send + Sync + 'static>(
    shard: &'s Shard<K, V>,
    writer: &mut Writer,
) -> &'s Table<K, V> {
    let current = shard.table.load(Ordering::Relaxed);
    // SAFETY: the current table, under the lock: only this writer can retire it.
    let table = (!current.is_null()).then(|| unsafe { &*current });
    let capacity = table.map_or(0, |it| it.slots.len());
    if (writer.used + 1) * 4 <= capacity * 3 {
        return table.expect("a table with room");
    }
    let len = shard.len.load(Ordering::Relaxed);
    let grown = Table::new(((len + 1) * 2).next_power_of_two().max(SMALLEST));
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
    let grown = Box::into_raw(Box::new_in(grown, Global));
    // Release: the slots above, before a reader that loads the table reads them.
    shard.table.store(grown, Ordering::Release);
    if !current.is_null() {
        retire_table(shard, writer, current);
    }
    // SAFETY: just published, retired only by a later writer under the lock.
    unsafe { &*grown }
}

/// Under the lock: `entry`, unlinked just now, held until no reader can be in it.
fn retire_entry<K: Send + Sync + 'static, V: Send + Sync + 'static>(
    shard: &Shard<K, V>,
    writer: &mut Writer,
    entry: *mut Entry<K, V>,
    plant: Plant,
) {
    // SAFETY: unlinked under the lock, by this writer alone: the limbo is its one owner now.
    let entry = unsafe { Box::from_raw_in(entry, Global) };
    // The planted bug frees the entry at once as far as the model can tell (its bytes marked
    // freed), and leaks the box, so the test reads no memory that is really freed.
    #[cfg(test)]
    if plant == Plant::FreeAtOnce {
        sync::freed(ptr::from_ref(&*entry));
        core::mem::forget(entry);
        return;
    }
    _ = plant;
    writer.limbo.retire(entry);
    shard.held.fetch_add(1, Ordering::Relaxed);
}

/// Under the lock: `table`, unlinked just now, held until no reader can be in it. Its slots are
/// pointers: the entries are the map's or the limbo's, never the table's.
fn retire_table<K: Send + Sync + 'static, V: Send + Sync + 'static>(
    shard: &Shard<K, V>,
    writer: &mut Writer,
    table: *mut Table<K, V>,
) {
    // SAFETY: unlinked under the lock, by this writer alone.
    let table = unsafe { Box::from_raw_in(table, Global) };
    writer.limbo.retire(table);
    shard.held.fetch_add(1, Ordering::Relaxed);
}

/// Under the lock: what passed its grace by `horizon`, taken out to be dropped once the lock is
/// let go.
fn collect<K, V>(shard: &Shard<K, V>, writer: &mut Writer, horizon: Horizon) -> blaze_reclaim::Passed {
    let passed = writer.limbo.collect_passed(horizon);
    shard.held.store(writer.limbo.len(), Ordering::Relaxed);
    passed
}

/// A set shared between threads: a [`Map`] to `()`.
pub struct Set<K, S>(Map<K, (), S>);

impl<K, S: Default> Default for Set<K, S> {
    fn default() -> Set<K, S> {
        Set(Map::default())
    }
}

impl<K, S> Set<K, S> {
    pub fn with_hasher(hasher: S) -> Set<K, S> {
        Set(Map::with_hasher(hasher))
    }

    pub fn pin(&self) -> PinnedSet<'_, K, S> {
        PinnedSet(self.0.pin())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K: fmt::Debug, S> fmt::Debug for Set<K, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pinned = self.pin();
        f.debug_set().entries(pinned.iter()).finish()
    }
}

/// A [`Set`] lent to this thread's task.
pub struct PinnedSet<'m, K, S>(Pinned<'m, K, (), S>);

impl<K, S> PinnedSet<'_, K, S> {
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

impl<K, S: BuildHasher> PinnedSet<'_, K, S> {
    pub fn contains<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> bool {
        self.0.contains_key(key)
    }

    pub fn get<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<&K> {
        self.0.get_key_value(key).map(|(key, ())| key)
    }
}

impl<K: Hash + Eq + Send + Sync + 'static, S: BuildHasher> PinnedSet<'_, K, S> {
    /// Puts `key`; whether it was not held.
    pub fn insert(&self, key: K) -> bool {
        self.0.try_insert(key, ()).is_ok()
    }

    /// Takes `key` out; whether it was held.
    pub fn remove<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> bool {
        self.0.remove(key).is_some()
    }

    pub fn retain(&self, mut keep: impl FnMut(&K) -> bool) {
        self.0.retain(|key, ()| keep(key));
    }

    pub fn clear(&self) {
        self.0.clear();
    }
}
