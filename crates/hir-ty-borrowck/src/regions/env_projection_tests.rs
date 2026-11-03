//! An associated type normalized through what the environment gives, a where clause's binding
//! (`D: DJ<O = &'a u8>`) or a `dyn` type's (`dyn DJ<O = &'a u8>`), is that binding's type with
//! its lifetimes: `d.get()` is a `&'a u8`, which outlives `'static` only if `'a` does. Each
//! `bad_*` is one rustc rejects and each `ok_*` one it accepts. Checked against rustc
//! +nightly-2026-10-27 in the rpitconc review (/tmp/rpitconc-rv/rs and rs2): `bad_gen_simple`,
//! `bad_gen_proj`, the four `bad_dyn_*`, `ok_dyn_simple`, the `ok_proj_proj*` and the
//! `ok_*_bound`s. The others vary those by one step (a `let`, an annotation, a `'static`, a
//! `'b: 'a` or `D: 'a` binding, a closure) and are not yet checked against rustc.

use base_db::{Db, all_crates};
use hir_def::{
    FunctionId, ModuleDefId, TraitId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TraitSignature},
};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized, fn, dispatch_from_dyn
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub fn take_static(_: impl Plain + 'static) {}
pub fn need_univ<'s, X: 's>(_: X, y: &'s u8) -> &'s u8 { y }
pub static B: u8 = 0;

pub trait DJ { type O; fn get(&self) -> Self::O; }
pub trait DI<'a> { type O; fn get(&self, x: &'a u8) -> Self::O; }

pub fn bad_gen_simple<'a, D: DJ<O = &'a u8> + ?Sized>(d: &D) -> &'static u8 { d.get() }
pub fn bad_gen_proj<'a, D: DI<'a, O = &'a u8> + ?Sized>(d: &D, c: &'a u8) { take_static(d.get(c)) }
pub fn bad_gen_let<'a, D: DJ<O = &'a u8>>(d: &D) { let r = d.get(); take_static(r) }
pub fn bad_gen_annot<'a, D: DJ<O = &'a u8>>(d: &D) { let r: D::O = d.get(); take_static(r) }
pub fn ok_gen_simple<'a, D: DJ<O = &'a u8> + ?Sized>(d: &D) -> &'a u8 { d.get() }
pub fn ok_gen_static<D: DJ<O = &'static u8> + ?Sized>(d: &D) -> &'static u8 { d.get() }
pub fn ok_gen_outlives<'a, 'b: 'a, D: DJ<O = &'b u8> + ?Sized>(d: &D) -> &'a u8 { d.get() }
pub fn ok_gen_bounded<'a, D: DJ<O = &'a u8> + 'a + ?Sized>(d: &'a D) -> &'a u8 { d.get() }
pub fn ok_gen_closure<'a, D: DJ<O = &'a u8>>(d: &D) -> &'a u8 { let f = || d.get(); f() }

pub fn bad_dyn_simple<'a>(d: &dyn DJ<O = &'a u8>) -> &'static u8 { d.get() }
pub fn bad_dyn_simple_take<'a>(d: &dyn DJ<O = &'a u8>) { take_static(d.get()) }
pub fn bad_dyn_proj<'a>(d: &dyn DI<'a, O = &'a u8>, c: &'a u8) { take_static(d.get(c)) }
pub fn bad_dyn_proj_static_arg<'a>(d: &dyn DI<'a, O = &'a u8>) { take_static(d.get(&B)) }
pub fn ok_dyn_simple<'a>(d: &dyn DJ<O = &'a u8>) -> &'a u8 { d.get() }
pub fn ok_dyn_static(d: &dyn DJ<O = &'static u8>) -> &'static u8 { d.get() }

pub struct P<T>(pub T);
pub trait Inner { type B; fn b(&self) -> Self::B; }
pub trait Outer { type A; fn a(&self) -> Self::A; }
impl<T: Inner> Outer for P<T> { type A = T::B; fn a(&self) -> T::B { self.0.b() } }
pub fn ok_proj_proj<T: Inner>(p: &P<T>) -> T::B { p.a() }
pub fn ok_proj_proj_let<T: Inner>(p: &P<T>) { let x: T::B = p.a(); let _y: <P<T> as Outer>::A = x; }
pub trait InnerL<'a> { type B; }
pub trait OuterL<'a> { type A; fn a(&self, x: &'a u8) -> Self::A; }
impl<'a, T: InnerL<'a>> OuterL<'a> for P<T> { type A = <T as InnerL<'a>>::B; fn a(&self, _: &'a u8) -> Self::A { loop {} } }
pub fn ok_proj_proj_lt<'a, T: InnerL<'a>>(p: &P<T>, x: &'a u8) -> <T as InnerL<'a>>::B { p.a(x) }

pub trait Mk { fn mk<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
pub trait Mk5 { fn mk5(x: &u8) -> impl Plain + '_; }
pub struct Z;
impl Mk5 for Z { fn mk5(x: &u8) -> impl Plain + '_ { x } }
pub fn ok_gen_elided_bound<'s, T: Mk5>(x: &'s u8) -> &'s u8 { need_univ(T::mk5(x), x) }
pub fn ok_gen_named_bound<'s, T: Mk>(t: &T, x: &'s u8) -> &'s u8 { need_univ(t.mk(x), x) }
pub fn ok_conc_elided_bound(x: &u8) -> &u8 { need_univ(Z::mk5(x), x) }
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}`")
}

fn trait_named(db: &Db, name: &str) -> TraitId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::TraitId(it) = def
                    && TraitSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no trait `{name}`")
}

/// Each trait is dyn compatible in the fixture, so a call on a `dyn` of it is the object's
/// method with the object type as `Self`. Without minicore's `dispatch_from_dyn` (its `Unsize`
/// and `DispatchFromDyn`), no `&self` receiver is dispatchable, as rustc's
/// `receiver_is_dispatchable` has it; method confirmation then gives the call error arguments,
/// `<{type error} as DJ>::O` normalizes to an error, and every `bad_dyn_*` is accepted for
/// that reason alone.
fn assert_dyn_compatible(names: &[&str]) {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        for &name in names {
            let trait_ = trait_named(&db, name);
            let violation = hir_ty::dyn_compatibility::dyn_compatibility(&db, trait_);
            assert!(violation.is_none(), "`{name}` is not dyn compatible: {violation:?}");
        }
    })
}

/// Each name's result in NLL and in Polonius, with a fixture database of its own.
fn checked(names: &[&str]) -> Vec<(String, Mode, BorrowckResult)> {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let mut out = Vec::new(); // alloc: test
        for &name in names {
            let function = function(&db, name);
            for mode in [Mode::Nll, Mode::Polonius] {
                let result = borrowck_in_mode(&db, function.into(), mode);
                out.push((name.to_owned(), mode, result)); // alloc: test
            }
        }
        out
    })
}

/// Every name rejected and every other accepted, in both modes; all of them checked before
/// any assertion, so a failure lists each name that is wrong.
fn expect(rejected: &[&str], accepted: &[&str]) {
    let mut wrong = Vec::new(); // alloc: test
    let names: Vec<&str> = rejected.iter().chain(accepted).copied().collect(); // alloc: test
    for (name, mode, result) in checked(&names) {
        if result.not_checked.is_some() || result.skipped.is_some() {
            let (not_checked, skipped) = (&result.not_checked, &result.skipped);
            // alloc: test
            wrong.push(format!("`{name}` {mode:?}: not checked ({not_checked:?}, {skipped:?})"));
            continue;
        }
        let errors: Vec<&str> = // alloc: test
            result.diagnostics.iter().filter(|it| it.is_error()).map(|it| it.code).collect();
        let want_rejected = rejected.contains(&name.as_str());
        if want_rejected && errors.is_empty() {
            wrong.push(format!("`{name}` {mode:?}: accepted, rustc rejects it")); // alloc: test
        }
        if !want_rejected && !result.diagnostics.is_empty() {
            let diagnostics = &result.diagnostics;
            // alloc: test
            wrong.push(format!("`{name}` {mode:?}: {diagnostics:?}, rustc accepts it"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// `D: DJ<O = &'a u8>` normalizes `<D as DJ>::O` to the where clause's `&'a u8`, whose `'a`
/// is the body's: the call's value is a `&'a u8`, so `-> &'static u8 { d.get() }` needs
/// `'a: 'static`. A `'b: 'a`, `D: 'a` or `'static` binding still holds.
#[test]
fn a_where_clauses_binding_keeps_its_lifetime_when_normalized() {
    expect(
        &["bad_gen_simple", "bad_gen_proj", "bad_gen_let", "bad_gen_annot"],
        &[
            "ok_gen_simple",
            "ok_gen_static",
            "ok_gen_outlives",
            "ok_gen_bounded",
            "ok_gen_closure",
            "ok_proj_proj",
            "ok_proj_proj_let",
            "ok_proj_proj_lt",
            "ok_gen_elided_bound",
            "ok_gen_named_bound",
            "ok_conc_elided_bound",
        ],
    );
}

/// `<dyn DJ<O = &'a u8> as DJ>::O` is the object's binding, `&'a u8`, with the lifetime of
/// the receiver's type: the call's value outlives `'static` only if `'a` does.
#[test]
fn a_dyn_types_binding_keeps_its_lifetime_when_normalized() {
    assert_dyn_compatible(&["DJ", "DI"]);
    expect(
        &["bad_dyn_simple", "bad_dyn_simple_take", "bad_dyn_proj", "bad_dyn_proj_static_arg"],
        &["ok_dyn_simple", "ok_dyn_static"],
    );
}
