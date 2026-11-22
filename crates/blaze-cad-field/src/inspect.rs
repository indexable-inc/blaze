//! INSPECTION: questions a design asks of a solid, each answered with a proof or a typed
//! "could not within the budget", never a guess.
//!
//! - [`bounds`]: the box the solid lies in, to within one cell of a bisection.
//! - [`classify`]: inside, outside, or the surface passes within a given distance.
//! - [`distance_between`]: the gap between two solids as an interval, by branch and bound.
//! - [`section`]: the field on a plane, as a tape over the plane's coordinates.
//! - [`ray`]: the first crossing along a ray, by sphere tracing with a stated stopping rule.

use blaze_math::{Aabb3, Interval, Vec3, next_down, next_up};
use blaze_own::{AVec, Alloc, Vec};

use crate::error::{CompileError, InspectError};
use crate::eval::{Scratch, eval_box, eval_grad, eval_point};
use crate::level::Level;
use crate::placement::Placement;
use crate::tape::{self, Op, Rw, Source, Tape};

fn finite(b: &Aabb3) -> bool {
    let (l, h) = (b.lo.to_array(), b.hi.to_array());
    l.iter().chain(h.iter()).all(|v| v.is_finite())
}

/// The eight halves-of-halves of `b`.
fn octants(b: &Aabb3) -> [Aabb3; 8] {
    let (l, c, h) = (b.lo, b.center(), b.hi);
    let mut out = [Aabb3::EMPTY; 8];
    for (i, o) in out.iter_mut().enumerate() {
        let pick = |bit: usize, lo: f64, mid: f64, hi: f64| if i & bit == 0 { (lo, mid) } else { (mid, hi) };
        let (x0, x1) = pick(1, l.x, c.x, h.x);
        let (y0, y1) = pick(2, l.y, c.y, h.y);
        let (z0, z1) = pick(4, l.z, c.z, h.z);
        *o = Aabb3::new(Vec3::new(x0, y0, z0), Vec3::new(x1, y1, z1));
    }
    out
}

/// What [`bounds`] found.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bounds {
    /// A box holding every point of the solid ([`Aabb3::EMPTY`] when it has none).
    pub aabb: Aabb3,
    /// The cell size: each face is at most this far outside the tightest box.
    pub cell: Vec3,
}

/// What [`bounds`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct BoundsOpts {
    /// Bisections of the tape's box: the answer is within `box / 2^depth` of the tightest.
    pub depth: u32 = 7,
    /// Boxes it may evaluate.
    pub most_boxes: u32 = 1 << 20,
}

/// The box the solid lies in, by breadth-first bisection of the tape's box: a cell is kept
/// when its interval admits a point at or below zero; a cell wholly inside is kept whole.
pub fn bounds(t: &Tape<'_>, o: BoundsOpts, s: &mut Scratch<'_>) -> Result<Bounds, InspectError> {
    let root = t.aabb();
    if !finite(&root) {
        return Err(InspectError::Unbounded { what: "the tape's box" });
    }
    let alloc = s.alloc();
    let mut layer: AVec<'_, Aabb3> = Vec::new_in(alloc);
    let mut next: AVec<'_, Aabb3> = Vec::new_in(alloc);
    layer.push(root);
    let mut found = Aabb3::EMPTY;
    let mut spent: u64 = 0;
    for _ in 0..o.depth {
        next.clear();
        for b in layer.iter() {
            for c in octants(b) {
                spent += 1;
                if spent > u64::from(o.most_boxes) {
                    return Err(InspectError::Budget { what: "boxes for bounds", most: u64::from(o.most_boxes) });
                }
                let (iv, _) = eval_box(t, &c, s);
                if iv.hi() < 0.0 {
                    found = found.union(&c);
                } else if iv.lo() <= 0.0 {
                    next.push(c);
                }
            }
        }
        core::mem::swap(&mut layer, &mut next);
    }
    for b in layer.iter() {
        found = found.union(b);
    }
    let f = f64::from(1u32 << o.depth.min(31));
    let size = root.hi - root.lo;
    Ok(Bounds { aabb: found, cell: Vec3::new(size.x / f, size.y / f, size.z / f) })
}

/// Where a point is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Class {
    /// Inside, farther than `within` from the surface.
    Inside,
    /// Outside, farther than `within` from the surface.
    Outside,
    /// The surface passes within this distance: two points of opposite sign were found in
    /// the ball, or the field is zero here, or (an exact field) its magnitude is this small.
    OnSurface { within: f64 },
}

/// Where `p` is, to `within` (see [`Class`]). For an exact field one evaluation decides. For a
/// weaker field `|v| > within` decides (the field is at most the distance); otherwise the
/// cube around the ball is bisected, cells that cannot hold the other sign or miss the ball are
/// dropped, and a cell centre of the other sign in the ball proves the surface; no cell left
/// proves its absence.
pub fn classify(t: &Tape<'_>, p: Vec3, within: f64, most_boxes: u32, s: &mut Scratch<'_>) -> Result<Class, InspectError> {
    if !(within > 0.0 && within.is_finite()) {
        return Err(InspectError::Argument { arg: "within", value: within, accepted: "a finite distance above zero" });
    }
    let v = eval_point(t, p, s);
    let side = if v < 0.0 { Class::Inside } else { Class::Outside };
    if v == 0.0 {
        return Ok(Class::OnSurface { within });
    }
    if v.abs() > within {
        return Ok(side);
    }
    if t.level() == Level::Exact {
        return Ok(Class::OnSurface { within });
    }
    let alloc = s.alloc();
    let mut work: AVec<'_, Aabb3> = Vec::new_in(alloc);
    work.push(Aabb3::around(p, Vec3::splat(within)));
    let mut spent: u64 = 0;
    while let Some(b) = work.pop() {
        spent += 1;
        if spent > u64::from(most_boxes) {
            return Err(InspectError::Budget { what: "boxes for classify", most: u64::from(most_boxes) });
        }
        // The nearest point of the cell to `p`: a cell outside the ball is dropped.
        let q = p.max(b.lo).min(b.hi);
        if (q - p).length() > within {
            continue;
        }
        let (iv, _) = eval_box(t, &b, s);
        let other = if v < 0.0 { iv.hi() >= 0.0 } else { iv.lo() <= 0.0 };
        if !other {
            continue;
        }
        let c = b.center();
        if (c - p).length() <= within {
            let w = eval_point(t, c, s);
            if (v < 0.0 && w >= 0.0) || (v > 0.0 && w <= 0.0) {
                return Ok(Class::OnSurface { within });
            }
        }
        for o in octants(&b) {
            work.push(o);
        }
    }
    Ok(side)
}

/// The gap between two solids.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Gap {
    /// Holds the distance, and is at most the tolerance wide (zero when they touch).
    Settled(Interval),
    /// The budget ran out: the distance lies in `so_far`, after `boxes` boxes.
    BudgetSpent { so_far: Interval, boxes: u64 },
    /// The first solid has no points.
    Empty,
}

/// What [`distance_between`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DistanceOpts {
    /// How wide the answer may be, metres.
    pub tolerance: f64,
    /// Boxes it may evaluate.
    pub most_boxes: u32 = 1 << 20,
}

/// What [`distance_between_stopping`] takes: [`DistanceOpts`] plus optional early-exit
/// distance thresholds (metres).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DistanceStopOpts {
    /// How wide the answer may be, metres.
    pub tolerance: f64,
    /// Boxes it may evaluate.
    pub most_boxes: u32 = 1 << 20,
    /// Stop early as soon as the upper bound is strictly below this distance (metres).
    pub stop_below: Option<f64> = None,
    /// Stop early as soon as the lower bound is at or above this distance (metres).
    pub stop_above: Option<f64> = None,
}

/// A box in the search, keyed by the least gap it can hold.
#[derive(Clone, Copy)]
struct Cand {
    lb: f64,
    b: Aabb3,
}

fn heap_push(h: &mut AVec<'_, Cand>, c: Cand) {
    h.push(c);
    let mut i = h.len() - 1;
    while i > 0 {
        let p = (i - 1) / 2;
        if h[p].lb <= h[i].lb {
            break;
        }
        h.swap(p, i);
        i = p;
    }
}

fn heap_pop(h: &mut AVec<'_, Cand>) -> Option<Cand> {
    let n = h.len();
    if n == 0 {
        return None;
    }
    h.swap(0, n - 1);
    let top = h.pop();
    let n = h.len();
    let mut i = 0;
    loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < n && h[l].lb < h[m].lb {
            m = l;
        }
        if r < n && h[r].lb < h[m].lb {
            m = r;
        }
        if m == i {
            break;
        }
        h.swap(i, m);
        i = m;
    }
    top
}

/// The distance between the solids of `a` and `b` (zero when they meet), best first over the
/// boxes of `a`'s box that can hold points of `a`, each keyed by `max(0, lo of b's field,
/// AABB gap to b's box)` there: the least key is a LOWER bound (a field is at most the
/// distance outside its solid, and every point of `b` lies in `b`'s box). An UPPER bound
/// needs a field that IS the distance outside its solid: `b`'s when it is at least `Outside`,
/// evaluated at centres found inside `a`; with weaker fields the answer stays open above and
/// the budget ends it.
pub fn distance_between(a: &Tape<'_>, b: &Tape<'_>, o: DistanceOpts, s: &mut Scratch<'_>) -> Result<Gap, InspectError> {
    distance_between_stopping(a, b, DistanceStopOpts { tolerance: o.tolerance, most_boxes: o.most_boxes, .. }, s)
}

/// [`distance_between`] with optional early-exit bounds (`stop_below` / `stop_above`): returns
/// `Gap::Settled([lb, ub])` immediately once `lb >= stop_above` or `ub < stop_below`, in
/// addition to the standard `ub - lb <= tolerance` stopping rule.
pub fn distance_between_stopping(a: &Tape<'_>, b: &Tape<'_>, o: DistanceStopOpts, s: &mut Scratch<'_>) -> Result<Gap, InspectError> {
    if !(o.tolerance >= 0.0) {
        return Err(InspectError::Argument { arg: "tolerance", value: o.tolerance, accepted: "at least zero" });
    }
    if let Some(lim) = o.stop_below
        && !(lim >= 0.0)
    {
        return Err(InspectError::Argument { arg: "stop_below", value: lim, accepted: "at least zero" });
    }
    if let Some(lim) = o.stop_above
        && !(lim >= 0.0)
    {
        return Err(InspectError::Argument { arg: "stop_above", value: lim, accepted: "at least zero" });
    }
    let root = a.aabb();
    if root.is_empty() {
        return Ok(Gap::Empty);
    }
    if !finite(&root) {
        return Err(InspectError::Unbounded { what: "the first solid's box" });
    }
    let b_box = b.aabb();
    let box_lb = |q: &Aabb3| if b_box.is_empty() { 0.0 } else { q.gap(&b_box) };
    let root_lb = box_lb(&root);
    let measures = b.level().at_least(Level::Outside);
    let alloc = s.alloc();
    let mut heap: AVec<'_, Cand> = Vec::new_in(alloc);
    heap_push(&mut heap, Cand { lb: root_lb, b: root });
    let mut ub = f64::INFINITY;
    let mut any = false;
    let mut spent: u64 = 0;
    while let Some(c) = heap_pop(&mut heap) {
        let lb = c.lb;
        if let Some(lim) = o.stop_above
            && lb >= lim
        {
            return Ok(Gap::Settled(Interval::hull_of(lb, ub)));
        }
        if let Some(lim) = o.stop_below
            && ub < lim
        {
            return Ok(Gap::Settled(Interval::hull_of(lb, ub)));
        }
        if ub - lb <= o.tolerance {
            return Ok(Gap::Settled(Interval::hull_of(lb, ub)));
        }
        if lb >= ub {
            break;
        }
        spent += 1;
        if spent > u64::from(o.most_boxes) {
            return Ok(Gap::BudgetSpent { so_far: Interval::hull_of(lb, ub), boxes: spent });
        }
        let centre = c.b.center();
        if eval_point(a, centre, s) <= 0.0 {
            any = true;
            if measures {
                ub = ub.min(eval_point(b, centre, s).max(0.0));
                if let Some(lim) = o.stop_below
                    && ub < lim
                {
                    return Ok(Gap::Settled(Interval::hull_of(lb, ub)));
                }
                if ub - lb <= o.tolerance {
                    return Ok(Gap::Settled(Interval::hull_of(lb, ub)));
                }
            }
        }
        for q in octants(&c.b) {
            let (fa, _) = eval_box(a, &q, s);
            if fa.lo() > 0.0 {
                continue;
            }
            let (fb, _) = eval_box(b, &q, s);
            let qlb = fb.lo().max(box_lb(&q)).max(0.0);
            if !ub.is_finite() || qlb < ub {
                heap_push(&mut heap, Cand { lb: qlb, b: q });
            }
        }
    }
    if !any && ub == f64::INFINITY {
        return Ok(Gap::Empty);
    }
    Ok(Gap::Settled(Interval::hull_of(ub, ub)))
}

/// The field on the plane `frame` maps the `uv` plane to: a tape over `(u, v, w)` (world point
/// `frame(u, v, w)`), its level `Bound` (a solid's distance along a plane is at least its
/// distance in space), its box the world box carried back. Evaluate at `w = 0` for the section.
pub fn section<'a>(t: &Tape<'_>, frame: &Placement, alloc: Alloc<'a>) -> Result<Tape<'a>, CompileError> {
    let m = frame.linear();
    let sh = frame.shift_m().to_array();
    let rows = [
        [m.rows[0][0], m.rows[0][1], m.rows[0][2], sh[0]],
        [m.rows[1][0], m.rows[1][1], m.rows[1][2], sh[1]],
        [m.rows[2][0], m.rows[2][1], m.rows[2][2], sh[2]],
    ];
    let aabb = if finite(&t.aabb()) { frame.inverse().apply_box(&t.aabb()) } else { Aabb3::ALL };
    let rewrite = |_: u32, i: &crate::tape::Instr| match i.op {
        Op::X => Rw::Row(rows[0]),
        Op::Y => Rw::Row(rows[1]),
        Op::Z => Rw::Row(rows[2]),
        _ => Rw::Keep,
    };
    tape::rebuild(Source::of(t), t.root(), rewrite, Level::Bound, aabb, *t.limits(), alloc, blaze_own::planted::NoPlant)
}

/// What [`ray`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct RayOpts {
    pub origin: Vec3,
    /// Need not be unit length; must not be zero.
    pub dir: Vec3,
    /// The farthest `t` (metres along the unit direction) looked at.
    pub max_t: f64,
    /// The least step: a feature thinner than this along the ray may be stepped over.
    pub min_step: f64 = 1e-7,
    /// Steps it may take.
    pub most_steps: u32 = 1 << 16,
}

/// A ray's first crossing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Hit {
    /// Metres along the unit direction; the field is at or below zero here and above zero
    /// at `t` minus the bisection's last width.
    pub t: f64,
    /// The unit gradient there, when it is not zero.
    pub normal: Option<Vec3>,
}

/// Clips the ray interval `[0, max_t]` against `b` (which may have infinite faces), widening
/// each finite slab entry/exit outward by two ulps so grazing surface hits are never missed.
fn clip_ray_aabb(origin: Vec3, dir: Vec3, max_t: f64, b: &Aabb3) -> Option<(f64, f64)> {
    if b.is_empty() {
        return None;
    }
    let o = origin.to_array();
    if !o.iter().all(|v| v.is_finite()) {
        return Some((0.0, max_t));
    }
    let d = dir.to_array();
    let lo = b.lo.to_array();
    let hi = b.hi.to_array();
    let (mut t_lo, mut t_hi) = (0.0f64, max_t);
    for i in 0..3 {
        if d[i] == 0.0 {
            if o[i] < lo[i] || o[i] > hi[i] {
                return None;
            }
            continue;
        }
        let t0 = if lo[i].is_finite() {
            (lo[i] - o[i]) / d[i]
        } else if d[i] > 0.0 {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        let t1 = if hi[i].is_finite() {
            (hi[i] - o[i]) / d[i]
        } else if d[i] > 0.0 {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        };
        let (mut s_lo, mut s_hi) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        if s_lo.is_finite() {
            s_lo = next_down(next_down(s_lo));
        }
        if s_hi.is_finite() {
            s_hi = next_up(next_up(s_hi));
        }
        t_lo = t_lo.max(s_lo);
        t_hi = t_hi.min(s_hi);
        if t_lo > t_hi {
            return None;
        }
    }
    Some((t_lo, t_hi))
}

/// The ray's first crossing, by sphere tracing. Clips `[0, max_t]` against `t.aabb()` first so
/// rays missing the bounding box return immediately and rays starting far away jump straight to
/// entry. STOPPING RULE: from `t`, step by the field's value or `min_step`, whichever is more
/// (the field is at most the distance, so a step of its value crosses nothing); a step that
/// lands at or below zero brackets a crossing, which is bisected to the last representable
/// width. A start at or below zero is a hit at `t = 0`. MISS: no point at or below zero was met
/// before `t` passed `max_t`. A crossing and recrossing both within one `min_step`, or a tangent
/// graze that never goes below zero, is not seen.
pub fn ray(t: &Tape<'_>, o: RayOpts, s: &mut Scratch<'_>) -> Result<Option<Hit>, InspectError> {
    let Some(d) = o.dir.normalized() else {
        return Err(InspectError::ZeroDirection);
    };
    if !(o.min_step > 0.0 && o.min_step.is_finite()) {
        return Err(InspectError::Argument { arg: "min_step", value: o.min_step, accepted: "a finite step above zero" });
    }
    if !(o.max_t >= 0.0) {
        return Err(InspectError::Argument { arg: "max_t", value: o.max_t, accepted: "at least zero" });
    }
    let Some((t_enter, t_exit)) = clip_ray_aabb(o.origin, d, o.max_t, &t.aabb()) else {
        return Ok(None);
    };
    let at = |k: f64| o.origin + d * k;
    let hit = |k: f64, s: &mut Scratch<'_>| {
        let (_, g) = eval_grad(t, at(k), s);
        Hit { t: k, normal: g.normalized() }
    };
    let mut k = t_enter;
    let mut v = eval_point(t, at(k), s);
    if v <= 0.0 {
        return Ok(Some(hit(k, s)));
    }
    for _ in 0..o.most_steps {
        let next = k + v.max(o.min_step);
        if next > t_exit {
            // The last stretch to `t_exit` itself.
            if k < t_exit && eval_point(t, at(t_exit), s) <= 0.0 {
                return Ok(Some(hit(bisect(t, &at, k, t_exit, s), s)));
            }
            return Ok(None);
        }
        let w = eval_point(t, at(next), s);
        if w <= 0.0 {
            return Ok(Some(hit(bisect(t, &at, k, next, s), s)));
        }
        k = next;
        v = w;
    }
    Err(InspectError::Budget { what: "ray steps", most: u64::from(o.most_steps) })
}

/// The bracket `(lo above zero, hi at or below)` narrowed until no float lies between.
fn bisect(t: &Tape<'_>, at: &impl Fn(f64) -> Vec3, mut lo: f64, mut hi: f64, s: &mut Scratch<'_>) -> f64 {
    for _ in 0..1100 {
        let mid = lo + (hi - lo) * 0.5;
        if mid <= lo || mid >= hi || next_up(lo) >= hi || next_down(hi) <= lo {
            break;
        }
        if eval_point(t, at(mid), s) <= 0.0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}
