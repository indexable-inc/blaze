//! [`VecDeque`]: a ring buffer over [`RawBuf`], pushed and popped at both ends in constant
//! amortized time.
//!
//! Costs: `insert` and `remove` move every value after the index one slot (std moves the shorter
//! side; this moves the back). `make_contiguous` rotates the whole buffer, `O(capacity)`. A
//! drain's gap closes by moving the shorter side. Not here yet: `range`, `split_off`.

use core::{
    alloc::Allocator,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    marker::PhantomData,
    mem::{DropGuard, MaybeUninit, size_of},
    ops::{Index, IndexMut},
    ptr::{self, NonNull},
    slice,
};

#[cfg(test)]
use crate::plant::{self, Plant};
use crate::{
    error::{AllocFailure, fail},
    make::CloneIn,
    raw::RawBuf,
    spec::CloneSlice,
    vec::{Refused, out_of_bounds},
};

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

    /// [`VecDeque::push_back`], or the value back with why there was no room; the deque is as
    /// it was.
    pub fn try_push_back(&mut self, value: T) -> Result<(), Refused<T>> {
        if self.len == self.buf.capacity()
            && let Err(why) = self.try_reserve(1)
        {
            return Err(Refused { value, why });
        }
        self.push_back(value);
        Ok(())
    }

    /// [`VecDeque::push_front`], or the value back with why there was no room; the deque is as
    /// it was.
    pub fn try_push_front(&mut self, value: T) -> Result<(), Refused<T>> {
        if self.len == self.buf.capacity()
            && let Err(why) = self.try_reserve(1)
        {
            return Err(Refused { value, why });
        }
        self.push_front(value);
        Ok(())
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

    /// The slots of the `n` values from index `from`, as the one or two runs they sit in.
    fn runs(&self, from: usize, n: usize) -> (*mut [T], *mut [T]) {
        let first = self.slot(from);
        let to_end = self.buf.capacity() - first;
        if n <= to_end {
            (ptr::slice_from_raw_parts_mut(self.at(first), n), ptr::slice_from_raw_parts_mut(self.buf.ptr(), 0))
        } else {
            (
                ptr::slice_from_raw_parts_mut(self.at(first), to_end),
                ptr::slice_from_raw_parts_mut(self.buf.ptr(), n - to_end),
            )
        }
    }

    /// Moves the value at index `from` to index `to`, a bitwise move.
    ///
    /// # Safety
    ///
    /// Index `from` holds a value and index `to` is empty (or is `from`).
    #[inline]
    unsafe fn move_value(&mut self, from: usize, to: usize) {
        if from != to {
            // SAFETY: as the caller promises; distinct slots of one value each.
            unsafe { ptr::copy_nonoverlapping(self.at(self.slot(from)), self.at(self.slot(to)), 1) };
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.retain_mut(|value| keep(value));
    }

    /// Keeps the values `keep` says yes to, in order. A panic in `keep` or in a value's drop
    /// keeps the values not yet seen; none is dropped twice.
    pub fn retain_mut(&mut self, mut keep: impl FnMut(&mut T) -> bool) {
        let len = self.len;
        // Nothing counts as in the ring while it is compacted; the guard sets the length.
        self.len = 0;
        let mut at = DropGuard::new(Compact { ring: self, read: 0, write: 0, len }, Compact::finish);
        while at.read < len {
            let slot = at.ring.slot(at.read);
            // SAFETY: index `read < len` holds a value not yet moved or dropped.
            let value = unsafe { &mut *at.ring.at(slot) };
            if !keep(&mut *value) {
                // Counted as gone before its drop, so a panic there does not drop it again.
                at.read += 1;
                // SAFETY: the value is no longer counted by `Compact` and is dropped once.
                unsafe { ptr::drop_in_place(value) };
                continue;
            }
            let (read, write) = (at.read, at.write);
            // SAFETY: `write <= read`; index `write` was emptied (moved down or dropped) or is
            // `read`.
            unsafe { at.ring.move_value(read, write) };
            at.read += 1;
            at.write += 1;
        }
    }

    /// Takes the values in `range` out, front to back. The values after it close the gap when
    /// the drain is dropped, by moving whichever side of the gap is shorter.
    ///
    /// # Panics
    ///
    /// When `range` is backwards or ends past the length.
    #[track_caller]
    pub fn drain<R: core::ops::RangeBounds<usize>>(&mut self, range: R) -> Drain<'_, T, A> {
        let len = self.len;
        let core::ops::Range { start, end } = crate::vec::range_of(&range, len, "VecDeque::drain");
        // Only the values before the range count while the drain lives.
        self.len = start;
        Drain { ring: NonNull::from(self), start, front: start, back: end, end, len, _ring: PhantomData }
    }

    /// Moves every value of `other` to the back of this ring; `other` is left empty with its
    /// capacity.
    pub fn append<B: Allocator>(&mut self, other: &mut VecDeque<T, B>) {
        self.reserve(other.len);
        let (front, back) = other.as_slices();
        let (front, front_len, back, back_len) = (front.as_ptr(), front.len(), back.as_ptr(), back.len());
        // SAFETY: each run of `other` holds values, which move bitwise into room reserved here,
        // in another buffer; `other` then counts none of them, so each is owned once.
        unsafe { self.move_to_back(front, front_len) };
        // SAFETY: as above, for the second run.
        unsafe { self.move_to_back(back, back_len) };
        other.len = 0;
        other.head = 0;
    }

    /// Moves `n` values from `src` to the back, bitwise, in at most two copies (the free room
    /// may wrap), and counts them.
    ///
    /// # Safety
    ///
    /// `src` holds `n` values, which the caller then treats as moved; the ring has room for `n`
    /// more; `src` is not in this ring's buffer.
    unsafe fn move_to_back(&mut self, src: *const T, n: usize) {
        let start = self.slot(self.len);
        let first = n.min(self.buf.capacity() - start);
        // SAFETY: the free room runs from slot `start` to the end of the buffer, then from slot
        // 0 up to `head`; `first` values fit the first part, the rest the second.
        unsafe { ptr::copy_nonoverlapping(src, self.at(start), first) };
        // SAFETY: as above; `n - first` is 0 when the room did not wrap.
        unsafe { ptr::copy_nonoverlapping(src.add(first), self.at(0), n - first) };
        self.len += n;
    }

    /// Puts `value` at `index` from the front, moving the `len - index` values after it back
    /// one: linear.
    #[track_caller]
    pub fn insert_shift(&mut self, index: usize, value: T) {
        if index > self.len {
            out_of_bounds("VecDeque::insert_shift", index, self.len);
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

    /// Takes the value at `index` out, moving the values after it forward one: linear.
    pub fn remove_shift(&mut self, index: usize) -> Option<T> {
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

/// The state of a [`VecDeque::retain_mut`]: indices below `write` are kept, `write..read` are
/// empty, `read..len` not yet seen.
struct Compact<'a, T, A: Allocator> {
    ring: &'a mut VecDeque<T, A>,
    read: usize,
    write: usize,
    len: usize,
}

impl<T, A: Allocator> Compact<'_, T, A> {
    /// Moves the values not yet seen down to the kept ones and sets the length: the end of a
    /// compaction, or its unwinding.
    fn finish(mut c: Compact<'_, T, A>) {
        while c.read < c.len {
            // SAFETY: `read` holds a value not yet seen; `write <= read` is empty or `read`.
            unsafe { c.ring.move_value(c.read, c.write) };
            c.read += 1;
            c.write += 1;
        }
        c.ring.len = c.write;
    }
}

/// The values of a range taken out of a ring ([`VecDeque::drain`]).
///
/// # Invariants
///
/// The ring's length is `start` and its head unchanged; indices `front..back` hold the values
/// not yet given out, `end..len` the values after the range.
pub struct Drain<'a, T, A: Allocator> {
    ring: NonNull<VecDeque<T, A>>,
    start: usize,
    front: usize,
    back: usize,
    end: usize,
    len: usize,
    _ring: PhantomData<&'a mut VecDeque<T, A>>,
}

// SAFETY: a drain is a `&mut VecDeque` and the values it moves out, as `vec::Drain`.
unsafe impl<T: Send, A: Allocator + Send> Send for Drain<'_, T, A> {}
// SAFETY: a shared drain gives out nothing.
unsafe impl<T: Sync, A: Allocator + Sync> Sync for Drain<'_, T, A> {}

impl<T, A: Allocator> Drain<'_, T, A> {
    fn ring(&self) -> &VecDeque<T, A> {
        // SAFETY: the drain holds the ring's `&mut` for its lifetime.
        unsafe { self.ring.as_ref() }
    }

    /// Closes the gap `start..end` by moving the shorter side, and counts the values again.
    fn close(&mut self) {
        let (start, end, len) = (self.start, self.end, self.len);
        // SAFETY: the drain holds the ring's `&mut` for its lifetime.
        let ring = unsafe { self.ring.as_mut() };
        let (gap, tail) = (end - start, len - end);
        if gap == 0 {
            ring.len = len;
            return;
        }
        if start < tail {
            // The front part moves back by the gap, last first (the destinations are later).
            for i in (0..start).rev() {
                // SAFETY: index `i` holds a value; `i + gap` is in the gap or was emptied by the
                // step before.
                unsafe { ring.move_value(i, i + gap) };
            }
            ring.head = ring.slot(gap);
        } else {
            // The tail moves forward by the gap, first first.
            for i in end..len {
                // SAFETY: index `i` holds a value; `i - gap` is in the gap or was emptied.
                unsafe { ring.move_value(i, i - gap) };
            }
        }
        ring.len = start + tail;
        if ring.len == 0 {
            ring.head = 0;
        }
    }
}

impl<T, A: Allocator> Iterator for Drain<'_, T, A> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        let ring = self.ring();
        // SAFETY: index `front < back` holds a value, read out once as `front` passes it.
        let value = unsafe { ptr::read(ring.at(ring.slot(self.front))) };
        self.front += 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back - self.front;
        (left, Some(left))
    }
}

impl<T, A: Allocator> DoubleEndedIterator for Drain<'_, T, A> {
    fn next_back(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        let ring = self.ring();
        // SAFETY: index `back` held the last value not given out, read out once.
        Some(unsafe { ptr::read(ring.at(ring.slot(self.back))) })
    }
}

impl<T, A: Allocator> ExactSizeIterator for Drain<'_, T, A> {}

// SAFETY: `size_hint` is `back - front`, exactly the values left to give.
unsafe impl<T, A: Allocator> core::iter::TrustedLen for Drain<'_, T, A> {}
impl<T, A: Allocator> FusedIterator for Drain<'_, T, A> {}

impl<T, A: Allocator> Drop for Drain<'_, T, A> {
    fn drop(&mut self) {
        let (first, second) = self.ring().runs(self.front, self.back - self.front);
        self.front = self.back;
        // The gap closes after the values left are dropped, a panic in one among the ways; the
        // second run is dropped after the first, the same way.
        let _close = DropGuard::new(self, |drain| drain.close());
        let _second = DropGuard::new(second, |run| {
            // SAFETY: the run's slots hold values not given out, which nothing counts now;
            // each dropped once.
            unsafe { ptr::drop_in_place(run) };
        });
        // SAFETY: as above.
        unsafe { ptr::drop_in_place(first) };
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

/// The old names, whose cost the name did not say; each forwards to the one that says it.
impl<T, A: Allocator> VecDeque<T, A> {
    #[deprecated(note = "linear: use `insert_shift`, which says so")]
    #[track_caller]
    #[inline]
    pub fn insert(&mut self, index: usize, value: T) {
        self.insert_shift(index, value);
    }

    #[deprecated(note = "linear: use `remove_shift`, which says so")]
    #[inline]
    pub fn remove(&mut self, index: usize) -> Option<T> {
        self.remove_shift(index)
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for VecDeque<T, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    /// Front first, unwrapped; one block copy per run for `TrivialClone` values.
    fn clone(&self) -> VecDeque<T, A> {
        let mut out = VecDeque::with_capacity_in(self.len, self.allocator().clone());
        let (front, back) = self.as_slices();
        let base = out.buf.ptr();
        // SAFETY: `out` is new, its head 0, with room for `len` values from slot 0, apart from
        // `self`'s buffer; its length counts each clone as it lands.
        unsafe { <T as CloneSlice>::clone_to(front, base, &mut out.len) };
        // SAFETY: as above, after the front run.
        unsafe { <T as CloneSlice>::clone_to(back, base, &mut out.len) };
        out
    }
}

impl<T, A: Allocator> VecDeque<T, A> {
    /// A deep copy in another allocator, front first, unwrapped: room for exactly the length in
    /// `alloc`, each value copied with [`CloneIn`] into `alloc` too. A refusal drops what was
    /// copied.
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<VecDeque<T::In, B>, AllocFailure>
    where
        T: CloneIn<B>,
    {
        let mut out = VecDeque::try_with_capacity_in(self.len, alloc.clone())?;
        let (front, back) = self.as_slices();
        let base = out.buf.ptr();
        // SAFETY: `out` is new, its head 0, with room for `len` values from slot 0, apart from
        // `self`'s buffer; its length counts each copy as it lands, so a refusal drops those.
        unsafe { T::try_clone_slice_in(front, base, &mut out.len, &alloc)? };
        // SAFETY: as above, after the front run.
        unsafe { T::try_clone_slice_in(back, base, &mut out.len, &alloc)? };
        Ok(out)
    }

    /// [`VecDeque::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> VecDeque<T::In, B>
    where
        T: CloneIn<B>,
    {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
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
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for &value in iter {
            self.push_back(value);
        }
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

// SAFETY: `size_hint` is the lengths of the two slice runs left, exactly the values left.
unsafe impl<T> core::iter::TrustedLen for Iter<'_, T> {}
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

// SAFETY: as `Iter`'s.
unsafe impl<T> core::iter::TrustedLen for IterMut<'_, T> {}
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

// SAFETY: `size_hint` is the ring's length, exactly the values its pops give.
unsafe impl<T, A: Allocator> core::iter::TrustedLen for IntoIter<T, A> {}
impl<T, A: Allocator> FusedIterator for IntoIter<T, A> {}
