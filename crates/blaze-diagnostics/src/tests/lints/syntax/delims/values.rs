//! Parentheses around the value an expression takes: an assigned value, an argument, a
//! condition, a scrutinee, a `return` or `break` value and a block's tail.

use super::ui_core;
use crate::{
    Options,
    tests::{assert_reports, error, ui, ui_run_pass},
};

#[test]
fn parentheses_around_an_assigned_value_are_removed_without_a_trailing_space() {
    let source = r#"fn main() {
    #[deny(unused_parens)]
    let _x = (3 + 6);
    //~^ ERROR unnecessary parentheses around assigned value
}
"#;
    let expected = r#"error: unnecessary parentheses around assigned value
  --> $DIR/unused-parens-trailing-space-issue-158583.rs:3:14
   |
LL |     let _x = (3 + 6);
   |              ^     ^
   |
note: the lint level is defined here
  --> $DIR/unused-parens-trailing-space-issue-158583.rs:2:12
   |
LL |     #[deny(unused_parens)]
   |            ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -     let _x = (3 + 6);
LL +     let _x = 3 + 6;
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unused-parens-trailing-space-issue-158583.rs", "2015", source), expected);
}

/// The test's stderr is rustc's JSON; this is its `rendered` text, the human form.
#[test]
fn only_the_outer_parentheses_are_removed() {
    let source = r#"//@ compile-flags: --error-format json
//@ run-rustfix

// The output for humans should just highlight the whole span without showing
// the suggested replacement, but we also want to test that suggested
// replacement only removes one set of parentheses, rather than naïvely
// stripping away any starting or ending parenthesis characters—hence this
// test of the JSON error format.

#![deny(unused_parens)]
#![allow(unreachable_code, unused_braces)]

fn main() {
    // We want to suggest the properly-balanced expression `1 / (2 + 3)`, not
    // the malformed `1 / (2 + 3`
    let _a = (1 / (2 + 3));
    //~^ ERROR unnecessary parentheses around assigned value
    f();
}

fn f() -> bool {
    loop {
        if (break { return true }) {
        }
    }
    false
}
"#;
    let expected = r#"error: unnecessary parentheses around assigned value
  --> $DIR/unused_parens_json_suggestion.rs:16:14
   |
LL |     let _a = (1 / (2 + 3));
   |              ^           ^
   |
note: the lint level is defined here
  --> $DIR/unused_parens_json_suggestion.rs:10:9
   |
LL | #![deny(unused_parens)]
   |         ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -     let _a = (1 / (2 + 3));
LL +     let _a = 1 / (2 + 3);
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unused_parens_json_suggestion.rs", "2015", source), expected);
}

#[test]
fn a_space_is_kept_between_a_name_and_an_argument_a_macro_was_handed() {
    let source = r#"//@ run-rustfix

#![deny(unused_parens)]

macro_rules! wrap {
    ($name:ident $arg:expr) => {
        $name($arg);
    };
}

fn main() {
    wrap!(unary(routine())); //~ ERROR unnecessary parentheses around function argument
    wrap!(unary (routine())); //~ ERROR unnecessary parentheses around function argument
}

fn unary(_: ()) {}
fn routine() {}
"#;
    assert_reports(
        &ui("unused_parens_follow_ident.rs", "2015", source),
        &[error(12, &[]), error(13, &[])],
    );
}

#[test]
fn a_for_iterator_ending_in_a_loop_or_match_needs_no_parentheses() {
    let source = r#"// Make sure unused parens lint emit is emitted for loop and match.
// See https://github.com/rust-lang/rust/issues/90807
// and https://github.com/rust-lang/rust/pull/91956#discussion_r771647953
#![deny(unused_parens)]

fn main() {
    for _ in (1..loop { break 2 }) {} //~ERROR
    for _ in (1..match () { () => 2 }) {} //~ERROR
}
"#;
    assert_reports(
        &ui_core("issue-90807-unused-paren-error.rs", "2015", "iterator, range", source),
        &[error(7, &["`for`"]), error(8, &["`for`"])],
    );
}

#[test]
fn a_for_iterator_ending_in_a_block_needs_its_parentheses() {
    let source = r#"//@ check-pass
// Make sure unused parens lint doesn't emit a false positive.
// See https://github.com/rust-lang/rust/issues/90807
#![deny(unused_parens)]

fn main() {
    for _ in (1..{ 2 }) {}
}
"#;
    assert_reports(&ui_core("issue-90807-unused-paren.rs", "2015", "iterator, range", source), &[]);
}

#[test]
fn an_assignment_returned_or_broken_with_keeps_its_parentheses() {
    let source = r#"//@ run-rustfix
#![deny(unused_parens)]
#![allow(unreachable_code)]

fn foo() {
    loop {
        break (_ = 42);
        // lint unused_parens should not be triggered here.
    }

    let _ = loop {
        let a = 1;
        let b = 2;
        break (a + b); //~ERROR unnecessary parentheses
    };

    loop {
        if (break return ()) {
            //~^ ERROR unnecessary parentheses
        }
        if break (return ()) {
            //~^ ERROR unnecessary parentheses
        }
    }

    return (_ = 42);
    // lint unused_parens should not be triggered here.
}

fn main() {
    let _ = foo();
}
"#;
    let expected = r#"error: unnecessary parentheses around `break` value
  --> $DIR/unused-parens-assign-expr-in-ret-issue-131989.rs:14:15
   |
LL |         break (a + b);
   |               ^     ^
   |
note: the lint level is defined here
  --> $DIR/unused-parens-assign-expr-in-ret-issue-131989.rs:2:9
   |
LL | #![deny(unused_parens)]
   |         ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -         break (a + b);
LL +         break a + b;
   |

error: unnecessary parentheses around `if` condition
  --> $DIR/unused-parens-assign-expr-in-ret-issue-131989.rs:18:12
   |
LL |         if (break return ()) {
   |            ^               ^
   |
help: remove these parentheses
   |
LL -         if (break return ()) {
LL +         if break return () {
   |

error: unnecessary parentheses around `break` value
  --> $DIR/unused-parens-assign-expr-in-ret-issue-131989.rs:21:18
   |
LL |         if break (return ()) {
   |                  ^         ^
   |
help: remove these parentheses
   |
LL -         if break (return ()) {
LL +         if break return () {
   |

error: aborting due to 3 previous errors

"#;
    assert_eq!(
        ui("unused-parens-assign-expr-in-ret-issue-131989.rs", "2015", source),
        expected
    );
}

#[test]
fn a_scrutinee_with_a_struct_literal_keeps_its_parentheses() {
    let source = r#"#![allow(dead_code)]
#![deny(unused_parens)]

enum State {
    Waiting { start_at: u64 }
}
struct Foo {}

fn main() {
    let e = &mut State::Waiting { start_at: 0u64 };
    match (&mut State::Waiting { start_at: 0u64 }) {
        _ => {}
    }

    match (e) {
        //~^ ERROR unnecessary parentheses around `match` scrutinee expression
        _ => {}
    }

    match &(State::Waiting { start_at: 0u64 }) {
        _ => {}
    }

    match (State::Waiting { start_at: 0u64 }) {
        _ => {}
    }

    match (&&Foo {}) {
        _ => {}
    }
}
"#;
    let expected = r#"error: unnecessary parentheses around `match` scrutinee expression
  --> $DIR/lint-struct-necessary.rs:15:11
   |
LL |     match (e) {
   |           ^ ^
   |
note: the lint level is defined here
  --> $DIR/lint-struct-necessary.rs:2:9
   |
LL | #![deny(unused_parens)]
   |         ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -     match (e) {
LL +     match e {
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("lint-struct-necessary.rs", "2015", source), expected);
}

#[test]
fn a_block_before_an_operator_keeps_its_parentheses() {
    let source = r#"//@ check-pass
// Make sure unused parens lint doesn't emit a false positive.
// See https://github.com/rust-lang/rust/issues/71290 for details.
#![deny(unused_parens)]

fn x() -> u8 {
    ({ 0 }) + 1
}

fn y() -> u8 {
    ({ 0 } + 1)
}

pub fn foo(a: bool, b: bool) -> u8 {
    (if a { 1 } else { 0 } + if b { 1 } else { 0 })
}

pub fn bar() -> u8 {
    // Make sure nested expressions are handled correctly as well
    ({ 0 } + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9)
}

fn main() {}
"#;
    assert_eq!(ui("issue-71290-unused-paren-binop.rs", "2015", source), "");
}

#[test]
fn a_return_before_a_block_keeps_its_parentheses() {
    let source = r#"//@ run-pass

#![deny(unused_parens)]
#![allow(unreachable_code)]

fn main() {
    match (return) {} // ok
    if (return) {} // ok
}
"#;
    assert_eq!(ui_run_pass("no-unused-parens-return-block.rs", "2015", source, &Options::default()), "");
}

#[test]
fn a_labelled_break_of_a_name_before_a_block_keeps_its_parentheses() {
    let source = r#"//@ check-pass
// testcase for https://github.com/rust-lang/rust/issues/143256

#![deny(unused_parens)]
#![allow(unreachable_code, unused_variables, dead_code)]

fn foo() {
    let _x = || 'outer: loop {
        let inner = 'inner: loop {
            let i = Default::default();
            // the parentheses here are necessary
            if (break 'outer i) {
                loop {
                    break 'inner 5i8;
                }
            } else if true {
                break 'inner 6;
            }
            break 7;
        };
        break inner < 8;
    };
}

fn main() {}
"#;
    assert_reports(
        &ui_core("unused-parens-labeled-break-issue-143256.rs", "2015", "default, ord, builtin_impls", source),
        &[
        ],
    );
}

#[test]
fn a_braced_macro_call_before_else_keeps_its_parentheses() {
    let source = r#"//@ run-rustfix

#![deny(unused_parens)]

fn main() {
    macro_rules! x {
        () => { None::<i32> };
    }

    let Some(_) = (x!{}) else { return }; // no error
    let Some(_) = ((x!{})) else { return };
    //~^ ERROR: unnecessary parentheses around assigned value

    let Some((_)) = (x!{}) else { return };
    //~^ ERROR: unnecessary parentheses around pattern

    let _ = x!{};
    let _ = (x!{});
    //~^ ERROR: unnecessary parentheses around assigned value

    if let Some(_) = x!{} {};
    if let Some(_) = (x!{}) {};
    //~^ ERROR: unnecessary parentheses around `let` scrutinee expression

    while let Some(_) = x!{} {};
    while let Some(_) = (x!{}) {};
    //~^ ERROR: unnecessary parentheses around `let` scrutinee expression
}
"#;
    let expected = r#"error: unnecessary parentheses around assigned value
  --> $DIR/unused-parens-for-macro-call-with-brace.rs:11:19
   |
LL |     let Some(_) = ((x!{})) else { return };
   |                   ^      ^
   |
note: the lint level is defined here
  --> $DIR/unused-parens-for-macro-call-with-brace.rs:3:9
   |
LL | #![deny(unused_parens)]
   |         ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -     let Some(_) = ((x!{})) else { return };
LL +     let Some(_) = (x!{}) else { return };
   |

error: unnecessary parentheses around pattern
  --> $DIR/unused-parens-for-macro-call-with-brace.rs:14:14
   |
LL |     let Some((_)) = (x!{}) else { return };
   |              ^ ^
   |
help: remove these parentheses
   |
LL -     let Some((_)) = (x!{}) else { return };
LL +     let Some(_) = (x!{}) else { return };
   |

error: unnecessary parentheses around assigned value
  --> $DIR/unused-parens-for-macro-call-with-brace.rs:18:13
   |
LL |     let _ = (x!{});
   |             ^    ^
   |
help: remove these parentheses
   |
LL -     let _ = (x!{});
LL +     let _ = x!{};
   |

error: unnecessary parentheses around `let` scrutinee expression
  --> $DIR/unused-parens-for-macro-call-with-brace.rs:22:22
   |
LL |     if let Some(_) = (x!{}) {};
   |                      ^    ^
   |
help: remove these parentheses
   |
LL -     if let Some(_) = (x!{}) {};
LL +     if let Some(_) = x!{} {};
   |

error: unnecessary parentheses around `let` scrutinee expression
  --> $DIR/unused-parens-for-macro-call-with-brace.rs:26:25
   |
LL |     while let Some(_) = (x!{}) {};
   |                         ^    ^
   |
help: remove these parentheses
   |
LL -     while let Some(_) = (x!{}) {};
LL +     while let Some(_) = x!{} {};
   |

error: aborting due to 5 previous errors

"#;
    assert_eq!(
        ui_core("unused-parens-for-macro-call-with-brace.rs", "2015", "option", source),
        expected
    );
}

/// The parser's recovery leaves the parentheses unclosed, and there is nothing to lint.
#[test]
fn unclosed_parentheses_are_not_linted() {
    let source = r#"// ignore-tidy-file-trailing-newlines

// Verify that unused parens lint does not try to create a span
// which points in the middle of a multibyte character.

//~v ERROR this file contains an unclosed delimiter
fn f(){(print!(á"#;
    let expected = r#"error: this file contains an unclosed delimiter
  --> $DIR/unused_parens_multibyte_recovery.rs:7:17
   |
LL | fn f(){(print!(á
   |       --      - ^
   |       ||      |
   |       ||      unclosed delimiter
   |       |unclosed delimiter
   |       unclosed delimiter

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unused_parens_multibyte_recovery.rs", "2015", source), expected);
}
