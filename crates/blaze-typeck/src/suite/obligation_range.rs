//! Where an unmet where-clause is filed (piece obligation-range; NEVER COMPILED when written,
//! 2026-11-16): an obligation carries the range of the syntax that created it.
//!
//! ```text
//! trait Show { fn shown() -> u8; }               // no impl for B
//! struct B;
//! struct W<T>;
//! impl<T> W<T> where T: Show { fn bounded(&self) -> u8; fn make() -> W<T>; }
//! impl<T> W<T> { fn get(&self) -> u8 where T: Show; }
//! fn grant<T: Show>(n: u8) -> u8;
//! fn need<T: Show>(t: &T) -> u8;
//!
//! fn c1() -> u8 { grant::<B>(1u8) }              // at `B` of `::<B>`
//! fn c2(b: B) -> u8 { need(&b) }                 // at `&b`
//! fn c3(w: W<B>) -> u8 { w.get() }               // at `w`
//! fn c4() { let w: W<B> = W::make(); }           // at `make`
//! fn c5() -> u8 { <B as Show>::shown() }         // at `B` of `<B as Show>`
//! fn c6(w: W<B>) -> u8 { w.bounded() }           // E0599 at `bounded`, naming `B: Show`
//! ```
//!
//! c3's bound is the METHOD's own where-clause. The probe does not check it, so the method is
//! picked and the bound is an obligation, E0277. c6's bound is on the IMPL. The probe checks
//! it (rustc method/probe.rs `consider_probe`, `InherentImplCandidate`: the impl's clauses are
//! registered and any error is `ProbeResult::NoMatch`, kept in `unsatisfied_predicates`), so
//! nothing applies. rustc then says E0599 (method/suggest.rs): "the method `bounded` exists for
//! struct `W<B>`, but its trait bounds were not satisfied", at the method name, with "the
//! following trait bounds were not satisfied: `B: Show`". blaze files the same E0599, at the
//! name, carrying the unmet `B: Show` (rules/method.rs `unmet_impl_clause`). Until
//! 2026-11-21, c3 here was `get` on the bounded impl and wanted E0277 at `w`. That was not
//! rustc's behavior.
//!
//! The Reference, "Trait and lifetime bounds": "Trait and lifetime bounds provide a way for
//! generic items to restrict which types and lifetimes are used as their parameters": each body
//! uses an item at `B` where the item's bound asks `B: Show`, which no impl gives, so each is
//! E0277. WHERE it is filed is blaze's rule (rules/obligation_site.rs; rule 5, useful over
//! parity): the narrowest text that introduced the obligation. A written generic argument for
//! the bounded parameter (c1); the argument passed for the first input that mentions it (c2;
//! rustc's `adjust_fulfillment_errors_for_expr_obligation` points there too); the receiver,
//! input 0 of a method call (c3); the segment naming the item when no input or written argument
//! names the parameter (c4: the impl's `T` is fixed by the annotation only); a trait item's
//! `Self` written as the anchor (c5). The whole expression only when no narrower text is known.
//!
//! The ranges come from `fixture::ranged` (an expression at its id, a turbofish argument at
//! 2000 + its index, a segment at 3000 + its index, an anchor type at 4000 + its path's index, a
//! method name at 5000 + the call's id): each test asserts the RANGE of every E0277, not only
//! its code.
//!
//! The control (rule 23): `Plant::ClauseAtWholeExpr` (rules/obligation_site.rs `whole_planted`)
//! files every such obligation at the whole expression the item was used at, as before this
//! piece; then every case's E0277 is at its `whole` range and none at its `want`, so each test
//! above fails under it. The planted edit is exactly: in `Machine::clause_at` and
//! `Machine::written_cause`, return the whole expression (`At::Expr(expr)`, or `cause`) first.

use super::check_body;
use crate::body::IntSuffix;
use crate::columns::Checked;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, List, Mutbl, Range, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The keys the bodies need.
#[derive(Clone, Copy, Debug)]
struct Items {
    b: ItemKey,
    w: ItemKey,
    c1: ItemKey,
    c2: ItemKey,
    c3: ItemKey,
    c4: ItemKey,
    c5: ItemKey,
    c6: ItemKey,
    show: ItemKey,
}

/// The world of the module doc. No `Sized` lang item, so no implied `Sized` clause: each item
/// has exactly the clause written.
fn world() -> (Fx, Items) {
    let mut fx = Fx::new();
    let show = fx.tr("Show", 1, std::vec::Vec::new(), &[]);
    let u8_ = fx.u8();
    let unit = fx.unit();
    fx.trait_fn(show, "shown", false, &[], u8_);
    let b_k = fx.strukt("B", 0, &[]);
    let b = fx.adt(b_k, &[]);
    let p0 = fx.param(0);
    let t_show = fx.bound(show, &[p0]);
    fx.func("grant", 1, &[u8_], u8_, std::vec![t_show]);
    let r_p0 = fx.r(Mutbl::Not, p0);
    fx.func("need", 1, &[r_p0], u8_, std::vec![t_show]);
    let w_k = fx.strukt("W", 1, &[]);
    let w_p0 = fx.adt(w_k, &[p0]);
    let imp = fx.imp("impl<T> W<T> where T: Show", 1, w_p0, None, &[], std::vec![t_show]);
    let r_w = fx.r(Mutbl::Not, w_p0);
    fx.impl_fn(imp, "bounded", true, &[r_w], u8_);
    fx.impl_fn(imp, "make", false, &[], w_p0);
    let own = fx.imp("impl<T> W<T>", 1, w_p0, None, &[], std::vec::Vec::new());
    fx.impl_fn_own(own, "get", true, 0, &[r_w], u8_, std::vec![t_show]);
    let w_b = fx.adt(w_k, &[b]);
    let c1 = fx.func("c1", 0, &[], u8_, std::vec::Vec::new());
    let c2 = fx.func("c2", 0, &[b], u8_, std::vec::Vec::new());
    let c3 = fx.func("c3", 0, &[w_b], u8_, std::vec::Vec::new());
    let c4 = fx.func("c4", 0, &[], unit, std::vec::Vec::new());
    let c5 = fx.func("c5", 0, &[], u8_, std::vec::Vec::new());
    let c6 = fx.func("c6", 0, &[w_b], u8_, std::vec::Vec::new());
    fx.finish();
    (fx, Items { b: b_k, w: w_k, c1, c2, c3, c4, c5, c6, show })
}

/// The five bodies of the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    /// c1: a bound on a path's generic parameter.
    GenericArg,
    /// c2: a bound on a function's parameter type.
    FnParam,
    /// c3: a method's receiver bound.
    Receiver,
    /// c4: a where-clause on an impl, named by no input and no written argument.
    ImplWhere,
    /// c5: a trait item's `Self` written as a qualified path's anchor.
    Anchor,
}

/// A built body: where its E0277 must be (`want`) and the whole expression the item was used at
/// (`whole`, where the control files it).
struct Built {
    b: Bb,
    root: ExprId,
    owner: ItemKey,
    want: Range,
    whole: Range,
}

/// The range a fixture expression has: its id, one byte.
const fn at_expr(e: ExprId) -> Range {
    Range { start: e.0, end: e.0 + 1 }
}

fn build(it: &Items, case: Case) -> Built {
    let mut b = Bb::new();
    match case {
        Case::GenericArg => {
            let bb = b.ty(PNode::Adt(it.b, List::EMPTY));
            let g = b.path_args_ranged("grant", &[bb]);
            let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
            let call = b.call(g.expr, &[one]);
            let root = b.block(&[], Some(call));
            let want = g.first_arg.expect("`grant::<B>` writes one argument");
            Built { b, root, owner: it.c1, want, whole: at_expr(g.expr) }
        }
        Case::FnParam => {
            let p = b.param("b");
            let n = b.path_ranged(&["need"]);
            let x = b.local(p);
            let rx = b.borrow(Mutbl::Not, x);
            let call = b.call(n.expr, &[rx]);
            let root = b.block(&[], Some(call));
            Built { b, root, owner: it.c2, want: at_expr(rx), whole: at_expr(n.expr) }
        }
        Case::Receiver => {
            let p = b.param("w");
            let x = b.local(p);
            let m = b.method_ranged(x, "get", &[], &[]);
            let root = b.block(&[], Some(m.expr));
            Built { b, root, owner: it.c3, want: at_expr(x), whole: at_expr(m.expr) }
        }
        Case::ImplWhere => {
            let w = b.bind("w");
            let mk = b.path_ranged(&["W", "make"]);
            let call = b.call(mk.expr, &[]);
            let bb = b.ty(PNode::Adt(it.b, List::EMPTY));
            let args = b.t.intern_list(&[bb]).expect("intern");
            let w_b = b.ty(PNode::Adt(it.w, args));
            let s = b.let_(w, Some(w_b), call);
            let root = b.block(&[s], None);
            Built { b, root, owner: it.c4, want: mk.name, whole: at_expr(mk.expr) }
        }
        Case::Anchor => {
            let bb = b.ty(PNode::Adt(it.b, List::EMPTY));
            let q = b.qpath_as_ranged(bb, "Show", &["shown"]);
            let call = b.call(q.expr, &[]);
            let root = b.block(&[], Some(call));
            let want = q.anchor.expect("a qualified path has an anchor");
            Built { b, root, owner: it.c5, want, whole: at_expr(q.expr) }
        }
    }
}

/// A body's E0277s: how many, where the first is, and how many are at `at`; and how many
/// findings of another code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Unmet {
    count: u32,
    first: Option<Range>,
    at: u32,
    others: u32,
}

fn unmet(c: &Checked<'_>, at: Range) -> Unmet {
    let mut u = Unmet { count: 0, first: None, at: 0, others: 0 };
    for d in c.diags.iter() {
        if d.code != DiagCode::Unsatisfied {
            u.others += 1;
            continue;
        }
        u.count += 1;
        if u.first.is_none() {
            u.first = Some(d.at);
        }
        if d.at == at {
            u.at += 1;
        }
    }
    u
}

/// The case checked with `plant`: its E0277s counted at `want` (and at `whole` beside).
fn run(case: Case, plant: Plant) -> (Built, Unmet, Unmet) {
    let (fx, it) = world();
    let built = build(&it, case);
    let c = check_body(&fx, &built.b, built.owner, built.root, plant, InferPlant::None);
    let at_want = unmet(&c, built.want);
    let at_whole = unmet(&c, built.whole);
    (built, at_want, at_whole)
}

/// Every E0277 of `case` is at its `want`, there is at least one, and nothing else is found.
fn assert_filed_at_want(case: Case, what: &str) {
    let (built, u, _) = run(case, Plant::None);
    assert!(
        u.count > 0 && u.at == u.count && u.others == 0,
        "{what}: want every E0277 at {:?}, got {} E0277 ({} there, the first at {:?}) and {} other findings",
        built.want,
        u.count,
        u.at,
        u.first,
        u.others,
    );
}

/// c1, `grant::<B>(1u8)`: `T: Show` is filed at `B` in `::<B>`, the argument written for `T`,
/// not at the path `grant::<B>` (the launcher's `guest::grant::<Io>(&io, name)` form).
/// Prints: `test suite::obligation_range::bound_on_a_path_generic_parameter_is_at_that_argument ... ok`
#[test]
fn bound_on_a_path_generic_parameter_is_at_that_argument() {
    assert_filed_at_want(Case::GenericArg, "grant::<B>(1u8)");
}

/// c2, `need(&b)`: `T: Show` with `t: &T` is filed at `&b`, the argument passed for `t`,
/// although the path's segment `need` has a range too.
/// Prints: `test suite::obligation_range::bound_on_a_parameter_type_is_at_the_argument_passed ... ok`
#[test]
fn bound_on_a_parameter_type_is_at_the_argument_passed() {
    assert_filed_at_want(Case::FnParam, "need(&b)");
}

/// c3, `w.get()`: the method's own `T: Show`, `T` in the receiver's type `&W<T>`, is filed at
/// the receiver `w`, not at the call or at the name `get`.
/// Prints: `test suite::obligation_range::receiver_bound_is_at_the_receiver ... ok`
#[test]
fn receiver_bound_is_at_the_receiver() {
    assert_filed_at_want(Case::Receiver, "w.get()");
}

/// c4, `let w: W<B> = W::make();`: the impl's `T: Show` is named by no input of `make` and by no
/// written argument, so it is filed at the segment `make` that names the method.
/// Prints: `test suite::obligation_range::impl_where_clause_is_at_the_segment_naming_the_method ... ok`
#[test]
fn impl_where_clause_is_at_the_segment_naming_the_method() {
    assert_filed_at_want(Case::ImplWhere, "W::make()");
}

/// c5, `<B as Show>::shown()`: `B: Show`, asked by the anchor and by `shown`'s own clause
/// `Self: Show`, is filed at the anchor's type `B`.
/// Prints: `test suite::obligation_range::anchor_self_bound_is_at_the_anchor_type ... ok`
#[test]
fn anchor_self_bound_is_at_the_anchor_type() {
    assert_filed_at_want(Case::Anchor, "<B as Show>::shown()");
}

/// c6, `w.bounded()`: the IMPL's `T: Show` fails in the probe, so nothing applies. This is
/// rustc's E0599 "the method `bounded` exists for struct `W<B>`, but its trait bounds were not
/// satisfied", at the method's name. The finding carries `B: Show` (rule 19) and no E0277.
/// Prints: `test suite::obligation_range::impl_bound_unmet_is_e0599_at_the_method_name ... ok`
#[test]
fn impl_bound_unmet_is_e0599_at_the_method_name() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let p = b.param("w");
    let x = b.local(p);
    let m = b.method_ranged(x, "bounded", &[], &[]);
    let root = b.block(&[], Some(m.expr));
    let c = check_body(&fx, &b, it.c6, root, Plant::None, InferPlant::None);
    let found: std::vec::Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(found, [(DiagCode::NoMethod, m.name)], "w.bounded(): want one E0599 at the name `bounded`");
    let d = c.diags[0];
    assert_eq!(d.name, Some(crate::ids::Name::of("bounded")), "the finding names the method");
    let Some(crate::diag::Because::Unmet { goal: crate::diag::UnmetGoal::Trait { trait_, args }, leaf: None }) = d.because else {
        panic!("w.bounded(): want the unmet `B: Show` as the reason, got {:?}", d.because);
    };
    assert_eq!(trait_, it.show, "the unmet bound is `Show`");
    let self_ty = c.types.view().list(args).expect("the bound's arguments in the body's table")[0];
    assert_eq!(c.types.view().get(self_ty), Some(&PNode::Adt(it.b, List::EMPTY)), "the unmet bound's self type is `B`");
}

/// The control: with every obligation filed at the whole expression (`Plant::ClauseAtWholeExpr`)
/// each case's E0277s are all at `whole` and none at `want`, so each test above fails under the
/// planted bug. (c6 has no E0277: its E0599 is the probe's, and no obligation is filed.)
/// Prints "whole expression: every case's E0277 left its narrow place (5 of 5)".
#[test]
fn control_whole_expression_fails_all_four() {
    let mut caught = 0u32;
    for case in [Case::GenericArg, Case::FnParam, Case::Receiver, Case::ImplWhere, Case::Anchor] {
        let (built, at_want, at_whole) = run(case, Plant::ClauseAtWholeExpr);
        assert!(
            at_want.count > 0 && at_want.at == 0 && at_whole.at == at_whole.count,
            "{case:?} under the plant: want every E0277 at the whole expression {:?} and none at {:?}; got {} E0277, the first at {:?}",
            built.whole,
            built.want,
            at_want.count,
            at_want.first,
        );
        caught += 1;
    }
    std::println!("whole expression: every case's E0277 left its narrow place ({caught} of 5)");
    assert_eq!(caught, 5);
}
