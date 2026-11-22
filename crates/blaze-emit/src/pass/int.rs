//! The integer operations past the plain arithmetic: rotations, high multiplies, minimum and
//! maximum, saturating arithmetic, the overflow operations (a pair: lib.rs, "Pairs") and the
//! bit operations. Each loads its operands (zero-extended, the cell invariant), computes in
//! whole 64-bit registers and stores a result zero-extended to its width.

use alloc::alloc::Allocator;

use blaze_lir::{Op, Value};

use super::{EmitError, Pass, mask, unsupported};
use crate::enc::{self, Cond, X9, X10, X11, X12, ZR};

impl<A: Allocator> Pass<'_, '_, A> {
    /// `rotl`, `rotr`, `umulhi`, `smulhi`, `umin`, `umax`, `smin`, `smax` and the saturating
    /// adds and subtracts.
    pub(super) fn int_binary(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let width = self.result_width(inst, op)?;
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        self.ld(X9, a);
        self.ld(X10, b);
        let signed = matches!(op, Op::Smulhi | Op::Smin | Op::Smax | Op::SaddSat | Op::SsubSat);
        if signed {
            self.widen_signed(X9, width);
            self.widen_signed(X10, width);
        }
        match op {
            Op::Rotl | Op::Rotr => self.rotate(op, width),
            Op::Umulhi | Op::Smulhi => match width {
                64 => self.put(match op {
                    Op::Umulhi => enc::umulh(X9, X9, X10),
                    _ => enc::smulh(X9, X9, X10),
                }),
                // The whole product fits 64 bits; its high half is a shift away.
                _ => {
                    self.put(enc::mul(X9, X9, X10));
                    self.put(match op {
                        Op::Umulhi => enc::lsr_imm(X9, X9, width),
                        _ => enc::asr_imm(X9, X9, width),
                    });
                }
            },
            Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let cond = match op {
                    Op::Umin => Cond::Lo,
                    Op::Umax => Cond::Hi,
                    Op::Smin => Cond::Lt,
                    _ => Cond::Gt,
                };
                self.put(enc::cmp(X9, X10));
                self.put(enc::csel(X9, X9, X10, cond));
            }
            Op::UsubSat => {
                self.put(enc::subs(X9, X9, X10));
                self.put(enc::csel(X9, ZR, X9, Cond::Lo));
            }
            Op::UaddSat => match width {
                64 => {
                    self.put(enc::adds(X9, X9, X10));
                    self.put(enc::movn(X11, 0, 0));
                    self.put(enc::csel(X9, X11, X9, Cond::Hs));
                }
                _ => {
                    self.put(enc::add(X9, X9, X10));
                    self.mov_imm(X11, mask(width));
                    self.put(enc::cmp(X9, X11));
                    self.put(enc::csel(X9, X11, X9, Cond::Hi));
                }
            },
            _ => match width {
                // On a signed overflow the wrapped sum's sign is the true one's opposite: the
                // bound is MAX when it reads negative, MIN when positive.
                64 => {
                    self.put(match op {
                        Op::SaddSat => enc::adds(X9, X9, X10),
                        _ => enc::subs(X9, X9, X10),
                    });
                    self.put(enc::asr_imm(X11, X9, 63));
                    self.mov_imm(X12, 1 << 63);
                    self.put(enc::eor(X11, X11, X12));
                    self.put(enc::csel(X9, X11, X9, Cond::Vs));
                }
                // The exact result fits 64 bits: clamp it to the width's range.
                _ => {
                    self.put(match op {
                        Op::SaddSat => enc::add(X9, X9, X10),
                        _ => enc::sub(X9, X9, X10),
                    });
                    let max = (1u64 << (width - 1)) - 1;
                    self.mov_imm(X11, max);
                    self.put(enc::cmp(X9, X11));
                    self.put(enc::csel(X9, X11, X9, Cond::Gt));
                    self.mov_imm(X11, !max);
                    self.put(enc::cmp(X9, X11));
                    self.put(enc::csel(X9, X11, X9, Cond::Lt));
                }
            },
        }
        self.narrow(X9, width);
        self.st(X9, inst);
        Ok(())
    }

    /// `x9 = rotl/rotr(x9, x10)` at `width`, the amount taken modulo the width.
    fn rotate(&mut self, op: Op, width: u32) {
        match width {
            // A rotation of one bit is the bit.
            1 => {}
            32 | 64 => {
                // `rotl` by n is `rotr` by -n: `rorv` takes the amount modulo the width, and the
                // low 5 bits of the 64-bit negation are those of the 32-bit one.
                if op == Op::Rotl {
                    self.put(enc::neg(X10, X10));
                }
                self.put(match width {
                    32 => enc::rorv_w(X9, X9, X10),
                    _ => enc::rorv(X9, X9, X10),
                });
            }
            // 8 and 16: `(a << n) | (a >> (width - n))`, n masked; at n = 0 the second shift is
            // by the width, which leaves nothing of a value narrower than 64 bits.
            _ => {
                self.put(enc::zext(X10, X10, width.trailing_zeros()));
                self.mov_imm(X12, u64::from(width));
                self.put(enc::sub(X12, X12, X10));
                match op {
                    Op::Rotl => {
                        self.put(enc::lslv(X11, X9, X10));
                        self.put(enc::lsrv(X12, X9, X12));
                    }
                    _ => {
                        self.put(enc::lsrv(X11, X9, X10));
                        self.put(enc::lslv(X12, X9, X12));
                    }
                }
                self.put(enc::orr(X9, X11, X12));
            }
        }
    }

    /// An overflow operation: the wrapped result at cell +0, whether it overflowed (Rust's
    /// `overflowing_*`) at cell +8.
    pub(super) fn overflow(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        if Self::is_wide(self.view.ty(a)) {
            return self.wide_overflow(inst, op);
        }
        let width = self.width_of(inst, op, a)?;
        if !self.is_pair(self.view.ty(Value(inst as u32))) {
            return Err(unsupported(inst, op, "an overflow operation whose result is not a pair"));
        }
        self.ld(X9, a);
        self.ld(X10, b);
        let signed = matches!(op, Op::SaddOverflow | Op::SsubOverflow | Op::SmulOverflow);
        if signed {
            self.widen_signed(X9, width);
            self.widen_signed(X10, width);
        }
        // x11: the result's 64 bits; x12: the flag.
        match (op, width) {
            (Op::UaddOverflow, 64) => {
                self.put(enc::adds(X11, X9, X10));
                self.put(enc::cset(X12, Cond::Hs));
            }
            (Op::SaddOverflow, 64) => {
                self.put(enc::adds(X11, X9, X10));
                self.put(enc::cset(X12, Cond::Vs));
            }
            (Op::UsubOverflow, _) => {
                self.put(enc::subs(X11, X9, X10));
                self.put(enc::cset(X12, Cond::Lo));
            }
            (Op::SsubOverflow, 64) => {
                self.put(enc::subs(X11, X9, X10));
                self.put(enc::cset(X12, Cond::Vs));
            }
            (Op::UmulOverflow, 64) => {
                self.put(enc::mul(X11, X9, X10));
                self.put(enc::umulh(X12, X9, X10));
                self.put(enc::cmp_imm(X12, 0));
                self.put(enc::cset(X12, Cond::Ne));
            }
            (Op::SmulOverflow, 64) => {
                // Overflowed unless the high half is the low half's sign.
                self.put(enc::mul(X11, X9, X10));
                self.put(enc::smulh(X12, X9, X10));
                self.put(enc::asr_imm(X10, X11, 63));
                self.put(enc::cmp(X12, X10));
                self.put(enc::cset(X12, Cond::Ne));
            }
            // Below 64 bits the exact result fits 64 (a product of two 32-bit values too): it
            // overflowed when it differs from itself extended back from the width.
            _ => {
                self.put(match op {
                    Op::UaddOverflow | Op::SaddOverflow => enc::add(X11, X9, X10),
                    Op::SsubOverflow => enc::sub(X11, X9, X10),
                    _ => enc::mul(X11, X9, X10),
                });
                match signed {
                    true => self.put(enc::sext(X12, X11, width)),
                    false => self.put(enc::zext(X12, X11, width)),
                }
                self.put(enc::cmp(X11, X12));
                self.put(enc::cset(X12, Cond::Ne));
            }
        }
        self.narrow(X11, width);
        self.st(X11, inst);
        self.st_sp(X12, inst as u32 * 16 + 8);
        Ok(())
    }

    /// `clz`, `ctz`, `popcnt`, `bswap`, `bitrev`.
    pub(super) fn bits(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let width = self.result_width(inst, op)?;
        let a = self.view.operands(inst).value();
        self.ld(X9, a);
        match op {
            // The 64-bit count over a zero-extended value counts 64 - width zeros too many.
            Op::Clz => {
                self.put(enc::clz(X9, X9));
                if width < 64 {
                    self.put(enc::sub_imm(X9, X9, 64 - width, false));
                }
            }
            // Bit `width` set first, so a zero input counts `width`.
            Op::Ctz => {
                if width < 64 {
                    self.put(enc::orr_bit(X9, X9, width));
                }
                self.put(enc::rbit(X9, X9));
                self.put(enc::clz(X9, X9));
            }
            Op::Popcnt => {
                self.put(enc::fmov_to_d(0, X9));
                self.put(enc::cnt_8b(0, 0));
                self.put(enc::addv_8b(0, 0));
                self.put(enc::fmov_from_d(X9, 0));
            }
            Op::Bswap => match width {
                // A value of one byte or less is its own byte swap.
                1 | 8 => {}
                16 => self.put(enc::rev16_w(X9, X9)),
                32 => self.put(enc::rev_w(X9, X9)),
                _ => self.put(enc::rev(X9, X9)),
            },
            _ => {
                self.put(enc::rbit(X9, X9));
                if width < 64 {
                    self.put(enc::lsr_imm(X9, X9, 64 - width));
                }
            }
        }
        self.st(X9, inst);
        Ok(())
    }
}
