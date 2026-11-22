//! x0-gen-02: the runnable twin of x-gen-02-provided-methods (no_core; success 42, else the
//! failed check's number). The Reference, "Traits": a provided method runs unless the impl
//! overrides it; inside it, `self.base()` is the impl's.
//! 1. Cat (base 3, default loud = base * 10): 30.   2. Dog (overrides loud = base + 1000): 1004.
//! 3. twice<T: Speak>: Cat 60, Dog 2008.             4. base: Cat 3, Dog 4.
//! A wrong builder that resolves Dog's `loud` to the default returns 2 (40).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Speak {
    fn base(&self) -> u32;
    fn loud(&self) -> u32 {
        self.base() * 10
    }
}

struct Cat;
struct Dog;

impl Speak for Cat {
    fn base(&self) -> u32 {
        3
    }
}

impl Speak for Dog {
    fn base(&self) -> u32 {
        4
    }
    fn loud(&self) -> u32 {
        self.base() + 1000
    }
}

fn twice<T: Speak>(t: &T) -> u32 {
    t.loud() + t.loud()
}

fn check() -> i32 {
    if Cat.loud() != 30 {
        1
    } else if Dog.loud() != 1004 {
        2
    } else if twice(&Cat) != 60 || twice(&Dog) != 2008 {
        3
    } else if Cat.base() != 3 || Dog.base() != 4 {
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
