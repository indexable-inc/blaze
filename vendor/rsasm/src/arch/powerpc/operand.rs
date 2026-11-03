//! PowerPC operand parsing.
//!
//! The grammar is small. An operand is a register name, an expression, or a
//! displaced memory reference `expr(base)`. What makes it interesting is that
//! most operands are written as bare numbers: in `add 3, 4, 5` the `3` is a
//! register, in `addi 3, 4, 5` the last `5` is an immediate, and nothing about
//! the token says which. So the parser does not classify — it records what it
//! saw, and [`super::encode`] reads each operand as the field the instruction
//! form asks for.

use super::reg::{self, Reg};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind};
use crate::source::Span;

/// A `d(rA)` memory reference. `rA` is written as a register or a bare number,
/// and the number 0 means "no base", not "r0" — the ISA reads the RA field
/// that way for every load and store.
#[derive(Copy, Clone, Debug)]
pub struct Mem {
    /// Absent for `(r4)`, which means a zero displacement.
    pub disp: Option<ExprRef>,
    pub base: Value,
    pub base_span: Span,
}

/// The two things an operand position can hold before a form gives it meaning.
#[derive(Copy, Clone, Debug)]
pub enum Value {
    Reg(Reg),
    Expr(ExprRef),
}

#[derive(Copy, Clone, Debug)]
pub enum OperandKind {
    Value(Value),
    Mem(Mem),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
}

impl Operand {
    pub fn describe(&self) -> String {
        match self.kind {
            OperandKind::Value(Value::Reg(r)) => format!("register `{}`", reg::describe(r)),
            OperandKind::Value(Value::Expr(_)) => "an expression".into(),
            OperandKind::Mem(_) => "a memory operand".into(),
        }
    }
}

/// Parses the comma-separated operand list of one instruction.
pub fn parse_list(cx: &mut AsmCtx<'_>, cur: &Cursor<'_>) -> Option<Vec<Operand>> {
    if cur.at_end() {
        return Some(Vec::new());
    }
    let pieces = cur.split_commas();
    let mut out = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let mut pc = Cursor::new(piece);
        if pc.at_end() {
            cx.error(cur.remaining_span(), "empty operand");
            return None;
        }
        let op = parse_one(cx, &mut pc)?;
        if !pc.at_end() && !pc.is_empty() {
            cx.error(pc.peek().span, "unexpected token after operand");
            return None;
        }
        out.push(op);
    }
    Some(out)
}

fn parse_one(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<Operand> {
    let start = cur.peek().span;

    if let Some(r) = try_register(cx, cur) {
        return Some(Operand {
            kind: OperandKind::Value(Value::Reg(r)),
            span: start,
        });
    }

    // `(rA)` with no displacement.
    if cur.check_punct(Punct::LParen) && looks_like_base(cx, cur, 1) {
        let (base, base_span, end) = parse_base(cx, cur)?;
        return Some(Operand {
            kind: OperandKind::Mem(Mem {
                disp: None,
                base,
                base_span,
            }),
            span: start.to(end),
        });
    }

    let e = {
        let mut p = cx.expr_parser();
        p.parse(cur)?
    };
    let end = cx.exprs.span(e);

    // A `(` directly after the expression makes this `d(rA)`. Anything else
    // that follows is the caller's problem to report.
    if cur.check_punct(Punct::LParen) {
        let (base, base_span, close) = parse_base(cx, cur)?;
        return Some(Operand {
            kind: OperandKind::Mem(Mem {
                disp: Some(e),
                base,
                base_span,
            }),
            span: start.to(close),
        });
    }

    Some(Operand {
        kind: OperandKind::Value(Value::Expr(e)),
        span: start.to(end),
    })
}

/// True when the token `n` places ahead could open the base of `(rA)`.
///
/// Without this check a leading `(` would be taken for a parenthesised
/// expression, and `(2+3)*4` as an operand would be misread as a memory
/// reference with base 2.
fn looks_like_base(cx: &AsmCtx<'_>, cur: &Cursor<'_>, n: usize) -> bool {
    let closes = cur.nth(n + 1).is_punct(Punct::RParen);
    match cur.nth(n).kind {
        TokKind::Int(_) => closes,
        TokKind::Ident(name) => closes && reg::is_register(&cx.name(name).to_ascii_lowercase()),
        TokKind::Punct(Punct::Percent) => true,
        _ => false,
    }
}

/// Parses `(rA)` starting at the `(`, returning the base and the span of the
/// closing paren.
fn parse_base(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<(Value, Span, Span)> {
    let open = cur.advance();
    let base_start = cur.peek().span;
    let base = match try_register(cx, cur) {
        Some(r) => Value::Reg(r),
        None => {
            let mut p = cx.expr_parser();
            Value::Expr(p.parse(cur)?)
        }
    };
    let base_span = base_start.to(cur.nth(0).span.shrink_to_lo());
    let close = cur.peek();
    if cur.eat_punct(Punct::RParen).is_none() {
        cx.error(
            open.span.to(close.span),
            "expected `)` to close a memory operand",
        );
        return None;
    }
    Some((base, base_span, close.span))
}

/// Consumes a register name if one is next: `r3`, or `%r3` for sources that
/// carry the AT&T sigil.
fn try_register(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<Reg> {
    let save = cur.pos();
    cur.eat_punct(Punct::Percent);
    if let TokKind::Ident(n) = cur.peek().kind
        && let Some(r) = reg::lookup(&cx.name(n).to_ascii_lowercase())
    {
        cur.advance();
        return Some(r);
    }
    cur.set_pos(save);
    None
}
