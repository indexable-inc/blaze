//! [`unrecorded`] and [`recorded_if`]: a guard whose reads land in no edge of the running query
//! and in no capture open on it, with what they folded into its frame put back; the goal
//! store's offer runs in one (`hir_ty::next_solver::goal_store`).
#![allow(clippy::disallowed_methods, reason = "the guard's own tests call it")]

use std::ops::ControlFlow;

use super::{
    super::{
        capture::probe::UNRECORDED_KEEPS, local::DEDUP_FROM, recorded_if, unrecorded,
    },
    *,
};

/// The shared table's slot of the goal's answer and of a lookup's reads: no cell's ID.
const SLOT: u32 = u32::MAX;

/// What a capture read, in read order, by key and kind: compared whole, never by count.
fn listed(db: &TestDb, reads: Reads) -> Vec<(DatabaseKeyIndex, ReadKind)> {
    let mut out = Vec::new();
    let _: Option<()> = reads.for_each_read(db.storage(), |read| {
        out.push((read.key, read.kind));
        ControlFlow::Continue(())
    });
    out
}

/// A read of `cell`'s field `field`, as [`listed`] lists it.
fn field(cell: Cell, field: u32) -> (DatabaseKeyIndex, ReadKind) {
    (DatabaseKeyIndex::new(Cells::ingredient().index(), cell.0), ReadKind::InputField(field))
}

/// The cells a test shares with its queries: `cells[0]` is what a goal reads, `cells[1]` what
/// its offer's bookkeeping reads.
fn cells(db: &TestDb) -> (Cell, Cell) {
    let cells = db.cells.lock();
    (Cell(cells[0]), Cell(cells[1]))
}

/// A body that solves its goal itself: the goal's capture (`cells[0]`'s `a`), kept in the
/// shared table as the goal table keeps an answer, then the offer's bookkeeping, which reads
/// `cells[1]` (and captures inside itself, as a fact's value that solves does), unrecorded.
query!(Solves, u64, |db, key| {
    let (goal, bookkeeping) = cells(db);
    let (value, reads) = capture_reads(db, || {
        let (answer, goal_reads) = capture_reads(db, || goal.a(db));
        db.shared.lock().insert(SLOT, (answer, goal_reads.expect("a plain read is shareable")));
        unrecorded(db, || {
            let (_, inner) = capture_reads(db, || bookkeeping.b(db) + bookkeeping.a(db));
            assert!(inner.is_some(), "a capture inside the guard is whole");
        });
        answer + key.a(db)
    });
    db.shared.lock().insert(key.0.as_u32(), (value, reads.expect("plain reads are shareable")));
    value
});

/// A body that takes the same goal from the table: it replays the goal's reads.
query!(Hits, u64, |db, key| {
    let (value, reads) = capture_reads(db, || {
        let (answer, goal_reads) = db.shared.lock()[&SLOT];
        replay_reads(db, goal_reads);
        answer + key.a(db)
    });
    db.shared.lock().insert(key.0.as_u32(), (value, reads.expect("plain reads are shareable")));
    value
});

fn made() -> (TestDb, Cell, Cell) {
    let db = TestDb::new();
    let goal = Cell::new(&db, 3, 0);
    let bookkeeping = Cell::new(&db, 5, 7);
    db.cells.lock().extend([goal.0, bookkeeping.0]);
    let (one, two) = (Cell::new(&db, 10, 0), Cell::new(&db, 20, 0));
    (db, one, two)
}

/// The trace of the body asked first and of the one asked second, by their own keys' reads
/// left out: what each one's capture holds besides its own cell.
fn traces(
    db: &TestDb,
    solver: Cell,
    hitter: Cell,
) -> (Vec<(DatabaseKeyIndex, ReadKind)>, Vec<(DatabaseKeyIndex, ReadKind)>) {
    let shared = db.shared.lock();
    (listed(db, shared[&solver.0.as_u32()].1), listed(db, shared[&hitter.0.as_u32()].1))
}

/// The invariant: a body's captured trace is the same whether it solved a goal (and offered
/// it, its bookkeeping unrecorded) or took the goal from the table another body filled: both
/// hold the goal's read then their own, byte for byte, and neither the bookkeeping's reads.
/// The run's own edges leave them out too: a write of what only the bookkeeping read reruns
/// no body. Control: the planted guard that puts nothing back ([`UNRECORDED_KEEPS`]) lists the
/// bookkeeping's reads in the solving body's trace, and the traces differ.
#[test]
fn a_trace_is_the_same_whether_its_body_offered_the_goal_or_hit_it() {
    for planted in [false, true] {
        UNRECORDED_KEEPS.set(planted);
        let (mut db, one, two) = made();
        assert_eq!(*get::<Solves>(&db, one), 13);
        assert_eq!(*get::<Hits>(&db, two), 23);
        let (goal, bookkeeping) = cells(&db);
        let (solved, hit) = traces(&db, one, two);
        assert_eq!(hit, vec![field(goal, 0), field(two, 0)]);
        if planted {
            assert_ne!(solved, vec![field(goal, 0), field(one, 0)], "the planted guard leaks");
            assert!(solved.contains(&field(bookkeeping, 1)), "{solved:?}");
            UNRECORDED_KEEPS.set(false);
            continue;
        }
        assert_eq!(solved, vec![field(goal, 0), field(one, 0)], "the bookkeeping is no read");
        bookkeeping.set_b(&mut db, 8);
        bookkeeping.set_a(&mut db, 6);
        assert_eq!(*get::<Solves>(&db, one), 13);
        assert_eq!(db.runs("Solves"), 1, "a write of what only the guard read reruns nothing");
        goal.set_a(&mut db, 4);
        assert_eq!(*get::<Solves>(&db, one), 14);
        assert_eq!(db.runs("Solves"), 2, "the goal's read is the run's edge");
    }
}

/// A run that read `a` and `c` and `a` again (a repeat below the guard), then many reads of
/// `b` and `d` in turn inside the guard: enough that its frame drops its repeats there.
query!(DedupsInside, u64, |db, key| {
    let (goal, bookkeeping) = cells(db);
    let (value, reads) = capture_reads(db, || {
        let value = goal.a(db) + key.a(db) + goal.a(db);
        unrecorded(db, || {
            for _ in 0..3 * DEDUP_FROM {
                bookkeeping.b(db);
                bookkeeping.a(db);
            }
        });
        value
    });
    db.shared.lock().insert(key.0.as_u32(), (value, reads.expect("plain reads are shareable")));
    value
});

/// The invariant: a dedup inside the guard moves none of the reads made before it, so ending
/// the guard drops exactly its own: the capture holds `a` of the goal and of the key, each
/// once, and nothing the guard read. (Were the dedup floor left where it was, the drop would
/// pull the repeat of the goal's read out from under the guard's start and keep one of the
/// guard's reads in its place.)
#[test]
fn a_dedup_inside_the_guard_keeps_the_reads_before_it_in_place() {
    let (mut db, one, _) = made();
    let (goal, bookkeeping) = cells(&db);
    assert_eq!(*get::<DedupsInside>(&db, one), 16);
    let reads = db.shared.lock()[&one.0.as_u32()].1;
    assert_eq!(listed(&db, reads), vec![field(goal, 0), field(one, 0)]);
    bookkeeping.set_b(&mut db, 1);
    assert_eq!(*get::<DedupsInside>(&db, one), 16);
    assert_eq!(db.runs("DedupsInside"), 1, "no read of the guard's is an edge");
}

/// A lookup: `cells[1]`'s `b` read under [`recorded_if`], kept when it is odd (a hit, whose
/// reads are its answer's key), dropped when it is even (a miss).
query!(LooksUp, u64, |db, _key| {
    let (_, lookup) = cells(db);
    let ((), reads) = capture_reads(db, || {
        recorded_if(db, || lookup.b(db), |found| found % 2 == 1);
    });
    db.shared.lock().insert(SLOT, (0, reads.expect("plain reads are shareable")));
    0
});

/// The invariant: [`recorded_if`] keeps its reads, in the run and in the capture around it,
/// exactly when `keep` says so: a hit's are its answer's key, a miss's are nothing.
#[test]
fn recorded_if_keeps_a_hit_s_reads_and_drops_a_miss_s() {
    let (mut db, one, _) = made();
    let (_, lookup) = cells(&db);
    assert_eq!(*get::<LooksUp>(&db, one), 0);
    assert_eq!(listed(&db, db.shared.lock()[&SLOT].1), vec![field(lookup, 1)], "7 is a hit");
    lookup.set_b(&mut db, 8);
    assert_eq!(*get::<LooksUp>(&db, one), 0);
    assert_eq!(db.runs("LooksUp"), 2, "the hit's read was the run's edge");
    assert_eq!(listed(&db, db.shared.lock()[&SLOT].1), vec![], "8 is a miss");
    lookup.set_b(&mut db, 9);
    assert_eq!(*get::<LooksUp>(&db, one), 0);
    assert_eq!(db.runs("LooksUp"), 2, "the miss's read is no edge");
}
