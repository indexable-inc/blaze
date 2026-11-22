//! Floats in the reference evaluator: `f32` and `f64` values, their arithmetic, comparisons and
//! conversions, by IEEE 754 through the host's own `f32` and `f64` operations, each of which
//! Rust defines exactly (correctly rounded, or exact) on every host. A float is held as its bits
//! in `Val::Int`, so it is stored, loaded, passed and returned as any scalar is. `f16` and `f128`
//! constants, loads, stores and bitcasts are modeled here; their two-operand arithmetic, `fcmp`
//! and conversions between float types by soft IEEE (eval/float.rs); their other operations are
//! refused by name.
//!
//! The one place the evaluator and machine code may differ is a NaN's payload and sign: IEEE
//! 754 and Rust leave which NaN an operation gives open (only that it is a NaN), and the host's
//! instruction picks one. So a harness compares results by [`agree`], which takes any two NaNs
//! of a float type as equal; a program that looks at a NaN's bits (a `bitcast`, or a store then
//! an integer load) is where the two may still differ, and a harness must not judge by those
//! bits. Two operations whose result LIR leaves open besides are refused by name rather than
//! answered: `fmin_num`/`fmax_num` of +0 and -0, and an `fmuladd` whose fused and unfused results
//! differ.

use core::{
    alloc::Allocator,
    ops::{Add, Div, Mul, Rem, Sub},
};

use super::{
    EvalError, Run, Val,
    float::{float_bin, float_cmp, float_convert, placed},
};
use crate::{
    FloatCond, Op, Scalar, Ty, Value,
    arith::{mask, sext},
};

/// What the evaluator asks of a host float type.
trait Float:
    Copy + PartialOrd + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> + Div<Output = Self> + Rem<Output = Self>
{
    const BITS: u32;
    fn of(bits: u128) -> Self;
    fn bits(self) -> u128;
    fn nan(self) -> bool;
    fn root(self) -> Self;
    fn down(self) -> Self;
    fn up(self) -> Self;
    fn toward_zero(self) -> Self;
    fn ties_even(self) -> Self;
    fn ties_away(self) -> Self;
    fn fused(self, b: Self, c: Self) -> Self;
    fn of_u128(value: u128) -> Self;
    fn of_i128(value: i128) -> Self;
    /// Rust's `as`: saturating, NaN to 0.
    fn to_u128(self) -> u128;
    fn to_i128(self) -> i128;
}

macro_rules! float {
    ($t:ty, $bits:ty) => {
        impl Float for $t {
            const BITS: u32 = <$bits>::BITS;
            fn of(bits: u128) -> $t {
                <$t>::from_bits(bits as $bits)
            }
            fn bits(self) -> u128 {
                u128::from(<$t>::to_bits(self))
            }
            fn nan(self) -> bool {
                <$t>::is_nan(self)
            }
            fn root(self) -> $t {
                <$t>::sqrt(self)
            }
            fn down(self) -> $t {
                <$t>::floor(self)
            }
            fn up(self) -> $t {
                <$t>::ceil(self)
            }
            fn toward_zero(self) -> $t {
                <$t>::trunc(self)
            }
            fn ties_even(self) -> $t {
                <$t>::round_ties_even(self)
            }
            fn ties_away(self) -> $t {
                <$t>::round(self)
            }
            fn fused(self, b: $t, c: $t) -> $t {
                <$t>::mul_add(self, b, c)
            }
            fn of_u128(value: u128) -> $t {
                value as $t
            }
            fn of_i128(value: i128) -> $t {
                value as $t
            }
            fn to_u128(self) -> u128 {
                self as u128
            }
            fn to_i128(self) -> i128 {
                self as i128
            }
        }
    };
}

float!(f32, u32);
float!(f64, u64);

const NARROW: &str = "an f16 or f128 operation other than two-operand arithmetic, `fcmp` or a float conversion";

/// A two-operand float operation.
fn binary<F: Float>(op: Op, a: F, b: F) -> Result<F, &'static str> {
    let sign = 1u128 << (F::BITS - 1);
    Ok(match op {
        Op::Fadd => a + b,
        Op::Fsub => a - b,
        Op::Fmul => a * b,
        Op::Fdiv => a / b,
        // Rust's `%` on floats is C's `fmod`, as LLVM's `frem` is.
        Op::Frem => a % b,
        // "IEEE 754-2008 `minNum`: a NaN operand gives the other (`f32::min`)."
        Op::FminNum | Op::FmaxNum => {
            if a.nan() {
                return Ok(b);
            }
            if b.nan() {
                return Ok(a);
            }
            if a == b && a.bits() != b.bits() {
                return Err("an `fmin_num` or `fmax_num` of +0 and -0, whose result LIR leaves open");
            }
            if (a < b) == (op == Op::FminNum) { a } else { b }
        }
        // "IEEE 754-2019 `minimum`: a NaN operand gives NaN"; -0 is below +0.
        Op::Fminimum | Op::Fmaximum => {
            if a.nan() {
                return Ok(a);
            }
            if b.nan() {
                return Ok(b);
            }
            let min = op == Op::Fminimum;
            if a == b {
                let negative = a.bits() & sign != 0;
                return Ok(if negative == min { a } else { b });
            }
            if (a < b) == min { a } else { b }
        }
        Op::Fcopysign => F::of((a.bits() & !sign) | (b.bits() & sign)),
        _ => return Err("this float operation"),
    })
}

/// A one-operand float operation: the sign operations on the bits, keeping a NaN's payload.
fn unary<F: Float>(op: Op, a: F) -> Result<F, &'static str> {
    let sign = 1u128 << (F::BITS - 1);
    Ok(match op {
        Op::Fneg => F::of(a.bits() ^ sign),
        Op::Fabs => F::of(a.bits() & !sign),
        Op::Sqrt => a.root(),
        Op::Floor => a.down(),
        Op::Ceil => a.up(),
        Op::Ftrunc => a.toward_zero(),
        Op::Nearest => a.ties_even(),
        Op::Fround => a.ties_away(),
        _ => return Err("this float operation"),
    })
}

/// `fma`, rounded once; `fmuladd`, fused or not, answered only where both agree.
fn ternary<F: Float>(op: Op, a: F, b: F, c: F) -> Result<F, &'static str> {
    let fused = a.fused(b, c);
    match op {
        Op::Fma => Ok(fused),
        Op::Fmuladd => {
            let apart = a * b + c;
            match fused.bits() == apart.bits() || (fused.nan() && apart.nan()) {
                true => Ok(fused),
                false => Err("an `fmuladd` whose fused and unfused results differ: LIR leaves which open"),
            }
        }
        _ => Err("this float operation"),
    }
}

/// `fcmp`: an ordered condition is false and an unordered one true when either is NaN.
fn fcmp<F: Float>(cond: FloatCond, a: F, b: F) -> bool {
    let uno = a.nan() || b.nan();
    match cond {
        FloatCond::False => false,
        FloatCond::Oeq => !uno && a == b,
        FloatCond::Ogt => !uno && a > b,
        FloatCond::Oge => !uno && a >= b,
        FloatCond::Olt => !uno && a < b,
        FloatCond::Ole => !uno && a <= b,
        FloatCond::One => !uno && a != b,
        FloatCond::Ord => !uno,
        FloatCond::Uno => uno,
        FloatCond::Ueq => uno || a == b,
        FloatCond::Ugt => uno || a > b,
        FloatCond::Uge => uno || a >= b,
        FloatCond::Ult => uno || a < b,
        FloatCond::Ule => uno || a <= b,
        FloatCond::Une => uno || a != b,
        FloatCond::True => true,
    }
}

/// `fptoui`/`fptosi` to `width` bits: "Saturates (Rust's `as`) ... NaN is 0."
fn to_int<F: Float>(op: Op, a: F, width: u32) -> u128 {
    match op {
        Op::FptoUi => a.to_u128().min(mask(width)),
        _ => {
            let max = (mask(width) >> 1) as i128;
            a.to_i128().clamp(-max - 1, max) as u128 & mask(width)
        }
    }
}

/// `uitofp`/`sitofp` of `a`, `from` bits wide: rounded to nearest, ties to even (Rust's `as`).
fn of_int<F: Float>(op: Op, a: u128, from: u32) -> u128 {
    match op {
        Op::UitoFp => F::of_u128(a).bits(),
        _ => F::of_i128(sext(a, from)).bits(),
    }
}

/// Whether `other`'s value of type `ty` agrees with the evaluator's `reference`: equal, or
/// both NaNs of a scalar float type whatever their payloads and signs, or anything where the
/// evaluator holds `uninit` (whose bits LIR leaves open).
pub fn agree(ty: Ty, ptr_bits: u32, reference: Val, other: Val) -> bool {
    if reference == other || reference == Val::Uninit {
        return true;
    }
    let (Val::Int(a), Val::Int(b)) = (reference, other) else { return false };
    let exponent = match (ty.is_scalar(), ty.elem()) {
        (true, Scalar::F16) => 5,
        (true, Scalar::F32) => 8,
        (true, Scalar::F64) => 11,
        (true, Scalar::F128) => 15,
        _ => return false,
    };
    let fraction = ty.bits(ptr_bits) - 1 - exponent;
    let nan = |x: u128| (x >> fraction) & mask(exponent) == mask(exponent) && x & mask(fraction) != 0;
    nan(a) && nan(b)
}

impl<A: Allocator + Clone> Run<'_, A> {
    /// A scalar float operand: its bits and its scalar.
    fn floats(&self, inst: usize, op: Op, value: Value) -> Result<(u128, Scalar), EvalError> {
        let ty = self.view.ty(value);
        if !ty.is_scalar() || !ty.is_float() {
            return Err(EvalError::Unsupported { inst, op, what: "an operand that is not a scalar float" });
        }
        match self.get(inst, value)? {
            Val::Int(bits) => Ok((bits, ty.elem())),
            _ => Err(EvalError::Unsupported { inst, op, what: "a float operand that holds no float" }),
        }
    }

    /// Float instruction `inst`: its value.
    pub(super) fn float(&self, inst: usize, op: Op) -> Result<Val, EvalError> {
        let refuse = |what| EvalError::Unsupported { inst, op, what };
        let mut o = self.view.operands(inst);
        let ty = self.view.ty(Value(inst as u32));
        let ptr_bits = self.view.head.ptr_bits;
        if !ty.is_scalar() {
            return Err(refuse("a vector float operation"));
        }
        let bits = match op {
            Op::Fconst => u128::from(o.imm()) & mask(ty.bits(ptr_bits)),
            Op::FconstWide => {
                let words = o.constant();
                words.iter().rev().fold(0u128, |it, &word| (it << 32) | u128::from(word)) & mask(ty.bits(ptr_bits))
            }
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Frem | Op::FminNum | Op::FmaxNum
            | Op::Fminimum | Op::Fmaximum | Op::Fcopysign => {
                let ((a, scalar), (b, _)) = (self.floats(inst, op, o.value())?, self.floats(inst, op, o.value())?);
                match scalar {
                    Scalar::F32 => binary(op, f32::of(a), f32::of(b)).map(Float::bits).map_err(refuse),
                    Scalar::F64 => binary(op, f64::of(a), f64::of(b)).map(Float::bits).map_err(refuse),
                    // f16 and f128 by soft IEEE (eval/float.rs).
                    _ => float_bin(op, ty, a, b).map_err(|error| placed(error, inst)),
                }?
            }
            Op::Fneg | Op::Fabs | Op::Sqrt | Op::Floor | Op::Ceil | Op::Ftrunc | Op::Nearest | Op::Fround => {
                let (a, scalar) = self.floats(inst, op, o.value())?;
                match scalar {
                    Scalar::F32 => unary(op, f32::of(a)).map(Float::bits),
                    Scalar::F64 => unary(op, f64::of(a)).map(Float::bits),
                    _ => Err(NARROW),
                }
                .map_err(refuse)?
            }
            Op::Fma | Op::Fmuladd => {
                let (a, scalar) = self.floats(inst, op, o.value())?;
                let ((b, _), (c, _)) = (self.floats(inst, op, o.value())?, self.floats(inst, op, o.value())?);
                match scalar {
                    Scalar::F32 => ternary(op, f32::of(a), f32::of(b), f32::of(c)).map(Float::bits),
                    Scalar::F64 => ternary(op, f64::of(a), f64::of(b), f64::of(c)).map(Float::bits),
                    _ => Err(NARROW),
                }
                .map_err(refuse)?
            }
            Op::Fcmp => {
                let cond = o.float_cond();
                let ((a, scalar), (b, _)) = (self.floats(inst, op, o.value())?, self.floats(inst, op, o.value())?);
                let holds = match scalar {
                    Scalar::F32 => fcmp(cond, f32::of(a), f32::of(b)),
                    Scalar::F64 => fcmp(cond, f64::of(a), f64::of(b)),
                    _ => float_cmp(cond, Ty::scalar(scalar), a, b).map_err(|error| placed(error, inst))?,
                };
                u128::from(holds)
            }
            Op::Fpext | Op::Fptrunc => {
                let (a, from) = self.floats(inst, op, o.value())?;
                match (op, from, ty.elem()) {
                    (Op::Fpext, Scalar::F32, Scalar::F64) => (f32::of(a) as f64).bits(),
                    (Op::Fptrunc, Scalar::F64, Scalar::F32) => (f64::of(a) as f32).bits(),
                    // Any pair with f16 or f128 (the verifier admits only a widening `fpext` and a
                    // narrowing `fptrunc`) by soft IEEE.
                    _ => float_convert(op, Ty::scalar(from), ty, a).map_err(|error| placed(error, inst))?,
                }
            }
            Op::FptoUi | Op::FptoSi => {
                let width = self.width(inst)?;
                let (a, from) = self.floats(inst, op, o.value())?;
                match from {
                    Scalar::F32 => to_int(op, f32::of(a), width),
                    Scalar::F64 => to_int(op, f64::of(a), width),
                    _ => return Err(refuse(NARROW)),
                }
            }
            Op::UitoFp | Op::SitoFp => {
                let (a, from) = self.int(inst, o.value())?;
                match ty.elem() {
                    Scalar::F32 => of_int::<f32>(op, a, from),
                    Scalar::F64 => of_int::<f64>(op, a, from),
                    _ => return Err(refuse(NARROW)),
                }
            }
            // "The same bits as another type of the same width: integers, floats and vectors,
            // never a pointer."
            Op::Bitcast => {
                let from = o.value();
                if !self.view.ty(from).is_scalar() || self.view.ty(from).is_ptr() || ty.is_ptr() {
                    return Err(refuse("a bitcast of a vector or a pointer"));
                }
                match self.get(inst, from)? {
                    Val::Int(bits) => bits,
                    _ => return Err(refuse("a bitcast of a value that holds no bits")),
                }
            }
            _ => return Err(refuse("this operation")),
        };
        Ok(Val::Int(bits))
    }
}
