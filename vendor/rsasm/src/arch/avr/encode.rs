//! The instruction driver: picks the form of a mnemonic the target has,
//! reads its operands constraint by constraint, and assembles the word.
//!
//! This is `md_assemble` and `avr_operands` from `gas/config/tc-avr.c`. The
//! way operand values combine into the word is theirs too, and it is less
//! obvious than it looks: a register operand is shifted into bits 8-4, unless
//! both operands are registers, in which case the second one's low four bits
//! go in bits 3-0 and its fifth bit in bit 9. That one rule is the whole
//! encoding of `add`, `mov`, `movw`, `muls`, `in`, `out`, `adiw` and the
//! rest.

use super::insn::{self, Insn};
use super::isa::{ISA_MEGA, Mcu};
use super::operand::{self, Expr, Operand};
use super::reloc;
use crate::arch::{AsmCtx, InsnRequest};
use crate::cursor::Cursor;
use crate::section::{Fixup, FixupKind, Variant};

/// One instruction being built.
struct Enc {
    word: u32,
    words: u8,
    fixups: Vec<Fixup>,
}

impl Enc {
    /// A field the core fills in, at `offset` bytes into the instruction.
    /// Every operand value goes through one, constants included, so that the
    /// core does the range check and names the limit.
    fn field(&mut self, offset: u32, x: Expr, kind: FixupKind) {
        self.fixups.push(Fixup {
            offset,
            expr: x.e,
            kind,
            span: x.span,
        });
    }

    fn variant(self) -> Variant {
        let bytes = if self.words == 2 {
            self.word.to_le_bytes().to_vec()
        } else {
            (self.word as u16).to_le_bytes().to_vec()
        };
        Variant {
            bytes,
            fixups: self.fixups,
        }
    }
}

/// Whether a constraint letter is a register, which decides how it is shifted
/// into the word: `REGISTER_P`.
fn is_register(c: u8) -> bool {
    matches!(c, b'r' | b'd' | b'w' | b'a' | b'v')
}

pub fn assemble(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    mcu: Mcu,
) -> Option<Vec<Variant>> {
    let forms = insn::forms(mnemonic);
    if forms.is_empty() {
        let msg = if mnemonic == "__gcc_isr" {
            // Only with `-mgcc-isr`, which rsasm does not model.
            "the `__gcc_isr` pseudo-instruction is not supported".to_string()
        } else {
            format!("unknown instruction `{mnemonic}`")
        };
        cx.error(req.mnemonic_span, msg);
        return None;
    }
    // The first form the target has, trying them in table order.
    let Some(mut index) = forms.iter().position(|f| f.isa & mcu.isa == f.isa) else {
        cx.error(
            req.mnemonic_span,
            format!("`{mnemonic}` is not available on {}", mcu.name),
        );
        return None;
    };
    let pieces = Cursor::new(req.operands).split_commas();
    let ops: Vec<Operand<'_>> = pieces.iter().map(|p| Operand::new(p, req.span)).collect();
    // A `?` form is the operand-less `lpm`, `elpm` or `spm`, and the row after
    // it the one with operands. GNU as moves on to that row without checking
    // its instruction set again, so `lpm r0, Z` assembles on a core that has
    // only the plain `lpm`; so does it here.
    if forms[index].ops.starts_with('?') && !ops.is_empty() && index + 1 < forms.len() {
        index += 1;
    }
    let form = &forms[index];
    let mark = cx.exprs.len();
    let enc = encode(cx, req, form, &ops, mcu)?;
    dot_after(cx, mark, u64::from(form.words) * 2);
    Some(vec![enc.variant()])
}

/// Moves every `.` in the operands just parsed to the end of the instruction.
///
/// `avr_operands` reserves the instruction's bytes with `frag_more` before it
/// reads a single operand, so to GNU as for AVR `.` in an operand is already
/// the address of the next instruction: `rjmp .` jumps to the instruction
/// after it, not to itself. The nodes are bound to the statement's start
/// later, so each becomes that start plus the instruction's length.
fn dot_after(cx: &mut AsmCtx<'_>, mark: usize, len: u64) {
    use crate::expr::{BinOp, ExprKind};
    let end = cx.exprs.len();
    for i in mark..end {
        if !matches!(cx.exprs.nodes[i].kind, ExprKind::Here) {
            continue;
        }
        let span = cx.exprs.nodes[i].span;
        let here = cx.exprs.alloc(ExprKind::Here, span);
        let n = cx.exprs.int(len, span);
        cx.exprs.nodes[i].kind = ExprKind::Binary(BinOp::Add, here, n);
    }
}

fn encode(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    form: &Insn,
    ops: &[Operand<'_>],
    mcu: Mcu,
) -> Option<Enc> {
    let letters: Vec<u8> = form.ops.bytes().filter(|&c| c != b',').collect();
    let mut enc = Enc {
        word: form.base as u32,
        words: form.words,
        fixups: Vec::new(),
    };
    // `r=r`: one operand, which is both.
    let same = letters.get(1) == Some(&b'=');
    let wanted = match letters.as_slice() {
        [] | [b'?'] => 0,
        _ if same => 1,
        l => l.len(),
    };
    if ops.len() != wanted {
        let msg = match wanted {
            0 => format!("`{}` takes no operands", form.name),
            1 => format!("`{}` takes one operand", form.name),
            _ => format!("`{}` takes two operands", form.name),
        };
        cx.error(req.span, msg);
        return None;
    }
    if wanted == 0 {
        return Some(enc);
    }

    let c1 = letters[0];
    let mut reg1 = operand(cx, &mut enc, form, c1, ops[0], mcu)? as u32;
    let reg1_present = is_register(c1);
    let mut reg2 = 0u32;
    if letters.len() > 1 {
        let (value, present) = if same {
            (reg1, true)
        } else {
            let c2 = letters[1];
            (
                operand(cx, &mut enc, form, c2, ops[1], mcu)? as u32,
                is_register(c2),
            )
        };
        reg2 = value;
        if reg1_present && present {
            reg2 = (reg2 & 0xf) | ((reg2 << 5) & 0x200);
        } else if present {
            reg2 <<= 4;
        }
    }
    if reg1_present {
        reg1 <<= 4;
    }
    enc.word |= reg1 | reg2;
    Some(enc)
}

/// Reads one operand for constraint `c`, returning the bits it ORs into the
/// word, or 0 for one that becomes a fixup instead.
fn operand(
    cx: &mut AsmCtx<'_>,
    enc: &mut Enc,
    form: &Insn,
    c: u8,
    op: Operand<'_>,
    mcu: Mcu,
) -> Option<u16> {
    match c {
        b'r' | b'd' | b'w' | b'a' | b'v' => operand::register(cx, op, c, mcu),
        b'e' => operand::pointer(cx, op, mcu),
        b'z' => operand::z_pointer(cx, op, form, mcu),
        b'b' => {
            let (mask, disp) = operand::base(cx, op)?;
            enc.field(0, disp, reloc::disp6());
            Some(mask)
        }
        b'h' => {
            enc.field(0, operand::expr(cx, op)?, reloc::call());
            Some(0)
        }
        b'L' => {
            let x = operand::expr(cx, op)?;
            let wraps = match cx.constant(x.e) {
                Some(_) => mcu.isa & ISA_MEGA == 0,
                None => matches!(mcu.mach, 2 | 25 | 4),
            };
            enc.field(0, x, reloc::rel13(wraps));
            Some(0)
        }
        b'l' => {
            enc.field(0, operand::expr(cx, op)?, reloc::rel7());
            Some(0)
        }
        b'i' => {
            enc.field(2, operand::expr(cx, op)?, reloc::data16());
            Some(0)
        }
        b'j' => {
            enc.field(0, operand::expr(cx, op)?, reloc::lds_sts_16());
            Some(0)
        }
        b'M' => {
            let (kind, x) = operand::ldi_expr(cx, op)?;
            enc.field(0, x, kind);
            Some(0)
        }
        b'n' => {
            let x = !operand::constant(cx, op, "the mask", 255)?;
            Some(((x & 0xf) | ((x << 4) & 0xf00)) as u16)
        }
        b'N' => operand::constant(cx, op, "the value", 255).map(|v| v as u16),
        b'K' => {
            enc.field(0, operand::expr(cx, op)?, reloc::adiw6());
            Some(0)
        }
        b's' => operand::constant(cx, op, "the bit number", 7).map(|v| v as u16),
        b'S' => operand::constant(cx, op, "the bit number", 7).map(|v| (v << 4) as u16),
        b'E' => operand::constant(cx, op, "the round number", 15).map(|v| (v << 4) as u16),
        b'P' => {
            enc.field(0, operand::expr(cx, op)?, reloc::port6());
            Some(0)
        }
        b'p' => {
            enc.field(0, operand::expr(cx, op)?, reloc::port5());
            Some(0)
        }
        _ => unreachable!("constraint `{}` in the opcode table", c as char),
    }
}
