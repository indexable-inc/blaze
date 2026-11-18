//! The seam's third form: the engine's in-memory [`Answer`] as the last answer (last.rs; rule 78:
//! one seam, each form tested equal). `Engine<Answer>` is the engine as it was before the seam:
//! every question here is the map lookup the engine made itself, and the request's writes are
//! applied to the maps at its end (overlay.rs `Work::apply_to`).
//!
//! Handles are the engine's own `Name` and `CellId`. A row of an owed task carries the plan's
//! phase, file and source and [`Flags::OWED`]; an owed task with no row is a row of the plan
//! alone with [`Flags::UNMADE`] (last.rs), as the page form's (blaze-cli `PagesAnswer`).
//! Nothing is read from storage, so nothing is counted ([`Touched`] stays zero).

use blaze_answer::{CellDigest, Flags, Serial, TreeDigest};
use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Key, Name};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};

use crate::engine::{Answer, CellId, Planned, Row};
use crate::last::{Found, LastAnswer, LastError, LastRow, ReadOf, Touched};

/// A row's reads as the seam's cells, in read order.
pub struct MemReads<'r> {
    inner: core::slice::Iter<'r, (CellId, ContentId)>,
}

impl Iterator for MemReads<'_> {
    type Item = CellId;

    fn next(&mut self) -> Option<CellId> {
        self.inner.next().map(|(id, _)| *id)
    }
}

/// A row's defines as the seam's cells and their kept digests, by cell.
pub struct MemDefines<'r> {
    of: Name,
    inner: core::slice::Iter<'r, (Cell, ContentId)>,
}

impl Iterator for MemDefines<'_> {
    type Item = (CellId, CellDigest);

    fn next(&mut self) -> Option<(CellId, CellDigest)> {
        let (cell, digest) = self.inner.next()?;
        Some((CellId { of: self.of, cell: *cell }, CellDigest::of(digest)))
    }
}

/// A row's reads with the digest it saw of each, in read order.
pub struct MemSeen<'r> {
    inner: core::slice::Iter<'r, (CellId, ContentId)>,
}

impl Iterator for MemSeen<'_> {
    type Item = (CellId, ContentId);

    fn next(&mut self) -> Option<(CellId, ContentId)> {
        self.inner.next().copied()
    }
}

/// The lists of `task`: its row's, empty for an owed task with no row, `None` for neither.
fn lists<'r, 'a, T>(answer: &'r Answer<'a>, task: &Name, of: impl FnOnce(&'r Row<'a>) -> &'r [T]) -> Option<&'r [T]> {
    match answer.rows.get(task) {
        Some(row) => Some(of(row)),
        None if answer.owed_tasks.contains_key(task) => Some(&[]),
        None => None,
    }
}

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
        let owed = self.owed_tasks.get(&task);
        let Some(row) = self.rows.get(&task) else {
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
            reads: u32::try_from(row.reads.len()).unwrap_or(u32::MAX),
            defines: u32::try_from(row.defines.len()).unwrap_or(u32::MAX),
        }))
    }

    fn reads(&self, task: Name) -> Option<MemReads<'_>> {
        lists(self, &task, |row| &row.reads[..]).map(|reads| MemReads { inner: reads.iter() })
    }

    fn defines(&self, task: Name) -> Option<MemDefines<'_>> {
        lists(self, &task, |row| &row.defines[..]).map(|defines| MemDefines { of: task, inner: defines.iter() })
    }

    fn seen(&self, task: Name) -> Option<MemSeen<'_>> {
        lists(self, &task, |row| &row.reads[..]).map(|reads| MemSeen { inner: reads.iter() })
    }

    fn stands(&self, task: Name) -> Option<bool> {
        match self.rows.get(&task) {
            Some(row) => Some(row.reads.iter().all(|(id, seen)| self.cell(id) == *seen)),
            None => self.owed_tasks.contains_key(&task).then_some(true),
        }
    }

    /// A read list is in read order (engine.rs invariant 9): the cell is found by a binary search
    /// over the row's order by cell (`Row::read_of`), O(log reads).
    fn read_of(&self, task: Name, cell: CellId) -> Result<ReadOf, LastError> {
        Ok(match self.rows.get(&task) {
            Some(row) => {
                let (seen, compared) = row.read_of(&cell);
                ReadOf { found: seen.map_or(Found::NotRead, Found::Saw), compared }
            }
            None => ReadOf { found: Found::NotRead, compared: 0 },
        })
    }

    fn digest(&self, cell: CellId) -> CellDigest {
        CellDigest::of(&Answer::cell(self, &cell))
    }

    fn content(&self, cell: CellId) -> ContentId {
        Answer::cell(self, &cell)
    }

    fn moved(&self, cell: CellId) -> Serial {
        match self.cells.get(&cell) {
            Some((_, at)) => Serial(at.0),
            None => Serial(self.owed_cells.get(&cell).map_or(0, |at| at.0)),
        }
    }

    fn readers<'m>(&self, cell: CellId, alloc: Alloc<'m>) -> Result<AVec<'m, Name>, LastError> {
        let mut out = Vec::new_in(alloc);
        if let Some(readers) = self.readers.get(&cell) {
            for reader in readers.iter() {
                out.push(*reader);
            }
        }
        Ok(out)
    }

    fn file_tasks<'m>(&self, file: Name, alloc: Alloc<'m>) -> Result<AVec<'m, Name>, LastError> {
        let mut out = Vec::new_in(alloc);
        if let Some(names) = self.by_file.get(&file) {
            for name in names.iter() {
                out.push(*name);
            }
        }
        Ok(out)
    }

    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        let mut out = Vec::with_capacity_in(self.owed_tasks.len(), alloc);
        for planned in self.owed_tasks.values() {
            out.push(*planned);
        }
        Ok(out)
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (CellId, Serial)>, LastError> {
        let mut out = Vec::with_capacity_in(self.owed_cells.len(), alloc);
        for (id, at) in self.owed_cells.iter() {
            out.push((*id, Serial(at.0)));
        }
        Ok(out)
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Name, u32)>, LastError> {
        let mut out = Vec::with_capacity_in(self.with_findings.len(), alloc);
        for name in self.with_findings.iter() {
            out.push((*name, self.rows.get(name).map_or(0, |row| row.findings)));
        }
        Ok(out)
    }

    fn touched(&self) -> Touched {
        Touched::default()
    }
}
