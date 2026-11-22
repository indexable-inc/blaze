//! Atomics and fences, by the mapping in lib.rs ("Atomics"): `ldar`/`stlr` for ordered loads
//! and stores, the LSE atomics when the core has them ([`crate::Isa::lse`]), else (and for
//! `nand`) an exclusive load and store retried until the store keeps its reservation.

use alloc::alloc::Allocator;

use blaze_lir::{AtomicRmw, Op, Ordering, Value, func::NONE};

use super::{EmitError, Pass, size_of_width, unsupported};
use crate::enc::{self, Barrier, Cond, Lse, Reg, Size, X0, X1, X2, X9, X10, X11, X12, X13};

const X3: Reg = 3;
const X4: Reg = 4;
const X5: Reg = 5;
/// Register 31 as a data-processing destination: the zero register (the result dropped).
const XZR: Reg = 31;

#[cfg(test)]
std::thread_local! {
    /// The control of atomic_tests_wide.rs (rule 23): on this thread, an `i128` atomic takes
    /// the path it took before [`Pass::atomic_wide`] existed, and is refused by name there.
    pub(crate) static WIDE_OFF: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Whether an ordering acquires and whether it releases (`SeqCst` both: the acquire-release
/// forms are RCsc on Armv8). The Fast tier's atomics read the same map (compose/emit.rs
/// `Composer::atomic`).
pub(crate) const fn sides(ordering: Ordering) -> (bool, bool) {
    match ordering {
        Ordering::Relaxed => (false, false),
        Ordering::Acquire => (true, false),
        Ordering::Release => (false, true),
        Ordering::AcqRel | Ordering::SeqCst => (true, true),
    }
}

impl<A: Allocator> Pass<'_, '_, A> {
    pub(super) fn atomic(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let flags = self.view.flags(inst);
        let ordering = flags.ordering().ok_or(unsupported(inst, op, "an ordering that is none of the five"))?;
        let (acq, rel) = sides(ordering);
        if op != Op::Fence && self.atomic_is_wide(inst, op) {
            return self.atomic_wide(inst, op, acq, rel);
        }
        let mut o = self.view.operands(inst);
        match op {
            Op::Fence => {
                // A single-thread fence (`compiler_fence`) needs no instruction: this tier
                // keeps every access in program order, one instruction each.
                if !flags.contains(blaze_lir::Flags::SINGLE_THREAD) {
                    self.put(enc::dmb(match ordering {
                        Ordering::Acquire => Barrier::IshLd,
                        _ => Barrier::Ish,
                    }));
                }
            }
            Op::AtomicLoad => {
                let size = self.atomic_size(inst, op, Value(inst as u32))?;
                self.ld(X9, o.value());
                self.put(match acq {
                    true => enc::ldar(size, X9, X9),
                    false => enc::ldr(size, X9, X9, 0),
                });
                self.st(X9, inst);
            }
            Op::AtomicStore => {
                let (value, ptr) = (o.value(), o.value());
                let size = self.atomic_size(inst, op, value)?;
                self.ld(X9, value);
                self.ld(X10, ptr);
                self.put(match rel {
                    true => enc::stlr(size, X9, X10),
                    false => enc::str(size, X9, X10, 0),
                });
            }
            Op::AtomicRmw => {
                let (rmw, ptr, value) = (o.rmw(), o.value(), o.value());
                let size = self.atomic_size(inst, op, value)?;
                let width = self.width_of(inst, op, value)?;
                self.ld(X9, ptr);
                self.ld(X10, value);
                match (self.isa.lse, lse_of(rmw)) {
                    (true, Some(form)) => {
                        // `sub` adds the negation, `and` clears the complement's bits.
                        match rmw {
                            AtomicRmw::Sub => self.put(enc::neg(X10, X10)),
                            AtomicRmw::And => self.put(enc::mvn(X10, X10)),
                            _ => {}
                        }
                        self.put(enc::lse(form, size, acq, rel, X10, X11, X9));
                    }
                    _ => self.rmw_loop(rmw, size, width, acq, rel)?,
                }
                self.st(X11, inst);
            }
            _ => {
                let (ptr, expected, new) = (o.value(), o.value(), o.value());
                let size = self.atomic_size(inst, op, expected)?;
                if !self.is_pair(self.view.ty(Value(inst as u32))) {
                    return Err(unsupported(inst, op, "a cmpxchg whose result is not a pair"));
                }
                let failure = flags.failure().ok_or(unsupported(inst, op, "a failure ordering that is none of the five"))?;
                // The acquire part from the stronger of success and failure.
                let acq = acq || sides(failure).0;
                self.ld(X9, ptr);
                self.ld(X10, expected);
                self.ld(X11, new);
                match self.isa.lse {
                    true => {
                        self.put(enc::mov(X12, X10));
                        self.put(enc::cas(size, acq, rel, X12, X11, X9));
                    }
                    false => {
                        // 0: ld{a}xr x12, [x9]; cmp x12, x10; b.ne fail; st{l}xr w13, x11, [x9];
                        //    cbnz w13, 0; b done; fail: clrex; done:
                        let top = self.here();
                        self.put(enc::ldxr(size, acq, X12, X9));
                        self.put(enc::cmp(X12, X10));
                        let fail = self.here();
                        self.put(enc::b_cond(Cond::Ne, 0));
                        self.put(enc::stxr(size, rel, X13, X11, X9));
                        self.back(enc::cbnz(X13, 0), top)?;
                        self.put(enc::b(2));
                        self.land(fail)?;
                        self.put(enc::clrex());
                    }
                }
                // x12 holds the old value zero-extended from its size, as x10 is.
                self.put(enc::cmp(X12, X10));
                self.put(enc::cset(X13, Cond::Eq));
                self.st(X12, inst);
                self.st_sp(X13, inst as u32 * 16 + 8);
            }
        }
        Ok(())
    }

    /// Whether the atomic at `inst` is of an `i128` (its loaded, stored, combined or compared
    /// value): [`Self::atomic_wide`]'s.
    fn atomic_is_wide(&self, inst: usize, op: Op) -> bool {
        #[cfg(test)]
        if WIDE_OFF.with(core::cell::Cell::get) {
            return false;
        }
        let mut o = self.view.operands(inst);
        let value = match op {
            Op::AtomicLoad => Value(inst as u32),
            Op::AtomicStore => o.value(),
            Op::AtomicRmw => {
                let _ = (o.rmw(), o.value());
                o.value()
            }
            _ => {
                let _ = o.value();
                o.value()
            }
        };
        Self::is_wide(self.view.ty(value))
    }

    /// An atomic of an `i128` (lib.rs, "Atomics"; the value's low word at 0 of its cell, the
    /// high at 8: pass/wide.rs). Armv8.0's only 16-byte single-copy-atomic access is a
    /// store-exclusive pair that succeeds (Arm ARM B2.2.1: "LDXP ... is single-copy atomic
    /// only if the STXP ... succeeds"), so every form is one exclusive loop: a load stores the
    /// pair it read back; a store reads the pair first to hold the reservation; an `rmw`
    /// stores `old op value`. A `cmpxchg` is one `casp` with FEAT_LSE ([`crate::Isa::lse`],
    /// which the caller states for the target: every Apple M-series core has it), else the loop,
    /// which on a mismatch stores the old pair back (so the value it answers was read
    /// atomically) and keeps the reservation's answer. No runtime probe: the target's features
    /// are the caller's `Isa`. The pointer is the access's own (16-aligned: `AtomicU128`'s
    /// alignment; a misaligned one faults, as rustc's code does).
    ///
    /// Registers: `x9` the pointer, `x0:x1` the operand (`value`, or `expected`), `x2:x3` the
    /// old pair, `x4:x5` the pair stored, `w13` the store's status. A body's registers hold
    /// nothing across an instruction (every value lives in its cell), so all are free here.
    fn atomic_wide(&mut self, inst: usize, op: Op, acq: bool, rel: bool) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        match op {
            Op::AtomicLoad => {
                // 0: ld{a}xp x2, x3, [x9]; st{l}xp w13, x2, x3, [x9]; cbnz w13, 0
                self.ld(X9, o.value());
                let top = self.here();
                self.put(enc::ldxp(acq, X2, X3, X9));
                self.put(enc::stxp(rel, X13, X2, X3, X9));
                self.back(enc::cbnz(X13, 0), top)?;
                self.st2(X2, X3, inst);
            }
            Op::AtomicStore => {
                // 0: ld{a}xp x2, x3, [x9]; st{l}xp w13, x0, x1, [x9]; cbnz w13, 0
                let (value, ptr) = (o.value(), o.value());
                self.ld2(X0, X1, value);
                self.ld(X9, ptr);
                let top = self.here();
                self.put(enc::ldxp(acq, X2, X3, X9));
                self.put(enc::stxp(rel, X13, X0, X1, X9));
                self.back(enc::cbnz(X13, 0), top)?;
            }
            Op::AtomicRmw => {
                let (rmw, ptr, value) = (o.rmw(), o.value(), o.value());
                self.ld(X9, ptr);
                self.ld2(X0, X1, value);
                // 0: ld{a}xp x2, x3, [x9]; x4:x5 = old rmw x0:x1; st{l}xp w13, x4, x5, [x9];
                //    cbnz w13, 0
                let top = self.here();
                self.put(enc::ldxp(acq, X2, X3, X9));
                let (lo, hi) = self.rmw_wide(rmw);
                self.put(enc::stxp(rel, X13, lo, hi, X9));
                self.back(enc::cbnz(X13, 0), top)?;
                self.st2(X2, X3, inst);
            }
            _ => {
                let side = self.wide_pair_at[inst];
                if side == NONE {
                    return Err(unsupported(inst, op, "a 128-bit cmpxchg whose result is not `{i128, i1}`"));
                }
                let flags = self.view.flags(inst);
                let failure = flags.failure().ok_or(unsupported(inst, op, "a failure ordering that is none of the five"))?;
                let acq = acq || sides(failure).0;
                let (ptr, expected, new) = (o.value(), o.value(), o.value());
                self.ld(X9, ptr);
                self.ld2(X0, X1, expected);
                self.ld2(X4, X5, new);
                match self.isa.lse {
                    true => {
                        // casp{a}{l} x2, x3, x4, x5, [x9]: x2:x3 in as `expected`, out as old.
                        self.put(enc::mov(X2, X0));
                        self.put(enc::mov(X3, X1));
                        self.put(enc::casp(acq, rel, X2, X4, X9));
                    }
                    false => {
                        // 0: ld{a}xp x2, x3, [x9]; cmp x2, x0; b.ne fail; cmp x3, x1; b.ne fail;
                        //    st{l}xp w13, x4, x5, [x9]; cbnz w13, 0; b done;
                        // fail: st{l}xp w13, x2, x3, [x9]; cbnz w13, 0; done:
                        let top = self.here();
                        self.put(enc::ldxp(acq, X2, X3, X9));
                        self.put(enc::cmp(X2, X0));
                        let fail_lo = self.here();
                        self.put(enc::b_cond(Cond::Ne, 0));
                        self.put(enc::cmp(X3, X1));
                        let fail_hi = self.here();
                        self.put(enc::b_cond(Cond::Ne, 0));
                        self.put(enc::stxp(rel, X13, X4, X5, X9));
                        self.back(enc::cbnz(X13, 0), top)?;
                        self.put(enc::b(3));
                        self.land(fail_lo)?;
                        self.land(fail_hi)?;
                        self.put(enc::stxp(rel, X13, X2, X3, X9));
                        self.back(enc::cbnz(X13, 0), top)?;
                    }
                }
                // Succeeded when the old pair is `expected`: (x2 ^ x0) | (x3 ^ x1) == 0.
                self.put(enc::eor(X10, X2, X0));
                self.put(enc::eor(X11, X3, X1));
                self.put(enc::orr(X10, X10, X11));
                self.put(enc::cmp_imm(X10, 0));
                self.put(enc::cset(X13, Cond::Eq));
                self.st2(X2, X3, inst);
                self.st_sp(X13, side);
            }
        }
        Ok(())
    }

    /// `old rmw x0:x1` of the old pair in `x2:x3`, into the registers it answers (`x4:x5`, or
    /// `x0:x1` for `xchg`). Max and min compare as one 128-bit subtraction (`cmp` low, `sbcs`
    /// high: N, V and C are the whole difference's; Z is not, so only `ge`, `lt`, `hs`, `lo`
    /// are read) and keep the old pair where it wins (an equal pair is the same either way).
    fn rmw_wide(&mut self, rmw: AtomicRmw) -> (Reg, Reg) {
        match rmw {
            AtomicRmw::Xchg => return (X0, X1),
            AtomicRmw::Add => {
                self.put(enc::adds(X4, X2, X0));
                self.put(enc::adc(X5, X3, X1));
            }
            AtomicRmw::Sub => {
                self.put(enc::subs(X4, X2, X0));
                self.put(enc::sbc(X5, X3, X1));
            }
            AtomicRmw::And => {
                self.put(enc::and(X4, X2, X0));
                self.put(enc::and(X5, X3, X1));
            }
            AtomicRmw::Nand => {
                self.put(enc::and(X4, X2, X0));
                self.put(enc::and(X5, X3, X1));
                self.put(enc::mvn(X4, X4));
                self.put(enc::mvn(X5, X5));
            }
            AtomicRmw::Or => {
                self.put(enc::orr(X4, X2, X0));
                self.put(enc::orr(X5, X3, X1));
            }
            AtomicRmw::Xor => {
                self.put(enc::eor(X4, X2, X0));
                self.put(enc::eor(X5, X3, X1));
            }
            AtomicRmw::Max | AtomicRmw::Min | AtomicRmw::Umax | AtomicRmw::Umin => {
                let keep_old = match rmw {
                    AtomicRmw::Max => Cond::Ge,
                    AtomicRmw::Min => Cond::Lt,
                    AtomicRmw::Umax => Cond::Hs,
                    _ => Cond::Lo,
                };
                self.put(enc::cmp(X2, X0));
                self.put(enc::sbcs(XZR, X3, X1));
                self.put(enc::csel(X4, X2, X0, keep_old));
                self.put(enc::csel(X5, X3, X1, keep_old));
            }
        }
        (X4, X5)
    }

    /// The access size of an atomic of `value`'s type: 8, 16, 32 or 64 bits.
    fn atomic_size(&self, inst: usize, op: Op, value: Value) -> Result<Size, EmitError> {
        let width = self.width_of(inst, op, value)?;
        size_of_width(width).ok_or(unsupported(
            inst,
            op,
            match width {
                1 => "an atomic of an `i1` (memory holds whole bytes)",
                128 => "an atomic of 128 bits that is not an `i128` (`atomic_wide` takes `i128` alone)",
                _ => "an atomic of a width that is not 8, 16, 32 or 64 bits",
            },
        ))
    }

    /// `x11 = ` the old value of `*x9`, `*x9 = old rmw x10`, by an exclusive load and store
    /// retried until the store keeps its reservation.
    fn rmw_loop(&mut self, rmw: AtomicRmw, size: Size, width: u32, acq: bool, rel: bool) -> Result<(), EmitError> {
        let signed = matches!(rmw, AtomicRmw::Max | AtomicRmw::Min);
        if signed {
            self.widen_signed(X10, width);
        }
        let top = self.here();
        self.put(enc::ldxr(size, acq, X11, X9));
        let stored = match rmw {
            AtomicRmw::Xchg => X10,
            AtomicRmw::Add => {
                self.put(enc::add(X12, X11, X10));
                X12
            }
            AtomicRmw::Sub => {
                self.put(enc::sub(X12, X11, X10));
                X12
            }
            AtomicRmw::And => {
                self.put(enc::and(X12, X11, X10));
                X12
            }
            AtomicRmw::Nand => {
                self.put(enc::and(X12, X11, X10));
                self.put(enc::mvn(X12, X12));
                X12
            }
            AtomicRmw::Or => {
                self.put(enc::orr(X12, X11, X10));
                X12
            }
            AtomicRmw::Xor => {
                self.put(enc::eor(X12, X11, X10));
                X12
            }
            AtomicRmw::Max | AtomicRmw::Min | AtomicRmw::Umax | AtomicRmw::Umin => {
                // The old value as the comparison reads it: sign-extended for a signed one.
                let old = match signed && width < 64 {
                    true => {
                        self.put(enc::sext(X12, X11, width));
                        X12
                    }
                    false => X11,
                };
                let keep = match rmw {
                    AtomicRmw::Max => Cond::Gt,
                    AtomicRmw::Min => Cond::Lt,
                    AtomicRmw::Umax => Cond::Hi,
                    _ => Cond::Lo,
                };
                self.put(enc::cmp(old, X10));
                self.put(enc::csel(X12, old, X10, keep));
                X12
            }
        };
        self.put(enc::stxr(size, rel, X13, stored, X9));
        self.back(enc::cbnz(X13, 0), top)
    }

    /// `word` (a branch with an `Imm19` field) pointed back at `to`.
    fn back(&mut self, word: u32, to: u32) -> Result<(), EmitError> {
        let at = self.here();
        let word = enc::Field::Imm19
            .patch(word, i64::from(to) - i64::from(at))
            .ok_or(EmitError::TooLarge { what: "a branch inside one instruction's code (1 MiB)" })?;
        self.put(word);
        Ok(())
    }
}

/// The LSE form of an `atomic_rmw`, if it has one (`nand` has none).
const fn lse_of(rmw: AtomicRmw) -> Option<Lse> {
    match rmw {
        AtomicRmw::Xchg => Some(Lse::Swp),
        AtomicRmw::Add | AtomicRmw::Sub => Some(Lse::Add),
        AtomicRmw::And => Some(Lse::Clr),
        AtomicRmw::Or => Some(Lse::Set),
        AtomicRmw::Xor => Some(Lse::Eor),
        AtomicRmw::Max => Some(Lse::Smax),
        AtomicRmw::Min => Some(Lse::Smin),
        AtomicRmw::Umax => Some(Lse::Umax),
        AtomicRmw::Umin => Some(Lse::Umin),
        AtomicRmw::Nand => None,
    }
}
