//! Floats and 128-bit integers in the walk.
//!
//! - `f32` and `f64` are `Repr::Float` (their LIR scalar); `f16` and `f128` are refused by name.
//!   Arithmetic is `fadd`/`fsub`/`fmul`/`fdiv`/`frem` (the emitter refuses `frem`: a call of
//!   `fmod`); unary `-` is `fneg`. The Reference, "Comparison operators": a float comparison is
//!   IEEE 754's, so with a `NaN` on either side `==`, `<`, `<=`, `>`, `>=` are false and `!=` is
//!   true: `fcmp` `oeq`, `olt`, `ole`, `ogt`, `oge`, and `une`.
//! - "Type cast expressions" (the table's rows for numeric casts): "Casting from a float to an
//!   integer will round the float towards zero; `NaN` will return `0`; values larger than the
//!   maximum integer value, including `INFINITY`, will saturate to the maximum value of the
//!   integer type; values smaller than the minimum integer value, including `NEG_INFINITY`,
//!   will saturate to the minimum value": `fptosi`/`fptoui` by the target's signedness, which
//!   the emitter makes saturating (blaze-emit pass/float.rs). "Casting from an integer to float
//!   will produce the closest possible float": `sitofp`/`uitofp` by the source's signedness.
//!   "Casting from an f32 to an f64 is perfect and lossless"; "from an f64 to an f32 will
//!   produce the closest possible f32": `fpext`/`fptrunc`.
//! - `u128`/`i128` are `Repr::Int { ty: I128 }` (walk.rs `int_ty`): every integer path of the
//!   walk serves them; the emitter computes them in register pairs (pass/wide.rs).
//! - A float literal's value is `Adts::float_lit`: its bits at its type.

use core::alloc::Allocator;

use blaze_lir::{Flags, FloatCond, Op, Ty, Value};
use blaze_typeck::{BinOp, ExprId, FloatTy};

use super::{Repr, TyErr, V, Walk, unsupported};
use crate::BuildError;

/// How a float type is held: `f32` and `f64`; `f16` and `f128` are refused by name.
pub(super) fn float_repr(f: FloatTy) -> Result<Repr, TyErr> {
    Ok(Repr::Float(match f {
        FloatTy::F32 => Ty::F32,
        FloatTy::F64 => Ty::F64,
        FloatTy::F16 | FloatTy::F128 => return Err(TyErr::Unsupported("`f16` or `f128`")),
    }))
}

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// `1.5`: its bits at its type.
    pub(super) fn float_lit(&mut self, e: ExprId) -> Result<V, BuildError> {
        let Repr::Float(ty) = self.expr_repr(e)? else {
            return Err(unsupported(e, "a float literal of a type that is not `f32` or `f64`"));
        };
        let bits = self.adts.float_lit(self.body.owner, e).ok_or(BuildError::NoLiteralValue { expr: e })?;
        if ty == Ty::F32 && bits > u128::from(u32::MAX) || bits > u128::from(u64::MAX) {
            return Err(BuildError::Malformed { what: "a float literal's bits past its type's width" });
        }
        Ok(V::Scalar(self.b.fconst(ty, bits)))
    }

    /// A binary operator on two floats: a comparison (an `i8` of 0 or 1) or arithmetic.
    pub(super) fn float_binary(&mut self, e: ExprId, op: BinOp, l: Value, r: Value) -> Result<Value, BuildError> {
        let cond = match op {
            BinOp::Eq => Some(FloatCond::Oeq),
            BinOp::Ne => Some(FloatCond::Une),
            BinOp::Lt => Some(FloatCond::Olt),
            BinOp::Le => Some(FloatCond::Ole),
            BinOp::Gt => Some(FloatCond::Ogt),
            BinOp::Ge => Some(FloatCond::Oge),
            _ => None,
        };
        if let Some(c) = cond {
            let bit = self.b.fcmp(c, l, r);
            return Ok(self.b.convert(Op::Zext, Flags::NONE, Ty::I8, bit));
        }
        let o = match op {
            BinOp::Add => Op::Fadd,
            BinOp::Sub => Op::Fsub,
            BinOp::Mul => Op::Fmul,
            BinOp::Div => Op::Fdiv,
            BinOp::Rem => Op::Frem,
            _ => return Err(unsupported(e, "this operator on floats")),
        };
        Ok(self.b.binary(o, Flags::NONE, l, r))
    }

    /// A cast from or to a float (the Reference's table, above).
    pub(super) fn float_cast(&mut self, e: ExprId, v: V, from: Repr, to: Repr) -> Result<V, BuildError> {
        let out = match (from, to) {
            (Repr::Float(f), Repr::Float(t)) => {
                let x = self.scalar(v, f)?;
                match (f == t, t == Ty::F64) {
                    (true, _) => x,
                    (false, true) => self.b.convert(Op::Fpext, Flags::NONE, t, x),
                    (false, false) => self.b.convert(Op::Fptrunc, Flags::NONE, t, x),
                }
            }
            (Repr::Float(f), Repr::Int { ty, signed }) => {
                let x = self.scalar(v, f)?;
                if ty == Ty::I128 {
                    return Err(unsupported(e, "a float to `u128`/`i128` cast (compiler_builtins' `__fixdfti` and kin)"));
                }
                self.b.convert(if signed { Op::FptoSi } else { Op::FptoUi }, Flags::NONE, ty, x)
            }
            (Repr::Int { ty, signed }, Repr::Float(t)) => {
                let x = self.scalar(v, ty)?;
                if ty == Ty::I128 {
                    return Err(unsupported(e, "a `u128`/`i128` to float cast (compiler_builtins' `__floattidf` and kin)"));
                }
                self.b.convert(if signed { Op::SitoFp } else { Op::UitoFp }, Flags::NONE, t, x)
            }
            _ => return Err(unsupported(e, "this cast of a float (to or from other than a number)")),
        };
        Ok(V::Scalar(out))
    }
}
