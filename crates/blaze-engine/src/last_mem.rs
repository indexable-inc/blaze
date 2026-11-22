//! The engine's [`Answer`] behind [`LastAnswer`]. On a restart each point read borrows its
//! fields from a verified page. The finished request keeps its writes and tombstones above
//! those pages. A fresh request builds its tables in memory.
//!
//! Handles are the engine's own `Name` and `CellId`. A row of an owed task carries the plan's
//! phase, file and source and [`Flags::OWED`]; an owed task with no row is a row of the plan
//! alone with [`Flags::UNMADE`] (last.rs), as the page form's (blaze-cli `PagesAnswer`).
//! The driver counts reads where its page source opens each record.

use blaze_answer::{CellDigest, Flags, Serial, TreeDigest};
use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Key, Name};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, SharedAlloc, Vec};

use crate::engine::{Answer, CellId, Gen, Planned, Row, SMap};
use crate::last::{Found, LastAnswer, LastError, LastRow, ReadOf, Touched};
use crate::overlay::{RowFacts, Work};
use crate::pages::{Header, PagesError, StoredDefines, StoredReads, StoredRow, VerifiedPage};

/// The driver's immutable, request-local page directory. An implementation retains each
/// verified page (or its typed refusal), so a worker may borrow its bytes until the merge.
pub trait StoredPages: Sync {
    fn page(&self, name: Name) -> Result<Option<&VerifiedPage<'_>>, PagesError>;
    /// [`StoredPages::page`] asked for `ask`'s purpose: a driver that counts its asks tells them
    /// by it (blaze-cli engine_answer.rs `AskedByKind`), so a growth names its reader.
    fn page_for(&self, name: Name, ask: Ask) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        let _ = ask;
        self.page(name)
    }
    /// Fixed fields needed to plan a producer. A store may group these in compact
    /// metadata pages, independently of the rows' variable-width observations.
    fn header(&self, name: Name) -> Result<Option<Header>, PagesError> {
        Ok(self.page_for(name, Ask::Header)?.and_then(|page| Header::from_page(page, name)))
    }
    /// The root's findings column, including failed rows which have zero findings.
    fn finding(&self, name: Name) -> Option<u32>;
    /// Exhaustive inspection, used only when the caller explicitly requests a full audit.
    fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), LastError>) -> Result<(), LastError>;
    /// The first failed page lookup, including one made by a worker's infallible query.
    fn refusal(&self) -> Option<PagesError>;
}

/// Why a reader asked the stored answer for a page by name ([`StoredPages::page_for`]). Asks of
/// one name for different purposes are different reads of its page (a row's facts, one of its
/// cells, its file's plan), so a count by name alone calls them repeats.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Ask {
    /// The row's facts, reads or defines (`Answer::entry`; the delta's `previous`).
    Row,
    /// One cell of the row, by its cell ([`CellId::cell`]).
    Cell(Cell),
    /// The row's header: its facts and counts ([`StoredPages::header`]).
    Header,
    /// A file's tasks or members (`file_tasks`; the delta's file plan).
    File,
    /// A task's planned key in its file's plan (`planned_key`).
    PlannedKey,
    /// A cell's reader list, on its slot's page (pages.rs `readers_slot`: the name is the cell's).
    Readers,
    /// A changed page's witness row (the delta's page rewrite).
    Witness,
    /// A reader that did not say ([`StoredPages::page`]).
    Unsaid,
}

impl Ask {
    /// How many purposes there are, and their words, by [`Ask::purpose`].
    pub const PURPOSES: usize = 8;
    pub const WORDS: [&'static str; Ask::PURPOSES] = ["row", "cell", "header", "file", "planned key", "readers", "witness", "unsaid"];

    /// The purpose's index, the cell set aside.
    pub fn purpose(self) -> usize {
        match self {
            Ask::Row => 0,
            Ask::Cell(_) => 1,
            Ask::Header => 2,
            Ask::File => 3,
            Ask::PlannedKey => 4,
            Ask::Readers => 5,
            Ask::Witness => 6,
            Ask::Unsaid => 7,
        }
    }
}

#[derive(Clone, Copy)]
enum Entry<'r, 'a> {
    Owned(&'r Row<'a>),
    Stored(StoredRow<'r>),
}

impl<'r, 'a> Entry<'r, 'a> {
    fn reads(&self) -> MemSeen<'r> {
        match *self {
            Self::Owned(row) => MemSeen::Owned(row.reads.iter()),
            Self::Stored(row) => MemSeen::Stored(row.reads()),
        }
    }

    fn defines(&self) -> Defines<'r> {
        match *self {
            Self::Owned(row) => Defines::Owned(row.defines.iter()),
            Self::Stored(row) => Defines::Stored(row.defines()),
        }
    }
}

impl<'a> Answer<'a> {
    /// Open a request over a stored directory. The caller supplies the root's sparse
    /// findings and owed columns in the corresponding tables before handing it to the engine.
    pub fn from_pages(stored: &'a dyn StoredPages, serial: Gen, alloc: SharedAlloc<'a>) -> Self {
        let mut answer = Self::new_in(alloc);
        answer.stored = Some(stored);
        answer.serial = serial;
        answer
    }

    pub fn stored(&self) -> Option<&dyn StoredPages> {
        self.stored
    }

    /// The net changes on top of the immutable base, including row and cell tombstones.
    pub fn work(&self) -> Option<&Work<'a>> {
        self.work.as_ref()
    }

    fn page(&self, name: Name, ask: Ask) -> Result<Option<&VerifiedPage<'_>>, LastError> {
        match self.stored {
            Some(stored) => stored.page_for(name, ask).map_err(LastError::Pages),
            None => Ok(None),
        }
    }

    fn entry(&self, name: Name) -> Result<Option<Entry<'_, 'a>>, LastError> {
        if let Some(slot) = self.work.as_ref().and_then(|work| work.rows.get(&name)) {
            return Ok(slot.as_ref().map(Entry::Owned));
        }
        if let Some(row) = self.rows.get(&name) {
            return Ok(Some(Entry::Owned(row)));
        }
        Ok(self.page(name, Ask::Row)?.and_then(|page| page.row(name)).map(Entry::Stored))
    }

    fn row_header(&self, name: Name) -> Result<Option<(RowFacts, u32, u32)>, LastError> {
        let owned = |row: &Row<'_>| (
            RowFacts::of(row),
            u32::try_from(row.reads.len()).unwrap_or(u32::MAX),
            u32::try_from(row.defines.len()).unwrap_or(u32::MAX),
        );
        if let Some(slot) = self.work.as_ref().and_then(|work| work.rows.get(&name)) {
            return Ok(slot.as_ref().map(owned));
        }
        if let Some(row) = self.rows.get(&name) {
            return Ok(Some(owned(row)));
        }
        let Some(source) = self.stored else { return Ok(None) };
        Ok(source.header(name).map_err(LastError::Pages)?.map(|header| (header.facts, header.reads, header.defines)))
    }

    /// A point read after applying the request's changes. An owed plan alone has no row.
    pub fn row_facts(&self, name: Name) -> Result<Option<RowFacts>, LastError> {
        Ok(self.row_header(name)?.map(|(facts, _, _)| facts))
    }

    pub fn observations(&self, name: Name) -> Result<Option<MemSeen<'_>>, LastError> {
        Ok(self.entry(name)?.map(|entry| entry.reads()))
    }

    /// Visit the current rows once, including inserted rows and excluding retired ones.
    /// This operation explicitly requests the whole answer.
    pub fn visit_rows(&self, mut visit: impl FnMut(Name) -> Result<(), LastError>) -> Result<(), LastError> {
        let shadowed = |name: &Name| self.work.as_ref().is_some_and(|work| work.rows.contains_key(name));
        if let Some(stored) = self.stored {
            stored.visit_rows(&mut |name| {
                if !shadowed(&name) && !self.rows.contains_key(&name) {
                    visit(name)?;
                }
                Ok(())
            })?;
        }
        for &name in self.rows.keys() {
            if !shadowed(&name) {
                visit(name)?;
            }
        }
        if let Some(work) = &self.work {
            for (&name, row) in work.rows.iter() {
                if row.is_some() {
                    visit(name)?;
                }
            }
        }
        Ok(())
    }

    fn cell_entry(&self, id: &CellId) -> Result<Option<(ContentId, Gen)>, LastError> {
        if let Some(slot) = self.work.as_ref().and_then(|work| work.cells.get(id)) {
            return Ok(*slot);
        }
        if let Some(cell) = self.cells.get(id) {
            return Ok(Some(*cell));
        }
        Ok(self.page(id.of, Ask::Cell(id.cell))?.and_then(|page| page.cell(id)))
    }

    /// Presence in the cell table, including an explicitly stored absent digest.
    pub fn has_cell(&self, id: &CellId) -> Result<bool, LastError> {
        Ok(self.cell_entry(id)?.is_some())
    }

    pub fn current_owed_tasks(&self) -> &SMap<'a, Name, Planned> {
        self.work.as_ref().map_or(&self.owed_tasks, |work| &work.owed_tasks)
    }

    pub fn current_owed_cells(&self) -> &SMap<'a, CellId, Gen> {
        self.work.as_ref().map_or(&self.owed_cells, |work| &work.owed_cells)
    }

    pub fn print_rows_in<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Name, u32)>, LastError> {
        let mut rows = Vec::with_capacity_in(self.with_findings.len(), alloc);
        for &name in self.with_findings.iter() {
            let count = match self.work.as_ref().and_then(|work| work.rows.get(&name)) {
                Some(slot) => slot.as_ref().map(|row| row.findings),
                None => match self.stored {
                    Some(stored) => stored.finding(name),
                    None => self.rows.get(&name).map(|row| row.findings),
                },
            };
            let count = count.ok_or(LastError::FindingRow { task: name })?;
            rows.push((name, count));
        }
        Ok(rows)
    }
}

/// A row's reads as the seam's cells, in read order.
pub struct MemReads<'r> {
    inner: MemSeen<'r>,
}

impl Iterator for MemReads<'_> {
    type Item = CellId;

    fn next(&mut self) -> Option<CellId> {
        self.inner.next().map(|(id, _)| id)
    }
}

/// A row's defines as the seam's cells and their kept digests, by cell.
pub struct MemDefines<'r> {
    of: Name,
    inner: Defines<'r>,
}

enum Defines<'r> {
    Owned(core::slice::Iter<'r, (Cell, ContentId)>),
    Stored(StoredDefines<'r>),
}

impl Iterator for MemDefines<'_> {
    type Item = (CellId, CellDigest);

    fn next(&mut self) -> Option<(CellId, CellDigest)> {
        let (cell, digest) = match &mut self.inner {
            Defines::Owned(items) => items.next().copied(),
            Defines::Stored(items) => items.next(),
        }?;
        Some((CellId { of: self.of, cell }, CellDigest::of(&digest)))
    }
}

/// A row's reads with the digest it saw of each, in read order.
pub enum MemSeen<'r> {
    Owned(core::slice::Iter<'r, (CellId, ContentId)>),
    Stored(StoredReads<'r>),
}

impl Iterator for MemSeen<'_> {
    type Item = (CellId, ContentId);

    fn next(&mut self) -> Option<(CellId, ContentId)> {
        match self { Self::Owned(items) => items.next().copied(), Self::Stored(items) => items.next() }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self { Self::Owned(items) => items.size_hint(), Self::Stored(items) => items.size_hint() }
    }
}

impl ExactSizeIterator for MemSeen<'_> {}

impl<'a> LastAnswer for Answer<'a> {
    type Task = Name;
    type Cell = CellId;
    type Reads<'r>
        = MemReads<'r>
    where
        Self: 'r;
    type Defines<'r>
        = MemDefines<'r>
    where
        Self: 'r;
    type Seen<'r>
        = MemSeen<'r>
    where
        Self: 'r;

    fn refusal(&self) -> Option<LastError> {
        self.stored.and_then(StoredPages::refusal).map(LastError::Pages)
    }

    fn serial(&self) -> Serial {
        Serial(self.serial.0)
    }

    /// The in-memory form keeps no tree digest: it is never the certificate (the command's
    /// stored forms are).
    fn certificate(&self, _tree: &TreeDigest) -> bool {
        false
    }

    fn task(&self, name: &Name) -> Option<Name> {
        Some(*name)
    }

    fn name(&self, task: Name) -> Option<Name> {
        Some(task)
    }

    fn cell(&self, id: &CellId) -> Option<CellId> {
        Some(*id)
    }

    fn cell_id(&self, cell: CellId) -> Option<CellId> {
        Some(cell)
    }

    fn row(&self, task: Name) -> Result<Option<LastRow>, LastError> {
        let owed = self.current_owed_tasks().get(&task);
        let Some((row, reads, defines)) = self.row_header(task)? else {
            return Ok(owed.map(|planned| LastRow {
                key: Key::default(),
                out: ABSENT,
                serial: Serial(self.serial.0),
                phase: planned.phase,
                file: planned.file,
                source: planned.source,
                flags: Flags(Flags::OWED | Flags::UNMADE),
                findings: 0,
                reads: 0,
                defines: 0,
            }));
        };
        let mut flags = 0u8;
        if row.failed {
            flags |= Flags::FAILED;
        }
        if row.inline {
            flags |= Flags::INLINE;
        }
        if owed.is_some() {
            flags |= Flags::OWED;
        }
        let (phase, file, source) = owed.map_or((row.phase, row.file, row.source), |planned: &Planned| (planned.phase, planned.file, planned.source));
        Ok(Some(LastRow {
            key: row.key,
            out: row.out,
            serial: Serial(row.serial.0),
            phase,
            file,
            source,
            flags: Flags(flags),
            findings: row.findings,
            reads,
            defines,
        }))
    }

    fn reads(&self, task: Name) -> Option<MemReads<'_>> {
        self.seen(task).map(|inner| MemReads { inner })
    }

    fn defines(&self, task: Name) -> Option<MemDefines<'_>> {
        // A failed lookup is held by StoredPages::refusal and stops the merge.
        let inner = match self.entry(task).ok()? {
            Some(entry) => entry.defines(),
            None if self.current_owed_tasks().contains_key(&task) => Defines::Owned([].iter()),
            None => return None,
        };
        Some(MemDefines { of: task, inner })
    }

    fn seen(&self, task: Name) -> Option<MemSeen<'_>> {
        match self.entry(task).ok()? {
            Some(entry) => Some(entry.reads()),
            None if self.current_owed_tasks().contains_key(&task) => Some(MemSeen::Owned([].iter())),
            None => None,
        }
    }

    fn stands(&self, task: Name) -> Option<bool> {
        self.seen(task).map(|mut reads| reads.all(|(id, seen)| self.cell(&id) == seen))
    }

    /// A read list is in read order (engine.rs invariant 9): the cell is found by a binary search
    /// over the row's order by cell (`Row::read_of`), O(log reads).
    fn read_of(&self, task: Name, cell: CellId) -> Result<ReadOf, LastError> {
        let (seen, compared) = match self.entry(task)? {
            Some(Entry::Owned(row)) => row.read_of(&cell),
            Some(Entry::Stored(row)) => row.read_of(&cell),
            None => (None, 0),
        };
        Ok(ReadOf { found: seen.map_or(Found::NotRead, Found::Saw), compared })
    }

    fn digest(&self, cell: CellId) -> CellDigest {
        CellDigest::of(&self.content(cell))
    }

    fn content(&self, cell: CellId) -> ContentId {
        // Worker reads are infallible. A refused page invalidates the batch before merge.
        self.cell_entry(&cell).ok().flatten().map_or(ABSENT, |(digest, _)| digest)
    }

    fn moved(&self, cell: CellId) -> Serial {
        match self.cell_entry(&cell).ok().flatten() {
            Some((_, at)) => Serial(at.0),
            None => Serial(self.current_owed_cells().get(&cell).map_or(0, |at| at.0)),
        }
    }

    fn readers<'m>(&self, cell: CellId, alloc: Alloc<'m>) -> Result<AVec<'m, Name>, LastError> {
        let mut out = Vec::new_in(alloc);
        // A page-backed answer can also carry an eager in-memory index from an earlier fold.
        // Both describe the same base, so combine them before applying the request's delta.
        if let Some(readers) = self.readers.get(&cell) {
            out.extend(readers.iter().copied());
        }
        if let Some(stored) = self.stored {
            // On its cell's own page (pages.rs `readers_slot`), not its task's.
            if let Some(page) = stored.page_for(crate::pages::readers_slot(&cell), Ask::Readers).map_err(LastError::Pages)? {
                out.extend(page.readers(&cell));
            }
        }
        if let Some(work) = &self.work {
            let edits = &work.readers.edits;
            let from = edits.partition_point(|((id, _), _)| *id < cell);
            let to = edits.partition_point(|((id, _), _)| *id <= cell);
            let delta = &edits[from..to];
            if !delta.is_empty() {
                out.retain(|name| delta.binary_search_by_key(name, |((_, reader), _)| *reader).is_err());
                out.extend(delta.iter().filter_map(|((_, reader), added)| added.then_some(*reader)));
                out.sort_unstable();
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn file_tasks<'m>(&self, file: Name, alloc: Alloc<'m>) -> Result<AVec<'m, Name>, LastError> {
        let mut out = Vec::new_in(alloc);
        if let Some(slot) = self.work.as_ref().and_then(|work| work.by_file.get(&file)) {
            if let Some(names) = slot {
                out.extend(names.iter().copied());
            }
        } else if let Some(names) = self.by_file.get(&file) {
            out.extend(names.iter().copied());
        } else if let Some(page) = self.page(file, Ask::File)? {
            out.extend(page.file_tasks(file));
        }
        Ok(out)
    }

    fn planned_key(&self, file: Name, name: Name) -> Result<Option<Key>, LastError> {
        if let Some(slot) = self.work.as_ref().and_then(|work| work.rows.get(&name)) {
            return Ok(slot.as_ref().map(|row| row.key));
        }
        if let Some(row) = self.rows.get(&name) {
            return Ok(Some(row.key));
        }
        Ok(self.page(file, Ask::PlannedKey)?.and_then(|page| page.planned_key(file, name)))
    }

    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        Ok(Vec::from_iter_in(self.current_owed_tasks().values().copied(), alloc))
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (CellId, Serial)>, LastError> {
        Ok(Vec::from_iter_in(self.current_owed_cells().iter().map(|(id, at)| (*id, Serial(at.0))), alloc))
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Name, u32)>, LastError> {
        self.print_rows_in(alloc)
    }

    fn touched(&self) -> Touched {
        Touched::default()
    }
}
