//! x0-float-03: the runnable twin of x-float-03-to-int (no_core; success 42, else the failed
//! check's number). The Reference, "Numeric cast": float to integer "will round the float
//! towards zero"; "NaN will return 0"; larger than the maximum (INFINITY included) "will
//! saturate to the maximum value", smaller than the minimum "to the minimum value".
//! 1. 1e10 as i32 = 2147483647.   2. -1e10 as i32 = -2147483648.   3. (0.0 / 0.0) as i32 = 0.
//! 4. -1.5 as u8 = 0.             5. 300.7 as u8 = 255.            6. -2.9 -> -2, 2.9 -> 2.
//! 7. (1.0 / 0.0) as u64 = !0u64.
//! A wrong builder that emits the bare hardware convert returns 1 or 3; one that rounds to
//! nearest returns 6.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn to_i32(x: f64) -> i32 {
    x as i32
}

fn to_u8(x: f64) -> u8 {
    x as u8
}

fn check(big: f64, zero: f64) -> i32 {
    if to_i32(big) != 2147483647 {
        return 1;
    }
    if to_i32(-big) != -2147483648 {
        return 2;
    }
    if to_i32(zero / zero) != 0 {
        return 3;
    }
    if to_u8(-1.5) != 0 {
        return 4;
    }
    if to_u8(300.7) != 255 {
        return 5;
    }
    if to_i32(-2.9) != -2 || to_i32(2.9) != 2 {
        return 6;
    }
    if (1.0 / zero) as u64 != !0u64 {
        return 7;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check(1e10, 0.0);
    if failed == 0 { 42 } else { failed }
}
