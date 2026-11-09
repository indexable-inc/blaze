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

/// blaze: a literal out of the range of an integer of `#![blaze::arbitrary_ints]` is an error
/// naming the literal as written, its type and the type's range, at every lint level: with
/// all of `TypeLimits`' lints allowed the pass still runs
/// (docs/v2/plan/design/arbitrary-ints.md).
#[test]
fn arbitrary_int_literal_out_of_range_is_refused() {
    use crate::tests::{Expect, assert_reports, error};
    let source = "#![blaze::arbitrary_ints]
fn f(x: u3) -> u3 { x }
fn main() {
    let a = f(9u3);
    let b: u3 = 8;
    let c: i3 = -5;
    let d = f(0b1000_u3);
    let ok = (f(7), -4i3, 3i3, 0u1, 1u1, -1i1, 0i1, 127u7, -64i7);
}
";
    let expected = [
        error(4, &["`9u3`", "`u3`", "`0..=7`"]),
        error(5, &["`8`", "`u3`", "`0..=7`"]),
        error(6, &["`-5`", "`i3`", "`-4..=3`"]),
        error(7, &["`0b1000_u3`", "`u3`", "`0..=7`"]),
    ];
    let stderr = ui(Ui {
        name: "arbitrary-ints.rs",
        core: "copy, unary_ops, builtin_impls",
        source,
        ..Ui::default()
    });
    assert_reports(&stderr, &expected);
    let allowed = "#![allow(overflowing_literals, unused_comparisons, invalid_nan_comparisons, \
                   ambiguous_wide_pointer_comparisons, unpredictable_function_pointer_comparisons)]\n";
    // The same lines, one lower: the `#![allow]` goes after the opt-in.
    let source = source.replacen('\n', &format!("\n{allowed}"), 1);
    let stderr = ui(Ui {
        name: "arbitrary-ints.rs",
        core: "copy, unary_ops, builtin_impls",
        source: &source,
        ..Ui::default()
    });
    let expected = expected.map(|it| Expect { line: it.line.map(|line| line + 1), ..it });
    assert_reports(&stderr, &expected);
}

/// blaze: as [`arbitrary_int_literal_out_of_range_is_refused`], of a pattern's literal.
#[test]
fn arbitrary_int_pattern_literal_out_of_range_is_refused() {
    use crate::tests::{assert_reports, error};
    let source = "#![blaze::arbitrary_ints]
fn f(x: u3, y: i3) -> u8 {
    let a: u8 = match x { 7 => 1, 8 => 2, _ => 0 };
    let b: u8 = match y { -4 => 1, -5 => 2, _ => 0 };
    a + b
}
fn main() { f(0, 0); }
";
    let stderr = ui(Ui {
        name: "arbitrary-ints-pat.rs",
        core: "copy, unary_ops, builtin_impls",
        source,
        ..Ui::default()
    });
    assert_reports(
        &stderr,
        &[error(3, &["`8`", "`u3`", "`0..=7`"]), error(4, &["`-5`", "`i3`", "`-4..=3`"])],
    );
}
