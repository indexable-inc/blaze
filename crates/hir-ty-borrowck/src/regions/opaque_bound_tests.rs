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
    let fx = fixture::db(FIXTURE);
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
        assert!(
            result.diagnostics.iter().any(|it| it.code == "E0521"),
            "{:?}",
            result.diagnostics
        );
    }
}
