//! Operand parsing, for both GNU and Motorola syntax.
//!
//! One grammar serves both, because GNU as accepts the Motorola spellings of
//! every addressing mode (`8(%a0,%d1.w)`, `-(%sp)`) alongside its own MIT
//! ones (`%a0@(8,%d1:w)`, `%sp@-`). What really separates the syntaxes is how
//! a register is spelled: GNU requires `%`, so a bare `d0` there is a symbol.
//!
//! The parser records what was written — a base, an index, a displacement and
//! any explicit `.w`/`.l` — and leaves every size decision to
//! [`super::encode`], which knows the CPU and whether a value is constant.
//!
//! Two things GNU as reads as separate operands arrive here attached to one:
//! a `{...}` after an operand (a bit field's `{offset:width}`, or `fmove.p`'s
//! k-factor `{#3}`), and the far side of a colon (`d1:d2`, `fp1:fp2`,
//! `(a0):(a1)`). `Operand::brace` and [`Mode::Pair`]/[`Mode::Colon`] keep
//! them, and `generic` spreads them back out.

use super::float;
use super::reg::{self, Reg};
use super::table::rid;
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// An explicit `.b`, `.w` or `.l` on a displacement or an absolute address.
///
/// [`Width::B`] is a request rather than a promise: the only field a byte fits
/// in is the 8-bit displacement of a brief extension word, so GNU as reads it
/// as a word wherever it cannot reach for one, and `:s` is another spelling of
/// it. See [`super::encode::no_brief_word`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Width {
    B,
    W,
    L,
}

/// An expression with the width it was pinned to, if any.
#[derive(Copy, Clone, Debug)]
pub struct Value {
    pub e: ExprRef,
    pub width: Option<Width>,
    pub span: Span,
}

/// An index register: `d1.w`, `%d1:l:4`, `a2.l*8`.
#[derive(Copy, Clone, Debug)]
pub struct Index {
    /// 0-7 for `d0`-`d7`, 8-15 for `a0`-`a7`.
    pub reg: u8,
    pub long: bool,
    /// 1, 2, 4 or 8.
    pub scale: u8,
    /// Whether the size was written rather than taken by default.
    pub(crate) sized: bool,
    pub span: Span,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Base {
    A(u8),
    Pc,
    /// No base register at all: the 68020's `(d,Xn)`.
    None,
}

/// Where a 68020 memory-indirect index is applied.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IndexAt {
    /// Inside the brackets: added before the memory fetch.
    Pre,
    /// After them: added to the fetched address.
    Post,
}

#[derive(Clone, Debug)]
pub enum Mode {
    DReg(u8),
    AReg(u8),
    /// `fp0`-`fp7`.
    FReg(u8),
    Ind(u8),
    PostInc(u8),
    PreDec(u8),
    /// `d(An)`, `d(An,Xn)`, `d(PC)`, `d(PC,Xn)` and the 68020 forms with a
    /// wide displacement or no base register.
    Indexed {
        base: Base,
        disp: Option<Value>,
        index: Option<Index>,
    },
    /// 68020 memory indirect: `([bd,An,Xn],od)` or `([bd,An],Xn,od)`.
    MemInd {
        base: Base,
        bd: Option<Value>,
        index: Option<(Index, IndexAt)>,
        od: Option<Value>,
    },
    Abs(Value),
    Imm(ExprRef, Span),
    /// A floating-point immediate, `#1.5` or `#0r1.5`; see `float`.
    FImm(f64, Span),
    Sr,
    Ccr,
    Usp,
    /// Any other register that is not a general one: an FPU control register,
    /// an MMU register, a cache name or a `movec` register, by its number in
    /// `rid`.
    Ctl(u16),
    /// A register list, GNU as's way: bit 0 = `d0` through bit 15 = `a7`,
    /// bits 16-23 `fp0`-`fp7`, and 24-26 `fpiar`, `fpsr` and `fpcr`.
    RegList(u32),
    /// `d2:d1`, the register pair of a 64-bit multiply or divide.
    Pair(u8, u8),
    /// Any other two operands joined by a colon: `fp1:fp2` for `fsincos`,
    /// `(a0):(a1)` for `cas2`.
    Colon(Box<Operand>, Box<Operand>),
}

#[derive(Clone, Debug)]
pub struct Operand {
    pub mode: Mode,
    pub span: Span,
    /// What a trailing `{...}` held: two operands for `{offset:width}`, one
    /// for a k-factor, none without braces.
    pub(crate) brace: Vec<Operand>,
}

impl Operand {
    pub fn describe(&self) -> &'static str {
        match &self.mode {
            Mode::DReg(_) => "a data register",
            Mode::AReg(_) => "an address register",
            Mode::Ind(_) => "`(An)`",
            Mode::PostInc(_) => "`(An)+`",
            Mode::PreDec(_) => "`-(An)`",
            Mode::Indexed {
                base: Base::Pc,
                index: Some(_),
                ..
            } => "a PC-relative indexed operand",
            Mode::Indexed { base: Base::Pc, .. } => "a PC-relative operand",
            Mode::Indexed { index: Some(_), .. } => "an indexed operand",
            Mode::Indexed { .. } => "a displacement operand",
            Mode::MemInd { .. } => "a memory-indirect operand",
            Mode::Abs(_) => "an absolute address",
            Mode::Imm(..) => "an immediate",
            Mode::FImm(..) => "a floating-point immediate",
            Mode::FReg(_) => "a floating-point register",
            Mode::Sr => "`sr`",
            Mode::Ccr => "`ccr`",
            Mode::Usp => "`usp`",
            Mode::RegList(_) => "a register list",
            Mode::Pair(..) => "a register pair",
            Mode::Colon(..) => "a pair of operands",
            Mode::Ctl(_) => "a control register",
        }
    }
}

/// Parses the comma-separated operands of one instruction.
pub(crate) fn parse_list(cx: &mut AsmCtx<'_>, cur: &Cursor<'_>) -> Option<Vec<Operand>> {
    if cur.at_end() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for piece in cur.split_commas() {
        if piece.is_empty() {
            cx.error(cur.remaining_span(), "empty operand");
            return None;
        }
        let mut p = Parser {
            cx: &mut *cx,
            gnu: false,
        };
        p.gnu = p.cx.dialect == crate::lexer::Dialect::Gas;
        out.push(p.operand(piece)?);
    }
    Some(out)
}

fn span_of(toks: &[Token]) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => Span::DUMMY,
    }
}

/// The index of the `)` or `]` closing the bracket at `open`.
fn matching(toks: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
            TokKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits on commas outside any bracket.
fn split_top(toks: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
            TokKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => depth -= 1,
            TokKind::Punct(Punct::Comma) if depth == 0 => {
                out.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&toks[start..]);
    out
}

/// A register as written, with whatever size and scale followed it.
struct RegTok {
    reg: Reg,
    /// `.w`/`.l` or `:w`/`:l`.
    long: Option<bool>,
    scale: Option<u8>,
    span: Span,
}

struct Parser<'a, 'b> {
    cx: &'a mut AsmCtx<'b>,
    gnu: bool,
}

impl Parser<'_, '_> {
    fn text(&self, t: &Token) -> Option<String> {
        match t.kind {
            TokKind::Ident(n) => Some(self.cx.name(n).to_ascii_lowercase()),
            _ => None,
        }
    }

    /// Reads a register at `toks[i..]`, returning it and how many tokens it
    /// took. Takes a `.w`/`.l` glued onto the name (`d1.w` lexes as one
    /// identifier) but not a `:w` or scale; those are the index parser's.
    fn reg_at(&self, toks: &[Token], i: usize) -> Option<(RegTok, usize)> {
        let mut j = i;
        let sigil = toks.get(j).is_some_and(|t| t.is_punct(Punct::Percent));
        if sigil {
            j += 1;
        } else if self.gnu {
            return None;
        }
        let t = toks.get(j)?;
        if sigil && t.preceded_by_space {
            return None;
        }
        let text = self.text(t)?;
        let (stem, long) = match text.split_once('.') {
            Some((stem, "w")) => (stem, Some(false)),
            Some((stem, "l")) => (stem, Some(true)),
            Some(_) => return None,
            None => (text.as_str(), None),
        };
        let reg = reg::lookup(stem, self.gnu)?;
        let span = toks[i].span.to(t.span);
        Some((
            RegTok {
                reg,
                long,
                scale: None,
                span,
            },
            j + 1 - i,
        ))
    }

    /// A register followed by the index decorations of either syntax:
    /// `d1.w*4`, `%d1:w:4`, `%d1:l`, `d1*2`. The whole slice must be used.
    fn index_item(&mut self, toks: &[Token]) -> Option<Option<RegTok>> {
        let Some((mut r, mut i)) = self.reg_at(toks, 0) else {
            return Some(None);
        };
        if toks.get(i).is_some_and(|t| t.is_punct(Punct::Colon)) {
            // `:w`, `:l`, and then perhaps `:4` — or directly `:4`.
            match toks.get(i + 1).map(|t| (t.kind, self.text(t))) {
                Some((_, Some(s))) if s == "w" || s == "l" => {
                    if r.long.is_some() {
                        self.cx.error(toks[i].span, "the index size is given twice");
                        return None;
                    }
                    r.long = Some(s == "l");
                    i += 2;
                    if toks.get(i).is_some_and(|t| t.is_punct(Punct::Colon)) {
                        i += 1;
                        r.scale = Some(self.scale_at(toks, i)?);
                        i += 1;
                    }
                }
                Some((TokKind::Int(_), _)) => {
                    i += 1;
                    r.scale = Some(self.scale_at(toks, i)?);
                    i += 1;
                }
                _ => {
                    self.cx
                        .error(toks[i].span, "expected `w`, `l` or a scale after `:`");
                    return None;
                }
            }
        } else if toks.get(i).is_some_and(|t| t.is_punct(Punct::Star)) {
            i += 1;
            r.scale = Some(self.scale_at(toks, i)?);
            i += 1;
        }
        if i != toks.len() {
            // Not a register item after all, such as `d0-d3` in a list; let
            // the caller decide.
            return Some(None);
        }
        r.span = span_of(toks);
        Some(Some(r))
    }

    fn scale_at(&mut self, toks: &[Token], i: usize) -> Option<u8> {
        match toks.get(i).map(|t| t.kind) {
            Some(TokKind::Int(n @ (1 | 2 | 4 | 8))) => Some(n as u8),
            Some(TokKind::Int(_)) => {
                self.cx
                    .error(toks[i].span, "an index scale must be 1, 2, 4 or 8");
                None
            }
            _ => {
                let span = toks.get(i).map_or(span_of(toks), |t| t.span);
                self.cx
                    .error(span, "expected an index scale of 1, 2, 4 or 8");
                None
            }
        }
    }

    /// Parses an expression that must use every token in `toks`, with an
    /// optional `.w`/`.l` (or GNU `:w`/`:l`) width at the end.
    fn value(&mut self, toks: &[Token]) -> Option<Value> {
        let span = span_of(toks);
        if toks.is_empty() {
            self.cx.error(span, "expected an expression");
            return None;
        }
        let (toks, width) = self.strip_width(toks);
        self.suffix_at_end(&toks)?;
        let mut cur = Cursor::new(&toks);
        let e = self.cx.expr_parser().parse(&mut cur)?;
        if !cur.is_empty() {
            self.cx
                .error(cur.peek().span, "unexpected token after expression");
            return None;
        }
        Some(Value { e, width, span })
    }

    /// Checks that a relocation suffix is the last thing the operand writes.
    ///
    /// GNU as takes the suffix off the end of an operand's text before it
    /// parses what is left, so `8+x@TLSGD` is the model applied to `x+8`
    /// while `x@TLSGD+8` is a syntax error. A data directive is the other way
    /// round: `m68k_elf_cons` reads the suffix after the expression and folds
    /// a constant behind it into the addend, so `.long x@TLSLDO+8` works.
    fn suffix_at_end(&mut self, toks: &[Token]) -> Option<()> {
        let n = toks.len();
        let ats = toks.iter().filter(|t| t.is_punct(Punct::At)).count();
        if ats == 0
            || (ats == 1
                && n >= 3
                && toks[n - 2].is_punct(Punct::At)
                && matches!(toks[n - 1].kind, TokKind::Ident(_)))
        {
            return Some(());
        }
        self.cx.error(
            span_of(toks),
            "a relocation suffix goes at the end of an operand, as in `x@TLSGD(%a0)`",
        );
        None
    }

    /// Removes a trailing width: a separate `.w` token (after a number or a
    /// parenthesis), a `.w` glued to a symbol name, or GNU's `:w`.
    ///
    /// `s`, for "short", is the other spelling of `b` in both references, and
    /// means the same thing in a displacement as it does in a branch.
    fn strip_width(&mut self, toks: &[Token]) -> (Vec<Token>, Option<Width>) {
        let n = toks.len();
        let width_of = |s: &str| match s {
            "b" | "s" => Some(Width::B),
            "w" => Some(Width::W),
            "l" => Some(Width::L),
            _ => None,
        };
        if n >= 3
            && toks[n - 2].is_punct(Punct::Colon)
            && let Some(w) = self.text(&toks[n - 1]).as_deref().and_then(width_of)
        {
            return (toks[..n - 2].to_vec(), Some(w));
        }
        if let Some(last) = toks.last()
            && let TokKind::Ident(name) = last.kind
        {
            let text = self.cx.name(name).to_string();
            if let Some(dot) = text.rfind('.') {
                let w = width_of(&text[dot + 1..].to_ascii_lowercase());
                if let Some(w) = w {
                    let stem = &text[..dot];
                    let mut v = toks[..n - 1].to_vec();
                    if !stem.is_empty() {
                        let name = self.cx.interner.intern(stem);
                        v.push(Token {
                            kind: TokKind::Ident(name),
                            ..*last
                        });
                    }
                    if !v.is_empty() {
                        return (v, Some(w));
                    }
                }
            }
        }
        (toks.to_vec(), None)
    }

    fn operand(&mut self, toks: &[Token]) -> Option<Operand> {
        let span = span_of(toks);

        // A trailing `{...}`: a bit field's `{offset:width}`, or a k-factor.
        let (toks, brace) = match toks.last() {
            Some(t) if t.is_punct(Punct::RBrace) => {
                let Some(open) = toks.iter().rposition(|t| t.is_punct(Punct::LBrace)) else {
                    self.cx.error(t.span, "`}` without a matching `{`");
                    return None;
                };
                let parts = self.brace(&toks[open + 1..toks.len() - 1], toks[open].span)?;
                (&toks[..open], parts)
            }
            _ => (toks, Vec::new()),
        };
        if toks.is_empty() {
            self.cx.error(span, "expected an operand before `{`");
            return None;
        }
        // GNU as ends an operand at a colon followed by what can start one: a
        // register, `#`, `(`, `@` or a digit. That splits `d1:d2`, `fp1:fp2`
        // and `(a0):(a1)`, but not `label:w`.
        if let Some(colon) = self.operand_colon(toks) {
            let left = self.operand(&toks[..colon])?;
            let right = self.operand(&toks[colon + 1..])?;
            let mode = match (&left.mode, &right.mode) {
                (Mode::DReg(h), Mode::DReg(l)) => Mode::Pair(*h, *l),
                _ => Mode::Colon(Box::new(left), Box::new(right)),
            };
            return Some(Operand { mode, span, brace });
        }
        let mode = self.mode(toks)?;
        Some(Operand { mode, span, brace })
    }

    /// The top-level colon that separates two operands, if there is one.
    fn operand_colon(&self, toks: &[Token]) -> Option<usize> {
        // A register with an index size or scale (`d1:w`, `%d1:l:4`) keeps
        // its colons.
        if self.looks_like_register_item(toks) {
            return None;
        }
        let mut depth = 0i32;
        for (i, t) in toks.iter().enumerate() {
            match t.kind {
                TokKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                TokKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => depth -= 1,
                TokKind::Punct(Punct::Colon) if depth == 0 && i > 0 => {
                    let starts = match toks.get(i + 1).map(|t| t.kind) {
                        Some(TokKind::Punct(
                            Punct::Hash | Punct::Amp | Punct::LParen | Punct::At | Punct::Percent,
                        )) => true,
                        Some(TokKind::Int(_) | TokKind::LocalRef(..) | TokKind::BadNumber(_)) => {
                            true
                        }
                        Some(TokKind::Ident(n)) => {
                            let c = self.cx.name(n).as_bytes()[0] | 0x20;
                            matches!(c, b'a' | b'd' | b'f')
                        }
                        _ => false,
                    };
                    if starts {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The operands of a `{...}`: `{offset:width}` or `{k}`. Each is a data
    /// register, `#expr` or a bare expression, all of which GNU as takes.
    fn brace(&mut self, toks: &[Token], open: Span) -> Option<Vec<Operand>> {
        let parts: Vec<&[Token]> = match toks.iter().position(|t| t.is_punct(Punct::Colon)) {
            Some(colon) => vec![&toks[..colon], &toks[colon + 1..]],
            None => vec![toks],
        };
        let mut out = Vec::new();
        for part in parts {
            if part.is_empty() {
                self.cx
                    .error(open, "a bit field is written `{offset:width}`");
                return None;
            }
            let span = span_of(part);
            let mode = if let Some((r, n)) = self.reg_at(part, 0)
                && n == part.len()
            {
                match r.reg {
                    Reg::D(d) if r.long.is_none() => Mode::DReg(d),
                    _ => {
                        self.cx
                            .error(r.span, "a register inside `{...}` must be `d0`-`d7`");
                        return None;
                    }
                }
            } else if part[0].is_punct(Punct::Hash) {
                self.mode(part)?
            } else {
                Mode::Abs(self.value(part)?)
            };
            out.push(Operand {
                mode,
                span,
                brace: Vec::new(),
            });
        }
        Some(out)
    }

    fn mode(&mut self, toks: &[Token]) -> Option<Mode> {
        let span = span_of(toks);

        // `#expr`, or a floating-point `#1.5`.
        if toks[0].is_punct(Punct::Hash) {
            if toks.len() == 1 {
                self.cx.error(span, "expected an expression after `#`");
                return None;
            }
            let rest = &toks[1..];
            let text = self.cx.sources.span_text(span_of(rest));
            if let Some(f) = float::parse(text, self.cx.dialect) {
                return Some(Mode::FImm(f, span));
            }
            self.suffix_at_end(rest)?;
            let mut cur = Cursor::new(rest);
            let e = self.cx.expr_parser().parse(&mut cur)?;
            if !cur.is_empty() {
                self.cx
                    .error(cur.peek().span, "unexpected token after immediate");
                return None;
            }
            return Some(Mode::Imm(e, span));
        }

        if let Some((r, n)) = self.reg_at(toks, 0) {
            if n == toks.len() {
                if r.long.is_some() {
                    self.cx.error(
                        r.span,
                        "a `.w`/`.l` size belongs on an index register, not an operand",
                    );
                    return None;
                }
                return match r.reg {
                    Reg::D(d) => Some(Mode::DReg(d)),
                    Reg::A(a) => Some(Mode::AReg(a)),
                    Reg::Fp(f) => Some(Mode::FReg(f)),
                    Reg::Ctl(rid::SR) => Some(Mode::Sr),
                    Reg::Ctl(rid::CCR) => Some(Mode::Ccr),
                    Reg::Ctl(rid::USP) => Some(Mode::Usp),
                    Reg::Ctl(id) => Some(Mode::Ctl(id)),
                    Reg::Pc => {
                        self.cx
                            .error(span, "`pc` can only be a base register, as in `label(pc)`");
                        None
                    }
                };
            }
            let next = toks[n];
            if next.is_punct(Punct::At) && self.gnu {
                return self.mit(r, &toks[n + 1..], span);
            }
            if next.is_punct(Punct::Slash) || next.is_punct(Punct::Minus) {
                return self.reglist(toks);
            }
        }

        // `-(An)`
        if toks[0].is_punct(Punct::Minus)
            && toks.len() >= 3
            && toks[1].is_punct(Punct::LParen)
            && matching(toks, 1) == Some(toks.len() - 1)
            && let Some((r, n)) = self.reg_at(toks, 2)
            && n + 3 == toks.len()
        {
            return match r.reg {
                Reg::A(a) if r.long.is_none() => Some(Mode::PreDec(a)),
                _ => {
                    self.cx.error(
                        span,
                        "predecrement needs an address register, as in `-(a0)`",
                    );
                    None
                }
            };
        }

        // Something in parentheses first: `(a0)`, `(a0)+`, `(4,a0,d1.w)`,
        // `([4,a0],8)` — or just a parenthesised expression, `(ABS).w`.
        if toks[0].is_punct(Punct::LParen) {
            let Some(close) = matching(toks, 0) else {
                self.cx.error(toks[0].span, "unclosed `(`");
                return None;
            };
            let inner = &toks[1..close];
            let rest = &toks[close + 1..];
            if self.is_memory_group(inner) {
                if rest.len() == 1 && rest[0].is_punct(Punct::Plus) {
                    return match self.reg_at(inner, 0) {
                        Some((
                            RegTok {
                                reg: Reg::A(a),
                                long: None,
                                ..
                            },
                            n,
                        )) if n == inner.len() => Some(Mode::PostInc(a)),
                        _ => {
                            self.cx.error(
                                span,
                                "postincrement needs an address register, as in `(a0)+`",
                            );
                            None
                        }
                    };
                }
                if !rest.is_empty() {
                    self.cx
                        .error(rest[0].span, "unexpected token after memory operand");
                    return None;
                }
                return self.group(inner, None, span);
            }
        }

        // An expression, then perhaps `(base...)`.
        let (e_end, paren) = self.split_disp(toks);
        if let Some(open) = paren {
            let close = matching(toks, open).filter(|&c| c == toks.len() - 1);
            let Some(close) = close else {
                self.cx
                    .error(toks[open].span, "unexpected tokens after `(`...`)`");
                return None;
            };
            let disp = self.value(&toks[..e_end])?;
            return self.group(&toks[open + 1..close], Some(disp), span);
        }
        let v = self.value(toks)?;
        Some(Mode::Abs(v))
    }

    /// Whether the inside of a leading `(...)` is an addressing mode rather
    /// than a parenthesised expression: it names a register or has a comma.
    fn is_memory_group(&self, inner: &[Token]) -> bool {
        if inner.is_empty() {
            return false;
        }
        if split_top(inner).len() > 1 || inner[0].is_punct(Punct::LBracket) {
            return true;
        }
        self.looks_like_register_item(inner)
    }

    /// Whether `toks` is a register with at most an index size and scale
    /// after it (`d1`, `d1.l*4`, `%d1:w:2`), checked without reporting
    /// anything.
    fn looks_like_register_item(&self, toks: &[Token]) -> bool {
        let Some((_, mut i)) = self.reg_at(toks, 0) else {
            return false;
        };
        let is_size = |t: Option<&Token>| {
            t.and_then(|t| self.text(t))
                .is_some_and(|s| s == "w" || s == "l")
        };
        let is_int = |t: Option<&Token>| t.is_some_and(|t| matches!(t.kind, TokKind::Int(_)));
        let colon = |t: Option<&Token>| t.is_some_and(|t| t.is_punct(Punct::Colon));
        if colon(toks.get(i)) && is_size(toks.get(i + 1)) {
            i += 2;
        }
        if (colon(toks.get(i)) || toks.get(i).is_some_and(|t| t.is_punct(Punct::Star)))
            && is_int(toks.get(i + 1))
        {
            i += 2;
        }
        i == toks.len()
    }

    /// Finds where a leading expression ends and a `(base,index)` group
    /// begins: the last top-level `(` whose group closes the operand and
    /// holds a register or a comma.
    fn split_disp(&self, toks: &[Token]) -> (usize, Option<usize>) {
        if !toks.last().is_some_and(|t| t.is_punct(Punct::RParen)) {
            return (toks.len(), None);
        }
        let mut depth = 0i32;
        for i in (0..toks.len()).rev() {
            match toks[i].kind {
                TokKind::Punct(Punct::RParen | Punct::RBracket) => depth += 1,
                TokKind::Punct(Punct::LParen | Punct::LBracket) => {
                    depth -= 1;
                    if depth == 0 {
                        if i > 0 && self.is_memory_group(&toks[i + 1..toks.len() - 1]) {
                            return (i, Some(i));
                        }
                        return (toks.len(), None);
                    }
                }
                _ => {}
            }
        }
        (toks.len(), None)
    }

    /// Classifies the items of a `(...)` group, with `disp` already read from
    /// in front of it as in `8(a0,d1.w)`.
    fn group(&mut self, inner: &[Token], disp: Option<Value>, span: Span) -> Option<Mode> {
        let items = split_top(inner);
        if items.iter().any(|i| i.is_empty()) {
            self.cx.error(span, "empty item in a memory operand");
            return None;
        }

        // `([bd,An,Xn],od)` and `([bd,An],Xn,od)`.
        if items[0][0].is_punct(Punct::LBracket) {
            if disp.is_some() {
                self.cx.error(
                    span,
                    "a memory-indirect operand cannot also have a displacement in front",
                );
                return None;
            }
            let first = items[0];
            if matching(first, 0) != Some(first.len() - 1) {
                self.cx
                    .error(first[0].span, "expected `]` to close the indirect part");
                return None;
            }
            let (base, bd, pre) = self.items(&split_top(&first[1..first.len() - 1]))?;
            let mut post = None;
            let mut od = None;
            for item in &items[1..] {
                if let Some(r) = self.index_item(item)? {
                    if post.is_some() || pre.is_some() {
                        self.cx
                            .error(r.span, "a memory operand takes one index register");
                        return None;
                    }
                    post = Some(self.index_of(r)?);
                } else if od.is_none() {
                    od = Some(self.value(item)?);
                } else {
                    self.cx
                        .error(span_of(item), "too many displacements in a memory operand");
                    return None;
                }
            }
            let index = match (pre, post) {
                (Some(i), _) => Some((i, IndexAt::Pre)),
                (_, Some(i)) => Some((i, IndexAt::Post)),
                _ => None,
            };
            return Some(Mode::MemInd {
                base,
                bd,
                index,
                od,
            });
        }

        let (base, inner_disp, index) = self.items(&items)?;
        let disp = match (disp, inner_disp) {
            (Some(_), Some(d)) => {
                self.cx
                    .error(d.span, "a memory operand takes one displacement");
                return None;
            }
            (a, b) => a.or(b),
        };
        match (base, disp, index) {
            (Base::A(a), None, None) => Some(Mode::Ind(a)),
            (Base::None, Some(v), None) => Some(Mode::Abs(v)),
            (base, disp, index) => Some(Mode::Indexed { base, disp, index }),
        }
    }

    /// Sorts the items of a group into base, displacement and index.
    ///
    /// The first address register (or `pc`) written without a size or scale
    /// is the base; any other register is the index. That reads `(a0,a1)` as
    /// base `a0` indexed by `a1`, and `(d1.w,a0)` the way GNU as does.
    fn items(&mut self, items: &[&[Token]]) -> Option<(Base, Option<Value>, Option<Index>)> {
        let mut base = Base::None;
        let mut disp = None;
        let mut regs = Vec::new();
        for item in items {
            match self.index_item(item)? {
                Some(r) => regs.push(r),
                None => {
                    if disp.is_some() {
                        self.cx
                            .error(span_of(item), "a memory operand takes one displacement");
                        return None;
                    }
                    disp = Some(self.value(item)?);
                }
            }
        }
        let mut index = None;
        let mut have_base = false;
        for r in regs {
            let plain = r.long.is_none() && r.scale.is_none();
            match r.reg {
                Reg::A(a) if plain && !have_base => {
                    base = Base::A(a);
                    have_base = true;
                }
                Reg::Pc if !have_base => {
                    if !plain {
                        self.cx.error(r.span, "`pc` cannot be an index register");
                        return None;
                    }
                    base = Base::Pc;
                    have_base = true;
                }
                _ => {
                    if index.is_some() {
                        self.cx
                            .error(r.span, "a memory operand takes one index register");
                        return None;
                    }
                    index = Some(self.index_of(r)?);
                }
            }
        }
        Some((base, disp, index))
    }

    fn index_of(&mut self, r: RegTok) -> Option<Index> {
        let Some(reg) = r.reg.index_bits() else {
            self.cx.error(
                r.span,
                "an index register must be a data or address register",
            );
            return None;
        };
        // Motorola's default index size is a word, and so it is in vasm and
        // Devpac. GNU as makes it a long in both of its syntaxes (`--mri`
        // included), and in GNU syntax that is what its users expect. The
        // difference is semantic — the upper word of the register — so
        // Motorola source keeps Motorola's meaning.
        let long = r.long.unwrap_or(self.gnu);
        Some(Index {
            reg,
            long,
            scale: r.scale.unwrap_or(1),
            sized: r.long.is_some(),
            span: r.span,
        })
    }

    /// GNU's MIT syntax after `reg@`: `@`, `@+`, `@-`, `@(d)`, `@(d,Xn)`,
    /// and the memory-indirect `@(bd)@(od,Xn)`.
    fn mit(&mut self, r: RegTok, rest: &[Token], span: Span) -> Option<Mode> {
        let base = match r.reg {
            Reg::A(a) if r.long.is_none() => Base::A(a),
            Reg::Pc if r.long.is_none() => Base::Pc,
            _ => {
                self.cx
                    .error(r.span, "`@` needs an address register or `pc` before it");
                return None;
            }
        };
        match rest {
            [] => {
                return Some(match base {
                    Base::A(a) => Mode::Ind(a),
                    _ => Mode::Indexed {
                        base,
                        disp: None,
                        index: None,
                    },
                });
            }
            [t] if t.is_punct(Punct::Plus) || t.is_punct(Punct::Minus) => {
                let Base::A(a) = base else {
                    self.cx
                        .error(span, "`pc` cannot be incremented or decremented");
                    return None;
                };
                return Some(if t.is_punct(Punct::Plus) {
                    Mode::PostInc(a)
                } else {
                    Mode::PreDec(a)
                });
            }
            _ => {}
        }
        if !rest[0].is_punct(Punct::LParen) {
            self.cx.error(rest[0].span, "expected `(` after `@`");
            return None;
        }
        let Some(close) = matching(rest, 0) else {
            self.cx.error(rest[0].span, "unclosed `(`");
            return None;
        };
        let (disp, index) = self.mit_items(&rest[1..close])?;
        let after = &rest[close + 1..];
        if after.is_empty() {
            return Some(Mode::Indexed { base, disp, index });
        }
        if !(after.len() >= 3 && after[0].is_punct(Punct::At) && after[1].is_punct(Punct::LParen))
            || matching(after, 1) != Some(after.len() - 1)
        {
            self.cx
                .error(after[0].span, "unexpected token after memory operand");
            return None;
        }
        let (od, post) = self.mit_items(&after[2..after.len() - 1])?;
        let index = match (index, post) {
            (Some(_), Some(p)) => {
                self.cx
                    .error(p.span, "a memory operand takes one index register");
                return None;
            }
            (Some(i), None) => Some((i, IndexAt::Pre)),
            (None, Some(p)) => Some((p, IndexAt::Post)),
            (None, None) => None,
        };
        Some(Mode::MemInd {
            base,
            bd: disp,
            index,
            od,
        })
    }

    fn mit_items(&mut self, inner: &[Token]) -> Option<(Option<Value>, Option<Index>)> {
        if inner.is_empty() {
            return Some((None, None));
        }
        let mut disp = None;
        let mut index = None;
        for item in split_top(inner) {
            if item.is_empty() {
                self.cx
                    .error(span_of(inner), "empty item in a memory operand");
                return None;
            }
            match self.index_item(item)? {
                Some(r) if index.is_none() => index = Some(self.index_of(r)?),
                Some(r) => {
                    self.cx
                        .error(r.span, "a memory operand takes one index register");
                    return None;
                }
                None if disp.is_none() && index.is_none() => disp = Some(self.value(item)?),
                None => {
                    self.cx
                        .error(span_of(item), "unexpected item in a memory operand");
                    return None;
                }
            }
        }
        Some((disp, index))
    }

    /// `d0-d3/a0-a2`, `d0/d2/a5`, `fp0-fp3`, `fpcr/fpsr`.
    fn reglist(&mut self, toks: &[Token]) -> Option<Mode> {
        let mut mask = 0u32;
        let mut i = 0;
        let mut expect_reg = true;
        let mut pending: Option<u8> = None;
        let mut in_range = false;
        while i < toks.len() {
            if expect_reg {
                let Some((r, n)) = self.reg_at(toks, i) else {
                    self.cx
                        .error(toks[i].span, "expected a register in a register list");
                    return None;
                };
                let bit = match r.reg {
                    _ if r.long.is_some() => None,
                    Reg::Fp(n) => Some(16 + n),
                    Reg::Ctl(rid::FPI) => Some(24),
                    Reg::Ctl(rid::FPS) => Some(25),
                    Reg::Ctl(rid::FPC) => Some(26),
                    reg => reg.index_bits(),
                };
                let Some(bit) = bit else {
                    self.cx.error(
                        r.span,
                        "a register list holds general, floating-point and FPU control registers",
                    );
                    return None;
                };
                if in_range {
                    let lo = pending.unwrap_or(bit);
                    let (a, b) = if lo <= bit { (lo, bit) } else { (bit, lo) };
                    for k in a..=b {
                        mask |= 1 << k;
                    }
                    pending = None;
                    in_range = false;
                } else {
                    mask |= 1 << bit;
                    pending = Some(bit);
                }
                i += n;
                expect_reg = false;
            } else {
                let t = toks[i];
                if t.is_punct(Punct::Slash) {
                    pending = None;
                } else if t.is_punct(Punct::Minus) && pending.is_some() {
                    in_range = true;
                } else {
                    self.cx
                        .error(t.span, "expected `/` or `-` in a register list");
                    return None;
                }
                i += 1;
                expect_reg = true;
            }
        }
        if expect_reg {
            self.cx
                .error(span_of(toks), "a register list cannot end with `/` or `-`");
            return None;
        }
        Some(Mode::RegList(mask))
    }
}
