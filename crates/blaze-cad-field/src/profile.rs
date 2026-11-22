//! TYPED 2D PROFILES: what an extrusion, a revolution, a loft station and a sweep take. A
//! profile lies in its own `(x, y)` plane; its field is negative inside.
//!
//! | profile | field | how it is evaluated |
//! |---|---|---|
//! | circle, rectangle, rounded rectangle | exact | a closed form in the tape |
//! | superellipse | a bound: `(|(x/a, y/b)|_e - 1) min(a, b) / L_e`, `L_e = max(1, 2^(1/e - 1/2))` | a closed form |
//! | polygon, lines-and-arcs path | exact | one `Path2` table op: distance to the nearest segment or arc, sign by crossings |
//! | NACA 4-digit | exact for its polygon | the section flattened to `points_per_side` cosine-spaced points a side, closed trailing edge |
//! | cubic Bezier path | exact for its polygon | each cubic flattened to `steps` chords |
//!
//! Every coordinate is a [`Length`] (whole nanometres); an arc's sweep is an [`Angle`]; flattened
//! points are snapped to the nanometre, so a profile's key is integers only.

use blaze_math::Vec2;
use blaze_units::{Angle, Length};

use crate::error::{CadError, Name, NodeKind, Value};
use crate::level::Level;

/// A point of a profile.
pub type Point2 = [Length; 2];

/// One piece of a lines-and-arcs path, from the previous point.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Segment {
    /// A straight line to `to`.
    Line { to: Point2 },
    /// A circular arc to `to` sweeping `sweep` (positive counterclockwise), strictly between
    /// minus and plus a whole turn.
    Arc { to: Point2, sweep: Angle },
}

/// One cubic Bezier piece, from the previous point.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cubic {
    pub c1: Point2,
    pub c2: Point2,
    pub to: Point2,
}

/// A profile as authored. The path forms are closed: the last point joins the first.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Profile<'s> {
    Circle { radius: Length },
    Rect { width: Length, height: Length },
    RoundedRect { width: Length, height: Length, radius: Length },
    Polygon { points: &'s [Point2] },
    Path { start: Point2, segments: &'s [Segment] },
    /// `|x/a|^e + |y/b|^e = 1` with half-widths `a`, `b`; `exponent >= 1` (convex).
    Superellipse { width: Length, height: Length, exponent: f64 },
    /// NACA `mptt`: `digits = [m, p, t, t]`, the chord along `+x` from the origin.
    Naca4 { digits: [u8; 4], chord: Length, points_per_side: u32 },
    Bezier { start: Point2, segments: &'s [Cubic], steps: u32 },
}

/// The closed-form profiles, as the tape lowers them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Closed {
    Circle { r: i64 },
    Rect { hw: i64, hh: i64, round: i64 },
    /// Exponent in millionths.
    Superellipse { hw: i64, hh: i64, e_ppm: i64 },
    /// Rows `[start, len)` of the graph's segment table.
    Path { start: u32, len: u32 },
}

/// One row of the segment table: a line (`b == 0`) or an arc with centre `(cx, cy)`, radius `r`
/// and bulge `b = tan(sweep / 4)`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Seg {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub cx: f64,
    pub cy: f64,
    pub r: f64,
    pub b: f64,
}

/// Values per segment row in a tape table.
pub(crate) const SEG_WIDTH: usize = 8;

impl Seg {
    pub(crate) fn to_row(self) -> [f64; SEG_WIDTH] {
        [self.x0, self.y0, self.x1, self.y1, self.cx, self.cy, self.r, self.b]
    }

    fn line(a: [i64; 2], b: [i64; 2]) -> Seg {
        let (x0, y0, x1, y1) = (nm(a[0]), nm(a[1]), nm(b[0]), nm(b[1]));
        Seg { x0, y0, x1, y1, cx: 0.0, cy: 0.0, r: 0.0, b: 0.0 }
    }

    fn arc(a: [i64; 2], b: [i64; 2], sweep: Angle) -> Seg {
        let (x0, y0, x1, y1) = (nm(a[0]), nm(a[1]), nm(b[0]), nm(b[1]));
        let bulge = blaze_math::tan(sweep.as_radians() / 4.0);
        let (ex, ey) = (x1 - x0, y1 - y0);
        let c = blaze_math::sqrt(ex * ex + ey * ey);
        // The centre is the chord's midpoint plus the left normal times (c/2) cot(sweep/2),
        // and cot(sweep/2) = (1 - b^2) / (2b).
        let k = (1.0 - bulge * bulge) / (2.0 * bulge) * 0.5;
        let (cx, cy) = ((x0 + x1) * 0.5 - ey * k, (y0 + y1) * 0.5 + ex * k);
        let r = c * (1.0 + bulge * bulge) / (4.0 * bulge.abs());
        Seg { x0, y0, x1, y1, cx, cy, r, b: bulge }
    }
}

fn nm(v: i64) -> f64 {
    v as f64 / 1e9
}

/// The kinds of profile, for a digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ProfileTag {
    Circle = 1,
    Rect = 2,
    RoundedRect = 3,
    Polygon = 4,
    Path = 5,
    Superellipse = 6,
    Naca4 = 7,
    Bezier = 8,
}

/// A profile checked and reduced to integers: what the graph keeps.
pub(crate) struct Reduced {
    pub tag: ProfileTag,
    /// The authored parameters as integers (nanometres, micro-arcseconds, counts, millionths):
    /// what the digest hashes, in order.
    pub key: [i64; 4],
    pub level: Level,
    /// Half-extents box in the profile plane, metres: `[lo_x, lo_y, hi_x, hi_y]`.
    pub bounds: [f64; 4],
}

/// What a profile check refuses, with the profile kind named.
fn bad(name: Option<Name>, arg: &'static str, value: Value, accepted: &'static str) -> CadError {
    CadError::Argument { kind: NodeKind::Profile, name, arg, value, accepted }
}

fn positive(name: Option<Name>, arg: &'static str, l: Length) -> Result<i64, CadError> {
    if l.as_nm() > 0 { Ok(l.as_nm()) } else { Err(bad(name, arg, Value::Length(l), "a length above zero")) }
}

/// Checks `p`, pushes its path rows (if any) through `push_seg` and its authored points (if any)
/// through `push_key` (each value the digest must hash beyond `Reduced::key`), and returns the
/// reduced form. `most_points` bounds a path's rows.
pub(crate) fn reduce(
    p: &Profile<'_>,
    name: Option<Name>,
    most_points: u32,
    mut push_seg: impl FnMut(Seg) -> Result<(), CadError>,
    mut push_key: impl FnMut(i64),
) -> Result<(Reduced, Option<Closed>), CadError> {
    let too_many = |n: u64| bad(name, "points", Value::Count(n), "at most Limits::most_profile_points");
    match *p {
        Profile::Circle { radius } => {
            let r = positive(name, "radius", radius)?;
            let rf = nm(r);
            Ok((
                Reduced { tag: ProfileTag::Circle, key: [r, 0, 0, 0], level: Level::Exact, bounds: [-rf, -rf, rf, rf] },
                Some(Closed::Circle { r }),
            ))
        }
        Profile::Rect { width, height } => {
            let (w, h) = (positive(name, "width", width)?, positive(name, "height", height)?);
            let (hw, hh) = (nm(w) * 0.5, nm(h) * 0.5);
            Ok((
                Reduced { tag: ProfileTag::Rect, key: [w, h, 0, 0], level: Level::Exact, bounds: [-hw, -hh, hw, hh] },
                Some(Closed::Rect { hw: w / 2, hh: h / 2, round: 0 }),
            ))
        }
        Profile::RoundedRect { width, height, radius } => {
            let (w, h) = (positive(name, "width", width)?, positive(name, "height", height)?);
            let r = radius.as_nm();
            if r < 0 || 2 * r > w.min(h) {
                return Err(bad(name, "radius", Value::Length(radius), "from zero to half the smaller side"));
            }
            let (hw, hh) = (nm(w) * 0.5, nm(h) * 0.5);
            Ok((
                Reduced { tag: ProfileTag::RoundedRect, key: [w, h, r, 0], level: Level::Exact, bounds: [-hw, -hh, hw, hh] },
                Some(Closed::Rect { hw: w / 2, hh: h / 2, round: r }),
            ))
        }
        Profile::Superellipse { width, height, exponent } => {
            let (w, h) = (positive(name, "width", width)?, positive(name, "height", height)?);
            if !(exponent >= 1.0 && exponent <= 1000.0) {
                return Err(bad(name, "exponent", Value::Ratio(exponent), "from 1 (a diamond) to 1000"));
            }
            let e_ppm = core::f64::math::round(exponent * 1e6) as i64;
            let (hw, hh) = (nm(w) * 0.5, nm(h) * 0.5);
            Ok((
                Reduced { tag: ProfileTag::Superellipse, key: [w, h, e_ppm, 0], level: Level::Bound, bounds: [-hw, -hh, hw, hh] },
                Some(Closed::Superellipse { hw: w / 2, hh: h / 2, e_ppm }),
            ))
        }
        Profile::Polygon { points } => {
            if points.len() < 3 {
                return Err(bad(name, "points", Value::Count(points.len() as u64), "at least 3"));
            }
            if points.len() as u64 > u64::from(most_points) {
                return Err(too_many(points.len() as u64));
            }
            let mut bounds = EMPTY2;
            for (i, a) in points.iter().enumerate() {
                let b = points[(i + 1) % points.len()];
                let (pa, pb) = ([a[0].as_nm(), a[1].as_nm()], [b[0].as_nm(), b[1].as_nm()]);
                push_key(pa[0]);
                push_key(pa[1]);
                grow2(&mut bounds, pa);
                push_seg(Seg::line(pa, pb))?;
            }
            let n = points.len() as i64;
            Ok((Reduced { tag: ProfileTag::Polygon, key: [n, 0, 0, 0], level: Level::Exact, bounds }, None))
        }
        Profile::Path { start, segments } => {
            if segments.is_empty() {
                return Err(bad(name, "segments", Value::Count(0), "at least 1"));
            }
            if segments.len() as u64 > u64::from(most_points) {
                return Err(too_many(segments.len() as u64));
            }
            let mut at = [start[0].as_nm(), start[1].as_nm()];
            let first = at;
            let mut bounds = EMPTY2;
            push_key(at[0]);
            push_key(at[1]);
            for (i, s) in segments.iter().enumerate() {
                let last = i + 1 == segments.len();
                match *s {
                    Segment::Line { to } => {
                        let to = [to[0].as_nm(), to[1].as_nm()];
                        push_key(0);
                        push_key(to[0]);
                        push_key(to[1]);
                        grow2(&mut bounds, at);
                        push_seg(Seg::line(at, to))?;
                        at = to;
                    }
                    Segment::Arc { to, sweep } => {
                        let turn = Angle::<blaze_units::Plain>::PER_TURN;
                        let s = sweep.as_microarcsec();
                        if s == 0 || s <= -turn || s >= turn {
                            return Err(bad(name, "sweep", Value::Angle(sweep), "strictly between minus and plus a turn, not zero"));
                        }
                        let to = [to[0].as_nm(), to[1].as_nm()];
                        if to == at {
                            return Err(bad(name, "arc end", Value::Length(Length::nm(0)), "an arc whose ends differ"));
                        }
                        push_key(1);
                        push_key(s);
                        push_key(to[0]);
                        push_key(to[1]);
                        let seg = Seg::arc(at, to, sweep);
                        grow2(&mut bounds, at);
                        // The arc stays within its circle's box.
                        grow2f(&mut bounds, [seg.cx - seg.r, seg.cy - seg.r]);
                        grow2f(&mut bounds, [seg.cx + seg.r, seg.cy + seg.r]);
                        push_seg(seg)?;
                        at = to;
                    }
                }
                if last && at != first {
                    grow2(&mut bounds, at);
                    push_seg(Seg::line(at, first))?;
                }
            }
            let n = segments.len() as i64;
            Ok((Reduced { tag: ProfileTag::Path, key: [n, 0, 0, 0], level: Level::Exact, bounds }, None))
        }
        Profile::Naca4 { digits, chord, points_per_side } => {
            let c = positive(name, "chord", chord)?;
            if digits.iter().any(|d| *d > 9) {
                return Err(bad(name, "digits", Value::Count(u64::from(digits.iter().copied().max().unwrap_or(0))), "four decimal digits"));
            }
            let t = f64::from(digits[2] * 10 + digits[3]) / 100.0;
            if t == 0.0 {
                return Err(bad(name, "thickness digits", Value::Count(0), "a thickness of 1 to 99 percent"));
            }
            if points_per_side < 4 || 2 * u64::from(points_per_side) > u64::from(most_points) {
                return Err(bad(name, "points_per_side", Value::Count(u64::from(points_per_side)), "4 or more, twice it within Limits::most_profile_points"));
            }
            let m = f64::from(digits[0]) / 100.0;
            let pp = f64::from(digits[1]) / 10.0;
            let cf = nm(c);
            let n = points_per_side;
            // Upper side from the trailing edge to the leading edge, then the lower side back:
            // 2n - 1 distinct points (the leading edge once), the trailing edge closed.
            let mut prev: Option<[i64; 2]> = None;
            let mut first: Option<[i64; 2]> = None;
            let mut bounds = EMPTY2;
            for k in 0..(2 * n - 1) {
                let (i, upper) = if k < n { (n - 1 - k, true) } else { (k - n + 1, false) };
                let beta = core::f64::consts::PI * f64::from(i) / f64::from(n - 1);
                let x = (1.0 - blaze_math::cos(beta)) * 0.5;
                let pt = naca_point(m, pp, t, x, upper);
                let q = [snap(pt.x * cf), snap(pt.y * cf)];
                grow2(&mut bounds, q);
                if let Some(a) = prev
                    && a != q
                {
                    push_seg(Seg::line(a, q))?;
                }
                if first.is_none() {
                    first = Some(q);
                }
                prev = Some(q);
            }
            if let (Some(a), Some(f)) = (prev, first)
                && a != f
            {
                push_seg(Seg::line(a, f))?;
            }
            let d = i64::from(digits[0]) * 1000 + i64::from(digits[1]) * 100 + i64::from(digits[2]) * 10 + i64::from(digits[3]);
            Ok((Reduced { tag: ProfileTag::Naca4, key: [d, c, i64::from(n), 0], level: Level::Exact, bounds }, None))
        }
        Profile::Bezier { start, segments, steps } => {
            if segments.is_empty() {
                return Err(bad(name, "segments", Value::Count(0), "at least 1"));
            }
            if steps == 0 || segments.len() as u64 * u64::from(steps) > u64::from(most_points) {
                return Err(bad(name, "steps", Value::Count(u64::from(steps)), "1 or more, segments times steps within Limits::most_profile_points"));
            }
            let mut at = [start[0].as_nm(), start[1].as_nm()];
            let first = at;
            let mut bounds = EMPTY2;
            push_key(at[0]);
            push_key(at[1]);
            for s in segments {
                let p0 = Vec2::new(nm(at[0]), nm(at[1]));
                let c1 = Vec2::new(s.c1[0].as_m(), s.c1[1].as_m());
                let c2 = Vec2::new(s.c2[0].as_m(), s.c2[1].as_m());
                let p3 = Vec2::new(s.to[0].as_m(), s.to[1].as_m());
                for v in [s.c1, s.c2, s.to] {
                    push_key(v[0].as_nm());
                    push_key(v[1].as_nm());
                }
                for k in 1..=steps {
                    let t = f64::from(k) / f64::from(steps);
                    let u = 1.0 - t;
                    let p = p0 * (u * u * u) + c1 * (3.0 * u * u * t) + c2 * (3.0 * u * t * t) + p3 * (t * t * t);
                    let q = if k == steps { [s.to[0].as_nm(), s.to[1].as_nm()] } else { [snap(p.x), snap(p.y)] };
                    grow2(&mut bounds, at);
                    if q != at {
                        push_seg(Seg::line(at, q))?;
                    }
                    at = q;
                }
            }
            grow2(&mut bounds, at);
            if at != first {
                push_seg(Seg::line(at, first))?;
            }
            let n = segments.len() as i64;
            Ok((Reduced { tag: ProfileTag::Bezier, key: [n, i64::from(steps), 0, 0], level: Level::Exact, bounds }, None))
        }
    }
}

const EMPTY2: [f64; 4] = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];

fn grow2(b: &mut [f64; 4], p: [i64; 2]) {
    grow2f(b, [nm(p[0]), nm(p[1])]);
}

fn grow2f(b: &mut [f64; 4], p: [f64; 2]) {
    b[0] = b[0].min(p[0]);
    b[1] = b[1].min(p[1]);
    b[2] = b[2].max(p[0]);
    b[3] = b[3].max(p[1]);
}

fn snap(m: f64) -> i64 {
    core::f64::math::round(m * 1e9) as i64
}

/// A point of the unit-chord NACA 4-digit section at `x` (closed trailing edge, -0.1036).
fn naca_point(m: f64, p: f64, t: f64, x: f64, upper: bool) -> Vec2 {
    let sx = blaze_math::sqrt(x);
    let yt = 5.0 * t * (0.2969 * sx - 0.1260 * x - 0.3516 * x * x + 0.2843 * x * x * x - 0.1036 * x * x * x * x);
    let (yc, dyc) = if m == 0.0 || p == 0.0 {
        (0.0, 0.0)
    } else if x < p {
        (m / (p * p) * (2.0 * p * x - x * x), 2.0 * m / (p * p) * (p - x))
    } else {
        let q = (1.0 - p) * (1.0 - p);
        (m / q * ((1.0 - 2.0 * p) + 2.0 * p * x - x * x), 2.0 * m / q * (p - x))
    };
    let th = blaze_math::atan(dyc);
    let (s, c) = blaze_math::sin_cos(th);
    if upper { Vec2::new(x - yt * s, yc + yt * c) } else { Vec2::new(x + yt * s, yc - yt * c) }
}

/// The signed distance from `(x, y)` to the closed path of `rows` (each [`SEG_WIDTH`] values),
/// and the unit direction of its gradient. Lines and arcs; the sign is by crossings of the
/// chords, with each arc's circular segment flipping it.
pub(crate) fn path_field(rows: &[f64], x: f64, y: f64) -> (f64, f64, f64) {
    let mut best = f64::INFINITY;
    let (mut dx, mut dy) = (0.0, 0.0);
    let mut inside = false;
    let mut consider = |d2: f64, vx: f64, vy: f64, best: &mut f64| {
        if d2 < *best {
            *best = d2;
            dx = vx;
            dy = vy;
        }
    };
    for r in rows.chunks_exact(SEG_WIDTH) {
        let (x0, y0, x1, y1, cx, cy, rad, b) = (r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
        if (y0 > y) != (y1 > y) {
            let xi = x0 + (y - y0) * (x1 - x0) / (y1 - y0);
            if x < xi {
                inside = !inside;
            }
        }
        let (ex, ey) = (x1 - x0, y1 - y0);
        if b == 0.0 {
            let (px, py) = (x - x0, y - y0);
            let ee = ex * ex + ey * ey;
            let t = if ee > 0.0 { ((px * ex + py * ey) / ee).clamp(0.0, 1.0) } else { 0.0 };
            let (qx, qy) = (px - ex * t, py - ey * t);
            consider(qx * qx + qy * qy, qx, qy, &mut best);
        } else {
            let (qx, qy) = (x - cx, y - cy);
            let (ax, ay, bx, by) = if b > 0.0 { (x0 - cx, y0 - cy, x1 - cx, y1 - cy) } else { (x1 - cx, y1 - cy, x0 - cx, y0 - cy) };
            let cross = |ux: f64, uy: f64, vx: f64, vy: f64| ux * vy - uy * vx;
            let in_sector = if b.abs() <= 1.0 {
                cross(ax, ay, qx, qy) >= 0.0 && cross(qx, qy, bx, by) >= 0.0
            } else {
                !(cross(bx, by, qx, qy) > 0.0 && cross(qx, qy, ax, ay) > 0.0)
            };
            let qq = qx * qx + qy * qy;
            let ql = blaze_math::sqrt(qq);
            if in_sector && ql > 0.0 {
                let gap = ql - rad;
                consider(gap * gap, qx / ql * gap, qy / ql * gap, &mut best);
            } else {
                consider((x - x0) * (x - x0) + (y - y0) * (y - y0), x - x0, y - y0, &mut best);
                consider((x - x1) * (x - x1) + (y - y1) * (y - y1), x - x1, y - y1, &mut best);
            }
            let side = ex * (y - y0) - ey * (x - x0);
            let bulge_side = if b > 0.0 { side < 0.0 } else { side > 0.0 };
            if qq < rad * rad && bulge_side {
                inside = !inside;
            }
        }
    }
    let d = blaze_math::sqrt(best);
    let (gx, gy) = if d > 0.0 { (dx / d, dy / d) } else { (0.0, 0.0) };
    if inside { (-d, -gx, -gy) } else { (d, gx, gy) }
}
