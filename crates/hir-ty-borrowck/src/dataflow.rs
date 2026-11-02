//! Gen/kill dataflow over the blocks of a body: each block's effect is `(word, gen, kill)`
//! triples ([`Effects`]), `out = (in & !kill) | gen`, and the least fixed point, unreachable
//! blocks empty, is found by difference propagation. A block keeps, besides its entry set, the
//! bits it gained and has not sent on; a visit sends only those, through its kill, to its
//! successors (forwards) or predecessors (backwards), and a block whose entry gains nothing is
//! not queued. Every transfer here, a block's and an edge's ([`Edge`]), is `x -> (x & !k) | g`
//! or a mask, which distributes over union, so sending the parts sends the whole once the
//! constant `g` has gone once: a block's gens and its edges' gens go on its first visit, and
//! every reachable block is visited. The fixed point is the one rustc's
//! `rustc_mir_dataflow` reaches by joining whole sets (it is unique), at a cost of the bits
//! that change, not the rows times the passes.

use blaze_ids::bits::Ones;
use hir_ty_mir::mir::BasicBlockId;

use crate::{
    cfg::{Cfg, index},
    mem::{FDeque, FVec, Frame},
    rows::{
        DENSE_WORDS, Dense, Edge, Effects, LINE, Line, Lined, Rows, Store, ZERO_LINE, line_of,
        lines_for,
    },
};

/// rustc's `WorkQueue`: each block at most once in the queue.
struct WorkQueue<'f> {
    queue: FDeque<'f, BasicBlockId>,
    queued: &'f mut [bool],
}

impl<'f> WorkQueue<'f> {
    fn new(frame: &'f Frame, blocks: usize, initial: impl Iterator<Item = BasicBlockId>) -> Self {
        let mut out = WorkQueue {
            queue: FDeque::with_capacity_in(blocks, frame),
            queued: frame.alloc_slice_fill_with(blocks, |_| false),
        };
        for it in initial {
            out.push(it);
        }
        out
    }

    #[inline]
    fn push(&mut self, it: BasicBlockId) {
        let queued = &mut self.queued[index(it)];
        if !*queued {
            *queued = true;
            self.queue.push_back(it);
        }
    }

    fn pop(&mut self) -> Option<BasicBlockId> {
        let it = self.queue.pop_front()?;
        self.queued[index(it)] = false;
        Some(it)
    }
}

/// A bit per line of each block's pending row: the lines that may hold bits not yet sent, so a
/// visit reads those lines and not the whole row.
struct Marks<'f> {
    per: usize,
    bits: &'f mut [u64],
}

impl<'f> Marks<'f> {
    fn new(frame: &'f Frame, rows: usize, lines: usize) -> Self {
        let per = lines.div_ceil(64);
        Marks { per, bits: frame.alloc_slice_fill_with(rows * per, |_| 0) }
    }

    #[inline]
    fn mark(&mut self, row: usize, line: usize) {
        self.bits[row * self.per + line / 64] |= 1 << (line % 64);
    }

    /// Moves row `row`'s marks into `into`, a row's length.
    #[inline]
    fn take(&mut self, row: usize, into: &mut [u64]) {
        for (into, from) in into.iter_mut().zip(&mut self.bits[row * self.per..][..self.per]) {
            *into = std::mem::take(from);
        }
    }
}

/// Marks in `into` the lines of the words of an effect row.
#[inline]
fn mark_lines(into: &mut [u64], words: &[u32]) {
    for &word in words {
        let line = word as usize / LINE;
        into[line / 64] |= 1 << (line % 64);
    }
}

/// The lines a visit of a block reads: those its pending row marks, and on its first visit
/// also those its effect and its edges' effects gen in, which go once whatever arrives.
#[inline]
fn visit_lines<'e>(
    marks: &mut Marks<'_>,
    row: usize,
    now: &mut [u64],
    first: bool,
    transfer: &Triples<'_>,
    edges: &[(BasicBlockId, Along<'e>)],
) {
    marks.take(row, now);
    if first {
        mark_lines(now, transfer.words);
        for (_, along) in edges {
            if let Along::Effect(it) = along {
                mark_lines(now, it.words);
            }
        }
    }
}

/// A row of [`Effects`] read a line at a time, lines in increasing order.
#[derive(Clone, Copy)]
struct Triples<'e> {
    words: &'e [u32],
    bits: &'e [[u64; 2]],
}

impl Triples<'_> {
    /// Applies the row's words of line `line` to it, their gens only when `gens`.
    #[inline]
    fn line(&mut self, line: usize, bits: &mut Line, gens: bool) {
        let start = (line * LINE) as u32;
        let end = start + LINE as u32;
        let skip = self.words.iter().take_while(|&&it| it < start).count();
        let take = self.words[skip..].iter().take_while(|&&it| it < end).count();
        for (&word, &[gen_, kill]) in
            self.words[skip..skip + take].iter().zip(&self.bits[skip..skip + take])
        {
            let word = &mut bits[word as usize % LINE];
            *word &= !kill;
            if gens {
                *word |= gen_;
            }
        }
        self.words = &self.words[skip + take..];
        self.bits = &self.bits[skip + take..];
    }
}

/// An [`Edge`] read a line at a time.
#[derive(Clone, Copy)]
enum Along<'e> {
    Same,
    Nothing,
    Mask(&'e [u64]),
    Effect(Triples<'e>),
}

impl<'e> Along<'e> {
    fn of(edge: Edge<'e>) -> Self {
        match edge {
            Edge::Same => Along::Same,
            Edge::Nothing => Along::Nothing,
            Edge::Mask(mask) => Along::Mask(mask),
            Edge::Effect { words, bits } => Along::Effect(Triples { words, bits }),
        }
    }

    #[inline]
    fn line(&mut self, line: usize, bits: &mut Line, gens: bool) {
        match self {
            Along::Same => (),
            Along::Nothing => *bits = ZERO_LINE,
            Along::Mask(mask) => {
                for (word, mask) in bits.iter_mut().zip(line_of(mask, line)) {
                    *word &= mask;
                }
            }
            Along::Effect(triples) => triples.line(line, bits, gens),
        }
    }
}

/// The state at the start of each block of a forward analysis whose start block begins with
/// `boundary` (a row's length): the least fixed point, unreachable blocks empty. `edge(from,
/// to)` is what happens to what leaves `from` for `to`.
pub(super) fn forward<'f, 'e>(
    frame: &'f Frame,
    cfg: &Cfg<'_>,
    blocks: &Effects<'_>,
    start: BasicBlockId,
    boundary: &[u64],
    edge: impl FnMut(BasicBlockId, BasicBlockId) -> Edge<'e>,
) -> Rows<'f> {
    if blocks.words() <= DENSE_WORDS {
        Rows::Dense(forward_in(frame, cfg, blocks, start, boundary, edge))
    } else {
        Rows::Lined(forward_in(frame, cfg, blocks, start, boundary, edge))
    }
}

pub(super) fn forward_in<'f, 'e, S: Store<'f>>(
    frame: &'f Frame,
    cfg: &Cfg<'_>,
    blocks: &Effects<'_>,
    start: BasicBlockId,
    boundary: &[u64],
    mut edge: impl FnMut(BasicBlockId, BasicBlockId) -> Edge<'e>,
) -> S {
    let count = cfg.reachable.len();
    let words = blocks.words();
    let lines = lines_for(words);
    let mut entry = S::new(frame, count, words);
    if count == 0 {
        return entry;
    }
    let mut pending = S::new(frame, count, words);
    let mut marks = Marks::new(frame, count, lines);
    let now = frame.alloc_slice_fill_with(marks.per, |_| 0u64);
    for line in 0..lines {
        let bits = line_of(boundary, line);
        if bits != ZERO_LINE {
            entry.merge(index(start), line, &bits);
            pending.merge(index(start), line, &bits);
            marks.mark(index(start), line);
        }
    }
    let visited = frame.alloc_slice_fill_with(count, |_| false);
    let mut out: FVec<'_, (BasicBlockId, Along<'e>)> = FVec::new_in(frame);
    let mut queue = WorkQueue::new(frame, count, cfg.order.iter().copied());
    while let Some(at) = queue.pop() {
        let first = !std::mem::replace(&mut visited[index(at)], true);
        out.clear();
        out.extend(cfg.successors(at).iter().map(|&to| (to, Along::of(edge(at, to)))));
        let (words, bits) = blocks.of(index(at));
        let mut transfer = Triples { words, bits };
        visit_lines(&mut marks, index(at), now, first, &transfer, &out);
        for line in Ones::<usize>::new(&*now) {
            let mut sent = ZERO_LINE;
            pending.take(index(at), line, &mut sent);
            transfer.line(line, &mut sent, first);
            if !first && sent == ZERO_LINE {
                continue;
            }
            for (to, along) in out.iter_mut() {
                let mut bits = sent;
                along.line(line, &mut bits, first);
                let new = entry.merge(index(*to), line, &bits);
                if new != ZERO_LINE {
                    pending.merge(index(*to), line, &new);
                    marks.mark(index(*to), line);
                    queue.push(*to);
                }
            }
        }
    }
    entry
}

/// The state at the start of each block of a backward analysis, nothing live past the end:
/// the least fixed point, unreachable blocks empty. `edge(from, to)` is what happens to what
/// comes back from `to` to the end of `from`.
pub(super) fn backward<'f, 'e>(
    frame: &'f Frame,
    cfg: &Cfg<'_>,
    blocks: &Effects<'_>,
    edge: impl FnMut(BasicBlockId, BasicBlockId) -> Edge<'e>,
) -> Rows<'f> {
    if blocks.words() <= DENSE_WORDS {
        Rows::Dense(backward_in(frame, cfg, blocks, edge))
    } else {
        Rows::Lined(backward_in(frame, cfg, blocks, edge))
    }
}

pub(super) fn backward_in<'f, 'e, S: Store<'f>>(
    frame: &'f Frame,
    cfg: &Cfg<'_>,
    blocks: &Effects<'_>,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId) -> Edge<'e>,
) -> S {
    let count = cfg.reachable.len();
    let words = blocks.words();
    let lines = lines_for(words);
    let mut entry = S::new(frame, count, words);
    if count == 0 {
        return entry;
    }
    // What has reached each block's exit and not yet been through it.
    let mut pending = S::new(frame, count, words);
    let mut marks = Marks::new(frame, count, lines);
    let now = frame.alloc_slice_fill_with(marks.per, |_| 0u64);
    let visited = frame.alloc_slice_fill_with(count, |_| false);
    let mut into: FVec<'_, (BasicBlockId, Along<'e>)> = FVec::new_in(frame);
    let mut queue = WorkQueue::new(frame, count, cfg.order.iter().rev().copied());
    while let Some(at) = queue.pop() {
        let first = !std::mem::replace(&mut visited[index(at)], true);
        into.clear();
        into.extend(
            cfg.predecessors(at)
                .iter()
                .filter(|&&from| cfg.is_reachable(from))
                .map(|&from| (from, Along::of(edge(from, at)))),
        );
        let (words, bits) = blocks.of(index(at));
        let mut transfer = Triples { words, bits };
        visit_lines(&mut marks, index(at), now, first, &transfer, &into);
        for line in Ones::<usize>::new(&*now) {
            let mut exit = ZERO_LINE;
            pending.take(index(at), line, &mut exit);
            transfer.line(line, &mut exit, first);
            let new = entry.merge(index(at), line, &exit);
            if !first && new == ZERO_LINE {
                continue;
            }
            for (from, along) in into.iter_mut() {
                let mut bits = new;
                along.line(line, &mut bits, first);
                if pending.merge(index(*from), line, &bits) != ZERO_LINE {
                    marks.mark(index(*from), line);
                    queue.push(*from);
                }
            }
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame};

    use super::{backward_in, forward_in};
    use crate::{
        bitset::Transfer,
        cfg::{Cfg, block, index},
        mem::Frame,
        rows::{Dense, Edge, Effects, Lined, Store},
    };

    /// What one point does: ranges it kills or gens, in order.
    #[derive(Clone, Default)]
    struct Effect {
        ops: Vec<(bool, Range<usize>)>,
    }

    impl Effect {
        fn apply(&self, state: &mut [bool]) {
            for (gen_, range) in &self.ops {
                state[range.clone()].fill(*gen_);
            }
        }
    }

    fn effect(rng: &mut SplitMix64, domain: usize) -> Effect {
        let mut out = Effect::default();
        for _ in 0..rng.index(4) {
            let from = rng.index(domain);
            // Mostly single bits, as statements touch; sometimes a run of lines, as a
            // storage-dead of a large place kills.
            let len = match rng.index(6) {
                0 => rng.index(domain - from + 1),
                _ => 1,
            };
            out.ops.push((rng.index(2) == 0, from..from + len));
        }
        out
    }

    enum EdgeKind {
        Effect(Effect),
        Mask(Vec<bool>),
        Nothing,
    }

    struct Case<'f> {
        cfg: Cfg<'f>,
        domain: usize,
        /// Each block's points, in the order the block runs them.
        points: Vec<Vec<Effect>>,
        edges: Vec<((usize, usize), EdgeKind)>,
        boundary: Vec<usize>,
    }

    fn case<'f>(
        frame: &'f Frame,
        rng: &mut SplitMix64,
        max_blocks: usize,
        max_domain: usize,
    ) -> Case<'f> {
        let blocks = 1 + rng.index(max_blocks);
        let domain = 1 + rng.index(max_domain);
        let mut edges = Vec::new();
        for b in 1..blocks {
            if rng.index(5) != 0 {
                edges.push((rng.index(b), b));
            }
        }
        for _ in 0..rng.index(blocks * 2 + 1) {
            edges.push((rng.index(blocks), rng.index(blocks)));
        }
        let lens: Vec<usize> = (0..blocks).map(|_| 1 + rng.index(5)).collect();
        let cfg = Cfg::from_edges(frame, &lens, &edges);
        let points =
            lens.iter().map(|&len| (0..len).map(|_| effect(rng, domain)).collect()).collect();
        let mut kinds = Vec::new();
        for (from, to) in edges {
            if kinds.iter().any(|(it, _)| *it == (from, to)) {
                continue;
            }
            let kind = match rng.index(6) {
                0 | 1 => EdgeKind::Effect(effect(rng, domain)),
                2 => EdgeKind::Mask((0..domain).map(|_| rng.index(3) != 0).collect()),
                3 => EdgeKind::Nothing,
                _ => continue,
            };
            kinds.push(((from, to), kind));
        }
        let boundary = (0..rng.index(4)).map(|_| rng.index(domain)).collect();
        Case { cfg, domain, points, edges: kinds, boundary }
    }

    fn edge_kind<'a>(case: &'a Case<'_>, from: usize, to: usize) -> Option<&'a EdgeKind> {
        case.edges.iter().find(|(it, _)| *it == (from, to)).map(|(_, it)| it)
    }

    fn cross(case: &Case<'_>, from: usize, to: usize, state: &mut [bool]) {
        match edge_kind(case, from, to) {
            None => (),
            Some(EdgeKind::Effect(it)) => it.apply(state),
            Some(EdgeKind::Mask(mask)) => {
                for (it, keep) in state.iter_mut().zip(mask) {
                    *it &= keep;
                }
            }
            Some(EdgeKind::Nothing) => state.fill(false),
        }
    }

    fn as_set(state: &[bool]) -> Vec<usize> {
        state.iter().enumerate().filter(|(_, it)| **it).map(|(at, _)| at).collect()
    }

    fn reachable(case: &Case<'_>) -> Vec<usize> {
        case.cfg.order.iter().map(|&it| index(it)).collect()
    }

    /// The forward fixed point taken a point at a time, every reachable point again until
    /// nothing changes.
    fn naive_forward(case: &Case<'_>) -> Vec<Vec<usize>> {
        let blocks = case.points.len();
        let mut entry = vec![vec![false; case.domain]; blocks];
        for &it in &case.boundary {
            entry[0][it] = true;
        }
        let order = reachable(case);
        loop {
            let mut changed = false;
            for &b in &order {
                let mut state = entry[b].clone();
                for point in &case.points[b] {
                    point.apply(&mut state);
                }
                for &to in case.cfg.successors(block(b)) {
                    let mut along = state.clone();
                    cross(case, b, index(to), &mut along);
                    for (into, from) in entry[index(to)].iter_mut().zip(&along) {
                        changed |= *from && !*into;
                        *into |= from;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        entry.iter().map(|it| as_set(it)).collect()
    }

    fn naive_backward(case: &Case<'_>) -> Vec<Vec<usize>> {
        let blocks = case.points.len();
        let mut entry = vec![vec![false; case.domain]; blocks];
        let order = reachable(case);
        loop {
            let mut changed = false;
            for &b in order.iter().rev() {
                let mut state = vec![false; case.domain];
                for &to in case.cfg.successors(block(b)) {
                    let mut along = entry[index(to)].clone();
                    cross(case, b, index(to), &mut along);
                    for (into, from) in state.iter_mut().zip(&along) {
                        *into |= from;
                    }
                }
                for point in case.points[b].iter().rev() {
                    point.apply(&mut state);
                }
                if state != entry[b] {
                    entry[b] = state;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        entry.iter().map(|it| as_set(it)).collect()
    }

    fn compose(row: &mut impl Transfer, point: &Effect) {
        for (gen_, range) in &point.ops {
            if *gen_ {
                row.gen_range(range.clone());
            } else {
                row.kill_range(range.clone());
            }
        }
    }

    /// Each block's effect composed point by point, as the analyses build theirs.
    fn composed<'f>(frame: &'f Frame, case: &Case<'_>, backward: bool) -> Effects<'f> {
        let mut out = Effects::new(frame, case.points.len(), case.domain);
        for (b, points) in case.points.iter().enumerate() {
            let mut row = out.row(b);
            if backward {
                points.iter().rev().for_each(|it| compose(&mut row, it));
            } else {
                points.iter().for_each(|it| compose(&mut row, it));
            }
        }
        out
    }

    /// The case's edges: their effects as rows, their masks as words.
    struct Edges<'f> {
        effects: Effects<'f>,
        masks: Vec<Vec<u64>>,
    }

    fn edges<'f>(frame: &'f Frame, case: &Case<'_>, words: usize) -> Edges<'f> {
        let mut effects = Effects::new(frame, case.edges.len(), case.domain);
        let mut masks = Vec::new();
        for (at, (_, kind)) in case.edges.iter().enumerate() {
            let mut mask = vec![0u64; words];
            match kind {
                EdgeKind::Effect(it) => compose(&mut effects.row(at), it),
                EdgeKind::Mask(keep) => {
                    for (bit, _) in keep.iter().enumerate().filter(|(_, it)| **it) {
                        mask[bit / 64] |= 1 << (bit % 64);
                    }
                }
                EdgeKind::Nothing => (),
            }
            masks.push(mask);
        }
        Edges { effects, masks }
    }

    fn edge<'a>(case: &Case<'_>, edges: &'a Edges<'_>, from: usize, to: usize) -> Edge<'a> {
        match case.edges.iter().position(|(it, _)| *it == (from, to)) {
            None => Edge::Same,
            Some(at) => match case.edges[at].1 {
                EdgeKind::Effect(_) => edges.effects.edge(at),
                EdgeKind::Mask(_) => Edge::Mask(&edges.masks[at]),
                EdgeKind::Nothing => Edge::Nothing,
            },
        }
    }

    fn sets<'f, S: Store<'f>>(rows: &S, blocks: usize, words: usize) -> Vec<Vec<usize>> {
        let mut out = vec![0u64; words];
        (0..blocks)
            .map(|b| {
                rows.load(b, &mut out);
                crate::bitset::iter_words(&out).collect()
            })
            .collect()
    }

    fn forward_case<'f, S: Store<'f>>(frame: &'f Frame, case: &Case<'_>) -> Vec<Vec<usize>> {
        let blocks = composed(frame, case, false);
        let words = blocks.words();
        let edges = edges(frame, case, words);
        let mut boundary = vec![0u64; words];
        for &it in &case.boundary {
            boundary[it / 64] |= 1 << (it % 64);
        }
        let entry: S = forward_in(frame, &case.cfg, &blocks, block(0), &boundary, |from, to| {
            edge(case, &edges, index(from), index(to))
        });
        sets(&entry, case.points.len(), words)
    }

    fn backward_case<'f, S: Store<'f>>(frame: &'f Frame, case: &Case<'_>) -> Vec<Vec<usize>> {
        let blocks = composed(frame, case, true);
        let words = blocks.words();
        let edges = edges(frame, case, words);
        let entry: S = backward_in(frame, &case.cfg, &blocks, |from, to| {
            edge(case, &edges, index(from), index(to))
        });
        sets(&entry, case.points.len(), words)
    }

    /// Runs `check` on random cases of up to `blocks` blocks and `domain` bits, a frame each.
    fn rounds(
        seed: u64,
        count: usize,
        blocks: usize,
        domain: usize,
        check: impl for<'f> Fn(&'f Frame, usize, &Case<'f>),
    ) {
        let mut rng = SplitMix64::new(seed);
        for round in 0..count {
            frame::task(Tag::Mir, |frame| {
                let case = case(frame, &mut rng, blocks, domain);
                check(frame, round, &case);
            });
        }
    }

    #[test]
    fn forward_matches_a_point_at_a_time() {
        rounds(0x2545_f491_4f6c_dd1d, 400, 16, 150, |frame, round, case| {
            let want = naive_forward(case);
            assert_eq!(forward_case::<Dense<'_>>(frame, case), want, "dense, round {round}");
            assert_eq!(forward_case::<Lined<'_>>(frame, case), want, "lined, round {round}");
        });
    }

    #[test]
    fn backward_matches_a_point_at_a_time() {
        rounds(0x853c_49e6_748f_ea9b, 400, 16, 150, |frame, round, case| {
            let want = naive_backward(case);
            assert_eq!(backward_case::<Dense<'_>>(frame, case), want, "dense, round {round}");
            assert_eq!(backward_case::<Lined<'_>>(frame, case), want, "lined, round {round}");
        });
    }

    /// Domains of many lines, where uniform lines stay slots and runs fill whole lines.
    #[test]
    fn large_domains_match_a_point_at_a_time() {
        rounds(0x9e37_79b9_7f4a_7c15, 40, 40, 3000, |frame, round, case| {
            let forward = naive_forward(case);
            let backward = naive_backward(case);
            assert_eq!(forward_case::<Lined<'_>>(frame, case), forward, "round {round}");
            assert_eq!(backward_case::<Lined<'_>>(frame, case), backward, "round {round}");
        });
    }

    /// Both fixpoints over a large domain, rows lined and lines marked, take nothing from the
    /// global allocator: every buffer is in the frame.
    #[test]
    fn a_fixpoint_takes_no_global_memory() {
        rounds(0x6a09_e667_f3bc_c908, 20, 40, 3000, |frame, _, case| {
            let blocks = composed(frame, case, false);
            let back = composed(frame, case, true);
            let words = blocks.words();
            let edges = edges(frame, case, words);
            let mut boundary = vec![0u64; words];
            for &it in &case.boundary {
                boundary[it / 64] |= 1 << (it % 64);
            }
            blaze_alloc_count::assert_at_most(0, "a forward and a backward fixpoint", || {
                let _: Lined<'_> =
                    forward_in(frame, &case.cfg, &blocks, block(0), &boundary, |from, to| {
                        edge(case, &edges, index(from), index(to))
                    });
                let _: Lined<'_> = backward_in(frame, &case.cfg, &back, |from, to| {
                    edge(case, &edges, index(from), index(to))
                });
            });
        });
    }

    /// A block's effect composed from ranges does to any set what applying them one after
    /// another does.
    #[test]
    fn composed_ranges_match_walking() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for round in 0..2000 {
            frame::task(Tag::Mir, |frame| {
                let domain = 1 + rng.index(300);
                let mut walked: Vec<bool> = (0..domain).map(|_| rng.index(2) == 0).collect();
                let mut start = vec![0u64; domain.div_ceil(64)];
                for (bit, _) in walked.iter().enumerate().filter(|(_, it)| **it) {
                    start[bit / 64] |= 1 << (bit % 64);
                }
                let mut transfer = Effects::new(frame, 1, domain);
                let point = effect(&mut rng, domain);
                compose(&mut transfer.row(0), &point);
                point.apply(&mut walked);
                transfer.apply(0, &mut start);
                let got: Vec<usize> = crate::bitset::iter_words(&start).collect();
                assert_eq!(got, as_set(&walked), "round {round}");
            });
        }
    }
}
