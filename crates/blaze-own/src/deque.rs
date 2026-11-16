//! [`VecDeque`]: a ring buffer over [`RawBuf`], pushed and popped at both ends in constant
//! amortized time.
//!
//! Costs: `insert` and `remove` move every value after the index one slot (std moves the shorter
//! side; this moves the back). `make_contiguous` rotates the whole buffer, `O(capacity)`. Not
//! here yet: `drain`, `retain`, `range`, `split_off`, `append`.

use core::{
    alloc::Allocator,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    mem::{DropGuard, MaybeUninit, size_of},
    ops::{Index, IndexMut},
    ptr, slice,
};

#[cfg(test)]
use crate::plant::{self, Plant};
use crate::{error::AllocFailure, raw::RawBuf, vec::out_of_bounds};

/// A ring of `T` in `A`.
///
/// # Invariants
///
/// `head < capacity` (or 0 when nothing is allocated); the `len` values are in the slots
/// `head, head + 1, ..` taken modulo the capacity; `len <= capacity`.
pub struct VecDeque<T, A: Allocator> {
    buf: RawBuf<T, A>,
    head: usize,
    len: usize,
}

impl<T, A: Allocator> VecDeque<T, A> {
    #[inline]
    pub const fn new_in(alloc: A) -> VecDeque<T, A> {
        VecDeque { buf: RawBuf::new_in(alloc), head: 0, len: 0 }
    }

    pub fn with_capacity_in(capacity: usize, alloc: A) -> VecDeque<T, A> {
        VecDeque { buf: RawBuf::with_capacity_in(capacity, alloc), head: 0, len: 0 }
    }

    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<VecDeque<T, A>, AllocFailure> {
        Ok(VecDeque { buf: RawBuf::try_with_capacity_in(capacity, alloc)?, head: 0, len: 0 })
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    pub const fn allocator(&self) -> &A {
        self.buf.allocator()
    }

    /// The slot of the value at `index` from the front; never overflows, the capacity of a
    /// `T` sized 0 being `usize::MAX`.
    #[inline]
    fn slot(&self, index: usize) -> usize {
        let to_end = self.buf.capacity() - self.head;
        if index >= to_end { index - to_end } else { self.head + index }
    }

    /// The slot before `head`, around the ring.
    #[inline]
    fn slot_before_head(&self) -> usize {
        if self.head == 0 { self.buf.capacity() - 1 } else { self.head - 1 }
    }

    #[inline]
    fn at(&self, slot: usize) -> *mut T {
        // SAFETY: a slot below the capacity is inside the buffer.
        unsafe { self.buf.ptr().add(slot) }
    }

    pub fn reserve(&mut self, additional: usize) {
        if self.buf.needs_to_grow(self.len, additional) {
            let old = self.buf.capacity();
            self.buf.reserve(self.len, additional);
            self.unwrap_after_grow(old);
        }
    }

    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        if self.buf.needs_to_grow(self.len, additional) {
            let old = self.buf.capacity();
            self.buf.try_reserve(self.len, additional)?;
            self.unwrap_after_grow(old);
        }
        Ok(())
    }

    /// After the buffer grew from `old` slots: a ring that wrapped around the old end has its
    /// wrapped part (or its head part, whichever is shorter and fits) moved, so the slots are
    /// in order modulo the new capacity.
    fn unwrap_after_grow(&mut self, old: usize) {
        let new = self.buf.capacity();
        if self.head <= old - self.len {
            return; // Did not wrap.
        }
        #[cfg(test)]
        if plant::on(Plant::DequeGrowKeepsWrap) {
            return;
        }
        let head_len = old - self.head;
        let tail_len = self.len - head_len;
        if tail_len < head_len && tail_len <= new - old {
            // SAFETY: slots `0..tail_len` hold the wrapped values; `old..old + tail_len` are new
            // and empty (`tail_len <= new - old`).
            unsafe { ptr::copy_nonoverlapping(self.at(0), self.at(old), tail_len) };
        } else {
            let new_head = new - head_len;
            // SAFETY: slots `head..old` hold the head part; they move to the end of the new
            // buffer, `new_head >= head`, past the wrapped part (`tail_len <= head`); they may
            // overlap, which `copy` allows.
            unsafe { ptr::copy(self.at(self.head), self.at(new_head), head_len) };
            self.head = new_head;
        }
    }

    pub fn push_back(&mut self, value: T) {
        if self.len == self.buf.capacity() {
            self.reserve(1);
        }
        let slot = self.slot(self.len);
        // SAFETY: `len < capacity`, so the slot after the back is in the buffer and empty.
        unsafe { ptr::write(self.at(slot), value) };
        self.len += 1;
    }

    pub fn push_front(&mut self, value: T) {
        if self.len == self.buf.capacity() {
            self.reserve(1);
        }
        self.head = self.slot_before_head();
        // SAFETY: `len < capacity`, so the slot before the old head is empty.
        unsafe { ptr::write(self.at(self.head), value) };
        self.len += 1;
    }

    pub fn pop_back(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let slot = self.slot(self.len);
        // SAFETY: the slot held the back value, which the length no longer counts.
        Some(unsafe { ptr::read(self.at(slot)) })
    }

    pub fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let slot = self.head;
        self.head = self.slot(1);
        self.len -= 1;
        // SAFETY: the slot held the front value, which the ring no longer counts.
        Some(unsafe { ptr::read(self.at(slot)) })
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        // SAFETY: `index < len`, so its slot holds a value.
        Some(unsafe { &*self.at(self.slot(index)) })
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.len {
            return None;
        }
        // SAFETY: as `get`, with `&mut self` the only access.
        Some(unsafe { &mut *self.at(self.slot(index)) })
    }

    pub fn front(&self) -> Option<&T> {
        self.get(0)
    }

    pub fn back(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|last| self.get(last))
    }

    pub fn front_mut(&mut self) -> Option<&mut T> {
        self.get_mut(0)
    }

    pub fn back_mut(&mut self) -> Option<&mut T> {
        let last = self.len.checked_sub(1)?;
        self.get_mut(last)
    }

    /// The values as two slices, front then back; the second is empty when the ring does not
    /// wrap.
    pub fn as_slices(&self) -> (&[T], &[T]) {
        let (front, back) = self.split();
        // SAFETY: `split` gives the two runs of slots that hold the `len` values.
        unsafe {
            (
                slice::from_raw_parts(self.at(self.head), front),
                slice::from_raw_parts(self.buf.ptr(), back),
            )
        }
    }

    pub fn as_mut_slices(&mut self) -> (&mut [T], &mut [T]) {
        let (front, back) = self.split();
        // SAFETY: as `as_slices`; the runs do not overlap, and `&mut self` is the only access.
        unsafe {
            (
                slice::from_raw_parts_mut(self.at(self.head), front),
                slice::from_raw_parts_mut(self.buf.ptr(), back),
            )
        }
    }

    /// How many values run from `head` to the end of the buffer, and how many wrap to its start.
    fn split(&self) -> (usize, usize) {
        let to_end = self.buf.capacity() - self.head;
        if self.len <= to_end { (self.len, 0) } else { (to_end, self.len - to_end) }
    }

    /// Moves the values so they are one slice from slot 0 and returns it; `O(capacity)`.
    pub fn make_contiguous(&mut self) -> &mut [T] {
        if size_of::<T>() != 0 && self.head != 0 {
            let cap = self.buf.capacity();
            // SAFETY: the whole buffer as `MaybeUninit<T>`, which needs no value in any slot;
            // rotating moves the slot at `head` to 0 and keeps the ring's order.
            let all = unsafe { slice::from_raw_parts_mut(self.buf.ptr().cast::<MaybeUninit<T>>(), cap) };
            all.rotate_left(self.head);
            self.head = 0;
        }
        // SAFETY: with `head` 0 (or `T` sized 0, where slots are not places) the values are the
        // first `len` slots.
        unsafe { slice::from_raw_parts_mut(self.at(self.head), self.len) }
    }

    /// Drops the values past `len`; nothing when it is no longer.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let (front, back) = self.split();
        // The values from `len` on, as the runs they sit in.
        let (cut_front, cut_back) = if len < front {
            (
                ptr::slice_from_raw_parts_mut(self.at(self.slot(len)), front - len),
                ptr::slice_from_raw_parts_mut(self.buf.ptr(), back),
            )
        } else {
            (
                ptr::slice_from_raw_parts_mut(self.at(len - front), back - (len - front)),
                ptr::slice_from_raw_parts_mut(self.buf.ptr(), 0),
            )
        };
        // The length goes first: a panicking drop leaves the ring holding only values not cut.
        self.len = len;
        // The second run is dropped after the first, a panic in it among the ways.
        let _second = DropGuard::new(cut_back, |run| {
            // SAFETY: the run's slots held values the ring no longer counts, each dropped once.
            unsafe { ptr::drop_in_place(run) };
        });
        // SAFETY: as above.
        unsafe { ptr::drop_in_place(cut_front) };
    }

    pub fn clear(&mut self) {
        self.truncate(0);
        self.head = 0;
    }

    /// Puts `value` at `index` from the front, moving the values after it back one.
    #[track_caller]
    pub fn insert(&mut self, index: usize, value: T) {
        if index > self.len {
            out_of_bounds("VecDeque::insert", index, self.len);
        }
        if self.len == self.buf.capacity() {
            self.reserve(1);
        }
        let mut at = self.len;
        while at > index {
            // SAFETY: slot of `at - 1` holds a value; slot of `at` is empty (the back's next, or
            // emptied by the step before).
            unsafe { ptr::copy_nonoverlapping(self.at(self.slot(at - 1)), self.at(self.slot(at)), 1) };
            at -= 1;
        }
        // SAFETY: the slot of `index` was emptied by the moves (or is the back's next).
        unsafe { ptr::write(self.at(self.slot(index)), value) };
        self.len += 1;
    }

    /// Takes the value at `index` out, moving the values after it forward one.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }
        // SAFETY: `index < len`: its slot holds a value, read out once.
        let value = unsafe { ptr::read(self.at(self.slot(index))) };
        for at in index..self.len - 1 {
            // SAFETY: slot of `at + 1` holds a value; slot of `at` was emptied by the step before.
            unsafe { ptr::copy_nonoverlapping(self.at(self.slot(at + 1)), self.at(self.slot(at)), 1) };
        }
        self.len -= 1;
        Some(value)
    }

    pub fn iter(&self) -> Iter<'_, T> {
        let (front, back) = self.as_slices();
        Iter { front: front.iter(), back: back.iter() }
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        let (front, back) = self.as_mut_slices();
        IterMut { front: front.iter_mut(), back: back.iter_mut() }
    }
}

// SAFETY (`may_dangle`): the drop drops the `T`s it owns and reads nothing else of them.
unsafe impl<#[may_dangle] T, A: Allocator> Drop for VecDeque<T, A> {
    fn drop(&mut self) {
        self.truncate(0);
    }
}

impl<T, A: Allocator> Index<usize> for VecDeque<T, A> {
    type Output = T;

    #[track_caller]
    fn index(&self, index: usize) -> &T {
        match self.get(index) {
            Some(value) => value,
            None => out_of_bounds("VecDeque::index", index, self.len),
        }
    }
}

impl<T, A: Allocator> IndexMut<usize> for VecDeque<T, A> {
    #[track_caller]
    fn index_mut(&mut self, index: usize) -> &mut T {
        let len = self.len;
        match self.get_mut(index) {
            Some(value) => value,
            None => out_of_bounds("VecDeque::index_mut", index, len),
        }
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for VecDeque<T, A> {
    fn clone(&self) -> VecDeque<T, A> {
        let mut out = VecDeque::with_capacity_in(self.len, self.allocator().clone());
        out.extend(self.iter().cloned());
        out
    }
}

impl<T, A: Allocator> Extend<T> for VecDeque<T, A> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for value in iter {
            self.push_back(value);
        }
    }
}

impl<'a, T: Copy + 'a, A: Allocator> Extend<&'a T> for VecDeque<T, A> {
    fn extend<I: IntoIterator<Item = &'a T>>(&mut self, iter: I) {
        self.extend(iter.into_iter().copied());
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for VecDeque<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<VecDeque<T, B>> for VecDeque<T, A> {
    fn eq(&self, other: &VecDeque<T, B>) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Eq, A: Allocator> Eq for VecDeque<T, A> {}

impl<T: PartialOrd, A: Allocator> PartialOrd for VecDeque<T, A> {
    fn partial_cmp(&self, other: &VecDeque<T, A>) -> Option<Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

impl<T: Ord, A: Allocator> Ord for VecDeque<T, A> {
    fn cmp(&self, other: &VecDeque<T, A>) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

impl<T: Hash, A: Allocator> Hash for VecDeque<T, A> {
    /// The length, then each value front to back, however the ring wraps: rings with equal
    /// values hash equally.
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.len);
        self.iter().for_each(|value| value.hash(state));
    }
}

impl<T, A: Allocator> IntoIterator for VecDeque<T, A> {
    type Item = T;
    type IntoIter = IntoIter<T, A>;

    fn into_iter(self) -> IntoIter<T, A> {
        IntoIter { ring: self }
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a VecDeque<T, A> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a mut VecDeque<T, A> {
    type Item = &'a mut T;
    type IntoIter = IterMut<'a, T>;

    fn into_iter(self) -> IterMut<'a, T> {
        self.iter_mut()
    }
}

/// A ring's values by reference, front to back.
#[derive(Clone)]
pub struct Iter<'a, T> {
    front: slice::Iter<'a, T>,
    back: slice::Iter<'a, T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        self.front.next().or_else(|| self.back.next())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.front.len() + self.back.len();
        (left, Some(left))
    }
}

impl<T> DoubleEndedIterator for Iter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.back.next_back().or_else(|| self.front.next_back())
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}
impl<T> FusedIterator for Iter<'_, T> {}

/// A ring's values by mutable reference, front to back.
pub struct IterMut<'a, T> {
    front: slice::IterMut<'a, T>,
    back: slice::IterMut<'a, T>,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<&'a mut T> {
        match self.front.next() {
            Some(value) => Some(value),
            None => self.back.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.front.len() + self.back.len();
        (left, Some(left))
    }
}

impl<T> DoubleEndedIterator for IterMut<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self.back.next_back() {
            Some(value) => Some(value),
            None => self.front.next_back(),
        }
    }
}

impl<T> ExactSizeIterator for IterMut<'_, T> {}
impl<T> FusedIterator for IterMut<'_, T> {}

/// A ring's values by value, front to back.
pub struct IntoIter<T, A: Allocator> {
    ring: VecDeque<T, A>,
}

impl<T, A: Allocator> Iterator for IntoIter<T, A> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.ring.pop_front()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.ring.len, Some(self.ring.len))
    }
}

impl<T, A: Allocator> DoubleEndedIterator for IntoIter<T, A> {
    fn next_back(&mut self) -> Option<T> {
        self.ring.pop_back()
    }
}

impl<T, A: Allocator> ExactSizeIterator for IntoIter<T, A> {}
impl<T, A: Allocator> FusedIterator for IntoIter<T, A> {}
