//! Filmic HDR tone mapping (AgX and ACES Fitted) and deterministic sRGB conversions.

#![blaze::sans_io]

use blaze_math::pow;

/// Tone-mapping operator applied to linear HDR radiance before sRGB display encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToneMap {
    /// Troy Sobotka's AgX filmic transform (Blender 4.x / cinema standard): smooth highlight
    /// desaturation that keeps bright speculars and emissives natural without hue twists.
    AgX,
    /// AgX with a gentle "punchy" filmic contrast curve for crisp studio & hero beauty shots.
    AgXPunchy,
    /// Narkowicz / Hill fitted ACES filmic curve.
    Aces,
    /// Unmapped linear clamp `[0, 1]` (diagnostic / albedo-check mode).
    Linear,
}

/// Deterministic sRGB byte `[0, 255]` to linear `[0, 1]`.
#[inline]
pub fn srgb_to_linear_u8(u: u8) -> f64 {
    srgb_to_linear(f64::from(u) / 255.0)
}

/// Deterministic sRGB `[0, 1]` to linear `[0, 1]`.
#[inline]
pub fn srgb_to_linear(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        pow((c + 0.055) / 1.055, 2.4)
    }
}

/// Deterministic linear `[0, 1]` to sRGB `[0, 1]`.
#[inline]
pub fn linear_to_srgb(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * pow(c, 1.0 / 2.4) - 0.055
    }
}

/// Quantize a normalized `[0, 1]` channel to `u8`, rounded to nearest-even.
#[inline]
pub fn quantize_u8(v: f64) -> u8 {
    let scaled = core::f64::math::round_ties_even(v.clamp(0.0, 1.0) * 255.0);
    scaled.clamp(0.0, 255.0) as u8
}

/// Apply exposure (`rgb * exposure`) and tone mapping, returning display-ready linear `[0, 1]`
/// (ready for `linear_to_srgb`).
pub fn apply_tonemap(rgb: [f64; 3], exposure: f64, mode: ToneMap) -> [f64; 3] {
    let e = exposure.max(0.0);
    let c = [rgb[0].max(0.0) * e, rgb[1].max(0.0) * e, rgb[2].max(0.0) * e];
    match mode {
        ToneMap::Linear => [c[0].clamp(0.0, 1.0), c[1].clamp(0.0, 1.0), c[2].clamp(0.0, 1.0)],
        ToneMap::Aces => [aces_channel(c[0]), aces_channel(c[1]), aces_channel(c[2])],
        ToneMap::AgX => agx_transform(c, false),
        ToneMap::AgXPunchy => agx_transform(c, true),
    }
}

#[inline]
fn aces_channel(x: f64) -> f64 {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    ((x * (a * x + b)) / (x * (c * x + d) + e)).clamp(0.0, 1.0)
}

/// AgX core (Troy Sobotka / Benjamin Wrensch minimal GLSL reference, exact in f64).
fn agx_transform(rgb: [f64; 3], punchy: bool) -> [f64; 3] {
    // Inset matrix (sRGB linear -> AgX working space).
    let r = 0.842479062253094 * rgb[0] + 0.0784335999999992 * rgb[1] + 0.0792237451477643 * rgb[2];
    let g = 0.0423282422610123 * rgb[0] + 0.878468636469772 * rgb[1] + 0.0791661274605434 * rgb[2];
    let b = 0.0423756549057051 * rgb[0] + 0.0784336 * rgb[1] + 0.879142973793104 * rgb[2];

    const MIN_EV: f64 = -12.47393;
    const MAX_EV: f64 = 4.026069;
    const SPAN: f64 = MAX_EV - MIN_EV;

    let encode = |v: f64| -> f64 {
        let clamped = v.max(1e-10);
        let log2_v = blaze_math::ln(clamped) * core::f64::consts::LOG2_E;
        let x = ((log2_v.clamp(MIN_EV, MAX_EV) - MIN_EV) / SPAN).clamp(0.0, 1.0);
        agx_default_contrast_approx(x)
    };
    let er = encode(r);
    let eg = encode(g);
    let eb = encode(b);

    // Outset matrix (AgX working space -> display sRGB-encoded space).
    let mut or = (1.19687900512017 * er - 0.0528968517574562 * eg - 0.0529716355144438 * eb).clamp(0.0, 1.0);
    let mut og = (-0.0980208811401368 * er + 1.15190312990417 * eg - 0.0980434501171241 * eb).clamp(0.0, 1.0);
    let mut ob = (-0.0990297440797205 * er - 0.0989611768448433 * eg + 1.15107367264116 * eb).clamp(0.0, 1.0);

    if punchy {
        // Gentle punchy look (power 1.18 + 1.12 luma saturation).
        or = pow(or, 1.18);
        og = pow(og, 1.18);
        ob = pow(ob, 1.18);
        let luma = 0.2126 * or + 0.7152 * og + 0.0722 * ob;
        let sat = 1.12;
        or = (luma + sat * (or - luma)).clamp(0.0, 1.0);
        og = (luma + sat * (og - luma)).clamp(0.0, 1.0);
        ob = (luma + sat * (ob - luma)).clamp(0.0, 1.0);
    }

    // Convert AgX's ~2.2-encoded output back to linear so downstream `linear_to_srgb` is exact!
    [pow(or, 2.2), pow(og, 2.2), pow(ob, 2.2)]
}

/// 6th-order polynomial approximation to the AgX default contrast sigmoid.
#[inline]
fn agx_default_contrast_approx(x: f64) -> f64 {
    let x2 = x * x;
    let x4 = x2 * x2;
    (15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x - 0.00232).clamp(0.0, 1.0)
}
