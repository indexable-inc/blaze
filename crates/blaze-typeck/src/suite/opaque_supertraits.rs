//! A bound written on a return-position `impl Trait` gives its trait's supertraits too. The
//! standard library's form (walk list-AJ: `array::from_fn`, `Iterator::any`, `Iterator::all`,
//! hashbrown's `make_hasher`), reduced, with `Once`/`Mut` standing for `FnOnce`/`FnMut`:
//!
//! ```text
//! trait Once<A> { type Output; }
//! trait Mut<A>: Once<A> {}
//! fn mk() -> impl Mut<u8, Output = u16> { .. }   // `impl FnMut(u8) -> u16`
//! fn need<X: Once<u8>>(x: X) -> <X as Once<u8>>::Output { .. }
//! fn user() -> u16 { need(mk()) }                 // clean
//! ```
//!
//! The front end files `impl FnMut(u8) -> u16` as `Opaque: FnMut<(u8,)>` and `<Opaque as
//! FnOnce<(u8,)>>::Output == u16` (front2/preds.rs: "`Fn(A) -> B`'s `Output` is `FnOnce`'s"),
//! so the projection bound names a trait that no written trait bound names. The library:
//! core/src/ops/function.rs `pub trait FnMut<Args: Tuple>: FnOnce<Args>`. The rules relied
//! on, from the Reference:
//!
//! - "Impl trait", "Abstract return types": "the caller can only use the methods declared by
//!   the trait bound": the caller proves what it needs of the opaque type from its bounds.
//! - "Traits", "Supertraits": "Supertraits are traits that are required to be implemented for
//!   a type to implement a specific trait. Furthermore, anywhere a generic or trait object is
//!   bounded by a trait, it has access to the associated items of its supertraits." and
//!   "Supertraits are declared by trait bounds on the `Self` type of a trait and transitively
//!   the supertraits of the traits declared in those trait bounds." So the opaque type, a
//!   `Mut<u8>`, is a `Once<u8>`, and `<Opaque as Once<u8>>::Output` is `u16` by its bound.
//!
//! The cause as found (2026-11-16, by reading; no run): the solver assembles a rigid alias's
//! bounds as they are given (blaze-solve alias.rs `push_alias_bounds`: a trait want matches
//! only a bound of that very trait), and the checker gave an opaque's written bounds alone
//! (infer/facts.rs `fill_opaque`), so `Opaque: Once<u8>` had no candidate and `need(..)` was
//! E0277. An associated type's item bounds were already elaborated for the same reason
//! (infer/facts/item_bounds.rs `elaborate_item_bounds`; suite/assoc_bound_supers.rs); an
//! opaque's bounds now go through the same function.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// What `mk`'s opaque type writes as its trait bound (beside `Output == u16` and `Sized`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Declared {
    /// `impl Mut<u8, Output = u16>`: `Once<u8>` is `Mut`'s supertrait.
    Mut,
    /// `impl Sub<u8, Output = u16>` with `trait Sub<A>: Mut<A> {}`: `Once<u8>` two levels up.
    Sub,
    /// `impl Other<u8>` with `trait Other<A> {}`: a bound that says nothing of `Once`.
    Other,
}

/// The program of the module's head with `declared` on `mk`'s opaque type.
fn world(declared: Declared) -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));

    // trait Once<A> { type Output; }  trait Mut<A>: Once<A> {}  trait Sub<A>: Mut<A> {}
    // trait Other<A> {}   (each with `Self` as `Param(0)` and `A` as `Param(1)`)
    let once = fx.tr("Once", 2, Vec::new(), &["Output"]);
    let self_once = fx.bound(once, &[p0, p1]);
    let mut_ = fx.tr("Mut", 2, std::vec![self_once], &[]);
    let self_mut = fx.bound(mut_, &[p0, p1]);
    let sub = fx.tr("Sub", 2, std::vec![self_mut], &[]);
    let other = fx.tr("Other", 2, Vec::new(), &[]);

    // fn mk() -> impl <declared><u8, Output = u16>: no generics in scope.
    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let o = fx.opaque_key("mk::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_sized = fx.bound(sized, &[opaque]);
    let bounds = match declared {
        Declared::Mut => {
            let is_mut = fx.bound(mut_, &[opaque, u8_]);
            let out = fx.proj(once, &[opaque, u8_], "Output", u16_);
            std::vec![is_mut, out, is_sized]
        }
        Declared::Sub => {
            let is_sub = fx.bound(sub, &[opaque, u8_]);
            let out = fx.proj(once, &[opaque, u8_], "Output", u16_);
            std::vec![is_sub, out, is_sized]
        }
        Declared::Other => {
            let is_other = fx.bound(other, &[opaque, u8_]);
            std::vec![is_other, is_sized]
        }
    };
    fx.set_opaque_bounds(o, bounds);
    fx.func("mk", 0, &[], opaque, Vec::new());

    // fn need<X: Once<u8>>(x: X) -> <X as Once<u8>>::Output
    let x_once = fx.bound(once, &[p0, u8_]);
    let out = fx.alias(once, "Output", &[p0, u8_]);
    fx.func("need", 1, &[p0], out, std::vec![x_once]);

    let user = fx.func("user", 0, &[], u16_, Vec::new());
    fx.finish();
    (world, user)
}

/// `need(mk())`: the body, its root and the outer call.
fn body() -> (Bb, ExprId, ExprId) {
    let mut b = Bb::new();
    let need = b.path(&["need"]);
    let mk = b.path(&["mk"]);
    let inner = b.call(mk, &[]);
    let outer = b.call(need, &[inner]);
    let root = b.block(&[], Some(outer));
    (b, root, outer)
}

/// Each diagnostic as `code at <expression> found <type>`.
fn diags_shown(fx: &Fx, c: &crate::columns::Checked<'_>) -> Vec<String> {
    c.diags
        .iter()
        .map(|d| format!("{} at {} found {:?}", d.code.code(), d.at.start, d.found.map(|t| super::show(fx, c.types.view(), t))))
        .collect()
}

/// `impl Mut<u8, Output = u16>` and, two levels up, `impl Sub<u8, Output = u16>`: `need(mk())`
/// checks as `u16` with no diagnostic. Without the supertraits of the written bound this is
/// E0277 at `need(..)` (`Opaque: Once<u8>`), the standard library's class.
///
/// Prints: `test suite::opaque_supertraits::opaque_bound_supertrait_projects ... ok`
#[test]
fn opaque_bound_supertrait_projects() {
    for declared in [Declared::Mut, Declared::Sub] {
        let (fx, user) = world(declared);
        let (b, root, _) = body();
        let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
        let d = diags_shown(&fx, &c);
        let which = if declared == Declared::Mut { "impl Mut<u8, Output = u16>" } else { "impl Sub<u8, Output = u16>" };
        assert!(d.is_empty(), "`{which}`: the body has diagnostics:\n{}", d.join("\n"));
    }
}

/// The control (rule 23: remove the cause, the effect goes): `impl Other<u8>` has no `Once`
/// above it, so `Opaque: Once<u8>` does not hold and `need(mk())` is refused with E0277. A fix
/// that takes any opaque bound as proof, or answers every goal on an opaque type, fails here.
///
/// Prints: `test suite::opaque_supertraits::opaque_bound_supertrait_unrelated_is_refused ... ok`
#[test]
fn opaque_bound_supertrait_unrelated_is_refused() {
    let (fx, user) = world(Declared::Other);
    let (b, root, outer) = body();
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let d = diags_shown(&fx, &c);
    assert!(
        c.diags.iter().any(|x| x.code == DiagCode::Unsatisfied),
        "`impl Other<u8>`: no E0277, so `Opaque: Once<u8>` held with no bound saying so (call is expression {}):\n{}",
        outer.0,
        d.join("\n")
    );
}
