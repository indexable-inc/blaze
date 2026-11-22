//! x0-match-08: the runnable twin of x-match-08-or (no_core; success 42, else the failed
//! check's number). The Reference, "Or-patterns": a match on one of the sub-patterns; every
//! binding bound in each.
//! 1. small(2) = 1.  2. small(5) = 2.  3. small(6) = 3.
//! 4. Some(1 | 2) -> 10 for Some(2); Some(3) -> 20; None -> 30.
//! 5. `(x, 0) | (0, x)`: (8, 0) -> 8, (0, 6) -> 6, (3, 4) -> 0.
//! A wrong builder that tests only the first alternative returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn small(x: u32) -> u32 {
    match x {
        1 | 2 | 3 => 1,
        4 | 5 => 2,
        _ => 3,
    }
}

fn inner(o: Option<u32>) -> u32 {
    match o {
        Some(1 | 2) => 10,
        Some(_) => 20,
        None => 30,
    }
}

fn either(t: (u32, u32)) -> u32 {
    match t {
        (x, 0) | (0, x) => x,
        _ => 0,
    }
}

fn check() -> i32 {
    if small(2) != 1 {
        1
    } else if small(5) != 2 {
        2
    } else if small(6) != 3 {
        3
    } else if inner(Some(2)) != 10 || inner(Some(3)) != 20 || inner(None) != 30 {
        4
    } else if either((8, 0)) != 8 || either((0, 6)) != 6 || either((3, 4)) != 0 {
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
