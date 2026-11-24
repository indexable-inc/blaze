//! Pure `#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]` port of `den`'s Slug
//! GPU quadratic Bézier outline font engine + zero-copy `BZG1` font table unpacker.
//!
//! Algorithm: Eric Lengyel, "GPU-Centered Font Rendering Directly from Glyph Outlines",
//! JCGT 6(2), 2017 (patent dedicated to the public domain; reference shaders MIT/Apache-2.0).
//!
//! Produces `den`'s exact storage-buffer layout:
//! - `curves`: `array<vec4<f32>>`, two `vec4<f32>`s per quadratic Bézier curve:
//!   `(p1.x, p1.y, p2.x, p2.y)` and `(p3.x, p3.y, 0.0, 0.0)`.
//! - `bands`: `array<u32>`, per glyph starting at `gdata[0]`: horizontal band `(count, list_off)`
//!   headers, vertical band `(count, list_off)` headers, then curve slot indices into `curves`
//!   sorted descending by max extent along the ray for early-exit ray termination.

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use crate::error::UiError;

/// Largest distance (ems) a cubic's quadratic replacements may stray from it at the same parameter.
pub const TOLERANCE: f64 = 1.0 / 1024.0;
/// Maximum bands a glyph is cut into along either axis.
pub const MAX_BANDS: usize = 16;
/// Band overlap epsilon (ems) so a curve on a seam belongs to both neighbors.
pub const BAND_EPS: f32 = 1.0 / 1024.0;
/// `u32` words per band header (`count`, `list_offset`).
pub const BAND_HEADER: u32 = 2;
/// `vec4<f32>` slots per quadratic Bézier curve in `curves`.
pub const CURVE_SLOTS: u32 = 2;

/// `BZG1` binary glyph table magic (`'BZG1'` little-endian).
pub const BZG1_MAGIC: u32 = 0x3147_5a42;
pub const BZG1_VERSION: u32 = 1;
pub const BZG1_HEADER_WORDS: usize = 16;
pub const BZG1_INSTANCE_WORDS: usize = 8;
pub const BZG1_DIR_WORDS: usize = 8;
pub const BZG1_NO_OUTLINE: u32 = 0xffff_ffff;

/// A quadratic Bézier curve in em space (y up): `[p1, p2, p3]`. A line is `[p1, p2, p2]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad(pub [[f32; 2]; 3]);

impl Quad {
    #[inline]
    pub const fn line(a: [f32; 2], b: [f32; 2]) -> Self {
        Self([a, b, b])
    }

    #[inline]
    fn coord(p: [f32; 2], axis: usize) -> f32 {
        if axis == 0 { p[0] } else { p[1] }
    }

    #[inline]
    pub fn span(&self, axis: usize) -> (f32, f32) {
        let a = Self::coord(self.0[0], axis);
        let b = Self::coord(self.0[1], axis);
        let c = Self::coord(self.0[2], axis);
        (a.min(b).min(c), a.max(b).max(c))
    }

    #[inline]
    pub fn flat(&self, axis: usize) -> bool {
        let a = Self::coord(self.0[0], axis);
        let b = Self::coord(self.0[1], axis);
        let c = Self::coord(self.0[2], axis);
        a == b && b == c
    }

    #[inline]
    pub fn at(&self, t: f32) -> [f32; 2] {
        let s = 1.0 - t;
        [
            s * s * self.0[0][0] + 2.0 * s * t * self.0[1][0] + t * t * self.0[2][0],
            s * s * self.0[0][1] + 2.0 * s * t * self.0[1][1] + t * t * self.0[2][1],
        ]
    }

    /// Exact curve bounding box `[x0, y0, x1, y1]`.
    pub fn bounds(&self) -> [f32; 4] {
        let axis_bounds = |axis: usize| -> (f32, f32) {
            let a = Self::coord(self.0[0], axis);
            let b = Self::coord(self.0[1], axis);
            let c = Self::coord(self.0[2], axis);
            let mut lo = a.min(c);
            let mut hi = a.max(c);
            let denom = a - 2.0 * b + c;
            if denom != 0.0 {
                let t = (a - b) / denom;
                if t > 0.0 && t < 1.0 {
                    let v = Self::coord(self.at(t), axis);
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            (lo, hi)
        };
        let (x0, x1) = axis_bounds(0);
        let (y0, y1) = axis_bounds(1);
        [x0, y0, x1, y1]
    }

    #[inline]
    fn is_point(&self) -> bool {
        self.0[0] == self.0[1] && self.0[1] == self.0[2]
    }

    #[inline]
    fn reversed(&self) -> Self {
        Self([self.0[2], self.0[1], self.0[0]])
    }
}

/// Signed area enclosed by a closed quadratic contour (positive = counter-clockwise, y up).
pub fn signed_area(contour: &[Quad]) -> f64 {
    let mut sum = 0.0f64;
    for q in contour {
        let c01 = f64::from(q.0[0][0]) * f64::from(q.0[1][1])
            - f64::from(q.0[0][1]) * f64::from(q.0[1][0]);
        let c12 = f64::from(q.0[1][0]) * f64::from(q.0[2][1])
            - f64::from(q.0[1][1]) * f64::from(q.0[2][0]);
        let c02 = f64::from(q.0[0][0]) * f64::from(q.0[2][1])
            - f64::from(q.0[0][1]) * f64::from(q.0[2][0]);
        sum += (2.0 / 3.0) * (c01 + c12) + c02 / 3.0;
    }
    sum * 0.5
}

/// Split cubic Bézier `c` into chained quadratic Béziers within `tol` ems.
pub fn cubic_quads<A: Allocator + Copy>(
    c: [[f64; 2]; 4],
    tol: f64,
    alloc: A,
) -> Result<OwnVec<[[f64; 2]; 3], A>, UiError> {
    let dx = c[3][0] - 3.0 * c[2][0] + 3.0 * c[1][0] - c[0][0];
    let dy = c[3][1] - 3.0 * c[2][1] + 3.0 * c[1][1] - c[0][1];
    let err = 0.048_112_522_432_468_81 * sqrt_f64(dx * dx + dy * dy);
    let ratio = err / tol.max(1e-12);
    let mut n: usize = 1;
    while n < 64 && f64::from(u32::try_from(n * n * n).unwrap_or(u32::MAX)) < ratio {
        n += 1;
    }

    let split = |p: [[f64; 2]; 4], t: f64| -> ([[f64; 2]; 4], [[f64; 2]; 4]) {
        let lerp = |a: [f64; 2], b: [f64; 2]| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        let ab = lerp(p[0], p[1]);
        let bc = lerp(p[1], p[2]);
        let cd = lerp(p[2], p[3]);
        let abc = lerp(ab, bc);
        let bcd = lerp(bc, cd);
        let mid = lerp(abc, bcd);
        ([p[0], ab, abc, mid], [mid, bcd, cd, p[3]])
    };

    let mut out: OwnVec<[[f64; 2]; 3], A> = OwnVec::try_with_capacity_in(n, alloc)?;
    let nf = f64::from(u32::try_from(n).unwrap_or(1));
    let mut rest = c;
    for i in 0..n {
        let t = 1.0 / (nf - f64::from(u32::try_from(i).unwrap_or(0)));
        let (piece, next_rest) = if i + 1 == n { (rest, rest) } else { split(rest, t) };
        rest = next_rest;
        let qx = (3.0 * (piece[1][0] + piece[2][0]) - (piece[0][0] + piece[3][0])) * 0.25;
        let qy = (3.0 * (piece[1][1] + piece[2][1]) - (piece[0][1] + piece[3][1])) * 0.25;
        let mut p0 = piece[0];
        if let Some(prev) = out.last() {
            p0 = prev[2];
        }
        out.try_push([p0, [qx, qy], piece[3]]).map_err(|r| UiError::Alloc(r.why))?;
    }
    Ok(out)
}

fn f64_to_f32(v: f64) -> f32 {
    // Exact IEEE-754 f64 -> f32 rounding without using `as` casts.
    let bits = v.to_bits();
    let sign = u32::try_from((bits >> 63) & 1).unwrap_or(0) << 31;
    let exp = u32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let frac = bits & 0x000f_ffff_ffff_ffff;
    if exp == 0x7ff {
        let nan_bit = if frac != 0 { 0x0040_0000 } else { 0 };
        return f32::from_bits(sign | 0x7f80_0000 | nan_bit);
    }
    if exp == 0 && frac == 0 {
        return f32::from_bits(sign);
    }
    let unbiased = i32::try_from(exp).unwrap_or(0) - 1023;
    let exp32 = unbiased + 127;
    if exp32 >= 255 {
        return f32::from_bits(sign | 0x7f80_0000);
    }
    if exp32 <= 0 {
        return f32::from_bits(sign);
    }
    let mant = u32::try_from(frac >> 29).unwrap_or(0);
    let round_bit = u32::try_from((frac >> 28) & 1).unwrap_or(0);
    let exp_bits = u32::try_from(exp32).unwrap_or(0) << 23;
    f32::from_bits(sign | (exp_bits + mant + round_bit))
}

fn narrow_pt(p: [f64; 2]) -> [f32; 2] {
    [f64_to_f32(p[0]), f64_to_f32(p[1])]
}

fn sqrt_f64(x: f64) -> f64 {
    if x <= 0.0 || !x.is_finite() {
        return 0.0;
    }
    let mut g = f64::from(gpu::math::sqrt(f64_to_f32(x).max(1e-30)));
    for _ in 0..4 {
        g = 0.5 * (g + x / g);
    }
    g
}

/// Path builder converting `move/line/quad/cubic/close` into closed contours of [`Quad`]s.
pub struct PathBuilder<A: Allocator + Copy> {
    alloc: A,
    done: OwnVec<OwnVec<Quad, A>, A>,
    cur: OwnVec<Quad, A>,
    start: [f64; 2],
    at: [f64; 2],
}

impl<A: Allocator + Copy> PathBuilder<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            alloc,
            done: OwnVec::new_in(alloc),
            cur: OwnVec::new_in(alloc),
            start: [0.0, 0.0],
            at: [0.0, 0.0],
        }
    }

    pub fn move_to(&mut self, p: [f64; 2]) -> Result<(), UiError> {
        self.close()?;
        self.start = p;
        self.at = p;
        Ok(())
    }

    pub fn line_to(&mut self, p: [f64; 2]) -> Result<(), UiError> {
        if p != self.at {
            self.cur
                .try_push(Quad::line(narrow_pt(self.at), narrow_pt(p)))
                .map_err(|r| UiError::Alloc(r.why))?;
            self.at = p;
        }
        Ok(())
    }

    pub fn quad_to(&mut self, c: [f64; 2], p: [f64; 2]) -> Result<(), UiError> {
        self.cur
            .try_push(Quad([narrow_pt(self.at), narrow_pt(c), narrow_pt(p)]))
            .map_err(|r| UiError::Alloc(r.why))?;
        self.at = p;
        Ok(())
    }

    pub fn cubic_to(&mut self, c1: [f64; 2], c2: [f64; 2], p: [f64; 2]) -> Result<(), UiError> {
        let quads = cubic_quads([self.at, c1, c2, p], TOLERANCE, self.alloc)?;
        for q in quads.as_slice() {
            self.cur
                .try_push(Quad([narrow_pt(q[0]), narrow_pt(q[1]), narrow_pt(q[2])]))
                .map_err(|r| UiError::Alloc(r.why))?;
        }
        self.at = p;
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), UiError> {
        if !self.cur.is_empty() {
            self.line_to(self.start)?;
            let mut finished = OwnVec::new_in(self.alloc);
            core::mem::swap(&mut finished, &mut self.cur);
            self.done.try_push(finished).map_err(|r| UiError::Alloc(r.why))?;
        }
        self.at = self.start;
        Ok(())
    }

    pub fn finish(mut self) -> Result<OwnVec<OwnVec<Quad, A>, A>, UiError> {
        self.close()?;
        Ok(self.done)
    }
}

/// A glyph's quadratic curves in em space, wound counter-clockwise (y up).
pub struct Outline<A: Allocator + Copy> {
    pub curves: OwnVec<Quad, A>,
    pub bbox: [f32; 4],
}

impl<A: Allocator + Copy> Outline<A> {
    pub fn new(
        mut contours: OwnVec<OwnVec<Quad, A>, A>,
        alloc: A,
    ) -> Result<Option<Self>, UiError> {
        let mut area = 0.0f64;
        for c in contours.as_slice() {
            area += signed_area(c.as_slice());
        }
        if area < 0.0 {
            for c in contours.as_mut_slice() {
                c.as_mut_slice().reverse();
                for q in c.as_mut_slice() {
                    *q = q.reversed();
                }
            }
        }
        let mut curves = OwnVec::new_in(alloc);
        let mut bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for c in contours.as_slice() {
            for &q in c.as_slice() {
                if q.is_point() {
                    continue;
                }
                let b = q.bounds();
                bbox[0] = bbox[0].min(b[0]);
                bbox[1] = bbox[1].min(b[1]);
                bbox[2] = bbox[2].max(b[2]);
                bbox[3] = bbox[3].max(b[3]);
                curves.try_push(q).map_err(|r| UiError::Alloc(r.why))?;
            }
        }
        if curves.is_empty() || !(bbox[2] > bbox[0] && bbox[3] > bbox[1]) {
            return Ok(None);
        }
        Ok(Some(Self { curves, bbox }))
    }
}

/// GPU-ready metadata for a single Slug outline glyph inside `den`-format buffers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SlugGlyphMeta {
    /// Em-space outline bounding box `[x0, y0, x1, y1]` (y up).
    pub bbox: [f32; 4],
    /// Band transform `[scale_x, scale_y, offset_x, offset_y]`.
    pub band_tf: [f32; 4],
    /// `[band_header_start_in_bands, max_vertical_band, max_horizontal_band, 0]`.
    pub gdata: [u32; 4],
    /// `true` when this glyph has drawable curves (false for whitespace).
    pub drawable: bool,
}

/// Pack an [`Outline`] directly into `den`-format `curves` (`[f32; 4]`) and `bands` (`u32`) buffers,
/// returning its [`SlugGlyphMeta`].
pub fn pack_outline_into<A: Allocator + Copy>(
    outline: &Outline<A>,
    curves_out: &mut OwnVec<[f32; 4], A>,
    bands_out: &mut OwnVec<u32, A>,
    alloc: A,
) -> Result<SlugGlyphMeta, UiError> {
    let curve_base = u32::try_from(curves_out.len()).unwrap_or(0);
    let band_base = u32::try_from(bands_out.len()).unwrap_or(0);

    for q in outline.curves.as_slice() {
        curves_out
            .try_push([q.0[0][0], q.0[0][1], q.0[1][0], q.0[1][1]])
            .map_err(|r| UiError::Alloc(r.why))?;
        curves_out
            .try_push([q.0[2][0], q.0[2][1], 0.0, 0.0])
            .map_err(|r| UiError::Alloc(r.why))?;
    }

    let n_curves = outline.curves.len();
    let nh = ((n_curves + 3) / 4).clamp(1, MAX_BANDS);
    let nv = ((n_curves + 3) / 4).clamp(1, MAX_BANDS);

    let [x0, y0, x1, y1] = outline.bbox;
    let w = (x1 - x0).max(1e-6);
    let h = (y1 - y0).max(1e-6);
    let nh_f = usize_to_f32(nh);
    let nv_f = usize_to_f32(nv);
    let band_tf = [nv_f / w, nh_f / h, -x0 * nv_f / w, -y0 * nh_f / h];

    let h_lists = assign_bands(outline.curves.as_slice(), 1, nh, y0, y1, alloc)?;
    let v_lists = assign_bands(outline.curves.as_slice(), 0, nv, x0, x1, alloc)?;

    let header_words = u32::try_from((nh + nv).saturating_mul(2)).unwrap_or(0);
    let mut cursor = header_words;
    for list in h_lists.as_slice() {
        let len = u32::try_from(list.len()).unwrap_or(0);
        bands_out.try_push(len).map_err(|r| UiError::Alloc(r.why))?;
        bands_out.try_push(cursor).map_err(|r| UiError::Alloc(r.why))?;
        cursor = cursor.saturating_add(len);
    }
    for list in v_lists.as_slice() {
        let len = u32::try_from(list.len()).unwrap_or(0);
        bands_out.try_push(len).map_err(|r| UiError::Alloc(r.why))?;
        bands_out.try_push(cursor).map_err(|r| UiError::Alloc(r.why))?;
        cursor = cursor.saturating_add(len);
    }
    for list in h_lists.as_slice() {
        for &ci in list.as_slice() {
            bands_out
                .try_push(curve_base.saturating_add(ci.saturating_mul(CURVE_SLOTS)))
                .map_err(|r| UiError::Alloc(r.why))?;
        }
    }
    for list in v_lists.as_slice() {
        for &ci in list.as_slice() {
            bands_out
                .try_push(curve_base.saturating_add(ci.saturating_mul(CURVE_SLOTS)))
                .map_err(|r| UiError::Alloc(r.why))?;
        }
    }

    Ok(SlugGlyphMeta {
        bbox: outline.bbox,
        band_tf,
        gdata: [
            band_base,
            u32::try_from(nv.saturating_sub(1)).unwrap_or(0),
            u32::try_from(nh.saturating_sub(1)).unwrap_or(0),
            0,
        ],
        drawable: true,
    })
}

fn assign_bands<A: Allocator + Copy>(
    curves: &[Quad],
    axis: usize,
    n: usize,
    lo: f32,
    hi: f32,
    alloc: A,
) -> Result<OwnVec<OwnVec<u32, A>, A>, UiError> {
    let thick = (hi - lo).max(1e-6) / usize_to_f32(n);
    let other = 1 - axis;
    let mut bands = OwnVec::try_with_capacity_in(n, alloc)?;
    for _ in 0..n {
        bands.try_push(OwnVec::new_in(alloc)).map_err(|r| UiError::Alloc(r.why))?;
    }
    for (idx, q) in curves.iter().enumerate() {
        if q.flat(axis) {
            continue;
        }
        let (q_lo, q_hi) = q.span(axis);
        let ci = u32::try_from(idx).unwrap_or(0);
        for b in 0..n {
            let b_lo = lo + usize_to_f32(b) * thick - BAND_EPS;
            let b_hi = lo + usize_to_f32(b + 1) * thick + BAND_EPS;
            if q_hi >= b_lo && q_lo <= b_hi {
                if let Some(list) = bands.get_mut(b) {
                    list.try_push(ci).map_err(|r| UiError::Alloc(r.why))?;
                }
            }
        }
    }
    // Sort each band descending by greatest coordinate on `other` axis (insertion sort, zero alloc).
    for list in bands.as_mut_slice() {
        let s = list.as_mut_slice();
        let mut i = 1usize;
        while i < s.len() {
            let mut j = i;
            while j > 0 {
                let (Some(&prev), Some(&cur)) = (s.get(j - 1), s.get(j)) else { break };
                let prev_hi = curves
                    .get(usize::try_from(prev).unwrap_or(0))
                    .map_or(f32::MIN, |q| q.span(other).1);
                let cur_hi = curves
                    .get(usize::try_from(cur).unwrap_or(0))
                    .map_or(f32::MIN, |q| q.span(other).1);
                if prev_hi < cur_hi {
                    s.swap(j - 1, j);
                    j -= 1;
                } else {
                    break;
                }
            }
            i += 1;
        }
    }
    Ok(bands)
}

// =============================================================================
// BZG1 Binary Glyph Table -> `den` GPU Storage Buffer Unpacker
// =============================================================================

/// One font instance stored in a `BZG1` Slug glyph table.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FontInstanceMeta {
    /// `0` = Inter, `1` = Berkeley Mono.
    pub family: u32,
    pub wght: u32,
    pub opsz: f32,
    pub upem: u32,
    pub ascent: f32,
    pub descent: f32,
    pub map_start: u32,
    pub map_count: u32,
}

/// Complete unpacked Slug vector font table ready to upload directly to `den`-format WebGPU buffers.
pub struct SlugFontTable<A: Allocator + Copy> {
    pub instances: OwnVec<FontInstanceMeta, A>,
    /// Sorted `(codepoint, glyph_id)` pairs grouped by `instance.map_start..+map_count`.
    pub cmap: OwnVec<(u32, u32), A>,
    /// Per-glyph metadata indexed by `glyph_id`.
    pub glyphs: OwnVec<SlugGlyphMeta, A>,
    /// `den`-format GPU storage buffer: 2 `vec4<f32>` per curve (`[p1x,p1y,p2x,p2y]`, `[p3x,p3y,0,0]`).
    pub curves: OwnVec<[f32; 4], A>,
    /// `den`-format GPU storage buffer: per-glyph band headers + curve slot lists.
    pub bands: OwnVec<u32, A>,
}

impl<A: Allocator + Copy> SlugFontTable<A> {
    pub fn empty_in(alloc: A) -> Self {
        Self {
            instances: OwnVec::new_in(alloc),
            cmap: OwnVec::new_in(alloc),
            glyphs: OwnVec::new_in(alloc),
            curves: OwnVec::new_in(alloc),
            bands: OwnVec::new_in(alloc),
        }
    }

    /// Find font instance matching `(family, wght, opsz)` (0 = Inter, 1 = Berkeley Mono).
    pub fn find_instance(&self, family: u32, wght: u32, opsz: f32) -> Option<&FontInstanceMeta> {
        for inst in self.instances.as_slice() {
            if inst.family == family && inst.wght == wght && (inst.opsz - opsz).abs() <= 0.05 {
                return Some(inst);
            }
        }
        // Fallback: closest opsz for same family + wght
        let mut best: Option<(&FontInstanceMeta, f32)> = None;
        for inst in self.instances.as_slice() {
            if inst.family == family && inst.wght == wght {
                let d = (inst.opsz - opsz).abs();
                if best.map_or(true, |(_, bd)| d < bd) {
                    best = Some((inst, d));
                }
            }
        }
        best.map(|(i, _)| i).or_else(|| self.instances.first())
    }

    /// Binary search `inst`'s sorted character map for `ch`, returning `glyph_id` (or `None`).
    pub fn lookup_glyph(&self, inst: &FontInstanceMeta, ch: char) -> Option<u32> {
        let cp = u32::from(ch);
        let start = usize::try_from(inst.map_start).ok()?;
        let count = usize::try_from(inst.map_count).ok()?;
        let slice = self.cmap.as_slice().get(start..start.saturating_add(count))?;
        let mut lo = 0usize;
        let mut hi = slice.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let Some(&(k, g)) = slice.get(mid) else { break };
            if k == cp {
                return if g == BZG1_NO_OUTLINE { None } else { Some(g) };
            }
            if k < cp {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        None
    }

    /// Decode a `BZG1` binary glyph table (`text.<hash>.bin`) into `den`-format GPU storage buffers.
    pub fn decode_bzg1(bytes: &[u8], alloc: A) -> Result<Self, UiError> {
        if bytes.len() < BZG1_HEADER_WORDS * 4 || bytes.len() % 4 != 0 {
            return Err(UiError::InvalidGlyphTable);
        }
        let total_words = bytes.len() / 4;
        let magic = read_u32(bytes, 0)?;
        let version = read_u32(bytes, 1)?;
        let inst_count = usize::try_from(read_u32(bytes, 2)?).unwrap_or(0);
        let glyph_count = usize::try_from(read_u32(bytes, 3)?).unwrap_or(0);
        let inst_at = usize::try_from(read_u32(bytes, 4)?).unwrap_or(0);
        let map_at = usize::try_from(read_u32(bytes, 5)?).unwrap_or(0);
        let dir_at = usize::try_from(read_u32(bytes, 6)?).unwrap_or(0);
        let curves_at = usize::try_from(read_u32(bytes, 7)?).unwrap_or(0);
        let bands_at = usize::try_from(read_u32(bytes, 8)?).unwrap_or(0);
        let declared_words = usize::try_from(read_u32(bytes, 9)?).unwrap_or(0);

        if magic != BZG1_MAGIC || version != BZG1_VERSION || declared_words != total_words {
            return Err(UiError::InvalidGlyphTable);
        }

        // 1. Read font instances
        let mut instances = OwnVec::try_with_capacity_in(inst_count, alloc)?;
        let mut max_map_end: usize = 0;
        for i in 0..inst_count {
            let base = inst_at.saturating_add(i.saturating_mul(BZG1_INSTANCE_WORDS));
            let map_start = read_u32(bytes, base + 6)?;
            let map_count = read_u32(bytes, base + 7)?;
            let end = usize::try_from(map_start)
                .unwrap_or(0)
                .saturating_add(usize::try_from(map_count).unwrap_or(0));
            if end > max_map_end {
                max_map_end = end;
            }
            instances
                .try_push(FontInstanceMeta {
                    family: read_u32(bytes, base)?,
                    wght: read_u32(bytes, base + 1)?,
                    opsz: f32::from_bits(read_u32(bytes, base + 2)?),
                    upem: read_u32(bytes, base + 3)?,
                    ascent: f32::from_bits(read_u32(bytes, base + 4)?),
                    descent: f32::from_bits(read_u32(bytes, base + 5)?),
                    map_start,
                    map_count,
                })
                .map_err(|r| UiError::Alloc(r.why))?;
        }

        // 2. Read sorted character map entries
        let mut cmap = OwnVec::try_with_capacity_in(max_map_end, alloc)?;
        for m in 0..max_map_end {
            let base = map_at.saturating_add(m.saturating_mul(2));
            let cp = read_u32(bytes, base)?;
            let g = read_u32(bytes, base + 1)?;
            cmap.try_push((cp, g)).map_err(|r| UiError::Alloc(r.why))?;
        }

        // 3. Unpack every glyph's f16 curves + u16 bands into `den`-format f32 curves + u32 bands!
        let mut glyphs = OwnVec::try_with_capacity_in(glyph_count, alloc)?;
        let mut curves = OwnVec::new_in(alloc);
        let mut bands = OwnVec::new_in(alloc);
        let mut rel_to_slot: OwnVec<(u32, u32), A> = OwnVec::new_in(alloc);

        for g in 0..glyph_count {
            let d_base = dir_at.saturating_add(g.saturating_mul(BZG1_DIR_WORDS));
            let curve_start = usize::try_from(read_u32(bytes, d_base)?).unwrap_or(0);
            let band_start = usize::try_from(read_u32(bytes, d_base + 1)?).unwrap_or(0);
            let nh_nv = read_u32(bytes, d_base + 2)?;
            let nh = usize::try_from(nh_nv & 0xffff).unwrap_or(0);
            let nv = usize::try_from(nh_nv >> 16).unwrap_or(0);
            let bx0 = f32::from_bits(read_u32(bytes, d_base + 4)?);
            let by0 = f32::from_bits(read_u32(bytes, d_base + 5)?);
            let bx1 = f32::from_bits(read_u32(bytes, d_base + 6)?);
            let by1 = f32::from_bits(read_u32(bytes, d_base + 7)?);

            if nh == 0 || nv == 0 || !(bx1 > bx0 && by1 > by0) {
                glyphs.try_push(SlugGlyphMeta::default()).map_err(|r| UiError::Alloc(r.why))?;
                continue;
            }

            let den_band_start = u32::try_from(bands.len()).unwrap_or(0);
            let total_bands = nh.saturating_add(nv);
            let header_words = u32::try_from(total_bands.saturating_mul(2)).unwrap_or(0);

            // Emit den band headers first (`count`, `offset_from_den_band_start`), then curve slot lists.
            let mut list_cursor = header_words;
            for bi in 0..total_bands {
                let cnt = u32::from(read_band_u16(bytes, bands_at, band_start + bi * 2)?);
                bands.try_push(cnt).map_err(|r| UiError::Alloc(r.why))?;
                bands.try_push(list_cursor).map_err(|r| UiError::Alloc(r.why))?;
                list_cursor = list_cursor.saturating_add(cnt);
            }

            rel_to_slot.clear();
            for bi in 0..total_bands {
                let cnt = usize::from(read_band_u16(bytes, bands_at, band_start + bi * 2)?);
                let list_off = usize::from(read_band_u16(bytes, bands_at, band_start + bi * 2 + 1)?);
                for k in 0..cnt {
                    let rel_t = u32::from(read_band_u16(bytes, bands_at, band_start + list_off + k)?);
                    let slot = match rel_to_slot.as_slice().iter().find(|&&(r, _)| r == rel_t) {
                        Some(&(_, s)) => s,
                        None => {
                            let s = u32::try_from(curves.len()).unwrap_or(0);
                            let t = curve_start.saturating_add(usize::try_from(rel_t).unwrap_or(0));
                            let w0 = read_u32(bytes, curves_at.saturating_add(t.saturating_mul(2)))?;
                            let w1 = read_u32(bytes, curves_at.saturating_add(t.saturating_mul(2) + 1))?;
                            let w2 = read_u32(bytes, curves_at.saturating_add(t.saturating_mul(2) + 2))?;
                            let (x1, y1) = unpack_2xf16(w0);
                            let (x2, y2) = unpack_2xf16(w1);
                            let (x3, y3) = unpack_2xf16(w2);
                            curves.try_push([x1, y1, x2, y2]).map_err(|r| UiError::Alloc(r.why))?;
                            curves.try_push([x3, y3, 0.0, 0.0]).map_err(|r| UiError::Alloc(r.why))?;
                            rel_to_slot.try_push((rel_t, s)).map_err(|r| UiError::Alloc(r.why))?;
                            s
                        }
                    };
                    bands.try_push(slot).map_err(|r| UiError::Alloc(r.why))?;
                }
            }

            let w = (bx1 - bx0).max(1e-6);
            let h = (by1 - by0).max(1e-6);
            let nv_f = usize_to_f32(nv);
            let nh_f = usize_to_f32(nh);
            let band_tf = [nv_f / w, nh_f / h, -bx0 * nv_f / w, -by0 * nh_f / h];
            glyphs
                .try_push(SlugGlyphMeta {
                    bbox: [bx0, by0, bx1, by1],
                    band_tf,
                    gdata: [
                        den_band_start,
                        u32::try_from(nv.saturating_sub(1)).unwrap_or(0),
                        u32::try_from(nh.saturating_sub(1)).unwrap_or(0),
                        0,
                    ],
                    drawable: true,
                })
                .map_err(|r| UiError::Alloc(r.why))?;
        }

        // Guarantee non-empty GPU storage buffers even for an empty table.
        if curves.is_empty() {
            curves.try_push([0.0; 4]).map_err(|r| UiError::Alloc(r.why))?;
        }
        if bands.is_empty() {
            bands.try_push(0).map_err(|r| UiError::Alloc(r.why))?;
        }

        Ok(Self { instances, cmap, glyphs, curves, bands })
    }
}

fn read_u32(bytes: &[u8], word_idx: usize) -> Result<u32, UiError> {
    let off = word_idx.checked_mul(4).ok_or(UiError::InvalidGlyphTable)?;
    let slice = bytes.get(off..off.saturating_add(4)).ok_or(UiError::InvalidGlyphTable)?;
    let arr = <[u8; 4]>::try_from(slice).map_err(|_| UiError::InvalidGlyphTable)?;
    Ok(u32::from_le_bytes(arr))
}

fn read_band_u16(bytes: &[u8], bands_at: usize, half_idx: usize) -> Result<u16, UiError> {
    let word = read_u32(bytes, bands_at.saturating_add(half_idx / 2))?;
    let shift = if half_idx % 2 == 0 { 0u32 } else { 16u32 };
    Ok(u16::try_from((word >> shift) & 0xffff).unwrap_or(0))
}

fn unpack_2xf16(word: u32) -> (f32, f32) {
    let lo = u16::try_from(word & 0xffff).unwrap_or(0);
    let hi = u16::try_from((word >> 16) & 0xffff).unwrap_or(0);
    (f16_to_f32(lo), f16_to_f32(hi))
}

/// Exact IEEE-754 binary16 -> binary32 decoder (zero `as` casts, handles normal/subnormal/zero).
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from((h >> 15) & 1) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let mant = u32::from(h & 0x03ff);
    if exp == 0 {
        if mant == 0 {
            return f32::from_bits(sign);
        }
        // Subnormal: mant * 2^-24
        let mut m = mant;
        let mut e: u32 = 127 - 14;
        while (m & 0x0400) == 0 {
            m <<= 1;
            e = e.saturating_sub(1);
        }
        m &= 0x03ff;
        return f32::from_bits(sign | (e << 23) | (m << 13));
    }
    if exp == 31 {
        let nan_bit = if mant != 0 { 0x0040_0000 } else { 0 };
        return f32::from_bits(sign | 0x7f80_0000 | nan_bit);
    }
    let exp32 = exp + (127 - 15);
    f32::from_bits(sign | (exp32 << 23) | (mant << 13))
}

fn usize_to_f32(v: usize) -> f32 {
    let u = u32::try_from(v).unwrap_or(u32::MAX);
    let hi = u16::try_from(u >> 16).unwrap_or(0);
    let lo = u16::try_from(u & 0xffff).unwrap_or(0);
    f32::from(hi) * 65536.0 + f32::from(lo)
}

// =============================================================================
// CPU Reference Port of `den.slang`'s `slug_render` (for unit-test verification)
// =============================================================================

#[inline]
pub fn slug_root_code_cpu(y1: f32, y2: f32, y3: f32) -> u32 {
    let mut s = 0u32;
    if y1 < 0.0 {
        s |= 1;
    }
    if y2 < 0.0 {
        s |= 2;
    }
    if y3 < 0.0 {
        s |= 4;
    }
    (0x2E74u32 >> s) & 0x0101u32
}

#[inline]
pub fn slug_solve_horiz_cpu(p12: [f32; 4], p3: [f32; 2]) -> [f32; 2] {
    let ax = p12[0] - p12[2] * 2.0 + p3[0];
    let ay = p12[1] - p12[3] * 2.0 + p3[1];
    let bx = p12[0] - p12[2];
    let by = p12[1] - p12[3];
    let lin = ay.abs() < 1.0 / 65536.0;
    let ra = 1.0 / if lin { 1.0 } else { ay };
    let rb = 0.5 / if by == 0.0 { 1.0 } else { by };
    let d = gpu::math::sqrt((by * by - ay * p12[1]).max(0.0));
    let t1 = if lin { p12[1] * rb } else { (by - d) * ra };
    let t2 = if lin { t1 } else { (by + d) * ra };
    [(ax * t1 - bx * 2.0) * t1 + p12[0], (ax * t2 - bx * 2.0) * t2 + p12[0]]
}

#[inline]
pub fn slug_solve_vert_cpu(p12: [f32; 4], p3: [f32; 2]) -> [f32; 2] {
    let ax = p12[0] - p12[2] * 2.0 + p3[0];
    let ay = p12[1] - p12[3] * 2.0 + p3[1];
    let bx = p12[0] - p12[2];
    let by = p12[1] - p12[3];
    let lin = ax.abs() < 1.0 / 65536.0;
    let ra = 1.0 / if lin { 1.0 } else { ax };
    let rb = 0.5 / if bx == 0.0 { 1.0 } else { bx };
    let d = gpu::math::sqrt((bx * bx - ax * p12[0]).max(0.0));
    let t1 = if lin { p12[0] * rb } else { (bx - d) * ra };
    let t2 = if lin { t1 } else { (bx + d) * ra };
    [(ay * t1 - by * 2.0) * t1 + p12[1], (ay * t2 - by * 2.0) * t2 + p12[1]]
}

/// CPU reference implementation of `den.slang::slug_render` — evaluates exact coverage in `[0, 1]`.
pub fn slug_render_cpu(
    curves: &[[f32; 4]],
    bands: &[u32],
    render: [f32; 2],
    band_tf: [f32; 4],
    gdata: [u32; 4],
    ppem: [f32; 2],
    embolden: f32,
) -> f32 {
    let max_v = gdata[1];
    let max_h = gdata[2];
    let bx_f = gpu::math::floor(render[0] * band_tf[0] + band_tf[2]).clamp(0.0, f32::from(u16::try_from(max_v).unwrap_or(0)));
    let by_f = gpu::math::floor(render[1] * band_tf[1] + band_tf[3]).clamp(0.0, f32::from(u16::try_from(max_h).unwrap_or(0)));
    let bx = f32_to_u32_small(bx_f);
    let by = f32_to_u32_small(by_f);
    let at = usize::try_from(gdata[0]).unwrap_or(0);

    let mut xcov = 0.0f32;
    let mut xwgt = 0.0f32;
    let hb = at + usize::try_from(2 * by).unwrap_or(0);
    let hcount = usize::try_from(bands.get(hb).copied().unwrap_or(0)).unwrap_or(0);
    let hlist = at + usize::try_from(bands.get(hb + 1).copied().unwrap_or(0)).unwrap_or(0);
    for i in 0..hcount {
        let slot = usize::try_from(bands.get(hlist + i).copied().unwrap_or(0)).unwrap_or(0);
        let c0 = curves.get(slot).copied().unwrap_or([0.0; 4]);
        let c1 = curves.get(slot + 1).copied().unwrap_or([0.0; 4]);
        let p12 = [c0[0] - render[0], c0[1] - render[1], c0[2] - render[0], c0[3] - render[1]];
        let p3 = [c1[0] - render[0], c1[1] - render[1]];
        if p12[0].max(p12[2]).max(p3[0]) * ppem[0] < -0.5 - embolden {
            break;
        }
        let code = slug_root_code_cpu(p12[1], p12[3], p3[1]);
        if code != 0 {
            let r = slug_solve_horiz_cpu(p12, p3);
            let rx = r[0] * ppem[0];
            let ry = r[1] * ppem[0];
            if (code & 1) != 0 {
                let x = rx - embolden;
                xcov += (x + 0.5).clamp(0.0, 1.0);
                xwgt = xwgt.max((1.0 - x.abs() * 2.0).clamp(0.0, 1.0));
            }
            if code > 1 {
                let x = ry + embolden;
                xcov -= (x + 0.5).clamp(0.0, 1.0);
                xwgt = xwgt.max((1.0 - x.abs() * 2.0).clamp(0.0, 1.0));
            }
        }
    }

    let mut ycov = 0.0f32;
    let mut ywgt = 0.0f32;
    let vb = at + usize::try_from(2 * (max_h + 1 + bx)).unwrap_or(0);
    let vcount = usize::try_from(bands.get(vb).copied().unwrap_or(0)).unwrap_or(0);
    let vlist = at + usize::try_from(bands.get(vb + 1).copied().unwrap_or(0)).unwrap_or(0);
    for j in 0..vcount {
        let slot = usize::try_from(bands.get(vlist + j).copied().unwrap_or(0)).unwrap_or(0);
        let c0 = curves.get(slot).copied().unwrap_or([0.0; 4]);
        let c1 = curves.get(slot + 1).copied().unwrap_or([0.0; 4]);
        let p12 = [c0[0] - render[0], c0[1] - render[1], c0[2] - render[0], c0[3] - render[1]];
        let p3 = [c1[0] - render[0], c1[1] - render[1]];
        if p12[1].max(p12[3]).max(p3[1]) * ppem[1] < -0.5 - embolden {
            break;
        }
        let code = slug_root_code_cpu(p12[0], p12[2], p3[0]);
        if code != 0 {
            let r = slug_solve_vert_cpu(p12, p3);
            let rx = r[0] * ppem[1];
            let ry = r[1] * ppem[1];
            if (code & 1) != 0 {
                let y = rx + embolden;
                ycov -= (y + 0.5).clamp(0.0, 1.0);
                ywgt = ywgt.max((1.0 - y.abs() * 2.0).clamp(0.0, 1.0));
            }
            if code > 1 {
                let y = ry - embolden;
                ycov += (y + 0.5).clamp(0.0, 1.0);
                ywgt = ywgt.max((1.0 - y.abs() * 2.0).clamp(0.0, 1.0));
            }
        }
    }

    let w_cov = (xcov * xwgt + ycov * ywgt).abs() / (xwgt + ywgt).max(1.0 / 65536.0);
    w_cov.max(xcov.abs().min(ycov.abs())).clamp(0.0, 1.0)
}

fn f32_to_u32_small(v: f32) -> u32 {
    let mut r = 0u32;
    while r < 64 && f32::from(u16::try_from(r + 1).unwrap_or(64)) <= v {
        r += 1;
    }
    r
}
