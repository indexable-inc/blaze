//! Programs the twin rungs do not cover, each a `no_core` program over a twin's prelude (read
//! from the rung named [`PRELUDE_OF`]): the layout questions front_equal.rs must see both
//! programs answer, `valid_range` above all (the lazy program has no answer of its own for
//! it, program/lazy/front_end.rs; the whole program reads the ADT row's bounds,
//! program.rs `valid_range`).

/// The twin whose `src/prelude.rs` each fixture uses as its own.
pub const PRELUDE_OF: &str = "x0-adt-06-discriminants";

/// One fixture: its name and its `src/main.rs`.
pub struct Fixture {
    pub name: &'static str,
    pub main: &'static str,
}

pub const FIXTURES: &[Fixture] = &[Fixture { name: "fixture-niches", main: NICHES }];

/// A niche in `Option<&u8>`, a `rustc_layout_scalar_valid_range_start` struct in an `Option`,
/// and a `repr(i8)` enum with negative discriminants: each asks `adt`, `repr`, `valid_range`
/// and `discriminant` of the ADTs it names. Status 42 when run, as a twin.
const NICHES: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[rustc_layout_scalar_valid_range_start(1)]
struct Nz(u32);

#[repr(i8)]
enum Low {
    A = -3,
    B = -1,
    C = 4,
}

fn first(x: Option<&u8>) -> u8 {
    match x {
        Some(v) => *v,
        None => 0,
    }
}

fn wrapped(n: Option<Nz>) -> u32 {
    match n {
        Some(Nz(v)) => v,
        None => 7,
    }
}

fn low(l: Low) -> i32 {
    l as i32
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let b = 9u8;
    let n = unsafe { Nz(5) };
    if first(Some(&b)) != 9 || first(None) != 0 {
        1
    } else if wrapped(Some(n)) != 5 || wrapped(None) != 7 {
        2
    } else if low(Low::A) != -3 || low(Low::B) != -1 || low(Low::C) != 4 {
        3
    } else {
        42
    }
}
";
