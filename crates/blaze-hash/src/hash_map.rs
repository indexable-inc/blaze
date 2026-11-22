//! The single-owner map, [`Map`], and its entry and iterator types: this crate's own SwissTable
//! ([`crate::raw`], on `blaze_swiss`'s probe), on a hasher of this crate's, in the allocator the
//! type names (AGENTS.md rule 11, bump-allocate). [`HashMap`](crate::HashMap),
//! [`ByHash`](crate::ByHash) and [`KeyedMap`](crate::KeyedMap) are this on
//! [`FixedState`](crate::FixedState), [`BuildKeyHasher`](crate::BuildKeyHasher) or a
//! [`SipKey`](crate::SipKey).
//!
//! The allocator has no default: a table that lives within one task is in its frame
//! (`HashMap<K, V, &Frame>`, from `blaze_mem::frame::current`), a longer one in the request,
//! revision or image allocator, and one on the global heap names `Global`. The hasher is named
//! too (decision D125), and both are fields of the one constructor's options (D126, rule 83):
//! `Map::new(MapOpts { hasher: FixedState, alloc: frame, capacity: None })`, `Some(n)` to
//! reserve.
//! A map of an iterator's pairs is `new` then `extend`. The older names (`new_in`,
//! `with_capacity_in`, `from_iter_in`, `with_hasher_in`, `with_capacity_and_hasher_in`) are
//! calls of `new` and go at the sweep.
//!
//! The methods are the ones the tree uses of the hashbrown map this replaces, with the same
//! names and meanings; its iteration order is the table's slot order, a function of the inserts,
//! the removals and the hasher, the same on every run.

use core::{alloc::Allocator, fmt, iter::FusedIterator, ops::Index};

use blaze_own::AllocFailure;

use crate::{
    Equivalent, KeyHash,
    raw::{self, RawTable},
};

/// A map on the hasher `S`, in the allocator `A`; it hashes through [`KeyHash`], a
/// `BuildHasher`'s `hash_one`, or for `FixedState` and a [`PlainKey`](crate::PlainKey) the
/// key's bytes.
pub struct Map<K, V, S, A: Allocator> {
    pub(crate) table: RawTable<(K, V), A>,
    pub(crate) hasher: S,
}

/// What hashes a held entry when the table is made again: its key, by `hasher`.
#[inline]
fn by_key<K, V, S: KeyHash<K>>(hasher: &S) -> impl Fn(&(K, V)) -> u64 + '_ {
    move |entry| hasher.hash_key(&entry.0)
}
/// What a [`Map`] is made from ([`Map::new`], rule 83 options-struct): every field is written at
/// every use, the hasher too (D125): `Map::new(MapOpts { hasher: FixedState, alloc: frame,
/// capacity: None })`.
///
/// A call's named arguments: write it as a literal in the call that takes it (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct MapOpts<S, A> {
    /// The hasher, as a value: `FixedState` for ordinary keys, a [`SipKey`] for keys an
    /// outsider picks, `BuildKeyHasher::default()` for keys that are hashes already.
    ///
    /// [`SipKey`]: crate::SipKey
    pub hasher: S,
    /// The allocator the table's memory is in.
    pub alloc: A,
    /// Entries room is made for at once: `None` allocates nothing until the first insert,
    /// `Some(n)` reserves room for `n`.
    pub capacity: Option<usize>,
}

impl<K, V, S, A: Allocator> Map<K, V, S, A> {
    /// The map `opts` describe: empty, on `opts.hasher` in `opts.alloc`, with room for
    /// `opts.capacity` entries; a refused allocation is a panic naming it ([`Map::try_new`]
    /// returns it). A map of an iterator's pairs is `new` then [`Extend::extend`].
    #[inline]
    pub fn new(opts: MapOpts<S, A>) -> Self {
        let table = RawTable::with_capacity_in(opts.capacity.unwrap_or(0), opts.alloc);
        Map { table, hasher: opts.hasher }
    }

    /// [`Map::new`], with a refused allocation of `opts.capacity` returned.
    #[inline]
    pub fn try_new(opts: MapOpts<S, A>) -> Result<Self, AllocFailure> {
        let table = RawTable::try_with_capacity_in(opts.capacity.unwrap_or(0), opts.alloc)?;
        Ok(Map { table, hasher: opts.hasher })
    }
}

impl<K, V, S: Default, A: Allocator> Map<K, V, S, A> {
    /// An empty map in `alloc` on `S::default()`.
    ///
    /// D126: goes at the sweep; write `Map::new(MapOpts { hasher, alloc, capacity: None })`.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Map::new(MapOpts { hasher: S::default(), alloc, capacity: None })
    }

    /// An empty map in `alloc` on `S::default()` with room for `capacity` entries.
    ///
    /// D126: goes at the sweep; write `Map::new(MapOpts { hasher, alloc, capacity: Some(n) })`.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        Map::new(MapOpts { hasher: S::default(), alloc, capacity: Some(capacity) })
    }
}

impl<K: Eq, V, S: KeyHash<K> + Default, A: Allocator> Map<K, V, S, A> {
    /// The map of `iter`'s pairs in `alloc` on `S::default()`, a later pair replacing an
    /// earlier one's value.
    ///
    /// D126: goes at the sweep; write `Map::new(MapOpts { hasher, alloc, capacity })`, then
    /// `extend`.
    #[inline]
    pub fn from_iter_in<I: IntoIterator<Item = (K, V)>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let capacity = Some(iter.size_hint().0);
        let mut map = Map::new(MapOpts { hasher: S::default(), alloc, capacity });
        map.extend(iter);
        map
    }
}

impl<K, V, S, A: Allocator> Map<K, V, S, A> {
    /// An empty map on `hasher` in `alloc`, in a `const` too. Its own body, not a call of
    /// [`Map::new`], because `new` is not `const` (it may allocate) and a `static` may be made
    /// by this one.
    ///
    /// D126: goes at the sweep where it is not in a `const`; write
    /// `Map::new(MapOpts { hasher, alloc, capacity: None })`.
    #[inline]
    pub const fn with_hasher_in(hasher: S, alloc: A) -> Self {
        Map { table: RawTable::new_in(alloc), hasher }
    }

    /// An empty map on `hasher` in `alloc` with room for `capacity` entries.
    ///
    /// D126: goes at the sweep; write `Map::new(MapOpts { hasher, alloc, capacity: Some(n) })`.
    #[inline]
    pub fn with_capacity_and_hasher_in(capacity: usize, hasher: S, alloc: A) -> Self {
        Map::new(MapOpts { hasher, alloc, capacity: Some(capacity) })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.table.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.table.len() == 0
    }

    /// The entries it holds before its table is made again.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.table.capacity()
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        &self.hasher
    }

    #[inline]
    pub fn allocator(&self) -> &A {
        self.table.allocator()
    }

    /// Removes every entry and keeps the memory.
    #[inline]
    pub fn clear(&mut self) {
        self.table.clear();
    }

    /// The entries, in the table's order.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter(self.table.iter())
    }

    /// The entries, their values to change.
    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut(self.table.iter_mut())
    }

    #[inline]
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys(self.table.iter())
    }

    #[inline]
    pub fn values(&self) -> Values<'_, K, V> {
        Values(self.table.iter())
    }

    #[inline]
    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        ValuesMut(self.table.iter_mut())
    }

    /// The keys, by value.
    #[inline]
    pub fn into_keys(self) -> IntoKeys<K, V, A> {
        IntoKeys(self.table.into_iter())
    }

    /// The values, by value.
    #[inline]
    pub fn into_values(self) -> IntoValues<K, V, A> {
        IntoValues(self.table.into_iter())
    }

    /// Every entry taken out; the memory is kept.
    #[inline]
    pub fn drain(&mut self) -> Drain<'_, K, V, A> {
        Drain(self.table.drain())
    }

    /// Keeps the entries `keep` says to.
    #[inline]
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        self.table.retain(|(key, value)| keep(key, value));
    }
}

impl<K: Eq, V, S: KeyHash<K>, A: Allocator> Map<K, V, S, A> {
    /// Room for `additional` more entries; a refused allocation is a panic naming it.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.table.reserve(additional, by_key(&self.hasher));
    }

    /// [`Map::reserve`], the refusal returned.
    #[inline]
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.table.try_reserve(additional, by_key(&self.hasher))
    }

    /// The smallest table that holds the entries.
    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.table.shrink_to(0, by_key(&self.hasher));
    }

    /// The smallest table that holds the entries and `min` in all.
    #[inline]
    pub fn shrink_to(&mut self, min: usize) {
        self.table.shrink_to(min, by_key(&self.hasher));
    }

    /// The slot of `key`.
    #[inline]
    fn find<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<usize>
    where
        S: KeyHash<Q>,
    {
        // Named, as `S` is `KeyHash` of both `K` and `Q` here.
        let hash = <S as KeyHash<Q>>::hash_key(&self.hasher, key);
        self.table.find(hash, |(held, _)| key.equivalent(held))
    }

    #[inline]
    pub fn get<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<&V>
    where
        S: KeyHash<Q>,
    {
        let index = self.find(key)?;
        Some(&self.table.get(index).1)
    }

    /// Starts loading what a [`Map::get`] of `key` reads first (its home control word and,
    /// when a group's eight entries fit in two cache lines, those entries), for a caller that
    /// interleaves its own lookups: issue it a few lookups ahead of the `get`. A hint: it
    /// changes nothing and reads no entry.
    #[inline]
    pub fn prefetch<Q: ?Sized>(&self, key: &Q)
    where
        S: KeyHash<Q>,
    {
        self.table.prefetch(<S as KeyHash<Q>>::hash_key(&self.hasher, key));
    }

    /// The slots of `keys`, by group prefetching: every key hashed and its home group's load
    /// started, then each probed in order, so on a table larger than the cache the batch's
    /// misses overlap.
    #[inline]
    pub(crate) fn find_many<Q: ?Sized + Equivalent<K>, const N: usize>(
        &self,
        keys: [&Q; N],
    ) -> [Option<usize>; N]
    where
        S: KeyHash<Q>,
    {
        let hashes = keys.map(|key| <S as KeyHash<Q>>::hash_key(&self.hasher, key));
        self.table.prefetch_all(&hashes);
        core::array::from_fn(|at| {
            self.table.find(hashes[at], |(held, _)| keys[at].equivalent(held))
        })
    }

    /// The slot of each of `keys`, `answer(at, slot)` for `keys[at]` in order, as a pipeline:
    /// key `at + raw::BATCH` is hashed and its home prefetched as key `at` is probed, so the
    /// misses in flight stay [`raw::BATCH`] deep with no stall between batches.
    #[inline]
    pub(crate) fn find_each<Q: Equivalent<K>>(
        &self,
        keys: &[Q],
        mut answer: impl FnMut(usize, Option<usize>),
    ) where
        S: KeyHash<Q>,
    {
        let hash = |key: &Q| <S as KeyHash<Q>>::hash_key(&self.hasher, key);
        // `ring[at % BATCH]` is key `at`'s hash, from its prefetch to its probe.
        let mut ring = [0u64; raw::BATCH];
        for (at, key) in keys.iter().take(raw::BATCH).enumerate() {
            ring[at] = hash(key);
            self.table.prefetch(ring[at]);
        }
        for (at, key) in keys.iter().enumerate() {
            let this = ring[at % raw::BATCH];
            if let Some(ahead) = keys.get(at + raw::BATCH) {
                let ahead = hash(ahead);
                self.table.prefetch(ahead);
                ring[at % raw::BATCH] = ahead;
            }
            answer(at, self.table.find(this, |(held, _)| key.equivalent(held)));
        }
    }

    /// [`Map::get`] of each of `keys`, as one batch: on a table larger than the cache, the
    /// lookups' cache misses overlap instead of queueing. Duplicates and absent keys are
    /// answered as `get` answers them.
    ///
    /// When it pays (`examples/hash_cost.rs batch`, `HashMap<u64, u32>` of 10^7 entries on an
    /// M-series Mac, against `get` one at a time), by what the caller does with each answer:
    /// - work between lookups (about 200 instructions a key, so the next lookup is past the
    ///   out-of-order window): batches of 4 to 8, 1.17 times;
    /// - a second load from the answer (an id, then its record), with the batch's records
    ///   prefetched before any is read: batches of 16 to 32, 1.33 times;
    /// - a tight loop of independent lookups (the control): 1.16 times at 4 for held keys,
    ///   and 0.65 to 0.83 times for absent ones (the slot line prefetched is wasted).
    ///
    /// It loses on a table the cache holds (10^3 to 10^5 entries: 0.83 to 1.03 times in
    /// every shape), so it is for tables past the cache and keys mostly held.
    #[inline]
    pub fn get_many<Q: ?Sized + Equivalent<K>, const N: usize>(
        &self,
        keys: [&Q; N],
    ) -> [Option<&V>; N]
    where
        S: KeyHash<Q>,
    {
        self.find_many(keys).map(|slot| slot.map(|index| &self.table.get(index).1))
    }

    /// [`Map::get_many`] of a slice: `out[at]` is the value of `keys[at]`, each key's home
    /// prefetched eight keys before its probe. Panics unless `out` is as long as `keys`.
    pub fn get_many_into<'a, Q: Equivalent<K>>(&'a self, keys: &[Q], out: &mut [Option<&'a V>])
    where
        S: KeyHash<Q>,
    {
        assert_eq!(keys.len(), out.len(), "Map::get_many_into: one answer per key");
        self.find_each(keys, |at, slot| out[at] = slot.map(|index| &self.table.get(index).1));
    }

    #[inline]
    pub fn get_mut<Q: ?Sized + Equivalent<K>>(&mut self, key: &Q) -> Option<&mut V>
    where
        S: KeyHash<Q>,
    {
        let index = self.find(key)?;
        Some(&mut self.table.get_mut(index).1)
    }

    #[inline]
    pub fn get_key_value<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> Option<(&K, &V)>
    where
        S: KeyHash<Q>,
    {
        let index = self.find(key)?;
        let (key, value) = self.table.get(index);
        Some((key, value))
    }

    #[inline]
    pub fn contains_key<Q: ?Sized + Equivalent<K>>(&self, key: &Q) -> bool
    where
        S: KeyHash<Q>,
    {
        self.find(key).is_some()
    }

    /// `value` under `key`: the old value if the key was held (the held key is kept).
    #[inline]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.entry(key) {
            Entry::Occupied(mut entry) => Some(entry.insert(value)),
            Entry::Vacant(entry) => {
                entry.insert(value);
                None
            }
        }
    }

    #[inline]
    pub fn remove<Q: ?Sized + Equivalent<K>>(&mut self, key: &Q) -> Option<V>
    where
        S: KeyHash<Q>,
    {
        self.remove_entry(key).map(|(_, value)| value)
    }

    #[inline]
    pub fn remove_entry<Q: ?Sized + Equivalent<K>>(&mut self, key: &Q) -> Option<(K, V)>
    where
        S: KeyHash<Q>,
    {
        let index = self.find(key)?;
        Some(self.table.remove(index))
    }

    /// The slot of `key`, or else of the entry `make(key)` builds, put now: one hash and one
    /// probe either way (a probe that misses ends at the slot the entry takes), and the owned
    /// key and value made only on a miss. Allocates only on a miss into a full table.
    #[inline]
    pub(crate) fn try_slot_or_insert_with<Q: ?Sized + Equivalent<K>>(
        &mut self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> Result<usize, AllocFailure>
    where
        S: KeyHash<Q>,
    {
        let hash = <S as KeyHash<Q>>::hash_key(&self.hasher, key);
        if self.table.growth_left() == 0 {
            // A miss grows the table, after which its vacancy is another: find first, so a
            // hit allocates nothing, and probe the grown table once on a miss.
            if let Some(index) = self.table.find(hash, |(held, _)| key.equivalent(held)) {
                return Ok(index);
            }
            self.table.try_reserve(1, by_key(&self.hasher))?;
        }
        match self.table.find_or_vacant(hash, |(held, _)| key.equivalent(held)) {
            Ok(index) => Ok(index),
            Err(vacancy) => {
                let entry = make(key);
                debug_assert!(
                    key.equivalent(&entry.0)
                        && <S as KeyHash<K>>::hash_key(&self.hasher, &entry.0) == hash,
                    "get_or_insert_with: `make` built a key that is not equal to the one asked"
                );
                Ok(self.table.insert_at(vacancy, hash, entry))
            }
        }
    }

    /// The value of `key`, or else the value of the entry `make(key)` builds, put now: the
    /// owned key and the value are made only on a miss, with one hash and one probe either
    /// way. What `map.entry(key.to_owned()).or_insert_with(..)` does, without the owned key
    /// on a hit. `make` must return a key equal to `key` (checked in debug builds); a refused
    /// allocation is a panic naming it.
    #[inline]
    pub fn get_or_insert_with<Q: ?Sized + Equivalent<K>>(
        &mut self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> &mut V
    where
        S: KeyHash<Q>,
    {
        match self.try_slot_or_insert_with(key, make) {
            Ok(index) => &mut self.table.get_mut(index).1,
            Err(failure) => raw::fail(failure),
        }
    }

    /// [`Map::get_or_insert_with`], a refused allocation returned (and `make` not called).
    #[inline]
    pub fn try_get_or_insert_with<Q: ?Sized + Equivalent<K>>(
        &mut self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> Result<&mut V, AllocFailure>
    where
        S: KeyHash<Q>,
    {
        let index = self.try_slot_or_insert_with(key, make)?;
        Ok(&mut self.table.get_mut(index).1)
    }

    /// The entry of `key`, to read, fill or change in place.
    #[inline]
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, S, A> {
        let hash = self.hasher.hash_key(&key);
        match self.table.find(hash, |(held, _)| *held == key) {
            Some(index) => Entry::Occupied(OccupiedEntry { map: self, index }),
            None => Entry::Vacant(VacantEntry { map: self, hash, key }),
        }
    }
}

/// A map's entry of one key: held or not.
pub enum Entry<'a, K, V, S, A: Allocator> {
    Occupied(OccupiedEntry<'a, K, V, S, A>),
    Vacant(VacantEntry<'a, K, V, S, A>),
}

/// An entry whose key is held.
pub struct OccupiedEntry<'a, K, V, S, A: Allocator> {
    map: &'a mut Map<K, V, S, A>,
    index: usize,
}

/// An entry whose key is not held: the key and its hash, to insert.
pub struct VacantEntry<'a, K, V, S, A: Allocator> {
    map: &'a mut Map<K, V, S, A>,
    hash: u64,
    key: K,
}

impl<'a, K, V, S: KeyHash<K>, A: Allocator> Entry<'a, K, V, S, A> {
    #[inline]
    pub fn or_insert(self, default: V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(default),
        }
    }

    #[inline]
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(make()),
        }
    }

    #[inline]
    pub fn or_insert_with_key(self, make: impl FnOnce(&K) -> V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let value = make(&entry.key);
                entry.insert(value)
            }
        }
    }

    #[inline]
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }

    /// `change` run on a held value; a vacant entry as it was.
    #[inline]
    pub fn and_modify(self, change: impl FnOnce(&mut V)) -> Self {
        match self {
            Entry::Occupied(mut entry) => {
                change(entry.get_mut());
                Entry::Occupied(entry)
            }
            Entry::Vacant(entry) => Entry::Vacant(entry),
        }
    }

    /// `value` under the key, held or not: the entry, now held.
    #[inline]
    pub fn insert(self, value: V) -> OccupiedEntry<'a, K, V, S, A> {
        match self {
            Entry::Occupied(mut entry) => {
                entry.insert(value);
                entry
            }
            Entry::Vacant(entry) => entry.insert_entry(value),
        }
    }
}

impl<K, V, S, A: Allocator> Entry<'_, K, V, S, A> {
    #[inline]
    pub fn key(&self) -> &K {
        match self {
            Entry::Occupied(entry) => entry.key(),
            Entry::Vacant(entry) => entry.key(),
        }
    }
}

impl<'a, K, V, S, A: Allocator> OccupiedEntry<'a, K, V, S, A> {
    #[inline]
    pub fn key(&self) -> &K {
        &self.map.table.get(self.index).0
    }

    #[inline]
    pub fn get(&self) -> &V {
        &self.map.table.get(self.index).1
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut V {
        &mut self.map.table.get_mut(self.index).1
    }

    /// The value, borrowed for as long as the map was.
    #[inline]
    pub fn into_mut(self) -> &'a mut V {
        let OccupiedEntry { map, index } = self;
        &mut map.table.get_mut(index).1
    }

    /// The key, borrowed for as long as the map was: what `Set::get_or_insert` returns.
    #[inline]
    pub(crate) fn into_key_ref(self) -> &'a K {
        let OccupiedEntry { map, index } = self;
        &map.table.get(index).0
    }

    /// Puts `value` in place of the held one, which it returns.
    #[inline]
    pub fn insert(&mut self, value: V) -> V {
        core::mem::replace(self.get_mut(), value)
    }

    #[inline]
    pub fn remove(self) -> V {
        self.remove_entry().1
    }

    #[inline]
    pub fn remove_entry(self) -> (K, V) {
        let OccupiedEntry { map, index } = self;
        map.table.remove(index)
    }
}

impl<'a, K, V, S, A: Allocator> VacantEntry<'a, K, V, S, A> {
    #[inline]
    pub fn key(&self) -> &K {
        &self.key
    }

    #[inline]
    pub fn into_key(self) -> K {
        self.key
    }
}

impl<'a, K, V, S: KeyHash<K>, A: Allocator> VacantEntry<'a, K, V, S, A> {
    /// `value` under the key: the value, borrowed for as long as the map was.
    #[inline]
    pub fn insert(self, value: V) -> &'a mut V {
        self.insert_entry(value).into_mut()
    }

    /// `value` under the key: the entry, now held.
    #[inline]
    pub fn insert_entry(self, value: V) -> OccupiedEntry<'a, K, V, S, A> {
        let VacantEntry { map, hash, key } = self;
        let index = {
            let Map { table, hasher } = &mut *map;
            table.insert_unique(hash, (key, value), by_key(&*hasher))
        };
        OccupiedEntry { map, index }
    }
}

impl<K: fmt::Debug, V, S, A: Allocator> fmt::Debug for VacantEntry<'_, K, V, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("VacantEntry").field(&self.key).finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S, A: Allocator> fmt::Debug for Entry<'_, K, V, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entry::Occupied(it) => f.debug_tuple("Entry").field(it).finish(),
            Entry::Vacant(it) => f.debug_tuple("Entry").field(it).finish(),
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S, A: Allocator> fmt::Debug for OccupiedEntry<'_, K, V, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OccupiedEntry").field("key", self.key()).field("value", self.get()).finish()
    }
}

/// A map's entries by reference.
pub struct Iter<'a, K, V>(raw::Iter<'a, (K, V)>);

impl<K, V> Clone for Iter<'_, K, V> {
    #[inline]
    fn clone(&self) -> Self {
        Iter(self.0.clone())
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        self.0.next().map(|(key, value)| (key, value))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's entries, their values to change.
pub struct IterMut<'a, K, V>(raw::IterMut<'a, (K, V)>);

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        self.0.next().map(|(key, value)| (&*key, value))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's keys.
pub struct Keys<'a, K, V>(raw::Iter<'a, (K, V)>);

impl<K, V> Clone for Keys<'_, K, V> {
    #[inline]
    fn clone(&self) -> Self {
        Keys(self.0.clone())
    }
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    #[inline]
    fn next(&mut self) -> Option<&'a K> {
        self.0.next().map(|(key, _)| key)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's values.
pub struct Values<'a, K, V>(raw::Iter<'a, (K, V)>);

impl<K, V> Clone for Values<'_, K, V> {
    #[inline]
    fn clone(&self) -> Self {
        Values(self.0.clone())
    }
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    #[inline]
    fn next(&mut self) -> Option<&'a V> {
        self.0.next().map(|(_, value)| value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's values to change.
pub struct ValuesMut<'a, K, V>(raw::IterMut<'a, (K, V)>);

impl<'a, K, V> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    #[inline]
    fn next(&mut self) -> Option<&'a mut V> {
        self.0.next().map(|(_, value)| value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's entries by value.
pub struct IntoIter<K, V, A: Allocator>(raw::IntoIter<(K, V), A>);

impl<K, V, A: Allocator> Iterator for IntoIter<K, V, A> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's keys by value.
pub struct IntoKeys<K, V, A: Allocator>(raw::IntoIter<(K, V), A>);

impl<K, V, A: Allocator> Iterator for IntoKeys<K, V, A> {
    type Item = K;

    #[inline]
    fn next(&mut self) -> Option<K> {
        self.0.next().map(|(key, _)| key)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's values by value.
pub struct IntoValues<K, V, A: Allocator>(raw::IntoIter<(K, V), A>);

impl<K, V, A: Allocator> Iterator for IntoValues<K, V, A> {
    type Item = V;

    #[inline]
    fn next(&mut self) -> Option<V> {
        self.0.next().map(|(_, value)| value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// A map's entries taken out by [`Map::drain`].
pub struct Drain<'a, K, V, A: Allocator>(raw::Drain<'a, (K, V), A>);

impl<K, V, A: Allocator> Iterator for Drain<'_, K, V, A> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

macro_rules! exact {
    ($($ty:ty),* $(,)?) => {$(
        impl<K, V> ExactSizeIterator for $ty {}
        impl<K, V> FusedIterator for $ty {}
    )*};
}

exact!(Iter<'_, K, V>, IterMut<'_, K, V>, Keys<'_, K, V>, Values<'_, K, V>, ValuesMut<'_, K, V>);

macro_rules! exact_in {
    ($($ty:ty),* $(,)?) => {$(
        impl<K, V, A: Allocator> ExactSizeIterator for $ty {}
        impl<K, V, A: Allocator> FusedIterator for $ty {}
    )*};
}

exact_in!(IntoIter<K, V, A>, IntoKeys<K, V, A>, IntoValues<K, V, A>, Drain<'_, K, V, A>);

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Iter<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

// The other iterators list what they have not yet yielded, as hashbrown's do: the entries as
// `(key, value)` pairs, or the keys, or the values.

/// The entries of `rest` as `(key, value)` pairs.
fn entries<K: fmt::Debug, V: fmt::Debug>(
    f: &mut fmt::Formatter<'_>,
    rest: raw::Iter<'_, (K, V)>,
) -> fmt::Result {
    f.debug_list().entries(rest.map(|(key, value)| (key, value))).finish()
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for IterMut<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        entries(f, self.0.rest())
    }
}

impl<K: fmt::Debug, V> fmt::Debug for Keys<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

impl<K, V: fmt::Debug> fmt::Debug for Values<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

impl<K, V: fmt::Debug> fmt::Debug for ValuesMut<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.rest().map(|(_, value)| value)).finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug, A: Allocator> fmt::Debug for IntoIter<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        entries(f, self.0.rest())
    }
}

impl<K: fmt::Debug, V, A: Allocator> fmt::Debug for IntoKeys<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.rest().map(|(key, _)| key)).finish()
    }
}

impl<K, V: fmt::Debug, A: Allocator> fmt::Debug for IntoValues<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.rest().map(|(_, value)| value)).finish()
    }
}

impl<K, V, A: Allocator> Drain<'_, K, V, A> {
    /// The keys not yet taken: what a set's drain lists.
    pub(crate) fn rest_keys(&self) -> impl Iterator<Item = &K> {
        self.0.rest().map(|(key, _)| key)
    }
}

impl<K: fmt::Debug, V: fmt::Debug, A: Allocator> fmt::Debug for Drain<'_, K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        entries(f, self.0.rest())
    }
}

/// A map in an allocator that is `Default` (`Global`, a zero-sized one), as `T::default()` and
/// `mem::take` ask for. A frame's (`&Frame`) is not, so a map in a frame is made with `new_in`.
/// The ratchet's: kept for the existing callers on the global heap; new code names its
/// allocator with [`Map::new_in`].
impl<K, V, S: Default, A: Allocator + Default> Default for Map<K, V, S, A> {
    #[inline]
    fn default() -> Self {
        Map::new_in(A::default())
    }
}

impl<K: Clone, V: Clone, S: Clone, A: Allocator + Clone> Clone for Map<K, V, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Map { table: self.table.clone(), hasher: self.hasher.clone() }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S, A: Allocator> fmt::Debug for Map<K, V, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The same keys, each with an equal value, whatever the order.
impl<K: Eq, V: PartialEq, S: KeyHash<K>, A: Allocator> PartialEq for Map<K, V, S, A> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.iter().all(|(key, value)| other.get(key).is_some_and(|it| *value == *it))
    }
}

impl<K: Eq, V: Eq, S: KeyHash<K>, A: Allocator> Eq for Map<K, V, S, A> {}

/// The value under `key`; a key not held panics, as a slice's index out of range does.
impl<K, Q, V, S, A> Index<&Q> for Map<K, V, S, A>
where
    K: Eq,
    Q: Equivalent<K> + ?Sized,
    S: KeyHash<K> + KeyHash<Q>,
    A: Allocator,
{
    type Output = V;

    #[inline]
    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("a key the map does not hold")
    }
}

impl<K, V, S, A: Allocator> IntoIterator for Map<K, V, S, A> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V, A>;

    #[inline]
    fn into_iter(self) -> IntoIter<K, V, A> {
        IntoIter(self.table.into_iter())
    }
}

impl<'a, K, V, S, A: Allocator> IntoIterator for &'a Map<K, V, S, A> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    #[inline]
    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K, V, S, A: Allocator> IntoIterator for &'a mut Map<K, V, S, A> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    #[inline]
    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

/// `collect()` into a map in an allocator that is `Default` (`Global`); one in a frame is
/// made with [`Map::from_iter_in`]. The ratchet's, as [`Default`].
impl<K: Eq, V, S: KeyHash<K> + Default, A: Allocator + Default> FromIterator<(K, V)>
    for Map<K, V, S, A>
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Map::from_iter_in(iter, A::default())
    }
}

impl<K: Eq, V, S: KeyHash<K>, A: Allocator> Extend<(K, V)> for Map<K, V, S, A> {
    #[inline]
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        // As hashbrown: half the hint once the map holds something, as many keys repeat.
        let hint = iter.size_hint().0;
        self.reserve(if self.is_empty() { hint } else { hint.div_ceil(2) });
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}

impl<'a, K: Eq + Copy, V: Copy, S: KeyHash<K>, A: Allocator> Extend<(&'a K, &'a V)>
    for Map<K, V, S, A>
{
    #[inline]
    fn extend<I: IntoIterator<Item = (&'a K, &'a V)>>(&mut self, iter: I) {
        self.extend(iter.into_iter().map(|(key, value)| (*key, *value)));
    }
}

// No serde forms: the `serde` feature was removed on 2026-11-20 (D170 C7); `hash_set` says why.
