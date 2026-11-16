//! x0-int-09: the runnable twin of x-int-09-divrem (no_core; success 42, else the failed
//! check's number). The Reference: "Integer division rounds towards zero."; `%`: "the
//! remainder will have the same sign as the dividend."
//! 1. -7 / 2 = -3.     2. -7 % 2 = -7 - 2 * (-3) = -1.     3. 7 % -2 = 7 - (-2) * (-3) = 1.
//! 4. -7 / -2 = 3.     5. 4000000000u32 / 3 = 1333333333 (3 * 1333333333 = 3999999999), % 3 = 1.
//! A wrong builder that floors returns 1 (-4); one that divides u32 as signed returns 5.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn div(a: i32, b: i32) -> i32 {
    a / b
}

fn rem(a: i32, b: i32) -> i32 {
    a % b
}

fn check(big: u32) -> i32 {
    if div(-7, 2) != -3 {
        1
    } else if rem(-7, 2) != -1 {
        2
    } else if rem(7, -2) != 1 {
        3
    } else if div(-7, -2) != 3 {
        4
    } else if big / 3 != 1333333333 || big % 3 != 1 {
        5
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(4000000000);
    if failed == 0 { 42 } else { failed }
}
