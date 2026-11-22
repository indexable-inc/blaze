//! x0-move-13: a `ref mut` binding in a `match` on a place behind a reference changes that
//! place, not a copy (no_core; success 42, else the failed check's number). Written by the
//! wrong-answers lane (2026-11-16, WRONG-NOTES.md "Runner 5", S5a).
//!
//! The Reference, "Match expressions": "If the scrutinee expression is a place expression, the
//! match does not allocate a temporary location"; "Identifier patterns": "`ref mut` .. binds a
//! mutable reference to the matched place"; "Binding modes": a non-reference pattern matched
//! against `&mut T` binds by `ref mut`.
//! 1. `match *r { (ref mut a, _) => *a = 10 }` with `r = &mut p`: `p.0` is 10.
//! 2. `match r { (a, _) => *a += 5 }` (default binding mode): `p.0` is 15.
//! 3. `match *o { Some(ref mut v) => *v = 9, None => {} }` with `o = &mut Some(3)`: 9.
//! 4. `match *s { Pair { ref mut y, .. } => *y = 4 }` behind `s = &mut q`: `q.y` is 4.
//! A builder that matches a copy of the place returns 1, 3 or 4.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct Pair {
    x: u32,
    y: u32,
}

fn first(t: &mut (u32, u32)) -> u32 {
    match *t {
        (ref mut a, _) => *a = 10,
    }
    t.0
}

fn bump(t: &mut (u32, u32)) {
    match t {
        (a, _) => *a += 5,
    }
}

fn set(o: &mut Option<u32>) {
    match *o {
        Some(ref mut v) => *v = 9,
        None => {}
    }
}

fn set_y(s: &mut Pair) {
    match *s {
        Pair { ref mut y, .. } => *y = 4,
    }
}

fn check() -> i32 {
    let mut p = (1u32, 2u32);
    if first(&mut p) != 10 || p.0 != 10 {
        return 1;
    }
    bump(&mut p);
    if p.0 != 15 || p.1 != 2 {
        return 2;
    }
    let mut o = Some(3u32);
    set(&mut o);
    let got = match o {
        Some(v) => v,
        None => 0,
    };
    if got != 9 {
        return 3;
    }
    let mut q = Pair { x: 1, y: 2 };
    set_y(&mut q);
    if q.y != 4 || q.x != 1 {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
