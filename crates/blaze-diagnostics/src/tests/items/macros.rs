//! rustc's UI tests of macros: calls of macros that do not exist, `compile_error!`,
//! `macro_rules!` definitions rustc refuses, and expansions that do not parse.

use crate::tests::{assert_reports, error, ui, ui_builtins};

#[test]
fn an_undefined_macro_suggests_the_similar_one() {
    // tests/ui/macros/macro_undefined.rs
    let source = r#"// Test macro_undefined issue

mod m {
    #[macro_export]
    macro_rules! kl {
        () => ()
    }
}

fn main() {
    k!(); //~ ERROR cannot find
    kl!();
}
"#;
    assert_reports(&ui("macro_undefined.rs", "2015", source), &[error(11, &["`k`"])]);
}

#[test]
fn a_macro_that_does_not_exist_is_not_found() {
    // tests/ui/resolve/nonexistent-macro.rs
    let source = r#"//! Test error handling for undefined macro calls

fn main() {
    iamnotanextensionthatexists!("");
    //~^ ERROR cannot find macro `iamnotanextensionthatexists` in this scope
}
"#;
    assert_reports(&ui("nonexistent-macro.rs", "2015", source), &[error(4, &["`iamnotanextensionthatexists`"])]);
}

#[test]
fn compile_error_is_an_error_at_the_call() {
    // tests/ui/macros/compile_error_macro.rs
    let source = r#"fn main() {
    compile_error!("a very descriptive error message"); //~ ERROR: a very descriptive error message
}
"#;
    assert_reports(&ui_builtins("compile_error_macro.rs", "2015", source), &[error(2, &["a very descriptive error message"])]);
}

#[test]
fn a_matcher_must_be_delimited() {
    // tests/ui/macros/invalid-macro-matcher.rs
    let source = r#"#![allow(unused_macros)]

macro_rules! invalid {
    _ => (); //~ ERROR invalid macro matcher
}

fn main() {
}
"#;
    let stderr = r#"error: invalid macro matcher; matchers must be contained in balanced delimiters
  --> $DIR/invalid-macro-matcher.rs:4:5
   |
LL |     _ => ();
   |     ^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("invalid-macro-matcher.rs", "2015", source), stderr);
}

#[test]
fn a_call_of_a_macro_rustc_refused_to_define_is_not_an_error_again() {
    // tests/ui/macros/malformed_macro_lhs.rs
    let source = r#"macro_rules! my_precioooous {
    t => (1); //~ ERROR invalid macro matcher
}

fn main() {
    my_precioooous!();
}
"#;
    let stderr = r#"error: invalid macro matcher; matchers must be contained in balanced delimiters
  --> $DIR/malformed_macro_lhs.rs:2:5
   |
LL |     t => (1);
   |     ^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("malformed_macro_lhs.rs", "2015", source), stderr);
}

#[test]
fn a_rule_with_no_right_hand_side_ends_the_definition() {
    // tests/ui/parser/macro/bad-macro-definition.rs, its first macro
    let source = r#"#![crate_type = "lib"]

macro_rules! a { {} => }
//~^ ERROR: macro definition ended unexpectedly
"#;
    let stderr = r#"error: macro definition ended unexpectedly
  --> $DIR/bad-macro-definition.rs:3:23
   |
LL | macro_rules! a { {} => }
   |                       ^ expected right-hand side of macro rule

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("bad-macro-definition.rs", "2015", source), stderr);
}

#[test]
fn a_rule_with_no_arrow_expects_one() {
    // tests/ui/parser/macro/bad-macro-definition.rs, its macro `e` alone
    let source = r#"#![crate_type = "lib"]

macro_rules! e { {} }
//~^ ERROR: expected `=>`, found end of macro arguments
"#;
    let stderr = r#"error: expected `=>`, found end of macro arguments
  --> $DIR/bad-macro-definition.rs:3:20
   |
LL | macro_rules! e { {} }
   |                    ^ expected `=>`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("bad-macro-definition.rs", "2015", source), stderr);
}

#[test]
fn a_macro_with_no_rules_is_refused() {
    // tests/ui/parser/issues/issue-7970b.rs: mbe defines it, rustc does not.
    let source = r#"fn main() {}

macro_rules! test {}
//~^ ERROR macros must contain at least one rule
"#;
    let stderr = r#"error: macros must contain at least one rule
  --> $DIR/issue-7970b.rs:3:1
   |
LL | macro_rules! test {}
   | ^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-7970b.rs", "2015", source), stderr);
}

#[test]
fn an_unknown_fragment_specifier_is_refused() {
    // tests/ui/macros/invalid-fragment-specifier.rs
    let source = r#"macro_rules! test {
    ($wrong:id) => {};
} //~^ ERROR: invalid fragment specifier `id`

// guard against breaking raw identifier diagnostic
macro_rules! test_raw_identifer {
    ($wrong:r#if) => {};
} //~^ ERROR: invalid fragment specifier `r#if`

fn main() {}
"#;
    let stderr = r#"error: invalid fragment specifier `id`
  --> $DIR/invalid-fragment-specifier.rs:2:6
   |
LL |     ($wrong:id) => {};
   |      ^^^^^^^^^
   |
   = help: valid fragment specifiers are `ident`, `block`, `stmt`, `expr`, `pat`, `ty`, `lifetime`, `literal`, `path`, `meta`, `tt`, `item` and `vis`, along with `expr_2021` and `pat_param` for edition compatibility

error: invalid fragment specifier `r#if`
  --> $DIR/invalid-fragment-specifier.rs:7:6
   |
LL |     ($wrong:r#if) => {};
   |      ^^^^^^^^^^^
   |
   = help: valid fragment specifiers are `ident`, `block`, `stmt`, `expr`, `pat`, `ty`, `lifetime`, `literal`, `path`, `meta`, `tt`, `item` and `vis`, along with `expr_2021` and `pat_param` for edition compatibility

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("invalid-fragment-specifier.rs", "2015", source), stderr);
}

#[test]
fn an_expansion_that_ends_inside_an_expression_says_so_at_its_last_token() {
    // tests/ui/parser/issues/issue-68092-unicode-ident-after-incomplete-expr.rs
    let source = r#"macro_rules! x {
    ($($c:tt)*) => {
        $($c)ö*
    };
}

fn main() {
    x!(!); //~ ERROR macro expansion ends with an incomplete expression: expected expression
}
"#;
    assert_reports(&ui("issue-68092-unicode-ident-after-incomplete-expr.rs", "2015", source), &[error(8, &[])]);
}
