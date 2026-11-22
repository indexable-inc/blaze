//! x0-dyn-03: a vtable whose methods are not all called through an object (no_core; success
//! 42, else the failed check's number). The Reference, "Trait objects": the object's vtable
//! holds every dispatchable method of the trait (and of its supertraits) for the concrete
//! type, whether or not the program calls it; a provided method the impl keeps is the default
//! body at that type. So a program that calls only `m` through `&dyn Tr` still builds a table
//! with `req` (the impl's) and `k` (the trait's default, at One).
//! 1. `m` through `&dyn Tr` on Two (overrides `m` to 2): 2.   2. on One (default 1): 1.
//! 3. `req` called directly on One (not through an object): 5.
//! A front end that reaches only the methods called through `dyn` leaves `req` and `k` out of
//! the table and the build is refused; a wrong table slot returns 1 or 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Tr {
    fn req(&self) -> u32;
    fn m(&self) -> u32 {
        1
    }
    fn k(&self) -> u32 {
        77
    }
}

struct One;
struct Two;

impl Tr for One {
    fn req(&self) -> u32 {
        5
    }
}

impl Tr for Two {
    fn req(&self) -> u32 {
        7
    }
    fn m(&self) -> u32 {
        2
    }
}

fn m_of(t: &dyn Tr) -> u32 {
    t.m()
}

fn check() -> i32 {
    if m_of(&Two) != 2 {
        1
    } else if m_of(&One) != 1 {
        2
    } else if One.req() != 5 {
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
