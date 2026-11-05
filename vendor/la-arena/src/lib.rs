//! Yet another index-based arena.

#![warn(rust_2018_idioms, unused_lifetimes, semicolon_in_expressions_from_macros)]
#![warn(missing_docs)]

use std::{
    cmp, fmt,
    hash::{Hash, Hasher},
    iter::{Enumerate, FusedIterator},
    marker::PhantomData,
    num::NonZeroU32,
    ops::{Index, IndexMut, Range, RangeInclusive},
};

mod map;
pub use map::{ArenaMap, Entry, OccupiedEntry, VacantEntry};

/// The raw index of a value in an arena.
///
/// blaze: it holds the index's complement, so `u32::MAX` is the one index it can't hold and
/// `Option<RawIdx>` and `Option<Idx<T>>` are 4 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RawIdx(NonZeroU32);

/// The most values an [`Arena`] holds: every index and the end of every range below `u32::MAX`.
const MAX_LEN: usize = (u32::MAX - 1) as usize;

impl RawIdx {
    /// Constructs a [`RawIdx`] from a u32.
    ///
    /// # Panics
    ///
    /// On `u32::MAX`, which no arena gives out.
    pub const fn from_u32(u32: u32) -> Self {
        match NonZeroU32::new(!u32) {
            Some(complement) => RawIdx(complement),
            None => panic!("la_arena::RawIdx can't hold u32::MAX"),
        }
    }

    /// Deconstructs a [`RawIdx`] into the underlying u32.
    pub const fn into_u32(self) -> u32 {
        !self.0.get()
    }
}

impl PartialOrd for RawIdx {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RawIdx {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.into_u32().cmp(&other.into_u32())
    }
}

impl Hash for RawIdx {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.into_u32().hash(state);
    }
}

impl From<RawIdx> for u32 {
    #[inline]
    fn from(raw: RawIdx) -> u32 {
        raw.into_u32()
    }
}

impl From<u32> for RawIdx {
    #[inline]
    fn from(idx: u32) -> RawIdx {
        RawIdx::from_u32(idx)
    }
}

impl fmt::Debug for RawIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.into_u32().fmt(f)
    }
}

impl fmt::Display for RawIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.into_u32().fmt(f)
    }
}

/// The index of a value allocated in an arena that holds `T`s.
pub struct Idx<T> {
    raw: RawIdx,
    _ty: PhantomData<fn() -> T>,
}

impl<T> Ord for Idx<T> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> PartialOrd for Idx<T> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        self.raw.partial_cmp(&other.raw)
    }
}

impl<T> Clone for Idx<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Idx<T> {}

impl<T> PartialEq for Idx<T> {
    fn eq(&self, other: &Idx<T>) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Idx<T> {}

impl<T> Hash for Idx<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut type_name = std::any::type_name::<T>();
        if let Some(idx) = type_name.rfind(':') {
            type_name = &type_name[idx + 1..];
        }
        write!(f, "Idx::<{}>({})", type_name, self.raw)
    }
}

impl<T> Idx<T> {
    /// Creates a new index from a [`RawIdx`].
    pub const fn from_raw(raw: RawIdx) -> Self {
        Idx { raw, _ty: PhantomData }
    }

    /// Converts this index into the underlying [`RawIdx`].
    pub const fn into_raw(self) -> RawIdx {
        self.raw
    }
}

/// A range of densely allocated arena values.
pub struct IdxRange<T> {
    range: Range<u32>,
    _p: PhantomData<T>,
}

impl<T> IdxRange<T> {
    /// Creates a new index range
    /// inclusive of the start value and exclusive of the end value.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let a = arena.alloc("a");
    /// let b = arena.alloc("b");
    /// let c = arena.alloc("c");
    /// let d = arena.alloc("d");
    ///
    /// let range = la_arena::IdxRange::new(b..d);
    /// assert_eq!(&arena[range], &["b", "c"]);
    /// ```
    pub fn new(range: Range<Idx<T>>) -> Self {
        Self { range: range.start.into_raw().into()..range.end.into_raw().into(), _p: PhantomData }
    }

    /// Creates a new index range
    /// inclusive of the start value and end value.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let foo = arena.alloc("foo");
    /// let bar = arena.alloc("bar");
    /// let baz = arena.alloc("baz");
    ///
    /// let range = la_arena::IdxRange::new_inclusive(foo..=baz);
    /// assert_eq!(&arena[range], &["foo", "bar", "baz"]);
    ///
    /// let range = la_arena::IdxRange::new_inclusive(foo..=foo);
    /// assert_eq!(&arena[range], &["foo"]);
    /// ```
    pub fn new_inclusive(range: RangeInclusive<Idx<T>>) -> Self {
        Self {
            range: u32::from(range.start().into_raw())..u32::from(range.end().into_raw()) + 1,
            _p: PhantomData,
        }
    }

    /// Returns whether the index range is empty.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let one = arena.alloc(1);
    /// let two = arena.alloc(2);
    ///
    /// assert!(la_arena::IdxRange::new(one..one).is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    /// Returns the start of the index range.
    pub fn start(&self) -> Idx<T> {
        Idx::from_raw(RawIdx::from(self.range.start))
    }

    /// Returns the end of the index range.
    pub fn end(&self) -> Idx<T> {
        Idx::from_raw(RawIdx::from(self.range.end))
    }
}

impl<T> Iterator for IdxRange<T> {
    type Item = Idx<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.range.next().map(|raw| Idx::from_raw(raw.into()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.range.count()
    }

    fn last(self) -> Option<Self::Item>
    where
        Self: Sized,
    {
        self.range.last().map(|raw| Idx::from_raw(raw.into()))
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.range.nth(n).map(|raw| Idx::from_raw(raw.into()))
    }
}

impl<T> DoubleEndedIterator for IdxRange<T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.range.next_back().map(|raw| Idx::from_raw(raw.into()))
    }
}

impl<T> ExactSizeIterator for IdxRange<T> {}

impl<T> FusedIterator for IdxRange<T> {}

impl<T> fmt::Debug for IdxRange<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(&format!("IdxRange::<{}>", std::any::type_name::<T>()))
            .field(&self.range)
            .finish()
    }
}

impl<T> Clone for IdxRange<T> {
    fn clone(&self) -> Self {
        Self { range: self.range.clone(), _p: PhantomData }
    }
}

impl<T> PartialEq for IdxRange<T> {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
    }
}

impl<T> Eq for IdxRange<T> {}

/// Yet another index-based arena.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Arena<T> {
    data: Vec<T>,
}

impl<T: fmt::Debug> fmt::Debug for Arena<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Arena").field("len", &self.len()).field("data", &self.data).finish()
    }
}

impl<T> Arena<T> {
    /// Creates a new empty arena.
    ///
    /// ```
    /// let arena: la_arena::Arena<i32> = la_arena::Arena::new();
    /// assert!(arena.is_empty());
    /// ```
    pub const fn new() -> Arena<T> {
        Arena { data: Vec::new() }
    }

    /// Create a new empty arena with specific capacity.
    ///
    /// ```
    /// let arena: la_arena::Arena<i32> = la_arena::Arena::with_capacity(42);
    /// assert!(arena.is_empty());
    /// ```
    pub fn with_capacity(capacity: usize) -> Arena<T> {
        Arena { data: Vec::with_capacity(capacity) }
    }

    /// Empties the arena, removing all contained values.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    ///
    /// arena.alloc(1);
    /// arena.alloc(2);
    /// arena.alloc(3);
    /// assert_eq!(arena.len(), 3);
    ///
    /// arena.clear();
    /// assert!(arena.is_empty());
    /// ```
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Returns the length of the arena.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// assert_eq!(arena.len(), 0);
    ///
    /// arena.alloc("foo");
    /// assert_eq!(arena.len(), 1);
    ///
    /// arena.alloc("bar");
    /// assert_eq!(arena.len(), 2);
    ///
    /// arena.alloc("baz");
    /// assert_eq!(arena.len(), 3);
    /// ```
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns whether the arena contains no elements.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// assert!(arena.is_empty());
    ///
    /// arena.alloc(0.5);
    /// assert!(!arena.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Allocates a new value on the arena, returning the value’s index.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let idx = arena.alloc(50);
    ///
    /// assert_eq!(arena[idx], 50);
    /// ```
    pub fn alloc(&mut self, value: T) -> Idx<T> {
        assert!(self.data.len() < MAX_LEN, "a la_arena::Arena holds at most {MAX_LEN} values");
        let idx = self.next_idx();
        self.data.push(value);
        idx
    }

    /// Densely allocates multiple values, returning the values’ index range.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let range = arena.alloc_many(0..4);
    ///
    /// assert_eq!(arena[range], [0, 1, 2, 3]);
    /// ```
    pub fn alloc_many<II: IntoIterator<Item = T>>(&mut self, iter: II) -> IdxRange<T> {
        let start = self.next_idx();
        self.extend(iter);
        let end = self.next_idx();
        IdxRange::new(start..end)
    }

    /// Returns an iterator over the arena’s elements.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let idx1 = arena.alloc(20);
    /// let idx2 = arena.alloc(40);
    /// let idx3 = arena.alloc(60);
    ///
    /// let mut iterator = arena.iter();
    /// assert_eq!(iterator.next(), Some((idx1, &20)));
    /// assert_eq!(iterator.next(), Some((idx2, &40)));
    /// assert_eq!(iterator.next(), Some((idx3, &60)));
    /// ```
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (Idx<T>, &T)> + ExactSizeIterator + DoubleEndedIterator + Clone {
        self.data
            .iter()
            .enumerate()
            .map(|(idx, value)| (Idx::from_raw(RawIdx::from_u32(idx as u32)), value))
    }

    /// Returns an iterator over the arena’s mutable elements.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let idx1 = arena.alloc(20);
    ///
    /// assert_eq!(arena[idx1], 20);
    ///
    /// let mut iterator = arena.iter_mut();
    /// *iterator.next().unwrap().1 = 10;
    /// drop(iterator);
    ///
    /// assert_eq!(arena[idx1], 10);
    /// ```
    pub fn iter_mut(
        &mut self,
    ) -> impl Iterator<Item = (Idx<T>, &mut T)> + ExactSizeIterator + DoubleEndedIterator {
        self.data
            .iter_mut()
            .enumerate()
            .map(|(idx, value)| (Idx::from_raw(RawIdx::from_u32(idx as u32)), value))
    }

    /// Returns an iterator over the arena’s values.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let idx1 = arena.alloc(20);
    /// let idx2 = arena.alloc(40);
    /// let idx3 = arena.alloc(60);
    ///
    /// let mut iterator = arena.values();
    /// assert_eq!(iterator.next(), Some(&20));
    /// assert_eq!(iterator.next(), Some(&40));
    /// assert_eq!(iterator.next(), Some(&60));
    /// ```
    pub fn values(&self) -> impl Iterator<Item = &T> + ExactSizeIterator + DoubleEndedIterator {
        self.data.iter()
    }

    /// Returns an iterator over the arena’s mutable values.
    ///
    /// ```
    /// let mut arena = la_arena::Arena::new();
    /// let idx1 = arena.alloc(20);
    ///
    /// assert_eq!(arena[idx1], 20);
    ///
    /// let mut iterator = arena.values_mut();
    /// *iterator.next().unwrap() = 10;
    /// drop(iterator);
    ///
    /// assert_eq!(arena[idx1], 10);
    /// ```
    pub fn values_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut T> + ExactSizeIterator + DoubleEndedIterator {
        self.data.iter_mut()
    }

    /// Reallocates the arena to make it take up as little space as possible.
    pub fn shrink_to_fit(&mut self) {
        self.data.shrink_to_fit();
    }

    /// Returns the index of the next value allocated on the arena.
    ///
    /// This method should remain private to make creating invalid `Idx`s harder.
    fn next_idx(&self) -> Idx<T> {
        Idx::from_raw(RawIdx::from_u32(self.data.len() as u32))
    }
}

impl<T> AsMut<[T]> for Arena<T> {
    fn as_mut(&mut self) -> &mut [T] {
        self.data.as_mut()
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Arena<T> {
        Arena { data: Vec::new() }
    }
}

impl<T> Index<Idx<T>> for Arena<T> {
    type Output = T;
    fn index(&self, idx: Idx<T>) -> &T {
        let idx = idx.into_raw().into_u32() as usize;
        &self.data[idx]
    }
}

impl<T> IndexMut<Idx<T>> for Arena<T> {
    fn index_mut(&mut self, idx: Idx<T>) -> &mut T {
        let idx = idx.into_raw().into_u32() as usize;
        &mut self.data[idx]
    }
}

impl<T> Index<IdxRange<T>> for Arena<T> {
    type Output = [T];
    fn index(&self, range: IdxRange<T>) -> &[T] {
        let start = range.range.start as usize;
        let end = range.range.end as usize;
        &self.data[start..end]
    }
}

impl<T> FromIterator<T> for Arena<T> {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
    {
        let data = Vec::from_iter(iter);
        assert!(data.len() <= MAX_LEN, "a la_arena::Arena holds at most {MAX_LEN} values");
        Arena { data }
    }
}

/// An iterator over the arena’s elements.
pub struct IntoIter<T>(Enumerate<<Vec<T> as IntoIterator>::IntoIter>);

impl<T> Iterator for IntoIter<T> {
    type Item = (Idx<T>, T);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(idx, value)| (Idx::from_raw(RawIdx::from_u32(idx as u32)), value))
    }
}

impl<T> IntoIterator for Arena<T> {
    type Item = (Idx<T>, T);

    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter(self.data.into_iter().enumerate())
    }
}

impl<T> Extend<T> for Arena<T> {
    fn extend<II: IntoIterator<Item = T>>(&mut self, iter: II) {
        for t in iter {
            self.alloc(t);
        }
    }
}

const _: () = assert!(std::mem::size_of::<Option<RawIdx>>() == 4);
const _: () = assert!(std::mem::size_of::<Option<Idx<u64>>>() == 4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raw_index_keeps_its_value_and_order() {
        for raw in [0, 1, 2, 1 << 31, u32::MAX - 2, u32::MAX - 1] {
            assert_eq!(RawIdx::from_u32(raw).into_u32(), raw);
            assert_eq!(u32::from(RawIdx::from(raw)), raw);
            assert_eq!(format!("{:?} {}", RawIdx::from(raw), RawIdx::from(raw)), format!("{raw} {raw}"));
        }
        assert!(RawIdx::from_u32(0) < RawIdx::from_u32(1));
        assert!(RawIdx::from_u32(u32::MAX - 2) < RawIdx::from_u32(u32::MAX - 1));
        let mut arena = Arena::new();
        let (a, b) = (arena.alloc('a'), arena.alloc('b'));
        assert!(a < b);
        assert_eq!(a.into_raw().into_u32(), 0);
        assert_eq!(arena.iter().map(|(idx, _)| idx).collect::<Vec<_>>(), [a, b]);
    }

    #[test]
    fn a_raw_index_hashes_as_its_value() {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let hasher = BuildHasherDefault::<DefaultHasher>::default();
        assert_eq!(hasher.hash_one(RawIdx::from_u32(7)), hasher.hash_one(7u32));
    }

    #[test]
    #[should_panic(expected = "la_arena::RawIdx can't hold u32::MAX")]
    fn u32_max_is_refused() {
        let _ = RawIdx::from_u32(u32::MAX);
    }
}
