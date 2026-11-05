//! Operand parsing, in the shape `msp430_srcoperand` and
//! `msp430_dstoperand` (`gas/config/tc-msp430.c`) give it.
//!
//! The MSP430 encodes an operand as a two-bit addressing mode next to a
//! register number, and gets seven addressing modes out of that by reading
//! `r0`/`PC`, `r2`/`SR` and `r3` specially:
//!
//! | Written | Mode | Register | Extra word |
//! |---|---|---|---|
//! | `rN` | 0 | N | — |
//! | `x(rN)` | 1 | N | `x` |
//! | `sym` | 1 | 0 (`PC`) | `sym - .` |
//! | `&addr` | 1 | 2 (`SR`) | `addr` |
//! | `@rN` | 2 | N | — |
//! | `@rN+` | 3 | N | — |
//! | `#imm` | 3 | 0 (`PC`) | `imm` |
//!
//! The two constant generators replace `#imm` with no word at all for the
//! six values the hardware can make up: `r3` in modes 0 to 3 gives 0, 1, 2
//! and −1, and `r2` in modes 2 and 3 gives 4 and 8.
//!
//! The order the reference tries these in is what decides the odd cases, so
//! this follows it exactly. In particular a bare number is looked at as a
//! register name before it is looked at as an address, so `mov 5, r6` moves
//! `r5`; see [`super::reg::check_reg`].

use super::reg;
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::{BinOp, ExprKind, ExprRef, UnOp};
use crate::lexer::{LocalDir, Punct, TokKind, Token};
use crate::source::Span;

/// An expression and where it was written.
#[derive(Copy, Clone, Debug)]
pub struct Expr {
    pub e: ExprRef,
    pub span: Span,
}

/// Whether the operand is a register, or an expression that needs a word of
/// its own; `OP_REG` and `OP_EXP` in the reference.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Reg,
    Exp,
}

/// Which 16-bit slice of an immediate the `#lo()` family selects.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Part {
    /// No wrapper: the whole value.
    All,
    /// `#lo(x)` and `#llo(x)`: bits 0 to 15.
    Lo,
    /// `#hi(x)` and `#lhi(x)`: bits 16 to 31.
    Hi,
}

/// One parsed operand.
#[derive(Copy, Clone, Debug)]
pub struct Operand {
    /// The addressing mode, `As` or `Ad`.
    pub am: u8,
    pub reg: u8,
    /// Extra words this operand adds to the instruction: 0 or 1.
    pub ol: u8,
    pub mode: Mode,
    /// The expression, when [`Operand::mode`] is [`Mode::Exp`].
    pub x: Option<Expr>,
    /// Its value, where the file already knows it.
    pub value: Option<i64>,
    /// Which 16-bit slice of a symbol's value the operand names.
    pub part: Part,
    /// `vshift` in the reference: 1 once the value has to be shifted down by
    /// a word, which is what tells `#hi(sym)` apart from `#lo(sym)`.
    pub vshift: i8,
    pub span: Span,
}

impl Operand {
    fn reg_mode(reg: u8, am: u8, span: Span) -> Operand {
        Operand {
            am,
            reg,
            ol: 0,
            mode: Mode::Reg,
            x: None,
            value: None,
            part: Part::All,
            vshift: 0,
            span,
        }
    }

    fn exp_mode(reg: u8, am: u8, x: Expr, value: Option<i64>) -> Operand {
        Operand {
            am,
            reg,
            ol: 1,
            mode: Mode::Exp,
            x: Some(x),
            value,
            part: Part::All,
            vshift: 0,
            span: x.span,
        }
    }
}

/// What the caller of [`src`] needs to say about the instruction, since the
/// reference's operand parser takes all three as arguments.
#[derive(Copy, Clone, Debug)]
pub struct Rules {
    /// `allow_20bit_values`: the instruction is an MSP430X one, so an
    /// immediate may be 20 bits wide and is not sign-extended from 16.
    pub wide: bool,
    /// `constants_allowed`: `#imm` may become a constant generator. `br` and
    /// `calla` turn this off, since they need a real word to relocate.
    pub constants: bool,
    /// The instruction is `push`, whose short `#4` and `#8` forms the
    /// original MSP430 does not have (silicon erratum CPU4).
    pub push: bool,
    /// The target is a CPUXV2 core, which cannot address indirectly through
    /// the PC.
    pub xv2: bool,
}

/// Splits the token tail into operands on commas.
pub fn split(toks: &[Token]) -> Vec<&[Token]> {
    Cursor::new(toks).split_commas()
}

fn span_of(toks: &[Token], fallback: Span) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => fallback,
    }
}

fn ident_lower(cx: &AsmCtx<'_>, t: &Token) -> Option<String> {
    t.ident().map(|n| cx.name(n).to_ascii_lowercase())
}

/// The register an operand's text starts with, as `check_reg` reads it.
///
/// The reference hands `check_reg` the whole operand, spaces removed, and
/// that stops at the first character that is not a letter or a digit, so
/// `r4+1` and `5(r6)` name `r4` and `r5` where that is all it looks for. In
/// tokens: the first one names a register, and the next, if any, is
/// punctuation.
pub fn leading_reg(cx: &AsmCtx<'_>, toks: &[Token]) -> Option<u8> {
    let first = toks.first()?;
    if toks
        .get(1)
        .is_some_and(|t| !matches!(t.kind, TokKind::Punct(_)))
    {
        return None;
    }
    match first.kind {
        TokKind::Ident(n) => reg::check_reg(cx.name(n)),
        TokKind::Int(v) => reg::number_reg(v as i64),
        _ => None,
    }
}

/// Parses one source operand. `imm` is the reference's `imm_op` out-parameter,
/// which decides some relocation types; see [`super::reloc`].
pub fn src(
    cx: &mut AsmCtx<'_>,
    toks: &[Token],
    fallback: Span,
    rules: Rules,
    imm: &mut bool,
) -> Option<Operand> {
    let span = span_of(toks, fallback);
    let Some(first) = toks.first() else {
        cx.error(span, "missing operand");
        return None;
    };

    if first.is_punct(Punct::Hash) {
        *imm = true;
        return immediate(cx, &toks[1..], span, rules);
    }

    if first.is_punct(Punct::Amp) {
        // `&addr`: absolute, through `SR` in mode 1.
        let x = expr(cx, &toks[1..], span)?;
        let value = known(cx, x.e);
        if let Some(v) = value
            && !in_range(v, rules.wide)
        {
            out_of_range(cx, x.span, v, rules.wide);
            return None;
        }
        return Some(Operand::exp_mode(reg::SR, 1, x, value));
    }

    if first.is_punct(Punct::At) {
        // `@rN` and `@rN+`.
        let plus = toks.iter().any(|t| t.is_punct(Punct::Plus));
        let Some(r) = leading_reg(cx, &toks[1..]) else {
            cx.error(span, "expected a register after `@`");
            return None;
        };
        if rules.xv2 && r == reg::PC {
            cx.error(
                span,
                "a CPUXV2 core cannot address indirectly through the PC",
            );
            return None;
        }
        return Some(Operand::reg_mode(r, if plus { 3 } else { 2 }, span));
    }

    // Everything from here on is what the reference reaches with `imm_op`
    // already set, whether or not it turns out to be indexed.
    *imm = true;

    if let Some(open) = last_open_paren(toks)
        && toks.last().is_some_and(|t| t.is_punct(Punct::RParen))
    {
        let Some(r) = leading_reg(cx, &toks[open + 1..toks.len() - 1]) else {
            cx.error(
                span,
                "expected a register in `x(rN)`; write `#x` for an immediate",
            );
            return None;
        };
        if r == reg::SR {
            cx.error(span, "r2 should not be used in indexed addressing mode");
            return None;
        }
        let x = expr(cx, &toks[..open], span)?;
        let value = known(cx, x.e);
        if let Some(v) = value {
            if !in_range(v, rules.wide) {
                out_of_range(cx, x.span, v, rules.wide);
                return None;
            }
            // A zero displacement is register-indirect, with no word.
            if v == 0 {
                return Some(Operand::reg_mode(r, 2, span));
            }
        }
        return Some(Operand::exp_mode(r, 1, x, value));
    }

    if let Some(r) = leading_reg(cx, toks) {
        return Some(Operand::reg_mode(r, 0, span));
    }

    // Symbolic: `x(PC)`, written without the register. An expression that
    // starts with a minus is a constant instead, which the reference reads as
    // mode 3 — the immediate the constant generators could not make.
    let am = if first.is_punct(Punct::Minus) { 3 } else { 1 };
    let x = expr(cx, toks, span)?;
    let value = known(cx, x.e);
    Some(Operand::exp_mode(reg::PC, am, x, value))
}

/// Parses one destination operand: a source operand restricted to modes 0 and
/// 1, with `@rN` rewritten as `0(rN)` the way the reference rewrites it.
pub fn dst(cx: &mut AsmCtx<'_>, toks: &[Token], fallback: Span, rules: Rules) -> Option<Operand> {
    let mut dummy = false;
    let op = src(cx, toks, fallback, rules, &mut dummy)?;
    if op.am == 2 {
        let zero = cx.exprs.alloc(ExprKind::Int(0), op.span);
        let x = Expr {
            e: zero,
            span: op.span,
        };
        return Some(Operand::exp_mode(op.reg, 1, x, Some(0)));
    }
    if op.am > 1 {
        cx.error(
            op.span,
            "this addressing mode is not applicable for destination operand",
        );
        return None;
    }
    Some(op)
}

/// `#imm`, with the `#lo()` family of extractors.
fn immediate(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span, rules: Rules) -> Option<Operand> {
    let (part, vshift, inner) = match wrapper(cx, toks) {
        Some((p, v, inner)) => (p, v, inner),
        None => (Part::All, -1, toks),
    };
    let x = expr(cx, inner, span)?;
    let mut op = Operand {
        ol: 1,
        vshift: vshift.max(0),
        part,
        ..Operand::exp_mode(reg::PC, 3, x, None)
    };

    let Some(mut v) = known(cx, x.e) else {
        if vshift > 1 {
            cx.error(x.span, "#hlo() and #hhi() cannot be used on a symbol");
            return None;
        }
        return Some(op);
    };

    // A constant is extracted here and now, which also means the relocation
    // never has to: only a symbol keeps its `vshift`.
    v = match vshift {
        0 => v & 0xffff,
        1 => (v >> 16) & 0xffff,
        2 | 3 => {
            if v < 0 {
                -1
            } else {
                0
            }
        }
        _ => v,
    };
    if vshift >= 1 {
        op.vshift = 0;
    }
    if !imm_in_range(v, rules.wide) {
        if rules.wide {
            cx.error(x.span, format!("value {v:#x} is out of the 20-bit range"));
        } else {
            cx.error(
                x.span,
                format!("value {v} is out of range; use #lo() or #hi()"),
            );
        }
        return None;
    }
    op.value = Some(v);

    // The constant generators, which the reference substitutes for the six
    // values they can make. `br` and `calla` ask for them to be left alone.
    let narrow = if rules.wide { v } else { v as i16 as i64 };
    if !rules.constants {
        return Some(op);
    }
    let cg = match narrow {
        0 => Some((reg::CG, 0)),
        1 => Some((reg::CG, 1)),
        2 => Some((reg::CG, 2)),
        -1 => Some((reg::CG, 3)),
        // Silicon erratum CPU4: the original MSP430 does not decode the
        // short `push #4` and `push #8`, so the reference leaves them long.
        4 if !rules.push => Some((reg::SR, 2)),
        8 if !rules.push => Some((reg::SR, 3)),
        _ => None,
    };
    match cg {
        Some((r, am)) => Some(Operand {
            value: op.value,
            ..Operand::reg_mode(r, am, op.span)
        }),
        None => Some(op),
    }
}

/// `lo(`, `hi(`, `llo(`, `lhi(`, `hlo(` or `hhi(` wrapping the whole rest of
/// the operand. Returns the part, the reference's `vshift`, and the inside.
fn wrapper<'t>(cx: &AsmCtx<'_>, toks: &'t [Token]) -> Option<(Part, i8, &'t [Token])> {
    if toks.len() < 4
        || !toks[1].is_punct(Punct::LParen)
        || !toks[toks.len() - 1].is_punct(Punct::RParen)
    {
        return None;
    }
    let (part, vshift) = match ident_lower(cx, &toks[0])?.as_str() {
        "lo" => (Part::Lo, 0),
        "hi" => (Part::Hi, 1),
        "llo" => (Part::Lo, 0),
        "lhi" => (Part::Hi, 1),
        "hlo" => (Part::All, 2),
        "hhi" => (Part::All, 3),
        _ => return None,
    };
    Some((part, vshift, &toks[2..toks.len() - 1]))
}

/// The index of the last `(` in the operand, as the reference's `strrchr`
/// finds it.
fn last_open_paren(toks: &[Token]) -> Option<usize> {
    toks.iter().rposition(|t| t.is_punct(Punct::LParen))
}

/// The range an address or displacement may hold: 20 bits for an MSP430X
/// instruction, 16 otherwise. The lower bound is the reference's, which for
/// these two is `-0x7ffff` rather than the `-0x80000` it allows an immediate.
fn in_range(v: i64, wide: bool) -> bool {
    if wide {
        (-0x7ffff..=0xfffff).contains(&v)
    } else {
        (-0x8000..=0xffff).contains(&v)
    }
}

/// The range an immediate may hold.
fn imm_in_range(v: i64, wide: bool) -> bool {
    if wide {
        (-0x80000..=0xfffff).contains(&v)
    } else {
        (-0x8000..=0xffff).contains(&v)
    }
}

fn out_of_range(cx: &mut AsmCtx<'_>, span: Span, v: i64, wide: bool) {
    let what = if wide { "20-bit" } else { "16-bit" };
    cx.error(span, format!("value {v:#x} is out of the {what} range"));
}

/// The value of `e`, if GNU as has a number there as it reads it.
///
/// That is more than [`AsmCtx::constant`] knows: GNU as also folds a
/// difference of two labels in one section with nothing between them that
/// can change size. Not in a code section, though, where the MSP430 linker
/// may relax the code between them (`msp430_allow_local_subtract`).
pub fn known(cx: &AsmCtx<'_>, e: ExprRef) -> Option<i64> {
    if let Some(v) = cx.constant(e) {
        return Some(v);
    }
    let node = cx.exprs.get(e);
    match node.kind {
        ExprKind::Binary(op, l, r) => {
            if op == BinOp::Sub
                && let (Some(to), Some(from)) = (label(cx, l), label(cx, r))
            {
                let section = cx.label_position(to)?.0;
                if cx.sections[section.0 as usize].flags.exec {
                    return None;
                }
                return cx.fixed_distance(cx.label_position(from)?, cx.label_position(to)?);
            }
            let (a, b) = (known(cx, l)?, known(cx, r)?);
            match op {
                BinOp::Add => Some(a.wrapping_add(b)),
                BinOp::Sub => Some(a.wrapping_sub(b)),
                _ => None,
            }
        }
        ExprKind::Unary(UnOp::Neg, x) => known(cx, x).map(i64::wrapping_neg),
        _ => None,
    }
}

/// The label an expression names, if it is only a label.
fn label(cx: &AsmCtx<'_>, e: ExprRef) -> Option<crate::symbol::SymbolId> {
    let node = cx.exprs.get(e);
    let id = match node.kind {
        ExprKind::SymId(id) => id,
        ExprKind::Sym(name) => cx.symbols.lookup(name)?,
        ExprKind::LocalRef(n, LocalDir::Backward) => cx.symbols.local_backward(n, node.span)?,
        _ => return None,
    };
    cx.label_position(id).map(|_| id)
}

/// Parses `toks` as one complete expression.
fn expr(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<Expr> {
    if toks.is_empty() {
        cx.error(span, "missing operand");
        return None;
    }
    let mut cur = Cursor::new(toks);
    let e = cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        cx.error(cur.remaining_span(), "unexpected tokens after the operand");
        return None;
    }
    Some(Expr {
        e,
        span: span_of(toks, span),
    })
}
