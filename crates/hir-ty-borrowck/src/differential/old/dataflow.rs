//! Gen/kill dataflow over the blocks of a body, as rustc's `rustc_mir_dataflow` solves it:
//! each block's effect is one gen set and one kill set, `out = (in & !kill) | gen`, found once
//! from its statements, and a work queue (forwards in reverse postorder, backwards in
//! postorder) joins by union until nothing changes. Effects on one edge only, a call's return
//! or what may be initialized leaving a block, are a callback on the edge.

use std::collections::VecDeque;

use hir_ty_mir::mir::BasicBlockId;

use super::{
    bitset::{BitMatrix, BitSet, Transfer, masks, union_words, words_for},
    cfg::{Cfg, index},
};

/// The effect of each block (or of anything else with a row) as a gen and a kill set: per
/// word, the gen word and the kill word side by side, so applying a row reads one stream.
/// 16 bytes per 64 bits per row.
pub(crate) struct GenKill {
    words: usize,
    data: Vec<[u64; 2]>,
}

impl GenKill {
    /// The identity effect for `rows` rows over sets of `domain` bits.
    pub(crate) fn new(rows: usize, domain: usize) -> Self {
        let words = words_for(domain);
        GenKill { words, data: vec![[0; 2]; rows * words] }
    }

    /// The row as a [`Transfer`]: each effect applied to it comes after those before.
    pub(crate) fn row(&mut self, row: usize) -> Row<'_> {
        Row { data: &mut self.data[row * self.words..(row + 1) * self.words] }
    }

    pub(crate) fn apply(&self, row: usize, state: &mut [u64]) {
        let data = &self.data[row * self.words..(row + 1) * self.words];
        for (word, [gen_, kill]) in state.iter_mut().zip(data) {
            *word = (*word & !kill) | gen_;
        }
    }
}

/// One row of a [`GenKill`], composing what is applied to it in order.
pub(crate) struct Row<'g> {
    data: &'g mut [[u64; 2]],
}

impl Transfer for Row<'_> {
    #[inline]
    fn gen_range(&mut self, range: std::ops::Range<usize>) {
        masks(range, |word, mask| {
            let [gen_, kill] = &mut self.data[word];
            *gen_ |= mask;
            *kill &= !mask;
        });
    }

    #[inline]
    fn kill_range(&mut self, range: std::ops::Range<usize>) {
        masks(range, |word, mask| {
            let [gen_, kill] = &mut self.data[word];
            *kill |= mask;
            *gen_ &= !mask;
        });
    }
}

/// rustc's `WorkQueue`: each block at most once in the queue.
struct WorkQueue {
    queue: VecDeque<BasicBlockId>,
    queued: BitSet,
}

impl WorkQueue {
    fn new(blocks: usize, initial: impl Iterator<Item = BasicBlockId>) -> Self {
        let mut out =
            WorkQueue { queue: VecDeque::with_capacity(blocks), queued: BitSet::new(blocks) };
        for it in initial {
            out.push(it);
        }
        out
    }

    fn push(&mut self, it: BasicBlockId) {
        if self.queued.insert(index(it)) {
            self.queue.push_back(it);
        }
    }

    fn pop(&mut self) -> Option<BasicBlockId> {
        let it = self.queue.pop_front()?;
        self.queued.remove(index(it));
        Some(it)
    }
}

/// The state at the start of each block of a forward analysis whose start block begins with
/// `boundary`: the least fixed point, unreachable blocks empty. `edge(from, to, state)`
/// changes what leaves `from` for `to`.
pub(crate) fn forward(
    cfg: &Cfg<'_>,
    blocks: &GenKill,
    start: BasicBlockId,
    boundary: &BitSet,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId, &mut [u64]),
) -> BitMatrix {
    let count = cfg.reachable.len();
    let mut entry = BitMatrix::new(count, blocks.words * 64);
    if count == 0 {
        return entry;
    }
    for (word, from) in entry.row_mut(index(start)).iter_mut().zip(boundary.words()) {
        *word = *from;
    }
    let mut out = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().copied());
    while let Some(at) = queue.pop() {
        out.copy_from_slice(entry.row(index(at)));
        blocks.apply(index(at), &mut out);
        for &to in cfg.successors(at) {
            along.copy_from_slice(&out);
            edge(at, to, &mut along);
            if union_words(entry.row_mut(index(to)), &along) {
                queue.push(to);
            }
        }
    }
    entry
}

/// The state at the start of each block of a backward analysis, nothing live past the end:
/// the least fixed point, unreachable blocks empty. `edge(from, to, state)` changes what
/// comes back from `to` to the end of `from`.
pub(crate) fn backward(
    cfg: &Cfg<'_>,
    blocks: &GenKill,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId, &mut [u64]),
) -> BitMatrix {
    let count = cfg.reachable.len();
    let mut entry = BitMatrix::new(count, blocks.words * 64);
    let mut exit = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().rev().copied());
    while let Some(at) = queue.pop() {
        exit.fill(0);
        for &to in cfg.successors(at) {
            along.copy_from_slice(entry.row(index(to)));
            edge(at, to, &mut along);
            union_words(&mut exit, &along);
        }
        blocks.apply(index(at), &mut exit);
        if entry.row(index(at)) != exit.as_slice() {
            entry.row_mut(index(at)).copy_from_slice(&exit);
            for &from in cfg.predecessors(at) {
                if cfg.reachable[index(from)] {
                    queue.push(from);
                }
            }
        }
    }
    entry
}
