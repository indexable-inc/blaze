//! Facts as columnar relations: rows `(kind, a, b)` gathered as they are found, then frozen
//! into a CSR over `(kind, a)`, so the `b`s of one `a` are one range: the moves at a point, the
//! loans of a local, the uses at a point. Freezing is a counting sort (count, prefix sum,
//! scatter, the start array its own cursors): two passes, into the run's frame, and no
//! allocation but the arrays it returns, whatever the number of kinds. A lookup is three loads.

use crate::mem::{FVec, Frame, filled};

/// That `a` relates to `b` by `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Fact {
    pub(super) kind: u8,
    pub(super) a: u32,
    pub(super) b: u32,
}

const _: () = assert!(size_of::<Fact>() == 12);

/// Adjacency lists in compressed sparse row form: the items of node `n` are
/// `items[start[n]..start[n + 1]]`. 4 bytes per node and one `T` per item.
#[derive(PartialEq, Eq, Debug)]
pub(super) struct Csr<'f, T = u32> {
    start: FVec<'f, u32>,
    items: FVec<'f, T>,
}

impl<'f, T> Csr<'f, T> {
    /// No nodes.
    pub(super) fn empty(frame: &'f Frame) -> Self {
        let mut start = Vec::with_capacity_in(1, frame);
        start.push(0);
        Csr { start, items: Vec::new_in(frame) }
    }
}

/// Counts each node of `nodes` into `start` (zeros, one more than the nodes) and makes it the
/// exclusive prefix sum: each node's first item, then the total, which it returns.
fn starts(start: &mut [u32], nodes: impl Iterator<Item = usize>) -> usize {
    let count = start.len() - 1;
    for node in nodes {
        assert!(node < count, "a CSR pair's node {node} is not below its {count} nodes");
        start[node] += 1;
    }
    let mut total = 0u32;
    for at in start.iter_mut() {
        let here = *at;
        *at = total;
        total += here;
    }
    total as usize
}

/// After a scatter that moved each node's start to its end (the next node's start), moves
/// them back.
fn rewind(start: &mut [u32]) {
    let count = start.len() - 1;
    if count > 0 {
        start.copy_within(0..count - 1, 1);
        start[0] = 0;
    }
}

impl<'f, T: Copy> Csr<'f, T> {
    /// The lists of `nodes` nodes from `(node, item)` pairs, each list in the order its pairs
    /// come in (a counting sort, which is stable). The pairs are walked twice and never
    /// copied. A node at or past `nodes` is a bug.
    pub(super) fn build<I>(frame: &'f Frame, nodes: usize, pairs: I) -> Self
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let pairs = pairs.into_iter();
        let mut start = filled(frame, nodes + 1, 0u32);
        // clone: the pairs' iterator, for the counting walk
        let total = starts(&mut start, pairs.clone().map(|(node, _)| node as usize));
        // clone: the pairs' iterator, for its first item
        let Some((_, filler)) = pairs.clone().next() else {
            return Csr { start, items: Vec::new_in(frame) };
        };
        let mut items = filled(frame, total, filler);
        for (node, item) in pairs {
            let at = &mut start[node as usize];
            items[*at as usize] = item;
            *at += 1;
        }
        rewind(&mut start);
        Csr { start, items }
    }

    /// [`Csr::build`] into this CSR's arrays, which keep their room.
    pub(super) fn rebuild<I>(&mut self, nodes: usize, pairs: I)
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let pairs = pairs.into_iter();
        self.start.clear();
        self.start.resize(nodes + 1, 0);
        // clone: the pairs' iterator, for the counting walk
        let total = starts(&mut self.start, pairs.clone().map(|(node, _)| node as usize));
        self.items.clear();
        // clone: the pairs' iterator, for its first item
        let Some((_, filler)) = pairs.clone().next() else { return };
        self.items.resize(total, filler);
        for (node, item) in pairs {
            let at = &mut self.start[node as usize];
            self.items[*at as usize] = item;
            *at += 1;
        }
        rewind(&mut self.start);
    }

    /// An empty CSR to append nodes to in order, with room for `nodes` nodes and `items`
    /// items.
    pub(super) fn with_capacity(frame: &'f Frame, nodes: usize, items: usize) -> Self {
        let mut start = Vec::with_capacity_in(nodes + 1, frame);
        start.push(0);
        Csr { start, items: Vec::with_capacity_in(items, frame) }
    }

    /// Adds an item to the node being appended.
    pub(super) fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// Adds the items of the ended node `node` to the node being appended.
    pub(super) fn push_from(&mut self, node: usize) {
        let (from, to) = (self.start[node] as usize, self.start[node + 1] as usize);
        self.items.extend_from_within(from..to);
    }

    /// The items of the node being appended, so far.
    pub(super) fn open(&self) -> &[T] {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        &self.items[from..]
    }

    /// The items of the node being appended, so far, to change in place.
    pub(super) fn open_mut(&mut self) -> &mut [T] {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        &mut self.items[from..]
    }

    /// Drops the open node's items past `len`.
    pub(super) fn truncate_open(&mut self, len: usize) {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        self.items.truncate(from + len);
    }

    /// Ends the node being appended; the next item starts the next node.
    pub(super) fn end_node(&mut self) {
        self.start.push(self.items.len() as u32);
    }

    /// The items of `node`, none for a node past the last.
    pub(super) fn of(&self, node: usize) -> &[T] {
        match (self.start.get(node), self.start.get(node + 1)) {
            (Some(&from), Some(&to)) => &self.items[from as usize..to as usize],
            _ => &[],
        }
    }

    pub(super) fn nodes(&self) -> usize {
        self.start.len() - 1
    }
}

/// Rows of [`Fact`]s, as they are found.
pub(super) struct Facts<'f> {
    rows: FVec<'f, Fact>,
}

impl<'f> Facts<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        Facts { rows: Vec::new_in(frame) }
    }

    pub(super) fn with_capacity(frame: &'f Frame, rows: usize) -> Self {
        Facts { rows: Vec::with_capacity_in(rows, frame) }
    }

    pub(super) fn push(&mut self, kind: u8, a: usize, b: usize) {
        self.rows.push(Fact { kind, a: a as u32, b: b as u32 });
    }

    /// One CSR over every kind, kind `k` taking `domains[k]` values of `a`: the `b`s of an `a`
    /// in the order their facts were pushed.
    pub(super) fn freeze(self, domains: &[usize]) -> Relation<'f> {
        let frame = *self.rows.allocator();
        let mut nodes = 0u32;
        let base = frame.alloc_slice_fill_with(domains.len() + 1, |at| {
            let here = nodes;
            if let Some(&domain) = domains.get(at) {
                nodes += domain as u32;
            }
            here
        });
        let base = &*base;
        let node = |fact: &Fact| (base[fact.kind as usize] + fact.a) as usize;
        let start = frame.alloc_slice_fill_with(nodes as usize + 1, |_| 0u32);
        let total = starts(start, self.rows.iter().map(node));
        let items = frame.alloc_slice_fill_with(total, |_| 0u32);
        for fact in &self.rows {
            let at = &mut start[node(fact)];
            items[*at as usize] = fact.b;
            *at += 1;
        }
        rewind(start);
        Relation { base, start, items }
    }
}

/// [`Facts`] frozen: the `b`s of each `(kind, a)`. An empty `base` makes every `get` empty.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Relation<'f> {
    /// Where each kind's nodes begin, and past the last, the number of nodes.
    base: &'f [u32],
    start: &'f [u32],
    items: &'f [u32],
}

impl<'f> Relation<'f> {
    pub(super) fn get(&self, kind: u8, a: usize) -> &'f [u32] {
        let kind = kind as usize;
        let (Some(&from), Some(&to)) = (self.base.get(kind), self.base.get(kind + 1)) else {
            return &[];
        };
        if a >= (to - from) as usize {
            return &[];
        }
        let node = from as usize + a;
        let items = self.items;
        &items[self.start[node] as usize..self.start[node + 1] as usize]
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame};

    use super::{Csr, Facts};

    #[test]
    fn a_relation_is_its_facts_grouped_in_order() {
        frame::task(Tag::Mir, |frame| {
            let mut rng = SplitMix64::new(0x9e3779b97f4a7c15);
            for _ in 0..200 {
                let domains = [1 + rng.index(20), 1 + rng.index(5), 1 + rng.index(40)];
                let mut rows = Vec::new();
                let mut facts = Facts::new(frame);
                for _ in 0..rng.below(100) {
                    let kind = rng.below(3) as u8;
                    let a = rng.index(domains[kind as usize]);
                    let b = rng.index(1000);
                    rows.push((kind, a, b));
                    facts.push(kind, a, b);
                }
                let relation = facts.freeze(&domains);
                for (kind, &domain) in domains.iter().enumerate() {
                    for a in 0..domain + 2 {
                        let want: Vec<u32> = rows
                            .iter()
                            .filter(|&&(k, x, _)| k as usize == kind && x == a)
                            .map(|&(_, _, b)| b as u32)
                            .collect();
                        assert_eq!(relation.get(kind as u8, a), want.as_slice());
                    }
                }
                assert!(relation.get(7, 0).is_empty());
            }
        });
    }

    #[test]
    fn a_csr_keeps_each_lists_order() {
        frame::task(Tag::Mir, |frame| {
            let pairs = [(2, 'a'), (0, 'b'), (2, 'c'), (3, 'd'), (2, 'e')];
            let csr = Csr::build(frame, 4, pairs.iter().copied());
            assert_eq!(csr.of(0), &['b']);
            assert_eq!(csr.of(1), &[] as &[char]);
            assert_eq!(csr.of(2), &['a', 'c', 'e']);
            assert_eq!(csr.of(3), &['d']);
            assert_eq!(csr.of(4), &[] as &[char]);
            assert_eq!((csr.nodes(), csr.items.len()), (4, 5));
            assert_eq!(Csr::<u32>::empty(frame).of(0), &[] as &[u32]);
            let empty = Csr::<u32>::build(frame, 3, std::iter::empty());
            assert_eq!((empty.nodes(), empty.of(2)), (3, &[] as &[u32]));
        });
    }

    #[test]
    fn rebuild_gives_what_build_does() {
        frame::task(Tag::Mir, |frame| {
            let pairs = [(2u32, 1u32), (0, 2), (2, 3), (3, 4), (2, 5)];
            let mut csr = Csr::build(frame, 4, pairs.iter().copied());
            csr.rebuild(5, pairs.iter().copied().chain([(4, 6)]));
            let want = Csr::build(frame, 5, pairs.iter().copied().chain([(4, 6)]));
            assert_eq!(csr, want);
            csr.rebuild(2, std::iter::empty());
            assert_eq!((csr.nodes(), csr.of(1)), (2, &[] as &[u32]));
        });
    }

    #[test]
    fn a_csr_matches_blaze_radix() {
        frame::task(Tag::Mir, |frame| {
            let mut rng = SplitMix64::new(0x2545f4914f6cdd1d);
            for _ in 0..300 {
                let nodes = 1 + rng.index(30);
                let pairs: Vec<(u32, u32)> = (0..rng.index(200))
                    .map(|it| (rng.index(nodes) as u32, it as u32))
                    .collect();
                let ours = Csr::build(frame, nodes, pairs.iter().copied());
                let radix = blaze_ids::radix::Csr::build(nodes, pairs.iter().copied());
                for node in 0..nodes {
                    assert_eq!(ours.of(node), radix.row(node));
                }
            }
        });
    }
}
