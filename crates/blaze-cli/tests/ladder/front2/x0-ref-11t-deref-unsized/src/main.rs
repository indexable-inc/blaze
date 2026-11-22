//! x0-ref-11t: the runnable no_core twin of x0-ref-11-deref-unsized (success 42, else the failed
//! check's number): a place behind an overloaded `Deref` to an unsized target (`[u8]`), used as
//! a place and not borrowed at once. `src/prelude.rs` is the twins' one prelude, byte-equal;
//! `Deref` is declared here as the pinned core declares it (library/core/src/ops/deref.rs:
//! `#[lang = "deref"] pub const trait Deref: PointeeSized { #[lang = "deref_target"] type
//! Target: ?Sized; fn deref(&self) -> &Self::Target; }`, without `const` and the attributes).
//!
//! The Reference, "The dereference operator": "On non-pointer types `*x` is equivalent to
//! `*std::ops::Deref::deref(&x)` in an immutable place expression context". So the place is the
//! builtin dereference of the wide reference `deref` returns, exactly as `*r` of `r: &[u8]`: its
//! length is the reference's second word ("Dynamically Sized Types": "Pointers to slices also
//! store the number of elements of the slice").
//! w = W([5, 6, 7, 8]):
//! 1. `w[2]` (autoderef through `Deref::deref`, then the builtin slice index) is 7.
//! 2. `match *w { [a, .., d] => a + d == 13 }` and `match **w` through `&W`: the last element is
//!    found by the length (4).
//! 3. `w[3]` through `fn third(w: &W)` (a builtin step, then the overloaded one) is 8.
//! 4. `&**w` through `fn as_slice(w: &W) -> &[u8]` matches `[_, b, _, _]` with `b == 6`.
//! WITHOUT THE BUILDER CHANGE (walk.rs `deref_call`, `push_index_base`) this twin does not build:
//! check 1's base is refused ("indexing a slice not reached by a builtin dereference of a wide
//! pointer") and check 2's place ("a place behind an overloaded dereference to an unsized
//! target"). A builder that reads the place as thin loses the length and exits 2 (or 4).
//! The std original's `(*s).len()` on a `str` target is left out: no_core has no `str::len`.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[lang = "deref"]
pub trait Deref: PointeeSized {
    #[lang = "deref_target"]
    type Target: ?Sized;
    fn deref(&self) -> &Self::Target;
}

struct W([u8; 4]);

impl Deref for W {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

fn third(w: &W) -> u8 {
    w[3]
}

fn as_slice(w: &W) -> &[u8] {
    &**w
}

fn ends(w: &W) -> bool {
    match **w {
        [a, .., d] => a + d == 13,
        _ => false,
    }
}

fn second_of_four(s: &[u8]) -> bool {
    match s {
        [_, b, _, _] => *b == 6,
        _ => false,
    }
}

fn check() -> i32 {
    let w = W([5, 6, 7, 8]);
    let matched = match *w {
        [a, .., d] => a + d == 13,
        _ => false,
    };
    if w[2] != 7 {
        1
    } else if !matched || !ends(&w) {
        2
    } else if third(&w) != 8 {
        3
    } else if !second_of_four(as_slice(&w)) {
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
