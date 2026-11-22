//! x0-drop-04: the runnable twin of x-drop-04-temporaries (no_core; success 42, else the failed
//! check's number). The Reference, "Temporary scopes": a temporary's scope is the smallest
//! enclosing statement; temporaries drop "in reverse order of ... creation".
//! log = log * 10 + id, through a raw pointer.
//! 1. `let n = mk(1).0 + mk(2).0;` drops 2 then 1 at its `;`: 21 on the next statement; n = 3.
//! 2. `let _k = mk(4);` is a variable: nothing dropped: 21.    3. at the block's end: 214.
//! A wrong builder that keeps temporaries to the block's end, or drops them in creation order,
//! returns 1.
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

fn mk(id: u32, log: *mut u32) -> D {
    D(id, log)
}

fn read(log: *mut u32) -> u32 {
    // SAFETY: as in `D::drop`.
    unsafe { *log }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let (after_stmt, n, after_let) = {
        let n = mk(1, log).0 + mk(2, log).0;
        let after_stmt = read(log);
        let _k = mk(4, log);
        (after_stmt, n, read(log))
    };
    if after_stmt != 21 || n != 3 {
        1
    } else if after_let != 21 {
        2
    } else if read(log) != 214 {
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
