//! x0-move-14: a `ref mut` binding in a `match` on a SCALAR place behind a reference changes
//! that place (no_core; success 42, else the failed check's number). Written by the
//! wrong-answers lane (2026-11-16, WRONG-NOTES.md "Runner 5", S5a); x0-move-13 is the same
//! rule on aggregates.
//!
//! The Reference, "Match expressions": "If the scrutinee expression is a place expression, the
//! match does not allocate a temporary location"; "Identifier patterns": "`ref mut` .. binds a
//! mutable reference to the matched place".
//! 1. `match *r { ref mut a => *a = 7 }` with `r = &mut n`: `n` is 7.
//! 2. `match *r { 7 => {} ref mut a => *a = 0 }` then `match *r { ref mut a if *a == 7 => *a
//!    += 1, _ => {} }`: `n` is 8.
//! A builder that matches a copy of the scalar returns 1 or 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn set(r: &mut u32) {
    match *r {
        ref mut a => *a = 7,
    }
}

fn bump(r: &mut u32) {
    match *r {
        7 => {}
        ref mut a => *a = 0,
    }
    match *r {
        ref mut a if *a == 7 => *a += 1,
        _ => {}
    }
}

fn check() -> i32 {
    let mut n = 1u32;
    set(&mut n);
    if n != 7 {
        return 1;
    }
    bump(&mut n);
    if n != 8 {
        return 2;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
