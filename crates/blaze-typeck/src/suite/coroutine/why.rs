//! The solver's chain of why an unmet auto-trait goal of an async block's future has no
//! solution, carried on the finding (`Diag::why`, the record's `why` column; blaze-solve
//! why_no.rs). The world is the parent module's: `spawn<F: Send>`, `Rc` whose `Send` impl has
//! the where-clause `Rc: Never` that never holds (the toy world's stand-in for
//! `impl !Send for Rc`), `Ready` a field-less struct and so `Send`.
//!
//! The rule: the Reference, "Auto traits": an async block's future is `Send` when every value
//! its state holds is ("Async blocks": its captures and what it holds across an `.await`);
//! "Implementations": an impl holds where its where-clauses hold. So `spawn(async move { r })`
//! with `r: Rc` fails at the root `{future}: Send`, because the interior's slot `Rc: Send`
//! does, because the impl's where-clause `Rc: Never` has no impl.
//!
//! Not here: an `Rc` held across an `.await` but captured by none. On the superset interior
//! the check cannot tell it is live there (rules/async_fn.rs, 2), so it is the interior gap
//! (`rc_held_across_await_is_a_gap`), or under `check_deferring` a stalled row decided on the
//! flow's exact interior, whose finding's chain the decision walks there (src/stalled.rs
//! `StalledDecision`; suite/coroutine/stalled.rs `a_stalled_finding_chains_on_the_exact_interior`).
//! The record's stalled row carries none: the superset's chain is not the exact one's.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::{Items, world};
use crate::body::Capture;
use crate::diag::{Because, DiagCode, UnmetGoal, WhyDef, WhyLeaf, WhyReached, WhyStep};
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::suite::check_body;

/// `fn c_sync() { let r = <make>(); spawn(async move { r }); }`: the fixture, the block, the
/// record.
fn moved_into_spawn(make: &str) -> (Fx, ExprId, crate::columns::Checked<'static>) {
    let (fx, it): (Fx, Items) = world();
    let mut b = Bb::new();
    let e0 = b.path(&[make]);
    let e1 = b.call(e0, &[]);
    let r = b.bind("r");
    let s0 = b.let_(r, None, e1);
    let e2 = b.path(&["spawn"]);
    let e3 = b.local(r);
    let body = b.block(&[], Some(e3));
    let fut = b.async_block(body, Capture::Move);
    let call = b.call(e2, &[fut]);
    let s1 = b.semi(call);
    let root = b.block(&[s0, s1], None);
    let c = check_body(&fx, &b, it.c_sync, root, Plant::None, InferPlant::None);
    (fx, fut, c)
}

/// A step's goal as `Self: Trait`, by the fixture's names.
fn shown(fx: &Fx, c: &crate::columns::Checked<'_>, g: Option<UnmetGoal>) -> String {
    let Some(UnmetGoal::Trait { trait_, args }) = g else { return format!("{g:?}") };
    let tys = c.types.view();
    let args = tys.list(args).expect("the goal's arguments in the record");
    format!("{}: {}", show(fx, tys, args[0]), fx.name_of(trait_))
}

/// `let r = make_rc(); spawn(async move { r });`: E0277 at the `spawn`, and the finding
/// carries the whole chain: the root `{future}: Send`; the interior's slot holding the moved
/// `Rc` (`Rc: Send`); the where-clause of `impl Send for Rc where Rc: Never` (`Rc: Never`),
/// which no impl names. The finding's leaf is the chain's last goal.
///
/// Without `why_chain` the finding has no `why` and the leaf is the one level the old impl
/// walk gave; without the slot step (no interior walked) the chain stops at the root.
///
/// Expected line: `test suite::coroutine::why::a_moved_rc_chains_root_slot_clause_to_no_impl ... ok`.
#[test]
fn a_moved_rc_chains_root_slot_clause_to_no_impl() {
    let (fx, fut, c) = moved_into_spawn("make_rc");
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<Vec<_>>(), std::vec![DiagCode::Unsatisfied], "{:?}", c.diags);
    let d = c.diags[0];
    let why = d.why.expect("the finding carries the solver's chain");
    let steps: &[WhyStep] = c.why_of(why).expect("the chain's rows in the record");
    let said: Vec<(WhyReached, String)> = steps.iter().map(|s| (s.reached, shown(&fx, &c, s.goal))).collect();
    assert_eq!(steps.len(), 3, "{said:?}");
    assert_eq!(steps[0].reached, WhyReached::Root, "{said:?}");
    assert!(said[0].1.ends_with(": Send"), "the root is the future's `Send`: {said:?}");
    assert!(
        matches!(steps[1].reached, WhyReached::CoroutineSlot { coroutine: WhyDef::Expr(e), .. } if e == fut),
        "the second step is a slot of the spawned block's interior: {said:?}"
    );
    assert_eq!(said[1].1, "Rc: Send", "{said:?}");
    let imp: ItemKey = fx.key_of("impl Send for Rc where Rc: Never");
    assert_eq!(steps[2].reached, WhyReached::ImplClause { impl_: imp, clause: 0 }, "{said:?}");
    assert_eq!(said[2].1, "Rc: Never", "{said:?}");
    assert_eq!(why.leaf, WhyLeaf::NoImpl, "no impl of `Never` names `Rc`: {said:?}");
    // The finding's leaf (what a person fixes) is the chain's last goal.
    let Some(Because::Unmet { leaf, .. }) = d.because else { panic!("an unmet goal's finding: {d:?}") };
    assert_eq!(leaf, steps[2].goal, "{said:?}");
}

/// The twin: `let r = ready(); spawn(async move { r });`: `Ready` is `Send` (no field), so the
/// interior is, and the Reference accepts the `spawn`: no finding, and no chain row.
///
/// Expected line: `test suite::coroutine::why::a_moved_send_value_has_no_finding_and_no_chain ... ok`.
#[test]
fn a_moved_send_value_has_no_finding_and_no_chain() {
    let (_, _, c) = moved_into_spawn("ready");
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert!(c.why.is_empty(), "{:?}", c.why);
}

/// `fn c_gen_unbound<T>(t: T) { spawn(async move { t }); }`: the chain is the root and the
/// interior's slot `T: Send`, which nothing says (a parameter has no auto impl and the
/// environment has no bound): it fails by itself.
///
/// Expected line: `test suite::coroutine::why::a_moved_parameter_chains_to_its_own_send ... ok`.
#[test]
fn a_moved_parameter_chains_to_its_own_send() {
    let (fx, it) = world();
    let mut b = Bb::new();
    b.param("t");
    let e0 = b.path(&["spawn"]);
    let body = super::move_t(&mut b);
    let fut = b.async_block(body, Capture::Move);
    let call = b.call(e0, &[fut]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, it.c_gen_unbound, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 1, "{:?}", c.diags);
    let why = c.diags[0].why.expect("the finding carries the solver's chain");
    let steps = c.why_of(why).expect("the chain's rows in the record");
    let said: Vec<(WhyReached, String)> = steps.iter().map(|s| (s.reached, shown(&fx, &c, s.goal))).collect();
    assert_eq!(steps.len(), 2, "{said:?}");
    assert!(matches!(steps[1].reached, WhyReached::CoroutineSlot { coroutine: WhyDef::Expr(e), .. } if e == fut), "{said:?}");
    assert_eq!(said[1].1, "P0: Send", "{said:?}");
    // No impl of `Send` is for a parameter: none at all, or only impls for other types.
    assert!(matches!(why.leaf, WhyLeaf::NoImpl | WhyLeaf::Refused { .. }), "{:?} {said:?}", why.leaf);
}
