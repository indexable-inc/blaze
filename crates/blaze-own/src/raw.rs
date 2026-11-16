//! [`RawBuf`]: room for `capacity` values of `T` in an allocator, none of them initialized. The
//! one place a buffer of `T` is allocated, grown, shrunk and freed; [`Vec`](crate::Vec) and
//! [`VecDeque`](crate::VecDeque) keep which slots hold values.

use core::{
    alloc::{Allocator, Layout},
    any::type_name,
    marker::PhantomData,
    mem::{ManuallyDrop, size_of},
    ptr::{self, NonNull},
};

use crate::error::{AllocFailure, fail};

/// Room for `capacity` values of `T` in `A`, uninitialized.
///
/// # Invariants
///
/// - `T` sized 0: `cap` is `usize::MAX` and nothing is ever allocated; `ptr` is dangling,
///   aligned.
/// - `cap` 0: `ptr` is dangling, aligned, and nothing is allocated.
/// - Otherwise `ptr` is a block `alloc` returned for `Layout::array::<T>(cap)` (at least that
///   size; the excess is not used), which this value frees when dropped.
///
/// Dropping it frees the block and drops no `T`: which slots hold values is the owner's to know.
pub struct RawBuf<T, A: Allocator> {
    ptr: NonNull<T>,
    cap: usize,
    alloc: A,
    /// Owns `T`s, for drop check (the owner's `#[may_dangle]` relies on it).
    _owns: PhantomData<T>,
}

// SAFETY: the buffer is uniquely owned, as a `Vec`'s is: sending it sends the values the owner
// keeps in it (`T: Send`) and the allocator that frees it (`A: Send`).
unsafe impl<T: Send, A: Allocator + Send> Send for RawBuf<T, A> {}
// SAFETY: a shared `&RawBuf` hands out only `*mut T` that the owner turns into `&T` for shared
// access (`T: Sync`) and `&A` (`A: Sync`).
unsafe impl<T: Sync, A: Allocator + Sync> Sync for RawBuf<T, A> {}

const fn is_zst<T>() -> bool {
    size_of::<T>() == 0
}

/// The least capacity a buffer grows to first: std's choice, few reallocations for small
/// buffers without wasting memory on large elements.
const fn min_non_zero_cap<T>() -> usize {
    if size_of::<T>() == 1 {
        8
    } else if size_of::<T>() <= 1024 {
        4
    } else {
        1
    }
}

impl<T, A: Allocator> RawBuf<T, A> {
    /// No room; allocates nothing.
    #[inline]
    pub const fn new_in(alloc: A) -> RawBuf<T, A> {
        RawBuf {
            ptr: NonNull::dangling(),
            cap: if is_zst::<T>() { usize::MAX } else { 0 },
            alloc,
            _owns: PhantomData,
        }
    }

    /// Room for at least `capacity` values.
    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<RawBuf<T, A>, AllocFailure> {
        let mut buf = RawBuf::new_in(alloc);
        buf.try_reserve_exact(0, capacity)?;
        Ok(buf)
    }

    /// Room for at least `capacity` values; panics with the [`AllocFailure`] when there is none.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> RawBuf<T, A> {
        match RawBuf::try_with_capacity_in(capacity, alloc) {
            Ok(buf) => buf,
            Err(failure) => fail(failure),
        }
    }

    /// Takes over a buffer from its parts.
    ///
    /// # Safety
    ///
    /// `ptr` and `cap` are as [`RawBuf::into_raw_parts`] gave them for a `RawBuf<T, A>` whose
    /// allocator `alloc` is (or is equivalent to), or `cap` is 0 and `ptr` dangling and aligned.
    /// For a `T` sized 0 any `cap` is taken as `usize::MAX`.
    #[inline]
    pub unsafe fn from_raw_parts_in(ptr: NonNull<T>, cap: usize, alloc: A) -> RawBuf<T, A> {
        let cap = if is_zst::<T>() { usize::MAX } else { cap };
        RawBuf { ptr, cap, alloc, _owns: PhantomData }
    }

    /// Its pointer, capacity and allocator; nothing is freed.
    #[inline]
    pub fn into_raw_parts(self) -> (NonNull<T>, usize, A) {
        let me = ManuallyDrop::new(self);
        // SAFETY: `me` is never used or dropped again, so the allocator is moved out once.
        let alloc = unsafe { ptr::read(&me.alloc) };
        (me.ptr, me.cap, alloc)
    }

    /// The first slot; dangling when nothing is allocated.
    #[inline]
    pub const fn ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    #[inline]
    pub const fn non_null(&self) -> NonNull<T> {
        self.ptr
    }

    /// The slots it has; `usize::MAX` for a `T` sized 0.
    #[inline]
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    #[inline]
    pub const fn allocator(&self) -> &A {
        &self.alloc
    }

    /// The layout of the block it holds, when it holds one.
    fn current(&self) -> Option<Layout> {
        if is_zst::<T>() || self.cap == 0 {
            return None;
        }
        // SAFETY: `cap` values of `T` were allocated (the invariant), so their array layout
        // was computed without overflow when the block was made.
        Some(unsafe { Layout::array::<T>(self.cap).unwrap_unchecked() })
    }

    /// Whether `additional` more values fit after `len`.
    #[inline]
    pub fn needs_to_grow(&self, len: usize, additional: usize) -> bool {
        additional > self.cap.wrapping_sub(len)
    }

    /// Room for `additional` more values after `len`, growing to at least twice the capacity,
    /// so pushes cost amortized constant time.
    #[inline]
    pub fn try_reserve(&mut self, len: usize, additional: usize) -> Result<(), AllocFailure> {
        if !self.needs_to_grow(len, additional) {
            return Ok(());
        }
        let overflow =
            AllocFailure::CapacityOverflow { what: type_name::<T>(), len, additional };
        let Some(required) = len.checked_add(additional) else { return Err(overflow) };
        let cap = required.max(self.cap.saturating_mul(2)).max(min_non_zero_cap::<T>());
        self.grow_to(cap, overflow)
    }

    /// [`RawBuf::try_reserve`], panicking with the [`AllocFailure`].
    #[inline]
    pub fn reserve(&mut self, len: usize, additional: usize) {
        if self.needs_to_grow(len, additional) {
            self.reserve_cold(len, additional);
        }
    }

    #[cold]
    #[inline(never)]
    fn reserve_cold(&mut self, len: usize, additional: usize) {
        if let Err(failure) = self.try_reserve(len, additional) {
            fail(failure);
        }
    }

    /// Room for exactly `additional` more values after `len` (the allocator may give more).
    pub fn try_reserve_exact(&mut self, len: usize, additional: usize) -> Result<(), AllocFailure> {
        if !self.needs_to_grow(len, additional) {
            return Ok(());
        }
        let overflow =
            AllocFailure::CapacityOverflow { what: type_name::<T>(), len, additional };
        let Some(required) = len.checked_add(additional) else { return Err(overflow) };
        self.grow_to(required, overflow)
    }

    /// Moves the buffer to a block of `cap` values; `overflow` is the failure when that does
    /// not fit `isize::MAX` bytes. Only called when growing, so never for a `T` sized 0 (its
    /// capacity is `usize::MAX`, which no request exceeds).
    fn grow_to(&mut self, cap: usize, overflow: AllocFailure) -> Result<(), AllocFailure> {
        debug_assert!(!is_zst::<T>() && cap > self.cap);
        let Ok(layout) = Layout::array::<T>(cap) else { return Err(overflow) };
        let refused = AllocFailure::Refused { what: type_name::<T>(), layout };
        let block = match self.current() {
            None => self.alloc.allocate(layout),
            // SAFETY: `ptr` is a block of `old` from this allocator (the invariant), and
            // `layout`'s size is larger with the same alignment.
            Some(old) => unsafe { self.alloc.grow(self.ptr.cast(), old, layout) },
        };
        let block = block.map_err(|_| refused)?;
        self.ptr = block.cast();
        self.cap = cap;
        Ok(())
    }

    /// Shrinks the buffer to `cap` values (no change when it is not smaller); 0 frees it.
    pub fn try_shrink_to(&mut self, cap: usize) -> Result<(), AllocFailure> {
        let Some(old) = self.current() else { return Ok(()) };
        if cap >= self.cap {
            return Ok(());
        }
        if cap == 0 {
            // SAFETY: `ptr` is a block of `old` from this allocator (the invariant); it is
            // forgotten at once.
            unsafe { self.alloc.deallocate(self.ptr.cast(), old) };
            self.ptr = NonNull::dangling();
            self.cap = 0;
            return Ok(());
        }
        // SAFETY: `cap < self.cap`, whose array layout fit.
        let layout = unsafe { Layout::array::<T>(cap).unwrap_unchecked() };
        // SAFETY: `ptr` is a block of `old` from this allocator, and `layout` is smaller with the
        // same alignment.
        let block = unsafe { self.alloc.shrink(self.ptr.cast(), old, layout) }
            .map_err(|_| AllocFailure::Refused { what: type_name::<T>(), layout })?;
        self.ptr = block.cast();
        self.cap = cap;
        Ok(())
    }

    /// [`RawBuf::try_shrink_to`], panicking with the [`AllocFailure`].
    pub fn shrink_to(&mut self, cap: usize) {
        if let Err(failure) = self.try_shrink_to(cap) {
            fail(failure);
        }
    }
}

// SAFETY (`may_dangle`): the drop frees the block and reads no `T`.
unsafe impl<#[may_dangle] T, A: Allocator> Drop for RawBuf<T, A> {
    fn drop(&mut self) {
        if let Some(layout) = self.current() {
            // SAFETY: `ptr` is a block of `layout` from this allocator (the invariant), freed
            // once: this is the buffer's drop. No `T` is touched, which `may_dangle` needs.
            unsafe { self.alloc.deallocate(self.ptr.cast(), layout) };
        }
    }
}
