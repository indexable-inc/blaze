//! The stored answer's two forms behind blaze-engine's seam (last.rs, engine_answer_rows.rs):
//! the page form read whole and blaze-answer's rows read in place answer every question alike
//! on a real engine-check answer; the rows form touches one row for one row's question; a root
//! for another tree is no certificate while its generations still decide; a row whose read moved
//! after it is not served as standing; and the write's order, cut at every point, never pins a
//! root whose segments are not put. A child of main.rs's tests: its helpers are theirs.

use super::*;

use std::collections::BTreeMap as StdMap;

use blaze_answer::{AnswerError, AnswerView, CellDigest, Flags, Health, LinesKey, Plant, SegmentRef, Serial, TreeDigest, nothing_changed};
use blaze_drive::cells::Cell;
use blaze_drive::{Key, Kind, Name};
use blaze_engine::last::{LastAnswer, LastError, LastRow, NextAnswer, NextRow};
use blaze_engine::{Answer, CellId, Phase, Planned, Stage, Tools};
use blaze_ids::ContentId;

use crate::engine_answer::{Facts, Printed};
use crate::engine_answer_rows::{
    AnswerSink, Counts, PagesAnswer, RootNames, RowsAnswer, RowsNext, RowsRoot, RowsUnreadable, StoreSink, certified, load_root, load_rows, put_whole, root_key, root_names, summary,
    write_effects,
};

pub(super) type List<T> = std::vec::Vec<T>;

/// The command's store on the checkout `ws`, opened as `blaze why` opens it (engine_why.rs
/// `explain`), handed to `f`, then committed: what `f` put and pinned is visible to the next.
pub(super) fn with_store<R>(io: &Io, f: impl for<'m, 'r, 's> FnOnce(&bstd::Cx, &'s mut crate::store::CommandStore<'m, 'r, bstd::io::LentDriver>, &engine_check::Opts<'static>) -> R) -> R {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let erased = blaze_own::Erased::new(frame);
    let opts = super::engine_check_tests::opts(io, 2, false);
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let err = io.need_err().expect("err");
    let phases = Phases::new(io.need_clock().expect("a clock"));
    let opened = crate::store::open_for_command(&cx, io, opts.place.home, &root, opts.place.checkout, phases.clock(), &err, opts.heap).expect("the store opens");
    let mut store = crate::store::CommandStore::new(&cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), &err, erased.handle()).expect("the command's store");
    let out = f(&cx, &mut store, &opts);
    crate::store::Store::after_answer(&mut store, &cx, &mut blaze_drive::profile::Profile::default()).expect("the store commits");
    drop(store);
    crate::store::after_answer_printed(&cx, opened.lent, false, phases.clock(), &err).expect("after the answer");
    out
}

/// Every answer a form gives, in names (a form's own ids are its own), sorted where the forms
/// keep another order, and the serials (each form's own numbering) zeroed.
#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct Asked {
    pub(super) rows: List<(Name, Option<LastRow>)>,
    pub(super) reads: List<(Name, Option<List<CellId>>)>,
    pub(super) defines: List<(Name, Option<List<(CellId, CellDigest)>>)>,
    pub(super) stands: List<(Name, Option<bool>)>,
    pub(super) saw: List<(Name, CellId, bool)>,
    pub(super) digests: List<(CellId, CellDigest)>,
    pub(super) readers: List<(CellId, List<Name>)>,
    pub(super) file_tasks: List<(Name, List<Name>)>,
    pub(super) owed: List<Planned>,
    pub(super) owed_cells: List<CellId>,
    pub(super) with_findings: List<(Name, u32)>,
    /// The certificate for the tree the answer answers, and for another.
    pub(super) certificate: (bool, bool),
}

/// Every question of the seam, asked of `form` for each of `names`, `cells` and `files`.
pub(super) fn ask<L: LastAnswer>(form: &L, names: &[Name], cells: &[CellId], files: &[Name], tree: &TreeDigest, other: &TreeDigest) -> Asked {
    let alloc = blaze_own::testing::heap().alloc();
    let named = |task: L::Task| form.name(task).expect("every task a form lists has a name");
    let id_of = |cell: L::Cell| form.cell_id(cell).expect("every cell a form lists has an id");
    let mut asked = Asked::default();
    for name in names {
        let task = form.task(name);
        let row = task.and_then(|t| form.row(t).expect("a row reads")).map(|row| LastRow { serial: Serial(0), ..row });
        asked.rows.push((*name, row));
        let reads = task.and_then(|t| form.reads(t)).map(|reads| {
            let mut ids: List<CellId> = reads.map(|cell| id_of(cell)).collect();
            ids.sort();
            ids
        });
        asked.reads.push((*name, reads));
        let defines = task.and_then(|t| form.defines(t)).map(|defines| {
            let mut ids: List<(CellId, CellDigest)> = defines.map(|(cell, digest)| (id_of(cell), digest)).collect();
            ids.sort();
            ids
        });
        asked.defines.push((*name, defines));
        asked.stands.push((*name, task.and_then(|t| form.stands(t))));
        for cell in cells {
            let saw = match (task, form.cell(cell)) {
                (Some(t), Some(c)) => form.saw(t, c).expect("a read found by search"),
                _ => false,
            };
            asked.saw.push((*name, *cell, saw));
        }
    }
    for cell in cells {
        let found = form.cell(cell);
        asked.digests.push((*cell, found.map_or(CellDigest::ABSENT, |c| form.digest(c))));
        let mut readers: List<Name> = match found {
            Some(c) => form.readers(c, alloc).expect("the readers read").iter().map(|task| named(*task)).collect(),
            None => List::new(),
        };
        readers.sort();
        asked.readers.push((*cell, readers));
    }
    for file in files {
        let mut tasks: List<Name> = match form.task(file) {
            Some(t) => form.file_tasks(t, alloc).expect("a file's tasks read").iter().map(|task| named(*task)).collect(),
            None => List::new(),
        };
        tasks.sort();
        asked.file_tasks.push((*file, tasks));
    }
    asked.owed = form.owed(alloc).expect("the owed tasks read").iter().copied().collect();
    asked.owed.sort_by_key(|planned| planned.name);
    asked.owed_cells = form.owed_cells(alloc).expect("the owed cells read").iter().map(|(cell, _)| id_of(*cell)).collect();
    asked.owed_cells.sort();
    asked.with_findings = form.with_findings(alloc).expect("the rows with findings read").iter().map(|(task, n)| (named(*task), *n)).collect();
    asked.with_findings.sort();
    asked.certificate = (form.certificate(tree), form.certificate(other));
    asked
}

/// What the questions are asked of: every name with a row or owed (and one with neither),
/// every cell read, defined or digested, every file with a plan.
pub(super) fn universe(answer: &Answer<'_>) -> (List<Name>, List<CellId>, List<Name>) {
    let mut names: List<Name> = answer.rows.keys().copied().chain(answer.owed_tasks.keys().copied()).collect();
    names.push(Name::new(Kind::BodyCheck, ContentId::of("engine_answer_rows_tests absent", b"none")));
    names.sort();
    names.dedup();
    let mut cells: List<CellId> = answer.cells.keys().copied().collect();
    for (name, row) in answer.rows.iter() {
        cells.extend(row.reads.iter().map(|(id, _)| *id));
        cells.extend(row.defines.iter().map(|(cell, _)| CellId { of: *name, cell: *cell }));
    }
    cells.sort();
    cells.dedup();
    (names, cells, answer.by_file.keys().copied().collect())
}

/// Hundredths of `n / d`, printed as `x.yy`.
pub(super) fn ratio(n: u64, d: u64) -> std::string::String {
    let h = n * 100 / d.max(1);
    format!("{}.{:02}", h / 100, h % 100)
}

pub(super) fn n64(n: usize) -> u64 {
    u64::try_from(n).expect("a count fits")
}

/// EQUAL and SIZE: a real three-crate engine-check answer, written whole as rows through the
/// store's one commit, then both forms asked every question of the seam, alike.
#[test]
fn both_forms_answer_alike() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    super::engine_check_tests::three_crates(&io);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let tree = TreeDigest([7; 32]);
    let other = TreeDigest([8; 32]);
    let alloc = blaze_own::testing::heap().alloc();
    let (saved, counts, facts) = with_store(&io, |cx, store, opts| {
        let frame = cx.frame();
        let name = crate::engine_answer::root_name(opts.place.checkout, &opts.tools, frame);
        let pointer = crate::engine_answer::load_pointer(cx, store, &name).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
        let empty = AnswerView::empty(Plant::None, opts.heap.shared());
        let mut next = RowsNext::new(&empty, Counts::default(), Plant::None, alloc);
        put_whole(&kept.answer, &|_| None, &mut next).expect("every row is put");
        let counts = next.counts();
        let effects = next.finish(&tree, &summary(&pointer.facts), &LinesKey(pointer.lines.0.0)).expect("the commit");
        let names = root_names(opts.place.checkout, &opts.tools, &tree, frame);
        let saved = write_effects(&mut StoreSink { cx, store: &mut *store }, &names, &[], &effects, pointer.lines, alloc).expect("the store takes the writes");
        (saved, counts, pointer.facts)
    });
    // The running counts, kept row by row, are the page form's whole count (engine_answer.rs
    // `Facts::of`).
    assert_eq!(counts, Counts { files: facts.files, findings: facts.findings }, "the rows' running counts against the pointer's facts");
    assert!(saved.new_segments > 0 && saved.puts == saved.new_segments + 1 && saved.segments == saved.new_segments, "a first answer puts its segments and its root: {saved:?}");
    assert!(saved.new_bytes > 0, "its segments have bytes: {saved:?}");
    let (pages, rows) = with_store(&io, |cx, store, opts| {
        let frame = cx.frame();
        let name = crate::engine_answer::root_name(opts.place.checkout, &opts.tools, frame);
        let pointer = crate::engine_answer::load_pointer(cx, store, &name).expect("the store reads").expect("a stored answer").expect("its pointer reads");
        let (kept, root) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
        let names = root_names(opts.place.checkout, &opts.tools, &tree, frame);
        let stored = load_root(cx, store, names.checkout.as_str()).expect("the store reads").expect("a rows root").expect("the rows root reads");
        let by_tree = load_root(cx, store, names.tree.as_str()).expect("the store reads").expect("a root by the tree").expect("it reads");
        assert_eq!(stored.key, by_tree.key, "the checkout's and the tree's names hold one root");
        let whole = facts.owed == 0 && facts.printed.replayable;
        assert_eq!(certified(&stored, &tree).expect("the root's summary reads"), whole, "the certificate from the root alone");
        assert!(!certified(&stored, &other).expect("the root's summary reads"), "no certificate for another tree");
        let rows = load_rows(cx, store, &stored, opts.heap.shared()).expect("the store reads").expect("the rows read");
        assert_eq!(rows.facts(), pointer.facts, "the root's summary holds the pointer's facts");
        let paged = PagesAnswer::new(&kept, root.bytes, pointer.facts, Some(tree));
        let (names_asked, cells, files) = universe(&kept.answer);
        let from_pages = ask(&paged, &names_asked, &cells, &files, &tree, &other);
        let from_rows = ask(&rows, &names_asked, &cells, &files, &tree, &other);
        // SIZE: bytes a row and a read in each form.
        let held = rows.view().rows_bytes();
        let page_reads: u64 = kept.answer.rows.values().map(|row| n64(row.reads.len())).sum();
        let page_rows = n64(kept.answer.rows.len());
        println!(
            "rows form: {} rows, {} B a row (fixed and cold), {} B a read ({} reads), {} B a define, {} B whole ({} B a row with everything)",
            held.rows,
            ratio(n64(held.fixed + held.cold), n64(held.rows)),
            ratio(n64(held.reads), n64(held.read_count)),
            held.read_count,
            ratio(n64(held.defines), n64(held.define_count)),
            rows.bytes(),
            ratio(rows.bytes(), n64(held.rows)),
        );
        println!(
            "page form: {page_rows} rows, {page_reads} reads, {} B whole: {} B a row with everything, {} B whole a read",
            root.bytes,
            ratio(root.bytes, page_rows),
            ratio(root.bytes, page_reads),
        );
        assert!(held.rows > 0 && held.read_count > 0, "the fixture has rows and reads: {held:?}");
        (from_pages, from_rows)
    });
    assert!(!pages.rows.is_empty() && !pages.readers.is_empty(), "the fixture asks something: {} names, {} cells", pages.rows.len(), pages.readers.len());
    assert_eq!(rows.rows, pages.rows, "rows");
    assert_eq!(rows.reads, pages.reads, "reads");
    assert_eq!(rows.defines, pages.defines, "defines");
    assert_eq!(rows.stands, pages.stands, "stands");
    assert_eq!(rows.saw, pages.saw, "saw");
    assert_eq!(rows.digests, pages.digests, "digests");
    assert_eq!(rows.readers, pages.readers, "readers");
    assert_eq!(rows.file_tasks, pages.file_tasks, "a file's tasks");
    assert_eq!(rows.owed, pages.owed, "owed tasks");
    assert_eq!(rows.owed_cells, pages.owed_cells, "owed cells");
    assert_eq!(rows.with_findings, pages.with_findings, "rows with findings");
    assert_eq!(rows.certificate, pages.certificate, "the certificate");
    assert_eq!(rows, pages);
}

/// One write of a commit, as a test's sink saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Put(Key, List<u8>),
    Pin(std::string::String, List<Key>),
}

/// The store as a model: records by key, names to keys, each op applied at once (no generation
/// flip: what a cut leaves is what the ops before it did).
#[derive(Clone, Default)]
pub(super) struct Memory {
    pub(super) records: StdMap<[u8; 32], List<u8>>,
    pub(super) pins: StdMap<std::string::String, List<Key>>,
}

impl Memory {
    pub(super) fn apply(&mut self, ops: &[Op]) {
        for op in ops {
            match op {
                Op::Put(key, bytes) => {
                    self.records.insert(key.0.0, bytes.clone());
                }
                Op::Pin(name, keys) => {
                    self.pins.insert(name.clone(), keys.clone());
                }
            }
        }
    }

    pub(super) fn root(&self, name: &str) -> Option<&[u8]> {
        let keys = self.pins.get(name)?;
        assert_eq!(keys.len(), 1, "{name} pins one root");
        self.records.get(&keys[0].0.0).map(|bytes| bytes.as_slice())
    }

    /// The rows answer `name` pins (a root must be pinned).
    pub(super) fn open(&self, name: &str) -> Result<RowsAnswer<'_>, RowsUnreadable> {
        let root = self.root(name).unwrap_or_else(|| panic!("{name} pins a root"));
        RowsAnswer::open(root, |one: &SegmentRef| self.records.get(&one.id.0).map(|bytes| bytes.as_slice()), Plant::None, blaze_own::testing::heap().shared())
    }
}

/// A sink that records the writes in order.
#[derive(Default)]
pub(super) struct Recording {
    pub(super) ops: List<Op>,
}

impl AnswerSink for Recording {
    type Error = core::convert::Infallible;

    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), core::convert::Infallible> {
        self.ops.push(Op::Put(key, bytes.to_vec()));
        Ok(())
    }

    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), core::convert::Infallible> {
        self.ops.push(Op::Pin(name.to_owned(), keys.to_vec()));
        Ok(())
    }
}

pub(super) const CELL: Cell = Cell([1; 16]);

pub(super) fn task_name(i: u32) -> Name {
    Name::new(Kind::BodyCheck, ContentId::of("engine_answer_rows_tests task", &i.to_le_bytes()))
}

pub(super) fn file_name() -> Name {
    Name::new(Kind::FileItems, ContentId::of("engine_answer_rows_tests file", b"src/lib.rs"))
}

pub(super) fn cell_of(i: u32) -> CellId {
    CellId { of: task_name(i), cell: CELL }
}

pub(super) fn digest(i: u32, version: u32) -> ContentId {
    let mut input = [0u8; 8];
    input[..4].copy_from_slice(&i.to_le_bytes());
    input[4..].copy_from_slice(&version.to_le_bytes());
    ContentId::of("engine_answer_rows_tests digest", &input)
}

/// A row to put: its name, what it read, what it defines.
pub(super) type Put = (Name, List<CellId>, List<(Cell, ContentId)>);

pub(super) fn put_of(i: u32, reads: impl Iterator<Item = u32>, version: u32) -> Put {
    (task_name(i), reads.map(cell_of).collect(), vec![(CELL, digest(i, version))])
}

pub(super) fn tools() -> Tools {
    Tools([blaze_drive::Tool(ContentId::of("engine_answer_rows_tests tool", b"t")); blaze_drive::key::KINDS])
}

/// One request over `last` putting `puts` for `tree`, written to a recording sink: its ops.
pub(super) fn commit(last: &AnswerView<'_>, counts: Counts, plant: Plant, tree: &TreeDigest, puts: &[Put], names: &RootNames<'_>) -> List<Op> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut next = RowsNext::new(last, counts, plant, alloc);
    for (name, reads, defines) in puts {
        let row = NextRow {
            key: Key(ContentId::of("engine_answer_rows_tests key", &name.subject.0)),
            out: ContentId::of("engine_answer_rows_tests out", &name.subject.0),
            phase: Phase::new(Stage::Bodies, 0),
            file: file_name(),
            source: ContentId::of("engine_answer_rows_tests source", &name.subject.0),
            flags: Flags(0),
            findings: 0,
            record: Key(ContentId::of("engine_answer_rows_tests record", &name.subject.0)),
        };
        next.put(*name, &row, reads.iter().copied(), defines).expect("a row is put");
    }
    assert!(next.moved().count() <= puts.len(), "a request moves at most the cells its rows define");
    let counts = next.counts();
    let facts = Facts { files: counts.files, printed: Printed { names: true, bodies_whole: true, replayable: true }, findings: counts.findings, owed: 0 };
    let effects = next.finish(tree, &summary(&facts), &LinesKey([0; 32])).expect("the commit");
    let mut sink = Recording::default();
    let lines = Key(ContentId([0; 32]));
    match write_effects(&mut sink, names, last.segments(), &effects, lines, alloc) {
        Ok(_) => {}
        Err(never) => match never {},
    }
    sink.ops
}

/// The ops of one request over the answer `memory` pins under the checkout's name.
pub(super) fn commit_over(memory: &Memory, plant: Plant, tree: &TreeDigest, puts: &[Put], names: &RootNames<'_>) -> List<Op> {
    let last = memory.open(names.checkout.as_str()).expect("the last answer opens");
    commit(last.view(), last.counts(), plant, tree, puts, names)
}

/// NOTHING WHOLE: on a 2,000-task answer, one row's question touches one row, and one row and
/// one cell's readers under 1% of the answer's bytes; the rows with findings, from their own
/// column, touch no row when none has findings (engine_answer_columns_tests.rs holds every
/// column's count against the answer's size).
#[test]
fn one_question_touches_one_row() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let tree = TreeDigest([1; 32]);
    let names = root_names("ws", &tools(), &tree, cx.frame());
    let puts: List<Put> = (0..2000).map(|i| put_of(i, i.saturating_sub(20)..i, 0)).collect();
    let alloc = blaze_own::testing::heap().alloc();
    let empty = AnswerView::empty(Plant::None, super::engine_check_tests::opts(&io, 2, false).heap.shared());
    let mut memory = Memory::default();
    memory.apply(&commit(&empty, Counts::default(), Plant::None, &tree, &puts, &names));
    let answer = memory.open(names.checkout.as_str()).expect("the answer opens");
    let total = answer.bytes();
    let task = answer.task(&task_name(1000)).expect("task 1000 is named");
    let row = answer.row(task).expect("its row reads").expect("it has a row");
    assert_eq!(row.reads, 20, "task 1000 read twenty cells");
    let cell = answer.cell(&cell_of(999)).expect("task 999's cell is named");
    let readers = answer.readers(cell, alloc).expect("its readers read");
    assert_eq!(readers.len(), 20, "tasks 1000 to 1019 read task 999's cell");
    let touched = answer.touched();
    println!("one row and one cell's readers of 2,000 rows: {touched:?} of {total} B");
    assert_eq!(touched.rows, 1, "one row's question touched one row: {touched:?}");
    assert!(touched.bytes * 100 < total, "under 1% of the answer's {total} B: {touched:?}");
    // The rows with findings come from their column: none here, so no row is read.
    assert!(answer.with_findings(alloc).expect("the rows with findings read").is_empty(), "no row has findings");
    assert_eq!(answer.touched().rows, 1, "the findings column read no row: {:?}", answer.touched());
}

/// A root for another tree: no certificate for the tree it does not answer, `nothing_changed`
/// says so from the root alone, the tree's own name keeps the last tree's root for a second
/// worktree, and a row's standing is still its generations'.
#[test]
fn another_tree_is_no_certificate() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let (first, second) = (TreeDigest([1; 32]), TreeDigest([2; 32]));
    let (names_1, names_2) = (root_names("ws", &tools(), &first, cx.frame()), root_names("ws", &tools(), &second, cx.frame()));
    let alloc = blaze_own::testing::heap().alloc();
    let empty = AnswerView::empty(Plant::None, super::engine_check_tests::opts(&io, 2, false).heap.shared());
    let mut memory = Memory::default();
    memory.apply(&commit(&empty, Counts::default(), Plant::None, &first, &[put_of(0, 0..0, 0), put_of(1, 0..1, 0)], &names_1));
    let ops = commit_over(&memory, Plant::None, &second, &[], &names_2);
    memory.apply(&ops);
    let root = memory.root(names_2.checkout.as_str()).expect("the checkout pins the second root");
    assert!(!nothing_changed(root, &first).expect("the root reads"), "the second root does not answer the first tree");
    assert!(nothing_changed(root, &second).expect("the root reads"), "the second root answers the second tree");
    let stored = RowsRoot { key: root_key(root), bytes: root };
    assert!(!certified(&stored, &first).expect("the summary reads"), "no certificate for the first tree");
    assert!(certified(&stored, &second).expect("the summary reads"), "a certificate for the second tree");
    let answer = memory.open(names_2.checkout.as_str()).expect("the second answer opens");
    assert!(!answer.certificate(&first) && answer.certificate(&second), "the seam's certificate is the root's");
    assert_eq!(answer.view().health(), Health::Fine, "two requests call for no compaction");
    let reader = answer.task(&task_name(1)).expect("task 1 is named");
    assert_eq!(answer.stands(reader), Some(true), "a carried row whose read did not move stands by its generation");
    let kept = memory.open(names_1.tree.as_str()).expect("the first tree's name still opens");
    assert!(kept.certificate(&first) && kept.serial() < answer.serial(), "the first tree's root is the first request's");
}

/// Control: a row whose read moved after it (a stale generation) is not served as standing, nor
/// its read as seen; the same digest put again moves nothing, and the reader put again stands.
/// The owed cells are the engine's to give (`Work::owed_cells`, `RowsNext::owe_cells`): a commit
/// that gives none owes none (engine_answer_columns_tests.rs gives them).
#[test]
fn a_stale_row_does_not_stand() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let tree = TreeDigest([1; 32]);
    let names = root_names("ws", &tools(), &tree, cx.frame());
    let alloc = blaze_own::testing::heap().alloc();
    let empty = AnswerView::empty(Plant::None, super::engine_check_tests::opts(&io, 2, false).heap.shared());
    let mut base = Memory::default();
    // Task 1 reads task 0's cell.
    base.apply(&commit(&empty, Counts::default(), Plant::None, &tree, &[put_of(0, 0..0, 0), put_of(1, 0..1, 0)], &names));
    let stands = |puts: &[Put]| -> (Option<bool>, bool, List<CellId>) {
        let mut memory = base.clone();
        let ops = commit_over(&memory, Plant::None, &tree, puts, &names);
        memory.apply(&ops);
        let answer = memory.open(names.checkout.as_str()).expect("the answer opens");
        let reader = answer.task(&task_name(1)).expect("task 1 is named");
        let cell = answer.cell(&cell_of(0)).expect("task 0's cell is named");
        let owed = answer.owed_cells(alloc).expect("the owed cells read").iter().map(|(c, _)| answer.cell_id(*c).expect("a cell's id")).collect();
        (answer.stands(reader), answer.saw(reader, cell).expect("a read found by search"), owed)
    };
    assert_eq!(stands(&[put_of(0, 0..0, 1)]), (Some(false), false, vec![]), "task 0's cell moved after task 1's row");
    assert_eq!(stands(&[put_of(0, 0..0, 0)]), (Some(true), true, vec![]), "the same digest put again moves nothing (early cutoff)");
    assert_eq!(stands(&[put_of(0, 0..0, 1), put_of(1, 0..1, 0)]), (Some(true), true, vec![]), "task 1 put again after the move stands");
}

/// The write's order, cut after every op of a second request: what the checkout's name pins
/// always opens whole (the first answer or the second), and the whole write gives the second.
/// Control: the root put first (`Plant::RootFirst`) leaves some cut pinning a root whose
/// segment is not put.
#[test]
fn a_cut_write_leaves_a_whole_answer() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let tree = TreeDigest([1; 32]);
    let names = root_names("ws", &tools(), &tree, cx.frame());
    let alloc = blaze_own::testing::heap().alloc();
    let empty = AnswerView::empty(Plant::None, super::engine_check_tests::opts(&io, 2, false).heap.shared());
    let mut base = Memory::default();
    base.apply(&commit(&empty, Counts::default(), Plant::None, &tree, &[put_of(0, 0..0, 0), put_of(1, 0..1, 0)], &names));
    let first = base.open(names.checkout.as_str()).expect("the first answer opens").serial();
    let puts = [put_of(0, 0..0, 1), put_of(2, 0..2, 0)];
    let cut_opens = |plant: Plant| -> List<Result<Serial, RowsUnreadable>> {
        let ops = commit_over(&base, plant, &tree, &puts, &names);
        assert!(ops.len() > 4, "a request writes segments, a root and four pins: {}", ops.len());
        (0..=ops.len())
            .map(|cut| {
                let mut memory = base.clone();
                memory.apply(&ops[..cut]);
                memory.open(names.checkout.as_str()).map(|answer| {
                    answer.view().verify().expect("an opened answer verifies");
                    answer.serial()
                })
            })
            .collect()
    };
    let opened = cut_opens(Plant::None);
    for (cut, got) in opened.iter().enumerate() {
        match got {
            Ok(serial) => assert!(*serial == first || *serial > first, "cut {cut}: the first answer or the second, not {serial:?}"),
            Err(why) => panic!("cut {cut}: the pinned answer does not open: {why}"),
        }
    }
    assert!(matches!(opened.last(), Some(Ok(serial)) if *serial > first), "the whole write gives the second answer: {:?}", opened.last());
    let planted = cut_opens(Plant::RootFirst);
    assert!(
        planted.iter().any(|got| matches!(got, Err(RowsUnreadable::Answer(LastError::Answer(AnswerError::MissingSegment { .. }))))),
        "control: the root put first pins a root whose segment is not put at some cut"
    );
}
