//! The harness over fixtures: an edit of each body reaches only it, in each case of
//! docs/v2/plan/lanes/firewall/notes.md section 2 that a body edit can be; and an edit that
//! changes what another item reads of a body is found.

use std::{collections::HashMap, sync::Arc};

use hir_firewall::{CodeLeak, Codegen, EDIT, Harness, Report};
use hir_expand::fixture;

/// Every function of `fixture` edited in turn, without codegen.
fn run(fixture: &str) -> Report {
    let mut db = fixture::db(fixture).into_db();
    let krate = fixture::test_crate(&db);
    let mut harness = Harness::new(&mut db, krate, None).expect("the harness sets up");
    harness.run(&mut db, 1).expect("every edit is made")
}

/// [`run`] with codegen's walk, as a dev build of the fixture's crate walks it.
fn run_compiled(fixture: &str) -> Report {
    let mut db = fixture::db(fixture).into_db();
    let krate = fixture::test_crate(&db);
    let codegen = Codegen::dev("fixture", Arc::new(HashMap::new())).expect("a dev build's policy for this machine");
    let mut harness = Harness::new(&mut db, krate, Some(codegen)).expect("the harness sets up");
    harness.run(&mut db, 1).expect("every edit is made")
}

#[track_caller]
fn assert_clean(report: &Report) {
    assert!(report.edits > 0, "no function was edited");
    assert!(report.is_clean(), "{report}");
}

#[test]
fn an_edit_of_each_function_reaches_only_it() {
    let report = run(
        r#"
//- minicore: sized, copy, fn, option, slice, index
pub struct Point { pub x: u32, pub y: u32 }

impl Point {
    pub fn new(x: u32, y: u32) -> Point { Point { x, y } }
    pub fn sum(&self) -> u32 { self.x + self.y }
}

pub trait Area {
    fn area(&self) -> u32;
    fn twice(&self) -> u32 { self.area() * 2 }
}

impl Area for Point {
    fn area(&self) -> u32 { self.x * self.y }
}

pub fn first<T: Copy>(items: &[T]) -> Option<T> {
    if items.len() == 0 { None } else { Some(items[0]) }
}

pub fn caller() -> u32 {
    let p = Point::new(1, 2);
    let f = |q: &Point| q.sum();
    f(&p) + p.twice() + first(&[1u32, 2]).unwrap_or(0)
}

mod inner {
    pub fn helper(n: u32) -> u32 {
        fn nested(m: u32) -> u32 { m + 1 }
        { let k = nested(n); k * 2 }
    }
}

pub fn uses_inner() -> u32 { inner::helper(3) }

pub const LIMIT: u32 = 10;
pub static NAME: &str = "point";
"#,
    );
    assert_clean(&report);
}

/// E1, E2: the hidden type of a return-position `impl Trait` and of an `async fn`'s future,
/// and the auto traits that leak from it. An edit that keeps the hidden type reaches no
/// caller.
#[test]
fn an_edit_that_keeps_a_hidden_type_reaches_no_caller() {
    let report = run(
        r#"
//- minicore: sized, copy, fn, future, send, sync
pub fn counter() -> impl Fn() -> u32 { || 1 }
pub fn calls_counter() -> u32 { counter()() }

pub async fn later() -> u32 { 2 }
fn is_send<T: Send>(_: T) {}
pub fn checks_send() { is_send(later()); }
"#,
    );
    assert_clean(&report);
}

/// E4: a `const fn` evaluated elsewhere. An edit that keeps its value evaluates the
/// constants that call it again (they are backdated) and reaches nothing past them.
#[test]
fn an_edit_of_a_const_fn_that_keeps_its_value_reaches_no_user_of_the_value() {
    let report = run(
        r#"
//- minicore: sized, copy
pub const fn one() -> u32 { 1 }
pub const ONE: u32 = one();
pub fn uses_one() -> [u8; ONE as usize] { [0; ONE as usize] }
pub fn other() -> u32 { 3 }
"#,
    );
    assert_clean(&report);
}

/// E4 the other way: an edit that changes a `const fn`'s value reaches the users of the
/// value, and the harness reports them; nothing else.
#[test]
fn an_edit_that_changes_a_const_fn_s_value_is_found_at_its_users() {
    let mut db = fixture::db(
        r#"
//- minicore: sized, copy
pub const fn one() -> u32 { 1 }
pub const ONE: u32 = one();
pub fn uses_one() -> [u8; ONE as usize] { [0; ONE as usize] }
pub fn other() -> u32 { 3 }
"#,
    )
    .into_db();
    let krate = fixture::test_crate(&db);
    let mut harness = Harness::new(&mut db, krate, None).expect("the harness sets up");
    let one = harness.function("one").expect("`one` has a body");
    let mut report = Report::default();
    harness.edit(&mut db, one, " return 2;", &mut report).expect("the edit is made");
    assert!(
        report.leaks.iter().any(|it| it.request.contains("uses_one")),
        "a change of `one`'s value reaches `uses_one`'s signature, and is found: {report}"
    );
    assert!(
        report.leaks.iter().all(|it| !it.request.contains("`other`")),
        "nothing of `other` reads `one`: {report}"
    );
    assert!(
        report.code.iter().all(|it| !matches!(it, CodeLeak::Body { body, .. } if body == "other")),
        "{report}"
    );
}

/// E3, E6: closures and coroutines of a body, and the instances whose type arguments name
/// them, are the body's: codegen compiles them again, and nothing else.
#[test]
fn an_edit_compiles_again_only_its_instances_and_those_over_its_closures() {
    let report = run_compiled(
        r#"
//- minicore: sized, copy, fn, drop
pub fn apply<F: Fn() -> u32>(f: F) -> u32 { f() }
pub fn with_closure() -> u32 { apply(|| 1) }
pub fn with_another() -> u32 { apply(|| 2) }
pub fn plain(n: u32) -> u32 { n + 1 }
pub fn calls_plain() -> u32 { plain(1) }
"#,
    );
    assert_clean(&report);
}

/// The edit the harness makes is one codegen sees: each edited function's code hash changes.
#[test]
fn the_harness_edit_changes_the_edited_body_s_code() {
    assert!(EDIT.starts_with(' ') && EDIT.ends_with(';'), "an edit is one statement after the `{{`");
    let report = run(
        r#"
//- minicore: sized
pub fn a() -> u32 { 1 }
pub fn b() -> u32 { a() }
"#,
    );
    assert!(report.code.iter().all(|it| !matches!(it, CodeLeak::Unchanged { .. })), "{report}");
    assert_eq!(report.edits, 2);
}
