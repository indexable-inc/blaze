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
//! Memory: two tiers. Readers added since the last [`Reverse::pack`] are in lists: a reader
//! is one `u32` in a block of its node's list, blocks double from [`FIRST`] to [`MOST`]
//! entries with a two-word header, and a node has a 4-byte head, so a node with one reader
//! takes 20 bytes for it (measured 2026-11-11, lane mem2-1111, a hello-world daemon: 25.5
//! million readers of 2.3 million nodes in 267 MiB, 10.5 bytes a reader, the largest
//! ingredient of the query storage). [`Reverse::pack`] moves them into the packed tier: every
//! node's readers side by side in one array, 4 bytes a reader and 4 a node, the forward edge
//! array's size. The driver packs when the lists hold as many readers as the packed tier
//! ([`Reverse::due`]), so packing costs each reader a bounded number of copies, and makes the
//! whole index again from the rows once it holds more than twice the edges the rows name.

#![blaze::sans_io]

/// The entries of a node's first block, and the most a block holds.
const FIRST: usize = 2;
const MOST: usize = 4096;

/// The readers in lists below which [`Reverse::due`] never asks for a pack: lists this small
/// (a megabyte) are not worth a pass over the packed tier.
const PACK_AT: u64 = if cfg!(test) { 8 } else { 1 << 16 };

/// For each node, the rows that read it, in two tiers (the module's "Memory"): the packed
/// tier, node `d`'s readers at `packed[starts[d]..starts[d + 1]]`, and the readers added since
/// the last pack, an unrolled list of blocks in one array. A block is
/// `[next, len | cap << 16, entries..]`; block 0 is no block.
#[derive(Default)]
pub(crate) struct Reverse {
    /// Where each node's packed readers start, and one past the last node's: empty before
    /// the first pack. A node past it has none packed.
    starts: Vec<u32>,
    packed: Vec<u32>,
    /// Each node's newest block, 0 for none.
    heads: Vec<u32>,
    blocks: Vec<u32>,
    /// The readers held in lists, the stale ones too.
    held: u64,
}

impl Reverse {
    /// The readers it holds, stale ones included: what the driver weighs against the edges
    /// the rows name.
    #[blaze::pure]
    pub(crate) fn held(&self) -> u64 {
        self.held + self.packed.len() as u64
    }

    /// The bytes it has reserved.
    #[blaze::pure]
    pub(crate) fn bytes(&self) -> u64 {
        let words = self.starts.capacity()
            + self.packed.capacity()
            + self.heads.capacity()
            + self.blocks.capacity();
        (words * size_of::<u32>()) as u64
    }

    /// Forgets every reader, keeping the lists' memory: before every row is folded again.
    #[blaze::pure]
    pub(crate) fn clear(&mut self) {
        self.starts = Vec::new();
        self.packed = Vec::new();
        self.heads.clear();
        self.blocks.clear();
        self.held = 0;
    }

    /// Whether the lists hold enough readers for a [`Reverse::pack`] to pay: at least
    /// [`PACK_AT`] and as many as the packed tier. A pack copies every reader once, so under
    /// this rule a reader is copied at most once per doubling of the index: a number of times
    /// logarithmic in the index's size, never once a fold.
    #[blaze::pure]
    pub(crate) fn due(&self) -> bool {
        self.held >= PACK_AT && self.held >= self.packed.len() as u64
    }

    /// Whether the lists hold enough readers for a pack to be worth it with time to spare (a
    /// build's end): at least [`PACK_AT`].
    #[blaze::pure]
    pub(crate) fn worth_packing(&self) -> bool {
        self.held >= PACK_AT
    }

    /// Moves every reader in the lists into the packed tier and gives the lists' memory
    /// back: after it, [`Reverse::readers`] gives each node what it gave before, in another
    /// order, from 4 bytes a reader. Costs the readers held and the nodes; holds the old and
    /// the new packed tier at once while it runs.
    ///
    /// # Panics
    ///
    /// When the packed tier would pass 2^32 readers, saying so.
    #[blaze::pure]
    pub(crate) fn pack(&mut self) {
        if self.held == 0 {
            return;
        }
        let nodes = self.heads.len().max(self.starts.len().saturating_sub(1));
        let total = self.held();
        assert!(
            total < u64::from(u32::MAX),
            "the query database's reverse edges passed 2^32 readers as they were packed"
        );
        let mut starts: Vec<u32> = Vec::with_capacity(nodes + 1); // alloc: the reverse index, kept for the storage's life
        let mut packed: Vec<u32> = Vec::with_capacity(total as usize); // alloc: as above
        for node in 0..nodes {
            starts.push(packed.len() as u32); // alloc: within the capacity taken above
            let before = self.packed_of(node as u32);
            packed.extend_from_slice(before); // alloc: within the capacity taken above
            self.listed(node as u32, |reader| packed.push(reader)); // alloc: within the capacity taken above
        }
        starts.push(packed.len() as u32); // alloc: within the capacity taken above
        debug_assert_eq!(packed.len() as u64, total);
        self.starts = starts;
        self.packed = packed;
        // The lists' arrays go back whole: what they grew to is a build's burst, not what the
        // next edits add.
        self.heads = Vec::new();
        self.blocks = Vec::new();
        self.held = 0;
    }

    /// Node `dep`'s readers in the packed tier.
    #[blaze::pure]
    fn packed_of(&self, dep: u32) -> &[u32] {
        let at = dep as usize;
        match (self.starts.get(at), self.starts.get(at + 1)) {
            (Some(&from), Some(&to)) => &self.packed[from as usize..to as usize],
            _ => &[],
        }
    }

    /// Gives `each` every reader of `dep` in the lists, newest block first; returns how many.
    ///
    /// Bounded: a block's `next` is a block made before it, so of a lower index.
    #[blaze::pure]
    fn listed(&self, dep: u32, mut each: impl FnMut(u32)) -> u64 {
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

    /// Gives `each` every reader of `dep`, the packed ones then the lists' newest block
    /// first; returns how many.
    #[blaze::pure]
    pub(crate) fn readers(&self, dep: u32, mut each: impl FnMut(u32)) -> u64 {
        let packed = self.packed_of(dep);
        for &reader in packed {
            each(reader);
        }
        packed.len() as u64 + self.listed(dep, each)
    }
}

/// A set of nodes: the walk's own, and the fold's scratch.
pub(crate) type Seen = hashbrown::HashSet<u32, blaze_hash::FixedState>;

/// A node that changed, with the view it changed in (its `changed` stamp): a reader verified
/// in that view or a later one has seen the change. [`ASSUMED`] for a node taken as changed
/// with no stamp to say when (marked, unsure, set apart, emptied): every reader is asked.
pub(crate) type ChangedIn = (u32, u32);

/// The stamp of a node taken as changed with nothing to say when: past every view.
pub(crate) const ASSUMED: u32 = u32::MAX;

/// One walk's bookkeeping: which changed nodes' readers were gathered, and which rows were
/// handed out to verify, each once. So a walk verifies a row at most once and gathers a
/// node's readers at most once a stamp it changed in (a node that changes again in the walk
/// is gathered again, and the readers handed out before are not handed out again): its work
/// is bounded by the nodes it reaches (rule 18).
pub(crate) struct Plan {
    /// Each node gathered, with the latest stamp it was gathered at.
    gathered: hashbrown::HashMap<u32, u32, blaze_hash::FixedState>,
    asked: Seen,
    /// Reverse edges followed.
    pub(crate) edges: u64,
    /// Rows handed out to verify.
    pub(crate) visited: u64,
    /// Readers passed over: verified at or after the view their dependency changed in.
    pub(crate) seen_since: u64,
    /// Readers passed over: rows behind a seal, which their group's one check answers for.
    pub(crate) sealed: u64,
}

impl Plan {
    #[blaze::pure]
    pub(crate) fn new() -> Plan {
        Plan {
            gathered: hashbrown::HashMap::with_hasher(blaze_hash::FixedState),
            asked: Seen::with_hasher(blaze_hash::FixedState),
            edges: 0,
            visited: 0,
            seen_since: 0,
            sealed: 0,
        }
    }

    /// Hands `row` out to verify, unless it was: `true` the first time.
    #[blaze::pure]
    pub(crate) fn ask(&mut self, row: u32) -> bool {
        let first = self.asked.insert(row); // alloc: a walk's set, the rows it reaches
        self.visited += u64::from(first);
        first
    }

    /// The readers of the nodes of `changed` that were not handed out yet and have not seen
    /// the change, pushed onto `suspects`, then sorted: the order the driver verifies and
    /// reports them in, the same on every run (rule 25). `verified` gives the view a reader
    /// was last verified in (0 when it cannot be told: running, provisional, no row).
    /// `changed` is emptied.
    ///
    /// The invariant: a reader is passed over only when it was verified in the view its
    /// dependency changed in or later. That is the engine's own test of an edge (a pull
    /// proves a row by `changed(dependency) <= verified(row)`, [`super::verify`]): such a
    /// reader ran or was proved after the change, so the change is in its value already, and
    /// if that made it another it said so itself. Without it a walk asks every reader of
    /// every row a build made again, whatever the edit: the first walk after a build was the
    /// build's size (a hello-world: 381,304 rows marked for an edit of one function,
    /// 2026-11-12).
    ///
    /// A node gathered at a stamp is not gathered again at that stamp or an earlier one.
    ///
    /// `sealed` says whether a reader is behind a seal ([`super::seal`]): such a reader is
    /// never handed out, whatever its dependency's stamp. A walk does not enter a sealed
    /// group; the group's check, of what its rows read from outside it, answers for all of
    /// them, and a group whose check fails has its rows named by the driver.
    #[blaze::pure]
    pub(crate) fn suspects(
        &mut self,
        reverse: &Reverse,
        changed: &mut Vec<ChangedIn>,
        suspects: &mut Vec<u32>,
        mut verified: impl FnMut(u32) -> u32,
        mut sealed: impl FnMut(u32) -> bool,
    ) {
        // By node, then stamp: a node's latest stamp is the last of its run.
        changed.sort_unstable();
        for (at, &(node, since)) in changed.iter().enumerate() {
            if changed.get(at + 1).is_some_and(|next| next.0 == node) {
                continue;
            }
            match self.gathered.entry(node) {
                hashbrown::hash_map::Entry::Occupied(mut had) => {
                    if *had.get() >= since {
                        continue;
                    }
                    had.insert(since);
                }
                hashbrown::hash_map::Entry::Vacant(new) => {
                    new.insert(since); // alloc: a walk's set, the nodes it reaches
                }
            }
            let (asked, visited, seen_since, behind_seal) =
                (&mut self.asked, &mut self.visited, &mut self.seen_since, &mut self.sealed);
            self.edges += reverse.readers(node, |reader| {
                if asked.contains(&reader) {
                    return;
                }
                if sealed(reader) {
                    *behind_seal += 1;
                    return;
                }
                if since != ASSUMED && verified(reader) >= since {
                    *seen_since += 1;
                    return;
                }
                asked.insert(reader); // alloc: as above
                *visited += 1;
                suspects.push(reader); // alloc: a walk's list, the rows it reaches
            });
        }
        changed.clear();
        suspects.sort_unstable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: a reader behind a seal is never handed out, whether its dependency has
    /// a stamp or is taken as changed with none; the readers beside it are.
    #[test]
    fn a_plan_never_hands_out_a_reader_behind_a_seal() {
        let mut reverse = Reverse::default();
        for reader in [10, 20, 30] {
            reverse.add(1, reader);
        }
        let mut plan = Plan::new();
        let (mut changed, mut suspects) = (vec![(1, ASSUMED)], Vec::new());
        plan.suspects(&reverse, &mut changed, &mut suspects, |_| 0, |reader| reader == 20);
        assert_eq!(suspects, [10, 30]);
        assert_eq!((plan.visited, plan.sealed, plan.edges), (2, 1, 3));
    }

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

    /// The invariant ([`Reverse`]'s, across packs): every reader added of a node is given
    /// for it, whatever was packed in between, and nothing is given that was not added.
    /// Checked against a model (a set of readers a node) over runs of adds and packs drawn
    /// from a seeded generator: 200 runs of up to 400 steps over 24 nodes, so a node's readers
    /// span both tiers and several blocks. The control below plants the bug.
    #[test]
    fn packing_keeps_every_reader_of_every_node() {
        for seed in 0..200u64 {
            if let Err(why) = run_against_the_model(seed, false) {
                panic!("seed {seed}: {why}");
            }
        }
    }

    /// The oracle catches a pack that loses the lists' readers of nodes past the packed
    /// tier's last (the planted bug: such a pack reads `starts` for how many nodes there are).
    #[test]
    fn the_oracle_catches_a_pack_that_drops_the_newest_nodes_readers() {
        let caught = (0..200u64).filter(|&seed| run_against_the_model(seed, true).is_err()).count();
        assert!(caught > 100, "the planted bug was caught in {caught} of 200 runs");
    }

    /// One run of adds and packs against the model; `planted` packs with the bug.
    fn run_against_the_model(seed: u64, planted: bool) -> Result<(), String> {
        use std::collections::{BTreeMap, BTreeSet};
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut draw = |below: u64| {
            // xorshift64*: a seeded stream, the same on every run (rule 25).
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D) % below
        };
        let mut reverse = Reverse::default();
        let mut model: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        let steps = 50 + draw(350);
        for step in 0..steps {
            if draw(40) == 0 {
                match planted {
                    false => reverse.pack(),
                    true => {
                        // The bug: only the nodes the packed tier already knows are packed.
                        let known = reverse.starts.len().saturating_sub(1);
                        if known > 0 && known < reverse.heads.len() {
                            reverse.heads.truncate(known);
                            reverse.held =
                                (0..known as u32).map(|dep| reverse.listed(dep, |_| {})).sum();
                        }
                        reverse.pack();
                    }
                }
                continue;
            }
            // Nodes grow over the run, so later packs meet nodes the packed tier lacks.
            let dep = draw(4 + step * 20 / steps) as u32;
            let reader = 100 + draw(64) as u32;
            reverse.add(dep, reader);
            model.entry(dep).or_default().insert(reader);
        }
        for dep in 0..32u32 {
            let given: BTreeSet<u32> = readers(&reverse, dep).into_iter().collect();
            let want = model.get(&dep).cloned().unwrap_or_default();
            if given != want {
                return Err(format!("node {dep}: given {given:?}, added {want:?}"));
            }
        }
        Ok(())
    }

    #[test]
    fn a_pack_is_four_bytes_a_reader_and_a_node() {
        let mut reverse = Reverse::default();
        for reader in 0..100_000u32 {
            reverse.add(reader, reader + 1);
            reverse.add(reader / 2, reader + 7);
        }
        let (held, listed) = (reverse.held(), reverse.bytes());
        assert!(reverse.due() && reverse.worth_packing());
        reverse.pack();
        assert_eq!(reverse.held(), held, "a pack keeps every reader");
        assert!(!reverse.worth_packing(), "and leaves the lists empty");
        assert_eq!(reverse.bytes(), (held + 100_001) * 4, "4 bytes a reader, 4 a node and one");
        assert!(reverse.bytes() * 2 < listed, "{} packed, {listed} in lists", reverse.bytes());
        assert_eq!(readers(&reverse, 3), [4, 13, 14]);
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
        let (mut changed, mut suspects) = (vec![(2, ASSUMED), (1, ASSUMED), (2, ASSUMED)], Vec::new());
        plan.suspects(&reverse, &mut changed, &mut suspects, |_| 0, |_| false);
        assert_eq!(suspects, [10, 20, 30]);
        assert!(changed.is_empty());
        suspects.clear();
        changed.extend([(20, ASSUMED), (1, ASSUMED)]);
        plan.suspects(&reverse, &mut changed, &mut suspects, |_| 0, |_| false);
        assert_eq!(suspects, [40], "node 1's readers were gathered, row 20's are new");
        assert!(!plan.ask(40));
        assert_eq!((plan.visited, plan.edges), (4, 5));
    }

    /// The invariant: a reader verified in the view its dependency changed in, or later, is
    /// not handed out; one verified before is; and a node that changes again later hands out
    /// the readers its earlier stamp passed over.
    #[test]
    fn a_plan_passes_over_the_readers_that_saw_the_change() {
        let mut reverse = Reverse::default();
        for reader in [10, 20, 30] {
            reverse.add(1, reader);
        }
        // Reader 10 was verified in view 4, 20 in view 5, 30 in view 6.
        let verified = |reader: u32| match reader {
            10 => 4,
            20 => 5,
            _ => 6,
        };
        let mut plan = Plan::new();
        let (mut changed, mut suspects) = (vec![(1, 5), (1, 3)], Vec::new());
        plan.suspects(&reverse, &mut changed, &mut suspects, verified, |_| false);
        assert_eq!(suspects, [10], "node 1 changed in view 5: only the reader of view 4 is behind");
        assert_eq!((plan.visited, plan.seen_since, plan.edges), (1, 2, 3));
        suspects.clear();
        changed.push((1, 5));
        plan.suspects(&reverse, &mut changed, &mut suspects, verified, |_| false);
        assert_eq!(suspects, [] as [u32; 0], "gathered at that stamp already");
        changed.push((1, 6));
        plan.suspects(&reverse, &mut changed, &mut suspects, verified, |_| false);
        assert_eq!(suspects, [20], "changed again in view 6: the reader of view 5 is behind now");
        suspects.clear();
        changed.push((1, ASSUMED));
        plan.suspects(&reverse, &mut changed, &mut suspects, verified, |_| false);
        assert_eq!(suspects, [30], "taken as changed with no stamp: every reader not yet asked");
    }
}
