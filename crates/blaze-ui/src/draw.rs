//! Zero-copy `#![no_std]` WebGPU Draw List & Berkeley Mono OpenType Ligature text shaper.
//!
//! Produces flat 16-byte-aligned GPU buffers ready for single-call `queue.writeBuffer`:
//! - `clips: Vec<ClipRectGpu, A>` (rounded-rect fragment clip table — zero scissor breaks!)
//! - `rects` / `overlay_rects`: `Vec<RectInst, A>` (80B SDF rounded rects + borders + lines + circles + shadows)
//! - `glyphs` / `overlay_glyphs`: `Vec<GlyphInst, A>` (64B instanced HiDPI atlas glyphs)

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use crate::components::GpuSlot;
use crate::error::UiError;
use crate::shaders::{ClipRectGpu, GlyphInst, RectInst, ViewportUniform, prim_kind};
use crate::types::{Rect, Rgba};

/// Contiguous instance patch range (`[start .. start + count)`) for partial WebGPU `writeBuffer`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuPatch {
    pub start: u32,
    pub count: u32,
}

const _: () = {
    assert!(core::mem::size_of::<GpuPatch>() == 8);
};

/// Off-screen hardware-clipped `RectInst` sentinel used to pad retained slab slack with 0 fragments.
#[inline]
pub const fn culled_rect_inst() -> RectInst {
    RectInst {
        rect: [-1000.0, -1000.0, -1000.0, -1000.0],
        color: [0.0, 0.0, 0.0, 0.0],
        border_color: [0.0, 0.0, 0.0, 0.0],
        params: [0.0, 0.0, 0.0, 0.0],
        clip_info: [0, 0, 0, 0],
    }
}

/// Off-screen hardware-clipped `GlyphInst` sentinel used to pad retained slab slack with 0 fragments.
#[inline]
pub const fn culled_glyph_inst() -> GlyphInst {
    GlyphInst {
        rect: [-1000.0, -1000.0, -1000.0, -1000.0],
        uv: [0.0, 0.0, 0.0, 0.0],
        color: [0.0, 0.0, 0.0, 0.0],
        clip_info: [0, 0, 0, 0],
    }
}

/// Atlas grid dimensions matching the WebGPU Berkeley Mono HiDPI font atlas (16×8 cells).
pub const ATLAS_COLS: f32 = 16.0;
pub const ATLAS_ROWS: f32 = 8.0;
pub const ATLAS_FIRST_CHAR: u8 = 32;
pub const ATLAS_LAST_CHAR: u8 = 126;

/// Double-width OpenType ligatures baked into Row 7 of the Berkeley Mono atlas.
const LIGATURES: [(&[u8; 2], u8); 8] = [
    (b"->", 0),
    (b"=>", 1),
    (b"::", 2),
    (b"!=", 3),
    (b"==", 4),
    (b"<=", 5),
    (b">=", 6),
    (b"..", 7),
];

/// Crisp symbols baked into Row 6 of the Berkeley Mono atlas.
const ROW6_SYMBOLS: [char; 16] = [
    '·', '×', 'µ', '→', 'Δ', '…', '—', '↵', '↗', '←', '▸', '▾', '●', '○', '◆', '⤢',
];

#[inline]
fn u8_to_f32(v: u8) -> f32 {
    f32::from(v)
}

#[inline]
fn usize_to_u32_sat(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Draw list frame buffers for base UI (`layer 0`) and floating overlays (`layer 1`), supporting
/// both full-frame rebuilds and $O(\Delta)$ retained in-place GPU buffer patching.
pub struct DrawList<A: Allocator + Copy> {
    pub viewport: ViewportUniform,
    /// Monospace advance width as fraction of font height (Berkeley Mono ≈ 0.60).
    pub advance_ratio: f32,
    pub clips: OwnVec<ClipRectGpu, A>,
    pub rects: OwnVec<RectInst, A>,
    pub glyphs: OwnVec<GlyphInst, A>,
    pub overlay_rects: OwnVec<RectInst, A>,
    pub overlay_glyphs: OwnVec<GlyphInst, A>,
    /// When `true`, the host uploads the full buffers; when `false`, only `*_patches` ranges need upload.
    pub full_upload: bool,
    pub rect_patches: OwnVec<GpuPatch, A>,
    pub glyph_patches: OwnVec<GpuPatch, A>,
    pub ov_rect_patches: OwnVec<GpuPatch, A>,
    pub ov_glyph_patches: OwnVec<GpuPatch, A>,
    clip_stack: OwnVec<u32, A>,
    overlay_mode: bool,
}

impl<A: Allocator + Copy> DrawList<A> {
    /// Create a new empty draw list using explicit allocator `alloc`.
    pub fn new_in(alloc: A) -> Self {
        Self {
            viewport: ViewportUniform { size: [1280.0, 800.0], dpr: 1.0, time: 0.0 },
            advance_ratio: 0.60,
            clips: OwnVec::new_in(alloc),
            rects: OwnVec::new_in(alloc),
            glyphs: OwnVec::new_in(alloc),
            overlay_rects: OwnVec::new_in(alloc),
            overlay_glyphs: OwnVec::new_in(alloc),
            full_upload: true,
            rect_patches: OwnVec::new_in(alloc),
            glyph_patches: OwnVec::new_in(alloc),
            ov_rect_patches: OwnVec::new_in(alloc),
            ov_glyph_patches: OwnVec::new_in(alloc),
            clip_stack: OwnVec::new_in(alloc),
            overlay_mode: false,
        }
    }

    /// Reset buffers for a full-rebuild frame and seed clip slot `0` with the full viewport rect.
    pub fn begin_frame(&mut self, width: f32, height: f32, dpr: f32, time: f32) -> Result<(), UiError> {
        let w = width.max(1.0);
        let h = height.max(1.0);
        self.viewport = ViewportUniform { size: [w, h], dpr: dpr.max(1.0), time };
        self.clips.clear();
        self.rects.clear();
        self.glyphs.clear();
        self.overlay_rects.clear();
        self.overlay_glyphs.clear();
        self.rect_patches.clear();
        self.glyph_patches.clear();
        self.ov_rect_patches.clear();
        self.ov_glyph_patches.clear();
        self.full_upload = true;
        self.clip_stack.clear();
        self.overlay_mode = false;
        self.clips
            .try_push(ClipRectGpu { rect: [0.0, 0.0, w, h], params: [0.0, 0.0, 0.0, 0.0] })
            .map_err(|r| UiError::Alloc(r.why))?;
        self.clip_stack.try_push(0).map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    /// Begin an incremental patch frame preserving existing GPU buffers and clearing patch lists.
    pub fn begin_incremental_frame(&mut self, width: f32, height: f32, dpr: f32, time: f32) {
        self.viewport = ViewportUniform { size: [width.max(1.0), height.max(1.0)], dpr: dpr.max(1.0), time };
        self.rect_patches.clear();
        self.glyph_patches.clear();
        self.ov_rect_patches.clear();
        self.ov_glyph_patches.clear();
        self.full_upload = false;
        self.overlay_mode = false;
    }

    /// Current rect & glyph counts for `layer` (`0` = base, `1` = overlay).
    #[inline]
    pub fn layer_counts(&self, layer: u8) -> (u32, u32) {
        if layer > 0 {
            (usize_to_u32_sat(self.overlay_rects.len()), usize_to_u32_sat(self.overlay_glyphs.len()))
        } else {
            (usize_to_u32_sat(self.rects.len()), usize_to_u32_sat(self.glyphs.len()))
        }
    }

    /// Append off-screen hardware-culled slack instances so future in-place edits avoid rebuilds.
    pub fn reserve_slack(&mut self, layer: u8, rect_slack: u32, glyph_slack: u32) -> Result<(), UiError> {
        let (rects, glyphs) = if layer > 0 {
            (&mut self.overlay_rects, &mut self.overlay_glyphs)
        } else {
            (&mut self.rects, &mut self.glyphs)
        };
        for _ in 0..rect_slack {
            rects.try_push(culled_rect_inst()).map_err(|r| UiError::Alloc(r.why))?;
        }
        for _ in 0..glyph_slack {
            glyphs.try_push(culled_glyph_inst()).map_err(|r| UiError::Alloc(r.why))?;
        }
        Ok(())
    }

    /// Patch `slot` in place with `new_rects` and `new_glyphs`, recording compact [`GpuPatch`]es.
    ///
    /// Returns `Ok(true)` if the new geometry fit inside `slot`'s reserved capacity, or `Ok(false)`
    /// if the entity overflowed its slack and requires a full extract pass.
    pub fn patch_slot(
        &mut self,
        slot: &mut GpuSlot,
        new_rects: &[RectInst],
        new_glyphs: &[GlyphInst],
    ) -> Result<bool, UiError> {
        let new_r_len = usize_to_u32_sat(new_rects.len());
        let new_g_len = usize_to_u32_sat(new_glyphs.len());
        if new_r_len > slot.rect_cap || new_g_len > slot.glyph_cap {
            return Ok(false);
        }

        let (rects, glyphs, r_patches, g_patches) = if slot.layer > 0 {
            (
                &mut self.overlay_rects,
                &mut self.overlay_glyphs,
                &mut self.ov_rect_patches,
                &mut self.ov_glyph_patches,
            )
        } else {
            (
                &mut self.rects,
                &mut self.glyphs,
                &mut self.rect_patches,
                &mut self.glyph_patches,
            )
        };

        let r_start = usize::try_from(slot.rect_start).unwrap_or(0);
        let r_cap = usize::try_from(slot.rect_cap).unwrap_or(0);
        let old_r_len = usize::try_from(slot.rect_len).unwrap_or(0);
        if r_cap > 0 {
            let r_end = r_start.saturating_add(r_cap);
            let Some(dst) = rects.as_mut_slice().get_mut(r_start..r_end) else {
                return Ok(false);
            };
            let span = new_rects.len().max(old_r_len).min(r_cap);
            let mut lo = span;
            let mut hi = 0usize;
            for i in 0..span {
                let next_v = new_rects.get(i).copied().unwrap_or_else(culled_rect_inst);
                if let Some(cell) = dst.get_mut(i) {
                    if *cell != next_v {
                        *cell = next_v;
                        if i < lo {
                            lo = i;
                        }
                        hi = i + 1;
                    }
                }
            }
            if lo < hi {
                r_patches
                    .try_push(GpuPatch {
                        start: slot.rect_start.saturating_add(usize_to_u32_sat(lo)),
                        count: usize_to_u32_sat(hi - lo),
                    })
                    .map_err(|r| UiError::Alloc(r.why))?;
            }
        }

        let g_start = usize::try_from(slot.glyph_start).unwrap_or(0);
        let g_cap = usize::try_from(slot.glyph_cap).unwrap_or(0);
        let old_g_len = usize::try_from(slot.glyph_len).unwrap_or(0);
        if g_cap > 0 {
            let g_end = g_start.saturating_add(g_cap);
            let Some(dst) = glyphs.as_mut_slice().get_mut(g_start..g_end) else {
                return Ok(false);
            };
            let span = new_glyphs.len().max(old_g_len).min(g_cap);
            let mut lo = span;
            let mut hi = 0usize;
            for i in 0..span {
                let next_v = new_glyphs.get(i).copied().unwrap_or_else(culled_glyph_inst);
                if let Some(cell) = dst.get_mut(i) {
                    if *cell != next_v {
                        *cell = next_v;
                        if i < lo {
                            lo = i;
                        }
                        hi = i + 1;
                    }
                }
            }
            if lo < hi {
                g_patches
                    .try_push(GpuPatch {
                        start: slot.glyph_start.saturating_add(usize_to_u32_sat(lo)),
                        count: usize_to_u32_sat(hi - lo),
                    })
                    .map_err(|r| UiError::Alloc(r.why))?;
            }
        }

        slot.rect_len = new_r_len;
        slot.glyph_len = new_g_len;
        Ok(true)
    }

    /// Total GPU bytes scheduled for incremental patch upload in this frame.
    pub fn patched_bytes(&self) -> u32 {
        let mut total: u32 = 0;
        for p in self.rect_patches.as_slice() {
            total = total.saturating_add(p.count.saturating_mul(80));
        }
        for p in self.ov_rect_patches.as_slice() {
            total = total.saturating_add(p.count.saturating_mul(80));
        }
        for p in self.glyph_patches.as_slice() {
            total = total.saturating_add(p.count.saturating_mul(64));
        }
        for p in self.ov_glyph_patches.as_slice() {
            total = total.saturating_add(p.count.saturating_mul(64));
        }
        total
    }

    #[inline]
    pub fn set_overlay(&mut self, overlay: bool) {
        self.overlay_mode = overlay;
    }

    #[inline]
    pub fn current_clip_id(&self) -> u32 {
        self.clip_stack.last().copied().unwrap_or(0)
    }

    #[inline]
    pub fn clip_rect(&self, clip_id: u32) -> Rect {
        let idx = usize::try_from(clip_id).unwrap_or(0);
        match self.clips.get(idx) {
            Some(c) => Rect::from_min_max(c.rect[0], c.rect[1], c.rect[2], c.rect[3]),
            None => Rect::new(0.0, 0.0, self.viewport.size[0], self.viewport.size[1]),
        }
    }

    /// Register a new GPU clip entry intersected with `parent_clip_id` and return its `clip_id`.
    pub fn add_clip(&mut self, parent_clip_id: u32, rect: Rect, radius: f32) -> Result<u32, UiError> {
        let parent = self.clip_rect(parent_clip_id);
        let inter = parent.intersect(rect);
        let id = usize_to_u32_sat(self.clips.len());
        self.clips
            .try_push(ClipRectGpu { rect: inter.to_min_max(), params: [radius.max(0.0), 0.0, 0.0, 0.0] })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(id)
    }

    /// Push a nested rounded clip region onto the active clip stack and return its `clip_id`.
    pub fn push_clip(&mut self, rect: Rect, radius: f32) -> Result<u32, UiError> {
        let parent_id = self.current_clip_id();
        let id = self.add_clip(parent_id, rect, radius)?;
        self.clip_stack.try_push(id).map_err(|r| UiError::Alloc(r.why))?;
        Ok(id)
    }

    /// Push an already-registered `clip_id` onto the active clip stack.
    pub fn push_clip_id(&mut self, clip_id: u32) -> Result<(), UiError> {
        self.clip_stack.try_push(clip_id).map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    /// Pop the innermost clip region (never pops root clip `0`).
    pub fn pop_clip(&mut self) {
        if self.clip_stack.len() > 1 {
            let _ = self.clip_stack.pop();
        }
    }

    /// Push a raw `RectInst` into the active layer (`base` or `overlay`).
    #[inline]
    pub fn push_rect_inst(&mut self, inst: RectInst) -> Result<(), UiError> {
        let target = if self.overlay_mode { &mut self.overlay_rects } else { &mut self.rects };
        target.try_push(inst).map_err(|r| UiError::Alloc(r.why))
    }

    /// Push a raw `GlyphInst` into the active layer (`base` or `overlay`).
    #[inline]
    pub fn push_glyph_inst(&mut self, inst: GlyphInst) -> Result<(), UiError> {
        let target = if self.overlay_mode { &mut self.overlay_glyphs } else { &mut self.glyphs };
        target.try_push(inst).map_err(|r| UiError::Alloc(r.why))
    }

    /// Draw a filled SDF rounded rectangle with optional integrated anti-aliased border.
    pub fn rounded_rect_bordered(
        &mut self,
        rect: Rect,
        radius: f32,
        fill: Rgba,
        border: Rgba,
        border_width: f32,
    ) -> Result<(), UiError> {
        if rect.is_empty() {
            return Ok(());
        }
        let has_fill = !fill.is_transparent();
        let has_border = border_width > 0.01 && !border.is_transparent();
        if !has_fill && !has_border {
            return Ok(());
        }
        let bw = if has_border { border_width } else { 0.0 };
        self.push_rect_inst(RectInst {
            rect: rect.to_min_max(),
            color: fill.0,
            border_color: border.0,
            params: [radius.max(0.0), bw, 0.0, prim_kind::ROUND_RECT],
            clip_info: [self.current_clip_id(), 0, 0, 0],
        })
    }

    /// Draw a filled SDF rounded rectangle (no border).
    #[inline]
    pub fn rounded_rect(&mut self, rect: Rect, radius: f32, fill: Rgba) -> Result<(), UiError> {
        self.rounded_rect_bordered(rect, radius, fill, Rgba::TRANSPARENT, 0.0)
    }

    /// Draw a crisp axis-aligned filled rectangle (`radius = 0`).
    #[inline]
    pub fn fill_rect(&mut self, rect: Rect, fill: Rgba) -> Result<(), UiError> {
        self.rounded_rect_bordered(rect, 0.0, fill, Rgba::TRANSPARENT, 0.0)
    }

    /// Draw a stroked rounded rectangle border (transparent interior).
    #[inline]
    pub fn stroke_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        border: Rgba,
        border_width: f32,
    ) -> Result<(), UiError> {
        self.rounded_rect_bordered(rect, radius, Rgba::TRANSPARENT, border, border_width)
    }

    /// Draw a soft SDF drop shadow behind `rect`.
    pub fn shadow(
        &mut self,
        rect: Rect,
        radius: f32,
        blur: f32,
        offset_y: f32,
        color: Rgba,
    ) -> Result<(), UiError> {
        if rect.is_empty() || color.is_transparent() || blur <= 0.1 {
            return Ok(());
        }
        let shifted = Rect::new(rect.x, rect.y + offset_y, rect.w, rect.h);
        self.push_rect_inst(RectInst {
            rect: shifted.to_min_max(),
            color: color.0,
            border_color: [0.0; 4],
            params: [radius.max(0.0), 0.0, blur, prim_kind::SHADOW],
            clip_info: [self.current_clip_id(), 0, 0, 0],
        })
    }

    /// Draw an anti-aliased SDF line segment from `(x0, y0)` to `(x1, y1)`.
    pub fn line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        thickness: f32,
        color: Rgba,
    ) -> Result<(), UiError> {
        if color.is_transparent() || thickness <= 0.05 {
            return Ok(());
        }
        self.push_rect_inst(RectInst {
            rect: [x0, y0, x1, y1],
            color: color.0,
            border_color: [0.0; 4],
            params: [thickness, 0.0, 0.0, prim_kind::LINE],
            clip_info: [self.current_clip_id(), 0, 0, 0],
        })
    }

    /// Draw an anti-aliased SDF filled circle (when `ring_width <= 0.0`) or stroked ring.
    pub fn circle(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        ring_width: f32,
        color: Rgba,
    ) -> Result<(), UiError> {
        if color.is_transparent() || radius <= 0.1 {
            return Ok(());
        }
        self.push_rect_inst(RectInst {
            rect: [cx - radius, cy - radius, cx + radius, cy + radius],
            color: color.0,
            border_color: [0.0; 4],
            params: [radius, ring_width.max(0.0), 0.0, prim_kind::CIRCLE],
            clip_info: [self.current_clip_id(), 0, 0, 0],
        })
    }

    /// Measure the advance width of `text` at `font_px` using the Berkeley Mono advance ratio.
    #[inline]
    pub fn measure_text(&self, text: &str, font_px: f32) -> f32 {
        let mut cols: u32 = 0;
        for _ in text.chars() {
            cols = cols.saturating_add(1);
        }
        let cols_f = u16::try_from(cols).map_or(10_000.0, f32::from);
        cols_f * font_px * self.advance_ratio
    }

    /// Draw single-line UTF-8 text at `(x, y)` (top-left of line box) with Berkeley Mono ligatures.
    pub fn text(&mut self, x: f32, y: f32, text: &str, font_px: f32, color: Rgba) -> Result<f32, UiError> {
        self.text_max_width(x, y, text, font_px, color, 1_000_000.0)
    }

    /// Draw single-line UTF-8 text truncated cleanly with `…` if wider than `max_w`.
    pub fn text_max_width(
        &mut self,
        x: f32,
        y: f32,
        text: &str,
        font_px: f32,
        color: Rgba,
        max_w: f32,
    ) -> Result<f32, UiError> {
        if text.is_empty() || color.is_transparent() || max_w <= 2.0 {
            return Ok(0.0);
        }
        let adv = (font_px * self.advance_ratio).max(1.0);
        let cell_h = font_px * 1.42;
        let cell_w = cell_h * (24.0 / 40.0);
        let y_off = y - (cell_h - font_px) * 0.44;
        let clip_id = self.current_clip_id();

        let total_w = self.measure_text(text, font_px);
        let needs_ellipsis = total_w > max_w + 0.5;
        let limit_x = if needs_ellipsis { x + (max_w - adv).max(0.0) } else { x + max_w + 1.0 };

        let bytes = text.as_bytes();
        let mut cx = x;
        let mut i: usize = 0;

        while i < bytes.len() {
            if cx + adv > limit_x {
                if needs_ellipsis && cx + adv <= x + max_w + 0.5 {
                    self.emit_atlas_cell(cx, y_off, cell_w, cell_h, 5, 6, 1, color, clip_id)?;
                    cx += adv;
                }
                break;
            }

            // Check Berkeley Mono double-width OpenType ligatures (row 7) when 2 ASCII bytes fit.
            if i + 1 < bytes.len() && cx + adv * 2.0 <= limit_x {
                if let (Some(&b0), Some(&b1)) = (bytes.get(i), bytes.get(i + 1)) {
                    let mut lig_col: Option<u8> = None;
                    for (pat, idx) in LIGATURES {
                        if pat[0] == b0 && pat[1] == b1 {
                            lig_col = Some(idx * 2);
                            break;
                        }
                    }
                    if let Some(col) = lig_col {
                        let x_pad = (cell_w - adv) * 0.5;
                        self.emit_atlas_cell(cx - x_pad * 2.0, y_off, cell_w * 2.0, cell_h, col, 7, 2, color, clip_id)?;
                        cx += adv * 2.0;
                        i += 2;
                        continue;
                    }
                }
            }

            // Decode next UTF-8 scalar cleanly without panics.
            let rest = text.get(i..).unwrap_or("");
            let Some(ch) = rest.chars().next() else { break };
            let ch_len = ch.len_utf8();

            if ch == ' ' {
                cx += adv;
                i += ch_len;
                continue;
            }

            if let Some((col, row)) = lookup_glyph_cell(ch) {
                let x_pad = (cell_w - adv) * 0.5;
                self.emit_atlas_cell(cx - x_pad, y_off, cell_w, cell_h, col, row, 1, color, clip_id)?;
            }
            cx += adv;
            i += ch_len;
        }
        Ok(cx - x)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_atlas_cell(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        col: u8,
        row: u8,
        col_span: u8,
        color: Rgba,
        clip_id: u32,
    ) -> Result<(), UiError> {
        let u0 = u8_to_f32(col) / ATLAS_COLS;
        let u1 = u8_to_f32(col.saturating_add(col_span)) / ATLAS_COLS;
        let v0 = u8_to_f32(row) / ATLAS_ROWS;
        let v1 = u8_to_f32(row.saturating_add(1)) / ATLAS_ROWS;
        self.push_glyph_inst(GlyphInst {
            rect: [x, y, x + w, y + h],
            uv: [u0, v0, u1, v1],
            color: color.0,
            clip_info: [clip_id, 0, 0, 0],
        })
    }
}

fn lookup_glyph_cell(ch: char) -> Option<(u8, u8)> {
    let code = u32::from(ch);
    if (u32::from(ATLAS_FIRST_CHAR)..=u32::from(ATLAS_LAST_CHAR)).contains(&code) {
        let idx = u8::try_from(code - u32::from(ATLAS_FIRST_CHAR)).ok()?;
        return Some((idx % 16, idx / 16));
    }
    for (i, &sym) in ROW6_SYMBOLS.iter().enumerate() {
        if sym == ch {
            let col = u8::try_from(i).ok()?;
            return Some((col, 6));
        }
    }
    None
}
