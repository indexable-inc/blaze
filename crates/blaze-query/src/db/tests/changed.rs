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
/// ([`a_first_edit_after_a_build`], one at a time under [`BUILT`]).
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

/// `Built`'s rows proved unchanged without running since the last take.
fn built_verified() -> u64 {
    let mut verified = 0;
    crate::db::take_counts(|name, counts| {
        if name == "Built" {
            verified += counts.verified;
        }
    });
    verified
}

/// Which of a `Pick`'s `deps` reads its first's value `first` chooses, if it has more than one.
fn picked(first: u64, deps: usize) -> Option<usize> {
    (deps > 1).then(|| 1 + (first % (deps as u64 - 1)) as usize)
}

fn mix(first: u64, second: u64) -> u64 {
    first.wrapping_mul(31).wrapping_add(second)
}

fn fold(op: FlowOp, values: impl Iterator<Item = u64>) -> u64 {
    match op {
        FlowOp::Max => values.max().unwrap_or(0),
        FlowOp::Mod3 => values.fold(0u64, |acc, it| acc.wrapping_add(it)) % 3,
        _ => values.fold(0u64, |acc, it| acc.wrapping_add(it)),
    }
}

/// The reference model: every node's value from scratch.
fn evaluate(flow: &[FlowNode], inputs: &[u64], world: u64) -> Vec<u64> {
    let mut values: Vec<u64> = Vec::with_capacity(flow.len());
    for spec in flow {
        let read = |dep: Dep| match dep {
            Dep::Input(input) => inputs[input],
            Dep::Query(query) => values[query],
        };
        let value = match spec.op {
            FlowOp::Pick => {
                let first = read(spec.deps[0]);
                picked(first, spec.deps.len()).map_or(first, |at| mix(first, read(spec.deps[at])))
            }
            FlowOp::World => spec.deps.iter().fold(world, |acc, &dep| acc.wrapping_add(read(dep))),
            op => fold(op, spec.deps.iter().map(|&dep| read(dep))),
        };
        values.push(value);
    }
    values
}

#[derive(Clone, Debug)]
enum Step {
    /// Sets an input (to its own value too: a write that changes nothing).
    Write(usize, u64),
    /// Changes the world an untracked read sees; a view opens with nothing written.
    World(u64),
    /// Lets go of every value, keeping stamps, edges and hashes.
    Evict,
    /// Ends an epoch: values unread for two go.
    Epoch,
    /// Asks for these nodes' values without asking for the changed set: a pull in a view no
    /// walk may ever run in.
    Pull(Vec<usize>),
    /// Folds what ran into the reverse index now, as a host does after a build's answer
    /// ([`crate::db::Storage::pack_reverse`]), where a view's boundary would fold it later.
    /// Then settles the changed set ([`crate::db::Storage::settle_changed_set`]): the nodes
    /// that said they changed whose readers have all seen it leave the next walk's start.
    Pack,
    /// Asks for the changed set since the ask `back` asks ago (0: the last; past the first:
    /// since the storage's first view), then for every node made.
    Ask(usize),
}

#[derive(Clone, Debug)]
struct Program {
    inputs: usize,
    flow: Vec<FlowNode>,
    steps: Vec<Step>,
}

fn program(tc: &TestCase) -> Program {
    let int = |low: usize, high: usize| tc.draw(gs::integers::<usize>().min_value(low).max_value(high));
    let inputs = int(1, 5);
    let nodes = int(1, 10);
    let mut flow = Vec::with_capacity(nodes);
    for at in 0..nodes {
        let op = [FlowOp::Sum, FlowOp::Mod3, FlowOp::Max, FlowOp::Pick, FlowOp::Pick, FlowOp::World]
            [int(0, 5)];
        let deps = (0..int(1, 4))
            .map(|_| {
                if at > 0 && tc.draw(gs::booleans()) {
                    Dep::Query(int(0, at - 1))
                } else {
                    Dep::Input(int(0, inputs - 1))
                }
            })
            .collect();
        flow.push(FlowNode { op, deps });
    }
    let mut steps = Vec::new();
    for _ in 0..int(1, 16) {
        steps.push(match int(0, 10) {
            0..=3 => Step::Write(int(0, inputs - 1), tc.draw(gs::integers::<u64>().max_value(6))),
            4 => Step::World(tc.draw(gs::integers::<u64>().max_value(3))),
            5 => Step::Evict,
            6 => Step::Epoch,
            7 | 8 => Step::Pull((0..int(1, 3)).map(|_| int(0, nodes - 1)).collect()),
            9 => Step::Pack,
            _ => Step::Ask(int(0, 3)),
        });
    }
    steps.push(Step::Ask(0));
    Program { inputs, flow, steps }
}

/// One ask the simulation made: the view it was made in, the step it followed, and the nodes
/// that had been made by then.
struct Asked {
    view: crate::db::View,
    step: usize,
    made: BTreeSet<usize>,
}

/// Runs `program` with `plant` in the engine and checks each ask against the model. `Err`
/// says what the oracle saw.
fn simulate(program: &Program, plant: Plant) -> Result<(), String> {
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = program.flow.clone();
    let mut inputs: Vec<u64> = (0..program.inputs as u64).collect();
    let mut world = 0u64;
    let cells: Vec<Cell> = inputs.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let nodes = program.flow.len();
    let ids: BTreeMap<u32, usize> =
        (0..nodes).map(|at| (node(&db, at).0.as_u32(), at)).collect();
    let made = |db: &TestDb| -> BTreeSet<usize> {
        db.log.lock().iter().filter(|it| it.0 == "Flow").map(|it| ids[&it.1]).collect()
    };
    // The model's values after each step; `snaps[0]` is before the first.
    let mut snaps = vec![evaluate(&program.flow, &inputs, world)];
    let first = Asked { view: db.storage().view(), step: 0, made: BTreeSet::new() };
    let mut asks: Vec<Asked> = Vec::new();
    for (at, step) in program.steps.iter().enumerate() {
        match step {
            Step::Write(input, value) => {
                inputs[*input] = *value;
                cells[*input].set_a(&mut db, *value);
            }
            Step::World(value) => {
                world = *value;
                db.world.store(world, Ordering::Relaxed);
                db.storage_mut().synthetic_write(Durability::LOW);
            }
            Step::Evict => {
                evict_values::<Flow>(&mut db);
            }
            Step::Epoch => {
                db.storage_mut().end_epoch();
            }
            Step::Pack => {
                db.storage_mut().pack_reverse();
                // And settles what the pulls since the last walk left, as a host does there.
                db.storage_mut().settle_changed_set();
                if db.storage().runs_unfolded() != 0 {
                    return Err(format!(
                        "step {at}: {} runs' edges are left to the next view's boundary after a fold",
                        db.storage().runs_unfolded()
                    ));
                }
            }
            Step::Pull(pulled) => {
                let now = evaluate(&program.flow, &inputs, world);
                for &it in pulled {
                    let got = *fetch::<Flow>(&db, node(&db, it));
                    if got != now[it] {
                        return Err(format!("step {at}: node {it} is {got}, from scratch {}", now[it]));
                    }
                }
            }
            Step::Ask(_) => {}
        }
        let now = evaluate(&program.flow, &inputs, world);
        snaps.push(now.clone());
        let Step::Ask(back) = step else { continue };
        let since = asks.len().checked_sub(1 + back).map_or(&first, |it| &asks[it]);
        let named: BTreeSet<usize> = match changed_since::<Flow>(&db, since.view) {
            Changed::Keys(keys) => keys.iter().map(|it| ids[&it.0.as_u32()]).collect(),
            Changed::Unknown(why) => return Err(format!("step {at}: the changed set is unknown: {why}")),
        };
        let then = &snaps[since.step];
        // Exact: every row made by then whose value differs is named.
        for &it in &since.made {
            if now[it] != then[it] && !named.contains(&it) {
                return Err(format!(
                    "step {at}: node {it} was {} at step {} and is {}, and the changed set since \
                     then does not name it (it names {named:?})",
                    then[it], since.step, now[it]
                ));
            }
        }
        // And only what the design says more: a value that moved in some step between.
        for &it in &named {
            let moved = (since.step..snaps.len() - 1).any(|at| snaps[at][it] != snaps[at + 1][it]);
            if !moved {
                return Err(format!(
                    "step {at}: the changed set since step {} names node {it}, whose value \
                     never moved from {}",
                    since.step, now[it]
                ));
            }
        }
        // The walk costs what it reaches, never more rows than were made.
        let work = db.storage().changed_set_work();
        let rows = made(&db).len() as u64;
        if work.visited > rows {
            return Err(format!("step {at}: the walk visited {} rows of {rows} made", work.visited));
        }
        // Every row made answers as the model does.
        for it in made(&db) {
            let got = *fetch::<Flow>(&db, node(&db, it));
            if got != now[it] {
                return Err(format!("step {at}: node {it} is {got}, from scratch {}", now[it]));
            }
        }
        asks.push(Asked { view: db.storage().view(), step: at + 1, made: made(&db) });
    }
    Ok(())
}

/// The invariant: `changed_since(V) ⊇ { k : value_now(k) ≠ value_at(V, k) }` over the keys
/// made by `V`, and every key it names more is one whose value moved in some view between
/// (a change and a change back); with no untracked read, no eviction and one walk a view it
/// equals the set. Checked against a from-scratch evaluation after every step of a random
/// program and edit sequence.
#[hegel::test]
fn the_changed_set_names_every_row_whose_value_differs_from_a_run_from_scratch(tc: TestCase) {
    let program = program(&tc);
    if let Err(why) = simulate(&program, Plant::None) {
        panic!("{why}\n{program:#?}");
    }
}

fn sum(deps: &[Dep]) -> FlowNode {
    FlowNode { op: FlowOp::Sum, deps: deps.to_vec() }
}

/// A directed program passes as the engine is and fails with `plant`, saying `says`.
fn caught(program: &Program, plant: Plant, says: &str) {
    assert_eq!(simulate(program, Plant::None), Ok(()), "the engine as it is must pass");
    let why = simulate(program, plant).expect_err("the oracle missed the planted bug");
    assert!(why.contains(says), "{why}");
}

/// Four chains of an input, its reader and that reader's reader, so some reader's node is
/// even: the plant drops the reverse edges to it, and its own reader is never verified.
#[test]
fn the_oracle_catches_a_reverse_edge_not_recorded() {
    // Three rows a chain: chain `c`'s are `3c..3c+3`.
    let flow: Vec<FlowNode> = (0..4)
        .flat_map(|chain| {
            [
                sum(&[Dep::Input(chain)]),
                sum(&[Dep::Query(3 * chain)]),
                sum(&[Dep::Query(3 * chain + 1)]),
            ]
        })
        .collect();
    let mut steps = vec![Step::Pull((0..12).collect()), Step::Ask(0)];
    steps.extend((0..4).map(|input| Step::Write(input, 10 + input as u64)));
    steps.push(Step::Ask(0));
    let program = Program { inputs: 4, flow, steps };
    caught(&program, Plant::DropsAnEdge, "does not name it");
}

/// A row that read input 1, runs again and reads input 2 instead: the plant keeps only its
/// first edges, so a write of input 2 never reaches it.
/// The fold a host asks for after a build's answer is the fold a view's boundary would have
/// made: the runs before it are in the reverse index, none is left for the next write, and
/// the walk after that write names what the write reached. Its control: with an edge dropped
/// in the fold, that walk misses a reader, and the oracle says so.
#[test]
fn a_fold_asked_for_after_the_runs_feeds_the_next_walk() {
    // The chains of `the_oracle_catches_a_reverse_edge_not_recorded`: some reader's node is
    // even, so the plant drops the edges to it.
    let flow: Vec<FlowNode> = (0..4)
        .flat_map(|chain| {
            [
                sum(&[Dep::Input(chain)]),
                sum(&[Dep::Query(3 * chain)]),
                sum(&[Dep::Query(3 * chain + 1)]),
            ]
        })
        .collect();
    let mut steps = vec![Step::Pull((0..12).collect()), Step::Pack, Step::Ask(0)];
    steps.extend((0..4).map(|input| Step::Write(input, 10 + input as u64)));
    steps.push(Step::Ask(0));
    let program = Program { inputs: 4, flow, steps };
    caught(&program, Plant::DropsAnEdge, "does not name it");
}

#[test]
fn the_oracle_catches_a_stale_edge_after_a_rows_dependencies_change() {
    let pick = FlowNode {
        op: FlowOp::Pick,
        deps: vec![Dep::Input(0), Dep::Input(1), Dep::Input(2)],
    };
    let steps = vec![
        Step::Pull(vec![0]),
        Step::Ask(0),
        Step::Write(0, 1),
        Step::Ask(0),
        Step::Write(2, 9),
        Step::Ask(0),
    ];
    let program = Program { inputs: 3, flow: vec![pick], steps };
    caught(&program, Plant::KeepsOldEdges, "does not name it");
}

/// A changed reader's own reader: the plant stops the walk at the first.
#[test]
fn the_oracle_catches_a_changed_reader_that_does_not_pass_the_walk_on() {
    let flow = vec![sum(&[Dep::Input(0)]), sum(&[Dep::Query(0)])];
    let steps = vec![Step::Pull(vec![1]), Step::Ask(0), Step::Write(0, 5), Step::Ask(0)];
    let program = Program { inputs: 1, flow, steps };
    caught(&program, Plant::StopsAtChangedReader, "does not name it");
}

/// A row that runs again to an equal value is backdated and is no change: the plant names it.
#[test]
fn the_oracle_catches_a_cutoff_not_applied() {
    let flow = vec![FlowNode { op: FlowOp::Mod3, deps: vec![Dep::Input(0)] }];
    let steps = vec![Step::Pull(vec![0]), Step::Ask(0), Step::Write(0, 3), Step::Ask(0)];
    let program = Program { inputs: 1, flow, steps };
    caught(&program, Plant::NamesEqualRows, "never moved");
}

/// A value let go between two views is still a row whose value can change: the plant passes
/// over it.
#[test]
fn the_oracle_catches_an_evicted_row_passed_over() {
    let flow = vec![sum(&[Dep::Input(0)])];
    let steps =
        vec![Step::Pull(vec![0]), Step::Ask(0), Step::Evict, Step::Write(0, 5), Step::Ask(0)];
    let program = Program { inputs: 1, flow, steps };
    caught(&program, Plant::SkipsEvictedRows, "does not name it");
}

/// A row that made an untracked read is asked by every walk: the world changes with nothing
/// written, and the row and its reader are named.
#[test]
fn an_untracked_read_puts_its_row_in_every_walk() {
    let flow = vec![FlowNode { op: FlowOp::World, deps: vec![Dep::Input(0)] }, sum(&[Dep::Query(0)])];
    let steps = vec![Step::Pull(vec![1]), Step::Ask(0), Step::World(2), Step::Ask(0)];
    let program = Program { inputs: 1, flow: flow.clone(), steps };
    assert_eq!(simulate(&program, Plant::None), Ok(()));
    let mut db = TestDb::new();
    *db.flow.lock() = flow;
    let cell = Cell::new(&db, 0, 0);
    db.cells.lock().push(cell.0);
    fetch::<Flow>(&db, node(&db, 1));
    let since = db.storage().view();
    db.world.store(2, Ordering::Relaxed);
    db.storage_mut().synthetic_write(Durability::LOW);
    let Changed::Keys(keys) = changed_since::<Flow>(&db, since) else { panic!("unknown") };
    let mut both = [node(&db, 0), node(&db, 1)];
    both.sort_by_key(|it| it.0.as_u32());
    assert_eq!(keys, both);
    let work = db.storage().changed_set_work();
    assert_eq!((work.written, work.untracked, work.visited), (0, 1, 2), "{work:?}");
}

/// The cost of an edit is what it reaches: one input of a thousand written, the walk visits
/// that input's readers (a wide level: marked, not run) and no other row, and the answer is
/// theirs.
#[test]
fn the_work_of_an_edit_is_its_readers_among_a_thousand_rows() {
    const WIDE: usize = 100;
    const OTHERS: usize = 900;
    let mut flow: Vec<FlowNode> = (0..WIDE).map(|_| sum(&[Dep::Input(0)])).collect();
    flow.extend((0..OTHERS).map(|at| sum(&[Dep::Input(1 + at % 3)])));
    let mut db = TestDb::new();
    *db.flow.lock() = flow;
    let cells: Vec<Cell> = (0..4).map(|_| Cell::new(&db, 1, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let pool =
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(4), false).unwrap();
    for at in 0..WIDE + OTHERS {
        fetch::<Flow>(&db, node(&db, at));
    }
    let since = db.storage().view();
    cells[0].set_a(&mut db, 7);
    let changed = pool.install(|| changed_since::<Flow>(&db, since));
    let Changed::Keys(keys) = changed else { panic!("unknown: {changed:?}") };
    let mut expected: Vec<Node> = (0..WIDE).map(|at| node(&db, at)).collect();
    expected.sort_by_key(|it| it.0.as_u32());
    assert_eq!(keys, expected);
    let work = db.storage().changed_set_work();
    assert_eq!((work.written, work.visited, work.changed), (1, WIDE as u64, WIDE as u64), "{work:?}");
    assert!(work.edges <= WIDE as u64, "{work:?}");
    // Asked again in the same view: no walk, the same answer.
    assert_eq!(changed_since::<Flow>(&db, since), Changed::Keys(expected));
    assert_eq!(changed_since::<Flow>(&db, db.storage().view()), Changed::Keys(Vec::new()));
}

/// The invariant: a walk's level of fewer rows than its serial bound is verified on the
/// walker's thread, every row, with no job handed to the pool: an edit's levels cost no
/// dispatch. 50 readers of one input,
/// more than the small-loop predictor's 32 and fewer than the walk's 64: each runs again on
/// the thread that asked for the changed set. Asked from a thread of no pool, so a row
/// handed to the pool would run on a worker, never here.
#[test]
fn a_level_under_the_walks_serial_bound_is_verified_on_the_walkers_thread() {
    const WIDE: usize = 50;
    assert!(WIDE > crate::db::SMALL_ITEMS, "past the small-loop predictor's bound");
    let flow: Vec<FlowNode> = (0..WIDE).map(|_| sum(&[Dep::Input(0)])).collect();
    let mut db = TestDb::new();
    *db.flow.lock() = flow;
    let cells: Vec<Cell> = (0..2).map(|_| Cell::new(&db, 1, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    for at in 0..WIDE {
        fetch::<Flow>(&db, node(&db, at));
    }
    take_ran_on(&db);
    let since = db.storage().view();
    cells[0].set_a(&mut db, 7);
    let me = std::thread::current().id();
    let changed = changed_since::<Flow>(&db, since);
    let Changed::Keys(keys) = changed else { panic!("unknown: {changed:?}") };
    assert_eq!(keys.len(), WIDE);
    let on = take_ran_on(&db);
    assert_eq!(on.len(), WIDE, "each reader ran again once");
    assert!(on.iter().all(|thread| *thread == me), "a row of the level left the walker's thread");
}

/// The invariant: a walk that meets a wide level runs no row and waits for none. From that
/// level on it marks: each row it reaches is named as changed and left to whoever asks for
/// it, so a change the whole program reads costs the walk its reverse edges, where a level
/// handed to the pool a job a row had its jobs wait on each other for minutes (2026-11-11).
/// 100 readers of one input, each read by a row of its own: the walk runs none of the 200,
/// names all, and a row asked for afterwards is run then and answers as the model.
#[test]
fn a_wide_level_is_marked_and_no_row_of_it_runs_in_the_walk() {
    const WIDE: usize = 100;
    let mut flow: Vec<FlowNode> = (0..WIDE).map(|_| sum(&[Dep::Input(0)])).collect();
    flow.extend((0..WIDE).map(|at| sum(&[Dep::Query(at)])));
    let mut db = TestDb::new();
    *db.flow.lock() = flow;
    let cells: Vec<Cell> = (0..2).map(|_| Cell::new(&db, 1, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    for at in 0..2 * WIDE {
        assert_eq!(*fetch::<Flow>(&db, node(&db, at)), 1);
    }
    take_ran_on(&db);
    let since = db.storage().view();
    cells[0].set_a(&mut db, 7);
    let changed = changed_since::<Flow>(&db, since);
    assert!(matches!(changed, Changed::Keys(ref keys) if keys.len() == 2 * WIDE), "{changed:?}");
    assert_eq!(take_ran_on(&db).len(), 0, "the walk ran a row of a wide level");
    let work = db.storage().changed_set_work();
    assert_eq!((work.visited, work.marked, work.changed), (200, 200, 200), "{work:?}");
    // A marked row is never taken as clean: asked for, it is run and gives the new value,
    // and what it reads with it.
    assert_eq!(*fetch::<Flow>(&db, node(&db, WIDE + 3)), 7);
    assert_eq!(take_ran_on(&db).len(), 2, "the row asked for and the one it reads");
    for at in 0..2 * WIDE {
        assert_eq!(*fetch::<Flow>(&db, node(&db, at)), 7, "row {at}");
    }
    // The next walk, after another write: the rows brought up to date are verified again
    // or marked again, and every answer is the model's.
    cells[0].set_a(&mut db, 9);
    let again = changed_since::<Flow>(&db, since);
    assert!(matches!(again, Changed::Keys(ref keys) if keys.len() == 2 * WIDE), "{again:?}");
    for at in 0..2 * WIDE {
        assert_eq!(*fetch::<Flow>(&db, node(&db, at)), 9, "row {at}");
    }
}

/// The control of [`a_wide_level_is_marked_and_no_row_of_it_runs_in_the_walk`]: a level
/// under the bound is verified by the walk itself, so its rows do run in it, and the oracle
/// tells a walk that marks from one that verifies.
#[test]
fn the_oracle_catches_a_walk_that_runs_a_levels_rows() {
    const NARROW: usize = 20;
    let flow: Vec<FlowNode> = (0..NARROW).map(|_| sum(&[Dep::Input(0)])).collect();
    let mut db = TestDb::new();
    *db.flow.lock() = flow;
    let cells: Vec<Cell> = (0..2).map(|_| Cell::new(&db, 1, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    for at in 0..NARROW {
        fetch::<Flow>(&db, node(&db, at));
    }
    take_ran_on(&db);
    let since = db.storage().view();
    cells[0].set_a(&mut db, 7);
    _ = changed_since::<Flow>(&db, since);
    assert_eq!(take_ran_on(&db).len(), NARROW, "a narrow level's rows run in the walk");
    assert_eq!(db.storage().changed_set_work().marked, 0);
}

/// 40 rows that read one input, in a level under the walk's serial bound, whose runs all
/// panic once the input is written: what rows keyed by something an edit removed do (`fn
/// main` replaced by another function was 83 such rows in a hello-world). Returns the walk's
/// work and how many of the rows it named.
fn a_walk_over_rows_whose_runs_panic(plant: Plant) -> (crate::db::ChangedSetWork, usize) {
    const DEAD: usize = 40;
    let flow: Vec<FlowNode> = (0..DEAD).map(|_| sum(&[Dep::Input(0)])).collect();
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = flow;
    let cells: Vec<Cell> = (0..2).map(|_| Cell::new(&db, 1, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    for at in 0..DEAD {
        assert_eq!(*fetch::<Flow>(&db, node(&db, at)), 1);
    }
    let since = db.storage().view();
    // What each row is keyed by is gone: its run reads an input that is not there, and panics.
    for spec in db.flow.lock().iter_mut() {
        spec.deps = vec![Dep::Input(0), Dep::Input(99)];
    }
    cells[0].set_a(&mut db, 7);
    let changed = changed_since::<Flow>(&db, since);
    let Changed::Keys(keys) = changed else { panic!("unknown: {changed:?}") };
    (db.storage().changed_set_work(), keys.len())
}

/// The invariant: one walk contains the unwinding of at most 8 verifications (rule 18); the
/// rows it reaches after that are marked, not run, and every row is still named as changed.
/// Of 40 rows whose runs panic, 8 are run and 32 marked.
#[test]
fn a_walk_contains_at_most_eight_unwinds_and_marks_the_rest() {
    let (work, named) = a_walk_over_rows_whose_runs_panic(Plant::None);
    assert_eq!((work.unwound, work.marked, named), (8, 32, 40), "{work:?}");
}

/// The control of [`a_walk_contains_at_most_eight_unwinds_and_marks_the_rest`]: with the
/// bound taken out the walk runs every row, and each run panics.
#[test]
fn the_oracle_catches_a_walk_that_contains_every_unwind() {
    let (work, named) = a_walk_over_rows_whose_runs_panic(Plant::ContainsEveryUnwind);
    assert_eq!((work.unwound, work.marked, named), (40, 0, 40), "{work:?}");
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
    built_verified();
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
    let verified = built_verified();
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
