//! The Slug algorithm's CPU half: glyph outlines as quadratic Béziers in em space, their packing into the curve and band
//! buffers `glyph_fs` reads, and (in tests) a port of the reference pixel shader that holds the GPU's arithmetic to
//! known shapes. Slug is Eric Lengyel's (JCGT 2017, "GPU-Centered Font Rendering Directly from Glyph Outlines"); the
//! patent is dedicated to the public domain and the reference shaders (https://github.com/EricLengyel/Slug) are MIT
//! or Apache-2.0, copyright 2017 Eric Lengyel, credited in shaders/harness.slang. Copied from den-native's slug.rs
//! (github.com/andrewgazelka/den) without its CPU reference shader. harness feeds it the brand marks' SVG paths
//! (icons.rs walks them into a [`PathBuilder`]).

/// Largest distance, in ems, a cubic's quadratic replacements may stray from it at the same parameter.
pub const TOLERANCE: f64 = 1.0 / 1024.0;
/// Most bands a glyph is cut into along either axis.
pub const MAX_BANDS: usize = 16;
/// Bands overlap by this much (ems) so a curve on a seam is in both.
pub const BAND_EPS: f32 = 1.0 / 1024.0;
/// Uints a band takes in the band buffer's header: its curve count, then where its list starts (from the glyph's start).
pub const BAND_HEADER: u32 = 2;
/// `float4`s a curve takes in the curve buffer: (p1, p2) then (p3, unused).
pub const CURVE_SLOTS: u32 = 2;

/// A quadratic Bézier in em space (y up): control points p1, p2, p3. A straight line is {p1, p2, p2}.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad(pub [[f32; 2]; 3]);

impl Quad {
    #[must_use]
    pub fn line(a: [f32; 2], b: [f32; 2]) -> Self {
        Self([a, b, b])
    }

    /// The control points' least and greatest coordinate on `axis` (0 x, 1 y): the curve lies between them.
    #[must_use]
    pub fn span(&self, axis: usize) -> (f32, f32) {
        let [a, b, c] = self.0.map(|p| p[axis]);
        (a.min(b).min(c), a.max(b).max(c))
    }

    /// Every control point has the same coordinate on `axis`: a line along the other axis.
    #[must_use]
    pub fn flat(&self, axis: usize) -> bool {
        self.0[0][axis] == self.0[1][axis] && self.0[1][axis] == self.0[2][axis]
    }

    #[must_use]
    pub fn at(&self, t: f32) -> [f32; 2] {
        let s = 1.0 - t;
        let [a, b, c] = self.0;
        [0, 1].map(|i| s * s * a[i] + 2.0 * s * t * b[i] + t * t * c[i])
    }

    /// The curve's own bounds (x0, y0, x1, y1): the extremes inside the span, not the control polygon's.
    #[must_use]
    pub fn bounds(&self) -> [f32; 4] {
        let mut out = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for axis in 0..2 {
            let [a, b, c] = self.0.map(|p| p[axis]);
            let (mut lo, mut hi) = (a.min(c), a.max(c));
            let denom = a - 2.0 * b + c;
            if denom != 0.0 {
                let t = (a - b) / denom;
                if t > 0.0 && t < 1.0 {
                    let v = self.at(t)[axis];
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            out[axis] = lo;
            out[axis + 2] = hi;
        }
        out
    }

    fn is_point(&self) -> bool {
        self.0[0] == self.0[1] && self.0[1] == self.0[2]
    }

    fn reversed(&self) -> Self {
        let [a, b, c] = self.0;
        Self([c, b, a])
    }
}

/// Twice the signed area a contour of quadratics encloses, halved (positive when it runs counter-clockwise, y up).
/// Exact: a quadratic's part of the line integral is (2/3)(p1×p2 + p2×p3) + (1/3) p1×p3.
#[must_use]
pub fn signed_area(contour: &[Quad]) -> f64 {
    let cross = |a: [f32; 2], b: [f32; 2]| {
        f64::from(a[0]) * f64::from(b[1]) - f64::from(a[1]) * f64::from(b[0])
    };
    contour
        .iter()
        .map(|q| {
            (2.0 / 3.0) * (cross(q.0[0], q.0[1]) + cross(q.0[1], q.0[2]))
                + cross(q.0[0], q.0[2]) / 3.0
        })
        .sum::<f64>()
        * 0.5
}

/// The quadratics standing for the cubic `c` (end to end, in order), each within `tol` of it at the same parameter.
///
/// One quadratic with control point (3(c1 + c2) - (c0 + c3)) / 4 stays within sqrt(3)/36 |c3 - 3 c2 + 3 c1 - c0| of
/// the cubic (the cubic's t³ term); cutting the parameter range into n equal pieces divides that by n³.
#[must_use]
pub fn cubic_quads(c: [[f64; 2]; 4], tol: f64) -> Vec<[[f64; 2]; 3]> {
    let d = [0, 1].map(|i| c[3][i] - 3.0 * c[2][i] + 3.0 * c[1][i] - c[0][i]);
    let err = 3f64.sqrt() / 36.0 * d[0].hypot(d[1]);
    let n = ((err / tol).cbrt().ceil() as usize).clamp(1, 64);
    // De Casteljau: the cubic cut at `t` into its two halves.
    let split = |c: [[f64; 2]; 4], t: f64| {
        let lerp = |a: [f64; 2], b: [f64; 2]| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        let (ab, bc, cd) = (lerp(c[0], c[1]), lerp(c[1], c[2]), lerp(c[2], c[3]));
        let (abc, bcd) = (lerp(ab, bc), lerp(bc, cd));
        let mid = lerp(abc, bcd);
        ([c[0], ab, abc, mid], [mid, bcd, cd, c[3]])
    };
    let mut out: Vec<[[f64; 2]; 3]> = (0..n)
        .map(|i| {
            let (t0, t1) = (i as f64 / n as f64, (i + 1) as f64 / n as f64);
            let rest = if i == 0 { c } else { split(c, t0).1 };
            let piece = if i + 1 == n { rest } else { split(rest, (t1 - t0) / (1.0 - t0)).0 };
            let q = [0, 1]
                .map(|k| (3.0 * (piece[1][k] + piece[2][k]) - (piece[0][k] + piece[3][k])) * 0.25);
            [piece[0], q, piece[3]]
        })
        .collect();
    // Each cut computed twice differs in the last bit: the next piece starts exactly where the last ended.
    for i in 1..out.len() {
        out[i][0] = out[i - 1][2];
    }
    out
}

/// Contours of quadratics from path elements: lines become {p1, p2, p2}, cubics split by [`cubic_quads`], and every
/// contour closes (a closing line when its last point is not its first).
#[derive(Debug, Default)]
pub struct PathBuilder {
    done: Vec<Vec<Quad>>,
    cur: Vec<Quad>,
    start: [f64; 2],
    at: [f64; 2],
}

fn narrow(p: [f64; 2]) -> [f32; 2] {
    [p[0] as f32, p[1] as f32]
}

impl PathBuilder {
    pub fn move_to(&mut self, p: [f64; 2]) {
        self.close();
        self.start = p;
        self.at = p;
    }

    pub fn line_to(&mut self, p: [f64; 2]) {
        if p != self.at {
            self.cur.push(Quad::line(narrow(self.at), narrow(p)));
            self.at = p;
        }
    }

    pub fn quad_to(&mut self, c: [f64; 2], p: [f64; 2]) {
        self.cur.push(Quad([narrow(self.at), narrow(c), narrow(p)]));
        self.at = p;
    }

    pub fn cubic_to(&mut self, c1: [f64; 2], c2: [f64; 2], p: [f64; 2]) {
        for q in cubic_quads([self.at, c1, c2, p], TOLERANCE) {
            self.cur.push(Quad(q.map(narrow)));
        }
        self.at = p;
    }

    pub fn close(&mut self) {
        if !self.cur.is_empty() {
            self.line_to(self.start);
            self.done.push(std::mem::take(&mut self.cur));
        }
        self.at = self.start;
    }

    #[must_use]
    pub fn finish(mut self) -> Vec<Vec<Quad>> {
        self.close();
        self.done
    }
}

/// A glyph's curves in em space, wound counter-clockwise (y up) so ink is on the left of every edge: the shader's
/// emboldening relies on it.
#[derive(Clone, Debug, PartialEq)]
pub struct Outline {
    pub curves: Vec<Quad>,
    /// x0, y0, x1, y1: the curves' own bounds.
    pub bbox: [f32; 4],
}

impl Outline {
    /// The outline of `contours`; None when there is nothing to draw (no curves, or bounds with no area).
    #[must_use]
    pub fn new(mut contours: Vec<Vec<Quad>>) -> Option<Self> {
        if contours.iter().map(|c| signed_area(c)).sum::<f64>() < 0.0 {
            for c in &mut contours {
                c.reverse();
                for q in c.iter_mut() {
                    *q = q.reversed();
                }
            }
        }
        let curves: Vec<Quad> = contours.into_iter().flatten().filter(|q| !q.is_point()).collect();
        let bbox = curves
            .iter()
            .map(Quad::bounds)
            .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])?;
        (bbox[2] > bbox[0] && bbox[3] > bbox[1]).then_some(Self { curves, bbox })
    }
}

/// A glyph as the buffers hold it.
#[derive(Clone, Debug, PartialEq)]
pub struct Packed {
    pub bbox: [f32; 4],
    /// Band scale x, scale y, offset x, offset y: `floor(em * scale + offset)` is the band index.
    pub band: [f32; 4],
    /// Greatest band index: vertical bands (cut along x) then horizontal ones (cut along y).
    pub max: [u32; 2],
    /// Per curve two `float4`s: (p1.x, p1.y, p2.x, p2.y), (p3.x, p3.y, 0, 0).
    pub curves: Vec<[f32; 4]>,
    /// Header of the horizontal bands (a count and a list offset each, `BAND_HEADER` uints), then of the vertical
    /// ones, then the lists: each curve's first `float4` index in the curve buffer. A horizontal band's list is
    /// sorted by its curves' greatest x, descending; a vertical band's by greatest y.
    pub bands: Vec<u32>,
}

/// The curves (by index) whose span on `axis` meets each of `n` equal bands over `lo..hi`, a band's list sorted by the
/// curves' greatest coordinate on the other axis, descending. A curve flat on `axis` (parallel to the ray) is in none.
fn assign(curves: &[Quad], axis: usize, n: usize, lo: f32, hi: f32) -> Vec<Vec<u32>> {
    let thick = (hi - lo) / n as f32;
    let other = 1 - axis;
    let mut bands = vec![Vec::new(); n];
    for (k, q) in curves.iter().enumerate().filter(|(_, q)| !q.flat(axis)) {
        let (cl, ch) = q.span(axis);
        for (b, list) in bands.iter_mut().enumerate() {
            let (bl, bh) = (lo + thick * b as f32, lo + thick * (b + 1) as f32);
            if ch >= bl - BAND_EPS && cl <= bh + BAND_EPS {
                list.push(k as u32);
            }
        }
    }
    for list in &mut bands {
        list.sort_by(|&a, &b| {
            curves[b as usize]
                .span(other)
                .1
                .total_cmp(&curves[a as usize].span(other).1)
                .then(a.cmp(&b))
        });
    }
    bands
}

/// The band count (up to [`MAX_BANDS`]) that keeps the longest list short, a few more bands costing less than a curve.
fn bands_along(curves: &[Quad], axis: usize, lo: f32, hi: f32) -> Vec<Vec<u32>> {
    (1..=MAX_BANDS)
        .map(|n| assign(curves, axis, n, lo, hi))
        .min_by_key(|b| b.iter().map(Vec::len).max().unwrap_or(0) * 8 + b.len())
        .expect("at least one band count")
}

/// `o` as buffer contents; its curves start at `float4` index `curve_base` of the curve buffer.
#[must_use]
pub fn pack(o: &Outline, curve_base: u32) -> Packed {
    let [x0, y0, x1, y1] = o.bbox;
    let horizontal = bands_along(&o.curves, 1, y0, y1);
    let vertical = bands_along(&o.curves, 0, x0, x1);
    let (nh, nv) = (horizontal.len(), vertical.len());
    let mut curves = Vec::with_capacity(o.curves.len() * CURVE_SLOTS as usize);
    for q in &o.curves {
        let [a, b, c] = q.0;
        curves.push([a[0], a[1], b[0], b[1]]);
        curves.push([c[0], c[1], 0.0, 0.0]);
    }
    let mut bands = vec![0u32; BAND_HEADER as usize * (nh + nv)];
    for (i, list) in horizontal.iter().chain(&vertical).enumerate() {
        let at = bands.len() as u32;
        bands[BAND_HEADER as usize * i] = list.len() as u32;
        bands[BAND_HEADER as usize * i + 1] = at;
        bands.extend(list.iter().map(|&k| curve_base + CURVE_SLOTS * k));
    }
    let (sx, sy) = (nv as f32 / (x1 - x0), nh as f32 / (y1 - y0));
    Packed {
        bbox: o.bbox,
        band: [sx, sy, -x0 * sx, -y0 * sy],
        max: [nv as u32 - 1, nh as u32 - 1],
        curves,
        bands,
    }
}
