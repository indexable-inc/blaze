//! The T32 (Thumb) encoder.
//!
//! Thumb is variable length: most instructions are one halfword, but the
//! Thumb-2 additions are two, and many operations exist in both widths. Where
//! both are possible this returns the 16-bit encoding first and the 32-bit one
//! second, and lets the layout pass raise the choice when a branch turns out
//! not to reach. Everything else picks a width here, because the choice
//! depends on the operands rather than on an address.
//!
//! A 32-bit Thumb instruction is *two little-endian halfwords*, not one
//! little-endian word, so it is assembled here as `(hw2 << 16) | hw1` and
//! written out as four bytes — which is also the order a scatter function
//! sees, since the core reads the field back as a little-endian integer.

use super::imm;
use super::insn::{AL, Mnem, Transfer, Width};
use super::operand::{Index, Mem, MemOffset, Operand, OperandKind, Shift, ShiftAmt};
use super::reg::{self, Reg};
use super::{Insn, encode, reloc};
use crate::arch::{ArchState, AsmCtx, Request};
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, LinkValue, Variant};
use crate::source::Span;

pub const NOP: u16 = 0xbf00;

/// The 32-bit `nop.w`, as its two halfwords; GNU as pads a Thumb-2
/// alignment with these.
pub const WIDE_NOP: (u16, u16) = (0xf3af, 0x8000);

fn low(r: Reg) -> bool {
    r < 8
}

/// GNU as's `reject_bad_reg`: an ordinary register operand of a 32-bit Thumb
/// instruction is neither the stack pointer nor the PC. The exceptions --
/// `add` and `sub` with the stack pointer, `mov`, `cmp`, and the word-sized
/// loads and stores -- check for themselves.
fn bad_reg(cx: &mut AsmCtx<'_>, span: Span, r: Reg) -> Option<()> {
    if r == reg::SP || r == reg::PC {
        cx.error(
            span,
            format!("`{}` is not allowed here in Thumb", reg::name_of(r)),
        );
        return None;
    }
    Some(())
}

/// The same, for a register that may still be the PC.
fn not_pc(cx: &mut AsmCtx<'_>, span: Span, r: Reg) -> Option<()> {
    if r == reg::PC {
        cx.error(span, "`pc` is not allowed here");
        return None;
    }
    Some(())
}

/// Every register an operand names, for the checks above: a shifted register
/// hides one inside it.
fn operand_regs(op: &Operand) -> Vec<Reg> {
    match op.kind {
        OperandKind::Reg(r) => vec![r],
        OperandKind::Shifted { rm, amount, .. } => match amount {
            ShiftAmt::Reg(rs) => vec![rm, rs],
            _ => vec![rm],
        },
        _ => vec![],
    }
}

/// Rejects the stack pointer and the PC in every register an operand names.
fn bad_regs(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<()> {
    for r in operand_regs(op) {
        bad_reg(cx, op.span, r)?;
    }
    Some(())
}

fn narrow(w: u16) -> Vec<Variant> {
    vec![Variant::new(w.to_le_bytes().to_vec())]
}

fn wide_bytes(hw1: u16, hw2: u16) -> Vec<u8> {
    let mut v = hw1.to_le_bytes().to_vec();
    v.extend_from_slice(&hw2.to_le_bytes());
    v
}

fn wide(hw1: u16, hw2: u16) -> Vec<Variant> {
    vec![Variant::new(wide_bytes(hw1, hw2))]
}

/// Rejects a condition suffix on anything but a branch outside an `it` block:
/// predication in Thumb comes from the block. (Inside one, the condition has
/// already been checked against the block's and taken off.)
fn unconditional(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<()> {
    if ins.cond_written && ins.cond != AL {
        cx.error(
            ins.span,
            format!(
                "`{}` is conditional, which in Thumb takes an `it` block",
                ins.text
            ),
        );
        return None;
    }
    Some(())
}

/// Whether a 16-bit data-processing form that sets the flags outside an `it`
/// block, and does not inside one, fits: `adds r0, r1, r2` outside a block
/// and `addeq r0, r1, r2` inside one are both 16 bits, while `add` outside
/// and `addseq` inside need 32.
fn sets_flags16(ins: &Insn<'_>) -> bool {
    ins.set_flags != ins.in_it
}

/// Where an `it` block's state lives in `ArchState::private`: the ITSTATE
/// byte, the condition of the next instruction in the top nibble and the
/// mask of the ones after it below.
const IT_SHIFT: u32 = 8;

pub(super) fn itstate(cx: &AsmCtx<'_>) -> u8 {
    (cx.state.private >> IT_SHIFT) as u8
}

fn set_itstate(cx: &mut AsmCtx<'_>, it: u8) {
    cx.state.private = (cx.state.private & !(0xff << IT_SHIFT)) | ((it as u64) << IT_SHIFT);
}

/// Where the block GNU as's `-mimplicit-it` makes up lives in the same word,
/// above the ITSTATE: GNU as's `now_pred` in its `AUTOMATIC_PRED_BLOCK`
/// state, less the pointer to the `it` instruction itself, which the core
/// keeps (see [`Request::ImplicitIt`]).
const AUTO_SHIFT: u32 = 16;
/// The twelve bits [`AUTO_SHIFT`] names, with the top one saying a block is
/// open at all.
const AUTO_BITS: u64 = 0xfff;
const AUTO_OPEN: u64 = 0x800;

/// An `it` block made up for a conditional instruction, as `now_pred` holds
/// one while it is open.
#[derive(Copy, Clone)]
struct Auto {
    /// `now_pred.cc`, the condition the block was opened with.
    cond: u8,
    /// `now_pred.mask`, the four bits its `it` instruction carries now.
    mask: u8,
    /// `now_pred.block_length`, how many instructions are in the block.
    len: u8,
}

fn auto(state: &ArchState) -> Option<Auto> {
    let bits = (state.private >> AUTO_SHIFT) & AUTO_BITS;
    (bits & AUTO_OPEN != 0).then_some(Auto {
        cond: (bits & 0xf) as u8,
        mask: ((bits >> 4) & 0xf) as u8,
        len: ((bits >> 8) & 7) as u8,
    })
}

fn set_auto(state: &mut ArchState, block: Option<Auto>) {
    let bits = match block {
        Some(a) => {
            AUTO_OPEN | u64::from(a.cond) | (u64::from(a.mask) << 4) | (u64::from(a.len) << 8)
        }
        None => 0,
    };
    state.private = (state.private & !(AUTO_BITS << AUTO_SHIFT)) | (bits << AUTO_SHIFT);
}

/// GNU as's `force_automatic_it_block_close`, which a label calls through
/// `arm_frob_label`: a label is a branch target, so a block nobody wrote
/// must not reach past it.
pub(super) fn close_implicit_it(state: &mut ArchState) {
    set_auto(state, None);
}

/// The halfword `output_it_inst` writes.
fn it_word(cond: u8, mask: u8) -> u16 {
    0xbf00 | (u16::from(cond) << 4) | u16::from(mask & 0xf)
}

/// Whether an instruction leaves its `it` block by changing the PC, which
/// only the last instruction of a block may do.
fn is_branch(ins: &Insn<'_>) -> bool {
    matches!(ins.mnem, Mnem::B | Mnem::Bl | Mnem::Bx | Mnem::Blx)
        || (matches!(ins.mnem, Mnem::Mov | Mnem::Add | Mnem::Ldr)
            && ins.ops.first().and_then(|op| op.reg()) == Some(reg::PC))
}

/// The ITSTATE an `it` whose letters are `pattern` leaves behind, for the
/// condition `cond`.
///
/// The letters are relative to the condition: a `t` repeats it, so with an
/// odd condition every letter bit flips, and the end bit does not.
pub(super) fn it_state_for(cond: u8, pattern: u8) -> u8 {
    let end = pattern & pattern.wrapping_neg();
    let letters = pattern & !end & 0xf;
    let mask = if cond & 1 != 0 {
        letters ^ (0xf & !(end | (end - 1)))
    } else {
        letters
    } | end;
    (cond << 4) | mask
}

/// `it`, `itt`, `ite` and the rest. `pattern` holds the letters after the
/// first `t` as bits, `e` set, from the high bit down, followed by a set
/// bit that ends them: the mask field for a condition whose low bit is
/// clear.
fn it_block(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, pattern: u8) -> Option<Vec<Variant>> {
    encode::arity(cx, ins, &[1])?;
    let op = &ins.ops[0];
    let Some(cond) = op.word.as_deref().and_then(super::insn::condition) else {
        cx.error(op.span, "expected a condition code");
        return None;
    };
    let it = it_state_for(cond, pattern);
    set_itstate(cx, it);
    Some(narrow(it_word(it >> 4, it)))
}

/// Takes one instruction off the open block, for a caller that does not
/// predicate from it: see [`super::arm_outside_it`].
pub(super) fn advance_itstate(cx: &mut AsmCtx<'_>) {
    let it = itstate(cx);
    set_itstate(cx, it_advance(it));
}

/// Opens the block a written `it` in ARM code names, which emits nothing of
/// its own; see [`super::arm_outside_it`].
pub(super) fn open_arm_it(cx: &mut AsmCtx<'_>, cond: u8, pattern: u8) {
    set_itstate(cx, it_state_for(cond, pattern));
}

/// The ITSTATE after one instruction of a block: the mask shifts into the
/// condition's low bit, and the block ends with its end bit.
fn it_advance(it: u8) -> u8 {
    if it & 0x7 == 0 {
        0
    } else {
        (it & 0xe0) | ((it << 1) & 0x1f)
    }
}

fn want_narrow(ins: &Insn<'_>) -> bool {
    ins.width != Width::Wide
}

fn want_wide(ins: &Insn<'_>) -> bool {
    ins.width != Width::Narrow
}

/// Refuses a `.n` on an instruction that has only a 32-bit encoding.
fn wide_only(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<()> {
    if want_wide(ins) {
        return Some(());
    }
    no_encoding(cx, ins)?;
    None
}

/// Reports that no encoding of this width exists, once every candidate has
/// been ruled out.
fn no_encoding(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let hint = match ins.width {
        Width::Narrow => " as a 16-bit instruction",
        Width::Wide => " as a 32-bit instruction",
        Width::Any => "",
    };
    cx.error(
        ins.span,
        format!(
            "`{}` cannot be encoded in Thumb{hint} with these operands",
            ins.text
        ),
    );
    None
}

/// Assembles one Thumb instruction, keeping track of the `it` block it is in.
///
/// The checks are GNU as's: an instruction in a block must carry the block's
/// condition, or its inverse where the block says `e`; one that changes the
/// PC must be the block's last; and an `al` block allows no instruction at
/// all. The instruction is then encoded without its condition, which the
/// block supplies.
///
/// Outside a block only a branch may carry a condition, unless the mode asks
/// for one to be made up; see [`implicit_it`].
pub fn assemble(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let it = itstate(cx);
    if let Mnem::It(pattern) = ins.mnem {
        // A written `it` ends a block that was made up, which is what GNU
        // as's `IT_INSN` case does before it starts the manual one.
        set_auto(cx.state, None);
        if it & 0xf != 0 {
            cx.error(
                ins.span,
                "`it` falls within the range of a previous `it` block",
            );
            return None;
        }
        return it_block(cx, ins, pattern);
    }
    if it & 0xf == 0 {
        return implicit_it(cx, ins);
    }
    set_itstate(cx, it_advance(it));
    let expected = it >> 4;
    if !ins.cond_written || expected == AL {
        cx.error(
            ins.span,
            format!(
                "`{}` is not allowed in an `it` block without its condition",
                ins.text
            ),
        );
        return None;
    }
    if ins.cond != expected {
        let want = super::insn::condition_name(expected);
        cx.error(
            ins.span,
            format!(
                "`{}` has the wrong condition for this `it` block, which expects `{want}` here",
                ins.text
            ),
        );
        return None;
    }
    if is_branch(ins) && it & 0xf != 0x8 {
        cx.error(
            ins.span,
            format!(
                "`{}` is a branch, which must be the last instruction in its `it` block",
                ins.text
            ),
        );
        return None;
    }
    let inner = Insn {
        cond: AL,
        cond_written: false,
        in_it: true,
        ..*ins
    };
    encode_insn(cx, &inner)
}

/// Whether an instruction is one a condition means nothing to and that an
/// `it` block may hold all the same: GNU as's `NEUTRAL_IT_INSN`, which of
/// the instructions this backend has is `nop`, `bkpt`, `hlt` and `udf`.
/// Inside a made-up block one of these extends it, taking the block's own
/// condition.
fn neutral(ins: &Insn<'_>) -> bool {
    matches!(ins.mnem, Mnem::Ext(at)
        if matches!(super::table::FORMS[at as usize].name, "nop" | "bkpt" | "hlt" | "udf"))
}

/// Whether a condition on this instruction is one only an `it` block can
/// supply, so that GNU as either refuses the line or makes a block up for
/// it.
///
/// `b<cond>` is the one instruction that carries its own condition in Thumb
/// — GNU as's single `IF_INSIDE_IT_LAST_INSN` — and a condition on a
/// [`neutral`] instruction is dropped rather than predicating anything.
fn needs_it(ins: &Insn<'_>) -> bool {
    ins.cond_written && ins.cond != AL && !matches!(ins.mnem, Mnem::B) && !neutral(ins)
}

/// Whether a condition may join a block opened with `cc`: GNU as's
/// `now_pred_compatible`, which takes the condition itself or its inverse.
fn compatible(cond: u8, cc: u8) -> bool {
    cond & !1 == cc & !1
}

/// Puts the halfword of a made-up `it` in front of every encoding of the
/// instruction it predicates, carrying the fixups along with the bytes.
///
/// GNU as emits the `it` as an instruction of its own; one fragment holding
/// both writes the same bytes and keeps the instruction's PC-relative
/// fixups measured from their own halfword, which is where they belong.
fn with_it(word: u16, variants: Vec<Variant>) -> Vec<Variant> {
    variants
        .into_iter()
        .map(|mut v| {
            let mut bytes = word.to_le_bytes().to_vec();
            bytes.extend_from_slice(&v.bytes);
            v.bytes = bytes;
            for f in &mut v.fixups {
                f.offset += 2;
            }
            v
        })
        .collect()
}

/// The instruction as it is encoded inside a block: the condition comes from
/// the `it` rather than from the instruction, which also decides which
/// 16-bit forms fit (see [`sets_flags16`]).
fn inside_it<'o>(ins: &Insn<'o>) -> Insn<'o> {
    Insn {
        cond: AL,
        cond_written: false,
        in_it: true,
        ..*ins
    }
}

/// Assembles an instruction that no written `it` block covers, making one up
/// where [`crate::assembler::ImplicitIt`] asks for it.
///
/// This is GNU as's `handle_pred_state` in its `OUTSIDE_PRED_BLOCK` and
/// `AUTOMATIC_PRED_BLOCK` states. A conditional instruction with nothing
/// open gets an `it` of its own in front of it, and each instruction after
/// that widens the `it`'s mask instead of getting one: the block takes the
/// condition or its inverse, holds at most four instructions, and ends at
/// the first instruction that cannot be in it — one that is unconditional,
/// one whose condition is neither the block's nor its inverse, one that
/// changes the PC, a written `it`, or a label, which `Arm::label_flags`
/// closes it for.
///
/// Where the mode does not ask for a block, nothing here applies and the
/// instruction is encoded as it was written, which is where the encoders
/// refuse a condition they have no block for.
///
/// A made-up block does not outlive a section switch, as GNU as's does:
/// `now_pred` is part of each section's state there, while here it is the
/// backend's own and the fragment the core remembers is one section's. A
/// section left and come back to continues its block only if no block was
/// made up in between.
fn implicit_it(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let Some(block) = auto(cx.state).filter(|_| cx.implicit_it_here) else {
        if !needs_it(ins) || !cx.implicit_it.makes_thumb_block() {
            return encode_insn(cx, ins);
        }
        return open_it(cx, ins);
    };
    if neutral(ins) {
        // GNU as's `NEUTRAL_IT_INSN`: the instruction joins with the block's
        // own condition, so the letter is always a `t`, and a fifth one ends
        // the block instead.
        if block.len >= 4 {
            set_auto(cx.state, None);
            return encode_insn(cx, ins);
        }
        return widen_it(cx, ins, block, block.cond, false);
    }
    let cond = if ins.cond_written { ins.cond } else { AL };
    // An instruction that changes the PC leaves the block, so it is the last
    // one in it.
    let last = is_branch(ins);
    if block.len < 4 && compatible(cond, block.cond) {
        return widen_it(cx, ins, block, cond, last);
    }
    set_auto(cx.state, None);
    if !needs_it(ins) {
        return encode_insn(cx, ins);
    }
    open_it(cx, ins)
}

/// GNU as's `new_automatic_it_block`: an `it` with the instruction's
/// condition and a mask of one instruction, in front of the instruction
/// itself.
fn open_it(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let block = Auto {
        cond: ins.cond,
        mask: 0x8,
        len: 1,
    };
    let variants = encode_insn(cx, &inside_it(ins))?;
    // A branch is the only instruction in the block it ends.
    set_auto(cx.state, (!is_branch(ins)).then_some(block));
    cx.requests.push(Request::ImplicitIt);
    Some(with_it(it_word(block.cond, block.mask), variants))
}

/// GNU as's `now_pred_add_mask`: one more instruction in the block, whose
/// letter is a `t` where `cond` has the block's parity and an `e` where it
/// does not, and the end bit after it.
fn widen_it(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    block: Auto,
    cond: u8,
    last: bool,
) -> Option<Vec<Variant>> {
    let len = block.len + 1;
    let bit = |mask: u8, value: u8, n: u32| (mask & !(1 << n)) | (value << n);
    let mask = bit(
        bit(block.mask & 0xf, cond & 1, 5 - u32::from(len)),
        1,
        4 - u32::from(len),
    );
    let variants = encode_insn(cx, &inside_it(ins))?;
    set_auto(
        cx.state,
        (!last).then_some(Auto {
            cond: block.cond,
            mask,
            len,
        }),
    );
    cx.requests.push(Request::ImplicitItMask(
        it_word(block.cond, mask).to_le_bytes(),
    ));
    Some(variants)
}

fn encode_insn(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    use Mnem::*;
    match ins.mnem {
        It(_) => None,
        B | Bl | Bx | Blx => branch(cx, ins),
        Mov | Mvn => mov(cx, ins),
        Add | Sub => add_sub(cx, ins),
        Addw | Subw => add_sub_wide(cx, ins),
        Cmp => compare(cx, ins),
        Cmn | Tst | Teq => test(cx, ins),
        Neg => negate(cx, ins),
        And | Eor | Orr | Orn | Bic | Adc | Sbc | Rsb => alu_reg(cx, ins),
        Lsl | Lsr | Asr | Ror | Rrx => shift_insn(cx, ins),
        Ldr | Str | Ldrb | Strb | Ldrh | Strh | Ldrsb | Ldrsh | Ldrd | Strd | Ldrt | Strt
        | Ldrbt | Strbt | Ldrht | Strht | Ldrsbt | Ldrsht => load_store(cx, ins),
        Pld | Pldw | Pli => preload(cx, ins),
        Push | Pop => push_pop(cx, ins),
        Adr => adr(cx, ins),
        Adrl => {
            cx.error(ins.span, "`adrl` is an ARM instruction; Thumb has `adr`");
            None
        }
        Ldm(_) | Stm(_) => block_transfer(cx, ins),
        Mul | Mla | Mls | Umull | Umlal | Smull | Smlal => multiply(cx, ins),
        Movw | Movt => move_wide(cx, ins),
        Ext(at) => super::generic::assemble(cx, ins, at),
        Mrs => status_read(cx, ins),
        Msr => status_write(cx, ins),
        Cbz | Cbnz => compare_branch(cx, ins),
        Rsc => {
            cx.error(
                ins.span,
                format!("`{}` is not supported in Thumb by this backend", ins.text),
            );
            None
        }
    }
}

// ---- branches --------------------------------------------------------------

/// 16-bit `b <label>`: an 11-bit halfword offset.
fn scatter_b16(w: u64, v: i64) -> u64 {
    (w & 0xf800) | (((v >> 1) as u64) & 0x7ff)
}

/// 16-bit `b<cond> <label>`: an 8-bit halfword offset.
fn scatter_bcc16(w: u64, v: i64) -> u64 {
    (w & 0xff00) | (((v >> 1) as u64) & 0xff)
}

/// The J-bit encoding shared by `b.w` and `bl`: the two bits that
/// extend the range are stored inverted relative to the sign bit, so that a
/// short forward branch has them clear.
fn scatter_t4(v: i64, low_bits: u64) -> u64 {
    let v = v as u64;
    let s = (v >> 24) & 1;
    let i1 = (v >> 23) & 1;
    let i2 = (v >> 22) & 1;
    let j1 = (i1 ^ 1) ^ s;
    let j2 = (i2 ^ 1) ^ s;
    let hw1 = 0xf000 | (s << 10) | ((v >> 12) & 0x3ff);
    let hw2 = low_bits | (j1 << 13) | (j2 << 11) | ((v >> 1) & 0x7ff);
    (hw2 << 16) | hw1
}

fn scatter_bw(_word: u64, v: i64) -> u64 {
    scatter_t4(v, 0x9000)
}

fn scatter_bl(_word: u64, v: i64) -> u64 {
    scatter_t4(v, 0xd000)
}

/// `blx <label>`, which clears bit 12 of the second halfword. Its offset is
/// from the PC rounded down to a word and lands on a word, and GNU as rounds
/// an odd one up rather than refuse it.
fn scatter_blx(_word: u64, v: i64) -> u64 {
    scatter_t4((v + 3) & !3, 0xc000)
}

/// Thumb `bl label`.
pub fn bl_kind() -> FixupKind {
    FixupKind::pcrel(4, 4)
        .with_field(25, 2)
        .with_reloc(reloc::THM_CALL)
        .link(LinkValue::Interwork(super::IW_THUMB_BL))
        .scatter(scatter_bl)
}

/// Thumb `blx label`, into ARM code.
pub fn blx_kind() -> FixupKind {
    FixupKind::pcrel(4, 4)
        .with_pc_align(4)
        .with_field(25, 2)
        .with_reloc(reloc::THM_CALL)
        .link(LinkValue::Interwork(super::IW_THUMB_BLX))
        .scatter(scatter_blx)
}

/// A `blx` GNU as turns into `bl`, for a call that stays in Thumb. It keeps
/// the `blx`'s base, the PC rounded down to a word, so a call from an
/// instruction that is not on a word boundary lands two bytes past its
/// target; GNU as does that, and warns.
pub fn blx_as_bl_kind() -> FixupKind {
    FixupKind {
        link: LinkValue::Plain,
        ..blx_kind()
    }
    .scatter(scatter_bl)
}

/// `bl` rewritten as `blx`: bit 12 of the second halfword cleared.
pub fn to_blx(w: u64) -> u64 {
    w & !(0x1000 << 16)
}

/// `blx` rewritten as `bl`.
pub fn to_bl(w: u64) -> u64 {
    w | (0x1000 << 16)
}

/// `b<cond>.w`: a 20-bit range with the condition in the first halfword and
/// the J bits stored directly rather than inverted.
fn scatter_bcc_w(w: u64, v: i64) -> u64 {
    let cond = (w >> 6) & 0xf;
    let v = v as u64;
    let s = (v >> 20) & 1;
    let j2 = (v >> 19) & 1;
    let j1 = (v >> 18) & 1;
    let hw1 = 0xf000 | (s << 10) | (cond << 6) | ((v >> 12) & 0x3f);
    let hw2 = 0x8000 | (j1 << 13) | (j2 << 11) | ((v >> 1) & 0x7ff);
    (hw2 << 16) | hw1
}

fn fixed(bytes: Vec<u8>, expr: ExprRef, kind: FixupKind, span: Span) -> Variant {
    Variant {
        bytes,
        fixups: vec![Fixup {
            offset: 0,
            expr,
            kind,
            span,
        }],
    }
}

fn branch(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[1])?;
    let op = &ins.ops[0];

    if matches!(ins.mnem, Mnem::Bx | Mnem::Blx) && op.reg().is_some() {
        unconditional(cx, ins)?;
        if !want_narrow(ins) {
            return no_encoding(cx, ins);
        }
        let rm = encode::reg_of(cx, op)? as u16;
        let base = if ins.mnem == Mnem::Bx { 0x4700 } else { 0x4780 };
        return Some(narrow(base | (rm << 3)));
    }

    let Some(e) = op.imm() else {
        cx.error(op.span, "expected a branch target");
        return None;
    };

    // Thumb reads the PC as the address of the instruction plus four,
    // whatever the instruction's own length.
    // A call to a label is always two halfwords.
    if matches!(ins.mnem, Mnem::Bl | Mnem::Blx) && !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    match ins.mnem {
        Mnem::Bl => {
            unconditional(cx, ins)?;
            Some(vec![fixed(
                wide_bytes(0xf000, 0xd000),
                e,
                bl_kind(),
                ins.span,
            )])
        }
        Mnem::Blx => {
            unconditional(cx, ins)?;
            Some(vec![fixed(
                wide_bytes(0xf000, 0xc000),
                e,
                blx_kind(),
                ins.span,
            )])
        }
        Mnem::Bx => {
            cx.error(op.span, "`bx` takes a register");
            None
        }
        _ if ins.cond != AL => {
            let mut out = Vec::new();
            if want_narrow(ins) {
                // No relocation: a 2-byte field cannot carry one, so an
                // unresolved target escalates to the wide form instead.
                let kind = FixupKind::pcrel(2, 4)
                    .with_field(9, 2)
                    .link(LinkValue::Interwork(super::IW_THUMB_JUMP16))
                    .scatter(scatter_bcc16);
                out.push(fixed(
                    (0xd000u16 | ((ins.cond as u16) << 8))
                        .to_le_bytes()
                        .to_vec(),
                    e,
                    kind,
                    ins.span,
                ));
            }
            if want_wide(ins) {
                let kind = FixupKind::pcrel(4, 4)
                    .with_field(21, 2)
                    .with_reloc(reloc::THM_JUMP19)
                    .link(LinkValue::Interwork(super::IW_THUMB_JUMP))
                    .scatter(scatter_bcc_w);
                // The condition sits in the first halfword; the scatter
                // function reads it back out of the placeholder.
                out.push(fixed(
                    wide_bytes(0xf000 | ((ins.cond as u16) << 6), 0x8000),
                    e,
                    kind,
                    ins.span,
                ));
            }
            if out.is_empty() {
                return no_encoding(cx, ins);
            }
            Some(out)
        }
        _ => {
            let mut out = Vec::new();
            if want_narrow(ins) {
                let kind = FixupKind::pcrel(2, 4)
                    .with_field(12, 2)
                    .link(LinkValue::Interwork(super::IW_THUMB_JUMP16))
                    .scatter(scatter_b16);
                out.push(fixed(0xe000u16.to_le_bytes().to_vec(), e, kind, ins.span));
            }
            if want_wide(ins) {
                let kind = FixupKind::pcrel(4, 4)
                    .with_field(25, 2)
                    .with_reloc(reloc::THM_JUMP24)
                    .link(LinkValue::Interwork(super::IW_THUMB_JUMP))
                    .scatter(scatter_bw);
                out.push(fixed(wide_bytes(0xf000, 0x9000), e, kind, ins.span));
            }
            if out.is_empty() {
                return no_encoding(cx, ins);
            }
            Some(out)
        }
    }
}

// ---- data processing --------------------------------------------------------

/// Splits the twelve bits of a `ThumbExpandImm` across the two halfwords.
fn expand_parts(imm12: u32) -> (u16, u16) {
    let i = ((imm12 >> 11) & 1) as u16;
    let rest = (((imm12 >> 8) & 7) << 12) as u16 | (imm12 & 0xff) as u16;
    (i, rest)
}

/// The 32-bit encodings of a data-processing operation: the first halfword
/// of the shifted-register form and of the modified-immediate form, without
/// the S bit or the first source register.
///
/// Six operations share another's opcode with a register fixed in it. A
/// comparison is the arithmetic instruction with `rd` set to 15, and `mov`
/// and `mvn` are `orr` and `orn` with `rn` set to 15, which is why
/// [`wide_dp`] can encode all sixteen the same way.
fn wide_op(m: Mnem) -> Option<(u16, u16)> {
    use Mnem::*;
    Some(match m {
        And | Tst => (0xea00, 0xf000),
        Bic => (0xea20, 0xf020),
        Orr | Mov => (0xea40, 0xf040),
        Orn | Mvn => (0xea60, 0xf060),
        Eor | Teq => (0xea80, 0xf080),
        Add | Cmn => (0xeb00, 0xf100),
        Adc => (0xeb40, 0xf140),
        Sbc => (0xeb60, 0xf160),
        Sub | Cmp => (0xeba0, 0xf1a0),
        Rsb | Neg => (0xebc0, 0xf1c0),
        _ => return None,
    })
}

/// The second halfword of a shifted-register form: the shift amount split
/// around the destination register as `imm3:imm2`, then the type and `rm`.
fn shift_hw2(rd: Reg, rm: Reg, shift: Shift, amount: u32) -> u16 {
    // A shift of zero is written as `lsl` whichever kind the source names,
    // and `lsr #32` and `asr #32` are spelled with a zero amount -- which is
    // why `lsr #0` cannot mean 32. `rrx` takes `ror`'s type with a zero
    // amount, so a real `ror #0` has no encoding of its own either.
    let (kind, n) = match shift {
        _ if amount == 0 => (Shift::Lsl, 0),
        Shift::Lsr | Shift::Asr if amount == 32 => (shift, 0),
        _ => (shift, amount),
    };
    (((n >> 2) & 7) << 12) as u16
        | ((rd as u16) << 8)
        | ((n & 3) << 6) as u16
        | (kind.code() << 4) as u16
        | rm as u16
}

/// A 32-bit data-processing instruction, `op{s}.w rd, rn, <operand2>`.
fn wide_dp(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    m: Mnem,
    rd: Reg,
    rn: Reg,
    src: &Operand,
    set_flags: bool,
) -> Option<Vec<Variant>> {
    let Some((reg_base, imm_base)) = wide_op(m) else {
        return no_encoding(cx, ins);
    };
    let s = u16::from(set_flags) << 4;
    match &src.kind {
        OperandKind::Reg(rm) => Some(wide(
            reg_base | s | rn as u16,
            ((rd as u16) << 8) | *rm as u16,
        )),
        OperandKind::Shifted { rm, shift, amount } => {
            let n = match amount {
                ShiftAmt::Imm(n) => *n,
                // `rrx` is `ror` by zero.
                ShiftAmt::None => 0,
                ShiftAmt::Reg(_) => {
                    cx.error(
                        src.span,
                        "a 32-bit Thumb instruction cannot take a register shift amount",
                    );
                    return None;
                }
            };
            let hw2 = if matches!(amount, ShiftAmt::None) {
                ((rd as u16) << 8) | (3 << 4) | *rm as u16
            } else {
                shift_hw2(rd, *rm, *shift, n)
            };
            Some(wide(reg_base | s | rn as u16, hw2))
        }
        OperandKind::Imm(_) => {
            let v = encode::imm32(cx, src)?;
            if let Some(imm12) = imm::thumb_expand(v) {
                let (i, rest) = expand_parts(imm12);
                return Some(wide(
                    imm_base | (i << 10) | s | rn as u16,
                    rest | ((rd as u16) << 8),
                ));
            }
            // The substitution A32 makes as well: `and rd, rn, #~x` is
            // `bic rd, rn, #x`, and `cmp rd, #-x` is `cmn rd, #x`.
            if let Some((partner, negate)) = m.immediate_partner() {
                let alt = if negate { v.wrapping_neg() } else { !v };
                if let Some(imm12) = imm::thumb_expand(alt)
                    && let Some((_, base)) = wide_op(partner)
                {
                    let (i, rest) = expand_parts(imm12);
                    return Some(wide(
                        base | (i << 10) | s | rn as u16,
                        rest | ((rd as u16) << 8),
                    ));
                }
            }
            cx.error(
                src.span,
                format!(
                    "{} (0x{v:08x}) is not a Thumb expandable immediate, and neither \
                     is its complement",
                    v as i32
                ),
            );
            None
        }
        _ => no_encoding(cx, ins),
    }
}

fn mov(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    let src = &ins.ops[1];
    let mvn = ins.mnem == Mnem::Mvn;
    let s = u16::from(ins.set_flags);
    // `movs pc, lr` is the exception return, which is `subs pc, lr, #0`.
    if !mvn && ins.set_flags && rd == reg::PC && src.reg() == Some(reg::LR) {
        if !want_wide(ins) {
            return no_encoding(cx, ins);
        }
        return Some(wide(0xf3de, 0x8f00));
    }
    // `do_t_mov_cmp`: only a plain `mov` between registers may name the
    // stack pointer or the PC, and then not both, and not in the 32-bit
    // form; `mvn` and every immediate form refuse both.
    if mvn || ins.set_flags || src.reg().is_none() {
        bad_reg(cx, ins.ops[0].span, rd)?;
        bad_regs(cx, src)?;
    }

    if let Some(rm) = src.reg() {
        if want_narrow(ins) {
            if mvn {
                if sets_flags16(ins) && low(rd) && low(rm) {
                    return Some(narrow(0x43c0 | ((rm as u16) << 3) | rd as u16));
                }
            } else if ins.set_flags {
                // `movs rd, rm` is `lsls rd, rm, #0`, which in an `it` block
                // would not set the flags.
                if !ins.in_it && low(rd) && low(rm) {
                    return Some(narrow(((rm as u16) << 3) | rd as u16));
                }
            } else {
                let d = rd as u16;
                return Some(narrow(
                    0x4600 | ((d & 8) << 4) | ((rm as u16) << 3) | (d & 7),
                ));
            }
        }
        if !want_wide(ins) {
            return no_encoding(cx, ins);
        }
        not_pc(cx, ins.ops[0].span, rd)?;
        not_pc(cx, src.span, rm)?;
        if rd == reg::SP && rm == reg::SP {
            cx.error(ins.span, "`mov.w sp, sp` is not encodable");
            return None;
        }
        return wide_dp(cx, ins, ins.mnem, rd, reg::PC, src, ins.set_flags);
    }
    if let OperandKind::Shifted { rm, shift, amount } = src.kind {
        if mvn {
            bad_reg(cx, ins.ops[0].span, rd)?;
            bad_regs(cx, src)?;
            if !want_wide(ins) {
                return no_encoding(cx, ins);
            }
            return wide_dp(cx, ins, ins.mnem, rd, reg::PC, src, ins.set_flags);
        }
        return encode_shift(cx, ins, rd, rm, shift, amount, src.span);
    }

    // `do_t_mov_cmp` relaxes the immediate form wherever a 16-bit one could
    // exist, whatever the constant turns out to be; see `relaxes_load_store`.
    if !mvn && sets_flags16(ins) && low(rd) && ins.width == Width::Any {
        cx.relaxable = true;
    }
    let v = encode::imm32(cx, src)?;
    if !mvn && sets_flags16(ins) && low(rd) && v <= 0xff && want_narrow(ins) {
        return Some(narrow(0x2000 | ((rd as u16) << 8) | v as u16));
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    let (base, other) = if mvn {
        (0xf06fu16, 0xf04fu16)
    } else {
        (0xf04f, 0xf06f)
    };
    if let Some(imm12) = imm::thumb_expand(v) {
        let (i, rest) = expand_parts(imm12);
        return Some(wide(base | (i << 10) | (s << 4), rest | ((rd as u16) << 8)));
    }
    // `movw` reaches any 16-bit constant, but has no flag-setting form.
    if !mvn && v <= 0xffff && !ins.set_flags {
        return Some(move_wide_bits(rd, v, false));
    }
    // Last, the complement: `mov r0, #-2` is `mvn r0, #1`. LLVM stops short
    // of turning a `movs` into a `mvns` that way, and so does this; the
    // other way round it does take, and so does this.
    if (!ins.set_flags || mvn)
        && let Some(imm12) = imm::thumb_expand(!v)
    {
        let (i, rest) = expand_parts(imm12);
        return Some(wide(
            other | (i << 10) | (s << 4),
            rest | ((rd as u16) << 8),
        ));
    }
    cx.error(
        src.span,
        format!(
            "{} (0x{v:08x}) is not a Thumb expandable immediate, does not fit in \
             the 16 bits of `movw`, and its complement is not expandable either",
            v as i32
        ),
    );
    None
}

fn move_wide_bits(rd: Reg, v: u32, top: bool) -> Vec<Variant> {
    let imm4 = ((v >> 12) & 0xf) as u16;
    let i = ((v >> 11) & 1) as u16;
    let imm3 = ((v >> 8) & 7) as u16;
    let imm8 = (v & 0xff) as u16;
    let base = if top { 0xf2c0 } else { 0xf240 };
    wide(
        base | (i << 10) | imm4,
        (imm3 << 12) | ((rd as u16) << 8) | imm8,
    )
}

/// The 16-bit immediate of a Thumb `movw`/`movt`, spread over `imm4` in the
/// first halfword and `i:imm3:imm8` in the second. The word is read from the
/// four bytes in the order they were written, so the first halfword is the
/// low half of it.
fn scatter_mov16(w: u64, v: i64) -> u64 {
    let v = v as u64 & 0xffff;
    let hw1 = (w & 0xffff & !0x040f) | ((v >> 12) & 0xf) | (((v >> 11) & 1) << 10);
    let hw2 = ((w >> 16) & 0xffff & !0x70ff) | (((v >> 8) & 7) << 12) | (v & 0xff);
    (hw2 << 16) | hw1
}

/// The Thumb `movw rd, #:lower16:sym` and `movt rd, #:upper16:sym`; see
/// [`encode::mov16_kind`], whose rules these share.
pub fn mov16_kind(top: bool) -> FixupKind {
    encode::mov16_kind(top)
        .with_reloc(if top {
            reloc::THM_MOVT_ABS
        } else {
            reloc::THM_MOVW_ABS_NC
        })
        .scatter(scatter_mov16)
}

fn move_wide(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    wide_only(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    bad_reg(cx, ins.ops[0].span, rd)?;
    if let OperandKind::Half(half, e) = ins.ops[1].kind {
        let top = encode::half_matches(cx, ins, half, ins.ops[1].span)?;
        let bytes = move_wide_bits(rd, 0, top).remove(0).bytes;
        return Some(vec![Variant {
            bytes,
            fixups: vec![Fixup {
                offset: 0,
                expr: e,
                kind: mov16_kind(top),
                span: ins.ops[1].span,
            }],
        }]);
    }
    let v = encode::imm_bits(cx, &ins.ops[1], 16)?;
    Some(move_wide_bits(rd, v, ins.mnem == Mnem::Movt))
}

// ---- add and subtract ------------------------------------------------------

fn add_sub(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::arity(cx, ins, &[2, 3])?;
    let sub = ins.mnem == Mnem::Sub;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    let two_operand = ins.ops.len() == 2;
    // `add r0, r1` and `add r0, #1` both leave the first source implicit.
    let (rn, src) = if two_operand {
        encode::no_shorthand_shift(cx, &ins.ops[1])?;
        (rd, &ins.ops[1])
    } else {
        (encode::reg_of(cx, &ins.ops[1])?, &ins.ops[2])
    };
    let s = u16::from(ins.set_flags);
    // `subs pc, lr, #imm` is the exception return, and the one instruction
    // that writes the PC and the flags at once. `movs pc, lr` is the same
    // encoding with no immediate.
    if sub && ins.set_flags && rd == reg::PC && rn == reg::LR && !two_operand {
        if !want_wide(ins) {
            return no_encoding(cx, ins);
        }
        let v = encode::imm_of(cx, src)?;
        if !(0..=0xFF).contains(&v) {
            cx.error(
                src.span,
                format!("immediate {v} is out of range (0 to 255)"),
            );
            return None;
        }
        return Some(wide(0xF3DE, 0x8F00 | v as u16));
    }
    // `do_t_add_sub`: the first source may be the stack pointer, and the
    // destination only when it is; nothing else may be, and the PC only
    // as the first source of the `addw`/`subw` a PC-relative address uses.
    if rd == reg::SP && rn != reg::SP {
        cx.error(
            ins.ops[0].span,
            "`sp` is the destination only when it is also the source",
        );
        return None;
    }

    if let Some(rm) = src.reg() {
        if sets_flags16(ins) && low(rd) && low(rn) && low(rm) && want_narrow(ins) {
            let base = if sub { 0x1a00 } else { 0x1800 };
            return Some(narrow(
                base | ((rm as u16) << 6) | ((rn as u16) << 3) | rd as u16,
            ));
        }
        if !ins.set_flags && !sub && (rd == rn || rd == rm) && want_narrow(ins) {
            // `add r0, r3, r0` adds the other source, as `do_t_add_sub`
            // swaps them to reach this encoding.
            let other = if rd == rn { rm } else { rn };
            let rd = rd as u16;
            return Some(narrow(
                0x4400 | ((rd & 8) << 4) | ((other as u16) << 3) | (rd & 7),
            ));
        }
        if want_wide(ins) {
            not_pc(cx, ins.ops[0].span, rd)?;
            not_pc(cx, ins.span, rn)?;
            bad_reg(cx, src.span, rm)?;
            return wide_dp(cx, ins, ins.mnem, rd, rn, src, ins.set_flags);
        }
        return no_encoding(cx, ins);
    }
    if matches!(src.kind, OperandKind::Shifted { .. }) {
        if !want_wide(ins) {
            return no_encoding(cx, ins);
        }
        not_pc(cx, ins.ops[0].span, rd)?;
        not_pc(cx, ins.span, rn)?;
        bad_regs(cx, src)?;
        return wide_dp(cx, ins, ins.mnem, rd, rn, src, ins.set_flags);
    }

    // `do_t_add_sub` reaches for one of four 16-bit forms, and relaxes the
    // instruction wherever the registers allow one; see `relaxes_load_store`.
    if ins.width == Width::Any
        && ((rd == reg::SP && rn == reg::SP && !ins.set_flags)
            || (low(rd) && !sub && !ins.set_flags && (rn == reg::SP || rn == reg::PC))
            || (low(rd) && low(rn) && sets_flags16(ins)))
    {
        cx.relaxable = true;
    }
    let written = encode::imm_of(cx, src)?;
    // A negative constant that the twelve-bit encoding holds as written is
    // written that way: `adds r6, #-1` is `adds.w r6, r6, #0xffffffff`,
    // which is not the same instruction as `subs r6, #1` -- it leaves a
    // different carry. Only where the value has no encoding of its own does
    // the other operation with the sign taken off stand in for it.
    if written < 0
        && want_wide(ins)
        && let Some(imm12) = imm::thumb_expand(written as u32)
    {
        not_pc(cx, ins.ops[0].span, rd)?;
        let (i, rest) = expand_parts(imm12);
        let base = if sub { 0xf1a0 } else { 0xf100 };
        return Some(wide(
            base | (i << 10) | (s << 4) | rn as u16,
            rest | ((rd as u16) << 8),
        ));
    }
    let sub = if written < 0 { !sub } else { sub };
    let Ok(v) = u32::try_from(written.unsigned_abs()) else {
        cx.error(
            src.span,
            format!("immediate {written} does not fit in 32 bits"),
        );
        return None;
    };

    if want_narrow(ins) {
        // The 16-bit `add sp, #imm` leaves the flags alone, so `adds` takes
        // the 32-bit form, as `do_t_add_sub`'s `!flags` says.
        if rn == reg::SP && rd == reg::SP && !ins.set_flags && v.is_multiple_of(4) && v / 4 <= 0x7f
        {
            let base = if sub { 0xb080 } else { 0xb000 };
            return Some(narrow(base | (v / 4) as u16));
        }
        if !sub
            && rn == reg::SP
            && low(rd)
            && !ins.set_flags
            && v.is_multiple_of(4)
            && v / 4 <= 0xff
        {
            return Some(narrow(0xa800 | ((rd as u16) << 8) | (v / 4) as u16));
        }
        if !sub
            && rn == reg::PC
            && low(rd)
            && !ins.set_flags
            && v.is_multiple_of(4)
            && v / 4 <= 0xff
        {
            // `add rd, pc, #imm` is what `adr` assembles to.
            return Some(narrow(0xa000 | ((rd as u16) << 8) | (v / 4) as u16));
        }
        if sets_flags16(ins) && low(rd) && low(rn) {
            // Which 16-bit form wins depends on how the source spelled it:
            // `adds r0, #1` is the 8-bit form and `adds r0, r0, #1` the 3-bit
            // one, even though they mean the same thing. Both GNU as and LLVM
            // keep the shape the programmer wrote.
            let imm8 = rd == rn && v <= 0xff;
            let base8 = if sub { 0x3800 } else { 0x3000 };
            if imm8 && two_operand {
                return Some(narrow(base8 | ((rd as u16) << 8) | v as u16));
            }
            if v <= 7 {
                let base = if sub { 0x1e00 } else { 0x1c00 };
                return Some(narrow(
                    base | ((v as u16) << 6) | ((rn as u16) << 3) | rd as u16,
                ));
            }
            if imm8 {
                return Some(narrow(base8 | ((rd as u16) << 8) | v as u16));
            }
        }
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    not_pc(cx, ins.ops[0].span, rd)?;
    if rn == reg::PC {
        // A PC-relative address always takes the plain twelve-bit form.
        if v > 0xfff || ins.set_flags {
            return no_encoding(cx, ins);
        }
        let i = ((v >> 11) & 1) as u16;
        let imm3 = ((v >> 8) & 7) as u16;
        let base = if sub { 0xf2af } else { 0xf20f };
        return Some(wide(
            base | (i << 10),
            (imm3 << 12) | ((rd as u16) << 8) | (v & 0xff) as u16,
        ));
    }
    if let Some(imm12) = imm::thumb_expand(v) {
        let (i, rest) = expand_parts(imm12);
        let base = if sub { 0xf1a0 } else { 0xf100 };
        return Some(wide(
            base | (i << 10) | (s << 4) | rn as u16,
            rest | ((rd as u16) << 8),
        ));
    }
    // `addw`/`subw` take a plain twelve-bit constant, but cannot set flags.
    if v <= 0xfff && !ins.set_flags {
        let i = ((v >> 11) & 1) as u16;
        let imm3 = ((v >> 8) & 7) as u16;
        let imm8 = (v & 0xff) as u16;
        let base = if sub { 0xf2a0 } else { 0xf200 };
        return Some(wide(
            base | (i << 10) | rn as u16,
            (imm3 << 12) | ((rd as u16) << 8) | imm8,
        ));
    }
    // Last, the other operation with the value negated: `sub rd, rn,
    // #0xababbabac` is `add rd, rn, #0x54545454`.
    if let Some(imm12) = imm::thumb_expand(v.wrapping_neg()) {
        let (i, rest) = expand_parts(imm12);
        let base = if sub { 0xf100 } else { 0xf1a0 };
        return Some(wide(
            base | (i << 10) | (s << 4) | rn as u16,
            rest | ((rd as u16) << 8),
        ));
    }
    cx.error(
        src.span,
        format!(
            "{v} is neither a Thumb expandable immediate nor a 12-bit constant, so \
             `{}` cannot encode it",
            ins.text
        ),
    );
    None
}

/// `addw rd, rn, #imm12` and `subw`, which are `add`/`sub` restricted to the
/// plain twelve-bit immediate form.
fn add_sub_wide(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    wide_only(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2, 3])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    let (rn, src) = if ins.ops.len() == 2 {
        encode::no_shorthand_shift(cx, &ins.ops[1])?;
        (rd, &ins.ops[1])
    } else {
        (encode::reg_of(cx, &ins.ops[1])?, &ins.ops[2])
    };
    // `addw sp, sp, #n` is allowed, as it is for `add`.
    not_pc(cx, ins.ops[0].span, rd)?;
    if rd == reg::SP && rn != reg::SP {
        cx.error(
            ins.ops[0].span,
            "`sp` is the destination only when it is also the source",
        );
        return None;
    }
    let written = encode::imm_of(cx, src)?;
    // A negative constant is the other operation with the sign taken off,
    // the same substitution `add` and `sub` make.
    let sub = (ins.mnem == Mnem::Subw) != (written < 0);
    let v = written.unsigned_abs();
    if v > 0xfff {
        cx.error(
            src.span,
            format!("immediate {written} does not fit in twelve bits"),
        );
        return None;
    }
    let v = v as u32;
    let i = ((v >> 11) & 1) as u16;
    let imm3 = ((v >> 8) & 7) as u16;
    let imm8 = (v & 0xff) as u16;
    let base = if sub { 0xf2a0 } else { 0xf200 };
    Some(wide(
        base | (i << 10) | rn as u16,
        (imm3 << 12) | ((rd as u16) << 8) | imm8,
    ))
}

// ---- comparisons and the register ALU forms --------------------------------

fn compare(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rn = encode::reg_of(cx, &ins.ops[0])?;
    let src = &ins.ops[1];
    not_pc(cx, ins.ops[0].span, rn)?;
    if let Some(rm) = src.reg()
        && want_narrow(ins)
    {
        not_pc(cx, src.span, rm)?;
        if low(rn) && low(rm) {
            return Some(narrow(0x4280 | ((rm as u16) << 3) | rn as u16));
        }
        // The high-register form cannot compare two low ones, which is what
        // the encoding above is for.
        let n = rn as u16;
        return Some(narrow(
            0x4500 | ((n & 8) << 4) | ((rm as u16) << 3) | (n & 7),
        ));
    }
    if let OperandKind::Imm(_) = src.kind {
        // `do_t_mov_cmp` again: a low base relaxes whatever the constant is.
        if low(rn) && ins.width == Width::Any {
            cx.relaxable = true;
        }
        let v = encode::imm32(cx, src)?;
        if low(rn) && v <= 0xff && want_narrow(ins) {
            return Some(narrow(0x2800 | ((rn as u16) << 8) | v as u16));
        }
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    bad_regs(cx, src)?;
    wide_dp(cx, ins, Mnem::Cmp, reg::PC, rn, src, true)
}

/// `tst`, `teq` and `cmn`, which have no destination.
fn test(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rn = encode::reg_of(cx, &ins.ops[0])?;
    let src = &ins.ops[1];
    if ins.mnem == Mnem::Cmn {
        not_pc(cx, ins.ops[0].span, rn)?;
    } else {
        bad_reg(cx, ins.ops[0].span, rn)?;
    }
    bad_regs(cx, src)?;
    if let Some(rm) = src.reg()
        && ins.mnem != Mnem::Teq
        && low(rn)
        && low(rm)
        && want_narrow(ins)
    {
        let op: u16 = if ins.mnem == Mnem::Tst { 8 } else { 11 };
        return Some(narrow(0x4000 | (op << 6) | ((rm as u16) << 3) | rn as u16));
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    wide_dp(cx, ins, ins.mnem, reg::PC, rn, src, true)
}

/// `neg rd, rm`, which is `rsb rd, rm, #0` written short.
fn negate(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    let rn = encode::reg_of(cx, &ins.ops[1])?;
    bad_reg(cx, ins.ops[0].span, rd)?;
    bad_reg(cx, ins.ops[1].span, rn)?;
    if sets_flags16(ins) && low(rd) && low(rn) && want_narrow(ins) {
        return Some(narrow(0x4240 | ((rn as u16) << 3) | rd as u16));
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    Some(wide(
        0xf1c0 | (u16::from(ins.set_flags) << 4) | rn as u16,
        (rd as u16) << 8,
    ))
}

/// The bitwise and carry-propagating operations: `and`, `eor`, `orr`, `orn`,
/// `bic`, `adc`, `sbc` and `rsb`. Only the two-register spellings of the
/// first seven have a 16-bit encoding, and it always sets the flags.
fn alu_reg(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    let op: u16 = match ins.mnem {
        Mnem::And => 0,
        Mnem::Eor => 1,
        Mnem::Adc => 5,
        Mnem::Sbc => 6,
        Mnem::Rsb => 9,
        Mnem::Orr => 12,
        Mnem::Bic => 14,
        // `orn` is Thumb-2's own, and has no 16-bit form.
        _ => 0xff,
    };
    encode::arity(cx, ins, &[2, 3])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    let (rn, src) = if ins.ops.len() == 2 {
        encode::no_shorthand_shift(cx, &ins.ops[1])?;
        (rd, &ins.ops[1])
    } else {
        (encode::reg_of(cx, &ins.ops[1])?, &ins.ops[2])
    };
    bad_reg(cx, ins.ops[0].span, rd)?;
    bad_reg(cx, ins.ops[0].span, rn)?;
    bad_regs(cx, src)?;
    // `rsbs rd, rn, #0` is Thumb's negate, and the only 16-bit `rsb`.
    if ins.mnem == Mnem::Rsb {
        let zero = matches!(src.kind, OperandKind::Imm(_)) && encode::imm_of(cx, src)? == 0;
        if zero && sets_flags16(ins) && low(rd) && low(rn) && want_narrow(ins) {
            return Some(narrow(0x4000 | (op << 6) | ((rn as u16) << 3) | rd as u16));
        }
    } else if let Some(rm) = src.reg()
        && op != 0xff
        && sets_flags16(ins)
        && low(rd)
        && low(rm)
        && low(rn)
        && want_narrow(ins)
    {
        // `and`, `eor`, `adc` and `orr` commute, so the 16-bit form takes
        // either source as the destination; `bic` and `sbc` do not.
        let commutes = matches!(ins.mnem, Mnem::And | Mnem::Eor | Mnem::Adc | Mnem::Orr);
        if rd == rn {
            return Some(narrow(0x4000 | (op << 6) | ((rm as u16) << 3) | rd as u16));
        }
        if commutes && rd == rm {
            return Some(narrow(0x4000 | (op << 6) | ((rn as u16) << 3) | rd as u16));
        }
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    wide_dp(cx, ins, ins.mnem, rd, rn, src, ins.set_flags)
}

fn shift_insn(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    if ins.mnem == Mnem::Rrx {
        encode::arity(cx, ins, &[2])?;
        let rd = encode::reg_of(cx, &ins.ops[0])?;
        let rm = encode::reg_of(cx, &ins.ops[1])?;
        return encode_shift(cx, ins, rd, rm, Shift::Rrx, ShiftAmt::None, ins.span);
    }
    encode::arity(cx, ins, &[2, 3])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    let (rm, amount) = if ins.ops.len() == 2 {
        (rd, &ins.ops[1])
    } else {
        (encode::reg_of(cx, &ins.ops[1])?, &ins.ops[2])
    };
    let shift = match ins.mnem {
        Mnem::Lsl => Shift::Lsl,
        Mnem::Lsr => Shift::Lsr,
        Mnem::Asr => Shift::Asr,
        _ => Shift::Ror,
    };
    let amt = match amount.reg() {
        Some(rs) => ShiftAmt::Reg(rs),
        None => {
            let v = encode::imm_of(cx, amount)?;
            let hi = match shift {
                Shift::Lsl | Shift::Ror => 31,
                _ => 32,
            };
            let lo = 0;
            if v < lo || v > hi {
                cx.error(
                    amount.span,
                    format!("shift amount {v} is out of range ({lo} to {hi})"),
                );
                return None;
            }
            ShiftAmt::Imm(v as u32)
        }
    };
    encode_shift(cx, ins, rd, rm, shift, amt, amount.span)
}

/// A shift of one register into another, which `mov` with a shifted source
/// is another spelling of: `movs r7, r5, lsl #29` is `lsls r7, r5, #29`, as
/// `do_t_mov_cmp` writes it.
fn encode_shift(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    rd: Reg,
    rm: Reg,
    shift: Shift,
    amount: ShiftAmt,
    span: Span,
) -> Option<Vec<Variant>> {
    let s = u16::from(ins.set_flags) << 4;
    bad_reg(cx, ins.ops[0].span, rd)?;
    bad_reg(cx, span, rm)?;
    if let ShiftAmt::Reg(rs) = amount {
        bad_reg(cx, span, rs)?;
        // The 16-bit form shifts the destination in place and sets the flags.
        if sets_flags16(ins) && rd == rm && low(rd) && low(rs) && want_narrow(ins) {
            let op: u16 = match shift {
                Shift::Lsl => 2,
                Shift::Lsr => 3,
                Shift::Asr => 4,
                _ => 7,
            };
            return Some(narrow(0x4000 | (op << 6) | ((rs as u16) << 3) | rd as u16));
        }
        if !want_wide(ins) {
            return no_encoding(cx, ins);
        }
        let base: u16 = match shift {
            Shift::Lsl => 0xfa00,
            Shift::Lsr => 0xfa20,
            Shift::Asr => 0xfa40,
            _ => 0xfa60,
        };
        return Some(wide(
            base | s | rm as u16,
            0xf000 | ((rd as u16) << 8) | rs as u16,
        ));
    }
    // `ror` has no 16-bit form at all, whatever its amount.
    let narrow_ok = shift != Shift::Ror;
    let (shift, v) = match amount {
        // A shift of zero is `lsl` whatever the source called it.
        ShiftAmt::Imm(0) => (Shift::Lsl, 0),
        ShiftAmt::Imm(v) => (shift, v),
        // `rrx` is `ror` by zero, and has no 16-bit form.
        _ => {
            if !want_wide(ins) {
                return no_encoding(cx, ins);
            }
            return Some(wide(0xea4f | s, ((rd as u16) << 8) | (3 << 4) | rm as u16));
        }
    };

    if narrow_ok && sets_flags16(ins) && low(rd) && low(rm) && want_narrow(ins) {
        let base: u16 = match shift {
            Shift::Lsl => 0x0000,
            Shift::Lsr => 0x0800,
            _ => 0x1000,
        };
        return Some(narrow(
            base | ((v as u16 & 0x1f) << 6) | ((rm as u16) << 3) | rd as u16,
        ));
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    // A shift by a constant is `mov` with that shift applied.
    Some(wide(0xea4f | s, shift_hw2(rd, rm, shift, v)))
}

// ---- loads and stores ------------------------------------------------------

fn load_store(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let t = ins.mnem.transfer()?;
    unconditional(cx, ins)?;
    encode::no_flags(cx, ins)?;
    if t.size == 8 {
        return load_store_dual(cx, ins, t);
    }
    encode::arity(cx, ins, &[2])?;
    let rt = encode::reg_of(cx, &ins.ops[0])?;
    if let OperandKind::Literal(e) = ins.ops[1].kind {
        return literal_load(cx, ins, t, rt, &ins.ops[1], e);
    }
    if let OperandKind::Imm(e) = ins.ops[1].kind {
        return pcrel_load(cx, ins, t, rt, rt, &ins.ops[1], e);
    }
    let OperandKind::Mem(mem) = ins.ops[1].kind else {
        cx.error(
            ins.ops[1].span,
            format!("expected a memory operand, found {}", ins.ops[1].describe()),
        );
        return None;
    };
    if mem.base == reg::PC && mem.index != Index::Offset {
        cx.error(mem.span, "a PC-relative address cannot write `pc` back");
        return None;
    }
    // `ldr` and `str` may move the stack pointer, and `ldr` the PC, which is
    // a branch; every other width, and every unprivileged form, may not.
    if t.size == 4 && !t.signed && !t.translate {
        if !t.load {
            not_pc(cx, ins.ops[0].span, rt)?;
        }
    } else {
        bad_reg(cx, ins.ops[0].span, rt)?;
    }
    if mem.base == reg::PC && (!t.load || mem.index != Index::Offset) {
        cx.error(
            mem.span,
            "a PC-relative address is a plain load, with no writeback",
        );
        return None;
    }
    // `do_t_ldst` reaches for a 16-bit form here, and where one could exist
    // gives the instruction a fragment its relaxation revisits -- even when
    // the offset does not fit it and the 32-bit form is what is written.
    if ins.width == Width::Any && !t.translate && low(rt) && relaxes_load_store(t, &mem) {
        cx.relaxable = true;
    }
    if !t.translate
        && want_narrow(ins)
        && let Some(v) = narrow_load_store(t, rt, &mem)
    {
        return Some(narrow(v));
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    wide_load_store(cx, ins, t, rt, &mem)
}

/// The first halfword of a 32-bit Thumb load or store: the base that takes a
/// positive twelve-bit offset, and the base that takes everything else --
/// an eight-bit signed offset, writeback, a scaled index register and the
/// unprivileged form.
fn wide_base(t: Transfer) -> Option<(u16, u16)> {
    Some(match (t.load, t.size, t.signed) {
        (false, 1, _) => (0xf880, 0xf800),
        (true, 1, false) => (0xf890, 0xf810),
        (true, 1, true) => (0xf990, 0xf910),
        (false, 2, _) => (0xf8a0, 0xf820),
        (true, 2, false) => (0xf8b0, 0xf830),
        (true, 2, true) => (0xf9b0, 0xf930),
        (false, 4, _) => (0xf8c0, 0xf840),
        (true, 4, _) => (0xf8d0, 0xf850),
        _ => return None,
    })
}

/// The index register of a 32-bit Thumb address, which is neither the stack
/// pointer nor the PC, and has no place at all beside a PC-relative base.
fn index_register(cx: &mut AsmCtx<'_>, mem: &Mem, rm: Reg) -> Option<()> {
    bad_reg(cx, mem.span, rm)?;
    if mem.base == reg::PC {
        cx.error(mem.span, "a PC-relative address takes no index register");
        return None;
    }
    Some(())
}

fn wide_load_store(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    t: Transfer,
    rt: Reg,
    mem: &Mem,
) -> Option<Vec<Variant>> {
    let Some((base12, base8)) = wide_base(t) else {
        return no_encoding(cx, ins);
    };
    let rn = mem.base as u16;
    let rt = (rt as u16) << 12;
    if let MemOffset::Reg {
        rm,
        add,
        shift,
        amount,
        ..
    } = mem.offset
    {
        index_register(cx, mem, rm)?;
        if !add || shift != Shift::Lsl || amount > 3 || mem.index != Index::Offset || t.translate {
            cx.error(
                mem.span,
                "a 32-bit Thumb index register is added, and may be shifted left \
                 by 0 to 3",
            );
            return None;
        }
        return Some(wide(base8 | rn, rt | ((amount as u16) << 4) | rm as u16));
    }
    let off = match mem.offset {
        MemOffset::None => 0,
        MemOffset::Imm(v) => v,
        MemOffset::Unindexed(_) => {
            cx.error(mem.span, "only `ldc` and `stc` take `[rn], {option}`");
            return None;
        }
        MemOffset::Reg { .. } => unreachable!(),
    };
    // A PC-relative address is the literal form, which has no unprivileged
    // spelling: GNU as writes the ordinary load for one.
    if mem.base == reg::PC && mem.index == Index::Offset {
        // The literal forms put the sign in the first halfword and give the
        // second twelve whole bits of offset.
        if off.unsigned_abs() > 0xfff {
            cx.error(
                mem.span,
                format!("offset {off} is out of range (-4095 to 4095)"),
            );
            return None;
        }
        encode::pc_load_aligned(cx, mem.span, (rt >> 12) as Reg, mem.base, off)?;
        let u = u16::from(off >= 0) << 7;
        return Some(wide(base8 | u | rn, rt | (off.unsigned_abs() as u16)));
    }
    // The unprivileged form has neither writeback nor a negative offset.
    if t.translate {
        if mem.index != Index::Offset {
            cx.error(
                mem.span,
                "an unprivileged Thumb transfer does not write its base register back",
            );
            return None;
        }
        if !(0..=255).contains(&off) {
            cx.error(
                mem.span,
                format!("offset {off} is out of range (0 to 255) for `{}`", ins.text),
            );
            return None;
        }
        return Some(wide(base8 | rn, rt | 0xe00 | off as u16));
    }
    if mem.index == Index::Offset && (0..=0xfff).contains(&off) {
        return Some(wide(base12 | rn, rt | off as u16));
    }
    if off.unsigned_abs() > 0xff {
        cx.error(
            mem.span,
            format!("offset {off} does not fit in the 8-bit field (-255 to 255)"),
        );
        return None;
    }
    let (p, w) = match mem.index {
        Index::Offset => (1, 0),
        Index::PreIndex => (1, 1),
        Index::PostIndex => (0, 1),
    };
    if w == 1 && rt >> 12 == u16::from(mem.base) {
        cx.error(
            mem.span,
            "a transfer that writes its base register back cannot move it",
        );
        return None;
    }
    let u = u16::from(off >= 0);
    Some(wide(
        base8 | rn,
        rt | 0x800 | (p << 10) | (u << 9) | (w << 8) | (off.unsigned_abs() as u16),
    ))
}

/// `ldrd` and `strd`, whose Thumb offset is a word count either way.
fn load_store_dual(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, t: Transfer) -> Option<Vec<Variant>> {
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    encode::arity(cx, ins, &[2, 3])?;
    let rt = encode::reg_of(cx, &ins.ops[0])?;
    let mut at = 1;
    let mut rt2 = rt.wrapping_add(1);
    if ins.ops.len() == 3 {
        rt2 = encode::reg_of(cx, &ins.ops[1])?;
        at = 2;
    }
    bad_reg(cx, ins.ops[0].span, rt)?;
    bad_reg(cx, ins.ops[at.min(ins.ops.len() - 1)].span, rt2)?;
    if let OperandKind::Imm(e) = ins.ops[at].kind {
        return pcrel_load(cx, ins, t, rt, rt2, &ins.ops[at], e);
    }
    let OperandKind::Mem(mem) = ins.ops[at].kind else {
        cx.error(
            ins.ops[at].span,
            format!(
                "expected a memory operand, found {}",
                ins.ops[at].describe()
            ),
        );
        return None;
    };
    if mem.base == reg::PC && (!t.load || mem.index != Index::Offset) {
        cx.error(
            mem.span,
            "a PC-relative address is a plain load, with no writeback",
        );
        return None;
    }
    let off = match mem.offset {
        MemOffset::None => 0,
        MemOffset::Imm(v) => v,
        _ => {
            cx.error(
                mem.span,
                "`ldrd` and `strd` take no index register in Thumb",
            );
            return None;
        }
    };
    if off % 4 != 0 || off.unsigned_abs() > 1020 {
        cx.error(
            mem.span,
            format!("offset {off} is out of range (-1020 to 1020 in steps of 4)"),
        );
        return None;
    }
    let (p, w) = match mem.index {
        Index::Offset => (1, 0),
        Index::PreIndex => (1, 1),
        Index::PostIndex => (0, 1),
    };
    let u = u16::from(off >= 0);
    let l = u16::from(t.load);
    Some(wide(
        0xe840 | (p << 8) | (u << 7) | (w << 5) | (l << 4) | mem.base as u16,
        ((rt as u16) << 12) | ((rt2 as u16) << 8) | ((off.unsigned_abs() / 4) as u16),
    ))
}

/// `pld`, `pldw` and `pli`, which borrow the load encodings with the
/// transfer register set to 15.
fn preload(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    // A preload is 32 bits wide, so a `.n` cannot be honoured.
    wide_only(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[1])?;
    let (base12, base8) = match ins.mnem {
        Mnem::Pld => (0xf890u16, 0xf810u16),
        Mnem::Pldw => (0xf8b0, 0xf830),
        _ => (0xf990, 0xf910),
    };
    if let OperandKind::Imm(e) = ins.ops[0].kind {
        encode::pcrel_label(cx, ins.ops[0].span, e)?;
        return Some(vec![fixed(
            wide_bytes(base8 | u16::from(reg::PC), 0xf000),
            e,
            FixupKind::pcrel(4, 4)
                .with_pc_align(4)
                .with_limits(-4095, 4095)
                .link(LinkValue::Interwork(super::IW_STRONG_ONLY))
                .scatter(scatter_literal32),
            ins.ops[0].span,
        )]);
    }
    let OperandKind::Mem(mem) = ins.ops[0].kind else {
        cx.error(
            ins.ops[0].span,
            format!("expected a memory operand, found {}", ins.ops[0].describe()),
        );
        return None;
    };
    if mem.index != Index::Offset {
        cx.error(mem.span, "a preload does not write its base register back");
        return None;
    }
    let rn = mem.base as u16;
    match mem.offset {
        MemOffset::Reg {
            rm,
            add,
            shift,
            amount,
            ..
        } => {
            index_register(cx, &mem, rm)?;
            if !add || shift != Shift::Lsl || amount > 3 {
                cx.error(
                    mem.span,
                    "a 32-bit Thumb index register is added, and may be shifted left \
                     by 0 to 3",
                );
                return None;
            }
            Some(wide(
                base8 | rn,
                0xf000 | ((amount as u16) << 4) | rm as u16,
            ))
        }
        MemOffset::Unindexed(_) => {
            cx.error(mem.span, "only `ldc` and `stc` take `[rn], {option}`");
            None
        }
        MemOffset::None | MemOffset::Imm(_) => {
            let off = match mem.offset {
                MemOffset::Imm(v) => v,
                _ => 0,
            };
            if mem.base == reg::PC {
                // The literal form keeps the sign in the first halfword.
                if off.unsigned_abs() > 0xfff {
                    cx.error(
                        mem.span,
                        format!("offset {off} is out of range (-4095 to 4095)"),
                    );
                    return None;
                }
                let u = u16::from(off >= 0) << 7;
                return Some(wide(base8 | u | rn, 0xf000 | (off.unsigned_abs() as u16)));
            }
            if (0..=0xfff).contains(&off) {
                return Some(wide(base12 | rn, 0xf000 | off as u16));
            }
            if off < -255 {
                cx.error(
                    mem.span,
                    format!("offset {off} is out of range (-255 to 4095)"),
                );
                return None;
            }
            Some(wide(base8 | rn, 0xfc00 | (off.unsigned_abs() as u16)))
        }
    }
}

/// 16-bit `adr rd, label`: a word count forward from the PC rounded down.
fn scatter_adr16(w: u64, v: i64) -> u64 {
    (w & 0xff00) | (((v >> 2) as u64) & 0xff)
}

/// 32-bit `addw rd, pc, #imm12`, or `subw` for a label behind.
fn scatter_adr32(w: u64, v: i64) -> u64 {
    let hw1: u64 = if v < 0 { 0xf2af } else { 0xf20f };
    let m = v.unsigned_abs() & 0xfff;
    let rd = (w >> 24) & 0xf;
    let hw2 = ((m >> 8) & 7) << 12 | rd << 8 | (m & 0xff);
    ((hw2 << 16) | hw1 | ((m >> 11) & 1) << 10) & 0xffff_ffff
}

/// `adr rd, label`. A low register gets GNU as's relaxable pair, a 16-bit
/// form reaching 1020 bytes forward to a word-aligned label and a 32-bit one
/// reaching 4095 bytes either way; anything else only the 32-bit form.
fn adr(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    if rd == reg::SP || rd == reg::PC {
        cx.error(
            ins.ops[0].span,
            format!("`adr` cannot load `{}` in Thumb", reg::name_of(rd)),
        );
        return None;
    }
    let Some(e) = ins.ops[1].imm() else {
        cx.error(ins.ops[1].span, "expected a label");
        return None;
    };
    let span = ins.ops[1].span;
    let e = encode::thumb_function_address(cx, e);
    // Only the form GNU as relaxes learns about a Thumb function defined
    // after the `adr`; see `Arm::interwork`. It sets the bit in the addend,
    // which the rest of the expression is not part of, so the class carries
    // the addend's parity to the point where the target is known.
    let wide_class = if encode::odd_addend(cx, e) {
        super::IW_THUMB_ADR_ODD
    } else {
        super::IW_THUMB_ADR
    };
    // A width the source fixed is left as written, so what the target turns
    // out to be only decides whether GNU as can resolve it at all.
    let relaxed = low(rd) && ins.width == Width::Any;
    let mut out = Vec::new();
    if low(rd) && want_narrow(ins) {
        let class = if relaxed {
            super::IW_THUMB_ADR16
        } else {
            super::IW_STRONG_ONLY
        };
        let kind = FixupKind::pcrel(2, 4)
            .with_pc_align(4)
            .with_field(12, 4)
            .with_limits(0, 1020)
            .link(LinkValue::Interwork(class))
            .scatter(scatter_adr16);
        out.push(fixed(
            (0xa000u16 | ((rd as u16) << 8)).to_le_bytes().to_vec(),
            e,
            kind,
            span,
        ));
    }
    if want_wide(ins) {
        let class = if relaxed {
            wide_class
        } else {
            super::IW_THUMB_ADR32
        };
        let kind = adr32_kind().link(LinkValue::Interwork(class));
        out.push(fixed(wide_bytes(0xf20f, (rd as u16) << 8), e, kind, span));
    }
    if out.is_empty() {
        return no_encoding(cx, ins);
    }
    Some(out)
}

/// 32-bit `adr`: `addw` or `subw` from the PC rounded down to a word.
pub fn adr32_kind() -> FixupKind {
    FixupKind::pcrel(4, 4)
        .with_pc_align(4)
        .with_limits(-4095, 4095)
        .scatter(scatter_adr32)
}

/// 16-bit `ldr rt, [pc, #imm8 * 4]`.
fn scatter_literal16(w: u64, v: i64) -> u64 {
    (w & 0xff00) | (((v >> 2) as u64) & 0xff)
}

/// 32-bit `ldr.w rt, [pc, #±imm12]`: the U bit is bit 7 of the first
/// halfword, and set for an offset of zero.
fn scatter_literal32(w: u64, v: i64) -> u64 {
    let up = if v >= 0 { 0x80 } else { 0 };
    (w & !0x0fff_0080) | up | ((v.unsigned_abs() & 0xfff) << 16)
}

/// 32-bit `ldrd rt, rt2, [pc, #±imm8 * 4]`: the U bit is bit 7 of the first
/// halfword, and the offset counts words in the low eight bits of the
/// second.
fn scatter_literal_dual(w: u64, v: i64) -> u64 {
    let up = if v >= 0 { 0x80 } else { 0 };
    (w & !0x00ff_0080) | up | (((v.unsigned_abs() / 4) & 0xff) << 16)
}

/// `ldr rt, label` in T32: the bare address GNU as's `parse_address_main`
/// turns into a load from the PC rounded down to a word.
///
/// Only `ldr` itself has a 16-bit form, `ldr rt, [pc, #imm8 * 4]`, and only
/// for a low register; `do_t_ldst` gives that one a fragment and
/// `relax_adr` sizes it, taking the 32-bit form for a target behind the
/// instruction, further than 1020 bytes ahead, not word-aligned, or that a
/// later definition could move — including a Thumb function, whose address
/// this load does *not* set the low bit of, unlike `adr`. Every other load
/// is 32 bits wide, reaching 4095 bytes either way, or, for `ldrd`, 1020 in
/// steps of four.
///
/// A store has no PC-relative form at all in Thumb. An unprivileged load
/// has none either, and GNU as writes the ordinary load for one rather than
/// refusing it.
fn pcrel_load(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    t: Transfer,
    rt: Reg,
    rt2: Reg,
    op: &crate::arch::arm::operand::Operand,
    e: ExprRef,
) -> Option<Vec<Variant>> {
    if !t.load {
        cx.error(
            op.span,
            format!("`{}`: a Thumb store has no PC-relative form", ins.text),
        );
        return None;
    }
    // Only `ldr` may name the stack pointer or the PC.
    if t.size != 4 || t.signed {
        bad_reg(cx, ins.ops[0].span, rt)?;
    }
    encode::pcrel_label(cx, op.span, e)?;
    encode::pc_load_aligned(cx, op.span, rt, reg::PC, encode::addend(cx, e).unwrap_or(0))?;
    let mut out = Vec::new();
    let narrow_ok = t.size == 4 && !t.signed && !t.translate && low(rt);
    let relaxed = narrow_ok && ins.width == Width::Any;
    if narrow_ok && want_narrow(ins) {
        // Only the form layout may widen is told about a Thumb function: a
        // `.n` stands as written, and GNU as resolves that one.
        let widens = if relaxed {
            super::IW_THUMB_LDR16
        } else {
            super::IW_STRONG_ONLY
        };
        let kind = FixupKind::pcrel(2, 4)
            .with_pc_align(4)
            .with_field(12, 4)
            .with_limits(0, 1020)
            .link(LinkValue::Interwork(widens))
            .scatter(scatter_literal16);
        out.push(fixed(
            (0x4800u16 | ((rt as u16) << 8)).to_le_bytes().to_vec(),
            e,
            kind,
            op.span,
        ));
    }
    if want_wide(ins) {
        let (bytes, kind) = if t.size == 8 {
            (
                // `1110 100P U1W1 1111`: pre-indexed, no writeback, a
                // load -- the store having been refused above -- and the PC
                // as the base.
                wide_bytes(
                    0xe950 | u16::from(reg::PC),
                    ((rt as u16) << 12) | ((rt2 as u16) << 8),
                ),
                FixupKind::pcrel(4, 4)
                    .with_pc_align(4)
                    .with_field(0, 4)
                    .with_limits(-1020, 1020)
                    .link(LinkValue::Interwork(super::IW_STRONG_ONLY))
                    .scatter(scatter_literal_dual),
            )
        } else {
            let (_, base8) = wide_base(t)?;
            (
                wide_bytes(base8 | u16::from(reg::PC), (rt as u16) << 12),
                FixupKind::pcrel(4, 4)
                    .with_pc_align(4)
                    .with_limits(-4095, 4095)
                    .link(LinkValue::Interwork(super::IW_STRONG_ONLY))
                    .scatter(scatter_literal32),
            )
        };
        out.push(fixed(bytes, e, kind, op.span));
    }
    if out.is_empty() {
        return no_encoding(cx, ins);
    }
    Some(out)
}

/// `ldr rt, =expr` in T32, and the byte and halfword loads that take an
/// `=expr` as well.
///
/// A number GNU as can move instead is moved, always with a 32-bit `mov.w`,
/// `mvn.w` or `movw` (a 16-bit `movs` would change the flags); the stack
/// pointer and the PC cannot take those, so they always load. Otherwise the
/// load reaches the pool from the PC rounded down to a word, as a 16-bit
/// instruction that reaches 1020 bytes forward if the register is a low one
/// and the load is a word one, and as a 32-bit one that reaches 4095 bytes
/// either way; layout picks. `do_t_ldst` widens every other literal load.
fn literal_load(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    t: Transfer,
    rt: Reg,
    op: &crate::arch::arm::operand::Operand,
    e: ExprRef,
) -> Option<Vec<Variant>> {
    encode::literal_transfer(cx, ins, t, op)?;
    // Only `ldr` may name the stack pointer or the PC.
    if t.size != 4 || t.signed {
        bad_reg(cx, ins.ops[0].span, rt)?;
    }
    let constant = encode::literal_constant(cx, op, e)?;
    let moved = constant
        .filter(|_| rt != reg::SP && rt != reg::PC)
        .and_then(|v| {
            if let Some(imm12) = imm::thumb_expand(v) {
                let (i, rest) = expand_parts(imm12);
                return Some(wide(0xf04f | (i << 10), rest | ((rt as u16) << 8)));
            }
            if let Some(imm12) = imm::thumb_expand(!v) {
                let (i, rest) = expand_parts(imm12);
                return Some(wide(0xf06f | (i << 10), rest | ((rt as u16) << 8)));
            }
            (v <= 0xffff).then(|| move_wide_bits(rt, v, false))
        });
    if let Some(out) = moved {
        // Every move is 32 bits wide, so a `.n` leaves GNU as -- which
        // substitutes the move first and checks the width afterwards --
        // nothing that fits.
        if !want_wide(ins) {
            return no_encoding(cx, ins);
        }
        return Some(out);
    }
    let value = match cx.constant(e) {
        Some(v) if constant.is_some() => crate::arch::Literal::Const(v),
        _ => crate::arch::Literal::Expr(e),
    };
    let entry = cx.literal_from(value, 4, op.span, encode::literal_unsigned(cx, e));
    let hint = "the literal pool is too far away; put an `.ltorg` nearer";
    let mut out = Vec::new();
    if t.size == 4 && !t.signed && low(rt) && want_narrow(ins) {
        let kind = FixupKind::pcrel(2, 4)
            .with_pc_align(4)
            .with_field(12, 4)
            .with_limits(0, 1020)
            .with_range_hint(hint)
            .scatter(scatter_literal16);
        out.push(fixed(
            (0x4800u16 | ((rt as u16) << 8)).to_le_bytes().to_vec(),
            entry,
            kind,
            op.span,
        ));
    }
    if want_wide(ins)
        && let Some((_, base8)) = wide_base(t)
    {
        let kind = FixupKind::pcrel(4, 4)
            .with_pc_align(4)
            .with_limits(-4095, 4095)
            .with_range_hint(hint)
            .scatter(scatter_literal32);
        out.push(fixed(
            // The PC-relative form is the base that takes a signed offset,
            // with the PC as its base register.
            wide_bytes(base8 | u16::from(reg::PC), (rt as u16) << 12),
            entry,
            kind,
            op.span,
        ));
    }
    if out.is_empty() {
        return no_encoding(cx, ins);
    }
    Some(out)
}

/// The 16-bit form of a load or store, if the operands fit one.
/// Whether `do_t_ldst` would try a 16-bit form for this address, which is
/// what decides whether GNU as gives the instruction a relaxable fragment,
/// whatever form it ends up writing: a plain `[rn, #imm]` on a low base, or
/// a word load from the PC or the stack pointer, or a word store to it.
fn relaxes_load_store(t: Transfer, mem: &Mem) -> bool {
    if mem.index != Index::Offset || !matches!(mem.offset, MemOffset::None | MemOffset::Imm(_)) {
        return false;
    }
    let word = t.size == 4 && !t.signed;
    (low(mem.base) && !t.signed)
        || (word && t.load && (mem.base == reg::PC || mem.base == reg::SP))
        || (word && !t.load && mem.base == reg::SP)
}

fn narrow_load_store(t: Transfer, rt: Reg, mem: &Mem) -> Option<u16> {
    if mem.index != Index::Offset {
        return None;
    }
    let base = mem.base;
    match mem.offset {
        MemOffset::Unindexed(_) => None,
        MemOffset::None | MemOffset::Imm(_) => {
            let off = match mem.offset {
                MemOffset::Imm(v) => v,
                _ => 0,
            };
            // Negative or huge offsets have no 16-bit form; the caller's wide
            // path reports the range.
            let off = u32::try_from(off).ok()?;
            if t.size != 4 || t.signed {
                // Only the word forms reach the stack and the PC.
            } else if base == reg::SP && low(rt) {
                if !off.is_multiple_of(4) || off / 4 > 0xff {
                    return None;
                }
                let opc: u16 = if t.load { 0x9800 } else { 0x9000 };
                return Some(opc | ((rt as u16) << 8) | (off / 4) as u16);
            } else if base == reg::PC && low(rt) && t.load {
                if !off.is_multiple_of(4) || off / 4 > 0xff {
                    return None;
                }
                return Some(0x4800 | ((rt as u16) << 8) | (off / 4) as u16);
            }
            if !low(rt) || !low(base) || t.signed {
                return None;
            }
            let (opc, scale): (u16, u32) = match (t.load, t.size) {
                (true, 4) => (0x6800, 4),
                (false, 4) => (0x6000, 4),
                (true, 1) => (0x7800, 1),
                (false, 1) => (0x7000, 1),
                (true, 2) => (0x8800, 2),
                (false, 2) => (0x8000, 2),
                _ => return None,
            };
            if !off.is_multiple_of(scale) || off / scale > 31 {
                return None;
            }
            Some(opc | ((off / scale) as u16) << 6 | ((base as u16) << 3) | rt as u16)
        }
        MemOffset::Reg {
            rm,
            add,
            shift,
            amount,
            shifted,
        } => {
            // An index register written with a shift, even `lsl #0`, has
            // only the 32-bit form.
            if !add || amount != 0 || shift != Shift::Lsl || shifted {
                return None;
            }
            if !low(rt) || !low(base) || !low(rm) {
                return None;
            }
            let opc: u16 = match (t.load, t.size, t.signed) {
                (false, 4, _) => 0x5000,
                (false, 2, _) => 0x5200,
                (false, 1, _) => 0x5400,
                (true, 1, true) => 0x5600,
                (true, 4, _) => 0x5800,
                (true, 2, false) => 0x5a00,
                (true, 1, false) => 0x5c00,
                (true, 2, true) => 0x5e00,
                _ => return None,
            };
            Some(opc | ((rm as u16) << 6) | ((base as u16) << 3) | rt as u16)
        }
    }
}

/// Thumb has no user-mode bank, so no `^` register list.
fn no_user_bank(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, user: bool) -> Option<()> {
    if user {
        cx.error(ins.span, "Thumb has no `^` register list");
        return None;
    }
    Some(())
}

/// What a 32-bit Thumb block transfer may carry: never the stack pointer,
/// never the PC in a store, and never both `lr` and `pc` in a load.
fn check_list(cx: &mut AsmCtx<'_>, span: Span, mask: u16, load: bool) -> Option<()> {
    if mask & (1 << reg::SP) != 0 {
        cx.error(span, "`sp` is not allowed in a 32-bit Thumb register list");
        return None;
    }
    if !load && mask & (1 << reg::PC) != 0 {
        cx.error(span, "`pc` is not allowed in a Thumb store");
        return None;
    }
    if load && mask & (1 << reg::LR) != 0 && mask & (1 << reg::PC) != 0 {
        cx.error(span, "`lr` and `pc` cannot both be loaded");
        return None;
    }
    Some(())
}

/// A block transfer of one register, which GNU as writes as the load or
/// store that means the same thing: `ldmia.w r0!, {r1}` is `ldr r1, [r0], #4`.
/// Whether `ldm sp, {rt}` reaches the 16-bit stack-relative load or store.
fn narrow_stack_transfer(ins: &Insn<'_>, rt: Reg) -> bool {
    want_narrow(ins) && low(rt)
}

fn single_transfer(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    rt: Reg,
    rn: Reg,
    load: bool,
    before: bool,
    writeback: bool,
) -> Option<Vec<Variant>> {
    // The address the one register would have moved through, which is what
    // the load or store then addresses: `ldmia r0!, {r1}` is `ldr r1, [r0],
    // #4`, and `ldmia r0, {r1}` a plain `[r0]`.
    let index = match (before, writeback) {
        (false, true) => Index::PostIndex,
        (true, true) => Index::PreIndex,
        _ => Index::Offset,
    };
    let off = if before {
        -4
    } else if writeback {
        4
    } else {
        0
    };
    let mem = Mem {
        base: rn,
        offset: if off == 0 {
            MemOffset::None
        } else {
            MemOffset::Imm(off)
        },
        index,
        align: None,
        span: ins.span,
    };
    let t = Transfer {
        load,
        size: 4,
        signed: false,
        translate: false,
    };
    if want_narrow(ins)
        && let Some(v) = narrow_load_store(t, rt, &mem)
    {
        return Some(narrow(v));
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    wide_load_store(cx, ins, t, rt, &mem)
}

fn push_pop(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[1])?;
    let OperandKind::List { mask, user } = ins.ops[0].kind else {
        cx.error(ins.ops[0].span, "expected a register list");
        return None;
    };
    no_user_bank(cx, ins, user)?;
    stack_transfer(cx, ins, ins.mnem == Mnem::Pop, mask)
}

/// `push`, `pop`, and the `stmdb sp!` and `ldmia sp!` that mean the same.
fn stack_transfer(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    load: bool,
    mask: u16,
) -> Option<Vec<Variant>> {
    let push = !load;
    // The 16-bit forms carry r0-r7 plus exactly one of lr (push) or pc (pop).
    let extra = if push { 1 << reg::LR } else { 1 << reg::PC };
    if want_narrow(ins) && mask & !(0xff | extra) == 0 {
        let base: u16 = if push { 0xb400 } else { 0xbc00 };
        let bit = u16::from(mask & extra != 0) << 8;
        return Some(narrow(base | bit | (mask & 0xff)));
    }
    if mask & (1 << reg::SP) != 0 {
        cx.error(
            ins.ops[0].span,
            "the base register cannot be in the list of a transfer that writes it back",
        );
        return None;
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    check_list(cx, ins.ops[0].span, mask, !push)?;
    if let Some(rt) = sole_register(mask) {
        return single_transfer(cx, ins, rt, reg::SP, !push, push, true);
    }
    // `push` is `stmdb sp!` and `pop` is `ldmia sp!`.
    let hw1 = if push { 0xe92d } else { 0xe8bd };
    Some(wide(hw1, mask))
}

/// The single register of a one-element list, if that is what this is.
fn sole_register(list: u16) -> Option<Reg> {
    (list.count_ones() == 1).then(|| list.trailing_zeros() as Reg)
}

fn block_transfer(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let (Mnem::Ldm(mode) | Mnem::Stm(mode)) = ins.mnem else {
        return None;
    };
    let load = matches!(ins.mnem, Mnem::Ldm(_));
    let rn = encode::reg_of(cx, &ins.ops[0])?;
    let OperandKind::List { mask, user } = ins.ops[1].kind else {
        cx.error(ins.ops[1].span, "expected a register list");
        return None;
    };
    no_user_bank(cx, ins, user)?;
    let writeback = ins.ops[0].writeback;
    if mode.before == mode.increment {
        cx.error(
            ins.span,
            "Thumb has only the increment-after and decrement-before block transfers",
        );
        return None;
    }
    // GNU as writes a stack-pointer transfer as `push` or `pop`, which
    // reaches the 16-bit encodings.
    if writeback && rn == reg::SP && load == (!mode.before && mode.increment) {
        return stack_transfer(cx, ins, load, mask);
    }
    not_pc(cx, ins.ops[0].span, rn)?;
    // The 16-bit form is the increment-after one over r0-r7, whose writeback
    // is implied by the base not being in the list.
    if !mode.before && mode.increment {
        let implied = load && mask & (1 << rn) != 0;
        if writeback && implied {
            cx.error(
                ins.ops[0].span,
                "the base register cannot be in the list of a transfer that writes it back",
            );
            return None;
        }
        if want_narrow(ins) && low(rn) && mask & !0xff == 0 && (writeback || implied) {
            let base: u16 = if load { 0xc800 } else { 0xc000 };
            return Some(narrow(base | ((rn as u16) << 8) | mask));
        }
    }
    // The 16-bit form's writeback is implied by the list, so only the 32-bit
    // one can have the base in it and write it back as well.
    if writeback && mask & (1 << rn) != 0 {
        cx.error(
            ins.ops[0].span,
            "the base register cannot be in the list of a transfer that writes it back",
        );
        return None;
    }
    check_list(cx, ins.ops[1].span, mask, load)?;
    if let Some(rt) = sole_register(mask) {
        // GNU as writes a single-register block transfer as the load or
        // store that means the same thing, which is how a `.n` reaches a
        // 16-bit encoding at all. Through the stack pointer with no
        // writeback its own rewrite only reaches the 16-bit form, and goes
        // wrong where that does not fit, so the block form stays -- which
        // is what llvm-mc writes for every one of these.
        let narrow_only = rn == reg::SP && !writeback;
        if !narrow_only || narrow_stack_transfer(ins, rt) {
            return single_transfer(cx, ins, rt, rn, load, mode.before, writeback);
        }
    }
    if !want_wide(ins) {
        return no_encoding(cx, ins);
    }
    let base: u16 = match (load, mode.before) {
        (false, false) => 0xe880,
        (true, false) => 0xe890,
        (false, true) => 0xe900,
        (true, true) => 0xe910,
    };
    Some(wide(base | (u16::from(writeback) << 5) | rn as u16, mask))
}

// ---- 32-bit arithmetic -----------------------------------------------------

fn multiply(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    let ops = ins.ops;
    for op in ops {
        bad_regs(cx, op)?;
    }
    match ins.mnem {
        Mnem::Mul => {
            // `mul rd, rn` shares the destination with the second source.
            encode::arity(cx, ins, &[2, 3])?;
            let rd = encode::reg_of(cx, &ops[0])?;
            let rn = encode::reg_of(cx, &ops[1])?;
            let rm = if ops.len() == 3 {
                encode::reg_of(cx, &ops[2])?
            } else {
                rd
            };
            // `muls rdm, rn, rdm` is the 16-bit form, and `mul` in an `it`
            // block, where it does not set the flags. Multiplication
            // commutes, so either source may be the destination.
            if sets_flags16(ins) && want_narrow(ins) && low(rn) && low(rm) && (rd == rm || rd == rn)
            {
                let other = if rd == rm { rn } else { rm };
                return Some(narrow(0x4340 | ((other as u16) << 3) | rd as u16));
            }
            if ins.set_flags || !want_wide(ins) {
                return no_encoding(cx, ins);
            }
            Some(wide(
                0xfb00 | rn as u16,
                0xf000 | ((rd as u16) << 8) | rm as u16,
            ))
        }
        Mnem::Mla | Mnem::Mls => {
            wide_only(cx, ins)?;
            encode::no_flags(cx, ins)?;
            encode::arity(cx, ins, &[4])?;
            let rd = encode::reg_of(cx, &ops[0])?;
            let rn = encode::reg_of(cx, &ops[1])?;
            let rm = encode::reg_of(cx, &ops[2])?;
            let ra = encode::reg_of(cx, &ops[3])?;
            let tail: u16 = if ins.mnem == Mnem::Mls { 0x10 } else { 0 };
            Some(wide(
                0xfb00 | rn as u16,
                ((ra as u16) << 12) | ((rd as u16) << 8) | tail | rm as u16,
            ))
        }
        _ => {
            wide_only(cx, ins)?;
            encode::no_flags(cx, ins)?;
            encode::arity(cx, ins, &[4])?;
            let lo = encode::reg_of(cx, &ops[0])?;
            let hi = encode::reg_of(cx, &ops[1])?;
            let rn = encode::reg_of(cx, &ops[2])?;
            let rm = encode::reg_of(cx, &ops[3])?;
            let base: u16 = match ins.mnem {
                Mnem::Smull => 0xfb80,
                Mnem::Umlal => 0xfbe0,
                Mnem::Smlal => 0xfbc0,
                _ => 0xfba0,
            };
            Some(wide(
                base | rn as u16,
                ((lo as u16) << 12) | ((hi as u16) << 8) | rm as u16,
            ))
        }
    }
}

// ---- status registers and the compare-and-branch ---------------------------

fn status_read(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    wide_only(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rd = encode::reg_of(cx, &ins.ops[0])?;
    bad_reg(cx, ins.ops[0].span, rd)?;
    let rd = rd as u16;
    let name = ins.ops[1].word.clone().unwrap_or_default();
    if let Some((r, m1, m)) = encode::banked(&name) {
        if !encode::banked_available(cx, ins.ops[1].span) {
            return None;
        }
        return Some(wide(
            0xf3e0 | ((r as u16) << 4) | m1 as u16,
            0x8000 | (rd << 8) | 0x20 | ((m as u16) << 4),
        ));
    }
    let r = match name.as_str() {
        "cpsr" | "apsr" => 0,
        "spsr" => 1,
        _ => {
            cx.error(
                ins.ops[1].span,
                "expected `cpsr`, `apsr`, `spsr` or a banked register",
            );
            return None;
        }
    };
    Some(wide(0xf3ef | (r << 4), 0x8000 | (rd << 8)))
}

fn status_write(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    wide_only(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let spec = ins.ops[0].word.clone().unwrap_or_default();
    if let Some((r, m1, m)) = encode::banked(&spec) {
        if !encode::banked_available(cx, ins.ops[0].span) {
            return None;
        }
        let rn = encode::reg_of(cx, &ins.ops[1])?;
        bad_reg(cx, ins.ops[1].span, rn)?;
        let rn = rn as u16;
        return Some(wide(
            0xf380 | ((r as u16) << 4) | rn,
            0x8000 | ((m1 as u16) << 8) | 0x20 | ((m as u16) << 4),
        ));
    }
    let (r, mask) = encode::psr_fields(cx, &ins.ops[0], &spec)?;
    let Some(rn) = ins.ops[1].reg() else {
        cx.error(
            ins.ops[1].span,
            "the Thumb encoding of `msr` takes a register, not an immediate",
        );
        return None;
    };
    bad_reg(cx, ins.ops[1].span, rn)?;
    Some(wide(
        0xf380 | ((r as u16) << 4) | rn as u16,
        0x8000 | ((mask as u16) << 8),
    ))
}

/// `cbz`/`cbnz`'s six-bit forward offset, which sits in two pieces.
///
/// A branch to the next instruction, which the architecture prohibits,
/// becomes a no-op rather than an error, as `md_apply_fix` writes it.
fn scatter_cbz(w: u64, v: i64) -> u64 {
    if v == -2 {
        return 0xbf00;
    }
    let v = v as u64;
    w | ((v & 0x3e) << 2) | ((v & 0x40) << 3)
}

/// `cbz rn, label` and `cbnz rn, label`, which reach forward only, have no
/// relocation, and so no 32-bit form to relax into: GNU as reports a branch
/// out of range rather than widening them.
fn compare_branch(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    unconditional(cx, ins)?;
    encode::no_flags(cx, ins)?;
    encode::arity(cx, ins, &[2])?;
    let rn = encode::reg_of(cx, &ins.ops[0])?;
    if !low(rn) {
        cx.error(ins.ops[0].span, format!("`{}` only tests r0-r7", ins.text));
        return None;
    }
    if ins.width == Width::Wide {
        cx.error(ins.span, format!("`{}` has no 32-bit encoding", ins.text));
        return None;
    }
    let Some(e) = ins.ops[1].imm() else {
        cx.error(ins.ops[1].span, "expected a branch target");
        return None;
    };
    let base: u16 = if ins.mnem == Mnem::Cbnz {
        0xb900
    } else {
        0xb100
    };
    let kind = FixupKind::pcrel(2, 4)
        .with_limits(-2, 126)
        .accepting(|v| v == -2 || ((0..=126).contains(&v) && v % 2 == 0))
        .with_range_hint("`cbz` and `cbnz` only reach 4 to 130 bytes forward")
        .scatter(scatter_cbz);
    Some(vec![fixed(
        (base | rn as u16).to_le_bytes().to_vec(),
        e,
        kind,
        ins.span,
    )])
}
