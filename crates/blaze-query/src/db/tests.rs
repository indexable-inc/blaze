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

use hegel::{TestCase, generators as gs};
use parking_lot::Mutex;

use super::{
    AsId, Cancelled, Cause, Cycle, Database, DatabaseKeyIndex, Durability, Event, FromIdIn, Hook,
    Id, Outcome, Provenance, ReadKind, Reads, Setter, Storage, attach, capture_reads,
    input::field_node,
    owner_is,
    plumbing::{
        CycleKind, Function, Ingredient, Input, Interned, Tag, Tracked, Vtable, evict_values,
        evict_values_of, fetch, input_fields, input_new, input_new_singleton, input_read,
        input_set, input_singleton, intern, intern_by, interned_arguments, interned_fields,
        set_lru_capacity, tracked_fields, tracked_new, value_hash,
    },
    replay_reads, report_untracked_read,
    row::VIEW_MASK,
    storage::{Summary, input_bits},
    unshareable_reads, unwind_if_cancelled, with_attached,
};

/// The database of every test: the storage, a log of runs, and what particular tests share
/// with their queries.
pub struct TestDb {
    storage: Storage,
    /// `(query, key, view)` of every run.
    log: Mutex<Vec<(&'static str, u32, u32)>>,
    shared: Mutex<HashMap<u32, (u64, Reads)>>,
    gate: Barrier,
    armed: [AtomicBool; 2],
    drops: Arc<AtomicUsize>,
    probe: AtomicU64,
    cells: Mutex<Vec<Id>>,
    graph: Vec<GNode>,
    /// The dependency values of each graph node's last run.
    snapshots: Mutex<HashMap<usize, Vec<u64>>>,
}

impl TestDb {
    fn new() -> TestDb {
        TestDb::made(None, Vec::new())
    }

    fn made(hook: Option<Hook>, graph: Vec<GNode>) -> TestDb {
        TestDb {
            storage: Storage::new(hook),
            log: Mutex::new(Vec::new()),
            shared: Mutex::new(HashMap::new()),
            gate: Barrier::new(2),
            armed: [AtomicBool::new(false), AtomicBool::new(false)],
            drops: Arc::new(AtomicUsize::new(0)),
            probe: AtomicU64::new(u64::MAX),
            cells: Mutex::new(Vec::new()),
            graph,
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    fn ran(&self, query: &'static str, key: Id) {
        let view = self.storage.view_bits();
        self.log.lock().push((query, key.as_u32(), view));
    }

    fn runs(&self, query: &str) -> usize {
        self.log.lock().iter().filter(|it| it.0 == query).count()
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
    const CAP: usize = 0;
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
    const LRU: usize = S::CAP;
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
    report_untracked_read(db);
    key.b(db)
});

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

fn recorded() -> (Arc<Mutex<Vec<String>>>, Hook) {
    let events = Arc::new(Mutex::new(Vec::new()));
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

#[test]
fn a_key_the_database_never_gave_is_refused_naming_the_query_and_the_database_still_drops() {
    let db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<Double>(&db, cell), 6);
    for forged in [Id::from_u32(5), Id::from_u32(Id::MAX_U32), Id::from_u32(u32::MAX)] {
        let payload = catch_unwind(AssertUnwindSafe(|| *get::<Double>(&db, Cell(forged))))
            .expect_err("a key of no owned page");
        let message = message(&*payload);
        assert!(message.contains("`Double`") && message.contains("never gave"), "{message}");
    }
    assert_eq!(*get::<Double>(&db, cell), 6);
    drop(db);
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

/// `(executed, verified, reran)` of the two counted queries since the last take; other tests
/// count other queries, and only this one takes.
fn counted(db: &TestDb) -> [(u64, u64, u64); 2] {
    let mut found = [(0, 0, 0); 2];
    db.storage().take_counts(|name, executed, verified, reran| {
        let at = match name {
            "CountedTens" => 0,
            "CountedReader" => 1,
            _ => return,
        };
        found[at] = (executed, verified, reran);
    });
    found
}

/// What observe's `reexecution_per_query_kind` reads: a run counts as executed, a row proved
/// unchanged as verified, and only a second run of a row in one view (its value evicted and
/// asked for again) as reran; a run in a later view is not one, nor is a hit anything.
#[test]
fn each_run_verify_and_rerun_in_one_view_is_counted_per_query() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 11, 0);
    counted(&db);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    assert_eq!(counted(&db), [(1, 0, 0), (1, 0, 0)], "a first run each, and a hit is nothing");

    cell.set_a(&mut db, 12);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    assert_eq!(counted(&db), [(1, 0, 0), (0, 1, 0)], "tens ran equal: its reader is verified");

    assert_eq!(evict_values_of::<CountedTens>(&mut db, &[cell]), 1);
    assert_eq!(*get::<CountedTens>(&db, cell), 1);
    assert_eq!(counted(&db), [(1, 0, 1), (0, 0, 0)], "run again in the view it ran in");

    cell.set_b(&mut db, 1);
    assert_eq!(evict_values_of::<CountedTens>(&mut db, &[cell]), 1);
    assert_eq!(*get::<CountedReader>(&db, cell), 2);
    assert_eq!(*get::<CountedTens>(&db, cell), 1);
    let [(executed, verified, reran), reader] = counted(&db);
    assert_eq!((executed, reran), (1, 0), "evicted in an earlier view: a run, not a rerun");
    assert_eq!(verified, 1, "proved unchanged before it ran for its value");
    assert_eq!(reader, (0, 1, 0));
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

query!(
    Capped,
    u64,
    |db, key| key.a(db);
    const CAP: usize = 2;
    const EVICT: bool = true;
);

fn values_held(db: &TestDb, name: &str) -> u64 {
    let mut usage = Vec::new();
    db.storage().memory_usage(&mut usage);
    usage.iter().find(|it| it.name == name).map_or(0, |it| it.value_bytes / 8)
}

#[test]
fn a_new_view_evicts_past_the_cap_and_keep_every_value_stops_it() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..4).map(|i| Cell::new(&db, i, 0)).collect();
    for &cell in &cells {
        get::<Capped>(&db, cell);
    }
    assert_eq!(values_held(&db, "Capped"), 4);
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "Capped"), 2);
    db.storage_mut().keep_every_value(true);
    for (i, &cell) in cells.iter().enumerate() {
        assert_eq!(*get::<Capped>(&db, cell), i as u64);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "Capped"), 4);
    assert_eq!(db.runs("Capped"), 6, "two evicted values ran again");
}

query!(
    CapLater,
    u64,
    |db, key| key.a(db);
    const CAP: usize = 2;
    const EVICT: bool = true;
);

#[test]
fn a_cap_set_at_runtime_applies_at_the_next_view_and_zero_is_none() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..6).map(|i| Cell::new(&db, i, 0)).collect();
    set_lru_capacity::<CapLater>(&mut db, 4);
    for &cell in &cells {
        get::<CapLater>(&db, cell);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "CapLater"), 4);
    set_lru_capacity::<CapLater>(&mut db, 0);
    for (i, &cell) in cells.iter().enumerate() {
        assert_eq!(*get::<CapLater>(&db, cell), i as u64);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "CapLater"), 6, "no cap: nothing evicted");
    set_lru_capacity::<Tens>(&mut db, 1);
    for &cell in &cells[..3] {
        get::<Tens>(&db, cell);
    }
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_eq!(values_held(&db, "Tens"), 1, "a function declared `lru = 0` is swept once capped");
}

#[test]
fn a_cap_on_a_function_without_lru_panics() {
    let mut db = TestDb::new();
    let payload = catch_unwind(AssertUnwindSafe(|| set_lru_capacity::<Double>(&mut db, 3)))
        .expect_err("Double has no lru");
    assert!(message(&*payload).contains("has no `lru`"));
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
    let events = Arc::new(Mutex::new(Vec::new()));
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

query!(Wide, u64, |db, _key| {
    let cells = db.cells.lock().clone();
    cells.iter().map(|&it| Cell(it).a(db)).sum()
});

#[test]
fn compaction_keeps_every_live_span() {
    let mut db = TestDb::new();
    let cells: Vec<Cell> = (0..128).map(|i| Cell::new(&db, i, 0)).collect();
    db.cells.lock().extend(cells.iter().map(|it| it.0));
    let key = cells[0];
    let mut expected: u64 = (0..128).sum();
    assert_eq!(*get::<Wide>(&db, key), expected);
    for round in 0..1200 {
        cells[1].set_a(&mut db, round + 2);
        expected = (0..128).sum::<u64>() - 1 + round + 2;
        assert_eq!(*get::<Wide>(&db, key), expected);
    }
    get::<Double>(&db, cells[5]);
    db.storage_mut().compact();
    assert_eq!(*get::<Wide>(&db, key), expected);
    assert_eq!(*get::<Double>(&db, cells[5]), 10);
    cells[2].set_a(&mut db, 100);
    assert_eq!(*get::<Wide>(&db, key), expected + 98);
    assert_eq!(*get::<Double>(&db, cells[5]), 10);
    assert_eq!(db.runs("Double"), 1);
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
