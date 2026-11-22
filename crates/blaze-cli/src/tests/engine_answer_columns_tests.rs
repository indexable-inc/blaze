//! blaze-answer's columns behind the rows form (engine_answer_rows.rs): a file's plan, the owed
//! tasks and cells, the rows with findings and each row's record key are each read from their
//! own segment, so a question costs what it answers, never the answer's size; `Engine<RowsAnswer>`
//! over a real `CommandStore` keeps, request after request, the answer `Engine<Answer>` keeps in
//! memory; and a request cut mid-commit leaves the answer before it whole. A child of main.rs's
//! tests; the store, the sink and the small names are engine_answer_rows_tests.rs's.
//!
//! The allocator: each test takes the heap of the options its world is made with
//! (`engine_check_tests::opts`), and hands it down; nothing here names a heap of its own.

use super::*;

use blaze_answer::{AnswerView, CellDigest, Flags, LinesKey, Plant, Serial, TreeDigest};
use blaze_drive::cells::Cell;
use blaze_drive::{Key, Kind, Name};
use blaze_engine::last::{LastAnswer, NextAnswer, NextRow, Touched};
use blaze_engine::{Answer, Buffer, CellId, Done, Edit, Engine, Finished, Fold, Phase, Planned, Pool, SVec, Scope, Stage, Task, View, drive};
use blaze_ids::ContentId;
use blaze_own::{AVec, SharedAlloc, Vec};

use crate::engine_answer::{Facts, Printed};
use crate::engine_answer_rows::{Close, Counts, RowPayload, RowsAnswer, RowsNext, RowsWritten, StoreSink, load_root, load_rows, root_names, summary, write_effects};

use super::engine_answer_rows_tests::{Asked, CELL, Memory, Recording, ask, cell_of, digest, file_name, task_name, tools, universe, with_store};

/// The heap the tests' options hand the command (engine_check_tests.rs `opts`).
fn heap_of(io: &Io) -> &'static blaze_own::Heap {
    super::engine_check_tests::opts(io, 2, false).heap
}

/// The record key the command would hand a row: a function of its name alone, so a carried row
/// keeps it and a row made again writes it equal.
fn record_of(name: &Name) -> Option<Key> {
    Some(Key(ContentId::of("engine_answer_columns_tests record", &name.subject.0)))
}

/// What a question cost: the counts after it less the counts before.
fn cost(before: Touched, after: Touched) -> Touched {
    Touched { rows: after.rows - before.rows, reads: after.reads - before.reads, readers: after.readers - before.readers, bytes: after.bytes - before.bytes }
}

/// O(ANSWER): on a 2,000-task answer with a 25-task plan, 5 owed tasks, 3 owed cells and 8 rows
/// with findings, each question reads its own column: a file's plan and the owed cells read no
/// row, the owed tasks and the rows with findings read one row each they answer, and each costs
/// under 1% of the answer's bytes. Each row's record key reads back as put.
#[test]
fn each_column_costs_its_answer() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let heap = heap_of(&io);
    let (alloc, shared) = (heap.alloc(), heap.shared());
    let tree = TreeDigest([1; 32]);
    let names = root_names("ws", &tools(), &tree, cx.frame());
    let small = Name::new(Kind::FileItems, ContentId::of("engine_answer_columns_tests file", b"src/small.rs"));
    let empty = AnswerView::empty(Plant::None, shared);
    let mut next = RowsNext::new(&empty, Counts::default(), Plant::None, alloc);
    for i in 0..2000u32 {
        let name = task_name(i);
        let row = NextRow {
            key: Key(ContentId::of("engine_answer_columns_tests key", &name.subject.0)),
            out: ContentId::of("engine_answer_columns_tests out", &name.subject.0),
            phase: Phase::new(Stage::Bodies, 0),
            file: if i % 100 == 0 { small } else { file_name() },
            source: ContentId::of("engine_answer_columns_tests source", &name.subject.0),
            flags: Flags(0),
            findings: if i % 250 == 0 { 2 } else { 0 },
            record: record_of(&name).unwrap_or_default(),
        };
        let reads = (i > 0).then(|| cell_of(i - 1));
        next.put(name, &row, reads, &[(CELL, digest(i, 0))]).expect("a row is put");
    }
    for j in 0..5u32 {
        let name = task_name(2000 + j);
        let planned = Planned { name, phase: Phase::new(Stage::Bodies, 0), source: ContentId::of("engine_answer_columns_tests source", &name.subject.0), file: small };
        next.owe(&planned).expect("a task is owed");
    }
    next.owe_cells([10u32, 20, 30].into_iter().map(|i| (cell_of(i), Serial(1)))).expect("the owed cells are given");
    let counts = next.counts();
    let facts = Facts { files: counts.files, printed: Printed { names: true, bodies_whole: true, replayable: true }, findings: counts.findings, owed: 8 };
    let effects = NextAnswer::finish(next, &tree, &summary(&facts), &LinesKey([0; 32])).expect("the commit");
    let mut sink = Recording::default();
    match write_effects(&mut sink, &names, &effects, alloc) {
        Ok(_) => {}
        Err(never) => match never {},
    }
    let mut memory = Memory::default();
    memory.apply(&sink.ops);
    let answer = memory.open(names.checkout.as_str()).expect("the answer opens");
    let total = answer.bytes();
    let file = answer.task(&small).expect("the small file is named");

    let before = answer.touched();
    let plan = answer.file_tasks(file, alloc).expect("the plan reads");
    let plan_cost = cost(before, answer.touched());
    let before = answer.touched();
    let owed = answer.owed(alloc).expect("the owed tasks read");
    let owed_cost = cost(before, answer.touched());
    let before = answer.touched();
    let owed_cells = answer.owed_cells(alloc).expect("the owed cells read");
    let cells_cost = cost(before, answer.touched());
    let before = answer.touched();
    let findings = answer.with_findings(alloc).expect("the rows with findings read");
    let findings_cost = cost(before, answer.touched());
    println!("2,000 rows, {total} B: plan of 25 {plan_cost:?}; 5 owed {owed_cost:?}; 3 owed cells {cells_cost:?}; 8 with findings {findings_cost:?}");

    assert_eq!((plan.len(), owed.len(), owed_cells.len(), findings.len()), (25, 5, 3, 8), "each column holds what was put");
    assert!(owed.iter().all(|planned| planned.file == small), "the owed tasks keep their plan");
    assert!(findings.iter().all(|(_, n)| *n == 2), "each row with findings keeps its count");
    assert_eq!((plan_cost.rows, cells_cost.rows), (0, 0), "a plan and the owed cells read no row");
    assert_eq!((owed_cost.rows, findings_cost.rows), (5, 8), "owed tasks and findings read one row each they answer");
    for (what, one) in [("plan", plan_cost), ("owed", owed_cost), ("owed cells", cells_cost), ("findings", findings_cost)] {
        assert!(one.bytes * 100 < total, "{what}: under 1% of the answer's {total} B: {one:?}");
    }
    for i in [0u32, 7, 1999] {
        let name = task_name(i);
        let task = answer.view().task(&name).expect("a put task is named");
        let row = answer.view().row(task).expect("it has a row");
        assert_eq!(Some(row.record()), record_of(&name), "task {i}'s record key reads back");
    }
    let unmade = answer.view().task(&task_name(2000)).and_then(|task| answer.view().row(task)).expect("an owed task has a row");
    assert_eq!(unmade.record(), Key::default(), "an owed task never made has no record");
}

/// OWED IS NOT RE-VALIDATED: a request moves a cell and owes its reader instead of running it;
/// the reader's row, carried as owed, keeps the serial it was checked at, so it does not stand
/// (the history test's request 2, lane RD9: stamped with the request's serial it stood, a stale
/// answer). Its reads, plan and record are its last row's.
#[test]
fn an_owed_row_keeps_its_serial() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let heap = heap_of(&io);
    let (alloc, shared) = (heap.alloc(), heap.shared());
    let tree = TreeDigest([1; 32]);
    let names = root_names("ws", &tools(), &tree, cx.frame());
    let commit_of = |last: &AnswerView<'_>, counts: Counts, moved: bool| -> Recording {
        let mut next = RowsNext::new(last, counts, Plant::None, alloc);
        for (i, version) in [(0u32, u32::from(moved)), (1, 0)] {
            let name = task_name(i);
            if moved && i == 1 {
                let planned = Planned { name, phase: Phase::new(Stage::Bodies, 0), source: ContentId::of("engine_answer_columns_tests source", &name.subject.0), file: file_name() };
                next.owe(&planned).expect("the reader is owed");
                continue;
            }
            let row = NextRow {
                key: Key(ContentId::of("engine_answer_columns_tests key", &name.subject.0)),
                out: ContentId::of("engine_answer_columns_tests out", &name.subject.0),
                phase: Phase::new(Stage::Bodies, 0),
                file: file_name(),
                source: ContentId::of("engine_answer_columns_tests source", &name.subject.0),
                flags: Flags(0),
                findings: 0,
                record: record_of(&name).unwrap_or_default(),
            };
            let reads = (i == 1).then(|| cell_of(0));
            next.put(name, &row, reads, &[(CELL, digest(i, version))]).expect("a row is put");
        }
        let counts = next.counts();
        let facts = Facts { files: counts.files, printed: Printed { names: true, bodies_whole: true, replayable: true }, findings: counts.findings, owed: u32::from(moved) };
        let effects = NextAnswer::finish(next, &tree, &summary(&facts), &LinesKey([0; 32])).expect("the commit");
        let mut sink = Recording::default();
        match write_effects(&mut sink, &names, &effects, alloc) {
            Ok(_) => {}
            Err(never) => match never {},
        }
        sink
    };
    let mut memory = Memory::default();
    memory.apply(&commit_of(&AnswerView::empty(Plant::None, shared), Counts::default(), false).ops);
    let first = memory.open(names.checkout.as_str()).expect("the first answer opens");
    let reader = first.task(&task_name(1)).expect("task 1 is named");
    assert_eq!(first.stands(reader), Some(true), "the reader stands after the request that made it");
    let ops = commit_of(first.view(), first.counts(), true).ops;
    drop(first);
    memory.apply(&ops);
    let second = memory.open(names.checkout.as_str()).expect("the second answer opens");
    let reader = second.task(&task_name(1)).expect("task 1 is named");
    let row = second.row(reader).expect("its row reads").expect("an owed task keeps its row");
    assert!(row.flags.0 & Flags::OWED != 0, "the row is owed");
    assert!(row.serial < second.serial(), "an owed row keeps the serial it was checked at: {:?} of {:?}", row.serial, second.serial());
    assert_eq!(second.stands(reader), Some(false), "a row owed because its read moved does not stand");
    assert_eq!(second.view().row(reader).map(|row| row.record()), record_of(&task_name(1)), "it keeps its record");
}

/// The tasks of one file: items whose heads read their signature's text and whose bodies read
/// their own text and the next item's head (so an edit reaches a reader through a head).
const ITEMS: usize = 12;
const PLANNED: usize = 1 + 2 * ITEMS;
const SIG: u8 = 2;
const BODY: u8 = 3;

/// The file as one request sees it.
#[derive(Clone, Copy, Debug)]
struct World {
    sig: [u32; ITEMS],
    body: [u32; ITEMS],
    /// Items in the file today (the last one comes and goes).
    items: usize,
    /// The scope runs no body (what moved under them owed).
    partial: bool,
}

fn head(i: usize) -> Name {
    Name::new(Kind::Signature, ContentId::of("engine_answer_columns_tests head", &u32::try_from(i).expect("an item").to_le_bytes()))
}

fn body(i: usize) -> Name {
    task_name(u32::try_from(i).expect("an item"))
}

fn item_cell(tag: u8, i: usize) -> Cell {
    let mut bytes = [0u8; 16];
    bytes[0] = tag;
    bytes[1] = u8::try_from(i).expect("an item");
    Cell(bytes)
}

fn words(what: &'static str, parts: &[u32]) -> ContentId {
    let mut bytes = [0u8; 4 * (2 * ITEMS + 2)];
    for (at, part) in parts.iter().enumerate() {
        bytes[4 * at..4 * at + 4].copy_from_slice(&part.to_le_bytes());
    }
    ContentId::of(what, &bytes[..4 * parts.len()])
}

fn pair(what: &'static str, a: &ContentId, b: &ContentId) -> ContentId {
    let mut bytes = [0u8; 64];
    bytes[..32].copy_from_slice(&a.0);
    bytes[32..].copy_from_slice(&b.0);
    ContentId::of(what, &bytes)
}

fn n32(n: usize) -> u32 {
    u32::try_from(n).expect("a count fits")
}

impl World {
    fn first() -> World {
        World { sig: [0; ITEMS], body: core::array::from_fn(n32), items: ITEMS, partial: false }
    }

    /// Request `r`'s edit: a signature toggled (so a head moves and, two edits on, moves back),
    /// a body's text made new, the last item gone one request in seven, no body run in two
    /// requests of four.
    fn step(self, r: u32) -> World {
        let mut next = self;
        let k = (r as usize * 5) % ITEMS;
        match r % 3 {
            0 => next.sig[k] ^= 1,
            1 => next.body[k] = 100 + r,
            _ => next.sig[(k + 1) % ITEMS] ^= 1,
        }
        next.items = if r % 7 == 3 { ITEMS - 1 } else { ITEMS };
        next.partial = matches!(r % 4, 1 | 2);
        next
    }

    fn sig_text(&self, i: usize) -> ContentId {
        words("engine_answer_columns_tests sig text", &[n32(i), self.sig[i]])
    }

    fn body_text(&self, i: usize) -> ContentId {
        words("engine_answer_columns_tests body text", &[n32(i), self.body[i], n32((i + 1) % self.items)])
    }

    /// The file's plan: its outline, then each item's head and body; the first
    /// [`World::planned`] are today's.
    fn plan(&self) -> [Planned; PLANNED] {
        let file = file_name();
        let mut text = [0u32; 2 * ITEMS + 1];
        text[0] = n32(self.items);
        text[1..=ITEMS].copy_from_slice(&self.sig);
        text[ITEMS + 1..].copy_from_slice(&self.body);
        let outline = Planned { name: file, phase: Phase::new(Stage::Outlines, 0), source: words("engine_answer_columns_tests file text", &text), file };
        core::array::from_fn(|at| match at {
            0 => outline,
            at if (at - 1) / 2 >= self.items => outline,
            at if at % 2 == 1 => Planned { name: head((at - 1) / 2), phase: Phase::new(Stage::Heads, 0), source: self.sig_text((at - 1) / 2), file },
            at => Planned { name: body((at - 1) / 2), phase: Phase::new(Stage::Bodies, 0), source: self.body_text((at - 1) / 2), file },
        })
    }

    fn planned(&self) -> usize {
        1 + 2 * self.items
    }

    fn tree(&self) -> TreeDigest {
        let plan = self.plan();
        let mut tree = [0u8; 32];
        for planned in plan[..self.planned()].iter() {
            for (at, byte) in planned.source.0.iter().enumerate() {
                tree[at] ^= byte;
            }
        }
        TreeDigest(tree)
    }
}

/// Every task, or every task but the bodies (as `blaze run` of no body would: what moved under
/// them is owed).
struct NoBodies(bool);

impl Scope for NoBodies {
    fn runs(&self, task: &Planned) -> bool {
        !(self.0 && task.phase.stage == Stage::Bodies)
    }
}

/// One worker, in plan order.
struct One;

impl<'a> Pool<'a> for One {
    type Cx = ();

    fn run(&mut self, tasks: &[Task], buffers: &mut [Buffer<'a>], work: &(dyn Fn(&(), &Task, &mut Buffer<'a>) + Sync)) {
        tasks.iter().for_each(|task| work(&(), task, &mut buffers[0]));
    }
}

/// The control reproduces the previous rows reader's loss of the suffix of a carried value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Handed {
    Whole,
    PrefixPadded,
}

/// The world's tasks over the view's cells. A body whose text's version is a multiple of three
/// has a finding. The digests a task records as seen are the view's, as handed; what it makes
/// of them is `how`'s.
fn work<'a, L: LastAnswer>(world: &World, how: Handed, shared: SharedAlloc<'a>, view: View<'_, 'a, L>, task: &Task, buffer: &mut Buffer<'a>) {
    let name = task.planned.name;
    let mut defines: SVec<'a, (Cell, ContentId)> = Vec::new_in(shared);
    let mut reads: SVec<'a, (CellId, ContentId)> = Vec::new_in(shared);
    let mut findings = 0;
    let out = if name == file_name() {
        for i in 0..world.items {
            defines.push((item_cell(SIG, i), world.sig_text(i)));
            defines.push((item_cell(BODY, i), world.body_text(i)));
        }
        task.planned.source
    } else if let Some(i) = (0..world.items).find(|i| head(*i) == name) {
        let id = CellId { of: file_name(), cell: item_cell(SIG, i) };
        let seen = view.cell(&id);
        reads.push((id, seen));
        let made = pair("engine_answer_columns_tests head", &seen, &seen);
        defines.push((CELL, made));
        made
    } else {
        let i = (0..world.items).find(|i| body(*i) == name).expect("a body of today's plan");
        let text = CellId { of: file_name(), cell: item_cell(BODY, i) };
        let callee = CellId { of: head((i + 1) % world.items), cell: CELL };
        let (a, mut b) = (view.cell(&text), view.cell(&callee));
        if how == Handed::PrefixPadded {
            b.0[20..].fill(0);
        }
        reads.push((text, a));
        reads.push((callee, b));
        if world.body[i] % 3 == 0 {
            findings = 1;
        }
        pair("engine_answer_columns_tests body", &a, &b)
    };
    defines.sort_unstable_by_key(|(cell, _)| *cell);
    buffer.done.push(Done { name, key: task.key, out, defines, reads, findings, failed: false });
}

/// One request of `world` over the last answer in the form `L`, the engine's bug `plant`.
fn request<'a, L: Fold<'a> + Sync>(world: &World, how: Handed, last: L, shared: SharedAlloc<'a>, plant: blaze_engine::Plant) -> Finished<'a, L::Out> {
    let plan = world.plan();
    let files = [(file_name(), &plan[..world.planned()])];
    let mut engine = Engine::new_in(tools(), last, &Edit { files: &files }, NoBodies(world.partial), plant, shared).expect("the engine starts");
    let mut buffers = [Buffer::new_in(shared)];
    drive(&mut engine, &mut One, &mut buffers, &|_, view, task, buffer| work(world, how, shared, view, task, buffer)).expect("the request runs");
    engine.finish().expect("the request finishes")
}

/// What the command hands the rows form's commit at a request's end, for `world`.
fn close(world: &World) -> Close<'static> {
    Close { tree: world.tree(), printed: Printed { names: true, bodies_whole: true, replayable: true }, lines: LinesKey([0; 32]), records: &|name| record_of(name).map(RowPayload::Stored) }
}

#[test]
fn a_changed_row_requires_a_payload_decision() {
    let world = World::first();
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let heap = heap_of(&io);
    let finished = request(&world, Handed::Whole, RowsAnswer::empty(heap.shared()), heap.shared(), blaze_engine::Plant::None);
    let task = body(5);
    let missing = |name: &Name| if *name == task { None } else { record_of(name).map(RowPayload::Stored) };
    let close = Close { records: &missing, ..close(&world) };
    assert!(matches!(finished.answer.commit(&close, Plant::None, heap.alloc()), Err(blaze_engine::last::LastError::RowPayload { task: refused }) if refused == task));
    let recordless = |name: &Name| if *name == task { Some(RowPayload::NoPayload) } else { record_of(name).map(RowPayload::Stored) };
    let close = Close { records: &recordless, ..close };
    let (effects, _) = finished.answer.commit(&close, Plant::None, heap.alloc()).expect("a body with an explicit recordless result can be published");
    let opened = AnswerView::open(effects.root(), |child| effects.writes().iter().find_map(|write| match write {
        blaze_answer::Write::Segment { at, bytes } if at == child => Some(bytes.as_slice()),
        _ => None,
    }), Plant::None, heap.shared()).expect("the recordless row reopens");
    let row = opened.task(&task).and_then(|id| opened.row(id)).expect("the body still has a row");
    assert_eq!(row.record(), Key::default(), "a recordless row names no payload");
}

/// Two answers' lists equal, or a panic naming the FIRST entry that differs, both sides of it
/// alone (the whole lists run past what the runner keeps of a line: lane RD9's failure printed
/// two cut lists and no difference).
fn same_list<T: PartialEq + core::fmt::Debug>(r: u32, what: &str, got: &[T], mem: &[T]) {
    if got == mem {
        return;
    }
    let at = got.iter().zip(mem.iter()).position(|(a, b)| a != b).unwrap_or(got.len().min(mem.len()));
    panic!(
        "request {r}: {what}: first difference at entry {at} of {} (rows form) and {} (memory form):\n  rows form:   {:?}\n  memory form: {:?}",
        got.len(),
        mem.len(),
        got.get(at),
        mem.get(at)
    );
}

/// What `compare` found besides the equal tables.
#[derive(Clone, Copy, Default, Debug)]
struct Compared {
    /// Rows standing by digest and not by generation.
    extras: u64,
    /// Cells owed by generation alone (`owed_by_generation`).
    gen_owed: u64,
}

/// The owed cells of the two forms (plan row B1's follow-up, lane RD10): the rows form owes
/// every cell the memory form owes (fewer is a stale answer), and may owe more, each by the
/// generation rule alone: a cell that moved and moved back while its readers were not run (the
/// history's requests 17 and 18: item 11 gone and back with no body run, so body 10's text
/// cell and head 11's cell end on the digest body 10's row saw). Such a cell is EXPLAINED when
/// every reader of it saw today's digest (so the digests settle it) and some reader's row is
/// older than the cell's move (so the generations do not). An extra owed cell not so explained,
/// or any left after a request that ran every task (`ran_all`: the readers were entered, came
/// out identical and were stamped again), is a cell that never settles: a panic naming it.
/// Gives the count of the explained extras.
fn owed_by_generation(r: u32, got: &Asked, mem: &Asked, ran_all: bool) -> u64 {
    for id in mem.owed_cells.iter() {
        assert!(got.owed_cells.binary_search(id).is_ok(), "request {r}: owed cells: {id:?} is owed by digest and settled by generation (a stale answer)");
    }
    let saw = |asked: &Asked, reader: &Name, id: &CellId| asked.saw.iter().find(|(name, cell, _)| name == reader && cell == id).is_some_and(|(_, _, saw)| *saw);
    let mut extra = 0u64;
    for id in got.owed_cells.iter() {
        if mem.owed_cells.binary_search(id).is_ok() {
            continue;
        }
        let readers = mem.readers.iter().find(|(cell, _)| cell == id).map(|(_, readers)| readers.as_slice()).unwrap_or(&[]);
        let by_digest = readers.iter().all(|reader| saw(mem, reader, id));
        let by_generation = readers.iter().any(|reader| !saw(got, reader, id));
        assert!(
            by_digest && by_generation,
            "request {r}: owed cells: {id:?} is owed by generation and not explained (every reader saw today's digest: {by_digest}; a reader's row older than the move: {by_generation}; readers {readers:?}): a cell that never settles"
        );
        extra += 1;
    }
    assert!(
        !ran_all || extra == 0,
        "request {r}: owed cells: {extra} owed only by generation after a request that ran every task: they never settle (first {:?})",
        got.owed_cells.iter().find(|id| mem.owed_cells.binary_search(id).is_err())
    );
    extra
}

/// The rows form against the in-memory form after the same requests: every question alike but
/// standing, where the rows form may say a row does not stand that the digests would keep (a
/// cell moved and moved back: the generation rule's extra), never the other way, and the owed
/// cells, which the rows form may hold more of, each explained ([`owed_by_generation`]).
/// `ran_all`: the request that wrote the answers ran every task. Each row made carries its
/// record key.
fn compare(memory: &Answer<'_>, rows: &RowsAnswer<'_>, r: u32, ran_all: bool) -> Compared {
    let (names, cells, files) = universe(memory);
    let (tree, other) = (TreeDigest([1; 32]), TreeDigest([2; 32]));
    let mem = ask(memory, &names, &cells, &files, &tree, &other);
    let got = ask(rows, &names, &cells, &files, &tree, &other);
    same_list(r, "rows", &got.rows, &mem.rows);
    same_list(r, "reads", &got.reads, &mem.reads);
    same_list(r, "defines", &got.defines, &mem.defines);
    same_list(r, "digests", &got.digests, &mem.digests);
    same_list(r, "readers", &got.readers, &mem.readers);
    same_list(r, "a file's plan", &got.file_tasks, &mem.file_tasks);
    same_list(r, "owed tasks", &got.owed, &mem.owed);
    let gen_owed = owed_by_generation(r, &got, &mem, ran_all);
    same_list(r, "rows with findings", &got.with_findings, &mem.with_findings);
    let mut extras = 0;
    for ((name, here), (_, there)) in got.stands.iter().zip(mem.stands.iter()) {
        match (here, there) {
            (Some(false), Some(true)) => extras += 1,
            _ => assert_eq!(here, there, "request {r}: whether {name:?} stands"),
        }
    }
    for name in memory.rows.keys() {
        let row = rows.view().task(name).and_then(|task| rows.view().row(task)).expect("a row of the memory form's is the rows form's");
        assert_eq!(Some(row.record()), record_of(name), "request {r}: {name:?}'s record key");
    }
    Compared { extras, gen_owed }
}

/// What one rows-form request did, for the history's lines.
struct Ran {
    extras: u64,
    gen_owed: u64,
    touched: Touched,
    written: RowsWritten,
    entered: AVec<'static, Name>,
}

/// HISTORY: thirty requests of one file (signatures toggled and toggled back, bodies made new,
/// the last item gone and back, no body run in two requests of four), each run twice: by
/// `Engine<Answer>` in memory, and by `Engine<RowsAnswer>` over the answer the last request
/// wrote to a real `CommandStore`, committed by `RowsFolded::commit` and written as the command
/// writes it. The oracle (blaze-engine last_gen_sim.rs): every task the memory form enters, the
/// rows form enters; the rows form's extras are rows that stand by digest and not by generation;
/// and the two answers are alike after every request. Each request prints the rows touched.
#[test]
fn rows_engine_keeps_the_memory_engines_answer() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let mut worlds = [World::first(); 30];
    for r in 1..30 {
        worlds[r] = worlds[r - 1].step(n32(r));
    }
    let (totals, _) = twin(&io, &worlds, Handed::Whole, true, blaze_engine::Plant::None);
    println!(
        "thirty requests: entered {} by digest, {} by generation ({} extra, {} standing extras); owed only by generation {} (at most {} at once); rows touched {}",
        totals.entered_mem, totals.entered_rows, totals.extra_entries, totals.extra_rows, totals.gen_owed, totals.gen_owed_most, totals.touched_rows
    );
    assert!(totals.entered_rows >= totals.entered_mem, "the rows form enters at least what the memory form does");
    // The history's requests 17 and 18 take item 11 out and back with no body run: the class
    // the comparison bounds happens here, or the bound is untested.
    assert!(totals.gen_owed > 0, "the history owes a cell by generation alone at least once (requests 17 and 18)");
}

/// CONTROL of the owed-cells bound: the rows form's engine with `Plant::StaleKeptOnTake` (a
/// taken row's stale reads left in the cells' counts, blaze-engine overlay.rs "Stale counts")
/// keeps a cell owed after its readers ran again: the comparison names it as one that never
/// settles.
#[test]
fn control_a_cell_that_never_settles_is_caught() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let mut worlds = [World::first(); 30];
    for r in 1..30 {
        worlds[r] = worlds[r - 1].step(n32(r));
    }
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| twin(&io, &worlds, Handed::Whole, false, blaze_engine::Plant::StaleKeptOnTake)));
    let why = match caught {
        Ok(_) => panic!("control: a cell kept owed after its readers ran again went unseen: the owed-cells bound is blind"),
        Err(why) => why,
    };
    let said = why.downcast_ref::<std::string::String>().map(|s| s.as_str()).or_else(|| why.downcast_ref::<&str>().copied()).unwrap_or("");
    println!("control caught: {said}");
    assert!(said.contains("owed cells") && said.contains("never settle"), "control: the comparison names a cell that never settles: {said}");
}

/// Editing body 5 while head 6 stands must hand the body all 32 bytes of its callee's
/// value. Its output depends on all 32. Padding the carried value must change that output.
#[test]
fn a_carried_cell_is_handed_in_its_kept_form() {
    let first = World::first();
    let mut edited = first;
    edited.body[5] = 100;
    let worlds = [first, edited];
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let (_, memory) = twin(&io, &worlds, Handed::Whole, false, blaze_engine::Plant::None);
    let id = CellId { of: head(6), cell: CELL };
    let alloc = heap_of(&io).shared();
    let fresh = request(&edited, Handed::Whole, Answer::new_in(alloc), alloc, blaze_engine::Plant::None).answer;
    let whole = fresh.cell(&id);
    assert_eq!(whole, memory.cell(&id), "the standing head equals a cold head");
    let fresh_out = fresh.rows.get(&body(5)).expect("a cold run checks the edited body").out;
    with_store(&io, |cx, store, opts, _| {
        let names = root_names(opts.place.checkout, &tools(), &edited.tree(), cx.frame());
        let root = load_root(cx, store, names.checkout.as_str()).expect("the store reads").expect("a rows root").expect("the rows root reads");
        let rows = load_rows(cx, store, &root, opts.heap.shared()).expect("the store reads").expect("the rows read");
        let cell = LastAnswer::cell(&rows, &id).expect("head 6's cell is named");
        let carried = LastAnswer::content(&rows, cell);
        assert!(whole.0[20..].iter().any(|&byte| byte != 0), "the fixture exercises the carried suffix");
        assert_eq!(carried, whole, "fresh content id == reopened content id");
        let body_row = rows.task(&body(5)).and_then(|task| rows.view().row(task)).expect("the edited body has a stored row");
        assert_eq!(body_row.out(), fresh_out, "using a carried callee gives the cold body's output");
        println!("fresh content id == reopened content id (32 bytes)");
    });

    let control = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| twin(&control, &worlds, Handed::PrefixPadded, false, blaze_engine::Plant::None)));
    let why = match caught {
        Ok(_) => panic!("control: the comparison missed a padded callee value"),
        Err(why) => why,
    };
    let said = why.downcast_ref::<std::string::String>().map(|s| s.as_str()).or_else(|| why.downcast_ref::<&str>().copied()).unwrap_or("");
    assert!(said.contains("request 2: rows: first difference"), "control: the comparison names the row that differs: {said}");
    assert!(said.contains("Bodies"), "control: the row that differs is a body's: {said}");
}

/// What a twin history counted.
#[derive(Clone, Copy, Default, Debug)]
struct Totals {
    entered_mem: u64,
    entered_rows: u64,
    extra_entries: u64,
    extra_rows: u64,
    touched_rows: u64,
    /// Cells owed by generation alone, summed over the comparisons, and the most at one.
    gen_owed: u64,
    gen_owed_most: u64,
}

/// `worlds`, one request each, run by `Engine<Answer>` in memory and by `Engine<RowsAnswer>` over
/// the answer the last request wrote to `io`'s store, their tasks taking cells as `how` says; the
/// two answers compared before each request and after the last (`compare`, which panics naming
/// the first difference). `plant` is the rows form's engine's bug (the memory form's runs
/// clean). Gives the counts and the memory form's last answer.
fn twin(io: &Io, worlds: &[World], how: Handed, print: bool, plant: blaze_engine::Plant) -> (Totals, Answer<'static>) {
    let heap = heap_of(io);
    let (alloc, shared) = (heap.alloc(), heap.shared());
    let mut memory = Answer::new_in(shared);
    let mut first = true;
    let mut totals = Totals::default();
    for (r, world) in worlds.iter().enumerate() {
        let r = n32(r);
        let world = *world;
        // Whether the request that wrote the answers compared now ran every task.
        let ran_all = r > 0 && !worlds[r as usize - 1].partial;
        let last_memory = &memory;
        let ran = with_store(io, |cx, store, opts, lent| {
            let names = root_names(opts.place.checkout, &tools(), &world.tree(), cx.frame());
            let (compared, last) = match first {
                true => (Compared::default(), RowsAnswer::empty(opts.heap.shared())),
                false => {
                    let root = load_root(cx, store, names.checkout.as_str()).expect("the store reads").expect("a rows root").expect("the rows root reads");
                    // Asked once to compare, then opened again for the engine: the questions'
                    // counts are the request's alone.
                    let asked = load_rows(cx, store, &root, opts.heap.shared()).expect("the store reads").expect("the rows read");
                    let compared = compare(last_memory, &asked, r, ran_all);
                    (compared, load_rows(cx, store, &root, opts.heap.shared()).expect("the store reads").expect("the rows read"))
                }
            };
            let finished = request(&world, if first { Handed::Whole } else { how }, last, opts.heap.shared(), plant);
            let (effects, written) = finished.answer.commit(&close(&world), Plant::None, alloc).expect("the rows commit");
            write_effects(&mut StoreSink { cx, store: &mut *store, lent, toolchain: opts.toolchain }, &names, &effects, alloc).expect("the store takes the writes");
            let mut entered: AVec<'static, Name> = Vec::new_in(alloc);
            entered.extend(finished.entered.iter().copied());
            Ran { extras: compared.extras, gen_owed: compared.gen_owed, touched: finished.touched, written, entered }
        });
        let ours = request(&world, Handed::Whole, core::mem::replace(&mut memory, Answer::new_in(shared)), shared, blaze_engine::Plant::None);
        memory = ours.answer;
        let mut mem_entered: AVec<'_, Name> = Vec::new_in(alloc);
        mem_entered.extend(ours.entered.iter().copied());
        mem_entered.sort_unstable();
        let mut rows_entered = ran.entered;
        rows_entered.sort_unstable();
        for name in mem_entered.iter() {
            assert!(rows_entered.binary_search(name).is_ok(), "request {r}: the memory form entered {name:?} and the rows form did not");
        }
        let extra = rows_entered.len() - mem_entered.len();
        if print {
            println!(
                "request {r:2}: entered {} (rows form {}, extra {extra}), rows standing by digest not generation {}, owed only by generation {}, rows touched {} (memory form {}), bytes {}, written {:?}",
                mem_entered.len(),
                rows_entered.len(),
                ran.extras,
                ran.gen_owed,
                ran.touched.rows,
                ours.touched.rows,
                ran.touched.bytes,
                ran.written,
            );
        }
        totals.extra_rows += ran.extras;
        totals.extra_entries += n64(extra);
        totals.entered_mem += n64(mem_entered.len());
        totals.entered_rows += n64(rows_entered.len());
        totals.touched_rows += ran.touched.rows;
        totals.gen_owed += ran.gen_owed;
        totals.gen_owed_most = totals.gen_owed_most.max(ran.gen_owed);
        first = false;
    }
    let last_world = worlds.last().copied().unwrap_or_else(World::first);
    let last_memory = &memory;
    let compared = with_store(io, |cx, store, opts, _| {
        let names = root_names(opts.place.checkout, &tools(), &last_world.tree(), cx.frame());
        let root = load_root(cx, store, names.checkout.as_str()).expect("the store reads").expect("a rows root").expect("the rows root reads");
        let rows = load_rows(cx, store, &root, opts.heap.shared()).expect("the store reads").expect("the rows read");
        rows.view().verify().expect("the last answer verifies");
        compare(last_memory, &rows, n32(worlds.len()), !last_world.partial)
    });
    totals.extra_rows += compared.extras;
    totals.gen_owed += compared.gen_owed;
    totals.gen_owed_most = totals.gen_owed_most.max(compared.gen_owed);
    (totals, memory)
}

fn n64(n: usize) -> u64 {
    u64::try_from(n).expect("a count fits")
}

/// CUT: a second request's commit, owed tasks and cells and findings and records included, cut
/// after every write: what the checkout's name pins opens whole and verifies, and is either the
/// first answer, asked alike, or the second; the whole write gives the second.
#[test]
fn a_cut_commit_leaves_the_last_answer_whole() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let heap = heap_of(&io);
    let (alloc, shared) = (heap.alloc(), heap.shared());
    let first_world = World::first();
    let names = root_names("ws", &tools(), &first_world.tree(), cx.frame());
    let ops_of = |world: &World, last: RowsAnswer<'_>| -> Recording {
        let finished = request(world, Handed::Whole, last, shared, blaze_engine::Plant::None);
        let (effects, _) = finished.answer.commit(&close(world), Plant::None, alloc).expect("the rows commit");
        let mut sink = Recording::default();
        match write_effects(&mut sink, &names, &effects, alloc) {
            Ok(_) => {}
            Err(never) => match never {},
        }
        sink
    };
    let mut base = Memory::default();
    base.apply(&ops_of(&first_world, RowsAnswer::empty(shared)).ops);
    // The second request runs no body and moves a head and a body's text: body 5 is owed, and
    // head 3's cell under body 2.
    let mut second_world = first_world.step(1);
    second_world.partial = true;
    second_world.sig[3] ^= 1;
    let ops = ops_of(&second_world, base.open(names.checkout.as_str()).expect("the first answer opens")).ops;
    assert!(ops.len() > 4, "a request writes segments, a root and its pins: {}", ops.len());
    let before = base.open(names.checkout.as_str()).expect("the first answer opens");
    let first = before.serial();
    let probe = |answer: &RowsAnswer<'_>| -> super::engine_answer_rows_tests::Asked {
        let mut names_asked: AVec<'_, Name> = Vec::new_in(alloc);
        names_asked.extend((0..ITEMS).flat_map(|i| [head(i), body(i)]));
        let mut cells: AVec<'_, CellId> = Vec::new_in(alloc);
        cells.extend((0..ITEMS).flat_map(|i| [CellId { of: file_name(), cell: item_cell(SIG, i) }, CellId { of: head(i), cell: CELL }]));
        let files = [file_name()];
        ask(answer, &names_asked, &cells, &files, &first_world.tree(), &TreeDigest([0; 32]))
    };
    let asked_first = probe(&before);
    let mut owed_seen = false;
    for cut in 0..=ops.len() {
        let mut memory = base.clone();
        memory.apply(&ops[..cut]);
        let answer = memory.open(names.checkout.as_str()).unwrap_or_else(|why| panic!("cut {cut}: the pinned answer does not open: {why}"));
        answer.view().verify().unwrap_or_else(|why| panic!("cut {cut}: the pinned answer does not verify: {why}"));
        let serial = answer.serial();
        if serial == first {
            assert_eq!(probe(&answer), asked_first, "cut {cut}: the first answer, whole");
        } else {
            assert!(serial > first, "cut {cut}: the first answer or the second, not {serial:?}");
            owed_seen |= !answer.owed(alloc).expect("the owed tasks read").is_empty();
        }
        if cut == ops.len() {
            assert!(serial > first, "the whole write gives the second answer");
        }
    }
    assert!(owed_seen, "the second answer owes the bodies its scope did not run");
    let digest_of = |answer: &RowsAnswer<'_>, i: usize| answer.view().cell(&head(i), &CELL).map_or(CellDigest::ABSENT, |cell| answer.view().digest(cell));
    let mut whole = base.clone();
    whole.apply(&ops);
    let second = whole.open(names.checkout.as_str()).expect("the second answer opens");
    assert_ne!(digest_of(&second, 3), digest_of(&before, 3), "the second answer holds the moved head");
}
