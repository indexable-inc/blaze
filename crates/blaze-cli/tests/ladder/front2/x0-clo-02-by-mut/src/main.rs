//! x0-clo-02: the runnable twin of x-clo-02-by-mut (no_core; success 42, else the failed
//! check's number). The Reference, "Capture modes": a closure that assigns to a captured
//! variable captures it by mutable borrow, so each call writes the caller's variable.
//! 1. inc = || n += 3, called twice: n = 6.     2. push(4); push(2) into d: 42.
//! 3. bump = || p.x += 10 twice from 1: 21; p.y untouched: 9.
//! A wrong builder that captures a copy returns 1 (n = 0).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct P {
    x: u32,
    y: u32,
}

fn check() -> i32 {
    let mut n = 0u32;
    let mut inc = || n += 3;
    inc();
    inc();
    let mut d = 0u32;
    let mut push = |k: u32| d = d * 10 + k;
    push(4);
    push(2);
    let mut p = P { x: 1, y: 9 };
    let mut bump = || p.x += 10;
    bump();
    bump();
    if n != 6 {
        1
    } else if d != 42 {
        2
    } else if p.x != 21 || p.y != 9 {
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
