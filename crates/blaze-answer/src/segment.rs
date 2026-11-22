//! The segment kinds (lib.rs, "The form"; plans and findings in the run's bytes, owed cells in
//! the cells segment's): each a builder that writes its bytes from sorted
//! columns, and a view over `&[u8]` that checks its bounds once at open and then reads every
//! field in place, infallibly.

use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};

use crate::bytes::{arr, filter_has, filter_set, filter_words, hash, packed_bytes, put_u16, put_u32, put_u64, search_u32, u16_at, u32_at, u64_at, unpack};
use crate::{AnswerError, CellDigest, CellNo, DIGEST_BYTES, FORMAT, Flags, MAX_ROWS_PER_SEGMENT, MAX_TASKS, Plant, SEGMENT_MAGIC, Serial, TaskId};

/// What a segment holds. Discriminants are the form's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum SegmentKind {
    /// Names and cell ids a request first named, with their dense ids.
    Dict = 1,
    /// The rows a request made or re-made, and the tasks it retired.
    Rows = 2,
    /// The cells that moved in a request, with that request's serial.
    Cells = 3,
    /// A run of signed reverse entries (cell, reader, +1 or -1).
    Run = 4,
    /// The file plans: a run of (a file's task id, a task its plan lists, +1 or -1), in the run's
    /// bytes.
    Plans = 5,
    /// The rows with findings: a run of (0, a task with findings or failed, +1 or -1).
    Findings = 6,
    /// The owed tasks, whole: the newest such segment is the answer's set (one per root).
    Owed = 7,
    /// The owed cells, whole, each with the request it moved in, in the cells segment's bytes: the
    /// newest such segment is the answer's set (one per root).
    OwedCells = 8,
}

impl SegmentKind {
    pub fn from_u8(byte: u8) -> Option<SegmentKind> {
        match byte {
            1 => Some(SegmentKind::Dict),
            2 => Some(SegmentKind::Rows),
            3 => Some(SegmentKind::Cells),
            4 => Some(SegmentKind::Run),
            5 => Some(SegmentKind::Plans),
            6 => Some(SegmentKind::Findings),
            7 => Some(SegmentKind::Owed),
            8 => Some(SegmentKind::OwedCells),
            _ => None,
        }
    }

    /// A kind whose segments are runs of signed entries, counted against [`crate::MAX_RUNS`].
    pub fn is_run(self) -> bool {
        matches!(self, SegmentKind::Run | SegmentKind::Plans | SegmentKind::Findings)
    }

    /// A kind whose newest segment is the whole set: a new one replaces the last's in the root.
    pub fn is_whole(self) -> bool {
        matches!(self, SegmentKind::Owed | SegmentKind::OwedCells)
    }
}

/// The head every segment starts with.
pub(crate) const HEAD: usize = 24;
const DICT_HEAD: usize = HEAD + 24;
const ROWS_HEAD: usize = HEAD + 16;
const CELLS_HEAD: usize = HEAD + 4;
const RUN_HEAD: usize = HEAD + 8;
const IDS_HEAD: usize = HEAD + 4;
/// Bit 31 of a run entry: the entry is -1.
const MINUS: u32 = 1 << 31;

fn put_head(out: &mut AVec<'_, u8>, kind: SegmentKind, serial: Serial) {
    out.extend_from_slice(&SEGMENT_MAGIC);
    put_u32(out, FORMAT);
    // Kind and seven zero bytes: the serial at 16, eight-aligned, the head 24 B (`HEAD`).
    out.extend_from_slice(&[kind as u8, 0, 0, 0, 0, 0, 0, 0]);
    put_u64(out, serial.0);
}

/// The segment's serial, once its head is this form's and of `kind`, with room for `fixed`.
fn check_head(bytes: &[u8], kind: SegmentKind, fixed: usize, segment: u32) -> Result<Serial, AnswerError> {
    if bytes.len() < fixed {
        return Err(AnswerError::SegmentSize { segment, section: "head" });
    }
    if bytes[..4] != SEGMENT_MAGIC {
        return Err(AnswerError::Magic { segment });
    }
    let format = u32_at(bytes, 4);
    if format != FORMAT {
        return Err(AnswerError::Format { segment, found: format });
    }
    if bytes[8] != kind as u8 {
        return Err(AnswerError::SegmentKind { segment, found: bytes[8] });
    }
    Ok(Serial(u64_at(bytes, 16)))
}

fn task_hash(kind: u8, subject: &[u8]) -> u64 {
    hash(&[&[kind], subject])
}

fn cell_hash(definer: u32, cell: &[u8]) -> u64 {
    hash(&[&definer.to_le_bytes(), cell])
}

fn count(bytes: &[u8], at: usize) -> usize {
    u32_at(bytes, at) as usize
}

// ---------------------------------------------------------------------------------------------
// The dictionary.

/// A request's new names and cell ids, in id order from `first_task` and `first_cell`.
pub(crate) struct DictBuilder<'m> {
    pub(crate) first_task: u32,
    pub(crate) first_cell: u32,
    kinds: AVec<'m, u8>,
    subjects: AVec<'m, [u8; 32]>,
    definers: AVec<'m, u32>,
    cells: AVec<'m, [u8; 16]>,
}

impl<'m> DictBuilder<'m> {
    pub(crate) fn new_in(first_task: u32, first_cell: u32, alloc: Alloc<'m>) -> DictBuilder<'m> {
        DictBuilder { first_task, first_cell, kinds: Vec::new_in(alloc), subjects: Vec::new_in(alloc), definers: Vec::new_in(alloc), cells: Vec::new_in(alloc) }
    }

    pub(crate) fn tasks(&self) -> u32 {
        self.kinds.len() as u32
    }

    pub(crate) fn cells(&self) -> u32 {
        self.cells.len() as u32
    }

    pub(crate) fn push_task(&mut self, name: Name) {
        self.kinds.push(name.kind as u8);
        self.subjects.push(name.subject.0);
    }

    pub(crate) fn name(&self, task: TaskId) -> Name {
        let local = (task.0 - self.first_task) as usize;
        let kind = Kind::from_u8(self.kinds[local]).expect("push_task stores a Kind");
        Name { kind, subject: ContentId(self.subjects[local]) }
    }

    pub(crate) fn push_cell(&mut self, definer: TaskId, cell: Cell) {
        self.definers.push(definer.0);
        self.cells.push(cell.0);
    }

    /// The definer of the builder's `local`th cell.
    pub(crate) fn definer(&self, local: usize) -> TaskId {
        TaskId(self.definers[local])
    }

    pub(crate) fn encode(&self, serial: Serial, alloc: Alloc<'m>) -> AVec<'m, u8> {
        let (n, m) = (self.kinds.len(), self.cells.len());
        let (tw, cw) = (filter_words(n), filter_words(m));
        let mut out = Vec::with_capacity_in(dict_layout(n, tw, m, cw)[8], alloc);
        put_head(&mut out, SegmentKind::Dict, serial);
        for v in [self.first_task, n as u32, tw as u32, self.first_cell, m as u32, cw as u32] {
            put_u32(&mut out, v);
        }
        out.extend_from_slice(&self.kinds);
        for subject in self.subjects.iter() {
            out.extend_from_slice(subject);
        }
        let mut sorted: AVec<'m, u32> = Vec::from_iter_in(0..n as u32, alloc);
        sorted.sort_unstable_by(|a, b| (self.kinds[*a as usize], &self.subjects[*a as usize]).cmp(&(self.kinds[*b as usize], &self.subjects[*b as usize])));
        for local in sorted.iter() {
            put_u32(&mut out, *local);
        }
        let mut filter: AVec<'m, u64> = Vec::from_elem_in(0, tw, alloc);
        for local in 0..n {
            filter_set(&mut filter, task_hash(self.kinds[local], &self.subjects[local]));
        }
        for word in filter.iter() {
            put_u64(&mut out, *word);
        }
        for definer in self.definers.iter() {
            put_u32(&mut out, *definer);
        }
        for cell in self.cells.iter() {
            out.extend_from_slice(cell);
        }
        let mut sorted: AVec<'m, u32> = Vec::from_iter_in(0..m as u32, alloc);
        sorted.sort_unstable_by(|a, b| (self.definers[*a as usize], &self.cells[*a as usize]).cmp(&(self.definers[*b as usize], &self.cells[*b as usize])));
        for local in sorted.iter() {
            put_u32(&mut out, *local);
        }
        let mut filter: AVec<'m, u64> = Vec::from_elem_in(0, cw, alloc);
        for local in 0..m {
            filter_set(&mut filter, cell_hash(self.definers[local], &self.cells[local]));
        }
        for word in filter.iter() {
            put_u64(&mut out, *word);
        }
        out
    }
}

/// Where each dictionary column starts, and the end: kind, subject, sorted, filter, definer,
/// cell, cell sorted, cell filter.
fn dict_layout(n: usize, tw: usize, m: usize, cw: usize) -> [usize; 9] {
    let widths = [n, 32 * n, 4 * n, 8 * tw, 4 * m, 16 * m, 4 * m, 8 * cw];
    let mut at = [0usize; 9];
    at[0] = DICT_HEAD;
    for (i, w) in widths.iter().enumerate() {
        at[i + 1] = at[i] + w;
    }
    at
}

/// One dictionary segment read in place: tasks `first_task..first_task + tasks` and cells
/// `first_cell..first_cell + cells`, each with its key, and the sorted runs a lookup walks.
#[derive(Clone, Copy)]
pub struct DictView<'a> {
    bytes: &'a [u8],
    pub(crate) first_task: u32,
    pub(crate) tasks: u32,
    task_words: usize,
    pub(crate) first_cell: u32,
    pub(crate) cells: u32,
    cell_words: usize,
    at: [usize; 9],
}

impl<'a> DictView<'a> {
    pub(crate) fn open(bytes: &'a [u8], segment: u32) -> Result<DictView<'a>, AnswerError> {
        check_head(bytes, SegmentKind::Dict, DICT_HEAD, segment)?;
        let (first_task, n, tw) = (u32_at(bytes, HEAD), count(bytes, HEAD + 4), count(bytes, HEAD + 8));
        let (first_cell, m, cw) = (u32_at(bytes, HEAD + 12), count(bytes, HEAD + 16), count(bytes, HEAD + 20));
        if tw != filter_words(n) || cw != filter_words(m) {
            return Err(AnswerError::SegmentSize { segment, section: "filters" });
        }
        let at = dict_layout(n, tw, m, cw);
        if at[8] != bytes.len() {
            return Err(AnswerError::SegmentSize { segment, section: "columns" });
        }
        let end = u64::from(first_task) + n as u64;
        if end > u64::from(MAX_TASKS) || u64::from(first_cell) + m as u64 > u64::from(u32::MAX) {
            return Err(AnswerError::Bounds { segment, section: "id ranges", at: 0 });
        }
        for local in 0..n {
            if Kind::from_u8(bytes[at[0] + local]).is_none() {
                return Err(AnswerError::Bounds { segment, section: "task kinds", at: local });
            }
            if count(bytes, at[2] + 4 * local) >= n {
                return Err(AnswerError::Bounds { segment, section: "task sorted", at: local });
            }
        }
        for local in 0..m {
            if u64::from(u32_at(bytes, at[4] + 4 * local)) >= end {
                return Err(AnswerError::Bounds { segment, section: "cell definers", at: local });
            }
            if count(bytes, at[6] + 4 * local) >= m {
                return Err(AnswerError::Bounds { segment, section: "cell sorted", at: local });
            }
        }
        Ok(DictView { bytes, first_task, tasks: n as u32, task_words: tw, first_cell, cells: m as u32, cell_words: cw, at })
    }

    fn task_key(&self, local: usize) -> (u8, &'a [u8]) {
        (self.bytes[self.at[0] + local], &self.bytes[self.at[1] + 32 * local..self.at[1] + 32 * local + 32])
    }

    fn cell_key(&self, local: usize) -> (u32, &'a [u8]) {
        (u32_at(self.bytes, self.at[4] + 4 * local), &self.bytes[self.at[5] + 16 * local..self.at[5] + 16 * local + 16])
    }

    /// The name of the segment's `local`th task.
    pub(crate) fn name(&self, local: usize) -> Name {
        let (kind, subject) = self.task_key(local);
        let kind = Kind::from_u8(kind).expect("a dictionary's kinds are checked at open");
        Name { kind, subject: ContentId(arr(subject, 0)) }
    }

    /// The definer and cell of the segment's `local`th cell.
    pub(crate) fn cell(&self, local: usize) -> (TaskId, Cell) {
        let (definer, cell) = self.cell_key(local);
        (TaskId(definer), Cell(arr(cell, 0)))
    }

    /// The id of `name`, when this segment gave it.
    pub(crate) fn find_task(&self, name: &Name) -> Option<TaskId> {
        let key = (name.kind as u8, &name.subject.0[..]);
        if !filter_has(self.bytes, self.at[3], self.task_words, task_hash(key.0, key.1)) {
            return None;
        }
        let n = self.tasks as usize;
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let local = count(self.bytes, self.at[2] + 4 * mid);
            match self.task_key(local).cmp(&key) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(TaskId(self.first_task + local as u32)),
            }
        }
        None
    }

    /// The number of `(definer, cell)`, when this segment gave it.
    pub(crate) fn find_cell(&self, definer: TaskId, cell: &Cell) -> Option<CellNo> {
        let key = (definer.0, &cell.0[..]);
        if !filter_has(self.bytes, self.at[7], self.cell_words, cell_hash(key.0, key.1)) {
            return None;
        }
        let m = self.cells as usize;
        let (mut lo, mut hi) = (0, m);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let local = count(self.bytes, self.at[6] + 4 * mid);
            match self.cell_key(local).cmp(&key) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(CellNo(self.first_cell + local as u32)),
            }
        }
        None
    }

    pub(crate) fn verify(&self, segment: u32) -> Result<(), AnswerError> {
        for local in 0..self.tasks as usize {
            let (kind, subject) = self.task_key(local);
            if !filter_has(self.bytes, self.at[3], self.task_words, task_hash(kind, subject)) {
                return Err(AnswerError::FilterMissing { segment, section: "task", at: local });
            }
        }
        for local in 0..self.cells as usize {
            let (definer, selector) = self.cell_key(local);
            if !filter_has(self.bytes, self.at[7], self.cell_words, cell_hash(definer, selector)) {
                return Err(AnswerError::FilterMissing { segment, section: "cell", at: local });
            }
        }
        for mid in 1..self.tasks as usize {
            let (a, b) = (count(self.bytes, self.at[2] + 4 * (mid - 1)), count(self.bytes, self.at[2] + 4 * mid));
            if self.task_key(a) >= self.task_key(b) {
                return Err(AnswerError::Unsorted { segment, section: "task sorted", at: mid });
            }
        }
        for mid in 1..self.cells as usize {
            let (a, b) = (count(self.bytes, self.at[6] + 4 * (mid - 1)), count(self.bytes, self.at[6] + 4 * mid));
            if self.cell_key(a) >= self.cell_key(b) {
                return Err(AnswerError::Unsorted { segment, section: "cell sorted", at: mid });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[test]
fn a_dictionary_filter_cannot_hide_a_stored_task() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut dict = DictBuilder::new_in(0, 0, alloc);
    let name = Name { kind: Kind::FileItems, subject: ContentId([9; 32]) };
    dict.push_task(name);
    let mut bytes = dict.encode(Serial(1), alloc);
    let at = dict_layout(1, filter_words(1), 0, filter_words(0));
    bytes[at[3]..at[4]].fill(0);
    let view = DictView::open(&bytes, 0).expect("the directory bounds are intact");
    assert_eq!(view.verify(0), Err(AnswerError::FilterMissing { segment: 0, section: "task", at: 0 }));
}

// ---------------------------------------------------------------------------------------------
// Rows.

/// A row's fixed and cold fields, as a builder takes them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RowHead {
    pub(crate) task: u32,
    pub(crate) key: [u8; 32],
    pub(crate) out: [u8; 32],
    pub(crate) serial: u64,
    pub(crate) file: u32,
    pub(crate) stage: u8,
    pub(crate) flags: u8,
    pub(crate) wave: u16,
    pub(crate) source: [u8; 32],
    pub(crate) findings: u32,
    pub(crate) record: [u8; 32],
}

struct RowRec {
    head: RowHead,
    meta: u32,
    reads: (usize, usize),
    defines: (usize, usize),
}

/// A segment's rows and tombstones before they are written, in any order.
pub(crate) struct RowsBuilder<'m> {
    recs: AVec<'m, RowRec>,
    reads: AVec<'m, u8>,
    defines: AVec<'m, (u32, [u8; DIGEST_BYTES])>,
    tombs: AVec<'m, u32>,
}

impl<'m> RowsBuilder<'m> {
    pub(crate) fn new_in(alloc: Alloc<'m>) -> RowsBuilder<'m> {
        RowsBuilder { recs: Vec::new_in(alloc), reads: Vec::new_in(alloc), defines: Vec::new_in(alloc), tombs: Vec::new_in(alloc) }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.recs.is_empty() && self.tombs.is_empty()
    }

    /// One row: its reads as packed (`meta` its count and width, `reads` its base and bits) and
    /// its defines sorted by cell.
    pub(crate) fn push(&mut self, head: RowHead, meta: u32, reads: &[u8], defines: &[(u32, [u8; DIGEST_BYTES])]) {
        let r0 = self.reads.len();
        self.reads.extend_from_slice(reads);
        let d0 = self.defines.len();
        self.defines.extend_from_slice(defines);
        self.recs.push(RowRec { head, meta, reads: (r0, self.reads.len()), defines: (d0, self.defines.len()) });
    }

    pub(crate) fn tomb(&mut self, task: TaskId) {
        self.tombs.push(task.0);
    }

    pub(crate) fn encode(&mut self, serial: Serial, alloc: Alloc<'m>) -> Result<AVec<'m, u8>, AnswerError> {
        let (n, t, r, d) = (self.recs.len(), self.tombs.len(), self.reads.len(), self.defines.len());
        if n > MAX_ROWS_PER_SEGMENT || t > MAX_ROWS_PER_SEGMENT {
            return Err(AnswerError::Cap { what: "rows a segment", cap: MAX_ROWS_PER_SEGMENT as u64 });
        }
        if r > u32::MAX as usize || d > u32::MAX as usize {
            return Err(AnswerError::Cap { what: "read bytes or defines a segment", cap: u64::from(u32::MAX) });
        }
        self.recs.sort_unstable_by_key(|rec| rec.head.task);
        self.tombs.sort_unstable();
        let at = rows_layout(n, t, r, d);
        let mut out = Vec::with_capacity_in(at[END], alloc);
        put_head(&mut out, SegmentKind::Rows, serial);
        for v in [n, t, r, d] {
            put_u32(&mut out, v as u32);
        }
        let recs = &self.recs;
        for rec in recs.iter() {
            put_u32(&mut out, rec.head.task);
        }
        for rec in recs.iter() {
            out.extend_from_slice(&rec.head.key);
        }
        for rec in recs.iter() {
            out.extend_from_slice(&rec.head.out);
        }
        for rec in recs.iter() {
            put_u64(&mut out, rec.head.serial);
        }
        for rec in recs.iter() {
            put_u32(&mut out, rec.head.file);
        }
        for rec in recs.iter() {
            out.extend_from_slice(&[rec.head.stage, rec.head.flags]);
            put_u16(&mut out, rec.head.wave);
        }
        let mut offset = 0usize;
        for rec in recs.iter() {
            put_u32(&mut out, offset as u32);
            offset += rec.reads.1 - rec.reads.0;
        }
        for rec in recs.iter() {
            put_u32(&mut out, rec.meta);
        }
        let mut end = 0usize;
        for rec in recs.iter() {
            end += rec.defines.1 - rec.defines.0;
            put_u32(&mut out, end as u32);
        }
        for rec in recs.iter() {
            out.extend_from_slice(&rec.head.source);
        }
        for rec in recs.iter() {
            put_u32(&mut out, rec.head.findings);
        }
        for rec in recs.iter() {
            out.extend_from_slice(&rec.head.record);
        }
        for tomb in self.tombs.iter() {
            put_u32(&mut out, *tomb);
        }
        for rec in recs.iter() {
            out.extend_from_slice(&self.reads[rec.reads.0..rec.reads.1]);
        }
        for rec in recs.iter() {
            for (cell, _) in self.defines[rec.defines.0..rec.defines.1].iter() {
                put_u32(&mut out, *cell);
            }
        }
        for rec in recs.iter() {
            for (_, digest) in self.defines[rec.defines.0..rec.defines.1].iter() {
                out.extend_from_slice(digest);
            }
        }
        Ok(out)
    }
}

const TASK: usize = 0;
const KEY: usize = 1;
const OUT: usize = 2;
const SERIAL: usize = 3;
const FILE: usize = 4;
const PHASE: usize = 5;
const READS_AT: usize = 6;
const READS_META: usize = 7;
const DEFINES_END: usize = 8;
const SOURCE: usize = 9;
const FINDINGS: usize = 10;
const RECORD: usize = 11;
const TOMB: usize = 12;
const READS: usize = 13;
const DEF_CELL: usize = 14;
const DEF_DIGEST: usize = 15;
const END: usize = 16;

/// Where each rows column starts, and the end (the column order of lib.rs, "The form").
fn rows_layout(n: usize, t: usize, r: usize, d: usize) -> [usize; 17] {
    let widths = [4 * n, 32 * n, 32 * n, 8 * n, 4 * n, 4 * n, 4 * n, 4 * n, 4 * n, 32 * n, 4 * n, 32 * n, 4 * t, r, 4 * d, DIGEST_BYTES * d];
    let mut at = [0usize; 17];
    at[0] = ROWS_HEAD;
    for (i, w) in widths.iter().enumerate() {
        at[i + 1] = at[i] + w;
    }
    at
}

/// One rows segment read in place: rows sorted by task, the tasks it retired, the read lists and
/// the defines.
#[derive(Clone, Copy)]
pub struct RowsView<'a> {
    bytes: &'a [u8],
    n: usize,
    t: usize,
    r: usize,
    at: [usize; 17],
}

/// Where a rows segment's bytes go, for the size test and the profile.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RowsBytes {
    pub rows: usize,
    pub fixed: usize,
    pub cold: usize,
    pub reads: usize,
    pub read_count: usize,
    pub defines: usize,
    pub define_count: usize,
    pub tombs: usize,
}

impl<'a> RowsView<'a> {
    pub(crate) fn open(bytes: &'a [u8], segment: u32) -> Result<RowsView<'a>, AnswerError> {
        check_head(bytes, SegmentKind::Rows, ROWS_HEAD, segment)?;
        let (n, t, r, d) = (count(bytes, HEAD), count(bytes, HEAD + 4), count(bytes, HEAD + 8), count(bytes, HEAD + 12));
        if n > MAX_ROWS_PER_SEGMENT || t > MAX_ROWS_PER_SEGMENT {
            return Err(AnswerError::Cap { what: "rows a segment", cap: MAX_ROWS_PER_SEGMENT as u64 });
        }
        let at = rows_layout(n, t, r, d);
        if at[END] != bytes.len() {
            return Err(AnswerError::SegmentSize { segment, section: "columns" });
        }
        let mut last_define = 0usize;
        for i in 0..n {
            let start = count(bytes, at[READS_AT] + 4 * i);
            let end = if i + 1 < n { count(bytes, at[READS_AT] + 4 * (i + 1)) } else { r };
            let meta = u32_at(bytes, at[READS_META] + 4 * i);
            let (reads, width) = ((meta & 0x00FF_FFFF) as usize, meta >> 24);
            if start > end || end > r || width > 32 || end - start < 4 + packed_bytes(reads, width) {
                return Err(AnswerError::Bounds { segment, section: "reads", at: i });
            }
            let define_end = count(bytes, at[DEFINES_END] + 4 * i);
            if define_end < last_define || define_end > d {
                return Err(AnswerError::Bounds { segment, section: "defines", at: i });
            }
            last_define = define_end;
        }
        Ok(RowsView { bytes, n, t, r, at })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The index of `task`'s row here.
    pub(crate) fn find(&self, task: TaskId) -> Option<usize> {
        search_u32(self.bytes, self.at[TASK], self.n, task.0).ok()
    }

    /// Whether this segment retired `task`.
    pub(crate) fn retires(&self, task: TaskId) -> bool {
        search_u32(self.bytes, self.at[TOMB], self.t, task.0).is_ok()
    }

    pub(crate) fn row(&self, i: usize) -> RowView<'_> {
        RowView { seg: self, i }
    }

    pub fn bytes(&self) -> RowsBytes {
        let read_count = (0..self.n).map(|i| (u32_at(self.bytes, self.at[READS_META] + 4 * i) & 0x00FF_FFFF) as usize).sum();
        RowsBytes {
            rows: self.n,
            fixed: self.at[SOURCE] - self.at[TASK],
            cold: self.at[TOMB] - self.at[SOURCE],
            reads: self.r,
            read_count,
            defines: self.at[END] - self.at[DEF_CELL],
            define_count: (self.at[DEF_DIGEST] - self.at[DEF_CELL]) / 4,
            tombs: self.at[READS] - self.at[TOMB],
        }
    }

    pub(crate) fn verify(&self, segment: u32) -> Result<(), AnswerError> {
        for i in 1..self.n {
            if u32_at(self.bytes, self.at[TASK] + 4 * (i - 1)) >= u32_at(self.bytes, self.at[TASK] + 4 * i) {
                return Err(AnswerError::Unsorted { segment, section: "rows", at: i });
            }
        }
        for i in 1..self.t {
            if u32_at(self.bytes, self.at[TOMB] + 4 * (i - 1)) >= u32_at(self.bytes, self.at[TOMB] + 4 * i) {
                return Err(AnswerError::Unsorted { segment, section: "tombs", at: i });
            }
        }
        for i in 0..self.n {
            let (start, end) = self.row(i).define_range();
            for j in start + 1..end {
                if u32_at(self.bytes, self.at[DEF_CELL] + 4 * (j - 1)) >= u32_at(self.bytes, self.at[DEF_CELL] + 4 * j) {
                    return Err(AnswerError::Unsorted { segment, section: "defines", at: j });
                }
            }
        }
        Ok(())
    }
}

/// One task's row, read in place: the engine's `Row` less its causes and findings, its reads as
/// cell numbers in read order and its defines as (cell, digest) sorted by cell.
#[derive(Clone, Copy)]
pub struct RowView<'v> {
    seg: &'v RowsView<'v>,
    i: usize,
}

impl<'v> RowView<'v> {
    fn field(&self, column: usize, width: usize) -> usize {
        self.seg.at[column] + width * self.i
    }

    pub fn task(&self) -> TaskId {
        TaskId(u32_at(self.seg.bytes, self.field(TASK, 4)))
    }

    /// The task's key: its kind's tool, its name and its source.
    pub fn key(&self) -> Key {
        Key(ContentId(arr(self.seg.bytes, self.field(KEY, 32))))
    }

    /// The digest of what it made: equal bytes move no reader.
    pub fn out(&self) -> ContentId {
        ContentId(arr(self.seg.bytes, self.field(OUT, 32)))
    }

    /// The request that made it.
    pub fn serial(&self) -> Serial {
        Serial(u64_at(self.seg.bytes, self.field(SERIAL, 8)))
    }

    /// The file whose outline lists it (its outline task's id).
    pub fn file(&self) -> TaskId {
        TaskId(u32_at(self.seg.bytes, self.field(FILE, 4)))
    }

    /// Its phase's stage, as the engine numbers stages.
    pub fn stage(&self) -> u8 {
        self.seg.bytes[self.field(PHASE, 4)]
    }

    pub fn flags(&self) -> Flags {
        Flags(self.seg.bytes[self.field(PHASE, 4) + 1])
    }

    /// Its phase's wave inside its stage.
    pub fn wave(&self) -> u16 {
        u16_at(self.seg.bytes, self.field(PHASE, 4) + 2)
    }

    /// Its source (cold): what an owed task is planned again from.
    pub fn source(&self) -> ContentId {
        ContentId(arr(self.seg.bytes, self.field(SOURCE, 32)))
    }

    /// How many findings it made (cold): the findings themselves are in its record.
    pub fn findings(&self) -> u32 {
        u32_at(self.seg.bytes, self.field(FINDINGS, 4))
    }

    /// The store key of the record its last entry made (cold): what a restart reads its result
    /// and findings from.
    pub fn record(&self) -> Key {
        Key(ContentId(arr(self.seg.bytes, self.field(RECORD, 32))))
    }

    fn meta(&self) -> u32 {
        u32_at(self.seg.bytes, self.field(READS_META, 4))
    }

    fn read_bytes(&self) -> &'v [u8] {
        let start = count(self.seg.bytes, self.field(READS_AT, 4));
        let end = if self.i + 1 < self.seg.n { count(self.seg.bytes, self.field(READS_AT, 4) + 4) } else { self.seg.r };
        &self.seg.bytes[self.seg.at[READS] + start..self.seg.at[READS] + end]
    }

    pub fn read_count(&self) -> usize {
        (self.meta() & 0x00FF_FFFF) as usize
    }

    /// The cells it read, in the order it read them (a cell read twice is listed twice).
    pub fn reads(&self) -> Reads<'v> {
        let bytes = self.read_bytes();
        Reads { packed: &bytes[4..], base: u32_at(bytes, 0), width: self.meta() >> 24, i: 0, n: self.read_count() }
    }

    /// The read list as stored (its count and width, its base and bits): copied whole by
    /// compaction.
    pub(crate) fn packed(&self) -> (u32, &'v [u8]) {
        (self.meta(), self.read_bytes())
    }

    fn define_range(&self) -> (usize, usize) {
        let start = if self.i == 0 { 0 } else { count(self.seg.bytes, self.field(DEFINES_END, 4) - 4) };
        (start, count(self.seg.bytes, self.field(DEFINES_END, 4)))
    }

    pub fn define_count(&self) -> usize {
        let (start, end) = self.define_range();
        end - start
    }

    /// The `j`th of its defines, by cell.
    pub fn define(&self, j: usize) -> (CellNo, CellDigest) {
        let at = self.define_range().0 + j;
        (CellNo(u32_at(self.seg.bytes, self.seg.at[DEF_CELL] + 4 * at)), CellDigest(arr(self.seg.bytes, self.seg.at[DEF_DIGEST] + DIGEST_BYTES * at)))
    }

    /// The cells it defines and their digests, sorted by cell.
    pub fn defines(&self) -> impl Iterator<Item = (CellNo, CellDigest)> + 'v {
        let row = *self;
        (0..self.define_count()).map(move |j| row.define(j))
    }

    /// The digest it gives `cell`, when it defines it.
    pub fn digest_of(&self, cell: CellNo) -> Option<CellDigest> {
        let (start, end) = self.define_range();
        let j = search_u32(self.seg.bytes, self.seg.at[DEF_CELL] + 4 * start, end - start, cell.0).ok()?;
        Some(self.define(j).1)
    }
}

/// A row's reads, unpacked one at a time in read order.
#[derive(Clone)]
pub struct Reads<'v> {
    packed: &'v [u8],
    base: u32,
    width: u32,
    i: usize,
    n: usize,
}

impl Iterator for Reads<'_> {
    type Item = CellNo;

    fn next(&mut self) -> Option<CellNo> {
        if self.i == self.n {
            return None;
        }
        let cell = unpack(self.packed, self.i, self.base, self.width);
        self.i += 1;
        Some(CellNo(cell))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.n - self.i, Some(self.n - self.i))
    }
}

impl ExactSizeIterator for Reads<'_> {}

// ---------------------------------------------------------------------------------------------
// Cells.

/// The segment of `kind` ([`SegmentKind::Cells`] or [`SegmentKind::OwedCells`]) of `moved`, each
/// (cell, the serial it last moved at), ascending by cell.
pub(crate) fn encode_cells<'m>(kind: SegmentKind, moved: &[(u32, u64)], serial: Serial, alloc: Alloc<'m>) -> AVec<'m, u8> {
    let mut out = Vec::with_capacity_in(CELLS_HEAD + 12 * moved.len(), alloc);
    put_head(&mut out, kind, serial);
    put_u32(&mut out, moved.len() as u32);
    for (cell, _) in moved.iter() {
        put_u32(&mut out, *cell);
    }
    for (_, at) in moved.iter() {
        put_u64(&mut out, *at);
    }
    out
}

/// One cells segment read in place: the serial each listed cell last moved at, as of its request.
#[derive(Clone, Copy)]
pub struct CellsView<'a> {
    bytes: &'a [u8],
    n: usize,
}

impl<'a> CellsView<'a> {
    pub(crate) fn open(bytes: &'a [u8], kind: SegmentKind, segment: u32) -> Result<CellsView<'a>, AnswerError> {
        check_head(bytes, kind, CELLS_HEAD, segment)?;
        let n = count(bytes, HEAD);
        if CELLS_HEAD + 12 * n != bytes.len() {
            return Err(AnswerError::SegmentSize { segment, section: "columns" });
        }
        Ok(CellsView { bytes, n })
    }

    pub(crate) fn find(&self, cell: CellNo) -> Option<Serial> {
        let i = search_u32(self.bytes, CELLS_HEAD, self.n, cell.0).ok()?;
        Some(Serial(u64_at(self.bytes, CELLS_HEAD + 4 * self.n + 8 * i)))
    }

    /// Every (cell, serial), ascending by cell.
    pub(crate) fn all(&self) -> impl Iterator<Item = (CellNo, Serial)> + '_ {
        (0..self.n).map(move |i| (CellNo(u32_at(self.bytes, CELLS_HEAD + 4 * i)), Serial(u64_at(self.bytes, CELLS_HEAD + 4 * self.n + 8 * i))))
    }

    pub(crate) fn verify(&self, segment: u32) -> Result<(), AnswerError> {
        for i in 1..self.n {
            if u32_at(self.bytes, CELLS_HEAD + 4 * (i - 1)) >= u32_at(self.bytes, CELLS_HEAD + 4 * i) {
                return Err(AnswerError::Unsorted { segment, section: "cells", at: i });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Runs.

/// What a run's entries may sum to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RunSums {
    /// One request's run: a reader added (+1), removed (-1), or both (dropped).
    Request,
    /// Every run of an answer merged: a reader (+1), or none (dropped).
    Whole,
}

/// The run of `kind` (a [`SegmentKind::is_run`] kind) of `entries` (key, task, +1 or -1): the key
/// a cell, a file's task id or 0. Sorted and summed per (key, task); `None` when every entry
/// cancels. A sum outside `sums` is [`AnswerError::ReadersSum`].
pub(crate) fn encode_run<'m>(kind: SegmentKind, entries: &mut AVec<'_, (u32, u32, i32)>, sums: RunSums, plant: Plant, serial: Serial, alloc: Alloc<'m>) -> Result<Option<AVec<'m, u8>>, AnswerError> {
    entries.sort_unstable_by_key(|(cell, task, _)| (*cell, *task));
    let mut cells: AVec<'m, u32> = Vec::new_in(alloc);
    let mut starts: AVec<'m, u32> = Vec::new_in(alloc);
    let mut kept: AVec<'m, u32> = Vec::new_in(alloc);
    let mut i = 0;
    while i < entries.len() {
        let (cell, task, _) = entries[i];
        let mut sum = 0i32;
        while i < entries.len() && entries[i].0 == cell && entries[i].1 == task {
            sum += entries[i].2;
            i += 1;
        }
        let entry = match (sum, sums) {
            (1, _) => Some(task),
            (-1, RunSums::Request) => Some(task | MINUS),
            (0, RunSums::Whole) if plant == Plant::CancelledKept => Some(task),
            (0, _) => None,
            _ => return Err(AnswerError::ReadersSum { kind, cell: CellNo(cell), task: TaskId(task), sum }),
        };
        if let Some(entry) = entry {
            if cells.last() != Some(&cell) {
                cells.push(cell);
                starts.push(kept.len() as u32);
            }
            kept.push(entry);
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    if kept.len() > u32::MAX as usize {
        return Err(AnswerError::Cap { what: "entries a run", cap: u64::from(u32::MAX) });
    }
    starts.push(kept.len() as u32);
    let mut out = Vec::with_capacity_in(RUN_HEAD + 4 * (cells.len() + starts.len() + kept.len()), alloc);
    put_head(&mut out, kind, serial);
    put_u32(&mut out, cells.len() as u32);
    put_u32(&mut out, kept.len() as u32);
    for column in [&cells, &starts, &kept] {
        for v in column.iter() {
            put_u32(&mut out, *v);
        }
    }
    Ok(Some(out))
}

/// One run read in place: for each of its cells, the readers added (+1) and removed (-1).
#[derive(Clone, Copy)]
pub struct RunView<'a> {
    bytes: &'a [u8],
    k: usize,
}

impl<'a> RunView<'a> {
    pub(crate) fn open(bytes: &'a [u8], kind: SegmentKind, segment: u32) -> Result<RunView<'a>, AnswerError> {
        check_head(bytes, kind, RUN_HEAD, segment)?;
        let (k, e) = (count(bytes, HEAD), count(bytes, HEAD + 4));
        if RUN_HEAD + 4 * (k + k + 1 + e) != bytes.len() {
            return Err(AnswerError::SegmentSize { segment, section: "columns" });
        }
        let run = RunView { bytes, k };
        let mut last = 0usize;
        for j in 0..=k {
            let start = run.start(j);
            if start < last || start > e || (j == 0 && start != 0) || (j == k && start != e) {
                return Err(AnswerError::Bounds { segment, section: "run starts", at: j });
            }
            last = start;
        }
        Ok(run)
    }

    fn start(&self, j: usize) -> usize {
        count(self.bytes, RUN_HEAD + 4 * self.k + 4 * j)
    }

    fn entry(&self, at: usize) -> (TaskId, i32) {
        let raw = u32_at(self.bytes, RUN_HEAD + 4 * (2 * self.k + 1) + 4 * at);
        match raw & MINUS {
            0 => (TaskId(raw), 1),
            _ => (TaskId(raw & !MINUS), -1),
        }
    }

    fn cell(&self, j: usize) -> u32 {
        u32_at(self.bytes, RUN_HEAD + 4 * j)
    }

    /// The entries of `cell` in this run, by task.
    pub(crate) fn of(&self, cell: CellNo) -> impl Iterator<Item = (TaskId, i32)> + '_ {
        let range = match self.k > 0 && self.cell(0) <= cell.0 && cell.0 <= self.cell(self.k - 1) {
            true => match search_u32(self.bytes, RUN_HEAD, self.k, cell.0) {
                Ok(j) => self.start(j)..self.start(j + 1),
                Err(_) => 0..0,
            },
            false => 0..0,
        };
        range.map(|at| self.entry(at))
    }

    /// `task`'s entry under `cell` in this run (+1, -1, or 0 for none), and the fields compared:
    /// a binary search for the cell, then one for the task among its entries (ascending by task,
    /// [`RunView::verify`]), read in place.
    pub(crate) fn sign_of(&self, cell: CellNo, task: TaskId) -> (i32, u32) {
        let mut compared = 0u32;
        let (mut lo, mut hi) = (0usize, self.k);
        let mut found = None;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            compared += 1;
            match self.cell(mid).cmp(&cell.0) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    found = Some(mid);
                    break;
                }
            }
        }
        let Some(j) = found else { return (0, compared) };
        let (mut lo, mut hi) = (self.start(j), self.start(j + 1));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            compared += 1;
            let (at, sign) = self.entry(mid);
            match at.0.cmp(&task.0) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return (sign, compared),
            }
        }
        (0, compared)
    }

    /// Every entry: (cell, task, +1 or -1).
    pub(crate) fn all(&self) -> impl Iterator<Item = (u32, u32, i32)> + '_ {
        (0..self.k).flat_map(move |j| (self.start(j)..self.start(j + 1)).map(move |at| {
            let (task, sign) = self.entry(at);
            (self.cell(j), task.0, sign)
        }))
    }

    pub(crate) fn verify(&self, segment: u32) -> Result<(), AnswerError> {
        for j in 0..self.k {
            if j > 0 && self.cell(j - 1) >= self.cell(j) {
                return Err(AnswerError::Unsorted { segment, section: "run cells", at: j });
            }
            for at in self.start(j) + 1..self.start(j + 1) {
                if self.entry(at - 1).0 >= self.entry(at).0 {
                    return Err(AnswerError::Unsorted { segment, section: "run entries", at });
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Owed tasks.

/// The owed tasks segment of `tasks`, ascending.
pub(crate) fn encode_owed<'m>(tasks: &[u32], serial: Serial, alloc: Alloc<'m>) -> AVec<'m, u8> {
    let mut out = Vec::with_capacity_in(IDS_HEAD + 4 * tasks.len(), alloc);
    put_head(&mut out, SegmentKind::Owed, serial);
    put_u32(&mut out, tasks.len() as u32);
    for task in tasks.iter() {
        put_u32(&mut out, *task);
    }
    out
}

/// One owed tasks segment read in place: the answer's owed tasks, ascending.
#[derive(Clone, Copy)]
pub struct OwedView<'a> {
    bytes: &'a [u8],
    n: usize,
}

impl<'a> OwedView<'a> {
    pub(crate) fn open(bytes: &'a [u8], segment: u32) -> Result<OwedView<'a>, AnswerError> {
        check_head(bytes, SegmentKind::Owed, IDS_HEAD, segment)?;
        let n = count(bytes, HEAD);
        if IDS_HEAD + 4 * n != bytes.len() {
            return Err(AnswerError::SegmentSize { segment, section: "owed tasks" });
        }
        Ok(OwedView { bytes, n })
    }

    pub(crate) fn contains(&self, task: TaskId) -> bool {
        search_u32(self.bytes, IDS_HEAD, self.n, task.0).is_ok()
    }

    pub(crate) fn all(&self) -> impl Iterator<Item = TaskId> + '_ {
        (0..self.n).map(move |i| TaskId(u32_at(self.bytes, IDS_HEAD + 4 * i)))
    }

    pub(crate) fn verify(&self, segment: u32) -> Result<(), AnswerError> {
        for i in 1..self.n {
            if u32_at(self.bytes, IDS_HEAD + 4 * (i - 1)) >= u32_at(self.bytes, IDS_HEAD + 4 * i) {
                return Err(AnswerError::Unsorted { segment, section: "owed tasks", at: i });
            }
        }
        Ok(())
    }
}
