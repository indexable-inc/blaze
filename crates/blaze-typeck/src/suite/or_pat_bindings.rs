//! Or-pattern binding unification (rules/pat.rs `unify_or_bindings`): every alternative of an
//! or-pattern that binds a name unifies that binding's type with the first alternative's binding
//! of the same name. This both constrains inference variables (as in `Finish::exited`:
//! `match (a, Ok(0)) { (Err(e), _) | (_, Err(e)) => ..., _ => () }`, where the second `Result`'s
//! error type is inferred from the first alternative's `e`) and reports `E0308` when two
//! alternatives bind the same name at conflicting types.
//!
//! ```text
//! enum Res<T, E> { Ok(T), Err(E) }
//! fn infer_err(a: Res<u8, u16>) {
//!     match (a, Res::Ok(0u8)) {
//!         (Res::Err(e), _) | (_, Res::Err(e)) => { let _: u16 = e; }
//!         _ => {}
//!     }
//! }
//! fn mismatch_err(a: Res<u8, u16>, b: Res<u8, u32>) {
//!     match (a, b) {
//!         (Res::Err(e), _) | (_, Res::Err(e)) => {}
//!         _ => {}
//!     }
//! }
//! ```

use super::check_body;
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let p0 = fx.ty(PNode::Param(0));
    let p1 = fx.ty(PNode::Param(1));
    let (res, _) = fx.enum_("Res", 2, &[("Ok", &[p0]), ("Err", &[p1])]);
    let unit = fx.unit();
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let u16_ = fx.ty(PNode::Uint(UintTy::U16));
    let u32_ = fx.ty(PNode::Uint(UintTy::U32));
    let res_u16 = fx.adt(res, &[u8_, u16_]);
    let res_u32 = fx.adt(res, &[u8_, u32_]);
    let infer_err = fx.func("infer_err", 0, &[res_u16], unit, Default::default());
    let mismatch_err = fx.func("mismatch_err", 0, &[res_u16, res_u32], unit, Default::default());
    fx.finish();
    (fx, infer_err, mismatch_err)
}

#[test]
fn or_pattern_unifies_error_type_variable_across_alternatives() {
    let (fx, infer_err, _) = world();
    let mut b = Bb::new();
    let a = b.param("a");
    let a_expr = b.local(a);
    let ok_fn = b.path(&["Res", "Ok"]);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let ok_zero = b.call(ok_fn, &[zero]);
    let scrut = b.tuple(&[a_expr, ok_zero]);

    let e1 = b.bind("e");
    let err1 = b.variant_pat(&["Res", "Err"], &[e1]);
    let w1 = b.wild();
    let alt1 = b.tuple_pat(&[err1, w1]);

    let w2 = b.wild();
    let e2 = b.bind("e");
    let err2 = b.variant_pat(&["Res", "Err"], &[e2]);
    let alt2 = b.tuple_pat(&[w2, err2]);

    let or_pat = b.or_pat(&[alt1, alt2]);
    let arm1 = b.block(&[], None);
    let catch_all = b.wild();
    let arm2 = b.block(&[], None);
    let m = b.match_(scrut, &[(or_pat, arm1), (catch_all, arm2)]);
    let root = b.block(&[], Some(m));

    let checked = check_body(&fx, &b, infer_err, root, Plant::None, InferPlant::None);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);
}

#[test]
fn or_pattern_reports_mismatch_when_alternatives_bind_different_types() {
    let (fx, _, mismatch_err) = world();
    let mut b = Bb::new();
    let a = b.param("a");
    let bb = b.param("b");
    let a_expr = b.local(a);
    let b_expr = b.local(bb);
    let scrut = b.tuple(&[a_expr, b_expr]);

    let e1 = b.bind("e");
    let err1 = b.variant_pat(&["Res", "Err"], &[e1]);
    let w1 = b.wild();
    let alt1 = b.tuple_pat(&[err1, w1]);

    let w2 = b.wild();
    let e2 = b.bind("e");
    let err2 = b.variant_pat(&["Res", "Err"], &[e2]);
    let alt2 = b.tuple_pat(&[w2, err2]);

    let or_pat = b.or_pat(&[alt1, alt2]);
    let arm1 = b.block(&[], None);
    let catch_all = b.wild();
    let arm2 = b.block(&[], None);
    let m = b.match_(scrut, &[(or_pat, arm1), (catch_all, arm2)]);
    let root = b.block(&[], Some(m));

    let checked = check_body(&fx, &b, mismatch_err, root, Plant::None, InferPlant::None);
    assert_eq!(checked.diags.len(), 1, "{:?}", checked.diags);
    assert_eq!(checked.diags[0].code, DiagCode::Mismatch, "{:?}", checked.diags);
}
