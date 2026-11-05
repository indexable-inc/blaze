//! What rustc lints on a macro's definition, against rustc's UI tests in `tests/ui/macros`
//! (MIT or Apache-2.0), kept byte for byte in `matchers/`.

use super::ui_fixture;
use crate::{
    Options,
    tests::{Mode, ui},
};

#[test]
fn a_variable_repeating_with_another_operator_is_linted() {
    let source = include_str!("matchers/issue-61053-different-kleene.rs");
    let expected = include_str!("matchers/issue-61053-different-kleene.stderr");
    assert_eq!(ui("issue-61053-different-kleene.rs", "2015", source), expected);
}

#[test]
fn a_variable_bound_twice_in_a_macro_a_transcriber_defines_is_linted() {
    let source = include_str!("matchers/issue-61053-duplicate-binder.rs");
    let expected = include_str!("matchers/issue-61053-duplicate-binder.stderr");
    assert_eq!(ui("issue-61053-duplicate-binder.rs", "2015", source), expected);
}

#[test]
fn a_variable_still_repeating_is_linted() {
    let source = include_str!("matchers/issue-61053-missing-repetition.rs");
    let expected = include_str!("matchers/issue-61053-missing-repetition.stderr");
    assert_eq!(ui("issue-61053-missing-repetition.rs", "2015", source), expected);
}

#[test]
fn an_unbound_variable_is_linted() {
    let source = include_str!("matchers/issue-61053-unbound.rs");
    let expected = include_str!("matchers/issue-61053-unbound.stderr");
    assert_eq!(ui("issue-61053-unbound.rs", "2015", source), expected);
}

#[test]
fn an_expr_before_2024_is_linted() {
    let source = include_str!("matchers/expr_2021_cargo_fix_edition.rs");
    let expected = include_str!("matchers/expr_2021_cargo_fix_edition.stderr");
    assert_eq!(ui("expr_2021_cargo_fix_edition.rs", "2021", source), expected);
}

/// The test's `main` needs std (`Result`, `assert_eq!`, `Into`); it keeps the call of the
/// other crate's macro, whose `pat` is not linted.
#[test]
fn a_pat_before_2021_that_an_or_may_follow_is_linted() {
    let source = include_str!("matchers/macro-or-patterns-back-compat.rs");
    let main = "fn main() {
    let result: Result<i64, i32> = Err(42);
    let int: i64 = match_any!(result, Ok(i) | Err(i) => i.into());
    assert_eq!(int, 42);
    a!(1|);
}
";
    assert!(source.ends_with(main));
    let source = source.replace(main, "fn main() {\n    a!(1|);\n}\n");
    let aux = include_str!("matchers/auxiliary/or-pattern.rs");
    let fixture = format!(
        "//- /macro-or-patterns-back-compat.rs crate:other:main edition:2015 deps:or_pattern\n{source}\
         //- /auxiliary/or-pattern.rs crate:or_pattern edition:2015\n{aux}"
    );
    let expected = include_str!("matchers/macro-or-patterns-back-compat.stderr");
    assert_eq!(ui_fixture(Mode::Other, &fixture, &Options::default()), expected);
}
