//! x0-dyn-05: dropping a value through a trait object (no_core; success 42, else the failed
//! check's number). The Reference, "Destructors": dropping a value of type `T` runs
//! `<T as Drop>::drop` if `T: Drop`, then drops its fields; for `dyn Tr` the concrete type's
//! glue is the vtable's drop entry ("Trait objects"); `ptr::drop_in_place` runs that glue.
//! Each drop appends its id: log = log * 10 + id. Every value here is dropped twice on purpose:
//! once by `drop_in_place` through `*mut dyn Tr`, once at its scope's end (its type is plain
//! data, so the second drop is sound).
//! 1. D(3): through the object, then at the end: 33.
//! 2. W { inner: D(4) } (no Drop of its own: the glue drops the field): 44.
//! 3. Plain (needs no drop: the drop entry is empty and the call does nothing): log unchanged.
//! 4. the object's `m` is still the concrete type's: D 1, W 2, Plain 3.
//! A builder that calls no glue through the object gives 3, 4; one that calls the wrong
//! type's gives another log; one that calls an empty entry crashes.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Tr {
    fn m(&self) -> u32;
}

struct D {
    log: *mut u32,
    id: u32,
}

impl Drop for D {
    fn drop(&mut self) {
        unsafe { *self.log = *self.log * 10 + self.id }
    }
}

struct W {
    inner: D,
}

struct Plain;

impl Tr for D {
    fn m(&self) -> u32 {
        1
    }
}

impl Tr for W {
    fn m(&self) -> u32 {
        2
    }
}

impl Tr for Plain {
    fn m(&self) -> u32 {
        3
    }
}

fn drop_object(p: *mut dyn Tr) {
    unsafe { drop_in_place(p) }
}

fn m_of(t: &dyn Tr) -> u32 {
    t.m()
}

fn check() -> i32 {
    let mut log: u32 = 0;
    let at: *mut u32 = &mut log;
    {
        let mut d = D { log: at, id: 3 };
        if m_of(&d) != 1 {
            return 4;
        }
        drop_object(&mut d);
    }
    if unsafe { *at } != 33 {
        return 1;
    }
    unsafe { *at = 0 };
    {
        let mut w = W { inner: D { log: at, id: 4 } };
        if m_of(&w) != 2 {
            return 4;
        }
        drop_object(&mut w);
    }
    if unsafe { *at } != 44 {
        return 2;
    }
    {
        let mut p = Plain;
        if m_of(&p) != 3 {
            return 4;
        }
        drop_object(&mut p);
    }
    if unsafe { *at } != 44 {
        return 3;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
