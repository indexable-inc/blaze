//! `Self::f(..)` in a generic impl calls `f` at the impl's own arguments: the result is never a
//! type variable. tempo's largest group of false E0282s (census 14), reduced:
//!
//! ```text
//! trait Storable { type Handler; }
//! trait Handler<T> { fn write(self, v: T) -> u8; }
//! struct W<T>;
//! impl<T: Storable> W<T> {
//!     fn ticket(n: u8) -> u8 { n }
//!     fn compute(n: u8) -> T::Handler { .. }
//!     fn make<U>() -> U { .. }
//!     // tempo_consensus gossip/actor.rs:279 and :396 (`Self::issue_ready_ticket(..)`,
//!     // returning `type ReadyTicket = u128`): was E0282 "`?T: Sized`" at the path.
//!     fn on_frame(n: u8) -> u8 { let t = Self::ticket(n); t }                          // clean
//!     // tempo_precompiles storage/types/vec.rs:290 (`push`; `at` :271 and `pop` :324 alike):
//!     // was E0282 at `Self::compute_handler`, then at the method call on its result.
//!     fn push(v: T, n: u8) -> u8 where T::Handler: Handler<T> {
//!         let e = Self::compute(n); e.write(v)                                          // clean
//!     }
//!     fn guess() { let x = Self::make(); }                                              // E0282
//! }
//! ```
//!
//! The cause (census 14 ran at 16:09, before the 17:22 snapshot d947642cb0): the world answers
//! `Self` in an impl of an ADT with the ADT itself (`Def::Adt`, one segment consumed:
//! blaze-ladder front2/world_paths.rs), and rules/lit_path.rs `value_path` then gave that ADT
//! FRESH arguments through `explicit_args`, as for a written name. `W<?0>` met the impl's own
//! fresh arguments in rules/method.rs `impl_args` (`W<?1>` unified with `W<?0>`), and nothing
//! fixed either variable: the fn's predicates (the impl's implied `T: Sized`, registered by
//! lit_path.rs `assoc_item_ty`) stayed ambiguous at `?0` ("`Error: Sized`" in the census, the
//! variable printed after writeback), and `T::Handler` became `<?0 as Storable>::Handler`, on
//! which no method is found. The fix is the `by_self` branch of `value_path` (hello-D, with
//! census14-C-2's no-generics case): `Self` stands at `impl_self_adt_args`, the impl's own
//! self type, so `impl_args` unifies the impl's fresh arguments with the impl's parameters.
//!
//! The rules:
//! - The Reference, "Paths", "`Self`": "In an implementation, it refers to the type being
//!   implemented." The type implemented by `impl<T: Storable> W<T>` is `W<T>` at the impl's own
//!   `T`, so `Self::ticket` is `<W<T>>::ticket` with nothing left to infer.
//! - The Reference, "Special types and traits", "`Sized`": "Type parameters (except `Self` in
//!   traits) are `Sized` by default", and an associated type is `Sized` unless declared
//!   `?Sized`: `T: Sized` and `T::Handler: Sized` hold from the where-clauses.
//! - The Reference, "Paths", "Paths in expressions": a generic item's arguments a path does
//!   not write are inferred, and "type annotations needed" when nothing fixes them: `make`'s own
//!   `U` is fixed by nothing in `let x = Self::make();`, so that stays E0282 (the control that
//!   the fix does not give a fn's OWN parameters anything).
//! - The Reference, "Method call expressions": a method of a trait in scope is a candidate for
//!   any receiver type; `write` on `<T as Storable>::Handler` is proved by `push`'s
//!   where-clause `T::Handler: Handler<T>`.
//!
//! No test here runs a compiler for its answer: each expectation is the rule above.

use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, PatId};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::world::Def;

/// The world of the module doc: `[on_frame, push, guess]`, `Self` answered as `W` (one segment
/// consumed), `Handler` in scope for method calls.
fn world() -> (Fx, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let (u8_, unit) = (fx.u8(), fx.unit());
    // `trait Storable { type Handler; }`: the associated type's implied `Sized`, written.
    let storable = fx.tr("Storable", 1, Vec::new(), &["Handler"]);
    let handler_of = fx.alias(storable, "Handler", &[p0]);
    let handler_sized = fx.bound(sized, &[handler_of]);
    fx.set_assoc_bounds(storable, "Handler", Vec::from([handler_sized]));
    // `trait Handler<T> { fn write(self, v: T) -> u8; }` (`Self` is `Param(0)`, `T` `Param(1)`).
    let handler = fx.tr("Handler", 2, Vec::new(), &[]);
    fx.trait_fn(handler, "write", true, &[p0, p1], u8_);
    fx.use_trait(handler);
    // `struct W<T>; impl<T: Storable> W<T> { .. }` (the impl adds `T: Sized`).
    let w = fx.strukt("W", 1, &[]);
    let w_t = fx.adt(w, &[p0]);
    let is_storable = fx.bound(storable, &[p0]);
    let imp = fx.imp("impl<T: Storable> W<T>", 1, w_t, None, &[], Vec::from([is_storable]));
    fx.impl_fn(imp, "ticket", false, &[u8_], u8_);
    fx.impl_fn(imp, "compute", false, &[u8_], handler_of);
    let u_sized = fx.bound(sized, &[p1]);
    fx.impl_fn_own(imp, "make", false, 1, &[], p1, Vec::from([u_sized]));
    let on_frame = fx.impl_fn(imp, "on_frame", false, &[u8_], u8_);
    let handles = fx.bound(handler, &[handler_of, p0]);
    let push = fx.impl_fn_own(imp, "push", false, 0, &[p0, u8_], u8_, Vec::from([handles]));
    let guess = fx.impl_fn(imp, "guess", false, &[], unit);
    fx.use_path("Self", Def::Adt(w));
    fx.finish();
    (fx, [on_frame, push, guess])
}

/// `let t = Self::ticket(n); t` in `impl<T: Storable> W<T>` is clean and typed `u8` throughout
/// (gossip/actor.rs:279, :396). Without the fix (`Self` at fresh arguments) this fails with
/// "on_frame: unexpected diagnostics: [AnnotationsNeeded]".
///
/// Prints: `test suite::self_fn_impl_args::self_fn_with_a_plain_return_is_at_the_impls_own_arguments ... ok`
#[test]
fn self_fn_with_a_plain_return_is_at_the_impls_own_arguments() {
    let (fx, [on_frame, _, _]) = world();
    let mut b = Bb::new();
    let n = b.param("n");
    let path = b.path(&["Self", "ticket"]);
    let n_use = b.local(n);
    let call = b.call(path, &[n_use]);
    let t = b.bind("t");
    let s = b.let_(t, None, call);
    let t_use = b.local(t);
    let root = b.block(&[s], Some(t_use));
    let c = check_body(&fx, &b, on_frame, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "on_frame: unexpected diagnostics: {codes:?}");
    let want = Want {
        exprs: &[(n_use, "u8"), (call, "u8"), (t_use, "u8"), (root, "u8")],
        pats: &[(PatId(0), "u8"), (PatId(1), "u8")],
        diags: &[],
    };
    let d: Vec<_> = differences(&fx, &b, &c, &want).into_iter().filter(|l| !l.contains("no expectation written")).collect();
    assert!(d.is_empty(), "on_frame:\n{}", d.join("\n"));
}

/// `let e = Self::compute(n); e.write(v)` in `impl<T: Storable> W<T>`, `push` having
/// `T::Handler: Handler<T>`, is clean: `e` is `<T as Storable>::Handler` at the impl's `T`, and
/// `write` is found on it and typed `u8` (storage/types/vec.rs:290; `at` :271 and `pop` :324 are
/// the same path). Without the fix this fails with "push: unexpected diagnostics:
/// [AnnotationsNeeded..".
///
/// Prints: `test suite::self_fn_impl_args::self_fn_returning_a_projection_takes_a_method_call ... ok`
#[test]
fn self_fn_returning_a_projection_takes_a_method_call() {
    let (fx, [_, push, _]) = world();
    let mut b = Bb::new();
    let v = b.param("v");
    let n = b.param("n");
    let path = b.path(&["Self", "compute"]);
    let n_use = b.local(n);
    let call = b.call(path, &[n_use]);
    let e = b.bind("e");
    let s = b.let_(e, None, call);
    let e_use = b.local(e);
    let v_use = b.local(v);
    let write = b.method(e_use, "write", &[v_use]);
    let root = b.block(&[s], Some(write));
    let c = check_body(&fx, &b, push, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "push: unexpected diagnostics: {codes:?}");
    let want = Want {
        exprs: &[(n_use, "u8"), (v_use, "P0"), (write, "u8"), (root, "u8")],
        pats: &[(PatId(0), "P0"), (PatId(1), "u8")],
        diags: &[],
    };
    let d: Vec<_> = differences(&fx, &b, &c, &want).into_iter().filter(|l| !l.contains("no expectation written")).collect();
    assert!(d.is_empty(), "push:\n{}", d.join("\n"));
}

/// The control: `let x = Self::make();` with `fn make<U>() -> U` stays E0282, with and without
/// the fix: `Self` gives the impl's arguments, never the fn's own (`U`), which nothing fixes. A
/// fix that resolved every argument of a `Self::f` path (or dropped the ambiguous `U: Sized`)
/// fails it.
///
/// Prints: `test suite::self_fn_impl_args::self_fn_with_an_unfixed_own_parameter_stays_e0282 ... ok`
#[test]
fn self_fn_with_an_unfixed_own_parameter_stays_e0282() {
    let (fx, [_, _, guess]) = world();
    let mut b = Bb::new();
    let path = b.path(&["Self", "make"]);
    let call = b.call(path, &[]);
    let x = b.bind("x");
    let s = b.let_(x, None, call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, guess, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.contains(&DiagCode::AnnotationsNeeded), "guess: want E0282 (AnnotationsNeeded) for `Self::make()`, got {codes:?}");
}
