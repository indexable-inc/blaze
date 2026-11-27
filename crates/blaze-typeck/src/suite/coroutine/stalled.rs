//! Auto-trait goals of an async block's future decided on its exact interior (src/stalled.rs;
//! async plan P13). The world and bodies are the parent module's (`spawn<F: Send>`, `Rc` not
//! `Send`, `Ready` a future). The exact interiors here are written by hand, as blaze-flow-lower's
//! `interior` gives them (its tests prove those: coroutine_tests.rs and coroutine_tests/send.rs,
//! which runs the whole path).
//!
//! The rule: the Reference, "Auto traits": an async block's future is `Send` when every value
//! its state holds is, and the state is "roughly equivalent to an enum with one variant per await
//! point, where each variant stores the data needed to resume from its corresponding point"
//! ("Async blocks"): its captures and the values live across an `.await`.

use std::vec::Vec;

use super::{Items, rc_across_await, rc_before_await, world};
use crate::body::Capture;
use crate::diag::{Diag, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::ExprId;
use crate::infer::InferCtx;
use crate::portable::{PNode, PTy, PortableTys};
use crate::stalled::{StalledError, check_deferring, decide_stalled};
use crate::world::Needs;

/// `fn c_sync() { spawn(async { inner }); }` checked deferring: the fixture, the body, the
/// block and the record.
struct Case {
    fx: Fx,
    b: Bb,
    owner: crate::ids::ItemKey,
    root: ExprId,
    block: ExprId,
    c: crate::columns::Checked<'static>,
}

fn case(inner: fn(&mut Bb) -> ExprId) -> Case {
    let alloc = blaze_own::testing::heap().alloc();
    let (fx, it): (Fx, Items) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = inner(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let mut cx = InferCtx::new_in(alloc);
    let needs = Needs::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &needs, alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    Case { fx, b, owner: it.c_sync, root, block, c }
}

/// The exact interior `(T0, T1, ..)` of `k`'s block in a fork of the record's table: `rc` adds
/// `Rc`, and the awaited `Ready` is always held (blaze-flow-lower coroutine_tests.rs
/// `a_local_live_across_the_await_is_in_the_interior`: "the live Rc<i32> and the awaited Ready").
fn exact(k: &Case, rc: bool) -> (PortableTys<'static>, PTy) {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = k.c.types.fork_in(alloc).expect("fork the record's table");
    let none = out.intern_list(&[]).expect("list");
    let ready = out.intern(PNode::Adt(k.fx.key_of("Ready"), none)).expect("Ready");
    let mut elems: Vec<PTy> = Vec::new();
    if rc {
        elems.push(out.intern(PNode::Adt(k.fx.key_of("Rc"), none)).expect("Rc"));
    }
    elems.push(ready);
    let list = out.intern_list(&elems).expect("list");
    let t = out.intern(PNode::Tuple(list)).expect("tuple");
    (out, t)
}

fn decide(k: &Case, out: &PortableTys<'_>, rows: &[(ExprId, PTy)]) -> Result<Vec<Diag>, StalledError> {
    let alloc = blaze_own::testing::heap().alloc();
    let got = decide_stalled(&k.b.view(k.owner, k.root), &k.c, out.view(), rows, &k.fx, alloc)?;
    Ok(got.iter().copied().collect())
}

/// `async { let r = make_rc(); ready().await; use_rc(r) }` passed to `spawn<F: Send>`: `r` is
/// live across the `.await`, so `Rc` is in the exact interior and the future is not `Send`:
/// E0277, at the place the check gives the unmet goal (the stalled row's own finding).
///
/// Expected line: `test suite::coroutine::stalled::rc_held_across_await_is_not_send ... ok`.
#[test]
fn rc_held_across_await_is_not_send() {
    let k = case(rc_across_await);
    assert!(k.c.diags.is_empty(), "nothing is decided before the flow: {:?}", k.c.diags);
    assert_eq!(k.c.stalled.len(), 1, "one stalled goal: the future's `Send`");
    let (out, t) = exact(&k, true);
    let got = decide(&k, &out, &[(k.block, t)]).expect("decided");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].code, DiagCode::Unsatisfied);
    assert_eq!(got[0], k.c.stalled[0].diag, "the finding is placed where the check placed it");
    assert!(got[0].at.end > got[0].at.start, "E0277 has a place");
}

/// `async { use_rc(make_rc()); ready().await; }`: the `Rc` is gone before the `.await`, so the
/// exact interior holds only the awaited `Ready` and the Reference accepts the `spawn`.
/// Without `decide_stalled` reading the exact interior this was the gap "an auto trait (`Send`,
/// `Sync`) of an async block's future that fails only on a value not known to be live across an
/// `.await`".
///
/// Expected line: `test suite::coroutine::stalled::rc_dropped_before_await_is_send ... ok`.
#[test]
fn rc_dropped_before_await_is_send() {
    let k = case(rc_before_await);
    assert_eq!(k.c.stalled.len(), 1, "the superset holds `Rc`: the goal stalls");
    let (out, t) = exact(&k, false);
    let got = decide(&k, &out, &[(k.block, t)]).expect("decided");
    assert!(got.is_empty(), "no finding on the exact interior: {got:?}");
}

/// A missing predicate record during replay supplies empty stand-ins for that
/// round. Even an error from a later solver read must yield to the missing record.
#[test]
fn replay_requests_the_missing_record_before_deciding_send() {
    let k = case(rc_before_await);
    let (out, t) = exact(&k, false);
    let alloc = blaze_own::testing::heap().alloc();
    let rows = [(k.block, t)];
    let mut withheld = crate::toy::ToyWorld::new(&[], &[], crate::toy::FieldTy::U8).unwrap();
    let question = crate::Question::Predicates(k.owner);
    withheld.missing.push(question);
    let got = decide_stalled(&k.b.view(k.owner, k.root), &k.c, out.view(), &rows, &withheld, alloc);
    assert!(matches!(got, Err(StalledError::Need(q)) if q == question), "{got:?}");
    assert!(decide(&k, &out, &rows).expect("the complete world supplies the record").is_empty());
}

/// Control: deciding on the SUPERSET interior (the row's own future's) instead of the exact one
/// refuses the accepted body: the decision reads the interior it is given. And a future with no
/// exact interior passed is refused by name, never decided.
///
/// Expected line: `test suite::coroutine::stalled::control_superset_refuses_and_missing_is_named ... ok`.
#[test]
fn control_superset_refuses_and_missing_is_named() {
    let k = case(rc_before_await);
    let row = k.c.stalled[0];
    let superset = match k.c.types.view().node(row.ty).expect("node") {
        PNode::Future(e, _, i) => (e, i),
        other => panic!("the stalled type is the future, got {other:?}"),
    };
    assert_eq!(superset.0, k.block);
    let alloc = blaze_own::testing::heap().alloc();
    let out = k.c.types.fork_in(alloc).expect("fork");
    let got = decide(&k, &out, &[superset]).expect("decided");
    assert_eq!(got.len(), 1, "the superset holds `Rc`: {got:?}");
    assert_eq!(decide(&k, &out, &[]), Err(StalledError::NoExactInterior { block: k.block }));
}

/// `async { let r = make_rc(); ready().await; use_rc(r) }` decided on its exact interior `(Rc,
/// Ready)` with the chain (`decide_stalled_with_storage`): the finding names rows of the
/// decision's own column, root first: the future's `Send`; slot 0 of the exact tuple the caller
/// passed (`Rc: Send`, the slot front2 says as the value `r`); the where-clause of `impl Send for
/// Rc where Rc: Never` (`Rc: Never`), which no impl names. The twin, `Rc` dropped before the
/// `.await`, has no finding and no row. The chainless entry (`decide_stalled`) gives the same
/// finding with no `why`.
///
/// Expected line: `test suite::coroutine::stalled::a_stalled_finding_chains_on_the_exact_interior ... ok`.
#[test]
fn a_stalled_finding_chains_on_the_exact_interior() {
    use crate::diag::{UnmetGoal, WhyDef, WhyLeaf, WhyReached};
    let k = case(rc_across_await);
    let (out, t) = exact(&k, true);
    let alloc = blaze_own::testing::heap().alloc();
    let view = k.b.view(k.owner, k.root);
    let got = k.c.decide_stalled_with_storage(&view, &out, &[(k.block, t)], &k.fx, alloc, |_, _, _| {}).expect("decided");
    assert_eq!(got.found.len(), 1, "{:?}", got.found);
    let w = got.found[0].why.expect("the stalled finding carries its chain");
    let steps = got.why.get(w.start as usize..(w.start + w.len) as usize).expect("the chain's rows in the decision's column");
    let tys = got.types.view();
    let said: Vec<std::string::String> = steps
        .iter()
        .map(|s| match s.goal {
            Some(UnmetGoal::Trait { trait_, args }) => {
                std::format!("{}: {}", crate::fixture::show(&k.fx, tys, tys.list(args).expect("args")[0]), k.fx.name_of(trait_))
            }
            other => std::format!("{other:?}"),
        })
        .collect();
    assert_eq!(steps.len(), 3, "{said:?}");
    assert_eq!(steps[0].reached, WhyReached::Root, "{said:?}");
    assert_eq!(steps[1].reached, WhyReached::CoroutineSlot { coroutine: WhyDef::Expr(k.block), slot: 0 }, "{said:?}");
    assert_eq!(said[1], "Rc: Send", "{said:?}");
    let imp = k.fx.key_of("impl Send for Rc where Rc: Never");
    assert_eq!(steps[2].reached, WhyReached::ImplClause { impl_: imp, clause: 0 }, "{said:?}");
    assert_eq!(said[2], "Rc: Never", "{said:?}");
    assert_eq!(w.leaf, WhyLeaf::NoImpl, "{said:?}");
    // The chainless entry: the same finding, no chain.
    let plain = decide(&k, &out, &[(k.block, t)]).expect("decided");
    assert_eq!(plain.len(), 1, "{plain:?}");
    assert_eq!(plain[0].why, None, "{plain:?}");
    let mut chainless = got.found[0];
    chainless.why = None;
    assert_eq!(chainless, plain[0], "the same finding but its chain");
    // The twin: nothing fails on the exact interior, so no finding and no row.
    let twin = case(rc_before_await);
    let (twin_out, twin_t) = exact(&twin, false);
    let twin_view = twin.b.view(twin.owner, twin.root);
    let none = twin.c.decide_stalled_with_storage(&twin_view, &twin_out, &[(twin.block, twin_t)], &twin.fx, alloc, |_, _, _| {}).expect("decided");
    assert!(none.found.is_empty() && none.why.is_empty(), "{:?} {:?}", none.found, none.why);
}

/// The entries that do not defer keep the typed gap: no caller that never decides a stalled
/// goal gets a pass (the parent module's `rc_held_across_await_is_a_gap` is the same claim
/// through `check_with`).
///
/// Expected line: `test suite::coroutine::stalled::check_keeps_the_gap ... ok`.
#[test]
fn check_keeps_the_gap() {
    let alloc = blaze_own::testing::heap().alloc();
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = rc_before_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let mut cx = InferCtx::new_in(alloc);
    let got = crate::check(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc);
    assert!(
        matches!(got, Err(crate::error::CheckError::FutureWitness { reason, at }) if reason == crate::INTERIOR_GAP && at.start < at.end),
        "{:?}",
        got.as_ref().err()
    );
}

/// Regression (Codex PR #2770 P2 #1): a deep projection obligation that stalls on a local async
/// block's interior and then selects cleanly on the exact interior but normalizes to a different
/// type than its required term must emit E0271 (`DiagCode::ProjectionMismatch`) with `found` and
/// `expected`, rather than collapsing into E0277 (`DiagCode::Unsatisfied`).
#[test]
fn stalled_projection_mismatch_emits_e0271_on_exact_replay() {
    use crate::ids::{Name, UintTy};
    let alloc = blaze_own::testing::heap().alloc();
    let (mut fx, it) = world();
    let (p0, u32_, u64_, unit) = (
        fx.param(0),
        fx.ty(PNode::Uint(UintTy::U32)),
        fx.ty(PNode::Uint(UintTy::U64)),
        fx.unit(),
    );
    let send = fx.key_of("Send");
    let inner = fx.tr("Inner", 1, Vec::new(), &["Out"]);
    let is_send = fx.bound(send, &[p0]);
    fx.imp("impl<F: Send> Inner for F", 1, p0, Some((inner, &[])), &[("Out", u32_)], std::vec![is_send]);
    let outer = fx.tr("Outer", 1, Vec::new(), &["Assoc"]);
    let wrap = fx.strukt("Wrap", 1, &[("v", p0)]);
    let wrap_p0 = fx.adt(wrap, &[p0]);
    let is_inner = fx.bound(inner, &[p0]);
    let inner_out = fx.alias(inner, "Out", &[p0]);
    fx.imp(
        "impl<T: Inner> Outer for Wrap<T>",
        1,
        wrap_p0,
        Some((outer, &[])),
        &[("Assoc", inner_out)],
        std::vec![is_inner],
    );
    let wrap_fn = fx.func("wrap", 1, &[p0], wrap_p0, Vec::new());
    let _ = wrap_fn;
    let is_outer = fx.bound(outer, &[p0]);
    let assoc_is_u64 = fx.proj(outer, &[p0], "Assoc", u64_);
    fx.func("need_u64", 1, &[p0], unit, std::vec![is_outer, assoc_is_u64]);
    fx.finish();

    let mut b = Bb::new();
    let body = rc_before_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let w_callee = b.path(&["wrap"]);
    let wrapped = b.call(w_callee, &[block]);
    let callee = b.path(&["need_u64"]);
    let call = b.call(callee, &[wrapped]);
    let s = b.semi(call);
    let root = b.block(&[s], None);

    let mut cx = InferCtx::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "stalled before replay: {:?}", c.diags);
    assert!(c.stalled.len() >= 2, "expected Outer trait + Assoc projection stalled: {:?}", c.stalled);

    // Exact interior without Rc: `F: Send` holds, `Outer` holds, and `<Wrap<F> as Outer>::Assoc`
    // normalizes cleanly to `u32` != `u64` -> E0271 with found=`u32`, expected=`u64`.
    let mut out = c.types.fork_in(alloc).expect("fork");
    let none = out.intern_list(&[]).expect("list");
    let ready = out.intern(PNode::Adt(fx.key_of("Ready"), none)).expect("Ready");
    let list = out.intern_list(&[ready]).expect("list");
    let exact_ok = out.intern(PNode::Tuple(list)).expect("tuple");
    let rc = out.intern(PNode::Adt(fx.key_of("Rc"), none)).expect("Rc");
    let bad_list = out.intern_list(&[rc, ready]).expect("list");
    let exact_bad = out.intern(PNode::Tuple(bad_list)).expect("tuple");
    let view = b.view(it.c_sync, root);
    let decided = c.decide_stalled_with_storage(&view, &out, &[(block, exact_ok)], &fx, alloc, |_, _, _| {}).expect("decided");
    assert_eq!(decided.found.len(), 1, "only the projection mismatch should fail: {:?}", decided.found);
    let d = decided.found[0];
    assert_eq!(d.code, DiagCode::ProjectionMismatch, "expected E0271, got {d:?}");
    assert_eq!(d.name, Some(Name::of("Assoc")), "{d:?}");
    let tys = decided.types.view();
    assert_eq!(d.found.map(|t| crate::fixture::show(&fx, tys, t)), Some(std::string::String::from("u32")), "{d:?}");
    assert_eq!(d.expected.map(|t| crate::fixture::show(&fx, tys, t)), Some(std::string::String::from("u64")), "{d:?}");

    // Negative twin: exact interior with Rc -> `F: Send` fails, so BOTH `Wrap<F>: Outer` and
    // `<Wrap<F> as Outer>::Assoc == u64` fail as E0277 at the same site; `drop_implied_projections`
    // must suppress the implied projection so only the single trait E0277 is emitted (Codex R2 P2 #3).
    let failed = c.decide_stalled_with_storage(&view, &out, &[(block, exact_bad)], &fx, alloc, |_, _, _| {}).expect("decided");
    assert_eq!(
        failed.found.len(),
        1,
        "implied projection must be suppressed when its trait fails at the same site: {:?}",
        failed.found
    );
    assert_eq!(failed.found[0].code, DiagCode::Unsatisfied, "{:?}", failed.found);
    assert!(
        matches!(
            failed.found[0].because,
            Some(crate::diag::Because::Unmet { goal: crate::diag::UnmetGoal::Trait { trait_, .. }, .. }) if trait_ == outer
        ),
        "remaining finding must be the owning Outer trait goal: {:?}",
        failed.found
    );
}

/// Regression (Codex PR #2770 P2 #2): when a stalled multi-argument trait goal fails during
/// replay because a non-`Self` argument holds a foreign `Superset` coroutine, `decide` must
/// scan every root goal argument and return `EXTERNAL_WITNESS_GAP` (`CheckError::ForeignWitness`),
/// never a false E0277.
#[test]
fn non_self_foreign_superset_witness_returns_gap_not_e0277() {
    let alloc = blaze_own::testing::heap().alloc();
    let (mut fx, it) = world();
    let (p0, p1, u8_, unit) = (fx.param(0), fx.param(1), fx.u8(), fx.unit());
    let sized = fx.key_of("Sized");
    let send = fx.key_of("Send");
    let rc_t = fx.adt(fx.key_of("Rc"), &[]);
    let send_wrap = fx.tr("SendWrap", 1, Vec::new(), &[]);
    let p0_send = fx.bound(send, &[p0]);
    fx.imp("impl<T: Send> SendWrap for T", 1, p0, Some((send_wrap, &[])), &[], std::vec![p0_send]);
    // `trait Pair<R> { type Out; }` with `impl<R: SendWrap> Pair<R> for u8 { type Out = u8; }`:
    // `Self` is plain `u8`, while `args[1]` (`R`) carries `(LocalFuture, ForeignFut)`.
    let pair = fx.tr("Pair", 2, Vec::new(), &["Out"]);
    let p1_wrap = fx.bound(send_wrap, &[p1]);
    fx.imp("impl<R: SendWrap> Pair<R> for u8", 2, u8_, Some((pair, &[p1])), &[("Out", u8_)], std::vec![p1_wrap]);
    let foreign_fn = fx.func("foreign_fn", 0, &[], unit, Vec::new());
    let foreign_op = fx.opaque_key("ForeignFut");
    let foreign_op_t = fx.ty(PNode::Opaque(foreign_op, crate::ids::List::EMPTY));
    let foreign_sized = fx.bound(sized, &[foreign_op_t]);
    fx.set_opaque_bounds(foreign_op, std::vec![foreign_sized]);
    fx.func("make_foreign", 0, &[], foreign_op_t, Vec::new());
    let pair_proj = fx.proj(pair, &[p0, p1], "Out", u8_);
    fx.func("check_pair", 2, &[p0, p1], unit, std::vec![pair_proj]);
    fx.finish();

    let mut b = Bb::new();
    let body = rc_before_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let mf = b.path(&["make_foreign"]);
    let foreign_val = b.call(mf, &[]);
    let both = b.tuple(&[block, foreign_val]);
    let u8_val = b.int(Some(crate::body::IntSuffix::Uint(crate::ids::UintTy::U8)));
    let callee = b.path(&["check_pair"]);
    let call = b.call(callee, &[u8_val, both]);
    let s = b.semi(call);
    let root = b.block(&[s], None);

    let mut cx = InferCtx::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(c.stalled.len(), 1, "expected one stalled Pair row: {:?}", c.stalled);
    assert_eq!(c.types.view().node(c.stalled[0].ty), Ok(PNode::Uint(crate::ids::UintTy::U8)), "Self is u8");

    // Case 2a (opaque in non-Self arg revealed to a Superset foreign coroutine):
    let cor_t = fx.ty(PNode::Coroutine { owner: foreign_fn, ordinal: 0, args: crate::ids::List::EMPTY });
    let superset_interior = fx.tuple(&[rc_t]);
    fx.hidden.push((foreign_op, cor_t));
    fx.coroutines.push((foreign_fn, 0, unit, superset_interior));
    fx.provisional_coroutines.push((foreign_fn, 0));

    let mut out = c.types.fork_in(alloc).expect("fork");
    let none = out.intern_list(&[]).expect("list");
    let ready = out.intern(PNode::Adt(fx.key_of("Ready"), none)).expect("Ready");
    let list = out.intern_list(&[ready]).expect("list");
    let exact_ok = out.intern(PNode::Tuple(list)).expect("tuple");
    let got = decide_stalled(&b.view(it.c_sync, root), &c, out.view(), &[(block, exact_ok)], &fx, alloc);
    assert!(
        matches!(
            got,
            Err(StalledError::Check(crate::error::CheckError::ForeignWitness {
                gap,
                item,
                ordinal: Some(0),
                ..
            })) if gap == crate::EXTERNAL_WITNESS_GAP && item == foreign_fn
        ),
        "expected EXTERNAL_WITNESS_GAP naming foreign_fn #0 (opaque non-Self arg), got {got:?}"
    );

    // Case 2b (direct foreign Coroutine in non-Self arg whose replay imports Superset `(Rc,)`
    // and produces a definitive solver `No`): `unavailable_goal_witness` must scan `args[1]`
    // and return `EXTERNAL_WITNESS_GAP`, never a false E0277.
    let (mut fx2, it2) = world();
    let (p0, p1, u8_, unit) = (fx2.param(0), fx2.param(1), fx2.u8(), fx2.unit());
    let send = fx2.key_of("Send");
    let rc_t = fx2.adt(fx2.key_of("Rc"), &[]);
    let ready_t = fx2.adt(fx2.key_of("Ready"), &[]);
    let send_wrap = fx2.tr("SendWrap", 1, Vec::new(), &[]);
    let p0_send = fx2.bound(send, &[p0]);
    fx2.imp("impl<T: Send> SendWrap for T", 1, p0, Some((send_wrap, &[])), &[], std::vec![p0_send]);
    let pair = fx2.tr("Pair", 2, Vec::new(), &[]);
    let p1_wrap = fx2.bound(send_wrap, &[p1]);
    fx2.imp("impl<R: SendWrap> Pair<R> for u8", 2, u8_, Some((pair, &[p1])), &[], std::vec![p1_wrap]);
    let foreign_fn2 = fx2.func("foreign_fn2", 0, &[], unit, Vec::new());
    let cor2_t = fx2.ty(PNode::Coroutine { owner: foreign_fn2, ordinal: 0, args: crate::ids::List::EMPTY });
    let exact_foreign = fx2.tuple(&[ready_t]);
    let superset_foreign = fx2.tuple(&[rc_t]);
    fx2.coroutines.push((foreign_fn2, 0, unit, exact_foreign));
    fx2.func("make_foreign_cor", 0, &[], cor2_t, Vec::new());
    let pair_bound = fx2.bound(pair, &[p0, p1]);
    fx2.func("check_pair2", 2, &[p0, p1], unit, std::vec![pair_bound]);
    fx2.finish();

    let mut b2 = Bb::new();
    let body2 = rc_before_await(&mut b2);
    let block2 = b2.async_block(body2, Capture::Ref);
    let mf2 = b2.path(&["make_foreign_cor"]);
    let foreign_cor = b2.call(mf2, &[]);
    let both2 = b2.tuple(&[block2, foreign_cor]);
    let u8_val2 = b2.int(Some(crate::body::IntSuffix::Uint(crate::ids::UintTy::U8)));
    let callee2 = b2.path(&["check_pair2"]);
    let call2 = b2.call(callee2, &[u8_val2, both2]);
    let s2 = b2.semi(call2);
    let root2 = b2.block(&[s2], None);

    let mut cx2 = InferCtx::new_in(alloc);
    let c2 = match check_deferring(&b2.view(it2.c_sync, root2), &fx2, &mut cx2, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c2.diags.is_empty(), "{:?}", c2.diags);
    assert_eq!(c2.stalled.len(), 1, "expected one stalled Pair row: {:?}", c2.stalled);
    assert_eq!(c2.types.view().node(c2.stalled[0].ty), Ok(PNode::Uint(crate::ids::UintTy::U8)), "Self is u8");

    // Flip `foreign_fn2 #0` to a `Superset` record holding `Rc`: `Exact::coroutine_of` imports
    // `(Rc,)` so replay returns `Unsatisfied` (`No`), which `unavailable_goal_witness` must
    // convert to `EXTERNAL_WITNESS_GAP` rather than reporting E0277.
    fx2.coroutines[0].3 = superset_foreign;
    fx2.provisional_coroutines.push((foreign_fn2, 0));

    let mut out2 = c2.types.fork_in(alloc).expect("fork");
    let none2 = out2.intern_list(&[]).expect("list");
    let ready2 = out2.intern(PNode::Adt(fx2.key_of("Ready"), none2)).expect("Ready");
    let list2 = out2.intern_list(&[ready2]).expect("list");
    let exact_ok2 = out2.intern(PNode::Tuple(list2)).expect("tuple");
    let got2 = decide_stalled(&b2.view(it2.c_sync, root2), &c2, out2.view(), &[(block2, exact_ok2)], &fx2, alloc);
    assert!(
        matches!(
            got2,
            Err(StalledError::Check(crate::error::CheckError::ForeignWitness {
                gap,
                item,
                ordinal: Some(0),
                ..
            })) if gap == crate::EXTERNAL_WITNESS_GAP && item == foreign_fn2
        ),
        "expected EXTERNAL_WITNESS_GAP naming foreign_fn2 #0 (direct Superset Coroutine non-Self arg), got {got2:?}"
    );
}

/// Regression (Codex PR #2770 P2 #3): an ordinary two-argument trait whose second argument
/// contains `dyn Send` must NOT run `unsize_storage` on replay failure and misattribute the
/// failure to a held coroutine field via the `failed_field` callback.
#[test]
fn ordinary_two_arg_trait_with_dyn_send_does_not_run_unsize_storage() {
    let alloc = blaze_own::testing::heap().alloc();
    let (mut fx, it) = world();
    let (p0, p1, unit) = (fx.param(0), fx.param(1), fx.unit());
    let send = fx.key_of("Send");
    let send_wrap = fx.tr("SendWrap", 1, Vec::new(), &[]);
    let p0_send = fx.bound(send, &[p0]);
    fx.imp("impl<T: Send> SendWrap for T", 1, p0, Some((send_wrap, &[])), &[], std::vec![p0_send]);
    let holder = fx.strukt("Holder", 1, &[("f", p0)]);
    let holder_p0 = fx.adt(holder, &[p0]);
    let outer2 = fx.tr("Outer2", 2, Vec::new(), &[]);
    let p0_wrap = fx.bound(send_wrap, &[p0]);
    fx.imp("impl<F: SendWrap, T> Outer2<T> for Holder<F>", 2, holder_p0, Some((outer2, &[p1])), &[], std::vec![p0_wrap]);
    fx.func("hold", 1, &[p0], holder_p0, Vec::new());
    let ex_send = fx.ty(PNode::ExAuto(send));
    let dyn_rows = fx.list(&[ex_send]);
    let dyn_send = fx.ty(PNode::Dynamic(dyn_rows));
    let ref_dyn_send = fx.ty(PNode::Ref(crate::ids::Mutbl::Not, dyn_send));
    fx.func("make_dyn_send", 0, &[], ref_dyn_send, Vec::new());
    let outer2_bound = fx.bound(outer2, &[p0, p1]);
    fx.func("check_outer2", 2, &[p0, p1], unit, std::vec![outer2_bound]);
    fx.finish();

    let mut b = Bb::new();
    let body = rc_across_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let hold = b.path(&["hold"]);
    let held = b.call(hold, &[block]);
    let md = b.path(&["make_dyn_send"]);
    let dyn_val = b.call(md, &[]);
    let callee = b.path(&["check_outer2"]);
    let call = b.call(callee, &[held, dyn_val]);
    let s = b.semi(call);
    let root = b.block(&[s], None);

    let mut cx = InferCtx::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(c.stalled.len(), 1, "expected one stalled Outer2 row: {:?}", c.stalled);

    let mut out = c.types.fork_in(alloc).expect("fork");
    let none = out.intern_list(&[]).expect("list");
    let rc = out.intern(PNode::Adt(fx.key_of("Rc"), none)).expect("Rc");
    let ready = out.intern(PNode::Adt(fx.key_of("Ready"), none)).expect("Ready");
    let bad_list = out.intern_list(&[rc, ready]).expect("list");
    let exact_bad = out.intern(PNode::Tuple(bad_list)).expect("tuple");
    let view = b.view(it.c_sync, root);
    let mut storage_callbacks = 0usize;
    let decided = c
        .decide_stalled_with_storage(&view, &out, &[(block, exact_bad)], &fx, alloc, |_, _, _| {
            storage_callbacks += 1;
        })
        .expect("decided");
    assert_eq!(decided.found.len(), 1, "{:?}", decided.found);
    assert_eq!(decided.found[0].code, DiagCode::Unsatisfied);
    assert_eq!(storage_callbacks, 0, "ordinary 2-arg trait must not trigger unsize_storage");
}

/// Regression (Codex PR #2770 R2 P2 #1): a local `!Send` future whose `Output` is a foreign
/// `Superset` opaque/coroutine must still fail definitively with E0277 under `Send` / wrapper
/// `where F: Send`, never `EXTERNAL_WITNESS_GAP`, because a future's `Output` does not affect
/// its auto traits.
#[test]
fn non_send_future_returning_foreign_superset_emits_e0277() {
    let alloc = blaze_own::testing::heap().alloc();
    let (mut fx, it) = world();
    let (p0, unit) = (fx.param(0), fx.unit());
    let sized = fx.key_of("Sized");
    let send = fx.key_of("Send");
    let rc_t = fx.adt(fx.key_of("Rc"), &[]);
    let send_wrap = fx.tr("SendWrap", 1, Vec::new(), &[]);
    let p0_send = fx.bound(send, &[p0]);
    fx.imp("impl<T: Send> SendWrap for T", 1, p0, Some((send_wrap, &[])), &[], std::vec![p0_send]);
    let holder = fx.strukt("Holder", 1, &[("f", p0)]);
    let holder_p0 = fx.adt(holder, &[p0]);
    let outer = fx.tr("Outer", 1, Vec::new(), &[]);
    let p0_wrap = fx.bound(send_wrap, &[p0]);
    fx.imp("impl<F: SendWrap> Outer for Holder<F>", 1, holder_p0, Some((outer, &[])), &[], std::vec![p0_wrap]);
    fx.func("hold", 1, &[p0], holder_p0, Vec::new());
    let outer_bound = fx.bound(outer, &[p0]);
    fx.func("check_outer", 1, &[p0], unit, std::vec![outer_bound]);
    let foreign_fn = fx.func("foreign_fn", 0, &[], unit, Vec::new());
    let foreign_op = fx.opaque_key("ForeignOut");
    let foreign_op_t = fx.ty(PNode::Opaque(foreign_op, crate::ids::List::EMPTY));
    let foreign_sized = fx.bound(sized, &[foreign_op_t]);
    fx.set_opaque_bounds(foreign_op, std::vec![foreign_sized]);
    let cor_t = fx.ty(PNode::Coroutine { owner: foreign_fn, ordinal: 0, args: crate::ids::List::EMPTY });
    let ready_t = fx.adt(fx.key_of("Ready"), &[]);
    let exact_foreign = fx.tuple(&[ready_t]);
    let superset_interior = fx.tuple(&[rc_t]);
    fx.hidden.push((foreign_op, cor_t));
    fx.coroutines.push((foreign_fn, 0, unit, exact_foreign));
    fx.func("make_foreign", 0, &[], foreign_op_t, Vec::new());
    fx.finish();

    // `async { let r = make_rc(); ready().await; use_rc(r); make_foreign() }`
    let mut b = Bb::new();
    let rc_pat = b.bind("r");
    let mk_rc = b.path(&["make_rc"]);
    let rc_val = b.call(mk_rc, &[]);
    let s0 = b.let_(rc_pat, None, rc_val);
    let rdy = b.path(&["ready"]);
    let rdy_call = b.call(rdy, &[]);
    let aw = b.await_(rdy_call);
    let s1 = b.semi(aw);
    let use_rc_fn = b.path(&["use_rc"]);
    let r_local = b.local(rc_pat);
    let use_call = b.call(use_rc_fn, &[r_local]);
    let s2 = b.semi(use_call);
    let mf = b.path(&["make_foreign"]);
    let tail = b.call(mf, &[]);
    let body = b.block(&[s0, s1, s2], Some(tail));
    let block = b.async_block(body, Capture::Ref);
    let hold = b.path(&["hold"]);
    let held = b.call(hold, &[block]);
    let callee = b.path(&["check_outer"]);
    let call = b.call(callee, &[held]);
    let s = b.semi(call);
    let root = b.block(&[s], None);

    let mut cx = InferCtx::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(c.stalled.len(), 1, "{:?}", c.stalled);

    // Flip `ForeignOut`'s hidden coroutine (the future's Output, NOT in its saved interior!)
    // to a foreign Superset holding `Rc`.
    fx.coroutines[0].3 = superset_interior;
    fx.provisional_coroutines.push((foreign_fn, 0));

    let mut out = c.types.fork_in(alloc).expect("fork");
    let none = out.intern_list(&[]).expect("list");
    let rc = out.intern(PNode::Adt(fx.key_of("Rc"), none)).expect("Rc");
    let ready = out.intern(PNode::Adt(fx.key_of("Ready"), none)).expect("Ready");
    let bad_list = out.intern_list(&[rc, ready]).expect("list");
    let exact_bad = out.intern(PNode::Tuple(bad_list)).expect("tuple");
    let got = decide_stalled(&b.view(it.c_sync, root), &c, out.view(), &[(block, exact_bad)], &fx, alloc)
        .expect("must report definitive E0277, not EXTERNAL_WITNESS_GAP");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].code, DiagCode::Unsatisfied, "{got:?}");
}

/// Regression (Codex PR #2770 R2 P2 #2): for a non-auto trait whose impl matches a `fn() -> F`
/// function pointer (`impl<F: SendWrap> OutputSend for fn() -> F`), `unavailable_witness` must
/// walk the `FnPtr` signature so a foreign `Superset` coroutine in the return type yields
/// `EXTERNAL_WITNESS_GAP` instead of a false E0277.
#[test]
fn fn_ptr_signature_foreign_superset_witness_returns_gap() {
    use crate::ids::FnPtrHeader;
    let alloc = blaze_own::testing::heap().alloc();
    let (mut fx, it) = world();
    let (p0, unit) = (fx.param(0), fx.unit());
    let send = fx.key_of("Send");
    let rc_t = fx.adt(fx.key_of("Rc"), &[]);
    let ready_t = fx.adt(fx.key_of("Ready"), &[]);
    let send_wrap = fx.tr("SendWrap", 1, Vec::new(), &[]);
    let p0_send = fx.bound(send, &[p0]);
    fx.imp("impl<T: Send> SendWrap for T", 1, p0, Some((send_wrap, &[])), &[], std::vec![p0_send]);
    let output_send = fx.tr("OutputSend", 1, Vec::new(), &[]);
    let fn_io = fx.list(&[p0]);
    let fn_ptr_p0 = fx.ty(PNode::FnPtr(fn_io, FnPtrHeader::RUST));
    let p0_wrap = fx.bound(send_wrap, &[p0]);
    fx.imp("impl<F: SendWrap> OutputSend for fn() -> F", 1, fn_ptr_p0, Some((output_send, &[])), &[], std::vec![p0_wrap]);
    let foreign_fn = fx.func("foreign_fn", 0, &[], unit, Vec::new());
    let cor_t = fx.ty(PNode::Coroutine { owner: foreign_fn, ordinal: 0, args: crate::ids::List::EMPTY });
    let exact_foreign = fx.tuple(&[ready_t]);
    let superset_foreign = fx.tuple(&[rc_t]);
    fx.coroutines.push((foreign_fn, 0, unit, exact_foreign));
    fx.func("make_foreign_cor", 0, &[], cor_t, Vec::new());
    let ret_tuple = fx.tuple(&[p0, cor_t]);
    let ret_io = fx.list(&[ret_tuple]);
    let ret_fn_ptr = fx.ty(PNode::FnPtr(ret_io, FnPtrHeader::RUST));
    fx.func("as_fn_ptr", 1, &[p0], ret_fn_ptr, Vec::new());
    let out_send_bound = fx.bound(output_send, &[p0]);
    fx.func("check_out_send", 1, &[p0], unit, std::vec![out_send_bound]);
    fx.finish();

    let mut b = Bb::new();
    let body = rc_before_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let afp = b.path(&["as_fn_ptr"]);
    let fp = b.call(afp, &[block]);
    let callee = b.path(&["check_out_send"]);
    let call = b.call(callee, &[fp]);
    let s = b.semi(call);
    let root = b.block(&[s], None);

    let mut cx = InferCtx::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(c.stalled.len(), 1, "{:?}", c.stalled);

    // Flip `foreign_fn #0` inside `fn() -> (LocalFuture, ForeignCor)` to `Superset` holding `Rc`.
    fx.coroutines[0].3 = superset_foreign;
    fx.provisional_coroutines.push((foreign_fn, 0));

    let mut out = c.types.fork_in(alloc).expect("fork");
    let none = out.intern_list(&[]).expect("list");
    let ready = out.intern(PNode::Adt(fx.key_of("Ready"), none)).expect("Ready");
    let list = out.intern_list(&[ready]).expect("list");
    let exact_ok = out.intern(PNode::Tuple(list)).expect("tuple");
    let got = decide_stalled(&b.view(it.c_sync, root), &c, out.view(), &[(block, exact_ok)], &fx, alloc);
    assert!(
        matches!(
            got,
            Err(StalledError::Check(crate::error::CheckError::ForeignWitness {
                gap,
                item,
                ordinal: Some(0),
                ..
            })) if gap == crate::EXTERNAL_WITNESS_GAP && item == foreign_fn
        ),
        "expected EXTERNAL_WITNESS_GAP naming foreign_fn #0 via fn() -> (LocalFuture, ForeignCor), got {got:?}"
    );
}

/// Regression (Codex PR #2770 R2 P2 #4): a replayed projection that normalizes cleanly to a
/// type containing a rigid alias with a composite self type (e.g. `<&T as Ext>::Out` from an
/// owner bound `where &T: Ext`) and disagrees with its required term (`u64`) must preserve E0271
/// (`DiagCode::ProjectionMismatch`), not downgrade to E0277.
#[test]
fn rigid_composite_self_projection_mismatch_emits_e0271() {
    use crate::ids::{Mutbl, Name, UintTy};
    let alloc = blaze_own::testing::heap().alloc();
    let (mut fx, _it) = world();
    let (p0, p1, u64_, unit) = (fx.param(0), fx.param(1), fx.ty(PNode::Uint(UintTy::U64)), fx.unit());
    let sized = fx.key_of("Sized");
    let send = fx.key_of("Send");
    let ext = fx.tr("Ext", 1, Vec::new(), &["Out"]);
    let ext_out_p0 = fx.alias(ext, "Out", &[p0]);
    let ext_out_sized = fx.bound(sized, &[ext_out_p0]);
    fx.set_assoc_bounds(ext, "Out", std::vec![ext_out_sized]);
    let ref_p0 = fx.ty(PNode::Ref(Mutbl::Not, p0));
    let ref_p0_ext = fx.bound(ext, &[ref_p0]);
    let ref_p0_out = fx.alias(ext, "Out", &[ref_p0]);
    // `trait Inner<U> { type Out; }  impl<F: Send, U> Inner<U> for F { type Out = U; }`
    let inner = fx.tr("Inner", 2, Vec::new(), &["Out"]);
    let p0_send = fx.bound(send, &[p0]);
    fx.imp("impl<F: Send, U> Inner<U> for F", 2, p0, Some((inner, &[p1])), &[("Out", p1)], std::vec![p0_send]);
    // `struct Wrap<F, U>(F, U);  trait Outer { type Assoc; }  impl<F: Inner<U>, U> Outer for Wrap<F, U> { type Assoc = <F as Inner<U>>::Out; }`
    let wrap = fx.strukt("Wrap", 2, &[("f", p0), ("u", p1)]);
    let wrap_p0_p1 = fx.adt(wrap, &[p0, p1]);
    let outer = fx.tr("Outer", 1, Vec::new(), &["Assoc"]);
    let p0_inner_p1 = fx.bound(inner, &[p0, p1]);
    let inner_out = fx.alias(inner, "Out", &[p0, p1]);
    fx.imp(
        "impl<F: Inner<U>, U> Outer for Wrap<F, U>",
        2,
        wrap_p0_p1,
        Some((outer, &[])),
        &[("Assoc", inner_out)],
        std::vec![p0_inner_p1],
    );
    let mk_wrap = fx.func("mk_wrap", 2, &[p0, p1], wrap_p0_p1, Vec::new());
    let _ = mk_wrap;
    let is_outer = fx.bound(outer, &[p0]);
    let assoc_is_u64 = fx.proj(outer, &[p0], "Assoc", u64_);
    fx.func("need_u64", 1, &[p0], unit, std::vec![is_outer, assoc_is_u64]);
    // Owner: `fn generic_owner<T>(x: <&T as Ext>::Out) where &T: Ext`
    let owner = fx.func("generic_owner", 1, &[ref_p0_out], unit, std::vec![ref_p0_ext]);
    fx.finish();

    let mut b = Bb::new();
    let x_pat = b.param("x");
    let body = rc_before_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let x_local = b.local(x_pat);
    let w_fn = b.path(&["mk_wrap"]);
    let wrapped = b.call(w_fn, &[block, x_local]);
    let callee = b.path(&["need_u64"]);
    let call = b.call(callee, &[wrapped]);
    let s = b.semi(call);
    let root = b.block(&[s], None);

    let mut cx = InferCtx::new_in(alloc);
    let c = match check_deferring(&b.view(owner, root), &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "{:?}", c.diags);

    let mut out = c.types.fork_in(alloc).expect("fork");
    let none = out.intern_list(&[]).expect("list");
    let ready = out.intern(PNode::Adt(fx.key_of("Ready"), none)).expect("Ready");
    let list = out.intern_list(&[ready]).expect("list");
    let exact_ok = out.intern(PNode::Tuple(list)).expect("tuple");
    let view = b.view(owner, root);
    let decided = c.decide_stalled_with_storage(&view, &out, &[(block, exact_ok)], &fx, alloc, |_, _, _| {}).expect("decided");
    assert_eq!(decided.found.len(), 1, "{:?}", decided.found);
    let d = decided.found[0];
    assert_eq!(d.code, DiagCode::ProjectionMismatch, "expected E0271 for rigid <&T as Ext>::Out != u64, got {d:?}");
    assert_eq!(d.name, Some(Name::of("Assoc")), "{d:?}");
    let tys = decided.types.view();
    assert_eq!(d.expected.map(|t| crate::fixture::show(&fx, tys, t)), Some(std::string::String::from("u64")), "{d:?}");
    assert!(d.found.is_some(), "found type should be exported: {d:?}");
}
