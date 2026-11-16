//! x0-name-01: the runnable twin of x-name-01-none (no_core; success 42, else the failed
//! check's number). `None` and `Some` come in by the glob `use prelude::*` (the prelude's own
//! `pub use Option::{None, Some}`), as std's prelude brings core's. The Reference, "Identifier
//! patterns": "Path patterns take precedence over identifier patterns."
//! 1. kind(Some(5)): `None` does not match; `Some(n)` gives 6.     2. kind(None): 1.
//! 3. `if let None = Some(2)` is false: 20.
//! 4. nested: Some(Some(4)) -> 4, Some(None) -> 200, None -> 100.
//! A WRONG builder that binds `None` as a variable takes the first arm every time: returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn kind(o: Option<u32>) -> u32 {
    match o {
        None => 1,
        Some(n) => n + 1,
    }
}

fn nested(o: Option<Option<u32>>) -> u32 {
    match o {
        None => 100,
        Some(None) => 200,
        Some(Some(v)) => v,
    }
}

fn check() -> i32 {
    let o = Some(2u32);
    let iflet = if let None = o { 10 } else { 20 };
    if kind(Some(5)) != 6 {
        1
    } else if kind(None) != 1 {
        2
    } else if iflet != 20 {
        3
    } else if nested(Some(Some(4))) != 4 || nested(Some(None)) != 200 || nested(None) != 100 {
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
