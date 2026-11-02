//! rustc's UI tests of the lints `rustc_ast_passes` and the parser buffer (`varargs_without_pattern`,
//! `missing_unsafe_on_extern`, `patterns_in_fns_without_body`, `break_with_label_and_loop`,
//! `missing_abi`, `unused_visibilities`, `deprecated_where_clause_location`) and of
//! `non_camel_case_types`, the type parameters the early pass checks with the items the
//! declarations check gives.

use super::ui_std;
use crate::tests::{assert_reports, error, ui, warning};

const PARSE_ERRORS: &str = r#"//@ check-fail
//@ run-rustfix
//@ revisions: e2015 e2018
//@[e2015] edition: 2015
//@[e2018] edition: 2018
#![crate_type = "lib"]
#![deny(varargs_without_pattern)]

#[cfg(false)]
mod module {
    unsafe extern "C" fn f(...) {
    //~^ ERROR missing pattern for `...` argument
    //~| WARN this was previously accepted by the compiler
        unsafe extern "C" fn f(...) {}
        //~^ ERROR missing pattern for `...` argument
        //~| WARN this was previously accepted by the compiler
    }

    impl A {
        unsafe extern "C" fn f(...) {}
        //~^ ERROR missing pattern for `...` argument
        //~| WARN this was previously accepted by the compiler
    }

    trait A {
        unsafe extern "C" fn f(...) {}
        //[e2018]~^ ERROR missing pattern for `...` argument
        //[e2018]~| WARN this was previously accepted by the compiler
    }

    unsafe extern "C" {
        fn f(...); // no error
    }

    type A = unsafe extern "C" fn(...); // no error
}
"#;

#[test]
fn a_vararg_without_a_pattern_is_linted_in_cfg_removed_code_but_not_in_a_2015_trait() {
    // tests/ui/c-variadic/parse-errors.rs, revision e2015
    let stderr = r#"error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:11:28
   |
LL |     unsafe extern "C" fn f(...) {
   |                            ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |     unsafe extern "C" fn f(_: ...) {
   |                            ++

error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:14:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:20:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

error: aborting due to 3 previous errors

Future incompatibility report: Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:11:28
   |
LL |     unsafe extern "C" fn f(...) {
   |                            ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |     unsafe extern "C" fn f(_: ...) {
   |                            ++

Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:14:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:20:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

"#;
    assert_eq!(ui("parse-errors.rs", "2015", PARSE_ERRORS), stderr);
}

#[test]
fn a_vararg_without_a_pattern_is_linted_in_a_trait_from_2018() {
    // tests/ui/c-variadic/parse-errors.rs, revision e2018
    let stderr = r#"error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:11:28
   |
LL |     unsafe extern "C" fn f(...) {
   |                            ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |     unsafe extern "C" fn f(_: ...) {
   |                            ++

error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:14:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:20:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:26:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

error: aborting due to 4 previous errors

Future incompatibility report: Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:11:28
   |
LL |     unsafe extern "C" fn f(...) {
   |                            ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |     unsafe extern "C" fn f(_: ...) {
   |                            ++

Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:14:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:20:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

Future breakage diagnostic:
error: missing pattern for `...` argument
  --> $DIR/parse-errors.rs:26:32
   |
LL |         unsafe extern "C" fn f(...) {}
   |                                ^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #145544 <https://github.com/rust-lang/rust/issues/145544>
note: the lint level is defined here
  --> $DIR/parse-errors.rs:7:9
   |
LL | #![deny(varargs_without_pattern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
help: name the argument, or use `_` to continue ignoring it
   |
LL |         unsafe extern "C" fn f(_: ...) {}
   |                                ++

"#;
    assert_eq!(ui("parse-errors.rs", "2018", PARSE_ERRORS), stderr);
}

#[test]
fn an_extern_block_without_unsafe_is_linted_before_2024() {
    // tests/ui/rust-2024/unsafe-extern-blocks/unsafe-extern-suggestion.rs
    let source = r#"//@ edition:2015
//@ run-rustfix

#![deny(missing_unsafe_on_extern)]
#![allow(unused)]

extern "C" {
    //~^ ERROR extern blocks should be unsafe [missing_unsafe_on_extern]
    //~| WARN this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
    static TEST1: i32;
    fn test1(i: i32);
}

unsafe extern "C" {
    static TEST2: i32;
    fn test2(i: i32);
}

fn main() {}
"#;
    let stderr = r#"error: extern blocks should be unsafe
  --> $DIR/unsafe-extern-suggestion.rs:7:1
   |
LL |   extern "C" {
   |   ^
   |   |
   |  _help: needs `unsafe` before the extern keyword: `unsafe`
   | |
LL | |
LL | |
LL | |     static TEST1: i32;
LL | |     fn test1(i: i32);
LL | | }
   | |_^
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/unsafe-extern.html>
note: the lint level is defined here
  --> $DIR/unsafe-extern-suggestion.rs:4:9
   |
LL | #![deny(missing_unsafe_on_extern)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unsafe-extern-suggestion.rs", "2015", source), stderr);
}

#[test]
fn an_extern_block_without_unsafe_is_allowed_by_default() {
    // tests/ui/rust-2024/unsafe-extern-blocks/safe-unsafe-on-unadorned-extern-block.rs, revision
    // edition2021
    let source = r#"//@ revisions: edition2021 edition2024
//@[edition2021] edition:2021
//@[edition2024] edition:2024

extern "C" {
    //[edition2024]~^ ERROR extern blocks must be unsafe
    safe static TEST1: i32;
    //~^ ERROR items in `extern` blocks without an `unsafe` qualifier cannot have safety qualifiers
    safe fn test1(i: i32);
    //~^ ERROR items in `extern` blocks without an `unsafe` qualifier cannot have safety qualifiers
}

fn test2() {
    test1(TEST1);
}

fn main() {}
"#;
    let stderr = r#"error: items in `extern` blocks without an `unsafe` qualifier cannot have safety qualifiers
  --> $DIR/safe-unsafe-on-unadorned-extern-block.rs:7:5
   |
LL |     safe static TEST1: i32;
   |     ^^^^^^^^^^^^^^^^^^^^^^^
   |
help: add `unsafe` to this `extern` block
   |
LL | unsafe extern "C" {
   | ++++++

error: items in `extern` blocks without an `unsafe` qualifier cannot have safety qualifiers
  --> $DIR/safe-unsafe-on-unadorned-extern-block.rs:9:5
   |
LL |     safe fn test1(i: i32);
   |     ^^^^^^^^^^^^^^^^^^^^^^
   |
help: add `unsafe` to this `extern` block
   |
LL | unsafe extern "C" {
   | ++++++

error: aborting due to 2 previous errors

"#;
    assert_eq!(crate::tests::items::ui_core("safe-unsafe-on-unadorned-extern-block.rs", "2021", "copy", source), stderr);
}

#[test]
fn a_mut_parameter_of_a_function_without_a_body_is_linted_after_the_pattern_error() {
    // tests/ui/pattern/no-patterns-in-args-2.rs
    let source = r#"//@ edition:2015
#![deny(patterns_in_fns_without_body)]

trait Tr {
    fn f1(mut arg: u8); //~ ERROR patterns aren't allowed in functions without bodies
                        //~^ WARN was previously accepted
    fn f2(&arg: u8); //~ ERROR patterns aren't allowed in functions without bodies
    fn g1(arg: u8); // OK
    fn g2(_: u8); // OK
    #[allow(anonymous_parameters)]
    fn g3(u8); // OK
}

fn main() {}
"#;
    let stderr = r#"error[E0642]: patterns aren't allowed in functions without bodies
  --> $DIR/no-patterns-in-args-2.rs:7:11
   |
LL |     fn f2(&arg: u8);
   |           ^^^^ pattern not allowed in function without body

error: patterns aren't allowed in functions without bodies
  --> $DIR/no-patterns-in-args-2.rs:5:11
   |
LL |     fn f1(mut arg: u8);
   |           ^^^^^^^ help: remove `mut` from the parameter: `arg`
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #35203 <https://github.com/rust-lang/rust/issues/35203>
note: the lint level is defined here
  --> $DIR/no-patterns-in-args-2.rs:2:9
   |
LL | #![deny(patterns_in_fns_without_body)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0642`.
"#;
    assert_eq!(ui("no-patterns-in-args-2.rs", "2015", source), stderr);
}

#[test]
fn a_labeled_break_of_a_block_fragment_is_linted_at_the_macro_definition() {
    // tests/ui/parser/issues/issue-87812.rs
    let source = r#"#![deny(break_with_label_and_loop)]

macro_rules! foo {
    ( $f:block ) => {
        '_l: loop {
            break '_l $f; //~ERROR
        }
    };
}

fn main() {
    let x = foo!({ 3 });
}
"#;
    assert_reports(&ui("issue-87812.rs", "2015", source), &[error(6, &[])]);
}

#[test]
fn a_labeled_break_of_an_unsafe_block_is_not_linted() {
    // tests/ui/lint/break-with-label-and-unsafe-block.rs
    let source = r#"//@ check-pass

#![deny(break_with_label_and_loop)]

unsafe fn foo() -> i32 { 42 }

fn main () {
    'label: loop {
        break 'label unsafe { foo() }
    };
}
"#;
    assert_eq!(ui("break-with-label-and-unsafe-block.rs", "2015", source), "");
}

#[test]
fn a_labeled_break_of_an_unlabeled_loop_is_linted_after_the_parse_errors() {
    // tests/ui/parser/lifetime_starts_expressions.rs
    let source = r#"#![allow(unused, dead_code)]

fn foo() -> u32 {
    return 'label: loop { break 'label 42; };
}

fn bar() -> u32 {
    loop { break 'label: loop { break 'label 42; }; }
    //~^ ERROR: parentheses are required around this expression to avoid confusion
    //~| HELP: wrap the expression in parentheses
}

fn baz() -> u32 {
    'label: loop {
        break 'label
        //~^ WARNING: this labeled break expression is easy to confuse with an unlabeled break
            loop { break 42; };
            //~^ HELP: wrap this expression in parentheses
    };

    'label2: loop {
        break 'label2 'inner: loop { break 42; };
        // no warnings or errors here
    }
}

pub fn main() {
    // Regression test for issue #86948, as resolved in #87026:
    let a = 'first_loop: loop {
        break 'first_loop 1;
    };
    let b = loop {
        break 'inner_loop: loop {
        //~^ ERROR: parentheses are required around this expression to avoid confusion
        //~| HELP: wrap the expression in parentheses
            break 'inner_loop 1;
        };
    };
}
"#;
    assert_reports(
        &ui("lifetime_starts_expressions.rs", "2015", source),
        &[
            error(8, &[]),
            error(33, &[]),
            warning(15, &[]),
        ],
    );
}

#[test]
fn a_labeled_break_of_a_parenthesized_block_is_not_linted() {
    // tests/ui/lint/unused/break-label-with-parens-147542.rs
    let source = r#"//@ check-pass

// Regression test for #147542
// Ensures that we don't suggest removing parens in a break with label and loop
// when the parens are necessary for correct parsing.

#![warn(unused_parens)]
#![warn(break_with_label_and_loop)]

fn xyz() -> usize {
    'foo: {
        // parens bellow are necessary break of break with label and loop
        break 'foo ({
            println!("Hello!");
            123
        });
    }
}

fn main() {
    xyz();
}
"#;
    let std_items = r#"pub mod io {
    pub fn _print(_args: core::fmt::Arguments<'_>) {}
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print(core::format_args_nl!($($arg)*));
    }};
}
"#;
    let std = (std_items, "pub use crate::println;");
    assert_eq!(ui_std("break-label-with-parens-147542.rs", "2015", "sized, fmt, builtin_impls", std, source), "");
}

#[test]
fn an_extern_without_an_abi_is_linted_on_functions_and_blocks() {
    // tests/ui/feature-gates/feature-gate-explicit-extern-abis.rs, revision current
    let source = r#"// The purpose of this feature gate is to make something into a hard error in a
// future edition.  Consequently, this test differs from most other feature gate
// tests.  Instead of verifying that an error occurs when the feature gate is
// missing, it ensures that the hard error is only produced with the feature
// gate is present in the `future` edition -- and otherwise that only a warning
// is emitted.

//@ revisions: current current_feature future future_feature

//@ [current] run-rustfix
//@ [current] check-pass

//@ [current_feature] run-rustfix
//@ [current_feature] check-pass

//@ [future] edition: future
//@ [future] compile-flags: -Z unstable-options
//@ [future] run-rustfix
//@ [future] check-pass

//@ [future_feature] edition: future
//@ [future_feature] compile-flags: -Z unstable-options

#![cfg_attr(future_feature, feature(explicit_extern_abis))]

extern fn _foo() {}
//[current]~^ WARN `extern` declarations without an explicit ABI are deprecated
//[current_feature]~^^ WARN `extern` declarations without an explicit ABI are deprecated
//[future]~^^^ WARN `extern` declarations without an explicit ABI are deprecated
//[future_feature]~^^^^ ERROR `extern` declarations without an explicit ABI are disallowed

unsafe extern fn _bar() {}
//[current]~^ WARN `extern` declarations without an explicit ABI are deprecated
//[current_feature]~^^ WARN `extern` declarations without an explicit ABI are deprecated
//[future]~^^^ WARN `extern` declarations without an explicit ABI are deprecated
//[future_feature]~^^^^ ERROR `extern` declarations without an explicit ABI are disallowed

unsafe extern {}
//[current]~^ WARN `extern` declarations without an explicit ABI are deprecated
//[current_feature]~^^ WARN `extern` declarations without an explicit ABI are deprecated
//[future]~^^^ WARN `extern` declarations without an explicit ABI are deprecated
//[future_feature]~^^^^ ERROR `extern` declarations without an explicit ABI are disallowed

fn main() {}
"#;
    let stderr = r#"warning: `extern` declarations without an explicit ABI are deprecated
  --> $DIR/feature-gate-explicit-extern-abis.rs:26:1
   |
LL | extern fn _foo() {}
   | ^^^^^^ help: explicitly specify the "C" ABI: `extern "C"`
   |
   = note: `#[warn(missing_abi)]` on by default

warning: `extern` declarations without an explicit ABI are deprecated
  --> $DIR/feature-gate-explicit-extern-abis.rs:32:8
   |
LL | unsafe extern fn _bar() {}
   |        ^^^^^^ help: explicitly specify the "C" ABI: `extern "C"`

warning: `extern` declarations without an explicit ABI are deprecated
  --> $DIR/feature-gate-explicit-extern-abis.rs:38:8
   |
LL | unsafe extern {}
   |        ^^^^^^ help: explicitly specify the "C" ABI: `extern "C"`

warning: 3 warnings emitted

"#;
    assert_eq!(ui("feature-gate-explicit-extern-abis.rs", "2015", source), stderr);
}

#[test]
fn a_visibility_on_const_underscore_is_linted_unless_the_underscore_is_a_macro_argument() {
    // tests/ui/lint/unused-visibilities.rs
    let source = r#"//@ check-pass
//@ run-rustfix

#![warn(unused_visibilities)]

pub const _: () = {};
//~^WARN visibility qualifiers have no effect on `const _` declarations

pub(self) const _: () = {};
//~^WARN visibility qualifiers have no effect on `const _` declarations

macro_rules! foo {
    () => {
        pub const _: () = {};
        //~^WARN visibility qualifiers have no effect on `const _` declarations
    };
}

foo!();

macro_rules! bar {
    ($tt:tt) => {
        pub const $tt: () = {};
    };
}

bar!(_);

fn main() {}
"#;
    let stderr = r#"warning: visibility qualifiers have no effect on `const _` declarations
  --> $DIR/unused-visibilities.rs:6:1
   |
LL | pub const _: () = {};
   | ^^^ help: remove the qualifier
   |
   = note: `const _` does not declare a name, so there is nothing for the qualifier to apply to
note: the lint level is defined here
  --> $DIR/unused-visibilities.rs:4:9
   |
LL | #![warn(unused_visibilities)]
   |         ^^^^^^^^^^^^^^^^^^^

warning: visibility qualifiers have no effect on `const _` declarations
  --> $DIR/unused-visibilities.rs:9:1
   |
LL | pub(self) const _: () = {};
   | ^^^^^^^^^ help: remove the qualifier
   |
   = note: `const _` does not declare a name, so there is nothing for the qualifier to apply to

warning: visibility qualifiers have no effect on `const _` declarations
  --> $DIR/unused-visibilities.rs:14:9
   |
LL |         pub const _: () = {};
   |         ^^^ help: remove the qualifier
...
LL | foo!();
   | ------ in this macro invocation
   |
   = note: `const _` does not declare a name, so there is nothing for the qualifier to apply to
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: 3 warnings emitted

"#;
    assert_eq!(ui("unused-visibilities.rs", "2015", source), stderr);
}

#[test]
fn a_where_clause_before_an_impl_types_value_is_moved_after_it_or_removed() {
    // tests/ui/where-clauses/where-clause-placement-assoc-type-in-impl.rs
    let source = r#"//@ check-pass
//@ run-rustfix

#![allow(dead_code)]

trait Trait {
    // Fine.
    type Assoc where u32: Copy;
    // Fine.
    type Assoc2 where u32: Copy, i32: Copy;
    //
    type Assoc3;
}

impl Trait for u32 {
    // Not fine, suggests moving.
    type Assoc where u32: Copy = ();
    //~^ WARNING where clause not allowed here
    // Not fine, suggests moving `u32: Copy`
    type Assoc2 where u32: Copy = () where i32: Copy;
    //~^ WARNING where clause not allowed here
    type Assoc3 where = ();
    //~^ WARNING where clause not allowed here
}

impl Trait for i32 {
    // Fine.
    type Assoc = () where u32: Copy;
    // Not fine, suggests moving both.
    type Assoc2 where u32: Copy, i32: Copy = ();
    //~^ WARNING where clause not allowed here
    type Assoc3 where = () where;
    //~^ WARNING where clause not allowed here
}

fn main() {}
"#;
    let stderr = r#"warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-impl.rs:17:16
   |
LL |     type Assoc where u32: Copy = ();
   |                ^^^^^^^^^^^^^^^
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information
   = note: `#[warn(deprecated_where_clause_location)]` on by default
help: move it to the end of the type declaration
   |
LL -     type Assoc where u32: Copy = ();
LL +     type Assoc = () where u32: Copy;
   |

warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-impl.rs:20:17
   |
LL |     type Assoc2 where u32: Copy = () where i32: Copy;
   |                 ^^^^^^^^^^^^^^^
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information
help: move it to the end of the type declaration
   |
LL -     type Assoc2 where u32: Copy = () where i32: Copy;
LL +     type Assoc2 = () where i32: Copy, u32: Copy;
   |

warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-impl.rs:22:17
   |
LL |     type Assoc3 where = ();
   |                 ^^^^^
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information
help: move it to the end of the type declaration
   |
LL -     type Assoc3 where = ();
LL +     type Assoc3 = () where;
   |

warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-impl.rs:30:17
   |
LL |     type Assoc2 where u32: Copy, i32: Copy = ();
   |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information
help: move it to the end of the type declaration
   |
LL -     type Assoc2 where u32: Copy, i32: Copy = ();
LL +     type Assoc2 = () where u32: Copy, i32: Copy;
   |

warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-impl.rs:32:17
   |
LL |     type Assoc3 where = () where;
   |                 ^^^^^ help: remove this `where`
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information

warning: 5 warnings emitted

"#;
    assert_eq!(crate::tests::items::ui_core("where-clause-placement-assoc-type-in-impl.rs", "2015", "copy", source), stderr);
}

#[test]
fn a_where_clause_before_a_trait_types_default_is_moved_after_it() {
    // tests/ui/where-clauses/where-clause-placement-assoc-type-in-trait.rs
    let source = r#"//@ check-pass
//@ run-rustfix

#![allow(dead_code)]
#![feature(associated_type_defaults)]

trait Trait {
    // Not fine, suggests moving.
    type Assoc where u32: Copy = ();
    //~^ WARNING where clause not allowed here
    // Not fine, suggests moving `u32: Copy`
    type Assoc2 where u32: Copy = () where i32: Copy;
    //~^ WARNING where clause not allowed here
}

fn main() {}
"#;
    let stderr = r#"warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-trait.rs:9:16
   |
LL |     type Assoc where u32: Copy = ();
   |                ^^^^^^^^^^^^^^^
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information
   = note: `#[warn(deprecated_where_clause_location)]` on by default
help: move it to the end of the type declaration
   |
LL -     type Assoc where u32: Copy = ();
LL +     type Assoc = () where u32: Copy;
   |

warning: where clause not allowed here
  --> $DIR/where-clause-placement-assoc-type-in-trait.rs:12:17
   |
LL |     type Assoc2 where u32: Copy = () where i32: Copy;
   |                 ^^^^^^^^^^^^^^^
   |
   = note: see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information
help: move it to the end of the type declaration
   |
LL -     type Assoc2 where u32: Copy = () where i32: Copy;
LL +     type Assoc2 = () where i32: Copy, u32: Copy;
   |

warning: 2 warnings emitted

"#;
    assert_eq!(crate::tests::items::ui_core("where-clause-placement-assoc-type-in-trait.rs", "2015", "copy", source), stderr);
}

#[test]
fn a_type_parameter_is_linted_with_the_types_the_declarations_check_gives() {
    // tests/ui/lint/lint-non-camel-case-types.rs
    let source = r#"#![forbid(non_camel_case_types)]
#![allow(dead_code)]

struct ONE_TWO_THREE;
//~^ ERROR type `ONE_TWO_THREE` should have an upper camel case name

struct foo { //~ ERROR type `foo` should have an upper camel case name
    bar: isize,
}

enum foo2 { //~ ERROR type `foo2` should have an upper camel case name
    Bar
}

struct foo3 { //~ ERROR type `foo3` should have an upper camel case name
    bar: isize
}

type foo4 = isize; //~ ERROR type `foo4` should have an upper camel case name

enum Foo5 {
    bar //~ ERROR variant `bar` should have an upper camel case name
}

trait foo6 { //~ ERROR trait `foo6` should have an upper camel case name
    type foo7; //~ ERROR associated type `foo7` should have an upper camel case name
    fn dummy(&self) { }
}

fn f<ty>(_: ty) {} //~ ERROR type parameter `ty` should have an upper camel case name

#[repr(C)]
struct foo7 {
    bar: isize,
}

fn main() { }
"#;
    let stderr = r#"error: type `ONE_TWO_THREE` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:4:8
   |
LL | struct ONE_TWO_THREE;
   |        ^^^^^^^^^^^^^ help: convert the identifier to upper camel case: `OneTwoThree`
   |
note: the lint level is defined here
  --> $DIR/lint-non-camel-case-types.rs:1:11
   |
LL | #![forbid(non_camel_case_types)]
   |           ^^^^^^^^^^^^^^^^^^^^

error: type `foo` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:7:8
   |
LL | struct foo {
   |        ^^^ help: convert the identifier to upper camel case (notice the capitalization): `Foo`

error: type `foo2` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:11:6
   |
LL | enum foo2 {
   |      ^^^^ help: convert the identifier to upper camel case (notice the capitalization): `Foo2`

error: type `foo3` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:15:8
   |
LL | struct foo3 {
   |        ^^^^ help: convert the identifier to upper camel case (notice the capitalization): `Foo3`

error: type `foo4` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:19:6
   |
LL | type foo4 = isize;
   |      ^^^^ help: convert the identifier to upper camel case (notice the capitalization): `Foo4`

error: variant `bar` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:22:5
   |
LL |     bar
   |     ^^^ help: convert the identifier to upper camel case: `Bar`

error: trait `foo6` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:25:7
   |
LL | trait foo6 {
   |       ^^^^ help: convert the identifier to upper camel case (notice the capitalization): `Foo6`

error: associated type `foo7` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:26:10
   |
LL |     type foo7;
   |          ^^^^ help: convert the identifier to upper camel case (notice the capitalization): `Foo7`

error: type parameter `ty` should have an upper camel case name
  --> $DIR/lint-non-camel-case-types.rs:30:6
   |
LL | fn f<ty>(_: ty) {}
   |      ^^ help: convert the identifier to upper camel case: `Ty`

error: aborting due to 9 previous errors

"#;
    assert_eq!(ui("lint-non-camel-case-types.rs", "2015", source), stderr);
}

#[test]
fn an_allow_on_a_variant_silences_non_camel_case_types() {
    // tests/ui/lint/lint-non-camel-case-variant.rs
    let source = r#"//@ check-pass

#![deny(non_camel_case_types)]

pub enum Foo {
    #[allow(non_camel_case_types)]
    bar
}

fn main() {}
"#;
    assert_eq!(ui("lint-non-camel-case-variant.rs", "2015", source), "");
}

#[test]
fn a_trailing_underscore_is_upper_camel_case() {
    // tests/ui/lint/lint-non-camel-case-with-trailing-underscores.rs
    let source = r#"//@ check-pass

#![allow(dead_code)]
// This is ok because we often use the trailing underscore to mean 'prime'


#[forbid(non_camel_case_types)]
type Foo_ = isize;

pub fn main() { }
"#;
    assert_eq!(ui("lint-non-camel-case-with-trailing-underscores.rs", "2015", source), "");
}

#[test]
fn leading_and_trailing_underscores_are_upper_camel_case() {
    // tests/ui/lint/issue-54099-camel-case-underscore-types.rs
    let source = r#"//@ check-pass

#![forbid(non_camel_case_types)]
#![allow(dead_code)]

// None of the following types should generate a warning
struct _X {}
struct __X {}
struct __ {}
struct X_ {}
struct X__ {}
struct X___ {}

fn main() { }
"#;
    assert_eq!(ui("issue-54099-camel-case-underscore-types.rs", "2015", source), "");
}
