//! Slang's free functions, under Slang's names, so a port reads line for line. Each is the same
//! function as Slang's (and Metal's: `lerp` is `mix`), written as plain Rust: on the host it runs
//! as written, on the device the backend lowers what it calls (`f32::powf` is `powf32`, and so on).
use crate::vec::{Vec2, Vec3, Vec4};
use core::ops::{Add, Mul, Sub};

/// A vector of `f32`s.
pub trait FloatVec: Copy {
    fn dot(self, o: Self) -> f32;
    fn length(self) -> f32;
    fn normalize(self) -> Self;
}

macro_rules! float_vec {
    ($($v:ident),+) => {$(
        impl FloatVec for $v {
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

/// `a` to `b` by `t` (Slang's `lerp`, Metal's `mix`): `a + (b - a) * t`. `t` is a scalar, or a
/// vector like `a` for a component-wise blend (Slang's `lerp(float2, float2, float2)`).
#[inline]
pub fn lerp<T, S>(a: T, b: T, t: S) -> T
where
    T: Copy + Add<Output = T> + Sub<Output = T> + Mul<S, Output = T>,
{
    a + (b - a) * t
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

/// `x` to the power `y`.
#[inline]
pub fn pow(x: f32, y: f32) -> f32 {
    x.powf(y)
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
    x - x.floor()
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

/// The bits of `x`, as a `u32` (Slang's `asuint`): a reinterpretation, not a conversion, so bit 31
/// is the sign, set for -0 too. The device lowers it to a bitcast.
#[inline]
pub fn asuint(x: f32) -> u32 {
    x.to_bits()
}
