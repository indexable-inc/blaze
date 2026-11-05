//! The control flow graph of a body as the borrow checker walks it: every edge, the imaginary
//! ones of `FalseEdge` and `FalseUnwind` included, and a number for each location.

use la_arena::RawIdx;
use smallvec::SmallVec;

use hir_ty_mir::mir::{BasicBlockId, MirBody};

use super::relation::Csr;
use crate::mem::{FSet, FVec, Frame, collect, filled, set};

/// A statement of a block, or its terminator when `statement` is the number of statements.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub(super) struct Location {
    pub(super) block: BasicBlockId,
    pub(super) statement: usize,
}

/// The graph as two CSRs over block indices: 4 bytes per block and 4 per edge each way, a
/// block's successors or predecessors one slice.
pub(super) struct Cfg<'f> {
    /// The number of the first location of each block, by its index.
    starts: FVec<'f, usize>,
    pub(super) points: usize,
    successors: Csr<'f, BasicBlockId>,
    predecessors: Csr<'f, BasicBlockId>,
    /// The blocks reachable from the start, in reverse postorder.
    pub(super) order: FVec<'f, BasicBlockId>,
    pub(super) reachable: FVec<'f, bool>,
}

pub(super) fn index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

pub(super) fn block(index: usize) -> BasicBlockId {
    BasicBlockId::from_raw(RawIdx::from_u32(index as u32))
}

impl<'f> Cfg<'f> {
    pub(super) fn new(frame: &'f Frame, body: &MirBody<'_>) -> Self {
        let count = body.basic_blocks.len();
        let mut starts = Vec::with_capacity_in(count, frame);
        let mut points = 0;
        let mut edges: FVec<'_, (u32, BasicBlockId)> = Vec::with_capacity_in(count * 2, frame);
        for (id, data) in body.basic_blocks.iter() {
            starts.push(points);
            points += data.statements.len() + 1;
            if let Some(terminator) = &data.terminator {
                let first = edges.len();
                for target in terminator.kind.all_successors() {
                    if !edges[first..].iter().any(|&(_, it)| it == target) {
                        edges.push((index(id) as u32, target));
                    }
                }
            }
        }
        // The edges to cleanup blocks, each once, after the block's own; those are in block
        // order, so a block's are found by two binary searches.
        let own = edges.len();
        let mut unwinds: FSet<'_, (u32, BasicBlockId)> = set(frame);
        for &(from, _, cleanup) in &body.panic_unwinds {
            let edge = (index(from) as u32, cleanup);
            let low = edges[..own].partition_point(|&(it, _)| it < edge.0);
            let high = edges[..own].partition_point(|&(it, _)| it <= edge.0);
            if !edges[low..high].contains(&edge) && unwinds.insert(edge) {
                edges.push(edge);
            }
        }
        Self::of_edges(frame, starts, points, &edges, body.start_block)
    }

    /// The graph from its edges, each once, in the order a block's successors and a block's
    /// predecessors are listed.
    fn of_edges(
        frame: &'f Frame,
        starts: FVec<'f, usize>,
        points: usize,
        edges: &[(u32, BasicBlockId)],
        start: BasicBlockId,
    ) -> Self {
        let count = starts.len();
        let successors = Csr::build(frame, count, edges.iter().copied());
        let predecessors = Csr::build(
            frame,
            count,
            edges.iter().map(|&(from, to)| (index(to) as u32, block(from as usize))),
        );
        let (order, reachable) = reverse_postorder(frame, &successors, start);
        Cfg { starts, points, successors, predecessors, order, reachable }
    }

    /// The graph of blocks of `lens[b]` points each with these edges, from block 0: for the
    /// tests of the analyses over it.
    #[cfg(test)]
    pub(super) fn from_edges(frame: &'f Frame, lens: &[usize], edges: &[(usize, usize)]) -> Self {
        let mut starts = Vec::with_capacity_in(lens.len(), frame);
        let mut points = 0;
        for &len in lens {
            starts.push(points);
            points += len;
        }
        let mut unique: FVec<'_, (u32, BasicBlockId)> = Vec::new_in(frame);
        let mut seen = crate::mem::set(frame);
        for &(from, to) in edges {
            if seen.insert((from, to)) {
                unique.push((from as u32, block(to)));
            }
        }
        Self::of_edges(frame, starts, points, &unique, block(0))
    }

    pub(super) fn successors(&self, block: BasicBlockId) -> &[BasicBlockId] {
        self.successors.of(index(block))
    }

    pub(super) fn predecessors(&self, block: BasicBlockId) -> &[BasicBlockId] {
        self.predecessors.of(index(block))
    }

    pub(super) fn point(&self, location: Location) -> usize {
        self.starts[index(location.block)] + location.statement
    }

    pub(super) fn location(&self, point: usize) -> Location {
        let block = self.starts.partition_point(|&start| start <= point) - 1;
        Location { block: self::block(block), statement: point - self.starts[block] }
    }

    pub(super) fn block_start(&self, block: BasicBlockId) -> usize {
        self.starts[index(block)]
    }

    /// The last location of `block`, its terminator.
    pub(super) fn block_end(&self, body: &MirBody<'_>, block: BasicBlockId) -> usize {
        self.starts[index(block)] + body.basic_blocks[block].statements.len()
    }

    pub(super) fn is_reachable(&self, block: BasicBlockId) -> bool {
        self.reachable[index(block)]
    }

    /// The locations control may come to `location` from.
    pub(super) fn predecessor_locations(
        &self,
        body: &MirBody<'_>,
        location: Location,
    ) -> SmallVec<[Location; 4]> {
        if location.statement > 0 {
            // alloc: inline; asked only while an error is explained
            return smallvec::smallvec![Location { statement: location.statement - 1, ..location }];
        }
        self.predecessors(location.block)
            .iter()
            .filter(|it| self.is_reachable(**it))
            .map(|&block| Location { block, statement: body.basic_blocks[block].statements.len() })
            .collect() // alloc: four inline; asked only while an error is explained
    }
}

/// The blocks reachable from `start` in reverse postorder, and which are. rustc's `Postorder`
/// takes each block's successors from the last, so its reverse postorder, the order errors are
/// found in, has a loop's exit before its body.
fn reverse_postorder<'f>(
    frame: &'f Frame,
    successors: &Csr<'_, BasicBlockId>,
    start: BasicBlockId,
) -> (FVec<'f, BasicBlockId>, FVec<'f, bool>) {
    let count = successors.nodes();
    let mut reachable = filled(frame, count, false);
    let mut postorder = Vec::with_capacity_in(count, frame);
    let mut stack: FVec<'_, (BasicBlockId, usize)> = Vec::with_capacity_in(count, frame);
    stack.push((start, 0usize));
    reachable[index(start)] = true;
    while let Some((block, next)) = stack.last_mut() {
        let block = *block;
        let all = successors.of(index(block));
        if let Some(&target) = all.len().checked_sub(*next + 1).map(|it| &all[it]) {
            *next += 1;
            if !reachable[index(target)] {
                reachable[index(target)] = true;
                stack.push((target, 0));
            }
        } else {
            postorder.push(block);
            stack.pop();
        }
    }
    postorder.reverse();
    (postorder, reachable)
}

/// The dominator tree of the reachable blocks, as rustc's: immediate dominators by semi-NCA
/// (Lengauer and Tarjan's semidominators, then nearest common ancestors), and each block's
/// entry and exit in a walk of the tree, so that whether one dominates another is two
/// comparisons. The iterative algorithm walked dominator chains for every predecessor, which a
/// long body of calls, each unwinding to one cleanup, made quadratic.
pub(super) struct Dominators<'f> {
    /// Each block's entry and exit in the walk of the tree; `usize::MAX` when unreachable.
    enter: FVec<'f, usize>,
    exit: FVec<'f, usize>,
}

const NONE: usize = usize::MAX;

impl<'f> Dominators<'f> {
    pub(super) fn new(frame: &'f Frame, cfg: &Cfg<'_>) -> Self {
        let start = cfg.order.first().map(|&it| index(it));
        Self::of_graph(
            frame,
            cfg.reachable.len(),
            start,
            |it| cfg.successors.of(it),
            |it| cfg.predecessors.of(it),
        )
    }

    fn of_graph<'g>(
        frame: &'f Frame,
        count: usize,
        start: Option<usize>,
        successors: impl Fn(usize) -> &'g [BasicBlockId],
        predecessors: impl Fn(usize) -> &'g [BasicBlockId],
    ) -> Self {
        let mut enter = filled(frame, count, NONE);
        let mut exit = filled(frame, count, NONE);
        let Some(start) = start else { return Dominators { enter, exit } };
        // Depth-first preorder: `pre` of each block, `vertex` and `parent` by that number.
        let mut pre = filled(frame, count, NONE);
        let mut vertex = Vec::with_capacity_in(count, frame);
        let mut parent = Vec::with_capacity_in(count, frame);
        let mut stack: FVec<'_, (usize, usize)> = Vec::with_capacity_in(count, frame);
        stack.push((start, NONE));
        while let Some((block, from)) = stack.pop() {
            if pre[block] != NONE {
                continue;
            }
            pre[block] = vertex.len();
            vertex.push(block);
            parent.push(from);
            for &next in successors(block).iter().rev() {
                if pre[index(next)] == NONE {
                    stack.push((index(next), pre[block]));
                }
            }
        }
        let n = vertex.len();
        let mut semi = collect(frame, 0..n);
        let mut label = collect(frame, 0..n);
        let mut ancestor = filled(frame, n, NONE);
        let mut path: FVec<'_, usize> = Vec::with_capacity_in(n, frame);
        for w in (1..n).rev() {
            for &pred in predecessors(vertex[w]) {
                let v = pre[index(pred)];
                if v == NONE {
                    continue;
                }
                // The least semidominator on the forest's path from `v` up, compressed.
                let u = if ancestor[v] == NONE {
                    v
                } else {
                    let mut x = v;
                    while ancestor[ancestor[x]] != NONE {
                        path.push(x);
                        x = ancestor[x];
                    }
                    while let Some(x) = path.pop() {
                        let up = ancestor[x];
                        if semi[label[up]] < semi[label[x]] {
                            label[x] = label[up];
                        }
                        ancestor[x] = ancestor[up];
                    }
                    label[v]
                };
                semi[w] = semi[w].min(semi[u]);
            }
            ancestor[w] = parent[w];
        }
        let mut idom = parent;
        for w in 1..n {
            let mut x = idom[w];
            while x > semi[w] {
                x = idom[x];
            }
            idom[w] = x;
        }
        let children = Csr::build(frame, n, (1..n).map(|w| (idom[w] as u32, w as u32)));
        let mut time = 0;
        let mut stack: FVec<'_, (usize, bool)> = Vec::with_capacity_in(2 * n, frame);
        stack.push((0, false));
        while let Some((w, done)) = stack.pop() {
            if done {
                exit[vertex[w]] = time;
                time += 1;
                continue;
            }
            enter[vertex[w]] = time;
            time += 1;
            stack.push((w, true));
            stack.extend(children.of(w).iter().map(|&it| (it as usize, false)));
        }
        Dominators { enter, exit }
    }

    pub(super) fn dominates(&self, a: BasicBlockId, b: BasicBlockId) -> bool {
        let (a, b) = (index(a), index(b));
        if self.enter[b] == NONE {
            return false;
        }
        a == b
            || (self.enter[a] != NONE
                && self.enter[a] <= self.enter[b]
                && self.exit[b] <= self.exit[a])
    }

    pub(super) fn location_dominates(&self, a: Location, b: Location) -> bool {
        if a.block == b.block {
            a.statement <= b.statement
        } else {
            self.dominates(a.block, b.block)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Dominators, block, index};
    use blaze_ids::rng::SplitMix64;
    use hir_ty_mir::mir::BasicBlockId;

    /// Whether `a` dominates `b`, from each block's set of dominators found to a fixed point.
    fn naive(count: usize, preds: &[Vec<BasicBlockId>], reachable: &[bool]) -> Vec<Vec<bool>> {
        let mut dom = vec![vec![true; count]; count];
        dom[0] = (0..count).map(|it| it == 0).collect();
        let mut changed = true;
        while changed {
            changed = false;
            for b in 1..count {
                if !reachable[b] {
                    continue;
                }
                let mut new = vec![true; count];
                for &p in &preds[b] {
                    if reachable[index(p)] {
                        for (it, d) in new.iter_mut().zip(&dom[index(p)]) {
                            *it &= d;
                        }
                    }
                }
                new[b] = true;
                if new != dom[b] {
                    dom[b] = new;
                    changed = true;
                }
            }
        }
        dom
    }

    #[test]
    fn match_the_dominator_sets() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let mut rng = SplitMix64::new(0x2545f4914f6cdd1d);
            for round in 0..500 {
                let count = 1 + rng.index(24);
                let edges = rng.index(count * 3 + 1);
                let mut succs = vec![Vec::new(); count];
                let mut preds = vec![Vec::new(); count];
                // A chain from the start keeps most blocks reachable, as a body's are.
                for b in 1..count {
                    if rng.index(4) != 0 {
                        let from = rng.index(b);
                        succs[from].push(block(b));
                        preds[b].push(block(from));
                    }
                }
                for _ in 0..edges {
                    let (from, to) = (rng.index(count), rng.index(count));
                    if !succs[from].contains(&block(to)) {
                        succs[from].push(block(to));
                        preds[to].push(block(from));
                    }
                }
                let mut reachable = vec![false; count];
                let mut stack = vec![0];
                reachable[0] = true;
                while let Some(b) = stack.pop() {
                    for &s in &succs[b] {
                        if !reachable[index(s)] {
                            reachable[index(s)] = true;
                            stack.push(index(s));
                        }
                    }
                }
                let fast = Dominators::of_graph(
                    frame,
                    count,
                    Some(0),
                    |it| succs[it].as_slice(),
                    |it| preds[it].as_slice(),
                );
                let want = naive(count, &preds, &reachable);
                for a in 0..count {
                    for b in 0..count {
                        let want = reachable[b] && reachable[a] && want[b][a];
                        assert_eq!(
                            fast.dominates(block(a), block(b)),
                            want,
                            "round {round}: {a} dom {b}"
                        );
                    }
                }
            }
        });
    }
}
