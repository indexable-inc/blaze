//! Atomics and fences, by the mapping in lib.rs ("Atomics"): `ldar`/`stlr` for ordered loads
//! and stores, the LSE atomics when the core has them ([`crate::Isa::lse`]), else (and for
//! `nand`) an exclusive load and store retried until the store keeps its reservation.

use std::alloc::Allocator;

use blaze_lir::{AtomicRmw, Op, Ordering, Value};

use super::{EmitError, Pass, size_of_width, unsupported};
use crate::enc::{self, Barrier, Cond, Lse, Size, X9, X10, X11, X12, X13};

/// Whether an ordering acquires and whether it releases (`SeqCst` both: the acquire-release
/// forms are RCsc on Armv8).
const fn sides(ordering: Ordering) -> (bool, bool) {
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

    /// The access size of an atomic of `value`'s type: 8, 16, 32 or 64 bits.
    fn atomic_size(&self, inst: usize, op: Op, value: Value) -> Result<Size, EmitError> {
        let width = self.width_of(inst, op, value)?;
        size_of_width(width).ok_or(unsupported(inst, op, "an atomic of an `i1` (memory holds whole bytes)"))
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
