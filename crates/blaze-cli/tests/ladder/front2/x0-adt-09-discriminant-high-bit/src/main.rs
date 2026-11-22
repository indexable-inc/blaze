//! x0-adt-09: an enum discriminant within 64 bits but above `i64::MAX` (no_core; success 42,
//! else the failed check's number). Expected: `PASS x0-adt-09-discriminant-high-bit`.
//! The Reference, "Type layout", `[layout.repr.primitive.enum]`: "For field-less enums,
//! primitive representations set the size and alignment to be the same as the primitive type
//! of the same name."; "Enumerations", `[items.enum.discriminant.intro]`: "Each enum instance
//! has a discriminant: an integer logically associated to it that is used to determine which
//! variant it holds."; "Enum cast": "Casts an enum to its discriminant, then uses a numeric
//! cast if needed." So a `repr(usize)` discriminant is any `usize` value (core's
//! `AlignmentEnum::_Align1Shl63 = 1 << 63`), and widening it zero-extends.
//! 1. `A::Hi as usize == 1 << 63`: a sign-extending or 63-bit fix fails here.
//! 2. a `match` on `pick(true)` (A::Hi at run time) takes the `Hi` arm: a fix that drops the
//!    check without masking the switch's cases fails here.
//! 3. `B::X as u64 == 0xFFFF_FFFF_FFFF_FFFE`.
//! 4. `size_of::<A>() == 8`.
//! 5. a `match` on `B::Y` (by `pick_b(false)`) reaches `Y`.
//! 6. `A::Hi as u128 == 1 << 63`: the cast extends by the enum's `usize`, not as an `i64`.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[repr(usize)]
enum A {
    Lo = 1,
    Hi = 1 << 63,
}

#[repr(u64)]
enum B {
    X = 0xFFFF_FFFF_FFFF_FFFE,
    Y = 3,
}

fn pick(c: bool) -> A {
    if c { A::Hi } else { A::Lo }
}

fn pick_b(c: bool) -> B {
    if c { B::X } else { B::Y }
}

fn arm_a(a: A) -> i32 {
    match a {
        A::Lo => 10,
        A::Hi => 20,
    }
}

fn arm_b(b: B) -> i32 {
    match b {
        B::X => 30,
        B::Y => 40,
    }
}

fn check() -> i32 {
    if A::Hi as usize != 1usize << 63 || A::Lo as usize != 1 {
        1
    } else if arm_a(pick(true)) != 20 || arm_a(pick(false)) != 10 {
        2
    } else if B::X as u64 != 0xFFFF_FFFF_FFFF_FFFE || B::Y as u64 != 3 {
        3
    } else if size_of::<A>() != 8 {
        4
    } else if arm_b(pick_b(false)) != 40 || arm_b(pick_b(true)) != 30 {
        5
    } else if pick(true) as u128 != 1u128 << 63 {
        6
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
