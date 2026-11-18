//! The generation rule's oracle (overlay.rs, "Generations"; rule 43: proven by simulation, never
//! by review): [`GenLast`], a form of the last answer that keeps NO digest a row saw (per row its
//! serial, per cell the serial it last moved at), so the engine over it judges every row of the
//! last answer by generation alone, as the rows form does (blaze-cli `RowsAnswer`).
//!
//! The twin runs each request of sim.rs's histories on `Engine<Answer>` (by digest) and on
//! `Engine<GenLast>` (by generation) and holds, after each:
//! - the generation form enters a SUPERSET of what the digest form enters (fewer is a stale
//!   answer);
//! - every row is equal in both (key, out, plan, lists; not the serial), and so are the cells,
//!   the reverse index, the file plans, the owed tasks and the findings; the owed cells by
//!   generation are a superset (a reader that saw a move-back keeps its cell owed);
//! - every EXTRA entry comes out identical to its last row, and is explained by a cell it read
//!   that took another digest after the row's serial and came back (the rule's one cost); the
//!   test keeps each request's digests for that, the form does not.
//!
//! The controls plant one bug each: a cell re-made to equal bytes advancing its generation
//! (caught by the explanation), `Overlay::saw` ignoring this request's moves (a stale answer),
//! and a row's serial taken from the request's end instead of the rule's (a stale answer in a
//! request that leaves a row of its own stale: the fixture, under `Plant::SameStageStale`).

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::println;
use std::vec::Vec as StdVec;

use blaze_answer::{CellDigest, Flags, Serial, TreeDigest};
use blaze_drive::cells::Cell;
use blaze_drive::key::{self, ABSENT, Key, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::engine::{Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Everything, Finished, Phase, Plant, Planned, SVec, Stage, Task, Tools, View, drive};
use crate::last::{Found, LastAnswer, LastError, LastRow, ReadOf, Touched};
use crate::overlay::{Fold, Work};
use crate::sim::{Crates, POISON, Pools, alloc, edit, first_world, oracle, request};

/// The bugs the controls plant: two in the engine's overlay (`Plant`), one in this form's fold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Bug {
    None,
    /// `Plant::GenIdenticalMoves`.
    IdenticalMoves,
    /// `Plant::GenSawIgnoresRequest`.
    SawIgnoresRequest,
    /// The fold stamps every row put with the request's serial, stale or not.
    SerialAtEnd,
}

impl Bug {
    fn plant(self) -> Plant {
        match self {
            Bug::IdenticalMoves => Plant::GenIdenticalMoves,
            Bug::SawIgnoresRequest => Plant::GenSawIgnoresRequest,
            Bug::None | Bug::SerialAtEnd => Plant::None,
        }
    }
}

/// One row as the generation form keeps it: no digest it saw.
#[derive(Clone, PartialEq, Eq, Debug)]
struct GenRow {
    key: Key,
    out: ContentId,
    /// The serial it stands from (overlay.rs `Generations::row`).
    serial: u64,
    phase: Phase,
    file: Name,
    source: ContentId,
    findings: u32,
    inline: bool,
    failed: bool,
    /// In read order.
    reads: StdVec<CellId>,
    /// By cell, with the whole digest the cell holds (what the row made, not what it saw).
    defines: StdVec<(CellId, ContentId)>,
}

/// The last answer by generation. Handles are the engine's `Name` and `CellId`.
pub(crate) struct GenLast {
    serial: u64,
    rows: BTreeMap<Name, GenRow>,
    /// Every cell ever moved, absent ones too: its digest ([`ABSENT`]) and its moved-at.
    cells: BTreeMap<CellId, (ContentId, u64)>,
    readers: BTreeMap<CellId, BTreeSet<Name>>,
    by_file: BTreeMap<Name, StdVec<Name>>,
    owed: BTreeMap<Name, Planned>,
    owed_cells: BTreeMap<CellId, u64>,
    findings: BTreeSet<Name>,
    bug: Bug,
}

/// A row's serial and reads before a request: what an extra entry is explained from.
type Before = BTreeMap<Name, (u64, StdVec<CellId>)>;

impl GenLast {
    pub(crate) fn new(bug: Bug) -> GenLast {
        GenLast {
            serial: 0,
            rows: BTreeMap::new(),
            cells: BTreeMap::new(),
            readers: BTreeMap::new(),
            by_file: BTreeMap::new(),
            owed: BTreeMap::new(),
            owed_cells: BTreeMap::new(),
            findings: BTreeSet::new(),
            bug,
        }
    }

    fn before(&self) -> Before {
        self.rows.iter().map(|(name, row)| (*name, (row.serial, row.reads.clone()))).collect()
    }

    fn moved_at(&self, cell: &CellId) -> u64 {
        match self.cells.get(cell) {
            Some((_, at)) => *at,
            None => self.owed_cells.get(cell).copied().unwrap_or(0),
        }
    }

    /// The lists of `task`: its row's, empty for an owed task with no row, `None` for neither.
    fn lists<'r, T>(&'r self, task: &Name, of: impl FnOnce(&'r GenRow) -> &'r [T]) -> Option<&'r [T]> {
        match self.rows.get(task) {
            Some(row) => Some(of(row)),
            None if self.owed.contains_key(task) => Some(&[]),
            None => None,
        }
    }
}

type GenReads<'r> = core::iter::Copied<core::slice::Iter<'r, CellId>>;
type GenDefines<'r> = core::iter::Map<core::slice::Iter<'r, (CellId, ContentId)>, fn(&(CellId, ContentId)) -> (CellId, CellDigest)>;

fn define_digest(define: &(CellId, ContentId)) -> (CellId, CellDigest) {
    (define.0, CellDigest::of(&define.1))
}

impl LastAnswer for GenLast {
    type Task = Name;
    type Cell = CellId;
    type Reads<'r>
        = GenReads<'r>
    where
        Self: 'r;
    type Defines<'r>
        = GenDefines<'r>
    where
        Self: 'r;
    type Seen<'r>
        = core::iter::Empty<(CellId, ContentId)>
    where
        Self: 'r;

    fn serial(&self) -> Serial {
        Serial(self.serial)
    }

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
        let owed = self.owed.get(&task);
        let Some(row) = self.rows.get(&task) else {
            return Ok(owed.map(|planned| LastRow {
                key: Key::default(),
                out: ABSENT,
                serial: Serial(self.serial),
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
        let (phase, file, source) = owed.map_or((row.phase, row.file, row.source), |planned| (planned.phase, planned.file, planned.source));
        Ok(Some(LastRow {
            key: row.key,
            out: row.out,
            serial: Serial(row.serial),
            phase,
            file,
            source,
            flags: Flags(flags),
            findings: row.findings,
            reads: row.reads.len() as u32,
            defines: row.defines.len() as u32,
        }))
    }

    fn reads(&self, task: Name) -> Option<GenReads<'_>> {
        self.lists(&task, |row| &row.reads[..]).map(|reads| reads.iter().copied())
    }

    fn defines(&self, task: Name) -> Option<GenDefines<'_>> {
        let walk: fn(&(CellId, ContentId)) -> (CellId, CellDigest) = define_digest;
        self.lists(&task, |row| &row.defines[..]).map(|defines| defines.iter().map(walk))
    }

    /// None, ever: this form judges by generation alone.
    fn seen(&self, _task: Name) -> Option<core::iter::Empty<(CellId, ContentId)>> {
        None
    }

    fn stands(&self, task: Name) -> Option<bool> {
        match self.rows.get(&task) {
            Some(row) => Some(row.reads.iter().all(|cell| self.moved_at(cell) <= row.serial)),
            None => self.owed.contains_key(&task).then_some(true),
        }
    }

    /// A test form: the read list walked, its count honest (the bound is finish_scale_sim.rs's).
    fn read_of(&self, task: Name, cell: CellId) -> Result<ReadOf, LastError> {
        Ok(match self.rows.get(&task) {
            Some(row) => match row.reads.iter().position(|read| *read == cell) {
                Some(at) => ReadOf { found: Found::ReadAt(Serial(row.serial)), compared: u32::try_from(at + 1).unwrap_or(u32::MAX) },
                None => ReadOf { found: Found::NotRead, compared: u32::try_from(row.reads.len()).unwrap_or(u32::MAX) },
            },
            None => ReadOf { found: Found::NotRead, compared: 0 },
        })
    }

    fn digest(&self, cell: CellId) -> CellDigest {
        CellDigest::of(&self.content(cell))
    }

    fn content(&self, cell: CellId) -> ContentId {
        self.cells.get(&cell).map_or(ABSENT, |(digest, _)| *digest)
    }

    fn moved(&self, cell: CellId) -> Serial {
        Serial(self.moved_at(&cell))
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
        let mut out = Vec::with_capacity_in(self.owed.len(), alloc);
        for planned in self.owed.values() {
            out.push(*planned);
        }
        Ok(out)
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (CellId, Serial)>, LastError> {
        let mut out = Vec::with_capacity_in(self.owed_cells.len(), alloc);
        for (id, at) in self.owed_cells.iter() {
            out.push((*id, Serial(*at)));
        }
        Ok(out)
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Name, u32)>, LastError> {
        let mut out = Vec::with_capacity_in(self.findings.len(), alloc);
        for name in self.findings.iter() {
            out.push((*name, self.rows.get(name).map_or(0, |row| row.findings)));
        }
        Ok(out)
    }

    fn touched(&self) -> Touched {
        Touched::default()
    }
}

impl Fold<'static> for GenLast {
    type Out = GenLast;

    /// The request's writes by the rule (overlay.rs `Work::generations`): cells first (a moved
    /// one takes the request's serial, one re-made to equal bytes keeps its own), then rows with
    /// the serial each stands from, then the reverse index, plans, findings and owed sets.
    fn fold(mut self, work: Work<'static>) -> Result<GenLast, EngineError> {
        let serial = work.serial().0;
        let generations = work.generations();
        let moved = generations.moved(|id| CellDigest::of(&self.content(*id)));
        for (id, now) in work.cells.iter() {
            let digest = now.map_or(ABSENT, |(digest, _)| digest);
            let at = self.cells.get(id).map_or(0, |(_, at)| *at);
            self.cells.insert(*id, (digest, at));
        }
        for id in moved.iter() {
            let digest = self.content(*id);
            self.cells.insert(*id, (digest, serial));
        }
        for (name, row) in work.rows.iter() {
            match row {
                Some(row) => {
                    let stands_from = match self.bug {
                        Bug::SerialAtEnd => serial,
                        _ => generations.row(row).0,
                    };
                    let kept = GenRow {
                        key: row.key,
                        out: row.out,
                        serial: stands_from,
                        phase: row.phase,
                        file: row.file,
                        source: row.source,
                        findings: row.findings,
                        inline: row.inline,
                        failed: row.failed,
                        reads: row.reads.iter().map(|(id, _)| *id).collect(),
                        defines: row.defines.iter().map(|(cell, digest)| (CellId { of: *name, cell: *cell }, *digest)).collect(),
                    };
                    self.rows.insert(*name, kept);
                }
                None => {
                    self.rows.remove(name);
                }
            }
        }
        for (id, delta) in work.readers.iter() {
            let readers = self.readers.entry(*id).or_default();
            for (reader, added) in delta.iter() {
                match added {
                    true => {
                        readers.insert(*reader);
                    }
                    false => {
                        readers.remove(reader);
                    }
                }
            }
            if readers.is_empty() {
                self.readers.remove(id);
            }
        }
        for (file, names) in work.by_file.iter() {
            match names {
                Some(names) => {
                    self.by_file.insert(*file, names.iter().copied().collect());
                }
                None => {
                    self.by_file.remove(file);
                }
            }
        }
        for (name, with) in work.findings.iter() {
            match with {
                true => {
                    self.findings.insert(*name);
                }
                false => {
                    self.findings.remove(name);
                }
            }
        }
        self.owed = work.owed_tasks.iter().map(|(name, planned)| (*name, *planned)).collect();
        self.owed_cells = work.owed_cells.iter().map(|(id, at)| (*id, at.0)).collect();
        self.serial = serial;
        Ok(self)
    }
}

/// The two forms' tables after request `k`: equal but for the rows' serials (the generation
/// form's owed cells a superset).
fn same_tables(k: u64, mem: &Answer<'_>, by_gen: &GenLast) -> Result<(), Broke> {
    let mem_names: StdVec<Name> = mem.rows.keys().copied().collect();
    let gen_names: StdVec<Name> = by_gen.rows.keys().copied().collect();
    oracle(mem_names == gen_names, || format!("request {k}: rows by digest {mem_names:?}, by generation {gen_names:?}"))?;
    for (name, row) in mem.rows.iter() {
        let Some(kept) = by_gen.rows.get(name) else { continue };
        let reads: StdVec<CellId> = row.reads.iter().map(|(id, _)| *id).collect();
        let defines: StdVec<(CellId, ContentId)> = row.defines.iter().map(|(cell, digest)| (CellId { of: *name, cell: *cell }, *digest)).collect();
        let equal = kept.key == row.key
            && kept.out == row.out
            && kept.phase == row.phase
            && kept.file == row.file
            && kept.source == row.source
            && kept.findings == row.findings
            && kept.inline == row.inline
            && kept.failed == row.failed
            && kept.reads == reads
            && kept.defines == defines;
        oracle(equal, || format!("request {k}: {name:?}'s row by digest {row:?}, by generation {kept:?}"))?;
    }
    for (id, (digest, _)) in mem.cells.iter() {
        oracle(by_gen.content(*id) == *digest, || format!("request {k}: cell {id:?} holds {digest:?} by digest, {:?} by generation", by_gen.content(*id)))?;
    }
    for (id, (digest, _)) in by_gen.cells.iter() {
        oracle(*digest == ABSENT || mem.cells.contains_key(id), || format!("request {k}: cell {id:?} is defined by generation alone"))?;
    }
    let mem_readers: BTreeMap<CellId, BTreeSet<Name>> = mem.readers.iter().map(|(id, readers)| (*id, readers.iter().copied().collect())).collect();
    oracle(mem_readers == by_gen.readers, || format!("request {k}: the reverse index differs between the forms"))?;
    let mem_files: BTreeMap<Name, StdVec<Name>> = mem.by_file.iter().map(|(file, names)| (*file, names.iter().copied().collect())).collect();
    oracle(mem_files == by_gen.by_file, || format!("request {k}: the file plans differ between the forms"))?;
    let mem_owed: BTreeMap<Name, Planned> = mem.owed_tasks.iter().map(|(name, planned)| (*name, *planned)).collect();
    oracle(mem_owed == by_gen.owed, || format!("request {k}: owed tasks by digest {mem_owed:?}, by generation {:?}", by_gen.owed))?;
    let mem_findings: BTreeSet<Name> = mem.with_findings.iter().copied().collect();
    oracle(mem_findings == by_gen.findings, || format!("request {k}: the rows with findings differ between the forms"))?;
    for id in mem.owed_cells.keys() {
        oracle(by_gen.owed_cells.contains_key(id), || format!("request {k}: {id:?} is owed by digest and settled by generation: a reader judged to have seen it"))?;
    }
    Ok(())
}

/// Whether the cell `id` took another digest after request `from` and before request `to` than it
/// held at the end of `from` (`history[j]`: every cell's digest at the end of request `j`).
fn moved_back(history: &[BTreeMap<CellId, ContentId>], from: u64, to: u64, id: &CellId) -> bool {
    let at = |j: u64| history.get(j as usize).and_then(|cells| cells.get(id)).copied().unwrap_or(ABSENT);
    let saw = at(from);
    (from + 1..to).any(|j| at(j) != saw)
}

/// Request `k`'s entries: the generation form's are a superset of the digest form's, and each
/// extra one is identical and explained by a move-back. Gives the extras' count.
fn judge(k: u64, mem: &Finished<'static>, by_gen: &Finished<'static, GenLast>, before: &Before, history: &[BTreeMap<CellId, ContentId>]) -> Result<u64, Broke> {
    let ours: BTreeSet<Name> = mem.entered.iter().copied().collect();
    let theirs: BTreeSet<Name> = by_gen.entered.iter().copied().collect();
    for name in ours.iter() {
        oracle(theirs.contains(name), || format!("request {k}: the digest form entered {name:?} and the generation form did not: a stale answer"))?;
    }
    let mut extras = 0u64;
    for name in theirs.difference(&ours) {
        extras += 1;
        let identical = by_gen.causes.iter().any(|entered| entered.name == *name && entered.identical);
        oracle(identical, || format!("request {k}: {name:?}, entered by generation alone, came out other bytes than its last row"))?;
        let explained = before.get(name).is_some_and(|(serial, reads)| reads.iter().any(|id| moved_back(history, *serial, k, id)));
        oracle(explained, || {
            format!("request {k}: {name:?}, entered by generation alone, read no cell that moved after its row and back: a move-back is the rule's only cost")
        })?;
    }
    Ok(extras)
}

/// Every cell's digest in the generation form: the history the test keeps (the form does not).
fn digests(by_gen: &GenLast) -> BTreeMap<CellId, ContentId> {
    by_gen.cells.iter().map(|(id, (digest, _))| (*id, *digest)).collect()
}

/// What the clean twin counts across its histories.
#[derive(Default)]
struct Tally {
    extras: u64,
    entered: u64,
    histories: u64,
}

/// One history (sim.rs's mix, as last_sim.rs's twin), each request on `Engine<Answer>` with no bug
/// and on `Engine<GenLast>` with `bug`, judged after each.
fn gen_twin(tape: &Tape, bug: Bug, tally: &mut Tally) -> Result<(), Broke> {
    let mut world = first_world(tape);
    let everything: BTreeSet<usize> = (0..world.crates.len()).collect();
    let mut mem = Answer::new_in(alloc());
    let mut by_gen = GenLast::new(bug);
    let mut history: StdVec<BTreeMap<CellId, ContentId>> = StdVec::from([BTreeMap::new()]);
    let mut last_plans: BTreeMap<Name, StdVec<Planned>> = BTreeMap::new();
    for at in 0..2 + tape.below(9, "requests") {
        let k = at + 1;
        if at > 0 {
            world = edit(tape, &world);
        }
        let asked: BTreeSet<usize> = match tape.one_in(2, "a narrower scope") {
            true => everything.iter().copied().filter(|_| tape.one_in(2, "a crate asked")).collect(),
            false => everything.clone(),
        };
        if tape.one_in(4, "a body poisoned") {
            let count = world.files.iter().map(|(_, items)| items.len()).sum::<usize>();
            if count > 0 {
                let pick = tape.index(count, "the poisoned body");
                if let Some(item) = world.files.iter_mut().flat_map(|(_, items)| items.iter_mut()).nth(pick) {
                    item.body = POISON;
                }
            }
        }
        let pool = [Pools::One, Pools::Many(1 + tape.index(4, "workers")), Pools::Reversed][tape.index(3, "the pool")];
        let scope = Crates::of(&world, asked);
        let plans = world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        let before = by_gen.before();
        let ours = request(&world, mem, &changed, scope.clone(), Plant::None, pool).map_err(|error| Broke::Harness(format!("request {k} by digest failed: {error}")))?;
        let theirs = request(&world, by_gen,&changed, scope, bug.plant(), pool).map_err(|error| Broke::Oracle(format!("request {k} by generation failed: {error}")))?;
        last_plans = plans;
        tally.extras += judge(k, &ours, &theirs, &before, &history)?;
        tally.entered += theirs.entered.len() as u64;
        same_tables(k, &ours.answer, &theirs.answer)?;
        history.push(digests(&theirs.answer));
        mem = ours.answer;
        by_gen = theirs.answer;
    }
    tally.histories += 1;
    Ok(())
}

/// Expected: "extra entries by generation: E of N over H histories", E a small share of N (each a
/// body text that moved and moved back while its crate was out of scope, or the like), and ok.
#[test]
fn generation_form_enters_a_superset() {
    let mut tally = Tally::default();
    check(SimConfig::cases(300), |tape| gen_twin(tape, Bug::None, &mut tally));
    println!("extra entries by generation: {} of {} over {} histories", tally.extras, tally.entered, tally.histories);
}

/// A cell re-made to equal bytes advances its generation: a reader is entered with nothing it
/// read ever taking another digest (caught by the explanation of extras).
#[test]
fn control_identical_remakes_moving_are_caught() {
    control(SimConfig::cases(300), Bug::None, Bug::IdenticalMoves, "", |tape, bug| gen_twin(tape, bug, &mut Tally::default()));
}

/// `Overlay::saw` takes a reader of the last answer to have seen a cell this request moved: the
/// reader is never entered (a stale answer).
#[test]
fn control_saw_ignoring_the_request_is_caught() {
    control(SimConfig::cases(300), Bug::None, Bug::SawIgnoresRequest, "", |tape, bug| gen_twin(tape, bug, &mut Tally::default()));
}

/// The fixture's tool, file and two heads: `SECOND` reads `FIRST`'s cell, both at Heads wave 0.
const FIXTURE_TOOL: Tool = Tool(ContentId([71; 32]));
const FILE: Name = Name::new(Kind::FileItems, ContentId([72; 32]));
const FIRST: Name = Name::new(Kind::Signature, ContentId([73; 32]));
const SECOND: Name = Name::new(Kind::Signature, ContentId([74; 32]));
const HELD: Cell = Cell([5; 16]);

/// The fixture's tasks: `FIRST` defines its source; `SECOND` reads `FIRST`'s cell and defines a
/// digest of what it saw.
fn fixture_work<L: LastAnswer>(view: View<'_, 'static, L>, task: &Task, buffer: &mut Buffer<'static>) {
    let a = alloc();
    let name = task.planned.name;
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(a);
    let value = match name == SECOND {
        true => {
            let read = CellId { of: FIRST, cell: HELD };
            let seen = view.cell(&read);
            reads.push((read, seen));
            key::digest("gen fixture second", &seen.0)
        }
        false => task.planned.source,
    };
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(a);
    defines.push((HELD, value));
    buffer.done.push(Done { name, key: task.key, out: value, defines, reads, findings: 0, failed: false });
}

fn fixture_request<L: Fold<'static> + Sync>(last: L, files: &[(Name, &[Planned])], plant: Plant) -> Result<Finished<'static, L::Out>, EngineError> {
    let mut engine = Engine::new_in(Tools([FIXTURE_TOOL; key::KINDS]), last, &Edit { files }, Everything, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = StdVec::from([Buffer::new_in(alloc())]);
    drive(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| fixture_work(view, task, buffer))?;
    engine.finish()
}

/// A request that leaves a row of its own stale: under `Plant::SameStageStale` (both engines),
/// `SECOND`, made at wave 0 on `FIRST`'s old digest, is not entered again at wave 1, and its
/// read is owed. The next request, with nothing edited, must enter it from the owed cell: by
/// generation that holds only when its serial is the rule's (`s - 1`: it saw a digest the request
/// then moved on), not the request's end.
fn same_request_history(bug: Bug) -> Result<(), Broke> {
    let phase = Phase::new(Stage::Heads, 0);
    let plan = [Planned { name: FIRST, phase, source: ContentId([75; 32]), file: FILE }, Planned { name: SECOND, phase, source: ContentId([76; 32]), file: FILE }];
    let edited: [(Name, &[Planned]); 1] = [(FILE, &plan[..])];
    let nothing: [(Name, &[Planned]); 0] = [];
    let mut mem = Answer::new_in(alloc());
    let mut by_gen = GenLast::new(bug);
    let mut history: StdVec<BTreeMap<CellId, ContentId>> = StdVec::from([BTreeMap::new()]);
    for (k, files) in [(1u64, &edited[..]), (2, &nothing[..])] {
        let before = by_gen.before();
        let ours = fixture_request(mem, files, Plant::SameStageStale).map_err(|error| Broke::Harness(format!("request {k} by digest failed: {error}")))?;
        let theirs = fixture_request(by_gen, files, Plant::SameStageStale).map_err(|error| Broke::Oracle(format!("request {k} by generation failed: {error}")))?;
        judge(k, &ours, &theirs, &before, &history)?;
        same_tables(k, &ours.answer, &theirs.answer)?;
        history.push(digests(&theirs.answer));
        mem = ours.answer;
        by_gen = theirs.answer;
    }
    Ok(())
}

/// Expected: the clean history holds; the row's serial taken from the request's end is caught as
/// "the digest form entered ... and the generation form did not".
#[test]
fn control_serial_at_the_end_is_caught() {
    if let Err(broke) = same_request_history(Bug::None) {
        panic!("the clean same-request history broke: {broke:?}");
    }
    match same_request_history(Bug::SerialAtEnd) {
        Err(Broke::Oracle(why)) => println!("plant SerialAtEnd caught: {why}"),
        other => panic!("the row's serial from the request's end was not caught: {other:?}"),
    }
}
