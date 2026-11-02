//! `TypeLimits`: rustc's UI tests of `overflowing_literals` and `unused_comparisons`, in `ui/`
//! byte for byte as `tests/ui/lint/` has them (rust-lang/rust `ad3d0bc141a`, MIT or
//! Apache-2.0).

use super::{Ui, ui};

/// rustc's core as far as the tests reach it: primitives that are `Copy`, and `-` of a signed
/// integer or a float through `Neg`.
fn check(name: &str, flags: &[&str], source: &str, stderr: &str) {
    const CORE: &str = "copy, unary_ops, builtin_impls";
    let actual = ui(Ui { name, core: CORE, flags, source, ..Ui::default() });
    assert_eq!(actual, stderr, "{name}");
}

#[test]
fn lint_type_limits() {
    check(
        "lint-type-limits.rs",
        &["-D unused-comparisons"],
        include_str!("limits/ui/lint-type-limits.rs"),
        include_str!("limits/ui/lint-type-limits.stderr"),
    );
}

#[test]
fn lint_type_limits2() {
    check(
        "lint-type-limits2.rs",
        &["-D unused-comparisons"],
        include_str!("limits/ui/lint-type-limits2.rs"),
        include_str!("limits/ui/lint-type-limits2.stderr"),
    );
}

#[test]
fn lint_type_limits3() {
    check(
        "lint-type-limits3.rs",
        &["-D unused-comparisons"],
        include_str!("limits/ui/lint-type-limits3.rs"),
        include_str!("limits/ui/lint-type-limits3.stderr"),
    );
}

#[test]
fn type_overflow() {
    check(
        "type-overflow.rs",
        &[],
        include_str!("limits/ui/type-overflow.rs"),
        include_str!("limits/ui/type-overflow.stderr"),
    );
}

#[test]
fn lint_type_overflow2() {
    check(
        "lint-type-overflow2.rs",
        &[],
        include_str!("limits/ui/lint-type-overflow2.rs"),
        include_str!("limits/ui/lint-type-overflow2.stderr"),
    );
}

#[test]
fn lint_overflowing_int_136675() {
    check(
        "lint-overflowing-int-136675.rs",
        &[],
        include_str!("limits/ui/lint-overflowing-int-136675.rs"),
        include_str!("limits/ui/lint-overflowing-int-136675.stderr"),
    );
}
