//! x0-drop-07: the runnable twin of x-drop-07-drop-flags (no_core; success 42, else the failed
//! check's number). The Reference, "Destructors": "When an initialized variable or temporary
//! goes out of scope, its destructor is run": a value moved on one path only is dropped once on
//! every path. log = log * 10 + id, through a raw pointer.
//! 1. moved: `take` drops a (1) at once, the scope's end drops b (2): 12.
//! 2. not moved: the scope's end drops b then a: 21.
//! 3. moved into another local (`let _c = a;`): one drop: 1.
//! A wrong builder with no drop flag returns 1 (121) or 2 (2).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct D(u32, *mut u32);

impl Drop for D {
    fn drop(&mut self) {
        // SAFETY: the pointer is the caller's live local; one thread.
        unsafe { *self.1 = *self.1 * 10 + self.0 };
    }
}

fn take(_d: D) {}

fn run(moved: bool) -> u32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    {
        let a = D(1, log);
        let _b = D(2, log);
        if moved {
            take(a);
        }
    }
    // SAFETY: as in `D::drop`.
    unsafe { *log }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    {
        let a = D(1, log);
        let _c = a;
    }
    // SAFETY: as in `D::drop`.
    let once = unsafe { *log };
    if run(true) != 12 {
        1
    } else if run(false) != 21 {
        2
    } else if once != 1 {
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
