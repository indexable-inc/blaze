//! Operand parsing.
//!
//! The grammar is the one `rl78-elf-as` accepts (`gas/config/rl78-parse.y`),
//! checked form by form against it. Addressing is written with sigils rather
//! than inferred:
//!
//! | Written | Meaning |
//! |---|---|
//! | `#e` | immediate |
//! | `!e` | 16-bit absolute address |
//! | `!!e` | 20-bit absolute address (`br`, `call`) |
//! | `$e` / `$!e` | 8- / 16-bit PC-relative target |
//! | `e` | short direct (`saddr`) or SFR address, chosen by value |
//! | `[de]` `[de+e]` `[hl]` `[hl+e]` `[hl+b]` `[hl+c]` `[sp]` `[sp+e]` `[bc]` | register indirect and based |
//! | `e[b]` `e[c]` `e[bc]` | based indexed |
//! | `es:` before `!e`, `[..]` or `e[..]` | the address is in the segment `ES` selects |
//! | `x.n` | bit `n` of `x`, in the bit-manipulation instructions only |
//!
//! A bare expression is deliberately not classified here. Whether `0xFFF10`
//! is a short direct address or an SFR depends on the instruction: the two
//! ranges overlap in `0xFFF00`–`0xFFF1F`, and the reference tries them in a
//! different order for different mnemonics.

use super::reg;
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// An expression and where it was written.
#[derive(Copy, Clone, Debug)]
pub struct Expr {
    pub e: ExprRef,
    pub span: Span,
}

/// The pointer register of a bracketed operand.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Ptr {
    De,
    Hl,
    Sp,
    Bc,
}

/// What is added to the pointer register.
#[derive(Copy, Clone, Debug)]
pub enum Offset {
    None,
    Disp(Expr),
    B,
    C,
}

/// The index register of `addr[b]`, `addr[c]` and `addr[bc]`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Index {
    B,
    C,
    Bc,
}

#[derive(Copy, Clone, Debug)]
pub enum Kind {
    Reg8(u8),
    Reg16(u8),
    Sp,
    /// The carry flag, as the bit instructions name it.
    Cy,
    /// A named SFR, as the low byte of its address.
    Sfr(u8),
    /// A register bank, `rb0`–`rb3`.
    Bank(u8),
    Imm(Expr),
    Abs16(Expr),
    Abs20(Expr),
    Rel(Expr),
    RelLong(Expr),
    /// A bare address: short direct or SFR, depending on the instruction.
    Direct(Expr),
    Ind {
        ptr: Ptr,
        off: Offset,
    },
    Based {
        base: Expr,
        index: Index,
    },
    /// `[e]` with no register inside, which only `callt` takes.
    Table(Expr),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: Kind,
    /// Written with an `es:` prefix.
    pub es: bool,
    /// The `.n` bit number, in a bit-manipulation instruction.
    pub bit: Option<u8>,
    pub span: Span,
}

/// Splits and parses every operand. `bits` says whether `.n` suffixes are
/// bit numbers, which is only true for the bit-manipulation mnemonics: the
/// reference makes the same distinction, so that labels may contain dots.
pub fn parse_all(
    cx: &mut AsmCtx<'_>,
    toks: &[Token],
    span: Span,
    bits: bool,
) -> Option<Vec<Operand>> {
    let pieces = Cursor::new(toks).split_commas();
    let mut out = Vec::with_capacity(pieces.len());
    let mut ok = true;
    for piece in pieces {
        match parse(cx, piece, span, bits) {
            Some(op) => out.push(op),
            None => ok = false,
        }
    }
    ok.then_some(out)
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

fn parse(cx: &mut AsmCtx<'_>, toks: &[Token], fallback: Span, bits: bool) -> Option<Operand> {
    let span = span_of(toks, fallback);
    if toks.is_empty() {
        cx.error(span, "expected an operand");
        return None;
    }

    // A branch target is never a bit, whatever its label is called.
    let bit_head;
    let es_tail;
    let mut toks = toks;
    let mut bit = None;
    if bits && !toks[0].is_punct(Punct::Dollar) {
        let (head, b) = split_bit(cx, toks, span)?;
        if b.is_some() {
            bit_head = head;
            toks = &bit_head;
            bit = b;
        }
    }

    let es = toks.len() >= 2
        && ident_lower(cx, &toks[0]).as_deref() == Some("es")
        && toks[1].is_punct(Punct::Colon);
    if es {
        es_tail = toks[2..].to_vec();
        toks = &es_tail;
        if toks.is_empty() {
            cx.error(span, "expected an address after `es:`");
            return None;
        }
    }

    let kind = parse_kind(cx, toks, span)?;
    // `ES:` only ever selects the segment of a memory address; everything the
    // reference accepts it on is one of these three shapes.
    if es
        && !matches!(
            kind,
            Kind::Abs16(_)
                | Kind::Based { .. }
                | Kind::Ind {
                    ptr: Ptr::De | Ptr::Hl | Ptr::Bc,
                    ..
                }
        )
    {
        cx.error(
            span,
            "an `es:` prefix is only allowed on `!addr`, `[de]`, `[hl]`, `[bc]` and `addr[b]` operands",
        );
        return None;
    }
    Some(Operand {
        kind,
        es,
        bit,
        span,
    })
}

/// Separates a trailing bit number from the operand it applies to.
///
/// The lexer has already made a mess of the dot, in three different ways:
/// `psw.7` and `label.3` are single identifiers, `0xfff20.3` and `[hl].3` end
/// in an identifier `.3`, and `a . 3` is a `.` token followed by a number.
fn split_bit(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<(Vec<Token>, Option<u8>)> {
    let n = toks.len();
    let last = toks[n - 1];
    let (head, number, num_span): (Vec<Token>, u64, Span) = match last.kind {
        TokKind::Int(v) if n >= 3 && toks[n - 2].is_punct(Punct::Dot) => {
            (toks[..n - 2].to_vec(), v, toks[n - 2].span.to(last.span))
        }
        TokKind::Ident(name) => {
            let text = cx.name(name).to_string();
            let Some(dot) = text.rfind('.') else {
                return Some((toks.to_vec(), None));
            };
            let digits = &text[dot + 1..];
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Some((toks.to_vec(), None));
            }
            let value = digits.parse::<u64>().unwrap_or(u64::MAX);
            let mut head = toks[..n - 1].to_vec();
            if dot > 0 {
                // Keep the part before the dot as an identifier of its own,
                // with a span covering just those characters.
                let lo = last.span.lo;
                let hi = (lo + dot as u32).min(last.span.hi);
                head.push(Token {
                    kind: TokKind::Ident(cx.interner.intern(&text[..dot])),
                    span: Span::new(lo, hi),
                    preceded_by_space: last.preceded_by_space,
                });
            }
            let num_span = Span::new((last.span.lo + dot as u32).min(last.span.hi), last.span.hi);
            (head, value, num_span)
        }
        _ => return Some((toks.to_vec(), None)),
    };
    if head.is_empty() {
        cx.error(span, "expected an address before the bit number");
        return None;
    }
    if number > 7 {
        cx.error(
            num_span,
            format!("bit number {number} is out of range (0 to 7)"),
        );
        return None;
    }
    Some((head, Some(number as u8)))
}

fn parse_kind(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<Kind> {
    let n = toks.len();
    let first = toks[0];

    if first.is_punct(Punct::Hash) {
        return Some(Kind::Imm(expr(cx, &toks[1..], span)?));
    }
    if first.is_punct(Punct::Bang) {
        if n >= 2 && toks[1].is_punct(Punct::Bang) {
            return Some(Kind::Abs20(expr(cx, &toks[2..], span)?));
        }
        return Some(Kind::Abs16(expr(cx, &toks[1..], span)?));
    }
    if first.is_punct(Punct::Dollar) {
        if n >= 2 && toks[1].is_punct(Punct::Bang) {
            return Some(Kind::RelLong(expr(cx, &toks[2..], span)?));
        }
        return Some(Kind::Rel(expr(cx, &toks[1..], span)?));
    }

    if first.is_punct(Punct::LBracket) && closes_at_end(toks) {
        return bracketed(cx, &toks[1..n - 1], span);
    }

    // `addr[b]`: the index register is the last thing written.
    if n >= 4 && toks[n - 1].is_punct(Punct::RBracket) && toks[n - 3].is_punct(Punct::LBracket) {
        let Some(name) = ident_lower(cx, &toks[n - 2]) else {
            cx.error(
                toks[n - 2].span,
                "expected `b`, `c` or `bc` as the index register",
            );
            return None;
        };
        let index = match (reg::reg8(&name), reg::reg16(&name)) {
            (Some(reg::B), _) => Index::B,
            (Some(reg::C), _) => Index::C,
            (_, Some(reg::BC)) => Index::Bc,
            _ => {
                cx.error(
                    toks[n - 2].span,
                    format!("`{name}` cannot index an address; only `b`, `c` and `bc` can"),
                );
                return None;
            }
        };
        let base = expr(cx, &toks[..n - 3], span)?;
        return Some(Kind::Based { base, index });
    }

    if let Some(name) = ident_lower(cx, &first) {
        let named = if let Some(r) = reg::reg8(&name) {
            Some(Kind::Reg8(r))
        } else if let Some(r) = reg::reg16(&name) {
            Some(Kind::Reg16(r))
        } else if let Some(r) = reg::sfr(&name) {
            Some(Kind::Sfr(r))
        } else if let Some(r) = reg::bank(&name) {
            Some(Kind::Bank(r))
        } else if name == "sp" {
            Some(Kind::Sp)
        } else if name == "cy" {
            Some(Kind::Cy)
        } else {
            None
        };
        if let Some(k) = named {
            if n == 1 {
                return Some(k);
            }
            cx.error(
                span,
                format!("`{name}` is a register and cannot be part of an address expression"),
            );
            return None;
        }
    }

    Some(Kind::Direct(expr(cx, toks, span)?))
}

/// True when the `[` at the start is closed by the last token, so `[hl]` is
/// bracketed but `[1]+[2]` would not be.
fn closes_at_end(toks: &[Token]) -> bool {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LBracket) => depth += 1,
            TokKind::Punct(Punct::RBracket) => {
                depth -= 1;
                if depth == 0 {
                    return i + 1 == toks.len();
                }
            }
            _ => {}
        }
    }
    false
}

fn bracketed(cx: &mut AsmCtx<'_>, inner: &[Token], span: Span) -> Option<Kind> {
    if inner.is_empty() {
        cx.error(span, "expected a register or an address inside `[]`");
        return None;
    }
    let name = ident_lower(cx, &inner[0]);
    let ptr = match name.as_deref().map(|s| (s, reg::reg16(s))) {
        Some((_, Some(reg::DE))) => Some(Ptr::De),
        Some((_, Some(reg::HL))) => Some(Ptr::Hl),
        Some((_, Some(reg::BC))) => Some(Ptr::Bc),
        Some(("sp", _)) => Some(Ptr::Sp),
        _ => None,
    };
    let Some(ptr) = ptr else {
        if let Some(name) = name.filter(|n| reg::is_reserved(n)) {
            cx.error(
                inner[0].span,
                format!("`{name}` cannot be used as a pointer; only `de`, `hl`, `bc` and `sp` can"),
            );
            return None;
        }
        return Some(Kind::Table(expr(cx, inner, span)?));
    };
    if inner.len() == 1 {
        return Some(Kind::Ind {
            ptr,
            off: Offset::None,
        });
    }
    if !inner[1].is_punct(Punct::Plus) {
        // `[hl-1]` included: the reference only adds displacements.
        cx.error(
            inner[1].span,
            "expected `+` or `]` after the pointer register",
        );
        return None;
    }
    let rest = &inner[2..];
    if let [t] = rest
        && let Some(r) = ident_lower(cx, t).as_deref().and_then(reg::reg8)
    {
        return match r {
            reg::B => Some(Kind::Ind {
                ptr,
                off: Offset::B,
            }),
            reg::C => Some(Kind::Ind {
                ptr,
                off: Offset::C,
            }),
            _ => {
                cx.error(
                    t.span,
                    "only `b` and `c` can be added to a pointer register",
                );
                None
            }
        };
    }
    let disp = expr(cx, rest, span)?;
    Some(Kind::Ind {
        ptr,
        off: Offset::Disp(disp),
    })
}

/// Parses `toks` as one complete expression.
fn expr(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<Expr> {
    if toks.is_empty() {
        cx.error(span, "expected an expression");
        return None;
    }
    // A register inside an expression is an error in the reference rather
    // than a symbol that happens to share its name.
    for t in toks {
        if let Some(name) = ident_lower(cx, t)
            && reg::is_reserved(&name)
        {
            cx.error(
                t.span,
                format!("`{name}` is a register and cannot be used as a value"),
            );
            return None;
        }
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
