//! A request's operations to the bytes it writes: the coordinator's [`Commit`] (ids, rows,
//! tombstones, the moves they make), each worker's [`RunWriter`] (the reverse entries of the rows
//! it made), and [`compact`]. The output is [`Effects`]: segments, then the root.

use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Name};
use blaze_id::ContentId;
use blaze_own::{ABTreeMap, ABTreeSet, AVec, Alloc, BTreeMap, BTreeSet, Vec};

use crate::answer::{AnswerView, SegmentRef, encode_root};
use crate::bytes::{bits, pack, put_u32};
use crate::segment::{DictBuilder, RowHead, RowView, RowsBuilder, RunSums, RunView, SegmentKind, encode_cells, encode_run};
use crate::{AnswerError, CellDigest, CellNo, DIGEST_BYTES, Flags, INPUT, LinesKey, MAX_CELLS, MAX_READS_PER_ROW, MAX_RUNS, MAX_SEGMENTS, MAX_SUMMARY, MAX_TASKS, Plant, Serial, TaskId, TreeDigest};

/// A row's fields as the engine's `Row` gives them, less its reads and defines (passed beside it
/// as cell numbers) and its causes and findings (which are records, not the row's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowIn {
    /// The task's key: its kind's tool, its name and its source.
    pub key: Key,
    /// The digest of what it made.
    pub out: ContentId,
    /// The file whose outline lists it (its outline task's id).
    pub file: TaskId,
    /// Its phase: the engine's stage number and the wave inside it.
    pub stage: u8,
    pub wave: u16,
    pub flags: Flags,
    /// What it is planned again from when owed.
    pub source: ContentId,
    /// How many findings its record holds.
    pub findings: u32,
}

/// One write of a commit, in the order [`Effects::writes`] lists it.
pub enum Write<'m> {
    /// A new immutable segment, stored under `at.id`.
    Segment { at: SegmentRef, bytes: AVec<'m, u8> },
    /// The root: the commit itself, written after every segment it names.
    Root { bytes: AVec<'m, u8> },
}

/// What a commit asks of the store: the writes, in order. The root's write is the commit; a
/// crash before it leaves the last root whole.
pub struct Effects<'m> {
    pub serial: Serial,
    pub writes: AVec<'m, Write<'m>>,
}

impl<'m> Effects<'m> {
    pub fn writes(&self) -> &[Write<'m>] {
        &self.writes
    }

    /// The root's bytes.
    pub fn root(&self) -> &[u8] {
        self.writes
            .iter()
            .find_map(|write| match write {
                Write::Root { bytes } => Some(&bytes[..]),
                Write::Segment { .. } => None,
            })
            .expect("every effects lists its root")
    }
}

/// One request's change to the answer `last`, on the coordinator: it gives ids to new names and
/// cells, takes each task's new row or tombstone and computes the cells they move, takes the
/// workers' runs, and writes the next answer's segments and root ([`Commit::finish`]).
pub struct Commit<'v, 'a, 'm> {
    last: &'v AnswerView<'a>,
    serial: Serial,
    plant: Plant,
    alloc: Alloc<'m>,
    dict: DictBuilder<'m>,
    new_tasks: ABTreeMap<'m, Name, TaskId>,
    new_cells: ABTreeMap<'m, (TaskId, [u8; 16]), CellNo>,
    rows: RowsBuilder<'m>,
    touched: ABTreeSet<'m, TaskId>,
    moved: ABTreeMap<'m, u32, u64>,
    entries: AVec<'m, (u32, u32, i32)>,
}

impl<'v, 'a, 'm> Commit<'v, 'a, 'm> {
    /// The request after `last`: its serial is the next.
    pub fn new(last: &'v AnswerView<'a>, plant: Plant, alloc: Alloc<'m>) -> Commit<'v, 'a, 'm> {
        Commit {
            last,
            serial: Serial(last.serial().0 + 1),
            plant,
            alloc,
            dict: DictBuilder::new_in(last.tasks(), last.cells(), alloc),
            new_tasks: BTreeMap::new_in(alloc),
            new_cells: BTreeMap::new_in(alloc),
            rows: RowsBuilder::new_in(alloc),
            touched: BTreeSet::new_in(alloc),
            moved: BTreeMap::new_in(alloc),
            entries: Vec::new_in(alloc),
        }
    }

    pub fn serial(&self) -> Serial {
        self.serial
    }

    fn tasks(&self) -> u32 {
        self.last.tasks() + self.dict.tasks()
    }

    fn cells(&self) -> u32 {
        self.last.cells() + self.dict.cells()
    }

    /// The id of `name`, given now when the answer has none (ids in call order).
    pub fn task(&mut self, name: Name) -> Result<TaskId, AnswerError> {
        if let Some(task) = self.last.task(&name).or_else(|| self.new_tasks.get(&name).copied()) {
            return Ok(task);
        }
        if self.tasks() >= MAX_TASKS {
            return Err(AnswerError::Cap { what: "task ids", cap: u64::from(MAX_TASKS) });
        }
        let task = TaskId(self.tasks());
        self.dict.push_task(name);
        self.new_tasks.insert(name, task);
        Ok(task)
    }

    /// The number of the cell `cell` of `definer`, given now when the answer has none.
    pub fn cell(&mut self, definer: TaskId, cell: Cell) -> Result<CellNo, AnswerError> {
        self.known_task(definer)?;
        if let Some(no) = self.last.cell_of(definer, &cell).or_else(|| self.new_cells.get(&(definer, cell.0)).copied()) {
            return Ok(no);
        }
        if self.cells() >= MAX_CELLS {
            return Err(AnswerError::Cap { what: "cell numbers", cap: u64::from(MAX_CELLS) });
        }
        let no = CellNo(self.cells());
        self.dict.push_cell(definer, cell);
        self.new_cells.insert((definer, cell.0), no);
        Ok(no)
    }

    fn known_task(&self, task: TaskId) -> Result<(), AnswerError> {
        match task.0 < self.tasks() {
            true => Ok(()),
            false => Err(AnswerError::UnknownTask { task }),
        }
    }

    fn definer(&self, cell: CellNo) -> Result<TaskId, AnswerError> {
        if cell.0 < self.last.cells() {
            return self.last.definer(cell).ok_or(AnswerError::UnknownCell { cell });
        }
        match cell.0 < self.cells() {
            true => Ok(self.dict.definer((cell.0 - self.last.cells()) as usize)),
            false => Err(AnswerError::UnknownCell { cell }),
        }
    }

    fn give_value(&mut self, task: TaskId) -> Result<(), AnswerError> {
        self.known_task(task)?;
        match self.touched.insert(task) {
            true => Ok(()),
            false => Err(AnswerError::TwoValues { task }),
        }
    }

    /// `cell` of `task` now holds `digest`: it moves in this request.
    fn moves(&mut self, task: TaskId, cell: CellNo, digest: CellDigest) {
        let mut at = self.serial.0;
        if self.plant == Plant::GenerationByDigest {
            if let Some(older) = self.last.versions(task).find(|row| row.digest_of(cell) == Some(digest)) {
                at = older.serial().0;
            }
        }
        self.moved.insert(cell.0, at);
    }

    /// `task`'s row as this request made it: `reads` in the order it read them, `defines` in any
    /// order. Each define whose digest differs from the cell's today moves the cell, and each cell
    /// the old row defined and this one does not moves to absent.
    pub fn put_row(&mut self, task: TaskId, row: &RowIn, reads: &[CellNo], defines: &[(CellNo, CellDigest)]) -> Result<(), AnswerError> {
        self.give_value(task)?;
        self.known_task(row.file)?;
        if reads.len() > MAX_READS_PER_ROW {
            return Err(AnswerError::Cap { what: "reads a row", cap: MAX_READS_PER_ROW as u64 });
        }
        for cell in reads.iter() {
            if cell.0 >= self.cells() {
                return Err(AnswerError::UnknownCell { cell: *cell });
            }
        }
        let mut sorted: AVec<'m, (CellNo, CellDigest)> = Vec::from_slice_in(defines, self.alloc);
        sorted.sort_unstable_by_key(|(cell, _)| *cell);
        for j in 0..sorted.len() {
            let cell = sorted[j].0;
            if j > 0 && sorted[j - 1].0 == cell {
                return Err(AnswerError::DefinedTwice { task, cell });
            }
            if self.definer(cell)? != task {
                return Err(AnswerError::NotDefiner { task, cell });
            }
        }
        let last = self.last;
        let old = last.row(task);
        for (cell, digest) in sorted.iter() {
            let was = old.and_then(|row| row.digest_of(*cell)).unwrap_or(CellDigest::ABSENT);
            if was != *digest {
                self.moves(task, *cell, *digest);
            }
        }
        if let Some(old) = old {
            for (cell, was) in old.defines() {
                if was != CellDigest::ABSENT && sorted.binary_search_by_key(&cell, |(c, _)| *c).is_err() {
                    self.moves(task, cell, CellDigest::ABSENT);
                }
            }
        }
        let mut packed: AVec<'m, u8> = Vec::new_in(self.alloc);
        let (base, top) = reads.iter().fold((u32::MAX, 0u32), |(lo, hi), cell| (lo.min(cell.0), hi.max(cell.0)));
        let base = if reads.is_empty() { 0 } else { base };
        let width = if reads.is_empty() { 0 } else { bits(top - base) };
        put_u32(&mut packed, base);
        pack(&mut packed, reads.iter().map(|cell| cell.0), base, width);
        let meta = reads.len() as u32 | (width << 24);
        let mut kept: AVec<'m, (u32, [u8; DIGEST_BYTES])> = Vec::with_capacity_in(sorted.len(), self.alloc);
        for (cell, digest) in sorted.iter() {
            kept.push((cell.0, digest.0));
        }
        let head = RowHead {
            task: task.0,
            key: row.key.0.0,
            out: row.out.0,
            serial: self.serial.0,
            file: row.file.0,
            stage: row.stage,
            flags: row.flags.0,
            wave: row.wave,
            source: row.source.0,
            findings: row.findings,
        };
        self.rows.push(head, meta, &packed, &kept);
        Ok(())
    }

    /// `task` has no row from this request on: a tombstone, and every cell it defined moves to
    /// absent. Its reverse entries are its worker's ([`RunWriter::row_retired`]).
    pub fn retire(&mut self, task: TaskId) -> Result<(), AnswerError> {
        self.give_value(task)?;
        let last = self.last;
        let Some(old) = last.row(task) else { return Err(AnswerError::NoRow { task }) };
        for (cell, was) in old.defines() {
            if was != CellDigest::ABSENT {
                self.moves(task, cell, CellDigest::ABSENT);
            }
        }
        self.rows.tomb(task);
        Ok(())
    }

    /// A worker's run ([`RunWriter::finish`]): merged into this request's one run.
    pub fn add_run(&mut self, bytes: &[u8]) -> Result<(), AnswerError> {
        let run = RunView::open(bytes, INPUT)?;
        run.verify(INPUT)?;
        for (cell, task, sign) in run.all() {
            self.known_task(TaskId(task))?;
            if cell >= self.cells() {
                return Err(AnswerError::UnknownCell { cell: CellNo(cell) });
            }
            self.entries.push((cell, task, sign));
        }
        Ok(())
    }

    /// The cells this request moved so far, ascending: whose readers the engine runs again.
    pub fn moved(&self) -> impl Iterator<Item = CellNo> + '_ {
        self.moved.keys().map(|cell| CellNo(*cell))
    }

    /// The next answer: its new segments and its root over them and every live segment of the
    /// last. Past [`MAX_SEGMENTS`] segments or [`MAX_RUNS`] runs it is
    /// [`AnswerError::CompactionOwed`], and nothing is written.
    pub fn finish(mut self, tree: &TreeDigest, summary: &[u8], lines: &LinesKey) -> Result<Effects<'m>, AnswerError> {
        if summary.len() > MAX_SUMMARY {
            return Err(AnswerError::Cap { what: "summary bytes", cap: MAX_SUMMARY as u64 });
        }
        let alloc = self.alloc;
        let serial = self.serial;
        let mut segments: AVec<'m, (SegmentKind, AVec<'m, u8>)> = Vec::new_in(alloc);
        if let Some(run) = encode_run(&mut self.entries, RunSums::Request, self.plant, serial, alloc)? {
            segments.push((SegmentKind::Run, run));
        }
        if !self.moved.is_empty() {
            let mut moved: AVec<'m, (u32, u64)> = Vec::with_capacity_in(self.moved.len(), alloc);
            moved.extend(self.moved.iter().map(|(cell, at)| (*cell, *at)));
            segments.push((SegmentKind::Cells, encode_cells(&moved, serial, alloc)));
        }
        if !self.rows.is_empty() {
            segments.push((SegmentKind::Rows, self.rows.encode(serial, alloc)?));
        }
        if self.dict.tasks() > 0 || self.dict.cells() > 0 {
            segments.push((SegmentKind::Dict, self.dict.encode(serial, alloc)));
        }
        let runs = self.last.segments().iter().filter(|one| one.kind == SegmentKind::Run).count() + segments.iter().filter(|(kind, _)| *kind == SegmentKind::Run).count();
        if runs > MAX_RUNS {
            return Err(AnswerError::CompactionOwed { what: "runs", count: runs, cap: MAX_RUNS });
        }
        let count = self.last.segments().len() + segments.len();
        if count > MAX_SEGMENTS {
            return Err(AnswerError::CompactionOwed { what: "segments", count, cap: MAX_SEGMENTS });
        }
        let mut refs: AVec<'m, SegmentRef> = Vec::with_capacity_in(count, alloc);
        for (kind, bytes) in segments.iter() {
            refs.push(SegmentRef::of(*kind, bytes));
        }
        refs.extend_from_slice(self.last.segments());
        let root = encode_root(serial, tree, lines, summary, &refs, alloc);
        Ok(effects(serial, refs.len() - self.last.segments().len(), &refs, segments, root, self.plant, alloc))
    }
}

/// The writes in order: the segments, then the root (the root first under the planted bug).
fn effects<'m>(serial: Serial, fresh: usize, refs: &[SegmentRef], segments: AVec<'m, (SegmentKind, AVec<'m, u8>)>, root: AVec<'m, u8>, plant: Plant, alloc: Alloc<'m>) -> Effects<'m> {
    let mut writes: AVec<'m, Write<'m>> = Vec::with_capacity_in(fresh + 1, alloc);
    if plant == Plant::RootFirst {
        writes.push(Write::Root { bytes: root });
        for (at, (_, bytes)) in refs.iter().zip(segments) {
            writes.push(Write::Segment { at: *at, bytes });
        }
    } else {
        for (at, (_, bytes)) in refs.iter().zip(segments) {
            writes.push(Write::Segment { at: *at, bytes });
        }
        writes.push(Write::Root { bytes: root });
    }
    Effects { serial, writes }
}

/// One worker's reverse entries for the rows it made: (cell, task, +1) for a cell a row reads now
/// and did not, -1 for one it read and does not. Its bytes go to [`Commit::add_run`].
pub struct RunWriter<'m> {
    serial: Serial,
    alloc: Alloc<'m>,
    entries: AVec<'m, (u32, u32, i32)>,
}

impl<'m> RunWriter<'m> {
    /// A worker's writer for the request of `serial`.
    pub fn new(serial: Serial, alloc: Alloc<'m>) -> RunWriter<'m> {
        RunWriter { serial, alloc, entries: Vec::new_in(alloc) }
    }

    pub fn read_added(&mut self, cell: CellNo, task: TaskId) {
        self.entries.push((cell.0, task.0, 1));
    }

    pub fn read_removed(&mut self, cell: CellNo, task: TaskId) {
        self.entries.push((cell.0, task.0, -1));
    }

    fn distinct(&self, cells: impl Iterator<Item = CellNo>) -> AVec<'m, CellNo> {
        let mut out: AVec<'m, CellNo> = Vec::from_iter_in(cells, self.alloc);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// `task`'s row was `old` (as the last answer has it) and now reads `new`.
    pub fn row_reads(&mut self, task: TaskId, old: Option<RowView<'_>>, new: &[CellNo]) {
        let was = self.distinct(old.into_iter().flat_map(|row| row.reads()));
        let now = self.distinct(new.iter().copied());
        let (mut i, mut j) = (0, 0);
        while i < was.len() || j < now.len() {
            match (was.get(i), now.get(j)) {
                (Some(a), Some(b)) if a == b => {
                    i += 1;
                    j += 1;
                }
                (Some(a), Some(b)) if a < b => {
                    self.read_removed(*a, task);
                    i += 1;
                }
                (Some(a), None) => {
                    self.read_removed(*a, task);
                    i += 1;
                }
                (_, Some(b)) => {
                    self.read_added(*b, task);
                    j += 1;
                }
                (None, None) => break,
            }
        }
    }

    /// `task`'s row `old` is retired: it reads nothing.
    pub fn row_retired(&mut self, task: TaskId, old: RowView<'_>) {
        self.row_reads(task, Some(old), &[]);
    }

    /// The worker's run; `None` when it changed no reader.
    pub fn finish(mut self) -> Result<Option<AVec<'m, u8>>, AnswerError> {
        encode_run(&mut self.entries, RunSums::Request, Plant::None, self.serial, self.alloc)
    }
}

/// The answer `last` as one segment of each kind: every task's row today (read lists copied as
/// stored), every cell that ever moved, every run merged with cancelled pairs dropped. The same
/// serial, tree, summary and lines; the old segments stay readable until the caller deletes them
/// after the new root is written.
pub fn compact<'m>(last: &AnswerView<'_>, plant: Plant, alloc: Alloc<'m>) -> Result<Effects<'m>, AnswerError> {
    let serial = last.serial();
    let mut dict = DictBuilder::new_in(0, 0, alloc);
    let mut rows = RowsBuilder::new_in(alloc);
    let mut kept: AVec<'m, (u32, [u8; DIGEST_BYTES])> = Vec::new_in(alloc);
    for t in 0..last.tasks() {
        let task = TaskId(t);
        dict.push_task(last.name(task).ok_or(AnswerError::UnknownTask { task })?);
        let Some(row) = last.row(task) else { continue };
        kept.clear();
        kept.extend(row.defines().map(|(cell, digest)| (cell.0, digest.0)));
        let head = RowHead {
            task: t,
            key: row.key().0.0,
            out: row.out().0,
            serial: row.serial().0,
            file: row.file().0,
            stage: row.stage(),
            flags: row.flags().0,
            wave: row.wave(),
            source: row.source().0,
            findings: row.findings(),
        };
        let (meta, packed) = row.packed();
        rows.push(head, meta, packed, &kept);
    }
    let mut moved: AVec<'m, (u32, u64)> = Vec::new_in(alloc);
    for c in 0..last.cells() {
        let cell = CellNo(c);
        let (definer, key) = last.cell_key(cell).ok_or(AnswerError::UnknownCell { cell })?;
        dict.push_cell(definer, key);
        let at = last.moved(cell);
        if at.0 > 0 {
            moved.push((c, at.0));
        }
    }
    let mut entries: AVec<'m, (u32, u32, i32)> = Vec::new_in(alloc);
    for run in last.runs() {
        entries.extend(run.all());
    }
    let mut segments: AVec<'m, (SegmentKind, AVec<'m, u8>)> = Vec::new_in(alloc);
    if let Some(run) = encode_run(&mut entries, RunSums::Whole, plant, serial, alloc)? {
        segments.push((SegmentKind::Run, run));
    }
    if !moved.is_empty() {
        segments.push((SegmentKind::Cells, encode_cells(&moved, serial, alloc)));
    }
    if !rows.is_empty() {
        segments.push((SegmentKind::Rows, rows.encode(serial, alloc)?));
    }
    if dict.tasks() > 0 || dict.cells() > 0 {
        segments.push((SegmentKind::Dict, dict.encode(serial, alloc)));
    }
    let mut refs: AVec<'m, SegmentRef> = Vec::with_capacity_in(segments.len(), alloc);
    for (kind, bytes) in segments.iter() {
        refs.push(SegmentRef::of(*kind, bytes));
    }
    let root = encode_root(serial, &last.tree(), &last.lines(), last.summary(), &refs, alloc);
    Ok(effects(serial, refs.len(), &refs, segments, root, plant, alloc))
}
