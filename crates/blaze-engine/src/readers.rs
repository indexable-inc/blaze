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

enum EditsRepr<'a> {
    /// Interned 8-byte-per-edge representation produced by [`Paired::finish`].
    ///
    /// Invariants:
    /// - `names` is sorted strictly ascending by `Name`.
    /// - `cells` is sorted strictly ascending by `(names[cc.of], cc.cell)` (i.e. canonical `CellId`
    ///   order), and every `cc.of` is a valid index into `names`.
    /// - `edges` is sorted strictly ascending by `(edge.cell, edge.reader())`, each pair at most
    ///   once, with every `edge.cell` valid in `cells` and every `edge.reader()` valid in `names`.
    Compact {
        edges: SVec<'a, CompactEdge>,
        cells: SVec<'a, CompactCell>,
        names: SVec<'a, Name>,
    },
    /// Flat representation used by [`Mapped::finish`] (the test oracle) and tiny hand-built deltas.
    ///
    /// Invariant: sorted strictly ascending by `(CellId, Name)`, each pair at most once.
    Flat(SVec<'a, ((CellId, Name), bool)>),
}

/// The request's reader writes at its end, each `(cell, reader)` once with its newest write
/// (`true`: reads it from now on; `false`: no longer), sorted by cell then reader: what a form's
/// fold reads (overlay.rs `Work::readers`). Stored compactly at 8 bytes per edge for [`Paired`].
pub struct ReaderEdits<'a> {
    repr: EditsRepr<'a>,
}

impl<'a> ReaderEdits<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        ReaderEdits { repr: EditsRepr::Flat(Vec::new_in(alloc)) }
    }

    /// A single `(cell, reader) -> reads` membership edit.
    pub fn from_single(id: CellId, reader: Name, reads: bool, alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        let mut edits = Vec::with_capacity_in(1, alloc);
        edits.push(((id, reader), reads));
        ReaderEdits { repr: EditsRepr::Flat(edits) }
    }

    /// Wraps a flat column already sorted strictly ascending by `(CellId, Name)`.
    pub fn from_sorted_flat(edits: SVec<'a, ((CellId, Name), bool)>) -> ReaderEdits<'a> {
        ReaderEdits { repr: EditsRepr::Flat(edits) }
    }

    /// Each cell written, ascending, with its writes (ascending by reader).
    pub fn iter(&self) -> ReaderEditsIter<'_> {
        let inner = match &self.repr {
            EditsRepr::Compact { edges, cells, names } => {
                ReaderEditsIterInner::Compact { edges: edges.as_slice(), cells: cells.as_slice(), names: names.as_slice() }
            }
            EditsRepr::Flat(edits) => ReaderEditsIterInner::Flat(edits.as_slice()),
        };
        ReaderEditsIter { inner }
    }

    /// Every `((CellId, Name), bool)` write, strictly ascending by `(CellId, Name)`.
    pub fn pairs(&self) -> impl Iterator<Item = ((CellId, Name), bool)> + '_ {
        self.iter().flat_map(|(id, edits)| edits.iter().map(move |(reader, reads)| ((id, reader), reads)))
    }

    /// The `index`-th `((CellId, Name), bool)` write in ascending order, decoded in `O(1)`.
    pub fn get(&self, index: usize) -> Option<((CellId, Name), bool)> {
        match &self.repr {
            EditsRepr::Compact { edges, cells, names } => {
                let edge = *edges.get(index)?;
                let cc = *cells.get(usize::try_from(edge.cell).unwrap_or(usize::MAX))?;
                let definer = *names.get(usize::try_from(cc.of).unwrap_or(usize::MAX))?;
                let reader = *names.get(usize::try_from(edge.reader()).unwrap_or(usize::MAX))?;
                Some(((CellId { of: definer, cell: cc.cell }, reader), edge.reads()))
            }
            EditsRepr::Flat(edits) => edits.get(index).copied(),
        }
    }

    /// The writes for `cell` alone, ascending by reader, in `O(log n)` with zero allocation.
    pub fn edits_of(&self, cell: CellId) -> Option<Edits<'_>> {
        match &self.repr {
            EditsRepr::Compact { edges, cells, names } => {
                let of_idx = u32::try_from(names.binary_search(&cell.of).ok()?).ok()?;
                let cell_idx = u32::try_from(
                    cells.binary_search_by(|cc| cc.of.cmp(&of_idx).then_with(|| cc.cell.cmp(&cell.cell))).ok()?,
                )
                .ok()?;
                let from = edges.partition_point(|edge| edge.cell < cell_idx);
                let rest = edges.get(from..)?;
                let len = rest.partition_point(|edge| edge.cell == cell_idx);
                let slice = rest.get(..len)?;
                (!slice.is_empty()).then_some(Edits { slice: EditsSlice::Compact { edges: slice, names: names.as_slice() } })
            }
            EditsRepr::Flat(edits) => {
                let from = edits.partition_point(|((id, _), _)| *id < cell);
                let rest = edits.get(from..)?;
                let len = rest.partition_point(|((id, _), _)| *id == cell);
                let slice = rest.get(..len)?;
                (!slice.is_empty()).then_some(Edits { slice: EditsSlice::Flat(slice) })
            }
        }
    }

    /// Whether any write removes a reader (`reads == false`) from a cell owned by `kind`.
    pub fn any_removed_of(&self, kind: Kind) -> bool {
        match &self.repr {
            EditsRepr::Compact { edges, cells, names } => {
                for edge in edges.iter() {
                    if !edge.reads()
                        && let Some(cc) = cells.get(usize::try_from(edge.cell).unwrap_or(usize::MAX))
                        && let Some(definer) = names.get(usize::try_from(cc.of).unwrap_or(usize::MAX))
                        && definer.kind == kind
                    {
                        return true;
                    }
                }
                false
            }
            EditsRepr::Flat(edits) => edits.iter().any(|((id, _), added)| !*added && id.of.kind == kind),
        }
    }

    /// The writes held: one a `(cell, reader)`.
    pub fn len(&self) -> usize {
        match &self.repr {
            EditsRepr::Compact { edges, .. } => edges.len(),
            EditsRepr::Flat(edits) => edits.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Exact heap capacity in bytes held by this delta.
    pub fn byte_size(&self) -> usize {
        match &self.repr {
            EditsRepr::Compact { edges, cells, names } => edges
                .capacity()
                .saturating_mul(size_of::<CompactEdge>())
                .saturating_add(cells.capacity().saturating_mul(size_of::<CompactCell>()))
                .saturating_add(names.capacity().saturating_mul(size_of::<Name>())),
            EditsRepr::Flat(edits) => edits.capacity().saturating_mul(size_of::<((CellId, Name), bool)>()),
        }
    }

    /// Keeps only the writes for which `keep(cell, reader, reads)` returns `Ok(true)`, in place.
    pub fn try_retain<E>(&mut self, mut keep: impl FnMut(CellId, Name, bool) -> Result<bool, E>) -> Result<(), E> {
        match &mut self.repr {
            EditsRepr::Compact { edges, cells, names } => {
                let mut kept = 0usize;
                for at in 0..edges.len() {
                    let Some(&edge) = edges.get(at) else { break };
                    let Some(&cc) = cells.get(usize::try_from(edge.cell).unwrap_or(usize::MAX)) else { continue };
                    let Some(&definer) = names.get(usize::try_from(cc.of).unwrap_or(usize::MAX)) else { continue };
                    let Some(&reader) = names.get(usize::try_from(edge.reader()).unwrap_or(usize::MAX)) else { continue };
                    if keep(CellId { of: definer, cell: cc.cell }, reader, edge.reads())? {
                        if let Some(dst) = edges.get_mut(kept) {
                            *dst = edge;
                        }
                        kept += 1;
                    }
                }
                edges.truncate(kept);
            }
            EditsRepr::Flat(edits) => {
                let mut kept = 0usize;
                for at in 0..edits.len() {
                    let Some(&entry) = edits.get(at) else { break };
                    let ((id, reader), added) = entry;
                    if keep(id, reader, added)? {
                        if let Some(dst) = edits.get_mut(kept) {
                            *dst = entry;
                        }
                        kept += 1;
                    }
                }
                edits.truncate(kept);
            }
        }
        Ok(())
    }

    /// Drops every removal tombstone (`reads == false`), in place (for `Plant::LostReaderTombstone`).
    pub fn retain_added(&mut self) {
        match &mut self.repr {
            EditsRepr::Compact { edges, .. } => edges.retain(|edge| edge.reads()),
            EditsRepr::Flat(edits) => edits.retain(|(_, added)| *added),
        }
    }

    /// Merges two sorted deltas keeping the newest write (`newer`) on equal `(cell, reader)` pairs.
    /// Preserves compact 8-byte-per-edge storage whenever either side is compact.
    pub fn merge_newer(older: ReaderEdits<'a>, newer: ReaderEdits<'a>, alloc: SharedAlloc<'a>) -> ReaderEdits<'a> {
        if older.is_empty() {
            return newer;
        }
        if newer.is_empty() {
            return older;
        }
        match (older.repr, newer.repr) {
            (EditsRepr::Flat(left), EditsRepr::Flat(right)) => {
                let mut out: SVec<'a, ((CellId, Name), bool)> = Vec::with_capacity_in(left.len().saturating_add(right.len()), alloc);
                let (mut i, mut j) = (0usize, 0usize);
                while let (Some(&a), Some(&b)) = (left.get(i), right.get(j)) {
                    match a.0.cmp(&b.0) {
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
                if let Some(rest) = left.get(i..) {
                    out.extend_from_slice(rest);
                }
                if let Some(rest) = right.get(j..) {
                    out.extend_from_slice(rest);
                }
                ReaderEdits { repr: EditsRepr::Flat(out) }
            }
            (left_repr, right_repr) => {
                let (e1, c1, n1) = into_compact_parts(left_repr, alloc);
                let (e2, c2, n2) = into_compact_parts(right_repr, alloc);
                merge_compact_edits(e1, c1, n1, e2, c2, n2, alloc)
            }
        }
    }
}

fn into_compact_parts<'a>(repr: EditsRepr<'a>, alloc: SharedAlloc<'a>) -> (SVec<'a, CompactEdge>, SVec<'a, CompactCell>, SVec<'a, Name>) {
    match repr {
        EditsRepr::Compact { edges, cells, names } => (edges, cells, names),
        EditsRepr::Flat(flat) => {
            let mut paired = Paired::new_in(alloc, Plant::None);
            paired.reserve(flat.len());
            for ((id, reader), reads) in flat {
                paired.write(id, reader, reads);
            }
            match paired.finish(alloc).repr {
                EditsRepr::Compact { edges, cells, names } => (edges, cells, names),
                EditsRepr::Flat(_) => (Vec::new_in(alloc), Vec::new_in(alloc), Vec::new_in(alloc)),
            }
        }
    }
}

fn merge_compact_edits<'a>(
    e1: SVec<'a, CompactEdge>,
    c1: SVec<'a, CompactCell>,
    n1: SVec<'a, Name>,
    e2: SVec<'a, CompactEdge>,
    c2: SVec<'a, CompactCell>,
    n2: SVec<'a, Name>,
    alloc: SharedAlloc<'a>,
) -> ReaderEdits<'a> {
    let mut names: SVec<'a, Name> = Vec::with_capacity_in(n1.len().max(n2.len()), alloc);
    let mut n1_map: SVec<'a, u32> = Vec::from_elem_in(0u32, n1.len(), alloc);
    let mut n2_map: SVec<'a, u32> = Vec::from_elem_in(0u32, n2.len(), alloc);
    let (mut i, mut j) = (0usize, 0usize);
    while let (Some(&a), Some(&b)) = (n1.get(i), n2.get(j)) {
        let idx = u32::try_from(names.len()).unwrap_or(MAX_READER_ID);
        match a.cmp(&b) {
            Ordering::Less => {
                names.push(a);
                if let Some(slot) = n1_map.get_mut(i) {
                    *slot = idx;
                }
                i += 1;
            }
            Ordering::Greater => {
                names.push(b);
                if let Some(slot) = n2_map.get_mut(j) {
                    *slot = idx;
                }
                j += 1;
            }
            Ordering::Equal => {
                names.push(a);
                if let Some(slot) = n1_map.get_mut(i) {
                    *slot = idx;
                }
                if let Some(slot) = n2_map.get_mut(j) {
                    *slot = idx;
                }
                i += 1;
                j += 1;
            }
        }
    }
    while let Some(&a) = n1.get(i) {
        let idx = u32::try_from(names.len()).unwrap_or(MAX_READER_ID);
        names.push(a);
        if let Some(slot) = n1_map.get_mut(i) {
            *slot = idx;
        }
        i += 1;
    }
    while let Some(&b) = n2.get(j) {
        let idx = u32::try_from(names.len()).unwrap_or(MAX_READER_ID);
        names.push(b);
        if let Some(slot) = n2_map.get_mut(j) {
            *slot = idx;
        }
        j += 1;
    }
    drop(n1);
    drop(n2);

    let mut cells: SVec<'a, CompactCell> = Vec::with_capacity_in(c1.len().max(c2.len()), alloc);
    let mut c1_map: SVec<'a, u32> = Vec::from_elem_in(0u32, c1.len(), alloc);
    let mut c2_map: SVec<'a, u32> = Vec::from_elem_in(0u32, c2.len(), alloc);
    let (mut i, mut j) = (0usize, 0usize);
    while let (Some(&ca), Some(&cb)) = (c1.get(i), c2.get(j)) {
        let oa = n1_map.get(usize::try_from(ca.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
        let ob = n2_map.get(usize::try_from(cb.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
        let idx = u32::try_from(cells.len()).unwrap_or(u32::MAX);
        match oa.cmp(&ob).then_with(|| ca.cell.cmp(&cb.cell)) {
            Ordering::Less => {
                cells.push(CompactCell { of: oa, cell: ca.cell });
                if let Some(slot) = c1_map.get_mut(i) {
                    *slot = idx;
                }
                i += 1;
            }
            Ordering::Greater => {
                cells.push(CompactCell { of: ob, cell: cb.cell });
                if let Some(slot) = c2_map.get_mut(j) {
                    *slot = idx;
                }
                j += 1;
            }
            Ordering::Equal => {
                cells.push(CompactCell { of: oa, cell: ca.cell });
                if let Some(slot) = c1_map.get_mut(i) {
                    *slot = idx;
                }
                if let Some(slot) = c2_map.get_mut(j) {
                    *slot = idx;
                }
                i += 1;
                j += 1;
            }
        }
    }
    while let Some(&ca) = c1.get(i) {
        let oa = n1_map.get(usize::try_from(ca.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
        let idx = u32::try_from(cells.len()).unwrap_or(u32::MAX);
        cells.push(CompactCell { of: oa, cell: ca.cell });
        if let Some(slot) = c1_map.get_mut(i) {
            *slot = idx;
        }
        i += 1;
    }
    while let Some(&cb) = c2.get(j) {
        let ob = n2_map.get(usize::try_from(cb.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
        let idx = u32::try_from(cells.len()).unwrap_or(u32::MAX);
        cells.push(CompactCell { of: ob, cell: cb.cell });
        if let Some(slot) = c2_map.get_mut(j) {
            *slot = idx;
        }
        j += 1;
    }
    drop(c1);
    drop(c2);

    let remap = |edge: CompactEdge, cm: &[u32], nm: &[u32]| -> CompactEdge {
        let c = cm.get(usize::try_from(edge.cell).unwrap_or(usize::MAX)).copied().unwrap_or(0);
        let r = nm.get(usize::try_from(edge.reader()).unwrap_or(usize::MAX)).copied().unwrap_or(0);
        CompactEdge::new(c, r, edge.reads())
    };
    let mut edges: SVec<'a, CompactEdge> = Vec::with_capacity_in(e1.len().saturating_add(e2.len()), alloc);
    let (mut i, mut j) = (0usize, 0usize);
    while let (Some(&raw_a), Some(&raw_b)) = (e1.get(i), e2.get(j)) {
        let a = remap(raw_a, &c1_map, &n1_map);
        let b = remap(raw_b, &c2_map, &n2_map);
        match a.pair_key().cmp(&b.pair_key()) {
            Ordering::Less => {
                edges.push(a);
                i += 1;
            }
            Ordering::Greater => {
                edges.push(b);
                j += 1;
            }
            Ordering::Equal => {
                edges.push(b);
                i += 1;
                j += 1;
            }
        }
    }
    while let Some(&raw_a) = e1.get(i) {
        edges.push(remap(raw_a, &c1_map, &n1_map));
        i += 1;
    }
    while let Some(&raw_b) = e2.get(j) {
        edges.push(remap(raw_b, &c2_map, &n2_map));
        j += 1;
    }
    ReaderEdits { repr: EditsRepr::Compact { edges, cells, names } }
}

enum ReaderEditsIterInner<'e> {
    Compact {
        edges: &'e [CompactEdge],
        cells: &'e [CompactCell],
        names: &'e [Name],
    },
    Flat(&'e [((CellId, Name), bool)]),
}

/// Iterator over cells in ascending [`CellId`] order, yielding each cell's [`Edits`].
pub struct ReaderEditsIter<'e> {
    inner: ReaderEditsIterInner<'e>,
}

impl<'e> Iterator for ReaderEditsIter<'e> {
    type Item = (CellId, Edits<'e>);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            ReaderEditsIterInner::Compact { edges, cells, names } => {
                let first = edges.first()?;
                let cell_idx = first.cell;
                let mut end = 1usize;
                while let Some(next) = edges.get(end)
                    && next.cell == cell_idx
                {
                    end += 1;
                }
                let (group, rest) = edges.split_at_checked(end)?;
                *edges = rest;
                let cc = *cells.get(usize::try_from(cell_idx).unwrap_or(usize::MAX))?;
                let definer = *names.get(usize::try_from(cc.of).unwrap_or(usize::MAX))?;
                Some((
                    CellId { of: definer, cell: cc.cell },
                    Edits { slice: EditsSlice::Compact { edges: group, names } },
                ))
            }
            ReaderEditsIterInner::Flat(edits) => {
                let first = edits.first()?;
                let cell_id = first.0.0;
                let mut end = 1usize;
                while let Some(next) = edits.get(end)
                    && next.0.0 == cell_id
                {
                    end += 1;
                }
                let (group, rest) = edits.split_at_checked(end)?;
                *edits = rest;
                Some((cell_id, Edits { slice: EditsSlice::Flat(group) }))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum EditsSlice<'e> {
    Compact {
        edges: &'e [CompactEdge],
        names: &'e [Name],
    },
    Flat(&'e [((CellId, Name), bool)]),
}

/// One cell's writes, ascending by reader.
#[derive(Clone, Copy)]
pub struct Edits<'e> {
    slice: EditsSlice<'e>,
}

impl<'e> Edits<'e> {
    /// Each reader written and whether it reads the cell from now on.
    pub fn iter(&self) -> EditsIter<'e> {
        EditsIter { slice: self.slice, at: 0 }
    }

    pub fn len(&self) -> usize {
        match self.slice {
            EditsSlice::Compact { edges, .. } => edges.len(),
            EditsSlice::Flat(edits) => edits.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Looks up `reader` in this cell's sorted writes in `O(log k)`.
    pub fn get(&self, reader: &Name) -> Option<bool> {
        match self.slice {
            EditsSlice::Compact { edges, names } => {
                let nr = u32::try_from(names.binary_search(reader).ok()?).ok()?;
                let at = edges.binary_search_by_key(&nr, |edge| edge.reader()).ok()?;
                edges.get(at).map(|edge| edge.reads())
            }
            EditsSlice::Flat(edits) => {
                let at = edits.binary_search_by_key(reader, |((_, r), _)| *r).ok()?;
                edits.get(at).map(|(_, reads)| *reads)
            }
        }
    }
}

/// Iterator over one cell's `(Name, bool)` writes, ascending by reader.
#[derive(Clone, Copy)]
pub struct EditsIter<'e> {
    slice: EditsSlice<'e>,
    at: usize,
}

impl Iterator for EditsIter<'_> {
    type Item = (Name, bool);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let item = match self.slice {
            EditsSlice::Compact { edges, names } => {
                let edge = *edges.get(self.at)?;
                let reader = *names.get(usize::try_from(edge.reader()).unwrap_or(usize::MAX))?;
                (reader, edge.reads())
            }
            EditsSlice::Flat(edits) => {
                let ((_, reader), reads) = *edits.get(self.at)?;
                (reader, reads)
            }
        };
        self.at = self.at.saturating_add(1);
        Some(item)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match self.slice {
            EditsSlice::Compact { edges, .. } => edges.len().saturating_sub(self.at),
            EditsSlice::Flat(edits) => edits.len().saturating_sub(self.at),
        };
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for EditsIter<'_> {}

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
        let Some(mut merged) = runs.pop() else {
            return ReaderEdits::new_in(alloc);
        };
        drop(runs);
        if merged.is_empty() {
            return ReaderEdits::new_in(alloc);
        }

        // Compute canonical ascending `Name` and `CellId` ranks so we remap and sort the 8-byte
        // `CompactEdge` column in place without materializing 83-byte tuples.
        let mut name_order: SVec<'a, u32> = Vec::with_capacity_in(names.len(), alloc);
        for i in 0..names.len() {
            name_order.push(u32::try_from(i).unwrap_or(u32::MAX));
        }
        name_order.sort_unstable_by(|&a, &b| {
            let na = names.get(usize::try_from(a).unwrap_or(usize::MAX));
            let nb = names.get(usize::try_from(b).unwrap_or(usize::MAX));
            na.cmp(&nb)
        });
        let mut sorted_names: SVec<'a, Name> = Vec::with_capacity_in(names.len(), alloc);
        let mut name_rank: SVec<'a, u32> = Vec::from_elem_in(0u32, names.len(), alloc);
        for (rank, &id) in name_order.iter().enumerate() {
            let idx = usize::try_from(id).unwrap_or(usize::MAX);
            if let Some(&name) = names.get(idx) {
                sorted_names.push(name);
            }
            if let Some(slot) = name_rank.get_mut(idx) {
                *slot = u32::try_from(rank).unwrap_or(MAX_READER_ID);
            }
        }
        drop(name_order);
        drop(names);

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
        let mut sorted_cells: SVec<'a, CompactCell> = Vec::with_capacity_in(cells.len(), alloc);
        let mut cell_rank: SVec<'a, u32> = Vec::from_elem_in(0u32, cells.len(), alloc);
        for (rank, &id) in cell_order.iter().enumerate() {
            let idx = usize::try_from(id).unwrap_or(usize::MAX);
            if let Some(&cc) = cells.get(idx) {
                let of = name_rank.get(usize::try_from(cc.of).unwrap_or(usize::MAX)).copied().unwrap_or(0);
                sorted_cells.push(CompactCell { of, cell: cc.cell });
            }
            if let Some(slot) = cell_rank.get_mut(idx) {
                *slot = u32::try_from(rank).unwrap_or(u32::MAX);
            }
        }
        drop(cell_order);
        drop(cells);

        for edge in merged.iter_mut() {
            let cr = cell_rank.get(usize::try_from(edge.cell).unwrap_or(usize::MAX)).copied().unwrap_or(0);
            let nr = name_rank.get(usize::try_from(edge.reader()).unwrap_or(usize::MAX)).copied().unwrap_or(0);
            edge.cell = cr;
            edge.reader_and_bit = (nr << 1) | u32::from(edge.reads());
        }
        drop(cell_rank);
        drop(name_rank);
        merged.sort_unstable_by_key(|edge| (u64::from(edge.cell) << 32) | u64::from(edge.reader_and_bit));

        ReaderEdits {
            repr: EditsRepr::Compact {
                edges: merged,
                cells: sorted_cells,
                names: sorted_names,
            },
        }
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
        ReaderEdits::from_sorted_flat(edits)
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

#[cfg(test)]
mod compact_reader_edits_tests {
    use super::*;
    use blaze_id::ContentId;

    fn mk_name(kind: Kind, n: u32) -> Name {
        Name::new(kind, ContentId::of("compact reader edits name", &n.to_le_bytes()))
    }

    fn mk_cell(kind: Kind, owner: u32, tag: u32) -> CellId {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&tag.to_le_bytes());
        CellId { of: mk_name(kind, owner), cell: Cell(bytes) }
    }

    fn collect_via_iter<'a>(
        edits: &ReaderEdits<'_>,
        alloc: SharedAlloc<'a>,
    ) -> Vec<((CellId, Name), bool), SharedAlloc<'a>> {
        let mut out = Vec::new_in(alloc);
        for (id, delta) in edits.iter() {
            for (reader, reads) in delta.iter() {
                out.push(((id, reader), reads));
            }
        }
        out
    }

    #[test]
    fn compact_finish_memory_footprint_and_oracle_equivalence() {
        let alloc = blaze_own::testing::heap().shared();
        let mut paired = Paired::new_in(alloc, Plant::None);
        let mut mapped = Mapped::new_in(alloc, Plant::None);
        let mut oracle: SMap<'_, (CellId, Name), bool> = SMap::new_in(alloc);

        // 10,000 edges across 100 unique cells and 100 unique reader names, with overwrites
        // and deletions (`reads = false`).
        for c in 0..100u32 {
            let kind = if c % 3 == 0 { Kind::Signature } else { Kind::Given };
            let cid = mk_cell(kind, c % 25, c);
            for r in 0..100u32 {
                let reader = mk_name(Kind::BodyCheck, r);
                let reads = (c + r) % 5 != 0;
                paired.write(cid, reader, reads);
                mapped.write(cid, reader, reads);
                oracle.insert((cid, reader), reads);
            }
        }
        // Overwrite a subset to exercise newest-wins and seal transitions.
        let mut scratch = Vec::new_in(alloc);
        paired.edits_of(mk_cell(Kind::Signature, 0, 0), &mut scratch);
        for i in 0..50u32 {
            let cid = mk_cell(if i % 3 == 0 { Kind::Signature } else { Kind::Given }, i % 25, i);
            let reader = mk_name(Kind::BodyCheck, (i * 7) % 100);
            let reads = i % 2 == 0;
            paired.write(cid, reader, reads);
            mapped.write(cid, reader, reads);
            oracle.insert((cid, reader), reads);
        }

        let compact = paired.finish(alloc);
        let flat = mapped.finish(alloc);
        let mut expected = Vec::new_in(alloc);
        expected.extend(oracle.iter().map(|(&k, &v)| (k, v)));
        expected.sort_unstable_by_key(|&(k, _)| k);

        assert_eq!(compact.len(), 10_000);
        assert!(!compact.is_empty());
        assert_eq!(compact.len(), flat.len());
        assert_eq!(compact.len(), expected.len());

        // Strict memory bound: O(edges * 8 + cells * 20 + names * 33), far below 83 B/edge flat floor.
        let bytes_per_edge = compact.byte_size() / compact.len();
        assert!(
            bytes_per_edge <= 16,
            "compact ReaderEdits must stay <= 16 B/edge (got {bytes_per_edge} B/edge, {} bytes for {} edges; flat is {} B/edge)",
            compact.byte_size(),
            compact.len(),
            flat.byte_size() / flat.len(),
        );
        assert!(compact.byte_size() * 5 < flat.byte_size(), "compact ({}) must be >5x smaller than flat ({})", compact.byte_size(), flat.byte_size());

        // Exact ordering & element equivalence via iter(), pairs(), and get(i).
        assert_eq!(collect_via_iter(&compact, alloc).as_slice(), expected.as_slice());
        assert_eq!(collect_via_iter(&flat, alloc).as_slice(), expected.as_slice());
        assert!(compact.pairs().eq(expected.iter().copied()));
        for (i, &want) in expected.iter().enumerate() {
            assert_eq!(compact.get(i), Some(want));
            assert_eq!(flat.get(i), Some(want));
        }
        assert_eq!(compact.get(expected.len()), None);

        // Point lookups (`edits_of`, `Edits::get`, `any_removed_of`).
        assert!(compact.any_removed_of(Kind::Signature));
        assert!(compact.any_removed_of(Kind::Given));
        assert!(!compact.any_removed_of(Kind::Units));
        assert_eq!(compact.any_removed_of(Kind::Signature), flat.any_removed_of(Kind::Signature));
        assert_eq!(compact.any_removed_of(Kind::Units), flat.any_removed_of(Kind::Units));

        let present = mk_cell(Kind::Signature, 0, 0);
        let absent = mk_cell(Kind::Units, 999, 999);
        assert!(compact.edits_of(absent).is_none());
        assert!(flat.edits_of(absent).is_none());
        let dc = compact.edits_of(present).expect("present cell in compact");
        let df = flat.edits_of(present).expect("present cell in flat");
        assert_eq!(dc.len(), df.len());
        assert!(dc.iter().eq(df.iter()));
        for r in 0..100u32 {
            let reader = mk_name(Kind::BodyCheck, r);
            assert_eq!(dc.get(&reader), df.get(&reader));
            assert_eq!(dc.get(&reader), oracle.get(&(present, reader)).copied());
        }
        assert_eq!(dc.get(&mk_name(Kind::Units, 999)), None);
    }

    #[test]
    fn compact_empty_single_retain_and_merge_equivalence() {
        let alloc = blaze_own::testing::heap().shared();

        // Empty index.
        let empty_p = Paired::new_in(alloc, Plant::None).finish(alloc);
        let empty_m = Mapped::new_in(alloc, Plant::None).finish(alloc);
        assert!(empty_p.is_empty() && empty_m.is_empty());
        assert_eq!(empty_p.len(), 0);
        assert_eq!(empty_p.get(0), None);
        assert!(empty_p.iter().next().is_none());
        assert!(!empty_p.any_removed_of(Kind::Given));

        // Single-edge index (both `true` and `false`).
        let c0 = mk_cell(Kind::Given, 1, 1);
        let r0 = mk_name(Kind::BodyCheck, 1);
        let mut p1 = Paired::new_in(alloc, Plant::None);
        p1.write(c0, r0, true);
        p1.write(c0, r0, false);
        let mut single = p1.finish(alloc);
        assert_eq!(single.len(), 1);
        assert_eq!(single.get(0), Some(((c0, r0), false)));
        assert!(single.any_removed_of(Kind::Given));
        single.retain_added();
        assert!(single.is_empty());
        assert!(!single.any_removed_of(Kind::Given));
        assert!(single.edits_of(c0).is_none());

        // `try_retain` and `merge_newer` across Compact + Compact and Compact + Flat.
        let c1 = mk_cell(Kind::Signature, 2, 2);
        let c2 = mk_cell(Kind::Given, 3, 3);
        let r1 = mk_name(Kind::BodyCheck, 10);
        let r2 = mk_name(Kind::BodyCheck, 20);

        let mut p_old = Paired::new_in(alloc, Plant::None);
        p_old.write(c0, r0, true);
        p_old.write(c1, r1, true);
        p_old.write(c1, r2, false);
        let mut old_c = p_old.finish(alloc);

        let mut m_old = Mapped::new_in(alloc, Plant::None);
        m_old.write(c0, r0, true);
        m_old.write(c1, r1, true);
        m_old.write(c1, r2, false);
        let mut old_f = m_old.finish(alloc);

        let filter = |id: CellId, reader: Name, _added: bool| -> Result<bool, ()> { Ok(!(id == c1 && reader == r1)) };
        old_c.try_retain(filter).expect("retain compact");
        old_f.try_retain(filter).expect("retain flat");
        assert!(old_c.pairs().eq(old_f.pairs()));

        let mut p_new = Paired::new_in(alloc, Plant::None);
        p_new.write(c0, r0, false);
        p_new.write(c2, r2, true);
        let new_c = p_new.finish(alloc);

        let mut m_new = Mapped::new_in(alloc, Plant::None);
        m_new.write(c0, r0, false);
        m_new.write(c2, r2, true);
        let new_f = m_new.finish(alloc);

        let merged_cc = ReaderEdits::merge_newer(old_c, new_c, alloc);
        let merged_ff = ReaderEdits::merge_newer(old_f, new_f, alloc);
        assert!(merged_cc.pairs().eq(merged_ff.pairs()));
        assert_eq!(merged_cc.len(), 3);
        assert_eq!(merged_cc.edits_of(c0).and_then(|d| d.get(&r0)), Some(false));
        assert_eq!(merged_cc.edits_of(c1).and_then(|d| d.get(&r2)), Some(false));
        assert_eq!(merged_cc.edits_of(c2).and_then(|d| d.get(&r2)), Some(true));

        // Compact + Flat merge keeps compact representation.
        let single_flat = ReaderEdits::from_single(c2, r0, true, alloc);
        let merged_cf = ReaderEdits::merge_newer(merged_cc, single_flat, alloc);
        assert_eq!(merged_cf.len(), 4);
        assert_eq!(merged_cf.edits_of(c2).and_then(|d| d.get(&r0)), Some(true));
        assert_eq!(merged_cf.edits_of(c2).and_then(|d| d.get(&r2)), Some(true));
    }
}
