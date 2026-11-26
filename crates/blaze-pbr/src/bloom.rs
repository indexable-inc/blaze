//! Deterministic separable HDR optical bloom and full-frame filmic post-processing (`blaze-pbr`).
//!
//! Operates on a linear HDR `[f64; 3]` framebuffer (`width × height`) in `#![no_std]` sans-IO
//! Rust with explicit scratch allocation: extracts HDR highlights above `threshold` (soft knee),
//! runs a multi-radius separable Gaussian blur (tight core + wide atmospheric halo), composites
//! back into the HDR buffer, applies subtle optional lens vignette, and finishes with AgX/ACES
//! tone mapping + sRGB RGBA8 encoding.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::tonemap::{ToneMap, apply_tonemap, linear_to_srgb, quantize_u8};

/// Optical post-processing settings applied to a linear HDR framebuffer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PostProcessOpts {
    pub tonemap: ToneMap,
    pub exposure: f64,
    /// Soft bloom luminance threshold in linear HDR units (e.g. `1.15`).
    pub bloom_threshold: f64,
    /// Bloom mix strength (`0.0` disables bloom; `0.18..=0.35` is natural photographic glow).
    pub bloom_strength: f64,
    /// Subtle edge darkening (`0.0` off; `0.12..=0.22` adds cinematic depth).
    pub vignette: f64,
}

impl PostProcessOpts {
    pub const DEFAULT: PostProcessOpts = PostProcessOpts {
        tonemap: ToneMap::AgXPunchy,
        exposure: 1.0,
        bloom_threshold: 1.15,
        bloom_strength: 0.24,
        vignette: 0.14,
    };
}

/// Apply deterministic dual-scale HDR bloom, optional subtle lens vignette, exposure, filmic
/// tone mapping, and sRGB RGBA8 quantization to `hdr` (`width × height` linear RGB pixels),
/// writing `width * height * 4` bytes into `out_rgba`.
pub fn post_process_hdr<'a>(
    hdr: &[[f64; 3]],
    width: u32,
    height: u32,
    opts: PostProcessOpts,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Vec<u8, Alloc<'a>>, AllocFailure> {
    let w = width as usize;
    let h = height as usize;
    let n = w * h;
    let mut out: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n * 4, alloc)?;

    // Optional dual-radius separable Gaussian bloom on half-res downsampled bright pass (fast + wide halo!).
    let mut bloom_full: Vec<[f64; 3], Alloc<'_>> = Vec::new_in(scratch);
    if opts.bloom_strength > 1e-4 && w >= 4 && h >= 4 {
        let bw = (w / 2).max(1);
        let bh = (h / 2).max(1);
        let bn = bw * bh;
        let mut bright: Vec<[f64; 3], Alloc<'_>> = Vec::try_with_capacity_in(bn, scratch)?;
        let thresh = opts.bloom_threshold.max(0.05);
        let knee = thresh * 0.5;

        for by in 0..bh {
            let y0 = (by * 2).min(h - 1);
            let y1 = (y0 + 1).min(h - 1);
            for bx in 0..bw {
                let x0 = (bx * 2).min(w - 1);
                let x1 = (x0 + 1).min(w - 1);
                let mut avg = [0.0; 3];
                for &(yy, xx) in &[(y0, x0), (y0, x1), (y1, x0), (y1, x1)] {
                    let c = hdr[yy * w + xx];
                    avg[0] += c[0] * 0.25;
                    avg[1] += c[1] * 0.25;
                    avg[2] += c[2] * 0.25;
                }
                let luma = 0.2126 * avg[0] + 0.7152 * avg[1] + 0.0722 * avg[2];
                // Soft quadratic knee around `thresh` prevents harsh threshold pops.
                let soft = (luma - thresh + knee).clamp(0.0, 2.0 * knee);
                let contrib = ((luma - thresh).max(0.0) + (soft * soft) / (4.0 * knee + 1e-6)) / luma.max(1e-6);
                let scale = contrib.clamp(0.0, 4.0);
                bright.try_push([avg[0] * scale, avg[1] * scale, avg[2] * scale]).map_err(|r| r.why)?;
            }
        }

        let mut tmp: Vec<[f64; 3], Alloc<'_>> = Vec::try_with_capacity_in(bn, scratch)?;
        let mut wide: Vec<[f64; 3], Alloc<'_>> = Vec::try_with_capacity_in(bn, scratch)?;
        for _ in 0..bn {
            tmp.try_push([0.0; 3]).map_err(|r| r.why)?;
            wide.try_push([0.0; 3]).map_err(|r| r.why)?;
        }

        // Pass 1: tight crisp glow (radius 3).
        blur_separable(&bright, &mut tmp, &mut wide, bw, bh, 3);
        // Copy tight into `bright`, then run wide atmospheric halo (radius 10) on `wide`!
        let mut tight = bright;
        tight.copy_from_slice(&wide);
        blur_separable(&tight, &mut tmp, &mut wide, bw, bh, 10);

        bloom_full.try_reserve(bn)?;
        for i in 0..bn {
            bloom_full
                .try_push([
                    tight[i][0] * 0.65 + wide[i][0] * 0.55,
                    tight[i][1] * 0.65 + wide[i][1] * 0.55,
                    tight[i][2] * 0.65 + wide[i][2] * 0.55,
                ])
                .map_err(|r| r.why)?;
        }

        let inv_w = 1.0 / (w as f64);
        let inv_h = 1.0 / (h as f64);
        for y in 0..h {
            let ny = (y as f64 + 0.5) * inv_h * 2.0 - 1.0;
            let by_f = ((y as f64 + 0.5) * 0.5 - 0.5).clamp(0.0, (bh - 1) as f64);
            let by0 = by_f as usize;
            let by1 = (by0 + 1).min(bh - 1);
            let ty = by_f - (by0 as f64);
            for x in 0..w {
                let nx = (x as f64 + 0.5) * inv_w * 2.0 - 1.0;
                let bx_f = ((x as f64 + 0.5) * 0.5 - 0.5).clamp(0.0, (bw - 1) as f64);
                let bx0 = bx_f as usize;
                let bx1 = (bx0 + 1).min(bw - 1);
                let tx = bx_f - (bx0 as f64);

                let b00 = bloom_full[by0 * bw + bx0];
                let b10 = bloom_full[by0 * bw + bx1];
                let b01 = bloom_full[by1 * bw + bx0];
                let b11 = bloom_full[by1 * bw + bx1];

                let mut col = hdr[y * w + x];
                for c in 0..3 {
                    let top = b00[c] + (b10[c] - b00[c]) * tx;
                    let bot = b01[c] + (b11[c] - b01[c]) * tx;
                    col[c] += (top + (bot - top) * ty) * opts.bloom_strength;
                }
                if opts.vignette > 0.0 {
                    let r2 = (nx * nx + ny * ny) * 0.5;
                    let vig = (1.0 - opts.vignette * r2).clamp(0.55, 1.0);
                    col[0] *= vig;
                    col[1] *= vig;
                    col[2] *= vig;
                }
                let mapped = apply_tonemap(col, opts.exposure, opts.tonemap);
                out.try_push(quantize_u8(linear_to_srgb(mapped[0]))).map_err(|r| r.why)?;
                out.try_push(quantize_u8(linear_to_srgb(mapped[1]))).map_err(|r| r.why)?;
                out.try_push(quantize_u8(linear_to_srgb(mapped[2]))).map_err(|r| r.why)?;
                out.try_push(255).map_err(|r| r.why)?;
            }
        }
        return Ok(out);
    }

    // No-bloom fast path.
    let inv_w = 1.0 / (w.max(1) as f64);
    let inv_h = 1.0 / (h.max(1) as f64);
    for y in 0..h {
        let ny = (y as f64 + 0.5) * inv_h * 2.0 - 1.0;
        for x in 0..w {
            let nx = (x as f64 + 0.5) * inv_w * 2.0 - 1.0;
            let mut col = hdr[y * w + x];
            if opts.vignette > 0.0 {
                let r2 = (nx * nx + ny * ny) * 0.5;
                let vig = (1.0 - opts.vignette * r2).clamp(0.55, 1.0);
                col[0] *= vig;
                col[1] *= vig;
                col[2] *= vig;
            }
            let mapped = apply_tonemap(col, opts.exposure, opts.tonemap);
            out.try_push(quantize_u8(linear_to_srgb(mapped[0]))).map_err(|r| r.why)?;
            out.try_push(quantize_u8(linear_to_srgb(mapped[1]))).map_err(|r| r.why)?;
            out.try_push(quantize_u8(linear_to_srgb(mapped[2]))).map_err(|r| r.why)?;
            out.try_push(255).map_err(|r| r.why)?;
        }
    }
    Ok(out)
}

/// Fast deterministic 2-pass separable Gaussian-like blur of radius `r`.
fn blur_separable(src: &[[f64; 3]], tmp: &mut [[f64; 3]], dst: &mut [[f64; 3]], w: usize, h: usize, radius: usize) {
    let r = radius.clamp(1, 16) as isize;
    let sigma = (r as f64) * 0.48;
    let inv_2s2 = 1.0 / (2.0 * sigma * sigma);
    let mut weights = [0.0f64; 33];
    let mut wsum = 0.0f64;
    for k in -r..=r {
        let wt = blaze_math::exp(-((k * k) as f64) * inv_2s2);
        weights[(k + r) as usize] = wt;
        wsum += wt;
    }
    for wt in &mut weights[..=((2 * r) as usize)] {
        *wt /= wsum;
    }

    // Horizontal pass.
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let mut acc = [0.0; 3];
            for k in -r..=r {
                let sx = (x as isize + k).clamp(0, (w - 1) as isize) as usize;
                let wt = weights[(k + r) as usize];
                let p = src[row + sx];
                acc[0] += p[0] * wt;
                acc[1] += p[1] * wt;
                acc[2] += p[2] * wt;
            }
            tmp[row + x] = acc;
        }
    }

    // Vertical pass.
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0; 3];
            for k in -r..=r {
                let sy = (y as isize + k).clamp(0, (h - 1) as isize) as usize;
                let wt = weights[(k + r) as usize];
                let p = tmp[sy * w + x];
                acc[0] += p[0] * wt;
                acc[1] += p[1] * wt;
                acc[2] += p[2] * wt;
            }
            dst[y * w + x] = acc;
        }
    }
}
