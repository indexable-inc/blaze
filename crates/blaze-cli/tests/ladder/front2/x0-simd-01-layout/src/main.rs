//! x0-simd-01: `repr(simd)` layout, moves and by-value passing (no_core; success 42, else the
//! failed check's number). Expected: `PASS x0-simd-01-layout`.
//! The Reference (1.88) has no section on `repr(simd)` (it names "SIMD vectors (structs defined
//! with #[repr(simd)] and which implement Copy)" once, under inline assembly); the pinned
//! library is the rule: `core/src/intrinsics/simd/mod.rs`: "In this module, a "vector" is any
//! `repr(simd)` type."; `stdarch/crates/core_arch/src/macros.rs`, `as_array`: "this type is
//! just an overaligned `[T; N]` with potential padding at the end, so pointer casting to a
//! `&[T; N]` is safe." The alignment is the target's: AAPCS64 5.9 aligns a 64-bit short vector
//! to 8 bytes and a 128-bit one to 16.
//! 1. `size_of::<U8x8>() == 8 && align_of::<U8x8>() == 8`: a layout keeping the array's own
//!    alignment (1) fails here.
//! 2. `align_of::<U32x4>() == 16`.
//! 3. a `U8x8` built, passed by value through two `#[inline(never)]` fns, read back through a
//!    pointer cast to `[u8; 8]`: lane 0 is 1 and lane 7 is 8 (a move that drops or reorders
//!    lanes fails here).
//! 4. `#[repr(C)] struct Holder { a: u8, v: U32x4 }`: `v` at offset 16, the struct 32 bytes
//!    (repr(C), so the declaration order is kept and only the alignment moves `v`).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, repr_simd)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[repr(simd)]
struct U8x8([u8; 8]);

impl Clone for U8x8 {
    fn clone(&self) -> U8x8 {
        *self
    }
}
impl Copy for U8x8 {}

#[repr(simd)]
struct U32x4([u32; 4]);

#[repr(C)]
struct Holder {
    a: u8,
    v: U32x4,
}

#[inline(never)]
fn keep(v: U8x8) -> U8x8 {
    v
}

#[inline(never)]
fn pass_on(v: U8x8) -> U8x8 {
    keep(v)
}

fn check() -> i32 {
    if size_of::<U8x8>() != 8 || align_of::<U8x8>() != 8 {
        return 1;
    }
    if align_of::<U32x4>() != 16 {
        return 2;
    }
    let v = pass_on(U8x8([1, 2, 3, 4, 5, 6, 7, 8]));
    let p = &v as *const U8x8 as *const [u8; 8];
    let (first, last) = unsafe { ((*p)[0], (*p)[7]) };
    if first != 1 || last != 8 {
        return 3;
    }
    let h = Holder { a: 7, v: U32x4([1, 2, 3, 4]) };
    let base = (&raw const h) as usize;
    let at = (&raw const h.v) as usize;
    if at - base != 16 || size_of::<Holder>() != 32 || h.a != 7 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
