//! Pure-Rust immediate-mode WebGPU Draw List builder.
//!
//! Emits two flat, zero-copy `#[repr(C)]` instance arrays directly in WASM linear memory:
//!   1. `rects: Vec<RectInst>`  (48 bytes each) -> SDF AA rounded rects, stroked borders, AA lines/Beziers, circles.
//!   2. `glyphs: Vec<GlyphInst>` (48 bytes each) -> Instanced HiDPI `Berkeley Mono` text from a 16x6 GPU atlas.
//!
//! The tiny TypeScript shim simply calls `device.queue.writeBuffer(...)` on these two slices
//! and issues 2 WebGPU instanced draw calls per frame (120 FPS, zero DOM).

pub const ATLAS_COLS: f32 = 16.0;
pub const ATLAS_ROWS: f32 = 8.0;
pub const ATLAS_FIRST_CHAR: u8 = 32;
pub const ATLAS_LAST_CHAR: u8 = 126;

/// Double-width OpenType ligatures baked into Row 7 of the `Berkeley Mono` GPU atlas
/// (`font-feature-settings: "liga" 1, "calt" 1`).
const LIGATURES: [(&[u8; 2], usize); 7] = [
    (b"->", 0),
    (b"=>", 1),
    (b"!=", 3),
    (b"==", 4),
    (b"<=", 5),
    (b">=", 6),
    (b"..", 7),
];

/// Palette matching `site/styles/tokens.css` (ix.dev/blaze monochrome obsidian + fire).
pub mod col {
    pub type Rgba = [f32; 4];

    pub const BG: Rgba = [0.063, 0.063, 0.067, 1.0];       // #101011
    pub const BG2: Rgba = [0.078, 0.078, 0.082, 1.0];      // #141415
    pub const BG3: Rgba = [0.098, 0.098, 0.102, 1.0];      // #19191a
    pub const BG4: Rgba = [0.133, 0.133, 0.141, 1.0];      // #222224
    pub const CARD_HOV: Rgba = [0.115, 0.115, 0.125, 1.0];
    pub const LINE: Rgba = [0.153, 0.153, 0.165, 1.0];     // #27272a
    pub const LINE2: Rgba = [0.220, 0.220, 0.235, 1.0];    // #38383c
    pub const INK1: Rgba = [0.922, 0.918, 0.902, 1.0];     // #ebeae6
    pub const INK2: Rgba = [0.702, 0.698, 0.686, 1.0];     // #b3b2af
    pub const INK3: Rgba = [0.490, 0.486, 0.478, 1.0];     // #7d7c7a
    pub const INK4: Rgba = [0.322, 0.322, 0.333, 1.0];     // #525255

    pub const FIRE: Rgba = [0.878, 0.275, 0.102, 1.0];     // #e0461a
    pub const FIRE_SOFT: Rgba = [0.878, 0.275, 0.102, 0.16];
    pub const FIRE_DIM: Rgba = [0.878, 0.275, 0.102, 0.45];
    pub const OK: Rgba = [0.298, 0.761, 0.478, 1.0];       // #4cc27a
    pub const OK_SOFT: Rgba = [0.298, 0.761, 0.478, 0.14];
    pub const WARN: Rgba = [0.910, 0.643, 0.227, 1.0];     // #e8a43a
    pub const DANGER: Rgba = [1.0, 0.443, 0.408, 1.0];     // #ff7168
    pub const DANGER_SOFT: Rgba = [1.0, 0.443, 0.408, 0.14];

    pub const SERIES1: Rgba = [0.851, 0.349, 0.149, 1.0];  // #d95926
    pub const SERIES2: Rgba = [0.224, 0.529, 0.898, 1.0];  // #3987e5
    pub const SERIES2_SOFT: Rgba = [0.224, 0.529, 0.898, 0.15];
    pub const SERIES3: Rgba = [0.098, 0.620, 0.439, 1.0];  // #199e70
    pub const SERIES4: Rgba = [0.498, 0.494, 0.510, 1.0];  // #7f7e82

    // Syntax colors for read-only Rust source peek
    pub const SYN_KW: Rgba = [0.941, 0.520, 0.340, 1.0];   // warm fire keyword
    pub const SYN_TY: Rgba = [0.430, 0.720, 0.960, 1.0];   // cool blue type
    pub const SYN_STR: Rgba = [0.470, 0.820, 0.580, 1.0];  // emerald string
    pub const SYN_NUM: Rgba = [0.910, 0.710, 0.380, 1.0];  // amber number
    pub const SYN_CMT: Rgba = [0.470, 0.470, 0.490, 1.0];  // muted comment
    pub const SYN_MAC: Rgba = [0.760, 0.600, 0.940, 1.0];  // violet macro/attr

    pub const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];

    pub fn with_alpha(c: Rgba, a: f32) -> Rgba {
        [c[0], c[1], c[2], a]
    }
}

/// Instanced SDF primitive (48 bytes, matches WGSL `struct RectInst`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RectInst {
    /// CSS pixel coordinates: `(x0, y0, x1, y1)` (or endpoints for line segments).
    pub rect: [f32; 4],
    /// Linear/sRGB RGBA color.
    pub color: [f32; 4],
    /// `(p0, p1, p2, kind)`:
    ///   - `kind = 0.0`: filled SDF rounded rect (`p0 = radius_px`)
    ///   - `kind = 1.0`: AA SDF line segment (`p0 = thickness_px`)
    ///   - `kind = 2.0`: AA SDF circle (`p0 = radius_px`)
    ///   - `kind = 3.0`: stroked SDF rounded rect border (`p0 = radius_px`, `p1 = border_px`)
    pub style: [f32; 4],
}

/// Instanced HiDPI glyph (48 bytes, matches WGSL `struct GlyphInst`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GlyphInst {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
}

#[derive(Clone, Copy)]
pub struct ClipRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl ClipRect {
    pub fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

pub const MAX_RECTS: usize = 32_768;
pub const MAX_GLYPHS: usize = 49_152;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Flame,
    Timeline,
    Graph,
    Cpu,
    Pulse,
    Memory,
    Layers,
    Bolt,
    Lock,
    Code,
    Pan,
    SelectRange,
    ZoomIn,
    ZoomOut,
    Pause,
    Play,
    Trash,
    Grid,
    Split,
    Expand,
    Restore,
    Close,
}

pub struct DrawList {
    pub rects: Vec<RectInst>,
    pub glyphs: Vec<GlyphInst>,
    pub ov_rects: Vec<RectInst>,
    pub ov_glyphs: Vec<GlyphInst>,
    pub overlay_mode: bool,
    pub width: f32,
    pub height: f32,
    /// Monospace advance width as fraction of font height (measured from Berkeley Mono, ~0.60).
    pub advance_ratio: f32,
    clips: Vec<ClipRect>,
}

impl Default for DrawList {
    fn default() -> Self {
        Self::new()
    }
}

impl DrawList {
    pub fn new() -> Self {
        Self {
            rects: Vec::with_capacity(4096),
            glyphs: Vec::with_capacity(8192),
            ov_rects: Vec::with_capacity(128),
            ov_glyphs: Vec::with_capacity(1024),
            overlay_mode: false,
            width: 1280.0,
            height: 800.0,
            advance_ratio: 0.605,
            clips: Vec::with_capacity(8),
        }
    }

    pub fn begin(&mut self, w: f32, h: f32) {
        self.width = w.max(1.0);
        self.height = h.max(1.0);
        self.rects.clear();
        self.glyphs.clear();
        self.ov_rects.clear();
        self.ov_glyphs.clear();
        self.overlay_mode = false;
        self.clips.clear();
        self.clips.push(ClipRect {
            x0: 0.0,
            y0: 0.0,
            x1: self.width,
            y1: self.height,
        });
    }

    #[inline]
    pub fn begin_overlay(&mut self) {
        self.overlay_mode = true;
    }

    #[inline]
    pub fn end_overlay(&mut self) {
        self.overlay_mode = false;
    }

    #[inline]
    fn push_rect_inst(&mut self, inst: RectInst) {
        let target = if self.overlay_mode {
            &mut self.ov_rects
        } else {
            &mut self.rects
        };
        if target.len() < MAX_RECTS {
            target.push(inst);
        }
    }

    #[inline]
    fn push_glyph_inst(&mut self, inst: GlyphInst) {
        let target = if self.overlay_mode {
            &mut self.ov_glyphs
        } else {
            &mut self.glyphs
        };
        if target.len() < MAX_GLYPHS {
            target.push(inst);
        }
    }

    #[inline]
    pub fn cur_clip(&self) -> ClipRect {
        *self.clips.last().unwrap_or(&ClipRect {
            x0: 0.0,
            y0: 0.0,
            x1: self.width,
            y1: self.height,
        })
    }

    pub fn push_clip(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let next = self.cur_clip().intersect(ClipRect {
            x0: x,
            y0: y,
            x1: x + w,
            y1: y + h,
        });
        self.clips.push(next);
    }

    pub fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    /// Filled sharp rectangle (clipped).
    #[inline]
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: col::Rgba) {
        self.rrect(x, y, w, h, 0.0, color);
    }

    /// Filled anti-aliased rounded rectangle (clipped).
    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: col::Rgba) {
        if self.rects.len() >= MAX_RECTS || w <= 0.0 || h <= 0.0 || color[3] <= 0.001 {
            return;
        }
        let c = self.cur_clip();
        let x0 = x.max(c.x0);
        let y0 = y.max(c.y0);
        let x1 = (x + w).min(c.x1);
        let y1 = (y + h).min(c.y1);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let r = radius.min((x1 - x0) * 0.5).min((y1 - y0) * 0.5);
        self.push_rect_inst(RectInst {
            rect: [x0, y0, x1, y1],
            color,
            style: [r, 0.0, 0.0, 0.0],
        });
    }

    /// Stroked anti-aliased rounded rectangle border (clipped).
    pub fn border(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, border_px: f32, color: col::Rgba) {
        if w <= 0.0 || h <= 0.0 || color[3] <= 0.001 {
            return;
        }
        let c = self.cur_clip();
        if x + w < c.x0 || x > c.x1 || y + h < c.y0 || y > c.y1 {
            return;
        }
        let r = radius.min(w * 0.5).min(h * 0.5);
        self.push_rect_inst(RectInst {
            rect: [x, y, x + w, y + h],
            color,
            style: [r, border_px, 0.0, 3.0],
        });
    }

    /// Card helper: rounded fill + crisp 1px border.
    #[inline]
    pub fn card(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, fill: col::Rgba, stroke: col::Rgba) {
        self.rrect(x, y, w, h, radius, fill);
        self.border(x, y, w, h, radius, 1.0, stroke);
    }

    /// Anti-aliased line segment (clipped loosely against active clip rect).
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, color: col::Rgba) {
        if color[3] <= 0.001 {
            return;
        }
        let c = self.cur_clip();
        if x0.max(x1) < c.x0 - thickness || x0.min(x1) > c.x1 + thickness {
            return;
        }
        if y0.max(y1) < c.y0 - thickness || y0.min(y1) > c.y1 + thickness {
            return;
        }
        self.push_rect_inst(RectInst {
            rect: [x0, y0, x1, y1],
            color,
            style: [thickness, 0.0, 0.0, 1.0],
        });
    }

    /// Dashed anti-aliased line (great for Tracy lock dependency arrows).
    pub fn dashed_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        thickness: f32,
        dash: f32,
        gap: f32,
        color: col::Rgba,
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < 1.0 {
            return;
        }
        let ux = dx / dist;
        let uy = dy / dist;
        let step = (dash + gap).max(2.0);
        let mut t = 0.0;
        while t < dist {
            let t1 = (t + dash).min(dist);
            self.line(x0 + ux * t, y0 + uy * t, x0 + ux * t1, y0 + uy * t1, thickness, color);
            t += step;
        }
    }

    /// Smooth cubic Bezier curve rendered as anti-aliased GPU line segments.
    pub fn cubic_bezier(
        &mut self,
        x0: f32,
        y0: f32,
        cx0: f32,
        cy0: f32,
        cx1: f32,
        cy1: f32,
        x1: f32,
        y1: f32,
        thickness: f32,
        color: col::Rgba,
    ) {
        const SEGS: usize = 16;
        let mut px = x0;
        let mut py = y0;
        for i in 1..=SEGS {
            let t = i as f32 / SEGS as f32;
            let u = 1.0 - t;
            let nx = u * u * u * x0 + 3.0 * u * u * t * cx0 + 3.0 * u * t * t * cx1 + t * t * t * x1;
            let ny = u * u * u * y0 + 3.0 * u * u * t * cy0 + 3.0 * u * t * t * cy1 + t * t * t * y1;
            self.line(px, py, nx, ny, thickness, color);
            px = nx;
            py = ny;
        }
    }

    /// Anti-aliased circle.
    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, color: col::Rgba) {
        if radius <= 0.0 || color[3] <= 0.001 {
            return;
        }
        let c = self.cur_clip();
        if cx + radius < c.x0 || cx - radius > c.x1 || cy + radius < c.y0 || cy - radius > c.y1 {
            return;
        }
        self.push_rect_inst(RectInst {
            rect: [cx - radius, cy - radius, cx + radius, cy + radius],
            color,
            style: [radius, 0.0, 0.0, 2.0],
        });
    }

    /// Approximate width in CSS px of `text` at `size` px.
    #[inline]
    pub fn measure_text(&self, text: &str, size: f32) -> f32 {
        text.chars().count() as f32 * (size * self.advance_ratio)
    }

    /// Draw crisp `Berkeley Mono` text using the GPU glyph atlas (auto-clipped to `max_w` with `...`).
    pub fn text(&mut self, x: f32, y: f32, size: f32, color: col::Rgba, text: &str) -> f32 {
        self.text_max(x, y, size, color, text, f32::INFINITY)
    }

    pub fn text_max(&mut self, x: f32, y: f32, size: f32, color: col::Rgba, text: &str, max_w: f32) -> f32 {
        if max_w < 4.0 || text.is_empty() || color[3] <= 0.001 {
            return 0.0;
        }
        let c = self.cur_clip();
        let scale = size / 28.0;
        let gw = 24.0 * scale;
        let gh = 40.0 * scale;
        let lig_w = 48.0 * scale;
        let y_off = y - size * 0.14;
        if y_off + gh < c.y0 || y_off > c.y1 {
            return 0.0;
        }

        let adv = size * self.advance_ratio;
        let pad_x = (gw - adv) * 0.5;
        let lig_pad_x = (lig_w - adv * 2.0) * 0.5;
        let max_chars = if max_w.is_finite() {
            (max_w / adv).floor().max(0.0) as usize
        } else {
            usize::MAX
        };
        if max_chars == 0 {
            return 0.0;
        }

        let chars: Vec<char> = text.chars().collect();
        let total_chars = chars.len();
        let truncate = total_chars > max_chars;
        let emit_chars = if truncate { max_chars.saturating_sub(1) } else { total_chars };

        let mut cx = x;
        let mut i = 0;
        while i < emit_chars {
            // Check 2-char Berkeley Mono OpenType ligatures (`->`, `=>`, `::`, `!=`, `==`, `<=`, `>=`, `..`)
            if i + 1 < emit_chars {
                if let Some(lig_idx) = match_ligature(chars[i], chars[i + 1]) {
                    if cx + adv * 2.0 >= c.x0 && cx <= c.x1 {
                        self.push_ligature(cx - lig_pad_x, y_off, lig_w, gh, lig_idx, color);
                    }
                    cx += adv * 2.0;
                    i += 2;
                    continue;
                }
            }
            if cx + adv >= c.x0 && cx <= c.x1 {
                if let Some(slot) = map_atlas_slot(chars[i]) {
                    self.push_slot(cx - pad_x, y_off, gw, gh, slot, color);
                }
            }
            cx += adv;
            i += 1;
        }
        if truncate && max_chars >= 1 && cx + adv >= c.x0 && cx <= c.x1 {
            self.push_slot(cx - pad_x, y_off, gw, gh, 101, color); // real ellipsis `…`
            cx += adv;
        }
        cx - x
    }

    #[inline]
    fn push_slot(&mut self, x: f32, y: f32, w: f32, h: f32, slot: usize, color: col::Rgba) {
        if slot == 0 {
            return;
        }
        let col_i = (slot % ATLAS_COLS as usize) as f32;
        let row_i = (slot / ATLAS_COLS as usize) as f32;
        let u0 = col_i / ATLAS_COLS;
        let v0 = row_i / ATLAS_ROWS;
        let u1 = (col_i + 1.0) / ATLAS_COLS;
        let v1 = (row_i + 1.0) / ATLAS_ROWS;
        self.push_glyph_inst(GlyphInst {
            rect: [x, y, x + w, y + h],
            uv: [u0, v0, u1, v1],
            color,
        });
    }

    #[inline]
    fn push_ligature(&mut self, x: f32, y: f32, w: f32, h: f32, lig_idx: usize, color: col::Rgba) {
        let col_i = (lig_idx * 2) as f32;
        let row_i = 7.0;
        let u0 = col_i / ATLAS_COLS;
        let v0 = row_i / ATLAS_ROWS;
        let u1 = (col_i + 2.0) / ATLAS_COLS;
        let v1 = (row_i + 1.0) / ATLAS_ROWS;
        self.push_glyph_inst(GlyphInst {
            rect: [x, y, x + w, y + h],
            uv: [u0, v0, u1, v1],
            color,
        });
    }

    /// Rounded pill badge with border and centered label; returns rendered width.
    pub fn badge(
        &mut self,
        x: f32,
        y: f32,
        h: f32,
        size: f32,
        text: &str,
        fg: col::Rgba,
        bg: col::Rgba,
        stroke: col::Rgba,
    ) -> f32 {
        let tw = self.measure_text(text, size);
        let w = tw + 12.0;
        self.card(x, y, w, h, 4.0, bg, stroke);
        let ty = y + (h - size * 1.25) * 0.5;
        self.text(x + 6.0, ty, size, fg, text);
        w
    }

    /// Bespoke anti-aliased WebGPU vector icon library (zero icon fonts, zero DOM SVGs).
    /// Drawn in CSS pixel box `(x, y, s, s)` using crisp SDF lines, circles, and rounded rects.
    pub fn icon(&mut self, kind: Icon, x: f32, y: f32, s: f32, color: col::Rgba) {
        let t = (s * 0.115).clamp(1.15, 1.85);
        let cx = x + s * 0.5;
        let cy = y + s * 0.5;
        match kind {
            Icon::Flame => {
                // Stylized two-tongue flame silhouette
                self.circle(cx, y + s * 0.62, s * 0.30, color);
                self.line(cx - s * 0.24, y + s * 0.52, cx + s * 0.04, y + s * 0.14, t * 1.3, color);
                self.line(cx + s * 0.24, y + s * 0.52, cx + s * 0.04, y + s * 0.14, t * 1.3, color);
                self.circle(cx, y + s * 0.66, s * 0.14, col::BG);
            }
            Icon::Timeline => {
                // Staggered horizontal trace bars
                self.rrect(x + s * 0.08, y + s * 0.16, s * 0.56, s * 0.18, 1.5, color);
                self.rrect(x + s * 0.28, y + s * 0.42, s * 0.64, s * 0.18, 1.5, col::with_alpha(color, 0.78));
                self.rrect(x + s * 0.14, y + s * 0.68, s * 0.46, s * 0.18, 1.5, col::with_alpha(color, 0.6));
            }
            Icon::Graph => {
                // 3 semantic nodes + 2 directed edges
                let n0 = (x + s * 0.22, y + s * 0.26);
                let n1 = (x + s * 0.78, y + s * 0.26);
                let n2 = (x + s * 0.78, y + s * 0.76);
                self.line(n0.0, n0.1, n1.0, n1.1, t, col::with_alpha(color, 0.7));
                self.line(n0.0, n0.1, n2.0, n2.1, t, col::with_alpha(color, 0.7));
                self.circle(n0.0, n0.1, s * 0.15, color);
                self.circle(n1.0, n1.1, s * 0.15, color);
                self.circle(n2.0, n2.1, s * 0.15, color);
            }
            Icon::Cpu => {
                // Microchip square + 8 edge pins
                let b = s * 0.24;
                let w = s * 0.52;
                self.border(x + b, y + b, w, w, 2.0, t, color);
                self.rrect(x + s * 0.39, y + s * 0.39, s * 0.22, s * 0.22, 1.0, color);
                for o in [0.38, 0.62] {
                    self.line(x + s * o, y + s * 0.08, x + s * o, y + b, t, color);
                    self.line(x + s * o, y + b + w, x + s * o, y + s * 0.92, t, color);
                    self.line(x + s * 0.08, y + s * o, x + b, y + s * o, t, color);
                    self.line(x + b + w, y + s * o, x + s * 0.92, y + s * o, t, color);
                }
            }
            Icon::Pulse => {
                // Oscilloscope hot-spot pulse
                let pts = [
                    (0.06, 0.54),
                    (0.28, 0.54),
                    (0.40, 0.20),
                    (0.56, 0.82),
                    (0.70, 0.44),
                    (0.94, 0.44),
                ];
                for w in pts.windows(2) {
                    self.line(x + s * w[0].0, y + s * w[0].1, x + s * w[1].0, y + s * w[1].1, t, color);
                }
            }
            Icon::Memory => {
                // Heap allocation blocks (2x2 grid with active cell)
                self.rrect(x + s * 0.12, y + s * 0.14, s * 0.34, s * 0.32, 1.5, color);
                self.border(x + s * 0.54, y + s * 0.14, s * 0.34, s * 0.32, 1.5, t, color);
                self.border(x + s * 0.12, y + s * 0.54, s * 0.34, s * 0.32, 1.5, t, color);
                self.rrect(x + s * 0.54, y + s * 0.54, s * 0.34, s * 0.32, 1.5, col::with_alpha(color, 0.65));
            }
            Icon::Layers => {
                // Stacked build phases / diamonds
                self.line(cx, y + s * 0.14, x + s * 0.88, y + s * 0.36, t, color);
                self.line(x + s * 0.88, y + s * 0.36, cx, y + s * 0.56, t, color);
                self.line(cx, y + s * 0.56, x + s * 0.12, y + s * 0.36, t, color);
                self.line(x + s * 0.12, y + s * 0.36, cx, y + s * 0.14, t, color);
                self.line(x + s * 0.14, y + s * 0.60, cx, y + s * 0.80, t, col::with_alpha(color, 0.8));
                self.line(cx, y + s * 0.80, x + s * 0.86, y + s * 0.60, t, col::with_alpha(color, 0.8));
            }
            Icon::Bolt => {
                // Sharp Advisor lightning bolt
                self.line(x + s * 0.56, y + s * 0.10, x + s * 0.24, y + s * 0.54, t * 1.15, color);
                self.line(x + s * 0.24, y + s * 0.54, x + s * 0.54, y + s * 0.54, t * 1.15, color);
                self.line(x + s * 0.54, y + s * 0.54, x + s * 0.42, y + s * 0.90, t * 1.15, color);
                self.line(x + s * 0.42, y + s * 0.90, x + s * 0.76, y + s * 0.46, t * 1.15, color);
                self.line(x + s * 0.76, y + s * 0.46, x + s * 0.46, y + s * 0.46, t * 1.15, color);
            }
            Icon::Lock => {
                // Padlock body + shackle
                self.border(x + s * 0.18, y + s * 0.46, s * 0.64, s * 0.42, 2.0, t, color);
                self.line(x + s * 0.32, y + s * 0.46, x + s * 0.32, y + s * 0.26, t, color);
                self.line(x + s * 0.68, y + s * 0.46, x + s * 0.68, y + s * 0.26, t, color);
                self.line(x + s * 0.32, y + s * 0.26, x + s * 0.42, y + s * 0.16, t, color);
                self.line(x + s * 0.42, y + s * 0.16, x + s * 0.58, y + s * 0.16, t, color);
                self.line(x + s * 0.58, y + s * 0.16, x + s * 0.68, y + s * 0.26, t, color);
                self.circle(cx, y + s * 0.67, s * 0.08, color);
            }
            Icon::Code => {
                // `< >` inspector chevrons
                self.line(x + s * 0.36, y + s * 0.26, x + s * 0.14, cy, t, color);
                self.line(x + s * 0.14, cy, x + s * 0.36, y + s * 0.74, t, color);
                self.line(x + s * 0.64, y + s * 0.26, x + s * 0.86, cy, t, color);
                self.line(x + s * 0.86, cy, x + s * 0.64, y + s * 0.74, t, color);
                self.line(x + s * 0.55, y + s * 0.22, x + s * 0.45, y + s * 0.78, t, col::with_alpha(color, 0.7));
            }
            Icon::Pan => {
                // 4-way move/pan arrows
                self.line(x + s * 0.16, cy, x + s * 0.84, cy, t, color);
                self.line(cx, y + s * 0.16, cx, y + s * 0.84, t, color);
                self.line(x + s * 0.16, cy, x + s * 0.30, cy - s * 0.14, t, color);
                self.line(x + s * 0.16, cy, x + s * 0.30, cy + s * 0.14, t, color);
                self.line(x + s * 0.84, cy, x + s * 0.70, cy - s * 0.14, t, color);
                self.line(x + s * 0.84, cy, x + s * 0.70, cy + s * 0.14, t, color);
            }
            Icon::SelectRange => {
                // Range selection brackets `[ | ]`
                self.line(x + s * 0.22, y + s * 0.18, x + s * 0.22, y + s * 0.82, t, color);
                self.line(x + s * 0.78, y + s * 0.18, x + s * 0.78, y + s * 0.82, t, color);
                self.rrect(x + s * 0.26, y + s * 0.26, s * 0.48, s * 0.48, 1.5, col::with_alpha(color, 0.32));
                self.line(x + s * 0.22, y + s * 0.18, x + s * 0.36, y + s * 0.18, t, color);
                self.line(x + s * 0.22, y + s * 0.82, x + s * 0.36, y + s * 0.82, t, color);
                self.line(x + s * 0.64, y + s * 0.18, x + s * 0.78, y + s * 0.18, t, color);
                self.line(x + s * 0.64, y + s * 0.82, x + s * 0.78, y + s * 0.82, t, color);
            }
            Icon::ZoomIn | Icon::ZoomOut => {
                // Magnifying lens + handle + `+` or `-`
                let lx = x + s * 0.44;
                let ly = y + s * 0.44;
                let lr = s * 0.28;
                self.border(lx - lr, ly - lr, lr * 2.0, lr * 2.0, lr, t, color);
                self.line(lx + lr * 0.7, ly + lr * 0.7, x + s * 0.86, y + s * 0.86, t * 1.15, color);
                self.line(lx - s * 0.13, ly, lx + s * 0.13, ly, t, color);
                if matches!(kind, Icon::ZoomIn) {
                    self.line(lx, ly - s * 0.13, lx, ly + s * 0.13, t, color);
                }
            }
            Icon::Pause => {
                self.rrect(x + s * 0.24, y + s * 0.20, s * 0.18, s * 0.60, 1.2, color);
                self.rrect(x + s * 0.58, y + s * 0.20, s * 0.18, s * 0.60, 1.2, color);
            }
            Icon::Play => {
                self.line(x + s * 0.28, y + s * 0.20, x + s * 0.28, y + s * 0.80, t * 1.2, color);
                self.line(x + s * 0.28, y + s * 0.20, x + s * 0.80, cy, t * 1.2, color);
                self.line(x + s * 0.28, y + s * 0.80, x + s * 0.80, cy, t * 1.2, color);
            }
            Icon::Trash => {
                self.line(x + s * 0.18, y + s * 0.26, x + s * 0.82, y + s * 0.26, t, color);
                self.line(x + s * 0.38, y + s * 0.14, x + s * 0.62, y + s * 0.14, t, color);
                self.border(x + s * 0.26, y + s * 0.28, s * 0.48, s * 0.56, 1.5, t, color);
            }
            Icon::Grid => {
                // 4-tile layout grid
                let g = s * 0.32;
                self.border(x + s * 0.14, y + s * 0.14, g, g, 1.5, t, color);
                self.border(x + s * 0.54, y + s * 0.14, g, g, 1.5, t, color);
                self.border(x + s * 0.14, y + s * 0.54, g, g, 1.5, t, color);
                self.rrect(x + s * 0.54, y + s * 0.54, g, g, 1.5, color);
            }
            Icon::Split => {
                // Split pane icon
                self.border(x + s * 0.14, y + s * 0.18, s * 0.72, s * 0.64, 2.0, t, color);
                self.line(cx, y + s * 0.20, cx, y + s * 0.80, t, color);
            }
            Icon::Expand => {
                // Outward diagonal arrows
                self.line(x + s * 0.58, y + s * 0.18, x + s * 0.82, y + s * 0.18, t, color);
                self.line(x + s * 0.82, y + s * 0.18, x + s * 0.82, y + s * 0.42, t, color);
                self.line(x + s * 0.82, y + s * 0.18, x + s * 0.56, y + s * 0.44, t, color);
                self.line(x + s * 0.42, y + s * 0.82, x + s * 0.18, y + s * 0.82, t, color);
                self.line(x + s * 0.18, y + s * 0.82, x + s * 0.18, y + s * 0.58, t, color);
                self.line(x + s * 0.18, y + s * 0.82, x + s * 0.44, y + s * 0.56, t, color);
            }
            Icon::Restore => {
                // Inward diagonal collapse
                self.line(x + s * 0.56, y + s * 0.44, x + s * 0.80, y + s * 0.44, t, color);
                self.line(x + s * 0.56, y + s * 0.44, x + s * 0.56, y + s * 0.20, t, color);
                self.line(x + s * 0.56, y + s * 0.44, x + s * 0.84, y + s * 0.16, t, color);
                self.line(x + s * 0.44, y + s * 0.56, x + s * 0.20, y + s * 0.56, t, color);
                self.line(x + s * 0.44, y + s * 0.56, x + s * 0.44, y + s * 0.80, t, color);
                self.line(x + s * 0.44, y + s * 0.56, x + s * 0.16, y + s * 0.84, t, color);
            }
            Icon::Close => {
                self.line(x + s * 0.24, y + s * 0.24, x + s * 0.76, y + s * 0.76, t, color);
                self.line(x + s * 0.76, y + s * 0.24, x + s * 0.24, y + s * 0.76, t, color);
            }
        }
    }

    /// Renders one line of Rust source code with crisp syntax highlighting directly into WebGPU glyphs.
    pub fn rust_code_line(&mut self, x: f32, y: f32, size: f32, max_w: f32, code: &str) {
        let adv = size * self.advance_ratio;
        let bytes = code.as_bytes();
        let len = bytes.len();
        let mut cx = x;
        let limit_x = x + max_w;
        let mut off = 0usize;
        let mut toks = blaze_lex::tokenize(code, blaze_lex::FrontmatterAllowed::No, blaze_lex::Cpu::SCALAR).peekable();
        let mut attr_depth = 0i32;

        while let Some(tok) = toks.next() {
            if cx + adv > limit_x {
                break;
            }
            let end = (off + tok.len as usize).min(len);
            let slice = &code[off..end];
            let prev_off = off;
            off = end;

            match tok.kind {
                blaze_lex::TokenKind::Whitespace => {
                    for ch in slice.chars() {
                        cx += if ch == '\t' { adv * 2.0 } else { adv };
                    }
                }
                blaze_lex::TokenKind::LineComment { .. } | blaze_lex::TokenKind::BlockComment { .. } => {
                    cx += self.text_max(cx, y, size, col::SYN_CMT, slice, limit_x - cx);
                }
                blaze_lex::TokenKind::Literal { kind, .. } => {
                    let c = match kind {
                        blaze_lex::LiteralKind::Int { .. } | blaze_lex::LiteralKind::Float { .. } => col::SYN_NUM,
                        _ => col::SYN_STR,
                    };
                    cx += self.text_max(cx, y, size, c, slice, limit_x - cx);
                }
                blaze_lex::TokenKind::Lifetime { .. } | blaze_lex::TokenKind::RawLifetime => {
                    cx += self.text_max(cx, y, size, col::SYN_TY, slice, limit_x - cx);
                }
                blaze_lex::TokenKind::Ident | blaze_lex::TokenKind::RawIdent => {
                    let is_macro = matches!(toks.peek().map(|t| t.kind), Some(blaze_lex::TokenKind::Bang));
                    let color = if attr_depth > 0 || is_macro {
                        col::SYN_MAC
                    } else if is_rust_kw(slice) {
                        col::SYN_KW
                    } else if is_rust_type(slice) || slice.as_bytes().first().map(|b| b.is_ascii_uppercase()).unwrap_or(false) {
                        col::SYN_TY
                    } else {
                        col::INK1
                    };
                    cx += self.text_max(cx, y, size, color, slice, limit_x - cx);
                }
                blaze_lex::TokenKind::Pound => {
                    let next_k = toks.peek().map(|t| t.kind);
                    if matches!(next_k, Some(blaze_lex::TokenKind::OpenBracket | blaze_lex::TokenKind::Bang)) {
                        attr_depth = attr_depth.max(1);
                    }
                    cx += self.text_max(cx, y, size, col::SYN_MAC, slice, limit_x - cx);
                }
                blaze_lex::TokenKind::OpenBracket => {
                    if attr_depth > 0 {
                        attr_depth += 1;
                    }
                    let c = if attr_depth > 0 { col::SYN_MAC } else { col::INK2 };
                    cx += self.text_max(cx, y, size, c, slice, limit_x - cx);
                }
                blaze_lex::TokenKind::CloseBracket => {
                    let c = if attr_depth > 0 { col::SYN_MAC } else { col::INK2 };
                    if attr_depth > 0 {
                        attr_depth = (attr_depth - 1).max(0);
                        if attr_depth == 1 {
                            attr_depth = 0;
                        }
                    }
                    cx += self.text_max(cx, y, size, c, slice, limit_x - cx);
                }
                _ => {
                    // Combine adjacent 2-char Berkeley Mono OpenType ligatures (`->`, `=>`, `!=`, `==`, `<=`, `>=`, `..`)
                    if slice.len() == 1 && prev_off + 1 < len {
                        if let Some(next_tok) = toks.peek() {
                            if next_tok.len == 1 {
                                if let Some(lig_idx) = match_ligature_bytes(bytes[prev_off], bytes[prev_off + 1]) {
                                    toks.next();
                                    off += 1;
                                    if cx + adv * 2.0 <= limit_x {
                                        let scale = size / 28.0;
                                        let lig_w = 48.0 * scale;
                                        let gh = 40.0 * scale;
                                        let lig_pad_x = (lig_w - adv * 2.0) * 0.5;
                                        self.push_ligature(cx - lig_pad_x, y - size * 0.14, lig_w, gh, lig_idx, col::INK2);
                                    }
                                    cx += adv * 2.0;
                                    continue;
                                }
                            }
                        }
                    }
                    let c = if attr_depth > 0 { col::SYN_MAC } else { col::INK2 };
                    cx += self.text_max(cx, y, size, c, slice, limit_x - cx);
                }
            }
        }
    }
}

#[inline]
fn match_ligature_bytes(a: u8, b: u8) -> Option<usize> {
    for (pair, idx) in LIGATURES {
        if pair[0] == a && pair[1] == b {
            return Some(idx);
        }
    }
    None
}

#[inline]
fn match_ligature(a: char, b: char) -> Option<usize> {
    if a.is_ascii() && b.is_ascii() {
        match_ligature_bytes(a as u8, b as u8)
    } else {
        None
    }
}

fn map_atlas_slot(ch: char) -> Option<usize> {
    match ch {
        '\u{00b7}' => Some(96),  // ·
        '\u{00d7}' => Some(97),  // ×
        '\u{00b5}' => Some(98),  // µ
        '\u{2192}' => Some(99),  // →
        '\u{0394}' => Some(100), // Δ
        '\u{2026}' => Some(101), // …
        '\u{2014}' | '\u{2013}' => Some(102), // —
        '\u{21b5}' => Some(103), // ↵
        '\u{2197}' => Some(104), // ↗
        '\u{2190}' => Some(105), // ←
        c if (c as u32) > 32 && (c as u32) <= 126 => Some((c as u32 - 32) as usize),
        _ => None,
    }
}

fn is_rust_kw(s: &str) -> bool {
    matches!(
        s,
        "fn" | "pub"
            | "let"
            | "mut"
            | "const"
            | "static"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "for"
            | "in"
            | "while"
            | "loop"
            | "if"
            | "else"
            | "match"
            | "return"
            | "break"
            | "continue"
            | "use"
            | "mod"
            | "crate"
            | "self"
            | "Self"
            | "super"
            | "where"
            | "as"
            | "unsafe"
            | "async"
            | "await"
            | "ref"
            | "move"
            | "type"
            | "true"
            | "false"
    )
}

fn is_rust_type(s: &str) -> bool {
    matches!(
        s,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "isize"
            | "f32"
            | "f64"
            | "bool"
            | "str"
            | "char"
    )
}
