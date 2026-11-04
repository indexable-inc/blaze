//! A returned `impl Trait`'s bounds hold of the type it hides when that type is, or has in it,
//! another `impl Trait` (a function's, a trait method's) or a projection: rustc proves the
//! bound `Hidden: 'static` of `-> impl Plain + 'static + use<'a>` as any type's outlives, and
//! an alias outlives `'static` only by a bound of its own or by every argument it captures
//! (`alias_ty_must_outlive`). `{ helper(c) }`, `helper` returning `impl Plain + 'a`, hides
//! the borrow `c` as `{ c }` does, and every caller trusts the `'static` (review S-B of
//! rpitconc: blaze accepted these, rustc rejects them). Each `bad_*` is rejected by rustc
//! +nightly-2026-10-27 and each `ok_*` accepted (/tmp/rpitconc-rv/rs2/probe2.rs for the
//! first four).

use base_db::{Db, all_crates};
use hir_def::{
    AssocItemId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub static B: u8 = 0;

pub fn helper<'a>(x: &'a u8) -> impl Plain + 'a { x }
pub fn captures<'a>(x: &'a u8) -> impl Plain + use<'a> { x }
pub fn deeper<'a>(x: &'a u8) -> impl Plain + 'a { helper(x) }
pub fn static_inner<'a>(_x: &'a u8) -> impl Plain + 'static + use<'a> { 0u8 }
pub fn nothing() -> impl Plain { 0u8 }
pub fn inner_t<'b, U>(x: &'b u8, _u: U) -> impl Plain + 'b { x }
pub fn static_t<U>(_u: U) -> impl Plain + 'static { 0u8 }

pub trait Mk { fn mk<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
impl Mk for u8 { fn mk<'a>(&self, x: &'a u8) -> impl Plain + 'a { x } }
pub trait MkH { fn mkh<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
impl MkH for u8 { fn mkh<'a>(&self, x: &'a u8) -> impl Plain + 'a { helper(x) } }

pub fn bad_direct<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { c }
pub fn bad_rpit<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { helper(c) }
pub fn bad_captures<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { captures(c) }
pub fn bad_two_deep<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { deeper(c) }
pub fn bad_wrapped<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { (helper(c),) }
pub fn bad_rpitit<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { 0u8.mk(c) }
pub fn bad_rpitit_h<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { 0u8.mkh(c) }
pub fn bad_generic_rpitit<'a, T: Mk + 'static>(t: &'a T, c: &'a u8) -> impl Plain + 'static + use<'a, T> { t.mk(c) }
pub fn bad_local<'a>(_c: &'a u8) -> impl Plain + 'static + use<'a> { let x = 0u8; helper(&x) }
pub fn bad_local_no_lifetime() -> impl Plain + 'static { let x = 0u8; helper(&x) }
pub fn bad_typaram<'a, T>(c: &'a u8, t: T) -> impl Plain + 'static + use<'a, T> { inner_t(c, t) }

pub fn ok_bounded<'a>(c: &'a u8) -> impl Plain + 'a { helper(c) }
pub fn ok_captures<'a>(c: &'a u8) -> impl Plain + use<'a> { captures(c) }
pub fn ok_two_deep<'a>(c: &'a u8) -> impl Plain + 'a { deeper(c) }
pub fn ok_static_arg<'a>(_c: &'a u8) -> impl Plain + 'static + use<'a> { helper(&B) }
pub fn ok_static_inner<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { static_inner(c) }
pub fn ok_captures_nothing<'a>(_c: &'a u8) -> impl Plain + 'static + use<'a> { nothing() }
pub fn ok_rpitit_static<'a>(_c: &'a u8) -> impl Plain + 'static + use<'a> { 0u8.mk(&B) }
pub fn ok_rpitit_bounded<'a>(c: &'a u8) -> impl Plain + 'a { 0u8.mk(c) }
pub fn ok_generic_rpitit<'a, T: Mk + 'static>(t: &'a T, _c: &'a u8) -> impl Plain + 'static + use<'a, T> { t.mk(&B) }
pub fn ok_no_lifetime() -> impl Plain + 'static { helper(&B) }
pub fn ok_typaram<'a, T>(x: &'a u8, t: T) -> impl Plain + 'a { inner_t(x, t) }
pub fn ok_typaram_static<T>(t: T) -> impl Plain + 'static { inner_t(&B, t) }
pub fn ok_static_t<T>(t: T) -> impl Plain + 'static { static_t(t) }

pub struct Holder(u8);
impl Holder {
    pub fn inner(&self) -> impl Plain + '_ { &self.0 }
    pub fn inner_captures(&self) -> impl Plain { &self.0 }
}
pub fn ok_elided_bound(h: &Holder) -> impl Plain + '_ { h.inner() }
pub fn ok_elided_captures(h: &Holder) -> impl Plain + '_ { h.inner_captures() }
pub struct H2<T>(T);
impl<T> H2<T> { pub fn inner(&self) -> impl Plain + '_ { &self.0 } }
pub fn ok_h2<T>(h: &H2<T>) -> impl Plain + '_ { h.inner() }
"#;

/// An `async fn` returning an `impl Trait`: the future's output hides it, related as the
/// signature declares it (`async_output_members`). Edition 2024 captures `'a` unasked.
const ASYNC: &str = r#"
//- minicore: sized, future
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub static B: u8 = 0;
pub fn helper<'a>(x: &'a u8) -> impl Plain + 'a { x }

pub async fn bad_async<'a>(c: &'a u8) -> impl Plain + 'static { helper(c) }
pub async fn ok_async_static<'a>(_c: &'a u8) -> impl Plain + 'static { helper(&B) }
pub async fn ok_async_bounded<'a>(c: &'a u8) -> impl Plain + 'a { helper(c) }
"#;

/// An async closure returned as the `impl Trait`: the hidden type is the closure, whose
/// upvars hold `helper(c)`'s `impl Trait` (review B1 of opaqret; rustc rejects it). Its
/// aliases are found through the closure's tupled upvars (`alias_components`).
const ASYNC_CLOSURE: &str = r#"
//- minicore: sized, future, async_fn
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub fn helper<'a>(x: &'a u8) -> impl Plain + 'a { x }

pub fn bad_async_closure<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> {
    let h = helper(c);
    async move || { let _h = &h; }
}
"#;

/// The lifetime the hidden type's inner `impl Trait` captures is one the body names, a
/// turbofish of an early-bound lifetime (`where 'h: 'h` makes it early-bound). Only the borrow
/// checker sees it: the hidden type item well-formedness reads (`type_of_opaque`) has every
/// region erased, as rustc's typeck writes it back (hir-ty's `consider_opaque_type_use`), and
/// rustc's `check_opaque_meets_bounds` has its regions from borrowck's hidden type, which here
/// is `hidden_aliases_outlive`.
const NAMED: &str = r#"
//- minicore: sized
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub static B: u8 = 0;
pub fn helper_early<'h>(x: &'h u8) -> impl Plain + 'h where 'h: 'h { x }
pub fn captures_early<'h>(x: &'h u8) -> impl Plain + use<'h> where 'h: 'h { x }

pub fn bad_named<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { helper_early::<'a>(c) }
pub fn bad_named_captures<'a>(c: &'a u8) -> impl Plain + 'static + use<'a> { captures_early::<'a>(c) }
pub fn ok_named<'a>(c: &'a u8) -> impl Plain + 'a { helper_early::<'a>(c) }
pub fn ok_named_captures<'a>(c: &'a u8) -> impl Plain + use<'a> { captures_early::<'a>(c) }
pub fn ok_named_static<'a>(_c: &'a u8) -> impl Plain + 'static + use<'a> { helper_early::<'static>(&B) }
"#;

/// Every `impl Trait` of a return type has a defining use of its own (review N1 of opaqret):
/// one an associated type bound of another names, `impl Iter<Item = impl Plain + 'static>`,
/// hides what the outer one's hidden type's `Item` is, and each of a tuple's hides its field.
/// Each has its own member constraints, its choices the lifetimes it captures, and its own
/// bounds proven of its hidden type. `Iter` and `once` stand for `Iterator` and
/// `core::iter::once`. Probes P5 and P6 of the review (with `core::iter::once`) are rejected
/// by rustc +nightly-2026-10-27 as the review expects; the other verdicts here follow
/// rustc's rule and were NOT run through rustc.
const MULTI: &str = r#"
//- minicore: sized
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub static B: u8 = 0;
pub fn helper<'a>(x: &'a u8) -> impl Plain + 'a { x }

pub trait Iter { type Item; }
pub struct Once<T>(T);
impl<T> Iter for Once<T> { type Item = T; }
pub fn once<T>(t: T) -> Once<T> { Once(t) }

pub fn bad_nested_rpit<'a>(c: &'a u8) -> impl Iter<Item = impl Plain + 'static + use<'a>> + use<'a> { once(helper(c)) }
pub fn bad_nested_direct<'a>(c: &'a u8) -> impl Iter<Item = impl Plain + 'static + use<'a>> + use<'a> { once(c) }
pub fn bad_nested_deep<'a>(c: &'a u8) -> impl Iter<Item = impl Iter<Item = impl Plain + 'static + use<'a>> + use<'a>> + use<'a> { once(once(c)) }
pub fn bad_nested_capture<'a, 'b>(_a: &'a u8, b: &'b u8) -> impl Iter<Item = impl Plain + use<'a>> + use<'a, 'b> { once(b) }
pub fn bad_two_first<'a>(c: &'a u8) -> (impl Plain + 'static + use<'a>, impl Plain + use<'a>) { (helper(c), 0u8) }
pub fn bad_two_second<'a>(c: &'a u8) -> (impl Plain + use<'a>, impl Plain + 'static + use<'a>) { (0u8, c) }
pub fn bad_two_capture<'a, 'b>(a: &'a u8, b: &'b u8) -> (impl Plain + use<'a>, impl Plain + use<'b>) { (b, a) }
pub struct Wrap<T>(pub T);
pub fn bad_two_in_struct<'a>(c: &'a u8) -> Wrap<(u8, impl Plain + 'static + use<'a>)> { Wrap((0u8, helper(c))) }

pub fn ok_nested_static<'a>(_c: &'a u8) -> impl Iter<Item = impl Plain + 'static + use<'a>> + use<'a> { once(&B) }
pub fn ok_nested_rpit_static<'a>(_c: &'a u8) -> impl Iter<Item = impl Plain + 'static + use<'a>> + use<'a> { once(helper(&B)) }
pub fn ok_nested_bounded<'a>(c: &'a u8) -> impl Iter<Item = impl Plain + 'a> + use<'a> { once(helper(c)) }
pub fn ok_nested_captures<'a>(c: &'a u8) -> impl Iter<Item = impl Plain + use<'a>> + use<'a> { once(c) }
pub fn ok_nested_deep<'a>(_c: &'a u8) -> impl Iter<Item = impl Iter<Item = impl Plain + 'static + use<'a>> + use<'a>> + use<'a> { once(once(&B)) }
pub fn ok_two<'a>(c: &'a u8) -> (impl Plain + 'a, impl Plain + use<'a>) { (helper(c), c) }
pub fn ok_two_static<'a>(c: &'a u8) -> (impl Plain + 'static + use<'a>, impl Plain + use<'a>) { (helper(&B), c) }
pub fn ok_two_capture<'a, 'b>(a: &'a u8, b: &'b u8) -> (impl Plain + use<'a>, impl Plain + use<'b>) { (a, b) }
"#;

/// An `async fn` whose `impl Trait` is bounded by the elided lifetime, `+ '_` (review N2 of
/// opaqret, probe P12), or whose output has `impl Trait`s other than at its top: each is
/// checked as a sync function's is. NOT run through rustc; the verdicts follow its rule.
const ASYNC_MULTI: &str = r#"
//- minicore: sized, future
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub static B: u8 = 0;
pub fn helper<'a>(x: &'a u8) -> impl Plain + 'a { x }

pub trait Iter { type Item; }
pub struct Once<T>(T);
impl<T> Iter for Once<T> { type Item = T; }
pub fn once<T>(t: T) -> Once<T> { Once(t) }

pub struct H(u8);
impl H {
    pub async fn bad_async_elided<'a>(&self, c: &'a u8) -> impl Plain + '_ { helper(c) }
    pub async fn bad_async_elided_direct<'a>(&self, c: &'a u8) -> impl Plain + '_ { c }
    pub async fn ok_async_elided<'a>(&self, _c: &'a u8) -> impl Plain + '_ { &self.0 }
    pub async fn ok_async_elided_static<'a>(&self, _c: &'a u8) -> impl Plain + '_ { helper(&B) }
}

pub async fn bad_async_nested<'a>(c: &'a u8) -> impl Iter<Item = impl Plain + 'static> { once(c) }
pub async fn bad_async_two<'a>(c: &'a u8) -> (u8, impl Plain + 'static) { (0u8, helper(c)) }
pub async fn ok_async_nested<'a>(_c: &'a u8) -> impl Iter<Item = impl Plain + 'static> { once(&B) }
pub async fn ok_async_two<'a>(c: &'a u8) -> (impl Plain + 'a, impl Plain + 'static) { (c, helper(&B)) }
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            let free = scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            });
            let methods = scope.impls().flat_map(|it| {
                it.impl_items(db).items.iter().filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                })
            });
            for it in free.chain(methods) {
                if FunctionSignature::of(db, it).name.as_str() == name {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}`")
}

/// Borrowck of `name` in both modes, and its item well-formedness errors' codes.
fn checked(fixture_text: &str, name: &str) -> ([BorrowckResult; 2], Vec<&'static str>) {
    let db = fixture::db(fixture_text).into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        let borrowck =
            [Mode::Nll, Mode::Polonius].map(|mode| borrowck_in_mode(&db, function.into(), mode));
        let wf = hir_ty_wfcheck::wfcheck(&db, function.into());
        // alloc: test
        let wf = wf.diagnostics.iter().map(|it| it.code).collect();
        (borrowck, wf)
    })
}

fn borrowck_rejects(fixture_text: &str, name: &str) {
    let (results, _) = checked(fixture_text, name);
    for result in results {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.iter().any(|it| it.is_error()), "`{name}` is accepted");
    }
}

fn accepted(fixture_text: &str, name: &str) {
    let (results, wf) = checked(fixture_text, name);
    for result in results {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
    }
    assert!(wf.is_empty(), "`{name}` well-formedness: {wf:?}");
}

#[test]
fn an_impl_trait_the_hidden_type_has_outlives_the_returned_ones_bound_only_by_its_own() {
    for name in [
        "bad_direct",
        "bad_rpit",
        "bad_captures",
        "bad_two_deep",
        "bad_wrapped",
        "bad_rpitit",
        "bad_rpitit_h",
        "bad_generic_rpitit",
        "bad_local",
        "bad_local_no_lifetime",
        "bad_typaram",
    ] {
        borrowck_rejects(FIXTURE, name);
    }
}

#[test]
fn an_impl_trait_the_hidden_type_has_that_outlives_the_bound_is_accepted() {
    for name in [
        "ok_bounded",
        "ok_captures",
        "ok_two_deep",
        "ok_static_arg",
        "ok_static_inner",
        "ok_captures_nothing",
        "ok_rpitit_static",
        "ok_rpitit_bounded",
        "ok_generic_rpitit",
        "ok_no_lifetime",
        "ok_elided_bound",
        "ok_elided_captures",
        "ok_typaram",
        "ok_typaram_static",
        "ok_static_t",
        "ok_h2",
    ] {
        accepted(FIXTURE, name);
    }
}

#[test]
fn an_async_fns_impl_trait_hiding_an_impl_trait_outlives_its_bound_only_by_its_own() {
    borrowck_rejects(ASYNC, "bad_async");
    accepted(ASYNC, "ok_async_static");
    accepted(ASYNC, "ok_async_bounded");
}

#[test]
fn an_async_closure_hiding_an_impl_trait_outlives_the_bound_only_by_its_own() {
    borrowck_rejects(ASYNC_CLOSURE, "bad_async_closure");
}

/// An inner `impl Trait` whose bound is a region of the body outlives by that bound alone,
/// which borrowck proves: item well-formedness asks nothing of the type parameters it
/// captures (rustc's unique-bound rule in `alias_ty_must_outlive`), so `bad_typaram` is
/// refused for `'a: 'static` only, with no E0310 for `T`.
#[test]
fn item_well_formedness_asks_nothing_of_a_body_bounded_impl_traits_captured_types() {
    for name in ["ok_typaram", "ok_typaram_static", "ok_static_t", "ok_h2", "bad_typaram"] {
        let (_, wf) = checked(FIXTURE, name);
        assert!(wf.is_empty(), "`{name}` well-formedness: {wf:?}");
    }
}

/// A named lifetime the inner `impl Trait` captures: borrowck refuses `'a: 'static`. Item
/// well-formedness is not asked: the hidden type it reads has its regions erased (see `NAMED`).
#[test]
fn a_hidden_impl_traits_bound_of_a_named_lifetime_is_proven() {
    for name in ["bad_named", "bad_named_captures"] {
        borrowck_rejects(NAMED, name);
    }
    for name in ["ok_named", "ok_named_captures", "ok_named_static"] {
        accepted(NAMED, name);
    }
}

#[test]
fn every_impl_trait_of_a_return_type_outlives_its_bound_and_captures_only_its_own() {
    for name in [
        "bad_nested_rpit",
        "bad_nested_direct",
        "bad_nested_deep",
        "bad_nested_capture",
        "bad_two_first",
        "bad_two_second",
        "bad_two_capture",
        "bad_two_in_struct",
    ] {
        borrowck_rejects(MULTI, name);
    }
    for name in [
        "ok_nested_static",
        "ok_nested_rpit_static",
        "ok_nested_bounded",
        "ok_nested_captures",
        "ok_nested_deep",
        "ok_two",
        "ok_two_static",
        "ok_two_capture",
    ] {
        accepted(MULTI, name);
    }
}

#[test]
fn an_async_fns_elided_and_nested_impl_traits_are_checked() {
    for name in ["bad_async_elided", "bad_async_elided_direct", "bad_async_nested", "bad_async_two"]
    {
        borrowck_rejects(ASYNC_MULTI, name);
    }
    for name in ["ok_async_elided", "ok_async_elided_static", "ok_async_nested", "ok_async_two"] {
        accepted(ASYNC_MULTI, name);
    }
}
