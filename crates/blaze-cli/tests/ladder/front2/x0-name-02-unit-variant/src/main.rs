//! x0-name-02: the runnable twin of x-name-02-unit-variant (no_core; success 42, else the
//! failed check's number). After `use Color::{Blue, Green, Red};` each bare name is its unit
//! variant (the Reference, "Identifier patterns": "Path patterns take precedence over
//! identifier patterns."); a unit struct's name likewise.
//! 1. code(Blue) = 3.  2. code(Green) = 2.  3. code(Red) = 1.  4. unit(Unit) = 9.
//! A WRONG builder that binds `Red` as a variable returns 1 (code(Blue) = 1).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

use Color::{Blue, Green, Red};

enum Color {
    Red,
    Green,
    Blue,
}

struct Unit;

fn code(c: Color) -> u32 {
    match c {
        Red => 1,
        Green => 2,
        Blue => 3,
    }
}

fn unit(u: Unit) -> u32 {
    match u {
        Unit => 9,
    }
}

fn check() -> i32 {
    if code(Blue) != 3 {
        1
    } else if code(Green) != 2 {
        2
    } else if code(Red) != 1 {
        3
    } else if unit(Unit) != 9 {
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
