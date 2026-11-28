//! Postfix `.do` suspension expressions (`docs/v2/plan/design/sans-io-machines.md`):
//! `foo.do`, `dir.open_write(opts).do?.flush().do?`, `a.do.b`, and float recovery `x.0.do`.

use super::{has_run, parse_lines};
use crate::Edition;

fn clean(text: &str) -> Vec<String> {
    let (tree, errors) = parse_lines(text, Edition::CURRENT);
    assert_eq!(errors, Vec::<String>::new(), "{text}: {tree:#?}");
    tree
}

#[test]
fn postfix_do_parses_as_do_expr() {
    let tree = clean("fn f() { foo.do; }");
    assert_eq!(tree.iter().filter(|it| *it == "DO_EXPR").count(), 1, "{tree:#?}");
    assert!(has_run(&tree, &["DOT \".\"", "DO_KW \"do\""]), "{tree:#?}");
}

#[test]
fn postfix_do_chains_with_try_and_methods() {
    let tree = clean("fn f() { dir.open_write(opts).do?.flush().do?; }");
    assert_eq!(tree.iter().filter(|it| *it == "DO_EXPR").count(), 2, "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "TRY_EXPR").count(), 2, "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "METHOD_CALL_EXPR").count(), 2, "{tree:#?}");
    assert!(has_run(&tree, &["TRY_EXPR", "DO_EXPR", "METHOD_CALL_EXPR"]), "{tree:#?}");
}

#[test]
fn postfix_do_chains_with_field_and_tuple_float_recovery() {
    let field_tree = clean("fn f() { a.do.b; }");
    assert_eq!(field_tree.iter().filter(|it| *it == "DO_EXPR").count(), 1, "{field_tree:#?}");
    assert_eq!(field_tree.iter().filter(|it| *it == "FIELD_EXPR").count(), 1, "{field_tree:#?}");
    assert!(has_run(&field_tree, &["FIELD_EXPR", "DO_EXPR", "PATH_EXPR"]), "{field_tree:#?}");

    let tuple_tree = clean("fn f() { x.0.do; }");
    assert_eq!(tuple_tree.iter().filter(|it| *it == "DO_EXPR").count(), 1, "{tuple_tree:#?}");
    assert_eq!(tuple_tree.iter().filter(|it| *it == "FIELD_EXPR").count(), 1, "{tuple_tree:#?}");
}

#[test]
fn defer_block_parses_as_defer_expr() {
    let tree = clean("fn f() { defer { f.close().do; }; }");
    assert_eq!(tree.iter().filter(|it| *it == "DEFER_EXPR").count(), 1, "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "DO_EXPR").count(), 1, "{tree:#?}");
    assert!(has_run(&tree, &["DEFER_EXPR", "DEFER_KW \"defer\"", "WHITESPACE \" \"", "BLOCK_EXPR"]), "{tree:#?}");

    // Block-like statement form without a trailing semicolon.
    let no_semi = clean("fn f() { defer { f.close().do; } work(); }");
    assert_eq!(no_semi.iter().filter(|it| *it == "DEFER_EXPR").count(), 1, "{no_semi:#?}");
    assert_eq!(no_semi.iter().filter(|it| *it == "CALL_EXPR").count(), 1, "{no_semi:#?}");
}

#[test]
fn defer_remains_ordinary_ident_outside_defer_block() {
    let tree = clean("fn defer() { let defer = 1; defer(2); x.defer; if defer { work(); } }");
    assert!(!tree.iter().any(|it| it == "DEFER_EXPR"), "{tree:#?}");
    assert!(!tree.iter().any(|it| it.starts_with("DEFER_KW")), "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "CALL_EXPR").count(), 2, "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "FIELD_EXPR").count(), 1, "{tree:#?}");
}
