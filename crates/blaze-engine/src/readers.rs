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

use core::cmp::Ordering;
use core::mem::size_of;

use blaze_drive::cells::Cell;
use blaze_drive::key::{Kind, Name};
use blaze_hash::{DigestKey, DigestState, Map, MapOpts, digest_word, fold_part};
use blaze_own::{BTreeMap, SharedAlloc, Vec};

use crate::engine::{CellId, Plant, SMap, SVec};

/// The bytes an edge needs at the least: a cell and a reader, as the engine names them.
pub const FLOOR_BYTES_PER_EDGE: usize = size_of::<(CellId, Name)>();

/// The bytes one interned edge occupies in a sealed run: `cell: u32` plus `(reader << 1) | bit`.
pub const COMPACT_EDGE_BYTES: usize = size_of::<CompactEdge>();

/// Maximum unique interned reader index that fits in 31 bits alongside the `reads` flag bit.
const MAX_READER_ID: u32 = u32::MAX >> 1;

/// Tail buffer capacity retained after a seal (bytes): larger tails are released back to the
/// allocator so a heavy phase does not pin idle scratch for the rest of the request.
const TAIL_RETAIN_BYTES: usize = 64 * 1024;

#[inline(always)]
const fn kind_u8(kind: Kind) -> u8 {
    match kind {
        Kind::Units => 1,
        Kind::UnitGraph => 2,
        Kind::Fetch => 3,
        Kind::FileItems => 4,
        Kind::ModuleScope => 5,
        Kind::Group => 6,
        Kind::Expansion => 7,
        Kind::Signature => 8,
        Kind::ImplBucket => 9,
        Kind::BodyCheck => 10,
        Kind::Code => 11,
        Kind::Test => 12,
        Kind::CrateScope => 13,
        Kind::MacroModule => 14,
        Kind::Surface => 15,
        Kind::Owners => 16,
        Kind::Script => 17,
        Kind::Links => 18,
        Kind::Native => 19,
        Kind::Given => 20,
        Kind::Lint => 21,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct NameKey(Name);

impl DigestKey for NameKey {
    #[inline(always)]
    fn digest_word(&self) -> u64 {
        fold_part(digest_word(&self.0.subject.0), u64::from(kind_u8(self.0.kind)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CompactCell {
    of: u32,
    cell: Cell,
}

impl DigestKey for CompactCell {
    #[inline(always)]
    fn digest_word(&self) -> u64 {
        let (chunks, _) = self.cell.0.as_chunks::<8>();
        let c0 = chunks.first().copied().map_or(0, u64::from_le_bytes);
        let c1 = chunks.get(1).copied().map_or(0, u64::from_le_bytes);
        fold_part(fold_part(fold_part(0, u64::from(self.of)), c0), c1)
    }
}

/// One sealed compact edge: 8 bytes total `(cell_u32, (reader_u31 << 1) | reads_bit)`.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct CompactEdge {
    cell: u32,
    reader_and_bit: u32,
}

impl CompactEdge {
    #[inline(always)]
    const fn new(cell: u32, reader: u32, reads: bool) -> Self {
        Self { cell, reader_and_bit: (reader << 1) | if reads { 1 } else { 0 } }
    }

    #[inline(always)]
    const fn reader(self) -> u32 {
        self.reader_and_bit >> 1
    }

    #[inline(always)]
    const fn reads(self) -> bool {
        (self.reader_and_bit & 1) != 0
    }

    #[inline(always)]
    const fn pair_key(self) -> (u32, u32) {
        (self.cell, self.reader_and_bit >> 1)
    }
}

/// One unsealed write in the tail, carrying its insertion `place` so unstable sort keeps the
/// newest write of each `(cell, reader)` pair last.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct TailEdge {
    cell: u32,
    reader_and_bit: u32,
    place: u32,
}

impl TailEdge {
    #[inline(always)]
    const fn pair_key(self) -> (u32, u32) {
        (self.cell, self.reader_and_bit >> 1)
    }

    #[inline(always)]
    const fn into_compact(self) -> CompactEdge {
        CompactEdge { cell: self.cell, reader_and_bit: self.reader_and_bit }
    }
}

const _: () = assert!(size_of::<CompactEdge>() == 8);

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

/// The engine's reader index: `Name` and `CellId` interned into dense tables, with membership
/// edges held as 8-byte sorted columns `(cell_u32, (reader_u31 << 1) | reads_bit)`.
pub struct Paired<'a> {
    alloc: SharedAlloc<'a>,
    /// Dense interned table of unique `Name`s (both readers and cell definers).
    names: SVec<'a, Name>,
    name_map: Map<NameKey, u32, DigestState, SharedAlloc<'a>>,
    /// Dense interned table of unique `CellId`s as `(definer_name_id, cell)` (20 bytes each).
    cells: SVec<'a, CompactCell>,
    cell_map: Map<CompactCell, u32, DigestState, SharedAlloc<'a>>,
    /// Sealed runs, oldest first: each sorted strictly by `(cell, reader)`.
    runs: SVec<'a, SVec<'a, CompactEdge>>,
    /// Unsealed writes since the last seal; released after big seals.
    tail: SVec<'a, TailEdge>,
    /// A question's scratch, kept across questions: `(reader_id, age, reads)`.
    scratch: SVec<'a, (u32, u64, bool)>,
    allocations: u64,
    seals: u32,
    writes: u64,
    plant: Plant,
}

impl<'a> Paired<'a> {
    fn intern_name(&mut self, name: Name) -> u32 {
        let key = NameKey(name);
        if let Some(&id) = self.name_map.get(&key) {
            return id;
        }
        let id = u32::try_from(self.names.len()).unwrap_or(MAX_READER_ID);
        let had_vec = self.names.capacity();
        self.names.push(name);
        self.allocations += u64::from(self.names.capacity() != had_vec);
        let had_map = self.name_map.capacity();
        self.name_map.insert(key, id);
        self.allocations += u64::from(self.name_map.capacity() != had_map);
        id
    }

    fn intern_cell(&mut self, id: CellId) -> u32 {
        let of = self.intern_name(id.of);
        let key = CompactCell { of, cell: id.cell };
        if let Some(&cid) = self.cell_map.get(&key) {
            return cid;
        }
        let cid = u32::try_from(self.cells.len()).unwrap_or(u32::MAX);
        let had_vec = self.cells.capacity();
        self.cells.push(key);
        self.allocations += u64::from(self.cells.capacity() != had_vec);
        let had_map = self.cell_map.capacity();
        self.cell_map.insert(key, cid);
        self.allocations += u64::from(self.cell_map.capacity() != had_map);
        cid
    }

    fn lookup_cell(&self, id: CellId) -> Option<u32> {
        let &of = self.name_map.get(&NameKey(id.of))?;
        self.cell_map.get(&CompactCell { of, cell: id.cell }).copied()
    }

    fn seal(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        self.seals = self.seals.saturating_add(1);
        self.tail.sort_unstable_by(|a, b| a.pair_key().cmp(&b.pair_key()).then(a.place.cmp(&b.place)));
        let unique = 1 + self.tail.windows(2).filter(|pair| pair.first().map(|e| e.pair_key()) != pair.get(1).map(|e| e.pair_key())).count();
        let mut run: SVec<'a, CompactEdge> = Vec::with_capacity_in(unique, self.alloc);
        self.allocations += 1;
        for group in self.tail.chunk_by(|a, b| a.pair_key() == b.pair_key()) {
            if let Some(&newest) = group.last() {
                run.push(newest.into_compact());
            }
        }
        self.tail.clear();
        if self.tail.capacity().saturating_mul(size_of::<TailEdge>()) > TAIL_RETAIN_BYTES {
            self.tail = Vec::new_in(self.alloc);
        }
        let had = self.runs.capacity();
        self.runs.push(run);
        self.allocations += u64::from(self.runs.capacity() != had);
        while self.runs.len() >= 2 {
            let n = self.runs.len();
            let older_len = self.runs.get(n - 2).map_or(0, Vec::len);
            let newer_len = self.runs.get(n - 1).map_or(0, Vec::len);
            if older_len > newer_len.saturating_mul(2) {
                break;
            }
            let (Some(newer), Some(older)) = (self.runs.pop(), self.runs.pop()) else {
                break;
            };
            let both = merge_compact_runs(&older, &newer, self.alloc);
            self.allocations += 1;
            self.runs.push(both);
        }
    }
}

fn merge_compact_runs<'a>(older: &[CompactEdge], newer: &[CompactEdge], alloc: SharedAlloc<'a>) -> SVec<'a, CompactEdge> {
    let mut out: SVec<'a, CompactEdge> = Vec::with_capacity_in(older.len().saturating_add(newer.len()), alloc);
    let (mut i, mut j) = (0usize, 0usize);
    while let (Some(&a), Some(&b)) = (older.get(i), newer.get(j)) {
        match a.pair_key().cmp(&b.pair_key()) {
            Ordering::Less => {
                out.push(a);
                i += 1;
            }
            Ordering::Greater => {
                out.push(b);
                j += 1;
            }
            Ordering::Equal => {
                out.push(b);
                i += 1;
                j += 1;
            }
        }
    }
    if let Some(rest) = older.get(i..) {
        out.extend_from_slice(rest);
    }
    if let Some(rest) = newer.get(j..) {
        out.extend_from_slice(rest);
    }
    out
}

impl<'a> ReaderIndex<'a> for Paired<'a> {
    fn new_in(alloc: SharedAlloc<'a>, plant: Plant) -> Paired<'a> {
        Paired {
            alloc,
            names: Vec::new_in(alloc),
            name_map: Map::new(MapOpts { hasher: DigestState, alloc, capacity: None }),
            cells: Vec::new_in(alloc),
            cell_map: Map::new(MapOpts { hasher: DigestState, alloc, capacity: None }),
            runs: Vec::new_in(alloc),
            tail: Vec::new_in(alloc),
            scratch: Vec::new_in(alloc),
            allocations: 0,
            seals: 0,
            writes: 0,
            plant,
        }
    }

    fn reserve(&mut self, writes: usize) {
        let cap = usize::try_from(u32::MAX).unwrap_or(usize::MAX);
        let want = writes.min(cap.saturating_sub(self.tail.len()));
        let had = self.tail.capacity();
        self.tail.reserve(want);
        self.allocations += u64::from(self.tail.capacity() != had);
        // Pre-size dense interners once on large batches so phase 0 does not double repeatedly.
        if writes >= 512 && self.names.is_empty() && self.cells.is_empty() {
            let est_names = (writes / 64).clamp(64, 4_096);
            let est_cells = (writes / 4).clamp(256, 65_536);
            let h1 = self.names.capacity();
            self.names.reserve(est_names);
            self.allocations += u64::from(self.names.capacity() != h1);
            let h2 = self.name_map.capacity();
            self.name_map.reserve(est_names);
            self.allocations += u64::from(self.name_map.capacity() != h2);
            let h3 = self.cells.capacity();
            self.cells.reserve(est_cells);
            self.allocations += u64::from(self.cells.capacity() != h3);
            let h4 = self.cell_map.capacity();
            self.cell_map.reserve(est_cells);
            self.allocations += u64::from(self.cell_map.capacity() != h4);
        }
    }

    fn write(&mut self, id: CellId, reader: Name, reads: bool) {
        let cap = usize::try_from(u32::MAX).unwrap_or(usize::MAX);
        if self.tail.len() >= cap {
            self.seal();
        }
        let cell = self.intern_cell(id);
        let reader_id = self.intern_name(reader);
        let place = u32::try_from(self.tail.len()).unwrap_or(u32::MAX);
        let edge = CompactEdge::new(cell, reader_id, reads);
        let had = self.tail.capacity();
        self.tail.push(TailEdge { cell: edge.cell, reader_and_bit: edge.reader_and_bit, place });
        self.allocations += u64::from(self.tail.capacity() != had);
        self.writes += 1;
    }

    fn edits_of(&mut self, id: CellId, out: &mut SVec<'a, (Name, bool)>) {
        out.clear();
        let Some(cell_id) = self.lookup_cell(id) else {
            return;
        };
        self.seal();
        let Paired { names, runs, scratch, allocations, plant, .. } = self;
        scratch.clear();
        let had = scratch.capacity();
        for (age, run) in (0u64..).zip(runs.iter()) {
            let from = run.partition_point(|edge| edge.cell < cell_id);
            for &edge in run.get(from..).unwrap_or(&[]) {
                if edge.cell != cell_id {
                    break;
                }
                scratch.push((edge.reader(), age, edge.reads()));
            }
        }
        *allocations += u64::from(scratch.capacity() != had);
        // Within a reader id, the newest write last.
        scratch.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for group in scratch.chunk_by(|a, b| a.0 == b.0) {
            if let Some(&(reader_id, _, reads)) = group.last()
                && let Some(&reader) = names.get(usize::try_from(reader_id).unwrap_or(usize::MAX))
            {
                out.push((reader, reads));
            }
        }
        // Preserve exact ascending `Name` order (Rule 25).
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if *plant == Plant::LastReaderDropped
            && let Some(at) = out.iter().rposition(|(_, reads)| *reads)
        {
            // The planted bug: the cell's last reader is lost.
            let _lost = out.remove_shift(at);
        }
    }

    fn finish(mut self, alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        self.seal();
        let Paired { names, name_map, cells, cell_map, mut runs, tail, scratch, .. } = self;
        drop(name_map);
        drop(cell_map);
        drop(tail);
        drop(scratch);
        while runs.len() >= 2 {
            let (Some(newer), Some(older)) = (runs.pop(), runs.pop()) else {
                break;
            };
            let both = merge_compact_runs(&older, &newer, alloc);
            runs.push(both);
        }
        let Some(merged) = runs.pop() else {
            return ReaderEdits::new_in(alloc);
        };
        drop(runs);
        if merged.is_empty() {
            return ReaderEdits::new_in(alloc);
        }

        // Compute canonical ascending `Name` and `CellId` ranks so we sort 8-byte `u64` keys
        // instead of 83-byte tuples.
        let mut name_order: SVec<'a, u32> = Vec::with_capacity_in(names.len(), alloc);
        for i in 0..names.len() {
            name_order.push(u32::try_from(i).unwrap_or(u32::MAX));
        }
        name_order.sort_unstable_by(|&a, &b| {
            let na = names.get(usize::try_from(a).unwrap_or(usize::MAX));
            let nb = names.get(usize::try_from(b).unwrap_or(usize::MAX));
            na.cmp(&nb)
        });
        let mut name_rank: SVec<'a, u32> = Vec::from_elem_in(0u32, names.len(), alloc);
        for (rank, &id) in name_order.iter().enumerate() {
            if let Some(slot) = name_rank.get_mut(usize::try_from(id).unwrap_or(usize::MAX)) {
                *slot = u32::try_from(rank).unwrap_or(MAX_READER_ID);
            }
        }

        let mut cell_order: SVec<'a, u32> = Vec::with_capacity_in(cells.len(), alloc);
        for i in 0..cells.len() {
            cell_order.push(u32::try_from(i).unwrap_or(u32::MAX));
        }
        cell_order.sort_unstable_by(|&a, &b| {
            let ca = cells.get(usize::try_from(a).unwrap_or(usize::MAX));
            let cb = cells.get(usize::try_from(b).unwrap_or(usize::MAX));
            match (ca, cb) {
                (Some(ca), Some(cb)) => {
                    let ra = name_rank.get(usize::try_from(ca.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
                    let rb = name_rank.get(usize::try_from(cb.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
                    ra.cmp(&rb).then_with(|| ca.cell.cmp(&cb.cell))
                }
                _ => Ordering::Equal,
            }
        });
        let mut cell_rank: SVec<'a, u32> = Vec::from_elem_in(0u32, cells.len(), alloc);
        for (rank, &id) in cell_order.iter().enumerate() {
            if let Some(slot) = cell_rank.get_mut(usize::try_from(id).unwrap_or(usize::MAX)) {
                *slot = u32::try_from(rank).unwrap_or(u32::MAX);
            }
        }

        let mut packed: SVec<'a, u64> = Vec::with_capacity_in(merged.len(), alloc);
        for edge in merged {
            let cr = cell_rank.get(usize::try_from(edge.cell).unwrap_or(usize::MAX)).copied().unwrap_or(0);
            let nr = name_rank.get(usize::try_from(edge.reader()).unwrap_or(usize::MAX)).copied().unwrap_or(0);
            let rb = (nr << 1) | u32::from(edge.reads());
            packed.push((u64::from(cr) << 32) | u64::from(rb));
        }
        drop(cell_rank);
        drop(name_rank);
        packed.sort_unstable();

        let mut edits: SVec<'a, ((CellId, Name), bool)> = Vec::with_capacity_in(packed.len(), alloc);
        for p in packed {
            let cr = usize::try_from(p >> 32).unwrap_or(usize::MAX);
            let rb = u32::try_from(p & 0xFFFF_FFFF).unwrap_or(0);
            let nr = usize::try_from(rb >> 1).unwrap_or(usize::MAX);
            let reads = (rb & 1) != 0;
            if let (Some(&cid_idx), Some(&nid_idx)) = (cell_order.get(cr), name_order.get(nr))
                && let Some(&cc) = cells.get(usize::try_from(cid_idx).unwrap_or(usize::MAX))
                && let Some(&definer) = names.get(usize::try_from(cc.of).unwrap_or(usize::MAX))
                && let Some(&reader) = names.get(usize::try_from(nid_idx).unwrap_or(usize::MAX))
            {
                edits.push(((CellId { of: definer, cell: cc.cell }, reader), reads));
            }
        }
        ReaderEdits { edits }
    }

    fn cost(&self) -> IndexCost {
        let edges: usize = self.runs.iter().map(Vec::len).sum::<usize>().saturating_add(self.tail.len());
        let runs_bytes: usize = self
            .runs
            .iter()
            .map(|run| run.capacity().saturating_mul(size_of::<CompactEdge>()))
            .sum::<usize>()
            .saturating_add(self.runs.capacity().saturating_mul(size_of::<SVec<'a, CompactEdge>>()))
            .saturating_add(self.tail.capacity().saturating_mul(size_of::<TailEdge>()));
        let intern_bytes: usize = self
            .names
            .capacity()
            .saturating_mul(size_of::<Name>())
            .saturating_add(self.cells.capacity().saturating_mul(size_of::<CompactCell>()))
            .saturating_add(self.name_map.capacity().saturating_mul(size_of::<(NameKey, u32)>() + 1))
            .saturating_add(self.cell_map.capacity().saturating_mul(size_of::<(CompactCell, u32)>() + 1));
        let scratch_bytes = self.scratch.capacity().saturating_mul(size_of::<(u32, u64, bool)>());
        let total_bytes = runs_bytes.saturating_add(intern_bytes).saturating_add(scratch_bytes);
        IndexCost {
            writes: self.writes,
            edges: u64::try_from(edges).unwrap_or(u64::MAX),
            bytes: u64::try_from(total_bytes).unwrap_or(u64::MAX),
            allocations: self.allocations,
            phase_allocations_max: 0,
            runs: u32::try_from(self.runs.len()).unwrap_or(u32::MAX),
            seals: self.seals,
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
