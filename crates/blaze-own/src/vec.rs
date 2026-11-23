//! [`Vec`]: a growable array in an allocator the caller names, over [`RawBuf`].
//!
//! Sorting is the slice's (`sort_unstable`, `sort_unstable_by`, `sort_unstable_by_key`, through
//! `Deref<Target = [T]>`): `core` has no stable sort, which needs a buffer; one in the vector's
//! allocator is left to write.

use core::{
    alloc::Allocator,
    any::type_name,
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    iter::{FusedIterator, TrustedLen},
    marker::PhantomData,
    mem::{DropGuard, ManuallyDrop, MaybeUninit},
    ops::{Bound, Deref, DerefMut, Index, IndexMut, Range, RangeBounds},
    ptr::{self, NonNull},
    slice::{self, SliceIndex},
};

#[cfg(test)]
use crate::plant::{self, Plant};
use crate::{
    boxed::Box,
    error::{AllocFailure, fail},
    make::CloneIn,
    raw::RawBuf,
    spec::{CloneSlice, Find},
};

/// A growable array of `T` in `A`: one block; index and push O(1), a push past capacity copies every value once into a block twice the size.
///
/// # Invariants
///
/// The first `len` slots of `buf` hold values; `len <= buf.capacity()`.
pub struct Vec<T, A: Allocator> {
    buf: RawBuf<T, A>,
    len: usize,
}

/// A value [`Vec::try_push`] could not push, given back with why.
#[derive(Debug)]
pub struct Refused<T> {
    pub value: T,
    pub why: AllocFailure,
}

impl<T, A: Allocator> Vec<T, A> {
    /// An empty vector in `alloc`; nothing is allocated until it grows.
    #[inline]
    pub const fn new_in(alloc: A) -> Vec<T, A> {
        Vec { buf: RawBuf::new_in(alloc), len: 0 }
    }

    /// An empty vector with room for at least `capacity` values.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Vec<T, A> {
        Vec { buf: RawBuf::with_capacity_in(capacity, alloc), len: 0 }
    }

    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<Vec<T, A>, AllocFailure> {
        Ok(Vec { buf: RawBuf::try_with_capacity_in(capacity, alloc)?, len: 0 })
    }

    /// The values `iter` gives, in `alloc`: `collect` with the allocator named.
    pub fn from_iter_in<I: IntoIterator<Item = T>>(iter: I, alloc: A) -> Vec<T, A> {
        let mut out = Vec::new_in(alloc);
        out.extend(iter);
        out
    }

    /// The values of `array`, moved into one allocation in `alloc` ([`vec_in!`]).
    pub fn from_array_in<const N: usize>(array: [T; N], alloc: A) -> Vec<T, A> {
        let mut out = Vec::with_capacity_in(N, alloc);
        let array = ManuallyDrop::new(array);
        // SAFETY: `out` has room for `N` values; the array's values move bitwise, and
        // `ManuallyDrop` keeps the array from dropping them again.
        unsafe { ptr::copy_nonoverlapping(array.as_ptr(), out.as_mut_ptr(), N) };
        out.len = N;
        out
    }

    /// `n` clones of `value` ([`vec_in!`]`[alloc; value; n]`).
    pub fn from_elem_in(value: T, n: usize, alloc: A) -> Vec<T, A>
    where
        T: Clone,
    {
        let mut out = Vec::with_capacity_in(n, alloc);
        out.resize(n, value);
        out
    }

    /// Clones of `values` in `alloc`: `to_vec` with the allocator named.
    pub fn from_slice_in(values: &[T], alloc: A) -> Vec<T, A>
    where
        T: Clone,
    {
        let mut out = Vec::with_capacity_in(values.len(), alloc);
        out.extend_cloned(values);
        out
    }

    /// Takes over a buffer from its parts.
    ///
    /// # Safety
    ///
    /// As [`RawBuf::from_raw_parts_in`] for `ptr`, `capacity` and `alloc`, and the first
    /// `length <= capacity` slots hold values that the vector now owns.
    #[inline]
    pub unsafe fn from_raw_parts_in(
        ptr: NonNull<T>,
        length: usize,
        capacity: usize,
        alloc: A,
    ) -> Vec<T, A> {
        // SAFETY: the caller's contract is `RawBuf::from_raw_parts_in`'s.
        let buf = unsafe { RawBuf::from_raw_parts_in(ptr, capacity, alloc) };
        Vec { buf, len: length }
    }

    /// Its pointer, length, capacity and allocator; nothing is dropped or freed.
    pub fn into_raw_parts_with_alloc(self) -> (NonNull<T>, usize, usize, A) {
        let me = ManuallyDrop::new(self);
        // SAFETY: `me` is never used or dropped again, so the buffer moves out once.
        let buf = unsafe { ptr::read(&me.buf) };
        let (ptr, cap, alloc) = buf.into_raw_parts();
        (ptr, me.len, cap, alloc)
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The values it holds without growing; `usize::MAX` for a `T` sized 0.
    #[inline]
    pub const fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    #[inline]
    pub const fn allocator(&self) -> &A {
        self.buf.allocator()
    }

    #[inline]
    pub const fn as_ptr(&self) -> *const T {
        self.buf.ptr()
    }

    #[inline]
    pub const fn as_mut_ptr(&mut self) -> *mut T {
        self.buf.ptr()
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` slots hold values (the invariant); the pointer is aligned and
        // non-null even when nothing is allocated.
        unsafe { slice::from_raw_parts(self.buf.ptr(), self.len) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, and `&mut self` is the only access.
        unsafe { slice::from_raw_parts_mut(self.buf.ptr(), self.len) }
    }

    /// Sets the length.
    ///
    /// # Safety
    ///
    /// `new_len <= capacity()`, and the first `new_len` slots hold values.
    #[inline]
    pub unsafe fn set_len(&mut self, new_len: usize) {
        self.len = new_len;
    }

    /// The slots past the length, to be written before [`Vec::set_len`] takes them in.
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<T>] {
        let spare = self.buf.capacity() - self.len;
        // SAFETY: slots `len..capacity` are in the buffer; `MaybeUninit` needs no value there.
        unsafe { slice::from_raw_parts_mut(self.buf.ptr().add(self.len).cast(), spare) }
    }
}

impl<A: Allocator> Vec<u8, A> {
    /// Open a zero-pre-zeroing [`crate::ByteCursor`] appending into this vector's spare capacity.
    #[inline]
    pub fn byte_cursor(&mut self) -> crate::ByteCursor<'_, A> {
        crate::ByteCursor::new(self)
    }
}

impl<T, A: Allocator> Vec<T, A> {

    /// Room for at least `additional` more values, growing by at least twice its capacity.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.buf.reserve(self.len, additional);
    }

    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.buf.try_reserve(self.len, additional)
    }

    /// Room for exactly `additional` more values (the allocator may give more).
    pub fn reserve_exact(&mut self, additional: usize) {
        if let Err(failure) = self.buf.try_reserve_exact(self.len, additional) {
            fail(failure);
        }
    }

    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.buf.try_reserve_exact(self.len, additional)
    }

    /// Gives back the room past its length.
    pub fn shrink_to_fit(&mut self) {
        self.buf.shrink_to(self.len);
    }

    pub fn try_shrink_to_fit(&mut self) -> Result<(), AllocFailure> {
        self.buf.try_shrink_to(self.len)
    }

    /// Gives back the room past `min_capacity` or its length, whichever is more.
    pub fn shrink_to(&mut self, min_capacity: usize) {
        self.buf.shrink_to(min_capacity.max(self.len));
    }

    #[inline]
    pub fn push(&mut self, value: T) {
        if self.len == self.buf.capacity() {
            self.buf.reserve(self.len, 1);
        }
        // SAFETY: slot `len` is in the buffer and holds no value.
        unsafe { ptr::write(self.buf.ptr().add(self.len), value) };
        self.len += 1;
    }

    /// Pushes `value`, or gives it back with the [`AllocFailure`] when there is no room.
    pub fn try_push(&mut self, value: T) -> Result<(), Refused<T>> {
        if let Err(why) = self.buf.try_reserve(self.len, 1) {
            return Err(Refused { value, why });
        }
        // SAFETY: as `push`.
        unsafe { ptr::write(self.buf.ptr().add(self.len), value) };
        self.len += 1;
        Ok(())
    }

    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: slot `len` held the last value, and the length no longer counts it.
        Some(unsafe { ptr::read(self.buf.ptr().add(self.len)) })
    }

    /// Puts `value` at `index`, moving the `len - index` values after it up one: linear, so
    /// quadratic in a loop (build in order and sort, or push and `remove_swap`).
    ///
    /// # Panics
    ///
    /// When `index > len()`, or there is no memory.
    #[track_caller]
    pub fn insert_shift(&mut self, index: usize, value: T) {
        let len = self.len;
        if index > len {
            out_of_bounds("Vec::insert_shift", index, len);
        }
        if len == self.buf.capacity() {
            self.buf.reserve(len, 1);
        }
        // SAFETY: `index <= len < capacity`; slots `index..len` hold values and move up one
        // into the room `len` has, then slot `index` is free for `value`.
        unsafe {
            let at = self.buf.ptr().add(index);
            ptr::copy(at, at.add(1), len - index);
            ptr::write(at, value);
        }
        self.len = len + 1;
    }

    /// Takes the value at `index` out, moving the `len - index - 1` values after it down one:
    /// linear, so quadratic in a loop (`remove_swap` when order does not matter, `retain` for
    /// many).
    ///
    /// # Panics
    ///
    /// When `index >= len()`.
    #[track_caller]
    pub fn remove_shift(&mut self, index: usize) -> T {
        let len = self.len;
        if index >= len {
            out_of_bounds("Vec::remove_shift", index, len);
        }
        // SAFETY: `index < len`: the value is read out once, then the values after it move down
        // over its slot, and the length drops by one.
        unsafe {
            let at = self.buf.ptr().add(index);
            let value = ptr::read(at);
            ptr::copy(at.add(1), at, len - index - 1);
            self.len = len - 1;
            value
        }
    }

    /// Takes the value at `index` out and puts the last value in its place: constant time, the
    /// order changed.
    ///
    /// # Panics
    ///
    /// When `index >= len()`.
    #[track_caller]
    pub fn remove_swap(&mut self, index: usize) -> T {
        let len = self.len;
        if index >= len {
            out_of_bounds("Vec::remove_swap", index, len);
        }
        // SAFETY: `index < len`: the value is read out once, the last value moves into its slot
        // (the same slot when it is the last, which `copy` allows), and the length drops by one.
        unsafe {
            let base = self.buf.ptr();
            let value = ptr::read(base.add(index));
            ptr::copy(base.add(len - 1), base.add(index), 1);
            self.len = len - 1;
            value
        }
    }

    /// Drops the values past `len`; nothing when it is no longer.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let cut = self.len - len;
        // The length is set first: a panic in a value's drop leaves the rest to the slice's drop,
        // which goes on through them, and none is dropped twice.
        self.len = len;
        #[cfg(test)]
        if plant::on(Plant::TruncateForgetsDrop) {
            return;
        }
        // SAFETY: slots `len..len + cut` held values, which the length no longer counts.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.buf.ptr().add(len), cut)) };
    }

    /// Drops every value; keeps the capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Appends clones of `values`.
    pub fn extend_from_slice(&mut self, values: &[T])
    where
        T: Clone,
    {
        self.reserve(values.len());
        self.extend_cloned(values);
    }

    pub fn try_extend_from_slice(&mut self, values: &[T]) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        self.try_reserve(values.len())?;
        self.extend_cloned(values);
        Ok(())
    }

    /// Appends clones of `values` into room already reserved, counting each as it lands, so a
    /// clone that panics leaves the vector holding the ones before it; one block copy for `Copy`
    /// values ([`CloneSlice`]).
    fn extend_cloned(&mut self, values: &[T])
    where
        T: Clone,
    {
        debug_assert!(self.buf.capacity() - self.len >= values.len());
        let base = self.buf.ptr();
        // SAFETY: room was reserved for every value past the length, in the vector's buffer,
        // which `values` (a shared borrow) is not.
        unsafe { <T as CloneSlice>::clone_to(values, base, &mut self.len) };
    }

    /// Puts clones of `values` at `index`, moving the `len - index` values after it up by their
    /// count (linear in the tail); one block copy for `TrivialClone` values. A clone that panics
    /// leaves the ones made before it, and the values after `index` closed up behind them.
    ///
    /// # Panics
    ///
    /// When `index > len()`, or there is no memory.
    #[track_caller]
    pub fn insert_slice_shift(&mut self, index: usize, values: &[T])
    where
        T: Clone,
    {
        if index > self.len {
            out_of_bounds("Vec::insert_slice_shift", index, self.len);
        }
        self.reserve(values.len());
        // SAFETY: `index <= len` and room for `values` was reserved.
        unsafe { self.insert_cloned(index, values) };
    }

    /// [`Vec::insert_slice_shift`], or the [`AllocFailure`] with the vector as it was.
    ///
    /// # Panics
    ///
    /// When `index > len()`.
    #[track_caller]
    pub fn try_insert_slice_shift(&mut self, index: usize, values: &[T]) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        if index > self.len {
            out_of_bounds("Vec::try_insert_many", index, self.len);
        }
        self.try_reserve(values.len())?;
        // SAFETY: as in `insert_many`.
        unsafe { self.insert_cloned(index, values) };
        Ok(())
    }

    /// # Safety
    ///
    /// `index <= len` and the buffer has room for `values.len()` more.
    unsafe fn insert_cloned(&mut self, index: usize, values: &[T])
    where
        T: Clone,
    {
        let (len, n) = (self.len, values.len());
        let base = self.buf.ptr();
        // SAFETY: slots `index..len` hold values and move up `n` into reserved room; `copy`
        // allows the overlap.
        unsafe { ptr::copy(base.add(index), base.add(index + n), len - index) };
        // While the gap fills only the values before it count; the guard closes what is left of
        // the gap (all of it on finishing) and counts the moved values again.
        self.len = index;
        let mut gap = DropGuard::new((&mut self.len, 0usize), move |(vec_len, made): (&mut usize, usize)| {
            if made < n {
                // SAFETY: the moved values are at `index + n..`, and `index + made..` is empty.
                unsafe { ptr::copy(base.add(index + n), base.add(index + made), len - index) };
            }
            *vec_len = len + made;
        });
        // SAFETY: the `n` slots from `index` are empty (moved from), in the buffer, and not in
        // `values`, a shared borrow apart from the vector.
        unsafe { <T as CloneSlice>::clone_to(values, base.add(index), &mut gap.1) };
    }

    /// The index of the first value equal to `needle`, by a linear scan: blaze-memchr's vector
    /// search for `u8`, `i8` and `bool`, `==` per value otherwise ([`Find`]). For repeated
    /// lookups keep the values sorted (`SortedSet`) or in a map.
    #[inline]
    pub fn scan_for(&self, needle: &T) -> Option<usize>
    where
        T: PartialEq,
    {
        <T as Find>::find(self.as_slice(), needle)
    }

    /// Makes its length `new_len`: clones of `value` added, or values past it dropped.
    pub fn resize(&mut self, new_len: usize, value: T)
    where
        T: Clone,
    {
        if new_len <= self.len {
            self.truncate(new_len);
            return;
        }
        let more = new_len - self.len;
        self.reserve(more);
        for _ in 1..more {
            let clone = value.clone();
            // SAFETY: room was reserved for `more` values, and slot `len` holds none.
            unsafe { ptr::write(self.buf.ptr().add(self.len), clone) };
            self.len += 1;
        }
        // SAFETY: as above, the last of the reserved slots.
        unsafe { ptr::write(self.buf.ptr().add(self.len), value) };
        self.len += 1;
    }

    /// Makes its length `new_len`: values from `make` added, or values past it dropped.
    pub fn resize_with(&mut self, new_len: usize, mut make: impl FnMut() -> T) {
        if new_len <= self.len {
            self.truncate(new_len);
            return;
        }
        self.reserve(new_len - self.len);
        while self.len < new_len {
            let value = make();
            // SAFETY: room was reserved up to `new_len`, and slot `len` holds none.
            unsafe { ptr::write(self.buf.ptr().add(self.len), value) };
            self.len += 1;
        }
    }

    /// Moves every value of `other` to its end, leaving `other` empty.
    pub fn append<B: Allocator>(&mut self, other: &mut Vec<T, B>) {
        let count = other.len;
        self.reserve(count);
        // SAFETY: room for `count` values was reserved; `other`'s values move bitwise and its
        // length then counts none of them, so each is owned once.
        unsafe { ptr::copy_nonoverlapping(other.buf.ptr(), self.buf.ptr().add(self.len), count) };
        other.len = 0;
        self.len += count;
    }

    /// Moves the values at `at..` into a new vector in a clone of its allocator.
    ///
    /// # Panics
    ///
    /// When `at > len()`, or there is no memory.
    #[track_caller]
    pub fn split_off(&mut self, at: usize) -> Vec<T, A>
    where
        A: Clone,
    {
        let len = self.len;
        if at > len {
            out_of_bounds("Vec::split_off", at, len);
        }
        let count = len - at;
        let mut other = Vec::with_capacity_in(count, self.allocator().clone());
        // SAFETY: `other` has room for `count`; the values move bitwise and `self`'s length then
        // stops counting them.
        unsafe { ptr::copy_nonoverlapping(self.buf.ptr().add(at), other.buf.ptr(), count) };
        self.len = at;
        other.len = count;
        other
    }

    /// Takes the values in `range` out as an iterator; the values after it move down when the
    /// iterator is dropped. A leaked iterator leaks the values after `range` too, never drops
    /// one twice.
    ///
    /// # Panics
    ///
    /// When `range` is backwards or ends past the length.
    #[track_caller]
    pub fn drain<R: RangeBounds<usize>>(&mut self, range: R) -> Drain<'_, T, A> {
        let len = self.len;
        let Range { start, end } = range_of(&range, len, "Vec::drain");
        self.len = start;
        Drain {
            vec: NonNull::from(self),
            front: start,
            back: end,
            tail_start: end,
            tail_len: len - end,
            _vec: PhantomData,
        }
    }

    /// Keeps the values `keep` says yes to, in order.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.retain_mut(|value| keep(value));
    }

    /// Keeps the values `keep` says yes to, in order. A panic in `keep` or in a value's drop
    /// keeps the values not yet seen; none is dropped twice.
    pub fn retain_mut(&mut self, mut keep: impl FnMut(&mut T) -> bool) {
        let len = self.len;
        if len == 0 {
            return;
        }
        let base = self.buf.ptr();
        // Nothing counts as in the vector while it is compacted; the guard sets the length.
        self.len = 0;
        let mut at = DropGuard::new(Compact { vec: self, read: 0, write: 0, len }, Compact::finish);
        while at.read < len {
            // SAFETY: slot `read < len` holds a value not yet moved or dropped.
            let value = unsafe { &mut *base.add(at.read) };
            if !keep(&mut *value) {
                // Counted as gone before its drop, so a panic there does not drop it again.
                at.read += 1;
                #[cfg(test)]
                if plant::on(Plant::RetainForgetsDrop) {
                    continue;
                }
                // SAFETY: the value is no longer counted by `Compact` and is dropped once.
                unsafe { ptr::drop_in_place(value) };
                continue;
            }
            if at.read != at.write {
                // SAFETY: `write < read`; slot `write` was emptied (its value moved down or
                // dropped) and slot `read` holds a value.
                unsafe { ptr::copy_nonoverlapping(base.add(at.read), base.add(at.write), 1) };
            }
            at.read += 1;
            at.write += 1;
        }
    }

    /// Drops each value that `same` says belongs with the last one kept before it: `same(value,
    /// kept)`. Panic safety as [`Vec::retain_mut`].
    pub fn dedup_by(&mut self, same: impl FnMut(&mut T, &mut T) -> bool) {
        self.dedup_range_by(.., same);
    }

    /// [`Vec::dedup_by`] inside `range` only: the values after it move down to close the gaps.
    /// With a sort of the same range through the slice it is a sorted set in a vector's tail:
    /// `v[from..].sort_unstable(); v.dedup_range(from..)`.
    ///
    /// # Panics
    ///
    /// When `range` is backwards or ends past the length.
    #[track_caller]
    pub fn dedup_range_by<R: RangeBounds<usize>>(&mut self, range: R, mut same: impl FnMut(&mut T, &mut T) -> bool) {
        let Range { start, end } = range_of(&range, self.len, "Vec::dedup_range_by");
        if end - start <= 1 {
            return;
        }
        let len = self.len;
        let base = self.buf.ptr();
        self.len = 0;
        // The first value of the range is kept; values before it are kept untouched, and the
        // guard moves the values from `read` on (the rest of the range, then the tail) down.
        let first = start + 1;
        let mut at = DropGuard::new(Compact { vec: self, read: first, write: first, len }, Compact::finish);
        while at.read < end {
            // SAFETY: slot `read < len` holds a value not yet seen, and slot `write - 1 < read`
            // the last one kept: two distinct values.
            let (value, kept) = unsafe { (&mut *base.add(at.read), &mut *base.add(at.write - 1)) };
            if same(&mut *value, kept) {
                at.read += 1;
                // SAFETY: no longer counted, dropped once.
                unsafe { ptr::drop_in_place(value) };
                continue;
            }
            if at.read != at.write {
                // SAFETY: as in `retain_mut`.
                unsafe { ptr::copy_nonoverlapping(base.add(at.read), base.add(at.write), 1) };
            }
            at.read += 1;
            at.write += 1;
        }
    }

    /// Drops consecutive values with equal keys but the first.
    pub fn dedup_by_key<K: PartialEq>(&mut self, mut key: impl FnMut(&mut T) -> K) {
        self.dedup_by(|value, kept| key(value) == key(kept));
    }

    /// Drops consecutive equal values but the first.
    pub fn dedup(&mut self)
    where
        T: PartialEq,
    {
        self.dedup_by(|value, kept| *value == *kept);
    }

    /// Drops consecutive equal values but the first, inside `range` only.
    #[track_caller]
    pub fn dedup_range<R: RangeBounds<usize>>(&mut self, range: R)
    where
        T: PartialEq,
    {
        self.dedup_range_by(range, |value, kept| *value == *kept);
    }

    /// The values as a `Box<[T], A>`, its room shrunk to its length.
    pub fn into_boxed_slice(mut self) -> Box<[T], A> {
        self.shrink_to_fit();
        let (ptr, len, _cap, alloc) = self.into_raw_parts_with_alloc();
        let slice = NonNull::slice_from_raw_parts(ptr, len);
        // SAFETY: after the shrink the block holds exactly `len` values of `T` (the capacity is
        // the length), which is the layout of `[T]` of that length; for a length of 0 or a `T`
        // sized 0 nothing is allocated, and the box frees nothing of size 0.
        unsafe { Box::from_non_null_in(slice, alloc) }
    }

    /// Gives up the values for as long as the allocator lives; nothing is freed.
    /// Freezes this bump-allocated vector into a mutable slice tied to the arena's lifetime `'a`.
    ///
    /// Only compiles for bulk-reclaimed [`BumpAlloc`](crate::BumpAlloc) arenas (`&Frame`,
    /// `ArenaLocal`, `&Inline`, `Bump<'a>`) — never `Global` or `Heap` — so memory is reclaimed
    /// in O(1) when the arena drops. As with bump arenas, elements' `Drop` impls are not run.
    #[inline]
    pub fn into_bump_slice<'a>(self) -> &'a mut [T]
    where
        A: crate::BumpAlloc + 'a,
    {
        let mut me = ManuallyDrop::new(self);
        let len = me.len;
        // SAFETY: `A: BumpAlloc + 'a` guarantees the buffer lives for `'a` and is reclaimed in
        // bulk when the arena drops; `me` is not dropped and its first `len` slots hold values.
        unsafe { slice::from_raw_parts_mut(me.as_mut_ptr(), len) }
    }

    /// Permanently leaks this `'static`-backed vector for the remainder of the process.
    ///
    /// Only valid for intentional process-lifetime OS hooks (such as POSIX `environ` or a
    /// global unwind registry). For scoped arenas, use [`Vec::into_bump_slice`] instead.
    pub fn leak_static(self) -> &'static mut [T]
    where
        A: 'static,
    {
        let mut me = ManuallyDrop::new(self);
        let len = me.len;
        // SAFETY: the vector is forgotten so the buffer lives for `'static`; its first `len`
        // slots are initialized.
        unsafe { slice::from_raw_parts_mut(me.as_mut_ptr(), len) }
    }
}

/// A vector being compacted in place (`retain_mut`, `dedup_by`): slots `..write` hold the
/// values kept, `write..read` are empty, `read..len` hold values not yet seen.
struct Compact<'v, T, A: Allocator> {
    vec: &'v mut Vec<T, A>,
    read: usize,
    write: usize,
    len: usize,
}

impl<T, A: Allocator> Compact<'_, T, A> {
    /// Moves the values not yet seen down to the ones kept and sets the length: the end of a
    /// compaction, by finishing or by a panic.
    fn finish(self) {
        let unseen = self.len - self.read;
        let base = self.vec.buf.ptr();
        if unseen > 0 && self.read != self.write {
            // SAFETY: `read..len` hold values and `write < read` starts the empty slots; the
            // ranges may overlap, which `copy` allows.
            unsafe { ptr::copy(base.add(self.read), base.add(self.write), unseen) };
        }
        self.vec.len = self.write + unseen;
    }
}

#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn out_of_bounds(what: &'static str, index: usize, len: usize) -> ! {
    panic!("blaze_own::{what}: index {index} is out of bounds of the length {len}")
}

/// `range` as a start and an end within `len`.
#[track_caller]
pub(crate) fn range_of(range: &impl RangeBounds<usize>, len: usize, what: &'static str) -> Range<usize> {
    let start = match range.start_bound() {
        Bound::Included(&start) => start,
        Bound::Excluded(&start) => match start.checked_add(1) {
            Some(start) => start,
            None => panic!("blaze_own::{what}: the range starts after usize::MAX"),
        },
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&end) => match end.checked_add(1) {
            Some(end) => end,
            None => panic!("blaze_own::{what}: the range ends at usize::MAX inclusive"),
        },
        Bound::Excluded(&end) => end,
        Bound::Unbounded => len,
    };
    if start > end {
        panic!("blaze_own::{what}: the range starts at {start}, after its end {end}");
    }
    if end > len {
        panic!("blaze_own::{what}: the range ends at {end}, past the length {len}");
    }
    start..end
}

/// `collect` with the allocator named: `iter.collect_in(alloc)`.
pub trait CollectIn: Iterator + Sized {
    fn collect_in<A: Allocator>(self, alloc: A) -> Vec<Self::Item, A> {
        Vec::from_iter_in(self, alloc)
    }
}

impl<I: Iterator> CollectIn for I {}

// SAFETY (`may_dangle`): the drop drops the `T`s it owns (the buffer's `PhantomData<T>` tells
// drop check so) and reads nothing else of `T`.
unsafe impl<#[may_dangle] T, A: Allocator> Drop for Vec<T, A> {
    fn drop(&mut self) {
        // SAFETY: the first `len` slots hold values (the invariant), each dropped once; the
        // buffer frees its block after, a panic in a drop among the ways.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.buf.ptr(), self.len)) };
    }
}

impl<T, A: Allocator> Deref for Vec<T, A> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, A: Allocator> DerefMut for Vec<T, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: SliceIndex<[T]>, A: Allocator> Index<I> for Vec<T, A> {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &I::Output {
        Index::index(self.as_slice(), index)
    }
}

impl<T, I: SliceIndex<[T]>, A: Allocator> IndexMut<I> for Vec<T, A> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut I::Output {
        IndexMut::index_mut(self.as_mut_slice(), index)
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for Vec<T, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> Vec<T, A> {
        Vec::from_slice_in(self, self.allocator().clone())
    }
}

impl<T, A: Allocator> Vec<T, A> {
    /// A deep copy in another allocator: room for exactly the length in `alloc`, and each value
    /// copied with [`CloneIn`] into `alloc` too, one block copy for `Copy` values. A refusal
    /// drops what was copied.
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<Vec<T::In, B>, AllocFailure>
    where
        T: CloneIn<B>,
    {
        clone_slice_in(self.as_slice(), alloc)
    }

    /// [`Vec::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> Vec<T::In, B>
    where
        T: CloneIn<B>,
    {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

/// The old names, whose cost the name did not say: each forwards to the name that says it, and
/// goes when its callers have moved (docs/v2/plan/design/std.md, "What std makes easy to get
/// slow").
impl<T, A: Allocator> Vec<T, A> {
    #[deprecated(note = "linear: use `insert_shift`, which says so")]
    #[track_caller]
    #[inline]
    pub fn insert(&mut self, index: usize, value: T) {
        self.insert_shift(index, value);
    }

    #[deprecated(note = "linear: use `remove_shift`, or `remove_swap` when order does not matter")]
    #[track_caller]
    #[inline]
    pub fn remove(&mut self, index: usize) -> T {
        self.remove_shift(index)
    }

    #[deprecated(note = "renamed `remove_swap`, beside `remove_shift`")]
    #[track_caller]
    #[inline]
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.remove_swap(index)
    }

    #[deprecated(note = "linear in the tail: use `insert_slice_shift`")]
    #[track_caller]
    #[inline]
    pub fn insert_many(&mut self, index: usize, values: &[T])
    where
        T: Clone,
    {
        self.insert_slice_shift(index, values);
    }

    #[deprecated(note = "linear in the tail: use `try_insert_slice_shift`")]
    #[track_caller]
    #[inline]
    pub fn try_insert_many(&mut self, index: usize, values: &[T]) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        self.try_insert_slice_shift(index, values)
    }

    #[deprecated(note = "a linear scan: use `scan_for(needle).is_some()`")]
    #[inline]
    pub fn contains(&self, needle: &T) -> bool
    where
        T: PartialEq,
    {
        self.scan_for(needle).is_some()
    }

    #[deprecated(note = "a linear scan: use `scan_for`")]
    #[inline]
    pub fn position(&self, needle: &T) -> Option<usize>
    where
        T: PartialEq,
    {
        self.scan_for(needle)
    }
}

/// Copies of `values` in a vector of exactly their length in `alloc`, through
/// [`CloneIn::try_clone_slice_in`] (one block copy for `Copy` values); a refusal drops what was
/// copied.
pub(crate) fn clone_slice_in<T: CloneIn<B>, B: Allocator + Clone>(values: &[T], alloc: B) -> Result<Vec<T::In, B>, AllocFailure> {
    let mut out = Vec::try_with_capacity_in(values.len(), alloc.clone())?;
    let base = out.buf.ptr();
    // SAFETY: `out` has room for `values.len()` values and holds none; it is a new block, apart
    // from `values`. Its length counts each copy as it lands, so a refusal drops those.
    unsafe { T::try_clone_slice_in(values, base, &mut out.len, &alloc)? };
    Ok(out)
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Vec<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: Hash, A: Allocator> Hash for Vec<T, A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        Hash::hash(self.as_slice(), state);
    }
}

impl<T: PartialEq<U>, U, A: Allocator, B: Allocator> PartialEq<Vec<U, B>> for Vec<T, A> {
    #[inline]
    fn eq(&self, other: &Vec<U, B>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq<U>, U, A: Allocator> PartialEq<[U]> for Vec<T, A> {
    #[inline]
    fn eq(&self, other: &[U]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq<U>, U, A: Allocator> PartialEq<&[U]> for Vec<T, A> {
    #[inline]
    fn eq(&self, other: &&[U]) -> bool {
        self.as_slice() == *other
    }
}

impl<T: PartialEq<U>, U, A: Allocator, const N: usize> PartialEq<[U; N]> for Vec<T, A> {
    #[inline]
    fn eq(&self, other: &[U; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, A: Allocator> Eq for Vec<T, A> {}

impl<T: PartialOrd, A: Allocator> PartialOrd for Vec<T, A> {
    #[inline]
    fn partial_cmp(&self, other: &Vec<T, A>) -> Option<Ordering> {
        PartialOrd::partial_cmp(self.as_slice(), other.as_slice())
    }
}

impl<T: Ord, A: Allocator> Ord for Vec<T, A> {
    #[inline]
    fn cmp(&self, other: &Vec<T, A>) -> Ordering {
        Ord::cmp(self.as_slice(), other.as_slice())
    }
}

impl<T, A: Allocator> AsRef<[T]> for Vec<T, A> {
    fn as_ref(&self) -> &[T] {
        self
    }
}

impl<T, A: Allocator> AsMut<[T]> for Vec<T, A> {
    fn as_mut(&mut self) -> &mut [T] {
        self
    }
}

impl<T, A: Allocator> Borrow<[T]> for Vec<T, A> {
    fn borrow(&self) -> &[T] {
        self
    }
}

impl<T, A: Allocator> BorrowMut<[T]> for Vec<T, A> {
    fn borrow_mut(&mut self) -> &mut [T] {
        self
    }
}

impl<T, A: Allocator> From<Box<[T], A>> for Vec<T, A> {
    fn from(values: Box<[T], A>) -> Vec<T, A> {
        Box::into_vec(values)
    }
}

impl<T, A: Allocator> Extend<T> for Vec<T, A> {
    /// Reserves once and writes with no capacity check per value for a `TrustedLen` iterator,
    /// and moves another vector's values in one copy ([`SpecExtend`]).
    #[inline]
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        <Vec<T, A> as SpecExtend<T, I::IntoIter>>::spec_extend(self, iter.into_iter());
    }
}

impl<'a, T: Copy + 'a, A: Allocator> Extend<&'a T> for Vec<T, A> {
    /// One block copy from a slice's `iter()` ([`SpecExtendRef`]).
    #[inline]
    fn extend<I: IntoIterator<Item = &'a T>>(&mut self, iter: I) {
        <Vec<T, A> as SpecExtendRef<'a, T, I::IntoIter>>::spec_extend_ref(self, iter.into_iter());
    }
}

/// `extend` by the iterator's kind: any (a check per value), `TrustedLen` (one reservation, no
/// check), or another vector's values (one copy). Each specializing impl is in
/// `min_specialization`'s subset (`spec`'s notes): the added bound is `TrustedLen`, a
/// specialization trait, or none; `IntoIter<T, B>` names `T` inside a type, not as a second
/// bare parameter.
trait SpecExtend<T, I> {
    fn spec_extend(&mut self, iter: I);
}

impl<T, I: Iterator<Item = T>, A: Allocator> SpecExtend<T, I> for Vec<T, A> {
    default fn spec_extend(&mut self, mut iter: I) {
        while let Some(value) = iter.next() {
            if self.len == self.buf.capacity() {
                let (lower, _) = iter.size_hint();
                self.buf.reserve(self.len, lower.saturating_add(1));
            }
            // SAFETY: slot `len` is in the buffer and holds no value; the length counts it at
            // once, so a panic in the iterator leaves every value pushed owned.
            unsafe { ptr::write(self.buf.ptr().add(self.len), value) };
            self.len += 1;
        }
    }
}

impl<T, I: Iterator<Item = T> + TrustedLen, A: Allocator> SpecExtend<T, I> for Vec<T, A> {
    default fn spec_extend(&mut self, iter: I) {
        let (lower, upper) = iter.size_hint();
        // `TrustedLen`: no upper bound means more than `usize::MAX` values.
        let Some(additional) = upper else {
            fail(AllocFailure::CapacityOverflow { what: type_name::<T>(), len: self.len, additional: lower })
        };
        self.reserve(additional);
        let base = self.buf.ptr();
        let start = self.len;
        // The length is set as the guard drops, a panic in the iterator among the ways, so every
        // value written is owned.
        let mut at = DropGuard::new((&mut self.len, start), |(len, written): (&mut usize, usize)| *len = written);
        iter.for_each(|value| {
            // SAFETY: `TrustedLen` gives exactly `additional` values, and room for that many was
            // reserved past `start`; slot `at.1` is one of them and empty.
            unsafe { base.add(at.1).write(value) };
            at.1 += 1;
        });
    }
}

impl<T, B: Allocator, A: Allocator> SpecExtend<T, IntoIter<T, B>> for Vec<T, A> {
    fn spec_extend(&mut self, mut iter: IntoIter<T, B>) {
        let left = iter.back - iter.front;
        self.reserve(left);
        // SAFETY: the iterator's slots `front..back` hold its values, which move bitwise into
        // reserved room in another buffer; `front` then passes them, so the iterator's drop
        // drops none of them and frees its block.
        unsafe { ptr::copy_nonoverlapping(iter.buf.ptr().add(iter.front), self.buf.ptr().add(self.len), left) };
        iter.front = iter.back;
        self.len += left;
    }
}

/// `extend` from references to `Copy` values: one block copy from a slice's `iter()`. Its
/// specializing impl adds no bound to the base's.
trait SpecExtendRef<'a, T: 'a, I> {
    fn spec_extend_ref(&mut self, iter: I);
}

impl<'a, T: Copy + 'a, I: Iterator<Item = &'a T>, A: Allocator> SpecExtendRef<'a, T, I> for Vec<T, A> {
    default fn spec_extend_ref(&mut self, iter: I) {
        <Vec<T, A> as SpecExtend<T, core::iter::Copied<I>>>::spec_extend(self, iter.copied());
    }
}

impl<'a, T: Copy + 'a, A: Allocator> SpecExtendRef<'a, T, slice::Iter<'a, T>> for Vec<T, A> {
    fn spec_extend_ref(&mut self, iter: slice::Iter<'a, T>) {
        let values = iter.as_slice();
        self.reserve(values.len());
        // SAFETY: room for `values` was reserved past the length, in this vector's buffer,
        // which a shared slice cannot be; `Copy` values duplicate bitwise.
        unsafe { ptr::copy_nonoverlapping(values.as_ptr(), self.buf.ptr().add(self.len), values.len()) };
        self.len += values.len();
    }
}

impl<T, A: Allocator> IntoIterator for Vec<T, A> {
    type Item = T;
    type IntoIter = IntoIter<T, A>;

    fn into_iter(self) -> IntoIter<T, A> {
        let (ptr, len, cap, alloc) = self.into_raw_parts_with_alloc();
        // SAFETY: the parts of a vector's buffer, given back whole.
        let buf = unsafe { RawBuf::from_raw_parts_in(ptr, cap, alloc) };
        IntoIter { buf, front: 0, back: len }
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a Vec<T, A> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a mut Vec<T, A> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.iter_mut()
    }
}

/// A vector's values by value ([`Vec::into_iter`]).
///
/// # Invariants
///
/// Slots `front..back` of `buf` hold the values not yet given out.
pub struct IntoIter<T, A: Allocator> {
    buf: RawBuf<T, A>,
    front: usize,
    back: usize,
}

impl<T, A: Allocator> IntoIter<T, A> {
    /// The values not yet given out.
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: slots `front..back` hold values (the invariant).
        unsafe { slice::from_raw_parts(self.buf.ptr().add(self.front), self.back - self.front) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, with `&mut self` the only access.
        unsafe {
            slice::from_raw_parts_mut(self.buf.ptr().add(self.front), self.back - self.front)
        }
    }

    pub fn allocator(&self) -> &A {
        self.buf.allocator()
    }
}

impl<T, A: Allocator> Iterator for IntoIter<T, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        // SAFETY: slot `front < back` holds a value, read out once as `front` passes it.
        let value = unsafe { ptr::read(self.buf.ptr().add(self.front)) };
        self.front += 1;
        Some(value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back - self.front;
        (left, Some(left))
    }
}

impl<T, A: Allocator> DoubleEndedIterator for IntoIter<T, A> {
    #[inline]
    fn next_back(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        // SAFETY: slot `back` held the last value not given out, read out once.
        Some(unsafe { ptr::read(self.buf.ptr().add(self.back)) })
    }
}

impl<T, A: Allocator> ExactSizeIterator for IntoIter<T, A> {}

// SAFETY: `size_hint` is `back - front`, exactly the values `next` and `next_back` give.
unsafe impl<T, A: Allocator> TrustedLen for IntoIter<T, A> {}

impl<T, A: Allocator> FusedIterator for IntoIter<T, A> {}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for IntoIter<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IntoIter").field(&self.as_slice()).finish()
    }
}

// SAFETY (`may_dangle`): drops the `T`s not given out and reads nothing else of `T`.
unsafe impl<#[may_dangle] T, A: Allocator> Drop for IntoIter<T, A> {
    fn drop(&mut self) {
        let left = self.back - self.front;
        // SAFETY: slots `front..back` hold values (the invariant), each dropped once; the buffer
        // frees its block after.
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.buf.ptr().add(self.front), left));
        }
    }
}

/// The values of a range taken out of a vector ([`Vec::drain`]).
///
/// # Invariants
///
/// The vector's length is the range's start; slots `front..back` hold the values not yet given
/// out, `tail_start..tail_start + tail_len` the values after the range, which the drop moves
/// down to the length.
pub struct Drain<'a, T, A: Allocator> {
    vec: NonNull<Vec<T, A>>,
    front: usize,
    back: usize,
    tail_start: usize,
    tail_len: usize,
    _vec: PhantomData<&'a mut Vec<T, A>>,
}

// SAFETY: a drain is a `&mut Vec` and the values it moves out: as std's, `Send` when the
// vector's values and allocator are.
unsafe impl<T: Send, A: Allocator + Send> Send for Drain<'_, T, A> {}
// SAFETY: a shared drain gives out only `&[T]` (`as_slice`).
unsafe impl<T: Sync, A: Allocator + Sync> Sync for Drain<'_, T, A> {}

impl<T, A: Allocator> Drain<'_, T, A> {
    fn base(&self) -> *mut T {
        // SAFETY: the drain holds the vector's `&mut` for its lifetime.
        unsafe { self.vec.as_ref() }.buf.ptr()
    }

    /// The values not yet given out.
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: slots `front..back` hold values (the invariant).
        unsafe { slice::from_raw_parts(self.base().add(self.front), self.back - self.front) }
    }

    /// Moves the values after the range down to the length and counts them again.
    fn close(&mut self) {
        // SAFETY: the drain holds the vector's `&mut` for its lifetime.
        let vec = unsafe { self.vec.as_mut() };
        let start = vec.len;
        if self.tail_len > 0 && self.tail_start != start {
            let base = vec.buf.ptr();
            // SAFETY: the tail's slots hold values and `start < tail_start` begins empty slots;
            // the ranges may overlap, which `copy` allows.
            unsafe { ptr::copy(base.add(self.tail_start), base.add(start), self.tail_len) };
        }
        vec.len = start + self.tail_len;
    }
}

impl<T, A: Allocator> Iterator for Drain<'_, T, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        // SAFETY: slot `front < back` holds a value, read out once as `front` passes it.
        let value = unsafe { ptr::read(self.base().add(self.front)) };
        self.front += 1;
        Some(value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back - self.front;
        (left, Some(left))
    }
}

impl<T, A: Allocator> DoubleEndedIterator for Drain<'_, T, A> {
    #[inline]
    fn next_back(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        // SAFETY: slot `back` held the last value not given out, read out once.
        Some(unsafe { ptr::read(self.base().add(self.back)) })
    }
}

impl<T, A: Allocator> ExactSizeIterator for Drain<'_, T, A> {}

// SAFETY: `size_hint` is `back - front`, exactly the values `next` and `next_back` give.
unsafe impl<T, A: Allocator> TrustedLen for Drain<'_, T, A> {}

impl<T, A: Allocator> FusedIterator for Drain<'_, T, A> {}

impl<T, A: Allocator> Drop for Drain<'_, T, A> {
    fn drop(&mut self) {
        let first = self.front;
        let left = self.back - self.front;
        let base = self.base();
        self.front = self.back;
        // The tail moves back after the values left are dropped, a panic in one among the ways.
        let _close = DropGuard::new(self, |drain: &mut Drain<'_, T, A>| drain.close());
        // SAFETY: slots `first..first + left` hold values not given out, now counted by nothing
        // else (`front` moved past them), each dropped once.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base.add(first), left)) };
    }
}
