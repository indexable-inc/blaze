//! `f32` and `f64` (lib.rs, "Floats"). A float's cell holds its bits as the cell invariant
//! says for an integer of its width (an `f32`'s 32 bits zero-extended); an operation moves its
//! operands into `d0`/`d1` (`s0`/`s1`) by `fmov`, computes, and moves the result back.
//!
//! The semantics are Rust's (the Reference, "Operator expressions"):
//! - arithmetic is IEEE 754 in the default rounding mode (FPCR's round to nearest, ties to even,
//!   which no code here changes);
//! - "Type cast expressions": "Casting from a float to an integer will round the float towards
//!   zero; `NaN` will return `0`; values larger than the maximum integer value, including
//!   `INFINITY`, will saturate to the maximum value of the integer type; values smaller than
//!   the minimum integer value, including `NEG_INFINITY`, will saturate to the minimum": that
//!   is `fcvtzs`/`fcvtzu` at 32 and 64 bits; 8 and 16 bits clamp the 32-bit result. "Casting
//!   from an integer to float will produce the closest possible float": `scvtf`/`ucvtf` from
//!   the value extended to 64 bits. "Casting from an f32 to an f64 is perfect and lossless";
//!   "from an f64 to an f32 will produce the closest possible f32": `fcvt`.
//! - comparisons: every ordered condition is false when either side is `NaN`, every unordered
//!   one true (`!=` is unordered: `NaN != NaN`).
//!
//! `frem` and the conversions to and from `i128` are calls (pass/runtime.rs).
//!
//! The float functions (`fabs`, `sqrt`, the roundings, the min/max pairs, `fcopysign`, `fma`,
//! `fmuladd`) are pass/float/math.rs's.
//!
//! `f16` and `f128` are only moved (pass/float_width.rs, which `lower` asks first); an
//! operation on one that reaches here is refused by name, as are the two conditions that need
//! two flags (`one`, `ueq`).

use alloc::alloc::Allocator;

mod math;

use blaze_lir::{FloatCond, Op, Ty, Value};

use super::{EmitError, Pass, unsupported};
use super::float_width::FloatWidth;
use crate::enc::{self, Cond, X9, X10};

/// `d0`/`s0` and `d1`/`s1`: the vector registers an operation computes in.
const V0: u32 = 0;
const V1: u32 = 1;

impl<A: Allocator> Pass<'_, '_, A> {
    /// Whether `ty` is an `f32` (`Some(false)`) or an `f64` (`Some(true)`).
    pub(super) fn float_ty(ty: Ty) -> Option<bool> {
        FloatWidth::of(ty).and_then(FloatWidth::computed)
    }

    fn float_of(&self, inst: usize, op: Op, value: Value) -> Result<bool, EmitError> {
        Self::float_ty(self.view.ty(value)).ok_or(unsupported(inst, op, "a float operand that is not an `f32` or `f64`"))
    }

    /// `v = ` value's float, through `x9`.
    fn fld(&mut self, v: u32, value: Value, double: bool) {
        self.ld(X9, value);
        self.put(if double { enc::fmov_to_d(v, X9) } else { enc::fmov_to_s(v, X9) });
    }

    /// Stores float register `v` to `inst`'s cell, its bits zero-extended.
    fn fst(&mut self, v: u32, inst: usize, double: bool) {
        self.put(if double { enc::fmov_from_d(X9, v) } else { enc::fmov_from_s(X9, v) });
        self.st(X9, inst);
    }

    /// An entry parameter or a call's return in vector register `v`, to `inst`'s cell.
    pub(super) fn float_in(&mut self, v: u32, inst: usize, double: bool) {
        self.fst(v, inst, double);
    }

    /// `value` into vector register `v`, as an argument or a return.
    pub(super) fn float_out(&mut self, v: u32, value: Value, double: bool) {
        self.fld(v, value, double);
    }

    pub(super) fn float(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let result = || Self::float_ty(view.ty(Value(inst as u32)));
        match op {
            Op::Fconst => {
                let double = result().ok_or(unsupported(inst, op, "a float constant of other than `f32`"))?;
                if double {
                    return Err(unsupported(inst, op, "an `f64` constant in one word"));
                }
                self.mov_imm(X9, u64::from(o.imm()));
                self.st(X9, inst);
            }
            Op::FconstWide => {
                let words = o.constant();
                if result() != Some(true) || words.len() != 2 {
                    return Err(unsupported(inst, op, "a wide float constant of other than `f64` (`f16`, `f128`)"));
                }
                self.mov_imm(X9, u64::from(words[0]) | (u64::from(words[1]) << 32));
                self.st(X9, inst);
            }
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv => {
                let (a, b) = (o.value(), o.value());
                let double = self.float_of(inst, op, a)?;
                self.fld(V0, a, double);
                self.fld(V1, b, double);
                let opcode = match op {
                    Op::Fadd => 0b0010,
                    Op::Fsub => 0b0011,
                    Op::Fmul => 0b0000,
                    _ => 0b0001,
                };
                self.put(enc::fbin(opcode, double, V0, V0, V1));
                self.fst(V0, inst, double);
            }
            Op::Fneg => {
                let a = o.value();
                let double = self.float_of(inst, op, a)?;
                self.fld(V0, a, double);
                self.put(enc::fneg(double, V0, V0));
                self.fst(V0, inst, double);
            }
            Op::Fcmp => {
                let cond = o.float_cond();
                let (a, b) = (o.value(), o.value());
                let double = self.float_of(inst, op, a)?;
                // After `fcmp`: equal 0110, less 1000, greater 0010, unordered 0011 (NZCV).
                let c = match cond {
                    FloatCond::Oeq => Some(Cond::Eq),
                    FloatCond::Une => Some(Cond::Ne),
                    FloatCond::Olt => Some(Cond::Mi),
                    FloatCond::Ole => Some(Cond::Ls),
                    FloatCond::Ogt => Some(Cond::Gt),
                    FloatCond::Oge => Some(Cond::Ge),
                    FloatCond::Ord => Some(Cond::Vc),
                    FloatCond::Uno => Some(Cond::Vs),
                    FloatCond::Ult => Some(Cond::Lt),
                    FloatCond::Ule => Some(Cond::Le),
                    FloatCond::Ugt => Some(Cond::Hi),
                    FloatCond::Uge => Some(Cond::Pl),
                    FloatCond::False | FloatCond::True => None,
                    FloatCond::One | FloatCond::Ueq => {
                        return Err(unsupported(inst, op, "`fcmp one` or `ueq` (two flags)"));
                    }
                };
                match c {
                    Some(c) => {
                        self.fld(V0, a, double);
                        self.fld(V1, b, double);
                        self.put(enc::fcmp(double, V0, V1));
                        self.put(enc::cset(X9, c));
                    }
                    None => self.mov_imm(X9, u64::from(cond == FloatCond::True)),
                }
                self.st(X9, inst);
            }
            Op::Fpext | Op::Fptrunc => {
                let a = o.value();
                let from = self.float_of(inst, op, a)?;
                let to = result().ok_or(unsupported(inst, op, "a float conversion to other than `f32` or `f64`"))?;
                if from == to || to != (op == Op::Fpext) {
                    return Err(unsupported(inst, op, "`fpext` or `fptrunc` not between `f32` and `f64`"));
                }
                self.fld(V0, a, from);
                self.put(enc::fcvt(to, V0, V0));
                self.fst(V0, inst, to);
            }
            // To and from `i128`: the runtime's routines (pass/runtime.rs).
            Op::FptoSi | Op::FptoUi if Self::is_wide(view.ty(Value(inst as u32))) => return self.float_to_wide(inst, op),
            Op::SitoFp | Op::UitoFp if Self::is_wide(view.ty(view.operands(inst).value())) => return self.wide_to_float(inst, op),
            Op::FptoSi | Op::FptoUi => {
                let a = o.value();
                let double = self.float_of(inst, op, a)?;
                let width = self.result_width(inst, op)?;
                let signed = op == Op::FptoSi;
                if width < 8 {
                    return Err(unsupported(inst, op, "a float to an `i1`"));
                }
                self.fld(V0, a, double);
                self.put(enc::fcvtz(signed, width == 64, double, X9, V0));
                if width < 32 {
                    // Saturated at 32 bits; clamped again to the narrower type's range.
                    let max = (1u64 << (width - signed as u32)) - 1;
                    if signed {
                        self.put(enc::sext(X9, X9, 32));
                        self.mov_imm(X10, max);
                        self.put(enc::cmp(X9, X10));
                        self.put(enc::csel(X9, X10, X9, Cond::Gt));
                        self.mov_imm(X10, (-(max as i64) - 1) as u64);
                        self.put(enc::cmp(X9, X10));
                        self.put(enc::csel(X9, X10, X9, Cond::Lt));
                    } else {
                        self.mov_imm(X10, max);
                        self.put(enc::cmp(X9, X10));
                        self.put(enc::csel(X9, X10, X9, Cond::Hi));
                    }
                }
                self.narrow(X9, width);
                self.st(X9, inst);
            }
            Op::SitoFp | Op::UitoFp => {
                let a = o.value();
                let width = self.width_of(inst, op, a)?;
                let double = result().ok_or(unsupported(inst, op, "an integer to other than `f32` or `f64`"))?;
                self.ld(X9, a);
                let signed = op == Op::SitoFp;
                if signed {
                    self.widen_signed(X9, width);
                }
                self.put(enc::cvtf(signed, true, double, V0, X9));
                self.fst(V0, inst, double);
            }
            // The same bits under another type of their width: the cell is already right.
            Op::Bitcast => {
                let a = o.value();
                let (from, to) = (view.ty(a), view.ty(Value(inst as u32)));
                let bits = |t: Ty| match Self::float_ty(t) {
                    Some(d) => Some(if d { 64 } else { 32 }),
                    None => self.width(t),
                };
                match (bits(from), bits(to)) {
                    (Some(f), Some(t)) if f == t => {
                        self.ld(X9, a);
                        self.st(X9, inst);
                    }
                    _ => return Err(unsupported(inst, op, "a bitcast of other than a float and an integer of its width")),
                }
            }
            Op::Load => {
                let double = result().ok_or(unsupported(inst, op, "a float load of other than `f32` or `f64`"))?;
                let (ptr, off) = (o.value(), o.offset());
                self.ld(X9, ptr);
                self.mem(true, if double { enc::Size::X } else { enc::Size::W }, X9, X9, off);
                self.st(X9, inst);
            }
            Op::Store => {
                let (value, ptr, off) = (o.value(), o.value(), o.offset());
                let double = self.float_of(inst, op, value)?;
                self.ld(X9, value);
                self.ld(X10, ptr);
                self.mem(false, if double { enc::Size::X } else { enc::Size::W }, X9, X10, off);
            }
            // A call of `fmod`/`fmodf` (pass/runtime.rs).
            Op::Frem => return self.frem(inst, op),
            // pass/float/math.rs.
            Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fma
            | Op::Fmuladd => return self.float_math(inst, op),
            _ => return Err(unsupported(inst, op, "this float operation (none other is routed here: a router bug)")),
        }
        Ok(())
    }
}
