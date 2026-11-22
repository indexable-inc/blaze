//! A where-clause relating two projections fixes a trait's other parameter before an argument
//! coerces to it: core's `impl<Ptr: Deref, Q: Deref> PartialOrd<Pin<Q>> for Pin<Ptr> where
//! Ptr::Target: PartialOrd<Q::Target>` calls `Ptr::Target::lt(self, other)` on autoderef'd
//! arguments (tempo's build census, 2026-11-18: the `Pin` rows, 7 bodies; arrayvec's
//! `(**self).partial_cmp(other)` the same class with an impl as the one candidate). rustc
//! `check_argument_types` (fn_ctxt/checks.rs) resolves each formal input with the pending
//! obligations first, so `Rhs` is `Q::Target` from the env's one candidate when `&&Q::Target`
//! coerces to `&Rhs`; the checker's home of that rule is machine.rs
//! `resolve_with_obligations` (suite/coerce_target_obligations.rs pins it through an impl).
//! This file pins it through the environment, the where-clause on two projections as the one
//! candidate:
//!
//! ```text
//! trait Deref { type Target; }
//! trait PartialOrd<Rhs> { fn lt(&self, other: &Rhs) -> u8; }
//! fn f<P: Deref, Q: Deref>(a: &P::Target, b: &&Q::Target) -> u8
//!     where P::Target: PartialOrd<Q::Target>
//! { lt(a, b) }                                           // clean in rustc: `&&` derefs to `&Q::Target`
//! ```
//!
//! Controls (rule 23): the where-clause removed: E0277 (nothing makes `P::Target` a
//! `PartialOrd`); the where-clause naming `P::Target` as `Rhs`: E0308 at the second argument
//! (`&&Q::Target` is no `&P::Target`), so the clause, not the argument, chose `Rhs`.

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::world::Def;

/// What `f`'s where-clause says.
#[derive(Clone, Copy)]
enum Clause {
    /// `P::Target: PartialOrd<Q::Target>`.
    OtherRhs,
    /// None.
    Nothing,
    /// `P::Target: PartialOrd<P::Target>`.
    SameRhs,
}

fn world(clause: Clause) -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let (p0, p1) = (fx.param(0), fx.param(1));
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    let ord = fx.tr("PartialOrd", 2, Vec::new(), &[]);
    let u8_ = fx.u8();
    let r_self = fx.r(Mutbl::Not, p0);
    let r_rhs = fx.r(Mutbl::Not, p1);
    let lt = fx.trait_fn(ord, "lt", true, &[r_self, r_rhs], u8_);
    fx.use_path("lt", Def::Fn(lt));
    let pt = fx.alias(deref, "Target", &[p0]);
    let qt = fx.alias(deref, "Target", &[p1]);
    let mut clauses = std::vec![fx.bound(deref, &[p0]), fx.bound(deref, &[p1])];
    match clause {
        Clause::OtherRhs => clauses.push(fx.bound(ord, &[pt, qt])),
        Clause::Nothing => {}
        Clause::SameRhs => clauses.push(fx.bound(ord, &[pt, pt])),
    }
    let a = fx.r(Mutbl::Not, pt);
    let r_qt = fx.r(Mutbl::Not, qt);
    let b = fx.r(Mutbl::Not, r_qt);
    let f = fx.func("f", 2, &[a, b], u8_, clauses);
    fx.finish();
    (fx, f)
}

/// `{ lt(a, b) }` as `f`'s body, checked: the codes. Expressions: the path 0, `a` 1, `b` 2,
/// the call 3, the block 4.
fn codes(clause: Clause) -> Vec<DiagCode> {
    let (fx, f) = world(clause);
    let mut b = Bb::new();
    let pa = b.param("a");
    let pb = b.param("b");
    let lt = b.path(&["lt"]);
    let ea = b.local(pa);
    let eb = b.local(pb);
    let call = b.call(lt, &[ea, eb]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// The env's one candidate fixes `Rhs = Q::Target`; `&&Q::Target` then derefs to it: clean.
///
/// Prints: `test suite::env_where_projection_rhs::the_where_clause_fixes_rhs_before_the_coercion ... ok`
#[test]
fn the_where_clause_fixes_rhs_before_the_coercion() {
    assert_eq!(codes(Clause::OtherRhs), Vec::<DiagCode>::new(), "`P::Target: PartialOrd<Q::Target>` decides `Rhs`");
}

/// The control: no where-clause, no candidate: the goal fails (E0277; what else is reported
/// beside it is not pinned, only that the body is not clean and E0277 is among it).
///
/// Prints: `test suite::env_where_projection_rhs::control_without_the_clause_is_e0277 ... ok`
#[test]
fn control_without_the_clause_is_e0277() {
    let c = codes(Clause::Nothing);
    assert!(c.contains(&DiagCode::Unsatisfied), "no clause makes `P::Target` a `PartialOrd`: {c:?}");
}

/// The control: the clause names `P::Target` as `Rhs`, so the second argument must be a
/// `&P::Target`: E0308 there, no E0277.
///
/// Prints: `test suite::env_where_projection_rhs::control_the_clause_decides_rhs ... ok`
#[test]
fn control_the_clause_decides_rhs() {
    assert_eq!(codes(Clause::SameRhs), std::vec![DiagCode::Mismatch], "`Rhs` is the clause's `P::Target`");
}
