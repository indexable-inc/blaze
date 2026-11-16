//! x0-match-04: the runnable twin of x-match-04-tuples (no_core; success 42, else the failed
//! check's number). The Reference, "Tuple patterns": every subpattern must match; arms in
//! order. 1. (0, 0): 1.  2. (0, 7): 2.  3. (7, 0): 3.  4. (4, 4): x + y = 8.
//! 5. (4, 5): the guard fails: 4 * 10 + 5 = 45.
//! A wrong builder that tests only the first element returns 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn pair(t: (u32, u32)) -> u32 {
    match t {
        (0, 0) => 1,
        (0, _) => 2,
        (_, 0) => 3,
        (x, y) if x == y => x + y,
        (x, y) => x * 10 + y,
    }
}

fn check() -> i32 {
    if pair((0, 0)) != 1 {
        1
    } else if pair((0, 7)) != 2 {
        2
    } else if pair((7, 0)) != 3 {
        3
    } else if pair((4, 4)) != 8 {
        4
    } else if pair((4, 5)) != 45 {
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
