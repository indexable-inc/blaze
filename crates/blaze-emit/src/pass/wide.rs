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
//! Division and remainder, and conversions to and from floats, are calls of the runtime's
//! routines (pass/runtime.rs: `__udivti3` and kin, `__floattidf` and kin).
//!
//! The overflow operations give a wide pair (lib.rs, "Pairs"): the wrapped result in the
//! cell, the flag in the third word past the cells. The flag: an add's carry or signed
//! overflow out of the high word; a subtract's borrow or signed overflow; an unsigned multiply
//! overflowed when the product's bits from 128 up are not all zero, the standard cross-product
//! test over the 64-bit halves (`a1·b1`, the high halves of `a0·b1` and `a1·b0`, and the
//! carries of the middle sum); a signed multiply is the unsigned one of the magnitudes, the
//! sign put back, and overflowed also when the signed result's sign is not the product's.
//!
//! The saturating adds and subtracts pick their bound by the flags (`wide`); the bit counts and
//! reversals work word by word (`wide_bits`). Refused by name: rotations, min and max, the high
//! multiplies (no reached function asks: the census of 2026-11-17).

use alloc::alloc::Allocator;

use blaze_lir::{IntCond, Op, Scalar, Ty, Value, View, func::NONE};

use super::{EmitError, Pass, unsupported};
use crate::enc::{self, Cond, Reg, Size, X0, X1, X2, X9, X10, X11};

/// Whether `ty` is a wide pair (lib.rs, "Pairs"): `{i128, iN}` with N at most 64, the result
/// of a 128-bit overflow operation.
pub(super) fn is_wide_pair(view: View<'_>, ty: Ty) -> bool {
    if ty.compound_row().is_none() {
        return false;
    }
    let members = view.members(ty);
    let narrow = |t: Ty| t.is_scalar() && matches!(t.elem(), Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64);
    members.len() == 2 && Ty(members[0]) == Ty::I128 && narrow(Ty(members[1]))
}

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
            // The runtime's routines (pass/runtime.rs), after the zero and overflow checks.
            Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => return self.wide_div(inst, op),
            // The flags of the two-word add or subtract pick the bound: `hs` (a carry) for an
            // unsigned add, `lo` (a borrow) for an unsigned subtract, `vs` for a signed one,
            // whose bound has `a`'s sign (a signed sum or difference overflows only toward the
            // side `a` is on: with `m` all ones when `a < 0`, `MAX`'s words are `!m`, `!m ^ 2⁶³`
            // when `a ≥ 0` and `MIN`'s are `0`, `2⁶³` when not, which `!m`, `!m ^ 2⁶³` are too).
            // Nothing between the `adcs`/`sbcs` and the `csel`s sets the flags.
            Op::UaddSat | Op::UsubSat | Op::SaddSat | Op::SsubSat => {
                let (a, b) = (o.value(), o.value());
                self.ld2(X0, X1, a);
                self.ld2(X2, X3, b);
                let (lo, hi, cond) = match op {
                    Op::UaddSat => {
                        self.put(enc::movn(X4, 0, 0));
                        (X4, X4, Cond::Hs)
                    }
                    Op::UsubSat => (enc::ZR, enc::ZR, Cond::Lo),
                    _ => {
                        self.put(enc::asr_imm(X5, X1, 63));
                        self.put(enc::mvn(X6, X5));
                        self.mov_imm(X7, 1 << 63);
                        self.put(enc::eor(X7, X6, X7));
                        (X6, X7, Cond::Vs)
                    }
                };
                match op {
                    Op::UaddSat | Op::SaddSat => {
                        self.put(enc::adds(X0, X0, X2));
                        self.put(enc::adcs(X1, X1, X3));
                    }
                    _ => {
                        self.put(enc::subs(X0, X0, X2));
                        self.put(enc::sbcs(X1, X1, X3));
                    }
                }
                self.put(enc::csel(X0, lo, X0, cond));
                self.put(enc::csel(X1, hi, X1, cond));
                self.st2(X0, X1, inst);
            }
            _ => return Err(unsupported(inst, op, "this `i128` operation (a rotate, a min or max, a high multiply)")),
        }
        Ok(())
    }

    /// `clz`, `ctz`, `popcnt`, `bswap`, `bitrev` of an `i128` (`x0:x1`): the counts from the
    /// two words' (the high word's count when it is not all zeros for `clz`, else 64 plus the
    /// low's; `ctz` the other way round; `popcnt` the sum), stored as the result's type is (an
    /// `i128`'s high word 0); the reversals swap the words and reverse each.
    pub(super) fn wide_bits(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let a = self.view.operands(inst).value();
        self.ld2(X0, X1, a);
        match op {
            Op::Bswap | Op::Bitrev => {
                let each = |rd, rn| match op {
                    Op::Bswap => enc::rev(rd, rn),
                    _ => enc::rbit(rd, rn),
                };
                self.put(each(X2, X1));
                self.put(each(X1, X0));
                self.st2(X2, X1, inst);
                return Ok(());
            }
            Op::Clz => {
                self.put(enc::clz(X2, X1));
                self.put(enc::clz(X3, X0));
                self.put(enc::add_imm(X3, X3, 64, false));
                self.put(enc::cmp_imm(X1, 0));
                self.put(enc::csel(X0, X2, X3, Cond::Ne));
            }
            Op::Ctz => {
                self.put(enc::rbit(X2, X0));
                self.put(enc::clz(X2, X2));
                self.put(enc::rbit(X3, X1));
                self.put(enc::clz(X3, X3));
                self.put(enc::add_imm(X3, X3, 64, false));
                self.put(enc::cmp_imm(X0, 0));
                self.put(enc::csel(X0, X2, X3, Cond::Ne));
            }
            Op::Popcnt => {
                for (word, count) in [(X0, X2), (X1, X3)] {
                    self.put(enc::fmov_to_d(0, word));
                    self.put(enc::cnt_8b(0, 0));
                    self.put(enc::addv_8b(0, 0));
                    self.put(enc::fmov_from_d(count, 0));
                }
                self.put(enc::add(X0, X2, X3));
            }
            _ => return Err(unsupported(inst, op, "an `i128` bit operation that is not a count or a reversal")),
        }
        let result = self.view.ty(Value(inst as u32));
        match Self::is_wide(result) {
            true => self.st2(X0, enc::ZR, inst),
            // At most 128: it fits any integer result of 8 bits or more.
            false => {
                let width = self.result_width(inst, op)?;
                if width < 8 {
                    return Err(unsupported(inst, op, "an `i128` count into an integer narrower than 8 bits"));
                }
                self.st(X0, inst);
            }
        }
        Ok(())
    }

    /// A 128-bit overflow operation: the wrapped result in its cell, the flag (0 or 1) in its
    /// third word.
    pub(super) fn wide_overflow(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let side = self.wide_pair_at[inst];
        if side == NONE {
            return Err(unsupported(inst, op, "a 128-bit overflow operation whose result is not `{i128, i1}`"));
        }
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        if !Self::is_wide(self.view.ty(b)) {
            return Err(unsupported(inst, op, "an overflow operation over an `i128` and a value of another type"));
        }
        self.ld2(X0, X1, a);
        self.ld2(X2, X3, b);
        match op {
            Op::UaddOverflow | Op::SaddOverflow => {
                self.put(enc::adds(X0, X0, X2));
                self.put(enc::adcs(X1, X1, X3));
                self.put(enc::cset(X9, if op == Op::UaddOverflow { Cond::Hs } else { Cond::Vs }));
            }
            Op::UsubOverflow | Op::SsubOverflow => {
                self.put(enc::subs(X0, X0, X2));
                self.put(enc::sbcs(X1, X1, X3));
                self.put(enc::cset(X9, if op == Op::UsubOverflow { Cond::Lo } else { Cond::Vs }));
            }
            Op::UmulOverflow => self.umul_wide(),
            Op::SmulOverflow => {
                // x7: all ones when the product is negative. Each operand becomes its
                // magnitude (`(v ^ m) - m`; `MIN`'s is 2¹²⁷, which fits unsigned).
                self.put(enc::asr_imm(X7, X1, 63));
                self.negate_if(X0, X1, X7);
                self.put(enc::asr_imm(X10, X3, 63));
                self.negate_if(X2, X3, X10);
                self.put(enc::eor(X7, X7, X10));
                self.umul_wide();
                // a·b ≡ ±|a|·|b| (mod 2¹²⁸).
                self.negate_if(X0, X1, X7);
                // Within 128 unsigned bits it still overflowed when the result's sign is not the
                // product's, unless the result is 0 (whose sign is both).
                self.put(enc::eor(X10, X1, X7));
                self.put(enc::lsr_imm(X10, X10, 63));
                self.put(enc::orr(X11, X0, X1));
                self.put(enc::cmp_imm(X11, 0));
                self.put(enc::cset(X11, Cond::Ne));
                self.put(enc::and(X10, X10, X11));
                self.put(enc::orr(X9, X9, X10));
            }
            _ => return Err(unsupported(inst, op, "an `i128` operation that is not an overflow operation")),
        }
        self.st2(X0, X1, inst);
        self.st_sp(X9, side);
        Ok(())
    }

    /// `lo:hi = (lo:hi ^ m) - m` over both words, `m` 0 or all ones: the negation when `m` is.
    fn negate_if(&mut self, lo: Reg, hi: Reg, m: Reg) {
        self.put(enc::eor(lo, lo, m));
        self.put(enc::eor(hi, hi, m));
        self.put(enc::subs(lo, lo, m));
        self.put(enc::sbc(hi, hi, m));
    }

    /// `x0:x1 = (x0:x1 · x2:x3) mod 2¹²⁸`, `x9` = whether the product reached 2¹²⁸. Takes
    /// `x4`, `x5`, `x6`, `x10`, `x11`; keeps `x7`.
    ///
    /// (a1·2⁶⁴ + a0)(b1·2⁶⁴ + b0) = a1·b1·2¹²⁸ + (a0·b1 + a1·b0 + umulh(a0, b0))·2⁶⁴ + (a0·b0
    /// mod 2⁶⁴): it fits 128 bits exactly when `a1·b1` is 0 (not both high halves non-zero),
    /// neither cross product reaches 2⁶⁴ (`umulh` of each is 0) and the middle sum of three
    /// 64-bit words does not carry.
    fn umul_wide(&mut self) {
        self.put(enc::umulh(X4, X0, X2));
        self.put(enc::mul(X5, X0, X3));
        self.put(enc::mul(X6, X1, X2));
        self.put(enc::adds(X4, X4, X5));
        self.put(enc::cset(X9, Cond::Hs));
        self.put(enc::adds(X4, X4, X6));
        self.put(enc::cset(X10, Cond::Hs));
        self.put(enc::orr(X9, X9, X10));
        self.put(enc::umulh(X5, X0, X3));
        self.put(enc::umulh(X6, X1, X2));
        self.put(enc::orr(X5, X5, X6));
        self.put(enc::cmp_imm(X5, 0));
        self.put(enc::cset(X10, Cond::Ne));
        self.put(enc::orr(X9, X9, X10));
        self.put(enc::cmp_imm(X1, 0));
        self.put(enc::cset(X10, Cond::Ne));
        self.put(enc::cmp_imm(X3, 0));
        self.put(enc::cset(X11, Cond::Ne));
        self.put(enc::and(X10, X10, X11));
        self.put(enc::orr(X9, X9, X10));
        self.put(enc::mul(X0, X0, X2));
        self.put(enc::mov(X1, X4));
    }

    /// `project` of a wide pair: member 0 the `i128` in its cell, member 1 the narrow integer
    /// in its third word.
    pub(super) fn wide_project(&mut self, inst: usize, pair: Value, k: u32) -> Result<(), EmitError> {
        let result = self.view.ty(Value(inst as u32));
        match k {
            0 if Self::is_wide(result) => {
                self.ld2(X0, X1, pair);
                self.st2(X0, X1, inst);
            }
            1 if self.width(result).is_some() => {
                self.ld_sp(X9, self.wide_pair_at[pair.index()]);
                self.st(X9, inst);
            }
            _ => {
                return Err(unsupported(
                    inst,
                    Op::Project,
                    "a member of an `{i128, iN}` pair other than its `i128` (0) or its narrow integer (1)",
                ));
            }
        }
        Ok(())
    }
}
