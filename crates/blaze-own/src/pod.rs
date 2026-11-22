//! Safe plain-old-data (`Pod`), zero-initialization (`Zeroable`), checked bit-pattern decoding
//! (`CheckedBitPattern`), and transparent wrapper conversions (`TransparentWrapper`) — `#![no_std]`,
//! zero third-party dependencies, and zero `core::mem::transmute`.
//!
//! This module is Blaze's canonical replacement for raw `core::mem::transmute` /
//! `core::mem::transmute_copy` / `core::mem::zeroed` across `blaze-own`, `blaze-core`, and `bstd`:
//!
//! | need | use | replaces |
//! |---|---|---|
//! | zero-initialized value / slice / `Vec` | [`zeroed`], [`write_zeroes`], [`fill_zeroes`], [`zeroed_vec_in`] | `unsafe { core::mem::zeroed() }` |
//! | value bit-cast (compile-time size check) | [`cast`], [`must_cast`], [`try_cast`] | `unsafe { core::mem::transmute(x) }` |
//! | reference bit-cast (alignment + size checked) | [`cast_ref`], [`try_cast_ref`], [`cast_mut`], [`try_cast_mut`] | `unsafe { &*(p as *const U) }` |
//! | view value or slice as raw bytes | [`bytes_of`], [`bytes_of_mut`], [`bytes_of_slice`], [`bytes_of_slice_mut`] | `unsafe { slice::from_raw_parts(p.cast(), size_of::<T>()) }` |
//! | decode value or slice from raw bytes | [`from_bytes`], [`try_from_bytes`], [`pod_read_unaligned`], [`try_pod_read_unaligned`] | `unsafe { ptr::read_unaligned(...) }` / `transmute` |
//! | slice element reinterpretation | [`cast_slice`], [`try_cast_slice`], [`cast_slice_mut`], [`try_cast_slice_mut`] | `unsafe { slice::from_raw_parts(...) }` |
//! | fallible decode for types with niches (`bool`, `char`, `NonZero*`, enums) | [`CheckedBitPattern`], [`try_checked_from_bytes`], [`try_checked_read_unaligned`], [`try_checked_cast`] | `unsafe { core::mem::transmute(...) }` |
//! | zero-cost `#[repr(transparent)]` newtype peeling/wrapping | [`TransparentWrapper`] | `unsafe { core::mem::transmute(...) }` |
//!
//! # Soundness & Miri
//!
//! Every `unsafe` operation inside this module upholds strict Rust aliasing, provenance, alignment,
//! and initialization invariants (no `core::mem::transmute` or `core::mem::transmute_copy` is used
//! anywhere). All size-sensitive value casts enforce `size_of::<A>() == size_of::<B>()` at compile
//! time via `const { assert!(...) }` where applicable.

use core::alloc::Allocator;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};
use core::num::{
    NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI128, NonZeroIsize, NonZeroU8, NonZeroU16,
    NonZeroU32, NonZeroU64, NonZeroU128, NonZeroUsize, Wrapping,
};
use core::ptr;
use core::slice;

use crate::{AllocFailure, Vec};

/// Why a fallible `pod` conversion was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CastError {
    /// Input and output types have different sizes (or byte slice length != `size_of::<T>()`).
    SizeMismatch,
    /// Input reference alignment is strictly less than output type alignment and the pointer is
    /// not aligned for the target type.
    TargetAlignmentGreaterAndInputNotAligned,
    /// Output slice element size does not evenly divide the input slice byte length.
    OutputSliceWouldHaveSlop,
    /// Input slice is not aligned to the output element type.
    AlignmentMismatch,
    /// Raw bits do not represent a valid value of the target [`CheckedBitPattern`] type.
    InvalidBitPattern,
}

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CastError::SizeMismatch => f.write_str("pod cast refused: size mismatch"),
            CastError::TargetAlignmentGreaterAndInputNotAligned => {
                f.write_str("pod cast refused: pointer is not aligned for the target type")
            }
            CastError::OutputSliceWouldHaveSlop => {
                f.write_str("pod slice cast refused: byte length is not a multiple of target element size")
            }
            CastError::AlignmentMismatch => f.write_str("pod slice cast refused: alignment mismatch"),
            CastError::InvalidBitPattern => {
                f.write_str("pod checked cast refused: bit pattern is invalid for target type")
            }
        }
    }
}

impl core::error::Error for CastError {}

// -------------------------------------------------------------------------------------------------
// Core Marker Traits
// -------------------------------------------------------------------------------------------------

/// Types for which an all-zero byte pattern is a valid, properly initialized value.
///
/// # Safety
///
/// Implementing this trait guarantees that `MaybeUninit::<Self>::zeroed().assume_init()` produces
/// a valid value of `Self` with no undefined behavior (no non-null references, no `NonZero*`
/// fields, no enums without a `0` discriminant, etc.).
pub unsafe trait Zeroable: Sized {}

/// Types whose entire in-memory byte representation (`size_of::<Self>()` bytes) contains no
/// uninitialized bytes (such as struct padding) and no interior mutability (`UnsafeCell`).
///
/// # Safety
///
/// Implementing this trait guarantees:
/// - `Self` has no padding bytes (every byte in `0..size_of::<Self>()` is always initialized),
/// - `Self` contains no `UnsafeCell` (so reading bytes through `&Self` cannot data-race),
/// - `Self: Copy + 'static` (no drop glue, no non-`'static` lifetimes).
pub unsafe trait NoUninit: Sized + Copy + 'static {}

/// Types for which **any** initialized byte pattern of length `size_of::<Self>()` is a valid
/// value of `Self`.
///
/// # Safety
///
/// Implementing this trait guarantees:
/// - Every bit pattern of `size_of::<Self>()` initialized bytes is a valid instance of `Self`,
/// - `Self` contains no `UnsafeCell` and no references/pointers with validity invariants,
/// - `Self: Zeroable + Copy + 'static`.
pub unsafe trait AnyBitPattern: Zeroable + Copy + 'static {}

/// Plain-Old-Data types: both [`NoUninit`] (no padding, safe to read as bytes) and
/// [`AnyBitPattern`] (any byte pattern is valid, safe to write arbitrary bytes into).
///
/// # Safety
///
/// Implementing this trait guarantees all invariants of [`Zeroable`], [`NoUninit`], and
/// [`AnyBitPattern`]. In particular, any `&Self` / `&mut Self` may be safely reinterpreted as
/// `&[u8]` / `&mut [u8]` and vice versa (subject to size and alignment).
pub unsafe trait Pod: Zeroable + NoUninit + AnyBitPattern {}

/// Types that may have invalid bit patterns (such as `bool`, `char`, `NonZero*`, or `#[repr(u8)]`
/// enums), but can validate raw bits before constructing a value safely.
///
/// # Safety
///
/// Implementing this trait guarantees:
/// - `size_of::<Self>() == size_of::<Self::Bits>()` and `align_of::<Self>() == align_of::<Self::Bits>()`,
/// - Whenever `Self::is_valid_bit_pattern(&bits)` returns `true`, reinterpreting `bits` as `Self`
///   produces a completely valid value of `Self` with no UB.
pub unsafe trait CheckedBitPattern: Copy + 'static {
    /// Underlying [`AnyBitPattern`] representation with identical size and alignment.
    type Bits: AnyBitPattern;

    /// Returns `true` iff `bits` encodes a valid value of `Self`.
    fn is_valid_bit_pattern(bits: &Self::Bits) -> bool;
}

/// Marker for `#[repr(transparent)]` wrappers around `Inner`, enabling zero-cost safe wrapping
/// and peeling of values, references, and slices without `transmute`.
///
/// # Safety
///
/// `Self` must be `#[repr(transparent)]` over `Inner` (identical ABI, size, and alignment), and
/// must impose no additional safety invariants beyond those of `Inner`.
pub unsafe trait TransparentWrapper<Inner: ?Sized> {
    #[inline]
    fn wrap(s: Inner) -> Self
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        let s = MaybeUninit::new(s);
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner` with identical size & alignment,
        // and `MaybeUninit` prevents `s` from being dropped after moving ownership into `Self`.
        unsafe { ptr::read(s.as_ptr().cast::<Self>()) }
    }

    #[inline]
    fn peel(self) -> Inner
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        let me = MaybeUninit::new(self);
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner`; `MaybeUninit` prevents double-drop.
        unsafe { ptr::read(me.as_ptr().cast::<Inner>()) }
    }

    #[inline]
    fn wrap_ref(s: &Inner) -> &Self
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner` with identical layout & lifetime.
        unsafe { &*ptr::from_ref(s).cast::<Self>() }
    }

    #[inline]
    fn peel_ref(&self) -> &Inner
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner` with identical layout & lifetime.
        unsafe { &*ptr::from_ref(self).cast::<Inner>() }
    }

    #[inline]
    fn wrap_mut(s: &mut Inner) -> &mut Self
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner` with identical layout & exclusivity.
        unsafe { &mut *ptr::from_mut(s).cast::<Self>() }
    }

    #[inline]
    fn peel_mut(&mut self) -> &mut Inner
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner` with identical layout & exclusivity.
        unsafe { &mut *ptr::from_mut(self).cast::<Inner>() }
    }

    #[inline]
    fn wrap_slice(s: &[Inner]) -> &[Self]
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner`, so `[Inner]` and `[Self]` have
        // identical stride, size, alignment, and validity for all `s.len()` elements.
        unsafe { slice::from_raw_parts(s.as_ptr().cast::<Self>(), s.len()) }
    }

    #[inline]
    fn peel_slice(s: &[Self]) -> &[Inner]
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner`.
        unsafe { slice::from_raw_parts(s.as_ptr().cast::<Inner>(), s.len()) }
    }

    #[inline]
    fn wrap_slice_mut(s: &mut [Inner]) -> &mut [Self]
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner`.
        unsafe { slice::from_raw_parts_mut(s.as_mut_ptr().cast::<Self>(), s.len()) }
    }

    #[inline]
    fn peel_slice_mut(s: &mut [Self]) -> &mut [Inner]
    where
        Self: Sized,
        Inner: Sized,
    {
        const {
            assert!(size_of::<Self>() == size_of::<Inner>(), "TransparentWrapper size mismatch");
            assert!(align_of::<Self>() == align_of::<Inner>(), "TransparentWrapper align mismatch");
        }
        // SAFETY: `Self` is `#[repr(transparent)]` over `Inner`.
        unsafe { slice::from_raw_parts_mut(s.as_mut_ptr().cast::<Inner>(), s.len()) }
    }
}

// -------------------------------------------------------------------------------------------------
// Safe Operations
// -------------------------------------------------------------------------------------------------

/// Returns an all-zero value of `T`. Safe replacement for `unsafe { core::mem::zeroed() }`.
#[inline]
#[must_use]
pub fn zeroed<T: Zeroable>() -> T {
    // SAFETY: `T: Zeroable` guarantees the all-zero bit pattern is a valid value of `T`.
    unsafe { MaybeUninit::<T>::zeroed().assume_init() }
}

/// Overwrites `dst` with all-zero bytes in place.
#[inline]
pub fn write_zeroes<T: Zeroable>(dst: &mut T) {
    *dst = zeroed();
}

/// Overwrites every element of `dst` with zeroes in place.
#[inline]
pub fn fill_zeroes<T: Zeroable>(dst: &mut [T]) {
    if size_of::<T>() == 0 || dst.is_empty() {
        return;
    }
    if core::mem::needs_drop::<T>() {
        for item in dst.iter_mut() {
            *item = zeroed();
        }
    } else {
        // SAFETY: `T: Zeroable` and `!needs_drop::<T>()`, so writing `0` bytes over `dst.len()`
        // elements produces valid zero-initialized `T` values without leaking any drop glue.
        unsafe { ptr::write_bytes(dst.as_mut_ptr(), 0, dst.len()) };
    }
}

/// Allocates a [`Vec<T, A>`] of `len` zeroed elements in `alloc`, returning [`AllocFailure`] on OOM.
#[inline]
pub fn try_zeroed_vec_in<T: Zeroable, A: Allocator>(len: usize, alloc: A) -> Result<Vec<T, A>, AllocFailure> {
    let mut v = Vec::try_with_capacity_in(len, alloc)?;
    if size_of::<T>() > 0 && len > 0 {
        let spare = v.spare_capacity_mut();
        // SAFETY: `spare` points to at least `len` writable `MaybeUninit<T>` slots and
        // `T: Zeroable` guarantees all-zero bytes are valid initialized `T` values.
        unsafe {
            ptr::write_bytes(spare.as_mut_ptr(), 0, len);
            v.set_len(len);
        }
    } else if len > 0 {
        // SAFETY: `T` is zero-sized and `Zeroable`, so `len` zero-sized elements are initialized.
        unsafe { v.set_len(len) };
    }
    Ok(v)
}

/// Allocates a [`Vec<T, A>`] of `len` zeroed elements in `alloc`.
#[inline]
#[must_use]
pub fn zeroed_vec_in<T: Zeroable, A: Allocator>(len: usize, alloc: A) -> Vec<T, A> {
    try_zeroed_vec_in(len, alloc).unwrap_or_else(|e| panic!("zeroed_vec_in failed: {e}"))
}

/// Compile-time-checked value bit-cast from `A` to `B` (fails to compile if `size_of::<A>() != size_of::<B>()`).
///
/// Safe replacement for `unsafe { core::mem::transmute::<A, B>(a) }`.
#[inline]
#[must_use]
pub fn cast<A: NoUninit, B: AnyBitPattern>(a: A) -> B {
    const {
        assert!(size_of::<A>() == size_of::<B>(), "pod::cast requires size_of::<A>() == size_of::<B>()");
    }
    // SAFETY: `A: NoUninit` guarantees all `size_of::<A>()` bytes are initialized; `B: AnyBitPattern`
    // guarantees any bit pattern of `size_of::<B>() == size_of::<A>()` bytes is a valid `B`;
    // `read_unaligned` handles arbitrary alignment differences safely.
    unsafe { ptr::read_unaligned(ptr::from_ref(&a).cast::<B>()) }
}

/// Alias for [`cast`] emphasizing the compile-time size proof.
#[inline]
#[must_use]
pub fn must_cast<A: NoUninit, B: AnyBitPattern>(a: A) -> B {
    cast(a)
}

/// Fallible value bit-cast from `A` to `B`, returning [`CastError::SizeMismatch`] if sizes differ.
#[inline]
pub fn try_cast<A: NoUninit, B: AnyBitPattern>(a: A) -> Result<B, CastError> {
    if size_of::<A>() != size_of::<B>() {
        return Err(CastError::SizeMismatch);
    }
    // SAFETY: `size_of::<A>() == size_of::<B>()`, `A: NoUninit`, `B: AnyBitPattern`, and
    // `read_unaligned` does not require `align_of::<B>() <= align_of::<A>()`.
    Ok(unsafe { ptr::read_unaligned(ptr::from_ref(&a).cast::<B>()) })
}

/// Fallible reference bit-cast `&A -> &B`, checking both size and pointer alignment.
#[inline]
pub fn try_cast_ref<A: NoUninit, B: AnyBitPattern>(a: &A) -> Result<&B, CastError> {
    if size_of::<A>() != size_of::<B>() {
        return Err(CastError::SizeMismatch);
    }
    let ptr = ptr::from_ref(a).cast::<B>();
    if !ptr.is_aligned() {
        return Err(CastError::TargetAlignmentGreaterAndInputNotAligned);
    }
    // SAFETY: `size_of::<A>() == size_of::<B>()`, `ptr` is properly aligned for `B`, `A: NoUninit`
    // guarantees all bytes are initialized with no interior mutability, and `B: AnyBitPattern`
    // guarantees the bytes form a valid `B` for the shared lifetime of `a`.
    Ok(unsafe { &*ptr })
}

/// Panicking reference bit-cast `&A -> &B`.
#[inline]
#[must_use]
pub fn cast_ref<A: NoUninit, B: AnyBitPattern>(a: &A) -> &B {
    match try_cast_ref(a) {
        Ok(b) => b,
        Err(e) => panic!("{e}"),
    }
}

/// Fallible mutable reference bit-cast `&mut A -> &mut B`, checking size and pointer alignment.
#[inline]
pub fn try_cast_mut<A: Pod, B: Pod>(a: &mut A) -> Result<&mut B, CastError> {
    if size_of::<A>() != size_of::<B>() {
        return Err(CastError::SizeMismatch);
    }
    let ptr = ptr::from_mut(a).cast::<B>();
    if !ptr.is_aligned() {
        return Err(CastError::TargetAlignmentGreaterAndInputNotAligned);
    }
    // SAFETY: `size_of::<A>() == size_of::<B>()`, `ptr` is aligned for `B`, and both `A` and `B`
    // are `Pod` (so writes through `&mut B` leave valid initialized bytes for `A` too).
    Ok(unsafe { &mut *ptr })
}

/// Panicking mutable reference bit-cast `&mut A -> &mut B`.
#[inline]
#[must_use]
pub fn cast_mut<A: Pod, B: Pod>(a: &mut A) -> &mut B {
    match try_cast_mut(a) {
        Ok(b) => b,
        Err(e) => panic!("{e}"),
    }
}

/// Reinterprets `&T` as a byte slice `&[u8]` of length `size_of::<T>()`.
#[inline]
#[must_use]
pub fn bytes_of<T: NoUninit>(t: &T) -> &[u8] {
    // SAFETY: `T: NoUninit` guarantees all `size_of::<T>()` bytes are initialized and contain no
    // `UnsafeCell`; `u8` has alignment 1.
    unsafe { slice::from_raw_parts(ptr::from_ref(t).cast::<u8>(), size_of::<T>()) }
}

/// Reinterprets `&mut T` as a mutable byte slice `&mut [u8]` of length `size_of::<T>()`.
#[inline]
#[must_use]
pub fn bytes_of_mut<T: Pod>(t: &mut T) -> &mut [u8] {
    // SAFETY: `T: Pod` guarantees all bytes are initialized and any byte mutation leaves a valid `T`.
    unsafe { slice::from_raw_parts_mut(ptr::from_mut(t).cast::<u8>(), size_of::<T>()) }
}

/// Reinterprets `&[T]` as a contiguous byte slice `&[u8]`.
#[inline]
#[must_use]
pub fn bytes_of_slice<T: NoUninit>(s: &[T]) -> &[u8] {
    let byte_len = core::mem::size_of_val(s);
    // SAFETY: `s` is a valid slice of `s.len()` initialized `NoUninit` elements totaling `byte_len` bytes.
    unsafe { slice::from_raw_parts(s.as_ptr().cast::<u8>(), byte_len) }
}

/// Reinterprets `&mut [T]` as a contiguous mutable byte slice `&mut [u8]`.
#[inline]
#[must_use]
pub fn bytes_of_slice_mut<T: Pod>(s: &mut [T]) -> &mut [u8] {
    let byte_len = core::mem::size_of_val(s);
    // SAFETY: `s` is a valid mutable slice of `Pod` elements totaling `byte_len` bytes.
    unsafe { slice::from_raw_parts_mut(s.as_mut_ptr().cast::<u8>(), byte_len) }
}

/// Fallible borrow of `bytes` as `&T` (requires exact length and target alignment).
#[inline]
pub fn try_from_bytes<T: AnyBitPattern>(bytes: &[u8]) -> Result<&T, CastError> {
    if bytes.len() != size_of::<T>() {
        return Err(CastError::SizeMismatch);
    }
    let ptr = bytes.as_ptr().cast::<T>();
    if !ptr.is_aligned() {
        return Err(CastError::TargetAlignmentGreaterAndInputNotAligned);
    }
    // SAFETY: `bytes.len() == size_of::<T>()`, `ptr` is aligned for `T`, and `T: AnyBitPattern`.
    Ok(unsafe { &*ptr })
}

/// Panicking borrow of `bytes` as `&T`.
#[inline]
#[must_use]
pub fn from_bytes<T: AnyBitPattern>(bytes: &[u8]) -> &T {
    match try_from_bytes(bytes) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

/// Fallible mutable borrow of `bytes` as `&mut T` (requires exact length and target alignment).
#[inline]
pub fn try_from_bytes_mut<T: Pod>(bytes: &mut [u8]) -> Result<&mut T, CastError> {
    if bytes.len() != size_of::<T>() {
        return Err(CastError::SizeMismatch);
    }
    let ptr = bytes.as_mut_ptr().cast::<T>();
    if !ptr.is_aligned() {
        return Err(CastError::TargetAlignmentGreaterAndInputNotAligned);
    }
    // SAFETY: `bytes.len() == size_of::<T>()`, `ptr` is aligned for `T`, and `T: Pod`.
    Ok(unsafe { &mut *ptr })
}

/// Panicking mutable borrow of `bytes` as `&mut T`.
#[inline]
#[must_use]
pub fn from_bytes_mut<T: Pod>(bytes: &mut [u8]) -> &mut T {
    match try_from_bytes_mut(bytes) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

/// Reads a `T` by value from `bytes` regardless of alignment (requires `bytes.len() == size_of::<T>()`).
#[inline]
pub fn try_pod_read_unaligned<T: AnyBitPattern>(bytes: &[u8]) -> Result<T, CastError> {
    if bytes.len() != size_of::<T>() {
        return Err(CastError::SizeMismatch);
    }
    // SAFETY: `bytes` holds `size_of::<T>()` initialized bytes and `T: AnyBitPattern`.
    Ok(unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

/// Panicking unaligned read of `T` from `bytes`.
#[inline]
#[must_use]
pub fn pod_read_unaligned<T: AnyBitPattern>(bytes: &[u8]) -> T {
    match try_pod_read_unaligned(bytes) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

/// Fallible slice cast `&[A] -> &[B]`.
#[inline]
pub fn try_cast_slice<A: NoUninit, B: AnyBitPattern>(a: &[A]) -> Result<&[B], CastError> {
    if size_of::<A>() == 0 || size_of::<B>() == 0 {
        return if size_of::<A>() == size_of::<B>() {
            // SAFETY: both `A` and `B` are zero-sized `Copy` types; a dangling aligned slice of
            // `a.len()` elements is always valid.
            Ok(unsafe { slice::from_raw_parts(ptr::NonNull::<B>::dangling().as_ptr(), a.len()) })
        } else {
            Err(CastError::SizeMismatch)
        };
    }
    let byte_len = core::mem::size_of_val(a);
    if !byte_len.is_multiple_of(size_of::<B>()) {
        return Err(CastError::OutputSliceWouldHaveSlop);
    }
    let ptr = a.as_ptr().cast::<B>();
    if !ptr.is_aligned() {
        return Err(CastError::AlignmentMismatch);
    }
    let new_len = byte_len / size_of::<B>();
    // SAFETY: `ptr` is aligned for `B`, `new_len * size_of::<B>() == byte_len`, `A: NoUninit`,
    // and `B: AnyBitPattern`.
    Ok(unsafe { slice::from_raw_parts(ptr, new_len) })
}

/// Panicking slice cast `&[A] -> &[B]`.
#[inline]
#[must_use]
pub fn cast_slice<A: NoUninit, B: AnyBitPattern>(a: &[A]) -> &[B] {
    match try_cast_slice(a) {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    }
}

/// Fallible mutable slice cast `&mut [A] -> &mut [B]`.
#[inline]
pub fn try_cast_slice_mut<A: Pod, B: Pod>(a: &mut [A]) -> Result<&mut [B], CastError> {
    if size_of::<A>() == 0 || size_of::<B>() == 0 {
        return if size_of::<A>() == size_of::<B>() {
            // SAFETY: both `A` and `B` are zero-sized `Pod` types.
            Ok(unsafe { slice::from_raw_parts_mut(ptr::NonNull::<B>::dangling().as_ptr(), a.len()) })
        } else {
            Err(CastError::SizeMismatch)
        };
    }
    let byte_len = core::mem::size_of_val(a);
    if !byte_len.is_multiple_of(size_of::<B>()) {
        return Err(CastError::OutputSliceWouldHaveSlop);
    }
    let ptr = a.as_mut_ptr().cast::<B>();
    if !ptr.is_aligned() {
        return Err(CastError::AlignmentMismatch);
    }
    let new_len = byte_len / size_of::<B>();
    // SAFETY: `ptr` is aligned for `B`, `new_len * size_of::<B>() == byte_len`, and both `A` and `B`
    // are `Pod`.
    Ok(unsafe { slice::from_raw_parts_mut(ptr, new_len) })
}

/// Panicking mutable slice cast `&mut [A] -> &mut [B]`.
#[inline]
#[must_use]
pub fn cast_slice_mut<A: Pod, B: Pod>(a: &mut [A]) -> &mut [B] {
    match try_cast_slice_mut(a) {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    }
}

/// Validates and borrows `bytes` as `&T` for a [`CheckedBitPattern`] type `T`.
#[inline]
pub fn try_checked_from_bytes<T: CheckedBitPattern>(bytes: &[u8]) -> Result<&T, CastError> {
    const {
        assert!(size_of::<T>() == size_of::<T::Bits>(), "CheckedBitPattern size mismatch");
        assert!(align_of::<T>() == align_of::<T::Bits>(), "CheckedBitPattern align mismatch");
    }
    let bits: &T::Bits = try_from_bytes(bytes)?;
    if !T::is_valid_bit_pattern(bits) {
        return Err(CastError::InvalidBitPattern);
    }
    // SAFETY: `bits` is aligned and sized for `T` and `T::is_valid_bit_pattern(bits)` returned `true`.
    Ok(unsafe { &*ptr::from_ref(bits).cast::<T>() })
}

/// Validates and reads `T` by value from unaligned `bytes` for a [`CheckedBitPattern`] type `T`.
#[inline]
pub fn try_checked_read_unaligned<T: CheckedBitPattern>(bytes: &[u8]) -> Result<T, CastError> {
    const {
        assert!(size_of::<T>() == size_of::<T::Bits>(), "CheckedBitPattern size mismatch");
        assert!(align_of::<T>() == align_of::<T::Bits>(), "CheckedBitPattern align mismatch");
    }
    let bits: T::Bits = try_pod_read_unaligned(bytes)?;
    if !T::is_valid_bit_pattern(&bits) {
        return Err(CastError::InvalidBitPattern);
    }
    // SAFETY: `bits` has the exact size/alignment of `T` and passed `T::is_valid_bit_pattern`.
    Ok(unsafe { ptr::read(ptr::from_ref(&bits).cast::<T>()) })
}

/// Validates and bit-casts `a: A` to `B: CheckedBitPattern` (e.g. `u8 -> bool`, `u32 -> char`, `u32 -> NonZeroU32`).
#[inline]
pub fn try_checked_cast<A: NoUninit, B: CheckedBitPattern>(a: A) -> Result<B, CastError> {
    const {
        assert!(size_of::<B>() == size_of::<B::Bits>(), "CheckedBitPattern size mismatch");
        assert!(align_of::<B>() == align_of::<B::Bits>(), "CheckedBitPattern align mismatch");
    }
    let bits: B::Bits = try_cast(a)?;
    if !B::is_valid_bit_pattern(&bits) {
        return Err(CastError::InvalidBitPattern);
    }
    // SAFETY: `bits` has identical layout to `B` and passed `B::is_valid_bit_pattern`.
    Ok(unsafe { ptr::read(ptr::from_ref(&bits).cast::<B>()) })
}

/// Panicking checked bit-cast `A -> B: CheckedBitPattern`.
#[inline]
#[must_use]
pub fn checked_cast<A: NoUninit, B: CheckedBitPattern>(a: A) -> B {
    match try_checked_cast(a) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

// -------------------------------------------------------------------------------------------------
// Built-in Implementations
// -------------------------------------------------------------------------------------------------

macro_rules! impl_primitive_pod {
    ($($t:ty),* $(,)?) => {
        $(
            // SAFETY: primitive numeric / unit type with no padding, no interior mutability,
            // and every bit pattern (including all-zeroes) is valid.
            unsafe impl Zeroable for $t {}
            unsafe impl NoUninit for $t {}
            unsafe impl AnyBitPattern for $t {}
            unsafe impl Pod for $t {}
        )*
    };
}

impl_primitive_pod!((), u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64);

// SAFETY: an array `[T; N]` has no padding between elements and inherits `T`'s validity invariants.
unsafe impl<T: Zeroable, const N: usize> Zeroable for [T; N] {}
unsafe impl<T: NoUninit, const N: usize> NoUninit for [T; N] {}
unsafe impl<T: AnyBitPattern, const N: usize> AnyBitPattern for [T; N] {}
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

// SAFETY: `Wrapping<T>` is `#[repr(transparent)]` over `T`.
unsafe impl<T: Zeroable> Zeroable for Wrapping<T> {}
unsafe impl<T: NoUninit> NoUninit for Wrapping<T> {}
unsafe impl<T: AnyBitPattern> AnyBitPattern for Wrapping<T> {}
unsafe impl<T: Pod> Pod for Wrapping<T> {}
unsafe impl<T> TransparentWrapper<T> for Wrapping<T> {}

// SAFETY: `MaybeUninit<T>` has no validity invariant on its bytes, so all-zeroes is always valid.
unsafe impl<T> Zeroable for MaybeUninit<T> {}

// SAFETY: `PhantomData<T>` is zero-sized (0 bytes).
unsafe impl<T: ?Sized> Zeroable for PhantomData<T> {}
unsafe impl<T: ?Sized + 'static> NoUninit for PhantomData<T> {}
unsafe impl<T: ?Sized + 'static> AnyBitPattern for PhantomData<T> {}
unsafe impl<T: ?Sized + 'static> Pod for PhantomData<T> {}

// SAFETY: a null raw pointer (`0`) is always a valid raw pointer value.
unsafe impl<T> Zeroable for *const T {}
unsafe impl<T> Zeroable for *mut T {}

// SAFETY: `bool` is 1 byte with no padding; valid bit patterns are `0` and `1`.
unsafe impl NoUninit for bool {}
unsafe impl CheckedBitPattern for bool {
    type Bits = u8;
    #[inline]
    fn is_valid_bit_pattern(bits: &u8) -> bool {
        *bits <= 1
    }
}

// SAFETY: `char` is 4 bytes with no padding; valid bit patterns are Unicode scalar values.
unsafe impl NoUninit for char {}
unsafe impl CheckedBitPattern for char {
    type Bits = u32;
    #[inline]
    fn is_valid_bit_pattern(bits: &u32) -> bool {
        core::char::from_u32(*bits).is_some()
    }
}

macro_rules! impl_nonzero_checked {
    ($($nz:ty => $raw:ty),* $(,)?) => {
        $(
            // SAFETY: `NonZero*` is `#[repr(transparent)]` over `$raw` with no padding;
            // all non-zero bit patterns are valid, and `Option<NonZero*>` uses `0` for `None`.
            unsafe impl NoUninit for $nz {}
            unsafe impl Zeroable for Option<$nz> {}
            unsafe impl CheckedBitPattern for $nz {
                type Bits = $raw;
                #[inline]
                fn is_valid_bit_pattern(bits: &$raw) -> bool {
                    *bits != 0
                }
            }
        )*
    };
}

impl_nonzero_checked!(
    NonZeroU8 => u8,
    NonZeroU16 => u16,
    NonZeroU32 => u32,
    NonZeroU64 => u64,
    NonZeroU128 => u128,
    NonZeroUsize => usize,
    NonZeroI8 => i8,
    NonZeroI16 => i16,
    NonZeroI32 => i32,
    NonZeroI64 => i64,
    NonZeroI128 => i128,
    NonZeroIsize => isize,
);

// -------------------------------------------------------------------------------------------------
// Compile-Time-Checked Declarative Macros for Structs
// -------------------------------------------------------------------------------------------------

/// Implements [`Zeroable`] for a struct while verifying at compile time that every field is
/// listed exhaustively, matches its declared type, and implements [`Zeroable`].
#[macro_export]
macro_rules! impl_zeroable {
    ($ty:path { $($field:ident : $fty:ty),* $(,)? }) => {
        const _: () = {
            const fn assert_zeroable<T: $crate::pod::Zeroable>() {}
            #[allow(dead_code)]
            fn assert_fields(v: $ty) {
                let $ty { $($field),* } = v;
                $(
                    let _: &$fty = &$field;
                )*
            }
            $(assert_zeroable::<$fty>();)*
        };
        // SAFETY: every field of `$ty` is exhaustively verified `Zeroable` at compile time above.
        unsafe impl $crate::pod::Zeroable for $ty {}
    };
    ($ty:ident ( $($bind:ident : $fty:ty),* $(,)? )) => {
        const _: () = {
            const fn assert_zeroable<T: $crate::pod::Zeroable>() {}
            #[allow(dead_code)]
            fn assert_fields(v: $ty) {
                let $ty($($bind),*) = v;
                $(
                    let _: &$fty = &$bind;
                )*
            }
            $(assert_zeroable::<$fty>();)*
        };
        // SAFETY: every field of `$ty` is exhaustively verified `Zeroable` at compile time above.
        unsafe impl $crate::pod::Zeroable for $ty {}
    };
}

/// Implements [`Zeroable`], [`NoUninit`], [`AnyBitPattern`], and [`Pod`] for a `#[repr(C)]` struct
/// (named-field or tuple struct), proving at **compile time** that:
/// 1. Every field is listed exhaustively with its exact type,
/// 2. Every field type implements [`Pod`] and `$ty: Copy + 'static`, AND
/// 3. `size_of::<$ty>() == sum(size_of::<$fty>())` (zero hidden padding bytes!).
///
/// If a struct has even 1 byte of alignment padding or an omitted field, this macro refuses to
/// compile.
#[macro_export]
macro_rules! impl_pod {
    ($ty:path { $($field:ident : $fty:ty),* $(,)? }) => {
        const _: () = {
            const fn assert_pod<T: $crate::pod::Pod>() {}
            const fn assert_copy<T: ::core::marker::Copy + 'static>() {}
            #[allow(dead_code)]
            fn assert_fields(v: $ty) {
                let $ty { $($field),* } = v;
                $(
                    let _: &$fty = &$field;
                )*
            }
            assert_copy::<$ty>();
            $(assert_pod::<$fty>();)*
            let fields_size: usize = 0 $(+ ::core::mem::size_of::<$fty>())*;
            assert!(
                ::core::mem::size_of::<$ty>() == fields_size,
                "impl_pod! refused: struct has hidden padding bytes (size_of != sum of field sizes)"
            );
        };
        // SAFETY: compile-time proof above exhaustively verified `$ty: Copy + 'static`, every field
        // is `Pod`, and total size equals the exact sum of field sizes (zero padding bytes).
        unsafe impl $crate::pod::Zeroable for $ty {}
        unsafe impl $crate::pod::NoUninit for $ty {}
        unsafe impl $crate::pod::AnyBitPattern for $ty {}
        unsafe impl $crate::pod::Pod for $ty {}
    };
    ($ty:ident ( $($bind:ident : $fty:ty),* $(,)? )) => {
        const _: () = {
            const fn assert_pod<T: $crate::pod::Pod>() {}
            const fn assert_copy<T: ::core::marker::Copy + 'static>() {}
            #[allow(dead_code)]
            fn assert_fields(v: $ty) {
                let $ty($($bind),*) = v;
                $(
                    let _: &$fty = &$bind;
                )*
            }
            assert_copy::<$ty>();
            $(assert_pod::<$fty>();)*
            let fields_size: usize = 0 $(+ ::core::mem::size_of::<$fty>())*;
            assert!(
                ::core::mem::size_of::<$ty>() == fields_size,
                "impl_pod! refused: struct has hidden padding bytes (size_of != sum of field sizes)"
            );
        };
        // SAFETY: compile-time proof above exhaustively verified `$ty: Copy + 'static`, every field
        // is `Pod`, and total size equals the exact sum of field sizes (zero padding bytes).
        unsafe impl $crate::pod::Zeroable for $ty {}
        unsafe impl $crate::pod::NoUninit for $ty {}
        unsafe impl $crate::pod::AnyBitPattern for $ty {}
        unsafe impl $crate::pod::Pod for $ty {}
    };
}

/// Implements [`TransparentWrapper`] for a `#[repr(transparent)]` single-field tuple newtype,
/// verifying identical size, alignment, and exact inner field type at compile time.
#[macro_export]
macro_rules! impl_transparent {
    ($outer:ident ( $inner:ty )) => {
        const _: () = {
            #[allow(dead_code)]
            fn assert_inner(v: $outer) {
                let $outer(inner) = v;
                let _: &$inner = &inner;
            }
            assert!(
                ::core::mem::size_of::<$outer>() == ::core::mem::size_of::<$inner>()
                    && ::core::mem::align_of::<$outer>() == ::core::mem::align_of::<$inner>(),
                "impl_transparent! refused: wrapper layout differs from inner type"
            );
        };
        // SAFETY: `$outer` is a single-field `#[repr(transparent)]` wrapper around `$inner` with
        // compile-time-verified identical size and alignment.
        unsafe impl $crate::pod::TransparentWrapper<$inner> for $outer {}
    };
}

// -------------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use core::num::{NonZeroU32, Wrapping};

    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Header {
        magic: u32,
        version: u16,
        flags: u16,
    }
    impl_pod!(Header { magic: u32, version: u16, flags: u16 });

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Word(u64);
    impl_transparent!(Word(u64));

    #[repr(C, align(16))]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct AlignedBlock([u8; 16]);
    impl_pod!(AlignedBlock(bytes: [u8; 16]));

    #[repr(u8)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[allow(dead_code)]
    enum Color {
        Red = 0,
        Green = 1,
        Blue = 2,
    }
    unsafe impl CheckedBitPattern for Color {
        type Bits = u8;
        fn is_valid_bit_pattern(bits: &u8) -> bool {
            *bits <= 2
        }
    }

    #[test]
    fn primitive_cast_and_bytes_roundtrip() {
        let bits: u32 = cast(1.5_f32);
        assert_eq!(bits, 1.5_f32.to_bits());
        let f: f32 = must_cast(bits);
        assert_eq!(f, 1.5_f32);

        let arr: [u8; 4] = cast(0x0102_0304_u32);
        assert_eq!(arr, 0x0102_0304_u32.to_ne_bytes());

        let mut hdr = Header { magic: 0x424C_5A31, version: 2, flags: 7 };
        let b = bytes_of(&hdr);
        assert_eq!(b.len(), 8);
        let decoded: Header = pod_read_unaligned(b);
        assert_eq!(decoded, hdr);
        assert_eq!(*from_bytes::<Header>(b), hdr);

        bytes_of_mut(&mut hdr).fill(0);
        assert_eq!(hdr, zeroed::<Header>());

        let blocks = [AlignedBlock([0xAB; 16]), AlignedBlock([0xCD; 16])];
        let flat = bytes_of_slice(&blocks);
        assert_eq!(flat.len(), 32);
        assert_eq!(flat[0], 0xAB);
        assert_eq!(flat[16], 0xCD);
    }

    #[test]
    fn slice_casts_and_error_cases() {
        let words: [u32; 4] = [1, 2, 3, 4];
        let raw: &[u8] = cast_slice(&words);
        assert_eq!(raw.len(), 16);
        assert_eq!(cast_slice::<u8, u32>(raw), &words);

        // Unaligned sub-slice fails alignment check on try_cast_slice, succeeds on pod_read_unaligned.
        assert_eq!(try_cast_slice::<u8, u32>(&raw[1..5]), Err(CastError::AlignmentMismatch));
        assert_eq!(try_cast_slice::<u8, u32>(&raw[0..6]), Err(CastError::OutputSliceWouldHaveSlop));
        assert_eq!(try_from_bytes::<u32>(&raw[0..3]), Err(CastError::SizeMismatch));
        assert_eq!(try_from_bytes::<u32>(&raw[1..5]), Err(CastError::TargetAlignmentGreaterAndInputNotAligned));

        let mut buf = [0u32; 2];
        let bytes = cast_slice_mut::<u32, u8>(&mut buf);
        bytes[0] = 0xAA;
        assert_eq!(bytes_of_slice(&buf)[0], 0xAA);
    }

    #[test]
    fn checked_bit_pattern_and_transparent_wrapper() {
        assert_eq!(try_checked_cast::<u8, bool>(0), Ok(false));
        assert_eq!(try_checked_cast::<u8, bool>(1), Ok(true));
        assert_eq!(try_checked_cast::<u8, bool>(2), Err(CastError::InvalidBitPattern));

        assert_eq!(try_checked_cast::<u32, char>('🦀' as u32), Ok('🦀'));
        assert_eq!(try_checked_cast::<u32, char>(0xD800), Err(CastError::InvalidBitPattern));

        assert_eq!(try_checked_cast::<u32, NonZeroU32>(0), Err(CastError::InvalidBitPattern));
        assert_eq!(try_checked_cast::<u32, NonZeroU32>(42), Ok(NonZeroU32::new(42).unwrap()));

        assert_eq!(try_checked_from_bytes::<Color>(&[1]), Ok(&Color::Green));
        assert_eq!(try_checked_from_bytes::<Color>(&[3]), Err(CastError::InvalidBitPattern));
        assert_eq!(try_checked_read_unaligned::<Color>(&[2]), Ok(Color::Blue));

        let w = Word::wrap(99);
        assert_eq!(w.peel(), 99);
        let nums = [10u64, 20, 30];
        let wrapped = Word::wrap_slice(&nums);
        assert_eq!(wrapped[1], Word(20));
        assert_eq!(Word::peel_slice(wrapped), &nums);
        let wrapping = Wrapping::wrap(7u32);
        assert_eq!(wrapping.0, 7u32);
    }

    #[test]
    fn zeroed_vec_and_fill_zeroes() {
        let alloc = crate::testing::heap().alloc();
        let mut v: Vec<Header, _> = zeroed_vec_in(8, alloc);
        assert_eq!(v.len(), 8);
        assert!(v.iter().all(|h| *h == zeroed()));
        v[3].magic = 123;
        fill_zeroes(&mut v);
        assert_eq!(v[3], zeroed());

        let opt_nz: Option<NonZeroU32> = zeroed();
        assert_eq!(opt_nz, None);
    }
}
