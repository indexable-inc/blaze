//! blaze's `#![feature(blaze_comptime)]` syntax (`docs/v2/plan/design/comptime.md`): `for const`,
//! `x.[f]`, bounded `for<F: ..>` binders in where clauses and `const static { .. }` blocks. The parser takes them in every
//! crate; hir-def lowering refuses them without the feature.

use super::{has_run, parse_lines};
use crate::Edition;

fn clean(text: &str) -> Vec<String> {
    let (tree, errors) = parse_lines(text, Edition::CURRENT);
    assert_eq!(errors, Vec::<String>::new(), "{text}: {tree:#?}");
    tree
}

/// The tree of `text` without the tokens of `drop`, which are each one token and one space.
fn without(tree: &[String], drop: &[&str]) -> Vec<String> {
    tree.iter().filter(|it| !drop.contains(&it.as_str())).cloned().collect()
}

#[test]
fn for_const_is_a_for_expr_with_a_const_token() {
    let comptime = clean("fn f() { 'a: for const f in fields_of::<T>() { g(f); } }");
    let plain = clean("fn f() { 'a: for f in fields_of::<T>() { g(f); } }");
    assert!(
        has_run(&comptime, &["FOR_KW \"for\"", "WHITESPACE \" \"", "CONST_KW \"const\""]),
        "{comptime:#?}"
    );
    // Apart from `const` and its space, the same tree as the plain loop's.
    let stripped: Vec<_> = comptime
        .iter()
        .enumerate()
        .filter(|(i, it)| {
            *it != "CONST_KW \"const\""
                && !(*it == "WHITESPACE \" \""
                    && comptime.get(i + 1).is_some_and(|n| n == "CONST_KW \"const\""))
        })
        .map(|(_, it)| it.clone())
        .collect();
    assert_eq!(stripped, plain);
}

#[test]
fn a_reflected_field_is_an_index_expr_with_a_dot() {
    let comptime = clean("fn f() { x.[f] = y.0.[g]; }");
    let plain = clean("fn f() { x[f] = y.0[g]; }");
    assert!(has_run(&comptime, &["INDEX_EXPR", "PATH_EXPR"]), "{comptime:#?}");
    assert!(has_run(&comptime, &["DOT \".\"", "L_BRACK \"[\""]), "{comptime:#?}");
    // `y.0.[g]`: the float `0.` is split, its `.` the index's.
    assert_eq!(comptime.iter().filter(|it| *it == "DOT \".\"").count(), 3, "{comptime:#?}");
    assert_eq!(comptime.iter().filter(|it| *it == "INDEX_EXPR").count(), 2, "{comptime:#?}");
    assert_eq!(without(&comptime, &["DOT \".\""]), without(&plain, &["DOT \".\""]));
}

#[test]
fn a_reflected_field_chains_as_a_postfix() {
    let tree = clean("fn f() { value.[f].hash(state); &mut x.[f]; x.[f].[g]; }");
    assert!(tree.iter().filter(|it| *it == "INDEX_EXPR").count() == 4, "{tree:#?}");
    assert!(has_run(&tree, &["METHOD_CALL_EXPR", "INDEX_EXPR"]), "{tree:#?}");
}

#[test]
fn a_where_clause_binder_takes_bounded_type_params() {
    let tree = clean(
        "fn f<T>() where for<F: Field<T>> F::Type: Hash + Eq, for<V: Variant<T>, F: Field<V>> F::Type: Debug {}",
    );
    assert!(
        has_run(
            &tree,
            &[
                "WHERE_PRED",
                "FOR_BINDER",
                "FOR_KW \"for\"",
                "GENERIC_PARAM_LIST",
                "L_ANGLE \"<\"",
                "TYPE_PARAM"
            ]
        ),
        "{tree:#?}"
    );
    // `fn f<T>`'s own `T`, then the binders' `F`, `V` and `F`, in source order; the `T` and `V`
    // in `Field<T>`, `Variant<T>` and `Field<V>` are arguments, not params.
    let params: Vec<&str> = tree
        .windows(3)
        .filter(|it| it[0] == "TYPE_PARAM" && it[1] == "NAME")
        .filter_map(|it| it[2].strip_prefix("IDENT \"")?.strip_suffix('"'))
        .collect();
    assert_eq!(params, ["T", "F", "V", "F"], "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "TYPE_PARAM").count(), 4, "{tree:#?}");
    assert_eq!(tree.iter().filter(|it| *it == "FOR_BINDER").count(), 2, "{tree:#?}");
}

/// Code without the extension parses as before: no `const` taken after `for` unless written,
/// no `.` taken before an index, and the near misses stay errors.
#[test]
fn code_without_the_extension_parses_as_before() {
    for text in
        ["fn f() { for x in [] {} }", "fn f() { x[1][2]; x.0[1]; }", "fn f() { for<'a> || (); }"]
    {
        let tree = clean(text);
        assert!(!tree.iter().any(|it| it == "CONST_KW \"const\""), "{text}: {tree:#?}");
        assert!(!has_run(&tree, &["DOT \".\"", "L_BRACK \"[\""]), "{text}: {tree:#?}");
    }
    let (_, errors) = parse_lines("fn f() { x.; }", Edition::CURRENT);
    assert!(!errors.is_empty(), "a `.` with nothing after it is still an error");
    let (_, errors) = parse_lines("fn f() { for const in x {} }", Edition::CURRENT);
    assert!(!errors.is_empty(), "`for const` still needs a pattern");
}

#[test]
fn const_static_is_a_block_expr_with_a_const_and_a_static_token() {
    let comptime = clean("fn f() { let x = const static { g() }; const static { h(); } }");
    let plain = clean("fn f() { let x = const { g() }; const { h(); } }");
    let run = ["BLOCK_EXPR", "CONST_KW \"const\"", "WHITESPACE \" \"", "STATIC_KW \"static\""];
    assert!(has_run(&comptime, &run), "{comptime:#?}");
    // In statement position too: a block, no item.
    assert_eq!(comptime.iter().filter(|it| *it == "BLOCK_EXPR").count(), 3, "{comptime:#?}");
    assert!(!comptime.iter().any(|it| it == "CONST" || it == "STATIC"), "{comptime:#?}");
    // Apart from `static` and its space, the same tree as the plain `const` blocks'.
    let stripped: Vec<_> = comptime
        .iter()
        .enumerate()
        .filter(|(i, it)| {
            *it != "STATIC_KW \"static\""
                && !(*it == "WHITESPACE \" \""
                    && comptime.get(i + 1).is_some_and(|n| n == "STATIC_KW \"static\""))
        })
        .map(|(_, it)| it.clone())
        .collect();
    assert_eq!(stripped, plain);
}

/// `const static` before a closure's `|`, and `const` and `static` items, parse as before.
#[test]
fn const_static_without_a_brace_is_as_before() {
    let tree =
        clean("const X: u8 = 1; static Y: u8 = 2; fn f() { const Z: u8 = 3; let c = const static || 1; }");
    assert!(tree.iter().any(|it| it == "CONST"), "{tree:#?}");
    assert!(tree.iter().any(|it| it == "STATIC"), "{tree:#?}");
    assert!(tree.iter().any(|it| it == "CLOSURE_EXPR"), "{tree:#?}");
    let blocks = tree.iter().filter(|it| *it == "BLOCK_EXPR").count();
    assert_eq!(blocks, 1, "the function's body, no `const static` block: {tree:#?}");
}
