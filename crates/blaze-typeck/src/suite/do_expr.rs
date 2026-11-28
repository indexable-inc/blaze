//! Postfix `.do` suspension operator (`Expr::Do`, `rules/do_expr.rs`):
//! 1. `expr.do` resolves `<T as Request>::Answer` cleanly (and rejects non-`Request` operands with
//!    `DiagCode::Unsatisfied`).
//! 2. Taking `&local` / `&mut local` and using it *before* `.do` passes with 0 findings.
//! 3. Taking `&local` / `&mut local` before `.do` and using it *after* `.do` (or holding `&local`
//!    on the evaluation stack across `.do`, e.g. `f(&local, req.do)`) emits
//!    `TypedFinding::LocalBorrowAcrossDo` (`lint() == "local-borrow-across-do"`).
//! 4. Owned values moved or used across `.do` (and dereferenced scalar copies `let y = *r;`) pass
//!    cleanly with 0 findings.

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::body::{Expr, IntSuffix, UnOp};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, Name, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::rules::args_inline::TypedFinding;
use crate::world::Def;

struct Items {
    /// `fn perform(req: ReadByte) -> u8`
    perform: ItemKey,
    /// `fn bad_perform(x: u8) -> u8`
    bad_perform: ItemKey,
    /// `fn with_local(x: u8, req: ReadByte) -> u8`
    with_local: ItemKey,
    /// `fn with_owned(o: Owned, req: ReadByte) -> u8`
    with_owned: ItemKey,
    /// `fn with_defer(req: ReadByte) -> ()`
    with_defer: ItemKey,
    /// `fn no_do(x: u8) -> ()`
    no_do: ItemKey,
}

fn world() -> (Fx, Items) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);

    let u8_ = fx.u8();
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());

    // `trait Request { type Answer; }` registered as `LangItem::Request`.
    let request = fx.tr("Request", 1, Vec::new(), &["Answer"]);
    fx.lang(LangItem::Request, request);

    let read_byte = fx.strukt("ReadByte", 0, &[]);
    let read_byte_t = fx.adt(read_byte, &[]);
    fx.imp(
        "impl Request for ReadByte",
        0,
        read_byte_t,
        Some((request, &[])),
        &[("Answer", u8_)],
        Vec::new(),
    );

    let owned = fx.strukt("Owned", 0, &[("v", u8_)]);
    let owned_t = fx.adt(owned, &[]);
    let ref_u8 = fx.r(Mutbl::Not, u8_);
    let mut_ref_u8 = fx.r(Mutbl::Mut, u8_);

    let perform = fx.func("perform", 0, &[read_byte_t], u8_, Vec::new());
    let bad_perform = fx.func("bad_perform", 0, &[u8_], u8_, Vec::new());
    let use_ref = fx.func("use_ref", 0, &[ref_u8], u8_, Vec::new());
    let use_mut_ref = fx.func("use_mut_ref", 0, &[mut_ref_u8], u8_, Vec::new());
    let take_two = fx.func("take_two", 0, &[ref_u8, u8_], u8_, Vec::new());
    let use_owned = fx.func("use_owned", 0, &[owned_t], u8_, Vec::new());
    let with_local = fx.func("with_local", 0, &[u8_, read_byte_t], u8_, Vec::new());
    let with_owned = fx.func("with_owned", 0, &[owned_t, read_byte_t], u8_, Vec::new());
    let unit = fx.unit();
    let with_defer = fx.func("with_defer", 0, &[read_byte_t], unit, Vec::new());
    let no_do = fx.func("no_do", 0, &[u8_], unit, Vec::new());

    fx.use_path("use_ref", Def::Fn(use_ref));
    fx.use_path("use_mut_ref", Def::Fn(use_mut_ref));
    fx.use_path("take_two", Def::Fn(take_two));
    fx.use_path("use_owned", Def::Fn(use_owned));

    fx.finish();
    (world, Items { perform, bad_perform, with_local, with_owned, with_defer, no_do })
}

/// 1a. `fn perform(req: ReadByte) -> u8 { req.do }` resolves `<ReadByte as Request>::Answer` to `u8`.
#[test]
fn do_expr_resolves_request_answer_type() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let req = b.param("req");
    let e0 = b.local(req);
    let e1 = b.do_expr(e0);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.perform, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "ReadByte"), (ExprId(1), "u8"), (ExprId(2), "u8")],
            pats: &[(PatId(0), "ReadByte")],
            diags: &[],
        },
    );
    assert!(c.findings.is_empty(), "expected 0 findings, got {:?}", c.findings);
}

/// 1b. Control: `x.do` on a type that does not implement `Request` reports `DiagCode::Unsatisfied`.
#[test]
fn do_expr_rejects_non_request_operand() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.local(x);
    let e1 = b.do_expr(e0);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.bad_perform, root, Plant::None, InferPlant::None);
    assert!(
        c.diags.iter().any(|d| d.code == DiagCode::Unsatisfied),
        "expected Unsatisfied diagnostic, got {:?}",
        c.diags
    );
}

/// 2. Taking `&local` and using it *before* `.do` passes cleanly with 0 findings.
#[test]
fn local_borrow_used_before_do_passes_cleanly() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let req = b.param("req");

    // let r = &x;
    let e_x = b.local(x);
    let e_ref = b.borrow(Mutbl::Not, e_x);
    let p_r = b.bind("r");
    let s_let = b.let_(p_r, None, e_ref);

    // use_ref(r);
    let e_fn = b.path(&["use_ref"]);
    let e_r = b.local(p_r);
    let e_call = b.call(e_fn, &[e_r]);
    let s_call = b.semi(e_call);

    // req.do
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let root = b.block(&[s_let, s_call], Some(e_do));

    let c = check_body(&fx, &b, it.with_local, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert!(c.findings.is_empty(), "borrow dead before .do must emit 0 findings, got {:?}", c.findings);
}

/// 3a. Taking `&local` before `.do` and using it *after* `.do` emits `LocalBorrowAcrossDo`.
#[test]
fn local_borrow_used_after_do_emits_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let req = b.param("req");

    // let r = &x;
    let e_x = b.local(x);
    let e_ref = b.borrow(Mutbl::Not, e_x);
    let p_r = b.bind("r");
    let s_let = b.let_(p_r, None, e_ref);

    // req.do;
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);

    // use_ref(r)
    let e_fn = b.path(&["use_ref"]);
    let e_r = b.local(p_r);
    let e_call = b.call(e_fn, &[e_r]);
    let root = b.block(&[s_let, s_do], Some(e_call));

    let c = check_body(&fx, &b, it.with_local, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert_eq!(c.findings.len(), 1, "expected 1 finding, got {:?}", c.findings);
    assert_eq!(c.findings[0].lint(), "local-borrow-across-do");
    assert!(
        matches!(c.findings[0], TypedFinding::LocalBorrowAcrossDo { local, .. } if local == Name::of("x")),
        "expected finding naming local `x`, got {:?}",
        c.findings[0]
    );
}

/// 3b. Taking `&mut local` before `.do` and using it *after* `.do` also emits `LocalBorrowAcrossDo`.
#[test]
fn local_mut_borrow_used_after_do_emits_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let req = b.param("req");

    // let r = &mut x;
    let e_x = b.local(x);
    let e_ref = b.borrow(Mutbl::Mut, e_x);
    let p_r = b.bind("r");
    let s_let = b.let_(p_r, None, e_ref);

    // req.do;
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);

    // use_mut_ref(r)
    let e_fn = b.path(&["use_mut_ref"]);
    let e_r = b.local(p_r);
    let e_call = b.call(e_fn, &[e_r]);
    let root = b.block(&[s_let, s_do], Some(e_call));

    let c = check_body(&fx, &b, it.with_local, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert_eq!(c.findings.len(), 1, "expected 1 finding, got {:?}", c.findings);
    assert_eq!(c.findings[0].lint(), "local-borrow-across-do");
    assert!(
        matches!(c.findings[0], TypedFinding::LocalBorrowAcrossDo { local, .. } if local == Name::of("x")),
        "expected finding naming local `x`, got {:?}",
        c.findings[0]
    );
}

/// 3c. Holding a temporary `&local` live on the call evaluation stack across `.do`
/// (`take_two(&x, req.do)`) emits `LocalBorrowAcrossDo`.
#[test]
fn temporary_local_borrow_in_sibling_arg_across_do_emits_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let req = b.param("req");

    let e_fn = b.path(&["take_two"]);
    let e_x = b.local(x);
    let e_ref = b.borrow(Mutbl::Not, e_x);
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let e_call = b.call(e_fn, &[e_ref, e_do]);
    let root = b.block(&[], Some(e_call));

    let c = check_body(&fx, &b, it.with_local, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert_eq!(c.findings.len(), 1, "expected 1 finding, got {:?}", c.findings);
    assert_eq!(c.findings[0].lint(), "local-borrow-across-do");
    assert!(
        matches!(c.findings[0], TypedFinding::LocalBorrowAcrossDo { local, .. } if local == Name::of("x")),
        "expected finding naming local `x`, got {:?}",
        c.findings[0]
    );
}

/// 4a. Owned value moved across `.do` (`req.do; use_owned(o)`) passes cleanly with 0 findings.
#[test]
fn owned_value_moved_across_do_passes_cleanly() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let req = b.param("req");

    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);

    let e_fn = b.path(&["use_owned"]);
    let e_o = b.local(o);
    let e_call = b.call(e_fn, &[e_o]);
    let root = b.block(&[s_do], Some(e_call));

    let c = check_body(&fx, &b, it.with_owned, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert!(c.findings.is_empty(), "owned value across .do must emit 0 findings, got {:?}", c.findings);
}

/// 4b. Copying a scalar out of `&local` before `.do` (`let r = &x; let y = *r; req.do; y`) passes
/// cleanly with 0 findings because `y: u8` holds no borrow.
#[test]
fn scalar_copied_from_local_ref_before_do_passes_cleanly() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let req = b.param("req");

    // let r = &x;
    let e_x = b.local(x);
    let e_ref = b.borrow(Mutbl::Not, e_x);
    let p_r = b.bind("r");
    let s_r = b.let_(p_r, None, e_ref);

    // let y = *r;
    let e_r = b.local(p_r);
    let e_deref = b.e(Expr::Unary { op: UnOp::Deref, operand: e_r });
    let p_y = b.bind("y");
    let s_y = b.let_(p_y, None, e_deref);

    // req.do;
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);

    // y
    let e_y = b.local(p_y);
    let root = b.block(&[s_r, s_y, s_do], Some(e_y));

    let c = check_body(&fx, &b, it.with_local, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert!(c.findings.is_empty(), "copied scalar across .do must emit 0 findings, got {:?}", c.findings);
}

/// 5a. `defer { req.do; }` typechecks cleanly to `()` and sets `checked.suspends == true`;
/// a body without `.do` leaves `checked.suspends == false`.
#[test]
fn defer_block_with_do_typechecks_to_unit_and_sets_suspends() {
    let (fx, it) = world();

    let mut b = Bb::new();
    let req = b.param("req");
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);
    let defer_body = b.block(&[s_do], None);
    let e_defer = b.defer_expr(defer_body);
    let s_defer = b.semi(e_defer);
    let root = b.block(&[s_defer], None);

    let c = check_body(&fx, &b, it.with_defer, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert!(c.findings.is_empty(), "expected 0 findings, got {:?}", c.findings);
    assert!(c.suspends, "defer containing `.do` must set checked.suspends");
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(e_req, "ReadByte"), (e_do, "u8"), (defer_body, "()"), (e_defer, "()"), (root, "()")],
            pats: &[(req, "ReadByte")],
            diags: &[],
        },
    );

    // Control: `defer` with no `.do` leaves `checked.suspends == false`.
    let mut b2 = Bb::new();
    let x = b2.param("x");
    let e_x = b2.local(x);
    let p_w = b2.wild();
    let s_let = b2.let_(p_w, None, e_x);
    let defer_body2 = b2.block(&[s_let], None);
    let e_defer2 = b2.defer_expr(defer_body2);
    let s_defer2 = b2.semi(e_defer2);
    let root2 = b2.block(&[s_defer2], None);

    let c2 = check_body(&fx, &b2, it.no_do, root2, Plant::None, InferPlant::None);
    assert!(c2.diags.is_empty(), "unexpected type errors: {:?}", c2.diags);
    assert!(!c2.suspends, "body without `.do` must not set checked.suspends");
}

/// 5b. `let x = 1u8; let r = &x; defer { req.do; let _ = *r; }` emits `LocalBorrowAcrossDo`.
#[test]
fn local_borrow_across_do_inside_defer_emits_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let req = b.param("req");

    // let x = 1u8;
    let e_one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let p_x = b.bind("x");
    let s_x = b.let_(p_x, None, e_one);

    // let r = &x;
    let e_x = b.local(p_x);
    let e_ref = b.borrow(Mutbl::Not, e_x);
    let p_r = b.bind("r");
    let s_r = b.let_(p_r, None, e_ref);

    // defer { req.do; let _ = *r; };
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);
    let e_r = b.local(p_r);
    let e_deref = b.e(Expr::Unary { op: UnOp::Deref, operand: e_r });
    let p_w = b.wild();
    let s_use = b.let_(p_w, None, e_deref);
    let defer_body = b.block(&[s_do, s_use], None);
    let e_defer = b.defer_expr(defer_body);
    let s_defer = b.semi(e_defer);

    let root = b.block(&[s_x, s_r, s_defer], None);
    let c = check_body(&fx, &b, it.with_defer, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert!(c.suspends, "defer containing `.do` must set checked.suspends");
    assert_eq!(c.findings.len(), 1, "expected 1 finding, got {:?}", c.findings);
    assert_eq!(c.findings[0].lint(), "local-borrow-across-do");
    assert!(
        matches!(c.findings[0], TypedFinding::LocalBorrowAcrossDo { local, .. } if local == Name::of("x")),
        "expected finding naming local `x`, got {:?}",
        c.findings[0]
    );
}

/// 5c. `let x = 1u8; let r = &x; defer { let _ = *r; }; req.do;` holds `&x` inside `defer` across
/// the enclosing scope's `.do` and emits `LocalBorrowAcrossDo`.
#[test]
fn local_borrow_held_by_defer_across_enclosing_do_emits_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let req = b.param("req");

    // let x = 1u8;
    let e_one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let p_x = b.bind("x");
    let s_x = b.let_(p_x, None, e_one);

    // let r = &x;
    let e_x = b.local(p_x);
    let e_ref = b.borrow(Mutbl::Not, e_x);
    let p_r = b.bind("r");
    let s_r = b.let_(p_r, None, e_ref);

    // defer { let _ = *r; };
    let e_r = b.local(p_r);
    let e_deref = b.e(Expr::Unary { op: UnOp::Deref, operand: e_r });
    let p_w = b.wild();
    let s_use = b.let_(p_w, None, e_deref);
    let defer_body = b.block(&[s_use], None);
    let e_defer = b.defer_expr(defer_body);
    let s_defer = b.semi(e_defer);

    // req.do;
    let e_req = b.local(req);
    let e_do = b.do_expr(e_req);
    let s_do = b.semi(e_do);

    let root = b.block(&[s_x, s_r, s_defer, s_do], None);
    let c = check_body(&fx, &b, it.with_defer, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "unexpected type errors: {:?}", c.diags);
    assert!(c.suspends, "body containing `.do` must set checked.suspends");
    assert_eq!(c.findings.len(), 1, "expected 1 finding, got {:?}", c.findings);
    assert_eq!(c.findings[0].lint(), "local-borrow-across-do");
    assert!(
        matches!(c.findings[0], TypedFinding::LocalBorrowAcrossDo { local, .. } if local == Name::of("x")),
        "expected finding naming local `x`, got {:?}",
        c.findings[0]
    );
}
