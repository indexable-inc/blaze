//! x0-struct-update-01: struct update expressions, `P { a: x, ..base }` (no_core; success 42,
//! else the failed check's number). The Reference, "Functional update syntax": the update
//! "uses the given values for the fields that were specified and moves or copies the remaining
//! fields from the base expression"; "Destructors": "If a variable is only partially
//! initialized, only its initialized fields are dropped." log = log * 10 + id.
//! 1. A local base: `P { a: D(4), ..base }` copies `n` and moves `c`; `q.n + q.c.0` is 8; the
//!    scope's end drops q (4, 3), then what is left of base (1): 431.
//! 2. A call-result base: `P { n: 7, ..make() }` moves `a` and `c` out of the temporary, which
//!    then holds nothing to drop at its statement's end (log 0); `q.n + q.a.0 + q.c.0` is 11;
//!    the scope's end drops q (1, 3): 13.
//! A wrong builder that drops a base whole after the update drops a moved `D` twice (4313,
//! or 13 at the statement's end); one that never drops what is left of a local base gives 43;
//! one that does not copy the taken fields gives other sums.
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
    n: u32,
    c: D,
}

fn make(log: *mut u32) -> P {
    P { a: D(1, log), n: 5, c: D(3, log) }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let sum1 = {
        let base = P { a: D(1, log), n: 5, c: D(3, log) };
        let q = P { a: D(4, log), ..base };
        let _still = &base.a;
        q.n + q.c.0
    };
    // SAFETY: as in `D::drop`.
    let end1 = unsafe { *log };
    let mut cell2 = 0u32;
    let log2: *mut u32 = &raw mut cell2;
    let (sum2, after_update) = {
        let q = P { n: 7, ..make(log2) };
        // SAFETY: as in `D::drop`.
        let seen = unsafe { *log2 };
        (q.n + q.a.0 + q.c.0, seen)
    };
    // SAFETY: as in `D::drop`.
    let end2 = unsafe { *log2 };
    if sum1 != 8 {
        1
    } else if end1 != 431 {
        2
    } else if after_update != 0 {
        3
    } else if sum2 != 11 {
        4
    } else if end2 != 13 {
        5
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
