//! LIR's integer arithmetic: what every integer operation without flags gives, by the crate's
//! "Semantics" (a shift masks its amount, division by zero and `sdiv` of `MIN` by -1 trap, a
//! conversion truncates or extends). The one home of it: the reference evaluator
//! ([`crate::eval`], in tests) runs these, and the simplify stage's folding
//! ([`crate::simplify`], in a build) calls the same functions, so a folded constant is by
//! construction what the evaluator gives for every operation and width.
//!
//! Moved out of `eval.rs` unchanged (rule 30: one concept, one home): the evaluator is built
//! only in tests and under its feature, and folding runs in every build.
//!
//! Values are `u128`s holding a `width`-bit integer, the bits above the width zero.

use crate::{IntCond, Op};

/// Why a run stopped by a trap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trap {
    /// `trap` with its code.
    Code(u32),
    /// A division or remainder by zero.
    DivByZero,
    /// `sdiv` of the type's minimum by -1.
    DivOverflow,
}

pub(crate) const fn mask(width: u32) -> u128 {
    match width {
        128.. => u128::MAX,
        width => (1u128 << width) - 1,
    }
}

pub(crate) const fn sext(value: u128, width: u32) -> i128 {
    let shift = 128 - width;
    ((value << shift) as i128) >> shift
}

const fn smin(width: u32) -> i128 {
    match width {
        128.. => i128::MIN,
        width => -(1i128 << (width - 1)),
    }
}

const fn smax(width: u32) -> i128 {
    match width {
        128.. => i128::MAX,
        width => (1i128 << (width - 1)) - 1,
    }
}

/// The 256-bit product of `a` and `b`: its high and low halves.
const fn wide_mul(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & (u64::MAX as u128));
    let (b1, b0) = (b >> 64, b & (u64::MAX as u128));
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let mid = (p00 >> 64) + (p01 & (u64::MAX as u128)) + (p10 & (u64::MAX as u128));
    let low = (p00 & (u64::MAX as u128)) | (mid << 64);
    let high = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (high, low)
}

/// The high half of the `2 * width`-bit product.
fn mulhi(a: u128, b: u128, width: u32, signed: bool) -> u128 {
    if width < 128 {
        let product = match signed {
            true => (sext(a, width) * sext(b, width) >> width) as u128,
            false => (a * b) >> width,
        };
        return product & mask(width);
    }
    let (high, _) = wide_mul(a, b);
    match signed {
        // The signed high half from the unsigned: less `b` where `a` is negative, and `a`
        // where `b` is.
        true => {
            let high = if (a as i128) < 0 { high.wrapping_sub(b) } else { high };
            if (b as i128) < 0 { high.wrapping_sub(a) } else { high }
        }
        false => high,
    }
}

/// A two-operand integer operation of `width` bits; `Err` for a trap.
pub(crate) fn binary(op: Op, a: u128, b: u128, width: u32) -> Result<u128, Trap> {
    let m = mask(width);
    let (sa, sb) = (sext(a, width), sext(b, width));
    let clamp = |value: i128| value.clamp(smin(width), smax(width)) as u128 & m;
    let amount = (b % u128::from(width)) as u32;
    Ok(match op {
        Op::Add => a.wrapping_add(b) & m,
        Op::Sub => a.wrapping_sub(b) & m,
        Op::Mul => a.wrapping_mul(b) & m,
        Op::Udiv | Op::Urem | Op::Sdiv | Op::Srem if b == 0 => return Err(Trap::DivByZero),
        Op::Udiv => a / b,
        Op::Urem => a % b,
        Op::Sdiv if sa == smin(width) && sb == -1 => return Err(Trap::DivOverflow),
        Op::Sdiv => sa.wrapping_div(sb) as u128 & m,
        // `MIN % -1` is 0, as Cranelift's `srem` gives.
        Op::Srem => sa.wrapping_rem(sb) as u128 & m,
        Op::And => a & b,
        Op::Or => a | b,
        Op::Xor => a ^ b,
        Op::Shl => (a << amount) & m,
        Op::Lshr => a >> amount,
        Op::Ashr => (sa >> amount) as u128 & m,
        Op::Rotl | Op::Rotr if amount == 0 => a,
        Op::Rotl => ((a << amount) | (a >> (width - amount))) & m,
        Op::Rotr => ((a >> amount) | (a << (width - amount))) & m,
        Op::Umulhi => mulhi(a, b, width, false),
        Op::Smulhi => mulhi(a, b, width, true),
        Op::Umin => a.min(b),
        Op::Umax => a.max(b),
        Op::Smin => sa.min(sb) as u128 & m,
        Op::Smax => sa.max(sb) as u128 & m,
        Op::UaddSat => match a.checked_add(b) {
            Some(sum) if sum <= m => sum,
            _ => m,
        },
        Op::UsubSat => a.saturating_sub(b),
        Op::SaddSat if width == 128 => sa.saturating_add(sb) as u128,
        Op::SsubSat if width == 128 => sa.saturating_sub(sb) as u128,
        Op::SaddSat => clamp(sa + sb),
        Op::SsubSat => clamp(sa - sb),
        _ => unreachable!("`binary` is called for the operations above"),
    })
}

/// An overflow operation's value and whether it overflowed.
pub(crate) fn overflow(op: Op, a: u128, b: u128, width: u32) -> (u128, bool) {
    let m = mask(width);
    let (sa, sb) = (sext(a, width), sext(b, width));
    let signed = |exact: Option<i128>| match exact {
        Some(it) => it < smin(width) || it > smax(width),
        None => true,
    };
    match op {
        Op::UaddOverflow => {
            let (sum, carry) = a.overflowing_add(b);
            (sum & m, carry || sum > m)
        }
        Op::UsubOverflow => (a.wrapping_sub(b) & m, a < b),
        Op::UmulOverflow => match a.checked_mul(b) {
            Some(product) => (product & m, product > m),
            None => (a.wrapping_mul(b) & m, true),
        },
        Op::SaddOverflow => (a.wrapping_add(b) & m, signed(sa.checked_add(sb))),
        Op::SsubOverflow => (a.wrapping_sub(b) & m, signed(sa.checked_sub(sb))),
        Op::SmulOverflow => (a.wrapping_mul(b) & m, signed(sa.checked_mul(sb))),
        _ => unreachable!("`overflow` is called for the overflow operations"),
    }
}

pub(crate) fn unary(op: Op, a: u128, width: u32) -> u128 {
    let m = mask(width);
    let spare = 128 - width;
    match op {
        Op::Neg => a.wrapping_neg() & m,
        Op::Not => !a & m,
        Op::Clz => u128::from(a.leading_zeros() - spare),
        Op::Ctz if a == 0 => u128::from(width),
        Op::Ctz => u128::from(a.trailing_zeros()),
        Op::Popcnt => u128::from(a.count_ones()),
        Op::Bswap if width < 16 => a,
        Op::Bswap => a.swap_bytes() >> spare,
        Op::Bitrev => a.reverse_bits() >> spare,
        _ => unreachable!("`unary` is called for the operations above"),
    }
}

pub(crate) fn compare(cond: IntCond, a: u128, b: u128, width: u32) -> bool {
    let (sa, sb) = (sext(a, width), sext(b, width));
    match cond {
        IntCond::Eq => a == b,
        IntCond::Ne => a != b,
        IntCond::Ult => a < b,
        IntCond::Ule => a <= b,
        IntCond::Ugt => a > b,
        IntCond::Uge => a >= b,
        IntCond::Slt => sa < sb,
        IntCond::Sle => sa <= sb,
        IntCond::Sgt => sa > sb,
        IntCond::Sge => sa >= sb,
    }
}

/// `zext`, `sext` or `trunc` of `a`, `from` bits wide, to `to` bits.
pub(crate) fn convert(op: Op, a: u128, from: u32, to: u32) -> u128 {
    match op {
        Op::Sext => sext(a, from) as u128 & mask(to),
        Op::Zext | Op::Trunc => a & mask(to),
        _ => unreachable!("`convert` is called for `zext`, `sext` and `trunc`"),
    }
}

/// The value of `iconst` with the word `word`, of a `width`-bit type: of at most 32 bits, its
/// bits; wider, the word sign-extended (`Op::Iconst`'s rule).
pub(crate) fn iconst_value(word: u32, width: u32) -> u128 {
    let value = match width {
        ..=32 => u128::from(word),
        _ => (word as i32 as i128) as u128,
    };
    value & mask(width)
}

/// The value of `iconst_wide` with the little-endian words `words`, of a `width`-bit type.
pub(crate) fn wide_value(words: &[u32], width: u32) -> u128 {
    let value = words.iter().rev().fold(0u128, |it, &word| (it << 32) | u128::from(word));
    value & mask(width)
}
