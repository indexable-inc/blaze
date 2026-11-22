//! The arm64 recipes: every template the Fast tier's composer looks up (mod.rs says the operand
//! conventions), each one or more calls of enc.rs's encoders with the operands' values. No bit
//! position is written here: [`super::discover`] finds every field.
//!
//! The code of each is the baseline tier's for the same operation (pass.rs), with operands in
//! registers instead of cells: results narrowed to their width (`zext`), operands sign-extended
//! where the sign is read, division by zero `brk #0xff00`, `sdiv` overflow `brk #0xff01`. A
//! template writes its result only after it has read every input (so the composer may give the
//! result an input's register), keeping what it computes before that in TEMP0/TEMP1.

use blaze_code::record::SiteKind;
use blaze_lir::{IntCond, Op};
use blaze_tier::{ARM64, Form, ImmClass, Inner, Pattern, Reg, RegClass, RegSet};

use super::{Code, GenError, RES, Recipe, SiteAt, Slot, SlotKind, T0, T1, Vals};
use crate::enc::{self, Cond, Size, X16, ZR};
use crate::pass::{BRK_DIV_OVERFLOW, BRK_DIV_ZERO, BRK_UNREACHABLE, cond_of, is_signed};

/// The composer's own instructions and the single-word forms added beside these (frame steps,
/// moves, float cells, `ret`, the poll, `xor` immediates, shifted operands, `cbz`).
#[path = "arm64_glue.rs"]
pub(crate) mod glue_recipes;

/// Shifts and divisions of 1 bit, and `freeze`, which the tier equality harness found missing.
#[path = "arm64_one_bit.rs"]
pub(crate) mod one_bit_recipes;

type Each<'e> = dyn FnMut(&Recipe<'_>) -> Result<(), GenError> + 'e;

const NONE: Form = Form::None;
const GPR: Form = Form::Reg(RegClass::Gpr);
const FPR: Form = Form::Reg(RegClass::Fpr);
const BLOCK: Form = Form::Block;
const SLOT: Form = Form::Slot;

/// What a call does not keep: every register but the callee-saved ones and `sp`.
const CALL_CLOBBERS: RegSet = RegSet(!ARM64.callee_saved.0).without(Reg(31));

/// Integer widths of the operations that only need the low bits right.
const WIDTHS: [u8; 5] = [1, 8, 16, 32, 64];
/// Widths of operations that read memory sizes or need a power of two above 1.
const SIZED: [u8; 4] = [8, 16, 32, 64];
/// Memory accesses, the whole grid the composer's `load`/`store` ask for (compose/emit.rs
/// `class_of`: an integer or pointer of 8 to 64 bits in a general register, an `f32` or `f64` in
/// a float one): width, the value's register class, size, the scaled offset's class and its
/// scale. No `i1`: memory holds whole bytes, and an `i1` access is refused by that rule in every
/// tier (pass.rs "a store of an `i1`"; compose/emit.rs `access_width`) and in the evaluator.
const ACCESSES: [(u8, RegClass, Size, ImmClass, u8); 6] = [
    (8, RegClass::Gpr, Size::B, ImmClass::U12Scaled0, 0),
    (16, RegClass::Gpr, Size::H, ImmClass::U12Scaled1, 1),
    (32, RegClass::Gpr, Size::W, ImmClass::U12Scaled2, 2),
    (64, RegClass::Gpr, Size::X, ImmClass::U12Scaled3, 3),
    (32, RegClass::Fpr, Size::W, ImmClass::U12Scaled2, 2),
    (64, RegClass::Fpr, Size::X, ImmClass::U12Scaled3, 3),
];

const fn reg(operand: u8) -> Slot {
    Slot { operand, kind: SlotKind::Reg, bits: 5 }
}

const fn imm(operand: u8, bits: u8) -> Slot {
    Slot { operand, kind: SlotKind::Imm, bits }
}

const fn branch(operand: u8, bits: u8) -> Slot {
    Slot { operand, kind: SlotKind::Branch { scale: 2 }, bits }
}

const fn frame(operand: u8, scale: u8) -> Slot {
    Slot { operand, kind: SlotKind::Frame { scale }, bits: 12 }
}

const fn pattern(op: Op, width: u8, variant: u8, result: Form, operands: [Form; 4]) -> Pattern {
    Pattern { op, width, variant, result, operands, inner: None }
}

/// Operand `k`'s register number as the encoders take it.
fn r(v: &Vals, k: u8) -> u32 {
    // Truncation on purpose: a register field holds five bits.
    (v[usize::from(k)] & 0x1f) as u32
}

/// Operand `k`'s low `bits` as an unsigned field value.
fn u(v: &Vals, k: u8, bits: u32) -> u32 {
    // Truncation on purpose: `bits` is at most 32.
    (v[usize::from(k)] & ((1u64 << bits) - 1)) as u32
}

/// Operand `k`'s low `bits` as the `i32` the PC-relative and unscaled encoders take; they mask
/// it back to `bits`, so only the low bits matter.
fn s(v: &Vals, k: u8, bits: u32) -> i32 {
    u(v, k, bits).cast_signed()
}

fn narrow(c: &mut Code, rd: u32, width: u8) {
    if width < 64 {
        c.word(enc::zext(rd, rd, u32::from(width)));
    }
}

fn variant_of(at: usize) -> Result<u8, GenError> {
    u8::try_from(at).map_err(|_| GenError::TooMany { what: "a variant" })
}

fn one(each: &mut Each<'_>, pattern: Pattern, slots: &[Slot], encode: &dyn Fn(&Vals, &mut Code)) -> Result<(), GenError> {
    each(&Recipe { pattern, slots, site: None, clobbers: RegSet::EMPTY, insn_bytes: 4, encode })
}

/// Every arm64 recipe, handed to `each` in a fixed order (the table sorts them).
pub(crate) fn recipes(each: &mut Each<'_>) -> Result<(), GenError> {
    integers(each)?;
    overflows(each)?;
    bits_and_provenance(each)?;
    divisions(each)?;
    shifts(each)?;
    compares(each)?;
    conversions(each)?;
    memory(each)?;
    atomics(each)?;
    thread_locals(each)?;
    constants_and_sites(each)?;
    floats(each)?;
    one_bit_recipes::recipes(each)?;
    glue_recipes::recipes(each)
}

/// The overflow FLAG of `uadd_overflow` .. `smul_overflow` at 8 to 64 bits: `RESULT` is 1 when
/// the operation overflowed (Rust's `overflowing_*`), 0 when not; the wrapped value is the
/// plain `add`/`sub`/`mul` template's (the composer stores the two into the pair's cell, +0 and
/// +8, the Reference tier's layout: pass/int.rs `overflow`). Inputs are zero-extended, as every
/// register holds a value. Below 64 bits the exact result fits 64 (a product of two 32-bit
/// values too): it overflowed when it differs from itself extended back from the width, the
/// Reference tier's test; unsigned addition's carry is bit `w` of the exact sum.
fn overflows(each: &mut Each<'_>) -> Result<(), GenError> {
    let ops = [Op::UaddOverflow, Op::SaddOverflow, Op::UsubOverflow, Op::SsubOverflow, Op::UmulOverflow, Op::SmulOverflow];
    for op in ops {
        for w in SIZED {
            let p = pattern(op, w, 0, GPR, [GPR, GPR, NONE, NONE]);
            let signed = matches!(op, Op::SaddOverflow | Op::SsubOverflow | Op::SmulOverflow);
            // Which temporaries the sequence keeps its partial results in (the result is
            // written last: the module doc's rule).
            let temps: u8 = match (op, w) {
                (Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow, 64) | (Op::UsubOverflow, _) => 0,
                (Op::UaddOverflow | Op::UmulOverflow, _) => 1,
                _ => 2,
            };
            let all = [reg(0), reg(1), reg(RES), reg(T0), reg(T1)];
            let slots = &all[..3 + usize::from(temps)];
            one(each, p, slots, &|v, c| {
                let (a, b, d, t, u) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0), r(v, T1));
                let w = u32::from(w);
                match (op, w) {
                    (Op::UaddOverflow, 64) => {
                        c.word(enc::adds(ZR, a, b));
                        c.word(enc::cset(d, Cond::Hs));
                    }
                    (Op::SaddOverflow, 64) => {
                        c.word(enc::adds(ZR, a, b));
                        c.word(enc::cset(d, Cond::Vs));
                    }
                    (Op::UsubOverflow, _) => {
                        c.word(enc::cmp(a, b));
                        c.word(enc::cset(d, Cond::Lo));
                    }
                    (Op::SsubOverflow, 64) => {
                        c.word(enc::cmp(a, b));
                        c.word(enc::cset(d, Cond::Vs));
                    }
                    (Op::UaddOverflow, _) => {
                        c.word(enc::add(t, a, b));
                        c.word(enc::lsr_imm(d, t, w));
                    }
                    (Op::UmulOverflow, 64) => {
                        c.word(enc::umulh(t, a, b));
                        c.word(enc::cmp_imm(t, 0));
                        c.word(enc::cset(d, Cond::Ne));
                    }
                    (Op::UmulOverflow, _) => {
                        c.word(enc::mul(t, a, b));
                        c.word(enc::lsr_imm(t, t, w));
                        c.word(enc::cmp_imm(t, 0));
                        c.word(enc::cset(d, Cond::Ne));
                    }
                    (Op::SmulOverflow, 64) => {
                        // Overflowed unless the high half is the low half's sign.
                        c.word(enc::mul(t, a, b));
                        c.word(enc::smulh(u, a, b));
                        c.word(enc::asr_imm(t, t, 63));
                        c.word(enc::cmp(u, t));
                        c.word(enc::cset(d, Cond::Ne));
                    }
                    _ => {
                        debug_assert!(signed, "every unsigned case is above");
                        c.word(enc::sext(t, a, w));
                        c.word(enc::sext(u, b, w));
                        c.word(match op {
                            Op::SaddOverflow => enc::add(t, t, u),
                            Op::SsubOverflow => enc::sub(t, t, u),
                            _ => enc::mul(t, t, u),
                        });
                        c.word(enc::sext(u, t, w));
                        c.word(enc::cmp(t, u));
                        c.word(enc::cset(d, Cond::Ne));
                    }
                }
            })?;
        }
    }
    Ok(())
}

fn integers(each: &mut Each<'_>) -> Result<(), GenError> {
    for op in [Op::Add, Op::Sub, Op::Mul, Op::And, Op::Or, Op::Xor] {
        for w in WIDTHS {
            one(each, pattern(op, w, 0, GPR, [GPR, GPR, NONE, NONE]), &[reg(0), reg(1), reg(RES)], &|v, c| {
                let (a, b, d) = (r(v, 0), r(v, 1), r(v, RES));
                c.word(match op {
                    Op::Add => enc::add(d, a, b),
                    Op::Sub => enc::sub(d, a, b),
                    Op::Mul => enc::mul(d, a, b),
                    Op::And => enc::and(d, a, b),
                    Op::Or => enc::orr(d, a, b),
                    _ => enc::eor(d, a, b),
                });
                if matches!(op, Op::Add | Op::Sub | Op::Mul) {
                    narrow(c, d, w);
                }
            })?;
        }
    }
    for op in [Op::Add, Op::Sub] {
        for w in WIDTHS {
            for (class, lsl12) in [(ImmClass::U12, false), (ImmClass::U12Lsl12, true)] {
                let p = pattern(op, w, 0, GPR, [GPR, Form::Imm(class), NONE, NONE]);
                one(each, p, &[reg(0), imm(1, 12), reg(RES)], &|v, c| {
                    let (a, k, d) = (r(v, 0), u(v, 1, 12), r(v, RES));
                    c.word(match op {
                        Op::Add => enc::add_imm(d, a, k, lsl12),
                        _ => enc::sub_imm(d, a, k, lsl12),
                    });
                    narrow(c, d, w);
                })?;
            }
        }
    }
    // A logical immediate, held as `N:immr:imms` (13 bits): `imms` the low six, `immr` the next
    // six, `N` bit 12. Inputs are zero-extended and so is the constant, so the 64-bit form is
    // right at every width.
    for op in [Op::And, Op::Or] {
        for w in WIDTHS {
            let p = pattern(op, w, 0, GPR, [GPR, Form::Imm(ImmClass::Logical64), NONE, NONE]);
            one(each, p, &[reg(0), imm(1, 13), reg(RES)], &|v, c| {
                let (a, d, k) = (r(v, 0), r(v, RES), u(v, 1, 13));
                let (n, immr, imms) = (k >> 12 & 1, k >> 6 & 0x3f, k & 0x3f);
                c.word(match op {
                    Op::And => enc::and_imm(d, a, n, immr, imms),
                    _ => enc::orr_imm(d, a, n, immr, imms),
                });
            })?;
        }
    }
    for op in [Op::Neg, Op::Not] {
        for w in WIDTHS {
            one(each, pattern(op, w, 0, GPR, [GPR, NONE, NONE, NONE]), &[reg(0), reg(RES)], &|v, c| {
                let (a, d) = (r(v, 0), r(v, RES));
                c.word(match op {
                    Op::Neg => enc::neg(d, a),
                    _ => enc::mvn(d, a),
                });
                narrow(c, d, w);
            })?;
        }
    }
    for w in WIDTHS {
        let p = pattern(Op::Select, w, 0, GPR, [GPR, GPR, GPR, NONE]);
        one(each, p, &[reg(0), reg(1), reg(2), reg(RES)], &|v, c| {
            c.word(enc::cmp_imm(r(v, 0), 0));
            c.word(enc::csel(r(v, RES), r(v, 1), r(v, 2), Cond::Ne));
        })?;
    }
    // `ptr_add`: its width is the offset's, sign-extended (pass.rs, `Op::PtrAdd`).
    for w in SIZED {
        let p = pattern(Op::PtrAdd, w, 0, GPR, [GPR, GPR, NONE, NONE]);
        let slots = [reg(0), reg(1), reg(RES), reg(T0)];
        // At 64 bits the offset is used as it is: no temporary.
        let used: &[Slot] = if w == 64 { &slots[..3] } else { &slots };
        one(each, p, used, &|v, c| {
            let (a, b, d, t) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0));
            match w {
                64 => c.word(enc::add(d, a, b)),
                _ => {
                    c.word(enc::sext(t, b, u32::from(w)));
                    c.word(enc::add(d, a, t));
                }
            }
        })?;
    }
    let p = pattern(Op::PtrAdd, 64, 0, GPR, [GPR, Form::Imm(ImmClass::U12), NONE, NONE]);
    one(each, p, &[reg(0), imm(1, 12), reg(RES)], &|v, c| c.word(enc::add_imm(r(v, RES), r(v, 0), u(v, 1, 12), false)))
}

/// The vector register `popcnt` counts in: an allocatable caller-saved one (v31), declared in
/// the template's clobbers so the composer moves out what it holds there first.
const POPCNT_LANE: u32 = 31;

/// `clz`, `ctz`, `popcnt`, `bswap`, `bitrev` at every width, the Reference tier's sequences
/// (pass/int.rs `bits`) over zero-extended inputs, and the provenance moves at 64 bits
/// (`ptr_addr`, `expose`, `from_exposed`: the value as it is; `ptr_with_addr`: the address).
fn bits_and_provenance(each: &mut Each<'_>) -> Result<(), GenError> {
    for w in WIDTHS {
        let wide = u32::from(w);
        let p = pattern(Op::Clz, w, 0, GPR, [GPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| {
            let d = r(v, RES);
            // The 64-bit count over a zero-extended value counts 64 - width zeros too many.
            c.word(enc::clz(d, r(v, 0)));
            if wide < 64 {
                c.word(enc::sub_imm(d, d, 64 - wide, false));
            }
        })?;
        let p = pattern(Op::Ctz, w, 0, GPR, [GPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| {
            let (a, d) = (r(v, 0), r(v, RES));
            // Bit `width` set first, so a zero input counts `width`.
            match wide < 64 {
                true => {
                    c.word(enc::orr_bit(d, a, wide));
                    c.word(enc::rbit(d, d));
                }
                false => c.word(enc::rbit(d, a)),
            }
            c.word(enc::clz(d, d));
        })?;
        each(&Recipe {
            pattern: pattern(Op::Popcnt, w, 0, GPR, [GPR, NONE, NONE, NONE]),
            slots: &[reg(0), reg(RES)],
            site: None,
            clobbers: RegSet(1u64 << (32 + POPCNT_LANE)),
            insn_bytes: 4,
            encode: &|v, c| {
                c.word(enc::fmov_to_d(POPCNT_LANE, r(v, 0)));
                c.word(enc::cnt_8b(POPCNT_LANE, POPCNT_LANE));
                c.word(enc::addv_8b(POPCNT_LANE, POPCNT_LANE));
                c.word(enc::fmov_from_d(r(v, RES), POPCNT_LANE));
            },
        })?;
        let p = pattern(Op::Bswap, w, 0, GPR, [GPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| {
            let (a, d) = (r(v, 0), r(v, RES));
            c.word(match wide {
                // A value of one byte or less is its own byte swap.
                1 | 8 => enc::mov(d, a),
                16 => enc::rev16_w(d, a),
                32 => enc::rev_w(d, a),
                _ => enc::rev(d, a),
            });
        })?;
        let p = pattern(Op::Bitrev, w, 0, GPR, [GPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| {
            let d = r(v, RES);
            c.word(enc::rbit(d, r(v, 0)));
            if wide < 64 {
                c.word(enc::lsr_imm(d, d, 64 - wide));
            }
        })?;
    }
    for op in [Op::PtrAddr, Op::Expose, Op::FromExposed] {
        let p = pattern(op, 64, 64, GPR, [GPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| c.word(enc::mov(r(v, RES), r(v, 0))))?;
    }
    // The pointer (operand 0) is read for its provenance only: no field holds it.
    let p = pattern(Op::PtrWithAddr, 64, 0, GPR, [GPR, GPR, NONE, NONE]);
    one(each, p, &[reg(1), reg(RES)], &|v, c| c.word(enc::mov(r(v, RES), r(v, 1))))
}

/// Atomics of 8 to 64 bits and fences, the Reference tier's Armv8.0 mapping (pass/atomic.rs;
/// no LSE, so one table serves every arm64 core). Variants are `glue::atomic`'s (acquire,
/// release, the `atomic_rmw` kind).
/// - `atomic_load p` (`[p]`): `ldar` when it acquires, else `ldr`; `atomic_store v, p`
///   (`[v, p]`): `stlr` when it releases, else `str`.
/// - `atomic_rmw p, v` (`[p, v, status]`, `status` a temporary the composer gives): `0: ld{a}xr
///   TEMP0, [p]; TEMP1 = TEMP0 rmw v; st{l}xr wstatus, TEMP1, [p]; cbnz status, 0; mov
///   RESULT, TEMP0`. Signed `max`/`min` below 64 bits compare both sign-extended (TEMP1 and
///   `status`, before the store writes it) and store the zero-extended winner.
/// - `cmpxchg p, e, n` (`[p, e, n, status]`): `0: ld{a}xr TEMP0, [p]; cmp TEMP0, e; b.ne 6;
///   st{l}xr wstatus, n, [p]; cbnz status, 0; b 7; 6: clrex; 7: mov RESULT, TEMP0`; the
///   composer's `icmp eq` of the old value and `e` is the pair's flag.
/// - `fence`: `dmb ishld` (acquire) or `dmb ish`.
///
/// RESULT is written last, so it may be any input's register, and `status` only after `p`,
/// `v`/`n` are read for the last time in an iteration (it is a register of its own).
fn atomics(each: &mut Each<'_>) -> Result<(), GenError> {
    use crate::compose::emit::glue;
    use blaze_lir::AtomicRmw;
    for (w, size) in [(8u8, Size::B), (16, Size::H), (32, Size::W), (64, Size::X)] {
        for side in [false, true] {
            let p = pattern(Op::AtomicLoad, w, glue::atomic(0, side, false), GPR, [GPR, NONE, NONE, NONE]);
            one(each, p, &[reg(0), reg(RES)], &|v, c| {
                let (a, d) = (r(v, 0), r(v, RES));
                c.word(if side { enc::ldar(size, d, a) } else { enc::ldr(size, d, a, 0) });
            })?;
            let p = pattern(Op::AtomicStore, w, glue::atomic(0, false, side), NONE, [GPR, GPR, NONE, NONE]);
            one(each, p, &[reg(0), reg(1)], &|v, c| {
                let (x, a) = (r(v, 0), r(v, 1));
                c.word(if side { enc::stlr(size, x, a) } else { enc::str(size, x, a, 0) });
            })?;
        }
        for (acq, rel) in [(false, false), (false, true), (true, false), (true, true)] {
            for rmw in AtomicRmw::ALL {
                let p = pattern(Op::AtomicRmw, w, glue::atomic(rmw as u8, acq, rel), GPR, [GPR, GPR, GPR, NONE]);
                let all = [reg(0), reg(1), reg(2), reg(RES), reg(T0), reg(T1)];
                // `xchg` stores `v` itself: no TEMP1.
                let slots: &[Slot] = if rmw == AtomicRmw::Xchg { &all[..5] } else { &all };
                one(each, p, slots, &|v, c| {
                    let (a, x, s, d, old, new) = (r(v, 0), r(v, 1), r(v, 2), r(v, RES), r(v, T0), r(v, T1));
                    c.word(enc::ldxr(size, acq, old, a));
                    let mut words = 1i32;
                    let mut put = |c: &mut Code, word: u32| {
                        c.word(word);
                        words += 1;
                    };
                    let stored = match rmw {
                        AtomicRmw::Xchg => x,
                        AtomicRmw::Add => {
                            put(c, enc::add(new, old, x));
                            new
                        }
                        AtomicRmw::Sub => {
                            put(c, enc::sub(new, old, x));
                            new
                        }
                        AtomicRmw::And => {
                            put(c, enc::and(new, old, x));
                            new
                        }
                        AtomicRmw::Nand => {
                            put(c, enc::and(new, old, x));
                            put(c, enc::mvn(new, new));
                            new
                        }
                        AtomicRmw::Or => {
                            put(c, enc::orr(new, old, x));
                            new
                        }
                        AtomicRmw::Xor => {
                            put(c, enc::eor(new, old, x));
                            new
                        }
                        AtomicRmw::Max | AtomicRmw::Min | AtomicRmw::Umax | AtomicRmw::Umin => {
                            let keep = match rmw {
                                AtomicRmw::Max => Cond::Gt,
                                AtomicRmw::Min => Cond::Lt,
                                AtomicRmw::Umax => Cond::Hi,
                                _ => Cond::Lo,
                            };
                            match matches!(rmw, AtomicRmw::Max | AtomicRmw::Min) && w < 64 {
                                true => {
                                    put(c, enc::sext(new, old, u32::from(w)));
                                    put(c, enc::sext(s, x, u32::from(w)));
                                    put(c, enc::cmp(new, s));
                                }
                                false => put(c, enc::cmp(old, x)),
                            }
                            put(c, enc::csel(new, old, x, keep));
                            new
                        }
                    };
                    put(c, enc::stxr(size, rel, s, stored, a));
                    c.word(enc::cbnz(s, -words));
                    c.word(enc::mov(d, old));
                })?;
            }
            let p = pattern(Op::Cmpxchg, w, glue::atomic(0, acq, rel), GPR, [GPR, GPR, GPR, GPR]);
            one(each, p, &[reg(0), reg(1), reg(2), reg(3), reg(RES), reg(T0)], &|v, c| {
                let (a, e, n, s, d, old) = (r(v, 0), r(v, 1), r(v, 2), r(v, 3), r(v, RES), r(v, T0));
                c.word(enc::ldxr(size, acq, old, a));
                c.word(enc::cmp(old, e));
                c.word(enc::b_cond(Cond::Ne, 4));
                c.word(enc::stxr(size, rel, s, n, a));
                c.word(enc::cbnz(s, -4));
                c.word(enc::b(2));
                c.word(enc::clrex());
                c.word(enc::mov(d, old));
            })?;
        }
    }
    one(each, pattern(Op::Fence, 0, glue::FENCE_LOADS, NONE, [NONE; 4]), &[], &|_, c| c.word(enc::dmb(enc::Barrier::IshLd)))?;
    one(each, pattern(Op::Fence, 0, glue::FENCE_ALL, NONE, [NONE; 4]), &[], &|_, c| c.word(enc::dmb(enc::Barrier::Ish)))
}

/// `tls_addr` by index, the Reference tier's sequence (pass/tls_index.rs, its offsets the same
/// constants), in two templates (one would pass `MOST_BYTES`); the composer loads the three
/// import words' values first (the descriptor's, the thread-local's index, the getter's):
/// - the root's address (`glue::tls_root(os)`, `[desc]` to RESULT): `ldr x3, [desc, #root];
///   and x3, x3, #~(1 << 63)`, then macOS `mrs x4, tpidrro_el0; and x4, x4, #~7` (the
///   specific-data slots, rule 45) or Linux `mrs x4, tpidr_el0`, then `add RESULT, x4, x3`;
/// - the lookup (`glue::TLS_LOOKUP`, `[at, index, getter, desc]`): the table at `at`, the copy
///   at its slot `index` when the table is long enough and the copy exists, else the getter
///   called with `(desc, index, at)`; the thread's copy left in x0 (the composer moves it to the
///   result after the call's reloads).
///
/// x3..x6 are work registers outside the allocatable set, like x0..x2; the lookup clobbers what
/// a call does.
fn thread_locals(each: &mut Each<'_>) -> Result<(), GenError> {
    use crate::compose::emit::glue;
    use crate::pass::tls_index::{DESC_ROOT_WORDS, TABLE_LEN_WORDS, TABLE_SLOTS};
    const X0: u32 = 0;
    const X1: u32 = 1;
    const X2: u32 = 2;
    const ROOT: u32 = 3;
    const SLOTS: u32 = 4;
    const TABLE: u32 = 5;
    const WORK: u32 = 6;
    for os in [blaze_tier::Os::MacOs, blaze_tier::Os::Linux] {
        let mac = os == blaze_tier::Os::MacOs;
        each(&Recipe {
            pattern: pattern(Op::TlsAddr, 64, glue::tls_root(os), GPR, [GPR, NONE, NONE, NONE]),
            slots: &[reg(0), reg(RES)],
            site: None,
            clobbers: RegSet((1 << ROOT) | (1 << SLOTS)),
            insn_bytes: 4,
            encode: &|v, c| {
                c.word(enc::ldr(Size::X, ROOT, r(v, 0), DESC_ROOT_WORDS));
                c.word(crate::enc_tls::and_clear_bit63(ROOT, ROOT));
                if mac {
                    c.word(enc::mrs_tpidrro_el0(SLOTS));
                    // `and x4, x4, #~7`: 61 ones rotated right by 61.
                    c.word(enc::and_imm(SLOTS, SLOTS, 1, 61, 60));
                } else {
                    c.word(crate::enc_tls::mrs_tpidr_el0(SLOTS));
                }
                c.word(enc::add(r(v, RES), SLOTS, ROOT));
            },
        })?;
    }
    each(&Recipe {
        pattern: pattern(Op::TlsAddr, 64, glue::TLS_LOOKUP, NONE, [GPR, GPR, GPR, GPR]),
        slots: &[reg(0), reg(1), reg(2), reg(3)],
        site: None,
        clobbers: CALL_CLOBBERS,
        insn_bytes: 4,
        encode: &|v, c| {
            let (at, index, getter, desc) = (r(v, 0), r(v, 1), r(v, 2), r(v, 3));
            c.word(enc::ldr(Size::X, TABLE, at, 0));
            // To `slow` (7 words on): no table yet.
            c.word(enc::cbz(TABLE, 7));
            c.word(enc::ldr(Size::X, WORK, TABLE, TABLE_LEN_WORDS));
            c.word(enc::cmp(index, WORK));
            // To `slow` (4 words on): the table is too short for the index.
            c.word(enc::b_cond(Cond::Hs, 4));
            c.word(enc::add_imm(WORK, TABLE, TABLE_SLOTS, false));
            c.word(enc::ldr_lsl3(X0, WORK, index));
            // To `done` (5 words on): the copy exists.
            c.word(enc::cbnz(X0, 5));
            // slow: the getter makes the table, grows it or makes the copy.
            c.word(enc::mov(X0, desc));
            c.word(enc::mov(X1, index));
            c.word(enc::mov(X2, at));
            c.word(enc::blr(getter));
            // done:
        },
    })
}

/// Division traps on zero (`cbnz` over a `brk`); `sdiv` traps on `MIN / -1` (LIR's rule),
/// `srem` of it is 0, which `msub` gives.
fn divisions(each: &mut Each<'_>) -> Result<(), GenError> {
    for w in SIZED {
        let slots = [reg(0), reg(1), reg(RES), reg(T0), reg(T1)];
        one(each, pattern(Op::Udiv, w, 0, GPR, [GPR, GPR, NONE, NONE]), &slots[..3], &|v, c| {
            let (a, b, d) = (r(v, 0), r(v, 1), r(v, RES));
            c.word(enc::cbnz(b, 2));
            c.word(enc::brk(BRK_DIV_ZERO));
            c.word(enc::udiv(d, a, b));
        })?;
        one(each, pattern(Op::Urem, w, 0, GPR, [GPR, GPR, NONE, NONE]), &slots[..4], &|v, c| {
            let (a, b, d, t) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0));
            c.word(enc::cbnz(b, 2));
            c.word(enc::brk(BRK_DIV_ZERO));
            c.word(enc::udiv(t, a, b));
            c.word(enc::msub(d, t, b, a));
        })?;
        let sdiv_slots: &[Slot] = if w == 64 { &slots[..3] } else { &slots };
        one(each, pattern(Op::Sdiv, w, 0, GPR, [GPR, GPR, NONE, NONE]), sdiv_slots, &|v, c| {
            let (a, b, d, t0, t1) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0), r(v, T1));
            c.word(enc::cbnz(b, 2));
            c.word(enc::brk(BRK_DIV_ZERO));
            match w {
                // `MIN / -1`: `b + 1 == 0` and `a - 1` overflows.
                64 => {
                    c.word(enc::cmn_imm(b, 1));
                    c.word(enc::b_cond(Cond::Ne, 4));
                    c.word(enc::cmp_imm(a, 1));
                    c.word(enc::b_cond(Cond::Vc, 2));
                    c.word(enc::brk(BRK_DIV_OVERFLOW));
                    c.word(enc::sdiv(d, a, b));
                }
                // Narrower: the quotient of the sign-extended operands overflows exactly when it
                // does not fit the width (`MIN / -1` is `2^(w-1)`).
                _ => {
                    let w = u32::from(w);
                    c.word(enc::sext(t0, a, w));
                    c.word(enc::sext(t1, b, w));
                    c.word(enc::sdiv(t0, t0, t1));
                    c.word(enc::sext(t1, t0, w));
                    c.word(enc::cmp(t0, t1));
                    c.word(enc::b_cond(Cond::Eq, 2));
                    c.word(enc::brk(BRK_DIV_OVERFLOW));
                    c.word(enc::zext(d, t0, w));
                }
            }
        })?;
        let srem_slots: &[Slot] = if w == 64 { &slots[..4] } else { &slots };
        one(each, pattern(Op::Srem, w, 0, GPR, [GPR, GPR, NONE, NONE]), srem_slots, &|v, c| {
            let (a, b, d, t0, t1) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0), r(v, T1));
            c.word(enc::cbnz(b, 2));
            c.word(enc::brk(BRK_DIV_ZERO));
            match w {
                64 => {
                    c.word(enc::sdiv(t0, a, b));
                    c.word(enc::msub(d, t0, b, a));
                }
                _ => {
                    let w = u32::from(w);
                    c.word(enc::sext(t0, a, w));
                    c.word(enc::sext(t1, b, w));
                    c.word(enc::sdiv(d, t0, t1));
                    c.word(enc::msub(d, d, t1, t0));
                    c.word(enc::zext(d, d, w));
                }
            }
        })?;
    }
    Ok(())
}

/// Shifts by a register take the amount modulo the width (pass.rs); by an immediate, the
/// amount is below the width (mod.rs). `lsl` by an immediate is not here: `ubfm` holds
/// `64 - s`, which no field holds linearly.
fn shifts(each: &mut Each<'_>) -> Result<(), GenError> {
    for op in [Op::Shl, Op::Lshr, Op::Ashr] {
        for w in SIZED {
            let slots = [reg(0), reg(1), reg(RES), reg(T0), reg(T1)];
            let used: &[Slot] = match (op, w) {
                (_, 64) => &slots[..3],
                (Op::Ashr, _) => &slots,
                _ => &slots[..4],
            };
            one(each, pattern(op, w, 0, GPR, [GPR, GPR, NONE, NONE]), used, &|v, c| {
                let (a, b, d, t0, t1) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0), r(v, T1));
                if w == 64 {
                    c.word(match op {
                        Op::Shl => enc::lslv(d, a, b),
                        Op::Lshr => enc::lsrv(d, a, b),
                        _ => enc::asrv(d, a, b),
                    });
                    return;
                }
                c.word(enc::zext(t0, b, u32::from(w).trailing_zeros()));
                match op {
                    Op::Shl => {
                        c.word(enc::lslv(d, a, t0));
                        narrow(c, d, w);
                    }
                    Op::Lshr => c.word(enc::lsrv(d, a, t0)),
                    _ => {
                        c.word(enc::sext(t1, a, u32::from(w)));
                        c.word(enc::asrv(d, t1, t0));
                        narrow(c, d, w);
                    }
                }
            })?;
        }
    }
    for (w, class, bits) in [(32u8, ImmClass::Shift5, 5u8), (64, ImmClass::Shift6, 6)] {
        let p = pattern(Op::Lshr, w, 0, GPR, [GPR, Form::Imm(class), NONE, NONE]);
        one(each, p, &[reg(0), imm(1, bits), reg(RES)], &|v, c| {
            c.word(enc::lsr_imm(r(v, RES), r(v, 0), u(v, 1, u32::from(bits))));
        })?;
        let slots = [reg(0), imm(1, bits), reg(RES), reg(T0)];
        let used: &[Slot] = if w == 64 { &slots[..3] } else { &slots };
        let p = pattern(Op::Ashr, w, 0, GPR, [GPR, Form::Imm(class), NONE, NONE]);
        one(each, p, used, &|v, c| {
            let (a, k, d, t) = (r(v, 0), u(v, 1, u32::from(bits)), r(v, RES), r(v, T0));
            match w {
                64 => c.word(enc::asr_imm(d, a, k)),
                _ => {
                    c.word(enc::sext(t, a, u32::from(w)));
                    c.word(enc::asr_imm(d, t, k));
                    narrow(c, d, w);
                }
            }
        })?;
    }
    Ok(())
}

/// The flags of `a` against `b` (a register, or a `U12` immediate when `b_imm`), the operands
/// sign-extended first when `cond` reads the sign below 64 bits.
fn compare(c: &mut Code, v: &Vals, cond: IntCond, w: u8, a: u8, b: u8, b_imm: bool) {
    let (x, t0, t1) = (r(v, a), r(v, T0), r(v, T1));
    let signed = is_signed(cond) && w < 64;
    let x = match signed {
        true => {
            c.word(enc::sext(t0, x, u32::from(w)));
            t0
        }
        false => x,
    };
    match b_imm {
        true => c.word(enc::cmp_imm(x, u(v, b, 12))),
        false => {
            let y = match signed {
                true => {
                    c.word(enc::sext(t1, r(v, b), u32::from(w)));
                    t1
                }
                false => r(v, b),
            };
            c.word(enc::cmp(x, y));
        }
    }
}

/// `icmp` into a register (`cset`), `br` of a value (`cbnz`), `br` fused with its `icmp`
/// (`b.cond`), and `jump` (`b`).
fn compares(each: &mut Each<'_>) -> Result<(), GenError> {
    for (at, cond) in IntCond::ALL.into_iter().enumerate() {
        let variant = variant_of(at)?;
        for w in WIDTHS {
            for b_imm in [false, true] {
                let b_form = if b_imm { Form::Imm(ImmClass::U12) } else { GPR };
                let b_slot = if b_imm { imm(1, 12) } else { reg(1) };
                let temps = match (is_signed(cond) && w < 64, b_imm) {
                    (false, _) => 0,
                    (true, true) => 1,
                    (true, false) => 2,
                };
                let slots = [reg(0), b_slot, reg(RES), reg(T0), reg(T1)];
                let p = pattern(Op::Icmp, w, variant, GPR, [GPR, b_form, NONE, NONE]);
                one(each, p, &slots[..3 + temps], &|v, c| {
                    compare(c, v, cond, w, 0, 1, b_imm);
                    c.word(enc::cset(r(v, RES), cond_of(cond)));
                })?;
                // Fused: `[None, Block, a, b]`, the compare's operands after the branch's two.
                let b_slot = if b_imm { imm(3, 12) } else { reg(3) };
                let slots = [branch(1, 19), reg(2), b_slot, reg(T0), reg(T1)];
                let p = Pattern {
                    op: Op::Br,
                    width: w,
                    variant: 0,
                    result: NONE,
                    operands: [NONE, BLOCK, GPR, b_form],
                    inner: Some(Inner { op: Op::Icmp, variant, operand: 0 }),
                };
                one(each, p, &slots[..3 + temps], &|v, c| {
                    compare(c, v, cond, w, 2, 3, b_imm);
                    c.word(enc::b_cond(cond_of(cond), s(v, 1, 19)));
                })?;
            }
        }
    }
    one(each, pattern(Op::Br, 1, 0, NONE, [GPR, BLOCK, NONE, NONE]), &[reg(0), branch(1, 19)], &|v, c| {
        c.word(enc::cbnz(r(v, 0), s(v, 1, 19)));
    })?;
    one(each, pattern(Op::Jump, 0, 0, NONE, [BLOCK, NONE, NONE, NONE]), &[branch(0, 26)], &|v, c| {
        c.word(enc::b(s(v, 0, 26)));
    })
}

/// `zext`, `sext` and `trunc` at the result's width, the source's width as the variant.
fn conversions(each: &mut Each<'_>) -> Result<(), GenError> {
    for from in [1u8, 8, 16, 32, 64] {
        for to in [1u8, 8, 16, 32, 64] {
            if from < to {
                let p = pattern(Op::Zext, to, from, GPR, [GPR, NONE, NONE, NONE]);
                one(each, p, &[reg(0), reg(RES)], &|v, c| c.word(enc::mov(r(v, RES), r(v, 0))))?;
                let p = pattern(Op::Sext, to, from, GPR, [GPR, NONE, NONE, NONE]);
                one(each, p, &[reg(0), reg(RES)], &|v, c| {
                    let d = r(v, RES);
                    c.word(enc::sext(d, r(v, 0), u32::from(from)));
                    narrow(c, d, to);
                })?;
            }
            if to < from {
                let p = pattern(Op::Trunc, to, from, GPR, [GPR, NONE, NONE, NONE]);
                one(each, p, &[reg(0), reg(RES)], &|v, c| c.word(enc::zext(r(v, RES), r(v, 0), u32::from(to))))?;
            }
        }
    }
    Ok(())
}

/// Loads and stores, the whole grid of [`ACCESSES`] times every address form the composer
/// makes (compose/emit.rs `load`, `store`, `offset`): an immediate offset (unscaled `S9`,
/// scaled `U12`), an offset in a register (one past both ranges), the base and 64-bit index of a
/// fused `ptr_add` (`[rn, rm]`), and a stack slot fused with its `slot_addr` (a `Frame` field off
/// `sp`); then `slot_addr` itself. A float access is the integer one's SIMD&FP form.
fn memory(each: &mut Each<'_>) -> Result<(), GenError> {
    for (w, class, size, scaled, scale) in ACCESSES {
        let value = Form::Reg(class);
        let fp = class == RegClass::Fpr;
        let form = move |word: u32| if fp { enc::fp_access(word) } else { word };
        let p = pattern(Op::Load, w, 0, value, [GPR, Form::Imm(ImmClass::S9), NONE, NONE]);
        one(each, p, &[reg(0), imm(1, 9), reg(RES)], &|v, c| c.word(form(enc::ldur(size, r(v, RES), r(v, 0), s(v, 1, 9)))))?;
        let p = pattern(Op::Load, w, 0, value, [GPR, Form::Imm(scaled), NONE, NONE]);
        one(each, p, &[reg(0), imm(1, 12), reg(RES)], &|v, c| c.word(form(enc::ldr(size, r(v, RES), r(v, 0), u(v, 1, 12)))))?;
        let p = pattern(Op::Load, w, 0, value, [GPR, GPR, NONE, NONE]);
        one(each, p, &[reg(0), reg(1), reg(RES)], &|v, c| c.word(form(enc::ldr_reg(size, r(v, RES), r(v, 0), r(v, 1)))))?;
        let p = pattern(Op::Store, w, 0, NONE, [value, GPR, Form::Imm(ImmClass::S9), NONE]);
        one(each, p, &[reg(0), reg(1), imm(2, 9)], &|v, c| c.word(form(enc::stur(size, r(v, 0), r(v, 1), s(v, 2, 9)))))?;
        let p = pattern(Op::Store, w, 0, NONE, [value, GPR, Form::Imm(scaled), NONE]);
        one(each, p, &[reg(0), reg(1), imm(2, 12)], &|v, c| c.word(form(enc::str(size, r(v, 0), r(v, 1), u(v, 2, 12)))))?;
        let p = pattern(Op::Store, w, 0, NONE, [value, GPR, GPR, NONE]);
        one(each, p, &[reg(0), reg(1), reg(2)], &|v, c| c.word(form(enc::str_reg(size, r(v, 0), r(v, 1), r(v, 2)))))?;

        // A fused `ptr_add` (compose/emit.rs `fuses_into_next`: a 64-bit index, offset 0): the
        // load's operands `[None, base, index, None]`, the store's `[value, None, base, index]`.
        let p = Pattern {
            op: Op::Load,
            width: w,
            variant: 0,
            result: value,
            operands: [NONE, GPR, GPR, NONE],
            inner: Some(Inner { op: Op::PtrAdd, variant: 0, operand: 0 }),
        };
        one(each, p, &[reg(1), reg(2), reg(RES)], &|v, c| c.word(form(enc::ldr_reg(size, r(v, RES), r(v, 1), r(v, 2)))))?;
        let p = Pattern {
            op: Op::Store,
            width: w,
            variant: 0,
            result: NONE,
            operands: [value, NONE, GPR, GPR],
            inner: Some(Inner { op: Op::PtrAdd, variant: 0, operand: 1 }),
        };
        one(each, p, &[reg(0), reg(2), reg(3)], &|v, c| c.word(form(enc::str_reg(size, r(v, 0), r(v, 2), r(v, 3)))))?;

        let p = Pattern {
            op: Op::Load,
            width: w,
            variant: 0,
            result: value,
            operands: [NONE, NONE, SLOT, NONE],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 0 }),
        };
        one(each, p, &[frame(2, scale), reg(RES)], &|v, c| c.word(form(enc::ldr(size, r(v, RES), enc::SP, u(v, 2, 12)))))?;
        let p = Pattern {
            op: Op::Store,
            width: w,
            variant: 0,
            result: NONE,
            operands: [value, NONE, NONE, SLOT],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 1 }),
        };
        one(each, p, &[reg(0), frame(3, scale)], &|v, c| c.word(form(enc::str(size, r(v, 0), enc::SP, u(v, 3, 12)))))?;
    }
    let p = pattern(Op::SlotAddr, 64, 0, GPR, [SLOT, NONE, NONE, NONE]);
    one(each, p, &[frame(0, 0), reg(RES)], &|v, c| c.word(enc::add_imm(r(v, RES), enc::SP, u(v, 0, 12), false)))
}

/// Constants, `null`, references through import sites, calls and traps.
fn constants_and_sites(each: &mut Each<'_>) -> Result<(), GenError> {
    for w in SIZED {
        let p = pattern(Op::Iconst, w, 0, GPR, [Form::Imm(ImmClass::Mov16), NONE, NONE, NONE]);
        one(each, p, &[imm(0, 16), reg(RES)], &|v, c| c.word(enc::movz(r(v, RES), u(v, 0, 16), 0)))?;
    }
    // Any 64-bit value: `movz` and three `movk`, one operand split over four fields.
    let wide = |v: &Vals, c: &mut Code| {
        let (d, k) = (r(v, RES), v[0]);
        for piece in 0..4u32 {
            // Truncation on purpose: one 16-bit piece.
            let bits = (k >> (16 * piece) & 0xffff) as u32;
            c.word(match piece {
                0 => enc::movz(d, bits, 0),
                _ => enc::movk(d, bits, piece),
            });
        }
    };
    for op in [Op::Iconst, Op::IconstWide] {
        let p = pattern(op, 64, 0, GPR, [Form::Imm(ImmClass::I64), NONE, NONE, NONE]);
        one(each, p, &[imm(0, 64), reg(RES)], &wide)?;
    }
    one(each, pattern(Op::Null, 64, 0, GPR, [NONE; 4]), &[reg(RES)], &|v, c| c.word(enc::mov(r(v, RES), ZR)))?;

    // An import word's value (a function's or a static's address): `adrp xd; ldr xd`.
    for op in [Op::FuncAddr, Op::DataAddr] {
        each(&Recipe {
            pattern: pattern(op, 64, 0, GPR, [Form::Site(SiteKind::Load), NONE, NONE, NONE]),
            slots: &[reg(RES)],
            site: Some(SiteAt { operand: 0, kind: SiteKind::Load, byte: 0 }),
            clobbers: RegSet::EMPTY,
            insn_bytes: 4,
            encode: &|v, c| {
                let d = r(v, RES);
                c.word(enc::adrp(d, 0));
                c.word(enc::ldr(Size::X, d, d, 0));
            },
        })?;
    }
    // A byte of the record's relro part: `adrp xd; add xd, xd`.
    each(&Recipe {
        pattern: pattern(Op::DataAddr, 64, 0, GPR, [Form::Site(SiteKind::Relro), NONE, NONE, NONE]),
        slots: &[reg(RES)],
        site: Some(SiteAt { operand: 0, kind: SiteKind::Relro, byte: 0 }),
        clobbers: RegSet::EMPTY,
        insn_bytes: 4,
        encode: &|v, c| {
            let d = r(v, RES);
            c.word(enc::adrp(d, 0));
            c.word(enc::add_imm(d, d, 0, false));
        },
    })?;
    each(&Recipe {
        pattern: pattern(Op::Call, 0, 0, NONE, [Form::Site(SiteKind::Call), NONE, NONE, NONE]),
        slots: &[],
        site: Some(SiteAt { operand: 0, kind: SiteKind::Call, byte: 0 }),
        clobbers: CALL_CLOBBERS,
        insn_bytes: 4,
        encode: &|_, c| {
            c.word(enc::adrp(X16, 0));
            c.word(enc::ldr(Size::X, X16, X16, 0));
            c.word(enc::blr(X16));
        },
    })?;
    each(&Recipe {
        pattern: pattern(Op::CallIndirect, 0, 0, NONE, [GPR, NONE, NONE, NONE]),
        slots: &[reg(0)],
        site: None,
        clobbers: CALL_CLOBBERS,
        insn_bytes: 4,
        encode: &|v, c| c.word(enc::blr(r(v, 0))),
    })?;
    let p = pattern(Op::Trap, 0, 0, NONE, [Form::Imm(ImmClass::Mov16), NONE, NONE, NONE]);
    one(each, p, &[imm(0, 16)], &|v, c| c.word(enc::brk(u(v, 0, 16))))?;
    one(each, pattern(Op::Unreachable, 0, 0, NONE, [NONE; 4]), &[], &|_, c| c.word(enc::brk(BRK_UNREACHABLE)))
}

/// `fadd`, `fsub`, `fmul`, `fdiv` (enc.rs `fbin`'s opcodes 2, 3, 0, 1) and `fneg`, at 32 and 64.
fn floats(each: &mut Each<'_>) -> Result<(), GenError> {
    for w in [32u8, 64] {
        let double = w == 64;
        for (op, opcode) in [(Op::Fadd, 2u32), (Op::Fsub, 3), (Op::Fmul, 0), (Op::Fdiv, 1)] {
            one(each, pattern(op, w, 0, FPR, [FPR, FPR, NONE, NONE]), &[reg(0), reg(1), reg(RES)], &|v, c| {
                c.word(enc::fbin(opcode, double, r(v, RES), r(v, 0), r(v, 1)));
            })?;
        }
        one(each, pattern(Op::Fneg, w, 0, FPR, [FPR, NONE, NONE, NONE]), &[reg(0), reg(RES)], &|v, c| {
            c.word(enc::fneg(double, r(v, RES), r(v, 0)));
        })?;
    }
    Ok(())
}
