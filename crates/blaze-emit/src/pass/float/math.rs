//! The float functions of one, two and three operands at `f32` and `f64`: `fabs`, `sqrt`, the
//! roundings, the IEEE min/max pairs, `fcopysign`, `fma` and `fmuladd` (INTRINSICS.md P7b: the
//! builder makes them for the generic LLVM float intrinsics the library's NEON code declares,
//! blaze-build intrinsics/target/float.rs). Each but `fcopysign` is one AArch64 instruction,
//! computed in `d0`..`d2` (`s0`..`s2`) as pass/float.rs computes `fadd`.
//!
//! Each instruction against LIR's meaning (blaze-lir op.rs) and the Arm ARM (DDI 0487, C7.2):
//! - `fabs` FABS, `sqrt` FSQRT (correctly rounded, as IEEE `squareRoot`);
//! - `floor` FRINTM (toward minus infinity), `ceil` FRINTP (toward plus infinity), `ftrunc`
//!   FRINTZ (toward zero), `nearest` FRINTN (to nearest, ties to even), `fround` FRINTA (to
//!   nearest, ties away from zero): each "rounds a floating-point value to an integral
//!   floating-point value of the same size" in the mode its name fixes, not FPCR's;
//! - `fmin_num`/`fmax_num` (IEEE 754-2008 `minNum`/`maxNum`: a NaN operand gives the other)
//!   FMINNM/FMAXNM: "If one value is a quiet NaN and the other is numerical, the result is the
//!   numerical value";
//! - `fminimum`/`fmaximum` (IEEE 754-2019, NaN propagating, -0 below +0) FMIN/FMAX: a NaN
//!   operand gives a NaN, and "-0.0 is less than +0.0";
//! - `fma` (`a * b + c` rounded once) FMADD `d = n * m + a`, unrounded product; `fmuladd`
//!   ("fused or not as the consumer finds faster") the same FMADD;
//! - `fcopysign` (the magnitude of `a`, the sign of `b`): in integer registers, the sign bit
//!   of `b` inserted into `a`'s bits by LSR and BFI (the cell holds an `f32`'s bits
//!   zero-extended, so bit 31 is its sign; an `f64`'s is bit 63).

use alloc::alloc::Allocator;

use blaze_lir::Op;

use super::super::{EmitError, Pass, unsupported};
use crate::enc::{self, Reg, X9, X10};

#[cfg(test)]
mod tests;

/// `d0`, `d1`, `d2` (`s0`..`s2`).
const V0: u32 = 0;
const V1: u32 = 1;
const V2: u32 = 2;

/// FP data-processing (1 source): `opcode` 000001 FABS, 000011 FSQRT, 001000 FRINTN,
/// 001001 FRINTP, 001010 FRINTM, 001011 FRINTZ, 001100 FRINTA.
pub(crate) const fn fone(opcode: u32, double: bool, rd: Reg, rn: Reg) -> u32 {
    0x1e20_4000 | ((double as u32) << 22) | (opcode << 15) | (rn << 5) | rd
}

/// FP data-processing (3 source), FMADD: `rd = rn * rm + ra`, rounded once.
pub(crate) const fn fmadd(double: bool, rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0x1f00_0000 | ((double as u32) << 22) | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// `lsr xd, xn, #shift` (UBFM xd, xn, #shift, #63).
pub(crate) const fn lsr64(rd: Reg, rn: Reg, shift: u32) -> u32 {
    0xd340_0000 | (shift << 16) | (63 << 10) | (rn << 5) | rd
}

/// `bfi xd, xn, #lsb, #1` (BFM xd, xn, #((64 - lsb) % 64), #0): bit 0 of `xn` into bit `lsb`
/// of `xd`, the rest of `xd` kept.
pub(crate) const fn bfi1(rd: Reg, rn: Reg, lsb: u32) -> u32 {
    0xb340_0000 | (((64 - lsb) % 64) << 16) | (rn << 5) | rd
}

/// The one-source opcode of `op`, if it is one.
const fn one_source(op: Op) -> Option<u32> {
    Some(match op {
        Op::Fabs => 0b000001,
        Op::Sqrt => 0b000011,
        Op::Nearest => 0b001000,
        Op::Ceil => 0b001001,
        Op::Floor => 0b001010,
        Op::Ftrunc => 0b001011,
        Op::Fround => 0b001100,
        _ => return None,
    })
}

/// The two-source opcode (pass/float.rs `fbin`'s) of `op`, if it is one.
const fn two_source(op: Op) -> Option<u32> {
    Some(match op {
        Op::Fmaximum => 0b0100,
        Op::Fminimum => 0b0101,
        Op::FmaxNum => 0b0110,
        Op::FminNum => 0b0111,
        _ => return None,
    })
}

impl<A: Allocator> Pass<'_, '_, A> {
    /// Emits float function `inst` of `op` (the module doc's list).
    pub(super) fn float_math(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let a = o.value();
        let double = self.float_of(inst, op, a)?;
        if let Some(opcode) = one_source(op) {
            self.fld(V0, a, double);
            self.put(fone(opcode, double, V0, V0));
            self.fst(V0, inst, double);
            return Ok(());
        }
        if let Some(opcode) = two_source(op) {
            let b = o.value();
            self.fld(V0, a, double);
            self.fld(V1, b, double);
            self.put(enc::fbin(opcode, double, V0, V0, V1));
            self.fst(V0, inst, double);
            return Ok(());
        }
        match op {
            Op::Fma | Op::Fmuladd => {
                let (b, c) = (o.value(), o.value());
                self.fld(V0, a, double);
                self.fld(V1, b, double);
                self.fld(V2, c, double);
                self.put(fmadd(double, V0, V0, V1, V2));
                self.fst(V0, inst, double);
            }
            Op::Fcopysign => {
                let b = o.value();
                let sign = if double { 63 } else { 31 };
                self.ld(X9, a);
                self.ld(X10, b);
                self.put(lsr64(X10, X10, sign));
                self.put(bfi1(X9, X10, sign));
                self.st(X9, inst);
            }
            _ => return Err(unsupported(inst, op, "an operation routed to the float functions that is not one (a router bug)")),
        }
        Ok(())
    }
}
