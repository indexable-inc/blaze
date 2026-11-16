//! x0-match-01: the runnable twin of x-match-01-order (no_core; success 42, else the failed
//! check's number). The Reference, "Match expressions": "The first arm with a matching pattern
//! is chosen as the branch target of the match".
//! 1. 5 matches `0..=9` (arm 1) and `5`: 10.     2. 12 matches `10..=19` before `12`: 30.
//! 3. with `12` written first: 40.               4. 99: only `_`: 50.
//! A wrong builder that lets a later or more specific arm win returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn first(x: u32) -> u32 {
    match x {
        0..=9 => 10,
        5 => 20,
        10..=19 => 30,
        12 => 40,
        _ => 50,
    }
}

fn swapped(x: u32) -> u32 {
    match x {
        12 => 40,
        10..=19 => 30,
        _ => 50,
    }
}

fn check() -> i32 {
    if first(5) != 10 {
        1
    } else if first(12) != 30 {
        2
    } else if swapped(12) != 40 {
        3
    } else if first(99) != 50 {
        4
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
