//! The insertion-ordered tables, [`OrderMap`] and [`OrderSet`]: entries kept dense in a vector
//! in the order they were inserted, and a table of indices into it. [`IndexMap`](crate::IndexMap)
//! and [`IndexSet`](crate::IndexSet) are these on [`FixedState`](crate::FixedState).
//!
//! What it is for: a table whose iteration reaches an output (a diagnostic's order, a store
//! key, a written file). A [`HashMap`](crate::HashMap)'s order is its hasher's and its
//! capacity's; this one's is the order of the inserts, whatever the hasher, the allocator, the
//! capacity or the process, so nothing has to be sorted afterwards (rule deterministic-output).
//! It is also the table to index by position: an entry's index is dense and stays until an
//! entry before it is removed.
//!
//! How it differs from the `indexmap` crate it replaces in blaze's own code:
//!
//! - The allocator is in the type and has no default, as for every table here
//!   ([`OrderMap::new_in`], [`OrderMap::with_capacity_in`]); there is no `new()`.
//! - The table holds `u32` indices, not `usize`: half the table's memory, and a lookup's probe
//!   reads half as many cache lines of it. A map holds at most `u32::MAX` entries, and one
//!   more is a panic that says so.
//! - Each entry keeps its key's hash, so growing the table hashes no key again and a removal
//!   finds its index's bucket without the key.
//! - There is no `remove`: a removal either moves the last entry into the hole
//!   ([`OrderMap::swap_remove`], O(1), the order of one entry changes) or shifts every later
//!   entry down ([`OrderMap::shift_remove`], O(n), the order is kept), and the call says which.
//! - Two maps are equal when they hold the same entries in the same order: the order is part
//!   of the value, since it is what a reader of the map sees.
//!
//! Cost: a lookup is one hash, one probe of the index table and one compare in the entries; an
//! insert appends; iteration is a walk of a vector. [`OrderMap::with_capacity_in`] makes two
//! allocations (the entries, the table) and none after while the map stays within it.

use std::{
    alloc::Allocator,
    fmt,
    hash::{BuildHasher, Hash},
    ops::Index,
};

use hashbrown::{Equivalent, HashTable};

/// One entry: the key's hash, kept so that the table is rebuilt and searched without the key.
struct Slot<K, V> {
    hash: u64,
    key: K,
    value: V,
}

impl<K: Clone, V: Clone> Clone for Slot<K, V> {
    fn clone(&self) -> Self {
        Slot { hash: self.hash, key: self.key.clone(), value: self.value.clone() }
    }
}

/// A map that iterates in the order its keys were first inserted, on the hasher `S`, in the
/// allocator `A`.
pub struct OrderMap<K, V, S, A: Allocator + Clone> {
    entries: Vec<Slot<K, V>, A>,
    /// The index in `entries` of each entry, found by the entry's hash.
    table: HashTable<u32, A>,
    hasher: S,
}

/// The index `at` as the table holds it.
#[inline]
fn held(at: usize) -> u32 {
    match u32::try_from(at) {
        Ok(it) => it,
        Err(_) => panic!("an insertion-ordered table holds at most {} entries", u32::MAX),
    }
}

impl<K, V, S: Default, A: Allocator + Clone> OrderMap<K, V, S, A> {
    /// An empty map in `alloc`: no allocation until the first insert.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self::with_hasher_in(S::default(), alloc)
    }

    /// An empty map in `alloc` with room for `capacity` entries: two allocations, and none
    /// after until it holds more.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        Self::with_capacity_and_hasher_in(capacity, S::default(), alloc)
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Default, A: Allocator + Clone> OrderMap<K, V, S, A> {
    /// The map of `iter`'s pairs in `alloc`, in `iter`'s order; a key given twice keeps its
    /// first place and its last value.
    pub fn from_iter_in<I: IntoIterator<Item = (K, V)>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let mut map = Self::with_capacity_in(iter.size_hint().0, alloc);
        map.extend(iter);
        map
    }
}

impl<K, V, S, A: Allocator + Clone> OrderMap<K, V, S, A> {
    /// An empty map in `alloc` whose keys are hashed by `hasher`: how a map of keys an attacker
    /// may pick is made, with a [`SipKey`](crate::SipKey).
    #[inline]
    pub fn with_hasher_in(hasher: S, alloc: A) -> Self {
        // clone: the allocator's handle, once per map: the entries and the table each hold one.
        OrderMap { entries: Vec::new_in(alloc.clone()), table: HashTable::new_in(alloc), hasher }
    }

    /// [`OrderMap::with_capacity_in`] with `hasher`.
    #[inline]
    pub fn with_capacity_and_hasher_in(capacity: usize, hasher: S, alloc: A) -> Self {
        OrderMap {
            // clone: the allocator's handle, once per map.
            entries: Vec::with_capacity_in(capacity, alloc.clone()),
            table: HashTable::with_capacity_in(capacity, alloc),
            hasher,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries it holds without allocating again.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.entries.capacity().min(self.table.capacity())
    }

    /// Removes every entry and keeps the memory.
    pub fn clear(&mut self) {
        self.table.clear();
        self.entries.clear();
    }

    /// Room for `additional` more entries.
    pub fn reserve(&mut self, additional: usize) {
        let OrderMap { entries, table, .. } = self;
        entries.reserve(additional);
        table.reserve(additional, |&at| entries[at as usize].hash);
    }

    /// The entry at `index` in insertion order.
    #[inline]
    pub fn get_index(&self, index: usize) -> Option<(&K, &V)> {
        self.entries.get(index).map(|it| (&it.key, &it.value))
    }

    /// The entry at `index`, its value to change.
    #[inline]
    pub fn get_index_mut(&mut self, index: usize) -> Option<(&K, &mut V)> {
        self.entries.get_mut(index).map(|it| (&it.key, &mut it.value))
    }

    /// The entry inserted first of those held.
    #[inline]
    pub fn first(&self) -> Option<(&K, &V)> {
        self.get_index(0)
    }

    /// The entry inserted last of those held.
    #[inline]
    pub fn last(&self) -> Option<(&K, &V)> {
        self.entries.last().map(|it| (&it.key, &it.value))
    }

    /// The entries in insertion order.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter(self.entries.iter())
    }

    /// The entries in insertion order, their values to change.
    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut(self.entries.iter_mut())
    }

    /// The keys in insertion order.
    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + DoubleEndedIterator + Clone {
        self.entries.iter().map(|it| &it.key)
    }

    /// The values in the insertion order of their keys.
    #[inline]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + DoubleEndedIterator + Clone {
        self.entries.iter().map(|it| &it.value)
    }

    /// The values in the insertion order of their keys, to change.
    #[inline]
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut V> + DoubleEndedIterator {
        self.entries.iter_mut().map(|it| &mut it.value)
    }

    /// Removes the entry inserted last of those held: no entry's index changes.
    pub fn pop(&mut self) -> Option<(K, V)> {
        let last = self.entries.pop()?;
        let at = held(self.entries.len());
        self.unlink(last.hash, at);
        Some((last.key, last.value))
    }

    /// Removes the entry at `index` by moving the last entry into its place: O(1), and the
    /// last entry's index becomes `index`.
    pub fn swap_remove_index(&mut self, index: usize) -> Option<(K, V)> {
        if index >= self.entries.len() {
            return None;
        }
        self.unlink(self.entries[index].hash, held(index));
        let removed = self.entries.swap_remove(index);
        // The entry that was last is at `index` now, unless the removed one was the last.
        if let Some(moved) = self.entries.get(index) {
            let was = held(self.entries.len());
            match self.table.find_mut(moved.hash, |&at| at == was) {
                Some(at) => *at = held(index),
                None => unreachable!("every entry's index is in the table"),
            }
        }
        Some((removed.key, removed.value))
    }

    /// Removes the entry at `index` and moves every later entry one index down: O(n), and the
    /// order of the entries left is as it was.
    pub fn shift_remove_index(&mut self, index: usize) -> Option<(K, V)> {
        if index >= self.entries.len() {
            return None;
        }
        self.unlink(self.entries[index].hash, held(index));
        let removed = self.entries.remove(index);
        let from = held(index);
        for at in self.table.iter_mut() {
            if *at > from {
                *at -= 1;
            }
        }
        Some((removed.key, removed.value))
    }

    /// Keeps the entries `keep` says to, in their order.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let before = self.entries.len();
        self.entries.retain_mut(|it| keep(&it.key, &mut it.value));
        if self.entries.len() != before {
            self.relink();
        }
    }

    /// Takes the index `at` of an entry with `hash` out of the table.
    fn unlink(&mut self, hash: u64, at: u32) {
        match self.table.find_entry(hash, |&it| it == at) {
            Ok(entry) => {
                entry.remove();
            }
            Err(_) => unreachable!("every entry's index is in the table"),
        }
    }

    /// The table made again from the entries, after their indices changed.
    fn relink(&mut self) {
        let OrderMap { entries, table, .. } = self;
        table.clear();
        for (at, entry) in entries.iter().enumerate() {
            table.insert_unique(entry.hash, held(at), |&it| entries[it as usize].hash);
        }
    }
}

impl<K: Eq + Hash, V, S: BuildHasher, A: Allocator + Clone> OrderMap<K, V, S, A> {
    /// Where `key` is, and its hash.
    #[inline]
    fn find<Q: Hash + Equivalent<K> + ?Sized>(&self, key: &Q) -> (u64, Option<usize>) {
        let hash = self.hasher.hash_one(key);
        let entries = &self.entries;
        let at = self.table.find(hash, |&at| key.equivalent(&entries[at as usize].key));
        (hash, at.map(|&at| at as usize))
    }

    /// `value` under `key`, at the end when the key is new; a key already held keeps its
    /// place and its key, and its old value is returned.
    #[inline]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.insert_full(key, value).1
    }

    /// [`OrderMap::insert`], with the entry's index.
    pub fn insert_full(&mut self, key: K, value: V) -> (usize, Option<V>) {
        let (hash, at) = self.find(&key);
        match at {
            Some(at) => (at, Some(std::mem::replace(&mut self.entries[at].value, value))),
            None => (self.push(hash, key, value), None),
        }
    }

    /// The value under `key`, made by `make` and put at the end when the key is new; with the
    /// entry's index.
    pub fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> (usize, &mut V) {
        let (hash, at) = self.find(&key);
        let at = match at {
            Some(at) => at,
            None => self.push(hash, key, make()),
        };
        (at, &mut self.entries[at].value)
    }

    /// A new entry at the end: its index.
    fn push(&mut self, hash: u64, key: K, value: V) -> usize {
        let OrderMap { entries, table, .. } = self;
        let at = entries.len();
        table.insert_unique(hash, held(at), |&it| entries[it as usize].hash);
        entries.push(Slot { hash, key, value });
        at
    }

    #[inline]
    pub fn get<Q: Hash + Equivalent<K> + ?Sized>(&self, key: &Q) -> Option<&V> {
        self.find(key).1.map(|at| &self.entries[at].value)
    }

    #[inline]
    pub fn get_mut<Q: Hash + Equivalent<K> + ?Sized>(&mut self, key: &Q) -> Option<&mut V> {
        self.find(key).1.map(|at| &mut self.entries[at].value)
    }

    /// The entry of `key` with its index.
    #[inline]
    pub fn get_full<Q: Hash + Equivalent<K> + ?Sized>(&self, key: &Q) -> Option<(usize, &K, &V)> {
        let at = self.find(key).1?;
        let entry = &self.entries[at];
        Some((at, &entry.key, &entry.value))
    }

    /// The index of `key` in insertion order.
    #[inline]
    pub fn get_index_of<Q: Hash + Equivalent<K> + ?Sized>(&self, key: &Q) -> Option<usize> {
        self.find(key).1
    }

    #[inline]
    pub fn contains_key<Q: Hash + Equivalent<K> + ?Sized>(&self, key: &Q) -> bool {
        self.find(key).1.is_some()
    }

    /// Removes `key` as [`OrderMap::swap_remove_index`] does: O(1), the last entry takes its
    /// place.
    pub fn swap_remove<Q: Hash + Equivalent<K> + ?Sized>(&mut self, key: &Q) -> Option<V> {
        let at = self.find(key).1?;
        self.swap_remove_index(at).map(|(_, value)| value)
    }

    /// Removes `key` as [`OrderMap::shift_remove_index`] does: O(n), the order is kept.
    pub fn shift_remove<Q: Hash + Equivalent<K> + ?Sized>(&mut self, key: &Q) -> Option<V> {
        let at = self.find(key).1?;
        self.shift_remove_index(at).map(|(_, value)| value)
    }

    /// Sorts the entries by key; equal maps of other orders become equal.
    pub fn sort_keys(&mut self)
    where
        K: Ord,
    {
        self.entries.sort_by(|a, b| a.key.cmp(&b.key));
        self.relink();
    }
}

/// The entries of an [`OrderMap`] in insertion order.
pub struct Iter<'a, K, V>(std::slice::Iter<'a, Slot<K, V>>);

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Iter(self.0.clone())
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|it| (&it.key, &it.value))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for Iter<'_, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|it| (&it.key, &it.value))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}

/// The entries of an [`OrderMap`] in insertion order, their values to change.
pub struct IterMut<'a, K, V>(std::slice::IterMut<'a, Slot<K, V>>);

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|it| (&it.key, &mut it.value))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for IterMut<'_, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|it| (&it.key, &mut it.value))
    }
}

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {}

/// The entries of an [`OrderMap`] taken by value, in insertion order.
pub struct IntoIter<K, V, A: Allocator>(std::vec::IntoIter<Slot<K, V>, A>);

impl<K, V, A: Allocator> Iterator for IntoIter<K, V, A> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        self.0.next().map(|it| (it.key, it.value))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V, A: Allocator> DoubleEndedIterator for IntoIter<K, V, A> {
    #[inline]
    fn next_back(&mut self) -> Option<(K, V)> {
        self.0.next_back().map(|it| (it.key, it.value))
    }
}

impl<K, V, A: Allocator> ExactSizeIterator for IntoIter<K, V, A> {}

impl<K, V, S, A: Allocator + Clone> IntoIterator for OrderMap<K, V, S, A> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V, A>;

    #[inline]
    fn into_iter(self) -> IntoIter<K, V, A> {
        IntoIter(self.entries.into_iter())
    }
}

impl<'a, K, V, S, A: Allocator + Clone> IntoIterator for &'a OrderMap<K, V, S, A> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    #[inline]
    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K, V, S, A: Allocator + Clone> IntoIterator for &'a mut OrderMap<K, V, S, A> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    #[inline]
    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

impl<K: Eq + Hash, V, S: BuildHasher, A: Allocator + Clone> Extend<(K, V)>
    for OrderMap<K, V, S, A>
{
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}

/// An empty map in the allocator's default, for a type that derives `Default`; code that makes
/// a map names its allocator ([`OrderMap::new_in`]).
impl<K, V, S: Default, A: Allocator + Clone + Default> Default for OrderMap<K, V, S, A> {
    fn default() -> Self {
        Self::new_in(A::default())
    }
}

impl<K: Clone, V: Clone, S: Clone, A: Allocator + Clone> Clone for OrderMap<K, V, S, A> {
    fn clone(&self) -> Self {
        OrderMap {
            entries: self.entries.clone(),
            table: self.table.clone(),
            hasher: self.hasher.clone(),
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S, A: Allocator + Clone> fmt::Debug for OrderMap<K, V, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The same entries in the same order.
impl<K: PartialEq, V: PartialEq, S, A: Allocator + Clone> PartialEq for OrderMap<K, V, S, A> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.iter().zip(other.iter()).all(|(a, b)| a.0 == b.0 && a.1 == b.1)
    }
}

impl<K: Eq, V: Eq, S, A: Allocator + Clone> Eq for OrderMap<K, V, S, A> {}

/// The value at `index` in insertion order; out of range panics, as a slice's index does.
impl<K, V, S, A: Allocator + Clone> Index<usize> for OrderMap<K, V, S, A> {
    type Output = V;

    #[inline]
    fn index(&self, index: usize) -> &V {
        &self.entries[index].value
    }
}

/// A set that iterates in the order its values were first inserted, on the hasher `S`, in the
/// allocator `A`: an [`OrderMap`] of its values to nothing.
pub struct OrderSet<T, S, A: Allocator + Clone>(OrderMap<T, (), S, A>);

impl<T, S: Default, A: Allocator + Clone> OrderSet<T, S, A> {
    /// An empty set in `alloc`: no allocation until the first insert.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        OrderSet(OrderMap::new_in(alloc))
    }

    /// An empty set in `alloc` with room for `capacity` values: two allocations, and none
    /// after until it holds more.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        OrderSet(OrderMap::with_capacity_in(capacity, alloc))
    }
}

impl<T: Eq + Hash, S: BuildHasher + Default, A: Allocator + Clone> OrderSet<T, S, A> {
    /// The set of `iter`'s values in `alloc`, each at the place it first came.
    pub fn from_iter_in<I: IntoIterator<Item = T>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let mut set = Self::with_capacity_in(iter.size_hint().0, alloc);
        set.extend(iter);
        set
    }
}

impl<T, S, A: Allocator + Clone> OrderSet<T, S, A> {
    /// An empty set in `alloc` whose values are hashed by `hasher`.
    #[inline]
    pub fn with_hasher_in(hasher: S, alloc: A) -> Self {
        OrderSet(OrderMap::with_hasher_in(hasher, alloc))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn reserve(&mut self, additional: usize) {
        self.0.reserve(additional);
    }

    /// The value at `index` in insertion order.
    #[inline]
    pub fn get_index(&self, index: usize) -> Option<&T> {
        self.0.get_index(index).map(|(it, ())| it)
    }

    #[inline]
    pub fn first(&self) -> Option<&T> {
        self.get_index(0)
    }

    #[inline]
    pub fn last(&self) -> Option<&T> {
        self.0.last().map(|(it, ())| it)
    }

    /// The values in insertion order.
    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &T> + DoubleEndedIterator + Clone {
        self.0.keys()
    }

    /// Removes the value inserted last of those held.
    pub fn pop(&mut self) -> Option<T> {
        self.0.pop().map(|(it, ())| it)
    }

    /// As [`OrderMap::swap_remove_index`].
    pub fn swap_remove_index(&mut self, index: usize) -> Option<T> {
        self.0.swap_remove_index(index).map(|(it, ())| it)
    }

    /// As [`OrderMap::shift_remove_index`].
    pub fn shift_remove_index(&mut self, index: usize) -> Option<T> {
        self.0.shift_remove_index(index).map(|(it, ())| it)
    }

    /// Keeps the values `keep` says to, in their order.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.0.retain(|it, ()| keep(it));
    }
}

impl<T: Eq + Hash, S: BuildHasher, A: Allocator + Clone> OrderSet<T, S, A> {
    /// Adds `value` at the end if it is not held: whether it was added.
    #[inline]
    pub fn insert(&mut self, value: T) -> bool {
        self.insert_full(value).1
    }

    /// [`OrderSet::insert`], with the value's index.
    pub fn insert_full(&mut self, value: T) -> (usize, bool) {
        let (at, old) = self.0.insert_full(value, ());
        (at, old.is_none())
    }

    #[inline]
    pub fn contains<Q: Hash + Equivalent<T> + ?Sized>(&self, value: &Q) -> bool {
        self.0.contains_key(value)
    }

    /// The value held that equals `value`.
    #[inline]
    pub fn get<Q: Hash + Equivalent<T> + ?Sized>(&self, value: &Q) -> Option<&T> {
        self.0.get_full(value).map(|(_, it, ())| it)
    }

    /// The index of `value` in insertion order.
    #[inline]
    pub fn get_index_of<Q: Hash + Equivalent<T> + ?Sized>(&self, value: &Q) -> Option<usize> {
        self.0.get_index_of(value)
    }

    /// Removes `value` as [`OrderMap::swap_remove`] does: whether it was held.
    pub fn swap_remove<Q: Hash + Equivalent<T> + ?Sized>(&mut self, value: &Q) -> bool {
        self.0.swap_remove(value).is_some()
    }

    /// Removes `value` as [`OrderMap::shift_remove`] does: whether it was held.
    pub fn shift_remove<Q: Hash + Equivalent<T> + ?Sized>(&mut self, value: &Q) -> bool {
        self.0.shift_remove(value).is_some()
    }

    /// Sorts the values.
    pub fn sort(&mut self)
    where
        T: Ord,
    {
        self.0.sort_keys();
    }
}

/// The values of an [`OrderSet`] taken by value, in insertion order.
pub struct SetIntoIter<T, A: Allocator>(IntoIter<T, (), A>);

impl<T, A: Allocator> Iterator for SetIntoIter<T, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        self.0.next().map(|(it, ())| it)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<T, A: Allocator> ExactSizeIterator for SetIntoIter<T, A> {}

impl<T, S, A: Allocator + Clone> IntoIterator for OrderSet<T, S, A> {
    type Item = T;
    type IntoIter = SetIntoIter<T, A>;

    #[inline]
    fn into_iter(self) -> SetIntoIter<T, A> {
        SetIntoIter(self.0.into_iter())
    }
}

impl<T: Eq + Hash, S: BuildHasher, A: Allocator + Clone> Extend<T> for OrderSet<T, S, A> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for value in iter {
            self.insert(value);
        }
    }
}

impl<T, S: Default, A: Allocator + Clone + Default> Default for OrderSet<T, S, A> {
    fn default() -> Self {
        Self::new_in(A::default())
    }
}

impl<T: Clone, S: Clone, A: Allocator + Clone> Clone for OrderSet<T, S, A> {
    fn clone(&self) -> Self {
        OrderSet(self.0.clone())
    }
}

impl<T: fmt::Debug, S, A: Allocator + Clone> fmt::Debug for OrderSet<T, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// The same values in the same order.
impl<T: PartialEq, S, A: Allocator + Clone> PartialEq for OrderSet<T, S, A> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Eq, S, A: Allocator + Clone> Eq for OrderSet<T, S, A> {}

/// The value at `index` in insertion order; out of range panics, as a slice's index does.
impl<T, S, A: Allocator + Clone> Index<usize> for OrderSet<T, S, A> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &T {
        &self.0.entries[index].key
    }
}
