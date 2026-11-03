//! The VFP literal loads, `vldr sN, =expr` and `vldr dN, =expr`.
//!
//! GNU as's `move_or_literal_pool` gives a `vldr` of an `=expr` the same
//! treatment it gives `ldr`, with two differences. A `d` register asks for
//! an eight-byte pool entry, which has to be a number and which the pool
//! holds as two four-byte slots (see [`crate::literals`]). And before the
//! pool is asked for anything, a number that a move instruction can hold is
//! moved instead: a 64-bit value with a NEON modified-immediate pattern
//! becomes `vmov.i64`, and one that is a single-precision "quarter float"
//! becomes the VFP `vmov.f32` or `vmov.f64` of that immediate — `fconsts`
//! and `fconstd` under their old names, which is how GNU as writes them.
//!
//! "A number" is whatever GNU as's expression parser has folded to one by
//! the time `parse_big_immediate` reads it, so a difference of labels a
//! fixed distance apart counts: `vldr d0, =l1-l0` with an `l0` behind it is
//! the distance, and one with an alignment in between, or with an `l1` still
//! to come, is the syntax error GNU as reports. [`crate::arch::fold_differences`]
//! is the same fold, so the same lines assemble here.
//!
//! The load itself is a coprocessor load from the PC, reaching 1020 bytes
//! either way in steps of four, which is a quarter of what `ldr` reaches.
//! `vldr sN, label` is that same load with the offset naming the label
//! instead of a pool entry, so [`super::generic`] writes it through the two
//! scatter functions here; the two spellings never meet, since only an
//! `=expr` is a literal operand.

use super::generic::{cmode_for_move, invert_size};
use super::insn::AL;
use super::operand::{OperandKind, VecKind};
use super::{Insn, THUMB_BITS};
use crate::arch::{AsmCtx, Literal};
use crate::section::{Fixup, FixupKind, Variant};

/// Whether this `vldr` loads a literal pool value, which [`literal_load`]
/// encodes rather than the table-driven encoder.
pub(super) fn is_literal_load(ins: &Insn<'_>) -> bool {
    ins.ops.len() == 2 && matches!(ins.ops[1].kind, OperandKind::Literal(_))
}

/// Whether `imm` is a single-precision number of the form
/// `0baBbbbbbc defgh000 00000000 00000000`, the eight-bit VFP immediate:
/// `is_quarter_float`.
fn quarter_float(imm: u32) -> bool {
    let bs = if imm & 0x2000_0000 != 0 {
        0x3e00_0000
    } else {
        0x4000_0000
    };
    imm & 0x7_ffff == 0 && (imm & 0x7e00_0000) ^ bs == 0
}

/// That number as the eight bits the encoding holds: `neon_qfloat_bits`.
fn qfloat_bits(imm: u32) -> u32 {
    ((imm >> 19) & 0x7f) | ((imm >> 24) & 0x80)
}

/// Whether a double-precision number is a single-precision one widened,
/// which is what lets `vmov.f64` hold it: `is_double_a_single`.
fn double_is_single(v: u64) -> bool {
    let exp = (v >> 52) & 0x7ff;
    let mantissa = v & 0xf_ffff_ffff_ffff;
    (exp == 0 || exp == 0x7ff || (1023 - 126..=1023 + 127).contains(&exp))
        && mantissa & 0x1fff_ffff == 0
}

/// That number narrowed to single precision, dropping the bits the wider
/// exponent and mantissa held: `double_to_single`.
fn double_to_single(v: u64) -> u32 {
    let sign = (v >> 63) & 1;
    let mut exp = ((v >> 52) & 0x7ff) as i64;
    let mut mantissa = v & 0xf_ffff_ffff_ffff;
    if exp == 0x7ff {
        exp = 0xff;
    } else {
        exp = exp - 1023 + 127;
        if exp >= 0xff {
            // Infinity.
            exp = 0x7f;
            mantissa = 0;
        } else if exp < 0 {
            // No denormalized numbers.
            exp = 0;
            mantissa = 0;
        }
    }
    ((sign as u32) << 31) | ((exp as u32) << 23) | ((mantissa >> 29) as u32)
}

/// A word as the two little-endian halfwords a 32-bit Thumb instruction is,
/// or as the four bytes an A32 one is.
fn bytes(thumb: bool, word: u32) -> Vec<u8> {
    if !thumb {
        return word.to_le_bytes().to_vec();
    }
    let mut v = ((word >> 16) as u16).to_le_bytes().to_vec();
    v.extend_from_slice(&(word as u16).to_le_bytes());
    v
}

/// Where a VFP register's number goes: the low four bits in one field and
/// the fifth bit in another, and which is which depends on the width.
/// `encode_arm_vfp_reg`.
fn vfp_reg(n: u8, single: bool) -> u32 {
    let (low, high) = if single {
        (u32::from(n) >> 1, u32::from(n) & 1)
    } else {
        (u32::from(n) & 0xf, u32::from(n) >> 4)
    };
    (low << 12) | (high << 22)
}

/// The 12-bit offset field of a coprocessor load from the PC, in A32: eight
/// bits counting words, and the U bit at 23 that an offset of zero leaves
/// alone -- GNU as assembles the load with U set and `md_apply_fix` only
/// clears it for an offset it writes.
pub(super) fn scatter_arm(w: u64, v: i64) -> u64 {
    if v == 0 {
        return w & !0xff;
    }
    let up = if v > 0 { 0x0080_0000 } else { 0 };
    (w & !0x0080_00ff) | up | (v.unsigned_abs() / 4)
}

/// The same field in T32, where the halfword holding the U bit comes first
/// in memory and so lies in the low half of the word the fixup sees.
pub(super) fn scatter_thumb(w: u64, v: i64) -> u64 {
    if v == 0 {
        return w & !0x00ff_0000;
    }
    let up = if v > 0 { 0x80 } else { 0 };
    (w & !0x00ff_0080) | up | ((v.unsigned_abs() / 4) << 16)
}

/// `vldr sN, =expr` and `vldr dN, =expr`.
pub(super) fn literal_load(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let thumb = cx.state.bits == THUMB_BITS;
    let op = ins.ops[1].clone();
    let OperandKind::Literal(e) = op.kind else {
        return None;
    };
    // GNU as's expression parser folds a difference of labels as it reads
    // the line, so `parse_big_immediate` sees a number where the two are a
    // fixed distance apart and an eight-byte entry is allowed; `e` itself
    // stays the expression the source wrote, which is what decides whether
    // the entry is shared.
    let folded = crate::arch::fold_differences(cx, e);
    if ins.set_flags {
        cx.error(ins.span, format!("`{}` cannot set the flags", ins.text));
        return None;
    }
    if thumb && ins.cond_written && ins.cond != AL {
        cx.error(
            ins.span,
            format!(
                "`{}` is conditional, which in Thumb takes an `it` block",
                ins.text
            ),
        );
        return None;
    }
    let cond = if thumb { AL } else { ins.cond };
    let Some(v) = ins.ops[0].vec() else {
        let what = ins.ops[0].describe();
        cx.error(
            ins.ops[0].span,
            format!("expected a vector register, found {what}"),
        );
        return None;
    };
    let single = match v.kind {
        VecKind::S => true,
        VecKind::D => false,
        VecKind::Q => {
            cx.error(
                ins.ops[0].span,
                "a literal pool value loads into an `s` or a `d` register, not a `q` one",
            );
            return None;
        }
    };
    if v.lane.is_some() || v.all {
        cx.error(ins.ops[0].span, "this operand takes a whole register");
        return None;
    }
    if v.n > 31 {
        cx.error(
            ins.ops[0].span,
            format!("`{}{}` is not a register", v.kind.letter(), v.n),
        );
        return None;
    }
    let vd = vfp_reg(v.n, single);
    // A number GNU as can move instead is moved. `vmov.i64` comes first,
    // and only for a `d` register; the VFP immediate after it, for either
    // width.
    if let Some(value) = cx.constant(folded) {
        if !single {
            let mut lo = value as u32;
            let mut hi = (value as u64 >> 32) as u32;
            let mut neon_op = 0;
            let found = cmode_for_move(lo, hi, &mut neon_op, 64).or_else(|| {
                invert_size(&mut lo, &mut hi, 64);
                neon_op ^= 1;
                cmode_for_move(lo, hi, &mut neon_op, 64)
            });
            if let Some((cmode, immbits)) = found {
                // The NEON move is unconditional whatever the `vldr` said,
                // as `move_or_literal_pool` builds it: it keeps only the
                // register bits of the load.
                let base = if thumb { 0xef80_0000 } else { 0xf280_0000 };
                let top = if thumb { 28 } else { 24 };
                let word = base
                    | (vd & 0x0040_f000)
                    | (cmode << 8)
                    | (neon_op << 5)
                    | (1 << 4)
                    | (immbits & 0xf)
                    | (((immbits >> 4) & 7) << 16)
                    | (((immbits >> 7) & 1) << top);
                return Some(vec![Variant::new(bytes(thumb, word))]);
            }
        }
        let quarter = if single {
            quarter_float(value as u32).then(|| qfloat_bits(value as u32))
        } else {
            let wide = value as u64;
            (double_is_single(wide) && quarter_float(double_to_single(wide)))
                .then(|| qfloat_bits(double_to_single(wide)))
        };
        if let Some(imm) = quarter {
            let base = if single { 0x0eb0_0a00 } else { 0x0eb0_0b00 };
            let word = (u32::from(cond) << 28) | base | vd | ((imm & 0xf0) << 12) | (imm & 0xf);
            return Some(vec![Variant::new(bytes(thumb, word))]);
        }
    }
    // Otherwise it is a pool entry: four bytes for a single, eight for a
    // double, which `add_to_lit_pool` takes only as a number.
    let value = if single {
        let constant = super::encode::literal_constant(cx, &op, folded)?;
        match cx.constant(folded) {
            Some(v) if constant.is_some() => Literal::Const(v),
            _ => Literal::Expr(e),
        }
    } else {
        match cx.constant(folded) {
            Some(v) => Literal::Const(v),
            None => {
                cx.error(op.span, "invalid type for literal pool");
                return None;
            }
        }
    };
    let size = if single { 4 } else { 8 };
    let entry = cx.literal_from(value, size, op.span, super::encode::literal_unsigned(cx, e));
    let hint = "the literal pool is too far away; put an `.ltorg` nearer";
    let kind = if thumb {
        FixupKind::pcrel(4, 4)
            .with_pc_align(4)
            .scatter(scatter_thumb)
    } else {
        FixupKind::pcrel(4, 8).scatter(scatter_arm)
    }
    .with_field(0, 4)
    .with_limits(-1020, 1020)
    .with_range_hint(hint);
    let base = if single { 0x0d9f_0a00 } else { 0x0d9f_0b00 };
    let word = (u32::from(cond) << 28) | base | vd;
    Some(vec![Variant {
        bytes: bytes(thumb, word),
        fixups: vec![Fixup {
            offset: 0,
            expr: entry,
            kind,
            span: op.span,
        }],
    }])
}
