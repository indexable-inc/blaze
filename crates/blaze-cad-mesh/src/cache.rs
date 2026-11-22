//! Orders for the GPU.
//!
//! [`optimize_vertex_cache`] reorders TRIANGLES by Tipsify (P. V. Sander, D. Nehab and J.
//! Barczak, "Fast triangle reordering for vertex locality and reduced overdraw", SIGGRAPH 2007):
//! it fans around one vertex at a time, emitting all of its triangles, then moves to the next
//! fanning vertex chosen among the vertices just touched by how long ago each entered the cache,
//! preferring ones that stay in it with their remaining triangles (`s − C[v] + 2·L[v] ≤ k`), and
//! on a dead end takes the most recent touched vertex with triangles left, else the next such
//! vertex in input order. CACHE MODEL: a FIFO post-transform cache of `k` entries (Tipsify's,
//! and [`acmr`]'s): a vertex is a hit when fewer than `k` misses happened since its own last
//! miss. Linear time.
//!
//! [`optimize_vertex_fetch`] renumbers VERTICES in the order the triangles first use them (the
//! unused after, in their old order), so the vertex buffer is read front to back.
//!
//! Both are PERMUTATIONS: the same triangles (with their corners, parts and roles) and the same
//! vertices (with every attribute), only in another order; the creases follow their vertices. A
//! verified mesh stays one. Both are deterministic for a fixed input order and not order-free
//! (an order is their output): start from [`crate::canon`]'s form for a result that depends on
//! the mesh alone.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::ids::{VertexPair, VertexId};
use crate::mesh::{Mesh, MeshVecs, MeshView, ShapeError, built, empty_vecs};
use crate::vecs::{V, adjacency, at, filled, len32, push};

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct CacheOpts<'a, 's> {
    /// The FIFO cache's entries (`k`); 16 to 32 on current GPUs.
    pub cache_size: u32,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct FetchOpts<'a, 's> {
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct AcmrOpts<'s> {
    pub cache_size: u32,
    pub scratch: Alloc<'s>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheError {
    /// A cache of no entries.
    ZeroCache,
    Alloc(AllocFailure),
    /// The mesh built here did not fit together: a bug in this module.
    Built(ShapeError),
}

impl From<AllocFailure> for CacheError {
    fn from(e: AllocFailure) -> CacheError {
        CacheError::Alloc(e)
    }
}

const NONE: u32 = u32::MAX;

/// Tipsify's triangle order: `order[i]` is the input triangle emitted `i`-th.
pub(crate) fn tipsify<'s>(view: &MeshView<'_>, k: u32, s: Alloc<'s>) -> Result<V<'s, u32>, AllocFailure> {
    let tris = view.triangles();
    let n = view.vertex_count();
    let adj = adjacency(tris, n, s)?;
    let mut live = filled(at(n), 0u32, s)?;
    for v in 0..n {
        live[at(v)] = len32(adj.of(v).len());
    }
    let mut stamp = filled(at(n), 0u64, s)?;
    let mut emitted = filled(tris.len(), false, s)?;
    let mut dead_end: V<'_, u32> = Vec::try_with_capacity_in(tris.len() * 3, s)?;
    let mut touched: V<'_, u32> = Vec::new_in(s);
    let mut order: V<'s, u32> = Vec::try_with_capacity_in(tris.len(), s)?;
    let k64 = u64::from(k);
    let mut time = k64 + 1;
    let mut cursor = 0u32;
    let mut fan = if n == 0 { NONE } else { 0 };
    while fan != NONE {
        touched.clear();
        for &t in adj.of(fan) {
            if emitted[at(t)] {
                continue;
            }
            emitted[at(t)] = true;
            push(&mut order, t)?;
            for &v in &tris[at(t)] {
                push(&mut dead_end, v)?;
                push(&mut touched, v)?;
                live[at(v)] -= 1;
                if time - stamp[at(v)] > k64 {
                    stamp[at(v)] = time;
                    time += 1;
                }
            }
        }
        // The next fanning vertex among those just touched.
        let mut best = NONE;
        let mut best_priority = 0u64;
        for &v in touched.iter() {
            if live[at(v)] == 0 {
                continue;
            }
            let age = time - stamp[at(v)];
            let priority = if age + 2 * u64::from(live[at(v)]) <= k64 { age } else { 0 };
            if best == NONE || priority > best_priority {
                best = v;
                best_priority = priority;
            }
        }
        if best == NONE {
            while let Some(v) = dead_end.pop() {
                if live[at(v)] > 0 {
                    best = v;
                    break;
                }
            }
        }
        if best == NONE {
            while cursor < n && live[at(cursor)] == 0 {
                cursor += 1;
            }
            if cursor < n {
                best = cursor;
            }
        }
        fan = best;
    }
    Ok(order)
}

/// `view` with its triangles in Tipsify's order for a FIFO cache of `opts.cache_size`.
pub fn optimize_vertex_cache<'a>(view: &MeshView<'_>, opts: CacheOpts<'a, '_>) -> Result<Mesh<'a>, CacheError> {
    if opts.cache_size == 0 {
        return Err(CacheError::ZeroCache);
    }
    let order = tipsify(view, opts.cache_size, opts.scratch)?;
    let mut out: MeshVecs<'a> = crate::mesh::vecs_of(view, opts.alloc)?;
    out.triangles.clear();
    out.parts.clear();
    out.roles.clear();
    for &t in order.iter() {
        push(&mut out.triangles, view.triangles()[at(t)])?;
        push(&mut out.parts, view.parts()[at(t)])?;
        push(&mut out.roles, view.roles()[at(t)])?;
    }
    built(out).map_err(CacheError::Built)
}

/// `view` with its vertices renumbered in first-use order.
pub fn optimize_vertex_fetch<'a>(view: &MeshView<'_>, opts: FetchOpts<'a, '_>) -> Result<Mesh<'a>, CacheError> {
    let s = opts.scratch;
    let n = view.vertex_count();
    let mut new_index = filled(at(n), NONE, s)?;
    let mut old_of: V<'_, u32> = Vec::try_with_capacity_in(at(n), s)?;
    for tri in view.triangles() {
        for &v in tri {
            if new_index[at(v)] == NONE {
                new_index[at(v)] = len32(old_of.len());
                push(&mut old_of, v)?;
            }
        }
    }
    for v in 0..n {
        if new_index[at(v)] == NONE {
            new_index[at(v)] = len32(old_of.len());
            push(&mut old_of, v)?;
        }
    }
    let mut out: MeshVecs<'a> = empty_vecs(opts.alloc);
    for &v in old_of.iter() {
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
    for tri in view.triangles() {
        push(&mut out.triangles, [new_index[at(tri[0])], new_index[at(tri[1])], new_index[at(tri[2])]])?;
    }
    crate::vecs::extend(&mut out.parts, view.parts())?;
    crate::vecs::extend(&mut out.roles, view.roles())?;
    for edge in view.creases() {
        push(&mut out.creases, VertexPair::new(VertexId(new_index[edge.lo().index()]), VertexId(new_index[edge.hi().index()])))?;
    }
    out.creases.sort_unstable();
    built(out).map_err(CacheError::Built)
}

/// The average cache miss ratio (misses per triangle) of `view`'s triangle order under the FIFO
/// model above: 3.0 at worst, about 0.5 for a regular grid at best.
pub fn acmr(view: &MeshView<'_>, opts: AcmrOpts<'_>) -> Result<f64, CacheError> {
    if opts.cache_size == 0 {
        return Err(CacheError::ZeroCache);
    }
    let k = u64::from(opts.cache_size);
    // `stamp[v]` is the miss count when `v` last missed, plus one (zero: never cached).
    let mut stamp = filled(at(view.vertex_count()), 0u64, opts.scratch)?;
    let mut misses = 0u64;
    for tri in view.triangles() {
        for &v in tri {
            let s = stamp[at(v)];
            // In the cache: among the last `k` misses, `s > misses - k`.
            if s == 0 || misses - s >= k {
                misses += 1;
                stamp[at(v)] = misses;
            }
        }
    }
    let t = view.triangle_count();
    Ok(if t == 0 { 0.0 } else { misses as f64 / f64::from(t) })
}
