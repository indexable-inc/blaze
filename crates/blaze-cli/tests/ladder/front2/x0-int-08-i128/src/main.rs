//! x0-int-08: the runnable twin of x-int-08-i128 (no_core; success 42, else the failed check's
//! number). Derivations in x-int-08; `i128::MIN` is written `1i128 << 127` (bit 127 alone set,
//! two's complement -2^127) and `u128::MAX` `!0u128`.
//! 1. -1i128 as u128 = !0u128.   2. -5 * 3 = -15.   3. i128::MIN < 0.
//! 4. -1i64 as i128 = -1 (sign extension).   5. -(2^70) >> 70 = -1.   6. -(2^70) as u64 = 0.
//! A wrong builder that zero-extends the high half on a cast returns 4.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn check(m1: i128, five: i128, small: i64) -> i32 {
    if m1 as u128 != !0u128 {
        return 1;
    }
    if -five * 3 != -15 {
        return 2;
    }
    if !((1i128 << 127) < m1 + 1) {
        return 3;
    }
    if small as i128 != -1 {
        return 4;
    }
    let big = -(1i128 << 70);
    if big >> 70 != -1 {
        return 5;
    }
    if big as u64 != 0 {
        return 6;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(-1, 5, -1);
    if failed == 0 { 42 } else { failed }
}
