//! A bound written on a trait's associated type gives its supertraits too. tempo's
//! "831 + 34 diag E0277: Error" (every `serde::Deserialize` derive's `visit_seq`, and
//! tempo_primitives' hand-written `serde::de::Error::missing_field(..)`), reduced:
//!
//! ```text
//! trait Std {}
//! trait Err2: Sized + Std { fn make(n: usize) -> Self; }
//! trait Seq<'d> { type Error: Err2; }
//! fn f<'d, A: Seq<'d>>() -> Result<u8, <A as Seq<'d>>::Error> {
//!     Result::Err(Err2::make(0usize))
//! }
//! ```
//!
//! serde's are `trait SeqAccess<'de> { type Error: Error; .. }` and `trait Error: Sized +
//! StdError { fn invalid_length(len: usize, exp: &dyn Expected) -> Self { .. } .. }`
//! (serde_core src/de/mod.rs); the derive writes `Err(_serde::de::Error::invalid_length(0usize,
//! &".."))` in a fn returning `Result<Self::Value, __A::Error>`. (Lifetimes are erased here, so
//! `Seq` has `Self` alone.) The rules relied on, from the Reference:
//!
//! - "Call expressions", "Disambiguating function calls": "All function calls are sugar for a
//!   more explicit fully-qualified syntax." `Err2::make(0)` names the trait's fn with `Self` to
//!   be inferred; `Err(..)` against the return type makes it `<A as Seq>::Error`.
//! - "Associated Items", "Associated Types": "The optional trait bounds must be fulfilled by
//!   the implementations of the type alias." So for any `A: Seq`, `<A as Seq>::Error` is an
//!   `Err2`: nothing in the function's where clauses says so; the trait does.
//! - "Traits", "Supertraits": "Supertraits are traits that are required to be implemented for
//!   a type to implement a specific trait." and "Supertraits are declared by trait bounds on
//!   the `Self` type of a trait and transitively the supertraits of the traits declared in
//!   those trait bounds." So what is an `Err2` is `Sized` and a `Std`.
//!
//! The cause as found (2026-11-16, by reading; no run): a trait fn's predicates are the
//! trait's own clause and its supertrait clauses (blaze-ladder front2/heads.rs
//! `trait_members`: `inherited`), so the call asks `Self: Err2`, `Self: Sized` and
//! `Self: Std`. The first holds by the written item bound. The solver reads an alias's item
//! bounds as they are given and elaborates only a param env, and the checker gave the written
//! bounds alone (infer/facts.rs `fill_trait`): `<A as Seq>::Error: Std` had no candidate. The
//! bounds are now elaborated where they are given (infer/facts/item_bounds.rs
//! `elaborate_item_bounds`).
//!
//! The direct form `<A as Seq>::Error::make(0)` is not written here: the fixture's qualified
//! path (`Bb::qpath`) has no trait and its lookup of a trait's fn on an alias is another rule;
//! its goal is the same one (`<A as Seq>::Error: Err2` and the supertraits).

use std::format;
use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::world::Def;

/// What `Seq` writes on its associated type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Declared {
    /// `type Error: Err2;`: only that bound is given (its `Sized` is `Err2`'s supertrait).
    Err2,
    /// `type Error: Sub;` with `trait Sub: Err2 {}`: `Err2` is one level up, `Std` two.
    Sub,
    /// `type Error;`: the implied `Sized` alone.
    Nothing,
    /// `type Error: Other;` with `trait Other {}`: a bound that says nothing of `Err2`.
    Other,
}

/// The program of the module's head with `declared` on `Seq::Error`. A trait fn's predicates
/// are given as the front end gives them: the trait's own clause, then its supertrait clauses.
fn world(declared: Declared) -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    fx.enum_("Result", 2, &[("Ok", &[p0]), ("Err", &[p1])]);
    let result = fx.key_of("Result");

    // trait Std {}  trait Err2: Sized + Std { fn make(n: usize) -> Self; }
    let std_tr = fx.tr("Std", 1, Vec::new(), &[]);
    let self_sized = fx.bound(sized, &[p0]);
    let self_std = fx.bound(std_tr, &[p0]);
    let err2 = fx.tr("Err2", 1, std::vec![self_sized, self_std], &[]);
    let usize_ = fx.usize();
    let make = fx.trait_fn_own(err2, "make", false, 0, &[usize_], p0, std::vec![self_sized, self_std]);
    // trait Sub: Err2 {}  trait Other {}
    let self_err2 = fx.bound(err2, &[p0]);
    let sub = fx.tr("Sub", 1, std::vec![self_err2], &[]);
    let other = fx.tr("Other", 1, Vec::new(), &[]);

    // trait Seq { type Error: <declared>; }
    let seq = fx.tr("Seq", 1, Vec::new(), &["Error"]);
    let seq_err = fx.alias(seq, "Error", &[p0]);
    let bounds = match declared {
        Declared::Err2 => std::vec![fx.bound(err2, &[seq_err])],
        Declared::Sub => std::vec![fx.bound(sub, &[seq_err])],
        Declared::Nothing => std::vec![fx.bound(sized, &[seq_err])],
        Declared::Other => std::vec![fx.bound(other, &[seq_err]), fx.bound(sized, &[seq_err])],
    };
    fx.set_assoc_bounds(seq, "Error", bounds);

    fx.use_path("make", Def::Fn(make));

    // fn f<A: Seq>() -> Result<u8, <A as Seq>::Error>
    let is_seq = fx.bound(seq, &[p0]);
    let u8_ = fx.u8();
    let res = fx.adt(result, &[u8_, seq_err]);
    let f = fx.func("f", 1, &[], res, std::vec![is_seq]);
    fx.finish();
    (world, f)
}

/// `Result::Err(Err2::make(0usize))`: the body, its root, the path `Err2::make` and its call.
fn body() -> (Bb, ExprId, ExprId, ExprId) {
    let mut b = Bb::new();
    let make = b.path(&["make"]);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::Usize)));
    let call = b.call(make, &[zero]);
    let err = b.path(&["Result", "Err"]);
    let wrapped = b.call(err, &[call]);
    let root = b.block(&[], Some(wrapped));
    (b, root, make, call)
}

/// Each diagnostic as `code at <expression> found <type>`.
fn diags_shown(fx: &Fx, c: &crate::columns::Checked<'_>) -> Vec<String> {
    c.diags
        .iter()
        .map(|d| format!("{} at {} found {:?}", d.code.code(), d.at.start, d.found.map(|t| super::show(fx, c.types.view(), t))))
        .collect()
}

/// `type Error: Err2;` and, two levels up, `type Error: Sub;`: the body checks with no
/// diagnostic. Without the supertraits of the written bound this is E0277 at `Err2::make`
/// (`<P0 as Seq>::Error: Std`, and `Sized` under `Result::Err`'s own clause), the tempo class.
///
/// Prints: `test suite::assoc_bound_supers::a_declared_bound_gives_its_supertraits ... ok`
#[test]
fn a_declared_bound_gives_its_supertraits() {
    for declared in [Declared::Err2, Declared::Sub] {
        let (fx, f) = world(declared);
        let (b, root, _, _) = body();
        let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
        let d = diags_shown(&fx, &c);
        let which = if declared == Declared::Err2 { "type Error: Err2;" } else { "type Error: Sub;" };
        assert!(d.is_empty(), "`{which}`: the body has diagnostics:\n{}", d.join("\n"));
    }
}

/// The control (rule 23: remove the cause, the effect goes): `type Error;` says nothing, so
/// `<A as Seq>::Error: Err2` does not hold and the call is refused with E0277 at `Err2::make`
/// (the path or its call), and nowhere else. A fix that answers every goal on an alias fails
/// here.
///
/// Prints: `test suite::assoc_bound_supers::no_declared_bound_is_still_refused ... ok`
#[test]
fn no_declared_bound_is_still_refused() {
    refused(Declared::Nothing, "type Error;");
}

/// `type Error: Other;`: a declared bound that is not `Err2` and has no `Err2` above it
/// proves nothing of it: E0277 at `Err2::make` still. A fix that takes any declared bound as
/// proof fails here.
///
/// Prints: `test suite::assoc_bound_supers::an_unrelated_declared_bound_is_still_refused ... ok`
#[test]
fn an_unrelated_declared_bound_is_still_refused() {
    refused(Declared::Other, "type Error: Other;");
}

fn refused(declared: Declared, which: &str) {
    let (fx, f) = world(declared);
    let (b, root, make, call) = body();
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    let d = diags_shown(&fx, &c);
    assert!(!c.diags.is_empty(), "`{which}`: the body checked, so `<A as Seq>::Error: Err2` held with no bound saying so");
    let elsewhere: Vec<&String> = c
        .diags
        .iter()
        .zip(d.iter())
        .filter(|(x, _)| x.code != DiagCode::Unsatisfied || x.at.start < make.0 || x.at.start > call.0)
        .map(|(_, s)| s)
        .collect();
    assert!(elsewhere.is_empty(), "`{which}`: diagnostics other than E0277 at `Err2::make` (expressions {}..={}):\n{elsewhere:?}", make.0, call.0);
}
