//! Dual-font (Apple SF Pro proportional sans + SF Mono/Berkeley Mono monospace) HiDPI glyph atlas
//! on macOS: CoreText selects and rasterizes glyphs across the type scale (`Mono`, `Ui`, `UiSmall`,
//! `Heading`) with automatic system fallback into a single 2048×2048 alpha-only texture uploaded
//! incrementally to Metal.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

use blaze_ui::shaders::GlyphInst;
use objc2_core_foundation::{CFRange, CFRetained, CFString, CGFloat, CGPoint, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext};
use objc2_core_text::{CTFont, CTFontOrientation, CTFontUIFontType};

use crate::view::{CellMetrics, TextFace, TextRun};

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct LigatureKey {
    style: u8,
    len: u8,
    bytes: [u8; 4],
}

#[inline]
fn is_ligature_op(b: u8) -> bool {
    matches!(
        b,
        b'-' | b'>' | b'<' | b'=' | b'!' | b'|' | b':' | b'&' | b'.' | b'/' | b'*' | b'~' | b'+'
            | b'?' | b'#' | b'^'
    )
}

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
    mono_small: [CFRetained<CTFont>; 2],
    ui: [CFRetained<CTFont>; 2],
    ui_small: [CFRetained<CTFont>; 2],
    heading: CFRetained<CTFont>,
    chrome: [CFRetained<CTFont>; 2],
    pub metrics: Metrics,
    mono_small_m: FaceMetrics,
    mono_small_cell_w: usize,
    ui_m: FaceMetrics,
    ui_small_m: FaceMetrics,
    heading_m: FaceMetrics,
    chrome_m: FaceMetrics,
    /// Exact device-pixel CoreText advances for ASCII `32..=126` in `ui[0]` (Regular) and `ui[1]` (Bold).
    ui_adv_px: [f32; 95],
    ui_bold_adv_px: [f32; 95],
    /// Ratio `chrome_px / (points * scale * 0.94)`.
    chrome_ratio: f32,
    slots: HashMap<Key, Option<Slot>>,
    /// CoreText-shaped OpenType programming ligature slots (`->`, `-->`, `=>`, `!=`, `|>`, etc.).
    ligatures: HashMap<LigatureKey, Option<Slot>>,
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
                &[
                    "BerkeleyMono-Regular",
                    "FiraCodeNFM-Reg",
                    "FiraCode-Regular",
                    "Lilex-Regular",
                    "JetBrainsMono-Regular",
                    "Iosevka",
                    "SFMono-Regular",
                    "Menlo-Regular",
                ],
                px,
            ),
            pick_font(
                &[
                    "BerkeleyMono-Bold",
                    "FiraCodeNFM-Bold",
                    "FiraCode-Bold",
                    "Lilex-Bold",
                    "JetBrainsMono-Bold",
                    "Iosevka-Bold",
                    "SFMono-Bold",
                    "Menlo-Bold",
                ],
                px,
            ),
            pick_font(
                &[
                    "BerkeleyMono-Oblique",
                    "FiraCodeNFM-Reg",
                    "Lilex-Regular",
                    "JetBrainsMono-Italic",
                    "Iosevka-Italic",
                    "SFMono-RegularItalic",
                    "Menlo-Italic",
                ],
                px,
            ),
            pick_font(
                &[
                    "BerkeleyMono-Bold-Oblique",
                    "FiraCodeNFM-Bold",
                    "Lilex-Bold",
                    "JetBrainsMono-BoldItalic",
                    "Iosevka-BoldItalic",
                    "SFMono-BoldItalic",
                    "Menlo-BoldItalic",
                ],
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

        // Compact `0.84×` monospace of the exact same primary font family for scrollback table
        // headers, badges, and metadata pills (keeps the scrollback canvas 100% unified monospace!).
        let mono_small_px = px * 0.84;
        // SAFETY: `post_script_name` is a plain getter on a live, retained `CTFont`.
        let mono_small_reg_name = unsafe { mono[0].post_script_name() }.to_string();
        // SAFETY: `post_script_name` is a plain getter on a live, retained `CTFont`.
        let mono_small_bold_name = unsafe { mono[1].post_script_name() }.to_string();
        let mono_small = [
            pick_font(&[mono_small_reg_name.as_str(), "SFMono-Regular", "Menlo-Regular"], mono_small_px),
            pick_font(&[mono_small_bold_name.as_str(), "SFMono-Bold", "Menlo-Bold"], mono_small_px),
        ];
        let mono_small_m = measure_face_metrics(&mono_small[0], cell_h, baseline);
        let mono_small_cell_w = glyph_of(&mono_small[0], 'M')
            .map(|gm| advance_of(&mono_small[0], gm).round().max(1.0) as usize)
            .unwrap_or_else(|| ((cell_w as f64) * 0.84).round().max(1.0) as usize);

        // Native Apple SF Pro faces strictly for floating overlays, popovers, & window chrome!
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
        let mut ui_bold_adv_px = [0.0f32; 95];
        for code in 32u8..=126u8 {
            let ch = code as char;
            let idx = (code - 32) as usize;
            ui_adv_px[idx] = glyph_of(&ui[0], ch)
                .map(|g| advance_of(&ui[0], g) as f32)
                .unwrap_or(ui_m.space_adv);
            ui_bold_adv_px[idx] = glyph_of(&ui[1], ch)
                .map(|g| advance_of(&ui[1], g) as f32)
                .unwrap_or(ui_adv_px[idx] * 1.06);
        }
        let chrome_ratio = (chrome_px / (px * 0.94).max(1.0)) as f32;

        Fonts {
            mono,
            mono_small,
            ui,
            ui_small,
            heading,
            chrome,
            metrics,
            mono_small_m,
            mono_small_cell_w,
            ui_m,
            ui_small_m,
            heading_m,
            chrome_m,
            ui_adv_px,
            ui_bold_adv_px,
            chrome_ratio,
            slots: HashMap::new(),
            ligatures: HashMap::new(),
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
        let mut ui_bold_adv = [0.0f32; 95];
        for i in 0..95 {
            ui_adv[i] = self.ui_adv_px[i] * inv_scale;
            ui_bold_adv[i] = self.ui_bold_adv_px[i] * inv_scale;
        }
        CellMetrics {
            cw: self.metrics.cell_w as f32 * inv_scale,
            ch: self.metrics.cell_h as f32 * inv_scale,
            ui_adv,
            ui_bold_adv,
            chrome_ratio: self.chrome_ratio,
        }
    }

    /// Look up or CoreText-shape a 2..=4 char ASCII operator ligature (`->`, `-->`, `=>`, `!=`,
    /// `|>`, etc.). Returns `None` if the active mono font does not substitute a ligature glyph.
    fn ligature_slot(&mut self, style: u8, seq: &[u8]) -> Option<Slot> {
        if seq.len() < 2 || seq.len() > 4 {
            return None;
        }
        let mut arr = [0u8; 4];
        arr[..seq.len()].copy_from_slice(seq);
        let key = LigatureKey { style, len: seq.len() as u8, bytes: arr };
        if let Some(&cached) = self.ligatures.get(&key) {
            return cached;
        }
        let rastered = self.raster_ligature(style, seq);
        self.ligatures.insert(key, rastered);
        rastered
    }

    fn raster_ligature(&mut self, style: u8, seq: &[u8]) -> Option<Slot> {
        let text = core::str::from_utf8(seq).ok()?;
        let font = self.mono[(style as usize) & 3].clone();

        let mut plain_gids = [0u16; 4];
        for (idx, &b) in seq.iter().enumerate() {
            plain_gids[idx] = glyph_of(&font, b as char)?;
        }

        let w = self.metrics.cell_w * seq.len();
        let h = self.metrics.cell_h;
        let buf = crate::image_atlas::shape_and_raster_ligature_alpha(
            CFRetained::as_ptr(&font).as_ptr().cast::<c_void>(),
            text,
            &plain_gids[..seq.len()],
            w,
            h,
            self.metrics.baseline,
        )?;

        if self.next_x + w > ATLAS {
            self.next_x = 0;
            self.next_y += self.row_h.max(h) + 1;
            self.row_h = 0;
        }
        if self.next_y + h > ATLAS {
            return None;
        }
        self.row_h = self.row_h.max(h);
        let slot = Slot {
            x: self.next_x,
            y: self.next_y,
            w,
            h,
            advance: w as f32,
            x_off: 0.0,
        };
        self.next_x += w + 1;
        self.pending.push((slot, buf));
        Some(slot)
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

    /// Convert visible [`TextRun`]s into GPU [`GlyphInst`] quads, culling offscreen runs outside
    /// `[0, viewport_h]` so tall rendered documents scroll at locked 120fps.
    pub fn rasterize_runs(&mut self, runs: &[TextRun], scale: f64, viewport_h: f32) -> Vec<GlyphInst> {
        let mut glyphs = Vec::with_capacity(runs.len().min(256) * 12);
        let inv_atlas = 1.0 / ATLAS as f32;
        let inv_scale = 1.0 / (scale as f32).max(1.0);
        let cw = self.metrics.cell_w as f32 * inv_scale;
        let ch = self.metrics.cell_h as f32 * inv_scale;

        for run in runs {
            if run.y + ch * 1.6 < 0.0 || run.y > viewport_h + ch {
                continue;
            }
            let mut x = run.x;
            let (space_adv, y_nudge) = match run.face {
                TextFace::Mono => (cw, 0.0),
                TextFace::MonoSmall => (
                    self.mono_small_cell_w as f32 * inv_scale,
                    self.mono_small_m.y_nudge_px * inv_scale,
                ),
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

            if run.face == TextFace::Mono {
                let style = (u8::from(run.bold)) | (u8::from(run.italic) << 1);
                let bytes = run.text.as_bytes();
                let mut i = 0usize;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b == b' ' || b == b'\t' {
                        x += space_adv;
                        i += 1;
                        continue;
                    }
                    if b < 0x20 {
                        i += 1;
                        continue;
                    }
                    // Shape multi-char ASCII operator sequences (`->`, `-->`, `=>`, `!=`, `|>`,
                    // `<=`, `>=`, `::`, `==`, etc.) via CoreText OpenType `calt`/`liga`!
                    if is_ligature_op(b) {
                        let mut end = i + 1;
                        while end < bytes.len() && end - i < 4 && is_ligature_op(bytes[end]) {
                            end += 1;
                        }
                        if end - i >= 2 {
                            let mut matched = None;
                            for cand_len in (2..=(end - i)).rev() {
                                if let Some(s) = self.ligature_slot(style, &bytes[i..i + cand_len]) {
                                    matched = Some((cand_len, s));
                                    break;
                                }
                            }
                            if let Some((cand_len, s)) = matched {
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
                                i += cand_len;
                                continue;
                            }
                        }
                    }

                    let Some(ch) = run.text[i..].chars().next() else {
                        break;
                    };
                    i += ch.len_utf8();
                    let wide = is_wide_char(ch);
                    let key = Key { ch, face: TextFace::Mono, bold: run.bold, italic: run.italic, wide };
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
                        x += if wide { cw * 2.0 } else { cw };
                    }
                }
                continue;
            }

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
            TextFace::MonoSmall => (
                self.mono_small[usize::from(key.bold)].clone(),
                self.mono_small_m.h,
                self.mono_small_m.baseline,
            ),
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
        } else if key.face == TextFace::MonoSmall {
            let cw = if key.wide { self.mono_small_cell_w * 2 } else { self.mono_small_cell_w };
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
