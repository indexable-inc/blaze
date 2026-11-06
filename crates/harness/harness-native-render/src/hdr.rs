//! HDR on macOS's EDR: one unit, one headroom, one tone-map, shared by every picture the renderer draws.
//!
//! The unit. The window's target (gpu.rs `PIXEL_FORMAT`, the layer's `kCGColorSpaceExtendedSRGB`) holds sRGB-encoded half
//! floats, and 1.0 is SDR reference white: every UI colour and glyph is drawn as it always was, so SDR content is bit for
//! bit what it was before EDR. Light brighter than reference white is encoded past 1 on sRGB's curve extended (the
//! shader's `srgb_encode_ext`), and in linear light it is a multiple of reference white. HDR sources are brought into that
//! linear scale with reference white at [`WHITE_NITS`] (ITU-R BT.2408, as ImageIO decodes PQ and HLG stills and as
//! harness-video's HDR10 master places white): 203 nits is 1.0, a 1000-nit highlight 4.93.
//!
//! The headroom. [`Headroom`] is what the display can show now, read from the window's NSScreen
//! (harness-native's window.rs) and carried into every frame as `Frame::textfx[0]` (pack.rs; [`Frame::headroom`]): the most
//! linear light the display can show over reference white this instant (1 on an SDR display, more on an XDR as brightness
//! allows; it moves with the brightness slider and ambient light). Everything that draws past white reads it from the frame,
//! the one home: HDR stills and videos tone-map to it ([`tone_map`], ported in harness.slang), the blaze mark's fire lifts
//! up to it, a Shader's prelude may read it.
//!
//! The curve. [`tone_map`] leaves light up to a knee untouched (all of SDR once the display has a third of a stop to spare)
//! and rolls what is above it off smoothly into the headroom, so a picture's own peak lands exactly on the display's and
//! nothing clips; a picture no brighter than the display passes through unchanged.
use crate::pack::Frame;

/// Nits of SDR reference white in HDR content (ITU-R BT.2408): what decodes to 1.0.
pub const WHITE_NITS: f32 = 203.0;
/// HLG's nominal display peak (ITU-R BT.2100's reference, Lw): an HLG signal of 1.0 shows at this many nits.
pub const HLG_PEAK_NITS: f32 = 1000.0;
/// Where the roll-off starts, as a share of the headroom (capped at reference white): on a display with 4/3 or more,
/// everything up to SDR white passes untouched; on an SDR display the top quarter compresses to make room for highlights.
pub const KNEE: f32 = 0.75;

/// The display's EDR headroom, in linear multiples of SDR reference white (NSScreen's
/// `maximum{,Potential,Reference}ExtendedDynamicRangeColorComponentValue`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Headroom {
    /// What it can show now (1 until an EDR layer is on screen; it moves with brightness).
    pub current: f32,
    /// The most it could show at all (16 on a MacBook Pro's XDR, 1 on an SDR display): whether EDR is worth asking for.
    pub potential: f32,
    /// In a reference mode (an XDR preset), the headroom content is graded for; 0 when not in one.
    pub reference: f32,
}

impl Headroom {
    /// An SDR display, or a target with no room past white (the offscreen shot's default, so its pixels are what they were).
    pub const SDR: Self = Self { current: 1.0, potential: 1.0, reference: 0.0 };

    /// The headroom pictures map to this frame: the current one, never below reference white (a reading of 0 or NaN is SDR).
    #[must_use]
    pub fn frame(&self) -> f32 {
        if self.current.is_finite() { self.current.max(1.0) } else { 1.0 }
    }
}

impl Default for Headroom {
    fn default() -> Self {
        Self::SDR
    }
}

impl Frame {
    /// The display's headroom this frame (linear, 1 for SDR): `textfx[0]`, where every pass reads it (harness.slang
    /// `frame_headroom`).
    #[must_use]
    pub fn headroom(&self) -> f32 {
        if self.textfx[0].is_finite() { self.textfx[0].max(1.0) } else { 1.0 }
    }
}

/// Where the roll-off starts for a display of `headroom` (linear): [`KNEE`] of it, at most reference white.
#[must_use]
pub fn knee(headroom: f32) -> f32 {
    (KNEE * headroom.max(1.0)).min(1.0)
}

/// Linear light `x` (a multiple of reference white) of a picture whose brightest is `peak`, shown on a display of
/// `headroom`: unchanged up to [`knee`], then rolled off so `peak` lands on `headroom` (with a `peak` of 0 or not finite,
/// unknown, the roll-off only approaches it). Continuous with slope 1 at the knee, never decreasing, never past the
/// headroom. A picture no brighter than the display is only clipped at its headroom. Negative and NaN light is 0.
/// Ported as-is in harness.slang (`tone_map`).
#[must_use]
pub fn tone_map(x: f32, peak: f32, headroom: f32) -> f32 {
    let h = headroom.max(1.0);
    if x.is_nan() || x <= 0.0 {
        return 0.0;
    }
    let known = peak.is_finite() && peak > 0.0;
    if known && peak <= h {
        return x.min(h);
    }
    let k = knee(h);
    if x <= k {
        return x;
    }
    // Over the knee, a = x - k goes to k + a / (1 + a c): slope 1 at a = 0, and with c = 1/(h - k) - 1/(peak - k) the
    // peak lands on h exactly (c = 1/(h - k) for an unknown peak: the curve tends to h).
    let span = h - k;
    let c = if known { 1.0 / span - 1.0 / (peak - k) } else { 1.0 / span };
    let a = x - k;
    if a.is_infinite() {
        return h;
    }
    (k + a / (1.0 + a * c)).min(h)
}

/// A colour in linear light tone-mapped by its brightest channel, so its hue and saturation hold as it rolls off.
#[must_use]
pub fn tone_map_rgb(rgb: [f32; 3], peak: f32, headroom: f32) -> [f32; 3] {
    let m = rgb[0].max(rgb[1]).max(rgb[2]);
    if !(m > 0.0) {
        return [0.0; 3];
    }
    let k = tone_map(m, peak, headroom) / m;
    rgb.map(|c| c * k)
}

/// How a picture's texels encode light (`SurfaceInst::hdr[0]`, read by harness.slang's `surface_fs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transfer {
    /// sRGB-encoded, as the UI's colours: a shader's picture, an SDR video (Rec. 709 is close enough to sRGB to show as
    /// it always has). Drawn as it is.
    #[default]
    Srgb = 0,
    /// SMPTE ST 2084 (PQ, HDR10, Dolby Vision's base): absolute nits.
    Pq = 1,
    /// ITU-R BT.2100 HLG: relative scene light, shown on a [`HLG_PEAK_NITS`] display.
    Hlg = 2,
}

/// A PQ signal (0..1) as linear light over reference white: 10000 nits is 49.26.
#[must_use]
pub fn pq_linear(e: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.843_75;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.687_5;
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    let nits = 10000.0 * ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1);
    nits / WHITE_NITS
}

/// HLG's inverse OETF: a signal (0..1) as relative scene light (0..1).
#[must_use]
pub fn hlg_scene(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let e = e.clamp(0.0, 1.0);
    if e <= 0.5 { e * e / 3.0 } else { (((e - C) / A).exp() + B) / 12.0 }
}

/// An HLG colour (BT.2020 primaries, non-linear signal) as linear light over reference white, through BT.2100's OOTF for
/// a [`HLG_PEAK_NITS`] display (system gamma 1.2): a signal of 1.0 on every channel is 1000 nits, 4.93.
#[must_use]
pub fn hlg_linear(rgb: [f32; 3]) -> [f32; 3] {
    let s = rgb.map(hlg_scene);
    let y = 0.2627 * s[0] + 0.6780 * s[1] + 0.0593 * s[2];
    let gain = HLG_PEAK_NITS * y.max(0.0).powf(0.2) / WHITE_NITS;
    s.map(|c| c * gain)
}

/// Linear BT.2020 to linear BT.709 / sRGB primaries (ITU-R BT.2087); out-of-gamut colours go negative, which the extended
/// target keeps.
#[must_use]
pub fn bt2020_to_709(c: [f32; 3]) -> [f32; 3] {
    [
        1.660_491 * c[0] - 0.587_641_1 * c[1] - 0.072_849_9 * c[2],
        -0.124_550_5 * c[0] + 1.132_9 * c[1] - 0.008_349_4 * c[2],
        -0.018_150_8 * c[0] - 0.100_578_9 * c[1] + 1.118_729_7 * c[2],
    ]
}

/// sRGB's encoding extended past 1 (mirrored for negatives): linear light to what the target stores.
#[must_use]
pub fn srgb_encode_ext(l: f32) -> f32 {
    let a = l.abs();
    let c = if a <= 0.003_130_8 { a * 12.92 } else { 1.055 * a.powf(1.0 / 2.4) - 0.055 };
    c.copysign(l)
}

/// sRGB's decoding extended past 1: what the target stores to linear light.
#[must_use]
pub fn srgb_decode_ext(c: f32) -> f32 {
    let a = c.abs();
    let l = if a <= 0.040_45 { a / 12.92 } else { ((a + 0.055) / 1.055).powf(2.4) };
    l.copysign(c)
}

/// What a frame's pixel value is when it shows `headroom` (linear) of light: the most a pass may write, encoded.
#[must_use]
pub fn encoded_ceiling(headroom: f32) -> f32 {
    srgb_encode_ext(headroom.max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arb(tc: &hegel::TestCase, lo: f32, hi: f32) -> f32 {
        use hegel::generators as gs;
        tc.draw(gs::floats::<f32>().min_value(lo).max_value(hi).allow_nan(false).allow_infinity(false))
    }

    /// The curve's contract over any light, peak and display: never decreasing, untouched to the knee, never past the
    /// headroom, the peak exactly on it, and pictures within the display's range left alone.
    #[hegel::test]
    fn the_tone_map_rolls_off_into_the_headroom_and_keeps_sdr(tc: hegel::TestCase) {
        let h = arb(&tc, 1.0, 64.0);
        let peak = arb(&tc, 0.0, 200.0);
        let x = arb(&tc, 0.0, 400.0);
        let dx = arb(&tc, 0.0, 50.0);
        let (y, y2) = (tone_map(x, peak, h), tone_map(x + dx, peak, h));
        assert!(y2 + 1e-5 * y2.max(1.0) >= y, "decreasing: f({x})={y} > f({})={y2} (peak {peak}, headroom {h})", x + dx);
        assert!(y <= h * (1.0 + 1e-6), "f({x})={y} past the headroom {h} (peak {peak})");
        if x <= knee(h) {
            assert_eq!(y, x, "under the knee {} f({x}) moved (peak {peak}, headroom {h})", knee(h));
        }
        if peak > 0.0 && peak <= h {
            assert_eq!(y, x.min(h), "a picture within the display (peak {peak} <= {h}) was changed at {x}");
        }
        if peak > h {
            let at = tone_map(peak, peak, h);
            assert!((at - h).abs() <= 1e-4 * h, "the peak {peak} lands at {at}, not the headroom {h}");
        }
        // SDR stays SDR wherever the display has a third of a stop to spare.
        if h >= 1.0 / KNEE && x <= 1.0 {
            assert_eq!(y, x, "reference white or below moved at {x} on a {h} display");
        }
    }

    /// The decoders agree with what ImageIO measured on this stack: PQ 203 nits is 1.0, 1000 nits 4.93; HLG's full signal
    /// is the 1000-nit peak.
    #[test]
    fn hdr_signals_decode_with_white_at_203_nits() {
        let pq = |nits: f32| {
            let y = (nits / 10000.0).powf(0.159_301_76);
            ((0.835_937_5 + 18.851_563 * y) / (1.0 + 18.687_5 * y)).powf(78.843_75)
        };
        assert!((pq_linear(pq(203.0)) - 1.0).abs() < 1e-3, "{}", pq_linear(pq(203.0)));
        assert!((pq_linear(pq(1000.0)) - 4.926).abs() < 5e-3, "{}", pq_linear(pq(1000.0)));
        let w = hlg_linear([1.0; 3]);
        assert!(w.iter().all(|c| (c - 4.926).abs() < 5e-3), "{w:?}");
        let white = bt2020_to_709([1.0; 3]);
        assert!(white.iter().all(|c| (c - 1.0).abs() < 1e-3), "{white:?}");
        assert!((srgb_decode_ext(srgb_encode_ext(4.0)) - 4.0).abs() < 1e-4);
    }
}
