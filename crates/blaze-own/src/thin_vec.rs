//! [`ThinVec`]: a growable array whose length and capacity (`u32` each) live in the heap block
//! header rather than inline on the stack.
//!
//! When the allocator `A` is zero-sized (e.g. `Global` or [`TestHeap`](crate::testing::TestHeap)),
//! `ThinVec<T, A>` and `Option<ThinVec<T, A>>` are each a single machine word (8 bytes on 64-bit,
//! vs 24 bytes for [`Vec<T, A>`](crate::Vec)). Over an [`Alloc<'a>`](crate::Alloc) handle
//! ([`AThinVec<'a, T>`](crate::AThinVec)), it is 2 words (16 bytes, vs 32 bytes for
//! [`AVec<'a, T>`](crate::AVec)) and preserves the `NonNull` niche.
//!
//! An empty `ThinVec` points to a static immutable sentinel header (`len = 0, cap = 0`) and
//! performs **zero heap allocations** until elements are pushed or non-zero capacity is reserved.

use core::{
    alloc::{Allocator, Layout},
    any::type_name,
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{DropGuard, align_of, size_of},
    ops::{Deref, DerefMut, Index, IndexMut},
    ptr::{self, NonNull},
    slice::{self, SliceIndex},
};

use crate::{
    error::{AllocFailure, fail},
    spec::CloneSlice,
    vec::Refused,
};

/// 8-byte header stored at offset 0 of every allocated [`ThinVec`] block (and of
/// [`EMPTY_HEADER`]).
#[repr(C)]
struct ThinVecHeader {
    len: u32,
    cap: u32,
}

/// Static immutable sentinel pointed to by every unallocated (empty) [`ThinVec`].
///
/// Never mutated: every write path checks `cap == 0` (or `len == cap`) and allocates a heap
/// block first.
static EMPTY_HEADER: ThinVecHeader = ThinVecHeader { len: 0, cap: 0 };

/// Byte offset of the first `T` from the start of an allocated [`ThinVec`] block.
///
/// Independent of capacity, so `Allocator::grow` preserves header and element offsets in place.
#[inline(always)]
const fn data_offset<T>() -> usize {
    size_of::<ThinVecHeader>().next_multiple_of(align_of::<T>())
}

/// Minimum non-zero capacity on initial growth (matches [`RawBuf`](crate::RawBuf) / `std`).
#[inline(always)]
const fn min_non_zero_cap<T>() -> usize {
    if size_of::<T>() == 1 {
        8
    } else if size_of::<T>() <= 1024 {
        4
    } else {
        1
    }
}

/// Computes `(layout, data_offset)` for a [`ThinVecHeader`] followed by `[T; cap]`, padded to
/// the combined alignment.
#[inline]
fn block_layout<T>(cap: usize) -> Option<(Layout, usize)> {
    if cap > u32::MAX as usize {
        return None;
    }
    let header_layout = Layout::new::<ThinVecHeader>();
    let slice_layout = Layout::array::<T>(cap).ok()?;
    let (combined, offset) = header_layout.extend(slice_layout).ok()?;
    debug_assert_eq!(offset, data_offset::<T>());
    Some((combined.pad_to_align(), offset))
}

/// A thin growable array of `T` backed by an explicit allocator `A`.
///
/// # Invariants
///
/// - When `header().cap == 0`, `ptr` points to [`EMPTY_HEADER`] (`len == 0`) and nothing is
///   allocated; [`EMPTY_HEADER`] is never mutated.
/// - When `header().cap > 0`, `ptr` points to a mutable heap block allocated by `alloc` with
///   `block_layout::<T>(cap)`, with full-block provenance; `len <= cap`, and the first `len`
///   slots starting at `data_offset::<T>()` hold initialized `T` values.
pub struct ThinVec<T, A: Allocator> {
    ptr: NonNull<u8>,
    alloc: A,
    /// Owns `T`s, for drop check (`#[may_dangle]` on `Drop` relies on it).
    _marker: PhantomData<T>,
}

#[cfg(any(test, feature = "std"))]
const _: () = {
    assert!(size_of::<ThinVec<u8, std::alloc::Global>>() == size_of::<usize>());
    assert!(size_of::<Option<ThinVec<u8, std::alloc::Global>>>() == size_of::<usize>());
    assert!(size_of::<ThinVec<[u64; 16], std::alloc::Global>>() == size_of::<usize>());
    assert!(size_of::<Option<ThinVec<[u64; 16], std::alloc::Global>>>() == size_of::<usize>());
};

const _: () = {
    assert!(size_of::<ThinVec<u8, crate::Alloc<'static>>>() == 2 * size_of::<usize>());
    assert!(size_of::<Option<ThinVec<u8, crate::Alloc<'static>>>>() == 2 * size_of::<usize>());
};

// SAFETY: `ThinVec` uniquely owns its elements (`T: Send`) and allocator (`A: Send`).
unsafe impl<T: Send, A: Allocator + Send> Send for ThinVec<T, A> {}
// SAFETY: a shared `&ThinVec` exposes only `&[T]` (`T: Sync`) and `&A` (`A: Sync`);
// `EMPTY_HEADER` is immutable static data.
unsafe impl<T: Sync, A: Allocator + Sync> Sync for ThinVec<T, A> {}

impl<T, A: Allocator> ThinVec<T, A> {
    /// Creates an empty `ThinVec` in `alloc`; performs zero heap allocations.
    #[inline]
    pub const fn new_in(alloc: A) -> ThinVec<T, A> {
        let ptr = NonNull::from_ref(&EMPTY_HEADER).cast::<u8>();
        ThinVec { ptr, alloc, _marker: PhantomData }
    }

    /// Creates an empty `ThinVec` with room for at least `capacity` elements in `alloc`; panics
    /// with [`AllocFailure`] if memory cannot be obtained.
    ///
    /// A `capacity` of `0` performs zero heap allocations.
    #[track_caller]
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> ThinVec<T, A> {
        match ThinVec::try_with_capacity_in(capacity, alloc) {
            Ok(v) => v,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible version of [`ThinVec::with_capacity_in`].
    #[inline]
    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<ThinVec<T, A>, AllocFailure> {
        let mut v = ThinVec::new_in(alloc);
        v.try_reserve_exact(capacity)?;
        Ok(v)
    }

    /// Creates a `ThinVec` containing clones of `values` in `alloc`.
    #[track_caller]
    pub fn from_slice_in(values: &[T], alloc: A) -> ThinVec<T, A>
    where
        T: Clone,
    {
        let mut v = ThinVec::with_capacity_in(values.len(), alloc);
        v.extend_from_slice(values);
        v
    }

    #[inline(always)]
    fn header(&self) -> &ThinVecHeader {
        // SAFETY: `self.ptr` always points to a live `ThinVecHeader` (either `EMPTY_HEADER` or an
        // allocated heap block).
        unsafe { &*self.ptr.as_ptr().cast::<ThinVecHeader>() }
    }

    /// Number of elements currently stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.header().len as usize
    }

    /// Whether the vector contains zero elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.header().len == 0
    }

    /// Number of elements the vector can hold without reallocating.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.header().cap as usize
    }

    /// Reference to the backing allocator.
    #[inline]
    pub const fn allocator(&self) -> &A {
        &self.alloc
    }

    /// Pointer to the first element (dangling and properly aligned when unallocated).
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        if self.header().cap == 0 {
            NonNull::dangling().as_ptr()
        } else {
            // SAFETY: `cap > 0` implies a live heap block with elements at `data_offset::<T>()`.
            unsafe { self.ptr.as_ptr().add(data_offset::<T>()).cast::<T>() }
        }
    }

    /// Mutable pointer to the first element (dangling and properly aligned when unallocated).
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        if self.header().cap == 0 {
            NonNull::dangling().as_ptr()
        } else {
            // SAFETY: `cap > 0` implies a live mutable heap block with elements at
            // `data_offset::<T>()`.
            unsafe { self.ptr.as_ptr().add(data_offset::<T>()).cast::<T>() }
        }
    }

    /// Extracts a slice containing the entire vector.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        let len = self.header().len as usize;
        if len == 0 {
            &[]
        } else {
            // SAFETY: `len > 0` implies `cap > 0` and `len` initialized `T`s at
            // `data_offset::<T>()`.
            unsafe { slice::from_raw_parts(self.ptr.as_ptr().add(data_offset::<T>()).cast::<T>(), len) }
        }
    }

    /// Extracts a mutable slice of the entire vector.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        let len = self.header().len as usize;
        if len == 0 {
            &mut []
        } else {
            // SAFETY: `len > 0` implies `cap > 0`, `len` initialized `T`s at `data_offset::<T>()`,
            // and `&mut self` grants exclusive access.
            unsafe {
                slice::from_raw_parts_mut(self.ptr.as_ptr().add(data_offset::<T>()).cast::<T>(), len)
            }
        }
    }

    /// Reserves room for at least `additional` more elements, growing amortized geometrically;
    /// panics with [`AllocFailure`] on failure.
    #[track_caller]
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        if let Err(failure) = self.try_reserve(additional) {
            fail(failure);
        }
    }

    /// Fallible version of [`ThinVec::reserve`].
    #[inline]
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.try_reserve_internal(additional, false)
    }

    /// Reserves room for at least `additional` more elements without over-allocating; panics with
    /// [`AllocFailure`] on failure.
    #[track_caller]
    #[inline]
    pub fn reserve_exact(&mut self, additional: usize) {
        if let Err(failure) = self.try_reserve_exact(additional) {
            fail(failure);
        }
    }

    /// Fallible version of [`ThinVec::reserve_exact`].
    #[inline]
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.try_reserve_internal(additional, true)
    }

    fn try_reserve_internal(&mut self, additional: usize, exact: bool) -> Result<(), AllocFailure> {
        let hdr = self.header();
        let len = hdr.len as usize;
        let cap = hdr.cap as usize;
        if additional <= cap.wrapping_sub(len) {
            return Ok(());
        }
        let overflow = AllocFailure::CapacityOverflow { what: type_name::<T>(), len, additional };
        let Some(required) = len.checked_add(additional) else {
            return Err(overflow);
        };
        if required > u32::MAX as usize {
            return Err(overflow);
        }
        let new_cap = if size_of::<T>() == 0 {
            // For ZST `T`, a single header allocation holds up to `u32::MAX` elements without
            // ever growing again.
            u32::MAX as usize
        } else if exact {
            required
        } else {
            let doubled = cap.saturating_mul(2).min(u32::MAX as usize);
            required.max(doubled).max(min_non_zero_cap::<T>())
        };
        self.grow_to(new_cap, overflow)
    }

    fn grow_to(&mut self, new_cap: usize, overflow: AllocFailure) -> Result<(), AllocFailure> {
        let old_cap = self.header().cap as usize;
        debug_assert!(new_cap > old_cap && new_cap <= u32::MAX as usize);
        let Some((new_layout, _)) = block_layout::<T>(new_cap) else {
            return Err(overflow);
        };
        let refused = AllocFailure::Refused { what: type_name::<T>(), layout: new_layout };
        if old_cap == 0 {
            let block = self.alloc.allocate(new_layout).map_err(|_| refused)?.cast::<u8>();
            // SAFETY: `block` is freshly allocated with `new_layout`, aligned for `ThinVecHeader`.
            unsafe {
                block
                    .as_ptr()
                    .cast::<ThinVecHeader>()
                    .write(ThinVecHeader { len: 0, cap: new_cap as u32 });
            }
            self.ptr = block;
            return Ok(());
        }
        // SAFETY: `old_cap > 0` implies the existing block was allocated with
        // `block_layout::<T>(old_cap)`.
        let (old_layout, _) = unsafe { block_layout::<T>(old_cap).unwrap_unchecked() };
        if new_layout == old_layout {
            // Padding bytes already cover `new_cap` elements (e.g. small `u8` capacities inside
            // the 4-byte header alignment); update `cap` in place with no allocator call.
            // SAFETY: `old_cap > 0` means `self.ptr` points to a mutable heap header.
            unsafe { (*self.ptr.as_ptr().cast::<ThinVecHeader>()).cap = new_cap as u32 };
            return Ok(());
        }
        // SAFETY: `self.ptr` was allocated by `self.alloc` with `old_layout`; `new_layout` has
        // the exact same alignment (`max(4, align_of::<T>())`) and strictly larger size.
        let block = unsafe { self.alloc.grow(self.ptr, old_layout, new_layout) }
            .map_err(|_| refused)?
            .cast::<u8>();
        // SAFETY: `grow` preserved the header and initialized elements; update `cap` to `new_cap`.
        unsafe { (*block.as_ptr().cast::<ThinVecHeader>()).cap = new_cap as u32 };
        self.ptr = block;
        Ok(())
    }

    #[cold]
    #[inline(never)]
    #[track_caller]
    fn grow_one_cold(&mut self) {
        if let Err(failure) = self.try_reserve(1) {
            fail(failure);
        }
    }

    /// Appends `value` to the back of the vector; panics with [`AllocFailure`] on allocation
    /// failure.
    #[track_caller]
    #[inline]
    pub fn push(&mut self, value: T) {
        let hdr = self.header();
        let len = hdr.len;
        if len == hdr.cap {
            self.grow_one_cold();
        }
        // SAFETY: after the check / `grow_one_cold`, `self.ptr` points to a mutable heap block
        // with `len < cap`, so slot `len` is in-bounds and uninitialized.
        unsafe {
            let ptr = self.ptr.as_ptr();
            ptr.add(data_offset::<T>()).cast::<T>().add(len as usize).write(value);
            (*ptr.cast::<ThinVecHeader>()).len = len + 1;
        }
    }

    /// Appends `value` to the back of the vector, returning `value` with [`AllocFailure`] if
    /// memory cannot be obtained.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), Refused<T>> {
        let hdr = self.header();
        let len = hdr.len;
        if len == hdr.cap {
            if let Err(why) = self.try_reserve(1) {
                return Err(Refused { value, why });
            }
        }
        // SAFETY: `len < cap`, so `self.ptr` is a mutable heap block and slot `len` is
        // uninitialized.
        unsafe {
            let ptr = self.ptr.as_ptr();
            ptr.add(data_offset::<T>()).cast::<T>().add(len as usize).write(value);
            (*ptr.cast::<ThinVecHeader>()).len = len + 1;
        }
        Ok(())
    }

    /// Removes and returns the last element, or `None` if empty.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        let len = self.header().len;
        if len == 0 {
            return None;
        }
        let new_len = len - 1;
        // SAFETY: `len > 0` implies `cap > 0` (mutable heap block) and slot `new_len` holds an
        // initialized `T`. Updating `len` before reading ensures ownership transfers once.
        unsafe {
            let ptr = self.ptr.as_ptr();
            (*ptr.cast::<ThinVecHeader>()).len = new_len;
            Some(ptr.add(data_offset::<T>()).cast::<T>().add(new_len as usize).read())
        }
    }

    /// Shortens the vector, keeping the first `new_len` elements and dropping the rest.
    ///
    /// Has no effect if `new_len >= self.len()`.
    pub fn truncate(&mut self, new_len: usize) {
        let len = self.len();
        if new_len >= len {
            return;
        }
        let drop_count = len - new_len;
        // SAFETY: `len > new_len` implies `len > 0` and `cap > 0` (mutable heap block). Setting
        // `len = new_len` before dropping ensures a panicking `T::drop` cannot cause double drops.
        unsafe {
            let ptr = self.ptr.as_ptr();
            let tail = ptr.add(data_offset::<T>()).cast::<T>().add(new_len);
            (*ptr.cast::<ThinVecHeader>()).len = new_len as u32;
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(tail, drop_count));
        }
    }

    /// Removes all elements from the vector, keeping its allocated capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Clones and appends all elements in `other` to the vector; panics with [`AllocFailure`] on
    /// allocation failure.
    #[track_caller]
    pub fn extend_from_slice(&mut self, other: &[T])
    where
        T: Clone,
    {
        if let Err(failure) = self.try_extend_from_slice(other) {
            fail(failure);
        }
    }

    /// Fallible version of [`ThinVec::extend_from_slice`].
    pub fn try_extend_from_slice(&mut self, other: &[T]) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        if other.is_empty() {
            return Ok(());
        }
        self.try_reserve(other.len())?;
        let old_len = self.len();
        let ptr = self.ptr.as_ptr();
        // SAFETY: `try_reserve(other.len())` with `other.len() > 0` guarantees `cap > 0` and
        // `old_len + other.len() <= cap`.
        let dst = unsafe { ptr.add(data_offset::<T>()).cast::<T>().add(old_len) };
        let hdr = ptr.cast::<ThinVecHeader>();
        // Record all clones that succeeded even if a subsequent `T::clone` panics.
        let mut written = DropGuard::new(0usize, |written| {
            // SAFETY: `old_len + written <= cap` and those slots hold initialized values.
            unsafe { (*hdr).len = (old_len + written) as u32 };
        });
        // SAFETY: `other.len()` slots at `dst` lie in one allocation, are uninitialized, and
        // cannot overlap `other` (`&mut self`).
        unsafe { CloneSlice::clone_to(other, dst, &mut written) };
        let count = DropGuard::dismiss(written);
        // SAFETY: `old_len + count <= cap` and all `count` slots are now initialized.
        unsafe { (*hdr).len = (old_len + count) as u32 };
        Ok(())
    }
}

// SAFETY (`may_dangle`): `Drop` drops the owned `T`s (`PhantomData<T>` informs dropck) and reads
// nothing else of them.
unsafe impl<#[may_dangle] T, A: Allocator> Drop for ThinVec<T, A> {
    #[inline]
    fn drop(&mut self) {
        let cap = self.header().cap as usize;
        if cap == 0 {
            return;
        }
        self.drop_slow(cap);
    }
}

impl<T, A: Allocator> ThinVec<T, A> {
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self, cap: usize) {
        let len = self.header().len as usize;
        // SAFETY: `cap > 0` implies the block was allocated with `block_layout::<T>(cap)`.
        let (layout, offset) = unsafe { block_layout::<T>(cap).unwrap_unchecked() };
        let block = self.ptr;
        // Ensure the block is deallocated even if a `T::drop` panics.
        let _dealloc = DropGuard::new((), |()| {
            // SAFETY: `block` was allocated by `self.alloc` with `layout`, freed once here.
            unsafe { self.alloc.deallocate(block, layout) };
        });
        if len > 0 {
            // SAFETY: `len` elements starting at `offset` are initialized and dropped once here.
            unsafe {
                let data = block.as_ptr().add(offset).cast::<T>();
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(data, len));
            }
        }
    }
}

impl<T, A: Allocator> Deref for ThinVec<T, A> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, A: Allocator> DerefMut for ThinVec<T, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, A: Allocator> AsRef<[T]> for ThinVec<T, A> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, A: Allocator> AsMut<[T]> for ThinVec<T, A> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, A: Allocator> Borrow<[T]> for ThinVec<T, A> {
    #[inline]
    fn borrow(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, A: Allocator> BorrowMut<[T]> for ThinVec<T, A> {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: SliceIndex<[T]>, A: Allocator> Index<I> for ThinVec<T, A> {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &I::Output {
        Index::index(self.as_slice(), index)
    }
}

impl<T, I: SliceIndex<[T]>, A: Allocator> IndexMut<I> for ThinVec<T, A> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut I::Output {
        IndexMut::index_mut(self.as_mut_slice(), index)
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for ThinVec<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: Hash, A: Allocator> Hash for ThinVec<T, A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        Hash::hash(self.as_slice(), state);
    }
}

impl<T: PartialEq<U>, U, A: Allocator, B: Allocator> PartialEq<ThinVec<U, B>> for ThinVec<T, A> {
    #[inline]
    fn eq(&self, other: &ThinVec<U, B>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq<U>, U, A: Allocator> PartialEq<[U]> for ThinVec<T, A> {
    #[inline]
    fn eq(&self, other: &[U]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq<U>, U, A: Allocator> PartialEq<&[U]> for ThinVec<T, A> {
    #[inline]
    fn eq(&self, other: &&[U]) -> bool {
        self.as_slice() == *other
    }
}

impl<T: PartialEq<U>, U, A: Allocator, const N: usize> PartialEq<[U; N]> for ThinVec<T, A> {
    #[inline]
    fn eq(&self, other: &[U; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, A: Allocator> Eq for ThinVec<T, A> {}

impl<T: PartialOrd, A: Allocator> PartialOrd for ThinVec<T, A> {
    #[inline]
    fn partial_cmp(&self, other: &ThinVec<T, A>) -> Option<Ordering> {
        PartialOrd::partial_cmp(self.as_slice(), other.as_slice())
    }
}

impl<T: Ord, A: Allocator> Ord for ThinVec<T, A> {
    #[inline]
    fn cmp(&self, other: &ThinVec<T, A>) -> Ordering {
        Ord::cmp(self.as_slice(), other.as_slice())
    }
}

impl<T, A: Allocator> Extend<T> for ThinVec<T, A> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        if lower > 0 {
            self.reserve(lower);
        }
        for item in iter {
            self.push(item);
        }
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a ThinVec<T, A> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> slice::Iter<'a, T> {
        self.as_slice().iter()
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a mut ThinVec<T, A> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.as_mut_slice().iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use core::{
        mem::{align_of, size_of},
        sync::atomic::{AtomicUsize, Ordering},
    };
    use std::{
        alloc::Global,
        format,
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc as StdArc,
        vec::Vec as StdVec,
    };

    use super::ThinVec;
    use crate::{
        AThinVec, Erased,
        testing::{Counting, TestHeap, every_failure},
    };

    #[test]
    fn size_and_niche() {
        assert_eq!(size_of::<ThinVec<u8, Global>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<ThinVec<u8, Global>>>(), size_of::<usize>());
        assert_eq!(size_of::<ThinVec<u64, TestHeap>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<ThinVec<u64, TestHeap>>>(), size_of::<usize>());

        assert_eq!(size_of::<AThinVec<'static, u64>>(), 2 * size_of::<usize>());
        assert_eq!(size_of::<Option<AThinVec<'static, u64>>>(), 2 * size_of::<usize>());
    }

    #[test]
    fn empty_thin_vec_does_zero_allocations() {
        let counting = Counting::new(Global);
        {
            let mut v = ThinVec::<u64, _>::new_in(&counting);
            assert!(v.is_empty());
            assert_eq!(v.len(), 0);
            assert_eq!(v.capacity(), 0);
            assert_eq!(v.as_slice(), &[]);
            assert_eq!(v.as_mut_slice(), &mut []);
            assert_eq!(v.pop(), None);
            v.clear();
            v.extend_from_slice(&[]);
            assert_eq!(counting.allocations(), 0);
            assert_eq!(counting.live(), 0);

            let v0 = ThinVec::<u64, _>::with_capacity_in(0, &counting);
            assert_eq!(v0.capacity(), 0);
            assert_eq!(counting.allocations(), 0);

            let erased = Erased::new(&counting);
            let av: AThinVec<'_, u64> = ThinVec::new_in(erased.handle());
            assert!(av.is_empty());
            assert_eq!(counting.allocations(), 0);
        }
        counting.assert_empty();
    }

    #[test]
    fn push_pop_grow_clear_and_extend_from_slice() {
        let counting = Counting::new(Global);
        {
            let mut v = ThinVec::new_in(&counting);
            for i in 0..200u32 {
                v.push(i);
            }
            assert_eq!(v.len(), 200);
            assert!(v.capacity() >= 200);
            for i in 0..200u32 {
                assert_eq!(v[i as usize], i);
            }
            // Mutate via DerefMut.
            v[10] = 999;
            assert_eq!(v.as_slice()[10], 999);
            v[10] = 10;

            for i in (100..200u32).rev() {
                assert_eq!(v.pop(), Some(i));
            }
            assert_eq!(v.len(), 100);

            v.extend_from_slice(&[1000, 1001, 1002]);
            assert_eq!(v.len(), 103);
            assert_eq!(&v[100..], &[1000, 1001, 1002]);

            let cap_before = v.capacity();
            v.clear();
            assert!(v.is_empty());
            assert_eq!(v.len(), 0);
            assert_eq!(v.capacity(), cap_before);
            assert_eq!(v.pop(), None);

            // Re-populate after clear.
            v.push(7);
            v.push(8);
            assert_eq!(v, [7, 8]);
            assert_eq!(format!("{v:?}"), "[7, 8]");

            // Test small `u8` capacity growth across padding boundary (`cap = 1 -> 2 -> 4 -> 5`).
            let mut bytes = ThinVec::<u8, _>::with_capacity_in(1, &counting);
            assert_eq!(bytes.capacity(), 1);
            for b in 0..20u8 {
                bytes.push(b);
            }
            assert_eq!(bytes.as_slice(), &(0..20u8).collect::<StdVec<_>>()[..]);
        }
        counting.assert_empty();
    }

    #[test]
    fn drop_counts_and_panic_safety() {
        struct DropItem {
            drops: StdArc<AtomicUsize>,
            panic_on_drop: bool,
        }
        impl Drop for DropItem {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
                if self.panic_on_drop {
                    panic!("item drop panic");
                }
            }
        }

        let counting = Counting::new(Global);
        let drops = StdArc::new(AtomicUsize::new(0));
        {
            let mut v = ThinVec::new_in(&counting);
            for _ in 0..10 {
                v.push(DropItem { drops: StdArc::clone(&drops), panic_on_drop: false });
            }
            assert_eq!(drops.load(Ordering::Relaxed), 0);

            let popped = v.pop().unwrap();
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            drop(popped);
            assert_eq!(drops.load(Ordering::Relaxed), 1);

            v.clear();
            assert_eq!(drops.load(Ordering::Relaxed), 10);

            for _ in 0..4 {
                v.push(DropItem { drops: StdArc::clone(&drops), panic_on_drop: false });
            }
        }
        assert_eq!(drops.load(Ordering::Relaxed), 14);
        counting.assert_empty();

        // Panic in `clear` drops all remaining elements and does not double-drop on `ThinVec::drop`.
        let drops = StdArc::new(AtomicUsize::new(0));
        let res = catch_unwind(AssertUnwindSafe(|| {
            let mut v = ThinVec::new_in(&counting);
            for i in 0..5 {
                v.push(DropItem { drops: StdArc::clone(&drops), panic_on_drop: i == 2 });
            }
            v.clear();
        }));
        assert!(res.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 5);
        counting.assert_empty();

        // Panic in `extend_from_slice` clone still tracks and drops the cloned prefix.
        struct PanicClone {
            drops: StdArc<AtomicUsize>,
            should_panic: bool,
        }
        impl Clone for PanicClone {
            fn clone(&self) -> Self {
                if self.should_panic {
                    panic!("clone panic");
                }
                PanicClone { drops: StdArc::clone(&self.drops), should_panic: false }
            }
        }
        impl Drop for PanicClone {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let clone_drops = StdArc::new(AtomicUsize::new(0));
        let src = [
            PanicClone { drops: StdArc::clone(&clone_drops), should_panic: false },
            PanicClone { drops: StdArc::clone(&clone_drops), should_panic: false },
            PanicClone { drops: StdArc::clone(&clone_drops), should_panic: true },
        ];
        let res = catch_unwind(AssertUnwindSafe(|| {
            let mut v = ThinVec::new_in(&counting);
            v.push(PanicClone { drops: StdArc::clone(&clone_drops), should_panic: false });
            v.extend_from_slice(&src);
        }));
        assert!(res.is_err());
        // 1 initial pushed + 2 cloned before panic = 3 drops.
        assert_eq!(clone_drops.load(Ordering::Relaxed), 3);
        counting.assert_empty();
    }

    #[test]
    fn over_aligned_and_zst_elements() {
        #[repr(align(64))]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Aligned64([u64; 8]);

        #[repr(align(64))]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct AlignedZst;

        let counting = Counting::new(Global);
        {
            let mut v = ThinVec::new_in(&counting);
            // Even when empty, `as_ptr()` is aligned to 64.
            assert_eq!((v.as_ptr() as usize) % align_of::<Aligned64>(), 0);
            for i in 0..32u64 {
                v.push(Aligned64([i; 8]));
                assert_eq!((v.as_ptr() as usize) % align_of::<Aligned64>(), 0);
            }
            for i in 0..32u64 {
                assert_eq!(v[i as usize], Aligned64([i; 8]));
            }
            assert_eq!(v.pop(), Some(Aligned64([31; 8])));

            // ZST: empty allocates nothing, first push allocates header once, subsequent pushes
            // never reallocate.
            let mut zv = ThinVec::<AlignedZst, _>::new_in(&counting);
            assert_eq!((zv.as_ptr() as usize) % 64, 0);
            let allocs_before = counting.allocations();
            zv.push(AlignedZst);
            assert_eq!(counting.allocations(), allocs_before + 1);
            assert_eq!((zv.as_ptr() as usize) % 64, 0);
            for _ in 0..100 {
                zv.push(AlignedZst);
            }
            assert_eq!(counting.allocations(), allocs_before + 1);
            assert_eq!(zv.len(), 101);
            assert_eq!(zv.pop(), Some(AlignedZst));
            assert_eq!(zv.len(), 100);
        }
        counting.assert_empty();
    }

    #[test]
    fn zst_drop_counts() {
        static ZST_DROPS: AtomicUsize = AtomicUsize::new(0);
        struct ZstDrop;
        impl Drop for ZstDrop {
            fn drop(&mut self) {
                ZST_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        assert_eq!(size_of::<ZstDrop>(), 0);
        ZST_DROPS.store(0, Ordering::Relaxed);
        let counting = Counting::new(Global);
        {
            let mut v = ThinVec::new_in(&counting);
            for _ in 0..9 {
                v.push(ZstDrop);
            }
            assert!(v.pop().is_some());
            assert_eq!(ZST_DROPS.load(Ordering::Relaxed), 1);
        }
        assert_eq!(ZST_DROPS.load(Ordering::Relaxed), 9);
        counting.assert_empty();
    }

    #[test]
    fn try_forms_fail_cleanly_at_every_allocation() {
        every_failure(&Global, |alloc| {
            let mut v = ThinVec::try_with_capacity_in(2, alloc)?;
            for i in 0..12u32 {
                v.try_push(i).map_err(|r| r.why)?;
            }
            v.try_extend_from_slice(&[100, 101, 102, 103])?;
            assert_eq!(v.len(), 16);
            assert_eq!(&v[12..], &[100, 101, 102, 103]);
            Ok::<(), crate::AllocFailure>(())
        })
        .unwrap();
    }
}
