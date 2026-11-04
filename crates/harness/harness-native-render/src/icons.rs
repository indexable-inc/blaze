//! Brand marks (icons/*.svg, Simple Icons, CC0: icons/SOURCE.txt) as Slug outlines: each SVG's path data is parsed at
//! load (M L H V C S Q T A Z, absolute and relative; arcs become cubics), packed by slug.rs into one curve buffer and one
//! band buffer that the glyph pipeline reads (gpu.rs), and drawn as an outline glyph (`GlyphInst.kind.w` 1), so a mark
//! is as crisp at every zoom as the curves are. A `ui.Icon` node (its `Mark {name}`) names one; app.rs places it.
//!
//! Sizing is optical, not by bounding box alone: a mark's scale is the geometric mean of the scale that gives its ink a
//! set spread (the root mean square distance of its covered area from its centroid) and the scale that gives its bounds
//! a set share of the box, capped so its bounds always fit; it is centred halfway between its bounds' centre and its
//! ink's centroid. A sparse starburst comes out a little smaller and a solid cube a little larger than equal bounds
//! would make them, which is what reads as one size beside the terminal glyph.
use crate::slug::{self, Outline, PathBuilder, Quad};
use std::sync::OnceLock;

/// The vendored marks: name, then the SVG file (a 24 by 24 viewBox, as Simple Icons draws every mark).
const SOURCES: &[(&str, &str)] = &[
    ("claude", include_str!("../icons/claude.svg")),
    ("openai", include_str!("../icons/openai.svg")),
    ("cursor", include_str!("../icons/cursor.svg")),
];

/// The viewBox side every Simple Icons SVG uses.
const VIEW: f64 = 24.0;

/// The ink's spread (RMS radius) and the bounds' longer side as shares of the box's side, and the most of the side the
/// bounds may take.
const SPREAD: f32 = 0.27;
const FILL: f32 = 0.80;
const MAX_FILL: f32 = 0.84;

/// One mark as the buffers hold it, with its optical centre and spread (ems: the viewBox is one em, y up).
#[derive(Clone, Debug)]
pub struct Mark {
    pub bbox: [f32; 4],
    pub band: [f32; 4],
    pub gdata: [u32; 4],
    pub centroid: [f32; 2],
    pub spread: f32,
}

pub struct Marks {
    pub names: Vec<&'static str>,
    pub marks: Vec<Mark>,
    pub curves: Vec<[f32; 4]>,
    pub bands: Vec<u32>,
}

/// Every vendored mark, parsed and packed once.
pub fn marks() -> &'static Marks {
    static M: OnceLock<Marks> = OnceLock::new();
    M.get_or_init(|| {
        let mut out = Marks { names: Vec::new(), marks: Vec::new(), curves: Vec::new(), bands: Vec::new() };
        for &(name, svg) in SOURCES {
            let d = path_data(svg).unwrap_or_else(|| panic!("icons/{name}.svg has no path"));
            let contours = parse(d).unwrap_or_else(|e| panic!("icons/{name}.svg: {e}"));
            let o = Outline::new(contours).unwrap_or_else(|| panic!("icons/{name}.svg draws nothing"));
            let (centroid, spread) = ink(&o);
            let p = slug::pack(&o, out.curves.len() as u32);
            let at = out.bands.len() as u32;
            out.curves.extend_from_slice(&p.curves);
            out.bands.extend_from_slice(&p.bands);
            out.names.push(name);
            out.marks.push(Mark { bbox: p.bbox, band: p.band, gdata: [at, p.max[0], p.max[1], 0], centroid, spread });
        }
        out
    })
}

/// The mark called `name`.
pub fn mark(name: &str) -> Option<&'static Mark> {
    let m = marks();
    m.names.iter().position(|n| *n == name).map(|i| &m.marks[i])
}

/// Where `m` goes in the box `(x, y, side)` (points, y down): its ink box in points and in ems (top-left then
/// bottom-right, as `GlyphInst.uv` takes it).
pub fn place(m: &Mark, x: f32, y: f32, side: f32) -> ([f32; 4], [f32; 4]) {
    let [x0, y0, x1, y1] = m.bbox;
    let long = (x1 - x0).max(y1 - y0);
    let s = ((side * SPREAD / m.spread.max(1e-6)) * (side * FILL / long)).sqrt().min(side * MAX_FILL / long);
    let c = [((x0 + x1) * 0.5 + m.centroid[0]) * 0.5, ((y0 + y1) * 0.5 + m.centroid[1]) * 0.5];
    let (cx, cy) = (x + side * 0.5, y + side * 0.5);
    let rect = [cx + (x0 - c[0]) * s, cy - (y1 - c[1]) * s, (x1 - x0) * s, (y1 - y0) * s];
    (rect, [x0, y1, x1, y0])
}

/// The `d` of an SVG's first `<path>`.
fn path_data(svg: &str) -> Option<&str> {
    let at = svg.find("<path")?;
    let rest = &svg[at..];
    let d = rest.find(" d=\"")? + 4;
    let end = rest[d..].find('"')?;
    Some(&rest[d..d + end])
}

/// The covered area's centroid and RMS radius about it (ems), sampled on a grid over the bounds with the nonzero rule.
fn ink(o: &Outline) -> ([f32; 2], f32) {
    const N: usize = 128;
    let segs: Vec<([f32; 2], [f32; 2])> = o
        .curves
        .iter()
        .flat_map(|q| {
            (0..8).map(move |i| (q.at(i as f32 / 8.0), q.at((i + 1) as f32 / 8.0)))
        })
        .collect();
    let [x0, y0, x1, y1] = o.bbox;
    let (dx, dy) = ((x1 - x0) / N as f32, (y1 - y0) / N as f32);
    let mut hits = Vec::new();
    for j in 0..N {
        let py = y0 + (j as f32 + 0.5) * dy;
        for i in 0..N {
            let px = x0 + (i as f32 + 0.5) * dx;
            let mut wind = 0i32;
            for &(a, b) in &segs {
                if (a[1] <= py) != (b[1] <= py) {
                    let t = (py - a[1]) / (b[1] - a[1]);
                    if a[0] + t * (b[0] - a[0]) > px {
                        wind += if b[1] > a[1] { 1 } else { -1 };
                    }
                }
            }
            if wind != 0 {
                hits.push([px, py]);
            }
        }
    }
    let n = hits.len().max(1) as f32;
    let c = hits.iter().fold([0.0, 0.0], |s, p| [s[0] + p[0] / n, s[1] + p[1] / n]);
    let r2 = hits.iter().map(|p| (p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sum::<f32>() / n;
    (c, r2.sqrt())
}

/// A tokenizer over SVG path data: commands, numbers (`-.5.5` is two), and arc flags (one digit each, `00` is two).
struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len() && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',') {
            self.i += 1;
        }
    }

    fn command(&mut self) -> Option<u8> {
        self.skip();
        let c = *self.s.get(self.i)?;
        c.is_ascii_alphabetic().then(|| {
            self.i += 1;
            c
        })
    }

    /// Another number follows (a command's next set of arguments).
    fn more(&mut self) -> bool {
        self.skip();
        self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
    }

    fn number(&mut self) -> Result<f64, String> {
        self.skip();
        let start = self.i;
        let at = |i: usize| self.s.get(i).copied().unwrap_or(0);
        let mut i = self.i;
        if matches!(at(i), b'-' | b'+') {
            i += 1;
        }
        let mut dot = false;
        while at(i).is_ascii_digit() || (at(i) == b'.' && !dot) {
            dot |= at(i) == b'.';
            i += 1;
        }
        if matches!(at(i), b'e' | b'E') && (at(i + 1).is_ascii_digit() || (matches!(at(i + 1), b'-' | b'+') && at(i + 2).is_ascii_digit())) {
            i += 2;
            while at(i).is_ascii_digit() {
                i += 1;
            }
        }
        self.i = i;
        std::str::from_utf8(&self.s[start..i]).ok().and_then(|t| t.parse().ok()).ok_or_else(|| format!("bad number at byte {start}"))
    }

    fn flag(&mut self) -> Result<bool, String> {
        self.skip();
        let c = self.s.get(self.i).copied();
        self.i += 1;
        match c {
            Some(b'0') => Ok(false),
            Some(b'1') => Ok(true),
            _ => Err(format!("bad arc flag at byte {}", self.i - 1)),
        }
    }
}

/// SVG path data `d` (viewBox units, y down) as closed contours of quadratics in ems (y up: the viewBox's foot is 0).
pub fn parse(d: &str) -> Result<Vec<Vec<Quad>>, String> {
    let mut lx = Lexer { s: d.as_bytes(), i: 0 };
    let mut pb = PathBuilder::default();
    let em = |p: [f64; 2]| [p[0] / VIEW, (VIEW - p[1]) / VIEW];
    let (mut cur, mut start) = ([0.0f64; 2], [0.0f64; 2]);
    // The last curve's second control point (for S and T), and which kind it was.
    let mut last_c: Option<(u8, [f64; 2])> = None;
    let mut cmd = lx.command().ok_or("path data does not start with a command")?;
    loop {
        let rel = cmd.is_ascii_lowercase();
        let up = cmd.to_ascii_uppercase();
        let base = if rel { cur } else { [0.0, 0.0] };
        let pt = |lx: &mut Lexer<'_>| -> Result<[f64; 2], String> { Ok([base[0] + lx.number()?, base[1] + lx.number()?]) };
        let reflect = |kind: u8| match last_c {
            Some((k, c)) if k == kind => [2.0 * cur[0] - c[0], 2.0 * cur[1] - c[1]],
            _ => cur,
        };
        match up {
            b'M' => {
                let p = pt(&mut lx)?;
                pb.move_to(em(p));
                (cur, start) = (p, p);
                last_c = None;
                // Further pairs after a moveto are linetos.
                cmd = if rel { b'l' } else { b'L' };
                if lx.more() {
                    continue;
                }
            }
            b'L' => {
                let p = pt(&mut lx)?;
                pb.line_to(em(p));
                cur = p;
                last_c = None;
            }
            b'H' => {
                let x = lx.number()? + if rel { cur[0] } else { 0.0 };
                cur = [x, cur[1]];
                pb.line_to(em(cur));
                last_c = None;
            }
            b'V' => {
                let y = lx.number()? + if rel { cur[1] } else { 0.0 };
                cur = [cur[0], y];
                pb.line_to(em(cur));
                last_c = None;
            }
            b'C' | b'S' => {
                let c1 = if up == b'C' { pt(&mut lx)? } else { reflect(b'C') };
                let c2 = pt(&mut lx)?;
                let p = pt(&mut lx)?;
                pb.cubic_to(em(c1), em(c2), em(p));
                cur = p;
                last_c = Some((b'C', c2));
            }
            b'Q' | b'T' => {
                let c = if up == b'Q' { pt(&mut lx)? } else { reflect(b'Q') };
                let p = pt(&mut lx)?;
                pb.quad_to(em(c), em(p));
                cur = p;
                last_c = Some((b'Q', c));
            }
            b'A' => {
                let (rx, ry, rot) = (lx.number()?, lx.number()?, lx.number()?);
                let (large, sweep) = (lx.flag()?, lx.flag()?);
                let p = pt(&mut lx)?;
                for [c1, c2, e] in arc_cubics(cur, [rx, ry], rot, large, sweep, p) {
                    pb.cubic_to(em(c1), em(c2), em(e));
                }
                cur = p;
                last_c = None;
            }
            b'Z' => {
                pb.close();
                cur = start;
                last_c = None;
            }
            _ => return Err(format!("unknown command {}", cmd as char)),
        }
        if up != b'Z' && lx.more() {
            continue;
        }
        match lx.command() {
            Some(c) => cmd = c,
            None if lx.i >= lx.s.len() => break,
            None => return Err(format!("stray input at byte {}", lx.i)),
        }
    }
    Ok(pb.finish())
}

/// An SVG elliptical arc (endpoint form, SVG 1.1 F.6.5) from `a` to `b` as cubics (control, control, end), a quarter
/// turn or less each.
fn arc_cubics(a: [f64; 2], r: [f64; 2], rot_deg: f64, large: bool, sweep: bool, b: [f64; 2]) -> Vec<[[f64; 2]; 3]> {
    let (mut rx, mut ry) = (r[0].abs(), r[1].abs());
    if a == b {
        return Vec::new();
    }
    if rx == 0.0 || ry == 0.0 {
        return vec![[a, b, b]];
    }
    let (sn, cs) = rot_deg.to_radians().sin_cos();
    let (hx, hy) = ((a[0] - b[0]) * 0.5, (a[1] - b[1]) * 0.5);
    let (x1, y1) = (cs * hx + sn * hy, -sn * hx + cs * hy);
    let lam = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lam > 1.0 {
        let k = lam.sqrt();
        rx *= k;
        ry *= k;
    }
    let num = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut co = (num / den).sqrt();
    if large == sweep {
        co = -co;
    }
    let (cxp, cyp) = (co * rx * y1 / ry, -co * ry * x1 / rx);
    let (cx, cy) = (cs * cxp - sn * cyp + (a[0] + b[0]) * 0.5, sn * cxp + cs * cyp + (a[1] + b[1]) * 0.5);
    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let t1 = angle(1.0, 0.0, (x1 - cxp) / rx, (y1 - cyp) / ry);
    let mut dt = angle((x1 - cxp) / rx, (y1 - cyp) / ry, (-x1 - cxp) / rx, (-y1 - cyp) / ry);
    if !sweep && dt > 0.0 {
        dt -= std::f64::consts::TAU;
    } else if sweep && dt < 0.0 {
        dt += std::f64::consts::TAU;
    }
    let n = (dt.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = dt / n as f64;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    // A point of the ellipse at parameter t, and its derivative.
    let at = |t: f64| {
        let (s, c) = t.sin_cos();
        [cx + rx * c * cs - ry * s * sn, cy + rx * c * sn + ry * s * cs]
    };
    let dir = |t: f64| {
        let (s, c) = t.sin_cos();
        [-rx * s * cs - ry * c * sn, -rx * s * sn + ry * c * cs]
    };
    let mut out = Vec::with_capacity(n);
    let mut p0 = a;
    for i in 0..n {
        let (ta, tb) = (t1 + step * i as f64, t1 + step * (i + 1) as f64);
        let p3 = if i + 1 == n { b } else { at(tb) };
        let (da, db) = (dir(ta), dir(tb));
        out.push([[p0[0] + k * da[0], p0[1] + k * da[1]], [p3[0] - k * db[0], p3[1] - k * db[1]], p3]);
        p0 = p3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(d: &str) -> f64 {
        parse(d).unwrap().iter().map(|c| slug::signed_area(c)).sum::<f64>() * VIEW * VIEW
    }

    #[test]
    fn path_data_parses_every_command_absolute_and_relative() {
        // A 10 square, absolute (H V Z) and relative (h v z, packed numbers): same area either way.
        assert!((area("M2 2H12V12H2Z").abs() - 100.0).abs() < 1e-3);
        assert!((area("m2 2h10v10h-10z").abs() - 100.0).abs() < 1e-3);
        // Implicit lineto after a moveto, and numbers that run together ("-.5.5" is -0.5 then 0.5).
        assert!((area("M0 0 10 0 10 10 0 10z").abs() - 100.0).abs() < 1e-3);
        assert_eq!(parse("M1-.5.5 2z").unwrap()[0].len(), 2);
        // A circle of radius 5 from two half arcs (flags packed with no separators) comes out at pi r^2.
        let circle = area("M7 12a5 5 0 1 0 10 0a5 5 0 1 0-10 0z").abs();
        assert!((circle - std::f64::consts::PI * 25.0).abs() < 0.05, "{circle}");
        // S reflects the last cubic's control point; T the last quadratic's; both close up.
        assert!(parse("M0 0C1 2 3 2 4 0S7-2 8 0Q9 2 10 0T12 0Z").is_ok());
        // Every vendored mark parses and packs.
        assert_eq!(marks().marks.len(), SOURCES.len());
        assert!(parse("M0 0X1 1").is_err());
    }
}
