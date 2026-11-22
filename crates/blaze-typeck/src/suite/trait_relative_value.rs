//! A value path through a trait with `Self` left to inference: `Trait::NAME` is
//! `<_ as Trait>::NAME` (rules/lit_path.rs `trait_relative_value`).
//!
//! The Reference, "Paths", "Paths in expressions" and "Qualified paths": an associated item may
//! be named through its trait, and the self type is then inferred. pyo3 0.29's `#[pymethods]`
//! writes `let mut holder_0 = ::pyo3::impl_::extract_argument::FunctionArgumentHolder::INIT;`
//! (`trait FunctionArgumentHolder: Sized { const INIT: Self; }`, impls for `()` and
//! `Option<T>`); the later `&mut holder_0` argument fixes `Self`. turso c1851p: 62 errors
//! "cannot find value `INIT` in the trait FunctionArgumentHolder (segment 5) (E0425)".
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait H { const INIT: Self; }
//! impl H for u8 { const INIT: Self = ..; }
//! fn g() -> u8 { H::INIT }     // clean: Self = u8 from the return type
//! fn h() -> u16 { H::INIT }    // E0277: `u16: H` (the negative twin rustc refuses too)
//! fn m() -> u8 { H::MISSING }  // E0425 after the trait (control: suite/path_miss.rs)
//! ```
//!
//! CONTROL: with the `Def::Trait(tr)` arm of `Machine::value_path` taken out, `g` is E0425
//! "cannot find value `INIT` in the trait H" and the first test fails.

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// The world above: `[g, h, m]`.
fn world() -> (Fx, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let self_ = fx.param(0);
    let h = fx.tr("H", 1, Vec::new(), &[]);
    fx.trait_const(h, "INIT", self_);
    let h_u8 = fx.imp("impl H for u8", 0, u8_, Some((h, &[])), &[], Vec::new());
    fx.impl_const(h_u8, "INIT", u8_);
    let g = fx.func("g", 0, &[], u8_, Vec::new());
    let hh = fx.func("h", 0, &[], u16_, Vec::new());
    let m = fx.func("m", 0, &[], u8_, Vec::new());
    fx.finish();
    (fx, [g, hh, m])
}

/// The codes of `owner`'s body `<segments>`.
fn codes(fx: &Fx, owner: ItemKey, segments: &[&str]) -> Vec<&'static str> {
    let mut b = Bb::new();
    let p = b.path(segments);
    let root = b.block(&[], Some(p));
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code.code()).collect()
}

/// Prints nothing; fails with "H::INIT in g: [..]" when the path is refused.
#[test]
fn a_trait_constant_with_an_inferred_self_type_checks() {
    let (fx, [g, _, _]) = world();
    let got = codes(&fx, g, &["H", "INIT"]);
    assert!(got.is_empty(), "H::INIT in g: {got:?}");
}

/// The twin: `Self` is fixed to `u16`, which has no impl of `H`: E0277, never clean.
#[test]
fn a_trait_constant_at_a_type_with_no_impl_is_e0277() {
    let (fx, [_, h, _]) = world();
    let got = codes(&fx, h, &["H", "INIT"]);
    assert!(got.contains(&"E0277"), "H::INIT in h: {got:?}");
}

/// A name the trait does not declare stays E0425 after the trait.
#[test]
fn a_name_the_trait_does_not_declare_is_e0425() {
    let (fx, [_, _, m]) = world();
    let got = codes(&fx, m, &["H", "MISSING"]);
    assert_eq!(got, ["E0425"], "H::MISSING in m");
}
