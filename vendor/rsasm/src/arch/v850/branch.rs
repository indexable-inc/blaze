//! Conditional branches and `loop`, whose size depends on the distance.
//!
//! A `Bcond` written in source is a 16-bit instruction with a 9-bit
//! displacement, reaching about ±256 bytes. When the target is further, GNU as
//! quietly substitutes something longer, and so does rsasm, by handing layout
//! every candidate smallest first:
//!
//! ```text
//!            V850                         RH850
//! bcond      2: bcond disp9               2: bcond disp9
//!                                         4: bcond disp17
//!            6: b!cond .+6; jr disp22     6: b!cond .+6; jr disp22
//! br         2: br disp9                  2: br disp9
//!            4: jr disp22                 4: jr disp22
//! bsa        2: bsa disp9                 2: bsa disp9
//!                                         4: bsa disp17
//!            8: bsa .+4; br .+6; jr       8: bsa .+4; br .+6; jr
//! ```
//!
//! `bsa` gets its own long form because "saturated" has no inverse condition
//! to branch around the jump with.
//!
//! A branch whose operand is a number is not relaxed: the number is the
//! displacement itself, as in GNU as, and has to fit the short form.
//!
//! One difference from GNU as is deliberate. GNU as sizes a branch to an
//! undefined symbol as if the symbol sat at address 0 of the current section,
//! so the same `bz ext` is 2 bytes near the start of `.text` and 6 bytes
//! further on. rsasm gives any branch whose target is unknown the longest
//! form, whose reach the linker can always honour.

use super::operand::{Arg, ArgKind, Imm, RelFn};
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprKind;
use crate::lexer::LocalDir;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// `br`, the unconditional member of the family.
const ALWAYS: u8 = 0x5;
/// `bsa`, branch if saturated.
const SATURATED: u8 = 0xd;

/// The condition code a branch mnemonic tests, if it is one.
pub fn condition(mnemonic: &str) -> Option<u8> {
    Some(match mnemonic {
        "bv" | "jv" => 0x0,
        "bl" | "bc" | "jl" | "jc" => 0x1,
        "be" | "bz" | "bt" | "je" | "jz" => 0x2,
        "bnh" | "jnh" => 0x3,
        "bn" | "jn" => 0x4,
        "br" | "jbr" => ALWAYS,
        "blt" | "jlt" => 0x6,
        "ble" | "jle" => 0x7,
        "bnv" | "jnv" => 0x8,
        "bnl" | "bnc" | "jnl" | "jnc" => 0x9,
        "bne" | "bnz" | "bf" | "jne" | "jnz" => 0xa,
        "bh" | "jh" => 0xb,
        "bp" | "jp" => 0xc,
        "bsa" => SATURATED,
        "bge" | "jge" => 0xe,
        "bgt" | "jgt" => 0xf,
        _ => return None,
    })
}

/// The 16-bit `Bcond` opcode.
fn short_word(cc: u8) -> u64 {
    0x0580 | cc as u64
}

fn fixup(offset: u32, imm: &Imm, kind: FixupKind) -> Fixup {
    Fixup {
        offset,
        expr: imm.expr,
        kind,
        span: imm.span,
    }
}

fn disp9_kind() -> FixupKind {
    FixupKind::pcrel(2, 0)
        .with_field(9, 2)
        .with_reloc(reloc::PC9)
        .scatter(reloc::disp9)
}

fn disp17_kind() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(17, 2)
        .with_reloc(reloc::PC17)
        .scatter(reloc::disp17)
}

fn disp22_kind() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(22, 2)
        .with_reloc(reloc::PCR22)
        .scatter(reloc::disp22)
}

fn le(words: &[(u64, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (w, n) in words {
        out.extend_from_slice(&w.to_le_bytes()[..*n]);
    }
    out
}

fn one_operand<'a>(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    args: &'a [Arg],
    span: Span,
) -> Option<&'a Imm> {
    match args {
        [
            Arg {
                kind: ArgKind::Imm(imm),
                ..
            },
        ] if imm.func == RelFn::None => Some(imm),
        [
            Arg {
                kind: ArgKind::Imm(imm),
                ..
            },
        ] => {
            cx.error(
                imm.span,
                format!(
                    "`{}` cannot be used for a branch target",
                    imm.func.spelling()
                ),
            );
            None
        }
        [other] => {
            cx.error(
                other.span,
                format!(
                    "`{mnemonic}` takes a branch target, but found {}",
                    other.describe()
                ),
            );
            None
        }
        _ => {
            cx.error(
                span,
                format!(
                    "`{mnemonic}` takes one operand, but {} were given",
                    args.len()
                ),
            );
            None
        }
    }
}

/// Checks a displacement written as a number.
fn literal(cx: &mut AsmCtx<'_>, span: Span, v: i64, lo: i64, hi: i64) -> Option<()> {
    if v < lo || v > hi {
        cx.error(
            span,
            format!("displacement {v} is out of range ({lo} to {hi})"),
        );
        return None;
    }
    if v % 2 != 0 {
        cx.error(span, format!("displacement {v} is not a multiple of 2"));
        return None;
    }
    Some(())
}

/// Every encoding of a `Bcond`, `br` or `bsa`.
pub fn bcond(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    cc: u8,
    args: &[Arg],
    span: Span,
    rh850: bool,
) -> Option<Vec<Variant>> {
    let imm = *one_operand(cx, mnemonic, args, span)?;

    if let Some(v) = cx.constant(imm.expr) {
        literal(cx, imm.span, v, -0x100, 0xfe)?;
        let word = reloc::disp9(short_word(cc), v);
        return Some(vec![Variant::new(le(&[(word, 2)]))]);
    }

    let mut out = vec![Variant {
        bytes: le(&[(short_word(cc), 2)]),
        fixups: vec![fixup(0, &imm, disp9_kind())],
    }];

    // The 17-bit form exists on RH850 for every condition but "always": an
    // unconditional branch that far is just `jr`, which is no longer.
    if rh850 && cc != ALWAYS {
        out.push(Variant {
            bytes: le(&[(0x0001_07e0 | cc as u64, 4)]),
            fixups: vec![fixup(0, &imm, disp17_kind())],
        });
    }

    let jr = 0x0000_0780u64;
    out.push(match cc {
        ALWAYS => Variant {
            bytes: le(&[(jr, 4)]),
            fixups: vec![fixup(0, &imm, disp22_kind())],
        },
        // There is no "not saturated" condition, so skip the jump by taking
        // the branch to it instead: bsa over an unconditional br.
        SATURATED => Variant {
            bytes: le(&[
                (reloc::disp9(short_word(SATURATED), 4), 2),
                (reloc::disp9(short_word(ALWAYS), 6), 2),
                (jr, 4),
            ]),
            fixups: vec![fixup(4, &imm, disp22_kind())],
        },
        // Flipping bit 3 of a condition inverts it: z/nz, lt/ge, and so on.
        _ => Variant {
            bytes: le(&[(reloc::disp9(short_word(cc ^ 8), 6), 2), (jr, 4)]),
            fixups: vec![fixup(2, &imm, disp22_kind())],
        },
    });
    Some(out)
}

/// `loop reg, target`: decrement `reg` and branch back while it is not zero.
///
/// The displacement is a 16-bit *backward* distance, so `loop` only reaches
/// targets behind it. Anything else, GNU as rewrites into the two
/// instructions `loop` stands for, `add -1, reg` and a 17-bit `bne`, and so
/// does rsasm.
pub fn loop_insn(cx: &mut AsmCtx<'_>, args: &[Arg], span: Span) -> Option<Vec<Variant>> {
    let (reg, imm) = match args {
        [
            Arg {
                kind: ArgKind::Reg(r),
                ..
            },
            Arg {
                kind: ArgKind::Imm(imm),
                ..
            },
        ] if imm.func == RelFn::None => (*r, *imm),
        [_, _] => {
            cx.error(
                span,
                "`loop` takes a register and a branch target, such as `loop r1, 1b`",
            );
            return None;
        }
        _ => {
            cx.error(
                span,
                format!("`loop` takes two operands, but {} were given", args.len()),
            );
            return None;
        }
    };
    let word = 0x0001_06e0 | reg as u64;

    // A number is the backward distance, positive, as GNU as reads it.
    if let Some(v) = cx.constant(imm.expr) {
        literal(cx, imm.span, v, 0, 0xfffe)?;
        let word = (word & 0x0001_ffff) | ((v as u64 & 0xfffe) << 16);
        return Some(vec![Variant::new(le(&[(word, 4)]))]);
    }

    let long = Variant {
        bytes: le(&[(0x025f | (reg as u64) << 11, 2), (0x0001_07ea, 4)]),
        fixups: vec![fixup(2, &imm, disp17_kind())],
    };
    if !is_behind(cx, &imm) {
        return Some(vec![long]);
    }

    // The short form's field is measured from the start of the instruction
    // but sits at offset 2, hence `adjust = -2`; that also makes the
    // relocation addend +2, which is what GNU as emits.
    //
    // A fixup's range check is symmetric, and this field takes 0 to -0xfffe.
    // The upper bound needs no check, because only a target defined before
    // the `loop` gets this form at all. The lower bound is a 17-bit signed
    // range shifted by 2, which a second fixup checks without writing
    // anything: it measures from offset 2, so it sees the distance minus 2,
    // and a 17-bit field refuses that below -0x10000.
    let field = FixupKind {
        adjust: -2,
        ..FixupKind::pcrel(2, 0)
    }
    .with_field(17, 2)
    .with_reloc(reloc::PC16U)
    .scatter(reloc::loop16);
    let guard = FixupKind::pcrel(1, 2)
        .with_field(17, 1)
        .scatter(reloc::unchanged);
    let short = Variant {
        bytes: le(&[(word, 4)]),
        fixups: vec![fixup(2, &imm, field), fixup(0, &imm, guard)],
    };
    Some(vec![short, long])
}

/// Whether a branch target is certainly at or before the instruction: a
/// label already defined, or a numeric `1b` reference.
///
/// A backend cannot see which section it is assembling into, so a label
/// defined earlier in a *different* section also counts. In a relocatable
/// object that is harmless: the reference crosses sections, stays unresolved,
/// and layout moves to the long form. In a flat binary, where it resolves,
/// the one case this misses is such a label placed at a higher address than
/// the `loop`; the core would need to tell backends the current section to
/// catch it.
fn is_behind(cx: &AsmCtx<'_>, imm: &Imm) -> bool {
    match cx.exprs.get(imm.expr).kind {
        ExprKind::LocalRef(_, LocalDir::Backward) => true,
        ExprKind::Sym(name) => cx.symbols.lookup(name).is_some_and(|id| {
            matches!(
                cx.symbols.get(id).value,
                crate::symbol::SymbolValue::Label { .. }
            )
        }),
        _ => false,
    }
}
