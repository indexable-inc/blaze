//! `BEP6` compact in-place stored answer page codec: per-page sorted unique dictionaries for
//! `Name`, `CellId`, and `ContentId` plus sparse `CAUSES` encoding (omitting the 113 zero bytes
//! of non-`ReadMoved` causes). Every page is read directly from its mapped bytes with zero
//! allocations and zero decompression (`VerifiedPage::open`), and two builders of the same page
//! always write byte-identical output (Rule 25).

use core::alloc::Allocator;

use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name};
use blaze_hash::{DigestKey, DigestState, Map, MapOpts, digest_word, fold_part};
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, Vec};

use super::view::{self, PageDicts, VerifiedPage};
use super::{
    CAUSES, CELL, CELL_ID, CELLS, DEFINE, FAILED, FILE_MEMBER, FILES, ID, INDEXED, INLINE, NAME,
    OWED_CELLS, OWED_TASKS, PAGE_MAGIC, READ, READERS, RECORDS, ROW_FIXED, ROWS, SECTIONS, SVec,
    put_id, put_name, put_phase, put_u32,
};
use crate::engine::{Cause, CellId, Gen, Phase, Planned, Row, Stage};

pub(super) const IDX: usize = 4;
/// Fixed header before the `(n + 1)` start table:
/// magic [4] | bits [1] | index [4] | names_count [4] | cells_count [4] | ids_count [4] | 8 x count [32].
pub(super) const HEAD_V6: usize = 4 + 1 + 4 + 4 * 3 + 4 * SECTIONS;
/// One `cells_dict` entry: `name_idx u32 | cell [16]`.
pub(super) const DICT_CELL: usize = IDX + CELL;
/// Fixed prefix of a `BEP6` row:
/// `name_idx | key_idx | phase [3] | file_idx | source_idx | out_idx | serial u64 | findings u32 | flags u8 | defines u32 | reads u32`.
pub(super) const ROW_FIXED_V6: usize = IDX + IDX + 3 + IDX + IDX + IDX + 8 + 4 + 1 + 4 + 4;
/// One `BEP6` row define entry: `cell_idx u32 | digest_idx u32`.
pub(super) const DEFINE_V6: usize = IDX + IDX;
/// One `BEP6` row read entry: `cell_idx u32 | digest_idx u32`.
pub(super) const READ_V6: usize = IDX + IDX;
/// One `BEP6` `CELLS` entry: `cell_idx u32 | digest_idx u32 | gen u64`.
pub(super) const CELL_ENTRY_V6: usize = IDX + IDX + 8;
/// One `BEP6` `OWED_CELLS` entry: `cell_idx u32 | gen u64`.
pub(super) const OWED_CELL_V6: usize = IDX + 8;
/// One `BEP6` `OWED_TASKS` entry: `name_idx u32 | phase [3] | source_idx u32 | file_idx u32`.
pub(super) const OWED_TASK_V6: usize = IDX + 3 + IDX + IDX;
/// One `BEP6` `FILES` member entry: `name_idx u32 | standing u8 | key_idx u32 | phase [3] | source_idx u32`.
pub(super) const FILE_MEMBER_V6: usize = IDX + 1 + IDX + 3 + IDX;
/// Short `BEP6` `CAUSES` entry (`New`, `KeyMoved`, `Owed`, `Inline`): `name_idx u32 | gen u64 | tag u8`.
pub(super) const CAUSE_SHORT_V6: usize = IDX + 8 + 1;
/// Full `BEP6` `CAUSES` entry (`ReadMoved`): short prefix + `cell_idx u32 | was_idx u32 | now_idx u32`.
pub(super) const CAUSE_READ_MOVED_V6: usize = CAUSE_SHORT_V6 + IDX + IDX + IDX;
/// One `BEP6` `RECORDS` entry: `name_idx u32 | key_idx u32`.
pub(super) const RECORD_V6: usize = IDX + IDX;

#[inline(always)]
pub(super) const fn kind_u8(kind: Kind) -> u8 {
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
struct CellKey(CellId);

impl DigestKey for CellKey {
    #[inline(always)]
    fn digest_word(&self) -> u64 {
        let w0 = fold_part(digest_word(&self.0.of.subject.0), u64::from(kind_u8(self.0.of.kind)));
        let (chunks, _) = self.0.cell.0.as_chunks::<8>();
        let c0 = chunks.first().copied().map_or(0, u64::from_le_bytes);
        let c1 = chunks.get(1).copied().map_or(0, u64::from_le_bytes);
        fold_part(fold_part(w0, c0), c1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct IdKey(ContentId);

impl DigestKey for IdKey {
    #[inline(always)]
    fn digest_word(&self) -> u64 {
        digest_word(&self.0.0)
    }
}

/// Per-page deterministic dictionary builder: collects every unique `Name`, `CellId`, and
/// `ContentId` referenced by the page's surviving entries, sorts each table strictly ascending,
/// and assigns canonical `u32` indices so every encoding path (`encode`, `encode_par`,
/// `encode_changes`, `try_repage`) emits byte-for-byte identical pages (Rule 25).
pub(super) struct DictBuilder<A: Allocator + Copy> {
    names: Vec<Name, A>,
    name_map: Map<NameKey, u32, DigestState, A>,
    cells: Vec<CellId, A>,
    cell_map: Map<CellKey, u32, DigestState, A>,
    ids: Vec<ContentId, A>,
    id_map: Map<IdKey, u32, DigestState, A>,
}

impl<A: Allocator + Copy> DictBuilder<A> {
    pub(super) fn new_in(alloc: A) -> Self {
        Self {
            names: Vec::new_in(alloc),
            name_map: Map::new(MapOpts { hasher: DigestState, alloc, capacity: None }),
            cells: Vec::new_in(alloc),
            cell_map: Map::new(MapOpts { hasher: DigestState, alloc, capacity: None }),
            ids: Vec::new_in(alloc),
            id_map: Map::new(MapOpts { hasher: DigestState, alloc, capacity: None }),
        }
    }

    #[inline]
    pub(super) fn add_name(&mut self, name: Name) {
        let key = NameKey(name);
        if self.name_map.get(&key).is_none() {
            self.name_map.insert(key, 0);
            self.names.push(name);
        }
    }

    #[inline]
    pub(super) fn add_cell(&mut self, cell: CellId) {
        self.add_name(cell.of);
        let key = CellKey(cell);
        if self.cell_map.get(&key).is_none() {
            self.cell_map.insert(key, 0);
            self.cells.push(cell);
        }
    }

    #[inline]
    pub(super) fn add_id(&mut self, id: ContentId) {
        let key = IdKey(id);
        if self.id_map.get(&key).is_none() {
            self.id_map.insert(key, 0);
            self.ids.push(id);
        }
    }

    /// Sort all three tables strictly ascending and assign canonical `0..len` indices.
    pub(super) fn finalize(&mut self) {
        self.names.sort_unstable();
        for (i, &n) in self.names.iter().enumerate() {
            self.name_map.insert(NameKey(n), u32::try_from(i).unwrap_or(u32::MAX));
        }
        self.cells.sort_unstable();
        for (i, &c) in self.cells.iter().enumerate() {
            self.cell_map.insert(CellKey(c), u32::try_from(i).unwrap_or(u32::MAX));
        }
        self.ids.sort_unstable();
        for (i, &id) in self.ids.iter().enumerate() {
            self.id_map.insert(IdKey(id), u32::try_from(i).unwrap_or(u32::MAX));
        }
    }

    pub(super) fn names_len(&self) -> usize {
        self.names.len()
    }

    pub(super) fn cells_len(&self) -> usize {
        self.cells.len()
    }

    pub(super) fn ids_len(&self) -> usize {
        self.ids.len()
    }

    #[inline]
    pub(super) fn name_idx(&self, name: Name) -> u32 {
        self.name_map.get(&NameKey(name)).copied().unwrap_or(0)
    }

    #[inline]
    pub(super) fn cell_idx(&self, cell: &CellId) -> u32 {
        self.cell_map.get(&CellKey(*cell)).copied().unwrap_or(0)
    }

    #[inline]
    pub(super) fn id_idx(&self, id: &ContentId) -> u32 {
        self.id_map.get(&IdKey(*id)).copied().unwrap_or(0)
    }

    pub(super) fn write_dicts(&self, out: &mut SVec<'_, u8>) {
        for &n in self.names.iter() {
            put_name(out, n);
        }
        for c in self.cells.iter() {
            out.extend_from_slice(&self.name_idx(c.of).to_le_bytes());
            out.extend_from_slice(&c.cell.0);
        }
        for id in self.ids.iter() {
            put_id(out, id);
        }
    }
}

/// Byte offset where the first section entry begins in a `BEP6` page.
pub(super) fn body_start_v6(entries: usize, names: usize, cells: usize, ids: usize) -> Option<usize> {
    let starts = entries.checked_add(1)?.checked_mul(IDX)?;
    let n_bytes = names.checked_mul(NAME)?;
    let c_bytes = cells.checked_mul(DICT_CELL)?;
    let i_bytes = ids.checked_mul(ID)?;
    HEAD_V6.checked_add(starts)?.checked_add(n_bytes)?.checked_add(c_bytes)?.checked_add(i_bytes)
}

#[inline]
pub(super) fn row_len_v6(defines: usize, reads: usize, indexed: bool) -> usize {
    ROW_FIXED_V6 + DEFINE_V6 * defines + READ_V6 * reads + if indexed { IDX * reads } else { 0 }
}

#[inline]
pub(super) fn cause_len_v6(cause: Cause) -> usize {
    match cause {
        Cause::ReadMoved { .. } => CAUSE_READ_MOVED_V6,
        Cause::New | Cause::KeyMoved | Cause::Owed | Cause::Inline => CAUSE_SHORT_V6,
    }
}

pub(super) fn write_header_v6(
    page: &mut SVec<'_, u8>,
    bits: u8,
    index: u32,
    dicts: &DictBuilder<impl Allocator + Copy>,
    counts: [usize; SECTIONS],
) {
    page.extend_from_slice(PAGE_MAGIC);
    page.push(bits);
    page.extend_from_slice(&index.to_le_bytes());
    put_u32(page, dicts.names_len());
    put_u32(page, dicts.cells_len());
    put_u32(page, dicts.ids_len());
    for count in counts {
        put_u32(page, count);
    }
}

#[inline]
pub(super) fn put_idx(out: &mut SVec<'_, u8>, idx: u32) {
    out.extend_from_slice(&idx.to_le_bytes());
}

pub(super) fn collect_row_v6<A: Allocator + Copy>(
    name: Name,
    row: &Row<'_>,
    indexed: bool,
    dicts: &mut DictBuilder<A>,
) -> usize {
    dicts.add_name(name);
    dicts.add_id(row.key.0);
    dicts.add_name(row.file);
    dicts.add_id(row.source);
    dicts.add_id(row.out);
    for &(cell, digest) in row.defines.iter() {
        dicts.add_cell(CellId { of: name, cell });
        dicts.add_id(digest);
    }
    for &(id, digest) in row.reads.iter() {
        dicts.add_cell(id);
        dicts.add_id(digest);
    }
    row_len_v6(row.defines.len(), row.reads.len(), indexed)
}

pub(super) fn write_row_v6<A: Allocator + Copy>(
    out: &mut SVec<'_, u8>,
    name: Name,
    row: &Row<'_>,
    indexed: bool,
    dicts: &DictBuilder<A>,
) {
    put_idx(out, dicts.name_idx(name));
    put_idx(out, dicts.id_idx(&row.key.0));
    put_phase(out, row.phase);
    put_idx(out, dicts.name_idx(row.file));
    put_idx(out, dicts.id_idx(&row.source));
    put_idx(out, dicts.id_idx(&row.out));
    out.extend_from_slice(&row.serial.0.to_le_bytes());
    out.extend_from_slice(&row.findings.to_le_bytes());
    out.push(u8::from(row.inline) * INLINE | u8::from(row.failed) * FAILED | u8::from(indexed) * INDEXED);
    put_u32(out, row.defines.len());
    put_u32(out, row.reads.len());
    for &(cell, digest) in row.defines.iter() {
        put_idx(out, dicts.cell_idx(&CellId { of: name, cell }));
        put_idx(out, dicts.id_idx(&digest));
    }
    for &(id, digest) in row.reads.iter() {
        put_idx(out, dicts.cell_idx(&id));
        put_idx(out, dicts.id_idx(&digest));
    }
    if indexed {
        for at in row.by_cell.iter() {
            out.extend_from_slice(&at.to_le_bytes());
        }
    }
}

pub(super) fn collect_cause_v6<A: Allocator + Copy>(
    name: Name,
    cause: Cause,
    dicts: &mut DictBuilder<A>,
) -> usize {
    dicts.add_name(name);
    if let Cause::ReadMoved { read, was, now } = cause {
        dicts.add_cell(read);
        dicts.add_id(was);
        dicts.add_id(now);
    }
    cause_len_v6(cause)
}

pub(super) fn write_cause_v6<A: Allocator + Copy>(
    out: &mut SVec<'_, u8>,
    name: Name,
    serial: Gen,
    cause: Cause,
    dicts: &DictBuilder<A>,
) {
    put_idx(out, dicts.name_idx(name));
    out.extend_from_slice(&serial.0.to_le_bytes());
    match cause {
        Cause::New => out.push(0),
        Cause::KeyMoved => out.push(1),
        Cause::ReadMoved { read, was, now } => {
            out.push(2);
            put_idx(out, dicts.cell_idx(&read));
            put_idx(out, dicts.id_idx(&was));
            put_idx(out, dicts.id_idx(&now));
        }
        Cause::Owed => out.push(3),
        Cause::Inline => out.push(4),
    }
}

/// Source of one pre-serialized section entry when merging or re-paging into a fresh `BEP6` page.
#[derive(Clone, Copy)]
pub(super) enum EntrySource<'p> {
    /// Flat staging bytes produced by `changes.rs` (`put_row`, `put_cause`, etc.).
    Flat(&'p [u8]),
    /// Existing `BEP6` entry borrowed from a previous `VerifiedPage`.
    Dict { dicts: PageDicts<'p>, bytes: &'p [u8] },
}

impl<'p> EntrySource<'p> {
    pub(super) fn from_page(page: &'p VerifiedPage<'p>, bytes: &'p [u8]) -> Self {
        Self::Dict { dicts: page.dicts(), bytes }
    }
}

/// Intern every `Name`, `CellId`, and `ContentId` referenced by `src` into `dicts` and return its
/// exact `BEP6` byte length.
pub(super) fn collect_entry_v6<A: Allocator + Copy>(
    section: usize,
    src: EntrySource<'_>,
    dicts: &mut DictBuilder<A>,
) -> usize {
    match src {
        EntrySource::Flat(b) => collect_flat_entry(section, b, dicts),
        EntrySource::Dict { dicts: old, bytes: b } => collect_dict_entry(section, old, b, dicts),
    }
}

/// Encode `src` into `out` using `dicts`. For `RECORDS`, returns the `(Name, Key)` pair.
pub(super) fn write_entry_v6<A: Allocator + Copy>(
    section: usize,
    src: EntrySource<'_>,
    dicts: &DictBuilder<A>,
    out: &mut SVec<'_, u8>,
) -> Option<(Name, Key)> {
    match src {
        EntrySource::Flat(b) => write_flat_entry(section, b, dicts, out),
        EntrySource::Dict { dicts: old, bytes: b } => write_dict_entry(section, old, b, dicts, out),
    }
}

fn read_cell_from(b: &[u8]) -> Cell {
    let mut c = [0u8; CELL];
    if let Some(src) = b.get(..CELL) {
        c.copy_from_slice(src);
    }
    Cell(c)
}

fn read_cell_id_from(b: &[u8]) -> CellId {
    CellId { of: view::name(b), cell: read_cell_from(b.get(NAME..).unwrap_or(&[])) }
}

fn collect_flat_entry<A: Allocator + Copy>(section: usize, b: &[u8], dicts: &mut DictBuilder<A>) -> usize {
    match section {
        ROWS => {
            let name = view::name(b);
            dicts.add_name(name);
            dicts.add_id(view::id(b.get(NAME..).unwrap_or(&[])));
            dicts.add_name(view::name(b.get(NAME + ID + 3..).unwrap_or(&[])));
            dicts.add_id(view::id(b.get(NAME + ID + 3 + NAME..).unwrap_or(&[])));
            dicts.add_id(view::id(b.get(NAME + ID + 3 + NAME + ID..).unwrap_or(&[])));
            let flags = b.get(ROW_FIXED - 9).copied().unwrap_or(0);
            let defs = usize::try_from(view::word(b.get(ROW_FIXED - 8..).unwrap_or(&[]))).unwrap_or(0);
            let reads = usize::try_from(view::word(b.get(ROW_FIXED - 4..).unwrap_or(&[]))).unwrap_or(0);
            let def_bytes = b.get(ROW_FIXED..ROW_FIXED + defs * DEFINE).unwrap_or(&[]);
            for chunk in def_bytes.chunks_exact(DEFINE) {
                dicts.add_cell(CellId { of: name, cell: read_cell_from(chunk) });
                dicts.add_id(view::id(chunk.get(CELL..).unwrap_or(&[])));
            }
            let read_off = ROW_FIXED + defs * DEFINE;
            let read_bytes = b.get(read_off..read_off + reads * READ).unwrap_or(&[]);
            for chunk in read_bytes.chunks_exact(READ) {
                dicts.add_cell(read_cell_id_from(chunk));
                dicts.add_id(view::id(chunk.get(CELL_ID..).unwrap_or(&[])));
            }
            row_len_v6(defs, reads, (flags & INDEXED) != 0)
        }
        CELLS => {
            dicts.add_cell(read_cell_id_from(b));
            dicts.add_id(view::id(b.get(CELL_ID..).unwrap_or(&[])));
            CELL_ENTRY_V6
        }
        OWED_CELLS => {
            dicts.add_cell(read_cell_id_from(b));
            OWED_CELL_V6
        }
        OWED_TASKS => {
            dicts.add_name(view::name(b));
            dicts.add_id(view::id(b.get(NAME + 3..).unwrap_or(&[])));
            dicts.add_name(view::name(b.get(NAME + 3 + ID..).unwrap_or(&[])));
            OWED_TASK_V6
        }
        FILES => {
            dicts.add_name(view::name(b));
            let members = b.get(NAME + 4..).unwrap_or(&[]);
            let mut count = 0usize;
            for m in members.chunks_exact(FILE_MEMBER) {
                count += 1;
                dicts.add_name(view::name(m));
                if m.get(NAME).copied() == Some(1) {
                    dicts.add_id(view::id(m.get(NAME + 1..).unwrap_or(&[])));
                    dicts.add_id(view::id(m.get(NAME + 1 + ID + 3..).unwrap_or(&[])));
                }
            }
            IDX + 4 + count * FILE_MEMBER_V6
        }
        CAUSES => {
            dicts.add_name(view::name(b));
            let tag = b.get(NAME + 8).copied().unwrap_or(0);
            if tag == 2 {
                dicts.add_cell(read_cell_id_from(b.get(NAME + 9..).unwrap_or(&[])));
                dicts.add_id(view::id(b.get(NAME + 9 + CELL_ID..).unwrap_or(&[])));
                dicts.add_id(view::id(b.get(NAME + 9 + CELL_ID + ID..).unwrap_or(&[])));
                CAUSE_READ_MOVED_V6
            } else {
                CAUSE_SHORT_V6
            }
        }
        RECORDS => {
            dicts.add_name(view::name(b));
            dicts.add_id(view::id(b.get(NAME..).unwrap_or(&[])));
            RECORD_V6
        }
        READERS => {
            dicts.add_cell(read_cell_id_from(b));
            let tail = b.get(CELL_ID + 4..).unwrap_or(&[]);
            let mut count = 0usize;
            for chunk in tail.chunks_exact(NAME) {
                count += 1;
                dicts.add_name(view::name(chunk));
            }
            IDX + 4 + count * IDX
        }
        _ => 0,
    }
}

fn write_flat_entry<A: Allocator + Copy>(
    section: usize,
    b: &[u8],
    dicts: &DictBuilder<A>,
    out: &mut SVec<'_, u8>,
) -> Option<(Name, Key)> {
    match section {
        ROWS => {
            let name = view::name(b);
            let key_id = view::id(b.get(NAME..).unwrap_or(&[]));
            let phase_bytes = b.get(NAME + ID..NAME + ID + 3).unwrap_or(&[0; 3]);
            let file = view::name(b.get(NAME + ID + 3..).unwrap_or(&[]));
            let source = view::id(b.get(NAME + ID + 3 + NAME..).unwrap_or(&[]));
            let out_id = view::id(b.get(NAME + ID + 3 + NAME + ID..).unwrap_or(&[]));
            let tail_fixed = b.get(NAME + ID + 3 + NAME + ID + ID..ROW_FIXED).unwrap_or(&[]);
            let flags = b.get(ROW_FIXED - 9).copied().unwrap_or(0);
            let defs = usize::try_from(view::word(b.get(ROW_FIXED - 8..).unwrap_or(&[]))).unwrap_or(0);
            let reads = usize::try_from(view::word(b.get(ROW_FIXED - 4..).unwrap_or(&[]))).unwrap_or(0);
            put_idx(out, dicts.name_idx(name));
            put_idx(out, dicts.id_idx(&key_id));
            out.extend_from_slice(phase_bytes);
            put_idx(out, dicts.name_idx(file));
            put_idx(out, dicts.id_idx(&source));
            put_idx(out, dicts.id_idx(&out_id));
            out.extend_from_slice(tail_fixed);
            let def_bytes = b.get(ROW_FIXED..ROW_FIXED + defs * DEFINE).unwrap_or(&[]);
            for chunk in def_bytes.chunks_exact(DEFINE) {
                let cid = CellId { of: name, cell: read_cell_from(chunk) };
                put_idx(out, dicts.cell_idx(&cid));
                put_idx(out, dicts.id_idx(&view::id(chunk.get(CELL..).unwrap_or(&[]))));
            }
            let read_off = ROW_FIXED + defs * DEFINE;
            let read_bytes = b.get(read_off..read_off + reads * READ).unwrap_or(&[]);
            for chunk in read_bytes.chunks_exact(READ) {
                let cid = read_cell_id_from(chunk);
                put_idx(out, dicts.cell_idx(&cid));
                put_idx(out, dicts.id_idx(&view::id(chunk.get(CELL_ID..).unwrap_or(&[]))));
            }
            if (flags & INDEXED) != 0 {
                let idx_off = read_off + reads * READ;
                if let Some(by_cell) = b.get(idx_off..idx_off + reads * IDX) {
                    out.extend_from_slice(by_cell);
                }
            }
            None
        }
        CELLS => {
            put_idx(out, dicts.cell_idx(&read_cell_id_from(b)));
            put_idx(out, dicts.id_idx(&view::id(b.get(CELL_ID..).unwrap_or(&[]))));
            out.extend_from_slice(b.get(CELL_ID + ID..CELL_ID + ID + 8).unwrap_or(&[0; 8]));
            None
        }
        OWED_CELLS => {
            put_idx(out, dicts.cell_idx(&read_cell_id_from(b)));
            out.extend_from_slice(b.get(CELL_ID..CELL_ID + 8).unwrap_or(&[0; 8]));
            None
        }
        OWED_TASKS => {
            put_idx(out, dicts.name_idx(view::name(b)));
            out.extend_from_slice(b.get(NAME..NAME + 3).unwrap_or(&[0; 3]));
            put_idx(out, dicts.id_idx(&view::id(b.get(NAME + 3..).unwrap_or(&[]))));
            put_idx(out, dicts.name_idx(view::name(b.get(NAME + 3 + ID..).unwrap_or(&[]))));
            None
        }
        FILES => {
            put_idx(out, dicts.name_idx(view::name(b)));
            let members = b.get(NAME + 4..).unwrap_or(&[]);
            put_u32(out, members.len() / FILE_MEMBER);
            for m in members.chunks_exact(FILE_MEMBER) {
                put_idx(out, dicts.name_idx(view::name(m)));
                let standing = m.get(NAME).copied().unwrap_or(0);
                out.push(standing);
                if standing == 1 {
                    put_idx(out, dicts.id_idx(&view::id(m.get(NAME + 1..).unwrap_or(&[]))));
                    out.extend_from_slice(m.get(NAME + 1 + ID..NAME + 1 + ID + 3).unwrap_or(&[0; 3]));
                    put_idx(out, dicts.id_idx(&view::id(m.get(NAME + 1 + ID + 3..).unwrap_or(&[]))));
                } else {
                    out.extend_from_slice(&[0u8; IDX + 3 + IDX]);
                }
            }
            None
        }
        CAUSES => {
            put_idx(out, dicts.name_idx(view::name(b)));
            out.extend_from_slice(b.get(NAME..NAME + 8).unwrap_or(&[0; 8]));
            let tag = b.get(NAME + 8).copied().unwrap_or(0);
            out.push(tag);
            if tag == 2 {
                put_idx(out, dicts.cell_idx(&read_cell_id_from(b.get(NAME + 9..).unwrap_or(&[]))));
                put_idx(out, dicts.id_idx(&view::id(b.get(NAME + 9 + CELL_ID..).unwrap_or(&[]))));
                put_idx(out, dicts.id_idx(&view::id(b.get(NAME + 9 + CELL_ID + ID..).unwrap_or(&[]))));
            }
            None
        }
        RECORDS => {
            let name = view::name(b);
            let rec_id = view::id(b.get(NAME..).unwrap_or(&[]));
            put_idx(out, dicts.name_idx(name));
            put_idx(out, dicts.id_idx(&rec_id));
            Some((name, Key(rec_id)))
        }
        READERS => {
            put_idx(out, dicts.cell_idx(&read_cell_id_from(b)));
            let tail = b.get(CELL_ID + 4..).unwrap_or(&[]);
            put_u32(out, tail.len() / NAME);
            for chunk in tail.chunks_exact(NAME) {
                put_idx(out, dicts.name_idx(view::name(chunk)));
            }
            None
        }
        _ => None,
    }
}

fn collect_dict_entry<A: Allocator + Copy>(
    section: usize,
    old: PageDicts<'_>,
    b: &[u8],
    dicts: &mut DictBuilder<A>,
) -> usize {
    match section {
        ROWS => {
            dicts.add_name(old.name(view::word(b)));
            dicts.add_id(old.id(view::word(b.get(4..).unwrap_or(&[]))));
            dicts.add_name(old.name(view::word(b.get(11..).unwrap_or(&[]))));
            dicts.add_id(old.id(view::word(b.get(15..).unwrap_or(&[]))));
            dicts.add_id(old.id(view::word(b.get(19..).unwrap_or(&[]))));
            let flags = b.get(ROW_FIXED_V6 - 9).copied().unwrap_or(0);
            let defs = usize::try_from(view::word(b.get(ROW_FIXED_V6 - 8..).unwrap_or(&[]))).unwrap_or(0);
            let reads = usize::try_from(view::word(b.get(ROW_FIXED_V6 - 4..).unwrap_or(&[]))).unwrap_or(0);
            let def_bytes = b.get(ROW_FIXED_V6..ROW_FIXED_V6 + defs * DEFINE_V6).unwrap_or(&[]);
            for chunk in def_bytes.chunks_exact(DEFINE_V6) {
                dicts.add_cell(old.cell_id(view::word(chunk)));
                dicts.add_id(old.id(view::word(chunk.get(4..).unwrap_or(&[]))));
            }
            let read_off = ROW_FIXED_V6 + defs * DEFINE_V6;
            let read_bytes = b.get(read_off..read_off + reads * READ_V6).unwrap_or(&[]);
            for chunk in read_bytes.chunks_exact(READ_V6) {
                dicts.add_cell(old.cell_id(view::word(chunk)));
                dicts.add_id(old.id(view::word(chunk.get(4..).unwrap_or(&[]))));
            }
            row_len_v6(defs, reads, (flags & INDEXED) != 0)
        }
        CELLS => {
            dicts.add_cell(old.cell_id(view::word(b)));
            dicts.add_id(old.id(view::word(b.get(4..).unwrap_or(&[]))));
            CELL_ENTRY_V6
        }
        OWED_CELLS => {
            dicts.add_cell(old.cell_id(view::word(b)));
            OWED_CELL_V6
        }
        OWED_TASKS => {
            dicts.add_name(old.name(view::word(b)));
            dicts.add_id(old.id(view::word(b.get(7..).unwrap_or(&[]))));
            dicts.add_name(old.name(view::word(b.get(11..).unwrap_or(&[]))));
            OWED_TASK_V6
        }
        FILES => {
            dicts.add_name(old.name(view::word(b)));
            let members = b.get(8..).unwrap_or(&[]);
            let mut count = 0usize;
            for m in members.chunks_exact(FILE_MEMBER_V6) {
                count += 1;
                dicts.add_name(old.name(view::word(m)));
                if m.get(4).copied() == Some(1) {
                    dicts.add_id(old.id(view::word(m.get(5..).unwrap_or(&[]))));
                    dicts.add_id(old.id(view::word(m.get(12..).unwrap_or(&[]))));
                }
            }
            IDX + 4 + count * FILE_MEMBER_V6
        }
        CAUSES => {
            dicts.add_name(old.name(view::word(b)));
            let tag = b.get(12).copied().unwrap_or(0);
            if tag == 2 {
                dicts.add_cell(old.cell_id(view::word(b.get(13..).unwrap_or(&[]))));
                dicts.add_id(old.id(view::word(b.get(17..).unwrap_or(&[]))));
                dicts.add_id(old.id(view::word(b.get(21..).unwrap_or(&[]))));
                CAUSE_READ_MOVED_V6
            } else {
                CAUSE_SHORT_V6
            }
        }
        RECORDS => {
            dicts.add_name(old.name(view::word(b)));
            dicts.add_id(old.id(view::word(b.get(4..).unwrap_or(&[]))));
            RECORD_V6
        }
        READERS => {
            dicts.add_cell(old.cell_id(view::word(b)));
            let tail = b.get(8..).unwrap_or(&[]);
            let mut count = 0usize;
            for chunk in tail.chunks_exact(IDX) {
                count += 1;
                dicts.add_name(old.name(view::word(chunk)));
            }
            IDX + 4 + count * IDX
        }
        _ => 0,
    }
}

fn write_dict_entry<A: Allocator + Copy>(
    section: usize,
    old: PageDicts<'_>,
    b: &[u8],
    dicts: &DictBuilder<A>,
    out: &mut SVec<'_, u8>,
) -> Option<(Name, Key)> {
    match section {
        ROWS => {
            put_idx(out, dicts.name_idx(old.name(view::word(b))));
            put_idx(out, dicts.id_idx(&old.id(view::word(b.get(4..).unwrap_or(&[])))));
            out.extend_from_slice(b.get(8..11).unwrap_or(&[0; 3]));
            put_idx(out, dicts.name_idx(old.name(view::word(b.get(11..).unwrap_or(&[])))));
            put_idx(out, dicts.id_idx(&old.id(view::word(b.get(15..).unwrap_or(&[])))));
            put_idx(out, dicts.id_idx(&old.id(view::word(b.get(19..).unwrap_or(&[])))));
            out.extend_from_slice(b.get(23..ROW_FIXED_V6).unwrap_or(&[]));
            let flags = b.get(ROW_FIXED_V6 - 9).copied().unwrap_or(0);
            let defs = usize::try_from(view::word(b.get(ROW_FIXED_V6 - 8..).unwrap_or(&[]))).unwrap_or(0);
            let reads = usize::try_from(view::word(b.get(ROW_FIXED_V6 - 4..).unwrap_or(&[]))).unwrap_or(0);
            let def_bytes = b.get(ROW_FIXED_V6..ROW_FIXED_V6 + defs * DEFINE_V6).unwrap_or(&[]);
            for chunk in def_bytes.chunks_exact(DEFINE_V6) {
                put_idx(out, dicts.cell_idx(&old.cell_id(view::word(chunk))));
                put_idx(out, dicts.id_idx(&old.id(view::word(chunk.get(4..).unwrap_or(&[])))));
            }
            let read_off = ROW_FIXED_V6 + defs * DEFINE_V6;
            let read_bytes = b.get(read_off..read_off + reads * READ_V6).unwrap_or(&[]);
            for chunk in read_bytes.chunks_exact(READ_V6) {
                put_idx(out, dicts.cell_idx(&old.cell_id(view::word(chunk))));
                put_idx(out, dicts.id_idx(&old.id(view::word(chunk.get(4..).unwrap_or(&[])))));
            }
            if (flags & INDEXED) != 0 {
                let idx_off = read_off + reads * READ_V6;
                if let Some(by_cell) = b.get(idx_off..idx_off + reads * IDX) {
                    out.extend_from_slice(by_cell);
                }
            }
            None
        }
        CELLS => {
            put_idx(out, dicts.cell_idx(&old.cell_id(view::word(b))));
            put_idx(out, dicts.id_idx(&old.id(view::word(b.get(4..).unwrap_or(&[])))));
            out.extend_from_slice(b.get(8..16).unwrap_or(&[0; 8]));
            None
        }
        OWED_CELLS => {
            put_idx(out, dicts.cell_idx(&old.cell_id(view::word(b))));
            out.extend_from_slice(b.get(4..12).unwrap_or(&[0; 8]));
            None
        }
        OWED_TASKS => {
            put_idx(out, dicts.name_idx(old.name(view::word(b))));
            out.extend_from_slice(b.get(4..7).unwrap_or(&[0; 3]));
            put_idx(out, dicts.id_idx(&old.id(view::word(b.get(7..).unwrap_or(&[])))));
            put_idx(out, dicts.name_idx(old.name(view::word(b.get(11..).unwrap_or(&[])))));
            None
        }
        FILES => {
            put_idx(out, dicts.name_idx(old.name(view::word(b))));
            let members = b.get(8..).unwrap_or(&[]);
            put_u32(out, members.len() / FILE_MEMBER_V6);
            for m in members.chunks_exact(FILE_MEMBER_V6) {
                put_idx(out, dicts.name_idx(old.name(view::word(m))));
                let standing = m.get(4).copied().unwrap_or(0);
                out.push(standing);
                if standing == 1 {
                    put_idx(out, dicts.id_idx(&old.id(view::word(m.get(5..).unwrap_or(&[])))));
                    out.extend_from_slice(m.get(9..12).unwrap_or(&[0; 3]));
                    put_idx(out, dicts.id_idx(&old.id(view::word(m.get(12..).unwrap_or(&[])))));
                } else {
                    out.extend_from_slice(&[0u8; IDX + 3 + IDX]);
                }
            }
            None
        }
        CAUSES => {
            put_idx(out, dicts.name_idx(old.name(view::word(b))));
            out.extend_from_slice(b.get(4..12).unwrap_or(&[0; 8]));
            let tag = b.get(12).copied().unwrap_or(0);
            out.push(tag);
            if tag == 2 {
                put_idx(out, dicts.cell_idx(&old.cell_id(view::word(b.get(13..).unwrap_or(&[])))));
                put_idx(out, dicts.id_idx(&old.id(view::word(b.get(17..).unwrap_or(&[])))));
                put_idx(out, dicts.id_idx(&old.id(view::word(b.get(21..).unwrap_or(&[])))));
            }
            None
        }
        RECORDS => {
            let name = old.name(view::word(b));
            let rec_id = old.id(view::word(b.get(4..).unwrap_or(&[])));
            put_idx(out, dicts.name_idx(name));
            put_idx(out, dicts.id_idx(&rec_id));
            Some((name, Key(rec_id)))
        }
        READERS => {
            put_idx(out, dicts.cell_idx(&old.cell_id(view::word(b))));
            let tail = b.get(8..).unwrap_or(&[]);
            put_u32(out, tail.len() / IDX);
            for chunk in tail.chunks_exact(IDX) {
                put_idx(out, dicts.name_idx(old.name(view::word(chunk))));
            }
            None
        }
        _ => None,
    }
}

/// Write a staging flat `FILE_MEMBER` (101 bytes) from a decoded `BEP6` file member slice.
pub(super) fn write_flat_file_member_from_v6(dicts: PageDicts<'_>, m: &[u8], out: &mut SVec<'_, u8>) -> bool {
    put_name(out, dicts.name(view::word(m)));
    let standing = m.get(4).copied().unwrap_or(0);
    out.push(standing);
    if standing == 1 {
        put_id(out, &dicts.id(view::word(m.get(5..).unwrap_or(&[]))));
        out.extend_from_slice(m.get(9..12).unwrap_or(&[0; 3]));
        put_id(out, &dicts.id(view::word(m.get(12..).unwrap_or(&[]))));
        true
    } else {
        out.extend_from_slice(&[0u8; ID + 3 + ID]);
        false
    }
}

/// Approximate `BEP6` logical/page weight for sizing `bits_kept` (`encode` / `try_repage`),
/// keeping compacted `BEP6` pages fine-grained (~16-32 KiB) for fast incremental edits.
pub(super) fn estimate_row_v6(defines: usize, reads: usize) -> u64 {
    let base = ROW_FIXED_V6 + NAME + 3 * ID;
    u64::try_from(base.saturating_add(defines.saturating_mul(32)).saturating_add(reads.saturating_mul(32)))
        .unwrap_or(u64::MAX)
}

/// Write a complete `BEP6` page from eight sections of `EntrySource` items.
pub(super) fn encode_page_from_sources<'a, A: Allocator + Copy>(
    bits: u8,
    index: u32,
    sections: [&[EntrySource<'_>]; SECTIONS],
    out_alloc: SharedAlloc<'a>,
    scratch: A,
) -> (SVec<'a, u8>, SVec<'a, (Name, Key)>) {
    let counts = core::array::from_fn(|s| sections[s].len());
    let entries: usize = counts.iter().sum();
    if entries == 0 {
        return (SVec::new_in(out_alloc), SVec::new_in(out_alloc));
    }
    let mut dicts = DictBuilder::new_in(scratch);
    let mut lengths: Vec<usize, A> = Vec::with_capacity_in(entries, scratch);
    let mut body_len = 0usize;
    for (section, items) in sections.iter().enumerate() {
        for &src in items.iter() {
            let len = collect_entry_v6(section, src, &mut dicts);
            lengths.push(len);
            body_len = body_len.saturating_add(len);
        }
    }
    dicts.finalize();
    let body_at = body_start_v6(entries, dicts.names_len(), dicts.cells_len(), dicts.ids_len()).unwrap_or(0);
    let total_len = body_at.saturating_add(body_len);
    let mut page: SVec<'a, u8> = Vec::with_capacity_in(total_len, out_alloc);
    write_header_v6(&mut page, bits, index, &dicts, counts);
    let mut at = body_at;
    for &len in lengths.iter() {
        put_u32(&mut page, at);
        at = at.saturating_add(len);
    }
    put_u32(&mut page, total_len);
    dicts.write_dicts(&mut page);
    let mut records: SVec<'a, (Name, Key)> = Vec::with_capacity_in(counts.get(RECORDS).copied().unwrap_or(0), out_alloc);
    for (section, items) in sections.iter().enumerate() {
        for &src in items.iter() {
            if let Some(rec) = write_entry_v6(section, src, &dicts, &mut page) {
                records.push(rec);
            }
        }
    }
    debug_assert_eq!(page.len(), total_len, "a BEP6 page's start table ends at its length");
    (page, records)
}
