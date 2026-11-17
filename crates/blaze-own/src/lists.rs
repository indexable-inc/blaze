//! Rows of items in two arrays: every item end to end in one, and where each row ends in the
//! other. In place of `Vec<Vec<T>>` (an allocation, a pointer, a length and a capacity per row,
//! the rows scattered over the heap) and of `HashMap<K, Vec<V>>` keyed by a dense index (the
//! multimap form, [`Lists::from_pairs_in`]); docs/v2/plan/design/std.md, "What std makes easy to
//! get slow".
//!
//! A row is pushed whole ([`Lists::push_row`]) or opened and filled ([`Lists::open_row`]); rows
//! are read as slices ([`Lists::row`]) and iterated in order. A row cannot grow once a later
//! one is pushed: these are built once and read many times, which is what the layout is for.
//!
//! The ends are `u32`: half the bytes of `usize` per row; a build past `u32::MAX` items is
//! refused with [`AllocFailure::CapacityOverflow`].

use core::{alloc::Allocator, fmt, iter::FusedIterator};

use crate::{
    error::{AllocFailure, fail},
    vec::{Vec, out_of_bounds},
};

/// Rows of `T` in one item array and one array of row ends ([module](self)): two blocks in all, 4 bytes a row past its items.
pub struct Lists<T, A: Allocator> {
    items: Vec<T, A>,
    /// Where each row ends in `items`; row `i` is `ends[i - 1]..ends[i]` (`0..ends[0]` for the
    /// first). Non-decreasing, and the last is `items.len()`.
    ends: Vec<u32, A>,
}

/// The type named in a refusal, saying the limit.
const WHAT: &str = "Lists items (row ends are u32: at most u32::MAX items)";

/// `len` as a row end, or the refusal: more than `u32::MAX` items.
fn end_of(len: usize, additional: usize) -> Result<u32, AllocFailure> {
    u32::try_from(len).map_err(|_| AllocFailure::CapacityOverflow { what: WHAT, len, additional })
}

impl<T, A: Allocator + Clone> Lists<T, A> {
    pub fn new_in(alloc: A) -> Lists<T, A> {
        Lists { items: Vec::new_in(alloc.clone()), ends: Vec::new_in(alloc) }
    }

    /// Room for `rows` rows of `items` items in all, in two allocations.
    #[track_caller]
    pub fn with_capacity_in(rows: usize, items: usize, alloc: A) -> Lists<T, A> {
        match Lists::try_with_capacity_in(rows, items, alloc) {
            Ok(lists) => lists,
            Err(failure) => fail(failure),
        }
    }

    pub fn try_with_capacity_in(rows: usize, items: usize, alloc: A) -> Result<Lists<T, A>, AllocFailure> {
        Ok(Lists {
            items: Vec::try_with_capacity_in(items, alloc.clone())?,
            ends: Vec::try_with_capacity_in(rows, alloc)?,
        })
    }

    /// Rows from `(row, item)` pairs, `rows` of them, each row's items in the order the pairs
    /// gave them: a counting sort, two passes over the pairs and no comparison. The multimap
    /// form of `HashMap<usize, Vec<T>>`, with the rows' keys dense.
    ///
    /// # Panics
    ///
    /// When a pair names a row at or past `rows`, or there is no memory.
    #[track_caller]
    pub fn from_pairs_in<B: Allocator>(rows: usize, pairs: Vec<(usize, T), B>, alloc: A) -> Lists<T, A> {
        match Lists::try_from_pairs_in(rows, pairs, alloc) {
            Ok(lists) => lists,
            Err(failure) => fail(failure),
        }
    }

    /// [`Lists::from_pairs_in`], or the [`AllocFailure`] (the pairs dropped).
    ///
    /// # Panics
    ///
    /// When a pair names a row at or past `rows`.
    #[track_caller]
    pub fn try_from_pairs_in<B: Allocator>(rows: usize, pairs: Vec<(usize, T), B>, alloc: A) -> Result<Lists<T, A>, AllocFailure> {
        let total = pairs.len();
        end_of(total, 0)?;
        let mut ends: Vec<u32, A> = Vec::try_with_capacity_in(rows, alloc.clone())?;
        // Within the capacity just reserved: no allocation.
        ends.resize(rows, 0);
        // Count each row's items. Every row is checked before anything moves.
        for &(row, _) in pairs.iter() {
            if row >= rows {
                out_of_bounds("Lists::from_pairs_in (a pair's row)", row, rows);
            }
            ends[row] += 1;
        }
        // Each row's end, then (by the reverse scatter below) its start.
        let mut sum = 0u32;
        for end in ends.iter_mut() {
            sum += *end;
            *end = sum;
        }
        let mut items: Vec<T, A> = Vec::try_with_capacity_in(total, alloc)?;
        let base = items.as_mut_ptr();
        // Last pair first, each into the last free slot of its row, so a row keeps its pairs'
        // order and each end steps down to its row's start. Moves only: nothing here panics.
        for (row, item) in pairs.into_iter().rev() {
            ends[row] -= 1;
            // SAFETY: `ends[row]` is below the row's end and not below its start (the row had
            // that many pairs), so a slot of the `total` reserved, written once.
            unsafe { base.add(ends[row] as usize).write(item) };
        }
        // SAFETY: every one of the `total` slots was written once (the counts sum to `total`).
        unsafe { items.set_len(total) };
        // Starts to ends: row `r` ends where row `r + 1` starts; the last ends at `total`.
        if rows > 0 {
            ends.copy_within(1.., 0);
            ends[rows - 1] = sum;
        }
        Ok(Lists { items, ends })
    }
}

impl<T, A: Allocator> Lists<T, A> {
    /// The number of rows.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The number of items in all rows.
    pub fn items_len(&self) -> usize {
        self.items.len()
    }

    /// Every item, row after row.
    pub fn items(&self) -> &[T] {
        &self.items
    }

    fn start(&self, row: usize) -> usize {
        if row == 0 { 0 } else { self.ends[row - 1] as usize }
    }

    /// Row `row`'s items.
    ///
    /// # Panics
    ///
    /// When `row >= len()`.
    #[track_caller]
    pub fn row(&self, row: usize) -> &[T] {
        match self.get(row) {
            Some(items) => items,
            None => out_of_bounds("Lists::row", row, self.len()),
        }
    }

    /// Row `row`'s items, to change in place (a row's length is fixed once pushed).
    #[track_caller]
    pub fn row_mut(&mut self, row: usize) -> &mut [T] {
        if row >= self.len() {
            out_of_bounds("Lists::row_mut", row, self.len());
        }
        let (start, end) = (self.start(row), self.ends[row] as usize);
        &mut self.items[start..end]
    }

    pub fn get(&self, row: usize) -> Option<&[T]> {
        let end = *self.ends.get(row)? as usize;
        Some(&self.items[self.start(row)..end])
    }

    /// Pushes a row of `items`; its index. One extend and one push: going through
    /// [`Lists::open_row`] cost twice this loop (10.5 ns a row of four `u32`s against 5.6 for
    /// two vectors by hand, `alloc_cost causes`). A panic in `items` drops the items it gave and
    /// leaves the lists as they were.
    #[track_caller]
    #[inline]
    pub fn push_row(&mut self, items: impl IntoIterator<Item = T>) -> usize {
        /// Cuts the items back to the row's start unless the row was closed: a panic in the
        /// iterator leaves the lists as they were.
        struct Cut<'a, T, A: Allocator> {
            items: &'a mut Vec<T, A>,
            start: usize,
        }
        impl<T, A: Allocator> Drop for Cut<'_, T, A> {
            fn drop(&mut self) {
                self.items.truncate(self.start);
            }
        }
        let row = self.ends.len();
        self.ends.reserve(1);
        let start = self.items.len();
        let cut = Cut { items: &mut self.items, start };
        cut.items.extend(items);
        let len = cut.items.len();
        let end = match end_of(len, 0) {
            Ok(end) => end,
            // The cut drops the row's items as the panic unwinds.
            Err(failure) => fail(failure),
        };
        core::mem::forget(cut);
        // Room for one end was reserved above: this does not allocate.
        self.ends.push(end);
        row
    }

    /// Opens a row at the end, filled by the [`OpenRow`]'s pushes and closed when it drops:
    /// building a row with no buffer of its own.
    #[track_caller]
    pub fn open_row(&mut self) -> OpenRow<'_, T, A> {
        self.ends.reserve(1);
        OpenRow { lists: self }
    }

    /// [`Lists::open_row`], or the [`AllocFailure`] with the lists as they were.
    pub fn try_open_row(&mut self) -> Result<OpenRow<'_, T, A>, AllocFailure> {
        self.ends.try_reserve(1)?;
        Ok(OpenRow { lists: self })
    }

    /// Takes the last row's items off; `false` when there are no rows.
    pub fn pop_row(&mut self) -> bool {
        if self.ends.pop().is_none() {
            return false;
        }
        let start = self.ends.last().map_or(0, |end| *end as usize);
        self.items.truncate(start);
        true
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.ends.clear();
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter { items: &self.items, ends: &self.ends, start: 0 }
    }

    /// The bytes this holds from its allocator: what the cost example compares.
    pub fn heap_bytes(&self) -> usize {
        self.items.capacity() * size_of::<T>() + self.ends.capacity() * size_of::<u32>()
    }
}

/// A row being filled at the end of a [`Lists`]; closed (its end recorded) when it drops, so a
/// panic while filling leaves the items pushed so far as a whole row.
pub struct OpenRow<'a, T, A: Allocator> {
    lists: &'a mut Lists<T, A>,
}

impl<T, A: Allocator> OpenRow<'_, T, A> {
    /// The index the row will have.
    pub fn index(&self) -> usize {
        self.lists.ends.len()
    }

    /// The row's items so far.
    pub fn items(&self) -> &[T] {
        &self.lists.items[self.lists.start(self.index())..]
    }

    #[track_caller]
    pub fn push(&mut self, item: T) {
        if let Err(failure) = end_of(self.lists.items.len() + 1, 0) {
            fail(failure);
        }
        self.lists.items.push(item);
    }

    /// [`OpenRow::push`], or the item handed back with the [`AllocFailure`].
    pub fn try_push(&mut self, item: T) -> Result<(), crate::Refused<T>> {
        if let Err(why) = end_of(self.lists.items.len() + 1, 0) {
            return Err(crate::Refused { value: item, why });
        }
        self.lists.items.try_push(item)
    }

    #[track_caller]
    pub fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        let items = items.into_iter();
        if let Err(failure) = end_of(self.lists.items.len().saturating_add(items.size_hint().0), 0) {
            fail(failure);
        }
        self.lists.items.extend(items);
        if let Err(failure) = end_of(self.lists.items.len(), 0) {
            // The iterator gave more than it said: the row is emptied before the panic, so
            // the end the drop records fits.
            let start = self.lists.start(self.index());
            self.lists.items.truncate(start);
            fail(failure);
        }
    }
}

impl<T, A: Allocator> Drop for OpenRow<'_, T, A> {
    fn drop(&mut self) {
        // Every push checked the count fits a `u32`; room for the end was reserved on opening.
        let end = self.lists.items.len() as u32;
        if self.lists.ends.try_push(end).is_err() {
            unreachable!("Lists: the row end's room was reserved when the row opened");
        }
    }
}

/// The rows of a [`Lists`], in order.
pub struct Iter<'a, T> {
    items: &'a [T],
    ends: &'a [u32],
    start: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a [T];

    #[inline]
    fn next(&mut self) -> Option<&'a [T]> {
        let (&end, rest) = self.ends.split_first()?;
        self.ends = rest;
        // SAFETY: the ends are non-decreasing from `start` and none passes `items.len()` (the
        // invariant of `Lists::ends`), so the range is in bounds; no check per row.
        let row = unsafe { self.items.get_unchecked(self.start..end as usize) };
        self.start = end as usize;
        Some(row)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.ends.len(), Some(self.ends.len()))
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}

impl<T> FusedIterator for Iter<'_, T> {}

// SAFETY: `size_hint` is the ends left, one row each.
unsafe impl<T> core::iter::TrustedLen for Iter<'_, T> {}

impl<'a, T, A: Allocator> IntoIterator for &'a Lists<T, A> {
    type Item = &'a [T];
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Lists<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<Lists<T, B>> for Lists<T, A> {
    fn eq(&self, other: &Lists<T, B>) -> bool {
        self.ends.as_slice() == other.ends.as_slice() && self.items.as_slice() == other.items.as_slice()
    }
}
