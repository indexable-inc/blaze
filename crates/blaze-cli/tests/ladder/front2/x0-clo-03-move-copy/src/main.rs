//! x0-clo-03: the runnable twin of x-clo-03-move-copy (no_core; success 42, else the failed
//! check's number). The Reference, "Closure expressions": "If the `move` keyword is used, then
//! all captures are by move or, for Copy types, by copy".
//! 1. f = move || x + 1 with x = 5: 6; x still 5.
//! 2. g = move || { y += 10; y } from y = 1: 11 then 21 (its own copy); y still 1.
//! 3. after `x = 50`, f() is still 6.
//! A wrong builder that captures by reference returns 2 or 3.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn check() -> i32 {
    let mut x = 5u32;
    let f = move || x + 1;
    let one = f() == 6 && x == 5;
    let mut y = 1u32;
    let mut g = move || {
        y += 10;
        y
    };
    let g1 = g();
    let g2 = g();
    x = 50;
    if !one {
        1
    } else if g1 != 11 || g2 != 21 || y != 1 {
        2
    } else if f() != 6 || x != 50 {
        3
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
