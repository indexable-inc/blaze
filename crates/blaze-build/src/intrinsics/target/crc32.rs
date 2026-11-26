//! The AArch64 CRC32 and CRC32C intrinsics (`FEAT_CRC32`): `llvm.aarch64.crc32{b,h,w,x}` (IEEE
//! 802.3 polynomial `0xEDB8_8320`) and `llvm.aarch64.crc32c{b,h,w,x}` (Castagnoli polynomial
//! `0x82F6_3B78`), built as branchless scalar LIR operations from the Arm Architecture Reference
//! Manual pseudocode so every backend and the LIR reference evaluator compute the exact bits.
//!
//! The pinned stdarch declares them in `extern "llvm-intrinsic"` blocks:
//! - `arm_shared/neon/generated.rs`: `crc32{b,h,w}` / `crc32c{b,h,w}` as `fn(u32, u32) -> u32`;
//! - `aarch64/neon/generated.rs`: `crc32x` / `crc32cx` as `fn(u32, u64) -> u32`.

use core::alloc::Allocator;

use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};

/// IEEE 802.3 CRC-32 reversed polynomial (`crc32{b,h,w,x}`).
const POLY_IEEE: u32 = 0xEDB8_8320;
/// Castagnoli CRC-32C reversed polynomial (`crc32c{b,h,w,x}`).
const POLY_CASTAGNOLI: u32 = 0x82F6_3B78;

/// An AArch64 CRC32 or CRC32C intrinsic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Crc32Fn {
    Crc32b,
    Crc32h,
    Crc32w,
    Crc32x,
    Crc32cb,
    Crc32ch,
    Crc32cw,
    Crc32cx,
}

impl Crc32Fn {
    /// The LLVM link name declared by stdarch.
    pub const fn link_name(self) -> &'static str {
        match self {
            Self::Crc32b => "llvm.aarch64.crc32b",
            Self::Crc32h => "llvm.aarch64.crc32h",
            Self::Crc32w => "llvm.aarch64.crc32w",
            Self::Crc32x => "llvm.aarch64.crc32x",
            Self::Crc32cb => "llvm.aarch64.crc32cb",
            Self::Crc32ch => "llvm.aarch64.crc32ch",
            Self::Crc32cw => "llvm.aarch64.crc32cw",
            Self::Crc32cx => "llvm.aarch64.crc32cx",
        }
    }

    /// Expected LIR scalar type of the second (`data`) operand.
    pub const fn data_ty(self) -> Ty {
        match self {
            Self::Crc32x | Self::Crc32cx => Ty::I64,
            Self::Crc32b | Self::Crc32h | Self::Crc32w | Self::Crc32cb | Self::Crc32ch | Self::Crc32cw => Ty::I32,
        }
    }

    const fn bits(self) -> u32 {
        match self {
            Self::Crc32b | Self::Crc32cb => 8,
            Self::Crc32h | Self::Crc32ch => 16,
            Self::Crc32w | Self::Crc32cw => 32,
            Self::Crc32x | Self::Crc32cx => 64,
        }
    }

    const fn poly(self) -> u32 {
        match self {
            Self::Crc32b | Self::Crc32h | Self::Crc32w | Self::Crc32x => POLY_IEEE,
            Self::Crc32cb | Self::Crc32ch | Self::Crc32cw | Self::Crc32cx => POLY_CASTAGNOLI,
        }
    }
}

/// Parses an AArch64 CRC32 / CRC32C LLVM intrinsic link name.
pub fn named(link_name: &str) -> Option<Crc32Fn> {
    Some(match link_name {
        "llvm.aarch64.crc32b" => Crc32Fn::Crc32b,
        "llvm.aarch64.crc32h" => Crc32Fn::Crc32h,
        "llvm.aarch64.crc32w" => Crc32Fn::Crc32w,
        "llvm.aarch64.crc32x" => Crc32Fn::Crc32x,
        "llvm.aarch64.crc32cb" => Crc32Fn::Crc32cb,
        "llvm.aarch64.crc32ch" => Crc32Fn::Crc32ch,
        "llvm.aarch64.crc32cw" => Crc32Fn::Crc32cw,
        "llvm.aarch64.crc32cx" => Crc32Fn::Crc32cx,
        _ => return None,
    })
}

fn step_bits<A: Allocator + Clone>(b: &mut Builder<A, Full>, mut acc: Value, count: u32, one: Value, zero: Value, poly: Value) -> Value {
    for _ in 0..count {
        let lsb = b.binary(Op::And, Flags::NONE, acc, one);
        let mask = b.binary(Op::Sub, Flags::NONE, zero, lsb);
        let shifted = b.binary(Op::Lshr, Flags::NONE, acc, one);
        let feedback = b.binary(Op::And, Flags::NONE, mask, poly);
        acc = b.binary(Op::Xor, Flags::NONE, shifted, feedback);
    }
    acc
}

/// Builds the CRC32/CRC32C update over `crc: i32` and `data: f.data_ty()`, returning `i32`.
pub fn build<A: Allocator + Clone>(b: &mut Builder<A, Full>, f: Crc32Fn, crc: Value, data: Value) -> Value {
    let one = b.iconst(Ty::I32, 1);
    let zero = b.iconst(Ty::I32, 0);
    let poly = b.iconst(Ty::I32, u128::from(f.poly()));
    match f.bits() {
        8 => {
            let m8 = b.iconst(Ty::I32, 0xff);
            let d = b.binary(Op::And, Flags::NONE, data, m8);
            let acc = b.binary(Op::Xor, Flags::NONE, crc, d);
            step_bits(b, acc, 8, one, zero, poly)
        }
        16 => {
            let m16 = b.iconst(Ty::I32, 0xffff);
            let d = b.binary(Op::And, Flags::NONE, data, m16);
            let acc = b.binary(Op::Xor, Flags::NONE, crc, d);
            step_bits(b, acc, 16, one, zero, poly)
        }
        32 => {
            let acc = b.binary(Op::Xor, Flags::NONE, crc, data);
            step_bits(b, acc, 32, one, zero, poly)
        }
        _ => {
            let lo = b.convert(Op::Trunc, Flags::NONE, Ty::I32, data);
            let acc = b.binary(Op::Xor, Flags::NONE, crc, lo);
            let acc = step_bits(b, acc, 32, one, zero, poly);
            let shift32 = b.iconst(Ty::I64, 32);
            let hi64 = b.binary(Op::Lshr, Flags::NONE, data, shift32);
            let hi = b.convert(Op::Trunc, Flags::NONE, Ty::I32, hi64);
            let acc = b.binary(Op::Xor, Flags::NONE, acc, hi);
            step_bits(b, acc, 32, one, zero, poly)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_lir::builder::{Param, Variadic};
    use blaze_lir::eval::{Outcome, Val, run_in};
    use blaze_lir::func::{BlockKind, Conv, Func};
    use blaze_lir::{Builder, Full, Ty};

    use super::{Crc32Fn, POLY_CASTAGNOLI, POLY_IEEE, build, named};
    use crate::intrinsics::target::{Target, named as target_named};

    fn ref_crc(mut crc: u32, data: u64, bits: u32, poly: u32) -> u32 {
        let mut d = data;
        for _ in 0..bits {
            let bit = (crc ^ (d as u32)) & 1;
            crc >>= 1;
            if bit != 0 {
                crc ^= poly;
            }
            d >>= 1;
        }
        crc
    }

    fn run_lir(f: Crc32Fn, crc: u32, data: u64) -> u32 {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let v_crc = b.iconst(Ty::I32, u128::from(crc));
        let v_data = b.iconst(f.data_ty(), u128::from(data));
        let res = build(&mut b, f, v_crc, v_data);
        b.ret(&[res]);
        let func: Func<Global> = b.finish_checked().expect("valid CRC32 LIR");
        match run_in(func.view(), &[], 1_000_000, Global).expect("LIR eval") {
            Outcome::Returned(v) => match v.first() {
                Some(Val::Int(x)) => u32::try_from(*x).expect("u32 bits"),
                other => panic!("expected Int, got {other:?}"),
            },
            other => panic!("did not return: {other:?}"),
        }
    }

    #[test]
    fn crc32_and_crc32c_match_reference_and_hardware() {
        let cases = [
            (Crc32Fn::Crc32b, "llvm.aarch64.crc32b", 8, POLY_IEEE),
            (Crc32Fn::Crc32h, "llvm.aarch64.crc32h", 16, POLY_IEEE),
            (Crc32Fn::Crc32w, "llvm.aarch64.crc32w", 32, POLY_IEEE),
            (Crc32Fn::Crc32x, "llvm.aarch64.crc32x", 64, POLY_IEEE),
            (Crc32Fn::Crc32cb, "llvm.aarch64.crc32cb", 8, POLY_CASTAGNOLI),
            (Crc32Fn::Crc32ch, "llvm.aarch64.crc32ch", 16, POLY_CASTAGNOLI),
            (Crc32Fn::Crc32cw, "llvm.aarch64.crc32cw", 32, POLY_CASTAGNOLI),
            (Crc32Fn::Crc32cx, "llvm.aarch64.crc32cx", 64, POLY_CASTAGNOLI),
        ];
        for (f, name, bits, poly) in cases {
            assert_eq!(named(name), Some(f));
            assert_eq!(target_named(name), Ok(Target::Crc32(f)));
            assert_eq!(Target::Crc32(f).link_name(), name);
            for (crc, data) in [(0u32, 0u64), (0xffff_ffff, 0x0123_4567_89ab_cdef), (0xdead_beef, 0xfedc_ba98_7654_3210)] {
                let got = run_lir(f, crc, data);
                let want = ref_crc(crc, data, bits, poly);
                assert_eq!(got, want, "{name}({crc:#x}, {data:#x})");
                #[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
                unsafe {
                    let hw = match f {
                        Crc32Fn::Crc32b => core::arch::aarch64::__crc32b(crc, data as u8),
                        Crc32Fn::Crc32h => core::arch::aarch64::__crc32h(crc, data as u16),
                        Crc32Fn::Crc32w => core::arch::aarch64::__crc32w(crc, data as u32),
                        Crc32Fn::Crc32x => core::arch::aarch64::__crc32d(crc, data),
                        Crc32Fn::Crc32cb => core::arch::aarch64::__crc32cb(crc, data as u8),
                        Crc32Fn::Crc32ch => core::arch::aarch64::__crc32ch(crc, data as u16),
                        Crc32Fn::Crc32cw => core::arch::aarch64::__crc32cw(crc, data as u32),
                        Crc32Fn::Crc32cx => core::arch::aarch64::__crc32cd(crc, data),
                    };
                    assert_eq!(got, hw, "hw mismatch {name}({crc:#x}, {data:#x})");
                }
            }
        }
    }
}
