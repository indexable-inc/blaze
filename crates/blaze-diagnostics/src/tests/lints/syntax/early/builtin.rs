//! The lints of rustc's `BuiltinCombinedEarlyLintPass` written in `rustc_lint/src/builtin.rs`,
//! `redundant_semicolon.rs` and `precedence.rs`, against rustc's UI tests.

use super::{ui_core, ui_std};
use crate::tests::{assert_reports, ui, warning};

#[test]
fn while_true_follows_the_level_of_warnings() {
    let source = r#"#![deny(warnings)]
#![allow(dead_code)]

fn main() {
    while true {} //~ ERROR: infinite
}

#[allow(warnings)]
fn foo() {
    while true {}
}

#[warn(warnings)]
fn bar() {
    while true {} //~ WARNING: infinite
}

#[forbid(warnings)]
fn baz() {
    while true {} //~ ERROR: infinite
}
"#;
    let stderr = r#"error: denote infinite loops with `loop { ... }`
  --> $DIR/lint-change-warnings.rs:5:5
   |
LL |     while true {}
   |     ^^^^^^^^^^ help: use `loop`
   |
note: the lint level is defined here
  --> $DIR/lint-change-warnings.rs:1:9
   |
LL | #![deny(warnings)]
   |         ^^^^^^^^
   = note: `#[deny(while_true)]` implied by `#[deny(warnings)]`

warning: denote infinite loops with `loop { ... }`
  --> $DIR/lint-change-warnings.rs:15:5
   |
LL |     while true {}
   |     ^^^^^^^^^^ help: use `loop`
   |
   = note: `#[warn(while_true)]` on by default

error: denote infinite loops with `loop { ... }`
  --> $DIR/lint-change-warnings.rs:20:5
   |
LL |     while true {}
   |     ^^^^^^^^^^ help: use `loop`
   |
note: the lint level is defined here
  --> $DIR/lint-change-warnings.rs:18:10
   |
LL | #[forbid(warnings)]
   |          ^^^^^^^^
   = note: `#[forbid(while_true)]` implied by `#[forbid(warnings)]`

error: aborting due to 2 previous errors; 1 warning emitted

"#;
    assert_eq!(ui("lint-change-warnings.rs", "2015", source), stderr);
}

#[test]
fn a_semicolon_after_an_item_statement_is_redundant() {
    let source = r#"#![deny(redundant_semicolons)]

fn main() {
    fn inner() {}; //~ ERROR unnecessary
    struct Bar {}; //~ ERROR unnecessary
}
"#;
    let stderr = r#"error: unnecessary trailing semicolon
  --> $DIR/item-stmt-semi.rs:4:18
   |
LL |     fn inner() {};
   |                  ^ help: remove this semicolon
   |
note: the lint level is defined here
  --> $DIR/item-stmt-semi.rs:1:9
   |
LL | #![deny(redundant_semicolons)]
   |         ^^^^^^^^^^^^^^^^^^^^

error: unnecessary trailing semicolon
  --> $DIR/item-stmt-semi.rs:5:18
   |
LL |     struct Bar {};
   |                  ^ help: remove this semicolon

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("item-stmt-semi.rs", "2015", source), stderr);
}

#[test]
fn mod_lib_and_mod_main_are_special_module_names() {
    let source = r#"mod lib;
//~^ WARN found module declaration for lib.rs
//~| ERROR file not found for module `lib`
mod main;
//~^ WARN found module declaration for main.rs
//~| ERROR file not found for module `main`

fn main() {}
"#;
    let stderr = r#"error[E0583]: file not found for module `lib`
  --> $DIR/special_module_name.rs:1:1
   |
LL | mod lib;
   | ^^^^^^^^
   |
   = help: to create the module `lib`, create file "$DIR/lib.rs" or "$DIR/lib/mod.rs"
   = note: if there is a `mod lib` elsewhere in the crate already, import it with `use crate::...` instead

error[E0583]: file not found for module `main`
  --> $DIR/special_module_name.rs:4:1
   |
LL | mod main;
   | ^^^^^^^^^
   |
   = help: to create the module `main`, create file "$DIR/main.rs" or "$DIR/main/mod.rs"
   = note: if there is a `mod main` elsewhere in the crate already, import it with `use crate::...` instead

warning: found module declaration for lib.rs
  --> $DIR/special_module_name.rs:1:1
   |
LL | mod lib;
   | ^^^^^^^^
   |
   = note: lib.rs is the root of this crate's library target
   = help: to refer to it from other targets, use the library's name as the path
   = note: `#[warn(special_module_name)]` on by default

warning: found module declaration for main.rs
  --> $DIR/special_module_name.rs:4:1
   |
LL | mod main;
   | ^^^^^^^^^
   |
   = note: a binary crate cannot be used as library

error: aborting due to 2 previous errors; 2 warnings emitted

For more information about this error, try `rustc --explain E0583`.
"#;
    assert_eq!(ui("special_module_name.rs", "2015", source), stderr);
}

#[test]
fn anonymous_parameters_of_trait_methods_are_deprecated_in_2015() {
    let source = r#"#![warn(anonymous_parameters)]
// Test for the anonymous_parameters deprecation lint (RFC 1685)

//@ check-pass
//@ edition:2015
//@ run-rustfix

#[allow(dead_code)]
trait T {
    fn foo(i32); //~ WARNING anonymous parameters are deprecated
                 //~| WARNING this is accepted in the current edition

    fn bar_with_default_impl(String, String) {}
    //~^ WARNING anonymous parameters are deprecated
    //~| WARNING this is accepted in the current edition
    //~| WARNING anonymous parameters are deprecated
    //~| WARNING this is accepted in the current edition
}

fn main() {}
"#;
    let std_items = "pub mod string {\n    pub struct String;\n}\n";
    let prelude = "pub use crate::string::String;";
    assert_reports(
        &ui_std("anon-params-deprecated.rs", "2015", "sized", (std_items, prelude), source),
        &[warning(10, &["`_: i32`"]), warning(13, &["`_: String`"]), warning(13, &["`_: String`"])],
    );
}

#[test]
fn an_ellipsis_range_is_deprecated_and_parenthesised_under_a_reference() {
    let source = r#"//@ edition:2015
//@ check-pass
//@ run-rustfix

#![warn(ellipsis_inclusive_range_patterns)]

fn main() {
    let despondency = 2;
    match despondency {
        1...2 => {}
        //~^ WARN `...` range patterns are deprecated
        //~| WARN this is accepted in the current edition
        _ => {}
    }

    match &despondency {
        &1...2 => {}
        //~^ WARN `...` range patterns are deprecated
        //~| WARN this is accepted in the current edition
        _ => {}
    }
}
"#;
    let stderr = r#"warning: `...` range patterns are deprecated
  --> $DIR/inclusive-range-pattern-syntax.rs:10:10
   |
LL |         1...2 => {}
   |          ^^^ help: use `..=` for an inclusive range
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/warnings-promoted-to-error.html>
note: the lint level is defined here
  --> $DIR/inclusive-range-pattern-syntax.rs:5:9
   |
LL | #![warn(ellipsis_inclusive_range_patterns)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: `...` range patterns are deprecated
  --> $DIR/inclusive-range-pattern-syntax.rs:17:9
   |
LL |         &1...2 => {}
   |         ^^^^^^ help: use `..=` for an inclusive range: `&(1..=2)`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/warnings-promoted-to-error.html>

warning: 2 warnings emitted

"#;
    assert_eq!(ui("inclusive-range-pattern-syntax.rs", "2015", source), stderr);
}

#[test]
fn an_ellipsis_range_is_an_error_from_2021() {
    let source = r#"//@ edition:2021

fn main() {
    let n = 2;
    match n {
        0...3 => {}
        //~^ ERROR `...` range patterns are deprecated
        4...10 => {}
        //~^ ERROR `...` range patterns are deprecated
        (11...100) => {}
        //~^ ERROR `...` range patterns are deprecated
        _ => {}
    }
}
"#;
    let stderr = r#"error[E0783]: `...` range patterns are deprecated
  --> $DIR/exclusive-range-patterns-2021.rs:6:9
   |
LL |         0...3 => {}
   |         ^---^
   |          |
   |          help: use `..=` for an inclusive range

error[E0783]: `...` range patterns are deprecated
  --> $DIR/exclusive-range-patterns-2021.rs:8:9
   |
LL |         4...10 => {}
   |         ^---^^
   |          |
   |          help: use `..=` for an inclusive range

error[E0783]: `...` range patterns are deprecated
  --> $DIR/exclusive-range-patterns-2021.rs:10:10
   |
LL |         (11...100) => {}
   |          ^^---^^^
   |            |
   |            help: use `..=` for an inclusive range

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0783`.
"#;
    assert_eq!(ui("exclusive-range-patterns-2021.rs", "2021", source), stderr);
}

#[test]
fn a_double_negation_is_linted_at_its_innermost_pair() {
    let source = r#"//@ check-pass
fn main() {
    let x = 1;
    -x;
    -(-x);
    --x; //~ WARN use of a double negation
    ---x; //~ WARN use of a double negation
    let _y = --(-x); //~ WARN use of a double negation
}
"#;
    assert_reports(
        &ui_core("lint-double-negations.rs", "2015", "copy, unary_ops, builtin_impls", source),
        &[
            warning(6, &[]),
            warning(7, &[]),
            warning(8, &[]),
        ],
    );
}

#[test]
fn an_unsafe_extern_block_is_unsafe_code() {
    let source = r#"#![deny(unsafe_code)]

#[allow(unsafe_code)]
unsafe extern "C" {
    fn foo();
}

unsafe extern "C" {
    //~^ ERROR usage of an `unsafe extern` block [unsafe_code]
    fn bar();
}

fn main() {}
"#;
    let stderr = r#"error: usage of an `unsafe extern` block
  --> $DIR/unsafe-extern-blocks.rs:8:1
   |
LL | / unsafe extern "C" {
LL | |
LL | |     fn bar();
LL | | }
   | |_^
   |
note: the lint level is defined here
  --> $DIR/unsafe-extern-blocks.rs:1:9
   |
LL | #![deny(unsafe_code)]
   |         ^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unsafe-extern-blocks.rs", "2015", source), stderr);
}
