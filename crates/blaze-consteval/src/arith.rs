//! The integer and float rules of a constant context, on bits: one home for both evaluators
//! (rule 30), the old walk's `Machine` and blaze-body-eval's interpreter over the body (D119).
//! Each function is a pure function of its arguments; `soft::Format` (binary16 and binary128,
//! which the host has not) stays private behind them.

use core::cmp::Ordering;

use blaze_typeck::{BinOp, FloatTy, IntTy, PNode, UintTy};

use crate::error::{Op as Arith, Refusal};
use crate::soft::Format;

/// Bits, signedness of an integer type (`bool` and `char` as unsigned 8 and 32).
pub fn int_info(n: PNode) -> Option<(u32, bool)> {
    Some(match n {
        PNode::Int(i) => (int_bits(i), true),
        PNode::Uint(u) => (uint_bits(u), false),
        PNode::Bool => (8, false),
        PNode::Char => (32, false),
        _ => return None,
    })
}

pub(crate) fn int_bits(i: IntTy) -> u32 {
    match i {
        IntTy::I8 => 8,
        IntTy::I16 => 16,
        IntTy::I32 => 32,
        IntTy::I64 | IntTy::Isize => 64,
        IntTy::I128 => 128,
    }
}

fn uint_bits(u: UintTy) -> u32 {
    match u {
        UintTy::U8 => 8,
        UintTy::U16 => 16,
        UintTy::U32 => 32,
        UintTy::U64 | UintTy::Usize => 64,
        UintTy::U128 => 128,
    }
}

/// `v` cut to its low `bits` bits.
pub fn mask(v: u128, bits: u32) -> u128 {
    if bits >= 128 { v } else { v & ((1u128 << bits) - 1) }
}

/// The low `bits` bits of `v` read as a signed integer of that width.
pub fn sext(v: u128, bits: u32) -> i128 {
    if bits >= 128 {
        v as i128
    } else {
        let s = 128 - bits;
        ((v << s) as i128) >> s
    }
}

pub(crate) fn smin(bits: u32) -> i128 {
    if bits >= 128 { i128::MIN } else { -(1i128 << (bits - 1)) }
}

pub(crate) fn smax(bits: u32) -> i128 {
    if bits >= 128 { i128::MAX } else { (1i128 << (bits - 1)) - 1 }
}

pub(crate) fn umax(bits: u32) -> u128 {
    mask(u128::MAX, bits)
}

fn op_of(op: BinOp) -> Arith {
    match op {
        BinOp::Sub => Arith::Sub,
        BinOp::Mul => Arith::Mul,
        BinOp::Div => Arith::Div,
        BinOp::Rem => Arith::Rem,
        BinOp::Shl => Arith::Shl,
        BinOp::Shr => Arith::Shr,
        _ => Arith::Add,
    }
}

/// Integer arithmetic as a constant context defines it (the Reference, "Arithmetic and Logical
/// Binary Operators" and "Constant evaluation"): overflow, division by zero and a shift past
/// the width are errors, not wraps. `plant` is the tests' control ([`Plant`]); `Plant::None`
/// outside them.
pub fn int_binop<'a>(op: BinOp, a: u128, b: u128, bits: u32, signed: bool, rhs: (u32, bool), plant: Plant) -> Result<u128, Refusal<'a>> {
    let overflow = Refusal::Overflow { op: op_of(op) };
    let fit_s = |r: Option<i128>| -> Result<u128, Refusal<'a>> {
        match r {
            Some(r) if r >= smin(bits) && r <= smax(bits) => Ok(mask(r as u128, bits)),
            Some(r) if plant == Plant::WrapOnOverflow => Ok(mask(r as u128, bits)),
            _ => Err(overflow.clone_kind()),
        }
    };
    let fit_u = |r: Option<u128>| -> Result<u128, Refusal<'a>> {
        match r {
            Some(r) if r <= umax(bits) => Ok(r),
            Some(r) if plant == Plant::WrapOnOverflow => Ok(mask(r, bits)),
            _ => Err(overflow.clone_kind()),
        }
    };
    let (x, y) = (sext(a, bits), sext(b, bits));
    Ok(match op {
        BinOp::Add if signed => fit_s(x.checked_add(y))?,
        BinOp::Add => fit_u(a.checked_add(b))?,
        BinOp::Sub if signed => fit_s(x.checked_sub(y))?,
        BinOp::Sub => fit_u(a.checked_sub(b).or(if plant == Plant::WrapOnOverflow { Some(a.wrapping_sub(b)) } else { None }))?,
        BinOp::Mul if signed => fit_s(x.checked_mul(y))?,
        BinOp::Mul => fit_u(a.checked_mul(b))?,
        BinOp::Div | BinOp::Rem => {
            if b == 0 {
                return Err(if op == BinOp::Div { Refusal::DivByZero } else { Refusal::RemByZero });
            }
            if signed {
                if x == smin(bits) && y == -1 {
                    return Err(overflow);
                }
                let r = if op == BinOp::Div { x / y } else { x % y };
                mask(r as u128, bits)
            } else if op == BinOp::Div {
                a / b
            } else {
                a % b
            }
        }
        BinOp::Shl | BinOp::Shr => {
            let (rbits, rsigned) = rhs;
            let amount: i128 = if rsigned { sext(b, rbits) } else if b > i128::MAX as u128 { i128::MAX } else { b as i128 };
            if amount < 0 || amount >= i128::from(bits) {
                return Err(Refusal::ShiftTooFar { amount, bits });
            }
            let s = amount as u32;
            if op == BinOp::Shl {
                mask(a << s, bits)
            } else if signed {
                mask((x >> s) as u128, bits)
            } else {
                a >> s
            }
        }
        BinOp::BitAnd => a & b,
        BinOp::BitOr => a | b,
        BinOp::BitXor => a ^ b,
        BinOp::Eq => u128::from(a == b),
        BinOp::Ne => u128::from(a != b),
        BinOp::Lt => u128::from(if signed { x < y } else { a < b }),
        BinOp::Le => u128::from(if signed { x <= y } else { a <= b }),
        BinOp::Gt => u128::from(if signed { x > y } else { a > b }),
        BinOp::Ge => u128::from(if signed { x >= y } else { a >= b }),
        BinOp::And | BinOp::Or => return Err(Refusal::Malformed { what: "a short-circuit operator as arithmetic" }),
    })
}

impl Refusal<'_> {
    /// A copy of a refusal with no owned parts (an arithmetic one).
    fn clone_kind<'b>(&self) -> Refusal<'b> {
        match self {
            Refusal::Overflow { op } => Refusal::Overflow { op: *op },
            _ => Refusal::Malformed { what: "a refusal copied that has parts" },
        }
    }
}

/// Planted bugs, for the tests' controls (rule 23); `None` outside tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    None,
    /// Arithmetic overflow wraps instead of refusing.
    WrapOnOverflow,
    /// Pointers of two allocations compare by offset instead of being refused.
    ComparePointers,
    /// A discriminant is taken from the variant before the one asked.
    DiscriminantOffByOne,
    /// The old behaviour: a pending refusal met while compiling is kept as if final, so its
    /// code stays complete (machine/late.rs `Late::makes_incomplete`).
    KeepPendingCode,
}

/// How two floats of type `ty` (as bits) order: IEEE 754's comparison, `None` when either is
/// a NaN (unordered); `-0` equals `+0`.
pub fn float_cmp(ty: FloatTy, a: u128, b: u128) -> Option<Ordering> {
    match ty {
        FloatTy::F64 => f64::from_bits(a as u64).partial_cmp(&f64::from_bits(b as u64)),
        FloatTy::F32 => f32::from_bits(a as u32).partial_cmp(&f32::from_bits(b as u32)),
        // binary16 and binary128, which the host has not: on their bits (soft.rs).
        FloatTy::F16 | FloatTy::F128 => Format::of(ty).compare(a, b),
    }
}

/// Float arithmetic: IEEE 754 with round-to-nearest, as the host's are and as run time's are
/// (the Reference: floats follow IEEE 754-2008); `f32`/`f64` on the host's, `f16`/`f128` on
/// their bits. A comparison gives `0` or `1` ([`float_cmp`]: a NaN compares unequal to all).
pub fn float_binop(op: BinOp, ty: FloatTy, a: u128, b: u128) -> Result<u128, Refusal<'static>> {
    let holds = |want: fn(Option<Ordering>) -> bool| Ok(u128::from(want(float_cmp(ty, a, b))));
    match op {
        BinOp::Eq => return holds(|o| o == Some(Ordering::Equal)),
        BinOp::Ne => return holds(|o| o != Some(Ordering::Equal)),
        BinOp::Lt => return holds(|o| o == Some(Ordering::Less)),
        BinOp::Le => return holds(|o| matches!(o, Some(Ordering::Less | Ordering::Equal))),
        BinOp::Gt => return holds(|o| o == Some(Ordering::Greater)),
        BinOp::Ge => return holds(|o| matches!(o, Some(Ordering::Greater | Ordering::Equal))),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {}
        _ => return Err(Refusal::Malformed { what: "a bit operator on a float" }),
    }
    Ok(match ty {
        FloatTy::F64 => {
            let (x, y) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            let r = match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y,
                _ => x % y,
            };
            u128::from(r.to_bits())
        }
        FloatTy::F32 => {
            let (x, y) = (f32::from_bits(a as u32), f32::from_bits(b as u32));
            let r = match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y,
                _ => x % y,
            };
            u128::from(r.to_bits())
        }
        FloatTy::F16 | FloatTy::F128 => {
            let fm = Format::of(ty);
            match op {
                BinOp::Add => fm.add(a, b),
                BinOp::Sub => fm.sub(a, b),
                BinOp::Mul => fm.mul(a, b),
                BinOp::Div => fm.div(a, b),
                _ => fm.rem(a, b),
            }
        }
    })
}

/// A host `f64` as the bits of an `f32` or `f64`; `f16` and `f128` go through `soft` instead.
fn float_val(f: FloatTy, v: f64) -> Result<u128, Refusal<'static>> {
    Ok(match f {
        FloatTy::F32 => u128::from((v as f32).to_bits()),
        FloatTy::F64 => u128::from(v.to_bits()),
        FloatTy::F16 | FloatTy::F128 => return Err(Refusal::Malformed { what: "an `f16` or `f128` through the host's floats" }),
    })
}

/// The bits of an `f32` or `f64` as a host `f64`.
fn float_in(f: FloatTy, a: u128) -> Result<f64, Refusal<'static>> {
    match f {
        FloatTy::F32 => Ok(f64::from(f32::from_bits(a as u32))),
        FloatTy::F64 => Ok(f64::from_bits(a as u64)),
        FloatTy::F16 | FloatTy::F128 => Err(Refusal::Malformed { what: "an `f16` or `f128` through the host's floats" }),
    }
}

/// `as` between scalar types (the Reference, "Type cast expressions"), on bits: integers
/// (sign-extended or cut), floats (a float to an integer saturates, NaN to 0), `u8` to `char`,
/// and an integer to a raw pointer (its bits). `bits` is the value of type `from`; the result
/// is of type `to`. A pointer, reference or enum source is the caller's (it is no number).
pub fn num_cast(from: PNode, to: PNode, bits: u128) -> Result<u128, Refusal<'static>> {
    let malformed = |what: &'static str| Refusal::Malformed { what };
    let a = bits;
    Ok(match (from, to) {
        // `f16` and `f128` on their bits (soft.rs), the host having neither.
        (PNode::Float(ff), PNode::Float(tf)) if matches!(ff, FloatTy::F16 | FloatTy::F128) || matches!(tf, FloatTy::F16 | FloatTy::F128) => {
            Format::of(ff).convert(Format::of(tf), a)
        }
        (PNode::Float(ff @ (FloatTy::F16 | FloatTy::F128)), _) => {
            let (bits, signed) = int_info(to).ok_or(malformed("a float cast to this type"))?;
            Format::of(ff).to_int(a, bits, signed)
        }
        (_, PNode::Float(tf @ (FloatTy::F16 | FloatTy::F128))) => {
            let (bits, signed) = int_info(from).ok_or(malformed("a cast to a float from this type"))?;
            let (negative, magnitude) = match signed && sext(a, bits) < 0 {
                true => (true, sext(a, bits).unsigned_abs()),
                false => (false, a),
            };
            Format::of(tf).from_int(negative, magnitude)
        }
        (PNode::Float(ff), PNode::Float(tf)) => float_val(tf, float_in(ff, a)?)?,
        (PNode::Float(ff), _) => {
            // Saturating, NaN to 0.
            let (bits, signed) = int_info(to).ok_or(malformed("a float cast to this type"))?;
            let x = float_in(ff, a)?;
            if signed {
                let r: i128 = if bits == 128 { x as i128 } else { (x as i128).clamp(smin(bits), smax(bits)) };
                mask(r as u128, bits)
            } else if bits == 128 {
                x as u128
            } else {
                (x as u128).min(umax(bits))
            }
        }
        (_, PNode::Float(tf)) => {
            let (bits, signed) = int_info(from).ok_or(malformed("a cast to a float from this type"))?;
            match tf {
                FloatTy::F32 => u128::from(if signed { sext(a, bits) as f32 } else { a as f32 }.to_bits()),
                FloatTy::F64 => u128::from(if signed { sext(a, bits) as f64 } else { a as f64 }.to_bits()),
                FloatTy::F16 | FloatTy::F128 => return Err(malformed("an `f16` or `f128` through the host's floats")),
            }
        }
        (_, PNode::Char) => {
            if from != PNode::Uint(UintTy::U8) {
                return Err(malformed("a cast to `char` from other than `u8`"));
            }
            a
        }
        (_, PNode::Ptr(..)) => a,
        _ => {
            let (fbits, fsigned) = int_info(from).ok_or(malformed("a cast from this type"))?;
            let (tbits, _) = int_info(to).ok_or(malformed("a cast to this type"))?;
            let wide = if fsigned { sext(a, fbits) as u128 } else { a };
            mask(wide, tbits)
        }
    })
}
