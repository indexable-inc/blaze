//! x0-clo-08: the runnable twin of x-clo-08-fn-pointers (no_core; success 42, else the failed
//! check's number). The Reference, "Type coercions": function items and non-capturing closures
//! coerce to fn pointers. table = [double, square, inc, |x| x + 100]:
//! 1. table[0](7) = 14.  2. table[1](7) = 49.  3. table[2](7) = 8.  4. table[3](7) = 107.
//! 5. v = table[i](v) for i = 0..4 from 1: 2, 4, 5, 105.
//! 6. pick(true) is square: 3 -> 9; pick(false) is double: 3 -> 6.
//! A wrong builder that calls the wrong entry returns 1 to 4.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn double(x: u32) -> u32 {
    x * 2
}

fn square(x: u32) -> u32 {
    x * x
}

fn inc(x: u32) -> u32 {
    x + 1
}

fn pick(sq: bool) -> fn(u32) -> u32 {
    if sq { square } else { double }
}

fn check() -> i32 {
    let table: [fn(u32) -> u32; 4] = [double, square, inc, |x| x + 100];
    let (mut v, mut i) = (1, 0);
    while i < 4 {
        v = table[i](v);
        i += 1;
    }
    if table[0](7) != 14 {
        1
    } else if table[1](7) != 49 {
        2
    } else if table[2](7) != 8 {
        3
    } else if table[3](7) != 107 {
        4
    } else if v != 105 {
        5
    } else if pick(true)(3) != 9 || pick(false)(3) != 6 {
        6
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
