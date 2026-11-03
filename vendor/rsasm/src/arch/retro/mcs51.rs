//! The Intel 8051 (MCS-51), in the Intel mnemonics ASM51 defined.
//!
//! # Where the table comes from
//!
//! Intel, *MCS-51 Microcontroller Family User's Manual* (order number
//! 272383): the instruction-set chapter, whose per-instruction entries give
//! each opcode's bit pattern with its `rrr` register, `i` indirect-register
//! and `aaa` page fields. The whole 256-entry map is written out in
//! [`for_each_opcode`], which the completeness test walks: 255 opcodes are
//! defined, and only A5H is not.
//!
//! Every encoding here was checked against the Macro Assembler AS (`asl -cpu
//! 8051`) and against SDCC's `sdas8051`; see `tools/xas-diff`.
//!
//! # What is special about this machine
//!
//! **Bit addressing.** Sixteen bytes of internal RAM (20H to 2FH) and the
//! special function registers whose address is a multiple of 8 have their
//! 128 + 128 bits numbered in one 8-bit space, and `SETB`, `CLR`, `CPL`,
//! `MOV C,`, `JB`, `JNB` and `JBC` take a number in it. Source writes such an
//! address either directly or as `P1.3`, which is the dialect's
//! `BinOp::BitAddr` operator and is computed wherever an expression is.
//!
//! **Paged jumps.** `AJMP` and `ACALL` hold 11 bits of target and take the
//! top five from the PC *after* the instruction, so the target has to be in
//! the same 2 KiB block as the byte following the jump — not within 2 KiB of
//! it. That is [`LinkValue::Region`], the same rule as a MIPS `j`, and it is
//! what makes the generic `JMP` and `CALL` fall back to the long forms.
//!
//! **Big-endian instruction fields, little-endian data.** `LJMP 1234H` is
//! `02 12 34` and `MOV DPTR,#1234H` is `90 12 34`, while AS's `DW 1234H` is
//! `34 12`. So the machine is little-endian as far as the assembler's data
//! directives go, and the two 16-bit instruction fields byte-swap on the way
//! into the encoding. (SDCC's `sdas8051` writes `.dw` the other way round;
//! AS is the reference the 8-bit dialect follows, here as elsewhere.)
//!
//! **Generic jumps are sized as AS sizes them.** AS lays a program out again
//! on every pass, choosing each `JMP` and `CALL` afresh from the addresses of
//! the pass before, so a `CALL` that one pass pushed into the next block can
//! come back to an `ACALL` once a jump before it has grown. rsasm uses
//! [`Relaxation::Shrinking`](crate::arch::Relaxation::Shrinking) for the same
//! reason: growing only, it would keep the `LCALL`.
//!
//! # Where rsasm and the references part
//!
//! AS is followed where the two references disagree: its `CY` for the
//! carry, its `DW` byte order, its refusal of a branch target below address
//! 0, which sdas8051 lets wrap, and its refusal of an operand that does not
//! fit, which sdas8051 truncates. rsasm departs from both, and says so where
//! the code is, in three places:
//!
//! - an `AJMP` or `ACALL` in the last two bytes of a 2 KiB block reaches the
//!   block after it, as on the CPU; see `Enc::addr11`;
//! - a bit of a byte that has no bit addresses (`30H.1`, `SBUF.1`) is refused,
//!   where AS assembles a bit of some other byte; see
//!   `expr::eval_bit_address`;
//! - `SETB A` is refused, where AS assembles `DA A`.

use super::common::{self, Enc};
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::section::{FixupKind, LinkValue, Variant};
use crate::source::Span;

/// One operand, as the MCS-51 grammar distinguishes them.
#[derive(Copy, Clone)]
enum Arg {
    /// The accumulator, written `A`. `ACC` is the SFR at E0H and a direct
    /// address instead, which is what makes `MOV ACC,ACC` a `MOV
    /// direct,direct`.
    A,
    /// The carry flag, written `C`, which is bit D7H under another name.
    C,
    /// `CY`, which AS takes for the carry flag wherever `C` could stand —
    /// either operand of `MOV`, the first of `ANL` and `ORL`, and the operand
    /// of `CLR`, `SETB` and `CPL` — and for the bit symbol D7H, which is the
    /// same bit, anywhere else. The two readings assemble differently: `CPL
    /// CY` is `B3`, not `B2 D7`.
    Cy(ExprRef),
    /// `AB`, the accumulator and B register that `MUL` and `DIV` use.
    Ab,
    /// The data pointer, `DPTR`.
    Dptr,
    /// `R0` to `R7` of the active register bank.
    R(u8),
    /// `@R0` or `@R1`, an 8-bit indirect internal-RAM address.
    AtR(u8),
    /// `@DPTR`, the 16-bit indirect external-RAM address.
    AtDptr,
    /// `@A+DPTR`.
    AtADptr,
    /// `@A+PC`.
    AtAPc,
    /// `#value`.
    Imm(ExprRef),
    /// `/bit`, the complement of a bit, which only `ANL C,` and `ORL C,`
    /// take.
    NotBit(ExprRef),
    /// Anything else: a direct address, a bit address or a branch target,
    /// told apart by the instruction rather than by how it is written.
    Value(ExprRef),
}

struct Operand {
    arg: Arg,
    span: Span,
}

impl Operand {
    /// A direct address, a bit address or a branch target.
    fn value(&self) -> Option<ExprRef> {
        match self.arg {
            Arg::Value(e) | Arg::Cy(e) => Some(e),
            _ => None,
        }
    }

    fn is_a(&self) -> bool {
        matches!(self.arg, Arg::A)
    }

    fn is_c(&self) -> bool {
        matches!(self.arg, Arg::C | Arg::Cy(_))
    }

    /// The operand with `CY` read as the bit symbol, for a position where
    /// the carry flag cannot stand.
    fn as_value(&self) -> Operand {
        let arg = match self.arg {
            Arg::Cy(e) => Arg::Value(e),
            arg => arg,
        };
        Operand {
            arg,
            span: self.span,
        }
    }
}

/// The register and register-like words, which are reserved: `MOV A,C` is an
/// error rather than a load of an undefined symbol named `c`.
fn register_word(name: &str) -> Option<Arg> {
    Some(match name {
        "a" => Arg::A,
        "c" => Arg::C,
        "ab" => Arg::Ab,
        "dptr" => Arg::Dptr,
        "r0" => Arg::R(0),
        "r1" => Arg::R(1),
        "r2" => Arg::R(2),
        "r3" => Arg::R(3),
        "r4" => Arg::R(4),
        "r5" => Arg::R(5),
        "r6" => Arg::R(6),
        "r7" => Arg::R(7),
        _ => return None,
    })
}

/// Parses one operand's tokens.
fn parse_operand(cx: &mut AsmCtx<'_>, toks: &[Token], fallback: Span) -> Option<Operand> {
    let span = common::span_of(toks, fallback);
    let arg = match toks.first().map(|t| t.kind) {
        Some(TokKind::Punct(Punct::Hash)) => Arg::Imm(common::expr_of(cx, &toks[1..], span)?),
        // `ANL C,/P1.0` and `ORL C,/P1.0`: the bit's complement.
        Some(TokKind::Punct(Punct::Slash)) => Arg::NotBit(common::expr_of(cx, &toks[1..], span)?),
        Some(TokKind::Punct(Punct::At)) => return indirect(cx, &toks[1..], span),
        _ => match common::sole_ident(cx, toks).as_deref() {
            Some("cy") => Arg::Cy(common::expr_of(cx, toks, span)?),
            name => match name.and_then(register_word) {
                Some(arg) => arg,
                None => Arg::Value(common::expr_of(cx, toks, span)?),
            },
        },
    };
    Some(Operand { arg, span })
}

/// The `@` forms: `@R0`, `@R1`, `@DPTR`, `@A+DPTR` and `@A+PC`.
fn indirect(cx: &mut AsmCtx<'_>, toks: &[Token], span: Span) -> Option<Operand> {
    let words: Vec<String> = toks
        .iter()
        .map(|t| match (t.ident(), t.kind) {
            (Some(n), _) => cx.name(n).to_ascii_lowercase(),
            (None, TokKind::Punct(p)) => p.as_str().to_string(),
            _ => String::new(),
        })
        .collect();
    let parts: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
    let arg = match parts.as_slice() {
        ["r0"] => Arg::AtR(0),
        ["r1"] => Arg::AtR(1),
        ["dptr"] => Arg::AtDptr,
        ["a", "+", "dptr"] => Arg::AtADptr,
        ["a", "+", "pc"] => Arg::AtAPc,
        _ => {
            cx.error(
                span,
                "expected `@R0`, `@R1`, `@DPTR`, `@A+DPTR` or `@A+PC` after `@`",
            );
            return None;
        }
    };
    Some(Operand { arg, span })
}

/// A 16-bit address or immediate as an instruction holds it: high byte
/// first, where the machine's data directives are little-endian. The two
/// bytes are read back little-endian, so the halves swap places.
fn swapped16(_word: u64, v: i64) -> u64 {
    let v = v as u64;
    ((v & 0xff) << 8) | ((v >> 8) & 0xff)
}

/// `AJMP` and `ACALL`: address bits 10 to 8 replace the opcode's top three,
/// and bits 7 to 0 are the second byte. Read little-endian, the opcode is the
/// low half of the pair, so its own five bits are kept from what is there.
fn paged11(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & 0x1f) | ((v & 0x700) >> 3) | ((v & 0xff) << 8)
}

/// The whole address space, which a branch target has to be in. AS reads a
/// relative target as an unsigned address, so a branch to below 0 is refused
/// rather than let the PC wrap, and a displacement alone cannot tell.
const ADDRESS_SPACE: LinkValue = LinkValue::Region {
    bits: 16,
    numbers: true,
};

impl Enc {
    /// A 16-bit address or immediate, high byte first, after the opcode:
    /// `LJMP`, `LCALL` and `MOV DPTR,#`. Like any 16-bit field it takes
    /// -32768 to 65535, which is also what AS takes for these.
    fn word16(&mut self, e: ExprRef, span: Span) {
        self.push_field(e, span, 0, FixupKind::data(2).scatter(swapped16));
    }

    /// The `LJMP` or `LCALL` a generic `JMP` or `CALL` becomes, whose target
    /// AS reads as an unsigned address in the 64 KiB after the instruction.
    fn far16(&mut self, e: ExprRef, span: Span) {
        let kind = FixupKind::data(2).scatter(swapped16).link(ADDRESS_SPACE);
        self.push_field(e, span, 0, kind);
    }

    /// A relative branch target, which has to be both in reach and in the
    /// address space.
    fn branch8(&mut self, e: ExprRef, span: Span) {
        self.fixups.push(crate::section::Fixup {
            offset: self.bytes.len() as u32,
            expr: e,
            kind: FixupKind::pcrel(1, 1).link(ADDRESS_SPACE),
            span,
        });
        self.bytes.push(0);
    }

    /// The 11-bit target of `AJMP` or `ACALL`, which has to share the 2 KiB
    /// block of the byte after the instruction, as the CPU takes the block
    /// from the PC once it has moved past the two bytes. The field covers the
    /// opcode byte too, since three of its bits live there.
    ///
    /// Both references test the instruction's own address instead, for the
    /// explicit mnemonics — AS's generic `JMP` and `CALL`, and its 80C390
    /// `AJMP`, test the address after it — so they differ from rsasm for an
    /// `AJMP` or `ACALL` that starts at one of the last two addresses of a
    /// block (07FEH or 07FFH, 0FFEH or 0FFFH, and so on): there they
    /// assemble a jump into the block being left, which the CPU takes into
    /// the next, and refuse one into the next. rsasm assembles what the CPU
    /// does, the same bytes AS gives a `JMP` in that place.
    fn addr11(&mut self, e: ExprRef, span: Span) {
        self.push_field(
            e,
            span,
            1,
            FixupKind::data(2).scatter(paged11).link(LinkValue::Region {
                bits: 11,
                numbers: true,
            }),
        );
    }

    /// Adds a two-byte field starting `back` bytes before the end of what has
    /// been emitted, and reserves the bytes it does not already cover.
    fn push_field(&mut self, e: ExprRef, span: Span, back: u32, kind: FixupKind) {
        let offset = self.bytes.len() as u32 - back;
        self.fixups.push(crate::section::Fixup {
            offset,
            expr: e,
            kind,
            span,
        });
        self.bytes.resize(offset as usize + 2, 0);
    }
}

pub fn assemble(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>, m: &str) -> Option<Vec<Variant>> {
    let parts = common::operands(insn.operands);
    let mut args = Vec::with_capacity(parts.len());
    for part in &parts {
        args.push(parse_operand(cx, part, insn.span)?);
    }
    encode(cx, insn, m, &args)
}

fn encode(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    args: &[Operand],
) -> Option<Vec<Variant>> {
    // Only these have a place for the carry flag; everywhere else `CY` is
    // the bit symbol.
    if !matches!(m, "mov" | "orl" | "anl" | "clr" | "setb" | "cpl") {
        let plain: Vec<Operand> = args.iter().map(Operand::as_value).collect();
        return encode_plain(cx, insn, m, &plain);
    }
    encode_plain(cx, insn, m, args)
}

fn encode_plain(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    args: &[Operand],
) -> Option<Vec<Variant>> {
    let span = insn.span;

    if let Some((_, op)) = NULLARY.iter().find(|(n, _)| *n == m) {
        if !args.is_empty() {
            cx.error(span, format!("`{m}` takes no operands"));
            return None;
        }
        return Enc::op(&[*op]).done();
    }
    // `RL A`, `MUL AB` and the rest: one fixed operand and no encoding in it.
    if let Some((_, word, op)) = FIXED_OPERAND.iter().find(|(n, _, _)| *n == m) {
        let [arg] = args else {
            return common::bad_operands(cx, span, m);
        };
        let ok = match *word {
            "a" => arg.is_a(),
            _ => matches!(arg.arg, Arg::Ab),
        };
        if !ok {
            return common::bad_operands(cx, span, m);
        }
        return Enc::op(&[*op]).done();
    }

    if let Some(base) = alu_base(m) {
        return alu(cx, insn, m, args, base);
    }
    if let Some(op) = branch_op(m) {
        let [target] = args else {
            return common::bad_operands(cx, span, m);
        };
        let Some(e) = target.value() else {
            return common::bad_operands(cx, span, m);
        };
        let mut enc = Enc::op(&[op]);
        enc.branch8(e, target.span);
        return enc.done();
    }
    if let Some(op) = bit_branch_op(m) {
        let [bit, target] = args else {
            return common::bad_operands(cx, span, m);
        };
        let (Some(b), Some(t)) = (bit.value(), target.value()) else {
            return common::bad_operands(cx, span, m);
        };
        let mut enc = Enc::op(&[op]);
        enc.addr8(b, bit.span);
        enc.branch8(t, target.span);
        return enc.done();
    }

    match m {
        "mov" => mov(cx, insn, args),
        "movc" => match args {
            [a, src] if a.is_a() => match src.arg {
                Arg::AtADptr => Enc::op(&[0x93]).done(),
                Arg::AtAPc => Enc::op(&[0x83]).done(),
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "movx" => match args {
            [a, src] if a.is_a() => match src.arg {
                Arg::AtDptr => Enc::op(&[0xe0]).done(),
                Arg::AtR(i) => Enc::op(&[0xe2 + i]).done(),
                _ => common::bad_operands(cx, span, m),
            },
            [dst, a] if a.is_a() => match dst.arg {
                Arg::AtDptr => Enc::op(&[0xf0]).done(),
                Arg::AtR(i) => Enc::op(&[0xf2 + i]).done(),
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "inc" | "dec" => {
            let base: u8 = if m == "inc" { 0x00 } else { 0x10 };
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            match arg.arg {
                Arg::A => Enc::op(&[base | 0x04]).done(),
                Arg::R(n) => Enc::op(&[base | 0x08 | n]).done(),
                Arg::AtR(i) => Enc::op(&[base | 0x06 | i]).done(),
                // `INC DPTR` exists; `DEC DPTR` does not.
                Arg::Dptr if m == "inc" => Enc::op(&[0xa3]).done(),
                Arg::Value(e) => addr_operand(base | 0x05, e, arg.span),
                _ => common::bad_operands(cx, span, m),
            }
        }
        "push" | "pop" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = arg.value() else {
                return common::bad_operands(cx, span, m);
            };
            addr_operand(if m == "push" { 0xc0 } else { 0xd0 }, e, arg.span)
        }
        "xch" => match args {
            [a, src] if a.is_a() => match src.arg {
                Arg::R(n) => Enc::op(&[0xc8 | n]).done(),
                Arg::AtR(i) => Enc::op(&[0xc6 | i]).done(),
                Arg::Value(e) => addr_operand(0xc5, e, src.span),
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "xchd" => match args {
            [a, src] if a.is_a() => match src.arg {
                Arg::AtR(i) => Enc::op(&[0xd6 | i]).done(),
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "clr" | "setb" | "cpl" => bit_op(cx, insn, m, args),
        "cjne" => cjne(cx, insn, args),
        "djnz" => {
            let [arg, target] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(t) = target.value() else {
                return common::bad_operands(cx, span, m);
            };
            match arg.arg {
                Arg::R(n) => {
                    let mut enc = Enc::op(&[0xd8 | n]);
                    enc.branch8(t, target.span);
                    enc.done()
                }
                Arg::Value(d) => {
                    let mut enc = Enc::op(&[0xd5]);
                    enc.addr8(d, arg.span);
                    enc.branch8(t, target.span);
                    enc.done()
                }
                _ => common::bad_operands(cx, span, m),
            }
        }
        "ljmp" | "lcall" => {
            let [target] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = target.value() else {
                return common::bad_operands(cx, span, m);
            };
            let mut enc = Enc::op(&[if m == "ljmp" { 0x02 } else { 0x12 }]);
            enc.word16(e, target.span);
            enc.done()
        }
        "ajmp" | "acall" => {
            let [target] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = target.value() else {
                return common::bad_operands(cx, span, m);
            };
            let mut enc = Enc::op(&[if m == "ajmp" { 0x01 } else { 0x11 }]);
            enc.addr11(e, target.span);
            enc.done()
        }
        // The generic forms, which AS resolves by what reaches the target:
        // the shortest jump first, then the paged one, then the long one.
        "jmp" => {
            let [target] = args else {
                return common::bad_operands(cx, span, m);
            };
            if matches!(target.arg, Arg::AtADptr) {
                return Enc::op(&[0x73]).done();
            }
            let Some(e) = target.value() else {
                return common::bad_operands(cx, span, m);
            };
            Some(jump_variants(e, target.span, true))
        }
        "call" => {
            let [target] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = target.value() else {
                return common::bad_operands(cx, span, m);
            };
            Some(jump_variants(e, target.span, false))
        }
        _ => common::unknown(cx, insn.mnemonic_span, "8051", m),
    }
}

/// The candidates a generic `JMP` or `CALL` may become. Layout takes the
/// first that reaches, and they are ordered as AS orders them: `SJMP` where
/// the displacement fits, `AJMP`/`ACALL` where the target shares the 2 KiB
/// block after the instruction, and otherwise the long form. `CALL` has no
/// short form.
fn jump_variants(e: ExprRef, span: Span, jump: bool) -> Vec<Variant> {
    let mut out = Vec::new();
    if jump {
        let mut short = Enc::op(&[0x80]);
        short.branch8(e, span);
        out.push(short.into_variant());
    }
    let mut paged = Enc::op(&[if jump { 0x01 } else { 0x11 }]);
    paged.addr11(e, span);
    out.push(paged.into_variant());
    let mut long = Enc::op(&[if jump { 0x02 } else { 0x12 }]);
    long.far16(e, span);
    out.push(long.into_variant());
    out
}

/// `CLR`, `SETB` and `CPL`, which take `A` (not `SETB`), `C` or a bit.
fn bit_op(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    args: &[Operand],
) -> Option<Vec<Variant>> {
    let [arg] = args else {
        return common::bad_operands(cx, insn.span, m);
    };
    let (acc, carry, bit) = match m {
        "clr" => (Some(0xe4u8), 0xc3u8, 0xc2u8),
        "cpl" => (Some(0xf4), 0xb3, 0xb2),
        _ => (None, 0xd3, 0xd2),
    };
    match arg.arg {
        Arg::A => match acc {
            Some(op) => Enc::op(&[op]).done(),
            // `SETB A` does not exist: setting every bit of A is `MOV
            // A,#0FFH`. AS assembles it as `DA A` (D4H), from a test in its
            // `CPL`/`CLR`/`SETB` decoder that can never be true.
            None => common::bad_operands(cx, insn.span, m),
        },
        Arg::C | Arg::Cy(_) => Enc::op(&[carry]).done(),
        Arg::Value(e) => {
            let mut enc = Enc::op(&[bit]);
            enc.addr8(e, arg.span);
            enc.done()
        }
        _ => common::bad_operands(cx, insn.span, m),
    }
}

/// `CJNE`, whose first operand decides the opcode and whose last is always a
/// displacement.
fn cjne(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>, args: &[Operand]) -> Option<Vec<Variant>> {
    let [dst, src, target] = args else {
        return common::bad_operands(cx, insn.span, "cjne");
    };
    let Some(t) = target.value() else {
        return common::bad_operands(cx, insn.span, "cjne");
    };
    let (op, operand, operand_span) = match (&dst.arg, &src.arg) {
        (Arg::A, Arg::Imm(e)) => (0xb4, *e, src.span),
        (Arg::A, Arg::Value(e)) => (0xb5, *e, src.span),
        (Arg::R(n), Arg::Imm(e)) => (0xb8 | n, *e, src.span),
        (Arg::AtR(i), Arg::Imm(e)) => (0xb6 | i, *e, src.span),
        _ => return common::bad_operands(cx, insn.span, "cjne"),
    };
    let mut enc = Enc::op(&[op]);
    if op == 0xb5 {
        enc.addr8(operand, operand_span);
    } else {
        enc.imm8(operand, operand_span);
    }
    enc.branch8(t, target.span);
    enc.done()
}

/// `MOV`, which reaches every addressing mode the machine has.
fn mov(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>, args: &[Operand]) -> Option<Vec<Variant>> {
    let [dst, src] = args else {
        return common::bad_operands(cx, insn.span, "mov");
    };
    let bad = |cx: &mut AsmCtx<'_>| common::bad_operands(cx, insn.span, "mov");
    // A carry on either side makes the other a bit, as AS reads it: `MOV
    // A,C` is refused rather than taken as `MOV A,direct`.
    if dst.is_c() {
        return match src.value() {
            Some(e) => addr_operand(0xa2, e, src.span),
            None => bad(cx),
        };
    }
    if src.is_c() {
        return match dst.value() {
            Some(e) => addr_operand(0x92, e, dst.span),
            None => bad(cx),
        };
    }
    match (&dst.arg, &src.arg) {
        // `MOV DPTR,#nnnn`, the only 16-bit load.
        (Arg::Dptr, Arg::Imm(e)) => {
            let mut enc = Enc::op(&[0x90]);
            enc.word16(*e, src.span);
            enc.done()
        }
        (Arg::A, Arg::R(n)) => Enc::op(&[0xe8 | n]).done(),
        (Arg::A, Arg::AtR(i)) => Enc::op(&[0xe6 | i]).done(),
        (Arg::A, Arg::Imm(e)) => imm_operand(0x74, *e, src.span),
        (Arg::A, Arg::Value(e)) => addr_operand(0xe5, *e, src.span),
        (Arg::R(n), Arg::A) => Enc::op(&[0xf8 | n]).done(),
        (Arg::R(n), Arg::Imm(e)) => imm_operand(0x78 | n, *e, src.span),
        (Arg::R(n), Arg::Value(e)) => addr_operand(0xa8 | n, *e, src.span),
        (Arg::AtR(i), Arg::A) => Enc::op(&[0xf6 | i]).done(),
        (Arg::AtR(i), Arg::Imm(e)) => imm_operand(0x76 | i, *e, src.span),
        (Arg::AtR(i), Arg::Value(e)) => addr_operand(0xa6 | i, *e, src.span),
        (Arg::Value(e), Arg::A) => addr_operand(0xf5, *e, dst.span),
        (Arg::Value(e), Arg::R(n)) => addr_operand(0x88 | n, *e, dst.span),
        (Arg::Value(e), Arg::AtR(i)) => addr_operand(0x86 | i, *e, dst.span),
        (Arg::Value(e), Arg::Imm(v)) => {
            let mut enc = Enc::op(&[0x75]);
            enc.addr8(*e, dst.span);
            enc.imm8(*v, src.span);
            enc.done()
        }
        // `MOV direct,direct` carries the *source* first, which is the one
        // place the machine's operand order and its encoding disagree.
        (Arg::Value(d), Arg::Value(s)) => {
            let mut enc = Enc::op(&[0x85]);
            enc.addr8(*s, src.span);
            enc.addr8(*d, dst.span);
            enc.done()
        }
        _ => bad(cx),
    }
}

/// An opcode and an 8-bit immediate.
fn imm_operand(op: u8, e: ExprRef, span: Span) -> Option<Vec<Variant>> {
    let mut enc = Enc::op(&[op]);
    enc.imm8(e, span);
    enc.done()
}

/// An opcode and an 8-bit direct or bit address.
fn addr_operand(op: u8, e: ExprRef, span: Span) -> Option<Vec<Variant>> {
    let mut enc = Enc::op(&[op]);
    enc.addr8(e, span);
    enc.done()
}

/// The arithmetic and logical group's base opcode: the `A,src` forms are
/// `base | 4` for an immediate, `| 5` for a direct address, `| 6 + i` for
/// `@Ri` and `| 8 + n` for `Rn`.
fn alu_base(m: &str) -> Option<u8> {
    Some(match m {
        "add" => 0x20,
        "addc" => 0x30,
        "orl" => 0x40,
        "anl" => 0x50,
        "xrl" => 0x60,
        "subb" => 0x90,
        _ => return None,
    })
}

fn alu(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    args: &[Operand],
    base: u8,
) -> Option<Vec<Variant>> {
    let [dst, src] = args else {
        return common::bad_operands(cx, insn.span, m);
    };
    // Only the three logical operations have a `direct` destination and the
    // carry forms; `ADD`, `ADDC` and `SUBB` are accumulator-only.
    let logical = matches!(m, "orl" | "anl" | "xrl");
    if logical && dst.is_c() {
        // `XRL` has no carry form: the machine can only and or or into it.
        let (direct, complement) = match m {
            "orl" => (0x72u8, 0xa0u8),
            "anl" => (0x82, 0xb0),
            _ => return common::bad_operands(cx, insn.span, m),
        };
        return match &src.arg {
            Arg::Value(e) | Arg::Cy(e) => addr_operand(direct, *e, src.span),
            Arg::NotBit(e) => addr_operand(complement, *e, src.span),
            _ => common::bad_operands(cx, insn.span, m),
        };
    }
    let src = &src.as_value();
    if dst.is_a() {
        return match &src.arg {
            Arg::Imm(e) => imm_operand(base | 0x04, *e, src.span),
            Arg::Value(e) => addr_operand(base | 0x05, *e, src.span),
            Arg::AtR(i) => Enc::op(&[base | 0x06 | i]).done(),
            Arg::R(n) => Enc::op(&[base | 0x08 | n]).done(),
            _ => common::bad_operands(cx, insn.span, m),
        };
    }
    if logical && let Arg::Value(d) = &dst.arg {
        return match &src.arg {
            Arg::A => addr_operand(base | 0x02, *d, dst.span),
            Arg::Imm(v) => {
                let mut enc = Enc::op(&[base | 0x03]);
                enc.addr8(*d, dst.span);
                enc.imm8(*v, src.span);
                enc.done()
            }
            _ => common::bad_operands(cx, insn.span, m),
        };
    }
    common::bad_operands(cx, insn.span, m)
}

/// The conditional jumps that take only a displacement.
fn branch_op(m: &str) -> Option<u8> {
    Some(match m {
        "jc" => 0x40,
        "jnc" => 0x50,
        "jz" => 0x60,
        "jnz" => 0x70,
        "sjmp" => 0x80,
        _ => return None,
    })
}

/// The conditional jumps that test a bit.
fn bit_branch_op(m: &str) -> Option<u8> {
    Some(match m {
        "jbc" => 0x10,
        "jb" => 0x20,
        "jnb" => 0x30,
        _ => return None,
    })
}

/// The instructions with no operand at all.
const NULLARY: [(&str, u8); 3] = [("nop", 0x00), ("ret", 0x22), ("reti", 0x32)];

/// The instructions whose single operand is fixed: (mnemonic, the word it
/// must be, opcode).
const FIXED_OPERAND: [(&str, &str, u8); 8] = [
    ("rr", "a", 0x03),
    ("rrc", "a", 0x13),
    ("rl", "a", 0x23),
    ("rlc", "a", 0x33),
    ("swap", "a", 0xc4),
    ("da", "a", 0xd4),
    ("div", "ab", 0x84),
    ("mul", "ab", 0xa4),
];

/// Whether `name`, lowercased, is an 8051 mnemonic.
pub fn is_mnemonic(name: &str) -> bool {
    let mut found = false;
    for_each_opcode(|m, _, _| found |= m == name);
    found
}

/// The words that define a symbol where a label would go, which ASM51 spells
/// `BIT`, `DATA`, `IDATA`, `XDATA` and `CODE` after the name.
///
/// Each names an address space as well as a value, and a real MCS-51
/// toolchain checks that a symbol is used in the space it was declared in.
/// Nothing here can: the output is a flat image with no relocations, and
/// neither reference records the space either — AS's `SFR` and `BIT` are the
/// only two it has, and `sdas8051` has none. So all five define a plain
/// constant, exactly as `EQU` does, and AS's own `SFR` and `SFRB` are
/// accepted under the same rule so that its MCS-51 headers read.
pub const EQUATES: &[&str] = &["bit", "data", "idata", "xdata", "code", "sfr", "sfrb"];

/// Walks the whole 8051 instruction set, calling `f` with each (mnemonic,
/// operand-byte count, opcode). The completeness test uses this to check
/// that the 255 defined opcodes are all reachable and all distinct, and that
/// A5H is the only hole.
pub fn for_each_opcode(mut f: impl FnMut(&'static str, u8, u8)) {
    // The arithmetic and logical group fills six rows of the map.
    for (m, base) in [
        ("add", 0x20u8),
        ("addc", 0x30),
        ("orl", 0x40),
        ("anl", 0x50),
        ("xrl", 0x60),
        ("subb", 0x90),
    ] {
        f(m, 1, base | 0x04);
        f(m, 1, base | 0x05);
        for i in 0..2u8 {
            f(m, 0, base | 0x06 | i);
        }
        for n in 0..8u8 {
            f(m, 0, base | 0x08 | n);
        }
    }
    for (m, base) in [("orl", 0x40u8), ("anl", 0x50), ("xrl", 0x60)] {
        f(m, 1, base | 0x02);
        f(m, 2, base | 0x03);
    }
    f("orl", 1, 0x72);
    f("orl", 1, 0xa0);
    f("anl", 1, 0x82);
    f("anl", 1, 0xb0);

    // `INC` and `DEC`, and `INC DPTR`.
    for (m, base) in [("inc", 0x00u8), ("dec", 0x10)] {
        f(m, 0, base | 0x04);
        f(m, 1, base | 0x05);
        for i in 0..2u8 {
            f(m, 0, base | 0x06 | i);
        }
        for n in 0..8u8 {
            f(m, 0, base | 0x08 | n);
        }
    }
    f("inc", 0, 0xa3);

    // `MOV`.
    f("mov", 1, 0x74);
    f("mov", 1, 0xe5);
    for i in 0..2u8 {
        f("mov", 0, 0xe6 | i);
        f("mov", 0, 0xf6 | i);
        f("mov", 1, 0x76 | i);
        f("mov", 1, 0xa6 | i);
        f("mov", 1, 0x86 | i);
    }
    for n in 0..8u8 {
        f("mov", 0, 0xe8 | n);
        f("mov", 0, 0xf8 | n);
        f("mov", 1, 0x78 | n);
        f("mov", 1, 0xa8 | n);
        f("mov", 1, 0x88 | n);
    }
    f("mov", 1, 0xf5);
    f("mov", 2, 0x75);
    f("mov", 2, 0x85);
    f("mov", 2, 0x90);
    f("mov", 1, 0xa2);
    f("mov", 1, 0x92);

    f("movc", 0, 0x83);
    f("movc", 0, 0x93);
    f("movx", 0, 0xe0);
    f("movx", 0, 0xf0);
    for i in 0..2u8 {
        f("movx", 0, 0xe2 | i);
        f("movx", 0, 0xf2 | i);
    }

    f("push", 1, 0xc0);
    f("pop", 1, 0xd0);
    f("xch", 1, 0xc5);
    for i in 0..2u8 {
        f("xch", 0, 0xc6 | i);
        f("xchd", 0, 0xd6 | i);
    }
    for n in 0..8u8 {
        f("xch", 0, 0xc8 | n);
    }

    f("clr", 0, 0xe4);
    f("clr", 0, 0xc3);
    f("clr", 1, 0xc2);
    f("setb", 0, 0xd3);
    f("setb", 1, 0xd2);
    f("cpl", 0, 0xf4);
    f("cpl", 0, 0xb3);
    f("cpl", 1, 0xb2);

    f("rr", 0, 0x03);
    f("rrc", 0, 0x13);
    f("rl", 0, 0x23);
    f("rlc", 0, 0x33);
    f("swap", 0, 0xc4);
    f("da", 0, 0xd4);
    f("div", 0, 0x84);
    f("mul", 0, 0xa4);
    f("nop", 0, 0x00);
    f("ret", 0, 0x22);
    f("reti", 0, 0x32);

    // Jumps and calls.
    for page in 0..8u8 {
        f("ajmp", 1, 0x01 | (page << 5));
        f("acall", 1, 0x11 | (page << 5));
    }
    f("ljmp", 2, 0x02);
    f("lcall", 2, 0x12);
    f("sjmp", 1, 0x80);
    f("jmp", 0, 0x73);
    f("jc", 1, 0x40);
    f("jnc", 1, 0x50);
    f("jz", 1, 0x60);
    f("jnz", 1, 0x70);
    f("jbc", 2, 0x10);
    f("jb", 2, 0x20);
    f("jnb", 2, 0x30);
    f("cjne", 2, 0xb4);
    f("cjne", 2, 0xb5);
    for i in 0..2u8 {
        f("cjne", 2, 0xb6 | i);
    }
    for n in 0..8u8 {
        f("cjne", 2, 0xb8 | n);
    }
    f("djnz", 2, 0xd5);
    for n in 0..8u8 {
        f("djnz", 1, 0xd8 | n);
    }
}

/// The special function registers an 8051 has, by the names and addresses
/// the Macro Assembler AS's `stddef51.inc` gives them for `CPU 8051`, which
/// are Intel's. `IEC` and `IPC` are AS's second names for `IE` and `IP`.
///
/// The 8052's timer 2 registers (`T2CON`, `RCAP2L`, `TL2` and the rest) are
/// not here, as AS defines them only for `CPU 8052`; `T2CON DATA 0C8H`
/// brings one in.
pub const REGISTERS: &[(&str, u8)] = &[
    ("P0", 0x80),
    ("SP", 0x81),
    ("DPL", 0x82),
    ("DPH", 0x83),
    ("PCON", 0x87),
    ("TCON", 0x88),
    ("TMOD", 0x89),
    ("TL0", 0x8a),
    ("TL1", 0x8b),
    ("TH0", 0x8c),
    ("TH1", 0x8d),
    ("P1", 0x90),
    ("SCON", 0x98),
    ("SBUF", 0x99),
    ("P2", 0xa0),
    ("IE", 0xa8),
    ("IEC", 0xa8),
    ("P3", 0xb0),
    ("IP", 0xb8),
    ("IPC", 0xb8),
    ("PSW", 0xd0),
    ("ACC", 0xe0),
    ("B", 0xf0),
];

/// The named bits, from the same header, as (name, register, bit number).
pub const BITS: &[(&str, &str, u8)] = &[
    ("IT0", "TCON", 0),
    ("IE0", "TCON", 1),
    ("IT1", "TCON", 2),
    ("IE1", "TCON", 3),
    ("TR0", "TCON", 4),
    ("TF0", "TCON", 5),
    ("TR1", "TCON", 6),
    ("TF1", "TCON", 7),
    ("RI", "SCON", 0),
    ("TI", "SCON", 1),
    ("RB8", "SCON", 2),
    ("TB8", "SCON", 3),
    ("REN", "SCON", 4),
    ("SM2", "SCON", 5),
    ("SM1", "SCON", 6),
    ("SM0", "SCON", 7),
    ("EX0", "IE", 0),
    ("ET0", "IE", 1),
    ("EX1", "IE", 2),
    ("ET1", "IE", 3),
    ("ES", "IE", 4),
    ("EA", "IE", 7),
    ("RXD", "P3", 0),
    ("TXD", "P3", 1),
    ("INT0", "P3", 2),
    ("INT1", "P3", 3),
    ("T0", "P3", 4),
    ("T1", "P3", 5),
    ("WR", "P3", 6),
    ("RD", "P3", 7),
    ("PX0", "IP", 0),
    ("PT0", "IP", 1),
    ("PX1", "IP", 2),
    ("PT1", "IP", 3),
    ("PS", "IP", 4),
    ("P", "PSW", 0),
    ("OV", "PSW", 2),
    ("RS0", "PSW", 3),
    ("RS1", "PSW", 4),
    ("F0", "PSW", 5),
    ("AC", "PSW", 6),
    ("CY", "PSW", 7),
];

/// The source assembled ahead of an 8051 program: the register and bit
/// names, and `USING`.
///
/// Symbols are case-sensitive in rsasm and AS's are not, so each name is
/// defined in upper and in lower case; source written in either works, and
/// one in mixed case needs a definition of its own. They are ordinary
/// redefinable names, so a program that defines `P1` itself, or includes a
/// header that does, is not refused; only a label spelled like one is, as it
/// is by Intel's ASM51 and by `sdas8051`, which predefine them too.
///
/// `USING n` selects the register bank the direct-address names `AR0` to
/// `AR7` refer to, `n * 8` to `n * 8 + 7`. It is AS's `stddef51.inc` macro,
/// without the bookkeeping symbol it keeps for its own listing. Only the
/// 8-bit dialect has the macro; in the GNU dialect the names are defined
/// with `.set`.
pub fn prelude(dialect: crate::lexer::Dialect) -> String {
    use crate::lexer::Dialect;
    use std::fmt::Write;
    let eight_bit = match dialect {
        Dialect::EightBit => true,
        Dialect::Gas => false,
        _ => return String::new(),
    };
    let mut out = String::new();
    let mut define = |name: &str, value: u8| {
        for n in [name.to_string(), name.to_ascii_lowercase()] {
            let _ = if eight_bit {
                writeln!(out, "{n} EQU {value}")
            } else {
                writeln!(out, ".set {n}, {value}")
            };
        }
    };
    for (name, addr) in REGISTERS {
        define(name, *addr);
    }
    for (name, register, bit) in BITS {
        let addr = REGISTERS
            .iter()
            .find(|(n, _)| n == register)
            .map(|(_, a)| *a)
            .expect("a bit's register is in the table");
        define(name, addr + bit);
    }
    if eight_bit {
        out.push_str("USING MACRO BANK\n");
        for n in 0..8 {
            let _ = writeln!(out, "AR{n} SET ((BANK)*8)+{n}");
            let _ = writeln!(out, "ar{n} SET ((BANK)*8)+{n}");
        }
        out.push_str(" ENDM\n");
    }
    out
}
