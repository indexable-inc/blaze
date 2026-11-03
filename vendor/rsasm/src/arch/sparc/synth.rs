//! Synthetic instructions.
//!
//! This is how SPARC assembly is actually written. The hardware has no `mov`,
//! no `cmp` and no `nop`; what it has is `%g0`, a register that reads as zero
//! and throws away writes, and almost every synthetic below is some real
//! instruction with `%g0` in one of its three slots:
//!
//! ```text
//! nop            sethi 0, %g0        write nothing, nowhere
//! mov b, a       or    %g0, b, a     add nothing to b
//! cmp a, b       subcc a, b, %g0     subtract for the flags, discard the result
//! tst a          orcc  a, %g0, %g0   likewise
//! clr a          or    %g0, %g0, a
//! ret            jmpl  %i7 + 8, %g0  jump, discarding the return address
//! ```
//!
//! `set` is the exception: it is the one synthetic that can expand to *two*
//! instructions, because a 32-bit constant does not fit in the 13-bit
//! immediate every format 3 instruction has to work with.

use super::encode::{self, Pending, Word};
use super::operand::{ImmPart, Operand};
use super::reg::{self, Reg};
use crate::arch::AsmCtx;
use crate::section::Variant;
use crate::source::Span;

/// `op3` values the synthetics expand into.
const OR: u8 = 0x02;
const ORCC: u8 = 0x12;
const ADD: u8 = 0x00;
const SUB: u8 = 0x04;
const SUBCC: u8 = 0x14;
const ANDCC: u8 = 0x11;
const ANDN: u8 = 0x05;
const XOR: u8 = 0x03;
const XNOR: u8 = 0x07;
const JMPL: u8 = 0x38;
const STW: u8 = 0x04;
// The other widths `clr` comes in, from the same op3 column as `STW`.
const STB: u8 = 0x05;
const STH: u8 = 0x06;
const STX: u8 = 0x0e;

pub fn is_synthetic(name: &str) -> bool {
    matches!(
        name,
        "nop"
            | "mov"
            | "cmp"
            | "tst"
            | "clr"
            | "clrb"
            | "clrh"
            | "clrx"
            | "not"
            | "neg"
            | "inc"
            | "dec"
            | "btst"
            | "bset"
            | "bclr"
            | "btog"
            | "set"
            | "ret"
            | "retl"
            | "jmp"
    )
}

pub fn assemble(cx: &mut AsmCtx<'_>, m: &str, span: Span, ops: &[Operand]) -> Option<Vec<Variant>> {
    match m {
        "nop" => {
            expect(cx, m, span, ops, 0)?;
            Some(encode::one(Word::plain(encode::format2(
                0,
                encode::OP2_SETHI,
                0,
            ))))
        }

        // `ret` returns from a routine that called `save`, so the return
        // address is in `%i7`; `retl` returns from a leaf, which never
        // rotated the window and still has it in `%o7`. Both skip the call's
        // delay slot, hence the `+ 8`.
        "ret" | "retl" => {
            expect(cx, m, span, ops, 0)?;
            let link = if m == "ret" { reg::I7 } else { reg::O7 };
            Some(encode::one(Word::plain(encode::format3(
                encode::OP_ALU,
                0,
                u32::from(JMPL),
                u32::from(link.num),
                encode::I_BIT | 8,
            ))))
        }

        "jmp" => {
            expect(cx, m, span, ops, 1)?;
            // `jmp` is `jmpl ..., %g0` and reads its target the same way,
            // which includes a bare value: that is `%g0 + value`.
            let addr = encode::target(cx, &ops[0])?;
            let (rs1, low, fixup) = encode::address(cx, &addr)?;
            Some(encode::one(word(
                encode::format3(encode::OP_ALU, 0, u32::from(JMPL), rs1, low),
                fixup,
            )))
        }

        "mov" => {
            expect(cx, m, span, ops, 2)?;
            mov(cx, ops)
        }

        "cmp" => {
            expect(cx, m, span, ops, 2)?;
            let rs1 = encode::int_reg(cx, &ops[0], "first source")?;
            alu(cx, SUBCC, rs1.num, &ops[1], 0)
        }

        "tst" => {
            expect(cx, m, span, ops, 1)?;
            let rs1 = encode::int_reg(cx, &ops[0], "operand")?;
            Some(encode::one(Word::plain(encode::format3(
                encode::OP_ALU,
                0,
                u32::from(ORCC),
                u32::from(rs1.num),
                0,
            ))))
        }

        "clr" | "clrb" | "clrh" | "clrx" => {
            expect(cx, m, span, ops, 1)?;
            // `clr [addr]` stores a zero word; `clrb`, `clrh` and `clrx`
            // store a byte, a half and a doubleword; `clr %reg` zeroes a
            // register.
            if let Some(rd) = ops[0].int_reg() {
                if m != "clr" {
                    cx.error(ops[0].span, format!("`{m}` takes an address"));
                    return None;
                }
                return Some(encode::one(Word::plain(encode::format3(
                    encode::OP_ALU,
                    u32::from(rd.num),
                    u32::from(OR),
                    0,
                    0,
                ))));
            }
            let Some(addr) = ops[0].as_addr() else {
                cx.error(ops[0].span, format!("`{m}` takes a register or an address"));
                return None;
            };
            let op3 = match m {
                "clrb" => STB,
                "clrh" => STH,
                "clrx" => STX,
                _ => STW,
            };
            let (rs1, low, fixup) = encode::address(cx, &addr)?;
            Some(encode::one(word(
                encode::format3(encode::OP_MEM, 0, u32::from(op3), rs1, low),
                fixup,
            )))
        }

        // `not` is `xnor x, %g0`; `neg` is `0 - x`. Written with one operand
        // they work in place.
        "not" | "neg" => {
            expect_range(cx, m, span, ops, &[1, 2])?;
            let src = encode::int_reg(cx, &ops[0], "source")?;
            let dst = if ops.len() == 2 {
                encode::int_reg(cx, &ops[1], "destination")?
            } else {
                src
            };
            let (op3, rs1, rs2) = if m == "not" {
                (XNOR, src.num, 0)
            } else {
                (SUB, 0, src.num)
            };
            Some(encode::one(Word::plain(encode::format3(
                encode::OP_ALU,
                u32::from(dst.num),
                u32::from(op3),
                u32::from(rs1),
                u32::from(rs2),
            ))))
        }

        // `inc`/`dec` default to a step of one: `inc %g1`, `inc 4, %g1`.
        "inc" | "dec" => {
            expect_range(cx, m, span, ops, &[1, 2])?;
            let op3 = if m == "inc" { ADD } else { SUB };
            let rd = encode::int_reg(cx, &ops[ops.len() - 1], "destination")?;
            if ops.len() == 1 {
                return Some(encode::one(Word::plain(encode::format3(
                    encode::OP_ALU,
                    u32::from(rd.num),
                    u32::from(op3),
                    u32::from(rd.num),
                    encode::I_BIT | 1,
                ))));
            }
            alu(cx, op3, rd.num, &ops[0], rd.num)
        }

        // The bit-twiddling group all read `value, register`, the opposite
        // order from the instructions they expand to.
        "btst" | "bset" | "bclr" | "btog" => {
            expect(cx, m, span, ops, 2)?;
            let rd = encode::int_reg(cx, &ops[1], "destination")?;
            let (op3, dst) = match m {
                "btst" => (ANDCC, 0),
                "bset" => (OR, rd.num),
                "bclr" => (ANDN, rd.num),
                _ => (XOR, rd.num),
            };
            alu(cx, op3, rd.num, &ops[0], dst)
        }

        "set" => {
            expect(cx, m, span, ops, 2)?;
            set(cx, ops)
        }

        _ => None,
    }
}

fn expect(cx: &mut AsmCtx<'_>, m: &str, span: Span, ops: &[Operand], n: usize) -> Option<()> {
    expect_range(cx, m, span, ops, &[n])
}

fn expect_range(
    cx: &mut AsmCtx<'_>,
    m: &str,
    span: Span,
    ops: &[Operand],
    want: &[usize],
) -> Option<()> {
    if want.contains(&ops.len()) {
        return Some(());
    }
    let want: Vec<String> = want.iter().map(|n| n.to_string()).collect();
    cx.error(
        span,
        format!(
            "`{m}` takes {} operand(s), but {} were given",
            want.join(" or "),
            ops.len()
        ),
    );
    None
}

fn word(w: u32, fixup: Option<Pending>) -> Word {
    match fixup {
        Some(f) => Word::fixed(w, f),
        None => Word::plain(w),
    }
}

/// `op3 rs1, src, rd` as a single format 3 word.
fn alu(cx: &mut AsmCtx<'_>, op3: u8, rs1: u8, src: &Operand, rd: u8) -> Option<Vec<Variant>> {
    let (low, fixup) = encode::source(cx, src)?;
    Some(encode::one(word(
        encode::format3(
            encode::OP_ALU,
            u32::from(rd),
            u32::from(op3),
            u32::from(rs1),
            low,
        ),
        fixup,
    )))
}

/// `mov`, which is `or %g0, src, rd` — except when either side is a state
/// register, where it becomes `rd %y` or `wr %g0, src, %y` instead.
fn mov(cx: &mut AsmCtx<'_>, ops: &[Operand]) -> Option<Vec<Variant>> {
    use super::reg::RegClass::Asr;
    if let Some(asr) = ops[0].reg().filter(|r| r.class == Asr) {
        let rd = encode::int_reg(cx, &ops[1], "destination")?;
        return Some(encode::one(Word::plain(encode::format3(
            encode::OP_ALU,
            u32::from(rd.num),
            0x28,
            u32::from(asr.num),
            0,
        ))));
    }
    if let Some(asr) = ops[1].reg().filter(|r| r.class == Asr) {
        let (low, fixup) = encode::source(cx, &ops[0])?;
        return Some(encode::one(word(
            encode::format3(encode::OP_ALU, u32::from(asr.num), 0x30, 0, low),
            fixup,
        )));
    }
    let rd = encode::int_reg(cx, &ops[1], "destination")?;
    alu(cx, OR, 0, &ops[0], rd.num)
}

/// `set value, rd`: load an arbitrary 32-bit constant.
///
/// The split is the whole point of the synthetic, and it has to match what
/// other assemblers pick, because a linker relaxing or a debugger decoding
/// the pair expects the canonical sequence:
///
/// * a value that fits `simm13` is one `or`;
/// * a value whose low ten bits are zero is one `sethi`;
/// * anything else, and anything symbolic, is `sethi` plus `or`.
fn set(cx: &mut AsmCtx<'_>, ops: &[Operand]) -> Option<Vec<Variant>> {
    let Some(imm) = ops[0].imm() else {
        cx.error(ops[0].span, "`set` takes a constant or a symbol");
        return None;
    };
    // `set` builds the halves itself, so an operator naming one of them has
    // nothing to say here. Both references let the `sethi` and `or` keep
    // their own `%hi()`/`%lo()` relocations and drop whatever the operand
    // asked for, which for a thread-local operator silently assembles an
    // ordinary address; refusing is the only answer that cannot be wrong.
    if imm.part != ImmPart::Whole {
        cx.error(
            imm.span,
            format!(
                "`set` builds the whole value; drop the {}",
                imm.part.spelling()
            ),
        );
        return None;
    }
    let rd = encode::int_reg(cx, &ops[1], "destination")?;

    let Some(v) = cx.constant(imm.expr) else {
        // A symbol's value is not known yet, so the two-instruction form is
        // the only one that can always work.
        return Some(vec![encode::variant(vec![
            Word::fixed(
                sethi(rd, 0),
                Pending {
                    expr: imm.expr,
                    kind: encode::hi22_fixup(),
                    span: imm.span,
                },
            ),
            Word::fixed(
                or_lo(rd, 0),
                Pending {
                    expr: imm.expr,
                    kind: encode::lo10_fixup(),
                    span: imm.span,
                },
            ),
        ])]);
    };

    if !(-(1i64 << 31)..=(1i64 << 32) - 1).contains(&v) {
        cx.error(imm.span, format!("`set` value {v} does not fit in 32 bits"));
        return None;
    }
    let bits = v as u32;
    let signed = bits as i32 as i64;

    if (encode::SIMM13.0..=encode::SIMM13.1).contains(&signed) {
        return Some(encode::one(Word::plain(encode::format3(
            encode::OP_ALU,
            u32::from(rd.num),
            u32::from(OR),
            0,
            encode::I_BIT | (bits & 0x1fff),
        ))));
    }
    let high = Word::plain(sethi(rd, bits >> 10));
    if bits & 0x3ff == 0 {
        return Some(vec![encode::variant(vec![high])]);
    }
    Some(vec![encode::variant(vec![
        high,
        Word::plain(or_lo(rd, bits & 0x3ff)),
    ])])
}

fn sethi(rd: Reg, imm22: u32) -> u32 {
    encode::format2(u32::from(rd.num), encode::OP2_SETHI, imm22)
}

fn or_lo(rd: Reg, lo: u32) -> u32 {
    encode::format3(
        encode::OP_ALU,
        u32::from(rd.num),
        u32::from(OR),
        u32::from(rd.num),
        encode::I_BIT | (lo & 0x3ff),
    )
}
