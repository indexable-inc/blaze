//! The lints of macros and their expansions, against rustc's UI tests.

mod asm;
mod matchers;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, error, library_features, ui, ui_core_as, warning},
};

/// [`crate::tests::ui_as`] of a fixture of several files: a UI test, its auxiliary crates
/// and the files it includes, each named `$DIR/<path>`, run as compiletest runs a test of
/// `mode` with the test's own flags in `options`.
fn ui_fixture(mode: Mode, fixture: &str, options: &Options) -> String {
    let options = crate::tests::compiletest(mode, options);
    let stderr = crate::tests::checked_with(fixture, "$DIR/", &options)
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

#[test]
fn a_removed_expression_in_trailing_position_is_linted() {
    let source = r#"// Ensures that we properly lint
// a removed 'expression' resulting from a macro
// in trailing expression position

macro_rules! expand_it {
    () => {
        #[cfg(false)] 25; //~  ERROR trailing semicolon in macro
                          //~| WARN this was previously
    }
}

fn main() {
    expand_it!()
}
"#;
    let expected = r#"error: trailing semicolon in macro used in expression position
  --> $DIR/lint-trailing-macro-call.rs:7:25
   |
LL |         #[cfg(false)] 25;
   |                         ^
...
LL |     expand_it!()
   |     ------------ in this macro invocation
   |
   = note: macro invocations at the end of a block are treated as expressions
   = note: to ignore the value produced by the macro, add a semicolon after the invocation of `expand_it`
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: `#[deny(semicolon_in_expressions_from_macros)]` (part of `#[deny(future_incompatible)]`) on by default
   = note: this error originates in the macro `expand_it` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

Future incompatibility report: Future breakage diagnostic:
error: trailing semicolon in macro used in expression position
  --> $DIR/lint-trailing-macro-call.rs:7:25
   |
LL |         #[cfg(false)] 25;
   |                         ^
...
LL |     expand_it!()
   |     ------------ in this macro invocation
   |
   = note: macro invocations at the end of a block are treated as expressions
   = note: to ignore the value produced by the macro, add a semicolon after the invocation of `expand_it`
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: `#[deny(semicolon_in_expressions_from_macros)]` (part of `#[deny(future_incompatible)]`) on by default
   = note: this error originates in the macro `expand_it` (in Nightly builds, run with -Z macro-backtrace for more info)

"#;
    assert_eq!(ui("lint-trailing-macro-call.rs", "2015", source), expected);
}

#[test]
fn a_semicolon_in_an_expression_is_denied_by_default() {
    let source = r#"// Ensure that trailing semicolons cause errors by default

macro_rules! foo {
    () => {
        true; //~  ERROR trailing semicolon in macro
              //~| WARN this was previously
    }
}

fn main() {
    let _val = match true {
        true => false,
        _ => foo!()
    };
}
"#;
    let expected = r#"error: trailing semicolon in macro used in expression position
  --> $DIR/warn-semicolon-in-expressions-from-macros.rs:5:13
   |
LL |         true;
   |             ^
...
LL |         _ => foo!()
   |              ------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: `#[deny(semicolon_in_expressions_from_macros)]` (part of `#[deny(future_incompatible)]`) on by default
   = note: this error originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

Future incompatibility report: Future breakage diagnostic:
error: trailing semicolon in macro used in expression position
  --> $DIR/warn-semicolon-in-expressions-from-macros.rs:5:13
   |
LL |         true;
   |             ^
...
LL |         _ => foo!()
   |              ------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: `#[deny(semicolon_in_expressions_from_macros)]` (part of `#[deny(future_incompatible)]`) on by default
   = note: this error originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

"#;
    assert_eq!(ui("warn-semicolon-in-expressions-from-macros.rs", "2015", source), expected);
}

#[test]
fn a_semicolon_in_an_anon_const_is_linted() {
    let source = r#"// Regression test for issue #84195
// Checks that we properly fire lints that occur inside
// anon consts.

#![deny(semicolon_in_expressions_from_macros)]

macro_rules! len {
    () => { 0; }; //~  ERROR trailing semicolon
                  //~| WARN this was previously accepted
}

fn main() {
    let val: [u8; len!()] = [];
}
"#;
    let expected = r#"error: trailing semicolon in macro used in expression position
  --> $DIR/issue-84195-lint-anon-const.rs:8:14
   |
LL |     () => { 0; };
   |              ^
...
LL |     let val: [u8; len!()] = [];
   |                   ------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/issue-84195-lint-anon-const.rs:5:9
   |
LL | #![deny(semicolon_in_expressions_from_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this error originates in the macro `len` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

Future incompatibility report: Future breakage diagnostic:
error: trailing semicolon in macro used in expression position
  --> $DIR/issue-84195-lint-anon-const.rs:8:14
   |
LL |     () => { 0; };
   |              ^
...
LL |     let val: [u8; len!()] = [];
   |                   ------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/issue-84195-lint-anon-const.rs:5:9
   |
LL | #![deny(semicolon_in_expressions_from_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this error originates in the macro `len` (in Nightly builds, run with -Z macro-backtrace for more info)

"#;
    assert_eq!(ui("issue-84195-lint-anon-const.rs", "2015", source), expected);
}

#[test]
fn a_semicolon_from_another_crates_macro_is_linted_at_the_call() {
    let fixture = r#"//- /auxiliary/foreign-crate.rs crate:foreign_crate edition:2015 library
#[macro_export]
macro_rules! my_macro {
    () => { true; }
}
//- /foreign-crate.rs crate:other:main edition:2015 deps:foreign_crate
//@ aux-build:foreign-crate.rs
#![deny(semicolon_in_expressions_from_non_local_macros)]

extern crate foreign_crate;

// Test that we fail with a macro in a foreign crate
fn main() {
    let _ = foreign_crate::my_macro!();
    //~^ ERROR trailing semicolon in macro used in expression position
    //~| WARN this was previously accepted
}
"#;
    let expected = r#"error: trailing semicolon in macro used in expression position
  --> $DIR/foreign-crate.rs:8:13
   |
LL |     let _ = foreign_crate::my_macro!();
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/foreign-crate.rs:2:9
   |
LL | #![deny(semicolon_in_expressions_from_non_local_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this error originates in the macro `foreign_crate::my_macro` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

Future incompatibility report: Future breakage diagnostic:
error: trailing semicolon in macro used in expression position
  --> $DIR/foreign-crate.rs:8:13
   |
LL |     let _ = foreign_crate::my_macro!();
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/foreign-crate.rs:2:9
   |
LL | #![deny(semicolon_in_expressions_from_non_local_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this error originates in the macro `foreign_crate::my_macro` (in Nightly builds, run with -Z macro-backtrace for more info)

"#;
    assert_eq!(ui_fixture(Mode::Other, fixture, &Options::default()), expected);
}

#[test]
fn the_level_is_that_of_the_node_around_the_call() {
    let source = r#"//@ check-pass
//@ edition:2018
#![feature(stmt_expr_attributes)]
#![warn(semicolon_in_expressions_from_macros)]

#[allow(dead_code)]
macro_rules! foo {
    ($val:ident) => {
        true; //~  WARN trailing semicolon in macro
              //~| WARN this was previously accepted
              //~| WARN trailing semicolon in macro
              //~| WARN this was previously accepted
              //~| WARN trailing semicolon in macro
              //~| WARN this was previously accepted
    }
}

#[allow(semicolon_in_expressions_from_macros)]
async fn bar() {
    foo!(first);
}

fn main() {
    #[allow(semicolon_in_expressions_from_macros)]
    let _ = {
        foo!(first)
    };

    #[allow(semicolon_in_expressions_from_macros)]
    let _ = foo!(second);

    #[allow(semicolon_in_expressions_from_macros)]
    fn inner() {
        let _ = foo!(third);
    }

    #[allow(semicolon_in_expressions_from_macros)]
    async {
        let _ = foo!(fourth);
    };

    let _ = {
        foo!(warn_in_block)
    };

    let _ = foo!(warn_in_expr);

    // This `#[allow]` does not work, since the attribute gets dropped
    // when we expand the macro
    let _ = #[allow(semicolon_in_expressions_from_macros)] foo!(allow_does_not_work);
    //~^ WARN unused attribute
}
"#;
    let expected = r#"warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |         foo!(warn_in_block)
   |         ------------------- in this macro invocation
   |
   = note: macro invocations at the end of a block are treated as expressions
   = note: to ignore the value produced by the macro, add a semicolon after the invocation of `foo`
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/semicolon-in-expressions-from-macros.rs:4:9
   |
LL | #![warn(semicolon_in_expressions_from_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |     let _ = foo!(warn_in_expr);
   |             ------------------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unused attribute `allow`
  --> $DIR/semicolon-in-expressions-from-macros.rs:50:13
   |
LL |     let _ = #[allow(semicolon_in_expressions_from_macros)] foo!(allow_does_not_work);
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
note: the built-in attribute `allow` will be ignored, since it's applied to the macro invocation `foo`
  --> $DIR/semicolon-in-expressions-from-macros.rs:50:60
   |
LL |     let _ = #[allow(semicolon_in_expressions_from_macros)] foo!(allow_does_not_work);
   |                                                            ^^^
   = note: requested on the command line with `-W unused-attributes`

warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |     let _ = #[allow(semicolon_in_expressions_from_macros)] foo!(allow_does_not_work);
   |                                                            ------------------------- in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: 4 warnings emitted

Future incompatibility report: Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |         foo!(first)
   |         ----------- in this macro invocation
   |
   = note: macro invocations at the end of a block are treated as expressions
   = note: to ignore the value produced by the macro, add a semicolon after the invocation of `foo`
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |     let _ = foo!(second);
   |             ------------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |         let _ = foo!(third);
   |                 ----------- in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |         let _ = foo!(fourth);
   |                 ------------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |         foo!(warn_in_block)
   |         ------------------- in this macro invocation
   |
   = note: macro invocations at the end of a block are treated as expressions
   = note: to ignore the value produced by the macro, add a semicolon after the invocation of `foo`
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/semicolon-in-expressions-from-macros.rs:4:9
   |
LL | #![warn(semicolon_in_expressions_from_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |     let _ = foo!(warn_in_expr);
   |             ------------------ in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/semicolon-in-expressions-from-macros.rs:4:9
   |
LL | #![warn(semicolon_in_expressions_from_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

Future breakage diagnostic:
warning: trailing semicolon in macro used in expression position
  --> $DIR/semicolon-in-expressions-from-macros.rs:9:13
   |
LL |         true;
   |             ^
...
LL |     let _ = #[allow(semicolon_in_expressions_from_macros)] foo!(allow_does_not_work);
   |                                                            ------------------------- in this macro invocation
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #79813 <https://github.com/rust-lang/rust/issues/79813>
note: the lint level is defined here
  --> $DIR/semicolon-in-expressions-from-macros.rs:4:9
   |
LL | #![warn(semicolon_in_expressions_from_macros)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: this warning originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

"#;
    // A core with `Future`, as rustc has std: the test's `async fn`s are lowered with it.
    let stderr = ui_core_as(Mode::Other, "semicolon-in-expressions-from-macros.rs", "2018", "future", source, &Options::default());
    assert_eq!(stderr, expected);
}

#[test]
fn an_include_of_more_than_one_expression_is_denied() {
    let fixture = r#"//- minicore: include
//- /include-single-expr.rs crate:other:main edition:2015
fn main() {
    include!("include-single-expr-helper.rs");
    include!("include-single-expr-helper-1.rs");
}

//~? ERROR include macro expected single expression
//- /include-single-expr-helper.rs
//@ ignore-auxiliary (used by `./include-single-expr.rs`)

0
10
100
//- /include-single-expr-helper-1.rs
//@ ignore-auxiliary (used by `./include-single-expr.rs`)

0

// trailing comment permitted
"#;
    let expected = r#"error: include macro expected single expression in source
  --> $DIR/include-single-expr-helper.rs:4:1
   |
LL | 10
   | ^^
   |
   = note: `#[deny(incomplete_include)]` on by default

error: aborting due to 1 previous error

"#;
    assert_reports(&ui_fixture(Mode::Other, fixture, &Options::default()), &[error(4, &[])]);
}

#[test]
fn lints_of_expansion_are_leveled_where_the_macro_is() {
    let fixture = r#"//- minicore: include
//- /expansion-time.rs crate:other:main edition:2015
//@ check-pass

#[warn(meta_variable_misuse)]
macro_rules! foo {
    ( $($i:ident)* ) => { $($i)+ }; //~ WARN meta-variable repeats with different Kleene operator
}

#[deprecated = "reason"]
macro_rules! deprecated {
    () => {}
}

#[allow(deprecated)]
mod deprecated {
    deprecated!(); // No warning
}

#[warn(incomplete_include)]
fn main() {
    // WARN see in the stderr file, the warning points to the included file.
    include!("expansion-time-include.rs");
}

//~? WARN include macro expected single expression in source
//- /expansion-time-include.rs
//@ ignore-auxiliary (used by `./expansion-time.rs`)

1
2
"#;
    let expected = r#"warning: meta-variable repeats with different Kleene operator
  --> $DIR/expansion-time.rs:5:29
   |
LL |     ( $($i:ident)* ) => { $($i)+ };
   |                  -          ^^ - conflicting repetition
   |                  |
   |                  expected repetition
   |
note: the lint level is defined here
  --> $DIR/expansion-time.rs:3:8
   |
LL | #[warn(meta_variable_misuse)]
   |        ^^^^^^^^^^^^^^^^^^^^

warning: include macro expected single expression in source
  --> $DIR/expansion-time-include.rs:4:1
   |
LL | 2
   | ^
   |
note: the lint level is defined here
  --> $DIR/expansion-time.rs:18:8
   |
LL | #[warn(incomplete_include)]
   |        ^^^^^^^^^^^^^^^^^^

warning: 2 warnings emitted

"#;
    assert_reports(&ui_fixture(Mode::Other, fixture, &Options::default()), &[warning(5, &[]), warning(4, &[])]);
}

#[test]
fn binders_of_nested_macro_definitions_are_not_misuse() {
    let fixture = r#"//- minicore: eq, add
//- /std.rs crate:std edition:2021 library
pub macro assert_eq($left:expr, $right:expr $(,)?) {
    let _: bool = $left == $right;
}
pub mod prelude {
    pub mod rust_2015 {
        pub use crate::assert_eq;
    }
}
//- /meta-variable-misuse.rs crate:other:main edition:2015 deps:std
//@ run-pass
#![deny(meta_variable_misuse)]

macro_rules! foo {
    ($($m:ident $($f:ident $v:tt)+),*) => {
        $($(macro_rules! $f { () => { $v } })+)*
        $(macro_rules! $m { () => { $(fn $f() -> i32 { $v })+ } })*
    }
}

foo!(m a 1 b 2, n c 3);
m!();
n!();

macro_rules! no_shadow {
    ($x:tt) => { macro_rules! bar { ($x:tt) => { 42 }; } };
}
no_shadow!(z);

macro_rules! make_plus {
    ($n: ident $x:expr) => { macro_rules! $n { ($y:expr) => { $x + $y }; } };
}
make_plus!(add3 3);

fn main() {
    assert_eq!(a!(), 1);
    assert_eq!(b!(), 2);
    assert_eq!(c!(), 3);
    assert_eq!(a(), 1);
    assert_eq!(b(), 2);
    assert_eq!(c(), 3);
    assert_eq!(bar!(z:tt), 42);
    assert_eq!(add3!(9), 12);
}
"#;
    assert_eq!(ui_fixture(Mode::RunPass, fixture, &Options::default()), "");
}

/// What `std` and `test` are to `duplicated-attributes.rs`: the builtin attribute macros in the
/// prelude, and `Bencher`.
const TEST_CRATES: &str = r#"//- /std.rs crate:std edition:2015 library
pub mod prelude {
    pub mod v1 {
        #[rustc_builtin_macro]
        pub macro test($item:item) {}
        #[rustc_builtin_macro]
        pub macro bench($item:item) {}
        #[rustc_builtin_macro]
        pub macro test_case($item:item) {}
        #[rustc_builtin_macro]
        pub macro cfg_eval($($tt:tt)*) {}
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
//- /test.rs crate:test edition:2015 library
pub struct Bencher;
"#;

#[test]
fn a_builtin_attribute_macro_given_twice_is_linted() {
    let source = r#"//- /duplicated-attributes.rs crate:other:main edition:2015 deps:std,test,lib_features cfg:test
// Test that, if an item is annotated with a builtin attribute more than once, a warning is
// emitted.
// Tests https://github.com/rust-lang/rust/issues/90979

//@ check-pass
//@ compile-flags: --test

#![feature(test)]
#![feature(cfg_eval)]

#[test]
#[test]
//~^ WARNING duplicated attribute
fn f() {}

// The following shouldn't trigger an error. The attribute is not duplicated.
#[test]
fn f2() {}

// The following shouldn't trigger an error either. The second attribute is not #[test].
#[test]
#[inline]
fn f3() {}

extern crate test;
use test::Bencher;

#[bench]
#[bench]
//~^ WARNING duplicated attribute
fn f4(_: &mut Bencher) {}

#[cfg_eval]
#[cfg_eval]
//~^ WARNING duplicated attribute
struct S;

#[cfg_eval]
struct S2;

fn main() {}
"#;
    let fixture = format!("{TEST_CRATES}{}{source}", library_features(&["cfg_eval"]));
    assert_reports(
        &ui_fixture(Mode::Other, &fixture, &Options::default()),
        &[
            warning(12, &[]),
            warning(29, &[]),
            warning(34, &[]),
        ],
    );
}
