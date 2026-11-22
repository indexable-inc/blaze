//! x0-box-03: a local's field moved out in one branch only, and a moved field assigned again
//! (no_core; success 42, else the failed check's number). The Reference, "Destructors": a
//! partially moved variable has only its remaining fields dropped, in declaration order, at its
//! scope's end; whether a field was moved is known only at run time when only one branch moves
//! it (a drop flag); assigning to a moved field drops nothing (there is no old value), and the
//! new value is dropped with the variable. Each drop appends: log = log * 10 + id.
//! 1. `let p = P { a: D 1, c: D 2 }; if sel { let x = p.a; } log(7);`
//!    sel: x at the inner block's end (1), the 7, then p's rest `c` (2): 172.
//! 2. the same, not sel: the 7, then p whole, `a` then `c`: 712.
//! 3. `let mut p = P { a: D 1, c: D 2 }; let x = p.a; p.a = D 3;` at the end: x (declared
//!    later, dropped first: 1), then p: `a` (3), `c` (2): 132.
//! A builder that drops a moved field (or skips an unmoved one) returns 1..3.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

static mut LOG: u64 = 0;

fn log(id: u64) {
    unsafe { LOG = LOG * 10 + id }
}

fn take_log() -> u64 {
    let got = unsafe { LOG };
    unsafe { LOG = 0 };
    got
}

struct D(u64);

impl Drop for D {
    fn drop(&mut self) {
        log(self.0);
    }
}

/// Two `D`s and no `Drop` of its own, so a field may be moved out of it.
struct P {
    a: D,
    c: D,
}

fn moved_in_branch(sel: bool) {
    let p = P { a: D(1), c: D(2) };
    if sel {
        let x = p.a;
        let _ = &x;
    }
    log(7);
}

fn moved_then_assigned() {
    let mut p = P { a: D(1), c: D(2) };
    let x = p.a;
    p.a = D(3);
    let _ = &x;
    let _ = &p;
}

fn check() -> i32 {
    moved_in_branch(true);
    if take_log() != 172 {
        return 1;
    }
    moved_in_branch(false);
    if take_log() != 712 {
        return 2;
    }
    moved_then_assigned();
    if take_log() != 132 {
        return 3;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
