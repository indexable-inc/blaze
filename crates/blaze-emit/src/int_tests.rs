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

/// An overflow operation whose cell is past `sp + 32 KiB` (the 2048th value on, where `ld_sp`
/// and `st_sp` address through pass.rs `FAR`) keeps its flag. Before `FAR` the far address was
/// made in `x12`, the register `overflow` holds its flag in, so storing the result wrote the
/// cell's address over the flag: every checked `+ - *` past value 2048 read as overflowed
/// (tempo's 61 failing tests, 2026-11-21: serde_json's `remaining_depth -= 1`, sha2's
/// `K64[t + 6]`, `len - 128` after a `len >= 128` guard). Padded by unused constants to put the
/// operation on each side of the boundary; each case against Rust's own `overflowing_*`. The
/// control (rule 23): with `FAR` set back to `X12` the non-overflowing cases at 2048 and on fail
/// with flag 1 and those below it pass.
#[test]
fn far_cells_keep_overflow_flags() {
    use crate::tests::{emitted, run};

    // (op, ty, a, b, wrapped result, overflowed): non-overflowing cases first, by Rust's ops.
    let cases: [(Op, Ty, u64, u64, (u64, bool)); 12] = [
        (Op::UsubOverflow, Ty::I64, 200, 128, (72, false)),
        (Op::UsubOverflow, Ty::I8, 128, 1, (127, false)),
        (Op::UaddOverflow, Ty::I64, 16, 6, (22, false)),
        (Op::SsubOverflow, Ty::I32, (-5i32) as u32 as u64, 3, ((-8i32) as u32 as u64, false)),
        (Op::SsubOverflow, Ty::I64, 0, 1, (u64::MAX, false)),
        (Op::SaddOverflow, Ty::I8, 0x7e, 1, (0x7f, false)),
        (Op::UmulOverflow, Ty::I16, 255, 257, (65535, false)),
        (Op::SmulOverflow, Ty::I64, u64::MAX, 3, ((-3i64) as u64, false)),
        // The overflowing twins.
        (Op::UsubOverflow, Ty::I64, 127, 128, (u64::MAX, true)),
        (Op::UaddOverflow, Ty::I8, 255, 1, (0, true)),
        (Op::SsubOverflow, Ty::I64, i64::MIN as u64, 1, (i64::MAX as u64, true)),
        (Op::SmulOverflow, Ty::I32, 0x4000_0000, 2, (0x8000_0000, true)),
    ];
    let wants: [(u64, bool); 12] = [
        200u64.overflowing_sub(128),
        (u64::from(128u8.overflowing_sub(1).0), 128u8.overflowing_sub(1).1),
        16u64.overflowing_add(6),
        ((-5i32).overflowing_sub(3).0 as u32 as u64, (-5i32).overflowing_sub(3).1),
        (0i64.overflowing_sub(1).0 as u64, 0i64.overflowing_sub(1).1),
        (0x7ei8.overflowing_add(1).0 as u8 as u64, 0x7ei8.overflowing_add(1).1),
        (u64::from(255u16.overflowing_mul(257).0), 255u16.overflowing_mul(257).1),
        ((-1i64).overflowing_mul(3).0 as u64, (-1i64).overflowing_mul(3).1),
        127u64.overflowing_sub(128),
        (u64::from(255u8.overflowing_add(1).0), 255u8.overflowing_add(1).1),
        (i64::MIN.overflowing_sub(1).0 as u64, i64::MIN.overflowing_sub(1).1),
        (0x4000_0000i32.overflowing_mul(2).0 as u32 as u64, 0x4000_0000i32.overflowing_mul(2).1),
    ];
    for (&(op, ty, a, b, expect), want) in cases.iter().zip(wants) {
        assert_eq!(expect, want, "{op:?} {ty:?} ({a:#x}, {b:#x}): the case's own expectation is Rust's");
    }
    for pad in [2040u32, 2045, 2046, 2047, 2048, 2100, 4200] {
        for &(op, ty, a, b, (result, flag)) in &cases {
            for (k, ret, want) in [(0, ty, result), (1, Ty::I1, u64::from(flag))] {
                let func = one(&[ty, ty], ret, |bb, v| {
                    for n in 0..pad {
                        let _ = bb.iconst(Ty::I64, u128::from(n));
                    }
                    let pair = bb.overflow(op, v[0], v[1]);
                    bb.project(pair, k)
                });
                let code = emitted(func.view(), &[]).expect("emits");
                let exec = run::place(&code, |_| unreachable!());
                let got = run::call(&exec, [a, b, 0, 0]) & crate::tests::mask(ret.bits(64));
                assert_eq!(got, want, "{op:?} {ty:?} member {k} of ({a:#x}, {b:#x}) after {pad} values");
            }
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
