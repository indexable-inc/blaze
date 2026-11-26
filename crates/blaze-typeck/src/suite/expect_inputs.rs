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
use crate::diag::{DiagCode, Node};
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

/// World for trait-method return deref coercion under an expected `&str`
/// (`blaze-source/src/interning.rs:37`, `InternedString { inner: strings.get(at) }`):
///
/// ```text
/// #[lang = "sized"] trait Sized {}
/// #[lang = "deref"] trait Deref { type Target: ?Sized; }
/// struct Box<T: ?Sized>;
/// impl<T: ?Sized> Deref for Box<T> { type Target = T; }
/// struct Idx<T>;
/// trait Interner<T: Sized> { fn get(&self, idx: Idx<T>) -> &T; }
/// struct Sharded<T>;
/// impl<T> Interner<T> for Sharded<T> { fn get(&self, idx: Idx<T>) -> &T; }
/// struct Multi;
/// impl Interner<Box<str>> for Multi { fn get(&self, idx: Idx<Box<str>>) -> &Box<str>; }
/// impl Interner<u8> for Multi { fn get(&self, idx: Idx<u8>) -> &u8; }
/// ```
///
/// Returns `(fx, [sharded_ok, multi_ok, sharded_wrong_idx, sharded_no_deref])`.
fn interner_world() -> (Fx, [ItemKey; 4]) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);

    let (u8_, p0, p1) = (fx.u8(), fx.param(0), fx.param(1));
    let str_ = fx.ty(PNode::Str);
    let r_str = fx.r(Mutbl::Not, str_);
    let r_u8 = fx.r(Mutbl::Not, u8_);

    let box_k = fx.strukt("Box", 1, &[]);
    let box_p0 = fx.adt(box_k, &[p0]);
    let box_deref = fx.imp("impl<T: ?Sized> Deref for Box<T>", 1, box_p0, Some((deref, &[])), &[("Target", p0)], Vec::new());
    fx.unsized_param(box_deref, 0);
    let box_str = fx.adt(box_k, &[str_]);
    let r_box_str = fx.r(Mutbl::Not, box_str);

    let idx_k = fx.strukt("Idx", 1, &[]);
    let idx_p0 = fx.adt(idx_k, &[p0]);
    let idx_p1 = fx.adt(idx_k, &[p1]);
    let idx_box_str = fx.adt(idx_k, &[box_str]);
    let idx_u8 = fx.adt(idx_k, &[u8_]);

    let interner = fx.tr("Interner", 2, Vec::new(), &[]);
    let r_self = fx.r(Mutbl::Not, p0);
    let r_t = fx.r(Mutbl::Not, p1);
    let t_sized = fx.bound(sized, &[p1]);
    fx.trait_fn_own(interner, "get", true, 0, &[r_self, idx_p1], r_t, std::vec![t_sized]);
    fx.use_trait(interner);

    let sharded_k = fx.strukt("Sharded", 1, &[]);
    let sharded_p0 = fx.adt(sharded_k, &[p0]);
    let r_sharded_p0 = fx.r(Mutbl::Not, sharded_p0);
    let r_p0 = fx.r(Mutbl::Not, p0);
    let imp_sharded = fx.imp("impl<T> Interner<T> for Sharded<T>", 1, sharded_p0, Some((interner, &[p0])), &[], Vec::new());
    fx.impl_fn(imp_sharded, "get", true, &[r_sharded_p0, idx_p0], r_p0);

    let multi_k = fx.strukt("Multi", 0, &[]);
    let multi_ty = fx.adt(multi_k, &[]);
    let r_multi = fx.r(Mutbl::Not, multi_ty);
    let imp_multi_str = fx.imp("impl Interner<Box<str>> for Multi", 0, multi_ty, Some((interner, &[box_str])), &[], Vec::new());
    fx.impl_fn(imp_multi_str, "get", true, &[r_multi, idx_box_str], r_box_str);
    let imp_multi_u8 = fx.imp("impl Interner<u8> for Multi", 0, multi_ty, Some((interner, &[u8_])), &[], Vec::new());
    fx.impl_fn(imp_multi_u8, "get", true, &[r_multi, idx_u8], r_u8);

    let sharded_box_str = fx.adt(sharded_k, &[box_str]);
    let r_sharded_box_str = fx.r(Mutbl::Not, sharded_box_str);
    let sharded_u8 = fx.adt(sharded_k, &[u8_]);
    let r_sharded_u8 = fx.r(Mutbl::Not, sharded_u8);

    let sharded_ok = fx.func("sharded_ok", 0, &[r_sharded_box_str, idx_box_str], r_str, Vec::new());
    let multi_ok = fx.func("multi_ok", 0, &[r_multi, idx_box_str], r_str, Vec::new());
    let sharded_wrong_idx = fx.func("sharded_wrong_idx", 0, &[r_sharded_box_str, idx_u8], r_str, Vec::new());
    let sharded_no_deref = fx.func("sharded_no_deref", 0, &[r_sharded_u8, idx_u8], r_str, Vec::new());
    fx.finish();
    (world, [sharded_ok, multi_ok, sharded_wrong_idx, sharded_no_deref])
}

/// `strings.get(at)` on `&Sharded<Box<str>>` returning `&str`: the unique impl
/// `impl<T> Interner<T> for Sharded<T>` resolves `?T = Box<str>` before `expected_inputs` probes
/// `&?T` against `&str`, so `strings.get(at)` returns `&Box<str>` and deref-coerces to `&str`.
#[test]
fn trait_method_output_resolves_obligations_before_expected_inputs_probe() {
    let (fx, [sharded_ok, ..]) = interner_world();
    let mut b = Bb::new();
    let strings = b.param("strings");
    let at = b.param("at");
    let recv = b.local(strings);
    let arg = b.local(at);
    let call = b.method(recv, "get", &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, sharded_ok, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "sharded_ok: {:?}", c.diags);
}

/// `m.get(at)` on `&Multi` (implementing both `Interner<Box<str>>` and `Interner<u8>`, neither
/// for `str`) returning `&str`: probing `?T = str` violates `Multi: Interner<str>` and
/// `str: Sized`, so `expected_inputs` does not bind `?T = str`; `at` infers `?T = Box<str>` and
/// `&Box<str>` deref-coerces to `&str`.
#[test]
fn multi_impl_trait_method_does_not_bind_unsatisfied_expected_output() {
    let (fx, [_, multi_ok, ..]) = interner_world();
    let mut b = Bb::new();
    let m = b.param("m");
    let at = b.param("at");
    let recv = b.local(m);
    let arg = b.local(at);
    let call = b.method(recv, "get", &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, multi_ok, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "multi_ok: {:?}", c.diags);
}

/// Negative control: `strings.get(bad_at)` on `&Sharded<Box<str>>` with `bad_at: Idx<u8>` fails
/// once with E0308 at `bad_at` (`Idx<u8>` vs `Idx<Box<str>>`), with no bogus E0277 for `str`.
#[test]
fn trait_method_wrong_idx_reports_mismatch_without_str_unsatisfied() {
    let (fx, [_, _, sharded_wrong_idx, _]) = interner_world();
    let mut b = Bb::new();
    let strings = b.param("strings");
    let at = b.param("at");
    let recv = b.local(strings);
    let arg = b.local(at);
    let call = b.method(recv, "get", &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, sharded_wrong_idx, root, Plant::None, InferPlant::None);
    let got: Vec<_> = c.diags.iter().map(|d| (d.code, d.node)).collect();
    assert_eq!(got, std::vec![(DiagCode::Mismatch, Some(Node::Expr(arg)))], "sharded_wrong_idx: {:?}", c.diags);
}

/// Negative control: `strings.get(at)` on `&Sharded<u8>` returning `&str` still fails with E0308
/// at the call because `&u8` does not deref-coerce to `&str`.
#[test]
fn trait_method_output_without_deref_to_expected_ref_is_mismatch() {
    let (fx, [_, _, _, sharded_no_deref]) = interner_world();
    let mut b = Bb::new();
    let strings = b.param("strings");
    let at = b.param("at");
    let recv = b.local(strings);
    let arg = b.local(at);
    let call = b.method(recv, "get", &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, sharded_no_deref, root, Plant::None, InferPlant::None);
    let got: Vec<_> = c.diags.iter().map(|d| (d.code, d.node)).collect();
    assert_eq!(got, std::vec![(DiagCode::Mismatch, Some(Node::Expr(call)))], "sharded_no_deref: {:?}", c.diags);
}
