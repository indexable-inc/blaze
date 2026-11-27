//! Hardware-floor branded unmanaged vectors: [`FrameVec`] (bump/frame arena) and
//! [`UnmanagedVec`] (linear heap-backed vector for [`DropWith`]).
//!
//! Both containers store **no allocator** and parameterize their length/capacity width by the
//! sealed [`LenIdx`] trait (`I = u32` default, `u16` for [`SmallFrameVec`]/[`SmallUnmanagedVec`],
//! `usize` for [`LargeFrameVec`]/[`LargeUnmanagedVec`]):
//!
//! | alias | `I` | size (64-bit) | `Option<Self>` | max elements |
//! |---|---|---|---|---|
//! | [`FrameVec`] / [`UnmanagedVec`] | `u32` | **16 B** (align 8) | **16 B** | `u32::MAX` |
//! | [`SmallFrameVec`] / [`SmallUnmanagedVec`] | `u16` | **12 B** (align 4) | **12 B** | `u16::MAX` |
//! | [`LargeFrameVec`] / [`LargeUnmanagedVec`] | `usize` | **24 B** (align 8) | **24 B** | `isize::MAX / size_of::<T>()` |
//!
//! Overflow past `I::MAX` is always refused explicitly with [`AllocFailure::CapacityOverflow`],
//! never silently truncated.

use core::{
    alloc::{Allocator, Layout},
    any::type_name,
    borrow::{Borrow, BorrowMut},
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{DropGuard, size_of},
    ops::{Deref, DerefMut, Index, IndexMut},
    ptr::{self, NonNull},
    slice::{self, SliceIndex},
};

use crate::{
    brand::{Brand, BrandedAlloc, BrandedBump, DropWith},
    error::{AllocFailure, fail},
    spec::CloneSlice,
    vec::Refused,
};

mod private {
    pub trait Sealed {}
}

/// 4-byte-aligned non-null pointer used by `I = u16` ([`SmallFrameVec`], [`SmallUnmanagedVec`]).
///
/// Storing `NonNull<T>` at 4-byte alignment eliminates the 4 bytes of tail padding after two
/// `u16` fields, keeping `SmallFrameVec<'id, T>` at **12 bytes** so `(u32, SmallFrameVec<'id, T>)`
/// packs into **16 bytes** total while preserving pointer provenance and the `NonNull` niche.
#[repr(C, packed(4))]
pub struct Align4NonNull<T>(NonNull<T>);

impl<T> Copy for Align4NonNull<T> {}
impl<T> Clone for Align4NonNull<T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> private::Sealed for NonNull<T> {}
impl<T> private::Sealed for Align4NonNull<T> {}

/// Sealed pointer storage paired with a [`LenIdx`].
pub trait PtrRepr<T>: Copy + private::Sealed {
    /// Dangling aligned non-null pointer for empty / ZST buffers.
    const DANGLING: Self;

    fn from_non_null(ptr: NonNull<T>) -> Self;
    fn into_non_null(self) -> NonNull<T>;
}

impl<T> PtrRepr<T> for NonNull<T> {
    const DANGLING: Self = NonNull::dangling();

    #[inline(always)]
    fn from_non_null(ptr: NonNull<T>) -> Self {
        ptr
    }

    #[inline(always)]
    fn into_non_null(self) -> NonNull<T> {
        self
    }
}

impl<T> PtrRepr<T> for Align4NonNull<T> {
    const DANGLING: Self = Align4NonNull(NonNull::dangling());

    #[inline(always)]
    fn from_non_null(ptr: NonNull<T>) -> Self {
        Align4NonNull(ptr)
    }

    #[inline(always)]
    fn into_non_null(self) -> NonNull<T> {
        self.0
    }
}

/// Sealed length/capacity index width for [`FrameVec`] and [`UnmanagedVec`].
///
/// Implemented for [`u16`], [`u32`] (default), and [`usize`].
pub trait LenIdx: Copy + Eq + Ord + private::Sealed {
    /// Pointer representation matched to this index width's alignment.
    type Ptr<T>: PtrRepr<T>;
    /// Zero (`0`).
    const ZERO: Self;
    /// Maximum representable element count.
    const MAX: Self;
    /// Maximum representable element count as `usize`.
    const MAX_USIZE: usize;

    fn to_usize(self) -> usize;
    fn from_usize(value: usize) -> Option<Self>;
}

impl private::Sealed for u16 {}
impl LenIdx for u16 {
    type Ptr<T> = Align4NonNull<T>;
    const ZERO: Self = 0;
    const MAX: Self = u16::MAX;
    const MAX_USIZE: usize = u16::MAX as usize;

    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }

    #[inline(always)]
    fn from_usize(value: usize) -> Option<Self> {
        u16::try_from(value).ok()
    }
}

impl private::Sealed for u32 {}
impl LenIdx for u32 {
    type Ptr<T> = NonNull<T>;
    const ZERO: Self = 0;
    const MAX: Self = u32::MAX;
    const MAX_USIZE: usize = u32::MAX as usize;

    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }

    #[inline(always)]
    fn from_usize(value: usize) -> Option<Self> {
        u32::try_from(value).ok()
    }
}

impl private::Sealed for usize {}
impl LenIdx for usize {
    type Ptr<T> = NonNull<T>;
    const ZERO: Self = 0;
    const MAX: Self = usize::MAX;
    const MAX_USIZE: usize = usize::MAX;

    #[inline(always)]
    fn to_usize(self) -> usize {
        self
    }

    #[inline(always)]
    fn from_usize(value: usize) -> Option<Self> {
        Some(value)
    }
}

#[inline(always)]
const fn is_zst<T>() -> bool {
    size_of::<T>() == 0
}

/// Minimum non-zero initial capacity (matches [`RawBuf`](crate::RawBuf) / `std`).
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

/// Shared raw storage (`ptr`, `len`, `cap`) for [`FrameVec`] and [`UnmanagedVec`].
///
/// Has **no `Drop` impl** so [`UnmanagedVec`] stays purely `#[blaze::linear]` with zero drop glue,
/// while `_owns: PhantomData<T>` declares ownership of `T` for dropck on [`FrameVec`].
///
/// # Invariants
///
/// - For zero-sized `T`: `cap == I::MAX`, `ptr` is dangling and aligned, and nothing is ever
///   allocated.
/// - For non-ZST `T` with `cap == 0`: `len == 0`, `ptr` is dangling and aligned, and nothing is
///   allocated.
/// - Otherwise `ptr` points to a block allocated by the matching branded allocator/arena for
///   `Layout::array::<T>(cap.to_usize())`, `len <= cap`, and slots `0..len` hold initialized `T`s.
struct RawParts<T, I: LenIdx> {
    ptr: I::Ptr<T>,
    len: I,
    cap: I,
    _owns: PhantomData<T>,
}

impl<T, I: LenIdx> RawParts<T, I> {
    #[inline]
    const fn new() -> Self {
        Self {
            ptr: I::Ptr::<T>::DANGLING,
            len: I::ZERO,
            cap: if is_zst::<T>() { I::MAX } else { I::ZERO },
            _owns: PhantomData,
        }
    }

    #[inline(always)]
    fn ptr_non_null(&self) -> NonNull<T> {
        self.ptr.into_non_null()
    }

    #[inline(always)]
    fn as_ptr(&self) -> *const T {
        self.ptr_non_null().as_ptr()
    }

    #[inline(always)]
    fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr_non_null().as_ptr()
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.len.to_usize()
    }

    #[inline(always)]
    fn capacity(&self) -> usize {
        self.cap.to_usize()
    }

    #[inline(always)]
    fn needs_to_grow(&self, additional: usize) -> bool {
        additional > self.capacity().wrapping_sub(self.len())
    }

    /// Layout of the currently allocated block, if any.
    #[inline]
    fn current_layout(&self) -> Option<Layout> {
        let cap = self.capacity();
        if is_zst::<T>() || cap == 0 {
            return None;
        }
        // SAFETY: `cap` elements of `T` were allocated by this type, so `Layout::array::<T>(cap)`
        // already succeeded without overflow.
        Some(unsafe { Layout::array::<T>(cap).unwrap_unchecked() })
    }

    /// Computes `(new_cap, new_layout)` for amortized (`exact == false`) or exact growth.
    fn plan_grow(&self, additional: usize, exact: bool) -> Result<(I, Layout), AllocFailure> {
        let len = self.len();
        let overflow = AllocFailure::CapacityOverflow { what: type_name::<T>(), len, additional };
        let Some(required) = len.checked_add(additional) else {
            return Err(overflow);
        };
        if required > I::MAX_USIZE {
            return Err(overflow);
        }
        // ZSTs have `cap == I::MAX`; reaching `plan_grow` means `required > I::MAX_USIZE`, already
        // returned above.
        debug_assert!(!is_zst::<T>() && required > self.capacity());

        let target = if exact {
            required
        } else {
            let doubled = self.capacity().saturating_mul(2).min(I::MAX_USIZE);
            required.max(doubled).max(min_non_zero_cap::<T>()).min(I::MAX_USIZE)
        };
        let layout = match Layout::array::<T>(target) {
            Ok(layout) => layout,
            Err(_) if !exact && target > required => {
                // Doubling exceeded `isize::MAX` bytes; fall back to `required` before refusing.
                let Ok(layout) = Layout::array::<T>(required) else {
                    return Err(overflow);
                };
                let target_idx = I::from_usize(required).ok_or(overflow)?;
                return Ok((target_idx, layout));
            }
            Err(_) => return Err(overflow),
        };
        let target_idx = I::from_usize(target).ok_or(overflow)?;
        Ok((target_idx, layout))
    }

    #[inline]
    fn as_slice(&self) -> &[T] {
        // SAFETY: slots `0..self.len()` hold initialized `T` values (invariant), and `self.ptr` is
        // non-null and properly aligned for `T` even when empty or ZST.
        unsafe { slice::from_raw_parts(self.as_ptr(), self.len()) }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, with exclusive `&mut self` access.
        unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), self.len()) }
    }

    /// Writes `value` into the next slot assuming `self.len() < self.capacity()`.
    ///
    /// # Safety
    ///
    /// `self.len() < self.capacity()`, so `self.len() + 1` fits in `I` and slot `self.len()` is
    /// within the allocated block (or `T` is zero-sized).
    #[inline]
    unsafe fn push_unchecked(&mut self, value: T) {
        let len = self.len();
        debug_assert!(len < self.capacity());
        // SAFETY: caller guarantees `len < capacity`, so `ptr.add(len)` is valid for writes.
        unsafe { self.as_mut_ptr().add(len).write(value) };
        // SAFETY: `len < capacity <= I::MAX_USIZE`, so `len + 1 <= I::MAX_USIZE`.
        self.len = unsafe { I::from_usize(len + 1).unwrap_unchecked() };
    }

    #[inline]
    fn pop(&mut self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let new_len = len - 1;
        // SAFETY: `new_len < len <= I::MAX_USIZE`, so `new_len` fits in `I`.
        self.len = unsafe { I::from_usize(new_len).unwrap_unchecked() };
        // SAFETY: slot `new_len` held an initialized `T` and is now outside `0..self.len`.
        Some(unsafe { self.as_ptr().add(new_len).read() })
    }

    fn truncate(&mut self, len: usize) {
        let cur = self.len();
        if len >= cur {
            return;
        }
        // SAFETY: `len < cur <= I::MAX_USIZE`, so `len` fits in `I`.
        self.len = unsafe { I::from_usize(len).unwrap_unchecked() };
        let drop_count = cur - len;
        if core::mem::needs_drop::<T>() {
            // SAFETY: slots `len..cur` hold initialized `T` values; updating `self.len` first
            // ensures an unwinding destructor cannot observe or double-drop them.
            unsafe {
                let tail = self.as_mut_ptr().add(len);
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(tail, drop_count));
            }
        }
    }

    #[inline]
    fn swap_remove(&mut self, index: usize) -> T {
        let len = self.len();
        assert!(index < len, "swap_remove index (is {index}) should be < len (is {len})");
        let last = len - 1;
        let ptr = self.as_mut_ptr();
        // SAFETY: `index < len`, so `ptr.add(index)` holds an initialized `T`.
        let value = unsafe { ptr.add(index).read() };
        if index != last {
            // SAFETY: `index != last` and both are `< len`, so slots are distinct and valid;
            // `ptr.add(last)` moves bitwise into the vacated `index` slot.
            unsafe { ptr::copy_nonoverlapping(ptr.add(last), ptr.add(index), 1) };
        }
        // SAFETY: `last < len <= I::MAX_USIZE`, so `last` fits in `I`.
        self.len = unsafe { I::from_usize(last).unwrap_unchecked() };
        value
    }

    /// Appends clones of `other`, assuming `other.len() <= self.capacity() - self.len()`.
    ///
    /// # Safety
    ///
    /// The caller has already ensured `self.capacity() - self.len() >= other.len()`.
    unsafe fn extend_from_slice_unchecked(&mut self, other: &[T])
    where
        T: Clone,
    {
        if other.is_empty() {
            return;
        }
        let mut count = self.len();
        let dst = self.as_mut_ptr();
        let len_slot = &mut self.len;
        // Commit however many clones landed even if `T::clone` panics mid-slice.
        let mut guard = DropGuard::new(&mut count, |c| {
            // SAFETY: `*c <= initial_len + other.len() <= capacity <= I::MAX_USIZE`.
            *len_slot = unsafe { I::from_usize(*c).unwrap_unchecked() };
        });
        // SAFETY: caller reserved at least `other.len()` slots past `count`, and `other` cannot
        // alias `self` because `&mut self` is exclusive.
        unsafe { T::clone_to(other, dst, &mut guard) };
    }

    /// Fills `n` copies of `value` into an empty buffer with `capacity >= n`.
    ///
    /// # Safety
    ///
    /// `self.len() == 0` and `self.capacity() >= n`.
    unsafe fn fill_elem_unchecked(&mut self, value: T, n: usize)
    where
        T: Clone,
    {
        if n == 0 {
            return;
        }
        let mut written = 0usize;
        let dst = self.as_mut_ptr();
        let len_slot = &mut self.len;
        let mut guard = DropGuard::new(&mut written, |w| {
            // SAFETY: `*w <= n <= capacity <= I::MAX_USIZE`.
            *len_slot = unsafe { I::from_usize(*w).unwrap_unchecked() };
        });
        for _ in 0..(n - 1) {
            let cloned = value.clone();
            // SAFETY: `*guard < n - 1 < capacity`, so slot `*guard` is within the allocation.
            unsafe { dst.add(**guard).write(cloned) };
            **guard += 1;
        }
        // Move the original `value` into the final slot without an extra clone.
        // SAFETY: `*guard == n - 1 < capacity`.
        unsafe { dst.add(**guard).write(value) };
        **guard += 1;
    }
}

/// Branded unmanaged bump-arena vector (`16` bytes default, `12` bytes for `I = u16`, `24` bytes
/// for `I = usize`).
///
/// Allocates from a [`BrandedBump<'id, A>`] arena whose backing storage is reclaimed in O(1) on
/// reset or drop. Growth allocates a fresh slice and copies elements without deallocating the old
/// bump slice. Dropping a `FrameVec` runs element destructors only (elided for non-`Drop` types)
/// and performs **zero `deallocate` calls**.
pub struct FrameVec<'id, T, I: LenIdx = u32> {
    raw: RawParts<T, I>,
    _brand: Brand<'id>,
}

/// 12-byte (align 4) bump-arena vector (`u16` len/cap, up to `65,535` elements).
///
/// Packs with a `u32` tag into **16 bytes total**: `size_of::<(u32, SmallFrameVec<'_, T>)>() == 16`.
pub type SmallFrameVec<'id, T> = FrameVec<'id, T, u16>;

/// 24-byte bump-arena vector (`usize` len/cap) for rare > 4 GiB buffers.
pub type LargeFrameVec<'id, T> = FrameVec<'id, T, usize>;

// SAFETY: `FrameVec` uniquely owns its initialized `T` elements (`T: Send`) and holds no allocator
// reference; its `Drop` only drops `T` elements.
unsafe impl<T: Send, I: LenIdx> Send for FrameVec<'_, T, I> {}
// SAFETY: a shared `&FrameVec` exposes only `&[T]` (`T: Sync`).
unsafe impl<T: Sync, I: LenIdx> Sync for FrameVec<'_, T, I> {}

impl<'id, T, I: LenIdx> FrameVec<'id, T, I> {
    /// Creates an empty `FrameVec` with zero allocations (needs no arena handle).
    #[inline]
    pub const fn new() -> Self {
        Self { raw: RawParts::new(), _brand: Brand::PHANTOM }
    }

    /// Creates an empty `FrameVec` with room for at least `capacity` elements in `arena`.
    #[inline]
    pub fn try_with_capacity_in<A: Allocator>(
        capacity: usize,
        arena: &BrandedBump<'id, A>,
    ) -> Result<Self, AllocFailure> {
        let mut vec = Self::new();
        vec.try_reserve_exact(capacity, arena)?;
        Ok(vec)
    }

    /// Creates an empty `FrameVec` with room for at least `capacity` elements; panics with
    /// [`AllocFailure`] on overflow or OOM.
    #[inline]
    #[track_caller]
    pub fn with_capacity_in<A: Allocator>(capacity: usize, arena: &BrandedBump<'id, A>) -> Self {
        match Self::try_with_capacity_in(capacity, arena) {
            Ok(vec) => vec,
            Err(failure) => fail(failure),
        }
    }

    /// Creates a `FrameVec` containing `n` clones of `value` in `arena`.
    pub fn try_from_elem_in<A: Allocator>(
        value: T,
        n: usize,
        arena: &BrandedBump<'id, A>,
    ) -> Result<Self, AllocFailure>
    where
        T: Clone,
    {
        let mut vec = Self::try_with_capacity_in(n, arena)?;
        // SAFETY: `vec` is empty and has capacity >= `n`.
        unsafe { vec.raw.fill_elem_unchecked(value, n) };
        Ok(vec)
    }

    /// Creates a `FrameVec` containing `n` clones of `value` in `arena`; panics with
    /// [`AllocFailure`] on overflow or OOM.
    #[track_caller]
    pub fn from_elem_in<A: Allocator>(
        value: T,
        n: usize,
        arena: &BrandedBump<'id, A>,
    ) -> Self
    where
        T: Clone,
    {
        match Self::try_from_elem_in(value, n, arena) {
            Ok(vec) => vec,
            Err(failure) => fail(failure),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Length in the native index width `I`.
    #[inline]
    pub fn len_idx(&self) -> I {
        self.raw.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.len == I::ZERO
    }

    /// Capacity in elements (`I::MAX` for zero-sized `T`).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    /// Capacity in the native index width `I`.
    #[inline]
    pub fn cap_idx(&self) -> I {
        self.raw.cap
    }

    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.raw.as_ptr()
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.raw.as_mut_ptr()
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        self.raw.as_slice()
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.raw.as_mut_slice()
    }

    /// Reserves room for at least `additional` more elements in `arena` (amortized doubling).
    ///
    /// Bump-arena growth allocates a new larger slice, copies existing elements, and updates
    /// `ptr`/`cap` without calling `deallocate` on the old slice.
    #[inline]
    pub fn try_reserve<A: Allocator>(
        &mut self,
        additional: usize,
        arena: &BrandedBump<'id, A>,
    ) -> Result<(), AllocFailure> {
        if !self.raw.needs_to_grow(additional) {
            return Ok(());
        }
        self.try_grow_cold(additional, false, arena)
    }

    /// [`FrameVec::try_reserve`], panicking with [`AllocFailure`] on overflow or OOM.
    #[inline]
    #[track_caller]
    pub fn reserve<A: Allocator>(&mut self, additional: usize, arena: &BrandedBump<'id, A>) {
        if let Err(failure) = self.try_reserve(additional, arena) {
            fail(failure);
        }
    }

    /// Reserves room for at least `additional` more elements without exponential over-allocation.
    #[inline]
    pub fn try_reserve_exact<A: Allocator>(
        &mut self,
        additional: usize,
        arena: &BrandedBump<'id, A>,
    ) -> Result<(), AllocFailure> {
        if !self.raw.needs_to_grow(additional) {
            return Ok(());
        }
        self.try_grow_cold(additional, true, arena)
    }

    /// [`FrameVec::try_reserve_exact`], panicking with [`AllocFailure`] on overflow or OOM.
    #[inline]
    #[track_caller]
    pub fn reserve_exact<A: Allocator>(
        &mut self,
        additional: usize,
        arena: &BrandedBump<'id, A>,
    ) {
        if let Err(failure) = self.try_reserve_exact(additional, arena) {
            fail(failure);
        }
    }

    #[cold]
    #[inline(never)]
    fn try_grow_cold<A: Allocator>(
        &mut self,
        additional: usize,
        exact: bool,
        arena: &BrandedBump<'id, A>,
    ) -> Result<(), AllocFailure> {
        let (new_cap, new_layout) = self.raw.plan_grow(additional, exact)?;
        let refused = AllocFailure::Refused { what: type_name::<T>(), layout: new_layout };
        // Allocate a fresh slice from the bump arena; never call `deallocate` on the old slice.
        let block = arena.alloc().allocate(new_layout).map_err(|_| refused)?;
        let new_ptr: NonNull<T> = block.cast();
        let len = self.raw.len();
        if len > 0 {
            // SAFETY: `self.raw.as_ptr()` holds `len` initialized `T`s; `new_ptr` is a freshly
            // allocated non-overlapping block with room for `new_cap >= len` elements.
            unsafe { ptr::copy_nonoverlapping(self.raw.as_ptr(), new_ptr.as_ptr(), len) };
        }
        self.raw.ptr = I::Ptr::<T>::from_non_null(new_ptr);
        self.raw.cap = new_cap;
        Ok(())
    }

    /// Appends `value`, returning `Err(Refused { value, why })` if growing fails.
    #[blaze::cost(amortized O(1))]
    #[inline]
    pub fn try_push<A: Allocator>(
        &mut self,
        value: T,
        arena: &BrandedBump<'id, A>,
    ) -> Result<(), Refused<T>> {
        if self.raw.len == self.raw.cap {
            if let Err(why) = self.try_grow_cold(1, false, arena) {
                return Err(Refused { value, why });
            }
        }
        // SAFETY: `self.raw.len < self.raw.cap` now holds.
        unsafe { self.raw.push_unchecked(value) };
        Ok(())
    }

    /// Appends `value`; panics with [`AllocFailure`] on overflow or OOM.
    #[blaze::cost(amortized O(1))]
    #[inline]
    #[track_caller]
    pub fn push<A: Allocator>(&mut self, value: T, arena: &BrandedBump<'id, A>) {
        if self.raw.len == self.raw.cap {
            if let Err(failure) = self.try_grow_cold(1, false, arena) {
                fail(failure);
            }
        }
        // SAFETY: `self.raw.len < self.raw.cap` now holds.
        unsafe { self.raw.push_unchecked(value) };
    }

    /// Appends clones of `other` into `self`.
    pub fn try_extend_from_slice<A: Allocator>(
        &mut self,
        other: &[T],
        arena: &BrandedBump<'id, A>,
    ) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        self.try_reserve(other.len(), arena)?;
        // SAFETY: `try_reserve` ensured room for at least `other.len()` additional elements.
        unsafe { self.raw.extend_from_slice_unchecked(other) };
        Ok(())
    }

    /// Appends clones of `other` into `self`; panics with [`AllocFailure`] on overflow or OOM.
    #[track_caller]
    pub fn extend_from_slice<A: Allocator>(
        &mut self,
        other: &[T],
        arena: &BrandedBump<'id, A>,
    ) where
        T: Clone,
    {
        if let Err(failure) = self.try_extend_from_slice(other, arena) {
            fail(failure);
        }
    }

    /// Removes and returns the last element, or `None` if empty.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.raw.pop()
    }

    /// Drops all elements, resetting `len` to 0 while retaining arena capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.raw.truncate(0);
    }

    /// Shortens the vector to `len`, dropping excess elements in place.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.raw.truncate(len);
    }

    /// Removes the element at `index` in O(1) by swapping in the last element.
    #[inline]
    #[track_caller]
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.raw.swap_remove(index)
    }
}

impl<'id, T, I: LenIdx> Default for FrameVec<'id, T, I> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY (`may_dangle`): `Drop` drops only the owned `T` elements (which `RawParts::_owns:
// PhantomData<T>` reports to dropck) and never reads `'id` or any allocator.
unsafe impl<#[may_dangle] 'id, #[may_dangle] T, I: LenIdx> Drop for FrameVec<'id, T, I> {
    #[inline]
    fn drop(&mut self) {
        let len = self.raw.len();
        if len > 0 && core::mem::needs_drop::<T>() {
            self.raw.len = I::ZERO;
            // SAFETY: slots `0..len` hold initialized `T` values; zeroing `self.raw.len` first
            // prevents double-drop on unwind. Zero `deallocate` call: the bump arena reclaims the
            // backing slice wholesale.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.raw.as_mut_ptr(), len));
            }
        }
    }
}

impl<T, I: LenIdx> Deref for FrameVec<'_, T, I> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, I: LenIdx> DerefMut for FrameVec<'_, T, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: LenIdx> AsRef<[T]> for FrameVec<'_, T, I> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, I: LenIdx> AsMut<[T]> for FrameVec<'_, T, I> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: LenIdx> Borrow<[T]> for FrameVec<'_, T, I> {
    #[inline]
    fn borrow(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, I: LenIdx> BorrowMut<[T]> for FrameVec<'_, T, I> {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: LenIdx, Idx: SliceIndex<[T]>> Index<Idx> for FrameVec<'_, T, I> {
    type Output = Idx::Output;

    #[inline]
    fn index(&self, index: Idx) -> &Self::Output {
        Index::index(self.as_slice(), index)
    }
}

impl<T, I: LenIdx, Idx: SliceIndex<[T]>> IndexMut<Idx> for FrameVec<'_, T, I> {
    #[inline]
    fn index_mut(&mut self, index: Idx) -> &mut Self::Output {
        IndexMut::index_mut(self.as_mut_slice(), index)
    }
}

impl<'a, T, I: LenIdx> IntoIterator for &'a FrameVec<'_, T, I> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a, T, I: LenIdx> IntoIterator for &'a mut FrameVec<'_, T, I> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

impl<T: fmt::Debug, I: LenIdx> fmt::Debug for FrameVec<'_, T, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: PartialEq<U>, U, I: LenIdx, J: LenIdx> PartialEq<FrameVec<'_, U, J>>
    for FrameVec<'_, T, I>
{
    #[inline]
    fn eq(&self, other: &FrameVec<'_, U, J>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq<U>, U, I: LenIdx> PartialEq<[U]> for FrameVec<'_, T, I> {
    #[inline]
    fn eq(&self, other: &[U]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq<U>, U, I: LenIdx> PartialEq<&[U]> for FrameVec<'_, T, I> {
    #[inline]
    fn eq(&self, other: &&[U]) -> bool {
        self.as_slice() == *other
    }
}

impl<T: PartialEq<U>, U, I: LenIdx, const N: usize> PartialEq<[U; N]> for FrameVec<'_, T, I> {
    #[inline]
    fn eq(&self, other: &[U; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, I: LenIdx> Eq for FrameVec<'_, T, I> {}

impl<T: Hash, I: LenIdx> Hash for FrameVec<'_, T, I> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

/// Branded, linear, heap-backed unmanaged vector (`16` bytes default, `12` bytes for `I = u16`,
/// `24` bytes for `I = usize`).
///
/// Stores **no allocator** and has **no `Drop` impl**: it must be explicitly consumed via
/// [`UnmanagedVec::free`] or [`DropWith::drop_with`] with the matching `&BrandedAlloc<'id, A>`.
/// Under blaze's linear-type checker (`docs/v2/plan/design/drop-with.md`), teardown is inserted
/// automatically whenever a single live `&BrandedAlloc<'id, A>` binding is in scope.
#[blaze::linear]
pub struct UnmanagedVec<'id, T, I: LenIdx = u32> {
    raw: RawParts<T, I>,
    _brand: Brand<'id>,
}

/// 12-byte (align 4) linear unmanaged vector (`u16` len/cap, up to `65,535` elements).
///
/// Packs with a `u32` tag into **16 bytes total**:
/// `size_of::<(u32, SmallUnmanagedVec<'_, T>)>() == 16`.
pub type SmallUnmanagedVec<'id, T> = UnmanagedVec<'id, T, u16>;

/// 24-byte linear unmanaged vector (`usize` len/cap) for rare > 4 GiB buffers.
pub type LargeUnmanagedVec<'id, T> = UnmanagedVec<'id, T, usize>;

// SAFETY: `UnmanagedVec` uniquely owns its initialized `T` elements (`T: Send`) and backing
// buffer; any subsequent allocation or free still requires `&BrandedAlloc<'id, A>` for `'id`.
unsafe impl<T: Send, I: LenIdx> Send for UnmanagedVec<'_, T, I> {}
// SAFETY: a shared `&UnmanagedVec` exposes only `&[T]` (`T: Sync`).
unsafe impl<T: Sync, I: LenIdx> Sync for UnmanagedVec<'_, T, I> {}

impl<'id, T, I: LenIdx> UnmanagedVec<'id, T, I> {
    /// Creates an empty `UnmanagedVec` with zero allocations (needs no allocator handle).
    #[inline]
    pub const fn new() -> Self {
        Self { raw: RawParts::new(), _brand: Brand::PHANTOM }
    }

    /// Creates an empty `UnmanagedVec` with room for at least `capacity` elements in `alloc`.
    #[inline]
    pub fn try_with_capacity_in<A: Allocator>(
        capacity: usize,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<Self, AllocFailure> {
        let mut vec = Self::new();
        vec.try_reserve_exact(capacity, alloc)?;
        Ok(vec)
    }

    /// Creates an empty `UnmanagedVec` with room for at least `capacity` elements; panics with
    /// [`AllocFailure`] on overflow or OOM.
    #[inline]
    #[track_caller]
    pub fn with_capacity_in<A: Allocator>(capacity: usize, alloc: &BrandedAlloc<'id, A>) -> Self {
        match Self::try_with_capacity_in(capacity, alloc) {
            Ok(vec) => vec,
            Err(failure) => fail(failure),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Length in the native index width `I`.
    #[inline]
    pub fn len_idx(&self) -> I {
        self.raw.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.len == I::ZERO
    }

    /// Capacity in elements (`I::MAX` for zero-sized `T`).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    /// Capacity in the native index width `I`.
    #[inline]
    pub fn cap_idx(&self) -> I {
        self.raw.cap
    }

    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.raw.as_ptr()
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.raw.as_mut_ptr()
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        self.raw.as_slice()
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.raw.as_mut_slice()
    }

    /// Reserves room for at least `additional` more elements in `alloc` (amortized doubling).
    #[inline]
    pub fn try_reserve<A: Allocator>(
        &mut self,
        additional: usize,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), AllocFailure> {
        if !self.raw.needs_to_grow(additional) {
            return Ok(());
        }
        self.try_grow_cold(additional, false, alloc)
    }

    /// [`UnmanagedVec::try_reserve`], panicking with [`AllocFailure`] on overflow or OOM.
    #[inline]
    #[track_caller]
    pub fn reserve<A: Allocator>(&mut self, additional: usize, alloc: &BrandedAlloc<'id, A>) {
        if let Err(failure) = self.try_reserve(additional, alloc) {
            fail(failure);
        }
    }

    /// Reserves room for at least `additional` more elements without exponential over-allocation.
    #[inline]
    pub fn try_reserve_exact<A: Allocator>(
        &mut self,
        additional: usize,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), AllocFailure> {
        if !self.raw.needs_to_grow(additional) {
            return Ok(());
        }
        self.try_grow_cold(additional, true, alloc)
    }

    /// [`UnmanagedVec::try_reserve_exact`], panicking with [`AllocFailure`] on overflow or OOM.
    #[inline]
    #[track_caller]
    pub fn reserve_exact<A: Allocator>(
        &mut self,
        additional: usize,
        alloc: &BrandedAlloc<'id, A>,
    ) {
        if let Err(failure) = self.try_reserve_exact(additional, alloc) {
            fail(failure);
        }
    }

    #[cold]
    #[inline(never)]
    fn try_grow_cold<A: Allocator>(
        &mut self,
        additional: usize,
        exact: bool,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), AllocFailure> {
        let (new_cap, new_layout) = self.raw.plan_grow(additional, exact)?;
        let refused = AllocFailure::Refused { what: type_name::<T>(), layout: new_layout };
        let block = match self.raw.current_layout() {
            None => alloc.alloc().allocate(new_layout),
            Some(old) => {
                // SAFETY: `ptr` is a live block allocated by `alloc.alloc()` with `old`
                // (guaranteed by `'id` invariance and `RawParts` invariant), and `new_layout` has
                // strictly larger size with identical alignment.
                unsafe { alloc.alloc().grow(self.raw.ptr_non_null().cast(), old, new_layout) }
            }
        }
        .map_err(|_| refused)?;
        self.raw.ptr = I::Ptr::<T>::from_non_null(block.cast());
        self.raw.cap = new_cap;
        Ok(())
    }

    /// Shrinks capacity down to `max(min_capacity, self.len())`; frees the block if `0`.
    pub fn try_shrink_to<A: Allocator>(
        &mut self,
        min_capacity: usize,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), AllocFailure> {
        let Some(old) = self.raw.current_layout() else {
            return Ok(());
        };
        let target = min_capacity.max(self.raw.len());
        if target >= self.raw.capacity() {
            return Ok(());
        }
        if target == 0 {
            // SAFETY: `ptr` is a live block allocated by `alloc.alloc()` with `old`; immediately
            // replaced with dangling pointer and zero capacity.
            unsafe { alloc.alloc().deallocate(self.raw.ptr_non_null().cast(), old) };
            self.raw.ptr = I::Ptr::<T>::DANGLING;
            self.raw.cap = I::ZERO;
            return Ok(());
        }
        // SAFETY: `target < self.raw.capacity()`, whose array layout already succeeded.
        let new_layout = unsafe { Layout::array::<T>(target).unwrap_unchecked() };
        // SAFETY: `ptr` is a live block of `old` from `alloc.alloc()`, and `new_layout` is smaller
        // with identical alignment.
        let block = unsafe { alloc.alloc().shrink(self.raw.ptr_non_null().cast(), old, new_layout) }
            .map_err(|_| AllocFailure::Refused { what: type_name::<T>(), layout: new_layout })?;
        self.raw.ptr = I::Ptr::<T>::from_non_null(block.cast());
        // SAFETY: `target < self.raw.capacity() <= I::MAX_USIZE`.
        self.raw.cap = unsafe { I::from_usize(target).unwrap_unchecked() };
        Ok(())
    }

    /// [`UnmanagedVec::try_shrink_to`], panicking with [`AllocFailure`] on refusal.
    #[track_caller]
    pub fn shrink_to<A: Allocator>(&mut self, min_capacity: usize, alloc: &BrandedAlloc<'id, A>) {
        if let Err(failure) = self.try_shrink_to(min_capacity, alloc) {
            fail(failure);
        }
    }

    /// Shrinks capacity as close to `self.len()` as possible.
    #[inline]
    pub fn try_shrink_to_fit<A: Allocator>(
        &mut self,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), AllocFailure> {
        self.try_shrink_to(0, alloc)
    }

    /// [`UnmanagedVec::try_shrink_to_fit`], panicking with [`AllocFailure`] on refusal.
    #[inline]
    #[track_caller]
    pub fn shrink_to_fit<A: Allocator>(&mut self, alloc: &BrandedAlloc<'id, A>) {
        self.shrink_to(0, alloc);
    }

    /// Appends `value`, returning `Err(Refused { value, why })` if growing fails.
    #[blaze::cost(amortized O(1))]
    #[inline]
    pub fn try_push<A: Allocator>(
        &mut self,
        value: T,
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), Refused<T>> {
        if self.raw.len == self.raw.cap {
            if let Err(why) = self.try_grow_cold(1, false, alloc) {
                return Err(Refused { value, why });
            }
        }
        // SAFETY: `self.raw.len < self.raw.cap` now holds.
        unsafe { self.raw.push_unchecked(value) };
        Ok(())
    }

    /// Appends `value`; panics with [`AllocFailure`] on overflow or OOM.
    #[blaze::cost(amortized O(1))]
    #[inline]
    #[track_caller]
    pub fn push<A: Allocator>(&mut self, value: T, alloc: &BrandedAlloc<'id, A>) {
        if self.raw.len == self.raw.cap {
            if let Err(failure) = self.try_grow_cold(1, false, alloc) {
                fail(failure);
            }
        }
        // SAFETY: `self.raw.len < self.raw.cap` now holds.
        unsafe { self.raw.push_unchecked(value) };
    }

    /// Appends clones of `other` into `self`.
    pub fn try_extend_from_slice<A: Allocator>(
        &mut self,
        other: &[T],
        alloc: &BrandedAlloc<'id, A>,
    ) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        self.try_reserve(other.len(), alloc)?;
        // SAFETY: `try_reserve` ensured room for at least `other.len()` additional elements.
        unsafe { self.raw.extend_from_slice_unchecked(other) };
        Ok(())
    }

    /// Appends clones of `other` into `self`; panics with [`AllocFailure`] on overflow or OOM.
    #[track_caller]
    pub fn extend_from_slice<A: Allocator>(
        &mut self,
        other: &[T],
        alloc: &BrandedAlloc<'id, A>,
    ) where
        T: Clone,
    {
        if let Err(failure) = self.try_extend_from_slice(other, alloc) {
            fail(failure);
        }
    }

    /// Removes and returns the last element, or `None` if empty.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.raw.pop()
    }

    /// Drops all elements, resetting `len` to 0 while keeping the allocated buffer.
    #[inline]
    pub fn clear(&mut self) {
        self.raw.truncate(0);
    }

    /// Shortens the vector to `len`, dropping excess elements in place.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.raw.truncate(len);
    }

    /// Removes the element at `index` in O(1) by swapping in the last element.
    #[inline]
    #[track_caller]
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.raw.swap_remove(index)
    }

    /// Consumes the vector, dropping all initialized elements and freeing its backing block in
    /// `alloc` (even if an element's destructor unwinds).
    #[blaze::consumes]
    pub fn free<A: Allocator>(mut self, alloc: &BrandedAlloc<'id, A>) {
        let ptr = self.raw.ptr_non_null();
        let len = self.raw.len();
        let layout = self.raw.current_layout();
        self.raw.len = I::ZERO;
        self.raw.cap = if is_zst::<T>() { I::MAX } else { I::ZERO };
        self.raw.ptr = I::Ptr::<T>::DANGLING;

        // Free the backing block via DropGuard so a panic in an element's `Drop` never leaks it.
        let _dealloc = DropGuard::new(layout, |layout| {
            if let Some(layout) = layout {
                // SAFETY: `ptr` was allocated by `alloc.alloc()` with `layout` (guaranteed by
                // `'id` invariance and `RawParts` invariant), and `self` is consumed so this block
                // is freed at most once.
                unsafe { alloc.alloc().deallocate(ptr.cast(), layout) };
            }
        });

        if len > 0 && core::mem::needs_drop::<T>() {
            // SAFETY: slots `0..len` hold initialized `T` values owned by `self`.
            unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(ptr.as_ptr(), len)) };
        }
    }
}

impl<'a, 'id, T, I: LenIdx, A: Allocator> DropWith<&'a BrandedAlloc<'id, A>>
    for UnmanagedVec<'id, T, I>
{
    #[inline]
    #[blaze::consumes]
    fn drop_with(self, cx: &'a BrandedAlloc<'id, A>) {
        self.free(cx);
    }
}

impl<T, I: LenIdx> Deref for UnmanagedVec<'_, T, I> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, I: LenIdx> DerefMut for UnmanagedVec<'_, T, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: LenIdx> AsRef<[T]> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, I: LenIdx> AsMut<[T]> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: LenIdx> Borrow<[T]> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn borrow(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, I: LenIdx> BorrowMut<[T]> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T, I: LenIdx, Idx: SliceIndex<[T]>> Index<Idx> for UnmanagedVec<'_, T, I> {
    type Output = Idx::Output;

    #[inline]
    fn index(&self, index: Idx) -> &Self::Output {
        Index::index(self.as_slice(), index)
    }
}

impl<T, I: LenIdx, Idx: SliceIndex<[T]>> IndexMut<Idx> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn index_mut(&mut self, index: Idx) -> &mut Self::Output {
        IndexMut::index_mut(self.as_mut_slice(), index)
    }
}

impl<'a, T, I: LenIdx> IntoIterator for &'a UnmanagedVec<'_, T, I> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a, T, I: LenIdx> IntoIterator for &'a mut UnmanagedVec<'_, T, I> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

impl<T: fmt::Debug, I: LenIdx> fmt::Debug for UnmanagedVec<'_, T, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: PartialEq<U>, U, I: LenIdx, J: LenIdx> PartialEq<UnmanagedVec<'_, U, J>>
    for UnmanagedVec<'_, T, I>
{
    #[inline]
    fn eq(&self, other: &UnmanagedVec<'_, U, J>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq<U>, U, I: LenIdx> PartialEq<[U]> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn eq(&self, other: &[U]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq<U>, U, I: LenIdx> PartialEq<&[U]> for UnmanagedVec<'_, T, I> {
    #[inline]
    fn eq(&self, other: &&[U]) -> bool {
        self.as_slice() == *other
    }
}

impl<T: PartialEq<U>, U, I: LenIdx, const N: usize> PartialEq<[U; N]>
    for UnmanagedVec<'_, T, I>
{
    #[inline]
    fn eq(&self, other: &[U; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, I: LenIdx> Eq for UnmanagedVec<'_, T, I> {}

impl<T: Hash, I: LenIdx> Hash for UnmanagedVec<'_, T, I> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

// Compile-time hardware-floor layout proofs (64-bit targets).
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Brand<'_>>() == 0);
    // Default I = u32: strictly 16 bytes (2 ARM64 registers), Option niche = 16 bytes.
    assert!(size_of::<FrameVec<'_, u64>>() == 16);
    assert!(size_of::<Option<FrameVec<'_, u64>>>() == 16);
    assert!(size_of::<UnmanagedVec<'_, u64>>() == 16);
    assert!(size_of::<Option<UnmanagedVec<'_, u64>>>() == 16);
    // Small I = u16: strictly 12 bytes (align 4), packs with u32 tag into 16 bytes total!
    assert!(size_of::<SmallFrameVec<'_, u64>>() == 12);
    assert!(size_of::<Option<SmallFrameVec<'_, u64>>>() == 12);
    assert!(size_of::<SmallUnmanagedVec<'_, u64>>() == 12);
    assert!(size_of::<Option<SmallUnmanagedVec<'_, u64>>>() == 12);
    assert!(size_of::<(u32, SmallFrameVec<'_, u64>)>() == 16);
    assert!(size_of::<(u32, SmallUnmanagedVec<'_, u64>)>() == 16);
    // Large I = usize: 24 bytes, Option niche = 24 bytes.
    assert!(size_of::<LargeFrameVec<'_, u64>>() == 24);
    assert!(size_of::<Option<LargeFrameVec<'_, u64>>>() == 24);
    assert!(size_of::<LargeUnmanagedVec<'_, u64>>() == 24);
    assert!(size_of::<Option<LargeUnmanagedVec<'_, u64>>>() == 24);
};

#[cfg(test)]
mod tests {
    use core::{
        alloc::{AllocError, Allocator, Layout},
        cell::Cell,
        mem::size_of,
        ptr::NonNull,
    };

    use super::*;
    use crate::{
        BumpAlloc, Inline,
        brand::{BrandedAlloc, BrandedBump, DropWith, with_brand},
        testing::{Counting, TestHeap, every_failure},
    };

    /// Bump allocator wrapper that panics if `deallocate` is ever called.
    struct NoDeallocBump<'a, const N: usize>(&'a Inline<N>);

    // SAFETY: delegates allocation to `&Inline<N>`, which is bulk-reclaimed on drop; `deallocate`
    // is forbidden and unreachable from `FrameVec`.
    unsafe impl<const N: usize> Allocator for NoDeallocBump<'_, N> {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            (&self.0).allocate(layout)
        }

        unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {
            panic!("FrameVec must never call deallocate on a BrandedBump arena");
        }
    }

    // SAFETY: backed by `&Inline<N>`, which reclaims all allocations in bulk when dropped.
    unsafe impl<const N: usize> BumpAlloc for NoDeallocBump<'_, N> {}

    /// Element that counts its own drops into a shared counter cell.
    struct DropCounter<'c> {
        id: u32,
        drops: &'c Cell<usize>,
    }

    impl Drop for DropCounter<'_> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn layout_sizes_and_niches() {
        assert_eq!(size_of::<Brand<'_>>(), 0);
        assert_eq!(size_of::<FrameVec<'_, u64>>(), 16);
        assert_eq!(size_of::<Option<FrameVec<'_, u64>>>(), 16);
        assert_eq!(size_of::<UnmanagedVec<'_, u64>>(), 16);
        assert_eq!(size_of::<Option<UnmanagedVec<'_, u64>>>(), 16);

        assert_eq!(size_of::<SmallFrameVec<'_, u64>>(), 12);
        assert_eq!(size_of::<SmallUnmanagedVec<'_, u64>>(), 12);
        assert_eq!(size_of::<(u32, SmallFrameVec<'_, u64>)>(), 16);
        assert_eq!(size_of::<(u32, SmallUnmanagedVec<'_, u64>)>(), 16);

        assert_eq!(size_of::<LargeFrameVec<'_, u64>>(), 24);
        assert_eq!(size_of::<Option<LargeFrameVec<'_, u64>>>(), 24);
        assert_eq!(size_of::<LargeUnmanagedVec<'_, u64>>(), 24);
        assert_eq!(size_of::<Option<LargeUnmanagedVec<'_, u64>>>(), 24);
    }

    #[test]
    fn frame_vec_growth_and_ops_without_dealloc() {
        let inline = Inline::<4096>::new();
        with_brand(|brand| {
            let arena = BrandedBump::new(NoDeallocBump(&inline), brand);
            let mut v = FrameVec::<u64>::new();
            assert!(v.is_empty());
            assert_eq!(v.len(), 0);
            assert_eq!(v.capacity(), 0);

            for i in 0..64u64 {
                v.push(i * 10, &arena);
            }
            assert_eq!(v.len(), 64);
            assert!(v.capacity() >= 64);
            assert_eq!(v[0], 0);
            assert_eq!(v[63], 630);

            // Mutate via DerefMut / IntoIterator.
            for slot in &mut v {
                *slot += 1;
            }
            let sum: u64 = (&v).into_iter().copied().sum();
            assert_eq!(sum, (0..64u64).map(|i| i * 10 + 1).sum::<u64>());

            // swap_remove, pop, truncate, clear.
            assert_eq!(v.swap_remove(0), 1);
            assert_eq!(v[0], 631);
            assert_eq!(v.len(), 63);
            assert_eq!(v.pop(), Some(621));
            assert_eq!(v.len(), 62);
            v.truncate(10);
            assert_eq!(v.len(), 10);
            v.extend_from_slice(&[7, 8, 9], &arena);
            assert_eq!(&v[10..], &[7, 8, 9]);
            v.clear();
            assert!(v.is_empty());

            // with_capacity_in and from_elem_in.
            let pre = FrameVec::<u32>::with_capacity_in(16, &arena);
            assert_eq!(pre.capacity(), 16);
            assert!(pre.is_empty());

            let repeated = FrameVec::<_>::from_elem_in(42u32, 5, &arena);
            assert_eq!(repeated, [42, 42, 42, 42, 42]);
        });
    }

    #[test]
    fn frame_vec_drop_counter_exact() {
        let inline = Inline::<4096>::new();
        let drops = Cell::new(0usize);

        with_brand(|brand| {
            let arena = BrandedBump::new(NoDeallocBump(&inline), brand);
            let mut v = FrameVec::<_>::new();
            for id in 0..20u32 {
                v.push(DropCounter { id, drops: &drops }, &arena);
            }
            assert_eq!(drops.get(), 0);

            // pop moves out without dropping until we drop the popped value.
            let popped = v.pop().unwrap();
            assert_eq!(popped.id, 19);
            assert_eq!(drops.get(), 0);
            drop(popped);
            assert_eq!(drops.get(), 1);

            // swap_remove moves out one element.
            let swapped = v.swap_remove(3);
            assert_eq!(swapped.id, 3);
            assert_eq!(drops.get(), 1);
            drop(swapped);
            assert_eq!(drops.get(), 2);

            // truncate drops 18 - 10 = 8 elements.
            v.truncate(10);
            assert_eq!(drops.get(), 10);

            // clear drops 10 elements.
            v.clear();
            assert_eq!(drops.get(), 20);

            // Re-populate 7 more and let FrameVec::drop run.
            for id in 100..107u32 {
                v.push(DropCounter { id, drops: &drops }, &arena);
            }
            assert_eq!(drops.get(), 20);
        });
        assert_eq!(drops.get(), 27);
    }

    #[test]
    fn frame_vec_zst_and_small_index_overflow() {
        let inline = Inline::<64>::new();
        with_brand(|brand| {
            let arena = BrandedBump::new(NoDeallocBump(&inline), brand);

            // ZST never allocates even on a tiny arena.
            let mut zst = FrameVec::<()>::new();
            assert_eq!(zst.capacity(), u32::MAX as usize);
            for _ in 0..1000 {
                zst.push((), &arena);
            }
            assert_eq!(zst.len(), 1000);
            assert_eq!(inline.used(), 0);
            assert_eq!(zst.pop(), Some(()));
            zst.truncate(10);
            assert_eq!(zst.len(), 10);

            // SmallFrameVec (u16): overflow past u16::MAX is refused cleanly, never truncated.
            let err = SmallFrameVec::<u32>::try_with_capacity_in(u16::MAX as usize + 1, &arena)
                .unwrap_err();
            assert!(matches!(err, AllocFailure::CapacityOverflow { .. }));

            let mut small_zst = SmallFrameVec::<()>::new();
            assert_eq!(small_zst.capacity(), u16::MAX as usize);
            small_zst.extend_from_slice(&[(); 16], &arena);
            assert_eq!(small_zst.len(), 16);
            let err = small_zst.try_reserve(u16::MAX as usize, &arena).unwrap_err();
            assert!(matches!(err, AllocFailure::CapacityOverflow { .. }));
            assert_eq!(small_zst.len(), 16);
        });
    }

    #[test]
    fn frame_vec_oom_preserves_contents() {
        // 32 bytes fits one initial slice of 4 u64s (32 B), then refuses growth to 8 u64s (64 B).
        let inline = Inline::<32>::new();
        with_brand(|brand| {
            let arena = BrandedBump::new(&inline, brand);
            let mut v = FrameVec::<u64>::new();
            for i in 0..4u64 {
                v.try_push(i, &arena).expect("fits in initial 32-byte slice");
            }
            let refused = v.try_push(99, &arena).unwrap_err();
            assert_eq!(refused.value, 99);
            assert!(matches!(refused.why, AllocFailure::Refused { .. }));
            assert_eq!(v.as_slice(), &[0, 1, 2, 3]);
        });
    }

    #[test]
    fn unmanaged_vec_counting_alloc_and_drop_with_composites() {
        let counting = Counting::new(TestHeap);
        let drops = Cell::new(0usize);

        with_brand(|brand| {
            let alloc = BrandedAlloc::new(&counting, brand);

            // Single UnmanagedVec with growth, shrink, slice ops, and drop_with.
            let mut v = UnmanagedVec::<u64>::with_capacity_in(4, &alloc);
            for i in 0..50u64 {
                v.push(i, &alloc);
            }
            v.extend_from_slice(&[100, 101, 102], &alloc);
            assert_eq!(v.len(), 53);
            assert_eq!(v.swap_remove(0), 0);
            assert_eq!(v[0], 102);
            v.truncate(10);
            v.shrink_to_fit(&alloc);
            assert_eq!(v.capacity(), 10);
            v.drop_with(&alloc);
            assert_eq!(counting.live(), 0);
            assert_eq!(counting.live_bytes(), 0);

            // Option<UnmanagedVec>: Some and None via DropWith.
            let mut some_v = UnmanagedVec::new();
            some_v.push(DropCounter { id: 1, drops: &drops }, &alloc);
            some_v.push(DropCounter { id: 2, drops: &drops }, &alloc);
            let opt_some: Option<UnmanagedVec<'_, DropCounter<'_>>> = Some(some_v);
            let opt_none: Option<UnmanagedVec<'_, DropCounter<'_>>> = None;
            opt_some.drop_with(&alloc);
            opt_none.drop_with(&alloc);
            assert_eq!(drops.get(), 2);
            assert_eq!(counting.live(), 0);

            // Result<UnmanagedVec, &str>: Ok and Err via DropWith.
            let mut ok_v = UnmanagedVec::new();
            ok_v.push(7u32, &alloc);
            let res_ok: Result<UnmanagedVec<'_, u32>, &str> = Ok(ok_v);
            let res_err: Result<UnmanagedVec<'_, u32>, &str> = Err("nope");
            res_ok.drop_with(&alloc);
            res_err.drop_with(&alloc);
            assert_eq!(counting.live(), 0);

            // Tuple (UnmanagedVec, UnmanagedVec) via DropWith where C: Copy.
            let mut a = UnmanagedVec::<_>::new();
            let mut b = UnmanagedVec::<_>::new();
            for i in 0..12u32 {
                a.push(i, &alloc);
                b.push(i * 2, &alloc);
            }
            (a, b).drop_with(&alloc);
            assert_eq!(counting.live(), 0);

            // SmallUnmanagedVec (12 B) + LargeUnmanagedVec (24 B) + ZST.
            let mut small = SmallUnmanagedVec::<u32>::new();
            small.extend_from_slice(&[1, 2, 3, 4], &alloc);
            assert_eq!(small.as_slice(), &[1, 2, 3, 4]);
            let overflow = small.try_reserve(u16::MAX as usize, &alloc).unwrap_err();
            assert!(matches!(overflow, AllocFailure::CapacityOverflow { .. }));
            small.drop_with(&alloc);

            let mut large = LargeUnmanagedVec::<u8>::new();
            large.extend_from_slice(b"blaze", &alloc);
            assert_eq!(large.as_slice(), b"blaze");
            large.drop_with(&alloc);

            let mut zst = UnmanagedVec::<()>::new();
            zst.push((), &alloc);
            zst.push((), &alloc);
            assert_eq!(zst.len(), 2);
            zst.drop_with(&alloc);
        });

        counting.assert_empty();
    }

    #[test]
    fn unmanaged_vec_every_failure_preserves_contents_and_zero_leaks() {
        let events = every_failure(&TestHeap, |alloc| -> Result<(), AllocFailure> {
            with_brand(|brand| {
                let branded = BrandedAlloc::new(alloc, brand);
                let mut v = UnmanagedVec::<u64>::new();
                // Ensure `v` is always freed even on `?` error return.
                let res = (|| -> Result<(), AllocFailure> {
                    for i in 0..24u64 {
                        if let Err(refused) = v.try_push(i, &branded) {
                            // Contents prior to refusal must stay completely intact.
                            assert_eq!(v.len(), i as usize);
                            for j in 0..i {
                                assert_eq!(v[j as usize], j);
                            }
                            return Err(refused.why);
                        }
                    }
                    v.try_extend_from_slice(&[100, 101, 102, 103], &branded)?;
                    Ok(())
                })();
                v.drop_with(&branded);
                res
            })
        })
        .expect("every allocation refusal must fail cleanly with zero leaks");
        assert!(events >= 3, "expected multiple allocations/growths, saw {events}");
    }

    #[test]
    fn unmanaged_vec_free_deallocates_on_element_drop_panic() {
        struct PanicOnDrop(bool);
        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                if self.0 {
                    panic!("intentional drop panic");
                }
            }
        }

        let counting = Counting::new(TestHeap);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_brand(|brand| {
                let alloc = BrandedAlloc::new(&counting, brand);
                let mut v = UnmanagedVec::<_>::new();
                v.push(PanicOnDrop(false), &alloc);
                v.push(PanicOnDrop(true), &alloc);
                v.push(PanicOnDrop(false), &alloc);
                v.free(&alloc);
            });
        }));
        assert!(result.is_err());
        counting.assert_empty();
    }
}
