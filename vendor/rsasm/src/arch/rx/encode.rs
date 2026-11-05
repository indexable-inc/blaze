//! Instruction assembly: bit fields, displacements, and immediates whose
//! width depends on their value.
//!
//! RX instructions are a few base bytes with packed bit fields, followed by
//! displacement and immediate bytes. [`Enc`] builds exactly that, with the
//! field numbering GNU as uses (bit 0 is the most significant bit of the first
//! byte), so each encoding reads like the line of `rx-parse.y` it was checked
//! against.
//!
//! # Size classes
//!
//! Most of RX's variable length comes from two places:
//!
//! - **Displacements** are 0, 8 or 16 bits, chosen from the value, and are
//!   stored *divided by the operand size*: `mov.l 8[r1], r2` stores 2.
//! - **Immediates** are 8, 16, 24 or 32 bits, with a two-bit length code
//!   (`li`) in the opcode, where `00` means 32. Several instructions also have
//!   a shorter form for small unsigned values (`mov #uimm4, r1`, `cmp #uimm8,
//!   r1`), picked before the general one.
//!
//! GNU as decides all of this while it parses, from whether the operand is a
//! constant *at that moment*. That is not the same thing as whether it is a
//! constant eventually, and matching the reference means following it:
//!
//! - a constant takes the shortest form that fits. So does a difference of
//!   two labels GNU as's expression parser has already folded, which it does
//!   when nothing between them can change size ([`AsmCtx::fixed_distance`]);
//! - any other difference of labels is an immediate GNU as relaxes itself,
//!   among the general form's 8/16/24/32-bit fields and never the short
//!   forms, re-picking on every pass as it does for branches
//!   ([`FixupKind::relax_difference`]). As a displacement it is always 16
//!   bits, and stored undivided;
//! - anything else referring to a symbol is a 32-bit immediate with a
//!   relocation, even if the symbol turns out to be nearby, and cannot be a
//!   displacement at all.
//!
//! Because folding depends on what lies between two labels, which
//! instructions GNU as can resize matters beyond their own encoding. Those
//! are the ones with a relaxed branch or immediate, and — whatever its
//! value — every one whose grammar rule reads a displacement, even an absent
//! one; the backend reports them through [`AsmCtx::relaxable`].

use super::reg::Size;
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::{BinOp, ExprKind, ExprRef, SymbolEnv, UnOp};
use crate::intern::Name;
use crate::section::{Fixup, FixupKind, SectionId, Variant};
use crate::source::Span;
use crate::symbol::{SymbolId, SymbolValue};

/// An instruction under construction.
#[derive(Clone, Debug)]
pub struct Enc {
    pub bytes: Vec<u8>,
    pub fixups: Vec<Fixup>,
}

impl Enc {
    pub fn new(base: &[u8]) -> Enc {
        Enc {
            bytes: base.to_vec(),
            fixups: Vec::new(),
        }
    }

    /// ORs `val` into the `sz`-bit field starting `pos` bits from the most
    /// significant bit of the first byte. Bits of `val` above `sz` are
    /// dropped; callers range-check first.
    pub fn field(&mut self, val: u32, pos: u32, sz: u32) -> &mut Enc {
        for i in 0..sz {
            if (val >> (sz - 1 - i)) & 1 == 1 {
                let p = pos + i;
                self.bytes[(p / 8) as usize] |= 0x80 >> (p % 8);
            }
        }
        self
    }

    /// Appends `n` little-endian bytes of `v`. Operand bytes are always
    /// little-endian on RX, whatever the data byte order.
    pub fn push(&mut self, v: i64, n: usize) -> &mut Enc {
        for i in 0..n {
            self.bytes.push((v >> (8 * i)) as u8);
        }
        self
    }

    /// Appends `kind.size` placeholder bytes to be filled from `expr`.
    pub fn fixup(&mut self, expr: ExprRef, kind: FixupKind, span: Span) -> &mut Enc {
        self.fixup_at(self.bytes.len(), expr, kind, span);
        self.bytes
            .extend(std::iter::repeat_n(0, kind.size as usize));
        self
    }

    /// Records a fixup over bytes that already exist.
    pub fn fixup_at(&mut self, offset: usize, expr: ExprRef, kind: FixupKind, span: Span) {
        self.fixups.push(Fixup {
            offset: offset as u32,
            expr,
            kind,
            span,
        });
    }

    pub fn variant(self) -> Variant {
        Variant {
            bytes: self.bytes,
            fixups: self.fixups,
        }
    }

    pub fn one(self) -> Option<Vec<Variant>> {
        Some(vec![self.variant()])
    }
}

/// Folds `e` to a number, or reports `what` must be a constant.
pub fn constant(cx: &mut AsmCtx<'_>, e: ExprRef, span: Span, what: &str) -> Option<i64> {
    match known(cx, e) {
        Some(v) => Some(v),
        None => {
            cx.error(span, format!("{what} must be a constant"));
            None
        }
    }
}

/// A constant in `lo..=hi`, or a diagnostic naming that range.
pub fn constant_in(
    cx: &mut AsmCtx<'_>,
    e: ExprRef,
    span: Span,
    what: &str,
    lo: i64,
    hi: i64,
) -> Option<i64> {
    let v = constant(cx, e, span, what)?;
    if v < lo || v > hi {
        cx.error(span, format!("{what} {v} is out of range ({lo} to {hi})"));
        return None;
    }
    Some(v)
}

/// A field GNU as fills in itself once the value is known, `n` bytes wide.
///
/// Its overflow check in `fixup_segment` reads the field as unsigned but
/// lets the value's negation pass too, so a byte takes anything from -255 to
/// 255: `int #s-e` with `e` 200 bytes past `s` stores `38`.
fn gnu_field(n: u8) -> FixupKind {
    let max = (1i64 << (8 * n as u32)) - 1;
    FixupKind::data(n)
        .signed()
        .with_field(8 * n + 1, 1)
        .with_limits(-max, max)
}

// ---- displacements --------------------------------------------------------

/// Appends a displacement for an operand of size `size`, writing its length
/// code (0: none, 1: 8-bit, 2: 16-bit) into the two-bit field at `pos`.
///
/// A zero displacement, written or not, takes no bytes. Otherwise the stored
/// value is the displacement divided by the operand size, which is why a
/// `.l` operand reaches 262140 bytes and must be a multiple of 4. A
/// difference of labels GNU as has not folded is the exception: its
/// `displacement` gives that 16 bits and stores it as it is, undivided.
/// Checked: `mov.l (e-s)[r1], r2` with `e - s` a forward 200 is `ee 12 c8
/// 00`.
///
/// This is GNU as's `DSP`, which makes the instruction relaxable whether or
/// not a displacement was written, so the grammar rules for a plain `[reg]`
/// that read none must not call it.
pub fn disp(
    cx: &mut AsmCtx<'_>,
    enc: &mut Enc,
    pos: u32,
    disp: Option<ExprRef>,
    size: Size,
    span: Span,
) -> Option<()> {
    cx.relaxable = true;
    let Some(e) = disp else {
        return Some(());
    };
    let v = match classify(cx, e) {
        Val::Const(v) => v,
        Val::Relax(e) => {
            enc.field(2, pos, 2).fixup(e, gnu_field(2), span);
            return Some(());
        }
        Val::Sym(_) => {
            cx.error(
                span,
                "displacements must be constants or differences of labels; GNU as reads \
                 them before any label is placed",
            );
            return None;
        }
    };
    if v == 0 {
        return Some(());
    }
    if v < 0 {
        cx.error(
            span,
            format!("displacement {v} is negative; RX displacements are unsigned"),
        );
        return None;
    }
    let scale = size.scale();
    if v % scale != 0 {
        cx.error(
            span,
            format!(
                "displacement {v} is not a multiple of {scale}, the size of a `{}` operand",
                [".b", ".w", ".l"][size as usize]
            ),
        );
        return None;
    }
    let units = v / scale;
    if units <= 0xff {
        enc.field(1, pos, 2).push(units, 1);
    } else if units <= 0xffff {
        enc.field(2, pos, 2).push(units, 2);
    } else {
        cx.error(
            span,
            format!(
                "displacement {v} is too large (the limit for this operand size is {})",
                0xffff * scale
            ),
        );
        return None;
    }
    Some(())
}

/// The five-bit scaled displacement of the short `mov`/`movu` forms, if `disp`
/// has one: a written constant from 0 up to 31 units, aligned to the operand
/// size. A missing displacement does not count, because GNU as's grammar
/// sends `[reg]` to the long form.
pub fn disp5(cx: &AsmCtx<'_>, disp: Option<ExprRef>, size: Size) -> Option<u32> {
    let v = known(cx, disp?)?;
    let scale = size.scale();
    (v >= 0 && v % scale == 0 && v / scale <= 31).then_some((v / scale) as u32)
}

// ---- operand values -------------------------------------------------------

/// What is known about an operand when the instruction is read.
#[derive(Copy, Clone, Debug)]
pub enum Val {
    /// A constant, including a difference of labels GNU as has folded.
    Const(i64),
    /// A difference of labels GNU as leaves to be resolved later.
    Relax(ExprRef),
    /// Anything else: a relocation.
    Sym(ExprRef),
}

/// Classifies an operand the way GNU as's parser would see it.
///
/// GNU as's expression parser turns `a - b` into a constant when `a` and `b`
/// are the same symbol, or labels already defined in one section with
/// nothing between them that can change size. Any other `a - b`, give or
/// take added constants, stays a difference (`O_subtract`) for its RX port
/// to relax or resolve later.
///
/// A symbol set to such a difference (`len = . - msg`) was folded the same
/// way where it was set, and is a plain constant from then on; if it was not
/// foldable there, it is a symbol of its own, never a difference.
pub fn classify(cx: &AsmCtx<'_>, e: ExprRef) -> Val {
    classify_as_of(cx, e, Reading::default())
}

/// Where an expression is being read: in the statement being assembled, or
/// in the definition of a symbol it refers to.
#[derive(Copy, Clone, Default)]
struct Reading {
    /// Symbol definitions followed to get here.
    depth: u32,
    /// The [`Symbol::def_order`](crate::symbol::Symbol::def_order) of that
    /// definition: only what was defined before it counts as defined.
    before: Option<u32>,
}

impl Reading {
    fn sees(self, cx: &AsmCtx<'_>, id: SymbolId) -> bool {
        self.before.is_none_or(|b| cx.symbols.get(id).def_order < b)
    }
}

fn classify_as_of(cx: &AsmCtx<'_>, e: ExprRef, at: Reading) -> Val {
    if let Some(c) = cx.constant(e) {
        return Val::Const(c);
    }
    let mut d = Difference::default();
    if !d.collect(cx, e, false, at) {
        return Val::Sym(e);
    }
    let (plus, minus) = match (d.plus, d.minus) {
        (Some(plus), Some(minus)) => (plus, minus),
        // Symbols set to constants, and nothing else.
        (None, None) => return Val::Const(d.addend),
        _ => return Val::Sym(e),
    };
    if plus == minus {
        return Val::Const(d.addend);
    }
    if let (Some(to), Some(from)) = (plus.position(cx, at), minus.position(cx, at))
        && let Some(n) = cx.fixed_distance(from, to)
    {
        return Val::Const(n.wrapping_add(d.addend));
    }
    Val::Relax(e)
}

/// The value of `e`, if GNU as has a constant there when reading it.
pub fn known(cx: &AsmCtx<'_>, e: ExprRef) -> Option<i64> {
    match classify(cx, e) {
        Val::Const(v) => Some(v),
        _ => None,
    }
}

/// A symbol in a difference. A name seen for the first time is not in the
/// symbol table yet, and `.` is not bound to its label until the statement
/// has been assembled.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Term {
    Id(SymbolId),
    Name(Name),
    Here,
}

impl Term {
    fn position(self, cx: &AsmCtx<'_>, at: Reading) -> Option<(SectionId, u32)> {
        match self {
            Term::Id(id) if at.sees(cx, id) => cx.label_position(id),
            Term::Id(_) | Term::Name(_) => None,
            Term::Here => Some(cx.here()),
        }
    }
}

/// `plus - minus + addend`, read off an expression's syntax.
#[derive(Default)]
struct Difference {
    plus: Option<Term>,
    minus: Option<Term>,
    addend: i64,
}

impl Difference {
    /// Adds `e`, negated if `neg`. Returns false if the sum is not one GNU as
    /// keeps as a single difference: more than one symbol on a side, or a
    /// symbol negated on its own (`-b + a`), which GNU as makes an expression
    /// symbol of.
    fn collect(&mut self, cx: &AsmCtx<'_>, e: ExprRef, neg: bool, at: Reading) -> bool {
        let kind = &cx.exprs.get(e).kind;
        let id = match kind {
            ExprKind::Sym(name) => cx.symbols.lookup(*name),
            ExprKind::SymId(id) => Some(*id),
            _ => None,
        };
        let constant = match id {
            Some(id) => folded_symbol(cx, id, at),
            None => cx.constant(e),
        };
        if let Some(c) = constant {
            self.addend = if neg {
                self.addend.wrapping_sub(c)
            } else {
                self.addend.wrapping_add(c)
            };
            return true;
        }
        let term = match (kind, id) {
            (_, Some(id)) => Term::Id(id),
            (ExprKind::Sym(name), None) => Term::Name(*name),
            (ExprKind::Here, _) => Term::Here,
            (ExprKind::Unary(UnOp::Plus, x), _) => return self.collect(cx, *x, neg, at),
            (ExprKind::Binary(BinOp::Add, l, r), _) => {
                return self.collect(cx, *l, neg, at) && self.collect(cx, *r, neg, at);
            }
            (ExprKind::Binary(BinOp::Sub, l, r), _) => {
                return self.collect(cx, *l, neg, at) && self.collect(cx, *r, !neg, at);
            }
            _ => return false,
        };
        let slot = if neg { &mut self.minus } else { &mut self.plus };
        slot.replace(term).is_none()
    }
}

/// The value of symbol `id`, if it is set to something GNU as folded to a
/// constant where it was set — so it must have been set by then too.
fn folded_symbol(cx: &AsmCtx<'_>, id: SymbolId, at: Reading) -> Option<i64> {
    let SymbolValue::Expr(x) = cx.symbols.get(id).value else {
        return None;
    };
    // As deep as `SymbolEnv` follows definitions before calling them
    // circular.
    if !at.sees(cx, id) || at.depth >= 64 {
        return None;
    }
    let inner = Reading {
        depth: at.depth + 1,
        before: Some(cx.symbols.get(id).def_order),
    };
    match classify_as_of(cx, x, inner) {
        Val::Const(c) => Some(c),
        _ => None,
    }
}

// ---- immediates -----------------------------------------------------------

/// The values a constant field accepts.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Range {
    /// 0..=255 for a byte.
    Unsigned,
    /// Either reading: -128..=255 for a byte.
    Either,
}

impl Range {
    fn bounds(self, bits: u32) -> (i64, i64) {
        let half = 1i64 << (bits - 1);
        match self {
            Range::Unsigned => (0, 2 * half - 1),
            Range::Either => (-half, 2 * half - 1),
        }
    }
}

/// Where an immediate goes in one candidate encoding.
#[derive(Copy, Clone, Debug)]
pub enum Place {
    /// An unsigned four-bit field at bit `pos` of the opcode.
    Nibble(u32),
    /// `n` trailing bytes, relocated with `reloc` when symbolic.
    Bytes { n: u8, range: Range, reloc: u32 },
    /// GNU as's `IMM`: one to four trailing bytes, with the length code at bit
    /// `li` of the opcode. `bits` is the operand width, 8, 16 or 32, which
    /// limits the value and decides how a constant is sign-folded: `mov.b
    /// #255` stores one byte, `ff`, as does `mov #0xffffffff`.
    Imm { li: u32, bits: u32 },
}

/// One candidate encoding of an instruction with an immediate.
#[derive(Clone, Debug)]
pub struct Rung {
    pub enc: Enc,
    pub place: Place,
    /// The value is stored negated: `sub #n` is `add #-n`.
    pub negate: bool,
    /// GNU as uses this form for a non-constant immediate.
    pub symbolic: bool,
    /// GNU as never relaxes this form's immediate below 32 bits.
    pub wide_when_symbolic: bool,
}

impl Rung {
    pub fn new(enc: Enc, place: Place) -> Rung {
        Rung {
            enc,
            place,
            negate: false,
            symbolic: !matches!(place, Place::Nibble(_)),
            wide_when_symbolic: false,
        }
    }

    /// Marks an immediate that follows a constant displacement.
    ///
    /// GNU as records the displacement as a relaxation too, and its relaxation
    /// pass then looks for the immediate's expression in the slot a
    /// displacement fixup would have taken, finds nothing it can evaluate,
    /// and settles on 32 bits. Checked: `mov.w #e-s, 4[r2]` with `e - s` a
    /// forward 5 is `f9 21 02 05 00 00 00`.
    pub fn after_displacement(mut self) -> Rung {
        self.wide_when_symbolic = true;
        self
    }

    /// A form GNU as only picks for a constant: the short immediate forms.
    pub fn const_only(mut self) -> Rung {
        self.symbolic = false;
        self
    }

    pub fn negated(mut self) -> Rung {
        self.negate = true;
        self
    }
}

/// The byte length an `IMM` needs for `v`, after GNU as's sign fold.
fn imm_len(v: i64, bits: u32) -> Option<u8> {
    let (lo, hi) = Range::Either.bounds(bits);
    if v < lo || v > hi {
        return None;
    }
    // A value in the upper half of the operand width is its negative
    // reading: `mov.w #0xffff` is `mov.w #-1`.
    let half = 1i64 << (bits - 1);
    let v = if v >= half { v - 2 * half } else { v };
    Some(match v {
        -0x80..=0x7f => 1,
        -0x8000..=0x7fff => 2,
        -0x80_0000..=0x7f_ffff => 3,
        _ => 4,
    })
}

/// The `li` code for an `n`-byte immediate: 32 bits is 0.
fn li_code(n: u8) -> u32 {
    (n % 4) as u32
}

/// Assembles an instruction whose candidate encodings are `rungs`, in the
/// order GNU as tries them, for the immediate `e`.
pub fn immediate(
    cx: &mut AsmCtx<'_>,
    rungs: Vec<Rung>,
    e: ExprRef,
    span: Span,
) -> Option<Vec<Variant>> {
    match classify(cx, e) {
        Val::Const(c) => const_immediate(cx, rungs, c, span).map(|v| vec![v]),
        Val::Relax(e) => symbolic(cx, rungs, e, span, true),
        Val::Sym(e) => symbolic(cx, rungs, e, span, false),
    }
}

/// Picks the first rung a constant fits, or explains the widest one's limit.
pub fn const_immediate(
    cx: &mut AsmCtx<'_>,
    rungs: Vec<Rung>,
    c: i64,
    span: Span,
) -> Option<Variant> {
    let mut limit = (0, 0);
    for Rung {
        mut enc,
        place,
        negate,
        ..
    } in rungs
    {
        let v = if negate { c.wrapping_neg() } else { c };
        match place {
            Place::Nibble(pos) => {
                if (0..=15).contains(&v) {
                    enc.field(v as u32, pos, 4);
                    return Some(enc.variant());
                }
                limit = (0, 15);
            }
            Place::Bytes { n, range, .. } => {
                let (lo, hi) = range.bounds(8 * n as u32);
                if (lo..=hi).contains(&v) {
                    enc.push(v, n as usize);
                    return Some(enc.variant());
                }
                limit = (lo, hi);
            }
            Place::Imm { li, bits } => {
                if let Some(n) = imm_len(v, bits) {
                    enc.field(li_code(n), li, 2).push(v, n as usize);
                    return Some(enc.variant());
                }
                limit = Range::Either.bounds(bits);
                if negate {
                    limit = (-limit.1, -limit.0);
                }
            }
        }
    }
    cx.error(
        span,
        format!("immediate {c} is out of range ({} to {})", limit.0, limit.1),
    );
    None
}

fn negate4(_word: u64, v: i64) -> u64 {
    (v.wrapping_neg() as u64) & 0xffff_ffff
}

/// The relocation GNU as gives an `n`-byte immediate.
fn imm_reloc(n: u8) -> u32 {
    match n {
        1 => reloc::DIR8S,
        2 => reloc::DIR16,
        3 => reloc::DIR24S,
        _ => reloc::DIR32,
    }
}

/// The forms GNU as uses for a non-constant immediate. `relax` is true for a
/// difference, which it sizes by value; any other symbol gets 32 bits.
fn symbolic(
    cx: &mut AsmCtx<'_>,
    rungs: Vec<Rung>,
    e: ExprRef,
    span: Span,
    relax: bool,
) -> Option<Vec<Variant>> {
    let mut rungs: Vec<Rung> = rungs.into_iter().filter(|r| r.symbolic).collect();
    let Some(last) = rungs.pop() else {
        cx.error(span, "this immediate must be a constant");
        return None;
    };
    let mut out = Vec::new();
    match last.place {
        Place::Imm { li, .. } if last.negate => {
            // The stored value is `-e`: always 32 bits in GNU as, which does
            // not relax it. What resolves in the file, a difference or a
            // symbol set to a constant later, is negated when written; a
            // label or an external symbol would need GNU as's stack-machine
            // relocation (`R_RX_SYM`, `R_RX_OPneg`, `R_RX_ABS32`), which one
            // fixup cannot express. That is refused here where it is already
            // certain, and by layout otherwise.
            let relocated = SymbolEnv::new(cx.exprs, cx.symbols)
                .value(e)
                .is_some_and(|v| {
                    v.plus.is_some_and(|p| cx.symbols.get(p).is_defined()) && v.minus.is_none()
                });
            if relocated {
                cx.error(
                    span,
                    "a symbolic immediate here would be stored negated, which needs a \
                     relocation expression rsasm cannot emit",
                );
                return None;
            }
            let mut enc = last.enc;
            enc.field(0, li, 2);
            enc.fixup(e, gnu_field(4).scatter(negate4), span);
            out.push(enc.variant());
        }
        Place::Imm { li, .. } => {
            // GNU as relaxes every symbolic `IMM`, though it only ever finds
            // a value for a difference; anything else ends up 32 bits. It
            // starts each at one byte all the same, and the pass that grows
            // one to four moves everything after it, so a one-byte candidate
            // that never fits stands in for that first estimate.
            cx.relaxable = true;
            let sized = relax && !last.wide_when_symbolic;
            let ladder: &[u8] = if sized { &[1, 2, 3, 4] } else { &[1, 4] };
            for &n in ladder {
                let mut enc = last.enc.clone();
                enc.field(li_code(n), li, 2);
                let mut kind = gnu_field(n)
                    .with_reloc(imm_reloc(n))
                    .relaxed_as_difference();
                if !sized && n < 4 {
                    kind = kind.with_limits(1, 0);
                }
                enc.fixup(e, kind, span);
                out.push(enc.variant());
            }
        }
        Place::Bytes { n, reloc, .. } => {
            let mut enc = last.enc;
            enc.fixup(e, gnu_field(n).with_reloc(reloc), span);
            out.push(enc.variant());
        }
        Place::Nibble(_) => {
            cx.error(span, "this immediate must be a constant");
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_count_from_the_most_significant_bit() {
        let mut e = Enc::new(&[0, 0]);
        e.field(0xb, 4, 4);
        assert_eq!(e.bytes, [0x0b, 0]);
        // A field that straddles two bytes.
        let mut e = Enc::new(&[0, 0]);
        e.field(0b101, 7, 3);
        assert_eq!(e.bytes, [0x01, 0x40]);
    }

    #[test]
    fn immediates_are_sign_folded_to_their_operand_width() {
        assert_eq!(imm_len(0xffff_ffff, 32), Some(1));
        assert_eq!(imm_len(0x80, 32), Some(2));
        assert_eq!(imm_len(-129, 32), Some(2));
        assert_eq!(imm_len(0xff, 8), Some(1));
        assert_eq!(imm_len(0x100, 8), None);
        assert_eq!(imm_len(0x8000, 16), Some(2));
        assert_eq!(imm_len(0x1_0000_0000, 32), None);
    }

    #[test]
    fn resolved_fields_take_what_gnu_as_lets_through() {
        let byte = gnu_field(1);
        assert!(byte.fits(255) && byte.fits(-255));
        assert!(!byte.fits(256) && !byte.fits(-256));
        let long = gnu_field(4);
        assert!(long.fits(0xffff_ffff) && long.fits(-0xffff_ffff));
    }
}
