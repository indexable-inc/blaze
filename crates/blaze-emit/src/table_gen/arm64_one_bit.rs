//! The arm64 recipes T1 left out that the tier equality harness found missing (tier_tests.rs,
//! "not compared, fast: .. x the table lacks .."): shifts and divisions at width 1, and `freeze`
//! at every width the Fast tier holds in a register.
//!
//! A child of arm64.rs (its `#[path]` line there), so it shares that file's helpers; its
//! [`recipes`] is called from arm64.rs's, so the generator, the encoder-agreement test and the
//! planted control in tests.rs walk these recipes too. After this file is wired the committed
//! table (crates/blaze-emit/table/arm64.bztab and arm64.json) must be made again by the
//! generator example (`examples/gen_table.rs`), or the Fast tier still reads the old table.
//!
//! # LIR's semantics at width 1 (crates/blaze-lir/src/arith.rs, `binary`; the evaluator)
//!
//! - Shifts: "`let amount = (b % u128::from(width)) as u32`", so at width 1 the amount is always
//!   0, and `shl`, `lshr` and `ashr` give `a` (`(a << 0) & m`, `a >> 0`, `(sext(a) >> 0) & m`).
//!   The Reference tier does the same: pass.rs, "The amount is taken modulo the width, a power
//!   of two; at 1 bit it is 0", where it only copies `a`. The template is `mov d, a`; the amount
//!   operand is in the pattern (the composer looks it up by its operands' forms) but no field
//!   reads it.
//! - `udiv`, `urem`: "`if b == 0 => return Err(Trap::DivByZero)`", else `a / b` and `a % b` on
//!   the zero-extended operands (so `a / 1 = a`, `a % 1 = 0`): the 8 to 64-bit recipes'
//!   sequence, which reads the zero-extended registers, is right at width 1 as it is.
//! - `sdiv`: the operands sign-extended (`sext(1, 1) = -1`); "`if sa == smin(width) && sb == -1
//!   => Err(Trap::DivOverflow)`" with `smin(1) = -1`, so `1 sdiv 1` traps (it is `-1 / -1 = 1`,
//!   which does not fit), `0 sdiv 1 = 0`. `srem`: "`sa.wrapping_rem(sb) as u128 & m`", always 0
//!   at width 1. The narrower recipes' sequence of arm64.rs (sign-extend both, `sdiv`, trap when
//!   the quotient does not fit the width, zero-extend) is exactly that at width 1: the Reference
//!   tier widens the same way (pass.rs, `widen_signed`, its `min` `-(1 << (width - 1))`).
//!
//! # `freeze`
//!
//! LIR's evaluator: "`Op::Freeze => match self.get(inst, o.value())? { Val::Uninit => .. , it =>
//! it }`": a defined value is its own `freeze`; the Reference tier copies its cell (pass.rs,
//! `Op::Zext | Op::Freeze | ..`). A register already holds some bits of an `uninit` too, so the
//! template is a move: `mov d, a` (integers and pointers, the value zero-extended to 64 bits
//! already) and `fmov d, d` (`f32` and `f64`, held in a `d` register).

use blaze_lir::Op;

use super::glue_recipes::fmov_d;
use super::{Each, FPR, GPR, NONE, WIDTHS, one, pattern, r, reg};
use crate::enc;
use crate::pass::{BRK_DIV_OVERFLOW, BRK_DIV_ZERO};
use crate::table_gen::{GenError, RES, T0, T1};

/// Every recipe of this file, handed to `each` in a fixed order (the table sorts them).
pub(super) fn recipes(each: &mut Each<'_>) -> Result<(), GenError> {
    shifts_at_one_bit(each)?;
    divisions_at_one_bit(each)?;
    freezes(each)
}

/// `shl`, `lshr`, `ashr` of 1 bit by a register: the amount is 0 (LIR's `b % 1`), so `a`.
fn shifts_at_one_bit(each: &mut Each<'_>) -> Result<(), GenError> {
    for op in [Op::Shl, Op::Lshr, Op::Ashr] {
        let p = pattern(op, 1, 0, GPR, [GPR, GPR, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| c.word(enc::mov(r(v, RES), r(v, 0))))?;
    }
    Ok(())
}

/// `udiv`, `urem`, `sdiv`, `srem` of 1 bit, each trapping as LIR says (module doc).
fn divisions_at_one_bit(each: &mut Each<'_>) -> Result<(), GenError> {
    let slots = [reg(0), reg(1), reg(RES), reg(T0), reg(T1)];
    one(each, pattern(Op::Udiv, 1, 0, GPR, [GPR, GPR, NONE, NONE]), &slots[..3], &|v, c| {
        let (a, b, d) = (r(v, 0), r(v, 1), r(v, RES));
        c.word(enc::cbnz(b, 2));
        c.word(enc::brk(BRK_DIV_ZERO));
        c.word(enc::udiv(d, a, b));
    })?;
    one(each, pattern(Op::Urem, 1, 0, GPR, [GPR, GPR, NONE, NONE]), &slots[..4], &|v, c| {
        let (a, b, d, t) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0));
        c.word(enc::cbnz(b, 2));
        c.word(enc::brk(BRK_DIV_ZERO));
        c.word(enc::udiv(t, a, b));
        c.word(enc::msub(d, t, b, a));
    })?;
    one(each, pattern(Op::Sdiv, 1, 0, GPR, [GPR, GPR, NONE, NONE]), &slots, &|v, c| {
        let (a, b, d, t0, t1) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0), r(v, T1));
        c.word(enc::cbnz(b, 2));
        c.word(enc::brk(BRK_DIV_ZERO));
        // The quotient of the sign-extended operands overflows exactly when it does not fit one
        // bit: `-1 / -1 = 1`.
        c.word(enc::sext(t0, a, 1));
        c.word(enc::sext(t1, b, 1));
        c.word(enc::sdiv(t0, t0, t1));
        c.word(enc::sext(t1, t0, 1));
        c.word(enc::cmp(t0, t1));
        c.word(enc::b_cond(enc::Cond::Eq, 2));
        c.word(enc::brk(BRK_DIV_OVERFLOW));
        c.word(enc::zext(d, t0, 1));
    })?;
    one(each, pattern(Op::Srem, 1, 0, GPR, [GPR, GPR, NONE, NONE]), &slots, &|v, c| {
        let (a, b, d, t0, t1) = (r(v, 0), r(v, 1), r(v, RES), r(v, T0), r(v, T1));
        c.word(enc::cbnz(b, 2));
        c.word(enc::brk(BRK_DIV_ZERO));
        c.word(enc::sext(t0, a, 1));
        c.word(enc::sext(t1, b, 1));
        c.word(enc::sdiv(d, t0, t1));
        c.word(enc::msub(d, d, t1, t0));
        c.word(enc::zext(d, d, 1));
    })
}

/// `freeze`: a move, at every integer width and at `f32` and `f64`.
fn freezes(each: &mut Each<'_>) -> Result<(), GenError> {
    for w in WIDTHS {
        let p = pattern(Op::Freeze, w, 0, GPR, [GPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| c.word(enc::mov(r(v, RES), r(v, 0))))?;
    }
    for w in [32u8, 64] {
        let p = pattern(Op::Freeze, w, 0, FPR, [FPR, NONE, NONE, NONE]);
        one(each, p, &[reg(0), reg(RES)], &|v, c| c.word(fmov_d(r(v, RES), r(v, 0))))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The table serves every pattern of this file, and each template, filled, is the word the
    //! Arm A64 rule gives (written beside each, field by field, independent of enc.rs).

    use blaze_lir::Op;
    use blaze_own::{AVec, Vec};
    use blaze_tier::{Arch, Form, Machine, Os, Pattern, RegClass, Table};

    use crate::pass::{BRK_DIV_OVERFLOW, BRK_DIV_ZERO};
    use crate::table_gen::{EncodedTable, OPERANDS, RES, Vals, fill, generate};

    const MACHINE: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: 0 };
    const N: Form = Form::None;
    const G: Form = Form::Reg(RegClass::Gpr);
    const F: Form = Form::Reg(RegClass::Fpr);

    fn p(op: Op, width: u8, result: Form, operands: [Form; 4]) -> Pattern {
        Pattern { op, width, variant: 0, result, operands, inner: None }
    }

    fn table_bytes() -> AVec<'static, u8> {
        let alloc = blaze_own::testing::heap().alloc();
        let mut out = Vec::new_in(alloc);
        if let Err(error) = generate(MACHINE, &mut out, alloc) {
            panic!("the arm64 table was not generated: {error}");
        }
        out
    }

    fn with(set: &[(u8, u64)]) -> Vals {
        let mut vals = [0u64; OPERANDS];
        for &(at, value) in set {
            vals[usize::from(at)] = value;
        }
        vals
    }

    fn filled(table: &EncodedTable<'_>, pattern: &Pattern, vals: &Vals) -> std::vec::Vec<u32> {
        let Some(template) = table.find(pattern) else {
            panic!("the table lacks {pattern:?}");
        };
        let mut code = template.bytes.to_vec();
        fill(&mut code, template.fields, vals);
        code.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
    }

    /// ORR (shifted register), 64-bit, `rn = xzr`: `mov xd, xm`. `sf 31 = 1, opc 30:29 = 01,
    /// 01010 28:24, shift 23:22 = 00, N 21 = 0, Rm 20:16, imm6 15:10 = 0, Rn 9:5 = 31, Rd 4:0`.
    fn mov_rule(d: u32, m: u32) -> u32 {
        (1 << 31) | (0b01 << 29) | (0b01010 << 24) | (m << 16) | (31 << 5) | d
    }

    /// FMOV (register), double: `00011110 31:24, ftype 23:22 = 01, 1 21, 0000 20:17, opc 16:15
    /// = 00, 10000 14:10, Rn 9:5, Rd 4:0`.
    fn fmov_rule(d: u32, n: u32) -> u32 {
        (0b0001_1110 << 24) | (0b01 << 22) | (1 << 21) | (0b10000 << 10) | (n << 5) | d
    }

    /// CBNZ, 64-bit: `sf 31 = 1, 011010 30:25, op 24 = 1, imm19 23:5 (words), Rt 4:0`.
    fn cbnz_rule(t: u32, words: u32) -> u32 {
        (1 << 31) | (0b011010 << 25) | (1 << 24) | (words << 5) | t
    }

    /// BRK: `11010100 001 31:21, imm16 20:5, 00000 4:0`.
    fn brk_rule(imm16: u32) -> u32 {
        (0b1101_0100_001 << 21) | (imm16 << 5)
    }

    /// UDIV, 64-bit: `sf 31 = 1, 0 30, 0 29, 11010110 28:21, Rm 20:16, 00001 15:11, o1 10 = 0,
    /// Rn 9:5, Rd 4:0`.
    fn udiv_rule(d: u32, n: u32, m: u32) -> u32 {
        (1 << 31) | (0b1101_0110 << 21) | (m << 16) | (0b00001 << 11) | (n << 5) | d
    }

    /// Every pattern this file adds is in the generated table: the four divisions and three
    /// shifts at width 1, `freeze` at 1, 8, 16, 32 and 64 bits in a general register and at 32
    /// and 64 in a float register.
    /// Expected: prints "one-bit and freeze: 14 patterns in the table" and passes. Fails with the
    /// first pattern missing if a recipe is dropped or this file is not called from arm64.rs.
    #[test]
    fn every_one_bit_and_freeze_pattern_is_in_the_table() {
        let bytes = table_bytes();
        let table = match EncodedTable::read(&bytes, blaze_own::testing::heap().alloc()) {
            Ok(table) => table,
            Err(error) => panic!("the generated table does not read back: {error}"),
        };
        let mut wanted: std::vec::Vec<Pattern> = std::vec::Vec::new();
        for op in [Op::Shl, Op::Lshr, Op::Ashr, Op::Udiv, Op::Urem, Op::Sdiv, Op::Srem] {
            wanted.push(p(op, 1, G, [G, G, N, N]));
        }
        for w in [1u8, 8, 16, 32, 64] {
            wanted.push(p(Op::Freeze, w, G, [G, N, N, N]));
        }
        for w in [32u8, 64] {
            wanted.push(p(Op::Freeze, w, F, [F, N, N, N]));
        }
        if let Some(missing) = wanted.iter().find(|it| table.find(it).is_none()) {
            panic!("the table lacks {missing:?}");
        }
        std::println!("one-bit and freeze: {} patterns in the table", wanted.len());
    }

    /// The templates, filled, are the A64 rule's words: a 1-bit shift and an integer `freeze`
    /// are `mov x0, x1` (0xaa0103e0, the Arm manual's `mov x0, x1`); a float `freeze` is
    /// `fmov d0, d1` (0x1e604020); a 1-bit `udiv` of x1 by x2 into x0 is `cbnz x2, #8; brk
    /// #0xff00; udiv x0, x1, x2`. The 1-bit `sdiv` ends in `ubfx x0, t0, #0, #1` after its
    /// overflow `brk #0xff01`: its words are checked for the two `brk`s at their places.
    /// Expected: passes. Fails if a template's fixed bits or a field are wrong.
    #[test]
    fn one_bit_and_freeze_templates_are_the_rules_words() {
        let bytes = table_bytes();
        let table = match EncodedTable::read(&bytes, blaze_own::testing::heap().alloc()) {
            Ok(table) => table,
            Err(error) => panic!("the generated table does not read back: {error}"),
        };
        let vals = with(&[(0, 1), (1, 2), (RES, 0)]);
        assert_eq!(mov_rule(0, 1), 0xaa01_03e0, "the rule's own `mov x0, x1`");
        assert_eq!(fmov_rule(0, 1), 0x1e60_4020, "the rule's own `fmov d0, d1`");
        for op in [Op::Shl, Op::Lshr, Op::Ashr] {
            let got = filled(&table, &p(op, 1, G, [G, G, N, N]), &vals);
            assert_eq!(&got[..], &[mov_rule(0, 1)], "`{}` of 1 bit: {got:08x?}", op.name());
        }
        for w in [1u8, 8, 16, 32, 64] {
            let got = filled(&table, &p(Op::Freeze, w, G, [G, N, N, N]), &vals);
            assert_eq!(&got[..], &[mov_rule(0, 1)], "`freeze` of {w} bits: {got:08x?}");
        }
        for w in [32u8, 64] {
            let got = filled(&table, &p(Op::Freeze, w, F, [F, N, N, N]), &vals);
            assert_eq!(&got[..], &[fmov_rule(0, 1)], "float `freeze` of {w} bits: {got:08x?}");
        }
        let got = filled(&table, &p(Op::Udiv, 1, G, [G, G, N, N]), &vals);
        let want = [cbnz_rule(2, 2), brk_rule(BRK_DIV_ZERO), udiv_rule(0, 1, 2)];
        assert_eq!(&got[..], &want[..], "`udiv` of 1 bit: {got:08x?}");
        let got = filled(&table, &p(Op::Sdiv, 1, G, [G, G, N, N]), &vals);
        assert_eq!(got.len(), 10, "`sdiv` of 1 bit: ten words: {got:08x?}");
        assert_eq!((got[0], got[1]), (cbnz_rule(2, 2), brk_rule(BRK_DIV_ZERO)), "`sdiv` of 1 bit traps on zero first");
        assert_eq!(got[8], brk_rule(BRK_DIV_OVERFLOW), "`sdiv` of 1 bit traps on overflow before its result");
    }
}
