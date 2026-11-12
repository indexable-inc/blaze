//! The reverse of the dependency graph, and the walk's bookkeeping over it: for a node, the
//! rows that read it. What the changed-set walk ([`super::frontier`]) follows from what was
//! written to the rows that may have changed, so an edit costs the rows it reaches and never
//! the rows the program has (docs/v2/plan/design/incremental-walk.md, "The changed set from
//! the engine").
//!
//! A core: plain data, one owner, no IO, no clock, no lock and no atomic. The driver
//! ([`super::frontier`]) brings it what runs stored and asks it what to verify next; the
//! deterministic simulation (`db/tests/changed.rs`) runs the driver over it.
//!
//! The invariant: for every row `r` whose stored edges, as of the last fold, hold a node `d`,
//! [`Reverse::readers`] of `d` gives `r`. It may give more (a row that ran again without the
//! edge stays, until [`Reverse::clear`] and a fold of every row): a reader named too often
//! costs one verification, a reader missed is a stale result.
//!
//! Memory: a reader is one `u32` in a block of its node's list. Blocks double from
//! [`FIRST`] to [`MOST`] entries with a two-word header, so a list holds between 4 bytes a
//! reader (full blocks) and 8 (its newest block just made), plus 4 bytes a node for its
//! head: at most twice the forward edge array's 4 bytes an edge, and the driver makes it
//! again from the rows once it holds more than twice the edges the rows name.

#![blaze::sans_io]

/// The entries of a node's first block, and the most a block holds.
const FIRST: usize = 2;
const MOST: usize = 4096;

/// For each node, the rows that read it: an unrolled list of blocks in one array. A block is
/// `[next, len | cap << 16, entries..]`; block 0 is no block.
#[derive(Default)]
pub(crate) struct Reverse {
    /// Each node's newest block, 0 for none.
    heads: Vec<u32>,
    blocks: Vec<u32>,
    /// The readers held, the stale ones too.
    held: u64,
}

impl Reverse {
    /// The readers it holds, stale ones included: what the driver weighs against the edges
    /// the rows name.
    #[blaze::pure]
    pub(crate) fn held(&self) -> u64 {
        self.held
    }

    /// The bytes it has reserved.
    #[blaze::pure]
    pub(crate) fn bytes(&self) -> u64 {
        ((self.heads.capacity() + self.blocks.capacity()) * size_of::<u32>()) as u64
    }

    /// Forgets every reader, keeping its memory: before every row is folded again.
    #[blaze::pure]
    pub(crate) fn clear(&mut self) {
        self.heads.clear();
        self.blocks.clear();
        self.held = 0;
    }

    /// Row `reader` reads node `dep`. A repeat of the node's newest reader is not kept.
    ///
    /// # Panics
    ///
    /// When the list array would pass 2^32 words, naming the edge.
    #[blaze::pure]
    pub(crate) fn add(&mut self, dep: u32, reader: u32) {
        let at = dep as usize;
        if at >= self.heads.len() {
            self.heads.resize(at + 1, 0); // alloc: the reverse index, kept for the storage's life
        }
        if self.blocks.is_empty() {
            // Block 0 is no block.
            self.blocks.extend_from_slice(&[0, 0]); // alloc: as above
        }
        let head = self.heads[at] as usize;
        let mut cap = FIRST;
        if head != 0 {
            let word = self.blocks[head + 1];
            let (len, had) = ((word & 0xffff) as usize, (word >> 16) as usize);
            if len > 0 && self.blocks[head + 1 + len] == reader {
                return;
            }
            if len < had {
                self.blocks[head + 2 + len] = reader;
                self.blocks[head + 1] = word + 1;
                self.held += 1;
                return;
            }
            cap = (had * 2).min(MOST);
        }
        let block = self.blocks.len();
        assert!(
            block + 2 + cap <= u32::MAX as usize,
            "the query database's reverse edges passed 2^32 words storing that node {reader} \
             reads node {dep}"
        );
        self.blocks.resize(block + 2 + cap, 0); // alloc: as above
        self.blocks[block] = head as u32;
        self.blocks[block + 1] = 1 | ((cap as u32) << 16);
        self.blocks[block + 2] = reader;
        self.heads[at] = block as u32;
        self.held += 1;
    }

    /// Row `reader` stored the edges `new` where it held `old` (empty for a first run): the
    /// nodes of `new` that `old` lacks gain it as a reader. What `old` alone held keeps it, a
    /// stale reader ([`Reverse`]'s invariant allows it). `seen` is scratch.
    #[blaze::pure]
    pub(crate) fn stored(&mut self, reader: u32, old: &[u32], new: &[u32], seen: &mut Seen) {
        if old.is_empty() {
            for &dep in new {
                self.add(dep, reader);
            }
            return;
        }
        seen.clear();
        seen.extend(old.iter().copied()); // alloc: scratch, kept by the driver between folds
        for &dep in new {
            if !seen.contains(&dep) {
                self.add(dep, reader);
            }
        }
    }

    /// Gives `each` every reader of `dep`, newest block first; returns how many.
    ///
    /// Bounded: a block's `next` is a block made before it, so of a lower index.
    #[blaze::pure]
    pub(crate) fn readers(&self, dep: u32, mut each: impl FnMut(u32)) -> u64 {
        let mut block = self.heads.get(dep as usize).copied().unwrap_or(0) as usize;
        let mut walked = 0;
        while block != 0 {
            let len = (self.blocks[block + 1] & 0xffff) as usize;
            for &reader in &self.blocks[block + 2..block + 2 + len] {
                each(reader);
            }
            walked += len as u64;
            let next = self.blocks[block] as usize;
            debug_assert!(next < block, "a block's next was made before it");
            block = next;
        }
        walked
    }
}

/// A set of nodes: the walk's own, and the fold's scratch.
pub(crate) type Seen = hashbrown::HashSet<u32, blaze_hash::FixedState>;

/// One walk's bookkeeping: which changed nodes' readers were gathered, and which rows were
/// handed out to verify, each once. So a walk verifies a row at most once and gathers a
/// node's readers at most once: its work is bounded by the nodes it reaches (rule 18).
pub(crate) struct Plan {
    gathered: Seen,
    asked: Seen,
    /// Reverse edges followed.
    pub(crate) edges: u64,
    /// Rows handed out to verify.
    pub(crate) visited: u64,
}

impl Plan {
    #[blaze::pure]
    pub(crate) fn new() -> Plan {
        Plan {
            gathered: Seen::with_hasher(blaze_hash::FixedState),
            asked: Seen::with_hasher(blaze_hash::FixedState),
            edges: 0,
            visited: 0,
        }
    }

    /// Hands `row` out to verify, unless it was: `true` the first time.
    #[blaze::pure]
    pub(crate) fn ask(&mut self, row: u32) -> bool {
        let first = self.asked.insert(row); // alloc: a walk's set, the rows it reaches
        self.visited += u64::from(first);
        first
    }

    /// The readers of the nodes of `changed` that were not handed out yet, pushed onto
    /// `suspects`, then sorted: the order the driver verifies and reports them in, the same
    /// on every run (rule 25). A node whose readers were gathered is not gathered again.
    /// `changed` is emptied.
    #[blaze::pure]
    pub(crate) fn suspects(
        &mut self,
        reverse: &Reverse,
        changed: &mut Vec<u32>,
        suspects: &mut Vec<u32>,
    ) {
        changed.sort_unstable();
        changed.dedup();
        for &node in changed.iter() {
            if !self.gathered.insert(node) {
                continue;
            }
            let (asked, visited) = (&mut self.asked, &mut self.visited);
            self.edges += reverse.readers(node, |reader| {
                if asked.insert(reader) {
                    *visited += 1;
                    suspects.push(reader); // alloc: a walk's list, the rows it reaches
                }
            });
        }
        changed.clear();
        suspects.sort_unstable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readers(reverse: &Reverse, dep: u32) -> Vec<u32> {
        let mut out = Vec::new();
        reverse.readers(dep, |it| out.push(it));
        out.sort_unstable();
        out
    }

    #[test]
    fn a_node_gives_every_reader_added_across_its_blocks() {
        let mut reverse = Reverse::default();
        for reader in 0..10_000u32 {
            reverse.add(7, reader + 100);
            reverse.add(reader % 13, reader + 100);
        }
        assert_eq!(readers(&reverse, 7), (100..10_100).collect::<Vec<u32>>());
        assert_eq!(readers(&reverse, 9_999), Vec::<u32>::new());
        // Every thirteenth reader's second add is of node 7 again, the node's newest reader:
        // a repeat, not kept.
        let repeats = (0..10_000u32).filter(|reader| reader % 13 == 7).count() as u64;
        assert_eq!(reverse.held(), 20_000 - repeats);
        // 4 to 8 bytes a reader, and a head a node.
        assert!(reverse.bytes() <= 20_000 * 8 + 14 * 4 + 4096 * 8, "{}", reverse.bytes());
    }

    #[test]
    fn a_rerun_adds_only_the_edges_it_gained_and_keeps_the_ones_it_lost() {
        let mut reverse = Reverse::default();
        let mut seen = Seen::with_hasher(blaze_hash::FixedState);
        reverse.stored(50, &[], &[1, 2, 3], &mut seen);
        reverse.stored(50, &[1, 2, 3], &[1, 4], &mut seen);
        assert_eq!(readers(&reverse, 1), [50]);
        assert_eq!(readers(&reverse, 4), [50]);
        assert_eq!(readers(&reverse, 2), [50], "a stale reader is kept: named too often, never missed");
        assert_eq!(reverse.held(), 4);
    }

    #[test]
    fn a_plan_hands_each_row_out_once_in_order() {
        let mut reverse = Reverse::default();
        for (dep, reader) in [(1, 30), (1, 20), (2, 20), (2, 10), (20, 40)] {
            reverse.add(dep, reader);
        }
        let mut plan = Plan::new();
        let (mut changed, mut suspects) = (vec![2, 1, 2], Vec::new());
        plan.suspects(&reverse, &mut changed, &mut suspects);
        assert_eq!(suspects, [10, 20, 30]);
        assert!(changed.is_empty());
        suspects.clear();
        changed.extend([20, 1]);
        plan.suspects(&reverse, &mut changed, &mut suspects);
        assert_eq!(suspects, [40], "node 1's readers were gathered, row 20's are new");
        assert!(!plan.ask(40));
        assert_eq!((plan.visited, plan.edges), (4, 5));
    }
}
