//! CA78K0 operand syntax.
//!
//! The first character of an operand picks its addressing mode, as described
//! in the RA78K0 language manual (U17198EJ1V0UM00, section 2.2.5, Table 2-8,
//! page 38) and in U12326EJ4V0UM section 4.1.1, page 32:
//!
//! * `#` immediate data, `!` an absolute address, `$` a relative branch
//!   target, `[ ]` indirect addressing;
//! * with no sigil, a register name, or an address that is short direct
//!   (`saddr`) or special-function-register (`sfr`) addressing;
//! * `X.Y` anywhere in the operand is a bit term: the byte `X` and the bit
//!   position `Y` (section 2.5, pages 65–67).
//!
//! Register names are reserved words in either spelling: the function names
//! (`A`, `AX`, ...) or the absolute ones (`R1`, `RP0`, ...). Both are
//! case-insensitive, inside the brackets as well as outside them, so `[RP2]`
//! is `[DE]`, `[RP3+R3]` is `[HL+B]` and `[RP3].3` is `[HL].3`.

use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// Where a bit term's byte is.
#[derive(Copy, Clone, Debug)]
pub enum BitBase {
    A,
    Psw,
    Hl,
    /// A short direct or SFR address.
    Addr(ExprRef),
}

#[derive(Copy, Clone, Debug)]
pub enum Operand {
    /// An 8-bit register, by its `R` code.
    Reg8(u8),
    /// A register pair, by its `P` code.
    Reg16(u8),
    Sp,
    Psw,
    Cy,
    /// `RB0`..`RB3`.
    Bank(u8),
    /// `#expr`
    Imm(ExprRef),
    /// `!expr`
    Abs(ExprRef),
    /// `$expr`
    Rel(ExprRef),
    /// An expression with no sigil: an address, or the `1` of `ROR A,1`.
    Bare(ExprRef),
    De,
    Hl,
    HlByte(ExprRef),
    HlB,
    HlC,
    /// `[expr]`, which only `CALLT` takes.
    Ind(ExprRef),
    Bit(BitBase, u8),
}

#[derive(Copy, Clone, Debug)]
pub struct Arg {
    pub op: Operand,
    pub span: Span,
}

/// The `R` code of an 8-bit register name (U12326EJ4V0UM section 4.2.1,
/// page 38).
pub fn reg8(name: &str) -> Option<u8> {
    Some(match name.to_ascii_uppercase().as_str() {
        "X" | "R0" => 0,
        "A" | "R1" => 1,
        "C" | "R2" => 2,
        "B" | "R3" => 3,
        "E" | "R4" => 4,
        "D" | "R5" => 5,
        "L" | "R6" => 6,
        "H" | "R7" => 7,
        _ => return None,
    })
}

/// The `P` code of a register pair name (same page).
pub fn reg16(name: &str) -> Option<u8> {
    Some(match name.to_ascii_uppercase().as_str() {
        "AX" | "RP0" => 0,
        "BC" | "RP1" => 1,
        "DE" | "RP2" => 2,
        "HL" | "RP3" => 3,
        _ => return None,
    })
}

fn special(name: &str) -> Option<Operand> {
    if let Some(r) = reg8(name) {
        return Some(Operand::Reg8(r));
    }
    if let Some(p) = reg16(name) {
        return Some(Operand::Reg16(p));
    }
    Some(match name.to_ascii_uppercase().as_str() {
        "SP" => Operand::Sp,
        "PSW" => Operand::Psw,
        "CY" => Operand::Cy,
        "RB0" => Operand::Bank(0),
        "RB1" => Operand::Bank(1),
        "RB2" => Operand::Bank(2),
        "RB3" => Operand::Bank(3),
        _ => return None,
    })
}

fn ident_text<'a>(cx: &'a AsmCtx<'_>, t: &Token) -> Option<&'a str> {
    t.ident().map(|n| cx.name(n))
}

fn sole_ident<'a>(cx: &'a AsmCtx<'_>, toks: &[Token]) -> Option<&'a str> {
    match toks {
        [t] => ident_text(cx, t),
        _ => None,
    }
}

/// Span covering a slice of tokens, or `fallback` for an empty one.
pub fn span_of(toks: &[Token], fallback: Span) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => fallback,
    }
}

/// Parses `toks` as one complete expression.
fn expr_of(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<ExprRef> {
    if toks.is_empty() {
        cx.error(span, "expected an expression");
        return None;
    }
    let mut cur = Cursor::new(toks);
    let e = cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        cx.error(cur.remaining_span(), "unexpected tokens after the operand");
        return None;
    }
    Some(e)
}

/// Splits the operand list and parses each operand.
pub fn parse_all(cx: &mut AsmCtx<'_>, toks: &[Token], fallback: Span) -> Option<Vec<Arg>> {
    if toks.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in Cursor::new(toks).split_commas() {
        let span = span_of(part, fallback);
        if part.is_empty() {
            cx.error(span, "expected an operand");
            return None;
        }
        let op = parse_one(cx, part, span)?;
        out.push(Arg { op, span });
    }
    Some(out)
}

fn parse_one(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<Operand> {
    let first = toks[0];
    let rest = &toks[1..];
    let sigil = |p| first.is_punct(p);
    // The sigil is consumed before the expression parser sees the rest: to
    // it, `$` is the location counter, so `BR $LOOP` would otherwise read as
    // `here` followed by stray tokens, and `BR $$-1` as the section start.
    if sigil(Punct::Hash) {
        return Some(Operand::Imm(sigil_expr(cx, rest, first, "#")?));
    }
    if sigil(Punct::Bang) {
        return Some(Operand::Abs(sigil_expr(cx, rest, first, "!")?));
    }
    if sigil(Punct::Dollar) {
        return Some(Operand::Rel(sigil_expr(cx, rest, first, "$")?));
    }

    if let Some((left, right)) = split_bit(cx, toks)? {
        let base = bit_base(cx, &left, span)?;
        let bit = bit_number(cx, &right, span)?;
        return Some(Operand::Bit(base, bit));
    }

    if let Some(inner) = bracketed(toks) {
        return indirect(cx, inner, span);
    }

    if let Some(name) = sole_ident(cx, toks)
        && let Some(op) = special(name)
    {
        return Some(op);
    }
    Some(Operand::Bare(expr_of(cx, toks, span)?))
}

fn sigil_expr(cx: &mut AsmCtx<'_>, rest: &[Token], sigil: Token, what: &str) -> Option<ExprRef> {
    if rest.is_empty() {
        cx.error(sigil.span, format!("expected an expression after `{what}`"));
        return None;
    }
    let span = span_of(rest, sigil.span);
    if let Some(name) = sole_ident(cx, rest)
        && special(name).is_some()
    {
        let name = name.to_string();
        cx.error(
            span,
            format!("`{name}` is a register name and cannot follow `{what}`"),
        );
        return None;
    }
    expr_of(cx, rest, span)
}

/// The contents of `[ ... ]` when the brackets enclose the whole operand.
fn bracketed(toks: &[Token]) -> Option<&[Token]> {
    let (first, last) = (toks.first()?, toks.last()?);
    if toks.len() < 2 || !first.is_punct(Punct::LBracket) || !last.is_punct(Punct::RBracket) {
        return None;
    }
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LBracket) => depth += 1,
            TokKind::Punct(Punct::RBracket) => {
                depth -= 1;
                if depth == 0 && i + 1 != toks.len() {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(&toks[1..toks.len() - 1])
}

const INDIRECT_FORMS: &str = "the 78K0 has [DE], [HL], [HL+byte], [HL+B] and [HL+C]";

fn indirect(cx: &mut AsmCtx<'_>, inner: &[Token], span: Span) -> Option<Operand> {
    if inner.is_empty() {
        cx.error(span, "expected something inside `[ ]`");
        return None;
    }
    if let Some(name) = sole_ident(cx, inner) {
        match name.to_ascii_uppercase().as_str() {
            "DE" | "RP2" => return Some(Operand::De),
            "HL" | "RP3" => return Some(Operand::Hl),
            _ => {}
        }
    }
    let head = ident_text(cx, &inner[0]).map(str::to_ascii_uppercase);
    if matches!(head.as_deref(), Some("HL" | "RP3"))
        && inner.len() >= 2
        && inner[1].is_punct(Punct::Plus)
    {
        let disp = &inner[2..];
        match sole_ident(cx, disp).map(str::to_ascii_uppercase).as_deref() {
            Some("B" | "R3") => return Some(Operand::HlB),
            Some("C" | "R2") => return Some(Operand::HlC),
            _ => {}
        }
        if disp.is_empty() {
            cx.error(span, "expected a displacement after `HL+`");
            return None;
        }
        let dspan = span_of(disp, span);
        return Some(Operand::HlByte(expr_of(cx, disp, dspan)?));
    }
    // Anything else that starts with a register is an indirect form the CPU
    // does not have, such as `[DE+1]` or `[HL-1]`; reading it as an address
    // would silently treat the register name as a symbol.
    if let Some(name) = head
        && special(&name).is_some()
    {
        cx.error(
            span,
            format!("unsupported indirect operand: {INDIRECT_FORMS}"),
        );
        return None;
    }
    Some(Operand::Ind(expr_of(cx, inner, span)?))
}

/// Finds a bit position specifier and splits the operand around it.
///
/// The lexer does not give `.` a token of its own here: it is a legal
/// identifier character, so `P0.3` arrives as one identifier and `0FE20H.7` as
/// a number followed by the identifier `.7`. The split therefore happens
/// inside identifiers as well as at a lone `.`. The manual says the specifier
/// ignores operator precedence — everything left of it is the byte, everything
/// right of it the bit — which is exactly what splitting the token list does:
/// `1 + 0FE30H.3` is `0FE31H` bit 3, and `0FE40H.4 + 2` is `0FE40H` bit 6.
///
/// Returns `Some(None)` when the operand has no specifier, and `None` after
/// reporting a malformed one.
fn split_bit(cx: &mut AsmCtx<'_>, toks: &[Token]) -> Option<Option<(Vec<Token>, Vec<Token>)>> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LBracket | Punct::LParen) => depth += 1,
            TokKind::Punct(Punct::RBracket | Punct::RParen) => depth -= 1,
            _ => {}
        }
        if depth != 0 {
            continue;
        }
        if t.is_punct(Punct::Dot) {
            return Some(Some((toks[..i].to_vec(), toks[i + 1..].to_vec())));
        }
        let Some(text) = ident_text(cx, t) else {
            continue;
        };
        let Some(dot) = text.find('.') else {
            continue;
        };
        let text = text.to_string();
        let (before, after) = (&text[..dot], &text[dot + 1..]);
        if after.contains('.') {
            cx.error(t.span, "a bit term has exactly one `.`");
            return None;
        }
        let lo = t.span.lo;
        let mut left = toks[..i].to_vec();
        if !before.is_empty() {
            let span = Span::new(lo, lo + dot as u32);
            left.push(piece(cx, before, span, t.preceded_by_space)?);
        }
        let mut right = Vec::new();
        if !after.is_empty() {
            let span = Span::new(lo + dot as u32 + 1, t.span.hi);
            right.push(piece(cx, after, span, false)?);
        }
        right.extend_from_slice(&toks[i + 1..]);
        return Some(Some((left, right)));
    }
    Some(None)
}

/// Re-tokenises one side of an identifier that was split at its `.`.
fn piece(cx: &mut AsmCtx<'_>, text: &str, span: Span, spaced: bool) -> Option<Token> {
    let kind = if text.starts_with(|c: char| c.is_ascii_digit()) {
        match number(text) {
            Some(v) => TokKind::Int(v),
            None => {
                cx.error(span, format!("invalid number `{text}`"));
                return None;
            }
        }
    } else {
        TokKind::Ident(cx.interner.intern(text))
    };
    Some(Token {
        kind,
        span,
        preceded_by_space: spaced,
    })
}

/// A CA78K0 numeric constant (RA78K0 manual, Table 2-7, page 37): digits
/// with an optional `H`, `B`/`Y`, `O`/`Q` or `D`/`T` suffix.
fn number(text: &str) -> Option<u64> {
    let upper = text.to_ascii_uppercase();
    let (body, radix) = match upper.as_bytes().last()? {
        b'H' => (&upper[..upper.len() - 1], 16),
        b'B' | b'Y' => (&upper[..upper.len() - 1], 2),
        b'O' | b'Q' => (&upper[..upper.len() - 1], 8),
        b'D' | b'T' => (&upper[..upper.len() - 1], 10),
        _ => (upper.as_str(), 10),
    };
    if body.is_empty() {
        return None;
    }
    u64::from_str_radix(body, radix).ok()
}

fn bit_base(cx: &mut AsmCtx<'_>, left: &[Token], span: Span) -> Option<BitBase> {
    if left.is_empty() {
        cx.error(span, "expected a byte before the bit position `.`");
        return None;
    }
    if let Some(name) = sole_ident(cx, left) {
        match name.to_ascii_uppercase().as_str() {
            "A" => return Some(BitBase::A),
            "PSW" => return Some(BitBase::Psw),
            _ => {}
        }
        if special(name).is_some() {
            let name = name.to_string();
            cx.error(
                span,
                format!(
                    "`{name}` has no addressable bits: bit terms take A, PSW, [HL], \
                     or a short direct or SFR address"
                ),
            );
            return None;
        }
    }
    if let Some(inner) = bracketed(left) {
        if sole_ident(cx, inner)
            .is_some_and(|n| n.eq_ignore_ascii_case("HL") || n.eq_ignore_ascii_case("RP3"))
        {
            return Some(BitBase::Hl);
        }
        cx.error(
            span,
            "`[HL]` is the only indirect byte with addressable bits",
        );
        return None;
    }
    let lspan = span_of(left, span);
    Some(BitBase::Addr(expr_of(cx, left, lspan)?))
}

fn bit_number(cx: &mut AsmCtx<'_>, right: &[Token], span: Span) -> Option<u8> {
    if right.is_empty() {
        cx.error(span, "expected a bit position after `.`");
        return None;
    }
    let rspan = span_of(right, span);
    let e = expr_of(cx, right, rspan)?;
    // The manual allows only an absolute number here (section 2.5, page 66),
    // so the bit is known now and can go straight into the opcode.
    match cx.constant(e) {
        Some(v @ 0..=7) => Some(v as u8),
        Some(v) => {
            cx.error(rspan, format!("bit position {v} is out of range (0 to 7)"));
            None
        }
        None => {
            cx.error(rspan, "a bit position must be an absolute number (0 to 7)");
            None
        }
    }
}
