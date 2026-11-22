//! [`SortedMap`] and [`SortedSet`]: ordered by key, over one sorted [`Vec`] searched by halving.
//! The flat ordered map, built once and read many times; the B-tree, for a map that changes
//! after it is built, is [`crate::BTreeMap`].
//!
//! Costs, for `n` entries: a lookup is `O(log n)` comparisons over one contiguous array (a cache
//! miss per halving at most, none once the array fits in cache); `insert` and `remove` are
//! `O(log n)` comparisons and `O(n)` moves, so building one by inserts in random order is
//! `O(n²)` moves: build from sorted entries ([`SortedMap::from_sorted`]) where you can. Iteration
//! in order is a slice walk. One allocation in all, no per-entry overhead.

use core::{
    alloc::Allocator,
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::{Bound, Index, RangeBounds},
    slice,
};

use crate::{
    error::{AllocFailure, fail},
    make::CloneIn,
    vec::{Refused, Vec},
};

/// A map ordered by key, one sorted vector of `(key, value)`: lookup O(log n) on one array, insert and remove O(n) moves; build it once with `from_sorted`.
///
/// # Invariants
///
/// The keys are strictly increasing.
pub struct SortedMap<K, V, A: Allocator> {
    entries: Vec<(K, V), A>,
}

/// Entries given to [`SortedMap::from_sorted`] that were not strictly increasing by key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotSorted {
    /// The first index whose key is not greater than the one before.
    pub at: usize,
}

impl fmt::Display for NotSorted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the key at {} is not greater than the key before it", self.at)
    }
}

impl core::error::Error for NotSorted {}

impl<K: Ord, V, A: Allocator> SortedMap<K, V, A> {
    pub const fn new_in(alloc: A) -> SortedMap<K, V, A> {
        SortedMap { entries: Vec::new_in(alloc) }
    }

    pub fn with_capacity_in(capacity: usize, alloc: A) -> SortedMap<K, V, A> {
        SortedMap { entries: Vec::with_capacity_in(capacity, alloc) }
    }

    /// Takes over `entries` already strictly increasing by key, checked in `O(n)`; else gives
    /// where they are not, and the entries are dropped.
    pub fn from_sorted(entries: Vec<(K, V), A>) -> Result<SortedMap<K, V, A>, NotSorted> {
        if let Some(at) = entries.windows(2).position(|pair| pair[0].0 >= pair[1].0) {
            return Err(NotSorted { at: at + 1 });
        }
        Ok(SortedMap { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn allocator(&self) -> &A {
        self.entries.allocator()
    }

    fn find<Q: Ord + ?Sized>(&self, key: &Q) -> Result<usize, usize>
    where
        K: Borrow<Q>,
    {
        self.entries.binary_search_by(|(k, _)| k.borrow().cmp(key))
    }

    pub fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key).ok()?;
        Some(&self.entries[at].1)
    }

    pub fn get_key_value<Q: Ord + ?Sized>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key).ok()?;
        let (k, v) = &self.entries[at];
        Some((k, v))
    }

    pub fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key).ok()?;
        Some(&mut self.entries[at].1)
    }

    pub fn contains_key<Q: Ord + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.find(key).is_ok()
    }

    /// Puts `value` under `key`; gives the value it replaced. `O(n)` moves when the key is new.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.find(&key) {
            Ok(at) => Some(core::mem::replace(&mut self.entries[at].1, value)),
            Err(at) => {
                self.entries.insert_shift(at, (key, value));
                None
            }
        }
    }

    /// The value under `key`; on a miss, `make(key)` gives the owned key and its value, put in
    /// where the one search left off. So the owned key is made only on a miss, where
    /// `map.entry(key.to_owned())` makes it on every hit. `O(n)` moves on a miss.
    ///
    /// # Panics
    ///
    /// When `make` gives a key not equal to `key` (it would sit out of order), or there is no
    /// memory.
    #[track_caller]
    pub fn get_or_insert_with<Q: Ord + ?Sized>(&mut self, key: &Q, make: impl FnOnce(&Q) -> (K, V)) -> &mut V
    where
        K: Borrow<Q>,
    {
        match self.try_get_or_insert_with(key, make) {
            Ok(value) => value,
            Err(refused) => fail(refused.why),
        }
    }

    /// [`SortedMap::get_or_insert_with`], or the made pair handed back with the
    /// [`AllocFailure`] when the map must grow and cannot; the map is as it was.
    ///
    /// # Panics
    ///
    /// When `make` gives a key not equal to `key`.
    #[track_caller]
    pub fn try_get_or_insert_with<Q: Ord + ?Sized>(
        &mut self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> Result<&mut V, Refused<(K, V)>>
    where
        K: Borrow<Q>,
    {
        let at = match self.find(key) {
            Ok(at) => return Ok(&mut self.entries[at].1),
            Err(at) => at,
        };
        let (owned, value) = make(key);
        if owned.borrow().cmp(key) != Ordering::Equal {
            panic!("SortedMap::get_or_insert_with: `make` gave a key not equal to the one looked up");
        }
        if let Err(why) = self.entries.try_reserve(1) {
            return Err(Refused { value: (owned, value), why });
        }
        self.entries.insert_shift(at, (owned, value));
        Ok(&mut self.entries[at].1)
    }

    /// The place of `key`, found once: [`Entry::or_insert_with`], [`Entry::and_modify`] and the
    /// rest then act on it with no second search.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, A> {
        match self.find(&key) {
            Ok(at) => Entry::Occupied(OccupiedEntry { map: self, at }),
            Err(at) => Entry::Vacant(VacantEntry { map: self, at, key }),
        }
    }

    /// [`SortedMap::insert`], or the key and value handed back with the [`AllocFailure`](crate::AllocFailure) when it
    /// must grow and cannot (as [`Vec::try_push`] hands its value back); the map is as it was.
    pub fn try_insert(&mut self, key: K, value: V) -> Result<Option<V>, Refused<(K, V)>> {
        match self.find(&key) {
            Ok(at) => Ok(Some(core::mem::replace(&mut self.entries[at].1, value))),
            Err(at) => {
                if let Err(why) = self.entries.try_reserve(1) {
                    return Err(Refused { value: (key, value), why });
                }
                self.entries.insert_shift(at, (key, value));
                Ok(None)
            }
        }
    }

    pub fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.remove_entry(key).map(|(_, v)| v)
    }

    pub fn remove_entry<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key).ok()?;
        Some(self.entries.remove_shift(at))
    }

    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        self.entries.first().map(|(k, v)| (k, v))
    }

    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        self.entries.last().map(|(k, v)| (k, v))
    }

    pub fn pop_first(&mut self) -> Option<(K, V)> {
        if self.entries.is_empty() { None } else { Some(self.entries.remove_shift(0)) }
    }

    pub fn pop_last(&mut self) -> Option<(K, V)> {
        self.entries.pop()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Keeps the entries `keep` says yes to.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        self.entries.retain_mut(|(k, v)| keep(k, v));
    }

    /// The entries, in key order.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { entries: self.entries.iter() }
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut { entries: self.entries.iter_mut() }
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> + ExactSizeIterator + '_ {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + ExactSizeIterator + '_ {
        self.entries.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut V> + ExactSizeIterator + '_ {
        self.entries.iter_mut().map(|(_, v)| v)
    }

    /// The entries with keys in `range`, in key order: two searches, then a slice walk.
    pub fn range<Q: Ord + ?Sized, R: RangeBounds<Q>>(&self, range: R) -> Iter<'_, K, V>
    where
        K: Borrow<Q>,
    {
        let start = match range.start_bound() {
            Bound::Included(q) => self.entries.partition_point(|(k, _)| k.borrow() < q),
            Bound::Excluded(q) => self.entries.partition_point(|(k, _)| k.borrow() <= q),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(q) => self.entries.partition_point(|(k, _)| k.borrow() <= q),
            Bound::Excluded(q) => self.entries.partition_point(|(k, _)| k.borrow() < q),
            Bound::Unbounded => self.entries.len(),
        };
        Iter { entries: self.entries[start..end.max(start)].iter() }
    }

    /// The entries as one slice, in key order.
    pub fn as_slice(&self) -> &[(K, V)] {
        &self.entries
    }

    /// The entries as the sorted vector.
    pub fn into_vec(self) -> Vec<(K, V), A> {
        self.entries
    }
}

/// A key's place in a [`SortedMap`], from [`SortedMap::entry`].
pub enum Entry<'a, K, V, A: Allocator> {
    Occupied(OccupiedEntry<'a, K, V, A>),
    Vacant(VacantEntry<'a, K, V, A>),
}

/// A key the map holds, at index `at`.
pub struct OccupiedEntry<'a, K, V, A: Allocator> {
    map: &'a mut SortedMap<K, V, A>,
    at: usize,
}

/// A key the map does not hold, and the index `at` that keeps the keys increasing.
pub struct VacantEntry<'a, K, V, A: Allocator> {
    map: &'a mut SortedMap<K, V, A>,
    at: usize,
    key: K,
}

impl<'a, K, V, A: Allocator> Entry<'a, K, V, A> {
    pub fn key(&self) -> &K {
        match self {
            Entry::Occupied(entry) => entry.key(),
            Entry::Vacant(entry) => entry.key(),
        }
    }

    pub fn or_insert(self, value: V) -> &'a mut V {
        self.or_insert_with(|| value)
    }

    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(make()),
        }
    }

    /// As [`Entry::or_insert_with`], with the key to make the value from.
    pub fn or_insert_with_key(self, make: impl FnOnce(&K) -> V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let value = make(&entry.key);
                entry.insert(value)
            }
        }
    }

    /// Changes the value when the key is there; then any `or_` method inserts when it is not.
    pub fn and_modify(self, change: impl FnOnce(&mut V)) -> Entry<'a, K, V, A> {
        match self {
            Entry::Occupied(mut entry) => {
                change(entry.get_mut());
                Entry::Occupied(entry)
            }
            vacant @ Entry::Vacant(_) => vacant,
        }
    }
}

impl<'a, K, V, A: Allocator> OccupiedEntry<'a, K, V, A> {
    pub fn key(&self) -> &K {
        &self.map.entries[self.at].0
    }

    pub fn get(&self) -> &V {
        &self.map.entries[self.at].1
    }

    pub fn get_mut(&mut self) -> &mut V {
        &mut self.map.entries[self.at].1
    }

    pub fn into_mut(self) -> &'a mut V {
        &mut self.map.entries[self.at].1
    }

    pub fn insert(&mut self, value: V) -> V {
        core::mem::replace(self.get_mut(), value)
    }

    pub fn remove_entry(self) -> (K, V) {
        self.map.entries.remove_shift(self.at)
    }

    pub fn remove(self) -> V {
        self.remove_entry().1
    }
}

impl<'a, K, V, A: Allocator> VacantEntry<'a, K, V, A> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn into_key(self) -> K {
        self.key
    }

    /// Puts `value` under the key, `O(n)` moves; panics as `Vec::insert` when it cannot grow.
    pub fn insert(self, value: V) -> &'a mut V {
        let VacantEntry { map, at, key } = self;
        map.entries.insert_shift(at, (key, value));
        &mut map.entries[at].1
    }
}

impl<K: Ord + Borrow<Q>, Q: Ord + ?Sized, V, A: Allocator> Index<&Q> for SortedMap<K, V, A> {
    type Output = V;

    /// # Panics
    ///
    /// When the key is not in the map.
    #[track_caller]
    fn index(&self, key: &Q) -> &V {
        match self.get(key) {
            Some(value) => value,
            None => panic!("blaze_own::SortedMap::index: the key is not in the map of {} entries", self.len()),
        }
    }
}

impl<K: Ord, V, A: Allocator> Extend<(K, V)> for SortedMap<K, V, A> {
    /// Inserts each in turn, the later of two equal keys winning: `O(n)` moves per new key.
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}

impl<K: Clone, V: Clone, A: Allocator + Clone> Clone for SortedMap<K, V, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> SortedMap<K, V, A> {
        SortedMap { entries: self.entries.clone() }
    }
}

impl<K, V, A: Allocator> SortedMap<K, V, A> {
    /// A deep copy in another allocator: each key and value copied with [`CloneIn`] into
    /// `alloc`, in place (a copy orders as its original, `CloneIn`'s contract, so nothing is
    /// sorted again). A refusal drops what was copied.
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<SortedMap<K::In, V::In, B>, AllocFailure>
    where
        K: CloneIn<B>,
        V: CloneIn<B>,
    {
        let mut entries = Vec::try_with_capacity_in(self.entries.len(), alloc.clone())?;
        for (key, value) in self.entries.as_slice() {
            let key = key.try_clone_in(alloc.clone())?;
            let value = value.try_clone_in(alloc.clone())?;
            // The room is reserved: the push does not allocate.
            entries.push((key, value));
        }
        Ok(SortedMap { entries })
    }

    /// [`SortedMap::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> SortedMap<K::In, V::In, B>
    where
        K: CloneIn<B>,
        V: CloneIn<B>,
    {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, A: Allocator> fmt::Debug for SortedMap<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.entries.iter().map(|(k, v)| (k, v))).finish()
    }
}

impl<K: PartialEq, V: PartialEq, A: Allocator, B: Allocator> PartialEq<SortedMap<K, V, B>>
    for SortedMap<K, V, A>
{
    fn eq(&self, other: &SortedMap<K, V, B>) -> bool {
        self.entries.as_slice() == other.entries.as_slice()
    }
}

impl<K: Eq, V: Eq, A: Allocator> Eq for SortedMap<K, V, A> {}

impl<K: PartialOrd, V: PartialOrd, A: Allocator> PartialOrd for SortedMap<K, V, A> {
    fn partial_cmp(&self, other: &SortedMap<K, V, A>) -> Option<Ordering> {
        self.entries.as_slice().partial_cmp(other.entries.as_slice())
    }
}

impl<K: Ord, V: Ord, A: Allocator> Ord for SortedMap<K, V, A> {
    fn cmp(&self, other: &SortedMap<K, V, A>) -> Ordering {
        self.entries.as_slice().cmp(other.entries.as_slice())
    }
}

impl<K: Hash, V: Hash, A: Allocator> Hash for SortedMap<K, V, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entries.hash(state);
    }
}

impl<K, V, A: Allocator> IntoIterator for SortedMap<K, V, A> {
    type Item = (K, V);
    type IntoIter = crate::vec::IntoIter<(K, V), A>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a, K: Ord, V, A: Allocator> IntoIterator for &'a SortedMap<K, V, A> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

/// A map's entries by reference, in key order.
#[derive(Clone)]
pub struct Iter<'a, K, V> {
    entries: slice::Iter<'a, (K, V)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        self.entries.next().map(|(k, v)| (k, v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for Iter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back().map(|(k, v)| (k, v))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}

/// A map's entries, values mutable, in key order. Keys stay shared: changing one could break
/// the order.
pub struct IterMut<'a, K, V> {
    entries: slice::IterMut<'a, (K, V)>,
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        self.entries.next().map(|(k, v)| (&*k, v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for IterMut<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back().map(|(k, v)| (&*k, v))
    }
}

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {}

/// A set ordered by value, one sorted vector; costs as [`SortedMap`]'s.
///
/// # Invariants
///
/// The values are strictly increasing.
pub struct SortedSet<T, A: Allocator> {
    values: Vec<T, A>,
}

impl<T: Ord, A: Allocator> SortedSet<T, A> {
    pub const fn new_in(alloc: A) -> SortedSet<T, A> {
        SortedSet { values: Vec::new_in(alloc) }
    }

    pub fn with_capacity_in(capacity: usize, alloc: A) -> SortedSet<T, A> {
        SortedSet { values: Vec::with_capacity_in(capacity, alloc) }
    }

    /// Takes over `values` already strictly increasing, checked in `O(n)`.
    pub fn from_sorted(values: Vec<T, A>) -> Result<SortedSet<T, A>, NotSorted> {
        if let Some(at) = values.windows(2).position(|pair| pair[0] >= pair[1]) {
            return Err(NotSorted { at: at + 1 });
        }
        Ok(SortedSet { values })
    }

    /// `values` sorted and with duplicates dropped, in `O(n log n)`.
    pub fn from_vec(mut values: Vec<T, A>) -> SortedSet<T, A> {
        values.sort_unstable();
        values.dedup();
        SortedSet { values }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn allocator(&self) -> &A {
        self.values.allocator()
    }

    fn find<Q: Ord + ?Sized>(&self, value: &Q) -> Result<usize, usize>
    where
        T: Borrow<Q>,
    {
        self.values.binary_search_by(|v| v.borrow().cmp(value))
    }

    pub fn contains<Q: Ord + ?Sized>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.find(value).is_ok()
    }

    pub fn get<Q: Ord + ?Sized>(&self, value: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
    {
        let at = self.find(value).ok()?;
        Some(&self.values[at])
    }

    /// Adds `value`; `false` when an equal one was there (it stays, `value` is dropped).
    pub fn insert(&mut self, value: T) -> bool {
        match self.find(&value) {
            Ok(_) => false,
            Err(at) => {
                self.values.insert_shift(at, value);
                true
            }
        }
    }

    pub fn remove<Q: Ord + ?Sized>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.take(value).is_some()
    }

    pub fn take<Q: Ord + ?Sized>(&mut self, value: &Q) -> Option<T>
    where
        T: Borrow<Q>,
    {
        let at = self.find(value).ok()?;
        Some(self.values.remove_shift(at))
    }

    pub fn first(&self) -> Option<&T> {
        self.values.first()
    }

    pub fn last(&self) -> Option<&T> {
        self.values.last()
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.values.retain(keep);
    }

    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.values.iter()
    }

    /// The values in `range`, in order.
    pub fn range<Q: Ord + ?Sized, R: RangeBounds<Q>>(&self, range: R) -> slice::Iter<'_, T>
    where
        T: Borrow<Q>,
    {
        let start = match range.start_bound() {
            Bound::Included(q) => self.values.partition_point(|v| v.borrow() < q),
            Bound::Excluded(q) => self.values.partition_point(|v| v.borrow() <= q),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(q) => self.values.partition_point(|v| v.borrow() <= q),
            Bound::Excluded(q) => self.values.partition_point(|v| v.borrow() < q),
            Bound::Unbounded => self.values.len(),
        };
        self.values[start..end.max(start)].iter()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.values
    }

    pub fn into_vec(self) -> Vec<T, A> {
        self.values
    }
}

impl<T: Ord, A: Allocator> Extend<T> for SortedSet<T, A> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.insert(value);
        }
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for SortedSet<T, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> SortedSet<T, A> {
        SortedSet { values: self.values.clone() }
    }
}

impl<T, A: Allocator> SortedSet<T, A> {
    /// A deep copy in another allocator, each value copied with [`CloneIn`] in place.
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<SortedSet<T::In, B>, AllocFailure>
    where
        T: CloneIn<B>,
    {
        Ok(SortedSet { values: self.values.try_clone_in(alloc)? })
    }

    /// [`SortedSet::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> SortedSet<T::In, B>
    where
        T: CloneIn<B>,
    {
        SortedSet { values: self.values.clone_in(alloc) }
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for SortedSet<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.values.iter()).finish()
    }
}

impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<SortedSet<T, B>> for SortedSet<T, A> {
    fn eq(&self, other: &SortedSet<T, B>) -> bool {
        self.values.as_slice() == other.values.as_slice()
    }
}

impl<T: Eq, A: Allocator> Eq for SortedSet<T, A> {}

impl<T: Hash, A: Allocator> Hash for SortedSet<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.values.hash(state);
    }
}

impl<T, A: Allocator> IntoIterator for SortedSet<T, A> {
    type Item = T;
    type IntoIter = crate::vec::IntoIter<T, A>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.into_iter()
    }
}

impl<'a, T: Ord, A: Allocator> IntoIterator for &'a SortedSet<T, A> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.iter()
    }
}
