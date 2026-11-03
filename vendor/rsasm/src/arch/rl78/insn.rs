//! The instruction set: one encoder per mnemonic group.
//!
//! # Where the encodings come from
//!
//! Every byte below was observed in `rl78-elf-as` output (GNU binutils 2.47),
//! and the corpus in `tools/xas-diff/rl78.txt` holds a case for each form. The
//! structure — which operand shapes exist, and which order an ambiguous
//! address is tried in — follows that assembler's grammar,
//! `gas/config/rl78-parse.y`, which is the only machine-readable description
//! of RL78 syntax there is. No encoding was taken from the Renesas manual.
//!
//! # How the opcode space is laid out
//!
//! RL78 has a one-byte first page and three escape bytes, each of which opens a
//! second page:
//!
//! * `61` — the 8-bit ALU on registers and `[hl+b]`/`[hl+c]`, `xch`, the
//!   rotates, `call`/`br` through a register, `sel`, `sk*`, `bh`/`bnh`, and
//!   the control instructions (`halt`, `stop`, `reti`, `brk`).
//! * `71` — every single-bit operation. The second byte is `0bbb oooo` or
//!   `1bbb oooo`: the bit number sits in bits 6–4 for all of them, and the
//!   low nibble with bit 7 selects operation and address space.
//! * `31` — `bt`/`bf`/`btclr` (same layout as `71`), the multi-bit shifts
//!   (count in the high nibble), and `xchw`.
//!
//! `ce fb` opens the G14 multiply/divide group. On top of all of that, `11`
//! is the `ES:` prefix, which goes in front of any of it.
//!
//! The first page is irregular but dense: the ALU instructions are `op << 4`
//! ORed with an addressing-mode nibble (`add` 0, `addc` 1, `sub` 2, `subc` 3,
//! `cmp` 4, `and` 5, `or` 6, `xor` 7), and the 16-bit ALU does the same with
//! `addw` 0, `subw` 2, `cmpw` 4. Most of the remaining one-byte opcodes carry
//! an 8-bit register number in their low three bits or a 16-bit one in bits
//! 2–1 (`movw`, `push`, `incw`) or bits 5–4 (`call`, `movw rp, saddr`).

use super::encode::{self, Area, Direct, Enc, Order};
use super::operand::{self, Expr, Index, Kind, Offset, Operand, Ptr};
use super::reg;
use crate::arch::{AsmCtx, InsnRequest};
use crate::section::Variant;
use crate::source::Span;

/// Which RL78 core the source targets. Only the G14's hardware multiply and
/// divide instructions differ between the ones this backend models.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Isa {
    /// RL78/G13: no `mulhu`, `mulh`, `divhu`, `divwu`, `machu` or `mach`.
    G13,
    /// RL78/G14, and plain `rl78`: the full instruction set.
    G14,
}

/// Instructions that take no operands.
///
/// `ei` and `di` are `set1 psw.7` and `clr1 psw.7` under their own names:
/// `IE` is bit 7 of `PSW`, whose SFR byte is `fa`.
const IMPLIED: &[(&str, &[u8])] = &[
    ("nop", &[0x00]),
    ("brk", &[0x61, 0xcc]),
    ("brk1", &[0xff]),
    ("halt", &[0x61, 0xed]),
    ("stop", &[0x61, 0xfd]),
    ("ret", &[0xd7]),
    ("reti", &[0x61, 0xfc]),
    ("retb", &[0x61, 0xec]),
    ("ei", &[0x71, 0x7a, 0xfa]),
    ("di", &[0x71, 0x7b, 0xfa]),
    ("skc", &[0x61, 0xc8]),
    ("sknc", &[0x61, 0xd8]),
    ("skz", &[0x61, 0xe8]),
    ("sknz", &[0x61, 0xf8]),
    ("skh", &[0x61, 0xe3]),
    ("sknh", &[0x61, 0xf3]),
];

/// The G14 multiply/divide group, all `ce fb nn`.
///
/// `divwu` is `0b`, not `04`: binutils notes that editions of the software
/// manual with the same version number disagree, and that `0b` is what the
/// hardware does.
const MULDIV: &[(&str, u8)] = &[
    ("mulhu", 0x01),
    ("mulh", 0x02),
    ("divhu", 0x03),
    ("machu", 0x05),
    ("mach", 0x06),
    ("divwu", 0x0b),
];

/// The 8-bit ALU mnemonics, as the high nibble of their first-page opcodes.
const ALU: &[(&str, u8)] = &[
    ("add", 0x00),
    ("addc", 0x10),
    ("sub", 0x20),
    ("subc", 0x30),
    ("cmp", 0x40),
    ("and", 0x50),
    ("or", 0x60),
    ("xor", 0x70),
];

/// The 16-bit ALU mnemonics, likewise.
const ALUW: &[(&str, u8)] = &[("addw", 0x00), ("subw", 0x20), ("cmpw", 0x40)];

/// Every other mnemonic, so an unknown one is reported before its operands are
/// picked apart.
const OTHERS: &[&str] = &[
    "mov", "xch", "movw", "xchw", "oneb", "clrb", "onew", "clrw", "cmp0", "cmps", "movs", "inc",
    "dec", "incw", "decw", "mulu", "rol", "rolc", "rolwc", "ror", "rorc", "sar", "sarw", "shl",
    "shlw", "shr", "shrw", "set1", "clr1", "not1", "mov1", "and1", "or1", "xor1", "bt", "bf",
    "btclr", "bc", "bnc", "bz", "bnz", "bh", "bnh", "br", "call", "callt", "push", "pop", "sel",
];

fn lookup<T: Copy>(table: &[(&str, T)], m: &str) -> Option<T> {
    table.iter().find(|(n, _)| *n == m).map(|(_, v)| *v)
}

/// CC-RL accepts `[DE]` and `[HL]` in a few operand positions where the
/// instruction set only has `[DE+byte]` or `[HL+byte]`, and assembles them
/// with a zero displacement (R20UT3123EJ0115 §5.2.9, page 537). GNU as has no
/// such forms, so the list is taken as the manual gives it and no further.
fn implicit_zero_displacement(cx: &mut AsmCtx<'_>, m: &str, ops: &mut [Operand]) {
    let slot = match (m, &ops[..]) {
        (
            "mov",
            [
                _,
                Operand {
                    kind: Kind::Imm(_), ..
                },
            ],
        ) => 0,
        ("movs" | "inc" | "dec" | "incw" | "decw", [_, ..]) => 0,
        ("cmps" | "addw" | "subw" | "cmpw", [_, _]) => 1,
        _ => return,
    };
    let hl_only = m != "mov";
    let op = &mut ops[slot];
    if let Kind::Ind {
        ptr: ptr @ (Ptr::De | Ptr::Hl),
        off: off @ Offset::None,
    } = &mut op.kind
        && (*ptr == Ptr::Hl || !hl_only)
    {
        let e = cx.exprs.int(0, op.span);
        *off = Offset::Disp(Expr { e, span: op.span });
    }
}

pub fn assemble(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    isa: Isa,
) -> Option<Vec<Variant>> {
    let span = insn.span;
    let known = lookup(IMPLIED, m).is_some()
        || lookup(MULDIV, m).is_some()
        || lookup(ALU, m).is_some()
        || lookup(ALUW, m).is_some()
        || OTHERS.contains(&m);
    if !known {
        cx.error(
            insn.mnemonic_span,
            format!("unknown RL78 instruction `{m}`"),
        );
        return None;
    }

    let bits = matches!(
        m,
        "set1" | "clr1" | "not1" | "mov1" | "and1" | "or1" | "xor1" | "bt" | "bf" | "btclr"
    );
    let mut ops = operand::parse_all(cx, insn.operands, span, bits)?;
    if cx.dialect == crate::lexer::Dialect::CcRl {
        implicit_zero_displacement(cx, m, &mut ops);
    }

    if let Some(bytes) = lookup(IMPLIED, m) {
        arity(cx, &ops, span, m, 0)?;
        return Enc::new(false, bytes).done();
    }
    if let Some(code) = lookup(MULDIV, m) {
        arity(cx, &ops, span, m, 0)?;
        if isa != Isa::G14 {
            cx.error(
                insn.mnemonic_span,
                format!("`{m}` is only available on the RL78/G14"),
            );
            return None;
        }
        return Enc::new(false, &[0xce, 0xfb, code]).done();
    }
    if let Some(op) = lookup(ALU, m) {
        return alu(cx, &ops, span, m, op);
    }
    if let Some(op) = lookup(ALUW, m) {
        return aluw(cx, &ops, span, m, op);
    }

    match m {
        "mov" => mov(cx, &ops, span),
        "xch" => xch(cx, &ops, span),
        "movw" => movw(cx, &ops, span),
        "xchw" => {
            let [d, s] = two(cx, &ops, span, m)?;
            match (d.kind, s.kind) {
                (Kind::Reg16(reg::AX), Kind::Reg16(r)) if r != reg::AX => {
                    Enc::new(false, &[0x31 | r << 1]).done()
                }
                _ => bad(cx, span, m),
            }
        }
        "oneb" | "clrb" => onebyte(cx, &ops, span, m, if m == "oneb" { 0xe0 } else { 0xf0 }),
        "cmp0" => onebyte(cx, &ops, span, m, 0xd0),
        "onew" | "clrw" => {
            let [o] = one(cx, &ops, span, m)?;
            let base = if m == "onew" { 0xe6 } else { 0xf6 };
            match o.kind {
                Kind::Reg16(r @ (reg::AX | reg::BC)) => Enc::new(false, &[base | r]).done(),
                _ => bad(cx, span, m),
            }
        }
        "cmps" | "movs" => {
            let [d, s] = two(cx, &ops, span, m)?;
            // `cmps x, [hl+d]` and `movs [hl+d], x`: the same operands, in the
            // order the operation reads them.
            let (mem, x, code) = if m == "cmps" {
                (s, d, 0xde)
            } else {
                (d, s, 0xce)
            };
            match (mem.kind, x.kind) {
                (
                    Kind::Ind {
                        ptr: Ptr::Hl,
                        off: Offset::Disp(o),
                    },
                    Kind::Reg8(reg::X),
                ) => {
                    let mut e = Enc::new(mem.es, &[0x61, code]);
                    e.imm8(o);
                    e.done()
                }
                _ => bad(cx, span, m),
            }
        }
        "inc" | "dec" => incdec(cx, &ops, span, m, if m == "inc" { 0x00 } else { 0x10 }),
        "incw" | "decw" => incdecw(cx, &ops, span, m, if m == "incw" { 0x00 } else { 0x10 }),
        "mulu" => {
            let [o] = one(cx, &ops, span, m)?;
            match o.kind {
                Kind::Reg8(reg::X) => Enc::new(false, &[0xd6]).done(),
                _ => bad(cx, span, m),
            }
        }
        "rol" | "rolc" | "ror" | "rorc" | "rolwc" | "sar" | "sarw" | "shl" | "shlw" | "shr"
        | "shrw" => shift(cx, &ops, span, m),
        "set1" | "clr1" => setclr1(cx, &ops, span, m),
        "not1" => {
            let [o] = one(cx, &ops, span, m)?;
            match (o.kind, o.bit) {
                (Kind::Cy, None) => Enc::new(false, &[0x71, 0xc0]).done(),
                _ => bad(cx, span, m),
            }
        }
        "mov1" => mov1(cx, &ops, span),
        "and1" | "or1" | "xor1" => {
            let op = match m {
                "and1" => 0x05,
                "or1" => 0x06,
                _ => 0x07,
            };
            and1(cx, &ops, span, m, op)
        }
        "bt" | "bf" | "btclr" => {
            let op = match m {
                "bt" => 0x02,
                "bf" => 0x04,
                _ => 0x00,
            };
            bit_branch(cx, &ops, span, m, op)
        }
        "bc" | "bnc" | "bz" | "bnz" | "bh" | "bnh" => cond_branch(cx, &ops, span, m),
        "br" => br(cx, &ops, span),
        "call" => call(cx, &ops, span),
        "callt" => callt(cx, &ops, span),
        "push" | "pop" => {
            let [o] = one(cx, &ops, span, m)?;
            let push = m == "push";
            match o.kind {
                Kind::Reg16(r) => {
                    Enc::new(false, &[if push { 0xc1 } else { 0xc0 } | r << 1]).done()
                }
                Kind::Sfr(reg::SFR_PSW) => {
                    Enc::new(false, &[0x61, if push { 0xdd } else { 0xcd }]).done()
                }
                _ => bad(cx, span, m),
            }
        }
        "sel" => {
            let [o] = one(cx, &ops, span, m)?;
            match o.kind {
                Kind::Bank(n) => Enc::new(false, &[0x61, 0xcf | n << 4]).done(),
                _ => bad(cx, span, m),
            }
        }
        _ => {
            // Every name in `OTHERS` has an arm above; this is unreachable in
            // practice but must not panic if the two lists ever drift apart.
            cx.error(
                insn.mnemonic_span,
                format!("unknown RL78 instruction `{m}`"),
            );
            None
        }
    }
}

// ---- shared helpers ---------------------------------------------------------

fn bad(cx: &mut AsmCtx<'_>, span: Span, m: &str) -> Option<Vec<Variant>> {
    cx.error(span, format!("invalid operands for `{m}`"));
    None
}

fn arity(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str, n: usize) -> Option<()> {
    if ops.len() == n {
        return Some(());
    }
    let want = match n {
        0 => "no operands".to_string(),
        1 => "one operand".to_string(),
        _ => format!("{n} operands"),
    };
    cx.error(span, format!("`{m}` takes {want}, found {}", ops.len()));
    None
}

fn one(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str) -> Option<[Operand; 1]> {
    arity(cx, ops, span, m, 1)?;
    Some([ops[0]])
}

fn two(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str) -> Option<[Operand; 2]> {
    arity(cx, ops, span, m, 2)?;
    Some([ops[0], ops[1]])
}

/// The register field of `mov r, !addr` and `mov r, saddr`, which exist for
/// only three registers besides `A`, numbered in the high nibble.
fn xbc(cx: &mut AsmCtx<'_>, op: &Operand, r: u8) -> Option<u8> {
    match r {
        reg::X => Some(0x10),
        reg::B => Some(0x20),
        reg::C => Some(0x30),
        _ => {
            cx.error(
                op.span,
                "only `a`, `x`, `b` and `c` can be loaded from an address directly",
            );
            None
        }
    }
}

/// A one-byte instruction followed by a short direct or SFR address.
fn direct_form(cx: &mut AsmCtx<'_>, x: Expr, order: Order, saddr: u8, sfr: u8) -> Option<Enc> {
    let d = encode::classify(cx, x, order)?;
    let mut e = Enc::new(false, &[if d.area == Area::Sfr { sfr } else { saddr }]);
    e.direct(d);
    Some(e)
}

// ---- data transfer ------------------------------------------------------------

fn mov(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    use Kind::{Abs16, Based, Direct as Dir, Imm, Ind, Reg8, Sfr};
    const A: u8 = reg::A;
    let [d, s] = two(cx, ops, span, "mov")?;
    match (d.kind, s.kind) {
        // `A` has its own immediate opcode; the others share `50` with the
        // register in the low bits, and `ES` gets a one-byte form of its own
        // because far addressing reloads it constantly.
        (Reg8(A), Imm(x)) => imm(Enc::new(false, &[0x51]), x),
        (Reg8(r), Imm(x)) => imm(Enc::new(false, &[0x50 | r]), x),
        (Sfr(reg::SFR_ES), Imm(x)) => imm(Enc::new(false, &[0x41]), x),
        (Sfr(c), Imm(x)) => imm(Enc::new(false, &[0xce, c]), x),
        (Dir(a), Imm(x)) => imm(direct_form(cx, a, Order::SfrFirst, 0xcd, 0xce)?, x),
        (Abs16(a), Imm(x)) => {
            let mut e = Enc::new(d.es, &[0xcf]);
            e.addr16(cx, a)?;
            imm(e, x)
        }

        (Reg8(r), Reg8(A)) if r != A => Enc::new(false, &[0x70 | r]).done(),
        (Reg8(A), Reg8(r)) if r != A => Enc::new(false, &[0x60 | r]).done(),

        (Reg8(A), Abs16(a)) => addr(cx, Enc::new(s.es, &[0x8f]), a),
        (Abs16(a), Reg8(A)) => addr(cx, Enc::new(d.es, &[0x9f]), a),
        (Reg8(r), Abs16(a)) => {
            let f = xbc(cx, &d, r)?;
            addr(cx, Enc::new(s.es, &[0xc9 | f]), a)
        }
        (Reg8(A), Dir(a)) => direct_form(cx, a, Order::SaddrFirst, 0x8d, 0x8e)?.done(),
        (Dir(a), Reg8(A)) => direct_form(cx, a, Order::SfrFirst, 0x9d, 0x9e)?.done(),
        (Reg8(r), Dir(a)) => {
            let f = xbc(cx, &d, r)?;
            direct_form(cx, a, Order::SaddrOnly, 0xc8 | f, 0)?.done()
        }

        (Reg8(A), Sfr(c)) => Enc::new(false, &[0x8e, c]).done(),
        (Sfr(c), Reg8(A)) => Enc::new(false, &[0x9e, c]).done(),
        (Sfr(reg::SFR_ES), Dir(a)) => {
            let dd = encode::classify(cx, a, Order::SaddrOnly)?;
            let mut e = Enc::new(false, &[0x61, 0xb8]);
            e.direct(dd);
            e.done()
        }

        // Indirect and based forms: `8x` loads A, `9x` stores it, and the
        // low nibble names the addressing mode.
        (Reg8(A), Ind { ptr, off }) => mem8(cx, s.es, ptr, off, None, span, "mov", true),
        (Ind { ptr, off }, Reg8(A)) => mem8(cx, d.es, ptr, off, None, span, "mov", false),
        (Ind { ptr, off }, Imm(x)) => mem8(cx, d.es, ptr, off, Some(x), span, "mov", false),
        (Reg8(A), Based { base, index }) => {
            let op = match index {
                Index::B => 0x09,
                Index::C => 0x29,
                Index::Bc => 0x49,
            };
            based(Enc::new(s.es, &[op]), base, None)
        }
        (Based { base, index }, Reg8(A)) => {
            let op = match index {
                Index::B => 0x18,
                Index::C => 0x28,
                Index::Bc => 0x48,
            };
            based(Enc::new(d.es, &[op]), base, None)
        }
        (Based { base, index }, Imm(x)) => {
            let op = match index {
                Index::B => 0x19,
                Index::C => 0x38,
                Index::Bc => 0x39,
            };
            based(Enc::new(d.es, &[op]), base, Some(x))
        }
        _ => bad(cx, span, "mov"),
    }
}

fn imm(mut e: Enc, x: Expr) -> Option<Vec<Variant>> {
    e.imm8(x);
    e.done()
}

fn addr(cx: &mut AsmCtx<'_>, mut e: Enc, a: Expr) -> Option<Vec<Variant>> {
    e.addr16(cx, a)?;
    e.done()
}

fn based(mut e: Enc, base: Expr, value: Option<Expr>) -> Option<Vec<Variant>> {
    e.imm16(base);
    if let Some(v) = value {
        e.imm8(v);
    }
    e.done()
}

/// `mov` between `A` (or an immediate) and a register-indirect address.
///
/// `[bc]` has no form of its own: the reference writes it as `0[bc]`, a zero
/// 16-bit base, and `[sp]` likewise as `[sp+0]`. `[de]` and `[hl]` do have
/// displacement-free opcodes.
#[allow(clippy::too_many_arguments)]
fn mem8(
    cx: &mut AsmCtx<'_>,
    es: bool,
    ptr: Ptr,
    off: Offset,
    value: Option<Expr>,
    span: Span,
    m: &str,
    load: bool,
) -> Option<Vec<Variant>> {
    // (load A, store A, store #imm)
    let codes: (&[u8], &[u8], &[u8]) = match (ptr, off) {
        (Ptr::De, Offset::None) => (&[0x89], &[0x99], &[]),
        (Ptr::De, Offset::Disp(_)) => (&[0x8a], &[0x9a], &[0xca]),
        (Ptr::Hl, Offset::None) => (&[0x8b], &[0x9b], &[]),
        (Ptr::Hl, Offset::Disp(_)) => (&[0x8c], &[0x9c], &[0xcc]),
        (Ptr::Hl, Offset::B) => (&[0x61, 0xc9], &[0x61, 0xd9], &[]),
        (Ptr::Hl, Offset::C) => (&[0x61, 0xe9], &[0x61, 0xf9], &[]),
        (Ptr::Bc, Offset::None) => (&[0x49], &[0x48], &[0x39]),
        (Ptr::Sp, Offset::None | Offset::Disp(_)) => (&[0x88], &[0x98], &[0xc8]),
        _ => return bad(cx, span, m),
    };
    let op = match (load, value) {
        (true, _) => codes.0,
        (false, None) => codes.1,
        (false, Some(_)) => codes.2,
    };
    if op.is_empty() {
        return bad(cx, span, m);
    }
    let mut e = Enc::new(es, op);
    match (ptr, off) {
        (_, Offset::Disp(o)) => e.imm8(o),
        (Ptr::Bc, _) => e.bytes.extend_from_slice(&[0, 0]),
        (Ptr::Sp, _) => e.byte(0),
        _ => {}
    }
    if let Some(v) = value {
        e.imm8(v);
    }
    e.done()
}

fn xch(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    let [d, s] = two(cx, ops, span, "xch")?;
    if !matches!(d.kind, Kind::Reg8(reg::A)) {
        return bad(cx, span, "xch");
    }
    let two_byte = |es: bool, b: u8| Enc::new(es, &[0x61, b]);
    match s.kind {
        // `xch a, x` is the only one-byte exchange.
        Kind::Reg8(reg::X) => Enc::new(false, &[0x08]).done(),
        Kind::Reg8(r) if r != reg::A => two_byte(false, 0x88 | r).done(),
        Kind::Abs16(a) => addr(cx, two_byte(s.es, 0xaa), a),
        Kind::Ind { ptr, off } => {
            let code = match (ptr, off) {
                (Ptr::De, Offset::None) => 0xae,
                (Ptr::De, Offset::Disp(_)) => 0xaf,
                (Ptr::Hl, Offset::None) => 0xac,
                (Ptr::Hl, Offset::Disp(_)) => 0xad,
                (Ptr::Hl, Offset::B) => 0xb9,
                (Ptr::Hl, Offset::C) => 0xa9,
                _ => return bad(cx, span, "xch"),
            };
            let mut e = two_byte(s.es, code);
            if let Offset::Disp(o) = off {
                e.imm8(o);
            }
            e.done()
        }
        Kind::Direct(a) => {
            let dd = encode::classify(cx, a, Order::SfrFirst)?;
            let mut e = two_byte(false, if dd.area == Area::Sfr { 0xab } else { 0xa8 });
            e.direct(dd);
            e.done()
        }
        _ => bad(cx, span, "xch"),
    }
}

fn movw(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    use Kind::{Abs16, Based, Direct as Dir, Imm, Ind, Reg16, Sp};
    const AX: u8 = reg::AX;
    let [d, s] = two(cx, ops, span, "movw")?;
    let imm16 = |mut e: Enc, x: Expr| {
        e.imm16(x);
        e.done()
    };
    match (d.kind, s.kind) {
        (Reg16(r), Imm(x)) => imm16(Enc::new(false, &[0x30 | r << 1]), x),
        (Sp, Imm(x)) => imm16(Enc::new(false, &[0xcb, 0xf8]), x),
        (Dir(a), Imm(x)) => imm16(direct_form(cx, a, Order::SaddrFirst, 0xc9, 0xcb)?, x),

        (Reg16(AX), Reg16(r)) if r != AX => Enc::new(false, &[0x11 | r << 1]).done(),
        (Reg16(r), Reg16(AX)) if r != AX => Enc::new(false, &[0x10 | r << 1]).done(),

        // `SP` is the SFR pair at `0xFFFF8`, and these are the `sfr` forms
        // with that byte, `f8`. `movw rp, sp` has no sfr form, so it is
        // written as `movw rp, !0xfff8` — the 16-bit alias of the same place.
        (Sp, Reg16(AX)) => Enc::new(false, &[0xbe, 0xf8]).done(),
        (Reg16(AX), Sp) => Enc::new(false, &[0xae, 0xf8]).done(),
        (Reg16(r), Sp) => Enc::new(false, &[0xcb | r << 4, 0xf8, 0xff]).done(),

        (Reg16(AX), Dir(a)) => {
            encode::word_aligned(cx, a)?;
            direct_form(cx, a, Order::SaddrFirst, 0xad, 0xae)?.done()
        }
        (Dir(a), Reg16(AX)) => {
            encode::word_aligned(cx, a)?;
            direct_form(cx, a, Order::SaddrFirst, 0xbd, 0xbe)?.done()
        }
        (Reg16(r), Dir(a)) => {
            encode::word_aligned(cx, a)?;
            direct_form(cx, a, Order::SaddrOnly, 0xca | r << 4, 0)?.done()
        }

        (Reg16(AX), Abs16(a)) => {
            encode::word_aligned(cx, a)?;
            addr(cx, Enc::new(s.es, &[0xaf]), a)
        }
        (Abs16(a), Reg16(AX)) => {
            encode::word_aligned(cx, a)?;
            addr(cx, Enc::new(d.es, &[0xbf]), a)
        }
        (Reg16(r), Abs16(a)) => {
            encode::word_aligned(cx, a)?;
            addr(cx, Enc::new(s.es, &[0xcb | r << 4]), a)
        }

        (Reg16(AX), Ind { ptr, off }) => mem16(cx, s.es, ptr, off, span, true),
        (Ind { ptr, off }, Reg16(AX)) => mem16(cx, d.es, ptr, off, span, false),
        (Reg16(AX), Based { base, index }) => {
            let op = match index {
                Index::B => 0x59,
                Index::C => 0x69,
                Index::Bc => 0x79,
            };
            based(Enc::new(s.es, &[op]), base, None)
        }
        (Based { base, index }, Reg16(AX)) => {
            let op = match index {
                Index::B => 0x58,
                Index::C => 0x68,
                Index::Bc => 0x78,
            };
            based(Enc::new(d.es, &[op]), base, None)
        }
        _ => bad(cx, span, "movw"),
    }
}

/// `movw` between `AX` and a register-indirect address: `a9`–`ac` load, and
/// `b9`–`bc` store, with `[sp+d]` on `a8`/`b8` and `[bc]` as `0[bc]`.
fn mem16(
    cx: &mut AsmCtx<'_>,
    es: bool,
    ptr: Ptr,
    off: Offset,
    span: Span,
    load: bool,
) -> Option<Vec<Variant>> {
    let code = match (ptr, off) {
        (Ptr::De, Offset::None) => 0xa9,
        (Ptr::De, Offset::Disp(_)) => 0xaa,
        (Ptr::Hl, Offset::None) => 0xab,
        (Ptr::Hl, Offset::Disp(_)) => 0xac,
        (Ptr::Sp, Offset::None | Offset::Disp(_)) => 0xa8,
        (Ptr::Bc, Offset::None) => 0x79,
        _ => return bad(cx, span, "movw"),
    };
    // Loads and stores differ by 0x10 in the first-page groups, but `[bc]`
    // borrows `79`/`78` from `movw ax, addr[bc]`, which differ by one.
    let code = match (ptr, load) {
        (_, true) => code,
        (Ptr::Bc, false) => code - 1,
        (_, false) => code + 0x10,
    };
    let mut e = Enc::new(es, &[code]);
    match (ptr, off) {
        (Ptr::Sp, Offset::Disp(o)) => {
            encode::word_aligned(cx, o)?;
            e.imm8(o);
        }
        (_, Offset::Disp(o)) => e.imm8(o),
        (Ptr::Sp, _) => e.byte(0),
        (Ptr::Bc, _) => e.bytes.extend_from_slice(&[0, 0]),
        _ => {}
    }
    e.done()
}

/// `oneb`, `clrb` and `cmp0`: the four registers with a one-byte form share
/// the low two bits with their register numbers (`x a c b`), then `saddr` on
/// 4 and `!addr` on 5.
fn onebyte(
    cx: &mut AsmCtx<'_>,
    ops: &[Operand],
    span: Span,
    m: &str,
    base: u8,
) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, m)?;
    match o.kind {
        Kind::Reg8(r) if r <= reg::B => Enc::new(false, &[base | r]).done(),
        Kind::Direct(a) => direct_form(cx, a, Order::SaddrOnly, base | 4, 0)?.done(),
        Kind::Abs16(a) => addr(cx, Enc::new(o.es, &[base | 5]), a),
        _ => bad(cx, span, m),
    }
}

// ---- arithmetic -----------------------------------------------------------------

/// `add`, `addc`, `sub`, `subc`, `cmp`, `and`, `or`, `xor`: `op` is the high
/// nibble shared by the whole group.
fn alu(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str, op: u8) -> Option<Vec<Variant>> {
    use Kind::{Abs16, Direct as Dir, Imm, Ind, Reg8};
    const A: u8 = reg::A;
    let [d, s] = two(cx, ops, span, m)?;
    match (d.kind, s.kind) {
        (Reg8(A), Imm(x)) => imm(Enc::new(false, &[0x0c | op]), x),
        (Dir(a), Imm(x)) => imm(direct_form(cx, a, Order::SaddrOnly, 0x0a | op, 0)?, x),
        // On the `61` page, `A` on the left is `08 | r`, `A` on the right
        // `00 | r`, and `A, A` — which would collide with `r, A` for `r = A`
        // — has `01`.
        (Reg8(A), Reg8(A)) => Enc::new(false, &[0x61, 0x01 | op]).done(),
        (Reg8(A), Reg8(r)) => Enc::new(false, &[0x61, 0x08 | op | r]).done(),
        (Reg8(r), Reg8(A)) => Enc::new(false, &[0x61, op | r]).done(),
        (Reg8(A), Dir(a)) => direct_form(cx, a, Order::SaddrOnly, 0x0b | op, 0)?.done(),
        (Reg8(A), Abs16(a)) => addr(cx, Enc::new(s.es, &[0x0f | op]), a),
        (Reg8(A), Ind { ptr: Ptr::Hl, off }) => {
            let mut e = match off {
                Offset::None => Enc::new(s.es, &[0x0d | op]),
                Offset::Disp(_) => Enc::new(s.es, &[0x0e | op]),
                Offset::B => Enc::new(s.es, &[0x61, 0x80 | op]),
                Offset::C => Enc::new(s.es, &[0x61, 0x82 | op]),
            };
            if let Offset::Disp(o) = off {
                e.imm8(o);
            }
            e.done()
        }
        // The one ALU operation on a 16-bit address with an immediate, and it
        // took `cmp`'s slot 0 on the first page, which is why it is `40`.
        (Abs16(a), Imm(x)) if op == 0x40 => {
            let mut e = Enc::new(d.es, &[0x40]);
            e.addr16(cx, a)?;
            imm(e, x)
        }
        _ => bad(cx, span, m),
    }
}

/// `addw`, `subw`, `cmpw`.
fn aluw(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str, op: u8) -> Option<Vec<Variant>> {
    use Kind::{Abs16, Direct as Dir, Imm, Ind, Reg16, Sp};
    let [d, s] = two(cx, ops, span, m)?;
    match (d.kind, s.kind) {
        (Reg16(reg::AX), Imm(x)) => {
            let mut e = Enc::new(false, &[0x04 | op]);
            e.imm16(x);
            e.done()
        }
        (Reg16(reg::AX), Reg16(r)) => Enc::new(false, &[0x01 | op | r << 1]).done(),
        (Reg16(reg::AX), Dir(a)) => direct_form(cx, a, Order::SaddrOnly, 0x06 | op, 0)?.done(),
        (Reg16(reg::AX), Abs16(a)) => addr(cx, Enc::new(s.es, &[0x02 | op]), a),
        // No displacement-free `[hl]` opcode exists; it is `[hl+0]`.
        (
            Reg16(reg::AX),
            Ind {
                ptr: Ptr::Hl,
                off: off @ (Offset::None | Offset::Disp(_)),
            },
        ) => {
            let mut e = Enc::new(s.es, &[0x61, 0x09 | op]);
            match off {
                Offset::Disp(o) => e.imm8(o),
                _ => e.byte(0),
            }
            e.done()
        }
        // `addw sp, #n` and `subw sp, #n` are one-byte opcodes with an 8-bit
        // immediate: stack frames are small.
        (Sp, Imm(x)) if op != 0x40 => imm(Enc::new(false, &[if op == 0 { 0x10 } else { 0x20 }]), x),
        _ => bad(cx, span, m),
    }
}

fn incdec(
    cx: &mut AsmCtx<'_>,
    ops: &[Operand],
    span: Span,
    m: &str,
    dec: u8,
) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, m)?;
    match o.kind {
        Kind::Reg8(r) => Enc::new(false, &[0x80 | dec | r]).done(),
        Kind::Direct(a) => direct_form(cx, a, Order::SaddrOnly, 0xa4 | dec, 0)?.done(),
        Kind::Abs16(a) => addr(cx, Enc::new(o.es, &[0xa0 | dec]), a),
        Kind::Ind {
            ptr: Ptr::Hl,
            off: Offset::Disp(x),
        } => {
            let mut e = Enc::new(o.es, &[0x61, 0x59 + dec]);
            e.imm8(x);
            e.done()
        }
        _ => bad(cx, span, m),
    }
}

fn incdecw(
    cx: &mut AsmCtx<'_>,
    ops: &[Operand],
    span: Span,
    m: &str,
    dec: u8,
) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, m)?;
    match o.kind {
        Kind::Reg16(r) => Enc::new(false, &[0xa1 | dec | r << 1]).done(),
        Kind::Direct(a) => direct_form(cx, a, Order::SaddrOnly, 0xa6 | dec, 0)?.done(),
        Kind::Abs16(a) => addr(cx, Enc::new(o.es, &[0xa2 | dec]), a),
        Kind::Ind {
            ptr: Ptr::Hl,
            off: Offset::Disp(x),
        } => {
            let mut e = Enc::new(o.es, &[0x61, 0x79 + dec]);
            e.imm8(x);
            e.done()
        }
        _ => bad(cx, span, m),
    }
}

// ---- shifts and rotates ---------------------------------------------------------

fn shift(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str) -> Option<Vec<Variant>> {
    let [d, count] = two(cx, ops, span, m)?;
    // The single-bit rotates exist only as "by 1"; the count is written, and
    // checked, but not encoded.
    let rotate = match (m, d.kind) {
        ("rol", Kind::Reg8(reg::A)) => Some(0xeb),
        ("rolc", Kind::Reg8(reg::A)) => Some(0xdc),
        ("ror", Kind::Reg8(reg::A)) => Some(0xdb),
        ("rorc", Kind::Reg8(reg::A)) => Some(0xfb),
        ("rolwc", Kind::Reg16(reg::AX)) => Some(0xee),
        ("rolwc", Kind::Reg16(reg::BC)) => Some(0xfe),
        _ => None,
    };
    if let Some(code) = rotate {
        encode::small_constant(cx, &count, "rotate count", 1, 1)?;
        return Enc::new(false, &[0x61, code]).done();
    }
    // The multi-bit shifts are on the `31` page with the count in the high
    // nibble, so a byte-wide shift takes 1..7 and a word-wide one 1..15.
    let (code, max) = match (m, d.kind) {
        ("shl", Kind::Reg8(reg::C)) => (0x07, 7),
        ("shl", Kind::Reg8(reg::B)) => (0x08, 7),
        ("shl", Kind::Reg8(reg::A)) => (0x09, 7),
        ("shr", Kind::Reg8(reg::A)) => (0x0a, 7),
        ("sar", Kind::Reg8(reg::A)) => (0x0b, 7),
        ("shlw", Kind::Reg16(reg::BC)) => (0x0c, 15),
        ("shlw", Kind::Reg16(reg::AX)) => (0x0d, 15),
        ("shrw", Kind::Reg16(reg::AX)) => (0x0e, 15),
        ("sarw", Kind::Reg16(reg::AX)) => (0x0f, 15),
        _ => return bad(cx, span, m),
    };
    let n = encode::small_constant(cx, &count, "shift count", 1, max)?;
    Enc::new(false, &[0x31, code | n << 4]).done()
}

// ---- bit manipulation -----------------------------------------------------------

/// Where a bit lives.
enum BitLoc {
    Direct(Direct),
    A,
    /// `[hl]`, possibly with `es:`.
    Hl(bool),
    /// `!addr`, possibly with `es:`; only `set1` and `clr1` have this form.
    Abs16(bool, Expr),
}

/// Resolves a `x.n` operand. Named SFRs (`psw.7`) and SFR addresses given as
/// numbers are the same thing here, so both become a [`Direct`].
fn bit_loc(cx: &mut AsmCtx<'_>, op: &Operand, order: Order, m: &str) -> Option<(BitLoc, u8)> {
    let Some(bit) = op.bit else {
        cx.error(op.span, format!("`{m}` needs a bit operand, such as `a.3`"));
        return None;
    };
    let loc = match op.kind {
        Kind::Sfr(c) => {
            let x = Expr {
                e: cx.exprs.int(0xfff00 | c as u64, op.span),
                span: op.span,
            };
            BitLoc::Direct(Direct {
                area: Area::Sfr,
                value: Some(0xfff00 | c as i64),
                x,
            })
        }
        Kind::Direct(x) => BitLoc::Direct(encode::classify(cx, x, order)?),
        Kind::Reg8(reg::A) => BitLoc::A,
        Kind::Ind {
            ptr: Ptr::Hl,
            off: Offset::None,
        } => BitLoc::Hl(op.es),
        Kind::Abs16(x) => BitLoc::Abs16(op.es, x),
        _ => {
            cx.error(
                op.span,
                format!("`{m}` cannot address a bit of this operand"),
            );
            return None;
        }
    };
    Some((loc, bit))
}

fn setclr1(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, m)?;
    let clr = u8::from(m == "clr1");
    if matches!(o.kind, Kind::Cy) && o.bit.is_none() {
        return Enc::new(false, &[0x71, if clr == 1 { 0x88 } else { 0x80 }]).done();
    }
    let (loc, bit) = bit_loc(cx, &o, Order::SfrFirst, m)?;
    let b = bit << 4;
    match loc {
        BitLoc::Direct(d) => {
            let code = if d.area == Area::Sfr { 0x0a } else { 0x02 };
            let mut e = Enc::new(false, &[0x71, code | clr | b]);
            e.direct(d);
            e.done()
        }
        BitLoc::A => Enc::new(false, &[0x71, 0x8a | clr | b]).done(),
        BitLoc::Hl(es) => Enc::new(es, &[0x71, 0x82 | clr | b]).done(),
        // Here `clr1` moves from the low bit to bit 3, the one irregular
        // member of the family.
        BitLoc::Abs16(es, a) => addr(cx, Enc::new(es, &[0x71, clr << 3 | b]), a),
    }
}

fn mov1(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    let [d, s] = two(cx, ops, span, "mov1")?;
    // `mov1 cy, x.n` loads the carry and `mov1 x.n, cy` stores it; the store
    // opcodes are the load opcodes minus 3.
    let (bitop, store) = match (d.kind, s.kind) {
        (Kind::Cy, _) if d.bit.is_none() => (s, false),
        (_, Kind::Cy) if s.bit.is_none() => (d, true),
        _ => return bad(cx, span, "mov1"),
    };
    let (loc, bit) = bit_loc(cx, &bitop, Order::SaddrFirst, "mov1")?;
    let adj = |code: u8| (if store { code - 3 } else { code }) | bit << 4;
    match loc {
        BitLoc::Direct(dd) => {
            let code = if dd.area == Area::Sfr { 0x0c } else { 0x04 };
            let mut e = Enc::new(false, &[0x71, adj(code)]);
            e.direct(dd);
            e.done()
        }
        BitLoc::A => Enc::new(false, &[0x71, adj(0x8c)]).done(),
        BitLoc::Hl(es) => Enc::new(es, &[0x71, adj(0x84)]).done(),
        BitLoc::Abs16(..) => bad(cx, span, "mov1"),
    }
}

fn and1(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str, op: u8) -> Option<Vec<Variant>> {
    let [d, s] = two(cx, ops, span, m)?;
    if !matches!(d.kind, Kind::Cy) || d.bit.is_some() {
        return bad(cx, span, m);
    }
    let (loc, bit) = bit_loc(cx, &s, Order::SfrFirst, m)?;
    let b = bit << 4;
    match loc {
        BitLoc::Direct(dd) => {
            let code = if dd.area == Area::Sfr { 0x08 } else { 0x00 };
            let mut e = Enc::new(false, &[0x71, code | op | b]);
            e.direct(dd);
            e.done()
        }
        BitLoc::A => Enc::new(false, &[0x71, 0x88 | op | b]).done(),
        BitLoc::Hl(es) => Enc::new(es, &[0x71, 0x80 | op | b]).done(),
        BitLoc::Abs16(..) => bad(cx, span, m),
    }
}

// ---- branches -------------------------------------------------------------------

/// A conditional branch in both its forms, smallest first.
///
/// The short form is the instruction with an 8-bit displacement. The long
/// form is what the reference relaxes an out-of-range one to: the opposite
/// condition, skipping 3 bytes, over a `br $!target` (`ee` and a 16-bit
/// displacement). `head` is everything before the displacement, `inverted`
/// the same with the condition flipped; both displacements are measured from
/// the end of the whole instruction.
fn relaxable(
    es: bool,
    head: &[u8],
    inverted: &[u8],
    addr: Option<Direct>,
    target: Expr,
) -> Vec<Variant> {
    let mut short = Enc::new(es, head);
    let mut long = Enc::new(es, inverted);
    if let Some(d) = addr {
        short.direct(d);
        long.direct(d);
    }
    short.rel8(target);
    long.bytes.extend_from_slice(&[0x03, 0xee]);
    long.rel16(target);
    vec![short.variant(), long.variant()]
}

fn target(cx: &mut AsmCtx<'_>, op: &Operand, m: &str) -> Option<Expr> {
    match op.kind {
        Kind::Rel(x) => Some(x),
        _ => {
            cx.error(
                op.span,
                format!("`{m}` needs a relative target, written `$label`"),
            );
            None
        }
    }
}

fn cond_branch(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span, m: &str) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, m)?;
    let t = target(cx, &o, m)?;
    // `dc`–`df` pair up as C/NC and Z/NZ, one bit apart; `bh`/`bnh` live on
    // the `61` page and differ in bit 4.
    let (head, inverted): (&[u8], &[u8]) = match m {
        "bc" => (&[0xdc], &[0xde]),
        "bnc" => (&[0xde], &[0xdc]),
        "bz" => (&[0xdd], &[0xdf]),
        "bnz" => (&[0xdf], &[0xdd]),
        "bh" => (&[0x61, 0xc3], &[0x61, 0xd3]),
        _ => (&[0x61, 0xd3], &[0x61, 0xc3]),
    };
    Some(relaxable(false, head, inverted, None, t))
}

/// `bt`, `bf` and `btclr`, whose second byte has the `71`-page layout with
/// `op` 2, 4 and 0.
fn bit_branch(
    cx: &mut AsmCtx<'_>,
    ops: &[Operand],
    span: Span,
    m: &str,
    op: u8,
) -> Option<Vec<Variant>> {
    let [bitop, dest] = two(cx, ops, span, m)?;
    let (loc, bit) = bit_loc(cx, &bitop, Order::SfrFirst, m)?;
    let t = target(cx, &dest, m)?;
    let b = bit << 4;
    let (es, low, addr) = match loc {
        BitLoc::Direct(d) => (
            false,
            if d.area == Area::Sfr { 0x80 } else { 0x00 },
            Some(d),
        ),
        BitLoc::A => (false, 0x01, None),
        BitLoc::Hl(es) => (es, 0x81, None),
        BitLoc::Abs16(..) => return bad(cx, span, m),
    };
    let head = [0x31, low | op | b];
    // `btclr` has no inverse to skip over — clearing the bit is its point —
    // so the reference never relaxes it, and neither does this.
    if op == 0 {
        let mut e = Enc::new(es, &head);
        if let Some(d) = addr {
            e.direct(d);
        }
        e.rel8(t);
        return e.done();
    }
    // `bt` and `bf` swap by flipping 2 and 4.
    let inverted = [0x31, low | (op ^ 0x06) | b];
    Some(relaxable(es, &head, &inverted, addr, t))
}

fn no_es(cx: &mut AsmCtx<'_>, op: &Operand, m: &str) -> Option<()> {
    if op.es {
        cx.error(op.span, format!("`{m}` cannot take an `es:` target"));
        return None;
    }
    Some(())
}

/// `br`. Unlike the conditional branches it is never relaxed: the reference
/// keeps whichever form was written, so `br $far` is a range error rather
/// than a silently longer instruction.
fn br(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, "br")?;
    no_es(cx, &o, "br")?;
    match o.kind {
        Kind::Reg16(reg::AX) => Enc::new(false, &[0x61, 0xcb]).done(),
        Kind::Rel(x) => {
            let mut e = Enc::new(false, &[0xef]);
            e.rel8(x);
            e.done()
        }
        Kind::RelLong(x) => {
            let mut e = Enc::new(false, &[0xee]);
            e.rel16(x);
            e.done()
        }
        Kind::Abs16(x) => addr(cx, Enc::new(false, &[0xed]), x),
        Kind::Abs20(x) => {
            let mut e = Enc::new(false, &[0xec]);
            e.addr20(x);
            e.done()
        }
        _ => bad(cx, span, "br"),
    }
}

/// `call`, which has `br`'s forms except the 8-bit relative one, and takes
/// any 16-bit register rather than only `AX`.
fn call(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, "call")?;
    no_es(cx, &o, "call")?;
    match o.kind {
        Kind::Reg16(r) => Enc::new(false, &[0x61, 0xca | r << 4]).done(),
        Kind::RelLong(x) => {
            let mut e = Enc::new(false, &[0xfe]);
            e.rel16(x);
            e.done()
        }
        Kind::Abs16(x) => addr(cx, Enc::new(false, &[0xfd]), x),
        Kind::Abs20(x) => {
            let mut e = Enc::new(false, &[0xfc]);
            e.addr20(x);
            e.done()
        }
        _ => bad(cx, span, "call"),
    }
}

/// `callt [addr]`: a call through one of 32 vectors at `0x80`–`0xBE`.
///
/// The vector number `(addr - 0x80) / 2` is split across the second byte: its
/// low three bits in bits 6–4 and its top two in bits 1–0, around the fixed
/// `84`.
fn callt(cx: &mut AsmCtx<'_>, ops: &[Operand], span: Span) -> Option<Vec<Variant>> {
    let [o] = one(cx, ops, span, "callt")?;
    let Kind::Table(x) = o.kind else {
        return bad(cx, span, "callt");
    };
    let Some(v) = cx.constant(x.e) else {
        cx.error(x.span, "`callt` needs a constant table address");
        return None;
    };
    if !(0x80..=0xbe).contains(&v) {
        cx.error(
            x.span,
            format!("`callt` table address {v:#x} is out of range (0x80 to 0xbe)"),
        );
        return None;
    }
    if v & 1 != 0 {
        cx.error(x.span, format!("`callt` table address {v:#x} must be even"));
        return None;
    }
    let i = v as u8;
    Enc::new(false, &[0x61, 0x84 | ((i >> 1) & 7) << 4 | ((i >> 4) & 3)]).done()
}
