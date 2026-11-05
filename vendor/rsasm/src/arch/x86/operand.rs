//! x86 operand parsing, for both AT&T and Intel syntax.
//!
//! Both parsers produce the same [`Operand`], so the instruction matcher and
//! encoder never need to know which syntax the source used. The two grammars
//! differ more than the sigils suggest:
//!
//! ```text
//! AT&T    seg:disp(base, index, scale)   $imm   %reg   *%reg   label(%rip)
//! Intel   seg:[base + index*scale + disp]  imm    reg    [reg]   [rip + label]
//! ```

use super::reg::{self, Reg, RegClass};
use crate::arch::{AsmCtx, Syntax};
use crate::cursor::Cursor;
use crate::expr::{ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

#[derive(Clone, Debug)]
pub struct Mem {
    pub seg: Option<Reg>,
    pub base: Option<Reg>,
    pub index: Option<Reg>,
    pub scale: u8,
    pub disp: Option<ExprRef>,
    /// `disp(%rip)` / `[rip + disp]`.
    pub rip_relative: bool,
    /// Address-size of the base/index registers, in bytes.
    pub addr_size: u8,
    /// Written in parentheses or brackets, rather than as a bare address. A
    /// bare address can also be a direct branch target.
    pub bracketed: bool,
    pub span: Span,
}

impl Mem {
    fn empty(span: Span) -> Mem {
        Mem {
            seg: None,
            base: None,
            index: None,
            scale: 1,
            disp: None,
            rip_relative: false,
            addr_size: 8,
            bracketed: false,
            span,
        }
    }
}

/// AVX-512 embedded rounding control, written as a pseudo-operand.
///
/// `{rn-sae}` and its siblings both pick a rounding mode and suppress
/// floating-point exceptions; `{sae}` only suppresses them. Both ride in the
/// EVEX `b` bit, with the mode in the `L'L` field that would otherwise hold
/// the vector length — which is why they only exist on 512-bit forms.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RoundCtl {
    Rn,
    Rd,
    Ru,
    Rz,
    /// `{sae}`: exception suppression with the default rounding mode.
    Sae,
}

impl RoundCtl {
    pub fn from_name(name: &str) -> Option<RoundCtl> {
        Some(match name {
            "rn-sae" => RoundCtl::Rn,
            "rd-sae" => RoundCtl::Rd,
            "ru-sae" => RoundCtl::Ru,
            "rz-sae" => RoundCtl::Rz,
            "sae" => RoundCtl::Sae,
            _ => return None,
        })
    }

    /// The `L'L` value this mode encodes.
    ///
    /// `{sae}` has no mode to store, and the field is then ignored by the CPU;
    /// GNU as and llvm-mc both leave it zero rather than the vector length, so
    /// rsasm does too.
    pub fn ll(self) -> u8 {
        match self {
            RoundCtl::Rn | RoundCtl::Sae => 0,
            RoundCtl::Rd => 1,
            RoundCtl::Ru => 2,
            RoundCtl::Rz => 3,
        }
    }

    pub fn is_sae_only(self) -> bool {
        self == RoundCtl::Sae
    }
}

/// The `{...}` decorators AVX-512 hangs off an individual operand.
#[derive(Copy, Clone, Default, Debug)]
pub struct Decor {
    /// Writemask register from `{%k1}`. `k0` is never a writemask — it is the
    /// encoding for "unmasked" — so writing it is rejected.
    pub mask: Option<Reg>,
    /// `{z}`: masked-out elements are zeroed instead of left untouched.
    pub zeroing: bool,
    /// A `{1toN}` broadcast decorator.
    pub broadcast: Option<Broadcast>,
    /// Span covering the decorators, for diagnostics about them.
    pub span: Span,
}

/// A `{1toN}` decorator as written.
///
/// N is redundant — it is the element count the instruction already implies —
/// so it is only kept to check the source against.
#[derive(Copy, Clone, Debug)]
pub struct Broadcast {
    pub span: Span,
    /// The N the source wrote, read straight off its `BadNumber` token.
    pub count: u32,
}

impl Decor {
    pub fn is_empty(&self) -> bool {
        self.mask.is_none() && !self.zeroing && self.broadcast.is_none()
    }
}

#[derive(Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    Imm(ExprRef),
    Mem(Mem),
    /// A branch target given as a plain label or expression.
    #[allow(dead_code)]
    Rel(ExprRef),
    /// `jmp *%rax` / `jmp rax`: an indirect branch through a register or
    /// memory operand.
    Indirect(Box<OperandKind>),
    /// `{rn-sae}` and friends, which the source writes in the operand list but
    /// which encode as bits rather than as an operand.
    Rounding(RoundCtl),
    /// A direct far pointer, `seg:offset` in Intel syntax. AT&T writes the
    /// two halves as separate immediates, `ljmp $seg, $offset`, and the
    /// matcher pairs them up.
    FarPtr {
        seg: ExprRef,
        off: ExprRef,
    },
}

#[derive(Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    /// Explicit operand size in bytes from `dword ptr` or an AT&T suffix.
    pub size_hint: Option<u8>,
    pub decor: Decor,
    pub span: Span,
}

impl Operand {
    pub fn reg(&self) -> Option<Reg> {
        match &self.kind {
            OperandKind::Reg(r) => Some(*r),
            _ => None,
        }
    }

    pub fn is_mem(&self) -> bool {
        matches!(self.kind, OperandKind::Mem(_))
    }

    pub fn rounding(&self) -> Option<RoundCtl> {
        match &self.kind {
            OperandKind::Rounding(r) => Some(*r),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match &self.kind {
            OperandKind::Reg(r) => format!("register `{}`", reg::name_of(*r)),
            OperandKind::Imm(_) => "an immediate".into(),
            OperandKind::Mem(_) => "a memory operand".into(),
            OperandKind::Rel(_) => "a branch target".into(),
            OperandKind::Indirect(_) => "an indirect branch target".into(),
            OperandKind::Rounding(_) => "a rounding-control decorator".into(),
            OperandKind::FarPtr { .. } => "a far pointer".into(),
        }
    }
}

/// Maps `byte`/`word`/`dword`/`qword`/`xmmword` to a width in bytes.
pub fn size_keyword(name: &str) -> Option<u8> {
    Some(match name {
        "byte" => 1,
        "word" => 2,
        "dword" => 4,
        "qword" => 8,
        "fword" => 6,
        "tbyte" | "tword" => 10,
        "xmmword" | "oword" => 16,
        "ymmword" => 32,
        "zmmword" => 64,
        _ => return None,
    })
}

pub struct OperandParser<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
    pub syntax: Syntax,
    /// Address size in bytes implied by the current mode.
    pub addr_size: u8,
    /// The instruction has a relative form, so `short` and `near` name a
    /// displacement width here rather than being names of their own.
    pub relative: bool,
}

impl OperandParser<'_, '_> {
    pub fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        match self.syntax {
            Syntax::Att => self.parse_att(cur),
            Syntax::Intel => self.parse_intel(cur),
        }
    }

    fn expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut p = self.cx.expr_parser();
        p.parse(cur)
    }

    // ---- AVX-512 decorators -----------------------------------------------

    /// Reads a `{rn-sae}`-style pseudo-operand.
    ///
    /// Returns `None` when the brace group is not a rounding mode, having
    /// consumed nothing, so the caller can go on to try a masked operand.
    fn try_rounding(&mut self, cur: &mut Cursor<'_>) -> Option<Option<Operand>> {
        let TokKind::Ident(n) = cur.nth(1).kind else {
            return None;
        };
        let head = self.cx.interner.get(n).to_ascii_lowercase();
        // `{sae}` is one token; `{rn-sae}` lexes as `rn`, `-`, `sae`.
        let (name, len) = if cur.nth(2).is_punct(Punct::Minus) {
            let TokKind::Ident(m) = cur.nth(3).kind else {
                return None;
            };
            (format!("{head}-{}", self.cx.interner.get(m)), 4)
        } else {
            (head, 2)
        };
        let ctl = RoundCtl::from_name(&name.to_ascii_lowercase())?;
        let start = cur.peek().span;
        for _ in 0..len {
            cur.advance();
        }
        let close = cur.peek();
        if cur.eat_punct(Punct::RBrace).is_none() {
            self.cx
                .error(close.span, "expected `}` to close a rounding decorator");
            return Some(None);
        }
        Some(Some(Operand {
            kind: OperandKind::Rounding(ctl),
            size_hint: None,
            decor: Decor::default(),
            span: start.to(close.span),
        }))
    }

    /// Reads `ccmp`'s and `ctest`'s `{dfv=of,sf,zf,cf}` pseudo-suffix, and
    /// answers with the flag mask in the bit order `EVEX.vvvv` keeps.
    ///
    /// GNU as writes it between the mnemonic and the first operand with no
    /// comma, in either syntax, so it is read off the instruction line rather
    /// than parsed as an operand. Returns `None`, having consumed nothing,
    /// when the brace group is not one; `Some(None)` when it is malformed,
    /// having said so.
    pub(super) fn dfv_suffix(&mut self, cur: &mut Cursor<'_>) -> Option<Option<(u8, Span)>> {
        let TokKind::Ident(n) = cur.nth(1).kind else {
            return None;
        };
        if !self.cx.interner.get(n).eq_ignore_ascii_case("dfv") || !cur.nth(2).is_punct(Punct::Eq) {
            return None;
        }
        let start = cur.peek().span;
        cur.advance();
        cur.advance();
        cur.advance();
        // The four flags, in the bit order `EVEX.vvvv` keeps them. An empty
        // list is the default and means all four read as clear.
        let mut mask = 0u8;
        while !cur.check_punct(Punct::RBrace) {
            let tok = cur.peek();
            let TokKind::Ident(f) = tok.kind else {
                self.cx
                    .error(tok.span, "expected `of`, `sf`, `zf` or `cf` in `{dfv=...}`");
                return Some(None);
            };
            cur.advance();
            let bit = match self.cx.interner.get(f).to_ascii_lowercase().as_str() {
                "of" => 8,
                "sf" => 4,
                "zf" => 2,
                "cf" => 1,
                other => {
                    self.cx.error(
                        tok.span,
                        format!("`{other}` is not one of `of`, `sf`, `zf` and `cf`"),
                    );
                    return Some(None);
                }
            };
            if mask & bit != 0 {
                self.cx
                    .error(tok.span, "the same flag is named twice in `{dfv=...}`");
                return Some(None);
            }
            mask |= bit;
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        let close = cur.peek();
        if cur.eat_punct(Punct::RBrace).is_none() {
            self.cx
                .error(close.span, "expected `}` to close `{dfv=...}`");
            return Some(None);
        }
        Some(Some((mask, start.to(close.span))))
    }

    /// An operand that starts with `{`, which can only be a rounding mode.
    fn rounding_operand(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        if let Some(o) = self.try_rounding(cur) {
            return o;
        }
        self.cx.error(
            cur.remaining_span(),
            "expected a rounding-control decorator: `{rn-sae}`, `{rd-sae}`, `{ru-sae}`, `{rz-sae}` or `{sae}`",
        );
        None
    }

    /// Reads any run of `{...}` decorators that follows an operand.
    fn decorators(&mut self, cur: &mut Cursor<'_>) -> Option<Decor> {
        let mut decor = Decor::default();
        while cur.check_punct(Punct::LBrace) {
            let open = cur.advance();
            self.one_decorator(cur, &mut decor, open.span)?;
            let close = cur.peek();
            if cur.eat_punct(Punct::RBrace).is_none() {
                self.cx
                    .error(close.span, "expected `}` to close an operand decorator");
                return None;
            }
            decor.span = if decor.span.is_dummy() {
                open.span.to(close.span)
            } else {
                decor.span.to(close.span)
            };
        }
        Some(decor)
    }

    fn one_decorator(&mut self, cur: &mut Cursor<'_>, decor: &mut Decor, open: Span) -> Option<()> {
        // `{1toN}`. It starts with a digit, so it reaches here as a
        // `BadNumber` token carrying its text; inside a decorator that is not
        // an error, it is the broadcast count.
        if let TokKind::Int(_) | TokKind::BadNumber(_) = cur.peek().kind {
            let tok = cur.advance();
            let count = match tok.kind {
                TokKind::BadNumber(name) => {
                    let text = self.cx.name(name).to_string();
                    match text.strip_prefix("1to").and_then(|n| n.parse::<u32>().ok()) {
                        Some(n) => n,
                        None => {
                            self.cx.error(
                                tok.span,
                                format!(
                                    "`{{{text}}}` is not a broadcast decorator; expected `{{1toN}}`"
                                ),
                            );
                            return None;
                        }
                    }
                }
                // A well-formed integer is never a broadcast count.
                _ => {
                    self.cx
                        .error(tok.span, "expected `1toN` in a broadcast decorator");
                    return None;
                }
            };
            if decor.broadcast.is_some() {
                self.cx
                    .error(tok.span, "duplicate broadcast decorator on one operand");
                return None;
            }
            decor.broadcast = Some(Broadcast {
                span: tok.span,
                count,
            });
            return Some(());
        }

        let masked = cur.eat_punct(Punct::Percent).is_some();
        let tok = cur.peek();
        let TokKind::Ident(n) = tok.kind else {
            self.cx.error(
                open.to(tok.span),
                "expected `z`, a mask register or `1toN` in an operand decorator",
            );
            return None;
        };
        cur.advance();
        let text = self.cx.interner.get(n).to_ascii_lowercase();
        if text == "z" && !masked {
            if decor.zeroing {
                self.cx.error(tok.span, "duplicate `{z}` decorator");
                return None;
            }
            decor.zeroing = true;
            return Some(());
        }
        match reg::lookup(&text) {
            Some(r) if r.class == RegClass::Mask => {
                if r.num == 0 {
                    // `k0` is the encoding for "no writemask", so it can never
                    // be named as one; the source almost certainly meant k1-k7.
                    self.cx
                        .error(tok.span, "`k0` cannot be used as a writemask register");
                    return None;
                }
                if decor.mask.is_some() {
                    self.cx
                        .error(tok.span, "an operand may carry only one writemask");
                    return None;
                }
                decor.mask = Some(r);
                Some(())
            }
            _ => {
                self.cx.error(
                    tok.span,
                    format!("`{text}` is not a mask register or operand decorator"),
                );
                None
            }
        }
    }

    // ---- AT&T -------------------------------------------------------------

    fn parse_att(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;

        // A lone `{rn-sae}` is an operand of its own in AT&T syntax, written
        // before the sources.
        if cur.check_punct(Punct::LBrace) {
            return self.rounding_operand(cur);
        }

        // `*` marks an indirect branch target.
        if cur.eat_punct(Punct::Star).is_some() {
            let inner = self.parse_att(cur)?;
            return Some(Operand {
                kind: OperandKind::Indirect(Box::new(inner.kind)),
                size_hint: inner.size_hint,
                decor: inner.decor,
                span: start.to(inner.span),
            });
        }

        // `$imm`
        if cur.eat_punct(Punct::Dollar).is_some() {
            let e = self.expr(cur)?;
            return Some(Operand {
                kind: OperandKind::Imm(e),
                size_hint: None,
                decor: Decor::default(),
                span: start.to(self.cx.exprs.span(e)),
            });
        }

        // `%reg`, or `%seg:` introducing a memory operand.
        if cur.check_punct(Punct::Percent) {
            let r = self.att_register(cur)?;
            if r.class == RegClass::Segment && cur.check_punct(Punct::Colon) {
                cur.advance();
                let mut m = self.att_memory(cur, start)?;
                m.seg = Some(r);
                let span = start.to(m.span);
                let decor = self.decorators(cur)?;
                return Some(Operand {
                    kind: OperandKind::Mem(m),
                    size_hint: None,
                    decor,
                    span,
                });
            }
            let decor = self.decorators(cur)?;
            return Some(Operand {
                kind: OperandKind::Reg(r),
                size_hint: Some(r.size),
                decor,
                span: start,
            });
        }

        // Anything else is a memory operand: `disp`, `disp(...)` or `(...)`.
        let m = self.att_memory(cur, start)?;
        let span = start.to(m.span);
        let decor = self.decorators(cur)?;
        Some(Operand {
            kind: OperandKind::Mem(m),
            size_hint: None,
            decor,
            span,
        })
    }

    fn att_register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let pct = cur.advance(); // `%`
        let tok = cur.peek();
        let TokKind::Ident(n) = tok.kind else {
            self.cx
                .error(pct.span.to(tok.span), "expected a register name after `%`");
            return None;
        };
        cur.advance();
        let text = self.cx.interner.get(n).to_ascii_lowercase();
        match reg::lookup(&text) {
            Some(r) if r.class == RegClass::St => self.st_index(cur, r),
            Some(r) if self.cx.state.bits != 64 && r.only_64() => {
                self.cx.error(
                    pct.span.to(tok.span),
                    format!("`%{text}` is only available in 64-bit mode"),
                );
                None
            }
            Some(r) => Some(r),
            None => {
                self.cx
                    .error(pct.span.to(tok.span), format!("unknown register `%{text}`"));
                None
            }
        }
    }

    /// The `(n)` of `st(n)`, if one follows `st`.
    fn st_index(&mut self, cur: &mut Cursor<'_>, top: Reg) -> Option<Reg> {
        if !cur.check_punct(Punct::LParen) {
            return Some(top);
        }
        let open = cur.advance();
        let tok = cur.advance();
        let close = cur.peek();
        let n = match tok.kind {
            TokKind::Int(v) => reg::st(u8::try_from(v).unwrap_or(u8::MAX)),
            _ => None,
        };
        let Some(r) = n else {
            self.cx.error(
                open.span.to(tok.span),
                "an x87 stack register is `st(0)` to `st(7)`",
            );
            return None;
        };
        if cur.eat_punct(Punct::RParen).is_none() {
            self.cx
                .error(close.span, "expected `)` after the x87 stack index");
            return None;
        }
        Some(r)
    }

    /// `disp(base, index, scale)`, any part of which may be absent.
    fn att_memory(&mut self, cur: &mut Cursor<'_>, start: Span) -> Option<Mem> {
        let mut m = Mem::empty(start);
        m.addr_size = self.addr_size;

        if !cur.check_punct(Punct::LParen) {
            m.disp = Some(self.expr(cur)?);
        }

        if cur.eat_punct(Punct::LParen).is_none() {
            m.span = start.to(cur.peek().span.shrink_to_lo());
            return Some(m);
        }
        m.bracketed = true;

        // base
        if cur.check_punct(Punct::Percent) {
            let r = self.att_register(cur)?;
            if r.class == RegClass::Rip {
                m.rip_relative = true;
            } else if r.class == RegClass::Gpr {
                m.base = Some(r);
                m.addr_size = r.size;
            } else {
                self.cx.error(cur.peek().span, "invalid base register");
                return None;
            }
        }

        // index and scale
        if cur.eat_punct(Punct::Comma).is_some() {
            if cur.check_punct(Punct::Percent) {
                let r = self.att_register(cur)?;
                if !r.valid_index() {
                    self.cx.error(
                        cur.peek().span,
                        format!("`%{}` cannot be used as an index register", reg::name_of(r)),
                    );
                    return None;
                }
                m.index = Some(r);
                note_addr_size(&mut m, r);
            }
            if cur.eat_punct(Punct::Comma).is_some() {
                let tok = cur.peek();
                let e = self.expr(cur)?;
                match self.cx.constant(e) {
                    Some(s @ (1 | 2 | 4 | 8)) => m.scale = s as u8,
                    _ => {
                        self.cx.error(tok.span, "scale must be 1, 2, 4 or 8");
                        return None;
                    }
                }
            }
        }

        let close = cur.peek();
        if cur.eat_punct(Punct::RParen).is_none() {
            self.cx
                .error(close.span, "expected `)` to close a memory operand");
            return None;
        }
        m.span = start.to(close.span);
        Some(m)
    }

    // ---- Intel ------------------------------------------------------------

    fn parse_intel(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;
        let mut size_hint = None;

        if cur.check_punct(Punct::LBrace) {
            return self.rounding_operand(cur);
        }

        // `dword ptr [...]`, or just `dword [...]` as NASM allows.
        if let TokKind::Ident(n) = cur.peek().kind {
            let text = self.cx.interner.get(n).to_ascii_lowercase();
            if let Some(sz) = size_keyword(&text) {
                // Only a size keyword if what follows can start an operand;
                // `byte` might legitimately be a symbol name. NASM makes the
                // size words reserved, so there it also qualifies an
                // immediate: `mov [eax], byte 1`.
                let next = cur.nth(1);
                let looks_like_ptr = matches!(next.kind, TokKind::Ident(m)
                    if self.cx.interner.get(m).eq_ignore_ascii_case("ptr"))
                    || next.is_punct(Punct::LBracket);
                let nasm_hint = self.cx.dialect == crate::lexer::Dialect::Nasm
                    && !next.is_eol()
                    && !next.is_punct(Punct::Comma);
                if looks_like_ptr || nasm_hint {
                    cur.advance();
                    if let TokKind::Ident(m) = cur.peek().kind
                        && self.cx.interner.get(m).eq_ignore_ascii_case("ptr")
                    {
                        cur.advance();
                    }
                    size_hint = Some(sz);
                }
            }
        }

        // NASM names a branch's width in front of the target instead, with
        // `short` for the one-byte displacement and `near` for the mode's
        // full-width one. Both are the width of the displacement, so they
        // land in the same field a size keyword would, and `near` is the
        // width that gets written rather than the width of the row: two
        // bytes in 16-bit code.
        if size_hint.is_none()
            && self.relative
            && self.cx.dialect == crate::lexer::Dialect::Nasm
            && let TokKind::Ident(n) = cur.peek().kind
        {
            let text = self.cx.interner.get(n).to_ascii_lowercase();
            let width = match text.as_str() {
                "short" => Some(1),
                "near" => Some(if self.cx.state.bits == 16 { 2 } else { 4 }),
                _ => None,
            };
            let next = cur.nth(1);
            if let Some(w) = width
                && !next.is_eol()
                && !next.is_punct(Punct::Comma)
            {
                cur.advance();
                size_hint = Some(w);
            }
        }

        // A bare register.
        if let TokKind::Ident(n) = cur.peek().kind {
            let text = self.cx.interner.get(n).to_ascii_lowercase();
            if let Some(r) = reg::lookup_in_mode(&text, self.cx.state.bits) {
                cur.advance();
                let r = if r.class == RegClass::St {
                    self.st_index(cur, r)?
                } else {
                    r
                };
                // `seg:[...]`
                if r.class == RegClass::Segment && cur.check_punct(Punct::Colon) {
                    cur.advance();
                    let mut m = self.intel_memory(cur, start)?;
                    m.seg = Some(r);
                    let span = start.to(m.span);
                    let decor = self.decorators(cur)?;
                    return Some(Operand {
                        kind: OperandKind::Mem(m),
                        size_hint,
                        decor,
                        span,
                    });
                }
                let decor = self.decorators(cur)?;
                return Some(Operand {
                    kind: OperandKind::Reg(r),
                    size_hint: size_hint.or(Some(r.size)),
                    decor,
                    span: start,
                });
            }
        }

        if cur.check_punct(Punct::LBracket) {
            let m = self.intel_memory(cur, start)?;
            let span = start.to(m.span);
            let decor = self.decorators(cur)?;
            return Some(Operand {
                kind: OperandKind::Mem(m),
                size_hint,
                decor,
                span,
            });
        }

        // `offset sym` is the address as an immediate.
        let offset = matches!(cur.peek().kind, TokKind::Ident(n)
            if self.cx.interner.get(n).eq_ignore_ascii_case("offset"))
            && !cur.nth(1).is_punct(Punct::Comma)
            && !cur.nth(1).is_eol();
        if offset {
            cur.advance();
        }

        // Otherwise an immediate or branch target; the matcher decides which.
        let e = self.expr(cur)?;
        // `seg:offset`, a direct far branch target.
        if !offset && cur.eat_punct(Punct::Colon).is_some() {
            let off = self.expr(cur)?;
            return Some(Operand {
                kind: OperandKind::FarPtr { seg: e, off },
                size_hint,
                decor: Decor::default(),
                span: start.to(self.cx.exprs.span(off)),
            });
        }
        // A value that is not a known constant names an address, and without
        // `offset` an address in GNU Intel syntax means the memory there:
        // `mov eax, sym` is a load. A branch reads the same operand as its
        // target. NASM writes memory in brackets only, so there it is the
        // address.
        if !offset
            && self.cx.dialect != crate::lexer::Dialect::Nasm
            && self.cx.constant(e).is_none()
        {
            let span = start.to(self.cx.exprs.span(e));
            let mut m = Mem::empty(span);
            m.addr_size = self.addr_size;
            m.disp = Some(e);
            return Some(Operand {
                kind: OperandKind::Mem(m),
                size_hint,
                decor: Decor::default(),
                span,
            });
        }
        Some(Operand {
            kind: OperandKind::Imm(e),
            size_hint,
            decor: Decor::default(),
            span: start.to(self.cx.exprs.span(e)),
        })
    }

    /// Reads a `rel` or `abs` inside a memory operand, which says whether an
    /// address with no register is RIP-relative whatever `default rel` asked
    /// for. Anything else is left where it is.
    fn rip_keyword(&mut self, cur: &mut Cursor<'_>, m: &mut Mem, force_abs: &mut bool) {
        if let TokKind::Ident(n) = cur.peek().kind {
            match self.cx.interner.get(n).to_ascii_lowercase().as_str() {
                "rel" => {
                    cur.advance();
                    m.rip_relative = true;
                }
                "abs" => {
                    cur.advance();
                    *force_abs = true;
                }
                _ => {}
            }
        }
    }

    /// `[ base + index*scale + disp ]`, in any order.
    fn intel_memory(&mut self, cur: &mut Cursor<'_>, start: Span) -> Option<Mem> {
        let open = cur.peek();
        if cur.eat_punct(Punct::LBracket).is_none() {
            self.cx
                .error(open.span, "expected `[` to start a memory operand");
            return None;
        }
        let mut m = Mem::empty(start);
        m.addr_size = self.addr_size;
        m.bracketed = true;

        // `[rel x]` forces a RIP-relative reference, `[abs x]` an absolute one,
        // overriding `default rel`.
        let mut force_abs = false;
        self.rip_keyword(cur, &mut m, &mut force_abs);

        // A segment override written inside the brackets, `[es:eax]`, the way
        // NASM spells it.
        if let TokKind::Ident(n) = cur.peek().kind
            && cur.nth(1).is_punct(Punct::Colon)
        {
            let text = self.cx.interner.get(n).to_ascii_lowercase();
            if let Some(r) = reg::lookup(&text)
                && r.class == RegClass::Segment
            {
                cur.advance();
                cur.advance();
                m.seg = Some(r);
            }
        }

        // NASM takes the keyword on either side of a segment override, so
        // `[ss:rel x]` reads as `[rel ss:x]` does.
        if !m.rip_relative && !force_abs {
            self.rip_keyword(cur, &mut m, &mut force_abs);
        }

        // Terms are accumulated into a displacement expression as they are
        // recognised, so `[rax + 4*8 + sym]` folds naturally.
        let mut disp: Option<ExprRef> = None;
        let mut negate_next = false;
        loop {
            if cur.check_punct(Punct::RBracket) || cur.at_end() {
                break;
            }
            let term_start = cur.peek().span;
            let term = self.intel_term(cur, &mut m, negate_next)?;
            if let Some(e) = term {
                let e = if negate_next {
                    let span = self.cx.exprs.span(e);
                    self.cx
                        .exprs
                        .alloc(ExprKind::Unary(crate::expr::UnOp::Neg, e), span)
                } else {
                    e
                };
                disp = Some(match disp {
                    None => e,
                    Some(prev) => {
                        let span = self.cx.exprs.span(prev).to(self.cx.exprs.span(e));
                        self.cx
                            .exprs
                            .alloc(ExprKind::Binary(crate::expr::BinOp::Add, prev, e), span)
                    }
                });
            }
            match cur.peek().kind {
                TokKind::Punct(Punct::Plus) => {
                    cur.advance();
                    negate_next = false;
                }
                TokKind::Punct(Punct::Minus) => {
                    cur.advance();
                    negate_next = true;
                }
                TokKind::Punct(Punct::RBracket) => break,
                _ => {
                    let t = cur.peek();
                    if t.span == term_start {
                        // No progress: bail rather than spin.
                        self.cx
                            .error(t.span, "expected `+`, `-` or `]` in memory operand");
                        return None;
                    }
                    self.cx
                        .error(t.span, "expected `+`, `-` or `]` in memory operand");
                    return None;
                }
            }
        }

        let close = cur.peek();
        if cur.eat_punct(Punct::RBracket).is_none() {
            self.cx
                .error(close.span, "expected `]` to close a memory operand");
            return None;
        }
        m.disp = disp;
        m.span = start.to(close.span);
        // NASM turns an index with nothing else into a base: `[esi*1]` is
        // `[esi]`, and `[esi*2]` is `[esi+esi*1]`. Both are what an
        // index-only address means, and both are four bytes shorter, since
        // an index with no base needs a displacement it does not otherwise
        // have. GNU as writes the long form for either, so the dialect
        // decides. This is after the whole operand has been read, because
        // `[esi*2+eax]` does have a base and must keep the scale it was
        // written with.
        if self.cx.dialect == crate::lexer::Dialect::Nasm
            && m.base.is_none()
            && let Some(r) = m.index
            && !r.is_vector()
            && matches!(m.scale, 1 | 2)
        {
            m.base = Some(r);
            if m.scale == 1 {
                m.index = None;
            } else {
                m.scale = 1;
            }
        }
        // 16-bit addressing pairs `bx` or `bp` with `si` or `di`, and ModRM
        // encodes the pair rather than an order, so `[si+bx]` is `[bx+si]`.
        if let (Some(b), Some(i)) = (m.base, m.index)
            && b.size == 2
            && i.size == 2
            && m.scale == 1
            && matches!(b.num, 6 | 7)
            && matches!(i.num, 3 | 5)
        {
            m.base = Some(i);
            m.index = Some(b);
        }
        // `default rel` makes a reference to a symbol RIP-relative when it has
        // no register of its own and was not written `[abs …]`. A pure number
        // stays absolute, as NASM leaves it.
        let default_rel = self.cx.state.features & crate::arch::FEATURE_DEFAULT_REL != 0;
        if default_rel
            && !force_abs
            && !m.rip_relative
            && m.base.is_none()
            && m.index.is_none()
            && m.disp.is_some_and(|e| self.disp_is_symbolic(e))
        {
            m.rip_relative = true;
        }
        Some(m)
    }

    /// Whether a displacement expression names a symbol, so `default rel`
    /// applies to it. A plain constant does not.
    fn disp_is_symbolic(&self, e: ExprRef) -> bool {
        use crate::expr::ExprKind::*;
        match &self.cx.exprs.get(e).kind {
            Sym(_) | SymId(_) | Here | SectionStart | LocalRef(..) => true,
            Unary(_, a) | Modifier(_, a) => self.disp_is_symbolic(*a),
            Binary(_, a, b) => self.disp_is_symbolic(*a) || self.disp_is_symbolic(*b),
            Int(_) => false,
        }
    }

    /// One `+`-separated term inside `[...]`. Registers are stored into `m`;
    /// anything else is returned as part of the displacement.
    fn intel_term(
        &mut self,
        cur: &mut Cursor<'_>,
        m: &mut Mem,
        negated: bool,
    ) -> Option<Option<ExprRef>> {
        // `reg` or `reg*scale`
        if let TokKind::Ident(n) = cur.peek().kind {
            let text = self.cx.interner.get(n).to_ascii_lowercase();
            if let Some(r) = reg::lookup_in_mode(&text, self.cx.state.bits) {
                let tok = cur.advance();
                if negated {
                    self.cx.error(
                        tok.span,
                        "a register cannot be subtracted in a memory operand",
                    );
                    return None;
                }
                if r.class == RegClass::Rip {
                    m.rip_relative = true;
                    return Some(None);
                }
                // A vector register here is a VSIB index, as gather and
                // scatter use; anything else cannot address memory at all.
                if r.class != RegClass::Gpr && !r.is_vector() {
                    self.cx.error(
                        tok.span,
                        "only general-purpose registers may address memory",
                    );
                    return None;
                }
                if r.is_vector() && m.index.is_some() {
                    self.cx
                        .error(tok.span, "a memory operand may have only one index");
                    return None;
                }
                // A vector register is always the index, never the base, so
                // `[zmm1]` does not silently become base-relative addressing.
                if r.is_vector() && !cur.check_punct(Punct::Star) {
                    m.index = Some(r);
                    return Some(None);
                }
                // `reg * scale` makes it the index.
                if cur.check_punct(Punct::Star) {
                    cur.advance();
                    let stok = cur.peek();
                    // Only the scale itself, not the `+ disp` that may follow.
                    let e = self.intel_disp_term(cur)?;
                    // NASM folds `reg*3`, `reg*5` and `reg*9` into
                    // `reg + reg*2/4/8` when there is no base yet, since
                    // those scales have no encoding of their own.
                    if let Some(s @ (3 | 5 | 9)) = self.cx.constant(e)
                        && m.base.is_none()
                        && m.index.is_none()
                        && r.valid_index()
                    {
                        m.base = Some(r);
                        m.index = Some(r);
                        m.scale = (s - 1) as u8;
                        note_addr_size(m, r);
                        return Some(None);
                    }
                    let Some(s @ (1 | 2 | 4 | 8)) = self.cx.constant(e) else {
                        self.cx.error(stok.span, "scale must be 1, 2, 4 or 8");
                        return None;
                    };
                    if m.index.is_some() {
                        self.cx.error(
                            tok.span,
                            "a memory operand may have only one index register",
                        );
                        return None;
                    }
                    if !r.valid_index() {
                        // `[esp*1]` is the one scale that needs no index at
                        // all, and NASM reads it as the base it means. The
                        // fold at the end of `intel_memory` cannot do this
                        // one, because by then the operand has been refused.
                        if s == 1
                            && m.base.is_none()
                            && self.cx.dialect == crate::lexer::Dialect::Nasm
                        {
                            m.base = Some(r);
                            note_addr_size(m, r);
                            return Some(None);
                        }
                        self.cx.error(
                            tok.span,
                            format!("`{}` cannot be used as an index register", reg::name_of(r)),
                        );
                        return None;
                    }
                    m.index = Some(r);
                    m.scale = s as u8;
                    note_addr_size(m, r);
                    return Some(None);
                }
                // First bare register is the base, a second becomes the index.
                if m.base.is_none() {
                    m.base = Some(r);
                } else if m.index.is_none() {
                    if !r.valid_index() {
                        // `[rax + rsp]` is invalid, but `[rsp + rax]` is fine:
                        // swap so the unusable register becomes the base.
                        if m.base.is_some_and(|b| b.valid_index()) {
                            m.index = m.base;
                            m.base = Some(r);
                        } else {
                            self.cx.error(
                                tok.span,
                                format!(
                                    "`{}` cannot be used as an index register",
                                    reg::name_of(r)
                                ),
                            );
                            return None;
                        }
                    } else {
                        m.index = Some(r);
                    }
                } else {
                    self.cx
                        .error(tok.span, "too many registers in a memory operand");
                    return None;
                }
                note_addr_size(m, r);
                return Some(None);
            }
        }

        // `scale * reg`
        if let TokKind::Int(v) = cur.peek().kind
            && cur.nth(1).is_punct(Punct::Star)
            && let TokKind::Ident(n) = cur.nth(2).kind
        {
            let text = self.cx.interner.get(n).to_ascii_lowercase();
            if let Some(r) = reg::lookup_in_mode(&text, self.cx.state.bits) {
                let tok = cur.peek();
                if !matches!(v, 1 | 2 | 4 | 8) {
                    self.cx.error(tok.span, "scale must be 1, 2, 4 or 8");
                    return None;
                }
                if !r.valid_index() {
                    self.cx.error(
                        tok.span,
                        format!("`{}` cannot be used as an index register", reg::name_of(r)),
                    );
                    return None;
                }
                cur.advance();
                cur.advance();
                cur.advance();
                m.index = Some(r);
                m.scale = v as u8;
                note_addr_size(m, r);
                return Some(None);
            }
        }

        // Everything else contributes to the displacement. Parse at a
        // precedence above `+`/`-` so those stay term separators.
        let e = self.intel_disp_term(cur)?;
        Some(Some(e))
    }

    /// A displacement term: a full expression except that top-level `+` and
    /// `-` are left for [`Self::intel_memory`] to consume.
    fn intel_disp_term(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let sub = take_until_term_break(cur);
        let mut sub_cur = Cursor::new(sub);
        let e = {
            let mut p = self.cx.expr_parser();
            p.parse(&mut sub_cur)?
        };
        if !sub_cur.at_end() && !sub_cur.is_empty() {
            self.cx
                .error(sub_cur.peek().span, "unexpected token in memory operand");
            return None;
        }
        Some(e)
    }
}

/// Records the address size a base or index register implies.
///
/// A VSIB index is a vector register, which says nothing about how wide the
/// address is: that still comes from the base, or from the mode.
fn note_addr_size(m: &mut Mem, r: Reg) {
    if r.class == RegClass::Gpr {
        m.addr_size = r.size;
    }
}

/// Consumes tokens up to the next top-level `+`, `-` or `]`.
fn take_until_term_break<'t>(cur: &mut Cursor<'t>) -> &'t [Token] {
    let rest = cur.rest();
    let mut depth = 0i32;
    let mut end = rest.len();
    for (i, t) in rest.iter().enumerate() {
        match t.kind {
            TokKind::Punct(Punct::LParen | Punct::LBracket) => depth += 1,
            TokKind::Punct(Punct::RParen) => depth -= 1,
            TokKind::Punct(Punct::RBracket) => {
                if depth == 0 {
                    end = i;
                    break;
                }
                depth -= 1;
            }
            TokKind::Punct(Punct::Plus | Punct::Minus) if depth == 0 && i > 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let out = &rest[..end];
    cur.set_pos(cur.pos() + end);
    out
}
