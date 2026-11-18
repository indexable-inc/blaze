//! An enum variant named through an associated type: `Self::Error::Overflow` (ruint's
//! `impl TryFrom<Uint<BITS, LIMBS>> for u8 { type Error = FromUintError<u8>; .. Err(
//! Self::Error::Overflow(..)) }`, thirteen fns of the tempo build census of 2026-11-18, E0599
//! "found `FromUintError<u8>`, about `Overflow`"). RFC 2338 (`type_alias_enum_variants`, stable
//! since 1.37) lets any type-relative path that names an enum's type name its variants, and
//! rustc_hir_typeck `resolve_fully_qualified_call` looks for a variant of the enum before it
//! probes for an associated item. The checker's one home for `T::name` as a value
//! (rules/lit_path.rs `assoc_value`) now does the same for every prefix: an associated type,
//! a qualified `<E>::V`, an alias.
//!
//! ```text
//! enum E { Overflow, Other }   struct S;
//! trait HasErr { type Error; fn make() -> Self::Error; }
//! impl HasErr for S { type Error = E; fn make() -> E { Self::Error::Overflow } }
//! ```

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, List};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};
use crate::world::Def;

/// The world of the module doc: `make` of `impl HasErr for S`, and `E`'s type.
fn world() -> (Fx, ItemKey, PTy) {
    let mut fx = Fx::new();
    let (e, _) = fx.enum_("E", 0, &[("Overflow", &[]), ("Other", &[])]);
    let e_t = fx.adt(e, &[]);
    let s = fx.strukt("S", 0, &[]);
    let s_t = fx.adt(s, &[]);
    fx.use_path("Self", Def::Adt(s));
    let has_err = fx.tr("HasErr", 1, Vec::new(), &["Error"]);
    let p0 = fx.param(0);
    let error = fx.alias(has_err, "Error", &[p0]);
    fx.trait_fn(has_err, "make", false, &[], error);
    let imp = fx.imp("impl HasErr for S", 0, s_t, Some((has_err, &[])), &[("Error", e_t)], Vec::new());
    let make = fx.impl_fn(imp, "make", false, &[], e_t);
    fx.finish();
    (fx, make, e_t)
}

/// `Self::Error::<variant>` as `make`'s body: the path is expression 0, the block 1.
fn through_assoc(variant: &str) -> (Fx, Bb, ItemKey, ExprId) {
    let (fx, make, _) = world();
    let mut b = Bb::new();
    let p = b.path(&["Self", "Error", variant]);
    let root = b.block(&[], Some(p));
    (fx, b, make, root)
}

/// `Self::Error::Overflow` is `E::Overflow`: both nodes are `E`, nothing reported.
#[test]
fn a_variant_through_an_associated_type_is_the_variants_ctor() {
    let (fx, b, make, root) = through_assoc("Overflow");
    let c = check_body(&fx, &b, make, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &Want { exprs: &[(ExprId(0), "E"), (ExprId(1), "E")], pats: &[], diags: &[] });
}

/// The negative beside it: a name that is no variant of `E` and no associated item of it is
/// still E0599 at the path, as in rustc ("no variant or associated item named `Missing`").
#[test]
fn a_name_that_is_no_variant_through_an_associated_type_is_e0599() {
    let (fx, b, make, root) = through_assoc("Missing");
    let c = check_body(&fx, &b, make, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, std::vec![DiagCode::NoMethod], "{:?}", c.diags);
}

/// The same rule through a qualified path `<E>::Other` (the Reference, "Qualified paths": `<T>::x`
/// is `x` looked up on `T` as `T::x` is): `E`.
#[test]
fn a_variant_through_a_qualified_path_is_the_variants_ctor() {
    let (fx, make, _) = world();
    let mut b = Bb::new();
    // The anchor's type is the body's own (its table), never the world's `PTy`: a world type
    // in the body's table is a node whose children are not interned before it (TypeOrder).
    let e_t = b.ty(PNode::Adt(fx.key_of("E"), List::EMPTY));
    let p = b.qpath(e_t, &["Other"]);
    let root = b.block(&[], Some(p));
    let c = check_body(&fx, &b, make, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &Want { exprs: &[(ExprId(0), "E"), (ExprId(1), "E")], pats: &[], diags: &[] });
}
