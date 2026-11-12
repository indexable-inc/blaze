//! A fixpoint over nested cycles whose inner head hears that a row above it moved: the five
//! rows a random draw of `random_cyclic_fixpoints_agree_with_a_run_from_scratch` found on
//! 2026-11-12 ("node 4 at step 1 ...: left 0, right 1"), built by hand.
//!
//! Each row is the max of what it reads, from 0:
//!
//! - row 0 reads row 3;
//! - row 1 reads rows 3 and 4;
//! - row 2 reads the input;
//! - row 3 reads the input and rows 0 and 1;
//! - row 4 reads row 1.
//!
//! Asked from row 0 the stack is 0, 3, 1, 4, and every one of the first three is read again
//! from above it: 0 is the outermost head, 3 and 1 are inner heads, 4 is a member. With the
//! input at 1:
//!
//! 1. Round 1 ends with 0 = 1, 3 = 1, 1 = 0, 4 = 0. Row 0 moved (0 to 1), so round 2 runs; the
//!    inner heads keep their values to be read on the stack, row 4 is emptied.
//! 2. Round 2: row 3 runs and reads row 1, which runs and reads row 3 on the stack (1) and row
//!    4, which runs and reads row 1 ON THE STACK: its value of round 1, 0. So 4 = 0. Row 1
//!    then ends with max(1, 0) = 1: it moved, after row 4 read it. Row 1's head is row 3 (the
//!    lowest it read on the stack), and row 3 ends a member of row 0's cycle with 1, as before.
//! 3. Row 0 ends with 1, as before. With the bug, row 1's word that it moved had gone to row
//!    3's frame and no further: row 0 saw a round in which nothing moved and made every row
//!    final, row 4 with 0. Fixed, row 3 hands the word on, round 3 runs, and row 4 reads 1.
//!
//! No write is needed: a storage made with the input at 1 answers the same. After a write (the
//! reproducer: made at 0, every row asked, then written to 1) the same rounds run, each row's
//! verify having turned into its run.

//!
//! The same rounds show what each row's `cycle_recover` is told of the round ([`Told`]): rows 3
//! and 1 are kept from one round to the next and run again on new frames, which start at the
//! round of the head that kept them, so row 1, whose head is row 3, is told 1, 2, 3 as row 0
//! is. Before, it was told its head's frame's round: 1 in every round.

use std::cell::RefCell;

use super::{
    super::function::probe, Cell, CycGraph, Cycle, CycleKind, Database, Dep, GNode, Indices,
    Ingredient, Node, Op, Spec, TestDb, Vtable, fetch, interned_fields, least_fixpoint, node,
    settled,
};

/// The five rows.
fn graph() -> Vec<GNode> {
    let max = |deps: &[Dep]| GNode { op: Op::Max, deps: deps.to_vec() };
    vec![
        max(&[Dep::Query(3)]),
        max(&[Dep::Query(3), Dep::Query(4)]),
        max(&[Dep::Input(0)]),
        max(&[Dep::Input(0), Dep::Query(0), Dep::Query(1)]),
        max(&[Dep::Query(1)]),
    ]
}

/// What the engine answers for each row with the input at 1, the rows asked in order from row
/// `first` round. `write`: the storage is made with the input at 0, every row asked, and the
/// input then written to 1 (the reproducer's shape); else it is made with the input at 1.
/// `plant`: with the planted bug ([`probe::INNER_HEADS_FORGET`]).
fn answers(first: usize, write: bool, plant: bool) -> Vec<u64> {
    let mut db = TestDb::made(None, graph());
    let cell = Cell::new(&db, u64::from(!write), 0);
    db.cells.lock().push(cell.0);
    let rows = db.graph.len();
    let ask = |db: &TestDb| -> Vec<u64> {
        let mut values = vec![0; rows];
        for at in (0..rows).map(|at| (at + first) % rows) {
            values[at] = *fetch::<CycGraph>(db, node(db, at));
            assert!(settled(), "row {at} left reads on the thread");
        }
        values
    };
    probe::INNER_HEADS_FORGET.with(|it| it.set(plant));
    if write {
        assert_eq!(ask(&db), [0; 5], "every row is 0 while the input is");
        cell.set_a(&mut db, 1);
    }
    let values = ask(&db);
    probe::INNER_HEADS_FORGET.with(|it| it.set(false));
    values
}

/// The invariant: a cycle's rows are made final only by a round in which every value a run
/// read from a row of the cycle on the stack is the value that row ended the round with. Every
/// row of the cycle that ends a round with another value than was read from it says so, and
/// the word reaches the outermost head through every head between them, so the answers are the
/// fixpoint a run from scratch reaches, whichever row is asked first and whether the rows were
/// made in this view or verified into it after a write.
#[test]
fn a_cycle_ends_only_after_a_round_in_which_no_row_read_on_the_stack_moved() {
    let expected = least_fixpoint(&graph(), &[1]);
    assert_eq!(expected, [1; 5]);
    for first in 0..expected.len() {
        for write in [false, true] {
            assert_eq!(
                answers(first, write, false),
                expected,
                "asked from row {first}, the input written: {write}"
            );
        }
    }
}

/// The control: with the word stopped at the inner head's frame, as it was, the outermost head
/// ends the cycle a round early and row 4 keeps the 0 it read from row 1 on the stack: the
/// property test's failure, from scratch and after the write alike.
#[test]
fn the_oracle_catches_an_inner_head_that_keeps_a_moved_row_to_itself() {
    for write in [false, true] {
        assert_eq!(
            answers(0, write, true),
            [1, 1, 1, 1, 0],
            "asked from row 0, the input written: {write}"
        );
    }
}

std::thread_local! {
    /// The key and the round of each `cycle_recover` of [`Told`] on this thread, in order.
    static TOLD: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
}

/// [`CycGraph`], its `cycle_recover` noting the round it is told. It reads nothing there: a
/// recovery's reads would be its run's.
struct Told;

impl Spec for Told {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "Told";
    const CYCLES: CycleKind = CycleKind::Fixpoint;

    fn cache() -> &'static Ingredient {
        ingredient!(function::<Told>)
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
                Dep::Query(query) => *fetch::<Told>(db, node(db, query)),
            });
        }
        value
    }

    fn initial(_: &TestDb, _: Node) -> u64 {
        0
    }

    fn recover(_: &TestDb, cycle: &Cycle<'_>, _: &u64, new: u64, key: Node) -> u64 {
        TOLD.with(|it| it.borrow_mut().push((key.0.as_u32(), cycle.iteration())));
        new
    }
}

/// The rounds each row's `cycle_recover` was told, in order, in a storage made with the input
/// at 1 and asked for row 0: three rounds (above). `plant`: with the planted bug
/// ([`probe::KEPT_HEADS_START_AGAIN`]).
fn told(plant: bool) -> Vec<Vec<u32>> {
    let db = TestDb::made(None, graph());
    let cell = Cell::new(&db, 1, 0);
    db.cells.lock().push(cell.0);
    TOLD.with(|it| it.borrow_mut().clear());
    probe::KEPT_HEADS_START_AGAIN.with(|it| it.set(plant));
    let head = *fetch::<Told>(&db, node(&db, 0));
    probe::KEPT_HEADS_START_AGAIN.with(|it| it.set(false));
    assert_eq!((head, *fetch::<Told>(&db, node(&db, 4))), (1, 1), "the fixpoint, in three rounds");
    assert!(settled(), "the rows left reads on the thread");
    let told = TOLD.with(RefCell::take);
    (0..db.graph.len())
        .map(|at| {
            let key = node(&db, at).0.as_u32();
            told.iter().filter(|it| it.0 == key).map(|it| it.1).collect()
        })
        .collect()
}

/// The invariant: the round a row's `cycle_recover` is told is its cycle's outermost head's. A
/// head the cycle keeps from one round to the next is told each round's number in turn, at
/// whatever depth it is nested: its new frame starts at the round of the head that kept it.
/// Rows 0, 3 and 1 are each read on the stack in each of the three rounds; rows 2 and 4 never
/// are, so they recover nothing.
#[test]
fn a_kept_head_is_told_the_round_of_its_cycles_outermost_head() {
    let rounds = vec![1, 2, 3];
    assert_eq!(told(false), [rounds.clone(), rounds.clone(), vec![], rounds, vec![]]); // clone: the expected rows
}

/// The control: with a kept head's frame started at 1 again, as it was, row 1 (whose head, row
/// 3, is such a frame) is told round 1 three times; row 3, whose head is the outermost, is
/// still told the round.
#[test]
fn the_oracle_catches_a_kept_head_that_counts_its_rounds_from_one_again() {
    let rounds = vec![1, 2, 3];
    assert_eq!(told(true), [rounds.clone(), vec![1, 1, 1], vec![], rounds, vec![]]); // clone: as above
}
