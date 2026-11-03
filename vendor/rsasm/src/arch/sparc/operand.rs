//! SPARC operand parsing.
//!
//! SPARC operands are simple enough that one grammar covers all of them:
//!
//! ```text
//! %g1                 a register
//! 42                  an expression
//! %hi(sym) %lo(sym)   the two halves of a 32-bit constant
//! %tle_hix22(sym)     a step of a thread-local access model
//! [%g1 + %g2]         a memory address
//! [%g1 - 8]  [%g1]    ... with an immediate, or none at all
//! %o7 + 8             the same address unbracketed, which is how `jmpl`,
//!                     `call`, `flush` and `return` spell their target
//! ```
//!
//! The one thing worth knowing is that `%hi(x)` is *not* the generic `x@hi`
//! modifier the shared expression parser understands: the sigil comes first
//! and the argument is parenthesised, so it has to be recognised here, before
//! the expression parser gets a chance to read `%` as a remainder operator.
//! Every operator is looked up before a register is, since `%tle_hix22`
//! otherwise reads as a register name nothing defines.

use super::reg::{self, Reg, RegClass};
use super::reloc;
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::{ExprKind, ExprRef, UnOp};
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// One step of a thread-local access model, as SPARC spells it: `%tle_hix22`
/// and its relatives.
///
/// These are not modifiers on a field so much as names for a step of a
/// sequence. The relocation says which step of which model, and the linker —
/// the only thing that knows where a variable sits in a thread's block, and
/// the only thing that may rewrite a sequence into a cheaper model — writes
/// whatever field there is. Eight of them fill no field at all and mark the
/// instruction they are written after; see [`TlsOp::mark`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TlsOp {
    /// How it is written, without the `%` and the argument.
    pub name: &'static str,
    /// The relocation it asks for.
    pub reloc: u32,
    /// Whether it marks the whole instruction rather than filling a field in
    /// it, which is what decides where it is written.
    pub mark: bool,
}

const fn field(name: &'static str, reloc: u32) -> TlsOp {
    TlsOp {
        name,
        reloc,
        mark: false,
    }
}

const fn mark(name: &'static str, reloc: u32) -> TlsOp {
    TlsOp {
        name,
        reloc,
        mark: true,
    }
}

/// Every thread-local operator `sparc64-elf-as` reads, in the four models:
/// local exec, initial exec, general dynamic and local dynamic. llvm-mc reads
/// the same eighteen names and writes the same relocations for them.
const TLS_OPS: &[TlsOp] = &[
    field("tle_hix22", reloc::TLS_LE_HIX22),
    field("tle_lox10", reloc::TLS_LE_LOX10),
    field("tie_hi22", reloc::TLS_IE_HI22),
    field("tie_lo10", reloc::TLS_IE_LO10),
    mark("tie_ld", reloc::TLS_IE_LD),
    mark("tie_ldx", reloc::TLS_IE_LDX),
    mark("tie_add", reloc::TLS_IE_ADD),
    field("tgd_hi22", reloc::TLS_GD_HI22),
    field("tgd_lo10", reloc::TLS_GD_LO10),
    mark("tgd_add", reloc::TLS_GD_ADD),
    mark("tgd_call", reloc::TLS_GD_CALL),
    field("tldm_hi22", reloc::TLS_LDM_HI22),
    field("tldm_lo10", reloc::TLS_LDM_LO10),
    mark("tldm_add", reloc::TLS_LDM_ADD),
    mark("tldm_call", reloc::TLS_LDM_CALL),
    field("tldo_hix22", reloc::TLS_LDO_HIX22),
    field("tldo_lox10", reloc::TLS_LDO_LOX10),
    mark("tldo_add", reloc::TLS_LDO_ADD),
];

/// The thread-local operator `name` spells, written without its `%`.
pub fn tls_op(name: &str) -> Option<TlsOp> {
    TLS_OPS.iter().copied().find(|o| o.name == name)
}

/// The function the dynamic models call, which `%tgd_call()` and
/// `%tldm_call()` mark. The mark takes the place of the `call`'s own
/// displacement, so the linker has only the name to work that displacement
/// out from, and GNU as accordingly takes the mark on no other target.
pub const TLS_GET_ADDR: &str = "__tls_get_addr";

/// Which part of a value an immediate refers to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ImmPart {
    /// The value itself.
    Whole,
    /// `%hi(x)`: bits 31-10, which is exactly what `sethi` writes.
    Hi,
    /// `%lo(x)`: bits 9-0, which always fit a 13-bit signed field.
    Lo,
    /// A step of a thread-local access model, whose field is the linker's
    /// whichever instruction it lands in.
    Tls(TlsOp),
}

impl ImmPart {
    /// The operator as it is written, for a diagnostic that has to name it.
    pub fn spelling(self) -> String {
        match self {
            ImmPart::Whole => String::new(),
            ImmPart::Hi => "`%hi()`".into(),
            ImmPart::Lo => "`%lo()`".into(),
            ImmPart::Tls(op) => format!("`%{}()`", op.name),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Imm {
    pub part: ImmPart,
    pub expr: ExprRef,
    pub span: Span,
}

/// What is added to an address's base register.
#[derive(Copy, Clone, Debug)]
pub enum Offset {
    /// `[%g1]`: no offset at all. Encoded as `+ %g0` with the `i` bit clear,
    /// which is what both GNU as and llvm-mc emit, and is deliberately *not*
    /// the same word as the `[%g1 + 0]` an explicit zero produces.
    None,
    Reg(Reg),
    Imm(Imm),
}

#[derive(Copy, Clone, Debug)]
pub struct Addr {
    pub base: Reg,
    pub offset: Offset,
}

#[derive(Copy, Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    Imm(Imm),
    /// `[...]`, a load or store address.
    Mem(Addr),
    /// `%o7 + 8`, an unbracketed address.
    Addr(Addr),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
}

impl Operand {
    pub fn reg(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) => Some(r),
            _ => None,
        }
    }

    pub fn int_reg(&self) -> Option<Reg> {
        self.reg().filter(Reg::is_int)
    }

    pub fn float_reg(&self) -> Option<Reg> {
        self.reg().filter(Reg::is_float)
    }

    pub fn imm(&self) -> Option<Imm> {
        match self.kind {
            OperandKind::Imm(i) => Some(i),
            _ => None,
        }
    }

    /// The address this operand denotes, if it can be read as one. A bare
    /// integer register is the address `[reg + %g0]`, which is how
    /// `jmpl %o7, %g0` and `flush %g1` are written.
    ///
    /// A bare value is deliberately *not* an address here: `call 0x1234` is
    /// a displacement from the program counter, and it comes through this
    /// same accessor. `jmpl`, `flush` and `return`, where a value does mean
    /// `%g0 + value`, say so themselves.
    pub fn as_addr(&self) -> Option<Addr> {
        match self.kind {
            OperandKind::Mem(a) | OperandKind::Addr(a) => Some(a),
            OperandKind::Reg(r) if r.is_int() => Some(Addr {
                base: r,
                offset: Offset::None,
            }),
            _ => None,
        }
    }

    /// Whether the operand puts an immediate in the instruction's one
    /// relocatable field, which is what keeps a thread-local mark off it.
    pub fn has_immediate(&self) -> bool {
        match self.kind {
            OperandKind::Imm(_) => true,
            OperandKind::Mem(a) | OperandKind::Addr(a) => matches!(a.offset, Offset::Imm(_)),
            OperandKind::Reg(_) => false,
        }
    }

    /// True for `%icc` / `%xcc` / `%fccN`, which is how a V9 predicted branch
    /// or conditional move says which condition-code bank it tests.
    pub fn is_cc(&self) -> bool {
        matches!(self.reg(), Some(r) if matches!(r.class, RegClass::Icc | RegClass::Fcc))
    }

    pub fn describe(&self) -> String {
        match self.kind {
            OperandKind::Reg(r) => format!("register `{}`", reg::name_of(r)),
            OperandKind::Imm(i) => match i.part {
                ImmPart::Whole => "an immediate".into(),
                part => format!("a {} immediate", part.spelling()),
            },
            OperandKind::Mem(_) => "a memory operand".into(),
            OperandKind::Addr(_) => "an address".into(),
        }
    }
}

/// Splits a trailing `, %tie_add(x)` off an instruction's operands.
///
/// A marker is not an operand: it says what the whole instruction is for, and
/// GNU as reads it after the last one, so it has to come off before the
/// operands are split on commas. Answers with the tokens before the comma and
/// the tokens of the operator itself, or the whole list where there is no
/// marker.
pub fn split_mark<'t>(
    cx: &AsmCtx<'_>,
    toks: &'t [Token],
) -> (&'t [Token], Option<(TlsOp, &'t [Token])>) {
    let mut depth = 0i32;
    let mut last = None;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
            TokKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => depth -= 1,
            TokKind::Punct(Punct::Comma) if depth <= 0 => last = Some(i),
            _ => {}
        }
    }
    let Some(i) = last else {
        return (toks, None);
    };
    let rest = &toks[i + 1..];
    let (Some(pct), Some(word), Some(paren)) = (rest.first(), rest.get(1), rest.get(2)) else {
        return (toks, None);
    };
    let TokKind::Ident(n) = word.kind else {
        return (toks, None);
    };
    if !pct.is_punct(Punct::Percent) || word.preceded_by_space || !paren.is_punct(Punct::LParen) {
        return (toks, None);
    }
    match tls_op(&cx.interner.get(n).to_ascii_lowercase()).filter(|o| o.mark) {
        Some(op) => (&toks[..i], Some((op, rest))),
        None => (toks, None),
    }
}

/// Span from `start` through the last token the cursor consumed.
fn consumed(cur: &Cursor<'_>, start: Span) -> Span {
    match cur.pos().checked_sub(1).and_then(|i| cur.all().get(i)) {
        Some(t) => start.to(t.span),
        None => start,
    }
}

pub struct OperandParser<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
}

impl OperandParser<'_, '_> {
    /// Parses every comma-separated operand left in `cur`.
    pub fn parse_list(&mut self, cur: &Cursor<'_>) -> Option<Vec<Operand>> {
        if cur.at_end() {
            return Some(Vec::new());
        }
        let pieces = cur.split_commas();
        let mut out = Vec::with_capacity(pieces.len());
        for piece in pieces {
            let mut pc = Cursor::new(piece);
            if pc.at_end() {
                self.cx.error(cur.remaining_span(), "empty operand");
                return None;
            }
            let o = self.parse(&mut pc)?;
            if !pc.at_end() {
                self.cx
                    .error(pc.peek().span, "unexpected token after operand");
                return None;
            }
            out.push(o);
        }
        Some(out)
    }

    pub fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;

        if cur.eat_punct(Punct::LBracket).is_some() {
            let a = self.address(cur)?;
            if cur.eat_punct(Punct::RBracket).is_none() {
                let t = cur.peek();
                self.cx.error(t.span, "expected `]` to close the address");
                return None;
            }
            return Some(Operand {
                kind: OperandKind::Mem(a),
                span: consumed(cur, start),
            });
        }

        if let Some(part) = self.peek_part(cur) {
            let imm = self.modifier(cur, part)?;
            return Some(Operand {
                kind: OperandKind::Imm(imm),
                span: imm.span,
            });
        }

        if self.peek_register(cur) {
            let r = self.register(cur)?;
            // Only an integer register can carry an offset, and only then is
            // this an address rather than a plain register operand.
            let kind =
                if r.is_int() && (cur.check_punct(Punct::Plus) || cur.check_punct(Punct::Minus)) {
                    OperandKind::Addr(Addr {
                        base: r,
                        offset: self.offset(cur)?,
                    })
                } else {
                    OperandKind::Reg(r)
                };
            return Some(Operand {
                kind,
                span: consumed(cur, start),
            });
        }

        let e = self.expr(cur)?;
        Some(Operand {
            kind: OperandKind::Imm(Imm {
                part: ImmPart::Whole,
                expr: e,
                span: consumed(cur, start),
            }),
            span: consumed(cur, start),
        })
    }

    /// `base` followed by an optional `+ offset` or `- offset`, or an
    /// offset on its own.
    ///
    /// `[ 0x66 ]` is `[ %g0 + 0x66 ]`: the base register is hardwired to
    /// zero, so leaving it out means the same address. That is how GNU's
    /// disassembler prints one, and both references read it back.
    fn address(&mut self, cur: &mut Cursor<'_>) -> Option<Addr> {
        let start = cur.peek().span;
        if !self.peek_register(cur) {
            let offset = if let Some(part) = self.peek_part(cur) {
                Offset::Imm(self.modifier(cur, part)?)
            } else {
                let e = self.expr(cur)?;
                Offset::Imm(Imm {
                    part: ImmPart::Whole,
                    expr: e,
                    span: consumed(cur, start),
                })
            };
            return Some(Addr {
                base: reg::G0,
                offset,
            });
        }
        let base = self.register(cur)?;
        if !base.is_int() {
            self.cx.error(
                start,
                format!(
                    "`{}` cannot be an address base; only the integer registers can",
                    reg::name_of(base)
                ),
            );
            return None;
        }
        Some(Addr {
            base,
            offset: self.offset(cur)?,
        })
    }

    fn offset(&mut self, cur: &mut Cursor<'_>) -> Option<Offset> {
        let negate = if cur.eat_punct(Punct::Plus).is_some() {
            false
        } else if cur.eat_punct(Punct::Minus).is_some() {
            true
        } else {
            return Some(Offset::None);
        };
        let start = cur.peek().span;

        if let Some(part) = self.peek_part(cur) {
            if negate {
                self.cx
                    .error(start, "`%hi()` and `%lo()` cannot be negated here");
                return None;
            }
            return Some(Offset::Imm(self.modifier(cur, part)?));
        }
        if self.peek_register(cur) {
            let r = self.register(cur)?;
            if negate {
                self.cx
                    .error(start, "an index register cannot be subtracted");
                return None;
            }
            if !r.is_int() {
                self.cx.error(
                    start,
                    format!(
                        "`{}` cannot be an address index; only the integer registers can",
                        reg::name_of(r)
                    ),
                );
                return None;
            }
            return Some(Offset::Reg(r));
        }

        let e = self.expr(cur)?;
        let span = consumed(cur, start);
        let expr = if negate {
            self.cx.exprs.alloc(ExprKind::Unary(UnOp::Neg, e), span)
        } else {
            e
        };
        Some(Offset::Imm(Imm {
            part: ImmPart::Whole,
            expr,
            span,
        }))
    }

    fn expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut p = self.cx.expr_parser();
        p.parse(cur)
    }

    /// The identifier one token past a `%`, lowercased.
    fn sigil_word(&self, cur: &Cursor<'_>) -> Option<String> {
        if !cur.check_punct(Punct::Percent) {
            return None;
        }
        // A register name is never separated from its sigil, which is what
        // keeps the remainder in `x % hi` apart from the modifier in `%hi(x)`.
        let tok = cur.nth(1);
        if tok.preceded_by_space {
            return None;
        }
        let TokKind::Ident(n) = tok.kind else {
            return None;
        };
        Some(self.cx.interner.get(n).to_ascii_lowercase())
    }

    /// True for anything shaped like `%name`. An unrecognised name is still
    /// claimed here so that it is reported as an unknown register rather than
    /// falling through to the expression parser, which would only complain
    /// about the `%`.
    fn peek_register(&self, cur: &Cursor<'_>) -> bool {
        self.sigil_word(cur).is_some() && self.peek_part(cur).is_none()
    }

    /// `%hi(`, `%lo(` or a thread-local operator, which only count as
    /// operators when the parenthesis is actually there.
    fn peek_part(&self, cur: &Cursor<'_>) -> Option<ImmPart> {
        let word = self.sigil_word(cur)?;
        let part = match word.as_str() {
            "hi" => ImmPart::Hi,
            "lo" => ImmPart::Lo,
            name => ImmPart::Tls(tls_op(name)?),
        };
        cur.nth(2).is_punct(Punct::LParen).then_some(part)
    }

    /// `%hi(x)` and the rest, from the `%` through the closing parenthesis.
    ///
    /// A thread-local operator becomes an [`ExprKind::Modifier`] around its
    /// argument as well as an [`ImmPart`], which is how the rest of the
    /// assembler learns what the operator implies about the symbol: that it
    /// is thread-local, and for the two call markers that the object refers
    /// to `__tls_get_addr`.
    fn modifier(&mut self, cur: &mut Cursor<'_>, part: ImmPart) -> Option<Imm> {
        let start = cur.peek().span;
        cur.advance(); // `%`
        cur.advance(); // the operator's name
        cur.advance(); // `(`
        let mut e = self.expr(cur)?;
        let close = cur.peek();
        if cur.eat_punct(Punct::RParen).is_none() {
            self.cx.error(
                close.span,
                format!("expected `)` after the argument of {}", part.spelling()),
            );
            return None;
        }
        let span = start.to(close.span);
        if let ImmPart::Tls(op) = part {
            let name = self.cx.interner.intern(op.name);
            e = self.cx.exprs.alloc(ExprKind::Modifier(name, e), span);
        }
        Some(Imm {
            part,
            expr: e,
            span,
        })
    }

    /// The `%tie_add(x)` an instruction ends with, given the tokens
    /// [`split_mark`] set aside for it.
    pub fn mark(&mut self, toks: &[Token], op: TlsOp) -> Option<Imm> {
        let mut cur = Cursor::new(toks);
        let imm = self.modifier(&mut cur, ImmPart::Tls(op))?;
        if !cur.at_end() {
            self.cx
                .error(cur.peek().span, "unexpected token after operand");
            return None;
        }
        Some(imm)
    }

    fn register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let pct = cur.advance(); // `%`
        let tok = cur.peek();
        let TokKind::Ident(n) = tok.kind else {
            self.cx
                .error(pct.span.to(tok.span), "expected a register name after `%`");
            return None;
        };
        cur.advance();
        let text = self.cx.interner.get(n).to_ascii_lowercase();
        if let Some(r) = reg::lookup(&text) {
            return Some(r);
        }
        // `%hi` and the thread-local operators are only operators when a `(`
        // follows, so one written without its argument reaches this far.
        let msg = if text == "hi" || text == "lo" || tls_op(&text).is_some() {
            format!("`%{text}` takes its argument in parentheses: `%{text}(sym)`")
        } else {
            format!("unknown register `%{text}`")
        };
        self.cx.error(pct.span.to(tok.span), msg);
        None
    }
}
