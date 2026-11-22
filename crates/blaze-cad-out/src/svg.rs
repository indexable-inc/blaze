//! A 2D vector SVG 1.1 cross-section writer: Marching Squares with cell-centre asymptotic
//! decider, deterministic polyline chaining, and clean standalone engineering drawing output.
//!
//! # Input
//!
//! [`SectionGrid`] is a regular 2D grid of signed distance samples in metres (negative inside,
//! positive outside), laid out row-major (`values[iy * nx + ix]`), designed to pair directly with
//! `blaze-cad-field::section` without coupling `blaze-cad-out` to the field kernel.
//!
//! # Contour extraction and determinism
//!
//! Every cell `(cx, cy)` is classified by `v < 0.0` (so exact zeros lie on the outside boundary,
//! preventing degenerate zero-denominator edges). Saddle cases `5` and `10` are disambiguated
//! with the cell-centre average `0.25 · (v0 + v1 + v2 + v3)` so contours never self-intersect or
//! misconnect. Segments are oriented with the interior (`v < 0.0`) on the left, producing
//! counter-clockwise outer contours and clockwise holes. Segments are chained strictly by
//! topological grid-edge keys (sorted in scratch memory, never hashed), closed loops are rotated
//! to their lexicographically smallest vertex, and contours are sorted canonically.
//!
//! # SVG output
//!
//! Written directly through `blaze-text` into a [`Bytes`] sink (never `core::fmt`). One user
//! unit equals 1 mm (`width`/`height` in `mm`), with math Y-up mapped to SVG Y-down `(x, -y)`.
//! Closed loops are emitted as one compound `<path id="silhouette" fill-rule="evenodd" .../>` so
//! interior holes punch out cleanly, accompanied by optional engineering grid, datum axes, overall
//! width/height dimension callouts in mm, and a scale bar.

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_int::{Int, idx};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::Writes;

use crate::Limits;
use crate::plant::SvgPlant;
use crate::sink::Bytes;

/// Default cross-section fill colour (light blueprint tint).
pub const DEFAULT_FILL: &str = "#dbeafe";

/// Default contour outline colour (deep slate).
pub const DEFAULT_STROKE: &str = "#0f172a";

const MM_PER_M: f64 = 1000.0;

/// A regular 2D grid of signed distance samples in metres (negative inside, positive outside).
///
/// Sample `(ix, iy)` sits at `values[iy * nx + ix]` and world coordinates
/// `(origin[0] + ix * step, origin[1] + iy * step)`.
#[derive(Clone, Copy, Debug)]
pub struct SectionGrid<'a> {
    pub nx: u32,
    pub ny: u32,
    pub origin: [f64; 2],
    pub step: f64,
    pub values: &'a [f64],
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct SvgOpts<'a, 's, 't> {
    /// Document `<title>` (omitted when empty).
    pub title: &'t str,
    /// CSS fill colour for closed cross-section regions (e.g. [`DEFAULT_FILL`] or `"none"`).
    pub fill: &'t str,
    /// CSS stroke colour for contour outlines (e.g. [`DEFAULT_STROKE`]).
    pub stroke: &'t str,
    /// Contour stroke width in millimetres; `0.0` picks a scale-proportional width.
    pub stroke_width: f64,
    /// Draw overall width and height dimension callouts in millimetres.
    pub show_dimensions: bool,
    /// Draw a subtle engineering grid and X/Y datum axes.
    pub show_grid: bool,
    pub limits: Limits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SvgError {
    /// Fewer than 2 samples on an axis: at least one cell is required.
    EmptyGrid { nx: u32, ny: u32 },
    /// Cell count `(nx - 1) · (ny - 1)` exceeds `limits.most_svg_cells`.
    TooManyCells { cells: u64, most: u64 },
    /// `values.len()` does not equal `nx · ny`.
    Length { want: u64, got: usize },
    /// `origin` or grid extents are not finite, or `step` is not finite and strictly positive.
    BadGrid,
    /// Sample `index` in `values` is NaN or infinite.
    NonFinite { index: usize },
    /// `stroke_width` is not finite or is negative.
    BadStyle,
    Alloc(AllocFailure),
}

impl From<AllocFailure> for SvgError {
    fn from(e: AllocFailure) -> SvgError {
        SvgError::Alloc(e)
    }
}

/// Render `grid`'s zero-level cross-section as standalone SVG 1.1 UTF-8 bytes in `opts.alloc`.
pub fn svg<'a>(grid: &SectionGrid<'_>, opts: SvgOpts<'a, '_, '_>) -> Result<Vec<u8, Alloc<'a>>, SvgError> {
    svg_planted(grid, opts, NoPlant)
}

/// A topological grid edge: `kind == 0` is horizontal `(x, y)–(x+1, y)`, `kind == 1` is vertical
/// `(x, y)–(x, y+1)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey {
    kind: u8,
    y: u32,
    x: u32,
}

impl EdgeKey {
    #[inline]
    const fn horiz(x: u32, y: u32) -> EdgeKey {
        EdgeKey { kind: 0, y, x }
    }

    #[inline]
    const fn vert(x: u32, y: u32) -> EdgeKey {
        EdgeKey { kind: 1, y, x }
    }
}

#[derive(Clone, Copy)]
struct Seg {
    start: EdgeKey,
    end: EdgeKey,
}

#[derive(Clone, Copy)]
struct ContourRec {
    start: usize,
    len: usize,
    closed: bool,
}

#[inline]
fn canon(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

#[inline]
fn cmp_pt(a: [f64; 2], b: [f64; 2]) -> Ordering {
    a[0].total_cmp(&b[0]).then_with(|| a[1].total_cmp(&b[1]))
}

fn cmp_pts(a: &[[f64; 2]], b: &[[f64; 2]]) -> Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let ord = cmp_pt(a[i], b[i]);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

#[inline]
fn sample(grid: &SectionGrid<'_>, x: u32, y: u32) -> f64 {
    grid.values[idx(y) * idx(grid.nx) + idx(x)]
}

#[inline]
fn interp(va: f64, vb: f64, wrong_interp: bool) -> f64 {
    if wrong_interp {
        return 0.5;
    }
    let d = va - vb;
    let t = if d.is_finite() { va / d } else { (va * 0.5) / (va * 0.5 - vb * 0.5) };
    t.clamp(0.0, 1.0)
}

#[inline]
fn edge_point_mm(grid: &SectionGrid<'_>, edge: EdgeKey, wrong_interp: bool) -> [f64; 2] {
    let (px, py) = if edge.kind == 0 {
        let va = sample(grid, edge.x, edge.y);
        let vb = sample(grid, edge.x + 1, edge.y);
        let t = interp(va, vb, wrong_interp);
        (grid.origin[0] + (f64::from(edge.x) + t) * grid.step, grid.origin[1] + f64::from(edge.y) * grid.step)
    } else {
        let va = sample(grid, edge.x, edge.y);
        let vb = sample(grid, edge.x, edge.y + 1);
        let t = interp(va, vb, wrong_interp);
        (grid.origin[0] + f64::from(edge.x) * grid.step, grid.origin[1] + (f64::from(edge.y) + t) * grid.step)
    };
    [canon(px * MM_PER_M), canon(py * MM_PER_M)]
}

/// Pick a deterministic 1-2-5 step size near `target` using only IEEE basic ops.
fn nice_step(target: f64) -> f64 {
    if !(target > 0.0 && target.is_finite()) {
        return 1.0;
    }
    let mut p = 1.0f64;
    for _ in 0..320 {
        let next = p * 10.0;
        if next.is_finite() && next <= target {
            p = next;
        } else {
            break;
        }
    }
    for _ in 0..320 {
        if p > target {
            let next = p * 0.1;
            if next > 0.0 {
                p = next;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    let r = target / p;
    if r < 1.5 {
        p
    } else if r < 3.5 {
        2.0 * p
    } else if r < 7.5 {
        5.0 * p
    } else {
        10.0 * p
    }
}

/// Write `text` with XML 1.0 character escaping and control characters filtered.
fn xml_escape(text: &str, out: &mut Bytes<'_>) {
    for c in text.chars() {
        match c {
            '&' => {
                out.str("&amp;");
            }
            '<' => {
                out.str("&lt;");
            }
            '>' => {
                out.str("&gt;");
            }
            '"' => {
                out.str("&quot;");
            }
            '\'' => {
                out.str("&apos;");
            }
            c if c < '\x20' && c != '\t' && c != '\n' && c != '\r' => {
                out.byte(b' ');
            }
            c => {
                out.char(c);
            }
        }
    }
}

#[inline]
fn put_num(out: &mut Bytes<'_>, x: f64) {
    out.float(canon(x));
}

/// [`svg`] with a planted bug, for the tests' controls.
pub fn svg_planted<'a>(
    grid: &SectionGrid<'_>,
    opts: SvgOpts<'a, '_, '_>,
    plant: impl Planted<SvgPlant>,
) -> Result<Vec<u8, Alloc<'a>>, SvgError> {
    // 1. Validate inputs.
    if grid.nx < 2 || grid.ny < 2 {
        return Err(SvgError::EmptyGrid { nx: grid.nx, ny: grid.ny });
    }
    let cells = u64::from(grid.nx - 1) * u64::from(grid.ny - 1);
    if cells > opts.limits.most_svg_cells {
        return Err(SvgError::TooManyCells { cells, most: opts.limits.most_svg_cells });
    }
    let want = u64::from(grid.nx) * u64::from(grid.ny);
    if grid.values.len().widen_to::<u64>() != want {
        return Err(SvgError::Length { want, got: grid.values.len() });
    }
    if !(grid.origin[0].is_finite()
        && grid.origin[1].is_finite()
        && grid.step.is_finite()
        && grid.step > 0.0)
    {
        return Err(SvgError::BadGrid);
    }
    let grid_min_x = grid.origin[0] * MM_PER_M;
    let grid_min_y = grid.origin[1] * MM_PER_M;
    let grid_max_x = (grid.origin[0] + f64::from(grid.nx - 1) * grid.step) * MM_PER_M;
    let grid_max_y = (grid.origin[1] + f64::from(grid.ny - 1) * grid.step) * MM_PER_M;
    if !(grid_min_x.is_finite() && grid_min_y.is_finite() && grid_max_x.is_finite() && grid_max_y.is_finite()) {
        return Err(SvgError::BadGrid);
    }
    if !(opts.stroke_width.is_finite() && opts.stroke_width >= 0.0) {
        return Err(SvgError::BadStyle);
    }
    for (i, &v) in grid.values.iter().enumerate() {
        if !v.is_finite() {
            return Err(SvgError::NonFinite { index: i });
        }
    }

    let s = opts.scratch;

    // 2. Marching Squares: extract directed segments with inside (< 0) on the left.
    let mut segs: Vec<Seg, Alloc<'_>> = Vec::new_in(s);
    let wrong_saddle = plant.has(SvgPlant::WrongSaddle);
    let mut push_seg = |start: EdgeKey, end: EdgeKey| -> Result<(), SvgError> {
        segs.try_push(Seg { start, end }).map_err(|r| r.why)?;
        Ok(())
    };
    for cy in 0..(grid.ny - 1) {
        for cx in 0..(grid.nx - 1) {
            let v0 = sample(grid, cx, cy);
            let v1 = sample(grid, cx + 1, cy);
            let v2 = sample(grid, cx + 1, cy + 1);
            let v3 = sample(grid, cx, cy + 1);
            let mask = u8::from(v0 < 0.0)
                | (u8::from(v1 < 0.0) << 1)
                | (u8::from(v2 < 0.0) << 2)
                | (u8::from(v3 < 0.0) << 3);
            if mask == 0 || mask == 15 {
                continue;
            }
            let b = EdgeKey::horiz(cx, cy);
            let r = EdgeKey::vert(cx + 1, cy);
            let t = EdgeKey::horiz(cx, cy + 1);
            let l = EdgeKey::vert(cx, cy);
            match mask {
                1 => push_seg(b, l)?,
                2 => push_seg(r, b)?,
                3 => push_seg(r, l)?,
                4 => push_seg(t, r)?,
                5 => {
                    let vc = 0.25 * v0 + 0.25 * v1 + 0.25 * v2 + 0.25 * v3;
                    let connect = if wrong_saddle { vc >= 0.0 } else { vc < 0.0 };
                    if connect {
                        push_seg(b, r)?;
                        push_seg(t, l)?;
                    } else {
                        push_seg(b, l)?;
                        push_seg(t, r)?;
                    }
                }
                6 => push_seg(t, b)?,
                7 => push_seg(t, l)?,
                8 => push_seg(l, t)?,
                9 => push_seg(b, t)?,
                10 => {
                    let vc = 0.25 * v0 + 0.25 * v1 + 0.25 * v2 + 0.25 * v3;
                    let connect = if wrong_saddle { vc >= 0.0 } else { vc < 0.0 };
                    if connect {
                        push_seg(l, b)?;
                        push_seg(r, t)?;
                    } else {
                        push_seg(r, b)?;
                        push_seg(l, t)?;
                    }
                }
                11 => push_seg(r, t)?,
                12 => push_seg(l, r)?,
                13 => push_seg(b, r)?,
                14 => push_seg(l, b)?,
                _ => {}
            }
        }
    }

    // 3. Deterministic polyline chaining via sorted start EdgeKeys (no hashing).
    segs.sort_unstable_by_key(|seg| seg.start);
    let n_segs = segs.len();
    let mut next_of: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(n_segs, s)?;
    let mut has_pred: Vec<bool, Alloc<'_>> = Vec::try_with_capacity_in(n_segs, s)?;
    let mut visited: Vec<bool, Alloc<'_>> = Vec::try_with_capacity_in(n_segs, s)?;
    for _ in 0..n_segs {
        next_of.try_push(u32::MAX).map_err(|r| r.why)?;
        has_pred.try_push(false).map_err(|r| r.why)?;
        visited.try_push(false).map_err(|r| r.why)?;
    }
    for i in 0..n_segs {
        if let Ok(j) = segs.binary_search_by_key(&segs[i].end, |seg| seg.start) {
            next_of[i] = j.narrow_or_stop();
            has_pred[j] = true;
        }
    }

    let wrong_interp = plant.has(SvgPlant::WrongInterp);
    let mut pts: Vec<[f64; 2], Alloc<'_>> = Vec::try_with_capacity_in(n_segs, s)?;
    let mut contours: Vec<ContourRec, Alloc<'_>> = Vec::new_in(s);

    // Pass A: open polylines starting on grid boundaries (no predecessor).
    for i in 0..n_segs {
        if has_pred[i] || visited[i] {
            continue;
        }
        let start = pts.len();
        pts.try_push(edge_point_mm(grid, segs[i].start, wrong_interp)).map_err(|r| r.why)?;
        let mut cur = i;
        loop {
            visited[cur] = true;
            let p = edge_point_mm(grid, segs[cur].end, wrong_interp);
            if pts.last() != Some(&p) {
                pts.try_push(p).map_err(|r| r.why)?;
            }
            let nxt = next_of[cur];
            if nxt == u32::MAX || visited[idx(nxt)] {
                break;
            }
            cur = idx(nxt);
        }
        let len = pts.len() - start;
        if len >= 2 {
            contours.try_push(ContourRec { start, len, closed: false }).map_err(|r| r.why)?;
        }
    }

    // Pass B: closed loops.
    for i in 0..n_segs {
        if visited[i] {
            continue;
        }
        let start = pts.len();
        let mut cur = i;
        loop {
            visited[cur] = true;
            let p = edge_point_mm(grid, segs[cur].start, wrong_interp);
            if pts.len() == start || pts.last() != Some(&p) {
                pts.try_push(p).map_err(|r| r.why)?;
            }
            let nxt = next_of[cur];
            if nxt == u32::MAX || visited[idx(nxt)] {
                break;
            }
            cur = idx(nxt);
        }
        if pts.len() > start + 1 && pts[pts.len() - 1] == pts[start] {
            pts.pop();
        }
        let len = pts.len() - start;
        if len >= 3 {
            // Canonical start vertex: rotate cyclically (preserving orientation) to the
            // lexicographically smallest (x, y) point.
            let slice = &mut pts[start..start + len];
            let mut best = 0usize;
            for k in 1..len {
                if cmp_pt(slice[k], slice[best]) == Ordering::Less {
                    best = k;
                }
            }
            slice.rotate_left(best);
            contours.try_push(ContourRec { start, len, closed: true }).map_err(|r| r.why)?;
        }
    }

    // Canonical contour ordering: closed loops first, then open polylines, ordered by vertices.
    contours.sort_unstable_by(|a, b| {
        b.closed
            .cmp(&a.closed)
            .then_with(|| cmp_pts(&pts[a.start..a.start + a.len], &pts[b.start..b.start + b.len]))
    });

    // 4. Compute tight bounding box in mm and viewBox layout.
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut closed_count = 0usize;
    let mut open_count = 0usize;
    for c in contours.iter() {
        if c.closed {
            closed_count += 1;
        } else {
            open_count += 1;
        }
        for &p in &pts[c.start..c.start + c.len] {
            if p[0] < min_x {
                min_x = p[0];
            }
            if p[0] > max_x {
                max_x = p[0];
            }
            if p[1] < min_y {
                min_y = p[1];
            }
            if p[1] > max_y {
                max_y = p[1];
            }
        }
    }
    let has_contours = !contours.is_empty();
    if !has_contours {
        min_x = grid_min_x;
        min_y = grid_min_y;
        max_x = grid_max_x;
        max_y = grid_max_y;
    }
    let w_mm = (max_x - min_x).max(0.0);
    let h_mm = (max_y - min_y).max(0.0);
    let grid_span = (grid_max_x - grid_min_x).max(grid_max_y - grid_min_y);
    let span = w_mm.max(h_mm).max(grid_span * 0.05).max(1.0);
    let pad = span * 0.25;
    let vb_x = min_x - pad;
    let vb_y = -(max_y + pad);
    let vb_w = w_mm + 2.0 * pad;
    let vb_h = h_mm + 2.0 * pad;
    let stroke_w = if opts.stroke_width > 0.0 { opts.stroke_width } else { span * 0.006 };
    let font_size = span * 0.04;
    let thin = span * 0.0025;

    let no_y_flip = plant.has(SvgPlant::NoYFlip);
    let svg_y = |y_mm: f64| -> f64 { if no_y_flip { canon(y_mm) } else { canon(-y_mm) } };

    // 5. Emit standalone SVG 1.1 via blaze-text (zero core::fmt).
    let mut out = Bytes::new(opts.alloc);
    out.str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.str("<svg xmlns=\"http://www.w3.org/2000/svg\" version=\"1.1\" width=\"");
    put_num(&mut out, vb_w);
    out.str("mm\" height=\"");
    put_num(&mut out, vb_h);
    out.str("mm\" viewBox=\"");
    put_num(&mut out, vb_x);
    out.byte(b' ');
    put_num(&mut out, vb_y);
    out.byte(b' ');
    put_num(&mut out, vb_w);
    out.byte(b' ');
    put_num(&mut out, vb_h);
    out.str("\">\n");

    if !opts.title.is_empty() {
        out.str("  <title>");
        xml_escape(opts.title, &mut out);
        out.str("</title>\n");
    }
    out.str("  <desc>blaze-cad-out 2D cross-section (1 user unit = 1 mm, Y-up to SVG Y-down)</desc>\n");

    // Clean drafting background.
    out.str("  <rect x=\"");
    put_num(&mut out, vb_x);
    out.str("\" y=\"");
    put_num(&mut out, vb_y);
    out.str("\" width=\"");
    put_num(&mut out, vb_w);
    out.str("\" height=\"");
    put_num(&mut out, vb_h);
    out.str("\" fill=\"#ffffff\"/>\n");

    // Optional engineering grid and datum axes.
    if opts.show_grid {
        let gstep = nice_step(span / 8.0);
        out.str("  <g id=\"grid\" stroke=\"#e2e8f0\" stroke-width=\"");
        put_num(&mut out, thin);
        out.str("\">\n");
        emit_grid_lines(&mut out, vb_x, vb_x + vb_w, vb_y, vb_y + vb_h, gstep, true);
        emit_grid_lines(&mut out, vb_y, vb_y + vb_h, vb_x, vb_x + vb_w, gstep, false);
        out.str("  </g>\n");

        let has_x_axis = vb_y <= 0.0 && 0.0 <= vb_y + vb_h;
        let has_y_axis = vb_x <= 0.0 && 0.0 <= vb_x + vb_w;
        if has_x_axis || has_y_axis {
            out.str("  <g id=\"axes\" stroke=\"#94a3b8\" stroke-width=\"");
            put_num(&mut out, thin * 1.4);
            out.str("\" stroke-dasharray=\"");
            put_num(&mut out, span * 0.018);
            out.byte(b' ');
            put_num(&mut out, span * 0.012);
            out.str("\">\n");
            if has_x_axis {
                emit_line(&mut out, vb_x, 0.0, vb_x + vb_w, 0.0);
            }
            if has_y_axis {
                emit_line(&mut out, 0.0, vb_y, 0.0, vb_y + vb_h);
            }
            out.str("  </g>\n");
        }
    }

    // Cross-section vector contours.
    out.str("  <g id=\"section\">\n");
    if closed_count > 0 {
        out.str("    <path id=\"silhouette\" d=\"");
        let drop_z = plant.has(SvgPlant::DropZClose);
        let mut first_sub = true;
        for c in contours.iter().filter(|c| c.closed) {
            if !first_sub {
                out.byte(b' ');
            }
            first_sub = false;
            let slice = &pts[c.start..c.start + c.len];
            for (k, &p) in slice.iter().enumerate() {
                if k == 0 {
                    out.str("M ");
                } else {
                    out.str(" L ");
                }
                put_num(&mut out, p[0]);
                out.byte(b' ');
                put_num(&mut out, svg_y(p[1]));
            }
            if !drop_z {
                out.str(" Z");
            }
        }
        out.str("\" fill=\"");
        xml_escape(opts.fill, &mut out);
        out.byte(b'"');
        if !plant.has(SvgPlant::OmitEvenOdd) {
            out.str(" fill-rule=\"evenodd\"");
        }
        out.str(" stroke=\"");
        xml_escape(opts.stroke, &mut out);
        out.str("\" stroke-width=\"");
        put_num(&mut out, stroke_w);
        out.str("\" stroke-linejoin=\"round\"/>\n");
    }
    if open_count > 0 {
        out.str("    <path id=\"open-contours\" d=\"");
        let mut first_sub = true;
        for c in contours.iter().filter(|c| !c.closed) {
            if !first_sub {
                out.byte(b' ');
            }
            first_sub = false;
            let slice = &pts[c.start..c.start + c.len];
            for (k, &p) in slice.iter().enumerate() {
                if k == 0 {
                    out.str("M ");
                } else {
                    out.str(" L ");
                }
                put_num(&mut out, p[0]);
                out.byte(b' ');
                put_num(&mut out, svg_y(p[1]));
            }
        }
        out.str("\" fill=\"none\" stroke=\"");
        xml_escape(opts.stroke, &mut out);
        out.str("\" stroke-width=\"");
        put_num(&mut out, stroke_w);
        out.str("\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n");
    }
    out.str("  </g>\n");

    // Overall width & height dimension callouts (in mm).
    if opts.show_dimensions && has_contours {
        let dim_off = pad * 0.42;
        let tick = span * 0.015;
        let top_y = -(max_y + dim_off);
        let right_x = max_x + dim_off;
        out.str("  <g id=\"dimensions\" stroke=\"#475569\" stroke-width=\"");
        put_num(&mut out, thin * 1.2);
        out.str("\" fill=\"#334155\" font-family=\"monospace\" font-size=\"");
        put_num(&mut out, font_size);
        out.str("\">\n");
        // Width callout above geometry.
        emit_line(&mut out, min_x, -max_y - pad * 0.08, min_x, top_y - tick);
        emit_line(&mut out, max_x, -max_y - pad * 0.08, max_x, top_y - tick);
        emit_line(&mut out, min_x, top_y, max_x, top_y);
        emit_line(&mut out, min_x - tick, top_y + tick, min_x + tick, top_y - tick);
        emit_line(&mut out, max_x - tick, top_y + tick, max_x + tick, top_y - tick);
        out.str("    <text x=\"");
        put_num(&mut out, 0.5 * (min_x + max_x));
        out.str("\" y=\"");
        put_num(&mut out, top_y - font_size * 0.35);
        out.str("\" text-anchor=\"middle\" stroke=\"none\">");
        out.float_fixed(canon(w_mm), 2);
        out.str(" mm</text>\n");

        // Height callout right of geometry.
        emit_line(&mut out, max_x + pad * 0.08, -min_y, right_x + tick, -min_y);
        emit_line(&mut out, max_x + pad * 0.08, -max_y, right_x + tick, -max_y);
        emit_line(&mut out, right_x, -min_y, right_x, -max_y);
        emit_line(&mut out, right_x - tick, -min_y + tick, right_x + tick, -min_y - tick);
        emit_line(&mut out, right_x - tick, -max_y + tick, right_x + tick, -max_y - tick);
        let mid_y = -0.5 * (min_y + max_y);
        let label_x = right_x + font_size * 0.45;
        out.str("    <text x=\"");
        put_num(&mut out, label_x);
        out.str("\" y=\"");
        put_num(&mut out, mid_y);
        out.str("\" text-anchor=\"middle\" dominant-baseline=\"central\" transform=\"rotate(-90 ");
        put_num(&mut out, label_x);
        out.byte(b' ');
        put_num(&mut out, mid_y);
        out.str(")\" stroke=\"none\">");
        out.float_fixed(canon(h_mm), 2);
        out.str(" mm</text>\n");
        out.str("  </g>\n");
    }

    // Scale bar (bottom-left margin).
    {
        let bar_mm = nice_step(vb_w * 0.2);
        let bar_x = vb_x + pad * 0.28;
        let bar_y = vb_y + vb_h - pad * 0.28;
        let tick = span * 0.014;
        out.str("  <g id=\"scale-bar\" stroke=\"#0f172a\" stroke-width=\"");
        put_num(&mut out, thin * 1.6);
        out.str("\" fill=\"#0f172a\" font-family=\"monospace\" font-size=\"");
        put_num(&mut out, font_size * 0.9);
        out.str("\">\n");
        emit_line(&mut out, bar_x, bar_y, bar_x + bar_mm, bar_y);
        emit_line(&mut out, bar_x, bar_y - tick, bar_x, bar_y + tick);
        emit_line(&mut out, bar_x + bar_mm, bar_y - tick, bar_x + bar_mm, bar_y + tick);
        out.str("    <text x=\"");
        put_num(&mut out, bar_x + 0.5 * bar_mm);
        out.str("\" y=\"");
        put_num(&mut out, bar_y - tick - font_size * 0.25);
        out.str("\" text-anchor=\"middle\" stroke=\"none\">");
        put_num(&mut out, bar_mm);
        out.str(" mm</text>\n");
        out.str("  </g>\n");
    }

    out.str("</svg>\n");
    out.finish().map_err(SvgError::Alloc)
}

fn emit_line(out: &mut Bytes<'_>, x1: f64, y1: f64, x2: f64, y2: f64) {
    out.str("    <line x1=\"");
    put_num(out, x1);
    out.str("\" y1=\"");
    put_num(out, y1);
    out.str("\" x2=\"");
    put_num(out, x2);
    out.str("\" y2=\"");
    put_num(out, y2);
    out.str("\"/>\n");
}

fn emit_grid_lines(out: &mut Bytes<'_>, lo: f64, hi: f64, cross_lo: f64, cross_hi: f64, step: f64, vertical: bool) {
    if !(step > 0.0 && step.is_finite()) {
        return;
    }
    let ratio = lo / step;
    if !(ratio.is_finite() && ratio.abs() < 4.5e15) {
        return;
    }
    let mut k = ratio as i64;
    if (k as f64) * step < lo {
        k += 1;
    }
    for _ in 0..64 {
        let coord = (k as f64) * step;
        if coord > hi {
            break;
        }
        if vertical {
            emit_line(out, coord, cross_lo, coord, cross_hi);
        } else {
            emit_line(out, cross_lo, coord, cross_hi, coord);
        }
        k += 1;
    }
}
