//! x0-simd-03: the vector `simd_*` intrinsics `is_ascii_neon` reaches (no_core; success 42,
//! else the failed check's number). Expected: `PASS x0-simd-03-neon-ascii`.
//! `library/core/src/slice/ascii.rs` `is_ascii_neon` ORs four `vld1q_u8` loads with `vorrq_u8`
//! (`simd_or`) and takes `vmaxvq_u8` (`simd_reduce_max`) of the result: a byte is not ASCII
//! when the maximum is at least 128. The rules are the declarations' doc comments in the pinned
//! `library/core/src/intrinsics/simd/mod.rs`: `simd_reduce_max`: "Returns the maximum element
//! of a vector." (`T` of `u8` lanes: compared unsigned); `simd_saturating_sub`: "Subtracts two
//! simd vectors elementwise, with saturation." (`vqsubq_u8`); `simd_or`: "\"Ors\" vectors
//! elementwise."
//! 1. 16 ASCII bytes ("hello, world!!!!") have a maximum below 128.
//! 2. The same with 0xC3 in lane 9 has a maximum of 0xC3 (a signed maximum, which sees 0xC3 as
//!    -61, says ASCII and fails here).
//! 3. `max(saturating [1, .., 15, 200] - splat(5))` is 195 (a wrapping subtract gives 252).
//! 4. A 64-byte chunk as `is_ascii_neon` checks it: four vectors ORed, one byte 0x80 among
//!    them, so the maximum of the OR is at least 128.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, repr_simd)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[repr(simd)]
struct U8x16([u8; 16]);

impl Clone for U8x16 {
    fn clone(&self) -> U8x16 {
        *self
    }
}
impl Copy for U8x16 {}

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_reduce_max<T, U>(x: T) -> U;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_saturating_sub<T>(lhs: T, rhs: T) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_or<T>(x: T, y: T) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn simd_splat<T, U>(value: U) -> T;

/// `vld1q_u8`: the 16 bytes as a vector (a separate fn, so the vector arrives in memory).
#[inline(never)]
fn load(bytes: [u8; 16]) -> U8x16 {
    U8x16(bytes)
}

/// `vmaxvq_u8`.
#[inline(never)]
fn max(v: U8x16) -> u8 {
    unsafe { simd_reduce_max(v) }
}

fn check() -> i32 {
    // "hello, world!!!!"
    let ascii = [104u8, 101, 108, 108, 111, 44, 32, 119, 111, 114, 108, 100, 33, 33, 33, 33];
    if max(load(ascii)) >= 128 {
        return 1;
    }
    let mut high = ascii;
    high[9] = 0xC3;
    if max(load(high)) != 0xC3 {
        return 2;
    }
    let x = load([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 200]);
    let five: U8x16 = unsafe { simd_splat(5u8) };
    let diff: U8x16 = unsafe { simd_saturating_sub(x, five) };
    if max(diff) != 195 {
        return 3;
    }
    let mut odd = ascii;
    odd[3] = 0x80;
    let a1 = load(ascii);
    let a2 = load(ascii);
    let b1 = load(odd);
    let b2 = load(ascii);
    let combined: U8x16 = unsafe { simd_or(simd_or(a1, a2), simd_or(b1, b2)) };
    if max(combined) < 128 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
