//! x0-match-02: the runnable twin of x-match-02-guards (no_core; success 42, else the failed
//! check's number). The Reference, "Match guards": if the guard is false "the next pattern,
//! including other matches with the `|` operator in the same arm, is tested"; its own example
//! (`1 | _ if { i += 1; false }`) runs the guard once per alternative: twice. The counter is a
//! local written through a raw pointer (no `Cell` without core).
//! 1. pick(7): `n if n > 10` fails, `n if n > 5` passes: 2.   2. pick(3): `_`, 4.
//! 3. pick(12): 1.                                            4. the guard ran twice: 2.
//! A wrong builder that commits to an arm once its pattern matches returns 1 or 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn pick(x: u32) -> u32 {
    match x {
        n if n > 10 => 1,
        n if n > 5 => 2,
        7 => 3,
        _ => 4,
    }
}

fn bump(p: *mut u32) -> bool {
    // SAFETY: `p` points at `main`'s live local `runs`, written only here.
    unsafe { *p += 1 };
    false
}

fn check() -> i32 {
    let mut runs = 0u32;
    let p: *mut u32 = &raw mut runs;
    match 1 {
        1 | _ if bump(p) => {}
        _ => {}
    }
    // SAFETY: as in `bump`.
    let ran = unsafe { *p };
    if pick(7) != 2 {
        1
    } else if pick(3) != 4 {
        2
    } else if pick(12) != 1 {
        3
    } else if ran != 2 {
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
