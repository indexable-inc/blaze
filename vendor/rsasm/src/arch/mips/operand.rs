//! MIPS operand parsing.
//!
//! The grammar is small: a register, an expression, or an expression followed
//! by a base register in parentheses.
//!
//! ```text
//! $t0            register        ($sp)          memory, zero displacement
//! 8($sp)         memory          %hi(sym)       relocation modifier
//! -4             immediate       %lo(sym)($gp)  modifier plus base
//! ```
//!
//! The shared lexer has no notion of a register sigil, so `$t0` arrives as
//! `Punct::Dollar` followed by `Ident("t0")`, and `$8` as `Punct::Dollar`
//! followed by `Int(8)`.

use super::reg::{self, Reg, RegClass};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// A `%hi` / `%lo` wrapper around an expression.
///
/// These are not general expression operators: they select which part of an
/// address a 16-bit field gets, and which relocation carries it. The
/// thread-local models and the position-independent ones are spelled the
/// same way and go in the same fields; what sets those apart is that
/// nothing here can ever compute one, since a thread's block and the GOT
/// are laid out by the linker.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum RelocMod {
    #[default]
    None,
    /// `%hi(x)` — bits 31..16 of `x`, biased by 0x8000 so that adding the
    /// sign-extended `%lo(x)` reconstructs `x`.
    Hi,
    /// `%lo(x)` — bits 15..0 of `x`.
    Lo,
    /// `%higher(x)` — bits 47..32, biased the same way so that the halves
    /// below it reconstruct `x`; and `%highest(x)`, bits 63..48. A 64-bit
    /// address takes four such fields, and neither operator exists in o32,
    /// where no relocation carries it.
    Higher,
    Highest,
    /// `%half(x)` — `x` itself in a 16-bit field.
    Half,
    /// `%got(x)` — the offset from `$gp` of the GOT entry for `x`. Against
    /// a local symbol the entry is for the symbol's page and a `%lo` of the
    /// same address completes it, which is the pair `la` expands to in
    /// position-independent o32 code.
    Got,
    /// `%call16(x)` — the same entry for a call, which the linker may point
    /// at a stub instead of at `x`.
    Call16,
    /// `%got_disp(x)` — the GOT entry for `x` itself, in an ABI whose
    /// entries hold whole addresses.
    GotDisp,
    /// `%got_page(x)` and `%got_ofst(x)` — the entry for the page `x` is
    /// in, and `x`'s offset within it.
    GotPage,
    GotOfst,
    /// `%got_hi(x)` and `%got_lo(x)` — the halves of a GOT offset that does
    /// not fit one field, added to `$gp` in between.
    GotHi,
    GotLo,
    /// `%call_hi(x)` and `%call_lo(x)` — the same halves for a call.
    CallHi,
    CallLo,
    /// `%gp_rel(x)`, also spelled `%gprel(x)` — the offset of `x` from
    /// `$gp`, which only the linker knows.
    GpRel,
    /// `%neg(x)` — the negation of what it wraps, which is why it is never
    /// written on its own: `%hi(%neg(%gp_rel(f)))` is how `.cpsetup` turns
    /// a function's address into the `$gp` its caller had.
    Neg,
    /// `%tlsgd(x)` — the GOT entry general dynamic hands to
    /// `__tls_get_addr`, as an offset from `$gp`.
    TlsGd,
    /// `%tlsldm(x)` — the same entry for local dynamic, which names the
    /// module rather than the variable.
    TlsLdm,
    /// `%dtprel_hi(x)` — the high half of the variable's offset within its
    /// module's block, which local dynamic adds to what the call returned.
    DtprelHi,
    /// `%dtprel_lo(x)` — the low half of that offset.
    DtprelLo,
    /// `%gottprel(x)` — the GOT entry initial exec reads, holding the
    /// offset from the thread pointer.
    Gottprel,
    /// `%tprel_hi(x)` — the high half of that offset, for local exec, where
    /// the linker knows it without a GOT entry.
    TprelHi,
    /// `%tprel_lo(x)` — the low half of it.
    TprelLo,
}

impl RelocMod {
    /// The relocation type this operator asks for. `None` has none: a bare
    /// immediate takes the field's own.
    pub fn reloc(self) -> u32 {
        use super::reloc as r;
        match self {
            RelocMod::None => 0,
            RelocMod::Hi => r::HI16,
            RelocMod::Lo => r::LO16,
            RelocMod::Higher => r::HIGHER,
            RelocMod::Highest => r::HIGHEST,
            RelocMod::Half => r::R16,
            RelocMod::Got => r::GOT16,
            RelocMod::Call16 => r::CALL16,
            RelocMod::GotDisp => r::GOT_DISP,
            RelocMod::GotPage => r::GOT_PAGE,
            RelocMod::GotOfst => r::GOT_OFST,
            RelocMod::GotHi => r::GOT_HI16,
            RelocMod::GotLo => r::GOT_LO16,
            RelocMod::CallHi => r::CALL_HI16,
            RelocMod::CallLo => r::CALL_LO16,
            RelocMod::GpRel => r::GPREL16,
            RelocMod::Neg => r::SUB,
            RelocMod::TlsGd => r::TLS_GD,
            RelocMod::TlsLdm => r::TLS_LDM,
            RelocMod::DtprelHi => r::TLS_DTPREL_HI16,
            RelocMod::DtprelLo => r::TLS_DTPREL_LO16,
            RelocMod::Gottprel => r::TLS_GOTTPREL,
            RelocMod::TprelHi => r::TLS_TPREL_HI16,
            RelocMod::TprelLo => r::TLS_TPREL_LO16,
        }
    }

    /// How this operator is spelled, for a diagnostic that names it back.
    pub fn name(self) -> &'static str {
        match self {
            RelocMod::None => "",
            RelocMod::Hi => "%hi",
            RelocMod::Lo => "%lo",
            RelocMod::Higher => "%higher",
            RelocMod::Highest => "%highest",
            RelocMod::Half => "%half",
            RelocMod::Got => "%got",
            RelocMod::Call16 => "%call16",
            RelocMod::GotDisp => "%got_disp",
            RelocMod::GotPage => "%got_page",
            RelocMod::GotOfst => "%got_ofst",
            RelocMod::GotHi => "%got_hi",
            RelocMod::GotLo => "%got_lo",
            RelocMod::CallHi => "%call_hi",
            RelocMod::CallLo => "%call_lo",
            RelocMod::GpRel => "%gp_rel",
            RelocMod::Neg => "%neg",
            RelocMod::TlsGd => "%tlsgd",
            RelocMod::TlsLdm => "%tlsldm",
            RelocMod::DtprelHi => "%dtprel_hi",
            RelocMod::DtprelLo => "%dtprel_lo",
            RelocMod::Gottprel => "%gottprel",
            RelocMod::TprelHi => "%tprel_hi",
            RelocMod::TprelLo => "%tprel_lo",
        }
    }
}

/// The operators wrapping an expression, innermost first — the order the
/// n64 `r_info` packs them in, which is the reverse of how they are
/// written. Empty for a plain expression, and never longer than three,
/// which is all an `r_info` holds.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct RelocMods {
    mods: [RelocMod; 3],
    len: u8,
}

impl RelocMods {
    /// The chain as written, outermost first, reversed into the order the
    /// ABI applies it.
    pub fn from_written(written: &[RelocMod]) -> RelocMods {
        let mut mods = [RelocMod::None; 3];
        for (i, m) in written.iter().rev().enumerate().take(3) {
            mods[i] = *m;
        }
        RelocMods {
            mods,
            len: written.len().min(3) as u8,
        }
    }

    pub fn one(m: RelocMod) -> RelocMods {
        RelocMods::from_written(&[m])
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The operator applied first, which decides what the field holds and
    /// which relocation leads the chain.
    pub fn inner(self) -> RelocMod {
        self.mods[0]
    }

    /// The operator written outermost, which is the last applied.
    pub fn outer(self) -> RelocMod {
        self.mods[self.len.saturating_sub(1) as usize]
    }

    pub fn as_slice(&self) -> &[RelocMod] {
        &self.mods[..self.len as usize]
    }

    /// The relocation the whole chain asks for, composed as n64 packs it.
    pub fn reloc(self) -> u32 {
        let types: Vec<u32> = self.as_slice().iter().map(|m| m.reloc()).collect();
        super::reloc::compose(&types)
    }

    /// The chain as it was written, for a diagnostic.
    pub fn written(self) -> String {
        let mut s = String::new();
        for m in self.as_slice().iter().rev() {
            s.push_str(m.name());
            s.push('(');
        }
        s.push_str("...");
        for _ in self.as_slice() {
            s.push(')');
        }
        s
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Imm {
    pub expr: ExprRef,
    pub mods: RelocMods,
    pub span: Span,
}

#[derive(Copy, Clone, Debug)]
pub struct Mem {
    pub base: Reg,
    /// Absent means a zero displacement, as in `lw $a0, ($sp)`.
    pub disp: Option<Imm>,
}

#[derive(Copy, Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    Imm(Imm),
    Mem(Mem),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
}

impl Operand {
    pub fn gpr(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) if r.is_gpr() => Some(r),
            _ => None,
        }
    }

    pub fn fpr(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) if r.class == RegClass::Fpr => Some(r),
            _ => None,
        }
    }

    pub fn fcc(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) if r.class == RegClass::Fcc => Some(r),
            _ => None,
        }
    }

    pub fn imm(&self) -> Option<Imm> {
        match self.kind {
            OperandKind::Imm(i) => Some(i),
            _ => None,
        }
    }

    pub fn mem(&self) -> Option<Mem> {
        match self.kind {
            OperandKind::Mem(m) => Some(m),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self.kind {
            OperandKind::Reg(r) => format!("register `{}`", reg::name_of(r)),
            OperandKind::Imm(_) => "an immediate".into(),
            OperandKind::Mem(_) => "a memory operand".into(),
        }
    }
}

pub struct OperandParser<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
}

impl OperandParser<'_, '_> {
    /// Parses one operand out of `toks`, which must be consumed entirely.
    pub fn parse_all(&mut self, toks: &[Token], whole: Span) -> Option<Operand> {
        if toks.is_empty() {
            self.cx.error(whole, "empty operand");
            return None;
        }
        let mut cur = Cursor::new(toks);
        let o = self.parse(&mut cur)?;
        if !cur.at_end() {
            self.cx
                .error(cur.peek().span, "unexpected token after operand");
            return None;
        }
        Some(o)
    }

    /// One `$reg` naming an integer register, for the `$gp` setup
    /// directives, whose operands are not an instruction's.
    pub fn gp_register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let tok = cur.peek();
        if !cur.check_punct(Punct::Dollar) {
            self.cx.error(tok.span, "expected a register, as in `$25`");
            return None;
        }
        let r = self.register(cur)?;
        if !r.is_gpr() {
            self.cx.error(
                tok.span,
                format!(
                    "`{}` is not an integer register; `$gp` holds an address",
                    reg::name_of(r)
                ),
            );
            return None;
        }
        Some(r)
    }

    fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;
        if cur.check_punct(Punct::Dollar) {
            let r = self.register(cur)?;
            return Some(Operand {
                kind: OperandKind::Reg(r),
                span: start.to(last_span(cur, start)),
            });
        }
        // `($sp)`: a base register with no displacement written at all.
        if cur.check_punct(Punct::LParen) {
            let base = self.base(cur)?;
            return Some(Operand {
                kind: OperandKind::Mem(Mem { base, disp: None }),
                span: start.to(last_span(cur, start)),
            });
        }
        let imm = self.immediate(cur)?;
        if cur.check_punct(Punct::LParen) {
            let base = self.base(cur)?;
            return Some(Operand {
                kind: OperandKind::Mem(Mem {
                    base,
                    disp: Some(imm),
                }),
                span: start.to(last_span(cur, start)),
            });
        }
        Some(Operand {
            kind: OperandKind::Imm(imm),
            span: imm.span,
        })
    }

    /// `$` followed by a number or an ABI name.
    fn register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let dollar = cur.advance();
        let tok = cur.peek();
        let span = dollar.span.to(tok.span);
        match tok.kind {
            TokKind::Int(v) => {
                cur.advance();
                if v > 31 {
                    self.cx
                        .error(span, format!("`${v}` is not a register; MIPS has $0-$31"));
                    return None;
                }
                Some(Reg::gpr(v as u8))
            }
            TokKind::Ident(n) => {
                cur.advance();
                let name = self.cx.name(n).to_ascii_lowercase();
                match reg::lookup(&name) {
                    Some(r) => Some(r),
                    None => {
                        self.cx.error(span, format!("unknown register `${name}`"));
                        None
                    }
                }
            }
            _ => {
                self.cx.error(span, "expected a register name after `$`");
                None
            }
        }
    }

    /// `( $reg )`.
    fn base(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let open = cur.advance();
        if !cur.check_punct(Punct::Dollar) {
            self.cx
                .error(cur.peek().span, "expected a base register after `(`");
            return None;
        }
        let r = self.register(cur)?;
        if !r.is_gpr() {
            self.cx.error(
                open.span,
                format!(
                    "`{}` cannot be a base register; addresses come from the integer file",
                    reg::name_of(r)
                ),
            );
            return None;
        }
        if cur.eat_punct(Punct::RParen).is_none() {
            self.cx
                .error(cur.peek().span, "expected `)` after the base register");
            return None;
        }
        Some(r)
    }

    /// An expression, optionally wrapped in relocation operators:
    /// `%hi(sym)`, or a chain of them as in `%hi(%neg(%gp_rel(f)))`.
    fn immediate(&mut self, cur: &mut Cursor<'_>) -> Option<Imm> {
        let start = cur.peek().span;
        if !cur.check_punct(Punct::Percent) {
            let expr = self.expr(cur)?;
            let span = start.to(self.cx.exprs.span(expr));
            return Some(Imm {
                expr,
                mods: RelocMods::default(),
                span,
            });
        }
        // An operator may wrap another, and the ABI decides how deep: an
        // n64 `r_info` holds three relocation types, an o32 one holds the
        // first alone, so GNU as reads three operators under the new ABIs
        // and one under o32 and calls anything more a bad expression.
        let depth = if self.cx.state.bits == 64 { 3 } else { 1 };
        let mut written: Vec<RelocMod> = Vec::new();
        while cur.check_punct(Punct::Percent) {
            let m = self.operator(cur, written.len() == depth)?;
            written.push(m);
        }
        let expr = self.expr(cur)?;
        // One `)` per operator, the innermost expression having consumed
        // any parentheses of its own.
        for _ in &written {
            if cur.eat_punct(Punct::RParen).is_none() {
                self.cx.error(
                    cur.peek().span,
                    "expected `)` closing a relocation operator",
                );
                return None;
            }
        }
        let mods = RelocMods::from_written(&written);
        // `%neg` is the one operator that cannot be the outermost: it
        // negates what another reads, and GNU as stops with an internal
        // error on one that wraps nothing else.
        if mods.outer() == RelocMod::Neg {
            self.cx.error(
                start.to(last_span(cur, start)),
                "`%neg` only negates another operator, as in `%hi(%neg(%gp_rel(f)))`; \
                 GNU as has no relocation for it on its own",
            );
            return None;
        }
        Some(Imm {
            expr,
            mods,
            span: start.to(last_span(cur, start)),
        })
    }

    /// One `%name(` at the head of an operand. `full` says the chain has
    /// already reached the depth this ABI's `r_info` holds.
    fn operator(&mut self, cur: &mut Cursor<'_>, full: bool) -> Option<RelocMod> {
        let pct = cur.advance();
        let tok = cur.peek();
        let name = match tok.kind {
            TokKind::Ident(n) => {
                cur.advance();
                self.cx.name(n).to_ascii_lowercase()
            }
            _ => {
                self.cx.error(
                    pct.span.to(tok.span),
                    "expected a relocation name after `%`",
                );
                return None;
            }
        };
        let span = pct.span.to(tok.span);
        let wide = self.cx.state.bits == 64;
        let modifier = match name.as_str() {
            "hi" => RelocMod::Hi,
            "lo" => RelocMod::Lo,
            "half" => RelocMod::Half,
            // elf32-mips has no howto for either, so GNU as answers
            // "relocation %highest isn't supported by the current ABI".
            "higher" | "highest" if !wide => {
                self.cx.error(
                    span,
                    format!(
                        "`%{name}` names part of a 64-bit address, which o32 has no \
                         relocation for"
                    ),
                );
                return None;
            }
            "higher" => RelocMod::Higher,
            "highest" => RelocMod::Highest,
            "got" => RelocMod::Got,
            "call16" => RelocMod::Call16,
            "got_disp" => RelocMod::GotDisp,
            "got_page" => RelocMod::GotPage,
            "got_ofst" => RelocMod::GotOfst,
            "got_hi" => RelocMod::GotHi,
            "got_lo" => RelocMod::GotLo,
            "call_hi" => RelocMod::CallHi,
            "call_lo" => RelocMod::CallLo,
            // GNU as spells the same relocation both ways.
            "gp_rel" | "gprel" => RelocMod::GpRel,
            "neg" => RelocMod::Neg,
            "tlsgd" => RelocMod::TlsGd,
            "tlsldm" => RelocMod::TlsLdm,
            "dtprel_hi" => RelocMod::DtprelHi,
            "dtprel_lo" => RelocMod::DtprelLo,
            "gottprel" => RelocMod::Gottprel,
            "tprel_hi" => RelocMod::TprelHi,
            "tprel_lo" => RelocMod::TprelLo,
            _ => {
                self.cx.error(
                    span,
                    format!(
                        "unsupported relocation operator `%{name}`; this backend has \
                         %hi, %lo, %half, %higher, %highest, the position-independent \
                         %got, %call16, %got_disp, %got_page, %got_ofst, %got_hi, \
                         %got_lo, %call_hi, %call_lo, %gp_rel and %neg, and the \
                         thread-local %tlsgd, %tlsldm, %dtprel_hi, %dtprel_lo, \
                         %gottprel, %tprel_hi and %tprel_lo"
                    ),
                );
                return None;
            }
        };
        if full {
            let most = if wide { "three" } else { "one" };
            self.cx.error(
                span,
                format!(
                    "`%{name}` is one relocation operator too many: this ABI packs {most} \
                     into a relocation"
                ),
            );
            return None;
        }
        if !cur.check_punct(Punct::LParen) {
            self.cx
                .error(cur.peek().span, format!("expected `(` after `%{name}`"));
            return None;
        }
        cur.advance();
        Some(modifier)
    }

    fn expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut p = self.cx.expr_parser();
        p.parse(cur)
    }
}

/// Span of the token the cursor just passed, for closing an operand's span.
fn last_span(cur: &Cursor<'_>, fallback: Span) -> Span {
    match cur.pos().checked_sub(1).and_then(|i| cur.all().get(i)) {
        Some(t) => t.span,
        None => fallback,
    }
}
