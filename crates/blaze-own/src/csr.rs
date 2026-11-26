//! Compressed Sparse Row ([`Csr`]) container: flat, zero-unsafe storage for ragged rows of `T`.
//!
//! Stores all row elements contiguously in `values: Vec<T, A>` and row boundaries in
//! `offsets: Vec<u32, A>` starting with `[0]`, so row `i` is `&values[offsets[i]..offsets[i + 1]]`
//! with only two allocations total (4 bytes of overhead per row plus 4 bytes for the leading `0`).
//!
//! Built once or appended row-by-row, then read as slices with zero indirection and zero per-row
//! heap headers. Row counts past `u32::MAX` values are refused with
//! [`AllocFailure::CapacityOverflow`].

use core::alloc::Allocator;
use core::fmt;
use core::iter::FusedIterator;

use crate::{
    Int,
    error::{AllocFailure, fail},
    vec::Vec,
};

/// The type named in a capacity-overflow refusal.
const WHAT: &str = "Csr values (row offsets are u32: at most u32::MAX values)";

/// Converts `len` to a `u32` CSR offset or returns [`AllocFailure::CapacityOverflow`].
#[inline]
fn offset_of(len: usize, additional: usize) -> Result<u32, AllocFailure> {
    u32::try_from(len).map_err(|_| AllocFailure::CapacityOverflow { what: WHAT, len, additional })
}

/// A zero-unsafe Compressed Sparse Row container of `T` in allocator `A`.
///
/// # Invariants
///
/// - `offsets` starts with `0` (`offsets.len() == self.len() + 1`).
/// - `offsets` is monotonically non-decreasing, and its last element equals `values.len()`.
pub struct Csr<T, A: Allocator> {
    offsets: Vec<u32, A>,
    values: Vec<T, A>,
}

impl<T, A: Allocator + Copy> Csr<T, A> {
    /// Creates an empty [`Csr`] in `alloc` with `offsets = [0]`.
    #[track_caller]
    pub fn new_in(alloc: A) -> Csr<T, A> {
        match Csr::try_new_in(alloc) {
            Ok(csr) => csr,
            Err(failure) => fail(failure),
        }
    }

    /// Creates an empty [`Csr`] in `alloc`, or returns [`AllocFailure`].
    pub fn try_new_in(alloc: A) -> Result<Csr<T, A>, AllocFailure> {
        Csr::try_with_capacity_in(0, 0, alloc)
    }

    /// Creates an empty [`Csr`] with pre-allocated capacity for `rows` rows and `values` elements.
    #[track_caller]
    pub fn with_capacity_in(rows: usize, values: usize, alloc: A) -> Csr<T, A> {
        match Csr::try_with_capacity_in(rows, values, alloc) {
            Ok(csr) => csr,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible form of [`Csr::with_capacity_in`].
    pub fn try_with_capacity_in(rows: usize, values: usize, alloc: A) -> Result<Csr<T, A>, AllocFailure> {
        let offset_cap = rows.checked_add(1).ok_or(AllocFailure::CapacityOverflow {
            what: WHAT,
            len: rows,
            additional: 1,
        })?;
        let mut offsets = Vec::try_with_capacity_in(offset_cap, alloc)?;
        let values = Vec::try_with_capacity_in(values, alloc)?;
        offsets.try_push(0).map_err(|refused| refused.why)?;
        Ok(Csr { offsets, values })
    }
}

impl<T, A: Allocator> Csr<T, A> {
    /// Returns the number of rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Returns `true` if there are no rows.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the total number of elements across all rows.
    #[inline]
    pub fn values_len(&self) -> usize {
        self.values.len()
    }

    /// Returns the flat slice of all elements across all rows.
    #[inline]
    pub fn values(&self) -> &[T] {
        self.values.as_slice()
    }

    /// Returns the mutable flat slice of all elements across all rows.
    #[inline]
    pub fn values_mut(&mut self) -> &mut [T] {
        self.values.as_mut_slice()
    }

    /// Returns the row-boundary offsets slice (length `len() + 1`, starting at `0`).
    #[inline]
    pub fn offsets(&self) -> &[u32] {
        self.offsets.as_slice()
    }

    /// Appends a new row from `items`, returning its row index.
    ///
    /// If the iterator panics mid-iteration, already-pushed items for this row are dropped and
    /// `self` retains only previously committed rows.
    #[track_caller]
    #[inline]
    pub fn push_row(&mut self, items: impl IntoIterator<Item = T>) -> usize {
        match self.try_push_row(items) {
            Ok(idx) => idx,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible form of [`Csr::push_row`]. Leaves `self` unchanged on allocation or `u32` offset
    /// overflow failure.
    #[inline]
    pub fn try_push_row(&mut self, items: impl IntoIterator<Item = T>) -> Result<usize, AllocFailure> {
        /// Rolls back `values` to `start` if an iterator panics or an error returns early.
        struct Cut<'a, T, A: Allocator> {
            values: &'a mut Vec<T, A>,
            start: usize,
        }
        impl<T, A: Allocator> Drop for Cut<'_, T, A> {
            fn drop(&mut self) {
                self.values.truncate(self.start);
            }
        }

        let row_idx = self.len();
        self.offsets.try_reserve(1)?;
        let iter = items.into_iter();
        let (lower, _) = iter.size_hint();
        let start = self.values.len();
        let min_end = start.checked_add(lower).ok_or(AllocFailure::CapacityOverflow {
            what: WHAT,
            len: start,
            additional: lower,
        })?;
        let _ = offset_of(min_end, lower)?;
        if lower > 0 {
            self.values.try_reserve(lower)?;
        }
        let max_values = u32::MAX.widen_to::<usize>();
        let cut = Cut { values: &mut self.values, start };
        for item in iter {
            if cut.values.len() >= max_values {
                return Err(AllocFailure::CapacityOverflow {
                    what: WHAT,
                    len: start,
                    additional: cut.values.len().saturating_sub(start).saturating_add(1),
                });
            }
            if let Err(refused) = cut.values.try_push(item) {
                return Err(refused.why);
            }
        }
        let end = offset_of(cut.values.len(), 0)?;
        if let Err(refused) = self.offsets.try_push(end) {
            return Err(refused.why);
        }
        core::mem::forget(cut);
        Ok(row_idx)
    }

    /// Appends a new row copied from `slice`, returning its row index.
    #[track_caller]
    #[inline]
    pub fn push_slice(&mut self, slice: &[T]) -> usize
    where
        T: Copy,
    {
        match self.try_push_slice(slice) {
            Ok(idx) => idx,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible form of [`Csr::push_slice`]. Leaves `self` unchanged on failure.
    #[inline]
    pub fn try_push_slice(&mut self, slice: &[T]) -> Result<usize, AllocFailure>
    where
        T: Copy,
    {
        let row_idx = self.len();
        let start = self.values.len();
        let new_len = start.checked_add(slice.len()).ok_or(AllocFailure::CapacityOverflow {
            what: WHAT,
            len: start,
            additional: slice.len(),
        })?;
        let end = offset_of(new_len, slice.len())?;
        self.offsets.try_reserve(1)?;
        self.values.try_extend_from_slice(slice)?;
        if let Err(refused) = self.offsets.try_push(end) {
            self.values.truncate(start);
            return Err(refused.why);
        }
        Ok(row_idx)
    }

    /// Returns row `idx` as a slice, or `None` if `idx >= self.len()`.
    #[inline]
    pub fn row(&self, idx: usize) -> Option<&[T]> {
        let start = self.offsets.get(idx)?.widen_to::<usize>();
        let end = self.offsets.get(idx.checked_add(1)?)?.widen_to::<usize>();
        self.values.get(start..end)
    }

    /// Returns row `idx` as a mutable slice, or `None` if `idx >= self.len()`.
    #[inline]
    pub fn row_mut(&mut self, idx: usize) -> Option<&mut [T]> {
        let start = self.offsets.get(idx)?.widen_to::<usize>();
        let end = self.offsets.get(idx.checked_add(1)?)?.widen_to::<usize>();
        self.values.get_mut(start..end)
    }

    /// Returns an iterator over all rows in order.
    #[inline]
    pub fn rows(&self) -> Rows<'_, T> {
        Rows {
            values: self.values.as_slice(),
            offsets: self.offsets.as_slice(),
        }
    }

    /// Removes all rows while retaining backing allocations (`offsets` resets to `[0]`).
    pub fn clear(&mut self) {
        self.values.clear();
        self.offsets.truncate(1);
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
    }

    /// Returns the total allocated buffer bytes held by `offsets` and `values`.
    pub fn heap_bytes(&self) -> usize {
        self.values
            .capacity()
            .saturating_mul(size_of::<T>())
            .saturating_add(self.offsets.capacity().saturating_mul(size_of::<u32>()))
    }
}

/// An iterator over the row slices of a [`Csr`].
pub struct Rows<'a, T> {
    values: &'a [T],
    offsets: &'a [u32],
}

impl<'a, T> Iterator for Rows<'a, T> {
    type Item = &'a [T];

    #[inline]
    fn next(&mut self) -> Option<&'a [T]> {
        let (start_ref, rest) = self.offsets.split_first()?;
        let end_ref = rest.first()?;
        self.offsets = rest;
        let start = start_ref.widen_to::<usize>();
        let end = end_ref.widen_to::<usize>();
        self.values.get(start..end)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.offsets.len().saturating_sub(1);
        (remaining, Some(remaining))
    }
}

impl<'a, T> DoubleEndedIterator for Rows<'a, T> {
    #[inline]
    fn next_back(&mut self) -> Option<&'a [T]> {
        let (end_ref, rest) = self.offsets.split_last()?;
        let start_ref = rest.last()?;
        self.offsets = rest;
        let start = start_ref.widen_to::<usize>();
        let end = end_ref.widen_to::<usize>();
        self.values.get(start..end)
    }
}

impl<T> ExactSizeIterator for Rows<'_, T> {}

impl<T> FusedIterator for Rows<'_, T> {}

impl<'a, T, A: Allocator> IntoIterator for &'a Csr<T, A> {
    type Item = &'a [T];
    type IntoIter = Rows<'a, T>;

    #[inline]
    fn into_iter(self) -> Rows<'a, T> {
        self.rows()
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Csr<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.rows()).finish()
    }
}

impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<Csr<T, B>> for Csr<T, A> {
    fn eq(&self, other: &Csr<T, B>) -> bool {
        self.offsets.as_slice() == other.offsets.as_slice()
            && self.values.as_slice() == other.values.as_slice()
    }
}

impl<T: Eq, A: Allocator> Eq for Csr<T, A> {}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::alloc::Global;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::vec::Vec as StdVec;

    use crate::{
        AllocFailure,
        csr::Csr,
        testing::{Counting, every_failure},
    };

    #[test]
    fn csr_empty_push_slice_row_iter_and_clear() {
        let counting = Counting::new(&Global);
        {
            let mut csr = Csr::<u32, _>::new_in(&counting);
            assert!(csr.is_empty());
            assert_eq!(csr.len(), 0);
            assert_eq!(csr.values_len(), 0);
            assert_eq!(csr.offsets(), &[0]);
            assert_eq!(csr.row(0), None);
            assert_eq!(csr.rows().len(), 0);
            assert_eq!(csr.rows().next(), None);
            assert_eq!(csr.rows().next_back(), None);

            assert_eq!(csr.push_row([10, 20, 30]), 0);
            assert_eq!(csr.push_slice(&[]), 1);
            assert_eq!(csr.push_slice(&[40, 50]), 2);
            assert_eq!(csr.push_row(core::iter::empty()), 3);
            assert_eq!(csr.push_row([60]), 4);

            assert!(!csr.is_empty());
            assert_eq!(csr.len(), 5);
            assert_eq!(csr.values_len(), 6);
            assert_eq!(csr.offsets(), &[0, 3, 3, 5, 5, 6]);
            assert_eq!(csr.values(), &[10, 20, 30, 40, 50, 60]);

            assert_eq!(csr.row(0), Some(&[10, 20, 30][..]));
            assert_eq!(csr.row(1), Some(&[][..]));
            assert_eq!(csr.row(2), Some(&[40, 50][..]));
            assert_eq!(csr.row(3), Some(&[][..]));
            assert_eq!(csr.row(4), Some(&[60][..]));
            assert_eq!(csr.row(5), None);

            if let Some(r) = csr.row_mut(2) {
                r[1] = 55;
            }
            assert_eq!(csr.row(2), Some(&[40, 55][..]));

            let forward: StdVec<&[u32]> = csr.rows().collect();
            assert_eq!(forward, std::vec![&[10, 20, 30][..], &[][..], &[40, 55][..], &[][..], &[60][..]]);

            let reverse: StdVec<&[u32]> = csr.rows().rev().collect();
            assert_eq!(reverse, std::vec![&[60][..], &[][..], &[40, 55][..], &[][..], &[10, 20, 30][..]]);

            let mut mixed = csr.rows();
            assert_eq!(mixed.len(), 5);
            assert_eq!(mixed.next(), Some(&[10, 20, 30][..]));
            assert_eq!(mixed.next_back(), Some(&[60][..]));
            assert_eq!(mixed.len(), 3);
            assert_eq!(mixed.next(), Some(&[][..]));
            assert_eq!(mixed.next_back(), Some(&[][..]));
            assert_eq!(mixed.next(), Some(&[40, 55][..]));
            assert_eq!(mixed.next(), None);
            assert_eq!(mixed.next_back(), None);

            csr.clear();
            assert!(csr.is_empty());
            assert_eq!(csr.len(), 0);
            assert_eq!(csr.values_len(), 0);
            assert_eq!(csr.offsets(), &[0]);
            assert_eq!(csr.row(0), None);

            assert_eq!(csr.push_slice(&[7, 8]), 0);
            assert_eq!(csr.row(0), Some(&[7, 8][..]));
        }
        counting.assert_empty();
    }

    #[test]
    fn csr_with_capacity_and_equality() {
        let counting = Counting::new(&Global);
        {
            let mut a = Csr::<u16, _>::with_capacity_in(4, 10, &counting);
            let mut b = Csr::<u16, _>::new_in(&counting);
            a.push_slice(&[1, 2, 3]);
            a.push_row([4]);
            b.push_row([1, 2, 3]);
            b.push_slice(&[4]);
            assert_eq!(a, b);
            b.push_slice(&[]);
            assert_ne!(a, b);
        }
        counting.assert_empty();
    }

    #[test]
    fn csr_push_row_rolls_back_and_drops_on_iterator_panic() {
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Tracked(u32);
        impl Drop for Tracked {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        DROPS.store(0, Ordering::Relaxed);

        let counting = Counting::new(&Global);
        {
            let mut csr = Csr::<Tracked, _>::new_in(&counting);
            csr.push_row([Tracked(1), Tracked(2)]);
            assert_eq!(DROPS.load(Ordering::Relaxed), 0);

            let panicked = catch_unwind(AssertUnwindSafe(|| {
                csr.push_row((0..5u32).map(|i| {
                    if i == 3 {
                        panic!("boom mid-row");
                    }
                    Tracked(10 + i)
                }));
            }));
            assert!(panicked.is_err());
            // The 3 items yielded before the panic were dropped; committed row 0 remains intact.
            assert_eq!(DROPS.load(Ordering::Relaxed), 3);
            assert_eq!(csr.len(), 1);
            assert_eq!(csr.values_len(), 2);
            assert_eq!(csr.offsets(), &[0, 2]);
            let row0: StdVec<u32> = csr.row(0).unwrap().iter().map(|t| t.0).collect();
            assert_eq!(row0, std::vec![1, 2]);
        }
        assert_eq!(DROPS.load(Ordering::Relaxed), 5);
        counting.assert_empty();
    }

    #[test]
    fn csr_zst_values_work_and_drop_cleanly() {
        static ZST_DROPS: AtomicUsize = AtomicUsize::new(0);
        struct ZstDrop;
        impl Drop for ZstDrop {
            fn drop(&mut self) {
                ZST_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        ZST_DROPS.store(0, Ordering::Relaxed);

        let counting = Counting::new(&Global);
        {
            let mut csr = Csr::<ZstDrop, _>::new_in(&counting);
            csr.push_row([ZstDrop, ZstDrop]);
            csr.push_row(core::iter::empty());
            csr.push_row([ZstDrop, ZstDrop, ZstDrop]);
            assert_eq!(csr.len(), 3);
            assert_eq!(csr.values_len(), 5);
            assert_eq!(csr.row(0).map(|r| r.len()), Some(2));
            assert_eq!(csr.row(1).map(|r| r.len()), Some(0));
            assert_eq!(csr.row(2).map(|r| r.len()), Some(3));
            csr.clear();
            assert_eq!(ZST_DROPS.load(Ordering::Relaxed), 5);
        }
        counting.assert_empty();
    }

    #[test]
    fn csr_try_forms_fail_cleanly_at_every_allocation() {
        every_failure(&Global, |alloc| {
            let mut csr = Csr::<u32, _>::try_with_capacity_in(1, 2, alloc)?;
            csr.try_push_row([1, 2])?;
            csr.try_push_slice(&[3, 4, 5, 6])?;
            csr.try_push_row(core::iter::empty())?;
            csr.try_push_row(7..20u32)?;
            assert_eq!(csr.len(), 4);
            assert_eq!(csr.row(0), Some(&[1, 2][..]));
            assert_eq!(csr.row(1), Some(&[3, 4, 5, 6][..]));
            assert_eq!(csr.row(2), Some(&[][..]));
            assert_eq!(csr.row(3).map(|r| r.len()), Some(13));
            Ok::<(), AllocFailure>(())
        })
        .unwrap();

        every_failure(&Global, |alloc| {
            let mut csr = Csr::<u64, _>::try_new_in(alloc)?;
            for row in 0..8u64 {
                csr.try_push_slice(&[row, row + 1, row + 2])?;
            }
            assert_eq!(csr.len(), 8);
            assert_eq!(csr.values_len(), 24);
            Ok::<(), AllocFailure>(())
        })
        .unwrap();
    }
}
