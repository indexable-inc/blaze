//! Encoding and operand-parsing helpers shared by the 8-bit backends.

use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// One instruction being built: opcode bytes plus the fixups that fill in its
/// operand fields.
///
/// Operand fields always go through a fixup, even for a literal constant. The
/// core already knows how to fold, range-check and report one, so a backend
/// that resolved constants itself would only be duplicating (and eventually
/// contradicting) that.
#[derive(Default)]
pub struct Enc {
    pub bytes: Vec<u8>,
    pub fixups: Vec<Fixup>,
}

impl Enc {
    pub fn new() -> Enc {
        Enc::default()
    }

    /// An instruction that is nothing but opcode bytes.
    pub fn op(bytes: &[u8]) -> Enc {
        Enc {
            bytes: bytes.to_vec(),
            fixups: Vec::new(),
        }
    }

    pub fn byte(&mut self, b: u8) {
        self.bytes.push(b);
    }

    fn field(&mut self, size: u8, e: ExprRef, kind: FixupKind, span: Span) {
        self.fixups.push(Fixup {
            offset: self.bytes.len() as u32,
            expr: e,
            kind,
            span,
        });
        self.bytes.extend(std::iter::repeat_n(0u8, size as usize));
    }

    /// An 8-bit operand: an immediate, or a zero-page / port address.
    pub fn imm8(&mut self, e: ExprRef, span: Span) {
        self.field(1, e, FixupKind::data(1), span);
    }

    /// An 8-bit address with no negative spelling: an MCS-51 direct or bit
    /// address, where `-1` is refused rather than read as FFH.
    pub fn addr8(&mut self, e: ExprRef, span: Span) {
        self.field(1, e, FixupKind::data(1).with_limits(0, 0xff), span);
    }

    /// A signed 8-bit displacement, as in `(IX+d)`.
    pub fn disp8(&mut self, e: ExprRef, span: Span) {
        self.field(1, e, FixupKind::data(1).signed(), span);
    }

    /// A 16-bit little-endian address or immediate.
    pub fn imm16(&mut self, e: ExprRef, span: Span) {
        self.field(2, e, FixupKind::data(2), span);
    }

    /// A branch displacement measured from the *following* instruction, which
    /// is what `adjust = 1` says: the fixup byte is the last of the
    /// instruction, so the next one starts one byte past it.
    pub fn rel8(&mut self, e: ExprRef, span: Span) {
        self.field(1, e, FixupKind::pcrel(1, 1), span);
    }

    pub fn into_variant(self) -> Variant {
        Variant {
            bytes: self.bytes,
            fixups: self.fixups,
        }
    }

    /// The usual return of an encoder: exactly one candidate encoding.
    pub fn done(self) -> Option<Vec<Variant>> {
        Some(vec![self.into_variant()])
    }
}

/// Splits the operand tokens on top-level commas.
///
/// An empty slice means no operands at all, which is how an implied-mode
/// instruction arrives.
pub fn operands(toks: &[Token]) -> Vec<&[Token]> {
    Cursor::new(toks).split_commas()
}

/// The identifier an operand consists of, lowercased, if it is exactly one.
///
/// Register and condition names are recognised this way rather than by a
/// dedicated token, so `b` reaches the backend as an ordinary symbol name and
/// is only treated as a register where the instruction expects one.
pub fn sole_ident(cx: &AsmCtx<'_>, toks: &[Token]) -> Option<String> {
    match toks {
        [t] => t.ident().map(|n| cx.name(n).to_ascii_lowercase()),
        _ => None,
    }
}

/// Parses `toks` as a complete expression, reporting anything left over.
pub fn expr_of(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<ExprRef> {
    if toks.is_empty() {
        cx.error(span, "expected an operand");
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

/// Span covering a slice of operand tokens, falling back to the whole
/// instruction when the slice is empty.
pub fn span_of(toks: &[Token], fallback: Span) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => fallback,
    }
}

/// True when `toks` is a parenthesised group covering the whole slice, as in
/// `(hl)` or `(1234)` but not `(1)+(2)`.
pub fn parenthesised(toks: &[Token]) -> bool {
    let Some(first) = toks.first() else {
        return false;
    };
    if !first.is_punct(Punct::LParen) {
        return false;
    }
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LParen) => depth += 1,
            TokKind::Punct(Punct::RParen) => {
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

/// The contents of a parenthesised group, `(` and `)` stripped.
pub fn inside_parens(toks: &[Token]) -> &[Token] {
    match toks {
        [_, rest @ .., _] => rest,
        _ => &[],
    }
}

/// Reports "unknown instruction", the diagnostic every backend ends with.
pub fn unknown(
    cx: &mut AsmCtx<'_>,
    span: Span,
    arch: &str,
    mnemonic: &str,
) -> Option<Vec<Variant>> {
    cx.error(span, format!("unknown {arch} instruction `{mnemonic}`"));
    None
}

/// Reports operands that no form of `mnemonic` accepts.
pub fn bad_operands(cx: &mut AsmCtx<'_>, span: Span, mnemonic: &str) -> Option<Vec<Variant>> {
    cx.error(span, format!("invalid operands for `{mnemonic}`"));
    None
}
