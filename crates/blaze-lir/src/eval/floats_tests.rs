//! The evaluator's floats ([`super::floats`]): IEEE 754 arithmetic, comparisons with NaN,
//! saturating conversions, an open result refused by name, and the NaN-blind comparison a
//! harness uses. Each expectation is from LIR's own semantics (docs/v2/plan/design/lir.md and
//! the operation's doc in op.rs), quoted at the test, and IEEE 754 binary64.

use std::alloc::Global;

use super::{EvalError, Outcome, Val, agree, run_in};
use crate::{
    builder::{Builder, Full, Param, Variadic},
    flags::{FloatCond, Flags},
    func::{BlockKind, Conv, Func, Value},
    op::Op,
    ty::Ty,
};

const FUEL: u64 = 100_000;
const NAN: u128 = 0x7FF8_0000_0000_0000;
const ONE: u128 = 0x3FF0_0000_0000_0000;

/// A builder of a function taking nothing and returning `rets`, its entry block begun.
fn start(rets: &[Ty]) -> Builder<Global, Full> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let rets: Vec<Param> = rets.iter().map(|&it| Param::of(it)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &[], &rets);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b
}

fn ran(f: &Func<Global>) -> Outcome<Global> {
    run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it")
}

/// `0.1 + 0.2` in binary64 is the double just above 0.3, bits 0x3FD3333333333334, rounded to
/// nearest as IEEE 754 asks (lir.md: "`f16`, `f32`, `f64`, `f128`, with their arithmetic as
/// ops").
/// Expected: `test eval::floats_tests::f64_addition_rounds_as_ieee_754 ... ok`.
#[test]
fn f64_addition_rounds_as_ieee_754() {
    let mut b = start(&[Ty::F64]);
    let a = b.fconst(Ty::F64, 0x3FB9_9999_9999_999A);
    let c = b.fconst(Ty::F64, 0x3FC9_9999_9999_999A);
    let sum = b.binary(Op::Fadd, Flags::NONE, a, c);
    b.ret(&[sum]);
    let f = b.finish_checked().expect("f builds");
    assert_eq!(ran(&f), Outcome::<Global>::Returned(vec![Val::Int(0x3FD3_3333_3333_3334)]));
}

/// `fcmp` with a NaN: an unordered condition holds (`uno NaN, 1.0` is 1, `une NaN, NaN` is 1)
/// and an ordered one does not (`oeq NaN, NaN` is 0).
/// Expected: `test eval::floats_tests::comparisons_with_nan_are_unordered ... ok`.
#[test]
fn comparisons_with_nan_are_unordered() {
    let mut b = start(&[Ty::I1, Ty::I1, Ty::I1]);
    let (nan, one) = (b.fconst(Ty::F64, NAN), b.fconst(Ty::F64, ONE));
    let uno = b.fcmp(FloatCond::Uno, nan, one);
    let oeq = b.fcmp(FloatCond::Oeq, nan, nan);
    let une = b.fcmp(FloatCond::Une, nan, nan);
    b.ret(&[uno, oeq, une]);
    let f = b.finish_checked().expect("f builds");
    assert_eq!(ran(&f), Outcome::<Global>::Returned(vec![Val::Int(1), Val::Int(0), Val::Int(1)]));
}

/// `fptosi`/`fptoui`: "Saturates (Rust's `as`), unless `in_range` (`to_int_unchecked`). NaN is
/// 0." So 1e10 to `i32` is 2147483647, NaN to `i32` is 0, -1.5 to `u8` is 0 and 300.0 to `u8`
/// is 255.
/// Expected: `test eval::floats_tests::float_to_integer_saturates_and_nan_is_zero ... ok`.
#[test]
fn float_to_integer_saturates_and_nan_is_zero() {
    let mut b = start(&[Ty::I32, Ty::I32, Ty::I8, Ty::I8]);
    let big = b.fconst(Ty::F64, 0x4202_A05F_2000_0000);
    let nan = b.fconst(Ty::F64, NAN);
    let negative = b.fconst(Ty::F64, 0xBFF8_0000_0000_0000);
    let wide = b.fconst(Ty::F64, 0x4072_C000_0000_0000);
    let a = b.convert(Op::FptoSi, Flags::NONE, Ty::I32, big);
    let c = b.convert(Op::FptoSi, Flags::NONE, Ty::I32, nan);
    let d = b.convert(Op::FptoUi, Flags::NONE, Ty::I8, negative);
    let e = b.convert(Op::FptoUi, Flags::NONE, Ty::I8, wide);
    b.ret(&[a, c, d, e]);
    let f = b.finish_checked().expect("f builds");
    let want = vec![Val::Int(0x7FFF_FFFF), Val::Int(0), Val::Int(0), Val::Int(255)];
    assert_eq!(ran(&f), Outcome::<Global>::Returned(want));
}

/// `fmin_num`: "a NaN operand gives the other", so `fmin_num NaN, 2.0` is 2.0; of +0 and -0
/// IEEE 754-2008 lets either come back, so the evaluator refuses it by name instead of picking.
/// Expected: `test eval::floats_tests::fmin_num_takes_the_number_and_refuses_signed_zeros ... ok`.
#[test]
fn fmin_num_takes_the_number_and_refuses_signed_zeros() {
    let two = 0x4000_0000_0000_0000;
    let min = |a: u128, c: u128| -> (Func<Global>, Value) {
        let mut b = start(&[Ty::F64]);
        let (a, c) = (b.fconst(Ty::F64, a), b.fconst(Ty::F64, c));
        let m = b.binary(Op::FminNum, Flags::NONE, a, c);
        b.ret(&[m]);
        (b.finish_checked().expect("f builds"), m)
    };
    let (f, _) = min(NAN, two);
    assert_eq!(ran(&f), Outcome::<Global>::Returned(vec![Val::Int(two)]));
    let (f, m) = min(0, 1 << 63);
    let refused = run_in(f.view(), &[], FUEL, Global).map(|_| ());
    let what = "an `fmin_num` or `fmax_num` of +0 and -0, whose result LIR leaves open";
    assert_eq!(refused, Err(EvalError::Unsupported { inst: m.0 as usize, op: Op::FminNum, what }));
}

/// The harness's comparison: two NaNs of `f64` agree whatever their payloads and signs (IEEE
/// 754 leaves a result NaN's payload open), 1.0 and 2.0 do not, and an `i64` compares by bits.
/// Expected: `test eval::floats_tests::nans_agree_whatever_their_payloads ... ok`.
#[test]
fn nans_agree_whatever_their_payloads() {
    let other_nan = 0xFFF0_0000_0000_0001;
    assert!(agree(Ty::F64, 64, Val::Int(NAN), Val::Int(other_nan)));
    assert!(!agree(Ty::F64, 64, Val::Int(ONE), Val::Int(0x4000_0000_0000_0000)));
    assert!(!agree(Ty::I64, 64, Val::Int(NAN), Val::Int(other_nan)));
    assert!(agree(Ty::I64, 64, Val::Uninit, Val::Int(3)));
}
