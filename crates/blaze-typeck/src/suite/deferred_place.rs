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
//! sees the placeless error come back.

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
    let planted = findings(&fx, &b, wide, root, Plant::SignatureUnsettled);
    if !matches!(planted, Err(CheckError::DiagWithoutPlace { name: None, .. })) {
        wrong.push(format!("control: Plant::SignatureUnsettled should give DiagWithoutPlace, got {planted:?}"));
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    std::println!("signature_projection_is_settled_before_the_body: ok (control caught)");
}
