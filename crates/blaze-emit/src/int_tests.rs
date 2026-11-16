//! Rotations, high multiplies, min/max, saturating arithmetic, the overflow operations (a pair
//! read back with `project`) and the bit operations: emitted, run on this Mac and compared with
//! the reference evaluator on every pair of a width's edge values.

#![cfg(target_arch = "aarch64")]

use blaze_lir::{Flags, Op, Ty};

use crate::tests::{WIDTHS, agrees, edges, one};

/// Every pair of `ty`'s edge values.
fn pairs(ty: Ty) -> Vec<[u128; 2]> {
    let values = edges(ty.bits(64));
    values.iter().flat_map(|&a| values.iter().map(move |&b| [a, b])).collect()
}

#[test]
fn rotations_min_max_and_saturation_agree_with_the_evaluator() {
    let ops = [
        Op::Rotl,
        Op::Rotr,
        Op::Umulhi,
        Op::Smulhi,
        Op::Umin,
        Op::Umax,
        Op::Smin,
        Op::Smax,
        Op::UaddSat,
        Op::UsubSat,
        Op::SaddSat,
        Op::SsubSat,
    ];
    for ty in WIDTHS {
        for op in ops {
            let func = one(&[ty, ty], ty, |b, v| b.binary(op, Flags::NONE, v[0], v[1]));
            for args in pairs(ty) {
                agrees(&func, &args);
            }
        }
    }
}

/// Both members of every overflow operation's pair, at 8, 16, 32 and 64 bits.
#[test]
fn overflow_pairs_agree_with_the_evaluator() {
    let ops = [
        Op::UaddOverflow,
        Op::SaddOverflow,
        Op::UsubOverflow,
        Op::SsubOverflow,
        Op::UmulOverflow,
        Op::SmulOverflow,
    ];
    for ty in [Ty::I8, Ty::I16, Ty::I32, Ty::I64] {
        for op in ops {
            for (k, ret) in [(0, ty), (1, Ty::I1)] {
                let func = one(&[ty, ty], ret, |b, v| {
                    let pair = b.overflow(op, v[0], v[1]);
                    b.project(pair, k)
                });
                for args in pairs(ty) {
                    agrees(&func, &args);
                }
            }
        }
    }
}

/// Spot values against Rust's own `overflowing_*`, so the evaluator is not the only witness.
#[test]
fn overflow_flags_match_rusts_overflowing_ops() {
    use crate::tests::{emitted, run};

    let cases: [(Op, Ty, u64, u64, u64, bool); 8] = [
        (Op::UaddOverflow, Ty::I64, u64::MAX, 1, 0, true),
        (Op::SaddOverflow, Ty::I64, i64::MAX as u64, 1, i64::MIN as u64, true),
        (Op::UsubOverflow, Ty::I32, 0, 1, u32::MAX.into(), true),
        (Op::SsubOverflow, Ty::I8, 0x80, 1, 0x7f, true),
        (Op::UmulOverflow, Ty::I64, 1 << 32, 1 << 32, 0, true),
        (Op::SmulOverflow, Ty::I64, i64::MIN as u64, u64::MAX, i64::MIN as u64, true),
        (Op::SmulOverflow, Ty::I16, 0xffff, 0xffff, 1, false),
        (Op::UmulOverflow, Ty::I32, 0xffff, 0x10001, 0xffff_ffff, false),
    ];
    let wants = [
        (u64::MAX.overflowing_add(1).0, u64::MAX.overflowing_add(1).1),
        ((i64::MAX.overflowing_add(1).0) as u64, i64::MAX.overflowing_add(1).1),
        (u64::from(0u32.overflowing_sub(1).0), 0u32.overflowing_sub(1).1),
        ((i8::MIN.overflowing_sub(1).0) as u8 as u64, i8::MIN.overflowing_sub(1).1),
        ((1u64 << 32).overflowing_mul(1 << 32).0, (1u64 << 32).overflowing_mul(1 << 32).1),
        ((i64::MIN.overflowing_mul(-1).0) as u64, i64::MIN.overflowing_mul(-1).1),
        (((-1i16).overflowing_mul(-1).0) as u16 as u64, (-1i16).overflowing_mul(-1).1),
        (u64::from(0xffffu32.overflowing_mul(0x10001).0), 0xffffu32.overflowing_mul(0x10001).1),
    ];
    for ((op, ty, a, b, result, flag), want) in cases.into_iter().zip(wants) {
        assert_eq!((result, flag), want, "{op:?} {ty:?}: the case's own expectation is Rust's");
        for (k, ret, expect) in [(0, ty, result), (1, Ty::I1, u64::from(flag))] {
            let func = one(&[ty, ty], ret, |bb, v| {
                let pair = bb.overflow(op, v[0], v[1]);
                bb.project(pair, k)
            });
            let code = emitted(func.view(), &[]).expect("emits");
            let exec = run::place(&code, |_| unreachable!());
            let got = run::call(&exec, [a, b, 0, 0]) & crate::tests::mask(ret.bits(64));
            assert_eq!(got, expect, "{op:?} {ty:?} member {k} of ({a:#x}, {b:#x})");
        }
    }
}

#[test]
fn bit_operations_agree_with_the_evaluator() {
    for ty in WIDTHS {
        for op in [Op::Clz, Op::Ctz, Op::Popcnt, Op::Bswap, Op::Bitrev] {
            let func = one(&[ty], ty, |b, v| b.unary(op, Flags::NONE, v[0]));
            for value in edges(ty.bits(64)) {
                agrees(&func, &[value]);
            }
        }
    }
}
