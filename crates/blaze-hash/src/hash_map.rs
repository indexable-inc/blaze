//! The single-owner map, [`Map`], and its entry and iterator types: this crate's own SwissTable
//! ([`crate::raw`], on `blaze_swiss`'s probe), on a hasher of this crate's, in the allocator the
//! type names (AGENTS.md rule 11, bump-allocate). [`HashMap`](crate::HashMap),
//! [`ByHash`](crate::ByHash) and [`KeyedMap`](crate::KeyedMap) are this on
//! [`FixedState`](crate::FixedState), [`BuildKeyHasher`](crate::BuildKeyHasher) or a
//! [`SipKey`](crate::SipKey).
//!
//! The allocator has no default: a table that lives within one task is in its frame
//! (`HashMap<K, V, &Frame>`, from `blaze_mem::frame::current`), a longer one in the request,
//! revision or image allocator, and one on the global heap names `Global`. A table is made with
//! [`Map::new_in`], [`Map::with_capacity_in`] or [`Map::from_iter_in`], which take the
//! allocator and build the hasher, so no caller names one.
//!
//! The methods are the ones the tree uses of the hashbrown map this replaces, with the same
//! names and meanings; its iteration order is the table's slot order, a function of the inserts,
//! the removals and the hasher, the same on every run.

use core::{
    alloc::Allocator,
    fmt,
    hash::{BuildHasher, Hash},
    iter::FusedIterator,
    ops::Index,
};

use blaze_own::AllocFailure;

use crate::{
    Equivalent,
    raw::{self, RawTable},
};

/// A map on the hasher `S`, in the allocator `A`.
pub struct Map<K, V, S, A: Allocator> {
    pub(crate) table: RawTable<(K, V), A>,
    pub(crate) hasher: S,
}

/// What hashes a held entry when the table is made again: its key, by `hasher`.
#[inline]
fn by_key<K: Hash, V, S: BuildHasher>(hasher: &S) -> impl Fn(&(K, V)) -> u64 + '_ {
    move |entry| hasher.hash_one(&entry.0)
}

impl<K, V, S: Default, A: Allocator> Map<K, V, S, A> {
    /// An empty map in `alloc`; it allocates nothing until the first insert.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Map::with_hasher_in(S::default(), alloc)
    }

    /// An empty map in `alloc` with room for `capacity` entries.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        Map::with_capacity_and_hasher_in(capacity, S::default(), alloc)
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Default, A: Allocator> Map<K, V, S, A> {
    /// The map of `iter`'s pairs in `alloc`, a later pair replacing an earlier one's value:
    /// what `collect()` makes, with the allocator named.
    #[inline]
    pub fn from_iter_in<I: IntoIterator<Item = (K, V)>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let mut map = Map::with_capacity_in(iter.size_hint().0, alloc);
        map.extend(iter);
        map
    }
}

impl<K, V, S, A: Allocator> Map<K, V, S, A> {
    /// An empty map on `hasher` in `alloc`, in a `const` too.
    #[inline]
    pub const fn with_hasher_in(hasher: S, alloc: A) -> Self {
        Map { table: RawTable::new_in(alloc), hasher }
    }

    /// An empty map on `hasher` in `alloc` with room for `capacity` entries.
    #[inline]
    pub fn with_capacity_and_hasher_in(capacity: usize, hasher: S, alloc: A) -> Self {
        Map { table: RawTable::with_capacity_in(capacity, alloc), hasher }
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

impl<K: Eq + Hash, V, S: BuildHasher, A: Allocator> Map<K, V, S, A> {
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
    fn find<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<usize> {
        let hash = self.hasher.hash_one(key);
        self.table.find(hash, |(held, _)| key.equivalent(held))
    }

    #[inline]
    pub fn get<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<&V> {
        let index = self.find(key)?;
        Some(&self.table.get(index).1)
    }

    #[inline]
    pub fn get_mut<Q: ?Sized + Hash + Equivalent<K>>(&mut self, key: &Q) -> Option<&mut V> {
        let index = self.find(key)?;
        Some(&mut self.table.get_mut(index).1)
    }

    #[inline]
    pub fn get_key_value<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> Option<(&K, &V)> {
        let index = self.find(key)?;
        let (key, value) = self.table.get(index);
        Some((key, value))
    }

    #[inline]
    pub fn contains_key<Q: ?Sized + Hash + Equivalent<K>>(&self, key: &Q) -> bool {
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
    pub fn remove<Q: ?Sized + Hash + Equivalent<K>>(&mut self, key: &Q) -> Option<V> {
        self.remove_entry(key).map(|(_, value)| value)
    }

    #[inline]
    pub fn remove_entry<Q: ?Sized + Hash + Equivalent<K>>(&mut self, key: &Q) -> Option<(K, V)> {
        let index = self.find(key)?;
        Some(self.table.remove(index))
    }

    /// The entry of `key`, to read, fill or change in place.
    #[inline]
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, S, A> {
        let hash = self.hasher.hash_one(&key);
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

impl<'a, K: Hash, V, S: BuildHasher, A: Allocator> Entry<'a, K, V, S, A> {
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

impl<'a, K: Hash, V, S: BuildHasher, A: Allocator> VacantEntry<'a, K, V, S, A> {
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
impl<K: Eq + Hash, V: PartialEq, S: BuildHasher, A: Allocator> PartialEq for Map<K, V, S, A> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.iter().all(|(key, value)| other.get(key).is_some_and(|it| *value == *it))
    }
}

impl<K: Eq + Hash, V: Eq, S: BuildHasher, A: Allocator> Eq for Map<K, V, S, A> {}

/// The value under `key`; a key not held panics, as a slice's index out of range does.
impl<K, Q, V, S, A> Index<&Q> for Map<K, V, S, A>
where
    K: Eq + Hash,
    Q: Hash + Equivalent<K> + ?Sized,
    S: BuildHasher,
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
impl<K: Eq + Hash, V, S: BuildHasher + Default, A: Allocator + Default> FromIterator<(K, V)>
    for Map<K, V, S, A>
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Map::from_iter_in(iter, A::default())
    }
}

impl<K: Eq + Hash, V, S: BuildHasher, A: Allocator> Extend<(K, V)> for Map<K, V, S, A> {
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

impl<'a, K: Eq + Hash + Copy, V: Copy, S: BuildHasher, A: Allocator> Extend<(&'a K, &'a V)>
    for Map<K, V, S, A>
{
    #[inline]
    fn extend<I: IntoIterator<Item = (&'a K, &'a V)>>(&mut self, iter: I) {
        self.extend(iter.into_iter().map(|(key, value)| (*key, *value)));
    }
}

// serde's forms, a map of the entries: behind the `serde` feature, which the crates that
// persist a table with serde turn on (the root manifest's entry).
#[cfg(feature = "serde")]
impl<K, V, S, A> serde::Serialize for Map<K, V, S, A>
where
    K: serde::Serialize,
    V: serde::Serialize,
    A: Allocator,
{
    fn serialize<Z: serde::Serializer>(&self, serializer: Z) -> Result<Z::Ok, Z::Error> {
        serializer.collect_map(self.iter())
    }
}

#[cfg(feature = "serde")]
impl<'de, K, V, S, A> serde::Deserialize<'de> for Map<K, V, S, A>
where
    K: serde::Deserialize<'de> + Eq + Hash,
    V: serde::Deserialize<'de>,
    S: BuildHasher + Default,
    A: Allocator + Default,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use core::marker::PhantomData;

        struct Entries<K, V, S, A>(PhantomData<fn() -> (K, V, S, A)>);

        impl<'de, K, V, S, A> serde::de::Visitor<'de> for Entries<K, V, S, A>
        where
            K: serde::Deserialize<'de> + Eq + Hash,
            V: serde::Deserialize<'de>,
            S: BuildHasher + Default,
            A: Allocator + Default,
        {
            type Value = Map<K, V, S, A>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut entries: M) -> Result<Self::Value, M::Error> {
                // At most a page's worth up front: the hint is the input's, not to be trusted.
                let hint = entries.size_hint().unwrap_or(0).min(4096);
                let mut map = Map::with_capacity_in(hint, A::default());
                while let Some((key, value)) = entries.next_entry()? {
                    map.insert(key, value);
                }
                Ok(map)
            }
        }

        deserializer.deserialize_map(Entries(PhantomData))
    }
}
