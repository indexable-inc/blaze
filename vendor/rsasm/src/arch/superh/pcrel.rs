//! Branches and PC-relative loads.
//!
//! # The PC base
//!
//! SH executes a branch's delay slot before the branch takes effect, and the
//! program counter a PC-relative field is added to is the address of the
//! instruction plus four: two instructions on, past the delay slot. Every
//! PC-relative field here is therefore a fixup with `adjust = 4`, so the
//! value it receives is `target - (here + 4)` in bytes, and the field stores
//! that divided by the instruction width or the operand size.
//!
//! The longword loads, `mov.l label,rn` and `mova label,r0`, differ in one
//! more way: the CPU clears the low two bits of `PC + 4` first, so the base
//! depends on whether the instruction itself sits on a four-byte boundary.
//! See [`load`].
//!
//! # Odd distances
//!
//! GNU as halves an odd branch or `mov.w` distance and drops the remainder,
//! which only code or data at an odd address can produce. Those are refused
//! here instead, since the instruction would not reach the label it names.
//!
//! # Delay slots
//!
//! Nothing here moves an instruction into a delay slot or fills one: the
//! instruction after a delayed branch is whatever the source put there. The
//! one change of shape is GNU as's own for a conditional branch that cannot
//! reach its target, described at [`cond_branch`].

use super::encode::{Pending, Words, disp8_by2, disp8_by4};
use super::operand::{Kind, Value};
use crate::arch::{AsmCtx, Endian};
use crate::expr::{BinOp, ExprKind, ExprRef, UnOp};
use crate::section::{FixupKind, Variant};
use crate::source::Span;

const NOP: u16 = 0x0009;
const BRA: u16 = 0xa000;

// ---- branches ---------------------------------------------------------------

/// `bt` / `bf` and their delayed forms: eight bits of signed word count.
fn branch8(word: u64, v: i64) -> u64 {
    (word & !0xff) | ((v >> 1) as u64 & 0xff)
}

/// `bra` / `bsr`: twelve bits of signed word count.
fn branch12(word: u64, v: i64) -> u64 {
    (word & !0xfff) | ((v >> 1) as u64 & 0xfff)
}

/// A conditional branch's field: 8 bits of instructions, so 9 bits of byte
/// offset from PC + 4, reaching -256 to +254.
fn branch8_fixup() -> FixupKind {
    FixupKind::pcrel(2, 4).with_field(9, 2).scatter(branch8)
}

/// `bra` / `bsr`: 12 bits of instructions, -4096 to +4094 bytes.
fn branch12_fixup() -> FixupKind {
    FixupKind::pcrel(2, 4).with_field(13, 2).scatter(branch12)
}

/// `bra` and `bsr`, which have no longer form to grow into.
///
/// No relocation is offered: GNU as does not emit one for a branch to
/// another section or to an undefined symbol either (it reports that the
/// displacement overflows), since an out-of-section 12-bit reach is not
/// something a linker could honour in general.
pub fn branch(word: u16, target: Value, endian: Endian) -> Variant {
    let mut w = Words::new(endian);
    w.push(word, [pending(target, branch12_fixup())]);
    w.finish()
}

/// `bt`, `bf`, `bt/s` and `bf/s`.
///
/// SH has no long conditional branch, so when the target is out of an
/// 8-bit reach GNU as rewrites the branch as the opposite condition jumping
/// over a `bra` — and, since a `bra` has a delay slot of its own, puts a `nop`
/// in that slot:
///
/// ```text
/// bt far        =>   bf 1f
///                    bra far
///                    nop
///                 1:
/// ```
///
/// A delayed `bt/s` keeps its own slot instruction for the `bra` instead, so
/// it becomes `bf 1f; bra far; 1:`, four bytes rather than six: the slot
/// instruction written after `bt/s` runs whichever way the branch goes, just
/// as it did before. The rewrite is offered as a second variant, and layout
/// takes it only when the short form does not fit; the boundaries match
/// GNU as's exactly, which the corpus checks. GNU as warns when it does
/// this; the layout pass has no way to, so the change is silent here.
///
/// If the `bra` cannot reach either, that is the error reported.
pub fn cond_branch(word: u16, target: Value, endian: Endian) -> Vec<Variant> {
    let mut short = Words::new(endian);
    short.push(word, [pending(target, branch8_fixup())]);

    // Bit 9 selects true/false and bit 10 the delay slot.
    let delayed = word & 0x0400 != 0;
    let inverted = (word ^ 0x0200) & !0x0400;
    let mut long = Words::new(endian);
    // Skip the `bra` (and its `nop`): from PC + 4, that is 0 or 1 words.
    long.push(inverted | if delayed { 0 } else { 1 }, []);
    long.push(BRA, [pending(target, branch12_fixup())]);
    if !delayed {
        long.push(NOP, []);
    }
    vec![short.finish(), long.finish()]
}

fn pending(v: Value, kind: FixupKind) -> Pending {
    Pending {
        expr: v.expr,
        kind,
        span: v.span,
    }
}

// ---- PC-relative loads ------------------------------------------------------

/// `mov.w`: eight bits of words from PC + 4, so 0 to 510 bytes forward.
fn word_load_fixup() -> FixupKind {
    FixupKind::pcrel(2, 4)
        .with_field(10, 2)
        .with_limits(0, 255 * 2)
        .scatter(disp8_by2)
}

/// `mov.l` / `mova`: eight bits of longs from (PC + 4) & ~3, so 0 to 1020
/// bytes forward, to a target on a four-byte boundary.
///
/// The base is written as (PC + 5) & ~3, which is the same address for any
/// instruction on a two-byte boundary. It differs only for one at an odd
/// address, which can never run, and there it is the base GNU as uses.
fn long_load_fixup() -> FixupKind {
    FixupKind::pcrel(2, 5)
        .with_pc_align(4)
        .with_field(11, 4)
        .with_limits(0, 255 * 4)
        .scatter(disp8_by4)
}

/// `mov.w label,rn`, `mov.l label,rn` and `mova label,r0`, in either
/// spelling: a bare `label`, or `@(disp,pc)`.
///
/// `word` already holds the register. `scale` is the operand size, which is
/// also what the eight-bit field counts in.
///
/// The fields are unsigned, so a literal before the instruction is out of
/// range, as it is to GNU as. The longword loads measure from PC + 4 with
/// its low two bits cleared, so where the instruction sits on a two-byte
/// boundary the base is only two bytes on; either way the literal itself
/// must be on a four-byte boundary. GNU as's own checks come to the same
/// thing: it measures from PC + 4, refuses a literal off a four-byte
/// boundary or more than two bytes back, and stores the distance plus two
/// divided by four.
pub fn load(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    word: u16,
    scale: u8,
    kind: Kind,
    span: Span,
    endian: Endian,
) -> Option<Vec<Variant>> {
    let target = match kind {
        Kind::Addr(v) => v,
        // A constant `n` is the address `. + n`, which GNU as range-checks
        // exactly as it would a label there. `operands_use_location` is what
        // has the core give `.` a label for this spelling.
        Kind::PcDisp(v) if cx.constant(v.expr).is_some() => {
            let here = cx.exprs.alloc(ExprKind::Here, v.span);
            Value {
                expr: cx
                    .exprs
                    .alloc(ExprKind::Binary(BinOp::Add, here, v.expr), v.span),
                span: v.span,
            }
        }
        Kind::PcDisp(v) => {
            // GNU as still reads `@(label,pc)` as plain `label`, warning that
            // the spelling is deprecated; `@(expr,pc)` with a computed `expr`
            // means `. + expr`, which is only supported for constants here.
            if !is_symbol_plus_constant(cx, v.expr) {
                cx.error(
                    v.span,
                    "the displacement in `@(disp,pc)` must be a constant; write the target \
                     label on its own instead",
                );
                return None;
            }
            cx.diags.warning(
                span,
                "`@(label,pc)` is deprecated syntax for a plain `label`",
            );
            v
        }
        // Matching lets nothing else into a PC-relative slot.
        _ => {
            cx.error(span, format!("`{mnemonic}` needs a label here"));
            return None;
        }
    };
    let fixup = if scale == 2 {
        word_load_fixup()
    } else {
        long_load_fixup()
    };
    let mut w = Words::new(endian);
    w.push(word, [pending(target, fixup)]);
    Some(vec![w.finish()])
}

/// True for `sym`, `sym + k` and `sym - k`: the shapes GNU as classifies as a
/// symbol reference rather than a computed displacement.
fn is_symbol_plus_constant(cx: &AsmCtx<'_>, e: ExprRef) -> bool {
    match &cx.exprs.get(e).kind {
        ExprKind::Sym(_) | ExprKind::SymId(_) | ExprKind::LocalRef(..) => true,
        ExprKind::Unary(UnOp::Plus, a) => is_symbol_plus_constant(cx, *a),
        ExprKind::Binary(BinOp::Add, a, b) => {
            (is_symbol_plus_constant(cx, *a) && cx.constant(*b).is_some())
                || (cx.constant(*a).is_some() && is_symbol_plus_constant(cx, *b))
        }
        ExprKind::Binary(BinOp::Sub, a, b) => {
            is_symbol_plus_constant(cx, *a) && cx.constant(*b).is_some()
        }
        _ => false,
    }
}
