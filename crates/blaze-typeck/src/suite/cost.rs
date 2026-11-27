//! Checked complexity cost contracts (`#[blaze::cost(...)]`, rules/cost.rs):
//! time, space, synchronization, loop marks (`bounded`, `amortized`, `reset`), closures inside
//! loops, direct recursion, `trusted = "..."`, opt-in zero-finding behavior for unannotated fns,
//! and malformed contracts/loop marks (Rule 3).

use std::vec;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::body::LoopCost;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey};
use crate::infer::InferCtx;
use crate::rules::args_inline::TypedFinding;
use crate::rules::cost::{CostViolation, EffectiveCost};
use crate::world::{CostClass, CostContract, Def, Needs, SyncCost, TypeAttrs};
use crate::{Outcome, check};

/// Builds a small world with callees carrying every cost class, space class, and sync class.
struct CostWorld {
    fx: Fx,
    fn_o1: ItemKey,
    fn_amort: ItemKey,
    fn_logn: ItemKey,
    fn_on: ItemKey,
    fn_on_space_o1: ItemKey,
    fn_o1_space_on: ItemKey,
    fn_onlogn: ItemKey,
    fn_on2: ItemKey,
    fn_trusted_logn: ItemKey,
    fn_local: ItemKey,
    fn_atomic: ItemKey,
    fn_locks: ItemKey,
    fn_unannotated: ItemKey,
}

impl CostWorld {
    fn new() -> Self {
        let mut fx = Fx::new();
        let unit = fx.unit();

        let add = |fx: &mut Fx, name: &str, cost: CostContract| -> ItemKey {
            let k = fx.func(name, 0, &[], unit, vec![]);
            fx.use_path(name, Def::Fn(k));
            if !cost.is_none() {
                fx.set_attrs(k, TypeAttrs(0).with_cost(cost));
            }
            k
        };

        let fn_o1 = add(&mut fx, "f_o1", CostContract::time_only(CostClass::O1, false));
        let fn_amort = add(&mut fx, "f_amort", CostContract::time_only(CostClass::AmortizedO1, false));
        let fn_logn = add(&mut fx, "f_logn", CostContract::time_only(CostClass::OLogN, false));
        let fn_on = add(&mut fx, "f_on", CostContract::time_only(CostClass::ON, false));
        let fn_on_space_o1 = add(
            &mut fx,
            "f_on_space_o1",
            CostContract::new(CostClass::ON, CostClass::O1, SyncCost::None, false),
        );
        let fn_o1_space_on = add(
            &mut fx,
            "f_o1_space_on",
            CostContract::new(CostClass::O1, CostClass::ON, SyncCost::None, false),
        );
        let fn_onlogn = add(&mut fx, "f_onlogn", CostContract::time_only(CostClass::ONLogN, false));
        let fn_on2 = add(&mut fx, "f_on2", CostContract::time_only(CostClass::ON2, false));
        let fn_trusted_logn = add(&mut fx, "f_trusted_logn", CostContract::time_only(CostClass::OLogN, true));
        let fn_local = add(
            &mut fx,
            "f_local",
            CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Local, false),
        );
        let fn_atomic = add(
            &mut fx,
            "f_atomic",
            CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Atomic, false),
        );
        let fn_locks = add(
            &mut fx,
            "f_locks",
            CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Lock, false),
        );
        let fn_unannotated = add(&mut fx, "f_unannotated", CostContract::NONE);

        Self {
            fx,
            fn_o1,
            fn_amort,
            fn_logn,
            fn_on,
            fn_on_space_o1,
            fn_o1_space_on,
            fn_onlogn,
            fn_on2,
            fn_trusted_logn,
            fn_local,
            fn_atomic,
            fn_locks,
            fn_unannotated,
        }
    }

    fn owner(&mut self, name: &str, cost: CostContract) -> ItemKey {
        let unit = self.fx.unit();
        let k = self.fx.func(name, 0, &[], unit, vec![]);
        self.fx.use_path(name, Def::Fn(k));
        if !cost.is_none() {
            self.fx.set_attrs(k, TypeAttrs(0).with_cost(cost));
        }
        k
    }

    fn check_findings(&mut self, owner: ItemKey, b: &Bb, root: ExprId) -> Vec<TypedFinding> {
        self.fx.finish();
        let bv = b.view(owner, root);
        let mut ty = InferCtx::new_in(Alloc::global());
        let done = match check(&bv, &self.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            Ok(Outcome::Need(n)) => panic!("cost test missed: {:?}", n.questions),
            Err(e) => panic!("cost test check failed: {e}"),
        };
        assert!(done.diags.is_empty(), "expected clean typing, got {:?}", done.diags);
        done.findings.iter().copied().collect()
    }
}

fn call_fn(b: &mut Bb, name: &str) -> ExprId {
    let p = b.path(&[name]);
    b.call(p, &[])
}

/// Builds a `loop { inner; }` with an explicit `break` so the loop types as `()`.
fn make_loop(b: &mut Bb, inner: ExprId, cost: LoopCost) -> ExprId {
    // Build body block with `inner;`, then wrap in `loop`, then patch a second `break`?
    // Even simpler: a `loop` whose body diverges or returns `()` via `return ()`:
    let s1 = b.semi(inner);
    let unit = b.tuple(&[]);
    let ret = b.ret(Some(unit));
    let s2 = b.semi(ret);
    let blk = b.block(&[s1, s2], None);
    let lp = b.loop_with_cost(blk, cost);
    let s = b.semi(lp);
    let tail = b.tuple(&[]);
    b.block(&[s], Some(tail))
}

#[test]
fn o1_calling_o1_passes_and_o1_calling_amortized_fails() {
    let mut w = CostWorld::new();
    let ok = w.owner("ok", CostContract::time_only(CostClass::O1, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_o1");
    assert!(w.check_findings(ok, &b, root).is_empty());

    let mut w = CostWorld::new();
    let bad = w.owner("bad", CostContract::time_only(CostClass::O1, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_amort");
    let fs = w.check_findings(bad, &b, root);
    assert_eq!(fs.len(), 2, "both time and space exceed O(1) vs amortized O(1): {fs:?}");
    assert!(fs.iter().any(|f| matches!(
        f,
        TypedFinding::Cost {
            violation: CostViolation::CallExceedsBudget,
            declared: CostClass::O1,
            actual: EffectiveCost::AmortizedO1,
            callee: Some(k),
            ..
        } if *k == w.fn_amort
    )));
}

#[test]
fn o1_unmarked_loop_fails_and_bounded_loop_passes() {
    let mut w = CostWorld::new();
    let bad = w.owner("bad_loop", CostContract::time_only(CostClass::O1, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_o1");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(bad, &b, root);
    assert!(
        fs.iter().any(|f| matches!(
            f,
            TypedFinding::Cost {
                violation: CostViolation::LoopExceedsBudget,
                declared: CostClass::O1,
                actual: EffectiveCost::ON,
                loop_depth: 1,
                ..
            }
        )),
        "expected LoopExceedsBudget: {fs:?}"
    );

    let mut w = CostWorld::new();
    let ok = w.owner("ok_bounded", CostContract::time_only(CostClass::O1, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_o1");
    let root = make_loop(&mut b, inner, LoopCost::Bounded);
    assert!(w.check_findings(ok, &b, root).is_empty());
}

#[test]
fn on_single_loop_allows_o1_and_amortized_but_rejects_on_and_ologn() {
    for callee in ["f_o1", "f_amort"] {
        let mut w = CostWorld::new();
        let ok = w.owner("ok", CostContract::time_only(CostClass::ON, false));
        let mut b = Bb::new();
        let inner = call_fn(&mut b, callee);
        let root = make_loop(&mut b, inner, LoopCost::Normal);
        let fs = w.check_findings(ok, &b, root);
        assert!(fs.is_empty(), "{callee} in O(n) single loop should pass: {fs:?}");
    }

    // O(n) loop calling O(n) => O(n^2) > O(n) (the scan_for-in-loop cliff!)
    let mut w = CostWorld::new();
    let bad_on = w.owner("bad_on", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_on");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(bad_on, &b, root);
    assert!(
        fs.iter().any(|f| matches!(
            f,
            TypedFinding::Cost {
                violation: CostViolation::CallExceedsBudget,
                declared: CostClass::ON,
                actual: EffectiveCost::ON2,
                loop_depth: 1,
                callee: Some(k),
                ..
            } if *k == w.fn_on
        )),
        "expected O(n) in loop => O(n^2): {fs:?}"
    );

    // O(n) loop calling O(log n) => O(n log n) > O(n)
    let mut w = CostWorld::new();
    let bad_logn = w.owner("bad_logn", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_logn");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(bad_logn, &b, root);
    assert!(
        fs.iter().any(|f| matches!(
            f,
            TypedFinding::Cost {
                violation: CostViolation::CallExceedsBudget,
                declared: CostClass::ON,
                actual: EffectiveCost::ONLogN,
                loop_depth: 1,
                callee: Some(k),
                ..
            } if *k == w.fn_logn
        )),
        "expected O(log n) in loop => O(n log n): {fs:?}"
    );

    // O(n log n) loop calling O(log n) passes!
    let mut w = CostWorld::new();
    let ok_nlogn = w.owner("ok_nlogn", CostContract::time_only(CostClass::ONLogN, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_logn");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(ok_nlogn, &b, root);
    assert!(fs.is_empty(), "O(n log n) loop calling O(log n) should pass: {fs:?}");
}

#[test]
fn on_nested_loops_fail_unless_inner_is_bounded_or_amortized() {
    let mut w = CostWorld::new();
    let bad = w.owner("bad_nested", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let c = call_fn(&mut b, "f_o1");
    let inner = make_loop(&mut b, c, LoopCost::Normal);
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(bad, &b, root);
    assert!(
        fs.iter().any(|f| matches!(
            f,
            TypedFinding::Cost {
                violation: CostViolation::LoopExceedsBudget,
                declared: CostClass::ON,
                actual: EffectiveCost::ON2,
                loop_depth: 2,
                ..
            }
        )),
        "expected nested loop violation: {fs:?}"
    );

    for mark in [LoopCost::Bounded, LoopCost::Amortized] {
        let mut w = CostWorld::new();
        let ok = w.owner("ok_nested", CostContract::time_only(CostClass::ON, false));
        let mut b = Bb::new();
        let c = call_fn(&mut b, "f_o1");
        let inner = make_loop(&mut b, c, mark);
        let root = make_loop(&mut b, inner, LoopCost::Normal);
        let fs = w.check_findings(ok, &b, root);
        assert!(fs.is_empty(), "inner loop marked {mark:?} should pass: {fs:?}");
    }
}

#[test]
fn trusted_passes_own_loop_and_callers_see_declared_class() {
    let mut w = CostWorld::new();
    let trusted = w.owner("bsearch", CostContract::time_only(CostClass::OLogN, true));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_o1");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    assert!(w.check_findings(trusted, &b, root).is_empty(), "trusted fn skips own body check");

    // Caller with O(n) calling `f_trusted_logn` inside a loop sees O(n log n) > O(n)!
    let mut w = CostWorld::new();
    let caller = w.owner("caller", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_trusted_logn");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(caller, &b, root);
    assert!(
        fs.iter().any(|f| matches!(
            f,
            TypedFinding::Cost {
                violation: CostViolation::CallExceedsBudget,
                declared: CostClass::ON,
                actual: EffectiveCost::ONLogN,
                callee: Some(k),
                ..
            } if *k == w.fn_trusted_logn
        )),
        "caller must see trusted callee's declared O(log n): {fs:?}"
    );
}

#[test]
fn unannotated_fn_is_opt_in_and_malformed_is_reported() {
    // Unannotated fn with nested loops calling O(n^2) produces ZERO findings.
    let mut w = CostWorld::new();
    let plain = w.owner("plain", CostContract::NONE);
    let mut b = Bb::new();
    let c = call_fn(&mut b, "f_on2");
    let inner = make_loop(&mut b, c, LoopCost::Normal);
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    assert!(w.check_findings(plain, &b, root).is_empty());

    // Malformed contract produces MalformedContract.
    let mut w = CostWorld::new();
    let bogus = w.owner("bogus", CostContract::INVALID);
    let mut b = Bb::new();
    let root = b.tuple(&[]);
    let fs = w.check_findings(bogus, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(fs[0], TypedFinding::Cost { violation: CostViolation::MalformedContract, .. }));

    // Malformed loop mark is reported even in an unannotated fn (Rule 3).
    let mut w = CostWorld::new();
    let bad_mark = w.owner("bad_mark", CostContract::NONE);
    let mut b = Bb::new();
    let c = call_fn(&mut b, "f_o1");
    let root = make_loop(&mut b, c, LoopCost::Invalid);
    let fs = w.check_findings(bad_mark, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(fs[0], TypedFinding::Cost { violation: CostViolation::MalformedLoopMark, .. }));
}

#[test]
fn direct_recursion_and_closure_inside_loop_are_caught() {
    // Direct recursion in O(log n) without trusted fails.
    let mut w = CostWorld::new();
    let rec = w.owner("rec", CostContract::time_only(CostClass::OLogN, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "rec");
    let fs = w.check_findings(rec, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost { violation: CostViolation::DirectRecursion, declared: CostClass::OLogN, callee: Some(k), .. } if k == rec
    ));

    // Closure inside a loop inherits enclosing loop depth!
    let mut w = CostWorld::new();
    let clo_fn = w.owner("clo_in_loop", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let c = call_fn(&mut b, "f_on");
    let clo = b.closure(&[], c);
    let root = make_loop(&mut b, clo, LoopCost::Normal);
    let fs = w.check_findings(clo_fn, &b, root);
    assert!(
        fs.iter().any(|f| matches!(
            f,
            TypedFinding::Cost {
                violation: CostViolation::CallExceedsBudget,
                declared: CostClass::ON,
                actual: EffectiveCost::ON2,
                loop_depth: 1,
                callee: Some(k),
                ..
            } if *k == w.fn_on
        )),
        "closure inside loop must inherit loop depth: {fs:?}"
    );
}

#[test]
fn space_contracts_and_reset_loops() {
    // O(n), space = O(1) calling O(n), space = O(1) passes.
    let mut w = CostWorld::new();
    let ok = w.owner("ok_space", CostContract::new(CostClass::ON, CostClass::O1, SyncCost::None, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_on_space_o1");
    assert!(w.check_findings(ok, &b, root).is_empty());

    // O(n), space = O(1) calling O(n) (default space = O(n)) fails SpaceExceedsBudget.
    let mut w = CostWorld::new();
    let bad = w.owner("bad_space", CostContract::new(CostClass::ON, CostClass::O1, SyncCost::None, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_on");
    let fs = w.check_findings(bad, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::SpaceExceedsBudget,
            declared: CostClass::O1,
            actual: EffectiveCost::ON,
            callee: Some(k),
            ..
        } if k == w.fn_on
    ));

    // O(n) unmarked loop calling O(1), space = O(n) fails SpaceExceedsBudget (O(n^2) space)!
    let mut w = CostWorld::new();
    let bad_loop_space = w.owner("bad_loop_space", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_o1_space_on");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(bad_loop_space, &b, root);
    assert_eq!(fs.len(), 1, "only space should fail (time is O(n)): {fs:?}");
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::SpaceExceedsBudget,
            declared: CostClass::ON,
            actual: EffectiveCost::ON2,
            loop_depth: 1,
            callee: Some(k),
            ..
        } if k == w.fn_o1_space_on
    ));

    // Marking the loop `#[blaze::cost(reset)]` allows O(1), space = O(n) inside O(n) loop...
    let mut w = CostWorld::new();
    let ok_reset = w.owner("ok_reset", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_o1_space_on");
    let root = make_loop(&mut b, inner, LoopCost::Reset);
    assert!(w.check_findings(ok_reset, &b, root).is_empty());

    // ...while still compounding TIME in a `reset` loop (calling O(n), space = O(1) fails TIME)!
    let mut w = CostWorld::new();
    let bad_reset_time = w.owner("bad_reset_time", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_on_space_o1");
    let root = make_loop(&mut b, inner, LoopCost::Reset);
    let fs = w.check_findings(bad_reset_time, &b, root);
    assert_eq!(fs.len(), 1, "reset loop still compounds time: {fs:?}");
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::CallExceedsBudget,
            declared: CostClass::ON,
            actual: EffectiveCost::ON2,
            loop_depth: 1,
            callee: Some(k),
            ..
        } if k == w.fn_on_space_o1
    ));
}

#[test]
fn sync_contracts_and_lock_in_loop() {
    // lock_free calling locks fails SyncExceedsBudget.
    let mut w = CostWorld::new();
    let lf_bad_lock = w.owner("lf_bad_lock", CostContract::new(CostClass::ON, CostClass::ON, SyncCost::Local, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_locks");
    let fs = w.check_findings(lf_bad_lock, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::SyncExceedsBudget,
            declared_sync: SyncCost::Local,
            actual_sync: SyncCost::Lock,
            callee: Some(k),
            ..
        } if k == w.fn_locks
    ));

    // lock_free calling atomic fails SyncExceedsBudget.
    let mut w = CostWorld::new();
    let lf_bad_atomic = w.owner("lf_bad_atomic", CostContract::new(CostClass::ON, CostClass::ON, SyncCost::Local, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_atomic");
    let fs = w.check_findings(lf_bad_atomic, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::SyncExceedsBudget,
            declared_sync: SyncCost::Local,
            actual_sync: SyncCost::Atomic,
            callee: Some(k),
            ..
        } if k == w.fn_atomic
    ));

    // sync-only `#[blaze::cost(lock_free)]` (time = None) calling locks fails SyncExceedsBudget.
    let mut w = CostWorld::new();
    let lf_only = w.owner("lf_only", CostContract::new(CostClass::None, CostClass::None, SyncCost::Local, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_locks");
    let fs = w.check_findings(lf_only, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::SyncExceedsBudget,
            declared_sync: SyncCost::Local,
            actual_sync: SyncCost::Lock,
            ..
        }
    ));

    // atomic calling atomic passes; atomic calling locks fails.
    let mut w = CostWorld::new();
    let at_ok = w.owner("at_ok", CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Atomic, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_atomic");
    assert!(w.check_findings(at_ok, &b, root).is_empty());

    let mut w = CostWorld::new();
    let at_bad = w.owner("at_bad", CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Atomic, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_locks");
    let fs = w.check_findings(at_bad, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::SyncExceedsBudget,
            declared_sync: SyncCost::Atomic,
            actual_sync: SyncCost::Lock,
            ..
        }
    ));

    // O(n) (sync = None) calling locks OUTSIDE a loop passes...
    let mut w = CostWorld::new();
    let lock_out = w.owner("lock_out", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let root = call_fn(&mut b, "f_locks");
    assert!(w.check_findings(lock_out, &b, root).is_empty());

    // ...calling locks INSIDE an unmarked loop fails LockInLoop!
    let mut w = CostWorld::new();
    let lock_in = w.owner("lock_in", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_locks");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    let fs = w.check_findings(lock_in, &b, root);
    assert_eq!(fs.len(), 1);
    assert!(matches!(
        fs[0],
        TypedFinding::Cost {
            violation: CostViolation::LockInLoop,
            actual_sync: SyncCost::Lock,
            loop_depth: 1,
            callee: Some(k),
            ..
        } if k == w.fn_locks
    ));

    // ...calling locks inside a `#[blaze::cost(bounded)]` loop passes!
    let mut w = CostWorld::new();
    let lock_bounded = w.owner("lock_bounded", CostContract::time_only(CostClass::ON, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_locks");
    let root = make_loop(&mut b, inner, LoopCost::Bounded);
    assert!(w.check_findings(lock_bounded, &b, root).is_empty());

    // ...and an fn explicitly declaring `locks` may call locks in a loop!
    let mut w = CostWorld::new();
    let lock_declared = w.owner("lock_declared", CostContract::new(CostClass::ON, CostClass::ON, SyncCost::Lock, false));
    let mut b = Bb::new();
    let inner = call_fn(&mut b, "f_locks");
    let root = make_loop(&mut b, inner, LoopCost::Normal);
    assert!(w.check_findings(lock_declared, &b, root).is_empty());

    // Also verify unannotated callee is allowed (opt-in) and local callee is allowed in lock_free.
    let mut w = CostWorld::new();
    let lf_ok = w.owner("lf_ok", CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Local, false));
    let mut b = Bb::new();
    let c1 = call_fn(&mut b, "f_local");
    let s1 = b.semi(c1);
    let c2 = call_fn(&mut b, "f_unannotated");
    let root = b.block(&[s1], Some(c2));
    assert!(w.check_findings(lf_ok, &b, root).is_empty());

    // Touch unused field to keep dead_code clean.
    let _ = (w.fn_o1, w.fn_onlogn, w.fn_on2, w.fn_local, w.fn_unannotated);
}

#[test]
fn const_block_is_ignored_at_runtime() {
    let mut w = CostWorld::new();
    let ok = w.owner("const_blk", CostContract::time_only(CostClass::O1, false));
    let mut b = Bb::new();
    // Put a loop inside a const block: compile-time work is O(1) at runtime.
    let inner = call_fn(&mut b, "f_on");
    let lp = make_loop(&mut b, inner, LoopCost::Normal);
    let s = b.semi(lp);
    let unit = b.tuple(&[]);
    let cb = b.e_const_block(&[s], Some(unit));
    assert!(w.check_findings(ok, &b, cb).is_empty());
}
