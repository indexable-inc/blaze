//! x0-clo-04: the runnable twin of x-clo-04-generic-fn (no_core; success 42, else the failed
//! check's number). The Reference, "Closure types": each closure has its own type, so a generic
//! `F: Fn(u32) -> u32` is instantiated per closure and runs that closure's body and captures.
//! 1. apply(|x| x * 2, 21) = 42.     2. apply(|x| x + k, 1) with k = 7: 8.
//! 3. twice(|x| x * 3, 2) = 18; twice(|x| x + k, 0) = 14.
//! 4. a named fn item: apply(inc, 9) = 10.
//! A wrong builder that shares one instance between closures returns 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn apply<F: Fn(u32) -> u32>(f: F, x: u32) -> u32 {
    f(x)
}

fn twice<F: Fn(u32) -> u32>(f: F, x: u32) -> u32 {
    f(f(x))
}

fn inc(x: u32) -> u32 {
    x + 1
}

fn check() -> i32 {
    let k = 7u32;
    if apply(|x| x * 2, 21) != 42 {
        1
    } else if apply(|x| x + k, 1) != 8 {
        2
    } else if twice(|x| x * 3, 2) != 18 || twice(|x| x + k, 0) != 14 {
        3
    } else if apply(inc, 9) != 10 {
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
