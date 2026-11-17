//! x0-box-01: moves out of a `Box` (no_core; success 42, else the failed check's number). The
//! twin declares its own `#[lang = "owned_box"]` box over a raw pointer into a local array (the
//! "heap"), whose `Drop` frees nothing and logs 9 (std's `impl Drop for Box` only deallocates;
//! the contents are dropped by the box's drop glue before it, library/alloc/src/boxed.rs).
//! The Reference, "Destructors": a moved value is not dropped; a partially moved variable has
//! only its remaining fields dropped; locals are dropped in reverse order of declaration;
//! "Dereference expression": `*b` on a `Box` is a place that may be moved out of (the box is then
//! moved out of, and at its scope's end only freed). Each drop appends: log = log * 10 + id.
//! 1. `let b = box(D 3); let x = *b;` at the end: x (3), then b's free (9): 39.
//! 2. `eat(box(D 4))`: dropped in `eat` at its end, D then free: 49; nothing after.
//! 3. `let b = box(P { a: D 5, c: D 6 }); let x = b.a;` at the end: x (5), then b's rest `c` (6),
//!    then its free (9): 569.
//! 4. `let b = box(D 7);` not moved: D (7) then free (9): 79.
//! A builder that drops moved contents again, or never frees a moved-out box, returns 1..4.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[lang = "owned_box"]
pub struct Box<T: ?Sized>(*mut T);

// Declared as the pinned core declares it (library/core/src/ops/deref.rs, without `const` and
// the attributes), with alloc's impl for `Box`: `*b` on a box is the builtin dereference, but
// the checker reads the lang item for it.
#[lang = "deref"]
pub trait Deref: PointeeSized {
    #[lang = "deref_target"]
    type Target: ?Sized;
    fn deref(&self) -> &Self::Target;
}

impl<T: ?Sized> Deref for Box<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &**self
    }
}

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

/// One word, as the heap's cells.
struct D(u64);

impl Drop for D {
    fn drop(&mut self) {
        log(self.0);
    }
}

/// Two `D`s and no `Drop` of its own, so a field may be moved out of it.
struct P {
    a: D,
    c: D,
}

fn eat(_b: Box<D>) -> u64 {
    0
}

fn moved_out(heap: &mut [u64; 4]) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    let x = *b;
    let _ = &x;
}

fn moved_into_call(heap: &mut [u64; 4]) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    eat(b);
}

fn partly_moved(heap: &mut [u64; 4]) {
    let b: Box<P> = Box(heap as *mut [u64; 4] as *mut P);
    // Written `(*b).a`: the move out of a field goes through the box's builtin dereference
    // (`b.a` autoderefs the same way in rustc; this checker gave E0609 for it without a
    // `Deref` impl: it is kept explicit so the rung is about the move, not the autoderef).
    let x = (*b).a;
    let _ = &x;
}

fn not_moved(heap: &mut [u64; 4]) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    let _ = &b;
}

fn check() -> i32 {
    let mut heap: [u64; 4] = [3, 0, 0, 0];
    moved_out(&mut heap);
    if take_log() != 39 {
        return 1;
    }
    heap = [4, 0, 0, 0];
    moved_into_call(&mut heap);
    if take_log() != 49 {
        return 2;
    }
    heap = [5, 6, 0, 0];
    partly_moved(&mut heap);
    if take_log() != 569 {
        return 3;
    }
    heap = [7, 0, 0, 0];
    not_moved(&mut heap);
    if take_log() != 79 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
