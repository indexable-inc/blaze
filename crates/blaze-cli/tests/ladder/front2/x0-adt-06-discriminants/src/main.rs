//! x0-adt-06: the runnable twin of x-adt-06-discriminants (no_core; success 42, else the failed
//! check's number). The Reference, "Discriminants": the first unspecified is 0; an unspecified
//! one is "one higher than the discriminant of the previous variant"; "Enum cast": "Casts an
//! enum to its discriminant, then uses a numeric cast if needed."
//! 1. F { X, Y, Z }: 0, 1, 2.
//! 2. E { A = 1, B = 5, C, D = -2, G }: 1, 5, 6, -2, -1.
//! 3. #[repr(u8)] H { P = 255, Q = 7 }: P as u8 255, as i8 -1, as u32 255; Q 7.
//! 4. chosen at run time: pick(true) = E::C: 6; pick(false) = E::D: -2.
//! A wrong builder that numbers variants by position returns 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

enum F {
    X,
    Y,
    Z,
}

enum E {
    A = 1,
    B = 5,
    C,
    D = -2,
    G,
}

#[repr(u8)]
enum H {
    P = 255,
    Q = 7,
}

fn pick(c: bool) -> E {
    if c { E::C } else { E::D }
}

fn check() -> i32 {
    if F::X as i32 != 0 || F::Y as i32 != 1 || F::Z as i32 != 2 {
        1
    } else if E::A as i32 != 1 || E::B as i32 != 5 || E::C as i32 != 6 || E::D as i32 != -2 || E::G as i32 != -1 {
        2
    } else if H::P as u8 != 255 || H::P as i8 != -1 || H::P as u32 != 255 || H::Q as u8 != 7 {
        3
    } else if pick(true) as i64 != 6 || pick(false) as i64 != -2 {
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
