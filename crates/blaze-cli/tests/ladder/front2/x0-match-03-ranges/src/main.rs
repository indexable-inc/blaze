//! x0-match-03: the runnable twin of x-match-03-ranges (no_core; success 42, else the failed
//! check's number). The Reference, "Range patterns": `a..=b` includes b, `a..b` excludes b;
//! chars compare by scalar value ('a' 97 .. 'z' 122, '{' 123). i32's minimum is written
//! -2147483648.
//! 1. 'z' -> 1.  2. 'Z' -> 2.  3. '0' -> 3.  4. ' ' -> 4.  5. '{' -> 4.
//! 6. -1 in -2147483648..=-1 -> 1.  7. 0 in 0..10 -> 2.  8. 10 not in 0..10, in 10..=10 -> 3.
//! A wrong builder that excludes an inclusive end returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn class(c: char) -> u32 {
    match c {
        'a'..='z' => 1,
        'A'..='Z' => 2,
        '0'..='9' => 3,
        _ => 4,
    }
}

fn sign(x: i32) -> u32 {
    match x {
        -2147483648..=-1 => 1,
        0..10 => 2,
        10..=10 => 3,
        _ => 4,
    }
}

fn check() -> i32 {
    if class('z') != 1 {
        1
    } else if class('Z') != 2 {
        2
    } else if class('0') != 3 {
        3
    } else if class(' ') != 4 {
        4
    } else if class('{') != 4 {
        5
    } else if sign(-1) != 1 {
        6
    } else if sign(0) != 2 {
        7
    } else if sign(10) != 3 {
        8
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
