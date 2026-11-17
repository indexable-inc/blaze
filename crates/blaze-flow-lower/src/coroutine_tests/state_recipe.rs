//! The state recipe (state_recipe.rs, ASYNC.md P8 option (b)): what an async block saves across
//! its `.await`s, as types, from its own flow.
//!
//! The rule (blaze-flow suspend.rs, rustc's coroutine/layout.rs `locals_live_across_suspend_points`):
//! a local is saved when, at a suspension, it is maybe storage-live and (used after it, or maybe
//! borrowed). Two cases: a local only BORROWED across the `.await` (no use of the local itself
//! after it), and a TEMPORARY evaluated before the `.await` and used after it (main's ruling:
//! it is a flow local, so it is saved).
//!
//! Control: blaze-flow's own `SavedPlant::NoBorrows` (suspend_tests.rs
//! `a_borrowed_local_is_saved_even_if_unused_after`) removes the borrow rule; there `x` is not
//! saved, and a state laid from that recipe would have no field for `x` while `r` points at it
//! (freed stack after the first suspension). The plant is crate-private to blaze-flow, so the
//! control runs there; here the recipe is asserted to hold `x`.

use std::alloc::Global;

use blaze_typeck::fixture::Bb;
use blaze_typeck::{ExprId, Mutbl, PNode, PTy, UintTy};

use super::{Case, adt_ty, borrow_across, case, lowered};
use crate::identity::{Identity, Role};
use crate::state_recipe::{StateRecipe, state_recipe};

/// The recipe of case `k`'s block.
fn recipe_of(k: &Case) -> StateRecipe<Global> {
    let l = lowered(k);
    let view = k.b.view(k.it.owner, k.block);
    state_recipe(&view, &l, Global).unwrap_or_else(|r| panic!("state_recipe refused: {r:?}"))
}

/// The record's type whose node is `Uint(U8)`.
fn u8_ty(c: &blaze_typeck::Checked<'_>) -> Option<PTy> {
    c.types.view().nodes().position(|n| matches!(*n, PNode::Uint(UintTy::U8))).map(|i| PTy(i as u32))
}

/// `take(&make_rc(), ready().await)`: the `Rc<i32>` temporary is made before the `.await` and
/// borrowed by the call after it.
pub(super) fn temp_across(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["take"]);
    let e1 = b.path(&["make_rc"]);
    let e2 = b.call(e1, &[]);
    let e3 = b.borrow(Mutbl::Not, e2);
    let e4 = b.path(&["ready"]);
    let e5 = b.call(e4, &[]);
    let e6 = b.await_(e5);
    let e7 = b.call(e0, &[e3, e6]);
    b.block(&[], Some(e7))
}

/// `async { let x = 1u8; let r = &x; ready().await; read(r) }`: `x` is saved though nothing
/// names `x` after the `.await` (it is borrowed across it), with `r`.
/// Prints "a borrowed local is in the recipe: x and r".
#[test]
fn a_local_borrowed_across_an_await_is_saved() {
    let k = case(borrow_across);
    let recipe = recipe_of(&k);
    let u8_ = u8_ty(&k.c).expect("the fixture has u8");
    assert!(
        recipe.saved.iter().any(|s| s.ty == u8_ && s.wrap.is_none() && matches!(s.identity, Identity::Binding(_))),
        "x: u8 is borrowed across the await, so it is saved: {:?}",
        recipe.saved
    );
    assert_eq!(recipe.points, 1, "one .await");
    std::println!("a borrowed local is in the recipe: x and r");
}

/// `take(&make_rc(), ready().await)`: the temporary `Rc<i32>` lives across the `.await` (the
/// argument list is evaluated left to right, the Reference, "Call expressions"; the temporary
/// lives to the end of the statement, "Temporary scopes"), so the flow names it and the recipe
/// holds it. Without it the state would have no place for the temporary the call reads after
/// the resume, and the future's `Send` answer would leave it out (send.rs
/// `a_temporary_held_across_an_await_is_not_send`).
/// Prints "a temporary across the await is in the recipe".
#[test]
fn a_temporary_across_an_await_is_saved() {
    let k = case(temp_across);
    let recipe = recipe_of(&k);
    let rc = adt_ty(&k.c, k.it.rc).expect("the fixture has Rc<i32>");
    assert!(
        recipe.saved.iter().any(|s| s.ty == rc && matches!(s.identity, Identity::Temp(_, Role::Value))),
        "the Rc<i32> temporary is held across the await: {:?}",
        recipe.saved
    );
    std::println!("a temporary across the await is in the recipe");
}

/// What the builder looks up (identity.rs; async-P8-4's first lines): across the `.await` of
/// `borrow_across` the state saves the future `tmp` by its key (`Role::Awaitee` at the `.await`),
/// and never the `Pin` (made at the loop's head each poll, storage-dead before the suspend) nor
/// the `.await`'s value (`Role::Result`: written after the resume). Every saved key is unique.
/// Prints "the awaitee is saved by its key; no pin, no result".
#[test]
fn the_awaitee_is_saved_and_the_pin_is_not() {
    let k = case(borrow_across);
    let recipe = recipe_of(&k);
    let roles: std::vec::Vec<Role> = recipe
        .saved
        .iter()
        .filter_map(|s| match s.identity {
            Identity::Temp(_, r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(roles.contains(&Role::Awaitee), "the future is held across its own suspension: {:?}", recipe.saved);
    assert!(!roles.contains(&Role::Pin), "the pin is remade at the head each poll: {:?}", recipe.saved);
    assert!(!roles.contains(&Role::Result), "the await's value is written after the resume: {:?}", recipe.saved);
    std::println!("the awaitee is saved by its key; no pin, no result");
}
