//! `asm!` lowered with its operands: `builtin # asm(..)`, the form blaze-expand writes for
//! `asm!` (its `fn asm`), becomes one [`Expr::Asm`] whose row is an [`AsmForm`]: the
//! templates' pieces (text, the operand a placeholder names with its modifier, and the line
//! break between two template strings), the operands (direction, register class or explicit
//! register, expressions), the options and the `clobber_abi` names. `global_asm!` and
//! `naked_asm!` are refused by name ([`Why::Asm`]) until they have a lowering of their own.
//!
//! What is checked here is what the Reference states from the syntax alone (inline-assembly,
//! "Template string arguments", "Options", "ABI clobbers"): every placeholder names an operand,
//! every named and positional operand is used, positional operands come first, the options'
//! combinations, and generic-class outputs beside `clobber_abi`. Each refusal is a
//! [`DiagKind::Asm`] naming its [`AsmRule`]. What needs the target or the types (which
//! classes, registers and modifiers exist, an operand's type, `att_syntax` off x86, two
//! operands on one register, labels beside outputs under `asm_goto_with_outputs`) is the
//! checker's.
//!
//! The parser's nodes inside `ASM_EXPR` (`ASM_OPERAND_NAMED`, `ASM_CLOBBER_ABI`,
//! `ASM_OPTIONS`) are each packed into one value by [`Machine::asm_part`] when they close; the
//! nodes inside those read through (their tokens and lowered children are the part's).

use blaze_own::{AVec, Vec};
use blaze_surface::Range;
use blaze_typeck::Name;
use parser::LexedStr;
use parser::SyntaxKind::{self, *};

use crate::columns::{Lens, View};
use crate::form::*;
use crate::machine::{Frame, Machine, V, Val, idx};

/// A part of an `asm!` that was refused where it closed (its diagnostic already written):
/// [`Node::e`] of the value it leaves.
const BROKEN: u32 = 1;

/// `asm!`'s builtin form, a row: the pieces (rows of [`AsmPiece`], all templates in order),
/// the operands (rows of [`AsmOperand`], in the order written; a piece's `index` is a
/// position here), the options (the `AsmForm::PURE` .. bits) and the `clobber_abi` names
/// (`ids` of [`NameIdx`], each one string's contents, in the order written).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsmForm {
    pub pieces: List,
    pub operands: List,
    pub options: u16,
    pub clobber_abis: List,
    /// The target's architecture, read as `target_arch` atoms at the `asm!`
    /// (machine/asm_arch.rs): `None` when no tabled one holds. Kept in `flags` bits 12..16.
    pub arch: Option<blaze_typeck::AsmArch>,
}

impl AsmForm {
    /// The option bits, the same as blaze-lir's `AsmRec` bits.
    pub const PURE: u16 = 1 << 0;
    pub const NOMEM: u16 = 1 << 1;
    pub const READONLY: u16 = 1 << 2;
    pub const PRESERVES_FLAGS: u16 = 1 << 3;
    pub const NORETURN: u16 = 1 << 4;
    pub const NOSTACK: u16 = 1 << 5;
    pub const ATT_SYNTAX: u16 = 1 << 6;
    pub const RAW: u16 = 1 << 7;
    pub const MAY_UNWIND: u16 = 1 << 8;

    pub(crate) fn encode(self) -> Node {
        let mut n = Node::of(ROW_ASM);
        n.a = self.pieces.start;
        n.b = self.pieces.len;
        n.c = self.operands.start;
        n.d = self.operands.len;
        n.e = self.clobber_abis.start;
        n.f = self.clobber_abis.len;
        let arch: u16 = match self.arch {
            None => 0,
            Some(blaze_typeck::AsmArch::Aarch64) => 1,
            Some(blaze_typeck::AsmArch::X86_64) => 2,
        };
        n.flags = (self.options & 0x0fff) | (arch << 12);
        n
    }

    pub(crate) fn decode(n: Node) -> Result<AsmForm, BadRow> {
        if n.tag != ROW_ASM {
            return Err(BadRow { column: "asm", tag: n.tag });
        }
        let arch = match n.flags >> 12 {
            0 => None,
            1 => Some(blaze_typeck::AsmArch::Aarch64),
            2 => Some(blaze_typeck::AsmArch::X86_64),
            _ => return Err(BadRow { column: "asm", tag: n.tag }),
        };
        Ok(AsmForm {
            pieces: List { start: n.a, len: n.b },
            operands: List { start: n.c, len: n.d },
            options: n.flags & 0x0fff,
            clobber_abis: List { start: n.e, len: n.f },
            arch,
        })
    }
}

/// One piece of an `asm!` template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmPiece {
    /// Text, by its range in a template literal. `{{` and `}}` are cut to one brace (the range
    /// ends after the first, the next piece starts after the second). `cooked`: the literal is
    /// not raw, so its string escapes (`\n`, `\t`, a line continuation) are still to be applied
    /// to the range's text; no escape in it makes a brace (those are refused,
    /// [`Why::AsmTemplateEscape`]).
    Text { text: BodyRange, cooked: bool },
    /// `{}`, `{0}` or `{name}`, with `:m` a one-character modifier: the operand's position
    /// among the [`AsmForm`]'s operands, the modifier, and the placeholder's range (braces
    /// included) for the checker's refusals.
    Operand { index: u32, modifier: Option<char>, at: BodyRange },
    /// The `\n` between two template strings: the Reference, "Template string arguments": "an
    /// asm! with multiple template string arguments is treated as if all the strings were
    /// concatenated with a \n between them".
    Line,
}

impl AsmPiece {
    pub(crate) fn encode(self) -> Node {
        let mut n = Node::of(ROW_ASM_PIECE);
        match self {
            AsmPiece::Text { text, cooked } => {
                n.a = text.raw().start;
                n.b = text.raw().end;
                n.flags = if cooked { 4 } else { 0 };
            }
            AsmPiece::Operand { index, modifier, at } => {
                n.a = at.raw().start;
                n.b = at.raw().end;
                n.c = index;
                n.d = match modifier {
                    Some(m) => m as u32,
                    None => NONE,
                };
                n.flags = 1;
            }
            AsmPiece::Line => n.flags = 2,
        }
        n
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<AsmPiece, BadRow> {
        let bad = BadRow { column: "asm pieces", tag: n.tag };
        if n.tag != ROW_ASM_PIECE {
            return Err(bad);
        }
        let range = BodyRange::new(Range::new(n.a, n.b), lens);
        Ok(match n.flags & 3 {
            0 => AsmPiece::Text { text: range, cooked: n.flags & 4 != 0 },
            1 => {
                let modifier = match n.d {
                    NONE => None,
                    m => Some(char::from_u32(m).ok_or(bad)?),
                };
                AsmPiece::Operand { index: n.c, modifier, at: range }
            }
            2 => AsmPiece::Line,
            _ => return Err(bad),
        })
    }
}

/// How an operand passes its value: the Reference, inline-assembly, "Operand type".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmKind {
    In,
    Out,
    LateOut,
    InOut,
    InLateOut,
    /// `sym path`: [`AsmOperand::expr`] is the path as an expression (a local is the
    /// checker's refusal: the Reference, "`sym <path>`": "`<path>` must refer to a `fn` or
    /// `static`").
    Sym,
    /// `const expr`.
    Const,
    /// `label { .. }`: [`AsmOperand::expr`] is the block.
    Label,
}

impl AsmKind {
    const ALL: [AsmKind; 8] = [
        AsmKind::In,
        AsmKind::Out,
        AsmKind::LateOut,
        AsmKind::InOut,
        AsmKind::InLateOut,
        AsmKind::Sym,
        AsmKind::Const,
        AsmKind::Label,
    ];

    const fn code(self) -> u16 {
        match self {
            AsmKind::In => 0,
            AsmKind::Out => 1,
            AsmKind::LateOut => 2,
            AsmKind::InOut => 3,
            AsmKind::InLateOut => 4,
            AsmKind::Sym => 5,
            AsmKind::Const => 6,
            AsmKind::Label => 7,
        }
    }

    /// Whether the operand writes a place (or discards with `_`) at the end of the assembly.
    pub const fn is_output(self) -> bool {
        matches!(self, AsmKind::Out | AsmKind::LateOut | AsmKind::InOut | AsmKind::InLateOut)
    }
}

/// A register operand's `<reg>`: the Reference, "Register operands": "Input and output
/// operands can be specified either as an explicit register or as a register class".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmReg {
    /// `in(reg)`: the class's name as written.
    Class(NameIdx),
    /// `in("x0")`: the string's contents.
    Explicit(NameIdx),
}

/// One operand, a row: its name with `name = ..`, its kind, its register (only `in`, `out`,
/// `lateout`, `inout` and `inlateout` have one), its expression (the input or the place; a
/// `const`'s expression; a `sym`'s path; a `label`'s block) and, for `inout(..) a => b`, the
/// output place `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsmOperand {
    pub name: Option<NameIdx>,
    pub kind: AsmKind,
    pub reg: Option<AsmReg>,
    pub expr: ExprId,
    pub out_expr: Option<ExprId>,
}

impl AsmOperand {
    pub(crate) fn encode(self) -> Node {
        let mut n = Node::of(ROW_ASM_OPERAND);
        n.a = opt(self.name.map(|n| n.0));
        let (reg, explicit) = match self.reg {
            Some(AsmReg::Class(r)) => (r.0, 0),
            Some(AsmReg::Explicit(r)) => (r.0, 1 << 4),
            None => (NONE, 0),
        };
        n.b = reg;
        n.c = self.expr.0;
        n.d = opt(self.out_expr.map(|e| e.0));
        n.flags = self.kind.code() | explicit;
        n
    }

    pub(crate) fn decode(n: Node) -> Result<AsmOperand, BadRow> {
        let bad = BadRow { column: "asm operands", tag: n.tag };
        if n.tag != ROW_ASM_OPERAND {
            return Err(bad);
        }
        let kind = *AsmKind::ALL.get(usize::from(n.flags & 15)).ok_or(bad)?;
        let reg = match (unopt(n.b), n.flags & (1 << 4) != 0) {
            (Some(r), false) => Some(AsmReg::Class(NameIdx(r))),
            (Some(r), true) => Some(AsmReg::Explicit(NameIdx(r))),
            (None, false) => None,
            (None, true) => return Err(bad),
        };
        Ok(AsmOperand {
            name: unopt(n.a).map(NameIdx),
            kind,
            reg,
            expr: ExprId(n.c),
            out_expr: unopt(n.d).map(ExprId),
        })
    }
}

/// Why an `asm!` is refused: the rule of the Reference (inline-assembly) it breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmRule {
    /// No template string ("asm.ts-args.one-or-more": "An asm! invocation may have one or
    /// more template string arguments").
    NoTemplate,
    /// A template that is no string literal ("FormatString → STRING_LITERAL |
    /// RAW_STRING_LITERAL | MacroInvocation"; a macro call is [`Why::NonStringTemplate`]).
    TemplateNotString,
    /// A `}` that closes nothing and is not `}}` ("asm.ts-args.syntax": "the same syntax as
    /// format strings"; alloc's fmt docs: "maybe_format := '{' '{' | '}' '}' | format").
    UnmatchedClose,
    /// A `{` with no `}` after it in its template.
    UnclosedPlaceholder,
    /// A placeholder whose argument is neither empty, an integer nor an identifier (alloc's
    /// fmt docs: "argument := integer | identifier").
    BadPlaceholder,
    /// A modifier that is not one character ("asm.template-modifiers.only-one": "Only one
    /// modifier is allowed per template placeholder").
    ModifierNotOneChar,
    /// `{}` or `{n}` past the positional operands ("asm.ts-args.register-operands": "Explicit
    /// register operands cannot be used by placeholders in the template string"; a named
    /// operand is reached by its name: "accessed in order, by index, or by name").
    NoSuchPositional,
    /// `{name}` naming no operand ("asm.ts-args.no-implicit": "implicit named arguments ...
    /// are not supported").
    NoSuchName,
    /// A named or positional operand no placeholder uses ("asm.ts-args.at-least-once": "All
    /// other named and positional operands must appear at least once in the template
    /// string").
    UnusedOperand,
    /// An unnamed class operand after a named or explicit-register one
    /// ("asm.ts-args.positional-first": "positional arguments must appear before named
    /// arguments and explicit register operands").
    PositionalAfterNamed,
    /// `name = in("x0") ..`: a name no placeholder may use ("asm.ts-args.register-operands").
    NamedExplicitRegister,
    /// Two operands of one name: `{name}` would name both ("accessed ... by name").
    DuplicateName,
    /// A register-class output beside `clobber_abi` ("asm.abi-clobbers.must-specify": "all
    /// outputs must specify an explicit register").
    ClassOutputWithClobberAbi,
    /// `clobber_abi()` with no ABI ("ClobberAbi → clobber_abi ( Abi ( , Abi )* ,? )").
    ClobberAbiEmpty,
    /// "asm.options.checks-mutually-exclusive": "The nomem and readonly options are mutually
    /// exclusive".
    NomemAndReadonly,
    /// "asm.options.supported-options-pure": "The pure option must be combined with either the
    /// nomem or readonly options".
    PureNeedsNomemOrReadonly,
    /// "asm.options.checks-pure": "It is a compile-time error to specify pure on an asm block
    /// with no outputs or only discarded outputs (_)".
    PureWithoutOutputs,
    /// "asm.options.checks-noreturn": "It is a compile-time error to specify noreturn on an asm
    /// block with outputs and without labels".
    NoreturnWithOutputs,
    /// An attribute other than `cfg`/`cfg_attr` on an operand ("asm.attributes.supported-
    /// attributes": "Other attributes are parsed but rejected").
    AttributeNotCfg,
    /// A part the parser left incomplete (its syntax error is reported too).
    Malformed,
}

impl AsmRule {
    const ALL: [AsmRule; 20] = [
        AsmRule::NoTemplate,
        AsmRule::TemplateNotString,
        AsmRule::UnmatchedClose,
        AsmRule::UnclosedPlaceholder,
        AsmRule::BadPlaceholder,
        AsmRule::ModifierNotOneChar,
        AsmRule::NoSuchPositional,
        AsmRule::NoSuchName,
        AsmRule::UnusedOperand,
        AsmRule::PositionalAfterNamed,
        AsmRule::NamedExplicitRegister,
        AsmRule::DuplicateName,
        AsmRule::ClassOutputWithClobberAbi,
        AsmRule::ClobberAbiEmpty,
        AsmRule::NomemAndReadonly,
        AsmRule::PureNeedsNomemOrReadonly,
        AsmRule::PureWithoutOutputs,
        AsmRule::NoreturnWithOutputs,
        AsmRule::AttributeNotCfg,
        AsmRule::Malformed,
    ];

    /// Its code in a diagnostic's row ([`Node::c`]).
    pub(crate) fn code(self) -> u32 {
        let at = AsmRule::ALL.iter().position(|r| *r == self);
        at.map_or(NONE, idx)
    }

    pub(crate) fn of_code(code: u32) -> Option<AsmRule> {
        AsmRule::ALL.get(code as usize).copied()
    }
}

impl View<'_> {
    pub fn asm_form(&self, n: Node) -> Result<AsmForm, BadRow> {
        AsmForm::decode(n)
    }

    pub fn asm_piece(&self, n: Node) -> Result<AsmPiece, BadRow> {
        AsmPiece::decode(n, self.lens())
    }

    pub fn asm_operand(&self, n: Node) -> Result<AsmOperand, BadRow> {
        AsmOperand::decode(n)
    }
}

/// Why an `asm!` gives no form: a rule it breaks (a diagnostic) or a form not lowered.
#[derive(Clone, Copy, Debug)]
enum Refusal {
    Rule(AsmRule, Range),
    Form(Why, Range),
}

/// What a template expression is, read before its slot becomes a hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TemplateKind {
    Lit,
    Macro,
    Other,
}

const fn option_bit(k: SyntaxKind) -> Option<u16> {
    Some(match k {
        PURE_KW => AsmForm::PURE,
        NOMEM_KW => AsmForm::NOMEM,
        READONLY_KW => AsmForm::READONLY,
        PRESERVES_FLAGS_KW => AsmForm::PRESERVES_FLAGS,
        NORETURN_KW => AsmForm::NORETURN,
        NOSTACK_KW => AsmForm::NOSTACK,
        ATT_SYNTAX_KW => AsmForm::ATT_SYNTAX,
        RAW_KW => AsmForm::RAW,
        MAY_UNWIND_KW => AsmForm::MAY_UNWIND,
        _ => return None,
    })
}

const fn operand_kind(k: SyntaxKind) -> Option<AsmKind> {
    Some(match k {
        IN_KW => AsmKind::In,
        OUT_KW => AsmKind::Out,
        LATEOUT_KW => AsmKind::LateOut,
        INOUT_KW => AsmKind::InOut,
        INLATEOUT_KW => AsmKind::InLateOut,
        SYM_KW => AsmKind::Sym,
        CONST_KW => AsmKind::Const,
        LABEL_KW => AsmKind::Label,
        _ => return None,
    })
}

/// The bytes a string escape at `b[i]` (a `\`) takes, or `Err` when its character is a brace
/// (`\u{7b}`, `\x7D`), which a template's syntax would read: rustc parses the template's value,
/// and blaze keeps text by its range, so such a template is refused by name. An escape the
/// lexer refused is skipped as text (its error is the lexer's).
fn escape_len(b: &[u8], i: usize) -> Result<usize, ()> {
    let brace = |v: u32| v == u32::from(b'{') || v == u32::from(b'}');
    match b.get(i + 1) {
        Some(b'u') => {
            if b.get(i + 2) != Some(&b'{') {
                return Ok(2);
            }
            let Some(close) = (i + 3..b.len()).find(|&j| b[j] == b'}') else { return Ok(b.len() - i) };
            let mut v = 0u32;
            for &d in &b[i + 3..close] {
                if d == b'_' {
                    continue;
                }
                let Some(h) = char::from(d).to_digit(16) else { return Ok(close + 1 - i) };
                v = v.saturating_mul(16).saturating_add(h);
            }
            if brace(v) { Err(()) } else { Ok(close + 1 - i) }
        }
        Some(b'x') => {
            let hi = b.get(i + 2).and_then(|&d| char::from(d).to_digit(16));
            let lo = b.get(i + 3).and_then(|&d| char::from(d).to_digit(16));
            match (hi, lo) {
                (Some(hi), Some(lo)) if brace(hi * 16 + lo) => Err(()),
                _ => Ok(4),
            }
        }
        Some(_) => Ok(2),
        None => Ok(1),
    }
}

/// Whether `s` is an identifier a placeholder may name.
fn is_ident(s: &str) -> bool {
    let s = s.strip_prefix("r#").unwrap_or(s);
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c == '_' || c.is_alphabetic() => {}
        _ => return false,
    }
    s != "_" && cs.all(|c| c == '_' || c.is_alphanumeric())
}

/// The operand a placeholder's inside (`0`, `name:w`, ``) names, and its modifier.
fn placeholder(
    inner: &str,
    at: Range,
    named: &[(u64, u32)],
    npos: u32,
    next: &mut u32,
) -> Result<(u32, Option<char>), Refusal> {
    // alloc's fmt docs: "format := '{' [ argument ] [ ':' format_spec ] [ ws ] * '}'".
    let inner = inner.trim_end_matches(|c: char| c.is_ascii_whitespace());
    let (arg, spec) = match inner.find(':') {
        Some(c) => (&inner[..c], Some(&inner[c + 1..])),
        None => (inner, None),
    };
    let positional = |k: u32| if k < npos { Ok(k) } else { Err(Refusal::Rule(AsmRule::NoSuchPositional, at)) };
    let index = if arg.is_empty() {
        let k = *next;
        *next = next.saturating_add(1);
        positional(k)?
    } else if arg.bytes().all(|d| d.is_ascii_digit()) {
        let k = arg.parse::<u32>().map_err(|_| Refusal::Rule(AsmRule::NoSuchPositional, at))?;
        positional(k)?
    } else if is_ident(arg) {
        let hash = Name::of(arg.strip_prefix("r#").unwrap_or(arg)).0;
        match named.iter().find(|(h, _)| *h == hash) {
            Some(&(_, i)) => i,
            None => return Err(Refusal::Rule(AsmRule::NoSuchName, at)),
        }
    } else {
        return Err(Refusal::Rule(AsmRule::BadPlaceholder, at));
    };
    let modifier = match spec {
        None => None,
        Some(spec) => {
            let mut cs = spec.chars();
            match (cs.next(), cs.next()) {
                (None, _) => None,
                (Some(c), None) => Some(c),
                (Some(_), Some(_)) => return Err(Refusal::Rule(AsmRule::ModifierNotOneChar, at)),
            }
        }
    };
    Ok((index, modifier))
}

fn push_text(pieces: &mut AVec<'_, (Node, Range)>, at: u32, from: usize, to: usize, cooked: bool) {
    if to > from {
        let r = Range::new(at + idx(from), at + idx(to));
        pieces.push((AsmPiece::Text { text: BodyRange::building(r), cooked }.encode(), r));
    }
}

/// The pieces of one template's contents `s` (at `at` in the machine's text), each
/// placeholder's operand marked in `used`. `raw`: the `raw` option ("asm.options.supported-
/// options-raw": "the template string to be parsed as a raw assembly string, with no special
/// handling for { and }"). One pass, no recursion.
#[expect(clippy::too_many_arguments, reason = "one template's pass: its text, the operands' table, and the outputs")]
fn template_pieces(
    s: &str,
    at: u32,
    cooked: bool,
    raw: bool,
    named: &[(u64, u32)],
    npos: u32,
    next: &mut u32,
    used: &mut [bool],
    pieces: &mut AVec<'_, (Node, Range)>,
) -> Result<(), Refusal> {
    let b = s.as_bytes();
    let n = b.len();
    let abs = |i: usize| at + idx(i);
    if raw {
        push_text(pieces, at, 0, n, cooked);
        return Ok(());
    }
    let mut i = 0usize;
    let mut from = 0usize;
    while i < n {
        match b[i] {
            b'\\' if cooked => match escape_len(b, i) {
                Ok(len) => i = (i + len).min(n),
                Err(()) => {
                    let end = (i + 2).min(n);
                    return Err(Refusal::Form(Why::AsmTemplateEscape, Range::new(abs(i), abs(end))));
                }
            },
            b'{' if b.get(i + 1) == Some(&b'{') => {
                push_text(pieces, at, from, i + 1, cooked);
                i += 2;
                from = i;
            }
            b'{' => {
                push_text(pieces, at, from, i, cooked);
                let stop = (i + 1..n).find(|&j| matches!(b[j], b'}' | b'{' | b'\\'));
                let j = match stop {
                    Some(j) if b[j] == b'}' => j,
                    Some(j) if b[j] == b'\\' => {
                        return Err(Refusal::Form(Why::AsmTemplateEscape, Range::new(abs(i), abs(j + 1))));
                    }
                    _ => return Err(Refusal::Rule(AsmRule::UnclosedPlaceholder, Range::new(abs(i), abs(n)))),
                };
                let place = Range::new(abs(i), abs(j + 1));
                // `i + 1` follows an ASCII `{` and `j` is an ASCII `}`: both char boundaries.
                let (index, modifier) = placeholder(&s[i + 1..j], place, named, npos, next)?;
                if let Some(u) = used.get_mut(index as usize) {
                    *u = true;
                }
                let piece = AsmPiece::Operand { index, modifier, at: BodyRange::building(place) };
                pieces.push((piece.encode(), place));
                i = j + 1;
                from = i;
            }
            b'}' if b.get(i + 1) == Some(&b'}') => {
                push_text(pieces, at, from, i + 1, cooked);
                i += 2;
                from = i;
            }
            b'}' => return Err(Refusal::Rule(AsmRule::UnmatchedClose, Range::new(abs(i), abs(i + 1)))),
            _ => i += 1,
        }
    }
    push_text(pieces, at, from, n, cooked);
    Ok(())
}

impl Machine<'_, '_, '_> {
    /// Packs a closed `ASM_OPERAND_NAMED`, `ASM_CLOBBER_ABI` or `ASM_OPTIONS` into one value
    /// (an unplaced row of `ROW_ASM_OPERAND`, `ROW_ASM_CLOBBER` per ABI, or `ROW_ASM_OPTIONS`)
    /// for its `ASM_EXPR`. A part refused here writes its diagnostic and leaves a value marked
    /// [`BROKEN`], so the `asm!` gives no form and says nothing twice.
    pub(crate) fn asm_part(&mut self, f: &Frame, r: Range) {
        let base = f.base as usize;
        match f.kind {
            ASM_OPTIONS => {
                let mut bits = 0u16;
                let mut rule = None;
                for v in &self.vals[base..] {
                    match v.v {
                        V::Tok(k) => match option_bit(k) {
                            Some(bit) => bits |= bit,
                            None if matches!(k, OPTIONS_KW | L_PAREN | R_PAREN | COMMA) => {}
                            None => rule = rule.or(Some(AsmRule::Malformed)),
                        },
                        V::Row(ROW_ATTR) => rule = rule.or(Some(AsmRule::AttributeNotCfg)),
                        _ => rule = rule.or(Some(AsmRule::Malformed)),
                    }
                }
                self.vals.truncate(base);
                let mut node = Node::of(ROW_ASM_OPTIONS);
                node.flags = bits;
                self.close_asm_part(node, rule, r);
            }
            ASM_CLOBBER_ABI => {
                let alloc = *self.vals.allocator();
                let mut strings: AVec<'_, Range> = Vec::new_in(alloc);
                let mut rule = None;
                for v in &self.vals[base..] {
                    match v.v {
                        V::Tok(STRING) => strings.push(v.range),
                        V::Tok(CLOBBER_ABI_KW | L_PAREN | R_PAREN | COMMA) => {}
                        V::Row(ROW_ATTR) => rule = rule.or(Some(AsmRule::AttributeNotCfg)),
                        _ => rule = rule.or(Some(AsmRule::Malformed)),
                    }
                }
                self.vals.truncate(base);
                if rule.is_none() && strings.is_empty() {
                    rule = Some(AsmRule::ClobberAbiEmpty);
                }
                if rule.is_some() {
                    self.close_asm_part(Node::of(ROW_ASM_CLOBBER), rule, r);
                    return;
                }
                for &s in strings.iter() {
                    match self.string_name(s) {
                        Some(name) => {
                            let mut node = Node::of(ROW_ASM_CLOBBER);
                            node.a = name.0;
                            self.close_asm_part(node, None, s);
                        }
                        None => self.close_asm_part(Node::of(ROW_ASM_CLOBBER), Some(AsmRule::Malformed), s),
                    }
                }
            }
            _ => self.asm_operand_part(f, r),
        }
    }

    /// `ASM_OPERAND_NAMED`: `[name =] kind (reg) expr [=> expr]`, `sym path`, `const expr` or
    /// `label { .. }`, its inner nodes read through.
    fn asm_operand_part(&mut self, f: &Frame, r: Range) {
        let base = f.base as usize;
        let mut name = None;
        let mut kind = None;
        let mut class = None;
        let mut explicit = None;
        let mut exprs: [Option<ExprId>; 2] = [None, None];
        let mut n_exprs = 0usize;
        let mut path = None;
        let mut rule = None;
        for v in &self.vals[base..] {
            match v.v {
                V::Name => name = Some(NameIdx(v.id)),
                V::Tok(k) => match (operand_kind(k), k) {
                    (Some(op), _) if kind.is_none() => kind = Some(op),
                    (None, STRING) if explicit.is_none() => explicit = Some(v.range),
                    (None, EQ | L_PAREN | R_PAREN | FAT_ARROW) => {}
                    _ => rule = rule.or(Some(AsmRule::Malformed)),
                },
                V::NameRef(_) if class.is_none() => class = Some(NameIdx(v.id)),
                V::Expr => {
                    match exprs.get_mut(n_exprs) {
                        Some(slot) => *slot = Some(ExprId(v.id)),
                        None => rule = rule.or(Some(AsmRule::Malformed)),
                    }
                    n_exprs += 1;
                }
                V::Path if path.is_none() => path = Some((PathIdx(v.id), v.range)),
                V::Row(ROW_ATTR) => rule = rule.or(Some(AsmRule::AttributeNotCfg)),
                _ => rule = rule.or(Some(AsmRule::Malformed)),
            }
        }
        self.vals.truncate(base);
        let reg = match (class, explicit) {
            (Some(c), None) => Some(AsmReg::Class(c)),
            (None, Some(s)) => match self.string_name(s) {
                Some(n) => Some(AsmReg::Explicit(n)),
                None => {
                    rule = rule.or(Some(AsmRule::Malformed));
                    None
                }
            },
            (None, None) => None,
            (Some(_), Some(_)) => {
                rule = rule.or(Some(AsmRule::Malformed));
                None
            }
        };
        let shape = match (kind, reg, exprs, path) {
            (Some(k @ (AsmKind::In | AsmKind::Out | AsmKind::LateOut)), Some(_), [Some(e), None], None) => {
                Some((k, e, None))
            }
            (Some(k @ (AsmKind::InOut | AsmKind::InLateOut)), Some(_), [Some(e), out], None) => Some((k, e, out)),
            (Some(k @ (AsmKind::Const | AsmKind::Label)), None, [Some(e), None], None) => Some((k, e, None)),
            (Some(AsmKind::Sym), None, [None, None], Some((p, pr))) => {
                // The Reference, "`sym <path>`": the path is resolved as a value's; a local
                // it names is the checker's refusal.
                let e = self.path_expr(p);
                Some((AsmKind::Sym, self.put_expr(e, pr), None))
            }
            _ => None,
        };
        match (shape, rule) {
            (Some((kind, expr, out_expr)), None) => {
                let node = AsmOperand { name, kind, reg, expr, out_expr }.encode();
                self.close_asm_part(node, None, r);
            }
            (_, rule) => self.close_asm_part(Node::of(ROW_ASM_OPERAND), Some(rule.unwrap_or(AsmRule::Malformed)), r),
        }
    }

    fn close_asm_part(&mut self, mut node: Node, rule: Option<AsmRule>, r: Range) {
        if let Some(rule) = rule {
            self.diag(DiagKind::Asm(rule), r);
            node.e = BROKEN;
        }
        self.vals.push(Val { v: V::Row(node.tag), id: NONE, node, range: r });
    }

    /// The name of a string token's contents (an explicit register, an ABI).
    fn string_name(&mut self, tok: Range) -> Option<NameIdx> {
        let (cs, ce) = crate::fmt::string_contents(tok.of(self.text()))?;
        Some(self.name(Range::new(tok.start + idx(cs), tok.start + idx(ce))))
    }

    /// `builtin # asm(..)`: its templates, then its parts as [`Machine::asm_part`] packed them.
    /// Reads and clears the frame's values; the caller places the expression returned.
    pub(crate) fn asm_expr(&mut self, f: &Frame, r: Range) -> Expr {
        let base = f.base as usize;
        let alloc = *self.vals.allocator();
        let mut keyword = None;
        let mut templates: AVec<'_, (ExprId, Range)> = Vec::new_in(alloc);
        let mut operands: AVec<'_, (Node, Range)> = Vec::new_in(alloc);
        let mut clobbers: AVec<'_, u32> = Vec::new_in(alloc);
        let mut options = 0u16;
        let mut broken = false;
        let mut malformed = false;
        for v in &self.vals[base..] {
            match v.v {
                V::Tok(k @ (ASM_KW | GLOBAL_ASM_KW | NAKED_ASM_KW)) => keyword = Some(k),
                V::Tok(BUILTIN_KW | POUND | L_PAREN | R_PAREN | COMMA) => {}
                V::Expr => templates.push((ExprId(v.id), v.range)),
                V::Row(ROW_ASM_OPERAND | ROW_ASM_OPTIONS | ROW_ASM_CLOBBER) if v.node.e == BROKEN => broken = true,
                V::Row(ROW_ASM_OPERAND) => operands.push((v.node, v.range)),
                V::Row(ROW_ASM_OPTIONS) => options |= v.node.flags,
                V::Row(ROW_ASM_CLOBBER) => clobbers.push(v.node.a),
                _ => malformed = true,
            }
        }
        self.vals.truncate(base);
        // Only a template's text is read: no node holds its literal, so its slot becomes a
        // hole the checker and the writeback skip (as `fmt.rs` does for `format_args!`).
        let mut kinds: AVec<'_, TemplateKind> = Vec::new_in(alloc);
        for &(t, _) in templates.iter() {
            let kind = match Expr::decode(self.out.exprs[t.0 as usize]) {
                Ok(Expr::Lit(_)) => TemplateKind::Lit,
                Ok(Expr::MacroCall { .. }) => TemplateKind::Macro,
                _ => TemplateKind::Other,
            };
            kinds.push(kind);
            self.out.exprs[t.0 as usize] = Expr::Missing.encode();
        }
        match keyword {
            Some(ASM_KW) => {}
            Some(_) => {
                self.unhandled(f.kind, r, Why::Asm);
                return Expr::Missing;
            }
            None => malformed = true,
        }
        // The target's architecture, a read of the record (main's ruling (A), P9b).
        let arch = self.asm_arch(r);
        if broken {
            return Expr::Missing;
        }
        if malformed {
            self.diag(DiagKind::Asm(AsmRule::Malformed), r);
            return Expr::Missing;
        }
        match self.asm_form(&templates, &kinds, &operands, options, &clobbers, arch, r) {
            Ok(row) => Expr::Asm { row },
            Err(Refusal::Rule(rule, at)) => {
                self.diag(DiagKind::Asm(rule), at);
                Expr::Missing
            }
            Err(Refusal::Form(why, at)) => {
                self.unhandled(f.kind, at, why);
                Expr::Missing
            }
        }
    }

    /// The checks the Reference makes from the syntax, then the pieces and the row placed.
    fn asm_form(
        &mut self,
        templates: &[(ExprId, Range)],
        kinds: &[TemplateKind],
        operands: &[(Node, Range)],
        options: u16,
        clobbers: &[u32],
        arch: Option<blaze_typeck::AsmArch>,
        r: Range,
    ) -> Result<RowIdx, Refusal> {
        if templates.is_empty() {
            return Err(Refusal::Rule(AsmRule::NoTemplate, r));
        }
        let alloc = *self.vals.allocator();
        let mut named: AVec<'_, (u64, u32)> = Vec::new_in(alloc);
        let mut used: AVec<'_, bool> = Vec::new_in(alloc);
        let mut npos = 0u32;
        let mut past_positional = false;
        let mut outputs = 0u32;
        let mut kept_outputs = 0u32;
        let mut labels = 0u32;
        for (i, &(node, range)) in operands.iter().enumerate() {
            let op = AsmOperand::decode(node).map_err(|_| Refusal::Rule(AsmRule::Malformed, range))?;
            let explicit = matches!(op.reg, Some(AsmReg::Explicit(_)));
            match op.name {
                Some(n) => {
                    if explicit {
                        return Err(Refusal::Rule(AsmRule::NamedExplicitRegister, range));
                    }
                    let hash = self.out.names[n.0 as usize];
                    if named.iter().any(|&(h, _)| h == hash) {
                        return Err(Refusal::Rule(AsmRule::DuplicateName, range));
                    }
                    named.push((hash, idx(i)));
                    past_positional = true;
                }
                None if explicit => past_positional = true,
                None => {
                    if past_positional {
                        return Err(Refusal::Rule(AsmRule::PositionalAfterNamed, range));
                    }
                    npos += 1;
                }
            }
            // An explicit register is used by its name in the text, never by a placeholder.
            used.push(explicit);
            if op.kind.is_output() {
                outputs += 1;
                if !clobbers.is_empty() && !explicit {
                    return Err(Refusal::Rule(AsmRule::ClassOutputWithClobberAbi, range));
                }
                let place = match (op.kind, op.out_expr) {
                    (AsmKind::InOut | AsmKind::InLateOut, Some(out)) => out,
                    _ => op.expr,
                };
                if !matches!(Expr::decode(self.out.exprs[place.0 as usize]), Ok(Expr::Underscore)) {
                    kept_outputs += 1;
                }
            }
            if op.kind == AsmKind::Label {
                labels += 1;
            }
        }
        let has = |bit: u16| options & bit != 0;
        if has(AsmForm::NOMEM) && has(AsmForm::READONLY) {
            return Err(Refusal::Rule(AsmRule::NomemAndReadonly, r));
        }
        if has(AsmForm::PURE) && !has(AsmForm::NOMEM) && !has(AsmForm::READONLY) {
            return Err(Refusal::Rule(AsmRule::PureNeedsNomemOrReadonly, r));
        }
        if has(AsmForm::PURE) && kept_outputs == 0 {
            return Err(Refusal::Rule(AsmRule::PureWithoutOutputs, r));
        }
        if has(AsmForm::NORETURN) && outputs > 0 && labels == 0 {
            return Err(Refusal::Rule(AsmRule::NoreturnWithOutputs, r));
        }
        // The templates, in order, one `Line` between two; `{}` counts on across them
        // ("treated as if all the strings were concatenated").
        let pe = crate::parser_edition(self.edition);
        let text = self.text();
        let mut pieces: AVec<'_, (Node, Range)> = Vec::new_in(alloc);
        let mut next = 0u32;
        for (k, (&(_, tr), &kind)) in templates.iter().zip(kinds.iter()).enumerate() {
            match kind {
                TemplateKind::Lit => {}
                TemplateKind::Macro => return Err(Refusal::Form(Why::NonStringTemplate, tr)),
                TemplateKind::Other => return Err(Refusal::Rule(AsmRule::TemplateNotString, tr)),
            }
            // The literal is the expression's last token (a `cfg` attribute may stand first).
            let lexed = LexedStr::new(pe, tr.of(text));
            let last = (0..lexed.len()).rev().find(|&i| !lexed.kind(i).is_trivia() && lexed.kind(i) != EOF);
            let Some(last) = last.filter(|&i| lexed.kind(i) == STRING) else {
                return Err(Refusal::Rule(AsmRule::TemplateNotString, tr));
            };
            let tok = lexed.text(last);
            let Some((cs, ce)) = crate::fmt::string_contents(tok) else {
                return Err(Refusal::Rule(AsmRule::TemplateNotString, tr));
            };
            let at = tr.start + idx(lexed.text_start(last) + cs);
            if k > 0 {
                pieces.push((AsmPiece::Line.encode(), Range::new(at, at)));
            }
            let cooked = !tok.starts_with('r');
            let raw = has(AsmForm::RAW);
            template_pieces(&tok[cs..ce], at, cooked, raw, &named, npos, &mut next, &mut used, &mut pieces)?;
        }
        if let Some(i) = used.iter().position(|u| !*u) {
            return Err(Refusal::Rule(AsmRule::UnusedOperand, operands[i].1));
        }
        let pieces = self.put_rows_of(&pieces);
        let operands = self.put_rows_of(operands);
        let clobber_abis = self.put_ids_of(clobbers);
        Ok(self.put_row(AsmForm { pieces, operands, options, clobber_abis, arch }.encode(), r))
    }
}
