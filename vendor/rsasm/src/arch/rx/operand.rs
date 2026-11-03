//! RX operand parsing, in GNU as's spelling.
//!
//! ```text
//! r1                  a general register (`sp` is `r0`)
//! psw                 a control register
//! r1-r5               a register range, for pushm/popm/rtsd
//! #expr               an immediate
//! [r1]  4[r1]         register indirect, with an optional displacement
//! 4[r1].w             ... and the operand-size suffix arithmetic takes
//! [r1+]  [-r1]        post-increment and pre-decrement
//! [r1, r2]            indexed: `[index, base]`
//! expr                a branch target
//! ```
//!
//! The size of a memory operand in `add`, `cmp` and the like is written after
//! it, not on the mnemonic: `add 4[r1].w, r2`. Moves are the other way round
//! (`mov.w 4[r1], r2`), which is why the suffix is kept on the operand here
//! and each instruction decides whether it may have one.

use super::reg::{self, Creg, MemEx};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::intern::Name;
use crate::lexer::{Dialect, Punct, TokKind, Token};
use crate::source::Span;

#[derive(Copy, Clone, Debug)]
pub enum OpKind {
    Reg(u8),
    Creg(Creg),
    Range(u8, u8),
    Imm(ExprRef),
    /// `[base]` or `disp[base]`. `ext` is the `.b`/`.w`/`.l`/`.ub`/`.uw`
    /// suffix, if one was written.
    Mem {
        disp: Option<ExprRef>,
        base: u8,
        ext: Option<MemEx>,
    },
    PostInc(u8),
    PreDec(u8),
    /// `[index, base]`.
    Index {
        index: u8,
        base: u8,
    },
    Expr(ExprRef),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: OpKind,
    pub span: Span,
    /// The operand's text when it is a single bare identifier, so that flag
    /// names (`setpsw c`) can be read without reserving them as symbols.
    pub ident: Option<Name>,
    /// A CC-RX bit length specifier on the immediate or displacement,
    /// `#imm:8` or `dsp:16[r1]`, and where it was written.
    pub width: Option<(u8, Span)>,
}

impl Operand {
    pub fn describe(&self) -> &'static str {
        match self.kind {
            OpKind::Reg(_) => "a register",
            OpKind::Creg(_) => "a control register",
            OpKind::Range(..) => "a register range",
            OpKind::Imm(_) => "an immediate",
            OpKind::Mem { .. } => "a memory operand",
            OpKind::PostInc(_) => "a post-increment operand",
            OpKind::PreDec(_) => "a pre-decrement operand",
            OpKind::Index { .. } => "an indexed operand",
            OpKind::Expr(_) => "an address",
        }
    }
}

fn span_of(toks: &[Token], fallback: Span) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => fallback,
    }
}

fn ident<'a>(cx: &'a AsmCtx<'_>, t: &Token) -> Option<&'a str> {
    match t.kind {
        TokKind::Ident(n) => Some(cx.name(n)),
        _ => None,
    }
}

fn gpr_tok(cx: &AsmCtx<'_>, t: &Token) -> Option<u8> {
    let name = ident(cx, t)?;
    // CC-RX's substitute names `__PID_R0` to `__PID_R15` are those registers
    // (R20UT3248EJ0115 §5.1.10, Table 5.29, pages 471-472).
    if cx.dialect == Dialect::CcRx
        && let Some(prefix) = name.get(..6)
        && prefix.eq_ignore_ascii_case("__pid_")
    {
        let rest = &name[6..];
        return if rest.eq_ignore_ascii_case("sp") {
            None
        } else {
            reg::gpr(rest)
        };
    }
    reg::gpr(name)
}

/// Splits the statement's operand tokens on top-level commas and parses each.
/// Returns `None` after reporting a diagnostic.
pub fn parse_all(cx: &mut AsmCtx<'_>, toks: &[Token], stmt: Span) -> Option<Vec<Operand>> {
    let cur = Cursor::new(toks);
    if cur.at_end() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for piece in cur.split_commas() {
        out.push(parse_one(cx, piece, stmt)?);
    }
    Some(out)
}

fn parse_one(cx: &mut AsmCtx<'_>, toks: &[Token], stmt: Span) -> Option<Operand> {
    let span = span_of(toks, stmt);
    let Some(first) = toks.first() else {
        cx.error(span, "expected an operand");
        return None;
    };
    let single_ident = match (toks.len(), first.kind) {
        (1, TokKind::Ident(n)) => Some(n),
        _ => None,
    };
    let mk = |kind| {
        Some(Operand {
            kind,
            span,
            ident: single_ident,
            width: None,
        })
    };
    if cx.dialect == Dialect::CcRx
        && let Some(t) = toks
            .iter()
            .find(|t| ident(cx, t).is_some_and(|w| w.eq_ignore_ascii_case("__PID_REG")))
    {
        cx.error(
            t.span,
            "`__PID_REG` names the register the Renesas `-pid` option selects, \
             which rsasm has no option for; write the register",
        );
        return None;
    }

    if first.is_punct(Punct::Hash) {
        let mut cur = Cursor::new(&toks[1..]);
        let e = cx.expr_parser().parse(&mut cur)?;
        if let Some(width) = bit_length(cx, &mut cur)? {
            if !cur.at_end() {
                cx.error(
                    cur.remaining_span(),
                    "unexpected tokens after the immediate",
                );
                return None;
            }
            return Some(Operand {
                kind: OpKind::Imm(e),
                span,
                ident: None,
                width: Some(width),
            });
        }
        if !cur.at_end() {
            cx.error(
                cur.remaining_span(),
                "unexpected tokens after the immediate",
            );
            return None;
        }
        return mk(OpKind::Imm(e));
    }

    if first.is_punct(Punct::LBracket) {
        return parse_bracket(cx, toks, None, span, stmt);
    }

    if let Some(r) = gpr_tok(cx, first) {
        match toks.len() {
            1 => return mk(OpKind::Reg(r)),
            3 if toks[1].is_punct(Punct::Minus) => {
                if let Some(r2) = gpr_tok(cx, &toks[2]) {
                    return mk(OpKind::Range(r, r2));
                }
            }
            _ => {}
        }
    }
    if toks.len() == 1
        && let Some(c) = ident(cx, first).and_then(reg::creg)
    {
        return mk(OpKind::Creg(c));
    }

    // An expression: a branch target, or a displacement if `[` follows.
    let mut cur = Cursor::new(toks);
    let e = cx.expr_parser().parse(&mut cur)?;
    if cur.at_end() {
        return mk(OpKind::Expr(e));
    }
    let width = bit_length(cx, &mut cur)?;
    if cur.check_punct(Punct::LBracket) {
        let mut op = parse_bracket(cx, cur.rest(), Some(e), span, stmt)?;
        op.width = width;
        return Some(op);
    }
    cx.error(
        cur.remaining_span(),
        "unexpected tokens in operand; a displacement is written `disp[reg]`",
    );
    None
}

/// Parses `[...]` and any size suffix after it. `toks` starts at the `[`.
fn parse_bracket(
    cx: &mut AsmCtx<'_>,
    toks: &[Token],
    disp: Option<ExprRef>,
    span: Span,
    stmt: Span,
) -> Option<Operand> {
    let close = toks.iter().position(|t| t.is_punct(Punct::RBracket));
    let Some(close) = close else {
        cx.error(span, "expected `]`");
        return None;
    };
    let inner = &toks[1..close];
    let after = &toks[close + 1..];
    let inner_span = span_of(inner, span);
    let bad = |cx: &mut AsmCtx<'_>| {
        cx.error(
            inner_span,
            "expected `[reg]`, `[reg+]`, `[-reg]` or `[index, base]`",
        );
        None
    };

    let mut ext = None;
    match after {
        [] => {}
        [t] => match ident(cx, t).and_then(MemEx::from_suffix) {
            Some(x) => ext = Some(x),
            None => {
                cx.error(
                    t.span,
                    "expected a size suffix (`.b`, `.w`, `.l`, `.ub` or `.uw`) after `]`",
                );
                return None;
            }
        },
        _ => {
            cx.error(span_of(after, stmt), "unexpected tokens after `]`");
            return None;
        }
    }

    let kind = match inner {
        [r] => match gpr_tok(cx, r) {
            Some(base) => OpKind::Mem { disp, base, ext },
            None => return bad(cx),
        },
        [r, plus] if plus.is_punct(Punct::Plus) => match gpr_tok(cx, r) {
            Some(b) => OpKind::PostInc(b),
            None => return bad(cx),
        },
        [minus, r] if minus.is_punct(Punct::Minus) => match gpr_tok(cx, r) {
            Some(b) => OpKind::PreDec(b),
            None => return bad(cx),
        },
        [a, comma, b] if comma.is_punct(Punct::Comma) => match (gpr_tok(cx, a), gpr_tok(cx, b)) {
            (Some(index), Some(base)) => OpKind::Index { index, base },
            _ => return bad(cx),
        },
        _ => return bad(cx),
    };

    // Only a plain `[reg]` can carry a displacement or a size suffix.
    if !matches!(kind, OpKind::Mem { .. }) {
        if disp.is_some() {
            cx.error(span, "a displacement is only allowed with `[reg]`");
            return None;
        }
        if ext.is_some() {
            cx.error(span, "a size suffix is only allowed after `[reg]`");
            return None;
        }
    }
    Some(Operand {
        kind,
        span,
        ident: None,
        width: None,
    })
}

/// A CC-RX bit length specifier, `:width`, at the cursor: 1 to 5, 8, 16, 24
/// or 32 bits (R20UT3248EJ0115 §5.1.5 (3), page 460, and `#imm:1` on page
/// 457). `Some(None)` when there is none; `None` after an error. GNU as's
/// syntax has no such thing, so only CC-RX source looks for one.
fn bit_length(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<Option<(u8, Span)>> {
    if cx.dialect != Dialect::CcRx || !cur.check_punct(Punct::Colon) {
        return Some(None);
    }
    let colon = cur.advance();
    let tok = cur.advance();
    let span = colon.span.to(tok.span);
    match tok.kind {
        TokKind::Int(n @ (1..=5 | 8 | 16 | 24 | 32)) => Some(Some((n as u8, span))),
        _ => {
            cx.error(
                span,
                "expected a bit length specifier: `:1` to `:5`, `:8`, `:16`, `:24` or `:32`",
            );
            None
        }
    }
}
