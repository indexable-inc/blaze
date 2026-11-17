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
/// Memory accesses: width, size, and the scaled offset's class.
const ACCESSES: [(u8, Size, ImmClass, u8); 4] = [
    (8, Size::B, ImmClass::U12Scaled0, 0),
    (16, Size::H, ImmClass::U12Scaled1, 1),
    (32, Size::W, ImmClass::U12Scaled2, 2),
    (64, Size::X, ImmClass::U12Scaled3, 3),
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
    divisions(each)?;
    shifts(each)?;
    compares(each)?;
    conversions(each)?;
    memory(each)?;
    constants_and_sites(each)?;
    floats(each)
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

/// Loads and stores at an immediate offset (unscaled `S9`, scaled `U12`), of a stack slot fused
/// with its `slot_addr` (a `Frame` field off `sp`), and `slot_addr` itself.
fn memory(each: &mut Each<'_>) -> Result<(), GenError> {
    for (w, size, scaled, scale) in ACCESSES {
        let p = pattern(Op::Load, w, 0, GPR, [GPR, Form::Imm(ImmClass::S9), NONE, NONE]);
        one(each, p, &[reg(0), imm(1, 9), reg(RES)], &|v, c| c.word(enc::ldur(size, r(v, RES), r(v, 0), s(v, 1, 9))))?;
        let p = pattern(Op::Load, w, 0, GPR, [GPR, Form::Imm(scaled), NONE, NONE]);
        one(each, p, &[reg(0), imm(1, 12), reg(RES)], &|v, c| c.word(enc::ldr(size, r(v, RES), r(v, 0), u(v, 1, 12))))?;
        let p = pattern(Op::Store, w, 0, NONE, [GPR, GPR, Form::Imm(ImmClass::S9), NONE]);
        one(each, p, &[reg(0), reg(1), imm(2, 9)], &|v, c| c.word(enc::stur(size, r(v, 0), r(v, 1), s(v, 2, 9))))?;
        let p = pattern(Op::Store, w, 0, NONE, [GPR, GPR, Form::Imm(scaled), NONE]);
        one(each, p, &[reg(0), reg(1), imm(2, 12)], &|v, c| c.word(enc::str(size, r(v, 0), r(v, 1), u(v, 2, 12))))?;

        let p = Pattern {
            op: Op::Load,
            width: w,
            variant: 0,
            result: GPR,
            operands: [NONE, NONE, SLOT, NONE],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 0 }),
        };
        one(each, p, &[frame(2, scale), reg(RES)], &|v, c| c.word(enc::ldr(size, r(v, RES), enc::SP, u(v, 2, 12))))?;
        let p = Pattern {
            op: Op::Store,
            width: w,
            variant: 0,
            result: NONE,
            operands: [GPR, NONE, NONE, SLOT],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 1 }),
        };
        one(each, p, &[reg(0), frame(3, scale)], &|v, c| c.word(enc::str(size, r(v, 0), enc::SP, u(v, 3, 12))))?;
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
