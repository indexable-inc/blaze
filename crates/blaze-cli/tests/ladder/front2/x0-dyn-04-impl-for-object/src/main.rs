//! x0-dyn-04: a trait implemented FOR an object type (no_core; success 42, else the failed
//! check's number). The Reference, "Implementations": `impl Other for dyn Tr` is an impl whose
//! self type is the trait object type; a call `t.o()` on `t: &dyn Tr` selects it statically (an
//! ordinary call: `Other` is not `dyn Tr`'s principal, so no vtable holds `o`), and inside it
//! `self.m()` is a method call on a trait object: dispatched through the vtable ("Trait objects").
//! Two overrides `m` (2; the default is 1).
//! 1. `t.o()` = `self.m() + 10`: 12.   2. `Other::o(t)`: 12.
//! A builder that resolves the inner `self.m()` to the default body returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Tr {
    fn m(&self) -> u32 {
        1
    }
}

trait Other {
    fn o(&self) -> u32;
}

impl Other for dyn Tr {
    fn o(&self) -> u32 {
        self.m() + 10
    }
}

struct Two;

impl Tr for Two {
    fn m(&self) -> u32 {
        2
    }
}

fn by_method(t: &dyn Tr) -> u32 {
    t.o()
}

fn by_path(t: &dyn Tr) -> u32 {
    Other::o(t)
}

fn check() -> i32 {
    if by_method(&Two) != 12 {
        1
    } else if by_path(&Two) != 12 {
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
