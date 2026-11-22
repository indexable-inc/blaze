//! The root and the answer it names: [`AnswerView`] opens a root over its segments' bytes (mapped
//! or read: this crate only sees slices), checks each bound once, then answers every lookup a
//! request makes in place: a task's id and row, a cell's number, its last move and its digest,
//! its readers, whether a row stands.

use blaze_drive::cells::Cell;
use blaze_drive::key::Name;
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, SharedAlloc, Vec};

/// A view's own table: on [`SharedAlloc`], so an [`AnswerView`] is `Sync` and a phase's workers
/// read it at once.
type Table<'a, T> = Vec<T, SharedAlloc<'a>>;

use crate::bytes::{arr, put_u32, put_u64, u32_at, u64_at};
use crate::segment::{CellsView, DictView, OwedView, RowView, RowsBytes, RowsView, RunView, SegmentKind};
use crate::{AnswerError, COMPACT_AT, CellDigest, CellNo, FORMAT, LinesKey, MAX_RUNS, MAX_SEGMENTS, MAX_SUMMARY, Plant, ROOT, ROOT_MAGIC, Serial, TaskId, TreeDigest};

/// The domain of a segment's id: the content digest of its bytes.
pub(crate) const SEGMENT_DOMAIN: &str = "blaze-answer segment 1";

const ROOT_HEAD: usize = 4 + 4 + 8 + 32 + 32 + 4;
const ROOT_ENTRY: usize = 1 + 8 + 32;

/// One segment as its root names it: what it holds, its length, and the digest of its bytes,
/// which is its name in the store (so a second worktree's root names the same segments).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentRef {
    pub kind: SegmentKind,
    pub len: u64,
    pub id: ContentId,
}

impl SegmentRef {
    /// The reference a root holds for `bytes` of `kind`.
    pub fn of(kind: SegmentKind, bytes: &[u8]) -> SegmentRef {
        SegmentRef { kind, len: bytes.len() as u64, id: ContentId::of(SEGMENT_DOMAIN, bytes) }
    }
}

/// A bounded root directory. Parsing checks its structure without opening any child.
#[derive(Clone, Copy, Debug)]
pub struct RootView<'a> {
    serial: Serial,
    tree: TreeDigest,
    lines: LinesKey,
    summary: &'a [u8],
    entries: &'a [u8],
}

impl<'a> RootView<'a> {
    /// Maximum bytes to read or authenticate for one root.
    pub const MAX_BYTES: usize = ROOT_HEAD + MAX_SUMMARY + 4 + MAX_SEGMENTS * ROOT_ENTRY;

    /// Validate the root's format, layout, directory limits and set cardinalities.
    pub fn parse(bytes: &'a [u8]) -> Result<RootView<'a>, AnswerError> {
        if bytes.len() < ROOT_HEAD {
            return Err(AnswerError::RootShort { at: bytes.len() });
        }
        if bytes[..4] != ROOT_MAGIC {
            return Err(AnswerError::Magic { segment: ROOT });
        }
        let format = u32_at(bytes, 4);
        if format != FORMAT {
            return Err(AnswerError::Format { segment: ROOT, found: format });
        }
        let summary_len = u32_at(bytes, ROOT_HEAD - 4) as usize;
        if summary_len > MAX_SUMMARY {
            return Err(AnswerError::Cap { what: "summary bytes", cap: MAX_SUMMARY as u64 });
        }
        let at = ROOT_HEAD + summary_len;
        if bytes.len() < at + 4 {
            return Err(AnswerError::RootShort { at: bytes.len() });
        }
        let count = u32_at(bytes, at) as usize;
        if count > MAX_SEGMENTS {
            return Err(AnswerError::Cap { what: "segments a root", cap: MAX_SEGMENTS as u64 });
        }
        let end = at + 4 + count * ROOT_ENTRY;
        if bytes.len() < end {
            return Err(AnswerError::RootShort { at: bytes.len() });
        }
        if bytes.len() > end {
            return Err(AnswerError::RootTrailing { at: end });
        }
        let entries = &bytes[at + 4..end];
        let mut runs = 0;
        let mut whole = 0u16;
        for (i, entry) in entries.chunks_exact(ROOT_ENTRY).enumerate() {
            let segment = i as u32;
            let kind = SegmentKind::from_u8(entry[0]).ok_or(AnswerError::SegmentKind { segment, found: entry[0] })?;
            if kind.is_run() {
                runs += 1;
                if runs > MAX_RUNS {
                    return Err(AnswerError::Cap { what: "runs a root", cap: MAX_RUNS as u64 });
                }
            }
            if kind.is_whole() {
                let bit = 1u16 << entry[0];
                if whole & bit != 0 {
                    return Err(AnswerError::TwoWhole { kind, segment });
                }
                whole |= bit;
            }
        }
        Ok(RootView {
            serial: Serial(u64_at(bytes, 8)),
            tree: TreeDigest(arr(bytes, 16)),
            lines: LinesKey(arr(bytes, 48)),
            summary: &bytes[ROOT_HEAD..ROOT_HEAD + summary_len],
            entries,
        })
    }

    pub fn tree(&self) -> TreeDigest {
        self.tree
    }

    pub fn lines(&self) -> LinesKey {
        self.lines
    }

    pub fn summary(&self) -> &'a [u8] {
        self.summary
    }

    pub fn byte_len(&self) -> usize {
        ROOT_HEAD + self.summary.len() + 4 + self.entries.len()
    }

    /// Child references in lookup order, newest first.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = SegmentRef> + '_ {
        self.entries.chunks_exact(ROOT_ENTRY).map(|entry| SegmentRef {
            kind: SegmentKind::from_u8(entry[0]).expect("root kinds checked at parse"),
            len: u64_at(entry, 1),
            id: ContentId(arr(entry, 9)),
        })
    }

    /// Explicitly check the contents of every child against this root and verify its indexes.
    /// A scrub reads all segment bytes. Certificate lookup uses the directory alone.
    pub fn scrub<'s>(&'s self, fetch: impl FnMut(&SegmentRef) -> Option<&'s [u8]>, alloc: SharedAlloc<'s>) -> Result<(), AnswerError> {
        let view = AnswerView::read_segments(*self, fetch, |segment, child, bytes| {
            let found = ContentId::of(SEGMENT_DOMAIN, bytes);
            if found != child.id {
                return Err(AnswerError::SegmentDigest { segment, expected: child.id, found });
            }
            Ok(())
        }, Plant::None, alloc)?;
        view.verify()
    }
}

#[cfg(test)]
#[test]
fn root_limits_are_checked_before_following_children() {
    let refusal = |root: &[u8]| {
        AnswerView::open(root, |child| panic!("opened child {child:?} of an invalid root"), Plant::None, blaze_own::testing::heap().shared()).err()
    };
    let mut bytes = [0; ROOT_HEAD + 4 + 2 * ROOT_ENTRY];
    bytes[..4].copy_from_slice(&ROOT_MAGIC);
    bytes[4..8].copy_from_slice(&FORMAT.to_le_bytes());
    bytes[ROOT_HEAD - 4..ROOT_HEAD].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(refusal(&bytes), Some(AnswerError::Cap { what: "summary bytes", cap: MAX_SUMMARY as u64 }));
    bytes[ROOT_HEAD - 4..ROOT_HEAD].fill(0);
    bytes[ROOT_HEAD..ROOT_HEAD + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(refusal(&bytes), Some(AnswerError::Cap { what: "segments a root", cap: MAX_SEGMENTS as u64 }));

    bytes[ROOT_HEAD..ROOT_HEAD + 4].copy_from_slice(&2u32.to_le_bytes());
    bytes[ROOT_HEAD + 4] = SegmentKind::Owed as u8;
    bytes[ROOT_HEAD + 4 + ROOT_ENTRY] = SegmentKind::Owed as u8;
    assert_eq!(refusal(&bytes), Some(AnswerError::TwoWhole { kind: SegmentKind::Owed, segment: 1 }));
}

/// The root's bytes: the commit record of one answer.
pub(crate) fn encode_root<'m>(serial: Serial, tree: &TreeDigest, lines: &LinesKey, summary: &[u8], segments: &[SegmentRef], alloc: Alloc<'m>) -> AVec<'m, u8> {
    let mut out = Vec::with_capacity_in(ROOT_HEAD + summary.len() + 4 + ROOT_ENTRY * segments.len(), alloc);
    out.extend_from_slice(&ROOT_MAGIC);
    put_u32(&mut out, FORMAT);
    put_u64(&mut out, serial.0);
    out.extend_from_slice(&tree.0);
    out.extend_from_slice(&lines.0);
    put_u32(&mut out, summary.len() as u32);
    out.extend_from_slice(summary);
    put_u32(&mut out, segments.len() as u32);
    for one in segments.iter() {
        out.push(one.kind as u8);
        put_u64(&mut out, one.len);
        out.extend_from_slice(&one.id.0);
    }
    out
}

/// "Nothing changed": whether the root answers `tree`. Reads the root's head only, none of its
/// segments (design point 6; the floor's "change detection plus one certificate read").
pub fn nothing_changed(root: &[u8], tree: &TreeDigest) -> Result<bool, AnswerError> {
    Ok(RootView::parse(root)?.tree == *tree)
}

/// The closing line's summary the root holds, read from the root alone (no segment): what a
/// certificate prints when [`nothing_changed`] says the root answers today's tree.
pub fn summary_of(root: &[u8]) -> Result<&[u8], AnswerError> {
    Ok(RootView::parse(root)?.summary)
}

/// Whether the answer's segments call for compaction (rule 18: a state, never a failure).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Health {
    Fine,
    /// At least [`COMPACT_AT`] segments, or half of [`MAX_RUNS`] runs: compact before more
    /// requests reach [`AnswerError::CompactionOwed`].
    CompactionDue { segments: usize, runs: usize },
}

/// One answer, as a root and its segments name it: what the checkout kept from its last request.
/// Every lookup probes the segments newest to oldest and reads in place; nothing is decoded and
/// no index is built when it opens.
pub struct AnswerView<'a> {
    serial: Serial,
    tree: TreeDigest,
    lines: LinesKey,
    summary: &'a [u8],
    segments: Table<'a, SegmentRef>,
    /// Oldest first: id order. Each with its index in the root's list.
    dicts: Table<'a, (u32, DictView<'a>)>,
    /// Newest first, as the root lists them.
    rows: Table<'a, (u32, RowsView<'a>)>,
    cells: Table<'a, (u32, CellsView<'a>)>,
    runs: Table<'a, (u32, RunView<'a>)>,
    /// The file plans' runs and the findings' runs, newest first.
    plans: Table<'a, (u32, RunView<'a>)>,
    findings: Table<'a, (u32, RunView<'a>)>,
    /// The one owed tasks segment and the one owed cells segment, when the root lists one.
    owed: Option<(u32, OwedView<'a>)>,
    owed_cells: Option<(u32, CellsView<'a>)>,
    tasks: u32,
    cell_count: u32,
    plant: Plant,
}

impl<'a> AnswerView<'a> {
    /// The answer before any request: serial 0, no task, no cell, no segment.
    pub fn empty(plant: Plant, alloc: SharedAlloc<'a>) -> AnswerView<'a> {
        AnswerView {
            serial: Serial(0),
            tree: TreeDigest::default(),
            lines: LinesKey::default(),
            summary: &[],
            segments: Vec::new_in(alloc),
            dicts: Vec::new_in(alloc),
            rows: Vec::new_in(alloc),
            cells: Vec::new_in(alloc),
            runs: Vec::new_in(alloc),
            plans: Vec::new_in(alloc),
            findings: Vec::new_in(alloc),
            owed: None,
            owed_cells: None,
            tasks: 0,
            cell_count: 0,
            plant,
        }
    }

    /// The answer `root` names, each segment's bytes from `fetch` (a mapping of the store's
    /// segment of that id). Checks the root, each segment's head and length against the root, and
    /// every bound an accessor indexes by (rule 74: once, here); each refusal names the segment.
    pub fn open<F: FnMut(&SegmentRef) -> Option<&'a [u8]>>(root: &'a [u8], fetch: F, plant: Plant, alloc: SharedAlloc<'a>) -> Result<AnswerView<'a>, AnswerError> {
        Self::from_root(RootView::parse(root)?, fetch, plant, alloc)
    }

    /// Open store-held segments of a previously parsed root. The store supplies the bytes
    /// filed under the requested identity.
    pub fn from_root<F: FnMut(&SegmentRef) -> Option<&'a [u8]>>(parsed: RootView<'a>, fetch: F, plant: Plant, alloc: SharedAlloc<'a>) -> Result<AnswerView<'a>, AnswerError> {
        Self::read_segments(parsed, fetch, |_, _, _| Ok(()), plant, alloc)
    }

    fn read_segments(
        parsed: RootView<'a>,
        mut fetch: impl FnMut(&SegmentRef) -> Option<&'a [u8]>,
        mut check: impl FnMut(u32, &SegmentRef, &[u8]) -> Result<(), AnswerError>,
        plant: Plant,
        alloc: SharedAlloc<'a>,
    ) -> Result<AnswerView<'a>, AnswerError> {
        let mut view = AnswerView::empty(plant, alloc);
        view.serial = parsed.serial;
        view.tree = parsed.tree;
        view.lines = parsed.lines;
        view.summary = parsed.summary;
        for (i, one) in parsed.segments().enumerate() {
            let segment = i as u32;
            let Some(bytes) = fetch(&one) else { return Err(AnswerError::MissingSegment { segment, id: one.id }) };
            if bytes.len() as u64 != one.len {
                return Err(AnswerError::SegmentLength { segment, expected: one.len, found: bytes.len() as u64 });
            }
            check(segment, &one, bytes)?;
            match one.kind {
                SegmentKind::Dict => view.dicts.push((segment, DictView::open(bytes, segment)?)),
                SegmentKind::Rows => view.rows.push((segment, RowsView::open(bytes, segment)?)),
                SegmentKind::Cells => view.cells.push((segment, CellsView::open(bytes, SegmentKind::Cells, segment)?)),
                SegmentKind::Run => view.runs.push((segment, RunView::open(bytes, SegmentKind::Run, segment)?)),
                SegmentKind::Plans => view.plans.push((segment, RunView::open(bytes, SegmentKind::Plans, segment)?)),
                SegmentKind::Findings => view.findings.push((segment, RunView::open(bytes, SegmentKind::Findings, segment)?)),
                SegmentKind::Owed => view.owed = Some((segment, OwedView::open(bytes, segment)?)),
                SegmentKind::OwedCells => view.owed_cells = Some((segment, CellsView::open(bytes, SegmentKind::OwedCells, segment)?)),
            }
            view.segments.push(one);
        }
        view.dicts.reverse();
        for (segment, dict) in view.dicts.iter() {
            if dict.first_task != view.tasks {
                return Err(AnswerError::IdGap { segment: *segment, section: "tasks", expected: view.tasks, found: dict.first_task });
            }
            if dict.first_cell != view.cell_count {
                return Err(AnswerError::IdGap { segment: *segment, section: "cells", expected: view.cell_count, found: dict.first_cell });
            }
            view.tasks += dict.tasks;
            view.cell_count += dict.cells;
        }
        Ok(view)
    }

    /// Another checkout's answer for `tree` (a second worktree at the same commit, a branch
    /// switched back, a tree CI checked), its root found in the shared store by the tree's digest:
    /// The root must identify the requested tree before any child is opened.
    pub fn adopt<F: FnMut(&SegmentRef) -> Option<&'a [u8]>>(tree: &TreeDigest, root: &'a [u8], fetch: F, plant: Plant, alloc: SharedAlloc<'a>) -> Result<AnswerView<'a>, AnswerError> {
        let directory = RootView::parse(root)?;
        if directory.tree != *tree {
            return Err(AnswerError::TreeMismatch);
        }
        Self::from_root(directory, fetch, plant, alloc)
    }

    /// The request that made this answer.
    pub fn serial(&self) -> Serial {
        self.serial
    }

    pub fn tree(&self) -> TreeDigest {
        self.tree
    }

    /// The key of the record of the lines the closing request printed.
    pub fn lines(&self) -> LinesKey {
        self.lines
    }

    /// The closing line's summary, as the committing request gave it.
    pub fn summary(&self) -> &'a [u8] {
        self.summary
    }

    /// The live segments, newest first: what the next root lists after its own.
    pub fn segments(&self) -> &[SegmentRef] {
        &self.segments
    }

    /// How many task ids the dictionary has given out: ids are `0..tasks`.
    pub fn tasks(&self) -> u32 {
        self.tasks
    }

    /// How many cell numbers the dictionary has given out.
    pub fn cells(&self) -> u32 {
        self.cell_count
    }

    /// The runs of every run kind (reverse index, plans, findings): what [`MAX_RUNS`] caps.
    pub(crate) fn run_count(&self) -> usize {
        self.runs.len() + self.plans.len() + self.findings.len()
    }

    pub fn health(&self) -> Health {
        let (segments, runs) = (self.segments.len(), self.run_count());
        match segments >= COMPACT_AT || runs >= MAX_RUNS / 2 {
            true => Health::CompactionDue { segments, runs },
            false => Health::Fine,
        }
    }

    /// The runs of `kind` ([`SegmentKind::is_run`]), newest first.
    pub(crate) fn runs(&self, kind: SegmentKind) -> impl Iterator<Item = &RunView<'a>> {
        let list = match kind {
            SegmentKind::Plans => &self.plans,
            SegmentKind::Findings => &self.findings,
            _ => &self.runs,
        };
        list.iter().map(|(_, run)| run)
    }

    /// The owed tasks, ascending: the newest owed segment's.
    pub fn owed(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.owed.iter().flat_map(|(_, owed)| owed.all())
    }

    /// Whether `task` is owed: one binary search.
    pub fn is_owed(&self, task: TaskId) -> bool {
        self.owed.is_some_and(|(_, owed)| owed.contains(task))
    }

    /// The owed cells, ascending, each with the request it moved in: the newest owed cells
    /// segment's.
    pub fn owed_cells(&self) -> impl Iterator<Item = (CellNo, Serial)> + '_ {
        self.owed_cells.iter().flat_map(|(_, cells)| cells.all())
    }

    /// The tasks `file`'s plan lists, ascending: every plans run's entries for the file, summed.
    pub fn file_tasks<'m>(&self, file: TaskId, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, AnswerError> {
        summed(SegmentKind::Plans, &self.plans, file.0, alloc)
    }

    /// The tasks whose row made findings or failed, ascending: every findings run, summed.
    pub fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, AnswerError> {
        summed(SegmentKind::Findings, &self.findings, 0, alloc)
    }

    /// The id of the task `name`.
    pub fn task(&self, name: &Name) -> Option<TaskId> {
        self.dicts.iter().find_map(|(_, dict)| dict.find_task(name))
    }

    fn dict_of_task(&self, task: TaskId) -> Option<&DictView<'a>> {
        let at = self.dicts.partition_point(|(_, dict)| dict.first_task + dict.tasks <= task.0);
        self.dicts.get(at).map(|(_, dict)| dict).filter(|dict| dict.first_task <= task.0 && task.0 < dict.first_task + dict.tasks)
    }

    fn dict_of_cell(&self, cell: CellNo) -> Option<&DictView<'a>> {
        let at = self.dicts.partition_point(|(_, dict)| dict.first_cell + dict.cells <= cell.0);
        self.dicts.get(at).map(|(_, dict)| dict).filter(|dict| dict.first_cell <= cell.0 && cell.0 < dict.first_cell + dict.cells)
    }

    /// The name of `task`.
    pub fn name(&self, task: TaskId) -> Option<Name> {
        self.dict_of_task(task).map(|dict| dict.name((task.0 - dict.first_task) as usize))
    }

    /// The number of the cell `cell` of the task `definer`.
    pub fn cell_of(&self, definer: TaskId, cell: &Cell) -> Option<CellNo> {
        self.dicts.iter().find_map(|(_, dict)| dict.find_cell(definer, cell))
    }

    /// The number of the engine's `CellId { of, cell }`.
    pub fn cell(&self, of: &Name, cell: &Cell) -> Option<CellNo> {
        self.cell_of(self.task(of)?, cell)
    }

    /// The definer and the cell of `cell`.
    pub fn cell_key(&self, cell: CellNo) -> Option<(TaskId, Cell)> {
        self.dict_of_cell(cell).map(|dict| dict.cell((cell.0 - dict.first_cell) as usize))
    }

    /// The task that defines `cell`.
    pub fn definer(&self, cell: CellNo) -> Option<TaskId> {
        self.cell_key(cell).map(|(definer, _)| definer)
    }

    /// The task's row today: the newest segment that has the task decides, a tombstone meaning
    /// none.
    pub fn row(&self, task: TaskId) -> Option<RowView<'_>> {
        for (_, seg) in self.rows.iter() {
            if self.plant != Plant::TombstoneIgnored && seg.retires(task) {
                return None;
            }
            if let Some(i) = seg.find(task) {
                return Some(seg.row(i));
            }
        }
        None
    }

    /// Every row of the task still in a segment, newest first, past tombstones: older versions
    /// live until compaction.
    pub(crate) fn versions(&self, task: TaskId) -> impl Iterator<Item = RowView<'_>> {
        self.rows.iter().filter_map(move |(_, seg)| seg.find(task).map(|i| seg.row(i)))
    }

    /// The request `cell` last moved in; [`Serial`] 0 when it never did.
    pub fn moved(&self, cell: CellNo) -> Serial {
        self.cells.iter().find_map(|(_, cells)| cells.find(cell)).unwrap_or(Serial(0))
    }

    /// What `cell` holds today: its definer's row's define entry, else absent.
    pub fn digest(&self, cell: CellNo) -> CellDigest {
        self.definer(cell).and_then(|definer| self.row(definer)).and_then(|row| row.digest_of(cell)).unwrap_or(CellDigest::ABSENT)
    }

    /// The tasks whose rows read `cell`, ascending: the signed entries of every run for the
    /// cell, summed per task (at most [`MAX_RUNS`] runs, each a binary search). A sum other than
    /// 0 or 1 is [`AnswerError::ReadersSum`].
    pub fn readers<'m>(&self, cell: CellNo, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, AnswerError> {
        summed(SegmentKind::Run, &self.runs, cell.0, alloc)
    }

    /// Whether `task`'s row reads `cell`, and the fields compared: the task's signed entries under
    /// the cell in every run, summed, each run two binary searches in place (at most
    /// [`MAX_RUNS`] runs). The read list is in read order, so this is how one read is found
    /// without a walk (blaze-engine `LastAnswer::read_of`). A sum other than 0 or 1 is
    /// [`AnswerError::ReadersSum`].
    pub fn reads_cell(&self, cell: CellNo, task: TaskId) -> Result<(bool, u32), AnswerError> {
        let (mut sum, mut compared) = (0i32, 0u32);
        for (_, run) in self.runs.iter() {
            let (sign, probes) = run.sign_of(cell, task);
            sum += sign;
            compared = compared.saturating_add(probes);
        }
        match sum {
            0 => Ok((false, compared)),
            1 => Ok((true, compared)),
            _ => Err(AnswerError::ReadersSum { kind: SegmentKind::Run, cell, task, sum }),
        }
    }

    /// Whether `task`'s row stands: no cell it read moved after the row's serial, walked in read
    /// order, stopping at the first that moved. `None`: no row.
    pub fn stands(&self, task: TaskId) -> Option<bool> {
        let row = self.row(task)?;
        let serial = row.serial();
        Some(row.reads().all(|cell| self.moved(cell) <= serial))
    }

    /// Where the rows segments' bytes go, summed.
    pub fn rows_bytes(&self) -> RowsBytes {
        let mut sum = RowsBytes::default();
        for (_, seg) in self.rows.iter() {
            let one = seg.bytes();
            sum.rows += one.rows;
            sum.fixed += one.fixed;
            sum.cold += one.cold;
            sum.reads += one.reads;
            sum.read_count += one.read_count;
            sum.defines += one.defines;
            sum.define_count += one.define_count;
            sum.tombs += one.tombs;
        }
        sum
    }

    /// The order of every key (what open does not check: a wrong order reads wrong answers, never
    /// out of bounds). For segments from elsewhere and for the simulation.
    pub fn verify(&self) -> Result<(), AnswerError> {
        for (segment, dict) in self.dicts.iter() {
            dict.verify(*segment)?;
        }
        for (segment, rows) in self.rows.iter() {
            rows.verify(*segment)?;
        }
        for (segment, cells) in self.cells.iter() {
            cells.verify(*segment)?;
        }
        for (segment, run) in self.runs.iter().chain(self.plans.iter()).chain(self.findings.iter()) {
            run.verify(*segment)?;
        }
        if let Some((segment, owed)) = self.owed {
            owed.verify(segment)?;
        }
        if let Some((segment, cells)) = self.owed_cells {
            cells.verify(segment)?;
        }
        Ok(())
    }
}

/// The tasks under `key` in `runs` (of `kind`), ascending: each run's entries for the key (one
/// binary search a run), summed per task. Keep one cursor per matching run. The leaves of the
/// tournament hold their current postings, and each internal node holds its smallest child.
/// Advancing one cursor updates only its path to the root.
fn summed<'m>(kind: SegmentKind, runs: &[(u32, RunView<'_>)], key: u32, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, AnswerError> {
    // A posting uses bit 31 for its sign, so no decoded TaskId can be u32::MAX.
    const END: (u32, usize, i32) = (u32::MAX, 0, 0);
    let mut cursors = Vec::with_capacity_in(runs.len(), alloc);
    let mut tree = [END; 2 * MAX_RUNS];
    // AnswerView::open and Commit::finish cap the total number of runs at MAX_RUNS.
    let leaves = runs.len().next_power_of_two();
    for (segment, run) in runs.iter() {
        let mut cursor = run.of(CellNo(key)).enumerate();
        if let Some((_, (task, sign))) = cursor.next() {
            let slot = cursors.len();
            tree[leaves + slot] = (task.0, slot, sign);
            cursors.push((*segment, cursor));
        }
    }
    for parent in (1..leaves).rev() {
        tree[parent] = tree[2 * parent].min(tree[2 * parent + 1]);
    }
    let mut out = Vec::new_in(alloc);
    while tree[1] != END {
        let task = TaskId(tree[1].0);
        let mut sum = 0;
        while tree[1].0 == task.0 {
            let (_, slot, sign) = tree[1];
            sum += sign;
            let mut node = leaves + slot;
            let (segment, cursor) = &mut cursors[slot];
            tree[node] = match cursor.next() {
                Some((_, (next, sign))) if next > task => (next.0, slot, sign),
                Some((at, _)) => return Err(AnswerError::Unsorted { segment: *segment, section: "matched run slice", at }),
                None => END,
            };
            while node > 1 {
                node /= 2;
                tree[node] = tree[2 * node].min(tree[2 * node + 1]);
            }
        }
        match sum {
            0 => {}
            1 => out.push(task),
            _ => return Err(AnswerError::ReadersSum { kind, cell: CellNo(key), task, sum }),
        }
    }
    Ok(out)
}

#[cfg(test)]
#[test]
fn a_disordered_posting_slice_names_its_segment() {
    use crate::segment::{RunSums, encode_run};

    let alloc = blaze_own::testing::heap().alloc();
    let mut entries = Vec::from_slice_in(&[(7, 1, 1), (7, 3, 1)], alloc);
    let mut bytes = encode_run(SegmentKind::Run, &mut entries, RunSums::Request, Plant::None, Serial(1), alloc).unwrap().unwrap();
    let last = bytes.len() - 4;
    bytes[last..].copy_from_slice(&0u32.to_le_bytes());
    let run = RunView::open(&bytes, SegmentKind::Run, 5).unwrap();
    assert_eq!(
        summed(SegmentKind::Run, &[(5, run)], 7, alloc).unwrap_err(),
        AnswerError::Unsorted { segment: 5, section: "matched run slice", at: 1 }
    );
}
