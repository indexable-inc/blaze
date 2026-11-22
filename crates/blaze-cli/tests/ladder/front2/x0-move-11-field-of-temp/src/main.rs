//! x0-move-11: a field moved out of a temporary (no_core; success 42, else the failed check's
//! number). The Reference, [expr.move.movable-place]: "Only the following place expressions may
//! be moved out of: ... Temporary values. Fields of a place expression which can be moved out
//! of and don't implement `Drop`."; "Temporary scopes": a temporary lives to the end of its
//! statement; "Destructors": "If a variable is only partially initialized, only its
//! initialized fields are dropped"; "The fields of a struct are dropped in declaration order",
//! a tuple's in order. log = log * 10 + id.
//! 1. `let b = make(log).b;`: the temporary's rest (a, c) drops at the statement's end: 13.
//! 2. `let t = (D(4), D(5)).1;`: the tuple's rest drops: 134.
//! 3. `drop(t); drop(b);`: each moved field dropped once, by its new owner: 13452.
//! A builder that never drops the rest returns 1 (0); one that drops the temporary whole
//! returns 1 too (123), and its moved field would drop twice.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct D(u32, *mut u32);

impl Drop for D {
    fn drop(&mut self) {
        // SAFETY: the pointer is `check`'s live local; one thread.
        unsafe { *self.1 = *self.1 * 10 + self.0 };
    }
}

struct P {
    a: D,
    b: D,
    c: D,
}

fn make(log: *mut u32) -> P {
    P { a: D(1, log), b: D(2, log), c: D(3, log) }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let b = make(log).b;
    // SAFETY: as in `D::drop`.
    let first = unsafe { *log };
    let t = (D(4, log), D(5, log)).1;
    // SAFETY: as in `D::drop`.
    let second = unsafe { *log };
    drop(t);
    drop(b);
    // SAFETY: as in `D::drop`.
    let end = unsafe { *log };
    if first != 13 {
        1
    } else if second != 134 {
        2
    } else if end != 13452 {
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
