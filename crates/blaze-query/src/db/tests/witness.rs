//! The frames that are no row's ([`crate::db::witnessed`], [`crate::db::covered`]), the
//! changed keys a witness is checked against ([`crate::db::changed_keys_since`]), and the
//! scoped memo's [`crate::db::plumbing::Scoped::settled`] hook. Each test's control plants
//! the bug its oracle is for.

use std::cell::RefCell;

use super::{
    super::{
        View, changed_keys_since, covered,
        frontier::Plant,
        plumbing::evict_values,
        scoped::probe as scoped_probe,
        witness::probe as witness_probe,
        witnessed,
    },
    changed::{FlowNode, FlowOp, evaluate, flowed, sum},
    *,
};

/// The flow of [`super::changed`] again as a query of its own: no other test runs its rows.
struct Seen;

impl Spec for Seen {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "Seen";
    const EVICT: bool = true;

    fn cache() -> &'static Ingredient {
        ingredient!(function::<Seen>)
    }

    fn compute(db: &TestDb, key: Node) -> u64 {
        flowed::<Seen>(db, key)
    }
}

#[derive(Clone, Debug)]
enum WStep {
    /// Sets an input (to its own value too: a write that changes nothing).
    Write(usize, u64),
    /// Changes the world an untracked read sees.
    World(u64),
    /// Lets go of every value of [`Seen`].
    Evict,
    /// Ends an epoch.
    Epoch,
    /// Asks for these nodes' rows.
    Pull(Vec<usize>),
    /// Folds the reverse index and settles the changed set, as a host does after a build.
    Pack,
    /// Computes this node's value in a witness frame: its body run inline, as no row (its
    /// query reads are rows, its input reads fields).
    Witness(usize),
    /// Checks every witness so far against the changed keys since its view.
    Check,
}

#[derive(Clone, Debug)]
struct WProgram {
    inputs: usize,
    flow: Vec<FlowNode>,
    steps: Vec<WStep>,
}

fn wprogram(tc: &TestCase) -> WProgram {
    let int = |low: usize, high: usize| tc.draw(gs::integers::<usize>().min_value(low).max_value(high));
    let inputs = int(1, 4);
    let nodes = int(1, 8);
    let mut flow = Vec::with_capacity(nodes);
    for at in 0..nodes {
        let op = [FlowOp::Sum, FlowOp::Mod3, FlowOp::Max, FlowOp::Pick, FlowOp::Pick, FlowOp::World]
            [int(0, 5)];
        let deps = (0..int(1, 3))
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
            0..=2 => WStep::Write(int(0, inputs - 1), tc.draw(gs::integers::<u64>().max_value(6))),
            3 => WStep::World(tc.draw(gs::integers::<u64>().max_value(3))),
            4 => WStep::Evict,
            5 => WStep::Epoch,
            6 => WStep::Pull((0..int(1, 3)).map(|_| int(0, nodes - 1)).collect()),
            7 => WStep::Pack,
            8 | 9 => WStep::Witness(int(0, nodes - 1)),
            _ => WStep::Check,
        });
    }
    steps.push(WStep::Check);
    WProgram { inputs, flow, steps }
}

/// One witnessed run: the view it ran in, the node, what it gave, the keys it read below
/// `HIGH` and whether those are all it read.
struct Witnessed {
    view: View,
    at: usize,
    value: u64,
    reads: Vec<u64>,
    complete: bool,
}

/// Runs `program` with `plant` in the changed set; `Err` says what the oracle saw.
fn simulate(program: &WProgram, plant: Plant) -> Result<(), String> {
    let mut db = TestDb::new();
    db.storage_mut().plant_in_changed_set(plant);
    *db.flow.lock() = program.flow.clone();
    let mut inputs: Vec<u64> = (0..program.inputs as u64).collect();
    let mut world = 0u64;
    let cells: Vec<Cell> = inputs.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let mut witnessed_runs: Vec<Witnessed> = Vec::new();
    let mut changed = Vec::new();
    for (step_at, step) in program.steps.iter().enumerate() {
        match step {
            WStep::Write(input, value) => {
                inputs[*input] = *value;
                cells[*input].set_a(&mut db, *value);
            }
            WStep::World(value) => {
                world = *value;
                db.world.store(world, Ordering::Relaxed);
                db.storage_mut().synthetic_write(Durability::LOW);
            }
            WStep::Evict => {
                evict_values::<Seen>(&mut db);
            }
            WStep::Epoch => {
                db.storage_mut().end_epoch();
            }
            WStep::Pack => {
                db.storage_mut().pack_reverse();
                db.storage_mut().settle_changed_set();
            }
            WStep::Pull(pulled) => {
                let now = evaluate(&program.flow, &inputs, world);
                for &it in pulled {
                    let got = *fetch::<Seen>(&db, node(&db, it));
                    if got != now[it] {
                        return Err(format!("step {step_at}: row {it} is {got}, from scratch {}", now[it]));
                    }
                }
            }
            WStep::Witness(at) => {
                let at = *at;
                let view = db.storage().view();
                let mut reads = Vec::new();
                let key = node(&db, at);
                let (value, witness) = witnessed(&db, &mut reads, || flowed::<Seen>(&db, key));
                let now = evaluate(&program.flow, &inputs, world);
                if value != now[at] {
                    return Err(format!(
                        "step {step_at}: the witnessed run of {at} gave {value}, from scratch {}",
                        now[at]
                    ));
                }
                witnessed_runs.push(Witnessed { view, at, value, reads, complete: witness.complete });
            }
            WStep::Check => {
                let now = evaluate(&program.flow, &inputs, world);
                for run in &witnessed_runs {
                    if !run.complete || now[run.at] == run.value {
                        continue;
                    }
                    changed_keys_since(&db, run.view, &mut changed).map_err(|why| {
                        format!("step {step_at}: the changed keys since {:?} are unknown: {why:?}", run.view)
                    })?;
                    if !run.reads.iter().any(|key| changed.binary_search(key).is_ok()) {
                        return Err(format!(
                            "step {step_at}: the run of {} witnessed at {:?} gave {} and gives {} \
                             now, and none of its reads {:?} is named in the changed keys since \
                             then ({changed:?}): not named",
                            run.at, run.view, run.value, now[run.at], run.reads
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// A directed program passes as the engine is and fails under `plant` (with `set(true)` on
/// this thread first and `set(false)` after: a probe), saying `says`.
fn caught_with(program: &WProgram, plant: Plant, set: fn(bool), says: &str) {
    assert_eq!(simulate(program, Plant::None), Ok(()), "the engine as it is must pass");
    set(true);
    let found = simulate(program, plant);
    set(false);
    let why = found.expect_err("the oracle missed the planted bug");
    assert!(why.contains(says), "{why}");
}

/// The invariant: for any graph and edit sequence, a run witnessed at view V whose value now
/// differs has a key of its `reads` in `changed_keys_since(V)`, or `complete` was false.
/// Checked after random writes, world changes, evictions, epochs, pulls and folds against a
/// from-scratch evaluation. The controls: the changed keys forgetting the fields written
/// (`Plant::ForgetsWrittenFields`), and a witness that skips every read (`skip_at` 0), each
/// leave a changed run unnamed.
#[hegel::test]
fn a_witnessed_runs_changed_keys_name_every_run_whose_value_is_another(tc: TestCase) {
    let program = wprogram(&tc);
    if let Err(why) = simulate(&program, Plant::None) {
        panic!("{why}\n{program:#?}");
    }
}

/// The directed program of the controls: a witness of a node that reads an input field
/// itself, that field written after.
fn reads_a_field() -> WProgram {
    WProgram {
        inputs: 1,
        flow: vec![sum(&[Dep::Input(0)])],
        steps: vec![WStep::Witness(0), WStep::Write(0, 9), WStep::Check],
    }
}

#[test]
fn the_witness_oracle_catches_the_fields_written_left_out() {
    caught_with(&reads_a_field(), Plant::ForgetsWrittenFields, |_| {}, "not named");
}

#[test]
fn the_witness_oracle_catches_a_witness_that_skips_every_read() {
    caught_with(
        &reads_a_field(),
        Plant::None,
        |on| witness_probe::SKIPS_EVERY_READ.with(|it| it.set(on)),
        "not named",
    );
}

thread_local! {
    /// What the witness inside [`WitnessesB`] last read, and whether that was all.
    static B_READS: RefCell<(Vec<u64>, bool)> = const { RefCell::new((Vec::new(), false)) };
}

query!(WitnessesB, u64, |db, key| {
    let mut reads = Vec::new();
    let (b, witness) = witnessed(db, &mut reads, || key.b(db));
    B_READS.with(|it| *it.borrow_mut() = (reads, witness.complete));
    key.a(db) * 10 + b
});

/// Fetches [`WitnessesB`] of a fresh cell `(1, 2)`, writes its `b` to 5 and fetches it again:
/// both values, the runs, and what the first witness read.
fn witness_below_a_row() -> (u64, u64, usize, (Vec<u64>, bool), u64) {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 2);
    let first = *get::<WitnessesB>(&db, cell);
    let read = B_READS.with(|it| it.borrow().clone()); // clone: the test's copy of a thread's record
    let b_key = db.storage().key_of(field_node::<Cells>(db.storage(), cell.0, 1));
    cell.set_b(&mut db, 5);
    let second = *get::<WitnessesB>(&db, cell);
    (first, second, db.runs("WitnessesB"), read, b_key)
}

/// The invariant: a witnessed run gives the query running below it no read: what it read is
/// the caller's to answer for (its keys), so a write of it alone does not make that row run
/// again. The row is stale on purpose here: the witness's caller keeps the keys. The control:
/// a witness that pushes no frame lands its reads in the row's frame, and the row reruns.
#[test]
fn a_witnessed_run_gives_the_query_below_it_no_read() {
    let (first, second, runs, (reads, complete), b_key) = witness_below_a_row();
    assert_eq!(first, 12);
    assert_eq!(reads, vec![b_key], "the witness's one read is the field `b`");
    assert!(complete);
    assert_eq!(runs, 1, "the row read `a` alone, which did not change");
    assert_eq!(second, 12, "verified by its own reads, the witness's not among them");

    witness_probe::PUSHES_NONE.with(|it| it.set(true));
    let (first, second, runs, _, _) = witness_below_a_row();
    witness_probe::PUSHES_NONE.with(|it| it.set(false));
    assert_eq!(first, 12);
    assert_eq!((runs, second), (2, 15), "the control: the reads landed in the row's frame");
}

query!(DoubleA, u64, |db, key| key.a(db) * 2);

/// The invariant: a covered run is held to `Durability::HIGH` reads: a read below it (an
/// input field, a persistent row) panics naming the key read and what the cover is for, and
/// the stack is whole after. The control: the same reads of a `HIGH` cell pass.
#[test]
fn a_covered_run_refuses_a_read_below_high_by_name() {
    let db = TestDb::new();
    let high = Cell::of(&db, 3, 4, Durability::HIGH);
    let low = Cell::of(&db, 3, 4, Durability::LOW);
    // The control: at `HIGH` both pass.
    assert_eq!(covered(&db, "a test's cover", || high.a(&db)), 3);
    assert_eq!(covered(&db, "a test's cover", || *get::<DoubleA>(&db, high)), 6);

    let field = catch_unwind(AssertUnwindSafe(|| covered(&db, "a test's cover", || low.a(&db))))
        .expect_err("a LOW field read under a cover");
    let text = message(&*field);
    assert!(text.contains("a test's cover") && text.contains("`Cell.a`"), "{text}");

    let row = catch_unwind(AssertUnwindSafe(|| {
        covered(&db, "a test's cover", || *get::<DoubleA>(&db, low))
    }))
    .expect_err("a LOW row read under a cover");
    let text = message(&*row);
    assert!(text.contains("a test's cover") && text.contains("`DoubleA`"), "{text}");

    // The stack is whole: a cover again, and a row with reads of its own.
    assert_eq!(covered(&db, "again", || high.b(&db)), 4);
    assert_eq!(*get::<DoubleA>(&db, low), 6);
}

query!(QuadA, u64, |db, key| *get::<DoubleA>(db, key) * 2);

/// The invariant: the cause a witness's key is given ([`super::super::Witness::cause_text`])
/// follows the row's reads below `Durability::HIGH` down to the input field read: a row over a
/// row over `Cell.a` of a `LOW` cell names all three, outermost first. The control: a witness
/// of a `HIGH` cell's rows gives no key, so nothing is named.
#[test]
fn a_cause_names_the_chain_of_reads_below_high_down_to_the_field() {
    let db = TestDb::new();
    let high = Cell::of(&db, 3, 4, Durability::HIGH);
    let low = Cell::of(&db, 3, 4, Durability::LOW);
    let mut reads = Vec::new(); // alloc: a test's keys
    let (value, witness) = witnessed(&db, &mut reads, || *get::<QuadA>(&db, high));
    assert_eq!((value, witness.complete, reads.len()), (12, true, 0), "the control: `HIGH` reads give no key");

    let (value, witness) = witnessed(&db, &mut reads, || *get::<QuadA>(&db, low));
    assert_eq!((value, witness.complete, reads.len()), (12, true, 1), "the witness's one read is `QuadA`'s row");
    let cause = super::super::Witness::cause_text(&db, reads[0]);
    let quad = cause.find("`QuadA`").expect(&cause);
    let double = cause.find("`DoubleA`").expect(&cause);
    let field = cause.find("`Cell.a`").expect(&cause);
    assert!(quad < double && double < field, "outermost first: {cause}");
    assert_eq!(cause.matches(" < ").count(), 2, "three reads, joined: {cause}");
}

thread_local! {
    /// What [`ClimbPair::settled`] was told, in order.
    static SETTLED: RefCell<Vec<(bool, u64)>> = const { RefCell::new(Vec::new()) };
}

/// A task-scoped fixpoint of two keys a cell: each one more than the other, up to the cell's
/// `a`.
struct ClimbPair;

impl Scoped for ClimbPair {
    type Db = TestDb;
    type Key = (Cell, bool);
    type Value = u64;
    const NAME: &'static str = "ClimbPair";
    const CYCLE: CycleKind = CycleKind::Fixpoint;
    const TAG: Tag = Tag::Query;

    fn execute(db: &TestDb, key: &(Cell, bool)) -> u64 {
        let other = fetch_scoped::<ClimbPair, _>(db, (key.0, !key.1), |it| *it);
        (other + 1).min(key.0.a(db))
    }

    fn cycle_initial(_: &TestDb, _: &(Cell, bool)) -> u64 {
        0
    }

    fn equal(old: &u64, new: &u64) -> bool {
        old == new
    }

    fn write_key(key: &(Cell, bool), out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{:?}, {}", key.0, key.1)
    }

    fn settled(_: &TestDb, key: &(Cell, bool), value: &u64) {
        SETTLED.with(|it| it.borrow_mut().push((key.1, *value)));
    }
}

/// Both keys of a fresh cell with `a` 4, asked for in one scope: their values and what
/// `settled` was told.
fn climb_pair() -> ((u64, u64), Vec<(bool, u64)>) {
    SETTLED.with(|it| it.borrow_mut().clear());
    let db = TestDb::new();
    let cell = Cell::new(&db, 4, 0);
    let values = task_scope(&db, || {
        let a = fetch_scoped::<ClimbPair, _>(&db, (cell, false), |it| *it);
        let b = fetch_scoped::<ClimbPair, _>(&db, (cell, true), |it| *it);
        (a, b)
    });
    (values, SETTLED.with(|it| std::mem::take(&mut *it.borrow_mut())))
}

/// The invariant: `settled` is told each value that becomes final in a scope, once, and never
/// a round's value of a cycle: the head when its fixpoint holds, the member when it is asked
/// again after (its round's value gone with the round). The control: told the `Provisional`
/// entries too, it hears a round's value.
#[test]
fn settled_is_told_each_final_value_once_and_no_round_value() {
    let (values, told) = climb_pair();
    assert_eq!(values, (4, 4));
    assert_eq!(told, vec![(false, 4), (true, 4)]);

    scoped_probe::SETTLES_PROVISIONAL.with(|it| it.set(true));
    let (values, told) = climb_pair();
    scoped_probe::SETTLES_PROVISIONAL.with(|it| it.set(false));
    assert_eq!(values, (4, 4));
    assert!(told.contains(&(true, 1)), "the control: a round's value was told: {told:?}");
}
