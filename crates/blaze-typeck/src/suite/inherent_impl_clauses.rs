//! A method probe drops an inherent candidate whose impl's where-clause cannot hold, and goes on
//! to the next candidate, autoref or step (rules/method.rs `impl_clauses_may_hold`). The rule
//! is rustc_hir_typeck method/probe.rs `consider_probe`, `InherentImplCandidate`: "Check whether
//! the impl imposes obligations we have to worry about", with the impl's `clauses_of`
//! registered and any error a `ProbeResult::NoMatch`. The Reference, "Method call
//! expressions", says the candidates are tried by-value, then by `&` and `&mut` at each step,
//! inherent before trait, and the first that applies is the pick.
//!
//! Two crates turso reaches use this "autoref specialization" (turso c1851p):
//! - miette 7.6 macro_helpers.rs has `impl<T: IsOption> OptionalWrapper<T> { fn to_option(self,
//!   ..) }` beside `impl<T> ToOption for &OptionalWrapper<T>`. Before the fix it gave
//!   "`SourceSpan: IsOption` (E0277)" at the inherent pick.
//! - pyo3 0.29 impl_/wrap.rs has `impl<T: IntoPyObject> IntoPyObjectConverter<T> { fn wrap }`
//!   beside `impl<T: IntoPyObject, E> IntoPyObjectConverter<Result<T, E>> { fn wrap }`. Before
//!   the fix it gave "found `Converter<Result<(), PyErr>>`, about `wrap` (E0034)" 22 times.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait IsOpt {}                       impl IsOpt for u16 {}
//! struct W<T>;                         impl<T: IsOpt> W<T> { fn pick(self, v: u8) -> u16 }
//! trait ToOpt { fn pick(self, v: u8) -> u8; }   impl<T> ToOpt for &W<T> { .. }
//! struct C<T>;                         impl<T: IsOpt> C<T> { fn wrap(&self) -> u8 }
//!                                      impl C<u8> { fn wrap(&self) -> u16 }
//! fn g(w: W<u8>, v: u8) -> u8 { w.pick(v) }     // clean: the trait's, at `&W<u8>` (miette)
//! fn h(w: W<u16>, v: u8) -> u16 { w.pick(v) }   // CONTROL, clean: the inherent one, by value
//! fn k(w: W<u8>, v: u8) -> u16 { w.pick(v) }    // the twin: E0308, the trait's returns u8
//! fn p(c: C<u8>) -> u16 { c.wrap() }            // clean: only `impl C<u8>` applies (pyo3)
//! fn q(c: C<usize>) -> u16 { c.wrap() }         // the twin: E0599, neither impl applies
//! ```
//! rustc takes this program (coherence takes the two impls of `C` as disjoint, since `u8:
//! IsOpt` does not hold), and the expectations above are what it gives.
//!
//! CONTROL (rule 23): with the `else if !self.impl_clauses_may_hold(..)` branch taken out of
//! `instantiate_method`, `g` is E0277 (`u8: IsOpt` at the inherent pick) and `p` is E0034.

use std::vec::Vec;

use super::check_body;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// The world above: `[g, h, k, p, q]`.
fn world() -> (Fx, [ItemKey; 5]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, u8_, u16_, usize_) = (fx.param(0), fx.u8(), fx.u16(), fx.usize());
    let is_opt = fx.tr("IsOpt", 1, Vec::new(), &[]);
    fx.imp("impl IsOpt for u16", 0, u16_, Some((is_opt, &[])), &[], Vec::new());

    let w = fx.strukt("W", 1, &[]);
    let w_t = fx.adt(w, &[p0]);
    let t_is_opt = fx.bound(is_opt, &[p0]);
    let w_inh = fx.imp("impl<T: IsOpt> W<T>", 1, w_t, None, &[], Vec::from([t_is_opt]));
    fx.impl_fn(w_inh, "pick", true, &[w_t, u8_], u16_);
    let to_opt = fx.tr("ToOpt", 1, Vec::new(), &[]);
    fx.trait_fn(to_opt, "pick", true, &[p0, u8_], u8_);
    fx.use_trait(to_opt);
    let r_w_t = fx.r(Mutbl::Not, w_t);
    let w_tr = fx.imp("impl<T> ToOpt for &W<T>", 1, r_w_t, Some((to_opt, &[])), &[], Vec::new());
    fx.impl_fn(w_tr, "pick", true, &[r_w_t, u8_], u8_);

    let c = fx.strukt("C", 1, &[]);
    let c_t = fx.adt(c, &[p0]);
    let r_c_t = fx.r(Mutbl::Not, c_t);
    let c_any = fx.imp("impl<T: IsOpt> C<T>", 1, c_t, None, &[], Vec::from([t_is_opt]));
    fx.impl_fn(c_any, "wrap", true, &[r_c_t], u8_);
    let c_u8 = fx.adt(c, &[u8_]);
    let r_c_u8 = fx.r(Mutbl::Not, c_u8);
    let c_one = fx.imp("impl C<u8>", 0, c_u8, None, &[], Vec::new());
    fx.impl_fn(c_one, "wrap", true, &[r_c_u8], u16_);

    let (w_u8, w_u16, c_usize) = (fx.adt(w, &[u8_]), fx.adt(w, &[u16_]), fx.adt(c, &[usize_]));
    let g = fx.func("g", 0, &[w_u8, u8_], u8_, Vec::new());
    let h = fx.func("h", 0, &[w_u16, u8_], u16_, Vec::new());
    let k = fx.func("k", 0, &[w_u8, u8_], u16_, Vec::new());
    let p = fx.func("p", 0, &[c_u8], u16_, Vec::new());
    let q = fx.func("q", 0, &[c_usize], u16_, Vec::new());
    fx.finish();
    (fx, [g, h, k, p, q])
}

/// The codes of `owner`'s body `recv.name(v?)`, the receiver its first parameter and `v` its
/// second when `with_arg`.
fn codes(fx: &Fx, owner: ItemKey, name: &str, with_arg: bool) -> Vec<&'static str> {
    let mut b = Bb::new();
    let recv = b.param("recv");
    let recv_use = b.local(recv);
    let call = if with_arg {
        let v = b.param("v");
        let v_use = b.local(v);
        b.method(recv_use, name, &[v_use])
    } else {
        b.method(recv_use, name, &[])
    };
    let root = b.block(&[], Some(call));
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code.code()).collect()
}

/// miette's shape: the inherent `pick` needs `u8: IsOpt`, so the trait's at the autoref is the
/// pick. Prints nothing; fails with "g: [..]" (E0277 before the fix).
#[test]
fn an_inherent_method_whose_impl_clause_fails_yields_to_the_autoref_trait_method() {
    let (fx, [g, _, _, _, _]) = world();
    let got = codes(&fx, g, "pick", true);
    assert!(got.is_empty(), "g: {got:?}");
}

/// CONTROL: where the clause holds the inherent method is still the by-value pick.
#[test]
fn control_an_inherent_method_whose_impl_clause_holds_is_picked_first() {
    let (fx, [_, h, _, _, _]) = world();
    let got = codes(&fx, h, "pick", true);
    assert!(got.is_empty(), "h: {got:?}");
}

/// The twin: the trait's method returns `u8`, so a `u16` return is E0308 (and proves the
/// trait's was the pick, not the inherent one).
#[test]
fn the_trait_pick_returns_its_own_type() {
    let (fx, [_, _, k, _, _]) = world();
    let got = codes(&fx, k, "pick", true);
    assert_eq!(got, ["E0308"], "k");
}

/// pyo3's shape: two inherent impls at one step, one whose clause fails. Prints nothing; fails
/// with "p: [..]" (E0034 before the fix).
#[test]
fn of_two_inherent_impls_at_one_step_the_one_whose_clauses_hold_is_picked() {
    let (fx, [_, _, _, p, _]) = world();
    let got = codes(&fx, p, "wrap", false);
    assert!(got.is_empty(), "p: {got:?}");
}

/// The unknown twin (D193 point 5): an inherent impl whose where-clause the world cannot answer
/// (a `where const` condition with no value, so a `Stop::Need`) beside the right candidate.
/// ```text
/// struct D<T>;
/// impl<T> D<T> where const { ?? } { fn get(&self) -> u8 }   // its clause: unknown
/// impl D<u8>  { fn get(self) -> u16 }
/// impl D<u16> { fn get(&self) -> u16 }
/// fn r(d: D<u8>) -> u16 { d.get() }    // clean: `impl D<u8>`'s, by value; the unknown one's
///                                      //   `&self` does not match `D<u8>`, so it is never asked
/// fn s(d: D<u16>) -> u16 { d.get() }   // at `&D<u16>`, `impl D<u16>`'s and the unknown one:
///                                      //   "not checked" (Outcome::Need), never E0034 or a drop
/// ```
fn unknown_world() -> (Fx, [ItemKey; 2]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, u8_, u16_) = (fx.param(0), fx.u8(), fx.u16());
    let d = fx.strukt("D", 1, &[]);
    let d_t = fx.adt(d, &[p0]);
    let r_d_t = fx.r(Mutbl::Not, d_t);
    // No `fx.condition` names this key, so the world's `const_holds` answers `Missing`.
    let unanswered = fx.holds(ItemKey(u128::MAX), &[p0]);
    let d_any = fx.imp("impl<T> D<T> where const { ?? }", 1, d_t, None, &[], Vec::from([unanswered]));
    fx.impl_fn(d_any, "get", true, &[r_d_t], u8_);
    let (d_u8, d_u16) = (fx.adt(d, &[u8_]), fx.adt(d, &[u16_]));
    let d_one = fx.imp("impl D<u8>", 0, d_u8, None, &[], Vec::new());
    fx.impl_fn(d_one, "get", true, &[d_u8], u16_);
    let r_d_u16 = fx.r(Mutbl::Not, d_u16);
    let d_two = fx.imp("impl D<u16>", 0, d_u16, None, &[], Vec::new());
    fx.impl_fn(d_two, "get", true, &[r_d_u16], u16_);
    let r = fx.func("r", 0, &[d_u8], u16_, Vec::new());
    let s = fx.func("s", 0, &[d_u16], u16_, Vec::new());
    fx.finish();
    (fx, [r, s])
}

/// The body `recv.get()`, `recv` the owner's first parameter.
fn get_body() -> (Bb, crate::ids::ExprId) {
    let mut b = Bb::new();
    let recv = b.param("recv");
    let recv_use = b.local(recv);
    let call = b.method(recv_use, "get", &[]);
    let root = b.block(&[], Some(call));
    (b, root)
}

/// The right candidate is chosen beside one whose clause is unknown. Prints nothing; fails with
/// "r: [..]" when the unknown one is taken for applying (E0034) or stops the body.
#[test]
fn the_right_candidate_beside_an_unknown_one_is_still_chosen() {
    let (fx, [r, _]) = unknown_world();
    let (b, root) = get_body();
    let c = check_body(&fx, &b, r, root, Plant::None, InferPlant::None);
    let got: Vec<&str> = c.diags.iter().map(|d| d.code.code()).collect();
    assert!(got.is_empty(), "r: {got:?}");
}

/// An unknown candidate in the picking group leaves the body not checked: an `Outcome::Need`
/// naming the question, never a finding (E0034) and never a check that dropped it.
#[test]
fn an_unknown_candidate_in_the_picking_group_leaves_the_body_not_checked() {
    use blaze_own::Alloc;
    let (fx, [_, s]) = unknown_world();
    let (b, root) = get_body();
    let mut cx = crate::infer::InferCtx::new_in(Alloc::global());
    let needs = crate::world::Needs::new_in(Alloc::global());
    match crate::check_with(&b.view(s, root), &fx, &mut cx, &needs, Plant::None, Alloc::global()) {
        Ok(crate::Outcome::Need(_)) => {}
        Ok(crate::Outcome::Done(c)) => panic!("s: checked, with {:?}", c.diags.iter().map(|d| d.code.code()).collect::<Vec<_>>()),
        Err(e) => panic!("s: {e:?}"),
    }
}

/// The twin: neither impl of `C` applies to `C<usize>`: E0599, never a pick.
#[test]
fn with_no_inherent_impl_whose_clauses_hold_the_method_is_not_found() {
    let (fx, [_, _, _, _, q]) = world();
    let got = codes(&fx, q, "wrap", false);
    assert_eq!(got, ["E0599"], "q");
}
