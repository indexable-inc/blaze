use std::alloc::Global;
use std::vec::Vec;

use crate::flow::{BorrowKind, Bound, Flow, FlowBuilder, Loan, Operand, Place, Point, Span, Stmt};
use crate::record::Plant;

use super::solver::{LIMIT, Meter, solve};
use super::{BorrowFinding, TierOne, facts, tier_one, tier_two_oracle, tier_two_oracle_counted};

const SPAN: Span = Span { lo: 4, hi: 7 };
const BODY: &str = "generated::log_tempo_event";

/// N locals (including the return place and the argument), N origins and N loans.
/// Each expansion does `tmp = &arg; arg = value; log(tmp)`. The write conflicts
/// with exactly the loan just issued. It also kills that loan on the outgoing edge.
fn logging_body(n: u32) -> Flow<Global> {
    let mut builder = FlowBuilder::new_in(n, 1, Global);
    let block = builder.block();
    builder.start(block);
    for i in 0..n {
        let tmp = 2 + i % (n - 2);
        let origin = builder.origin(false);
        builder.mentions(tmp, origin);
        builder.borrow(Place(tmp), BorrowKind::Shared, Place(1), origin, SPAN);
        builder.assign(Place(1), Operand::Const, SPAN);
        builder.call(&[Operand::Copy(Place(tmp))], Place(0), SPAN);
    }
    builder.ret(SPAN);
    builder.finish().expect("one terminated block")
}

/// The location-insensitive filter scans the N loans on the shared place at
/// each borrow and each write. Reporting and killing scan them twice more at
/// each write: 4 N² entries. Each loan travels across one edge, so all
/// remaining work is linear except for sorting N distinct findings. The
/// multiplier leaves room for implementation constants at the smallest N.
fn step_bound(n: u32) -> u64 {
    256 * u64::from(n).pow(2)
}

/// Exact count of the oracle's inner-loop cell visits for this family. The
/// subset relation is empty. Each pass therefore visits O² cells twice at
/// each point and O² + OL cells on each edge. O = L = N and E = P - 1.
/// The first forward pass propagates every issued loan to its write.
/// Every write kills its loan, and the second pass sees no change.
fn dense_steps(n: u32) -> u64 {
    let points = 6 * u64::from(n) + 2;
    (8 * points - 4) * u64::from(n).pow(2)
}

#[test]
fn generated_logging_bodies_stay_within_quadratic_steps() {
    for n in [16, 64, 256, 1024] {
        let flow = logging_body(n);
        let facts = facts(&flow, Global).expect("liveness and activation converge");
        assert_eq!(tier_one(&flow, &facts, Plant::None, Global), Ok(TierOne::Refused));
        let bound = step_bound(n);
        let mut meter = Meter::new(bound.min(LIMIT));
        let found = solve(BODY, &flow, &facts, Plant::None, Global, &mut meter)
            .expect("short consecutive loans fit the quadratic work bound");
        let expected: Vec<_> = (0..n)
            .map(|i| BorrowFinding::Invalidated {
                at: Point::start(Stmt(3 * i + 1)),
                loan: Loan(i),
                span: SPAN,
                returned: false,
            })
            .collect();
        assert_eq!(found.as_slice(), expected.as_slice());
        assert!(meter.used() > 0 && meter.used() <= bound);
        std::println!(
            "N={n} steps={} old_cells={} bound={bound} findings={n}",
            meter.used(), dense_steps(n)
        );
    }
}

#[test]
fn single_bit_control_exceeds_the_same_quadratic_bound() {
    // Plant the former evaluator at the solver seam. Count its actual loop
    // visits at this size, small enough to run even with debug assertions.
    let n = 16;
    let flow = logging_body(n);
    let facts = facts(&flow, Global).expect("liveness and activation converge");
    let mut cells = 0;
    let dense = tier_two_oracle_counted(&flow, &facts, Plant::None, Global, &mut cells)
        .expect("the dense oracle converges");
    let mut meter = Meter::new(step_bound(n));
    let sparse = solve(BODY, &flow, &facts, Plant::None, Global, &mut meter)
        .expect("the sparse solver meets the bound");
    assert_eq!(dense.as_slice(), sparse.as_slice());
    assert_eq!(cells, dense_steps(n));
    assert!(cells > step_bound(n));
}

#[test]
fn exhausted_budget_names_function_and_dimensions() {
    let flow = logging_body(16);
    let facts = facts(&flow, Global).expect("liveness and activation converge");
    let mut meter = Meter::new(step_bound(16));
    let full = solve(BODY, &flow, &facts, Plant::None, Global, &mut meter)
        .expect("the completed check supplies its exact work");
    let exact = meter.used();
    let at_limit = solve(BODY, &flow, &facts, Plant::None, Global, &mut Meter::new(exact))
        .expect("the last allowed step succeeds");
    assert_eq!(at_limit.as_slice(), full.as_slice());
    let budget = exact.checked_sub(1).expect("the solver processed a loan");
    let refusal = solve(BODY, &flow, &facts, Plant::None, Global, &mut Meter::new(budget))
        .expect_err("one fewer step refuses the body");
    assert_eq!(refusal, Bound::TierTwo { function: BODY, points: 98, origins: 16, loans: 16, budget });
    let anonymous = solve((), &flow, &facts, Plant::None, Global, &mut Meter::new(budget))
        .expect_err("the driver's anonymous flow carries the same dimensions");
    assert_eq!(refusal, anonymous.in_function(BODY));
    assert_eq!(
        std::format!("{refusal}"),
        std::format!("tier two exceeded {budget} steps in function `generated::log_tempo_event` (98 points, 16 origins, 16 loans)")
    );
}

#[test]
fn subset_survives_a_dead_bridge_and_cycles_are_point_local() {
    // The signature supplies 'a and 'b with no known relation. The first
    // assignment has 'a <-> bridge -> local. Even though bridge is dead on
    // the next edge, the induced relation 'a -> local crosses with the two
    // live endpoints. The reverse 'b -> 'a only starts at the return.
    let mut builder = FlowBuilder::new_in(2, 1, Global);
    let a = builder.origin(true);
    let b = builder.origin(true);
    let bridge = builder.origin(false);
    let local = builder.origin(false);
    builder.mentions(1, local);
    let block = builder.block();
    builder.start(block);
    builder.assign(Place(0), Operand::Const, SPAN);
    builder.subset(a, bridge);
    builder.subset(bridge, a);
    builder.subset(bridge, local);
    let forward = builder.assign(Place(0), Operand::Copy(Place(1)), SPAN);
    builder.subset(local, b);
    let reverse = builder.ret(SPAN);
    builder.subset(b, a);
    let flow = builder.finish().expect("one terminated block");
    let facts = facts(&flow, Global).expect("liveness and activation converge");
    let found = solve("outlives", &flow, &facts, Plant::None, Global, &mut Meter::new(u64::MAX))
        .expect("the two relations converge");
    let oracle = tier_two_oracle(&flow, &facts, Plant::None, Global).expect("two placeholder relations");
    assert_eq!(found.as_slice(), oracle.as_slice());
    assert_eq!(found.as_slice(), &[
        BorrowFinding::UnprovenOutlives { at: Point::mid(forward), from: a, into: b, span: SPAN },
        BorrowFinding::UnprovenOutlives { at: Point::mid(reverse), from: b, into: a, span: SPAN },
    ]);
}

#[test]
fn later_return_marks_invalidated_loans_on_both_sides_of_word_boundary() {
    // Each shared loan survives the exclusive borrow (an invalidation, not
    // an assignment to the referent). Its origin flows into the return
    // origin only afterwards. Every earlier finding must say "returned".
    let mut builder = FlowBuilder::new_in(4, 1, Global);
    let shared = builder.origin(true);
    let exclusive = builder.origin(false);
    let returned = builder.origin(false);
    builder.mentions(2, shared);
    builder.mentions(0, returned);
    let block = builder.block();
    builder.start(block);
    for _ in 0..65 {
        builder.borrow(Place(2), BorrowKind::Shared, Place(1), shared, SPAN);
    }
    builder.borrow(Place(3), BorrowKind::Mut, Place(1), exclusive, SPAN);
    builder.assign(Place(0), Operand::Copy(Place(2)), SPAN);
    builder.subset(shared, returned);
    builder.ret(SPAN);
    let flow = builder.finish().expect("one terminated block");
    let facts = facts(&flow, Global).expect("liveness and activation converge");
    let found = solve("returned", &flow, &facts, Plant::None, Global, &mut Meter::new(u64::MAX))
        .expect("all loans reach the return origin");
    let oracle = tier_two_oracle(&flow, &facts, Plant::None, Global).expect("65 loans reach the return origin");
    assert_eq!(found.as_slice(), oracle.as_slice());
    let expected: Vec<_> = (0..65)
        .map(|l| BorrowFinding::Invalidated {
            at: Point::start(Stmt(65)), loan: Loan(l), span: SPAN, returned: true,
        })
        .collect();
    assert_eq!(found.as_slice(), expected.as_slice());
}

#[test]
fn candidates_keep_an_unloaned_placeholder_bridge_and_drop_an_unobservable_loan() {
    let mut b = FlowBuilder::new_in(4, 1, Global);
    let held = b.origin(false);
    let unused = b.origin(false);
    let a = b.origin(true);
    let bridge = b.origin(false);
    let z = b.origin(true);
    let leaf = b.origin(false);
    b.mentions(2, held);
    let block = b.block();
    b.start(block);
    b.borrow(Place(2), BorrowKind::Shared, Place(1), held, SPAN);
    let write = b.assign(Place(1), Operand::Const, SPAN);
    b.call(&[Operand::Copy(Place(2))], Place(0), SPAN);
    // The only conflicting write precedes this issue in the acyclic CFG.
    b.borrow(Place(3), BorrowKind::Shared, Place(1), unused, SPAN);
    let end = b.ret(SPAN);
    b.subset(a, bridge);
    b.subset(bridge, z);
    b.subset(a, leaf);
    let flow = b.finish().expect("terminated block");
    let facts = facts(&flow, Global).unwrap();
    let keep = super::filter::candidates(&flow, &facts, Plant::None, &mut Meter::new(LIMIT), Global).unwrap();
    assert_eq!(keep.loans.as_slice(), &[true, false]);
    assert_eq!(keep.origins.as_slice(), &[true, false, true, true, true, false]);
    let found = super::tier_two(&flow, &facts, Plant::None, Global).unwrap();
    assert_eq!(found.as_slice(), &[
        BorrowFinding::Invalidated { at: Point::start(write), loan: Loan(0), span: SPAN, returned: false },
        BorrowFinding::UnprovenOutlives { at: Point::mid(end), from: a, into: z, span: SPAN },
    ]);
}
