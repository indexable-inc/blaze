//! x0-drop-01: the runnable twin of x-drop-01-locals (no_core; success 42, else the failed
//! check's number). The Reference, "Drop scopes": "all variables associated to that scope are
//! dropped in reverse order of declaration". Each drop appends its id to a log reached through
//! a raw pointer: log = log * 10 + id.
//! 1. nothing dropped inside the block: 0.     2. a, b, c (1, 2, 3) drop 3, 2, 1: 321.
//! 3. the inner block's 4 before the outer block's 5: 32145.
//! A wrong builder that drops in declaration order returns 2 (123).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct D {
    id: u32,
    log: *mut u32,
}

impl Drop for D {
    fn drop(&mut self) {
        // SAFETY: `log` points at `check`'s local, alive past every `D`; one thread.
        unsafe { *self.log = *self.log * 10 + self.id };
    }
}

fn read(log: *mut u32) -> u32 {
    // SAFETY: as in `drop`.
    unsafe { *log }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let seen_inside;
    {
        let _a = D { id: 1, log };
        let _b = D { id: 2, log };
        let _c = D { id: 3, log };
        seen_inside = read(log);
    }
    let after_three = read(log);
    {
        let _outer = D { id: 5, log };
        {
            let _inner = D { id: 4, log };
        }
    }
    if seen_inside != 0 {
        1
    } else if after_three != 321 {
        2
    } else if read(log) != 32145 {
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
