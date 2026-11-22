//! The single-owner set, [`Set`], and its iterator types: a [`Map`] of its elements to nothing,
//! on this crate's own SwissTable, in the allocator the type names. [`HashSet`](crate::HashSet),
//! [`ByHashSet`](crate::ByHashSet) and [`KeyedSet`](crate::KeyedSet) are this on this crate's
//! hashers. The methods are the ones the tree uses of the hashbrown set this replaces.

use core::{
    alloc::Allocator,
    fmt,
    iter::FusedIterator,
    ops::{BitAnd, BitOr, BitXor, Sub},
};

use blaze_own::AllocFailure;

use crate::{
    Equivalent, KeyHash,
    hash_map::{self, Entry, Map, MapOpts},
};

/// A set on the hasher `S`, in the allocator `A`, hashed through [`KeyHash`] as [`Map`] is.
pub struct Set<T, S, A: Allocator>(Map<T, (), S, A>);

/// What a [`Set`] is made from ([`Set::new`], rule 83 options-struct), as [`MapOpts`] for a
/// map: `Set::new(SetOpts { hasher: FixedState, alloc: frame, capacity: None })`.
///
/// A call's named arguments: write it as a literal in the call that takes it (rule 83).
///
/// [`MapOpts`]: crate::hash_map::MapOpts
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct SetOpts<S, A> {
    /// The hasher, as a value (D125).
    pub hasher: S,
    /// The allocator the set's memory is in.
    pub alloc: A,
    /// Elements room is made for at once: `None` allocates nothing until the first insert,
    /// `Some(n)` reserves room for `n`.
    pub capacity: Option<usize>,
}

impl<T, S, A: Allocator> Set<T, S, A> {
    /// The set `opts` describe: empty, on `opts.hasher` in `opts.alloc`, with room for
    /// `opts.capacity` elements; a refused allocation is a panic naming it ([`Set::try_new`]
    /// returns it). A set of an iterator's elements is `new` then [`Extend::extend`].
    #[inline]
    pub fn new(opts: SetOpts<S, A>) -> Self {
        Set(Map::new(MapOpts { hasher: opts.hasher, alloc: opts.alloc, capacity: opts.capacity }))
    }

    /// [`Set::new`], with a refused allocation of `opts.capacity` returned.
    #[inline]
    pub fn try_new(opts: SetOpts<S, A>) -> Result<Self, AllocFailure> {
        Ok(Set(Map::try_new(MapOpts {
            hasher: opts.hasher,
            alloc: opts.alloc,
            capacity: opts.capacity,
        })?))
    }
}

impl<T, S: Default, A: Allocator> Set<T, S, A> {
    /// An empty set in `alloc` on `S::default()`.
    ///
    /// D126: goes at the sweep; write `Set::new(SetOpts { hasher, alloc, capacity: None })`.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Set::new(SetOpts { hasher: S::default(), alloc, capacity: None })
    }

    /// An empty set in `alloc` on `S::default()` with room for `capacity` elements.
    ///
    /// D126: goes at the sweep; write `Set::new(SetOpts { hasher, alloc, capacity: Some(n) })`.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        Set::new(SetOpts { hasher: S::default(), alloc, capacity: Some(capacity) })
    }
}

impl<T, S, A: Allocator> Set<T, S, A> {
    /// An empty set on `hasher` in `alloc`, in a `const` too: its own body, as
    /// [`Map::with_hasher_in`]'s, since [`Set::new`] is not `const`.
    ///
    /// D126: goes at the sweep where it is not in a `const`; write
    /// `Set::new(SetOpts { hasher, alloc, capacity: None })`.
    #[inline]
    pub const fn with_hasher_in(hasher: S, alloc: A) -> Self {
        Set(Map::with_hasher_in(hasher, alloc))
    }

    /// An empty set on `hasher` in `alloc` with room for `capacity` elements.
    ///
    /// D126: goes at the sweep; write `Set::new(SetOpts { hasher, alloc, capacity: Some(n) })`.
    #[inline]
    pub fn with_capacity_and_hasher_in(capacity: usize, hasher: S, alloc: A) -> Self {
        Set::new(SetOpts { hasher, alloc, capacity: Some(capacity) })
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

    #[inline]
    pub fn hasher(&self) -> &S {
        self.0.hasher()
    }

    #[inline]
    pub fn allocator(&self) -> &A {
        self.0.allocator()
    }

    #[inline]
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The elements, in the table's order.
    #[inline]
    pub fn iter(&self) -> Iter<'_, T> {
        Iter(self.0.keys())
    }

    /// Every element taken out; the memory is kept.
    #[inline]
    pub fn drain(&mut self) -> Drain<'_, T, A> {
        Drain(self.0.drain())
    }

    /// Keeps the elements `keep` says to.
    #[inline]
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.0.retain(|it, ()| keep(it));
    }
}

impl<T: Eq, S: KeyHash<T> + Default, A: Allocator> Set<T, S, A> {
    /// The set of `iter`'s elements in `alloc` on `S::default()`.
    ///
    /// D126: goes at the sweep; write `Set::new(SetOpts { hasher, alloc, capacity })`, then
    /// `extend`.
    #[inline]
    pub fn from_iter_in<I: IntoIterator<Item = T>>(iter: I, alloc: A) -> Self {
        let iter = iter.into_iter();
        let capacity = Some(iter.size_hint().0);
        let mut set = Set::new(SetOpts { hasher: S::default(), alloc, capacity });
        set.extend(iter);
        set
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> Set<T, S, A> {
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.0.reserve(additional);
    }

    #[inline]
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.0.try_reserve(additional)
    }

    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
    }

    #[inline]
    pub fn shrink_to(&mut self, min: usize) {
        self.0.shrink_to(min);
    }

    /// Adds `value`: whether it was not held (a held equal element is kept).
    #[inline]
    pub fn insert(&mut self, value: T) -> bool {
        match self.0.entry(value) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(());
                true
            }
        }
    }

    /// Adds `value`, putting it in place of an equal element held, which it returns.
    #[inline]
    pub fn replace(&mut self, value: T) -> Option<T> {
        let old = self.0.remove_entry(&value).map(|(it, ())| it);
        self.0.insert(value, ());
        old
    }

    /// The held element equal to `value`, which is added if none is.
    #[inline]
    pub fn get_or_insert(&mut self, value: T) -> &T {
        self.0.entry(value).insert(()).into_key_ref()
    }

    #[inline]
    pub fn contains<Q: ?Sized + Equivalent<T>>(&self, value: &Q) -> bool
    where
        S: KeyHash<Q>,
    {
        self.0.contains_key(value)
    }

    /// The held element equal to `value`.
    #[inline]
    pub fn get<Q: ?Sized + Equivalent<T>>(&self, value: &Q) -> Option<&T>
    where
        S: KeyHash<Q>,
    {
        self.0.get_key_value(value).map(|(it, ())| it)
    }

    /// The held element equal to `value`, or else `make(value)`, added now: the owned element
    /// is made only when none is held, with one hash and one probe either way. `make` must
    /// return an element equal to `value` (checked in debug builds); a refused allocation is
    /// a panic naming it.
    #[inline]
    pub fn get_or_insert_with<Q: ?Sized + Equivalent<T>>(
        &mut self,
        value: &Q,
        make: impl FnOnce(&Q) -> T,
    ) -> &T
    where
        S: KeyHash<Q>,
    {
        match self.0.try_slot_or_insert_with(value, |value| (make(value), ())) {
            Ok(index) => &self.0.table.get(index).0,
            Err(failure) => crate::raw::fail(failure),
        }
    }

    /// [`Set::get_or_insert_with`], a refused allocation returned (and `make` not called).
    #[inline]
    pub fn try_get_or_insert_with<Q: ?Sized + Equivalent<T>>(
        &mut self,
        value: &Q,
        make: impl FnOnce(&Q) -> T,
    ) -> Result<&T, AllocFailure>
    where
        S: KeyHash<Q>,
    {
        let index = self.0.try_slot_or_insert_with(value, |value| (make(value), ()))?;
        Ok(&self.0.table.get(index).0)
    }

    /// Starts loading what a [`Set::get`] or [`Set::contains`] of `value` reads first, as
    /// [`Map::prefetch`].
    #[inline]
    pub fn prefetch<Q: ?Sized>(&self, value: &Q)
    where
        S: KeyHash<Q>,
    {
        self.0.prefetch(value);
    }

    /// [`Set::get`] of each of `values`, as one batch: [`Map::get_many`]'s group prefetching.
    #[inline]
    pub fn get_many<Q: ?Sized + Equivalent<T>, const N: usize>(
        &self,
        values: [&Q; N],
    ) -> [Option<&T>; N]
    where
        S: KeyHash<Q>,
    {
        self.0.find_many(values).map(|slot| slot.map(|index| &self.0.table.get(index).0))
    }

    /// [`Set::get_many`] of a slice: `out[at]` is the held element equal to `values[at]`, in
    /// [`Map::get_many_into`]'s pipeline. Panics unless `out` is as long as `values`.
    pub fn get_many_into<'a, Q: Equivalent<T>>(&'a self, values: &[Q], out: &mut [Option<&'a T>])
    where
        S: KeyHash<Q>,
    {
        assert_eq!(values.len(), out.len(), "Set::get_many_into: one answer per value");
        self.0.find_each(values, |at, slot| out[at] = slot.map(|index| &self.0.table.get(index).0));
    }

    /// Removes `value`: whether it was held.
    #[inline]
    pub fn remove<Q: ?Sized + Equivalent<T>>(&mut self, value: &Q) -> bool
    where
        S: KeyHash<Q>,
    {
        self.0.remove(value).is_some()
    }

    /// Removes and returns the held element equal to `value`.
    #[inline]
    pub fn take<Q: ?Sized + Equivalent<T>>(&mut self, value: &Q) -> Option<T>
    where
        S: KeyHash<Q>,
    {
        self.0.remove_entry(value).map(|(it, ())| it)
    }

    /// The elements of either.
    #[inline]
    pub fn union<'a>(&'a self, other: &'a Self) -> Union<'a, T, S, A> {
        // The larger first, so the rest walked against it is the smaller.
        let (larger, smaller) = if self.len() >= other.len() { (self, other) } else { (other, self) };
        Union { first: larger.iter(), rest: smaller.difference(larger) }
    }

    /// The elements of both, walking the smaller.
    #[inline]
    pub fn intersection<'a>(&'a self, other: &'a Self) -> Intersection<'a, T, S, A> {
        let (smaller, larger) = if self.len() <= other.len() { (self, other) } else { (other, self) };
        Intersection { walk: smaller.iter(), other: larger }
    }

    /// The elements of this one not in `other`.
    #[inline]
    pub fn difference<'a>(&'a self, other: &'a Self) -> Difference<'a, T, S, A> {
        Difference { walk: self.iter(), other }
    }

    /// The elements of exactly one.
    #[inline]
    pub fn symmetric_difference<'a>(&'a self, other: &'a Self) -> SymmetricDifference<'a, T, S, A> {
        SymmetricDifference { first: self.difference(other), second: other.difference(self) }
    }

    #[inline]
    pub fn is_disjoint(&self, other: &Self) -> bool {
        self.intersection(other).next().is_none()
    }

    #[inline]
    pub fn is_subset(&self, other: &Self) -> bool {
        self.len() <= other.len() && self.iter().all(|it| other.contains(it))
    }

    #[inline]
    pub fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }
}

/// A set's elements by reference.
pub struct Iter<'a, T>(hash_map::Keys<'a, T, ()>);

impl<T> Clone for Iter<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        Iter(self.0.clone())
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}
impl<T> FusedIterator for Iter<'_, T> {}

impl<T: fmt::Debug> fmt::Debug for Iter<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

/// A set's elements by value.
pub struct IntoIter<T, A: Allocator>(hash_map::IntoKeys<T, (), A>);

impl<T, A: Allocator> Iterator for IntoIter<T, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<T, A: Allocator> ExactSizeIterator for IntoIter<T, A> {}
impl<T, A: Allocator> FusedIterator for IntoIter<T, A> {}

/// The elements not yet taken, as hashbrown's set iterator lists them.
impl<T: fmt::Debug, A: Allocator> fmt::Debug for IntoIter<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A set's elements taken out by [`Set::drain`].
pub struct Drain<'a, T, A: Allocator>(hash_map::Drain<'a, T, (), A>);

impl<T, A: Allocator> Iterator for Drain<'_, T, A> {
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

impl<T, A: Allocator> ExactSizeIterator for Drain<'_, T, A> {}
impl<T, A: Allocator> FusedIterator for Drain<'_, T, A> {}

/// The elements not yet taken.
impl<T: fmt::Debug, A: Allocator> fmt::Debug for Drain<'_, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.rest_keys()).finish()
    }
}

/// The elements a set operation has not yet yielded, listed from a copy of it, as hashbrown's.
macro_rules! debug_by_clone {
    ($($name:ident),* $(,)?) => {$(
        impl<T: fmt::Debug + Eq, S: KeyHash<T>, A: Allocator> fmt::Debug
            for $name<'_, T, S, A>
        {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_list().entries(self.clone()).finish()
            }
        }
    )*};
}

debug_by_clone!(Difference, Intersection, Union, SymmetricDifference);

/// The elements of one set not in another: [`Set::difference`].
pub struct Difference<'a, T, S, A: Allocator> {
    walk: Iter<'a, T>,
    other: &'a Set<T, S, A>,
}

impl<T, S, A: Allocator> Clone for Difference<'_, T, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Difference { walk: self.walk.clone(), other: self.other }
    }
}

impl<'a, T: Eq, S: KeyHash<T>, A: Allocator> Iterator for Difference<'a, T, S, A> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        let other = self.other;
        self.walk.find(|it| !other.contains(*it))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.walk.size_hint().1)
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> FusedIterator for Difference<'_, T, S, A> {}

/// The elements of both sets: [`Set::intersection`].
pub struct Intersection<'a, T, S, A: Allocator> {
    walk: Iter<'a, T>,
    other: &'a Set<T, S, A>,
}

impl<T, S, A: Allocator> Clone for Intersection<'_, T, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Intersection { walk: self.walk.clone(), other: self.other }
    }
}

impl<'a, T: Eq, S: KeyHash<T>, A: Allocator> Iterator for Intersection<'a, T, S, A> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        let other = self.other;
        self.walk.find(|it| other.contains(*it))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.walk.size_hint().1)
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> FusedIterator for Intersection<'_, T, S, A> {}

/// The elements of either set, each once: [`Set::union`].
pub struct Union<'a, T, S, A: Allocator> {
    first: Iter<'a, T>,
    rest: Difference<'a, T, S, A>,
}

impl<T, S, A: Allocator> Clone for Union<'_, T, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Union { first: self.first.clone(), rest: self.rest.clone() }
    }
}

impl<'a, T: Eq, S: KeyHash<T>, A: Allocator> Iterator for Union<'a, T, S, A> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        self.first.next().or_else(|| self.rest.next())
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let (first, _) = self.first.size_hint();
        (first, first.checked_add(self.rest.size_hint().1.unwrap_or(0)))
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> FusedIterator for Union<'_, T, S, A> {}

/// The elements of exactly one of two sets: [`Set::symmetric_difference`].
pub struct SymmetricDifference<'a, T, S, A: Allocator> {
    first: Difference<'a, T, S, A>,
    second: Difference<'a, T, S, A>,
}

impl<T, S, A: Allocator> Clone for SymmetricDifference<'_, T, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        SymmetricDifference { first: self.first.clone(), second: self.second.clone() }
    }
}

impl<'a, T: Eq, S: KeyHash<T>, A: Allocator> Iterator
    for SymmetricDifference<'a, T, S, A>
{
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        self.first.next().or_else(|| self.second.next())
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.first.size_hint().1.zip(self.second.size_hint().1).map(|(a, b)| a + b))
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> FusedIterator
    for SymmetricDifference<'_, T, S, A>
{
}

/// A set in an allocator that is `Default` (`Global`). The ratchet's: kept for the existing
/// callers on the global heap; new code names its allocator with [`Set::new_in`].
impl<T, S: Default, A: Allocator + Default> Default for Set<T, S, A> {
    #[inline]
    fn default() -> Self {
        Set::new_in(A::default())
    }
}

impl<T: Clone, S: Clone, A: Allocator + Clone> Clone for Set<T, S, A> {
    #[inline]
    fn clone(&self) -> Self {
        Set(self.0.clone())
    }
}

impl<T: fmt::Debug, S, A: Allocator> fmt::Debug for Set<T, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// The same elements, whatever the order.
impl<T: Eq, S: KeyHash<T>, A: Allocator> PartialEq for Set<T, S, A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> Eq for Set<T, S, A> {}

impl<T, S, A: Allocator> IntoIterator for Set<T, S, A> {
    type Item = T;
    type IntoIter = IntoIter<T, A>;

    #[inline]
    fn into_iter(self) -> IntoIter<T, A> {
        IntoIter(self.0.into_keys())
    }
}

impl<'a, T, S, A: Allocator> IntoIterator for &'a Set<T, S, A> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

/// `collect()` into a set in an allocator that is `Default` (`Global`); one in a frame is
/// made with [`Set::from_iter_in`]. The ratchet's, as [`Default`].
impl<T: Eq, S: KeyHash<T> + Default, A: Allocator + Default> FromIterator<T>
    for Set<T, S, A>
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Set::from_iter_in(iter, A::default())
    }
}

impl<T: Eq, S: KeyHash<T>, A: Allocator> Extend<T> for Set<T, S, A> {
    #[inline]
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.0.extend(iter.into_iter().map(|it| (it, ())));
    }
}

impl<'a, T: 'a + Eq + Copy, S: KeyHash<T>, A: Allocator> Extend<&'a T> for Set<T, S, A> {
    #[inline]
    fn extend<I: IntoIterator<Item = &'a T>>(&mut self, iter: I) {
        self.extend(iter.into_iter().copied());
    }
}

/// The set operators make a new set in `A::default()`, so only for an allocator that has one.
macro_rules! set_operator {
    ($($trait:ident $method:ident $walk:ident),*) => {$(
        impl<T, S, A> $trait<&Set<T, S, A>> for &Set<T, S, A>
        where
            T: Eq + Clone,
            S: KeyHash<T> + Default,
            A: Allocator + Default,
        {
            type Output = Set<T, S, A>;

            #[inline]
            fn $method(self, rhs: &Set<T, S, A>) -> Set<T, S, A> {
                self.$walk(rhs).cloned().collect()
            }
        }
    )*};
}

set_operator!(
    BitOr bitor union,
    BitAnd bitand intersection,
    BitXor bitxor symmetric_difference,
    Sub sub difference
);

// No serde forms: the `serde` feature (hand-written `Serialize`/`Deserialize`, the second on
// an allocator that is `Default`, so `Global`) was removed on 2026-11-20 (D170 C7). blaze's own
// records do not go through serde; its last users were blaze-manifest's `EnvConfig` (the
// cargo layer, a `BTreeMap` now) and one blaze-protocol test.
