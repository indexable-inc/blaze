//! Region inference: what each region holds, from where liveness says it is live and the
//! outlives constraints between regions, as rustc's `RegionInferenceContext` computes it on
//! the strongly connected components of the constraint graph.

use super::{
    bitset::{BitMatrix, bits, matrix},
    intervals::IntervalSet,
    regions::{Regions, Rv},
    relation::Csr,
};
use crate::mem::{FVec, Frame, filled};

/// The points each region holds at, and the universal regions it outlives: one value for each
/// strongly connected component of the outlives graph, whose regions hold the same. The
/// universals are a bit matrix, a row of `universals / 64` words per component.
pub(super) struct RegionValues<'f> {
    scc: FVec<'f, u32>,
    points: FVec<'f, IntervalSet<'f>>,
    universals: BitMatrix<'f>,
}

impl<'f> RegionValues<'f> {
    pub(super) fn contains_point(&self, rv: Rv, point: usize) -> bool {
        self.points[self.scc[rv] as usize].contains(point)
    }

    pub(super) fn points_of(&self, rv: Rv) -> &IntervalSet<'f> {
        &self.points[self.scc[rv] as usize]
    }

    pub(super) fn universals_of(&self, rv: Rv) -> impl Iterator<Item = usize> + '_ {
        self.universals.iter_row(self.scc[rv] as usize)
    }
}

/// Region inference: the values liveness gives grown along the outlives constraints until
/// each region holds wherever a region it outlives does. As rustc's, it works on the strongly
/// connected components of the graph, each once, those it outlives first.
pub(super) fn infer<'f>(
    frame: &'f Frame,
    regions: &Regions<'_, '_>,
    count: usize,
    points: usize,
    live: &[IntervalSet<'_>],
) -> RegionValues<'f> {
    // `sup: sub` makes `sup` hold what `sub` does: an edge from `sup` to `sub`, appended in
    // `sup` order, so no sort.
    let mut edges = Csr::with_capacity(frame, count, regions.outlives.len());
    for sup in 0..count {
        for sub in regions.outlives.subs_of(count, sup) {
            edges.push(sub as u32);
        }
        edges.end_node();
    }
    let (scc, sccs) = strongly_connected(frame, &edges);
    // Tarjan's algorithm numbers a component after every component it reaches.
    let mut values = filled(frame, sccs, IntervalSet::new(frame));
    let mut universals = matrix(frame, sccs, regions.universals.len());
    for (rv, it) in live.iter().enumerate() {
        values[scc[rv] as usize].union(it);
    }
    for (index, &(rv, _)) in regions.universals.iter().enumerate() {
        if rv >= count {
            continue;
        }
        let at = scc[rv] as usize;
        if points > 0 {
            values[at].insert_range(0, points - 1);
        }
        universals.insert(at, index);
    }
    let members =
        Csr::build(frame, sccs, scc.iter().enumerate().map(|(rv, &at)| (at, rv as u32)));
    let seen = frame.alloc_slice_fill_with(sccs, |_| u32::MAX);
    for at in 0..sccs {
        for &rv in members.of(at) {
            for &sub in edges.of(rv as usize) {
                let from = scc[sub as usize] as usize;
                if from == at || seen[from] == at as u32 {
                    continue;
                }
                seen[from] = at as u32;
                debug_assert!(from < at);
                let (done, rest) = values.split_at_mut(at);
                rest[0].union(&done[from]);
                universals.union_rows(at, from);
            }
        }
    }
    RegionValues { scc, points: values, universals }
}

/// The strongly connected components of a graph, by Tarjan's algorithm without recursion: the
/// component of each node, numbered so that every component a component reaches has a
/// smaller number, and how many there are.
fn strongly_connected<'f>(frame: &'f Frame, edges: &Csr<'_>) -> (FVec<'f, u32>, usize) {
    let count = edges.nodes();
    const NONE: u32 = u32::MAX;
    let index = frame.alloc_slice_fill_with(count, |_| NONE);
    let low = frame.alloc_slice_fill_with(count, |_| 0u32);
    let mut on_stack = bits(frame, count);
    let mut scc = filled(frame, count, NONE);
    // Each node is on each stack at most once, so `count` bounds both.
    let mut stack: FVec<'_, u32> = Vec::with_capacity_in(count, frame);
    let mut next_index = 0u32;
    let mut sccs = 0u32;
    // (node, the next of its edges to follow)
    let mut calls: FVec<'_, (u32, usize)> = Vec::with_capacity_in(count, frame);
    for root in 0..count as u32 {
        if index[root as usize] != NONE {
            continue;
        }
        calls.push((root, 0));
        index[root as usize] = next_index;
        low[root as usize] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack.insert(root as usize);
        while let Some(&mut (node, ref mut next)) = calls.last_mut() {
            let n = node as usize;
            if let Some(&to) = edges.of(n).get(*next) {
                *next += 1;
                let t = to as usize;
                if index[t] == NONE {
                    index[t] = next_index;
                    low[t] = next_index;
                    next_index += 1;
                    stack.push(to);
                    on_stack.insert(t);
                    calls.push((to, 0));
                } else if on_stack.contains(t) {
                    low[n] = low[n].min(index[t]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent as usize] = low[parent as usize].min(low[n]);
            }
            if low[n] == index[n] {
                loop {
                    let member = stack.pop().unwrap();
                    on_stack.remove(member as usize);
                    scc[member as usize] = sccs;
                    if member == node {
                        break;
                    }
                }
                sccs += 1;
            }
        }
    }
    (scc, sccs as usize)
}
