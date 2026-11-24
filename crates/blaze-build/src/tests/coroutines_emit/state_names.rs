//! PROOF of async-P8-4 (5): every slot a coroutine keeps across a suspension names a flow local
//! (walk/state_names.rs; blaze-lir coroutine_region.rs), on this file's native runner.
//!
//! The program is the parent's `two` (`async fn two() -> u64 { let a: u64 = 40; let r: u64 =
//! twice().await; a + r }`), built into a state laid from TYPES by `lay_state` over the flow's
//! saved locals, keyed by identity as the flow layer emits them (blaze-flow-lower
//! identity.rs): `a` (`Binding(a)`, a `u64`), and the `.await`'s future (`Temp(await,
//! Awaitee)`, a `Twice`, 16 bytes). (Which locals the flow saves for such a body is the
//! flow-lower test `the_awaitee_is_saved_and_the_pin_is_not`; here the table is written out.)
//!
//! - The run: the split refuses any slot read after the suspension that no name places, so a
//!   build that succeeds IS one whose kept slots all name a flow local; it then runs as the
//!   unlaid build does (ready(42) at poll 3, `twice()` called once).
//! - THE PLANT (`StatePlant::BuilderSlot`): each `.await` makes a slot of the walk's own before
//!   its loop and reads it after the resume. Refused by name at the `.await`.
//! - The control: the same table without `a`'s row: `a`'s home is a slot the walk made that the
//!   table does not name, read after the resume (`a + r`): refused by name.

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_flow_lower::identity::{Identity, Role};
use blaze_own::Alloc;
use blaze_typeck::{BinOp, Capture, ExprId, PatId};

use super::{Prog, Ran, Script, async_body, async_fn, bin, call_await, let_u64, lit, open_world, run, run_laid};
use crate::BuildError;
use crate::coroutine_state::{
    LaidState, StateName, StateNames, StatePlan, StatePlant, UNNAMED_ACROSS, lay_state, lay_state_with_flags, lay_state_with_upvar_flags,
};

/// A state laid from a written table: each saved local's identity, layout and whether it needs
/// drop, in the recipe's order.
pub(super) struct Laid {
    laid: LaidState,
    rows: StdVec<StateName>,
    plant: StatePlant,
}

impl Laid {
    /// `saved` laid after an upvars' tuple of `env`, with `points` suspensions.
    pub(super) fn new(env: (u64, u8), saved: &[(Identity, (u64, u8), bool)], points: u32, plant: StatePlant) -> Laid {
        let sizes: StdVec<(u64, u8)> = saved.iter().map(|s| s.1).collect();
        let flags: StdVec<bool> = saved.iter().map(|s| s.2).collect();
        let l = lay_state(env, &sizes, &flags, points, Alloc::global(), Global).unwrap_or_else(|e| panic!("lay_state: {e}"));
        let mut rows: StdVec<StateName> = saved
            .iter()
            .enumerate()
            .map(|(i, s)| StateName { identity: s.0, offset: l.field[i], size: s.1.0 as u32, flag: l.flag[i], rest: l.rest[i] })
            .collect();
        rows.sort_unstable_by_key(|r| r.identity);
        Laid { laid: l.state, rows, plant }
    }

    pub(super) fn plan(&self) -> StatePlan<'_> {
        let names = StateNames::new(&self.rows, self.laid.region).unwrap_or_else(|e| panic!("the written table: {e}"));
        StatePlan { laid: self.laid, names, plant: self.plant }
    }
}

/// `two`'s body in `b`, giving the root, `a`'s pattern and the `.await`.
fn two(b: &mut blaze_typeck::fixture::Bb, lits: &mut StdVec<(ExprId, u128)>) -> (ExprId, PatId, ExprId) {
    let forty = lit(b, lits, 40);
    let (s0, a) = let_u64(b, "a", forty);
    let awaited = call_await(b, "twice");
    let (s1, r) = let_u64(b, "r", awaited);
    let (ae, re) = (b.local(a), b.local(r));
    let sum = bin(b, BinOp::Add, ae, re);
    let body = b.block(&[s0, s1], Some(sum));
    (b.async_block(body, Capture::Move), a, awaited)
}

/// Prints "state names: laid ready(42) at poll 3; builder-made slot refused at the await;
/// without a's row refused (expression N)".
#[test]
fn every_slot_kept_across_a_suspension_names_a_flow_local() {
    let mut w = open_world();
    let item = async_fn(&mut w, "two", "two_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let (root, a, awaited) = two(&mut b, &mut lits);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    let unlaid = run(&w, &progs, Script::Complete);
    let saved = [
        (Identity::Binding(a), (8, 3), false),
        (Identity::Temp(awaited, Role::Awaitee), (16, 3), false),
    ];

    // The run: built into the laid state, every kept slot named; it behaves as the unlaid build.
    let laid = Laid::new((0, 0), &saved, 1, StatePlant::None);
    let ran = run_laid(&w, &progs, Script::Complete, Some(&laid)).unwrap_or_else(|e| panic!("the laid build: {e}"));
    assert_eq!(ran, unlaid);
    assert_eq!(ran, Ran { value: 42, polls: 3, drops: 0, made: 1 });

    // THE PLANT: a slot of the walk's own across the suspension, refused at the `.await`.
    let planted = Laid::new((0, 0), &saved, 1, StatePlant::BuilderSlot);
    match run_laid(&w, &progs, Script::Complete, Some(&planted)) {
        Err(BuildError::Unsupported { expr, what }) => {
            assert_eq!((expr, what), (awaited, UNNAMED_ACROSS), "the planted slot is named at its `.await`");
        }
        other => panic!("the planted builder-made slot went uncaught: {other:?}"),
    }

    // The control: no row for `a`, whose home is read after the resume.
    let without_a = Laid::new((0, 0), &saved[1..], 1, StatePlant::None);
    let refused = match run_laid(&w, &progs, Script::Complete, Some(&without_a)) {
        Err(BuildError::Unsupported { expr, what }) if what == UNNAMED_ACROSS => expr,
        other => panic!("a's slot, unnamed and read after the resume, went uncaught: {other:?}"),
    };
    std::println!(
        "state names: laid ready({}) at poll {}; builder-made slot refused at the await; without a's row refused (expression {})",
        ran.value, ran.polls, refused.0
    );
}

#[test]
fn lay_state_with_upvar_flags_places_flags_inside_region() {
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let saved_sizes = [(8u64, 3u8), (16, 3)];
    let saved_flags = [true, false];
    let laid = lay_state_with_upvar_flags((24, 3), &saved_sizes, &saved_flags, 2, 1, a, a)
        .unwrap_or_else(|e| panic!("lay_state_with_upvar_flags: {e}"));
    assert_eq!(laid.upvar_flag.len(), 2);
    let region = laid.state.region;
    let mut rows = StdVec::new();
    for (i, &flag_off) in laid.upvar_flag.iter().enumerate() {
        assert!(flag_off >= region.start && flag_off + 1 <= region.end);
        rows.push(StateName {
            identity: Identity::Binding(PatId(i as u32)),
            offset: flag_off,
            size: 0,
            flag: Some(flag_off),
            rest: None,
        });
    }
    for (i, &(size, _)) in saved_sizes.iter().enumerate() {
        rows.push(StateName {
            identity: Identity::Binding(PatId((i + 10) as u32)),
            offset: laid.field[i],
            size: size as u32,
            flag: laid.flag[i],
            rest: laid.rest[i],
        });
    }
    rows.sort_unstable_by_key(|r| r.identity);
    let names = StateNames::new(&rows, region).unwrap_or_else(|e| panic!("StateNames::new: {e}"));
    assert_eq!(names.get(Identity::Binding(PatId(0))).and_then(|r| r.flag), Some(laid.upvar_flag[0]));
    assert_eq!(names.get(Identity::Binding(PatId(1))).and_then(|r| r.flag), Some(laid.upvar_flag[1]));
    assert_ne!(laid.upvar_flag[0], laid.upvar_flag[1]);
}

/// A `bool` literal expression (`Expr::Lit(Lit::Bool(v))`).
fn bool_lit(b: &mut blaze_typeck::fixture::Bb, v: bool) -> ExprId {
    let e = b.bool();
    b.exprs[e.ix()] = blaze_typeck::Expr::Lit(blaze_typeck::Lit::Bool(v));
    e
}

/// `if cond { then } else { else_ }`.
fn if_expr(b: &mut blaze_typeck::fixture::Bb, cond: ExprId, then: ExprId, else_: Option<ExprId>) -> ExprId {
    let e = b.int(None);
    b.exprs[e.ix()] = blaze_typeck::Expr::If { cond, then, else_ };
    e
}

/// `lhs = rhs`.
fn assign_expr(b: &mut blaze_typeck::fixture::Bb, lhs: ExprId, rhs: ExprId) -> ExprId {
    let e = b.int(None);
    b.exprs[e.ix()] = blaze_typeck::Expr::Assign { lhs, rhs };
    e
}

/// `base.<index>` (tuple field).
fn tuple_field(b: &mut blaze_typeck::fixture::Bb, base: ExprId, index: u32) -> ExprId {
    let e = b.int(None);
    b.exprs[e.ix()] = blaze_typeck::Expr::TupleField { base, index };
    e
}

/// A `Drop` local conditionally reassigned (giving it a dynamic drop flag) and then
/// unconditionally moved out before `.await` must end its slot before suspension so a laid state
/// that saves only the `.await`'s future builds cleanly and drops the local once.
#[test]
fn moved_flagged_local_before_await_ends_its_slot() {
    let mut w = open_world();
    let item = async_fn(&mut w, "moved_before_await", "moved_before_await_future");
    w.fx.finish();
    let mut b = async_body();
    let lits = StdVec::new();
    // let mut d = mk_d();
    let d = b.bind("d");
    let mk0 = b.path(&["mk_d"]);
    let call_mk0 = b.call(mk0, &[]);
    let s0 = b.let_(d, None, call_mk0);
    // if false { d = mk_d(); }
    let cond = bool_lit(&mut b, false);
    let dl0 = b.local(d);
    let mk1 = b.path(&["mk_d"]);
    let call_mk1 = b.call(mk1, &[]);
    let assign = assign_expr(&mut b, dl0, call_mk1);
    let s_assign = b.semi(assign);
    let then_blk = b.block(&[s_assign], None);
    let if_e = if_expr(&mut b, cond, then_blk, None);
    let s1 = b.semi(if_e);
    // consume_d(d);
    let consume = b.path(&["consume_d"]);
    let dl1 = b.local(d);
    let call_consume = b.call(consume, &[dl1]);
    let s2 = b.semi(call_consume);
    // twice().await
    let awaited = call_await(&mut b, "twice");
    let body = b.block(&[s0, s1, s2], Some(awaited));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    // Only the `.await`'s future is saved across the suspension; neither `d` nor its drop flag is saved.
    let saved = [(Identity::Temp(awaited, Role::Awaitee), (16, 3), false)];
    let laid = Laid::new((0, 0), &saved, 1, StatePlant::None);
    let ran = run_laid(&w, &progs, Script::Complete, Some(&laid))
        .unwrap_or_else(|e| panic!("moved flagged local before await failed in laid state: {e}"));
    assert_eq!(ran, Ran { value: 2, polls: 3, drops: 1, made: 1 });
}

/// An `if` returning a `Copy` aggregate before `.await` (whose join uses a shared slot and whose
/// inner local settles out) plus a tuple literal wrapping `.await` `(twice().await, 37)` must not
/// require unnamed slots across the suspension in a laid state.
#[test]
fn aggregate_literal_around_await_and_if_join_before_await() {
    let mut w = open_world();
    let item = async_fn(&mut w, "agg_and_join", "agg_and_join_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    // let pre: (u64, u64) = if true { let inner = (1, 2); inner } else { (3, 4) };
    let cond = bool_lit(&mut b, true);
    let one = lit(&mut b, &mut lits, 1);
    let two_v = lit(&mut b, &mut lits, 2);
    let t_in = b.tuple(&[one, two_v]);
    let inner = b.bind("inner");
    let s_inner = b.let_(inner, None, t_in);
    let inner_l = b.local(inner);
    let then_blk = b.block(&[s_inner], Some(inner_l));
    let three = lit(&mut b, &mut lits, 3);
    let four = lit(&mut b, &mut lits, 4);
    let t_else = b.tuple(&[three, four]);
    let else_blk = b.block(&[], Some(t_else));
    let if_agg = if_expr(&mut b, cond, then_blk, Some(else_blk));
    let pre = b.bind("pre");
    let s0 = b.let_(pre, None, if_agg);
    // let base: u64 = pre.0 + pre.1;  (pre is dead before `.await`; only `base` crosses)
    let p0 = b.local(pre);
    let f0 = tuple_field(&mut b, p0, 0);
    let p1 = b.local(pre);
    let f1 = tuple_field(&mut b, p1, 1);
    let sum0 = bin(&mut b, BinOp::Add, f0, f1);
    let (s1, base) = let_u64(&mut b, "base", sum0);
    // let pair = (twice().await, 37);
    let awaited = call_await(&mut b, "twice");
    let k37 = lit(&mut b, &mut lits, 37);
    let pair_e = b.tuple(&[awaited, k37]);
    let pair = b.bind("pair");
    let s2 = b.let_(pair, None, pair_e);
    // base + pair.0 + pair.1 == 3 + 2 + 37 == 42
    let bl = b.local(base);
    let pl0 = b.local(pair);
    let pf0 = tuple_field(&mut b, pl0, 0);
    let pl1 = b.local(pair);
    let pf1 = tuple_field(&mut b, pl1, 1);
    let s_ab = bin(&mut b, BinOp::Add, bl, pf0);
    let total = bin(&mut b, BinOp::Add, s_ab, pf1);
    let body = b.block(&[s0, s1, s2], Some(total));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    // Only `base` and `awaited` cross the suspension — neither `pre`, `inner`, `if_agg`, nor `pair_e`!
    let saved = [
        (Identity::Binding(base), (8, 3), false),
        (Identity::Temp(awaited, Role::Awaitee), (16, 3), false),
    ];
    let laid = Laid::new((0, 0), &saved, 1, StatePlant::None);
    let ran = run_laid(&w, &progs, Script::Complete, Some(&laid))
        .unwrap_or_else(|e| panic!("agg literal / if join around await failed in laid state: {e}"));
    assert_eq!(ran, Ran { value: 42, polls: 3, drops: 0, made: 1 });
}

/// A dead `Copy` (`LocalOwn::Kept`) aggregate binding whose address escaped to an `sret` call
/// (`mk_big`) and a by-address call (`sum_big`) before `.await` must end its slot before
/// suspension when not saved in `StateNames` and never read again.
#[test]
fn dead_copy_sret_local_before_await_ends_its_slot() {
    let mut w = open_world();
    let item = async_fn(&mut w, "dead_copy_sret", "dead_copy_sret_future");
    w.fx.finish();
    let mut b = async_body();
    let lits = StdVec::new();
    // let big = mk_big();
    let big = b.bind("big");
    let mk_p = b.path(&["mk_big"]);
    let mk_c = b.call(mk_p, &[]);
    let s0 = b.let_(big, None, mk_c);
    // let base = sum_big(big);
    let sum_p = b.path(&["sum_big"]);
    let big_l = b.local(big);
    let sum_c = b.call(sum_p, &[big_l]);
    let (s1, base) = let_u64(&mut b, "base", sum_c);
    // let r = twice().await; base + r
    let awaited = call_await(&mut b, "twice");
    let (s2, r) = let_u64(&mut b, "r", awaited);
    let base_l = b.local(base);
    let r_l = b.local(r);
    let total = bin(&mut b, BinOp::Add, base_l, r_l);
    let body = b.block(&[s0, s1, s2], Some(total));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    // Only `base` and `awaited` cross the suspension — `big` is dead before `.await`!
    let saved = [
        (Identity::Binding(base), (8, 3), false),
        (Identity::Temp(awaited, Role::Awaitee), (16, 3), false),
    ];
    let laid = Laid::new((0, 0), &saved, 1, StatePlant::None);
    let ran = run_laid(&w, &progs, Script::Complete, Some(&laid))
        .unwrap_or_else(|e| panic!("dead Copy sret local before await failed in laid state: {e}"));
    assert_eq!(ran, Ran { value: 42, polls: 3, drops: 0, made: 1 });
}

/// A unit enum variant (`Tag::B`, lowered as `At::Const` with no flow local) evaluated as an
/// argument before `.await` (`tag_plus(Tag::B, twice().await)`) must end its pre-await scratch
/// slot and re-materialize on the Ready edge without requiring a state slot.
#[test]
fn unit_enum_variant_across_await_rematerializes_without_state_slot() {
    let mut w = open_world();
    let item = async_fn(&mut w, "unit_enum_across", "unit_enum_across_future");
    w.fx.finish();
    let mut b = async_body();
    let lits = StdVec::new();
    // tag_plus(Tag::B, twice().await)
    let callee = b.path(&["tag_plus"]);
    let tag_b = b.path(&["Tag", "B"]);
    let awaited = call_await(&mut b, "twice");
    let call = b.call(callee, &[tag_b, awaited]);
    let body = b.block(&[], Some(call));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    // Only the `.await`'s future is in `StateNames`; `Tag::B` has no flow local.
    let saved = [(Identity::Temp(awaited, Role::Awaitee), (16, 3), false)];
    let laid = Laid::new((0, 0), &saved, 1, StatePlant::None);
    let ran = run_laid(&w, &progs, Script::Complete, Some(&laid))
        .unwrap_or_else(|e| panic!("unit enum variant across await failed in laid state: {e}"));
    assert_eq!(ran, Ran { value: 12, polls: 3, drops: 0, made: 1 });
}

/// A partially-moved `Drop` binding across `.await` (`let pair = (mk_d(), mk_d()); consume_d(pair.0);
/// twice().await`) places its 8-byte `rest` field-flags word inside the laid state region so both
/// normal completion and cancellation drop only the remaining unmoved field (`pair.1`).
#[test]
fn partially_moved_local_across_await_keeps_rest_word_in_state() {
    let mut w = open_world();
    let item = async_fn(&mut w, "partial_move_across", "partial_move_across_future");
    w.fx.finish();
    let mut b = async_body();
    let lits = StdVec::new();
    // let pair = (mk_d(), mk_d());
    let mk0 = b.path(&["mk_d"]);
    let d0 = b.call(mk0, &[]);
    let mk1 = b.path(&["mk_d"]);
    let d1 = b.call(mk1, &[]);
    let tup = b.tuple(&[d0, d1]);
    let pair = b.bind("pair");
    let s0 = b.let_(pair, None, tup);
    // consume_d(pair.0);
    let consume = b.path(&["consume_d"]);
    let pair_l = b.local(pair);
    let f0 = tuple_field(&mut b, pair_l, 0);
    let call_c = b.call(consume, &[f0]);
    let s1 = b.semi(call_c);
    // twice().await
    let awaited = call_await(&mut b, "twice");
    let body = b.block(&[s0, s1], Some(awaited));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    let saved_sizes = [(16u64, 3u8), (16u64, 3u8)];
    let needs_flag = [true, false];
    let needs_rest = [true, false];
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let l = lay_state_with_flags((0, 0), &saved_sizes, &needs_flag, &needs_rest, &[], &[], 1, a, a)
        .unwrap_or_else(|e| panic!("lay_state_with_flags: {e}"));
    assert!(l.rest[0].is_some_and(|w| w % 8 == 0 && w >= l.state.region.start && w + 8 <= l.state.region.end));
    let mut rows = StdVec::from([
        StateName { identity: Identity::Binding(pair), offset: l.field[0], size: 16, flag: l.flag[0], rest: l.rest[0] },
        StateName { identity: Identity::Temp(awaited, Role::Awaitee), offset: l.field[1], size: 16, flag: l.flag[1], rest: l.rest[1] },
    ]);
    rows.sort_unstable_by_key(|r| r.identity);
    let laid = Laid { laid: l.state, rows, plant: StatePlant::None };

    let done = run_laid(&w, &progs, Script::Complete, Some(&laid))
        .unwrap_or_else(|e| panic!("partially moved local across await failed on completion: {e}"));
    assert_eq!(done, Ran { value: 2, polls: 3, drops: 2, made: 1 });

    let cancelled = run_laid(&w, &progs, Script::CancelAfter(1), Some(&laid))
        .unwrap_or_else(|e| panic!("partially moved local across await failed on cancel: {e}"));
    assert_eq!(cancelled, Ran { value: u64::MAX, polls: 1, drops: 2, made: 1 });
}
