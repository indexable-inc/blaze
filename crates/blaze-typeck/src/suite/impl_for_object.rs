//! An impl whose self type is a trait object type (x0-dyn-04, 2026-11-16).
//!
//! ```text
//! trait Tr { fn m(&self) -> u32; }
//! trait Other { fn o(&self) -> u32; }      // in scope
//! impl Other for dyn Tr { .. }             // left out by the control
//! fn need<T: ?Sized + Other>(x: &T) -> u32;
//! fn by_method(t: &dyn Tr) -> u32 { t.o() }
//! fn by_bound(t: &dyn Tr) -> u32 { need(t) }
//! ```
//!
//! The Reference, "Implementations": "An implementation is an item that associates items with
//! an implementing type"; "Trait object types": `dyn Tr` is a type, so `impl Other for dyn Tr`
//! implements `Other` for it and `dyn Tr: Other` holds by that impl. "Method call expressions":
//! for each candidate type `T` (here `&dyn Tr`, then `dyn Tr` by dereference), "T's inherent
//! methods (methods implemented directly on T)", then "any of the methods provided by a visible
//! trait implemented by T"; for an object the "inherent" ones are its principal's and
//! supertraits' methods ("Trait objects": "the trait object implements the base trait ...
//! and any supertraits of the base trait"). `o` is none of `Tr`'s: it is found at the second
//! group, as `Other`'s method, `Other` being in scope and implemented by `dyn Tr` through the
//! impl. So `t.o()` checks, and so does `need(t)` (`T = dyn Tr`, its bound met by the impl).
//!
//! The bug fixed with this test: the solver asked the impl table for `Other`'s impls at
//! `Dynamic(0)` (`shape_of_kind` cannot read an object's principal) while the table filed the
//! impl under `Dynamic(Tr)`: blaze-solve object_shape.rs. Without that fix,
//! `impl_for_an_object_is_found` fails with
//! "by_method: want [], got [(NoMethod, 1)]" and "by_bound: want [], got [(Unsatisfied, 1)]".

use std::format;
use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{List, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The world of the module doc; `impl Other for dyn Tr` only `with_impl`.
fn world(with_impl: bool) -> Fx {
    let mut fx = Fx::new();
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let u32_ = fx.u32();
    fx.trait_fn(tr, "m", true, &[r_self], u32_);
    let other = fx.tr("Other", 1, Vec::new(), &[]);
    fx.trait_fn(other, "o", true, &[r_self], u32_);
    fx.use_trait(other);
    let ex_tr = fx.ty(PNode::ExTrait(tr, List::EMPTY));
    let dyn_tr = fx.t.dynamic(&[ex_tr]).expect("dyn");
    if with_impl {
        fx.imp("other_for_dyn_tr", 0, dyn_tr, Some((other, &[])), &[], Vec::new());
    }
    // No `Sized` lang item in this world, so `need`'s `T` is `?Sized` as written.
    let t_other = fx.bound(other, &[p0]);
    fx.func("need", 1, &[r_self], u32_, std::vec![t_other]);
    let r_dyn = fx.r(Mutbl::Not, dyn_tr);
    fx.func("by_method", 0, &[r_dyn], u32_, Vec::new());
    fx.func("by_bound", 0, &[r_dyn], u32_, Vec::new());
    fx.finish();
    fx
}

/// Each body's diagnostics, `(code, expression)` sorted, against `want`: one line per body
/// that differs. Expression ids: by_method `t` 0, `t.o()` 1, block 2; by_bound `need` 0,
/// `t` 1, `need(t)` 2, block 3.
fn diag_differences(with_impl: bool, want_method: &[(DiagCode, u32)], want_bound: &[(DiagCode, u32)]) -> Vec<String> {
    let fx = world(with_impl);
    let mut out = Vec::new();
    let mut b = Bb::new();
    let t = b.param("t");
    let e0 = b.local(t);
    let e1 = b.method(e0, "o", &[]);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, fx.key_of("by_method"), root, Plant::None, InferPlant::None);
    let mut got: Vec<(DiagCode, u32)> = c.diags.iter().map(|d| (d.code, d.at.start)).collect();
    got.sort();
    if got != want_method {
        out.push(format!("by_method: want {want_method:?}, got {got:?}"));
    }
    let mut b = Bb::new();
    let t = b.param("t");
    let f = b.path(&["need"]);
    let e1 = b.local(t);
    let call = b.call(f, &[e1]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, fx.key_of("by_bound"), root, Plant::None, InferPlant::None);
    let mut got: Vec<(DiagCode, u32)> = c.diags.iter().map(|d| (d.code, d.at.start)).collect();
    got.sort();
    if got != want_bound {
        out.push(format!("by_bound: want {want_bound:?}, got {got:?}"));
    }
    out
}

/// `t.o()` and `need(t)` on `t: &dyn Tr` check with `impl Other for dyn Tr` (module doc).
/// Prints nothing; fails, before blaze-solve object_shape.rs, with the two lines the module
/// doc names.
#[test]
fn impl_for_an_object_is_found() {
    let got = diag_differences(true, &[], &[]);
    assert!(got.is_empty(), "\n{}", got.join("\n"));
}

/// The control (the cause removed: no `impl Other for dyn Tr`): `t.o()` finds no method
/// (E0599) and `need(t)` has an unmet bound (E0277), so the test above is about the impl.
/// Prints "without the impl: by_method [(NoMethod, 1)], by_bound [(Unsatisfied, 1)]".
#[test]
fn control_no_impl_for_the_object() {
    // The bound a callee's where-clauses owe is filed at the argument passed for the input that
    // names it (`t` of `need(t)`, 1: rules/obligation_site.rs).
    let got = diag_differences(false, &[(DiagCode::NoMethod, 1)], &[(DiagCode::Unsatisfied, 1)]);
    std::println!("without the impl: by_method [(NoMethod, 1)], by_bound [(Unsatisfied, 1)]");
    assert!(got.is_empty(), "\n{}", got.join("\n"));
}
