//! The lifetime a `&dyn Trait` parameter leaves out of its trait object is the reference's, as
//! rustc's `resolve_object_lifetime_default` gives it: `fn g(x: &dyn T)` is
//! `for<'r> fn(&'r (dyn T + 'r))`, not `fn(&'r (dyn T + 'static))`.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized, fn
trait T {}

fn g(_x: &dyn T) {}
fn passes<'a>(x: &'a (dyn T + 'a)) {
    g(x)
}

fn beside_static(_x: &dyn T, _y: &(dyn T + 'static)) {}
fn passes_beside_static<'a>(x: &'a (dyn T + 'a), y: &'static (dyn T + 'static)) {
    beside_static(x, y)
}

fn take(_f: impl Fn(&dyn T)) {}
fn passes_as_a_fn() {
    take(g)
}

fn back(x: &dyn T) -> &dyn T {
    x
}
fn passes_back<'a>(x: &'a (dyn T + 'a)) -> &'a (dyn T + 'a) {
    back(x)
}

fn escapes<'a>(x: &'a (dyn T + 'a)) -> &'static (dyn T + 'static) {
    back(x)
}
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
    let _ = fx.only_file();
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
fn an_elided_dyn_parameter_is_accepted() {
    accepted("g");
}

#[test]
fn passing_a_short_lived_object_to_an_elided_dyn_parameter_is_accepted() {
    accepted("passes");
}

#[test]
fn an_elided_dyn_parameter_beside_a_static_one_is_accepted() {
    accepted("beside_static");
}

#[test]
fn passing_a_short_lived_object_beside_a_static_one_is_accepted() {
    accepted("passes_beside_static");
}

#[test]
fn a_function_with_an_elided_dyn_parameter_is_a_higher_ranked_fn() {
    accepted("passes_as_a_fn");
}

#[test]
fn an_elided_dyn_return_is_the_parameter_it_comes_from() {
    accepted("back");
}

#[test]
fn passing_a_short_lived_object_through_an_elided_dyn_return_is_accepted() {
    accepted("passes_back");
}

#[test]
fn an_elided_dyn_return_is_still_tied_to_its_parameter() {
    for result in checked("escapes") {
        assert_eq!(result.not_checked, None);
        let messages: Vec<&str> = result.diagnostics.iter().map(|it| it.message.as_str()).collect();
        assert_eq!(messages, ["lifetime may not live long enough"]);
    }
}
