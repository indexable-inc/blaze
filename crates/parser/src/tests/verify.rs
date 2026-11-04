//! blaze_verify's clauses (`grammar/verify.rs`).

use super::{has_run, parse_lines};
use crate::Edition;

/// A function with no `->` takes its clauses as clauses: `requires` is not read as a return
/// type that lost its `->` (the recovery for `fn foo() usize {}`).
#[test]
fn a_function_with_no_return_type_takes_its_clauses() {
    for text in ["fn f(x: u32) requires x < 10, {}", "fn f(x: u32) ensures ({ x < 10 }) {}"] {
        let (tree, errors) = parse_lines(text, Edition::CURRENT);
        assert_eq!(errors, Vec::<String>::new(), "{text}: {tree:#?}");
        assert!(!tree.iter().any(|it| it == "RET_TYPE"), "{text}: {tree:#?}");
        assert!(has_run(&tree, &["WHITESPACE \" \"", "SPEC_CLAUSE"]), "{text}: {tree:#?}");
    }
}

/// `-> (r: T)` names the return: a pattern and a type in the return type, not a tuple type
/// that stops at the `:`; `-> (a::B)` stays a parenthesized path.
#[test]
fn a_named_return_type_is_a_pattern_and_a_type() {
    let text = "fn f(x: u32) -> (r: u32) ensures r > x, { x + 1 }";
    let (tree, errors) = parse_lines(text, Edition::CURRENT);
    assert_eq!(errors, Vec::<String>::new(), "{text}: {tree:#?}");
    assert!(
        has_run(
            &tree,
            &["RET_TYPE", "THIN_ARROW \"->\"", "WHITESPACE \" \"", "L_PAREN \"(\"", "IDENT_PAT"]
        ),
        "{tree:#?}"
    );
    let (tree, errors) = parse_lines("fn f() -> (a::B) {}", Edition::CURRENT);
    assert_eq!(errors, Vec::<String>::new(), "{tree:#?}");
    assert!(has_run(&tree, &["L_PAREN \"(\"", "PATH_TYPE"]), "{tree:#?}");
}
