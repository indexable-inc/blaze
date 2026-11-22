//! x0-int-11: the `three_way_compare` intrinsic (no_core; success 42, else the failed check's
//! number). Its declaration and doc in the pinned library (library/core/src/intrinsics/mod.rs):
//!
//!   /// Does a three-way comparison between the two arguments,
//!   /// which must be of character or integer (signed or unsigned) type.
//!   pub const fn three_way_compare<T: Copy>(lhs: T, rhss: T) -> crate::cmp::Ordering;
//!
//! and `Ordering` (library/core/src/cmp.rs; the prelude's copy): `#[repr(i8)]`, Less = -1,
//! Equal = 0, Greater = 1.
//! 1. -1i32 vs 1 is Less (-1).          2. u32::MAX vs 1 is Greater (1): an unsigned compare.
//! 3. 'a' vs 'a' is Equal (0).          4. i8::MIN vs i8::MAX is Less (-1): a signed compare.
//! 5. 'z' vs 'a' is Greater (1); 7u64 vs 9 is Less (-1).
//! A builder that compares every `T` signed fails 2; one that compares every `T` unsigned
//! fails 1 and 4; one that swaps Less and Greater fails 1. The run prints nothing:
//! `front2/x0-int-11-three-way: PASS (status 42, stdout byte-equal)`.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn three_way_compare<T: Copy>(lhs: T, rhss: T) -> Ordering;

fn check(neg: i32, max: u32, a: char, lo: i8, hi: i8) -> i32 {
    if three_way_compare(neg, 1i32) as i8 != -1 {
        return 1;
    }
    if three_way_compare(max, 1u32) as i8 != 1 {
        return 2;
    }
    if three_way_compare(a, 'a') as i8 != 0 {
        return 3;
    }
    if three_way_compare(lo, hi) as i8 != -1 {
        return 4;
    }
    if three_way_compare('z', a) as i8 != 1 || three_way_compare(7u64, 9u64) as i8 != -1 {
        return 5;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(-1, 4294967295, 'a', -128, 127);
    if failed == 0 { 42 } else { failed }
}
