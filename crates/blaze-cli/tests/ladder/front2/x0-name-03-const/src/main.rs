//! x0-name-03: the runnable twin of x-name-03-const (no_core; success 42, else the failed
//! check's number). The Reference, "Path patterns": "Unqualified path patterns can refer to:
//! enum variants, structs, constants, associated constants"; a constant pattern matches a value
//! equal to it. `u32: PartialEq` (the prelude) makes the constants usable as patterns.
//! 1. limit(3): neither LIMIT (10) nor ZERO (0): n * 2 = 6.  2. limit(10): 100.
//! 3. limit(0): 200.  4. `Self::MAX_LEN` (4): fits(4) = 1, fits(5) = 0.
//! A WRONG builder that binds `LIMIT` as a variable returns 1 (100).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

const LIMIT: u32 = 10;
const ZERO: u32 = 0;

fn limit(x: u32) -> u32 {
    match x {
        LIMIT => 100,
        ZERO => 200,
        n => n * 2,
    }
}

struct Buf;

impl Buf {
    const MAX_LEN: u32 = 4;
    fn fits(x: u32) -> u32 {
        match x {
            Self::MAX_LEN => 1,
            _ => 0,
        }
    }
}

fn check() -> i32 {
    if limit(3) != 6 {
        1
    } else if limit(10) != 100 {
        2
    } else if limit(0) != 200 {
        3
    } else if Buf::fits(4) != 1 || Buf::fits(5) != 0 {
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
