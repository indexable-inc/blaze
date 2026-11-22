//! Slang's free functions, under Slang's names, plus deterministic `#![no_std]` IEEE-754 math.
//!
//! On the host every function runs deterministically with `core::f32::math` and `f64`-precision
//! range-reduced polynomials (accurate to `< 1 ULP` in `f32`, with zero `std` or `libm`
//! dependencies). On the device the backend lowers intrinsics directly.

use crate::vec::{Vec2, Vec3, Vec4};
use core::f64::consts::{FRAC_2_PI, FRAC_PI_2, FRAC_PI_4, LN_2, LOG2_E, PI, SQRT_2};
use core::ops::{Add, Mul, Neg, Sub};

/// A vector of `f32`s.
pub trait FloatVec:
    Copy
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<f32, Output = Self>
    + Neg<Output = Self>
{
    const ZERO: Self;
    fn dot(self, o: Self) -> f32;
    fn length(self) -> f32;
    fn normalize(self) -> Self;
}

macro_rules! float_vec {
    ($($v:ident),+) => {$(
        impl FloatVec for $v {
            const ZERO: Self = $v::splat(0.0);
            #[inline]
            fn dot(self, o: $v) -> f32 {
                $v::dot(self, o)
            }
            #[inline]
            fn length(self) -> f32 {
                $v::length(self)
            }
            #[inline]
            fn normalize(self) -> $v {
                $v::normalize(self)
            }
        }
    )+};
}
float_vec!(Vec2, Vec3, Vec4);

#[inline]
pub fn dot<V: FloatVec>(a: V, b: V) -> f32 {
    a.dot(b)
}

#[inline]
pub fn length<V: FloatVec>(v: V) -> f32 {
    v.length()
}

#[inline]
pub fn normalize<V: FloatVec>(v: V) -> V {
    v.normalize()
}

/// Cross product of two 3D vectors.
#[inline]
pub const fn cross(a: Vec3, b: Vec3) -> Vec3 {
    a.cross(b)
}

/// Euclidean distance between `a` and `b`.
#[inline]
pub fn distance<V: FloatVec>(a: V, b: V) -> f32 {
    (a - b).length()
}

/// Reflection of incident vector `i` about unit surface normal `n`: `i - 2 * dot(n, i) * n`.
#[inline]
pub fn reflect<V: FloatVec>(i: V, n: V) -> V {
    i - n * (2.0 * dot(n, i))
}

/// Refraction of unit incident vector `i` through surface with unit normal `n` and index ratio
/// `eta` (Snell's law; returns zero on total internal reflection).
#[inline]
pub fn refract<V: FloatVec>(i: V, n: V, eta: f32) -> V {
    let ndoti = dot(n, i);
    let k = 1.0 - eta * eta * (1.0 - ndoti * ndoti);
    if k < 0.0 {
        V::ZERO
    } else {
        i * eta - n * (eta * ndoti + sqrt(k))
    }
}

/// Orients normal `n` to face against incident vector `i` using reference normal `nref`
/// (returns `n` if `dot(nref, i) < 0`, else `-n`, matching Slang/GLSL/WGSL).
#[inline]
pub fn faceforward<V: FloatVec>(n: V, i: V, nref: V) -> V {
    if dot(nref, i) < 0.0 { n } else { -n }
}

/// `a` to `b` by `t` (Slang's `lerp`, Metal's `mix`): `a + (b - a) * t`. `t` is a scalar, or a
/// vector like `a` for a component-wise blend (Slang's `lerp(float2, float2, float2)`).
#[inline]
pub fn lerp<T, S>(a: T, b: T, t: S) -> T
where
    T: Copy + Add<Output = T> + Sub<Output = T> + Mul<S, Output = T>,
{
    a + (b - a) * t
}

/// Absolute value of `x` (clears the sign bit).
#[inline]
pub const fn abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7fff_ffff)
}

/// Largest integer `<= x`.
#[inline]
pub fn floor(x: f32) -> f32 {
    core::f32::math::floor(x)
}

/// Smallest integer `>= x`.
#[inline]
pub fn ceil(x: f32) -> f32 {
    core::f32::math::ceil(x)
}

/// Nearest integer to `x`, rounding halfway cases away from zero.
#[inline]
pub fn round(x: f32) -> f32 {
    core::f32::math::round(x)
}

/// Integer part of `x` (rounded toward zero).
#[inline]
pub fn trunc(x: f32) -> f32 {
    core::f32::math::trunc(x)
}

/// Square root of `x`.
#[inline]
pub fn sqrt(x: f32) -> f32 {
    core::f32::math::sqrt(x)
}

/// Reciprocal square root `1.0 / sqrt(x)`.
#[inline]
pub fn rsqrt(x: f32) -> f32 {
    1.0 / sqrt(x)
}

/// Fused multiply-add: `a * b + c` with a single rounding.
#[inline]
pub fn fma(a: f32, b: f32, c: f32) -> f32 {
    core::f32::math::mul_add(a, b, c)
}

/// `x` clamped to 0 to 1.
#[inline]
pub fn saturate(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// Hermite between `e0` and `e1`: 0 at or below `e0`, 1 at or above `e1`.
#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = saturate((x - e0) / (e1 - e0));
    t * t * (3.0 - 2.0 * t)
}

/// 0 below `edge`, 1 at or above it.
#[inline]
pub fn step(edge: f32, x: f32) -> f32 {
    if x < edge { 0.0 } else { 1.0 }
}

/// The fractional part, `x - floor(x)` (Slang's `frac`, Metal's `fract`): 0 to 1, also for a
/// negative `x` (`frac(-0.25)` is 0.75).
#[inline]
pub fn frac(x: f32) -> f32 {
    x - floor(x)
}

/// Alias for [`frac`] (Metal / WGSL / GLSL spelling).
#[inline]
pub fn fract(x: f32) -> f32 {
    frac(x)
}

/// -1, 0 or 1 by the sign of `x` (Slang's `sign`): 0 for 0, -0 and NaN, where `f32::signum` gives
/// 1, -1 and NaN.
#[inline]
pub fn sign(x: f32) -> f32 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// Deterministic base-2 exponential `2^x`.
#[inline]
pub fn exp2(x: f32) -> f32 {
    exp2_f64(x as f64) as f32
}

/// Deterministic natural exponential `e^x`.
#[inline]
pub fn exp(x: f32) -> f32 {
    if x == 0.0 {
        return 1.0;
    }
    exp2_f64((x as f64) * LOG2_E) as f32
}

/// Deterministic base-2 logarithm `log2(x)`.
#[inline]
pub fn log2(x: f32) -> f32 {
    log2_f64(x as f64) as f32
}

/// Deterministic natural logarithm `ln(x)`.
#[inline]
pub fn ln(x: f32) -> f32 {
    ln_f64(x as f64) as f32
}

/// Deterministic `x` to the power `y`.
#[inline]
pub fn pow(x: f32, y: f32) -> f32 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return f32::NAN;
    }
    if x == 0.0 {
        return if y > 0.0 {
            if x.is_sign_negative() && is_odd_integer(y) { -0.0 } else { 0.0 }
        } else if x.is_sign_negative() && is_odd_integer(y) {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        };
    }
    if x < 0.0 {
        if trunc(y) != y {
            return f32::NAN;
        }
        let mag = exp2_f64((y as f64) * log2_f64((-x) as f64)) as f32;
        return if is_odd_integer(y) { -mag } else { mag };
    }
    if x.is_infinite() {
        return if y > 0.0 { f32::INFINITY } else { 0.0 };
    }
    exp2_f64((y as f64) * log2_f64(x as f64)) as f32
}

/// Deterministic sine `sin(x)` (radians).
#[inline]
pub fn sin(x: f32) -> f32 {
    sin_cos_f64(x as f64).0 as f32
}

/// Deterministic cosine `cos(x)` (radians).
#[inline]
pub fn cos(x: f32) -> f32 {
    sin_cos_f64(x as f64).1 as f32
}

/// Deterministic tangent `tan(x)` (radians).
#[inline]
pub fn tan(x: f32) -> f32 {
    let (s, c) = sin_cos_f64(x as f64);
    (s / c) as f32
}

/// Deterministic four-quadrant arctangent `atan2(y, x)` (radians in `[-pi, pi]`).
#[inline]
pub fn atan2(y: f32, x: f32) -> f32 {
    if y.is_nan() || x.is_nan() {
        return f32::NAN;
    }
    let yd = y as f64;
    let xd = x as f64;
    let y_neg = y.is_sign_negative();
    let x_neg = x.is_sign_negative();
    if y == 0.0 {
        return if x_neg {
            if y_neg { -core::f32::consts::PI } else { core::f32::consts::PI }
        } else if y_neg {
            -0.0
        } else {
            0.0
        };
    }
    if x == 0.0 {
        return if y_neg { -core::f32::consts::FRAC_PI_2 } else { core::f32::consts::FRAC_PI_2 };
    }
    if y.is_infinite() && x.is_infinite() {
        let a = if x_neg { 3.0 * FRAC_PI_4 } else { FRAC_PI_4 };
        return (if y_neg { -a } else { a }) as f32;
    }
    if y.is_infinite() {
        return if y_neg { -core::f32::consts::FRAC_PI_2 } else { core::f32::consts::FRAC_PI_2 };
    }
    if x.is_infinite() {
        return if x_neg {
            if y_neg { -core::f32::consts::PI } else { core::f32::consts::PI }
        } else if y_neg {
            -0.0
        } else {
            0.0
        };
    }
    let ay = if yd < 0.0 { -yd } else { yd };
    let ax = if xd < 0.0 { -xd } else { xd };
    let base = atan_pos_f64(ay / ax);
    let angle = if xd > 0.0 {
        if yd >= 0.0 { base } else { -base }
    } else if yd >= 0.0 {
        PI - base
    } else {
        -(PI - base)
    };
    angle as f32
}

/// The bits of `x`, as a `u32` (Slang's `asuint`): a reinterpretation, not a conversion, so bit 31
/// is the sign, set for -0 too. The device lowers it to a bitcast.
#[inline]
pub const fn asuint(x: f32) -> u32 {
    x.to_bits()
}

/// The bits of `x`, as an `i32` (Slang's `asint`).
#[inline]
pub const fn asint(x: f32) -> i32 {
    x.to_bits() as i32
}

/// Reinterprets the bits of `x` as an `f32` (Slang's `asfloat`).
#[inline]
pub const fn asfloat(x: u32) -> f32 {
    f32::from_bits(x)
}

/// `x` as an IEEE half, in the low 16 bits (Slang's `f32tof16`): rounded to the nearest half, ties
/// to even, as Metal converts; past the largest half it is infinity, and a NaN stays a NaN. The
/// device lowers it to `as_type<uint>(half2(x, 0))`.
#[inline(never)]
pub fn f32tof16(x: f32) -> u32 {
    let bits = x.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    if exp == 0xff {
        // Infinity, or a NaN kept quiet and non-zero.
        return sign | 0x7c00 | if man != 0 { 0x200 | (man >> 13) } else { 0 };
    }
    // The exponent rebiased from 127 to 15.
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        // A subnormal half, or zero: the mantissa with its implicit bit, shifted into place.
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rest = m & ((1 << shift) - 1);
        let mid = 1 << (shift - 1);
        let up = rest > mid || (rest == mid && half & 1 == 1);
        return sign | (half + up as u32);
    }
    let half = ((e as u32) << 10) | (man >> 13);
    let rest = man & 0x1fff;
    // A carry out of the mantissa steps the exponent, and out of the largest half, to infinity.
    let up = rest > 0x1000 || (rest == 0x1000 && half & 1 == 1);
    sign | (half + up as u32)
}

/// The IEEE half in the low 16 bits of `x`, as an `f32` (Slang's `f16tof32`); exact, since every
/// half is an `f32`. The bits above 16 are ignored. The device lowers it to
/// `float2(as_type<half2>(x)).x`.
#[inline(never)]
pub fn f16tof32(x: u32) -> f32 {
    let sign = (x & 0x8000) << 16;
    let exp = (x >> 10) & 0x1f;
    let man = x & 0x3ff;
    let bits = match exp {
        0 if man == 0 => sign,
        0 => {
            // A subnormal: normalized into an f32's range.
            let mut e = 127 - 15 + 1;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => sign | 0x7f80_0000 | (man << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

// ---------------------------------------------------------------------------
// Internal f64-precision deterministic IEEE-754 kernels
// ---------------------------------------------------------------------------

#[inline]
fn is_odd_integer(y: f32) -> bool {
    if !y.is_finite() || trunc(y) != y {
        return false;
    }
    // Any float >= 2^24 is even.
    let ay = abs(y);
    if ay >= 16_777_216.0 {
        return false;
    }
    ((ay as u32) & 1) == 1
}

#[inline]
fn exp2_f64(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x >= 1024.0 {
        return f64::INFINITY;
    }
    if x <= -1075.0 {
        return 0.0;
    }
    let kf = core::f64::math::round(x);
    let k = kf as i64;
    let f = x - kf;
    let z = f * LN_2;
    // Degree-11 Taylor polynomial of exp(z) on |z| <= 0.3466 (error < 6e-15).
    let poly = 1.0
        + z * (1.0
            + z * (0.5
                + z * (1.0 / 6.0
                    + z * (1.0 / 24.0
                        + z * (1.0 / 120.0
                            + z * (1.0 / 720.0
                                + z * (1.0 / 5_040.0
                                    + z * (1.0 / 40_320.0
                                        + z * (1.0 / 362_880.0
                                            + z * (1.0 / 3_628_800.0 + z * (1.0 / 39_916_800.0)))))))))));
    if k >= -1022 {
        let scale = f64::from_bits(((k + 1023) as u64) << 52);
        poly * scale
    } else {
        let s1 = f64::from_bits(1u64 << 52); // 2^-1022
        let s2 = f64::from_bits((((k + 1022) + 1023) as u64) << 52);
        (poly * s1) * s2
    }
}

#[inline]
fn ln_parts_f64(mut x: f64) -> (i64, f64) {
    let mut bits = x.to_bits();
    let mut exp = ((bits >> 52) as i64) - 1023;
    if (bits >> 52) == 0 {
        // Normalize subnormal f64.
        x *= f64::from_bits((1023 + 54) << 52);
        bits = x.to_bits();
        exp = ((bits >> 52) as i64) - 1023 - 54;
    }
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023u64 << 52));
    if m > SQRT_2 {
        m *= 0.5;
        exp += 1;
    }
    let u = (m - 1.0) / (m + 1.0);
    let u2 = u * u;
    let ln_m = 2.0
        * u
        * (1.0
            + u2 * (1.0 / 3.0
                + u2 * (1.0 / 5.0
                    + u2 * (1.0 / 7.0
                        + u2 * (1.0 / 9.0 + u2 * (1.0 / 11.0 + u2 * (1.0 / 13.0)))))));
    (exp, ln_m)
}

#[inline]
fn ln_f64(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    if x == 1.0 {
        return 0.0;
    }
    let (exp, ln_m) = ln_parts_f64(x);
    (exp as f64) * LN_2 + ln_m
}

#[inline]
fn log2_f64(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    if x == 1.0 {
        return 0.0;
    }
    let (exp, ln_m) = ln_parts_f64(x);
    (exp as f64) + ln_m * LOG2_E
}

#[inline]
fn sin_cos_f64(x: f64) -> (f64, f64) {
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    // Split PI/2 for accurate Cody-Waite range reduction.
    const PIO2_HI: f64 = 1.57079632679489655800e+00;
    const PIO2_LO: f64 = 6.12323399573676603587e-17;
    let kf = core::f64::math::round(x * FRAC_2_PI);
    let k = kf as i64;
    let r = (x - kf * PIO2_HI) - kf * PIO2_LO;
    let r2 = r * r;
    let s = r
        * (1.0
            + r2 * (-1.0 / 6.0
                + r2 * (1.0 / 120.0
                    + r2 * (-1.0 / 5_040.0
                        + r2 * (1.0 / 362_880.0
                            + r2 * (-1.0 / 39_916_800.0
                                + r2 * (1.0 / 6_227_020_800.0
                                    - r2 * (1.0 / 1_307_674_368_000.0))))))));
    let c = 1.0
        + r2 * (-0.5
            + r2 * (1.0 / 24.0
                + r2 * (-1.0 / 720.0
                    + r2 * (1.0 / 40_320.0
                        + r2 * (-1.0 / 3_628_800.0
                            + r2 * (1.0 / 479_001_600.0 - r2 * (1.0 / 87_178_291_200.0)))))));
    match k.rem_euclid(4) {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

#[inline]
fn atan_small_f64(u: f64) -> f64 {
    let u2 = u * u;
    u * (1.0
        + u2 * (-1.0 / 3.0
            + u2 * (1.0 / 5.0
                + u2 * (-1.0 / 7.0
                    + u2 * (1.0 / 9.0
                        + u2 * (-1.0 / 11.0
                            + u2 * (1.0 / 13.0
                                + u2 * (-1.0 / 15.0
                                    + u2 * (1.0 / 17.0
                                        + u2 * (-1.0 / 19.0
                                            + u2 * (1.0 / 21.0 - u2 * (1.0 / 23.0))))))))))))
}

#[inline]
fn atan_pos_f64(t: f64) -> f64 {
    const TAN_3PI_8: f64 = 2.414213562373095; // 1 + sqrt(2)
    const TAN_PI_8: f64 = 0.41421356237309503; // sqrt(2) - 1
    if t > TAN_3PI_8 {
        FRAC_PI_2 - atan_small_f64(1.0 / t)
    } else if t > TAN_PI_8 {
        FRAC_PI_4 + atan_small_f64((t - 1.0) / (t + 1.0))
    } else {
        atan_small_f64(t)
    }
}
