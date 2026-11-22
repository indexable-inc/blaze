//! Operand order within one statement (lane B13, for lane B11's `twice_p`; NEVER COMPILED when
//! written, 2026-11-18). The Reference, "Evaluation order of operands": a tuple's, an array's
//! and a call's operands are evaluated left to right, so in `(p, p)` and `h(p, p)` of a
//! non-`Copy` `p` the second operand reads a moved value (rustc E0382). The move check read
//! every operand against the statement's entry state and found nothing.
//!
//! POSITIVE: `_2 = (move _1, move _1)` and `_2 = call(move _1, move _1)` each find
//! `UseOfMoved` of `_1` at the statement. NEGATIVE OF THE NEGATIVE (stays clean): `(copy _1,
//! copy _1)` and `(copy _1, move _1)` (a read, then the move). CONTROL (rule 23):
//! `Plant::OperandsAtEntry` (the form before) finds nothing in the positive.

use std::vec::Vec;

use blaze_own::Inline;

use crate::flow::{FlowBuilder, Operand, Place, Span, Stmt};
use crate::moves::{MoveFindingKind, check_moves_planted};
use crate::record::Plant;

const S: Span = Span { lo: 0, hi: 0 };

/// The findings of `_0 = const; <one statement over _1>; return` with `_1` the one argument.
fn found(call: bool, ops: [Operand; 2], plant: Plant) -> Vec<(Stmt, Place, MoveFindingKind)> {
    // The case's own heap, on the stack (refuses when full, never falls back).
    let heap: Inline<65536> = Inline::new();
    let mut b = FlowBuilder::new_in(3, 1, &heap);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    if call {
        b.call(&ops, Place(2), S);
    } else {
        b.aggregate(Place(2), &ops, S);
    }
    b.ret(S);
    let flow = match b.finish() {
        Ok(f) => f,
        Err(e) => panic!("the case's flow: {e:?}"),
    };
    crate::loans::assert_tiers_equal_for_fixture(&flow);
    match check_moves_planted(&flow, plant, &heap) {
        Ok(v) => v.iter().map(|f| (f.at.stmt(), f.place, f.kind)).collect(),
        Err(b) => panic!("the move check's bound: {b:?}"),
    }
}

/// Prints `aggregate (move, move): [(Stmt(1), Place(1), UseOfMoved)]`, the same for the call,
/// `copy, copy: []`, `copy, move: []` and `control: []`.
#[test]
fn the_second_operand_sees_the_first_ones_move() {
    let (mv, cp) = (Operand::Move(Place(1)), Operand::Copy(Place(1)));
    let want = [(Stmt(1), Place(1), MoveFindingKind::UseOfMoved)];
    for call in [false, true] {
        let got = found(call, [mv, mv], Plant::None);
        std::println!("{} (move, move): {got:?}", if call { "call" } else { "aggregate" });
        assert_eq!(got, want);
        let got = found(call, [cp, cp], Plant::None);
        std::println!("copy, copy: {got:?}");
        assert_eq!(got, []);
        let got = found(call, [cp, mv], Plant::None);
        std::println!("copy, move: {got:?}");
        assert_eq!(got, []);
        let got = found(call, [mv, mv], Plant::OperandsAtEntry);
        std::println!("control: {got:?}");
        assert_eq!(got, []);
    }
}
