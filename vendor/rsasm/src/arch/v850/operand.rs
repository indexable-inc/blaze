//! Operand parsing.
//!
//! The grammar GNU as accepts for V850 is small and has no sigils:
//!
//! ```text
//! r5  sp  lp            general register
//! [r5]                  register used as an address
//! 16[sp]  lo(x)[r1]     displacement and base register
//! 42  x+4  hi(x)        immediate, optionally wrapped in a relocation function
//! {r20-r29, r31}        register list (prepare / dispose)
//! r20-r25               register range (pushsp / popsp / dbpush)
//! psw  z  chbii         names of system registers, conditions and operations
//! ```
//!
//! The last line is why this parser does not decide what a bare word means.
//! `z` is a condition to `setf` and a symbol to `movea`, so a word that is not
//! a general register is kept as an immediate that remembers its spelling,
//! and the instruction's operand slot interprets it.
//!
//! Relocation functions are written as prefixes, `hi(sym)`, and GNU as applies
//! one to *everything that follows it* in the operand: `lo(sym)+4` means
//! `lo(sym+4)`. The shared expression parser only knows the `sym@mod` suffix
//! form, so the prefix is recognised here and the rest of the operand is
//! parsed as an ordinary expression.

use super::reg;
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::intern::Name;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// A relocation function wrapped around an immediate.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RelFn {
    None,
    /// `hi(x)`: the high half of `x`, *adjusted* for a sign-extended `lo(x)`.
    Hi,
    /// `hi0(x)`: the high half of `x`, unadjusted.
    Hi0,
    /// `lo(x)`: the low 16 bits of `x`, read as signed.
    Lo,
    /// `hilo(x)`: all 32 bits, for the 48-bit `mov` and `prepare`.
    HiLo,
    /// `lo23(x)`: the low 23 bits, for the 48-bit loads and stores.
    Lo23,
    /// `zdaoff(x)`: `x` as an offset from address 0, i.e. from `r0`.
    ZdaOff,
    /// `sdaoff(x)`: `x` as an offset from the small-data pointer.
    SdaOff,
    /// `tdaoff(x)`: `x` as an offset from the tiny-data pointer.
    TdaOff,
    /// `ctoff(x)`: `x` as an offset into the `callt` table.
    CtOff,
}

impl RelFn {
    fn from_name(name: &str) -> Option<RelFn> {
        // Case-sensitive, like GNU as: `LO(x)` is not a relocation function.
        Some(match name {
            "hi" => RelFn::Hi,
            "hi0" => RelFn::Hi0,
            "lo" => RelFn::Lo,
            "hilo" => RelFn::HiLo,
            "lo23" => RelFn::Lo23,
            "zdaoff" => RelFn::ZdaOff,
            "sdaoff" => RelFn::SdaOff,
            "tdaoff" => RelFn::TdaOff,
            "ctoff" => RelFn::CtOff,
            _ => return None,
        })
    }

    pub fn spelling(self) -> &'static str {
        match self {
            RelFn::None => "",
            RelFn::Hi => "hi()",
            RelFn::Hi0 => "hi0()",
            RelFn::Lo => "lo()",
            RelFn::HiLo => "hilo()",
            RelFn::Lo23 => "lo23()",
            RelFn::ZdaOff => "zdaoff()",
            RelFn::SdaOff => "sdaoff()",
            RelFn::TdaOff => "tdaoff()",
            RelFn::CtOff => "ctoff()",
        }
    }

    /// Applies the function to a value known at assembly time.
    ///
    /// The results are what the field receives, read as signed where the
    /// field is: `lo(0x12348000)` is -32768, not 32768.
    pub fn apply(self, v: i64) -> i64 {
        match self {
            RelFn::None | RelFn::HiLo | RelFn::Lo23 => v,
            RelFn::Lo | RelFn::ZdaOff | RelFn::SdaOff | RelFn::TdaOff | RelFn::CtOff => sext16(v),
            RelFn::Hi0 => sext16(v >> 16),
            RelFn::Hi => sext16(hi_adjusted(v)),
        }
    }
}

/// Sign-extends the low 16 bits.
pub fn sext16(v: i64) -> i64 {
    v as i16 as i64
}

/// The high half of `v`, adjusted so that adding the sign-extended low half
/// gives `v` back.
///
/// `movhi hi(x), r0, r1` followed by `movea lo(x), r1, r1` loads `x`, but
/// `movea` sign-extends its 16-bit operand. When bit 15 of `x` is set, `lo(x)`
/// is negative and subtracts 0x10000 from what `movhi` loaded, so `hi(x)` has
/// to be one more than the plain top half to compensate. Adding bit 15 into
/// the top half does exactly that: `hi(0x12348000)` is 0x1235, and
/// 0x12350000 + (-0x8000) is 0x12348000. The carry out of bit 31 is dropped,
/// which is what the 32-bit machine does too.
pub fn hi_adjusted(v: i64) -> i64 {
    ((v >> 16) & 0xffff) + ((v >> 15) & 1)
}

#[derive(Copy, Clone, Debug)]
pub struct Imm {
    pub expr: ExprRef,
    pub func: RelFn,
    /// The word as written, if the operand was a single identifier. Slots
    /// that take names (conditions, system registers) look at this first.
    pub ident: Option<Name>,
    pub span: Span,
}

#[derive(Copy, Clone, Debug)]
pub enum ArgKind {
    /// `r5`.
    Reg(u8),
    /// `[r5]`.
    Bracket(u8),
    /// `disp[r5]`.
    Mem { disp: Imm, base: u8 },
    /// An expression, or a name the slot will interpret.
    Imm(Imm),
    /// `{...}`: a set of registers, bit `n` standing for `rn`.
    List(u32),
}

#[derive(Copy, Clone, Debug)]
pub struct Arg {
    pub kind: ArgKind,
    pub span: Span,
}

impl Arg {
    pub fn describe(&self) -> &'static str {
        match self.kind {
            ArgKind::Reg(_) => "a register",
            ArgKind::Bracket(_) => "a bracketed register",
            ArgKind::Mem { .. } => "a memory operand",
            ArgKind::Imm(_) => "an immediate",
            ArgKind::List(_) => "a register list",
        }
    }
}

/// Splits and parses the operands of one statement.
///
/// `ranges` allows `rA-rB` to stand for the two operands `rA, rB`, which GNU
/// as allows only for `pushsp`, `popsp` and `dbpush`; anywhere else the same
/// text is a register used in an expression, and an error.
pub fn parse_operands(
    cx: &mut AsmCtx<'_>,
    toks: &[Token],
    whole: Span,
    ranges: bool,
) -> Option<Vec<Arg>> {
    // CC-RH writes the address of a label inside a separator operator as
    // `HIGHW1(#label)` (CC-RH page 499). Inside parentheses an address is the
    // only thing a label can mean, so the `#` is dropped.
    let stripped: Vec<Token>;
    let toks = if cx.dialect == crate::lexer::Dialect::CcRh {
        stripped = toks
            .iter()
            .enumerate()
            .filter(|(i, t)| {
                !(t.is_punct(Punct::Hash) && *i > 0 && toks[i - 1].is_punct(Punct::LParen))
            })
            .map(|(_, t)| *t)
            .collect();
        &stripped[..]
    } else {
        toks
    };
    let cur = Cursor::new(toks);
    if cur.at_end() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for piece in cur.split_commas() {
        let mut p = Parser { cx };
        if ranges && let Some((a, b)) = p.range(piece) {
            out.push(a);
            out.push(b);
            continue;
        }
        out.push(p.operand(piece, whole)?);
    }
    Some(out)
}

struct Parser<'c, 'a> {
    cx: &'c mut AsmCtx<'a>,
}

impl Parser<'_, '_> {
    fn name(&self, n: Name) -> &str {
        self.cx.name(n)
    }

    fn reg_token(&self, t: &Token) -> Option<u8> {
        t.ident().and_then(|n| reg::gpr(self.name(n)))
    }

    /// `rA-rB`, if that is exactly what `toks` holds.
    fn range(&self, toks: &[Token]) -> Option<(Arg, Arg)> {
        let [a, dash, b] = toks else { return None };
        if !dash.is_punct(Punct::Minus) {
            return None;
        }
        let (ra, rb) = (self.reg_token(a)?, self.reg_token(b)?);
        Some((
            Arg {
                kind: ArgKind::Reg(ra),
                span: a.span,
            },
            Arg {
                kind: ArgKind::Reg(rb),
                span: b.span,
            },
        ))
    }

    fn operand(&mut self, toks: &[Token], whole: Span) -> Option<Arg> {
        let Some(first) = toks.first() else {
            self.cx.error(whole, "missing operand");
            return None;
        };
        let span = first.span.to(toks[toks.len() - 1].span);

        if first.is_punct(Punct::LBrace) {
            let mask = self.list(toks, span)?;
            return Some(Arg {
                kind: ArgKind::List(mask),
                span,
            });
        }

        if first.is_punct(Punct::LBracket) {
            let mut cur = Cursor::new(toks);
            let base = self.bracket(&mut cur)?;
            self.expect_end(&cur)?;
            return Some(Arg {
                kind: ArgKind::Bracket(base),
                span,
            });
        }

        if let Some(r) = self.reg_token(first) {
            if toks.len() > 1 {
                self.cx.error(
                    toks[1].span.to(span),
                    format!(
                        "unexpected `{}` after register `{}`",
                        describe_token(self.cx, &toks[1]),
                        reg::gpr_name(r)
                    ),
                );
                return None;
            }
            return Some(Arg {
                kind: ArgKind::Reg(r),
                span,
            });
        }

        let mut cur = Cursor::new(toks);
        let imm = self.immediate(&mut cur)?;
        if cur.check_punct(Punct::LBracket) {
            let base = self.bracket(&mut cur)?;
            self.expect_end(&cur)?;
            return Some(Arg {
                kind: ArgKind::Mem { disp: imm, base },
                span,
            });
        }
        self.expect_end(&cur)?;
        Some(Arg {
            kind: ArgKind::Imm(imm),
            span,
        })
    }

    fn expect_end(&mut self, cur: &Cursor<'_>) -> Option<()> {
        if cur.at_end() {
            return Some(());
        }
        let t = cur.peek();
        let text = describe_token(self.cx, &t);
        self.cx.error(
            t.span.to(cur.remaining_span()),
            format!("unexpected `{text}` after operand"),
        );
        None
    }

    /// `[reg]`.
    fn bracket(&mut self, cur: &mut Cursor<'_>) -> Option<u8> {
        let open = cur.advance();
        let tok = cur.peek();
        let Some(r) = self.reg_token(&tok) else {
            self.cx
                .error(open.span.to(tok.span), "expected a register inside `[...]`");
            return None;
        };
        cur.advance();
        if cur.eat_punct(Punct::RBracket).is_none() {
            self.cx.error(
                cur.peek().span,
                format!("expected `]` after `[{}`", reg::gpr_name(r)),
            );
            return None;
        }
        Some(r)
    }

    /// `{r20-r29, r31}`: returns bit `n` set for each `rn` named.
    ///
    /// Only r20-r31 can be saved by `prepare`, so anything else is refused
    /// here rather than silently dropped by the encoder.
    fn list(&mut self, toks: &[Token], span: Span) -> Option<u32> {
        let close = toks.iter().position(|t| t.is_punct(Punct::RBrace));
        let Some(close) = close else {
            self.cx
                .error(span, "register list is missing its closing `}`");
            return None;
        };
        if close + 1 != toks.len() {
            self.cx.error(
                toks[close + 1].span.to(span),
                "unexpected tokens after the register list",
            );
            return None;
        }
        let inner = &toks[1..close];
        let mut mask = 0u32;
        let mut i = 0;
        while i < inner.len() {
            let t = inner[i];
            let Some(lo) = self.reg_token(&t) else {
                self.cx
                    .error(t.span, "expected a register in the register list");
                return None;
            };
            let mut hi = lo;
            let mut end_span = t.span;
            if inner.get(i + 1).is_some_and(|d| d.is_punct(Punct::Minus)) {
                let Some(t2) = inner.get(i + 2) else {
                    self.cx
                        .error(inner[i + 1].span, "expected a register after `-`");
                    return None;
                };
                let Some(r2) = self.reg_token(t2) else {
                    self.cx.error(t2.span, "expected a register after `-`");
                    return None;
                };
                if r2 < lo {
                    self.cx.error(
                        t.span.to(t2.span),
                        format!(
                            "register range `r{lo}-r{r2}` runs backwards; write the lower register first"
                        ),
                    );
                    return None;
                }
                hi = r2;
                end_span = t2.span;
                i += 2;
            }
            if lo < 20 {
                self.cx.error(
                    t.span.to(end_span),
                    format!(
                        "`{}` cannot be in a register list; only r20-r31 are saved and restored",
                        reg::gpr_name(lo)
                    ),
                );
                return None;
            }
            for r in lo..=hi {
                mask |= 1 << r;
            }
            i += 1;
            match inner.get(i) {
                None => {}
                Some(c) if c.is_punct(Punct::Comma) => {
                    i += 1;
                    if i == inner.len() {
                        self.cx.error(c.span, "expected a register after `,`");
                        return None;
                    }
                }
                Some(other) => {
                    self.cx
                        .error(other.span, "expected `,` or `}` in the register list");
                    return None;
                }
            }
        }
        Some(mask)
    }

    /// An expression, optionally behind a relocation function.
    fn immediate(&mut self, cur: &mut Cursor<'_>) -> Option<Imm> {
        let first = cur.peek();
        let ccrh = self.cx.dialect == crate::lexer::Dialect::CcRh;
        let mut func = if ccrh {
            self.ccrh_reference(cur)?
        } else {
            RelFn::None
        };
        if func == RelFn::None
            && let Some(n) = first.ident()
            && cur.nth(1).is_punct(Punct::LParen)
            && let Some(f) = RelFn::from_name(self.name(n))
        {
            func = f;
            cur.advance();
        }
        let expr_start = cur.pos();
        let mut expr = {
            let mut p = self.cx.expr_parser();
            p.parse(cur)?
        };
        // CC-RH's `HIGHW1(x)`, `LOWW(x)` and `HIGHW(x)` of a value the
        // assembler cannot fold are GNU as's `hi(x)`, `lo(x)` and `hi0(x)`:
        // the manual describes the same three halves (CC-RH §5.8 (2)(i),
        // page 499), and on RH850 each has its relocation.
        if ccrh
            && func == RelFn::None
            && self.cx.constant(expr).is_none()
            && let crate::expr::ExprKind::Unary(op, inner) = self.cx.exprs.get(expr).kind
        {
            use crate::expr::UnOp;
            let f = match op {
                UnOp::HighW1 => Some(RelFn::Hi),
                UnOp::LowW => Some(RelFn::Lo),
                UnOp::HighW => Some(RelFn::Hi0),
                _ => None,
            };
            if let Some(f) = f {
                func = f;
                expr = inner;
            }
        }
        let consumed = &cur.all()[expr_start..cur.pos()];
        // The expression parser would read `r1` as a symbol. GNU as refuses
        // registers in expressions, and so does rsasm, rather than emit a
        // reference to a symbol nobody defined.
        if let Some((t, r)) = consumed
            .iter()
            .find_map(|t| self.reg_token(t).map(|r| (t, r)))
        {
            self.cx.error(
                t.span,
                format!(
                    "register `{}` cannot be used in an expression",
                    reg::gpr_name(r)
                ),
            );
            return None;
        }
        let ident = match (func, consumed) {
            (RelFn::None, [t]) => t.ident(),
            _ => None,
        };
        let last = cur.all()[..cur.pos()]
            .last()
            .map(|t| t.span)
            .unwrap_or(first.span);
        Some(Imm {
            expr,
            func,
            ident,
            span: first.span.to(last),
        })
    }

    /// A CC-RH label reference sigil at the start of an operand (CC-RH §5.8
    /// (2)(f), Table 5.21, pages 493-494), as the relocation function GNU as
    /// spells the same reference with:
    ///
    /// - `#label`, the 32-bit absolute address, is `hilo(label)`, which the
    ///   instruction expansions split into `hi()` and `lo()` where needed.
    /// - `!label`, the address as a 16-bit value, is `zdaoff(label)`: an
    ///   offset from address 0.
    /// - `$label` and `%label` are offsets from `gp` and `ep`, for which the
    ///   RH850 ELF ABI GNU binutils implements has no relocation; they are
    ///   refused.
    ///
    /// `!` is also CC-RH's bitwise NOT, so it is only a reference in front of
    /// a name that is not already a constant. Returns `None` after reporting
    /// an error.
    fn ccrh_reference(&mut self, cur: &mut Cursor<'_>) -> Option<RelFn> {
        let tok = cur.peek();
        let next = cur.nth(1);
        if tok.is_punct(Punct::Hash) {
            cur.advance();
            return Some(RelFn::HiLo);
        }
        if tok.is_punct(Punct::Bang)
            && let Some(n) = next.ident()
            && reg::gpr(self.name(n)).is_none()
            && !self
                .cx
                .symbols
                .lookup(n)
                .is_some_and(|id| match self.cx.symbols.get(id).value {
                    crate::symbol::SymbolValue::Expr(e) => self.cx.constant(e).is_some(),
                    _ => false,
                })
        {
            cur.advance();
            return Some(RelFn::ZdaOff);
        }
        if (tok.is_punct(Punct::Dollar) || tok.is_punct(Punct::Percent)) && next.ident().is_some() {
            let (sigil, base) = if tok.is_punct(Punct::Dollar) {
                ("$", "gp")
            } else {
                ("%", "ep")
            };
            self.cx.error(
                tok.span.to(next.span),
                format!(
                    "`{sigil}label` is an offset from `{base}`, and the RH850 ELF ABI has no \
                     relocation for it; address the data with `#label` or `!label` instead"
                ),
            );
            return None;
        }
        Some(RelFn::None)
    }
}

fn describe_token(cx: &AsmCtx<'_>, t: &Token) -> String {
    match t.kind {
        TokKind::Ident(n) => cx.name(n).to_string(),
        TokKind::Int(v) => v.to_string(),
        TokKind::Punct(p) => p.as_str().to_string(),
        TokKind::Str(_) => "string".to_string(),
        TokKind::LocalRef(n, _) => n.to_string(),
        TokKind::BadNumber(n) => cx.name(n).to_string(),
        TokKind::Eof | TokKind::Eol => "end of line".to_string(),
    }
}
