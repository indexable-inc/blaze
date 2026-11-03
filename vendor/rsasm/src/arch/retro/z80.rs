//! The Zilog Z80 opcode tables and encoder.
//!
//! # Where the tables come from
//!
//! Zilog, *Z80 CPU User Manual* (UM0080): the "Z80 Instruction Description"
//! chapter, whose per-instruction entries give the opcode bits of every
//! documented form, including the `CB`, `ED`, `DD`/`FD` and `DD CB d`
//! encodings.
//!
//! The bit structure encoded below is the standard octal reading of the Z80
//! opcode matrix, in which an opcode byte splits as `xx yyy zzz`:
//!
//! ```text
//!   x = 0   z = 4/5/6   INC r[y] / DEC r[y] / LD r[y],n
//!   x = 1               LD r[y],r[z]        (y = z = 6 is HALT)
//!   x = 2               alu[y] r[z]
//!   x = 3   z = 0/2/4   RET cc[y] / JP cc[y],nn / CALL cc[y],nn
//! ```
//!
//! The table names below (`r`, `rp`, `rp2`, `cc`, `alu`, `rot`) and their
//! orders are those of Cristian Dinu's "Decoding Z80 Opcodes"
//! (`z80.info/decoding.htm`), so a row can be checked against that page
//! directly. Building the encoder out of `r[]`, `rp[]`, `cc[]`, `alu[]` and
//! `rot[]` rather than out of 700-odd hand-written rows is what makes it
//! reviewable: a mistake is visible as a wrong table entry, not as one wrong
//! byte buried in a list.
//!
//! # Undocumented instructions
//!
//! Those that GNU as and vasm both accept are implemented, and checked against
//! both: the halves of the index registers (`IXH`, `IXL`, `IYH`, `IYL`) in
//! the 8-bit load and arithmetic groups, `IN F,(C)` (also `IN (C)`) and
//! `OUT (C),0`, and `SLL`. The `DD CB d op,r` forms that write a register as
//! well as memory are not: vasm refuses them, and GNU as only takes them
//! when asked. The documented `DD CB d op` forms *are* implemented.
//!
//! `EX AF,AF'` may also be written without the prime, which rsasm's lexer
//! could once not read; GNU as accepts both too.

use super::common::{self, Enc};
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::ExprRef;
use crate::lexer::{Punct, Token};
use crate::section::Variant;
use crate::source::Span;

// ---- the matrix -----------------------------------------------------------

/// `r[]`, the 3-bit register field. Slot 6 is `(HL)`, which is why an 8-bit
/// operand carries an optional index prefix and displacement as well as a
/// field number.
pub const R: [&str; 8] = ["b", "c", "d", "e", "h", "l", "(hl)", "a"];

/// `rp[]`, the 16-bit pair field of `LD rp,nn`, `ADD HL,rp` and friends.
pub const RP: [&str; 4] = ["bc", "de", "hl", "sp"];

/// `rp2[]`, the same field as `PUSH` and `POP` read it: `AF` in place of `SP`.
pub const RP2: [&str; 4] = ["bc", "de", "hl", "af"];

/// `cc[]`, the 3-bit condition field. The first four are also the conditions
/// `JR` can take, which is exactly why they come first.
pub const CC: [&str; 8] = ["nz", "z", "nc", "c", "po", "pe", "p", "m"];

/// `alu[]`, the accumulator ALU group of `x = 2` and of the `x = 3, z = 6`
/// immediate column.
pub const ALU: [&str; 8] = ["add", "adc", "sub", "sbc", "and", "xor", "or", "cp"];

/// `rot[]`, the `CB` page's shift and rotate group. `sll` (slot 6) is
/// undocumented but universally implemented; it shifts left and sets bit 0.
pub const ROT: [&str; 8] = ["rlc", "rrc", "rl", "rr", "sla", "sra", "sll", "srl"];

/// `x = 0, z = 7`: the accumulator housekeeping ops, in `y` order.
const ACC_OPS: [&str; 8] = ["rlca", "rrca", "rla", "rra", "daa", "cpl", "scf", "ccf"];

/// The `ED` page's `x = 2` quadrant: block transfer, search, and block I/O.
/// Rows are `y = 4..7`, columns `z = 0..3`.
const BLOCK: [[&str; 4]; 4] = [
    ["ldi", "cpi", "ini", "outi"],
    ["ldd", "cpd", "ind", "outd"],
    ["ldir", "cpir", "inir", "otir"],
    ["lddr", "cpdr", "indr", "otdr"],
];

/// The index-register prefixes. `DD` reinterprets every `HL` on the main page
/// as `IX` and every `(HL)` as `(IX+d)`; `FD` does the same for `IY`.
const PREFIX_IX: u8 = 0xdd;
const PREFIX_IY: u8 = 0xfd;
const PREFIX_CB: u8 = 0xcb;
const PREFIX_ED: u8 = 0xed;

// Opcode constructors, shared with the 8080 backend so the two never disagree
// about a byte value.

/// `LD r,r'` (8080 `MOV d,s`).
pub const fn ld_r_r(d: u8, s: u8) -> u8 {
    0x40 | d << 3 | s
}
/// `alu[op] r` (8080 `ADD`/`ANA`/...).
pub const fn alu_r(op: u8, r: u8) -> u8 {
    0x80 | op << 3 | r
}
/// `alu[op] n` (8080 `ADI`/`ANI`/...).
pub const fn alu_n(op: u8) -> u8 {
    0xc6 | op << 3
}
/// `LD r,n` (8080 `MVI`).
pub const fn ld_r_n(r: u8) -> u8 {
    0x06 | r << 3
}
/// `INC r` (8080 `INR`).
pub const fn inc_r(r: u8) -> u8 {
    0x04 | r << 3
}
/// `DEC r` (8080 `DCR`).
pub const fn dec_r(r: u8) -> u8 {
    0x05 | r << 3
}
/// `LD rp,nn` (8080 `LXI`).
pub const fn ld_rp_nn(p: u8) -> u8 {
    0x01 | p << 4
}
/// `ADD HL,rp` (8080 `DAD`).
pub const fn add_hl_rp(p: u8) -> u8 {
    0x09 | p << 4
}
/// `INC rp` (8080 `INX`).
pub const fn inc_rp(p: u8) -> u8 {
    0x03 | p << 4
}
/// `DEC rp` (8080 `DCX`).
pub const fn dec_rp(p: u8) -> u8 {
    0x0b | p << 4
}
/// `PUSH rp2` (8080 `PUSH`).
pub const fn push_rp2(p: u8) -> u8 {
    0xc5 | p << 4
}
/// `POP rp2` (8080 `POP`).
pub const fn pop_rp2(p: u8) -> u8 {
    0xc1 | p << 4
}
/// `RET cc` (8080 `RNZ`/`RZ`/...).
pub const fn ret_cc(c: u8) -> u8 {
    0xc0 | c << 3
}
/// `JP cc,nn` (8080 `JNZ`/`JZ`/...).
pub const fn jp_cc(c: u8) -> u8 {
    0xc2 | c << 3
}
/// `CALL cc,nn` (8080 `CNZ`/`CZ`/...).
pub const fn call_cc(c: u8) -> u8 {
    0xc4 | c << 3
}
/// `RST t`, where `t` is the vector number and the target address is `t * 8`.
pub const fn rst(t: u8) -> u8 {
    0xc7 | t << 3
}
/// `LD (BC),A` / `LD (DE),A` for `p = 0`/`1`, and their `LD A,(rp)` mates.
pub const fn ld_mem_rp_a(p: u8, load: bool) -> u8 {
    0x02 | p << 4 | (load as u8) << 3
}
/// The `CB` page's `rot[y] r[z]`.
pub const fn cb_rot(y: u8, z: u8) -> u8 {
    y << 3 | z
}
/// The `CB` page's `BIT`/`RES`/`SET`: `x` is 1, 2 or 3.
pub const fn cb_bit(x: u8, bit: u8, z: u8) -> u8 {
    x << 6 | bit << 3 | z
}

fn position(table: &[&str], name: &str) -> Option<u8> {
    table.iter().position(|s| *s == name).map(|i| i as u8)
}

/// The `r[]` field for a named 8-bit register. Slot 6 is `(HL)`, which is not
/// a register name, so it can never come from here.
fn r8_of(name: &str) -> Option<u8> {
    position(&R, name).filter(|i| *i != 6)
}

/// Names that are never symbols: every register, pair and condition.
fn is_reserved(name: &str) -> bool {
    R.contains(&name)
        || RP.contains(&name)
        || RP2.contains(&name)
        || CC.contains(&name)
        || matches!(
            name,
            "i" | "r" | "ix" | "iy" | "af'" | "ixh" | "ixl" | "iyh" | "iyl"
        )
}

/// The `alu[]` field of an ALU mnemonic. Only called with names the
/// dispatcher has already matched against that table.
fn alu_index(m: &str) -> u8 {
    position(&ALU, m).unwrap_or(0)
}

fn index_prefix(name: &str) -> Option<u8> {
    match name {
        "ix" => Some(PREFIX_IX),
        "iy" => Some(PREFIX_IY),
        _ => None,
    }
}

// ---- operands -------------------------------------------------------------

/// A parenthesised operand.
#[derive(Clone)]
enum Mem {
    /// `(hl)`, `(bc)`, `(de)`, `(sp)`, `(c)`, `(ix)`, `(iy)`.
    Reg(String),
    /// `(ix+d)` / `(iy-d)`: the prefix byte and the displacement expression.
    Idx(u8, ExprRef),
    /// `(nn)`.
    Addr(ExprRef),
}

/// One operand, kept in every form it might be needed in: `c` is register C
/// in `in c,(c)` and the carry condition in `jp c,nn`, and only the mnemonic
/// decides which.
struct Arg {
    /// The lowercased identifier, when the operand is exactly one.
    name: Option<String>,
    mem: Option<Mem>,
    /// The operand read as an expression; `None` for a parenthesised operand.
    expr: Option<ExprRef>,
    span: Span,
}

impl Arg {
    fn is(&self, name: &str) -> bool {
        self.name.as_deref() == Some(name)
    }

    /// The `rp[]`/index-register this operand names, as (prefix, field).
    fn wide(&self, table: &[&str; 4]) -> Option<(Option<u8>, u8)> {
        let name = self.name.as_deref()?;
        if let Some(p) = index_prefix(name) {
            // `IX` stands in for `HL`, so it takes HL's field value.
            return Some((Some(p), 2));
        }
        position(table, name).map(|p| (None, p))
    }

    fn cond(&self) -> Option<u8> {
        position(&CC, self.name.as_deref()?)
    }
}

/// A resolved 8-bit operand: its `r[]` field plus, when it is `(IX+d)`, the
/// prefix and displacement that have to be emitted around the opcode.
struct Slot {
    field: u8,
    idx: Option<(u8, Option<ExprRef>)>,
    /// The prefix of an index register half, `IXH` to `IYL`: the `H` or `L`
    /// field read through `DD` or `FD`, with no displacement.
    half: Option<u8>,
    span: Span,
}

impl Slot {
    fn prefix(&self) -> Option<u8> {
        self.idx.as_ref().map(|(p, _)| *p)
    }
}

fn slot_of(arg: &Arg) -> Option<Slot> {
    if let Some(name) = arg.name.as_deref()
        && let Some(f) = r8_of(name)
    {
        return Some(Slot {
            field: f,
            idx: None,
            half: None,
            span: arg.span,
        });
    }
    if let Some((reg, half)) = arg.name.as_deref().and_then(|n| n.split_at_checked(2))
        && let Some(prefix) = index_prefix(reg)
        && let Some(field) = position(&["h", "l"], half)
    {
        return Some(Slot {
            field: 4 + field,
            idx: None,
            half: Some(prefix),
            span: arg.span,
        });
    }
    match arg.mem.as_ref()? {
        Mem::Reg(r) if r == "hl" => Some(Slot {
            field: 6,
            idx: None,
            half: None,
            span: arg.span,
        }),
        // A bare `(ix)` is `(ix+0)`; the displacement byte is still there.
        Mem::Reg(r) => index_prefix(r).map(|p| Slot {
            field: 6,
            idx: Some((p, None)),
            half: None,
            span: arg.span,
        }),
        Mem::Idx(p, d) => Some(Slot {
            field: 6,
            idx: Some((*p, Some(*d))),
            half: None,
            span: arg.span,
        }),
        Mem::Addr(_) => None,
    }
}

/// Emits `[prefix] opcode [d]` — the shape of every main-page instruction
/// that can carry an `(IX+d)` operand.
fn emit_slot(enc: &mut Enc, slot: &Slot, opcode: u8) {
    if let Some(prefix) = slot.half {
        enc.byte(prefix);
    }
    match &slot.idx {
        Some((prefix, disp)) => {
            enc.byte(*prefix);
            enc.byte(opcode);
            match disp {
                Some(e) => enc.disp8(*e, slot.span),
                None => enc.byte(0),
            }
        }
        None => enc.byte(opcode),
    }
}

fn parse_args(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Arg>> {
    let mut out = Vec::new();
    for part in common::operands(insn.operands) {
        let span = common::span_of(part, insn.span);
        if part.is_empty() {
            cx.error(span, "expected an operand");
            return None;
        }
        if common::parenthesised(part) {
            out.push(parse_mem(cx, common::inside_parens(part), span)?);
            continue;
        }
        let name = common::sole_ident(cx, part);
        // Register and condition names are reserved words in Zilog syntax. If
        // they were also allowed to read as symbols, `ld (ix+1),hl` would
        // quietly become a load of an undefined symbol `hl`.
        let expr = match name.as_deref() {
            Some(n) if is_reserved(n) => None,
            _ => Some(common::expr_of(cx, part, span)?),
        };
        out.push(Arg {
            name,
            mem: None,
            expr,
            span,
        });
    }
    Some(out)
}

/// Parses the inside of a `( )` operand.
fn parse_mem(cx: &mut AsmCtx<'_>, inner: &[Token], span: Span) -> Option<Arg> {
    let mem = if let Some(name) = common::sole_ident(cx, inner) {
        Mem::Reg(name)
    } else if let Some(idx) = indexed(cx, inner, span)? {
        idx
    } else {
        Mem::Addr(common::expr_of(cx, inner, span)?)
    };
    Some(Arg {
        name: None,
        mem: Some(mem),
        expr: None,
        span,
    })
}

/// Recognises `ix+d` / `iy-d`. The displacement is parsed starting *at* the
/// sign, so the expression parser's unary minus supplies the sign and
/// `(ix-1-1)` means what it says.
fn indexed(cx: &mut AsmCtx<'_>, inner: &[Token], span: Span) -> Option<Option<Mem>> {
    let [first, rest @ ..] = inner else {
        return Some(None);
    };
    let Some(second) = rest.first() else {
        return Some(None);
    };
    let Some(prefix) = first
        .ident()
        .and_then(|n| index_prefix(&cx.name(n).to_ascii_lowercase()))
    else {
        return Some(None);
    };
    if !second.is_punct(Punct::Plus) && !second.is_punct(Punct::Minus) {
        return Some(None);
    }
    let disp = common::expr_of(cx, rest, span)?;
    Some(Some(Mem::Idx(prefix, disp)))
}

// ---- encoding -------------------------------------------------------------

pub fn assemble(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>, m: &str) -> Option<Vec<Variant>> {
    if let Some(bytes) = nullary(m) {
        if !insn.operands.is_empty() {
            cx.error(insn.span, format!("`{m}` takes no operands"));
            return None;
        }
        return Enc::op(&bytes).done();
    }
    let args = parse_args(cx, insn)?;
    encode(cx, insn, m, &args)
}

/// Instructions with no operands at all: the main page's housekeeping ops and
/// the whole fixed part of the `ED` page.
fn nullary(m: &str) -> Option<Vec<u8>> {
    if let Some(y) = position(&ACC_OPS, m) {
        return Some(vec![y << 3 | 0x07]);
    }
    for (row, names) in BLOCK.iter().enumerate() {
        if let Some(z) = position(names, m) {
            // `x = 2`, `y = row + 4`.
            return Some(vec![PREFIX_ED, 0xa0 | (row as u8) << 3 | z]);
        }
    }
    Some(match m {
        "nop" => vec![0x00],
        "halt" => vec![0x76],
        "di" => vec![0xf3],
        "ei" => vec![0xfb],
        "exx" => vec![0xd9],
        "neg" => vec![PREFIX_ED, 0x44],
        "retn" => vec![PREFIX_ED, 0x45],
        "reti" => vec![PREFIX_ED, 0x4d],
        "rrd" => vec![PREFIX_ED, 0x67],
        "rld" => vec![PREFIX_ED, 0x6f],
        _ => return None,
    })
}

fn encode(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    args: &[Arg],
) -> Option<Vec<Variant>> {
    let span = insn.span;
    match m {
        "ld" => match args {
            [dst, src] => ld(cx, m, dst, src),
            _ => common::bad_operands(cx, span, m),
        },
        "ex" => match args {
            // `EX AF,AF'`, with or without the prime; see the module comment.
            [a, b] if a.is("af") && (b.is("af'") || b.is("af")) => Enc::op(&[0x08]).done(),
            [a, b] if a.is("de") && b.is("hl") => Enc::op(&[0xeb]).done(),
            [a, b] if matches!(&a.mem, Some(Mem::Reg(r)) if r == "sp") => match b.wide(&RP) {
                Some((prefix, 2)) => {
                    let mut enc = Enc::new();
                    if let Some(p) = prefix {
                        enc.byte(p);
                    }
                    enc.byte(0xe3);
                    enc.done()
                }
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "add" | "adc" | "sbc" => {
            let op = alu_index(m);
            // The 16-bit forms come first: `ADD HL,rp` is on the main page,
            // `ADC HL,rp` and `SBC HL,rp` on the `ED` page, and none of them
            // shares a shape with the 8-bit group.
            if let [dst, src] = args
                && let Some((prefix, 2)) = dst.wide(&RP)
                && dst.name.as_deref() != Some("a")
                && let Some((src_prefix, p)) = src.wide(&RP)
            {
                return wide_add(cx, m, op, prefix, src_prefix, p, span);
            }
            alu(cx, m, op, args, span)
        }
        "sub" | "and" | "xor" | "or" | "cp" => alu(cx, m, alu_index(m), args, span),
        "inc" | "dec" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let up = m == "inc";
            if let Some((prefix, p)) = arg.wide(&RP) {
                let mut enc = Enc::new();
                if let Some(x) = prefix {
                    enc.byte(x);
                }
                enc.byte(if up { inc_rp(p) } else { dec_rp(p) });
                return enc.done();
            }
            let Some(slot) = slot_of(arg) else {
                return common::bad_operands(cx, span, m);
            };
            let mut enc = Enc::new();
            let opcode = if up {
                inc_r(slot.field)
            } else {
                dec_r(slot.field)
            };
            emit_slot(&mut enc, &slot, opcode);
            enc.done()
        }
        "push" | "pop" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some((prefix, p)) = arg.wide(&RP2) else {
                return common::bad_operands(cx, span, m);
            };
            let mut enc = Enc::new();
            if let Some(x) = prefix {
                enc.byte(x);
            }
            enc.byte(if m == "push" { push_rp2(p) } else { pop_rp2(p) });
            enc.done()
        }
        "jp" => match args {
            // `JP (HL)` is not a memory read: it loads PC from HL.
            [a] => match &a.mem {
                Some(Mem::Reg(r)) if r == "hl" => Enc::op(&[0xe9]).done(),
                // `JP (IX)` is the same instruction behind a prefix.
                Some(Mem::Reg(r)) => match index_prefix(r) {
                    Some(prefix) => Enc::op(&[prefix, 0xe9]).done(),
                    None => common::bad_operands(cx, span, m),
                },
                _ => jump(cx, m, 0xc3, a, span),
            },
            [c, a] => match c.cond() {
                Some(cc) => jump(cx, m, jp_cc(cc), a, span),
                None => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "call" => match args {
            [a] => jump(cx, m, 0xcd, a, span),
            [c, a] => match c.cond() {
                Some(cc) => jump(cx, m, call_cc(cc), a, span),
                None => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "ret" => match args {
            [] => Enc::op(&[0xc9]).done(),
            [c] => match c.cond() {
                Some(cc) => Enc::op(&[ret_cc(cc)]).done(),
                None => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "jr" | "djnz" => {
            // `x = 0, z = 0`: `DJNZ` is `y = 2`, unconditional `JR` is
            // `y = 3`, and `JR cc` is `y = 4..7` — so only the first four
            // conditions have a relative form.
            let (target, opcode) = match (m, args) {
                ("djnz", [a]) => (a, 0x10),
                ("jr", [a]) => (a, 0x18),
                ("jr", [c, a]) => match c.cond() {
                    Some(cc) if cc < 4 => (a, 0x20 | cc << 3),
                    Some(_) => {
                        cx.error(
                            c.span,
                            "`jr` only has the conditions `nz`, `z`, `nc` and `c`",
                        );
                        return None;
                    }
                    None => return common::bad_operands(cx, span, m),
                },
                _ => return common::bad_operands(cx, span, m),
            };
            let Some(e) = target.expr else {
                return common::bad_operands(cx, span, m);
            };
            let mut enc = Enc::op(&[opcode]);
            enc.rel8(e, target.span);
            enc.done()
        }
        "rst" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = arg.expr else {
                return common::bad_operands(cx, span, m);
            };
            let Some(v) = cx.constant(e) else {
                cx.error(arg.span, "an `rst` target must be a constant known here");
                return None;
            };
            if !(0..=0x38).contains(&v) || v % 8 != 0 {
                cx.error(
                    arg.span,
                    format!("an `rst` target must be one of 0, 8, 16, 24, 32, 40, 48, 56, not {v}"),
                );
                return None;
            }
            Enc::op(&[rst(v as u8 / 8)]).done()
        }
        "im" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = arg.expr else {
                return common::bad_operands(cx, span, m);
            };
            let Some(v) = cx.constant(e) else {
                cx.error(arg.span, "an interrupt mode must be a constant known here");
                return None;
            };
            // The three modes are not consecutive on the `ED` page: they sit
            // at y = 0, 2 and 3 of the `z = 6` column.
            let opcode = match v {
                0 => 0x46,
                1 => 0x56,
                2 => 0x5e,
                _ => {
                    cx.error(
                        arg.span,
                        format!("interrupt mode must be 0, 1 or 2, not {v}"),
                    );
                    return None;
                }
            };
            Enc::op(&[PREFIX_ED, opcode]).done()
        }
        "in" => match args {
            // `IN F,(C)`, also written `IN (C)`, sets the flags and keeps
            // nothing (undocumented).
            [src] if matches!(&src.mem, Some(Mem::Reg(r)) if r == "c") => {
                Enc::op(&[PREFIX_ED, 0x70]).done()
            }
            [dst, src] if dst.is("f") && matches!(&src.mem, Some(Mem::Reg(r)) if r == "c") => {
                Enc::op(&[PREFIX_ED, 0x70]).done()
            }
            // `IN A,(n)` reads port `n`; `IN r,(C)` reads the port in BC and
            // is a different page entirely.
            [dst, src] => match &src.mem {
                Some(Mem::Reg(r)) if r == "c" => match dst.name.as_deref().and_then(r8_of) {
                    Some(f) => Enc::op(&[PREFIX_ED, 0x40 | f << 3]).done(),
                    None => common::bad_operands(cx, span, m),
                },
                Some(Mem::Addr(e)) if dst.is("a") => {
                    let mut enc = Enc::op(&[0xdb]);
                    enc.imm8(*e, src.span);
                    enc.done()
                }
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        "out" => match args {
            // `OUT (C),0` (undocumented): the `y = 6` slot that would be
            // `OUT (C),(HL)`.
            [dst, src]
                if matches!(&dst.mem, Some(Mem::Reg(r)) if r == "c")
                    && src.name.is_none()
                    && src.mem.is_none() =>
            {
                match src.expr.and_then(|e| cx.constant(e)) {
                    Some(0) => Enc::op(&[PREFIX_ED, 0x71]).done(),
                    _ => {
                        cx.error(src.span, "`out (c),` takes a register or 0");
                        None
                    }
                }
            }
            [dst, src] => match &dst.mem {
                Some(Mem::Reg(r)) if r == "c" => match src.name.as_deref().and_then(r8_of) {
                    Some(f) => Enc::op(&[PREFIX_ED, 0x41 | f << 3]).done(),
                    None => common::bad_operands(cx, span, m),
                },
                Some(Mem::Addr(e)) if src.is("a") => {
                    let mut enc = Enc::op(&[0xd3]);
                    enc.imm8(*e, dst.span);
                    enc.done()
                }
                _ => common::bad_operands(cx, span, m),
            },
            _ => common::bad_operands(cx, span, m),
        },
        _ => {
            if let Some(y) = position(&ROT, m) {
                let [arg] = args else {
                    return common::bad_operands(cx, span, m);
                };
                let Some(slot) = slot_of(arg).filter(|s| s.half.is_none()) else {
                    return common::bad_operands(cx, span, m);
                };
                return cb(&slot, cb_rot(y, slot.field));
            }
            if let Some(x) = match m {
                "bit" => Some(1),
                "res" => Some(2),
                "set" => Some(3),
                _ => None,
            } {
                let [n, arg] = args else {
                    return common::bad_operands(cx, span, m);
                };
                let Some(e) = n.expr else {
                    return common::bad_operands(cx, span, m);
                };
                let Some(v) = cx.constant(e) else {
                    cx.error(n.span, "a bit number must be a constant known here");
                    return None;
                };
                if !(0..=7).contains(&v) {
                    cx.error(n.span, format!("a bit number must be 0 to 7, not {v}"));
                    return None;
                }
                let Some(slot) = slot_of(arg).filter(|s| s.half.is_none()) else {
                    return common::bad_operands(cx, span, m);
                };
                let opcode = cb_bit(x, v as u8, slot.field);
                return cb(&slot, opcode);
            }
            common::unknown(cx, insn.mnemonic_span, "Z80", m)
        }
    }
}

/// Emits a `CB`-page instruction, either `CB op` or the four-byte
/// `DD CB d op`. Note the order: on the index pages the displacement comes
/// *before* the opcode, which is the one place the prefix scheme is not a
/// simple insertion.
fn cb(slot: &Slot, opcode: u8) -> Option<Vec<Variant>> {
    let mut enc = Enc::new();
    match &slot.idx {
        Some((prefix, disp)) => {
            enc.byte(*prefix);
            enc.byte(PREFIX_CB);
            match disp {
                Some(e) => enc.disp8(*e, slot.span),
                None => enc.byte(0),
            }
            enc.byte(opcode);
        }
        None => {
            enc.byte(PREFIX_CB);
            enc.byte(opcode);
        }
    }
    enc.done()
}

/// `JP nn` / `CALL nn` and their conditional forms.
fn jump(
    cx: &mut AsmCtx<'_>,
    m: &str,
    opcode: u8,
    target: &Arg,
    span: Span,
) -> Option<Vec<Variant>> {
    let Some(e) = target.expr else {
        return common::bad_operands(cx, span, m);
    };
    let mut enc = Enc::op(&[opcode]);
    enc.imm16(e, target.span);
    enc.done()
}

/// `ADD HL,rp`, `ADC HL,rp`, `SBC HL,rp` and the `IX`/`IY` forms of the first.
fn wide_add(
    cx: &mut AsmCtx<'_>,
    m: &str,
    op: u8,
    prefix: Option<u8>,
    src_prefix: Option<u8>,
    p: u8,
    span: Span,
) -> Option<Vec<Variant>> {
    // `ADD IX,IX` is legal but `ADD IX,HL` is not: with a prefix in force the
    // `rp = 2` slot *is* the index register, so the two must agree.
    if src_prefix.is_some() && src_prefix != prefix {
        cx.error(span, format!("`{m}` cannot mix `hl`, `ix` and `iy`"));
        return None;
    }
    if prefix.is_some() && p == 2 && src_prefix.is_none() {
        cx.error(span, format!("`{m}` cannot mix `hl`, `ix` and `iy`"));
        return None;
    }
    let mut enc = Enc::new();
    match op {
        // ADD
        0 => {
            if let Some(x) = prefix {
                enc.byte(x);
            }
            enc.byte(add_hl_rp(p));
        }
        // ADC and SBC are `ED` page only, so they have no index form.
        1 | 3 => {
            if prefix.is_some() {
                cx.error(span, format!("`{m}` has no `ix`/`iy` form"));
                return None;
            }
            enc.byte(PREFIX_ED);
            enc.byte(if op == 1 {
                0x4a | p << 4
            } else {
                0x42 | p << 4
            });
        }
        _ => return common::bad_operands(cx, span, m),
    }
    enc.done()
}

/// The 8-bit ALU group. The accumulator may be named or left implicit, so
/// `add a,b` and `add b` are the same instruction.
fn alu(cx: &mut AsmCtx<'_>, m: &str, op: u8, args: &[Arg], span: Span) -> Option<Vec<Variant>> {
    let src = match args {
        [src] => src,
        [dst, src] if dst.is("a") => src,
        _ => return common::bad_operands(cx, span, m),
    };
    if let Some(slot) = slot_of(src) {
        let mut enc = Enc::new();
        emit_slot(&mut enc, &slot, alu_r(op, slot.field));
        return enc.done();
    }
    match src.expr {
        Some(e) => {
            let mut enc = Enc::op(&[alu_n(op)]);
            enc.imm8(e, src.span);
            enc.done()
        }
        None => common::bad_operands(cx, span, m),
    }
}

/// Every form of `LD`. Only `(hl)` and `(ix+d)` are slots of the 8-bit
/// matrix; `(bc)`, `(de)` and `(nn)` look the same in source but are separate
/// opcodes that only the accumulator or a 16-bit register can use, so they
/// are peeled off first.
fn ld(cx: &mut AsmCtx<'_>, m: &str, dst: &Arg, src: &Arg) -> Option<Vec<Variant>> {
    let span = dst.span.to(src.span);

    // The `ED` page's interrupt-vector and refresh-register transfers. `i`
    // and `r` are not 8-bit register names, so nothing else can match these.
    let ir = match (dst.name.as_deref(), src.name.as_deref()) {
        (Some("i"), Some("a")) => Some(0x47),
        (Some("r"), Some("a")) => Some(0x4f),
        (Some("a"), Some("i")) => Some(0x57),
        (Some("a"), Some("r")) => Some(0x5f),
        _ => None,
    };
    if let Some(opcode) = ir {
        return Enc::op(&[PREFIX_ED, opcode]).done();
    }

    // Stores through an address or a pair.
    if let Some(mem) = &dst.mem {
        match mem {
            Mem::Addr(e) => {
                if src.is("a") {
                    let mut enc = Enc::op(&[0x32]);
                    enc.imm16(*e, dst.span);
                    return enc.done();
                }
                let Some((prefix, p)) = src.wide(&RP) else {
                    return common::bad_operands(cx, span, m);
                };
                let mut enc = Enc::new();
                if p == 2 {
                    if let Some(x) = prefix {
                        enc.byte(x);
                    }
                    enc.byte(0x22);
                } else {
                    // `LD (nn),BC/DE/SP` only exists on the `ED` page.
                    enc.byte(PREFIX_ED);
                    enc.byte(0x43 | p << 4);
                }
                enc.imm16(*e, dst.span);
                return enc.done();
            }
            Mem::Reg(r) if (r == "bc" || r == "de") && src.is("a") => {
                let p = if r == "bc" { 0 } else { 1 };
                return Enc::op(&[ld_mem_rp_a(p, false)]).done();
            }
            // `(hl)` and `(ix+d)` fall through to the 8-bit matrix below.
            _ => {}
        }
    }

    // 16-bit destinations.
    if let Some((prefix, p)) = dst.wide(&RP) {
        if let Some(Mem::Addr(e)) = &src.mem {
            let mut enc = Enc::new();
            if p == 2 {
                if let Some(x) = prefix {
                    enc.byte(x);
                }
                enc.byte(0x2a);
            } else {
                enc.byte(PREFIX_ED);
                enc.byte(0x4b | p << 4);
            }
            enc.imm16(*e, src.span);
            return enc.done();
        }
        // `LD SP,HL` is the only register-to-register 16-bit move.
        if let Some((src_prefix, sp)) = src.wide(&RP)
            && src.mem.is_none()
        {
            if p == 3 && sp == 2 {
                let mut enc = Enc::new();
                if let Some(x) = src_prefix {
                    enc.byte(x);
                }
                enc.byte(0xf9);
                return enc.done();
            }
            return common::bad_operands(cx, span, m);
        }
        let Some(e) = src.expr else {
            return common::bad_operands(cx, span, m);
        };
        let mut enc = Enc::new();
        if let Some(x) = prefix {
            enc.byte(x);
        }
        enc.byte(ld_rp_nn(p));
        enc.imm16(e, src.span);
        return enc.done();
    }

    // Loads from an address or a pair, which only the accumulator can do.
    if let Some(mem) = &src.mem {
        match mem {
            Mem::Addr(e) if dst.is("a") => {
                let mut enc = Enc::op(&[0x3a]);
                enc.imm16(*e, src.span);
                return enc.done();
            }
            Mem::Reg(r) if (r == "bc" || r == "de") && dst.is("a") => {
                let p = if r == "bc" { 0 } else { 1 };
                return Enc::op(&[ld_mem_rp_a(p, true)]).done();
            }
            _ => {}
        }
    }

    // The 8-bit matrix.
    let Some(d) = slot_of(dst) else {
        return common::bad_operands(cx, span, m);
    };
    if let Some(s) = slot_of(src) {
        if d.prefix().is_some() && s.prefix().is_some() {
            cx.error(span, "only one operand may be indexed by `ix` or `iy`");
            return None;
        }
        if d.field == 6 && s.field == 6 {
            // `LD (HL),(HL)` would be opcode 0x76, which is HALT — and with a
            // prefix, `LD (IX+d),(HL)` would be `DD 76`, which is HALT too.
            cx.error(
                span,
                "`ld (hl),(hl)` does not exist; that encoding is `halt`",
            );
            return None;
        }
        // The prefix that makes `H` read as `IXH` makes it read that way on
        // both sides, and makes `(HL)` read as `(IX+d)`.
        if let Some(prefix) = d.half.or(s.half) {
            let other = if d.half.is_some() { &s } else { &d };
            let clash = match other.half {
                Some(p) => p != prefix,
                None => other.field >= 4 && other.field <= 6,
            };
            if clash {
                cx.error(
                    span,
                    "an index register half can only be loaded to or from `a` to `e`, a number, \
                     or a half of the same register",
                );
                return None;
            }
        }
        let indexed = if d.prefix().is_some() || d.half.is_some() {
            &d
        } else {
            &s
        };
        let mut enc = Enc::new();
        emit_slot(&mut enc, indexed, ld_r_r(d.field, s.field));
        return enc.done();
    }
    let Some(e) = src.expr else {
        return common::bad_operands(cx, span, m);
    };
    let mut enc = Enc::new();
    emit_slot(&mut enc, &d, ld_r_n(d.field));
    enc.imm8(e, src.span);
    enc.done()
}

/// Whether `name` is a Z80 mnemonic at all, for the 8080 backend's "did you
/// mean the Z80 spelling?" diagnostics and for the table tests.
pub fn is_mnemonic(name: &str) -> bool {
    if nullary(name).is_some() || position(&ROT, name).is_some() {
        return true;
    }
    matches!(
        name,
        "ld" | "ex"
            | "add"
            | "adc"
            | "sub"
            | "sbc"
            | "and"
            | "xor"
            | "or"
            | "cp"
            | "inc"
            | "dec"
            | "push"
            | "pop"
            | "jp"
            | "jr"
            | "djnz"
            | "call"
            | "ret"
            | "rst"
            | "im"
            | "in"
            | "out"
            | "bit"
            | "res"
            | "set"
    )
}
