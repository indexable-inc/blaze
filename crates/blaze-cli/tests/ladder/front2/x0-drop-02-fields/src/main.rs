//! x0-drop-02: the runnable twin of x-drop-02-fields (no_core; success 42, else the failed
//! check's number). The Reference, "Destructors": a value's own `Drop::drop` runs, then its
//! fields' destructors; "The fields of a struct are dropped in declaration order."
//! log = log * 10 + id, through a raw pointer.
//! 1. Pair { a: 1, b: 2 }: 12.     2. Outer (its own drop logs 9) { 3, 4 }: 12 then 934: 12934.
//! 3. `Pair { b: 6, a: 5 }` (initializers in the other order) still drops a then b: 1293456.
//! A wrong builder that drops fields in reverse returns 1 (21).
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

struct Pair {
    a: D,
    b: D,
}

struct Outer {
    log: *mut u32,
    first: D,
    second: D,
}

impl Drop for Outer {
    fn drop(&mut self) {
        // SAFETY: as in `D::drop`.
        unsafe { *self.log = *self.log * 10 + 9 };
    }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    drop(Pair { a: D(1, log), b: D(2, log) });
    // SAFETY: as in `D::drop`.
    let one = unsafe { *log };
    drop(Outer { log, first: D(3, log), second: D(4, log) });
    let two = unsafe { *log };
    drop(Pair { b: D(6, log), a: D(5, log) });
    let three = unsafe { *log };
    if one != 12 {
        1
    } else if two != 12934 {
        2
    } else if three != 1293456 {
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
