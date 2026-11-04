//! An `impl Trait` that captures a lifetime outlives a region its bounds name, whatever the
//! lifetimes it captures: its written `+ 'static`, or a supertrait's `Self: 'static` that
//! rustc's `item_self_bounds` elaborates, as rustc's `alias_ty_must_outlive` reads its
//! `declared_bounds_from_definition`. Reduced from wasmtime's `memory_images`
//! (vendor/wasmtime/src/runtime/module.rs), which passes `module_memory_image_source(&self) ->
//! &Arc<impl ModuleMemoryImageSource>` (a `'static` supertrait) to an `impl
//! ModuleMemoryImageSource` argument, and which blaze refused with E0521 "borrowed data escapes
//! outside of function".

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized
//- /main.rs edition:2024
pub trait Plain {}
impl Plain for u8 {}
pub trait Source: 'static {}
impl Source for u8 {}
pub trait Indirect: Source {}
impl Indirect for u8 {}

pub fn plain(x: &u8) -> impl Plain { *x }
pub fn source(x: &u8) -> impl Source { *x }
pub fn indirect(x: &u8) -> impl Indirect + use<'_> { *x }
pub fn bounded(x: &u8) -> impl Plain + 'static + use<'_> { *x }

pub fn take(_: impl Source) {}
pub fn take_ref(_: &impl Source) {}
pub fn take_static(_: impl Plain + 'static) {}

pub fn by_supertrait(c: &u8) { take(source(c)) }
pub fn by_grand_supertrait(c: &u8) { take(indirect(c)) }
pub fn by_written_bound(c: &u8) { take_static(bounded(c)) }
pub fn behind_a_reference(c: &u8) { take_ref(&source(c)) }
pub fn unbounded(c: &u8) { take_static(plain(c)) }

pub static B: u8 = 0;
pub fn two<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Plain + 'b + use<'a, 'b> { let _ = x; *y }
pub fn second_static(c: &u8) { take_static(two(c, &B)) }
pub fn second_borrowed(c: &u8) { take_static(two(&B, c)) }

pub fn needs_static<T: 'static>(_: T) {}
pub trait Two<'a, 'b> { type A: 'b; }
pub fn projection_second_static<'a, T: Two<'a, 'static>>(x: T::A) { needs_static(x) }
pub fn projection_second_borrowed<'a, T: Two<'static, 'a>>(x: T::A) { needs_static(x) }
"#;

/// An associated type whose one bound is an undeclared lifetime, an error region: rustc
/// refuses the trait (E0261, which blaze-diagnostics gives) and maps the error region to
/// `'static` in borrowck, so the bound holds and borrowck reports nothing more.
const UNNAMED_BOUND: &str = r#"
//- minicore: sized
//- /main.rs edition:2024
pub fn needs_static<T: 'static>(_: T) {}
pub trait Unnamed { type A: 'undeclared; }
pub fn unnamed_bound<T: Unnamed>(x: T::A) { needs_static(x) }
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

fn checked(name: &str) -> [BorrowckResult; 2] {
    checked_in(FIXTURE, name)
}

fn checked_in(fixture_text: &str, name: &str) -> [BorrowckResult; 2] {
    let fx = fixture::db(fixture_text);
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius].map(|mode| borrowck_in_mode(&db, function.into(), mode))
    })
}

fn accepted(name: &str) {
    for result in checked(name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
    }
}

#[test]
fn an_impl_trait_outlives_static_by_its_traits_static_supertrait() {
    accepted("by_supertrait");
    accepted("by_grand_supertrait");
    accepted("behind_a_reference");
}

#[test]
fn an_impl_trait_outlives_static_by_its_written_static_bound() {
    accepted("by_written_bound");
}

#[test]
fn an_impl_trait_with_no_static_bound_outlives_static_only_if_its_captures_do() {
    for result in checked("unbounded") {
        assert_eq!(result.not_checked, None);
        assert!(result.diagnostics.iter().any(|it| it.code == "E0521"), "{:?}", result.diagnostics);
    }
}

fn rejected(fixture_text: &str, name: &str) {
    for result in checked_in(fixture_text, name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.iter().any(|it| it.is_error()), "`{name}` is accepted");
    }
}

/// A bound names one argument of several: `+ 'b` of `two<'a, 'b>` and `type A: 'b` of
/// `Two<'a, 'b>` hold by what is passed for `'b`, whatever is passed for `'a`.
#[test]
fn a_bound_on_the_second_lifetime_is_the_second_arguments() {
    accepted("second_static");
    rejected(FIXTURE, "second_borrowed");
    accepted("projection_second_static");
    rejected(FIXTURE, "projection_second_borrowed");
}

/// An error region bound is `'static`, as rustc's `to_region_vid` maps it: `T::A: 'static`
/// with `type A: 'undeclared` holds, and E0261 is the program's one error, not E0310 too.
#[test]
fn an_error_region_bound_is_static() {
    for result in checked_in(UNNAMED_BOUND, "unnamed_bound") {
        assert_eq!(result.not_checked, None);
        assert_eq!(result.skipped, None);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }
}
