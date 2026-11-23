//! Revision comparison ([`diff_pictures`]) and multi-panel contact-sheet composition
//! ([`compose_side_by_side`], [`compose_grid`]).
//!
//! Purely deterministic, `#![no_std]`, zero `Global` allocations, bounded loops (rules 11, 18,
//! 25).

#![blaze::sans_io]

use blaze_own::{Alloc, Vec};

use crate::shade::quantize;
use crate::{Picture, PictureError, PictureStats};

/// Crisp border thickness in pixels drawn around each panel in [`compose_side_by_side`] and
/// [`compose_grid`], leaving 100% of every panel's own pixels untouched.
pub const PANEL_BORDER_PX: u32 = 1;

/// Safety pixel cap for composed images (matching [`crate::PictureOpts::most_pixels`]).
const MOST_COMPARE_PIXELS: u64 = 1 << 26;

/// Pixel-wise revision comparison between two same-sized [`Picture`] renders, paired with a
/// diagnostic heatmap showing where pixels changed.
#[derive(Debug)]
pub struct PictureDiff<'a> {
    /// Root-mean-square RGB channel error across all pixels, normalized to `0.0..=1.0`.
    pub rms_error: f64,
    /// Maximum single-channel absolute delta (`0..=255`) across all RGBA channels and pixels.
    pub max_channel_delta: u8,
    /// Number of pixels whose maximum RGBA channel delta strictly exceeds `threshold`.
    pub changed_pixels: u64,
    /// Total number of pixels compared (`width * height`).
    pub total_pixels: u64,
    /// Fraction of pixels changed (`changed_pixels / total_pixels`).
    pub changed_fraction: f64,
    /// Diagnostic heatmap: unchanged pixels in muted dark slate (preserving silhouette context),
    /// changed pixels ramped from amber to crimson scaled by `amplify`.
    pub heatmap: Picture<'a>,
}

/// Compare `previous` and `current` pixel by pixel, returning quantitative diff metrics and a
/// diagnostic dark-slate/amber/crimson [`PictureDiff::heatmap`].
///
/// Returns [`PictureError::DimensionMismatch`] when the two pictures differ in width or height, or
/// [`PictureError::Empty`] if either dimension is zero.
pub fn diff_pictures<'a>(
    previous: &Picture<'_>,
    current: &Picture<'_>,
    threshold: u8,
    amplify: f64,
    alloc: Alloc<'a>,
) -> Result<PictureDiff<'a>, PictureError> {
    if previous.width != current.width || previous.height != current.height {
        return Err(PictureError::DimensionMismatch {
            left: (previous.width, previous.height),
            right: (current.width, current.height),
        });
    }
    let (w, h) = (previous.width, previous.height);
    if w == 0 || h == 0 {
        return Err(PictureError::Empty { width: w, height: h });
    }
    let total_pixels = u64::from(w) * u64::from(h);
    if total_pixels > MOST_COMPARE_PIXELS {
        return Err(PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS });
    }
    let n = usize::try_from(total_pixels)
        .map_err(|_| PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS })?;
    let needed_bytes = n
        .checked_mul(4)
        .ok_or(PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS })?;
    if previous.pixels.len() < needed_bytes || current.pixels.len() < needed_bytes {
        return Err(PictureError::DimensionMismatch { left: (w, h), right: (w, h) });
    }

    let mut pixels: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(needed_bytes, alloc)?;
    let mut depth: Vec<f64, Alloc<'a>> = Vec::try_with_capacity_in(n, alloc)?;

    let amp = if amplify.is_finite() && amplify > 0.0 { amplify } else { 1.0 };
    let mut sq_sum = 0.0f64;
    let mut max_channel_delta = 0u8;
    let mut changed_pixels = 0u64;

    for i in 0..n {
        let off = i * 4;
        let a = &previous.pixels[off..off + 4];
        let b = &current.pixels[off..off + 4];

        let dr = a[0].abs_diff(b[0]);
        let dg = a[1].abs_diff(b[1]);
        let db = a[2].abs_diff(b[2]);
        let da = a[3].abs_diff(b[3]);
        let max_d = dr.max(dg).max(db).max(da);
        if max_d > max_channel_delta {
            max_channel_delta = max_d;
        }

        let fr = f64::from(dr) / 255.0;
        let fg = f64::from(dg) / 255.0;
        let fb = f64::from(db) / 255.0;
        sq_sum += (fr * fr + fg * fg + fb * fb) * (1.0 / 3.0);

        let rgba = if max_d > threshold {
            changed_pixels += 1;
            let mag = ((f64::from(max_d) / 255.0) * amp).clamp(0.0, 1.0);
            let (hr, hg, hb) = if mag < 0.5 {
                let t = mag * 2.0;
                (0.64 + (0.98 - 0.64) * t, 0.36 + (0.72 - 0.36) * t, 0.10 + (0.14 - 0.10) * t)
            } else {
                let t = (mag - 0.5) * 2.0;
                (0.98 + (0.92 - 0.98) * t, 0.72 + (0.10 - 0.72) * t, 0.14 + (0.22 - 0.14) * t)
            };
            [quantize(hr), quantize(hg), quantize(hb), 255]
        } else {
            // Muted dark slate carrying subtle luminance context from `current`.
            let luma = (0.2126 * f64::from(b[0]) + 0.7152 * f64::from(b[1]) + 0.0722 * f64::from(b[2])) / 255.0;
            [quantize(0.08 + 0.10 * luma), quantize(0.10 + 0.12 * luma), quantize(0.14 + 0.15 * luma), 255]
        };

        pixels.try_extend_from_slice(&rgba)?;
        let d = current.depth.get(i).copied().unwrap_or(f64::INFINITY);
        depth.try_push(d).map_err(|r| r.why)?;
    }

    let rms_error = blaze_math::sqrt(sq_sum / (total_pixels as f64));
    let changed_fraction = (changed_pixels as f64) / (total_pixels as f64);
    let heatmap = Picture { width: w, height: h, pixels, depth, stats: PictureStats::default() };

    Ok(PictureDiff { rms_error, max_channel_delta, changed_pixels, total_pixels, changed_fraction, heatmap })
}

/// Place `panels` horizontally left-to-right with `gap_px` dividers and crisp 1 px border frames
/// ([`PANEL_BORDER_PX`]) around each panel.
///
/// Returns [`PictureError::Empty`] if `panels` is empty or any panel has zero width/height.
pub fn compose_side_by_side<'a>(
    panels: &[&Picture<'_>],
    gap_px: u32,
    background: [u8; 4],
    alloc: Alloc<'a>,
) -> Result<Picture<'a>, PictureError> {
    if panels.is_empty() {
        return Err(PictureError::Empty { width: 0, height: 0 });
    }
    let cols = u32::try_from(panels.len()).map_err(|_| PictureError::TooManyPixels {
        pixels: panels.len() as u64,
        most: MOST_COMPARE_PIXELS,
    })?;
    compose_grid(panels, cols, gap_px, background, alloc)
}

/// Arrange `panels` in a contact-sheet grid of up to `cols` columns with `gap_px` gutters and
/// crisp 1 px border frames ([`PANEL_BORDER_PX`]) around every panel.
///
/// Each panel's interior pixels and depths are copied verbatim inside its border frame. For equal
/// `W × H` panels arranged in `C × R` cells, the output dimensions are:
/// `width = C * (W + 2 * PANEL_BORDER_PX) + (C - 1) * gap_px`,
/// `height = R * (H + 2 * PANEL_BORDER_PX) + (R - 1) * gap_px`.
pub fn compose_grid<'a>(
    panels: &[&Picture<'_>],
    cols: u32,
    gap_px: u32,
    background: [u8; 4],
    alloc: Alloc<'a>,
) -> Result<Picture<'a>, PictureError> {
    if panels.is_empty() || cols == 0 {
        return Err(PictureError::Empty { width: 0, height: 0 });
    }
    let count_u32 = u32::try_from(panels.len()).map_err(|_| PictureError::TooManyPixels {
        pixels: panels.len() as u64,
        most: MOST_COMPARE_PIXELS,
    })?;
    for p in panels {
        if p.width == 0 || p.height == 0 {
            return Err(PictureError::Empty { width: p.width, height: p.height });
        }
        let needed = usize::try_from(u64::from(p.width) * u64::from(p.height) * 4)
            .map_err(|_| PictureError::Empty { width: p.width, height: p.height })?;
        if p.pixels.len() < needed {
            return Err(PictureError::Empty { width: p.width, height: p.height });
        }
    }

    let cols_eff = cols.min(count_u32);
    let rows = count_u32.div_ceil(cols_eff);
    let border2 = PANEL_BORDER_PX * 2;

    let col_width = |c: u32| -> u32 {
        let mut max_w = 0u32;
        for r in 0..rows {
            let idx = (r as usize) * (cols_eff as usize) + (c as usize);
            if let Some(p) = panels.get(idx) {
                max_w = max_w.max(p.width);
            }
        }
        max_w
    };
    let row_height = |r: u32| -> u32 {
        let mut max_h = 0u32;
        for c in 0..cols_eff {
            let idx = (r as usize) * (cols_eff as usize) + (c as usize);
            if let Some(p) = panels.get(idx) {
                max_h = max_h.max(p.height);
            }
        }
        max_h
    };

    let mut total_w = 0u64;
    for c in 0..cols_eff {
        if c > 0 {
            total_w = total_w.saturating_add(u64::from(gap_px));
        }
        total_w = total_w.saturating_add(u64::from(col_width(c)) + u64::from(border2));
    }
    let mut total_h = 0u64;
    for r in 0..rows {
        if r > 0 {
            total_h = total_h.saturating_add(u64::from(gap_px));
        }
        total_h = total_h.saturating_add(u64::from(row_height(r)) + u64::from(border2));
    }

    let total_pixels = total_w.saturating_mul(total_h);
    if total_pixels > MOST_COMPARE_PIXELS {
        return Err(PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS });
    }
    let out_w = u32::try_from(total_w).map_err(|_| PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS })?;
    let out_h = u32::try_from(total_h).map_err(|_| PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS })?;
    let n = usize::try_from(total_pixels)
        .map_err(|_| PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS })?;
    let n_bytes = n
        .checked_mul(4)
        .ok_or(PictureError::TooManyPixels { pixels: total_pixels, most: MOST_COMPARE_PIXELS })?;

    let mut pixels: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n_bytes, alloc)?;
    let mut depth: Vec<f64, Alloc<'a>> = Vec::try_with_capacity_in(n, alloc)?;
    for _ in 0..n {
        pixels.try_extend_from_slice(&background)?;
        depth.try_push(f64::INFINITY).map_err(|r| r.why)?;
    }

    // Crisp contrasting 1 px border colour derived deterministically from background luminance.
    let bg_luma = 0.2126 * f64::from(background[0]) + 0.7152 * f64::from(background[1]) + 0.0722 * f64::from(background[2]);
    let border_rgba: [u8; 4] = if bg_luma >= 128.0 { [68, 74, 84, 255] } else { [126, 134, 148, 255] };

    let out_w_us = out_w as usize;
    let set_px = |buf: &mut [u8], x: u32, y: u32, rgba: [u8; 4]| {
        let off = ((y as usize) * out_w_us + (x as usize)) * 4;
        buf[off] = rgba[0];
        buf[off + 1] = rgba[1];
        buf[off + 2] = rgba[2];
        buf[off + 3] = rgba[3];
    };

    let mut cell_y = 0u32;
    for r in 0..rows {
        let rh = row_height(r);
        let mut cell_x = 0u32;
        for c in 0..cols_eff {
            let cw = col_width(c);
            let idx = (r as usize) * (cols_eff as usize) + (c as usize);
            if let Some(panel) = panels.get(idx) {
                let pad_x = (cw - panel.width) / 2;
                let pad_y = (rh - panel.height) / 2;
                let ix = cell_x + PANEL_BORDER_PX + pad_x;
                let iy = cell_y + PANEL_BORDER_PX + pad_y;

                // Draw crisp 1 px border frame around `[ix, ix + panel.width) × [iy, iy + panel.height)`.
                let x_left = ix - PANEL_BORDER_PX;
                let x_right = ix + panel.width;
                let y_top = iy - PANEL_BORDER_PX;
                let y_bot = iy + panel.height;
                for x in x_left..=x_right {
                    set_px(&mut pixels, x, y_top, border_rgba);
                    set_px(&mut pixels, x, y_bot, border_rgba);
                }
                for y in iy..y_bot {
                    set_px(&mut pixels, x_left, y, border_rgba);
                    set_px(&mut pixels, x_right, y, border_rgba);
                }

                // Copy panel pixels and depths verbatim.
                let pw = panel.width as usize;
                for py in 0..panel.height {
                    let src_row = (py as usize) * pw;
                    let dst_row = ((iy + py) as usize) * out_w_us + (ix as usize);
                    let src_bytes = &panel.pixels[src_row * 4..(src_row + pw) * 4];
                    pixels[dst_row * 4..(dst_row + pw) * 4].copy_from_slice(src_bytes);
                    if panel.depth.len() >= src_row + pw {
                        depth[dst_row..dst_row + pw].copy_from_slice(&panel.depth[src_row..src_row + pw]);
                    }
                }
            }
            cell_x = cell_x.saturating_add(cw).saturating_add(border2).saturating_add(gap_px);
        }
        cell_y = cell_y.saturating_add(rh).saturating_add(border2).saturating_add(gap_px);
    }

    Ok(Picture { width: out_w, height: out_h, pixels, depth, stats: PictureStats::default() })
}
