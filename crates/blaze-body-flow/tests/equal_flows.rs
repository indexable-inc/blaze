//! The oracle of the one lowering (D119 P3): for each source, A is blaze-flow-lower's flow and
//! B the projection of blaze-body-lower's body; their canonical texts (`blaze_body_flow::canon`,
//! whose module doc says what it renumbers and joins) must be equal. One source per family:
//! straight-line code, borrows, a call that can unwind past droppable locals, `if` and `match`,
//! a loop whose `break` carries a value, a struct update, a closure.
//!
//! Each test prints `<family>: equal, N lines`; on a difference it prints both texts and the
//! body's own text. Run with `--features oracle` (Cargo.toml).

#![feature(allocator_api)]

use std::println;
use std::string::String;
use std::vec;

use blaze_body_flow::{canon, project};
use blaze_body_lower::BodyFacts;
use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts};
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{BodyView, Checked, Expr, ExprId, FieldInit, InferCtx, ItemKey, List, Mutbl, Name, Needs, Outcome};

/// Every ADT a plain struct needing drop, no `Copy`, no lifetimes; every callee a fn item.
struct Plain {
    closures: bool,
}

impl Facts for Plain {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        &blaze_flow_lower::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn closure_bodies_checked(&self) -> bool {
        self.closures
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

impl BodyFacts for Plain {
    fn trait_item(&self, _: ItemKey) -> Option<bool> {
        Some(false)
    }
}

const PLAIN: Plain = Plain { closures: false };

/// A test's root allocator capability (D102).
fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn checked(view: &BodyView<'_>, fx: &Fx) -> Checked<'static> {
    let mut cx = InferCtx::new_in(alloc());
    match blaze_typeck::check(view, fx, &mut cx, &Needs::new_in(alloc()), alloc()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// Lowers `view` both ways and asserts the canonical texts equal.
fn equal(family: &str, fx: &Fx, view: &BodyView<'_>, facts: &Plain) {
    let c = checked(view, fx);
    let old = match blaze_flow_lower::lower(view, &c, facts, alloc()) {
        Ok(l) => l,
        Err(r) => panic!("{family}: blaze-flow-lower refused {r:?}"),
    };
    let mut a = String::new();
    canon(&old.flow, &mut a, alloc()).expect("a String takes every write");

    let body = match blaze_body_lower::lower(view, &c, facts, alloc()) {
        Ok(b) => b,
        Err(e) => panic!("{family}: blaze-body-lower refused {e:?}"),
    };
    let mut body_text = String::new();
    blaze_body::print(&body.view(), &mut body_text).expect("a lowered body prints");
    let v = match blaze_body::verify(&body) {
        Ok(v) => v,
        Err(e) => panic!("{family}: verify refused {e:?}:\n{body_text}"),
    };
    let new = match project(v, alloc()) {
        Ok(p) => p,
        Err(e) => panic!("{family}: project refused: {e}\n{body_text}"),
    };
    let mut b = String::new();
    canon(&new.flow, &mut b, alloc()).expect("a String takes every write");
    assert_eq!(a, b, "{family}: the flows differ\n--- blaze-flow-lower\n{a}--- projected body\n{b}--- body\n{body_text}");
    println!("{family}: equal, {} lines", a.lines().count());
}

/// `struct S; fn g(_: &S); fn h(_: S); fn mk() -> S; fn f(s: S)`.
fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let _g = fx.func("g", 0, &[rs], unit, vec![]);
    let _h = fx.func("h", 0, &[st], unit, vec![]);
    let _mk = fx.func("mk", 0, &[], st, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    (fx, f)
}

/// `name(args..)` as a statement.
fn call_stmt(b: &mut Bb, name: &str, args: &[ExprId]) -> blaze_typeck::StmtId {
    let callee = b.path(&[name]);
    let c = b.call(callee, args);
    b.semi(c)
}

/// A fresh expression slot to overwrite: the fixture has no builder for `if`, `loop`, `break`.
fn slot(b: &mut Bb) -> ExprId {
    b.bool()
}

/// `fn f(s: S) { let a = s; h(a); }`.
#[test]
fn straight_line() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let a = b.bind("a");
    let ls = b.local(s);
    let st1 = b.let_(a, None, ls);
    let la = b.local(a);
    let st2 = call_stmt(&mut b, "h", &[la]);
    let root = b.block(&[st1, st2], None);
    equal("straight_line", &fx, &b.view(f, root), &PLAIN);
}

/// `fn f(s: S) { let r = &s; g(r); let q = &s; g(q); h(s); }`.
#[test]
fn borrows() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (r, q) = (b.bind("r"), b.bind("q"));
    let ls = b.local(s);
    let br = b.borrow(Mutbl::Not, ls);
    let st1 = b.let_(r, None, br);
    let lr = b.local(r);
    let st2 = call_stmt(&mut b, "g", &[lr]);
    let ls2 = b.local(s);
    let bq = b.borrow(Mutbl::Not, ls2);
    let st3 = b.let_(q, None, bq);
    let lq = b.local(q);
    let st4 = call_stmt(&mut b, "g", &[lq]);
    let ls3 = b.local(s);
    let st5 = call_stmt(&mut b, "h", &[ls3]);
    let root = b.block(&[st1, st2, st3, st4, st5], None);
    equal("borrows", &fx, &b.view(f, root), &PLAIN);
}

/// `fn f(s: S) { let a = s; let t = mk(); h(t); }`: `a` is live across two calls that can
/// unwind, so the body has cleanup blocks the flow has not.
#[test]
fn calls_with_unwind() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (a, t) = (b.bind("a"), b.bind("t"));
    let ls = b.local(s);
    let st1 = b.let_(a, None, ls);
    let mk = b.path(&["mk"]);
    let made = b.call(mk, &[]);
    let st2 = b.let_(t, None, made);
    let lt = b.local(t);
    let st3 = call_stmt(&mut b, "h", &[lt]);
    let root = b.block(&[st1, st2, st3], None);
    equal("calls_with_unwind", &fx, &b.view(f, root), &PLAIN);
}

/// `fn f(s: S) { if true { h(s); } else { } }`.
#[test]
fn if_else() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let cond = b.bool();
    let ls = b.local(s);
    let st = call_stmt(&mut b, "h", &[ls]);
    let then = b.block(&[st], None);
    let else_ = b.block(&[], None);
    let at = slot(&mut b);
    b.exprs[at.ix()] = Expr::If { cond, then, else_: Some(else_) };
    let st_if = b.semi(at);
    let root = b.block(&[st_if], None);
    equal("if_else", &fx, &b.view(f, root), &PLAIN);
}

/// `fn f(s: S) { match s { x => h(x) } }`.
#[test]
fn match_binding() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let x = b.bind("x");
    let ls = b.local(s);
    let h = b.path(&["h"]);
    let lx = b.local(x);
    let arm = b.call(h, &[lx]);
    let m = b.match_(ls, &[(x, arm)]);
    let st = b.semi(m);
    let root = b.block(&[st], None);
    equal("match_binding", &fx, &b.view(f, root), &PLAIN);
}

/// `fn f(s: S) { let x = loop { break s; }; h(x); }`.
#[test]
fn loop_break_value() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let x = b.bind("x");
    let lp = slot(&mut b);
    let ls = b.local(s);
    let brk = slot(&mut b);
    b.exprs[brk.ix()] = Expr::Break { value: Some(ls), target: lp };
    let st_brk = b.semi(brk);
    let body = b.block(&[st_brk], None);
    b.exprs[lp.ix()] = Expr::Loop { body };
    let st1 = b.let_(x, None, lp);
    let lx = b.local(x);
    let st2 = call_stmt(&mut b, "h", &[lx]);
    let root = b.block(&[st1, st2], None);
    equal("loop_break_value", &fx, &b.view(f, root), &PLAIN);
}

/// `struct S; struct P { a: S, c: S }; fn h(_: P); fn f(p: P, s: S) { let q = P { a: s, ..p }; h(q); }`.
#[test]
fn struct_update() {
    let mut fx = Fx::new();
    let sk = fx.strukt("S", 0, &[]);
    let st = fx.adt(sk, &[]);
    let pk = fx.strukt("P", 0, &[("a", st), ("c", st)]);
    let pt = fx.adt(pk, &[]);
    let unit = fx.unit();
    let _h = fx.func("h", 0, &[pt], unit, vec![]);
    let f = fx.func("f", 0, &[pt, st], unit, vec![]);
    fx.finish();

    let mut b = Bb::new();
    let p = b.param("p");
    let s = b.param("s");
    let q = b.bind("q");
    let ls = b.local(s);
    let base = b.local(p);
    let up = b.path(&["P"]);
    let Expr::Path(path) = b.exprs[up.ix()] else { panic!("`Bb::path` made other than a path expression") };
    b.exprs[up.ix()] = Expr::Struct { path, fields: List { start: 0, len: 1 }, base: blaze_typeck::StructBase::Expr(base) };
    let st1 = b.let_(q, None, up);
    let lq = b.local(q);
    let st2 = call_stmt(&mut b, "h", &[lq]);
    let root = b.block(&[st1, st2], None);
    let inits = [FieldInit { name: Name::of("a"), expr: ls }];
    let mut view = b.view(f, root);
    view.field_inits = &inits;
    equal("struct_update", &fx, &view, &PLAIN);
}

/// `fn f(s: S) { let c = || g(&s); h(s); }`: the closure borrows `s` for as long as `c` lives,
/// which ends before `h(s)`; its body is checked on its own (`closure_bodies_checked`).
#[test]
fn closure_capture() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let c = b.bind("c");
    let ls = b.local(s);
    let rs = b.borrow(Mutbl::Not, ls);
    let g = b.path(&["g"]);
    let body = b.call(g, &[rs]);
    let clo = b.closure(&[], body);
    let st1 = b.let_(c, None, clo);
    let ls2 = b.local(s);
    let st2 = call_stmt(&mut b, "h", &[ls2]);
    let root = b.block(&[st1, st2], None);
    equal("closure_capture", &fx, &b.view(f, root), &Plain { closures: true });
}
