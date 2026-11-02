//! The errors of builtin macros and derives in rustc's words (`rustc_builtin_macros`), and the `;`
//! rustc ignores after the expression a macro other than a `macro_rules!` one expands to: each as
//! rustc prints it, its message and code, the text at each of its primary spans, and the notes and
//! helps under it. The macros' cases are rustc's `tests/ui/macros/macros-nonfatal-errors.rs`.
//! Also what a `macro_rules!` call no rule matches carries of the rule that got furthest, where a
//! macro fragment's node maps up to, and which identifiers `${concat(..)}` made.

use expect_test::{Expect, expect};
use hir_expand::{
    BuiltinChild, ExpandErrorKind, ExpandTo, FileRange, InFile, MacroCallId, MacroCallKind,
    MbeErrorKind, resolve_span,
};
use span::Span;
use stdx::format_to;
use syntax::{
    AstPtr, SyntaxKind, TextSize, ast as tag,
    view::{self as ast, AstNode},
};
use hir_expand::fixture;

use super::resolve_macro_call_id;
use crate::{
    nameres::{ModuleSourceView, crate_def_map},
    test_db::{TestDB, TestDBExt},
};

/// The text `span` covers.
fn text_at(db: &TestDB, span: Span) -> String {
    let FileRange { file_id, range } = resolve_span(db, span);
    file_id.parse(db).root().text()[range].to_owned()
}

/// The macro calls of the test crate: those written in its root file, in items and in bodies,
/// and its derives.
pub(super) fn macro_calls(db: &TestDB) -> Vec<MacroCallId> {
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(db, krate);
    let source = def_map.module(db, def_map.root).definition_source_view(db);
    let ModuleSourceView::SourceFile(file) = source.value else {
        panic!("the root module is a file")
    };
    let mut calls = Vec::new();
    for call in file.syntax().descendants().filter_map(ast::MacroCall::cast) {
        let ast_id = source.file_id.ast_id_map(db).ast_id_of_view::<tag::MacroCall>(call);
        let ast_id = InFile::new(source.file_id, ast_id);
        let ptr = InFile::new(source.file_id, AstPtr::<tag::MacroCall>::of(call));
        calls.extend(resolve_macro_call_id(db, def_map, ast_id, ptr));
    }
    let in_scopes = def_map.modules(db).flat_map(|(_, module)| module.scope(db).all_macro_calls());
    for call in in_scopes {
        if matches!(call.loc(db).kind, MacroCallKind::Derive { .. }) && !calls.contains(&call) {
            calls.push(call);
        }
    }
    calls
}

/// The error of each macro call of `ra_fixture` that has one, in the order of their spans.
#[track_caller]
fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(false);
    let db = fixture::db(ra_fixture).into_db();
    let mut errors: Vec<(TextSize, String)> = Vec::new();
    for call in macro_calls(&db) {
        let Some(err) = &call.parse_macro_expansion(&db).err else { continue };
        let rendered = err.render_to_string(&db);
        let mut out = String::new();
        match err.kind() {
            ExpandErrorKind::Builtin(e) => {
                let code = e.code.map(|it| format!("[E{it:04}]")).unwrap_or_default();
                format_to!(out, "{}{code}: {}\n", rendered.kind, e.message);
                for &span in std::iter::once(&err.span()).chain(e.also_at.iter()) {
                    format_to!(out, "  at `{}`\n", text_at(&db, span));
                }
                for (child, text) in &e.children {
                    let child = match child {
                        BuiltinChild::Note => "note",
                        BuiltinChild::Help => "help",
                    };
                    format_to!(out, "  = {child}: {text}\n");
                }
            }
            _ => {
                format_to!(out, "{}: {}\n", rendered.kind, rendered.message);
                format_to!(out, "  at `{}`\n", text_at(&db, err.span()));
            }
        }
        errors.push((resolve_span(&db, err.span()).range.start(), out));
    }
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(true);
    errors.sort_by_key(|(at, _)| *at);
    let errors: String = errors.into_iter().map(|(_, it)| it).collect();
    expect.assert_eq(&errors);
}

#[test]
fn env_option_env_and_include_arguments() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! env {() => {}}
#[rustc_builtin_macro]
macro_rules! option_env {() => {}}
#[rustc_builtin_macro]
macro_rules! include {() => {}}
#[rustc_builtin_macro]
macro_rules! include_str {() => {}}
#[rustc_builtin_macro]
macro_rules! include_bytes {() => {}}

fn main() {
    option_env!(invalid);
    env!(invalid);
    env!(foo, abr, baz);
    env!("RUST_HOPEFULLY_THIS_DOESNT_EXIST");
    include!(invalid);
    include_str!(invalid);
    include_bytes!(invalid);
}
"#,
        expect![[r#"
builtin-macro-error: argument must be a string literal
  at `invalid`
builtin-macro-error: expected string literal
  at `invalid`
builtin-macro-error: `env!()` takes 1 or 2 arguments
  at `env!(foo, abr, baz)`
builtin-macro-error: environment variable `RUST_HOPEFULLY_THIS_DOESNT_EXIST` not defined at compile time
  at `env!("RUST_HOPEFULLY_THIS_DOESNT_EXIST")`
  = help: use `std::env::var("RUST_HOPEFULLY_THIS_DOESNT_EXIST")` to read the variable at run time
builtin-macro-error: argument must be a string literal
  at `invalid`
builtin-macro-error: argument must be a string literal
  at `invalid`
builtin-macro-error: argument must be a string literal
  at `invalid`
"#]],
    );
}

#[test]
fn env_of_an_unset_variable() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! env {() => {}}
#[rustc_builtin_macro]
macro_rules! option_env {() => {}}
#[rustc_builtin_macro]
macro_rules! include_str {() => {}}

fn main() {
    env!("CARGO_PKG_VERSOIN");
    env!("DEP_BLAZE_TEST_UNSET_LINKS");
    env!("BLAZE_TEST_UNSET", "the message");
    env!(r"BLAZE_TEST_UNSET");
    env!(1);
    option_env!("BLAZE_TEST_UNSET", "two");
    include_str!("a", "b");
}
"#,
        expect![[r#"
builtin-macro-error: environment variable `CARGO_PKG_VERSOIN` not defined at compile time
  at `env!("CARGO_PKG_VERSOIN")`
  = help: there is a similar Cargo environment variable: `CARGO_PKG_VERSION`
builtin-macro-error: environment variable `DEP_BLAZE_TEST_UNSET_LINKS` not defined at compile time
  at `env!("DEP_BLAZE_TEST_UNSET_LINKS")`
  = help: `DEP_BLAZE_TEST_UNSET_LINKS` may not be available for the current Cargo target
  = help: Cargo sets build script variables at run time. Use `std::env::var("DEP_BLAZE_TEST_UNSET_LINKS")` instead
builtin-macro-error: the message
  at `env!("BLAZE_TEST_UNSET", "the message")`
builtin-macro-error: environment variable `BLAZE_TEST_UNSET` not defined at compile time
  at `env!(r"BLAZE_TEST_UNSET")`
  = help: use `std::env::var(r"BLAZE_TEST_UNSET")` to read the variable at run time
builtin-macro-error: expected string literal
  at `1`
builtin-macro-error: option_env! takes 1 argument
  at `option_env!("BLAZE_TEST_UNSET", "two")`
builtin-macro-error: include_str! takes 1 argument
  at `include_str!("a", "b")`
"#]],
    );
}

#[test]
fn line_and_column_take_no_arguments() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! line {() => {}}
#[rustc_builtin_macro]
macro_rules! column {() => {}}

fn main() {
    line!(1);
    column!(x);
}
"#,
        expect![[r#"
builtin-macro-error: line! takes no arguments
  at `line!(1)`
builtin-macro-error: column! takes no arguments
  at `column!(x)`
"#]],
    );
}

#[test]
fn concat_errors() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! concat {() => {}}

fn main() {
    concat!("a", b'b');
    concat!("a", c"c");
    concat!(x, 1, y);
}
"#,
        expect![[r#"
builtin-macro-error: cannot concatenate a byte string literal
  at `b'b'`
builtin-macro-error: cannot concatenate a C string literal
  at `c"c"`
builtin-macro-error: expected a literal
  at `x`
  at `y`
  = note: only literals (like `"foo"`, `-42` and `3.14`) can be passed to `concat!()`
"#]],
    );
}

#[test]
fn concat_bytes_errors() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! concat_bytes {() => {}}

fn main() {
    concat_bytes!("a");
    concat_bytes!([b"a"]);
    concat_bytes!([256]);
    concat_bytes!([1u16]);
    concat_bytes!(1);
    concat_bytes!(true);
    concat_bytes!(x);
}
"#,
        expect![[r#"
builtin-macro-error: cannot concatenate string literals
  at `"a"`
builtin-macro-error: cannot concatenate doubly nested array
  at `b"a"`
  = note: byte strings are treated as arrays of bytes
  = help: try flattening the array
builtin-macro-error: numeric literal is out of bounds
  at `256`
builtin-macro-error: numeric literal is not a `u8`
  at `1u16`
builtin-macro-error: cannot concatenate numeric literals
  at `1`
builtin-macro-error: cannot concatenate boolean literals
  at `true`
builtin-macro-error: expected a byte literal
  at `x`
  = note: only byte literals (like `b"foo"`, `b's'` and `[3, 4, 5]`) can be passed to `concat_bytes!()`
"#]],
    );
}

#[test]
fn concat_bytes_literal_has_the_call_span() {
    let db = fixture::db(
        r#"
#[rustc_builtin_macro]
macro_rules! concat_bytes {() => {}}

fn main() { concat_bytes!(b'A', [66]); }
"#,
    )
    .into_db();
    let [call] = macro_calls(&db)[..] else { panic!("one call") };
    let (parse, map) = &call.parse_macro_expansion(&db).value;
    let literal = parse
        .root()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .find(|it| it.kind() == SyntaxKind::BYTE_STRING)
        .expect("a byte string");
    let span = map.span_at(literal.text_range().start());
    assert_eq!(literal.text(), r#"b"AB""#);
    assert_eq!(text_at(&db, span), "concat_bytes");
}

#[test]
fn a_semicolon_after_an_expression_expansion() {
    check(
        r#"
//- proc_macros: mirror
macro_rules! m { () => { 1; } }

fn main() {
    let x = proc_macros::mirror!(; 1);
    let y = m!();
}
"#,
        expect![[r#"
builtin-macro-error: macro expansion ignores `;` and any tokens following
  at `;`
  = note: the usage of `proc_macros::mirror!` is likely invalid in expression context
"#]],
    );
}

/// rustc's `parse_stmt_mac`: the last call of a block, not in braces and with no `;` after it,
/// is an expression, so statements are not what it can expand to.
#[test]
fn a_blocks_last_call_expands_to_an_expression() {
    let db = fixture::db(
        r#"
macro_rules! m { () => { let x = 1; x } }

fn f() -> i32 { m!() }
fn g() { m!(); }
fn h() -> i32 { m! {} }
"#,
    )
    .into_db();
    let mut found = Vec::new();
    for call in macro_calls(&db) {
        let MacroCallKind::FnLike { expand_to, .. } = call.loc(&db).kind else {
            panic!("the calls are function-like")
        };
        let parse_errors = !call.parse_macro_expansion(&db).value.0.errors().is_empty();
        found.push((expand_to, parse_errors));
    }
    assert_eq!(
        found,
        [(ExpandTo::Expr, true), (ExpandTo::Statements, false), (ExpandTo::Statements, false)]
    );
}

#[test]
fn the_dropped_semicolon_is_in_the_expansion_span_map() {
    let text = r#"
macro_rules! m { () => { 1; } }
macro_rules! n { () => { 1 } }

fn main() {
    let y = m!();
    let z = n!();
}
"#;
    let db = fixture::db(text).into_db();
    let mut found = Vec::new();
    for call in macro_calls(&db) {
        let map = call.expansion_span_map(&db);
        found.push(map.dropped_semicolon.map(|span| {
            let FileRange { file_id, range } = resolve_span(&db, span);
            let written = file_id.parse(&db).root().text();
            let body = written.find("1; }").expect("m's body");
            (usize::from(range.start()) - body, text_at(&db, span))
        }));
    }
    assert_eq!(found, [Some((1, ";".to_owned())), None]);
}

/// rustc's `failed_to_match_macro`: the token the rule that got furthest stopped at, what it
/// was trying to match there, and whether a `,` makes a rule match, carried in the error's kind.
/// rustc's `expand_include_str` refuses a file that isn't UTF-8, at the call.
#[test]
fn include_str_of_a_file_that_is_not_utf_8_is_an_error() {
    let file = std::env::temp_dir().join(format!("blaze-include-str-{}.bin", std::process::id()));
    std::fs::write(&file, [b'a', 0xff, 0xfe]).expect("the temporary file");
    let path = file.to_str().expect("a UTF-8 temporary path");
    let text = format!(
        r#"
#[rustc_builtin_macro]
macro_rules! include_str {{() => {{}}}}

fn main() {{
    include_str!({path:?});
}}
"#
    );
    let db = fixture::db(&text).into_db();
    let errors: Vec<_> = macro_calls(&db)
        .into_iter()
        .filter_map(|call| call.parse_macro_expansion(&db).err.clone())
        .map(|err| (err.render_to_string(&db).message, text_at(&db, err.span())))
        .collect();
    std::fs::remove_file(&file).expect("the temporary file");
    assert_eq!(
        errors,
        [(format!("`{path}` wasn't a utf-8 file"), format!("include_str!({path:?})"))]
    );
}

#[test]
fn a_call_no_rule_matches_carries_the_best_failure() {
    let text = r#"
macro_rules! m { ($a:ident ,) => {} }
macro_rules! n { ($a:ident, $b:ident) => {} }
m!(x);
n!(x y);
"#;
    let db = fixture::db(text).into_db();
    let mut found = Vec::new();
    for call in macro_calls(&db) {
        let err = call.parse_macro_expansion(&db).err.clone().expect("no rule matches");
        let ExpandErrorKind::Mbe(MbeErrorKind::MatchFailure(failure)) = err.kind() else {
            panic!("a match failure, not {:?}", err.kind())
        };
        let comma = failure.missing_comma.map(|span| {
            let FileRange { file_id, range } = resolve_span(&db, span);
            assert!(range.is_empty(), "the comma's place is empty");
            let written = file_id.parse(&db).root().text();
            let call = written.find("n!(x y)").expect("n's call");
            written[call..usize::from(range.start())].to_owned()
        });
        found.push((
            failure.token.as_deref().map(str::to_owned),
            text_at(&db, err.span()),
            failure.label(),
            failure.remaining.to_string(),
            failure.remaining_span.map(|span| text_at(&db, span)),
            comma,
        ));
    }
    assert_eq!(
        found,
        [
            (
                None,
                ")".to_owned(),
                "missing tokens in macro arguments",
                "`,`".to_owned(),
                Some(",".to_owned()),
                None,
            ),
            (
                Some("`y`".to_owned()),
                "y".to_owned(),
                "no rules expected this token in macro call",
                "`,`".to_owned(),
                Some(",".to_owned()),
                Some("n!(x".to_owned()),
            ),
        ]
    );
}

/// The node that is exactly a `$s:stmt` fragment maps up to the metavariable in the rule (its
/// name, with or without the `$`), not to the call's tokens: where a lint of the statement
/// written by the rule points.
#[test]
fn a_fragments_node_maps_up_to_its_metavariable() {
    let text = r#"
macro_rules! m { ($s:stmt) => { $s } }

fn main() {
    m!(let x = 1);
}
"#;
    let db = fixture::db(text).into_db();
    let [call] = macro_calls(&db)[..] else { panic!("one call") };
    let (parse, map) = &call.parse_macro_expansion(&db).value;
    let stmt = parse
        .root()
        .descendants()
        .find(|it| it.kind() == SyntaxKind::LET_STMT)
        .expect("a `let` statement");
    let (FileRange { file_id, range }, _) =
        hir_expand::map_node_range_up(&db, map, stmt.text_range()).expect("the node maps up");
    let written = file_id.parse(&db).root().text();
    let dollar = written.find("{ $s }").expect("m's body") + 2;
    let (start, end) = (usize::from(range.start()), usize::from(range.end()));
    assert!(
        (dollar..=dollar + 1).contains(&start) && end == dollar + 2,
        "the statement maps up to {:?}, not to the `$s`",
        &written[range]
    );
}

/// The lexer lints skip an identifier `${concat(..)}` made, so the expansion keeps which.
#[test]
fn the_identifiers_concat_made_are_in_the_expansion_span_map() {
    let text = r#"
macro_rules! m { ($a:ident) => { let ${concat($a, é)} = 1; } }

fn main() {
    m!(x);
}
"#;
    let db = fixture::db(text).into_db();
    let [call] = macro_calls(&db)[..] else { panic!("one call") };
    let (parse, map) = &call.parse_macro_expansion(&db).value;
    let made = parse
        .root()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .find(|it| it.kind() == SyntaxKind::IDENT && it.text() == "xé")
        .expect("the made identifier");
    assert_eq!(&*map.made_idents, [map.span_at(made.text_range().start())]);
}

#[test]
fn from_derive_errors() {
    check(
        r#"
//- minicore: derive_from
use core::from::From;

#[derive(From)]
enum Either { A(u8) }

#[derive(From)]
struct Two(u8, u8);
"#,
        expect![[r#"
builtin-derive-error: `#[derive(From)]` used on an enum
  at `From`
  at `Either`
  = note: `#[derive(From)]` can only be used on structs with exactly one field
builtin-derive-error: `#[derive(From)]` used on a struct with multiple fields
  at `From`
  at `Two`
  = note: `#[derive(From)]` can only be used on structs with exactly one field
"#]],
    );
}

#[test]
fn coerce_pointee_errors_have_their_code() {
    check(
        r#"
//- minicore: coerce_pointee
use core::marker::CoercePointee;

#[derive(CoercePointee)]
enum Enum {}

#[derive(CoercePointee)]
#[repr(transparent)]
struct Two<#[pointee] T: ?Sized, #[pointee] U: ?Sized>(T, U);

#[derive(CoercePointee)]
#[repr(transparent)]
struct NotMaybe<T>(T);

#[derive(CoercePointee)]
struct NotTransparent<T: ?Sized>(T);
"#,
        expect![[r#"
builtin-derive-error[E0802]: `CoercePointee` can only be derived on `struct`s with `#[repr(transparent)]`
  at `CoercePointee`
builtin-derive-error[E0802]: only one type parameter can be marked as `#[pointee]` when deriving `CoercePointee` traits
  at `T`
builtin-derive-error[E0802]: `derive(CoercePointee)` requires `T` to be marked `?Sized`
  at `T`
builtin-derive-error[E0802]: `derive(CoercePointee)` is only applicable to `struct` with `repr(transparent)` layout
  at `CoercePointee`
"#]],
    );
}
