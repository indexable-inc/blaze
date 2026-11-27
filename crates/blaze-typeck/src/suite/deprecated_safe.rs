//! `#[rustc_deprecated_safe_2024]` / `#[deprecated_safe]` unsafety rules by edition
//! (rules/unsafety.rs `unsafe_call`, `TypeAttrs::DEPRECATED_SAFE_2024`, RFC 3458).
//!
//! Direct calls to an `unsafe fn` item carrying `TypeAttrs::DEPRECATED_SAFE_2024` are safe in
//! editions before 2024 (2015, 2018, 2021) and require `unsafe` (E0133) from edition 2024.
//! Controls verify that:
//! - Edition 2024 refuses the same call with `DiagCode::RequiresUnsafe` (and files
//!   `TypedFinding::UnsafeOpInUnsafeFn` in an `unsafe fn` body);
//! - Edition 2021 still refuses a plain `unsafe fn` without `DEPRECATED_SAFE_2024`;
//! - Edition 2021 still refuses an indirect call through a reified `unsafe fn()` pointer;
//! - Edition 2021 still enforces `TypeAttrs::TARGET_FEATURE` when combined with
//!   `DEPRECATED_SAFE_2024`;
//! - Cross-edition macro spans follow `BodyView::expr_edition`.

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, Edition, FnPtrHeader, ItemKey, LangItem, Mutbl, Safety};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::rules::args_inline::TypedFinding;
use crate::world::TypeAttrs;

const UNSAFE_RUST: FnPtrHeader = FnPtrHeader { abi: Abi::Rust, safety: Safety::Unsafe, c_variadic: false };

struct World {
    fx: Fx,
    owner_safe: ItemKey,
    owner_unsafe: ItemKey,
    owner_method: ItemKey,
    owner_ptr: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unit = fx.unit();
    let s = fx.strukt("S", 0, &[]);
    let s_ty = fx.adt(s, &[]);
    let r_s = fx.r(Mutbl::Not, s_ty);

    let dep_safe = fx.func("dep_safe", 0, &[], unit, Vec::new());
    fx.set_header(dep_safe, UNSAFE_RUST);
    fx.set_attrs(dep_safe, TypeAttrs(TypeAttrs::DEPRECATED_SAFE_2024));

    let plain_unsafe = fx.func("plain_unsafe", 0, &[], unit, Vec::new());
    fx.set_header(plain_unsafe, UNSAFE_RUST);

    let dep_safe_tf = fx.func("dep_safe_tf", 0, &[], unit, Vec::new());
    fx.set_header(dep_safe_tf, UNSAFE_RUST);
    fx.set_attrs(dep_safe_tf, TypeAttrs(TypeAttrs::DEPRECATED_SAFE_2024 | TypeAttrs::TARGET_FEATURE));

    let imp = fx.imp("impl S", 0, s_ty, None, &[], Vec::new());
    let dep_method = fx.impl_fn(imp, "dep_m", true, &[r_s], unit);
    fx.set_header(dep_method, UNSAFE_RUST);
    fx.set_attrs(dep_method, TypeAttrs(TypeAttrs::DEPRECATED_SAFE_2024));

    let plain_method = fx.impl_fn(imp, "plain_m", true, &[r_s], unit);
    fx.set_header(plain_method, UNSAFE_RUST);

    let owner_safe = fx.func("owner_safe", 0, &[], unit, Vec::new());
    let owner_unsafe = fx.func("owner_unsafe", 0, &[], unit, Vec::new());
    fx.set_header(owner_unsafe, UNSAFE_RUST);
    let owner_method = fx.func("owner_method", 0, &[r_s], unit, Vec::new());
    let owner_ptr = fx.func("owner_ptr", 0, &[], unit, Vec::new());

    fx.finish();
    World { fx, owner_safe, owner_unsafe, owner_method, owner_ptr }
}

fn call_free(w: &World, owner: ItemKey, callee: &str, body_ed: Edition, call_ed: Option<Edition>) -> (Vec<DiagCode>, Vec<TypedFinding>) {
    let mut b = Bb::new();
    b.body_edition(body_ed);
    let p = b.path(&[callee]);
    let c = b.call(p, &[]);
    if let Some(ed) = call_ed {
        b.expr_edition(c, ed);
    }
    let root = b.block(&[], Some(c));
    b.clear_unsafe_blocks();
    let checked = check_body(&w.fx, &b, owner, root, Plant::None, InferPlant::None);
    (checked.diags.iter().map(|d| d.code).collect(), checked.findings.to_vec())
}

fn call_method(w: &World, method: &str, body_ed: Edition) -> Vec<DiagCode> {
    let mut b = Bb::new();
    b.body_edition(body_ed);
    let s = b.param("s");
    let recv = b.local(s);
    let c = b.method(recv, method, &[]);
    let root = b.block(&[], Some(c));
    b.clear_unsafe_blocks();
    check_body(&w.fx, &b, w.owner_method, root, Plant::None, InferPlant::None).diags.iter().map(|d| d.code).collect()
}

#[test]
fn deprecated_safe_2024_free_fn_allowed_before_2024_and_refused_in_2024() {
    let w = world();
    for ed in [Edition::E2015, Edition::E2018, Edition::E2021] {
        let (diags, findings) = call_free(&w, w.owner_safe, "dep_safe", ed, None);
        assert!(diags.is_empty(), "edition {ed:?} direct call to deprecated_safe_2024 must be clean: {diags:?}");
        assert!(findings.is_empty(), "edition {ed:?} must emit no findings: {findings:?}");
    }
    // Control 1: Edition 2024 refuses the exact same call.
    let (diags, _) = call_free(&w, w.owner_safe, "dep_safe", Edition::E2024, None);
    assert_eq!(diags, std::vec![DiagCode::RequiresUnsafe]);

    // Control 2: Edition 2021 still refuses a plain `unsafe fn`.
    let (diags, _) = call_free(&w, w.owner_safe, "plain_unsafe", Edition::E2021, None);
    assert_eq!(diags, std::vec![DiagCode::RequiresUnsafe]);
}

#[test]
fn deprecated_safe_2024_method_allowed_before_2024_and_refused_in_2024() {
    let w = world();
    assert_eq!(call_method(&w, "dep_m", Edition::E2021), Vec::<DiagCode>::new());
    assert_eq!(call_method(&w, "dep_m", Edition::E2024), std::vec![DiagCode::RequiresUnsafe]);
    assert_eq!(call_method(&w, "plain_m", Edition::E2021), std::vec![DiagCode::RequiresUnsafe]);
}

#[test]
fn deprecated_safe_2024_fn_pointer_call_still_requires_unsafe_in_2021() {
    let w = world();
    let mut b = Bb::new();
    b.body_edition(Edition::E2021);
    let unit = b.ty(PNode::Tuple(crate::ids::List::EMPTY));
    let io = b.t.intern_list(&[unit]).expect("intern");
    let ptr_ty = b.ty(PNode::FnPtr(io, UNSAFE_RUST));
    let init = b.path(&["dep_safe"]);
    let f = b.bind("f");
    let s = b.let_(f, Some(ptr_ty), init);
    let callee = b.local(f);
    let call = b.call(callee, &[]);
    let root = b.block(&[s], Some(call));
    b.clear_unsafe_blocks();
    let diags: Vec<DiagCode> = check_body(&w.fx, &b, w.owner_ptr, root, Plant::None, InferPlant::None).diags.iter().map(|d| d.code).collect();
    assert_eq!(diags, std::vec![DiagCode::RequiresUnsafe]);
}

#[test]
fn deprecated_safe_2024_still_checks_target_feature_in_2021() {
    let w = world();
    let (diags, _) = call_free(&w, w.owner_safe, "dep_safe_tf", Edition::E2021, None);
    assert_eq!(diags, std::vec![DiagCode::RequiresUnsafe]);
}

#[test]
fn deprecated_safe_2024_follows_call_expr_edition_and_unsafe_fn_lint() {
    let w = world();
    // 2021 macro expanding inside a 2024 body: allowed.
    let (diags, _) = call_free(&w, w.owner_safe, "dep_safe", Edition::E2024, Some(Edition::E2021));
    assert!(diags.is_empty(), "2021 macro call in 2024 body must be clean: {diags:?}");
    // 2024 macro expanding inside a 2021 body: refused.
    let (diags, _) = call_free(&w, w.owner_safe, "dep_safe", Edition::E2021, Some(Edition::E2024));
    assert_eq!(diags, std::vec![DiagCode::RequiresUnsafe]);

    // Inside an `unsafe fn`: 2024 emits `UnsafeOpInUnsafeFn`, 2021 emits nothing.
    let (diags_24, findings_24) = call_free(&w, w.owner_unsafe, "dep_safe", Edition::E2024, None);
    assert!(diags_24.is_empty());
    assert!(matches!(findings_24.as_slice(), [TypedFinding::UnsafeOpInUnsafeFn { .. }]));
    let (diags_21, findings_21) = call_free(&w, w.owner_unsafe, "dep_safe", Edition::E2021, None);
    assert!(diags_21.is_empty());
    assert!(findings_21.is_empty());
}
