//! x0-int-05: the runnable twin of x-int-05-casts (no_core, edition 2024; success is status 42,
//! a failed check returns its number). Same checks; each value is derived there from the
//! Reference's "Numeric cast" rules (same size: no-op in two's complement; larger to smaller:
//! truncate; smaller to larger: zero-extend unsigned, sign-extend signed). `u64::MAX` is
//! written `!0u64` (all 64 bits set, the Reference's `!` "Bitwise NOT").
//! A wrong builder that zero-extends signed values returns 4.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn check(m1: i8, big: i32, max: u64, b: u8, wide: i64) -> i32 {
    if m1 as u8 != 255 {
        return 1;
    }
    if big as u8 != 44 {
        return 2;
    }
    if max as i32 != -1 {
        return 3;
    }
    if m1 as i64 != -1 {
        return 4;
    }
    if m1 as u64 != !0u64 {
        return 5;
    }
    if b as i32 != 128 {
        return 6;
    }
    if (b + 127) as i8 != -1 {
        return 7;
    }
    if (m1 * 127 - 1) as u32 != 4294967168 {
        return 8;
    }
    if wide as u16 != 0x6789 {
        return 9;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(-1, 300, !0u64, 128, 0x1_2345_6789);
    if failed == 0 { 42 } else { failed }
}
