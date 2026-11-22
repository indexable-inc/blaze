//! The convex hull of a point set by quickhull (C. B. Barber, D. P. Dobkin and H. Huhdanpaa,
//! "The Quickhull algorithm for convex hulls", ACM TOMS 22(4), 1996), in `f64` on the stored
//! `f32` points.
//!
//! TOLERANCE. `eps = 2^-40 · (max|x| + max|y| + max|z|)` over the input: a point within `eps` of a
//! face's plane is on it, not outside. A face's plane is its unit normal (the normalised cross
//! product of two edge differences, each difference of two `f32`s exact in `f64`) and offset, so a
//! signed distance is accurate to a few `2^-53` of the same scale: `eps` leaves three orders of
//! magnitude for rounding. PROMISE: every input point is at most `eps` outside every face's plane
//! (`Hull::tolerance`), and so is every hull vertex (the hull is convex within `eps`).
//!
//! STEPS. The starting simplex is the farthest pair among the six axis-extreme points (ties to the
//! lower index), the point farthest from their line, the point farthest from their plane; each
//! other point goes to the outside set of the first face it is more than `eps` above. Then, while
//! a face has an outside set: the lowest-numbered such face's farthest point (ties to the lower
//! index) is the eye; the faces it sees are found by a walk from that face over neighbours; the
//! horizon's edges each make a new face with the eye; the seen faces' outside points go to the
//! first new face they are more than `eps` above, or are dropped (they are inside). Each point
//! becomes a vertex at most once, so the steps are bounded by the input.
//!
//! FAILURES, typed: fewer than four points, all points within `eps` of one point, one line or
//! one plane ([`HullError::Degenerate`]), a horizon that is not one loop or a new face of no area
//! (rounding beyond the tolerance: [`HullError::Numerical`]), and the limits.
//!
//! DETERMINISM: a function of the input in its order. The output's vertices are ordered by input
//! index and its triangles sorted, so for points in general position (no four within `eps` of one
//! plane on the hull, no repeated point) the output's positions and triangles are ORDER-FREE;
//! where four hull points are coplanar, which diagonal splits their face follows the input order.

#![blaze::sans_io]

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::MeshLimits;
use crate::geom::{D3, P3, cross, distance, dot, sub, unit, wide};
use crate::plant::HullPlant;
use crate::vecs::{V, at, filled, len32, push};
use crate::weld::least_first;

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct HullOpts<'a, 's> {
    pub limits: MeshLimits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// A closed convex triangle mesh: vertices in input order, triangles outward and sorted.
#[derive(Debug)]
pub struct Hull<'a> {
    pub points: Vec<P3, Alloc<'a>>,
    /// The input index of each hull vertex.
    pub sources: Vec<u32, Alloc<'a>>,
    pub triangles: Vec<[u32; 3], Alloc<'a>>,
    /// The `eps` the hull was built with (module documentation).
    pub tolerance: f64,
}

impl Hull<'_> {
    /// The hull's volume (exact polyhedral sum about its first vertex).
    pub fn volume(&self) -> f64 {
        match self.points.first() {
            Some(&o) => crate::measure::signed_volume(&self.points, &self.triangles, wide(o)),
            None => 0.0,
        }
    }

    /// The signed distance of `p` above the plane of triangle `t` (positive outside).
    pub fn height(&self, t: usize, p: P3) -> f64 {
        let tri = self.triangles[t];
        let a = wide(self.points[at(tri[0])]);
        let n = cross(sub(wide(self.points[at(tri[1])]), a), sub(wide(self.points[at(tri[2])]), a));
        match unit(n) {
            Some(n) => dot(n, sub(wide(p), a)),
            None => 0.0,
        }
    }
}

/// What a degenerate input is degenerate as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flat {
    /// Every point within `eps` of one point.
    Point,
    Line,
    Plane,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum HullError {
    TooFewPoints { points: usize },
    TooManyPoints { points: usize, most: u32 },
    NonFinite { point: u32, at: P3 },
    Degenerate { flat: Flat },
    /// The hull grew past [`MeshLimits::most_hull_vertices`].
    TooManyVertices { most: u32 },
    /// Rounding beyond the tolerance at step `step` (with the eye point `eye`).
    Numerical { step: u32, eye: u32 },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for HullError {
    fn from(e: AllocFailure) -> HullError {
        HullError::Alloc(e)
    }
}

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Face {
    v: [u32; 3],
    /// The face across edge `k` (`v[k]` to `v[k + 1]`).
    nbr: [u32; 3],
    n: D3,
    d: f64,
    alive: bool,
    /// The head of this face's outside list (through `next`), its farthest point and distance.
    first: u32,
    far: u32,
    far_dist: f64,
}

struct State<'s, 'p> {
    pts: &'p [P3],
    faces: V<'s, Face>,
    next: V<'s, u32>,
    eps: f64,
}

impl State<'_, '_> {
    fn p(&self, i: u32) -> D3 {
        wide(self.pts[at(i)])
    }

    fn height(&self, f: u32, p: u32) -> f64 {
        let face = &self.faces[at(f)];
        dot(face.n, self.p(p)) - face.d
    }

    /// A face on `v`, outward, or `None` when it has no area.
    fn face(&self, v: [u32; 3]) -> Option<Face> {
        let a = self.p(v[0]);
        let n = unit(cross(sub(self.p(v[1]), a), sub(self.p(v[2]), a)))?;
        Some(Face { v, nbr: [NONE; 3], n, d: dot(n, a), alive: true, first: NONE, far: NONE, far_dist: 0.0 })
    }

    /// `p` onto the outside list of the first of `faces` it is above by more than `eps`.
    fn assign(&mut self, p: u32, faces: core::ops::Range<u32>) -> bool {
        for f in faces {
            let h = self.height(f, p);
            if h > self.eps {
                let face = &mut self.faces[at(f)];
                self.next[at(p)] = face.first;
                face.first = p;
                if face.far == NONE || h > face.far_dist || (h == face.far_dist && p < face.far) {
                    face.far = p;
                    face.far_dist = h;
                }
                return true;
            }
        }
        false
    }
}

pub fn convex_hull<'a>(points: &[P3], opts: HullOpts<'a, '_>) -> Result<Hull<'a>, HullError> {
    convex_hull_planted(points, opts, NoPlant)
}

/// [`convex_hull`] with a planted bug, for the tests' controls.
pub fn convex_hull_planted<'a>(
    points: &[P3],
    opts: HullOpts<'a, '_>,
    plant: impl Planted<HullPlant>,
) -> Result<Hull<'a>, HullError> {
    if points.len() > blaze_int::idx(opts.limits.most_hull_points) {
        return Err(HullError::TooManyPoints { points: points.len(), most: opts.limits.most_hull_points });
    }
    if points.len() < 4 {
        return Err(HullError::TooFewPoints { points: points.len() });
    }
    let n = len32(points.len());
    let mut most = [0.0f64; 3];
    for (i, p) in points.iter().enumerate() {
        if !p.iter().all(|x| x.is_finite()) {
            return Err(HullError::NonFinite { point: len32(i), at: *p });
        }
        for a in 0..3 {
            most[a] = most[a].max(f64::from(p[a]).abs());
        }
    }
    let eps = (most[0] + most[1] + most[2]) / 1_099_511_627_776.0;
    let s = opts.scratch;
    let mut st = State { pts: points, faces: Vec::new_in(s), next: filled(at(n), NONE, s)?, eps };
    let simplex = simplex(&st, n)?;
    let [i0, i1, i2, i3] = simplex;
    for (tri, opposite) in [([i0, i1, i2], i3), ([i0, i1, i3], i2), ([i0, i2, i3], i1), ([i1, i2, i3], i0)] {
        let Some(mut f) = st.face(tri) else {
            return Err(HullError::Degenerate { flat: Flat::Plane });
        };
        if dot(f.n, st.p(opposite)) - f.d > 0.0 {
            let Some(flipped) = st.face([tri[0], tri[2], tri[1]]) else {
                return Err(HullError::Degenerate { flat: Flat::Plane });
            };
            f = flipped;
        }
        push(&mut st.faces, f)?;
    }
    link(&mut st.faces, 0..4, s)?;
    let mut skipped = false;
    for p in 0..n {
        if simplex.contains(&p) {
            continue;
        }
        if plant.has(HullPlant::SkipVertex) && !skipped && (0..4).any(|f| st.height(f, p) > eps) {
            skipped = true;
            continue;
        }
        st.assign(p, 0..4);
    }
    let mut vertices = 4u32;
    let mut cursor = 0u32;
    let mut stamp: V<'_, u32> = filled(4, 0u32, s)?;
    let mut visible: V<'_, u32> = Vec::new_in(s);
    let mut stack: V<'_, u32> = Vec::new_in(s);
    let mut horizon: V<'_, (u32, u32, u32)> = Vec::new_in(s);
    let mut ends: V<'_, (u32, u32)> = Vec::new_in(s);
    let mut gathered: V<'_, u32> = Vec::new_in(s);
    let mut step = 0u32;
    loop {
        while at(cursor) < st.faces.len() && !(st.faces[at(cursor)].alive && st.faces[at(cursor)].first != NONE) {
            cursor += 1;
        }
        if at(cursor) == st.faces.len() {
            break;
        }
        step += 1;
        vertices += 1;
        if vertices > opts.limits.most_hull_vertices {
            return Err(HullError::TooManyVertices { most: opts.limits.most_hull_vertices });
        }
        let eye = st.faces[at(cursor)].far;
        // The faces the eye sees, by a walk from the face it was found over.
        visible.clear();
        stack.clear();
        push(&mut stack, cursor)?;
        stamp[at(cursor)] = step;
        while let Some(f) = stack.pop() {
            push(&mut visible, f)?;
            for k in 0..3 {
                let g = st.faces[at(f)].nbr[k];
                if stamp[at(g)] != step && st.height(g, eye) > eps {
                    stamp[at(g)] = step;
                    push(&mut stack, g)?;
                }
            }
        }
        visible.sort_unstable();
        horizon.clear();
        for &f in visible.iter() {
            let face = st.faces[at(f)];
            for k in 0..3 {
                let g = face.nbr[k];
                if stamp[at(g)] != step {
                    push(&mut horizon, (face.v[k], face.v[(k + 1) % 3], g))?;
                }
            }
        }
        let new_start = len32(st.faces.len());
        for &(a, b, g) in horizon.iter() {
            let Some(mut f) = st.face([a, b, eye]) else {
                return Err(HullError::Numerical { step, eye });
            };
            let id = len32(st.faces.len());
            f.nbr[0] = g;
            let old = &mut st.faces[at(g)];
            let Some(k) = (0..3).find(|&k| old.v[k] == b && old.v[(k + 1) % 3] == a) else {
                return Err(HullError::Numerical { step, eye });
            };
            old.nbr[k] = id;
            push(&mut st.faces, f)?;
            push(&mut stamp, 0)?;
        }
        // Each new face's edge `b → eye` meets the new face whose `a` is that `b`.
        ends.clear();
        for id in new_start..len32(st.faces.len()) {
            push(&mut ends, (st.faces[at(id)].v[0], id))?;
        }
        ends.sort_unstable();
        for pair in ends.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(HullError::Numerical { step, eye });
            }
        }
        for id in new_start..len32(st.faces.len()) {
            let b = st.faces[at(id)].v[1];
            let Ok(found) = ends.binary_search_by(|e| e.0.cmp(&b)) else {
                return Err(HullError::Numerical { step, eye });
            };
            let other = ends[found].1;
            st.faces[at(id)].nbr[1] = other;
            st.faces[at(other)].nbr[2] = id;
        }
        gathered.clear();
        for &f in visible.iter() {
            st.faces[at(f)].alive = false;
            let mut p = st.faces[at(f)].first;
            while p != NONE {
                push(&mut gathered, p)?;
                p = st.next[at(p)];
            }
        }
        gathered.sort_unstable();
        let end = len32(st.faces.len());
        for &p in gathered.iter() {
            if p != eye {
                st.assign(p, new_start..end);
            }
        }
    }
    output(&st, opts.alloc, s)
}

fn simplex(st: &State<'_, '_>, n: u32) -> Result<[u32; 4], HullError> {
    let eps = st.eps;
    let mut extreme = [0u32; 6];
    for p in 0..n {
        let q = st.pts[at(p)];
        for a in 0..3 {
            if q[a] < st.pts[at(extreme[a * 2])][a] {
                extreme[a * 2] = p;
            }
            if q[a] > st.pts[at(extreme[a * 2 + 1])][a] {
                extreme[a * 2 + 1] = p;
            }
        }
    }
    let (mut i0, mut i1, mut best) = (0, 0, -1.0);
    for x in 0..6 {
        for y in x + 1..6 {
            let d = distance(st.p(extreme[x]), st.p(extreme[y]));
            if d > best {
                (i0, i1, best) = (extreme[x], extreme[y], d);
            }
        }
    }
    if !(best > eps) {
        return Err(HullError::Degenerate { flat: Flat::Point });
    }
    let (a, ab) = (st.p(i0), sub(st.p(i1), st.p(i0)));
    let (mut i2, mut best) = (0, -1.0);
    for p in 0..n {
        let d = crate::geom::length(cross(ab, sub(st.p(p), a))) / crate::geom::length(ab);
        if d > best {
            (i2, best) = (p, d);
        }
    }
    if !(best > eps) {
        return Err(HullError::Degenerate { flat: Flat::Line });
    }
    let Some(nrm) = unit(cross(ab, sub(st.p(i2), a))) else {
        return Err(HullError::Degenerate { flat: Flat::Line });
    };
    let (mut i3, mut best) = (0, -1.0);
    for p in 0..n {
        let d = dot(nrm, sub(st.p(p), a)).abs();
        if d > best {
            (i3, best) = (p, d);
        }
    }
    if !(best > eps) {
        return Err(HullError::Degenerate { flat: Flat::Plane });
    }
    Ok([i0, i1, i2, i3])
}

/// The neighbours of `faces[ids]` among themselves, by matching each directed edge with its
/// reverse.
fn link(faces: &mut V<'_, Face>, ids: core::ops::Range<u32>, s: Alloc<'_>) -> Result<(), AllocFailure> {
    let mut edges: V<'_, (u32, u32, u32, u32)> = Vec::new_in(s);
    for f in ids {
        let v = faces[at(f)].v;
        for k in 0..3u32 {
            push(&mut edges, (v[at(k)], v[at((k + 1) % 3)], f, k))?;
        }
    }
    edges.sort_unstable();
    for i in 0..edges.len() {
        let (a, b, f, k) = edges[i];
        if let Ok(j) = edges.binary_search_by(|e| (e.0, e.1).cmp(&(b, a))) {
            faces[at(f)].nbr[at(k)] = edges[j].2;
        }
    }
    Ok(())
}

fn output<'a>(st: &State<'_, '_>, alloc: Alloc<'a>, s: Alloc<'_>) -> Result<Hull<'a>, HullError> {
    let n = len32(st.pts.len());
    let mut new_index = filled(at(n), NONE, s)?;
    for f in st.faces.iter().filter(|f| f.alive) {
        for &v in &f.v {
            new_index[at(v)] = 0;
        }
    }
    let mut hull =
        Hull { points: Vec::new_in(alloc), sources: Vec::new_in(alloc), triangles: Vec::new_in(alloc), tolerance: st.eps };
    for v in 0..n {
        if new_index[at(v)] == 0 {
            new_index[at(v)] = len32(hull.points.len());
            push(&mut hull.points, st.pts[at(v)])?;
            push(&mut hull.sources, v)?;
        }
    }
    for f in st.faces.iter().filter(|f| f.alive) {
        push(&mut hull.triangles, least_first([new_index[at(f.v[0])], new_index[at(f.v[1])], new_index[at(f.v[2])]]))?;
    }
    hull.triangles.sort_unstable();
    Ok(hull)
}
