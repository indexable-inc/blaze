//! The f32 functions: each is its f64 function on the exactly widened argument, rounded once to
//! f32. The f64 result is within its stated bound (at most a few f64 ulp, that is under 2^-27
//! f32 ulp), so the f32 result is the correctly rounded one except when the true value lies
//! within that distance of a halfway point between two f32 values: under 0.5 + 2^-27 ulp. This
//! is musl's own method for most of its f32 functions. The square root and the rounding
//! functions are exact this way (f64 has more than twice f32's precision plus two bits).
//!
//! The vector forms widen with `cast`, run the f64 lane form, and narrow with `cast`: the same
//! two IEEE conversions per lane as the scalar form, so the same bits.

use core::simd::{Simd, num::SimdFloat};

use crate::fns;

macro_rules! unary_f32 {
    ($($name:ident => $f64:ident, $lanes:ident;)*) => {$(
        #[doc = concat!("`", stringify!($f64), "` on f32 (see the module).")]
        #[inline]
        pub fn $name(x: f32) -> f32 {
            // The one rounding to f32 is the method.
            fns::$f64::<f64>(f64::from(x)) as f32
        }

        #[doc = concat!("`", stringify!($name), "` on each lane, the same bits as the scalar form.")]
        #[inline]
        pub fn $lanes<const N: usize>(x: Simd<f32, N>) -> Simd<f32, N> {
            fns::$f64::<Simd<f64, N>>(x.cast::<f64>()).cast::<f32>()
        }
    )*};
}

macro_rules! binary_f32 {
    ($($name:ident => $f64:ident, $lanes:ident;)*) => {$(
        #[doc = concat!("`", stringify!($f64), "` on f32 (see the module).")]
        #[inline]
        pub fn $name(a: f32, b: f32) -> f32 {
            fns::$f64::<f64>(f64::from(a), f64::from(b)) as f32
        }

        #[doc = concat!("`", stringify!($name), "` on each lane, the same bits as the scalar form.")]
        #[inline]
        pub fn $lanes<const N: usize>(a: Simd<f32, N>, b: Simd<f32, N>) -> Simd<f32, N> {
            fns::$f64::<Simd<f64, N>>(a.cast::<f64>(), b.cast::<f64>()).cast::<f32>()
        }
    )*};
}

unary_f32! {
    sqrtf => sqrt, sqrtf_lanes;
    floorf => floor, floorf_lanes;
    ceilf => ceil, ceilf_lanes;
    roundf => round, roundf_lanes;
    truncf => trunc, truncf_lanes;
    absf => abs, absf_lanes;
    sinf => sin, sinf_lanes;
    cosf => cos, cosf_lanes;
    tanf => tan, tanf_lanes;
    asinf => asin, asinf_lanes;
    acosf => acos, acosf_lanes;
    atanf => atan, atanf_lanes;
    expf => exp, expf_lanes;
    exp2f => exp2, exp2f_lanes;
    lnf => ln, lnf_lanes;
    log2f => log2, log2f_lanes;
    cbrtf => cbrt, cbrtf_lanes;
}

binary_f32! {
    atan2f => atan2, atan2f_lanes;
    powf => pow, powf_lanes;
    hypotf => hypot, hypotf_lanes;
    copysignf => copysign, copysignf_lanes;
}

/// `x * a + b` rounded once, in f32: IEEE 754's f32 `fusedMultiplyAdd`, only where a caller asks
/// for it by this name.
#[inline]
pub fn mul_addf(x: f32, a: f32, b: f32) -> f32 {
    core::f32::math::mul_add(x, a, b)
}

/// `(sin x, cos x)` on f32.
#[inline]
pub fn sin_cosf(x: f32) -> (f32, f32) {
    let (s, c) = fns::sin_cos::<f64>(f64::from(x));
    (s as f32, c as f32)
}
