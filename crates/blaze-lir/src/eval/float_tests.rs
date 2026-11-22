//! The evaluator's soft IEEE floats ([`super::float`]) against what Rust's floats are.
//!
//! The rule (the pinned library, core/src/primitive_docs.rs, `f32`): "When a primitive
//! operation (addition, subtraction, multiplication, or division) is performed on this type,
//! the result is rounded according to the roundTiesToEven direction defined in IEEE 754-2008.
//! That means: The result is the representable value closest to the true value, if there is a
//! unique closest representable value. If the true value is exactly half-way between two
//! representable values, the result is the one with an even least-significant binary digit.
//! ... If the result of a sum exactly equals zero, the outcome is +0.0 unless both arguments
//! were negative, then it is -0.0." `%` is C's `fmod`, exact. Of a NaN result the same file
//! says "The result has a non-deterministic sign" and its quiet bit and payload are chosen
//! among several options: so a NaN is compared by class, every other result by its bits.
//!
//! The host's `f32` and `f64` operations are that rule (Rust defines them so on every host),
//! which makes them the reference for binary32 and binary64. For binary16 the reference is
//! the binary32 result rounded once to binary16: exact for `+ - * /`, because binary32 carries
//! 24 >= 2 * 11 + 2 significand bits (Figueroa, "When is double rounding innocuous?", 1995:
//! with p' >= 2p + 2 the doubly rounded result equals the singly rounded one), so the test
//! needs no other implementation of binary16 arithmetic. Its two conversions are written here
//! independently of the code under test.

use super::float::float_bin;
use crate::{Op, Ty};

/// A seeded splitmix64: the same pairs on every run and machine.
struct Seq(u64);

impl Seq {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

const OPS: [Op; 5] = [Op::Fadd, Op::Fsub, Op::Fmul, Op::Fdiv, Op::Frem];

/// Values random bits rarely reach: zeros, ones, the extremes, infinities, NaNs.
const F32_EDGES: [u32; 12] = [
    0x0000_0000, 0x8000_0000, 0x3F80_0000, 0xBF80_0000, 0x0000_0001, 0x8000_0001, 0x007F_FFFF,
    0x0080_0000, 0x7F7F_FFFF, 0x7F80_0000, 0xFF80_0000, 0x7FC0_0000,
];
const F64_EDGES: [u64; 12] = [
    0, 1 << 63, 0x3FF0_0000_0000_0000, 0xBFF0_0000_0000_0000, 1, (1 << 63) | 1, 0x000F_FFFF_FFFF_FFFF,
    0x0010_0000_0000_0000, 0x7FEF_FFFF_FFFF_FFFF, 0x7FF0_0000_0000_0000, 0xFFF0_0000_0000_0000,
    0x7FF8_0000_0000_0000,
];

fn f32_host(op: Op, a: f32, b: f32) -> f32 {
    match op {
        Op::Fadd => a + b,
        Op::Fsub => a - b,
        Op::Fmul => a * b,
        Op::Fdiv => a / b,
        _ => a % b,
    }
}

fn f64_host(op: Op, a: f64, b: f64) -> f64 {
    match op {
        Op::Fadd => a + b,
        Op::Fsub => a - b,
        Op::Fmul => a * b,
        Op::Fdiv => a / b,
        _ => a % b,
    }
}

/// One in eight draws is an edge value; the rest are random bits, with half of those given a
/// nearby exponent to the other operand's so sums cancel and quotients round.
fn f32_pair(seq: &mut Seq) -> (u32, u32) {
    let pick = |seq: &mut Seq| {
        let r = seq.next();
        match r % 8 {
            0 => F32_EDGES[(r >> 8) as usize % F32_EDGES.len()],
            _ => (r >> 32) as u32,
        }
    };
    let a = pick(seq);
    let b = match seq.next() % 2 {
        0 => pick(seq),
        _ => (a & 0xFF80_0000) ^ ((seq.next() as u32) & 0x00FF_FFFF),
    };
    (a, b)
}

fn f64_pair(seq: &mut Seq) -> (u64, u64) {
    let pick = |seq: &mut Seq| {
        let r = seq.next();
        match r % 8 {
            0 => F64_EDGES[(r >> 8) as usize % F64_EDGES.len()],
            _ => seq.next(),
        }
    };
    let a = pick(seq);
    let b = match seq.next() % 2 {
        0 => pick(seq),
        _ => (a & 0xFFF0_0000_0000_0000) ^ (seq.next() & 0x001F_FFFF_FFFF_FFFF),
    };
    (a, b)
}

/// 10,000 seeded pairs of `f32` and of `f64`, each through `fadd`, `fsub`, `fmul`, `fdiv` and
/// `frem`: the soft result is the host's, bit for bit, or both are NaN. A rounding off by one
/// ulp, a wrong sign of zero, an infinity for a finite result or a lost subnormal fails it.
///
/// Expected line: `test eval::float_tests::f32_matches_host ... ok`.
#[test]
fn f32_matches_host() {
    let mut seq = Seq(0x5EED_F32F_64AB_C001);
    for _ in 0..10_000 {
        let (a, b) = f32_pair(&mut seq);
        for op in OPS {
            let host = f32_host(op, f32::from_bits(a), f32::from_bits(b));
            let soft = float_bin(op, Ty::F32, u128::from(a), u128::from(b)).expect("a modeled f32 operation");
            let soft = f32::from_bits(soft as u32);
            assert!(
                (host.is_nan() && soft.is_nan()) || host.to_bits() == soft.to_bits(),
                "f32 {}: {a:#010x}, {b:#010x}: host {:#010x}, soft {:#010x}",
                op.name(),
                host.to_bits(),
                soft.to_bits(),
            );
        }
        let (a, b) = f64_pair(&mut seq);
        for op in OPS {
            let host = f64_host(op, f64::from_bits(a), f64::from_bits(b));
            let soft = float_bin(op, Ty::F64, u128::from(a), u128::from(b)).expect("a modeled f64 operation");
            let soft = f64::from_bits(soft as u64);
            assert!(
                (host.is_nan() && soft.is_nan()) || host.to_bits() == soft.to_bits(),
                "f64 {}: {a:#018x}, {b:#018x}: host {:#018x}, soft {:#018x}",
                op.name(),
                host.to_bits(),
                soft.to_bits(),
            );
        }
    }
}

/// binary16 to binary32, exact: every binary16 value is a binary32 value.
fn widen(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let (e, f) = (u32::from((h >> 10) & 0x1F), u32::from(h & 0x3FF));
    match (e, f) {
        (0x1F, 0) => f32::from_bits(sign | 0x7F80_0000),
        (0x1F, _) => f32::NAN,
        // A subnormal: `f * 2^-24`, both factors exact in binary32 (0x3380_0000 is 2^-24).
        (0, _) => f32::from_bits(sign | (f32::from_bits(0x3380_0000) * f as f32).to_bits()),
        _ => f32::from_bits(sign | ((e + 127 - 15) << 23) | (f << 13)),
    }
}

/// binary32 to binary16, rounded to nearest, ties to even.
fn narrow(x: f32) -> u16 {
    let sign = ((x.to_bits() >> 16) & 0x8000) as u16;
    if x.is_nan() {
        return 0x7E00;
    }
    let a = x.abs();
    // binary16's largest finite is 65504; "If the true value's magnitude is >= MAX +
    // 2^(MAX_EXP - MANTISSA_DIGITS - 1)" (65504 + 2^4) "the result is inf".
    if a >= 65520.0 {
        return sign | 0x7C00;
    }
    if a < f32::from_bits(0x3880_0000) {
        // Below 2^-14, binary16's subnormals: count units of 2^-24, scaled exactly.
        let units = (a * f32::from_bits(0x4B80_0000)).round_ties_even();
        return sign | units as u16;
    }
    let bits = a.to_bits();
    let e = (bits >> 23) as i32 - 127;
    let m = bits & 0x7F_FFFF;
    let (mut r, rest) = (m >> 13, m & 0x1FFF);
    if rest > 0x1000 || (rest == 0x1000 && r & 1 == 1) {
        r += 1;
    }
    // A carry out of the fraction moves into the exponent field, as the encoding allows.
    sign | ((((e + 15) as u32) << 10) + r) as u16
}

/// Every binary16 `a` (all 2^16) with 64 seeded `b`, through `fadd`, `fsub`, `fmul` and
/// `fdiv`: the soft binary16 result is the binary32 result of the widened operands rounded
/// once to binary16, bit for bit, or both are NaN.
///
/// Expected line: `test eval::float_tests::f16_via_f32_exact ... ok`.
#[test]
fn f16_via_f32_exact() {
    let mut seq = Seq(0x00F1_6F16_0000_0016);
    let bs: [u16; 64] = core::array::from_fn(|_| seq.next() as u16);
    for a in 0..=u16::MAX {
        for &b in &bs {
            for op in [Op::Fadd, Op::Fsub, Op::Fmul, Op::Fdiv] {
                let want = narrow(f32_host(op, widen(a), widen(b)));
                let got = float_bin(op, Ty::F16, u128::from(a), u128::from(b)).expect("a modeled f16 operation") as u16;
                let nan = |h: u16| h & 0x7C00 == 0x7C00 && h & 0x3FF != 0;
                assert!(
                    (nan(want) && nan(got)) || want == got,
                    "f16 {}: {a:#06x}, {b:#06x}: want {want:#06x}, got {got:#06x}",
                    op.name(),
                );
            }
        }
    }
}
