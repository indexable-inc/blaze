//! The values the known-panics lint computes with: integers, `bool`s, `char`s and floats, with
//! rustc's interpreter's arithmetic and casts of them, and rustc's printing of an integer in a
//! message (`1_i32`, `i32::MAX`, `u8::MAX`).
//!
//! Follows `compiler/rustc_const_eval/src/interpret/operator.rs` (`binary_op`,
//! `binary_int_op`, `binary_bool_op`, `binary_char_op`, `binary_float_op`, `unary_op`),
//! `compiler/rustc_const_eval/src/interpret/cast.rs` (`cast_from_int_like`,
//! `float_to_float_or_int`) and `compiler/rustc_middle/src/ty/consts/int.rs` (`ConstInt`'s
//! `Debug`) (MIT or Apache-2.0).

#[cfg(test)]
mod tests;

use std::cmp::Ordering;

use hir_ty::mir::{BinOp, UnOp};
use rustc_type_ir::FloatTy;

/// What a [`Scalar`] is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// `isize` and `usize` are `pointer_sized`.
    Int { signed: bool, pointer_sized: bool },
    Bool,
    Char,
    Float(FloatTy),
}

/// A value of a primitive type: its `size` bytes, as rustc's `ScalarInt` holds them (the bits
/// above them zero).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Scalar {
    pub(super) bits: u128,
    pub(super) size: u8,
    pub(super) kind: Kind,
}

const _: () = assert!(size_of::<Scalar>() <= 32);

/// What a binary operation gives: a value, or with `AddWithOverflow` and the like, the value
/// with whether it overflowed.
pub(super) enum Output {
    One(Scalar),
    Pair(Scalar, bool),
}

impl Scalar {
    /// `bits` truncated to `size` bytes.
    pub(super) fn new(bits: u128, size: u8, kind: Kind) -> Scalar {
        Scalar { bits: truncate(bits, size), size, kind }
    }

    pub(super) fn from_bool(value: bool) -> Scalar {
        Scalar { bits: u128::from(value), size: 1, kind: Kind::Bool }
    }

    pub(super) fn is_integral(self) -> bool {
        matches!(self.kind, Kind::Int { .. })
    }

    fn signed(self) -> bool {
        matches!(self.kind, Kind::Int { signed: true, .. })
    }

    /// The bits sign-extended from `size` bytes.
    fn int(self) -> i128 {
        sign_extend(self.bits, self.size)
    }

    fn with(self, bits: u128) -> Scalar {
        Scalar::new(bits, self.size, self.kind)
    }
}

fn truncate(bits: u128, size: u8) -> u128 {
    let width = u32::from(size) * 8;
    if width >= 128 { bits } else { bits & ((1 << width) - 1) }
}

fn sign_extend(bits: u128, size: u8) -> i128 {
    let shift = 128 - u32::from(size) * 8;
    ((bits << shift) as i128) >> shift
}

fn signed_min(size: u8) -> i128 {
    sign_extend(1 << (u32::from(size) * 8 - 1), size)
}

fn signed_max(size: u8) -> i128 {
    (truncate(u128::MAX, size) >> 1) as i128
}

/// `InterpCx::binary_op`: `None` where the interpreter gives an error (a division by zero, an
/// operation on operands it does not take).
pub(super) fn binary_op(op: &BinOp, l: Scalar, r: Scalar) -> Option<Output> {
    match l.kind {
        Kind::Char if r.kind == Kind::Char => Some(Output::One(Scalar::from_bool(compare(op, l.bits.cmp(&r.bits))?))),
        Kind::Bool if r.kind == Kind::Bool => bool_op(op, l, r),
        Kind::Float(ty) if r.kind == l.kind => float_op(op, ty, l, r),
        Kind::Int { .. } => int_op(op, l, r),
        _ => None,
    }
}

/// Whether `l op r` overflows, for an operation that says (`AddWithOverflow` and the like).
pub(super) fn overflows(op: &BinOp, l: Scalar, r: Scalar) -> Option<bool> {
    match binary_op(op, l, r)? {
        Output::Pair(_, overflowed) => Some(overflowed),
        Output::One(_) => None,
    }
}

fn compare(op: &BinOp, ordering: Ordering) -> Option<bool> {
    Some(match op {
        BinOp::Eq => ordering.is_eq(),
        BinOp::Ne => ordering.is_ne(),
        BinOp::Lt => ordering.is_lt(),
        BinOp::Le => ordering.is_le(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::Ge => ordering.is_ge(),
        _ => return None,
    })
}

/// `binary_bool_op`.
fn bool_op(op: &BinOp, l: Scalar, r: Scalar) -> Option<Output> {
    let (a, b) = (l.bits != 0, r.bits != 0);
    let value = match op {
        BinOp::BitAnd => a & b,
        BinOp::BitOr => a | b,
        BinOp::BitXor => a ^ b,
        _ => compare(op, a.cmp(&b))?,
    };
    Some(Output::One(Scalar::from_bool(value)))
}

/// `binary_float_op`, of `f32` and `f64`.
fn float_op(op: &BinOp, ty: FloatTy, l: Scalar, r: Scalar) -> Option<Output> {
    macro_rules! op {
        ($float:ty, $bits:ty) => {{
            let a = <$float>::from_bits(l.bits as $bits);
            let b = <$float>::from_bits(r.bits as $bits);
            let value = match op {
                BinOp::Eq => return Some(Output::One(Scalar::from_bool(a == b))),
                BinOp::Ne => return Some(Output::One(Scalar::from_bool(a != b))),
                BinOp::Lt => return Some(Output::One(Scalar::from_bool(a < b))),
                BinOp::Le => return Some(Output::One(Scalar::from_bool(a <= b))),
                BinOp::Gt => return Some(Output::One(Scalar::from_bool(a > b))),
                BinOp::Ge => return Some(Output::One(Scalar::from_bool(a >= b))),
                BinOp::Add => a + b,
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Rem => a % b,
                _ => return None,
            };
            Some(Output::One(l.with(u128::from(value.to_bits()))))
        }};
    }
    match ty {
        FloatTy::F32 => op!(f32, u32),
        FloatTy::F64 => op!(f64, u64),
        FloatTy::F16 | FloatTy::F128 => None,
    }
}

/// `binary_int_op`: a shift by its amount modulo the width of the left side, the rest of the
/// same type on both sides, computed wide and truncated, overflowing when that loses bits.
fn int_op(op: &BinOp, l: Scalar, r: Scalar) -> Option<Output> {
    let Kind::Int { signed, .. } = l.kind else { return None };
    if let BinOp::Shl | BinOp::Shr = op {
        let Kind::Int { signed: r_signed, .. } = r.kind else { return None };
        let width = u32::from(l.size) * 8;
        let amount = match r_signed {
            true => r.int().rem_euclid(i128::from(width)) as u32,
            false => (r.bits % u128::from(width)) as u32,
        };
        let bits = match (op, signed) {
            (BinOp::Shl, true) => (l.int() << amount) as u128,
            (BinOp::Shr, true) => (l.int() >> amount) as u128,
            (BinOp::Shl, false) => l.bits << amount,
            _ => l.bits >> amount,
        };
        return Some(Output::One(l.with(bits)));
    }
    if r.kind != l.kind || r.size != l.size {
        return None;
    }
    let with_overflow = matches!(op, BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow);
    let output = |value: Scalar, overflowed: bool| match with_overflow {
        true => Output::Pair(value, overflowed),
        false => Output::One(value),
    };
    if signed {
        let (a, b) = (l.int(), r.int());
        if let BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge = op {
            return Some(Output::One(Scalar::from_bool(compare(op, a.cmp(&b))?)));
        }
        let wide = match op {
            BinOp::Div | BinOp::Rem if b == 0 => return None,
            BinOp::Div | BinOp::Rem if a == signed_min(l.size) && b == -1 => return None,
            BinOp::Div => Some(a.overflowing_div(b)),
            BinOp::Rem => Some(a.overflowing_rem(b)),
            BinOp::Add | BinOp::AddWithOverflow => Some(a.overflowing_add(b)),
            BinOp::Sub | BinOp::SubWithOverflow => Some(a.overflowing_sub(b)),
            BinOp::Mul | BinOp::MulWithOverflow => Some(a.overflowing_mul(b)),
            _ => None,
        };
        if let Some((result, overflowed)) = wide {
            let value = l.with(result as u128);
            return Some(output(value, overflowed || value.int() != result));
        }
    }
    let (a, b) = (l.bits, r.bits);
    Some(match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            Output::One(Scalar::from_bool(compare(op, a.cmp(&b))?))
        }
        BinOp::BitOr => Output::One(l.with(a | b)),
        BinOp::BitAnd => Output::One(l.with(a & b)),
        BinOp::BitXor => Output::One(l.with(a ^ b)),
        _ => {
            let (result, overflowed) = match op {
                BinOp::Add | BinOp::AddWithOverflow => a.overflowing_add(b),
                BinOp::Sub | BinOp::SubWithOverflow => a.overflowing_sub(b),
                BinOp::Mul | BinOp::MulWithOverflow => a.overflowing_mul(b),
                BinOp::Div | BinOp::Rem if b == 0 => return None,
                BinOp::Div => a.overflowing_div(b),
                BinOp::Rem => a.overflowing_rem(b),
                _ => return None,
            };
            let value = l.with(result);
            output(value, overflowed || value.bits != result)
        }
    })
}

/// `InterpCx::unary_op`.
pub(super) fn unary_op(op: UnOp, value: Scalar) -> Option<Scalar> {
    match (value.kind, op) {
        (Kind::Bool, UnOp::Not) => Some(Scalar::from_bool(value.bits == 0)),
        (Kind::Float(_), UnOp::Neg) => Some(value.with(value.bits ^ (1 << (u32::from(value.size) * 8 - 1)))),
        (Kind::Int { signed: true, .. }, UnOp::Neg) => Some(value.with(value.int().wrapping_neg() as u128)),
        (Kind::Int { .. }, UnOp::Not) => Some(value.with(!value.bits)),
        _ => None,
    }
}

/// `cast_from_int_like`: an integer, `bool` or `char` as an integer, a float or (from `u8`) a
/// `char` of `size` bytes.
pub(super) fn int_cast(value: Scalar, kind: Kind, size: u8) -> Option<Scalar> {
    if let Kind::Float(_) = value.kind {
        return None;
    }
    let signed = value.signed();
    let bits = if signed { value.int() as u128 } else { value.bits };
    let float = |bits: u128| -> Option<u128> {
        Some(match kind {
            Kind::Float(FloatTy::F32) if signed => u128::from((bits as i128 as f32).to_bits()),
            Kind::Float(FloatTy::F32) => u128::from((bits as f32).to_bits()),
            Kind::Float(FloatTy::F64) if signed => u128::from((bits as i128 as f64).to_bits()),
            Kind::Float(FloatTy::F64) => u128::from((bits as f64).to_bits()),
            _ => return None,
        })
    };
    match kind {
        Kind::Int { .. } => Some(Scalar::new(bits, size, kind)),
        Kind::Char => Some(Scalar::new(u128::from(u8::try_from(bits).ok()?), size, kind)),
        Kind::Float(_) => Some(Scalar::new(float(bits)?, size, kind)),
        Kind::Bool => None,
    }
}

/// `float_to_float_or_int`: an `f32` or `f64` as a float, or as an integer saturating (NaN is
/// 0), as `to_i128_r` and `to_u128_r` round.
pub(super) fn float_cast(value: Scalar, kind: Kind, size: u8) -> Option<Scalar> {
    let float = match value.kind {
        Kind::Float(FloatTy::F32) => f64::from(f32::from_bits(value.bits as u32)),
        Kind::Float(FloatTy::F64) => f64::from_bits(value.bits as u64),
        _ => return None,
    };
    let bits = match kind {
        Kind::Int { signed: true, .. } => (float as i128).clamp(signed_min(size), signed_max(size)) as u128,
        Kind::Int { signed: false, .. } => (float as u128).min(truncate(u128::MAX, size)),
        Kind::Float(FloatTy::F32) => u128::from((float as f32).to_bits()),
        Kind::Float(FloatTy::F64) => u128::from(float.to_bits()),
        _ => return None,
    };
    Some(Scalar::new(bits, size, kind))
}

/// rustc's `ConstInt` of `value` as `Debug` prints it: `i32::MIN`, `i32::MAX`, `u8::MAX`, else
/// the number, with its type after it (`1_i32`) when `alternate` (`{:#?}`).
pub(super) fn const_int(value: Scalar, alternate: bool) -> String {
    let (signed, pointer_sized) = match value.kind {
        Kind::Int { signed, pointer_sized } => (signed, pointer_sized),
        _ => (false, false),
    };
    let width = u32::from(value.size) * 8;
    let name = |prefix: char| match pointer_sized {
        // alloc: a lint's message, for a panic found
        true => format!("{prefix}size"),
        // alloc: a lint's message, for a panic found
        false => format!("{prefix}{width}"),
    };
    let (prefix, number) = if signed {
        let min = 1u128 << (width - 1);
        if value.bits == min {
            // alloc: a lint's message, for a panic found
            return format!("{}::MIN", name('i'));
        }
        if value.bits == min - 1 {
            // alloc: a lint's message, for a panic found
            return format!("{}::MAX", name('i'));
        }
        // alloc: a lint's message, for a panic found
        ('i', value.int().to_string())
    } else {
        if value.bits == truncate(u128::MAX, value.size) {
            // alloc: a lint's message, for a panic found
            return format!("{}::MAX", name('u'));
        }
        // alloc: a lint's message, for a panic found
        ('u', value.bits.to_string())
    };
    match alternate {
        // alloc: a lint's message, for a panic found
        true => format!("{number}_{}", name(prefix)),
        false => number,
    }
}
