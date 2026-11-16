//! x0-int-10: the runnable twin of x-int-10-bits, checks 1 to 3 (no_core; success 42, else the
//! failed check's number). The Reference, "Negation operators": `!` on integers is bitwise NOT
//! (two's complement: !x = -x - 1); `&`, `|`, `^` bitwise AND, OR, XOR.
//! 1. !0u8 = 255; !5i32 = -6.      2. 0xF0 & 0x3C = 0x30.
//! 3. 0xF0 | 0x0F = 0xFF; 0xFF ^ 0x0F = 0xF0.
//! 4. bool: !true = false; true & false = false; true ^ true = false; false | true = true.
//! A wrong builder that negates (`-x`) for `!` returns 1.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn check(z: u8, f: i32, hi: u8, t: bool) -> i32 {
    if !z != 255 || !f != -6 {
        return 1;
    }
    if hi & 0x3C != 0x30 {
        return 2;
    }
    if hi | 0x0F != 0xFF || 0xFFu8 ^ 0x0F != hi {
        return 3;
    }
    if !t != false || t & !t != false || t ^ t != false || !t | t != true {
        return 4;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(0, 5, 0xF0, true);
    if failed == 0 { 42 } else { failed }
}
