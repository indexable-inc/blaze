//! Fixtures: bodies built with the type checker's fixture world, checked by the real type
//! checker, lowered here and checked by blaze-flow. Each expectation is stated from a written
//! rule (quoted per test); each catch has a control with a planted bug that hides it, and every
//! test prints the line it caught.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{Checked, ExprId, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome};

use crate::{AdtFacts, CalleeFacts, Facts, Plant, lower_planted};

/// Every ADT of the fixtures is a plain struct with no `Copy` impl, no lifetimes, needing drop.
struct Plain;

impl Facts for Plain {
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: false, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
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

fn checked<'b>(fx: &Fx, b: &'b Bb, owner: ItemKey, root: ExprId) -> Checked<'static> {
    let mut cx = InferCtx::new_in(Alloc::global());
    match blaze_typeck::check(&b.view(owner, root), fx, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// The findings of one body under `plant`, one line each, and the flow's text.
fn findings(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, plant: Plant) -> (vec::Vec<String>, String) {
    let c = checked(fx, b, owner, root);
    let view = b.view(owner, root);
    let lowered = match lower_planted(&view, &c, &Plain, plant, Alloc::global()) {
        Ok(l) => l,
        Err(r) => panic!("refused: {r:?}"),
    };
    let mut text = String::new();
    lowered.flow.print(&mut text).expect("print");
    let rec = check(&lowered.flow, Alloc::global()).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    (out, text)
}

/// `struct S; fn g(_: &S); fn f(s: S)`, and the fixture world finished by `body`.
fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let _g = fx.func("g", 0, &[rs], unit, vec![]);
    let _h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    (fx, s, f)
}

/// The Reference, "Moved and copied types": "After moving out of a place expression that
/// evaluates to a local variable, the location is deinitialized and cannot be read from again
/// until it is reinitialized." `fn f(s: S) { let a = s; let b = s; }`: the second read is a use of
/// a moved value. Control: lowering every read as a copy (`Plant::AllCopy`) hides it.
#[test]
fn moved_twice() {
    let (fx, _s, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (a, bb) = (b.bind("a"), b.bind("b"));
    let r1 = b.local(s);
    let st1 = b.let_(a, None, r1);
    let r2 = b.local(s);
    let st2 = b.let_(bb, None, r2);
    let root = b.block(&[st1, st2], None);
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("moved_twice: {got:?}");
    assert!(got.iter().any(|l| l.contains("UseOfMoved")), "the second move is caught:\n{text}");
    let (planted, _) = findings(&fx, &b, f, root, Plant::AllCopy);
    std::println!("moved_twice, planted AllCopy: {planted:?}");
    assert!(planted.is_empty(), "the control hides it");
}

/// `fn f(s: S) { let a = s; h(a); }`: one move each: no finding.
#[test]
fn moved_once() {
    let (fx, _s, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let a = b.bind("a");
    let r1 = b.local(s);
    let st1 = b.let_(a, None, r1);
    let h = b.path(&["h"]);
    let ra = b.local(a);
    let call = b.call(h, &[ra]);
    let st2 = b.semi(call);
    let root = b.block(&[st1, st2], None);
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("moved_once: {got:?}");
    assert!(got.is_empty(), "{got:?}\n{text}");
}

/// Polonius: "It is an error to invalidate a loan that is live at a given point."
/// `fn f(s: S) { let r = &s; h(s); g(r); }`: moving `s` writes it deeply while the loan, through
/// `r`, is live until `g(r)`. Control: an assignment that relates no origins (`Plant::NoRelate`)
/// leaves the loan in the borrow's temporary, dead before the move: no finding.
#[test]
fn move_while_borrowed() {
    let (fx, _s, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let r = b.bind("r");
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let st1 = b.let_(r, None, bor);
    let h = b.path(&["h"]);
    let ls2 = b.local(s);
    let c1 = b.call(h, &[ls2]);
    let st2 = b.semi(c1);
    let g = b.path(&["g"]);
    let lr = b.local(r);
    let c2 = b.call(g, &[lr]);
    let st3 = b.semi(c2);
    let root = b.block(&[st1, st2, st3], None);
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("move_while_borrowed: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}\n{text}");
    let (planted, _) = findings(&fx, &b, f, root, Plant::NoRelate);
    std::println!("move_while_borrowed, planted NoRelate: {planted:?}");
    assert!(!planted.iter().any(|l| l.starts_with("borrow")), "the control hides it");
}

/// The same with the use first: `let r = &s; g(r); h(s);`: the loan is dead at the move (RFC
/// 2094: "a variable is live if the current value that it holds may be used later").
#[test]
fn move_after_last_use() {
    let (fx, _s, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let r = b.bind("r");
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let st1 = b.let_(r, None, bor);
    let g = b.path(&["g"]);
    let lr = b.local(r);
    let c2 = b.call(g, &[lr]);
    let st2 = b.semi(c2);
    let h = b.path(&["h"]);
    let ls2 = b.local(s);
    let c1 = b.call(h, &[ls2]);
    let st3 = b.semi(c1);
    let root = b.block(&[st1, st2, st3], None);
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("move_after_last_use: {got:?}");
    assert!(got.is_empty(), "{got:?}\n{text}");
}
