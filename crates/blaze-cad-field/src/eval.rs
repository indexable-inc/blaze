//! THE INTERPRETERS over one [`Tape`]: points (one lane, four or eight), boxes (intervals and
//! the choices they decide), simplification by those choices, and gradients.
//!
//! CONTRACTS (each a test in `tests.rs`):
//! - [`eval_points`] gives, at every lane count, the same bits as the one-lane form: every op
//!   is written once over `blaze_math::Real` and the lane forms do the same IEEE operations in
//!   the same order (tables run per lane through the one scalar function).
//! - [`eval_box`] gives an interval holding the value at every point of the box (outward
//!   rounded throughout, by blaze-math's `Interval`), and for each choice site (`min`, `max`,
//!   `select`) which operand the point form takes at EVERY point of the box, where one does.
//! - [`simplify`] with those choices gives a tape whose value at every point of that box has
//!   the same bits as the whole tape's: a chosen site becomes the operand it takes, and the
//!   canonical emitter drops what no longer reaches the root. Outside the box it promises
//!   nothing. Its digest is the region key.
//! - [`eval_grad`] gives the value and the gradient by forward differentiation of the same
//!   ops (a kink takes the side the point form takes).

use core::simd::Simd;

use blaze_math::{Aabb3, Interval, Real, Vec3, next_down, next_up};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{AVec, Alloc, Vec};

use crate::error::{CompileError, EvalError};
use crate::plant::CadPlant;
use crate::profile::path_field;
use crate::tape::{self, GRID_BLOCK, GRID_SUPER, GridSummary, Instr, Op, Rw, Source, Table, Tape};

/// How many points one pass of the point interpreter takes: a choice the caller makes for its
/// machine (nothing here reads the CPU's features).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lanes {
    One,
    Four,
    Eight,
}

/// Points as three coordinate columns of one length.
#[derive(Clone, Copy, Debug)]
pub struct Points<'p> {
    pub x: &'p [f64],
    pub y: &'p [f64],
    pub z: &'p [f64],
}

/// The registers the interpreters work in, kept between calls so a mesher's millions of
/// calls allocate nothing.
pub struct Scratch<'a> {
    alloc: Alloc<'a>,
    one: AVec<'a, f64>,
    four: AVec<'a, Simd<f64, 4>>,
    eight: AVec<'a, Simd<f64, 8>>,
    boxes: AVec<'a, Interval>,
    choices: AVec<'a, Choice>,
    grads: AVec<'a, [f64; 4]>,
    grads_four: AVec<'a, [Simd<f64, 4>; 4]>,
    grads_eight: AVec<'a, [Simd<f64, 8>; 4]>,
}

impl<'a> Scratch<'a> {
    /// Empty registers in `alloc`.
    pub fn new(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch {
            alloc,
            one: Vec::new_in(alloc),
            four: Vec::new_in(alloc),
            eight: Vec::new_in(alloc),
            boxes: Vec::new_in(alloc),
            choices: Vec::new_in(alloc),
            grads: Vec::new_in(alloc),
            grads_four: Vec::new_in(alloc),
            grads_eight: Vec::new_in(alloc),
        }
    }

    /// The allocator the registers live in.
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }
}

/// Ensures `v` holds at least `n` initialized slots without clearing or zero-filling
/// existing capacity on repeated calls: SSA tapes write every live register before reading it.
#[inline]
fn ensure<T: Copy>(v: &mut AVec<'_, T>, n: u32, zero: T) {
    let need = n as usize;
    if v.len() < need {
        v.reserve(need - v.len());
        while v.len() < need {
            v.push(zero);
        }
    }
}

/// `if b < a { b } else { a }`.
#[inline]
fn min_r<R: Real>(a: R, b: R) -> R {
    R::select(b.lt(a), b, a)
}

/// `if a < b { b } else { a }`.
#[inline]
fn max_r<R: Real>(a: R, b: R) -> R {
    R::select(a.lt(b), b, a)
}

/// One pure op (no pool but `k`, no table) on lanes: the one definition the point interpreters
/// and constant folding share. `recip` is the planted vector bug (division by multiplying by
/// the reciprocal).
#[inline]
pub(crate) fn pure<R: Real>(op: Op, a: R, b: R, c: R, d: R, k: f64, recip: bool) -> R {
    match op {
        Op::Neg => -a,
        Op::Abs => a.abs(),
        Op::Sqrt => a.sqrt(),
        Op::Square => a * a,
        Op::Sin => blaze_math::sin(a),
        Op::Cos => blaze_math::cos(a),
        Op::Round => blaze_math::round(a),
        Op::Add => a + b,
        Op::Sub => a - b,
        Op::Mul => a * b,
        Op::Div => {
            if recip {
                a * (R::splat(1.0) / b)
            } else {
                a / b
            }
        }
        Op::Min => min_r(a, b),
        Op::Max => max_r(a, b),
        Op::Atan2 => blaze_math::atan2(a, b),
        Op::PowConst => blaze_math::pow(a, R::splat(k)),
        Op::Select => R::select(a.lt(b), c, d),
        Op::X | Op::Y | Op::Z | Op::Const | Op::Row | Op::BoxBound | Op::Path2 | Op::Grid3 => {
            unreachable!("{} is not a pure op", op.name())
        }
    }
}

/// The rows of a table.
fn rows<'t>(t: &'t Tape<'_>, i: u32) -> (&'t Table, &'t [f64]) {
    let tb = &t.tables[i as usize];
    (tb, &t.data[tb.start as usize..(tb.start + tb.len) as usize])
}

/// The grid field of table head `h` and values `v` at `p`, and its gradient: trilinear inside
/// the grid's box; outside, at distance `e` from it, `max(v(q) - e, e)` with `q` the nearest
/// point of the box (both are at most the distance to a solid inside the box).
pub(crate) fn grid_at(h: &[f64; 8], v: &[f64], p: [f64; 3]) -> (f64, [f64; 3]) {
    let n = [h[5] as usize, h[6] as usize, h[7] as usize];
    let step = h[3];
    let mut d = [0.0; 3];
    let mut i0 = [0usize; 3];
    let mut t = [0.0; 3];
    let mut e2 = 0.0;
    for a in 0..3 {
        let top = h[a] + (n[a] - 1) as f64 * step;
        let q = p[a].clamp(h[a], top);
        d[a] = p[a] - q;
        e2 += d[a] * d[a];
        let u = (q - h[a]) / step;
        let f = blaze_math::floor(u).clamp(0.0, (n[a] - 2) as f64);
        i0[a] = f as usize;
        t[a] = (u - f).clamp(0.0, 1.0);
    }
    let at = |a: usize, b: usize, c: usize| v[(i0[0] + a) + n[0] * ((i0[1] + b) + n[1] * (i0[2] + c))];
    let lerp = |p: f64, q: f64, s: f64| p + (q - p) * s;
    let (c000, c100, c010, c110) = (at(0, 0, 0), at(1, 0, 0), at(0, 1, 0), at(1, 1, 0));
    let (c001, c101, c011, c111) = (at(0, 0, 1), at(1, 0, 1), at(0, 1, 1), at(1, 1, 1));
    let x00 = lerp(c000, c100, t[0]);
    let x10 = lerp(c010, c110, t[0]);
    let x01 = lerp(c001, c101, t[0]);
    let x11 = lerp(c011, c111, t[0]);
    let y0 = lerp(x00, x10, t[1]);
    let y1 = lerp(x01, x11, t[1]);
    let val = lerp(y0, y1, t[2]);
    let gx = lerp(lerp(c100 - c000, c110 - c010, t[1]), lerp(c101 - c001, c111 - c011, t[1]), t[2]) / step;
    let gy = lerp(x10 - x00, x11 - x01, t[2]) / step;
    let gz = (y1 - y0) / step;
    let mut g = [gx, gy, gz];
    for a in 0..3 {
        if d[a] != 0.0 {
            g[a] = 0.0;
        }
    }
    if e2 > 0.0 {
        let e = blaze_math::sqrt(e2);
        let inner = val - e;
        if e < inner {
            (inner, [g[0] - d[0] / e, g[1] - d[1] / e, g[2] - d[2] / e])
        } else {
            (e, [d[0] / e, d[1] / e, d[2] / e])
        }
    } else {
        (val, g)
    }
}

/// Runs the register program on lanes, telling `seen` each instruction's value (the shipped
/// callers pass a closure that does nothing, which inlining removes).
#[inline]
fn run<R: Real>(t: &Tape<'_>, regs: &mut [R], x: R, y: R, z: R, recip: bool, seen: &mut impl FnMut(usize, R)) -> R {
    for (j, ri) in t.prog.iter().enumerate() {
        let [a, b, c, d] = ri.args.map(|r| regs[r as usize]);
        let e = ri.extra as usize;
        let v = match ri.op {
            Op::X => x,
            Op::Y => y,
            Op::Z => z,
            Op::Const => R::splat(t.pool[e]),
            Op::Row => {
                let k = &t.pool[e..e + 4];
                ((R::splat(k[0]) * a + R::splat(k[1]) * b) + R::splat(k[2]) * c) + R::splat(k[3])
            }
            Op::BoxBound => {
                let k = &t.pool[e..e + 6];
                let bx = (a - R::splat(k[0])).abs() - R::splat(k[3]);
                let by = (b - R::splat(k[1])).abs() - R::splat(k[4]);
                let bz = (c - R::splat(k[2])).abs() - R::splat(k[5]);
                max_r(max_r(bx, by), bz)
            }
            Op::Path2 => {
                let (_, rows) = rows(t, ri.extra);
                R::map2(a, b, |u, v| path_field(rows, u, v).0)
            }
            Op::Grid3 => {
                let (tb, vals) = rows(t, ri.extra);
                R::map3(a, b, c, |u, v, w| grid_at(&tb.head, vals, [u, v, w]).0)
            }
            Op::PowConst => pure(Op::PowConst, a, b, c, d, t.pool[e], recip),
            op => pure(op, a, b, c, d, 0.0, recip),
        };
        regs[ri.out as usize] = v;
        seen(j, v);
    }
    regs[t.out as usize]
}

/// The field at one point.
pub fn eval_point(t: &Tape<'_>, p: Vec3, s: &mut Scratch<'_>) -> f64 {
    ensure(&mut s.one, t.regs, 0.0);
    run(t, &mut s.one, p.x, p.y, p.z, false, &mut |_, _| {})
}

/// [`eval_point`], telling `seen` the value of every instruction (the containment oracle
/// checks each one, not only the root, where later widening could hide an inward end).
#[cfg(test)]
pub(crate) fn eval_point_each(t: &Tape<'_>, p: Vec3, s: &mut Scratch<'_>, seen: &mut impl FnMut(usize, f64)) -> f64 {
    ensure(&mut s.one, t.regs, 0.0);
    run(t, &mut s.one, p.x, p.y, p.z, false, seen)
}

/// The field at every point of `pts` into `out` (see the module's contract).
pub fn eval_points(t: &Tape<'_>, pts: Points<'_>, out: &mut [f64], lanes: Lanes, s: &mut Scratch<'_>) -> Result<(), EvalError> {
    eval_points_with(t, pts, out, lanes, s, NoPlant)
}

/// [`eval_points`] through the plant seam.
pub fn eval_points_with(t: &Tape<'_>, pts: Points<'_>, out: &mut [f64], lanes: Lanes, s: &mut Scratch<'_>, plant: impl Planted<CadPlant>) -> Result<(), EvalError> {
    let n = out.len();
    if pts.x.len() != n || pts.y.len() != n || pts.z.len() != n {
        return Err(EvalError::Lengths { x: pts.x.len() as u64, y: pts.y.len() as u64, z: pts.z.len() as u64, out: n as u64 });
    }
    let recip = plant.has(CadPlant::VectorDivReciprocal);
    let mut i = 0;
    match lanes {
        Lanes::One => {}
        Lanes::Four => {
            if n >= 4 {
                ensure(&mut s.four, t.regs, Simd::splat(0.0));
                while i + 4 <= n {
                    let (x, y, z) = (Simd::from_slice(&pts.x[i..]), Simd::from_slice(&pts.y[i..]), Simd::from_slice(&pts.z[i..]));
                    run(t, &mut s.four, x, y, z, recip, &mut |_, _| {}).copy_to_slice(&mut out[i..i + 4]);
                    i += 4;
                }
            }
        }
        Lanes::Eight => {
            if n >= 8 {
                ensure(&mut s.eight, t.regs, Simd::splat(0.0));
                while i + 8 <= n {
                    let (x, y, z) = (Simd::from_slice(&pts.x[i..]), Simd::from_slice(&pts.y[i..]), Simd::from_slice(&pts.z[i..]));
                    run(t, &mut s.eight, x, y, z, recip, &mut |_, _| {}).copy_to_slice(&mut out[i..i + 8]);
                    i += 8;
                }
            }
        }
    }
    if i < n {
        ensure(&mut s.one, t.regs, 0.0);
        while i < n {
            out[i] = run(t, &mut s.one, pts.x[i], pts.y[i], pts.z[i], false, &mut |_, _| {});
            i += 1;
        }
    }
    Ok(())
}

/// Which operand a choice site takes over a whole box.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Choice {
    /// Either, by the point.
    Both,
    /// `a` of a min or max; `c` of a select.
    Left,
    /// `b` of a min or max; `d` of a select.
    Right,
}

/// One [`Choice`] per choice site of a tape, in instruction order, held in the scratch that
/// [`eval_box`] wrote them to (a box allocates nothing).
#[derive(Clone, Copy)]
pub struct Choices<'c> {
    of: &'c [Choice],
}

impl<'c> Choices<'c> {
    /// Choices given directly (for a caller that keeps them, and for tests).
    pub fn of(choices: &'c [Choice]) -> Choices<'c> {
        Choices { of: choices }
    }

    /// The choices.
    pub fn as_slice(&self) -> &'c [Choice] {
        self.of
    }

    /// How many sites were decided.
    pub fn decided(&self) -> usize {
        self.of.iter().filter(|c| **c != Choice::Both).count()
    }
}

const ULPS: f64 = 64.0 * f64::EPSILON;

/// The interval of the field over `b`, and the choices it decides.
pub fn eval_box<'s>(t: &Tape<'_>, b: &Aabb3, s: &'s mut Scratch<'_>) -> (Interval, Choices<'s>) {
    eval_box_with(t, b, s, NoPlant)
}

/// [`eval_box`] through the plant seam.
pub fn eval_box_with<'s>(t: &Tape<'_>, b: &Aabb3, s: &'s mut Scratch<'_>, plant: impl Planted<CadPlant>) -> (Interval, Choices<'s>) {
    eval_box_each(t, b, s, plant, &mut |_, _| {})
}

/// [`eval_box_with`], telling `seen` the interval of every instruction.
pub(crate) fn eval_box_each<'s>(
    t: &Tape<'_>,
    b: &Aabb3,
    s: &'s mut Scratch<'_>,
    plant: impl Planted<CadPlant>,
    seen: &mut impl FnMut(usize, Interval),
) -> (Interval, Choices<'s>) {
    let inward = plant.has(CadPlant::IntervalNotOutward);
    ensure(&mut s.boxes, t.regs, Interval::point(0.0));
    s.choices.clear();
    let ch = &mut s.choices;
    let (bx, by, bz) = (Interval::hull_of(b.lo.x, b.hi.x), Interval::hull_of(b.lo.y, b.hi.y), Interval::hull_of(b.lo.z, b.hi.z));
    let regs = &mut s.boxes;
    for (j, ri) in t.prog.iter().enumerate() {
        let [a, bb, c, d] = ri.args.map(|r| regs[r as usize]);
        let e = ri.extra as usize;
        let v = match ri.op {
            Op::X => bx,
            Op::Y => by,
            Op::Z => bz,
            Op::Const => Interval::point(t.pool[e]),
            Op::Neg => a.neg(),
            Op::Abs => a.abs(),
            Op::Sqrt => a.sqrt(),
            Op::Square => a.square(),
            Op::Sin => a.sin(),
            Op::Cos => a.cos(),
            Op::Round => a.round(),
            Op::Add => a.add(bb),
            Op::Sub => {
                if inward {
                    // The planted bug: each end rounded one step inward.
                    Interval::hull_of(next_up(a.lo() - bb.hi()), next_down(a.hi() - bb.lo()))
                } else {
                    a.sub(bb)
                }
            }
            Op::Mul => a.mul(bb),
            Op::Div => a.div(bb),
            Op::Min => {
                ch.push(if a.hi() <= bb.lo() {
                    Choice::Left
                } else if bb.hi() < a.lo() {
                    Choice::Right
                } else {
                    Choice::Both
                });
                a.min(bb)
            }
            Op::Max => {
                ch.push(if a.lo() >= bb.hi() {
                    Choice::Left
                } else if a.hi() < bb.lo() {
                    Choice::Right
                } else {
                    Choice::Both
                });
                a.max(bb)
            }
            Op::Select => {
                ch.push(if a.hi() < bb.lo() {
                    Choice::Left
                } else if a.lo() >= bb.hi() {
                    Choice::Right
                } else {
                    Choice::Both
                });
                Interval::select_lt(a, bb, c, d)
            }
            Op::Atan2 => a.atan2(bb),
            Op::PowConst => a.pow_const(t.pool[e]),
            Op::Row => {
                let k = &t.pool[e..e + 4];
                a.scale(k[0]).add(bb.scale(k[1])).add(c.scale(k[2])).shift(k[3])
            }
            Op::BoxBound => {
                let k = &t.pool[e..e + 6];
                let qx = a.shift(-k[0]).abs().shift(-k[3]);
                let qy = bb.shift(-k[1]).abs().shift(-k[4]);
                let qz = c.shift(-k[2]).abs().shift(-k[5]);
                qx.max(qy).max(qz)
            }
            Op::Path2 => {
                let (_, rows) = rows(t, ri.extra);
                path_box(rows, a, bb)
            }
            Op::Grid3 => {
                let (tb, vals) = rows(t, ri.extra);
                let sum = &t.grid_summaries[ri.extra as usize];
                grid_box(&tb.head, vals, sum, &t.grid_blocks, [a, bb, c])
            }
        };
        regs[ri.out as usize] = v;
        seen(j, v);
    }
    let out = regs[t.out as usize];
    (out, Choices { of: &s.choices })
}

fn finite(i: Interval) -> bool {
    i.lo().is_finite() && i.hi().is_finite()
}

/// A path's distance over a box: the centre's value, give or take the half-diagonal (the
/// field is 1-Lipschitz), widened by a rounding slack of 64 ulps of the magnitudes involved.
fn path_box(rows: &[f64], u: Interval, v: Interval) -> Interval {
    if !finite(u) || !finite(v) {
        return Interval::ENTIRE;
    }
    let (cu, cv) = (u.mid(), v.mid());
    let f = path_field(rows, cu, cv).0;
    let r = blaze_math::hypot(u.radius(), v.radius());
    let slack = ULPS * (1.0 + f.abs() + r + cu.abs() + cv.abs());
    Interval::hull_of(next_down(f - r - slack), next_up(f + r + slack))
}

/// A grid's field over a box: the least and most grid values the box's clamp touches (the
/// trilinear value is a convex combination of them), and outside the grid's box the range of
/// `max(v - e, e)` over the box's range of distances `e` to it, widened by a rounding slack.
/// Uses the tape's deterministic 2-level coarse min-max block summary (`4^3` / `16^3`) for
/// multi-block queries so coarse octree boxes avoid scanning `O(N^3)` voxels.
fn grid_box(h: &[f64; 8], vals: &[f64], sum: &GridSummary, blocks: &[[f64; 2]], p: [Interval; 3]) -> Interval {
    if !p.iter().all(|i| finite(*i)) {
        return Interval::ENTIRE;
    }
    let n = [h[5] as usize, h[6] as usize, h[7] as usize];
    let step = h[3];
    let mut lo_i = [0usize; 3];
    let mut hi_i = [0usize; 3];
    let (mut e_lo2, mut e_hi2) = (0.0, 0.0);
    for a in 0..3 {
        let top = h[a] + (n[a] - 1) as f64 * step;
        let (plo, phi) = (p[a].lo(), p[a].hi());
        let near = if phi < h[a] { h[a] - phi } else if plo > top { plo - top } else { 0.0 };
        let far = (h[a] - plo).max(phi - top).max(0.0);
        e_lo2 += near * near;
        e_hi2 += far * far;
        let (qlo, qhi) = (plo.clamp(h[a], top), phi.clamp(h[a], top));
        let last = (n[a] - 1) as f64;
        lo_i[a] = blaze_math::floor((qlo - h[a]) / step).clamp(0.0, last) as usize;
        hi_i[a] = blaze_math::ceil((qhi - h[a]) / step).clamp(0.0, last) as usize;
    }
    let vox_count = (hi_i[0] - lo_i[0] + 1)
        .saturating_mul(hi_i[1] - lo_i[1] + 1)
        .saturating_mul(hi_i[2] - lo_i[2] + 1);
    let (mut vmin, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY);
    if vox_count <= 64 {
        for k in lo_i[2]..=hi_i[2] {
            for j in lo_i[1]..=hi_i[1] {
                let row = n[0] * (j + n[1] * k);
                for i in lo_i[0]..=hi_i[0] {
                    let v = vals[row + i];
                    vmin = vmin.min(v);
                    vmax = vmax.max(v);
                }
            }
        }
    } else {
        let b1_lo = [lo_i[0] / GRID_BLOCK, lo_i[1] / GRID_BLOCK, lo_i[2] / GRID_BLOCK];
        let b1_hi = [hi_i[0] / GRID_BLOCK, hi_i[1] / GRID_BLOCK, hi_i[2] / GRID_BLOCK];
        let b1_count = (b1_hi[0] - b1_lo[0] + 1)
            .saturating_mul(b1_hi[1] - b1_lo[1] + 1)
            .saturating_mul(b1_hi[2] - b1_lo[2] + 1);
        if b1_count <= 256 {
            let bd = [sum.b1_dims[0] as usize, sum.b1_dims[1] as usize];
            let base = sum.b1_start as usize;
            for bk in b1_lo[2]..=b1_hi[2] {
                for bj in b1_lo[1]..=b1_hi[1] {
                    let row = base + bd[0] * (bj + bd[1] * bk);
                    for bi in b1_lo[0]..=b1_hi[0] {
                        let pair = blocks[row + bi];
                        vmin = vmin.min(pair[0]);
                        vmax = vmax.max(pair[1]);
                    }
                }
            }
        } else {
            let b2_lo = [lo_i[0] / GRID_SUPER, lo_i[1] / GRID_SUPER, lo_i[2] / GRID_SUPER];
            let b2_hi = [hi_i[0] / GRID_SUPER, hi_i[1] / GRID_SUPER, hi_i[2] / GRID_SUPER];
            let sd = [sum.b2_dims[0] as usize, sum.b2_dims[1] as usize];
            let base = sum.b2_start as usize;
            for sk in b2_lo[2]..=b2_hi[2] {
                for sj in b2_lo[1]..=b2_hi[1] {
                    let row = base + sd[0] * (sj + sd[1] * sk);
                    for si in b2_lo[0]..=b2_hi[0] {
                        let pair = blocks[row + si];
                        vmin = vmin.min(pair[0]);
                        vmax = vmax.max(pair[1]);
                    }
                }
            }
        }
    }
    let (e_lo, e_hi) = (blaze_math::sqrt(e_lo2), blaze_math::sqrt(e_hi2));
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    if e_lo == 0.0 {
        lo = vmin;
        hi = vmax;
    }
    if e_hi > 0.0 {
        lo = lo.min((vmin - e_hi).max(e_lo));
        hi = hi.max((vmax - e_lo).max(e_hi));
    }
    let slack = ULPS * (1.0 + vmin.abs().max(vmax.abs()) + e_hi + step);
    Interval::hull_of(next_down(lo - slack), next_up(hi + slack))
}

/// The tape with each decided site replaced by the operand it takes over the box the choices
/// came from (see the module's contract), in `alloc`.
pub fn simplify<'a>(t: &Tape<'_>, choices: Choices<'_>, alloc: Alloc<'a>) -> Result<Tape<'a>, CompileError> {
    simplify_with(t, choices, alloc, NoPlant)
}

/// [`simplify`] through the plant seam.
pub fn simplify_with<'a>(t: &Tape<'_>, choices: Choices<'_>, alloc: Alloc<'a>, plant: impl Planted<CadPlant>) -> Result<Tape<'a>, CompileError> {
    if choices.of.len() as u64 != u64::from(t.sites) {
        return Err(CompileError::WrongChoices { sites: u64::from(t.sites), given: choices.of.len() as u64 });
    }
    let mut site: AVec<'a, Choice> = Vec::with_capacity_in(t.instrs.len(), alloc);
    let mut k = 0;
    for i in t.instrs.iter() {
        if i.op.choice() {
            site.push(choices.of[k]);
            k += 1;
        } else {
            site.push(Choice::Both);
        }
    }
    let wrong = plant.has(CadPlant::SimplifyWrongSide);
    let keep = plant.has(CadPlant::SimplifyKeepsBothSides);
    let rewrite = |at: u32, i: &Instr| -> Rw {
        if keep {
            return Rw::Keep;
        }
        let [a, b, c, d] = i.args;
        match (i.op, site[at as usize]) {
            (_, Choice::Both) => Rw::Keep,
            (Op::Min, Choice::Left) => Rw::Alias(if wrong { b } else { a }),
            (Op::Min | Op::Max, Choice::Left) => Rw::Alias(a),
            (Op::Min | Op::Max, Choice::Right) => Rw::Alias(b),
            (Op::Select, Choice::Left) => Rw::Alias(c),
            (Op::Select, Choice::Right) => Rw::Alias(d),
            _ => Rw::Keep,
        }
    };
    tape::rebuild(Source::of(t), t.root, rewrite, t.level, t.aabb, t.limits, alloc, plant)
}

/// Per-lane table gradient lookup helper so `run_grad<R>` runs identically for scalar and SIMD.
trait GradLane: Real {
    fn map2_grad(a: Self, b: Self, f: impl Fn(f64, f64) -> (f64, f64, f64)) -> (Self, Self, Self);
    fn map3_grad(a: Self, b: Self, c: Self, f: impl Fn(f64, f64, f64) -> (f64, [f64; 3])) -> (Self, [Self; 3]);
}

impl GradLane for f64 {
    #[inline]
    fn map2_grad(a: f64, b: f64, f: impl Fn(f64, f64) -> (f64, f64, f64)) -> (f64, f64, f64) {
        f(a, b)
    }
    #[inline]
    fn map3_grad(a: f64, b: f64, c: f64, f: impl Fn(f64, f64, f64) -> (f64, [f64; 3])) -> (f64, [f64; 3]) {
        f(a, b, c)
    }
}

impl<const N: usize> GradLane for Simd<f64, N>
where
    Simd<f64, N>: Real,
{
    #[inline]
    fn map2_grad(a: Self, b: Self, f: impl Fn(f64, f64) -> (f64, f64, f64)) -> (Self, Self, Self) {
        let (aa, bb) = (a.to_array(), b.to_array());
        let (mut v, mut gx, mut gy) = ([0.0; N], [0.0; N], [0.0; N]);
        for i in 0..N {
            let (vi, xi, yi) = f(aa[i], bb[i]);
            v[i] = vi;
            gx[i] = xi;
            gy[i] = yi;
        }
        (Simd::from_array(v), Simd::from_array(gx), Simd::from_array(gy))
    }

    #[inline]
    fn map3_grad(a: Self, b: Self, c: Self, f: impl Fn(f64, f64, f64) -> (f64, [f64; 3])) -> (Self, [Self; 3]) {
        let (aa, bb, cc) = (a.to_array(), b.to_array(), c.to_array());
        let (mut v, mut gx, mut gy, mut gz) = ([0.0; N], [0.0; N], [0.0; N], [0.0; N]);
        for i in 0..N {
            let (vi, gi) = f(aa[i], bb[i], cc[i]);
            v[i] = vi;
            gx[i] = gi[0];
            gy[i] = gi[1];
            gz[i] = gi[2];
        }
        (Simd::from_array(v), [Simd::from_array(gx), Simd::from_array(gy), Simd::from_array(gz)])
    }
}

#[inline]
fn lin_r<R: Real>(ka: R, a: [R; 4], kb: R, b: [R; 4]) -> [R; 4] {
    [R::splat(0.0), ka * a[1] + kb * b[1], ka * a[2] + kb * b[2], ka * a[3] + kb * b[3]]
}

#[inline]
fn select4<R: Real>(m: R::Mask, a: [R; 4], b: [R; 4]) -> [R; 4] {
    [R::select(m, a[0], b[0]), R::select(m, a[1], b[1]), R::select(m, a[2], b[2]), R::select(m, a[3], b[3])]
}

/// Forward-mode AD over `R` (`f64`, `Simd<f64, 4>`, or `Simd<f64, 8>`): bit-equal across all
/// lane widths because every lane executes the exact same IEEE operations in the same order.
#[inline]
fn run_grad<R: GradLane>(t: &Tape<'_>, regs: &mut [[R; 4]], x: R, y: R, z: R, unsigned_abs: bool, recip: bool) -> [R; 4] {
    let zero = R::splat(0.0);
    let one = R::splat(1.0);
    let z4 = [zero; 4];
    for ri in t.prog.iter() {
        let [a, b, c, d] = ri.args.map(|r| regs[r as usize]);
        let e = ri.extra as usize;
        let mut g = match ri.op {
            Op::X => [x, one, zero, zero],
            Op::Y => [y, zero, one, zero],
            Op::Z => [z, zero, zero, one],
            Op::Const => [R::splat(t.pool[e]), zero, zero, zero],
            Op::Neg => lin_r(-one, a, zero, z4),
            Op::Abs => {
                let sgn = if unsigned_abs { one } else { R::select(a[0].lt(zero), -one, one) };
                lin_r(sgn, a, zero, z4)
            }
            Op::Sqrt => {
                let r = a[0].sqrt();
                let k = R::select(zero.lt(r), R::splat(0.5) / r, zero);
                lin_r(k, a, zero, z4)
            }
            Op::Square => lin_r(R::splat(2.0) * a[0], a, zero, z4),
            Op::Sin => lin_r(blaze_math::cos(a[0]), a, zero, z4),
            Op::Cos => lin_r(-blaze_math::sin(a[0]), a, zero, z4),
            Op::Round => z4,
            Op::Add => lin_r(one, a, one, b),
            Op::Sub => lin_r(one, a, -one, b),
            Op::Mul => lin_r(b[0], a, a[0], b),
            Op::Div => lin_r(one / b[0], a, -a[0] / (b[0] * b[0]), b),
            Op::Min => select4(b[0].lt(a[0]), b, a),
            Op::Max => select4(a[0].lt(b[0]), b, a),
            Op::Select => select4(a[0].lt(b[0]), c, d),
            Op::Atan2 => {
                let q = a[0] * a[0] + b[0] * b[0];
                let m = zero.lt(q);
                select4(m, lin_r(b[0] / q, a, -a[0] / q, b), z4)
            }
            Op::PowConst => {
                let k = t.pool[e];
                let pos = zero.lt(a[0]);
                let safe_a = R::select(pos, a[0], one);
                let dp = R::select(pos, R::splat(k) * blaze_math::pow(safe_a, R::splat(k - 1.0)), zero);
                lin_r(dp, a, zero, z4)
            }
            Op::Row => {
                let k = &t.pool[e..e + 4];
                let ab = lin_r(R::splat(k[0]), a, R::splat(k[1]), b);
                lin_r(one, ab, R::splat(k[2]), c)
            }
            Op::BoxBound => {
                let k = &t.pool[e..e + 6];
                let mut best = [R::splat(f64::NEG_INFINITY), zero, zero, zero];
                for (q, (ci, hi)) in [a, b, c].iter().zip([(k[0], k[3]), (k[1], k[4]), (k[2], k[5])]) {
                    let diff = q[0] - R::splat(ci);
                    let v = diff.abs() - R::splat(hi);
                    let take = best[0].lt(v);
                    let sgn = R::select(diff.lt(zero), -one, one);
                    let mut cand = lin_r(sgn, *q, zero, z4);
                    cand[0] = v;
                    best = select4(take, cand, best);
                }
                best
            }
            Op::Path2 => {
                let (_, rows) = rows(t, ri.extra);
                let (val, gx, gy) = R::map2_grad(a[0], b[0], |u, v| path_field(rows, u, v));
                let mut out = lin_r(gx, a, gy, b);
                out[0] = val;
                out
            }
            Op::Grid3 => {
                let (tb, vals) = rows(t, ri.extra);
                let (val, gg) = R::map3_grad(a[0], b[0], c[0], |u, v, w| grid_at(&tb.head, vals, [u, v, w]));
                let ab = lin_r(gg[0], a, gg[1], b);
                let mut out = lin_r(one, ab, gg[2], c);
                out[0] = val;
                out
            }
        };
        // The value is the point form's, bit for bit.
        g[0] = match ri.op {
            Op::X | Op::Y | Op::Z | Op::Const | Op::Min | Op::Max | Op::Select | Op::BoxBound | Op::Path2 | Op::Grid3 => g[0],
            Op::Row => {
                let k = &t.pool[e..e + 4];
                ((R::splat(k[0]) * a[0] + R::splat(k[1]) * b[0]) + R::splat(k[2]) * c[0]) + R::splat(k[3])
            }
            op => pure(op, a[0], b[0], c[0], d[0], if op == Op::PowConst { t.pool[e] } else { 0.0 }, recip),
        };
        regs[ri.out as usize] = g;
    }
    regs[t.out as usize]
}

/// The field and its gradient at `p`.
pub fn eval_grad(t: &Tape<'_>, p: Vec3, s: &mut Scratch<'_>) -> (f64, Vec3) {
    eval_grad_with(t, p, s, NoPlant)
}

/// [`eval_grad`] through the plant seam.
pub fn eval_grad_with(t: &Tape<'_>, p: Vec3, s: &mut Scratch<'_>, plant: impl Planted<CadPlant>) -> (f64, Vec3) {
    let unsigned_abs = plant.has(CadPlant::GradAbsSignIgnored);
    ensure(&mut s.grads, t.regs, [0.0; 4]);
    let r = run_grad(t, &mut s.grads, p.x, p.y, p.z, unsigned_abs, false);
    (r[0], Vec3::new(r[1], r[2], r[3]))
}

/// The field values and forward-mode gradients at every point of `pts`, writing into `vals` and
/// `grads`, bit-equal to scalar [`eval_grad`] at every [`Lanes`] width.
pub fn eval_grads(
    t: &Tape<'_>,
    pts: Points<'_>,
    vals: &mut [f64],
    grads: &mut [Vec3],
    lanes: Lanes,
    s: &mut Scratch<'_>,
) -> Result<(), EvalError> {
    eval_grads_with(t, pts, vals, grads, lanes, s, NoPlant)
}

/// [`eval_grads`] through the plant seam.
pub fn eval_grads_with(
    t: &Tape<'_>,
    pts: Points<'_>,
    vals: &mut [f64],
    grads: &mut [Vec3],
    lanes: Lanes,
    s: &mut Scratch<'_>,
    plant: impl Planted<CadPlant>,
) -> Result<(), EvalError> {
    let n = vals.len();
    if pts.x.len() != n || pts.y.len() != n || pts.z.len() != n || grads.len() != n {
        return Err(EvalError::GradLengths {
            x: pts.x.len() as u64,
            y: pts.y.len() as u64,
            z: pts.z.len() as u64,
            vals: n as u64,
            grads: grads.len() as u64,
        });
    }
    let unsigned_abs = plant.has(CadPlant::GradAbsSignIgnored);
    let recip = plant.has(CadPlant::VectorDivReciprocal);
    let mut i = 0;
    match lanes {
        Lanes::One => {}
        Lanes::Four => {
            if n >= 4 {
                ensure(&mut s.grads_four, t.regs, [Simd::splat(0.0); 4]);
                while i + 4 <= n {
                    let (x, y, z) = (Simd::from_slice(&pts.x[i..]), Simd::from_slice(&pts.y[i..]), Simd::from_slice(&pts.z[i..]));
                    let r = run_grad(t, &mut s.grads_four, x, y, z, unsigned_abs, recip);
                    r[0].copy_to_slice(&mut vals[i..i + 4]);
                    let (gx, gy, gz) = (r[1].to_array(), r[2].to_array(), r[3].to_array());
                    for lane in 0..4 {
                        grads[i + lane] = Vec3::new(gx[lane], gy[lane], gz[lane]);
                    }
                    i += 4;
                }
            }
        }
        Lanes::Eight => {
            if n >= 8 {
                ensure(&mut s.grads_eight, t.regs, [Simd::splat(0.0); 4]);
                while i + 8 <= n {
                    let (x, y, z) = (Simd::from_slice(&pts.x[i..]), Simd::from_slice(&pts.y[i..]), Simd::from_slice(&pts.z[i..]));
                    let r = run_grad(t, &mut s.grads_eight, x, y, z, unsigned_abs, recip);
                    r[0].copy_to_slice(&mut vals[i..i + 8]);
                    let (gx, gy, gz) = (r[1].to_array(), r[2].to_array(), r[3].to_array());
                    for lane in 0..8 {
                        grads[i + lane] = Vec3::new(gx[lane], gy[lane], gz[lane]);
                    }
                    i += 8;
                }
            }
        }
    }
    if i < n {
        ensure(&mut s.grads, t.regs, [0.0; 4]);
        while i < n {
            let r = run_grad(t, &mut s.grads, pts.x[i], pts.y[i], pts.z[i], unsigned_abs, false);
            vals[i] = r[0];
            grads[i] = Vec3::new(r[1], r[2], r[3]);
            i += 1;
        }
    }
    Ok(())
}
