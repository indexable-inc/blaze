//! The single-owner tables, [`Map`] and [`Set`]: hashbrown's, on a hasher of this crate's, in
//! the allocator the type names (AGENTS.md rule 11, bump-allocate). [`HashMap`](crate::HashMap),
//! [`HashSet`](crate::HashSet), [`ByHash`](crate::ByHash) and [`ByHashSet`](crate::ByHashSet)
//! are these on [`FixedState`](crate::FixedState) or [`BuildKeyHasher`](crate::BuildKeyHasher).
//!
//! The allocator has no default: a table that lives within one task is in its frame
//! (`HashMap<K, V, &Frame>`, from `blaze_mem::frame::current`), a longer one in the request,
//! revision or image allocator, and one on the global heap writes `std::alloc::Global` and says
//! why (`// alloc: <why>`). A table is made with [`Map::new_in`], [`Map::with_capacity_in`] or
//! [`Map::from_iter_in`], which take the allocator and build the hasher, so no caller names one.
//!
//! Everything else is hashbrown's, through `Deref` (`get`, `insert`, `entry`, `iter`, `retain`,
//! ..); what takes the table by value (`into_iter`, `into_keys`, `into_values`) and the traits
//! (`Extend`, `FromIterator`, `Index`, `PartialEq`, serde's) are forwarded here. The entry and
//! iterator types are hashbrown's, in [`hash_map`](crate::hash_map) and
//! [`hash_set`](crate::hash_set).

use std::{
    alloc::Allocator,
    fmt,
    hash::{BuildHasher, Hash},
    ops::{BitAnd, BitOr, BitXor, Deref, DerefMut, Index, Sub},
};

use hashbrown::{Equivalent, hash_map, hash_set};

/// A map on the hasher `S`, in the allocator `A`.
pub struct Map<K, V, S, A: Allocator>(hashbrown::HashMap<K, V, S, A>);

/// A set on the hasher `S`, in the allocator `A`.
pub struct Set<T, S, A: Allocator>(hashbrown::HashSet<T, S, A>);

impl<K, V, S: Default, A: Allocator> Map<K, V, S, A> {
    /// An empty map in `alloc`; it allocates nothing until the first insert.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Map(hashbrown::HashMap::with_hasher_in(S::default(), alloc))
    }

    /// An empty map in `alloc` with room for `capacity` entries.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        Map(hashbrown::HashMap::with_capacity_and_hasher_in(capacity, S::default(), alloc))
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Default, A: Allocator> Map<K, V, S, A> {
    /// The map of `iter`'s pairs in `alloc`, a later pair replacing an earlier one's value:
    /// what `collect()` makes, with the allocator named.
    #[inline]
    pub fn from_iter_in<I: IntoIterator<Item = (K, V)>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let mut map = Map::with_capacity_in(iter.size_hint().0, alloc);
        map.0.extend(iter);
        map
    }
}

impl<K, V, S, A: Allocator> Map<K, V, S, A> {
    /// An empty map on `hasher` in `alloc`, in a `const` too.
    #[inline]
    pub const fn with_hasher_in(hasher: S, alloc: A) -> Self {
        Map(hashbrown::HashMap::with_hasher_in(hasher, alloc))
    }

    /// The hashbrown map this is.
    #[inline]
    pub fn into_inner(self) -> hashbrown::HashMap<K, V, S, A> {
        self.0
    }

    /// The keys, by value, in no order.
    #[inline]
    pub fn into_keys(self) -> hash_map::IntoKeys<K, V, A> {
        self.0.into_keys()
    }

    /// The values, by value, in no order.
    #[inline]
    pub fn into_values(self) -> hash_map::IntoValues<K, V, A> {
        self.0.into_values()
    }
}

impl<T, S: Default, A: Allocator> Set<T, S, A> {
    /// An empty set in `alloc`; it allocates nothing until the first insert.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Set(hashbrown::HashSet::with_hasher_in(S::default(), alloc))
    }

    /// An empty set in `alloc` with room for `capacity` elements.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        Set(hashbrown::HashSet::with_capacity_and_hasher_in(capacity, S::default(), alloc))
    }
}

impl<T, S, A: Allocator> Set<T, S, A> {
    /// An empty set on `hasher` in `alloc`, in a `const` too.
    #[inline]
    pub const fn with_hasher_in(hasher: S, alloc: A) -> Self {
        Set(hashbrown::HashSet::with_hasher_in(hasher, alloc))
    }
}

impl<T: Eq + Hash, S: BuildHasher + Default, A: Allocator> Set<T, S, A> {
    /// The set of `iter`'s elements in `alloc`: what `collect()` makes, with the allocator
    /// named.
    #[inline]
    pub fn from_iter_in<I: IntoIterator<Item = T>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let mut set = Set::with_capacity_in(iter.size_hint().0, alloc);
        set.0.extend(iter);
        set
    }
}

impl<T, S, A: Allocator> Set<T, S, A> {
    /// The hashbrown set this is.
    #[inline]
    pub fn into_inner(self) -> hashbrown::HashSet<T, S, A> {
        self.0
    }
}

impl<K, V, S, A: Allocator> From<hashbrown::HashMap<K, V, S, A>> for Map<K, V, S, A> {
    #[inline]
    fn from(map: hashbrown::HashMap<K, V, S, A>) -> Self {
        Map(map)
    }
}

impl<T, S, A: Allocator> From<hashbrown::HashSet<T, S, A>> for Set<T, S, A> {
    #[inline]
    fn from(set: hashbrown::HashSet<T, S, A>) -> Self {
        Set(set)
    }
}

impl<K, V, S, A: Allocator> Deref for Map<K, V, S, A> {
    type Target = hashbrown::HashMap<K, V, S, A>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<K, V, S, A: Allocator> DerefMut for Map<K, V, S, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T, S, A: Allocator> Deref for Set<T, S, A> {
    type Target = hashbrown::HashSet<T, S, A>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, S, A: Allocator> DerefMut for Set<T, S, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// A map in an allocator that is `Default` (`Global`, a zero-sized one), as `T::default()` and
/// `mem::take` ask for. A frame's (`&Frame`) is not, so a map in a frame is made with `new_in`.
impl<K, V, S: Default, A: Allocator + Default> Default for Map<K, V, S, A> {
    #[inline]
    fn default() -> Self {
        Map::new_in(A::default())
    }
}

impl<T, S: Default, A: Allocator + Default> Default for Set<T, S, A> {
    #[inline]
    fn default() -> Self {
        Set::new_in(A::default())
    }
}

impl<K: Clone, V: Clone, S: Clone, A: Allocator + Clone> Clone for Map<K, V, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Map(self.0.clone())
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        self.0.clone_from(&source.0);
    }
}

impl<T: Clone, S: Clone, A: Allocator + Clone> Clone for Set<T, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Set(self.0.clone())
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        self.0.clone_from(&source.0);
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S, A: Allocator> fmt::Debug for Map<K, V, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: fmt::Debug, S, A: Allocator> fmt::Debug for Set<T, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<K: Eq + Hash, V: PartialEq, S: BuildHasher, A: Allocator> PartialEq for Map<K, V, S, A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<K: Eq + Hash, V: Eq, S: BuildHasher, A: Allocator> Eq for Map<K, V, S, A> {}

impl<T: Eq + Hash, S: BuildHasher, A: Allocator> PartialEq for Set<T, S, A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Eq + Hash, S: BuildHasher, A: Allocator> Eq for Set<T, S, A> {}

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
        &self.0[key]
    }
}

impl<K, V, S, A: Allocator> IntoIterator for Map<K, V, S, A> {
    type Item = (K, V);
    type IntoIter = hash_map::IntoIter<K, V, A>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, K, V, S, A: Allocator> IntoIterator for &'a Map<K, V, S, A> {
    type Item = (&'a K, &'a V);
    type IntoIter = hash_map::Iter<'a, K, V>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a, K, V, S, A: Allocator> IntoIterator for &'a mut Map<K, V, S, A> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = hash_map::IterMut<'a, K, V>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

impl<T, S, A: Allocator> IntoIterator for Set<T, S, A> {
    type Item = T;
    type IntoIter = hash_set::IntoIter<T, A>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T, S, A: Allocator> IntoIterator for &'a Set<T, S, A> {
    type Item = &'a T;
    type IntoIter = hash_set::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// `collect()` into a map in an allocator that is `Default` (`Global`); one in a frame is
/// made with [`Map::from_iter_in`].
impl<K: Eq + Hash, V, S: BuildHasher + Default, A: Allocator + Default> FromIterator<(K, V)>
    for Map<K, V, S, A>
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Map::from_iter_in(iter, A::default())
    }
}

impl<T: Eq + Hash, S: BuildHasher + Default, A: Allocator + Default> FromIterator<T>
    for Set<T, S, A>
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Set::from_iter_in(iter, A::default())
    }
}

impl<K: Eq + Hash, V, S: BuildHasher, A: Allocator> Extend<(K, V)> for Map<K, V, S, A> {
    #[inline]
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<'a, K: Eq + Hash + Copy, V: Copy, S: BuildHasher, A: Allocator> Extend<(&'a K, &'a V)>
    for Map<K, V, S, A>
{
    #[inline]
    fn extend<I: IntoIterator<Item = (&'a K, &'a V)>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<T: Eq + Hash, S: BuildHasher, A: Allocator> Extend<T> for Set<T, S, A> {
    #[inline]
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<'a, T: 'a + Eq + Hash + Copy, S: BuildHasher, A: Allocator> Extend<&'a T> for Set<T, S, A> {
    #[inline]
    fn extend<I: IntoIterator<Item = &'a T>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

/// The set operators make a new set in `A::default()`, so only for an allocator that has one.
macro_rules! set_operator {
    ($($trait:ident $method:ident),*) => {$(
        impl<T, S, A> $trait<&Set<T, S, A>> for &Set<T, S, A>
        where
            T: Eq + Hash + Clone,
            S: BuildHasher + Default,
            A: Allocator + Default,
        {
            type Output = Set<T, S, A>;

            #[inline]
            fn $method(self, rhs: &Set<T, S, A>) -> Set<T, S, A> {
                Set($trait::$method(&self.0, &rhs.0))
            }
        }
    )*};
}

set_operator!(BitOr bitor, BitAnd bitand, BitXor bitxor, Sub sub);

impl<K, V, S, A> serde::Serialize for Map<K, V, S, A>
where
    K: serde::Serialize + Eq + Hash,
    V: serde::Serialize,
    S: BuildHasher,
    A: Allocator,
{
    fn serialize<Z: serde::Serializer>(&self, serializer: Z) -> Result<Z::Ok, Z::Error> {
        serde::Serialize::serialize(&self.0, serializer)
    }
}

impl<'de, K, V, S, A> serde::Deserialize<'de> for Map<K, V, S, A>
where
    K: serde::Deserialize<'de> + Eq + Hash,
    V: serde::Deserialize<'de>,
    S: BuildHasher + Default,
    A: Allocator + Default,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <hashbrown::HashMap<K, V, S, A> as serde::Deserialize<'de>>::deserialize(deserializer)
            .map(Map)
    }
}

impl<T, S, A> serde::Serialize for Set<T, S, A>
where
    T: serde::Serialize + Eq + Hash,
    S: BuildHasher,
    A: Allocator,
{
    fn serialize<Z: serde::Serializer>(&self, serializer: Z) -> Result<Z::Ok, Z::Error> {
        serde::Serialize::serialize(&self.0, serializer)
    }
}

impl<'de, T, S, A> serde::Deserialize<'de> for Set<T, S, A>
where
    T: serde::Deserialize<'de> + Eq + Hash,
    S: BuildHasher + Default,
    A: Allocator + Default,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <hashbrown::HashSet<T, S, A> as serde::Deserialize<'de>>::deserialize(deserializer).map(Set)
    }
}
