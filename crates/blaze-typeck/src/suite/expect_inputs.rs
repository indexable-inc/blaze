//! A call's expected type reaches its arguments (rules/expect_inputs.rs). tempo's "33 diag
//! E0308" (alloy's `sol!` writes, in `impl<P: Clone, N> XInstance<&P, N>`,
//! `fn with_cloned_provider(self) -> XInstance<P, N> { XInstance { provider:
//! ::core::clone::Clone::clone(&self.provider), .. } }`), reduced:
//!
//! ```text
//! trait Clone { fn clone(&self) -> Self; }          // library/core/src/clone.rs
//! impl Clone for u8 {}   impl Clone for u16 {}
//! enum Opt<T> { Some(T), None }
//! fn with_cloned<P: Clone>(p: &P) -> Opt<P> { Opt::Some(Clone::clone(&p)) }   // clean
//! fn direct<P: Clone>(p: P) { let x: P = Clone::clone(&&p); }                  // clean
//! fn wrong_self() { let x: u8 = Clone::clone(&0u16); }                         // E0308
//! fn takes(x: &u16) {}
//! fn wrong_arg() { takes(&0u8) }                                               // E0308
//! ```
//!
//! The fixture's body builder writes no struct literal, so the field of the generated code is
//! the one field of `Opt::Some`, and `self.provider` (a `&P`) is the parameter `p: &P`: in both
//! the call's expected type is `P`, given by the place the call's value goes, and its argument
//! is a `&&P`.
//!
//! The Reference, "Type coercions", "Coercion sites": "Arguments for function calls: The value
//! being coerced is the actual parameter, and it is coerced to the type of the formal
//! parameter." and "Coercion types": "`&T` or `&mut T` to `&U` if `T` implements
//! `Deref<Target = U>`". With `Self = P` from the expected type, the formal parameter is `&P`
//! and `&&P` coerces to it. No coercion listed there takes `&u16` to `&u8` or `&u8` to `&u16`.
//!
//! Each test prints `test suite::expect_inputs::<name> ... ok`.

use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

/// `with_cloned`, `direct`, `wrong_self`, `wrong_arg`.
fn world() -> (Fx, [ItemKey; 4]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (u8_, u16_, unit, p0) = (fx.u8(), fx.u16(), fx.unit(), fx.param(0));
    let clone = fx.tr("Clone", 1, Vec::new(), &[]);
    let r_self = fx.r(Mutbl::Not, p0);
    let clone_fn = fx.trait_fn(clone, "clone", true, &[r_self], p0);
    // The whole path names the trait's method, as the front end answers
    // `::core::clone::Clone::clone` on tempo (`Def::Fn`, every segment consumed: the checker
    // then makes `Self` a fresh variable, `value_def_ty`). A path the world answers as
    // `Def::Trait` with one segment left has no rule in `value_path` yet and is not this test's.
    fx.use_path("clone", Def::Fn(clone_fn));
    let r_u8 = fx.r(Mutbl::Not, u8_);
    let r_u16 = fx.r(Mutbl::Not, u16_);
    let for_u8 = fx.imp("impl Clone for u8", 0, u8_, Some((clone, &[])), &[], Vec::new());
    fx.impl_fn(for_u8, "clone", true, &[r_u8], u8_);
    let for_u16 = fx.imp("impl Clone for u16", 0, u16_, Some((clone, &[])), &[], Vec::new());
    fx.impl_fn(for_u16, "clone", true, &[r_u16], u16_);
    let (opt, _) = fx.enum_("Opt", 1, &[("Some", &[p0]), ("None", &[])]);
    let opt_p = fx.adt(opt, &[p0]);
    let p_clone = fx.bound(clone, &[p0]);
    let with_cloned = fx.func("with_cloned", 1, &[r_self], opt_p, std::vec![p_clone]);
    let p_clone = fx.bound(clone, &[p0]);
    let direct = fx.func("direct", 1, &[p0], unit, std::vec![p_clone]);
    let wrong_self = fx.func("wrong_self", 0, &[], unit, Vec::new());
    fx.func("takes", 0, &[r_u16], unit, Vec::new());
    let wrong_arg = fx.func("wrong_arg", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, [with_cloned, direct, wrong_self, wrong_arg])
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// `Opt::Some(Clone::clone(&p))` with `p: &P`, returned as `Opt<P>`: `Opt<?T>` is the expected
/// `Opt<P>`, so `Some`'s input is `P`; that is `clone`'s expected type, so `Self = P`, its
/// input is `&P`, and `&p` (`&&P`) coerces to it. Without the expected type reaching the
/// arguments: `Self = &P` from the argument and one E0308 (found `&P`, expected `P`).
#[test]
fn a_calls_expected_type_decides_self_before_its_argument() {
    let (fx, [with_cloned, ..]) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let some = b.path(&["Opt", "Some"]);
    let clone = b.path(&["clone"]);
    let local = b.local(p);
    let arg = b.borrow(Mutbl::Not, local);
    let cloned = b.call(clone, &[arg]);
    let made = b.call(some, &[cloned]);
    let root = b.block(&[], Some(made));
    let c = check_body(&fx, &b, with_cloned, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `let x: P = Clone::clone(&&p);` with `p: P`: the annotation is the call's expected type,
/// `Self = P`, and `&&P` coerces to `&P`.
#[test]
fn a_lets_type_decides_self_before_the_argument() {
    let (fx, [_, direct, ..]) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let clone = b.path(&["clone"]);
    let local = b.local(p);
    let inner = b.borrow(Mutbl::Not, local);
    let arg = b.borrow(Mutbl::Not, inner);
    let cloned = b.call(clone, &[arg]);
    let x = b.bind("x");
    let p_ty = b.ty(PNode::Param(0));
    let stmt = b.let_(x, Some(p_ty), cloned);
    let root = b.block(&[stmt], None);
    let c = check_body(&fx, &b, direct, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `let x: u8 = Clone::clone(&0u16);`: `Self = u8` from the annotation, the input is `&u8`,
/// and `&0u16` is a `&u16`: refused once, at the argument (no coercion takes `&u16` to `&u8`).
/// Without the expected type reaching the arguments the one E0308 is at the call instead.
#[test]
fn a_wrong_argument_under_an_expected_type_is_refused_at_the_argument() {
    let (fx, [_, _, wrong_self, _]) = world();
    let mut b = Bb::new();
    let clone = b.path(&["clone"]);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U16)));
    let arg = b.borrow(Mutbl::Not, zero);
    let cloned = b.call(clone, &[arg]);
    let x = b.bind("x");
    let u8_ty = b.ty(PNode::Uint(UintTy::U8));
    let stmt = b.let_(x, Some(u8_ty), cloned);
    let root = b.block(&[stmt], None);
    let c = check_body(&fx, &b, wrong_self, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::Mismatch, arg)] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `takes(&0u8)` where `takes` wants a `&u16`: an argument is a coercion site, and Rust has no
/// coercion from `&u8` to `&u16`: E0308 at the argument.
#[test]
fn an_argument_needing_a_coercion_rust_lacks_is_refused() {
    let (fx, [_, _, _, wrong_arg]) = world();
    let mut b = Bb::new();
    let takes = b.path(&["takes"]);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let arg = b.borrow(Mutbl::Not, zero);
    let call = b.call(takes, &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, wrong_arg, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::Mismatch, arg)] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}
