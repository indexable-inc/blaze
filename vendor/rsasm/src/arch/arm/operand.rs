//! ARM operand parsing, shared by the A32 and T32 encoders.
//!
//! Two things make ARM operands awkward for a comma-splitting parser. A
//! shifted register is written as *two* comma-separated pieces that belong to
//! one operand (`add r0, r1, r2, lsl #3`), and a post-indexed address puts the
//! offset after the closing bracket (`ldr r0, [r1], #4`). Both are handled by
//! parsing the operand list as a whole with a little lookahead, rather than
//! splitting on commas first the way the x86 backend can.
//!
//! Note on `#`: GAS-dialect lexing treats `#` as a line comment, so in this
//! assembler an ARM immediate is written bare (`add r0, r1, 1`). A `#` is
//! accepted where the lexer ever delivers one, so sources keep working if the
//! core learns the ARM comment character.

use super::reg::{self, Reg};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind};
use crate::source::Span;

/// Which bank a vector register comes from: 32 single-precision `s`
/// registers, 32 double-precision `d` registers over the same bytes, and 16
/// quadword `q` registers over pairs of those. Not API.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum VecKind {
    S,
    D,
    Q,
}

#[doc(hidden)]
impl VecKind {
    pub fn letter(self) -> char {
        match self {
            VecKind::S => 's',
            VecKind::D => 'd',
            VecKind::Q => 'q',
        }
    }
}

/// One vector register, and the lane of it an operand may name. Not API.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct VecReg {
    pub kind: VecKind,
    pub n: u8,
    /// `d0[1]`: which element of the register, which only a scalar operand
    /// or a structure transfer takes.
    pub lane: Option<u32>,
    /// `d0[]`: every element of it at once, which a structure load writes
    /// one element over.
    pub all: bool,
}

/// The vector register a name spells, if it is one. Not API.
#[doc(hidden)]
pub fn vec_register(name: &str) -> Option<(VecKind, u8)> {
    let (kind, rest) = match name.as_bytes().first()? {
        b's' => (VecKind::S, &name[1..]),
        b'd' => (VecKind::D, &name[1..]),
        b'q' => (VecKind::Q, &name[1..]),
        _ => return None,
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u8 = rest.parse().ok()?;
    let last = if kind == VecKind::Q { 15 } else { 31 };
    (n <= last).then_some((kind, n))
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
    /// Rotate right through carry: a one-bit shift with no amount.
    Rrx,
}

impl Shift {
    pub fn from_name(name: &str) -> Option<Shift> {
        Some(match name {
            "lsl" | "asl" => Shift::Lsl,
            "lsr" => Shift::Lsr,
            "asr" => Shift::Asr,
            "ror" => Shift::Ror,
            "rrx" => Shift::Rrx,
            _ => return None,
        })
    }

    /// The two-bit shift-type field. `rrx` shares `ror`'s encoding, with a
    /// zero amount.
    pub fn code(self) -> u32 {
        match self {
            Shift::Lsl => 0,
            Shift::Lsr => 1,
            Shift::Asr => 2,
            Shift::Ror | Shift::Rrx => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shift::Lsl => "lsl",
            Shift::Lsr => "lsr",
            Shift::Asr => "asr",
            Shift::Ror => "ror",
            Shift::Rrx => "rrx",
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ShiftAmt {
    Imm(u32),
    Reg(Reg),
    /// `rrx`, which encodes as `ror` by zero.
    None,
}

/// How a load or store updates its base register.
// The names are the manual's, `pre-indexed` and `post-indexed`.
#[allow(clippy::enum_variant_names)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Index {
    /// `[rn, off]`: the base is unchanged.
    Offset,
    /// `[rn, off]!`
    PreIndex,
    /// `[rn], off`
    PostIndex,
}

#[derive(Copy, Clone, Debug)]
pub enum MemOffset {
    None,
    /// A signed byte count; the sign becomes the U bit.
    Imm(i64),
    /// `[rn], {imm}`: the unindexed form of `ldc` and `stc`, whose byte the
    /// coprocessor reads and the core does not.
    Unindexed(i64),
    Reg {
        rm: Reg,
        add: bool,
        shift: Shift,
        amount: u32,
        /// Whether a shift was written at all. `[r0, r1, lsl #0]` shifts by
        /// nothing and still has no 16-bit Thumb form.
        shifted: bool,
    },
}

#[derive(Copy, Clone, Debug)]
pub struct Mem {
    pub base: Reg,
    pub offset: MemOffset,
    pub index: Index,
    /// `[r0:64]`: the alignment a NEON structure transfer may promise, in
    /// bits. Not API.
    #[doc(hidden)]
    pub align: Option<u32>,
    pub span: Span,
}

/// Which half of a 32-bit value a `movw`/`movt` operand asks for.
///
/// A whole address takes two instructions to build, and the pair writes the
/// halves of one value, so each half needs a relocation that says which it is
/// rather than a number the assembler could compute.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Half {
    /// `:lower16:`, bits 0 to 15.
    Lower,
    /// `:upper16:`, bits 16 to 31.
    Upper,
}

impl Half {
    pub fn name(self) -> &'static str {
        match self {
            Half::Lower => ":lower16:",
            Half::Upper => ":upper16:",
        }
    }
}

#[derive(Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    /// A register with a barrel shift applied.
    Shifted {
        rm: Reg,
        shift: Shift,
        amount: ShiftAmt,
    },
    /// An immediate or a branch target; which one depends on the instruction.
    Imm(ExprRef),
    Mem(Mem),
    /// `{r0-r3, lr}`, as a bitmask of registers. `user` is the `^` that
    /// makes an A32 block transfer read the user-mode register bank, or,
    /// with the PC in the list, restore the saved status register.
    List {
        mask: u16,
        user: bool,
    },
    /// `=expr`: a value for `ldr` to load from the literal pool.
    Literal(ExprRef),
    /// `#:lower16:expr` or `#:upper16:expr`: one half of a 32-bit value, which
    /// only `movw` and `movt` take.
    Half(Half, ExprRef),
    /// `{expr}`: `nop`'s hint number and the coprocessor opcode of `cdp`.
    Braced(ExprRef),
    /// A VFP or NEON register, perhaps with a lane index.
    Vec(VecReg),
    /// `{d0-d3}`, `{s0, s1}` or `{d0[2], d1[2]}`: a run of vector registers,
    /// held as the first of them and how many there are. `spaced` is the
    /// `{d0, d2}` the structure transfers take, whose registers step by two,
    /// and `all` the `{d0[]}` one of them writes an element over.
    VecList {
        kind: VecKind,
        first: u8,
        count: u8,
        lane: Option<u32>,
        spaced: bool,
        all: bool,
        /// `{d5-d5}`, a range of one register: only a structure transfer
        /// reads that.
        degenerate: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
    /// Set when the operand was a single bare identifier, so instructions with
    /// keyword operands (`dmb sy`, `mrs r0, cpsr`) can read it without
    /// re-parsing. Such an operand is *also* available as an expression, since
    /// only the instruction knows which reading is meant.
    pub word: Option<String>,
    /// True when the register was followed by `!` (writeback on `ldm`/`stm`).
    pub writeback: bool,
}

impl Operand {
    /// The vector register this operand is, if it is one. Not API.
    #[doc(hidden)]
    pub fn vec(&self) -> Option<VecReg> {
        match self.kind {
            OperandKind::Vec(v) => Some(v),
            _ => None,
        }
    }

    pub fn reg(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) => Some(r),
            _ => None,
        }
    }

    pub fn imm(&self) -> Option<ExprRef> {
        match self.kind {
            OperandKind::Imm(e) => Some(e),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match &self.kind {
            OperandKind::Reg(r) => format!("register `{}`", reg::name_of(*r)),
            OperandKind::Shifted { .. } => "a shifted register".into(),
            OperandKind::Imm(_) => "an immediate".into(),
            OperandKind::Mem(_) => "a memory operand".into(),
            OperandKind::List { .. } => "a register list".into(),
            OperandKind::Literal(_) => "a literal pool value".into(),
            OperandKind::Half(h, _) => format!("a `{}` value", h.name()),
            OperandKind::Braced(_) => "a value in braces".into(),
            OperandKind::Vec(v) => format!("register `{}{}`", v.kind.letter(), v.n),
            OperandKind::VecList { .. } => "a vector register list".into(),
        }
    }
}

pub struct Parser<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
    /// The `sym(NAME)` relocation suffixes this instruction's operands may
    /// carry. GNU as reads one only for the operand type its branches use, so
    /// `bl sym(PLT)` is a branch target and `ldr r0, =sym(GOT)` is the syntax
    /// error `parse_big_immediate` gives.
    pub suffixes: &'static [&'static str],
}

impl Parser<'_, '_> {
    /// Parses the whole operand list of one instruction.
    pub fn parse_list(&mut self, cur: &mut Cursor<'_>) -> Option<Vec<Operand>> {
        let mut out = Vec::new();
        if cur.at_end() {
            return Some(out);
        }
        loop {
            let op = self.parse_one(cur)?;
            out.push(op);
            // A shift keyword after a comma continues the previous operand
            // rather than starting a new one.
            while cur.check_punct(Punct::Comma) && self.peek_shift(cur, 1).is_some() {
                cur.advance();
                let last = out.len() - 1;
                out[last] = self.apply_shift(cur, out[last].clone())?;
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        if !cur.at_end() {
            let span = cur.peek().span;
            self.cx.error(span, "unexpected token after operand");
            return None;
        }
        Some(out)
    }

    /// The shift keyword `n` tokens ahead, if there is one.
    fn peek_shift(&self, cur: &Cursor<'_>, n: usize) -> Option<Shift> {
        match cur.nth(n).kind {
            TokKind::Ident(name) => {
                Shift::from_name(&self.cx.interner.get(name).to_ascii_lowercase())
            }
            _ => None,
        }
    }

    /// Turns `base` into a shifted register, having consumed the comma.
    fn apply_shift(&mut self, cur: &mut Cursor<'_>, base: Operand) -> Option<Operand> {
        let start = cur.peek().span;
        let OperandKind::Reg(rm) = base.kind else {
            self.cx
                .error(start, "a shift can only be applied to a register");
            return None;
        };
        let Some(shift) = self.peek_shift(cur, 0) else {
            self.cx.error(start, "expected a shift");
            return None;
        };
        cur.advance();
        let amount = if shift == Shift::Rrx {
            ShiftAmt::None
        } else if let Some(r) = self.eat_register(cur) {
            ShiftAmt::Reg(r)
        } else {
            cur.eat_punct(Punct::Hash);
            let e = self.cx.expr_parser().parse(cur)?;
            let Some(v) = self.cx.constant(e) else {
                self.cx
                    .error(start, "a shift amount must be a constant expression");
                return None;
            };
            // `lsr #32` and `asr #32` are real shifts, encoded as zero; `lsl`
            // and `ror` stop at 31.
            let max = match shift {
                Shift::Lsr | Shift::Asr => 32,
                _ => 31,
            };
            if v < 0 || v > max {
                self.cx.error(
                    start,
                    format!(
                        "shift amount {v} is out of range for `{}` (0 to {max})",
                        shift.name()
                    ),
                );
                return None;
            }
            ShiftAmt::Imm(v as u32)
        };
        let span = base.span.to(cur.nth(0).span);
        Some(Operand {
            kind: OperandKind::Shifted { rm, shift, amount },
            span,
            word: None,
            writeback: false,
        })
    }

    /// A vector register and the lane it may name, if that is what comes
    /// next. `None` means it was not one; an error inside the lane index is
    /// reported and returns `Some(None)`'s outer `None`.
    fn eat_vec_register(&mut self, cur: &mut Cursor<'_>) -> Option<Option<VecReg>> {
        let TokKind::Ident(name) = cur.peek().kind else {
            return Some(None);
        };
        let Some((kind, n)) = vec_register(&self.cx.interner.get(name).to_ascii_lowercase()) else {
            return Some(None);
        };
        cur.advance();
        let mut lane = None;
        if cur.check_punct(Punct::LBracket) {
            let span = cur.advance().span;
            // `d0[]` is every lane of the register at once, which is how a
            // structure load spells the element it copies over them.
            if cur.eat_punct(Punct::RBracket).is_some() {
                return Some(Some(VecReg {
                    kind,
                    n,
                    lane: None,
                    all: true,
                }));
            }
            let e = self.cx.expr_parser().parse(cur)?;
            let Some(v) = self.cx.constant(e) else {
                self.cx
                    .error(span, "a lane index must be a constant expression");
                return None;
            };
            if !(0..16).contains(&v) {
                self.cx
                    .error(span, format!("lane {v} is out of range (0 to 15)"));
                return None;
            }
            if cur.eat_punct(Punct::RBracket).is_none() {
                let span = cur.peek().span;
                self.cx.error(span, "expected `]` after a lane index");
                return None;
            }
            lane = Some(v as u32);
        }
        Some(Some(VecReg {
            kind,
            n,
            lane,
            all: false,
        }))
    }

    fn eat_register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let TokKind::Ident(name) = cur.peek().kind else {
            return None;
        };
        let r = reg::lookup(&self.cx.interner.get(name).to_ascii_lowercase())?;
        cur.advance();
        Some(r)
    }

    fn parse_one(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;
        if cur.check_punct(Punct::LBrace) {
            return self.parse_reglist(cur);
        }
        if cur.eat_punct(Punct::Eq).is_some() {
            cur.eat_punct(Punct::Hash);
            let e = self.cx.expr_parser().parse(cur)?;
            return Some(Operand {
                kind: OperandKind::Literal(e),
                span: start.to(cur.nth(0).span),
                word: None,
                writeback: false,
            });
        }
        if cur.check_punct(Punct::LBracket) {
            return self.parse_mem(cur);
        }
        if let Some(v) = self.eat_vec_register(cur)? {
            let writeback = cur.eat_punct(Punct::Bang).is_some();
            return Some(Operand {
                kind: OperandKind::Vec(v),
                span: start.to(cur.nth(0).span),
                word: None,
                writeback,
            });
        }
        if let Some(r) = self.eat_register(cur) {
            let writeback = cur.eat_punct(Punct::Bang).is_some();
            return Some(Operand {
                kind: OperandKind::Reg(r),
                span: start.to(cur.nth(0).span),
                word: None,
                writeback,
            });
        }
        // Everything else is an expression: an immediate, a branch target, or
        // a keyword operand such as a barrier option.
        let word = match cur.peek().kind {
            TokKind::Ident(n) if cur.nth(1).is_eol() || cur.nth(1).is_punct(Punct::Comma) => {
                Some(self.cx.interner.get(n).to_ascii_lowercase())
            }
            _ => None,
        };
        cur.eat_punct(Punct::Hash);
        if let Some(half) = self.eat_half(cur)? {
            let e = self.expr(cur)?;
            return Some(Operand {
                kind: OperandKind::Half(half, e),
                span: start.to(cur.nth(0).span),
                word: None,
                writeback: false,
            });
        }
        let e = self.expr(cur)?;
        Some(Operand {
            kind: OperandKind::Imm(e),
            span: start.to(cur.nth(0).span),
            word,
            writeback: false,
        })
    }

    /// An expression, with whatever relocation suffixes this instruction
    /// allows after a symbol in it.
    fn expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut p = self.cx.expr_parser();
        p.paren_modifiers = self.suffixes;
        let e = p.parse(cur)?;
        // `parse_reloc` reads the suffix as the end of the operand, so
        // `bl 4 + sym(tlscall)` is a branch and `bl sym(PLT) + 4` is the
        // "garbage following instruction" GNU as calls it.
        if let Some(m) = self.modifier_node(e).filter(|_| !self.suffixes.is_empty()) {
            let (whole, suffix) = (self.cx.exprs.span(e), self.cx.exprs.span(m));
            if suffix.hi != whole.hi {
                self.cx.error(
                    Span::new(suffix.hi, whole.hi),
                    "nothing may follow a relocation suffix",
                );
                return None;
            }
        }
        Some(e)
    }

    /// The relocation suffix in `e`, if there is one.
    fn modifier_node(&self, e: ExprRef) -> Option<ExprRef> {
        use crate::expr::ExprKind::*;
        match &self.cx.exprs.get(e).kind {
            Modifier(..) => Some(e),
            Unary(_, a) => self.modifier_node(*a),
            Binary(_, a, b) => self.modifier_node(*a).or_else(|| self.modifier_node(*b)),
            _ => None,
        }
    }

    /// `:lower16:` or `:upper16:` in front of an operand, if one is there.
    /// `Some(None)` means there was none; an error inside one returns `None`.
    fn eat_half(&mut self, cur: &mut Cursor<'_>) -> Option<Option<Half>> {
        if !cur.check_punct(Punct::Colon) {
            return Some(None);
        }
        let colon = cur.advance();
        let name = cur.peek();
        let text = match name.kind {
            TokKind::Ident(n) => self.cx.interner.get(n).to_ascii_lowercase(),
            _ => String::new(),
        };
        let half = match text.as_str() {
            "lower16" => Half::Lower,
            "upper16" => Half::Upper,
            _ => {
                self.cx.error(
                    colon.span.to(name.span),
                    "expected `:lower16:` or `:upper16:`",
                );
                return None;
            }
        };
        cur.advance();
        if cur.eat_punct(Punct::Colon).is_none() {
            let span = cur.peek().span;
            self.cx
                .error(span, format!("expected `:` to close `{}`", half.name()));
            return None;
        }
        Some(Some(half))
    }

    fn parse_reglist(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.advance().span; // `{`
        let mut mask = 0u16;
        if cur.eat_punct(Punct::RBrace).is_some() {
            self.cx.error(start, "empty register list");
            return None;
        }
        if let Some(v) = self.eat_vec_register(cur)? {
            return self.parse_vec_list(cur, start, v);
        }
        // `{5}` is not a register list at all: it is the hint number of
        // `nop` and the coprocessor option of `cdp` and `ldc`.
        if !matches!(cur.peek().kind, TokKind::Ident(n)
            if reg::is_register(&self.cx.interner.get(n).to_ascii_lowercase()))
        {
            let e = self.cx.expr_parser().parse(cur)?;
            if cur.eat_punct(Punct::RBrace).is_none() {
                let span = cur.peek().span;
                self.cx.error(span, "expected `}`");
                return None;
            }
            return Some(Operand {
                kind: OperandKind::Braced(e),
                span: start.to(cur.nth(0).span),
                word: None,
                writeback: false,
            });
        }
        loop {
            let span = cur.peek().span;
            let Some(lo) = self.eat_register(cur) else {
                self.cx.error(span, "expected a register in the list");
                return None;
            };
            let mut ranged = false;
            let hi = if cur.eat_punct(Punct::Minus).is_some() {
                let span = cur.peek().span;
                ranged = true;
                match self.eat_register(cur) {
                    Some(r) => r,
                    None => {
                        self.cx.error(span, "expected a register after `-`");
                        return None;
                    }
                }
            } else {
                lo
            };
            if ranged && hi == lo {
                self.cx.error(span, "bad range in register list");
                return None;
            }
            if hi < lo {
                self.cx.error(
                    span,
                    format!(
                        "register range `{}-{}` runs backwards",
                        reg::name_of(lo),
                        reg::name_of(hi)
                    ),
                );
                return None;
            }
            for r in lo..=hi {
                mask |= 1 << r;
            }
            if cur.eat_punct(Punct::Comma).is_some() {
                continue;
            }
            if cur.eat_punct(Punct::RBrace).is_some() {
                break;
            }
            let span = cur.peek().span;
            self.cx
                .error(span, "expected `,` or `}` in a register list");
            return None;
        }
        // `^` after the list asks for the user-mode bank, or, with the PC
        // in it, an exception return.
        let user = cur.eat_punct(Punct::Caret).is_some();
        Some(Operand {
            kind: OperandKind::List { mask, user },
            span: start.to(cur.nth(0).span),
            word: None,
            writeback: false,
        })
    }

    /// The rest of `{d0-d3}`, `{s0, s1}` or `{d0[2], d2[2]}`, having read
    /// the first register. A list is a run, written either as a range or as
    /// each register in turn, and the structure transfers also take one
    /// whose registers step by two.
    fn parse_vec_list(
        &mut self,
        cur: &mut Cursor<'_>,
        start: Span,
        first: VecReg,
    ) -> Option<Operand> {
        let mut last = first;
        let mut count = 1u8;
        let mut step = 1u8;
        let mut degenerate = false;
        if cur.eat_punct(Punct::Minus).is_some() {
            let span = cur.peek().span;
            let Some(hi) = self.eat_vec_register(cur)? else {
                self.cx.error(span, "expected a register after `-`");
                return None;
            };
            if hi.kind != first.kind || hi.n < first.n {
                self.cx
                    .error(span, "a register range runs from low to high");
                return None;
            }
            // `{d5-d5}` is a range of one, which GNU as reads in a structure
            // transfer's list and nowhere else.
            degenerate = hi.n == first.n;
            count = hi.n - first.n + 1;
            last = hi;
        }
        while cur.eat_punct(Punct::Comma).is_some() {
            let span = cur.peek().span;
            let Some(next) = self.eat_vec_register(cur)? else {
                self.cx.error(span, "expected a register in the list");
                return None;
            };
            if next.kind != first.kind || next.lane != first.lane || next.all != first.all {
                self.cx
                    .error(span, "a vector register list holds one kind of register");
                return None;
            }
            if count == 1 && next.n == last.n + 2 {
                step = 2;
            }
            if next.n != last.n + step {
                self.cx
                    .error(span, "a vector register list is a run of registers");
                return None;
            }
            last = next;
            count += 1;
        }
        if cur.eat_punct(Punct::RBrace).is_none() {
            let span = cur.peek().span;
            self.cx
                .error(span, "expected `,` or `}` in a register list");
            return None;
        }
        Some(Operand {
            kind: OperandKind::VecList {
                kind: first.kind,
                first: first.n,
                count,
                lane: first.lane,
                spaced: step == 2,
                all: first.all,
                degenerate,
            },
            span: start.to(cur.nth(0).span),
            word: None,
            writeback: false,
        })
    }

    fn parse_mem(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.advance().span; // `[`
        let span = cur.peek().span;
        let Some(base) = self.eat_register(cur) else {
            self.cx.error(span, "expected a base register");
            return None;
        };

        // `[r0:64]`, `[r0 :64]` and `[r0, :64]` all promise an alignment,
        // which only the NEON structure transfers take.
        let mut align = None;
        let mut offset = MemOffset::None;
        let colon = cur.check_punct(Punct::Colon)
            || (cur.check_punct(Punct::Comma) && cur.nth(1).kind == TokKind::Punct(Punct::Colon));
        if colon {
            cur.eat_punct(Punct::Comma);
            let span = cur.advance().span;
            let e = self.cx.expr_parser().parse(cur)?;
            let Some(v) = self.cx.constant(e) else {
                self.cx
                    .error(span, "an alignment must be a constant expression");
                return None;
            };
            align = Some(v.clamp(0, u32::MAX.into()) as u32);
        } else if cur.eat_punct(Punct::Comma).is_some() {
            offset = self.parse_mem_offset(cur)?;
        }
        if cur.eat_punct(Punct::RBracket).is_none() {
            let span = cur.peek().span;
            self.cx.error(span, "expected `]`");
            return None;
        }

        let index = if cur.eat_punct(Punct::Bang).is_some() {
            Index::PreIndex
        } else if matches!(offset, MemOffset::None) && cur.check_punct(Punct::Comma) {
            // `[rn], off`: the offset written after the bracket is applied
            // after the transfer. A memory operand is always last, so there is
            // nothing else this comma could introduce.
            cur.advance();
            if cur.check_punct(Punct::LBrace) {
                // `[rn], {8}`: `ldc`'s unindexed form, where the byte is the
                // coprocessor's and the base is not changed.
                let span = cur.peek().span;
                let braced = self.parse_reglist(cur)?;
                let OperandKind::Braced(e) = braced.kind else {
                    self.cx.error(span, "expected a value in braces");
                    return None;
                };
                let Some(v) = self.cx.constant(e) else {
                    self.cx
                        .error(span, "this option must be a constant expression");
                    return None;
                };
                offset = MemOffset::Unindexed(v);
                Index::Offset
            } else {
                offset = self.parse_mem_offset(cur)?;
                Index::PostIndex
            }
        } else {
            Index::Offset
        };

        Some(Operand {
            kind: OperandKind::Mem(Mem {
                base,
                offset,
                index,
                align,
                span: start.to(cur.nth(0).span),
            }),
            span: start.to(cur.nth(0).span),
            word: None,
            writeback: false,
        })
    }

    fn parse_mem_offset(&mut self, cur: &mut Cursor<'_>) -> Option<MemOffset> {
        let start = cur.peek().span;
        // A sign directly in front of a register is the U bit, not arithmetic.
        let mut add = true;
        let save = cur.pos();
        if cur.eat_punct(Punct::Minus).is_some() {
            add = false;
        } else {
            cur.eat_punct(Punct::Plus);
        }
        if let Some(rm) = self.eat_register(cur) {
            let mut shift = Shift::Lsl;
            let mut amount = 0u32;
            let mut shifted = false;
            if cur.check_punct(Punct::Comma) && self.peek_shift(cur, 1).is_some() {
                shifted = true;
                cur.advance();
                let s = self.peek_shift(cur, 0)?;
                cur.advance();
                shift = s;
                if s != Shift::Rrx {
                    cur.eat_punct(Punct::Hash);
                    let e = self.cx.expr_parser().parse(cur)?;
                    let Some(v) = self.cx.constant(e) else {
                        self.cx
                            .error(start, "a shift amount must be a constant expression");
                        return None;
                    };
                    let max = match s {
                        Shift::Lsr | Shift::Asr => 32,
                        _ => 31,
                    };
                    if v < 0 || v > max {
                        self.cx.error(
                            start,
                            format!("shift amount {v} is out of range (0 to {max})"),
                        );
                        return None;
                    }
                    amount = v as u32;
                }
            }
            return Some(MemOffset::Reg {
                rm,
                add,
                shift,
                amount,
                shifted,
            });
        }
        // Not a register after all; re-read the whole thing as an expression
        // so that `[r0, -4]` keeps its sign.
        cur.set_pos(save);
        cur.eat_punct(Punct::Hash);
        let e = self.cx.expr_parser().parse(cur)?;
        let Some(v) = self.cx.constant(e) else {
            self.cx
                .error(start, "a memory offset must be a constant expression");
            return None;
        };
        Some(MemOffset::Imm(v))
    }
}
