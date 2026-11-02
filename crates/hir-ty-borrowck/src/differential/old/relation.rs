//! Facts as columnar relations: rows `(kind, a, b)` gathered as they are found, then frozen
//! into a CSR over `(kind, a)`, so the `b`s of one `a` are one range: the moves at a point, the
//! loans of a local, the uses at a point. Freezing is `blaze_radix`'s counting sort (count,
//! prefix sum, scatter): two passes, and no allocation but the arrays it returns and its
//! cursors, whatever the number of kinds. A lookup is three loads.

/// That `a` relates to `b` by `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fact {
    pub(crate) kind: u8,
    pub(crate) a: u32,
    pub(crate) b: u32,
}

const _: () = assert!(size_of::<Fact>() == 12);

/// Adjacency lists in compressed sparse row form: the items of node `n` are
/// `items[start[n]..start[n + 1]]`. 4 bytes per node and one `T` per item.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Csr<T = u32> {
    start: Vec<u32>,
    items: Vec<T>,
}

impl<T> Default for Csr<T> {
    fn default() -> Self {
        Csr { start: vec![0], items: Vec::new() }
    }
}

impl<T: Copy> Csr<T> {
    /// The lists of `nodes` nodes from `(node, item)` pairs, each list in the order its pairs
    /// come in (a counting sort, which is stable). The pairs are walked twice and never
    /// copied. A node at or past `nodes` is a bug.
    pub(crate) fn build<I>(nodes: usize, pairs: I) -> Self
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let (start, items) = blaze_ids::radix::Csr::build(nodes, pairs).into_parts();
        Csr { start, items }
    }

    /// An empty CSR to append nodes to in order, with room for `nodes` nodes and `items`
    /// items.
    pub(crate) fn with_capacity(nodes: usize, items: usize) -> Self {
        let mut start = Vec::with_capacity(nodes + 1);
        start.push(0);
        Csr { start, items: Vec::with_capacity(items) }
    }

    /// Adds an item to the node being appended.
    pub(crate) fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// Adds the items of the ended node `node` to the node being appended.
    pub(crate) fn push_from(&mut self, node: usize) {
        let (from, to) = (self.start[node] as usize, self.start[node + 1] as usize);
        self.items.extend_from_within(from..to);
    }

    /// The items of the node being appended, so far.
    pub(crate) fn open(&self) -> &[T] {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        &self.items[from..]
    }

    /// The items of the node being appended, so far, to change in place.
    pub(crate) fn open_mut(&mut self) -> &mut [T] {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        &mut self.items[from..]
    }

    /// Drops the open node's items past `len`.
    pub(crate) fn truncate_open(&mut self, len: usize) {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        self.items.truncate(from + len);
    }

    /// Ends the node being appended; the next item starts the next node.
    pub(crate) fn end_node(&mut self) {
        self.start.push(self.items.len() as u32);
    }

    /// The items of `node`, none for a node past the last.
    pub(crate) fn of(&self, node: usize) -> &[T] {
        match (self.start.get(node), self.start.get(node + 1)) {
            (Some(&from), Some(&to)) => &self.items[from as usize..to as usize],
            _ => &[],
        }
    }

    pub(crate) fn nodes(&self) -> usize {
        self.start.len() - 1
    }
}

/// Rows of [`Fact`]s, as they are found.
#[derive(Default)]
pub(crate) struct Facts {
    rows: Vec<Fact>,
}

impl Facts {
    pub(crate) fn with_capacity(rows: usize) -> Self {
        Facts { rows: Vec::with_capacity(rows) }
    }

    pub(crate) fn push(&mut self, kind: u8, a: usize, b: usize) {
        self.rows.push(Fact { kind, a: a as u32, b: b as u32 });
    }

    /// One CSR over every kind, kind `k` taking `domains[k]` values of `a`: the `b`s of an `a`
    /// in the order their facts were pushed.
    pub(crate) fn freeze(self, domains: &[usize]) -> Relation {
        let mut base = Vec::with_capacity(domains.len() + 1);
        let mut nodes = 0u32;
        for &domain in domains {
            base.push(nodes);
            nodes += domain as u32;
        }
        base.push(nodes);
        let node = |fact: &Fact| base[fact.kind as usize] + fact.a;
        let (start, items) = blaze_ids::radix::Csr::build(
            nodes as usize,
            self.rows.iter().map(|fact| (node(fact), fact.b)),
        )
        .into_parts();
        Relation { base, start, items }
    }
}

/// [`Facts`] frozen: the `b`s of each `(kind, a)`.
#[derive(Clone, Debug)]
pub(crate) struct Relation {
    /// Where each kind's nodes begin, and past the last, the number of nodes.
    base: Vec<u32>,
    start: Vec<u32>,
    items: Vec<u32>,
}

impl Default for Relation {
    fn default() -> Self {
        Relation { base: vec![0], start: vec![0], items: Vec::new() }
    }
}

impl Relation {
    pub(crate) fn get(&self, kind: u8, a: usize) -> &[u32] {
        let kind = kind as usize;
        let (Some(&from), Some(&to)) = (self.base.get(kind), self.base.get(kind + 1)) else {
            return &[];
        };
        if a >= (to - from) as usize {
            return &[];
        }
        let node = from as usize + a;
        &self.items[self.start[node] as usize..self.start[node + 1] as usize]
    }
}
