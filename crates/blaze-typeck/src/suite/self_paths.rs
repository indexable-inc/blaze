//! Type-relative paths through `Self`, as the front end answers them: `Self` in an impl of an
//! ADT resolves to `Def::Adt` consuming one segment (blaze-ladder front2/world_paths.rs), and
//! the next segment is the checker's to find. tempo's "611 diag E0599" (alloy's `sol!` writes
//! `Self::Variant(..)` and `Self::abi_decode_raw_with_config(..)` in `impl SolInterface for
//! XCalls`), reduced; the real-front-end form is blaze-ladder tests/front2_selfpaths.rs.
//!
//! ```text
//! enum E { A(u8), B }
//! impl E { fn b2() -> E { Self::A(3) } }            // the variant, not an associated item
//! trait Tb { fn g() -> u8; }                         // NOT imported where the impl is
//! impl Tb for E { fn g() -> u8 { 1 } fn b4() -> u8 { Self::g() } }
//! ```
//!
//! rustc's `associated_path_to_ty` looks for a variant of the enum before any associated item;
//! rustc_resolve counts the trait of the impl a body is in among the traits in scope
//! (`traits_in_scope` adds `current_trait_ref`), so `Self::g()` needs no `use`.

use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::world::Def;

fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let (e, _) = fx.enum_("E", 0, &[("A", &[u8_]), ("B", &[])]);
    let e_t = fx.adt(e, &[]);
    fx.use_path("Self", Def::Adt(e));
    let inherent = fx.imp("impl E", 0, e_t, None, &[], Vec::new());
    let b2 = fx.impl_fn(inherent, "b2", false, &[], e_t);
    let tb = fx.tr("Tb", 1, Vec::new(), &[]);
    fx.trait_fn(tb, "g", false, &[], u8_);
    let tb_e = fx.imp("impl Tb for E", 0, e_t, Some((tb, &[])), &[], Vec::new());
    fx.impl_fn(tb_e, "g", false, &[], u8_);
    let b4 = fx.impl_fn(tb_e, "b4", false, &[], u8_);
    fx.finish();
    (fx, b2, b4)
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// `Self::A(3)` is the variant's ctor: without the variant step it is E0599 (`A` is no
/// associated item of `E`).
#[test]
fn a_variant_through_self_is_the_variants_ctor() {
    let (fx, b2, _) = world();
    let mut b = Bb::new();
    let ctor = b.path(&["Self", "A"]);
    let three = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = b.call(ctor, &[three]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, b2, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(2), "E"), (ExprId(3), "E")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `Self::g()` in `impl Tb for E` with `Tb` not in scope: the impl's own trait is a candidate.
/// Without it: E0599.
#[test]
fn self_reaches_the_impls_own_trait_without_an_import() {
    let (fx, _, b4) = world();
    let mut b = Bb::new();
    let g = b.path(&["Self", "g"]);
    let call = b.call(g, &[]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, b4, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(1), "u8"), (ExprId(2), "u8")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The world of `Q::NAME` on a type parameter (the real-front-end form is blaze-ladder
/// tests/front2_params.rs):
///
/// ```text
/// trait A1 { const N: u8; }   trait A2 { const N: u8; }
/// trait Zst { const Z: bool; }   impl<T> Zst for T {}      // core's SizedTypeProperties
/// trait Zb { const Z: bool; }
/// fn one<Q: A1>() -> u8 { Q::N }          // A2 is in scope and does not hold: A1
/// fn both<Q: A1 + A2>() -> u8 { Q::N }    // E0034
/// fn first<Q: Zb>() -> bool { Q::Z }      // the bound's, before the blanket impl's
/// fn blanket<Q>() -> bool { Q::Z }        // the blanket impl's (`T::IS_ZST`)
/// ```
///
/// The Reference, "Method call expressions": "If `T` is a type parameter, methods provided by
/// trait bounds on `T` are looked up first. Then all remaining methods in scope are looked
/// up." and "If this results in multiple possible candidates, then it is an error". For a
/// path the Reference is silent; rustc keeps the same order (rustc_hir_typeck `probe_op` in
/// `Mode::Path`: `assemble_inherent_candidates_from_param` first).
fn param_world() -> (Fx, [ItemKey; 4]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let bool_ = fx.ty(crate::portable::PNode::Bool);
    let q = fx.param(0);
    fx.use_path("Q", Def::TyParam(0));
    let a1 = fx.tr("A1", 1, Vec::new(), &[]);
    fx.trait_const(a1, "N", u8_);
    let a2 = fx.tr("A2", 1, Vec::new(), &[]);
    fx.trait_const(a2, "N", u8_);
    let zst = fx.tr("Zst", 1, Vec::new(), &[]);
    fx.trait_const(zst, "Z", bool_);
    fx.imp("impl<T> Zst for T", 1, q, Some((zst, &[])), &[], Vec::new());
    let zb = fx.tr("Zb", 1, Vec::new(), &[]);
    fx.trait_const(zb, "Z", bool_);
    let c_a1 = fx.bound(a1, &[q]);
    let one = fx.func("one", 1, &[], u8_, std::vec![c_a1]);
    let (c_a1, c_a2) = (fx.bound(a1, &[q]), fx.bound(a2, &[q]));
    let both = fx.func("both", 1, &[], u8_, std::vec![c_a1, c_a2]);
    let c_zb = fx.bound(zb, &[q]);
    let first = fx.func("first", 1, &[], bool_, std::vec![c_zb]);
    let blanket = fx.func("blanket", 1, &[], bool_, Vec::new());
    fx.finish();
    (fx, [one, both, first, blanket])
}

/// The body `{ Q::name }` of `owner`, checked: its differences from `want`.
fn param_path(fx: &Fx, owner: ItemKey, name: &str, want: &Want<'_>) -> Vec<String> {
    let mut b = Bb::new();
    let p = b.path(&["Q", name]);
    let root = b.block(&[], Some(p));
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    written(differences(fx, &b, &c, want))
}

/// A constant through a bound where another trait in scope has the name and does not hold.
#[test]
fn a_parameters_constant_comes_from_its_bound() {
    let (fx, [one, _, _, _]) = param_world();
    let d = param_path(&fx, one, "N", &Want { exprs: &[(ExprId(0), "u8")], pats: &[], diags: &[] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// A constant through a blanket impl of a trait in scope where no bound has the name (core's
/// `T::IS_ZST`). OPEN (typeck.md item (14)): today "want bool, got {error}; diagnostics: want
/// [], got [(NoMethod, 0)]": `may_hold(Zst for Q)` answers no although the blanket impl is a
/// candidate of the fixture's `impls_of`; the cause is not found yet.
#[test]
#[ignore = "open: `Q: Zst` through `impl<T> Zst for T` is answered no (typeck.md item (14))"]
fn a_parameters_constant_comes_from_a_blanket_impl() {
    let (fx, [_, _, _, blanket]) = param_world();
    let d = param_path(&fx, blanket, "Z", &Want { exprs: &[(ExprId(0), "bool")], pats: &[], diags: &[] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `Q: Zb` with the blanket `Zst` in scope, both with `Z`: the bound's is picked. Without the
/// two tiers both are candidates and the path is E0034, which rustc does not give.
#[test]
fn a_parameters_bound_comes_before_the_traits_in_scope() {
    let (fx, [_, _, first, _]) = param_world();
    let d = param_path(&fx, first, "Z", &Want { exprs: &[(ExprId(0), "bool")], pats: &[], diags: &[] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// Two bounds with the name: E0034.
#[test]
fn two_bounds_with_one_name_are_ambiguous() {
    let (fx, [_, both, _, _]) = param_world();
    let d = param_path(&fx, both, "N", &Want { exprs: &[], pats: &[], diags: &[(DiagCode::AmbiguousMethod, ExprId(0))] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}
