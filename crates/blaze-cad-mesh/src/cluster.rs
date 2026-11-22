//! Clusters (meshlets) for mesh shaders and cluster culling.
//!
//! GREEDY GROWTH WITH A LOCALITY SCORE (the approach of meshoptimizer's `buildMeshlets`, A.
//! Kapoulkine, written here from its description): the seed of a cluster is the lowest-numbered
//! triangle not yet placed; then, while one fits the caps, the cluster takes the unplaced
//! triangle sharing a vertex with it that adds the FEWEST NEW VERTICES, then is NEAREST the
//! cluster's centroid (squared distance in `f64` between the triangle's centroid and the mean of
//! the cluster's triangles' centroids), then has the lowest id. A cluster closes when no
//! neighbouring triangle fits; the next seed is the next unplaced triangle in order. Work per
//! added triangle is bounded by the caps times the largest vertex degree.
//!
//! BOUNDS. Each cluster's sphere is Ritter's (J. Ritter, "An efficient bounding sphere",
//! Graphics Gems, 1990: the two far points, then grown for each point outside), with its centre
//! rounded to `f32` and its radius then recomputed as the largest distance from that centre and
//! rounded UP to `f32`, so it holds every vertex exactly. The normal cone's axis is the
//! normalised sum of the triangles' unit normals and `cone_cos` the least cosine between the
//! axis and a triangle's normal, rounded DOWN to `f32` (`-1.0` when the normals span more than a
//! half-space: no culling). Every triangle of the cluster faces away from a camera looking along
//! the unit direction `d` (camera to cluster, orthographic) when
//! `dot(d, axis) ≥ sqrt(1 − cone_cos²)` and `cone_cos > 0`.
//!
//! Deterministic for a fixed input order; not order-free (start from [`crate::canon`]'s form).

#![blaze::sans_io]

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::geom::{D3, P3, add, distance, dot, narrow, scale, sub, tri_normal, unit, up_f32, wide};
use crate::ids::TriId;
use crate::mesh::MeshView;
use crate::plant::ClusterPlant;
use crate::vecs::{V, adjacency, at, filled, len32, push};

/// The caps of one cluster.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClusterCaps {
    /// At most 255 (a local index is a byte); 64 is the usual mesh-shader size.
    pub most_vertices: u32,
    /// At most 512; 124 is the usual (it fills 128 bytes of local indices with room to spare).
    pub most_triangles: u32,
}

impl ClusterCaps {
    pub const SHIPPED: ClusterCaps = ClusterCaps { most_vertices: 64, most_triangles: 124 };
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ClusterOpts<'a, 's> {
    pub caps: ClusterCaps,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// One cluster: its ranges in [`Clusters`]' columns and its bounds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cluster {
    pub vertex_offset: u32,
    pub vertex_count: u32,
    pub triangle_offset: u32,
    pub triangle_count: u32,
    pub centre: P3,
    pub radius: f32,
    pub cone_axis: P3,
    pub cone_cos: f32,
}

/// Every cluster, and the columns their ranges index.
#[derive(Debug)]
pub struct Clusters<'a> {
    pub clusters: Vec<Cluster, Alloc<'a>>,
    /// The mesh's vertex of each local vertex, cluster after cluster.
    pub vertices: Vec<u32, Alloc<'a>>,
    /// Each local triangle as three local vertex indices, in the input triangle's corner order.
    pub triangles: Vec<[u8; 3], Alloc<'a>>,
    /// The input triangle of each local triangle.
    pub sources: Vec<TriId, Alloc<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClusterError {
    /// Caps outside `1..=255` vertices (and at least 3) or `1..=512` triangles.
    BadCaps { caps: ClusterCaps },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for ClusterError {
    fn from(e: AllocFailure) -> ClusterError {
        ClusterError::Alloc(e)
    }
}

const NONE: u32 = u32::MAX;

pub fn clusters<'a>(view: &MeshView<'_>, opts: ClusterOpts<'a, '_>) -> Result<Clusters<'a>, ClusterError> {
    clusters_planted(view, opts, NoPlant)
}

/// [`clusters`] with a planted bug, for the tests' controls.
pub fn clusters_planted<'a>(
    view: &MeshView<'_>,
    opts: ClusterOpts<'a, '_>,
    plant: impl Planted<ClusterPlant>,
) -> Result<Clusters<'a>, ClusterError> {
    let caps = opts.caps;
    if !(3..=255).contains(&caps.most_vertices) || !(1..=512).contains(&caps.most_triangles) {
        return Err(ClusterError::BadCaps { caps });
    }
    let most_vertices = if plant.has(ClusterPlant::OverCap) { caps.most_vertices + 1 } else { caps.most_vertices };
    let s = opts.scratch;
    let tris = view.triangles();
    let n = view.vertex_count();
    let adj = adjacency(tris, n, s)?;
    let mut centroid: V<'_, D3> = Vec::try_with_capacity_in(tris.len(), s)?;
    for t in 0..view.triangle_count() {
        let c = view.corners(TriId(t));
        push(&mut centroid, scale(add(add(wide(c[0]), wide(c[1])), wide(c[2])), 1.0 / 3.0))?;
    }
    let mut placed = filled(tris.len(), false, s)?;
    // `local[v]` is `v`'s local index in the open cluster, or NONE.
    let mut local = filled(at(n), NONE, s)?;
    let mut out = Clusters {
        clusters: Vec::new_in(opts.alloc),
        vertices: Vec::new_in(opts.alloc),
        triangles: Vec::new_in(opts.alloc),
        sources: Vec::new_in(opts.alloc),
    };
    let mut cursor = 0usize;
    loop {
        while cursor < tris.len() && placed[cursor] {
            cursor += 1;
        }
        if cursor == tris.len() {
            break;
        }
        let vertex_offset = len32(out.vertices.len());
        let triangle_offset = len32(out.triangles.len());
        let mut sum = [0.0f64; 3];
        let mut next = Some(len32(cursor));
        while let Some(t) = next {
            placed[at(t)] = true;
            let mut l = [0u8; 3];
            for (k, &v) in tris[at(t)].iter().enumerate() {
                if local[at(v)] == NONE {
                    local[at(v)] = len32(out.vertices.len()) - vertex_offset;
                    push(&mut out.vertices, v)?;
                }
                // At most 255 vertices a cluster (the caps), so a local index fits a byte; the
                // planted cap is at most 256, whose last index is 255.
                l[k] = blaze_int::Int::truncate_to::<u8>(local[at(v)]);
            }
            push(&mut out.triangles, l)?;
            push(&mut out.sources, TriId(t))?;
            sum = add(sum, centroid[at(t)]);
            let count = len32(out.triangles.len()) - triangle_offset;
            let used = len32(out.vertices.len()) - vertex_offset;
            if count >= caps.most_triangles {
                break;
            }
            let mean = scale(sum, 1.0 / f64::from(count));
            let mut best: Option<(u32, f64, u32)> = None;
            for &v in &out.vertices[at(vertex_offset)..] {
                for &cand in adj.of(v) {
                    if placed[at(cand)] {
                        continue;
                    }
                    let new = len32(tris[at(cand)].iter().filter(|&&w| local[at(w)] == NONE).count());
                    if used + new > most_vertices {
                        continue;
                    }
                    let d = sub(centroid[at(cand)], mean);
                    let near = dot(d, d);
                    let better = match best {
                        None => true,
                        Some((b_new, b_near, b_cand)) => {
                            new.cmp(&b_new).then(near.total_cmp(&b_near)).then(cand.cmp(&b_cand)).is_lt()
                        }
                    };
                    if better {
                        best = Some((new, near, cand));
                    }
                }
            }
            next = best.map(|b| b.2);
        }
        let vertices = &out.vertices[at(vertex_offset)..];
        for &v in vertices {
            local[at(v)] = NONE;
        }
        let (centre, radius) = sphere(view, vertices);
        let (cone_axis, cone_cos) = cone(view, &out.sources[at(triangle_offset)..]);
        let cluster = Cluster {
            vertex_offset,
            vertex_count: len32(vertices.len()),
            triangle_offset,
            triangle_count: len32(out.triangles.len()) - triangle_offset,
            centre,
            radius,
            cone_axis,
            cone_cos,
        };
        push(&mut out.clusters, cluster)?;
    }
    Ok(out)
}

/// Ritter's sphere, its centre rounded to `f32` and its radius made exact (module documentation).
fn sphere(view: &MeshView<'_>, vertices: &[u32]) -> (P3, f32) {
    let p = |v: u32| wide(view.positions()[at(v)]);
    let first = p(vertices[0]);
    let far = |from: D3| -> D3 {
        let mut best = from;
        let mut best_d = -1.0;
        for &v in vertices {
            let d = distance(p(v), from);
            if d > best_d {
                best_d = d;
                best = p(v);
            }
        }
        best
    };
    let a = far(first);
    let b = far(a);
    let mut c = scale(add(a, b), 0.5);
    let mut r = distance(a, b) * 0.5;
    for &v in vertices {
        let d = distance(p(v), c);
        if d > r {
            let grown = (r + d) * 0.5;
            c = add(c, scale(sub(p(v), c), (grown - r) / d));
            r = grown;
        }
    }
    let centre = narrow(c);
    let mut exact = 0.0f64;
    for &v in vertices {
        exact = exact.max(distance(p(v), wide(centre)));
    }
    (centre, up_f32(exact))
}

/// The normal cone (module documentation).
fn cone(view: &MeshView<'_>, sources: &[TriId]) -> (P3, f32) {
    let normal = |t: TriId| {
        let c = view.corners(t);
        unit(tri_normal(wide(c[0]), wide(c[1]), wide(c[2])))
    };
    let mut sum = [0.0f64; 3];
    for &t in sources {
        if let Some(n) = normal(t) {
            sum = add(sum, n);
        }
    }
    let Some(axis) = unit(sum) else {
        return ([0.0; 3], -1.0);
    };
    let axis32 = narrow(axis);
    let mut least = 1.0f64;
    for &t in sources {
        match normal(t) {
            Some(n) => least = least.min(dot(wide(axis32), n)),
            None => return (axis32, -1.0),
        }
    }
    if least <= 0.0 {
        return (axis32, -1.0);
    }
    // Rounded down: the stored cone is never narrower than the true one.
    (axis32, -up_f32(-least))
}
