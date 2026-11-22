//! Three rules around a fn item becoming a fn pointer, and a coercion site's least upper bound
//! (fn-lub-3): rules/target_feature.rs, rules/coerce_lub.rs `lub_earlier_to_new`, rules/cast.rs.
//!
//! 1. A safe `#[target_feature]` fn is never a SAFE fn pointer outside a context that enables
//!    its features (the Reference, "Code generation attributes", "The `target_feature`
//!    attribute": "Safe `#[target_feature]` functions can only be coerced to safe function
//!    pointers in contexts that enable all the target features that the coercee enables. This
//!    restriction does not apply to `unsafe` function pointers."; the pinned library's
//!    stdarch/crates/core_arch/src/arm_shared/neon/generated.rs line 2041, `pub fn vaddq_u8`,
//!    is such a safe fn). At a single site and in a lub it is
//!    [`DiagCode::TargetFeatureToSafeFnPointer`] (E0308) at the item; to an `unsafe fn`
//!    pointer it reifies; two of them meet at `unsafe fn(u8) -> u8`. In a body with its own
//!    `#[target_feature]` the feature sets decide, which the world does not answer yet:
//!    [`DiagCode::TargetFeatureContextUnread`], by name.
//! 2. The Reference, "Least upper bound coercions", and rustc's `try_find_coercion_lub` ("there
//!    are no coercions applied to *any* of the previous expressions, other than noop
//!    reborrows"): in `match c { 0 => a, 1 => b, _ => d }` with `a, b: &mut [u8; 4]` and
//!    `d: &mut [u8]`, `b` was reborrowed (`&mut *b`) to the site's `&mut [u8; 4]`; that
//!    reborrow of the same mutability is no adjustment of its own, so `a` and `b` both coerce
//!    to `&mut [u8]`, and `b`'s reborrow is REPLACED (a reborrow has no effect before a
//!    dereference), never stacked: both read `* -> [u8; 4], &mut -> &mut [u8; 4], unsize ->
//!    &mut [u8]`.
//! 3. A cast of an intrinsic is E0606 ([`DiagCode::InvalidCast`]): it has no address
//!    (core/src/intrinsics/mod.rs lines 7 and 13, "The actual implementations live in the
//!    compiler.", "Intrinsics don't need a body."), whatever the target type, a fn pointer
//!    included (there the coercion's E0308 is not the cast's error). A fn pointer type is no
//!    cast target past a coercion (E0605): `tf as fn(u8) -> u8` outside its features.
//!
//! ```text
//! fn f(s: u8) -> u8;
//! #[target_feature(enable = "neon")] fn tf(s: u8) -> u8;
//! #[target_feature(enable = "neon")] fn tf2(s: u8) -> u8;
//! #[rustc_intrinsic] fn intr(s: u8) -> u8;
//! fn main(a: &mut [u8; 4], b: &mut [u8; 4], d: &mut [u8]);
//! #[target_feature(enable = "neon")] fn feat(a: &mut [u8; 4], b: &mut [u8; 4], d: &mut [u8]);
//! ```
//!
//! Planted bug: `Plant::LubReborrowOff` (a noop reborrow counts as an adjustment): caught by
//! `control_without_the_reborrow_rule_the_last_arm_is_e0308`.

use std::string::String;
use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::Expr;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, FnPtrHeader, ItemKey, Mutbl, PatId, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};
use crate::world::TypeAttrs;

/// The two owners: `main` (no features) and `feat` (`#[target_feature]` of its own).
struct World {
    fx: Fx,
    main: ItemKey,
    feat: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let usize_ = fx.ty(PNode::Uint(UintTy::Usize));
    let unit = fx.unit();
    let four = fx.ty(PNode::ConstValue { ty: usize_, bits: 4 });
    let arr = fx.ty(PNode::Array(u8_, four));
    let slice = fx.ty(PNode::Slice(u8_));
    let r_arr = fx.r(Mutbl::Mut, arr);
    let r_slice = fx.r(Mutbl::Mut, slice);
    fx.func("f", 0, &[u8_], u8_, Vec::new());
    let tf = fx.func("tf", 0, &[u8_], u8_, Vec::new());
    fx.set_attrs(tf, TypeAttrs(TypeAttrs::TARGET_FEATURE));
    let tf2 = fx.func("tf2", 0, &[u8_], u8_, Vec::new());
    fx.set_attrs(tf2, TypeAttrs(TypeAttrs::TARGET_FEATURE));
    let intr = fx.func("intr", 0, &[u8_], u8_, Vec::new());
    fx.set_attrs(intr, TypeAttrs(TypeAttrs::INTRINSIC));
    let main = fx.func("main", 0, &[r_arr, r_arr, r_slice], unit, Vec::new());
    let feat = fx.func("feat", 0, &[r_arr, r_arr, r_slice], unit, Vec::new());
    fx.set_attrs(feat, TypeAttrs(TypeAttrs::TARGET_FEATURE));
    fx.finish();
    World { fx, main, feat }
}

/// A source: a fn item by name, or the parameter `a`, `b` or `d`.
#[derive(Clone, Copy)]
enum Src {
    Item(&'static str),
    A,
    B,
    D,
}

/// What one case gave.
struct Got {
    /// `(code, expression)` of each finding.
    diags: Vec<(DiagCode, u32)>,
    /// The expressions the case names, in order.
    srcs: Vec<ExprId>,
    /// Each of those expressions' adjustments.
    adj: Vec<String>,
    /// The type of the binding `x`.
    x: String,
}

/// The body's parameters `a`, `b`, `d`, in the signature's order.
struct Params {
    a: PatId,
    b: PatId,
    d: PatId,
}

fn params(bb: &mut Bb) -> Params {
    Params { a: bb.param("a"), b: bb.param("b"), d: bb.param("d") }
}

fn source(bb: &mut Bb, p: &Params, s: Src) -> ExprId {
    match s {
        Src::Item(name) => bb.path(&[name]),
        Src::A => bb.local(p.a),
        Src::B => bb.local(p.b),
        Src::D => bb.local(p.d),
    }
}

/// `[unsafe] fn(u8) -> u8` in the body's table.
fn fn_u8(bb: &mut Bb, safety: Safety) -> PTy {
    let u8_ = bb.ty(PNode::Uint(UintTy::U8));
    let io = bb.t.intern_list(&[u8_, u8_]).expect("intern");
    bb.ty(PNode::FnPtr(io, FnPtrHeader { safety, ..FnPtrHeader::RUST }))
}

fn finish(w: &World, bb: &Bb, owner: ItemKey, root: ExprId, x: PatId, srcs: Vec<ExprId>, plant: Plant) -> Got {
    let c = check_body(&w.fx, bb, owner, root, plant, InferPlant::None);
    Got {
        diags: c.diags.iter().map(|d| (d.code, d.at.start)).collect(),
        adj: srcs.iter().map(|&e| adjusts(&w.fx, &c, e)).collect(),
        srcs,
        x: show(&w.fx, c.types.view(), c.local_ty[x.0 as usize]),
    }
}

/// `fn OWNER(a, b, d) { let x = match true { a0 => S0, a1 => S1, .. }; }`.
fn match_case(owner: fn(&World) -> ItemKey, arms: &[Src], plant: Plant) -> Got {
    const NAMES: [&str; 3] = ["a0", "a1", "a2"];
    assert!(arms.len() <= NAMES.len(), "the case's arm names");
    let w = world();
    let mut bb = Bb::new();
    let p = params(&mut bb);
    let scrutinee = bb.bool();
    let mut srcs = Vec::new();
    let mut pairs = Vec::new();
    for (i, &s) in arms.iter().enumerate() {
        let e = source(&mut bb, &p, s);
        let pat = bb.bind(NAMES[i]);
        srcs.push(e);
        pairs.push((pat, e));
    }
    let m = bb.match_(scrutinee, &pairs);
    let x = bb.bind("x");
    let s0 = bb.let_(x, None, m);
    let root = bb.block(&[s0], None);
    finish(&w, &bb, owner(&w), root, x, srcs, plant)
}

/// `fn OWNER(a, b, d) { let x: [unsafe] fn(u8) -> u8 = NAME; }`: the single-site coercion.
fn let_case(owner: fn(&World) -> ItemKey, name: &'static str, safety: Safety) -> Got {
    let w = world();
    let mut bb = Bb::new();
    let _p = params(&mut bb);
    let e = bb.path(&[name]);
    let x = bb.bind("x");
    let t = fn_u8(&mut bb, safety);
    let s0 = bb.let_(x, Some(t), e);
    let root = bb.block(&[s0], None);
    finish(&w, &bb, owner(&w), root, x, std::vec![e], Plant::None)
}

/// The cast's target.
#[derive(Clone, Copy)]
enum To {
    Usize,
    Fn(Safety),
}

/// `fn main(a, b, d) { let x = NAME as TO; }`; the case's one expression is the cast.
fn cast_case(name: &'static str, to: To) -> Got {
    let w = world();
    let mut bb = Bb::new();
    let _p = params(&mut bb);
    let operand = bb.path(&[name]);
    let ty = match to {
        To::Usize => bb.ty(PNode::Uint(UintTy::Usize)),
        To::Fn(s) => fn_u8(&mut bb, s),
    };
    let cast = bb.e(Expr::Cast { operand, ty });
    let x = bb.bind("x");
    let s0 = bb.let_(x, None, cast);
    let root = bb.block(&[s0], None);
    finish(&w, &bb, w.main, root, x, std::vec![cast], Plant::None)
}

fn main_of(w: &World) -> ItemKey {
    w.main
}

fn feat_of(w: &World) -> ItemKey {
    w.feat
}

/// Case 1: `let x: fn(u8) -> u8 = tf;`, `match .. { tf, f }` and `{ f, tf }` in `main` are each
/// one [`DiagCode::TargetFeatureToSafeFnPointer`] at `tf`, and `tf` is not reified; in `feat`
/// (features of its own, not read) the `let` is [`DiagCode::TargetFeatureContextUnread`].
/// Without the gate: no finding, and `ReifyFnPointer -> fn(u8) -> u8` on `tf` (a safe pointer
/// to a fn that needs NEON, callable from safe code).
#[test]
fn a_target_feature_fn_is_never_a_safe_fn_pointer() {
    let g = let_case(main_of, "tf", Safety::Safe);
    assert_eq!(g.diags, std::vec![(DiagCode::TargetFeatureToSafeFnPointer, g.srcs[0].0)], "let tf");
    assert_eq!(g.adj[0], "", "let tf");
    let g = match_case(main_of, &[Src::Item("tf"), Src::Item("f")], Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::TargetFeatureToSafeFnPointer, g.srcs[0].0)], "tf, f");
    let g = match_case(main_of, &[Src::Item("f"), Src::Item("tf")], Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::TargetFeatureToSafeFnPointer, g.srcs[1].0)], "f, tf");
    let g = let_case(feat_of, "tf", Safety::Safe);
    assert_eq!(g.diags, std::vec![(DiagCode::TargetFeatureContextUnread, g.srcs[0].0)], "feat: let tf");
}

/// Case 1's control: `tf` to an `unsafe fn` pointer reifies with no finding; `tf` and `tf2`
/// meet at `unsafe fn(u8) -> u8`; a plain `f` to a safe pointer reifies as before.
#[test]
fn control_a_target_feature_fn_is_an_unsafe_fn_pointer() {
    let g = let_case(main_of, "tf", Safety::Unsafe);
    assert_eq!((g.diags.len(), g.adj[0].as_str()), (0, "ReifyFnPointer -> unsafe fn(u8) -> u8"), "let tf: unsafe");
    let g = match_case(main_of, &[Src::Item("tf"), Src::Item("tf2")], Plant::None);
    assert_eq!(g.diags, Vec::new(), "tf, tf2");
    assert_eq!(g.x, "unsafe fn(u8) -> u8", "tf, tf2");
    let g = let_case(main_of, "f", Safety::Safe);
    assert_eq!((g.diags.len(), g.adj[0].as_str()), (0, "ReifyFnPointer -> fn(u8) -> u8"), "let f");
}

/// Case 2: `match .. { a, b, d }`: `b`'s reborrow is replaced, `a` and `b` read the same three
/// adjustments, `x` is `&mut [u8]`. Without the rule: E0308 (IncompatibleArms) at `d` (the
/// control below); a stacked reborrow would give `b` five adjustments.
#[test]
fn a_noop_reborrow_in_an_earlier_arm_is_replaced() {
    let g = match_case(main_of, &[Src::A, Src::B, Src::D], Plant::None);
    assert_eq!(g.diags, Vec::new());
    assert!(g.adj[0].ends_with("unsize -> &mut [u8]"), "a: `{}`", g.adj[0]);
    assert_eq!(g.adj[0].matches(" -> ").count(), 3, "a: `{}`", g.adj[0]);
    assert_eq!(g.adj[1], g.adj[0], "b is replaced, not stacked");
    assert_eq!(g.adj[2], "", "d");
    assert_eq!(g.x, "&mut [u8]");
}

/// Case 2's control: with the noop reborrow counted as an adjustment (the planted bug), the
/// same match is the E0308 at `d`.
#[test]
fn control_without_the_reborrow_rule_the_last_arm_is_e0308() {
    let g = match_case(main_of, &[Src::A, Src::B, Src::D], Plant::LubReborrowOff);
    assert_eq!(g.diags, std::vec![(DiagCode::IncompatibleArms, g.srcs[2].0)]);
}

/// Case 3: `intr as usize` and `intr as fn(u8) -> u8` are each one E0606 at the cast (not the
/// coercion's E0308, and not accepted as an address); `tf as fn(u8) -> u8` in `main` is E0605.
/// Without the cast rule: `intr as usize` has no finding, and `intr as fn(u8) -> u8` is
/// [`DiagCode::IntrinsicToFnPointer`].
#[test]
fn a_cast_of_an_intrinsic_is_e0606() {
    let g = cast_case("intr", To::Usize);
    assert_eq!(g.diags, std::vec![(DiagCode::InvalidCast, g.srcs[0].0)], "intr as usize");
    let g = cast_case("intr", To::Fn(Safety::Safe));
    assert_eq!(g.diags, std::vec![(DiagCode::InvalidCast, g.srcs[0].0)], "intr as fn");
    let g = cast_case("tf", To::Fn(Safety::Safe));
    assert_eq!(g.diags, std::vec![(DiagCode::NonPrimitiveCast, g.srcs[0].0)], "tf as fn");
}

/// Case 3's control: `f as usize`, `f as fn(u8) -> u8`, `tf as usize` and
/// `tf as unsafe fn(u8) -> u8` are all accepted.
#[test]
fn control_casts_of_plain_fns_and_to_unsafe_pointers_pass() {
    for (name, to, what) in [
        ("f", To::Usize, "f as usize"),
        ("f", To::Fn(Safety::Safe), "f as fn"),
        ("tf", To::Usize, "tf as usize"),
        ("tf", To::Fn(Safety::Unsafe), "tf as unsafe fn"),
    ] {
        let g = cast_case(name, to);
        assert_eq!(g.diags, Vec::new(), "{what}");
    }
}
