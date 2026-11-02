//! rust-lang/rust#25860: a function whose signature implies a bound (`'b: 'a` of `&'a &'b ()`)
//! coerced to a higher-ranked fn pointer, or one fn pointer used as a more general one, keeps
//! that bound: it is proven for the lifetimes the more general signature's `for<'a>` is
//! instantiated with, and assumed for its placeholders only where its own signature implies it.
//! rustc drops it, which cve-rs turns into use-after-free in safe code; blaze refuses it.

use base_db::{Db, all_crates};
use hir_def::{
    AssocItemId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized, copy
static UNIT: &'static &'static () = &&();

fn helper<'a, 'b, T>(_: &'a &'b (), v: &'b T) -> &'a T {
    v
}

fn expand<'a, 'b, T>(x: &'a T) -> &'b T {
    let f: for<'x> fn(_, &'x T) -> &'b T = helper;
    f(UNIT, x)
}

fn shorten<'a, 'b: 'a, T>(x: &'b T) -> &'a T {
    let f: fn(&'static &'static (), &'b T) -> &'a T = helper;
    f(UNIT, x)
}

fn helper3<'a, 'b, T>(_: &'a &'b (), v: &'b T, _: &()) -> &'a T {
    v
}

fn expand_elided<'a, T>(x: &'a T) -> &'static T {
    let f: fn(_, &'a T, &()) -> &'static T = helper3;
    f(UNIT, x, &())
}

fn expand_through_a_pointer<'a, 'b, T>(x: &'a T) -> &'b T {
    let f: for<'p, 'q> fn(&'p &'q (), &'q T) -> &'p T = helper;
    let g: for<'x> fn(_, &'x T) -> &'b T = f;
    g(UNIT, x)
}

fn as_its_own_pointer<'c, T>(x: &'c &'c (), v: &'c T) -> &'c T {
    let f: for<'p, 'q> fn(&'p &'q (), &'q T) -> &'p T = helper;
    let g: for<'p, 'q> fn(&'p &'q (), &'q T) -> &'p T = f;
    g(x, v)
}

struct Formatter<'a>(&'a ());

fn fmt_like<'a, 'b>(_: &'a u8, _: &'a mut Formatter<'b>) {}

fn fmt_pointer() {
    let f: for<'a, 'b> fn(&'a u8, &'a mut Formatter<'b>) = fmt_like;
    let g: fn(&u8, &mut Formatter<'_>) = fmt_like;
    let _ = (f, g);
}

fn takes<'a, T>(_: &'a T) -> &'a () {
    &()
}

fn type_bound_assumed<T>() {
    let f: for<'x> fn(&'x T) -> &'x () = takes;
    let _ = f;
}
"#;

const CVE_RS: &str = concat!("//- minicore: sized, copy, option\n", include_str!("cve_rs_fixture.rs"));

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

fn checked(fixture_text: &str, name: &str) -> [BorrowckResult; 2] {
    let fx = fixture::db(fixture_text);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius].map(|mode| borrowck_in_mode(&db, function.into(), mode))
    })
}

fn accepted_in(fixture_text: &str, name: &str) {
    for result in checked(fixture_text, name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
    }
}

/// `name` is refused with one error naming the function or fn pointer coerced, the bound its
/// signature implies, and rust-lang/rust#25860.
fn refused_in(fixture_text: &str, name: &str, source: &str, bound: &str) {
    for result in checked(fixture_text, name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        let found = result.diagnostics.iter().any(|it| {
            it.message.starts_with(&format!("`{source}` coerced to "))
                && it.message.contains(&format!("loses the bound `{bound}`"))
                && it.message.contains("rust-lang/rust#25860")
        });
        assert!(found, "`{name}`: {:?}", result.diagnostics);
    }
}

#[test]
fn an_implied_bound_dropped_by_a_higher_ranked_fn_pointer_coercion_is_refused() {
    refused_in(FIXTURE, "expand", "helper", "'b: 'a");
}

#[test]
fn the_error_names_the_function_the_target_the_bound_and_the_rustc_issue() {
    for result in checked(FIXTURE, "expand") {
        let messages: Vec<&str> = result.diagnostics.iter().map(|it| it.message.as_str()).collect(); // alloc: a test's list
        assert_eq!(
            messages,
            [
                "`helper` coerced to `for<'x> fn(&&(), &T) -> &'b T` loses the bound `'b: 'a` its \
                 signature implies, which no call of the fn pointer proves: rustc accepts this \
                 through its soundness hole rust-lang/rust#25860 \
                 (https://github.com/rust-lang/rust/issues/25860), blaze refuses it"
            ]
        );
    }
}

#[test]
fn a_coercion_to_a_fn_pointer_higher_ranked_only_in_an_elided_lifetime_is_refused() {
    refused_in(FIXTURE, "expand_elided", "helper3", "'b: 'a");
}

#[test]
fn a_fn_pointer_used_as_a_more_general_one_keeps_its_implied_bound() {
    refused_in(
        FIXTURE,
        "expand_through_a_pointer",
        "for<'p, 'q> fn(&&(), &T) -> &T",
        "'q: 'p",
    );
}

#[test]
fn a_coercion_whose_target_keeps_the_bound_is_accepted() {
    accepted_in(FIXTURE, "shorten");
    accepted_in(FIXTURE, "as_its_own_pointer");
    accepted_in(FIXTURE, "fmt_pointer");
}

#[test]
fn a_type_bound_the_target_signature_implies_is_assumed_for_its_placeholders() {
    accepted_in(FIXTURE, "type_bound_assumed");
}

#[test]
fn cve_rs_lifetime_expansion_is_refused_naming_the_bound() {
    refused_in(CVE_RS, "expand", "lifetime_translator", "'b: 'a");
    refused_in(CVE_RS, "expand_mut", "lifetime_translator_mut", "'b: 'a");
    // Its callers in `transmute`, `use_after_free` and `buffer_overflow` trust `expand_mut`'s
    // signature: what refuses them is the error above.
    accepted_in(CVE_RS, "transmute_inner");
    accepted_in(CVE_RS, "get_horrible_buffer_mut");
    accepted_in(CVE_RS, "sentinel");
}
