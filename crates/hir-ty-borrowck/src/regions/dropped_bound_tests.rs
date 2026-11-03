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

/// What every call of the target proves is assumed for its placeholders only: the shapes of
/// method tables and `if`-merged fn items rustc accepts, and what must still be refused.
const TARGET: &str = r#"
//- minicore: sized, copy, cell
use core::cell::Cell;

static UNIT: &'static &'static () = &&();

struct Row<'r>(&'r str);

impl<'r> Row<'r> {
    fn name(&self) -> &str {
        self.0
    }

    fn first<'a>(&'a self) -> &'a str {
        self.0
    }
}

struct Table<'t> {
    f: for<'a> fn(&'a Row<'t>) -> &'a str,
}

fn table<'t>() -> Table<'t> {
    Table { f: Row::name }
}

fn table_of_two<'t>() -> [Table<'t>; 2] {
    [Table { f: Row::name }, Table { f: Row::first }]
}

fn merged<'s, 't>(b: bool, row: &'s Row<'t>) -> &'s str {
    let f = if b { Row::name } else { Row::first };
    f(row)
}

fn through_a_mut<'s, 't>(row: &'s Row<'t>, f: &mut for<'a> fn(&'a Row<'t>) -> &'a str) -> &'s str {
    let g: &mut for<'a> fn(&'a Row<'t>) -> &'a str = f;
    *g = Row::first;
    (*g)(row)
}

fn through_a_cell<'t>(c: &Cell<for<'a> fn(&'a Row<'t>) -> &'a str>) {
    c.set(Row::name);
}

fn helper<'a, 'b, T>(_: &'a &'b (), v: &'b T) -> &'a T {
    v
}

fn expand_through_a_cell<'a, 'b, T>(c: &Cell<for<'x> fn(&'static &'static (), &'x T) -> &'b T>) {
    c.set(helper);
}

fn expand_through_a_mut<'b, T>(f: &mut for<'x> fn(&'static &'static (), &'x T) -> &'b T) {
    *f = helper;
}

struct Local<'r>(&'r u8);

fn local_first<'a, 'r>(l: &'a Local<'r>) -> &'a u8 {
    l.0
}

struct Tab<'t> {
    f: for<'a> fn(&'a Local<'t>) -> &'a u8,
}

fn tab<'t>() -> Tab<'t> {
    Tab { f: local_first }
}

fn call_a_column() {
    let text = 0u8;
    let row = Local(&text);
    let t = tab();
    let _ = (t.f)(&row);
}

fn call_a_parameter<'t>(f: for<'a> fn(&'a Local<'t>) -> &'a u8, row: Local<'t>) {
    let _ = f(&row);
}

fn pass<'a, T>(x: &'a T, _: &()) -> &'a T {
    x
}

fn static_type_bound<T>() -> for<'x> fn(&'static T, &'x ()) -> &'static T {
    pass
}

fn noop(_: &&(), _: &&()) {}

fn chained<'a, 'b>(x: &'a ()) -> &'b () {
    let f: for<'x> fn(&'x &'a (), &'b &'x ()) = noop;
    let _ = f;
    x
}

fn chained_alone<'a, 'b>() {
    let f: for<'x> fn(&'x &'a (), &'b &'x ()) = noop;
    let _ = f;
}

fn needs_static<T: 'static>(_: T) {}

fn type_bound_alone<T>() -> for<'x> fn(&'x T, &'static &'x ()) {
    takes_any
}

fn takes_any<T>(_: &T, _: &&()) {}

fn type_bound_leak<T>(x: T) -> for<'x> fn(&'x T, &'static &'x ()) {
    needs_static(x);
    takes_any
}

struct Inv<'p, 'q>(fn(&'p ()) -> &'p (), fn(&'q ()) -> &'q ());

impl<'p, 'q> Inv<'p, 'q> {
    fn early(_: Inv<'p, 'q>, _: &'p &'q (), _: &()) {}
}

fn early_unproven<'a, 'b>() {
    let f: for<'x> fn(Inv<'b, 'a>, &'static &'static (), &'x ()) = Inv::early;
    let _ = f;
}

fn early_proven<'a: 'b, 'b>() {
    let f: for<'x> fn(Inv<'b, 'a>, &'static &'static (), &'x ()) = Inv::early;
    let _ = f;
}
"#;

/// Lane s25c's regression programs for the review of v2-s25860b: placeholders whose bounds
/// name each other (B1), and a placeholder with two bounds answered one at a time (B2).
const REVIEW: &str = r#"
//- minicore: sized, copy
fn noop2(_: &&(), _: &&()) {}

fn noop1(_: &&()) {}

static Z: u64 = 0;

fn src<'o2, 'b>(_: &'o2 u64, d: &'b u64) -> (&'b u64, for<'j> fn(&'j &'o2 (), &'o2 &'j ())) {
    (d, noop2)
}

fn extend<'b2>(d: &'b2 u64) -> &'static u64 {
    let f: for<'o> fn(&'o u64, &'b2 u64) -> (&'o u64, for<'i> fn(&'i &'o (), &'o &'i ())) = src;
    f(&Z, d).0
}

fn src_one_way<'o2, 'b>(_: &'o2 u64, d: &'b u64) -> (&'b u64, for<'j> fn(&'j &'o2 ())) {
    (d, noop1)
}

fn extend_one_way<'b2>(d: &'b2 u64) -> &'static u64 {
    let f: for<'o> fn(&'o u64, &'b2 u64) -> (&'o u64, for<'i> fn(&'i &'o ())) = src_one_way;
    f(&Z, d).0
}

fn cycle_sound<'b2>(d: &'b2 u64) -> &'b2 u64 {
    let f: for<'o> fn(&'o u64, &'o u64) -> (&'o u64, for<'i> fn(&'i &'o (), &'o &'i ())) = src;
    f(d, d).0
}

struct Local<'r>(&'r u8);

struct Tab2<'t, 'u> {
    f: for<'a> fn(&'a Local<'t>, &'a Local<'u>) -> &'a u8,
}

fn take2<'t, 'u>(_: for<'a> fn(&'a Local<'t>, &'a Local<'u>) -> &'a u8) {}

fn call_two<'t, 'u>(f: for<'a> fn(&'a Local<'t>, &'a Local<'u>) -> &'a u8, a: &Local<'t>, b: &Local<'u>) -> u8 {
    *f(a, b)
}

fn copy_two<'t, 'u>(f: for<'a> fn(&'a Local<'t>, &'a Local<'u>) -> &'a u8, a: &Local<'t>, b: &Local<'u>) -> u8 {
    let g = f;
    *g(a, b)
}

fn field_call_two<'t, 'u>(c: &Tab2<'t, 'u>, a: &Local<'t>, b: &Local<'u>) -> u8 {
    *(c.f)(a, b)
}

fn construct_two<'t, 'u>(f: for<'a> fn(&'a Local<'t>, &'a Local<'u>) -> &'a u8) -> Tab2<'t, 'u> {
    Tab2 { f }
}

fn copy_field_two<'t, 'u>(t: &Tab2<'t, 'u>) -> Tab2<'t, 'u> {
    Tab2 { f: t.f }
}

fn pass_two<'t, 'u>(t: &Tab2<'t, 'u>) {
    take2(t.f)
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

fn checked(fixture_text: &str, name: &str) -> [BorrowckResult; 3] {
    let fx = fixture::db(fixture_text);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius, Mode::PoloniusFlow]
            .map(|mode| borrowck_in_mode(&db, function.into(), mode))
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
    refused_from(fixture_text, name, &format!("`{source}` coerced to "), bound);
}

/// `name` is refused with an error whose message starts with `start`, naming the bound and
/// rust-lang/rust#25860.
fn refused_from(fixture_text: &str, name: &str, start: &str, bound: &str) {
    for result in checked(fixture_text, name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        let found = result.diagnostics.iter().any(|it| {
            it.message.starts_with(start)
                && it.message.contains(" coerced to ")
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
    // Named by the variable coerced, with its type.
    refused_from(
        FIXTURE,
        "expand_through_a_pointer",
        "`f`, a `for<'p, 'q> fn(&&(), &T) -> &T`, coerced to ",
        "'q: 'p",
    );
}

/// `name` is refused, with any error.
fn refused_at_all(fixture_text: &str, name: &str) {
    for result in checked(fixture_text, name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert!(!result.diagnostics.is_empty(), "`{name}` is accepted");
    }
}

#[test]
fn a_method_tied_to_self_coerced_into_a_struct_field_is_accepted() {
    accepted_in(TARGET, "table");
    accepted_in(TARGET, "table_of_two");
}

#[test]
fn two_methods_merged_into_one_fn_pointer_by_an_if_are_accepted() {
    accepted_in(TARGET, "merged");
}

#[test]
fn a_fn_pointer_in_an_invariant_position_is_related_both_ways_with_the_target_assumed() {
    accepted_in(TARGET, "through_a_mut");
    accepted_in(TARGET, "through_a_cell");
    refused_in(TARGET, "expand_through_a_cell", "helper", "'b: 'a");
    refused_in(TARGET, "expand_through_a_mut", "helper", "'b: 'a");
}

#[test]
fn a_type_bound_the_target_implies_of_a_region_not_its_own_is_assumed() {
    accepted_in(TARGET, "static_type_bound");
}

#[test]
fn a_target_chaining_two_lifetimes_through_a_placeholder_does_not_relate_them_in_the_body() {
    // The coercion is sound; returning `x` needs `'a: 'b`, which only `'a: 'x` and `'x: 'b`
    // of the target's placeholder would give.
    accepted_in(TARGET, "chained_alone");
    for result in checked(TARGET, "chained") {
        assert!(
            result.diagnostics.iter().any(|it| !it.message.contains("rust-lang/rust#25860")),
            "`chained`: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn a_type_bound_the_target_implies_of_a_placeholder_does_not_hold_in_the_rest_of_the_body() {
    // `T: 'x` and `'x: 'static` of the placeholder are not `T: 'static` of the body.
    accepted_in(TARGET, "type_bound_alone");
    refused_at_all(TARGET, "type_bound_leak");
}

#[test]
fn a_higher_ranked_reify_proves_the_bound_on_early_bound_lifetimes() {
    refused_at_all(TARGET, "early_unproven");
    accepted_in(TARGET, "early_proven");
}

#[test]
fn a_higher_ranked_fn_pointer_called_with_a_local_borrow_is_accepted() {
    // Its call proves the signature well-formed for the lifetime it is called with, a local
    // one, never for every lifetime its binder ranges over.
    accepted_in(TARGET, "call_a_column");
    accepted_in(TARGET, "call_a_parameter");
}

#[test]
fn placeholders_whose_bounds_name_each_other_do_not_vouch_for_each_other() {
    // `for<'i> fn(&'i &'o (), &'o &'i ())` implies `'i: 'o` and `'o: 'i`: that the two are one
    // lifetime, nothing of `'b2`, which the coercion makes outlive `'o`. rustc refuses it at
    // the coercion; blaze once took each placeholder's bound as proof of the other's and
    // turned `'b2` into `'static`, a use-after-free.
    refused_at_all(REVIEW, "extend");
    refused_at_all(REVIEW, "extend_one_way");
    accepted_in(REVIEW, "cycle_sound");
}

#[test]
fn a_placeholder_with_two_bounds_is_answered_by_the_bound_in_its_position() {
    // `'t: 'a` of a copied `for<'a> fn(&'a Local<'t>, &'a Local<'u>)` is answered by the
    // copy's region in place of `'t` alone; answered by both, `'t` and `'u` would be one.
    for name in
        ["call_two", "copy_two", "field_call_two", "construct_two", "copy_field_two", "pass_two"]
    {
        accepted_in(REVIEW, name);
    }
}
