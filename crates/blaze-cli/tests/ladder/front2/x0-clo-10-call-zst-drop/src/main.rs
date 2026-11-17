//! x0-clo-10: x0-clo-09's zero-sized `move` closure capturing a zero-sized `Drop` value,
//! CALLED (no_core; success 42, else the failed check's number). Written by the wrong-answers
//! lane (2026-11-16, WRONG-NOTES.md "Runner 6", item 4): blaze-build closure.rs `closure_value`
//! returns early for a closure of no bytes; if that skipped marking `z` moved, `z` would drop
//! at its own scope's end AND in the closure's drop: twice.
//!
//! The Reference, "Closure expressions": "If the `move` keyword is used, then all captures are
//! by move"; "Destructors" ([destructors.intro]): "When an initialized variable or temporary
//! goes out of scope, its destructor is run"; a moved-from variable is not initialized. So:
//! - `f` called through `Fn` (`by_ref(&f)`) does not consume it: nothing drops at the call;
//!   `z` drops once, when `f` goes out of scope (LOG 1).
//! - `g` called through `FnOnce` by value (`once(g)`): `g` moves into `once` and is consumed by
//!   the call, so its capture drops when the closure body's `self` is dropped, inside `once`,
//!   before it returns (LOG 11 after the call); nothing more at the scope's end.
//! Each closure returns 7 when called.
//! The calls go through generic fns (as x0-clo-04): a direct call of a closure needs the
//! `call` lang fn the twin prelude lacks.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

static mut LOG: i32 = 0;

struct Z;

impl Drop for Z {
    fn drop(&mut self) {
        unsafe {
            LOG = LOG * 10 + 1;
        }
    }
}

fn by_ref<F: Fn() -> i32>(f: &F) -> i32 {
    f()
}

fn once<F: FnOnce() -> i32>(f: F) -> i32 {
    f()
}

fn check() -> i32 {
    {
        let z = Z;
        let f = move || {
            let _seen = &z;
            7
        };
        if by_ref(&f) != 7 {
            return 1;
        }
        if unsafe { LOG } != 0 {
            return 2;
        }
    }
    if unsafe { LOG } != 1 {
        return 3;
    }
    {
        let z = Z;
        let g = move || {
            let _seen = &z;
            7
        };
        if once(g) != 7 {
            return 4;
        }
        if unsafe { LOG } != 11 {
            return 5;
        }
    }
    if unsafe { LOG } != 11 {
        return 6;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
