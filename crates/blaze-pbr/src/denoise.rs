//! Deterministic Edge-Preserving G-Buffer À-Trous Wavelet Denoiser (`blaze-pbr`).
//!
//! Designed for physically-based path-traced and GI-rendered linear HDR framebuffers:
//! removes Monte Carlo shadow, GI, and rough-reflection variance while strictly preserving
//! sharp geometric silhouettes (`depth`), crease/normal-map edges (`normal`), high-frequency
//! CC0 texture detail (`albedo`), and bright HDR emissive light sources (`emissive_preserve`).
//!
//! Pure `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`, deterministic IEEE math,
//! explicit scratch allocator — reusable by any game engine, CAD previewer, or Flecs renderer.

#![blaze::sans_io]

use blaze_math::{exp, pow};
use blaze_own::{Alloc, AllocFailure, Vec};

/// Per-pixel G-buffer guidance data accompanying a linear HDR radiance buffer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GBufferPixel {
    /// Linear base surface albedo (or environment sky colour on misses) used to preserve
    /// crisp high-frequency texture edges (wood seams, mortar joints, rivets, mullions).
    pub albedo: [f32; 3],
    /// Unit world-space shading normal (`[0, 0, 0]` for background sky pixels).
    pub normal: [f32; 3],
    /// Linear ray hit distance in metres (`f64::INFINITY` for background sky).
    pub depth: f64,
    /// `0.0` = matte/diffuse, `1.0` = glass/mirror/emissive (reduces filter blur so mirror
    /// reflections and bright lamps stay razor crisp).
    pub preserve: f32,
}

impl GBufferPixel {
    pub const SKY: GBufferPixel = GBufferPixel {
        albedo: [0.0; 3],
        normal: [0.0; 3],
        depth: f64::INFINITY,
        preserve: 1.0,
    };
}

/// Configuration for [`denoise_atrous`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DenoiseOpts {
    /// Number of À-Trous wavelet iterations (`0` disables; `3` covers 15×15 effective support).
    pub passes: u8,
    /// Relative depth edge sensitivity (smaller = stricter silhouette preservation).
    pub sigma_depth: f64,
    /// Normal dot-product sharpness exponent (`32.0..=128.0`; higher = crisper bevels/normal maps).
    pub normal_power: f64,
    /// Linear albedo edge sensitivity (smaller = preserves finer texture detail like wood grain & rivets).
    pub sigma_albedo: f64,
    /// Luminance variance edge sensitivity (prevents blurring across crisp shadow boundaries).
    pub sigma_luma: f64,
    /// Suppress isolated 1-pixel Monte Carlo firefly spikes via 3×3 neighborhood clamping before filtering.
    pub firefly_clamp: bool,
}

impl DenoiseOpts {
    pub const DEFAULT: DenoiseOpts = DenoiseOpts {
        passes: 4,
        sigma_depth: 0.035,
        normal_power: 48.0,
        sigma_albedo: 0.055,
        sigma_luma: 0.95,
        firefly_clamp: true,
    };
}

const B3_KERNEL: [f64; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

#[inline]
fn luma(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Run a deterministic multi-pass Edge-Preserving À-Trous Wavelet Denoiser over linear HDR
/// radiance `hdr` (`width × height`) guided by `gbuf`, writing the denoised HDR result into
/// `hdr` in place (using `scratch` for ping-pong buffers).
pub fn denoise_atrous(
    hdr: &mut [[f64; 3]],
    gbuf: &[GBufferPixel],
    width: u32,
    height: u32,
    opts: DenoiseOpts,
    scratch: Alloc<'_>,
) -> Result<(), AllocFailure> {
    let w = width as usize;
    let h = height as usize;
    let n = w * h;
    if opts.passes == 0 || w < 3 || h < 3 || hdr.len() != n || gbuf.len() != n {
        return Ok(());
    }

    let mut tmp: Vec<[f64; 3], Alloc<'_>> = Vec::try_with_capacity_in(n, scratch)?;
    for &c in hdr.iter() {
        tmp.try_push(c).map_err(|r| r.why)?;
    }

    // Pass 0: Optional conservative 3×3 neighborhood firefly suppressor (only clamps isolated
    // non-emissive specular spikes > 4x the local 3×3 average!).
    if opts.firefly_clamp {
        for y in 1..(h - 1) {
            for x in 1..(w - 1) {
                let idx = y * w + x;
                let gp = gbuf[idx];
                if gp.preserve > 0.75 || !gp.depth.is_finite() {
                    continue;
                }
                let cp = hdr[idx];
                let lp = luma(cp);
                if lp < 0.8 {
                    continue;
                }
                let mut sum_l = 0.0f64;
                let mut max_nb = 0.0f64;
                for dy in -1isize..=1 {
                    for dx in -1isize..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let ni = ((y as isize + dy) as usize) * w + ((x as isize + dx) as usize);
                        let nl = luma(hdr[ni]);
                        sum_l += nl;
                        max_nb = max_nb.max(nl);
                    }
                }
                let avg_l = sum_l * 0.125;
                let cap = (avg_l * 3.5).max(max_nb * 1.35).max(1.2);
                if lp > cap {
                    let s = cap / lp;
                    tmp[idx] = [cp[0] * s, cp[1] * s, cp[2] * s];
                }
            }
        }
        hdr.copy_from_slice(&tmp);
    }

    let inv_2sig_a = 1.0 / (2.0 * opts.sigma_albedo.max(1e-3) * opts.sigma_albedo.max(1e-3));
    let inv_sig_d = 1.0 / opts.sigma_depth.max(1e-4);

    // Multi-scale À-Trous passes: two `step = 1` passes suppress 1-pixel Nyquist Monte Carlo
    // grain cleanly before dyadic widening (`2, 4, 8`) smooths wide soft shadow & GI penumbras.
    const STRIDES: [isize; 5] = [1, 1, 2, 4, 8];
    let n_passes = usize::from(opts.passes).min(STRIDES.len());
    for (pass, &step) in STRIDES[..n_passes].iter().enumerate() {
        // Slightly tighten luminance sensitivity on wider passes so contact shadows stay crisp.
        let sig_l = opts.sigma_luma * (1.0 - 0.12 * (pass as f64)).max(0.20);
        let inv_sig_l = 1.0 / sig_l;

        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                let gp = gbuf[idx];
                let cp = hdr[idx];

                // Never blur background sky or mirror/emissive surfaces!
                if !gp.depth.is_finite() || gp.preserve >= 0.92 {
                    tmp[idx] = cp;
                    continue;
                }

                let lp = luma(cp);
                let luma_scale = lp.max(0.25);
                let dp = gp.depth.max(1e-3);
                let np = gp.normal;
                let ap = gp.albedo;
                let preserve_scale = (1.0 - f64::from(gp.preserve)).clamp(0.0, 1.0);

                let center_w = B3_KERNEL[2] * B3_KERNEL[2];
                let mut acc = [cp[0] * center_w, cp[1] * center_w, cp[2] * center_w];
                let mut w_sum = center_w;

                for ky in -2isize..=2 {
                    let qy = (y as isize + ky * step).clamp(0, (h - 1) as isize) as usize;
                    let wy = B3_KERNEL[(ky + 2) as usize];
                    for kx in -2isize..=2 {
                        if kx == 0 && ky == 0 {
                            continue;
                        }
                        let qx = (x as isize + kx * step).clamp(0, (w - 1) as isize) as usize;
                        let qidx = qy * w + qx;
                        let gq = gbuf[qidx];
                        if !gq.depth.is_finite() {
                            continue;
                        }

                        // 1. Depth edge stopping (relative depth jump across silhouettes & balusters).
                        let rel_d = (gp.depth - gq.depth).abs() / dp;
                        if rel_d > opts.sigma_depth * 5.0 {
                            continue;
                        }
                        let wd = exp(-rel_d * inv_sig_d);

                        // 2. Normal edge stopping (preserves bevels, 90° corners, and normal maps).
                        let ndot = f64::from(np[0] * gq.normal[0] + np[1] * gq.normal[1] + np[2] * gq.normal[2]).clamp(0.0, 1.0);
                        if ndot < 0.72 {
                            continue;
                        }
                        let wn = pow(ndot, opts.normal_power);

                        // 3. Albedo edge stopping (preserves sharp wood grain, mortar, panel seams & rivets!).
                        let da0 = f64::from(ap[0] - gq.albedo[0]);
                        let da1 = f64::from(ap[1] - gq.albedo[1]);
                        let da2 = f64::from(ap[2] - gq.albedo[2]);
                        let da2_sum = da0 * da0 + da1 * da1 + da2 * da2;
                        let wa = exp(-da2_sum * inv_2sig_a);

                        // 4. Relative luminance stopping (preserves sharp direct sun shadow edges!).
                        let cq = hdr[qidx];
                        let dl = (lp - luma(cq)).abs() / luma_scale;
                        let wl = exp(-dl * inv_sig_l);

                        let k_w = wy * B3_KERNEL[(kx + 2) as usize];
                        let w_tot = k_w * wd * wn * wa * wl * preserve_scale;
                        acc[0] += cq[0] * w_tot;
                        acc[1] += cq[1] * w_tot;
                        acc[2] += cq[2] * w_tot;
                        w_sum += w_tot;
                    }
                }

                let inv_w = 1.0 / w_sum;
                tmp[idx] = [acc[0] * inv_w, acc[1] * inv_w, acc[2] * inv_w];
            }
        }
        hdr.copy_from_slice(&tmp);
    }

    Ok(())
}
