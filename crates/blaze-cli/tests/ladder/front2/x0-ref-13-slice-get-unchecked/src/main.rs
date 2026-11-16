//! x0-ref-13: the `slice_get_unchecked` intrinsic (no_core; success 42, else the failed check's
//! number). Its declaration and doc in the pinned library (library/core/src/intrinsics/mod.rs):
//!
//!   /// Projects to the `index`-th element of `slice_ptr`, as the same kind of pointer
//!   /// as the slice was provided -- so `&mut [T] → &mut T`, `&[T] → &T`,
//!   /// `*mut [T] → *mut T`, or `*const [T] → *const T` -- without a bounds check.
//!   /// ...
//!   /// - `index < PtrMetadata(slice_ptr)`, so the indexing is in-bounds for the slice
//!   pub const unsafe fn slice_get_unchecked<
//!       ItemPtr: bounds::ChangePointee<[T], Pointee = T, Output = SlicePtr>, SlicePtr, T,
//!   >(slice_ptr: SlicePtr, index: usize) -> ItemPtr;
//!
//! `BuiltinDeref` and `ChangePointee` are core's (library/core/src/intrinsics/bounds.rs).
//! On `[10u16, 20, 32, 42]`:
//! 1. `*slice_get_unchecked::<*const u16, *const [u16], u16>(p, 3) == 42`: the stride is 2
//!    bytes; a stride of 1 (or the index as a byte offset) reads other bits.
//! 2. `*slice_get_unchecked::<&u16, &[u16], u16>(s, 2) == 32`: a reference the same way.
//! 3. index 0 is the data pointer itself: 10.
//! The run prints nothing: `front2/x0-ref-13-slice-get-unchecked: PASS (status 42, stdout byte-equal)`.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

/// core::intrinsics::bounds::BuiltinDeref: "Types with a built-in dereference operator in
/// runtime MIR, aka references and raw pointers."
pub unsafe trait BuiltinDeref: Sized {
    type Pointee: PointeeSized;
}

unsafe impl<T: PointeeSized> BuiltinDeref for &T {
    type Pointee = T;
}
unsafe impl<T: PointeeSized> BuiltinDeref for *const T {
    type Pointee = T;
}

/// core::intrinsics::bounds::ChangePointee.
pub trait ChangePointee<U: PointeeSized>: BuiltinDeref {
    type Output;
}
impl<'a, T: PointeeSized + 'a, U: PointeeSized + 'a> ChangePointee<U> for &'a T {
    type Output = &'a U;
}
impl<T: PointeeSized, U: PointeeSized> ChangePointee<U> for *const T {
    type Output = *const U;
}

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn slice_get_unchecked<ItemPtr: ChangePointee<[T], Pointee = T, Output = SlicePtr>, SlicePtr, T>(
    slice_ptr: SlicePtr,
    index: usize,
) -> ItemPtr;

fn check(s: &[u16]) -> i32 {
    let p = s as *const [u16];
    unsafe {
        if *slice_get_unchecked::<*const u16, *const [u16], u16>(p, 3) != 42 {
            return 1;
        }
        if *slice_get_unchecked::<&u16, &[u16], u16>(s, 2) != 32 {
            return 2;
        }
        if *slice_get_unchecked::<*const u16, *const [u16], u16>(p, 0) != 10 {
            return 3;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let a: [u16; 4] = [10, 20, 32, 42];
    let failed = check(&a);
    if failed == 0 { 42 } else { failed }
}
