//! A bound written on a supertrait's associated type in a trait's header is implied by the
//! trait (the reference's `explicit_implied_clauses_of`, `PredicateFilter::
//! SelfAndAssociatedTypeBounds`, rustc_hir_analysis collect/predicates_of.rs; RFC 2289):
//! `trait Pcp: Curve<Fbs: Modulus>` gives `C: Pcp` both `C: Curve` and
//! `<C as Curve>::Fbs: Modulus`. Tempo's build census (2026-11-18, 24 bodies): primeorder 0.14
//! `PrimeCurveParams: Curve<FieldBytesSize: sec1::ModulusSize> + ..` and ecdsa 0.17
//! `EcdsaCurve: Curve<FieldBytesSize: Add<Output: ArraySize<..>>> + ..`, whose impls write only
//! `where C: PrimeCurveParams`.
//!
//! ```text
//! trait Curve { type Fbs; }
//! trait Modulus {}
//! trait Pcp: Curve<Fbs: Modulus> {}            // implied: Self: Curve, <Self as Curve>::Fbs: Modulus
//! fn need<S: Modulus>(s: S) -> u8;
//! fn f<C: Pcp>(x: <C as Curve>::Fbs) -> u8 { need(x) }   // clean in rustc
//! ```
//!
//! The cause as found (2026-11-18, by reading): blaze-ladder front2/heads.rs `trait_members`
//! kept as the trait's `supertraits` only the clauses whose first argument is `Self`, so the
//! constraint's clause (first argument `<Self as Curve>::Fbs`) was dropped and `f`'s env had
//! no `Modulus` for the alias: E0277 at `need(x)`. The front end now hands over the implied
//! clauses (front2/preds.rs `predicates_implying`). This miniature is the checker's half: the
//! trait's `supertraits` given as the front end now gives them, the env takes them.
//!
//! The control (rule 23): the same trait with `Self: Curve` alone, which is also what
//! `trait Pcp: Curve where <Self as Curve>::Fbs: Modulus` implies (a where predicate on another
//! type than `Self` is not implied: the Reference, "Traits", "Supertraits";
//! rust-lang/rust#20671): E0277 at the call.

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::ItemKey;
use crate::infer::InferPlant;
use crate::machine::Plant;

/// The world with `Pcp`'s implied clauses: the constraint's clause too (`implied`), or
/// `Self: Curve` alone. Returns `f`.
fn world(implied: bool) -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let p0 = fx.param(0);
    let curve = fx.tr("Curve", 1, Vec::new(), &["Fbs"]);
    let modulus = fx.tr("Modulus", 1, Vec::new(), &[]);
    let self_fbs = fx.alias(curve, "Fbs", &[p0]);
    let mut supers = std::vec![fx.bound(curve, &[p0])];
    if implied {
        supers.push(fx.bound(modulus, &[self_fbs]));
    }
    let pcp = fx.tr("Pcp", 1, supers, &[]);
    let u8_ = fx.u8();
    let s_mod = fx.bound(modulus, &[p0]);
    fx.func("need", 1, &[p0], u8_, std::vec![s_mod]);
    let c_pcp = fx.bound(pcp, &[p0]);
    let c_fbs = fx.alias(curve, "Fbs", &[p0]);
    let f = fx.func("f", 1, &[c_fbs], u8_, std::vec![c_pcp]);
    fx.finish();
    (fx, f)
}

/// `{ need(x) }` as `f`'s body, checked: the codes. Expressions: the path 0, `x` 1, the call
/// 2, the block 3.
fn codes(implied: bool) -> Vec<DiagCode> {
    let (fx, f) = world(implied);
    let mut b = Bb::new();
    let x = b.param("x");
    let need = b.path(&["need"]);
    let xe = b.local(x);
    let call = b.call(need, &[xe]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// The header's constraint is implied: `need(x)` is clean.
///
/// Prints: `test suite::env_supertrait_assoc_bound::a_header_constraint_bound_is_in_the_env ... ok`
#[test]
fn a_header_constraint_bound_is_in_the_env() {
    assert_eq!(codes(true), Vec::<DiagCode>::new(), "`C: Pcp` implies `<C as Curve>::Fbs: Modulus`");
}

/// The control: `Self: Curve` alone (the old front end's set, and the where-clause form's):
/// one E0277 at the call.
///
/// Prints: `test suite::env_supertrait_assoc_bound::control_without_the_implied_clause_is_e0277 ... ok`
#[test]
fn control_without_the_implied_clause_is_e0277() {
    assert_eq!(codes(false), std::vec![DiagCode::Unsatisfied], "nothing says `<C as Curve>::Fbs: Modulus`");
}
