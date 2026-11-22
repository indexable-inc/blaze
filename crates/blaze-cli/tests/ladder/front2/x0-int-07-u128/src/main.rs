//! x0-int-07: the runnable twin of x-int-07-u128 (no_core; success 42, else the failed check's
//! number). Derivations in x-int-07 (2^64 = 18446744073709551616):
//! 1. (1 << 64) * 3 = 55340232221128654848.
//! 2. (2^64 - 1)^2 = 2^128 - 2^65 + 1 = 340282366920938463426481119284349108225.
//! 3. (1 << 100) >> 99 = 2.      4. 2^64 > 2^64 - 1.
//! 5. ((1 << 64) + 5) as u64 = 5 (truncation).
//! 6. the `wrapping_add` intrinsic (core's `u128::wrapping_add` calls it): !0u128 + 1 wraps to 0.
//! A wrong builder that drops a product's high half returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn check(one: u128, m: u64) -> i32 {
    let two64 = one << 64;
    if two64 * 3 != 55340232221128654848 {
        return 1;
    }
    if m as u128 * m as u128 != 340282366920938463426481119284349108225 {
        return 2;
    }
    if (one << 100) >> 99 != 2 {
        return 3;
    }
    if !(two64 > m as u128) {
        return 4;
    }
    if (two64 + 5) as u64 != 5 {
        return 5;
    }
    if wrapping_add(!0u128, one) != 0 {
        return 6;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(1, !0u64);
    if failed == 0 { 42 } else { failed }
}
