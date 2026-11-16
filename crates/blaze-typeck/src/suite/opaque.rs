//! Return-position `impl Trait` seen by a caller, as the front end answers it (crates/blaze-cli/
//! tests/ladder/README.md, "Heads lane: types and bounds"): one key per occurrence; the
//! function's output is `Opaque(k, the generics in scope)`; `Predicates(k)` holds its bounds
//! with that opaque type as each clause's `Self`; the hidden type is not the caller's.
//!
//! The Reference, "Impl trait", "Abstract return types": "a caller of the function is not
//! permitted to know the concrete type"; "the caller can only use the methods declared by the
//! trait bound". So a caller proves `Tr` of the opaque type from its bounds, and normalizes
//! `<Opaque as Tr>::Out` through its bounds, at the call's own arguments for the generics.
//!
//! ```text
//! trait Tr { type Out; }
//! fn mk<T>(t: T) -> impl Tr<Out = T> { .. }
//! fn need<X: Tr>(x: X) -> <X as Tr>::Out { .. }
//! fn user() -> u8 { need(mk(1u8)) }     // clean
//! fn user16() -> u16 { need(mk(1u8)) }  // E0308: the bound says `Out = u8`
//! ```

use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &["Out"]);
    let p0 = fx.param(0);
    let o = fx.opaque_key("mk::{opaque#0}");
    let ps = fx.list(&[p0]);
    let opaque = fx.ty(PNode::Opaque(o, ps));
    let is_tr = fx.bound(tr, &[opaque]);
    let out_is_t = fx.proj(tr, &[opaque], "Out", p0);
    fx.set_opaque_bounds(o, std::vec![is_tr, out_is_t]);
    fx.func("mk", 1, &[p0], opaque, Vec::new());
    let out = fx.alias(tr, "Out", &[p0]);
    let x_tr = fx.bound(tr, &[p0]);
    fx.func("need", 1, &[p0], out, std::vec![x_tr]);
    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let user = fx.func("user", 0, &[], u8_, Vec::new());
    let user16 = fx.func("user16", 0, &[], u16_, Vec::new());
    fx.finish();
    (fx, user, user16)
}

/// `need(mk(1u8))`: expr 4 is the outer call; the root block is expr 5.
fn body() -> (Bb, ExprId) {
    let mut b = Bb::new();
    let need = b.path(&["need"]);
    let mk = b.path(&["mk"]);
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let inner = b.call(mk, &[one]);
    let outer = b.call(need, &[inner]);
    let root = b.block(&[], Some(outer));
    (b, root)
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// The caller proves `impl Tr<Out = T>: Tr` and normalizes `Out` from the bounds at `T = u8`.
/// Without the generics substituted (`Out` left `T`, a parameter of `mk`), the call would not
/// be `u8`.
#[test]
fn a_caller_uses_an_opaque_types_bounds_at_its_arguments() {
    let (fx, user, _) = world();
    let (b, root) = body();
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(3), "impl mk::{opaque#0}"), (ExprId(4), "u8"), (ExprId(5), "u8")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The bound fixes `Out = u8` at the call: a `u16` return is E0308, so the caller does know
/// the bound's term and no more.
#[test]
fn an_opaque_types_projection_is_its_bound_and_nothing_else() {
    let (fx, _, user16) = world();
    let (b, root) = body();
    let c = check_body(&fx, &b, user16, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(4), "u8")], pats: &[], diags: &[(DiagCode::Mismatch, ExprId(4))] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}
