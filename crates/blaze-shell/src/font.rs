//! Dual-font (Apple SF Pro proportional sans + SF Mono/Berkeley Mono monospace) HiDPI glyph atlas
//! on macOS: CoreText selects and rasterizes glyphs across the type scale (`Mono`, `Ui`, `UiSmall`,
//! `Heading`) with automatic system fallback into a single 2048×2048 alpha-only texture uploaded
//! incrementally to Metal.

use std::collections::HashMap;
use std::ptr::NonNull;

use blaze_ui::shaders::GlyphInst;
use objc2_core_foundation::{CFRange, CFRetained, CFString, CGFloat, CGPoint, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext};
use objc2_core_text::{CTFont, CTFontOrientation, CTFontUIFontType};

use crate::view::{CellMetrics, TextFace, TextRun};

pub const ATLAS: usize = 2048;

/// Primary monospace cell metrics in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: f64,
}

#[derive(Clone, Copy, Debug)]
struct FaceMetrics {
    h: usize,
    baseline: f64,
    space_adv: f32,
    y_nudge_px: f32,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    ch: char,
    face: TextFace,
    bold: bool,
    italic: bool,
    wide: bool,
}

/// Where a rasterized glyph sits in the atlas, in texels, plus its device-pixel advance & draw offset.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub advance: f32,
    pub x_off: f32,
}

pub struct Fonts {
    mono: [CFRetained<CTFont>; 4],
    ui: [CFRetained<CTFont>; 2],
    ui_small: [CFRetained<CTFont>; 2],
    heading: CFRetained<CTFont>,
    chrome: [CFRetained<CTFont>; 2],
    pub metrics: Metrics,
    ui_m: FaceMetrics,
    ui_small_m: FaceMetrics,
    heading_m: FaceMetrics,
    chrome_m: FaceMetrics,
    /// Exact device-pixel CoreText advances for ASCII `32..=126` in `ui[0]`.
    ui_adv_px: [f32; 95],
    /// Ratio `chrome_px / (points * scale * 0.94)`.
    chrome_ratio: f32,
    slots: HashMap<Key, Option<Slot>>,
    next_x: usize,
    next_y: usize,
    row_h: usize,
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

fn pick_ui_font(px: f64, bold: bool) -> CFRetained<CTFont> {
    let ui_type = if bold {
        CTFontUIFontType::EmphasizedSystem
    } else {
        CTFontUIFontType::System
    };
    // SAFETY: valid `CTFontUIFontType` and positive size, null language string -> native Apple SF Pro.
    if let Some(f) = unsafe { CTFont::new_ui_font_for_language(ui_type, px as CGFloat, None) } {
        return f;
    }
    if bold {
        pick_font(&["SFProText-Semibold", "HelveticaNeue-Medium", "Helvetica-Bold"], px)
    } else {
        pick_font(&["SFProText-Regular", "HelveticaNeue", "Helvetica"], px)
    }
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

fn advance_of(f: &CTFont, g: u16) -> f64 {
    let mut gg = g;
    let mut adv = CGSize { width: 0.0, height: 0.0 };
    // SAFETY: single glyph and single advance output pointer on a live `CTFont`.
    unsafe {
        f.advances_for_glyphs(
            CTFontOrientation::Default,
            NonNull::from(&mut gg),
            &mut adv,
            1,
        );
    }
    adv.width.max(1.0)
}

fn measure_face_metrics(f: &CTFont, mono_cell_h: usize, mono_baseline: f64) -> FaceMetrics {
    // SAFETY: plain metric getters on a live `CTFont`.
    let (ascent, descent, leading) = unsafe { (f.ascent(), f.descent(), f.leading()) };
    let h = (ascent + descent + leading).ceil().max(1.0) as usize;
    let baseline = (ascent + leading / 2.0).round();
    let space_adv = glyph_of(f, ' ')
        .map(|g| advance_of(f, g) as f32)
        .unwrap_or((h as f32) * 0.25);
    // Align proportional UI text optically onto the primary monospace cell's vertical baseline.
    let y_nudge_px = (mono_baseline - baseline) as f32;
    let _ = mono_cell_h;
    FaceMetrics { h, baseline, space_adv, y_nudge_px }
}

impl Fonts {
    pub fn new(points: f64, scale: f64) -> Fonts {
        let px = points * scale;
        let mono = [
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
        let f = &mono[0];
        // SAFETY: plain metric getters on a live `CTFont`.
        let (ascent, descent, leading) = unsafe { (f.ascent(), f.descent(), f.leading()) };
        let g = glyph_of(f, 'M').unwrap_or(0);
        let cell_w = advance_of(f, g).round().max(1.0) as usize;
        let cell_h = (ascent + descent + leading).ceil().max(1.0) as usize;
        let baseline = (ascent + leading / 2.0).round();
        let metrics = Metrics { cell_w, cell_h, baseline };

        // Native Apple SF Pro faces for UI chrome, table headers, badges, & rendered Markdown!
        let ui = [pick_ui_font(px * 0.94, false), pick_ui_font(px * 0.94, true)];
        let ui_small = [pick_ui_font(px * 0.84, false), pick_ui_font(px * 0.84, true)];
        let heading = pick_ui_font(px * 1.18, true);
        // Fixed native macOS 12pt SF Pro for titlebar tabs & pane headers (never overflows on Cmd+ zoom!).
        let chrome_px = 12.0 * scale;
        let chrome = [pick_ui_font(chrome_px, false), pick_ui_font(chrome_px, true)];

        let ui_m = measure_face_metrics(&ui[0], cell_h, baseline);
        let ui_small_m = measure_face_metrics(&ui_small[0], cell_h, baseline);
        let heading_m = FaceMetrics {
            y_nudge_px: 0.0,
            ..measure_face_metrics(&heading, cell_h, baseline)
        };
        let chrome_m = FaceMetrics {
            y_nudge_px: 0.0,
            ..measure_face_metrics(&chrome[0], cell_h, baseline)
        };

        let mut ui_adv_px = [0.0f32; 95];
        for code in 32u8..=126u8 {
            let ch = code as char;
            ui_adv_px[(code - 32) as usize] = glyph_of(&ui[0], ch)
                .map(|g| advance_of(&ui[0], g) as f32)
                .unwrap_or(ui_m.space_adv);
        }
        let chrome_ratio = (chrome_px / (px * 0.94).max(1.0)) as f32;

        Fonts {
            mono,
            ui,
            ui_small,
            heading,
            chrome,
            metrics,
            ui_m,
            ui_small_m,
            heading_m,
            chrome_m,
            ui_adv_px,
            chrome_ratio,
            slots: HashMap::new(),
            next_x: 0,
            next_y: 0,
            row_h: 0,
            pending: Vec::new(),
        }
    }

    /// Logical CSS-pixel cell metrics for the layout engine, carrying exact CoreText advances.
    pub fn cell_metrics(&self, scale: f64) -> CellMetrics {
        let inv_scale = 1.0 / (scale as f32).max(1.0);
        let mut ui_adv = [0.0f32; 95];
        for (dst, &src) in ui_adv.iter_mut().zip(self.ui_adv_px.iter()) {
            *dst = src * inv_scale;
        }
        CellMetrics {
            cw: self.metrics.cell_w as f32 * inv_scale,
            ch: self.metrics.cell_h as f32 * inv_scale,
            ui_adv,
            chrome_ratio: self.chrome_ratio,
        }
    }

    /// Look up or rasterize `ch` in the given face & style; `None` for whitespace or a full atlas.
    fn slot(&mut self, key: Key) -> Option<Slot> {
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
            let (space_adv, y_nudge) = match run.face {
                TextFace::Mono => (cw, 0.0),
                TextFace::Ui => (self.ui_m.space_adv * inv_scale, self.ui_m.y_nudge_px * inv_scale),
                TextFace::UiSmall => (
                    self.ui_small_m.space_adv * inv_scale,
                    self.ui_small_m.y_nudge_px * inv_scale,
                ),
                TextFace::Heading => (
                    self.heading_m.space_adv * inv_scale,
                    self.heading_m.y_nudge_px * inv_scale,
                ),
                TextFace::Chrome => (
                    self.chrome_m.space_adv * inv_scale,
                    self.chrome_m.y_nudge_px * inv_scale,
                ),
            };
            let y = run.y + y_nudge;

            for ch in run.text.chars() {
                if ch == ' ' || ch == '\t' {
                    x += space_adv;
                    continue;
                }
                if (ch as u32) < 0x20 {
                    continue;
                }
                let wide = run.face == TextFace::Mono && is_wide_char(ch);
                let key = Key { ch, face: run.face, bold: run.bold, italic: run.italic, wide };
                if let Some(s) = self.slot(key) {
                    let gw = s.w as f32 * inv_scale;
                    let gh = s.h as f32 * inv_scale;
                    let draw_x = x + s.x_off * inv_scale;
                    glyphs.push(GlyphInst {
                        rect: [draw_x, y, draw_x + gw, y + gh],
                        uv: [
                            s.x as f32 * inv_atlas,
                            s.y as f32 * inv_atlas,
                            (s.x + s.w) as f32 * inv_atlas,
                            (s.y + s.h) as f32 * inv_atlas,
                        ],
                        color: run.color,
                        clip_info: [run.clip_id, 0, 0, 0],
                    });
                    x += s.advance * inv_scale;
                } else {
                    x += if run.face == TextFace::Mono {
                        if wide { cw * 2.0 } else { cw }
                    } else {
                        space_adv
                    };
                }
            }
        }
        glyphs
    }

    fn raster(&mut self, key: Key) -> Option<Slot> {
        let (base_font, h, baseline) = match key.face {
            TextFace::Mono => {
                let f = &self.mono[usize::from(key.bold) | (usize::from(key.italic) << 1)];
                (f.clone(), self.metrics.cell_h, self.metrics.baseline)
            }
            TextFace::Ui => (
                self.ui[usize::from(key.bold)].clone(),
                self.ui_m.h,
                self.ui_m.baseline,
            ),
            TextFace::UiSmall => (
                self.ui_small[usize::from(key.bold)].clone(),
                self.ui_small_m.h,
                self.ui_small_m.baseline,
            ),
            TextFace::Heading => (self.heading.clone(), self.heading_m.h, self.heading_m.baseline),
            TextFace::Chrome => (
                self.chrome[usize::from(key.bold)].clone(),
                self.chrome_m.h,
                self.chrome_m.baseline,
            ),
        };

        let (font, glyph) = match glyph_of(&base_font, key.ch) {
            Some(g) => (base_font, g),
            None => {
                let s = CFString::from_str(key.ch.encode_utf8(&mut [0; 4]));
                // SAFETY: live `CTFont` and a valid UTF-16 range inside `s`.
                let fb = unsafe {
                    base_font.for_string(&s, CFRange { location: 0, length: s.length() })
                };
                let g = glyph_of(&fb, key.ch)?;
                (fb, g)
            }
        };

        let (w, advance, pad_l) = if key.face == TextFace::Mono {
            let cw = if key.wide { self.metrics.cell_w * 2 } else { self.metrics.cell_w };
            (cw, cw as f32, 0.0f64)
        } else {
            let adv = advance_of(&font, glyph);
            // 2px horizontal pad on each side prevents italic/kerning overhang clipping in SF Pro!
            let pad = 2.0f64;
            let bw = (adv.ceil() as usize + 4).max(4).min(self.metrics.cell_w * 4);
            (bw, adv as f32, pad)
        };

        if self.next_x + w > ATLAS {
            self.next_x = 0;
            self.next_y += self.row_h.max(h) + 1;
            self.row_h = 0;
        }
        if self.next_y + h > ATLAS {
            return None;
        }
        self.row_h = self.row_h.max(h);

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
            let mut at = CGPoint { x: pad_l, y: h as f64 - baseline };
            // SAFETY: one glyph and one point; `ctx` is a valid bitmap context.
            unsafe {
                font.draw_glyphs(NonNull::from(&mut g), NonNull::from(&mut at), 1, &ctx);
            }
        }
        if buf.iter().all(|&a| a == 0) {
            return None;
        }
        let slot = Slot {
            x: self.next_x,
            y: self.next_y,
            w,
            h,
            advance,
            x_off: -(pad_l as f32),
        };
        self.next_x += w + 1;
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
