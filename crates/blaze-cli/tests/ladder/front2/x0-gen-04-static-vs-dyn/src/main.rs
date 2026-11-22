//! x0-gen-04: the runnable twin of x-gen-04-static-vs-dyn (no_core; success 42, else the
//! failed check's number). The Reference, "Trait objects": a call through `&dyn Eval` reaches
//! the same impl a direct call does. Lin { a, b } is a * x + b; Sq is x * x.
//! 1. Lin { 3, 4 } at 5: 19 both ways.   2. Sq at 7: 49 both ways.
//! 3. a generic `T: Eval + ?Sized` at `dyn Eval` and at `Lin`: Lin at 2: 10 both.
//! 4. `d.eval(2)` on the `&dyn` itself: 10.
//! A wrong builder that puts methods in the wrong vtable slot returns 1 or 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Eval {
    fn eval(&self, x: u32) -> u32;
}

struct Lin {
    a: u32,
    b: u32,
}

struct Sq;

impl Eval for Lin {
    fn eval(&self, x: u32) -> u32 {
        self.a * x + self.b
    }
}

impl Eval for Sq {
    fn eval(&self, x: u32) -> u32 {
        x * x
    }
}

fn by_dyn(e: &dyn Eval, x: u32) -> u32 {
    e.eval(x)
}

fn by_generic<T: Eval + ?Sized>(e: &T, x: u32) -> u32 {
    e.eval(x)
}

fn check() -> i32 {
    let lin = Lin { a: 3, b: 4 };
    let d: &dyn Eval = &lin;
    if lin.eval(5) != 19 || by_dyn(&lin, 5) != 19 {
        1
    } else if Sq.eval(7) != 49 || by_dyn(&Sq, 7) != 49 {
        2
    } else if by_generic(d, 2) != 10 || by_generic(&lin, 2) != 10 {
        3
    } else if d.eval(2) != 10 {
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
