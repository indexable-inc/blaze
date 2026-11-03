//! The table-driven half of the backend: SIMD, floating point, SVE, and the
//! general-purpose groups built the same way.
//!
//! These instruction sets are thousands of forms that differ in a handful of
//! opcode bits and in which lane arrangement or element size each register
//! takes. Writing them out family by family would be writing out the ARM ARM;
//! instead `tools/tables/aarch64.py` asks llvm-mc. It disassembles random
//! words to find every form llvm-mc prints, assembles each one with a single
//! operand changed at a time to measure where that operand's bits go, and
//! writes the result to [`table_data`]: for each form, the operands it takes
//! and the opcode left when all of them are zero.
//!
//! The general-purpose groups here are the ones shaped the same way: the
//! load/store exclusives, the acquire/release accesses and the atomics, one
//! form per operation, ordering and access size, the
//! pointer-authentication instructions that name a register, memory
//! tagging, the CRC32 checksums, and the unprivileged and unscaled
//! acquire-release loads and stores, one form per access size and
//! signedness. The rest of the general-purpose instruction
//! set, whose interest is in its aliases and in the one-off forms that
//! constrain an operand in a way no row here could say, is written out in
//! [`super::insn`].
//!
//! This module is the other half: an operand grammar covering what those
//! forms are written with (`v0.4s`, `d3`, `v1.s[2]`, `z0.d`, `p1/z`,
//! `{ v0.16b, v1.16b }`, `[x0, #1, mul vl]`, `#1.5`, `pow2`), and a matcher
//! that walks a mnemonic's forms, first to last, until one takes the
//! operands. Where two forms take the same text — `mov z0.s, #imm` is `dup`
//! for a small number and `dupm` for a mask — the generator orders them so
//! the first match is the one llvm-mc makes.

use super::encode::{logical_imm, word};
use super::reg::{self, RegClass};
use super::table_data::{FEATS, FORMS, MNEMONICS, SHAPES, SLOTS};
use super::table_names::{PATTERNS, PREFETCHES};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::lexer::{Punct, TokKind, Token};
use crate::section::Variant;
use crate::source::Span;

// ---- the vocabulary the generated table is written in ----------------------

/// Lane arrangements, as `v0.<arrangement>` spells them.
pub const A_8B: u8 = 0;
pub const A_16B: u8 = 1;
pub const A_4H: u8 = 2;
pub const A_8H: u8 = 3;
pub const A_2S: u8 = 4;
pub const A_4S: u8 = 5;
pub const A_1D: u8 = 6;
pub const A_2D: u8 = 7;
pub const A_1Q: u8 = 8;
/// In the grammar, but no form in the table takes it today.
#[allow(dead_code)]
pub const A_2Q: u8 = 9;
pub const A_4B: u8 = 10;
pub const A_2H: u8 = 11;
pub const A_2B: u8 = 12;
const ARRANGEMENTS: [&str; 13] = [
    "8b", "16b", "4h", "8h", "2s", "4s", "1d", "2d", "1q", "2q", "4b", "2h", "2b",
];

/// Element sizes, as a `.b`/`.h`/`.s`/`.d`/`.q` suffix or a scalar register
/// letter spells them. `E_NONE` is an SVE register written without one.
pub const E_B: u8 = 0;
pub const E_H: u8 = 1;
pub const E_S: u8 = 2;
pub const E_D: u8 = 3;
pub const E_Q: u8 = 4;
pub const E_NONE: u8 = 5;
const ELEMS: [&str; 6] = ["b", "h", "s", "d", "q", ""];

/// General-purpose register widths, and how register 31 is spelled in a
/// field: the zero register, or the stack pointer.
pub const G_W: u8 = 0;
pub const G_X: u8 = 1;
pub const S_ZR: u8 = 0;
pub const S_SP: u8 = 1;
pub const S_ANY: u8 = 2;

/// How an SVE governing predicate is written: `p0`, `p0/m` or `p0/z`.
pub const P_PLAIN: u8 = 0;
pub const P_MERGE: u8 = 1;
pub const P_ZERO: u8 = 2;

/// A shift written as an operand of its own: `lsl #8`, `msl #16`, `mul #4`.
pub const SH_LSL: u8 = 0;
pub const SH_MSL: u8 = 1;
pub const SH_MUL: u8 = 2;
const SHIFTS: [&str; 3] = ["lsl", "msl", "mul"];

/// The extend or shift applied to an index inside an address.
pub const X_UXTW: u8 = 0;
pub const X_SXTW: u8 = 1;
/// In the grammar, but no form in the table takes it today.
#[allow(dead_code)]
pub const X_UXTX: u8 = 2;
/// In the grammar, but no form in the table takes it today.
#[allow(dead_code)]
pub const X_SXTX: u8 = 3;
pub const X_LSL: u8 = 4;
const EXTENDS: [&str; 5] = ["uxtw", "sxtw", "uxtx", "sxtx", "lsl"];

/// What an operand has to look like to fill a slot.
#[derive(Copy, Clone, Debug)]
pub enum Kind {
    /// `v0.16b`, with this arrangement.
    Vec(u8),
    /// `v0.s[1]`, one element of this size.
    VecIdx(u8),
    /// `v0.4b[1]`: an arrangement with a lane index, as the dot-product
    /// indexed forms write their third operand.
    VecIdxArr(u8),
    /// `b0` … `q0`.
    Scalar(u8),
    /// `w0`/`x0`, and how register 31 is spelled.
    Gpr(u8, u8),
    /// `z0.s`, or `z0` for `E_NONE`.
    Z(u8),
    /// `z0.s[1]`.
    ZIdx(u8),
    /// `p0.b`, `p0/m` or `p0/z`.
    P(u8, u8),
    /// `{ v0.16b, v1.16b }`: this many registers of this arrangement.
    VecList(u8, u8),
    /// `{ v0.b, v1.b }[3]`.
    VecListIdx(u8, u8),
    /// `{ z0.d, z1.d }`.
    ZList(u8, u8),
    /// `{ p0.d, p1.d }`.
    PList(u8, u8),
    Imm,
    /// A floating-point immediate, `#1.5`.
    FImm,
    /// A condition name.
    Cond,
    /// An SVE element-count pattern: `pow2`, `vl8`, `all`.
    Pat,
    /// An SVE prefetch operation: `pldl1keep`.
    Prf,
    Shift(u8),
    /// `mul vl`, after an SVE offset.
    MulVl,
    /// `[`, `]` and `]!` of an address: the last is the pre-index writeback
    /// of `stlr w0, [x1, #-4]!`, whose offset is the one value the form
    /// takes.
    Open,
    Close,
    CloseWb,
    /// An index extend with no amount: `uxtw`.
    Ext(u8),
    /// An index extend or shift with one: `sxtw #2`, `lsl #3`.
    ExtAmt(u8),
}

/// A function of an immediate the word holds in place of the immediate.
#[derive(Copy, Clone, Debug)]
pub enum Xf {
    /// The 8-bit floating-point immediate.
    FpImm,
    /// The bitmask immediate, for elements of this many bits.
    LogImm(u8),
    /// The bitmask immediate of the complement, as `bic`, `orn` and `eon`
    /// write theirs.
    NotLogImm(u8),
    /// A mask of whole bytes, one bit per byte: `movi d0, #0xff00ff00…`.
    ByteMask,
}

/// How a number in an operand reaches the instruction word. Every encoding
/// clears the bits it owns before setting them, so the opcode's contents
/// there do not matter, except for `Affine`, which counts from it.
#[derive(Copy, Clone, Debug)]
pub enum Enc {
    None,
    /// The form takes this value and no other.
    Fixed(i64),
    /// The value itself, in one run of bits.
    Field {
        lsb: u8,
        width: u8,
        min: i64,
        max: i64,
    },
    /// The value itself, bit `.0` to word bit `.1`; one value bit may go to
    /// several word bits. The lowest bit placed says what the value counts
    /// in: `ldraa x0, [x1, #8]` reaches word bits 12-20 and 22 from its own
    /// bits 3-12, so its offset is a multiple of eight.
    Scatter {
        min: i64,
        max: i64,
        bits: &'static [(u8, u8)],
    },
    /// A run of bits that moves by `sign` each `step` of the value, starting
    /// from what the opcode holds for `min`: right shifts count down.
    Affine {
        lsb: u8,
        width: u8,
        sign: i8,
        step: i64,
        min: i64,
        max: i64,
    },
    Xform(Xf, u8, u8),
    XformBits(Xf, &'static [(u8, u8)]),
    /// Must equal the value of an earlier number in the same form, and
    /// encodes nothing of its own: `add z0.b, p0/m, z0.b, z1.b`.
    Tied(u8),
    /// Must be the register after an earlier one, and encodes nothing of its
    /// own: `casp x0, x1, x2, x3, [x4]` names each pair by its even
    /// register, and the odd one has to follow it.
    TiedNext(u8),
    /// A few values, each with a code: `#90`/`#270`.
    Choice {
        lsb: u8,
        width: u8,
        map: &'static [(i64, u8)],
    },
    FChoice {
        lsb: u8,
        width: u8,
        map: &'static [(f64, u8)],
    },
}

/// One operand of a form: what it looks like, and where its one or two
/// numbers go. `b` is used only by the indexed kinds, for the index.
#[derive(Copy, Clone, Debug)]
pub struct Slot {
    pub kind: Kind,
    pub a: Enc,
    pub b: Enc,
}

/// A form: its mnemonic and operand shape, as indices, its opcode, the
/// element width its signed immediates wrap at, or 0, and which ways: bit 0
/// for a number above the range, bit 1 for one below it.
///
/// Both references read a number of an SVE element's width as the element's
/// bits, so `mov z0.h, #0xfff0` is `mov z0.h, #-16` and `mov z0.b, #-241` is
/// `mov z0.b, #15`; the generator sets the width and the ways for each form
/// llvm-mc was seen to do that for.
///
/// The last field is which instruction-set features the target needs for the
/// form, as an index into [`super::table_data::FEATS`]; see [`super::cpu`].
#[derive(Copy, Clone, Debug)]
pub struct Form(pub u16, pub u16, pub u32, pub u8, pub u8, pub u16);

// ---- operands -----------------------------------------------------------------

/// One parsed operand, or one piece of an address. The numbers are, in
/// order, a register, then what qualifies it: an arrangement or element
/// size, a count, a lane index.
#[derive(Clone, Debug)]
enum Atom {
    /// `v0.16b`.
    Vec(u8, u8),
    /// `v0.s[1]`.
    VecIdx(u8, u8, i64),
    /// `v0.4b[1]`.
    VecIdxArr(u8, u8, i64),
    /// `b0` … `q0`.
    Scalar(u8, u8),
    /// `w0`/`x0`, and whether 31 was spelled as the stack pointer.
    Gpr(u8, u8, bool),
    /// `z0`, or `z0.s`.
    Z(u8, u8),
    /// `z0.s[1]`.
    ZIdx(u8, u8, i64),
    /// `p0`, `p0.b`, `p0/m` or `p0/z`: number, element size, mode.
    P(u8, u8, u8),
    /// `{ v0.16b, v1.16b }`: first register, count, arrangement.
    VecList(u8, u8, u8),
    /// `{ v0.b, v1.b }[3]`, with the index last.
    VecListIdx(u8, u8, u8, i64),
    /// `{ z0.d, z1.d }`.
    ZList(u8, u8, u8),
    /// `{ p0.d, p1.d }`.
    PList(u8, u8, u8),
    Imm(i64),
    /// A number with a fraction, `#1.5`.
    Float(f64),
    /// A bare name: a condition, an SVE pattern, a prefetch operation.
    Word(String),
    /// `lsl #8`, `msl #16`, `mul #4`: which, and how much.
    Shift(u8, i64),
    /// `mul vl`, after an SVE offset.
    MulVl,
    /// `[`, `]` and `]!` of an address.
    Open,
    Close,
    CloseWb,
    /// An index extend with no amount, `uxtw`.
    Ext(u8),
    /// An index extend or shift with one, `sxtw #2`.
    ExtAmt(u8, i64),
}

impl Atom {
    fn describe(&self) -> String {
        match self {
            Atom::Vec(n, a) => format!("v{n}.{}", ARRANGEMENTS[*a as usize]),
            Atom::VecIdx(n, e, i) => format!("v{n}.{}[{i}]", ELEMS[*e as usize]),
            Atom::VecIdxArr(n, a, i) => format!("v{n}.{}[{i}]", ARRANGEMENTS[*a as usize]),
            Atom::Scalar(n, e) => format!("{}{n}", ELEMS[*e as usize]),
            Atom::Gpr(n, c, sp) => {
                let x = *c == G_X;
                match (*n == 31, *sp, x) {
                    (true, true, true) => "sp".into(),
                    (true, true, false) => "wsp".into(),
                    (true, false, true) => "xzr".into(),
                    (true, false, false) => "wzr".into(),
                    _ => format!("{}{n}", if x { 'x' } else { 'w' }),
                }
            }
            Atom::Z(n, e) => z_name('z', *n, *e),
            Atom::ZIdx(n, e, i) => format!("{}[{i}]", z_name('z', *n, *e)),
            Atom::P(n, e, m) => {
                let mode = ["", "/m", "/z"][*m as usize];
                format!("{}{mode}", z_name('p', *n, *e))
            }
            Atom::VecList(_, c, a) => format!("a list of {c} `.{}`", ARRANGEMENTS[*a as usize]),
            Atom::VecListIdx(_, c, e, _) => {
                format!("a list of {c} indexed `.{}`", ELEMS[*e as usize])
            }
            Atom::ZList(_, c, e) | Atom::PList(_, c, e) => {
                format!("a list of {c} `.{}`", ELEMS[*e as usize])
            }
            Atom::Imm(v) => format!("#{v}"),
            Atom::Float(v) => format!("#{v}"),
            Atom::Word(w) => format!("`{w}`"),
            Atom::Shift(s, v) => format!("{} #{v}", SHIFTS[*s as usize]),
            Atom::MulVl => "mul vl".into(),
            Atom::Open => "[".into(),
            Atom::Close => "]".into(),
            Atom::CloseWb => "]!".into(),
            Atom::Ext(x) => EXTENDS[*x as usize].into(),
            Atom::ExtAmt(x, v) => format!("{} #{v}", EXTENDS[*x as usize]),
        }
    }
}

fn z_name(letter: char, n: u8, e: u8) -> String {
    if e == E_NONE {
        format!("{letter}{n}")
    } else {
        format!("{letter}{n}.{}", ELEMS[e as usize])
    }
}

/// A register number, `0`..`31` with no leading zero.
fn reg_number(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() == 2 && s.starts_with('0') {
        return None;
    }
    let n: u8 = s.parse().ok()?;
    (n <= 31).then_some(n)
}

fn elem_code(s: &str) -> Option<u8> {
    ELEMS[..5].iter().position(|e| *e == s).map(|i| i as u8)
}

fn arrangement_code(s: &str) -> Option<u8> {
    ARRANGEMENTS.iter().position(|a| *a == s).map(|i| i as u8)
}

/// A register name with no index: every kind of register this grammar knows.
fn register(name: &str) -> Option<Atom> {
    if let Some(r) = reg::lookup(name) {
        return Some(match r.class {
            RegClass::W => Atom::Gpr(r.num, G_W, r.sp),
            RegClass::X => Atom::Gpr(r.num, G_X, r.sp),
            RegClass::B => Atom::Scalar(r.num, E_B),
            RegClass::H => Atom::Scalar(r.num, E_H),
            RegClass::S => Atom::Scalar(r.num, E_S),
            RegClass::D => Atom::Scalar(r.num, E_D),
            RegClass::Q => Atom::Scalar(r.num, E_Q),
        });
    }
    let (letter, rest) = name.split_at_checked(1)?;
    let (num, suffix) = match rest.split_once('.') {
        Some((n, s)) => (n, Some(s)),
        None => (rest, None),
    };
    let n = reg_number(num)?;
    match (letter, suffix) {
        ("v", Some(s)) => Some(Atom::Vec(n, arrangement_code(s)?)),
        ("z", None) => Some(Atom::Z(n, E_NONE)),
        ("z", Some(s)) => Some(Atom::Z(n, elem_code(s)?)),
        ("p", None) if n < 16 => Some(Atom::P(n, E_NONE, P_PLAIN)),
        ("p", Some(s)) if n < 16 => Some(Atom::P(n, elem_code(s)?, P_PLAIN)),
        _ => None,
    }
}

/// True for a register name only this grammar knows: vector, SVE and
/// predicate registers.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    matches!(
        register(name),
        Some(Atom::Vec(..) | Atom::Z(..) | Atom::P(..))
    )
}

fn ident(cx: &AsmCtx<'_>, t: &Token) -> Option<String> {
    match t.kind {
        TokKind::Ident(n) => Some(cx.name(n).to_ascii_lowercase()),
        _ => None,
    }
}

fn span_of(toks: &[Token]) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => Span::DUMMY,
    }
}

/// Parses an operand list into atoms, with an address's pieces between its
/// `Open` and `Close`.
fn parse(cx: &mut AsmCtx<'_>, toks: &[Token]) -> Option<Vec<(Atom, Span)>> {
    let cur = Cursor::new(toks);
    let mut out = Vec::new();
    if cur.at_end() {
        return Some(out);
    }
    for piece in cur.split_commas() {
        if piece.is_empty() {
            cx.error(cur.remaining_span(), "empty operand");
            return None;
        }
        parse_operand(cx, piece, &mut out)?;
    }
    Some(out)
}

fn parse_operand(cx: &mut AsmCtx<'_>, toks: &[Token], out: &mut Vec<(Atom, Span)>) -> Option<()> {
    let span = span_of(toks);
    let first = toks[0];
    if first.is_punct(Punct::LBracket) {
        return parse_address(cx, toks, out);
    }
    if first.is_punct(Punct::LBrace) {
        let atom = parse_list(cx, toks)?;
        out.push((atom, span));
        return Some(());
    }
    if let Some(word) = ident(cx, &first) {
        // `p0/m`, `p0/z`.
        if toks.len() == 3
            && toks[1].is_punct(Punct::Slash)
            && let Some(Atom::P(n, e, _)) = register(&word)
            && let Some(mode) = ident(cx, &toks[2])
        {
            let mode = match mode.as_str() {
                "m" => P_MERGE,
                "z" => P_ZERO,
                _ => {
                    cx.error(span, "a predicate qualifier is `/m` or `/z`");
                    return None;
                }
            };
            out.push((Atom::P(n, e, mode), span));
            return Some(());
        }
        // `v0.s[1]`, `v0.4b[1]`, `z0.d[3]`, and the lookup-table forms that
        // index a register with no element size at all, `z0[7]`.
        if toks.len() == 4 && toks[1].is_punct(Punct::LBracket) && toks[3].is_punct(Punct::RBracket)
        {
            let index = match toks[2].kind {
                TokKind::Int(i) => i as i64,
                _ => {
                    cx.error(toks[2].span, "a lane index must be a number");
                    return None;
                }
            };
            let (letter, rest) = word.split_at_checked(1).unwrap_or(("", ""));
            let (num, suffix) = match rest.split_once('.') {
                Some((n, s)) => (n, Some(s)),
                None => (rest, None),
            };
            if let Some(n) = reg_number(num) {
                let atom = match (letter, suffix) {
                    ("v", Some(s)) => elem_code(s)
                        .map(|e| Atom::VecIdx(n, e, index))
                        .or_else(|| arrangement_code(s).map(|a| Atom::VecIdxArr(n, a, index))),
                    ("v", None) => Some(Atom::VecIdx(n, E_NONE, index)),
                    ("z", Some(s)) => elem_code(s).map(|e| Atom::ZIdx(n, e, index)),
                    ("z", None) => Some(Atom::ZIdx(n, E_NONE, index)),
                    _ => None,
                };
                if let Some(atom) = atom {
                    out.push((atom, span));
                    return Some(());
                }
            }
        }
        if toks.len() == 1 {
            if let Some(atom) = register(&word) {
                out.push((atom, span));
                return Some(());
            }
            out.push((Atom::Word(word), span));
            return Some(());
        }
        // `lsl #8`, `msl #16`, `mul #4`.
        if let Some(op) = SHIFTS.iter().position(|s| *s == word) {
            let amount = constant(cx, &toks[1..], "a shift amount")?;
            out.push((Atom::Shift(op as u8, amount), span));
            return Some(());
        }
    }
    out.push((immediate(cx, toks)?, span));
    Some(())
}

/// `#1`, `#-2`, `#0x10`, `#(1 << 3)`, or a floating-point number, `#1.5`.
fn immediate(cx: &mut AsmCtx<'_>, toks: &[Token]) -> Option<Atom> {
    if let Some(f) = float(cx, toks) {
        return Some(Atom::Float(f));
    }
    Some(Atom::Imm(constant(cx, toks, "an immediate")?))
}

/// A decimal number with a fraction, and an exponent where there is one.
///
/// The lexer has no float token, so the pieces arrive as they were spelled:
/// `1.5` is the integer `1` and the name `.5`, `1.5e3` is `1` and `.5e3`,
/// and `2.0e+1`, whose exponent has a sign, is `2`, `.0e`, `+` and `1`. GNU
/// objdump prints the last of those, so GNU as source has it.
fn float(cx: &AsmCtx<'_>, toks: &[Token]) -> Option<f64> {
    let mut i = 0;
    if toks.first().is_some_and(|t| t.is_punct(Punct::Hash)) {
        i += 1;
    }
    let mut text = String::new();
    match toks.get(i) {
        Some(t) if t.is_punct(Punct::Minus) => {
            text.push('-');
            i += 1;
        }
        Some(t) if t.is_punct(Punct::Plus) => i += 1,
        _ => {}
    }
    let TokKind::Int(whole) = toks.get(i)?.kind else {
        return None;
    };
    let frac = ident(cx, toks.get(i + 1)?)?;
    i += 2;
    let digits = frac.strip_prefix('.')?;
    let (digits, open_exponent) = match digits.strip_suffix(['e', 'E']) {
        Some(rest) => (rest, true),
        None => (digits, false),
    };
    let (digits, exponent) = match digits.split_once(['e', 'E']) {
        Some((d, e)) => (d, !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())),
        None => (digits, false),
    };
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (open_exponent && exponent)
    {
        return None;
    }
    text.push_str(&format!("{whole}{frac}"));
    if open_exponent {
        // The sign and the digits of the exponent are tokens of their own.
        match toks.get(i) {
            Some(t) if t.is_punct(Punct::Minus) => {
                text.push('-');
                i += 1;
            }
            Some(t) if t.is_punct(Punct::Plus) => i += 1,
            _ => {}
        }
        let TokKind::Int(exp) = toks.get(i)?.kind else {
            return None;
        };
        text.push_str(&exp.to_string());
        i += 1;
    }
    if i != toks.len() {
        return None;
    }
    text.parse().ok()
}

/// An expression that has to be known now.
fn constant(cx: &mut AsmCtx<'_>, toks: &[Token], what: &str) -> Option<i64> {
    let span = span_of(toks);
    if toks.is_empty() {
        cx.error(span, format!("expected {what}"));
        return None;
    }
    let mut cur = Cursor::new(toks);
    cur.eat_punct(Punct::Hash);
    let e = cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        cx.error(cur.peek().span, format!("unexpected token after {what}"));
        return None;
    }
    match cx.constant(e) {
        Some(v) => Some(v),
        None => {
            cx.error(span, format!("{what} must be a constant"));
            None
        }
    }
}

/// `{ v0.16b, v1.16b }`, `{v0.16b-v3.16b}`, `{ v0.b }[3]`.
fn parse_list(cx: &mut AsmCtx<'_>, toks: &[Token]) -> Option<Atom> {
    let span = span_of(toks);
    let Some(close) = toks.iter().position(|t| t.is_punct(Punct::RBrace)) else {
        cx.error(span, "unterminated `{` in a register list");
        return None;
    };
    let after = &toks[close + 1..];
    let index = match after {
        [] => None,
        [l, t, r] if l.is_punct(Punct::LBracket) && r.is_punct(Punct::RBracket) => match t.kind {
            TokKind::Int(i) => Some(i as i64),
            _ => {
                cx.error(t.span, "a lane index must be a number");
                return None;
            }
        },
        _ => {
            cx.error(span_of(after), "unexpected token after a register list");
            return None;
        }
    };
    let inner = Cursor::new(&toks[1..close]);
    // Each item is a register, or a range of them written with `-`.
    let mut regs: Vec<(char, u8, String)> = Vec::new();
    for item in inner.split_commas() {
        let names: Vec<&[Token]> = item.split(|t| t.is_punct(Punct::Minus)).collect();
        let mut ends = Vec::new();
        for n in &names {
            let parsed = match n {
                [t] => ident(cx, t).and_then(|w| {
                    let (letter, rest) = w.split_at_checked(1)?;
                    let (num, suffix) = rest.split_once('.')?;
                    let c = letter.chars().next()?;
                    matches!(c, 'v' | 'z' | 'p')
                        .then(|| reg_number(num).map(|n| (c, n, suffix.to_string())))?
                }),
                _ => None,
            };
            let Some(p) = parsed else {
                cx.error(span_of(n), "expected a register in a register list");
                return None;
            };
            ends.push(p);
        }
        match ends.as_slice() {
            [one] => regs.push(one.clone()),
            [lo, hi] if lo.0 == hi.0 && lo.2 == hi.2 => {
                let count = (hi.1 as i32 - lo.1 as i32).rem_euclid(32) + 1;
                for k in 0..count {
                    regs.push((lo.0, ((lo.1 as i32 + k) % 32) as u8, lo.2.clone()));
                }
            }
            _ => {
                cx.error(
                    span_of(item),
                    "a register range joins two registers of one kind",
                );
                return None;
            }
        }
    }
    let Some((letter, first, suffix)) = regs.first().cloned() else {
        cx.error(span, "an empty register list");
        return None;
    };
    let modulus = if letter == 'p' { 16 } else { 32 };
    for (k, r) in regs.iter().enumerate() {
        if r.0 != letter || r.2 != suffix {
            cx.error(span, "every register in a list must be of the same kind");
            return None;
        }
        if r.1 as usize != (first as usize + k) % modulus {
            cx.error(span, "the registers in a list must be consecutive");
            return None;
        }
    }
    let count = regs.len() as u8;
    let bad = || format!("`.{suffix}` is not an element size or arrangement here");
    Some(match (letter, index) {
        ('v', Some(i)) => match elem_code(&suffix) {
            Some(e) => Atom::VecListIdx(first, count, e, i),
            None => {
                cx.error(span, bad());
                return None;
            }
        },
        ('v', None) => match arrangement_code(&suffix) {
            Some(a) => Atom::VecList(first, count, a),
            None => {
                cx.error(span, bad());
                return None;
            }
        },
        (_, Some(_)) => {
            cx.error(span, "only a vector register list takes a lane index");
            return None;
        }
        (c, None) => match elem_code(&suffix) {
            Some(e) if c == 'z' => Atom::ZList(first, count, e),
            Some(e) => Atom::PList(first, count, e),
            None => {
                cx.error(span, bad());
                return None;
            }
        },
    })
}

/// `[x0]`, `[x0, #1, mul vl]`, `[x0, z1.d, lsl #3]`, `[x0, #8]!`.
fn parse_address(cx: &mut AsmCtx<'_>, toks: &[Token], out: &mut Vec<(Atom, Span)>) -> Option<()> {
    let span = span_of(toks);
    let Some(close) = toks.iter().position(|t| t.is_punct(Punct::RBracket)) else {
        cx.error(span, "unterminated `[` in an address");
        return None;
    };
    let after = &toks[close + 1..];
    let writeback = match after {
        [] => false,
        [t] if t.is_punct(Punct::Bang) => true,
        _ => {
            cx.error(span_of(after), "unexpected token after `]`");
            return None;
        }
    };
    out.push((Atom::Open, toks[0].span));
    let inner = Cursor::new(&toks[1..close]);
    for (k, part) in inner.split_commas().into_iter().enumerate() {
        if part.is_empty() {
            cx.error(span, "an empty part of an address");
            return None;
        }
        let pspan = span_of(part);
        if let Some(word) = ident(cx, &part[0]) {
            if word == "mul" && part.len() == 2 && ident(cx, &part[1]).as_deref() == Some("vl") {
                out.push((Atom::MulVl, pspan));
                continue;
            }
            if k > 0
                && let Some(x) = EXTENDS.iter().position(|e| *e == word)
            {
                let atom = if part.len() == 1 {
                    Atom::Ext(x as u8)
                } else {
                    Atom::ExtAmt(x as u8, constant(cx, &part[1..], "an extend amount")?)
                };
                out.push((atom, pspan));
                continue;
            }
        }
        parse_operand(cx, part, out)?;
    }
    out.push((
        if writeback {
            Atom::CloseWb
        } else {
            Atom::Close
        },
        toks[close].span,
    ));
    Some(())
}

// ---- matching ----------------------------------------------------------------------

/// A number taken from an operand.
#[derive(Copy, Clone, Debug)]
enum Val {
    Int(i64),
    Float(f64),
}

impl Val {
    fn int(self) -> Option<i64> {
        match self {
            Val::Int(v) => Some(v),
            Val::Float(_) => None,
        }
    }
}

/// The numbers an operand gives a slot of this kind, if it fits the kind.
fn fits(kind: Kind, atom: &Atom) -> Option<(Option<Val>, Option<Val>)> {
    use Val::Int;
    let one = |v: i64| Some((Some(Int(v)), None));
    let two = |v: i64, i: i64| Some((Some(Int(v)), Some(Int(i))));
    match (kind, atom) {
        (Kind::Vec(a), Atom::Vec(n, b)) if a == *b => one(*n as i64),
        (Kind::VecIdx(e), Atom::VecIdx(n, f, i)) if e == *f => two(*n as i64, *i),
        (Kind::VecIdxArr(a), Atom::VecIdxArr(n, b, i)) if a == *b => two(*n as i64, *i),
        (Kind::Scalar(e), Atom::Scalar(n, f)) if e == *f => one(*n as i64),
        (Kind::Gpr(c, spell), Atom::Gpr(n, d, sp)) if c == *d => {
            // Register 31 is one of two registers depending on the field;
            // writing the other name would silently mean something else.
            if *n == 31 && spell != S_ANY && *sp != (spell == S_SP) {
                return None;
            }
            one(*n as i64)
        }
        (Kind::Z(e), Atom::Z(n, f)) if e == *f => one(*n as i64),
        (Kind::ZIdx(e), Atom::ZIdx(n, f, i)) if e == *f => two(*n as i64, *i),
        (Kind::P(e, m), Atom::P(n, f, k)) if e == *f && m == *k => one(*n as i64),
        (Kind::VecList(c, a), Atom::VecList(n, d, b)) if c == *d && a == *b => one(*n as i64),
        (Kind::VecListIdx(c, e), Atom::VecListIdx(n, d, f, i)) if c == *d && e == *f => {
            two(*n as i64, *i)
        }
        (Kind::ZList(c, e), Atom::ZList(n, d, f)) if c == *d && e == *f => one(*n as i64),
        // GNU as and llvm-mc both take a lone register for a one-register
        // list: `ld1d z0.d, p0/z, [x0]`.
        (Kind::ZList(1, e), Atom::Z(n, f)) if e == *f => one(*n as i64),
        (Kind::PList(c, e), Atom::PList(n, d, f)) if c == *d && e == *f => one(*n as i64),
        (Kind::Imm, Atom::Imm(v)) => one(*v),
        (Kind::FImm, Atom::Float(v)) => Some((Some(Val::Float(*v)), None)),
        (Kind::FImm, Atom::Imm(v)) => Some((Some(Val::Float(*v as f64)), None)),
        (Kind::Cond, Atom::Word(w)) => reg::cond(w).map(|c| (Some(Int(c as i64)), None)),
        (Kind::Pat, Atom::Word(w)) => PATTERNS
            .iter()
            .find(|(name, _)| name == w)
            .map(|(_, c)| (Some(Int(*c as i64)), None)),
        (Kind::Prf, Atom::Word(w)) => PREFETCHES
            .iter()
            .find(|(name, _)| name == w)
            .map(|(_, c)| (Some(Int(*c as i64)), None)),
        (Kind::Shift(s), Atom::Shift(t, v)) if s == *t => one(*v),
        (Kind::MulVl, Atom::MulVl)
        | (Kind::Open, Atom::Open)
        | (Kind::Close, Atom::Close)
        | (Kind::CloseWb, Atom::CloseWb) => Some((None, None)),
        (Kind::Ext(x), Atom::Ext(y)) if x == *y => Some((None, None)),
        (Kind::ExtAmt(x), Atom::ExtAmt(y, v)) if x == *y => one(*v),
        _ => None,
    }
}

fn set_field(word: u32, lsb: u8, width: u8, x: u64) -> u32 {
    let mask = if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    (word & !(mask << lsb)) | (((x as u32) & mask) << lsb)
}

fn set_bits(mut word: u32, bits: &[(u8, u8)], x: u64) -> u32 {
    for &(vb, wb) in bits {
        word = (word & !(1 << wb)) | ((((x >> vb) & 1) as u32) << wb);
    }
    word
}

/// The 8-bit floating-point immediate for a value, if it has one: a sign, an
/// exponent of -3..4 and a fraction in sixteenths.
fn fp_imm8(v: f64) -> Option<u64> {
    (0..256u64).find(|&imm8| {
        let sign = if imm8 & 0x80 != 0 { -1.0 } else { 1.0 };
        let b = (imm8 >> 6) & 1;
        let cd = ((imm8 >> 4) & 3) as i32;
        let exp = if b == 1 { cd - 3 } else { cd + 1 };
        let frac = (imm8 & 15) as f64;
        sign * 2f64.powi(exp) * (1.0 + frac / 16.0) == v
    })
}

/// What an immediate becomes under a transform, or why it cannot.
fn transform(xf: Xf, v: Val) -> Result<u64, String> {
    match (xf, v) {
        (Xf::FpImm, Val::Float(f)) => {
            fp_imm8(f).ok_or_else(|| format!("{f} is not an 8-bit floating-point immediate"))
        }
        (Xf::FpImm, Val::Int(_)) => Err("expected a floating-point immediate".into()),
        (Xf::NotLogImm(esize), Val::Int(v)) => {
            let bits = esize as u32;
            if bits < 64 && (v < -(1i64 << (bits - 1)) || v >= (1i64 << bits)) {
                return Err(format!("{v:#x} does not fit a {bits}-bit element"));
            }
            let mask = if bits < 64 {
                (1u64 << bits) - 1
            } else {
                u64::MAX
            };
            transform(Xf::LogImm(esize), Val::Int((!(v as u64) & mask) as i64))
                .map_err(|_| format!("{v:#x} is not the complement of a valid logical immediate"))
        }
        (Xf::LogImm(esize), Val::Int(v)) => {
            let bits = esize as u32;
            // A negative number that fits the element signed is taken as its
            // low `esize` bits, as both references take `and z0.b, z0.b, #-2`.
            let value = if bits < 64 {
                if v < -(1i64 << (bits - 1)) || v >= (1i64 << bits) {
                    return Err(format!("{v:#x} does not fit a {bits}-bit element"));
                }
                (v as u64) & ((1u64 << bits) - 1)
            } else {
                v as u64
            };
            logical_imm(value, bits)
                .map(|(n, immr, imms)| ((n << 12) | (immr << 6) | imms) as u64)
                .ok_or_else(|| {
                    format!(
                        "{v:#x} is not a valid logical immediate: the field holds only a repeating run of ones"
                    )
                })
        }
        (Xf::ByteMask, Val::Int(v)) => {
            let mut out = 0;
            for k in 0..8 {
                match (v as u64 >> (8 * k)) & 0xff {
                    0xff => out |= 1 << k,
                    0 => {}
                    _ => {
                        return Err(format!(
                            "{v:#x} is not a byte mask: every byte must be 0x00 or 0xff"
                        ));
                    }
                }
            }
            Ok(out)
        }
        (_, Val::Float(_)) => Err("expected an integer".into()),
    }
}

/// A number of an element's width as a signed range reads it; see [`Form`].
fn wrapped(enc: Enc, v: Option<Val>, wrap: u8, dirs: u8) -> Option<Val> {
    let (min, max) = match enc {
        Enc::Field { min, max, .. }
        | Enc::Scatter { min, max, .. }
        | Enc::Affine { min, max, .. } => (min, max),
        _ => return v,
    };
    let Some(Val::Int(x)) = v else {
        return v;
    };
    if wrap == 0 || min >= 0 {
        return v;
    }
    let size = 1i64 << wrap;
    if dirs & 1 != 0 && x > max && (size / 2..size).contains(&x) {
        return Some(Val::Int(x - size));
    }
    if dirs & 2 != 0 && x < min && (-size + 1..-size / 2).contains(&x) {
        return Some(Val::Int(x + size));
    }
    v
}

/// Puts one number into the word, or says why it does not fit.
fn apply(enc: Enc, v: Option<Val>, word: u32) -> Result<u32, String> {
    let range = |v: i64, min: i64, max: i64| {
        if v < min || v > max {
            Err(format!("must be {min}..={max}, but is {v}"))
        } else {
            Ok(())
        }
    };
    let int = |v: Option<Val>| {
        v.and_then(Val::int)
            .ok_or("expected an integer".to_string())
    };
    match enc {
        Enc::None => Ok(word),
        Enc::Fixed(want) => match v {
            Some(Val::Int(got)) if got == want => Ok(word),
            Some(Val::Float(got)) if got == want as f64 => Ok(word),
            Some(Val::Int(got)) => Err(format!("must be {want}, but is {got}")),
            Some(Val::Float(got)) => Err(format!("must be {want}, but is {got}")),
            None => Ok(word),
        },
        Enc::Field {
            lsb,
            width,
            min,
            max,
        } => {
            let v = int(v)?;
            range(v, min, max)?;
            Ok(set_field(word, lsb, width, v as u64))
        }
        Enc::Scatter { min, max, bits } => {
            let v = int(v)?;
            range(v, min, max)?;
            let step = 1i64 << bits.first().map_or(0, |b| b.0);
            if (v - min).rem_euclid(step) != 0 {
                return Err(format!(
                    "must be a multiple of {step} from {min}, but is {v}"
                ));
            }
            Ok(set_bits(word, bits, v as u64))
        }
        Enc::Affine {
            lsb,
            width,
            sign,
            step,
            min,
            max,
        } => {
            let v = int(v)?;
            range(v, min, max)?;
            if (v - min) % step != 0 {
                return Err(format!(
                    "must be a multiple of {step} from {min}, but is {v}"
                ));
            }
            let mask = (1i64 << width) - 1;
            let origin = ((word >> lsb) as i64) & mask;
            let field = origin + sign as i64 * ((v - min) / step);
            Ok(set_field(word, lsb, width, (field & mask) as u64))
        }
        Enc::Xform(xf, lsb, width) => {
            let x = transform(xf, v.ok_or("expected an immediate")?)?;
            Ok(set_field(word, lsb, width, x))
        }
        Enc::XformBits(xf, bits) => {
            let x = transform(xf, v.ok_or("expected an immediate")?)?;
            Ok(set_bits(word, bits, x))
        }
        // Checked by `encode`, which knows which operand is repeated.
        Enc::Tied(_) | Enc::TiedNext(_) => Ok(word),
        Enc::Choice { lsb, width, map } => {
            let v = int(v)?;
            match map.iter().find(|(x, _)| *x == v) {
                Some((_, code)) => Ok(set_field(word, lsb, width, *code as u64)),
                None => {
                    let list: Vec<String> = map.iter().map(|(x, _)| x.to_string()).collect();
                    Err(format!("must be one of {}, but is {v}", list.join(", ")))
                }
            }
        }
        Enc::FChoice { lsb, width, map } => {
            let f = match v {
                Some(Val::Float(f)) => f,
                Some(Val::Int(i)) => i as f64,
                None => return Ok(word),
            };
            match map.iter().find(|(x, _)| *x == f) {
                Some((_, code)) => Ok(set_field(word, lsb, width, *code as u64)),
                None => {
                    let list: Vec<String> = map.iter().map(|(x, _)| format!("{x:?}")).collect();
                    Err(format!("must be one of {}, but is {f}", list.join(", ")))
                }
            }
        }
    }
}

/// Encodes one form, or explains which operand is out of its range.
fn encode(form: Form, shape: &[u16], atoms: &[(Atom, Span)]) -> Result<u32, (Span, String)> {
    let mut word = form.2;
    let mut values: Vec<Option<Val>> = Vec::with_capacity(atoms.len() * 2);
    // Which operand each value came from, for a tie's diagnostic.
    let mut owners: Vec<usize> = Vec::with_capacity(atoms.len() * 2);
    for (k, (&slot, (atom, span))) in shape.iter().zip(atoms).enumerate() {
        let slot = SLOTS[slot as usize];
        let (a, b) = fits(slot.kind, atom).unwrap_or((None, None));
        for (which, enc, v) in [("", slot.a, a), (" index", slot.b, b)] {
            if matches!(enc, Enc::None) {
                continue;
            }
            if let Enc::Tied(j) = enc
                && v.and_then(Val::int) != values[j as usize].and_then(Val::int)
            {
                return Err((
                    *span,
                    format!(
                        "operand {} has to be the same register as operand {}",
                        k + 1,
                        owners[j as usize] + 1
                    ),
                ));
            }
            if let Enc::TiedNext(j) = enc
                && v.and_then(Val::int) != values[j as usize].and_then(Val::int).map(|n| n + 1)
            {
                return Err((
                    *span,
                    format!(
                        "operand {} has to be the register after operand {}",
                        k + 1,
                        owners[j as usize] + 1
                    ),
                ));
            }
            let v = wrapped(enc, v, form.3, form.4);
            word = apply(enc, v, word)
                .map_err(|why| (*span, format!("operand {}{which}: {why}", k + 1)))?;
            values.push(v);
            owners.push(k);
        }
    }
    Ok(word)
}

/// The forms of one mnemonic, in the order they are tried.
fn forms_of(mnemonic: &str) -> Option<&'static [Form]> {
    let index = MNEMONICS.binary_search(&mnemonic).ok()? as u16;
    let lo = FORMS.partition_point(|f| f.0 < index);
    let hi = FORMS.partition_point(|f| f.0 <= index);
    Some(&FORMS[lo..hi])
}

/// True if the table has any form of this mnemonic.
pub fn knows(mnemonic: &str) -> bool {
    forms_of(mnemonic).is_some()
}

/// True if the operand tokens name a register only a table form can take:
/// a vector, SVE or predicate register, a scalar SIMD register, or a list.
pub fn has_simd_operand(cx: &AsmCtx<'_>, toks: &[Token], scalar: bool) -> bool {
    toks.iter().any(|t| {
        if t.is_punct(Punct::LBrace) {
            return true;
        }
        let Some(word) = ident(cx, t) else {
            return false;
        };
        let name = word.as_str();
        match register(name) {
            Some(Atom::Vec(..) | Atom::Z(..) | Atom::P(..)) => true,
            Some(Atom::Scalar(..)) => scalar,
            _ => {
                // `v0.s` before `[1]`, which the lexer leaves in one name.
                let (letter, rest) = name.split_at_checked(1).unwrap_or(("", ""));
                matches!(letter, "v" | "z")
                    && rest
                        .split_once('.')
                        .is_some_and(|(n, s)| reg_number(n).is_some() && elem_code(s).is_some())
            }
        }
    })
}

/// Assembles `mnemonic` from the table.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    mnemonic_span: Span,
    toks: &[Token],
) -> Option<Vec<Variant>> {
    let Some(forms) = forms_of(mnemonic) else {
        cx.error(mnemonic_span, format!("unknown instruction `{mnemonic}`"));
        return None;
    };
    let atoms = parse(cx, toks)?;
    let mut range_error = None;
    // The first form whose operands fit and whose features the target has
    // not, for the diagnostic where no form it does have fits.
    let mut absent = None;
    for &form in forms {
        let shape = SHAPES[form.1 as usize];
        if shape.len() != atoms.len()
            || !shape
                .iter()
                .zip(&atoms)
                .all(|(&s, (a, _))| fits(SLOTS[s as usize].kind, a).is_some())
        {
            continue;
        }
        let want = FEATS[form.5 as usize];
        if !super::cpu::supports(cx.state, want) {
            absent = absent.or(Some(want));
            continue;
        }
        match encode(form, shape, &atoms) {
            Ok(w) => return Some(vec![word(w)]),
            // The forms of a shape run narrowest first, so the last one to
            // refuse a value is the most general, and says most about it.
            Err(e) => range_error = Some(e),
        }
    }
    if let Some((span, why)) = range_error {
        cx.error(span, format!("`{mnemonic}` {why}"));
        return None;
    }
    // The operands fit a form, and the target has no form they fit: GNU as
    // reports that as "selected processor does not support", whatever else
    // the mnemonic can take.
    if let Some(want) = absent {
        let msg = super::cpu::unsupported(cx.state, mnemonic, want);
        cx.error(mnemonic_span, msg);
        return None;
    }
    let given: Vec<String> = atoms.iter().map(|(a, _)| a.describe()).collect();
    let mut shapes: Vec<String> = forms
        .iter()
        .map(|f| describe_shape(SHAPES[f.1 as usize]))
        .collect();
    shapes.dedup();
    let more = shapes.len().saturating_sub(6);
    shapes.truncate(6);
    let mut msg = format!(
        "`{mnemonic}` does not take `{}`; it takes {}",
        given.join(", "),
        shapes.join(" or ")
    );
    if more > 0 {
        msg.push_str(&format!(", or {more} other forms"));
    }
    let span = if toks.is_empty() {
        mnemonic_span
    } else {
        mnemonic_span.to(span_of(toks))
    };
    cx.error(span, msg);
    None
}

/// A form's operands as a person would write them, for a diagnostic.
fn describe_shape(shape: &[u16]) -> String {
    let mut out = String::from("`");
    for (i, &s) in shape.iter().enumerate() {
        let slot = SLOTS[s as usize];
        let text = match slot.kind {
            Kind::Vec(a) => format!("vN.{}", ARRANGEMENTS[a as usize]),
            Kind::VecIdx(e) => format!("vN.{}[i]", ELEMS[e as usize]),
            Kind::VecIdxArr(a) => format!("vN.{}[i]", ARRANGEMENTS[a as usize]),
            Kind::Scalar(e) => format!("{}N", ELEMS[e as usize]),
            Kind::Gpr(c, _) => if c == G_X { "xN" } else { "wN" }.into(),
            Kind::Z(e) => z_name('z', 0, e).replace('0', "N"),
            Kind::ZIdx(e) => format!("zN.{}[i]", ELEMS[e as usize]),
            Kind::P(e, m) => format!(
                "{}{}",
                z_name('p', 0, e).replace('0', "N"),
                ["", "/m", "/z"][m as usize]
            ),
            Kind::VecList(c, a) => format!("{{ {c} × vN.{} }}", ARRANGEMENTS[a as usize]),
            Kind::VecListIdx(c, e) => format!("{{ {c} × vN.{} }}[i]", ELEMS[e as usize]),
            Kind::ZList(c, e) => format!("{{ {c} × zN.{} }}", ELEMS[e as usize]),
            Kind::PList(c, e) => format!("{{ {c} × pN.{} }}", ELEMS[e as usize]),
            Kind::Imm => match slot.a {
                Enc::Fixed(v) => format!("#{v}"),
                _ => "#imm".into(),
            },
            Kind::FImm => "#fp".into(),
            Kind::Cond => "cond".into(),
            Kind::Pat => "pattern".into(),
            Kind::Prf => "prefetch-op".into(),
            Kind::Shift(sh) => format!("{} #n", SHIFTS[sh as usize]),
            Kind::MulVl => "mul vl".into(),
            Kind::Open => "[".into(),
            Kind::Close => "]".into(),
            Kind::CloseWb => "]!".into(),
            Kind::Ext(x) => EXTENDS[x as usize].into(),
            Kind::ExtAmt(x) => format!("{} #n", EXTENDS[x as usize]),
        };
        let separator = i > 0 && !matches!(slot.kind, Kind::Close | Kind::CloseWb) && {
            let prev = SLOTS[shape[i - 1] as usize].kind;
            !matches!(prev, Kind::Open)
        };
        if separator {
            out.push_str(", ");
        }
        out.push_str(&text);
    }
    out.push('`');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_are_grouped_by_mnemonic_for_the_binary_search() {
        assert!(MNEMONICS.windows(2).all(|w| w[0] < w[1]));
        assert!(FORMS.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(
            FORMS
                .iter()
                .all(|f| (f.1 as usize) < SHAPES.len() && (f.0 as usize) < MNEMONICS.len())
        );
        assert!(
            SHAPES
                .iter()
                .flat_map(|s| s.iter())
                .all(|&i| (i as usize) < SLOTS.len())
        );
    }

    /// A tie names a value the encoder has already seen.
    #[test]
    fn ties_point_backwards() {
        for shape in SHAPES {
            let mut seen = 0;
            for &i in *shape {
                let slot = SLOTS[i as usize];
                for enc in [slot.a, slot.b] {
                    match enc {
                        Enc::None => continue,
                        Enc::Tied(j) | Enc::TiedNext(j) => {
                            assert!((j as usize) < seen, "{shape:?}")
                        }
                        _ => {}
                    }
                    seen += 1;
                }
            }
        }
    }

    /// llvm-mc's `fmov s0, #1.0` is `1e2e1000`, `#-0.1875` has imm8 `0xc8`
    /// (`fmov d31, #-0.1875` is `1e79101f`), and zero has no 8-bit form.
    #[test]
    fn the_floating_point_immediate() {
        assert_eq!(fp_imm8(1.0), Some(0x70));
        assert_eq!(fp_imm8(-0.1875), Some(0xc8));
        assert_eq!(fp_imm8(0.0), None);
        assert_eq!(fp_imm8(0.3), None);
    }
}
