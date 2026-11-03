//! Operand parsing, one constraint letter at a time.
//!
//! AVR operands have almost no grammar of their own — a register name, a
//! pointer register with `-` or `+`, or an expression — so this follows
//! `avr_operand` in `gas/config/tc-avr.c` constraint by constraint rather
//! than parsing an operand first and matching it against a form afterwards.
//! Which is also why the same text means different things in different
//! places: `x` is a register only in `movw` and `adiw`, and a pointer only in
//! `ld` and `st`.

use super::insn::Insn;
use super::isa::{ISA_MOVW, ISA_SRAM, Mcu};
use super::reloc;
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::section::FixupKind;
use crate::source::Span;

/// One operand: the tokens between two commas, and where they were written.
#[derive(Copy, Clone)]
pub struct Operand<'t> {
    pub toks: &'t [Token],
    pub span: Span,
}

impl<'t> Operand<'t> {
    pub fn new(toks: &'t [Token], fallback: Span) -> Operand<'t> {
        let span = match (toks.first(), toks.last()) {
            (Some(a), Some(b)) => a.span.to(b.span),
            _ => fallback,
        };
        Operand { toks, span }
    }

    /// The whole operand as one lowercased identifier, if that is all it is.
    fn word(&self, cx: &AsmCtx<'_>) -> Option<String> {
        match self.toks {
            [t] => t.ident().map(|n| cx.name(n).to_ascii_lowercase()),
            _ => None,
        }
    }
}

/// An expression and where it was written.
#[derive(Copy, Clone, Debug)]
pub struct Expr {
    pub e: ExprRef,
    pub span: Span,
}

/// The register a name stands for, without any of the per-constraint range
/// checks: `r0`-`r31`, the halves `xl`-`zh`, and, where `wide` (the `movw`
/// and `adiw` constraints), the pair names `x`, `y` and `z`.
fn register_name(name: &str, wide: bool) -> Option<u16> {
    let b = name.as_bytes();
    match b {
        [b'r', d] if d.is_ascii_digit() => Some((d - b'0') as u16),
        [b'r', d, e] if d.is_ascii_digit() && e.is_ascii_digit() => {
            Some(((d - b'0') * 10 + (e - b'0')) as u16)
        }
        [c @ b'x'..=b'z', h @ (b'l' | b'h')] => {
            Some((c - b'x') as u16 * 2 + u16::from(*h == b'h') + 26)
        }
        [c @ b'x'..=b'z'] if wide => Some((c - b'x') as u16 * 2 + 26),
        _ => None,
    }
}

/// A register operand for constraint `c`, returned as the field value that
/// constraint encodes.
///
/// A name that is not a register is read as a constant expression instead, as
/// `avr_operand` does, so `add 1, 2` and `.set n, 5` / `mov r0, n` work.
pub fn register(cx: &mut AsmCtx<'_>, op: Operand<'_>, c: u8, mcu: Mcu) -> Option<u16> {
    let wide = c == b'v' || c == b'w';
    let named = op.word(cx).and_then(|n| register_name(&n, wide));
    let mut r = match named {
        Some(r) => r,
        None => {
            let v = constant(cx, op, "a register number", 31)?;
            v as u16
        }
    };
    // The reduced core has only r16-r31, and numbers them from 16.
    if mcu.mach == 100 {
        if !(16..=31).contains(&r) {
            cx.error(
                op.span,
                "this core has only r16 to r31; a register name or number from 16 to 31 is required",
            );
            return None;
        }
    } else if r > 31 {
        cx.error(
            op.span,
            format!("register number {r} is out of range (0 to 31)"),
        );
        return None;
    }
    match c {
        b'a' => {
            if !(16..=23).contains(&r) {
                cx.error(op.span, "this operand needs a register from r16 to r23");
                return None;
            }
            r -= 16;
        }
        b'd' => {
            if r < 16 {
                cx.error(op.span, "this operand needs a register from r16 to r31");
                return None;
            }
            r -= 16;
        }
        b'v' => {
            if r & 1 != 0 {
                cx.error(
                    op.span,
                    format!(
                        "`r{r}` is odd; this operand needs an even register, or `x`, `y` or `z`"
                    ),
                );
                return None;
            }
            r >>= 1;
        }
        b'w' => {
            if r & 1 != 0 || r < 24 {
                cx.error(
                    op.span,
                    "this operand needs r24, r26, r28 or r30, or `x`, `y` or `z`",
                );
                return None;
            }
            r = (r - 24) >> 1;
        }
        _ => {}
    }
    Some(r)
}

/// The `e` constraint: `X`, `Y` or `Z`, optionally with a `-` before it or a
/// `+` after it. The bits are `avr_operand`'s: 0x100c for X, 8 for Y, 0 for
/// Z, plus 0x1002 for predecrement and 0x1001 for postincrement.
pub fn pointer(cx: &mut AsmCtx<'_>, op: Operand<'_>, mcu: Mcu) -> Option<u16> {
    let mut toks = op.toks;
    let mut mask = 0u16;
    if toks.first().is_some_and(|t| t.is_punct(Punct::Minus)) {
        mask |= 0x1002;
        toks = &toks[1..];
    }
    let post = toks.last().is_some_and(|t| t.is_punct(Punct::Plus));
    if post {
        toks = &toks[..toks.len() - 1];
    }
    let name = Operand::new(toks, op.span).word(cx);
    match name.as_deref() {
        Some("x") => mask |= 0x100c,
        Some("y") => mask |= 0x8,
        Some("z") => {}
        _ => {
            cx.error(op.span, "expected the pointer register `x`, `y` or `z`");
            return None;
        }
    }
    if post {
        if mask & 2 != 0 {
            cx.error(
                op.span,
                "a pointer cannot both predecrement and postincrement",
            );
            return None;
        }
        mask |= 0x1001;
    }
    // avr1 has `ld r, Z` and `st Z, r` and nothing else: no X, no Y, no
    // predecrement and no postincrement.
    if mask & 0x100f != 0 && mcu.isa & ISA_SRAM == 0 {
        cx.error(
            op.span,
            format!("this addressing mode is not available on {}", mcu.name),
        );
        return None;
    }
    Some(mask)
}

/// The `z` constraint of `lpm`, `elpm` and `spm`: `Z`, optionally with `+`.
/// Which bit the `+` sets is in the form's pattern, so `lpm Rd, Z+` sets bit
/// 0 and `spm Z+` bit 4.
pub fn z_pointer(cx: &mut AsmCtx<'_>, op: Operand<'_>, form: &Insn, mcu: Mcu) -> Option<u16> {
    if op.toks.first().is_some_and(|t| t.is_punct(Punct::Minus)) {
        cx.error(op.span, "`z` cannot predecrement here");
        return None;
    }
    let mut toks = op.toks;
    let post = toks.last().is_some_and(|t| t.is_punct(Punct::Plus));
    if post {
        toks = &toks[..toks.len() - 1];
    }
    if Operand::new(toks, op.span).word(cx).as_deref() != Some("z") {
        cx.error(op.span, "expected the pointer register `z`");
        return None;
    }
    if !post {
        return Some(0);
    }
    let mask = form.postinc_bit();
    // The ATtiny26 has `lpm Rd, Z` but not `lpm Rd, Z+`; GNU as tests the
    // same bit, which only the `lpm` and `elpm` forms set.
    if mask & 1 != 0 && mcu.isa & ISA_MOVW == 0 {
        cx.error(
            op.span,
            format!(
                "`z+` is not available on {}: this core has no postincrementing `lpm`",
                mcu.name
            ),
        );
        return None;
    }
    Some(mask)
}

/// The `b` constraint of `ldd` and `std`: `Y` or `Z` and a displacement.
///
/// The `+` is not optional. GNU as reads one character past the register name
/// looking for it, so `ldd r16, Y` ends up as "garbage at end of line" there
/// rather than a zero displacement; this says so instead.
pub fn base(cx: &mut AsmCtx<'_>, op: Operand<'_>) -> Option<(u16, Expr)> {
    let plus = op.toks.iter().position(|t| t.is_punct(Punct::Plus));
    let Some(plus) = plus else {
        cx.error(
            op.span,
            "expected a displacement, as in `y+3`; `ldd` and `std` always have one",
        );
        return None;
    };
    let mask = match Operand::new(&op.toks[..plus], op.span).word(cx).as_deref() {
        Some("y") => 0x8,
        Some("z") => 0,
        _ => {
            cx.error(op.span, "expected the base register `y` or `z`");
            return None;
        }
    };
    let disp = expr(cx, Operand::new(&op.toks[plus + 1..], op.span))?;
    Some((mask, disp))
}

/// A constant expression that must be in `0..=max`, as `avr_get_constant`
/// requires it: a register number written as a number, a bit number, the
/// `des` round, or `cbr`'s mask.
pub fn constant(cx: &mut AsmCtx<'_>, op: Operand<'_>, what: &str, max: i64) -> Option<i64> {
    let e = expr(cx, op)?;
    let Some(v) = cx.constant(e.e) else {
        cx.error(e.span, format!("{what} must be a constant"));
        return None;
    };
    if !(0..=max).contains(&v) {
        cx.error(
            e.span,
            format!("{what} is {v}, which is out of range (0 to {max})"),
        );
        return None;
    }
    Some(v)
}

/// Parses an operand as one complete expression.
pub fn expr(cx: &mut AsmCtx<'_>, op: Operand<'_>) -> Option<Expr> {
    if op.toks.is_empty() {
        cx.error(op.span, "expected an operand");
        return None;
    }
    let mut cur = Cursor::new(op.toks);
    let e = cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        cx.error(cur.remaining_span(), "unexpected tokens after the operand");
        return None;
    }
    Some(Expr { e, span: op.span })
}

/// The `M` constraint: an `ldi`-family immediate, which may be wrapped in one
/// of the byte-selecting modifiers.
///
/// The shapes are `avr_ldi_expression`'s, and each picks a relocation:
///
/// ```text
/// lo8(x)  hi8(x)  hh8(x)  hlo8(x)  hhi8(x)  pm_lo8(x)  pm_hi8(x)  pm_hh8(x)
/// lo8(-(x))       the _NEG relocation of any of them
/// lo8(pm(x))      the _PM relocation, counting the address in words
/// lo8(gs(x))      the _GS relocation, which lets the linker build a stub
/// lo8(-(pm(x)))   both
/// ```
///
/// Anything else is a plain expression with `R_AVR_LDI`.
pub fn ldi_expr(cx: &mut AsmCtx<'_>, op: Operand<'_>) -> Option<(FixupKind, Expr)> {
    match split_modifier(cx, op) {
        Some(m) => {
            let inner = expr(cx, Operand::new(m.inner, op.span))?;
            Some((m.kind, inner))
        }
        None => Some((reloc::ldi(), expr(cx, op)?)),
    }
}

struct Modified<'t> {
    kind: FixupKind,
    inner: &'t [Token],
}

/// The `exp_mod` table of `gas/config/tc-avr.c`: the modifier name, the
/// relocation it picks, and whether a `pm(` or `gs(` may follow it — which
/// moves it to the row below, the one already counting in words.
///
/// Each row is (name, plain, negated, program-memory form or `None`).
type Row = (&'static str, u32, u32, Option<(u32, u32)>);
const MODIFIERS: &[Row] = &[
    (
        "hh8",
        reloc::R_AVR_HH8_LDI,
        reloc::R_AVR_HH8_LDI_NEG,
        Some((reloc::R_AVR_HH8_LDI_PM, reloc::R_AVR_HH8_LDI_PM_NEG)),
    ),
    (
        "pm_hh8",
        reloc::R_AVR_HH8_LDI_PM,
        reloc::R_AVR_HH8_LDI_PM_NEG,
        None,
    ),
    (
        "hi8",
        reloc::R_AVR_HI8_LDI,
        reloc::R_AVR_HI8_LDI_NEG,
        Some((reloc::R_AVR_HI8_LDI_PM, reloc::R_AVR_HI8_LDI_PM_NEG)),
    ),
    (
        "pm_hi8",
        reloc::R_AVR_HI8_LDI_PM,
        reloc::R_AVR_HI8_LDI_PM_NEG,
        None,
    ),
    (
        "lo8",
        reloc::R_AVR_LO8_LDI,
        reloc::R_AVR_LO8_LDI_NEG,
        Some((reloc::R_AVR_LO8_LDI_PM, reloc::R_AVR_LO8_LDI_PM_NEG)),
    ),
    (
        "pm_lo8",
        reloc::R_AVR_LO8_LDI_PM,
        reloc::R_AVR_LO8_LDI_PM_NEG,
        None,
    ),
    ("hlo8", reloc::R_AVR_HH8_LDI, reloc::R_AVR_HH8_LDI_NEG, None),
    ("hhi8", reloc::R_AVR_MS8_LDI, reloc::R_AVR_MS8_LDI_NEG, None),
];

/// Peels a byte-selecting modifier off an `ldi` operand, if it has one.
///
/// Returns `None` for an operand that is just an expression, and reports the
/// error itself for one that starts like a modifier and then goes wrong.
fn split_modifier<'t>(cx: &mut AsmCtx<'_>, op: Operand<'t>) -> Option<Modified<'t>> {
    // Case matters: `avr_ldi_expression` looks the word up as written, so
    // `LO8(x)` is not a modifier there, and GNU as then refuses the operand.
    let name = op.toks.first()?.ident().map(|n| cx.name(n).to_string())?;
    let row = MODIFIERS.iter().find(|(n, ..)| *n == name)?;
    if !op.toks.get(1).is_some_and(|t| t.is_punct(Punct::LParen)) {
        return None;
    }
    let mut rest = &op.toks[2..];
    // The parentheses still to close after the expression, besides the
    // modifier's own.
    let mut closes = 0usize;
    let mut neg = false;
    let mut pm = None;

    // `-(pm(` and `-(gs(` come as one piece, before any other `-(`.
    if let Some(tail) = eat_word_paren(cx, rest, "-")
        .and_then(|t| eat_word_paren(cx, t, "pm").or_else(|| eat_word_paren(cx, t, "gs")))
    {
        let gs = eat_word_paren(cx, &rest[2..], "gs").is_some();
        neg = true;
        pm = Some(gs);
        closes += 2;
        rest = tail;
    } else if let Some(tail) =
        eat_word_paren(cx, rest, "pm").or_else(|| eat_word_paren(cx, rest, "gs"))
    {
        pm = Some(eat_word_paren(cx, rest, "gs").is_some());
        closes += 1;
        rest = tail;
    }
    if pm.is_some() && row.3.is_none() {
        cx.error(
            op.span,
            format!("`{name}()` already counts in words; `pm()` cannot go inside it"),
        );
        return None;
    }
    if let Some(tail) = eat_word_paren(cx, rest, "-") {
        neg = !neg;
        closes += 1;
        rest = tail;
    }
    // The closing parens, the modifier's own last.
    let want = closes + 1;
    if rest.len() < want
        || !rest[rest.len() - want..]
            .iter()
            .all(|t| t.is_punct(Punct::RParen))
    {
        cx.error(op.span, format!("`{name}(` is not closed"));
        return None;
    }
    let inner = &rest[..rest.len() - want];
    let mut r = match (pm, neg) {
        (Some(_), false) => row.3.map_or(row.1, |(p, _)| p),
        (Some(_), true) => row.3.map_or(row.2, |(_, n)| n),
        (None, false) => row.1,
        (None, true) => row.2,
    };
    // `gs()` asks the linker for a stub, which only the two low bytes have a
    // relocation for; GNU as leaves the rest as the plain program-memory form.
    if pm == Some(true) {
        r = match r {
            reloc::R_AVR_LO8_LDI_PM => reloc::R_AVR_LO8_LDI_GS,
            reloc::R_AVR_HI8_LDI_PM => reloc::R_AVR_HI8_LDI_GS,
            other => other,
        };
    }
    Some(Modified {
        kind: reloc::ldi_part(r),
        inner,
    })
}

/// Matches a leading `word (`, or a leading `-` and `(` where `word` is `-`,
/// and returns what follows it.
fn eat_word_paren<'t>(cx: &AsmCtx<'_>, toks: &'t [Token], word: &str) -> Option<&'t [Token]> {
    let head = toks.first()?;
    let matched = if word == "-" {
        head.is_punct(Punct::Minus)
    } else {
        matches!(head.kind, TokKind::Ident(n) if cx.name(n) == word)
    };
    if !matched || !toks.get(1).is_some_and(|t| t.is_punct(Punct::LParen)) {
        return None;
    }
    Some(&toks[2..])
}
