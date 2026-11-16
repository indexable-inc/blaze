//! Floats (pass/float.rs) and `i128` (pass/wide.rs) run natively, each result compared with
//! the same operation in Rust (the Reference's semantics, which the host's own code follows),
//! the arguments and returns crossing in the registers AAPCS64 names: a float in `v0..v7`, an
//! `i128` in an even pair of `x` registers. The test binary's `extern "C"` call is the witness.

#![cfg(target_arch = "aarch64")]

use blaze_lir::{FloatCond, Flags, IntCond, Op, Ty};

use crate::tests::{emitted, one, run};

fn place(func: &blaze_lir::Func<std::alloc::Global>) -> blaze_inproc::exec::Executable {
    let code = emitted(func.view(), &[]).unwrap_or_else(|why| panic!("{why}\n{}", blaze_lir::print(func.view())));
    run::place(&code, |_| unreachable!("no literal words"))
}

const SAMPLES: [f64; 12] = [0.0, -0.0, 1.5, -2.75, 1e300, -1e300, 3.0e9, -3.0e9, 255.9, -128.5, f64::INFINITY, f64::NAN];

/// `fadd`, `fsub`, `fmul`, `fdiv` and `fneg` of `f64` and `f32` parameters, returned in `v0`.
#[test]
fn float_arithmetic_crosses_in_vector_registers() {
    for (op, rust) in [
        (Op::Fadd, (|a, b| a + b) as fn(f64, f64) -> f64),
        (Op::Fsub, |a, b| a - b),
        (Op::Fmul, |a, b| a * b),
        (Op::Fdiv, |a, b| a / b),
    ] {
        let d = place(&one(&[Ty::F64, Ty::F64], Ty::F64, |b, v| b.binary(op, Flags::NONE, v[0], v[1])));
        let s = place(&one(&[Ty::F32, Ty::F32], Ty::F32, |b, v| b.binary(op, Flags::NONE, v[0], v[1])));
        // SAFETY: emitted C functions of these signatures, mapped while `d` and `s` live.
        let (fd, fs) = unsafe {
            (
                std::mem::transmute::<usize, extern "C" fn(f64, f64) -> f64>(d.code_address() as usize),
                std::mem::transmute::<usize, extern "C" fn(f32, f32) -> f32>(s.code_address() as usize),
            )
        };
        for a in SAMPLES {
            for b in SAMPLES {
                assert_eq!(fd(a, b).to_bits(), rust(a, b).to_bits(), "{op:?} {a} {b}");
                let (x, y) = (a as f32, b as f32);
                let want = rust(f64::from(x), f64::from(y)) as f32;
                assert_eq!(fs(x, y).to_bits(), want.to_bits(), "{op:?} f32 {x} {y}");
            }
        }
    }
    let neg = place(&one(&[Ty::F64], Ty::F64, |b, v| b.unary(Op::Fneg, Flags::NONE, v[0])));
    // SAFETY: as above.
    let neg = unsafe { std::mem::transmute::<usize, extern "C" fn(f64) -> f64>(neg.code_address() as usize) };
    assert_eq!(neg(1.5), -1.5);
}

/// The Reference, "Comparison operators" on floats: `==`, `<`, `<=`, `>`, `>=` are false and
/// `!=` true when either side is `NaN` (LLVM's `oeq`, `olt`, `ole`, `ogt`, `oge`, `une`).
#[test]
fn float_comparisons_follow_nan() {
    let conds: [(FloatCond, fn(f64, f64) -> bool); 6] = [
        (FloatCond::Oeq, |a, b| a == b),
        (FloatCond::Une, |a, b| a != b),
        (FloatCond::Olt, |a, b| a < b),
        (FloatCond::Ole, |a, b| a <= b),
        (FloatCond::Ogt, |a, b| a > b),
        (FloatCond::Oge, |a, b| a >= b),
    ];
    for (cond, rust) in conds {
        let f = place(&one(&[Ty::F64, Ty::F64], Ty::I8, |b, v| {
            let bit = b.fcmp(cond, v[0], v[1]);
            b.convert(Op::Zext, Flags::NONE, Ty::I8, bit)
        }));
        // SAFETY: an emitted C function of this signature, mapped while `f` lives.
        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(f64, f64) -> u8>(f.code_address() as usize) };
        for a in SAMPLES {
            for b in SAMPLES {
                assert_eq!(f(a, b) != 0, rust(a, b), "{cond:?} {a} {b}");
            }
        }
    }
}

/// The Reference, "Type cast expressions": float to integer rounds toward zero, `NaN` is 0,
/// out of range saturates; integer to float is the closest float; `f64` to `f32` the closest.
#[test]
fn float_casts_saturate_and_round() {
    macro_rules! to_int {
        ($t:ty, $ty:expr, $op:expr) => {{
            let f = place(&one(&[Ty::F64], $ty, |b, v| b.convert($op, Flags::NONE, $ty, v[0])));
            // SAFETY: an emitted C function of this signature, mapped while `f` lives.
            let g = unsafe { std::mem::transmute::<usize, extern "C" fn(f64) -> $t>(f.code_address() as usize) };
            for a in SAMPLES {
                assert_eq!(g(a), a as $t, "{a} as {}", stringify!($t));
            }
        }};
    }
    to_int!(i8, Ty::I8, Op::FptoSi);
    to_int!(u8, Ty::I8, Op::FptoUi);
    to_int!(i16, Ty::I16, Op::FptoSi);
    to_int!(u16, Ty::I16, Op::FptoUi);
    to_int!(i32, Ty::I32, Op::FptoSi);
    to_int!(u32, Ty::I32, Op::FptoUi);
    to_int!(i64, Ty::I64, Op::FptoSi);
    to_int!(u64, Ty::I64, Op::FptoUi);
    let from = place(&one(&[Ty::I64], Ty::F64, |b, v| b.convert(Op::SitoFp, Flags::NONE, Ty::F64, v[0])));
    let ufrom = place(&one(&[Ty::I32], Ty::F32, |b, v| b.convert(Op::UitoFp, Flags::NONE, Ty::F32, v[0])));
    let narrow = place(&one(&[Ty::F64], Ty::F32, |b, v| b.convert(Op::Fptrunc, Flags::NONE, Ty::F32, v[0])));
    // SAFETY: emitted C functions of these signatures, mapped while they live.
    let (from, ufrom, narrow) = unsafe {
        (
            std::mem::transmute::<usize, extern "C" fn(i64) -> f64>(from.code_address() as usize),
            std::mem::transmute::<usize, extern "C" fn(u32) -> f32>(ufrom.code_address() as usize),
            std::mem::transmute::<usize, extern "C" fn(f64) -> f32>(narrow.code_address() as usize),
        )
    };
    for x in [0i64, -1, i64::MAX, i64::MIN, (1 << 53) + 1, -(1 << 60) - 3] {
        assert_eq!(from(x).to_bits(), (x as f64).to_bits(), "{x} as f64");
    }
    for x in [0u32, u32::MAX, 16_777_217, 3_000_000_001] {
        assert_eq!(ufrom(x).to_bits(), (x as f32).to_bits(), "{x} as f32");
    }
    for a in SAMPLES {
        assert_eq!(narrow(a).to_bits(), (a as f32).to_bits(), "{a} as f32");
    }
}

const WIDE: [u128; 8] = [
    0,
    1,
    u64::MAX as u128,
    1 << 64,
    u128::MAX,
    1 << 127,
    0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
    0xffff_0000_ffff_0000_0000_ffff_0000_ffff,
];

/// `i128` add, sub, mul, the bitwise forms and the shifts, crossing as register pairs: the
/// middle `u64` parameter puts the second `i128` in `x4:x5` on Linux (AAPCS64 C.8 rounds to
/// even) and in `x3:x4` on macOS (Apple's variant: this test failed "Add 0x0 0x0 left:
/// 590295810358705651712" while the emitter rounded to even there).
#[test]
fn i128_arithmetic_crosses_in_even_register_pairs() {
    let ops: [(Op, fn(u128, u128) -> u128); 9] = [
        (Op::Add, u128::wrapping_add),
        (Op::Sub, u128::wrapping_sub),
        (Op::Mul, u128::wrapping_mul),
        (Op::And, |a, b| a & b),
        (Op::Or, |a, b| a | b),
        (Op::Xor, |a, b| a ^ b),
        (Op::Shl, |a, b| a << (b & 127)),
        (Op::Lshr, |a, b| a >> (b & 127)),
        (Op::Ashr, |a, b| ((a as i128) >> (b & 127)) as u128),
    ];
    for (op, rust) in ops {
        let f = place(&one(&[Ty::I128, Ty::I64, Ty::I128], Ty::I128, |b, v| b.binary(op, Flags::NONE, v[0], v[2])));
        // SAFETY: an emitted C function of this signature, mapped while `f` lives.
        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u128, u64, u128) -> u128>(f.code_address() as usize) };
        for a in WIDE {
            for b in WIDE.iter().copied().chain([3, 63, 64, 65, 127]) {
                assert_eq!(f(a, 7, b), rust(a, b), "{op:?} {a:#x} {b:#x}");
            }
        }
    }
}

/// `i128` comparisons and the casts to and from 64 bits.
#[test]
fn i128_compares_and_casts() {
    let conds: [(IntCond, fn(u128, u128) -> bool); 6] = [
        (IntCond::Eq, |a, b| a == b),
        (IntCond::Ult, |a, b| a < b),
        (IntCond::Ugt, |a, b| a > b),
        (IntCond::Sle, |a, b| (a as i128) <= (b as i128)),
        (IntCond::Sge, |a, b| (a as i128) >= (b as i128)),
        (IntCond::Slt, |a, b| (a as i128) < (b as i128)),
    ];
    for (cond, rust) in conds {
        let f = place(&one(&[Ty::I128, Ty::I128], Ty::I8, |b, v| {
            let bit = b.icmp(cond, v[0], v[1]);
            b.convert(Op::Zext, Flags::NONE, Ty::I8, bit)
        }));
        // SAFETY: an emitted C function of this signature, mapped while `f` lives.
        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u128, u128) -> u8>(f.code_address() as usize) };
        for a in WIDE {
            for b in WIDE {
                assert_eq!(f(a, b) != 0, rust(a, b), "{cond:?} {a:#x} {b:#x}");
            }
        }
    }
    let sext = place(&one(&[Ty::I32], Ty::I128, |b, v| b.convert(Op::Sext, Flags::NONE, Ty::I128, v[0])));
    let trunc = place(&one(&[Ty::I128], Ty::I64, |b, v| b.convert(Op::Trunc, Flags::NONE, Ty::I64, v[0])));
    let big = place(&one(&[], Ty::I128, |b, _| b.iconst(Ty::I128, 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210)));
    // SAFETY: emitted C functions of these signatures, mapped while they live.
    let (sext, trunc, big) = unsafe {
        (
            std::mem::transmute::<usize, extern "C" fn(i32) -> i128>(sext.code_address() as usize),
            std::mem::transmute::<usize, extern "C" fn(u128) -> u64>(trunc.code_address() as usize),
            std::mem::transmute::<usize, extern "C" fn() -> u128>(big.code_address() as usize),
        )
    };
    assert_eq!(sext(-5), -5i128);
    assert_eq!(trunc(WIDE[6]), WIDE[6] as u64);
    assert_eq!(big(), WIDE[6]);
}
