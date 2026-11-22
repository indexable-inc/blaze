//! A reference to an associated type's projection unsizes to an object of its declared bound.
//! tempo's `E0308 found &Alias { .. }, expected &Dynamic(..)` (a derived `Debug` of a struct
//! whose field names an associated type), reduced:
//!
//! ```text
//! trait Debug { fn fmt(&self) -> u8; }
//! trait Digestible { type Digest: Debug; }
//! struct Digested<T: Digestible> { digest: T::Digest }
//! fn take(v: &dyn Debug) -> u8;
//! fn f<T: Digestible>(x: &Digested<T>) -> u8 { take(&x.digest) }
//! ```
//!
//! The rules relied on, from the Reference:
//!
//! - "Type coercions", "Coercion types": "`&T` to `&U` .. where `U` can be obtained from `T`
//!   by unsized coercion"; "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U +
//!   Sized`, and `U` is dyn compatible".
//! - "Associated Items", "Associated Types": "The optional trait bounds must be fulfilled by
//!   the implementations of the type alias. There is an implicit `Sized` bound on associated
//!   types": for any `T: Digestible`, `<T as Digestible>::Digest` is `Debug` and `Sized`.
//!
//! The cause as found (2026-11-16): the field's type is lowered as a variable bound by its
//! `Projection` obligation (`Machine::lower`, `Lower::Args`); `coerce_unsized` refused a
//! variable pointee before that obligation was selected, and the reborrow arm's autoderef then
//! resolved it to the rigid alias, so `&<T as Digestible>::Digest` to `&dyn Debug` was a
//! mismatch. The pointee is now resolved first when the target is an object
//! (rules/coerce.rs `coerce_unsized`).

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::Expr;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{LangItem, List, Mutbl, Name};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The program above; `with_bounds: false` leaves `Digest`'s bounds unsaid (the control).
fn world(with_bounds: bool) -> Fx {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let p0 = fx.param(0);
    let u8_ = fx.u8();
    let debug = fx.tr("Debug", 1, Vec::new(), &[]);
    let r_self = fx.r(Mutbl::Not, p0);
    fx.trait_fn(debug, "fmt", true, &[r_self], u8_);
    let digestible = fx.tr("Digestible", 1, Vec::new(), &["Digest"]);
    let digest = fx.alias(digestible, "Digest", &[p0]);
    if with_bounds {
        let is_debug = fx.bound(debug, &[digest]);
        let is_sized = fx.bound(sized, &[digest]);
        fx.set_assoc_bounds(digestible, "Digest", std::vec![is_debug, is_sized]);
    }
    let digested = fx.strukt("Digested", 1, &[("digest", digest)]);
    let ex_debug = fx.ty(PNode::ExTrait(debug, List::EMPTY));
    let dyn_debug = fx.t.dynamic(&[ex_debug]).expect("dyn");
    let r_dyn = fx.r(Mutbl::Not, dyn_debug);
    fx.func("take", 0, &[r_dyn], u8_, Vec::new());
    let digested_t = fx.adt(digested, &[p0]);
    let r_digested = fx.r(Mutbl::Not, digested_t);
    let is_digestible = fx.bound(digestible, &[p0]);
    fx.func("f", 1, &[r_digested], u8_, std::vec![is_digestible]);
    fx.finish();
    fx
}

/// `take(&x.digest)` in `f`: the check, and the borrow's expression.
fn check(fx: &Fx) -> (crate::columns::Checked<'static>, crate::ids::ExprId) {
    let mut b = Bb::new();
    let x = b.param("x");
    let base = b.local(x);
    let field = b.e(Expr::Field { base, name: Name::of("digest") });
    let arg = b.borrow(Mutbl::Not, field);
    let take = b.path(&["take"]);
    let call = b.call(take, &[arg]);
    let root = b.block(&[], Some(call));
    (check_body(fx, &b, fx.key_of("f"), root, Plant::None, InferPlant::None), arg)
}

/// The derive's body checks: no diagnostic, and the argument's last adjustment is the unsize
/// to `&dyn Debug`. Before the fix this was E0308 (`&<P0 as Digestible>::Digest` against
/// `&dyn Debug`).
///
/// Prints: `test suite::projection_unsize::a_projection_field_unsizes_to_its_bound_object ... ok`
#[test]
fn a_projection_field_unsizes_to_its_bound_object() {
    let fx = world(true);
    let (c, arg) = check(&fx);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> &dyn Debug"), "adjustments `{got}`");
}

/// The control (rule 23): with `Digest`'s bounds unsaid, nothing proves `<T as
/// Digestible>::Digest: Debug + Sized`, and the same body is refused with E0308. A fix that took
/// the coercion without asking `Unsize` would pass here and fail.
///
/// Prints: `test suite::projection_unsize::control_without_the_bound_the_projection_is_refused ... ok`
#[test]
fn control_without_the_bound_the_projection_is_refused() {
    let fx = world(false);
    let (c, _) = check(&fx);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::Mismatch), "no E0308 without the bound: {:?}", c.diags);
}
