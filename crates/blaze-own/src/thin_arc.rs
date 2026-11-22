//! [`ThinArc`], [`ThinArcSlice`], and [`ThinStr`]: single-word, atomically reference-counted
//! header + inline slice allocations with an explicit allocator stored inside the heap block.
//!
//! Unlike [`Arc<[T], A>`](crate::Arc), which is a fat pointer plus allocator on the stack (24+
//! bytes), a [`ThinArc<H, T, A>`] stores its strong count (`u32`, no weak count), slice length
//! (`u32`), allocator `A`, and header `H` inline before the `[T; len]` payload in a single heap
//! allocation. Every handle is one non-null pointer (8 bytes on 64-bit, with niche optimization
//! so `Option<ThinArc<H, T, A>>` is also 8 bytes) even when `A` is a non-ZST handle like
//! [`Alloc<'a>`](crate::Alloc).
//!
//! Because `A` lives once in the heap header rather than being duplicated per handle, cloning a
//! [`ThinArc`] never clones `A` (no `AllocatorClone` bound required).
//!
//! # Immortal handles
//!
//! Calling [`ThinArc::make_immortal`] (or [`ThinStr::make_immortal`]) marks the allocation as
//! immortal. Subsequent [`Clone`] and [`Drop`] calls perform only a single relaxed load and
//! branch — zero atomic read-modify-write instructions — eliminating cache-line bouncing on
//! widely shared read-only metadata and interned strings. An empty [`ThinStr`] points to a static
//! immortal sentinel and performs zero heap allocations.

use core::{
    alloc::{Allocator, Layout},
    any::type_name,
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{DropGuard, align_of, size_of},
    ops::Deref,
    ptr::{self, NonNull},
    slice, str,
    sync::atomic::{
        self, AtomicU32,
        Ordering::{Acquire, Relaxed, Release},
    },
};

use crate::{
    error::{AllocFailure, abort_count_overflow, fail},
    spec::CloneSlice,
};

/// Strong-count value stored by [`ThinArc::make_immortal`].
///
/// Any count with bit 31 set (`(count as i32) < 0`) is treated as immortal. Placing the mark at
/// `0xC000_0000` leaves over one billion counts of headroom in both directions so any racing
/// `fetch_add(1)` or `fetch_sub(1)` that passed the relaxed immortal check just before
/// `make_immortal` wrote the mark still stays strictly inside `[0x8000_0000, 0xFFFF_FFFF]` and
/// can never wrap to `0` or reach `1`.
const IMMORTAL_MARK: u32 = 0xC000_0000;

/// Public sentinel value reported by `strong_count` for immortal handles.
const IMMORTAL_COUNT: usize = u32::MAX as usize;

/// Maximum mortal strong count before aborting to prevent overflow into the immortal range.
const MAX_REFCOUNT: u32 = 0x3FFF_FFFF;

#[inline(always)]
const fn is_immortal_count(count: u32) -> bool {
    (count as i32) < 0
}

/// Fixed 8-byte prefix at offset 0 of every [`ThinArc`] block (and of [`EMPTY_STR`]).
#[repr(C)]
struct HeaderPrefix {
    strong: AtomicU32,
    len: u32,
}

/// Heap header preceding the inline `[T; len]` payload.
///
/// `#[repr(C)]` guarantees `prefix` sits at offset 0 so `strong` and `len` can always be read
/// through `HeaderPrefix` without touching `alloc` or `header`.
#[repr(C)]
struct ThinArcHeader<H, A> {
    prefix: HeaderPrefix,
    alloc: A,
    header: H,
}

/// Static immortal empty-string header. Only bytes `0..8` (`HeaderPrefix`) are ever read; its
/// strong count has bit 31 set so `clone`, `drop`, and `make_immortal` never write to it and
/// never read past byte 8.
static EMPTY_STR: HeaderPrefix = HeaderPrefix { strong: AtomicU32::new(u32::MAX), len: 0 };

/// Byte offset of the first `T` from the start of a [`ThinArc`] block.
///
/// Independent of `len`; matches the offset returned by [`Layout::extend`] in [`block_layout`].
#[inline(always)]
const fn data_offset<H, T, A>() -> usize {
    size_of::<ThinArcHeader<H, A>>().next_multiple_of(align_of::<T>())
}

/// Computes the combined `(layout, data_offset)` for a [`ThinArcHeader<H, A>`] followed by
/// `[T; len]`, padded to the combined alignment.
#[inline]
fn block_layout<H, T, A>(len: usize) -> Option<(Layout, usize)> {
    if len > u32::MAX as usize {
        return None;
    }
    let header_layout = Layout::new::<ThinArcHeader<H, A>>();
    let slice_layout = Layout::array::<T>(len).ok()?;
    let (combined, offset) = header_layout.extend(slice_layout).ok()?;
    debug_assert_eq!(offset, data_offset::<H, T, A>());
    Some((combined.pad_to_align(), offset))
}

/// A thin (single-pointer) atomically reference-counted header `H` paired with an inline slice
/// `[T]`, backed by an explicit allocator `A` stored in the heap header.
///
/// # Invariants
///
/// - For every public constructor of `ThinArc`, `ptr` points to a live block allocated by `alloc`
///   with `block_layout::<H, T, A>(len)`, with full-block provenance; its `HeaderPrefix`,
///   `alloc`, `header`, and `len` trailing `T` values at `data_offset::<H, T, A>()` are
///   initialized, and `strong` accounts for this handle (or is immortal).
/// - For the private empty-string sentinel used exclusively by [`ThinStr`], `ptr` points to
///   [`EMPTY_STR`] (`strong == u32::MAX`, `len == 0`) and only `HeaderPrefix` is ever read.
pub struct ThinArc<H, T, A: Allocator> {
    ptr: NonNull<u8>,
    _marker: PhantomData<(H, T, A)>,
}

/// A thin atomically reference-counted slice `[T]` with no extra header.
pub type ThinArcSlice<T, A> = ThinArc<(), T, A>;

const _: () = {
    assert!(size_of::<ThinArc<(), u8, crate::Alloc<'static>>>() == size_of::<usize>());
    assert!(size_of::<Option<ThinArc<(), u8, crate::Alloc<'static>>>>() == size_of::<usize>());
    assert!(size_of::<ThinArc<[u64; 8], [u8; 64], crate::Alloc<'static>>>() == size_of::<usize>());
    assert!(size_of::<Option<ThinArc<[u64; 8], [u8; 64], crate::Alloc<'static>>>>() == size_of::<usize>());
    assert!(size_of::<ThinStr<crate::Alloc<'static>>>() == size_of::<usize>());
    assert!(size_of::<Option<ThinStr<crate::Alloc<'static>>>>() == size_of::<usize>());
};

// SAFETY: a `ThinArc` on another thread can read `&H`, `&[T]`, and `&A` (`H: Sync, T: Sync,
// A: Sync`), and may be the last owner to drop `H`, `[T]`, and `A` and call `A::deallocate`
// there (`H: Send, T: Send, A: Send`).
unsafe impl<H: Send + Sync, T: Send + Sync, A: Allocator + Send + Sync> Send for ThinArc<H, T, A> {}
// SAFETY: `&ThinArc` can be cloned into an owned `ThinArc` on another thread, requiring the same
// bounds as `Send`.
unsafe impl<H: Send + Sync, T: Send + Sync, A: Allocator + Send + Sync> Sync for ThinArc<H, T, A> {}

impl<H, T, A: Allocator> Unpin for ThinArc<H, T, A> {}

impl<H, T, A: Allocator> ThinArc<H, T, A> {
    /// Allocates a new `ThinArc` containing `header` and the elements yielded by `items` in
    /// `alloc`; panics with [`AllocFailure`] if memory cannot be obtained.
    ///
    /// Panics if `items` yields fewer elements than its [`ExactSizeIterator::len`].
    #[track_caller]
    pub fn new_in<I>(header: H, items: I, alloc: A) -> ThinArc<H, T, A>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        match ThinArc::try_new_in(header, items, alloc) {
            Ok(arc) => arc,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible version of [`ThinArc::new_in`].
    pub fn try_new_in<I>(header: H, items: I, alloc: A) -> Result<ThinArc<H, T, A>, AllocFailure>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut iter = items.into_iter();
        let len = iter.len();
        let Some((layout, offset)) = block_layout::<H, T, A>(len) else {
            return Err(AllocFailure::CapacityOverflow {
                what: type_name::<T>(),
                len: 0,
                additional: len,
            });
        };
        let block = alloc
            .allocate(layout)
            .map_err(|_| AllocFailure::Refused { what: type_name::<ThinArc<H, T, A>>(), layout })?
            .cast::<u8>();
        // SAFETY: `offset` is within `layout` (or at end for ZST / `len == 0`) and aligned to `T`.
        let data_ptr = unsafe { block.as_ptr().add(offset).cast::<T>() };
        // Keep `header` and `alloc` on the stack until all items are written; if `iter.next()`
        // panics or terminates early, this guard drops the initialized prefix and frees `block`,
        // after which `header`, `iter`, and `alloc` drop normally.
        let mut written = DropGuard::new(0usize, |written| {
            // SAFETY: `data_ptr[..written]` are initialized and owned by no one else; `block` was
            // allocated by `alloc` with `layout`.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(data_ptr, written));
                alloc.deallocate(block, layout);
            }
        });
        for i in 0..len {
            let Some(item) = iter.next() else {
                panic!("blaze_own::ThinArc: ExactSizeIterator yielded fewer items than len()");
            };
            // SAFETY: `i < len`, so slot `i` lies within the allocated `[T; len]` region and is
            // uninitialized.
            unsafe { data_ptr.add(i).write(item) };
            *written = i + 1;
        }
        DropGuard::dismiss(written);
        // SAFETY: `block` is aligned to `ThinArcHeader<H, A>` and uninitialized in its header
        // prefix; `header` and `alloc` move in once.
        unsafe {
            block.as_ptr().cast::<ThinArcHeader<H, A>>().write(ThinArcHeader {
                prefix: HeaderPrefix { strong: AtomicU32::new(1), len: len as u32 },
                alloc,
                header,
            });
        }
        let arc = ThinArc { ptr: block, _marker: PhantomData };
        // Explicitly drop `iter` after `arc` is constructed so even a panicking `iter::Drop`
        // cleanly drops `arc`.
        drop(iter);
        Ok(arc)
    }

    /// Allocates a new `ThinArc` with `header` and clones of `slice` in `alloc`; panics with
    /// [`AllocFailure`] on allocation failure.
    #[track_caller]
    pub fn from_header_and_slice_in(header: H, slice: &[T], alloc: A) -> ThinArc<H, T, A>
    where
        T: Clone,
    {
        match ThinArc::try_from_header_and_slice_in(header, slice, alloc) {
            Ok(arc) => arc,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible version of [`ThinArc::from_header_and_slice_in`].
    pub fn try_from_header_and_slice_in(
        header: H,
        slice: &[T],
        alloc: A,
    ) -> Result<ThinArc<H, T, A>, AllocFailure>
    where
        T: Clone,
    {
        let len = slice.len();
        let Some((layout, offset)) = block_layout::<H, T, A>(len) else {
            return Err(AllocFailure::CapacityOverflow {
                what: type_name::<T>(),
                len: 0,
                additional: len,
            });
        };
        let block = alloc
            .allocate(layout)
            .map_err(|_| AllocFailure::Refused { what: type_name::<ThinArc<H, T, A>>(), layout })?
            .cast::<u8>();
        // SAFETY: `offset` is within `layout` (or at end for ZST / `len == 0`) and aligned to `T`.
        let data_ptr = unsafe { block.as_ptr().add(offset).cast::<T>() };
        let mut written = DropGuard::new(0usize, |written| {
            // SAFETY: `data_ptr[..written]` are initialized; `block` was allocated by `alloc`
            // with `layout`.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(data_ptr, written));
                alloc.deallocate(block, layout);
            }
        });
        if len > 0 {
            // SAFETY: `len` slots starting at `data_ptr` lie in one fresh allocation, are
            // uninitialized, and cannot overlap `slice`.
            unsafe { CloneSlice::clone_to(slice, data_ptr, &mut written) };
        }
        DropGuard::dismiss(written);
        // SAFETY: `block` is aligned to `ThinArcHeader<H, A>` and uninitialized in its header
        // region; `header` and `alloc` move in once.
        unsafe {
            block.as_ptr().cast::<ThinArcHeader<H, A>>().write(ThinArcHeader {
                prefix: HeaderPrefix { strong: AtomicU32::new(1), len: len as u32 },
                alloc,
                header,
            });
        }
        Ok(ThinArc { ptr: block, _marker: PhantomData })
    }

    #[inline(always)]
    fn prefix(&self) -> &HeaderPrefix {
        // SAFETY: `self.ptr` always points to a live `HeaderPrefix` at offset 0 (invariant).
        unsafe { &*self.ptr.as_ptr().cast::<HeaderPrefix>() }
    }

    #[inline(always)]
    fn strong(&self) -> &AtomicU32 {
        &self.prefix().strong
    }

    /// Reference to the inline header `H`.
    #[inline]
    pub fn header(&self) -> &H {
        // SAFETY: every public `ThinArc` holds a live `ThinArcHeader<H, A>` at offset 0.
        unsafe { &(*self.ptr.as_ptr().cast::<ThinArcHeader<H, A>>()).header }
    }

    /// Reference to the inline slice `[T]`.
    #[inline]
    pub fn slice(&self) -> &[T] {
        let len = self.prefix().len as usize;
        if len == 0 {
            &[]
        } else {
            // SAFETY: `len > 0` implies this is a heap allocation with `len` initialized `T`s at
            // `data_offset::<H, T, A>()`.
            unsafe {
                slice::from_raw_parts(
                    self.ptr.as_ptr().add(data_offset::<H, T, A>()).cast::<T>(),
                    len,
                )
            }
        }
    }

    /// Number of elements in the inline slice.
    #[inline]
    pub fn len(&self) -> usize {
        self.prefix().len as usize
    }

    /// Whether the inline slice has zero elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.prefix().len == 0
    }

    /// Pointer to the first element of the inline slice (dangling and aligned when empty).
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        if self.prefix().len == 0 {
            NonNull::dangling().as_ptr()
        } else {
            // SAFETY: `len > 0` implies a live block with elements at `data_offset::<H, T, A>()`.
            unsafe { self.ptr.as_ptr().add(data_offset::<H, T, A>()).cast::<T>() }
        }
    }

    /// Reference to the allocator stored in the heap header.
    #[inline]
    pub fn allocator(&self) -> &A {
        // SAFETY: every public `ThinArc` holds a live `ThinArcHeader<H, A>` at offset 0.
        unsafe { &(*self.ptr.as_ptr().cast::<ThinArcHeader<H, A>>()).alloc }
    }

    /// Current strong reference count (`u32::MAX as usize` when immortal).
    #[inline]
    pub fn strong_count(&self) -> usize {
        let count = self.strong().load(Relaxed);
        if is_immortal_count(count) { IMMORTAL_COUNT } else { count as usize }
    }

    /// Marks this allocation as immortal so all future [`Clone`] and [`Drop`] calls skip atomic
    /// RMW instructions (`fetch_add`/`fetch_sub`) and never deallocate the block.
    #[inline]
    pub fn make_immortal(&self) {
        if !is_immortal_count(self.strong().load(Relaxed)) {
            // Relaxed: the block was already published before `&self` was shared and will never
            // be deallocated once immortal.
            self.strong().store(IMMORTAL_MARK, Relaxed);
        }
    }

    /// Whether this handle points to an immortal allocation.
    #[inline]
    pub fn is_immortal(&self) -> bool {
        is_immortal_count(self.strong().load(Relaxed))
    }

    /// Whether `this` and `other` point to the same allocation.
    #[inline]
    pub fn ptr_eq(this: &ThinArc<H, T, A>, other: &ThinArc<H, T, A>) -> bool {
        ptr::addr_eq(this.ptr.as_ptr(), other.ptr.as_ptr())
    }

    /// Mutable references to `(header, slice)` if this is the sole non-immortal owner.
    #[inline]
    pub fn get_mut(&mut self) -> Option<(&mut H, &mut [T])> {
        // Acquire: synchronizes with the Release decrements of any dropped clones so their reads
        // happen before our writes. Immortal handles have bit 31 set, so they never equal 1.
        if self.strong().load(Acquire) != 1 {
            return None;
        }
        let len = self.prefix().len as usize;
        // SAFETY: `strong == 1` and we hold `&mut self`, so no other handle exists. `header` and
        // `[T; len]` occupy disjoint byte ranges inside the allocation.
        unsafe {
            let hdr_ptr = self.ptr.as_ptr().cast::<ThinArcHeader<H, A>>();
            let header = &mut (*hdr_ptr).header;
            let data = if len == 0 {
                &mut []
            } else {
                slice::from_raw_parts_mut(
                    self.ptr.as_ptr().add(data_offset::<H, T, A>()).cast::<T>(),
                    len,
                )
            };
            Some((header, data))
        }
    }

    /// Mutable reference to the header `H` if this is the sole non-immortal owner.
    #[inline]
    pub fn get_mut_header(&mut self) -> Option<&mut H> {
        self.get_mut().map(|(h, _)| h)
    }

    /// Mutable reference to the inline slice `[T]` if this is the sole non-immortal owner.
    #[inline]
    pub fn get_mut_slice(&mut self) -> Option<&mut [T]> {
        self.get_mut().map(|(_, s)| s)
    }
}

impl<T, A: Allocator> ThinArc<(), T, A> {
    /// Allocates a [`ThinArcSlice`] from clones of `slice` in `alloc`.
    #[track_caller]
    pub fn from_slice_in(slice: &[T], alloc: A) -> ThinArcSlice<T, A>
    where
        T: Clone,
    {
        ThinArc::from_header_and_slice_in((), slice, alloc)
    }

    /// Fallible version of [`ThinArc::from_slice_in`].
    pub fn try_from_slice_in(slice: &[T], alloc: A) -> Result<ThinArcSlice<T, A>, AllocFailure>
    where
        T: Clone,
    {
        ThinArc::try_from_header_and_slice_in((), slice, alloc)
    }

    /// Allocates a [`ThinArcSlice`] from an exact-size iterator in `alloc`.
    #[track_caller]
    pub fn from_iter_in<I>(items: I, alloc: A) -> ThinArcSlice<T, A>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        ThinArc::new_in((), items, alloc)
    }

    /// Fallible version of [`ThinArc::from_iter_in`].
    pub fn try_from_iter_in<I>(items: I, alloc: A) -> Result<ThinArcSlice<T, A>, AllocFailure>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        ThinArc::try_new_in((), items, alloc)
    }
}

impl<A: Allocator> ThinArc<(), u8, A> {
    /// Private constructor for [`ThinStr`]'s zero-allocation empty string.
    ///
    /// Only `HeaderPrefix` (`strong`, `len`), `slice()` (which returns `&[]` for `len == 0`),
    /// `Clone`, `Drop`, `make_immortal`, `is_immortal`, `strong_count`, and `ptr_eq` may be
    /// called on the returned handle; `ThinStr` never exposes `allocator()` or `header()`.
    #[inline]
    fn empty_immortal_str() -> ThinArc<(), u8, A> {
        let ptr = NonNull::from(&EMPTY_STR).cast::<u8>();
        ThinArc { ptr, _marker: PhantomData }
    }
}

impl<H, T, A: Allocator> Clone for ThinArc<H, T, A> {
    #[inline]
    fn clone(&self) -> ThinArc<H, T, A> {
        // Fast path for immortal handles: a single relaxed load, zero atomic RMW.
        let cur = self.strong().load(Relaxed);
        if !is_immortal_count(cur) {
            // Relaxed: the existing handle keeps the allocation alive; Drop's Release/Acquire
            // synchronizes accesses before deallocation.
            let old = self.strong().fetch_add(1, Relaxed);
            if !is_immortal_count(old) && old > MAX_REFCOUNT {
                abort_count_overflow(type_name::<T>());
            }
        }
        ThinArc { ptr: self.ptr, _marker: PhantomData }
    }
}

// SAFETY (`may_dangle`): `Drop` drops the owned `H` and `[T]` (`PhantomData<(H, T, A)>` informs
// dropck) and reads nothing else of them.
unsafe impl<#[may_dangle] H, #[may_dangle] T, A: Allocator> Drop for ThinArc<H, T, A> {
    #[inline]
    fn drop(&mut self) {
        // Fast path for immortal handles (including `ThinStr`'s static `EMPTY_STR` sentinel):
        // a single relaxed load, zero atomic RMW, never touches `alloc` or deallocates.
        let cur = self.strong().load(Relaxed);
        if is_immortal_count(cur) {
            return;
        }
        // Release: all reads through this handle happen before the decrement, synchronizing with
        // the last owner's Acquire fence.
        if self.strong().fetch_sub(1, Release) != 1 {
            return;
        }
        // Acquire: synchronizes with every other owner's Release decrement before dropping.
        atomic::fence(Acquire);
        self.drop_slow();
    }
}

impl<H, T, A: Allocator> ThinArc<H, T, A> {
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self) {
        let block = self.ptr;
        let len = self.prefix().len as usize;
        // SAFETY: a live heap block was allocated with `block_layout::<H, T, A>(len)`.
        let (layout, offset) = unsafe { block_layout::<H, T, A>(len).unwrap_unchecked() };
        let hdr = block.as_ptr().cast::<ThinArcHeader<H, A>>();
        // SAFETY: we are the sole owner; move `alloc` out once so it stays available to
        // deallocate the block even if dropping `header` or any `T` panics.
        let alloc = unsafe { ptr::read(&raw const (*hdr).alloc) };
        let _dealloc = DropGuard::new((), |()| {
            // SAFETY: `block` was allocated by `alloc` with `layout`, freed once here; `alloc`
            // itself is dropped after `deallocate` returns.
            unsafe { alloc.deallocate(block, layout) };
        });
        // SAFETY: `offset` is `data_offset::<H, T, A>()` inside `block`.
        let data_ptr = unsafe { block.as_ptr().add(offset).cast::<T>() };
        // Ensure `[T]` is dropped even if `H::drop` panics.
        let _drop_slice = DropGuard::new((), |()| {
            if len > 0 {
                // SAFETY: `data_ptr[..len]` are initialized and dropped once here.
                unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(data_ptr, len)) };
            }
        });
        // SAFETY: `header` is initialized and dropped once here.
        unsafe { ptr::drop_in_place(&raw mut (*hdr).header) };
    }
}

impl<H, T, A: Allocator> Deref for ThinArc<H, T, A> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.slice()
    }
}

impl<H, T, A: Allocator> AsRef<[T]> for ThinArc<H, T, A> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.slice()
    }
}

impl<H: fmt::Debug, T: fmt::Debug, A: Allocator> fmt::Debug for ThinArc<H, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThinArc")
            .field("header", self.header())
            .field("slice", &self.slice())
            .finish()
    }
}

impl<H: PartialEq, T: PartialEq, A: Allocator, B: Allocator> PartialEq<ThinArc<H, T, B>>
    for ThinArc<H, T, A>
{
    #[inline]
    fn eq(&self, other: &ThinArc<H, T, B>) -> bool {
        self.header() == other.header() && self.slice() == other.slice()
    }
}

impl<H: Eq, T: Eq, A: Allocator> Eq for ThinArc<H, T, A> {}

impl<H: PartialOrd, T: PartialOrd, A: Allocator> PartialOrd for ThinArc<H, T, A> {
    #[inline]
    fn partial_cmp(&self, other: &ThinArc<H, T, A>) -> Option<Ordering> {
        match self.header().partial_cmp(other.header()) {
            Some(Ordering::Equal) => self.slice().partial_cmp(other.slice()),
            ord => ord,
        }
    }
}

impl<H: Ord, T: Ord, A: Allocator> Ord for ThinArc<H, T, A> {
    #[inline]
    fn cmp(&self, other: &ThinArc<H, T, A>) -> Ordering {
        self.header().cmp(other.header()).then_with(|| self.slice().cmp(other.slice()))
    }
}

impl<H: Hash, T: Hash, A: Allocator> Hash for ThinArc<H, T, A> {
    #[inline]
    fn hash<S: Hasher>(&self, state: &mut S) {
        self.header().hash(state);
        self.slice().hash(state);
    }
}

// ---------------------------------------------------------------------------------------------
// ThinStr.

/// A thin (single-pointer) atomically reference-counted immutable UTF-8 string backed by an
/// explicit allocator `A`.
///
/// An empty string points to a static immortal sentinel and performs zero heap allocations.
pub struct ThinStr<A: Allocator> {
    inner: ThinArc<(), u8, A>,
}

impl<A: Allocator> ThinStr<A> {
    /// Copies `text` into a new `ThinStr` in `alloc`; panics with [`AllocFailure`] if memory
    /// cannot be obtained.
    ///
    /// When `text` is empty, performs zero heap allocations and returns a handle to the static
    /// immortal empty string.
    #[track_caller]
    pub fn from_str_in(text: &str, alloc: A) -> ThinStr<A> {
        match ThinStr::try_from_str_in(text, alloc) {
            Ok(s) => s,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible version of [`ThinStr::from_str_in`].
    pub fn try_from_str_in(text: &str, alloc: A) -> Result<ThinStr<A>, AllocFailure> {
        if text.is_empty() {
            drop(alloc);
            return Ok(ThinStr { inner: ThinArc::empty_immortal_str() });
        }
        let inner = ThinArc::try_from_slice_in(text.as_bytes(), alloc)?;
        Ok(ThinStr { inner })
    }

    /// The string's UTF-8 slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `ThinStr` is only constructed from valid `&str` bytes (or empty).
        unsafe { str::from_utf8_unchecked(self.inner.slice()) }
    }

    /// The string's raw UTF-8 bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.slice()
    }

    /// Byte length of the string.
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Current strong reference count (`u32::MAX as usize` when immortal).
    #[inline]
    pub fn strong_count(&self) -> usize {
        self.inner.strong_count()
    }

    /// Marks this string as immortal (zero atomic RMW on future clones and drops).
    #[inline]
    pub fn make_immortal(&self) {
        self.inner.make_immortal();
    }

    /// Whether this string is immortal (always `true` for empty strings).
    #[inline]
    pub fn is_immortal(&self) -> bool {
        self.inner.is_immortal()
    }

    /// Whether `this` and `other` point to the same underlying allocation.
    #[inline]
    pub fn ptr_eq(this: &ThinStr<A>, other: &ThinStr<A>) -> bool {
        ThinArc::ptr_eq(&this.inner, &other.inner)
    }
}

impl<A: Allocator> Clone for ThinStr<A> {
    #[inline]
    fn clone(&self) -> ThinStr<A> {
        ThinStr { inner: self.inner.clone() }
    }
}

impl<A: Allocator> Deref for ThinStr<A> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> AsRef<str> for ThinStr<A> {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> AsRef<[u8]> for ThinStr<A> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<A: Allocator> Borrow<str> for ThinStr<A> {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> fmt::Debug for ThinStr<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<A: Allocator> fmt::Display for ThinStr<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl<A: Allocator, B: Allocator> PartialEq<ThinStr<B>> for ThinStr<A> {
    #[inline]
    fn eq(&self, other: &ThinStr<B>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<A: Allocator> PartialEq<str> for ThinStr<A> {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<A: Allocator> PartialEq<&str> for ThinStr<A> {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<A: Allocator> Eq for ThinStr<A> {}

impl<A: Allocator> PartialOrd for ThinStr<A> {
    #[inline]
    fn partial_cmp(&self, other: &ThinStr<A>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<A: Allocator> Ord for ThinStr<A> {
    #[inline]
    fn cmp(&self, other: &ThinStr<A>) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl<A: Allocator> Hash for ThinStr<A> {
    #[inline]
    fn hash<S: Hasher>(&self, state: &mut S) {
        // Must hash as `str` (not `[u8]`) to satisfy `Borrow<str>`.
        self.as_str().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use core::{
        hash::{Hash, Hasher},
        mem::{align_of, size_of},
        sync::atomic::{AtomicUsize, Ordering},
    };
    use std::{
        alloc::Global,
        collections::hash_map::DefaultHasher,
        format,
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc as StdArc,
        thread,
        vec::Vec as StdVec,
    };

    use super::{ThinArc, ThinArcSlice, ThinStr};
    use crate::{
        AThinArc, AThinStr, Erased,
        testing::{Counting, TestHeap, every_failure},
    };

    fn hash_of<T: Hash + ?Sized>(val: &T) -> u64 {
        let mut h = DefaultHasher::new();
        val.hash(&mut h);
        h.finish()
    }

    #[test]
    fn size_and_niche_are_one_pointer() {
        assert_eq!(size_of::<ThinArc<(), u8, Global>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<ThinArc<(), u8, Global>>>(), size_of::<usize>());
        assert_eq!(size_of::<ThinArc<[u64; 4], [u8; 64], Global>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<ThinArc<[u64; 4], [u8; 64], Global>>>(), size_of::<usize>());

        // Non-ZST Alloc<'a> stored inside the heap header keeps the handle 8 bytes!
        assert_eq!(size_of::<AThinArc<'static, [u64; 4], u32>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<AThinArc<'static, [u64; 4], u32>>>(), size_of::<usize>());
        assert_eq!(size_of::<AThinStr<'static>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<AThinStr<'static>>>(), size_of::<usize>());
        assert_eq!(size_of::<ThinStr<Global>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<ThinStr<Global>>>(), size_of::<usize>());
    }

    #[test]
    fn basic_header_slice_and_cloning() {
        let counting = Counting::new(Global);
        {
            let a = ThinArc::new_in(42u64, [10i32, 20, 30], &counting);
            assert_eq!(*a.header(), 42);
            assert_eq!(a.slice(), &[10, 20, 30]);
            assert_eq!(&*a, &[10, 20, 30]);
            assert_eq!(a.len(), 3);
            assert!(!a.is_empty());
            assert_eq!(a.strong_count(), 1);
            assert_eq!(counting.live(), 1);

            let b = a.clone();
            assert!(ThinArc::ptr_eq(&a, &b));
            assert_eq!(a.strong_count(), 2);
            assert_eq!(b.strong_count(), 2);
            assert_eq!(a, b);
            assert_eq!(hash_of(&a), hash_of(&b));

            let c = ThinArc::new_in(42u64, [10i32, 20, 30], &counting);
            assert!(!ThinArc::ptr_eq(&a, &c));
            assert_eq!(a, c);
            assert_eq!(hash_of(&a), hash_of(&c));

            let d = ThinArc::new_in(99u64, [10i32, 20, 30], &counting);
            assert_ne!(a, d);

            drop(b);
            assert_eq!(a.strong_count(), 1);
        }
        counting.assert_empty();
    }

    #[test]
    fn drop_counts_no_leaks_no_double_drops() {
        #[derive(Debug)]
        struct DropCounter {
            id: usize,
            drops: StdArc<AtomicUsize>,
        }
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        let counting = Counting::new(Global);
        let header_drops = StdArc::new(AtomicUsize::new(0));
        let elem_drops = StdArc::new(AtomicUsize::new(0));
        {
            let h = DropCounter { id: 999, drops: StdArc::clone(&header_drops) };
            let items = (0..16).map(|id| DropCounter { id, drops: StdArc::clone(&elem_drops) });
            let a = ThinArc::new_in(h, items, &counting);
            let b = a.clone();
            let c = b.clone();
            assert_eq!(a.header().id, 999);
            assert_eq!(a.len(), 16);
            assert_eq!(a[7].id, 7);
            drop(a);
            assert_eq!(header_drops.load(Ordering::Relaxed), 0);
            assert_eq!(elem_drops.load(Ordering::Relaxed), 0);
            drop(b);
            assert_eq!(header_drops.load(Ordering::Relaxed), 0);
            assert_eq!(elem_drops.load(Ordering::Relaxed), 0);
            drop(c);
            assert_eq!(header_drops.load(Ordering::Relaxed), 1);
            assert_eq!(elem_drops.load(Ordering::Relaxed), 16);
        }
        counting.assert_empty();
    }

    #[test]
    fn panic_safety_in_iterator_and_drop() {
        struct Tracked(StdArc<AtomicUsize>, bool);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
                if self.1 {
                    panic!("drop panic");
                }
            }
        }

        // Panic halfway through ExactSizeIterator construction.
        let counting = Counting::new(Global);
        let drops = StdArc::new(AtomicUsize::new(0));
        let h_drops = StdArc::new(AtomicUsize::new(0));
        let res = catch_unwind(AssertUnwindSafe(|| {
            let iter = (0..10usize).map(|i| {
                if i == 4 {
                    panic!("iter panic");
                }
                Tracked(StdArc::clone(&drops), false)
            });
            let _ = ThinArc::new_in(Tracked(StdArc::clone(&h_drops), false), iter, &counting);
        }));
        assert!(res.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 4);
        assert_eq!(h_drops.load(Ordering::Relaxed), 1);
        counting.assert_empty();

        // Panic in header Drop still drops all elements and frees the block.
        let drops = StdArc::new(AtomicUsize::new(0));
        let h_drops = StdArc::new(AtomicUsize::new(0));
        let res = catch_unwind(AssertUnwindSafe(|| {
            let items = (0..5).map(|_| Tracked(StdArc::clone(&drops), false));
            let _arc = ThinArc::new_in(Tracked(StdArc::clone(&h_drops), true), items, &counting);
        }));
        assert!(res.is_err());
        assert_eq!(h_drops.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 5);
        counting.assert_empty();

        // Panic in element Clone during `from_slice_in`.
        #[derive(Debug)]
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
            PanicClone { drops: StdArc::clone(&clone_drops), should_panic: false },
        ];
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = ThinArcSlice::from_slice_in(&src, &counting);
        }));
        assert!(res.is_err());
        assert_eq!(clone_drops.load(Ordering::Relaxed), 2);
        counting.assert_empty();
    }

    #[test]
    fn over_aligned_t_and_h_and_zst() {
        #[repr(align(64))]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Aligned64([u64; 8]);

        #[repr(align(64))]
        #[derive(Debug, PartialEq, Eq)]
        struct AlignedHeader(u32);

        #[repr(align(64))]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct AlignedZst;

        let counting = Counting::new(Global);
        {
            // Over-aligned T with ZST header.
            let vals = [Aligned64([1; 8]), Aligned64([2; 8]), Aligned64([3; 8])];
            let a = ThinArcSlice::from_slice_in(&vals, &counting);
            assert_eq!((a.as_ptr() as usize) % align_of::<Aligned64>(), 0);
            assert_eq!(a.slice(), &vals);

            // Over-aligned T with non-ZST Alloc<'a> handle!
            let erased = Erased::new(&counting);
            let b: AThinArc<'_, u8, Aligned64> =
                ThinArc::from_header_and_slice_in(7u8, &vals, erased.handle());
            assert_eq!(*b.header(), 7);
            assert_eq!((b.as_ptr() as usize) % align_of::<Aligned64>(), 0);
            assert_eq!(b.slice(), &vals);

            // Over-aligned header with byte slice.
            let c = ThinArc::from_header_and_slice_in(AlignedHeader(123), b"hello", &counting);
            assert_eq!((c.header() as *const AlignedHeader as usize) % 64, 0);
            assert_eq!(c.header().0, 123);
            assert_eq!(c.slice(), b"hello");

            // ZST T and ZST H, including over-aligned ZST.
            let z1 = ThinArc::new_in((), (0..100).map(|_| ()), &counting);
            assert_eq!(z1.len(), 100);
            let z2 = ThinArc::new_in((), [AlignedZst; 16], &counting);
            assert_eq!(z2.len(), 16);
            assert_eq!((z2.as_ptr() as usize) % 64, 0);

            // Empty slice with non-ZST header.
            let empty = ThinArc::<u64, Aligned64, _>::new_in(55, core::iter::empty(), &counting);
            assert_eq!(*empty.header(), 55);
            assert!(empty.is_empty());
            assert_eq!((empty.as_ptr() as usize) % 64, 0);
        }
        counting.assert_empty();
    }

    #[test]
    fn zst_drop_count() {
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
            let arc = ThinArc::new_in((), (0..7).map(|_| ZstDrop), &counting);
            assert_eq!(arc.len(), 7);
        }
        assert_eq!(ZST_DROPS.load(Ordering::Relaxed), 7);
        counting.assert_empty();
    }

    #[test]
    fn immortal_refcount_never_drops_and_skips_rmw() {
        let drops = StdArc::new(AtomicUsize::new(0));
        struct Watch(StdArc<AtomicUsize>);
        impl Drop for Watch {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Intentional leak for an immortal handle, allocated from Global.
        let arc = ThinArc::new_in(Watch(StdArc::clone(&drops)), [1u32, 2, 3, 4], Global);
        assert!(!arc.is_immortal());
        assert_eq!(arc.strong_count(), 1);
        arc.make_immortal();
        assert!(arc.is_immortal());
        assert_eq!(arc.strong_count(), u32::MAX as usize);

        // Cloning and dropping immortal handles across threads never drops the payload.
        let threads = if cfg!(miri) { 2 } else { 8 };
        let iters = if cfg!(miri) { 50 } else { 10_000 };
        thread::scope(|s| {
            for _ in 0..threads {
                let handle = arc.clone();
                s.spawn(move || {
                    for _ in 0..iters {
                        let c = handle.clone();
                        assert_eq!(c.slice(), &[1, 2, 3, 4]);
                        assert!(c.is_immortal());
                        drop(c);
                    }
                });
            }
        });
        drop(arc);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn multithreaded_clone_drop_stress() {
        let counting = Counting::new(TestHeap);
        let drops = StdArc::new(AtomicUsize::new(0));
        struct Payload(StdArc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        {
            let root = ThinArc::new_in(Payload(StdArc::clone(&drops)), 0u32..64, &counting);
            let threads = if cfg!(miri) { 2 } else { 8 };
            let iters = if cfg!(miri) { 40 } else { 2_000 };
            thread::scope(|s| {
                for t in 0..threads {
                    let shared = root.clone();
                    s.spawn(move || {
                        let mut local = StdVec::with_capacity(64);
                        for i in 0..iters {
                            let c = shared.clone();
                            assert_eq!(c.len(), 64);
                            assert_eq!(c[(i + t) & 63], ((i + t) & 63) as u32);
                            local.push(c);
                            if local.len() == 32 {
                                local.clear();
                            }
                        }
                    });
                }
            });
            assert_eq!(root.strong_count(), 1);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
        }
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        counting.assert_empty();
    }

    #[test]
    fn thin_str_roundtrip_eq_hash_and_empty_zero_alloc() {
        let counting = Counting::new(Global);
        {
            // Empty string performs ZERO heap allocations and shares the static sentinel.
            let e1 = ThinStr::from_str_in("", &counting);
            let e2 = ThinStr::from_str_in("", &counting);
            assert_eq!(counting.live(), 0);
            assert_eq!(counting.allocations(), 0);
            assert!(e1.is_empty());
            assert_eq!(e1.len(), 0);
            assert_eq!(e1.as_str(), "");
            assert!(e1.is_immortal());
            assert_eq!(e1.strong_count(), u32::MAX as usize);
            assert!(ThinStr::ptr_eq(&e1, &e2));
            assert_eq!(e1, e2);
            assert_eq!(e1, "");
            assert_eq!(hash_of(&e1), hash_of(""));
            let e3 = e1.clone();
            assert!(ThinStr::ptr_eq(&e1, &e3));
            e1.make_immortal(); // idempotent no-op

            // Non-empty UTF-8 round-trip, Eq, Hash matching `&str` (for Borrow<str>).
            let text = "hello, 世界 🚀 blaze-own!";
            let s1 = ThinStr::from_str_in(text, &counting);
            assert_eq!(counting.live(), 1);
            assert_eq!(s1.as_str(), text);
            assert_eq!(&*s1, text);
            assert_eq!(s1.as_bytes(), text.as_bytes());
            assert_eq!(s1.len(), text.len());
            assert!(!s1.is_empty());
            assert!(!s1.is_immortal());
            assert_eq!(s1, text);
            assert_eq!(hash_of(&s1), hash_of(text));

            let s2 = s1.clone();
            assert!(ThinStr::ptr_eq(&s1, &s2));
            assert_eq!(s1.strong_count(), 2);

            let s3 = ThinStr::from_str_in(text, &counting);
            assert!(!ThinStr::ptr_eq(&s1, &s3));
            assert_eq!(s1, s3);
            assert_eq!(hash_of(&s1), hash_of(&s3));

            let s4 = ThinStr::from_str_in("other", &counting);
            assert_ne!(s1, s4);
            assert!(s1 < s4);

            // Works with Alloc<'a> handle (`AThinStr`) as well.
            let erased = Erased::new(&counting);
            let a_str: AThinStr<'_> = ThinStr::from_str_in(text, erased.handle());
            assert_eq!(a_str, s1);
            assert_eq!(format!("{a_str}"), text);
            assert_eq!(format!("{a_str:?}"), format!("{text:?}"));
        }
        counting.assert_empty();
    }

    #[test]
    fn get_mut_and_share() {
        use crate::Share;

        let counting = Counting::new(Global);
        {
            let mut a = ThinArc::new_in(10u32, [1i32, 2, 3], &counting);
            {
                let (h, s) = a.get_mut().unwrap();
                *h = 20;
                s[1] = 200;
            }
            assert_eq!(*a.header(), 20);
            assert_eq!(a.slice(), &[1, 200, 3]);

            let b = a.share();
            assert!(a.get_mut().is_none());
            drop(b);
            assert!(a.get_mut().is_some());

            let s = ThinStr::from_str_in("shared", &counting);
            let s2 = s.share();
            assert!(ThinStr::ptr_eq(&s, &s2));
        }
        counting.assert_empty();

        let mut imm = ThinArc::new_in(1u32, [2u32], Global);
        imm.make_immortal();
        assert!(imm.get_mut().is_none());
    }

    #[test]
    fn try_forms_fail_cleanly_at_every_allocation() {
        every_failure(&Global, |alloc| {
            let a = ThinArc::try_new_in(123u64, [1u32, 2, 3, 4], alloc)?;
            assert_eq!(*a.header(), 123);
            assert_eq!(a.slice(), &[1, 2, 3, 4]);
            let b = ThinArcSlice::try_from_slice_in(&[10u16, 20, 30], alloc)?;
            assert_eq!(b.slice(), &[10, 20, 30]);
            let s = ThinStr::try_from_str_in("allocated string", alloc)?;
            assert_eq!(s.as_str(), "allocated string");
            let e = ThinStr::try_from_str_in("", alloc)?;
            assert_eq!(e.as_str(), "");
            Ok::<(), crate::AllocFailure>(())
        })
        .unwrap();
    }
}
