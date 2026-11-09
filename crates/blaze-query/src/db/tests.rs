//! The dense engine's tests, on hand-written ingredients (the macros are another crate's): hits
//! and edges, backdating, durability and summaries, every cycle form, inputs, interning,
//! tracked structs, captured reads, LRU, cancellation, allocation budgets, provenance, and
//! properties over random query graphs: checked against a from-scratch evaluation, and every
//! rerun's cause chain ending at the edit.

use std::{
    collections::HashMap,
    hash::Hash,
    ops::ControlFlow,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_format::FormatError;
use hegel::{TestCase, generators as gs};
use blaze_core::lock::Lock;

use super::{
    AsId, Cancelled, Cause, Counts, Cycle, Database, DatabaseKeyIndex, Durability, Event, FromIdIn,
    Hook, Id, Outcome, Provenance, ReadKind, Reads, Setter, Storage, attach, capture_reads,
    input::field_node,
    owner_is,
    plumbing::{
        CycleKind, Function, Ingredient, Input, Interned, Tag, Tracked, Vtable, evict_values,
        evict_values_of, fetch, input_fields, input_new, input_new_singleton, input_read,
        input_set, input_singleton, intern, intern_by, interned_arguments, interned_fields,
        tracked_fields, tracked_new, value_hash,
    },
    replay_reads, report_untracked_read,
    row::{Header, PROVISIONAL, VIEW_MASK},
    storage::{Summary, input_bits, node_key},
    unshareable_reads, unwind_if_cancelled, with_attached, with_attached_db,
};

/// The database of every test: the storage, a log of runs, and what particular tests share
/// with their queries.
pub struct TestDb {
    storage: Storage,
    /// `(query, key, view)` of every run.
    log: Lock<Vec<(&'static str, u32, u32)>>,
    shared: Lock<HashMap<u32, (u64, Reads)>>,
    gate: Barrier,
    armed: [AtomicBool; 2],
    drops: Arc<AtomicUsize>,
    probe: AtomicU64,
    cells: Lock<Vec<Id>>,
    graph: Vec<GNode>,
    /// The dependency values of each graph node's last run.
    snapshots: Lock<HashMap<usize, Vec<u64>>>,
}

impl TestDb {
    fn new() -> TestDb {
        TestDb::made(None, Vec::new())
    }

    fn made(hook: Option<Hook>, graph: Vec<GNode>) -> TestDb {
        TestDb {
            storage: Storage::new(hook),
            log: Lock::new("query.test.db.log", Vec::new()),
            shared: Lock::new("query.test.db.shared", HashMap::new()),
            gate: Barrier::new(2),
            armed: [AtomicBool::new(false), AtomicBool::new(false)],
            drops: Arc::new(AtomicUsize::new(0)),
            probe: AtomicU64::new(u64::MAX),
            cells: Lock::new("query.test.db.cells", Vec::new()),
            graph,
            snapshots: Lock::new("query.test.db.snapshots", HashMap::new()),
        }
    }

    fn ran(&self, query: &'static str, key: Id) {
        let view = self.storage.view_bits();
        self.log.lock().push((query, key.as_u32(), view));
    }

    fn runs(&self, query: &str) -> usize {
        self.log.lock().iter().filter(|it| it.0 == query).count()
    }

    fn runs_of(&self, query: &str, key: Id) -> usize {
        self.log.lock().iter().filter(|it| it.0 == query && it.1 == key.as_u32()).count()
    }
}

impl Database for TestDb {
    fn storage(&self) -> &Storage {
        &self.storage
    }

    fn storage_mut(&mut self) -> &mut Storage {
        &mut self.storage
    }
}

/// An ingredient's static vtable and number.
macro_rules! ingredient {
    ($kind:ident :: <$ty:ty>) => {{
        static VTABLE: Vtable = Vtable::$kind::<$ty>();
        static INGREDIENT: Ingredient = Ingredient::new(&VTABLE);
        &INGREDIENT
    }};
}

/// A test query, without the plumbing [`Function`] asks of every function.
pub trait Spec: 'static {
    type Key: AsId + FromIdIn + Copy + Send + Sync + 'static;
    type Value: PartialEq + Hash + Send + Sync + 'static;
    const QUERY: &'static str;
    /// `Some(bytes)`: kept by its bytes (`lru`), each value weighing `bytes`.
    const BYTES: Option<usize> = None;
    const EVICT: bool = false;
    const UNEQUAL: bool = false;
    const CYCLES: CycleKind = CycleKind::Panic;
    fn cache() -> &'static Ingredient;
    fn compute(db: &TestDb, key: Self::Key) -> Self::Value;
    fn fallback(_: &TestDb, _: Self::Key) -> Self::Value {
        panic!("{} has no cycle_result", Self::QUERY)
    }
    fn initial(_: &TestDb, _: Self::Key) -> Self::Value {
        panic!("{} has no cycle_initial", Self::QUERY)
    }
    fn recover(
        _: &TestDb,
        _: &Cycle<'_>,
        _: &Self::Value,
        new: Self::Value,
        _: Self::Key,
    ) -> Self::Value {
        new
    }
    fn hash_value(value: &Self::Value) -> u64 {
        value_hash(value)
    }
}

impl<S: Spec> Function for S {
    type Db = TestDb;
    type Key = S::Key;
    type Value = S::Value;
    const NAME: &'static str = S::QUERY;
    const FILE: &'static str = file!();
    const BUDGETED: bool = S::BYTES.is_some();
    const EVICTABLE: bool = S::EVICT;
    const NO_EQ: bool = S::UNEQUAL;
    const CYCLE: CycleKind = S::CYCLES;
    const TAG: Tag = Tag::Query;

    fn ingredient() -> &'static Ingredient {
        S::cache()
    }

    fn execute(db: &TestDb, key: S::Key) -> S::Value {
        db.ran(S::QUERY, key.as_id());
        S::compute(db, key)
    }

    fn cycle_result(db: &TestDb, _: Id, key: S::Key) -> S::Value {
        S::fallback(db, key)
    }

    fn cycle_initial(db: &TestDb, _: Id, key: S::Key) -> S::Value {
        S::initial(db, key)
    }

    fn cycle_recover(
        db: &TestDb,
        cycle: &Cycle<'_>,
        last: &S::Value,
        new: S::Value,
        key: S::Key,
    ) -> S::Value {
        S::recover(db, cycle, last, new, key)
    }

    fn equal(old: &S::Value, new: &S::Value) -> bool {
        !S::UNEQUAL && old == new
    }

    fn hash(value: &S::Value) -> u64 {
        S::hash_value(value)
    }

    fn weigh(_: &S::Value) -> usize {
        S::BYTES.expect("weighed only with `lru`")
    }
}

/// An input of two `u64` fields.
struct Cells;

impl Input for Cells {
    type Fields = (u64, u64);
    const NAME: &'static str = "Cell";
    const FILE: &'static str = file!();
    const FIELDS: u32 = 2;
    const FIELD_NAMES: &'static [&'static str] = &["a", "b"];
    const SINGLETON: bool = false;

    fn ingredient() -> &'static Ingredient {
        ingredient!(input::<Cells>)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Cell(Id);

impl AsId for Cell {
    fn as_id(&self) -> Id {
        self.0
    }
}

impl FromIdIn for Cell {
    fn from_id_in(storage: &Storage, id: Id) -> Option<Cell> {
        owner_is(storage, id, Cells::ingredient()).then_some(Cell(id))
    }
}

fn field_a(fields: &mut (u64, u64)) -> &mut u64 {
    &mut fields.0
}

fn field_b(fields: &mut (u64, u64)) -> &mut u64 {
    &mut fields.1
}

impl Cell {
    fn new(db: &TestDb, a: u64, b: u64) -> Cell {
        Cell::of(db, a, b, Durability::LOW)
    }

    fn of(db: &TestDb, a: u64, b: u64, durability: Durability) -> Cell {
        Cell(input_new::<Cells, 2>(db.storage(), (a, b), [durability; 2]))
    }

    fn a(self, db: &TestDb) -> u64 {
        input_read::<Cells>(db.storage(), self.0, 0);
        input_fields::<Cells>(db.storage(), self.0).0
    }

    fn b(self, db: &TestDb) -> u64 {
        input_read::<Cells>(db.storage(), self.0, 1);
        input_fields::<Cells>(db.storage(), self.0).1
    }

    fn set_a(self, db: &mut TestDb, value: u64) -> u64 {
        input_set::<Cells, u64>(db.storage_mut(), self.0, 0, field_a).to(value)
    }

    fn set_b(self, db: &mut TestDb, value: u64) -> u64 {
        input_set::<Cells, u64>(db.storage_mut(), self.0, 1, field_b).to(value)
    }
}

/// A function of one input; after a `;`, the [`Spec`] items it overrides.
macro_rules! query {
    ($name:ident, $value:ty, |$db:ident, $key:ident| $body:expr $(; $($extra:tt)*)?) => {
        struct $name;

        impl Spec for $name {
            type Key = Cell;
            type Value = $value;
            const QUERY: &'static str = stringify!($name);
            $($($extra)*)?

            fn cache() -> &'static Ingredient {
                ingredient!(function::<$name>)
            }

            fn compute($db: &TestDb, $key: Cell) -> $value {
                $body
            }
        }
    };
}

fn get<F: Function<Db = TestDb, Key = Cell>>(db: &TestDb, cell: Cell) -> &F::Value {
    fetch::<F>(db, cell)
}

query!(Double, u64, |db, key| key.a(db) * 2);
query!(Parity, u64, |db, key| key.a(db) % 2);
query!(Label, u64, |db, key| get::<Parity>(db, key) * 10 + 1);
query!(ReadB, u64, |db, key| key.b(db));
query!(Untracked, u64, |db, key| {
    report_untracked_read(db, crate::db::Untracked { what: "the test's own read, which no tracked read stands for" });
    key.b(db)
});

query!(SeesItsDatabase, u64, |db, _key| {
    u64::from(with_attached_db(|it: Option<&TestDb>| it.is_some_and(|it| std::ptr::eq(it, db))))
});

/// A query's run has its own database attached, whoever asked and wherever it runs, so the
/// text it makes reads the same database on every thread; the asker's thread has none again
/// once it returns.
#[test]
fn a_query_runs_with_its_database_attached() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert!(!with_attached(|it| it.is_some()));
    assert_eq!(*get::<SeesItsDatabase>(&db, cell), 1);
    assert!(!with_attached(|it| it.is_some()));
}

#[test]
fn a_hit_returns_the_memo_without_running() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Double>(&db, cell), 6);
    assert_eq!(*get::<Double>(&db, cell), 6);
    assert_eq!(db.runs("Double"), 1);
}

#[test]
fn a_change_reruns_its_readers_and_an_equal_value_backdates() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Label>(&db, cell), 11);
    cell.set_a(&mut db, 5);
    assert_eq!(*get::<Label>(&db, cell), 11);
    assert_eq!(
        (db.runs("Parity"), db.runs("Label")),
        (2, 1),
        "parity reran equal: label is backdated"
    );
    cell.set_a(&mut db, 4);
    assert_eq!(*get::<Label>(&db, cell), 1);
    assert_eq!((db.runs("Parity"), db.runs("Label")), (3, 2));
}

/// Each query its flight holder runs counts in [`super::completed`], what the daemon's
/// watchdog reads to tell query work that ends from work that goes around: two for a first
/// read of `Label` (it and `Parity`), at least one more once a write reruns `Parity`. Other
/// tests count too, so only at least.
#[test]
fn each_query_run_counts_as_completed() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    let before = super::completed();
    assert_eq!(*get::<Label>(&db, cell), 11);
    let ran = super::completed();
    assert!(ran >= before + 2, "`Label` and `Parity` ran: {before}, then {ran}");
    cell.set_a(&mut db, 5);
    assert_eq!(*get::<Label>(&db, cell), 11);
    let again = super::completed();
    assert!(again > ran, "`Parity` reran: {ran}, then {again}");
}

#[test]
fn each_field_has_its_own_edge() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 2);
    assert_eq!(*get::<ReadB>(&db, cell), 2);
    cell.set_a(&mut db, 9);
    assert_eq!(*get::<ReadB>(&db, cell), 2);
    assert_eq!(db.runs("ReadB"), 1);
    assert_eq!(cell.set_b(&mut db, 7), 2, "a setter gives back the old value");
    assert_eq!(*get::<ReadB>(&db, cell), 7);
    assert_eq!(db.runs("ReadB"), 2);
}

query!(Alternating, u64, |db, key| {
    let mut sum = 0u64;
    for _ in 0..(1u64 << 23) + 1 {
        sum = sum.wrapping_add(key.a(db)).wrapping_add(key.b(db));
    }
    sum
});

/// A run that reads the same nodes in turn, more times than a row's edge span holds
/// (2^24), keeps each edge once: it is stored, and a write of either field reruns it.
#[test]
fn a_run_reading_the_same_nodes_in_turn_keeps_each_edge_once() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 2);
    let reads = (1u64 << 23) + 1;
    assert_eq!(*get::<Alternating>(&db, cell), 3 * reads);
    cell.set_a(&mut db, 2);
    assert_eq!(*get::<Alternating>(&db, cell), 4 * reads);
    cell.set_b(&mut db, 3);
    assert_eq!(*get::<Alternating>(&db, cell), 5 * reads);
    assert_eq!(db.runs("Alternating"), 3);
}

#[test]
fn a_write_of_a_lower_durability_skips_higher_rows() {
    let mut db = TestDb::new();
    let library = Cell::of(&db, 4, 0, Durability::HIGH);
    let file = Cell::new(&db, 1, 0);
    assert_eq!(*get::<Double>(&db, library), 8);
    file.set_a(&mut db, 2);
    assert_eq!(*get::<Double>(&db, library), 8);
    assert_eq!(db.runs("Double"), 1);
    let old = input_set::<Cells, u64>(db.storage_mut(), library.0, 0, field_a)
        .with_durability(Durability::LOW)
        .to(5);
    assert_eq!(old, 4);
    assert_eq!(*get::<Double>(&db, library), 10);
    file.set_a(&mut db, 3);
    assert_eq!(*get::<Double>(&db, library), 10, "now LOW, verified by its edges: unchanged");
    assert_eq!(db.runs("Double"), 2);
}

#[test]
fn an_unrelated_write_of_the_same_durability_passes_the_summary() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..8).map(|i| Cell::new(&db, i, 0)).collect();
    let other = Cell::new(&db, 0, 0);
    for &cell in &cells {
        get::<Label>(&db, cell);
    }
    for round in 0..70 {
        other.set_a(&mut db, round + 1);
        for &cell in &cells {
            assert_eq!(*get::<Label>(&db, cell), (cell.a(&db) % 2) * 10 + 1);
        }
    }
    assert_eq!(db.runs("Label"), 8, "70 views, more than the history, and nothing reran");
    assert_eq!(db.runs("Parity"), 8);
    cells[3].set_a(&mut db, 4);
    assert_eq!(*get::<Label>(&db, cells[3]), 1);
}

fn bits(db: &TestDb, cell: Cell, field: u32) -> Summary {
    input_bits(field_node::<Cells>(db.storage(), cell.0, field))
}

/// A new cell neither of whose fields shares a summary bit with `cell`'s field `a`.
fn apart(db: &TestDb, cell: Cell) -> Cell {
    let mine = bits(db, cell, 0);
    loop {
        let other = Cell::new(db, 0, 0);
        let shared = |field| {
            let theirs = bits(db, other, field);
            (theirs[0] & mine[0]) | (theirs[1] & mine[1]) != 0
        };
        if !shared(0) && !shared(1) {
            return other;
        }
    }
}

fn recorded() -> (Arc<Lock<Vec<String>>>, Hook) {
    let events = Arc::new(Lock::new("query.test.db.events", Vec::new()));
    let seen = Arc::clone(&events);
    let hook: Hook = Box::new(move |_: &Storage, event: Event| {
        seen.lock().push(format!("{:?}", event.kind));
    });
    (events, hook)
}

#[test]
fn a_memo_green_by_its_summary_walks_nothing() {
    let (events, hook) = recorded();
    let mut db = TestDb::made(Some(hook), Vec::new());
    let cell = Cell::new(&db, 3, 0);
    let other = apart(&db, cell);
    assert_eq!(*get::<Label>(&db, cell), 11);
    events.lock().clear();
    other.set_a(&mut db, 1);
    assert_eq!(*get::<Label>(&db, cell), 11);
    let events = events.lock();
    assert!(
        events.iter().any(|it| it.starts_with("DidValidateMemoizedValue") && it.contains("Label("))
    );
    assert!(
        !events.iter().any(|it| it.contains("Parity(")),
        "its dependency was not visited: {events:?}"
    );
    assert_eq!((db.runs("Label"), db.runs("Parity")), (1, 1));
}

query!(Grows, u64, |db, key| { if key.a(db) == 0 { 0 } else { Cell(db.cells.lock()[0]).b(db) } });
query!(GrowsReader, u64, |db, key| get::<Grows>(db, key) + 1);

#[test]
fn a_walk_refreshes_the_summary_with_a_dependencys_new_reads() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    let late = apart(&db, cell);
    db.cells.lock().push(late.0);
    assert_eq!(*get::<GrowsReader>(&db, cell), 1);
    cell.set_a(&mut db, 1);
    assert_eq!(*get::<GrowsReader>(&db, cell), 1, "Grows reran, read `late.b` (0): backdated");
    assert_eq!((db.runs("Grows"), db.runs("GrowsReader")), (2, 1));
    late.set_b(&mut db, 5);
    assert_eq!(*get::<GrowsReader>(&db, cell), 6, "the reader's summary has `late.b` now");
    assert_eq!((db.runs("Grows"), db.runs("GrowsReader")), (3, 2));
}

query!(Link, u64, |db, key| {
    let at = db.cells.lock().iter().position(|it| *it == key.0).expect("a cell of the chain");
    let below = (at > 0).then(|| db.cells.lock()[at - 1]);
    below.map_or(0, |it| *get::<Link>(db, Cell(it))) + key.a(db)
});

/// A walk is bounded (rule 18): past its fuel it unwinds with [`super::OutOfFuel`] naming the
/// query, how far it got and how deep, the rows it left are run again by a later call, and no
/// bound outlives the call.
#[test]
fn a_walk_past_its_fuel_stops_naming_where_and_a_later_call_with_enough_ends() {
    let mut db = TestDb::new();
    let chain: Vec<Cell> = (0..64).map(|at| Cell::new(&db, at, 0)).collect();
    db.cells.lock().extend(chain.iter().map(|it| it.0));
    let top = chain[63];
    assert_eq!(
        super::with_fuel(1 << 10, || *get::<Link>(&db, top)).map(|it| it.0),
        Ok((0..64).sum())
    );
    chain[0].set_a(&mut db, 100);
    let out = super::with_fuel(10, || *get::<Link>(&db, top))
        .expect_err("64 rows to run again on 10 steps");
    assert_eq!((out.steps, out.query), (10, "Link"), "{out}");
    assert!((1..=64).contains(&out.depth), "{out}");
    assert!(out.to_string().contains("`Link`"), "{out}");
    let (value, spent) = super::with_fuel(1 << 10, || *get::<Link>(&db, top)).expect("enough fuel");
    assert_eq!(value, 100 + (1..64).sum::<u64>());
    assert!((1..=1 << 10).contains(&spent), "it says what it spent: {spent}");
    assert!(super::local::with(|local| local.fuel.is_none()), "the bound ended with its call");
    chain[1].set_a(&mut db, 0);
    assert_eq!(*get::<Link>(&db, top), 100 + (2..64).sum::<u64>(), "unbounded outside `with_fuel`");
}

/// A job pumped onto a fueled task's thread while it waits (above an unlifted barrier) is
/// another task: a bound it sets is its own, not what the waiting task has left, and what it
/// spends is not charged to the waiting task. Without that, the pumped job's bound would be the
/// waiter's 5 steps, it would run out on a 64-row walk, and the waiter's fuel would shrink by
/// work that is not its own, each as thread timing has it.
#[test]
fn a_bound_set_by_a_job_pumped_under_a_fueled_wait_is_its_own_and_charges_the_waiter_nothing() {
    let mut db = TestDb::new();
    let chain: Vec<Cell> = (0..64).map(|at| Cell::new(&db, at, 0)).collect();
    db.cells.lock().extend(chain.iter().map(|it| it.0));
    let top = chain[63];
    assert_eq!(*get::<Link>(&db, top), (0..64).sum::<u64>());
    chain[0].set_a(&mut db, 100);
    let left = || super::local::with(|local| local.fuel.map(|it| it.left));
    let ((pumped, waiter_left), waiter_spent) = super::with_fuel(5, || {
        let pumped = super::pump_barrier(|| super::with_fuel(1 << 10, || *get::<Link>(&db, top)));
        (pumped, left())
    })
    .expect("the waiter spent nothing of its own");
    let (value, spent) = pumped.expect("the pumped job's own bound, not the waiter's 5 steps");
    assert_eq!(value, 100 + (1..64).sum::<u64>());
    assert!(spent > 5, "the pumped job spent past what the waiter had: {spent}");
    assert_eq!(waiter_left, Some(5), "nothing the pumped job spent was charged to the waiter");
    assert_eq!(waiter_spent, 0);
}

/// Levels of [`Rung`]'s ladder below its head, two rungs each.
const LEVELS: usize = 16;

query!(
    Rung,
    u64,
    |db, key| {
        // The cells are the ladder, then the gate. The head is rung 0, level `l` (1..=LEVELS)
        // rungs `2l - 1` and `2l`; each reads both rungs of the level below it, and the last
        // level reads the head: one cycle, each rung reached along many paths.
        let cells = db.cells.lock().clone();
        let (gate, ladder) = cells.split_last().expect("a ladder and its gate");
        let at = ladder.iter().position(|it| *it == key.0).expect("a rung of the ladder");
        let level = at.div_ceil(2);
        let below = if level < LEVELS { &ladder[2 * level + 1..2 * level + 3] } else { &ladder[..1] };
        let most = below.iter().map(|it| *get::<Rung>(db, Cell(*it))).max().unwrap_or(0);
        most.max(key.a(db)) + get::<Gate>(db, Cell(*gate))
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(Gate, u64, |db, key| key.b(db) / 1000);

/// A verify that reaches a row it already proved on an assumption (pending until the cycle's
/// head is proved) takes that answer again: each row of a cycle is checked once in a view, not
/// once per path to it. Without it the ladder's warm verify checks its 34 rows about 2^17
/// times, which is how a proof bounded by the rows it brought up to date ran out of fuel on a
/// warm daemon only (cranelift-codegen's build script, selfbuild5).
#[test]
fn a_verify_reaching_a_pending_row_again_takes_its_answer_and_checks_each_row_once() {
    let mut db = TestDb::new();
    let ladder: Vec<Cell> =
        (0..1 + 2 * LEVELS as u64).map(|at| Cell::new(&db, at % 3, 0)).collect();
    let gate = Cell::new(&db, 0, 0);
    db.cells.lock().extend(ladder.iter().map(|it| it.0).chain([gate.0]));
    let head = ladder[0];
    assert_eq!(*get::<Rung>(&db, head), 2);
    let runs = db.runs("Rung");
    // Every rung reads the gate, whose new `b` changes nothing: a deep verify of the cycle.
    gate.set_b(&mut db, 1);
    let (value, spent) =
        super::with_fuel(1 << 30, || *get::<Rung>(&db, head)).expect("a bound far past it");
    assert_eq!(value, 2);
    assert_eq!(db.runs("Rung"), runs, "every rung was verified and none ran again");
    let rows = ladder.len() as u64 + 1;
    assert!(
        spent <= 2 * rows,
        "each row brought up to date about once, not once a path: {spent} for {rows} rows"
    );
}

/// How many times [`Endless`] reads its key's two fields: past any bound a test sets, and an
/// end for its run when no bound stops it, so a test without the bound fails, not hangs.
const ENDLESS: u64 = 1 << 21;

query!(Endless, u64, |db, key| {
    let mut sum = 0;
    for _ in 0..ENDLESS {
        sum += key.a(db) + key.b(db);
    }
    sum
});
query!(EndlessReader, u64, |db, key| get::<Endless>(db, key) + 1);

/// Runs `f` with this thread's bound on one run's reads lowered to `bound`, put back after.
fn with_run_reads<R>(bound: u64, f: impl FnOnce() -> R) -> R {
    let old = super::local::with(|local| std::mem::replace(&mut local.bounds.run_reads, bound));
    let out = f();
    super::local::with(|local| local.bounds.run_reads = old);
    out
}

/// The [`super::Runaway`] `f` unwinds with.
fn runaway_of(f: impl FnOnce()) -> super::Runaway {
    let payload = catch_unwind(AssertUnwindSafe(f)).expect_err("a run past its bound unwinds");
    match payload.downcast::<super::Runaway>() {
        Ok(out) => *out,
        Err(other) => std::panic::resume_unwind(other),
    }
}

/// A run that keeps reading (rule 18) fails once it recorded more reads than a run may, with a
/// [`super::Runaway`] naming the query, its key, where it is on its task's stack and what it
/// holds; the stack is left empty, the database answers the next ask, the same ask fails the
/// same way again (its row was emptied, not poisoned), and with room it ends. Its stack holds
/// each of its two reads once (repeats are dropped as the stack fills), so the bound counts the
/// reads recorded, not held: without it the loop would spin on a small stack until it ended,
/// and before repeats were dropped such a stack grew a `u32` per read (in run m2 of lane
/// checkheap it doubled to 97 GiB and the daemon was killed at 229 GiB, naming nothing).
#[test]
fn a_run_that_keeps_reading_fails_naming_itself_and_what_it_reads_and_the_next_ask_is_served() {
    let mut db = TestDb::new();
    let looping = Cell::new(&db, 1, 2);
    let other = Cell::new(&db, 3, 0);
    let bound = 1 << 12;
    let out = with_run_reads(bound, || runaway_of(|| _ = get::<EndlessReader>(&db, looping)));
    let named = (out.over, out.query, out.key, out.bound);
    assert_eq!(named, (super::Over::Reads, "Endless", looping.0, bound), "{out}");
    assert!(out.held >= bound, "{out}");
    assert_eq!((out.depth, out.frames), (2, 2), "under `EndlessReader`, on top: {out}");
    let mut names: Vec<&str> = out.most.iter().map(|it| it.0.as_str()).collect();
    names.sort_unstable();
    let (a, b) = (format!("`Cell.a`({:?})", looping.0), format!("`Cell.b`({:?})", looping.0));
    assert_eq!(names, [a.as_str(), b.as_str()], "the loop's body, its two reads: {out}");
    assert!(out.most.iter().all(|it| it.1 == 1), "each read held once, its repeats dropped: {out}");
    let text = out.to_string();
    let parts = ["`Endless` of", "query 2 of the 2", a.as_str(), b.as_str()];
    assert!(parts.iter().all(|it| text.contains(it)), "{text}");
    let payload: Box<dyn std::any::Any + Send> = Box::new(out.clone());
    assert_eq!(super::unwind_text(&*payload), Some(text), "a host names the job's failure by it");
    let empty = super::local::with(|local| local.is_empty());
    assert!(empty, "the failed runs left nothing on the stack");
    assert_eq!(*get::<Double>(&db, other), 6, "the next ask is served");
    let again = with_run_reads(bound, || runaway_of(|| _ = get::<EndlessReader>(&db, looping)));
    assert_eq!((again.query, again.key, &again.most), (out.query, out.key, &out.most));
    assert_eq!(*get::<EndlessReader>(&db, looping), ENDLESS * 3 + 1, "with room the run ends");
    other.set_a(&mut db, 4);
    assert_eq!(*get::<Double>(&db, other), 8, "and the next view is served");
}

/// Rows [`Rereader`] reads, each [`PASSES`] times: more than any other test's run holds, so
/// the high-water mark is its.
const WIDE: usize = 1 << 17;
const PASSES: u64 = 8;

query!(Rereader, u64, |db, _key| {
    let cells = db.cells.lock().clone();
    for _ in 0..PASSES {
        for cell in &cells {
            let _ = Cell(*cell).a(db);
        }
    }
    // What the run holds on its thread's edge stack as it ends.
    super::local::with(|local| {
        let frame = local.frames.last().expect("its run");
        (local.edges.len() - frame.edges_start as usize) as u64
    })
});

/// How many times [`FewRows`] reads its key's two fields.
const FEW_PASSES: u64 = 1 << 16;

query!(FewRows, u64, |db, key| {
    let mut sum = 0;
    for _ in 0..FEW_PASSES {
        sum += key.a(db) + key.b(db);
    }
    sum
});

/// A run that reads the same rows again and again holds each about once on its thread's edge
/// stack (its repeats are dropped as its own count doubles), not once a read, on a cold thread
/// and on a warm one whose stack an earlier run grew: one run of coord-120's check held 2^27
/// reads, 512 MiB of stack. Its query's counts say what it recorded, what its
/// row kept and the most one run held, so the profile names a query that re-reads; and the
/// memory report names the run that held the most.
#[test]
fn a_run_re_reading_its_rows_holds_each_about_once_and_its_counts_and_the_high_water_name_it() {
    let db = TestDb::new();
    let cells: Vec<Cell> = (0..WIDE as u64).map(|at| Cell::new(&db, at, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let wide = WIDE as u64;
    counts_of("Rereader");
    counts_of("FewRows");
    let held = *get::<Rereader>(&db, cells[0]);
    assert!(
        held <= 2 * wide,
        "{wide} rows read {PASSES} times held as {held} reads, not about {wide}"
    );
    let counts = counts_of("Rereader");
    assert_eq!(counts.executed, 1, "{counts:?}");
    assert_eq!((counts.reads_recorded, counts.reads_kept), (PASSES * wide, wide), "{counts:?}");
    assert_eq!(
        counts.reads_most,
        2 * wide,
        "at most twice its distinct reads at once, and it gets there: {counts:?}"
    );
    let most = super::stack_high_water();
    assert!(most.run_reads >= wide, "{most:?}");
    let named = format!("`Rereader` of {:?}", cells[0]);
    assert!(
        most.run.starts_with("`Rereader` of"),
        "the high-water mark names its run, {named}: {most:?}"
    );

    // Two rows read 2^16 times each, on this thread whose stack `Rereader` grew past all
    // 2^17 of those reads: recorded far past kept, and still held on a small stack. A dedup
    // fired by the stack's capacity never runs here (batch23-1 held all 131072).
    let warm = super::local::with(|local| local.edges.capacity()) as u64;
    assert!(warm >= 2 * FEW_PASSES, "the thread's stack is warm, {warm} entries");
    assert_eq!(*get::<FewRows>(&db, cells[1]), FEW_PASSES, "cell 1 is (1, 0)");
    let few = counts_of("FewRows");
    assert_eq!(
        (few.executed, few.reads_recorded, few.reads_kept),
        (1, 2 * FEW_PASSES, 2),
        "{few:?}"
    );
    // A run holds at most `max(4096, 2 x kept)` at once (`local::next_dedup`): 4096 here, and
    // it does reach it.
    assert_eq!(
        few.reads_most,
        1 << 12,
        "two rows held at most 4096 at once on a warm thread: {few:?}"
    );
}

/// Rows of [`Nest`] past its two heads and `y`: the chain `x`.
const CHAIN: usize = 32;

query!(
    Nest,
    u64,
    |db, key| {
        // The cells are the nest, then the gate: `h1` (0) reads `h2` and `y`; `h2` (1) reads
        // `x0` and `h1`; `y` (2) reads `x0`; `x_i` (3..) reads `x_{i+1}`, the last `h2`. So a
        // warm verify of `h1` proves the chain on `h2`'s assumption, then `h2` on `h1`'s, then
        // reaches the chain again through `y` after `h2`'s frame is gone.
        let cells = db.cells.lock().clone();
        let (gate, nest) = cells.split_last().expect("a nest and its gate");
        let at = nest.iter().position(|it| *it == key.0).expect("a row of the nest");
        let reads = match at {
            0 => [1, 2].as_slice(),
            1 => &[3, 0],
            2 => &[3],
            at if at + 1 < nest.len() => &[at + 1][..],
            _ => &[1],
        };
        let most = reads.iter().map(|it| *get::<Nest>(db, Cell(nest[*it]))).max().unwrap_or(0);
        most.max(key.a(db)) + get::<Gate>(db, Cell(*gate))
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

/// A verify proved on an inner cycle head's assumption, when that head is then proved on an
/// outer head's, stays answered from the pending list on the outer head's: a later path to it
/// takes that answer instead of verifying it, and everything below it, again. Without it the
/// chain's 32 rows are verified a second time through `y` (each nesting level of inner heads
/// is one more pass: the re-verify that filled run m2's pending list), and the pending list
/// holds each of them twice.
#[test]
fn an_inner_head_s_pending_rows_stay_answered_once_it_is_proved_on_an_outer_head() {
    let mut db = TestDb::new();
    let nest: Vec<Cell> = (0..3 + CHAIN as u64).map(|at| Cell::new(&db, at % 3, 0)).collect();
    let gate = Cell::new(&db, 0, 0);
    db.cells.lock().extend(nest.iter().map(|it| it.0).chain([gate.0]));
    let head = nest[0];
    assert_eq!(*get::<Nest>(&db, head), 2);
    let runs = db.runs("Nest");
    // Every row reads the gate, whose new `b` changes nothing: a deep verify of the nest.
    gate.set_b(&mut db, 1);
    let fueled = super::with_fuel(1 << 30, || *get::<Nest>(&db, head));
    let (value, spent) = fueled.expect("a bound far past it");
    assert_eq!(value, 2);
    assert_eq!(db.runs("Nest"), runs, "every row was verified and none ran again");
    let rows = nest.len() as u64 + 1;
    let most = rows + CHAIN as u64 / 2;
    let why = "each row brought up to date once, the chain not again";
    assert!(spent < most, "{why}: {spent} for {rows} rows");
}

query!(Stepper, u64, |db, key| {
    let at = db.cells.lock().iter().position(|it| *it == key.0).expect("a cell of the chain");
    super::step(|| at as u128, || format!("cell {at}"));
    // The same step again, as a walk that reaches a thing twice gives it twice.
    super::step(|| at as u128, || format!("cell {at}"));
    let below = (at > 0).then(|| db.cells.lock()[at - 1]);
    below.map_or(0, |it| *get::<Stepper>(db, Cell(it))) + key.a(db)
});

/// A walk bounded by its own steps (rule 18) counts each distinct step once; past its bound it
/// unwinds with [`super::OutOfSteps`] naming the step, the rows it left are run again by a later
/// call, no bound outlives the call, and a step given outside any bound costs nothing.
#[test]
fn a_walks_steps_are_its_distinct_keys_and_past_its_bound_it_stops_naming_the_step() {
    let db = TestDb::new();
    let chain: Vec<Cell> = (0..64).map(|at| Cell::new(&db, at, 0)).collect();
    db.cells.lock().extend(chain.iter().map(|it| it.0));
    let top = chain[63];
    let out = super::with_steps(10, || *get::<Stepper>(&db, top)).expect_err("64 steps on 10");
    assert_eq!((out.steps, out.at.as_str()), (10, "cell 53"), "{out}");
    assert!(super::local::with(|local| local.steps.is_none()), "the bound ended with its call");
    let (value, taken) =
        super::with_steps(64, || *get::<Stepper>(&db, top)).expect("64 steps, each taken once");
    assert_eq!((value, taken), ((0..64).sum(), 64));
    super::step(
        || unreachable!("outside a bound a step's key is never made"),
        || unreachable!("outside a bound a step is never named"),
    );
}

#[test]
fn an_untracked_read_reruns_in_every_view() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 0, 5);
    let other = Cell::new(&db, 0, 0);
    assert_eq!(*get::<Untracked>(&db, cell), 5);
    other.set_a(&mut db, 1);
    assert_eq!(*get::<Untracked>(&db, cell), 5);
    assert_eq!(db.runs("Untracked"), 2);
}

query!(
    OneOOne,
    u64,
    |db, key| get::<OneOOne>(db, key) + 1;
    const CYCLES: CycleKind = CycleKind::Result;
    fn fallback(_: &TestDb, _: Cell) -> u64 {
        100
    }
);

#[test]
fn cycle_fallback_immediate() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    assert_eq!(*get::<OneOOne>(&db, cell), 100);
}

query!(
    CycA,
    u64,
    |db, key| get::<CycB>(db, key) + 1;
    const CYCLES: CycleKind = CycleKind::Result;
    fn fallback(_: &TestDb, _: Cell) -> u64 {
        10
    }
);
query!(
    CycB,
    u64,
    |db, key| get::<CycA>(db, key) + 1;
    const CYCLES: CycleKind = CycleKind::Result;
    fn fallback(_: &TestDb, _: Cell) -> u64 {
        20
    }
);

#[test]
fn every_query_of_a_fallback_cycle_gets_its_fallback_whichever_is_asked() {
    for a_first in [true, false] {
        let db = TestDb::new();
        let cell = Cell::new(&db, 0, 0);
        let (a, b) = if a_first {
            (*get::<CycA>(&db, cell), *get::<CycB>(&db, cell))
        } else {
            let b = *get::<CycB>(&db, cell);
            (*get::<CycA>(&db, cell), b)
        };
        assert_eq!((a, b), (10, 20));
    }
}

query!(
    Count,
    u64,
    |db, key| (get::<Count>(db, key) + 1).min(key.a(db));
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

#[test]
fn a_fixpoint_converges_and_reconverges_after_a_write() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 5, 0);
    assert_eq!(*get::<Count>(&db, cell), 5);
    assert_eq!(db.runs("Count"), 6, "rounds 0, 1, .., 5, then 5 again");
    cell.set_a(&mut db, 3);
    assert_eq!(*get::<Count>(&db, cell), 3);
    cell.set_b(&mut db, 1);
    let before = db.runs("Count");
    assert_eq!(*get::<Count>(&db, cell), 3);
    assert_eq!(db.runs("Count"), before, "an unread field leaves the cycle's rows verified");
}

query!(
    FixA,
    u64,
    |db, key| *get::<FixB>(db, key);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(FixB, u64, |db, key| (get::<FixA>(db, key) + 1).min(key.a(db)));

#[test]
fn a_fixpoint_of_two_queries_converges_from_its_head() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<FixA>(&db, cell), 3);
    assert_eq!(*get::<FixB>(&db, cell), 3);
}

query!(
    RingA,
    u64,
    |db, key| (get::<RingB>(db, key) + 1).min(key.a(db));
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(
    RingB,
    u64,
    |db, key| get::<RingA>(db, key) + get::<RingGate>(db, key);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(RingGate, u64, |db, key| key.b(db) / 1000);

/// A converged cycle is verified as any row is: its head by what the whole cycle read outside
/// itself, a member by its head. A write its reads see but that changes none of their values
/// (the gate turns a new `b` into the same 0) leaves both rows proved without a run, view after
/// view, whichever is asked first. Before, the claim-free walk refused every cycle-made row,
/// and `summary` (hir-ty-suspend) ran 13M times in one check with none verified.
#[test]
fn a_converged_cycle_is_verified_whole_when_nothing_it_read_changed() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 4, 0);
    assert_eq!((*get::<RingA>(&db, cell), *get::<RingB>(&db, cell)), (4, 4));
    let runs = (db.runs("RingA"), db.runs("RingB"));
    counts_of("RingA");
    counts_of("RingB");
    for b in 1..=2 {
        cell.set_b(&mut db, b);
        if b == 1 {
            assert_eq!(*get::<RingB>(&db, cell), 4, "a member asked first, proved by its head");
            assert_eq!(*get::<RingA>(&db, cell), 4);
        } else {
            assert_eq!(*get::<RingA>(&db, cell), 4, "the head asked first");
            assert_eq!(*get::<RingB>(&db, cell), 4);
        }
    }
    assert!(settled(), "the cycle left nothing on this thread");
    assert_eq!((db.runs("RingA"), db.runs("RingB")), runs, "no row of the cycle ran again");
    let (a, b) = (counts_of("RingA"), counts_of("RingB"));
    assert_eq!((a.executed, a.verified), (0, 2), "the head proved once a view");
    assert_eq!((b.executed, b.verified), (0, 2), "the member proved once a view");
}

// `RingA` and `RingB` again under their own names: counts are process-wide, by query name, so
// each test reads only its own.
query!(
    LoopA,
    u64,
    |db, key| (get::<LoopB>(db, key) + 1).min(key.a(db));
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(
    LoopB,
    u64,
    |db, key| get::<LoopA>(db, key) + get::<LoopGate>(db, key);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(LoopGate, u64, |db, key| key.b(db) / 1000);

/// A cycle whose reads changed runs again from its head, members included, to the value a run
/// from scratch gives: here a new gate read only by the member changes the member and not the
/// head, and the member asked first runs the cycle again (heading it this time).
#[test]
fn a_converged_cycle_runs_again_when_a_members_read_changed() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 4, 0);
    assert_eq!((*get::<LoopA>(&db, cell), *get::<LoopB>(&db, cell)), (4, 4));
    let runs = (db.runs("LoopA"), db.runs("LoopB"));
    cell.set_b(&mut db, 5000);
    assert_eq!(*get::<LoopB>(&db, cell), 9, "the member's new gate: 4 + 5");
    assert_eq!(*get::<LoopA>(&db, cell), 4);
    assert!(settled(), "the rerun cycle left nothing on this thread");
    assert!(db.runs("LoopA") > runs.0 && db.runs("LoopB") > runs.1, "both rows ran again");
    let ran = (db.runs("LoopA"), db.runs("LoopB"));
    cell.set_b(&mut db, 5001);
    assert_eq!((*get::<LoopA>(&db, cell), *get::<LoopB>(&db, cell)), (4, 9));
    assert_eq!((db.runs("LoopA"), db.runs("LoopB")), ran, "an equal gate leaves it verified again");
    assert!(settled());
}

query!(
    EarlyA,
    u64,
    |db, key| *get::<EarlyB>(db, key);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(EarlyB, u64, |db, key| {
    // `b` is read only while the head is still its initial 0: in round 1, never in the last.
    let a = *get::<EarlyA>(db, key);
    if a == 0 { key.b(db) + 1 } else { a }
});

/// A cycle's edges hold what every round read, not only the last: `b` is read in round 1
/// alone, yet the converged value came from it, so a new `b` runs the cycle again to the value a
/// run from scratch gives (keys cover every input). Kept to the last round's reads, the head
/// would be proved unchanged and keep the old `b + 1`.
#[test]
fn a_read_only_an_early_round_made_runs_the_cycle_again() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 0, 2);
    assert_eq!(*get::<EarlyA>(&db, cell), 3);
    assert!(settled(), "the cycle's gathered reads went to its head and nowhere else");
    cell.set_b(&mut db, 5);
    assert_eq!(*get::<EarlyA>(&db, cell), 6, "as from scratch");
    assert_eq!(*get::<EarlyB>(&db, cell), 6);
    let ran = db.runs("EarlyA");
    cell.set_a(&mut db, 1);
    assert_eq!(*get::<EarlyA>(&db, cell), 6);
    assert_eq!(db.runs("EarlyA"), ran, "`a` was never read: the cycle stays verified");
    assert!(settled());
}

/// Nothing is left on this thread's query stacks: no frame, no edge, no gathered cycle read.
fn settled() -> bool {
    super::local::with(|local| local.is_empty())
}

/// How many times [`OnceSelf`] ran: it reads itself in its first run only.
static ONCE_SELF: AtomicUsize = AtomicUsize::new(0);

query!(
    OnceSelf,
    u64,
    |db, key| {
        if ONCE_SELF.fetch_add(1, Ordering::Relaxed) == 0 {
            *get::<OnceSelf>(db, key) + key.b(db)
        } else {
            key.b(db)
        }
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

/// A head whose later round reads nothing of its cycle ends as any run does, through `finish`,
/// with round 1's reads gathered for it: those go with the frame, not onto the next top-level
/// query (before, they stayed on the thread and joined an unrelated cycle's edges).
#[test]
fn a_head_that_ends_without_its_cycle_leaves_no_gathered_reads() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 0, 2);
    assert_eq!(*get::<OnceSelf>(&db, cell), 2);
    assert!(settled(), "round 1's gathered reads left on the thread");
    cell.set_b(&mut db, 7);
    assert_eq!(*get::<OnceSelf>(&db, cell), 7, "it read `b`, so a new `b` runs it again");
    assert!(settled());
}

query!(
    NestOuter,
    u64,
    |db, key| *get::<NestInner>(db, key);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(
    NestInner,
    u64,
    |db, key| {
        // Its own cycle first: `b` is read only in that inner cycle's round 1, before it reads
        // the outer head and joins its cycle.
        let own = *get::<NestInner>(db, key);
        if own == 0 { key.b(db) + 1 } else { own.max(*get::<NestOuter>(db, key)) }
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

/// A read made only in an inner head's early round, before that head joined an outer cycle,
/// is in the outer head's edges: a new `b` runs the whole cycle again to the value a run from
/// scratch gives, and a write to `a`, which nothing read, leaves it verified.
#[test]
fn a_read_of_an_inner_heads_early_round_reaches_the_outer_head() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 0, 2);
    assert_eq!(*get::<NestOuter>(&db, cell), 3);
    assert_eq!(*get::<NestInner>(&db, cell), 3);
    assert!(settled());
    cell.set_b(&mut db, 5);
    assert_eq!(*get::<NestOuter>(&db, cell), 6, "as from scratch");
    assert_eq!(*get::<NestInner>(&db, cell), 6);
    assert!(settled());
    let ran = (db.runs("NestOuter"), db.runs("NestInner"));
    cell.set_a(&mut db, 1);
    assert_eq!(*get::<NestOuter>(&db, cell), 6);
    assert_eq!((db.runs("NestOuter"), db.runs("NestInner")), ran, "`a` was never read");
    assert!(settled());
}

/// The row of `F`'s key `id` and its node, once it ran.
fn function_row<F: Function>(storage: &Storage, id: Id) -> Option<(&Header, u32)> {
    let key = node_key(F::ingredient().index(), id, 0);
    let node = (1..storage.nodes_made()).find(|&node| storage.key_of(node) == key)?;
    Some((super::verify::function_header(storage, storage.node(node))?, node))
}

/// What [`ProbeHead`] saw of its member's row mid-cycle after view 1: its word, and whether the
/// claim-free walk proved it.
static PROBED: Lock<Vec<(u32, bool)>> = Lock::new("query.test.db.probed", Vec::new());

query!(
    ProbeHead,
    u64,
    |db, key| {
        let member = *get::<ProbeMember>(db, key);
        let storage = db.storage();
        if storage.view_bits() > 1 {
            let (header, node) =
                function_row::<ProbeMember>(storage, key.0).expect("the member's row, run in view 1");
            let word = header.verified.load(Ordering::Acquire);
            let proved = super::verify::unclaimed(storage, header, word, node);
            PROBED.lock().push((word, proved));
        }
        (member + 1).min(key.a(db))
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(ProbeMember, u64, |db, key| *get::<ProbeHead>(db, key));

/// A provisional row (a round not yet converged) is never verified, even one whose word names
/// the earlier view it was final in: in round 1 of the cycle's rerun the member holds its old
/// view's bits with `PROVISIONAL`, and the claim-free walk refuses it.
#[test]
fn a_provisional_row_is_never_verified() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<ProbeHead>(&db, cell), 3);
    cell.set_a(&mut db, 2);
    let view = db.storage().view_bits();
    assert_eq!(*get::<ProbeHead>(&db, cell), 2);
    let probed = std::mem::take(&mut *PROBED.lock());
    assert!(!probed.is_empty(), "the head ran its cycle again after the write");
    for &(word, proved) in &probed {
        assert_ne!(word & PROVISIONAL, 0, "the member is mid-cycle: {word:#x}");
        assert!(!proved, "a provisional row was verified: {word:#x}");
    }
    let earlier = |&(word, _): &(u32, bool)| (1..view).contains(&(word & VIEW_MASK));
    assert!(probed.iter().any(earlier), "round 1 saw the member's earlier view: {probed:x?}");
    assert_eq!(*get::<ProbeMember>(&db, cell), 2);
    assert!(settled());
    // Once final, the head holds what the cycle read outside itself, and no row of the cycle;
    // the member's one edge is its head.
    let storage = db.storage();
    let (head, head_node) = function_row::<ProbeHead>(storage, cell.0).expect("the head's row");
    let (member, member_node) =
        function_row::<ProbeMember>(storage, cell.0).expect("the member's row");
    let edges_of = |header: &Header| {
        let span = header.edges.load(Ordering::Acquire);
        let start = super::row::span::start(span);
        (start..start + super::row::span::len(span)).map(|at| storage.edge(at)).collect::<Vec<_>>()
    };
    let read = edges_of(head);
    assert!(!read.is_empty(), "the head read `a`");
    assert!(
        !read.contains(&head_node) && !read.contains(&member_node),
        "a provisional read became an edge: {read:?}"
    );
    assert_eq!(edges_of(member), [head_node], "a member's one edge is its head");
    for header in [head, member] {
        assert_eq!(header.verified.load(Ordering::Acquire) & PROVISIONAL, 0, "final");
    }
}

query!(
    Diverge,
    u64,
    |db, key| get::<Diverge>(db, key) + 1;
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

fn message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
        .unwrap_or_default()
}

/// Rounds of [`Outer`] run so far; [`Settles`] reads its own and [`Outer`]'s provisional
/// values in round 1 only (an inner head), and nothing from round 2 on.
static OUTER_ROUNDS: AtomicU64 = AtomicU64::new(0);

query!(
    Outer,
    u64,
    |db, key| {
        let round = OUTER_ROUNDS.fetch_add(1, Ordering::SeqCst) + 1;
        let settled = *get::<Settles>(db, key);
        assert!(round != 2, "the outer head fails in round 2");
        settled + 1
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(
    Settles,
    u64,
    |db, key| {
        if OUTER_ROUNDS.load(Ordering::SeqCst) >= 2 {
            7
        } else {
            (*get::<Settles>(db, key)).max(*get::<Outer>(db, key))
        }
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

#[test]
fn a_head_a_later_round_made_final_outlives_its_cycle_unwinding() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    let payload = catch_unwind(AssertUnwindSafe(|| *get::<Outer>(&db, cell)))
        .expect_err("the outer head fails in its second round");
    assert!(message(&*payload).contains("round 2"), "{}", message(&*payload));
    assert_eq!(db.runs("Settles"), 2, "an inner head of round 1, then final in round 2");
    assert_eq!(*get::<Settles>(&db, cell), 7);
    assert_eq!(db.runs("Settles"), 2, "its published value was kept, not emptied by the unwind");
}

#[test]
fn a_key_the_database_never_gave_is_refused_naming_the_query_and_the_database_still_drops() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Double>(&db, cell), 6);
    for forged in [Id::from_u32(5), Id::from_u32(Id::MAX_U32)] {
        let payload = catch_unwind(AssertUnwindSafe(|| *get::<Double>(&db, Cell(forged))))
            .expect_err("a key of no owned page");
        let message = message(&*payload);
        assert!(message.contains("`Double`") && message.contains("never gave"), "{message}");
    }
    assert_eq!(*get::<Double>(&db, cell), 6);
    drop(db);
}

/// No `Id` lies past the last ID page: `fetch`'s hit path indexes the page directory by an
/// ID's page with no bounds load, so a key above [`Id::MAX_U32`] (here `u32::MAX`, page
/// 2^22 - 1 of a directory of 2^20) read past it and crashed (SIGSEGV, or a misaligned
/// pointer abort) instead of being refused. It is refused where it is made.
#[test]
fn an_id_past_the_last_page_is_refused_where_it_is_made_naming_its_bits() {
    for bits in [Id::MAX_U32 + 1, u32::MAX] {
        let payload = catch_unwind(|| Id::from_u32(bits)).expect_err("an `Id` past every page");
        let message = message(&*payload);
        assert!(message.contains(&bits.to_string()) && message.contains("MAX_U32"), "{message}");
    }
    let payload = catch_unwind(|| Id::from_u32(0)).expect_err("an `Id` of 0");
    assert!(message(&*payload).contains("of 0"), "{}", message(&*payload));
    assert_eq!(Id::from_u32(Id::MAX_U32).as_u32(), Id::MAX_U32, "the last ID of the last page");
}

/// What a reader of persisted or foreign bits goes through: `Id::new` refuses what
/// `Id::from_u32` panics on, and `Id::decode` makes it the record's typed error naming the word.
#[test]
fn a_word_that_is_no_id_is_refused_by_the_checked_constructors_naming_it() {
    for bits in [0, Id::MAX_U32 + 1, u32::MAX] {
        assert_eq!(Id::new(bits), None, "{bits}");
    }
    assert_eq!(Id::new(Id::MAX_U32).map(Id::as_u32), Some(Id::MAX_U32));
    let max = u64::from(Id::MAX_U32);
    for word in [max + 1, u64::from(u32::MAX), 1 << 32, u64::MAX] {
        assert_eq!(
            Id::decode(word, 40, "a test's id"),
            Err(FormatError::Bound { at: 40, what: "a test's id", found: word, bound: max }),
        );
    }
    let zero = Id::decode(0, 40, "a test's id");
    assert_eq!(zero, Err(FormatError::Zero { at: 40, what: "a test's id" }));
    assert!(zero.unwrap_err().to_string().contains("a test's id"), "names the field");
    assert_eq!(Id::decode(7, 40, "a test's id").map(Id::as_u32), Ok(7));
}

#[test]
fn a_fixpoint_that_never_converges_panics_naming_its_head_and_leaves_no_value() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    for _ in 0..2 {
        let payload =
            catch_unwind(AssertUnwindSafe(|| *get::<Diverge>(&db, cell))).expect_err("no fixpoint");
        let message = message(&*payload);
        assert!(message.contains("Diverge") && message.contains("did not converge"), "{message}");
    }
}

query!(
    PanicA,
    u64,
    |db, key| *get::<PanicB>(db, key);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(PanicB, u64, |db, key| {
    let a = *get::<PanicA>(db, key);
    assert!(key.b(db) == 0, "PanicB was told to panic");
    (a + 1).min(key.a(db))
});

#[test]
fn a_panic_in_a_cycle_leaves_its_rows_empty() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 4, 1);
    for _ in 0..2 {
        let payload = catch_unwind(AssertUnwindSafe(|| *get::<PanicA>(&db, cell)))
            .expect_err("PanicB panics");
        assert!(message(&*payload).contains("told to panic"));
    }
    cell.set_b(&mut db, 0);
    assert_eq!(*get::<PanicA>(&db, cell), 4);
    assert_eq!(*get::<PanicB>(&db, cell), 4);
}

query!(
    NestA,
    u64,
    |db, key| (get::<NestB>(db, key) + 1).min(6);
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(
    NestB,
    u64,
    |db, key| (*get::<NestA>(db, key)).max(*get::<NestC>(db, key));
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(NestC, u64, |db, key| *get::<NestB>(db, key));

#[test]
fn a_nested_cycle_merges_into_the_outermost_head() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    assert_eq!(*get::<NestA>(&db, cell), 6);
    assert_eq!(*get::<NestB>(&db, cell), 6);
    assert_eq!(*get::<NestC>(&db, cell), 6);
}

query!(
    TaskA,
    u64,
    |db, key| {
        if db.armed[0].swap(false, Ordering::SeqCst) {
            db.gate.wait();
        }
        get::<TaskB>(db, key) + 1
    };
    const CYCLES: CycleKind = CycleKind::Result;
    fn fallback(_: &TestDb, _: Cell) -> u64 {
        1
    }
);
query!(
    TaskB,
    u64,
    |db, key| {
        if db.armed[1].swap(false, Ordering::SeqCst) {
            db.gate.wait();
        }
        get::<TaskA>(db, key) + 1
    };
    const CYCLES: CycleKind = CycleKind::Result;
    fn fallback(_: &TestDb, _: Cell) -> u64 {
        2
    }
);

/// `F`'s value, asked again each time the task gave way, as a thread with no query below it does.
fn until_it_holds<F: Function<Db = TestDb, Key = Cell, Value = u64>>(
    db: &TestDb,
    cell: Cell,
) -> u64 {
    loop {
        match Cancelled::catch(AssertUnwindSafe(|| *get::<F>(db, cell))) {
            Ok(value) => return value,
            Err(Cancelled::WouldWaitOnItself { .. }) => {}
            Err(other) => panic!("cancelled: {other}"),
        }
    }
}

#[test]
fn a_cycle_across_two_tasks_is_resolved_by_one_of_them() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    db.armed[0].store(true, Ordering::SeqCst);
    db.armed[1].store(true, Ordering::SeqCst);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| until_it_holds::<TaskA>(&db, cell));
        let b = scope.spawn(|| until_it_holds::<TaskB>(&db, cell));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!((a, b), (1, 2));
}

#[test]
fn a_job_that_gave_way_runs_again_on_its_caller_and_never_reaches_it() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    db.armed[0].store(true, Ordering::SeqCst);
    db.armed[1].store(true, Ordering::SeqCst);
    let pool =
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(2), false).unwrap();
    let both = pool.install(|| {
        super::par_map(
            2,
            |at| if at == 0 { *get::<TaskA>(&db, cell) } else { *get::<TaskB>(&db, cell) },
        )
    });
    assert_eq!(both, [1, 2]);
}

query!(Slow, u64, |db, key| {
    blaze_graph::spin::busy(std::time::Duration::from_millis(20));
    key.a(db)
});

#[test]
fn one_run_of_a_key_however_many_tasks_ask() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 7, 0);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| assert_eq!(*get::<Slow>(&db, cell), 7));
        }
    });
    assert_eq!(db.runs("Slow"), 1);
}

#[test]
fn resume_readers_clears_the_stop_without_a_view() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 0);
    assert_eq!(*get::<Double>(&db, cell), 2);
    let view = db.storage().view();
    db.storage().stop_readers();
    db.storage_mut().resume_readers();
    assert!(!db.storage().readers_stopped());
    assert_eq!(db.storage().view(), view);
    assert_eq!(Cancelled::catch(AssertUnwindSafe(|| *get::<Double>(&db, cell))), Ok(2));
    assert_eq!(db.runs("Double"), 1);
}

#[test]
fn stopped_readers_unwind_until_the_next_write() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 0);
    assert_eq!(*get::<Double>(&db, cell), 2);
    db.storage().stop_readers();
    assert!(db.storage().readers_stopped());
    assert_eq!(
        Cancelled::catch(AssertUnwindSafe(|| *get::<Double>(&db, cell))),
        Err(Cancelled::PendingWrite)
    );
    assert_eq!(
        Cancelled::catch(AssertUnwindSafe(|| unwind_if_cancelled(&db))),
        Err(Cancelled::PendingWrite)
    );
    db.storage_mut().synthetic_write(Durability::LOW);
    assert!(!db.storage().readers_stopped());
    assert_eq!(*get::<Double>(&db, cell), 2);
    let other = catch_unwind(|| Cancelled::catch(|| -> u32 { panic!("not a cancel") }))
        .expect_err("passes through");
    assert!(message(&*other).contains("not a cancel"));
}

/// A singleton input of one field.
struct Config;

impl Input for Config {
    type Fields = (u32,);
    const NAME: &'static str = "Config";
    const FILE: &'static str = file!();
    const FIELDS: u32 = 1;
    const FIELD_NAMES: &'static [&'static str] = &["value"];
    const SINGLETON: bool = true;

    fn ingredient() -> &'static Ingredient {
        ingredient!(input::<Config>)
    }
}

#[test]
fn a_singleton_is_made_once() {
    let mut db = TestDb::new();
    assert_eq!(input_singleton::<Config>(db.storage()), None);
    let id = input_new_singleton::<Config, 1>(db.storage_mut(), (3,), [Durability::HIGH]);
    assert_eq!(input_singleton::<Config>(db.storage()), Some(id));
    assert_eq!(input_fields::<Config>(db.storage(), id).0, 3);
    let payload = catch_unwind(AssertUnwindSafe(|| {
        input_new_singleton::<Config, 1>(db.storage_mut(), (4,), [Durability::HIGH])
    }))
    .expect_err("a second singleton");
    assert!(message(&*payload).contains("singleton"));
    let payload = catch_unwind(AssertUnwindSafe(|| {
        input_new::<Config, 1>(db.storage(), (4,), [Durability::HIGH])
    }))
    .expect_err("a singleton made by `&`");
    assert!(message(&*payload).contains("singleton"));
}

query!(ConfigOrZero, u64, |db, _key| match input_singleton::<Config>(db.storage()) {
    Some(id) => {
        input_read::<Config>(db.storage(), id, 0);
        u64::from(input_fields::<Config>(db.storage(), id).0)
    }
    None => 0,
});

/// Reading a singleton's absence is a dependency: making it reruns exactly those readers,
/// though it is made after they ran in the same view (queue 01228).
#[test]
fn making_a_singleton_reruns_what_read_its_absence() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 2);
    let other = Cell::new(&db, 3, 4);
    assert_eq!(*get::<ConfigOrZero>(&db, cell), 0);
    assert_eq!(*get::<Double>(&db, other), 6);
    input_new_singleton::<Config, 1>(db.storage_mut(), (5,), [Durability::HIGH]);
    assert_eq!(*get::<ConfigOrZero>(&db, cell), 5, "its absence was read");
    assert_eq!(*get::<Double>(&db, other), 6);
    assert_eq!((db.runs("ConfigOrZero"), db.runs("Double")), (2, 1));
}

thread_local! {
    static RAN_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn count_run() {
    RAN_HERE.with(|it| it.set(it.get() + 1));
}

/// The stall watchdog is told of each run and of no hit ([`crate::on_each_run`]).
#[test]
fn each_run_is_told_and_no_hit_is() {
    assert!(crate::on_each_run(count_run).is_ok(), "only this test sets the hook");
    let db = TestDb::new();
    let cell = Cell::new(&db, 1, 2);
    let ran = || RAN_HERE.with(std::cell::Cell::get);
    let before = ran();
    assert_eq!(*get::<Double>(&db, cell), 2);
    let told = ran() - before;
    assert_eq!(told, db.runs("Double") as u64, "one per run, on the thread it ran on");
    assert_eq!(*get::<Double>(&db, cell), 2);
    assert_eq!(ran() - before, told, "a hit is not told");
    assert!(crate::on_each_run(count_run).is_err(), "once per process");
}

/// Interned strings.
struct Names;

impl Interned for Names {
    type Fields = String;
    const NAME: &'static str = "Name";
    const FILE: &'static str = file!();

    fn ingredient() -> &'static Ingredient {
        ingredient!(interned::<Names>)
    }

    fn ids_of(_: &String, _: &mut Vec<Id>) {}
}

fn name(db: &TestDb, text: &str) -> Id {
    intern_by::<Names, &str>(db.storage(), text, |text, it| *text == it.as_str(), str::to_owned)
}

#[test]
fn many_threads_interning_the_same_values_get_one_id_each() {
    let db = TestDb::new();
    let texts: Vec<String> = (0..500).map(|i| format!("name{i}")).collect();
    let all: Vec<Vec<Id>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|t| {
                let (db, texts) = (&db, &texts);
                scope.spawn(move || {
                    let mut ids = vec![None; texts.len()];
                    for at in (0..texts.len()).map(|i| (i * 7 + t * 31) % texts.len()) {
                        ids[at] = Some(name(db, &texts[at]));
                    }
                    ids.into_iter().map(Option::unwrap).collect::<Vec<Id>>()
                })
            })
            .collect();
        workers.into_iter().map(|it| it.join().unwrap()).collect()
    });
    assert!(all.iter().all(|ids| *ids == all[0]));
    let mut distinct = all[0].clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), texts.len());
    for (text, &id) in texts.iter().zip(&all[0]) {
        assert_eq!(interned_fields::<Names>(db.storage(), id), text);
        assert_eq!(intern::<Names>(db.storage(), text.clone()), id, "by value as by borrow");
    }
}

/// Interned pairs of an ID and a number, whose `ids_of` is the ID; a function's hidden key.
struct Pairs;

impl Interned for Pairs {
    type Fields = (Id, u32);
    const NAME: &'static str = "probe::interned_arguments";
    const FILE: &'static str = file!();

    fn ingredient() -> &'static Ingredient {
        ingredient!(interned::<Pairs>)
    }

    fn ids_of(fields: &(Id, u32), out: &mut Vec<Id>) {
        out.push(fields.0);
    }
}

/// An id from outside is checked before its fields are read: only a slot interned on an
/// interned ingredient's page is made; the next slot of that page, an input's row and a page
/// nobody owns are not.
#[test]
fn only_an_interned_slot_is_made() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    let id = name(&db, "made");
    let storage = db.storage();
    assert!(storage.made(id));
    let next = Id::at(id.page(), id.slot() + 1);
    assert_eq!(storage.owner(next), storage.owner(id), "the same page");
    assert!(!storage.made(next), "a slot of the page never interned");
    let payload = catch_unwind(AssertUnwindSafe(|| interned_fields::<Names>(storage, next)))
        .expect_err("what `made` keeps a caller from");
    assert!(message(&*payload).contains("was never interned"));
    assert!(!storage.made(cell.0), "an input's row");
    assert!(!storage.made(Id::from_u32(Id::MAX_U32)), "a page nobody owns");
}

#[test]
fn owners_ids_and_argument_names_are_read_from_the_pages() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    let pair = intern::<Pairs>(db.storage(), (cell.0, 9));
    let mut ids = Vec::new();
    db.storage().ids_of(pair, &mut ids);
    assert_eq!(ids, [cell.0]);
    assert_eq!(interned_arguments(db.storage(), pair), Some("probe"));
    assert_eq!(interned_arguments(db.storage(), cell.0), None);
    assert_eq!(db.storage().owner(cell.0), Some(Cells::ingredient().index()));
    assert_eq!(
        db.storage().ingredient_name(Pairs::ingredient().index()),
        "probe::interned_arguments"
    );
    assert!(db.storage().ingredient_file(Cells::ingredient().index()).ends_with("tests.rs"));
    let cells = Cells::ingredient().index();
    assert_eq!(db.storage().field_name(cells, 1), "b");
    let at = DatabaseKeyIndex::new(cells, cell.0);
    assert_eq!(db.storage().read_name(at, ReadKind::InputField(0)), "a");
    assert_eq!(db.storage().read_name(at, ReadKind::Function), "Cell");
    let items = Items::ingredient().index();
    assert_eq!(
        db.storage().read_name(DatabaseKeyIndex::new(items, cell.0), ReadKind::TrackedField(0)),
        "count"
    );
    let payload = catch_unwind(AssertUnwindSafe(|| db.storage().field_name(cells, 2)))
        .expect_err("`Cell` has two fields");
    assert!(message(&*payload).contains("not a field 2"));
    let payload = catch_unwind(AssertUnwindSafe(|| input_fields::<Cells>(db.storage(), pair).0))
        .expect_err("a pair read as a cell");
    assert!(message(&*payload).contains("read as Cell"));
    assert!(Cell::from_id_in(db.storage(), pair).is_none());
}

/// Tracked structs of one `#[tracked]` field, identified by a number.
struct Items;

impl Tracked for Items {
    type Identity = u64;
    type Fields = (u64,);
    const NAME: &'static str = "Item";
    const FILE: &'static str = file!();
    const TRACKED_FIELDS: u32 = 1;
    const TRACKED_FIELD_NAMES: &'static [&'static str] = &["count"];

    fn ingredient() -> &'static Ingredient {
        ingredient!(tracked::<Items>)
    }

    fn equal_field(_: u32, old: &(u64,), new: &(u64,)) -> bool {
        old == new
    }
}

query!(MakeItem, Id, |db, key| tracked_new::<Items>(db.storage(), 0, (key.a(db),)));
query!(ReadItem, u64, |db, key| {
    let item = *get::<MakeItem>(db, key);
    tracked_fields::<Items>(db.storage(), item, 0).0 * 2
});

#[test]
fn a_tracked_struct_keeps_its_id_and_its_field_has_an_edge() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<ReadItem>(&db, cell), 6);
    let item = *get::<MakeItem>(&db, cell);
    cell.set_a(&mut db, 4);
    assert_eq!(*get::<ReadItem>(&db, cell), 8);
    assert_eq!(*get::<MakeItem>(&db, cell), item, "the same identity is the same struct");
    assert_eq!((db.runs("MakeItem"), db.runs("ReadItem")), (2, 2));
    cell.set_b(&mut db, 1);
    assert_eq!(*get::<ReadItem>(&db, cell), 8);
    assert_eq!((db.runs("MakeItem"), db.runs("ReadItem")), (2, 2));
}

query!(Capture, u64, |db, key| {
    let (value, reads) = capture_reads(db, || key.a(db));
    db.shared.lock().insert(key.0.as_u32(), (value, reads.expect("a plain read is shareable")));
    value
});
query!(Replay, u64, |db, key| {
    let (value, reads) = db.shared.lock()[&key.0.as_u32()];
    replay_reads(db, reads);
    value + 1
});
query!(Unshare, bool, |db, key| {
    let ((), reads) = capture_reads(db, || {
        key.a(db);
        unshareable_reads(db);
    });
    reads.is_none()
});

#[test]
fn captured_reads_replay_into_another_query() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Capture>(&db, cell), 3);
    assert_eq!(*get::<Replay>(&db, cell), 4);
    let (_, reads) = db.shared.lock()[&cell.0.as_u32()];
    assert_eq!(reads.len(), 1);
    let first =
        reads.for_each_read(db.storage(), |read| ControlFlow::Break(read)).expect("one read");
    assert_eq!(first.kind, ReadKind::InputField(0));
    assert_eq!(
        (first.key.ingredient_index(), first.key.key_index()),
        (Cells::ingredient().index(), cell.0)
    );
    cell.set_a(&mut db, 5);
    assert_eq!(*get::<Capture>(&db, cell), 5);
    assert_eq!(*get::<Replay>(&db, cell), 6, "the replayed read is the replaying query's edge");
    cell.set_b(&mut db, 1);
    assert_eq!(*get::<Replay>(&db, cell), 6);
    assert_eq!(db.runs("Replay"), 2);
    assert!(capture_reads(&db, || 0).1.is_none(), "no query runs");
    assert!(*get::<Unshare>(&db, cell));
}

query!(
    Tens,
    u64,
    |db, key| key.a(db) / 10;
    const EVICT: bool = true;
);
query!(TensReader, u64, |db, key| get::<Tens>(db, key) + 1);

query!(Quad, u64, |db, key| get::<Double>(db, key) * 2);

fn table(db: &TestDb) -> Vec<Provenance> {
    let mut out = Vec::new();
    db.storage().provenance(&mut out);
    out
}

fn key_of<F: Function<Db = TestDb, Key = Cell>>(cell: Cell) -> DatabaseKeyIndex {
    DatabaseKeyIndex::new(F::ingredient().index(), cell.0)
}

/// What `F` of `cell` did in the view `table` is of, and why it ran.
fn did<F: Function<Db = TestDb, Key = Cell>>(
    table: &[Provenance],
    cell: Cell,
) -> Option<(Outcome, Option<Cause>)> {
    let key = key_of::<F>(cell);
    table.iter().find(|it| it.key == key).map(|it| (it.outcome, it.cause))
}

#[test]
fn a_view_tells_what_each_memo_did_and_why_it_ran() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 1, 5);
    let ask = |db: &TestDb| {
        get::<Quad>(db, cell);
        get::<Label>(db, cell);
        get::<ReadB>(db, cell);
        get::<Untracked>(db, cell);
    };
    ask(&db);
    let first = table(&db);
    assert_eq!(first.len(), 6, "Quad, Double, Label, Parity, ReadB, Untracked");
    assert!(first.iter().all(|it| (it.outcome, it.cause) == (Outcome::Reran, Some(Cause::New))));
    ask(&db);
    assert_eq!(table(&db), first, "a hit records nothing");
    let kept = Cell::new(&db, 11, 0);
    assert_eq!(*get::<TensReader>(&db, kept), 2);

    cell.set_a(&mut db, 3);
    ask(&db);
    let after = table(&db);
    let a = field_node::<Cells>(db.storage(), cell.0, 0);
    let double = after.iter().find(|it| it.key == key_of::<Double>(cell)).expect("Double").node;
    let changed = |node| Some(Cause::Changed(node));
    assert_eq!(did::<Double>(&after, cell), Some((Outcome::Reran, changed(a))));
    assert_eq!(did::<Quad>(&after, cell), Some((Outcome::Reran, changed(double))));
    assert_eq!(did::<Parity>(&after, cell), Some((Outcome::ReranSameOutput, changed(a))));
    assert_eq!(did::<Label>(&after, cell), Some((Outcome::Verified, None)), "cut off at Parity");
    assert_eq!(did::<ReadB>(&after, cell), Some((Outcome::Verified, None)));
    assert_eq!(
        did::<Untracked>(&after, cell),
        Some((Outcome::ReranSameOutput, Some(Cause::Untracked)))
    );
    let read = db.storage().node_read(a);
    assert_eq!((read.key.key_index(), read.kind), (cell.0, ReadKind::InputField(0)));
    assert_eq!(db.storage().node_read(double).key, key_of::<Double>(cell));

    assert_eq!(*get::<TensReader>(&db, kept), 2);
    assert_eq!(evict_values::<Tens>(&mut db), 1);
    assert_eq!(*get::<Tens>(&db, kept), 1);
    let evicted = table(&db);
    assert_eq!(did::<TensReader>(&evicted, kept), Some((Outcome::Verified, None)));
    assert_eq!(
        did::<Tens>(&evicted, kept),
        Some((Outcome::ReranSameOutput, Some(Cause::Evicted))),
        "a value of an earlier view, evicted and made again equal"
    );
}

#[test]
fn an_evicted_value_recomputed_equal_does_not_rerun_its_readers() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 11, 0);
    assert_eq!(*get::<TensReader>(&db, cell), 2);
    assert_eq!(evict_values::<Tens>(&mut db), 1);
    cell.set_a(&mut db, 12);
    assert_eq!(*get::<TensReader>(&db, cell), 2);
    assert_eq!((db.runs("Tens"), db.runs("TensReader")), (2, 1));
    assert_eq!(evict_values_of::<Tens>(&mut db, &[cell]), 1);
    assert_eq!(*get::<Tens>(&db, cell), 1, "evicted in this view: run again, stamp kept");
    cell.set_a(&mut db, 25);
    assert_eq!(*get::<TensReader>(&db, cell), 3);
}

query!(
    CountedTens,
    u64,
    |db, key| key.a(db) / 10;
    const EVICT: bool = true;
);
query!(CountedReader, u64, |db, key| get::<CountedTens>(db, key) + 1);
query!(
    CountedCapped,
    u64,
    |db, key| key.a(db);
    const BYTES: Option<usize> = Some(WEIGHT);
    const EVICT: bool = true;
);

/// Every query's counts taken since a test last asked for its own ([`counts_of`]): a take is
/// process-wide, so tests that read counts take into here, and each reads only its queries'.
static TAKEN: Lock<Vec<(&'static str, Counts)>> = Lock::new("query.test.db.taken", Vec::new());

/// What query `query` did since the last time a test asked for it: counts added, marks the
/// most.
fn counts_of(query: &str) -> Counts {
    let mut taken = TAKEN.lock();
    crate::db::take_counts(|name, counts| match taken.iter_mut().find(|it| it.0 == name) {
        Some((_, was)) => {
            *was = Counts {
                executed: was.executed + counts.executed,
                verified: was.verified + counts.verified,
                reran: was.reran + counts.reran,
                evicted_asked: was.evicted_asked + counts.evicted_asked,
                evicted: was.evicted + counts.evicted,
                dropped: was.dropped + counts.dropped,
                reads_recorded: was.reads_recorded + counts.reads_recorded,
                reads_kept: was.reads_kept + counts.reads_kept,
                reads_most: was.reads_most.max(counts.reads_most),
                written: was.written + counts.written,
                ran_new: was.ran_new + counts.ran_new,
                ran_changed: was.ran_changed + counts.ran_changed,
                ran_untracked: was.ran_untracked + counts.ran_untracked,
                ran_cycle: was.ran_cycle + counts.ran_cycle,
            };
        }
        None => taken.push((name, counts)),
    });
    taken
        .iter()
        .position(|it| it.0 == query)
        .map_or_else(Counts::default, |at| taken.swap_remove(at).1)
}

/// What the three counted queries did since the last take.
fn counted() -> [Counts; 3] {
    ["CountedTens", "CountedReader", "CountedCapped"].map(counts_of)
}

/// `(executed, verified, reran, evicted_asked)` of `counts`.
fn did_runs(counts: Counts) -> (u64, u64, u64, u64) {
    (counts.executed, counts.verified, counts.reran, counts.evicted_asked)
}

/// What the profile's `query_counts` table holds: a run counts as executed, a row proved
/// unchanged as verified, only a second run of a row in one view (its value evicted and asked
/// for again) as reran, and every run for an evicted value, in any view, as evicted-asked; a
/// hit is nothing. A value an explicit `evict_values_of` let go counts as dropped, one a pass
/// let go under its tag's budget as evicted.
#[test]
fn each_run_verify_rerun_and_eviction_is_counted_per_query() {
    let mut db = TestDb::new();
    db.storage_mut().set_value_budget(Tag::Query, Some(2 * WEIGHT as u64));
    let cell = Cell::new(&db, 11, 0);
    counted();
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    let [tens, reader, _] = counted().map(did_runs);
    assert_eq!([tens, reader], [(1, 0, 0, 0), (1, 0, 0, 0)], "a first run each, a hit nothing");

    cell.set_a(&mut db, 12);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    let [tens, reader, _] = counted().map(did_runs);
    assert_eq!([tens, reader], [(1, 0, 0, 0), (0, 1, 0, 0)], "tens ran equal: reader verified");

    assert_eq!(evict_values_of::<CountedTens>(&mut db, &[cell]), 1);
    assert_eq!(*get::<CountedTens>(&db, cell), 1);
    let [tens, reader, _] = counted();
    assert_eq!(
        [did_runs(tens), did_runs(reader)],
        [(1, 0, 1, 1), (0, 0, 0, 0)],
        "run again in the view it ran in"
    );
    assert_eq!((tens.dropped, tens.evicted), (1, 0), "an explicit drop, not a pass");

    cell.set_b(&mut db, 1);
    assert_eq!(evict_values_of::<CountedTens>(&mut db, &[cell]), 1);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    assert_eq!(*get::<CountedTens>(&db, cell), 1);
    let [tens, reader, _] = counted().map(did_runs);
    let (executed, verified, reran, evicted_asked) = tens;
    assert_eq!((executed, reran), (1, 0), "evicted in an earlier view: a run, not a rerun");
    assert_eq!(evicted_asked, 1, "but a run for an evicted value all the same");
    assert_eq!(verified, 1, "proved unchanged before it ran for its value");
    assert_eq!(reader, (0, 1, 0, 0));

    scan::<CountedCapped>(&mut db);
    let [_, _, capped] = counted();
    assert_eq!(
        capped.evicted, 2,
        "three values over a budget of two, down to its low water: two go"
    );
    assert_eq!((capped.evicted_asked, capped.dropped), (0, 0));
}

/// An input of one field only [`each_set_of_an_input_is_counted_as_written`] makes, so no
/// other test's writes reach its counts (a take is process-wide).
struct Written;

impl Input for Written {
    type Fields = (u64,);
    const NAME: &'static str = "Written";
    const FILE: &'static str = file!();
    const FIELDS: u32 = 1;
    const FIELD_NAMES: &'static [&'static str] = &["value"];
    const SINGLETON: bool = false;

    fn ingredient() -> &'static Ingredient {
        ingredient!(input::<Written>)
    }
}

fn written_value(fields: &mut (u64,)) -> &mut u64 {
    &mut fields.0
}

/// What `query_counts.written` holds: each setter's write of an input's field counts once
/// against that input, an equal value too (it opens a view all the same), and making one
/// counts nothing. harness/incremental.py names a no-op build's re-runs by it.
#[test]
fn each_set_of_an_input_is_counted_as_written() {
    let mut db = TestDb::new();
    let id = input_new::<Written, 1>(db.storage(), (1,), [Durability::LOW]);
    assert_eq!(counts_of("Written").written, 0, "made, not written");
    input_set::<Written, u64>(db.storage_mut(), id, 0, written_value).to(2);
    input_set::<Written, u64>(db.storage_mut(), id, 0, written_value).to(2);
    let counts = counts_of("Written");
    assert_eq!(counts.written, 2, "two writes, the second of an equal value: {counts:?}");
    assert_eq!(counts.executed, 0, "{counts:?}");
}

/// Asks for one key of `F` in two views, then for two other keys in a third, and opens a
/// fourth, whose pass is over a budget of two values. The key asked for in two views is
/// returned.
fn scan<F: Function<Db = TestDb, Key = Cell, Value = u64>>(db: &mut TestDb) -> Cell {
    let cells: Vec<Cell> = (0..3).map(|i| Cell::new(db, 40 + i, 0)).collect();
    let hot = cells[0];
    get::<F>(db, hot);
    db.storage_mut().synthetic_write(Durability::LOW);
    get::<F>(db, hot);
    db.storage_mut().synthetic_write(Durability::LOW);
    get::<F>(db, cells[1]);
    get::<F>(db, cells[2]);
    db.storage_mut().synthetic_write(Durability::LOW);
    hot
}

query!(
    Scanned,
    u64,
    |db, key| key.a(db);
    const BYTES: Option<usize> = Some(WEIGHT);
    const EVICT: bool = true;
);

/// What a budget is for: across views that each ask for one hot key and a scan of new ones, the
/// values never weigh more than the budget once a view is over, and the hot key, verified in
/// every view, is never let go (the newest view's values are the last to go), so it runs once.
#[test]
fn the_budget_holds_and_a_key_asked_for_in_every_view_is_never_let_go() {
    const BUDGET: u64 = 40 * WEIGHT as u64;
    let mut db = TestDb::new();
    db.storage_mut().set_value_budget(Tag::Query, Some(BUDGET));
    let hot = Cell::new(&db, 7, 0);
    let mut made = 0;
    for _ in 0..30 {
        assert_eq!(*get::<Scanned>(&db, hot), 7);
        for _ in 0..9 {
            made += 1;
            let cold = Cell::new(&db, 100 + made, 0);
            assert_eq!(*get::<Scanned>(&db, cold), 100 + made);
        }
        db.storage_mut().synthetic_write(Durability::LOW);
        let held = values_held(&db, "Scanned");
        assert!(
            held * WEIGHT as u64 <= BUDGET,
            "{held} values of {WEIGHT} bytes over a budget of {BUDGET}"
        );
    }
    assert_eq!(db.runs_of("Scanned", hot.as_id()), 1, "the hot key ran once: never let go");
    assert_eq!(db.runs("Scanned"), 1 + 270, "each key ran once: none was asked for after it went");
}

/// What the entry-count caps cost (1.0 M reruns in a self-build, `parse_macro_expansion` at 512
/// against about 500 k keys): values that fit their budget are never let go, however many.
#[test]
fn values_that_fit_the_budget_stay_however_many() {
    let mut db = TestDb::new();
    db.storage_mut().set_value_budget(Tag::Query, Some(5000 * WEIGHT as u64));
    let cells: Vec<Cell> = (0..5000).map(|i| Cell::new(&db, i, 0)).collect();
    for _ in 0..3 {
        for &cell in &cells {
            get::<Scanned>(&db, cell);
        }
        db.storage_mut().synthetic_write(Durability::LOW);
        assert_eq!(db.storage().swept(), 0, "under the budget, a view reads no value");
    }
    assert_eq!(values_held(&db, "Scanned"), 5000);
    assert_eq!(db.runs("Scanned"), 5000, "each ran once");
}

static HASHES: AtomicUsize = AtomicUsize::new(0);

query!(
    Hashed,
    u64,
    |db, key| key.a(db) / 10;
    const EVICT: bool = true;
    fn hash_value(value: &u64) -> u64 {
        HASHES.fetch_add(1, Ordering::SeqCst);
        value_hash(value)
    }
);
query!(HashedReader, u64, |db, key| get::<Hashed>(db, key) + 1);

#[test]
fn an_evictable_function_hashes_its_value_only_when_evicted() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 11, 0);
    assert_eq!(*get::<HashedReader>(&db, cell), 2);
    cell.set_a(&mut db, 15);
    assert_eq!(*get::<HashedReader>(&db, cell), 2, "rerun equal, backdated by `==`");
    assert_eq!(HASHES.load(Ordering::SeqCst), 0, "a run that keeps its value hashes nothing");
    assert_eq!(evict_values::<Hashed>(&mut db), 1);
    assert_eq!(HASHES.load(Ordering::SeqCst), 1, "hashed once, as it was evicted");
    cell.set_a(&mut db, 12);
    assert_eq!(*get::<HashedReader>(&db, cell), 2, "rerun equal, backdated by the hash");
    assert_eq!(
        HASHES.load(Ordering::SeqCst),
        2,
        "the rerun of an evicted row hashes its new value"
    );
    assert_eq!((db.runs("Hashed"), db.runs("HashedReader")), (3, 1));
}

static UNEQUAL_HASHES: AtomicUsize = AtomicUsize::new(0);

query!(
    UnequalHashed,
    u64,
    |db, key| key.a(db) / 10;
    const EVICT: bool = true;
    const UNEQUAL: bool = true;
    fn hash_value(value: &u64) -> u64 {
        UNEQUAL_HASHES.fetch_add(1, Ordering::SeqCst);
        value_hash(value)
    }
);
query!(UnequalReader, u64, |db, key| get::<UnequalHashed>(db, key) + 1);

#[test]
fn an_evictable_no_eq_function_never_hashes_and_every_rerun_changes() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 11, 0);
    assert_eq!(*get::<UnequalReader>(&db, cell), 2);
    cell.set_a(&mut db, 15);
    assert_eq!(*get::<UnequalReader>(&db, cell), 2);
    assert_eq!(evict_values::<UnequalHashed>(&mut db), 1);
    cell.set_a(&mut db, 12);
    assert_eq!(*get::<UnequalReader>(&db, cell), 2);
    assert_eq!(UNEQUAL_HASHES.load(Ordering::SeqCst), 0, "`no_eq`: nothing is hashed");
    assert_eq!(
        (db.runs("UnequalHashed"), db.runs("UnequalReader")),
        (3, 3),
        "an equal rerun is still a change, evicted or not"
    );
}

/// What each value of a test's `lru` query weighs.
const WEIGHT: usize = 8;

query!(
    Capped,
    u64,
    |db, key| key.a(db);
    const BYTES: Option<usize> = Some(WEIGHT);
    const EVICT: bool = true;
);

/// How many values the `lru` query `name` holds: what its values hold beside their rows over
/// [`WEIGHT`].
fn values_held(db: &TestDb, name: &str) -> u64 {
    let mut usage = Vec::new();
    db.storage().memory_usage(&mut usage);
    usage.iter().find(|it| it.name == name).map_or(0, |it| it.value_bytes / WEIGHT as u64)
}

/// The memory report tells a function's rows made (whole pages of 1024) from its rows in use
/// (those that ran), and an evicted value's row is still in use: its memo is kept. A row's
/// value in place is in its row's bytes, not counted again as a value's.
#[test]
fn the_memory_report_counts_a_function_s_rows_in_use_apart_from_its_rows_made() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..3).map(|i| Cell::new(&db, i, 0)).collect();
    get::<Capped>(&db, cells[0]);
    get::<Capped>(&db, cells[2]);
    let capped = |db: &TestDb| {
        let mut usage = Vec::new();
        db.storage().memory_usage(&mut usage);
        *usage.iter().find(|it| it.name == "Capped").expect("a row for `Capped`")
    };
    let usage = capped(&db);
    assert_eq!(
        (usage.rows, usage.used),
        (1024, 2),
        "one page made for the cells' ID page, two keys run"
    );
    assert_eq!(
        usage.value_bytes,
        2 * WEIGHT as u64,
        "two values weighed, their `u64`s in place in the rows"
    );
    assert_eq!(usage.held(), usage.row_bytes + 2 * WEIGHT as u64);
    assert_eq!(evict_values::<Capped>(&mut db), 2);
    let usage = capped(&db);
    assert_eq!(
        (usage.rows, usage.used, usage.value_bytes),
        (1024, 2, 0),
        "values gone, memos kept"
    );
}

/// A sweep between views (a build's release mid-build) holds each tag's values under the least
/// of its budget and the cap given, opens no view, and a value it let go runs again in the same
/// view when read.
#[test]
fn a_sweep_between_views_holds_values_under_the_cap_given_without_a_new_view() {
    let mut db = TestDb::new();
    db.storage_mut().set_value_budget(Tag::Query, Some(1 << 40));
    let cells: Vec<Cell> = (0..4).map(|i| Cell::new(&db, i, 0)).collect();
    for &cell in &cells {
        get::<Capped>(&db, cell);
    }
    let view = db.storage().view();
    let mut caps = [u64::MAX; Tag::COUNT];
    db.storage_mut().sweep_values(&caps);
    assert_eq!(values_held(&db, "Capped"), 4, "under its own budget, uncapped: kept");
    caps[Tag::Query.index()] = 2 * WEIGHT as u64;
    db.storage_mut().sweep_values(&caps);
    assert_eq!(values_held(&db, "Capped"), 1, "down to the cap's low water of 14 bytes");
    assert_eq!(db.storage().view(), view, "no view opened");
    for (i, &cell) in cells.iter().enumerate() {
        assert_eq!(*get::<Capped>(&db, cell), i as u64);
    }
    assert_eq!(db.runs("Capped"), 7, "the three let go ran again in the same view");
}

#[test]
fn a_new_view_lets_values_go_over_the_budget_and_keep_every_value_stops_it() {
    let mut db = TestDb::new();
    db.storage_mut().set_value_budget(Tag::Query, Some(2 * WEIGHT as u64));
    let cells: Vec<Cell> = (0..4).map(|i| Cell::new(&db, i, 0)).collect();
    for &cell in &cells {
        get::<Capped>(&db, cell);
    }
    assert_eq!(values_held(&db, "Capped"), 4);
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "Capped"), 1, "down to the low water of 14 bytes");
    db.storage_mut().keep_every_value(true);
    for (i, &cell) in cells.iter().enumerate() {
        assert_eq!(*get::<Capped>(&db, cell), i as u64);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "Capped"), 4);
    assert_eq!(db.runs("Capped"), 7, "three evicted values ran again");
}

/// Each input write opens a view, thousands of times a build: weighing at a view costs the
/// values made since the last one, and only a pass (over the budget) reads the values held,
/// never every row made (a sweep that read every row was most of a daemon self-build's time,
/// sb3slow).
#[test]
fn a_view_reads_no_value_under_the_budget_and_a_pass_only_those_held() {
    let mut db = TestDb::new();
    db.storage_mut().set_value_budget(Tag::Query, Some(2 * WEIGHT as u64));
    let cells: Vec<Cell> = (0..1000).map(|i| Cell::new(&db, i, 0)).collect();
    for &cell in &cells {
        get::<Capped>(&db, cell);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "Capped"), 1);
    assert_eq!(
        db.storage().swept(),
        1000,
        "the pass over the view that made them: each value once"
    );
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(db.storage().swept(), 0, "a view under the budget reads no value");
    let late = Cell::new(&db, 1000, 0);
    assert_eq!(*get::<Capped>(&db, late), 1000);
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(db.storage().swept(), 0, "two values, at the budget");
    let later = Cell::new(&db, 1001, 0);
    assert_eq!(*get::<Capped>(&db, later), 1001);
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(db.storage().swept(), 3, "over it: the three held, not 1002 rows");
    assert_eq!(values_held(&db, "Capped"), 1);
}

query!(
    BudgetLater,
    u64,
    |db, key| key.a(db);
    const BYTES: Option<usize> = Some(WEIGHT);
    const EVICT: bool = true;
);

#[test]
fn a_budget_set_at_runtime_applies_at_the_next_view() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..6).map(|i| Cell::new(&db, i, 0)).collect();
    db.storage_mut().set_value_budget(Tag::Query, Some(4 * WEIGHT as u64));
    for &cell in &cells {
        get::<BudgetLater>(&db, cell);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "BudgetLater"), 3, "48 bytes over 32, down to 28: three go");
    db.storage_mut().set_value_budget(Tag::Query, Some(u64::MAX));
    for (i, &cell) in cells.iter().enumerate() {
        assert_eq!(*get::<BudgetLater>(&db, cell), i as u64);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "BudgetLater"), 6, "no budget: nothing let go");
}

#[test]
fn views_renumber_at_the_limit_and_every_answer_holds() {
    let mut db = TestDb::new();
    let start = VIEW_MASK - 6;
    db.storage_mut().start_at_view(start);
    let before = db.storage().view();
    let cell = Cell::new(&db, 3, 7);
    let still = Cell::new(&db, 21, 0);
    let other = Cell::new(&db, 0, 0);
    assert_eq!(*get::<Label>(&db, cell), 11);
    assert_eq!(*get::<ReadB>(&db, cell), 7);
    assert_eq!(*get::<Double>(&db, still), 42);
    let mut a = 3;
    for round in 0..14 {
        if round % 3 == 0 {
            a = round + 4;
            cell.set_a(&mut db, a);
        } else {
            other.set_a(&mut db, round);
        }
        assert_eq!(*get::<Label>(&db, cell), (a % 2) * 10 + 1, "round {round}");
        assert_eq!(*get::<ReadB>(&db, cell), 7, "round {round}");
        assert_eq!(*get::<Double>(&db, still), 42, "round {round}");
        let view = table(&db);
        let ran_before = did::<ReadB>(&view, cell);
        assert_eq!(ran_before, Some((Outcome::Verified, None)), "round {round}: an old run");
        if round % 3 == 0 {
            let parity = view.iter().find(|it| it.key == key_of::<Parity>(cell)).expect("Parity");
            let field = field_node::<Cells>(db.storage(), cell.0, 0);
            assert_eq!(parity.cause, Some(Cause::Changed(field)), "round {round}");
            assert_eq!(
                did::<Label>(&view, cell),
                Some((Outcome::Reran, Some(Cause::Changed(parity.node)))),
                "round {round}"
            );
        }
    }
    assert!(db.storage().view_bits() < start, "renumbered past the limit");
    assert!(db.storage().view() > before, "the view hosts see is never reused");
    assert_eq!((db.runs("Double"), db.runs("ReadB")), (1, 1), "unchanged across it: never ran");
    assert_eq!(db.runs("Parity"), 6, "once, then once per write of `a`");
}

fn is_value<T: super::Value + ?Sized>() {}

#[test]
fn foreign_containers_of_values_are_values() {
    is_value::<either::Either<Id, Vec<u32>>>();
    is_value::<Option<either::Either<String, Option<Id>>>>();
    is_value::<Result<thin_vec::ThinVec<Id>, Box<str>>>();
    is_value::<smallvec::SmallVec<[Id; 2]>>();
}

/// A value that counts its drops.
struct Counted(u64, Arc<AtomicUsize>);

impl PartialEq for Counted {
    fn eq(&self, other: &Counted) -> bool {
        self.0 == other.0
    }
}

impl Hash for Counted {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

query!(MakeCounted, Counted, |db, key| Counted(key.a(db), Arc::clone(&db.drops)));

#[test]
fn dropping_the_storage_frees_every_value() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..3).map(|i| Cell::new(&db, i, 0)).collect();
    for &cell in &cells {
        get::<MakeCounted>(&db, cell);
    }
    cells[0].set_a(&mut db, 9);
    assert_eq!(get::<MakeCounted>(&db, cells[0]).0, 9);
    let drops = Arc::clone(&db.drops);
    assert_eq!(drops.load(Ordering::SeqCst), 1, "the replaced value");
    drop(db);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}

#[test]
fn events_reach_the_hook() {
    let events = Arc::new(Lock::new("query.test.db.events", Vec::new()));
    let seen = Arc::clone(&events);
    let hook: Hook =
        Box::new(move |_: &Storage, event: Event| seen.lock().push(format!("{:?}", event.kind)));
    let mut db = TestDb::made(Some(hook), Vec::new());
    let cell = Cell::new(&db, 1, 0);
    let other = Cell::new(&db, 0, 0);
    get::<Double>(&db, cell);
    other.set_a(&mut db, 1);
    get::<Double>(&db, cell);
    let events = events.lock();
    assert_eq!(
        events.iter().filter(|it| it.starts_with("WillExecute") && it.contains("Double(")).count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|it| it.starts_with("DidValidateMemoizedValue") && it.contains("Double("))
    );
}

#[test]
fn attach_lends_the_storage_to_its_closure() {
    let db = TestDb::new();
    assert!(!with_attached(|it| it.is_some()));
    let view = attach(&db, || with_attached(|it| it.map(Storage::view)));
    assert_eq!(view, Some(db.storage().view()));
    assert!(!with_attached(|it| it.is_some()));
}

/// A loop's tasks have their caller's database attached (or none), on whichever worker each
/// runs: a task formats what its caller would, not whatever its worker had.
#[test]
fn a_loops_tasks_have_their_callers_database_attached() {
    let db = TestDb::new();
    let addr = |db: Option<&TestDb>| db.map(|it| std::ptr::from_ref(it).addr());
    let pool =
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(4), false).unwrap();
    let task = |_| {
        // Long enough that the other workers take some of the tasks.
        blaze_graph::spin::busy(std::time::Duration::from_micros(300));
        (with_attached_db(addr), std::thread::current().id())
    };
    let mine = Some(std::ptr::from_ref(&db).addr());
    let attached = pool.install(|| attach(&db, || super::par_map(64, task)));
    let threads: std::collections::BTreeSet<String> =
        attached.iter().map(|it| format!("{:?}", it.1)).collect();
    assert!(threads.len() > 1, "the tasks ran on one thread: {threads:?}");
    assert!(attached.iter().all(|it| it.0 == mine), "{attached:?}");
    let detached = pool.install(|| super::par_map(64, task));
    assert!(detached.iter().all(|it| it.0.is_none()), "{detached:?}");
}

/// `with_attached_db` hands back the lent database as its own type, never as another's, and
/// the one attached before (another database, or none) is back once an inner `attach` ends.
#[test]
fn attach_lends_the_database_as_its_own_type_and_restores_the_one_before() {
    let outer = TestDb::new();
    let inner = TestDb::new();
    let addr = |db: Option<&TestDb>| db.map(|it| std::ptr::from_ref(it).addr());
    assert_eq!(with_attached_db(addr), None);
    attach(&outer, || {
        assert_eq!(with_attached_db(addr), Some(std::ptr::from_ref(&outer).addr()));
        assert!(with_attached_db(|it: Option<&Wrong>| it.is_none()), "lent only as a `TestDb`");
        attach(&inner, || {
            assert_eq!(with_attached_db(addr), Some(std::ptr::from_ref(&inner).addr()))
        });
        assert_eq!(with_attached_db(addr), Some(std::ptr::from_ref(&outer).addr()));
    });
    assert_eq!(with_attached_db(addr), None);

    #[expect(dead_code, reason = "only its type is asked for: none is made")]
    struct Wrong(Storage);
    impl Database for Wrong {
        fn storage(&self) -> &Storage {
            &self.0
        }
        fn storage_mut(&mut self) -> &mut Storage {
            &mut self.0
        }
    }
}

query!(Wide, u64, |db, _key| {
    let cells = db.cells.lock().clone();
    cells.iter().map(|&it| Cell(it).a(db)).sum()
});

/// A rerun replaces its row's edges; the views after free the old ones, so 1200 reruns of a
/// row of 128 reads hold its edges about twice, not 1200 times, and every live span still
/// verifies its row after the move.
#[test]
fn a_rerun_s_replaced_edges_are_freed_at_a_later_view_and_live_spans_survive_the_move() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..128).map(|i| Cell::new(&db, i, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let key = cells[0];
    let mut expected: u64 = (0..128).sum();
    assert_eq!(*get::<Wide>(&db, key), expected);
    get::<Double>(&db, cells[5]);
    let (mut settled, mut before) = (0, 0);
    for round in 0..1200 {
        cells[1].set_a(&mut db, round + 2);
        expected = (0..128).sum::<u64>() - 1 + round + 2;
        assert_eq!(*get::<Wide>(&db, key), expected);
        let (top, dead, _) = db.storage().edge_counts();
        let live = top - dead;
        assert_eq!(live, 129, "round {round}: `Wide`'s 128 edges and `Double`'s one");
        // A view frees the dead once they outnumber the live edges and the nodes; this
        // view's rerun adds one span more.
        let nodes = u64::from(db.storage().nodes_made());
        assert!(
            dead <= live.max(nodes) + 128,
            "round {round}: {dead} dead edges kept beside {live} live"
        );
        settled += u64::from(dead < before);
        before = dead;
    }
    assert!(settled > 0, "no view freed the dead edges");
    assert_eq!(*get::<Double>(&db, cells[5]), 10);
    cells[2].set_a(&mut db, 100);
    assert_eq!(*get::<Wide>(&db, key), expected + 98);
    assert_eq!(*get::<Double>(&db, cells[5]), 10);
    assert_eq!(db.runs("Double"), 1, "its span moved and still verifies it");
}

/// A full edge array is an error naming the query whose reads did not fit, how many, and what
/// the array holds.
#[test]
fn a_full_edge_array_names_the_query_whose_reads_did_not_fit() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..128).map(|i| Cell::new(&db, i, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    db.storage_mut().limit_edges(100);
    let payload = catch_unwind(AssertUnwindSafe(|| *get::<Wide>(&db, cells[0])))
        .expect_err("128 reads in an array of 100 places");
    let message = message(&*payload);
    assert!(
        message.contains("edge array is full (100 places: 0 named by rows, 0 left by reruns")
            && message.contains("the 128 reads of `Wide`("),
        "{message}"
    );
}

query!(Reread, u64, |db, key| {
    let mut sum = 0;
    for _ in 0..100 {
        sum += key.a(db) + key.b(db) + get::<Double>(db, key);
    }
    sum
});

/// A short run that reads the same few nodes again and again keeps each edge once.
#[test]
fn a_short_run_reading_nodes_again_keeps_each_edge_once() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 1, 2);
    assert_eq!(*get::<Reread>(&db, cell), 500);
    let (top, _, _) = db.storage().edge_counts();
    assert_eq!(top, 1 + 3, "`Double`'s read of `a`, and `Reread`'s of `a`, `b` and `Double`");
}

/// Depth of [`Nested`]'s captures.
const DEPTH: usize = 64;

/// Captures the read of `cells[0]` around a capture of the rest, `cells.len()` deep, as the
/// solver captures each goal inside its parent's.
fn nest(db: &TestDb, cells: &[Id]) -> (u64, Option<Reads>) {
    let Some((&first, rest)) = cells.split_first() else { return (0, None) };
    capture_reads(db, || Cell(first).a(db) + nest(db, rest).0)
}

query!(Nested, u64, |db, key| {
    let cells = db.cells.lock().clone();
    let (value, reads) = nest(db, &cells);
    db.shared.lock().insert(key.0.as_u32(), (value, reads.expect("plain reads are shareable")));
    value
});

/// Captures nested `DEPTH` deep hold each read once, by the innermost capture that made it,
/// and each inner capture as one entry: `2 * DEPTH - 1` entries, where copying each capture's
/// reads into every one around it holds `DEPTH * (DEPTH + 1) / 2`. The outermost still gives
/// every read, in read order, to a query that replays it, which reruns when the innermost
/// read changes.
#[test]
fn nested_captures_hold_each_read_once_not_once_per_level() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..DEPTH as u64).map(|i| Cell::new(&db, i, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let key = cells[0];
    let sum: u64 = (0..DEPTH as u64).sum();
    assert_eq!(*get::<Nested>(&db, key), sum);
    let (_, _, captured) = db.storage().edge_counts();
    assert_eq!(
        captured,
        2 * DEPTH as u64 - 1,
        "copies of each capture into those around it hold {}",
        DEPTH * (DEPTH + 1) / 2
    );
    let (_, reads) = db.shared.lock()[&key.0.as_u32()];
    assert_eq!(reads.len(), DEPTH);
    let mut read = Vec::new();
    let _: Option<()> = reads.for_each_read(db.storage(), |it| {
        read.push(it.key.key_index());
        ControlFlow::Continue(())
    });
    assert_eq!(
        read,
        cells.iter().map(|it| it.0).collect::<Vec<_>>(),
        "every level's read, outermost first"
    );
    assert_eq!(*get::<Replay>(&db, key), sum + 1);
    let last = DEPTH as u64 - 1;
    cells[DEPTH - 1].set_a(&mut db, 1000);
    assert_eq!(*get::<Nested>(&db, key), sum - last + 1000);
    assert_eq!(
        *get::<Replay>(&db, key),
        sum - last + 1001,
        "the innermost read reaches the replaying query"
    );
    assert_eq!(db.runs("Replay"), 2);
}

/// A view's captures are freed when the next view opens: a [`Reads`] is read only in its own
/// view, so none is kept for the next.
#[test]
fn a_view_s_captured_reads_are_freed_when_the_next_view_opens() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Capture>(&db, cell), 3);
    assert_eq!(db.storage().edge_counts().2, 1);
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(db.storage().edge_counts().2, 0, "the last view's capture is freed");
    let mut usage = Vec::new();
    db.storage().memory_usage(&mut usage);
    let captured =
        usage.iter().find(|it| it.name == "captured reads").expect("a row for captured reads");
    assert_eq!((captured.rows, captured.row_bytes), (0, 0), "its pages given back");
    cell.set_a(&mut db, 4);
    assert_eq!(*get::<Capture>(&db, cell), 4);
    assert_eq!(*get::<Replay>(&db, cell), 5, "a capture of the new view replays");
}

/// The dependency graph's columns have rows of their own in the memory report: what they hold
/// is in the profile beside every ingredient's.
#[test]
fn the_memory_report_has_the_dependency_graph_s_columns() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Label>(&db, cell), 11);
    let mut usage = Vec::new();
    db.storage().memory_usage(&mut usage);
    let row = |name: &str| {
        *usage.iter().find(|it| it.name == name).unwrap_or_else(|| panic!("no row {name}"))
    };
    let edges = row("dependency edges");
    assert_eq!(edges.rows, 2, "`Label` reads `Parity`, `Parity` reads `a`");
    assert!(edges.row_bytes >= 8, "{edges:?}");
    let nodes = row("dependency nodes");
    assert!(nodes.rows >= 4 && nodes.row_bytes >= 4 * 32, "{nodes:?}");
}

/// Interned numbers, for an intern hit that builds nothing.
struct Numbers;

impl Interned for Numbers {
    type Fields = u64;
    const NAME: &'static str = "Number";
    const FILE: &'static str = file!();

    fn ingredient() -> &'static Ingredient {
        ingredient!(interned::<Numbers>)
    }

    fn ids_of(_: &u64, _: &mut Vec<Id>) {}
}

query!(Probe, u64, |db, key| {
    let target = Cell(db.cells.lock()[0]);
    let (value, used) = blaze_alloc_count::measure(|| *get::<Double>(db, target));
    db.probe.store(used.allocations, Ordering::SeqCst);
    value + key.a(db)
});

#[test]
fn warm_paths_allocate_nothing() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    let other = Cell::new(&db, 0, 0);
    db.cells.lock().push(cell.0);
    get::<Double>(&db, cell);
    let (_, used) = blaze_alloc_count::measure(|| *get::<Double>(&db, cell));
    assert_eq!(used.allocations, 0, "a warm hit");

    for round in 0..3 {
        other.set_a(&mut db, round + 1);
        get::<Probe>(&db, other);
    }
    assert_eq!(db.probe.load(Ordering::SeqCst), 0, "a hit inside a query once its stacks grew");

    intern::<Numbers>(db.storage(), 7);
    let (_, used) = blaze_alloc_count::measure(|| intern::<Numbers>(db.storage(), 7));
    assert_eq!(used.allocations, 0, "an interned hit");

    get::<Label>(&db, cell);
    for round in 0..3 {
        other.set_b(&mut db, round + 1);
        let (_, used) = blaze_alloc_count::measure(|| *get::<Label>(&db, cell));
        if round > 0 {
            assert_eq!(used.allocations, 0, "a verify of an unchanged graph");
        }
    }
}

/// A node of a random query graph: how it combines what it reads.
#[derive(Clone, Copy, Debug)]
enum Op {
    Sum,
    Mod3,
    Max,
}

#[derive(Clone, Copy, Debug)]
enum Dep {
    Input(usize),
    Query(usize),
}

#[derive(Clone, Debug)]
struct GNode {
    op: Op,
    deps: Vec<Dep>,
}

/// Interned node numbers: the graph's keys.
struct Indices;

impl Interned for Indices {
    type Fields = u32;
    const NAME: &'static str = "Index";
    const FILE: &'static str = file!();

    fn ingredient() -> &'static Ingredient {
        ingredient!(interned::<Indices>)
    }

    fn ids_of(_: &u32, _: &mut Vec<Id>) {}
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Node(Id);

impl AsId for Node {
    fn as_id(&self) -> Id {
        self.0
    }
}

impl FromIdIn for Node {
    fn from_id_in(storage: &Storage, id: Id) -> Option<Node> {
        owner_is(storage, id, Indices::ingredient()).then_some(Node(id))
    }
}

fn node(db: &TestDb, at: usize) -> Node {
    Node(intern::<Indices>(db.storage(), at as u32))
}

fn combine(op: Op, values: &[u64]) -> u64 {
    match op {
        Op::Sum => values.iter().fold(0u64, |acc, it| acc.wrapping_add(*it)),
        Op::Mod3 => values.iter().fold(0u64, |acc, it| acc.wrapping_add(*it)) % 3,
        Op::Max => values.iter().copied().max().unwrap_or(0),
    }
}

struct Graph;

impl Spec for Graph {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "Graph";

    fn cache() -> &'static Ingredient {
        ingredient!(function::<Graph>)
    }

    fn compute(db: &TestDb, key: Node) -> u64 {
        let at = *interned_fields::<Indices>(db.storage(), key.0) as usize;
        let spec = &db.graph[at];
        let values: Vec<u64> = spec
            .deps
            .iter()
            .map(|&dep| match dep {
                Dep::Input(input) => {
                    let cell = Cell(db.cells.lock()[input]);
                    cell.a(db)
                }
                Dep::Query(query) => *fetch::<Graph>(db, node(db, query)),
            })
            .collect();
        if let Some(last) = db.snapshots.lock().insert(at, values.clone()) {
            assert_ne!(last, values, "node {at} ran again though nothing it read changed");
        }
        combine(spec.op, &values)
    }
}

/// Every node's value from scratch.
fn evaluate(graph: &[GNode], inputs: &[u64]) -> Vec<u64> {
    let mut values: Vec<u64> = Vec::with_capacity(graph.len());
    for spec in graph {
        let read: Vec<u64> = spec
            .deps
            .iter()
            .map(|&dep| match dep {
                Dep::Input(input) => inputs[input],
                Dep::Query(query) => values[query],
            })
            .collect();
        values.push(combine(spec.op, &read));
    }
    values
}

/// A random acyclic graph: how many inputs it reads, and its nodes, each reading inputs and
/// nodes before it.
fn random_graph(tc: &TestCase) -> (usize, Vec<GNode>) {
    let inputs: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let queries: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let mut graph = Vec::with_capacity(queries);
    for at in 0..queries {
        let op = [Op::Sum, Op::Mod3, Op::Max][tc.draw(gs::integers::<usize>().max_value(2))];
        let count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
        let deps = (0..count)
            .map(|_| {
                if at > 0 && tc.draw(gs::booleans()) {
                    Dep::Query(tc.draw(gs::integers::<usize>().max_value(at - 1)))
                } else {
                    Dep::Input(tc.draw(gs::integers::<usize>().max_value(inputs - 1)))
                }
            })
            .collect();
        graph.push(GNode { op, deps });
    }
    (inputs, graph)
}

#[hegel::test]
fn random_graphs_agree_with_a_run_from_scratch(tc: TestCase) {
    let (inputs, graph) = random_graph(&tc);
    let queries = graph.len();
    let mut values: Vec<u64> = (0..inputs).map(|i| i as u64).collect();
    let mut db = TestDb::made(None, graph);
    let cells: Vec<Cell> = values.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let writes: usize = tc.draw(gs::integers::<usize>().max_value(12));
    for step in 0..=writes {
        if step > 0 {
            let input: usize = tc.draw(gs::integers::<usize>().max_value(inputs - 1));
            values[input] += 1 + tc.draw(gs::integers::<u64>().max_value(3));
            cells[input].set_a(&mut db, values[input]);
        }
        let expected = evaluate(&db.graph, &values);
        let reverse = tc.draw(gs::booleans());
        for at in 0..queries {
            let at = if reverse { queries - 1 - at } else { at };
            assert_eq!(
                *fetch::<Graph>(&db, node(&db, at)),
                expected[at],
                "node {at} at step {step}"
            );
        }
    }
    let log = db.log.lock();
    let mut seen = HashMap::new();
    for &(query, key, view) in log.iter() {
        *seen.entry((query, key, view)).or_insert(0) += 1;
    }
    assert!(seen.values().all(|&count| count == 1), "a query ran twice in one view");
}

/// A random graph's node read as a fixpoint: the max of what it reads, from 0, through cycles.
struct CycGraph;

impl Spec for CycGraph {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "CycGraph";
    const CYCLES: CycleKind = CycleKind::Fixpoint;

    fn cache() -> &'static Ingredient {
        ingredient!(function::<CycGraph>)
    }

    fn compute(db: &TestDb, key: Node) -> u64 {
        let at = *interned_fields::<Indices>(db.storage(), key.0) as usize;
        let mut value = 0;
        for &dep in &db.graph[at].deps {
            value = value.max(match dep {
                Dep::Input(input) => {
                    let cell = Cell(db.cells.lock()[input]);
                    cell.a(db)
                }
                Dep::Query(query) => *fetch::<CycGraph>(db, node(db, query)),
            });
        }
        value
    }

    fn initial(_: &TestDb, _: Node) -> u64 {
        0
    }
}

/// Every node's least fixpoint: each the max of what it reads, from all 0, until none moves.
fn least_fixpoint(graph: &[GNode], inputs: &[u64]) -> Vec<u64> {
    let mut values = vec![0u64; graph.len()];
    loop {
        let mut moved = false;
        for (at, spec) in graph.iter().enumerate() {
            let value = spec
                .deps
                .iter()
                .map(|&dep| match dep {
                    Dep::Input(input) => inputs[input],
                    Dep::Query(query) => values[query],
                })
                .max()
                .unwrap_or(0);
            moved |= value != values[at];
            values[at] = value;
        }
        if !moved {
            return values;
        }
    }
}

/// Converged cycles verified whole agree with a run from scratch: a random graph of max nodes
/// whose reads may point anywhere (so cycles, nested and overlapping), its inputs written up
/// and down at random, every node asked for in a random order after each write.
#[hegel::test]
fn random_cyclic_fixpoints_agree_with_a_run_from_scratch(tc: TestCase) {
    let inputs: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let queries: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let graph: Vec<GNode> = (0..queries)
        .map(|_| {
            let count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
            let deps = (0..count)
                .map(|_| {
                    if tc.draw(gs::booleans()) {
                        Dep::Query(tc.draw(gs::integers::<usize>().max_value(queries - 1)))
                    } else {
                        Dep::Input(tc.draw(gs::integers::<usize>().max_value(inputs - 1)))
                    }
                })
                .collect();
            GNode { op: Op::Max, deps }
        })
        .collect();
    let mut values: Vec<u64> = (0..inputs).map(|i| i as u64).collect();
    let mut db = TestDb::made(None, graph);
    let cells: Vec<Cell> = values.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let writes: usize = tc.draw(gs::integers::<usize>().max_value(10));
    for step in 0..=writes {
        if step > 0 {
            let input: usize = tc.draw(gs::integers::<usize>().max_value(inputs - 1));
            values[input] = tc.draw(gs::integers::<u64>().max_value(9));
            cells[input].set_a(&mut db, values[input]);
        }
        let expected = least_fixpoint(&db.graph, &values);
        let first: usize = tc.draw(gs::integers::<usize>().max_value(queries - 1));
        for at in (0..queries).map(|at| (at + first) % queries) {
            assert_eq!(
                *fetch::<CycGraph>(&db, node(&db, at)),
                expected[at],
                "node {at} at step {step} of {:?}",
                db.graph
            );
            assert!(settled(), "node {at} at step {step} left reads on the thread");
        }
    }
}

/// After one input is edited and every node is asked for again, every memo that ran did so
/// because of a changed read, and following each cause (a memo that changed in this view, by
/// its own cause) reaches the edited field.
#[hegel::test]
fn after_editing_one_input_every_rerun_memos_cause_chain_leads_back_to_it(tc: TestCase) {
    let (inputs, graph) = random_graph(&tc);
    let queries = graph.len();
    let mut values: Vec<u64> = (0..inputs).map(|i| i as u64).collect();
    let mut db = TestDb::made(None, graph);
    let cells: Vec<Cell> = values.iter().map(|&value| Cell::new(&db, value, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let ask = |db: &TestDb| {
        for at in 0..queries {
            fetch::<Graph>(db, node(db, at));
        }
    };
    ask(&db);
    let first = table(&db);
    assert_eq!(first.len(), queries);
    assert!(first.iter().all(|it| (it.outcome, it.cause) == (Outcome::Reran, Some(Cause::New))));
    let writes: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    for step in 0..writes {
        let input: usize = tc.draw(gs::integers::<usize>().max_value(inputs - 1));
        values[input] += 1 + tc.draw(gs::integers::<u64>().max_value(3));
        cells[input].set_a(&mut db, values[input]);
        ask(&db);
        let edited = field_node::<Cells>(db.storage(), cells[input].0, 0);
        let view = table(&db);
        assert_eq!(view.len(), queries, "every node asked for is in the table at step {step}");
        let by_node: HashMap<u32, &Provenance> = view.iter().map(|it| (it.node, it)).collect();
        for entry in &view {
            let mut cause = entry.cause;
            while let Some(why) = cause {
                let Cause::Changed(at) = why else {
                    panic!("step {step}: {entry:?}'s chain reaches a run for {why:?}, not the edit")
                };
                if at == edited {
                    break;
                }
                let Some(dep) = by_node.get(&at) else {
                    panic!(
                        "step {step}: {entry:?}'s chain reaches {:?}, neither the edited field \
                         nor a memo of this view",
                        db.storage().node_read(at)
                    )
                };
                assert_eq!(dep.outcome, Outcome::Reran, "step {step}: a cause changed");
                cause = dep.cause;
                assert!(cause.is_some(), "step {step}: {dep:?} reran without a cause");
            }
        }
    }
}
