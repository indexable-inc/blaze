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
use crate::vector::Seg;
use std::sync::OnceLock;

/// The vendored marks: name, then the SVG file (a 24 by 24 viewBox, as Simple Icons draws every mark).
const SOURCES: &[(&str, &str)] = &[
    ("claude", include_str!("../icons/claude.svg")),
    ("openai", include_str!("../icons/openai.svg")),
    ("cursor", include_str!("../icons/cursor.svg")),
    // Not a brand: the tree's disclosure chevron (an SF-like thin chevron.right, drawn here), turned open by its spring.
    ("chevron", include_str!("../icons/chevron.svg")),
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
        let mut out =
            Marks { names: Vec::new(), marks: Vec::new(), curves: Vec::new(), bands: Vec::new() };
        for &(name, svg) in SOURCES {
            let d = path_data(svg).unwrap_or_else(|| panic!("icons/{name}.svg has no path"));
            let contours = parse(d).unwrap_or_else(|e| panic!("icons/{name}.svg: {e}"));
            let o =
                Outline::new(contours).unwrap_or_else(|| panic!("icons/{name}.svg draws nothing"));
            let (centroid, spread) = ink(&o);
            let p = slug::pack(&o, out.curves.len() as u32);
            let at = out.bands.len() as u32;
            out.curves.extend_from_slice(&p.curves);
            out.bands.extend_from_slice(&p.bands);
            out.names.push(name);
            out.marks.push(Mark {
                bbox: p.bbox,
                band: p.band,
                gdata: [at, p.max[0], p.max[1], 0],
                centroid,
                spread,
            });
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
    let s = ((side * SPREAD / m.spread.max(1e-6)) * (side * FILL / long))
        .sqrt()
        .min(side * MAX_FILL / long);
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
        .flat_map(|q| (0..8).map(move |i| (q.at(i as f32 / 8.0), q.at((i + 1) as f32 / 8.0))))
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

/// SVG path data `d` (viewBox units, y down) as closed contours of quadratics in ems (y up: the viewBox's foot is 0),
/// read by the one parser (vector.rs).
pub fn parse(d: &str) -> Result<Vec<Vec<Quad>>, String> {
    let mut pb = PathBuilder::default();
    let em = |p: [f64; 2]| [p[0] / VIEW, (VIEW - p[1]) / VIEW];
    for s in crate::vector::segments(d)? {
        match s {
            Seg::Move(p) => pb.move_to(em(p)),
            Seg::Line(p) => pb.line_to(em(p)),
            Seg::Quad(c, p) => pb.quad_to(em(c), em(p)),
            Seg::Cubic(c1, c2, p) => pb.cubic_to(em(c1), em(c2), em(p)),
            Seg::Close => pb.close(),
        }
    }
    Ok(pb.finish())
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
