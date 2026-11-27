//! Compressed 32-bit pointer tier over a base-anchored branded arena:
//!
//! - [`ArenaBase<'id, A>`]: contiguous 64-byte-aligned bump slab anchored at a single `base`
//!   pointer, addressing up to **32 GiB** in 8-byte slots (`offset << 3`, one ARM64 `add`
//!   instruction). Reclaims the entire slab in one `deallocate` on drop.
//! - [`Ref32<'id, T>`]: **4-byte** typed compressed pointer (`Option<Ref32<'_, T>>` is also
//!   **4 bytes** via the `NonZeroU32` niche), invariant in `'id` so a reference can never be
//!   dereferenced against the wrong arena.
//! - [`ArenaVec<'id, T, I = u16>`]: **8-byte** compressed vector for default `I = u16` (fits in
//!   one 64-bit register, **4× smaller than `AVec`**; **12 bytes** for `I = u32`).

use core::{
    alloc::{Allocator, Layout},
    any::type_name,
    cell::Cell,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{DropGuard, align_of, size_of},
    num::NonZeroU32,
    ptr::{self, NonNull},
    slice,
};

use crate::{
    brand::{Brand, DropWith},
    error::{AllocFailure, fail},
    frame_vec::LenIdx,
    spec::CloneSlice,
    vec::Refused,
};

/// Slot shift for compressed 32-bit offsets: 8-byte slots address up to 32 GiB (`u32::MAX << 3`).
const SLOT_SHIFT: u32 = 3;
/// Minimum slot alignment in bytes (`1 << SLOT_SHIFT`).
const SLOT_ALIGN: usize = 1 << SLOT_SHIFT;
/// Backing slab alignment: 64 bytes (one cache line), supporting all types with `align <= 64`.
pub const SLAB_ALIGN: usize = 64;
/// Maximum addressable slab size in bytes (32 GiB - 8 B on 64-bit; `isize::MAX` on 32-bit).
pub const MAX_ARENA_BYTES: usize = {
    let max_by_u32 = (u32::MAX as usize).saturating_mul(SLOT_ALIGN);
    let max_isize = isize::MAX as usize;
    if max_by_u32 < max_isize { max_by_u32 } else { max_isize }
};

#[inline(always)]
const fn is_zst<T>() -> bool {
    size_of::<T>() == 0
}

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

/// Base-anchored contiguous bump arena paired with an invariant [`Brand<'id>`].
///
/// Reserves the first 8 bytes (slot 0) so every real allocation has a non-zero 8-byte slot index
/// (`NonZeroU32`), decoding in a single shift-add (`base + ((off as usize) << 3)`).
///
/// There is intentionally no `reset(&mut self)` reusing `'id`, because previously handed-out
/// [`Ref32<'id, T>`] handles would otherwise become dangling or type-confused. To reclaim memory,
/// drop `ArenaBase` and enter a fresh [`with_brand`](crate::with_brand) scope.
pub struct ArenaBase<'id, A: Allocator> {
    base: NonNull<u8>,
    /// Total allocated slab bytes (including the 8-byte reserved slot 0), or `0` if empty.
    cap: usize,
    /// Next free byte offset inside `[0..cap]`. Starts at `SLOT_ALIGN` when `cap > 0`.
    top: Cell<usize>,
    alloc: A,
    brand: Brand<'id>,
}

impl<'id, A: Allocator> ArenaBase<'id, A> {
    /// Allocates a base-anchored slab with at least `capacity_bytes` of usable storage.
    pub fn try_with_capacity_in(
        capacity_bytes: usize,
        alloc: A,
        brand: Brand<'id>,
    ) -> Result<Self, AllocFailure> {
        if capacity_bytes == 0 {
            return Ok(Self {
                base: NonNull::dangling(),
                cap: 0,
                top: Cell::new(0),
                alloc,
                brand,
            });
        }
        let overflow = AllocFailure::CapacityOverflow {
            what: "ArenaBase",
            len: SLOT_ALIGN,
            additional: capacity_bytes,
        };
        let Some(with_guard) = capacity_bytes.checked_add(SLOT_ALIGN) else {
            return Err(overflow);
        };
        let Some(total) = with_guard.checked_next_multiple_of(SLOT_ALIGN) else {
            return Err(overflow);
        };
        if total > MAX_ARENA_BYTES {
            return Err(overflow);
        }
        let Ok(layout) = Layout::from_size_align(total, SLAB_ALIGN) else {
            return Err(overflow);
        };
        let block = alloc
            .allocate(layout)
            .map_err(|_| AllocFailure::Refused { what: "ArenaBase", layout })?;
        Ok(Self {
            base: block.cast(),
            cap: total,
            top: Cell::new(SLOT_ALIGN),
            alloc,
            brand,
        })
    }

    /// Allocates a base-anchored slab with at least `capacity_bytes` of usable storage; panics
    /// with [`AllocFailure`] on overflow or OOM.
    #[track_caller]
    pub fn with_capacity_in(capacity_bytes: usize, alloc: A, brand: Brand<'id>) -> Self {
        match Self::try_with_capacity_in(capacity_bytes, alloc, brand) {
            Ok(arena) => arena,
            Err(failure) => fail(failure),
        }
    }

    /// The invariant brand identifying this arena scope.
    #[inline]
    pub const fn brand(&self) -> Brand<'id> {
        self.brand
    }

    /// The underlying allocator owning the slab.
    #[inline]
    pub const fn alloc(&self) -> &A {
        &self.alloc
    }

    /// Usable bytes allocated so far (excluding the 8-byte reserved slot 0).
    #[inline]
    pub fn used_bytes(&self) -> usize {
        self.top.get().saturating_sub(SLOT_ALIGN)
    }

    /// Total usable byte capacity of this slab.
    #[inline]
    pub const fn capacity_bytes(&self) -> usize {
        self.cap.saturating_sub(SLOT_ALIGN)
    }

    /// Allocates raw bytes for `layout` inside the slab, returning its non-zero 8-byte slot
    /// offset (`byte_offset = (off.get() as usize) << 3`).
    fn alloc_slot(&self, layout: Layout, what: &'static str) -> Result<NonZeroU32, AllocFailure> {
        debug_assert!(layout.size() > 0);
        let refused = AllocFailure::Refused { what, layout };
        if layout.align() > SLAB_ALIGN {
            return Err(refused);
        }
        let align = layout.align().max(SLOT_ALIGN);
        let top = self.top.get();
        let Some(start) = top.checked_next_multiple_of(align) else {
            return Err(refused);
        };
        let Some(end) = start.checked_add(layout.size()) else {
            return Err(refused);
        };
        if end > self.cap {
            return Err(refused);
        }
        debug_assert!(start >= SLOT_ALIGN && (start & (SLOT_ALIGN - 1)) == 0);
        let slot = start >> SLOT_SHIFT;
        let Ok(slot_u32) = u32::try_from(slot) else {
            return Err(refused);
        };
        // SAFETY: `start >= SLOT_ALIGN` (`8`), so `slot = start >> 3 >= 1`.
        let off = unsafe { NonZeroU32::new_unchecked(slot_u32) };
        self.top.set(end);
        Ok(off)
    }

    /// Decodes a non-zero 8-byte slot offset into a raw pointer to `T` inside this slab.
    ///
    /// # Safety
    ///
    /// `off` must have been returned by [`ArenaBase::alloc_slot`] on this arena (guaranteed by
    /// `'id` invariance) with alignment >= `align_of::<T>()` and size >= the accessed `T` range.
    #[inline(always)]
    unsafe fn ptr_from_slot<T>(&self, off: NonZeroU32) -> NonNull<T> {
        let byte_off = (off.get() as usize) << SLOT_SHIFT;
        debug_assert!(byte_off >= SLOT_ALIGN && byte_off < self.cap);
        // SAFETY: `byte_off < self.cap`, so `self.base.add(byte_off)` is in-bounds of the slab
        // allocated by `self` and properly aligned for `T`.
        unsafe { self.base.add(byte_off).cast::<T>() }
    }

    /// Allocates `val` inside this arena and returns a 4-byte [`Ref32<'id, T>`] handle.
    ///
    /// Note: like a bump arena, values placed behind [`Ref32`] do not run `Drop` automatically
    /// when `ArenaBase` drops.
    pub fn alloc_ref32<T>(&self, val: T) -> Result<Ref32<'id, T>, AllocFailure> {
        if is_zst::<T>() {
            // ZST needs no slab bytes; `NonZeroU32::MIN` is a valid non-zero sentinel.
            core::mem::forget(val);
            return Ok(Ref32 {
                off: NonZeroU32::MIN,
                _brand: self.brand,
                _marker: PhantomData,
            });
        }
        let off = self.alloc_slot(Layout::new::<T>(), type_name::<T>())?;
        // SAFETY: `alloc_slot` returned a fresh, unaliased, `align_of::<T>()`-aligned slot of
        // `size_of::<T>()` bytes within `self`'s slab.
        unsafe { self.ptr_from_slot::<T>(off).as_ptr().write(val) };
        Ok(Ref32 { off, _brand: self.brand, _marker: PhantomData })
    }

    /// Dereferences a 4-byte [`Ref32<'id, T>`] to a shared reference (`base + (off << 3)`).
    #[blaze::cost(O(1))]
    #[inline(always)]
    pub fn get<T>(&self, r: Ref32<'id, T>) -> &T {
        if is_zst::<T>() {
            // SAFETY: any aligned non-null pointer is valid for reading a zero-sized `T`.
            return unsafe { NonNull::<T>::dangling().as_ref() };
        }
        // SAFETY: `r` carries this arena's invariant `'id`, so `r.off` was initialized by
        // `self.alloc_ref32::<T>`; `&self` prevents concurrent `get_mut`.
        unsafe { self.ptr_from_slot::<T>(r.off).as_ref() }
    }

    /// Dereferences a 4-byte [`Ref32<'id, T>`] to an exclusive mutable reference.
    ///
    /// Taking `&mut self` guarantees no other `get` or `get_mut` borrow on this arena is active,
    /// keeping `Ref32: Copy` 100% sound.
    #[blaze::cost(O(1))]
    #[inline(always)]
    pub fn get_mut<T>(&mut self, r: Ref32<'id, T>) -> &mut T {
        if is_zst::<T>() {
            // SAFETY: any aligned non-null pointer is valid for a zero-sized `T`, and `&mut self`
            // guarantees exclusive access.
            return unsafe { NonNull::<T>::dangling().as_mut() };
        }
        // SAFETY: `r` carries this arena's invariant `'id`, so `r.off` was initialized by
        // `self.alloc_ref32::<T>`; `&mut self` guarantees exclusive access across the arena.
        unsafe { self.ptr_from_slot::<T>(r.off).as_mut() }
    }
}

impl<A: Allocator> Drop for ArenaBase<'_, A> {
    fn drop(&mut self) {
        if self.cap > 0 {
            // SAFETY: `self.cap` succeeded `Layout::from_size_align(total, SLAB_ALIGN)` at
            // construction.
            let layout = unsafe { Layout::from_size_align_unchecked(self.cap, SLAB_ALIGN) };
            // SAFETY: `self.base` was allocated by `self.alloc` with `layout`, freed once here.
            unsafe { self.alloc.deallocate(self.base, layout) };
        }
    }
}

/// 4-byte compressed pointer into an [`ArenaBase<'id, A>`] (`Option<Ref32<'_, T>>` is also
/// 4 bytes via `NonZeroU32`).
///
/// Decodes in one shift-add (`base + ((off.get() as usize) << 3)`), addressing up to 32 GiB.
pub struct Ref32<'id, T> {
    off: NonZeroU32,
    _brand: Brand<'id>,
    _marker: PhantomData<T>,
}

impl<T> Copy for Ref32<'_, T> {}
impl<T> Clone for Ref32<'_, T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> PartialEq for Ref32<'_, T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.off == other.off
    }
}

impl<T> Eq for Ref32<'_, T> {}

impl<T> Hash for Ref32<'_, T> {
    #[inline(always)]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.off.hash(state);
    }
}

impl<T> fmt::Debug for Ref32<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ref32").field(&self.off.get()).finish()
    }
}

impl<'id, T> Ref32<'id, T> {
    /// Raw non-zero 8-byte slot offset relative to the arena base.
    #[inline(always)]
    pub const fn raw_slot(self) -> NonZeroU32 {
        self.off
    }
}

/// Compressed-offset bump vector backed by an [`ArenaBase<'id, A>`].
///
/// - Default `I = u16`: **8 bytes** (`off: u32, len: u16, cap: u16`) — fits in one 64-bit
///   register, **4× smaller than `AVec`**.
/// - `I = u32`: **12 bytes** (`off: u32, len: u32, cap: u32`).
/// - `I = usize`: **24 bytes**.
///
/// Does not store a pointer or allocator. For `Drop` element types, call [`ArenaVec::clear`] or
/// [`DropWith::drop_with`] with `&arena` to run element destructors before the arena drops.
pub struct ArenaVec<'id, T, I: LenIdx = u16> {
    /// `0` when unallocated or ZST; otherwise a non-zero 8-byte slot index in `ArenaBase<'id, _>`.
    off: u32,
    len: I,
    cap: I,
    _brand: Brand<'id>,
    _owns: PhantomData<T>,
}

// SAFETY: `ArenaVec` uniquely owns its initialized `T` elements (`T: Send`) and stores only an
// offset + lengths (dereferencing still requires `&ArenaBase<'id, A>`).
unsafe impl<T: Send, I: LenIdx> Send for ArenaVec<'_, T, I> {}
// SAFETY: a shared `&ArenaVec` exposes only `&[T]` (`T: Sync`).
unsafe impl<T: Sync, I: LenIdx> Sync for ArenaVec<'_, T, I> {}

impl<'id, T, I: LenIdx> ArenaVec<'id, T, I> {
    /// Creates an empty `ArenaVec` with zero allocations.
    #[inline]
    pub const fn new() -> Self {
        Self {
            off: 0,
            len: I::ZERO,
            cap: if is_zst::<T>() { I::MAX } else { I::ZERO },
            _brand: Brand::PHANTOM,
            _owns: PhantomData,
        }
    }

    /// Creates an empty `ArenaVec` with room for at least `capacity` elements in `arena`.
    #[inline]
    pub fn try_with_capacity_in<A: Allocator>(
        capacity: usize,
        arena: &ArenaBase<'id, A>,
    ) -> Result<Self, AllocFailure> {
        let mut vec = Self::new();
        vec.try_reserve_exact(arena, capacity)?;
        Ok(vec)
    }

    /// Creates an empty `ArenaVec` with room for at least `capacity` elements; panics with
    /// [`AllocFailure`] on overflow or arena exhaustion.
    #[inline]
    #[track_caller]
    pub fn with_capacity_in<A: Allocator>(capacity: usize, arena: &ArenaBase<'id, A>) -> Self {
        match Self::try_with_capacity_in(capacity, arena) {
            Ok(vec) => vec,
            Err(failure) => fail(failure),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len.to_usize()
    }

    #[inline]
    pub fn len_idx(&self) -> I {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == I::ZERO
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap.to_usize()
    }

    #[inline]
    pub fn cap_idx(&self) -> I {
        self.cap
    }

    /// Raw compressed 8-byte slot offset (`0` when unallocated or ZST).
    #[inline]
    pub const fn raw_offset(&self) -> u32 {
        self.off
    }

    #[inline(always)]
    fn data_ptr<A: Allocator>(&self, arena: &ArenaBase<'id, A>) -> NonNull<T> {
        match NonZeroU32::new(self.off) {
            None => NonNull::dangling(),
            Some(slot) => {
                // SAFETY: `self.off` was allocated by `arena.alloc_slot` for `Layout::array::<T>(cap)`
                // (guaranteed by `'id` invariance).
                unsafe { arena.ptr_from_slot::<T>(slot) }
            }
        }
    }

    /// Views the initialized elements as `&[T]`.
    #[inline]
    pub fn as_slice<'a, A: Allocator>(&'a self, arena: &'a ArenaBase<'id, A>) -> &'a [T] {
        // SAFETY: slots `0..self.len()` hold initialized `T`s inside `arena`; both `self` and
        // `arena` are borrowed for `'a`.
        unsafe { slice::from_raw_parts(self.data_ptr(arena).as_ptr(), self.len()) }
    }

    /// Views the initialized elements as `&mut [T]`.
    ///
    /// Borrowing `&'a mut self` prevents aliasing this vector's slice, while borrowing
    /// `&'a ArenaBase` keeps the slab live and still allows disjoint `ArenaVec`s in the same
    /// arena to be mutated independently.
    #[inline]
    pub fn as_mut_slice<'a, A: Allocator>(
        &'a mut self,
        arena: &'a ArenaBase<'id, A>,
    ) -> &'a mut [T] {
        // SAFETY: `self` uniquely owns its disjoint backing slice inside `arena`; `&'a mut self`
        // prevents any concurrent alias to this slice and `&'a ArenaBase` prevents `arena.get_mut`
        // or slab deallocation.
        unsafe { slice::from_raw_parts_mut(self.data_ptr(arena).as_ptr(), self.len()) }
    }

    #[inline(always)]
    fn needs_to_grow(&self, additional: usize) -> bool {
        additional > self.capacity().wrapping_sub(self.len())
    }

    /// Reserves room for at least `additional` more elements in `arena` (amortized doubling).
    #[inline]
    pub fn try_reserve<A: Allocator>(
        &mut self,
        arena: &ArenaBase<'id, A>,
        additional: usize,
    ) -> Result<(), AllocFailure> {
        if !self.needs_to_grow(additional) {
            return Ok(());
        }
        self.try_grow_cold(arena, additional, false)
    }

    /// [`ArenaVec::try_reserve`], panicking with [`AllocFailure`] on overflow or exhaustion.
    #[inline]
    #[track_caller]
    pub fn reserve<A: Allocator>(&mut self, arena: &ArenaBase<'id, A>, additional: usize) {
        if let Err(failure) = self.try_reserve(arena, additional) {
            fail(failure);
        }
    }

    /// Reserves room for at least `additional` more elements without exponential over-allocation.
    #[inline]
    pub fn try_reserve_exact<A: Allocator>(
        &mut self,
        arena: &ArenaBase<'id, A>,
        additional: usize,
    ) -> Result<(), AllocFailure> {
        if !self.needs_to_grow(additional) {
            return Ok(());
        }
        self.try_grow_cold(arena, additional, true)
    }

    #[cold]
    #[inline(never)]
    fn try_grow_cold<A: Allocator>(
        &mut self,
        arena: &ArenaBase<'id, A>,
        additional: usize,
        exact: bool,
    ) -> Result<(), AllocFailure> {
        let len = self.len();
        let overflow = AllocFailure::CapacityOverflow { what: type_name::<T>(), len, additional };
        let Some(required) = len.checked_add(additional) else {
            return Err(overflow);
        };
        if required > I::MAX_USIZE {
            return Err(overflow);
        }
        debug_assert!(!is_zst::<T>() && required > self.capacity());

        let target = if exact {
            required
        } else {
            let doubled = self.capacity().saturating_mul(2).min(I::MAX_USIZE);
            required.max(doubled).max(min_non_zero_cap::<T>()).min(I::MAX_USIZE)
        };
        let (target_cap, layout) = match Layout::array::<T>(target) {
            Ok(layout) => (target, layout),
            Err(_) if !exact && target > required => {
                let Ok(layout) = Layout::array::<T>(required) else {
                    return Err(overflow);
                };
                (required, layout)
            }
            Err(_) => return Err(overflow),
        };
        let target_idx = I::from_usize(target_cap).ok_or(overflow)?;

        // Bump-allocate a fresh slice in `arena` and copy existing elements; old slice stays in
        // the slab until `ArenaBase` drops.
        let new_slot = arena.alloc_slot(layout, type_name::<T>())?;
        if len > 0 {
            let old_ptr = self.data_ptr(arena).as_ptr();
            // SAFETY: `new_slot` was just allocated by `arena.alloc_slot` with `layout`.
            let new_ptr = unsafe { arena.ptr_from_slot::<T>(new_slot) }.as_ptr();
            // SAFETY: `old_ptr` holds `len` initialized `T`s; `new_ptr` is a freshly allocated
            // non-overlapping block with room for `target_cap >= len` elements.
            unsafe { ptr::copy_nonoverlapping(old_ptr, new_ptr, len) };
        }
        self.off = new_slot.get();
        self.cap = target_idx;
        Ok(())
    }

    /// Appends `val`, returning `Err(Refused { value, why })` if growing fails.
    #[blaze::cost(amortized O(1))]
    #[inline]
    pub fn try_push<A: Allocator>(
        &mut self,
        arena: &ArenaBase<'id, A>,
        val: T,
    ) -> Result<(), Refused<T>> {
        if self.len == self.cap {
            if let Err(why) = self.try_grow_cold(arena, 1, false) {
                return Err(Refused { value: val, why });
            }
        }
        let len = self.len();
        // SAFETY: `len < self.capacity()`, so slot `len` is valid for writes.
        unsafe { self.data_ptr(arena).as_ptr().add(len).write(val) };
        // SAFETY: `len < self.capacity() <= I::MAX_USIZE`, so `len + 1` fits in `I`.
        self.len = unsafe { I::from_usize(len + 1).unwrap_unchecked() };
        Ok(())
    }

    /// Appends `val`; panics with [`AllocFailure`] on overflow or arena exhaustion.
    #[blaze::cost(amortized O(1))]
    #[inline]
    #[track_caller]
    pub fn push<A: Allocator>(&mut self, arena: &ArenaBase<'id, A>, val: T) {
        if let Err(refused) = self.try_push(arena, val) {
            fail(refused.why);
        }
    }

    /// Appends clones of `other` into `self`.
    pub fn try_extend_from_slice<A: Allocator>(
        &mut self,
        arena: &ArenaBase<'id, A>,
        other: &[T],
    ) -> Result<(), AllocFailure>
    where
        T: Clone,
    {
        if other.is_empty() {
            return Ok(());
        }
        self.try_reserve(arena, other.len())?;
        let mut count = self.len();
        let dst = self.data_ptr(arena).as_ptr();
        let len_slot = &mut self.len;
        let mut guard = DropGuard::new(&mut count, |c| {
            // SAFETY: `*c <= initial_len + other.len() <= capacity <= I::MAX_USIZE`.
            *len_slot = unsafe { I::from_usize(*c).unwrap_unchecked() };
        });
        // SAFETY: `try_reserve` ensured room for at least `other.len()` elements; `other` cannot
        // alias `self`'s slice because `&mut self` is exclusive.
        unsafe { T::clone_to(other, dst, &mut guard) };
        Ok(())
    }

    /// Appends clones of `other` into `self`; panics with [`AllocFailure`] on overflow or exhaustion.
    #[track_caller]
    pub fn extend_from_slice<A: Allocator>(&mut self, arena: &ArenaBase<'id, A>, other: &[T])
    where
        T: Clone,
    {
        if let Err(failure) = self.try_extend_from_slice(arena, other) {
            fail(failure);
        }
    }

    /// Removes and returns the last element, or `None` if empty.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn pop<A: Allocator>(&mut self, arena: &ArenaBase<'id, A>) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let new_len = len - 1;
        // SAFETY: `new_len < len <= I::MAX_USIZE`.
        self.len = unsafe { I::from_usize(new_len).unwrap_unchecked() };
        // SAFETY: slot `new_len` held an initialized `T` and is now outside `0..self.len`.
        Some(unsafe { self.data_ptr(arena).as_ptr().add(new_len).read() })
    }

    /// Shortens the vector to `len`, dropping excess elements in place.
    pub fn truncate<A: Allocator>(&mut self, len: usize, arena: &ArenaBase<'id, A>) {
        let cur = self.len();
        if len >= cur {
            return;
        }
        // SAFETY: `len < cur <= I::MAX_USIZE`.
        self.len = unsafe { I::from_usize(len).unwrap_unchecked() };
        let drop_count = cur - len;
        if core::mem::needs_drop::<T>() {
            // SAFETY: slots `len..cur` hold initialized `T`s; `self.len` was updated first so an
            // unwinding destructor cannot double-drop them.
            unsafe {
                let tail = self.data_ptr(arena).as_ptr().add(len);
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(tail, drop_count));
            }
        }
    }

    /// Drops all elements in place, resetting `len` to 0 while keeping capacity in `arena`.
    #[inline]
    pub fn clear<A: Allocator>(&mut self, arena: &ArenaBase<'id, A>) {
        self.truncate(0, arena);
    }

    /// Removes the element at `index` in O(1) by swapping in the last element.
    #[inline]
    #[track_caller]
    pub fn swap_remove<A: Allocator>(&mut self, index: usize, arena: &ArenaBase<'id, A>) -> T {
        let len = self.len();
        assert!(index < len, "swap_remove index (is {index}) should be < len (is {len})");
        let last = len - 1;
        let ptr = self.data_ptr(arena).as_ptr();
        // SAFETY: `index < len`, so slot `index` holds an initialized `T`.
        let value = unsafe { ptr.add(index).read() };
        if index != last {
            // SAFETY: `index != last` and both are `< len`, so slots are distinct and valid.
            unsafe { ptr::copy_nonoverlapping(ptr.add(last), ptr.add(index), 1) };
        }
        // SAFETY: `last < len <= I::MAX_USIZE`.
        self.len = unsafe { I::from_usize(last).unwrap_unchecked() };
        value
    }
}

impl<T, I: LenIdx> fmt::Debug for ArenaVec<'_, T, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArenaVec")
            .field("off", &self.off)
            .field("len", &self.len())
            .field("cap", &self.capacity())
            .finish()
    }
}

impl<'id, T, I: LenIdx> Default for ArenaVec<'id, T, I> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, 'id, T, I: LenIdx, A: Allocator> DropWith<&'a ArenaBase<'id, A>>
    for ArenaVec<'id, T, I>
{
    #[inline]
    #[blaze::consumes]
    fn drop_with(mut self, cx: &'a ArenaBase<'id, A>) {
        self.clear(cx);
    }
}

// Compile-time compressed-pointer layout proofs.
const _: () = {
    // Ref32: strictly 4 bytes, Option<Ref32> = 4 bytes via NonZeroU32 niche.
    assert!(size_of::<Ref32<'_, u64>>() == 4);
    assert!(size_of::<Option<Ref32<'_, u64>>>() == 4);
    assert!(size_of::<Ref32<'_, [u64; 16]>>() == 4);
    assert!(size_of::<Option<Ref32<'_, [u64; 16]>>>() == 4);
    // ArenaVec: 8 bytes default (I = u16, 1 register!), 12 bytes for I = u32.
    assert!(size_of::<ArenaVec<'_, u64>>() == 8);
    assert!(size_of::<ArenaVec<'_, u64, u16>>() == 8);
    assert!(size_of::<ArenaVec<'_, u64, u32>>() == 12);
    assert!(align_of::<ArenaVec<'_, u64>>() == 4);
};

#[cfg(test)]
mod tests {
    use core::{cell::Cell, mem::size_of};

    use super::*;
    use crate::{
        brand::{DropWith, with_brand},
        testing::{Counting, TestHeap},
    };

    struct DropTrack<'c> {
        val: u32,
        drops: &'c Cell<usize>,
    }

    impl Drop for DropTrack<'_> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn compressed_pointer_sizes_and_niches() {
        assert_eq!(size_of::<Ref32<'_, u64>>(), 4);
        assert_eq!(size_of::<Option<Ref32<'_, u64>>>(), 4);
        assert_eq!(size_of::<ArenaVec<'_, u64>>(), 8);
        assert_eq!(size_of::<ArenaVec<'_, u64, u32>>(), 12);
    }

    #[test]
    fn ref32_alloc_read_write_zst_and_zero_leaks() {
        let counting = Counting::new(TestHeap);
        with_brand(|brand| {
            let mut arena = ArenaBase::with_capacity_in(4096, &counting, brand);
            assert_eq!(arena.used_bytes(), 0);
            assert!(arena.capacity_bytes() >= 4096);

            let r_u8 = arena.alloc_ref32(0xABu8).unwrap();
            let r_u64 = arena.alloc_ref32(0xDEAD_BEEF_CAFE_BABEu64).unwrap();
            let r_u128 = arena.alloc_ref32(123_456_789_012_345_678u128).unwrap();
            let r_zst = arena.alloc_ref32(()).unwrap();

            assert_eq!(*arena.get(r_u8), 0xAB);
            assert_eq!(*arena.get(r_u64), 0xDEAD_BEEF_CAFE_BABE);
            assert_eq!(*arena.get(r_u128), 123_456_789_012_345_678);
            assert_eq!(*arena.get(r_zst), ());

            // Copyable Ref32 + exclusive get_mut.
            let r_copy = r_u64;
            *arena.get_mut(r_u64) = 99;
            assert_eq!(*arena.get(r_copy), 99);
            assert_eq!(r_u64, r_copy);

            // Option<Ref32> niche check at runtime.
            let opt: Option<Ref32<'_, u64>> = Some(r_u64);
            assert_eq!(opt.map(|r| *arena.get(r)), Some(99));
            let none: Option<Ref32<'_, u64>> = None;
            assert!(none.is_none());
        });
        counting.assert_empty();
    }

    #[test]
    fn arena_vec_grow_mutate_disjoint_drop_and_oom() {
        let counting = Counting::new(TestHeap);
        let drops = Cell::new(0usize);

        with_brand(|brand| {
            let arena = ArenaBase::with_capacity_in(8192, &counting, brand);

            // Two independent 8-byte ArenaVecs mutated simultaneously via disjoint `&mut v1`, `&mut v2`.
            let mut v1 = ArenaVec::<u64>::new();
            let mut v2 = ArenaVec::<u32, u32>::with_capacity_in(8, &arena);

            for i in 0..40u64 {
                v1.push(&arena, i * 3);
            }
            v2.extend_from_slice(&arena, &[10, 20, 30, 40]);

            {
                let s1 = v1.as_mut_slice(&arena);
                let s2 = v2.as_mut_slice(&arena);
                s1[0] = 777;
                s2[3] = 400;
            }
            assert_eq!(v1.as_slice(&arena)[0], 777);
            assert_eq!(v1.as_slice(&arena)[39], 117);
            assert_eq!(v2.as_slice(&arena), &[10, 20, 30, 400]);

            assert_eq!(v1.swap_remove(0, &arena), 777);
            assert_eq!(v1.as_slice(&arena)[0], 117);
            assert_eq!(v1.pop(&arena), Some(114));
            v1.truncate(5, &arena);
            assert_eq!(v1.len(), 5);

            // Drop elements cleanly through DropWith<&ArenaBase>.
            let mut tracked = ArenaVec::<_>::new();
            for val in 0..12u32 {
                tracked.push(&arena, DropTrack { val, drops: &drops });
            }
            assert_eq!(tracked.as_slice(&arena)[11].val, 11);
            assert_eq!(drops.get(), 0);
            tracked.truncate(8, &arena);
            assert_eq!(drops.get(), 4);
            tracked.drop_with(&arena);
            assert_eq!(drops.get(), 12);

            // ZST ArenaVec allocates zero slab bytes.
            let used_before = arena.used_bytes();
            let mut zst = ArenaVec::<()>::new();
            for _ in 0..100 {
                zst.push(&arena, ());
            }
            assert_eq!(zst.len(), 100);
            assert_eq!(arena.used_bytes(), used_before);

            // u16 overflow refuses cleanly without truncation.
            let err = ArenaVec::<u32>::try_with_capacity_in(u16::MAX as usize + 1, &arena)
                .unwrap_err();
            assert!(matches!(err, AllocFailure::CapacityOverflow { .. }));
        });
        counting.assert_empty();

        // Tight arena exhaustion refuses cleanly and keeps existing elements intact.
        with_brand(|brand| {
            let tight = ArenaBase::with_capacity_in(32, &counting, brand);
            let mut v = ArenaVec::<u64>::new();
            for i in 0..4u64 {
                v.try_push(&tight, i).expect("fits in 32-byte slab");
            }
            let refused = v.try_push(&tight, 99).unwrap_err();
            assert_eq!(refused.value, 99);
            assert!(matches!(refused.why, AllocFailure::Refused { .. }));
            assert_eq!(v.as_slice(&tight), &[0, 1, 2, 3]);
        });
        counting.assert_empty();
    }
}
