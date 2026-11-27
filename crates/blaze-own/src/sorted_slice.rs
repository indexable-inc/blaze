//! [`SortedSlice`]: a zero-allocation borrowed proof wrapper over `&'a [(K, V)]` whose keys are
//! guaranteed to be sorted ascending (`K: Ord`), enabling O(log N) lookups by construction.
//!
//! Taking `SortedSlice<'a, K, V>` at API boundaries prevents callers from passing unsorted slices
//! into binary search and removes ad-hoc runtime `is_sorted` copies inside hot functions (Rule 73,
//! Rule 74).

use core::{alloc::Allocator, borrow::Borrow, slice};

use crate::vec::Vec;

/// A borrowed slice `&'a [(K, V)]` proven to be sorted ascending (non-decreasing) by key `K`.
///
/// Zero allocation, `#![no_std]`, zero `unsafe`.
#[derive(Debug, PartialEq, Eq)]
pub struct SortedSlice<'a, K, V>(&'a [(K, V)]);

impl<K, V> Clone for SortedSlice<'_, K, V> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for SortedSlice<'_, K, V> {}

impl<K, V> Default for SortedSlice<'_, K, V> {
    #[inline]
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<'a, K, V> SortedSlice<'a, K, V> {
    /// An empty sorted slice.
    pub const EMPTY: Self = Self(&[]);

    /// Returns an empty [`SortedSlice`].
    #[inline]
    pub const fn empty() -> Self {
        Self(&[])
    }

    /// Returns the number of `(K, V)` entries.
    #[inline]
    pub const fn len(self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the slice contains no entries.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// Returns the underlying sorted slice.
    #[inline]
    pub const fn as_slice(self) -> &'a [(K, V)] {
        self.0
    }

    /// Returns an iterator over the sorted `(K, V)` entries.
    #[inline]
    pub fn iter(self) -> slice::Iter<'a, (K, V)> {
        self.0.iter()
    }
}

impl<'a, K: Ord, V> SortedSlice<'a, K, V> {
    /// Verifies in O(N) that `slice` is sorted ascending by key and returns the proof wrapper, or
    /// `None` if any adjacent pair is out of order.
    #[blaze::cost(O(n), space = O(1))]
    #[inline]
    pub fn try_from_sorted(slice: &'a [(K, V)]) -> Option<Self> {
        if slice.is_sorted_by(|(a, _), (b, _)| a <= b) {
            Some(Self(slice))
        } else {
            None
        }
    }

    /// Sorts `slice` in place ascending by key and returns the proof wrapper.
    #[blaze::cost(O(n log n), space = O(1))]
    #[inline]
    pub fn sort_slice(slice: &'a mut [(K, V)]) -> Self {
        slice.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        Self(slice)
    }

    /// Sorts `vec` in place ascending by key and returns a borrowed proof wrapper over its entries.
    #[blaze::cost(O(n log n), space = O(1))]
    #[inline]
    pub fn sort_and_wrap<A: Allocator>(vec: &'a mut Vec<(K, V), A>) -> Self {
        Self::sort_slice(vec.as_mut_slice())
    }

    /// Looks up `key` in O(log N) via binary search, returning a reference to the associated value
    /// if present.
    #[blaze::cost(O(log n), space = O(1))]
    #[inline]
    pub fn get<Q: Ord + ?Sized>(self, key: &Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
    {
        let idx = self.0.binary_search_by(|(k, _)| k.borrow().cmp(key)).ok()?;
        Some(&self.0[idx].1)
    }

    /// Returns `true` if `key` is present in O(log N).
    #[blaze::cost(O(log n), space = O(1))]
    #[inline]
    pub fn contains_key<Q: Ord + ?Sized>(self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.0.binary_search_by(|(k, _)| k.borrow().cmp(key)).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::SortedSlice;
    use crate::{Vec, testing};

    #[test]
    fn empty_and_default_sorted_slice() {
        let e1: SortedSlice<'_, u32, &str> = SortedSlice::EMPTY;
        let e2: SortedSlice<'_, u32, &str> = SortedSlice::empty();
        let e3: SortedSlice<'_, u32, &str> = SortedSlice::default();
        assert_eq!(e1, e2);
        assert_eq!(e2, e3);
        assert!(e1.is_empty());
        assert_eq!(e1.len(), 0);
        assert!(e1.as_slice().is_empty());
        assert_eq!(e1.get(&0), None);
        assert!(!e1.contains_key(&42));
    }

    #[test]
    fn try_from_sorted_validates_ordering() {
        assert_eq!(SortedSlice::try_from_sorted(&[] as &[(u32, u8)]), Some(SortedSlice::EMPTY));

        let single = [(7u32, "seven")];
        let s = SortedSlice::try_from_sorted(&single).expect("single entry is sorted");
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
        assert_eq!(s.get(&7), Some(&"seven"));
        assert_eq!(s.get(&6), None);
        assert_eq!(s.get(&8), None);

        let sorted = [(1u32, 10u32), (3, 30), (3, 31), (10, 100), (99, 990)];
        let s = SortedSlice::try_from_sorted(&sorted).expect("non-decreasing keys are valid");
        assert_eq!(s.len(), 5);
        assert_eq!(s.get(&1), Some(&10));
        assert!(matches!(s.get(&3), Some(&30 | &31)));
        assert_eq!(s.get(&10), Some(&100));
        assert_eq!(s.get(&99), Some(&990));
        assert_eq!(s.get(&0), None);
        assert_eq!(s.get(&2), None);
        assert_eq!(s.get(&50), None);
        assert_eq!(s.get(&100), None);

        let unsorted = [(1u32, 10u32), (5, 50), (4, 40)];
        assert_eq!(SortedSlice::try_from_sorted(&unsorted), None);

        let reversed = [(2u32, 'b'), (1, 'a')];
        assert_eq!(SortedSlice::try_from_sorted(&reversed), None);
    }

    #[test]
    fn sort_and_wrap_sorts_in_place_and_supports_non_copy_keys() {
        let alloc = testing::heap().alloc();
        let mut pairs: Vec<(u32, u64), _> = Vec::new_in(alloc);
        for &(k, v) in &[(42u32, 420u64), (5, 50), (100, 1000), (0, 1), (17, 170)] {
            pairs.push((k, v));
        }

        let sorted = SortedSlice::sort_and_wrap(&mut pairs);
        assert_eq!(sorted.len(), 5);
        assert_eq!(sorted.as_slice(), &[(0, 1), (5, 50), (17, 170), (42, 420), (100, 1000)]);
        assert_eq!(sorted.get(&0), Some(&1));
        assert_eq!(sorted.get(&5), Some(&50));
        assert_eq!(sorted.get(&17), Some(&170));
        assert_eq!(sorted.get(&42), Some(&420));
        assert_eq!(sorted.get(&100), Some(&1000));
        assert_eq!(sorted.get(&1), None);
        assert_eq!(sorted.get(&99), None);
        assert_eq!(sorted.get(&101), None);

        // Verify non-Copy keys (`std::string::String`) and `Borrow<str>` lookups work cleanly.
        let mut owned_keys: Vec<(std::string::String, usize), _> = Vec::new_in(alloc);
        owned_keys.push((std::string::String::from("charlie"), 3));
        owned_keys.push((std::string::String::from("alpha"), 1));
        owned_keys.push((std::string::String::from("bravo"), 2));
        let wrapped = SortedSlice::sort_and_wrap(&mut owned_keys);
        assert_eq!(wrapped.get("alpha"), Some(&1));
        assert_eq!(wrapped.get("bravo"), Some(&2));
        assert_eq!(wrapped.get("charlie"), Some(&3));
        assert_eq!(wrapped.get("delta"), None);
        assert!(wrapped.contains_key("bravo"));
        assert!(!wrapped.contains_key("echo"));
    }
}
