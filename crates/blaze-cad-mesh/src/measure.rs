//! Measures of a mesh: its box, area, connected components, and the mass properties of the solid
//! a verified mesh bounds.
//!
//! Mass properties are the exact polyhedral integrals by the canonical-tetrahedron covariance
//! method (J. Blow and A. Binstock, "How to find the inertia tensor (or other mass properties)
//! of a 3D solid body represented by a triangle mesh", 2004; the same integrals as F. Tonon,
//! "Explicit exact formulas for the 3-D tetrahedron inertia tensor in terms of its vertex
//! coordinates", J. Math. Stat. 1(1), 2004): the solid is the signed sum of the tetrahedra from a
//! reference point `o` to each triangle `(a, b, c)`; tetrahedron `t` has volume `det/6` and
//! second moment `C_t = det/120 · (a aᵀ + b bᵀ + c cᵀ + s sᵀ)` with `det = a · (b × c)` and
//! `s = a + b + c` (positions relative to `o`). Summed: `V`, the first moment and
//! `C = ∫ x xᵀ dV`; moved to the centroid `c`, `C_c = C − V c cᵀ`; the inertia tensor for unit
//! density is `I = tr(C_c) 1 − C_c`. Exact up to rounding: everything is `f64`, summed in
//! triangle order, `o` the middle of the box so fewer digits cancel.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::{Area, Volume};

use crate::check::Verified;
use crate::geom::{Aabb, D3, M3, add, cross, dot, length, scale, sub, tri_normal, wide};
use crate::ids::TriId;
use crate::mesh::MeshView;
use crate::vecs::{Dsu, Half, V, at, edge_runs, filled, halves, spots};

/// The box of every vertex (used or not).
pub fn bounds(view: &MeshView<'_>) -> Aabb {
    let mut b = Aabb::EMPTY;
    for &p in view.positions() {
        b.include(p);
    }
    b
}

/// The surface's area, summed in triangle order.
pub fn area(view: &MeshView<'_>) -> f64 {
    let mut total = 0.0;
    for t in 0..view.triangle_count() {
        let c = view.corners(TriId(t));
        total += length(tri_normal(wide(c[0]), wide(c[1]), wide(c[2]))) * 0.5;
    }
    total
}

/// Each triangle's connected component, the components numbered in the order of their first
/// triangle. Two triangles are connected when they share an edge (a pair of positions).
#[derive(Debug)]
pub struct Components<'a> {
    pub of_triangle: Vec<u32, Alloc<'a>>,
    pub count: u32,
}

/// `view`'s components, in `alloc` (which also holds the scratch, freed before return).
pub fn components<'a>(view: &MeshView<'_>, alloc: Alloc<'a>) -> Result<Components<'a>, AllocFailure> {
    let spots = spots(view.positions(), alloc)?;
    let halves = halves(view.triangles(), &spots.of_vertex, alloc)?;
    components_from(&halves, view.triangle_count(), alloc)
}

pub(crate) fn components_from<'a>(halves: &[Half], triangles: u32, alloc: Alloc<'a>) -> Result<Components<'a>, AllocFailure> {
    let mut dsu = Dsu::new(triangles, alloc)?;
    for (start, end) in edge_runs(halves) {
        for h in &halves[start + 1..end] {
            dsu.union(halves[start].tri, h.tri);
        }
    }
    let mut number: V<'_, u32> = filled(at(triangles), u32::MAX, alloc)?;
    let mut of_triangle = filled(at(triangles), 0u32, alloc)?;
    let mut count = 0u32;
    for t in 0..triangles {
        let root = dsu.find(t);
        if number[at(root)] == u32::MAX {
            number[at(root)] = count;
            count += 1;
        }
        of_triangle[at(t)] = number[at(root)];
    }
    Ok(Components { of_triangle, count })
}

/// The mass properties of the solid a verified mesh bounds (positions in metres), for unit
/// density. Volume and area are blaze-units' typed answers; a material's mass is
/// `Density * volume`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MassProps {
    pub volume: Volume,
    pub area: Area,
    /// Metres.
    pub centroid: D3,
    /// The inertia tensor about the centroid, axes those of the mesh, for unit density (scale by
    /// the density for a material's).
    pub inertia: M3,
}

/// See the module's documentation for the method.
pub fn mass_properties(mesh: &Verified<'_>) -> MassProps {
    let view = mesh.view();
    let o = bounds(&view).centre();
    let mut volume = 0.0f64;
    let mut first = [0.0f64; 3];
    let mut c = [[0.0f64; 3]; 3];
    for t in 0..view.triangle_count() {
        let p = view.corners(TriId(t));
        let (a, b, cc) = (sub(wide(p[0]), o), sub(wide(p[1]), o), sub(wide(p[2]), o));
        let det = dot(a, cross(b, cc));
        let s = add(add(a, b), cc);
        volume += det / 6.0;
        first = add(first, scale(s, det / 24.0));
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] += det / 120.0 * (a[i] * a[j] + b[i] * b[j] + cc[i] * cc[j] + s[i] * s[j]);
            }
        }
    }
    let centre = scale(first, 1.0 / volume);
    let mut cc = c;
    for i in 0..3 {
        for j in 0..3 {
            cc[i][j] -= volume * centre[i] * centre[j];
        }
    }
    let trace = cc[0][0] + cc[1][1] + cc[2][2];
    let mut inertia = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            inertia[i][j] = if i == j { trace - cc[i][j] } else { -cc[i][j] };
        }
    }
    // A verified mesh is finite and closed with positive volume, so both are finite.
    MassProps { volume: Volume::m3(volume), area: Area::m2(area(&view)), centroid: add(o, centre), inertia }
}

/// The signed volume of a closed triangle set about `o`, summed in order: for hulls and pieces.
pub(crate) fn signed_volume(points: &[[f32; 3]], triangles: &[[u32; 3]], o: D3) -> f64 {
    let mut volume = 0.0;
    for tri in triangles {
        let a = sub(wide(points[at(tri[0])]), o);
        let b = sub(wide(points[at(tri[1])]), o);
        let c = sub(wide(points[at(tri[2])]), o);
        volume += dot(a, cross(b, c)) / 6.0;
    }
    volume
}
