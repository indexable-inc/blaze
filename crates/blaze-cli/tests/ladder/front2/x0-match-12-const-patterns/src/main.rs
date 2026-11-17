//! x0-match-12: constants named ONLY in patterns, of each scalar kind the builder compares
//! (no_core; success 42, else the failed check's number). The shape of check 1 to 3 is
//! bstd's `wire::Refusal::of` (crates/blaze-std/src/wire.rs), the hello-world path's reader of
//! every host answer: `match answer.code { code::OK => Ok(answer.value), .. other => .. }`.
//!
//! The Reference, "Patterns", "Path patterns": "Unqualified path patterns can refer to: enum
//! variants, structs, constants, associated constants. Qualified path patterns can only refer
//! to associated constants." And "Constant patterns": "When a constant `C` of type `T` is used
//! as a pattern, we first check that `T: PartialEq`. Furthermore we require that the value of
//! `C` has (recursive) structural equality"; the pattern matches a value equal to `C`'s value.
//! Every constant here is named in no expression, so its value reaches the builder only if the
//! constants a PATTERN names are evaluated like those an expression names.
//!
//! What is right, from those rules and arithmetic:
//! 1. read(0, 7): `code::OK` (0) matches: value 7.
//! 2. read(5, 7): `code::END` (5) matches: refused 5, value 1005.
//! 3. read(9, 7): neither constant: the binding `other` takes 9: value 2009.
//! 4. `NL` ('\n'): is_nl('\n') = 1, is_nl('n') = 0.
//! 5. `ON` (true): on(true) = 1, on(false) = 2.
//! 6. `NEG` (-3i8): neg(-3) = 1, neg(3) = 0 (the byte 0xFD read at the scrutinee's width).
//! 7. `BIG` (0x1_0000_0002u64): big(0x1_0000_0002) = 1, big(2) = 0 (a value cut to 32 bits
//!    would match 2).
//! 8. pair(0, true) = 1, pair(0, false) = 2, pair(1, true) = 3: constants inside a tuple pattern.
//! A WRONG builder that binds a constant as a variable answers 7 for read(5, 7) and fails 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

mod code {
    pub const OK: u32 = 0;
    pub const END: u32 = 5;
}

const NL: char = '\n';
const ON: bool = true;
const NEG: i8 = -3;
const BIG: u64 = 0x1_0000_0002;

struct Answer {
    code: u32,
    value: u64,
}

enum Read {
    Value(u64),
    End,
    NoSuchCode(u32),
}

fn of(answer: Answer) -> Read {
    match answer.code {
        code::OK => Read::Value(answer.value),
        code::END => Read::End,
        other => Read::NoSuchCode(other),
    }
}

fn read(c: u32, v: u64) -> u64 {
    match of(Answer { code: c, value: v }) {
        Read::Value(x) => x,
        Read::End => 1005,
        Read::NoSuchCode(9) => 2009,
        Read::NoSuchCode(_) => 3000,
    }
}

fn is_nl(c: char) -> u32 {
    match c {
        NL => 1,
        _ => 0,
    }
}

fn on(b: bool) -> u32 {
    match b {
        ON => 1,
        _ => 2,
    }
}

fn neg(x: i8) -> u32 {
    match x {
        NEG => 1,
        _ => 0,
    }
}

fn big(x: u64) -> u32 {
    match x {
        BIG => 1,
        _ => 0,
    }
}

fn pair(c: u32, b: bool) -> u32 {
    match (c, b) {
        (code::OK, ON) => 1,
        (code::OK, _) => 2,
        _ => 3,
    }
}

fn check() -> i32 {
    if read(0, 7) != 7 {
        1
    } else if read(5, 7) != 1005 {
        2
    } else if read(9, 7) != 2009 {
        3
    } else if is_nl('\n') != 1 || is_nl('n') != 0 {
        4
    } else if on(true) != 1 || on(false) != 2 {
        5
    } else if neg(-3) != 1 || neg(3) != 0 {
        6
    } else if big(0x1_0000_0002) != 1 || big(2) != 0 {
        7
    } else if pair(0, true) != 1 || pair(0, false) != 2 || pair(1, true) != 3 {
        8
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
