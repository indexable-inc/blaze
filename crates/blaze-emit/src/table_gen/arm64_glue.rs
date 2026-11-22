//! The arm64 recipes of the composer's own instructions (compose/emit.rs, `glue`), and the
//! single-word forms T1 left out: the frame steps (`stack_pointer` variants 1 to 7), register
//! moves within and across the classes (a near cell's load and store of either class is the
//! access grid's, arm64.rs `memory`), `ret`, the
//! poll, `eor` by a logical immediate, shifted-register operands, `cbz`, and the far frame (the
//! stack probe, `stack_pointer` variant 8, and the cell and slot forms past the near reach).
//!
//! A child of arm64.rs (its `#[path]` line there), so it shares that file's helpers; its
//! [`recipes`] is called at the end of arm64.rs's, so the generator, the encoder-agreement test
//! and the planted control in tests.rs walk these recipes too. The composer's patterns are taken
//! from `glue` itself: one home for what the composer looks up and what the table serves.
//!
//! Where enc.rs has no encoder for an instruction, the word is written here by the Arm A64
//! instruction set's encoding, stated beside it (fields as `name hi:lo`); the tests below check
//! each such word against a second encoder built field by field, and against words read from the
//! Arm manual's examples.
//!
//! Not here, by name: `shl` by an immediate. Its one-word form is `ubfm xd, xn, #(64 - s),
//! #(63 - s)`, whose fields are functions of `s`, not bits of it; piece 0's `Field` copies operand
//! bits only, so it needs a field transform in piece 0. (`movz t, #s; lslv` would hold `s`
//! linearly, but that is the code the composer already makes from a register amount.)

use blaze_lir::Op;
use blaze_tier::{Form, ImmClass, Inner, Pattern, Reg, RegClass, RegSet};

use super::{BLOCK, Each, GPR, NONE, WIDTHS, branch, imm, narrow, one, pattern, r, reg, s, u};
use crate::compose::emit::glue;
use crate::enc::{self, Cond, FP, LR, SP, Size, X16, ZR};
use crate::enc_probe;
use crate::table_gen::{GenError, RES, Recipe, Slot, SlotKind, T0, Vals};

/// The variant of an unfused `br` that branches when its condition is ZERO (`cbz`): the else
/// edge, so the composer can invert an unfused branch. Variant 0 (`cbnz`) is the then edge
/// (mod.rs's conventions).
pub(crate) const BR_IF_ZERO: u8 = 1;

/// Every recipe of this file, handed to `each` in a fixed order (the table sorts them).
pub(super) fn recipes(each: &mut Each<'_>) -> Result<(), GenError> {
    frame_steps(each)?;
    moves(each)?;
    // A cell's near load and store (`glue::cell_load`, `glue::cell_store`) of either class are
    // the access grid's fused `slot_addr` cells (arm64.rs `memory`): one generator a pattern.
    poll_and_return(each)?;
    xor_immediates(each)?;
    shifted_operands(each)?;
    branch_if_zero(each)?;
    far_frame(each)
}

// ---- words enc.rs has no encoder for ----------------------------------------------------------

/// FMOV (register), double: `M 31 = 0, 0 30, S 29 = 0, 11110 28:24, ftype 23:22 = 01, 1 21,
/// 0000 20:17, opc 16:15 = 00, 10000 14:10, Rn 9:5, Rd 4:0`.
pub(super) const fn fmov_d(rd: u32, rn: u32) -> u32 {
    0x1e60_4000 | (rn << 5) | rd
}

/// LDR (immediate, SIMD&FP), unsigned offset, 64-bit: `size 31:30 = 11, 111 29:27, V 26 = 1,
/// 01 25:24, opc 23:22 = 01, imm12 21:10 (the offset / 8), Rn 9:5, Rt 4:0`.
pub(super) const fn ldr_d(rt: u32, rn: u32, imm12: u32) -> u32 {
    0xfd40_0000 | (imm12 << 10) | (rn << 5) | rt
}

/// STR (immediate, SIMD&FP), unsigned offset, 64-bit: as [`ldr_d`] with `opc 23:22 = 00`.
pub(super) const fn str_d(rt: u32, rn: u32, imm12: u32) -> u32 {
    0xfd00_0000 | (imm12 << 10) | (rn << 5) | rt
}

/// STP (SIMD&FP), pre-index, 64-bit: `opc 31:30 = 01, 101 29:27, V 26 = 1, 011 25:23, L 22 = 0,
/// imm7 21:15 (the signed offset / 8), Rt2 14:10, Rn 9:5, Rt 4:0`.
pub(super) const fn stp_d_pre(rt: u32, rt2: u32, rn: u32, off: i32) -> u32 {
    0x6d80_0000 | (((off / 8).cast_unsigned() & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// LDP (SIMD&FP), post-index, 64-bit: `opc 31:30 = 01, 101 29:27, V 26 = 1, 001 25:23, L 22 = 1,
/// imm7 21:15 (the signed offset / 8), Rt2 14:10, Rn 9:5, Rt 4:0`.
pub(super) const fn ldp_d_post(rt: u32, rt2: u32, rn: u32, off: i32) -> u32 {
    0x6cc0_0000 | (((off / 8).cast_unsigned() & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// EOR (immediate), 64-bit: `sf 31 = 1, opc 30:29 = 10, 100100 28:23, N 22, immr 21:16,
/// imms 15:10, Rn 9:5, Rd 4:0`.
pub(super) const fn eor_imm(rd: u32, rn: u32, n: u32, immr: u32, imms: u32) -> u32 {
    0xd200_0000 | (n << 22) | (immr << 16) | (imms << 10) | (rn << 5) | rd
}

/// The shift of a shifted-register operand: `shift 23:22`, `00` LSL, `01` LSR, `10` ASR.
pub(super) const fn shift_code(op: Op) -> u32 {
    match op {
        Op::Lshr => 0b01,
        Op::Ashr => 0b10,
        _ => 0b00,
    }
}

/// ADD/SUB (shifted register), 64-bit (`sf 31 = 1, op 30, S 29 = 0, 01011 28:24, shift 23:22,
/// 0 21, Rm 20:16, imm6 15:10, Rn 9:5, Rd 4:0`) and AND/ORR/EOR (shifted register), 64-bit
/// (`sf 31 = 1, opc 30:29, 01010 28:24, shift 23:22, N 21 = 0, Rm, imm6, Rn, Rd`): `word` is
/// enc.rs's unshifted form (`shift` and `imm6` zero), the shift and its amount set here.
pub(super) const fn shifted(word: u32, shift: u32, amount: u32) -> u32 {
    word | (shift << 22) | (amount << 10)
}

// ---- the recipes ------------------------------------------------------------------------------

/// The frame steps: ENTER (`stp x29, x30, [sp, #-16]!; mov x29, sp`, the `mov` being `add x29,
/// sp, #0`: `orr` would read `xzr`), SAVE_PAIR and RESTORE_PAIR of `x` or `d` registers, SUB_SP,
/// SUB_SP_HI and SP_FROM_FP (`sub sp, sp|x29, #imm{, lsl #12}`), and LEAVE.
fn frame_steps(each: &mut Each<'_>) -> Result<(), GenError> {
    one(each, glue::frame(glue::ENTER, [NONE; 4]), &[], &|_, c| {
        c.word(enc::stp_pre(FP, LR, SP, -16));
        c.word(enc::add_imm(FP, SP, 0, false));
    })?;
    for class in [RegClass::Gpr, RegClass::Fpr] {
        let f = Form::Reg(class);
        one(each, glue::frame(glue::SAVE_PAIR, [f, f, NONE, NONE]), &[reg(0), reg(1)], &|v, c| {
            let (a, b) = (r(v, 0), r(v, 1));
            c.word(match class {
                RegClass::Gpr => enc::stp_pre(a, b, SP, -16),
                RegClass::Fpr => stp_d_pre(a, b, SP, -16),
            });
        })?;
        one(each, glue::frame(glue::RESTORE_PAIR, [f, f, NONE, NONE]), &[reg(0), reg(1)], &|v, c| {
            let (a, b) = (r(v, 0), r(v, 1));
            c.word(match class {
                RegClass::Gpr => enc::ldp_post(a, b, SP, 16),
                RegClass::Fpr => ldp_d_post(a, b, SP, 16),
            });
        })?;
    }
    let subs = [
        (glue::SUB_SP, ImmClass::U12, false, SP),
        (glue::SUB_SP_HI, ImmClass::U12Lsl12, true, SP),
        (glue::SP_FROM_FP, ImmClass::U12, false, FP),
    ];
    for (step, class, lsl12, from) in subs {
        let p = glue::frame(step, [Form::Imm(class), NONE, NONE, NONE]);
        one(each, p, &[imm(0, 12)], &|v, c| c.word(enc::sub_imm(SP, from, u(v, 0, 12), lsl12)))?;
    }
    one(each, glue::frame(glue::LEAVE, [NONE; 4]), &[], &|_, c| c.word(enc::ldp_post(FP, LR, SP, 16)))
}

/// A block parameter's move within a class (`mov`, `fmov d, d`) and 64 bits between the classes
/// (`fmov d, x`; `fmov x, d`).
fn moves(each: &mut Each<'_>) -> Result<(), GenError> {
    let slots = [reg(0), reg(RES)];
    one(each, glue::mov(RegClass::Gpr), &slots, &|v, c| c.word(enc::mov(r(v, RES), r(v, 0))))?;
    one(each, glue::mov(RegClass::Fpr), &slots, &|v, c| c.word(fmov_d(r(v, RES), r(v, 0))))?;
    one(each, glue::cross(true), &slots, &|v, c| c.word(enc::fmov_to_d(r(v, RES), r(v, 0))))?;
    one(each, glue::cross(false), &slots, &|v, c| c.word(enc::fmov_from_d(r(v, RES), r(v, 0))))
}

/// The poll (`adrp x16, #0; ldr xzr, [x16, #0]`: no fields, the composer writes its `Page21` and
/// `PageOff12 { scale: 3 }` relocations; it writes x16) and `ret`.
fn poll_and_return(each: &mut Each<'_>) -> Result<(), GenError> {
    each(&Recipe {
        pattern: glue::poll(),
        slots: &[],
        site: None,
        clobbers: RegSet::EMPTY.with(Reg(16)),
        insn_bytes: 4,
        encode: &|_, c| {
            c.word(enc::adrp(X16, 0));
            c.word(enc::ldr(Size::X, ZR, X16, 0));
        },
    })?;
    one(each, glue::ret(), &[], &|_, c| c.word(enc::ret()))
}

/// `xor` by a logical immediate, held as `N:immr:imms` (13 bits) as `and`'s and `or`'s are
/// (arm64.rs): a zero-extended input and constant give a zero-extended result at every width.
fn xor_immediates(each: &mut Each<'_>) -> Result<(), GenError> {
    for w in WIDTHS {
        let p = pattern(Op::Xor, w, 0, GPR, [GPR, Form::Imm(ImmClass::Logical64), NONE, NONE]);
        one(each, p, &[reg(0), imm(1, 13), reg(RES)], &|v, c| {
            let (a, d, k) = (r(v, 0), r(v, RES), u(v, 1, 13));
            c.word(eor_imm(d, a, k >> 12 & 1, k >> 6 & 0x3f, k & 0x3f));
        })?;
    }
    Ok(())
}

/// `add`, `sub`, `and`, `or`, `xor` whose second operand is a fused shift by an immediate (piece
/// 0's `Inner`: "a shifted operand"): `[a, None, b, Imm(amount)]` with `inner` the shift at
/// operand 1, its operands after the outer two. The amount is below the width (`Shift5` at 32,
/// `Shift6` at 64; mod.rs: the composer reduces it). `lshr` reads a zero-extended input, so the
/// 64-bit `lsr` is right at 32; `ashr` would need the sign first, so it is fused at 64 only. The
/// result is narrowed where its high bits can be set: a sum or difference, or an `or`/`xor` with
/// a left shift.
fn shifted_operands(each: &mut Each<'_>) -> Result<(), GenError> {
    for shift in [Op::Shl, Op::Lshr, Op::Ashr] {
        for (w, class, bits) in [(32u8, ImmClass::Shift5, 5u8), (64, ImmClass::Shift6, 6)] {
            if shift == Op::Ashr && w < 64 {
                continue;
            }
            for op in [Op::Add, Op::Sub, Op::And, Op::Or, Op::Xor] {
                let p = Pattern {
                    op,
                    width: w,
                    variant: 0,
                    result: GPR,
                    operands: [GPR, NONE, GPR, Form::Imm(class)],
                    inner: Some(Inner { op: shift, variant: 0, operand: 1 }),
                };
                let high = match op {
                    Op::Add | Op::Sub => true,
                    Op::Or | Op::Xor => shift == Op::Shl,
                    _ => false,
                };
                one(each, p, &[reg(0), reg(2), imm(3, bits), reg(RES)], &|v, c| {
                    let (a, b, d, k) = (r(v, 0), r(v, 2), r(v, RES), u(v, 3, u32::from(bits)));
                    let word = match op {
                        Op::Add => enc::add(d, a, b),
                        Op::Sub => enc::sub(d, a, b),
                        Op::And => enc::and(d, a, b),
                        Op::Or => enc::orr(d, a, b),
                        _ => enc::eor(d, a, b),
                    };
                    c.word(shifted(word, shift_code(shift), k));
                    if high {
                        narrow(c, d, w);
                    }
                })?;
            }
        }
    }
    Ok(())
}

/// `br` of a value's else edge: `cbz` (variant [`BR_IF_ZERO`]).
fn branch_if_zero(each: &mut Each<'_>) -> Result<(), GenError> {
    let p = pattern(Op::Br, 1, BR_IF_ZERO, NONE, [GPR, BLOCK, NONE, NONE]);
    one(each, p, &[reg(0), branch(1, 19)], &|v, c| c.word(enc::cbz(r(v, 0), s(v, 1, 19))))
}

/// A frame offset of `bits` bits in units of `1 << scale` bytes: a far template splits it over
/// two instructions (`add .., #hi, lsl #12` and the access at `#lo`), and the generator finds the
/// two fields, `from_bit` 0 and the low part's width, by its flips like any other.
const fn far(operand: u8, scale: u8, bits: u8) -> Slot {
    Slot { operand, kind: SlotKind::Frame { scale }, bits }
}

/// The frame past the near forms' reach (compose/emit.rs, `glue::PROBE` and `glue::FAR`):
///
/// - the stack probe, pass/probe.rs's sequence with its count in TEMP0 (a lent scratch register,
///   never x17 by name: the composer's pool hands it out) and no tail branch, as it runs in line;
/// - a cell's load and store of a general or float register past 32 KiB: `add t, sp, #(u >> 9),
///   lsl #12` and the access at `#(u & 0x1ff)` scaled by 8 (`u` the offset in 8-byte units, 21
///   bits: up to 16 MiB), `t` the result itself for a general load, else TEMP0;
/// - a slot's address past 4 KiB: `add RESULT, sp, #(f >> 12), lsl #12; add RESULT, RESULT,
///   #(f & 0xfff)` (24 bits of bytes).
fn far_frame(each: &mut Each<'_>) -> Result<(), GenError> {
    let probe = glue::frame(glue::PROBE, [Form::Slot, NONE, NONE, NONE]);
    one(each, probe, &[far(0, 0, 24), reg(T0)], &|v, c| {
        let (f, t) = (u(v, 0, 24), r(v, T0));
        c.word(enc::movz(t, f & 0xffff, 0));
        c.word(enc::movk(t, f >> 16, 1));
        // loop: one step down, touched; then the count down by one step while a whole step is
        // left (`b.hs`, pass/probe.rs's doc says why not `b.hi`); then the rest.
        c.word(enc::sub_imm(SP, SP, 1, true));
        c.word(enc::str(Size::X, ZR, SP, 0));
        c.word(enc::sub_imm(t, t, 1, true));
        c.word(enc_probe::cmp_imm_lsl12(t, 1));
        c.word(enc::b_cond(Cond::Hs, -4));
        c.word(enc_probe::sub_ext_uxtx(SP, SP, t));
    })?;
    let hi = |v: &Vals, k: u8| u(v, k, 21) >> 9;
    let lo = |v: &Vals, k: u8| u(v, k, 21) & 0x1ff;
    one(each, glue::cell_load_far(RegClass::Gpr), &[far(2, 3, 21), reg(RES)], &|v, c| {
        let d = r(v, RES);
        c.word(enc::add_imm(d, SP, hi(v, 2), true));
        c.word(enc::ldr(Size::X, d, d, lo(v, 2)));
    })?;
    one(each, glue::cell_load_far(RegClass::Fpr), &[far(2, 3, 21), reg(RES), reg(T0)], &|v, c| {
        let t = r(v, T0);
        c.word(enc::add_imm(t, SP, hi(v, 2), true));
        c.word(ldr_d(r(v, RES), t, lo(v, 2)));
    })?;
    for class in [RegClass::Gpr, RegClass::Fpr] {
        one(each, glue::cell_store_far(class), &[reg(0), far(3, 3, 21), reg(T0)], &|v, c| {
            let t = r(v, T0);
            c.word(enc::add_imm(t, SP, hi(v, 3), true));
            c.word(match class {
                RegClass::Gpr => enc::str(Size::X, r(v, 0), t, lo(v, 3)),
                RegClass::Fpr => str_d(r(v, 0), t, lo(v, 3)),
            });
        })?;
    }
    one(each, glue::slot_address_far(), &[far(0, 0, 24), reg(RES)], &|v, c| {
        let (d, f) = (r(v, RES), u(v, 0, 24));
        c.word(enc::add_imm(d, SP, f >> 12, true));
        c.word(enc::add_imm(d, d, f & 0xfff, false));
    })
}

#[cfg(test)]
mod tests {
    //! The glue recipes' tests: every composer pattern is in the generated table; each word this
    //! file writes by the A64 rule equals a second encoder built field by field from the same
    //! rule; and the table's templates, filled, give the words of the Arm manual's examples.

    use blaze_lir::{IntCond, Op};
    use blaze_own::{AVec, Alloc, Vec};
    use blaze_sim::{Broke, Config, Tape};
    use blaze_tier::{Arch, Form, ImmClass, Inner, Machine, Os, Pattern, RegClass, Table};

    use super::{BR_IF_ZERO, eor_imm, fmov_d, ldp_d_post, ldr_d, shift_code, shifted, stp_d_pre, str_d};
    use crate::compose::emit::glue;
    use crate::enc;
    use crate::table_gen::{EncodedTable, OPERANDS, RES, T0, Vals, fill, generate};

    const MACHINE: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: 0 };
    const N: Form = Form::None;
    const G: Form = Form::Reg(RegClass::Gpr);
    const F: Form = Form::Reg(RegClass::Fpr);

    fn table_bytes() -> AVec<'static, u8> {
        let mut out = Vec::new_in(Alloc::global());
        if let Err(error) = generate(MACHINE, &mut out, Alloc::global()) {
            panic!("the arm64 table was not generated: {error}");
        }
        out
    }

    /// A word from `(value, low bit, width)` fields: the A64 rule written out, independent of the
    /// hex constants in the encoders it checks.
    fn rule(fields: &[(u32, u32, u32)]) -> u32 {
        let mut word = 0u32;
        for &(value, at, width) in fields {
            let mask = if width >= 32 { u32::MAX } else { (1u32 << width) - 1 };
            word |= (value & mask) << at;
        }
        word
    }

    /// Every pattern compose/emit.rs's `glue` looks up is served by the generated table.
    /// Expected: prints "glue: 43 composer patterns in the table" and passes. Fails with the first
    /// pattern missing (the composer's `TableError::Missing`) if a recipe is dropped.
    #[test]
    fn every_composer_pattern_is_in_the_table() {
        let bytes = table_bytes();
        let table = match EncodedTable::read(&bytes, Alloc::global()) {
            Ok(table) => table,
            Err(error) => panic!("the generated table does not read back: {error}"),
        };
        let u12 = [Form::Imm(ImmClass::U12), N, N, N];
        let mut wanted: std::vec::Vec<Pattern> = std::vec![
            glue::frame(glue::ENTER, [N; 4]),
            glue::frame(glue::SAVE_PAIR, [G, G, N, N]),
            glue::frame(glue::SAVE_PAIR, [F, F, N, N]),
            glue::frame(glue::SUB_SP, u12),
            glue::frame(glue::SUB_SP_HI, [Form::Imm(ImmClass::U12Lsl12), N, N, N]),
            glue::frame(glue::SP_FROM_FP, u12),
            glue::frame(glue::RESTORE_PAIR, [G, G, N, N]),
            glue::frame(glue::RESTORE_PAIR, [F, F, N, N]),
            glue::frame(glue::LEAVE, [N; 4]),
            glue::mov(RegClass::Gpr),
            glue::mov(RegClass::Fpr),
            glue::cross(true),
            glue::cross(false),
            glue::cell_load(RegClass::Gpr),
            glue::cell_load(RegClass::Fpr),
            glue::cell_store(RegClass::Gpr),
            glue::cell_store(RegClass::Fpr),
            glue::ret(),
            glue::poll(),
            glue::jump(),
            glue::branch(),
            glue::call(),
            glue::call_indirect(),
            glue::address(Op::FuncAddr),
            glue::address(Op::DataAddr),
            glue::slot_address(),
            glue::compare_branch(64, IntCond::ALL[0], G),
            glue::compare_branch(32, IntCond::ALL[0], Form::Imm(ImmClass::U12)),
            glue::frame(glue::PROBE, [Form::Slot, N, N, N]),
            glue::cell_load_far(RegClass::Gpr),
            glue::cell_load_far(RegClass::Fpr),
            glue::cell_store_far(RegClass::Gpr),
            glue::cell_store_far(RegClass::Fpr),
            glue::slot_address_far(),
        ];
        for width in [1u8, 8, 16, 32] {
            wanted.push(glue::narrow(width));
        }
        for from in [1u8, 8, 16] {
            wanted.push(glue::sext32(from));
        }
        wanted.push(Pattern {
            op: Op::Br,
            width: 1,
            variant: BR_IF_ZERO,
            result: N,
            operands: [G, Form::Block, N, N],
            inner: None,
        });
        wanted.push(Pattern {
            op: Op::Xor,
            width: 64,
            variant: 0,
            result: G,
            operands: [G, Form::Imm(ImmClass::Logical64), N, N],
            inner: None,
        });
        if let Some(missing) = wanted.iter().find(|p| table.find(p).is_none()) {
            panic!("the table lacks {missing:?}");
        }
        std::println!("glue: {} composer patterns in the table", wanted.len());
    }

    /// Each word this file writes by the A64 rule equals the rule built field by field, for random
    /// operands. Expected: passes. Fails if a fixed bit of an encoder's constant is wrong (a
    /// class, size, opc or index bit), or a field sits at the wrong position.
    #[test]
    fn rule_words_equal_a_second_encoder() {
        blaze_sim::check(Config::cases(256), |tape: &Tape| {
            let next = |bits: u32| -> u32 {
                // Truncation on purpose: at most 13 bits are kept.
                (tape.next_u64("a field's value") & ((1u64 << bits) - 1)) as u32
            };
            let (d, n, m, k12, k6, k13) = (next(5), next(5), next(5), next(12), next(6), next(13));
            let regs = |extra: &[(u32, u32, u32)]| {
                let mut all = std::vec![(d, 0, 5), (n, 5, 5)];
                all.extend_from_slice(extra);
                rule(&all)
            };
            let checks: [(&str, u32, u32); 7] = [
                ("fmov d, d", fmov_d(d, n), regs(&[(0b1_0000, 10, 5), (1, 21, 1), (0b01, 22, 2), (0b11110, 24, 5)])),
                ("ldr d", ldr_d(d, n, k12), regs(&[(k12, 10, 12), (0b01, 22, 2), (0b01, 24, 2), (1, 26, 1), (0b111, 27, 3), (0b11, 30, 2)])),
                ("str d", str_d(d, n, k12), regs(&[(k12, 10, 12), (0b00, 22, 2), (0b01, 24, 2), (1, 26, 1), (0b111, 27, 3), (0b11, 30, 2)])),
                (
                    "stp d pre",
                    stp_d_pre(d, m, n, -16),
                    regs(&[(m, 10, 5), (0x7e, 15, 7), (0, 22, 1), (0b011, 23, 3), (1, 26, 1), (0b101, 27, 3), (0b01, 30, 2)]),
                ),
                (
                    "ldp d post",
                    ldp_d_post(d, m, n, 16),
                    regs(&[(m, 10, 5), (2, 15, 7), (1, 22, 1), (0b001, 23, 3), (1, 26, 1), (0b101, 27, 3), (0b01, 30, 2)]),
                ),
                (
                    "eor imm",
                    eor_imm(d, n, k13 >> 12 & 1, k13 >> 6 & 0x3f, k13 & 0x3f),
                    regs(&[(k13 & 0x3f, 10, 6), (k13 >> 6 & 0x3f, 16, 6), (k13 >> 12, 22, 1), (0b100100, 23, 6), (0b10, 29, 2), (1, 31, 1)]),
                ),
                (
                    "add lsr",
                    shifted(enc::add(d, n, m), shift_code(Op::Lshr), k6),
                    regs(&[(k6, 10, 6), (m, 16, 5), (0b01, 22, 2), (0b01011, 24, 5), (1, 31, 1)]),
                ),
            ];
            for (name, got, want) in checks {
                if got != want {
                    return Err(Broke::Oracle(format!("{name}: {got:#010x}, the rule gives {want:#010x}")));
                }
            }
            // The logical shifted forms: opc at 30:29, `01010` at 28:24.
            for (opc, word) in [(0b00, enc::and(d, n, m)), (0b01, enc::orr(d, n, m)), (0b10, enc::eor(d, n, m))] {
                let got = shifted(word, shift_code(Op::Ashr), k6);
                let want = regs(&[(k6, 10, 6), (m, 16, 5), (0b10, 22, 2), (0b01010, 24, 5), (opc, 29, 2), (1, 31, 1)]);
                if got != want {
                    return Err(Broke::Oracle(format!("logical opc {opc:#b} asr: {got:#010x}, the rule gives {want:#010x}")));
                }
            }
            Ok(())
        });
    }

    /// One template filled, against words from the Arm manual's examples.
    fn filled(table: &EncodedTable<'_>, pattern: &Pattern, vals: &Vals) -> std::vec::Vec<u32> {
        let Some(template) = table.find(pattern) else {
            panic!("the table lacks {pattern:?}");
        };
        let mut code = template.bytes.to_vec();
        fill(&mut code, template.fields, vals);
        code.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
    }

    /// `vals` with `(operand, value)` set.
    fn with(set: &[(u8, u64)]) -> Vals {
        let mut vals = [0u64; OPERANDS];
        for &(at, value) in set {
            vals[usize::from(at)] = value;
        }
        vals
    }

    /// The table's templates, filled, are the words the Arm A64 manual gives for these
    /// instructions (each written beside it). Expected: passes. Fails if a template's fixed bits
    /// or a field are wrong, whatever the recipe's encoder says.
    #[test]
    fn templates_are_the_manuals_words() {
        let bytes = table_bytes();
        let table = match EncodedTable::read(&bytes, Alloc::global()) {
            Ok(table) => table,
            Err(error) => panic!("the generated table does not read back: {error}"),
        };
        let shl_add = Pattern {
            op: Op::Add,
            width: 64,
            variant: 0,
            result: G,
            operands: [G, N, G, Form::Imm(ImmClass::Shift6)],
            inner: Some(Inner { op: Op::Shl, variant: 0, operand: 1 }),
        };
        let xor_imm = Pattern {
            op: Op::Xor,
            width: 64,
            variant: 0,
            result: G,
            operands: [G, Form::Imm(ImmClass::Logical64), N, N],
            inner: None,
        };
        let cbz = Pattern { op: Op::Br, width: 1, variant: BR_IF_ZERO, result: N, operands: [G, Form::Block, N, N], inner: None };
        let u12 = [Form::Imm(ImmClass::U12), N, N, N];
        let cases: [(&str, Pattern, Vals, &[u32]); 19] = [
            ("stp x29, x30, [sp, #-16]!; mov x29, sp", glue::frame(glue::ENTER, [N; 4]), with(&[]), &[0xa9bf_7bfd, 0x9100_03fd]),
            ("stp x19, x20, [sp, #-16]!", glue::frame(glue::SAVE_PAIR, [G, G, N, N]), with(&[(0, 19), (1, 20)]), &[0xa9bf_53f3]),
            ("stp d8, d9, [sp, #-16]!", glue::frame(glue::SAVE_PAIR, [F, F, N, N]), with(&[(0, 8), (1, 9)]), &[0x6dbf_27e8]),
            ("sub sp, sp, #16", glue::frame(glue::SUB_SP, u12), with(&[(0, 16)]), &[0xd100_43ff]),
            (
                "sub sp, sp, #1, lsl #12",
                glue::frame(glue::SUB_SP_HI, [Form::Imm(ImmClass::U12Lsl12), N, N, N]),
                with(&[(0, 1)]),
                &[0xd140_07ff],
            ),
            ("sub sp, x29, #16", glue::frame(glue::SP_FROM_FP, u12), with(&[(0, 16)]), &[0xd100_43bf]),
            ("ldp x19, x20, [sp], #16", glue::frame(glue::RESTORE_PAIR, [G, G, N, N]), with(&[(0, 19), (1, 20)]), &[0xa8c1_53f3]),
            ("ldp d8, d9, [sp], #16", glue::frame(glue::RESTORE_PAIR, [F, F, N, N]), with(&[(0, 8), (1, 9)]), &[0x6cc1_27e8]),
            ("ldp x29, x30, [sp], #16", glue::frame(glue::LEAVE, [N; 4]), with(&[]), &[0xa8c1_7bfd]),
            ("mov x0, x1", glue::mov(RegClass::Gpr), with(&[(0, 1), (RES, 0)]), &[0xaa01_03e0]),
            ("fmov d0, d1", glue::mov(RegClass::Fpr), with(&[(0, 1), (RES, 0)]), &[0x1e60_4020]),
            ("fmov d0, x1", glue::cross(true), with(&[(0, 1), (RES, 0)]), &[0x9e67_0020]),
            ("fmov x0, d1", glue::cross(false), with(&[(0, 1), (RES, 0)]), &[0x9e66_0020]),
            ("ldr d0, [sp, #8]", glue::cell_load(RegClass::Fpr), with(&[(2, 1), (RES, 0)]), &[0xfd40_07e0]),
            ("str d0, [sp, #8]", glue::cell_store(RegClass::Fpr), with(&[(0, 0), (3, 1)]), &[0xfd00_07e0]),
            ("ret", glue::ret(), with(&[]), &[0xd65f_03c0]),
            ("adrp x16, #0; ldr xzr, [x16]", glue::poll(), with(&[]), &[0x9000_0010, 0xf940_021f]),
            ("add x0, x1, x2, lsl #3", shl_add, with(&[(0, 1), (2, 2), (3, 3), (RES, 0)]), &[0x8b02_0c20]),
            ("eor x0, x1, #1", xor_imm, with(&[(0, 1), (1, 1 << 12), (RES, 0)]), &[0xd240_0020]),
        ];
        for (name, pattern, vals, want) in cases.iter() {
            let got = filled(&table, pattern, vals);
            assert_eq!(&got[..], *want, "{name}: the template gives {got:08x?}");
        }
        let got = filled(&table, &cbz, &with(&[(0, 0), (1, 2)]));
        assert_eq!(&got[..], &[0xb400_0040], "cbz x0, #8: the template gives {got:08x?}");
        // The far forms: one offset split over two words. A cell at 8-byte unit 4097 (byte
        // 32776, one unit past the near reach) is `#8, lsl #12` (8 << 9 units) and `#1`.
        let far = [
            ("add x16, sp, #8, lsl #12; str x0, [x16, #8]", glue::cell_store_far(RegClass::Gpr), with(&[(0, 0), (3, 4097), (T0, 16)]), [0x9140_23f0u32, 0xf900_0600]),
            ("add x1, sp, #8, lsl #12; ldr x1, [x1, #8]", glue::cell_load_far(RegClass::Gpr), with(&[(2, 4097), (RES, 1)]), [0x9140_23e1, 0xf940_0421]),
            ("add x0, sp, #1, lsl #12; add x0, x0, #16", glue::slot_address_far(), with(&[(0, 4096 + 16), (RES, 0)]), [0x9140_07e0, 0x9100_4000]),
        ];
        for (name, pattern, vals, want) in far.iter() {
            let got = filled(&table, pattern, vals);
            assert_eq!(&got[..], &want[..], "{name}: the template gives {got:08x?}");
        }
        // The probe: pass/probe.rs's loop and rest with the count in x16, for a 1 MiB frame.
        let got = filled(&table, &glue::frame(glue::PROBE, [Form::Slot, N, N, N]), &with(&[(0, 1 << 20), (T0, 16)]));
        let want = [
            enc::movz(16, 0, 0),
            enc::movk(16, 16, 1),
            enc::sub_imm(enc::SP, enc::SP, 1, true),
            enc::str(enc::Size::X, enc::ZR, enc::SP, 0),
            enc::sub_imm(16, 16, 1, true),
            crate::enc_probe::cmp_imm_lsl12(16, 1),
            enc::b_cond(enc::Cond::Hs, -4),
            crate::enc_probe::sub_ext_uxtx(enc::SP, enc::SP, 16),
        ];
        assert_eq!(&got[..], &want[..], "the probe template gives {got:08x?}");
    }
}
