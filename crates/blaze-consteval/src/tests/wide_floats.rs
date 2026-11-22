//! `f16` and `f128` in constants: the constants of the pinned core's library/core/src/num/f128.rs
//! and f16.rs, written as core writes them, evaluate to the bit patterns IEEE 754 gives them
//! (tempo build census 7: "f16 and f128 in a constant is not evaluated yet", e.g.
//! `core::f128::<impl f128>::MAX`). The fixture world has no impl blocks, so each associated
//! constant is a free one here with core's own body; `to_bits`/`from_bits` are core's
//! (`unsafe { mem::transmute(self) }`) as `const fn`s.
//!
//! The patterns (binary128: 1 + 15 + 112 bits, bias 16383; binary16: 1 + 5 + 10, bias 15):
//! `f128::MAX` = (2 - 2^-112) * 2^16383 = 0x7ffe then 28 hex `f`s; `MIN` the same with the sign
//! bit (0xfffe...); `MIN_POSITIVE` = 2^-16382, biased exponent 1: 1 << 112; `EPSILON` = 2^-112,
//! biased 16383 - 112 = 0x3f8f: 0x3f8f << 112; `INFINITY` = exponent all ones: 0x7fff << 112;
//! `NEG_INFINITY` 0xffff << 112; `NAN` = 0.0 / 0.0, the preferred NaN 0x7fff8 << 108.
//! `f16::MAX` = 65504 = 0x7bff, `EPSILON` = 2^-10 = 0x1400, `MIN_POSITIVE` = 2^-14 = 0x0400,
//! `INFINITY` 0x7c00, `NAN` 0x7e00.
//!
//! Control: with the `FloatTy::F16 | FloatTy::F128` arms of machine.rs `repr` taken out, every
//! case is refused before any arithmetic; with soft.rs `round_pack` rounding toward zero (no
//! `m += 1`), `MAX`'s literal still gives `MAX` but `TENTH` (0.1 at f128) ends in 9.

use super::{eval_int, world};

const F128_MAX: u128 = 0x7ffe_ffff_ffff_ffff_ffff_ffff_ffff_ffff;

/// Prints `test tests::wide_floats::core_f128_constants ... ok`.
#[test]
fn core_f128_constants() {
    let mut w = world();
    w.const_fn("to_bits", &[("x", "f128")], "u128", "{ transmute::<f128, u128>(x) }");
    w.const_fn("from_bits", &[("v", "u128")], "f128", "{ transmute::<u128, f128>(v) }");
    w.konst("MAX", "f128", "1.18973149535723176508575932662800702e+4932_f128");
    w.konst("MIN", "f128", "-1.18973149535723176508575932662800702e+4932_f128");
    w.konst("MIN_POSITIVE", "f128", "3.36210314311209350626267781732175260e-4932_f128");
    w.konst("EPSILON", "f128", "1.92592994438723585305597794258492732e-34_f128");
    w.konst("NAN", "f128", "0.0_f128 / 0.0_f128");
    w.konst("INFINITY", "f128", "1.0_f128 / 0.0_f128");
    w.konst("NEG_INFINITY", "f128", "-1.0_f128 / 0.0_f128");
    w.konst("HI", "f128", "MAX * 0.5");
    w.konst("TENTH", "f128", "0.1_f128");
    w.konst("MAX_BITS", "u128", "to_bits(MAX)");
    w.konst("ROUND_TRIP", "bool", "from_bits(0x7ffe_ffff_ffff_ffff_ffff_ffff_ffff_ffff) == MAX");
    w.konst("ORDERED", "bool", "MIN < -1.0 && MIN_POSITIVE > 0.0 && EPSILON < 1.0 && !(NAN == NAN)");
    w.konst("AS_U8", "u8", "3.9_f128 as u8");
    w.konst("AS_F64", "f64", "0.1_f128 as f64");
    w.build();
    assert_eq!(eval_int(&mut w, "MAX"), Ok(F128_MAX));
    assert_eq!(eval_int(&mut w, "MIN"), Ok(F128_MAX | 1 << 127));
    assert_eq!(eval_int(&mut w, "MIN_POSITIVE"), Ok(1 << 112));
    assert_eq!(eval_int(&mut w, "EPSILON"), Ok(0x3f8f << 112));
    assert_eq!(eval_int(&mut w, "NAN"), Ok(0x7fff8 << 108));
    assert_eq!(eval_int(&mut w, "INFINITY"), Ok(0x7fff << 112));
    assert_eq!(eval_int(&mut w, "NEG_INFINITY"), Ok(0xffff << 112));
    assert_eq!(eval_int(&mut w, "HI"), Ok(0x7ffd_ffff_ffff_ffff_ffff_ffff_ffff_ffff));
    assert_eq!(eval_int(&mut w, "TENTH"), Ok(0x3ffb_9999_9999_9999_9999_9999_9999_999a));
    assert_eq!(eval_int(&mut w, "MAX_BITS"), Ok(F128_MAX));
    assert_eq!(eval_int(&mut w, "ROUND_TRIP"), Ok(1));
    assert_eq!(eval_int(&mut w, "ORDERED"), Ok(1));
    assert_eq!(eval_int(&mut w, "AS_U8"), Ok(3));
    assert_eq!(eval_int(&mut w, "AS_F64"), Ok(u128::from(0.1f64.to_bits())));
}

/// Prints `test tests::wide_floats::core_f16_constants ... ok`.
#[test]
fn core_f16_constants() {
    let mut w = world();
    w.konst("MAX", "f16", "6.5504e+4_f16");
    w.konst("MIN", "f16", "-6.5504e+4_f16");
    w.konst("EPSILON", "f16", "9.7656e-4_f16");
    w.konst("MIN_POSITIVE", "f16", "6.1035e-5_f16");
    w.konst("NAN", "f16", "0.0_f16 / 0.0_f16");
    w.konst("INFINITY", "f16", "1.0_f16 / 0.0_f16");
    w.konst("HI", "f16", "MAX * 0.5");
    w.konst("AS_F16", "f16", "65504u32 as f16");
    w.konst("MAX_BITS", "u16", "transmute::<f16, u16>(MAX)");
    w.build();
    assert_eq!(eval_int(&mut w, "MAX"), Ok(0x7bff));
    assert_eq!(eval_int(&mut w, "MIN"), Ok(0xfbff));
    assert_eq!(eval_int(&mut w, "EPSILON"), Ok(0x1400));
    assert_eq!(eval_int(&mut w, "MIN_POSITIVE"), Ok(0x0400));
    assert_eq!(eval_int(&mut w, "NAN"), Ok(0x7e00));
    assert_eq!(eval_int(&mut w, "INFINITY"), Ok(0x7c00));
    assert_eq!(eval_int(&mut w, "HI"), Ok(0x77ff));
    assert_eq!(eval_int(&mut w, "AS_F16"), Ok(0x7bff));
    assert_eq!(eval_int(&mut w, "MAX_BITS"), Ok(0x7bff));
}
