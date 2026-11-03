//! Operand parsing.
//!
//! RISC-V operands are positional and their shape is fixed by the mnemonic, so
//! rather than classifying each one and matching afterwards the way x86 does,
//! the caller asks for what the instruction needs — `xreg(0)`, `mem(1)` — and
//! gets a diagnostic naming the expected shape when the source disagrees.
//!
//! The one piece of real grammar is `%hi(sym)`. It is a prefix, unlike the
//! `sym@modifier` form the generic expression parser already understands, so
//! it is recognised here.

use super::reg::{self, Reg, RegClass};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// A `%`-prefixed relocation modifier.
///
/// `%got_pcrel_hi` is the high half of the address of a symbol's GOT slot,
/// which `%pcrel_lo(label)` completes as it completes `%pcrel_hi`; it is what
/// `lga`, and `la` under `.option pic`, expand to.
///
/// The thread-local ones name the four access models the psABI has: local
/// exec (`%tprel_hi`, `%tprel_lo`, and the `%tprel_add` that marks the add of
/// the thread pointer), initial exec (`%tls_ie_pcrel_hi`), general dynamic
/// (`%tls_gd_pcrel_hi`) and the descriptor sequence (`%tlsdesc_hi` and the
/// three that complete it). The two `pcrel_hi` ones take the same
/// `%pcrel_lo(label)` as an ordinary PC-relative pair does.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Modifier {
    Hi,
    Lo,
    PcrelHi,
    PcrelLo,
    GotPcrelHi,
    TprelHi,
    TprelLo,
    TprelAdd,
    TlsIePcrelHi,
    TlsGdPcrelHi,
    TlsdescHi,
    TlsdescLoadLo,
    TlsdescAddLo,
    TlsdescCall,
}

impl Modifier {
    fn from_name(name: &str) -> Option<Modifier> {
        Some(match name {
            "hi" => Modifier::Hi,
            "lo" => Modifier::Lo,
            "pcrel_hi" => Modifier::PcrelHi,
            "pcrel_lo" => Modifier::PcrelLo,
            "got_pcrel_hi" => Modifier::GotPcrelHi,
            "tprel_hi" => Modifier::TprelHi,
            "tprel_lo" => Modifier::TprelLo,
            "tprel_add" => Modifier::TprelAdd,
            "tls_ie_pcrel_hi" => Modifier::TlsIePcrelHi,
            "tls_gd_pcrel_hi" => Modifier::TlsGdPcrelHi,
            "tlsdesc_hi" => Modifier::TlsdescHi,
            "tlsdesc_load_lo" => Modifier::TlsdescLoadLo,
            "tlsdesc_add_lo" => Modifier::TlsdescAddLo,
            "tlsdesc_call" => Modifier::TlsdescCall,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone)]
pub struct Imm {
    pub expr: ExprRef,
    pub modifier: Option<Modifier>,
    pub span: Span,
}

#[derive(Copy, Clone)]
pub struct Mem {
    pub base: Reg,
    pub off: Option<Imm>,
    #[allow(dead_code)]
    pub span: Span,
}

/// The comma-separated operand list of one instruction.
pub struct Operands<'t> {
    pieces: Vec<&'t [Token]>,
    /// Where to point an "expected N operands" diagnostic.
    pub span: Span,
}

fn span_of(piece: &[Token], fallback: Span) -> Span {
    match (piece.first(), piece.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => fallback,
    }
}

impl<'t> Operands<'t> {
    pub fn parse(cur: &Cursor<'t>, span: Span) -> Operands<'t> {
        let pieces = if cur.at_end() {
            Vec::new()
        } else {
            cur.split_commas()
        };
        Operands { pieces, span }
    }

    pub fn len(&self) -> usize {
        self.pieces.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    fn piece(&self, i: usize) -> Option<&'t [Token]> {
        self.pieces.get(i).copied()
    }

    pub fn piece_span(&self, i: usize) -> Span {
        self.piece(i).map_or(self.span, |p| span_of(p, self.span))
    }

    /// Checks the operand count, reporting the mismatch itself.
    pub fn arity(&self, cx: &mut AsmCtx<'_>, mnemonic: &str, want: &[usize]) -> Option<()> {
        if want.contains(&self.pieces.len()) {
            return Some(());
        }
        let list: Vec<String> = want.iter().map(|n| n.to_string()).collect();
        cx.error(
            self.span,
            format!(
                "`{mnemonic}` takes {} operand(s), but {} were given",
                list.join(" or "),
                self.pieces.len()
            ),
        );
        None
    }

    /// A bare identifier: a rounding mode, a `fence` set or a CSR name.
    pub fn word(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<String> {
        let p = self.piece(i)?;
        match p {
            [tok] => match tok.kind {
                TokKind::Ident(n) => Some(cx.name(n).to_ascii_lowercase()),
                _ => None,
            },
            _ => None,
        }
    }

    fn reg_in(&self, cx: &mut AsmCtx<'_>, i: usize, class: RegClass) -> Option<Reg> {
        let want = match class {
            RegClass::X => "an integer register",
            RegClass::F => "a floating-point register",
        };
        let Some(p) = self.piece(i) else {
            cx.error(self.span, format!("expected {want}"));
            return None;
        };
        let name = match p {
            [tok] => tok.ident().map(|n| cx.name(n).to_ascii_lowercase()),
            _ => None,
        };
        match name.as_deref().and_then(reg::lookup) {
            Some(r) if r.class == class => Some(r),
            _ => {
                cx.error(span_of(p, self.span), format!("expected {want}"));
                None
            }
        }
    }

    pub fn xreg(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        self.reg_in(cx, i, RegClass::X)
    }

    /// True when operand `i` is a register name, of either bank, reporting
    /// nothing when it is not.
    ///
    /// Several mnemonics mean one instruction or another depending on it:
    /// `and a0, a1, a2` is `and` and `and a0, a1, 4` is `andi`, `jalr a0, a1`
    /// links into `a0` and `jalr a0, 4` links into `ra`. GNU as picks between
    /// them by whether the operand parses as a register, and so does this --
    /// a register of the *wrong* bank is still a register, so `add a0, a1,
    /// fa2` stays the three-register form and is refused as one, rather than
    /// turning into `addi` against a symbol named `fa2`.
    pub fn is_reg(&self, cx: &mut AsmCtx<'_>, i: usize) -> bool {
        match self.piece(i) {
            Some([tok]) => tok
                .ident()
                .map(|n| cx.name(n).to_ascii_lowercase())
                .as_deref()
                .and_then(reg::lookup)
                .is_some(),
            _ => false,
        }
    }

    pub fn freg(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        self.reg_in(cx, i, RegClass::F)
    }

    /// An immediate, a branch target or a `%hi(...)`-style modified symbol.
    pub fn imm(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Imm> {
        let Some(p) = self.piece(i) else {
            cx.error(self.span, "expected an immediate");
            return None;
        };
        parse_imm(cx, p, self.span)
    }

    /// `off(base)`, `(base)`, or a bare `base` meaning offset zero.
    pub fn mem(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Mem> {
        let Some(p) = self.piece(i) else {
            cx.error(self.span, "expected a memory operand");
            return None;
        };
        let span = span_of(p, self.span);
        let Some((off, base)) = split_paren(p) else {
            cx.error(span, "expected an address of the form `offset(reg)`");
            return None;
        };
        let base = match base {
            [tok] => tok
                .ident()
                .map(|n| cx.name(n).to_ascii_lowercase())
                .as_deref()
                .and_then(reg::lookup),
            _ => None,
        };
        let Some(base) = base.filter(|r: &Reg| r.is_x()) else {
            cx.error(span, "the base of an address must be an integer register");
            return None;
        };
        let off = if off.is_empty() {
            None
        } else {
            Some(parse_imm(cx, off, span)?)
        };
        Some(Mem { base, off, span })
    }

    /// True when operand `i` ends in a parenthesised group, as `offset(reg)`
    /// does, even when what is inside is not a register.
    ///
    /// `lw a0, sym` loads from a symbol, so a load has to tell that from an
    /// address; one written `4(fa1)` is a mistaken address, and should be
    /// reported as one rather than read as an expression.
    pub fn ends_in_group(&self, i: usize) -> bool {
        self.piece(i).and_then(split_paren).is_some()
    }

    /// True when operand `i` looks like `something(reg)` rather than a plain
    /// expression, which is how `jalr rd, off(rs1)` is told from
    /// `jalr rd, rs1, off`.
    pub fn looks_like_mem(&self, cx: &mut AsmCtx<'_>, i: usize) -> bool {
        let Some(p) = self.piece(i) else {
            return false;
        };
        match split_paren(p) {
            Some((_, [tok])) => tok
                .ident()
                .map(|n| cx.name(n).to_ascii_lowercase())
                .as_deref()
                .and_then(reg::lookup)
                .is_some_and(|r| r.is_x()),
            _ => false,
        }
    }
}

/// Splits `prefix(inner)` at the parenthesis that closes the whole piece.
///
/// Scanning from the end is what makes `%lo(sym)(a1)` come apart the right
/// way: the trailing group is the base register, and everything before it is
/// the offset.
fn split_paren(p: &[Token]) -> Option<(&[Token], &[Token])> {
    let last = p.last()?;
    if !last.is_punct(Punct::RParen) {
        return None;
    }
    let mut depth = 0i32;
    for (i, t) in p.iter().enumerate().rev() {
        match t.kind {
            TokKind::Punct(Punct::RParen) => depth += 1,
            TokKind::Punct(Punct::LParen) => {
                depth -= 1;
                if depth == 0 {
                    return Some((&p[..i], &p[i + 1..p.len() - 1]));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_imm(cx: &mut AsmCtx<'_>, p: &[Token], fallback: Span) -> Option<Imm> {
    let span = span_of(p, fallback);

    // `%hi(sym)` and friends: a `%`, a name, and a parenthesised expression
    // that runs to the end of the operand.
    if p.first().is_some_and(|t| t.is_punct(Punct::Percent))
        && let Some(name) = p.get(1).and_then(Token::ident)
    {
        let text = cx.name(name).to_ascii_lowercase();
        let Some(modifier) = Modifier::from_name(&text) else {
            cx.error(span, format!("unknown relocation modifier `%{text}`"));
            return None;
        };
        let Some((before, inner)) = split_paren(&p[2..]) else {
            cx.error(span, format!("expected `%{text}(symbol)`"));
            return None;
        };
        if !before.is_empty() || inner.is_empty() {
            cx.error(span, format!("expected `%{text}(symbol)`"));
            return None;
        }
        let expr = expr_of(cx, inner)?;
        return Some(Imm {
            expr,
            modifier: Some(modifier),
            span,
        });
    }

    let expr = expr_of(cx, p)?;
    Some(Imm {
        expr,
        modifier: None,
        span,
    })
}

fn expr_of(cx: &mut AsmCtx<'_>, toks: &[Token]) -> Option<ExprRef> {
    let mut cur = Cursor::new(toks);
    let e = {
        let mut p = cx.expr_parser();
        p.parse(&mut cur)?
    };
    if !cur.at_end() {
        cx.error(cur.peek().span, "unexpected token after an expression");
        return None;
    }
    Some(e)
}
