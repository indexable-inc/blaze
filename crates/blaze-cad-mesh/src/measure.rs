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
use blaze_units::{Area, Length, Volume};

use crate::check::Verified;
use crate::geom::{Aabb, D3, M3, add, cross, distance, dot, length, length_up, scale, sub, tri_normal, unit, wide};
use crate::ids::TriId;
use crate::mesh::MeshView;
use crate::vecs::{Dsu, Half, V, at, edge_runs, filled, halves, len32, spots};
use crate::weld::least_first;

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

/// Topological invariants of a mesh on welded unique positions (`spots`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Topology {
    /// Distinct vertex positions (ignoring normal/UV split duplicates).
    pub vertices: u32,
    /// Distinct undirected position edges.
    pub edges: u32,
    /// Triangles in the mesh.
    pub triangles: u32,
    /// Edge-connected shell components.
    pub components: u32,
    /// Boundary edges with exactly one incident triangle (0 for a watertight mesh).
    pub open_edges: u32,
    /// Edges with more than two incident triangles (0 for a manifold mesh).
    pub non_manifold_edges: u32,
    /// Euler characteristic `V - E + F`.
    pub euler: i64,
    /// Exact total topological genus (number of through-holes / handles) when the mesh is closed
    /// and edge-manifold (`C - (V - E + F) / 2`); `None` when open or non-manifold.
    pub genus: Option<u32>,
}

/// Compute exact topological invariants (`V`, `E`, `F`, connected components, Euler characteristic,
/// and through-hole genus) of `view` on welded unique positions.
pub fn topology(view: &MeshView<'_>, scratch: Alloc<'_>) -> Result<Topology, AllocFailure> {
    let triangles = view.triangle_count();
    if triangles == 0 {
        return Ok(Topology {
            vertices: 0,
            edges: 0,
            triangles: 0,
            components: 0,
            open_edges: 0,
            non_manifold_edges: 0,
            euler: 0,
            genus: Some(0),
        });
    }
    let sp = spots(view.positions(), scratch)?;
    let hs = halves(view.triangles(), &sp.of_vertex, scratch)?;
    // Count only unique position spots actually referenced by triangles (isolated vertices do not
    // contribute to the surface's Euler characteristic).
    let mut used = filled(at(sp.count()), false, scratch)?;
    for tri in view.triangles() {
        for &v in tri {
            used[at(sp.of_vertex[at(v)])] = true;
        }
    }
    let vertices = len32(used.iter().filter(|&&u| u).count());
    let mut edges = 0u32;
    let mut open_edges = 0u32;
    let mut non_manifold_edges = 0u32;
    for (start, end) in edge_runs(&hs) {
        edges += 1;
        let deg = end - start;
        if deg == 1 {
            open_edges += 1;
        } else if deg > 2 {
            non_manifold_edges += 1;
        }
    }
    let comps = components_from(&hs, triangles, scratch)?;
    let euler = i64::from(vertices) - i64::from(edges) + i64::from(triangles);
    let genus = if open_edges == 0 && non_manifold_edges == 0 {
        let two_g = 2 * i64::from(comps.count) - euler;
        if two_g >= 0 && two_g % 2 == 0 {
            u32::try_from(two_g / 2).ok()
        } else {
            None
        }
    } else {
        None
    };
    Ok(Topology {
        vertices,
        edges,
        triangles,
        components: comps.count,
        open_edges,
        non_manifold_edges,
        euler,
        genus,
    })
}

/// Options for [`crease_profile`]: dihedral cosine thresholds separating smooth, bevel/chamfer,
/// and sharp crease edges.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct CreaseProfileOpts<'s> {
    /// Cosine of the minimum dihedral angle treated as a bevel/transition edge
    /// (default: `cos(15°) ≈ 0.9659258`).
    pub cos_bevel: f32 = 0.965_925_8,
    /// Cosine of the minimum dihedral angle treated as a sharp crease
    /// (default: `cos(60°) = 0.5`).
    pub cos_sharp: f32 = 0.5,
    pub scratch: Alloc<'s>,
}

/// Dihedral edge classification of a mesh: separates smooth surfaces, bevel/chamfer transitions
/// (`[15°, 60°)` by default), and sharp architectural creases (`>= 60°`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CreaseProfile {
    pub smooth_edges: u32,
    pub bevel_edges: u32,
    pub sharp_edges: u32,
    /// Total length of all bevel-angle edges (`cos_sharp < dot(n0, n1) <= cos_bevel`).
    pub bevel_edge_length: Length,
    /// Total length of all sharp crease edges (`dot(n0, n1) <= cos_sharp`).
    pub sharp_edge_length: Length,
    /// Total area of triangles incident to at least one bevel-angle edge.
    pub bevel_band_area: Area,
}

/// Classify every manifold edge of `view` by its geometric dihedral angle, deterministically and
/// independently of input vertex/triangle numbering.
pub fn crease_profile(view: &MeshView<'_>, o: CreaseProfileOpts<'_>) -> Result<CreaseProfile, AllocFailure> {
    let tris = view.triangle_count();
    if tris == 0 {
        return Ok(CreaseProfile {
            smooth_edges: 0,
            bevel_edges: 0,
            sharp_edges: 0,
            bevel_edge_length: Length::ZERO,
            sharp_edge_length: Length::ZERO,
            bevel_band_area: Area::m2(0.0),
        });
    }
    let s = o.scratch;
    let sp = spots(view.positions(), s)?;
    let hs = halves(view.triangles(), &sp.of_vertex, s)?;
    let mut normals: V<'_, Option<D3>> = filled(at(tris), None, s)?;
    let mut areas: V<'_, f64> = filled(at(tris), 0.0, s)?;
    for t in 0..tris {
        let raw = view.triangles()[at(t)];
        let v = least_first([sp.of_vertex[at(raw[0])], sp.of_vertex[at(raw[1])], sp.of_vertex[at(raw[2])]]);
        let a = wide(view.positions()[at(sp.first[at(v[0])])]);
        let b = wide(view.positions()[at(sp.first[at(v[1])])]);
        let c = wide(view.positions()[at(sp.first[at(v[2])])]);
        let n = tri_normal(a, b, c);
        areas[at(t)] = length(n) * 0.5;
        normals[at(t)] = unit(n);
    }

    let cos_bevel = f64::from(o.cos_bevel);
    let cos_sharp = f64::from(o.cos_sharp);
    let mut smooth_edges = 0u32;
    let mut bevel_edges = 0u32;
    let mut sharp_edges = 0u32;
    let mut bevel_len = 0.0f64;
    let mut sharp_len = 0.0f64;
    let mut on_bevel = filled(at(tris), false, s)?;

    for (start, end) in edge_runs(&hs) {
        if end - start != 2 {
            continue;
        }
        let h0 = hs[start];
        let h1 = hs[start + 1];
        let (Some(n0), Some(n1)) = (normals[at(h0.tri)], normals[at(h1.tri)]) else { continue };
        let c = dot(n0, n1);
        let p0 = wide(view.positions()[at(sp.first[at(h0.lo)])]);
        let p1 = wide(view.positions()[at(sp.first[at(h0.hi)])]);
        let elen = distance(p0, p1);
        if c <= cos_sharp {
            sharp_edges += 1;
            sharp_len += elen;
        } else if c <= cos_bevel {
            bevel_edges += 1;
            bevel_len += elen;
            on_bevel[at(h0.tri)] = true;
            on_bevel[at(h1.tri)] = true;
        } else {
            smooth_edges += 1;
        }
    }

    let mut band_area = 0.0f64;
    for t in 0..tris {
        if on_bevel[at(t)] {
            band_area += areas[at(t)];
        }
    }
    Ok(CreaseProfile {
        smooth_edges,
        bevel_edges,
        sharp_edges,
        bevel_edge_length: length_up(bevel_len),
        sharp_edge_length: length_up(sharp_len),
        bevel_band_area: Area::m2(band_area),
    })
}
