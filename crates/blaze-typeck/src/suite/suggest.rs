//! The fix offered for a one-segment value name that does not resolve (rules/suggest.rs), one
//! fixture per rule, each with the control that shows the rule doing the work. The owner's case
//! (2026-11-16, on the website):
//!
//! ```text
//! fn add(a: u32, b: u32) -> u32 { a + c }
//! ```
//!
//! was offered `a` (one letter from `c`); `b` is right: it is the parameter nobody used.
//!
//! A fixture's body has no source text (the builder gives each expression `i..i + 1` and each
//! pattern `1000 + i..1001 + i`): [`Names`] maps those ranges to the names as written, which is
//! what `pick` reads from an item's text.

use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use super::check_body;
use crate::body::{BinOp, Expr, IntSuffix};
use crate::columns::Checked;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, PatId, Range, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};
use crate::rules::suggest::{NameText, Pick, Rules, Taken, pick};

/// The names as written, by the builder's ranges.
struct Names(Vec<(Range, String)>);

impl NameText for Names {
    fn bytes(&self, r: Range) -> Option<&[u8]> {
        self.0.iter().find(|(at, _)| *at == r).map(|(_, s)| s.as_bytes())
    }
}

/// A body under construction and the text of its names.
struct Case {
    b: Bb,
    names: Names,
}

impl Case {
    fn new() -> Case {
        Case { b: Bb::new(), names: Names(Vec::new()) }
    }

    fn pat_named(&mut self, p: PatId, name: &str) {
        self.names.0.push((Range { start: 1000 + p.0, end: 1001 + p.0 }, String::from(name)));
    }

    fn param(&mut self, name: &str) -> PatId {
        let p = self.b.param(name);
        self.pat_named(p, name);
        p
    }

    fn bind(&mut self, name: &str) -> PatId {
        let p = self.b.bind(name);
        self.pat_named(p, name);
        p
    }

    /// A one-segment path that resolves to nothing in the world.
    fn hole(&mut self, name: &str) -> ExprId {
        let e = self.b.path(&[name]);
        self.names.0.push((Range { start: e.0, end: e.0 + 1 }, String::from(name)));
        e
    }

    fn add(&mut self, lhs: ExprId, rhs: ExprId) -> ExprId {
        self.b.e(Expr::Binary { op: BinOp::Add, lhs, rhs })
    }

    fn name_of(&self, p: PatId) -> &str {
        let r = Range { start: 1000 + p.0, end: 1001 + p.0 };
        self.names.0.iter().find(|(at, _)| *at == r).map_or("?", |(_, s)| s.as_str())
    }
}

/// A world holding one fn `f` with these inputs and output.
fn world(inputs: impl Fn(&mut Fx) -> Vec<PTy>, output: impl Fn(&mut Fx) -> PTy) -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let ins = inputs(&mut fx);
    let out = output(&mut fx);
    let f = fx.func("f", 0, &ins, out, Vec::new());
    fx.finish();
    (fx, f)
}

fn u32s(n: usize) -> impl Fn(&mut Fx) -> Vec<PTy> {
    move |fx: &mut Fx| (0..n).map(|_| fx.u32()).collect()
}

fn check(fx: &Fx, c: &Case, owner: ItemKey, root: ExprId) -> Checked<'static> {
    check_body(fx, &c.b, owner, root, Plant::None, InferPlant::None)
}

/// The pick for the record's only hole, said as a test reads it.
fn said(c: &Case, done: &Checked<'_>, rules: Rules) -> String {
    let s = &done.suggests;
    assert_eq!(s.holes.len(), 1, "one hole: {:?}", s);
    let named = |t: Taken| match t {
        Taken::Local(p) => String::from(c.name_of(p)),
    };
    match pick(s, &s.holes[0], &c.names, rules, Alloc::global()).expect("the names are in the text") {
        Pick::Nothing => String::from("nothing"),
        Pick::Unused(x) => std::format!("unused {}", named(x.taken)),
        Pick::Spelled(x) => std::format!("spelled {}", named(x.taken)),
        Pick::Tied(x, y) => std::format!("tied {} {}", named(x.taken), named(y.taken)),
    }
}

/// (1) The owner's case: `b`, the parameter nobody used. Without rule (2) `a` and `b` are equal
/// on every count left (both one letter from `c`, both parameters): no fix.
#[test]
fn the_owners_case_offers_the_unused_parameter() {
    let (fx, f) = world(u32s(2), |fx| fx.u32());
    let mut c = Case::new();
    let a = c.param("a");
    c.param("b");
    let la = c.b.local(a);
    let hole = c.hole("c");
    let sum = c.add(la, hole);
    let root = c.b.block(&[], Some(sum));
    let done = check(&fx, &c, f, root);
    assert_eq!(said(&c, &done, Rules::All), "unused b");
    assert_eq!(said(&c, &done, Rules::WithoutUnused), "tied a b", "the control: rule (2) decides");
    let s = &done.suggests;
    let fix = pick(s, &s.holes[0], &c.names, Rules::All, Alloc::global()).expect("text").fix(&s.holes[0]);
    assert_eq!(fix.map(|x| (x.at, x.from)), Some((Range { start: hole.0, end: hole.0 + 1 }, Range { start: 1001, end: 1002 })));
}

/// (2) The type filter: `s: &str` never fits beside a `u32` operand; `a` is used, so the pick
/// falls to spelling. And at a tail, the return type is the expectation.
#[test]
fn a_candidate_of_the_wrong_type_is_never_offered() {
    let str_ref = |fx: &mut Fx| {
        let s = fx.ty(PNode::Str);
        fx.r(Mutbl::Not, s)
    };
    let (fx, f) = world(|fx| std::vec![fx.u32(), str_ref(fx)], |fx| fx.u32());
    let mut c = Case::new();
    let a = c.param("a");
    c.param("s");
    let la = c.b.local(a);
    let hole = c.hole("c");
    let sum = c.add(la, hole);
    let root = c.b.block(&[], Some(sum));
    let done = check(&fx, &c, f, root);
    assert_eq!(said(&c, &done, Rules::All), "spelled a");
    // The tail itself: `{ c }` expects `u32` from the return type; `a` (unused here) fits.
    let mut t = Case::new();
    t.param("a");
    t.param("s");
    let hole = t.hole("c");
    let root = t.b.block(&[], Some(hole));
    let done = check(&fx, &t, f, root);
    assert_eq!(said(&t, &done, Rules::All), "unused a");
    let cands = done.suggests.cands_of(&done.suggests.holes[0]);
    assert!(cands.iter().all(|x| x.taken != Taken::Local(PatId(1))), "`s` is dropped by its type: {cands:?}");
}

/// (3) Spelling, with transposition: `coutn` is `count` with two letters swapped (distance 1;
/// without transposition 2, past the limit of 5 / 3 = 1). `other` is too far to be offered.
#[test]
fn a_misspelled_local_is_offered_by_spelling() {
    let (fx, f) = world(u32s(0), |fx| fx.u32());
    let mut c = Case::new();
    let count = c.bind("count");
    let one = c.b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let s1 = c.b.let_(count, None, one);
    let other = c.bind("other");
    let lc = c.b.local(count);
    let s2 = c.b.let_(other, None, lc);
    let hole = c.hole("coutn");
    let lo = c.b.local(other);
    let sum = c.add(hole, lo);
    let root = c.b.block(&[s1, s2], Some(sum));
    let done = check(&fx, &c, f, root);
    assert_eq!(said(&c, &done, Rules::All), "spelled count");
}

/// (4) Two unused `u32` parameters, equal on every count: no fix, both named.
#[test]
fn a_tie_offers_no_fix_and_names_both() {
    let (fx, f) = world(u32s(3), |fx| fx.u32());
    let mut c = Case::new();
    let a = c.param("a");
    c.param("x");
    c.param("y");
    let la = c.b.local(a);
    let hole = c.hole("c");
    let sum = c.add(la, hole);
    let root = c.b.block(&[], Some(sum));
    let done = check(&fx, &c, f, root);
    assert_eq!(said(&c, &done, Rules::All), "tied x y");
    let s = &done.suggests;
    let p = pick(s, &s.holes[0], &c.names, Rules::All, Alloc::global()).expect("text");
    assert_eq!(p.fix(&s.holes[0]), None);
}

/// (6) Nothing in scope: no help at all.
#[test]
fn nothing_in_scope_offers_nothing() {
    let (fx, f) = world(u32s(0), |fx| fx.u32());
    let mut c = Case::new();
    let hole = c.hole("c");
    let root = c.b.block(&[], Some(hole));
    let done = check(&fx, &c, f, root);
    assert_eq!(said(&c, &done, Rules::All), "nothing");
}

/// (7a) The failing place is no use: with the hole written twice, `b` is still the unused one
/// at each. (7b) `_b` is unused on purpose: never offered as "never used"; `a` wins by spelling.
#[test]
fn the_hole_is_no_use_and_an_underscore_is_unused_on_purpose() {
    let (fx, f) = world(u32s(2), |fx| fx.u32());
    let mut c = Case::new();
    let a = c.param("a");
    c.param("b");
    let la = c.b.local(a);
    let h1 = c.hole("c");
    let h2 = c.hole("c");
    let s1 = c.add(la, h1);
    let sum = c.add(s1, h2);
    let root = c.b.block(&[], Some(sum));
    let done = check(&fx, &c, f, root);
    let s = &done.suggests;
    assert_eq!(s.holes.len(), 2);
    for h in &s.holes {
        assert!(matches!(pick(s, h, &c.names, Rules::All, Alloc::global()), Ok(Pick::Unused(x)) if x.taken == Taken::Local(PatId(1))), "{s:?}");
    }
    let mut u = Case::new();
    let a = u.param("a");
    u.param("_b");
    let la = u.b.local(a);
    let hole = u.hole("c");
    let sum = u.add(la, hole);
    let root = u.b.block(&[], Some(sum));
    let done = check(&fx, &u, f, root);
    assert_eq!(said(&u, &done, Rules::All), "spelled a");
}

/// The cost: nothing for a body where every name resolves; one pass and a scope walk for a body
/// with a hole among 200 locals.
#[test]
fn the_search_costs_nothing_on_a_clean_body_and_is_counted_on_200_locals() {
    let (fx, f) = world(u32s(2), |fx| fx.u32());
    let mut c = Case::new();
    let a = c.param("a");
    let b = c.param("b");
    let la = c.b.local(a);
    let lb = c.b.local(b);
    let sum = c.add(la, lb);
    let root = c.b.block(&[], Some(sum));
    let done = check(&fx, &c, f, root);
    assert!(done.diags.is_empty(), "{:?}", done.diags);
    assert_eq!((done.suggests.holes.len(), done.suggests.cands.len(), done.suggests.steps), (0, 0, 0));

    let (fx, f) = world(u32s(0), |fx| fx.u32());
    let mut big = Case::new();
    let mut stmts = Vec::new();
    for i in 0..200 {
        let p = big.bind(&std::format!("x{i}"));
        let one = big.b.int(Some(IntSuffix::Uint(UintTy::U32)));
        stmts.push(big.b.let_(p, None, one));
    }
    let hole = big.hole("x2000");
    let root = big.b.block(&stmts, Some(hole));
    let done = check(&fx, &big, f, root);
    let s = &done.suggests;
    assert_eq!(s.cands.len(), 200);
    // 202 expressions (200 literals, the hole, the block), 2 ancestors, 200 patterns: the floor
    // (each expression and each pattern once). With a list for the shadowing check it was 20304.
    assert_eq!(s.steps, 202 + 2 + 200, "200 locals: the count, exactly");
}
