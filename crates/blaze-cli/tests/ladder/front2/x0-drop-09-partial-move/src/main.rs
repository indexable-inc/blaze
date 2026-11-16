//! x0-drop-09: the runnable twin of x-drop-09-partial-move (no_core; success 42, else the
//! failed check's number). The Reference, "Destructors": "If a variable is only partially
//! initialized, only its initialized fields are dropped." log = log * 10 + id.
//! 1. `let a = p.a; drop(a);`: 1.     2. the scope's end drops p's b then c only: 123.
//! A wrong builder that drops p whole returns 2 (1123).
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

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let after_drop = {
        let p = P { a: D(1, log), b: D(2, log), c: D(3, log) };
        let a = p.a;
        drop(a);
        // SAFETY: as in `D::drop`.
        let seen = unsafe { *log };
        let _still = (&p.b, &p.c);
        seen
    };
    // SAFETY: as in `D::drop`.
    let end = unsafe { *log };
    if after_drop != 1 {
        1
    } else if end != 123 {
        2
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
