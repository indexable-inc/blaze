//! x0-clo-09: a `move` closure whose only capture is a zero-sized value with a destructor
//! (no_core; success 42, else the failed check's number). Written by the wrong-answers lane
//! (2026-11-16, WRONG-NOTES.md "Runner 5", S4): blaze-build's `closure_value` returned early
//! for a closure of no bytes before marking its by-value captures moved, so whether a binding
//! was moved was decided by how the closure is held (its size), not by the capture.
//!
//! The Reference, "Closure expressions": "If the `move` keyword is used, then all captures are
//! by move"; "Destructors", [destructors.intro]: "When an initialized variable or temporary
//! goes out of scope, its destructor is run"; a moved-from variable is not initialized, so `z`
//! is dropped once, by the closure's destructor, when `f` is dropped.
//! `f` is never called (a call needs the `call` lang fn the twin prelude lacks): only its
//! drops are the question.
//! 2. nothing has dropped before `drop(f)`: LOG = 0.
//! 3. `drop(f)` drops the captured `z`: LOG = 1.
//! 4. at the block's end nothing more drops (a wrong builder drops `z` again): LOG = 1.
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

fn check() -> i32 {
    {
        let z = Z;
        let f = move || {
            let _seen = &z;
            7
        };
        if unsafe { LOG } != 0 {
            return 2;
        }
        drop(f);
        if unsafe { LOG } != 1 {
            return 3;
        }
    }
    if unsafe { LOG } != 1 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
