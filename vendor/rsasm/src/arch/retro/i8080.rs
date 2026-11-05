//! The Intel 8080, in Intel mnemonics.
//!
//! # Where the table comes from
//!
//! Intel, *8080/8085 Assembly Language Programming Manual* (order number
//! 9800301): the instruction-set chapter, whose per-instruction entries give
//! each opcode's bit pattern with its `DDD`/`SSS` register and `RP` pair
//! fields, and the instruction summary that lists all 244 opcodes.
//!
//! # Its relationship to the Z80 table
//!
//! The Z80 was designed to be binary compatible with the 8080: every 8080
//! opcode has the same value on the Z80, and only the *spelling* differs
//! (`MOV A,B` versus `LD A,B`, `MVI` versus `LD r,n`, `JNZ` versus `JP NZ,`).
//! So none of the byte values live here. This file is a table of Intel
//! mnemonics over the opcode constructors in [`super::z80`] — `ld_r_r`,
//! `alu_r`, `ld_rp_nn` and the rest — which is the same `xx yyy zzz`
//! decomposition read with Intel's register names:
//!
//! ```text
//!   Z80  r[]  = B C D E H L (HL) A      8080  B C D E H L M A
//!   Z80  rp[] = BC DE HL SP             8080  B D H SP
//!   Z80  rp2  = BC DE HL AF             8080  B D H PSW
//! ```
//!
//! Twelve Z80 opcodes have no 8080 instruction (`EX AF,AF'`, `EXX`, `DJNZ`,
//! `JR` and its four conditional forms, and the four prefix bytes), and this
//! table simply does not name them.

use super::common::{self, Enc};
use super::z80;
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::ExprRef;
use crate::section::Variant;
use crate::source::Span;

/// The 8080's spelling of `r[]`: `M` is the `(HL)` slot.
const R: [&str; 8] = ["b", "c", "d", "e", "h", "l", "m", "a"];

/// The 8080's spelling of `rp[]`: a pair is named after its high byte.
const RP: [&str; 4] = ["b", "d", "h", "sp"];

/// The 8080's spelling of `rp2[]`, as `PUSH` and `POP` see it. `PSW` is the
/// accumulator and flags together.
const RP2: [&str; 4] = ["b", "d", "h", "psw"];

/// The register-to-register ALU group, in `alu[]` order, and its
/// immediate-operand twin. Intel gives the two forms different mnemonics
/// where Zilog gives them different operands.
const ALU_R: [&str; 8] = ["add", "adc", "sub", "sbb", "ana", "xra", "ora", "cmp"];
const ALU_I: [&str; 8] = ["adi", "aci", "sui", "sbi", "ani", "xri", "ori", "cpi"];

/// Conditional returns, jumps and calls, in `cc[]` order (NZ, Z, NC, C, PO,
/// PE, P, M). `RET`, `JMP` and `CALL` themselves are separate opcodes.
const RET_CC: [&str; 8] = ["rnz", "rz", "rnc", "rc", "rpo", "rpe", "rp", "rm"];
const JMP_CC: [&str; 8] = ["jnz", "jz", "jnc", "jc", "jpo", "jpe", "jp", "jm"];
const CALL_CC: [&str; 8] = ["cnz", "cz", "cnc", "cc", "cpo", "cpe", "cp", "cm"];

/// The single-byte instructions with no operand, as (mnemonic, opcode). These
/// are the `x = 0, z = 7` column plus the handful of `x = 3` corners; the
/// opcodes are written out because there is no field to compute them from.
const NULLARY: [(&str, u8); 17] = [
    ("nop", 0x00),
    ("rlc", 0x07),
    ("rrc", 0x0f),
    ("ral", 0x17),
    ("rar", 0x1f),
    ("daa", 0x27),
    ("cma", 0x2f),
    ("stc", 0x37),
    ("cmc", 0x3f),
    ("hlt", 0x76),
    ("ret", 0xc9),
    ("pchl", 0xe9),
    ("xthl", 0xe3),
    ("xchg", 0xeb),
    ("sphl", 0xf9),
    ("di", 0xf3),
    ("ei", 0xfb),
];

/// The instructions with a 16-bit address operand that are not conditional
/// jumps or calls.
const ADDR_OPS: [(&str, u8); 6] = [
    ("shld", 0x22),
    ("lhld", 0x2a),
    ("sta", 0x32),
    ("lda", 0x3a),
    ("jmp", 0xc3),
    ("call", 0xcd),
];

fn position(table: &[&str], name: &str) -> Option<u8> {
    table.iter().position(|s| *s == name).map(|i| i as u8)
}

/// One operand as the 8080 grammar sees it: a register name or an expression.
struct Arg {
    name: Option<String>,
    expr: Option<ExprRef>,
    span: Span,
}

impl Arg {
    fn reg(&self, table: &[&str]) -> Option<u8> {
        position(table, self.name.as_deref()?)
    }
}

fn parse_args(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Arg>> {
    let mut out = Vec::new();
    for part in common::operands(insn.operands) {
        let span = common::span_of(part, insn.span);
        let name = common::sole_ident(cx, part);
        // Register names are reserved, so `mvi a,b` is an error rather than
        // a load of an undefined symbol `b`.
        let expr = match name.as_deref() {
            Some(n) if R.contains(&n) || RP.contains(&n) || RP2.contains(&n) => None,
            _ => Some(common::expr_of(cx, part, span)?),
        };
        out.push(Arg { name, expr, span });
    }
    Some(out)
}

pub fn assemble(cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>, m: &str) -> Option<Vec<Variant>> {
    if let Some((_, op)) = NULLARY.iter().find(|(n, _)| *n == m) {
        if !insn.operands.is_empty() {
            cx.error(insn.span, format!("`{m}` takes no operands"));
            return None;
        }
        return Enc::op(&[*op]).done();
    }
    let args = parse_args(cx, insn)?;
    encode(cx, insn, m, &args)
}

fn encode(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    m: &str,
    args: &[Arg],
) -> Option<Vec<Variant>> {
    let span = insn.span;

    // Two-register `MOV`, which is the whole `x = 1` quadrant.
    if m == "mov" {
        let [dst, src] = args else {
            return common::bad_operands(cx, span, m);
        };
        let (Some(d), Some(s)) = (dst.reg(&R), src.reg(&R)) else {
            return common::bad_operands(cx, span, m);
        };
        if d == 6 && s == 6 {
            cx.error(span, "`mov m,m` does not exist; that encoding is `hlt`");
            return None;
        }
        return Enc::op(&[z80::ld_r_r(d, s)]).done();
    }

    if let Some(op) = position(&ALU_R, m) {
        let [src] = args else {
            return common::bad_operands(cx, span, m);
        };
        let Some(r) = src.reg(&R) else {
            return common::bad_operands(cx, span, m);
        };
        return Enc::op(&[z80::alu_r(op, r)]).done();
    }

    if let Some(op) = position(&ALU_I, m) {
        return byte_operand(cx, m, args, z80::alu_n(op), span);
    }

    if let Some(cc) = position(&RET_CC, m) {
        if !args.is_empty() {
            return common::bad_operands(cx, span, m);
        }
        return Enc::op(&[z80::ret_cc(cc)]).done();
    }
    if let Some(cc) = position(&JMP_CC, m) {
        return addr_operand(cx, m, args, z80::jp_cc(cc), span);
    }
    if let Some(cc) = position(&CALL_CC, m) {
        return addr_operand(cx, m, args, z80::call_cc(cc), span);
    }
    if let Some((_, op)) = ADDR_OPS.iter().find(|(n, _)| *n == m) {
        return addr_operand(cx, m, args, *op, span);
    }

    match m {
        "mvi" => match args {
            [dst, n] => {
                let Some(r) = dst.reg(&R) else {
                    return common::bad_operands(cx, span, m);
                };
                let Some(e) = n.expr else {
                    return common::bad_operands(cx, span, m);
                };
                let mut enc = Enc::op(&[z80::ld_r_n(r)]);
                enc.imm8(e, n.span);
                enc.done()
            }
            _ => common::bad_operands(cx, span, m),
        },
        "lxi" => match args {
            [dst, nn] => {
                let Some(p) = dst.reg(&RP) else {
                    return common::bad_operands(cx, span, m);
                };
                let Some(e) = nn.expr else {
                    return common::bad_operands(cx, span, m);
                };
                let mut enc = Enc::op(&[z80::ld_rp_nn(p)]);
                enc.imm16(e, nn.span);
                enc.done()
            }
            _ => common::bad_operands(cx, span, m),
        },
        "inr" | "dcr" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(r) = arg.reg(&R) else {
                return common::bad_operands(cx, span, m);
            };
            let op = if m == "inr" {
                z80::inc_r(r)
            } else {
                z80::dec_r(r)
            };
            Enc::op(&[op]).done()
        }
        "inx" | "dcx" | "dad" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(p) = arg.reg(&RP) else {
                return common::bad_operands(cx, span, m);
            };
            let op = match m {
                "inx" => z80::inc_rp(p),
                "dcx" => z80::dec_rp(p),
                _ => z80::add_hl_rp(p),
            };
            Enc::op(&[op]).done()
        }
        "push" | "pop" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(p) = arg.reg(&RP2) else {
                return common::bad_operands(cx, span, m);
            };
            let op = if m == "push" {
                z80::push_rp2(p)
            } else {
                z80::pop_rp2(p)
            };
            Enc::op(&[op]).done()
        }
        // Only BC and DE can be used indirectly, so the `p` field is one bit
        // wide here; `STAX H` is not an instruction.
        "stax" | "ldax" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let p = match arg.reg(&RP) {
                Some(p @ (0 | 1)) => p,
                _ => {
                    cx.error(arg.span, format!("`{m}` takes `b` or `d`"));
                    return None;
                }
            };
            Enc::op(&[z80::ld_mem_rp_a(p, m == "ldax")]).done()
        }
        "in" | "out" => byte_operand(cx, m, args, if m == "in" { 0xdb } else { 0xd3 }, span),
        "rst" => {
            let [arg] = args else {
                return common::bad_operands(cx, span, m);
            };
            let Some(e) = arg.expr else {
                return common::bad_operands(cx, span, m);
            };
            let Some(v) = cx.constant(e) else {
                cx.error(arg.span, "an `rst` vector must be a constant known here");
                return None;
            };
            // Intel numbers the vectors 0 to 7, where Zilog writes the target
            // address; `RST 1` here is `RST 08H` there.
            if !(0..=7).contains(&v) {
                cx.error(arg.span, format!("an `rst` vector must be 0 to 7, not {v}"));
                return None;
            }
            Enc::op(&[z80::rst(v as u8)]).done()
        }
        _ => {
            if z80::is_mnemonic(m) {
                cx.error(
                    insn.mnemonic_span,
                    format!("`{m}` is a Z80 mnemonic; the 8080 backend uses Intel mnemonics"),
                );
                return None;
            }
            common::unknown(cx, insn.mnemonic_span, "8080", m)
        }
    }
}

fn byte_operand(
    cx: &mut AsmCtx<'_>,
    m: &str,
    args: &[Arg],
    opcode: u8,
    span: Span,
) -> Option<Vec<Variant>> {
    let [arg] = args else {
        return common::bad_operands(cx, span, m);
    };
    let Some(e) = arg.expr else {
        return common::bad_operands(cx, span, m);
    };
    let mut enc = Enc::op(&[opcode]);
    enc.imm8(e, arg.span);
    enc.done()
}

fn addr_operand(
    cx: &mut AsmCtx<'_>,
    m: &str,
    args: &[Arg],
    opcode: u8,
    span: Span,
) -> Option<Vec<Variant>> {
    let [arg] = args else {
        return common::bad_operands(cx, span, m);
    };
    let Some(e) = arg.expr else {
        return common::bad_operands(cx, span, m);
    };
    let mut enc = Enc::op(&[opcode]);
    enc.imm16(e, arg.span);
    enc.done()
}

/// Whether `name`, lowercased, is an 8080 mnemonic.
pub fn is_mnemonic(name: &str) -> bool {
    let mut found = false;
    for_each_opcode(|m, _, _| found |= m == name);
    found
}

/// Walks the whole 8080 instruction set, calling `f` with each
/// (mnemonic, operand-byte count, opcode). The completeness test uses this to
/// check that the 244 defined opcodes are all reachable and all distinct.
pub fn for_each_opcode(mut f: impl FnMut(&'static str, u8, u8)) {
    for (d, _) in R.iter().enumerate() {
        for (s, _) in R.iter().enumerate() {
            if d == 6 && s == 6 {
                continue; // HLT
            }
            f("mov", 0, z80::ld_r_r(d as u8, s as u8));
        }
    }
    for (op, name) in ALU_R.iter().enumerate() {
        for r in 0..8u8 {
            f(name, 0, z80::alu_r(op as u8, r));
        }
    }
    for (op, name) in ALU_I.iter().enumerate() {
        f(name, 1, z80::alu_n(op as u8));
    }
    for r in 0..8u8 {
        f("mvi", 1, z80::ld_r_n(r));
        f("inr", 0, z80::inc_r(r));
        f("dcr", 0, z80::dec_r(r));
    }
    for p in 0..4u8 {
        f("lxi", 2, z80::ld_rp_nn(p));
        f("dad", 0, z80::add_hl_rp(p));
        f("inx", 0, z80::inc_rp(p));
        f("dcx", 0, z80::dec_rp(p));
        f("push", 0, z80::push_rp2(p));
        f("pop", 0, z80::pop_rp2(p));
    }
    for p in 0..2u8 {
        f("stax", 0, z80::ld_mem_rp_a(p, false));
        f("ldax", 0, z80::ld_mem_rp_a(p, true));
    }
    for cc in 0..8u8 {
        f(RET_CC[cc as usize], 0, z80::ret_cc(cc));
        f(JMP_CC[cc as usize], 2, z80::jp_cc(cc));
        f(CALL_CC[cc as usize], 2, z80::call_cc(cc));
    }
    for t in 0..8u8 {
        f("rst", 0, z80::rst(t));
    }
    for (name, opcode) in ADDR_OPS {
        f(name, 2, opcode);
    }
    f("in", 1, 0xdb);
    f("out", 1, 0xd3);
    for (name, opcode) in NULLARY {
        f(name, 0, opcode);
    }
}
