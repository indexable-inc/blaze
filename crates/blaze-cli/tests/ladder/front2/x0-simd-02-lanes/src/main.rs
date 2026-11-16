//! x0-simd-02: the `simd_*` intrinsics lane by lane (no_core; success 42, else the failed
//! check's number). Expected: `PASS x0-simd-02-lanes`.
//! The rules are the declarations' doc comments in the pinned
//! `library/core/src/intrinsics/simd/mod.rs`:
//! `simd_eq`/`simd_lt`: "`T` must be a vector of integers or floats. `U` must be a vector of
//! integers with the same number of elements and element size as `T`. Returns `0` for false
//! and `!0` for true."; `simd_splat`: "Creates a vector where every lane has the provided
//! value."; `simd_extract`: "`T` must be a vector with element type `U`, and `idx` must be
//! `const`."; `simd_and`: "\"And\"s vectors elementwise." A vector's bytes are its lanes in
//! order (`as_array` in core_arch's macros.rs), so its `u64` is lane 0 in the low byte.
//! 1. `simd_eq([1, 2, .., 8], splat(3))` as `u64` is `0x0000_0000_00FF_0000` (lane 2 all
//!    ones; a build writing 1 for true fails here).
//! 2. `simd_lt` of `i8` lanes `[-1, 0, 1, -128, 5, 6, 7, 8]` against zero is `0xFF` in lanes 0
//!    and 3 only (an unsigned compare gives 0 and fails here).
//! 3. `simd_extract::<_, u8>(v, 7) == 8`.
//! 4. `simd_and(eq, splat(0x0F))` keeps `0x0F` in lane 2 only.
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
struct I8x8([i8; 8]);

impl Clone for I8x8 {
    fn clone(&self) -> I8x8 {
        *self
    }
}
impl Copy for I8x8 {}

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_eq<T, U>(x: T, y: T) -> U;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_lt<T, U>(x: T, y: T) -> U;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_splat<T, U>(value: U) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_extract<T, U>(x: T, idx: u32) -> U;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_and<T>(x: T, y: T) -> T;

/// The vector's 8 bytes as one `u64` (as `vreinterpret_u64_u8`, a `transmute`).
#[inline(never)]
fn bits(v: U8x8) -> u64 {
    unsafe { transmute::<U8x8, u64>(v) }
}

fn check() -> i32 {
    let v = U8x8([1, 2, 3, 4, 5, 6, 7, 8]);
    let three: U8x8 = unsafe { simd_splat(3u8) };
    let eq: U8x8 = unsafe { simd_eq(v, three) };
    if bits(eq) != 0x0000_0000_00FF_0000 {
        return 1;
    }
    // `-128i8` written as its bits, so the check does not lean on literal negation.
    let s = I8x8([-1, 0, 1, 0x80u8 as i8, 5, 6, 7, 8]);
    let zero: I8x8 = unsafe { simd_splat(0i8) };
    let lt: U8x8 = unsafe { simd_lt(s, zero) };
    if bits(lt) != 0x0000_0000_FF00_00FF {
        return 2;
    }
    let last: u8 = unsafe { simd_extract(v, 7) };
    if last != 8 {
        return 3;
    }
    let low: U8x8 = unsafe { simd_splat(0x0Fu8) };
    let masked: U8x8 = unsafe { simd_and(eq, low) };
    if bits(masked) != 0x0000_0000_000F_0000 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
