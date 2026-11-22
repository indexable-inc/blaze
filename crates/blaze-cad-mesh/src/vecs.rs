//! The scratch structures several algorithms share: vectors grown with typed refusals, the
//! position classes ("spots") topology is read on, half-edges sorted into edges, a union-find,
//! and vertex-to-triangle adjacency. Every one is a plain vector in a handed allocator, built in
//! an order fixed by its input, with no hashing.

#![blaze::sans_io]

use blaze_int::Int;
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::geom::{P3, plus_zeros, point_order};

pub(crate) type V<'a, T> = Vec<T, Alloc<'a>>;

/// `x` pushed, or the refusal.
#[inline(always)]
pub(crate) fn push<T>(v: &mut V<'_, T>, x: T) -> Result<(), AllocFailure> {
    if v.len() == v.capacity() {
        v.try_reserve(1)?;
    }
    v.push(x);
    Ok(())
}

/// `len` copies of `value`.
pub(crate) fn filled<T: Copy>(len: usize, value: T, alloc: Alloc<'_>) -> Result<V<'_, T>, AllocFailure> {
    let mut v = Vec::try_with_capacity_in(len, alloc)?;
    v.resize(len, value);
    Ok(v)
}

/// A copy of `src`.
pub(crate) fn copied<'a, T: Copy>(src: &[T], alloc: Alloc<'a>) -> Result<V<'a, T>, AllocFailure> {
    let mut v = Vec::try_with_capacity_in(src.len(), alloc)?;
    v.extend_from_slice(src);
    Ok(v)
}

/// `src` appended.
#[inline]
pub(crate) fn extend<T: Copy>(v: &mut V<'_, T>, src: &[T]) -> Result<(), AllocFailure> {
    v.try_extend_from_slice(src)
}

/// `0, 1, .., len - 1`.
pub(crate) fn counting(len: u32, alloc: Alloc<'_>) -> Result<V<'_, u32>, AllocFailure> {
    let mut v = Vec::try_with_capacity_in(blaze_int::idx(len), alloc)?;
    for i in 0..len {
        v.push(i);
    }
    Ok(v)
}

/// A count of a slice the caller has already bounded by `u32` (every mesh view is).
#[inline]
pub(crate) fn len32(len: usize) -> u32 {
    // A `MeshView` refuses any column longer than `u32::MAX`, and every slice counted here is
    // one of a view's or no longer than one.
    len.narrow_or_stop::<u32>()
}

#[inline]
pub(crate) fn at(i: u32) -> usize {
    blaze_int::idx(i)
}

/// The vertices grouped by position: `of_vertex[v]` is `v`'s spot, the spots numbered in the
/// order of their positions (`geom::point_order`, negative zero as positive), so the numbering is
/// a function of the positions alone. `first[s]` is the least vertex at spot `s`.
pub(crate) struct Spots<'s> {
    pub(crate) of_vertex: V<'s, u32>,
    pub(crate) first: V<'s, u32>,
}

impl Spots<'_> {
    #[inline]
    pub(crate) fn count(&self) -> u32 {
        len32(self.first.len())
    }
}

pub(crate) fn spots<'s>(positions: &[P3], scratch: Alloc<'s>) -> Result<Spots<'s>, AllocFailure> {
    let n = len32(positions.len());
    let mut order = counting(n, scratch)?;
    order.sort_unstable_by(|&a, &b| {
        point_order(plus_zeros(positions[at(a)]), plus_zeros(positions[at(b)])).then(a.cmp(&b))
    });
    let mut of_vertex = filled(at(n), 0u32, scratch)?;
    let mut first: V<'s, u32> = Vec::new_in(scratch);
    let mut last: Option<P3> = None;
    for &v in order.iter() {
        let p = plus_zeros(positions[at(v)]);
        let same = match last {
            Some(q) => point_order(p, q).is_eq(),
            None => false,
        };
        if !same {
            push(&mut first, v)?;
            last = Some(p);
        }
        of_vertex[at(v)] = len32(first.len()) - 1;
    }
    Ok(Spots { of_vertex, first })
}

/// One side of an edge: triangle `tri`'s edge from its corner `slot` to corner `slot + 1`, between
/// spots `lo < hi`; `forward` when it runs from `lo` to `hi`. Sorted, the halves of one edge are
/// neighbours.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Half {
    pub(crate) lo: u32,
    pub(crate) hi: u32,
    pub(crate) tri: u32,
    pub(crate) slot: u8,
    pub(crate) forward: bool,
}

/// Every half-edge of `triangles` over `spot`, sorted by edge then triangle; a triangle side whose
/// two ends are one spot is left out (the verifier names such a triangle before it asks).
pub(crate) fn halves<'s>(triangles: &[[u32; 3]], spot: &[u32], scratch: Alloc<'s>) -> Result<V<'s, Half>, AllocFailure> {
    let mut out: V<'s, Half> = Vec::try_with_capacity_in(triangles.len() * 3, scratch)?;
    for (t, tri) in triangles.iter().enumerate() {
        for slot in 0..3u8 {
            let a = spot[at(tri[usize::from(slot)])];
            let b = spot[at(tri[usize::from((slot + 1) % 3)])];
            if a == b {
                continue;
            }
            let (lo, hi, forward) = if a < b { (a, b, true) } else { (b, a, false) };
            push(&mut out, Half { lo, hi, tri: len32(t), slot, forward })?;
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// The runs of `halves` that are one edge each: `(start, end)` index ranges, in order.
pub(crate) fn edge_runs(halves: &[Half]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut start = 0;
    core::iter::from_fn(move || {
        if start >= halves.len() {
            return None;
        }
        let mut end = start + 1;
        while end < halves.len() && halves[end].lo == halves[start].lo && halves[end].hi == halves[start].hi {
            end += 1;
        }
        let run = (start, end);
        start = end;
        Some(run)
    })
}

/// Union-find whose root is always the least member of its class, so classes and their roots
/// are a function of the unions made, in any order.
pub(crate) struct Dsu<'s> {
    parent: V<'s, u32>,
}

impl<'s> Dsu<'s> {
    pub(crate) fn new(n: u32, scratch: Alloc<'s>) -> Result<Dsu<'s>, AllocFailure> {
        Ok(Dsu { parent: counting(n, scratch)? })
    }

    /// `x`'s root, halving the path on the way (iterative: no recursion).
    pub(crate) fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[at(x)] != x {
            let up = self.parent[at(self.parent[at(x)])];
            self.parent[at(x)] = up;
            x = up;
        }
        x
    }

    pub(crate) fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra < rb {
            self.parent[at(rb)] = ra;
        } else if rb < ra {
            self.parent[at(ra)] = rb;
        }
    }
}

/// Vertex-to-triangle adjacency in compressed rows: the triangles at vertex `v` are
/// `tris[start[v]..start[v + 1]]`, in increasing order.
pub(crate) struct Adjacency<'s> {
    pub(crate) start: V<'s, u32>,
    pub(crate) tris: V<'s, u32>,
}

impl Adjacency<'_> {
    #[inline]
    pub(crate) fn of(&self, v: u32) -> &[u32] {
        &self.tris[at(self.start[at(v)])..at(self.start[at(v) + 1])]
    }
}

pub(crate) fn adjacency<'s>(triangles: &[[u32; 3]], vertices: u32, scratch: Alloc<'s>) -> Result<Adjacency<'s>, AllocFailure> {
    let mut start = filled(at(vertices) + 1, 0u32, scratch)?;
    for tri in triangles {
        for &v in tri {
            start[at(v) + 1] += 1;
        }
    }
    for v in 0..at(vertices) {
        start[v + 1] += start[v];
    }
    let mut fill = copied(&start, scratch)?;
    let mut tris = filled(triangles.len() * 3, 0u32, scratch)?;
    for (t, tri) in triangles.iter().enumerate() {
        for &v in tri {
            tris[at(fill[at(v)])] = len32(t);
            fill[at(v)] += 1;
        }
    }
    Ok(Adjacency { start, tris })
}
