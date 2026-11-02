//! The driver over whole crates: every error of every file given, whatever was edited, and
//! rustc's abort before its late passes.

use super::{assert_reports, checked, error, ui};
use crate::emit::Style;

/// #1522: an error in a `#[cfg(test)]` module is an error of the crate built for tests.
#[test]
fn a_missing_field_in_a_test_module_is_an_error() {
    let checked = checked(
        r#"
//- /lib.rs crate:main cfg:test
pub struct S { pub a: u32, pub b: u32 }

#[cfg(test)]
mod tests {
    fn t() -> super::S {
        super::S { a: 1 }
    }
}
"#,
    );
    assert_reports(&checked.stderr_with(Style::default()), &[error(6, &["`b`", "`S`"])]);
}

/// #780: the errors of a crate are those of all its files, not of the files an edit touched.
#[test]
fn an_error_in_a_module_file_is_the_crates() {
    let checked = checked(
        r#"
//- /main.rs crate:other:main
mod a;
fn main() {}
//- /a.rs
pub struct S { pub a: u32 }
pub fn f() -> S {
    S {}
}
"#,
    );
    assert_eq!(checked.diags[0].message, "missing field `a` in initializer of `S`");
    assert_eq!(checked.diags[0].span.primary[0].file, "a.rs");
}

/// rustc stops before its late passes once there is an error: no `dead_code` beside it.
#[test]
fn an_error_leaves_out_the_late_lints() {
    let checked = checked(
        r#"
//- /main.rs crate:other:main
struct S { a: u32 }
fn unused() {}
fn main() {
    let _ = S {};
}
"#,
    );
    assert_reports(&checked.stderr_with(Style::default()), &[error(4, &["`a`", "`S`"])]);
    let late = checked.messages().into_iter().filter(|it| it.starts_with("dead_code:")).collect::<Vec<_>>();
    assert!(late.is_empty(), "late lints after an error: {late:?}");
}

#[test]
fn without_errors_the_late_lints_are_given() {
    let checked = checked(
        r#"
//- /main.rs crate:other:main
fn unused() {}
fn main() {}
"#,
    );
    assert_eq!(checked.messages(), [
        "dead_code: function `unused` is never used",
    ]);
}

/// #1440: a call of a non-const function in a constant is refused, as rustc's
/// `tests/ui/consts/const-call.rs`.
#[test]
fn a_non_const_call_in_a_constant_is_refused() {
    let source = r#"fn f(x: usize) -> usize {
    x
}

fn main() {
    let _ = [0; f(2)];
    //~^ ERROR cannot call non-const function
}
"#;
    assert_reports(&ui("const-call.rs", "2015", source), &[error(6, &["`f`"])]);
}

/// rustc reads `#![recursion_limit]` before expansion and stops at its error
/// (`ShouldEmit::EarlyFatal`): the type error after it is never found.
/// `tests/ui/recursion/recursion_limit/invalid_digit_type.rs` with a body that does not type.
#[test]
fn an_early_fatal_error_stops_every_later_pass() {
    let source = r#"#![recursion_limit = 123] //~ ERROR malformed `recursion_limit` attribute

fn main() { let _x: u32 = "a"; }
"#;
    assert_eq!(ui("invalid_digit_type.rs", "2015", source), r#"error[E0539]: malformed `recursion_limit` attribute input
  --> $DIR/invalid_digit_type.rs:1:4
   |
LL | #![recursion_limit = 123]
   |    ^^^^^^^^^^^^^^^^^^---
   |                      |
   |                      expected a string literal here
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/limits.html#the-recursion_limit-attribute>
help: must be of the form
   |
LL - #![recursion_limit = 123]
LL + #![recursion_limit = "N"]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#);
}

/// What deciding lint levels says of an attribute after the fatal error (`unknown_lints`, which
/// rustc gives in its early lint pass, after expansion) is not written either.
#[test]
fn what_lint_attributes_say_after_a_fatal_error_is_not_written() {
    let source = r#"#![recursion_limit = 123] //~ ERROR malformed `recursion_limit` attribute

#[allow(this_lint_does_not_exist)]
fn main() {}
"#;
    assert_eq!(ui("invalid_digit_type.rs", "2015", source), r#"error[E0539]: malformed `recursion_limit` attribute input
  --> $DIR/invalid_digit_type.rs:1:4
   |
LL | #![recursion_limit = 123]
   |    ^^^^^^^^^^^^^^^^^^---
   |                      |
   |                      expected a string literal here
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/limits.html#the-recursion_limit-attribute>
help: must be of the form
   |
LL - #![recursion_limit = 123]
LL + #![recursion_limit = "N"]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#);
}
