//! The validity census ([`crate::db::Validity`]): each way the engine proves a row of an
//! earlier view unchanged is counted under its own name. One tiny database a proof, each
//! built so that exactly that proof answers first for one row, on one thread; the counts are
//! read for this module's own queries, never taken (a take is the process's).

use super::{
    super::{
        SealKind, Validity,
        counts::{self, probe::MISWIRED},
        plumbing::{input_seal_group, walk_changed_set},
    },
    *,
};

query!(CensusLeaf, u64, |db, key| key.a(db) % 2);
query!(CensusOver, u64, |db, key| get::<CensusLeaf>(db, key) + 1);
query!(CensusTop, u64, |db, key| {
    let wide = Cell(db.cells.lock()[0]);
    get::<CensusLeaf>(db, key) + wide.a(db)
});

/// One census at a time: the counts are by query, and both tests here run the same queries.
static CENSUS: Lock<()> = Lock::new("query.test.db.validity.census", ());

/// Writes past the written-field history ([`crate::db::storage`]'s 64 views): a row verified
/// before them is proved by no summary counted from its own view.
const PAST_HISTORY: u64 = 65;

const SEAL: usize = 0;
const DURABILITY: usize = 1;
const SUMMARY: usize = 2;
const FLOOR_DURABILITY: usize = 3;
const FLOOR_SUMMARY: usize = 4;
const WALK_END: usize = 5;
const REACH: usize = 6;
const FIRST: usize = 7;
const EDGE_WALKED: usize = 8;
const EDGE_GAVE_UP: usize = 9;
const EDGE_CLAIMED: usize = 10;
const EDGES_COMPARED: usize = 11;

/// A census as a row, in the order of the constants above.
fn row(of: Validity) -> [u64; 12] {
    let Validity {
        proved_seal,
        proved_durability,
        proved_summary,
        proved_floor_durability,
        proved_floor_summary,
        proved_walk_end,
        proved_reach,
        proved_first,
        edge_walked,
        edge_gave_up,
        edge_claimed,
        edges_compared,
    } = of;
    [
        proved_seal,
        proved_durability,
        proved_summary,
        proved_floor_durability,
        proved_floor_summary,
        proved_walk_end,
        proved_reach,
        proved_first,
        edge_walked,
        edge_gave_up,
        edge_claimed,
        edges_compared,
    ]
}

/// A row of zeros but for `counts`.
fn only(counts: &[(usize, u64)]) -> [u64; 12] {
    let mut row = [0; 12];
    for &(at, count) in counts {
        row[at] = count;
    }
    row
}

/// What `scenario` added to the census of `Q`'s rows.
fn gained<Q: Spec>(scenario: impl FnOnce()) -> [u64; 12] {
    let index = Q::cache().index();
    let before = row(counts::validity_now(index));
    scenario();
    let after = row(counts::validity_now(index));
    std::array::from_fn(|at| after[at] - before[at])
}

/// `count` writes of `cell`'s field `a`, each of another value: `count` views.
fn write_many(db: &mut TestDb, cell: Cell, count: u64) {
    for round in 0..count {
        cell.set_a(db, 1000 + round);
    }
}

/// A library's row behind a seal that holds, asked after an unrelated write.
fn by_seal() {
    let mut db = TestDb::new();
    let group = db.storage_mut().seal_group("census", SealKind::Library, &[]);
    let cell = Cell::new(&db, 3, 0);
    input_seal_group::<Cells>(db.storage_mut(), cell.0, group);
    let other = apart(&db, cell);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    let storage = db.storage_mut();
    storage.pack_reverse();
    storage.settle_changed_set();
    assert_eq!(storage.seal_quiet_groups().sealed, 1, "the library is sealed by its first pass");
    other.set_a(&mut db, 1);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    assert_eq!(db.runs("CensusLeaf"), 1);
}

/// A row of a `HIGH` input, asked after a `LOW` write.
fn by_durability() {
    let mut db = TestDb::new();
    let library = Cell::of(&db, 3, 0, Durability::HIGH);
    let file = Cell::new(&db, 0, 0);
    assert_eq!(*get::<CensusLeaf>(&db, library), 1);
    file.set_a(&mut db, 1);
    assert_eq!(*get::<CensusLeaf>(&db, library), 1);
    assert_eq!(db.runs("CensusLeaf"), 1);
}

/// A row of a `LOW` input, asked after a write of a `LOW` field whose bits its summary lacks.
fn by_summary() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    let other = apart(&db, cell);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    other.set_a(&mut db, 1);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    assert_eq!(db.runs("CensusLeaf"), 1);
}

/// A row of a `HIGH` input, verified before more `HIGH` writes than the history holds, a
/// settle that left nothing (the floor), and a `LOW` write: its own view proves nothing, and
/// from the floor no `HIGH` input was written.
fn by_floor_durability() {
    let mut db = TestDb::new();
    let library = Cell::of(&db, 3, 0, Durability::HIGH);
    let other = Cell::of(&db, 0, 0, Durability::HIGH);
    let file = Cell::new(&db, 0, 0);
    assert_eq!(*get::<CensusLeaf>(&db, library), 1);
    write_many(&mut db, other, PAST_HISTORY);
    assert_eq!(db.storage_mut().settle_changed_set().left, 0, "nothing reads what was written");
    file.set_a(&mut db, 1);
    assert_eq!(*get::<CensusLeaf>(&db, library), 1);
    assert_eq!(db.runs("CensusLeaf"), 1);
}

/// A row of a `LOW` input, verified before more writes than the history holds, a settle that
/// left nothing (the floor), and one more write of a field its summary lacks: its own view
/// proves nothing, and from the floor its summary does.
fn by_floor_summary() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    let other = apart(&db, cell);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    write_many(&mut db, other, PAST_HISTORY);
    assert_eq!(db.storage_mut().settle_changed_set().left, 0, "nothing reads what was written");
    other.set_a(&mut db, 1);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    assert_eq!(db.runs("CensusLeaf"), 1);
}

/// A row asked after a walk of the changed set ended in this view without reaching it: the
/// fetch's first question (`frontier::clean`).
fn by_walk_end() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    let other = apart(&db, cell);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    other.set_a(&mut db, 1);
    walk_changed_set(&db);
    assert_eq!(*get::<CensusLeaf>(&db, cell), 1);
    assert_eq!(db.runs("CensusLeaf"), 1);
}

/// A row read by the one row a walk reaches, verified more writes back than the history
/// holds, with no floor: the walk verifies the reader, whose first edge is the row, outside
/// the walk's reach.
fn by_reach() {
    let mut db = TestDb::new();
    let wide = Cell::new(&db, 0, 0);
    db.cells.lock().push(wide.0);
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<CensusTop>(&db, cell), 1);
    write_many(&mut db, wide, PAST_HISTORY);
    walk_changed_set(&db);
    assert_eq!(*get::<CensusTop>(&db, cell), 1 + 1000 + PAST_HISTORY - 1);
    assert_eq!((db.runs("CensusLeaf"), db.runs("CensusTop")), (1, 2));
}

/// A row over a row over an input, asked after more unrelated writes than the history holds,
/// with no floor and no walk: the claim-free walk proves both by their edges.
fn by_edges_walked() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    let other = Cell::new(&db, 0, 0);
    assert_eq!(*get::<CensusOver>(&db, cell), 2);
    write_many(&mut db, other, PAST_HISTORY);
    assert_eq!(*get::<CensusOver>(&db, cell), 2);
    assert_eq!((db.runs("CensusLeaf"), db.runs("CensusOver")), (1, 1));
}

/// The same two rows after a write of the input that leaves the leaf's value: the walk gives
/// up at the written field, the claimed path runs the leaf (backdated) and proves the row
/// over it by its edge.
fn by_edges_claimed() {
    let mut db = TestDb::new();
    let cell = Cell::new(&db, 3, 0);
    assert_eq!(*get::<CensusOver>(&db, cell), 2);
    cell.set_a(&mut db, 5);
    assert_eq!(*get::<CensusOver>(&db, cell), 2);
    assert_eq!((db.runs("CensusLeaf"), db.runs("CensusOver")), (2, 1));
}

/// One finding: the scenario, the query whose rows are counted, what it added to that
/// query's census and what it should have.
type Finding = (&'static str, &'static str, [u64; 12], [u64; 12]);

/// Every scenario's census, each beside what it should be.
fn census() -> Vec<Finding> {
    let leaf = |name: &'static str, scenario: fn(), expected: &[(usize, u64)]| -> Finding {
        (name, "CensusLeaf", gained::<CensusLeaf>(scenario), only(expected))
    };
    let over = |name: &'static str, scenario: fn(), expected: &[(usize, u64)]| -> Finding {
        (name, "CensusOver", gained::<CensusOver>(scenario), only(expected))
    };
    vec![
        leaf("seal", by_seal, &[(SEAL, 1)]),
        leaf("durability", by_durability, &[(DURABILITY, 1)]),
        leaf("summary", by_summary, &[(SUMMARY, 1)]),
        leaf("floor, durability", by_floor_durability, &[(FLOOR_DURABILITY, 1)]),
        leaf("floor, summary", by_floor_summary, &[(FLOOR_SUMMARY, 1)]),
        leaf("walk end", by_walk_end, &[(WALK_END, 1), (FIRST, 1)]),
        leaf("reach", by_reach, &[(REACH, 1)]),
        leaf("edges walked", by_edges_walked, &[(EDGE_WALKED, 1), (EDGES_COMPARED, 1)]),
        over("edges walked", by_edges_walked, &[(EDGE_WALKED, 1), (EDGES_COMPARED, 1)]),
        // The leaf is on the stack of two walks that give up (its reader's, then its own),
        // then its one edge is compared under its flight: three edges compared in all.
        leaf(
            "edges claimed",
            by_edges_claimed,
            &[(EDGE_GAVE_UP, 2), (EDGE_CLAIMED, 1), (EDGES_COMPARED, 3)],
        ),
        over(
            "edges claimed",
            by_edges_claimed,
            &[(EDGE_GAVE_UP, 1), (EDGE_CLAIMED, 1), (EDGES_COMPARED, 2)],
        ),
    ]
}

/// The invariant: a row proved unchanged is counted once, under the proof that answered
/// first in the engine's order (seal, durability, summary, the floor by each, a walk's end,
/// a walk's reach), or under the path that compared its edges with the edges it compared,
/// and under nothing else.
#[test]
fn each_proof_of_a_row_is_counted_under_its_own_name() {
    let _one = CENSUS.lock();
    for (scenario, query, got, expected) in census() {
        assert_eq!(got, expected, "{scenario}: the census of {query}");
    }
}

/// The control of [`each_proof_of_a_row_is_counted_under_its_own_name`] (rule
/// test-the-cause): with the durability proof's counter wired to the summary's
/// ([`MISWIRED`]), the same scenarios say so, in the durability scenario and in no other.
#[test]
fn the_census_catches_a_counter_wired_to_the_wrong_branch() {
    let _one = CENSUS.lock();
    MISWIRED.set(true);
    let found = census();
    MISWIRED.set(false);
    for (scenario, query, got, expected) in found {
        if scenario == "durability" {
            assert_eq!(got, only(&[(SUMMARY, 1)]), "the planted wire: the census of {query}");
            assert_ne!(got, expected);
        } else {
            assert_eq!(got, expected, "{scenario}: the census of {query}");
        }
    }
}
