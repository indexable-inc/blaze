//! [`InlineVec<T, N, A>`]: up to `N` values inline, then spilled to the allocator: the honest
//! replacement for `smallvec`, which the compiler's lists use everywhere (most hold under four
//! values).
//!
//! # Layout and floor
//!
//! One word, `capacity`, says which state the vector is in, as smallvec's union form does, with
//! no tag byte: at most `N`, the values are inline and the word is their count; above `N`, they
//! are in a heap block of that capacity, and the count is the second word of the heap pair. So
//! `size_of::<InlineVec<T, N, A>>()` is `8 + max(N * size_of::<T>(), 16) + size_of::<A>()`
//! (rounded to `T`'s alignment), the least a spilling vector with an allocator can be. Per
//! value: `size_of::<T>()`. A push, a read or a pop touches the header's line and the value's,
//! one line when they share it (inline, `N * size_of::<T>()` up to 56 bytes); spilled, a read
//! is one extra dependent load (the heap pointer), as `Vec`'s.
//!
//! It never moves back inline on its own: once spilled it stays spilled until
//! [`InlineVec::try_shrink_to_fit`] is called, so a vector that shrinks and grows again does not
//! copy back and forth.
//!
//! Zero-sized `T` is refused at compile time: a list of them is a count.
//!
//! # Measured (`examples/inline_vec.rs`, release, one Mac run, 2026-11-15)
//!
//! Before: `InlineVec<u32, 4>` against `Vec<u32>`, ns per vector made, filled, summed and
//! dropped: 3 values 12.3 against 10.0, 8 values 31.6 against 36.4, 64 values 309 against 169.
//! Cause read from the code (not profiled): each push branched on the state three times
//! (`len`, `capacity`, `parts_mut`) with the growth path inlined into the loop, so the spilled
//! loop was not `Vec`'s; `drop` was an out-of-line call even inline with nothing to drop.
//! Fix: `try_push` reads the state once and keeps growth cold and out of line,
//! `try_extend_from_slice` reads it once per slice, `drop` returns at once inline for a `T`
//! with no drop. After (2026-11-15, one run, this machine): 3 values 2.51 against `Vec`'s
//! 10.88, 8 values 15.92 against 32.82, 64 values 105.76 against 145.42: ahead at every size.

use core::{
    alloc::{Allocator, Layout},
    any::type_name,
    fmt,
    hash::{Hash, Hasher},
    mem::{ManuallyDrop, MaybeUninit, size_of},
    ops::{Deref, DerefMut, Range},
    ptr::{self, NonNull},
    slice,
};

use blaze_own::{AllocFailure, Refused};

use crate::{
    error::{fail, overflow, refused},
    plant::{self, Plant},
};

/// The storage: `N` inline slots, or a heap block's pointer and the count of values in it.
///
/// # Invariant (the owner's)
///
/// When the owner's `capacity <= N`, `inline` is the field in use and its first `capacity`
/// slots hold values. When `capacity > N`, `heap` is in use: `heap.0` is a block `alloc` gave
/// for `Layout::array::<T>(capacity)`, its first `heap.1` slots hold values, `heap.1 <=
/// capacity`.
union Data<T, const N: usize> {
    inline: ManuallyDrop<[MaybeUninit<T>; N]>,
    heap: (NonNull<T>, usize),
}

/// Up to `N` values of `T` inline, then in a block of `A` (see the module).
pub struct InlineVec<T, const N: usize, A: Allocator> {
    /// The count, inline; the heap block's capacity, spilled (see [`Data`]).
    capacity: usize,
    data: Data<T, N>,
    alloc: A,
}

// SAFETY: the vector owns its values and its block uniquely, as a `Vec` does: sending it sends
// the values (`T: Send`) and the allocator that frees the block (`A: Send`).
unsafe impl<T: Send, const N: usize, A: Allocator + Send> Send for InlineVec<T, N, A> {}
// SAFETY: a shared `&InlineVec` gives out only `&T` (`T: Sync`) and `&A` (`A: Sync`).
unsafe impl<T: Sync, const N: usize, A: Allocator + Sync> Sync for InlineVec<T, N, A> {}

/// Frees a heap block when dropped, so a value's panicking drop still frees it.
struct FreeBlock<'a, A: Allocator> {
    alloc: &'a A,
    block: NonNull<u8>,
    layout: Layout,
}

impl<A: Allocator> Drop for FreeBlock<'_, A> {
    fn drop(&mut self) {
        // SAFETY: made only in `InlineVec::drop`, for the vector's live block and the layout it
        // was allocated with; the vector is being dropped, so nothing frees it again.
        unsafe { self.alloc.deallocate(self.block, self.layout) }
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn out_of_bounds(what: &'static str, index: usize, len: usize) -> ! {
    panic!("blaze_kit: index {index} is out of bounds of a `{what}` of {len} values")
}

impl<T, const N: usize, A: Allocator> InlineVec<T, N, A> {
    /// An empty vector, inline; allocates nothing until it holds more than `N`.
    #[inline]
    pub const fn new_in(alloc: A) -> InlineVec<T, N, A> {
        const { assert!(size_of::<T>() != 0, "an InlineVec of a zero-sized type: keep a count instead") };
        InlineVec {
            capacity: 0,
            data: Data { inline: ManuallyDrop::new([const { MaybeUninit::uninit() }; N]) },
            alloc,
        }
    }

    /// An empty vector with room for `capacity` values: inline when that is at most `N`.
    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<InlineVec<T, N, A>, AllocFailure> {
        let mut vec = InlineVec::new_in(alloc);
        if capacity > N {
            vec.try_grow_to(capacity)?;
        }
        Ok(vec)
    }

    /// [`InlineVec::try_with_capacity_in`], panicking with the refusal.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> InlineVec<T, N, A> {
        match InlineVec::try_with_capacity_in(capacity, alloc) {
            Ok(vec) => vec,
            Err(failure) => fail(failure),
        }
    }

    /// A vector of clones of `values`.
    pub fn try_from_slice_in(values: &[T], alloc: A) -> Result<InlineVec<T, N, A>, AllocFailure>
    where
        T: Clone,
    {
        let mut vec = InlineVec::try_with_capacity_in(values.len(), alloc)?;
        vec.try_extend_from_slice(values)?;
        Ok(vec)
    }

    /// Whether the values are in a heap block.
    #[inline(always)]
    pub const fn spilled(&self) -> bool {
        self.capacity > N
    }

    #[inline]
    pub fn len(&self) -> usize {
        if self.spilled() {
            // SAFETY: spilled, so `heap` is the field in use (`Data`'s invariant).
            unsafe { self.data.heap.1 }
        } else {
            self.capacity
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The values it holds without growing: `N` inline, the block's capacity spilled.
    #[inline]
    pub fn capacity(&self) -> usize {
        if self.spilled() { self.capacity } else { N }
    }

    pub fn allocator(&self) -> &A {
        &self.alloc
    }

    /// The first slot.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        if self.spilled() {
            // SAFETY: spilled, so `heap` is the field in use.
            unsafe { self.data.heap.0.as_ptr() }
        } else {
            // SAFETY: inline, so `inline` is the field in use.
            unsafe { self.data.inline.as_ptr().cast::<T>() }
        }
    }

    /// The first slot, writable.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.parts_mut().0
    }

    /// The first slot and the count of values, writable.
    #[inline]
    fn parts_mut(&mut self) -> (*mut T, &mut usize) {
        if self.spilled() {
            // SAFETY: spilled, so `heap` is the field in use.
            let heap = unsafe { &mut self.data.heap };
            (heap.0.as_ptr(), &mut heap.1)
        } else {
            // SAFETY: inline, so `inline` is the field in use.
            let first = unsafe { (*self.data.inline).as_mut_ptr().cast::<T>() };
            (first, &mut self.capacity)
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` slots hold values (`Data`'s invariant); the pointer is
        // aligned and non-null in both states.
        unsafe { slice::from_raw_parts(self.as_ptr(), self.len()) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        let (first, len) = self.parts_mut();
        let len = *len;
        // SAFETY: as `as_slice`, and `&mut self` makes the borrow unique.
        unsafe { slice::from_raw_parts_mut(first, len) }
    }

    /// Moves the values to a block of exactly `new_cap` slots, which is above `N` and at least
    /// the length; refused with nothing changed.
    fn try_grow_to(&mut self, new_cap: usize) -> Result<(), AllocFailure> {
        let len = self.len();
        let what = type_name::<Self>();
        debug_assert!(new_cap > N && new_cap >= len && new_cap > self.capacity);
        let new_layout = Layout::array::<T>(new_cap).map_err(|_| overflow(what, len, new_cap - len))?;
        if self.spilled() {
            // SAFETY: spilled, so `heap` is the field in use.
            let (old, len) = unsafe { self.data.heap };
            let old_layout = Layout::array::<T>(self.capacity).map_err(|_| overflow(what, len, 0))?;
            // SAFETY: `old` is a live block of `alloc` made with `old_layout` (`Data`'s
            // invariant), and `new_layout` is larger (`new_cap > capacity`).
            let block = unsafe { self.alloc.grow(old.cast(), old_layout, new_layout) }
                .map_err(|_| refused(what, new_layout))?;
            self.data.heap = (block.cast::<T>(), len);
        } else {
            let block = self.alloc.allocate(new_layout).map_err(|_| refused(what, new_layout))?;
            let to = block.cast::<T>();
            // SAFETY: inline, so the first `len` inline slots hold values; the block is fresh,
            // with room for `new_cap >= len` values, so the two do not overlap. The values
            // move: the inline slots are not read again once `heap` is the field in use.
            unsafe { ptr::copy_nonoverlapping(self.data.inline.as_ptr().cast::<T>(), to.as_ptr(), len) };
            let kept = if plant::on(Plant::SpillDropsLast) && len > 0 { len - 1 } else { len };
            self.data.heap = (to, kept);
        }
        self.capacity = new_cap;
        Ok(())
    }

    /// Room for `additional` more values, growing by doubling.
    fn try_grow_for(&mut self, additional: usize) -> Result<(), AllocFailure> {
        let (len, cap) = (self.len(), self.capacity());
        let Some(needed) = len.checked_add(additional) else {
            return Err(overflow(type_name::<Self>(), len, additional));
        };
        if needed <= cap {
            return Ok(());
        }
        self.try_grow_to(needed.max(cap.saturating_mul(2)).max(N + 1).max(4))
    }

    /// Room for `additional` more values; refused with nothing changed.
    #[inline]
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.try_grow_for(additional)
    }

    /// [`InlineVec::try_reserve`], panicking with the refusal.
    pub fn reserve(&mut self, additional: usize) {
        if let Err(failure) = self.try_reserve(additional) {
            fail(failure);
        }
    }

    /// Room for exactly `additional` more values (or `N + 1` slots, the least a spilled block
    /// has); refused with nothing changed.
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), AllocFailure> {
        let (len, cap) = (self.len(), self.capacity());
        let Some(needed) = len.checked_add(additional) else {
            return Err(overflow(type_name::<Self>(), len, additional));
        };
        if needed <= cap {
            return Ok(());
        }
        self.try_grow_to(needed.max(N + 1))
    }

    /// Gives back the room past the values: back inline when they fit `N`, else a block of
    /// exactly their count. The one way a spilled vector goes back inline.
    pub fn try_shrink_to_fit(&mut self) -> Result<(), AllocFailure> {
        if !self.spilled() {
            return Ok(());
        }
        let what = type_name::<Self>();
        // SAFETY: spilled, so `heap` is the field in use.
        let (block, len) = unsafe { self.data.heap };
        let old_layout = Layout::array::<T>(self.capacity).map_err(|_| overflow(what, len, 0))?;
        if len <= N {
            self.data.inline = ManuallyDrop::new([const { MaybeUninit::uninit() }; N]);
            // SAFETY: the block's first `len` slots hold values; `inline` (now the field in
            // use) has `N >= len` slots and lives in `self`, not in the block. The values move.
            unsafe { ptr::copy_nonoverlapping(block.as_ptr(), (*self.data.inline).as_mut_ptr().cast::<T>(), len) };
            self.capacity = len;
            // SAFETY: `block` was `alloc`'s, made with `old_layout`, and holds no value now.
            unsafe { self.alloc.deallocate(block.cast(), old_layout) };
        } else if len < self.capacity {
            let new_layout = Layout::array::<T>(len).map_err(|_| overflow(what, len, 0))?;
            // SAFETY: `block` is a live block made with `old_layout`; `new_layout` is smaller
            // and still holds the `len` values.
            let smaller = unsafe { self.alloc.shrink(block.cast(), old_layout, new_layout) }
                .map_err(|_| refused(what, new_layout))?;
            self.data.heap = (smaller.cast::<T>(), len);
            self.capacity = len;
        }
        Ok(())
    }

    /// Appends `value`, or gives it back with the refusal and nothing changed.
    ///
    /// The state is read once: inline with room, one compare and a store; spilled with room,
    /// `Vec::push`'s code (a compare of length and capacity, a store). Growth, the spill
    /// included, is out of line and cold, so the loop a push sits in carries no growth code.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), Refused<T>> {
        if self.capacity < N {
            // SAFETY: inline (`capacity < N`), so `inline` is the field in use and slot
            // `capacity`, below `N`, is free.
            unsafe { (*self.data.inline).as_mut_ptr().cast::<T>().add(self.capacity).write(value) };
            self.capacity += 1;
            return Ok(());
        }
        if self.capacity > N {
            // SAFETY: spilled, so `heap` is the field in use.
            let heap = unsafe { &mut self.data.heap };
            if heap.1 < self.capacity {
                // SAFETY: slot `heap.1` is inside the block (below its capacity) and free.
                unsafe { heap.0.as_ptr().add(heap.1).write(value) };
                heap.1 += 1;
                return Ok(());
            }
        }
        self.push_grown(value)
    }

    /// A push that grows first: the spill, or a full block.
    #[cold]
    #[inline(never)]
    fn push_grown(&mut self, value: T) -> Result<(), Refused<T>> {
        if let Err(why) = self.try_grow_for(1) {
            return Err(Refused { value, why });
        }
        let (first, len) = self.parts_mut();
        // SAFETY: there is room now, so slot `len` is inside the storage and holds no value.
        unsafe { first.add(*len).write(value) };
        *len += 1;
        Ok(())
    }

    /// [`InlineVec::try_push`], panicking with the refusal.
    #[inline]
    pub fn push(&mut self, value: T) {
        if let Err(refused) = self.try_push(value) {
            fail(refused.why);
        }
    }

    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        let (first, len) = self.parts_mut();
        if *len == 0 {
            return None;
        }
        *len -= 1;
        // SAFETY: slot `len` held a value, which is now outside the count and read once.
        Some(unsafe { first.add(*len).read() })
    }

    /// Inserts `value` at `index`, shifting the rest right; refused with the value back and
    /// nothing changed. An index past the length panics.
    #[track_caller]
    pub fn try_insert(&mut self, index: usize, value: T) -> Result<(), Refused<T>> {
        let len = self.len();
        if index > len {
            out_of_bounds(type_name::<Self>(), index, len);
        }
        if let Err(why) = self.try_grow_for(1) {
            return Err(Refused { value, why });
        }
        let (first, count) = self.parts_mut();
        // SAFETY: `index <= len < capacity`: the values from `index` move one slot right into
        // room the storage has, then slot `index` takes the new one.
        unsafe {
            let at = first.add(index);
            ptr::copy(at, at.add(1), len - index);
            at.write(value);
        }
        *count = len + 1;
        Ok(())
    }

    /// [`InlineVec::try_insert`], panicking with the refusal.
    #[track_caller]
    pub fn insert(&mut self, index: usize, value: T) {
        if let Err(refused) = self.try_insert(index, value) {
            fail(refused.why);
        }
    }

    /// Takes out the value at `index`, shifting the rest left. An index past the end panics.
    #[track_caller]
    pub fn remove(&mut self, index: usize) -> T {
        let len = self.len();
        if index >= len {
            out_of_bounds(type_name::<Self>(), index, len);
        }
        let (first, count) = self.parts_mut();
        // SAFETY: `index < len`: the value is read once, and the values after it move left over
        // its slot, within the count.
        let value = unsafe {
            let at = first.add(index);
            let value = at.read();
            ptr::copy(at.add(1), at, len - index - 1);
            value
        };
        *count = len - 1;
        value
    }

    /// Takes out the value at `index`, the last value moving into its place. An index past the
    /// end panics.
    #[track_caller]
    pub fn swap_remove(&mut self, index: usize) -> T {
        let len = self.len();
        if index >= len {
            out_of_bounds(type_name::<Self>(), index, len);
        }
        let (first, count) = self.parts_mut();
        // SAFETY: `index < len`: the value is read once, then the last value (perhaps the same
        // slot, which `copy` allows) moves into its slot and leaves the count.
        let value = unsafe {
            let at = first.add(index);
            let value = at.read();
            ptr::copy(first.add(len - 1), at, 1);
            value
        };
        *count = len - 1;
        value
    }

    /// Keeps the first `len` values, dropping the rest. A panicking drop leaks the values after
    /// it; nothing is dropped twice.
    pub fn truncate(&mut self, len: usize) {
        let (first, count) = self.parts_mut();
        let old = *count;
        if len >= old {
            return;
        }
        *count = len;
        // SAFETY: slots `len..old` held values and are now outside the count: dropped once.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(first.add(len), old - len)) };
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Drops the values in `range` and shifts the rest left. A range past the length panics;
    /// a panicking drop leaks the values after the range.
    #[track_caller]
    pub fn remove_range(&mut self, range: Range<usize>) {
        let len = self.len();
        if range.start > range.end || range.end > len {
            out_of_bounds(type_name::<Self>(), range.end, len);
        }
        let (first, count) = self.parts_mut();
        *count = range.start;
        // SAFETY: slots `range` held values and are outside the count now: dropped once. Then
        // the `len - range.end` values after them move left to `range.start`, within the
        // storage, and the count takes them back.
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(first.add(range.start), range.end - range.start));
            ptr::copy(first.add(range.end), first.add(range.start), len - range.end);
        }
        *count = len - (range.end - range.start);
    }

    /// Keeps the values `keep` says to, in order. A panic in `keep` or in a drop leaks the
    /// values not yet visited; nothing is dropped twice.
    pub fn retain(&mut self, mut keep: impl FnMut(&mut T) -> bool) {
        let (first, count) = self.parts_mut();
        let len = *count;
        *count = 0;
        let mut kept = 0;
        for at in 0..len {
            // SAFETY: slot `at` holds a value not yet visited (`kept <= at`); a kept one moves to
            // slot `kept`, which holds none (moved or dropped before), a removed one is dropped
            // once. The count is 0 meanwhile, so a panic leaks rather than drops twice.
            unsafe {
                let slot = first.add(at);
                if keep(&mut *slot) {
                    if kept != at {
                        ptr::copy_nonoverlapping(slot, first.add(kept), 1);
                    }
                    kept += 1;
                } else {
                    ptr::drop_in_place(slot);
                }
            }
        }
        *count = kept;
    }

    /// Appends clones of `values`; refused with nothing changed. A panicking clone keeps those
    /// appended before it.
    pub fn try_extend_from_slice(&mut self, values: &[T]) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        self.try_grow_for(values.len())?;
        // The state is read once for the whole slice.
        let (first, count) = self.parts_mut();
        for value in values {
            // SAFETY: room for all of `values` was made, so slot `count` is free.
            unsafe { first.add(*count).write(value.clone()) };
            *count += 1;
        }
        Ok(())
    }

    /// A copy in a clone of the allocator; refused with nothing made.
    pub fn try_clone(&self) -> Result<InlineVec<T, N, A>, AllocFailure>
    where
        T: Clone,
        A: Clone,
    {
        InlineVec::try_from_slice_in(self.as_slice(), self.alloc.clone())
    }
}

impl<T, const N: usize, A: Allocator> Drop for InlineVec<T, N, A> {
    #[inline]
    fn drop(&mut self) {
        // Inline values that need no drop: nothing to do, no call made.
        if !self.spilled() && !core::mem::needs_drop::<T>() {
            return;
        }
        let len = self.len();
        let first = self.as_mut_ptr();
        let _free = if self.spilled() {
            Layout::array::<T>(self.capacity).ok().map(|layout| FreeBlock {
                alloc: &self.alloc,
                // SAFETY: spilled, so `first` is the heap block's pointer, non-null.
                block: unsafe { NonNull::new_unchecked(first.cast::<u8>()) },
                layout,
            })
        } else {
            None
        };
        // SAFETY: the first `len` slots hold values, dropped once here; the block (if any) is
        // freed after, by `_free`, even when a drop panics.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(first, len)) };
    }
}

impl<T: Clone, const N: usize, A: Allocator + Clone> Clone for InlineVec<T, N, A> {
    fn clone(&self) -> Self {
        match self.try_clone() {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<T, const N: usize, A: Allocator> Extend<T> for InlineVec<T, N, A> {
    fn extend<It: IntoIterator<Item = T>>(&mut self, values: It) {
        let values = values.into_iter();
        self.reserve(values.size_hint().0);
        for value in values {
            self.push(value);
        }
    }
}

impl<T, const N: usize, A: Allocator> Deref for InlineVec<T, N, A> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, const N: usize, A: Allocator> DerefMut for InlineVec<T, N, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, const N: usize, A: Allocator> AsRef<[T]> for InlineVec<T, N, A> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<'a, T, const N: usize, A: Allocator> IntoIterator for &'a InlineVec<T, N, A> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.as_slice().iter()
    }
}

impl<'a, T, const N: usize, A: Allocator> IntoIterator for &'a mut InlineVec<T, N, A> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.as_mut_slice().iter_mut()
    }
}

impl<T: fmt::Debug, const N: usize, A: Allocator> fmt::Debug for InlineVec<T, N, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<T: PartialEq, const N: usize, const M: usize, A: Allocator, B: Allocator> PartialEq<InlineVec<T, M, B>>
    for InlineVec<T, N, A>
{
    fn eq(&self, other: &InlineVec<T, M, B>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq, const N: usize, A: Allocator> PartialEq<[T]> for InlineVec<T, N, A> {
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: Eq, const N: usize, A: Allocator> Eq for InlineVec<T, N, A> {}

impl<T: Hash, const N: usize, A: Allocator> Hash for InlineVec<T, N, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}
