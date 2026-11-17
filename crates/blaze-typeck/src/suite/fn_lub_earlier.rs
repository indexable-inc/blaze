//! The other direction of a coercion site's least upper bound, and the intrinsic that is never a
//! fn pointer (rules/coerce_lub.rs `lub_earlier_to_new`, rules/reify.rs).
//!
//! The Reference, "Type coercions", "Least upper bound coercions": the lub of the sources' types
//! "T0..Tn" is found one by one, and where a later source's type is the more general one, the
//! earlier sources coerce to it: in `if c { f } else { p }` with `f` a fn item and `p: fn(u8) ->
//! u8`, the second arm cannot become the fn item, so the first arm becomes the fn pointer
//! (`ReifyFnPointer` recorded on it) and the `if` is `fn(u8) -> u8`. Only earlier sources that
//! carry no adjustment of their own are coerced again; otherwise the types must be equal.
//!
//! The pinned library, core/src/intrinsics/mod.rs lines 7 and 13 ("The actual implementations
//! live in the compiler.", "Intrinsics don't need a body."): an intrinsic has no address, so it
//! is never reified, at one site or in a lub: [`DiagCode::IntrinsicToFnPointer`] (E0308), once,
//! at the intrinsic's own expression.
//!
//! ```text
//! fn f(s: u8) -> u8;
//! fn g(s: u8) -> u8;
//! #[rustc_intrinsic] fn intr(s: u8) -> u8;
//! fn main(p: fn(u8) -> u8, q: fn(u8) -> u16);
//! ```
//!
//! Planted bug: `Plant::LubEarlierOff` (the second direction is skipped): caught by
//! `control_without_the_second_direction_the_else_is_e0308`. Without rules/reify.rs's check,
//! `an_intrinsic_is_never_a_fn_pointer` fails: no finding, and the intrinsic is reified.

use std::string::String;
use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::Expr;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, FnPtrHeader, ItemKey, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};
use crate::world::TypeAttrs;

fn world() -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let u16_ = fx.ty(PNode::Uint(UintTy::U16));
    let unit = fx.unit();
    let io = fx.list(&[u8_, u8_]);
    let p_ty = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    let io16 = fx.list(&[u8_, u16_]);
    let q_ty = fx.ty(PNode::FnPtr(io16, FnPtrHeader::RUST));
    fx.func("f", 0, &[u8_], u8_, Vec::new());
    fx.func("g", 0, &[u8_], u8_, Vec::new());
    let intr = fx.func("intr", 0, &[u8_], u8_, Vec::new());
    fx.set_attrs(intr, TypeAttrs(TypeAttrs::INTRINSIC));
    let main = fx.func("main", 0, &[p_ty, q_ty], unit, Vec::new());
    fx.finish();
    (world, main)
}

/// A source of the site: a fn item by name, or the parameter `p` or `q`.
#[derive(Clone, Copy)]
enum Src {
    Item(&'static str),
    P,
    Q,
}

/// What one case gave.
struct Got {
    /// `(code, expression)` of each finding.
    diags: Vec<(DiagCode, u32)>,
    /// Each source's expression, in order.
    srcs: Vec<ExprId>,
    /// Each source's adjustments, in order.
    adj: Vec<String>,
    /// The type of the binding `x` the site's value is bound to.
    x: String,
}

/// The parameters' patterns, then the source's expression.
fn source(b: &mut Bb, p: PatId, q: PatId, s: Src) -> ExprId {
    match s {
        Src::Item(name) => b.path(&[name]),
        Src::P => b.local(p),
        Src::Q => b.local(q),
    }
}

fn finish(fx: &Fx, b: &Bb, main: ItemKey, root: ExprId, x: PatId, srcs: Vec<ExprId>, plant: Plant) -> Got {
    let c = check_body(fx, b, main, root, plant, InferPlant::None);
    Got {
        diags: c.diags.iter().map(|d| (d.code, d.at.start)).collect(),
        adj: srcs.iter().map(|&e| adjusts(fx, &c, e)).collect(),
        srcs,
        x: show(fx, c.types.view(), c.local_ty[x.0 as usize]),
    }
}

/// `fn main(p, q) { let x = if true { A } else { B }; }` (the arms are the sources themselves).
fn if_case(a: Src, b_: Src, plant: Plant) -> Got {
    let (fx, main) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let q = b.param("q");
    let cond = b.bool();
    let then = source(&mut b, p, q, a);
    let els = source(&mut b, p, q, b_);
    let iff = b.e(Expr::If { cond, then, else_: Some(els) });
    let x = b.bind("x");
    let s0 = b.let_(x, None, iff);
    let root = b.block(&[s0], None);
    finish(&fx, &b, main, root, x, std::vec![then, els], plant)
}

/// `fn main(p, q) { let x = match true { a0 => S0, a1 => S1, .. }; }`.
fn match_case(arms: &[Src], plant: Plant) -> Got {
    const NAMES: [&str; 4] = ["a0", "a1", "a2", "a3"];
    assert!(arms.len() <= NAMES.len(), "the case's arm names");
    let (fx, main) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let q = b.param("q");
    let scrutinee = b.bool();
    let mut srcs = Vec::new();
    let mut pairs = Vec::new();
    for (i, &s) in arms.iter().enumerate() {
        let e = source(&mut b, p, q, s);
        let pat = b.bind(NAMES[i]);
        srcs.push(e);
        pairs.push((pat, e));
    }
    let m = b.match_(scrutinee, &pairs);
    let x = b.bind("x");
    let s0 = b.let_(x, None, m);
    let root = b.block(&[s0], None);
    finish(&fx, &b, main, root, x, srcs, plant)
}

/// `fn(u8) -> u8` in the body's table.
fn fn_u8(b: &mut Bb) -> PTy {
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let io = b.t.intern_list(&[u8_, u8_]).expect("intern");
    b.ty(PNode::FnPtr(io, FnPtrHeader::RUST))
}

/// `fn main(p, q) { let x: fn(u8) -> u8 = NAME; }`: the single-site coercion.
fn let_case(name: &'static str) -> Got {
    let (fx, main) = world();
    let mut b = Bb::new();
    let _p = b.param("p");
    let _q = b.param("q");
    let e = b.path(&[name]);
    let x = b.bind("x");
    let t = fn_u8(&mut b);
    let s0 = b.let_(x, Some(t), e);
    let root = b.block(&[s0], None);
    finish(&fx, &b, main, root, x, std::vec![e], Plant::None)
}

const REIFY: &str = "ReifyFnPointer -> fn(u8) -> u8";

/// `if true { f } else { p }`: the `else` is the more general type; the `then` arm reifies to
/// it, and `x` is `fn(u8) -> u8`. Without the fix: one E0308 (IncompatibleArms) at `p`.
#[test]
fn a_later_fn_pointer_takes_the_earlier_fn_item() {
    let g = if_case(Src::Item("f"), Src::P, Plant::None);
    assert_eq!(g.diags, Vec::new());
    assert_eq!(g.adj, std::vec![String::from(REIFY), String::new()]);
    assert_eq!(g.x, "fn(u8) -> u8");
}

/// The control: with the second direction planted off, the case above is the E0308 at the
/// `else`, and nothing reifies.
#[test]
fn control_without_the_second_direction_the_else_is_e0308() {
    let g = if_case(Src::Item("f"), Src::P, Plant::LubEarlierOff);
    assert_eq!(g.diags, std::vec![(DiagCode::IncompatibleArms, g.srcs[1].0)]);
    assert_eq!(g.adj, std::vec![String::new(), String::new()]);
}

/// Three arms, both directions and the fn-item lub: `f, p, g` (`f` reified when `p` comes,
/// `g` coerced to the pointer), `f, g, p` (the two items meet at the pointer, `p` is equal),
/// `p, f, g` (each item coerced to the pointer as it comes). Each is `fn(u8) -> u8`, with every
/// fn item reified and the pointer untouched.
#[test]
fn three_arms_of_fn_items_and_a_pointer_meet_at_the_pointer() {
    let r = String::from(REIFY);
    let none = String::new();
    let cases: [([Src; 3], [&String; 3]); 3] = [
        ([Src::Item("f"), Src::P, Src::Item("g")], [&r, &none, &r]),
        ([Src::Item("f"), Src::Item("g"), Src::P], [&r, &r, &none]),
        ([Src::P, Src::Item("f"), Src::Item("g")], [&none, &r, &r]),
    ];
    for (arms, want) in cases {
        let g = match_case(&arms, Plant::None);
        assert_eq!(g.diags, Vec::new(), "arms {:?}", g.srcs);
        let want: Vec<String> = want.iter().map(|s| (*s).clone()).collect();
        assert_eq!(g.adj, want, "arms {:?}", g.srcs);
        assert_eq!(g.x, "fn(u8) -> u8", "arms {:?}", g.srcs);
    }
}

/// A control: a later pointer of another signature is no lub: `f` then `q: fn(u8) -> u16` stays
/// E0308 at `q`, and `f` is not reified.
#[test]
fn a_later_pointer_of_another_signature_is_still_e0308() {
    let g = match_case(&[Src::Item("f"), Src::Q], Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::IncompatibleArms, g.srcs[1].0)]);
    assert_eq!(g.adj, std::vec![String::new(), String::new()]);
}

/// An intrinsic is never a fn pointer, by the one rule on every path: beside a fn item (the
/// fn-item lub), before a pointer (the second direction), and at a single `let` site. Each is
/// one [`DiagCode::IntrinsicToFnPointer`] at the intrinsic's expression, no second error, and
/// the intrinsic is not reified. Without rules/reify.rs's check: no finding, and
/// `ReifyFnPointer` on the intrinsic.
#[test]
fn an_intrinsic_is_never_a_fn_pointer() {
    let g = match_case(&[Src::Item("f"), Src::Item("intr")], Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::IntrinsicToFnPointer, g.srcs[1].0)], "f, intr");
    assert_eq!(g.adj[1], "", "f, intr");
    let g = match_case(&[Src::Item("intr"), Src::Item("f")], Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::IntrinsicToFnPointer, g.srcs[0].0)], "intr, f");
    assert_eq!(g.adj[0], "", "intr, f");
    let g = match_case(&[Src::Item("intr"), Src::P], Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::IntrinsicToFnPointer, g.srcs[0].0)], "intr, p");
    assert_eq!(g.adj[0], "", "intr, p");
    let g = let_case("intr");
    assert_eq!(g.diags, std::vec![(DiagCode::IntrinsicToFnPointer, g.srcs[0].0)], "let intr");
    assert_eq!(g.adj[0], "", "let intr");
}

/// The control: the same three shapes and the `let` with `g`, a fn of the same signature that is
/// no intrinsic, have no finding and reify `g`.
#[test]
fn control_a_plain_fn_in_the_same_places_reifies() {
    let g = match_case(&[Src::Item("f"), Src::Item("g")], Plant::None);
    assert_eq!((g.diags.len(), g.adj[1].as_str()), (0, REIFY), "f, g");
    let g = match_case(&[Src::Item("g"), Src::Item("f")], Plant::None);
    assert_eq!((g.diags.len(), g.adj[0].as_str()), (0, REIFY), "g, f");
    let g = match_case(&[Src::Item("g"), Src::P], Plant::None);
    assert_eq!((g.diags.len(), g.adj[0].as_str()), (0, REIFY), "g, p");
    let g = let_case("g");
    assert_eq!((g.diags.len(), g.adj[0].as_str()), (0, REIFY), "let g");
    assert_eq!(g.x, "fn(u8) -> u8");
}
