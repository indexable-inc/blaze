//! `CoerceMany` does not eagerly bind an unconstrained outer type variable on source 0.
//!
//! When a coercion site's expected type is an unresolved general type variable (such as `?V`
//! from `fn with_labels<V: AsRef<str>>(vals: &[V])` called with `&[&s, "lit"]`), coercing
//! source 0 (`&String`) directly to `?V` would freeze `?V := &String`. When source 1 (`&str`)
//! later widens the site's LUB to `&str`, the outer unsizing `&[&str; 2] -> &[?V]` would fail
//! with E0308 (`found &[&str; 2], expected &[&String]`).
//!
//! Diverging (`!`) sources likewise defer their `NeverToAny` adjustment until
//! `coerce_many_complete` so `[&s, diverge(), "lit"]` and `[diverge(), &s, "lit"]` record
//! `NeverToAny -> &str` rather than a stale `&String`.

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::Expr;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct World {
    fx: Fx,
    owner: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let as_ref = fx.tr("AsRef", 2, Vec::new(), &[]);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);

    let str_ = fx.ty(PNode::Str);
    let u32_ = fx.ty(PNode::Uint(UintTy::U32));
    let unit = fx.unit();
    let never = fx.never();
    let r_str = fx.r(Mutbl::Not, str_);
    let r_u32 = fx.r(Mutbl::Not, u32_);

    let string_k = fx.strukt("String", 0, &[]);
    let string_ty = fx.adt(string_k, &[]);
    let r_string = fx.r(Mutbl::Not, string_ty);
    fx.imp("impl Deref for String", 0, string_ty, Some((deref, &[])), &[("Target", str_)], Vec::new());
    fx.imp("impl AsRef<str> for &str", 0, r_str, Some((as_ref, &[str_])), &[], Vec::new());
    fx.imp("impl AsRef<str> for &String", 0, r_string, Some((as_ref, &[str_])), &[], Vec::new());

    let c1_k = fx.strukt("C1", 0, &[]);
    let c1_ty = fx.adt(c1_k, &[]);
    let r_c1 = fx.r(Mutbl::Not, c1_ty);
    let c2_k = fx.strukt("C2", 0, &[]);
    let c2_ty = fx.adt(c2_k, &[]);
    let r_c2 = fx.r(Mutbl::Not, c2_ty);
    fx.imp("impl Tr for C1", 0, c1_ty, Some((tr, &[])), &[], Vec::new());
    fx.imp("impl Tr for C2", 0, c2_ty, Some((tr, &[])), &[], Vec::new());

    let p0 = fx.param(0);
    let slice_p0 = fx.ty(PNode::Slice(p0));
    let r_slice_p0 = fx.r(Mutbl::Not, slice_p0);
    let p0_as_ref = fx.bound(as_ref, &[p0, str_]);
    fx.func("with_labels", 1, &[r_slice_p0], unit, std::vec![p0_as_ref]);
    fx.func("take", 1, &[p0], unit, Vec::new());

    let slice_r_string = fx.ty(PNode::Slice(r_string));
    let r_slice_r_string = fx.r(Mutbl::Not, slice_r_string);
    fx.func("take_strings", 0, &[r_slice_r_string], unit, Vec::new());

    let empty = fx.list(&[]);
    let ex_tr = fx.ty(PNode::ExTrait(tr, empty));
    let dyn_preds = fx.list(&[ex_tr]);
    let dyn_tr = fx.ty(PNode::Dynamic(dyn_preds));
    let r_dyn_tr = fx.r(Mutbl::Not, dyn_tr);
    let slice_r_dyn = fx.ty(PNode::Slice(r_dyn_tr));
    let r_slice_r_dyn = fx.r(Mutbl::Not, slice_r_dyn);
    fx.func("take_dyn", 0, &[r_slice_r_dyn], unit, Vec::new());

    fx.func("diverge", 0, &[], never, Vec::new());

    let owner = fx.func("owner", 0, &[r_string, r_u32, r_c1, r_c2], unit, Vec::new());
    fx.finish();
    World { fx, owner }
}

/// Positive 1: `with_labels(&[s, "lit"])` and `with_labels(&["lit", s])` both pass, and `s`
/// records deref coercion adjustments to `&str`.
#[test]
fn generic_slice_array_lub_in_both_orders() {
    let w = world();
    for s_first in [true, false] {
        let mut b = Bb::new();
        let s = b.param("s");
        let _n = b.param("n");
        let _c1 = b.param("c1");
        let _c2 = b.param("c2");
        let e_s = b.local(s);
        let e_lit = b.str_();
        let elems = if s_first { [e_s, e_lit] } else { [e_lit, e_s] };
        let arr = b.array(&elems);
        let r_arr = b.borrow(Mutbl::Not, arr);
        let callee = b.path(&["with_labels"]);
        let call = b.call(callee, &[r_arr]);
        let root = b.block(&[], Some(call));
        let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "s_first={s_first}: {:?}", c.diags);
        let s_adj = adjusts(&w.fx, &c, e_s);
        assert!(s_adj.ends_with("& -> &str"), "s_first={s_first}: expected deref coercion to &str, got `{s_adj}`");
        assert_eq!(adjusts(&w.fx, &c, e_lit), "", "s_first={s_first}: literal needs no adjustment");
    }
}

/// Positive 2: `take(if cond { s } else { "lit" })` and `take('b: { if cond { break 'b s; } "lit" })`
/// both widen to `&str` without freezing `?T := &String`.
#[test]
fn generic_take_if_and_labeled_block_lub() {
    let w = world();
    // 1. `take(if true { s } else { "lit" })`
    {
        let mut b = Bb::new();
        let s = b.param("s");
        let _n = b.param("n");
        let _c1 = b.param("c1");
        let _c2 = b.param("c2");
        let cond = b.bool();
        let e_s = b.local(s);
        let e_lit = b.str_();
        let iff = b.e(Expr::If { cond, then: e_s, else_: Some(e_lit) });
        let callee = b.path(&["take"]);
        let call = b.call(callee, &[iff]);
        let root = b.block(&[], Some(call));
        let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "if: {:?}", c.diags);
        assert!(adjusts(&w.fx, &c, e_s).ends_with("& -> &str"), "if: `{}`", adjusts(&w.fx, &c, e_s));
    }
    // 2. `take(loop { if true { break s; } break "lit"; })`
    {
        let mut b = Bb::new();
        let s = b.param("s");
        let _n = b.param("n");
        let _c1 = b.param("c1");
        let _c2 = b.param("c2");
        let cond = b.bool();
        let e_s = b.local(s);
        let e_lit = b.str_();
        let lp = b.loop_with_cost(e_s, crate::body::LoopCost::Bounded);
        let brk1 = b.e(Expr::Break { value: Some(e_s), target: lp });
        let brk2 = b.e(Expr::Break { value: Some(e_lit), target: lp });
        let iff = b.e(Expr::If { cond, then: brk1, else_: None });
        let s0 = b.semi(iff);
        let s1 = b.semi(brk2);
        let body = b.block(&[s0, s1], None);
        b.exprs[lp.ix()] = Expr::Loop { body };
        let callee = b.path(&["take"]);
        let call = b.call(callee, &[lp]);
        let root = b.block(&[], Some(call));
        let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "loop: {:?}", c.diags);
        assert!(adjusts(&w.fx, &c, e_s).ends_with("& -> &str"), "loop: `{}`", adjusts(&w.fx, &c, e_s));
    }
}

/// Positive 3: diverging elements (`[s, diverge(), "lit"]` and `[diverge(), s, "lit"]`) pass
/// `with_labels`, and the diverging element's `NeverToAny` targets the final `&str` type.
#[test]
fn generic_slice_array_lub_with_diverging_element() {
    let w = world();
    for diverge_first in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let _n = b.param("n");
        let _c1 = b.param("c1");
        let _c2 = b.param("c2");
        let e_s = b.local(s);
        let div_fn = b.path(&["diverge"]);
        let e_div = b.call(div_fn, &[]);
        let e_lit = b.str_();
        let elems = if diverge_first { [e_div, e_s, e_lit] } else { [e_s, e_div, e_lit] };
        let arr = b.array(&elems);
        let r_arr = b.borrow(Mutbl::Not, arr);
        let callee = b.path(&["with_labels"]);
        let call = b.call(callee, &[r_arr]);
        let root = b.block(&[], Some(call));
        let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "diverge_first={diverge_first}: {:?}", c.diags);
        assert!(
            adjusts(&w.fx, &c, e_s).ends_with("& -> &str"),
            "diverge_first={diverge_first}: s adj `{}`",
            adjusts(&w.fx, &c, e_s)
        );
        assert_eq!(
            adjusts(&w.fx, &c, e_div),
            "NeverToAny -> &str",
            "diverge_first={diverge_first}: NeverToAny must target final &str"
        );
    }
}

/// Positive 4: concrete `&[&dyn Tr]` expectation still unsizes each element.
#[test]
fn concrete_dyn_slice_array_still_unsizes() {
    let w = world();
    let mut b = Bb::new();
    let _s = b.param("s");
    let _n = b.param("n");
    let c1 = b.param("c1");
    let c2 = b.param("c2");
    let e_c1 = b.local(c1);
    let e_c2 = b.local(c2);
    let arr = b.array(&[e_c1, e_c2]);
    let r_arr = b.borrow(Mutbl::Not, arr);
    let callee = b.path(&["take_dyn"]);
    let call = b.call(callee, &[r_arr]);
    let root = b.block(&[], Some(call));
    let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "dyn: {:?}", c.diags);
    assert!(adjusts(&w.fx, &c, e_c1).contains("unsize -> &dyn"), "c1: `{}`", adjusts(&w.fx, &c, e_c1));
    assert!(adjusts(&w.fx, &c, e_c2).contains("unsize -> &dyn"), "c2: `{}`", adjusts(&w.fx, &c, e_c2));
}

/// Negative 1: concrete `&[&String]` expectation still rejects `&[s, "lit"]` with E0308.
#[test]
fn concrete_string_slice_rejects_str_literal() {
    let w = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let _n = b.param("n");
    let _c1 = b.param("c1");
    let _c2 = b.param("c2");
    let e_s = b.local(s);
    let e_lit = b.str_();
    let arr = b.array(&[e_s, e_lit]);
    let r_arr = b.borrow(Mutbl::Not, arr);
    let callee = b.path(&["take_strings"]);
    let call = b.call(callee, &[r_arr]);
    let root = b.block(&[], Some(call));
    let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.contains(&DiagCode::Mismatch), "expected E0308 Mismatch, got {codes:?}");
}

/// Negative 2: incompatible element types `&[s, n]` (`&String` and `&u32`) still fail with E0308.
#[test]
fn incompatible_elements_in_generic_slice_still_fail() {
    let w = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let n = b.param("n");
    let _c1 = b.param("c1");
    let _c2 = b.param("c2");
    let e_s = b.local(s);
    let e_n = b.local(n);
    let arr = b.array(&[e_s, e_n]);
    let r_arr = b.borrow(Mutbl::Not, arr);
    let callee = b.path(&["with_labels"]);
    let call = b.call(callee, &[r_arr]);
    let root = b.block(&[], Some(call));
    let c = check_body(&w.fx, &b, w.owner, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.contains(&DiagCode::IncompatibleArms), "expected E0308 IncompatibleArms, got {codes:?}");
}

/// Positive 5: a defining RPIT body whose only return paths diverge (`fn f() -> impl Sized { diverge() }`
/// and `if true { diverge() } else { return diverge(); }`) queues its hidden-type variable for
/// diverging fallback, resolving the hidden type to `!` with zero diagnostics.
#[test]
fn diverging_rpit_body_resolves_hidden_type_to_never() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let never = fx.never();
    fx.func("diverge", 0, &[], never, Vec::new());

    let o1 = fx.opaque_key("div_direct::{opaque#0}");
    let none1 = fx.list(&[]);
    let opaque1 = fx.ty(PNode::Opaque(o1, none1));
    let is_sized1 = fx.bound(sized, &[opaque1]);
    fx.set_opaque_bounds(o1, std::vec![is_sized1]);
    let f_direct = fx.func("div_direct", 0, &[], opaque1, Vec::new());

    let o2 = fx.opaque_key("div_if::{opaque#0}");
    let none2 = fx.list(&[]);
    let opaque2 = fx.ty(PNode::Opaque(o2, none2));
    let is_sized2 = fx.bound(sized, &[opaque2]);
    fx.set_opaque_bounds(o2, std::vec![is_sized2]);
    let f_if = fx.func("div_if", 0, &[], opaque2, Vec::new());
    fx.finish();

    // 1. `fn div_direct() -> impl Sized { diverge() }`
    {
        let mut b = Bb::new();
        let d = b.path(&["diverge"]);
        let call = b.call(d, &[]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, f_direct, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "div_direct: {:?}", c.diags);
        assert_eq!(c.hidden.len(), 1, "div_direct hidden: {:?}", c.hidden);
        assert_eq!(crate::fixture::show(&fx, c.types.view(), c.hidden[0].1), "!");
    }

    // 2. `fn div_if() -> impl Sized { if true { diverge() } else { return diverge(); } }`
    {
        let mut b = Bb::new();
        let cond = b.bool();
        let d1 = b.path(&["diverge"]);
        let c1 = b.call(d1, &[]);
        let d2 = b.path(&["diverge"]);
        let c2 = b.call(d2, &[]);
        let r2 = b.ret(Some(c2));
        let iff = b.e(Expr::If { cond, then: c1, else_: Some(r2) });
        let root = b.block(&[], Some(iff));
        let c = check_body(&fx, &b, f_if, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "div_if: {:?}", c.diags);
        assert_eq!(c.hidden.len(), 1, "div_if hidden: {:?}", c.hidden);
        assert_eq!(crate::fixture::show(&fx, c.types.view(), c.hidden[0].1), "!");
    }
}
