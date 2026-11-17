//! `Self::NAME` in an impl stands at the impl's own arguments, never at fresh ones: the hello
//! world's left-out `Ctrl::live` (crates/blaze-swiss `impl<const SLOTS: usize> Ctrl<SLOTS>`
//! reads `Self::HIGH`, `Self::SEVEN`), reduced:
//!
//! ```text
//! struct C<const N: usize>;
//! impl<const N: usize> C<N> { const H: u64 = 1; fn f() -> u64 { Self::H } }   // clean
//! impl<const N: usize> C<N> { fn k() -> u64 { C::H } }                        // E0282
//! struct W<T>;
//! impl<T> W<T> { const Z: usize = 0; fn g() -> usize { Self::Z } }            // clean
//! ```
//!
//! The rules:
//! - The Reference, "Paths", "`Self`": "In an implementation, it refers to the type being
//!   implemented." The type implemented by `impl<const N: usize> C<N>` is `C<N>` at the impl's
//!   own `N`; so `Self::H` is `<C<N>>::H`, with nothing left to infer.
//! - The Reference, "Paths", "Paths in expressions": generic arguments a path to a generic
//!   item does not write are inferred ("type annotations needed" when nothing fixes them).
//!   `C::H` writes none, and the type of `H` (`u64`) does not mention `N`: nothing fixes it,
//!   which is E0282. The written name keeps that rule; only the segment `Self` takes the
//!   impl's arguments (rules/self_path.rs `impl_self_adt_args`).
//! - The Reference, "Special types and traits", "`Sized`": "Type parameters (except `Self` in
//!   traits) are `Sized` by default". A const parameter is no type parameter and has no
//!   implied `Sized`: the impl above has NO predicate on `N` (the front end, blaze-ladder
//!   front2/preds.rs, skips const parameters: "A const parameter ... is never `Sized`"). This
//!   world has the `Sized` lang item, as the hello world does, so a world (or fixture) that
//!   gave `N` the implied `Sized` asks `Sized` of the const argument `N` and the solver
//!   refuses that goal by name (`Malformed(BadPred { kind: Trait, args: [Some(Const), ..] })`):
//!   the first writer's test failed exactly so (2026-11-16), from `Fx::const_param` leaving the
//!   implied clause `Fx::imp` had made before it knew `N` is a const.
//!
//! No test here runs a compiler for its answer: each expectation is the rule above.

use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

/// `struct C<const N: usize>; impl<const N: usize> C<N> { const H: u64; fn f; fn k }`, with
/// `Self` and `C` both answered as `C` (the world answers `Self` in an impl of an ADT with the
/// ADT, one segment consumed: blaze-ladder front2/world_paths.rs): `[f, k]`. `N` is declared
/// a const parameter of the impl and of each member: `Fx::const_param` takes its implied
/// `Sized` away from the impl and from the members already made from it.
fn world_c() -> (Fx, [ItemKey; 2]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (u64_, usize_) = (fx.ty(PNode::Uint(UintTy::U64)), fx.usize());
    let n0 = fx.ty(PNode::ConstParam(0));
    let c = fx.strukt("C", 1, &[]);
    fx.const_param(c, 0, usize_);
    let c_n = fx.adt(c, &[n0]);
    let imp = fx.imp("impl<const N: usize> C<N>", 1, c_n, None, &[], Vec::new());
    fx.const_param(imp, 0, usize_);
    let h = fx.impl_const(imp, "H", u64_);
    fx.const_param(h, 0, usize_);
    let f = fx.impl_fn(imp, "f", false, &[], u64_);
    fx.const_param(f, 0, usize_);
    let k = fx.impl_fn(imp, "k", false, &[], u64_);
    fx.const_param(k, 0, usize_);
    fx.use_path("Self", Def::Adt(c));
    fx.use_path("C", Def::Adt(c));
    fx.finish();
    (fx, [f, k])
}

/// `struct W<T>; impl<T> W<T> { const Z: usize; fn g }`, `Self` answered as `W`: `g`. `T` keeps
/// its implied `Sized` (a type parameter), which the solver proves from the where-clause.
fn world_w() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let usize_ = fx.usize();
    let t0 = fx.param(0);
    let w = fx.strukt("W", 1, &[]);
    let w_t = fx.adt(w, &[t0]);
    let imp = fx.imp("impl<T> W<T>", 1, w_t, None, &[], Vec::new());
    fx.impl_const(imp, "Z", usize_);
    let g = fx.impl_fn(imp, "g", false, &[], usize_);
    fx.use_path("Self", Def::Adt(w));
    fx.finish();
    (fx, g)
}

/// The body `{ <segments> }` of `owner`, checked: its differences from `want` (the path is
/// expression 0, the block expression 1), and its diagnostic codes.
fn path_body(fx: &Fx, owner: ItemKey, segments: &[&str], want: &Want<'_>) -> (Vec<String>, Vec<DiagCode>) {
    let mut b = Bb::new();
    let p = b.path(segments);
    let root = b.block(&[], Some(p));
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    let lines: Vec<String> = differences(fx, &b, &c, want).into_iter().filter(|l| !l.contains("no expectation written")).collect();
    (lines, c.diags.iter().map(|d| d.code).collect())
}

/// `Self::H` in `impl<const N: usize> C<N>` and `Self::Z` in `impl<T> W<T>` are clean and typed
/// `u64` / `usize`: `Self` is the impl's self type at the impl's own arguments. Without the fix
/// (`Self` through `explicit_args`, fresh variables nothing fixes) both are E0282 and this
/// fails with "f: unexpected diagnostics: [AnnotationsNeeded]". Without the fixture's rule (a
/// const parameter has no implied `Sized`) `f` stops with "Solve(Malformed(Pred { .. BadPred {
/// kind: Trait, .. args: [Some(Const), ..".  The last case is the control that the written name
/// `C::H` still takes fresh arguments (E0282): a fix that gave every path to the impl's ADT the
/// impl's arguments fails it.
///
/// Prints: `test suite::self_impl_args::self_takes_the_impls_own_arguments ... ok`
#[test]
fn self_takes_the_impls_own_arguments() {
    let (fx, [f, k]) = world_c();
    let want = Want { exprs: &[(ExprId(0), "u64"), (ExprId(1), "u64")], pats: &[], diags: &[] };
    let (d, codes) = path_body(&fx, f, &["Self", "H"], &want);
    assert!(codes.is_empty(), "f: unexpected diagnostics: {codes:?}");
    assert!(d.is_empty(), "f:\n{}", d.join("\n"));

    let (fx_w, g) = world_w();
    let want = Want { exprs: &[(ExprId(0), "usize"), (ExprId(1), "usize")], pats: &[], diags: &[] };
    let (d, codes) = path_body(&fx_w, g, &["Self", "Z"], &want);
    assert!(codes.is_empty(), "g: unexpected diagnostics: {codes:?}");
    assert!(d.is_empty(), "g:\n{}", d.join("\n"));

    let none = Want { exprs: &[], pats: &[], diags: &[] };
    let (_, codes) = path_body(&fx, k, &["C", "H"], &none);
    assert!(codes.contains(&DiagCode::AnnotationsNeeded), "k: want E0282 (AnnotationsNeeded) for `C::H`, got {codes:?}");
}
