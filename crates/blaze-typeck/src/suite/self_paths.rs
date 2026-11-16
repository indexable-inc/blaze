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
