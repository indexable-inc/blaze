//! The saved locals of a coroutine body (`crate::suspend`, ASYNC.md D-A1, prompt P1), on
//! hand-made flows. Each case states which locals must be saved and why, from the Reference.
//!
//! The Reference, "Async blocks": "The future type that rustc generates is roughly equivalent to
//! an enum with one variant per await point, where each variant stores the data needed to resume
//! from its corresponding point." A saved local is exactly such "data needed to resume" (or to
//! drop the future while parked there).
#![cfg(test)]

use std::alloc::Global;
use std::format;
use std::string::String;

use crate::flow::{Block, BorrowKind, Flow, FlowBuilder, Local, Operand, Place, Span, Stmt, StmtKind};
use crate::suspend::{SavedPlant, saved, saved_planted};

const S: Span = Span { lo: 0, hi: 0 };

fn finish(b: FlowBuilder<Global>) -> Flow<Global> {
    let flow = b.finish().expect("a well-formed suspend fixture");
    crate::loans::assert_tiers_equal_for_fixture(&flow);
    flow
}

/// The first `Suspend` statement of the flow.
fn first_suspend(flow: &Flow<Global>) -> Stmt {
    match flow.stmts.iter().position(|r| matches!(r.kind, StmtKind::Suspend { .. })) {
        Some(s) => Stmt(s as u32),
        None => panic!("the flow has no suspend"),
    }
}

/// `_1 = const; suspend; use _1`.
///
/// The Reference, "Await expressions": "If the call to poll returns Poll::Pending, then the
/// future returns Poll::Pending, suspending its state so that, when the surrounding async
/// context is re-polled, execution returns to step 3". `_1` is read after the resume, so its
/// value is part of the suspended state: saved. `_2` is never touched: not saved; `_0` never is.
///
/// Expected: passes (one of the four `suspend` tests in `test blaze-flow: exit 0; N passed`).
#[test]
fn a_local_used_after_the_suspension_is_saved() {
    let mut b = FlowBuilder::new_in(3, 0, Global);
    let (bb0, resume, cancel) = (b.block(), b.block(), b.block());
    b.start(bb0);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.assign(Place(1), Operand::Const, S);
    b.suspend(resume, cancel);
    b.start(resume);
    b.assign(Place(0), Operand::Copy(Place(1)), S);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    b.start(cancel);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    let flow = finish(b);
    let s = first_suspend(&flow);
    let got = match saved(&flow, Global) {
        Ok(g) => g,
        Err(e) => panic!("{e:?}"),
    };
    assert_eq!(got.suspends, 1, "one suspend in the body");
    assert!(got.is_saved(Local(1)), "local _1 must be saved: it is read after suspend {s}");
    assert!(!got.is_saved(Local(2)), "local _2 must not be saved: nothing reads it");
    assert!(!got.is_saved(Local(0)), "local _0, the return place, is never saved");
}

/// `_1 = const; _2 = &_1; StorageDead(_1); suspend; return`.
///
/// The Reference, "Destructors": "Each variable or temporary is associated to a drop scope. When
/// control flow leaves a drop scope all variables associated to that scope are dropped": `_1`'s
/// scope ended before the suspension, so it holds nothing to resume with, and its borrow ended
/// with its storage. Not saved. (A wrong fix that dropped BOTH the storage-live intersection and
/// `StorageDead`'s kill of the borrow would save it.)
///
/// Expected: passes.
#[test]
fn a_local_dead_before_the_suspension_is_not_saved() {
    let mut b = FlowBuilder::new_in(3, 0, Global);
    let o = b.origin(false);
    b.mentions(2, o);
    let (bb0, resume, cancel) = (b.block(), b.block(), b.block());
    b.start(bb0);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.assign(Place(1), Operand::Const, S);
    b.borrow(Place(2), BorrowKind::Shared, Place(1), o, S);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.suspend(resume, cancel);
    b.start(resume);
    b.ret(S);
    b.start(cancel);
    b.ret(S);
    let flow = finish(b);
    let s = first_suspend(&flow);
    let got = match saved(&flow, Global) {
        Ok(g) => g,
        Err(e) => panic!("{e:?}"),
    };
    assert!(!got.is_saved(Local(1)), "local _1 must not be saved: its storage ended before suspend {s}");
}

/// The flow of the borrowed-local case: `s0 live _1; s1 _1 = const; s2 _2 = &_1; s3 suspend;
/// return` on both paths.
fn borrowed_flow() -> Flow<Global> {
    let mut b = FlowBuilder::new_in(3, 0, Global);
    let o = b.origin(false);
    b.mentions(2, o);
    let (bb0, resume, cancel): (Block, Block, Block) = (b.block(), b.block(), b.block());
    b.start(bb0);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.assign(Place(1), Operand::Const, S);
    b.borrow(Place(2), BorrowKind::Shared, Place(1), o, S);
    b.suspend(resume, cancel);
    b.start(resume);
    b.ret(S);
    b.start(cancel);
    b.ret(S);
    finish(b)
}

fn borrowed_case(plant: SavedPlant) -> Result<(), String> {
    let flow = borrowed_flow();
    let s = first_suspend(&flow);
    let got = match saved_planted(&flow, plant, Global) {
        Ok(g) => g,
        Err(e) => return Err(format!("{e:?}")),
    };
    if !got.is_saved(Local(1)) {
        return Err(format!("local _1 must be saved: it was borrowed before suspend {s}"));
    }
    Ok(())
}

/// `_2 = &_1; suspend; return`: `_1` is saved though no statement after the suspension names it.
///
/// `Future::poll` takes `self: Pin<&mut Self>` (core/src/future/future.rs:113): the state does
/// not move once polled, so a loan of a saved local stays valid across the suspension, and a
/// reference to `_1` may be read through after the resume (D-A1: "maybe-borrowed since its
/// `StorageLive`"). `_1` must therefore live in the suspended state.
///
/// Planted control: with the borrow rule removed (`SavedPlant::NoBorrows`) the same case fails
/// with "local _1 must be saved: it was borrowed before suspend s3".
///
/// Expected: passes; without the borrow rule it prints
/// "local _1 must be saved: it was borrowed before suspend s3".
#[test]
fn a_borrowed_local_is_saved_even_if_unused_after() {
    if let Err(e) = borrowed_case(SavedPlant::None) {
        panic!("{e}");
    }
    assert_eq!(
        borrowed_case(SavedPlant::NoBorrows),
        Err(String::from("local _1 must be saved: it was borrowed before suspend s3")),
        "the planted control (no borrow rule) must fail with the named message",
    );
}

/// A `Drop(_1)` only in the cancel block saves `_1`; a local moved out before the suspension and
/// dropped there is not saved.
///
/// The Reference, "Destructors": "When an initialized variable or temporary goes out of scope,
/// its destructor is run or it is dropped." Dropping the future while it is parked at the
/// suspension leaves `_1`'s scope (the cancel path, coroutines.md D2), so `_1`, initialized there,
/// is dropped from the suspended state: saved. In the second flow `_1` was moved into `_2` before
/// the suspension, so the cancel path's `Drop(_1)` runs no destructor ("initialized" is false),
/// and `_1` is not saved; `_2`, dropped on the cancel path while initialized, is.
///
/// Expected: passes.
#[test]
fn a_local_used_only_on_the_cancel_path_is_saved() {
    let mut b = FlowBuilder::new_in(2, 0, Global);
    let (bb0, resume, cancel) = (b.block(), b.block(), b.block());
    b.start(bb0);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.assign(Place(1), Operand::Const, S);
    b.suspend(resume, cancel);
    b.start(resume);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    b.start(cancel);
    b.stmt(StmtKind::Drop(Place(1)), S);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    let flow = finish(b);
    let s = first_suspend(&flow);
    let got = match saved(&flow, Global) {
        Ok(g) => g,
        Err(e) => panic!("{e:?}"),
    };
    assert!(got.is_saved(Local(1)), "local _1 must be saved: the cancel path of suspend {s} drops it");

    let mut b = FlowBuilder::new_in(3, 0, Global);
    let (bb0, resume, cancel) = (b.block(), b.block(), b.block());
    b.start(bb0);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.stmt(StmtKind::StorageLive(Local(2)), S);
    b.assign(Place(1), Operand::Const, S);
    b.assign(Place(2), Operand::Move(Place(1)), S);
    b.suspend(resume, cancel);
    b.start(resume);
    b.stmt(StmtKind::StorageDead(Local(2)), S);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    b.start(cancel);
    b.stmt(StmtKind::Drop(Place(2)), S);
    b.stmt(StmtKind::StorageDead(Local(2)), S);
    b.stmt(StmtKind::Drop(Place(1)), S);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    let flow = finish(b);
    let s = first_suspend(&flow);
    let got = match saved(&flow, Global) {
        Ok(g) => g,
        Err(e) => panic!("{e:?}"),
    };
    assert!(got.is_saved(Local(2)), "local _2 must be saved: the cancel path of suspend {s} drops it");
    assert!(
        !got.is_saved(Local(1)),
        "local _1 must not be saved: it was moved out before suspend {s}, so its drop runs nothing",
    );
}
