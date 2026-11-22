//! Normals.
//!
//! [`vertex_normals`] gives each vertex the normalised sum of its triangles' area vectors (each
//! triangle weighted by its area): smooth across every edge, for a mesh whose splits are
//! already where they should be.
//!
//! [`shade()`] puts the splits there: around each position, the corners of two triangles that share
//! an edge are one smooth group unless the edge is a crease mark, a border between parts or
//! material roles, or the angle between the two triangles' normals is more than the crease angle;
//! each group becomes a vertex whose normal is the area-weighted sum over the group. Area
//! weighting, not corner-angle weighting: an angle needs `acos`, which this crate does not use
//! (only basic operations and `sqrt`, for the same bits everywhere). The crease angle is given as
//! its cosine for the same reason: [`COS_32_DEGREES`] is the CAD exporter's default.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::check::corner_pair;
use crate::geom::{D3, P3, add, dot, narrow, tri_normal, unit, wide};
use crate::ids::{VertexPair, TriId, VertexId};
use crate::mesh::{Mesh, MeshVecs, MeshView, ShapeError, built, empty_vecs};
use crate::vecs::{Dsu, V, at, edge_runs, filled, halves, len32, push, spots};

/// `cos(32°)`, rounded to the nearest `f32`: the crease angle marten's exporter splits at.
pub const COS_32_DEGREES: f32 = 0.848_048_1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShadeError {
    /// A used vertex whose triangles' area vectors sum to zero: it has no direction.
    NoNormal { vertex: VertexId },
    /// A triangle of zero area: it has no normal to contribute or compare.
    ZeroArea { tri: TriId },
    Alloc(AllocFailure),
    /// The mesh built here did not fit together: a bug in `shade`.
    Built(ShapeError),
}

impl From<AllocFailure> for ShadeError {
    fn from(e: AllocFailure) -> ShadeError {
        ShadeError::Alloc(e)
    }
}

/// Smooth normals, one per vertex, in `alloc`: each the normalised sum (in `f64`, in triangle
/// order) of its triangles' area vectors. A vertex no triangle uses gets the zero vector.
pub fn vertex_normals<'a>(view: &MeshView<'_>, alloc: Alloc<'a>) -> Result<Vec<P3, Alloc<'a>>, ShadeError> {
    let positions = view.positions().len();
    let mut sum = filled(positions, [0.0f64; 3], alloc)?;
    let mut used = filled(positions, false, alloc)?;
    for (t, tri) in view.triangles().iter().enumerate() {
        let c = view.corners(TriId(len32(t)));
        let n = tri_normal(wide(c[0]), wide(c[1]), wide(c[2]));
        for &v in tri {
            sum[at(v)] = add(sum[at(v)], n);
            used[at(v)] = true;
        }
    }
    let mut out = Vec::try_with_capacity_in(positions, alloc)?;
    for v in 0..positions {
        let n = if used[v] {
            match unit(sum[v]) {
                Some(n) => narrow(n),
                None => return Err(ShadeError::NoNormal { vertex: VertexId(len32(v)) }),
            }
        } else {
            [0.0; 3]
        };
        push(&mut out, n)?;
    }
    Ok(out)
}

/// The options of [`shade()`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ShadeOpts<'a, 's> {
    /// Two triangles meeting at an edge are shaded smooth across it when the cosine of the angle
    /// between their normals is at least this ([`COS_32_DEGREES`]; `-1.0` splits only at marks
    /// and borders, `1.0` at every edge between triangles not exactly coplanar).
    pub cos_crease: f32,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// `view` with its vertices split into smooth groups and each given its group's normal; see the
/// module's documentation. Tangents, UVs and colours are carried from the vertex each corner
/// came from; a vertex no triangle uses is left out. Deterministic for a fixed input: output
/// vertices are ordered by the input vertex they came from, then by their group's least corner.
pub fn shade<'a>(view: &MeshView<'_>, opts: ShadeOpts<'a, '_>) -> Result<Mesh<'a>, ShadeError> {
    let scratch = opts.scratch;
    let tris = view.triangles();
    let spots = spots(view.positions(), scratch)?;
    let spot = &spots.of_vertex;
    let mut face: V<'_, D3> = Vec::try_with_capacity_in(tris.len(), scratch)?;
    let mut unit_face: V<'_, D3> = Vec::try_with_capacity_in(tris.len(), scratch)?;
    for t in 0..view.triangle_count() {
        let c = view.corners(TriId(t));
        let n = tri_normal(wide(c[0]), wide(c[1]), wide(c[2]));
        let Some(u) = unit(n) else {
            return Err(ShadeError::ZeroArea { tri: TriId(t) });
        };
        push(&mut face, n)?;
        push(&mut unit_face, u)?;
    }
    let mut marked: V<'_, (u32, u32)> = Vec::try_with_capacity_in(view.creases().len(), scratch)?;
    for edge in view.creases() {
        let (a, b) = (spot[edge.lo().index()], spot[edge.hi().index()]);
        push(&mut marked, if a <= b { (a, b) } else { (b, a) })?;
    }
    marked.sort_unstable();
    let halves = halves(tris, spot, scratch)?;
    let corners = view.triangle_count() * 3;
    let mut dsu = Dsu::new(corners, scratch)?;
    let cos = f64::from(opts.cos_crease);
    for (start, end) in edge_runs(&halves) {
        if end - start != 2 {
            continue;
        }
        let (h1, h2) = (halves[start], halves[start + 1]);
        let (t1, t2) = (at(h1.tri), at(h2.tri));
        let smooth = marked.binary_search(&(h1.lo, h1.hi)).is_err()
            && view.roles()[t1] == view.roles()[t2]
            && view.parts()[t1] == view.parts()[t2]
            && dot(unit_face[t1], unit_face[t2]) >= cos;
        if smooth {
            let (a_lo, a_hi) = corner_pair(h1);
            let (b_lo, b_hi) = corner_pair(h2);
            dsu.union(a_lo, b_lo);
            dsu.union(a_hi, b_hi);
        }
    }
    // Each group's area-weighted normal sum, kept at the group's root corner.
    let mut sum = filled(at(corners), [0.0f64; 3], scratch)?;
    let mut keys: V<'_, (u32, u32, u32)> = Vec::try_with_capacity_in(at(corners), scratch)?;
    for (t, tri) in tris.iter().enumerate() {
        for (k, &v) in tri.iter().enumerate() {
            let corner = len32(t * 3 + k);
            let root = dsu.find(corner);
            sum[at(root)] = add(sum[at(root)], face[t]);
            push(&mut keys, (v, root, corner))?;
        }
    }
    keys.sort_unstable();
    let mut new_index = filled(at(corners), 0u32, scratch)?;
    let mut out: MeshVecs<'a> = empty_vecs(opts.alloc);
    let mut last: Option<(u32, u32)> = None;
    for &(v, root, corner) in keys.iter() {
        if last != Some((v, root)) {
            last = Some((v, root));
            let Some(n) = unit(sum[at(root)]) else {
                return Err(ShadeError::NoNormal { vertex: VertexId(v) });
            };
            push(&mut out.positions, view.positions()[at(v)])?;
            push(&mut out.normals, narrow(n))?;
            if let Some(t) = view.tangents() {
                push(&mut out.tangents, t[at(v)])?;
            }
            if let Some(uv) = view.uvs() {
                push(&mut out.uvs, uv[at(v)])?;
            }
            if let Some(colour) = view.colours() {
                push(&mut out.colours, colour[at(v)])?;
            }
        }
        new_index[at(corner)] = len32(out.positions.len()) - 1;
    }
    for t in 0..tris.len() {
        let c = t * 3;
        push(&mut out.triangles, [new_index[c], new_index[c + 1], new_index[c + 2]])?;
    }
    crate::vecs::extend(&mut out.parts, view.parts())?;
    crate::vecs::extend(&mut out.roles, view.roles())?;
    // Every split copy of a marked edge stays marked.
    for h in halves.iter() {
        if marked.binary_search(&(h.lo, h.hi)).is_ok() {
            let (lo, hi) = corner_pair(*h);
            push(&mut out.creases, VertexPair::new(VertexId(new_index[at(lo)]), VertexId(new_index[at(hi)])))?;
        }
    }
    out.creases.sort_unstable();
    out.creases.dedup();
    built(out).map_err(ShadeError::Built)
}
