//! A point's subset graph. No component crosses a CFG edge: a subset can travel across
//! that edge only between two origins live at its destination. Closing before that filter
//! preserves paths through a dying intermediate origin.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::ops::Range;

use super::solver::{Exhausted, Meter};
use crate::flow::Origin;

const NONE: usize = usize::MAX;

struct Vertex {
    origin: Origin,
    first_edge: usize,
    preorder: usize,
    low: usize,
    component: usize,
}

#[derive(Clone, Copy)]
struct Edge {
    into: usize,
    next: usize,
}

struct Visit {
    vertex: usize,
    next_edge: usize,
}

struct Group {
    members: Range<usize>,
    outgoing: Range<usize>,
    reach: Range<usize>,
    // Dense among queried target components only.
    target_bit: usize,
    // Source that visited this component in the current condensation or projection.
    marked_by: usize,
    // First member that survives the liveness filter of the current CFG edge.
    representative: Option<Origin>,
}

#[derive(Clone, Copy)]
struct Word {
    index: usize,
    bits: u64,
}

/// One reusable scratch per body. Only the lookup by original origin id spans the
/// body. Graph records and nonzero reach words span the largest single point so far.
pub(super) struct Components<A: Allocator> {
    by_origin: Vec<usize, A>,
    vertices: Vec<Vertex, A>,
    edges: Vec<Edge, A>,
    dfs: Vec<Visit, A>,
    pending: Vec<usize, A>,
    groups: Vec<Group, A>,
    members: Vec<Origin, A>,
    outgoing: Vec<usize, A>,
    reach: Vec<Word, A>,
    target_offsets: Vec<usize, A>,
    target_origins: Vec<Origin, A>,
    scratch: Vec<u64, A>,
    touched: Vec<usize, A>,
}

impl<A: Allocator + Copy> Components<A> {
    pub(super) fn new(alloc: A) -> Self {
        Self {
            by_origin: Vec::new_in(alloc),
            vertices: Vec::new_in(alloc),
            edges: Vec::new_in(alloc),
            dfs: Vec::new_in(alloc),
            pending: Vec::new_in(alloc),
            groups: Vec::new_in(alloc),
            members: Vec::new_in(alloc),
            outgoing: Vec::new_in(alloc),
            reach: Vec::new_in(alloc),
            target_offsets: Vec::new_in(alloc),
            target_origins: Vec::new_in(alloc),
            scratch: Vec::new_in(alloc),
            touched: Vec::new_in(alloc),
        }
    }

    fn vertex(&mut self, origin: Origin, meter: &mut Meter) -> Result<usize, Exhausted> {
        let id = origin.index();
        if id >= self.by_origin.len() {
            meter.charge(id + 1 - self.by_origin.len())?;
            self.by_origin.resize(id + 1, NONE);
        }
        if self.by_origin[id] == NONE {
            self.by_origin[id] = self.vertices.len();
            self.vertices.push(Vertex { origin, first_edge: NONE, preorder: NONE, low: NONE, component: NONE });
        }
        Ok(self.by_origin[id])
    }

    pub(super) fn build(
        &mut self,
        edges: &[(Origin, Origin)],
        meter: &mut Meter,
    ) -> Result<(), Exhausted> {
        meter.charge(self.vertices.len())?;
        for v in &self.vertices {
            self.by_origin[v.origin.index()] = NONE;
        }
        self.vertices.clear();
        self.edges.clear();
        self.dfs.clear();
        self.pending.clear();
        // Usually empty; a budget refusal may have interrupted a row union.
        meter.charge(self.touched.len())?;
        for &word in &self.touched {
            self.scratch[word] = 0;
        }
        self.touched.clear();
        self.groups.clear();
        self.members.clear();
        self.outgoing.clear();
        self.reach.clear();
        meter.charge(edges.len())?;
        for &(from, into) in edges {
            // The diagonal transfers a loan only to itself, never witnesses a
            // placeholder error, and adds no path between distinct origins.
            if from != into {
                let source = self.vertex(from, meter)?;
                let target = self.vertex(into, meter)?;
                let first = self.vertices[source].first_edge;
                self.vertices[source].first_edge = self.edges.len();
                self.edges.push(Edge { into: target, next: first });
            }
        }
        self.find_components(meter)?;
        self.condense(meter)
    }

    fn discover(&mut self, vertex: usize, preorder: usize) {
        let v = &mut self.vertices[vertex];
        v.preorder = preorder;
        v.low = preorder;
        self.pending.push(vertex);
        self.dfs.push(Visit { vertex, next_edge: v.first_edge });
    }

    /// Iterative Tarjan. An edge to a pending vertex uses its discovery number; only
    /// return from a DFS child propagates a low link. A finished SCC has no back edge
    /// to a pending ancestor and its members occupy a suffix of the pending stack.
    fn find_components(&mut self, meter: &mut Meter) -> Result<(), Exhausted> {
        let mut preorder = 0;
        meter.charge(self.vertices.len())?;
        for root in 0..self.vertices.len() {
            if self.vertices[root].preorder != NONE {
                continue;
            }
            self.discover(root, preorder);
            preorder += 1;
            while let Some(frame) = self.dfs.last_mut() {
                meter.charge(1)?;
                let source = frame.vertex;
                if frame.next_edge != NONE {
                    let edge = self.edges[frame.next_edge];
                    frame.next_edge = edge.next;
                    let target_order = self.vertices[edge.into].preorder;
                    if target_order == NONE {
                        self.discover(edge.into, preorder);
                        preorder += 1;
                    } else if self.vertices[edge.into].component == NONE {
                        let v = &mut self.vertices[source];
                        v.low = v.low.min(target_order);
                    }
                    continue;
                }
                self.dfs.pop();
                let low = self.vertices[source].low;
                if low == self.vertices[source].preorder {
                    let first = self.members.len();
                    loop {
                        meter.charge(1)?;
                        let member = self.pending.pop().expect("a Tarjan root is on its pending stack");
                        let v = &mut self.vertices[member];
                        v.component = self.groups.len();
                        self.members.push(v.origin);
                        if member == source {
                            break;
                        }
                    }
                    // The same equivalence class may be discovered through different
                    // edges on later visits. Emit one canonical cycle, so a CFG loop
                    // cannot grow its input just by permuting that cycle's generators.
                    let members = &mut self.members[first..];
                    if members.len() > 1 {
                        meter.sort(members.len())?;
                        members.sort_unstable();
                    }
                    self.groups.push(Group {
                        members: first..self.members.len(),
                        outgoing: 0..0,
                        reach: 0..0,
                        target_bit: NONE,
                        marked_by: NONE,
                        representative: None,
                    });
                }
                if let Some(parent) = self.dfs.last() {
                    let v = &mut self.vertices[parent.vertex];
                    v.low = v.low.min(low);
                }
            }
        }
        Ok(())
    }

    fn condense(&mut self, meter: &mut Meter) -> Result<(), Exhausted> {
        // Tarjan finishes every target before its distinct sources. Reverse to expose
        // source-first order for the loan pass and sink-first order for closure.
        meter.charge(self.vertices.len())?;
        self.groups.reverse();
        for v in &mut self.vertices {
            v.component = self.groups.len() - 1 - v.component;
        }
        for source in 0..self.groups.len() {
            let first = self.outgoing.len();
            for member in self.groups[source].members.clone() {
                let v = self.by_origin[self.members[member].index()];
                let mut edge = self.vertices[v].first_edge;
                while edge != NONE {
                    meter.charge(1)?;
                    let Edge { next, into } = self.edges[edge];
                    let target = self.vertices[into].component;
                    if source != target && self.groups[target].marked_by != source {
                        self.groups[target].marked_by = source;
                        self.outgoing.push(target);
                    }
                    edge = next;
                }
            }
            self.groups[source].outgoing = first..self.outgoing.len();
        }
        Ok(())
    }

    /// Index only components that contain queried placeholder targets. Two targets
    /// need at most one word for each component that reaches either, even if the
    /// condensation is a long chain. Loan transport needs just the DAG from build.
    pub(super) fn index_targets(
        &mut self,
        targets: &[Origin],
        meter: &mut Meter,
    ) -> Result<(), Exhausted> {
        meter.charge(self.groups.len())?;
        self.reach.clear();
        for group in &mut self.groups {
            group.target_bit = NONE;
            group.reach = 0..0;
        }
        meter.charge(targets.len().saturating_mul(3))?;
        self.target_offsets.clear();
        self.target_origins.clear();
        for &origin in targets {
            if let Some(component) = self.component(origin) {
                let bit = &mut self.groups[component].target_bit;
                if *bit == NONE {
                    *bit = self.target_offsets.len();
                    self.target_offsets.push(0);
                }
                self.target_offsets[*bit] += 1;
                self.target_origins.push(origin);
            }
        }
        let columns = self.target_offsets.len();
        if columns == 0 {
            return Ok(());
        }
        let mut end = 0;
        for count in &mut self.target_offsets {
            end += *count;
            *count = end;
        }
        self.target_offsets.push(end);
        for &origin in targets {
            if let Some(c) = self.component(origin) {
                let cursor = &mut self.target_offsets[self.groups[c].target_bit];
                *cursor -= 1;
                self.target_origins[*cursor] = origin;
            }
        }
        let words = columns.div_ceil(64);
        meter.charge(words)?;
        self.scratch.resize(words, 0);
        for source in (0..self.groups.len()).rev() {
            meter.charge(1)?;
            let own = self.groups[source].target_bit;
            if own != NONE {
                self.touched.push(own / 64);
                self.scratch[own / 64] = 1 << (own % 64);
            }
            for &target in &self.outgoing[self.groups[source].outgoing.clone()] {
                meter.charge(1)?;
                let bit = self.groups[target].target_bit;
                if bit != NONE && self.scratch[bit / 64] & (1 << (bit % 64)) != 0 {
                    continue; // A previously joined row included this target and its successors.
                }
                let row = &self.reach[self.groups[target].reach.clone()];
                meter.charge(row.len())?;
                for word in row {
                    let dst = &mut self.scratch[word.index];
                    if *dst == 0 {
                        self.touched.push(word.index);
                    }
                    *dst |= word.bits;
                }
            }
            let first = self.reach.len();
            meter.charge(self.touched.len())?;
            // The diagnostics also ask individual reachability questions. Sorting just the
            // occupied words makes a query logarithmic without scanning empty origin words.
            if self.touched.len() > 1 {
                let width = self.touched.len().ilog2() as usize + 1;
                meter.charge(self.touched.len().saturating_mul(width))?;
                self.touched.sort_unstable();
            }
            for &index in &self.touched {
                self.reach.push(Word { index, bits: core::mem::take(&mut self.scratch[index]) });
            }
            self.groups[source].reach = first..self.reach.len();
            self.touched.clear();
        }
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.groups.len()
    }

    pub(super) fn component(&self, origin: Origin) -> Option<usize> {
        let &vertex = self.by_origin.get(origin.index())?;
        (vertex != NONE).then(|| self.vertices[vertex].component)
    }

    pub(super) fn members(&self, component: usize) -> &[Origin] {
        &self.members[self.groups[component].members.clone()]
    }

    /// Reachability from an origin to a queried target. Its component's own bit means
    /// that every distinct member of the same component reaches that target.
    pub(super) fn reaches(&self, from: Origin, into: Origin) -> bool {
        if from == into {
            return false;
        }
        let (Some(source), Some(target)) = (self.component(from), self.component(into)) else {
            return false;
        };
        let bit = self.groups[target].target_bit;
        if bit == NONE {
            return false;
        }
        let row = &self.reach[self.groups[source].reach.clone()];
        row.binary_search_by_key(&(bit / 64), |word| word.index)
            .is_ok_and(|i| row[i].bits & (1 << (bit % 64)) != 0)
    }

    pub(super) fn successors(&self, component: usize) -> &[usize] {
        &self.outgoing[self.groups[component].outgoing.clone()]
    }

    /// Append generators for the closed subset relation restricted to live endpoints.
    /// Each surviving SCC needs only a cycle through its surviving members. A path
    /// leaving that SCC crosses zero or more dead SCCs to reach its first live SCC:
    /// one edge to that representative suffices, since the rest of the path is
    /// generated by the next live SCC's own frontier.
    pub(super) fn project(
        &mut self,
        live: &[u64],
        out: &mut Vec<(Origin, Origin), A>,
        meter: &mut Meter,
    ) -> Result<(), Exhausted> {
        self.pending.clear();
        meter.charge(self.groups.len())?;
        for group in &mut self.groups {
            group.marked_by = NONE;
            group.representative = None;
            let members = &self.members[group.members.clone()];
            meter.charge(members.len())?;
            let mut previous = None;
            for &member in members {
                if crate::bits::test(live, member.index()) {
                    if let Some(prev) = previous {
                        out.push((prev, member));
                    } else {
                        group.representative = Some(member);
                    }
                    previous = Some(member);
                }
            }
            if let (Some(first), Some(last)) = (group.representative, previous) {
                if last != first {
                    out.push((last, first));
                }
            }
        }
        for source in 0..self.groups.len() {
            let Some(from) = self.groups[source].representative else {
                continue;
            };
            self.pending.push(source);
            while let Some(at) = self.pending.pop() {
                let outgoing = &self.outgoing[self.groups[at].outgoing.clone()];
                meter.charge(1 + outgoing.len())?;
                for &target in outgoing {
                    let group = &mut self.groups[target];
                    if group.marked_by == source {
                        continue;
                    }
                    group.marked_by = source;
                    if let Some(into) = group.representative {
                        out.push((from, into));
                    } else {
                        self.pending.push(target);
                    }
                }
            }
        }
        Ok(())
    }

    /// Visit only reachable, distinct target origins. A sparse placeholder graph
    /// need not probe the Cartesian product of its placeholder endpoints.
    pub(super) fn for_each_target(
        &self, from: Origin, meter: &mut Meter, mut visit: impl FnMut(Origin),
    ) -> Result<(), Exhausted> {
        let Some(c) = self.component(from) else { return Ok(()); };
        let row = &self.reach[self.groups[c].reach.clone()];
        meter.charge(row.len())?;
        for word in row {
            let mut bits = word.bits;
            while bits != 0 {
                let bit = 64 * word.index + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let targets = &self.target_origins[self.target_offsets[bit]..self.target_offsets[bit + 1]];
                meter.charge(targets.len())?;
                for &into in targets {
                    if from != into { visit(into); }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Components, Meter, Origin};
    use std::alloc::Global;
    use std::vec::Vec;

    #[test]
    fn point_components_close_across_words_and_split_at_the_next_point() {
        // Two disjoint long chains, a back edge in the first, and one branch into a
        // separate two-origin cycle. Subsets go down each chain. The back edge
        // makes three origins mutually reachable. The branch is one-way.
        let mut edges: Vec<_> = (0..129)
            .chain(132..195)
            .map(|i| (Origin(i), Origin(i + 1)))
            .collect();
        edges.extend([
            (Origin(66), Origin(64)),
            (Origin(2), Origin(130)),
            (Origin(130), Origin(131)),
            (Origin(131), Origin(130)),
        ]);
        let mut graph = Components::new(Global);
        let mut meter = Meter::new(u64::MAX);
        assert!(graph.build(&edges, &mut meter).is_ok());
        assert!(graph.reach.is_empty());
        let targets: Vec<_> = (0..196).map(Origin).collect();
        assert!(graph.index_targets(&[Origin(129), Origin(195)], &mut meter).is_ok());
        assert!(graph.reaches(Origin(0), Origin(129)));
        assert!(!graph.reaches(Origin(0), Origin(195)));
        assert!(graph.reach.len() <= graph.len());
        assert!(graph.index_targets(&targets, &mut meter).is_ok());
        let mut expected = Vec::new();
        for from in 0..196 {
            for into in 0..196 {
                let reachable = from != into && (
                    (from < into && into < 130)
                    || ((64..=66).contains(&from) && (64..=66).contains(&into))
                    || ((from <= 2 || (130..132).contains(&from)) && (130..132).contains(&into))
                    || (132 <= from && from < into)
                );
                assert_eq!(graph.reaches(Origin(from), Origin(into)), reachable, "{from} -> {into}");
                if reachable {
                    expected.push((Origin(from), Origin(into)));
                }
            }
        }
        let mut expanded = Vec::new();
        for &from in &targets {
            graph.for_each_target(from, &mut meter, |into| expanded.push((from, into))).unwrap();
        }
        expanded.sort_unstable();
        assert_eq!(expanded, expected);
        for source in 0..graph.len() {
            assert!(graph.successors(source).iter().all(|&target| source < target));
        }

        // Losing the middle member of the three-origin cycle still leaves two
        // mutually reachable members. Long paths of dead origins bridge live ends.
        let mut live = [0u64; 4];
        for i in [0, 64, 66, 68, 131, 135, 195] {
            crate::bits::put(&mut live, i, true);
        }
        let mut generators = blaze_own::Vec::new_in(Global);
        assert!(graph.project(&live, &mut generators, &mut meter).is_ok());
        assert_eq!(generators.len(), 6);
        assert!(graph.build(&generators, &mut meter).is_ok());
        assert!(graph.index_targets(&targets, &mut meter).is_ok());
        for from in 0..196 {
            for into in 0..196 {
                let endpoints = (Origin(from), Origin(into));
                let survives = crate::bits::test(&live, endpoints.0.index())
                    && crate::bits::test(&live, endpoints.1.index())
                    && expected.binary_search(&endpoints).is_ok();
                assert_eq!(graph.reaches(endpoints.0, endpoints.1), survives);
            }
        }

        // At a new point only one direction of the old cycle survives. Its two
        // endpoints are now distinct components and the old third endpoint is absent.
        assert!(graph.build(&[(Origin(66), Origin(65))], &mut meter).is_ok());
        assert!(graph.index_targets(&targets, &mut meter).is_ok());
        assert_eq!(graph.len(), 2);
        assert_eq!(graph.component(Origin(64)), None);
        assert!(graph.reaches(Origin(66), Origin(65)));
        assert!(!graph.reaches(Origin(65), Origin(66)));
    }
}
