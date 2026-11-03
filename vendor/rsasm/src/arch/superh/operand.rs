//! SuperH operand parsing.
//!
//! The addressing modes, spelled the way GNU as spells them:
//!
//! ```text
//! rn                      register direct
//! @rn   @rn+   @-rn       indirect, post-increment, pre-decrement
//! @(disp,rn)              indirect with a displacement
//! @(r0,rn)                indirect indexed by r0
//! @(disp,gbr) @(r0,gbr)   GBR-relative
//! @(disp,pc)              PC-relative
//! #imm                    immediate
//! label                   a PC-relative target: a branch destination, or
//!                         the literal `mov.l label, rn` loads
//! ```
//!
//! Nothing here scales or range-checks a displacement: whether `@(8,r1)`
//! means two longs or eight bytes depends on the instruction, so that happens
//! in [`super::encode`].

use super::reg::{self, Ctl, Reg};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::diag::Diagnostic;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind};
use crate::source::Span;

/// A value an operand carries, with where it was written.
#[derive(Copy, Clone, Debug)]
pub struct Value {
    pub expr: ExprRef,
    pub span: Span,
}

#[derive(Copy, Clone, Debug)]
pub enum Kind {
    Reg(Reg),
    /// `#expr`.
    Imm(Value),
    /// `@rn`.
    Ind(u8),
    /// `@rn+`.
    PostInc(u8),
    /// `@-rn`.
    PreDec(u8),
    /// `@(disp,rn)`.
    Disp(Value, u8),
    /// `@(r0,rn)`.
    R0Index(u8),
    /// `@(disp,gbr)`.
    GbrDisp(Value),
    /// `@(r0,gbr)`.
    R0Gbr,
    /// `@(disp,pc)`.
    PcDisp(Value),
    /// A bare expression.
    Addr(Value),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: Kind,
    pub span: Span,
}

impl Operand {
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::Reg(r) => format!("register `{}`", reg::name_of(r)),
            Kind::Imm(_) => "an immediate".into(),
            Kind::Ind(_) => "`@rn`".into(),
            Kind::PostInc(_) => "`@rn+`".into(),
            Kind::PreDec(_) => "`@-rn`".into(),
            Kind::Disp(..) => "`@(disp,rn)`".into(),
            Kind::R0Index(_) => "`@(r0,rn)`".into(),
            Kind::GbrDisp(_) => "`@(disp,gbr)`".into(),
            Kind::R0Gbr => "`@(r0,gbr)`".into(),
            Kind::PcDisp(_) => "`@(disp,pc)`".into(),
            Kind::Addr(_) => "an address".into(),
        }
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

    fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;
        if cur.eat_punct(Punct::Hash).is_some() {
            let v = self.value(cur)?;
            return Some(Operand {
                kind: Kind::Imm(v),
                span: consumed(cur, start),
            });
        }
        if cur.eat_punct(Punct::At).is_some() {
            let kind = self.at(cur, start)?;
            return Some(Operand {
                kind,
                span: consumed(cur, start),
            });
        }
        if let Some(r) = self.peek_register(cur) {
            cur.advance();
            return Some(Operand {
                kind: Kind::Reg(r),
                span: start,
            });
        }
        let v = self.value(cur)?;
        Some(Operand {
            kind: Kind::Addr(v),
            span: consumed(cur, start),
        })
    }

    /// Everything after an `@`.
    fn at(&mut self, cur: &mut Cursor<'_>, start: Span) -> Option<Kind> {
        if cur.eat_punct(Punct::Minus).is_some() {
            let n = self.gpr(cur, "`@-`")?;
            return Some(Kind::PreDec(n));
        }
        if cur.eat_punct(Punct::LParen).is_some() {
            return self.parenthesised(cur, start);
        }
        let n = self.gpr(cur, "`@`")?;
        if cur.eat_punct(Punct::Plus).is_some() {
            return Some(Kind::PostInc(n));
        }
        Some(Kind::Ind(n))
    }

    /// `@(r0,rn)`, `@(r0,gbr)`, `@(disp,rn)`, `@(disp,gbr)` or `@(disp,pc)`,
    /// after the opening parenthesis.
    fn parenthesised(&mut self, cur: &mut Cursor<'_>, start: Span) -> Option<Kind> {
        // A register first can only be the `r0` index: GNU as never reads a
        // register as a displacement, so `@(r1,r2)` is an error rather than
        // a symbol called `r1`.
        let kind = if let Some(first) = self.peek_register(cur) {
            let span = cur.advance().span;
            if first != Reg::Gpr(0) {
                self.cx.error(
                    span,
                    format!(
                        "an indexed address must be `@(r0,...)`; `{}` cannot index",
                        reg::name_of(first)
                    ),
                );
                return None;
            }
            self.comma(cur)?;
            let base_span = cur.peek().span;
            match self.peek_register(cur) {
                Some(Reg::Gpr(n)) => {
                    cur.advance();
                    Kind::R0Index(n)
                }
                Some(Reg::Ctl(Ctl::Gbr)) => {
                    cur.advance();
                    Kind::R0Gbr
                }
                _ => {
                    self.cx.error(
                        base_span,
                        "expected a general register or `gbr` after `@(r0,`",
                    );
                    return None;
                }
            }
        } else {
            let disp = self.value(cur)?;
            self.comma(cur)?;
            let base_span = cur.peek().span;
            match self.peek_register(cur) {
                Some(Reg::Gpr(n)) => {
                    cur.advance();
                    Kind::Disp(disp, n)
                }
                Some(Reg::Ctl(Ctl::Gbr)) => {
                    cur.advance();
                    Kind::GbrDisp(disp)
                }
                Some(Reg::Ctl(Ctl::Pc)) => {
                    cur.advance();
                    Kind::PcDisp(disp)
                }
                _ => {
                    self.cx.error(
                        base_span,
                        "expected a general register, `gbr` or `pc` as the base of `@(disp,...)`",
                    );
                    return None;
                }
            }
        };
        if cur.eat_punct(Punct::RParen).is_none() {
            let t = cur.peek();
            self.cx.diags.emit(
                Diagnostic::error(t.span, "expected `)`").with_note(start, "to close this `@(`"),
            );
            return None;
        }
        Some(kind)
    }

    fn comma(&mut self, cur: &mut Cursor<'_>) -> Option<()> {
        if cur.eat_punct(Punct::Comma).is_none() {
            let t = cur.peek();
            self.cx.error(t.span, "expected `,` inside `@(...)`");
            return None;
        }
        Some(())
    }

    /// A general register, where the addressing mode requires one.
    fn gpr(&mut self, cur: &mut Cursor<'_>, after: &str) -> Option<u8> {
        let t = cur.peek();
        match self.peek_register(cur) {
            Some(Reg::Gpr(n)) => {
                cur.advance();
                Some(n)
            }
            Some(r) => {
                self.cx.error(
                    t.span,
                    format!(
                        "`{}` cannot be used after {after}; only `r0`-`r15` can",
                        reg::name_of(r)
                    ),
                );
                None
            }
            None => {
                self.cx
                    .error(t.span, format!("expected a general register after {after}"));
                None
            }
        }
    }

    fn peek_register(&self, cur: &Cursor<'_>) -> Option<Reg> {
        let TokKind::Ident(n) = cur.peek().kind else {
            return None;
        };
        reg::lookup(self.cx.interner.get(n))
    }

    fn value(&mut self, cur: &mut Cursor<'_>) -> Option<Value> {
        let start = cur.peek().span;
        let expr = self.cx.expr_parser().parse(cur)?;
        Some(Value {
            expr,
            span: consumed(cur, start),
        })
    }
}
