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
        let at = *interned_fields::<Indices>(db.storage(), key.0) as usize;
        let spec = db.flow.lock()[at].clone();
        let read = |dep: Dep| match dep {
            Dep::Input(input) => {
                let cell = Cell(db.cells.lock()[input]);
                cell.a(db)
            }
            Dep::Query(query) => *fetch::<Flow>(db, node(db, query)),
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
    /// Packs the reverse index, shedding its durable readers (a build's end).
    Pack,
    /// Asks for these nodes' values without asking for the changed set: a pull in a view no
    /// walk may ever run in.
    Pull(Vec<usize>),
    /// Asks for the changed set since the ask `back` asks ago (0: the last; past the first:
    /// since the storage's first view), then for every node made.
    Ask(usize),
}

#[derive(Clone, Debug)]
struct Program {
    inputs: usize,
    /// The inputs of kind `MEDIUM` or `HIGH` (a library's), by index and whether `HIGH`; the
    /// rest are `LOW`. A row that reads only these is durable, and a pack sheds it from the
    /// reverse index.
    durable: Vec<(usize, bool)>,
    flow: Vec<FlowNode>,
    steps: Vec<Step>,
}

fn program(tc: &TestCase) -> Program {
    let int = |low: usize, high: usize| tc.draw(gs::integers::<usize>().min_value(low).max_value(high));
    let inputs = int(1, 5);
    let durable =
        (0..inputs).filter(|_| tc.draw(gs::booleans())).map(|at| (at, tc.draw(gs::booleans()))).collect();
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
        steps.push(match int(0, 9) {
            0..=3 => Step::Write(int(0, inputs - 1), tc.draw(gs::integers::<u64>().max_value(6))),
            4 => Step::World(tc.draw(gs::integers::<u64>().max_value(3))),
            5 => Step::Evict,
            6 => [Step::Epoch, Step::Pack][int(0, 1)].clone(),
            7 | 8 => Step::Pull((0..int(1, 3)).map(|_| int(0, nodes - 1)).collect()),
            _ => Step::Ask(int(0, 3)),
        });
    }
    steps.push(Step::Ask(0));
    Program { inputs, durable, flow, steps }
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
    let kind = |input: usize| match program.durable.iter().find(|it| it.0 == input) {
        Some((_, true)) => Durability::HIGH,
        Some((_, false)) => Durability::MEDIUM,
        None => Durability::LOW,
    };
    let cells: Vec<Cell> =
        inputs.iter().enumerate().map(|(at, &value)| Cell::of(&db, value, 0, kind(at))).collect();
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
                // As many packs in a row as a shed ever waits for (`frontier::quiet_due`'s
                // most), so each such step sheds where a walk allows it, whatever came before.
                for _ in 0..=64 {
                    db.storage_mut().pack_reverse();
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
    let program = Program { inputs: 4, durable: Vec::new(), flow, steps };
    caught(&program, Plant::DropsAnEdge, "does not name it");
}

/// A row that read input 1, runs again and reads input 2 instead: the plant keeps only its
/// first edges, so a write of input 2 never reaches it.
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
    let program = Program { inputs: 3, durable: Vec::new(), flow: vec![pick], steps };
    caught(&program, Plant::KeepsOldEdges, "does not name it");
}

/// A changed reader's own reader: the plant stops the walk at the first.
#[test]
fn the_oracle_catches_a_changed_reader_that_does_not_pass_the_walk_on() {
    let flow = vec![sum(&[Dep::Input(0)]), sum(&[Dep::Query(0)])];
    let steps = vec![Step::Pull(vec![1]), Step::Ask(0), Step::Write(0, 5), Step::Ask(0)];
    let program = Program { inputs: 1, durable: Vec::new(), flow, steps };
    caught(&program, Plant::StopsAtChangedReader, "does not name it");
}

/// A row that runs again to an equal value is backdated and is no change: the plant names it.
#[test]
fn the_oracle_catches_a_cutoff_not_applied() {
    let flow = vec![FlowNode { op: FlowOp::Mod3, deps: vec![Dep::Input(0)] }];
    let steps = vec![Step::Pull(vec![0]), Step::Ask(0), Step::Write(0, 3), Step::Ask(0)];
    let program = Program { inputs: 1, durable: Vec::new(), flow, steps };
    caught(&program, Plant::NamesEqualRows, "never moved");
}

/// A value let go between two views is still a row whose value can change: the plant passes
/// over it.
#[test]
fn the_oracle_catches_an_evicted_row_passed_over() {
    let flow = vec![sum(&[Dep::Input(0)])];
    let steps =
        vec![Step::Pull(vec![0]), Step::Ask(0), Step::Evict, Step::Write(0, 5), Step::Ask(0)];
    let program = Program { inputs: 1, durable: Vec::new(), flow, steps };
    caught(&program, Plant::SkipsEvictedRows, "does not name it");
}

/// A durable row shed from the reverse index and then a write of the durable input it reads:
/// the plant leaves the index shed, so the walk from the written field finds no reader. The
/// second row reads a `LOW` input as well, is never shed, and shows the pack alone loses
/// nothing: the program passes as the engine is.
#[test]
fn the_oracle_catches_a_durable_write_over_an_index_left_shed() {
    let flow = vec![sum(&[Dep::Input(0)]), sum(&[Dep::Query(0), Dep::Input(1)])];
    let steps = vec![
        Step::Pull(vec![0, 1]),
        // A view boundary, so the runs' edges are in the index the pack sheds, and a walk
        // after it, so the pack does shed.
        Step::Write(1, 3),
        Step::Ask(0),
        // The first pack sees the inputs' making; the next two, nothing durable written
        // since, and the second of them sheds.
        Step::Pack,
        Step::Pack,
        Step::Pack,
        Step::Write(1, 7),
        Step::Ask(0),
        Step::Write(0, 5),
        Step::Ask(0),
    ];
    for high in [false, true] {
        let program =
            Program { inputs: 2, durable: vec![(0, high)], flow: flow.clone(), steps: steps.clone() };
        caught(&program, Plant::KeepsShedAcrossDurableWrite, "does not name it");
    }
}

/// What a pack sheds and what a durable write brings back, counted: two durable rows and one
/// below `HIGH` over the same durable row; the pack keeps the one reader below `HIGH`, and
/// the fold of a write of the durable input holds all three again.
#[test]
fn a_pack_sheds_the_durable_readers_and_a_durable_write_makes_the_index_again() {
    let mut db = TestDb::new();
    let flow = vec![
        sum(&[Dep::Input(0)]),
        sum(&[Dep::Query(0)]),
        sum(&[Dep::Query(0), Dep::Input(1)]),
    ];
    *db.flow.lock() = flow;
    let cells = [Cell::of(&db, 1, 0, Durability::MEDIUM), Cell::new(&db, 2, 0)];
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    for at in 0..3 {
        fetch::<Flow>(&db, node(&db, at));
    }
    // A view boundary folds what the runs stored: input 0 -> row 0, row 0 -> rows 1 and 2,
    // input 1 -> row 2.
    cells[1].set_a(&mut db, 3);
    assert_eq!(db.storage().reverse_readers(), 4);
    db.storage_mut().pack_reverse();
    assert_eq!(db.storage().reverse_readers(), 4, "no walk since the inputs were made: nothing is shed");
    let since = db.storage().view();
    assert!(matches!(changed_since::<Flow>(&db, since), Changed::Keys(_)));
    db.storage_mut().pack_reverse();
    assert_eq!(db.storage().reverse_readers(), 4, "one quiet pack: a shed waits for two");
    db.storage_mut().pack_reverse();
    assert_eq!(db.storage().reverse_readers(), 2, "row 2's two edges: it reads a `LOW` input");
    cells[1].set_a(&mut db, 4);
    assert_eq!(db.storage().reverse_readers(), 2, "a `LOW` write leaves the index shed");
    cells[0].set_a(&mut db, 5);
    assert_eq!(db.storage().reverse_readers(), 4, "a `MEDIUM` write makes it again from every row");
    assert_eq!(*fetch::<Flow>(&db, node(&db, 2)), 9);
    // One shed was ended by a durable write, so the next waits for four quiet packs: the
    // first pack after the write is not quiet, then four are.
    let since = db.storage().view();
    assert!(matches!(changed_since::<Flow>(&db, since), Changed::Keys(_)));
    for (pack, held) in [4, 4, 4, 4, 2].into_iter().enumerate() {
        db.storage_mut().pack_reverse();
        assert_eq!(db.storage().reverse_readers(), held, "pack {pack} after the durable write");
    }
}

/// A row that made an untracked read is asked by every walk: the world changes with nothing
/// written, and the row and its reader are named.
#[test]
fn an_untracked_read_puts_its_row_in_every_walk() {
    let flow = vec![FlowNode { op: FlowOp::World, deps: vec![Dep::Input(0)] }, sum(&[Dep::Query(0)])];
    let steps = vec![Step::Pull(vec![1]), Step::Ask(0), Step::World(2), Step::Ask(0)];
    let program = Program { inputs: 1, durable: Vec::new(), flow: flow.clone(), steps };
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
