//! x0-dyn-01: a PROVIDED method called on a trait object (no_core; success 42, else the
//! failed check's number). The Reference, "Trait objects": a method call on a trait object
//! dispatches through the object's vtable to the implementation of the concrete type, for a
//! provided method as for a required one; a provided method the impl does not override is, in
//! that type's vtable, the default body at that type ("Traits", provided methods).
//! 1. `m` through `&dyn Tr` on Two (overrides `m` to 2): 2.
//! 2. `m` through `&dyn Tr` on One (keeps the default 1): 1.
//! 3. a supertrait's provided `b` through `&dyn Sub` on Two (overrides `b` to 20): 20; on One: 10.
//! 4. a provided `p` that calls the required `req` (`req() + 100`), through `&dyn Tr`:
//!    One (req 5): 105; Two overrides `p` to `req() * 3` (req 7): 21.
//! 5. the same calls through `&mut dyn Tr`: 2 and 1.
//! 6. the required `req` (5, 7) and `s` (0) through the objects.
//! A builder that calls the trait's default body directly instead of the vtable's word returns
//! 1 (it runs the default 1 for Two), 3 or 4.
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
    fn req(&self) -> u32;
    fn m(&self) -> u32 {
        1
    }
    fn p(&self) -> u32 {
        self.req() + 100
    }
}

trait Sub: Base {
    fn s(&self) -> u32;
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
    fn p(&self) -> u32 {
        self.req() * 3
    }
}

impl Base for One {}

impl Base for Two {
    fn b(&self) -> u32 {
        20
    }
}

impl Sub for One {
    fn s(&self) -> u32 {
        0
    }
}

impl Sub for Two {
    fn s(&self) -> u32 {
        0
    }
}

fn m_of(t: &dyn Tr) -> u32 {
    t.m()
}

fn p_of(t: &dyn Tr) -> u32 {
    t.p()
}

fn b_of(t: &dyn Sub) -> u32 {
    t.b()
}

fn m_of_mut(t: &mut dyn Tr) -> u32 {
    t.m()
}

// Every method of every object trait is called through an object somewhere (`req`, `s`), so
// each vtable word's body is reached by a call too (a table's words whose methods nothing calls
// through `dyn` are x0-dyn-03's question).
fn req_of(t: &dyn Tr) -> u32 {
    t.req()
}

fn s_of(t: &dyn Sub) -> u32 {
    t.s()
}

fn check() -> i32 {
    let mut one = One;
    let mut two = Two;
    if m_of(&two) != 2 {
        1
    } else if m_of(&one) != 1 {
        2
    } else if b_of(&two) != 20 || b_of(&one) != 10 {
        3
    } else if p_of(&one) != 105 || p_of(&two) != 21 {
        4
    } else if m_of_mut(&mut two) != 2 || m_of_mut(&mut one) != 1 {
        5
    } else if req_of(&one) != 5 || req_of(&two) != 7 || s_of(&one) != 0 || s_of(&two) != 0 {
        6
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
