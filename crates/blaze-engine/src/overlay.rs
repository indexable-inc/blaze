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
//!    This request's writes to the reverse index are the engine's while it runs (readers.rs, the
//!    strategy seam, applied in engine.rs `Engine::readers_into`): the overlay answers the last
//!    answer's half ([`Overlay::last_readers_into`]) and holds the writes once the request ends
//!    ([`Work::readers`], one sorted column).
//! 4. A file's plan this request replaced or took shadows the last answer's.
//! 5. The owed tasks and cells are this request's whole sets, started from the last answer's.
//!
//! A row the last answer holds and this request did not touch is asked of the seam alone; the
//! engine never walks the last answer's rows, and the seam counts the rows it was asked for
//! ([`Touched`]).
//!
//! # Generations (rule 53: the rule, stated once)
//!
//! A form that keeps no digest a row saw (the rows form; last_gen_sim.rs's `GenLast`) judges a
//! row of the last answer by generation, and this is the whole rule:
//!
//! **A row stands iff every cell it read has a moved-at no later than the row's serial. A cell's
//! moved-at becomes the request's serial `s` only when `s` ends with another digest in it than
//! `s` began with, or when a row `s` put read it and saw another digest than `s` ends with; a
//! cell re-made to equal bytes keeps its moved-at. A row `s` put gets serial `s` when every read
//! it holds saw the digest `s` ends with, else `s - 1`.**
//!
//! Inside a request no finer serial is needed: a row this request put is judged by the digests
//! it saw, which [`Work`] keeps until the end, and a row of the last answer was made before
//! anything of this request, so one of its reads moved iff the cell moved after the row in the
//! last answer, or this request left another digest in it than the last answer held
//! ([`Overlay::moved_now`]: a cell this request re-made to equal bytes did not move). At the end
//! [`Work::generations`] turns the kept digests into the rule's serials: a row put this request
//! that saw a digest the request then moved on is already stale, and `s - 1` keeps it so (its
//! stale read's cell moved at `s`, by the second clause). The cost of the rule against digests:
//! a cell that moved and moved back re-enters a reader that saw the first digest, whose result
//! then comes out identical (last_gen_sim.rs counts them).
//!
//! # Stale counts (plan row B1)
//!
//! A cell is owed until every reader's row saw what it holds (engine.rs invariant 5). The
//! overlay keeps, for each cell the engine visited (`Engine::readers_of`: each owed cell of the
//! last answer when the request opens, and each cell after it moves), the number of its readers
//! whose row did not see what it holds ([`Overlay::saw`] `== Some(false)`). The invariant, from
//! a visit to the request's end: **a counted cell's count is the number of its readers, under
//! shadowing rule 3, whose row read it and saw another digest than it holds now.** It holds
//! because:
//! - the visit sets it so, asking each reader (`count_stale`);
//! - a cell's digest changes only by [`Overlay::set_cell`], and the engine visits the cell after
//!   each (`move_cell`), setting the count again;
//! - a reader's freshness for a cell depends only on the reader's row and that cell's digest
//!   (by digest: the seen digest against today's; by generation: the cell's moved-at and
//!   [`Overlay::moved_now`] of that cell alone), and a row changes only by [`Overlay::take_row`]
//!   (its stale reads of counted cells leave the counts) and [`Overlay::put_row`] (its stale
//!   reads join them), the reverse index changing with them (the engine's index writes).
//!
//! So `Engine::finish` settles a cell when its count is 0 and asks no reader again: what it once
//! asked of every reader of every owed cell, each a walk of the reader's read list, is a lookup
//! a cell. A count below zero, or an owed cell never counted, is [`EngineError::StaleCount`].
//! `Plant::StaleKeptOnTake` drops the first bullet of `take_row`; finish_scale_sim.rs catches it.
//!
//! # The end
//!
//! [`Fold`] says how a form takes a finished request's [`Work`]: the in-memory form applies it
//! to its maps ([`Work::apply_to`]); a form by generation takes [`Work::generations`] with it
//! (last_gen_sim.rs `GenLast`, blaze-cli's `RowsAnswer` through `NextAnswer`).

use core::sync::atomic::{AtomicBool, Ordering};

use blaze_answer::{CellDigest, Flags, Serial};
use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::{Alloc, BTreeMap, BTreeSet, SharedAlloc, Vec};

use crate::cell_table::CellTable;
use crate::engine::{Answer, Cause, CellId, EngineError, Gen, Phase, Plant, Planned, Row, SMap, SSet, SVec, Stage};
use crate::last::{Found, LastAnswer, LastError, LastRow, Touched};
use crate::readers::ReaderEdits;

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
    pub(crate) fn of(row: &Row<'_>) -> RowFacts {
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

/// A membership is undefined iff both its cell and its defining row are absent.
/// Ordinary failures are indexed by reader so removing an example promotes the next
/// one in the driver's (reader, observation order). Signature failures need only a count.
pub struct ReadAudit<'a> {
    missing: SSet<'a, (Name, CellId)>,
    signatures: u64,
    heads: usize,
    examples: [Option<(Name, CellId)>; 8],
    examples_changed: bool,
}

/// Fields for the answer writer's verified record. The base is the answer root it
/// describes. All ordinary failures are needed to promote examples after a later edit.
pub struct ReadAuditRecord<M> {
    pub base: Key,
    pub heads: u64,
    pub signatures: u64,
    /// Strictly ascending (reader, cell).
    pub missing: M,
}

impl<'a> ReadAudit<'a> {
    pub(crate) fn from_pages(answer: &Answer<'_>, alloc: SharedAlloc<'a>) -> Result<Self, LastError> {
        let mut audit = Self { missing: BTreeSet::new_in(alloc), signatures: 0, heads: 0, examples: [None; 8], examples_changed: false };
        answer.visit_rows(|reader| {
            if answer.row_facts(reader)?.is_some_and(|row| row.phase.stage == Stage::Heads) {
                audit.heads += 1;
            }
            for (cell, _) in answer.observations(reader)?.into_iter().flatten() {
                if answer.row_facts(cell.of)?.is_none() && !answer.has_cell(&cell)? {
                    audit.membership(reader, cell, true);
                }
            }
            Ok(())
        })?;
        audit.refresh_examples(answer);
        Ok(audit)
    }

    pub fn record(&self, base: Key) -> ReadAuditRecord<impl Iterator<Item = (Name, CellId)> + '_> {
        ReadAuditRecord { base, heads: self.heads as u64, signatures: self.signatures, missing: self.missing.iter().copied() }
    }

    pub(crate) fn from_answer(answer: &Answer<'_>, alloc: SharedAlloc<'a>) -> Self {
        let heads = answer.rows.values().filter(|row| row.phase.stage == Stage::Heads).count();
        let mut audit = Self { missing: BTreeSet::new_in(alloc), signatures: 0, heads, examples: [None; 8], examples_changed: false };
        // A decoded page has already established its reverse index. Classify a selector
        // once for all its readers, walking individual edges only for ordinary failures.
        for (&id, readers) in answer.readers.iter() {
            if answer.rows.contains_key(&id.of) || answer.cells.contains_key(&id) {
                continue;
            }
            if id.of.kind == Kind::Signature {
                audit.signatures += readers.len() as u64;
            } else {
                for &reader in readers.iter() {
                    audit.membership(reader, id, true);
                }
            }
        }
        audit.refresh_examples(answer);
        audit
    }

    pub fn heads(&self) -> u32 {
        u32::try_from(self.heads).unwrap_or(u32::MAX)
    }

    /// Missing memberships, excluding signatures, followed by missing signature memberships.
    pub fn undefined_counts(&self) -> (u32, u32) {
        (u32::try_from(self.missing.len()).unwrap_or(u32::MAX), u32::try_from(self.signatures).unwrap_or(u32::MAX))
    }

    /// The first eight ordinary missing memberships in the exhaustive audit's order.
    pub fn examples(&self) -> impl Iterator<Item = (Name, CellId)> + '_ {
        self.examples.iter().flatten().copied()
    }

    fn refresh_examples(&mut self, answer: &Answer<'_>) {
        if !self.examples_changed {
            return;
        }
        self.examples = [None; 8];
        let mut slots = self.examples.iter_mut();
        let mut previous = None;
        // The first eight pairs by reader cover all readers that can supply an example.
        // Within each selected row the task's observation order breaks ties.
        for &(reader, _) in self.missing.iter().take(8) {
            if previous == Some(reader) {
                continue;
            }
            previous = Some(reader);
            for (id, _) in answer.seen(reader).into_iter().flatten() {
                if self.missing.contains(&(reader, id)) {
                    let Some(slot) = slots.next() else { break };
                    *slot = Some((reader, id));
                    if slots.len() == 0 {
                        break;
                    }
                }
            }
            if slots.len() == 0 {
                break;
            }
        }
        self.examples_changed = false;
    }

    fn membership(&mut self, reader: Name, id: CellId, missing: bool) {
        if id.of.kind == Kind::Signature {
            if missing {
                self.signatures += 1;
            } else {
                self.signatures -= 1;
            }
        } else {
            let changed = if missing { self.missing.insert((reader, id)) } else { self.missing.remove(&(reader, id)) };
            // A failure after all eight reported readers cannot change their examples.
            self.examples_changed |= changed && self.examples[7].is_none_or(|(last, _)| reader <= last);
        }
    }

    /// First reclassify standing memberships whose definer or cell changed presence.
    /// Then the net membership delta is valued entirely at the final availability.
    fn advance(&mut self, before: &Answer<'_>, work: &Work<'_>) {
        let row_exists = |name: &Name| work.rows.get(name).map_or_else(|| before.rows.contains_key(name), Option::is_some);
        let has_cell = |id: &CellId| work.cells.get(id).map_or_else(|| before.cells.contains_key(id), Option::is_some);
        self.examples_changed |= self.examples.iter().flatten().any(|(reader, _)| work.rows.contains_key(reader));
        for (&name, row) in work.rows.iter() {
            let old = before.rows.get(&name);
            self.heads -= usize::from(old.is_some_and(|r| r.phase.stage == Stage::Heads));
            self.heads += usize::from(row.as_ref().is_some_and(|r| r.phase.stage == Stage::Heads));
            if old.is_some() == row.is_some() || work.plant == Plant::SkipDefinerReaders {
                continue;
            }
            let first = CellId { of: name, cell: Cell([0; 16]) };
            let last = CellId { of: name, cell: Cell([u8::MAX; 16]) };
            for (id, readers) in before.readers.range(first..=last) {
                let was_missing = old.is_none() && !before.cells.contains_key(id);
                let missing = row.is_none() && !has_cell(id);
                if missing != was_missing {
                    for &reader in readers.iter() {
                        self.membership(reader, *id, missing);
                    }
                }
            }
        }
        for (id, cell) in work.cells.iter() {
            // A row presence transition covered every selector above. If the row stood
            // throughout, even a selector it never defined is valid.
            if row_exists(&id.of) || before.rows.contains_key(&id.of) {
                continue;
            }
            if cell.is_some() != before.cells.contains_key(id)
                && let Some(readers) = before.readers.get(id)
            {
                for &reader in readers.iter() {
                    self.membership(reader, *id, cell.is_none());
                }
            }
        }
        for (id, edits) in work.readers.iter() {
            if !row_exists(&id.of) && !has_cell(id) {
                for (&reader, &added) in edits.iter() {
                    self.membership(reader, *id, added);
                }
            }
        }
    }
}

impl<'a> Answer<'a> {
    /// Retain writes above immutable pages. If the caller drives a second request from
    /// the folded view before saving it, keep both requests relative to the same root.
    fn keep_work(&mut self, mut next: Work<'a>) {
        self.serial = next.serial;
        for (&name, &with) in next.findings.iter() {
            if with {
                self.with_findings.insert(name);
            } else {
                self.with_findings.remove(&name);
            }
        }
        let Some(mut prior) = self.work.take() else {
            self.work = Some(next);
            return;
        };
        prior.serial = next.serial;
        prior.rows.append(&mut next.rows);
        prior.by_file.append(&mut next.by_file);
        prior.findings.append(&mut next.findings);
        prior.owed_tasks = next.owed_tasks;
        prior.owed_cells = next.owed_cells;
        for (&id, &cell) in next.cells.iter() {
            prior.cells.insert(id, cell);
        }
        prior.cells.finish_order();
        // Keep the newest membership for an edge across requests. Each column is
        // sorted and contains each (cell,reader) at most once.
        let mut edges = ReaderEdits::new_in(next.alloc);
        let mut left = prior.readers.edits.iter().copied().peekable();
        let mut right = next.readers.edits.iter().copied().peekable();
        while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
            match a.0.cmp(&b.0) {
                core::cmp::Ordering::Less => edges.edits.extend(left.next()),
                core::cmp::Ordering::Greater => edges.edits.extend(right.next()),
                core::cmp::Ordering::Equal => {
                    left.next();
                    edges.edits.extend(right.next());
                }
            }
        }
        edges.edits.extend(left);
        edges.edits.extend(right);
        prior.readers = edges;
        self.work = Some(prior);
    }

    /// Attach the proof from the writer's authenticated record to the decoded answer
    /// whose root is `base`. A materialized answer supplies its row count directly.
    /// An explicit restore on a page view counts its rows to validate the proof's bounds.
    pub fn restore_read_audit(
        &mut self,
        base: Key,
        record: ReadAuditRecord<impl IntoIterator<Item = (Name, CellId)>>,
        alloc: SharedAlloc<'a>,
    ) -> Result<(), EngineError> {
        if record.base != base {
            return Err(EngineError::AuditBase { expected: base, recorded: record.base });
        }
        let rows = if self.stored.is_some() {
            let mut count = 0u64;
            self.visit_rows(|_| { count += 1; Ok(()) })?;
            count
        } else {
            self.rows.len() as u64
        };
        let edges = rows.saturating_mul(crate::READS_CAP as u64);
        for (field, count, limit) in [("heads", record.heads, rows), ("undefined signature reads", record.signatures, edges)] {
            if count > limit {
                return Err(EngineError::AuditCount { base, field, count, limit });
            }
        }
        let mut audit = ReadAudit {
            heads: record.heads as usize, signatures: record.signatures,
            missing: BTreeSet::new_in(alloc), examples: [None; 8], examples_changed: false,
        };
        let mut last = None;
        for (reader, id) in record.missing {
            if last.is_some_and(|pair| pair >= (reader, id))
                || id.of.kind == Kind::Signature || self.row_facts(id.of)?.is_some() || self.has_cell(&id)?
                || !matches!(self.read_of(reader, id)?.found, Found::Saw(_))
            {
                return Err(EngineError::AuditWitness { base, reader, cell: id });
            }
            last = Some((reader, id));
            audit.membership(reader, id, true);
        }
        audit.refresh_examples(self);
        if let Some(error) = self.refusal() {
            return Err(error.into());
        }
        self.read_audit = Some(Ok(audit));
        Ok(())
    }
}

/// What this request wrote (the module's "The working set"). Its tables are open to a form's
/// [`Fold`], which reads them once at the end; nothing else writes them but the overlay.
pub struct Work<'a> {
    alloc: SharedAlloc<'a>,
    serial: Gen,
    /// The bug the simulation plants (overlay.rs and [`Work::generations`] read it).
    plant: Plant,
    /// The rows this request put (`Some`) or took out (`None`), by name.
    pub rows: SMap<'a, Name, Option<Row<'a>>>,
    /// The cells this request moved: their digest and this request's serial, or `None`: absent.
    pub cells: CellTable<'a, Option<(ContentId, Gen)>>,
    /// The net additions and removals since the base, sorted by cell then reader.
    /// Filled by the engine at finish. Changes reversed in a later wave are absent.
    pub readers: ReaderEdits<'a>,
    /// The file plans this request replaced (`Some`, sorted) or took (`None`).
    pub by_file: SMap<'a, Name, Option<SVec<'a, Name>>>,
    /// Whether each row this request touched is one with findings or failed, from now on.
    pub findings: SMap<'a, Name, bool>,
    /// The owed tasks, whole: the last answer's under this request's changes.
    pub owed_tasks: SMap<'a, Name, Planned>,
    /// The owed cells, whole, with the request each moved in.
    pub owed_cells: SMap<'a, CellId, Gen>,
}

/// The generation rule at a request's end (the module's "Generations"): the serial each row this
/// request put stands from, and the cells whose moved-at becomes this request's serial. A form
/// that keeps no seen digests takes both from here, so the rule has one home (rule 30).
pub struct Generations<'w, 'a> {
    work: &'w Work<'a>,
    /// The cells a row this request put read and saw another digest of than the request ends with.
    stale: SSet<'a, CellId>,
}

impl<'a> Generations<'_, 'a> {
    /// The serial `row`, put this request, stands from: this request's when every read it holds
    /// saw the digest the request ends with, else the one before (it is stale already).
    pub fn row(&self, row: &Row<'_>) -> Gen {
        let serial = self.work.serial;
        match row.reads.iter().any(|(id, seen)| self.work.ends_other(id, seen)) {
            true => Gen(serial.0.saturating_sub(1)),
            false => serial,
        }
    }

    /// The cells whose moved-at becomes this request's serial, ascending: each this request
    /// touched that ends with another digest than `before` gives (the last answer's), or that a
    /// row put this request saw another digest of. `Plant::GenIdenticalMoves`: every cell touched
    /// and every cell a row put this request defines, equal bytes or not.
    pub fn moved(&self, before: impl Fn(&CellId) -> CellDigest) -> SVec<'a, CellId> {
        let planted = self.work.plant == Plant::GenIdenticalMoves;
        let mut out: SVec<'a, CellId> = Vec::new_in(self.work.alloc);
        for (id, now) in self.work.cells.iter() {
            let end = CellDigest::of(&now.map_or(ABSENT, |(digest, _)| digest));
            if planted || end != before(id) || self.stale.contains(id) {
                out.push(*id);
            }
        }
        if planted {
            for (name, row) in self.work.rows.iter() {
                if let Some(row) = row {
                    for (cell, _) in row.defines.iter() {
                        out.push(CellId { of: *name, cell: *cell });
                    }
                }
            }
            out.sort_unstable();
            out.dedup();
        }
        out
    }
}

impl<'a> Work<'a> {
    /// The generation rule's serials for this request's writes (the module's "Generations").
    pub fn generations(&self) -> Generations<'_, 'a> {
        let mut stale: SSet<'a, CellId> = BTreeSet::new_in(self.alloc);
        for row in self.rows.values().flatten() {
            for (id, seen) in row.reads.iter() {
                if self.ends_other(id, seen) {
                    stale.insert(*id);
                }
            }
        }
        Generations { work: self, stale }
    }

    /// Whether this request moved `id` and ends with another digest in it than `seen`. A cell
    /// this request did not touch holds what the last answer held, which is what any row of this
    /// request saw of it.
    fn ends_other(&self, id: &CellId, seen: &ContentId) -> bool {
        self.cells.get(id).is_some_and(|now| now.map_or(ABSENT, |(digest, _)| digest) != *seen)
    }

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
        // A cold request starts with the empty proof. Legacy page readers populate
        // the public tables directly and can certify that base once decoding is complete.
        if answer.rows.is_empty() && answer.readers.is_empty() {
            answer.certify_reads(alloc);
        }
        let mut read_audit = answer.read_audit.take();
        if let Some(Ok(audit)) = read_audit.as_mut() {
            audit.advance(answer, &self);
        }
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
        for (&id, &cell) in self.cells.iter() {
            match cell {
                Some(cell) => {
                    answer.cells.insert(id, cell);
                }
                None => {
                    answer.cells.remove(&id);
                }
            }
        }
        drop(self.cells);
        for (id, delta) in self.readers.iter() {
            let readers = answer.readers.entry(*id).or_insert_with(|| BTreeSet::new_in(alloc));
            for (reader, added) in delta.iter() {
                match *added {
                    true => {
                        readers.insert(*reader);
                    }
                    false => {
                        readers.remove(reader);
                    }
                }
            }
            if readers.is_empty() {
                answer.readers.remove(id);
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
        if let Some(Ok(audit)) = read_audit.as_mut() {
            audit.refresh_examples(answer);
        }
        answer.read_audit = read_audit;
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
        if self.stored.is_some() {
            let audited = self.read_audit.take().transpose()?.is_some();
            let alloc = work.alloc;
            self.keep_work(work);
            if audited {
                self.read_audit = Some(Ok(ReadAudit::from_pages(&self, alloc)?));
            }
            return Ok(self);
        }
        work.apply_to(&mut self);
        Ok(self)
    }
}

/// The walk `Plant::LinearSaw` restores: what the first read matching `is` holds, and the reads
/// compared to reach it (all of them when none matches).
fn walked<C, T>(reads: impl Iterator<Item = (C, T)>, mut is: impl FnMut(&C) -> bool) -> (Option<T>, u32) {
    let mut compared = 0u32;
    for (read, what) in reads {
        compared = compared.saturating_add(1);
        if is(&read) {
            return (Some(what), compared);
        }
    }
    (None, compared)
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
    /// Each visited cell's stale readers (the module's "Stale counts").
    stale: SMap<'a, CellId, u32>,
    /// Only these readers can have reversed a membership change in this request.
    replaced_again: SSet<'a, Name>,
    /// The `saw` questions `take_row` asked and the reads they compared, for the engine's report.
    saw_calls: u64,
    compared: u64,
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
            plant,
            rows: BTreeMap::new_in(alloc),
            cells: CellTable::new_in(alloc),
            readers: ReaderEdits::new_in(alloc),
            by_file: BTreeMap::new_in(alloc),
            findings: BTreeMap::new_in(alloc),
            owed_tasks,
            owed_cells,
        };
        Ok(Overlay {
            last, work, plant, refused: AtomicBool::new(false), stale: BTreeMap::new_in(alloc),
            replaced_again: BTreeSet::new_in(alloc), saw_calls: 0, compared: 0,
        })
    }

    /// The last answer and this request's writes, for the form's [`Fold`].
    pub fn into_parts(mut self) -> (L, Work<'a>) {
        self.work.cells.finish_order();
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

    pub(crate) fn check_last(&self) -> Result<(), EngineError> {
        self.last.refusal().map_or(Ok(()), |error| Err(error.into()))
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

    /// The rows `name`'s row read a cell of, each once, sorted by name, into `out` (cleared
    /// first); none for no row. The ready driver's same-stage finality walks these (ready.rs R1).
    pub fn producers_into(&self, name: Name, out: &mut SVec<'a, Name>) -> Result<(), EngineError> {
        out.clear();
        if let Some(slot) = self.work.rows.get(&name) {
            if let Some(row) = slot {
                for (id, _) in row.reads.iter() {
                    out.push(id.of);
                }
            }
        } else if let Some(task) = self.last.task(&name)
            && self.last_facts(task)?.is_some()
            && let Some(cells) = self.last.reads(task)
        {
            for cell in cells {
                out.push(self.id_of(cell, name)?.of);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(())
    }

    /// Takes `name`'s row out of the tables, giving its facts and lists; `None`: no row.
    pub fn take_row(&mut self, name: Name) -> Result<Option<Taken<'a>>, EngineError> {
        let alloc = self.work.alloc;
        if let Some(slot) = self.work.rows.get_mut(&name) {
            self.replaced_again.insert(name);
            let Some(row) = slot.take() else { return Ok(None) };
            let mut reads = Vec::with_capacity_in(row.reads.len(), alloc);
            for (id, seen) in row.reads.iter() {
                reads.push(*id);
                // Its stale reads of counted cells leave the counts (the module's "Stale counts").
                if self.stale.contains_key(id) && *seen != self.digest(id) {
                    self.unstale(*id)?;
                }
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
        // By selector, as the engine's merge compares them (engine.rs `apply`): the last
        // answer lists a task's definitions by dense cell id, and a selector interned later can
        // sort before an older one while holding the larger id (lane A1).
        defines.sort_unstable_by_key(|(cell, _)| *cell);
        // Its stale reads of counted cells leave the counts, asked while the row is still the
        // last answer's (the module's "Stale counts").
        for id in reads.iter() {
            if !self.stale.contains_key(id) {
                continue;
            }
            let (saw, compared) = self.saw(name, *id, self.digest(id))?;
            self.saw_calls += 1;
            self.compared += u64::from(compared);
            if saw == Some(false) {
                self.unstale(*id)?;
            }
        }
        self.work.rows.insert(name, None);
        Ok(Some(Taken { facts, reads, defines }))
    }

    /// Puts `name`'s row as this request made it; its stale reads of counted cells join the
    /// counts (the module's "Stale counts").
    pub fn put_row(&mut self, name: Name, row: Row<'a>) {
        for (id, seen) in row.reads.iter() {
            if self.stale.contains_key(id) && *seen != self.digest(id)
                && let Some(count) = self.stale.get_mut(id)
            {
                *count += 1;
            }
        }
        self.work.rows.insert(name, Some(row));
    }

    /// `id`'s stale readers from this visit on: `Engine::readers_of` asked each.
    pub fn count_stale(&mut self, id: CellId, stale: u32) {
        self.stale.insert(id, stale);
    }

    /// `id`'s stale readers, `None` when it was never visited.
    pub fn stale_of(&self, id: &CellId) -> Option<u32> {
        self.stale.get(id).copied()
    }

    /// A stale reader of the counted cell `id` left it. `Plant::StaleKeptOnTake`: it stays.
    fn unstale(&mut self, id: CellId) -> Result<(), EngineError> {
        if self.plant == Plant::StaleKeptOnTake {
            return Ok(());
        }
        let count = self.stale.get_mut(&id).ok_or(EngineError::StaleCount { cell: id })?;
        *count = count.checked_sub(1).ok_or(EngineError::StaleCount { cell: id })?;
        Ok(())
    }

    /// The `saw` questions `take_row` asked and the reads they compared, since the last take.
    pub fn take_cost(&mut self) -> (u64, u64) {
        let cost = (self.saw_calls, self.compared);
        self.saw_calls = 0;
        self.compared = 0;
        cost
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
    /// the form kept what the row saw, else by generation (the module's "Generations": moved in
    /// the last answer after the row, or left another digest by this request; its `was` is then
    /// the last answer's digest).
    fn moved_read(&self, name: Name, serial: Gen, cell: L::Cell, saw: Option<ContentId>) -> Result<Option<Cause>, EngineError> {
        let id = self.id_of(cell, name)?;
        let now = self.digest(&id);
        Ok(match saw {
            Some(saw) => (now != saw).then_some(Cause::ReadMoved { read: id, was: saw, now }),
            None => {
                let moved = self.moved_now(&id, cell) || self.last.moved(cell) > Serial(serial.0);
                moved.then(|| Cause::ReadMoved { read: id, was: self.last.content(cell), now })
            }
        })
    }

    /// Whether this request moved the cell `id` (`cell` in the last answer) for a row of the last
    /// answer: it holds another digest now than the last answer held. A cell re-made to equal
    /// bytes, or moved and moved back inside this request, did not move (early cutoff, engine.rs
    /// invariant 12). `Plant::GenIdenticalMoves`: any cell this request touched moved.
    pub fn moved_now(&self, id: &CellId, cell: L::Cell) -> bool {
        match self.work.cells.get(id) {
            None => false,
            Some(_) if self.plant == Plant::GenIdenticalMoves => true,
            Some(now) => CellDigest::of(&now.map_or(ABSENT, |(digest, _)| digest)) != self.last.digest(cell),
        }
    }

    /// Whether `reader`'s row read `id` and saw `now`: `None` for no row (engine.rs `readers_of`
    /// skips a reader that saw today's digest and counts the rest, the module's "Stale counts"),
    /// and the reads compared to say so: a binary search, O(log reads), in this request's row
    /// (`Row::read_of`) and in the last answer's (`LastAnswer::read_of`). `Plant::LinearSaw`
    /// walks the read list, as before plan row B1.
    pub fn saw(&self, reader: Name, id: CellId, now: ContentId) -> Result<(Option<bool>, u32), EngineError> {
        let linear = self.plant == Plant::LinearSaw;
        if let Some(slot) = self.work.rows.get(&reader) {
            let Some(row) = slot else { return Ok((None, 0)) };
            let (seen, compared) = match linear {
                true => walked(row.reads.iter().map(|(read, seen)| (*read, *seen)), |read| *read == id),
                false => row.read_of(&id),
            };
            return Ok((Some(seen == Some(now)), compared));
        }
        let Some(task) = self.last.task(&reader) else { return Ok((None, 0)) };
        let Some(facts) = self.last_facts(task)? else { return Ok((None, 0)) };
        let Some(cell) = self.last.cell(&id) else { return Ok((Some(false), 0)) };
        let (found, compared) = match linear {
            true => match self.last.seen(task) {
                Some(seen) => {
                    let (saw, compared) = walked(seen, |read| *read == cell);
                    (saw.map_or(Found::NotRead, Found::Saw), compared)
                }
                None => {
                    let (read, compared) = walked(self.last.reads(task).into_iter().flatten().map(|read| (read, ())), |read| *read == cell);
                    (read.map_or(Found::NotRead, |()| Found::ReadAt(Serial(facts.serial.0))), compared)
                }
            },
            false => {
                let read = self.last.read_of(task, cell)?;
                (read.found, read.compared)
            }
        };
        // By generation: the row saw what the last answer held of the cell (no move after the
        // row), and this request left the cell holding that (`moved_now`).
        // `Plant::GenSawIgnoresRequest` drops the second.
        let saw = match found {
            Found::NotRead => false,
            Found::Saw(seen) => seen == now,
            Found::ReadAt(serial) if self.plant == Plant::GenSawIgnoresRequest => self.last.moved(cell) <= serial,
            Found::ReadAt(serial) => !self.moved_now(&id, cell) && self.last.moved(cell) <= serial,
        };
        Ok((Some(saw), compared))
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

    /// The last answer's readers of `id`, pushed on `out` in the last answer's order: the first
    /// half of shadowing rule 3, which the engine completes with this request's writes
    /// (engine.rs `Engine::readers_into`).
    pub fn last_readers_into(&self, id: CellId, out: &mut SVec<'a, Name>) -> Result<(), EngineError> {
        if let Some(cell) = self.last.cell(&id) {
            for task in self.last.readers(cell, Alloc::from(self.work.alloc))?.iter() {
                out.push(self.name_of(*task, id.of)?);
            }
        }
        Ok(())
    }

    /// The request's writes to the reverse index, at its end (the engine's finish).
    pub fn put_readers(&mut self, mut edits: ReaderEdits<'a>) -> Result<(), EngineError> {
        // Within a phase and across waves the live index always answers with the latest
        // write. At the fold, omit writes that restored a reader's original membership.
        // A row changed only once emitted a symmetric difference already.
        if !self.replaced_again.is_empty() {
            let mut kept = 0;
            for at in 0..edits.edits.len() {
                let ((id, reader), added) = edits.edits[at];
                if self.replaced_again.contains(&reader) {
                    let had = match (self.last.task(&reader), self.last.cell(&id)) {
                        (Some(task), Some(cell)) => self.last.read_of(task, cell)?.found != Found::NotRead,
                        _ => false,
                    };
                    if added == had {
                        continue;
                    }
                }
                edits.edits[kept] = edits.edits[at];
                kept += 1;
            }
            edits.edits.truncate(kept);
        }
        if self.plant == Plant::LostReaderTombstone {
            edits.edits.retain(|(_, added)| *added);
        }
        self.work.readers = edits;
        Ok(())
    }

    // File plans (rule 4).

    /// Compare a planned member's key from the file's proof column. Rows changed or
    /// retired in this request shadow every proof in the last root. A task newly
    /// listed by this file can still have a row elsewhere in the last answer.
    pub(crate) fn member_stands(&self, file: Name, name: Name, key: Key) -> Result<bool, EngineError> {
        if let Some(slot) = self.work.rows.get(&name) {
            return Ok(slot.as_ref().is_some_and(|row| row.key == key));
        }
        if self.last.planned_key(file, name)? == Some(key) {
            return Ok(true);
        }
        Ok(self.row(name)?.is_some_and(|row| row.key == key))
    }

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
