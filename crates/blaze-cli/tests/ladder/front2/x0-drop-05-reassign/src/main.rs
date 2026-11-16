//! x0-drop-05: the runnable twin of x-drop-05-reassign (no_core; success 42, else the failed
//! check's number). The Reference, "Assignment expressions": assigning first drops "the value
//! at the assigned place, unless the place is an uninitialized local variable or an
//! uninitialized field of a local variable". log = log * 10 + id, through a raw pointer.
//! 1. `a = D(2)` over D(1): 1 dropped: 1.    2. `b = D(3)` into an uninitialized b: still 1.
//! 3. `p.0 = D(5)` over D(4): 14.            4. the block's end: p (5), b (3), a (2): 14532.
//! A wrong builder that forgets the old value returns 1.
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

fn read(log: *mut u32) -> u32 {
    // SAFETY: as in `D::drop`.
    unsafe { *log }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let (one, two, three) = {
        let mut a = D(1, log);
        a = D(2, log);
        let one = read(log);
        let b;
        b = D(3, log);
        let two = read(log);
        let mut p = (D(4, log), 0u8);
        p.0 = D(5, log);
        let _keep = (&a, &b, &p);
        (one, two, read(log))
    };
    if one != 1 {
        1
    } else if two != 1 {
        2
    } else if three != 14 {
        3
    } else if read(log) != 14532 {
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
