//! `(include "path")` through a real front end's view: a path is relative to the view's root and
//! read from the view's files, each file once; a missing file, a cycle, and an error inside an
//! included file each name the file and its line.

use super::{query, texts, view};

const LIB: &str = r#"
pub fn old(x: u32) -> u32 { x }
pub fn top() -> u32 { old(1) }
pub mod inner {
    pub fn deep() -> u32 { crate::old(2) }
}
"#;

/// The shared library: every call of `old`, and a lint on each.
const CALLS: &str = r#"; calls of lib::old
(rule (calls-old e x) (expr e "lib::old($x)"))
(lint calls-old warning "a call of old: $e")
"#;

/// A front end of `LIB` whose view also holds `rules`, each a path and its text.
fn with_rules(rules: &[(&str, &str)]) -> (base_db::Db, super::View) {
    let mut fixture = format!("//- /lib.rs crate:lib edition:2021\n{LIB}");
    for (path, text) in rules {
        fixture.push_str(&format!("//- /{path}\n{text}"));
    }
    let mut paths = vec!["lib.rs"];
    paths.extend(rules.iter().map(|(path, _)| *path));
    view(&fixture, 0, &paths)
}

/// The error a query that must fail gives, shown.
#[track_caller]
fn refused(db: &base_db::Db, files: &super::View, rules: &str) -> String {
    match query(db, files, rules) {
        Ok(rows) => panic!("the query ran: {:?}", rows.relations),
        Err(error) => error.to_string(),
    }
}

#[test]
fn two_rules_read_the_relation_a_shared_library_defines() {
    let (db, files) = with_rules(&[("rules/lib/calls.astlog", CALLS)]);
    let rules = r#"
        (include "rules/lib/calls.astlog")
        (rule (in-fn f x) (calls-old e x) (ancestor f e) (kind f "FN"))
        (rule (argument x) (calls-old e x))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "argument"), [["1"], ["2"]]);
    let in_fn = texts(&outcome, "in-fn");
    assert_eq!(in_fn.len(), 2, "{in_fn:?}");
    assert!(in_fn[0][0].contains("fn top") && in_fn[1][0].contains("fn deep"), "{in_fn:?}");
}

#[test]
fn a_file_two_includes_reach_is_read_once() {
    let (db, files) = with_rules(&[
        ("rules/lib/calls.astlog", CALLS),
        ("rules/a.astlog", "(include \"rules/lib/calls.astlog\")\n(rule (a x) (calls-old e x))\n"),
        ("rules/b.astlog", "(include \"rules/lib/calls.astlog\")\n(rule (b x) (calls-old e x))\n"),
    ]);
    let rules = "(include \"rules/a.astlog\")\n(include \"rules/b.astlog\")\n";
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "a"), texts(&outcome, "b"));
    // Read twice, the library's lint would be declared twice and report each call twice.
    let messages: Vec<&str> = outcome.findings.iter().map(|it| it.message.as_str()).collect();
    assert_eq!(messages, ["a call of old: old(1)", "a call of old: crate::old(2)"]);
}

#[test]
fn an_include_of_a_path_the_view_does_not_hold_names_the_path_and_the_including_line() {
    let (db, files) = with_rules(&[("rules/a.astlog", "\n(include \"rules/lib/gone.astlog\")\n")]);
    let error = refused(&db, &files, "(include \"rules/a.astlog\")");
    assert!(
        error.starts_with("rules/a.astlog:2: `(include \"rules/lib/gone.astlog\")`: there is no file"),
        "{error}"
    );
    // The view's root is the base, whichever file includes: a path relative to the including
    // file's directory names nothing.
    let (db, files) = with_rules(&[
        ("rules/lib/calls.astlog", CALLS),
        ("rules/a.astlog", "(include \"lib/calls.astlog\")\n"),
    ]);
    let error = refused(&db, &files, "(include \"rules/a.astlog\")");
    assert!(error.contains("there is no file at `lib/calls.astlog`"), "{error}");
    let error = refused(&db, &files, "(include \"../rules/a.astlog\")");
    assert!(error.starts_with("rules:1: ") && error.contains("`..` segment"), "{error}");
}

#[test]
fn an_include_cycle_is_refused_naming_the_files_on_it_in_order() {
    let (db, files) = with_rules(&[
        ("rules/a.astlog", "(include \"rules/b.astlog\")\n"),
        ("rules/b.astlog", "(rule (b x) (expr x \"1\"))\n(include \"rules/a.astlog\")\n"),
    ]);
    let error = refused(&db, &files, "(include \"rules/a.astlog\")");
    assert_eq!(
        error,
        "rules/b.astlog:2: `(include \"rules/a.astlog\")` closes a cycle of includes: \
         `rules/a.astlog` -> `rules/b.astlog` -> `rules/a.astlog`"
    );
}

#[test]
fn an_error_inside_an_included_file_names_that_file_and_its_line() {
    let bad = "(rule (ok e) (expr e \"lib::old(1)\"))\n\n(rule (bad e) (expr e \"lib::nothing_here(\"))\n";
    let (db, files) = with_rules(&[("rules/lib/bad.astlog", bad)]);
    let error = refused(&db, &files, "\n\n(include \"rules/lib/bad.astlog\")");
    assert!(error.starts_with("rules/lib/bad.astlog:3: the expr pattern"), "{error}");
    let (db, files) = with_rules(&[("rules/lib/calls.astlog", CALLS)]);
    let error = refused(&db, &files, "(include \"rules/lib/calls.astlog\")\n(rule (r x) (calls-old x))");
    assert!(error.starts_with("rules:2: relation `calls-old` used with arity 1"), "{error}");
}
