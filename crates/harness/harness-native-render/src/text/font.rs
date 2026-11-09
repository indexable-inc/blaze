//! The fonts text is set in, as the platform has them, and all den's typesetter asks of the platform: finding one by
//! name and falling back for what it lacks, its metrics and advances, the data the typesetter shapes from, and drawing
//! a glyph or an outline into coverage. CoreText and CoreGraphics on macOS (text/font_mac.rs); fontdb, skrifa and
//! tiny-skia on Linux (text/font_linux.rs, docs/linux.md). Everything above this (shaping, breaking, the atlas) is the
//! same on both.
use read_fonts::model::Source;

use crate::math::PathSeg;

pub use super::font_mac::Font;

/// A font at one size: what [`super::Fonts`] holds and registers by its PostScript name and size.
pub trait FontSource: Clone + Sized {
    /// The font whose PostScript name is `name` at `size`, if it is installed (never a substitute).
    fn named(name: &str, size: f32) -> Option<Self>;
    /// The regular face of the installed family `family` (Ghostty's font-family: "Berkeley Mono") at `size`, if any.
    fn family(family: &str, size: f32) -> Option<Self>;
    /// The system's UI font at `size`, its emphasized weight when `bold`: text's font when Inter is not installed.
    fn system(size: f32, bold: bool) -> Self;
    /// The system's fixed-pitch font at `size`: code's when Berkeley Mono is not installed.
    fn fixed(size: f32) -> Self;
    /// The font in the file `bytes` at `size`, if it loads.
    fn from_data(bytes: &[u8], size: f32) -> Option<Self>;
    /// This font's family at weight 500, if it has one.
    fn medium(&self, size: f32) -> Option<Self>;
    /// This font with tabular figures, so the digits of a column of numbers line up.
    fn tabular(&self, size: f32) -> Self;
    /// This font's family slanted (and bold when `bold`), if it has such a face.
    fn italic(&self, size: f32, bold: bool) -> Option<Self>;
    /// This font at `size` set on its variation axes at `axes` (tags and values: `wght` 600), if it has them all.
    fn varied(&self, size: f32, axes: &[(u32, f32)]) -> Option<Self>;
    /// The font that sets `text` (a grapheme cluster this one lacks a glyph for) in this one's stead.
    fn fallback(&self, text: &str) -> Self;

    fn post_script_name(&self) -> String;
    fn size(&self) -> f32;
    /// Ascent, descent, cap height and x-height, points.
    fn ascent(&self) -> f32;
    fn descent(&self) -> f32;
    fn cap_height(&self) -> f32;
    fn x_height(&self) -> f32;
    /// The underline's thickness, points (the `post` table's).
    fn underline_thickness(&self) -> f32;
    /// The glyph `ch` maps to, 0 when it has none.
    fn glyph(&self, ch: char) -> u16;
    /// `glyph`'s advance as the platform sets it at this size (with its tracking), points.
    fn advance(&self, glyph: u16) -> f64;

    /// The font data the typesetter reads (and the face index in it), at no particular variation.
    fn source(&self) -> Option<(Source, u32)>;
    /// The OpenType features it was made with (tabular figures), as tags and values.
    fn features(&self) -> Vec<(u32, u32)>;
    /// The variation axes it is set at away from their defaults, as tags and values.
    fn variations(&self) -> Vec<(u32, f32)>;

    /// `glyph`'s ink box in points from its origin, `[x, y, w, h]` (y up), empty when it draws nothing.
    fn bounds(&self, glyph: u16) -> [f64; 4];
    /// Whether its glyphs are colour bitmaps (an emoji font): drawn as they are, not tinted.
    fn colour(&self) -> bool;
    /// `glyph` drawn into `buf`, a `w` by `h` bitmap whose bottom-left pixel is (`x0`, `y0`) in pixels from the glyph's
    /// origin (y up), at `scale` pixels per point and shifted `subx` pixels right: coverage one byte a pixel, or
    /// premultiplied RGBA when [`FontSource::colour`]. Rows top first.
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &self,
        glyph: u16,
        buf: &mut [u8],
        w: usize,
        h: usize,
        x0: i32,
        y0: i32,
        scale: f64,
        subx: f64,
    );
}

/// `segs` (ems, y down) filled (even-odd, each piece on its own) or stroked `stroke` ems wide into `buf`, a `w` by `h`
/// coverage bitmap whose bottom-left pixel is (`x0`, `y0`) in pixels from the outline's origin (y up), at `scale` pixels
/// to the em and shifted `subx` pixels right. Rows top first.
#[allow(clippy::too_many_arguments)]
pub fn fill_outline(segs: &[PathSeg], fill: bool, stroke: f32, buf: &mut [u8], w: usize, h: usize, x0: i32, y0: i32, scale: f64, subx: f64) {
    super::font_mac::fill_outline(segs, fill, stroke, buf, w, h, x0, y0, scale, subx);
}

/// A glyph or outline drawn for the atlas: `w` by `h` premultiplied RGBA pixels (a plain glyph's coverage as white), its
/// left edge `left` pixels right of the origin and its top `top` pixels over the baseline; `colour` when it is drawn as
/// it is, not tinted (an emoji).
pub struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub w: u32,
    pub h: u32,
    pub colour: bool,
    pub rgba: Vec<u8>,
}

impl Bitmap {
    const EMPTY: Self = Self { left: 0, top: 0, w: 0, h: 0, colour: false, rgba: Vec::new() };
}

/// `glyph` of `font` at `scale` pixels to the point, `subx` of a pixel right of the pixel grid; empty when it draws
/// nothing.
pub fn raster_glyph(font: &Font, glyph: u16, scale: f32, subx: f64) -> Bitmap {
    let [bx, by, bw, bh] = font.bounds(glyph);
    if bw <= 0.0 || bh <= 0.0 {
        return Bitmap::EMPTY;
    }
    let colour = font.colour();
    let s = f64::from(scale);
    let x0 = (bx * s + subx).floor() as i32 - 1;
    let y0 = (by * s).floor() as i32 - 1;
    let x1 = ((bx + bw) * s + subx).ceil() as i32 + 1;
    let y1 = ((by + bh) * s).ceil() as i32 + 1;
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut buf = vec![0u8; w * h * if colour { 4 } else { 1 }];
    font.draw(glyph, &mut buf, w, h, x0, y0, s, subx);
    let rgba = if colour { buf } else { buf.iter().flat_map(|&c| [c; 4]).collect() };
    Bitmap { left: x0, top: y1, w: w as u32, h: h as u32, colour, rgba }
}

/// A formula's outline (`segs`, in ems from its origin, y down) at `scale` pixels to the em, `subx` of a pixel right
/// of the grid, filled (each piece on its own, so overlaps do not cancel) or stroked `stroke` ems wide.
pub fn raster_outline(segs: &[PathSeg], fill: bool, stroke: f32, scale: f32, subx: f64) -> Bitmap {
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for s in segs {
        let (points, n) = match *s {
            PathSeg::Move(a) | PathSeg::Line(a) => ([a; 3], 1),
            PathSeg::Quad(a, b) => ([a, b, b], 2),
            PathSeg::Cubic(a, b, c) => ([a, b, c], 3),
            PathSeg::Close => ([[0.0; 2]; 3], 0),
        };
        for p in &points[..n] {
            for k in 0..2 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
    }
    if lo[0] > hi[0] {
        return Bitmap::EMPTY;
    }
    let reach = if fill { 0.0 } else { stroke * 0.5 };
    let s = f64::from(scale);
    let x0 = (f64::from(lo[0] - reach) * s + subx).floor() as i32 - 1;
    let x1 = (f64::from(hi[0] + reach) * s + subx).ceil() as i32 + 1;
    // y is down in a formula and up in a bitmap: the shape's top is its greatest height over the origin.
    let y0 = (-f64::from(hi[1] + reach) * s).floor() as i32 - 1;
    let y1 = (-f64::from(lo[1] - reach) * s).ceil() as i32 + 1;
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut buf = vec![0u8; w * h];
    fill_outline(segs, fill, stroke, &mut buf, w, h, x0, y0, s, subx);
    Bitmap { left: x0, top: y1, w: w as u32, h: h as u32, colour: false, rgba: buf.iter().flat_map(|&c| [c; 4]).collect() }
}
