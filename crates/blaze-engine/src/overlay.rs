//! This request's writes over the last answer (docs/v2/plan/design/runtime.md, "The answer's
//! stored form"; the seam is last.rs): what the engine asks of the tables, answered from what this
//! request already did first and from the last answer, through [`LastAnswer`], second.
//!
//! # The working set
//!
//! [`Work`] holds what this request wrote and nothing else: the rows it put (`Some`) or took
//! out (`None`), the cells it moved, the reverse index's entries it added (`true`) or dropped
//! (`false`), the file plans it replaced, the findings it set, and the owed tasks and cells,
//! which are small by construction and so are read whole from the last answer when the request
//! opens. Its size is the request's work, never the answer's: a request that enters `k` tasks
//! holds `k` rows, their cells and their reads.
//!
//! # The shadowing rules (rule 53)
//!
//! 1. A row this request put or took out shadows the last answer's row of that name.
//! 2. A cell this request moved shadows the last answer's digest of it; a cell it took to absent
//!    is absent.
//! 3. A cell's readers are the last answer's, less those this request dropped, plus those it
//!    added (`Plant::ReadersStale` drops the "less": the planted bug of last_sim.rs's control).
//! 4. A file's plan this request replaced or took shadows the last answer's.
//! 5. The owed tasks and cells are this request's whole sets, started from the last answer's.
//!
//! A row the last answer holds and this request did not touch is asked of the seam alone; the
//! engine never walks the last answer's rows, and the seam counts the rows it was asked for
//! ([`Touched`]).
//!
//! # The end
//!
//! [`Fold`] says how a form takes a finished request's [`Work`]: the in-memory form applies it
//! to its maps ([`Work::apply_to`]); the rows form will hand it to `NextAnswer` (lane RD8's
//! report, NOT DONE: `NextAnswer::put` carries no seen digests and nothing names the owed cells).

use core::sync::atomic::{AtomicBool, Ordering};

use blaze_answer::{CellDigest, Flags, Serial};
use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Key, Name};
use blaze_id::ContentId;
use blaze_own::{Alloc, BTreeMap, BTreeSet, SharedAlloc, Vec};

use crate::engine::{Answer, Cause, CellId, EngineError, Gen, Phase, Plant, Planned, Row, SMap, SVec};
use crate::last::{LastAnswer, LastRow, Touched};

/// A row's fixed fields as the engine asks for them, from this request's row or the last
/// answer's: what a worker's view and the engine's walk read without the row's lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowFacts {
    pub key: Key,
    pub phase: Phase,
    pub file: Name,
    pub source: ContentId,
    pub out: ContentId,
    /// The request that made it.
    pub serial: Gen,
    pub findings: u32,
    /// Made inline: no file's plan lists it.
    pub inline: bool,
    /// The task failed on its reads (engine.rs invariant 7).
    pub failed: bool,
}

impl RowFacts {
    fn of(row: &Row<'_>) -> RowFacts {
        RowFacts {
            key: row.key,
            phase: row.phase,
            file: row.file,
            source: row.source,
            out: row.out,
            serial: row.serial,
            findings: row.findings,
            inline: row.inline,
            failed: row.failed,
        }
    }

    /// The last answer's row, or `None` for one owed and never made ([`Flags::UNMADE`]).
    fn of_last(row: &LastRow) -> Option<RowFacts> {
        let has = |flag: u8| row.flags.0 & flag != 0;
        if has(Flags::UNMADE) {
            return None;
        }
        Some(RowFacts {
            key: row.key,
            phase: row.phase,
            file: row.file,
            source: row.source,
            out: row.out,
            serial: Gen(row.serial.0),
            findings: row.findings,
            inline: has(Flags::INLINE),
            failed: has(Flags::FAILED),
        })
    }

    /// The task as its row plans it again.
    pub fn planned(&self, name: Name) -> Planned {
        Planned { name, phase: self.phase, source: self.source, file: self.file }
    }
}

/// A row taken out of the tables (retired, or replaced by a result): its facts and its lists,
/// for the reverse index and the cells it defined.
pub struct Taken<'a> {
    pub facts: RowFacts,
    /// Its reads, in read order.
    pub reads: SVec<'a, CellId>,
    /// Its defined cells and their digests, by cell.
    pub defines: SVec<'a, (Cell, CellDigest)>,
}

/// What this request wrote (the module's "The working set").
pub struct Work<'a> {
    alloc: SharedAlloc<'a>,
    serial: Gen,
    rows: SMap<'a, Name, Option<Row<'a>>>,
    cells: SMap<'a, CellId, Option<(ContentId, Gen)>>,
    readers: SMap<'a, CellId, SMap<'a, Name, bool>>,
    by_file: SMap<'a, Name, Option<SVec<'a, Name>>>,
    findings: SMap<'a, Name, bool>,
    owed_tasks: SMap<'a, Name, Planned>,
    owed_cells: SMap<'a, CellId, Gen>,
}

impl<'a> Work<'a> {
    /// The allocator the working set is over.
    pub fn alloc(&self) -> SharedAlloc<'a> {
        self.alloc
    }

    /// This request's number.
    pub fn serial(&self) -> Gen {
        self.serial
    }

    /// The in-memory form's end: `answer`, the last answer this request read, becomes the next
    /// by this request's writes, table by table; the owed sets are replaced whole.
    pub fn apply_to(self, answer: &mut Answer<'a>) {
        let alloc = self.alloc;
        answer.serial = self.serial;
        for (name, row) in self.rows.into_iter() {
            match row {
                Some(row) => {
                    answer.rows.insert(name, row);
                }
                None => {
                    answer.rows.remove(&name);
                }
            }
        }
        for (id, cell) in self.cells.into_iter() {
            match cell {
                Some(cell) => {
                    answer.cells.insert(id, cell);
                }
                None => {
                    answer.cells.remove(&id);
                }
            }
        }
        for (id, delta) in self.readers.into_iter() {
            let readers = answer.readers.entry(id).or_insert_with(|| BTreeSet::new_in(alloc));
            for (reader, added) in delta.into_iter() {
                match added {
                    true => {
                        readers.insert(reader);
                    }
                    false => {
                        readers.remove(&reader);
                    }
                }
            }
            if readers.is_empty() {
                answer.readers.remove(&id);
            }
        }
        for (file, names) in self.by_file.into_iter() {
            match names {
                Some(names) => {
                    answer.by_file.insert(file, names);
                }
                None => {
                    answer.by_file.remove(&file);
                }
            }
        }
        for (name, with) in self.findings.into_iter() {
            match with {
                true => {
                    answer.with_findings.insert(name);
                }
                false => {
                    answer.with_findings.remove(&name);
                }
            }
        }
        answer.owed_tasks = self.owed_tasks;
        answer.owed_cells = self.owed_cells;
    }
}

/// How a form of the last answer takes a finished request's writes: the next answer, in what
/// form the caller keeps.
pub trait Fold<'a>: LastAnswer + Sized {
    type Out;
    fn fold(self, work: Work<'a>) -> Result<Self::Out, EngineError>;
}

impl<'a> Fold<'a> for Answer<'a> {
    type Out = Answer<'a>;

    fn fold(mut self, work: Work<'a>) -> Result<Answer<'a>, EngineError> {
        work.apply_to(&mut self);
        Ok(self)
    }
}

/// The last answer `L` under this request's [`Work`] (the module's shadowing rules). `Sync`
/// when `L` is: a phase's workers read it at once through the engine's `View`.
pub struct Overlay<'a, L> {
    last: L,
    work: Work<'a>,
    plant: Plant,
    /// A worker's view question the last answer refused (a `LastError`): the next merge stops
    /// with [`EngineError::ViewRefused`]. A view answers infallibly, so the refusal is kept here.
    refused: AtomicBool,
}

impl<'a, L: LastAnswer> Overlay<'a, L> {
    /// The request numbered `serial` over `last`: the owed tasks and cells read whole (small by
    /// construction), nothing else read.
    pub fn new_in(last: L, serial: Gen, plant: Plant, alloc: SharedAlloc<'a>) -> Result<Overlay<'a, L>, EngineError> {
        let mut owed_tasks: SMap<'a, Name, Planned> = BTreeMap::new_in(alloc);
        for planned in last.owed(Alloc::from(alloc))?.iter() {
            owed_tasks.insert(planned.name, *planned);
        }
        let mut owed_cells: SMap<'a, CellId, Gen> = BTreeMap::new_in(alloc);
        for (cell, at) in last.owed_cells(Alloc::from(alloc))?.iter() {
            let id = last.cell_id(*cell).ok_or(EngineError::LastUnnamed { what: "an owed cell", by: None })?;
            owed_cells.insert(id, Gen(at.0));
        }
        let work = Work {
            alloc,
            serial,
            rows: BTreeMap::new_in(alloc),
            cells: BTreeMap::new_in(alloc),
            readers: BTreeMap::new_in(alloc),
            by_file: BTreeMap::new_in(alloc),
            findings: BTreeMap::new_in(alloc),
            owed_tasks,
            owed_cells,
        };
        Ok(Overlay { last, work, plant, refused: AtomicBool::new(false) })
    }

    /// The last answer and this request's writes, for the form's [`Fold`].
    pub fn into_parts(self) -> (L, Work<'a>) {
        (self.last, self.work)
    }

    /// What the seam's questions cost so far.
    pub fn touched(&self) -> Touched {
        self.last.touched()
    }

    /// A worker's question was refused by the last answer.
    pub fn refuse(&self) {
        self.refused.store(true, Ordering::Relaxed);
    }

    /// Whether a worker's question was refused since the request opened.
    pub fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    fn id_of(&self, cell: L::Cell, by: Name) -> Result<CellId, EngineError> {
        self.last.cell_id(cell).ok_or(EngineError::LastUnnamed { what: "a cell", by: Some(by) })
    }

    fn name_of(&self, task: L::Task, by: Name) -> Result<Name, EngineError> {
        self.last.name(task).ok_or(EngineError::LastUnnamed { what: "a task", by: Some(by) })
    }

    /// The last answer's row of `task`, `None` for none or one owed and never made.
    fn last_facts(&self, task: L::Task) -> Result<Option<RowFacts>, EngineError> {
        Ok(self.last.row(task)?.as_ref().and_then(RowFacts::of_last))
    }

    // Rows (rule 1).

    /// `name`'s row as it stands: put this request, or the last answer's untouched.
    pub fn row(&self, name: Name) -> Result<Option<RowFacts>, EngineError> {
        if let Some(slot) = self.work.rows.get(&name) {
            return Ok(slot.as_ref().map(RowFacts::of));
        }
        match self.last.task(&name) {
            Some(task) => self.last_facts(task),
            None => Ok(None),
        }
    }

    /// `name`'s row when this request put it: a row of this request's serial.
    pub fn row_now(&self, name: Name) -> Option<&Row<'a>> {
        self.work.rows.get(&name).and_then(|slot| slot.as_ref())
    }

    /// Whether `name` is a made record no plan lists: re-validated only while still needed
    /// (engine.rs invariant 10).
    pub fn made_only(&self, name: Name) -> Result<bool, EngineError> {
        Ok(!self.work.owed_tasks.contains_key(&name) && self.row(name)?.is_some_and(|row| row.inline))
    }

    /// The cells `name`'s row defines, by cell; none for no row.
    pub fn defines(&self, name: Name) -> Result<SVec<'a, Cell>, EngineError> {
        let mut out = Vec::new_in(self.work.alloc);
        if let Some(slot) = self.work.rows.get(&name) {
            if let Some(row) = slot {
                for (cell, _) in row.defines.iter() {
                    out.push(*cell);
                }
            }
            return Ok(out);
        }
        let Some(task) = self.last.task(&name) else { return Ok(out) };
        if self.last_facts(task)?.is_none() {
            return Ok(out);
        }
        if let Some(defines) = self.last.defines(task) {
            for (cell, _) in defines {
                out.push(self.id_of(cell, name)?.cell);
            }
        }
        Ok(out)
    }

    /// Takes `name`'s row out of the tables, giving its facts and lists; `None`: no row.
    pub fn take_row(&mut self, name: Name) -> Result<Option<Taken<'a>>, EngineError> {
        let alloc = self.work.alloc;
        if let Some(slot) = self.work.rows.get_mut(&name) {
            let Some(row) = slot.take() else { return Ok(None) };
            let mut reads = Vec::with_capacity_in(row.reads.len(), alloc);
            for (id, _) in row.reads.iter() {
                reads.push(*id);
            }
            let mut defines = Vec::with_capacity_in(row.defines.len(), alloc);
            for (cell, digest) in row.defines.iter() {
                defines.push((*cell, CellDigest::of(digest)));
            }
            return Ok(Some(Taken { facts: RowFacts::of(&row), reads, defines }));
        }
        let Some(task) = self.last.task(&name) else { return Ok(None) };
        let Some(facts) = self.last_facts(task)? else { return Ok(None) };
        let mut reads = Vec::new_in(alloc);
        if let Some(cells) = self.last.reads(task) {
            for cell in cells {
                reads.push(self.id_of(cell, name)?);
            }
        }
        let mut defines = Vec::new_in(alloc);
        if let Some(cells) = self.last.defines(task) {
            for (cell, digest) in cells {
                defines.push((self.id_of(cell, name)?.cell, digest));
            }
        }
        self.work.rows.insert(name, None);
        Ok(Some(Taken { facts, reads, defines }))
    }

    /// Puts `name`'s row as this request made it.
    pub fn put_row(&mut self, name: Name, row: Row<'a>) {
        self.work.rows.insert(name, Some(row));
    }

    // Validity: what a row saw against what the cells hold today.

    /// The first read of `name`'s row, in its read order, whose cell moved since the row saw it,
    /// and how many reads were looked up to find it: the walk stops there (engine.rs invariant 9).
    /// `reverse` walks from the last read: `Plant::LaterReadFirst` alone. No row: nothing moved.
    pub fn first_moved(&self, name: Name, reverse: bool) -> Result<(Option<Cause>, u32), EngineError> {
        if let Some(slot) = self.work.rows.get(&name) {
            let Some(row) = slot else { return Ok((None, 0)) };
            let mut compared = 0u32;
            let mut moved = |(id, seen): &(CellId, ContentId)| {
                compared += 1;
                let now = self.digest(id);
                (now != *seen).then_some(Cause::ReadMoved { read: *id, was: *seen, now })
            };
            let found = match reverse {
                true => row.reads.iter().rev().find_map(&mut moved),
                false => row.reads.iter().find_map(&mut moved),
            };
            return Ok((found, compared));
        }
        let Some(task) = self.last.task(&name) else { return Ok((None, 0)) };
        let Some(facts) = self.last_facts(task)? else { return Ok((None, 0)) };
        match self.last.seen(task) {
            Some(seen) => self.walk(name, facts.serial, seen.map(|(cell, saw)| (cell, Some(saw))), reverse),
            None => match self.last.reads(task) {
                Some(reads) => self.walk(name, facts.serial, reads.map(|cell| (cell, None)), reverse),
                None => Ok((None, 0)),
            },
        }
    }

    /// The walk of a last row's reads: each `(cell, what the row saw)`, `None` when the form keeps
    /// no seen digest (the generation rule).
    fn walk<I: Iterator<Item = (L::Cell, Option<ContentId>)>>(&self, name: Name, serial: Gen, reads: I, reverse: bool) -> Result<(Option<Cause>, u32), EngineError> {
        let mut compared = 0u32;
        if reverse {
            let mut all: SVec<'a, (L::Cell, Option<ContentId>)> = Vec::new_in(self.work.alloc);
            for one in reads {
                all.push(one);
            }
            for (cell, saw) in all.iter().rev() {
                compared += 1;
                if let Some(cause) = self.moved_read(name, serial, *cell, *saw)? {
                    return Ok((Some(cause), compared));
                }
            }
            return Ok((None, compared));
        }
        for (cell, saw) in reads {
            compared += 1;
            if let Some(cause) = self.moved_read(name, serial, cell, saw)? {
                return Ok((Some(cause), compared));
            }
        }
        Ok((None, compared))
    }

    /// Whether the read of `cell` by `name`'s last row (made at `serial`) moved: by digest when
    /// the form kept what the row saw, else by generation (moved this request, or in the last
    /// answer after the row; its `was` is then the last answer's digest).
    fn moved_read(&self, name: Name, serial: Gen, cell: L::Cell, saw: Option<ContentId>) -> Result<Option<Cause>, EngineError> {
        let id = self.id_of(cell, name)?;
        let now = self.digest(&id);
        Ok(match saw {
            Some(saw) => (now != saw).then_some(Cause::ReadMoved { read: id, was: saw, now }),
            None => {
                let moved = self.work.cells.contains_key(&id) || self.last.moved(cell) > Serial(serial.0);
                moved.then(|| Cause::ReadMoved { read: id, was: self.last.content(cell), now })
            }
        })
    }

    /// Whether `reader`'s row read `id` and saw `now`: `None` for no row (engine.rs `readers_of`
    /// skips a reader that saw today's digest; `finish` settles a cell every row saw).
    pub fn saw(&self, reader: Name, id: CellId, now: ContentId) -> Result<Option<bool>, EngineError> {
        if let Some(slot) = self.work.rows.get(&reader) {
            return Ok(slot.as_ref().map(|row| row.reads.iter().any(|(read, seen)| *read == id && *seen == now)));
        }
        let Some(task) = self.last.task(&reader) else { return Ok(None) };
        if self.last_facts(task)?.is_none() {
            return Ok(None);
        }
        let Some(cell) = self.last.cell(&id) else { return Ok(Some(false)) };
        Ok(Some(match self.last.seen(task) {
            Some(mut seen) => seen.any(|(read, saw)| read == cell && saw == now),
            None => !self.work.cells.contains_key(&id) && self.last.saw(task, cell),
        }))
    }

    // Cells (rule 2).

    /// The cell's digest today ([`ABSENT`]: no task defines it).
    pub fn digest(&self, id: &CellId) -> ContentId {
        if let Some(cell) = self.work.cells.get(id) {
            return cell.map_or(ABSENT, |(digest, _)| digest);
        }
        self.last.cell(id).map_or(ABSENT, |cell| self.last.content(cell))
    }

    /// Whether some task defines the cell today.
    pub fn has_cell(&self, id: &CellId) -> bool {
        if let Some(cell) = self.work.cells.get(id) {
            return cell.is_some();
        }
        self.last.cell(id).is_some_and(|cell| self.last.content(cell) != ABSENT)
    }

    /// The cell moved this request: to `digest`, or to absent.
    pub fn set_cell(&mut self, id: CellId, digest: Option<ContentId>) {
        let serial = self.work.serial;
        self.work.cells.insert(id, digest.map(|digest| (digest, serial)));
    }

    // The reverse index (rule 3).

    /// The tasks whose rows read `id`, ascending by name.
    pub fn readers(&self, id: CellId) -> Result<SVec<'a, Name>, EngineError> {
        let alloc = self.work.alloc;
        let delta = self.work.readers.get(&id);
        let mut out: SVec<'a, Name> = Vec::new_in(alloc);
        if let Some(cell) = self.last.cell(&id) {
            for task in self.last.readers(cell, Alloc::from(alloc))?.iter() {
                let name = self.name_of(*task, id.of)?;
                let dropped = delta.is_some_and(|delta| delta.get(&name) == Some(&false));
                if !dropped || self.plant == Plant::ReadersStale {
                    out.push(name);
                }
            }
        }
        if let Some(delta) = delta {
            for (name, added) in delta.iter() {
                if *added {
                    out.push(*name);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// `reader`'s row reads `id` from now on.
    pub fn read_by(&mut self, id: CellId, reader: Name) {
        let alloc = self.work.alloc;
        self.work.readers.entry(id).or_insert_with(|| BTreeMap::new_in(alloc)).insert(reader, true);
    }

    /// `reader`'s row no longer reads `id`.
    pub fn unread_by(&mut self, id: CellId, reader: Name) {
        let alloc = self.work.alloc;
        self.work.readers.entry(id).or_insert_with(|| BTreeMap::new_in(alloc)).insert(reader, false);
    }

    // File plans (rule 4).

    /// Takes `file`'s planned tasks out, sorted by name: this request's, or the last answer's.
    pub fn take_file(&mut self, file: Name) -> Result<SVec<'a, Name>, EngineError> {
        let alloc = self.work.alloc;
        if let Some(slot) = self.work.by_file.get_mut(&file) {
            return Ok(slot.take().unwrap_or_else(|| Vec::new_in(alloc)));
        }
        let mut out: SVec<'a, Name> = Vec::new_in(alloc);
        if let Some(task) = self.last.task(&file) {
            for one in self.last.file_tasks(task, Alloc::from(alloc))?.iter() {
                out.push(self.name_of(*one, file)?);
            }
        }
        out.sort_unstable();
        self.work.by_file.insert(file, None);
        Ok(out)
    }

    /// `file`'s planned tasks from now on, sorted.
    pub fn put_file(&mut self, file: Name, names: SVec<'a, Name>) {
        self.work.by_file.insert(file, Some(names));
    }

    // Findings.

    /// Whether `name`'s row is one with findings or failed, from now on.
    pub fn set_findings(&mut self, name: Name, with: bool) {
        self.work.findings.insert(name, with);
    }

    // Owed (rule 5).

    /// `name`'s plan when it is owed.
    pub fn owed(&self, name: Name) -> Option<Planned> {
        self.work.owed_tasks.get(&name).copied()
    }

    pub fn owe(&mut self, planned: Planned) {
        self.work.owed_tasks.insert(planned.name, planned);
    }

    pub fn unowe(&mut self, name: Name) {
        self.work.owed_tasks.remove(&name);
    }

    /// Every owed task, by name.
    pub fn owed_tasks(&self) -> SVec<'a, Planned> {
        let mut out = Vec::with_capacity_in(self.work.owed_tasks.len(), self.work.alloc);
        for planned in self.work.owed_tasks.values() {
            out.push(*planned);
        }
        out
    }

    /// Every owed cell, by id.
    pub fn owed_cells(&self) -> SVec<'a, CellId> {
        let mut out = Vec::with_capacity_in(self.work.owed_cells.len(), self.work.alloc);
        for id in self.work.owed_cells.keys() {
            out.push(*id);
        }
        out
    }

    /// `id` moved at `at` while a reader of it may not have seen it.
    pub fn owe_cell(&mut self, id: CellId, at: Gen) {
        self.work.owed_cells.insert(id, at);
    }

    /// Every reader of `id` saw it.
    pub fn settle(&mut self, id: CellId) {
        self.work.owed_cells.remove(&id);
    }

    /// `Plant::OwedForgotten`: the owed cells dropped.
    pub fn forget_owed_cells(&mut self) {
        self.work.owed_cells.clear();
    }
}
