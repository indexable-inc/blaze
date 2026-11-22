//! The canonical form of a mesh: vertices sorted by all their attributes (positions, then
//! normals, tangents, UVs, colours, each float by IEEE `totalOrder`) with exactly equal vertices
//! merged, each triangle turned to start at its least vertex (keeping its orientation), the
//! triangles sorted by `(vertices, part, role)`, the creases renumbered and sorted.
//!
//! Two meshes that differ only in vertex numbering, triangle order and the corner each triangle
//! starts at have byte-equal canonical forms. The algorithms here that are deterministic only
//! for a fixed input order (simplification, levels of detail, cache and fetch order, clusters,
//! decomposition) are order-free when started from this form.

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::ids::{VertexPair, VertexId};
use crate::mesh::{Mesh, MeshVecs, MeshView, ShapeError, built, empty_vecs};
use crate::vecs::{V, at, counting, filled, len32, push};
use crate::weld::least_first;

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct CanonOpts<'a, 's> {
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CanonError {
    Alloc(AllocFailure),
    /// The mesh built here did not fit together: a bug in `canonical`.
    Built(ShapeError),
}

impl From<AllocFailure> for CanonError {
    fn from(e: AllocFailure) -> CanonError {
        CanonError::Alloc(e)
    }
}

fn floats(a: &[f32], b: &[f32]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = x.total_cmp(y);
        if o.is_ne() {
            return o;
        }
    }
    Ordering::Equal
}

/// The order of two vertices by all their attributes; `Equal` only for bit-equal vertices.
pub(crate) fn vertex_order(view: &MeshView<'_>, a: u32, b: u32) -> Ordering {
    let (a, b) = (at(a), at(b));
    let mut o = floats(&view.positions()[a], &view.positions()[b]).then_with(|| floats(&view.normals()[a], &view.normals()[b]));
    if let Some(t) = view.tangents() {
        o = o.then_with(|| floats(&t[a], &t[b]));
    }
    if let Some(uv) = view.uvs() {
        o = o.then_with(|| floats(&uv[a], &uv[b]));
    }
    if let Some(c) = view.colours() {
        o = o.then_with(|| c[a].cmp(&c[b]));
    }
    o
}

/// `view` in canonical form, in `opts.alloc`.
pub fn canonical<'a>(view: &MeshView<'_>, opts: CanonOpts<'a, '_>) -> Result<Mesh<'a>, CanonError> {
    let scratch = opts.scratch;
    let mut order = counting(view.vertex_count(), scratch)?;
    order.sort_unstable_by(|&a, &b| vertex_order(view, a, b).then(a.cmp(&b)));
    let mut new_index = filled(order.len(), 0u32, scratch)?;
    let mut out: MeshVecs<'a> = empty_vecs(opts.alloc);
    let mut last: Option<u32> = None;
    for &v in order.iter() {
        if last.is_none_or(|l| vertex_order(view, l, v).is_ne()) {
            last = Some(v);
            let i = at(v);
            push(&mut out.positions, view.positions()[i])?;
            push(&mut out.normals, view.normals()[i])?;
            if let Some(t) = view.tangents() {
                push(&mut out.tangents, t[i])?;
            }
            if let Some(uv) = view.uvs() {
                push(&mut out.uvs, uv[i])?;
            }
            if let Some(c) = view.colours() {
                push(&mut out.colours, c[i])?;
            }
        }
        new_index[at(v)] = len32(out.positions.len()) - 1;
    }
    let mut tris: V<'_, ([u32; 3], u32, u32)> = Vec::try_with_capacity_in(view.triangles().len(), scratch)?;
    for (t, tri) in view.triangles().iter().enumerate() {
        let v = [new_index[at(tri[0])], new_index[at(tri[1])], new_index[at(tri[2])]];
        let turned = if v[0] == v[1] || v[1] == v[2] || v[0] == v[2] { v } else { least_first(v) };
        push(&mut tris, (turned, view.parts()[t].0, view.roles()[t].0))?;
    }
    tris.sort_unstable();
    for &(tri, part, role) in tris.iter() {
        push(&mut out.triangles, tri)?;
        push(&mut out.parts, crate::ids::PartId(part))?;
        push(&mut out.roles, crate::ids::MaterialRole(role))?;
    }
    for edge in view.creases() {
        let (a, b) = (new_index[edge.lo().index()], new_index[edge.hi().index()]);
        if a != b {
            push(&mut out.creases, VertexPair::new(VertexId(a), VertexId(b)))?;
        }
    }
    out.creases.sort_unstable();
    out.creases.dedup();
    built(out).map_err(CanonError::Built)
}
