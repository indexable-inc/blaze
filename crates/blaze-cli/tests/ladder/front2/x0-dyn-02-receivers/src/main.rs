//! x0-dyn-02: every way a call reaches a method whose receiver is a trait object (no_core;
//! success 42, else the failed check's number). The Reference, "Trait objects": a method
//! call on a trait object dispatches through its vtable to the concrete type's implementation;
//! "Method-call expressions" and "Paths" (qualified paths): `<dyn Tr as Tr>::m(t)` and
//! `Tr::m(t)` are the same call as `t.m()`; "Type coercions" (trait upcasting): `&dyn Sub`
//! coerces to `&dyn Base` and the result dispatches to the same type's `Base` methods.
//! Two overrides the provided `m` (2, default 1) and Base's provided `b` (20, default 10).
//! 1. `g::<dyn Tr>(t)` with `g<T: Tr + ?Sized>` calling `t.m()`: 2 (the instance's callee is
//!    `Tr::m` at `Self = dyn Tr`: dispatched, not the default).
//! 2. `<dyn Tr as Tr>::m(t)`: 2.   3. `Tr::m(t)`: 2.
//! 4. `&dyn Sub` upcast to `&dyn Base`, then `b()`: 20.
//! 5. `(*p).m()` on `p: *const dyn Tr`: 2.
//! 7. a provided method of the subtrait (`sb = self.b() + 1`) through `&dyn Sub`: 21.
//! (6, an impl FOR the object type, is x0-dyn-04.)
//! A builder that calls a default body directly where the vtable decides returns 1..7.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Base {
    fn b(&self) -> u32 {
        10
    }
}

trait Tr {
    fn m(&self) -> u32 {
        1
    }
}

trait Sub: Base {
    fn sb(&self) -> u32 {
        self.b() + 1
    }
}

struct Two;

impl Tr for Two {
    fn m(&self) -> u32 {
        2
    }
}

impl Base for Two {
    fn b(&self) -> u32 {
        20
    }
}

impl Sub for Two {}

fn g<T: Tr + ?Sized>(t: &T) -> u32 {
    t.m()
}

fn qualified(t: &dyn Tr) -> u32 {
    <dyn Tr as Tr>::m(t)
}

fn by_trait_path(t: &dyn Tr) -> u32 {
    Tr::m(t)
}

fn upcast(t: &dyn Sub) -> u32 {
    let b: &dyn Base = t;
    b.b()
}

fn through_raw(p: *const dyn Tr) -> u32 {
    unsafe { (*p).m() }
}

fn sb_of(t: &dyn Sub) -> u32 {
    t.sb()
}

// Every object trait's every method is called through an object (`b` through `dyn Base`, `sb`
// through `dyn Sub`, `m` through `dyn Tr`), so every vtable word's body is reached by a call.
fn b_of(t: &dyn Sub) -> u32 {
    t.b()
}

fn check() -> i32 {
    let two = Two;
    let t: &dyn Tr = &two;
    if g::<dyn Tr>(t) != 2 {
        1
    } else if qualified(&two) != 2 {
        2
    } else if by_trait_path(&two) != 2 {
        3
    } else if upcast(&two) != 20 {
        4
    } else if through_raw(t as *const dyn Tr) != 2 {
        5
    } else if sb_of(&two) != 21 || b_of(&two) != 20 {
        7
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
