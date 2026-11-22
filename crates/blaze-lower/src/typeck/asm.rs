//! `asm!` converted to blaze-typeck's form (INTRINSICS.md P9b): the `AsmForm` row's pieces,
//! operands and `clobber_abi` names become rows of the body's `asm_pieces`, `asm_operands` and
//! `asm_clobbers`, every text they name (a template's text, cooked; a class's or register's
//! name; an ABI's name) a range of the body's one `asm_text`. The architecture the lowering
//! read (`machine/asm_arch.rs`) is carried; none read (a target neither tabled) is refused by
//! name here.
//!
//! Cooking, the Reference, "String literals": "\n" "\r" "\t" "\\" "\0" "\'" "\"" and `\x` (7-bit)
//! and `\u{..}` escapes stand for their character; "a backslash at the end of a line ... the
//! newline and all whitespace at the beginning of the next line are ignored". A raw literal's
//! text is as written. `{{`/`}}` were cut to one brace by the lowering's piece ranges.

use blaze_typeck as tc;

use super::{Conv, Refused, len};
use crate::asm::{AsmKind, AsmPiece, AsmReg};
use crate::form::{BodyRange, List, NameIdx, RowIdx};

/// `text` with its string escapes applied, pushed onto `out`; `None` for an escape the lexer
/// would not have accepted.
fn cook(text: &str, out: &mut blaze_own::AString<'_>) -> Option<()> {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '0' => out.push('\0'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'x' => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                let v = hi * 16 + lo;
                if v > 0x7f {
                    return None;
                }
                out.push(char::from_u32(v)?);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut v = 0u32;
                loop {
                    let d = chars.next()?;
                    if d == '}' {
                        break;
                    }
                    if d == '_' {
                        continue;
                    }
                    v = v.checked_mul(16)?.checked_add(d.to_digit(16)?)?;
                }
                out.push(char::from_u32(v)?);
            }
            '\n' => {
                while chars.next_if(|c| matches!(c, ' ' | '\t' | '\n' | '\r')).is_some() {}
            }
            '\r' if chars.next_if_eq(&'\n').is_some() => {
                while chars.next_if(|c| matches!(c, ' ' | '\t' | '\n' | '\r')).is_some() {}
            }
            _ => return None,
        }
    }
    Some(())
}

impl Conv<'_, '_, '_, '_, '_> {
    /// `text` appended to the body's `asm_text`: its range there.
    fn asm_text_push(&mut self, text: &str) -> (u32, u32) {
        let first = len(self.out.asm_text.len());
        self.out.asm_text.push_str(text);
        (first, len(self.out.asm_text.len()) - first)
    }

    fn asm_name(&mut self, n: NameIdx) -> Result<(u32, u32), Refused> {
        let text = self.v.name_text(n)?;
        Ok(self.asm_text_push(text))
    }

    /// `Expr::Asm { row }` as blaze-typeck's `Expr::Asm`.
    pub(super) fn asm(&mut self, row: RowIdx, r: BodyRange) -> Result<tc::Expr, Refused> {
        let form = self.v.asm_form(self.v.row(row)?)?;
        let Some(arch) = form.arch else {
            return self.refuse("inline assembly on a target other than aarch64 and x86_64 (no `asm!` rules tabled)", r);
        };
        let pieces_start = len(self.out.asm_pieces.len());
        for &n in self.v.rows(form.pieces)? {
            let piece = match self.v.asm_piece(n)? {
                AsmPiece::Text { text, cooked } => {
                    let raw = self.v.text_at(text)?;
                    let first = len(self.out.asm_text.len());
                    if cooked {
                        if cook(raw, &mut self.out.asm_text).is_none() {
                            return self.refuse("an escape in an `asm!` template that is no string escape", r);
                        }
                    } else {
                        self.out.asm_text.push_str(raw);
                    }
                    tc::AsmPiece::Text { first, len: len(self.out.asm_text.len()) - first }
                }
                AsmPiece::Operand { index, modifier, .. } => tc::AsmPiece::Operand { index, modifier },
                AsmPiece::Line => tc::AsmPiece::Line,
            };
            self.out.asm_pieces.push(piece);
        }
        let operands_start = len(self.out.asm_operands.len());
        for &n in self.v.rows(form.operands)? {
            let o = self.v.asm_operand(n)?;
            let kind = match o.kind {
                AsmKind::In => tc::AsmKind::In,
                AsmKind::Out => tc::AsmKind::Out,
                AsmKind::LateOut => tc::AsmKind::LateOut,
                AsmKind::InOut => tc::AsmKind::InOut,
                AsmKind::InLateOut => tc::AsmKind::InLateOut,
                AsmKind::Sym => tc::AsmKind::Sym,
                AsmKind::Const => tc::AsmKind::Const,
                AsmKind::Label => tc::AsmKind::Label,
            };
            let reg = match o.reg {
                Some(AsmReg::Class(c)) => {
                    let (first, len) = self.asm_name(c)?;
                    Some(tc::AsmReg::Class { first, len })
                }
                Some(AsmReg::Explicit(x)) => {
                    let (first, len) = self.asm_name(x)?;
                    Some(tc::AsmReg::Explicit { first, len })
                }
                None => None,
            };
            self.out.asm_operands.push(tc::AsmOperand { kind, reg, expr: o.expr, out_expr: o.out_expr });
        }
        let clobbers_start = len(self.out.asm_clobbers.len());
        for &id in self.v.ids(form.clobber_abis)? {
            let range = self.asm_name(NameIdx(id))?;
            self.out.asm_clobbers.push(range);
        }
        Ok(tc::Expr::Asm {
            pieces: List { start: pieces_start, len: form.pieces.len },
            operands: List { start: operands_start, len: form.operands.len },
            options: form.options,
            clobber_abis: List { start: clobbers_start, len: form.clobber_abis.len },
            arch,
        })
    }
}
