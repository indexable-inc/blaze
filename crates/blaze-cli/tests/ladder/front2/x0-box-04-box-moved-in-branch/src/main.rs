//! x0-box-04: a `Box`'s contents moved out in one branch only (no_core; success 42, else the
//! failed check's number). The twin declares its own `#[lang = "owned_box"]` box over a raw
//! pointer into a local array (the "heap"), whose `Drop` frees nothing and logs 9 (std's
//! `impl Drop for Box` only deallocates; "the T in the Box is dropped by the compiler before the
//! destructor is run", library/alloc/src/boxed.rs). The Reference, "Dereference expression":
//! `*b` on a `Box` may be moved out of; the box is then only freed at its scope's end. When only
//! one branch moves it, which drop runs is known at run time (a drop flag). Each drop appends:
//! log = log * 10 + id.
//! 1. `let b = box(D 3); if sel { let x = *b; } log(7);`
//!    sel: x at the inner block's end (3), the 7, then b's free alone (9): 379.
//! 2. the same, not sel: the 7, then b whole: D (3), then its free (9): 739.
//! 3. `let b = box(P { a: D 5, c: D 6 }); if sel { let x = (*b).a; } log(7);`
//!    sel: x (5), the 7, b's rest `c` (6), its free (9): 5769.
//! 4. the same, not sel: 7, then `a` (5), `c` (6), free (9): 7569.
//! A builder that drops moved contents again, skips unmoved ones, or never frees returns 1..4.
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

fn whole_in_branch(heap: &mut [u64; 4], sel: bool) {
    let b: Box<D> = Box(heap as *mut [u64; 4] as *mut D);
    if sel {
        let x = *b;
        let _ = &x;
    }
    log(7);
}

fn field_in_branch(heap: &mut [u64; 4], sel: bool) {
    let b: Box<P> = Box(heap as *mut [u64; 4] as *mut P);
    if sel {
        // Written `(*b).a` as in x0-box-01: the rung is about the move, not the autoderef.
        let x = (*b).a;
        let _ = &x;
    }
    log(7);
}

fn check() -> i32 {
    let mut heap: [u64; 4] = [3, 0, 0, 0];
    whole_in_branch(&mut heap, true);
    if take_log() != 379 {
        return 1;
    }
    heap = [3, 0, 0, 0];
    whole_in_branch(&mut heap, false);
    if take_log() != 739 {
        return 2;
    }
    heap = [5, 6, 0, 0];
    field_in_branch(&mut heap, true);
    if take_log() != 5769 {
        return 3;
    }
    heap = [5, 6, 0, 0];
    field_in_branch(&mut heap, false);
    if take_log() != 7569 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
