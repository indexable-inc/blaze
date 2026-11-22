//! The answer's simulation (rules 43, 64): random requests (rows made, re-made and retired, cells
//! moved and moved back, readers added and removed, compactions, crashes between the segments and
//! the root) are committed to a store of bytes and applied to a plain in-memory model; after every
//! step the views over the stored bytes equal the model (rows, cells, readers, and the plans,
//! findings, owed tasks and owed cells columns). The controls plant one bug at a time. The size
//! test prints what an answer of 24,000 rows costs.

use std::collections::BTreeMap;
use std::format;
use std::println;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::{Alloc, SharedAlloc};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::{
    AnswerError, AnswerView, CellDigest, CellNo, Commit, DEFINE_BYTES, Effects, Flags, Health, LinesKey, Plant, ROW_FIXED_BYTES, RowIn, RunWriter, SegmentKind, Serial, TaskId, TreeDigest, Write,
    compact, has_findings, nothing_changed, summary_of,
};

/// The simulation's one heap: what [`alloc`] and [`shared`] hand out.
fn heap() -> &'static blaze_own::Heap {
    blaze_own::testing::heap()
}

fn alloc() -> Alloc<'static> {
    heap().alloc()
}

/// The allocator a view's own tables take: shared, so a view is `Sync`.
fn shared() -> SharedAlloc<'static> {
    heap().shared()
}

fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

fn refused(what: &'static str) -> impl FnOnce(AnswerError) -> Broke {
    move |error| Broke::Oracle(format!("{what}: {error}"))
}

const ABSENT: [u8; 32] = [0; 32];

// ---------------------------------------------------------------------------------------------
// The universe: twelve bodies, each defining up to three cells, two files.

const BODIES: usize = 12;
const TAGS: usize = 3;

fn body(i: usize) -> Name {
    Name { kind: Kind::BodyCheck, subject: ContentId([i as u8 + 1; 32]) }
}

fn file(i: usize) -> Name {
    Name { kind: Kind::FileItems, subject: ContentId([i as u8 + 101; 32]) }
}

fn tag(t: usize) -> Cell {
    Cell([t as u8 + 1; 16])
}

// Every generated value shares its first twenty bytes with absence. A suffix-only change
// must move a cell, including a definition removed and introduced again after a restart.
fn digest(v: u64) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[20..24].copy_from_slice(&[0x31, 0xa9, 0x72, 0xe3]);
    bytes[24..].copy_from_slice(&v.to_be_bytes());
    bytes
}

#[derive(Clone, Debug)]
struct Draft {
    file: usize,
    stage: u8,
    wave: u16,
    flags: u8,
    key: u8,
    out: u8,
    source: u8,
    findings: u32,
    record: u8,
    /// (definer body, tag), in read order.
    reads: StdVec<(usize, usize)>,
    /// (tag, digest value).
    defines: StdVec<(usize, u64)>,
}

#[derive(Clone, Debug)]
enum Op {
    Put { task: usize, row: Draft },
    Retire { task: usize },
}

#[derive(Clone, Debug)]
struct Request {
    ops: StdVec<Op>,
    tree: [u8; 32],
    lines: [u8; 32],
    summary: StdVec<u8>,
    /// The owed cells, whole (cell numbers given before the request, each with a serial), or
    /// `None`: the last set stands.
    owed_cells: Option<StdVec<(u32, u64)>>,
}

fn row_in(d: &Draft, file: TaskId) -> RowIn {
    RowIn {
        key: Key(ContentId([d.key; 32])),
        out: ContentId([d.out; 32]),
        file,
        stage: d.stage,
        wave: d.wave,
        flags: Flags(d.flags),
        source: ContentId([d.source; 32]),
        findings: d.findings,
        record: Key(ContentId([d.record; 32])),
    }
}

fn draft(reads: StdVec<(usize, usize)>, defines: StdVec<(usize, u64)>) -> Draft {
    Draft { file: 0, stage: 5, wave: 0, flags: 0, key: 1, out: 1, source: 1, findings: 0, record: 1, reads, defines }
}

fn draw_request(tape: &Tape, model: &Model, serial: u64) -> Request {
    let n = 1 + tape.below(6, "ops") as usize;
    let mut tasks: StdVec<usize> = (0..BODIES).collect();
    tape.shuffle(&mut tasks, "tasks");
    tasks.truncate(n);
    let mut ops = StdVec::new();
    for task in tasks {
        let has = model.task_of(&body(task)).is_some_and(|id| model.rows.contains_key(&id));
        if has && tape.one_in(4, "retire") {
            ops.push(Op::Retire { task });
            continue;
        }
        let failed = tape.one_in(6, "failed");
        let mut flags = if failed { Flags::FAILED } else { 0 };
        if tape.one_in(8, "owed") {
            flags |= Flags::OWED;
        }
        if tape.one_in(8, "inline") {
            flags |= Flags::INLINE;
        }
        let reads = (0..tape.below(9, "reads")).map(|_| (tape.index(BODIES, "read of"), tape.index(TAGS, "read tag"))).collect();
        let mut defines = StdVec::new();
        if !failed {
            for t in 0..TAGS {
                if tape.coin("defines") {
                    defines.push((t, 1 + tape.below(2, "digest")));
                }
            }
        }
        let row = Draft {
            file: tape.index(2, "file"),
            stage: tape.below(6, "stage") as u8,
            wave: tape.below(4, "wave") as u16,
            flags,
            key: tape.below(250, "key") as u8,
            out: tape.below(250, "out") as u8,
            source: tape.below(250, "source") as u8,
            findings: tape.below(3, "findings") as u32,
            record: 1 + tape.below(250, "record") as u8,
            reads,
            defines,
        };
        ops.push(Op::Put { task, row });
    }
    let mut tree = [0u8; 32];
    tree[..8].copy_from_slice(&serial.to_le_bytes());
    tree[8] = tape.below(4, "tree") as u8;
    let mut lines = [0u8; 32];
    lines[0] = tape.below(256, "lines") as u8;
    let summary = (0..tape.below(9, "summary")).map(|i| i as u8).collect();
    let owed_cells = match model.cells.is_empty() || !tape.one_in(3, "owe cells") {
        true => None,
        false => {
            let mut set: BTreeMap<u32, u64> = BTreeMap::new();
            for _ in 0..tape.below(4, "owed cells") {
                set.insert(tape.index(model.cells.len(), "owed cell") as u32, 1 + tape.below(serial, "owed at"));
            }
            Some(set.into_iter().collect())
        }
    };
    Request { ops, tree, lines, summary, owed_cells }
}

// ---------------------------------------------------------------------------------------------
// The model: the answer as plain maps.

#[derive(Clone, Debug)]
struct MRow {
    head: RowIn,
    serial: u64,
    reads: StdVec<u32>,
    defines: BTreeMap<u32, [u8; 32]>,
    /// The digests of its reads when it was made: a standing row's must be today's.
    saw: StdVec<[u8; 32]>,
}

#[derive(Clone, Default, Debug)]
struct Model {
    serial: u64,
    tree: [u8; 32],
    lines: [u8; 32],
    summary: StdVec<u8>,
    tasks: StdVec<Name>,
    cells: StdVec<(u32, [u8; 16])>,
    rows: BTreeMap<u32, MRow>,
    moved: BTreeMap<u32, u64>,
    owed_cells: BTreeMap<u32, u64>,
}

impl Model {
    /// The tasks `file`'s plan lists: its rows not made inline.
    fn plan(&self, file: u32) -> StdVec<u32> {
        self.rows.iter().filter(|(_, row)| row.head.file.0 == file && row.head.flags.0 & Flags::INLINE == 0).map(|(task, _)| *task).collect()
    }

    fn with_findings(&self) -> StdVec<u32> {
        self.rows.iter().filter(|(_, row)| has_findings(row.head.findings, row.head.flags)).map(|(task, _)| *task).collect()
    }

    fn owed(&self) -> StdVec<u32> {
        self.rows.iter().filter(|(_, row)| row.head.flags.0 & Flags::OWED != 0).map(|(task, _)| *task).collect()
    }

    fn task_of(&self, name: &Name) -> Option<u32> {
        self.tasks.iter().position(|one| one == name).map(|at| at as u32)
    }

    fn task(&mut self, name: Name) -> u32 {
        match self.task_of(&name) {
            Some(id) => id,
            None => {
                self.tasks.push(name);
                self.tasks.len() as u32 - 1
            }
        }
    }

    fn cell(&mut self, definer: u32, cell: [u8; 16]) -> u32 {
        match self.cells.iter().position(|one| *one == (definer, cell)) {
            Some(at) => at as u32,
            None => {
                self.cells.push((definer, cell));
                self.cells.len() as u32 - 1
            }
        }
    }

    fn digest(&self, cell: u32) -> [u8; 32] {
        let (definer, _) = self.cells[cell as usize];
        self.rows.get(&definer).and_then(|row| row.defines.get(&cell)).copied().unwrap_or(ABSENT)
    }

    fn moved(&self, cell: u32) -> u64 {
        self.moved.get(&cell).copied().unwrap_or(0)
    }

    fn stands(&self, task: u32) -> Option<bool> {
        let row = self.rows.get(&task)?;
        Some(row.reads.iter().all(|cell| self.moved(*cell) <= row.serial))
    }

    fn readers(&self, cell: u32) -> StdVec<u32> {
        self.rows.iter().filter(|(_, row)| row.reads.contains(&cell)).map(|(task, _)| *task).collect()
    }

    fn apply(&mut self, req: &Request) {
        let serial = self.serial + 1;
        let mut made = StdVec::new();
        for op in req.ops.iter() {
            match op {
                Op::Retire { task } => {
                    let id = self.task(body(*task));
                    let old = self.rows.remove(&id).expect("a retire is drawn only for a row");
                    for (cell, was) in old.defines {
                        if was != ABSENT {
                            self.moved.insert(cell, serial);
                        }
                    }
                }
                Op::Put { task, row } => {
                    let id = self.task(body(*task));
                    let file = self.task(file(row.file));
                    let mut reads = StdVec::new();
                    for (of, t) in row.reads.iter() {
                        let definer = self.task(body(*of));
                        reads.push(self.cell(definer, tag(*t).0));
                    }
                    let mut defines = BTreeMap::new();
                    for (t, v) in row.defines.iter() {
                        defines.insert(self.cell(id, tag(*t).0), digest(*v));
                    }
                    let old = self.rows.get(&id).map(|row| row.defines.clone()).unwrap_or_default();
                    for (cell, now) in defines.iter() {
                        if old.get(cell).copied().unwrap_or(ABSENT) != *now {
                            self.moved.insert(*cell, serial);
                        }
                    }
                    for (cell, was) in old.iter() {
                        if *was != ABSENT && !defines.contains_key(cell) {
                            self.moved.insert(*cell, serial);
                        }
                    }
                    let head = row_in(row, TaskId(file));
                    self.rows.insert(id, MRow { head, serial, reads, defines, saw: StdVec::new() });
                    made.push(id);
                }
            }
        }
        for id in made {
            let saw = self.rows[&id].reads.iter().map(|cell| self.digest(*cell)).collect();
            self.rows.get_mut(&id).expect("made above").saw = saw;
        }
        if let Some(cells) = &req.owed_cells {
            self.owed_cells = cells.iter().copied().collect();
        }
        self.serial = serial;
        self.tree = req.tree;
        self.lines = req.lines;
        self.summary = req.summary.clone();
    }
}

// ---------------------------------------------------------------------------------------------
// The store: segments by id, and one root.

#[derive(Default)]
struct Store {
    segments: BTreeMap<[u8; 32], StdVec<u8>>,
    root: Option<StdVec<u8>>,
}

impl Store {
    fn open(&self, plant: Plant) -> Result<AnswerView<'_>, AnswerError> {
        match &self.root {
            None => Ok(AnswerView::empty(plant, shared())),
            Some(root) => AnswerView::open(root, |at| self.segments.get(&at.id.0).map(|bytes| &bytes[..]), plant, shared()),
        }
    }

    /// The first `upto` of the writes, in their order; a crash loses the rest.
    fn write(&mut self, effects: &Effects<'_>, upto: usize) {
        for write in effects.writes().iter().take(upto) {
            match write {
                Write::Segment { at, bytes } => {
                    self.segments.insert(at.id.0, StdVec::from(&bytes[..]));
                }
                Write::Root { bytes } => self.root = Some(StdVec::from(&bytes[..])),
            }
        }
    }
}

fn same_writes(a: &Effects<'_>, b: &Effects<'_>) -> bool {
    a.writes().len() == b.writes().len()
        && a.writes().iter().zip(b.writes()).all(|pair| match pair {
            (Write::Segment { at: x, bytes: p }, Write::Segment { at: y, bytes: q }) => x == y && p[..] == q[..],
            (Write::Root { bytes: p }, Write::Root { bytes: q }) => p[..] == q[..],
            _ => false,
        })
}

/// The request as the engine would make it: ids on the coordinator, each op's reverse entries on
/// the worker `split` gives it.
fn commit_request(view: &AnswerView<'_>, req: &Request, split: &[usize], plant: Plant) -> Result<Effects<'static>, AnswerError> {
    let mut commit = Commit::new(view, plant, alloc());
    let n = split.iter().max().map_or(1, |most| most + 1);
    let mut workers: StdVec<RunWriter<'static>> = (0..n).map(|_| RunWriter::new(commit.serial(), alloc())).collect();
    for (op, worker) in req.ops.iter().zip(split) {
        let worker = &mut workers[*worker];
        match op {
            Op::Retire { task } => {
                let id = commit.task(body(*task))?;
                commit.retire(id)?;
                if let Some(old) = view.row(id) {
                    worker.row_retired(id, old);
                }
            }
            Op::Put { task, row } => {
                let id = commit.task(body(*task))?;
                let file = commit.task(file(row.file))?;
                let mut reads = StdVec::new();
                for (of, t) in row.reads.iter() {
                    let definer = commit.task(body(*of))?;
                    reads.push(commit.cell(definer, tag(*t))?);
                }
                let mut defines = StdVec::new();
                for (t, v) in row.defines.iter() {
                    let mut bytes = digest(*v);
                    if plant == Plant::PrefixPadded {
                        bytes[20..].fill(0);
                    }
                    defines.push((commit.cell(id, tag(*t))?, CellDigest::of(&ContentId(bytes))));
                }
                commit.put_row(id, &row_in(row, file), &reads, &defines)?;
                worker.row_reads(id, view.row(id), &reads);
            }
        }
    }
    for worker in workers {
        if let Some(run) = worker.finish()? {
            commit.add_run(&run)?;
        }
    }
    if let Some(cells) = &req.owed_cells {
        let given: StdVec<(CellNo, Serial)> = cells.iter().map(|(cell, at)| (CellNo(*cell), Serial(*at))).collect();
        commit.owe_cells(&given)?;
    }
    commit.finish(&TreeDigest(req.tree), &req.summary, &LinesKey(req.lines))
}

// ---------------------------------------------------------------------------------------------
// The oracle.

fn compare(view: &AnswerView<'_>, model: &Model, at: &str) -> Result<(), Broke> {
    view.verify().map_err(refused("the answer's keys are out of order"))?;
    oracle(view.serial().0 == model.serial, || format!("{at}: serial {} is not the model's {}", view.serial().0, model.serial))?;
    oracle(view.tree().0 == model.tree && view.lines().0 == model.lines && view.summary() == &model.summary[..], || format!("{at}: the root's tree, lines or summary is not the model's"))?;
    oracle(view.tasks() as usize == model.tasks.len(), || format!("{at}: {} task ids, the model has {}", view.tasks(), model.tasks.len()))?;
    for (i, name) in model.tasks.iter().enumerate() {
        let task = TaskId(i as u32);
        oracle(view.name(task) == Some(*name) && view.task(name) == Some(task), || format!("{at}: task {i}'s name does not map both ways"))?;
    }
    oracle(view.cells() as usize == model.cells.len(), || format!("{at}: {} cells, the model has {}", view.cells(), model.cells.len()))?;
    for (i, (definer, cell)) in model.cells.iter().enumerate() {
        let no = CellNo(i as u32);
        oracle(view.cell_key(no) == Some((TaskId(*definer), Cell(*cell))) && view.cell_of(TaskId(*definer), &Cell(*cell)) == Some(no), || format!("{at}: cell {i} does not map both ways"))?;
    }
    for t in 0..model.tasks.len() as u32 {
        let task = TaskId(t);
        match (view.row(task), model.rows.get(&t)) {
            (None, None) => {}
            (Some(row), Some(m)) => {
                let head = RowIn {
                    key: row.key(),
                    out: row.out(),
                    file: row.file(),
                    stage: row.stage(),
                    wave: row.wave(),
                    flags: row.flags(),
                    source: row.source(),
                    findings: row.findings(),
                    record: row.record(),
                };
                oracle(head == m.head && row.serial().0 == m.serial, || format!("{at}: row of task {t}: {head:?} at {}, the model's {:?} at {}", row.serial().0, m.head, m.serial))?;
                let reads: StdVec<u32> = row.reads().map(|cell| cell.0).collect();
                oracle(reads == m.reads, || format!("{at}: row of task {t} reads {reads:?}, the model's {:?}", m.reads))?;
                // One read found by search, not by a walk (`AnswerView::reads_cell`, plan row B1):
                // every cell asked, the row's reads alone answer yes.
                for c in 0..model.cells.len() as u32 {
                    let (found, _) = view.reads_cell(CellNo(c), task).map_err(refused("a read found by search"))?;
                    oracle(found == m.reads.contains(&c), || format!("{at}: task {t}: reads_cell({c}) is {found}, the model's reads {:?}", m.reads))?;
                }
                let defines: StdVec<(u32, [u8; 32])> = row.defines().map(|(cell, d)| (cell.0, d.0)).collect();
                let want: StdVec<(u32, [u8; 32])> = m.defines.iter().map(|(cell, d)| (*cell, *d)).collect();
                oracle(defines == want, || format!("{at}: row of task {t} defines {defines:?}, the model's {want:?}"))?;
            }
            (Some(_), None) => return Err(Broke::Oracle(format!("{at}: task {t} has a row, the model none (retired)"))),
            (None, Some(_)) => return Err(Broke::Oracle(format!("{at}: task {t} has no row, the model has one"))),
        }
        oracle(view.stands(task) == model.stands(t), || format!("{at}: validity of task {t}: {:?}, the model's {:?}", view.stands(task), model.stands(t)))?;
        if model.stands(t) == Some(true) {
            let row = &model.rows[&t];
            let today: StdVec<[u8; 32]> = row.reads.iter().map(|cell| model.digest(*cell)).collect();
            oracle(today == row.saw, || format!("{at}: the model's serial rule says task {t} stands but a read's digest changed"))?;
        }
    }
    for c in 0..model.cells.len() as u32 {
        let cell = CellNo(c);
        oracle(view.moved(cell).0 == model.moved(c), || format!("{at}: cell {c} last moved at {}, the model's {}", view.moved(cell).0, model.moved(c)))?;
        oracle(view.digest(cell).0 == model.digest(c), || format!("{at}: cell {c}'s digest is not the model's"))?;
        let readers: StdVec<u32> = view.readers(cell, alloc()).map_err(refused("readers"))?.iter().map(|task| task.0).collect();
        let want = model.readers(c);
        oracle(readers == want, || format!("{at}: readers of cell {c}: {readers:?}, the model's {want:?}"))?;
    }
    // The columns of kinds 5 to 8: each against the model's derivation.
    for t in 0..model.tasks.len() as u32 {
        let plan: StdVec<u32> = view.file_tasks(TaskId(t), alloc()).map_err(refused("a file's plan"))?.iter().map(|task| task.0).collect();
        let want = model.plan(t);
        oracle(plan == want, || format!("{at}: plan of file {t}: {plan:?}, the model's {want:?}"))?;
    }
    let findings: StdVec<u32> = view.with_findings(alloc()).map_err(refused("the rows with findings"))?.iter().map(|task| task.0).collect();
    let want = model.with_findings();
    oracle(findings == want, || format!("{at}: rows with findings {findings:?}, the model's {want:?}"))?;
    let owed: StdVec<u32> = view.owed().map(|task| task.0).collect();
    let want = model.owed();
    oracle(owed == want, || format!("{at}: owed tasks {owed:?}, the model's {want:?}"))?;
    for t in 0..model.tasks.len() as u32 {
        oracle(view.is_owed(TaskId(t)) == want.contains(&t), || format!("{at}: owed tasks: is_owed({t}) disagrees with the list"))?;
    }
    let owed_cells: StdVec<(u32, u64)> = view.owed_cells().map(|(cell, s)| (cell.0, s.0)).collect();
    let want: StdVec<(u32, u64)> = model.owed_cells.iter().map(|(cell, s)| (*cell, *s)).collect();
    oracle(owed_cells == want, || format!("{at}: owed cells {owed_cells:?}, the model's {want:?}"))?;
    Ok(())
}

/// What open and adopt say of the store's root next to the model's tree.
fn check_root(store: &Store, model: &Model, plant: Plant, at: &str) -> Result<(), Broke> {
    let Some(root) = &store.root else { return Ok(()) };
    let mut other = model.tree;
    other[31] ^= 1;
    let same = nothing_changed(root, &TreeDigest(model.tree)).map_err(refused("nothing_changed"))?;
    let changed = nothing_changed(root, &TreeDigest(other)).map_err(refused("nothing_changed"))?;
    oracle(same && !changed, || format!("{at}: nothing_changed is wrong about the root's tree"))?;
    let adopted = AnswerView::adopt(&TreeDigest(model.tree), root, |one| store.segments.get(&one.id.0).map(|bytes| &bytes[..]), plant, shared()).map_err(refused("adopt"))?;
    compare(&adopted, model, at)?;
    // The root alone gives the summary the opened answer gives (blaze-cli's certificate reads it so).
    oracle(summary_of(root).ok() == Some(adopted.summary()), || format!("{at}: summary_of disagrees with the opened root's summary"))?;
    let wrong = AnswerView::adopt(&TreeDigest(other), root, |_| panic!("opened a child of another tree"), plant, shared());
    oracle(matches!(wrong, Err(AnswerError::TreeMismatch)), || format!("{at}: adopt took a root of another tree"))
}

fn compact_store(store: &mut Store, model: &Model, plant: Plant, at: &str) -> Result<(), Broke> {
    let effects = {
        let view = store.open(plant).map_err(refused("open before compaction"))?;
        let effects = compact(&view, plant, alloc()).map_err(refused("compact"))?;
        let again = compact(&view, plant, alloc()).map_err(refused("compact"))?;
        oracle(same_writes(&effects, &again), || format!("{at}: two compactions of one answer wrote different bytes"))?;
        effects
    };
    let all = effects.writes().len();
    store.write(&effects, all);
    let view = store.open(plant).map_err(refused("open after compaction"))?;
    oracle(view.segments().iter().filter(|one| one.kind == SegmentKind::Run).count() <= 1, || format!("{at}: compaction left more than one run"))?;
    compare(&view, model, at)
}

/// One committed request (or a crash in its middle), then the checks.
fn step(tape: &Tape, store: &mut Store, model: &mut Model, req: &Request, plant: Plant, at: &str) -> Result<(), Broke> {
    let n = 1 + tape.below(3, "workers") as usize;
    let split: StdVec<usize> = req.ops.iter().map(|_| tape.index(n, "worker")).collect();
    let other: StdVec<usize> = req.ops.iter().map(|_| tape.index(n, "other worker")).collect();
    let effects = {
        let view = store.open(plant).map_err(refused("open"))?;
        // A history is at most 16 requests of at most eight segments (three runs) each, and compacts when
        // health says so: compaction owed here is a broken cap, not a state to handle.
        let effects = commit_request(&view, req, &split, plant).map_err(refused("commit"))?;
        let again = commit_request(&view, req, &other, plant).map_err(refused("commit"))?;
        oracle(same_writes(&effects, &again), || format!("{at}: two worker splits of one request wrote different bytes (rule 25)"))?;
        effects
    };
    finish_step(tape, store, model, req, effects, plant, at)
}

fn finish_step(tape: &Tape, store: &mut Store, model: &mut Model, req: &Request, effects: Effects<'static>, plant: Plant, at: &str) -> Result<(), Broke> {
    let all = effects.writes().len();
    if all > 1 && tape.one_in(5, "crash") {
        let upto = 1 + tape.index(all - 1, "crash after");
        store.write(&effects, upto);
        let view = store.open(plant).map_err(refused("after a crash between the segments and the root, the last root"))?;
        compare(&view, model, at)?;
        return check_root(store, model, plant, at);
    }
    store.write(&effects, all);
    model.apply(req);
    let view = store.open(plant).map_err(refused("open after a commit"))?;
    compare(&view, model, at)?;
    let due = view.health() != Health::Fine;
    drop(view);
    check_root(store, model, plant, at)?;
    if due || tape.one_in(4, "compact") {
        compact_store(store, model, plant, at)?;
        check_root(store, model, plant, at)?;
    }
    Ok(())
}

fn histories(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut store = Store::default();
    let mut model = Model::default();
    compare(&store.open(plant).map_err(refused("open empty"))?, &model, "empty")?;
    let steps = 1 + tape.below(16, "steps");
    for i in 0..steps {
        let req = draw_request(tape, &model, model.serial + 1);
        let at = format!("request {i} (serial {})", model.serial + 1);
        tape.note(format_args!("{at}: {req:?}"));
        step(tape, &mut store, &mut model, &req, plant, &at)?;
    }
    Ok(())
}

/// Plant (c), constructed: D defines X = A at 1; at 2 D makes X = B and R reads X; at 3 D makes
/// X = A again. X moved at 3, after R was made: R must not stand, though X's digest is the one
/// D's first row held. Removal at 4 and reintroduction at 5 each move X once more.
fn move_back(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let t = tape.index(TAGS, "tag");
    let a = 1 + tape.below(2, "a");
    let b = 3 - a;
    let request = |ops: StdVec<Op>, serial: u64| Request { ops, tree: [serial as u8; 32], lines: [0; 32], summary: StdVec::new(), owed_cells: None };
    let history = [
        request(StdVec::from([Op::Put { task: 0, row: draft(StdVec::new(), StdVec::from([(t, a)])) }]), 1),
        request(StdVec::from([Op::Put { task: 0, row: draft(StdVec::new(), StdVec::from([(t, b)])) }, Op::Put { task: 1, row: draft(StdVec::from([(0, t)]), StdVec::new()) }]), 2),
        request(StdVec::from([Op::Put { task: 0, row: draft(StdVec::new(), StdVec::from([(t, a)])) }]), 3),
        request(StdVec::from([Op::Retire { task: 0 }]), 4),
        request(StdVec::from([Op::Put { task: 0, row: draft(StdVec::new(), StdVec::from([(t, a)])) }]), 5),
    ];
    let mut store = Store::default();
    let mut model = Model::default();
    for (i, req) in history.iter().enumerate() {
        let at = format!("move back, request {}", i + 1);
        let effects = {
            let view = store.open(plant).map_err(refused("open"))?;
            commit_request(&view, req, &[0, 0], plant).map_err(refused("commit"))?
        };
        let all = effects.writes().len();
        store.write(&effects, all);
        model.apply(req);
        compare(&store.open(plant).map_err(refused("open"))?, &model, &at)?;
    }
    let r = model.task_of(&body(1)).expect("R was named");
    oracle(model.stands(r) == Some(false), || String::from("the model lets R stand after X moved back"))?;
    let retire_reader = request(StdVec::from([Op::Retire { task: 1 }]), 6);
    let effects = {
        let view = store.open(plant).map_err(refused("open before retiring the sole reader"))?;
        commit_request(&view, &retire_reader, &[0], plant).map_err(refused("retire the sole reader"))?
    };
    store.write(&effects, effects.writes().len());
    model.apply(&retire_reader);
    compare(&store.open(plant).map_err(refused("reopen after retiring the sole reader"))?, &model, "sole reader retired")
}

/// The refinement: after every simulated request, compaction, or crash between segments and root, the views over the stored bytes equal the in-memory model for rows, cells, readers, plans, findings, owed tasks, and owed cells.
#[test]
fn histories_match_the_model() {
    check(SimConfig::cases(300), |tape| histories(tape, Plant::None));
}

#[test]
fn a_move_back_is_a_move() {
    check(SimConfig::cases(20), |tape| move_back(tape, Plant::None));
}

#[test]
fn the_oracle_catches_prefix_padding() {
    control(SimConfig::cases(20), Plant::None, Plant::PrefixPadded, "defines", move_back);
}

#[test]
fn a_prefix_only_answer_reports_its_format_before_fetching_rows() {
    // BAR1 form 2, empty summary, one 41-byte rows reference. Both a certificate probe and a
    // normal open must report the old home even if its rows have already been reclaimed.
    let mut old = [0u8; 88 + 41];
    old[..4].copy_from_slice(b"BAR1");
    old[4..8].copy_from_slice(&2u32.to_le_bytes());
    old[84..88].copy_from_slice(&1u32.to_le_bytes());
    old[88] = SegmentKind::Rows as u8;
    let why = AnswerError::Format { segment: crate::ROOT, found: 2 };
    assert_eq!(nothing_changed(&old, &TreeDigest([0; 32])), Err(why));
    let opened = AnswerView::open(&old, |_| panic!("opened a form 2 row"), Plant::None, shared());
    assert_eq!(opened.err(), Some(why));
}

#[test]
fn a_scrub_names_the_corrupt_child_of_an_unchanged_root() {
    check(SimConfig::cases(24), |tape| {
        let request = Request {
            ops: StdVec::from([Op::Put { task: 0, row: draft(StdVec::new(), StdVec::from([(0, 1)])) }]),
            tree: [1; 32], lines: [0; 32], summary: StdVec::new(), owed_cells: None,
        };
        let effects = commit_request(&AnswerView::empty(Plant::None, shared()), &request, &[0], Plant::None).map_err(refused("commit"))?;
        let mut store = Store::default();
        store.write(&effects, effects.writes().len());
        let root = store.root.as_deref().expect("committed root");
        let directory = crate::RootView::parse(root).map_err(refused("parse root"))?;
        directory.scrub(|one| store.segments.get(&one.id.0).map(|bytes| &bytes[..]), shared()).map_err(refused("scrub intact answer"))?;
        let index = tape.index(directory.segments().len(), "damaged segment");
        let child = directory.segments().nth(index).expect("chosen directory entry");
        let bytes = store.segments.get_mut(&child.id.0).expect("committed child");
        let at = tape.index(bytes.len(), "damaged byte");
        bytes[at] ^= 1 << tape.below(8, "damaged bit");
        let same_tree = nothing_changed(root, &TreeDigest(request.tree)).map_err(refused("read root"))?;
        let result = directory.scrub(|one| store.segments.get(&one.id.0).map(|bytes| &bytes[..]), shared());
        oracle(same_tree && matches!(result, Err(AnswerError::SegmentDigest { segment, expected, found }) if segment as usize == index && expected == child.id && found != expected),
            || format!("scrub of damaged child {index} at {at}: {result:?}"))
    });
}

#[test]
fn control_tombstone_ignored() {
    control(SimConfig::cases(300), Plant::None, Plant::TombstoneIgnored, "", histories);
}

#[test]
fn control_cancelled_kept() {
    control(SimConfig::cases(300), Plant::None, Plant::CancelledKept, "readers", histories);
}

#[test]
fn the_oracle_catches_a_missing_last_reader_removal() {
    control(SimConfig::cases(20), Plant::None, Plant::LastReaderRemovalDropped, "readers of cell", move_back);
}

#[test]
fn control_generation_by_digest() {
    control(SimConfig::cases(20), Plant::None, Plant::GenerationByDigest, "validity", move_back);
}

#[test]
fn control_root_first() {
    control(SimConfig::cases(300), Plant::None, Plant::RootFirst, "after a crash", histories);
}

// One planted bug per column of kinds 5 to 8 and the record key, each caught by `compare`.

#[test]
fn control_plan_kept_on_retire() {
    control(SimConfig::cases(300), Plant::None, Plant::PlanKeptOnRetire, "plan of file", histories);
}

#[test]
fn control_findings_kept_on_clear() {
    control(SimConfig::cases(300), Plant::None, Plant::FindingsKeptOnClear, "rows with findings", histories);
}

#[test]
fn control_owed_kept_on_retire() {
    control(SimConfig::cases(300), Plant::None, Plant::OwedKeptOnRetire, "owed tasks", histories);
}

#[test]
fn control_owed_cells_empty_dropped() {
    control(SimConfig::cases(300), Plant::None, Plant::OwedCellsEmptyDropped, "owed cells", histories);
}

#[test]
fn control_record_dropped() {
    control(SimConfig::cases(300), Plant::None, Plant::RecordDropped, "row of task", histories);
}

// ---------------------------------------------------------------------------------------------
// Size.

struct Mix(u64);

impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        crate::bytes::mix(self.0)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// 24,000 rows of 300 reads each (200 distinct cells) over 60,000 cells, each cell defined by
/// task `c % 24,000`: the bytes per row's fixed part, per read and per defined cell, and the total.
#[test]
fn size_of_the_answer() {
    const ROWS: usize = 24_000;
    const CELLS: usize = 60_000;
    const READS: usize = 300;
    const DISTINCT: usize = 200;
    let mut mix = Mix(17);
    let empty = AnswerView::empty(Plant::None, shared());
    let mut commit = Commit::new(&empty, Plant::None, alloc());
    let mut tasks = StdVec::with_capacity(ROWS);
    for i in 0..ROWS {
        let mut subject = [0u8; 32];
        subject[..8].copy_from_slice(&(i as u64).to_le_bytes());
        tasks.push(commit.task(Name { kind: Kind::BodyCheck, subject: ContentId(subject) }).expect("a task id"));
    }
    let file = commit.task(Name { kind: Kind::FileItems, subject: ContentId([0xEE; 32]) }).expect("a file id");
    let mut cells = StdVec::with_capacity(CELLS);
    for c in 0..CELLS {
        cells.push(commit.cell(tasks[c % ROWS], Cell([(c / ROWS) as u8 + 1; 16])).expect("a cell number"));
    }
    let mut worker = RunWriter::new(commit.serial(), alloc());
    let mut seen = StdVec::from_iter(core::iter::repeat_n(false, CELLS));
    for (t, task) in tasks.iter().enumerate() {
        let mut distinct = StdVec::with_capacity(DISTINCT);
        while distinct.len() < DISTINCT {
            let c = mix.below(CELLS);
            if !seen[c] {
                seen[c] = true;
                distinct.push(c);
            }
        }
        for c in distinct.iter() {
            seen[*c] = false;
        }
        let mut reads: StdVec<CellNo> = distinct.iter().map(|c| cells[*c]).collect();
        for _ in DISTINCT..READS {
            reads.push(cells[distinct[mix.below(DISTINCT)]]);
        }
        for i in (1..reads.len()).rev() {
            reads.swap(i, mix.below(i + 1));
        }
        let mut defines = StdVec::new();
        for c in (t..CELLS).step_by(ROWS) {
            let d = digest(mix.next());
            defines.push((cells[c], CellDigest(d)));
        }
        let row = RowIn { key: Key(ContentId([1; 32])), out: ContentId([2; 32]), file, stage: 5, wave: 0, flags: Flags(0), source: ContentId([3; 32]), findings: 0, record: Key(ContentId([4; 32])) };
        commit.put_row(*task, &row, &reads, &defines).expect("a row");
        worker.row_reads(*task, None, &reads);
    }
    let run = worker.finish().expect("the run").expect("readers");
    commit.add_run(&run).expect("the run is taken");
    let effects = commit.finish(&TreeDigest([9; 32]), b"24000 rows", &LinesKey([8; 32])).expect("the commit");
    let mut store = Store::default();
    let all = effects.writes().len();
    store.write(&effects, all);
    let view = store.open(Plant::None).expect("the answer opens");
    let rows = view.rows_bytes();
    assert_eq!(rows.rows, ROWS);
    assert_eq!(rows.read_count, ROWS * READS);
    assert_eq!(rows.fixed, ROWS * ROW_FIXED_BYTES);
    assert!(rows.fixed / rows.rows <= 96, "fixed part {} B a row", rows.fixed / rows.rows);
    let per_read = rows.reads as f64 / rows.read_count as f64;
    assert!(per_read <= 4.0, "{per_read} B a read");
    assert_eq!(rows.defines, rows.define_count * DEFINE_BYTES);
    assert_eq!(rows.defines / rows.define_count, 4 + 32, "one cell number and its complete content id");
    let mut total = 0usize;
    for write in effects.writes() {
        match write {
            Write::Segment { at, bytes } => {
                println!("{:?} segment: {} B", at.kind, bytes.len());
                total += bytes.len();
            }
            Write::Root { bytes } => {
                println!("root: {} B", bytes.len());
                total += bytes.len();
            }
        }
    }
    println!(
        "rows: fixed {} B a row, cold {} B a row; reads {:.3} B a read ({} B a 300-read row of 200 distinct cells); defines {} B each ({} defined cells)",
        rows.fixed / rows.rows,
        rows.cold / rows.rows,
        per_read,
        rows.reads / rows.rows,
        rows.defines / rows.define_count,
        rows.define_count
    );
    println!("total: {total} B for {ROWS} rows, {} B a row", total / ROWS);
    let r = view.task(&Name { kind: Kind::FileItems, subject: ContentId([0xEE; 32]) }).expect("the file");
    assert_eq!(r, file);
}
