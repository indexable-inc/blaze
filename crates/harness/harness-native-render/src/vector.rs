//! SVG path data, the one parser: `d` (M L H V C S Q T A Z, absolute and relative; arcs become cubics) read into
//! absolute [`Seg`]s once, then flattened into polylines in output points at draw time ([`flatten`]), with as many pieces
//! as the curve needs at the scale it lands at. The brand marks (icons.rs) feed the same segments to Slug's PathBuilder;
//! a `Vector` node's fill (`PathInst`) and stroke (`LineInst`) take the flattened rings.

/// One element of a path, every point absolute (user units, y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move([f64; 2]),
    Line([f64; 2]),
    Quad([f64; 2], [f64; 2]),
    Cubic([f64; 2], [f64; 2], [f64; 2]),
    Close,
}

/// One subpath flattened: its points in output units, and whether it closes (a `Z`), which a stroke draws back to the
/// start. A fill closes every subpath.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ring {
    pub pts: Vec<[f32; 2]>,
    pub closed: bool,
}

/// A tokenizer over SVG path data: commands, numbers (`-.5.5` is two), and arc flags (one digit each, `00` is two).
struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',')
        {
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
        if matches!(at(i), b'e' | b'E')
            && (at(i + 1).is_ascii_digit()
                || (matches!(at(i + 1), b'-' | b'+') && at(i + 2).is_ascii_digit()))
        {
            i += 2;
            while at(i).is_ascii_digit() {
                i += 1;
            }
        }
        self.i = i;
        std::str::from_utf8(&self.s[start..i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| format!("bad number at byte {start}"))
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

/// SVG path data `d` as absolute segments. Err names the byte where it stops making sense.
pub fn segments(d: &str) -> Result<Vec<Seg>, String> {
    let mut lx = Lexer { s: d.as_bytes(), i: 0 };
    let mut out = Vec::new(); // alloc: a path's segments, cached by the painter for as long as its node shows the path
    if d.trim().is_empty() {
        return Ok(out);
    }
    let (mut cur, mut start) = ([0.0f64; 2], [0.0f64; 2]);
    // The last curve's second control point (for S and T), and which kind it was.
    let mut last_c: Option<(u8, [f64; 2])> = None;
    let mut cmd = lx.command().ok_or("path data does not start with a command")?;
    loop {
        let rel = cmd.is_ascii_lowercase();
        let up = cmd.to_ascii_uppercase();
        let base = if rel { cur } else { [0.0, 0.0] };
        let pt = |lx: &mut Lexer<'_>| -> Result<[f64; 2], String> {
            Ok([base[0] + lx.number()?, base[1] + lx.number()?])
        };
        let reflect = |kind: u8| match last_c {
            Some((k, c)) if k == kind => [2.0 * cur[0] - c[0], 2.0 * cur[1] - c[1]],
            _ => cur,
        };
        match up {
            b'M' => {
                let p = pt(&mut lx)?;
                out.push(Seg::Move(p));
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
                out.push(Seg::Line(p));
                cur = p;
                last_c = None;
            }
            b'H' => {
                let x = lx.number()? + if rel { cur[0] } else { 0.0 };
                cur = [x, cur[1]];
                out.push(Seg::Line(cur));
                last_c = None;
            }
            b'V' => {
                let y = lx.number()? + if rel { cur[1] } else { 0.0 };
                cur = [cur[0], y];
                out.push(Seg::Line(cur));
                last_c = None;
            }
            b'C' | b'S' => {
                let c1 = if up == b'C' { pt(&mut lx)? } else { reflect(b'C') };
                let c2 = pt(&mut lx)?;
                let p = pt(&mut lx)?;
                out.push(Seg::Cubic(c1, c2, p));
                cur = p;
                last_c = Some((b'C', c2));
            }
            b'Q' | b'T' => {
                let c = if up == b'Q' { pt(&mut lx)? } else { reflect(b'Q') };
                let p = pt(&mut lx)?;
                out.push(Seg::Quad(c, p));
                cur = p;
                last_c = Some((b'Q', c));
            }
            b'A' => {
                let (rx, ry, rot) = (lx.number()?, lx.number()?, lx.number()?);
                let (large, sweep) = (lx.flag()?, lx.flag()?);
                let p = pt(&mut lx)?;
                for [c1, c2, e] in arc_cubics(cur, [rx, ry], rot, large, sweep, p) {
                    out.push(Seg::Cubic(c1, c2, e));
                }
                cur = p;
                last_c = None;
            }
            b'Z' => {
                out.push(Seg::Close);
                cur = start;
                last_c = None;
            }
            _ => return Err(format!("unknown command {:?} at byte {}", cmd as char, lx.i.saturating_sub(1))),
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
    Ok(out)
}

/// The subpaths of `segs` as polylines: each point taken through `to` (user units to output points), curves cut into
/// pieces no further than `tol` output points from the curve (measured after `to`, so a curve drawn small takes few).
/// A subpath of one point is dropped; at most `max_pts` points come out in all (the rest of the path is cut, and
/// `cut` says so).
pub fn flatten(segs: &[Seg], to: impl Fn([f64; 2]) -> [f32; 2], tol: f32, max_pts: usize) -> (Vec<Ring>, bool) {
    let mut rings: Vec<Ring> = Vec::new(); // alloc: one frame's rings of one node, dropped once drawn
    let mut cur = Ring::default();
    let (mut at, mut start) = ([0.0f64; 2], [0.0f64; 2]);
    let mut total = 0usize;
    let mut cut = false;
    let tol = f64::from(tol.max(0.01));
    let end = |cur: &mut Ring, rings: &mut Vec<Ring>| {
        if cur.pts.len() >= 2 {
            rings.push(std::mem::take(cur));
        } else {
            cur.pts.clear();
            cur.closed = false;
        }
    };
    let push = |cur: &mut Ring, p: [f32; 2], total: &mut usize, cut: &mut bool| {
        if *total >= max_pts {
            *cut = true;
            return;
        }
        if cur.pts.last() != Some(&p) {
            cur.pts.push(p);
            *total += 1;
        }
    };
    let dist = |a: [f32; 2], b: [f32; 2]| f64::from((a[0] - b[0]).hypot(a[1] - b[1]));
    for s in segs {
        match *s {
            Seg::Move(p) => {
                end(&mut cur, &mut rings);
                push(&mut cur, to(p), &mut total, &mut cut);
                (at, start) = (p, p);
            }
            Seg::Line(p) => {
                if cur.pts.is_empty() {
                    push(&mut cur, to(at), &mut total, &mut cut);
                }
                push(&mut cur, to(p), &mut total, &mut cut);
                at = p;
            }
            Seg::Quad(c, p) => {
                if cur.pts.is_empty() {
                    push(&mut cur, to(at), &mut total, &mut cut);
                }
                // A quadratic's deviation from its chord is a quarter of |p0 - 2c + p1|; n pieces divide it by n^2.
                let (a, cc, b) = (to(at), to(c), to(p));
                let dev = f64::from((a[0] - 2.0 * cc[0] + b[0]).hypot(a[1] - 2.0 * cc[1] + b[1])) * 0.25;
                let n = ((dev / tol).sqrt().ceil() as usize).clamp(1, 256);
                for i in 1..=n {
                    let t = i as f64 / n as f64;
                    let u = 1.0 - t;
                    let q = [0, 1].map(|k| u * u * at[k] + 2.0 * u * t * c[k] + t * t * p[k]);
                    push(&mut cur, to(q), &mut total, &mut cut);
                }
                at = p;
            }
            Seg::Cubic(c1, c2, p) => {
                if cur.pts.is_empty() {
                    push(&mut cur, to(at), &mut total, &mut cut);
                }
                let (a, k1, k2, b) = (to(at), to(c1), to(c2), to(p));
                // The control polygon bounds the curve; its excess over the chord bounds the flattening error.
                let poly = dist(a, k1) + dist(k1, k2) + dist(k2, b);
                let dev = (poly - dist(a, b)).max(0.0);
                let n = ((dev / tol).sqrt().ceil() as usize + 1).clamp(1, 256);
                for i in 1..=n {
                    let t = i as f64 / n as f64;
                    let u = 1.0 - t;
                    let q = [0, 1].map(|k| {
                        u * u * u * at[k] + 3.0 * u * u * t * c1[k] + 3.0 * u * t * t * c2[k] + t * t * t * p[k]
                    });
                    push(&mut cur, to(q), &mut total, &mut cut);
                }
                at = p;
            }
            Seg::Close => {
                cur.closed = true;
                end(&mut cur, &mut rings);
                at = start;
            }
        }
    }
    end(&mut cur, &mut rings);
    (rings, cut)
}

/// An SVG elliptical arc (endpoint form, SVG 1.1 F.6.5) from `a` to `b` as cubics (control, control, end), a quarter
/// turn or less each.
fn arc_cubics(
    a: [f64; 2],
    r: [f64; 2],
    rot_deg: f64,
    large: bool,
    sweep: bool,
    b: [f64; 2],
) -> Vec<[[f64; 2]; 3]> {
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
    let (cx, cy) =
        (cs * cxp - sn * cyp + (a[0] + b[0]) * 0.5, sn * cxp + cs * cyp + (a[1] + b[1]) * 0.5);
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
        out.push([
            [p0[0] + k * da[0], p0[1] + k * da[1]],
            [p3[0] - k * db[0], p3[1] - k * db[1]],
            p3,
        ]);
        p0 = p3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The area a path's flattened rings enclose (shoelace, signed by winding, summed).
    fn area(d: &str) -> f64 {
        let (rings, _) = flatten(&segments(d).unwrap(), |p| [p[0] as f32, p[1] as f32], 0.01, 1 << 20);
        rings
            .iter()
            .map(|r| {
                (0..r.pts.len())
                    .map(|i| {
                        let (a, b) = (r.pts[i], r.pts[(i + 1) % r.pts.len()]);
                        f64::from(a[0] * b[1] - b[0] * a[1])
                    })
                    .sum::<f64>()
                    * 0.5
            })
            .sum()
    }

    #[test]
    fn path_data_flattens_to_the_area_it_encloses() {
        assert!((area("M2 2H12V12H2Z").abs() - 100.0).abs() < 1e-3);
        assert!((area("m2 2h10v10h-10z").abs() - 100.0).abs() < 1e-3);
        assert!((area("M0 0 10 0 10 10 0 10z").abs() - 100.0).abs() < 1e-3);
        // Flattening at 0.01pt loses at most tolerance x perimeter (~31 for these) of area.
        // A pie's quarter slice: a right angle at the centre and a quarter arc of radius 10.
        let quarter = area("M0,0 L10,0 A10,10 0 0 1 0,10 Z").abs();
        assert!((quarter - std::f64::consts::PI * 25.0).abs() < 0.36, "{quarter}");
        let circle = area("M7 12a5 5 0 1 0 10 0a5 5 0 1 0-10 0z").abs();
        assert!((circle - std::f64::consts::PI * 25.0).abs() < 0.32, "{circle}");
        assert!(segments("M0 0C1 2 3 2 4 0S7-2 8 0Q9 2 10 0T12 0Z").is_ok());
        assert!(segments("M0 0X1 1").is_err());
        assert_eq!(segments("  ").unwrap(), Vec::new());
    }

    #[test]
    fn an_open_path_stays_open_and_the_cap_cuts() {
        let (rings, cut) = flatten(&segments("M0 0 L10 0 L10 10").unwrap(), |p| [p[0] as f32, p[1] as f32], 0.1, 100);
        assert_eq!(rings.len(), 1);
        assert!(!rings[0].closed && !cut);
        let (_, cut) = flatten(&segments("M0 0 L1 0 L2 0 L3 0").unwrap(), |p| [p[0] as f32, p[1] as f32], 0.1, 2);
        assert!(cut);
    }
}
