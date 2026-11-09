//! blaze: the integers of any width of `#![blaze::arbitrary_ints]` (`u1`..`u127` and
//! `i1`..`i127` but the standard widths), their traits and their inherent items
//! (docs/v2/plan/design/arbitrary-ints.md).
//!
//! Every trait impl here is written once, as a blanket impl over [`ArbitraryInt`], a marker the
//! trait solver answers from the type alone: it holds of every integer of an arbitrary width and
//! of nothing else. So `u3 + u3` type-checks against `impl<T: ArbitraryInt> Add for T`, and
//! every width's impl is a monomorphization of one body. No other type is affected: a goal
//! whose types cannot be an arbitrary width never lists these impls.
//!
//! The inherent items (`wrapping_add`, `MAX`, `count_ones`, ..) are the default items of
//! [`ArbitraryInt`]; method lookup and path resolution see them on `u3` as inherent, without
//! the trait in scope. `to_le_bytes` and its kin, whose array length depends on the width, are
//! inherent impls of the 22 widths that are a whole number of bytes.
//!
//! The bodies are core's generic integer intrinsics, which blaze evaluates and compiles at
//! every width, and [`intrinsics::blaze_int_cast`], `as` between two integer types of any width.
//! A panic says the type it is at: ``attempt to add with overflow at type `u3` ``, as the
//! builtin operators of these types do (at compile time, an overflow in a body here says the
//! operation only).

use crate::clone::TrivialClone;
use crate::cmp::Ordering;
use crate::hash::{Hash, Hasher};
use crate::intrinsics::{self, const_eval_select};
use crate::marker::PhantomData;
use crate::num::{IntErrorKind, TryFromIntError};
use crate::ops::{
    Add, AddAssign, BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Div, DivAssign,
    Mul, MulAssign, Neg, Not, Rem, RemAssign, Shl, ShlAssign, Shr, ShrAssign, Sub, SubAssign,
};
use crate::{fmt, mem};

/// An integer of an arbitrary width: `u3`, `i24`, every `uN` and `iN` of
/// `#![blaze::arbitrary_ints]` whose width is not 8, 16, 32, 64 or 128 bits.
///
/// The trait solver answers it from the type alone; no impl of it can be written. Its items
/// are the inherent items of every such type: `u3::MAX` and `x.wrapping_add(y)` need no import.
/// A generic function names it to be generic over the widths: `fn f<T: ArbitraryInt>(x: T)`
/// gets every operator, comparison and formatting trait the types have.
#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[lang = "arbitrary_int"]
#[rustc_deny_explicit_impl]
pub const trait ArbitraryInt: Copy {
    /// The size of this integer type in bits.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    const BITS: u32 = intrinsics::ctpop(narrow::<Self>(-1));

    /// The smallest value of this integer type: `0` unsigned, `-2^(BITS - 1)` signed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    const MIN: Self = narrow(if <Self as Shape>::SIGNED { -1i128 << (Self::BITS - 1) } else { 0 });

    /// The largest value of this integer type: `2^BITS - 1` unsigned, `2^(BITS - 1) - 1`
    /// signed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    const MAX: Self =
        narrow((u128::MAX >> (128 - Self::BITS) >> (<Self as Shape>::SIGNED as u32)) as i128);

    /// Wrapping (modular) addition: `self + rhs`, wrapping around at the type's boundary.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_add(self, rhs: Self) -> Self {
        intrinsics::wrapping_add(self, rhs)
    }

    /// Wrapping (modular) subtraction: `self - rhs`, wrapping around at the type's boundary.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_sub(self, rhs: Self) -> Self {
        intrinsics::wrapping_sub(self, rhs)
    }

    /// Wrapping (modular) multiplication: `self * rhs`, wrapping around at the type's
    /// boundary.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_mul(self, rhs: Self) -> Self {
        intrinsics::wrapping_mul(self, rhs)
    }

    /// Wrapping division: `self / rhs`, where `MIN / -1` of a signed type is `MIN`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn wrapping_div(self, rhs: Self) -> Self {
        div_overflowing(self, rhs).0
    }

    /// Wrapping remainder: `self % rhs`, where `MIN % -1` of a signed type is `0`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn wrapping_rem(self, rhs: Self) -> Self {
        rem_overflowing(self, rhs).0
    }

    /// Wrapping Euclidean division, where `MIN.div_euclid(-1)` of a signed type is `MIN`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn wrapping_div_euclid(self, rhs: Self) -> Self {
        div_euclid_overflowing(self, rhs).0
    }

    /// Wrapping Euclidean remainder, where `MIN.rem_euclid(-1)` of a signed type is `0`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn wrapping_rem_euclid(self, rhs: Self) -> Self {
        rem_euclid_overflowing(self, rhs).0
    }

    /// Wrapping (modular) negation: `-self`, wrapping around at the type's boundary.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_neg(self) -> Self {
        neg_overflowing(self).0
    }

    /// Panic-free shift left: `self << (rhs % Self::BITS)`.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_shl(self, rhs: u32) -> Self {
        shl_wrapping(self, rhs)
    }

    /// Panic-free shift right: `self >> (rhs % Self::BITS)`, arithmetic for a signed type.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_shr(self, rhs: u32) -> Self {
        shr_wrapping(self, rhs)
    }

    /// Wrapping (modular) exponentiation.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn wrapping_pow(self, exp: u32) -> Self {
        pow_overflowing(self, exp).0
    }

    /// Checked addition: `None` when `self + rhs` overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_add(self, rhs: Self) -> Option<Self> {
        checked(intrinsics::add_with_overflow(self, rhs))
    }

    /// Checked subtraction: `None` when `self - rhs` overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_sub(self, rhs: Self) -> Option<Self> {
        checked(intrinsics::sub_with_overflow(self, rhs))
    }

    /// Checked multiplication: `None` when `self * rhs` overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_mul(self, rhs: Self) -> Option<Self> {
        checked(intrinsics::mul_with_overflow(self, rhs))
    }

    /// Checked division: `None` when `rhs` is zero or the division overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_div(self, rhs: Self) -> Option<Self> {
        if is_zero(rhs) { None } else { checked(div_overflowing(self, rhs)) }
    }

    /// Checked remainder: `None` when `rhs` is zero or the division overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_rem(self, rhs: Self) -> Option<Self> {
        if is_zero(rhs) { None } else { checked(rem_overflowing(self, rhs)) }
    }

    /// Checked Euclidean division: `None` when `rhs` is zero or the division overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_div_euclid(self, rhs: Self) -> Option<Self> {
        if is_zero(rhs) { None } else { checked(div_euclid_overflowing(self, rhs)) }
    }

    /// Checked Euclidean remainder: `None` when `rhs` is zero or the division overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_rem_euclid(self, rhs: Self) -> Option<Self> {
        if is_zero(rhs) { None } else { checked(rem_euclid_overflowing(self, rhs)) }
    }

    /// Checked negation: `None` when `-self` overflows (every nonzero value of an unsigned
    /// type, and `MIN` of a signed one).
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_neg(self) -> Option<Self> {
        checked(neg_overflowing(self))
    }

    /// Checked shift left: `None` when `rhs` is `Self::BITS` or more.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_shl(self, rhs: u32) -> Option<Self> {
        if rhs < Self::BITS { Some(shl_wrapping(self, rhs)) } else { None }
    }

    /// Checked shift right: `None` when `rhs` is `Self::BITS` or more.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_shr(self, rhs: u32) -> Option<Self> {
        if rhs < Self::BITS { Some(shr_wrapping(self, rhs)) } else { None }
    }

    /// Checked exponentiation: `None` when it overflows.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn checked_pow(self, exp: u32) -> Option<Self> {
        checked(pow_overflowing(self, exp))
    }

    /// Saturating addition: `self + rhs`, at `MIN` or `MAX` where it would overflow.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn saturating_add(self, rhs: Self) -> Self {
        intrinsics::saturating_add(self, rhs)
    }

    /// Saturating subtraction: `self - rhs`, at `MIN` or `MAX` where it would overflow.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn saturating_sub(self, rhs: Self) -> Self {
        intrinsics::saturating_sub(self, rhs)
    }

    /// Saturating multiplication: `self * rhs`, at `MIN` or `MAX` where it would overflow.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn saturating_mul(self, rhs: Self) -> Self {
        match intrinsics::mul_with_overflow(self, rhs) {
            (product, false) => product,
            _ if negative(self) != negative(rhs) => Self::MIN,
            _ => Self::MAX,
        }
    }

    /// Saturating division: `self / rhs`, where `MIN / -1` of a signed type is `MAX`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn saturating_div(self, rhs: Self) -> Self {
        match div_overflowing(self, rhs) {
            (quotient, false) => quotient,
            _ => Self::MAX,
        }
    }

    /// Saturating exponentiation: at `MIN` or `MAX` where it would overflow.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn saturating_pow(self, exp: u32) -> Self {
        match pow_overflowing(self, exp) {
            (power, false) => power,
            _ if negative(self) && exp % 2 == 1 => Self::MIN,
            _ => Self::MAX,
        }
    }

    /// `self + rhs` wrapped, and whether it overflowed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_add(self, rhs: Self) -> (Self, bool) {
        intrinsics::add_with_overflow(self, rhs)
    }

    /// `self - rhs` wrapped, and whether it overflowed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_sub(self, rhs: Self) -> (Self, bool) {
        intrinsics::sub_with_overflow(self, rhs)
    }

    /// `self * rhs` wrapped, and whether it overflowed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_mul(self, rhs: Self) -> (Self, bool) {
        intrinsics::mul_with_overflow(self, rhs)
    }

    /// `self / rhs` wrapped, and whether it overflowed (only `MIN / -1` of a signed type
    /// does).
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn overflowing_div(self, rhs: Self) -> (Self, bool) {
        div_overflowing(self, rhs)
    }

    /// `self % rhs` wrapped, and whether the division overflowed (only `MIN % -1` of a signed
    /// type does).
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn overflowing_rem(self, rhs: Self) -> (Self, bool) {
        rem_overflowing(self, rhs)
    }

    /// `self.div_euclid(rhs)` wrapped, and whether it overflowed.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn overflowing_div_euclid(self, rhs: Self) -> (Self, bool) {
        div_euclid_overflowing(self, rhs)
    }

    /// `self.rem_euclid(rhs)` wrapped, and whether the division overflowed.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn overflowing_rem_euclid(self, rhs: Self) -> (Self, bool) {
        rem_euclid_overflowing(self, rhs)
    }

    /// `-self` wrapped, and whether it overflowed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_neg(self) -> (Self, bool) {
        neg_overflowing(self)
    }

    /// `self << (rhs % Self::BITS)`, and whether `rhs` is `Self::BITS` or more.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_shl(self, rhs: u32) -> (Self, bool) {
        (shl_wrapping(self, rhs), rhs >= Self::BITS)
    }

    /// `self >> (rhs % Self::BITS)`, and whether `rhs` is `Self::BITS` or more.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_shr(self, rhs: u32) -> (Self, bool) {
        (shr_wrapping(self, rhs), rhs >= Self::BITS)
    }

    /// `self` to the power `exp` wrapped, and whether it overflowed.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn overflowing_pow(self, exp: u32) -> (Self, bool) {
        pow_overflowing(self, exp)
    }

    /// `self` to the power `exp`.
    ///
    /// # Panics
    ///
    /// On overflow, when the calling crate checks overflow; it wraps otherwise.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    #[rustc_inherit_overflow_checks]
    fn pow(self, exp: u32) -> Self {
        let (power, overflowed) = pow_overflowing(self, exp);
        if intrinsics::overflow_checks() && overflowed {
            fail::<Self>("attempt to multiply with overflow")
        }
        power
    }

    /// Euclidean division: the `q` with `self = q * rhs + r` and `0 <= r < |rhs|`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero, or the division overflows (`MIN.div_euclid(-1)`).
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn div_euclid(self, rhs: Self) -> Self {
        match div_euclid_overflowing(self, rhs) {
            (quotient, false) => quotient,
            _ => fail::<Self>("attempt to divide with overflow"),
        }
    }

    /// Euclidean remainder: the `r` with `self = q * rhs + r` and `0 <= r < |rhs|`.
    ///
    /// # Panics
    ///
    /// When `rhs` is zero, or the division overflows (`MIN.rem_euclid(-1)`).
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    #[track_caller]
    fn rem_euclid(self, rhs: Self) -> Self {
        match rem_euclid_overflowing(self, rhs) {
            (remainder, false) => remainder,
            _ => fail::<Self>("attempt to calculate the remainder with overflow"),
        }
    }

    /// The number of ones in the binary representation of `self` (of its `BITS` bits).
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn count_ones(self) -> u32 {
        intrinsics::ctpop(self)
    }

    /// The number of zeros in the binary representation of `self` (of its `BITS` bits).
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn count_zeros(self) -> u32 {
        Self::BITS - intrinsics::ctpop(self)
    }

    /// The number of leading zeros in the binary representation of `self` (of its `BITS`
    /// bits): `BITS` for zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn leading_zeros(self) -> u32 {
        intrinsics::ctlz(self)
    }

    /// The number of trailing zeros in the binary representation of `self`: `BITS` for zero.
    #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
    #[must_use = "this returns the result of the operation, without modifying the original"]
    #[inline]
    fn trailing_zeros(self) -> u32 {
        intrinsics::cttz(self)
    }
}

/// A signed integer of an arbitrary width: `i3`, `i24`. The trait solver answers it from the
/// type alone; no impl of it can be written.
#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[lang = "arbitrary_signed"]
#[rustc_deny_explicit_impl]
pub trait ArbitrarySigned: ArbitraryInt {}

/// `Self` holds every value of the integer type `S`, on every target, and the two are
/// different integer types, at least one of an arbitrary width: `u16: IntFrom<u3>`. It gives
/// `From<S> for Self`. The trait solver answers it from the types alone; no impl of it can be
/// written.
#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[lang = "int_from"]
#[rustc_deny_explicit_impl]
pub trait IntFrom<S>: Copy {}

/// `Self` and the integer type `S` are different integer types, at least one of an arbitrary
/// width, and `Self` does not hold every value of `S`: `u3: IntTryFrom<u8>`. It gives
/// `TryFrom<S> for Self`. The trait solver answers it from the types alone; no impl of it can
/// be written.
#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[lang = "int_try_from"]
#[rustc_deny_explicit_impl]
pub trait IntTryFrom<S>: Copy {}

/// What the items here read off an integer type of any width, standard or not.
trait Shape: Copy {
    /// Whether the type is signed: `-1` of it is less than `0`.
    const SIGNED: bool = matches!(
        intrinsics::three_way_compare(narrow::<Self>(-1), narrow::<Self>(0)),
        Ordering::Less
    );
}

impl<T: Copy> Shape for T {}

/// `x` as an `i128`, which holds every value of an integer of an arbitrary width.
#[inline(always)]
const fn wide<T: Copy>(x: T) -> i128 {
    intrinsics::blaze_int_cast::<T, i128>(x)
}

/// `x` truncated to `T`.
#[inline(always)]
const fn narrow<T: Copy>(x: i128) -> T {
    intrinsics::blaze_int_cast::<i128, T>(x)
}

/// The bits of `x`, sign-extended to 128 when `T` is signed.
#[inline(always)]
const fn bits<T: Copy>(x: T) -> u128 {
    intrinsics::blaze_int_cast::<T, u128>(x)
}

/// The `T::BITS` bits of `x`, as an unsigned number: `-1i3` is 7.
#[inline(always)]
const fn pattern<T: ArbitraryInt>(x: T) -> u128 {
    bits(x) & (u128::MAX >> (128 - T::BITS))
}

/// Whether `x`, of an integer type of any width, is negative.
#[inline(always)]
const fn negative<T: Copy>(x: T) -> bool {
    <T as Shape>::SIGNED && wide(x) < 0
}

#[inline(always)]
const fn is_zero<T: Copy>(x: T) -> bool {
    bits(x) == 0
}

/// Whether `a / b` overflows: `MIN / -1` of a signed type.
#[inline(always)]
const fn div_overflows<T: ArbitraryInt>(a: T, b: T) -> bool {
    <T as Shape>::SIGNED && wide(b) == -1 && wide(a) == wide(T::MIN)
}

#[inline(always)]
const fn checked<T: Copy>((value, overflowed): (T, bool)) -> Option<T> {
    if overflowed { None } else { Some(value) }
}

/// Panics with `what` and, at run time, the type: ``attempt to add with overflow at type
/// `u3` ``, the words the builtin operators of these types panic with.
#[cold]
#[inline(never)]
#[track_caller]
#[rustc_allow_const_fn_unstable(const_eval_select)]
const fn fail<T: Copy>(what: &'static str) -> ! {
    const_eval_select!(
        @capture[T: Copy] { what: &'static str = what, _ty: PhantomData<T> = PhantomData::<T> } -> !:
        if const #[track_caller] {
            panic!("{}", what)
        } else #[track_caller] {
            panic!("{what} at type `{}`", crate::any::type_name::<T>())
        }
    )
}

/// `a / b` and whether it overflowed, as `overflowing_div`.
#[inline]
#[track_caller]
const fn div_overflowing<T: ArbitraryInt>(a: T, b: T) -> (T, bool) {
    if is_zero(b) {
        fail::<T>("attempt to divide by zero")
    }
    if div_overflows(a, b) {
        return (a, true);
    }
    // SAFETY: `b` is not zero and `a / b` is not `MIN / -1`.
    (unsafe { intrinsics::unchecked_div(a, b) }, false)
}

/// `a % b` and whether the division overflowed, as `overflowing_rem`.
#[inline]
#[track_caller]
const fn rem_overflowing<T: ArbitraryInt>(a: T, b: T) -> (T, bool) {
    if is_zero(b) {
        fail::<T>("attempt to calculate the remainder with a divisor of zero")
    }
    if div_overflows(a, b) {
        return (narrow(0), true);
    }
    // SAFETY: `b` is not zero and `a % b` is not `MIN % -1`.
    (unsafe { intrinsics::unchecked_rem(a, b) }, false)
}

/// `a.div_euclid(b)` and whether it overflowed, computed exactly in `i128`.
#[inline]
#[track_caller]
const fn div_euclid_overflowing<T: ArbitraryInt>(a: T, b: T) -> (T, bool) {
    if is_zero(b) {
        fail::<T>("attempt to divide by zero")
    }
    let (a, b) = (wide(a), wide(b));
    // Neither is past 127 bits, so `a / b` and `a % b` are exact (`MIN / -1` included).
    let mut quotient = a / b;
    if a % b < 0 {
        quotient = if b > 0 { quotient - 1 } else { quotient + 1 };
    }
    let value = narrow::<T>(quotient);
    (value, wide(value) != quotient)
}

/// `a.rem_euclid(b)` and whether the division overflowed, computed exactly in `i128`.
#[inline]
#[track_caller]
const fn rem_euclid_overflowing<T: ArbitraryInt>(a: T, b: T) -> (T, bool) {
    if is_zero(b) {
        fail::<T>("attempt to calculate the remainder with a divisor of zero")
    }
    let overflowed = div_overflows(a, b);
    let (a, b) = (wide(a), wide(b));
    let mut remainder = a % b;
    if remainder < 0 {
        remainder = if b < 0 { remainder - b } else { remainder + b };
    }
    (narrow(remainder), overflowed)
}

/// `-a` and whether it overflowed: every nonzero value of an unsigned type and `MIN` of a
/// signed one does.
#[inline(always)]
const fn neg_overflowing<T: ArbitraryInt>(a: T) -> (T, bool) {
    intrinsics::sub_with_overflow(narrow::<T>(0), a)
}

/// `a << (amount % T::BITS)`.
#[inline(always)]
const fn shl_wrapping<T: ArbitraryInt>(a: T, amount: u32) -> T {
    // SAFETY: the amount is below `T::BITS`.
    unsafe { intrinsics::unchecked_shl(a, amount % T::BITS) }
}

/// `a >> (amount % T::BITS)`, arithmetic for a signed `T`.
#[inline(always)]
const fn shr_wrapping<T: ArbitraryInt>(a: T, amount: u32) -> T {
    // SAFETY: the amount is below `T::BITS`.
    unsafe { intrinsics::unchecked_shr(a, amount % T::BITS) }
}

/// `base` to the power `exp` wrapped, and whether it overflowed, by squaring as core's
/// `overflowing_pow` does.
#[inline]
const fn pow_overflowing<T: ArbitraryInt>(base: T, exp: u32) -> (T, bool) {
    if <T as Shape>::SIGNED && T::BITS == 1 {
        // `i1` holds `0` and `-1` but no `1`: `0` to a power other than `0` is `0`, `-1` to an
        // odd power is `-1`, and every other power is `1`, which wraps to `-1`. Squaring would
        // see `-1 * -1` overflow even where the power fits.
        return if exp != 0 && is_zero(base) { (base, false) } else { (narrow(-1), exp % 2 == 0) };
    }
    let mut overflowed = false;
    let (mut acc, mut base, mut exp) = (narrow::<T>(1), base, exp);
    while exp > 0 {
        if exp & 1 == 1 {
            let (product, o) = intrinsics::mul_with_overflow(acc, base);
            acc = product;
            overflowed |= o;
        }
        exp >>= 1;
        if exp > 0 {
            let (square, o) = intrinsics::mul_with_overflow(base, base);
            base = square;
            overflowed |= o;
        }
    }
    (acc, overflowed)
}

/// `a + b` of `Add`: it panics on overflow when the calling crate checks overflow.
#[inline]
#[track_caller]
#[rustc_inherit_overflow_checks]
const fn op_add<T: ArbitraryInt>(a: T, b: T) -> T {
    let (sum, overflowed) = intrinsics::add_with_overflow(a, b);
    if intrinsics::overflow_checks() && overflowed {
        fail::<T>("attempt to add with overflow")
    }
    sum
}

/// `a - b` of `Sub`: it panics on overflow when the calling crate checks overflow.
#[inline]
#[track_caller]
#[rustc_inherit_overflow_checks]
const fn op_sub<T: ArbitraryInt>(a: T, b: T) -> T {
    let (difference, overflowed) = intrinsics::sub_with_overflow(a, b);
    if intrinsics::overflow_checks() && overflowed {
        fail::<T>("attempt to subtract with overflow")
    }
    difference
}

/// `a * b` of `Mul`: it panics on overflow when the calling crate checks overflow.
#[inline]
#[track_caller]
#[rustc_inherit_overflow_checks]
const fn op_mul<T: ArbitraryInt>(a: T, b: T) -> T {
    let (product, overflowed) = intrinsics::mul_with_overflow(a, b);
    if intrinsics::overflow_checks() && overflowed {
        fail::<T>("attempt to multiply with overflow")
    }
    product
}

/// `a / b` of `Div`: it always panics on a zero `b` and on `MIN / -1`.
#[inline]
#[track_caller]
const fn op_div<T: ArbitraryInt>(a: T, b: T) -> T {
    match div_overflowing(a, b) {
        (quotient, false) => quotient,
        _ => fail::<T>("attempt to divide with overflow"),
    }
}

/// `a % b` of `Rem`: it always panics on a zero `b` and on `MIN % -1`.
#[inline]
#[track_caller]
const fn op_rem<T: ArbitraryInt>(a: T, b: T) -> T {
    match rem_overflowing(a, b) {
        (remainder, false) => remainder,
        _ => fail::<T>("attempt to calculate the remainder with overflow"),
    }
}

/// `-a` of `Neg`: it panics on overflow (`-MIN`) when the calling crate checks overflow.
#[inline]
#[track_caller]
#[rustc_inherit_overflow_checks]
const fn op_neg<T: ArbitraryInt>(a: T) -> T {
    let (negated, overflowed) = neg_overflowing(a);
    if intrinsics::overflow_checks() && overflowed {
        fail::<T>("attempt to negate with overflow")
    }
    negated
}

/// `!a` of `Not`.
#[inline(always)]
const fn op_not<T: ArbitraryInt>(a: T) -> T {
    narrow(!wide(a))
}

/// `a & b` of `BitAnd`.
#[inline(always)]
const fn op_bitand<T: ArbitraryInt>(a: T, b: T) -> T {
    narrow(wide(a) & wide(b))
}

/// `a | b` of `BitOr`.
#[inline(always)]
const fn op_bitor<T: ArbitraryInt>(a: T, b: T) -> T {
    narrow(wide(a) | wide(b))
}

/// `a ^ b` of `BitXor`.
#[inline(always)]
const fn op_bitxor<T: ArbitraryInt>(a: T, b: T) -> T {
    narrow(wide(a) ^ wide(b))
}

/// `a << amount` of `Shl`, `amount` an unsigned number of its own type's width (`-1i32` is
/// `u32::MAX`): it panics when `amount` is `T::BITS` or more and the calling crate checks
/// overflow, and shifts by `amount % T::BITS` otherwise.
#[inline]
#[track_caller]
#[rustc_inherit_overflow_checks]
const fn op_shl<T: ArbitraryInt>(a: T, amount: u128) -> T {
    let bits = T::BITS as u128;
    if intrinsics::overflow_checks() && amount >= bits {
        fail::<T>("attempt to shift left with overflow")
    }
    shl_wrapping(a, (amount % bits) as u32)
}

/// `a >> amount` of `Shr`, as [`op_shl`].
#[inline]
#[track_caller]
#[rustc_inherit_overflow_checks]
const fn op_shr<T: ArbitraryInt>(a: T, amount: u128) -> T {
    let bits = T::BITS as u128;
    if intrinsics::overflow_checks() && amount >= bits {
        fail::<T>("attempt to shift right with overflow")
    }
    shr_wrapping(a, (amount % bits) as u32)
}

/// `impl $Op<$R> for $L` (given) forwarded to its reference forms: `&$L op $R`, `$L op &$R`,
/// `&$L op &$R`, `$L op= $R` and `$L op= &$R`.
macro_rules! arbitrary_int_ref_forms {
    ([$($g:tt)*] $Op:ident $op:ident $OpAssign:ident $op_assign:ident, $L:ty, $R:ty) => {
        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<$($g)*> $Op<$R> for &$L {
            type Output = <$L as $Op<$R>>::Output;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn $op(self, rhs: $R) -> <$L as $Op<$R>>::Output {
                $Op::$op(*self, rhs)
            }
        }

        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<$($g)*> $Op<&$R> for $L {
            type Output = <$L as $Op<$R>>::Output;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn $op(self, rhs: &$R) -> <$L as $Op<$R>>::Output {
                $Op::$op(self, *rhs)
            }
        }

        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<$($g)*> $Op<&$R> for &$L {
            type Output = <$L as $Op<$R>>::Output;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn $op(self, rhs: &$R) -> <$L as $Op<$R>>::Output {
                $Op::$op(*self, *rhs)
            }
        }

        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<$($g)*> $OpAssign<$R> for $L {
            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn $op_assign(&mut self, rhs: $R) {
                *self = $Op::$op(*self, rhs);
            }
        }

        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<$($g)*> $OpAssign<&$R> for $L {
            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn $op_assign(&mut self, rhs: &$R) {
                *self = $Op::$op(*self, *rhs);
            }
        }
    };
}

/// `impl<T: ArbitraryInt> $Op for T` by `$f`, with its reference and assign forms.
macro_rules! arbitrary_int_binops {
    ($($Op:ident $op:ident $OpAssign:ident $op_assign:ident => $f:ident;)*) => {$(
        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<T: ArbitraryInt> $Op for T {
            type Output = T;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn $op(self, rhs: T) -> T {
                $f(self, rhs)
            }
        }

        arbitrary_int_ref_forms! { [T: ArbitraryInt] $Op $op $OpAssign $op_assign, T, T }
    )*};
}

arbitrary_int_binops! {
    Add add AddAssign add_assign => op_add;
    Sub sub SubAssign sub_assign => op_sub;
    Mul mul MulAssign mul_assign => op_mul;
    Div div DivAssign div_assign => op_div;
    Rem rem RemAssign rem_assign => op_rem;
    BitAnd bitand BitAndAssign bitand_assign => op_bitand;
    BitOr bitor BitOrAssign bitor_assign => op_bitor;
    BitXor bitxor BitXorAssign bitxor_assign => op_bitxor;
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_ops", issue = "143802")]
const impl<T: ArbitrarySigned> Neg for T {
    type Output = T;

    #[inline]
    #[track_caller]
    #[rustc_inherit_overflow_checks]
    fn neg(self) -> T {
        op_neg(self)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_ops", issue = "143802")]
const impl<T: ArbitrarySigned> Neg for &T {
    type Output = T;

    #[inline]
    #[track_caller]
    #[rustc_inherit_overflow_checks]
    fn neg(self) -> T {
        op_neg(*self)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_ops", issue = "143802")]
const impl<T: ArbitraryInt> Not for T {
    type Output = T;

    #[inline]
    fn not(self) -> T {
        op_not(self)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_ops", issue = "143802")]
const impl<T: ArbitraryInt> Not for &T {
    type Output = T;

    #[inline]
    fn not(self) -> T {
        op_not(*self)
    }
}

/// `impl Shl<$R> for T` and `impl Shr<$R> for T` of an arbitrary width `T`, with their reference
/// and assign forms; `$amount` reads the amount `$x` as an unsigned number of its own width.
macro_rules! arbitrary_int_shifts {
    ($([$($g:tt)*] $R:ty => |$x:ident| $amount:expr;)*) => {$(
        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<T: ArbitraryInt, $($g)*> Shl<$R> for T {
            type Output = T;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn shl(self, $x: $R) -> T {
                op_shl(self, $amount)
            }
        }

        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<T: ArbitraryInt, $($g)*> Shr<$R> for T {
            type Output = T;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn shr(self, $x: $R) -> T {
                op_shr(self, $amount)
            }
        }

        arbitrary_int_ref_forms! { [T: ArbitraryInt, $($g)*] Shl shl ShlAssign shl_assign, T, $R }
        arbitrary_int_ref_forms! { [T: ArbitraryInt, $($g)*] Shr shr ShrAssign shr_assign, T, $R }
    )*};
}

arbitrary_int_shifts! {
    [S: ArbitraryInt] S => |rhs| pattern(rhs);
    [] u8 => |rhs| rhs as u128;
    [] u16 => |rhs| rhs as u128;
    [] u32 => |rhs| rhs as u128;
    [] u64 => |rhs| rhs as u128;
    [] u128 => |rhs| rhs;
    [] usize => |rhs| rhs as u128;
    [] i8 => |rhs| rhs as u8 as u128;
    [] i16 => |rhs| rhs as u16 as u128;
    [] i32 => |rhs| rhs as u32 as u128;
    [] i64 => |rhs| rhs as u64 as u128;
    [] i128 => |rhs| rhs as u128;
    [] isize => |rhs| rhs as usize as u128;
}

/// `impl<S: ArbitraryInt> Shl<S> for $t` and `Shr` of the standard integers, with their
/// reference and assign forms: a standard integer shifted by an arbitrary width, with the
/// standard integer's words when it overflows.
macro_rules! shifts_by_arbitrary_int {
    ($($t:ty)*) => {$(
        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<S: ArbitraryInt> Shl<S> for $t {
            type Output = $t;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn shl(self, rhs: S) -> $t {
                let (amount, bits) = (pattern(rhs), <$t>::BITS as u128);
                if intrinsics::overflow_checks() && amount >= bits {
                    panic!("attempt to shift left with overflow")
                }
                self.wrapping_shl((amount % bits) as u32)
            }
        }

        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        #[rustc_const_unstable(feature = "const_ops", issue = "143802")]
        const impl<S: ArbitraryInt> Shr<S> for $t {
            type Output = $t;

            #[inline]
            #[track_caller]
            #[rustc_inherit_overflow_checks]
            fn shr(self, rhs: S) -> $t {
                let (amount, bits) = (pattern(rhs), <$t>::BITS as u128);
                if intrinsics::overflow_checks() && amount >= bits {
                    panic!("attempt to shift right with overflow")
                }
                self.wrapping_shr((amount % bits) as u32)
            }
        }

        arbitrary_int_ref_forms! { [S: ArbitraryInt] Shl shl ShlAssign shl_assign, $t, S }
        arbitrary_int_ref_forms! { [S: ArbitraryInt] Shr shr ShrAssign shr_assign, $t, S }
    )*};
}

shifts_by_arbitrary_int! { u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize }

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_cmp", issue = "143800")]
const impl<T: ArbitraryInt> PartialEq for T {
    #[inline]
    fn eq(&self, other: &T) -> bool {
        wide(*self) == wide(*other)
    }

    #[inline]
    fn ne(&self, other: &T) -> bool {
        wide(*self) != wide(*other)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_cmp", issue = "143800")]
const impl<T: ArbitraryInt> Eq for T {}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_cmp", issue = "143800")]
const impl<T: ArbitraryInt> PartialOrd for T {
    #[inline]
    fn partial_cmp(&self, other: &T) -> Option<Ordering> {
        Some(intrinsics::three_way_compare(wide(*self), wide(*other)))
    }

    #[inline]
    fn lt(&self, other: &T) -> bool {
        wide(*self) < wide(*other)
    }

    #[inline]
    fn le(&self, other: &T) -> bool {
        wide(*self) <= wide(*other)
    }

    #[inline]
    fn gt(&self, other: &T) -> bool {
        wide(*self) > wide(*other)
    }

    #[inline]
    fn ge(&self, other: &T) -> bool {
        wide(*self) >= wide(*other)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_cmp", issue = "143800")]
const impl<T: ArbitraryInt> Ord for T {
    #[inline]
    fn cmp(&self, other: &T) -> Ordering {
        intrinsics::three_way_compare(wide(*self), wide(*other))
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_clone", issue = "142757")]
const impl<T: ArbitraryInt> Clone for T {
    #[inline(always)]
    fn clone(&self) -> T {
        *self
    }
}

#[doc(hidden)]
#[unstable(feature = "trivial_clone", issue = "none")]
#[rustc_const_unstable(feature = "const_clone", issue = "142757")]
const unsafe impl<T: ArbitraryInt> TrivialClone for T {}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_default", issue = "143894")]
const impl<T: ArbitraryInt> Default for T {
    /// Returns the default value of `0`.
    #[inline(always)]
    fn default() -> T {
        narrow(0)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
impl<T: ArbitraryInt> Hash for T {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The value's storage, as a standard integer of the same size hashes.
        let bits = bits(*self);
        match mem::size_of::<T>() {
            1 => state.write_u8(bits as u8),
            2 => state.write_u16(bits as u16),
            4 => state.write_u32(bits as u32),
            8 => state.write_u64(bits as u64),
            _ => state.write_u128(bits),
        }
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
impl<T: ArbitraryInt> fmt::Display for T {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&wide(*self), f)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
impl<T: ArbitraryInt> fmt::Debug for T {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match f.options().get_debug_as_hex() {
            Some(fmt::DebugAsHex::Lower) => fmt::LowerHex::fmt(self, f),
            Some(fmt::DebugAsHex::Upper) => fmt::UpperHex::fmt(self, f),
            None => fmt::Display::fmt(self, f),
        }
    }
}

/// `impl<T: ArbitraryInt> fmt::$Trait for T`: the value's `BITS` bits, as a negative value of a
/// standard signed integer is written in its two's complement.
macro_rules! arbitrary_int_radix_fmt {
    ($($Trait:ident)*) => {$(
        #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
        impl<T: ArbitraryInt> fmt::$Trait for T {
            #[inline]
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::$Trait::fmt(&pattern(*self), f)
            }
        }
    )*};
}

arbitrary_int_radix_fmt! { LowerHex UpperHex Binary Octal }

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_convert", issue = "143773")]
const impl<S: Copy, D: IntFrom<S>> From<S> for D {
    /// Converts `value` losslessly: every value of `S` is one of `D`.
    #[inline(always)]
    fn from(value: S) -> D {
        intrinsics::blaze_int_cast::<S, D>(value)
    }
}

#[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
#[rustc_const_unstable(feature = "const_convert", issue = "143773")]
const impl<S: Copy, D: IntTryFrom<S>> TryFrom<S> for D {
    type Error = TryFromIntError;

    /// Converts `value` when it is a value of `D`, and errs otherwise.
    #[inline]
    fn try_from(value: S) -> Result<D, TryFromIntError> {
        let target = intrinsics::blaze_int_cast::<S, D>(value);
        let back = intrinsics::blaze_int_cast::<D, S>(target);
        if bits(back) == bits(value) && negative(target) == negative(value) {
            Ok(target)
        } else if negative(value) {
            Err(TryFromIntError(IntErrorKind::NegOverflow))
        } else {
            Err(TryFromIntError(IntErrorKind::PosOverflow))
        }
    }
}

/// The byte methods of the arbitrary widths that are a whole number of bytes, `$bytes` of
/// them, each held in the standard integer `$storage`.
macro_rules! arbitrary_int_bytes {
    ($($t:ident $storage:ident $bytes:literal;)*) => {$(
        impl $t {
            /// The memory representation of this integer as a byte array in little-endian
            /// byte order.
            #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[must_use = "this returns the result of the operation, without modifying the original"]
            #[inline]
            pub const fn to_le_bytes(self) -> [u8; $bytes] {
                let all = (self as $storage).to_le_bytes();
                let mut bytes = [0; $bytes];
                let mut at = 0;
                while at < $bytes {
                    bytes[at] = all[at];
                    at += 1;
                }
                bytes
            }

            /// The memory representation of this integer as a byte array in big-endian
            /// (network) byte order.
            #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[must_use = "this returns the result of the operation, without modifying the original"]
            #[inline]
            pub const fn to_be_bytes(self) -> [u8; $bytes] {
                let all = (self as $storage).to_be_bytes();
                let skip = all.len() - $bytes;
                let mut bytes = [0; $bytes];
                let mut at = 0;
                while at < $bytes {
                    bytes[at] = all[skip + at];
                    at += 1;
                }
                bytes
            }

            /// The memory representation of this integer as a byte array in the target's
            /// byte order.
            #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[must_use = "this returns the result of the operation, without modifying the original"]
            #[inline]
            pub const fn to_ne_bytes(self) -> [u8; $bytes] {
                if cfg!(target_endian = "little") { self.to_le_bytes() } else { self.to_be_bytes() }
            }

            /// The integer whose representation is `bytes` in little-endian byte order.
            #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[must_use]
            #[inline]
            pub const fn from_le_bytes(bytes: [u8; $bytes]) -> $t {
                let mut all = (0 as $storage).to_le_bytes();
                let mut at = 0;
                while at < $bytes {
                    all[at] = bytes[at];
                    at += 1;
                }
                // The cast keeps the low `$bytes` bytes, extended by the type's sign.
                <$storage>::from_le_bytes(all) as $t
            }

            /// The integer whose representation is `bytes` in big-endian (network) byte order.
            #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[must_use]
            #[inline]
            pub const fn from_be_bytes(bytes: [u8; $bytes]) -> $t {
                let mut all = (0 as $storage).to_be_bytes();
                let skip = all.len() - $bytes;
                let mut at = 0;
                while at < $bytes {
                    all[skip + at] = bytes[at];
                    at += 1;
                }
                <$storage>::from_be_bytes(all) as $t
            }

            /// The integer whose representation is `bytes` in the target's byte order.
            #[stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[rustc_const_stable(feature = "blaze_arbitrary_ints", since = "CURRENT_RUSTC_VERSION")]
            #[must_use]
            #[inline]
            pub const fn from_ne_bytes(bytes: [u8; $bytes]) -> $t {
                if cfg!(target_endian = "little") {
                    Self::from_le_bytes(bytes)
                } else {
                    Self::from_be_bytes(bytes)
                }
            }
        }
    )*};
}

arbitrary_int_bytes! {
    u24 u32 3; i24 i32 3;
    u40 u64 5; i40 i64 5;
    u48 u64 6; i48 i64 6;
    u56 u64 7; i56 i64 7;
    u72 u128 9; i72 i128 9;
    u80 u128 10; i80 i128 10;
    u88 u128 11; i88 i128 11;
    u96 u128 12; i96 i128 12;
    u104 u128 13; i104 i128 13;
    u112 u128 14; i112 i128 14;
    u120 u128 15; i120 i128 15;
}
