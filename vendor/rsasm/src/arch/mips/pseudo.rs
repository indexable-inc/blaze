//! Macro instructions.
//!
//! Most hand-written MIPS is made of these. They are not encodings the CPU
//! has: `move` is an `or` with `$zero`, `li` is one or two real instructions
//! depending on the constant, and the ordered branches are a `slt` followed by
//! a branch on the result. GNU as calls the multi-instruction ones *macros*
//! and lets `.set nomacro` warn about them; this backend always expands them,
//! because refusing would reject nearly every real source file.
//!
//! Expansions that need a scratch register use `$at`, exactly as GNU as and
//! llvm-mc do.

use super::encode::{Args, Words, branch_fixup, imm, place_imm16, rd, rs, rt};
use super::operand::{Imm, RelocMod, RelocMods};
use super::reg::{self, Reg};
use crate::arch::{AsmCtx, Endian};
use crate::section::Variant;

/// SPECIAL function codes the expansions build on.
const OR: u32 = 0x25;
const NOR: u32 = 0x27;
const SUB: u32 = 0x22;
const SUBU: u32 = 0x23;
const DSUB: u32 = 0x2e;
const DSUBU: u32 = 0x2f;
const SLT: u32 = 0x2a;
const SLTU: u32 = 0x2b;

const ADDIU: u32 = 0x09 << 26;
const DADDIU: u32 = 0x19 << 26;
const ORI: u32 = 0x0d << 26;
const LUI: u32 = 0x0f << 26;
const BEQ: u32 = 0x04 << 26;
const BNE: u32 = 0x05 << 26;
const BEQL: u32 = 0x14 << 26;
const BNEL: u32 = 0x15 << 26;
const BLEZ: u32 = 0x06 << 26;
const BGTZ: u32 = 0x07 << 26;
const REGIMM: u32 = 0x01 << 26;
const BLTZ: u32 = REGIMM;
const BGEZ: u32 = REGIMM | (1 << 16);
const BGEZAL: u32 = REGIMM | (0x11 << 16);

/// True if `name` is a macro this module expands, so the caller can try it
/// before the instruction table.
pub fn is_pseudo(name: &str) -> bool {
    matches!(
        name,
        "move"
            | "not"
            | "neg"
            | "negu"
            | "dneg"
            | "dnegu"
            | "li"
            | "la"
            | "b"
            | "bal"
            | "beqz"
            | "bnez"
            | "beqzl"
            | "bnezl"
            | "bge"
            | "bgt"
            | "ble"
            | "blt"
            | "bgeu"
            | "bgtu"
            | "bleu"
            | "bltu"
    )
}

pub fn expand(
    cx: &mut AsmCtx<'_>,
    name: &str,
    a: &Args<'_>,
    endian: Endian,
    is64: bool,
) -> Option<Variant> {
    let mut w = Words::new(endian);
    match name {
        // Register-to-register aliases, each an arithmetic instruction with
        // `$zero` in one slot.
        "move" | "not" | "neg" | "negu" | "dneg" | "dnegu" => {
            a.arity(cx, 2)?;
            // The doubleword negations expand to `dsub` and `dsubu`, which
            // only a 64-bit implementation has.
            if !is64 && name.starts_with('d') {
                cx.error(
                    a.span,
                    format!("`{name}` is a 64-bit instruction; this target is 32-bit MIPS"),
                );
                return None;
            }
            let (d, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            // Each leaves `$zero` in the field it does not use.
            super::abi::mark_zero(cx.state);
            w.push(match name {
                "move" => rd(d.num) | rs(s.num) | OR,
                "not" => rd(d.num) | rs(s.num) | NOR,
                // Negation subtracts *from* zero, so the source is `rt`.
                "neg" => rd(d.num) | rt(s.num) | SUB,
                "negu" => rd(d.num) | rt(s.num) | SUBU,
                "dneg" => rd(d.num) | rt(s.num) | DSUB,
                _ => rd(d.num) | rt(s.num) | DSUBU,
            });
        }

        "li" => {
            a.arity(cx, 2)?;
            let d = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            // `li` loads a word even on a 64-bit target, so its narrow form
            // stays `addiu`; `la` below is the one that widens.
            load_constant(cx, &mut w, d, v, ADDIU)?;
        }

        "la" => {
            a.arity(cx, 2)?;
            let d = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            // On a 64-bit target an address fills the whole register, so the
            // narrow form has to sign-extend through all 64 bits: `daddiu`,
            // not `addiu`.
            let narrow = if is64 { DADDIU } else { ADDIU };
            if cx.constant(v.expr).is_some() {
                // An address that is already a number is just a constant.
                load_constant(cx, &mut w, d, v, narrow)?;
            } else {
                if !v.mods.is_empty() {
                    cx.error(
                        v.span,
                        "`la` already splits its operand into %hi and %lo halves",
                    );
                    return None;
                }
                // Under `.abicalls` GNU as reaches a symbol through the GOT
                // instead, in a sequence that differs by ABI and by whether
                // the symbol is local; rsasm expands none of it.
                if cx.state.features & super::FEATURE_PIC != 0 {
                    cx.error(
                        v.span,
                        "`la` of a symbol in position-independent code loads a GOT \
                         entry, which rsasm does not expand; write the `lw` with \
                         `%got` or `%got_disp` yourself",
                    );
                    return None;
                }
                let hi = Imm {
                    mods: RelocMods::one(RelocMod::Hi),
                    ..v
                };
                let lo = Imm {
                    mods: RelocMods::one(RelocMod::Lo),
                    ..v
                };
                place_imm16(cx, &mut w, LUI | rt(d.num), hi, "address")?;
                place_imm16(cx, &mut w, narrow | rt(d.num) | rs(d.num), lo, "address")?;
            }
        }

        // Unconditional branches. `b` is `beq $zero, $zero`; `bal` is the
        // always-true `bgezal $zero`, which is why it links.
        "b" | "bal" => {
            a.arity(cx, 1)?;
            let target = a.target(cx, 0)?;
            let word = if name == "b" { BEQ } else { BGEZAL };
            // `beq $zero, $zero` and `bgezal $zero` both name it.
            super::abi::mark_zero(cx.state);
            w.push_fixup(word, target.expr, branch_fixup(), target.span);
        }

        // Each compares a register against `$zero`; the `l` spellings pick
        // the branch that annuls its delay slot.
        "beqz" | "bnez" | "beqzl" | "bnezl" => {
            a.arity(cx, 2)?;
            let s = a.gpr(cx, 0)?;
            let target = a.target(cx, 1)?;
            let word = match name {
                "beqz" => BEQ,
                "bnez" => BNE,
                "beqzl" => BEQL,
                _ => BNEL,
            };
            // The register compared against is `$zero`.
            super::abi::mark_zero(cx.state);
            w.push_fixup(word | rs(s.num), target.expr, branch_fixup(), target.span);
        }

        "bge" | "bgt" | "ble" | "blt" | "bgeu" | "bgtu" | "bleu" | "bltu" => {
            a.arity(cx, 3)?;
            let (x, y) = (a.gpr_unused(cx, 0)?, a.gpr_unused(cx, 1)?);
            let target = a.target(cx, 2)?;
            ordered_branch(cx, &mut w, name, x, y, target);
        }

        _ => return None,
    }
    Some(w.finish())
}

/// `li` / `la` of a value that is already known.
///
/// The split follows GNU as and llvm-mc exactly: a value that fits a
/// sign-extended 16-bit field is one `addiu`, one that fits a zero-extended
/// one is one `ori`, a value with no low half is one `lui`, and only the rest
/// take two instructions. Note the *signed 32-bit* reading: `li $a0,
/// 0xffffffff` is `addiu $a0, $zero, -1`, not a two-instruction sequence.
fn load_constant(cx: &mut AsmCtx<'_>, w: &mut Words, dest: Reg, v: Imm, narrow: u32) -> Option<()> {
    // The whole value goes in, so there is no field for a relocation
    // operator to name part of.
    if !v.mods.is_empty() {
        cx.error(
            v.span,
            format!(
                "a whole register is loaded here, so `{}` has nowhere to go",
                v.mods.written()
            ),
        );
        return None;
    }
    let Some(n) = cx.constant(v.expr) else {
        cx.error(
            v.span,
            "`li` needs a value known at assembly time; use `la` for an address",
        );
        return None;
    };
    if !(-0x8000_0000..=0xffff_ffff).contains(&n) {
        cx.error(
            v.span,
            format!("{n} does not fit in 32 bits; MIPS `li` loads at most a word"),
        );
        return None;
    }
    let n32 = n as i32 as i64;
    let lo = n32 & 0xffff;
    let hi = (n32 >> 16) & 0xffff;
    if (-0x8000..=0x7fff).contains(&n32) {
        // The one-instruction forms add to, or or into, `$zero`.
        super::abi::mark_zero(cx.state);
        w.push(narrow | rt(dest.num) | imm(n32));
    } else if (0..=0xffff).contains(&n32) {
        super::abi::mark_zero(cx.state);
        w.push(ORI | rt(dest.num) | imm(n32));
    } else if lo == 0 {
        w.push(LUI | rt(dest.num) | imm(hi));
    } else {
        w.push(LUI | rt(dest.num) | imm(hi));
        w.push(ORI | rt(dest.num) | rs(dest.num) | imm(lo));
    }
    Some(())
}

/// `blt` and friends: compute the predicate into `$at`, then branch on it.
///
/// Comparing against `$zero` has a one-instruction form for the signed
/// orderings, since `bltz`/`bgez`/`bgtz`/`blez` already test a register's
/// sign. The unsigned orderings have no such shortcut — an unsigned value is
/// never less than zero — so they always go through `sltu`.
fn ordered_branch(cx: &mut AsmCtx<'_>, w: &mut Words, name: &str, x: Reg, y: Reg, target: Imm) {
    let unsigned = name.ends_with('u');
    super::abi::mark(cx.state, x);
    if !unsigned && y == reg::ZERO {
        // `bgez` and its relatives have no field for the second register, so
        // the `$zero` the source wrote is not in the object's masks.

        let word = match name {
            "bge" => BGEZ,
            "blt" => BLTZ,
            "bgt" => BGTZ,
            _ => BLEZ,
        };
        w.push_fixup(word | rs(x.num), target.expr, branch_fixup(), target.span);
        return;
    }
    let set = if unsigned { SLTU } else { SLT };
    // `x < y` for the strict-less orderings, `y < x` for the others; then
    // branch when the flag is set or clear as the ordering requires.
    let (lhs, rhs, branch_if_set) = match name.trim_end_matches('u') {
        "blt" => (x, y, true),
        "bge" => (x, y, false),
        "bgt" => (y, x, true),
        _ => (y, x, false),
    };
    super::abi::mark(cx.state, y);
    super::abi::mark(cx.state, reg::AT);
    // The branch compares the flag against `$zero`.
    super::abi::mark_zero(cx.state);
    w.push(rd(reg::AT.num) | rs(lhs.num) | rt(rhs.num) | set);
    let branch = if branch_if_set { BNE } else { BEQ };
    w.push_fixup(
        branch | rs(reg::AT.num),
        target.expr,
        branch_fixup(),
        target.span,
    );
}
