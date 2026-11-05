//! SPARC V8 and V9. `EM_SPARC` / `EM_SPARCV9`.
//!
//! One backend covers both: V9 is a strict superset of V8's user-mode
//! instruction set, so the difference is the pointer width, the ELF machine
//! number, and a set of mnemonics that a 32-bit target refuses. `--arch
//! sparc` is V8 and `--arch sparcv9` is V9.
//!
//! The interesting parts are split up: [`reg`] for the register file,
//! [`operand`] for the operand grammar (including SPARC's prefix `%hi()` /
//! `%lo()` and the thread-local operators, none of which is the generic `@`
//! modifier), [`insn`] for the opcode table, [`encode`] for the three
//! instruction formats, and [`synth`] for the synthetic instructions that
//! most SPARC assembly is actually written in.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
pub mod synth;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, InsnRequest, ModifierSymbols, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi};
use crate::expr::{ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind, Token};
use crate::section::{Fixup, Variant};
use encode::{BranchKind, BranchSuffix};
use insn::Form;
use operand::{Imm, ImmPart, Operand, OperandParser};

pub const NAMES: &[&str] = &["sparc", "sparcv9"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let v9 = match name {
        "sparc" | "sparc32" | "sparcv8" | "v8" => false,
        "sparcv9" | "sparc64" | "v9" => true,
        _ => return None,
    };
    Some(Box::new(Sparc { v9 }))
}

pub struct Sparc {
    v9: bool,
}

impl Architecture for Sparc {
    fn name(&self) -> &'static str {
        if self.v9 { "sparcv9" } else { "sparc" }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["sparc32", "sparc64", "sparcv8", "v8", "v9"]
    }

    /// SPARC is big-endian in every ABI this backend targets.
    fn endian(&self) -> Endian {
        Endian::Big
    }

    fn pointer_bytes(&self, state: &ArchState) -> u8 {
        state.bits / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: if self.v9 { 64 } else { 32 },
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// There is no Intel-flavoured SPARC syntax to support.
    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        if self.v9 {
            43 // EM_SPARCV9
        } else {
            2 // EM_SPARC
        }
    }

    /// SPARC comments with `!`; `#` is a comment only in the first column.
    fn comments(&self) -> crate::arch::CommentSyntax {
        crate::arch::CommentSyntax {
            anywhere: &["!", "//"],
            line_start: &["#"],
        }
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    /// llvm-mc writes the unaligned variant of an absolute data relocation
    /// for a field that is not on its width's boundary, as a DWARF section's
    /// fields often are not.
    fn reloc_at(&self, reloc: u32, offset: u64) -> u32 {
        match reloc {
            reloc::ABS32 if !offset.is_multiple_of(4) => reloc::UA32,
            reloc::ABS64 if !offset.is_multiple_of(8) => reloc::UA64,
            r => r,
        }
    }

    /// llvm-mc, the reference, aligns `.text` to 4 bytes; GNU as leaves it
    /// at 1.
    fn section_align(
        &self,
        _state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        if name == ".text" { 4 } else { 1 }
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        if pcrel {
            reloc::pcrel(size)
        } else {
            reloc::abs(size)
        }
    }

    /// Every thread-local operator makes its target `STT_TLS`, the marks
    /// included: GNU as sets the type from the relocation it wrote, whatever
    /// the symbol was before. The two call marks also put `__tls_get_addr` in
    /// the object even though nothing relocates against it, since they take
    /// the place of the `call`'s own relocation and the linker finds the
    /// function by name.
    ///
    /// These are the operators the operand parser wrapped the argument in;
    /// SPARC has no `@` modifier of its own for the core to pass here.
    fn modifier_symbols(&self, name: &str) -> ModifierSymbols {
        let Some(op) = operand::tls_op(name) else {
            return ModifierSymbols::default();
        };
        ModifierSymbols {
            needs: matches!(op.name, "tgd_call" | "tldm_call").then_some(operand::TLS_GET_ADDR),
            tls: true,
            ..ModifierSymbols::default()
        }
    }

    /// A thread-local operator is operand syntax, read and encoded with the
    /// instruction, so the relocation on the fixup is already the right one;
    /// the wrapper the operand parser left on the expression is only there to
    /// carry what the operator implies about the symbol. Nothing else is a
    /// modifier here, so a `sym@anything` the core found is refused.
    fn fixup_modifier_reloc(
        &self,
        name: &str,
        _kind: &crate::section::FixupKind,
    ) -> crate::arch::FixupModifier {
        match operand::tls_op(name) {
            Some(_) => crate::arch::FixupModifier::Encoded,
            None => crate::arch::FixupModifier::Unknown,
        }
    }

    /// llvm-mc's conventions, as for every SPARC encoding. A V9 frame starts
    /// with the CFA 2047 bytes above `%sp`, the stack bias.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if self.v9 { -8 } else { -4 },
                ra_column: 15,
                initial: vec![cfi::Insn::DefCfa(14, if self.v9 { 2047 } else { 0 })],
                fde_encoding: 0x1b,
                eh_frame_align: if self.v9 { 8 } else { 4 },
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// DWARF numbers the integer registers as the encoding does, and the
    /// floating-point ones from 32 — by name, so V9's `%f62` is 94 even
    /// though the encoding gives it the field value 31.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        let r = reg::lookup(name.strip_prefix('%')?)?;
        match r.class {
            reg::RegClass::Int => Some(r.num as u32),
            reg::RegClass::Float => Some(32 + r.num as u32),
            reg::RegClass::Asr if r.num == 0 => Some(64),
            _ => None,
        }
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        encode::nop_bytes(len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let m = cx.name(req.mnemonic).to_ascii_lowercase();
        let mut cur = req.cursor();

        if let Some(def) = insn::lookup(&m) {
            if def.v9 && cx.state.bits < 64 {
                cx.error(
                    req.mnemonic_span,
                    format!("`{m}` is a SPARC V9 instruction; this target is V8"),
                );
                return None;
            }
            // A branch's `,a` / `,pn` / `,pt` suffixes are separate tokens,
            // so they have to come off before the operands are split on
            // commas.
            let sfx = match def.form {
                Form::Branch { .. } | Form::BranchFloat(_) | Form::BranchReg(_) => {
                    branch_suffix(cx, &mut cur)?
                }
                _ => BranchSuffix::default(),
            };
            let (body, mark) = tls_mark(cx, cur.rest())?;
            let ops = OperandParser { cx }.parse_list(&Cursor::new(body))?;
            let words = match def.form {
                Form::Branch { cond, predicted } => {
                    let kind = BranchKind {
                        cond,
                        predicted,
                        float: false,
                    };
                    encode::branch(cx, &m, req.span, kind, sfx, &ops)
                }
                Form::BranchFloat(cond) => {
                    let kind = BranchKind {
                        cond,
                        predicted: false,
                        float: true,
                    };
                    encode::branch(cx, &m, req.span, kind, sfx, &ops)
                }
                Form::BranchReg(rcond) => encode::branch_reg(cx, &m, req.span, rcond, sfx, &ops),
                form => encode::encode(cx, &m, req.span, form, &ops),
            };
            return attach_mark(cx, words?, mark, &ops);
        }

        if synth::is_synthetic(&m) {
            let (body, mark) = tls_mark(cx, cur.rest())?;
            let ops = OperandParser { cx }.parse_list(&Cursor::new(body))?;
            let words = synth::assemble(cx, &m, req.span, &ops)?;
            return attach_mark(cx, words, mark, &ops);
        }

        cx.error(req.mnemonic_span, format!("unknown instruction `{m}`"));
        None
    }
}

/// Splits the `, %tie_add(x)` an instruction may end with off its operands
/// and parses it, answering with the tokens the operands are left in.
fn tls_mark<'t>(cx: &mut AsmCtx<'_>, toks: &'t [Token]) -> Option<(&'t [Token], Option<Imm>)> {
    let (body, mark) = operand::split_mark(cx, toks);
    let Some((op, rest)) = mark else {
        return Some((body, None));
    };
    let imm = OperandParser { cx }.mark(rest, op)?;
    Some((body, Some(imm)))
}

/// Puts a thread-local mark on the instruction just assembled.
///
/// GNU as makes the operator the instruction's own relocation rather than a
/// field's, so it refuses one on an instruction that has a field to relocate
/// already: every immediate operand claims that one relocation, whether or
/// not its value is known, and so does a branch or call target.
/// `%tgd_call()` and `%tldm_call()` are the exception — they replace the
/// `call`'s displacement, which is why they are allowed only on
/// `call __tls_get_addr`, the one target whose address the linker can work
/// out from the relocation alone.
fn attach_mark(
    cx: &mut AsmCtx<'_>,
    mut words: Vec<Variant>,
    mark: Option<Imm>,
    ops: &[Operand],
) -> Option<Vec<Variant>> {
    let Some(mark) = mark else {
        return Some(words);
    };
    let ImmPart::Tls(op) = mark.part else {
        return Some(words);
    };
    let [v] = &mut words[..] else {
        cx.error(
            mark.span,
            "this instruction cannot carry a thread-local mark",
        );
        return None;
    };
    let call = matches!(op.name, "tgd_call" | "tldm_call");
    let replaces = v
        .fixups
        .first()
        .filter(|f| call && v.fixups.len() == 1 && f.kind.reloc == reloc::WDISP30)
        .is_some_and(|f| calls_tls_get_addr(cx, f.expr));
    if call && !replaces {
        cx.error(
            mark.span,
            format!(
                "`%{}()` goes only on `call {}`, whose displacement it stands in for",
                op.name,
                operand::TLS_GET_ADDR
            ),
        );
        return None;
    }
    if !call && (!v.fixups.is_empty() || ops.iter().any(Operand::has_immediate)) {
        cx.error(
            mark.span,
            format!(
                "`%{}()` becomes the instruction's own relocation, so it goes only on one \
                 with no immediate and no target of its own",
                op.name
            ),
        );
        return None;
    }
    v.fixups.clear();
    v.fixups.push(Fixup {
        offset: 0,
        expr: mark.expr,
        kind: encode::tls_mark_fixup(op.reloc),
        span: mark.span,
    });
    Some(words)
}

/// Whether a `call`'s target is exactly `__tls_get_addr`. GNU as takes no
/// addend and no other name.
fn calls_tls_get_addr(cx: &AsmCtx<'_>, e: ExprRef) -> bool {
    matches!(cx.exprs.get(e).kind, ExprKind::Sym(n) if cx.name(n) == operand::TLS_GET_ADDR)
}

/// Consumes `,a`, `,pn` and `,pt` from the front of a branch's operands.
fn branch_suffix(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<BranchSuffix> {
    let mut sfx = BranchSuffix::default();
    while cur.check_punct(Punct::Comma) {
        let TokKind::Ident(n) = cur.nth(1).kind else {
            break;
        };
        let word = cx.name(n).to_ascii_lowercase();
        match word.as_str() {
            "a" => sfx.annul = true,
            "pn" => sfx.predict = Some(false),
            "pt" => sfx.predict = Some(true),
            _ => break,
        }
        cur.advance();
        cur.advance();
    }
    Some(sfx)
}
