//! Simplification by quadric error metrics (M. Garland and P. S. Heckbert, "Surface
//! simplification using quadric error metrics", SIGGRAPH 1997): edges are collapsed cheapest
//! first, each vertex carrying the sum of the squared-distance quadrics of the planes of its
//! original triangles (area-weighted), plus a penalty quadric for each feature edge (a plane
//! through the edge perpendicular to its face, weight `PENALTY · |e|²`, Garland and Heckbert's
//! boundary constraint), and the merged vertex placed where the summed quadric is least (the
//! 3x3 solve; where it is singular, the best of the two ends and the middle, in that order).
//!
//! FEATURES. An edge is a feature when it is a crease mark, a border between parts or material
//! roles, a boundary (one triangle) or non-manifold (more than two). A vertex's feature degree is
//! how many feature edges it has: 0 is free, 2 is on a chain, any other count is a chain END
//! (a corner). A collapse is allowed only:
//! - between two free vertices (placed at the optimum);
//! - from a free vertex into a feature vertex (placed at the feature vertex);
//! - ALONG a feature edge from a chain vertex into a chain vertex (the optimum) or into a corner
//!   (placed at the corner).
//! So corners never move and are never removed, and features are only shortened along
//! themselves. The marks of a removed triangle's sides pass to the sides that replace them.
//!
//! VALIDITY, checked when a candidate is taken: the link condition (the vertices adjacent to
//! both ends are exactly the far corners of the triangles on the edge: the collapse keeps the
//! surface a manifold, T. K. Dey, H. Edelsbrunner, S. Guha and D. V. Nekhayev, "Topology
//! preserving edge contraction", 1999); no two triangles on one set of vertices after it (the
//! tetrahedron case the link condition admits); no triangle's normal turned by 90 degrees or more,
//! and no collapse may leave an ill-conditioned face in its fan (NORMAL-FLIP REJECTION).
//!
//! ERROR BOUND: a SUCCESSIVE MAP (J. Cohen, M. Olano and D. Manocha, "Appearance-preserving
//! simplification", SIGGRAPH 1998, its successive mapping), measured against the INPUT, never
//! summed step by step. The work keeps a map `M` from the input surface ONTO the live surface,
//! as PIECES: each live triangle owns convex polygons whose corners pair `cur` (a point on that
//! triangle) with `orig` (the input point mapped there), `M` affine on each. Every input point
//! `x` is in one piece and `|M(x) − x|` is affine-bounded there, so its largest value is at a
//! corner: the error of a piece is `base + max |cur − orig|` over its corners, exactly. Since `M`
//! is onto, every live point is `M(x)` of an input point within that error, and every input
//! point is that close to its image: the largest piece error is a two-sided (Hausdorff) bound.
//! The input is one piece per triangle, `cur = orig`. A collapse of `a` and `b` into `p` replaces
//! the REGION (the live triangles around either end) by the FAN (each region triangle not on the
//! edge, its `a` or `b` corner moved to `p`, keeping its slot), and composes `M` with a map from
//! region onto fan:
//! - VERTICAL, when region and fan are both graphs over one plane (every normal, before and after,
//!   at least `MIN_COS` along a direction `n`: of the bisector of the two most divergent normals
//!   and the normalised sum, the one with the larger least cosine), the fan closed around `p` and
//!   winding once about it: both then cover the link polygon once, and a region point goes to the
//!   fan point over it along `n`. Each piece is projected, cut by each fan triangle (its two
//!   spokes, one line per link vertex shared by both triangles on it so their sides complement
//!   exactly, and its link edge) and lifted onto it: affine per new piece, so still exact.
//! - BARYCENTRIC otherwise: a region triangle off the edge maps affinely onto its fan triangle;
//!   one on the edge `(a, b, c)` is squashed onto the segment `p c`, its pieces given to the first
//!   fan triangle holding `c` (marked squashed: kept even where cut to no area). Where no fan
//!   triangle holds `c`, or a map's arithmetic fails, the collapse is refused.
//!
//! MERGING. Pieces multiply under the vertical map: a cut leaves fragments of one input triangle in
//! several fan triangles, and a fan triangle gets fragments of one input triangle from several
//! region triangles. So each piece remembers its SOURCE, the input triangle its `orig` lies on, and
//! when a collapse is admitted, each live triangle's new fragments of one source are merged into one
//! piece: the convex hull of their corners (each keeping its `cur`), `orig` there the least-squares
//! affine fit `A` of their `orig` over `cur`, and `base` the largest `base + |orig − A(cur)|` over
//! their corners. Sound by the triangle inequality: on a fragment the old map and `A` are both
//! affine, so they differ by at most their largest difference at its corners; the hull holds every
//! fragment, and its extra area only claims more (every error is still measured at its corners).
//! A merge is kept when its error (`base + |cur − A(cur)|` at the hull's corners) is within the
//! target (for a triangle target, within the bound so far), and the bound takes it. A live
//! triangle then owns about one piece per input triangle under it, so the map stays the input's
//! size. One that would still own more than `FREEZE` becomes one piece, itself (`cur = orig`), with
//! `base` its largest piece error: a cap on a map's work, sound by the triangle inequality, but
//! every later error then starts at `base`. Merging replaced freezing at 64 pieces: a contoured
//! surface's vertices are off it by up to a leaf, so on such noisy input freezing set `base` near
//! the target early and stopped the long strips along a cylinder's axis (a prototype kept 3.4
//! times a noisy half tube's sagitta count frozen, 1.5 times merged).
//!
//! The bound carried out is the largest accepted collapse's or merge's error plus an allowance
//! for rounding and for the slivers the cutting's tolerance `TAU` may leave unclaimed (both `·`
//! the input's largest coordinate). The vertical map is what lets a flat face collapse to a few
//! triangles and a curved one to its curvature bound: sliding a vertex within a plane moves it far
//! but changes nothing (the barycentric map alone would cap every flat face at the tolerance's
//! ball, planted as `MovedOnly`). For [`Target::Error`] a collapse is admitted only when its error
//! is at most the target.
//!
//! COST, counted in [`DecimateStats`]. A candidate taken from the queue is evaluated (two stars and
//! a 3x3 solve) and checked (its rings); then its region is gathered (more than `MAX_REGION`
//! triangles is refused) and mapped. A map reads each piece of the region once: it projects the
//! corners, box-tests the piece only against the fan triangles whose boxes meet its region
//! triangle's pieces' box (about a third of the fan), and cuts it by the three lines of each fan
//! triangle it meets. It stops at the first piece over the target. Admitting the collapse sorts
//! its `P` new pieces by owner and source and merges each run: `O(P log P)`. So a map reads `R`
//! corners, the region's, at most `MAX_REGION · FREEZE` pieces, and with merging a live triangle
//! owns about `T / L` pieces (`T` input triangles, `L` live), so a run's maps read about
//! `c · T · (1 + ln(T / L_out))` corners in all: the last few collapses read the most. Maps are at
//! most the candidates offered: `3T` seeded plus one star per admitted collapse. The sagitta
//! property bounds the corners read per input triangle by that form, its control
//! `MeasuresEverything` (every map scanning every live piece: `O(T²)`).
//!
//! DETERMINISM. Ties between equal costs are broken by the edge's vertex ids, and vertex ids are
//! position classes numbered in position order (a total order on the positions, zeros made
//! positive); quadrics are summed in the canonical triangle order, and every per-triangle float
//! (its normal, its plane, the flip test) is computed from its least vertex, never from the
//! corner the input happened to list first (each rotation of `(b − a) × (c − a)` rounds
//! differently). So for a manifold input the output is a function of the mesh as a set of triangles:
//! ORDER-FREE (the input's triangle order and vertex numbering do not matter). Normals are
//! recomputed by [`crate::shade()`] at the given crease angle, split at marks and borders.
//! Tangents, UVs and colours are not carried: a mesh with them is refused by name.

#![blaze::sans_io]

use blaze_math::Vec3;
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Heap4, Vec};

use blaze_units::Length;

use crate::MeshLimits;
use crate::check::{CheckOpts, Defect, Proof, Verified, check};
use crate::feature::{FeatureProtect, FeatureZone};
use crate::geom::{D3, M3, P3, add, cross, distance, dot, length, length_up, narrow, plus_zeros, scale, solve3, sqrt, sub, tri_normal, unit, wide};
use crate::ids::{MaterialRole, PartId, TriId, VertexId, VertexPair};
use crate::mesh::{Column, Mesh, MeshVecs, MeshView, ShapeError, built, empty_vecs};
use crate::plant::DecimatePlant;
use crate::shade::{ShadeError, ShadeOpts, shade};
use crate::vecs::{V, at, counting, edge_runs, extend, filled, halves, len32, push, spots};
use crate::weld::least_first;

/// When to stop.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Target {
    /// Collapse until at most this many triangles are left.
    Triangles(u32),
    /// Collapse every edge whose collapse keeps the two-sided distance bound at most this (the
    /// mesh's positions are metres, so the bound is compared with `as_m()`, one rounding).
    Error(Length),
    /// Game-asset budget target: collapse freely within `error` (removing flat/coplanar
    /// redundancy even below `max_triangles`), and if the mesh still exceeds `max_triangles`,
    /// continue collapsing cheapest-first until `live <= max_triangles` or the two-sided error
    /// would exceed `max_error` (when `Some`, stopping with [`Stop::ErrorCeiling`]).
    Budget {
        /// Geometric accuracy target collapsed freely to eliminate coplanar/redundant triangles.
        error: Length,
        /// Per-part triangle budget cap.
        max_triangles: u32,
        /// Optional hard two-sided error ceiling that stops decimation before destroying shapes.
        max_error: Option<Length>,
    },
}

/// What is simplified: any shape-checked mesh, or a verified one, whose output is then
/// verified too (the collapses keep every property `check` establishes).
#[derive(Clone, Copy, Debug)]
pub enum Source<'v> {
    Open(MeshView<'v>),
    Verified(Verified<'v>),
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DecimateOpts<'a, 's, 'z> {
    pub target: Target,
    /// The crease angle the output's normals are split at, as its cosine (see `shade`).
    pub cos_crease: f32,
    /// Optional spatial feature zones that tighten error/normal bounds and prioritize preserving
    /// geometry around critical features (keyholes, bevels, mating surfaces).
    pub zones: &'z [FeatureZone] = &[],
    pub limits: MeshLimits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Why simplification stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// The target was met.
    Reached,
    /// Every collapse left would break a rule above before the triangle target was met.
    NoValidCollapse,
    /// For [`Target::Budget`], every remaining collapse before `max_triangles` would exceed
    /// `max_error`.
    ErrorCeiling,
}

#[derive(Debug)]
pub struct Decimated<'a> {
    pub mesh: Mesh<'a>,
    /// The two-sided distance bound between input and output (module documentation), rounded
    /// UP to the nanometre so it stays a bound.
    pub error_bound: Length,
    pub stop: Stop,
    /// What the run did, in work units, not time (the module's COST).
    pub stats: DecimateStats,
    proof: Option<Proof>,
}

/// Work counts of one simplification (module documentation, COST).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct DecimateStats {
    /// Candidates taken from the queue that were current (neither end collapsed since).
    pub taken: u64,
    /// Collapses admitted.
    pub collapses: u64,
    /// Maps built (vertical or barycentric; a candidate refused by the vertical map's conditions
    /// builds both).
    pub maps: u64,
    /// Corners of pieces the maps read, summed over every map.
    pub corners: u64,
    /// The most corners one map read.
    pub most_corners: u64,
    /// Candidates refused: by the error target, and by `MAX_REGION`.
    pub over_error: u64,
    pub over_region: u64,
    /// Runs of fragments of one source merged into one piece, and runs left as they were (the
    /// merge's error over the target).
    pub merged: u64,
    pub not_merged: u64,
    /// Triangles frozen (more than `FREEZE` pieces after merging).
    pub frozen: u64,
}

impl Decimated<'_> {
    /// The output with its proof, when the input was verified. The proof is the verifier's,
    /// run once on the output (a new mesh, so this is its one check, rule 74): the collapse rules
    /// keep every property, and a failure there is reported as [`DecimateError::LostProof`], a
    /// bug in this module, never passed on as a proof.
    pub fn verified(&self) -> Option<Verified<'_>> {
        self.proof.map(|proof| Verified::with_proof(self.mesh.view(), proof))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DecimateError {
    TooManyVertices { vertices: u32, most: u32 },
    TooManyTriangles { triangles: u32, most: u32 },
    /// A column simplification does not carry.
    NotCarried { column: Column },
    /// An error target that is negative.
    BadErrorTarget { error: Length },
    /// A triangle with two corners at one position, or of zero area: an open input must be free
    /// of them too.
    Degenerate { tri: TriId },
    Shade(ShadeError),
    Alloc(AllocFailure),
    /// The mesh built here did not fit together: a bug in `simplify`.
    Built(ShapeError),
    /// The output of a verified input failed the verifier: a bug in `simplify`, named with the
    /// defect.
    LostProof(Defect),
}

impl From<AllocFailure> for DecimateError {
    fn from(e: AllocFailure) -> DecimateError {
        DecimateError::Alloc(e)
    }
}

/// The weight of a feature edge's penalty plane, per squared length of the edge.
const PENALTY: f64 = 100.0;
const NONE: u32 = u32::MAX;
/// The least cosine between the vertical map's direction and any normal of region or fan: the
/// lift onto a fan triangle stretches a projected length by at most `1 / MIN_COS`, so the
/// cutting tolerance's slivers stay within the allowance.
const MIN_COS: f64 = 0.125;
/// The cutting tolerance, times the input's largest coordinate: a projected point this close to
/// a spoke is on it, and one this far outside a link edge is in.
const TAU: f64 = 1e-12;
/// The allowance added to every collapse's error, times the input's largest coordinate: the
/// rounding of the maps (a few operations per corner, composed over at most every collapse) and
/// the slivers of width `TAU`, stretched by at most `1 / MIN_COS`, that the cutting may leave
/// unclaimed.
const ALLOWANCE: f64 = 1e-9;
/// The most pieces a live triangle owns before it is frozen into one (module documentation,
/// MERGING): combined with adaptive slack-gated freezing for low-error flat/gently-curved faces,
/// this keeps worst-case map work tightly bounded on high-resolution CAD meshes while giving
/// noisy curved surfaces ample headroom before freezing.
const FREEZE: u32 = 192;
/// The most triangles a collapse's region may hold (both ends' stars): a larger one is refused. A
/// vertex of a contoured surface has six neighbours or so, and the prototypes' largest region was
/// 38; the cap bounds a map's pieces, not the result.
const MAX_REGION: usize = 64;

/// One corner of a piece: a point of the live surface and the input point the map sends there.
#[derive(Clone, Copy, Debug)]
struct Corr {
    cur: D3,
    orig: D3,
}

/// A convex polygon of the map, its corners `corr[first .. first + len]`. `base` is the distance
/// already spent before the piece's `orig` (zero until it is merged or its triangle frozen);
/// `squashed` marks a piece the barycentric map flattened onto a segment; `src` is the input
/// triangle its `orig` lies on, by its place in the canonical order (`NONE` for a frozen
/// triangle's piece, never merged).
#[derive(Clone, Copy, Debug)]
struct Piece {
    first: u32,
    len: u32,
    squashed: bool,
    base: f64,
    src: u32,
}

/// A piece's corner projected for cutting: where it lies in the plane, and its input point.
#[derive(Clone, Copy, Debug)]
struct Vtx {
    at: [f64; 2],
    orig: D3,
}

/// A directed line in the plane, its direction unit length.
#[derive(Clone, Copy, Debug)]
struct Line {
    at: [f64; 2],
    dir: [f64; 2],
}

impl Line {
    /// The line from `a` towards `b`, `None` when they coincide or are not finite.
    fn through(a: [f64; 2], b: [f64; 2]) -> Option<Line> {
        let d = [b[0] - a[0], b[1] - a[1]];
        let l = sqrt(d[0] * d[0] + d[1] * d[1]);
        if !(l > 0.0) || !l.is_finite() {
            return None;
        }
        Some(Line { at: a, dir: [d[0] / l, d[1] / l] })
    }

    /// The signed distance of `x` from the line, positive to its left.
    fn side(&self, x: [f64; 2]) -> f64 {
        self.dir[0] * (x[1] - self.at[1]) - self.dir[1] * (x[0] - self.at[0])
    }
}

/// The larger of two distances, NaN kept (a NaN anywhere refuses the collapse).
fn worst(a: f64, b: f64) -> f64 {
    if b > a || b.is_nan() { b } else { a }
}

/// `Q(p) = pᵀ A p + 2 bᵀ p + c` as `a00 a01 a02 a11 a12 a22 b0 b1 b2 c`.
#[derive(Clone, Copy, Debug)]
struct Quadric([f64; 10]);

impl Quadric {
    const ZERO: Quadric = Quadric([0.0; 10]);

    /// The squared distance to the plane `n · p + d = 0` (`n` unit), times `w`.
    fn plane(n: D3, d: f64, w: f64) -> Quadric {
        Quadric([
            w * n[0] * n[0],
            w * n[0] * n[1],
            w * n[0] * n[2],
            w * n[1] * n[1],
            w * n[1] * n[2],
            w * n[2] * n[2],
            w * n[0] * d,
            w * n[1] * d,
            w * n[2] * d,
            w * d * d,
        ])
    }

    fn add(self, o: Quadric) -> Quadric {
        let mut out = self.0;
        for (x, y) in out.iter_mut().zip(o.0) {
            *x += y;
        }
        Quadric(out)
    }

    fn eval(&self, p: D3) -> f64 {
        let q = &self.0;
        let ax = [
            q[0] * p[0] + q[1] * p[1] + q[2] * p[2],
            q[1] * p[0] + q[3] * p[1] + q[4] * p[2],
            q[2] * p[0] + q[4] * p[1] + q[5] * p[2],
        ];
        dot(p, ax) + 2.0 * (q[6] * p[0] + q[7] * p[1] + q[8] * p[2]) + q[9]
    }

    fn optimum(&self) -> Option<D3> {
        let q = &self.0;
        let a: M3 = [[q[0], q[1], q[2]], [q[1], q[3], q[4]], [q[2], q[4], q[5]]];
        let big = q[0].abs().max(q[3].abs()).max(q[5].abs());
        solve3(a, [-q[6], -q[7], -q[8]], 1e-10 * big * big * big)
    }
}

/// A candidate collapse in the queue: least cost first, ties by `tie` (zero unless planted)
/// then the edge's ids; stale when either end's version moved on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Cand {
    cost: u64,
    tie: u64,
    lo: u32,
    hi: u32,
    ver_lo: u32,
    ver_hi: u32,
}

/// A collapse decided: `gone` merges into `keep`, which moves to `p`.
#[derive(Clone, Copy, Debug)]
struct Plan {
    keep: u32,
    gone: u32,
    shared: u32,
    p: P3,
    q: Quadric,
    cost: f64,
}

/// Precomputed per-fan-triangle geometry for `vertical`, hoisted out of the `piece × fan` loop.
#[derive(Clone, Copy, Debug)]
struct FanData {
    owner: u32,
    q: [[f64; 2]; 3],
    bbox: [[f64; 2]; 2],
    sc: Line,
    sd: Line,
    link: Option<Line>,
    a2: f64,
    corners: [D3; 3],
}

/// One side of a vertex's star: a neighbour, the triangle, and whether that side is marked.
type Side = (u32, u32, bool);

struct Work<'s, 'v> {
    pos: V<'s, P3>,
    quad: V<'s, Quadric>,
    /// The map (module documentation, ERROR BOUND): pieces' corners, the pieces, and each
    /// triangle slot's pieces as `(first, count)` in `pieces` (`(0, 0)` once dead). Replaced
    /// pieces stay behind until [`compact`]; `live_corr` and `live_pieces` count the owned ones.
    corr: V<'s, Corr>,
    pieces: V<'s, Piece>,
    own: V<'s, (u32, u32)>,
    live_corr: u32,
    live_pieces: u32,
    /// `TAU` and `ALLOWANCE` times the input's largest coordinate, metres.
    tau: f64,
    allowance: f64,
    parent: V<'s, u32>,
    version: V<'s, u32>,
    head: V<'s, u32>,
    next: V<'s, u32>,
    tri: V<'s, [u32; 3]>,
    alive: V<'s, bool>,
    mark: V<'s, bool>,
    parts: &'v [PartId],
    roles: &'v [MaterialRole],
    live: u32,
    /// Per-vertex spatial feature-zone constraints (empty when `DecimateOpts::zones` is empty).
    has_zones: bool,
    zone_limit: V<'s, f64>,
    zone_min_cos_sq: V<'s, f64>,
    zone_weight: V<'s, f64>,
    zone_pinned: V<'s, bool>,
}

/// The candidate queue, with the counter only the planted tie-break reads.
struct Queue<'s, P> {
    heap: Heap4<Cand, Alloc<'s>>,
    pushed: u64,
    plant: P,
}

/// The scratch lists a step reuses.
struct Lists<'s> {
    star_a: V<'s, Side>,
    star_b: V<'s, Side>,
    ring_a: V<'s, u32>,
    ring_b: V<'s, u32>,
    triples: V<'s, [u32; 3]>,
    fixes: V<'s, (u32, u32, u32, bool)>,
    neighbours: V<'s, u32>,
    /// The collapse's region: least-first triples with their slots, sorted, once each.
    region: V<'s, ([u32; 3], u32)>,
    /// The fan: `[NONE (the new vertex), c, d]` per region triangle off the edge, with its slot.
    fan: V<'s, ([u32; 3], u32)>,
    /// The fan's spokes leaving and entering `p`, to see it closed.
    outs: V<'s, u32>,
    ins: V<'s, u32>,
    /// Unit normals of the region and the fan.
    normals: V<'s, D3>,
    /// The fan projected, one spoke line per link vertex (sorted by it), and precomputed per-fan data.
    fan2: V<'s, [[f64; 2]; 3]>,
    fan_data: V<'s, FanData>,
    spokes: V<'s, (u32, Line)>,
    /// A region triangle's pieces projected, corner by corner in piece order, and the fan
    /// triangles whose boxes meet theirs.
    proj: V<'s, Vtx>,
    near: V<'s, u32>,
    /// A run of fragments being merged: their corners in the owner's plane with their index in
    /// `new_corr`, and the hull's.
    pts: V<'s, ([f64; 2], u32)>,
    hull: V<'s, ([f64; 2], u32)>,
    /// The two buffers a projected piece is cut between.
    poly_a: V<'s, Vtx>,
    poly_b: V<'s, Vtx>,
    vals: V<'s, f64>,
    /// The collapse's new pieces before it is admitted: corners, and per piece its owner slot,
    /// the piece (its `first` an index into `new_corr`) and its error, plus compact sort order.
    new_corr: V<'s, Corr>,
    new_pieces: V<'s, (u32, Piece, f64)>,
    piece_order: V<'s, (u64, u32, u32)>,
}

/// What a vertex's star says about it and about one of its edges.
struct Facts {
    degree: u32,
    edge_tris: u32,
    edge_feature: bool,
}

impl Work<'_, '_> {
    fn live_corners(&self, s: u32, out: &mut V<'_, u32>) -> Result<(), AllocFailure> {
        out.clear();
        let mut c = self.head[at(s)];
        while c != NONE {
            if self.alive[at(c / 3)] {
                push(out, c)?;
            }
            c = self.next[at(c)];
        }
        Ok(())
    }

    fn star(&self, s: u32, out: &mut V<'_, Side>) -> Result<(), AllocFailure> {
        out.clear();
        let mut c = self.head[at(s)];
        while c != NONE {
            let t = c / 3;
            if self.alive[at(t)] {
                let k = c % 3;
                let tri = self.tri[at(t)];
                push(out, (tri[at((k + 1) % 3)], t, self.mark[at(t * 3 + k)]))?;
                push(out, (tri[at((k + 2) % 3)], t, self.mark[at(t * 3 + (k + 2) % 3)]))?;
            }
            c = self.next[at(c)];
        }
        out.sort_unstable();
        Ok(())
    }

    fn facts(&self, star: &[Side], other: u32) -> Facts {
        let mut facts = Facts { degree: 0, edge_tris: 0, edge_feature: false };
        for group in star.chunk_by(|a, b| a.0 == b.0) {
            let feature = group.len() != 2
                || group.iter().any(|side| side.2)
                || self.roles[at(group[0].1)] != self.roles[at(group[1].1)]
                || self.parts[at(group[0].1)] != self.parts[at(group[1].1)];
            if feature {
                facts.degree += 1;
            }
            if group[0].0 == other {
                facts.edge_tris = len32(group.len());
                facts.edge_feature = feature;
            }
        }
        facts
    }

    /// The side `{x, y}` of triangle `t`, as its corner number.
    fn side(&self, t: u32, x: u32, y: u32) -> Option<u32> {
        let tri = self.tri[at(t)];
        (0..3u32).find(|&k| {
            let (a, b) = (tri[at(k)], tri[at((k + 1) % 3)]);
            (a == x && b == y) || (a == y && b == x)
        })
        .map(|k| t * 3 + k)
    }

    fn contains(&self, t: u32, s: u32) -> bool {
        self.tri[at(t)].contains(&s)
    }

    #[inline]
    fn edge_limit(&self, a: u32, b: u32) -> f64 {
        if !self.has_zones {
            return f64::INFINITY;
        }
        self.zone_limit[at(a)].min(self.zone_limit[at(b)])
    }

    #[inline]
    fn edge_min_cos_sq(&self, a: u32, b: u32) -> f64 {
        if !self.has_zones {
            return 0.0;
        }
        self.zone_min_cos_sq[at(a)].max(self.zone_min_cos_sq[at(b)])
    }

    #[inline]
    fn edge_weight(&self, a: u32, b: u32) -> f64 {
        if !self.has_zones {
            return 1.0;
        }
        self.zone_weight[at(a)].max(self.zone_weight[at(b)])
    }

    #[inline]
    fn edge_pinned(&self, a: u32, b: u32) -> bool {
        self.has_zones && (self.zone_pinned[at(a)] || self.zone_pinned[at(b)])
    }
}

/// Mesh decimation by edge collapse. Named `decimate`, not `simplify`: blaze-cad-field owns
/// `simplify` (a tape specialised to a box), one name per concept.
pub fn decimate<'a>(source: Source<'_>, opts: DecimateOpts<'a, '_, '_>) -> Result<Decimated<'a>, DecimateError> {
    decimate_planted(source, opts, NoPlant)
}

/// [`decimate`] with a planted bug, for the tests' controls.
pub fn decimate_planted<'a>(
    source: Source<'_>,
    opts: DecimateOpts<'a, '_, '_>,
    plant: impl Planted<DecimatePlant>,
) -> Result<Decimated<'a>, DecimateError> {
    let (view, was_verified) = match source {
        Source::Open(v) => (v, false),
        Source::Verified(v) => (v.view(), true),
    };
    let (vertices, triangles) = (view.vertex_count(), view.triangle_count());
    if vertices > opts.limits.most_vertices {
        return Err(DecimateError::TooManyVertices { vertices, most: opts.limits.most_vertices });
    }
    if triangles > opts.limits.most_triangles {
        return Err(DecimateError::TooManyTriangles { triangles, most: opts.limits.most_triangles });
    }
    if view.tangents().is_some() {
        return Err(DecimateError::NotCarried { column: Column::Tangents });
    }
    if view.uvs().is_some() {
        return Err(DecimateError::NotCarried { column: Column::Uvs });
    }
    if view.colours().is_some() {
        return Err(DecimateError::NotCarried { column: Column::Colours });
    }
    match opts.target {
        Target::Error(e) if e < Length::ZERO => {
            return Err(DecimateError::BadErrorTarget { error: e });
        }
        Target::Budget { error, max_error, .. } => {
            if error < Length::ZERO {
                return Err(DecimateError::BadErrorTarget { error });
            }
            if let Some(ce) = max_error
                && (ce < Length::ZERO || ce < error)
            {
                return Err(DecimateError::BadErrorTarget { error: ce });
            }
        }
        _ => {}
    }
    for z in opts.zones {
        if let Some(me) = z.max_error
            && me < Length::ZERO
        {
            return Err(DecimateError::BadErrorTarget { error: me });
        }
    }
    let s = opts.scratch;
    let spots = spots(view.positions(), s)?;
    let n = spots.count();
    let spot = &spots.of_vertex;
    let mut tri: V<'_, [u32; 3]> = Vec::try_with_capacity_in(at(triangles), s)?;
    for (t, corners) in view.triangles().iter().enumerate() {
        let v = [spot[at(corners[0])], spot[at(corners[1])], spot[at(corners[2])]];
        if v[0] == v[1] || v[1] == v[2] || v[0] == v[2] {
            return Err(DecimateError::Degenerate { tri: TriId(len32(t)) });
        }
        push(&mut tri, v)?;
    }
    // A spot's position with its zeros made positive: which input vertex came first in a class
    // holding both `-0.0` and `0.0` is input order, so the bytes must not depend on it.
    let mut pos: V<'_, P3> = Vec::try_with_capacity_in(at(n), s)?;
    for &first in spots.first.iter() {
        push(&mut pos, plus_zeros(view.positions()[at(first)]))?;
    }
    let mut marked: V<'_, (u32, u32)> = Vec::try_with_capacity_in(view.creases().len(), s)?;
    for edge in view.creases() {
        let (a, b) = (spot[edge.lo().index()], spot[edge.hi().index()]);
        push(&mut marked, if a <= b { (a, b) } else { (b, a) })?;
    }
    marked.sort_unstable();
    let mut mark = filled(at(triangles) * 3, false, s)?;
    for (t, v) in tri.iter().enumerate() {
        for k in 0..3 {
            let (a, b) = (v[k], v[(k + 1) % 3]);
            mark[t * 3 + k] = marked.binary_search(&if a <= b { (a, b) } else { (b, a) }).is_ok();
        }
    }
    // Quadrics, summed in the canonical triangle order.
    let mut canon = counting(triangles, s)?;
    canon.sort_unstable_by(|&x, &y| {
        let kx = (least_first(tri[at(x)]), view.parts()[at(x)], view.roles()[at(x)]);
        let ky = (least_first(tri[at(y)]), view.parts()[at(y)], view.roles()[at(y)]);
        kx.cmp(&ky).then(x.cmp(&y))
    });
    let mut quad = filled(at(n), Quadric::ZERO, s)?;
    let mut unit_face = filled(at(triangles), [0.0f64; 3], s)?;
    for &t in canon.iter() {
        // From the least spot: `(b − a) × (c − a)` and `a · n` round differently from each corner,
        // and which corner the input listed first is input order.
        let v = least_first(tri[at(t)]);
        let (a, b, c) = (wide(pos[at(v[0])]), wide(pos[at(v[1])]), wide(pos[at(v[2])]));
        let nrm = tri_normal(a, b, c);
        let Some(u) = unit(nrm) else {
            return Err(DecimateError::Degenerate { tri: TriId(t) });
        };
        unit_face[at(t)] = u;
        let q = Quadric::plane(u, -dot(u, a), length(nrm) * 0.5);
        for &corner in &v {
            quad[at(corner)] = quad[at(corner)].add(q);
        }
    }
    // Spot `i` is spot `i`: halves over the working triangles' own numbering.
    let feature_halves = halves(&tri, &counting(n, s)?, s)?;
    for (start, end) in edge_runs(&feature_halves) {
        let h = feature_halves[start];
        let run = &feature_halves[start..end];
        let feature = run.len() != 2
            || marked.binary_search(&(h.lo, h.hi)).is_ok()
            || view.roles()[at(run[0].tri)] != view.roles()[at(run[1].tri)]
            || view.parts()[at(run[0].tri)] != view.parts()[at(run[1].tri)];
        if !feature {
            continue;
        }
        let (p0, p1) = (wide(pos[at(h.lo)]), wide(pos[at(h.hi)]));
        let e = sub(p1, p0);
        let mut qe = Quadric::ZERO;
        for side in run {
            if let Some(m) = unit(cross(e, unit_face[at(side.tri)])) {
                qe = qe.add(Quadric::plane(m, -dot(m, p0), PENALTY * dot(e, e)));
            }
        }
        quad[at(h.lo)] = quad[at(h.lo)].add(qe);
        quad[at(h.hi)] = quad[at(h.hi)].add(qe);
    }
    let mut head = filled(at(n), NONE, s)?;
    let mut next = filled(at(triangles) * 3, NONE, s)?;
    for c in (0..triangles * 3).rev() {
        let sp = tri[at(c / 3)][at(c % 3)];
        next[at(c)] = head[at(sp)];
        head[at(sp)] = c;
    }
    // The map starts as the identity: one piece per triangle, its corners least first. A piece's
    // source is its triangle's place in the canonical order, not its input number: pieces are
    // sorted by source (`place`), and that order must not hang on the input's.
    let mut rank = filled(at(triangles), 0u32, s)?;
    for (r, &t) in canon.iter().enumerate() {
        rank[at(t)] = len32(r);
    }
    let mut corr: V<'_, Corr> = Vec::try_with_capacity_in(at(triangles) * 3, s)?;
    let mut pieces: V<'_, Piece> = Vec::try_with_capacity_in(at(triangles), s)?;
    let mut own: V<'_, (u32, u32)> = Vec::try_with_capacity_in(at(triangles), s)?;
    for t in 0..triangles {
        push(&mut own, (t, 1))?;
        push(&mut pieces, Piece { first: t * 3, len: 3, squashed: false, base: 0.0, src: rank[at(t)] })?;
        for &v in &least_first(tri[at(t)]) {
            let q = wide(pos[at(v)]);
            push(&mut corr, Corr { cur: q, orig: q })?;
        }
    }
    let mut largest = 0.0f64;
    for q in pos.iter() {
        for &x in q {
            largest = largest.max(f64::from(x).abs());
        }
    }
    let has_zones = !opts.zones.is_empty();
    let (zone_limit, zone_min_cos_sq, zone_weight, zone_pinned) = if has_zones {
        let mut zl: V<'_, f64> = Vec::try_with_capacity_in(at(n), s)?;
        let mut zc: V<'_, f64> = Vec::try_with_capacity_in(at(n), s)?;
        let mut zw: V<'_, f64> = Vec::try_with_capacity_in(at(n), s)?;
        let mut zp: V<'_, bool> = Vec::try_with_capacity_in(at(n), s)?;
        for q in pos.iter() {
            let p3 = Vec3::new(f64::from(q[0]), f64::from(q[1]), f64::from(q[2]));
            let mut lim = f64::INFINITY;
            let mut mcs = 0.0f64;
            let mut wt = 1.0f64;
            let mut pin = false;
            for z in opts.zones {
                if z.region.contains(p3) {
                    if let Some(me) = z.max_error {
                        lim = lim.min(me.as_m());
                    }
                    if let Some(mc) = z.min_cos_sq {
                        mcs = mcs.max(mc.clamp(0.0, 0.9999));
                    }
                    if z.cost_weight.is_finite() && z.cost_weight > 1.0 {
                        wt = wt.max(z.cost_weight);
                    }
                    if z.protect == FeatureProtect::Pinned {
                        pin = true;
                    }
                }
            }
            push(&mut zl, lim)?;
            push(&mut zc, mcs)?;
            push(&mut zw, wt)?;
            push(&mut zp, pin)?;
        }
        (zl, zc, zw, zp)
    } else {
        (Vec::new_in(s), Vec::new_in(s), Vec::new_in(s), Vec::new_in(s))
    };
    let mut w = Work {
        pos,
        quad,
        corr,
        pieces,
        own,
        live_corr: triangles * 3,
        live_pieces: triangles,
        tau: TAU * largest,
        allowance: ALLOWANCE * largest,
        parent: counting(n, s)?,
        version: filled(at(n), 0u32, s)?,
        head,
        next,
        tri,
        alive: filled(at(triangles), true, s)?,
        mark,
        parts: view.parts(),
        roles: view.roles(),
        live: triangles,
        has_zones,
        zone_limit,
        zone_min_cos_sq,
        zone_weight,
        zone_pinned,
    };
    let mut lists = Lists {
        star_a: Vec::new_in(s),
        star_b: Vec::new_in(s),
        ring_a: Vec::new_in(s),
        ring_b: Vec::new_in(s),
        triples: Vec::new_in(s),
        fixes: Vec::new_in(s),
        neighbours: Vec::new_in(s),
        region: Vec::new_in(s),
        fan: Vec::new_in(s),
        outs: Vec::new_in(s),
        ins: Vec::new_in(s),
        normals: Vec::new_in(s),
        fan2: Vec::new_in(s),
        fan_data: Vec::new_in(s),
        spokes: Vec::new_in(s),
        proj: Vec::new_in(s),
        near: Vec::new_in(s),
        pts: Vec::new_in(s),
        hull: Vec::new_in(s),
        poly_a: Vec::new_in(s),
        poly_b: Vec::new_in(s),
        vals: Vec::new_in(s),
        new_corr: Vec::new_in(s),
        new_pieces: Vec::new_in(s),
        piece_order: Vec::new_in(s),
    };
    // The largest accepted collapse's error (module documentation, ERROR BOUND), metres.
    let mut error_bound = 0.0f64;
    let mut stats = DecimateStats::default();
    let mut over_ceiling: u64 = 0;
    // For `Target::Budget`, Phase 1 runs strictly under `error` so all flat/coplanar and low-error
    // collapses happen BEFORE any above-`error` budget collapse is ever considered; only if
    // `w.live > max_triangles` after Phase 1 empties does Phase 2 re-seed live edges under
    // `max_error` and collapse cheapest-first down to `max_triangles`.
    let mut budget_phase2 = false;
    let mut active_limit = match opts.target {
        Target::Error(e) => e.as_m() - w.allowance,
        Target::Triangles(_) => f64::INFINITY,
        Target::Budget { error, .. } => error.as_m() - w.allowance,
    };
    let mut queue = Queue { heap: Heap4::new_in(s), pushed: 0, plant };
    if queue.plant.has(DecimatePlant::TieByInsertion) {
        // Seeded in the input's triangle order: the order only a planted tie-break can see.
        for t in 0..triangles {
            for k in 0..3u32 {
                let (a, b) = (w.tri[at(t)][at(k)], w.tri[at(t)][at((k + 1) % 3)]);
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                queue.offer(&w, &mut lists, (lo, hi))?;
            }
        }
    } else {
        // Each distinct undirected edge once, in `(lo, hi)` order, reusing `star_a` across edges
        // sharing `lo`. With deterministic tie-breaking (`tie = 0`), duplicates in the heap are
        // strictly redundant (and would re-run refused maps twice).
        let mut last_lo = NONE;
        for (start, _) in edge_runs(&feature_halves) {
            let h = feature_halves[start];
            if h.lo != last_lo {
                w.star(h.lo, &mut lists.star_a)?;
                last_lo = h.lo;
            }
            w.star(h.hi, &mut lists.star_b)?;
            queue.offer_from_stars(&w, (h.lo, h.hi), &lists.star_a, &lists.star_b)?;
        }
    }
    let stop = loop {
        match opts.target {
            Target::Triangles(most) if w.live <= most => break Stop::Reached,
            Target::Budget { max_triangles, .. } if budget_phase2 && w.live <= max_triangles => break Stop::Reached,
            _ => {}
        }
        let Some(c) = queue.heap.pop() else {
            match opts.target {
                Target::Error(_) => break Stop::Reached,
                Target::Triangles(_) => break Stop::NoValidCollapse,
                Target::Budget { max_triangles, max_error, .. } => {
                    if w.live <= max_triangles {
                        break Stop::Reached;
                    }
                    if !budget_phase2 {
                        // Phase 1 finished above `max_triangles`: transition to Phase 2 and re-seed
                        // every live undirected edge once in deterministic `(lo, hi)` order.
                        budget_phase2 = true;
                        active_limit = max_error.map_or(f64::INFINITY, |ce| ce.as_m() - w.allowance);
                        for v in 0..n {
                            if w.parent[at(v)] != v || w.head[at(v)] == NONE {
                                continue;
                            }
                            w.star(v, &mut lists.star_a)?;
                            lists.neighbours.clear();
                            let mut last = NONE;
                            for side in lists.star_a.iter() {
                                if side.0 > v && side.0 != last {
                                    push(&mut lists.neighbours, side.0)?;
                                    last = side.0;
                                }
                            }
                            for ni in 0..lists.neighbours.len() {
                                let x = lists.neighbours[ni];
                                w.star(x, &mut lists.star_b)?;
                                queue.offer_from_stars(&w, (v, x), &lists.star_a, &lists.star_b)?;
                            }
                        }
                        continue;
                    }
                    break if over_ceiling > 0 {
                        Stop::ErrorCeiling
                    } else {
                        Stop::NoValidCollapse
                    };
                }
            }
        };
        if w.version[at(c.lo)] != c.ver_lo || w.version[at(c.hi)] != c.ver_hi {
            continue;
        }
        if w.parent[at(c.lo)] != c.lo || w.parent[at(c.hi)] != c.hi {
            continue;
        }
        stats.taken += 1;
        let Some(plan) = queue.evaluate(&w, &mut lists, (c.lo, c.hi))? else {
            continue;
        };
        // `evaluate` left both ends' stars in `star_a` and `star_b`, reused directly by `valid`.
        // In error-bounded simplification (and Phase 1 of `Target::Budget`), reject bevel-folding
        // collapses that twist a surviving triangle's normal past ~22.8° (`cos² = 0.85`), and
        // tighten further inside any marked `FeatureZone`. In aggressive triangle-budget reduction
        // (`Target::Triangles` / Phase 2), fall back to hemisphere non-flip (`dot > 0`) outside
        // feature zones while still honoring any zone's explicit `min_cos_sq`.
        let base_min_cos_sq: f64 = match opts.target {
            Target::Error(_) => 0.85,
            Target::Budget { .. } if !budget_phase2 => 0.85,
            _ => 0.0,
        };
        let min_cos_sq = base_min_cos_sq.max(w.edge_min_cos_sq(plan.keep, plan.gone));
        if !valid(&w, &mut lists, &plan, min_cos_sq)? {
            continue;
        }
        // `valid` left both ends' live corners in `ring_a` (`keep`) and `ring_b` (`gone`).
        region(&w, &mut lists)?;
        if lists.region.len() > MAX_REGION {
            stats.over_region += 1;
            continue;
        }
        let edge_zlim = w.edge_limit(plan.keep, plan.gone);
        let edge_pinned = w.edge_pinned(plan.keep, plan.gone);
        let zone_cap = if edge_zlim.is_finite() {
            match opts.target {
                Target::Error(_) => edge_zlim,
                Target::Budget { .. } if !budget_phase2 => edge_zlim,
                _ => {
                    if edge_pinned {
                        edge_zlim
                    } else {
                        edge_zlim * 2.0
                    }
                }
            }
        } else {
            f64::INFINITY
        };
        let eff_limit = if zone_cap.is_finite() { active_limit.min(zone_cap - w.allowance) } else { active_limit };
        let Some(step) = remap(&w, &mut lists, &plan, eff_limit, &mut stats, queue.plant)? else {
            continue;
        };
        let err = step + w.allowance;
        if zone_cap.is_finite() && !(err <= zone_cap) {
            stats.over_error += 1;
            if budget_phase2 {
                over_ceiling += 1;
            }
            continue;
        }
        match opts.target {
            Target::Error(e) if !(err <= e.as_m()) => {
                stats.over_error += 1;
                continue;
            }
            Target::Budget { error, max_error, .. } => {
                if !budget_phase2 {
                    if !(err <= error.as_m()) {
                        stats.over_error += 1;
                        continue;
                    }
                } else if let Some(ce) = max_error
                    && !(err <= ce.as_m())
                {
                    stats.over_error += 1;
                    over_ceiling += 1;
                    continue;
                }
            }
            _ => {}
        }
        collapse(&mut w, &mut lists, &plan)?;
        stats.collapses += 1;
        error_bound = error_bound.max(err);
        // A merge may err up to the target, or for a triangle target up to the bound so far: it
        // never raises that one. Inside a `FeatureZone`, also cap merges by the zone's limit.
        let merge_limit = match opts.target {
            Target::Error(_) => eff_limit,
            Target::Triangles(_) => (error_bound - w.allowance).min(eff_limit),
            Target::Budget { .. } => {
                if !budget_phase2 {
                    eff_limit
                } else {
                    (error_bound - w.allowance).min(eff_limit)
                }
            }
        };
        let freeze_ref = if eff_limit.is_finite() {
            eff_limit
        } else if active_limit.is_finite() {
            active_limit
        } else {
            error_bound.max(w.allowance * 1e4)
        };
        let merged = place(&mut w, &mut lists, merge_limit, freeze_ref, &mut stats, s)?;
        error_bound = error_bound.max(merged + w.allowance);
        w.star(plan.keep, &mut lists.star_a)?;
        lists.neighbours.clear();
        let mut last = NONE;
        for side in lists.star_a.iter() {
            if side.0 != last {
                push(&mut lists.neighbours, side.0)?;
                last = side.0;
            }
        }
        // Reuse `star_a` (`plan.keep`'s star) across all incident edges without recomputing it.
        for ni in 0..lists.neighbours.len() {
            let x = lists.neighbours[ni];
            w.star(x, &mut lists.star_b)?;
            if x < plan.keep {
                queue.offer_from_stars(&w, (x, plan.keep), &lists.star_b, &lists.star_a)?;
            } else {
                queue.offer_from_stars(&w, (plan.keep, x), &lists.star_a, &lists.star_b)?;
            }
        }
    };
    let shaped = output(&w, view.triangle_count(), s)?;
    let mesh = shade(&shaped.view(), ShadeOpts { cos_crease: opts.cos_crease, alloc: opts.alloc, scratch: s })
        .map_err(DecimateError::Shade)?;
    let proof = if was_verified {
        let verified = check(mesh.view(), CheckOpts { limits: opts.limits, scratch: s }).map_err(DecimateError::LostProof)?;
        Some(verified.proof())
    } else {
        None
    };
    Ok(Decimated { mesh, error_bound: length_up(error_bound), stop, stats, proof })
}

/// The live triangles as a position-only mesh, vertices in spot order, triangles in canonical
/// order, marks as creases.
fn output<'s>(w: &Work<'_, '_>, triangles: u32, s: Alloc<'s>) -> Result<Mesh<'s>, DecimateError> {
    let n = len32(w.pos.len());
    let mut new_index = filled(at(n), NONE, s)?;
    let mut out: MeshVecs<'s> = empty_vecs(s);
    for t in 0..triangles {
        if w.alive[at(t)] {
            for &v in &w.tri[at(t)] {
                new_index[at(v)] = 0;
            }
        }
    }
    for v in 0..n {
        if new_index[at(v)] == 0 {
            new_index[at(v)] = len32(out.positions.len());
            push(&mut out.positions, w.pos[at(v)])?;
            push(&mut out.normals, [0.0; 3])?;
        }
    }
    let mut keyed: V<'_, ([u32; 3], PartId, MaterialRole)> = Vec::new_in(s);
    for t in 0..triangles {
        if w.alive[at(t)] {
            let v = w.tri[at(t)];
            let mapped = [new_index[at(v[0])], new_index[at(v[1])], new_index[at(v[2])]];
            push(&mut keyed, (least_first(mapped), w.parts[at(t)], w.roles[at(t)]))?;
            for k in 0..3 {
                if w.mark[at(t * 3 + k)] {
                    push(&mut out.creases, VertexPair::new(VertexId(mapped[at(k)]), VertexId(mapped[at((k + 1) % 3)])))?;
                }
            }
        }
    }
    keyed.sort_unstable();
    for &(v, part, role) in keyed.iter() {
        push(&mut out.triangles, v)?;
        push(&mut out.parts, part)?;
        push(&mut out.roles, role)?;
    }
    out.creases.sort_unstable();
    out.creases.dedup();
    built(out).map_err(DecimateError::Built)
}

impl<P: Planted<DecimatePlant>> Queue<'_, P> {
    /// `edge`'s collapse queued at its cost, when the feature rule allows one.
    fn offer(&mut self, w: &Work<'_, '_>, lists: &mut Lists<'_>, edge: (u32, u32)) -> Result<(), AllocFailure> {
        let Some(plan) = self.evaluate(w, lists, edge)? else {
            return Ok(());
        };
        self.push_plan(w, edge, plan.cost)
    }

    /// `edge`'s collapse queued using precomputed sorted stars `(star_lo, star_hi)`.
    fn offer_from_stars(
        &mut self,
        w: &Work<'_, '_>,
        edge: (u32, u32),
        star_lo: &[Side],
        star_hi: &[Side],
    ) -> Result<(), AllocFailure> {
        let Some(plan) = evaluate_stars(w, edge, star_lo, star_hi, self.plant) else {
            return Ok(());
        };
        self.push_plan(w, edge, plan.cost)
    }

    fn push_plan(&mut self, w: &Work<'_, '_>, edge: (u32, u32), cost: f64) -> Result<(), AllocFailure> {
        let tie = if self.plant.has(DecimatePlant::TieByInsertion) { self.pushed } else { 0 };
        self.pushed += 1;
        let cand = Cand {
            cost: cost.to_bits(),
            tie,
            lo: edge.0,
            hi: edge.1,
            ver_lo: w.version[at(edge.0)],
            ver_hi: w.version[at(edge.1)],
        };
        self.heap.try_push(cand).map_err(|refused| refused.why)
    }

    fn evaluate(&self, w: &Work<'_, '_>, lists: &mut Lists<'_>, edge: (u32, u32)) -> Result<Option<Plan>, AllocFailure> {
        evaluate(w, lists, edge, self.plant)
    }
}

/// Where the collapse of `edge` would put the merged vertex and what it costs, or `None` when
/// the feature rule forbids it. Leaves both ends' sorted stars in `lists.star_a` and `lists.star_b`.
fn evaluate(
    w: &Work<'_, '_>,
    lists: &mut Lists<'_>,
    edge: (u32, u32),
    plant: impl Planted<DecimatePlant>,
) -> Result<Option<Plan>, AllocFailure> {
    let (a, b) = edge;
    w.star(a, &mut lists.star_a)?;
    w.star(b, &mut lists.star_b)?;
    Ok(evaluate_stars(w, edge, &lists.star_a, &lists.star_b, plant))
}

/// [`evaluate`] on precomputed sorted stars `(star_a, star_b)` of `edge = (a, b)`.
fn evaluate_stars(
    w: &Work<'_, '_>,
    edge: (u32, u32),
    star_a: &[Side],
    star_b: &[Side],
    plant: impl Planted<DecimatePlant>,
) -> Option<Plan> {
    let (a, b) = edge;
    let fa = w.facts(star_a, b);
    if fa.edge_tris == 0 {
        return None;
    }
    let fb = w.facts(star_b, a);
    let (da, db, feature) =
        if plant.has(DecimatePlant::CrossCrease) { (0, 0, false) } else { (fa.degree, fb.degree, fa.edge_feature) };
    enum Place {
        Optimum,
        AtA,
        AtB,
    }
    let place = match (da, db) {
        (0, 0) => Place::Optimum,
        (0, _) => Place::AtB,
        (_, 0) => Place::AtA,
        _ if !feature => return None,
        (2, 2) => Place::Optimum,
        (2, _) => Place::AtB,
        (_, 2) => Place::AtA,
        _ => return None,
    };
    let q = w.quad[at(a)].add(w.quad[at(b)]);
    let (pa, pb) = (wide(w.pos[at(a)]), wide(w.pos[at(b)]));
    let p: D3 = match place {
        Place::AtA => pa,
        Place::AtB => pb,
        Place::Optimum => match q.optimum() {
            Some(p) => p,
            None => {
                let mid = scale(add(pa, pb), 0.5);
                let mut best = pa;
                for cand in [pb, mid] {
                    if q.eval(cand) < q.eval(best) {
                        best = cand;
                    }
                }
                best
            }
        },
    };
    let p = narrow(p);
    if !p.iter().all(|x| x.is_finite()) {
        return None;
    }
    let cost = q.eval(wide(p));
    if cost.is_nan() {
        return None;
    }
    let mut cost = if cost > 0.0 { cost } else { 0.0 };
    if w.has_zones {
        let weight = w.edge_weight(a, b);
        if weight > 1.0 {
            // Bias critical/pinned edges behind equal-cost non-critical edges so flat faces
            // outside marked features collapse before marked keyholes, bevels, or mating surfaces.
            cost = (cost + 1e-18) * weight;
        }
    }
    let (keep, gone) = match place {
        Place::AtB => (b, a),
        _ => (a, b),
    };
    Some(Plan { keep, gone, shared: fa.edge_tris, p, q, cost })
}

/// The link condition, no duplicate triangle after, no normal flipped or folded past
/// `acos(sqrt(min_cos_sq))` (or 90° when `min_cos_sq == 0.0`), and no nearly collinear triangle.
/// Reuses the sorted stars `evaluate` left in `lists.star_a` and `lists.star_b`.
fn valid(w: &Work<'_, '_>, lists: &mut Lists<'_>, plan: &Plan, min_cos_sq: f64) -> Result<bool, AllocFailure> {
    let (a, b) = (plan.keep, plan.gone);
    let shared = plan.shared;
    // Distinct neighbours of each, the other end left out, counted in common.
    // Symmetric in `(a, b)`, so `star_a` / `star_b` from `evaluate` work in either order.
    let mut common = 0u32;
    let (mut i, mut j) = (0, 0);
    let (sa, sb) = (&lists.star_a, &lists.star_b);
    while i < sa.len() && j < sb.len() {
        let (x, y) = (sa[i].0, sb[j].0);
        if x < y {
            i += 1;
        } else if y < x {
            j += 1;
        } else {
            if x != a && x != b {
                common += 1;
            }
            while i < sa.len() && sa[i].0 == x {
                i += 1;
            }
            while j < sb.len() && sb[j].0 == x {
                j += 1;
            }
        }
    }
    if common != shared {
        return Ok(false);
    }
    w.live_corners(a, &mut lists.ring_a)?;
    w.live_corners(b, &mut lists.ring_b)?;
    lists.triples.clear();
    let p = wide(plan.p);
    for (ring, other) in [(&lists.ring_a, b), (&lists.ring_b, a)] {
        for &c in ring.iter() {
            let t = c / 3;
            if w.contains(t, other) {
                continue;
            }
            // From the least id, as the quadrics: the signs tested below must not hang on which
            // corner the input listed first.
            let v = least_first(w.tri[at(t)]);
            let mut moved = v;
            let mut corners = [wide(w.pos[at(v[0])]), wide(w.pos[at(v[1])]), wide(w.pos[at(v[2])])];
            let before = tri_normal(corners[0], corners[1], corners[2]);
            for k in 0..3 {
                if v[k] == a || v[k] == b {
                    corners[k] = p;
                    moved[k] = a;
                }
            }
            let ab = sub(corners[1], corners[0]);
            let ac = sub(corners[2], corners[0]);
            let bc = sub(corners[2], corners[1]);
            let after = cross(ab, ac);
            let area_sq = dot(after, after);
            let (ab_sq, ac_sq, bc_sq) = (dot(ab, ab), dot(ac, ac), dot(bc, bc));
            // Twice-area squared divided by the adjacent squared edge lengths is sin²(angle).
            // Check the worst corner. A positive f64 area alone admits slivers whose normal
            // loses most of its precision when the exported f32 corners are transformed.
            let max_edge_product = (ab_sq * ac_sq).max(bc_sq * ab_sq.max(ac_sq));
            if area_sq <= f64::from(f32::EPSILON) * max_edge_product {
                return Ok(false);
            }
            let d = dot(before, after);
            if !(d > 0.0) {
                return Ok(false);
            }
            if min_cos_sq > 0.0 && !(d * d >= min_cos_sq * dot(before, before) * area_sq) {
                return Ok(false);
            }
            if moved[0] > moved[1] {
                moved.swap(0, 1);
            }
            if moved[1] > moved[2] {
                moved.swap(1, 2);
            }
            if moved[0] > moved[1] {
                moved.swap(0, 1);
            }
            push(&mut lists.triples, moved)?;
        }
    }
    lists.triples.sort_unstable();
    // A triangle around both ends is in both rings: equal neighbours there are one triangle.
    let mut last: Option<[u32; 3]> = None;
    for &t in lists.triples.iter() {
        if last == Some(t) {
            // Only a triangle of `b`'s turned into one of `a`'s repeats a triple, since rings hold
            // each live triangle once and the shared ones were skipped.
            return Ok(false);
        }
        last = Some(t);
    }
    Ok(true)
}

/// The collapse's region from the rings `valid` left (both ends' live corners): each live
/// triangle around either end once, least-first with its slot, sorted (so every float below is
/// order-free: the triples of a manifold's live triangles are distinct).
fn region(w: &Work<'_, '_>, lists: &mut Lists<'_>) -> Result<(), AllocFailure> {
    lists.region.clear();
    for &c in lists.ring_a.iter().chain(lists.ring_b.iter()) {
        push(&mut lists.region, (least_first(w.tri[at(c / 3)]), c / 3))?;
    }
    lists.region.sort_unstable();
    lists.region.dedup();
    Ok(())
}

/// The map from region onto fan (module documentation, ERROR BOUND) composed onto the region's
/// pieces, left in `new_corr` and `new_pieces`, and the largest new piece error; `None` when no
/// map can be built (the collapse is refused). A map stops at the first piece erring more than
/// `limit`, returning that error (over the target, so refused). Needs [`region`] first.
fn remap(
    w: &Work<'_, '_>,
    lists: &mut Lists<'_>,
    plan: &Plan,
    limit: f64,
    stats: &mut DecimateStats,
    plant: impl Planted<DecimatePlant>,
) -> Result<Option<f64>, AllocFailure> {
    lists.new_corr.clear();
    lists.new_pieces.clear();
    // The corners a map reads: the region's pieces' (a map that stops early reads fewer; counted
    // whole, so the count is a bound).
    let mut corners = 0u64;
    let mut max_base = 0.0f64;
    for &(_, slot) in lists.region.iter() {
        let (first, count) = w.own[at(slot)];
        for pi in first..first + count {
            let pc = w.pieces[at(pi)];
            corners += u64::from(pc.len);
            max_base = worst(max_base, pc.base);
        }
    }
    if !(max_base <= limit) {
        return Ok(Some(max_base));
    }
    // Preserve material-role and part boundary curves within `limit`: a collapse that slides a
    // role-border corner along a flat plane has zero geometric Hausdorff error but drags the
    // material seam sideways across the face.
    if limit.is_finite() {
        let np = wide(plan.p);
        let mut border_shift = 0.0f64;
        for (v, star) in [(plan.keep, &lists.star_a), (plan.gone, &lists.star_b)] {
            let pv = wide(w.pos[at(v)]);
            let moved = sub(pv, np);
            if dot(moved, moved) == 0.0 {
                continue;
            }
            for group in star.chunk_by(|x, y| x.0 == y.0) {
                let nb = group[0].0;
                if nb == plan.keep || nb == plan.gone || group.len() != 2 {
                    continue;
                }
                let role_or_part = w.roles[at(group[0].1)] != w.roles[at(group[1].1)]
                    || w.parts[at(group[0].1)] != w.parts[at(group[1].1)];
                if !role_or_part {
                    continue;
                }
                let pn = wide(w.pos[at(nb)]);
                let seg = sub(pn, np);
                let seg_len = length(seg);
                let mut shift = if seg_len > 1e-12 {
                    length(cross(moved, seg)) / seg_len
                } else {
                    length(moved)
                };
                if dot(sub(pn, pv), seg) < 0.0 {
                    shift = shift.max(length(moved));
                }
                border_shift = worst(border_shift, shift);
            }
        }
        if !(border_shift <= limit) {
            return Ok(Some(border_shift));
        }
    }
    if plant.has(DecimatePlant::MeasuresEverything) {
        // Planted: the map looks through every live triangle's pieces, not the region's slots.
        for slot in 0..len32(w.own.len()) {
            if w.alive[at(slot)] {
                let (first, count) = w.own[at(slot)];
                for pi in first..first + count {
                    corners += u64::from(w.pieces[at(pi)].len);
                }
            }
        }
    }
    let mut err = None;
    if !plant.has(DecimatePlant::MovedOnly) {
        stats.maps += 1;
        stats.corners += corners;
        stats.most_corners = stats.most_corners.max(corners);
        err = vertical(w, lists, plan, limit)?;
    }
    if err.is_none() {
        lists.new_corr.clear();
        lists.new_pieces.clear();
        stats.maps += 1;
        stats.corners += corners;
        stats.most_corners = stats.most_corners.max(corners);
        err = barycentric(w, lists, plan, limit)?;
    }
    // A NaN anywhere (`worst` keeps it) is no map.
    Ok(err.filter(|e| *e >= 0.0))
}

/// Of the bisector of the two most divergent normals and their normalised sum, the direction
/// with the larger least cosine to every normal, when that is at least `MIN_COS`.
fn direction(normals: &[D3]) -> Option<D3> {
    let mut pair = (0, 0, 2.0f64);
    for i in 0..normals.len() {
        for j in i + 1..normals.len() {
            let d = dot(normals[i], normals[j]);
            if d < pair.2 {
                pair = (i, j, d);
            }
        }
    }
    let first = *normals.first()?;
    let mut sum = [0.0f64; 3];
    for &u in normals {
        sum = add(sum, u);
    }
    let bisector = add(normals.get(pair.0).copied().unwrap_or(first), normals.get(pair.1).copied().unwrap_or(first));
    let mut best: Option<(f64, D3)> = None;
    for cand in [unit(bisector), unit(sum)].into_iter().flatten() {
        let mut least = f64::INFINITY;
        for &u in normals {
            let d = dot(u, cand);
            if !(d >= least) {
                least = d;
            }
        }
        if best.is_none_or(|(b, _)| least > b) {
            best = Some((least, cand));
        }
    }
    match best {
        Some((least, n)) if least >= MIN_COS => Some(n),
        _ => None,
    }
}

/// `a × b` of two plane vectors.
fn cross2(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

fn minus2(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

/// Twice the signed area of a plane triangle, positive counter-clockwise.
fn area2(t: [[f64; 2]; 3]) -> f64 {
    cross2(minus2(t[1], t[0]), minus2(t[2], t[0]))
}

/// The spoke line of link vertex `v`.
fn spoke(spokes: &[(u32, Line)], v: u32) -> Option<Line> {
    spokes.binary_search_by_key(&v, |s| s.0).ok().map(|i| spokes[i].1)
}

enum CutResult {
    Empty,
    AllInside,
    Clipped { len: usize, on_line: bool },
}

#[inline(always)]
fn eval_cut<const SNAP: bool, const NEG: bool>(line: Line, tau: f64, x: [f64; 2]) -> f64 {
    let raw = line.side(x);
    let d = if NEG { -raw } else { raw };
    if !SNAP {
        d + tau
    } else if d.abs() <= tau {
        0.0
    } else {
        d
    }
}

#[inline(always)]
fn lerp_vtx(x: Vtx, y: Vtx, s: f64) -> Vtx {
    Vtx {
        at: [x.at[0] + s * (y.at[0] - x.at[0]), x.at[1] + s * (y.at[1] - x.at[1])],
        orig: add(x.orig, scale(sub(y.orig, x.orig), s)),
    }
}

/// Stack-backed Sutherland-Hodgman clip for convex polygons of up to 20 corners (zero heap writes).
#[inline(always)]
fn cut_line_stack<const SNAP: bool, const NEG: bool>(
    poly: &[Vtx],
    line: Line,
    tau: f64,
    out: &mut [Vtx; 24],
) -> CutResult {
    let m = poly.len();
    let mut vals = [0.0f64; 24];
    let mut any_nonneg = false;
    let mut all_pos = true;
    for i in 0..m {
        let val = eval_cut::<SNAP, NEG>(line, tau, poly[i].at);
        vals[i] = val;
        if val >= 0.0 {
            any_nonneg = true;
        }
        if !(val > 0.0) {
            all_pos = false;
        }
    }
    if !any_nonneg {
        return CutResult::Empty;
    }
    if all_pos {
        return CutResult::AllInside;
    }
    let mut n = 0usize;
    let mut on_line = true;
    let mut j = m - 1;
    for i in 0..m {
        let (dc, dp) = (vals[i], vals[j]);
        let (cur, prev) = (poly[i], poly[j]);
        if dc >= 0.0 {
            if dp < 0.0 && dc > 0.0 {
                out[n] = lerp_vtx(prev, cur, dp / (dp - dc));
                n += 1;
            }
            out[n] = cur;
            n += 1;
            if dc > 0.0 {
                on_line = false;
            }
        } else if dp > 0.0 {
            out[n] = lerp_vtx(prev, cur, dp / (dp - dc));
            n += 1;
        }
        j = i;
    }
    CutResult::Clipped { len: n, on_line }
}

/// Fallback heap-backed clip for polygons with more than 20 corners.
fn cut_line_vec<const SNAP: bool, const NEG: bool>(
    poly: &[Vtx],
    line: Line,
    tau: f64,
    vals: &mut V<'_, f64>,
    out: &mut V<'_, Vtx>,
) -> Result<CutResult, AllocFailure> {
    let m = poly.len();
    if m == 0 {
        return Ok(CutResult::Empty);
    }
    vals.clear();
    let mut any_nonneg = false;
    let mut all_pos = true;
    for v in poly {
        let val = eval_cut::<SNAP, NEG>(line, tau, v.at);
        if val >= 0.0 {
            any_nonneg = true;
        }
        if !(val > 0.0) {
            all_pos = false;
        }
        push(vals, val)?;
    }
    if !any_nonneg {
        return Ok(CutResult::Empty);
    }
    if all_pos {
        return Ok(CutResult::AllInside);
    }
    out.clear();
    let mut on_line = true;
    let mut j = m - 1;
    for i in 0..m {
        let (dc, dp) = (vals[i], vals[j]);
        let (cur, prev) = (poly[i], poly[j]);
        if dc >= 0.0 {
            if dp < 0.0 && dc > 0.0 {
                push(out, lerp_vtx(prev, cur, dp / (dp - dc)))?;
            }
            push(out, cur)?;
            if dc > 0.0 {
                on_line = false;
            }
        } else if dp > 0.0 {
            push(out, lerp_vtx(prev, cur, dp / (dp - dc)))?;
        }
        j = i;
    }
    Ok(CutResult::Clipped { len: out.len(), on_line })
}

/// The barycentric map (module documentation): `None` when an edge triangle's far corner is in
/// no fan triangle, or a region triangle has no area.
fn barycentric(w: &Work<'_, '_>, lists: &mut Lists<'_>, plan: &Plan, limit: f64) -> Result<Option<f64>, AllocFailure> {
    let (a, b) = (plan.keep, plan.gone);
    let p = wide(plan.p);
    let mut err = 0.0f64;
    for ri in 0..lists.region.len() {
        let (t, slot) = lists.region[ri];
        let edge = t.contains(&a) && t.contains(&b);
        let owner = if edge {
            let Some(&c) = t.iter().find(|&&x| x != a && x != b) else {
                return Ok(None);
            };
            let found = lists.region.iter().find(|&&(o, _)| o.contains(&c) && !(o.contains(&a) && o.contains(&b)));
            match found {
                Some(&(_, s)) => s,
                None => return Ok(None),
            }
        } else {
            slot
        };
        let x = [wide(w.pos[at(t[0])]), wide(w.pos[at(t[1])]), wide(w.pos[at(t[2])])];
        let mut y = x;
        for k in 0..3 {
            if t[k] == a || t[k] == b {
                y[k] = p;
            }
        }
        let nrm = tri_normal(x[0], x[1], x[2]);
        let nn = dot(nrm, nrm);
        if !(nn > 0.0) {
            return Ok(None);
        }
        let (first, count) = w.own[at(slot)];
        for pi in first..first + count {
            let piece = w.pieces[at(pi)];
            let start = len32(lists.new_corr.len());
            let mut most = 0.0f64;
            for k in piece.first..piece.first + piece.len {
                let c = w.corr[at(k)];
                let l0 = dot(tri_normal(c.cur, x[1], x[2]), nrm) / nn;
                let l1 = dot(tri_normal(x[0], c.cur, x[2]), nrm) / nn;
                let l2 = 1.0 - l0 - l1;
                let cur = add(add(scale(y[0], l0), scale(y[1], l1)), scale(y[2], l2));
                most = worst(most, distance(cur, c.orig));
                push(&mut lists.new_corr, Corr { cur, orig: c.orig })?;
            }
            let e = piece.base + most;
            err = worst(err, e);
            if !(err <= limit) {
                return Ok(Some(err));
            }
            let squashed = piece.squashed || edge;
            push(&mut lists.new_pieces, (owner, Piece { first: start, len: piece.len, squashed, base: piece.base, src: piece.src }, e))?;
        }
    }
    Ok(Some(err))
}

/// The vertical map (module documentation): `None` where its conditions do not hold.
fn vertical(w: &Work<'_, '_>, lists: &mut Lists<'_>, plan: &Plan, limit: f64) -> Result<Option<f64>, AllocFailure> {
    let (a, b) = (plan.keep, plan.gone);
    let p = wide(plan.p);
    let point = |v: u32| if v == NONE { p } else { wide(w.pos[at(v)]) };
    lists.fan.clear();
    lists.outs.clear();
    lists.ins.clear();
    for &(t, slot) in lists.region.iter() {
        let (on_a, on_b) = (t.contains(&a), t.contains(&b));
        if on_a && on_b {
            continue;
        }
        let Some(k) = (0..3).find(|&k| t[k] == a || t[k] == b) else {
            return Ok(None);
        };
        let f = [NONE, t[(k + 1) % 3], t[(k + 2) % 3]];
        push(&mut lists.fan, (f, slot))?;
        push(&mut lists.outs, f[1])?;
        push(&mut lists.ins, f[2])?;
    }
    // Closed around `p`: every link vertex once as a spoke out and once in.
    lists.outs.sort_unstable();
    lists.ins.sort_unstable();
    if lists.fan.is_empty() || lists.outs[..] != lists.ins[..] || lists.outs.windows(2).any(|x| x[0] == x[1]) {
        return Ok(None);
    }
    lists.normals.clear();
    for t in lists.region.iter().map(|r| r.0).chain(lists.fan.iter().map(|f| f.0)) {
        let Some(u) = unit(tri_normal(point(t[0]), point(t[1]), point(t[2]))) else {
            return Ok(None);
        };
        push(&mut lists.normals, u)?;
    }
    let Some(n) = direction(&lists.normals) else {
        return Ok(None);
    };
    let axis = if n[0].abs() <= n[1].abs() && n[0].abs() <= n[2].abs() {
        [1.0, 0.0, 0.0]
    } else if n[1].abs() <= n[2].abs() {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let Some(u) = unit(cross(n, axis)) else {
        return Ok(None);
    };
    let v = cross(n, u);
    let o = wide(w.pos[at(a)]);
    let proj = |q: D3| -> [f64; 2] {
        let d = sub(q, o);
        [dot(d, u), dot(d, v)]
    };
    // Every triangle a graph over the plane, before and after.
    for &(t, _) in lists.region.iter() {
        if !(area2([proj(point(t[0])), proj(point(t[1])), proj(point(t[2]))]) > 0.0) {
            return Ok(None);
        }
    }
    lists.fan2.clear();
    for &(f, _) in lists.fan.iter() {
        let q = [proj(point(f[0])), proj(point(f[1])), proj(point(f[2]))];
        if !(area2(q) > 0.0) {
            return Ok(None);
        }
        push(&mut lists.fan2, q)?;
    }
    let pp = proj(p);
    // The fan winds once about `p`: its sectors, half-open, cross the ray along `+u` once.
    let crossings = lists.fan2.iter().filter(|t| t[1][1] - pp[1] <= 0.0 && t[2][1] - pp[1] > 0.0).count();
    if crossings != 1 {
        return Ok(None);
    }
    // One line per spoke, so the two fan triangles on it see exactly complementary sides.
    lists.spokes.clear();
    for i in 0..lists.fan.len() {
        let Some(line) = Line::through(pp, lists.fan2[i][1]) else {
            return Ok(None);
        };
        push(&mut lists.spokes, (lists.fan[i].0[1], line))?;
    }
    lists.spokes.sort_unstable_by_key(|s| s.0);
    // Precompute all per-fan-triangle lines, boxes, areas and 3D corners once outside the inner loop.
    lists.fan_data.clear();
    for fi in 0..lists.fan.len() {
        let (f, owner) = lists.fan[fi];
        let q = lists.fan2[fi];
        let (Some(sc), Some(sd)) = (spoke(&lists.spokes, f[1]), spoke(&lists.spokes, f[2])) else {
            return Ok(None);
        };
        let mut bbox = [q[0], q[0]];
        for x in &q[1..] {
            for i in 0..2 {
                bbox[0][i] = bbox[0][i].min(x[i]);
                bbox[1][i] = bbox[1][i].max(x[i]);
            }
        }
        let link = Line::through(q[1], q[2]);
        let a2 = area2(q);
        let corners = [p, point(f[1]), point(f[2])];
        push(&mut lists.fan_data, FanData { owner, q, bbox, sc, sd, link, a2, corners })?;
    }
    let tau = w.tau;
    // Whether the boxes `lo..hi` and `b` are apart by more than `tau` along an axis.
    let apart = |lo: [f64; 2], hi: [f64; 2], b: [[f64; 2]; 2]| {
        hi[0] < b[0][0] - tau || lo[0] > b[1][0] + tau || hi[1] < b[0][1] - tau || lo[1] > b[1][1] + tau
    };
    let mut err = 0.0f64;
    let mut stack_a = [Vtx { at: [0.0; 2], orig: [0.0; 3] }; 24];
    let mut stack_b = [Vtx { at: [0.0; 2], orig: [0.0; 3] }; 24];
    for ri in 0..lists.region.len() {
        let (first, count) = w.own[at(lists.region[ri].1)];
        // The triangle's pieces projected, and the fan triangles near any of them: a piece's
        // box is within its triangle's, so the others are apart from every piece.
        lists.proj.clear();
        let (mut tlo, mut thi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for pi in first..first + count {
            let piece = w.pieces[at(pi)];
            for k in piece.first..piece.first + piece.len {
                let c = w.corr[at(k)];
                let x = proj(c.cur);
                tlo[0] = tlo[0].min(x[0]);
                thi[0] = thi[0].max(x[0]);
                tlo[1] = tlo[1].min(x[1]);
                thi[1] = thi[1].max(x[1]);
                push(&mut lists.proj, Vtx { at: x, orig: c.orig })?;
            }
        }
        lists.near.clear();
        for fi in 0..lists.fan_data.len() {
            if !apart(tlo, thi, lists.fan_data[fi].bbox) {
                push(&mut lists.near, len32(fi))?;
            }
        }
        let mut next = 0usize;
        for pi in first..first + count {
            let piece = w.pieces[at(pi)];
            let p_start = next;
            let p_end = next + at(piece.len);
            next = p_end;
            let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
            for &x in &lists.proj[p_start..p_end] {
                lo[0] = lo[0].min(x.at[0]);
                hi[0] = hi[0].max(x.at[0]);
                lo[1] = lo[1].min(x.at[1]);
                hi[1] = hi[1].max(x.at[1]);
            }
            for ni in 0..lists.near.len() {
                let fd = lists.fan_data[at(lists.near[ni])];
                if apart(lo, hi, fd.bbox) {
                    continue;
                }
                let Some(link) = fd.link else {
                    return Ok(None);
                };
                // Inside spoke `c` (its left), inside spoke `d` (its right), each snapped to the
                // line within `tau`; inside the link edge (its left) or within `tau` outside it.
                let mut flat = false;
                let mut use_stack = p_end - p_start <= 11;
                let mut cur_buf: u8 = 0; // 0 = proj, 1 = buf_a, 2 = buf_b
                let mut cur_len = p_end - p_start;
                let mut empty = false;

                macro_rules! do_cut {
                    ($snap:expr, $neg:expr, $line:expr) => {
                        if !empty {
                            if use_stack && cur_len <= 11 {
                                let (res, next_buf) = match cur_buf {
                                    0 => (cut_line_stack::<$snap, $neg>(&lists.proj[p_start..p_end], $line, tau, &mut stack_a), 1u8),
                                    1 => (cut_line_stack::<$snap, $neg>(&stack_a[..cur_len], $line, tau, &mut stack_b), 2u8),
                                    _ => (cut_line_stack::<$snap, $neg>(&stack_b[..cur_len], $line, tau, &mut stack_a), 1u8),
                                };
                                match res {
                                    CutResult::Empty => empty = true,
                                    CutResult::AllInside => {}
                                    CutResult::Clipped { len, on_line } => {
                                        cur_buf = next_buf;
                                        cur_len = len;
                                        flat |= on_line;
                                    }
                                }
                            } else {
                                if use_stack {
                                    // Spill current stack buffer into `poly_a` / `poly_b` before falling back.
                                    if cur_buf == 1 {
                                        lists.poly_a.clear();
                                        extend(&mut lists.poly_a, &stack_a[..cur_len])?;
                                    } else if cur_buf == 2 {
                                        lists.poly_b.clear();
                                        extend(&mut lists.poly_b, &stack_b[..cur_len])?;
                                    }
                                    use_stack = false;
                                }
                                let (res, next_buf) = match cur_buf {
                                    0 => (cut_line_vec::<$snap, $neg>(&lists.proj[p_start..p_end], $line, tau, &mut lists.vals, &mut lists.poly_a)?, 1u8),
                                    1 => (cut_line_vec::<$snap, $neg>(&lists.poly_a, $line, tau, &mut lists.vals, &mut lists.poly_b)?, 2u8),
                                    _ => (cut_line_vec::<$snap, $neg>(&lists.poly_b, $line, tau, &mut lists.vals, &mut lists.poly_a)?, 1u8),
                                };
                                match res {
                                    CutResult::Empty => empty = true,
                                    CutResult::AllInside => {}
                                    CutResult::Clipped { len, on_line } => {
                                        cur_buf = next_buf;
                                        cur_len = len;
                                        flat |= on_line;
                                    }
                                }
                            }
                        }
                    };
                }

                do_cut!(true, false, fd.sc);
                do_cut!(true, true, fd.sd);
                do_cut!(false, false, link);

                if empty {
                    continue;
                }
                let clipped: &[Vtx] = if use_stack {
                    match cur_buf {
                        0 => &lists.proj[p_start..p_end],
                        1 => &stack_a[..cur_len],
                        _ => &stack_b[..cur_len],
                    }
                } else {
                    match cur_buf {
                        0 => &lists.proj[p_start..p_end],
                        1 => &lists.poly_a,
                        _ => &lists.poly_b,
                    }
                };
                // A cut with no area is the neighbour's (its closure holds it), unless the piece
                // is squashed: it never had area, and every part of it must stay claimed.
                if clipped.is_empty() || (!piece.squashed && (clipped.len() < 3 || flat)) {
                    continue;
                }
                let q = fd.q;
                let a2 = fd.a2;
                let corners = fd.corners;
                let start = len32(lists.new_corr.len());
                lists.new_corr.try_reserve(clipped.len())?;
                let mut most = 0.0f64;
                for &x in clipped {
                    let l0 = cross2(minus2(q[1], x.at), minus2(q[2], x.at)) / a2;
                    let l1 = cross2(minus2(q[2], x.at), minus2(q[0], x.at)) / a2;
                    let l2 = 1.0 - l0 - l1;
                    let cur = add(add(scale(corners[0], l0), scale(corners[1], l1)), scale(corners[2], l2));
                    let d = distance(cur, x.orig);
                    most = worst(most, d);
                    let e_so_far = piece.base + most;
                    if !(e_so_far <= limit) {
                        return Ok(Some(worst(err, e_so_far)));
                    }
                    lists.new_corr.push(Corr { cur, orig: x.orig });
                }
                let e = piece.base + most;
                err = worst(err, e);
                if !(err <= limit) {
                    return Ok(Some(err));
                }
                let len = len32(clipped.len());
                let cut_piece = Piece { first: start, len, squashed: piece.squashed, base: piece.base, src: piece.src };
                push(&mut lists.new_pieces, (fd.owner, cut_piece, e))?;
            }
        }
    }
    Ok(Some(err))
}

/// Orthonormal frame on an owner triangle, computed once per owner slot in `place`.
#[derive(Clone, Copy)]
struct OwnerFrame {
    o: D3,
    u: D3,
    vv: D3,
}

impl OwnerFrame {
    fn of(w: &Work<'_, '_>, slot: u32) -> Option<OwnerFrame> {
        // The owner's plane, from its least vertex (as every per-triangle float).
        let v = least_first(w.tri[at(slot)]);
        let (o, b, c) = (wide(w.pos[at(v[0])]), wide(w.pos[at(v[1])]), wide(w.pos[at(v[2])]));
        let edge = sub(b, o);
        let (u, n) = (unit(edge)?, unit(tri_normal(o, b, c))?);
        let vv = unit(cross(n, u))?;
        Some(OwnerFrame { o, u, vv })
    }
}

/// The admitted collapse's new pieces given to their fan triangles (each region slot's old ones
/// left behind): sorted by owner and source, each run of one source's fragments merged into one
/// piece where the merge errs at most `limit` (module documentation, MERGING), a triangle still
/// owning more than `FREEZE` frozen into one, and the leftovers compacted once they outnumber the
/// owned. Runs after [`collapse`], on its positions. Returns the largest kept merge's error, zero
/// when none was kept.
fn place<'s>(
    w: &mut Work<'s, '_>,
    lists: &mut Lists<'_>,
    limit: f64,
    freeze_ref: f64,
    stats: &mut DecimateStats,
    s: Alloc<'s>,
) -> Result<f64, AllocFailure> {
    for &(_, slot) in lists.region.iter() {
        let (first, count) = w.own[at(slot)];
        for pi in first..first + count {
            w.live_corr -= w.pieces[at(pi)].len;
        }
        w.live_pieces -= count;
        w.own[at(slot)] = (0, 0);
    }
    // Sort compact 16-byte `(owner_src, first, idx)` keys instead of swapping 40-byte entries.
    // A new piece's `first` is its own place in `new_corr`, so the order is total and identical.
    lists.piece_order.clear();
    lists.piece_order.try_reserve(lists.new_pieces.len())?;
    for (idx, &(owner, piece, _)) in lists.new_pieces.iter().enumerate() {
        let key = (u64::from(owner) << 32) | u64::from(piece.src);
        lists.piece_order.push((key, piece.first, len32(idx)));
    }
    lists.piece_order.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut merged_most = 0.0f64;
    let mut i = 0;
    let total = lists.piece_order.len();
    while i < total {
        // Every owner is a fan triangle, alive after the collapse.
        let slot = (lists.piece_order[i].0 >> 32) as u32;
        let mut end = i;
        while end < total && (lists.piece_order[end].0 >> 32) as u32 == slot {
            end += 1;
        }
        let frame = OwnerFrame::of(w, slot);
        let (first, corr_start) = (len32(w.pieces.len()), w.corr.len());
        let (mut count, mut corners, mut most) = (0u32, 0u32, 0.0f64);
        let mut j = i;
        while j < end {
            let key = lists.piece_order[j].0;
            let src = key as u32;
            let mut k = j;
            while k < end && lists.piece_order[k].0 == key {
                k += 1;
            }
            let run = &lists.piece_order[j..k];
            let mergeable = run.len() > 1 && src != NONE;
            let kept = if mergeable {
                match frame {
                    Some(fr) => merge(w, fr, run, &lists.new_pieces, &lists.new_corr, &mut lists.pts, &mut lists.hull, limit)?,
                    None => None,
                }
            } else {
                None
            };
            if let Some((len, e)) = kept {
                stats.merged += 1;
                count += 1;
                corners += len;
                most = worst(most, e);
                merged_most = worst(merged_most, e);
            } else {
                if mergeable {
                    stats.not_merged += 1;
                }
                for &(_, _, idx) in run {
                    let (_, piece, e) = lists.new_pieces[at(idx)];
                    let start = len32(w.corr.len());
                    extend(&mut w.corr, &lists.new_corr[at(piece.first)..at(piece.first + piece.len)])?;
                    push(&mut w.pieces, Piece { first: start, ..piece })?;
                    count += 1;
                    corners += piece.len;
                    most = worst(most, e);
                }
            }
            j = k;
        }
        // Adaptive slack-gated freezing (sound by the triangle inequality):
        // - Near-flat / beveled faces carrying `<= 5%` of `freeze_ref` freeze at 12 pieces so
        //   large architectural walls, frames, and cleats stay O(1) pieces per triangle even under
        //   tight sub-millimetre tolerances or Phase-2 budget reduction.
        // - Gently curved faces carrying `<= 25%` of `freeze_ref` freeze at 48 pieces, retaining
        //   >= 75% headroom for curved cylinders/rounds while eliminating piece-list blowups.
        // - High-error curved triangles keep full per-source merging up to `FREEZE`.
        let low_tol = (w.allowance * 1000.0).max(if freeze_ref.is_finite() { freeze_ref * 0.05 } else { 0.0 });
        let mid_tol = if freeze_ref.is_finite() { freeze_ref * 0.25 } else { 0.0 };
        let flat_freeze = (count >= 12 && most <= low_tol) || (count >= 48 && mid_tol > 0.0 && most <= mid_tol);
        if count > FREEZE || flat_freeze {
            w.pieces.truncate(at(first));
            w.corr.truncate(corr_start);
            let start = len32(w.corr.len());
            for &v in &least_first(w.tri[at(slot)]) {
                let q = wide(w.pos[at(v)]);
                push(&mut w.corr, Corr { cur: q, orig: q })?;
            }
            push(&mut w.pieces, Piece { first: start, len: 3, squashed: false, base: most, src: NONE })?;
            stats.frozen += 1;
            (count, corners) = (1, 3);
        }
        w.own[at(slot)] = (first, count);
        w.live_pieces += count;
        w.live_corr += corners;
        i = end;
    }
    if w.corr.len() > 2 * at(w.live_corr) + 4096 || w.pieces.len() > 2 * at(w.live_pieces) + 1024 {
        compact(w, s)?;
    }
    Ok(merged_most)
}

/// One source's fragments in an owner triangle (a run of `piece_order` indices into `new_pieces`,
/// corners in `new_corr`) pushed as one piece (module documentation, MERGING), with its corner
/// count and error; `None`, nothing pushed, when the hull has no area, the fit fails, or the
/// merge errs more than `limit`.
#[allow(clippy::too_many_arguments)]
fn merge(
    w: &mut Work<'_, '_>,
    frame: OwnerFrame,
    run: &[(u64, u32, u32)],
    new_pieces: &[(u32, Piece, f64)],
    new_corr: &[Corr],
    pts: &mut V<'_, ([f64; 2], u32)>,
    hull: &mut V<'_, ([f64; 2], u32)>,
    limit: f64,
) -> Result<Option<(u32, f64)>, AllocFailure> {
    let OwnerFrame { o, u, vv } = frame;
    // Corners in the plane, about their mean (so the normal equations are centred).
    pts.clear();
    let mut mean = [0.0f64; 2];
    for &(_, _, idx) in run {
        let piece = new_pieces[at(idx)].1;
        for k in piece.first..piece.first + piece.len {
            let d = sub(new_corr[at(k)].cur, o);
            let x = [dot(d, u), dot(d, vv)];
            mean = [mean[0] + x[0], mean[1] + x[1]];
            push(pts, (x, k))?;
        }
    }
    let m = f64::from(len32(pts.len()));
    mean = [mean[0] / m, mean[1] / m];
    for p in pts.iter_mut() {
        p.0 = minus2(p.0, mean);
    }
    // The least-squares affine fit `orig ≈ o0 + A [1, x, y]`, `o0` the first corner's `orig`.
    let o0 = new_corr[at(pts[0].1)].orig;
    let mut normal: M3 = [[0.0; 3]; 3];
    let mut rhs = [[0.0f64; 3]; 3];
    for &(x, k) in pts.iter() {
        let (x0, x1) = (x[0], x[1]);
        let d = sub(new_corr[at(k)].orig, o0);
        normal[0][0] += 1.0;
        normal[0][1] += x0;
        normal[0][2] += x1;
        normal[1][0] += x0;
        normal[1][1] += x0 * x0;
        normal[1][2] += x0 * x1;
        normal[2][0] += x1;
        normal[2][1] += x1 * x0;
        normal[2][2] += x1 * x1;
        for (axis, r) in rhs.iter_mut().enumerate() {
            let da = d[axis];
            r[0] += da;
            r[1] += x0 * da;
            r[2] += x1 * da;
        }
    }
    // Cramer's rule with shared determinant & `cross(c1, c2)`, bit-equal to 3 calls to `solve3`.
    let det = dot(normal[0], cross(normal[1], normal[2]));
    if !(det.abs() > 0.0) || !det.is_finite() {
        return Ok(None);
    }
    let c0 = [normal[0][0], normal[1][0], normal[2][0]];
    let c1 = [normal[0][1], normal[1][1], normal[2][1]];
    let c2 = [normal[0][2], normal[1][2], normal[2][2]];
    let c1xc2 = cross(c1, c2);
    let mut coef = [[0.0f64; 3]; 3];
    for axis in 0..3 {
        let b = rhs[axis];
        let sol = [dot(b, c1xc2) / det, dot(c0, cross(b, c2)) / det, dot(c0, cross(c1, b)) / det];
        if !sol.iter().all(|v| v.is_finite()) {
            return Ok(None);
        }
        coef[axis] = sol;
    }
    let fit = |x: [f64; 2]| -> D3 {
        [
            o0[0] + (coef[0][0] + coef[0][1] * x[0] + coef[0][2] * x[1]),
            o0[1] + (coef[1][0] + coef[1][1] * x[0] + coef[1][2] * x[1]),
            o0[2] + (coef[2][0] + coef[2][1] * x[0] + coef[2][2] * x[1]),
        ]
    };
    // `base`: the old error before each corner's `orig`, plus how far the fit moved it.
    let mut base = 0.0f64;
    let mut at_corner = 0;
    for &(_, _, idx) in run {
        let piece = new_pieces[at(idx)].1;
        for _ in 0..piece.len {
            let (x, k) = pts[at_corner];
            at_corner += 1;
            base = worst(base, piece.base + distance(new_corr[at(k)].orig, fit(x)));
        }
    }
    // The hull (A. M. Andrew's monotone chain), counter-clockwise, no corner on a side's line.
    // Fast finite `<`/`>` pre-check before `total_cmp`: bit-identical total order.
    pts.sort_unstable_by(|p, q| {
        let c0 = if p.0[0] < q.0[0] {
            core::cmp::Ordering::Less
        } else if p.0[0] > q.0[0] {
            core::cmp::Ordering::Greater
        } else {
            p.0[0].total_cmp(&q.0[0])
        };
        c0.then_with(|| {
            if p.0[1] < q.0[1] {
                core::cmp::Ordering::Less
            } else if p.0[1] > q.0[1] {
                core::cmp::Ordering::Greater
            } else {
                p.0[1].total_cmp(&q.0[1])
            }
        })
        .then_with(|| p.1.cmp(&q.1))
    });
    hull.clear();
    for pass in 0..2 {
        let floor = hull.len();
        for i in 0..pts.len() {
            let p = if pass == 0 { pts[i] } else { pts[pts.len() - 1 - i] };
            while hull.len() >= floor + 2 {
                let (h0, h1) = (hull[hull.len() - 2].0, hull[hull.len() - 1].0);
                if cross2(minus2(h1, h0), minus2(p.0, h0)) > 0.0 {
                    break;
                }
                hull.pop();
            }
            push(hull, p)?;
        }
        // Each chain's last corner is the other's first.
        hull.pop();
    }
    if hull.len() < 3 {
        return Ok(None);
    }
    let mut fitted = [[0.0f64; 3]; 16];
    let use_fitted = hull.len() <= 16;
    let mut err = 0.0f64;
    for (hi, &(x, k)) in hull.iter().enumerate() {
        let fx = fit(x);
        if use_fitted {
            fitted[hi] = fx;
        }
        err = worst(err, base + distance(new_corr[at(k)].cur, fx));
    }
    if !(err <= limit) {
        return Ok(None);
    }
    let start = len32(w.corr.len());
    w.corr.try_reserve(hull.len())?;
    for (hi, &(x, k)) in hull.iter().enumerate() {
        let orig = if use_fitted { fitted[hi] } else { fit(x) };
        w.corr.push(Corr { cur: new_corr[at(k)].cur, orig });
    }
    let len = len32(hull.len());
    let src = new_pieces[at(run[0].2)].1.src;
    push(&mut w.pieces, Piece { first: start, len, squashed: false, base, src })?;
    Ok(Some((len, err)))
}

/// The owned pieces copied out, slot by slot, the leftovers dropped.
fn compact<'s>(w: &mut Work<'s, '_>, s: Alloc<'s>) -> Result<(), AllocFailure> {
    let mut corr: V<'s, Corr> = Vec::try_with_capacity_in(at(w.live_corr), s)?;
    let mut pieces: V<'s, Piece> = Vec::try_with_capacity_in(at(w.live_pieces), s)?;
    for slot in 0..w.own.len() {
        let (first, count) = w.own[slot];
        let start = len32(pieces.len());
        for pi in first..first + count {
            let piece = w.pieces[at(pi)];
            let c0 = len32(corr.len());
            extend(&mut corr, &w.corr[at(piece.first)..at(piece.first + piece.len)])?;
            push(&mut pieces, Piece { first: c0, ..piece })?;
        }
        w.own[slot] = (start, count);
    }
    w.corr = corr;
    w.pieces = pieces;
    Ok(())
}

/// Reuses `lists.ring_a` (`keep`'s live corners) and `lists.ring_b` (`gone`'s live corners)
/// left intact by `valid`, and relinks surviving corners without re-walking the vertex rings.
fn collapse(w: &mut Work<'_, '_>, lists: &mut Lists<'_>, plan: &Plan) -> Result<(), AllocFailure> {
    let (keep, gone) = (plan.keep, plan.gone);
    lists.fixes.clear();
    for &c in lists.ring_b.iter() {
        let t = c / 3;
        if !w.contains(t, keep) {
            continue;
        }
        let v = w.tri[at(t)];
        let third = v.iter().copied().find(|&x| x != keep && x != gone).unwrap_or(NONE);
        let across = |ring: &V<'_, u32>, end: u32| -> Option<u32> {
            ring.iter().map(|&c| c / 3).find(|&o| o != t && w.contains(o, end) && w.contains(o, third) && !w.contains(o, if end == gone { keep } else { gone }))
        };
        let n_gone = across(&lists.ring_b, gone);
        let n_keep = across(&lists.ring_a, keep);
        let mut m = w.side(t, gone, third).is_some_and(|c| w.mark[at(c)]) || w.side(t, keep, third).is_some_and(|c| w.mark[at(c)]);
        if let Some(o) = n_gone {
            m |= w.side(o, gone, third).is_some_and(|c| w.mark[at(c)]);
        }
        if let Some(o) = n_keep {
            m |= w.side(o, keep, third).is_some_and(|c| w.mark[at(c)]);
        }
        push(&mut lists.fixes, (n_gone.unwrap_or(NONE), n_keep.unwrap_or(NONE), third, m))?;
    }
    for &c in lists.ring_b.iter() {
        let t = c / 3;
        if w.contains(t, keep) {
            w.alive[at(t)] = false;
            w.live -= 1;
        }
    }
    for &c in lists.ring_b.iter() {
        let t = c / 3;
        if w.alive[at(t)] {
            w.tri[at(t)][at(c % 3)] = keep;
        }
    }
    for i in 0..lists.fixes.len() {
        let (n_gone, n_keep, third, m) = lists.fixes[i];
        for o in [n_gone, n_keep] {
            if o != NONE
                && w.alive[at(o)]
                && let Some(c) = w.side(o, keep, third)
            {
                w.mark[at(c)] = m;
            }
        }
    }
    // One ring of the survivors of both, in the exact order of `ring_b` then `ring_a`.
    let mut h = NONE;
    for &c in lists.ring_b.iter().rev().chain(lists.ring_a.iter().rev()) {
        if w.alive[at(c / 3)] {
            w.next[at(c)] = h;
            h = c;
        }
    }
    w.head[at(keep)] = h;
    w.head[at(gone)] = NONE;
    w.pos[at(keep)] = plan.p;
    w.quad[at(keep)] = plan.q;
    if w.has_zones {
        let (k, g) = (at(keep), at(gone));
        w.zone_limit[k] = w.zone_limit[k].min(w.zone_limit[g]);
        w.zone_min_cos_sq[k] = w.zone_min_cos_sq[k].max(w.zone_min_cos_sq[g]);
        w.zone_weight[k] = w.zone_weight[k].max(w.zone_weight[g]);
        w.zone_pinned[k] |= w.zone_pinned[g];
    }
    w.parent[at(gone)] = keep;
    w.version[at(keep)] += 1;
    w.version[at(gone)] += 1;
    Ok(())
}
