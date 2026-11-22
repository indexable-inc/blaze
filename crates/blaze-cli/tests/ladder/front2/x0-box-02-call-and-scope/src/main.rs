//! x0-box-02: a `Box` moved into a call, and one dropped at its scope's end (no_core; success
//! 42, else the failed check's number). The moves OUT of a box are x0-box-01 (refused by the
//! builder today: "a move out of a dereference whose type needs drop").
//! The twin declares its own `#[lang = "owned_box"]` box over a raw pointer into a local array
//! (the "heap"), whose `Drop` frees nothing and logs 9 (std's `impl Drop for Box` only
//! deallocates; the contents are dropped by the box's drop glue before it,
//! library/alloc/src/boxed.rs). The Reference, "Destructors": a value moved into a call is the
//! callee's and dropped at the callee's end, never by the caller; a box's glue drops its
//! contents, then runs its `Drop`. Each drop appends: log = log * 10 + id.
//! 1. `eat(box(D 4))`: in `eat` at its end, D then the free: 49; nothing after.
//! 2. `let b = box(D 7);` not moved: D (7) then the free (9): 79.
//! 3. `let b = box(D 3); let c = b;` (a move between locals): one drop, 39.
//! A builder that drops a moved box again, or never runs its contents' drop, returns 1..3.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[lang = "owned_box"]
pub struct Box<T: ?Sized>(*mut T);

impl<T: ?Sized> Drop for Box<T> {
    fn drop(&mut self) {
        log(9);
    }
}

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

fn eat(_b: Box<D>) -> u64 {
    0
}

fn moved_into_call(heap: &mut [u64; 4]) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    eat(b);
}

fn not_moved(heap: &mut [u64; 4]) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    let _ = &b;
}

fn moved_between_locals(heap: &mut [u64; 4]) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    let c = b;
    let _ = &c;
}

fn check() -> i32 {
    let mut heap: [u64; 4] = [4, 0, 0, 0];
    moved_into_call(&mut heap);
    if take_log() != 49 {
        return 1;
    }
    heap = [7, 0, 0, 0];
    not_moved(&mut heap);
    if take_log() != 79 {
        return 2;
    }
    heap = [3, 0, 0, 0];
    moved_between_locals(&mut heap);
    if take_log() != 39 {
        return 3;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
