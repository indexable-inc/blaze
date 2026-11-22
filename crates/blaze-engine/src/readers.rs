//! The reverse index as one request changes it: which rows read a cell (engine.rs invariant 4;
//! overlay.rs shadowing rule 3: the last answer's readers, less those this request dropped, plus
//! those it added). One seam, [`ReaderIndex`], a type parameter of the engine (rule 78), with two
//! strategies held equal decision for decision by readers_sim.rs:
//!
//! - [`Paired`], the engine's: every write `(cell, reader) -> reads` appended to one column and
//!   sorted once when a question follows writes ([`Runs`]): a handful of columns a request,
//!   whatever it reads. A cell's readers are found by halving, `O(log n)` a run.
//! - [`Mapped`], the index as it was: a map of cells, each to its own map of readers. Every cell
//!   read is a map of its own, and every map a node at least: the live-heap log of a 700-crate
//!   workspace 303 s into its Heads stage held 465 MB in 414,531 blocks under `Engine::merge`, the
//!   writes of this map among them, growing with every task merged. Kept as the reference the
//!   differential test drives beside the new one, and as the measure it is held to.
//!
//! # The floor (rule 67)
//!
//! An edge is two ids, the cell and the reader: [`FLOOR_BYTES_PER_EDGE`]. [`Paired`] holds one
//! entry an edge plus its newest-write flag and, in the tail, its place (the sort's scratch); its
//! allocations a phase are a constant (the seal's run and a merge or two), never one a read.
//! [`IndexCost`] says both, and readers_sim.rs prints the ratios.

use core::mem::size_of;

use blaze_drive::key::Name;
use blaze_own::{BTreeMap, SharedAlloc, Vec};

use crate::engine::{CellId, Plant, SMap, SVec};
use crate::runs::Runs;

/// The bytes an edge needs at the least: a cell and a reader, as the engine names them.
pub const FLOOR_BYTES_PER_EDGE: usize = size_of::<(CellId, Name)>();

/// What a reader index holds and has cost: the engine's [`crate::Report::index`], printed in
/// the profile (read edges held, bytes an edge, allocations a phase).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IndexCost {
    /// Membership writes issued, including entries later merged or cancelled.
    pub writes: u64,
    /// Read edges held: `(cell, reader)` writes kept (a pair written again after its run was
    /// sealed is held twice until a merge meets the two).
    pub edges: u64,
    /// Bytes held: [`Paired`]'s columns' capacity, exactly; [`Mapped`]'s entries' bytes, a lower
    /// bound (a B-tree node holds more than its entries).
    pub bytes: u64,
    /// Allocations made: [`Paired`]'s, exactly, over the request; [`Mapped`]'s B-tree nodes held
    /// now, a lower bound (a map of `n` entries holds at least `n / 15` of them, and one at least).
    pub allocations: u64,
    /// The most allocations one phase's merge made: filled by the engine.
    pub phase_allocations_max: u64,
    /// [`Paired`]'s runs held, and its seals (its sorts).
    pub runs: u32,
    pub seals: u32,
    /// Whether `bytes` and `allocations` are exact ([`Paired`]) or lower bounds ([`Mapped`]).
    pub exact: bool,
}

impl IndexCost {
    /// Bytes held an edge (0 for none).
    pub fn bytes_per_edge(&self) -> u64 {
        self.bytes.checked_div(self.edges).unwrap_or(0)
    }
}

/// The request's reader writes at its end, each `(cell, reader)` once with its newest write
/// (`true`: reads it from now on; `false`: no longer), sorted by cell then reader: what a form's
/// fold reads (overlay.rs `Work::readers`). One column.
pub struct ReaderEdits<'a> {
    pub edits: SVec<'a, ((CellId, Name), bool)>,
}

impl<'a> ReaderEdits<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        ReaderEdits { edits: Vec::new_in(alloc) }
    }

    /// Each cell written, ascending, with its writes.
    pub fn iter(&self) -> impl Iterator<Item = (&CellId, Edits<'_>)> + '_ {
        self.edits.chunk_by(|a, b| a.0.0 == b.0.0).map(|group| (&group[0].0.0, Edits { edits: group }))
    }

    /// The writes held: one a `(cell, reader)`.
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

/// One cell's writes, ascending by reader.
#[derive(Clone, Copy)]
pub struct Edits<'e> {
    edits: &'e [((CellId, Name), bool)],
}

impl<'e> Edits<'e> {
    /// Each reader written and whether it reads the cell from now on.
    pub fn iter(&self) -> impl Iterator<Item = (&'e Name, &'e bool)> + 'e {
        self.edits.iter().map(|edit| (&edit.0.1, &edit.1))
    }
}

/// The seam (rule 78): how a request keeps its writes to the reverse index. Each instantiation
/// gives the same answers to the same writes (readers_sim.rs).
pub trait ReaderIndex<'a> {
    /// An empty index over `alloc`. `plant` is the simulation's: [`Plant::LastReaderDropped`]
    /// is [`Paired`]'s planted bug, and nothing else here reads it.
    fn new_in(alloc: SharedAlloc<'a>, plant: Plant) -> Self
    where
        Self: Sized;

    /// Room for a batch of known membership changes. A replacement with an unknown
    /// difference grows the column only as it emits changes.
    fn reserve(&mut self, writes: usize);

    /// `reader`'s row reads `id` from now on (`true`) or no longer (`false`): the newest write of
    /// a pair wins.
    fn write(&mut self, id: CellId, reader: Name, reads: bool);

    /// This request's writes of `id`'s readers into `out` (cleared first): each reader once,
    /// with its newest write, ascending by name. May sort what was written since the last
    /// question (`&mut`): the one sort a phase's writes get.
    fn edits_of(&mut self, id: CellId, out: &mut SVec<'a, (Name, bool)>);

    /// Every write of the request, newest per pair, sorted by cell then reader.
    fn finish(self, alloc: SharedAlloc<'a>) -> ReaderEdits<'a>
    where
        Self: Sized;

    /// What the index holds and has cost so far.
    fn cost(&self) -> IndexCost;
}

/// The engine's reader index: `(cell, reader) -> reads` in sorted columns (the module's doc).
pub struct Paired<'a> {
    pairs: Runs<'a, (CellId, Name), bool>,
    /// A question's scratch, kept across questions: `(reader, age, reads)`.
    scratch: SVec<'a, (Name, u64, bool)>,
    /// Growths of the scratch: allocations too.
    scratch_grown: u64,
    writes: u64,
    plant: Plant,
}

impl<'a> ReaderIndex<'a> for Paired<'a> {
    fn new_in(alloc: SharedAlloc<'a>, plant: Plant) -> Paired<'a> {
        Paired { pairs: Runs::new_in(alloc), scratch: Vec::new_in(alloc), scratch_grown: 0, writes: 0, plant }
    }

    fn reserve(&mut self, writes: usize) {
        self.pairs.reserve(writes);
    }

    fn write(&mut self, id: CellId, reader: Name, reads: bool) {
        self.pairs.write((id, reader), reads);
        self.writes += 1;
    }

    fn edits_of(&mut self, id: CellId, out: &mut SVec<'a, (Name, bool)>) {
        out.clear();
        self.pairs.seal();
        let Paired { pairs, scratch, scratch_grown, plant, .. } = self;
        scratch.clear();
        let had = scratch.capacity();
        pairs.each_in(|key| key.0.cmp(&id), |key, reads, age| scratch.push((key.1, age, *reads)));
        *scratch_grown += u64::from(scratch.capacity() != had);
        // Within a reader, the newest write last.
        scratch.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for group in scratch.chunk_by(|a, b| a.0 == b.0) {
            let newest = group[group.len() - 1];
            out.push((newest.0, newest.2));
        }
        if *plant == Plant::LastReaderDropped
            && let Some(at) = out.iter().rposition(|(_, reads)| *reads)
        {
            // The planted bug: the cell's last reader is lost.
            let _lost = out.remove(at);
        }
    }

    fn finish(self, _alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        ReaderEdits { edits: self.pairs.into_sorted() }
    }

    fn cost(&self) -> IndexCost {
        let runs = self.pairs.cost();
        IndexCost {
            writes: self.writes,
            edges: runs.entries,
            bytes: runs.bytes + (self.scratch.capacity() * size_of::<(Name, u64, bool)>()) as u64,
            allocations: runs.allocations + self.scratch_grown,
            phase_allocations_max: 0,
            runs: runs.runs,
            seals: runs.seals,
            exact: true,
        }
    }
}

/// The index as it was before the columns (the module's doc): each cell's readers this request
/// added (`true`) or dropped (`false`), a map a cell.
pub struct Mapped<'a> {
    alloc: SharedAlloc<'a>,
    writes: u64,
    readers: SMap<'a, CellId, SMap<'a, Name, bool>>,
}

/// Entries a blaze-own B-tree node holds at most (btree.rs `CAPACITY`, `2 * B - 1` with `B = 8`):
/// a map of `n` entries holds at least `ceil(n / NODE)` nodes.
const NODE: u64 = 15;

impl<'a> ReaderIndex<'a> for Mapped<'a> {
    fn new_in(alloc: SharedAlloc<'a>, _plant: Plant) -> Mapped<'a> {
        Mapped { alloc, writes: 0, readers: BTreeMap::new_in(alloc) }
    }

    fn reserve(&mut self, _writes: usize) {}

    fn write(&mut self, id: CellId, reader: Name, reads: bool) {
        let alloc = self.alloc;
        self.readers.entry(id).or_insert_with(|| BTreeMap::new_in(alloc)).insert(reader, reads);
        self.writes += 1;
    }

    fn edits_of(&mut self, id: CellId, out: &mut SVec<'a, (Name, bool)>) {
        out.clear();
        if let Some(delta) = self.readers.get(&id) {
            for (reader, reads) in delta.iter() {
                out.push((*reader, *reads));
            }
        }
    }

    fn finish(self, alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        let count: usize = self.readers.values().map(|delta| delta.len()).sum();
        let mut edits: SVec<'a, ((CellId, Name), bool)> = Vec::with_capacity_in(count, alloc);
        for (id, delta) in self.readers.iter() {
            for (reader, reads) in delta.iter() {
                edits.push(((*id, *reader), *reads));
            }
        }
        ReaderEdits { edits }
    }

    fn cost(&self) -> IndexCost {
        let cells = self.readers.len() as u64;
        let mut edges = 0u64;
        let mut nodes = cells.div_ceil(NODE);
        for delta in self.readers.values() {
            let n = delta.len() as u64;
            edges += n;
            nodes += n.div_ceil(NODE).max(1);
        }
        let bytes = edges * size_of::<(Name, bool)>() as u64 + cells * size_of::<(CellId, SMap<'a, Name, bool>)>() as u64;
        IndexCost { writes: self.writes, edges, bytes, allocations: nodes, phase_allocations_max: 0, runs: 0, seals: 0, exact: false }
    }
}
