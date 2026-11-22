//! The importer's refusal names its caller (census 13, Brief 12): a portable term naming
//! `Param(i)` instantiated with fewer than `i + 1` arguments is `Malformed::Param`, and its `at`
//! is the line that asked for the import (through `#[track_caller]` on `fold::import`, the
//! `Cx` helpers and eval's and coherence's `import_pred`), so a census line says which rule
//! gave the short list instead of only that one was short.


use super::{Inst, Scratch, import};
use crate::error::{Malformed, SolveError};
use crate::facts::Portable;
use crate::term::{Ty, TyKind};
use crate::testing::tys::TestTys;
use crate::tys::Tys;

/// `Param(0)` imported with an empty argument list is refused as `Malformed::Param { index: 0,
/// args: 0, .. }` whose `at` is this test's call of `fold::import` (its file and line), with no
/// goal (the driver fills that, `eval::Search::drive`); the same term with one argument is that
/// argument. A refusal that names fold.rs's own line (no `#[track_caller]` on `import` or
/// `import_walk`) fails the `at` asserts; an import that guesses a term for the missing
/// argument fails the match.
///
/// Expected line: "test fold::tests::param_refusal_names_its_caller ... ok".
#[test]
fn param_refusal_names_its_caller() {
    let mut tys = TestTys::new();
    let mut s = Scratch::new_in(blaze_own::testing::heap().alloc());
    let nodes = [TyKind::Param(0)];
    let p = Portable { nodes: &nodes, lists: &[] };

    // Clean: one argument, so `Param(0)` is it.
    let bool_ = match tys.mk(TyKind::Bool) {
        Ok(t) => t,
        Err(e) => panic!("the test arena refused `bool`: {e:?}"),
    };
    match import(&mut tys, &mut s, p, Ty(0), Inst { args: &[bool_], bound: &[] }) {
        Ok(t) => assert_eq!(t, bool_, "`Param(0)` with `[bool]` is not `bool`"),
        Err(e) => panic!("`Param(0)` with one argument was refused: {e:?}"),
    }

    // Refused: no argument for `Param(0)`; the refusal names this line.
    let line = line!() + 1;
    let got = import(&mut tys, &mut s, p, Ty(0), Inst { args: &[], bound: &[] });
    match got {
        Err(SolveError::Malformed(Malformed::Param { index: 0, args: 0, at, goal: None, head: None })) => {
            assert_eq!(at.file(), file!(), "the refusal names {at}, not this test's call");
            assert_eq!(at.line(), line, "the refusal names {at}, not this test's call");
        }
        other => panic!("`Param(0)` with no arguments gave {other:?}, not `Malformed::Param {{ index: 0, args: 0, .. }}`"),
    }
}

/// A shared bound-variable node denotes the surrounding binder in the tuple, but a
/// signature-local variable in the fn pointer. An early argument containing a free
/// variable must cross both the fn pointer and the existential projection binders.
#[test]
fn substitution_respects_nested_binders() -> Result<(), SolveError> {
    use crate::term::{Def, FnPtrHeader, List};

    let mut tys = TestTys::new();
    let mut scratch = Scratch::new_in(blaze_own::testing::heap().alloc());
    let nodes = [
        TyKind::Bound(0, 0),
        TyKind::Bound(1, 0),
        TyKind::Bound(2, 0),
        TyKind::Param(0),
        TyKind::ExProj(Def(5), List { start: 0, len: 1 }, Ty(3)),
        TyKind::ExTrait(Def(4), List { start: 1, len: 1 }),
        TyKind::ReStatic,
        TyKind::Dynamic(List { start: 2, len: 2 }, Ty(6)),
        TyKind::FnPtr(List { start: 4, len: 4 }, FnPtrHeader::RUST),
        TyKind::Tuple(List { start: 8, len: 4 }),
    ];
    let lists = [Ty(2), Ty(2), Ty(5), Ty(4), Ty(0), Ty(1), Ty(2), Ty(7), Ty(0), Ty(3), Ty(1), Ty(8)];
    let p = Portable { nodes: &nodes, lists: &lists };
    let outer = tys.mk(TyKind::Char)?;
    let local = tys.mk(TyKind::Bound(0, 0))?;
    let free = tys.mk(TyKind::Bound(1, 7))?;
    let under_fn_and_projection = tys.mk(TyKind::Bound(3, 7))?;
    let outer_args = tys.mk_list(&[outer])?;
    let proj = tys.mk(TyKind::ExProj(Def(5), outer_args, under_fn_and_projection))?;
    let principal = tys.mk(TyKind::ExTrait(Def(4), outer_args))?;
    let preds = tys.mk_list(&[principal, proj])?;
    let static_ = tys.mk(TyKind::ReStatic)?;
    let object = tys.mk(TyKind::Dynamic(preds, static_))?;
    let farther_inside_fn = tys.mk(TyKind::Bound(1, 0))?;
    let sig = tys.mk_list(&[local, outer, farther_inside_fn, object])?;
    let ptr = tys.mk(TyKind::FnPtr(sig, FnPtrHeader::RUST))?;
    let items = tys.mk_list(&[outer, free, local, ptr])?;
    let expected = tys.mk(TyKind::Tuple(items))?;

    let root = Ty(9);
    let applied = import(&mut tys, &mut scratch, p, root, Inst { args: &[free], bound: &[outer] })?;
    assert_eq!(applied, expected);
    // In the staged case the caller's free argument first enters the still-present binder.
    let free_under_binder = tys.mk(TyKind::Bound(2, 7))?;
    let quantified = import(&mut tys, &mut scratch, p, root, Inst { args: &[free_under_binder], bound: &[] })?;
    assert_eq!(super::instantiate_bound(&mut tys, &mut scratch, quantified, outer_args)?, expected);
    Ok(())
}

/// `import_list_at` shares one memo generation across all elements of a list (a repeated compound
/// subterm is imported once, not once per list slot), handles lists longer than the 8-element
/// stack buffer, and leaves `memo` clean for subsequent imports with different arguments.
#[test]
fn import_list_shares_memo_and_spills_past_eight() -> Result<(), SolveError> {
    use crate::term::List;

    let mut tys = TestTys::new();
    let mut scratch = Scratch::new_in(blaze_own::testing::heap().alloc());
    let nodes = [
        TyKind::Param(0),                                   // 0: T
        TyKind::Slice(Ty(0)),                               // 1: [T]
        TyKind::RawPtr(Ty(1), crate::term::Mutbl::Not),     // 2: *const [T]
    ];
    // 10-element list (> 8 inline buffer) repeating the same compound node `Ty(2)` and leaf `Ty(0)`.
    let lists = [Ty(2), Ty(0), Ty(2), Ty(1), Ty(2), Ty(0), Ty(2), Ty(1), Ty(2), Ty(0)];
    let p = Portable { nodes: &nodes, lists: &lists };
    let t1 = tys.mk(TyKind::Bool)?;
    let t2 = tys.mk(TyKind::Char)?;

    let at = core::panic::Location::caller();
    let before = scratch.stats.terms_imported;
    let l1 = super::import_list_at(
        &mut tys,
        &mut scratch,
        p,
        List { start: 0, len: lists.len() as u32 },
        Inst { args: &[t1], bound: &[] },
        at,
    )?;
    let imported = scratch.stats.terms_imported - before;
    // Compound nodes (0, 1, 2) imported once (3) + 3 direct leaf slots of Ty(0) = 6,
    // far fewer than 5*3 + 2*2 + 3*1 = 22 without shared memo.
    assert_eq!(imported, 6, "shared memo should import compound subterms once across the list");

    let slice_1 = tys.mk(TyKind::Slice(t1))?;
    let ptr_1 = tys.mk(TyKind::RawPtr(slice_1, crate::term::Mutbl::Not))?;
    assert_eq!(l1.len, 10);
    assert_eq!(tys.list_get(l1, 0), ptr_1);
    assert_eq!(tys.list_get(l1, 1), t1);
    assert_eq!(tys.list_get(l1, 3), slice_1);
    assert_eq!(tys.list_get(l1, 8), ptr_1);
    assert_eq!(tys.list_get(l1, 9), t1);

    // Next import with a different argument must not see stale memo entries from the previous generation.
    let l2 = super::import_list_at(
        &mut tys,
        &mut scratch,
        p,
        List { start: 0, len: lists.len() as u32 },
        Inst { args: &[t2], bound: &[] },
        at,
    )?;
    let slice_2 = tys.mk(TyKind::Slice(t2))?;
    let ptr_2 = tys.mk(TyKind::RawPtr(slice_2, crate::term::Mutbl::Not))?;
    assert_eq!(tys.list_get(l2, 0), ptr_2);
    assert_eq!(tys.list_get(l2, 3), slice_2);
    assert_eq!(tys.list_get(l2, 9), t2);
    Ok(())
}
