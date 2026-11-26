//! Pure-`core`, deterministic floating-point math for `#![no_std]` BM25 and cosine scoring.
//!
//! Avoids platform `libm` differences so scores and rankings are bit-for-bit deterministic
//! across macOS arm64, Linux arm64, x86_64, and wasm (Rule 25).

/// Natural logarithm of 2 (IEEE-754 double).
const LN_2: f64 = core::f64::consts::LN_2;
/// `sqrt(0.5)` for symmetric range reduction into `[sqrt(0.5), sqrt(2)]`.
const FRAC_1_SQRT_2: f64 = core::f64::consts::FRAC_1_SQRT_2;

/// Deterministic square root in pure `core` via IEEE-754 exponent halving + Newton-Raphson.
pub(crate) fn sqrt_f64(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 || x.is_infinite() {
        return x;
    }
    // Scale subnormals up by 2^54 so exponent extraction is exact.
    let (scaled, scale_back) = if x < f64::MIN_POSITIVE {
        (x * (1u64 << 54) as f64, 1.0 / (1u64 << 27) as f64)
    } else {
        (x, 1.0)
    };
    let bits = scaled.to_bits();
    let exp = ((bits >> 52) as i64) - 1023;
    let half_exp = exp >> 1;
    let guess_bits = (((half_exp + 1023) as u64) << 52) | ((bits & 0x000F_FFFF_FFFF_FFFF) >> 1);
    let mut y = f64::from_bits(guess_bits);
    // Six Newton-Raphson iterations achieve full 53-bit IEEE-754 precision.
    let mut i = 0;
    while i < 6 {
        y = 0.5 * (y + scaled / y);
        i += 1;
    }
    y * scale_back
}

/// Deterministic natural logarithm in pure `core` via range reduction and `atanh` series.
///
/// Reduces `x = m * 2^e` with `m in [sqrt(0.5), sqrt(2)]`, then computes
/// `ln(m) = 2 * (t + t^3/3 + t^5/5 + ... + t^15/15)` where `t = (m - 1) / (m + 1)`.
/// Since `|t| <= 0.1716`, `t^15 / 15 < 2.2e-13` and `t^17 / 17 < 6e-15` (full double accuracy).
pub(crate) fn ln_f64(x: f64) -> f64 {
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

    let (norm, exp_bias): (f64, i64) = if x < f64::MIN_POSITIVE {
        (x * (1u64 << 54) as f64, -54)
    } else {
        (x, 0)
    };
    let bits = norm.to_bits();
    let mut exp = ((bits >> 52) as i64) - 1023 + exp_bias;
    // Extract mantissa in [1.0, 2.0).
    let mut m = f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
    // Shift into [sqrt(0.5), sqrt(2)] to minimize |t|.
    if m > core::f64::consts::SQRT_2 {
        m *= 0.5;
        exp += 1;
    } else if m < FRAC_1_SQRT_2 {
        m *= 2.0;
        exp -= 1;
    }

    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = t;
    let mut k = 3u32;
    while k <= 19 {
        term *= t2;
        sum += term / f64::from(k);
        k += 2;
    }

    2.0 * sum + (exp as f64) * LN_2
}

/// Rounds a non-negative finite score to 3 decimal places for compact tabular rendering.
pub(crate) fn round3(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let scaled = x * 1000.0;
    #[expect(clippy::cast_possible_truncation, reason = "scores are small bounded floats")]
    let rounded = if scaled >= 0.0 {
        (scaled + 0.5) as i64
    } else {
        (scaled - 0.5) as i64
    };
    (rounded as f64) / 1000.0
}
