//! The edition 2024 binding-mode errors by the edition of the pattern's own words, not the
//! body's (rules/pat.rs, `BodyView::pat_edition`).
//!
//! The Edition Guide, "Rust 2024: Match ergonomics reservations": "In Rust 2024, writing
//! `mut`, `ref`, or `ref mut` on a binding is an error when the default binding mode is not
//! `move`", and "reference patterns can only appear in the fully-explicit form". rustc decides
//! it by span: `add_rust_2024_migration_desugared_pat` labels the subpattern's span (with the
//! subpattern's own syntax context), and rustc_mir_build's migration.rs `emit` makes it a hard
//! error when a label's span is `at_least_rust_2024()`, else the migration lint. So a pattern a
//! 2021 crate's macro wrote is not refused in a 2024 body, and the same pattern written in the
//! body is.
//!
//! The world:
//! ```text
//! #[lang = "sized"] trait Sized {}
//! fn f(a: &(u8,));     // let (mut x,) = a;
//! fn g(a: &(&u8,));    // let (&y,) = a;
//! ```

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Edition, ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The world of the module doc, and `f`, `g`.
fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unit = fx.unit();
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let tup = fx.tuple(&[u8_]);
    let ref_tup = fx.r(Mutbl::Not, tup);
    let f = fx.func("f", 0, &[ref_tup], unit, Vec::new());
    let ref_u8 = fx.r(Mutbl::Not, u8_);
    let tup_ref = fx.tuple(&[ref_u8]);
    let ref_tup_ref = fx.r(Mutbl::Not, tup_ref);
    let g = fx.func("g", 0, &[ref_tup_ref], unit, Vec::new());
    fx.finish();
    (fx, f, g)
}

/// `fn f(a: &(u8,)) { let (mut x,) = a; }` in a body of edition `body`, the `mut x` written
/// by a macro of edition `written` (`None`: written in the body). The tuple pattern peels the
/// `&`, so `mut x` is under the `ref` default mode.
fn mut_under_ref(body: Edition, written: Option<Edition>) -> Vec<DiagCode> {
    let (fx, f, _) = world();
    let mut b = Bb::new();
    b.body_edition(body);
    let a = b.param("a");
    let x = b.bind_mut("x");
    if let Some(ed) = written {
        b.pat_edition(x, ed);
    }
    let pat = b.tuple_pat(&[x]);
    let init = b.local(a);
    let s = b.let_(pat, None, init);
    let root = b.block(&[s], None);
    check_body(&fx, &b, f, root, Plant::None, InferPlant::None).diags.iter().map(|d| d.code).collect()
}

/// `fn g(a: &(&u8,)) { let (&y,) = a; }`, as [`mut_under_ref`] with the `&y` the marked one.
fn ref_under_ref(body: Edition, written: Option<Edition>) -> Vec<DiagCode> {
    let (fx, _, g) = world();
    let mut b = Bb::new();
    b.body_edition(body);
    let a = b.param("a");
    let y = b.bind("y");
    let amp = b.ref_pat(Mutbl::Not, y);
    if let Some(ed) = written {
        b.pat_edition(amp, ed);
    }
    let pat = b.tuple_pat(&[amp]);
    let init = b.local(a);
    let s = b.let_(pat, None, init);
    let root = b.block(&[s], None);
    check_body(&fx, &b, g, root, Plant::None, InferPlant::None).diags.iter().map(|d| d.code).collect()
}

/// A 2021 macro's `mut x` under the `ref` default mode in a 2024 body: no error (the
/// migration lint's case). The checker refused it by the body's edition.
#[test]
fn a_2021_macros_mut_binding_in_a_2024_body_is_not_refused() {
    assert_eq!(mut_under_ref(Edition::E2024, Some(Edition::E2021)), Vec::<DiagCode>::new());
}

/// The negative twin: the same `mut x` written in the 2024 body is the 2024 error; and a 2024
/// macro's in a 2021 body is too (the pattern's edition, not the body's, either way).
#[test]
fn a_2024_mut_binding_under_ref_is_refused() {
    assert_eq!(mut_under_ref(Edition::E2024, None), std::vec![DiagCode::BindingModifier2024]);
    assert_eq!(mut_under_ref(Edition::E2021, Some(Edition::E2024)), std::vec![DiagCode::BindingModifier2024]);
    assert_eq!(mut_under_ref(Edition::E2021, None), Vec::<DiagCode>::new());
}

/// A 2021 macro's `&y` under the `ref` default mode in a 2024 body: no error.
#[test]
fn a_2021_macros_ref_pattern_in_a_2024_body_is_not_refused() {
    assert_eq!(ref_under_ref(Edition::E2024, Some(Edition::E2021)), Vec::<DiagCode>::new());
}

/// The negative twin: the same `&y` written in the 2024 body is the 2024 error.
#[test]
fn a_2024_ref_pattern_under_ref_is_refused() {
    assert_eq!(ref_under_ref(Edition::E2024, None), std::vec![DiagCode::BindingModifier2024]);
    assert_eq!(ref_under_ref(Edition::E2021, None), Vec::<DiagCode>::new());
}
