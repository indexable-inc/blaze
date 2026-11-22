//! The verifier (rule 74): [`check()`] reads a mesh once and either names the first defect,
//! with the triangle, edge or vertex and its coordinates, or returns a [`Verified`], which nothing
//! else in or out of this crate can make. Later stages take a `&Verified` and check nothing again.
//!
//! What a verified mesh is, each in the order checked:
//!
//! 1. within [`MeshLimits`]' vertex and triangle counts, and not empty;
//! 2. every coordinate finite;
//! 3. no triangle with two corners at one position, and none whose area vector (computed in
//!    `f64` from the stored `f32`s) is exactly zero;
//! 4. no two triangles on the same three positions, in either orientation;
//! 5. closed and manifold at every edge: each edge (a pair of positions) is the side of exactly
//!    two triangles, which run along it in opposite directions (consistent orientation);
//! 6. manifold at every vertex: the triangles around each position are one fan, not two cones
//!    touching at a point;
//! 7. outward: each connected component's signed volume is positive.
//!
//! Topology is read on POSITIONS, not vertex ids: a vertex split for a normal or a UV seam is
//! one point of the surface, so a shaded mesh verifies as the solid it is.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure};

use crate::MeshLimits;
use crate::digest::MeshDigest;
use crate::geom::{P3, cross, dot, sub, tri_normal, wide};
use crate::ids::{TriId, VertexId};
use crate::measure::{bounds, components_from};
use crate::mesh::MeshView;
use crate::vecs::{Dsu, V, at, edge_runs, filled, halves, len32, push, spots};

/// The verifier's options.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct CheckOpts<'s> {
    pub limits: MeshLimits,
    /// Where the verifier's scratch lives; all of it is freed before it returns.
    pub scratch: Alloc<'s>,
}

/// The first defect found, naming where.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Defect {
    TooManyVertices { vertices: u32, most: u32 },
    TooManyTriangles { triangles: u32, most: u32 },
    /// No triangle at all: nothing is a closed surface.
    Empty,
    NonFinite { vertex: VertexId, at: P3 },
    /// Two corners of one triangle at one position.
    RepeatedCorner { tri: TriId, at: [P3; 3] },
    /// A triangle whose area vector is exactly zero (its corners on one line).
    ZeroArea { tri: TriId, at: [P3; 3] },
    /// Two triangles on the same three positions.
    Duplicate { tris: [TriId; 2], at: [P3; 3] },
    /// An edge with one triangle: the surface has a hole there.
    OpenEdge { tri: TriId, at: [P3; 2] },
    /// An edge with more than two triangles (`tris` are the first two of them).
    NonManifoldEdge { tris: [TriId; 2], count: u32, at: [P3; 2] },
    /// Two triangles running the same way along their shared edge: one of them is flipped.
    Inconsistent { tris: [TriId; 2], at: [P3; 2] },
    /// A position whose triangles are `fans` separate fans.
    NonManifoldVertex { vertex: VertexId, fans: u32, at: P3 },
    /// A component (numbered by its first triangle, `tri`) whose signed volume is not positive:
    /// it faces inward.
    InsideOut { component: u32, tri: TriId, volume: f64 },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for Defect {
    fn from(e: AllocFailure) -> Defect {
        Defect::Alloc(e)
    }
}

/// What the verifier established, kept with the mesh it is about.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Proof {
    digest: MeshDigest,
    components: u32,
}

impl Proof {
    /// The digest of the mesh the proof is about.
    #[inline]
    pub fn digest(&self) -> MeshDigest {
        self.digest
    }

    /// How many connected components the surface has.
    #[inline]
    pub fn components(&self) -> u32 {
        self.components
    }
}

/// A mesh with its proof: closed, manifold, consistently and outward oriented, no degenerate or
/// duplicate triangle, every coordinate finite. Only [`check()`] makes one.
#[derive(Clone, Copy, Debug)]
pub struct Verified<'a> {
    view: MeshView<'a>,
    proof: Proof,
}

impl<'a> Verified<'a> {
    #[inline]
    pub fn view(&self) -> MeshView<'a> {
        self.view
    }

    #[inline]
    pub fn proof(&self) -> Proof {
        self.proof
    }

    /// A proof `check` gave for `view`, put back beside it by an owner that kept the two apart
    /// (a struct cannot hold a view of its own mesh). Crate-private: only code that got `proof`
    /// from `check` on this very mesh calls it.
    pub(crate) fn with_proof(view: MeshView<'a>, proof: Proof) -> Verified<'a> {
        Verified { view, proof }
    }
}

/// Verify `view`; see the module's documentation for what is checked, in which order.
pub fn check<'a>(view: MeshView<'a>, opts: CheckOpts<'_>) -> Result<Verified<'a>, Defect> {
    let scratch = opts.scratch;
    let (vertices, triangles) = (view.vertex_count(), view.triangle_count());
    if vertices > opts.limits.most_vertices {
        return Err(Defect::TooManyVertices { vertices, most: opts.limits.most_vertices });
    }
    if triangles > opts.limits.most_triangles {
        return Err(Defect::TooManyTriangles { triangles, most: opts.limits.most_triangles });
    }
    if triangles == 0 {
        return Err(Defect::Empty);
    }
    for (v, p) in view.positions().iter().enumerate() {
        if !p.iter().all(|x| x.is_finite()) {
            return Err(Defect::NonFinite { vertex: VertexId(len32(v)), at: *p });
        }
    }
    let spots = spots(view.positions(), scratch)?;
    let spot = &spots.of_vertex;
    let tris = view.triangles();
    for (t, tri) in tris.iter().enumerate() {
        let tri_id = TriId(len32(t));
        let s = [spot[at(tri[0])], spot[at(tri[1])], spot[at(tri[2])]];
        if s[0] == s[1] || s[1] == s[2] || s[0] == s[2] {
            return Err(Defect::RepeatedCorner { tri: tri_id, at: view.corners(tri_id) });
        }
        let c = view.corners(tri_id);
        let n = tri_normal(wide(c[0]), wide(c[1]), wide(c[2]));
        if n == [0.0; 3] {
            return Err(Defect::ZeroArea { tri: tri_id, at: c });
        }
    }
    duplicates(&view, spot, scratch)?;
    let halves = halves(tris, spot, scratch)?;
    let segment = |lo: u32, hi: u32| -> [P3; 2] {
        [view.positions()[at(spots.first[at(lo)])], view.positions()[at(spots.first[at(hi)])]]
    };
    for (start, end) in edge_runs(&halves) {
        let h = halves[start];
        let count = len32(end - start);
        if count == 1 {
            return Err(Defect::OpenEdge { tri: TriId(h.tri), at: segment(h.lo, h.hi) });
        }
        if count > 2 {
            return Err(Defect::NonManifoldEdge {
                tris: [TriId(h.tri), TriId(halves[start + 1].tri)],
                count,
                at: segment(h.lo, h.hi),
            });
        }
        if h.forward == halves[start + 1].forward {
            return Err(Defect::Inconsistent {
                tris: [TriId(h.tri), TriId(halves[start + 1].tri)],
                at: segment(h.lo, h.hi),
            });
        }
    }
    fans(&view, spot, &spots.first, &halves, scratch)?;
    let components = components_from(&halves, triangles, scratch)?;
    // Each component's signed volume, about the middle of the mesh's box (fewer digits cancel
    // than about the origin), summed in triangle order.
    let o = bounds(&view).centre();
    let mut volume = filled(at(components.count), 0.0f64, scratch)?;
    for (t, &component) in components.of_triangle.iter().enumerate() {
        let c = view.corners(TriId(len32(t)));
        let (a, b, cc) = (sub(wide(c[0]), o), sub(wide(c[1]), o), sub(wide(c[2]), o));
        volume[at(component)] += dot(a, cross(b, cc)) / 6.0;
    }
    for (component, &v) in volume.iter().enumerate() {
        if !(v > 0.0) {
            let first = components.of_triangle.iter().position(|&c| at(c) == component).map_or(0, len32);
            return Err(Defect::InsideOut { component: len32(component), tri: TriId(first), volume: v });
        }
    }
    Ok(Verified { proof: Proof { digest: MeshDigest::of(&view), components: components.count }, view })
}

/// No two triangles on one set of three positions.
fn duplicates(view: &MeshView<'_>, spot: &[u32], scratch: Alloc<'_>) -> Result<(), Defect> {
    let mut keyed: V<'_, ([u32; 3], u32)> = blaze_own::Vec::try_with_capacity_in(view.triangles().len(), scratch)?;
    for (t, tri) in view.triangles().iter().enumerate() {
        let mut s = [spot[at(tri[0])], spot[at(tri[1])], spot[at(tri[2])]];
        s.sort_unstable();
        push(&mut keyed, (s, len32(t)))?;
    }
    keyed.sort_unstable();
    for pair in keyed.windows(2) {
        if pair[0].0 == pair[1].0 {
            let (a, b) = (TriId(pair[0].1), TriId(pair[1].1));
            return Err(Defect::Duplicate { tris: [a, b], at: view.corners(a) });
        }
    }
    Ok(())
}

/// The corner of triangle `h.tri` at spot `lo` and the one at `hi`, as corner numbers `3t + k`.
pub(crate) fn corner_pair(h: crate::vecs::Half) -> (u32, u32) {
    let s = u32::from(h.slot);
    let next = (s + 1) % 3;
    let (at_lo, at_hi) = if h.forward { (s, next) } else { (next, s) };
    (h.tri * 3 + at_lo, h.tri * 3 + at_hi)
}

/// Each position's corners are one fan: corners of two triangles sharing an edge are joined at
/// both ends of the edge, and a position whose corners fall into more than one class is two fans.
fn fans(view: &MeshView<'_>, spot: &[u32], first: &[u32], halves: &[crate::vecs::Half], scratch: Alloc<'_>) -> Result<(), Defect> {
    let corners = view.triangle_count() * 3;
    let mut dsu = Dsu::new(corners, scratch)?;
    for (start, end) in edge_runs(halves) {
        // Every edge has exactly two halves here: the edge checks ran first.
        if end - start == 2 {
            let (a_lo, a_hi) = corner_pair(halves[start]);
            let (b_lo, b_hi) = corner_pair(halves[start + 1]);
            dsu.union(a_lo, b_lo);
            dsu.union(a_hi, b_hi);
        }
    }
    let mut pairs: V<'_, (u32, u32)> = blaze_own::Vec::try_with_capacity_in(at(corners), scratch)?;
    for (t, tri) in view.triangles().iter().enumerate() {
        for (k, &v) in tri.iter().enumerate() {
            let corner = len32(t * 3 + k);
            push(&mut pairs, (spot[at(v)], dsu.find(corner)))?;
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    for run in pairs.chunk_by(|a, b| a.0 == b.0) {
        if run.len() > 1 {
            let vertex = VertexId(first[at(run[0].0)]);
            return Err(Defect::NonManifoldVertex { vertex, fans: len32(run.len()), at: view.position(vertex) });
        }
    }
    Ok(())
}
