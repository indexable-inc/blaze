//! An approximate convex decomposition for colliders: the mesh's box is split by axis-aligned
//! planes into cells, and each piece is the convex hull of the solid within its cell.
//!
//! PIECE. The hull of the solid inside a box is the hull of its extreme points, all of which are
//! among: the mesh's vertices in the box (closed), the crossings of mesh edges with the box's
//! faces, the crossings of mesh triangles with the box's edges, and the box's corners inside the
//! solid (a corner is inside when the triangles above it along `+z` exit the solid once more
//! than they enter it). Each piece's hull is built from those points, so the pieces' union holds
//! the whole solid, every vertex included (up to the hull's tolerance and the `f32` rounding of
//! the crossing points).
//!
//! SPLIT. A piece's concavity is its hull's volume minus the solid's volume in its cell. The
//! solid's volume in a cell is ESTIMATED by a fixed grid of `rays × rays` rays along `+z` over
//! the mesh's box: a ray's length inside the solid between `z0` and `z1` is
//! `Σ sign(n_z) · clamp(z_hit, z0, z1)` over its hits (exits count up, entries down; no sorting,
//! and a closed outward mesh needs no pairing), and a cell's estimate sums the rays whose foot is
//! in the cell's footprint, times each ray's footprint area. While a piece's concavity is above
//! `concavity · V` (`V` the mesh's exact volume), its depth below [`MeshLimits::most_split_depth`] and
//! the piece count below [`MeshLimits::most_pieces`], it is split by the plane, among the three axes
//! at a quarter, half and three quarters of the cell, whose two halves have the least summed
//! concavity (ties to the lower axis, then the lower fraction). No recursion: a stack of cells.
//!
//! LIMITS. A cell whose solid is flat (its points within one plane) or empty is no piece. The
//! work is `O(pieces · 19 · (T + hull))` plus `O(rays² + T · rays²)` for the grid, so the input
//! should be a simplified level (thousands of triangles), not the render mesh; the estimate's
//! error falls as `1/rays`. No primitive (box, capsule) is fitted.
//!
//! DETERMINISM: a function of the input in its order (the hull's diagonal on coplanar points
//! follows its input order); not order-free.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::MeshLimits;
use crate::check::Verified;
use crate::geom::{Aabb, D3, P3, cross, dot, narrow, sub, tri_normal, wide};
use crate::hull::{Hull, HullError, HullOpts, convex_hull};
use crate::ids::TriId;
use crate::measure::{bounds, mass_properties};
use crate::mesh::MeshView;
use crate::vecs::{V, at, len32, push};

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DecomposeOpts<'a, 's> {
    /// The most concavity a piece may keep, as a fraction of the mesh's volume (0.01 to 0.05).
    pub concavity: f64,
    /// The side of the estimate's ray grid (32 to 128).
    pub rays: u32,
    pub limits: MeshLimits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[derive(Debug)]
pub struct ConvexPiece<'a> {
    pub hull: Hull<'a>,
    pub cell: Aabb,
    /// The piece's estimated concavity (module documentation).
    pub concavity: f64,
}

#[derive(Debug)]
pub struct Decomposition<'a> {
    pub pieces: Vec<ConvexPiece<'a>, Alloc<'a>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DecomposeError {
    /// A concavity that is negative or not finite.
    BadConcavity { concavity: f64 },
    /// A ray grid of no rays, or of more than 4,096 a side.
    BadRays { rays: u32 },
    /// The hull of a piece failed for a reason other than a flat or empty cell.
    Hull { cell: Aabb, error: HullError },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for DecomposeError {
    fn from(e: AllocFailure) -> DecomposeError {
        DecomposeError::Alloc(e)
    }
}

/// The ray grid's hits: `hits[start[r]..start[r + 1]]` are ray `r`'s, as `(z, sign of n_z)`.
struct Rays<'s> {
    side: u32,
    lo: [f64; 2],
    step: [f64; 2],
    start: V<'s, u32>,
    hits: V<'s, (f64, f64)>,
}

/// The barycentric weights of the point `xy` in triangle `abc`'s footprint, when it is inside
/// (closed).
fn footprint(tri: [D3; 3], xy: [f64; 2]) -> Option<[f64; 3]> {
    let [a, b, c] = tri;
    let [x, y] = xy;
    let area = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
    if area == 0.0 {
        return None;
    }
    let wa = ((b[0] - x) * (c[1] - y) - (c[0] - x) * (b[1] - y)) / area;
    let wb = ((c[0] - x) * (a[1] - y) - (a[0] - x) * (c[1] - y)) / area;
    let wc = 1.0 - wa - wb;
    if wa >= 0.0 && wb >= 0.0 && wc >= 0.0 { Some([wa, wb, wc]) } else { None }
}

fn rays<'s>(view: &MeshView<'_>, side: u32, s: Alloc<'s>) -> Result<Rays<'s>, AllocFailure> {
    let box_ = bounds(view);
    let lo = [f64::from(box_.min[0]), f64::from(box_.min[1])];
    let ext = box_.extent();
    let step = [ext[0] / f64::from(side), ext[1] / f64::from(side)];
    let mut tagged: V<'_, (u32, f64, f64)> = Vec::new_in(s);
    for t in 0..view.triangle_count() {
        let c = view.corners(TriId(t));
        let (a, b, cc) = (wide(c[0]), wide(c[1]), wide(c[2]));
        let nz = tri_normal(a, b, cc)[2];
        if nz == 0.0 {
            continue;
        }
        let sign = if nz > 0.0 { 1.0 } else { -1.0 };
        let index = |v: f64, axis: usize| -> u32 {
            if step[axis] > 0.0 { ((v - lo[axis]) / step[axis]).clamp(0.0, f64::from(side - 1)) as u32 } else { 0 }
        };
        let (i0, i1) = (index(a[0].min(b[0]).min(cc[0]), 0), index(a[0].max(b[0]).max(cc[0]), 0));
        let (j0, j1) = (index(a[1].min(b[1]).min(cc[1]), 1), index(a[1].max(b[1]).max(cc[1]), 1));
        for i in i0..=i1 {
            for j in j0..=j1 {
                let x = lo[0] + (f64::from(i) + 0.5) * step[0];
                let y = lo[1] + (f64::from(j) + 0.5) * step[1];
                if let Some(w) = footprint([a, b, cc], [x, y]) {
                    push(&mut tagged, (i * side + j, w[0] * a[2] + w[1] * b[2] + w[2] * cc[2], sign))?;
                }
            }
        }
    }
    tagged.sort_unstable_by(|p, q| p.0.cmp(&q.0).then(p.1.total_cmp(&q.1)).then(p.2.total_cmp(&q.2)));
    let total = side * side;
    let mut start = crate::vecs::filled(at(total) + 1, 0u32, s)?;
    let mut hits = Vec::try_with_capacity_in(tagged.len(), s)?;
    for &(r, z, sign) in tagged.iter() {
        start[at(r) + 1] += 1;
        push(&mut hits, (z, sign))?;
    }
    for r in 0..at(total) {
        start[r + 1] += start[r];
    }
    Ok(Rays { side, lo, step, start, hits })
}

impl Rays<'_> {
    /// The estimated solid volume inside `cell`.
    fn volume(&self, cell: &Aabb) -> f64 {
        let (z0, z1) = (f64::from(cell.min[2]), f64::from(cell.max[2]));
        let mut total = 0.0;
        for i in 0..self.side {
            let x = self.lo[0] + (f64::from(i) + 0.5) * self.step[0];
            if x < f64::from(cell.min[0]) || x >= f64::from(cell.max[0]) {
                continue;
            }
            for j in 0..self.side {
                let y = self.lo[1] + (f64::from(j) + 0.5) * self.step[1];
                if y < f64::from(cell.min[1]) || y >= f64::from(cell.max[1]) {
                    continue;
                }
                let r = at(i * self.side + j);
                for &(z, sign) in &self.hits[at(self.start[r])..at(self.start[r + 1])] {
                    total += sign * z.clamp(z0, z1);
                }
            }
        }
        total * self.step[0] * self.step[1]
    }
}

/// Whether `p` is inside the solid: the hits above it on a `+z` ray sum to one exit.
fn inside(view: &MeshView<'_>, p: D3) -> bool {
    let mut count = 0.0f64;
    for t in 0..view.triangle_count() {
        let c = view.corners(TriId(t));
        let (a, b, cc) = (wide(c[0]), wide(c[1]), wide(c[2]));
        let nz = tri_normal(a, b, cc)[2];
        if nz == 0.0 {
            continue;
        }
        if let Some(w) = footprint([a, b, cc], [p[0], p[1]])
            && w[0] * a[2] + w[1] * b[2] + w[2] * cc[2] > p[2]
        {
            count += if nz > 0.0 { 1.0 } else { -1.0 };
        }
    }
    count > 0.5
}

/// The points whose hull is the solid's hull in `cell` (module documentation).
fn cell_points<'s>(view: &MeshView<'_>, cell: &Aabb, s: Alloc<'s>) -> Result<V<'s, P3>, AllocFailure> {
    let mut out: V<'s, P3> = Vec::new_in(s);
    let (lo, hi) = (wide(cell.min), wide(cell.max));
    let within = |p: D3, skip: usize| (0..3).all(|a| a == skip || (lo[a] <= p[a] && p[a] <= hi[a]));
    for &p in view.positions() {
        if cell.contains(p) {
            push(&mut out, p)?;
        }
    }
    for t in 0..view.triangle_count() {
        let c = view.corners(TriId(t));
        let w = [wide(c[0]), wide(c[1]), wide(c[2])];
        // Edges against the six face planes.
        for k in 0..3 {
            let (a, b) = (w[k], w[(k + 1) % 3]);
            for axis in 0..3 {
                for plane in [lo[axis], hi[axis]] {
                    if (a[axis] < plane && b[axis] > plane) || (a[axis] > plane && b[axis] < plane) {
                        let f = (plane - a[axis]) / (b[axis] - a[axis]);
                        let mut p = [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f];
                        p[axis] = plane;
                        if within(p, axis) {
                            push(&mut out, narrow(p))?;
                        }
                    }
                }
            }
        }
        // The triangle against the twelve box edges.
        let n = cross(sub(w[1], w[0]), sub(w[2], w[0]));
        for axis in 0..3 {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            if n[axis] == 0.0 {
                continue;
            }
            for cu in [lo[u], hi[u]] {
                for cv in [lo[v], hi[v]] {
                    // The line `x_u = cu, x_v = cv` meets the plane at one `x_axis`.
                    let mut p = [0.0f64; 3];
                    p[u] = cu;
                    p[v] = cv;
                    p[axis] = w[0][axis] - (n[u] * (cu - w[0][u]) + n[v] * (cv - w[0][v])) / n[axis];
                    if p[axis] < lo[axis] || p[axis] > hi[axis] {
                        continue;
                    }
                    if in_triangle(w, p) {
                        push(&mut out, narrow(p))?;
                    }
                }
            }
        }
    }
    for corner in 0..8u32 {
        let p = [
            if corner & 1 == 0 { lo[0] } else { hi[0] },
            if corner & 2 == 0 { lo[1] } else { hi[1] },
            if corner & 4 == 0 { lo[2] } else { hi[2] },
        ];
        if inside(view, p) {
            push(&mut out, narrow(p))?;
        }
    }
    Ok(out)
}

/// Whether `p`, on the triangle's plane, is inside it (closed).
fn in_triangle(w: [D3; 3], p: D3) -> bool {
    let n = cross(sub(w[1], w[0]), sub(w[2], w[0]));
    (0..3).all(|k| dot(cross(sub(w[(k + 1) % 3], w[k]), sub(p, w[k])), n) >= 0.0)
}

/// What every piece is made against.
struct Ctx<'v, 'g, 's> {
    view: MeshView<'v>,
    grid: &'g Rays<'s>,
    limits: MeshLimits,
    scratch: Alloc<'s>,
}

impl Ctx<'_, '_, '_> {
    /// A cell's piece in `alloc`, or `None` for a flat or empty one.
    fn piece<'a>(&self, cell: Aabb, alloc: Alloc<'a>) -> Result<Option<ConvexPiece<'a>>, DecomposeError> {
        let points = cell_points(&self.view, &cell, self.scratch)?;
        match convex_hull(&points, HullOpts { limits: self.limits, alloc, scratch: self.scratch }) {
            Ok(hull) => {
                let concavity = hull.volume() - self.grid.volume(&cell);
                Ok(Some(ConvexPiece { hull, cell, concavity }))
            }
            Err(HullError::TooFewPoints { .. } | HullError::Degenerate { .. }) => Ok(None),
            Err(error) => Err(DecomposeError::Hull { cell, error }),
        }
    }
}

/// The two halves of `cell` split on `axis` at `fraction`.
fn halves(cell: Aabb, axis: usize, fraction: f64) -> (Aabb, Aabb) {
    let at_ = f64::from(cell.min[axis]) + (f64::from(cell.max[axis]) - f64::from(cell.min[axis])) * fraction;
    let plane = at_ as f32;
    let (mut a, mut b) = (cell, cell);
    a.max[axis] = plane;
    b.min[axis] = plane;
    (a, b)
}

pub fn decompose<'a>(mesh: &Verified<'_>, opts: DecomposeOpts<'a, '_>) -> Result<Decomposition<'a>, DecomposeError> {
    if !(opts.concavity >= 0.0 && opts.concavity.is_finite()) {
        return Err(DecomposeError::BadConcavity { concavity: opts.concavity });
    }
    if opts.rays == 0 || opts.rays > 4096 {
        return Err(DecomposeError::BadRays { rays: opts.rays });
    }
    let view = mesh.view();
    let s = opts.scratch;
    let whole = bounds(&view);
    let grid = rays(&view, opts.rays, s)?;
    let ctx = Ctx { view, grid: &grid, limits: opts.limits, scratch: s };
    let allowed = opts.concavity * mass_properties(mesh).volume.as_m3();
    let mut done: Vec<ConvexPiece<'a>, Alloc<'a>> = Vec::new_in(opts.alloc);
    let mut stack: V<'_, (ConvexPiece<'a>, u32)> = Vec::new_in(s);
    if let Some(root) = ctx.piece(whole, opts.alloc)? {
        push(&mut stack, (root, 0))?;
    }
    while let Some((current, depth)) = stack.pop() {
        let room = len32(done.len()) + len32(stack.len()) + 2 <= opts.limits.most_pieces;
        if current.concavity <= allowed || depth >= opts.limits.most_split_depth || !room {
            push(&mut done, current)?;
            continue;
        }
        let mut best: Option<(f64, usize, f64)> = None;
        for axis in 0..3 {
            if current.cell.min[axis] >= current.cell.max[axis] {
                continue;
            }
            for fraction in [0.25, 0.5, 0.75] {
                let (a, b) = halves(current.cell, axis, fraction);
                let mut sum = 0.0;
                for half in [a, b] {
                    if let Some(p) = ctx.piece(half, s)? {
                        sum += p.concavity;
                    }
                }
                if best.is_none_or(|(b_sum, _, _)| sum < b_sum) {
                    best = Some((sum, axis, fraction));
                }
            }
        }
        let Some((_, axis, fraction)) = best else {
            push(&mut done, current)?;
            continue;
        };
        let (a, b) = halves(current.cell, axis, fraction);
        // The second half is pushed first, so the first is split first.
        for half in [b, a] {
            if let Some(p) = ctx.piece(half, opts.alloc)? {
                push(&mut stack, (p, depth + 1))?;
            }
        }
    }
    Ok(Decomposition { pieces: done })
}
