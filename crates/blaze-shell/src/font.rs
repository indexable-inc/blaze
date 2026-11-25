//! Monospace font and HiDPI glyph atlas on macOS: CoreText picks and rasterizes glyphs
//! (with automatic system fallback for Unicode characters the primary font lacks) into a
//! single 2048x2048 alpha-only texture uploaded incrementally to Metal.

use std::collections::HashMap;
use std::ptr::NonNull;

use blaze_ui::shaders::GlyphInst;
use objc2_core_foundation::{CFRange, CFRetained, CFString, CGFloat, CGPoint, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext};
use objc2_core_text::{CTFont, CTFontOrientation};

use crate::view::{CellMetrics, TextRun};

pub const ATLAS: usize = 2048;

/// Cell metrics in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: f64,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    ch: char,
    bold: bool,
    italic: bool,
    wide: bool,
}

/// Where a rasterized glyph sits in the atlas, in texels.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub struct Fonts {
    faces: [CFRetained<CTFont>; 4],
    pub metrics: Metrics,
    slots: HashMap<Key, Option<Slot>>,
    next_x: usize,
    next_y: usize,
    /// Texels written since the last Metal texture upload: `(slot, alpha_bytes)`.
    pub pending: Vec<(Slot, Vec<u8>)>,
}

fn pick_font(names: &[&str], px: f64) -> CFRetained<CTFont> {
    for name in names {
        let s = CFString::from_str(name);
        // SAFETY: valid font PostScript name and positive size, null transform matrix.
        let f = unsafe { CTFont::with_name(&s, px as CGFloat, std::ptr::null()) };
        // SAFETY: plain CoreText getter on a live `CTFont`.
        let got = unsafe { f.post_script_name() }.to_string();
        if got.eq_ignore_ascii_case(name) {
            return f;
        }
    }
    let s = CFString::from_str("Menlo-Regular");
    // SAFETY: valid fallback system font name and positive size, null matrix.
    unsafe { CTFont::with_name(&s, px as CGFloat, std::ptr::null()) }
}

fn glyph_of(f: &CTFont, ch: char) -> Option<u16> {
    let mut units = [0u16; 2];
    let n = ch.encode_utf16(&mut units).len();
    let mut glyphs = [0u16; 2];
    // SAFETY: both buffers hold two UTF-16/glyph slots; at most `n <= 2` are read/written.
    let found = unsafe {
        f.glyphs_for_characters(
            NonNull::from(&mut units).cast(),
            NonNull::from(&mut glyphs).cast(),
            n as isize,
        )
    };
    if found && glyphs[0] != 0 { Some(glyphs[0]) } else { None }
}

impl Fonts {
    pub fn new(points: f64, scale: f64) -> Fonts {
        let px = points * scale;
        let faces = [
            pick_font(
                &["BerkeleyMono-Regular", "JetBrainsMono-Regular", "SFMono-Regular", "Menlo-Regular"],
                px,
            ),
            pick_font(
                &["BerkeleyMono-Bold", "JetBrainsMono-Bold", "SFMono-Bold", "Menlo-Bold"],
                px,
            ),
            pick_font(
                &["BerkeleyMono-Oblique", "JetBrainsMono-Italic", "SFMono-RegularItalic", "Menlo-Italic"],
                px,
            ),
            pick_font(
                &["BerkeleyMono-Bold-Oblique", "JetBrainsMono-BoldItalic", "SFMono-BoldItalic", "Menlo-BoldItalic"],
                px,
            ),
        ];
        let f = &faces[0];
        // SAFETY: plain metric getters on a live `CTFont`.
        let (ascent, descent, leading) = unsafe { (f.ascent(), f.descent(), f.leading()) };
        let mut g = glyph_of(f, 'M').unwrap_or(0);
        let mut adv = CGSize { width: 0.0, height: 0.0 };
        // SAFETY: single glyph and single advance output pointer.
        unsafe {
            f.advances_for_glyphs(
                CTFontOrientation::Default,
                NonNull::from(&mut g),
                &mut adv,
                1,
            );
        }
        let cell_w = adv.width.round().max(1.0) as usize;
        let cell_h = (ascent + descent + leading).ceil().max(1.0) as usize;
        let metrics = Metrics {
            cell_w,
            cell_h,
            baseline: (ascent + leading / 2.0).round(),
        };
        Fonts {
            faces,
            metrics,
            slots: HashMap::new(),
            next_x: 0,
            next_y: 0,
            pending: Vec::new(),
        }
    }

    /// Logical CSS-pixel cell metrics for the layout engine.
    pub fn cell_metrics(&self, scale: f64) -> CellMetrics {
        let inv_scale = 1.0 / (scale as f32).max(1.0);
        CellMetrics {
            cw: self.metrics.cell_w as f32 * inv_scale,
            ch: self.metrics.cell_h as f32 * inv_scale,
        }
    }

    /// Look up or rasterize `ch` in the given style; `None` for whitespace or a full atlas.
    pub fn slot(&mut self, ch: char, bold: bool, italic: bool, wide: bool) -> Option<Slot> {
        let key = Key { ch, bold, italic, wide };
        if let Some(s) = self.slots.get(&key) {
            return *s;
        }
        let s = self.raster(key);
        self.slots.insert(key, s);
        s
    }

    /// Convert laid-out [`TextRun`]s into GPU [`GlyphInst`] quads, rasterizing missing glyphs
    /// into `self.pending` on demand.
    pub fn rasterize_runs(&mut self, runs: &[TextRun], scale: f64) -> Vec<GlyphInst> {
        let mut glyphs = Vec::with_capacity(runs.len() * 12);
        let inv_atlas = 1.0 / ATLAS as f32;
        let inv_scale = 1.0 / (scale as f32).max(1.0);
        let cw = self.metrics.cell_w as f32 * inv_scale;

        for run in runs {
            let mut x = run.x;
            let y = run.y;
            for ch in run.text.chars() {
                if ch == ' ' || ch == '\t' {
                    x += cw;
                    continue;
                }
                if (ch as u32) < 0x20 {
                    continue;
                }
                let wide = is_wide_char(ch);
                if let Some(s) = self.slot(ch, run.bold, run.italic, wide) {
                    let gw = s.w as f32 * inv_scale;
                    let gh = s.h as f32 * inv_scale;
                    glyphs.push(GlyphInst {
                        rect: [x, y, x + gw, y + gh],
                        uv: [
                            s.x as f32 * inv_atlas,
                            s.y as f32 * inv_atlas,
                            (s.x + s.w) as f32 * inv_atlas,
                            (s.y + s.h) as f32 * inv_atlas,
                        ],
                        color: run.color,
                        clip_info: [run.clip_id, 0, 0, 0],
                    });
                }
                x += if wide { cw * 2.0 } else { cw };
            }
        }
        glyphs
    }

    fn raster(&mut self, key: Key) -> Option<Slot> {
        let m = self.metrics;
        let w = if key.wide { m.cell_w * 2 } else { m.cell_w };
        let h = m.cell_h;
        if self.next_x + w > ATLAS {
            self.next_x = 0;
            self.next_y += h;
        }
        if self.next_y + h > ATLAS {
            return None;
        }
        let face = &self.faces[usize::from(key.bold) | (usize::from(key.italic) << 1)];
        let (font, glyph) = match glyph_of(face, key.ch) {
            Some(g) => (face.clone(), g),
            None => {
                let s = CFString::from_str(key.ch.encode_utf8(&mut [0; 4]));
                // SAFETY: live `CTFont` and a valid UTF-16 range inside `s`.
                let fb = unsafe { face.for_string(&s, CFRange { location: 0, length: s.length() }) };
                let g = glyph_of(&fb, key.ch)?;
                (fb, g)
            }
        };
        let mut buf = vec![0u8; w * h];
        {
            let space = CGColorSpace::new_device_gray()?;
            // SAFETY: `buf` is `w * h` bytes (8-bit alpha-only bitmap, kCGImageAlphaOnly = 7);
            // the `CGContext` is dropped at the end of this block before `buf` is read.
            let ctx = unsafe {
                CGBitmapContextCreate(buf.as_mut_ptr().cast(), w, h, 8, w, Some(&space), 7)
            }?;
            CGContext::set_should_antialias(Some(&ctx), true);
            CGContext::set_allows_font_smoothing(Some(&ctx), false);
            CGContext::set_gray_fill_color(Some(&ctx), 1.0, 1.0);
            let mut g = glyph;
            // CoreGraphics origin is bottom-left; baseline sits `descent` above bottom.
            let mut at = CGPoint { x: 0.0, y: h as f64 - m.baseline };
            // SAFETY: one glyph and one point; `ctx` is a valid bitmap context.
            unsafe {
                font.draw_glyphs(NonNull::from(&mut g), NonNull::from(&mut at), 1, &ctx);
            }
        }
        if buf.iter().all(|&a| a == 0) {
            return None;
        }
        let slot = Slot { x: self.next_x, y: self.next_y, w, h };
        self.next_x += w;
        self.pending.push((slot, buf));
        Some(slot)
    }
}

fn is_wide_char(ch: char) -> bool {
    let cp = ch as u32;
    (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE10..=0xFE6F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp)
}
