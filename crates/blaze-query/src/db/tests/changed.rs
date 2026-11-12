//! The changed set's deterministic simulation: random programs of queries (sums, cutoffs,
//! reads chosen by a value so a row's edges change, reads the engine cannot see), run through
//! random sequences of writes, pulls, evictions, epochs and asks, on one thread, every choice
//! drawn by Hegel. The engine code that runs is the real one: the feeds in `function`, the
//! fold, the reverse index and the walk.
//!
//! The oracle is a from-scratch evaluation of the program after every step. The controls plant
//! one bug each ([`Plant`]) and run a directed program that passes as the engine is and fails
//! with the plant, as that bug does.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    super::{
        Changed, Untracked,
        frontier::Plant,
        plumbing::{changed_since, evict_values},
        report_untracked_read,
    },
    *,
};

/// The thread each run of [`Flow`] was on, by the address of the database it ran in: what
/// tells a row verified on the walker's thread from one handed to the pool.
static RAN_ON: Lock<Vec<(usize, std::thread::ThreadId)>> =
    Lock::new("test.query.changed.ran_on", Vec::new());

/// The threads the runs of `db`'s rows were on since the last take.
fn take_ran_on(db: &TestDb) -> Vec<std::thread::ThreadId> {
    let of = std::ptr::from_ref(db).addr();
    let mut own = Vec::new();
    RAN_ON.lock().retain(|&(it, thread)| {
        if it == of {
            own.push(thread);
        }
        it != of
    });
    own
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FlowOp {
    /// The sum of every read.
    Sum,
    /// That sum modulo 3: a value that often stays when its reads change (the cutoff).
    Mod3,
    /// The greatest read: another cutoff.
    Max,
    /// Reads its first, then one of the rest chosen by the first's value: its edges change
    /// when the first does.
    Pick,
    /// The sum of every read and of the world, which the engine cannot see: an untracked
    /// read.
    World,
}

#[derive(Clone, Debug)]
pub(super) struct FlowNode {
    op: FlowOp,
    deps: Vec<Dep>,
}

struct Flow;

impl Spec for Flow {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "Flow";
    const EVICT: bool = true;

    fn cache() -> &'static Ingredient {
        ingredient!(function::<Flow>)
    }

    fn compute(db: &TestDb, key: Node) -> u64 {
        RAN_ON.lock().push((std::ptr::from_ref(db).addr(), std::thread::current().id()));
        flowed::<Flow>(db, key)
    }
}

/// The value of node `key` of the database's flow, its query reads made through `Q`.
fn flowed<Q: Spec<Key = Node, Value = u64>>(db: &TestDb, key: Node) -> u64 {
    let at = *interned_fields::<Indices>(db.storage(), key.0) as usize;
    let spec = db.flow.lock()[at].clone();
    let read = |dep: Dep| match dep {
        Dep::Input(input) => {
            let cell = Cell(db.cells.lock()[input]);
            cell.a(db)
        }
        Dep::Query(query) => *fetch::<Q>(db, node(db, query)),
    };
    match spec.op {
        FlowOp::Pick => {
            let first = read(spec.deps[0]);
            picked(first, spec.deps.len()).map_or(first, |at| mix(first, read(spec.deps[at])))
        }
        FlowOp::World => {
            report_untracked_read(
                db,
                Untracked { what: "the simulation's world, which no tracked read stands for" },
            );
            let world = db.world.load(Ordering::Relaxed);
            spec.deps.iter().fold(world, |acc, &dep| acc.wrapping_add(read(dep)))
        }
        op => fold(op, spec.deps.iter().map(|&dep| read(dep))),
    }
}

/// The flow again as a query of its own, for the one test that counts its rows' work: the
/// engine's counts are a process's, by query, and no other test runs this one
/// ([`a_first_edit_after_a_build`], one at a time under [`BUILT`]). The counts are read, never
/// taken: a take is the process's, and would take the counts of the tests beside it.
struct Built;

impl Spec for Built {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "Built";

    fn cache() -> &'static Ingredient {
        ingredient!(function::<Built>)
    }

    fn compute(db: &TestDb, key: Node) -> u64 {
        flowed::<Built>(db, key)
    }
}

/// Held by each run of [`a_first_edit_after_a_build`]: `Built`'s counts are one run's.
static BUILT: Lock<()> = Lock::new("test.query.changed.built", ());

/// `Built`'s rows proved unchanged without running, so far in this process.
fn built_verified() -> u64 {
    let ingredient = <Built as crate::db::plumbing::Function>::ingredient().index();
    crate::db::counts::now(ingredient, crate::db::counts::VERIFIED)
}

/// What [`a_first_edit_after_a_build`] saw: the settle at the build's end, the first edit's
/// walk, the rows the edit's changed set named and the rows the edit proved unchanged.
#[derive(Debug, PartialEq, Eq)]
struct FirstEdit {
    /// Nodes the settle at the build's end left to the edit's walk.
    left: u64,
    /// Rows the edit's walk asked.
    asked: u64,
    /// Rows it set apart, so that a pull of one walks its edges again.
    marked: u64,
    /// The edit's walk: the nodes that had said they changed before it, the reverse edges it
    /// followed and the rows it asked.
    edit: (u64, u64, u64),
    /// The rows named to a host that asks from the build's end.
    named: usize,
    /// Rows the edit proved unchanged without running, in its walk and in the host's pull
    /// of what it holds (the application's row, the library's top and the second package's).
    verified: u64,
}

/// How many times the build writes the second package's configuration: more views than the
/// written-field history holds, as a build's hundreds of writes are.
const BUILD_WRITES: u64 = 70;

/// The inputs a wide library reads beside its configuration: enough that the summary of its
/// top holds every bit, as a real library's does, and no write is passed over by it.
const WIDE_INPUTS: usize = 200;

/// A build of a library of `library` pairs of rows under one top row, an application row and
/// a second package's row, then a first edit, as a host does them: row `2i` reads the
/// library's configuration (input 0) and row `2i + 1` reads row `2i`; the top reads every row
/// `2i + 1`; the application's row reads its source (input 1) and the top; the last row reads
/// the second package's configuration (input 2). In a `wide` library row `2i` also reads one
/// of [`WIDE_INPUTS`] inputs of its own. The build makes every row, then the library's
/// configuration is written (a build script's result applied after the rows that read it
/// were made) and the build pulls every row again: each library row runs again and says it
/// changed, with no walk. Then the second package's configuration is written
/// [`BUILD_WRITES`] times and its row pulled each time, so the library's rows were verified
/// more views back than the written-field history holds. At its end the host settles the
/// changed set (off the answer's path).
///
/// The edit writes the application's source. A narrow library's top is pulled first, before
/// any walk (as a host's maintainers pull before its body check walks): only the floor can
/// prove it without its edges. Then the host asks for the changed set since the build's end
/// (in a wide library the walk's verification of the application's row is what asks the top:
/// only the reach can prove it) and pulls the rows it holds. Every answer is checked against
/// a run from scratch.
fn a_first_edit_after_a_build(library: usize, wide: bool, plant: Plant) -> FirstEdit {
    let _one = BUILT.lock();
    let mut flow: Vec<FlowNode> = Vec::with_capacity(2 * library + 3);
    for at in 0..library {
        flow.push(match wide {
            true => sum(&[Dep::Input(0), Dep::Input(3 + at % WIDE_INPUTS)]),
            false => sum(&[Dep::Input(0)]),
        });
        flow.push(sum(&[Dep::Query(2 * at)]));
    }
    let top = flow.len();
    flow.push(sum(&(0..library).map(|at| Dep::Query(2 * at + 1)).collect::<Vec<_>>()));
    let application = flow.len();
    flow.push(sum(&[Dep::Input(1), Dep::Query(top)]));
    let second = flow.len();
    flow.push(sum(&[Dep::Input(2)]));
    let rows = flow.len();
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = flow.clone();
    let mut inputs = vec![1u64, 10, 100];
    inputs.extend((0..WIDE_INPUTS as u64).map(|at| at % 7));
    let cells: Vec<Cell> = inputs.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let pull = |db: &TestDb, inputs: &[u64], held: &[usize]| {
        let model = evaluate(&flow, inputs, 0);
        for &at in held {
            assert_eq!(*fetch::<Built>(db, node(db, at)), model[at], "row {at}");
        }
    };
    let every: Vec<usize> = (0..rows).collect();
    // The build: every row made, the configuration applied, every row pulled again.
    pull(&db, &inputs, &every);
    inputs[0] = 5;
    cells[0].set_a(&mut db, 5);
    pull(&db, &inputs, &every);
    for write in 1..=BUILD_WRITES {
        inputs[2] = 100 + write;
        cells[2].set_a(&mut db, 100 + write);
        pull(&db, &inputs, &[second]);
    }
    // Its end, after the answer: the host settles what the build's pulls left.
    let ended = db.storage_mut().settle_changed_set();
    let since = db.storage().view();
    let before = built_verified();
    // The first edit.
    inputs[1] = 11;
    cells[1].set_a(&mut db, 11);
    if !wide {
        pull(&db, &inputs, &[top]);
    }
    let changed = changed_since::<Built>(&db, since);
    let Changed::Keys(keys) = changed else { panic!("unknown: {changed:?}") };
    let edit = db.storage().changed_set_work();
    pull(&db, &inputs, &[application, top, second]);
    let verified = built_verified() - before;
    // And every row answers as a run from scratch does.
    pull(&db, &inputs, &every);
    if plant == Plant::None {
        assert_eq!(keys, [node(&db, application)], "the edit reaches the application's row");
    }
    FirstEdit {
        left: ended.left,
        asked: edit.visited,
        marked: edit.marked,
        edit: (edit.said, edit.edges, edit.visited),
        named: keys.len(),
        verified,
    }
}

/// The invariant: the work of the first edit after a build is bounded by what the edit
/// reaches, not by the database's size. A build leaves every row it made again saying it
/// changed; each reader of one was verified after it changed, so the settle at the build's
/// end takes every one out of the next walk's start with a load a reader, running nothing,
/// and its view becomes the floor. The edit's walk starts from the edit's write alone: one
/// reverse edge, one row asked, none set apart, one row named. The library's top, which the
/// application's row reads and which was verified more views back than the written-field
/// history holds, is proved with no look at what it read: pulled before the walk, from the
/// floor (a narrow library, whose summary the edit's write shares no bit with); asked by the
/// walk's own verification, by being outside the walk's reach (a wide one, whose summary
/// holds every bit). Two rows proved unchanged (it and the second package's), the same in a
/// database of 203 rows and one of 4,003.
#[test]
fn the_first_edit_after_a_build_costs_what_it_reaches_at_two_sizes() {
    let bounded =
        FirstEdit { left: 0, asked: 1, marked: 0, edit: (0, 1, 1), named: 1, verified: 2 };
    for wide in [false, true] {
        let small = a_first_edit_after_a_build(100, wide, Plant::None);
        let large = a_first_edit_after_a_build(2000, wide, Plant::None);
        assert_eq!(small, bounded, "wide: {wide}");
        assert_eq!(large, small, "the first edit's work grew with the database (wide: {wide})");
    }
}

/// The control of [`the_first_edit_after_a_build_costs_what_it_reaches_at_two_sizes`]: a
/// walk that asks every reader of a changed node, those verified since it changed too, meets
/// the build's rows as a wide level and sets them all apart: nothing the build left can be
/// settled, the edit's walk is the database's size, and the oracle says so at both sizes.
#[test]
fn the_oracle_catches_a_walk_that_asks_the_readers_that_saw_the_change() {
    let small = a_first_edit_after_a_build(100, true, Plant::AsksReadersThatSawTheChange);
    let large = a_first_edit_after_a_build(2000, true, Plant::AsksReadersThatSawTheChange);
    assert!(small.left >= 200 && small.marked >= 200 && small.asked >= 200, "{small:?}");
    assert!(large.left >= 4000 && large.marked >= 4000 && large.asked >= 4000, "{large:?}");
    assert_ne!(small, large);
}

/// The second control of it: with no floor kept, the edit's walk is as small, but the narrow
/// library's top, pulled before the walk, is proved by a walk of everything it read, every
/// row of the library, and the oracle says so at both sizes.
#[test]
fn the_oracle_catches_an_edit_that_walks_what_the_build_verified() {
    let small = a_first_edit_after_a_build(100, false, Plant::KeepsNoFloor);
    let large = a_first_edit_after_a_build(2000, false, Plant::KeepsNoFloor);
    assert_eq!((small.asked, small.marked, small.named), (1, 0, 1), "{small:?}");
    assert!(small.verified >= 200, "{small:?}");
    assert!(large.verified >= 4000, "{large:?}");
}

/// The third control of it: with no reach worked out, the wide library's top, asked by the
/// walk's verification of the application's row, is proved by a walk of everything it read
/// (its summary holds every bit, so neither its own view nor the floor proves it), and the
/// oracle says so at both sizes.
#[test]
fn the_oracle_catches_a_walk_with_no_reach() {
    let small = a_first_edit_after_a_build(100, true, Plant::WalksWithNoReach);
    let large = a_first_edit_after_a_build(2000, true, Plant::WalksWithNoReach);
    assert_eq!((small.asked, small.marked, small.named), (1, 0, 1), "{small:?}");
    // Each row the top reads at least (those hold from the floor, their summaries narrow).
    assert!(small.verified >= 100, "{small:?}");
    assert!(large.verified >= 2000, "{large:?}");
}

/// Row 2 reads input 1 and row 1; row 1 reads row 0; row 0 reads input 0. Both inputs are
/// written. The walk's first level is row 2 (it reads input 1) and row 0; row 2, the older
/// node, is verified first and reads row 1, which is behind row 0, which is behind the
/// write. Returns what row 2 answers after the walk (the model says 12).
fn a_row_read_through_a_row_that_is_behind(plant: Plant) -> u64 {
    let flow = vec![
        sum(&[Dep::Input(0)]),
        sum(&[Dep::Query(0)]),
        sum(&[Dep::Input(1), Dep::Query(1)]),
    ];
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = flow.clone();
    let mut inputs = vec![1u64, 1];
    let cells: Vec<Cell> = inputs.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    assert_eq!(*fetch::<Flow>(&db, node(&db, 2)), 2);
    let since = db.storage().view();
    inputs[0] = 7;
    cells[0].set_a(&mut db, 7);
    inputs[1] = 5;
    cells[1].set_a(&mut db, 5);
    _ = changed_since::<Flow>(&db, since);
    assert_eq!(evaluate(&flow, &inputs, 0)[2], 12);
    *fetch::<Flow>(&db, node(&db, 2))
}

/// The invariant: the walk's reach holds every row behind a row that is behind, at any
/// depth: a row a suspect reads is proved unchanged by the reach only when no write reaches
/// it.
#[test]
fn a_row_behind_a_row_that_is_behind_is_inside_the_reach() {
    assert_eq!(a_row_read_through_a_row_that_is_behind(Plant::None), 12);
}

/// The control of [`a_row_behind_a_row_that_is_behind_is_inside_the_reach`]: a reach of one
/// level takes row 1 as unreached, row 2 is made from its old value, and the oracle sees the
/// stale answer.
#[test]
fn the_oracle_catches_a_reach_of_one_level() {
    assert_eq!(a_row_read_through_a_row_that_is_behind(Plant::ReachesOneLevel), 6);
}

/// A row with 70 readers, each read by nothing, and one row that reads another input; the
/// first input is written and the walk marks the wide level without verifying it; nothing
/// pulls the marked rows; the other input is written and a second walk ends, whose view is
/// the floor. Returns what a marked row answers then (the model says 7).
fn a_marked_row_asked_after_a_later_walk(plant: Plant) -> u64 {
    const WIDE: usize = 70;
    let mut flow: Vec<FlowNode> = (0..WIDE).map(|_| sum(&[Dep::Input(0)])).collect();
    flow.push(sum(&[Dep::Input(1)]));
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = flow;
    let cells: Vec<Cell> = (0..2).map(|_| Cell::new(&db, 1, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    for at in 0..=WIDE {
        assert_eq!(*fetch::<Flow>(&db, node(&db, at)), 1);
    }
    let since = db.storage().view();
    cells[0].set_a(&mut db, 7);
    _ = changed_since::<Flow>(&db, since);
    assert_eq!(db.storage().changed_set_work().marked, WIDE as u64);
    cells[1].set_a(&mut db, 3);
    _ = changed_since::<Flow>(&db, since);
    assert_eq!(*fetch::<Flow>(&db, node(&db, WIDE)), 3);
    *fetch::<Flow>(&db, node(&db, 5))
}

/// The invariant: the floor never holds a row a walk set apart. A row marked by a walk and
/// asked for only after a later walk ended is run then, and answers as the model does.
#[test]
fn a_row_set_apart_is_not_held_by_a_later_floor() {
    assert_eq!(a_marked_row_asked_after_a_later_walk(Plant::None), 7);
}

/// The control of [`a_row_set_apart_is_not_held_by_a_later_floor`]: a floor that holds the
/// rows set apart too gives the marked row's old value.
#[test]
fn the_oracle_catches_a_floor_under_a_row_set_apart() {
    assert_eq!(a_marked_row_asked_after_a_later_walk(Plant::FloorsRowsSetApart), 1);
}

/// A row that picks what it reads by an input, under a reader: input 0 is 0, so row 0 reads
/// input 1 (31) and is 31; row 1 reads row 0. Input 0 is written to 1: row 0 runs again,
/// reads input 2 (0) and is 31 again, so its stamp does not move, and the walk ends without
/// reaching row 1, which the host's pull then marks unchanged with no look at its edges.
/// Input 2 is written to 5 and the host pulls row 1 before any walk. Returns what it answers
/// (the model says 36).
fn a_reader_asked_after_its_dependency_reads_another_input(plant: Plant) -> u64 {
    let flow = vec![
        FlowNode { op: FlowOp::Pick, deps: vec![Dep::Input(0), Dep::Input(1), Dep::Input(2)] },
        sum(&[Dep::Query(0)]),
    ];
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = flow.clone();
    let mut inputs = vec![0u64, 31, 0];
    let cells: Vec<Cell> = inputs.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    assert_eq!(*fetch::<Flow>(&db, node(&db, 1)), 31);
    let since = db.storage().view();
    inputs[0] = 1;
    cells[0].set_a(&mut db, 1);
    assert_eq!(changed_since::<Flow>(&db, since), Changed::Keys(Vec::new()), "row 0 is 31 again");
    assert_eq!(*fetch::<Flow>(&db, node(&db, 1)), 31);
    inputs[2] = 5;
    cells[2].set_a(&mut db, 5);
    assert_eq!(evaluate(&flow, &inputs, 0)[1], 36);
    *fetch::<Flow>(&db, node(&db, 1))
}

/// The invariant: a row's input summary holds what its dependencies read now, at every
/// write. A dependency that runs again, reads another input and gives an equal value moves
/// no stamp; its reader, marked unchanged with no look at its edges, still learns of the new
/// input ([`crate::db::Storage::complete_summaries`]), so a write of that input is not
/// passed over by the reader's summary.
#[test]
fn a_reader_learns_what_its_dependency_reads_now() {
    assert_eq!(a_reader_asked_after_its_dependency_reads_another_input(Plant::None), 36);
}

/// The control of [`a_reader_learns_what_its_dependency_reads_now`]: with the gain kept to
/// the dependency, the reader's summary shares no bit with the write and it answers its old
/// value (the engine before 2026-11-12).
#[test]
fn the_oracle_catches_a_reader_that_keeps_its_old_summary() {
    assert_eq!(
        a_reader_asked_after_its_dependency_reads_another_input(Plant::LeavesReadersSummaries),
        31
    );
}
