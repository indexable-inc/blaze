//! DETERMINISTIC FLOATING POINT: the same bits on every machine, for every lane count (rule 25,
//! D183). A platform's libm differs from another's in the last bits of `sin` or `exp`, and a
//! CAD kernel whose keys and meshes are records cannot have that: two builders of one key must
//! write equal bytes. So this crate computes every function itself, from IEEE 754's basic
//! operations alone, in a fixed order.
//!
//! # The functions, their methods and their bounds
//!
//! Each is written once over [`Real`] (in `fns`), so `sin::<f64>` is the scalar twin and
//! `sin::<Simd<f64, N>>` the vector form for any `N`, equal lane for lane bit for bit (tested).
//! The f32 forms ([`sinf`], [`sinf_lanes`], ...) run the f64 method on the widened argument
//! and round once (module `single`). Bounds are in ulp of the result, for the whole domain
//! unless said; they are fdlibm's stated bounds for its methods, which these follow constant
//! for constant.
//!
//! | function | method | bound |
//! |---|---|---|
//! | [`sqrt`] | IEEE `squareRoot` (`core::f64::math::sqrt`, one instruction) | exact (0.5) |
//! | [`floor`] [`ceil`] [`trunc`] [`round`] [`round_ties_even`] | bit masks on the exponent | exact |
//! | [`abs`] [`copysign`] | the sign bit | exact |
//! | [`mul_add`] | IEEE `fusedMultiplyAdd`, only where called by name: no expression here fuses implicitly | exact (0.5) |
//! | [`sin`] [`cos`] [`sin_cos`] | reduction by pi/2: below [`TRIG_MEDIUM_BELOW`] Cody-Waite with pi/2 in three 33-bit parts and a tail, every rounding error kept by two-sums; above, Payne-Hanek in integers against 1344 bits of 2/pi derived from Machin's formula; degree-13/14 minimax kernels (fdlibm `__sin`, `__cos`) | < 1 |
//! | [`tan`] | `sin/cos` of the same reduction | < 2.5 |
//! | [`atan`] | reduction around 0, 1/2, 1, 3/2, inf by the addition formula, degree-22 odd minimax (fdlibm `s_atan`) | < 1 |
//! | [`atan2`] | fdlibm `e_atan2`'s sign and special-value analysis over [`atan`], pi in two parts | < 2 |
//! | [`asin`] [`acos`] | fdlibm `e_asin`/`e_acos`: a rational minimax `R(z)` with the half-angle formula above 0.5 and the root split to carry its error | < 1 |
//! | [`exp`] | `x = k ln2 + r`, ln 2 in two parts, Remez rational `1 + r + rc/(2-c)` (fdlibm `e_exp`) | < 1 |
//! | [`exp2`] | `x = k + r` exactly, `r ln2` as an exact two-part product (fma), the same kernel | < 1 |
//! | [`ln`] | `x = 2^k (1+f)`, `s = f/(2+f)`, degree-14 minimax in `s` (fdlibm `e_log`) | < 1 |
//! | [`log2`] | [`ln`]'s parts with `f - f^2/2` split for an exact product by `1/ln2`'s high part (musl `log2`) | < 1 |
//! | [`pow`] | `e^(y ln|x|)` with the logarithm and the product each a double-double (two-sums, fma), IEEE's special cases | < 1 + `|y ln x|` (budget: 1 + `|y ln x|`/8) |
//! | [`hypot`] | power-of-two scaling, squares and their sum as exact two-part sums, one Newton step | < 1 |
//! | [`cbrt`] | Kahan's exponent/3 guess, a polynomial to 23 bits, one Newton step (musl `s_cbrt`) | < 0.667 |
//! | [`sin_cos_turns`] | exact integer reduction of a whole fraction of a turn | exact at multiples of 1/4, 1/8 (correctly rounded), 1/12 turn; < 2 otherwise |
//!
//! # What `core` gives without `std`
//!
//! `core::f64::math` (feature `core_float_math`) has `floor`, `ceil`, `round`, `trunc`,
//! `mul_add` and `sqrt` as intrinsics (exact IEEE operations, deterministic), and `cbrt` and
//! `abs_sub` as calls into the platform's C libm (NOT deterministic: never used here). `abs`
//! and `copysign` are inherent on `f64`. `core::simd` has no square root, rounding or
//! fused multiply-add without `std`'s `std_float`, so the vector forms take `sqrt` and `fma`
//! lane by lane from `core::f64::math` (still the one IEEE operation each) and do the rounding
//! functions by bit masks, as the scalar form does. Nothing in the crate is `unsafe`.
//!
//! # Geometry and intervals
//!
//! [`Vec2`], [`Vec3`], [`Mat3`], [`Affine3`] (its inverse kept beside it, and a [`Metric`]
//! saying whether it keeps distances), [`Aabb3`] (with the box of a mapped box, per local
//! axis), and [`Interval`]: outward rounded by one ulp per basic operation without a rounding
//! mode change (see `interval` for why that and not error-free transformations).
//!
//! Quantities (lengths, angles, masses, telemetry) are blaze-units (D184); nothing here depends
//! on them.

#![no_std]
#![feature(register_tool, custom_inner_attributes, portable_simd, core_float_math)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod fns;
mod geom;
mod interval;
mod real;
mod single;
mod turns;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod precise;

pub use fns::{
    TRIG_MEDIUM_BELOW, abs, acos, asin, atan, atan2, cbrt, ceil, copysign, cos, exp, exp2, floor, hypot, ln, log2, mul_add, pow, round,
    round_ties_even, sin, sin_cos, sqrt, tan, trunc,
};
pub use geom::{Aabb3, Affine3, Mat3, Metric, Singular, Vec2, Vec3};
pub use interval::{Interval, IntervalError, IntervalPlant};
pub use real::Real;
pub use single::*;
pub use turns::{NotQuarterable, TurnUnit, sin_cos_turns};

/// The least f64 above `x` (`inf` stays, NaN stays, `-0` and `+0` both go to the least
/// positive subnormal).
#[inline]
pub fn next_up(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let b = x.to_bits();
    if x > 0.0 { f64::from_bits(b + 1) } else { f64::from_bits(b - 1) }
}

/// The greatest f64 below `x` (mirror of [`next_up`]).
#[inline]
pub fn next_down(x: f64) -> f64 {
    -next_up(-x)
}

/// The distance in ulp between two finite f64 of one sign or zero, by their ordered bits: for
/// the tests and the harness against `std`.
pub fn ulp_distance(a: f64, b: f64) -> u64 {
    let ord = |x: f64| {
        let b = x.to_bits();
        if b >> 63 == 1 { !b } else { b | (1 << 63) }
    };
    if a.is_nan() && b.is_nan() {
        return 0;
    }
    if a.is_nan() || b.is_nan() {
        return u64::MAX;
    }
    let (x, y) = (ord(a), ord(b));
    x.abs_diff(y)
}
