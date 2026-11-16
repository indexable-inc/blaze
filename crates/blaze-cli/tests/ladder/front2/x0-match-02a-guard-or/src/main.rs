//! x0-match-02a: the reduction of x0-match-02-guards' wrong answer (2026-11-16: status 4, its
//! guard ran once where Rust runs it twice): a guard on an arm with or-patterns (no_core;
//! success 42, else the failed check's number).
//!
//! The Reference, "Match expressions", "Match guards": [expr.match.guard.behavior] "When the
//! pattern matches successfully, the pattern guard is executed."; [expr.match.guard.next]
//! "Otherwise, the next pattern, including other matches with the `|` operator in the same
//! arm, is tested."; its note: "Multiple matches using the `|` operator can cause the pattern
//! guard and the side effects it has to execute multiple times", with the example
//! `match 1 { 1 | _ if { i.set(i.get() + 1); false } => {} _ => {} }` after which `i` is 2.
//! "Patterns", [patterns.behavior.nested-or-patterns]: "The dynamic semantics of pattern
//! matching a scrutinee expression `e_s` against a pattern `c(p | q, ..rest)` at depth `d`
//! where `c` is some constructor, `p` and `q` are arbitrary patterns, and `rest` is optionally
//! any remaining potential factors in `c`, is defined as being the same as that of
//! `c(p, ..rest) | c(q, ..rest)`."
//!
//! 1. `1 | _ if bump(p)` on 1: both alternatives match, the guard runs for each: 2 runs.
//!    (The wrong builder: 1 run, the arm left after the first false guard.)
//! 2. `(a, _) | (_, a) if a == 2 => 10 + a` on (1, 2): the first alternative binds `a = 1`,
//!    its guard is false; the second binds `a = 2`, its guard is true: 12. (The wrong builder:
//!    0, the next arm: a wrong VALUE, with no effect in the guard.)
//! 3. `(1 | _, 2 | _) if bump(p)` on (1, 2): `(1, 2) | (1, _) | (_, 2) | (_, _)` all match:
//!    4 runs.
//! 4. `(a, b) | (b, a) if note(p, a, b)` on (1, 2): the guard sees `a = 1, b = 2`, then
//!    `a = 2, b = 1`: the log is 12 then 21: 1221.
//! 5. `3 | 3 if bump(p)` on 3: two alternatives, both match: 2 runs.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]
#![allow(unreachable_patterns)]

mod prelude;
use prelude::*;

fn bump(p: *mut u32) -> bool {
    // SAFETY: `p` points at a live local of `check`, written only through it.
    unsafe { *p += 1 };
    false
}

fn note(p: *mut u32, a: u32, b: u32) -> bool {
    // SAFETY: as in `bump`.
    unsafe { *p = *p * 100 + a * 10 + b };
    false
}

fn second(t: (u32, u32)) -> u32 {
    match t {
        (a, _) | (_, a) if a == 2 => 10 + a,
        _ => 0,
    }
}

fn check() -> i32 {
    let mut runs = 0u32;
    let p: *mut u32 = &raw mut runs;
    match 1 {
        1 | _ if bump(p) => {}
        _ => {}
    }
    // SAFETY: as in `bump`.
    let one = unsafe { *p };
    unsafe { *p = 0 };
    match (1u32, 2u32) {
        (1 | _, 2 | _) if bump(p) => {}
        _ => {}
    }
    let four = unsafe { *p };
    unsafe { *p = 0 };
    match (1u32, 2u32) {
        (a, b) | (b, a) if note(p, a, b) => {}
        _ => {}
    }
    let log = unsafe { *p };
    unsafe { *p = 0 };
    match 3u32 {
        3 | 3 if bump(p) => {}
        _ => {}
    }
    let twice = unsafe { *p };
    if one != 2 {
        1
    } else if second((1, 2)) != 12 {
        2
    } else if four != 4 {
        3
    } else if log != 1221 {
        4
    } else if twice != 2 {
        5
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
