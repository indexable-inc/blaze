//! A projection in the signature is settled before the body is checked, so a body value that
//! does not fit it is a finding at that value, never one without a place.
//!
//! ```text
//! trait Tr { type A; }
//! fn wide<X: Tr<A = u8>>() -> X::A { 1u16 }        // `1u16` (ExprId 0) is the place: E0308
//! ```
//!
//! Rule: the Reference, "Trait and lifetime bounds": "When type-checking and borrow-checking a
//! generic item, the bounds can be used to determine that a trait is implemented for a type";
//! the bound `X: Tr<A = u8>` makes the declared output `X::A` the type `u8` inside `wide`, so
//! `1u16` at the return coercion site ([type.coercion.site.fn]: "Function results—either the
//! final line of a block if it is not semicolon-terminated or any expression in a return
//! statement") is a `u16` where a `u8` is wanted. The checker's own invariant (error.rs
//! `DiagWithoutPlace`): a finding always points at its place.
//!
//! The mechanism: lowering the output `X::A` gives a fresh `?r` and the obligation
//! `<X as Tr>::A == ?r` with no place (`Cause::Body`: it is the signature's). Before the fix
//! nothing selected it before the body ran, so `1u16`, checked with `?r` expected, bound
//! `?r = u16`; the obligation then failed as `DiagWithoutPlace { code: Unsatisfied, name: None }`
//! (blaze-ladder tests/front2_lower_made.rs `the_made_heads_binding_is_kept`, case `wide`, run
//! of 2026-11-16 18:05). rules/control.rs `start_body` now selects once after the signature is
//! lowered (rustc's `check_fn` takes the signature normalized in the param env), so `?r = u8`
//! before the body. The planted bug `Plant::SignatureUnsettled` skips that select; the control
//! sees the finding move off `1u16`: the late projection fails at the body's block (the output
//! signature obligation's place, machine.rs `sig_place`), where E0308 at `1u16` is wanted. That
//! finding is E0271, not E0277: `X: Tr` holds by the bound and `X: Tr<A = u8>` decides `X::A` to
//! `u8`, another type than the term `u16`, rustc's "type mismatch resolving `<X as Tr>::A ==
//! u16`" (`report_projection_error`; machine.rs files it as `ProjectionMismatch`).
//!
//! The second case is the census class `check: Check: DiagWithoutPlace { code: Unsatisfied,
//! name: None }` (tempo build census 7, 8 functions, e.g. primeorder's `<impl<C>
//! From<AffinePoint<C>> for Sec1Point<C>>::from`): a projection goal that a relation defers
//! (unifying an alias with another type, `<X as Tr>::A` against `u16`) and the signature's own
//! obligations were registered with `Cause::Body`, so when one failed the finding had no place.
//!
//! ```text
//! fn rigid<X: Tr>(a: X::A) -> u16 { a }       // `a` (ExprId 0): rustc's E0308 there
//! ```
//!
//! Each deferred goal is now an obligation of the node the walk was checking
//! (machine.rs `Machine::place`, `TypeCtx::unify_at`), and each signature obligation one of
//! its parameter or of the body (`sig_place`). The plant `Plant::ObligationsWithoutPlace` puts
//! both back at `Cause::Body`; the control sees `DiagWithoutPlace` come back.

use std::format;
use std::vec::Vec;

use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::error::CheckError;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferCtx;
use crate::machine::Plant;
use crate::world::Needs;
use crate::{Outcome, check_with};

fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &["A"]);
    let p0 = fx.param(0);
    let u8_ = fx.u8();
    let a = fx.alias(tr, "A", &[p0]);
    let is_tr = fx.bound(tr, &[p0]);
    let a_is_u8 = fx.proj(tr, &[p0], "A", u8_);
    let wide = fx.func("wide", 1, &[], a, std::vec![is_tr, a_is_u8]);
    fx.finish();
    (fx, wide)
}

/// `{ 1u16 }`: `1u16` is ExprId 0, the block ExprId 1.
fn body() -> (Bb, ExprId, ExprId) {
    let mut b = Bb::new();
    let e0 = b.int(Some(IntSuffix::Uint(UintTy::U16)));
    let root = b.block(&[], Some(e0));
    (b, e0, root)
}

/// The body's findings as (code, start of range), sorted; or the check's own error.
fn findings(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, plant: Plant) -> Result<Vec<(DiagCode, u32)>, CheckError> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut cx = InferCtx::new_in(alloc);
    match check_with(&b.view(owner, root), fx, &mut cx, &Needs::new_in(alloc), plant, alloc)? {
        Outcome::Done(c) => {
            let mut got: Vec<(DiagCode, u32)> = c.diags.iter().map(|d| (d.code, d.at.start)).collect();
            got.sort();
            Ok(got)
        }
        Outcome::Need(n) => panic!("`{}`: the world missed {:?}", fx.name_of(owner), n.questions),
    }
}

/// Expected line: "signature_projection_is_settled_before_the_body: ok (control caught)".
#[test]
fn signature_projection_is_settled_before_the_body() {
    let (fx, wide) = world();
    let (b, lit, root) = body();
    let mut wrong = Vec::new();
    let got = findings(&fx, &b, wide, root, Plant::None);
    match &got {
        // The tail's own place (as suite/opaque.rs's `u16` return), or the block's when the
        // return site reports at its source; either is a place, never `DiagWithoutPlace`.
        Ok(d) if d.as_slice() == [(DiagCode::Mismatch, lit.0)] || d.as_slice() == [(DiagCode::Mismatch, root.0)] => {}
        other => wrong.push(format!("wide: want [(Mismatch, {})] (`1u16` where `u8` is wanted), got {other:?}", lit.0)),
    }
    // Settled late, the projection fails after `1u16` bound `?r`: a finding with a place (the
    // body's block, where the output's obligation is filed), but not E0308 at `1u16`. The trait
    // goal holds and the bound decides `X::A = u8` against the term `u16`: E0271.
    let planted = findings(&fx, &b, wide, root, Plant::SignatureUnsettled);
    match &planted {
        Ok(d) if !d.contains(&(DiagCode::Mismatch, lit.0)) && d.contains(&(DiagCode::ProjectionMismatch, root.0)) => {}
        other => wrong.push(format!(
            "control: Plant::SignatureUnsettled should give an E0271 at the block ({}), not E0308 at `1u16`, got {other:?}",
            root.0
        )),
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    std::println!("signature_projection_is_settled_before_the_body: ok (control caught)");
}

/// `trait Tr { type A; } fn rigid<X: Tr>(a: X::A) -> u16`: `a` is ExprId 0 and PatId 0 (range
/// start 1000), the block ExprId 1.
fn rigid() -> (Fx, ItemKey, Bb, ExprId, ExprId) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &["A"]);
    let p0 = fx.param(0);
    let a_ty = fx.alias(tr, "A", &[p0]);
    let u16_ = fx.ty(crate::PNode::Uint(UintTy::U16));
    let is_tr = fx.bound(tr, &[p0]);
    let f = fx.func("rigid", 1, &[a_ty], u16_, std::vec![is_tr]);
    fx.finish();
    let mut b = Bb::new();
    let a = b.param("a");
    let e0 = b.local(a);
    let root = b.block(&[], Some(e0));
    (fx, f, b, e0, root)
}

/// `trait Tr { type A; } fn unbound<X>(a: <X as Tr>::A) {}`: `a` is PatId 0 (range start
/// 1000), the empty block the root.
fn unbound() -> (Fx, ItemKey, Bb, ExprId) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &["A"]);
    let p0 = fx.param(0);
    let a_ty = fx.alias(tr, "A", &[p0]);
    let unit = fx.unit();
    let f = fx.func("unbound", 1, &[a_ty], unit, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let _a = b.param("a");
    let root = b.block(&[], None);
    (fx, f, b, root)
}

/// Expected line: "a_deferred_projection_is_at_the_relating_expression: ok (control caught)".
#[test]
fn a_deferred_projection_is_at_the_relating_expression() {
    let (fx, f, b, e0, root) = rigid();
    let mut wrong = Vec::new();
    // `X::A` is rigid under `X: Tr` (no clause names it), so `a` where `u16` is wanted is a
    // finding (rustc: E0308 at `a`); whichever code the checker gives, it has a place in the
    // body: `a`, the block, or the parameter `a`.
    let places = [e0.0, root.0, 1000];
    match findings(&fx, &b, f, root, Plant::None) {
        Ok(d) if !d.is_empty() && d.iter().all(|&(_, at)| places.contains(&at)) => {}
        other => wrong.push(format!("rigid: want findings at {places:?} (`a` where `u16` is wanted), got {other:?}")),
    }
    // Since `start_body` selects before the body (the test above), `X::A` is rigid when `a` is
    // related to `u16`: the relation defers nothing and the mismatch is at `a` with or without
    // the plant (run of 2026-11-17 03:4x: `Ok([(Mismatch, 0)])` under the plant). So the
    // control is the other half of the fix, a signature obligation that fails:
    // `fn unbound<X>(a: <X as Tr>::A) {}`, no `X: Tr` (the Reference, "Trait and lifetime
    // bounds": the projection needs `X: Tr`, E0277). It is a finding at the parameter `a`;
    // with the obligation filed at `Cause::Body` (the plant), it has no place.
    let (ufx, unbound, ub, uroot) = unbound();
    match findings(&ufx, &ub, unbound, uroot, Plant::None) {
        Ok(d) if !d.is_empty() && d.iter().all(|&(c, at)| c == DiagCode::Unsatisfied && at == 1000) => {}
        other => wrong.push(format!("unbound: want E0277 at the parameter `a` (1000), got {other:?}")),
    }
    let planted = findings(&ufx, &ub, unbound, uroot, Plant::ObligationsWithoutPlace);
    if !matches!(planted, Err(CheckError::DiagWithoutPlace { name: None, .. })) {
        wrong.push(format!(
            "control: Plant::ObligationsWithoutPlace should give DiagWithoutPlace (the signature obligation filed at \
             `Cause::Body`), got {planted:?}"
        ));
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    std::println!("a_deferred_projection_is_at_the_relating_expression: ok (control caught)");
}
