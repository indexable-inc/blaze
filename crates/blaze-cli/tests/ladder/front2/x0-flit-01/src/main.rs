// Own program x0-flit-01 (front2): float LITERAL EXPRESSIONS, each literal's value read from
// its text by the seam (`float_lit`: the text rounded once to the nearest `f64` and once to the
// nearest `f32`, picked by the type the check gave the literal), compared by BIT PATTERN.
//
// The Reference, "Literal expressions", "Floating-point literal expressions": "The value of the
// expression is determined from the string representation of the token as follows: Any suffix
// is removed from the string. Any underscores are removed from the string. The rest of the
// string is converted to the expression's type as if by `f32::from_str` or `f64::from_str`."
// And: "If the token has no suffix, the expression's type is determined by type inference".
//
// The bits are read as core's `f32::to_bits` and `f64::to_bits` read them (library/core
// num/f32.rs: "unsafe { mem::transmute(self) }"): a `#![no_core]` program has no `to_bits`, so
// the `transmute` intrinsic is declared here. Each expectation is the IEEE 754 value nearest
// the decimal, in hex:
//  1. 0.1f32                 0x3DCC_CCCD            (0x1.99999a p-4)
//  2. 1e-3 as f64            0x3F50_624D_D2F1_A9FC  (0x1.0624dd2f1a9fc p-10)
//  3. 1e-3 as f32            0x3A83_126F            (0x1.0624de p-10)
//  4. 2.5 as f32             0x4020_0000
//  5. 2.5 as f64             0x4004_0000_0000_0000
//  6. 1_000.5 as f64         0x408F_4400_0000_0000
//  7. 1_000.5 as f32         0x447A_2000
//  8. 0.1 as f64             0x3FB9_9999_9999_999A
//  9. 16777217.0 as f32      0x4B80_0000  (2^24 + 1 is a tie: to even, 2^24)
// 10. 16777217.0 as f64      0x4170_0000_1000_0000  (exact)
//
// The status is 42 when every check holds, else the number of the first that does not. A
// literal read as 0, as an integer's bits, at the other type's width (3, 4 and 7 against 2, 5
// and 6), with its underscore or its exponent dropped, or rounded the wrong way in its last
// bit (1, 2, 8, 9) gives that check's number. The run prints nothing:
// `front2/x0-flit-01: PASS (status 42, stdout byte-equal)` in tests/ladder_front2.rs.
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for f32 {}
impl Copy for f64 {}

#[rustc_intrinsic]
const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;

fn bits32(x: f32) -> u32 {
    unsafe { transmute::<f32, u32>(x) }
}

fn bits64(x: f64) -> u64 {
    unsafe { transmute::<f64, u64>(x) }
}

fn check() -> i32 {
    if bits32(0.1f32) != 0x3DCC_CCCD {
        return 1;
    }
    // Unsuffixed: the parameter's type is the literal's.
    if bits64(1e-3) != 0x3F50_624D_D2F1_A9FC {
        return 2;
    }
    if bits32(1e-3) != 0x3A83_126F {
        return 3;
    }
    // Unsuffixed: the `let`'s type is the literal's.
    let narrow: f32 = 2.5;
    if bits32(narrow) != 0x4020_0000 {
        return 4;
    }
    let wide: f64 = 2.5;
    if bits64(wide) != 0x4004_0000_0000_0000 {
        return 5;
    }
    if bits64(1_000.5) != 0x408F_4400_0000_0000 {
        return 6;
    }
    if bits32(1_000.5) != 0x447A_2000 {
        return 7;
    }
    if bits64(0.1) != 0x3FB9_9999_9999_999A {
        return 8;
    }
    if bits32(16777217.0) != 0x4B80_0000 {
        return 9;
    }
    if bits64(16777217.0) != 0x4170_0000_1000_0000 {
        return 10;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
