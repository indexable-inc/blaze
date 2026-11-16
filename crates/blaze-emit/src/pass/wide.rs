//! `i128` (lib.rs, "128-bit integers"): a value's 16-byte cell holds its low word at 0 and its
//! high word at 8. An operation loads both words of each operand into `x0..x3` (free inside a
//! body: the parameters were stored to their cells in the prologue, and a call sets them
//! anew), computes with the carries, and stores both words.
//!
//! The Reference, "Type layout": `u128`/`i128` are 16 bytes; their alignment is the target's
//! (16 on arm64). AAPCS64 C.8/C.9: a 16-byte-aligned integer argument takes the next
//! even-numbered pair of `x` registers (`x0:x1`, `x2:x3`, ...), low word first; one is returned
//! in `x0:x1` (pass.rs `run`, `call`, `Op::Return`).
//!
//! Refused by name: division and remainder (compiler_builtins' `__udivti3`, `__divti3`,
//! `__umodti3`, `__modti3`: no routine of the run path calls them yet), conversions to and
//! from floats (`__floattidf` and kin), the overflow and saturating forms, rotations and the
//! bit counts.

use std::alloc::Allocator;

use blaze_lir::{IntCond, Op, Ty, Value};

use super::{EmitError, Pass, unsupported};
use crate::enc::{self, Cond, Reg, Size, X0, X1, X2, X9};

const X3: Reg = 3;
const X4: Reg = 4;
const X5: Reg = 5;
const X6: Reg = 6;
const X7: Reg = 7;

impl<A: Allocator> Pass<'_, '_, A> {
    pub(super) fn is_wide(ty: Ty) -> bool {
        ty == Ty::I128
    }

    /// `lo, hi = ` value's two words.
    pub(super) fn ld2(&mut self, lo: Reg, hi: Reg, value: Value) {
        self.ld_sp(lo, value.0 * 16);
        self.ld_sp(hi, value.0 * 16 + 8);
    }

    /// Stores `lo, hi` to `inst`'s cell.
    pub(super) fn st2(&mut self, lo: Reg, hi: Reg, inst: usize) {
        self.st_sp(lo, inst as u32 * 16);
        self.st_sp(hi, inst as u32 * 16 + 8);
    }

    pub(super) fn wide(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        match op {
            Op::Iconst => {
                let v = i64::from(o.imm() as i32);
                self.mov_imm(X0, v as u64);
                self.mov_imm(X1, (v >> 63) as u64);
                self.st2(X0, X1, inst);
            }
            Op::IconstWide => {
                let w = o.constant();
                if w.len() != 4 {
                    return Err(unsupported(inst, op, "a 128-bit constant of other than four words"));
                }
                self.mov_imm(X0, u64::from(w[0]) | (u64::from(w[1]) << 32));
                self.mov_imm(X1, u64::from(w[2]) | (u64::from(w[3]) << 32));
                self.st2(X0, X1, inst);
            }
            Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor => {
                let (a, b) = (o.value(), o.value());
                self.ld2(X0, X1, a);
                self.ld2(X2, X3, b);
                match op {
                    Op::Add => {
                        self.put(enc::adds(X0, X0, X2));
                        self.put(enc::adc(X1, X1, X3));
                    }
                    Op::Sub => {
                        self.put(enc::subs(X0, X0, X2));
                        self.put(enc::sbc(X1, X1, X3));
                    }
                    // (a1·2⁶⁴ + a0)(b1·2⁶⁴ + b0) mod 2¹²⁸ = a0·b0 + (umulh(a0, b0) + a0·b1 + a1·b0)·2⁶⁴.
                    Op::Mul => {
                        self.put(enc::umulh(X4, X0, X2));
                        self.put(enc::madd(X4, X0, X3, X4));
                        self.put(enc::madd(X4, X1, X2, X4));
                        self.put(enc::mul(X0, X0, X2));
                        self.put(enc::mov(X1, X4));
                    }
                    Op::And => {
                        self.put(enc::and(X0, X0, X2));
                        self.put(enc::and(X1, X1, X3));
                    }
                    Op::Or => {
                        self.put(enc::orr(X0, X0, X2));
                        self.put(enc::orr(X1, X1, X3));
                    }
                    _ => {
                        self.put(enc::eor(X0, X0, X2));
                        self.put(enc::eor(X1, X1, X3));
                    }
                }
                self.st2(X0, X1, inst);
            }
            Op::Neg | Op::Not => {
                let a = o.value();
                self.ld2(X0, X1, a);
                if op == Op::Neg {
                    self.put(enc::subs(X0, enc::ZR, X0));
                    self.put(enc::sbc(X1, enc::ZR, X1));
                } else {
                    self.put(enc::mvn(X0, X0));
                    self.put(enc::mvn(X1, X1));
                }
                self.st2(X0, X1, inst);
            }
            Op::Shl | Op::Lshr | Op::Ashr => {
                let (a, b) = (o.value(), o.value());
                self.ld2(X0, X1, a);
                // The amount's low 7 bits (an amount of 128 or more is the walk's to check).
                if Self::is_wide(view.ty(b)) {
                    self.ld_sp(X2, b.0 * 16);
                } else {
                    self.ld(X2, b);
                }
                self.put(enc::and_imm(X2, X2, 1, 0, 6));
                self.put(enc::mvn(X3, X2)); // 63 - n, mod 64
                self.put(enc::cmp_imm(X2, 64));
                match op {
                    Op::Shl => {
                        self.put(enc::lslv(X4, X0, X2)); // lo << n
                        self.put(enc::lslv(X5, X1, X2)); // hi << n
                        self.put(enc::lsr_imm(X6, X0, 1));
                        self.put(enc::lsrv(X6, X6, X3)); // lo >> (64 - n), 0 when n is 0
                        self.put(enc::orr(X5, X5, X6));
                        self.put(enc::csel(X1, X4, X5, Cond::Hs));
                        self.put(enc::csel(X0, enc::ZR, X4, Cond::Hs));
                    }
                    _ => {
                        let arith = op == Op::Ashr;
                        self.put(enc::lsrv(X4, X0, X2)); // lo >> n
                        self.put(if arith { enc::asrv(X5, X1, X2) } else { enc::lsrv(X5, X1, X2) }); // hi >> n
                        self.put(enc::lsl_imm(X6, X1, 1));
                        self.put(enc::lslv(X6, X6, X3)); // hi << (64 - n), 0 when n is 0
                        self.put(enc::orr(X4, X4, X6));
                        let fill = if arith {
                            self.put(enc::asr_imm(X7, X1, 63));
                            X7
                        } else {
                            enc::ZR
                        };
                        self.put(enc::csel(X0, X5, X4, Cond::Hs));
                        self.put(enc::csel(X1, fill, X5, Cond::Hs));
                    }
                }
                self.st2(X0, X1, inst);
            }
            Op::Icmp => {
                let cond = o.int_cond();
                let (a, b) = (o.value(), o.value());
                self.ld2(X0, X1, a);
                self.ld2(X2, X3, b);
                let c = match cond {
                    IntCond::Eq | IntCond::Ne => {
                        self.put(enc::eor(X0, X0, X2));
                        self.put(enc::eor(X1, X1, X3));
                        self.put(enc::orr(X0, X0, X1));
                        self.put(enc::cmp_imm(X0, 0));
                        if cond == IntCond::Eq { Cond::Eq } else { Cond::Ne }
                    }
                    // `a - b` over both words: C and N, V are the 128-bit comparison's.
                    IntCond::Ult | IntCond::Uge | IntCond::Slt | IntCond::Sge => {
                        self.put(enc::cmp(X0, X2));
                        self.put(enc::sbcs(enc::ZR, X1, X3));
                        match cond {
                            IntCond::Ult => Cond::Lo,
                            IntCond::Uge => Cond::Hs,
                            IntCond::Slt => Cond::Lt,
                            _ => Cond::Ge,
                        }
                    }
                    // `a > b` is `b < a`.
                    _ => {
                        self.put(enc::cmp(X2, X0));
                        self.put(enc::sbcs(enc::ZR, X3, X1));
                        match cond {
                            IntCond::Ugt => Cond::Lo,
                            IntCond::Ule => Cond::Hs,
                            IntCond::Sgt => Cond::Lt,
                            _ => Cond::Ge,
                        }
                    }
                };
                self.put(enc::cset(X9, c));
                self.st(X9, inst);
            }
            Op::Zext | Op::Sext | Op::Freeze => {
                let a = o.value();
                if Self::is_wide(view.ty(a)) {
                    if op != Op::Freeze {
                        return Err(unsupported(inst, op, "an extension from `i128` to `i128`"));
                    }
                    self.ld2(X0, X1, a);
                } else {
                    let from = self.width_of(inst, op, a)?;
                    self.ld(X0, a);
                    if op == Op::Sext {
                        self.widen_signed(X0, from);
                        self.put(enc::asr_imm(X1, X0, 63));
                    } else {
                        self.put(enc::mov(X1, enc::ZR));
                    }
                }
                self.st2(X0, X1, inst);
            }
            Op::Trunc => {
                let a = o.value();
                let width = self.result_width(inst, op)?;
                self.ld_sp(X9, a.0 * 16);
                self.narrow(X9, width);
                self.st(X9, inst);
            }
            Op::Load => {
                let (ptr, off) = (o.value(), o.offset());
                self.ld(X2, ptr);
                self.mem(true, Size::X, X0, X2, off);
                self.mem(true, Size::X, X1, X2, off + 8);
                self.st2(X0, X1, inst);
            }
            Op::Store => {
                let (value, ptr, off) = (o.value(), o.value(), o.offset());
                self.ld2(X0, X1, value);
                self.ld(X2, ptr);
                self.mem(false, Size::X, X0, X2, off);
                self.mem(false, Size::X, X1, X2, off + 8);
            }
            Op::Select => {
                let (cond, a, b) = (o.value(), o.value(), o.value());
                self.ld(X9, cond);
                self.ld2(X0, X1, a);
                self.ld2(X2, X3, b);
                self.put(enc::cmp_imm(X9, 0));
                self.put(enc::csel(X0, X0, X2, Cond::Ne));
                self.put(enc::csel(X1, X1, X3, Cond::Ne));
                self.st2(X0, X1, inst);
            }
            Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => {
                return Err(unsupported(
                    inst,
                    op,
                    "`i128` division or remainder (compiler_builtins' `__udivti3`, `__divti3`, `__umodti3`, `__modti3` are not linked on the run path yet)",
                ));
            }
            _ => return Err(unsupported(inst, op, "this `i128` operation (overflow, saturating, rotate, bit count, float conversion)")),
        }
        Ok(())
    }
}
