//! `f16` widened (INTRINSICS.md 3.5): an operation is done in `f32` and its result truncated
//! back, where that is exact, and on the bits where it changes only the sign.
//!
//! Why widening is exact: `f16` has p = 11 significand bits and `f32` has q = 24 >= 2p + 2,
//! so for `+ - * /` and `sqrt` rounding the exact result to `f32` and then to `f16` equals
//! rounding it to `f16` once (Figueroa, "When is double rounding innocuous?", SIGNUM 1995).
//! `%`, min/max, and rounding to an integer give a result exactly representable in `f16`, so
//! the `f32` result is it. Every `f16` is exact in `f32` (`fpext`), and comparisons and
//! conversions to integers read the exact value. An integer converted to `f32` is exact below
//! 2^24, and from 2^24 up both routes give infinity in `f16` (whose largest finite value is
//! 65504 and whose overflow threshold is 65520), so the second rounding never differs.
//!
//! An `f16` fused multiply-add is refused: neither `f32` nor `f64` makes it exact, and the
//! routine set ([`super::SoftFloat`]) has none.

use super::{LegalizeError, Out, SoftFloat};
use crate::{Op, Ty, ty::Scalar};

/// The sign bit of an `f16` (IEEE 754 binary16: 1 sign, 5 exponent, 10 fraction bits).
const SIGN: u128 = 0x8000;

impl Out<'_, '_, '_> {
    /// `op` touching `f16` (and not `f128`) done in `f32` or on its bits; `None` for an
    /// operation the tiers take on `f16` as it is (a move, `fpext` to `f32`, `fptrunc` from
    /// `f32`).
    pub(super) fn half(
        &mut self,
        inst: usize,
        op: Op,
        flags: u8,
        cond: u32,
        ty: Ty,
        src: Ty,
        args: &[u32],
    ) -> Result<Option<u32>, LegalizeError> {
        let value = match op {
            Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround => self.in_f32(op, flags, args),
            // "fused or not as the consumer finds faster" (op.rs, `Fmuladd`): not fused, each
            // step rounded to `f16` as the unfused form is.
            Op::Fmuladd => {
                let product = self.in_f32(Op::Fmul, flags, &args[..2]);
                self.in_f32(Op::Fadd, flags, &[product, args[2]])
            }
            Op::Fma => {
                return Err(self.refuse(inst, "an `f16` fused multiply-add (no exact widening and no routine)"));
            }
            Op::Fneg | Op::Fabs | Op::Fcopysign => self.half_sign(op, args),
            Op::Fcmp => {
                let a = self.widen(args[0]);
                let b = self.widen(args[1]);
                self.emit(Op::Fcmp, flags, ty, cond, &[a, b])
            }
            Op::FptoSi | Op::FptoUi => {
                let a = self.widen(args[0]);
                self.emit(op, flags, ty, 0, &[a])
            }
            Op::SitoFp | Op::UitoFp => {
                let wide = self.emit(op, flags, Ty::F32, 0, args);
                self.narrow(wide)
            }
            Op::Fpext => match ty.elem() {
                Scalar::F32 => return Ok(None),
                Scalar::F64 => {
                    let wide = self.widen(args[0]);
                    self.emit(Op::Fpext, flags, Ty::F64, 0, &[wide])
                }
                _ => return Err(self.refuse(inst, "an `fpext` of `f16` to a type that is not a wider float")),
            },
            Op::Fptrunc => match src.elem() {
                Scalar::F32 => return Ok(None),
                // Through `f128`, exact for every `f64`: one rounding, to `f16`. (Through `f32`
                // it would round twice, and `f32` is not wide enough for that to be
                // innocuous: 24 < 2 * 53 + 2.)
                Scalar::F64 => {
                    let quad = self.routine(inst, SoftFloat::ExtendDfTf2, args)?;
                    self.routine(inst, SoftFloat::TruncTfHf2, &[quad])?
                }
                _ => return Err(self.refuse(inst, "an `fptrunc` to `f16` from a type that is not a wider float")),
            },
            _ => return Ok(None),
        };
        self.stats.widened += 1;
        Ok(Some(value))
    }

    /// `fpext` of an `f16` to `f32`.
    fn widen(&mut self, a: u32) -> u32 {
        self.emit(Op::Fpext, 0, Ty::F32, 0, &[a])
    }

    /// `fptrunc` of an `f32` to `f16`.
    fn narrow(&mut self, a: u32) -> u32 {
        self.emit(Op::Fptrunc, 0, Ty::F16, 0, &[a])
    }

    /// `op` over `f16` operands, done in `f32`, truncated back.
    fn in_f32(&mut self, op: Op, flags: u8, args: &[u32]) -> u32 {
        let mut wide = [0u32; 3];
        for (k, &arg) in args.iter().enumerate() {
            wide[k] = self.widen(arg);
        }
        let result = self.emit(op, flags, Ty::F32, 0, &wide[..args.len()]);
        self.narrow(result)
    }

    /// `fneg`, `fabs` and `fcopysign` of `f16` on its bits (IEEE 754-2019 5.5.1: they change
    /// "only the sign bit"; through `f32` a signalling NaN's payload would change).
    fn half_sign(&mut self, op: Op, args: &[u32]) -> u32 {
        let a = self.emit(Op::Bitcast, 0, Ty::I16, 0, &[args[0]]);
        let bits = match op {
            Op::Fneg => {
                let sign = self.iconst(Ty::I16, SIGN);
                self.emit(Op::Xor, 0, Ty::I16, 0, &[a, sign])
            }
            Op::Fabs => {
                let magnitude = self.iconst(Ty::I16, !SIGN);
                self.emit(Op::And, 0, Ty::I16, 0, &[a, magnitude])
            }
            _ => {
                let b = self.emit(Op::Bitcast, 0, Ty::I16, 0, &[args[1]]);
                let magnitude = self.iconst(Ty::I16, !SIGN);
                let sign = self.iconst(Ty::I16, SIGN);
                let kept = self.emit(Op::And, 0, Ty::I16, 0, &[a, magnitude]);
                let taken = self.emit(Op::And, 0, Ty::I16, 0, &[b, sign]);
                self.emit(Op::Or, 0, Ty::I16, 0, &[kept, taken])
            }
        };
        self.emit(Op::Bitcast, 0, Ty::F16, 0, &[bits])
    }
}
