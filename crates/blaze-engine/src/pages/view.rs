//! A page read in place (`BEP6`). Its bytes come proved ([`Sealed`]); its per-page sorted
//! dictionaries (`names`, `cells`, `ids`) and start table sit at its head, so [`VerifiedPage::open`]
//! checks the head alone and allocates nothing. Lists and payloads stay directly in the mapped
//! record, and lookups binary-search by `u32` dictionary index.

use super::bep6::{
    self, CAUSE_READ_MOVED_V6, CAUSE_SHORT_V6, DEFINE_V6, DICT_CELL, FILE_MEMBER_V6, HEAD_V6, IDX,
    READ_V6, ROW_FIXED_V6,
};
use super::*;
use crate::RowFacts;

pub(super) fn id(bytes: &[u8]) -> ContentId {
    let mut out = [0u8; ID];
    if let Some(src) = bytes.get(..ID) {
        out.copy_from_slice(src);
    }
    ContentId(out)
}

pub(super) fn name(bytes: &[u8]) -> Name {
    let kind = bytes.first().copied().and_then(Kind::from_u8).unwrap_or(Kind::Units);
    Name::new(kind, id(bytes.get(1..).unwrap_or(&[])))
}

fn cell(bytes: &[u8]) -> Cell {
    let mut out = [0u8; CELL];
    if let Some(src) = bytes.get(..CELL) {
        out.copy_from_slice(src);
    }
    Cell(out)
}

fn cell_id(bytes: &[u8]) -> CellId {
    CellId { of: name(bytes), cell: cell(bytes.get(NAME..).unwrap_or(&[])) }
}

pub(super) fn word(bytes: &[u8]) -> u32 {
    let mut w = [0u8; 4];
    if let Some(src) = bytes.get(..4) {
        w.copy_from_slice(src);
    }
    u32::from_le_bytes(w)
}

fn serial(bytes: &[u8]) -> Gen {
    let mut w = [0u8; 8];
    if let Some(src) = bytes.get(..8) {
        w.copy_from_slice(src);
    }
    Gen(u64::from_le_bytes(w))
}

fn phase(bytes: &[u8]) -> Phase {
    let stage = bytes
        .first()
        .copied()
        .and_then(|b| Stage::ALL.get(usize::from(b)).copied())
        .unwrap_or(Stage::Units);
    let wave = u16::from_le_bytes([
        bytes.get(1).copied().unwrap_or(0),
        bytes.get(2).copied().unwrap_or(0),
    ]);
    Phase::new(stage, wave)
}

/// Borrowed slices of a `BEP6` page's three sorted unique dictionaries (48 bytes, `Copy`, zero
/// allocations).
#[derive(Clone, Copy)]
pub(super) struct PageDicts<'b> {
    names: &'b [u8],
    cells: &'b [u8],
    ids: &'b [u8],
}

impl<'b> PageDicts<'b> {
    #[inline]
    pub(super) fn names_count(&self) -> usize {
        self.names.len() / NAME
    }

    #[inline]
    pub(super) fn cells_count(&self) -> usize {
        self.cells.len() / DICT_CELL
    }

    #[inline]
    pub(super) fn ids_count(&self) -> usize {
        self.ids.len() / ID
    }

    #[inline]
    pub(super) fn name(&self, idx: u32) -> Name {
        let off = usize::try_from(idx).unwrap_or(usize::MAX).saturating_mul(NAME);
        name(self.names.get(off..off.saturating_add(NAME)).unwrap_or(&[]))
    }

    #[inline]
    pub(super) fn cell_of_idx(&self, idx: u32) -> (u32, Cell) {
        let off = usize::try_from(idx).unwrap_or(usize::MAX).saturating_mul(DICT_CELL);
        let entry = self.cells.get(off..off.saturating_add(DICT_CELL)).unwrap_or(&[]);
        (word(entry), cell(entry.get(IDX..).unwrap_or(&[])))
    }

    #[inline]
    pub(super) fn cell(&self, idx: u32) -> Cell {
        self.cell_of_idx(idx).1
    }

    #[inline]
    pub(super) fn cell_id(&self, idx: u32) -> CellId {
        let (of_idx, c) = self.cell_of_idx(idx);
        CellId { of: self.name(of_idx), cell: c }
    }

    #[inline]
    pub(super) fn id(&self, idx: u32) -> ContentId {
        let off = usize::try_from(idx).unwrap_or(usize::MAX).saturating_mul(ID);
        id(self.ids.get(off..off.saturating_add(ID)).unwrap_or(&[]))
    }

    pub(super) fn find_name(&self, wanted: Name) -> Option<u32> {
        let mut needle = [0u8; NAME];
        needle[0] = bep6::kind_u8(wanted.kind);
        needle[1..].copy_from_slice(&wanted.subject.0);
        let (at, _) = crate::search(self.names_count(), |i| {
            let off = i.saturating_mul(NAME);
            self.names.get(off..off.saturating_add(NAME)).unwrap_or(&[]).cmp(&needle[..])
        });
        at.and_then(|i| u32::try_from(i).ok())
    }

    pub(super) fn find_cell_id(&self, wanted: &CellId) -> Option<u32> {
        let of_idx = self.find_name(wanted.of)?;
        let (at, _) = crate::search(self.cells_count(), |i| {
            let off = i.saturating_mul(DICT_CELL);
            let entry = self.cells.get(off..off.saturating_add(DICT_CELL)).unwrap_or(&[]);
            word(entry)
                .cmp(&of_idx)
                .then_with(|| entry.get(IDX..DICT_CELL).unwrap_or(&[]).cmp(&wanted.cell.0[..]))
        });
        at.and_then(|i| u32::try_from(i).ok())
    }
}

/// The list of names in a stored file plan or reverse reader list.
#[derive(Clone)]
pub struct StoredNames<'b> {
    chunks: core::slice::ChunksExact<'b, u8>,
    dicts: PageDicts<'b>,
}

impl Iterator for StoredNames<'_> {
    type Item = Name;
    fn next(&mut self) -> Option<Name> {
        self.chunks.next().map(|b| self.dicts.name(word(b)))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chunks.size_hint()
    }
}
impl ExactSizeIterator for StoredNames<'_> {}

#[derive(Clone)]
pub struct StoredReads<'b> {
    chunks: core::slice::ChunksExact<'b, u8>,
    dicts: Option<PageDicts<'b>>,
}

impl Iterator for StoredReads<'_> {
    type Item = (CellId, ContentId);
    fn next(&mut self) -> Option<Self::Item> {
        let b = self.chunks.next()?;
        Some(match self.dicts {
            Some(dicts) => (dicts.cell_id(word(b)), dicts.id(word(b.get(IDX..).unwrap_or(&[])))),
            None => (cell_id(b), id(b.get(CELL_ID..).unwrap_or(&[]))),
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chunks.size_hint()
    }
}
impl ExactSizeIterator for StoredReads<'_> {}

#[derive(Clone)]
pub struct StoredDefines<'b> {
    chunks: core::slice::ChunksExact<'b, u8>,
    dicts: Option<PageDicts<'b>>,
}

impl Iterator for StoredDefines<'_> {
    type Item = (Cell, ContentId);
    fn next(&mut self) -> Option<Self::Item> {
        let b = self.chunks.next()?;
        Some(match self.dicts {
            Some(dicts) => (dicts.cell(word(b)), dicts.id(word(b.get(IDX..).unwrap_or(&[])))),
            None => (cell(b), id(b.get(CELL..).unwrap_or(&[]))),
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chunks.size_hint()
    }
}
impl ExactSizeIterator for StoredDefines<'_> {}

/// A row's checked fixed fields and its borrowed variable-width columns.
#[derive(Clone, Copy)]
pub struct StoredRow<'b> {
    pub facts: RowFacts,
    defines: &'b [u8],
    reads: &'b [u8],
    by_cell: &'b [u8],
    dicts: Option<PageDicts<'b>>,
}

pub(super) fn row_facts(b: &[u8]) -> RowFacts {
    const PHASE: usize = NAME + ID;
    const FILE: usize = PHASE + 3;
    const SOURCE: usize = FILE + NAME;
    const OUT: usize = SOURCE + ID;
    const SERIAL: usize = OUT + ID;
    const FINDINGS: usize = SERIAL + 8;
    let flags = b.get(FINDINGS + 4).copied().unwrap_or(0);
    RowFacts {
        key: Key(id(b.get(NAME..).unwrap_or(&[]))),
        phase: phase(b.get(PHASE..).unwrap_or(&[])),
        file: name(b.get(FILE..).unwrap_or(&[])),
        source: id(b.get(SOURCE..).unwrap_or(&[])),
        out: id(b.get(OUT..).unwrap_or(&[])),
        serial: serial(b.get(SERIAL..).unwrap_or(&[])),
        findings: word(b.get(FINDINGS..).unwrap_or(&[])),
        inline: flags & INLINE != 0,
        failed: flags & FAILED != 0,
    }
}

fn row_facts_v6(b: &[u8], dicts: PageDicts<'_>) -> RowFacts {
    let flags = b.get(ROW_FIXED_V6 - 9).copied().unwrap_or(0);
    RowFacts {
        key: Key(dicts.id(word(b.get(4..).unwrap_or(&[])))),
        phase: phase(b.get(8..).unwrap_or(&[])),
        file: dicts.name(word(b.get(11..).unwrap_or(&[]))),
        source: dicts.id(word(b.get(15..).unwrap_or(&[]))),
        out: dicts.id(word(b.get(19..).unwrap_or(&[]))),
        serial: serial(b.get(23..).unwrap_or(&[])),
        findings: word(b.get(31..).unwrap_or(&[])),
        inline: flags & INLINE != 0,
        failed: flags & FAILED != 0,
    }
}

impl<'b> StoredRow<'b> {
    /// Flat `BEH1` header-shard row reader (`headers.rs`).
    pub(super) fn from_checked(b: &'b [u8]) -> Self {
        let definitions = usize::try_from(word(b.get(ROW_FIXED - 8..).unwrap_or(&[]))).unwrap_or(0);
        let observations = usize::try_from(word(b.get(ROW_FIXED - 4..).unwrap_or(&[]))).unwrap_or(0);
        let tail = b.get(ROW_FIXED..).unwrap_or(&[]);
        let def_end = definitions.saturating_mul(DEFINE).min(tail.len());
        let (defines, lists) = tail.split_at(def_end);
        let read_end = observations.saturating_mul(READ).min(lists.len());
        let (reads, by_cell) = lists.split_at(read_end);
        Self { facts: row_facts(b), defines, reads, by_cell, dicts: None }
    }

    /// Dictionary-indexed `BEP6` page row reader.
    pub(super) fn from_checked_v6(b: &'b [u8], dicts: PageDicts<'b>) -> Self {
        let definitions = usize::try_from(word(b.get(ROW_FIXED_V6 - 8..).unwrap_or(&[]))).unwrap_or(0);
        let observations = usize::try_from(word(b.get(ROW_FIXED_V6 - 4..).unwrap_or(&[]))).unwrap_or(0);
        let tail = b.get(ROW_FIXED_V6..).unwrap_or(&[]);
        let def_end = definitions.saturating_mul(DEFINE_V6).min(tail.len());
        let (defines, lists) = tail.split_at(def_end);
        let read_end = observations.saturating_mul(READ_V6).min(lists.len());
        let (reads, by_cell) = lists.split_at(read_end);
        Self { facts: row_facts_v6(b, dicts), defines, reads, by_cell, dicts: Some(dicts) }
    }

    pub fn defines(&self) -> StoredDefines<'b> {
        let stride = if self.dicts.is_some() { DEFINE_V6 } else { DEFINE };
        StoredDefines { chunks: self.defines.chunks_exact(stride), dicts: self.dicts }
    }

    pub fn reads(&self) -> StoredReads<'b> {
        let stride = if self.dicts.is_some() { READ_V6 } else { READ };
        StoredReads { chunks: self.reads.chunks_exact(stride), dicts: self.dicts }
    }

    fn position(&self, at: usize) -> usize {
        if self.by_cell.is_empty() {
            at
        } else {
            usize::try_from(word(self.by_cell.get(at.saturating_mul(4)..).unwrap_or(&[]))).unwrap_or(0)
        }
    }

    pub fn read_of(&self, wanted: &CellId) -> (Option<ContentId>, u32) {
        if let Some(dicts) = self.dicts {
            let Some(target) = dicts.find_cell_id(wanted) else {
                return (None, 0);
            };
            let count = self.reads.len() / READ_V6;
            let (found, compared) = crate::search(count, |at| {
                let off = self.position(at).saturating_mul(READ_V6);
                word(self.reads.get(off..).unwrap_or(&[])).cmp(&target)
            });
            let digest = found.map(|at| {
                let off = self.position(at).saturating_mul(READ_V6).saturating_add(IDX);
                dicts.id(word(self.reads.get(off..).unwrap_or(&[])))
            });
            return (digest, compared);
        }
        let (found, compared) = crate::search(self.reads.len() / READ, |at| {
            let off = self.position(at).saturating_mul(READ);
            cell_id(self.reads.get(off..).unwrap_or(&[])).cmp(wanted)
        });
        (
            found.map(|at| {
                let off = self.position(at).saturating_mul(READ).saturating_add(CELL_ID);
                id(self.reads.get(off..).unwrap_or(&[]))
            }),
            compared,
        )
    }

    pub fn to_owned<'a>(&self, alloc: SharedAlloc<'a>) -> Row<'a> {
        let f = self.facts;
        let reads = self.reads();
        let mut by_cell = SVec::with_capacity_in(reads.len(), alloc);
        for at in 0..reads.len() {
            by_cell.push(u32::try_from(self.position(at)).unwrap_or(u32::MAX));
        }
        let mut reads_vec = SVec::with_capacity_in(reads.len(), alloc);
        reads_vec.extend(reads);
        let defs = self.defines();
        let mut defines_vec = SVec::with_capacity_in(defs.len(), alloc);
        defines_vec.extend(defs);
        Row {
            key: f.key,
            phase: f.phase,
            file: f.file,
            source: f.source,
            out: f.out,
            serial: f.serial,
            findings: f.findings,
            failed: f.failed,
            inline: f.inline,
            by_cell,
            reads: reads_vec,
            defines: defines_vec,
        }
    }
}

fn list<'b>(r: &mut Rd<'b>, n: usize, width: usize) -> Result<&'b [u8], PagesError> {
    let len = n.checked_mul(width).ok_or(PagesError::Short {
        page: r.page,
        at: u32::try_from(r.at).unwrap_or(u32::MAX),
    })?;
    r.take(len)
}

/// Check flat `BEH1` row fields on admission (`headers.rs`).
pub(super) fn read_row<'b>(r: &mut Rd<'b>) -> Result<StoredRow<'b>, PagesError> {
    let key = Key(r.id()?);
    let phase = r.phase()?;
    let file = r.name()?;
    let source = r.id()?;
    let out = r.id()?;
    let serial = Gen(r.u64()?);
    let findings = r.u32()?;
    let flags = r.u8()?;
    if flags & !(INLINE | FAILED | INDEXED) != 0 {
        return Err(r.tag(flags));
    }
    let definitions = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
    let observations = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
    if observations > crate::READS_CAP {
        return Err(PagesError::Reads {
            page: r.page,
            at: u32::try_from(r.at).unwrap_or(u32::MAX),
            count: u32::try_from(observations).unwrap_or(u32::MAX),
        });
    }
    let defines = list(r, definitions, DEFINE)?;
    let reads = list(r, observations, READ)?;
    let by_cell = list(r, if flags & INDEXED == 0 { 0 } else { observations }, 4)?;
    Ok(StoredRow {
        facts: RowFacts {
            key,
            phase,
            file,
            source,
            out,
            serial,
            findings,
            failed: flags & FAILED != 0,
            inline: flags & INLINE != 0,
        },
        defines,
        reads,
        by_cell,
        dicts: None,
    })
}

/// Check flat `BEH1` row ordering (`headers.rs`).
pub(super) fn check_row(row: &StoredRow<'_>, page: u32, start: usize) -> Result<(), PagesError> {
    let start_u32 = u32::try_from(start).unwrap_or(u32::MAX);
    let unsorted = || PagesError::Unsorted { page, section: SECTION_NAMES[ROWS], at: start_u32 };
    let mut previous = None;
    for field in row.defines.chunks_exact(DEFINE) {
        let cell_slice = field.get(..CELL).unwrap_or(&[]);
        if previous.is_some_and(|last: &[u8]| last > cell_slice) {
            return Err(unsorted());
        }
        previous = Some(cell_slice);
    }
    previous = None;
    let count = row.reads.len() / READ;
    for at in 0..count {
        let position = row.position(at);
        if position >= count {
            return Err(unsorted());
        }
        let field = row.reads.get(position * READ..position * READ + CELL_ID).unwrap_or(&[]);
        let kind_byte = field.first().copied().unwrap_or(0);
        if Kind::from_u8(kind_byte).is_none() {
            return Err(PagesError::Tag { page, at: start_u32, byte: kind_byte });
        }
        if previous.is_some_and(|last| last >= field) {
            return Err(unsorted());
        }
        previous = Some(field);
    }
    Ok(())
}

fn check_idx(idx: u32, limit: usize, page: u32, at: usize) -> Result<u32, PagesError> {
    let idx_usize = usize::try_from(idx).unwrap_or(usize::MAX);
    if idx_usize >= limit {
        return Err(PagesError::Count { page, count: idx });
    }
    let _ = at;
    Ok(idx)
}

fn read_row_v6<'b>(
    r: &mut Rd<'b>,
    row_name_idx: u32,
    dicts: PageDicts<'b>,
    start: usize,
) -> Result<(), PagesError> {
    let page = r.page;
    let start_u32 = u32::try_from(start).unwrap_or(u32::MAX);
    let n_names = dicts.names_count();
    let n_cells = dicts.cells_count();
    let n_ids = dicts.ids_count();
    let _key_idx = check_idx(r.u32()?, n_ids, page, r.at)?;
    let _phase = r.phase()?;
    let _file_idx = check_idx(r.u32()?, n_names, page, r.at)?;
    let _source_idx = check_idx(r.u32()?, n_ids, page, r.at)?;
    let _out_idx = check_idx(r.u32()?, n_ids, page, r.at)?;
    let _serial = r.u64()?;
    let _findings = r.u32()?;
    let flags = r.u8()?;
    if flags & !(INLINE | FAILED | INDEXED) != 0 {
        return Err(r.tag(flags));
    }
    let definitions = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
    let observations = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
    if observations > crate::READS_CAP {
        return Err(PagesError::Reads {
            page,
            at: u32::try_from(r.at).unwrap_or(u32::MAX),
            count: u32::try_from(observations).unwrap_or(u32::MAX),
        });
    }
    let defines = list(r, definitions, DEFINE_V6)?;
    let reads = list(r, observations, READ_V6)?;
    let by_cell = list(r, if flags & INDEXED == 0 { 0 } else { observations }, IDX)?;

    let unsorted = || PagesError::Unsorted { page, section: SECTION_NAMES[ROWS], at: start_u32 };
    let mut prev_def: Option<u32> = None;
    for chunk in defines.chunks_exact(DEFINE_V6) {
        let cell_idx = check_idx(word(chunk), n_cells, page, start)?;
        let _dig_idx = check_idx(word(chunk.get(IDX..).unwrap_or(&[])), n_ids, page, start)?;
        let (definer_idx, _) = dicts.cell_of_idx(cell_idx);
        if definer_idx != row_name_idx {
            return Err(PagesError::Magic { page });
        }
        if prev_def.is_some_and(|last| last > cell_idx) {
            return Err(unsorted());
        }
        prev_def = Some(cell_idx);
    }

    let mut prev_read: Option<u32> = None;
    for at in 0..observations {
        let pos = if by_cell.is_empty() {
            at
        } else {
            usize::try_from(word(by_cell.get(at.saturating_mul(IDX)..).unwrap_or(&[]))).unwrap_or(usize::MAX)
        };
        if pos >= observations {
            return Err(unsorted());
        }
        let chunk = reads.get(pos * READ_V6..(pos + 1) * READ_V6).unwrap_or(&[]);
        let cell_idx = check_idx(word(chunk), n_cells, page, start)?;
        let _dig_idx = check_idx(word(chunk.get(IDX..).unwrap_or(&[])), n_ids, page, start)?;
        if prev_read.is_some_and(|last| last >= cell_idx) {
            return Err(unsorted());
        }
        prev_read = Some(cell_idx);
    }
    Ok(())
}

fn read_cause_v6(r: &mut Rd<'_>, dicts: PageDicts<'_>) -> Result<(Gen, Cause), PagesError> {
    let serial = Gen(r.u64()?);
    let tag = r.u8()?;
    let bad_tag = r.tag(tag);
    let cause = match tag {
        0 => Cause::New,
        1 => Cause::KeyMoved,
        2 => {
            let cell_idx = check_idx(r.u32()?, dicts.cells_count(), r.page, r.at)?;
            let was_idx = check_idx(r.u32()?, dicts.ids_count(), r.page, r.at)?;
            let now_idx = check_idx(r.u32()?, dicts.ids_count(), r.page, r.at)?;
            Cause::ReadMoved {
                read: dicts.cell_id(cell_idx),
                was: dicts.id(was_idx),
                now: dicts.id(now_idx),
            }
        }
        3 => Cause::Owed,
        4 => Cause::Inline,
        _ => return Err(bad_tag),
    };
    Ok((serial, cause))
}

pub(super) fn key_width(section: usize) -> usize {
    if matches!(section, CELLS | OWED_CELLS | READERS) { CELL_ID } else { NAME }
}

/// A `BEP6` page whose bytes are its key's ([`Sealed`]) and whose head delimits its dictionaries
/// and entries: the pages' one reader. Borrows the record directly, allocates nothing.
pub struct VerifiedPage<'b> {
    bytes: &'b [u8],
    /// The first entry of each section, and past the last: section `s` is entries
    /// `first[s]..first[s + 1]` of the start table.
    first: [usize; SECTIONS + 1],
    dicts: PageDicts<'b>,
}

impl<'b> VerifiedPage<'b> {
    /// The page's length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn dicts(&self) -> PageDicts<'b> {
        self.dicts
    }

    /// Where entry `j` starts (the start table's word `j`; word `n` is the page's length).
    fn start(&self, j: usize) -> usize {
        let off = HEAD_V6.saturating_add(j.saturating_mul(IDX));
        usize::try_from(word(self.bytes.get(off..off.saturating_add(IDX)).unwrap_or(&[]))).unwrap_or(0)
    }

    fn entry(&self, j: usize) -> &'b [u8] {
        let from = self.start(j);
        let to = self.start(j + 1);
        self.bytes.get(from..to).unwrap_or(&[])
    }

    pub(super) fn entry_key(&self, section: usize, entry: &[u8]) -> CellId {
        let idx = word(entry);
        if key_width(section) == CELL_ID {
            self.dicts.cell_id(idx)
        } else {
            CellId { of: self.dicts.name(idx), cell: Cell([0; CELL]) }
        }
    }

    pub(super) fn restore<'r>(&self, kept: &mut Kept<'r>, reader_pairs: &mut SVec<'r, (CellId, Name)>, alloc: SharedAlloc<'r>) {
        for n in self.row_names() {
            if let Some(row) = self.row(n) {
                let owned = row.to_owned(alloc);
                if owned.findings > 0 || owned.failed {
                    kept.answer.with_findings.insert(n);
                }
                kept.answer.rows.insert(n, owned);
            }
        }
        for b in self.entries(CELLS) {
            let cid = self.dicts.cell_id(word(b));
            let digest = self.dicts.id(word(b.get(4..).unwrap_or(&[])));
            let moved = serial(b.get(8..).unwrap_or(&[]));
            kept.answer.cells.insert(cid, (digest, moved));
        }
        for b in self.entries(OWED_CELLS) {
            let cid = self.dicts.cell_id(word(b));
            let moved = serial(b.get(4..).unwrap_or(&[]));
            kept.answer.owed_cells.insert(cid, moved);
        }
        for b in self.entries(OWED_TASKS) {
            let n = self.dicts.name(word(b));
            let plan = Planned {
                name: n,
                phase: phase(b.get(4..).unwrap_or(&[])),
                source: self.dicts.id(word(b.get(7..).unwrap_or(&[]))),
                file: self.dicts.name(word(b.get(11..).unwrap_or(&[]))),
            };
            kept.answer.owed_tasks.insert(n, plan);
        }
        for b in self.entries(FILES) {
            let f = self.dicts.name(word(b));
            let members = b.get(8..).unwrap_or(&[]);
            let mut names = SVec::new_in(alloc);
            names.extend(StoredNames { chunks: members.chunks_exact(FILE_MEMBER_V6), dicts: self.dicts });
            kept.answer.by_file.insert(f, names);
        }
        for b in self.entries(CAUSES) {
            let n = self.dicts.name(word(b));
            if let Ok(c) = read_cause_v6(&mut Rd { bytes: b, at: IDX, page: 0 }, self.dicts) {
                kept.causes.insert(n, c);
            }
        }
        kept.records.extend(self.records());
        for b in self.entries(READERS) {
            let id = self.dicts.cell_id(word(b));
            let tail = b.get(8..).unwrap_or(&[]);
            reader_pairs.extend(StoredNames { chunks: tail.chunks_exact(IDX), dicts: self.dicts }.map(|reader| (id, reader)));
        }
    }

    /// Page `page` of a root paged by `bits`, its bytes proved by `sealed`: the head alone is
    /// checked (magic `BEP6`, bits, index, dictionary sizes, start table bounds). Rejects `BEP5`
    /// and any other magic with [`PagesError::Magic`] (Rule 3 & Rule 26).
    pub fn open(sealed: Sealed<'b>, page: u32, bits: u8) -> Result<Self, PagesError> {
        let bytes = sealed.bytes();
        let mut r = Rd { bytes, at: 0, page };
        if r.take(4)? != PAGE_MAGIC || r.u8()? != bits || r.u32()? != page {
            return Err(PagesError::Magic { page });
        }
        if bits == 0 || bits > blaze_drive::answers::MOST_BITS || page >= (1u32 << bits) {
            return Err(PagesError::Count { page, count: u32::from(bits) });
        }
        let names_count = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
        let cells_count = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
        let ids_count = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
        let mut first = [0usize; SECTIONS + 1];
        for section in 0..SECTIONS {
            let count = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
            first[section + 1] = first[section].checked_add(count).ok_or(PagesError::Short {
                page,
                at: u32::try_from(r.at).unwrap_or(u32::MAX),
            })?;
        }
        let entries = first[SECTIONS];
        let body_at = bep6::body_start_v6(entries, names_count, cells_count, ids_count)
            .ok_or(PagesError::Short { page, at: u32::try_from(r.at).unwrap_or(u32::MAX) })?;
        if body_at > bytes.len() {
            return Err(PagesError::Short {
                page,
                at: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            });
        }
        let names_at = HEAD_V6 + (entries + 1) * IDX;
        let cells_at = names_at + names_count * NAME;
        let ids_at = cells_at + cells_count * DICT_CELL;
        let dicts = PageDicts {
            names: bytes.get(names_at..cells_at).unwrap_or(&[]),
            cells: bytes.get(cells_at..ids_at).unwrap_or(&[]),
            ids: bytes.get(ids_at..body_at).unwrap_or(&[]),
        };
        let view = Self { bytes, first, dicts };
        if view.start(0) != body_at {
            return Err(PagesError::Starts { page, entry: 0 });
        }
        if view.start(entries) != bytes.len() {
            return Err(PagesError::Starts {
                page,
                entry: u32::try_from(entries).unwrap_or(u32::MAX),
            });
        }
        Ok(view)
    }

    /// [`VerifiedPage::open`], then every dictionary entry and every section entry walked and
    /// verified (ordering, kind bytes, dictionary bounds, page placement, start table alignment).
    pub fn open_whole(sealed: Sealed<'b>, page: u32, bits: u8) -> Result<Self, PagesError> {
        let view = Self::open(sealed, page, bits)?;
        let bytes = view.bytes;
        let dicts = view.dicts;
        let n_names = dicts.names_count();
        let n_cells = dicts.cells_count();
        let n_ids = dicts.ids_count();

        // 1. Validate sorted unique `names_dict`.
        {
            let mut prev: Option<Name> = None;
            let mut r = Rd { bytes: dicts.names, at: 0, page };
            for _ in 0..n_names {
                let at = r.at;
                let n = r.name()?;
                if prev.is_some_and(|last| last >= n) {
                    return Err(PagesError::Unsorted {
                        page,
                        section: "names dictionary",
                        at: u32::try_from(at).unwrap_or(u32::MAX),
                    });
                }
                prev = Some(n);
            }
        }
        // 2. Validate sorted unique `cells_dict` (`name_idx < n_names`, strict ascending `(name_idx, cell)`).
        {
            let mut prev: Option<(u32, Cell)> = None;
            let mut r = Rd { bytes: dicts.cells, at: 0, page };
            for _ in 0..n_cells {
                let at = r.at;
                let of_idx = check_idx(r.u32()?, n_names, page, at)?;
                let c = r.cell()?;
                let cur = (of_idx, c);
                if prev.is_some_and(|last| last >= cur) {
                    return Err(PagesError::Unsorted {
                        page,
                        section: "cells dictionary",
                        at: u32::try_from(at).unwrap_or(u32::MAX),
                    });
                }
                prev = Some(cur);
            }
        }
        // 3. Validate sorted unique `ids_dict`.
        {
            let mut prev: Option<ContentId> = None;
            let mut r = Rd { bytes: dicts.ids, at: 0, page };
            for _ in 0..n_ids {
                let at = r.at;
                let cur = r.id()?;
                if prev.is_some_and(|last| last >= cur) {
                    return Err(PagesError::Unsorted {
                        page,
                        section: "ids dictionary",
                        at: u32::try_from(at).unwrap_or(u32::MAX),
                    });
                }
                prev = Some(cur);
            }
        }

        // 4. Validate all eight sections in order.
        let mut r = Rd { bytes, at: view.start(0), page };
        for section in 0..SECTIONS {
            let is_cell_key = key_width(section) == CELL_ID;
            let unsorted = |at: usize| PagesError::Unsorted {
                page,
                section: SECTION_NAMES[section],
                at: u32::try_from(at).unwrap_or(u32::MAX),
            };
            let mut prev_key: Option<u32> = None;
            for j in view.first[section]..view.first[section + 1] {
                let start = r.at;
                if view.start(j) != start {
                    return Err(PagesError::Starts {
                        page,
                        entry: u32::try_from(j).unwrap_or(u32::MAX),
                    });
                }
                let key_idx = r.u32()?;
                let (expected_page, reported) = if is_cell_key {
                    check_idx(key_idx, n_cells, page, start)?;
                    let cid = dicts.cell_id(key_idx);
                    let expected = if section == READERS {
                        readers_page_of(&cid, bits)
                    } else {
                        content_page_of(cid.of, bits)
                    };
                    (expected, cid.of)
                } else {
                    check_idx(key_idx, n_names, page, start)?;
                    let n = dicts.name(key_idx);
                    (content_page_of(n, bits), n)
                };
                if expected_page != page {
                    return Err(PagesError::Misplaced {
                        page,
                        section: SECTION_NAMES[section],
                        name: reported,
                    });
                }
                if prev_key.is_some_and(|last| last >= key_idx) {
                    return Err(unsorted(start));
                }
                prev_key = Some(key_idx);
                match section {
                    ROWS => read_row_v6(&mut r, key_idx, dicts, start)?,
                    CELLS => {
                        check_idx(r.u32()?, n_ids, page, r.at)?;
                        let _gen = r.u64()?;
                    }
                    OWED_CELLS => {
                        let _gen = r.u64()?;
                    }
                    OWED_TASKS => {
                        let _phase = r.phase()?;
                        check_idx(r.u32()?, n_ids, page, r.at)?;
                        check_idx(r.u32()?, n_names, page, r.at)?;
                    }
                    FILES => {
                        let count = r.count(FILE_MEMBER_V6)?;
                        let mut prev_member: Option<u32> = None;
                        for _ in 0..count {
                            let at = r.at;
                            let m_idx = check_idx(r.u32()?, n_names, page, at)?;
                            if prev_member.is_some_and(|last| last >= m_idx) {
                                return Err(unsorted(at));
                            }
                            prev_member = Some(m_idx);
                            match r.u8()? {
                                0 => {
                                    if r.take(IDX + 3 + IDX)?.iter().any(|&b| b != 0) {
                                        return Err(PagesError::Tag {
                                            page,
                                            at: u32::try_from(at).unwrap_or(u32::MAX),
                                            byte: 0,
                                        });
                                    }
                                }
                                1 => {
                                    check_idx(r.u32()?, n_ids, page, r.at)?;
                                    let _phase = r.phase()?;
                                    check_idx(r.u32()?, n_ids, page, r.at)?;
                                }
                                tag => return Err(r.tag(tag)),
                            }
                        }
                    }
                    CAUSES => {
                        let _ = read_cause_v6(&mut r, dicts)?;
                        let entry_len = r.at.saturating_sub(start);
                        if entry_len != CAUSE_SHORT_V6 && entry_len != CAUSE_READ_MOVED_V6 {
                            return Err(PagesError::Starts {
                                page,
                                entry: u32::try_from(j).unwrap_or(u32::MAX),
                            });
                        }
                    }
                    RECORDS => {
                        check_idx(r.u32()?, n_ids, page, r.at)?;
                    }
                    READERS => {
                        let count = r.count(IDX)?;
                        let mut prev_reader: Option<u32> = None;
                        for _ in 0..count {
                            let at = r.at;
                            let reader_idx = check_idx(r.u32()?, n_names, page, at)?;
                            if prev_reader.is_some_and(|last| last >= reader_idx) {
                                return Err(unsorted(at));
                            }
                            prev_reader = Some(reader_idx);
                        }
                    }
                    _ => unreachable!("a page section"),
                }
            }
        }
        if r.at != bytes.len() {
            return Err(PagesError::Trailing {
                page,
                at: u32::try_from(r.at).unwrap_or(u32::MAX),
            });
        }
        Ok(view)
    }

    pub(super) fn entries(&self, section: usize) -> impl Iterator<Item = &'b [u8]> + '_ {
        let from = self.first.get(section).copied().unwrap_or(0);
        let to = self.first.get(section + 1).copied().unwrap_or(from);
        (from..to).map(|j| self.entry(j))
    }

    fn find_by_idx(&self, section: usize, target: u32) -> Option<&'b [u8]> {
        let from = self.first.get(section).copied().unwrap_or(0);
        let to = self.first.get(section + 1).copied().unwrap_or(from);
        let (mut lo, mut hi) = (from, to);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = self.start(mid);
            let k = word(self.bytes.get(off..off.saturating_add(IDX)).unwrap_or(&[]));
            match k.cmp(&target) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(self.entry(mid)),
            }
        }
        None
    }

    fn find_name(&self, section: usize, wanted: Name) -> Option<&'b [u8]> {
        let target = self.dicts.find_name(wanted)?;
        self.find_by_idx(section, target)
    }

    fn find_cell(&self, section: usize, wanted: &CellId) -> Option<&'b [u8]> {
        let target = self.dicts.find_cell_id(wanted)?;
        self.find_by_idx(section, target)
    }

    pub fn row_names(&self) -> impl Iterator<Item = Name> + '_ {
        self.entries(ROWS).map(|b| self.dicts.name(word(b)))
    }

    pub fn row(&self, wanted: Name) -> Option<StoredRow<'b>> {
        self.find_name(ROWS, wanted).map(|b| StoredRow::from_checked_v6(b, self.dicts))
    }

    pub fn cell(&self, wanted: &CellId) -> Option<(ContentId, Gen)> {
        let b = self.find_cell(CELLS, wanted)?;
        Some((self.dicts.id(word(b.get(4..).unwrap_or(&[]))), serial(b.get(8..).unwrap_or(&[]))))
    }

    pub fn owed_cell(&self, wanted: &CellId) -> Option<Gen> {
        self.find_cell(OWED_CELLS, wanted).map(|b| serial(b.get(4..).unwrap_or(&[])))
    }

    pub fn owed_task(&self, wanted: Name) -> Option<Planned> {
        let b = self.find_name(OWED_TASKS, wanted)?;
        Some(Planned {
            name: wanted,
            phase: phase(b.get(4..).unwrap_or(&[])),
            source: self.dicts.id(word(b.get(7..).unwrap_or(&[]))),
            file: self.dicts.name(word(b.get(11..).unwrap_or(&[]))),
        })
    }

    pub fn cause(&self, wanted: Name) -> Option<(Gen, Cause)> {
        let b = self.find_name(CAUSES, wanted)?;
        read_cause_v6(&mut Rd { bytes: b, at: IDX, page: 0 }, self.dicts).ok()
    }

    pub fn record(&self, wanted: Name) -> Option<Key> {
        self.find_name(RECORDS, wanted)
            .map(|b| Key(self.dicts.id(word(b.get(4..).unwrap_or(&[])))))
    }

    pub fn records(&self) -> impl Iterator<Item = (Name, Key)> + '_ {
        self.entries(RECORDS).map(|b| {
            (self.dicts.name(word(b)), Key(self.dicts.id(word(b.get(4..).unwrap_or(&[])))))
        })
    }

    pub fn readers(&self, wanted: &CellId) -> StoredNames<'b> {
        let bytes = self.find_cell(READERS, wanted).and_then(|b| b.get(8..)).unwrap_or(&[]);
        StoredNames { chunks: bytes.chunks_exact(IDX), dicts: self.dicts }
    }

    pub(super) fn readers_present(&self, wanted: &CellId) -> bool {
        self.find_cell(READERS, wanted).is_some()
    }

    pub fn file_tasks(&self, wanted: Name) -> StoredNames<'b> {
        StoredNames { chunks: self.file_members(wanted).chunks_exact(FILE_MEMBER_V6), dicts: self.dicts }
    }

    /// All members of `wanted`'s stored file plan with each standing member's certified key,
    /// sorted ascending by `Name`, in a single lookup of the `FILES` section.
    pub fn file_member_keys(&self, wanted: Name) -> impl ExactSizeIterator<Item = (Name, Option<Key>)> + '_ {
        let dicts = self.dicts;
        self.file_members(wanted).chunks_exact(FILE_MEMBER_V6).map(move |b| {
            let standing = if b.get(4).copied() == Some(1) {
                Some(Key(dicts.id(word(b.get(5..).unwrap_or(&[])))))
            } else {
                None
            };
            (dicts.name(word(b)), standing)
        })
    }

    pub(super) fn file_members(&self, wanted: Name) -> &'b [u8] {
        self.find_name(FILES, wanted).and_then(|b| b.get(8..)).unwrap_or(&[])
    }

    pub(super) fn member_v6(&self, members: &'b [u8], wanted: Name) -> Option<&'b [u8]> {
        let target = self.dicts.find_name(wanted)?;
        let (at, _) = crate::search(members.len() / FILE_MEMBER_V6, |at| {
            word(members.get(at * FILE_MEMBER_V6..).unwrap_or(&[])).cmp(&target)
        });
        at.and_then(|at| members.get(at * FILE_MEMBER_V6..(at + 1) * FILE_MEMBER_V6))
    }

    /// A successful lookup certifies a row under exactly this key, present and unowed.
    pub fn planned_key(&self, file: Name, wanted: Name) -> Option<Key> {
        let b = self.member_v6(self.file_members(file), wanted)?;
        (b.get(4).copied() == Some(1))
            .then(|| Key(self.dicts.id(word(b.get(5..).unwrap_or(&[])))))
    }

    /// Check whether `wanted`'s stored `BEP6` file members equal the freshly built flat staging
    /// slice `flat_members` (without allocating).
    pub(super) fn file_members_match_flat(&self, wanted: Name, flat_members: &[u8]) -> bool {
        let v6 = self.file_members(wanted);
        let count = v6.len() / FILE_MEMBER_V6;
        if count != flat_members.len() / FILE_MEMBER {
            return false;
        }
        for (vm, fm) in v6.chunks_exact(FILE_MEMBER_V6).zip(flat_members.chunks_exact(FILE_MEMBER)) {
            if self.dicts.name(word(vm)) != name(fm) {
                return false;
            }
            let standing = vm.get(4).copied().unwrap_or(0);
            if standing != fm.get(NAME).copied().unwrap_or(0) {
                return false;
            }
            if standing == 1 {
                if self.dicts.id(word(vm.get(5..).unwrap_or(&[]))) != id(fm.get(NAME + 1..).unwrap_or(&[])) {
                    return false;
                }
                if vm.get(9..12) != fm.get(NAME + 1 + ID..NAME + 1 + ID + 3) {
                    return false;
                }
                if self.dicts.id(word(vm.get(12..).unwrap_or(&[]))) != id(fm.get(NAME + 1 + ID + 3..).unwrap_or(&[])) {
                    return false;
                }
            }
        }
        true
    }
}
