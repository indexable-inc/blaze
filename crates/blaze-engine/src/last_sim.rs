//! The engine over a second form of the last answer (rule 78: one seam, each instantiation tested
//! equal): [`SimLast`] answers the seam (last.rs) from sorted vectors by binary search, with dense
//! handles as a stored form has, and counts the rows each request asked for. The history
//! simulation (sim.rs) runs every request twice, on `Engine<Answer>` and on `Engine<SimLast>`,
//! and holds them equal after each: the tasks entered and in what order, why, the report, and
//! every table of the next answer (rows with their outs, reads and defines, cells, the reverse
//! index, file plans, owed tasks and cells, findings). The control plants
//! `Plant::ReadersStale` in the overlay (the readers this request dropped still answered) on the
//! `SimLast` side and holds that the oracle catches it.
//!
//! [`rows_touched_by_one_edit`] is the cost claim: one edit to a 2,000-task answer asks for a
//! twentieth of its rows or fewer.

use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::println;
use std::sync::Mutex;
use std::vec::Vec as StdVec;

use blaze_answer::{CellDigest, Flags, Serial, TreeDigest};
use blaze_drive::key::{ABSENT, Key, Name};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, BTreeSet as OwnSet, SharedAlloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::engine::{Answer, CellId, EngineError, Gen, Phase, Plant, Planned, Row};
use crate::last::{Found, LastAnswer, LastError, LastRow, ReadOf, Touched, search};
use crate::overlay::{Fold, Work};
use crate::sim::{Crates, Item, POISON, Pools, World, alloc, edit, first_world, oracle, request};

/// One row as the sorted form keeps it: dense handles, reads in read order with what each saw,
/// defines by cell with their whole digests (the in-memory answer is rebuilt from them).
#[derive(Clone, Debug)]
struct SimRow {
    key: Key,
    out: ContentId,
    serial: u64,
    phase: Phase,
    file: Name,
    source: ContentId,
    findings: u32,
    inline: bool,
    failed: bool,
    reads: StdVec<(u32, ContentId)>,
    /// The positions in `reads` sorted by cell: the in-memory row's (a handle's order is its
    /// cell's), so a search here compares as many reads as there (the twin's reports are equal).
    by_cell: StdVec<u32>,
    defines: StdVec<(u32, ContentId)>,
}

/// The last answer as sorted vectors. Handles are positions in the two dictionaries.
pub(crate) struct SimLast {
    serial: u64,
    /// Every name the answer holds, sorted: a task's handle is its position.
    names: StdVec<Name>,
    /// Every cell the answer holds, sorted: a cell's handle is its position.
    cells: StdVec<CellId>,
    /// By task.
    rows: StdVec<(u32, SimRow)>,
    /// By cell: its digest and the request it moved in.
    digests: StdVec<(u32, ContentId, u64)>,
    /// (cell, reader), sorted.
    readers: StdVec<(u32, u32)>,
    /// (file, planned task), sorted.
    by_file: StdVec<(u32, u32)>,
    /// By name.
    owed: StdVec<Planned>,
    /// By cell.
    owed_cells: StdVec<(u32, u64)>,
    /// By task.
    findings: StdVec<u32>,
    /// The distinct rows asked for.
    touched_rows: Mutex<BTreeSet<u32>>,
    touched_reads: AtomicU64,
    touched_readers: AtomicU64,
}

fn put<T: Ord + Copy>(set: &mut BTreeSet<T>, one: T) {
    set.insert(one);
}

impl SimLast {
    /// The sorted form of `answer`.
    pub(crate) fn of(answer: &Answer<'_>) -> SimLast {
        let mut names: BTreeSet<Name> = BTreeSet::new();
        let mut cells: BTreeSet<CellId> = BTreeSet::new();
        for (name, row) in answer.rows.iter() {
            put(&mut names, *name);
            for (id, _) in row.reads.iter() {
                put(&mut cells, *id);
            }
            for (cell, _) in row.defines.iter() {
                put(&mut cells, CellId { of: *name, cell: *cell });
            }
        }
        for id in answer.cells.keys() {
            put(&mut cells, *id);
        }
        for (id, readers) in answer.readers.iter() {
            put(&mut cells, *id);
            for reader in readers.iter() {
                put(&mut names, *reader);
            }
        }
        for (file, planned) in answer.by_file.iter() {
            put(&mut names, *file);
            for one in planned.iter() {
                put(&mut names, *one);
            }
        }
        for name in answer.owed_tasks.keys() {
            put(&mut names, *name);
        }
        for id in answer.owed_cells.keys() {
            put(&mut cells, *id);
        }
        for name in answer.with_findings.iter() {
            put(&mut names, *name);
        }
        let names: StdVec<Name> = names.into_iter().collect();
        let cells: StdVec<CellId> = cells.into_iter().collect();
        let task = |name: &Name| names.binary_search(name).map(|at| at as u32).expect("every name is in the dictionary");
        let cell = |id: &CellId| cells.binary_search(id).map(|at| at as u32).expect("every cell is in the dictionary");
        let rows = answer
            .rows
            .iter()
            .map(|(name, row)| {
                let reads = row.reads.iter().map(|(id, seen)| (cell(id), *seen)).collect();
                let defines = row.defines.iter().map(|(c, digest)| (cell(&CellId { of: *name, cell: *c }), *digest)).collect();
                let kept = SimRow {
                    key: row.key,
                    out: row.out,
                    serial: row.serial.0,
                    phase: row.phase,
                    file: row.file,
                    source: row.source,
                    findings: row.findings,
                    inline: row.inline,
                    failed: row.failed,
                    reads,
                    by_cell: row.by_cell.iter().copied().collect(),
                    defines,
                };
                (task(name), kept)
            })
            .collect();
        let digests = answer.cells.iter().map(|(id, (digest, at))| (cell(id), *digest, at.0)).collect();
        let mut readers: StdVec<(u32, u32)> = answer.readers.iter().flat_map(|(id, readers)| readers.iter().map(|reader| (cell(id), task(reader))).collect::<StdVec<_>>()).collect();
        readers.sort_unstable();
        let mut by_file: StdVec<(u32, u32)> = answer.by_file.iter().flat_map(|(file, planned)| planned.iter().map(|one| (task(file), task(one))).collect::<StdVec<_>>()).collect();
        by_file.sort_unstable();
        let owed = answer.owed_tasks.values().copied().collect();
        let owed_cells = answer.owed_cells.iter().map(|(id, at)| (cell(id), at.0)).collect();
        let findings = answer.with_findings.iter().map(task).collect();
        SimLast {
            serial: answer.serial.0,
            names,
            cells,
            rows,
            digests,
            readers,
            by_file,
            owed,
            owed_cells,
            findings,
            touched_rows: Mutex::new(BTreeSet::new()),
            touched_reads: AtomicU64::new(0),
            touched_readers: AtomicU64::new(0),
        }
    }

    /// The in-memory answer this form holds, table by table.
    fn to_answer(&self, alloc: SharedAlloc<'static>) -> Answer<'static> {
        let mut answer = Answer::new_in(alloc);
        answer.serial = Gen(self.serial);
        for (task, row) in self.rows.iter() {
            let name = self.names[*task as usize];
            let mut reads = Vec::with_capacity_in(row.reads.len(), alloc);
            for (cell, seen) in row.reads.iter() {
                reads.push((self.cells[*cell as usize], *seen));
            }
            let mut defines = Vec::with_capacity_in(row.defines.len(), alloc);
            for (cell, digest) in row.defines.iter() {
                defines.push((self.cells[*cell as usize].cell, *digest));
            }
            let mut by_cell = Vec::with_capacity_in(row.by_cell.len(), alloc);
            by_cell.extend(row.by_cell.iter().copied());
            let kept = Row {
                key: row.key,
                phase: row.phase,
                file: row.file,
                source: row.source,
                out: row.out,
                serial: Gen(row.serial),
                defines,
                reads,
                by_cell,
                findings: row.findings,
                inline: row.inline,
                failed: row.failed,
            };
            answer.rows.insert(name, kept);
        }
        for (cell, digest, at) in self.digests.iter() {
            answer.cells.insert(self.cells[*cell as usize], (*digest, Gen(*at)));
        }
        for (cell, reader) in self.readers.iter() {
            answer.readers.entry(self.cells[*cell as usize]).or_insert_with(|| OwnSet::new_in(alloc)).insert(self.names[*reader as usize]);
        }
        for (file, one) in self.by_file.iter() {
            answer.by_file.entry(self.names[*file as usize]).or_insert_with(|| Vec::new_in(alloc)).push(self.names[*one as usize]);
        }
        for planned in self.owed.iter() {
            answer.owed_tasks.insert(planned.name, *planned);
        }
        for (cell, at) in self.owed_cells.iter() {
            answer.owed_cells.insert(self.cells[*cell as usize], Gen(*at));
        }
        for task in self.findings.iter() {
            answer.with_findings.insert(self.names[*task as usize]);
        }
        answer
    }

    fn touch(&self, task: u32) {
        if let Ok(mut rows) = self.touched_rows.lock() {
            rows.insert(task);
        }
    }

    fn found_row(&self, task: u32) -> Option<&SimRow> {
        self.rows.binary_search_by_key(&task, |(at, _)| *at).ok().map(|at| &self.rows[at].1)
    }

    fn owed_of(&self, task: u32) -> Option<&Planned> {
        let name = self.names.get(task as usize)?;
        self.owed.binary_search_by_key(name, |planned| planned.name).ok().map(|at| &self.owed[at])
    }

    /// The lists of `task` through `of`: its row's, empty for an owed task with no row.
    fn lists<T>(&self, task: u32, of: impl FnOnce(&SimRow) -> &[T]) -> Option<&[T]> {
        self.touch(task);
        match self.found_row(task) {
            Some(row) => {
                let list = of(row);
                self.touched_reads.fetch_add(list.len() as u64, Ordering::Relaxed);
                Some(list)
            }
            None => self.owed_of(task).map(|_| &[][..]),
        }
    }

    fn range(pairs: &[(u32, u32)], first: u32) -> &[(u32, u32)] {
        let from = pairs.partition_point(|(at, _)| *at < first);
        let to = pairs.partition_point(|(at, _)| *at <= first);
        &pairs[from..to]
    }
}

type SimReads<'r> = core::iter::Map<core::slice::Iter<'r, (u32, ContentId)>, fn(&(u32, ContentId)) -> u32>;
type SimDefines<'r> = core::iter::Map<core::slice::Iter<'r, (u32, ContentId)>, fn(&(u32, ContentId)) -> (u32, CellDigest)>;
type SimSeen<'r> = core::iter::Copied<core::slice::Iter<'r, (u32, ContentId)>>;

fn read_cell(read: &(u32, ContentId)) -> u32 {
    read.0
}

fn define_digest(define: &(u32, ContentId)) -> (u32, CellDigest) {
    (define.0, CellDigest::of(&define.1))
}

impl LastAnswer for SimLast {
    type Task = u32;
    type Cell = u32;
    type Reads<'r>
        = SimReads<'r>
    where
        Self: 'r;
    type Defines<'r>
        = SimDefines<'r>
    where
        Self: 'r;
    type Seen<'r>
        = SimSeen<'r>
    where
        Self: 'r;

    fn serial(&self) -> Serial {
        Serial(self.serial)
    }

    fn certificate(&self, _tree: &TreeDigest) -> bool {
        false
    }

    fn task(&self, name: &Name) -> Option<u32> {
        self.names.binary_search(name).ok().map(|at| at as u32)
    }

    fn name(&self, task: u32) -> Option<Name> {
        self.names.get(task as usize).copied()
    }

    fn cell(&self, id: &CellId) -> Option<u32> {
        self.cells.binary_search(id).ok().map(|at| at as u32)
    }

    fn cell_id(&self, cell: u32) -> Option<CellId> {
        self.cells.get(cell as usize).copied()
    }

    fn row(&self, task: u32) -> Result<Option<LastRow>, LastError> {
        self.touch(task);
        let owed = self.owed_of(task);
        let Some(row) = self.found_row(task) else {
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

    fn reads(&self, task: u32) -> Option<SimReads<'_>> {
        let walk: fn(&(u32, ContentId)) -> u32 = read_cell;
        self.lists(task, |row| &row.reads[..]).map(|reads| reads.iter().map(walk))
    }

    fn defines(&self, task: u32) -> Option<SimDefines<'_>> {
        let walk: fn(&(u32, ContentId)) -> (u32, CellDigest) = define_digest;
        self.lists(task, |row| &row.defines[..]).map(|defines| defines.iter().map(walk))
    }

    fn seen(&self, task: u32) -> Option<SimSeen<'_>> {
        self.lists(task, |row| &row.reads[..]).map(|reads| reads.iter().copied())
    }

    fn stands(&self, task: u32) -> Option<bool> {
        self.touch(task);
        match self.found_row(task) {
            Some(row) => Some(row.reads.iter().all(|(cell, seen)| self.content(*cell) == *seen)),
            None => self.owed_of(task).map(|_| true),
        }
    }

    fn read_of(&self, task: u32, cell: u32) -> Result<ReadOf, LastError> {
        self.touch(task);
        Ok(match self.found_row(task) {
            Some(row) => {
                let (at, compared) = search(row.by_cell.len(), |i| row.reads[row.by_cell[i] as usize].0.cmp(&cell));
                ReadOf { found: at.map_or(Found::NotRead, |i| Found::Saw(row.reads[row.by_cell[i] as usize].1)), compared }
            }
            None => ReadOf { found: Found::NotRead, compared: 0 },
        })
    }

    fn digest(&self, cell: u32) -> CellDigest {
        CellDigest::of(&self.content(cell))
    }

    fn content(&self, cell: u32) -> ContentId {
        match self.digests.binary_search_by_key(&cell, |(at, _, _)| *at) {
            Ok(at) => self.digests[at].1,
            Err(_) => ABSENT,
        }
    }

    fn moved(&self, cell: u32) -> Serial {
        if let Ok(at) = self.digests.binary_search_by_key(&cell, |(at, _, _)| *at) {
            return Serial(self.digests[at].2);
        }
        match self.owed_cells.binary_search_by_key(&cell, |(at, _)| *at) {
            Ok(at) => Serial(self.owed_cells[at].1),
            Err(_) => Serial(0),
        }
    }

    fn readers<'m>(&self, cell: u32, alloc: Alloc<'m>) -> Result<AVec<'m, u32>, LastError> {
        let found = SimLast::range(&self.readers, cell);
        self.touched_readers.fetch_add(found.len() as u64, Ordering::Relaxed);
        let mut out = Vec::with_capacity_in(found.len(), alloc);
        for (_, reader) in found.iter() {
            out.push(*reader);
        }
        Ok(out)
    }

    fn file_tasks<'m>(&self, file: u32, alloc: Alloc<'m>) -> Result<AVec<'m, u32>, LastError> {
        let found = SimLast::range(&self.by_file, file);
        let mut out = Vec::with_capacity_in(found.len(), alloc);
        for (_, one) in found.iter() {
            out.push(*one);
        }
        Ok(out)
    }

    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        let mut out = Vec::with_capacity_in(self.owed.len(), alloc);
        for planned in self.owed.iter() {
            out.push(*planned);
        }
        Ok(out)
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (u32, Serial)>, LastError> {
        let mut out = Vec::with_capacity_in(self.owed_cells.len(), alloc);
        for (cell, at) in self.owed_cells.iter() {
            out.push((*cell, Serial(*at)));
        }
        Ok(out)
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (u32, u32)>, LastError> {
        let mut out = Vec::with_capacity_in(self.findings.len(), alloc);
        for task in self.findings.iter() {
            out.push((*task, self.found_row(*task).map_or(0, |row| row.findings)));
        }
        Ok(out)
    }

    fn touched(&self) -> Touched {
        let rows = self.touched_rows.lock().map_or(0, |rows| rows.len() as u64);
        Touched { rows, reads: self.touched_reads.load(Ordering::Relaxed), readers: self.touched_readers.load(Ordering::Relaxed), bytes: 0 }
    }
}

impl Fold<'static> for SimLast {
    type Out = Answer<'static>;

    fn fold(self, work: Work<'static>) -> Result<Answer<'static>, EngineError> {
        let mut answer = self.to_answer(work.alloc());
        work.apply_to(&mut answer);
        Ok(answer)
    }
}

/// The two answers' tables, equal or the first that differs.
fn same_tables(serial: u32, mem: &Answer<'_>, sim: &Answer<'_>) -> Result<(), Broke> {
    for (table, equal) in [
        ("rows", mem.rows == sim.rows),
        ("cells", mem.cells == sim.cells),
        ("reverse index", mem.readers == sim.readers),
        ("files", mem.by_file == sim.by_file),
        ("owed cells", mem.owed_cells == sim.owed_cells),
        ("owed tasks", mem.owed_tasks == sim.owed_tasks),
        ("rows with findings", mem.with_findings == sim.with_findings),
        ("serial", mem.serial == sim.serial),
    ] {
        oracle(equal, || format!("request {serial}: the engine over the sorted form leaves other {table} than over the in-memory answer"))?;
    }
    for (name, row) in mem.rows.iter() {
        oracle(sim.rows.get(name) == Some(row), || format!("request {serial}: {name:?}'s row: in memory {row:?}, sorted {:?}", sim.rows.get(name)))?;
    }
    Ok(())
}

/// One history (sim.rs's mix: scopes that shrink, poisoned bodies, every pool), each request run
/// on `Engine<Answer>` with no bug and on `Engine<SimLast>` with `plant`, held equal after each.
fn twin(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = first_world(tape);
    let everything: BTreeSet<usize> = (0..world.crates.len()).collect();
    let mut mem = Answer::new_in(alloc());
    let mut sim = Answer::new_in(alloc());
    let mut last_plans: BTreeMap<Name, StdVec<Planned>> = BTreeMap::new();
    for serial in 0..2 + tape.below(9, "requests") as u32 {
        if serial > 0 {
            world = edit(tape, &world);
        }
        let asked: BTreeSet<usize> = match tape.one_in(2, "a narrower scope") {
            true => everything.iter().copied().filter(|_| tape.one_in(2, "a crate asked")).collect(),
            false => everything.clone(),
        };
        if tape.one_in(4, "a body poisoned") {
            let count = world.files.iter().map(|(_, items)| items.len()).sum::<usize>();
            if count > 0 {
                let at = tape.index(count, "the poisoned body");
                if let Some(item) = world.files.iter_mut().flat_map(|(_, items)| items.iter_mut()).nth(at) {
                    item.body = POISON;
                }
            }
        }
        let pool = [Pools::One, Pools::Many(1 + tape.index(4, "workers")), Pools::Reversed][tape.index(3, "the pool")];
        let scope = Crates::of(&world, asked);
        let plans = world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        let ours = request(&world, mem, &changed, scope.clone(), Plant::None, pool).map_err(|error| Broke::Harness(format!("request {serial} in memory failed: {error}")))?;
        let theirs = request(&world, SimLast::of(&sim), &changed, scope, plant, pool)
            .map_err(|error| Broke::Oracle(format!("request {serial} over the sorted form failed: {error}")))?;
        last_plans = plans;
        oracle(ours.entered[..] == theirs.entered[..], || format!("request {serial}: entered in memory {:?}, sorted {:?}", &ours.entered[..], &theirs.entered[..]))?;
        oracle(ours.causes[..] == theirs.causes[..], || format!("request {serial}: causes in memory {:?}, sorted {:?}", &ours.causes[..], &theirs.causes[..]))?;
        oracle(ours.report == theirs.report, || format!("request {serial}: report in memory {:?}, sorted {:?}", ours.report, theirs.report))?;
        same_tables(serial, &ours.answer, &theirs.answer)?;
        mem = ours.answer;
        sim = theirs.answer;
    }
    Ok(())
}

/// Expected: "test last_sim::twin_engines_answer_alike ... ok".
#[test]
fn twin_engines_answer_alike() {
    check(SimConfig::cases(300), |tape| twin(tape, Plant::None));
}

/// A small DAG fits on one real answer page. Reopen the page after each publication,
/// including edits which rewrite observations while retaining the same reader edges.
struct DagPage<'b> {
    page: crate::pages::VerifiedPage<'b>,
    headers: BTreeMap<usize, crate::pages::IndexedHeaders<'b>>,
}

impl crate::StoredPages for DagPage<'_> {
    fn page(&self, _: Name) -> Result<Option<&crate::pages::VerifiedPage<'_>>, crate::pages::PagesError> {
        Ok(Some(&self.page))
    }

    fn header(&self, name: Name) -> Result<Option<crate::pages::Header>, crate::pages::PagesError> {
        Ok(match crate::pages::HeaderPages::slot(name) {
            Some(slot) => self.headers.get(&slot).and_then(|headers| headers.get(name)),
            None => crate::pages::Header::from_page(&self.page, name),
        })
    }

    fn finding(&self, _: Name) -> Option<u32> { None }

    fn refusal(&self) -> Option<crate::pages::PagesError> { None }

    fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), LastError>) -> Result<(), LastError> {
        self.page.row_names().try_for_each(visit)
    }
}

const SELECTORS: usize = 5;

fn dag_cell(of: Name, selector: usize) -> CellId {
    // WHOLE is zero. Fine selectors share their first fifteen bytes.
    let mut bytes = [0; 16];
    bytes[15] = selector as u8;
    CellId { of, cell: blaze_drive::cells::Cell(bytes) }
}

fn dag_body(i: usize) -> Name {
    Name::new(blaze_drive::Kind::BodyCheck, ContentId([i as u8; 32]))
}

fn dag_outline_source(revision: u8, values: &[ContentId; SELECTORS]) -> ContentId {
    let mut source = StdVec::from([revision]);
    for value in values {
        source.extend_from_slice(&value.0);
    }
    ContentId::of("DAG outline source", &source)
}

fn dag_body_source(revision: u8, selectors: &[usize]) -> ContentId {
    let mut source = StdVec::from([revision]);
    source.extend(selectors.iter().map(|&s| s as u8));
    ContentId::of("DAG body source", &source)
}

fn dag_request<'a>(
    last: Answer<'a>, files: &[(Name, &[Planned])], outline: Name, scope: Name,
    values: &[ContentId; SELECTORS], readers: &[StdVec<usize>],
) -> Result<crate::Finished<'a>, EngineError> {
    let tools = crate::Tools([blaze_drive::Tool(ContentId([7; 32])); blaze_drive::key::KINDS]);
    let mut engine = crate::Engine::new_in(tools, last, &crate::Edit { files }, crate::Everything, Plant::None, alloc())?;
    let mut batch = crate::Buffer::new_in(alloc());
    while engine.next_phase()?.is_some() {
        for task in engine.tasks() {
            let name = task.planned.name;
            let mut defines = Vec::new_in(alloc());
            let mut reads = Vec::new_in(alloc());
            let out = if name == outline {
                defines.push((dag_cell(outline, 0).cell, task.planned.source));
                // The outline's item digest can stand while the file digest moves.
                defines.push((dag_cell(outline, 1).cell, values[1]));
                task.planned.source
            } else if name == scope {
                let id = dag_cell(outline, 0);
                reads.push((id, engine.view().cell(&id)));
                defines.extend(values.iter().enumerate().map(|(i, &digest)| (dag_cell(scope, i).cell, digest)));
                values[0]
            } else {
                let i = usize::from(name.subject.0[0]);
                if i >= SELECTORS {
                    let id = dag_cell(outline, 1);
                    reads.push((id, engine.view().cell(&id)));
                }
                for &selector in &readers[i] {
                    let id = dag_cell(scope, selector);
                    reads.push((id, engine.view().cell(&id)));
                }
                name.subject
            };
            batch.done.push(crate::Done { name, key: task.key, out, reads, defines, findings: 0, failed: false });
        }
        engine.merge(core::slice::from_mut(&mut batch))?;
    }
    engine.finish()
}

fn paged_dag(tape: &Tape) -> Result<(), Broke> {
    use crate::pages::{HeaderPages, IndexedHeaders, Kept, Sealed, VerifiedPage, decode_root, encode, encode_changes};
    use crate::{Cause, Stage};

    let outline = Name::file(b"src/lib.rs");
    let scope = Name::new(blaze_drive::Kind::CrateScope, ContentId([6; 32]));
    let mut readers: StdVec<StdVec<usize>> = (0..SELECTORS).map(|s| StdVec::from([s])).collect();
    for _ in 0..1 + tape.index(5, "extra bodies") {
        // Reverse selector order to exercise the stored row's read-order index.
        readers.push((0..SELECTORS).rev().filter(|_| tape.one_in(2, "body reads selector")).collect());
    }
    let plan = |name, stage| Planned { name, phase: Phase::new(stage, 0), file: outline, source: ContentId([1; 32]) };
    let mut plans = StdVec::from([plan(outline, Stage::Outlines), plan(scope, Stage::Names)]);
    plans.extend(readers.iter().enumerate().map(|(i, selectors)| Planned {
        source: dag_body_source(0, selectors), ..plan(dag_body(i), Stage::Bodies)
    }));
    let mut values: [ContentId; SELECTORS] = core::array::from_fn(|i| ContentId([10 + i as u8; 32]));
    plans[0].source = dag_outline_source(0, &values);
    let cold = dag_request(Answer::new_in(alloc()), &[(outline, &plans)], outline, scope, &values, &readers)
        .map_err(|e| Broke::Harness(format!("cold DAG: {e}")))?;
    let mut memory = Kept::after(crate::SMap::new_in(alloc()), cold.answer, &cold.causes, crate::SMap::new_in(alloc()));
    let mut encoded = encode(&memory, None, alloc());
    let (mut header_keys, writes) = HeaderPages::encode(&memory, None, |_| Ok(None), alloc())
        .map_err(|e| Broke::Harness(format!("cold DAG headers: {e}")))?;
    let mut header_bytes: BTreeMap<_, _> = writes.into_iter().collect();
    let fine_only = 1 << (1 + tape.index(SELECTORS - 1, "one fine selector"));
    let random = tape.index(1 << SELECTORS, "changed selector mask");
    for (step, mask) in [0, fine_only, random, 1].into_iter().enumerate() {
        let old = values;
        let old_source = plans[0].source;
        for (i, digest) in values.iter_mut().enumerate() {
            if mask & (1 << i) != 0 {
                digest.0[31] ^= 1;
            }
        }
        plans[0].source = dag_outline_source(1 + step as u8, &values);
        // A prior edit can replace a body's read list while the names outputs stand.
        // Give that reader a changed source so its new memberships get published.
        if step == 0 {
            for (i, selectors) in readers.iter_mut().enumerate().skip(SELECTORS) {
                selectors.clear();
                selectors.extend((0..SELECTORS).rev().filter(|_| tape.one_in(2, "edited body reads selector")));
                plans[2 + i].source = dag_body_source(1, selectors);
            }
        }
        let files = [(outline, plans.as_slice())];
        let eager = dag_request(memory.answer, &files, outline, scope, &values, &readers)
            .map_err(|e| Broke::Harness(format!("in-memory DAG step {step}: {e}")))?;
        let mut expected = BTreeMap::from([
            (outline, Cause::KeyMoved),
            (scope, Cause::ReadMoved { read: dag_cell(outline, 0), was: old_source, now: plans[0].source }),
        ]);
        for (i, selectors) in readers.iter().enumerate() {
            if i >= SELECTORS && step == 0 {
                expected.insert(dag_body(i), Cause::KeyMoved);
            } else if i >= SELECTORS && old[1] != values[1] {
                expected.insert(dag_body(i), Cause::ReadMoved { read: dag_cell(outline, 1), was: old[1], now: values[1] });
            } else if let Some(&s) = selectors.iter().find(|&&s| old[s] != values[s]) {
                expected.insert(dag_body(i), Cause::ReadMoved { read: dag_cell(scope, s), was: old[s], now: values[s] });
            }
        }
        let actual: BTreeMap<_, _> = eager.causes.iter().map(|row| (row.name, row.cause)).collect();
        oracle(expected == actual && eager.entered.len() == expected.len(), || format!("DAG mask {mask:#b}: expected {expected:?}, memory {actual:?}"))?;
        let scope_entry = eager.causes.iter().find(|row| row.name == scope);
        oracle(scope_entry.is_some_and(|row| row.identical == (old[0] == values[0])), || {
            format!("DAG mask {mask:#b}: scope output {:?} -> {:?}, entry {scope_entry:?}", old[0], values[0])
        })?;
        let (next, (next_headers, writes)) = {
            let bytes = |key| encoded.puts.iter().find(|(k, _)| *k == key).expect("this edit rewrites the single page and root").1.as_slice();
            // Each record hashed against its key (no store proves them here), then each page and
            // shard walked whole: the simulation reads in verify mode.
            fn sealed(key: Key, bytes: &[u8]) -> Result<Sealed<'_>, Broke> {
                Sealed::hashed(key, bytes).map_err(|torn| Broke::Oracle(format!("DAG record {key:?}: {torn:?}")))
            }
            let root = decode_root(sealed(encoded.root, bytes(encoded.root))?, alloc()).map_err(|e| Broke::Oracle(format!("DAG root: {e}")))?;
            assert_eq!(root.bits, 0, "the small DAG fits on one page");
            let key = root.pages[0];
            let page = VerifiedPage::open_whole(sealed(key, bytes(key))?, 0, root.bits).map_err(|e| Broke::Oracle(format!("DAG page: {e}")))?;
            let mut headers = BTreeMap::new();
            for (slot, &key) in header_keys.keys.iter().enumerate().filter(|(_, key)| **key != Key::default()) {
                let shard = IndexedHeaders::open_whole(sealed(key, &header_bytes[&key])?, slot).map_err(|e| Broke::Oracle(format!("DAG header {slot}: {e}")))?;
                headers.insert(slot, shard);
            }
            let stored = DagPage { page, headers };
            for (&name, &cause) in memory.causes.iter() {
                let saved = stored.page.cause(name);
                oracle(saved == Some(cause), || format!("before DAG step {step}: {name:?} expected {cause:?}, page {saved:?}"))?;
            }
            let view = dag_request(Answer::from_pages(&stored, root.serial, alloc()), &files, outline, scope, &values, &readers)
                .map_err(|e| Broke::Oracle(format!("paged DAG step {step}: {e}")))?;
            oracle(eager.entered[..] == view.entered[..] && eager.causes[..] == view.causes[..], || {
                format!("DAG mask {mask:#b}: memory {:?}, page {:?}", &eager.causes[..], &view.causes[..])
            })?;
            let changed = Kept::after(crate::SMap::new_in(alloc()), view.answer, &view.causes, crate::SMap::new_in(alloc()));
            let next = encode_changes(&changed, &root, alloc(), |k| {
                encoded.puts.iter().find(|(pk, _)| *pk == k).map(|(_, b)| Sealed::hashed(k, b))
            })
            .map_err(|e| Broke::Oracle(format!("publish DAG step {step}: {e}")))?;
            let headers = HeaderPages::encode(&changed, Some(&header_keys), |slot| Ok(stored.headers.get(&slot)), alloc())
                .map_err(|e| Broke::Oracle(format!("publish DAG headers at step {step}: {e}")))?;
            (next, headers)
        };
        memory = Kept::after(memory.causes, eager.answer, &eager.causes, memory.records);
        let expected_root = encode(&memory, None, alloc()).root;
        oracle(next.root == expected_root, || {
            format!("after DAG step {step}, mask {mask:#b}: in-memory root {expected_root:?}, published {:?}", next.root)
        })?;
        encoded = next;
        header_keys = next_headers;
        header_bytes.extend(writes);
    }
    Ok(())
}

/// Compare task entry and each cause, including the selector and both complete digests,
/// over both the general history and page-backed re-runs with independent cell changes.
#[test]
fn stored_view_and_memory_enter_the_same_random_dag() {
    check(SimConfig::cases(500), |tape| twin(tape, Plant::None));
    check(SimConfig::cases(500), paged_dag);
}

/// The planted bug: the overlay answers a cell's readers ignoring this request's removals. A row
/// that stopped reading a made record keeps it alive, or a dropped reader is entered again.
#[test]
fn control_stale_readers_are_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::ReadersStale, "", twin);
}

/// One crate of 134 files, seven items each (sig `id % 4`: no derive; body 1: no finding, no
/// constant; no references): 2,011 tasks. One body edit enters its outline and its body, and asks
/// the last answer for a twentieth of its rows or fewer.
/// Expected: "rows touched T of 2011" with T below 100.
#[test]
fn rows_touched_by_one_edit() {
    let mut next = 0u32;
    let mut files: StdVec<(usize, StdVec<Item>)> = StdVec::new();
    for _ in 0..134 {
        let mut items = StdVec::new();
        for _ in 0..7 {
            items.push(Item { id: next, sig: next % 4, body: 1, refs: StdVec::new() });
            next += 1;
        }
        files.push((0, items));
    }
    let world = World { crates: StdVec::from([StdVec::new()]), files, next_id: next };
    let scope = Crates::of(&world, BTreeSet::from([0]));
    let plans = world.plans();
    let first = match request(&world, Answer::new_in(alloc()), &plans, scope.clone(), Plant::None, Pools::One) {
        Ok(first) => first,
        Err(error) => panic!("the first build failed: {error}"),
    };
    let total = first.answer.rows.len();
    let mut edited = world.clone();
    edited.files[0].1[0].body = 2;
    let next_plans = edited.plans();
    let changed: BTreeMap<Name, StdVec<Planned>> =
        next_plans.iter().filter(|(name, plan)| plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
    let after = match request(&edited, SimLast::of(&first.answer), &changed, scope, Plant::None, Pools::One) {
        Ok(after) => after,
        Err(error) => panic!("the edit failed: {error}"),
    };
    let touched = after.touched.rows;
    println!("rows touched {touched} of {total} (entered {}, reads walked {}, readers {})", after.report.entered, after.touched.reads, after.touched.readers);
    assert_eq!(after.report.entered, 2, "one body edit enters its outline and its body: {:?}", &after.entered[..]);
    assert!(touched * 20 < total as u64, "rows touched {touched} of {total}: one edit asked for a twentieth of the answer or more");
}

/// Growing from a tiny `bits = 0` base (like `tool-base`) to a large workspace must re-page
/// inside `encode_changes` and produce the exact same root as a full `encode`.
#[test]
fn encode_changes_repages_when_growing_from_small_base() {
    use crate::Stage;
    use crate::pages::{Kept, Sealed, VerifiedPage, decode_root, encode, encode_changes};

    struct SmallBase<'b> {
        page: VerifiedPage<'b>,
    }
    impl crate::StoredPages for SmallBase<'_> {
        fn page(&self, _: Name) -> Result<Option<&VerifiedPage<'_>>, crate::pages::PagesError> {
            Ok(Some(&self.page))
        }
        fn finding(&self, _: Name) -> Option<u32> { None }
        fn refusal(&self) -> Option<crate::pages::PagesError> { None }
        fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), LastError>) -> Result<(), LastError> {
            self.page.row_names().try_for_each(visit)
        }
    }

    let outline = Name::file(b"src/lib.rs");
    let scope = Name::new(blaze_drive::Kind::CrateScope, ContentId([6; 32]));
    let values = [ContentId([1; 32]), ContentId([2; 32]), ContentId([3; 32]), ContentId([4; 32]), ContentId([5; 32])];
    let plan = |name, stage| Planned { name, phase: Phase::new(stage, 0), file: outline, source: ContentId([1; 32]) };
    let small_readers: StdVec<StdVec<usize>> = StdVec::from([StdVec::from([0])]);
    let mut small_plans = StdVec::from([plan(outline, Stage::Outlines), plan(scope, Stage::Names)]);
    small_plans[0].source = dag_outline_source(0, &values);
    small_plans.push(Planned { source: dag_body_source(0, &small_readers[0]), ..plan(dag_body(0), Stage::Bodies) });
    let base_res = dag_request(Answer::new_in(alloc()), &[(outline, &small_plans)], outline, scope, &values, &small_readers).expect("small base");
    let base_kept = Kept::after(crate::SMap::new_in(alloc()), base_res.answer, &base_res.causes, crate::SMap::new_in(alloc()));
    let base_encoded = encode(&base_kept, None, alloc());
    assert_eq!(base_encoded.bits, 0, "small base fits on one page");
    let mut base_bytes: crate::SMap<'_, Key, crate::SVec<'_, u8>> = crate::SMap::new_in(alloc());
    for (k, v) in base_encoded.puts.iter() {
        base_bytes.insert(*k, crate::SVec::from_slice_in(v, alloc()));
    }
    let base_root = decode_root(Sealed::hashed(base_encoded.root, &base_bytes[&base_encoded.root]).expect("base root"), alloc()).expect("decode base root");
    let base_page_key = base_root.pages[0];

    // Grow to 220 bodies (each reading all 5 selectors -> > 64 KiB -> bits >= 1).
    let mut big_readers: StdVec<StdVec<usize>> = StdVec::from([StdVec::from([0])]);
    let mut big_plans = small_plans;
    for i in 1..220 {
        let sels = StdVec::from([4, 3, 2, 1, 0]);
        big_plans.push(Planned { source: dag_body_source(0, &sels), ..plan(dag_body(i), Stage::Bodies) });
        big_readers.push(sels);
    }
    let big_files = [(outline, big_plans.as_slice())];
    let eager = dag_request(base_kept.answer, &big_files, outline, scope, &values, &big_readers).expect("eager big");
    let repaged = {
        let base_page = VerifiedPage::open(Sealed::hashed(base_page_key, &base_bytes[&base_page_key]).expect("base page"), 0, 0).expect("open base page");
        let stored = SmallBase { page: base_page };
        let paged = dag_request(Answer::from_pages(&stored, base_root.serial, alloc()), &big_files, outline, scope, &values, &big_readers).expect("paged big");
        assert_eq!(&eager.entered[..], &paged.entered[..], "same tasks entered on paged vs in-memory base");
        let paged_kept = Kept::after(crate::SMap::new_in(alloc()), paged.answer, &paged.causes, crate::SMap::new_in(alloc()));
        encode_changes(&paged_kept, &base_root, alloc(), |k| base_bytes.get(&k).map(|b| Sealed::hashed(k, b))).expect("repage encode_changes")
    };
    let mem_kept = Kept::after(base_kept.causes, eager.answer, &eager.causes, base_kept.records);
    let expected = encode(&mem_kept, Some(&base_root), alloc());

    assert!(repaged.bits > base_root.bits, "growing from tiny base must increase bits: {} -> {}", base_root.bits, repaged.bits);
    assert_eq!(repaged.bits, expected.bits, "repaged bits match full encode");
    assert_eq!(repaged.root, expected.root, "repaged root is byte-for-byte identical to full encode");
    println!("repaged {} -> {} bits ({} bytes, {} pages), root matches full encode", base_root.bits, repaged.bits, repaged.bytes, repaged.pages.len());
}
