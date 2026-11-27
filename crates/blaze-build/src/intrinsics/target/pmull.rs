//! AArch64 NEON polynomial multiply-long intrinsics (`FEAT_PMULL` / NEON):
//! - `llvm.aarch64.neon.pmull64` (`vmull_p64`): `fn(p64, p64) -> int8x16_t` (64x64 -> 128-bit
//!   carry-less multiply over GF(2), stored little-endian as `[lo: u64, hi: u64]`).
//! - `llvm.aarch64.neon.pmull.v8i16` (`vmull_p8`): `fn(poly8x8_t, poly8x8_t) -> poly16x8_t`
//!   (8 lanes of 8x8 -> 16-bit carry-less multiply over GF(2)).
//!
//! Built as branchless scalar LIR operations from the Arm Architecture Reference Manual
//! `PolynomialMult` definition so every backend and the LIR reference evaluator compute exact bits.

use core::alloc::Allocator;

use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};

/// An AArch64 polynomial multiply-long intrinsic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PmullFn {
    /// `llvm.aarch64.neon.pmull64`: `(u64, u64) -> int8x16_t` (128-bit carry-less product).
    Pmull64,
    /// `llvm.aarch64.neon.pmull.v8i16`: `(poly8x8_t, poly8x8_t) -> poly16x8_t`.
    PmullV8I16,
}

impl PmullFn {
    /// The LLVM link name declared by stdarch.
    pub const fn link_name(self) -> &'static str {
        match self {
            Self::Pmull64 => "llvm.aarch64.neon.pmull64",
            Self::PmullV8I16 => "llvm.aarch64.neon.pmull.v8i16",
        }
    }
}

/// Parses an AArch64 polynomial multiply-long LLVM intrinsic link name.
pub fn named(link_name: &str) -> Option<PmullFn> {
    match link_name {
        "llvm.aarch64.neon.pmull64" => Some(PmullFn::Pmull64),
        "llvm.aarch64.neon.pmull.v8i16" => Some(PmullFn::PmullV8I16),
        _ => None,
    }
}

/// Builds 64x64 -> 128-bit carry-less multiply over GF(2), returning `(lo: i64, hi: i64)`.
pub fn build_pmull64<A: Allocator + Clone>(b: &mut Builder<A, Full>, lhs: Value, rhs: Value) -> (Value, Value) {
    let zero = b.iconst(Ty::I64, 0);
    let one = b.iconst(Ty::I64, 1);

    let b0 = b.binary(Op::And, Flags::NONE, rhs, one);
    let m0 = b.binary(Op::Sub, Flags::NONE, zero, b0);
    let mut lo = b.binary(Op::And, Flags::NONE, lhs, m0);
    let mut hi = zero;

    for i in 1u32..64 {
        let sh = b.iconst(Ty::I64, u128::from(i));
        let hi_sh = b.iconst(Ty::I64, u128::from(64 - i));
        let shifted_rhs = b.binary(Op::Lshr, Flags::NONE, rhs, sh);
        let bit = b.binary(Op::And, Flags::NONE, shifted_rhs, one);
        let mask = b.binary(Op::Sub, Flags::NONE, zero, bit);

        let term_lo = b.binary(Op::Shl, Flags::NONE, lhs, sh);
        let masked_lo = b.binary(Op::And, Flags::NONE, term_lo, mask);
        lo = b.binary(Op::Xor, Flags::NONE, lo, masked_lo);

        let term_hi = b.binary(Op::Lshr, Flags::NONE, lhs, hi_sh);
        let masked_hi = b.binary(Op::And, Flags::NONE, term_hi, mask);
        hi = b.binary(Op::Xor, Flags::NONE, hi, masked_hi);
    }
    (lo, hi)
}

/// Builds one 8x8 -> 16-bit carry-less multiply lane over GF(2) (`i8, i8 -> i16`).
pub fn build_pmull8<A: Allocator + Clone>(b: &mut Builder<A, Full>, lhs: Value, rhs: Value) -> Value {
    let a = b.convert(Op::Zext, Flags::NONE, Ty::I32, lhs);
    let d = b.convert(Op::Zext, Flags::NONE, Ty::I32, rhs);
    let zero = b.iconst(Ty::I32, 0);
    let one = b.iconst(Ty::I32, 1);

    let b0 = b.binary(Op::And, Flags::NONE, d, one);
    let m0 = b.binary(Op::Sub, Flags::NONE, zero, b0);
    let mut acc = b.binary(Op::And, Flags::NONE, a, m0);

    for i in 1u32..8 {
        let sh = b.iconst(Ty::I32, u128::from(i));
        let shifted_d = b.binary(Op::Lshr, Flags::NONE, d, sh);
        let bit = b.binary(Op::And, Flags::NONE, shifted_d, one);
        let mask = b.binary(Op::Sub, Flags::NONE, zero, bit);

        let term = b.binary(Op::Shl, Flags::NONE, a, sh);
        let masked = b.binary(Op::And, Flags::NONE, term, mask);
        acc = b.binary(Op::Xor, Flags::NONE, acc, masked);
    }
    b.convert(Op::Trunc, Flags::NONE, Ty::I16, acc)
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_lir::builder::{Param, Variadic};
    use blaze_lir::eval::{Outcome, Val, run_in};
    use blaze_lir::func::{BlockKind, Conv, Func};
    use blaze_lir::{Builder, Full, Ty};

    use super::{PmullFn, build_pmull8, build_pmull64, named};
    use crate::intrinsics::target::{Target, named as target_named};

    fn ref_pmull64(a: u64, b: u64) -> u128 {
        let mut out = 0u128;
        let a128 = u128::from(a);
        for i in 0..64 {
            if ((b >> i) & 1) != 0 {
                out ^= a128 << i;
            }
        }
        out
    }

    fn ref_pmull8(a: u8, b: u8) -> u16 {
        let mut out = 0u16;
        let a16 = u16::from(a);
        for i in 0..8 {
            if ((b >> i) & 1) != 0 {
                out ^= a16 << i;
            }
        }
        out
    }

    fn run_pmull64_lir(a: u64, b_val: u64) -> u128 {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64), Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let va = b.iconst(Ty::I64, u128::from(a));
        let vb = b.iconst(Ty::I64, u128::from(b_val));
        let (lo, hi) = build_pmull64(&mut b, va, vb);
        b.ret(&[lo, hi]);
        let func: Func<Global> = b.finish_checked().expect("valid pmull64 LIR");
        match run_in(func.view(), &[], 1_000_000, Global).expect("LIR eval") {
            Outcome::Returned(v) => match (v.first(), v.get(1)) {
                (Some(Val::Int(lo)), Some(Val::Int(hi))) => {
                    let lo64 = u64::try_from(*lo).expect("u64 lo");
                    let hi64 = u64::try_from(*hi).expect("u64 hi");
                    u128::from(lo64) | (u128::from(hi64) << 64)
                }
                other => panic!("expected two Ints, got {other:?}"),
            },
            other => panic!("did not return: {other:?}"),
        }
    }

    fn run_pmull8_lir(a: u8, b_val: u8) -> u16 {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I16)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let va = b.iconst(Ty::I8, u128::from(a));
        let vb = b.iconst(Ty::I8, u128::from(b_val));
        let res = build_pmull8(&mut b, va, vb);
        b.ret(&[res]);
        let func: Func<Global> = b.finish_checked().expect("valid pmull8 LIR");
        match run_in(func.view(), &[], 1_000_000, Global).expect("LIR eval") {
            Outcome::Returned(v) => match v.first() {
                Some(Val::Int(x)) => u16::try_from(*x).expect("u16 bits"),
                other => panic!("expected Int, got {other:?}"),
            },
            other => panic!("did not return: {other:?}"),
        }
    }

    #[test]
    fn pmull_matches_reference_and_hardware() {
        assert_eq!(named("llvm.aarch64.neon.pmull64"), Some(PmullFn::Pmull64));
        assert_eq!(target_named("llvm.aarch64.neon.pmull64"), Ok(Target::Pmull(PmullFn::Pmull64)));
        assert_eq!(Target::Pmull(PmullFn::Pmull64).link_name(), "llvm.aarch64.neon.pmull64");

        assert_eq!(named("llvm.aarch64.neon.pmull.v8i16"), Some(PmullFn::PmullV8I16));
        assert_eq!(target_named("llvm.aarch64.neon.pmull.v8i16"), Ok(Target::Pmull(PmullFn::PmullV8I16)));
        assert_eq!(Target::Pmull(PmullFn::PmullV8I16).link_name(), "llvm.aarch64.neon.pmull.v8i16");

        let pairs64 = [
            (0u64, 0u64),
            (1, 1),
            (0b11, 0b11),
            (1u64 << 63, 1u64 << 63),
            (u64::MAX, u64::MAX),
            (0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210),
        ];
        for (a, b) in pairs64 {
            let got = run_pmull64_lir(a, b);
            let want = ref_pmull64(a, b);
            assert_eq!(got, want, "pmull64({a:#x}, {b:#x})");
            #[cfg(all(target_arch = "aarch64", target_feature = "aes"))]
            unsafe {
                let hw = core::arch::aarch64::vmull_p64(a, b);
                assert_eq!(got, hw, "hw mismatch pmull64({a:#x}, {b:#x})");
            }
        }

        let pairs8 = [(0u8, 0u8), (1, 1), (0b11, 0b11), (0x80, 0x80), (0xff, 0xff), (0x5a, 0xa5), (0x1b, 0x73)];
        for (a, b) in pairs8 {
            let got = run_pmull8_lir(a, b);
            let want = ref_pmull8(a, b);
            assert_eq!(got, want, "pmull8({a:#x}, {b:#x})");
        }
    }
}
