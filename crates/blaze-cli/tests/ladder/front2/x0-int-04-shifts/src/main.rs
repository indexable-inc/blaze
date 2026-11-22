//! x0-int-04: the runnable twin of x-int-04-shifts (no_core; success 42, else the failed
//! check's number). Checks 1 to 5 of x-int-04 with their derivations there: `<<`, and `>>`
//! arithmetic on signed and logical on unsigned (the Reference, "Arithmetic and Logical Binary
//! Operators"). Check 6 (core's `wrapping_shl`) is not here.
//! 1. 1u32 << 31 = 2147483648.   2. -16i32 >> 2 = -4.   3. -7i32 >> 1 = floor(-3.5) = -4.
//! 4. -1i32 >> 31 = -1.          5. 0x8000_0000u32 >> 31 = 1.
//! A wrong builder that shifts signed values logically returns 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn shl(x: u32, n: u32) -> u32 {
    x << n
}

fn sar(x: i32, n: u32) -> i32 {
    x >> n
}

fn shr(x: u32, n: u32) -> u32 {
    x >> n
}

fn check() -> i32 {
    if shl(1, 31) != 2147483648 {
        1
    } else if sar(-16, 2) != -4 {
        2
    } else if sar(-7, 1) != -4 {
        3
    } else if sar(-1, 31) != -1 {
        4
    } else if shr(0x8000_0000, 31) != 1 {
        5
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
