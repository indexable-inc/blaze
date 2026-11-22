//! `f128` operations emitted and RUN as calls of the real soft-float routines: this test
//! binary's own compiler_builtins exports (`__addtf3`, .., and libm's `fmodf128`, which
//! compiler_builtins exports too: src/math/mod.rs `full_availability`), bound by address as the
//! run path binds the program's (front2/builtins_shim.rs). Each result is checked against bits
//! worked out by hand from IEEE 754 binary128 (1 sign bit, 15 exponent bits of bias 16383, 112
//! fraction bits), every case exactly representable:
//!
//! | value | bits (high 64 of 128; the low 64 are zero) | why |
//! |---|---|---|
//! | 1.0 | `0x3fff_0000_0000_0000` | exponent 16383 = 0x3fff, fraction 0 |
//! | 2.0 | `0x4000_0000_0000_0000` | exponent 16384 |
//! | 3.0 | `0x4000_8000_0000_0000` | 1.5 * 2: fraction's top bit |
//! | 1.5 | `0x3fff_8000_0000_0000` | |
//! | 0.25 | `0x3ffd_0000_0000_0000` | exponent 16381 |
//! | 7.0 | `0x4001_c000_0000_0000` | 1.75 * 4: fraction .11 |
//! | -1.0, -7.0, -0.0 | the sign bit (`0x8000_..`) over 1.0, 7.0, 0.0 | |
//! | 2^70 | `0x4045_0000_0000_0000` | exponent 16383 + 70 = 16453 |
//! | 2^127 | `0x407e_0000_0000_0000` | exponent 16510; `i128::MAX` (127 bits) rounds to it: the spacing there is 2^14 |
//! | quiet NaN | `0x7fff_8000_0000_0000` | exponent all ones, fraction's top bit |
//!
//! The ABI (AAPCS64 C.1): a quad goes in the next `v` register and returns in `v0`; a quad as
//! the third argument is in `v2`. THE CONTROL (rule 23): the same functions with no routine
//! bound are refused naming the routine; `frem` with every routine bound but `fmodf128` is
//! refused naming `fmodf128` (the refusal it had before was legalize's own "not bound" text).
//! The floor (rule 67): a soft-float operation is one call; the test prints the add's bytes.

#![cfg(target_arch = "aarch64")]

use blaze_lir::legalize::{LegalizeError, SoftFloat};
use blaze_lir::{Flags, FloatCond, Op, Ty};

use crate::routines::RoutinePlaces;
use crate::tests::{ISA, emitted, one, run, sum_func};
use crate::{Code, EmitError, Via, emit_with_routines};
use blaze_inproc::exec::Executable;

unsafe extern "C" {
    fn __addtf3(a: f128, b: f128) -> f128;
    fn __subtf3(a: f128, b: f128) -> f128;
    fn __multf3(a: f128, b: f128) -> f128;
    fn __divtf3(a: f128, b: f128) -> f128;
    fn fmodf128(a: f128, b: f128) -> f128;
    fn __extenddftf2(a: f64) -> f128;
    fn __trunctfdf2(a: f128) -> f64;
    fn __fixtfdi(a: f128) -> i64;
    fn __fixtfti(a: f128) -> i128;
    fn __fixunstfdi(a: f128) -> u64;
    fn __fixunstfti(a: f128) -> u128;
    fn __floatditf(a: i64) -> f128;
    fn __floattitf(a: i128) -> f128;
    fn __floatunditf(a: u64) -> f128;
    fn __floatuntitf(a: u128) -> f128;
    fn __eqtf2(a: f128, b: f128) -> i32;
    fn __lttf2(a: f128, b: f128) -> i32;
    fn __letf2(a: f128, b: f128) -> i32;
    fn __gttf2(a: f128, b: f128) -> i32;
    fn __getf2(a: f128, b: f128) -> i32;
    fn __unordtf2(a: f128, b: f128) -> i32;
}

const COUNT: usize = SoftFloat::ALL.len();

/// The routine's address in this binary, for the routines these tests reach (the `f16` ones
/// and `__extendsftf2`/`__trunctfsf2` are not declared: no case here needs them, and a
/// function that did would be refused naming it, never run unbound).
fn address(r: SoftFloat) -> Option<u64> {
    let at = match r {
        SoftFloat::AddTf3 => __addtf3 as usize,
        SoftFloat::SubTf3 => __subtf3 as usize,
        SoftFloat::MulTf3 => __multf3 as usize,
        SoftFloat::DivTf3 => __divtf3 as usize,
        SoftFloat::FmodF128 => fmodf128 as usize,
        SoftFloat::ExtendDfTf2 => __extenddftf2 as usize,
        SoftFloat::TruncTfDf2 => __trunctfdf2 as usize,
        SoftFloat::FixTfDi => __fixtfdi as usize,
        SoftFloat::FixTfTi => __fixtfti as usize,
        SoftFloat::FixUnsTfDi => __fixunstfdi as usize,
        SoftFloat::FixUnsTfTi => __fixunstfti as usize,
        SoftFloat::FloatDiTf => __floatditf as usize,
        SoftFloat::FloatTiTf => __floattitf as usize,
        SoftFloat::FloatUnDiTf => __floatunditf as usize,
        SoftFloat::FloatUnTiTf => __floatuntitf as usize,
        SoftFloat::EqTf2 => __eqtf2 as usize,
        SoftFloat::LtTf2 => __lttf2 as usize,
        SoftFloat::LeTf2 => __letf2 as usize,
        SoftFloat::GtTf2 => __gttf2 as usize,
        SoftFloat::GeTf2 => __getf2 as usize,
        SoftFloat::UnordTf2 => __unordtf2 as usize,
        SoftFloat::ExtendHfTf2 | SoftFloat::TruncTfHf2 | SoftFloat::ExtendSfTf2 | SoftFloat::TruncTfSf2 => return None,
    };
    Some(at as u64)
}

/// Every routine [`address`] gives, bound at the place `r as u32` (the caller's reference
/// places, as front2/builtins_shim.rs `Bound::bind` appends them), but `without`.
fn places(without: Option<SoftFloat>) -> RoutinePlaces {
    let mut places = RoutinePlaces::NONE;
    for r in SoftFloat::ALL {
        if Some(r) != without && address(r).is_some() {
            places.bind(r, r as u32);
        }
    }
    places
}

/// `func` legalized with `places` and emitted (literal-word form, every place called
/// [`Via::Address`]), in the allocator of a code the shared helper made.
fn emit_bound<A: core::alloc::Allocator + Clone>(func: blaze_lir::View<'_>, places: &RoutinePlaces, like: &Code<A>) -> (Result<(), EmitError>, Code<A>) {
    let mut out = Code::new_in(like.bytes.allocator().clone());
    let got = emit_with_routines(blaze_lir::verify(func).expect("the test's function is LIR"), &[Via::Address; COUNT], places, ISA, &mut out).map(|_| ());
    (got, out)
}

/// `func` emitted with every routine bound and placed, each literal word its routine's
/// address; with the code's length in bytes. The allocator is the shared helper's (tests.rs),
/// taken from a code it made, so no line here names one.
fn placed(func: &blaze_lir::Func<impl core::alloc::Allocator>) -> (Executable, usize) {
    let base = emitted(sum_func().view(), &[]).expect("the shared sum emits");
    let (got, code) = emit_bound(func.view(), &places(None), &base);
    got.unwrap_or_else(|why| panic!("emits with the routines bound: {why}"));
    let exec = run::place(&code, |fixup| {
        let r = SoftFloat::ALL
            .into_iter()
            .find(|&r| r as u32 == fixup.place)
            .unwrap_or_else(|| panic!("a literal word of place {}, which names no routine", fixup.place));
        address(r).unwrap_or_else(|| panic!("a call of {} (not declared here)", r.symbol()))
    });
    (exec, code.bytes.len())
}

const ONE: u128 = 0x3fff << 112;
const TWO: u128 = 0x4000 << 112;
const THREE: u128 = 0x4000_8 << 108;
const ONE_HALF: u128 = 0x3fff_8 << 108;
const QUARTER: u128 = 0x3ffd << 112;
const FOUR: u128 = 0x4001 << 112;
const SEVEN: u128 = 0x4001_c << 108;
const SIGN: u128 = 1 << 127;
const NAN: u128 = 0x7fff_8 << 108;
const P70: u128 = 0x4045 << 112;
const P127: u128 = 0x407e << 112;

fn q(bits: u128) -> f128 {
    f128::from_bits(bits)
}

/// `a op b` of two quads, for `fadd`, `fsub`, `fmul`, `fdiv` and `frem`.
///
/// Prints: `f128 add: N bytes, 1 call` then
/// `test quad_run_tests::f128_arithmetic_runs_through_the_routines ... ok`
#[test]
fn f128_arithmetic_runs_through_the_routines() {
    let cases: [(Op, u128, u128, u128); 7] = [
        (Op::Fadd, ONE, TWO, THREE),
        (Op::Fsub, THREE, ONE, TWO),
        (Op::Fmul, ONE_HALF, TWO, THREE),
        (Op::Fdiv, ONE, FOUR, QUARTER),
        (Op::Frem, SEVEN, TWO, ONE),
        (Op::Frem, SIGN | SEVEN, TWO, SIGN | ONE),
        (Op::Fadd, SIGN, 0, 0),
    ];
    for (op, a, b, want) in cases {
        let func = one(&[Ty::F128, Ty::F128], Ty::F128, |bd, v| bd.binary(op, Flags::NONE, v[0], v[1]));
        let (exec, bytes) = placed(&func);
        if op == Op::Fadd && a == ONE {
            println!("f128 add: {bytes} bytes, 1 call");
        }
        // SAFETY: an emitted C function of two quads (q0, q1) returning one (q0), its literal
        // words this binary's routines; mapped while `exec` lives.
        let f: extern "C" fn(f128, f128) -> f128 = unsafe { exec.as_fn() };
        let got = f(q(a), q(b)).to_bits();
        assert_eq!(got, want, "{op:?} {a:#034x} {b:#034x}: {got:#034x}");
    }
}

/// Comparisons, with NaN and with -0.0 (IEEE 754 5.11: -0 == +0; NaN is unordered with
/// everything, itself too).
///
/// Prints: `test quad_run_tests::f128_comparisons_with_nan_and_negative_zero ... ok`
#[test]
fn f128_comparisons_with_nan_and_negative_zero() {
    let cases: [(FloatCond, u128, u128, u64); 11] = [
        (FloatCond::Oeq, SIGN, 0, 1),
        (FloatCond::Oge, SIGN, 0, 1),
        (FloatCond::Olt, SIGN, 0, 0),
        (FloatCond::Oeq, NAN, NAN, 0),
        (FloatCond::Une, NAN, NAN, 1),
        (FloatCond::Uno, NAN, ONE, 1),
        (FloatCond::Ord, ONE, TWO, 1),
        (FloatCond::Olt, ONE, TWO, 1),
        (FloatCond::Olt, TWO, ONE, 0),
        (FloatCond::Ogt, NAN, ONE, 0),
        (FloatCond::Ult, NAN, ONE, 1),
    ];
    for (cond, a, b, want) in cases {
        let func = one(&[Ty::F128, Ty::F128], Ty::I1, |bd, v| bd.fcmp(cond, v[0], v[1]));
        let (exec, _) = placed(&func);
        // SAFETY: an emitted C function of two quads (q0, q1) returning an `i1` in x0,
        // zero-extended (lib.rs, "Values").
        let f: extern "C" fn(f128, f128) -> u64 = unsafe { exec.as_fn() };
        assert_eq!(f(q(a), q(b)), want, "{cond:?} {a:#034x} {b:#034x}");
    }
}

/// Conversions: 2^70 and `i128::MAX` from `i128`, -1 from `i64`, back to `i128` (2^127
/// saturates to `i128::MAX`, as Rust's `as` does), to `i64` and `u64` (-1.0 to `u64` is 0),
/// NaN to 0, and `f64` 1.5 widened and 3.0 narrowed.
///
/// Prints: `test quad_run_tests::f128_conversions_round_trip_exact_values ... ok`
#[test]
fn f128_conversions_round_trip_exact_values() {
    let from_i128 = one(&[Ty::I128], Ty::F128, |bd, v| bd.convert(Op::SitoFp, Flags::NONE, Ty::F128, v[0]));
    let (exec, _) = placed(&from_i128);
    // SAFETY: an emitted C function of an `i128` (x0:x1) returning a quad (q0).
    let f: extern "C" fn(i128) -> f128 = unsafe { exec.as_fn() };
    assert_eq!(f(1 << 70).to_bits(), P70, "2^70 as f128");
    assert_eq!(f(i128::MAX).to_bits(), P127, "i128::MAX as f128 rounds to 2^127");
    assert_eq!(f(-1).to_bits(), SIGN | ONE, "-1i128 as f128");

    let from_i64 = one(&[Ty::I64], Ty::F128, |bd, v| bd.convert(Op::SitoFp, Flags::NONE, Ty::F128, v[0]));
    let (exec, _) = placed(&from_i64);
    // SAFETY: an emitted C function of an `i64` (x0) returning a quad (q0).
    let f: extern "C" fn(i64) -> f128 = unsafe { exec.as_fn() };
    assert_eq!(f(-1).to_bits(), SIGN | ONE, "-1i64 as f128");

    let to_i128 = one(&[Ty::F128], Ty::I128, |bd, v| bd.convert(Op::FptoSi, Flags::NONE, Ty::I128, v[0]));
    let (exec, _) = placed(&to_i128);
    // SAFETY: an emitted C function of a quad (q0) returning an `i128` (x0:x1).
    let f: extern "C" fn(f128) -> i128 = unsafe { exec.as_fn() };
    assert_eq!(f(q(P70)), 1 << 70, "2^70 as i128");
    assert_eq!(f(q(P127)), i128::MAX, "2^127 as i128 saturates");
    assert_eq!(f(q(SIGN | P127)), i128::MIN, "-2^127 as i128 is MIN exactly");
    assert_eq!(f(q(NAN)), 0, "NaN as i128");

    let to_i64 = one(&[Ty::F128], Ty::I64, |bd, v| bd.convert(Op::FptoSi, Flags::NONE, Ty::I64, v[0]));
    let (exec, _) = placed(&to_i64);
    // SAFETY: an emitted C function of a quad (q0) returning an `i64` (x0).
    let f: extern "C" fn(f128) -> i64 = unsafe { exec.as_fn() };
    assert_eq!(f(q(SIGN | ONE)), -1, "-1.0 as i64");
    assert_eq!(f(q(P70)), i64::MAX, "2^70 as i64 saturates");

    let to_u64 = one(&[Ty::F128], Ty::I64, |bd, v| bd.convert(Op::FptoUi, Flags::NONE, Ty::I64, v[0]));
    let (exec, _) = placed(&to_u64);
    // SAFETY: as above.
    let f: extern "C" fn(f128) -> u64 = unsafe { exec.as_fn() };
    assert_eq!(f(q(SIGN | ONE)), 0, "-1.0 as u64 saturates to 0");
    assert_eq!(f(q(THREE)), 3, "3.0 as u64");

    let widen = one(&[Ty::F64], Ty::F128, |bd, v| bd.convert(Op::Fpext, Flags::NONE, Ty::F128, v[0]));
    let (exec, _) = placed(&widen);
    // SAFETY: an emitted C function of an `f64` (d0) returning a quad (q0).
    let f: extern "C" fn(f64) -> f128 = unsafe { exec.as_fn() };
    assert_eq!(f(1.5).to_bits(), ONE_HALF, "1.5f64 as f128");

    let narrow = one(&[Ty::F128], Ty::F64, |bd, v| bd.convert(Op::Fptrunc, Flags::NONE, Ty::F64, v[0]));
    let (exec, _) = placed(&narrow);
    // SAFETY: an emitted C function of a quad (q0) returning an `f64` (d0).
    let f: extern "C" fn(f128) -> f64 = unsafe { exec.as_fn() };
    assert_eq!(f(q(THREE)).to_bits(), 3.0f64.to_bits(), "3.0 as f64");
}

/// The ABI: a quad as the third argument (after a quad and an `f64`: `v0`, `v1`, `v2`) is
/// returned as it came, and added to the first.
///
/// Prints: `test quad_run_tests::a_quad_third_argument_is_passed_and_returned ... ok`
#[test]
fn a_quad_third_argument_is_passed_and_returned() {
    let same = one(&[Ty::F128, Ty::F64, Ty::F128], Ty::F128, |_, v| v[2]);
    let (exec, _) = placed(&same);
    // SAFETY: an emitted C function of a quad, an `f64` and a quad (v0, v1, v2) returning a
    // quad (q0).
    let f: extern "C" fn(f128, f64, f128) -> f128 = unsafe { exec.as_fn() };
    assert_eq!(f(q(ONE), 9.0, q(SEVEN)).to_bits(), SEVEN, "the third argument returned");
    let sum = one(&[Ty::F128, Ty::F64, Ty::F128], Ty::F128, |bd, v| bd.binary(Op::Fadd, Flags::NONE, v[0], v[2]));
    let (exec, _) = placed(&sum);
    // SAFETY: as above.
    let f: extern "C" fn(f128, f64, f128) -> f128 = unsafe { exec.as_fn() };
    assert_eq!(f(q(ONE), 9.0, q(TWO)).to_bits(), THREE, "first + third");
}

/// THE CONTROL: with no routine bound an `f128` add is refused naming `__addtf3`; with every
/// routine bound but `fmodf128`, a `frem` is refused naming `fmodf128` (the cause removed, the
/// run gone).
///
/// Prints: `test quad_run_tests::unbound_routines_are_refused_by_name ... ok`
#[test]
fn unbound_routines_are_refused_by_name() {
    let base = emitted(sum_func().view(), &[]).expect("the shared sum emits");
    let add = one(&[Ty::F128, Ty::F128], Ty::F128, |bd, v| bd.binary(Op::Fadd, Flags::NONE, v[0], v[1]));
    let (got, _) = emit_bound(add.view(), &RoutinePlaces::NONE, &base);
    assert_eq!(got, Err(EmitError::Legalize(LegalizeError::NoRoutine(SoftFloat::AddTf3))), "an add with nothing bound");
    let rem = one(&[Ty::F128, Ty::F128], Ty::F128, |bd, v| bd.binary(Op::Frem, Flags::NONE, v[0], v[1]));
    let (got, _) = emit_bound(rem.view(), &places(Some(SoftFloat::FmodF128)), &base);
    assert_eq!(got, Err(EmitError::Legalize(LegalizeError::NoRoutine(SoftFloat::FmodF128))), "a frem without fmodf128");
    assert_eq!(SoftFloat::FmodF128.symbol(), "fmodf128");
}
