//! Integers of any width (`#![blaze::arbitrary_ints]`) in the evaluator: `u3`, `i7` and every
//! other width that is not 8, 16, 32, 64 or 128 bits.
//!
//! A `uN` is held in its storage integer zero-extended from N bits and an `iN` sign-extended
//! (docs/v2/plan/design/arbitrary-ints.md). Every operation here reads its operands in
//! that form as `i128` (no arbitrary width is over 127 bits, so every value fits), computes,
//! and writes the result back in that form. The standard widths never come here, so their
//! evaluation is unchanged. Codegen (`hir-ty-codegen`) computes the same results and panics
//! with the same words.

use rustc_type_ir::{
    IntTy, UintTy,
    inherent::{IntoKind as _, Ty as _},
};

use super::{
    BinOp, Evaluator, GenericArgs, Interval, IntervalAndTy, IntervalOrOwned, IsSigned, Locals,
    MirEvalError, OwnedBytes, Result, Ty, TyKind, machine::Machine, not_supported, pad16,
};
use crate::intrinsic::Intrinsic;

/// An arbitrary-width integer type: its width, its sign and its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Narrow {
    /// The value width, 1..=127.
    bits: u32,
    signed: bool,
    /// `u3`, for the panics.
    name: &'static str,
}

/// How `l op r` of two N-bit values ended.
pub(super) enum Arith {
    /// The result wrapped to N bits, and whether it overflowed them.
    Value(i128, bool),
    /// `/` or `%` by zero.
    ByZero,
    /// `op` is not an arithmetic or bitwise operator.
    NotArithmetic,
}

impl Narrow {
    /// `ty`, if it is an arbitrary-width integer.
    pub(super) fn of(ty: Ty<'_>) -> Option<Narrow> {
        match ty.kind() {
            TyKind::Int(int @ IntTy::Bits(width)) => {
                Some(Narrow { bits: width.bits(), signed: true, name: int.name_str() })
            }
            TyKind::Uint(uint @ UintTy::Bits(width)) => {
                Some(Narrow { bits: width.bits(), signed: false, name: uint.name_str() })
            }
            _ => None,
        }
    }

    pub(super) fn bits(self) -> u32 {
        self.bits
    }

    pub(super) fn min(self) -> i128 {
        if self.signed { -(1i128 << (self.bits - 1)) } else { 0 }
    }

    pub(super) fn max(self) -> i128 {
        if self.signed { (1i128 << (self.bits - 1)) - 1 } else { (1i128 << self.bits) - 1 }
    }

    fn mask(self) -> u128 {
        (1u128 << self.bits) - 1
    }

    /// `raw` wrapped to N bits and extended back, as the type holds it: what `raw as uN` is.
    pub(super) fn normalize(self, raw: i128) -> i128 {
        let shift = 128 - self.bits;
        if self.signed {
            (raw << shift) >> shift
        } else {
            (((raw as u128) << shift) >> shift) as i128
        }
    }

    /// The N-bit pattern of `value`, below 2^N.
    fn pattern(self, value: i128) -> u128 {
        (value as u128) & self.mask()
    }

    /// The value a storage integer of this type holds. A storage integer with bits above N
    /// that are not N's extension is not a value of the type, as 2 is not a `bool`.
    pub(super) fn read<'db>(self, bytes: &[u8]) -> Result<'db, i128> {
        let value = i128::from_le_bytes(pad16(bytes, self.signed.into()));
        if self.normalize(value) != value {
            return Err(MirEvalError::UndefinedBehavior(
                // alloc: error path
                format!(
                    "{value:#x} is not a value of `{}`: its bits above {} are set",
                    self.name, self.bits
                ),
            ));
        }
        Ok(value)
    }

    /// `value` in a storage integer of `size` bytes.
    pub(super) fn bytes(value: i128, size: usize) -> OwnedBytes {
        OwnedBytes::from_slice(&value.to_le_bytes()[..size])
    }

    /// The result of an operation as this type holds it, from `exact`, the result as if the
    /// type had no width (`None` past `i128`), and `wrapping`, the same wrapped at 128 bits,
    /// which agrees with it mod 2^N. It overflowed when the two differ.
    fn finish(self, exact: Option<i128>, wrapping: i128) -> (i128, bool) {
        let wrapped = self.normalize(wrapping);
        (wrapped, exact != Some(wrapped))
    }

    /// `l + r`, wrapped, and whether it overflowed.
    pub(super) fn add(self, l: i128, r: i128) -> (i128, bool) {
        self.finish(l.checked_add(r), l.wrapping_add(r))
    }

    /// `l - r`, wrapped, and whether it overflowed.
    pub(super) fn sub(self, l: i128, r: i128) -> (i128, bool) {
        self.finish(l.checked_sub(r), l.wrapping_sub(r))
    }

    /// `l * r`, wrapped, and whether it overflowed.
    pub(super) fn mul(self, l: i128, r: i128) -> (i128, bool) {
        self.finish(l.checked_mul(r), l.wrapping_mul(r))
    }

    /// `l op r` for the arithmetic and bitwise operators. `iN::MIN / -1` and `iN::MIN % -1`
    /// overflow, as they do at every width.
    pub(super) fn arith(self, op: &BinOp, l: i128, r: i128) -> Arith {
        let (value, overflowed) = match op {
            BinOp::Add | BinOp::AddWithOverflow => self.add(l, r),
            BinOp::Sub | BinOp::SubWithOverflow => self.sub(l, r),
            BinOp::Mul | BinOp::MulWithOverflow => self.mul(l, r),
            BinOp::Div | BinOp::Rem if r == 0 => return Arith::ByZero,
            BinOp::Div if self.signed && l == self.min() && r == -1 => (self.min(), true),
            BinOp::Rem if self.signed && l == self.min() && r == -1 => (0, true),
            BinOp::Div => (l / r, false),
            BinOp::Rem => (l % r, false),
            BinOp::BitAnd => (l & r, false),
            BinOp::BitOr => (l | r, false),
            BinOp::BitXor => (l ^ r, false),
            _ => return Arith::NotArithmetic,
        };
        Arith::Value(value, overflowed)
    }

    /// `-value`, wrapped, and whether it overflowed (only `-iN::MIN` does).
    pub(super) fn neg(self, value: i128) -> (i128, bool) {
        self.finish(value.checked_neg(), value.wrapping_neg())
    }

    /// `!value`.
    pub(super) fn not(self, value: i128) -> i128 {
        self.normalize(!value)
    }

    /// `l << amount` or `l >> amount`, for an `amount` below N.
    pub(super) fn shift(self, op: &BinOp, l: i128, amount: u32) -> i128 {
        match op {
            BinOp::Shl => self.normalize(((l as u128) << amount) as i128),
            // An extended value shifts right as its type does: arithmetically for `iN`, and a
            // `uN` has no bits above N to bring in.
            _ => l >> amount,
        }
    }

    /// A float `as` this type: rounded toward zero and saturated at N bits, NaN as 0.
    pub(super) fn saturate(self, value: f64) -> i128 {
        if value.is_nan() { 0 } else { (value as i128).clamp(self.min(), self.max()) }
    }

    /// The panic of an overflowing `op`, naming the type:
    /// ``attempt to add with overflow at type `u3` ``.
    pub(super) fn overflow(self, op: &BinOp) -> String {
        let what = match op {
            BinOp::Add | BinOp::AddWithOverflow => "add",
            BinOp::Sub | BinOp::SubWithOverflow => "subtract",
            BinOp::Mul | BinOp::MulWithOverflow => "multiply",
            BinOp::Div => "divide",
            BinOp::Rem => "calculate the remainder",
            BinOp::Shl => "shift left",
            BinOp::Shr => "shift right",
            // alloc: error path
            _ => {
                return format!(
                    "attempt to compute `{op:?}` with overflow at type `{}`",
                    self.name
                );
            }
        };
        format!("attempt to {what} with overflow at type `{}`", self.name) // alloc: error path
    }

    /// The panic of `-iN::MIN`.
    pub(super) fn neg_overflow(self) -> String {
        format!("attempt to negate with overflow at type `{}`", self.name) // alloc: error path
    }

    /// The panic of `/ 0` or `% 0`.
    pub(super) fn by_zero(self, op: &BinOp) -> String {
        match op {
            // alloc: error path
            BinOp::Rem => format!(
                "attempt to calculate the remainder with a divisor of zero at type `{}`",
                self.name
            ),
            _ => format!("attempt to divide by zero at type `{}`", self.name), // alloc: error path
        }
    }
}

/// The amount of a shift: the operand as an unsigned number of its own type's value width, so
/// `-1i3` is 7 and `-1i32` is `u32::MAX`. A shift of an N-bit value is by that mod N when
/// unchecked, and overflows when it is N or more.
pub(super) fn shift_amount(bytes: &[u8], ty: Ty<'_>) -> u128 {
    let raw = u128::from_le_bytes(pad16(bytes, IsSigned::No));
    let width = Narrow::of(ty).map_or(bytes.len() as u32 * 8, |it| it.bits);
    if width >= 128 { raw } else { raw & ((1u128 << width) - 1) }
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// `l op r` for an arbitrary-width `l`, from the operands' storage bytes. `checked` is a
    /// `CheckedBinaryOp`, which panics on overflow when the body checks overflow; in runtime MIR
    /// the checks are `Assert`s before a `BinaryOp`, which wraps. A comparison is not here: the
    /// invariant makes the storage integers compare as the values do.
    pub(super) fn narrow_binary_op(
        &self,
        n: Narrow,
        op: &BinOp,
        checked: bool,
        (lhs, lhs_ty): (&[u8], Ty<'db>),
        (rhs, rhs_ty): (&[u8], Ty<'db>),
        locals: &Locals<'db>,
    ) -> Result<'db, OwnedBytes> {
        let size = lhs.len();
        let l = n.read(lhs)?;
        let panics = checked && self.checks_overflow(locals);
        Ok(match op {
            BinOp::Shl | BinOp::Shr => {
                let amount = shift_amount(rhs, rhs_ty);
                let amount = match amount < u128::from(n.bits) {
                    true => amount as u32,
                    false if panics => return Err(MirEvalError::Panic(n.overflow(op))),
                    // An unchecked shift is by its amount mod N, as rustc's is mod BITS.
                    false => (amount % u128::from(n.bits)) as u32,
                };
                Narrow::bytes(n.shift(op, l, amount), size)
            }
            BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow => {
                let r = n.read(rhs)?;
                let (value, overflowed) = match op {
                    BinOp::AddWithOverflow => n.add(l, r),
                    BinOp::SubWithOverflow => n.sub(l, r),
                    _ => n.mul(l, r),
                };
                // `(T, bool)`: the value, the flag right after it, and padding to `T`'s
                // alignment.
                let align = self.layout(lhs_ty)?.align.abi.bytes() as usize;
                let mut bytes = Narrow::bytes(value, size);
                bytes.push(u8::from(overflowed));
                bytes.resize(bytes.len().next_multiple_of(align), 0);
                bytes
            }
            op => match n.arith(op, l, n.read(rhs)?) {
                // Division overflows and divides by zero under both settings, as at every width.
                Arith::Value(_, true) if panics || matches!(op, BinOp::Div | BinOp::Rem) => {
                    return Err(MirEvalError::Panic(n.overflow(op)));
                }
                Arith::Value(value, _) => Narrow::bytes(value, size),
                Arith::ByZero => return Err(MirEvalError::Panic(n.by_zero(op))),
                Arith::NotArithmetic => {
                    not_supported!("`{op:?}` on the arbitrary-width integer `{}`", n.name)
                }
            },
        })
    }

    /// Runs an integer intrinsic on an arbitrary-width integer, or says `intrinsic` is not one
    /// or its operand is a standard integer (`Ok(false)`), for the standard path. The integer
    /// intrinsics that are right at the storage width under the invariant (comparisons, `|` of
    /// disjoint bits, transmutes) are left to it too.
    pub(super) fn narrow_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
    ) -> Result<'db, bool> {
        let Some(first) = args.first() else { return Ok(false) };
        let Some(n) = Narrow::of(first.ty) else { return Ok(false) };
        let size = first.interval.size;
        let operand = |this: &Self, at: usize| -> Result<'db, i128> {
            match args.get(at) {
                Some(arg) => n.read(arg.get(this)?),
                None => Err(MirEvalError::InternalError(
                    // alloc: error path
                    format!("`{intrinsic}` on `{}` has no operand {at}", n.name).into(),
                )),
            }
        };
        let ub = |what: String| Err(MirEvalError::UndefinedBehavior(what));
        let result = match intrinsic {
            Intrinsic::WrappingAdd | Intrinsic::WrappingSub | Intrinsic::WrappingMul => {
                let (l, r) = (operand(self, 0)?, operand(self, 1)?);
                let (value, _) = match intrinsic {
                    Intrinsic::WrappingAdd => n.add(l, r),
                    Intrinsic::WrappingSub => n.sub(l, r),
                    _ => n.mul(l, r),
                };
                Narrow::bytes(value, size)
            }
            Intrinsic::SaturatingAdd | Intrinsic::SaturatingSub => {
                let (l, r) = (operand(self, 0)?, operand(self, 1)?);
                let add = intrinsic == Intrinsic::SaturatingAdd;
                // Two values of at most 127 bits leave `i128` only for `uN + uN` past `u127`'s
                // half, which saturates at the top.
                let exact = if add { l.checked_add(r) } else { l.checked_sub(r) };
                let value = exact.map_or(n.max(), |it| it.clamp(n.min(), n.max()));
                Narrow::bytes(value, size)
            }
            Intrinsic::AddWithOverflow
            | Intrinsic::SubWithOverflow
            | Intrinsic::MulWithOverflow => {
                let (l, r) = (operand(self, 0)?, operand(self, 1)?);
                let (value, overflowed) = match intrinsic {
                    Intrinsic::AddWithOverflow => n.add(l, r),
                    Intrinsic::SubWithOverflow => n.sub(l, r),
                    _ => n.mul(l, r),
                };
                let result_ty = Ty::new_tup_from_iter(
                    self.interner(),
                    [first.ty, Ty::new_bool(self.interner())].into_iter(),
                );
                let layout = self.layout(result_ty)?;
                let result = self.construct_with_layout(
                    layout.size.bytes_usize(),
                    &layout,
                    None,
                    [Narrow::bytes(value, size), OwnedBytes::from_slice(&[u8::from(overflowed)])]
                        .into_iter()
                        .map(IntervalOrOwned::Owned),
                )?;
                destination.write_from_bytes(self, &result)?;
                return Ok(true);
            }
            Intrinsic::UncheckedAdd
            | Intrinsic::UncheckedSub
            | Intrinsic::UncheckedMul
            | Intrinsic::UncheckedDiv
            | Intrinsic::UncheckedRem
            | Intrinsic::ExactDiv => {
                let (l, r) = (operand(self, 0)?, operand(self, 1)?);
                let op = match intrinsic {
                    Intrinsic::UncheckedAdd => BinOp::Add,
                    Intrinsic::UncheckedSub => BinOp::Sub,
                    Intrinsic::UncheckedMul => BinOp::Mul,
                    Intrinsic::UncheckedRem => BinOp::Rem,
                    _ => BinOp::Div,
                };
                match n.arith(&op, l, r) {
                    Arith::Value(_, true) => {
                        // alloc: error path
                        return ub(format!(
                            "arithmetic overflow in `{intrinsic}` at type `{}`",
                            n.name
                        ));
                    }
                    Arith::Value(_, false) if intrinsic == Intrinsic::ExactDiv && l % r != 0 => {
                        // alloc: error path
                        return ub(format!(
                            "exact_div: {l} cannot be divided by {r} without remainder at type `{}`",
                            n.name
                        ));
                    }
                    Arith::Value(value, false) => Narrow::bytes(value, size),
                    Arith::ByZero | Arith::NotArithmetic => {
                        return ub(n.by_zero(&op));
                    }
                }
            }
            Intrinsic::UncheckedShl | Intrinsic::UncheckedShr => {
                let amount = match args.get(1) {
                    Some(arg) => {
                        let bytes = arg.get(self)?;
                        let signed = matches!(arg.ty.kind(), TyKind::Int(_));
                        let raw = i128::from_le_bytes(pad16(bytes, signed.into()));
                        u32::try_from(raw).ok().filter(|&it| it < n.bits)
                    }
                    None => None,
                };
                let Some(amount) = amount else {
                    // alloc: error path
                    return ub(format!("overflowing shift in `{intrinsic}` at type `{}`", n.name));
                };
                let op = if intrinsic == Intrinsic::UncheckedShl { BinOp::Shl } else { BinOp::Shr };
                Narrow::bytes(n.shift(&op, operand(self, 0)?, amount), size)
            }
            Intrinsic::Ctpop => {
                let count = n.pattern(operand(self, 0)?).count_ones();
                Narrow::bytes(i128::from(count), destination.size)
            }
            Intrinsic::Ctlz | Intrinsic::CtlzNonzero => {
                let count = n.pattern(operand(self, 0)?).leading_zeros() - (128 - n.bits);
                Narrow::bytes(i128::from(count), destination.size)
            }
            Intrinsic::Cttz | Intrinsic::CttzNonzero => {
                let count = n.pattern(operand(self, 0)?).trailing_zeros().min(n.bits);
                Narrow::bytes(i128::from(count), destination.size)
            }
            Intrinsic::Bswap => {
                if n.bits % 8 != 0 {
                    not_supported!(
                        "`bswap` of `{}`, whose {} bits are not a whole number of bytes",
                        n.name,
                        n.bits
                    );
                }
                let bytes = (n.bits / 8) as usize;
                let mut pattern = n.pattern(operand(self, 0)?).to_le_bytes();
                pattern[..bytes].reverse();
                Narrow::bytes(n.normalize(u128::from_le_bytes(pattern) as i128), size)
            }
            Intrinsic::Bitreverse => {
                let reversed = n.pattern(operand(self, 0)?).reverse_bits() >> (128 - n.bits);
                Narrow::bytes(n.normalize(reversed as i128), size)
            }
            Intrinsic::RotateLeft | Intrinsic::RotateRight => {
                let pattern = n.pattern(operand(self, 0)?);
                let amount = match args.get(1) {
                    Some(arg) => u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)),
                    None => 0,
                };
                // A rotation is by its amount mod N, as rustc's is mod BITS.
                let left = (amount % u128::from(n.bits)) as u32;
                let left = if intrinsic == Intrinsic::RotateLeft {
                    left
                } else {
                    (n.bits - left) % n.bits
                };
                let rotated = match left {
                    0 => pattern,
                    left => ((pattern << left) | (pattern >> (n.bits - left))) & n.mask(),
                };
                Narrow::bytes(n.normalize(rotated as i128), size)
            }
            Intrinsic::CarryingMulAdd
            | Intrinsic::CarrylessMul
            | Intrinsic::UncheckedFunnelShl
            | Intrinsic::UncheckedFunnelShr => {
                not_supported!(
                    "intrinsic `{intrinsic}` on the arbitrary-width integer `{}`",
                    n.name
                );
            }
            _ => return Ok(false),
        };
        destination.write_from_bytes(self, &result)?;
        Ok(true)
    }
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// `blaze_int_cast::<S, D>(x)`, which core writes its items of the integers of any width with:
    /// `x as D` between two integer types of any width, standard or not. It truncates to `D`'s
    /// width or extends by `S`'s sign, leaves `D`'s held form, and never panics.
    pub(super) fn int_cast(
        &mut self,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
    ) -> Result<'db, ()> {
        let types = generic_args.as_slice();
        let ty = |at: usize| types.get(at).and_then(|it| it.ty());
        let (Some(from), Some(to), [arg]) = (ty(0), ty(1), args) else {
            return Err(MirEvalError::InternalError(
                "`blaze_int_cast` is given two types and one operand".into(),
            ));
        };
        let signed = |ty: Ty<'db>| match ty.kind() {
            TyKind::Int(_) => Some(true),
            TyKind::Uint(_) => Some(false),
            _ => None,
        };
        let (Some(from_signed), Some(_)) = (signed(from), signed(to)) else {
            not_supported!(
                "intrinsic `blaze_int_cast` from `{from:?}` to `{to:?}`, which are not both integers"
            );
        };
        let bytes = arg.get(self)?;
        let value = match Narrow::of(from) {
            Some(n) => n.read(bytes)?,
            None => i128::from_le_bytes(pad16(bytes, from_signed.into())),
        };
        let value = Narrow::of(to).map_or(value, |n| n.normalize(value));
        let size = destination.size;
        destination.write_from_bytes(self, &value.to_le_bytes()[..size])
    }
}
