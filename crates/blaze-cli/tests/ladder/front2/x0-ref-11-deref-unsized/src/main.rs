//! x0-ref-11: a place behind an overloaded `Deref` to an unsized target (`[u8]`, `str`), used as
//! a place and not borrowed at once: indexed, matched by a slice pattern, borrowed by `&**w`.
//!
//! The Reference, "The dereference operator": "On non-pointer types `*x` is equivalent to
//! `*std::ops::Deref::deref(&x)` in an immutable place expression context". So the place is the
//! builtin dereference of the wide reference `deref` returns, exactly as `*r` of `r: &[u8]`: its
//! length is the reference's second word. "Dynamically Sized Types": "Pointers to slices also
//! store the number of elements of the slice."
//! w = W([5, 6, 7, 8]):
//! 1. `w[2]` (autoderef through `Deref::deref`, then the builtin slice index) is 7.
//! 2. `match *w { [a, .., d] => a + d == 13 }`: the last element is found by the length (4).
//! 3. `w[3]` through `fn third(w: &W)` (a builtin step, then the overloaded one) is 8.
//! 4. `&**w` through `fn as_slice(w: &W) -> &[u8]` has len 4 and element 1 is 6.
//! 5. `(*s).len()` of `S: Deref<Target = str>` over "hello" is 5.
//! A builder that reads the place as thin loses the length and fails 2 (or 4, 5); one that
//! refuses it does not build ("a place behind an overloaded dereference to an unsized target").
//! Exit 42.

use core::ops::Deref;

struct W([u8; 4]);

impl Deref for W {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

struct S(&'static str);

impl Deref for S {
    type Target = str;
    fn deref(&self) -> &str {
        self.0
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

fn main() -> std::entry::Status {
    let w = W([5, 6, 7, 8]);
    let s = S("hello");
    let matched = match *w {
        [a, .., d] => a + d == 13,
        _ => false,
    };
    let status = if w[2] != 7 {
        1
    } else if !matched || !ends(&w) {
        2
    } else if third(&w) != 8 {
        3
    } else if as_slice(&w).len() != 4 || as_slice(&w)[1] != 6 {
        4
    } else if (*s).len() != 5 {
        5
    } else {
        42
    };
    std::entry::Status(status)
}
