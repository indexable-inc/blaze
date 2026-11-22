//! x0-drop-08: the runnable twin of x-drop-08-return-break (no_core; success 42, else the
//! failed check's number). The Reference, "Drop scopes": leaving a scope drops its variables in
//! reverse declaration order; `return` leaves the function's scopes, `break` the loop body's.
//! log = log * 10 + id, through a raw pointer.
//! 1. early(true): a (1) live at the `return`, b never made: 1; result 7.
//! 2. early(false): b then a: 21; result 8.
//! 3. a loop making D(3), breaking on its second pass before making D(4): pass one drops 4, 3;
//!    pass two 3: 433.
//! A wrong builder that skips drops on `return` returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct D(u32, *mut u32);

impl Drop for D {
    fn drop(&mut self) {
        // SAFETY: the pointer is a live local of `check`; one thread.
        unsafe { *self.1 = *self.1 * 10 + self.0 };
    }
}

fn early(go: bool, log: *mut u32) -> u32 {
    let _a = D(1, log);
    if go {
        return 7;
    }
    let _b = D(2, log);
    8
}

fn check() -> i32 {
    let (mut c1, mut c2, mut c3) = (0u32, 0u32, 0u32);
    let (one, two, three): (*mut u32, *mut u32, *mut u32) = (&raw mut c1, &raw mut c2, &raw mut c3);
    let r1 = early(true, one);
    let r2 = early(false, two);
    let mut pass = 0;
    loop {
        let _x = D(3, three);
        pass += 1;
        if pass == 2 {
            break;
        }
        let _y = D(4, three);
    }
    // SAFETY: as in `D::drop`.
    let (v1, v2, v3) = unsafe { (*one, *two, *three) };
    if v1 != 1 || r1 != 7 {
        1
    } else if v2 != 21 || r2 != 8 {
        2
    } else if v3 != 433 {
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
