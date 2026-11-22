//! THE LANE SEAM (rule 78): every function of this crate is written once over [`Real`], and runs
//! as `f64` (one lane, the scalar twin) or as `Simd<f64, N>` (any lane count). Both forms do the
//! same IEEE basic operations in the same order on each lane, and a branch is a [`Real::select`]
//! of both arms, so lane `i` of the vector form equals the scalar form on lane `i`'s input, bit
//! for bit. The tests hold the two instantiations equal on drawn inputs.
//!
//! What a lane type must give, and nothing else: the four basic operations and negation, an
//! exactly rounded square root, a fused multiply-add (asked for by name: nothing here fuses
//! implicitly), comparisons into a mask, a select, and the bits of each lane as `u64` with
//! wrapping integer operations on them. `core::simd` has no square root or fused multiply-add
//! without `std` (`std_float`), so the vector form takes them lane by lane from
//! `core::f64::math`, which lowers each to the one IEEE operation (no libm call, no unsafe).

use core::{
    ops::{Add, BitAnd, BitOr, BitXor, Div, Mul, Neg, Not, Sub},
    simd::{
        Mask, Select, Simd,
        cmp::{SimdPartialEq, SimdPartialOrd},
        num::SimdFloat,
    },
};

/// One lane (`f64`) or many (`Simd<f64, N>`) of f64 values: what the functions are written over.
pub trait Real:
    Copy + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> + Div<Output = Self> + Neg<Output = Self>
{
    /// One truth value per lane.
    type Mask: Copy + BitAnd<Output = Self::Mask> + BitOr<Output = Self::Mask> + Not<Output = Self::Mask>;
    /// The bits of each lane, with wrapping integer arithmetic.
    type Bits: Copy
        + BitAnd<Output = Self::Bits>
        + BitOr<Output = Self::Bits>
        + BitXor<Output = Self::Bits>;

    /// `x` in every lane.
    fn splat(x: f64) -> Self;
    /// The bits of each lane.
    fn to_bits(self) -> Self::Bits;
    /// The values of the bits of each lane.
    fn from_bits(bits: Self::Bits) -> Self;
    /// `b` in every lane.
    fn bits(b: u64) -> Self::Bits;
    /// Wrapping sum of bits.
    fn bits_add(a: Self::Bits, b: Self::Bits) -> Self::Bits;
    /// Wrapping difference of bits.
    fn bits_sub(a: Self::Bits, b: Self::Bits) -> Self::Bits;
    /// Wrapping product of bits.
    fn bits_mul(a: Self::Bits, b: Self::Bits) -> Self::Bits;
    /// Logical shift left by a constant (`by < 64`).
    fn bits_shl(a: Self::Bits, by: u32) -> Self::Bits;
    /// Logical shift right by a constant (`by < 64`).
    fn bits_shr(a: Self::Bits, by: u32) -> Self::Bits;
    /// Logical shift left of each lane by that lane's amount (each `< 64`).
    fn bits_shl_each(a: Self::Bits, by: Self::Bits) -> Self::Bits;
    /// Unsigned `a < b` per lane.
    fn bits_lt(a: Self::Bits, b: Self::Bits) -> Self::Mask;
    /// `a == b` per lane.
    fn bits_eq(a: Self::Bits, b: Self::Bits) -> Self::Mask;
    /// `m ? a : b` per lane, on bits.
    fn select_bits(m: Self::Mask, a: Self::Bits, b: Self::Bits) -> Self::Bits;

    /// The exactly rounded square root (IEEE 754 `squareRoot`).
    fn sqrt(self) -> Self;
    /// `self * a + b` with one rounding (IEEE 754 `fusedMultiplyAdd`).
    fn fma(self, a: Self, b: Self) -> Self;
    /// `self < o` per lane (false where either is NaN).
    fn lt(self, o: Self) -> Self::Mask;
    /// `self <= o` per lane (false where either is NaN).
    fn le(self, o: Self) -> Self::Mask;
    /// `self == o` per lane (false where either is NaN; `-0 == +0`).
    fn eq(self, o: Self) -> Self::Mask;
    /// `m ? a : b` per lane.
    fn select(m: Self::Mask, a: Self, b: Self) -> Self;
    /// The mask true in every lane.
    fn all_true() -> Self::Mask;
    /// `f` of each lane of `a` and `b`, lane by lane: for the operations with no vector form
    /// (a table lookup), so they run once per lane exactly as the scalar form does.
    fn map2(a: Self, b: Self, f: impl Fn(f64, f64) -> f64) -> Self;
    /// `f` of each lane of `a`, `b` and `c`, lane by lane.
    fn map3(a: Self, b: Self, c: Self, f: impl Fn(f64, f64, f64) -> f64) -> Self;
    /// `f` of each lane with three results per lane, lane by lane: for the one path with no
    /// vector form (the Payne-Hanek reduction's integer arithmetic), run once per lane exactly as
    /// the scalar form runs it.
    fn map_triple(self, f: impl Fn(f64) -> (f64, f64, f64)) -> (Self, Self, Self);

    /// `|self|`: the sign bit cleared.
    #[inline]
    fn abs(self) -> Self {
        Self::from_bits(self.to_bits() & Self::bits(!SIGN))
    }

    /// The magnitude of `self` with the sign of `sign`.
    #[inline]
    fn copysign(self, sign: Self) -> Self {
        Self::from_bits((self.to_bits() & Self::bits(!SIGN)) | (sign.to_bits() & Self::bits(SIGN)))
    }

    /// Whether each lane is NaN.
    #[inline]
    fn is_nan(self) -> Self::Mask {
        !self.eq(self)
    }

    /// Whether each lane's sign bit is set (`-0` and negative NaNs too).
    #[inline]
    fn sign_bit(self) -> Self::Mask {
        !Self::bits_eq(self.to_bits() & Self::bits(SIGN), Self::bits(0))
    }
}

/// The sign bit of an f64.
pub const SIGN: u64 = 1 << 63;

impl Real for f64 {
    type Mask = bool;
    type Bits = u64;

    #[inline]
    fn splat(x: f64) -> f64 {
        x
    }
    #[inline]
    fn to_bits(self) -> u64 {
        f64::to_bits(self)
    }
    #[inline]
    fn from_bits(bits: u64) -> f64 {
        f64::from_bits(bits)
    }
    #[inline]
    fn bits(b: u64) -> u64 {
        b
    }
    #[inline]
    fn bits_add(a: u64, b: u64) -> u64 {
        a.wrapping_add(b)
    }
    #[inline]
    fn bits_sub(a: u64, b: u64) -> u64 {
        a.wrapping_sub(b)
    }
    #[inline]
    fn bits_mul(a: u64, b: u64) -> u64 {
        a.wrapping_mul(b)
    }
    #[inline]
    fn bits_shl(a: u64, by: u32) -> u64 {
        a << by
    }
    #[inline]
    fn bits_shr(a: u64, by: u32) -> u64 {
        a >> by
    }
    #[inline]
    fn bits_shl_each(a: u64, by: u64) -> u64 {
        a << by
    }
    #[inline]
    fn bits_lt(a: u64, b: u64) -> bool {
        a < b
    }
    #[inline]
    fn bits_eq(a: u64, b: u64) -> bool {
        a == b
    }
    #[inline]
    fn select_bits(m: bool, a: u64, b: u64) -> u64 {
        if m { a } else { b }
    }
    #[inline]
    fn sqrt(self) -> f64 {
        core::f64::math::sqrt(self)
    }
    #[inline]
    fn fma(self, a: f64, b: f64) -> f64 {
        core::f64::math::mul_add(self, a, b)
    }
    #[inline]
    fn lt(self, o: f64) -> bool {
        self < o
    }
    #[inline]
    fn le(self, o: f64) -> bool {
        self <= o
    }
    #[inline]
    fn eq(self, o: f64) -> bool {
        self == o
    }
    #[inline]
    fn select(m: bool, a: f64, b: f64) -> f64 {
        if m { a } else { b }
    }
    #[inline]
    fn all_true() -> bool {
        true
    }
    #[inline]
    fn map2(a: f64, b: f64, f: impl Fn(f64, f64) -> f64) -> f64 {
        f(a, b)
    }
    #[inline]
    fn map3(a: f64, b: f64, c: f64, f: impl Fn(f64, f64, f64) -> f64) -> f64 {
        f(a, b, c)
    }
    #[inline]
    fn map_triple(self, f: impl Fn(f64) -> (f64, f64, f64)) -> (f64, f64, f64) {
        f(self)
    }
}

impl<const N: usize> Real for Simd<f64, N> {
    type Mask = Mask<i64, N>;
    type Bits = Simd<u64, N>;

    #[inline]
    fn splat(x: f64) -> Self {
        Simd::splat(x)
    }
    #[inline]
    fn to_bits(self) -> Simd<u64, N> {
        SimdFloat::to_bits(self)
    }
    #[inline]
    fn from_bits(bits: Simd<u64, N>) -> Self {
        SimdFloat::from_bits(bits)
    }
    #[inline]
    fn bits(b: u64) -> Simd<u64, N> {
        Simd::splat(b)
    }
    #[inline]
    fn bits_add(a: Simd<u64, N>, b: Simd<u64, N>) -> Simd<u64, N> {
        a + b
    }
    #[inline]
    fn bits_sub(a: Simd<u64, N>, b: Simd<u64, N>) -> Simd<u64, N> {
        a - b
    }
    #[inline]
    fn bits_mul(a: Simd<u64, N>, b: Simd<u64, N>) -> Simd<u64, N> {
        a * b
    }
    #[inline]
    fn bits_shl(a: Simd<u64, N>, by: u32) -> Simd<u64, N> {
        a << u64::from(by)
    }
    #[inline]
    fn bits_shr(a: Simd<u64, N>, by: u32) -> Simd<u64, N> {
        a >> u64::from(by)
    }
    #[inline]
    fn bits_shl_each(a: Simd<u64, N>, by: Simd<u64, N>) -> Simd<u64, N> {
        a << by
    }
    #[inline]
    fn bits_lt(a: Simd<u64, N>, b: Simd<u64, N>) -> Mask<i64, N> {
        a.simd_lt(b)
    }
    #[inline]
    fn bits_eq(a: Simd<u64, N>, b: Simd<u64, N>) -> Mask<i64, N> {
        a.simd_eq(b)
    }
    #[inline]
    fn select_bits(m: Mask<i64, N>, a: Simd<u64, N>, b: Simd<u64, N>) -> Simd<u64, N> {
        m.select(a, b)
    }
    #[inline]
    fn sqrt(self) -> Self {
        Simd::from_array(self.to_array().map(core::f64::math::sqrt))
    }
    #[inline]
    fn fma(self, a: Self, b: Self) -> Self {
        let (x, a, b) = (self.to_array(), a.to_array(), b.to_array());
        Simd::from_array(core::array::from_fn(|i| core::f64::math::mul_add(x[i], a[i], b[i])))
    }
    #[inline]
    fn lt(self, o: Self) -> Mask<i64, N> {
        self.simd_lt(o)
    }
    #[inline]
    fn le(self, o: Self) -> Mask<i64, N> {
        self.simd_le(o)
    }
    #[inline]
    fn eq(self, o: Self) -> Mask<i64, N> {
        self.simd_eq(o)
    }
    #[inline]
    fn select(m: Mask<i64, N>, a: Self, b: Self) -> Self {
        m.select(a, b)
    }
    #[inline]
    fn all_true() -> Mask<i64, N> {
        Mask::splat(true)
    }
    #[inline]
    fn map2(a: Self, b: Self, f: impl Fn(f64, f64) -> f64) -> Self {
        let (a, b) = (a.to_array(), b.to_array());
        Simd::from_array(core::array::from_fn(|i| f(a[i], b[i])))
    }
    #[inline]
    fn map3(a: Self, b: Self, c: Self, f: impl Fn(f64, f64, f64) -> f64) -> Self {
        let (a, b, c) = (a.to_array(), b.to_array(), c.to_array());
        Simd::from_array(core::array::from_fn(|i| f(a[i], b[i], c[i])))
    }
    #[inline]
    fn map_triple(self, f: impl Fn(f64) -> (f64, f64, f64)) -> (Self, Self, Self) {
        let a = self.to_array();
        let (mut p, mut q, mut r) = ([0.0; N], [0.0; N], [0.0; N]);
        for i in 0..N {
            (p[i], q[i], r[i]) = f(a[i]);
        }
        (Simd::from_array(p), Simd::from_array(q), Simd::from_array(r))
    }
}
