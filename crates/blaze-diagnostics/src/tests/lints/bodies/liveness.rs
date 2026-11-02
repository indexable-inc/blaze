//! rustc's UI tests of its liveness lints, `unused_variables` and `unused_assignments`.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui, ui_run_pass},
};

/// [`ui`] of a UI test that needs core's items: core is minicore with `flags`
/// (`crates/test-utils/src/minicore.rs`).
fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}");
    let options = compiletest(Mode::Other, &Options::default());
    normalized(&checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() }))
}

#[test]
fn dead_assignments_point_at_what_overwrites_them() {
    // tests/ui/liveness/liveness-dead.rs
    let source = r#"#![allow(dead_code)]
#![deny(unused_assignments)]

fn f1(x: &mut isize) {
    *x = 1; // no error
}

fn f2() {
    let mut x: isize = 3; //~ ERROR: value assigned to `x` is never read
    x = 4;
    x.clone();
}

fn f3() {
    let mut x: isize = 3;
    x.clone();
    x = 4; //~ ERROR: value assigned to `x` is never read
}

fn f4(mut x: i32) { //~ ERROR: value passed to `x` is never read
    x = 4;
    x.clone();
}

fn f5(mut x: i32) {
    x.clone();
    x = 4; //~ ERROR: value assigned to `x` is never read
}

// #22630
fn f6() {
    let mut done = false;
    while !done {
        done = true; // no error
        continue;
    }
}

fn main() {}
"#;
    let stderr = r#"error: value assigned to `x` is never read
  --> $DIR/liveness-dead.rs:9:24
   |
LL |     let mut x: isize = 3;
   |                        ^ this value is reassigned later and never used
LL |     x = 4;
   |     ----- `x` is overwritten here before the previous value is read
   |
note: the lint level is defined here
  --> $DIR/liveness-dead.rs:2:9
   |
LL | #![deny(unused_assignments)]
   |         ^^^^^^^^^^^^^^^^^^

error: value assigned to `x` is never read
  --> $DIR/liveness-dead.rs:17:5
   |
LL |     x = 4;
   |     ^^^^^
   |
   = help: maybe it is overwritten before being read?

error: value passed to `x` is never read
  --> $DIR/liveness-dead.rs:20:7
   |
LL | fn f4(mut x: i32) {
   |       ^^^^^
   |
   = help: maybe it is overwritten before being read?

error: value assigned to `x` is never read
  --> $DIR/liveness-dead.rs:27:5
   |
LL |     x = 4;
   |     ^^^^^
   |
   = help: maybe it is overwritten before being read?

error: aborting due to 4 previous errors

"#;
    assert_eq!(ui_core("liveness-dead.rs", "2015", "clone, unary_ops, builtin_impls", source), stderr);
}

#[test]
fn a_binding_a_macro_makes_of_its_argument_points_at_the_macro() {
    // tests/ui/lint/unused/issue-117284-arg-in-macro.rs
    let source = r#"#![deny(unused_variables)]
macro_rules! make_var {
    ($struct:ident, $var:ident) => {
        let $var = $struct.$var; //~ ERROR unused variable: `var`
    };
}

#[allow(unused)]
struct MyStruct {
    var: i32,
}

fn main() {
    let s = MyStruct { var: 42 };
    make_var!(s, var);
    let a = 1; //~ ERROR unused variable: `a`
}
"#;
    assert_reports(
        &ui("issue-117284-arg-in-macro.rs", "2015", source),
        &[
            error(4, &["`var`"]),
            error(16, &["`a`"]),
        ],
    );
}

#[test]
fn the_level_of_a_match_arm_is_the_level_of_its_bindings() {
    // tests/ui/lint/lint-match-arms.rs
    let source = r#"fn deny_on_arm() {
    match 0 {
        #[deny(unused_variables)]
        //~^ NOTE the lint level is defined here
        y => (),
        //~^ ERROR unused variable
    }
}

#[deny(unused_variables)]
fn allow_on_arm() {
    match 0 {
        #[allow(unused_variables)]
        y => (), // OK
    }
}

fn main() {}
"#;
    let stderr = r#"error: unused variable: `y`
  --> $DIR/lint-match-arms.rs:5:9
   |
LL |         y => (),
   |         ^ help: if this is intentional, prefix it with an underscore: `_y`
   |
note: the lint level is defined here
  --> $DIR/lint-match-arms.rs:3:16
   |
LL |         #[deny(unused_variables)]
   |                ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("lint-match-arms.rs", "2015", source), stderr);
}

#[test]
fn a_renamed_field_of_a_parameter_is_prefixed() {
    // tests/ui/lint/unused_variables-issue-82488.rs
    let source = r#"//@ run-rustfix
#![deny(unused_variables)]

struct Point {
    x: u32,
    y: u32
}

fn process_point(Point { x, y: renamed }: Point) {
//~^ ERROR unused variable: `renamed`
    let _ = x;
}

fn main() {
    process_point(Point { x: 0, y: 0 });
}
"#;
    let stderr = r#"error: unused variable: `renamed`
  --> $DIR/unused_variables-issue-82488.rs:9:32
   |
LL | fn process_point(Point { x, y: renamed }: Point) {
   |                                ^^^^^^^ help: if this is intentional, prefix it with an underscore: `_renamed`
   |
note: the lint level is defined here
  --> $DIR/unused_variables-issue-82488.rs:2:9
   |
LL | #![deny(unused_variables)]
   |         ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unused_variables-issue-82488.rs", "2015", source), stderr);
}

#[test]
fn a_binding_of_a_match_assigned_to_a_variable() {
    // tests/ui/lint/unused/unused-var-in-match-arm.rs
    let source = r#"//! regression test for <https://github.com/rust-lang/rust/issues/22599>
#![deny(unused_variables)]

fn f(_: i32) {}

fn main() {
    let mut v = 0;
    f(v);
    v = match 0 { a => 0 }; //~ ERROR: unused variable: `a`
    f(v);
}
"#;
    let stderr = r#"error: unused variable: `a`
  --> $DIR/unused-var-in-match-arm.rs:9:19
   |
LL |     v = match 0 { a => 0 };
   |                   ^ help: if this is intentional, prefix it with an underscore: `_a`
   |
note: the lint level is defined here
  --> $DIR/unused-var-in-match-arm.rs:2:9
   |
LL | #![deny(unused_variables)]
   |         ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unused-var-in-match-arm.rs", "2015", source), stderr);
}

#[test]
fn a_raw_shorthand_field_is_ignored_by_its_raw_name() {
    // tests/ui/lint/unused/unused-variable-with-raw-identifier-in-struct-pattern.rs
    let source = r#"//@ check-pass
//@ run-rustfix

#![warn(unused)]
#![allow(dead_code)]

struct Foo {
    r#move: u32
}

fn main() {
    let y = Foo { r#move: 3 };

    let _ = match y {
         Foo { r#move } => 0 //~ WARNING unused variable: `r#move`
                             //~| HELP try ignoring the field
                             //~| SUGGESTION r#move: _
    };
}
"#;
    let stderr = r#"warning: unused variable: `r#move`
  --> $DIR/unused-variable-with-raw-identifier-in-struct-pattern.rs:15:16
   |
LL |          Foo { r#move } => 0
   |                ^^^^^^ help: try ignoring the field: `r#move: _`
   |
note: the lint level is defined here
  --> $DIR/unused-variable-with-raw-identifier-in-struct-pattern.rs:4:9
   |
LL | #![warn(unused)]
   |         ^^^^^^
   = note: `#[warn(unused_variables)]` implied by `#[warn(unused)]`

warning: 1 warning emitted

"#;
    assert_eq!(ui("unused-variable-with-raw-identifier-in-struct-pattern.rs", "2015", source), stderr);
}

#[test]
fn a_binding_of_a_rest_pattern_points_at_its_name() {
    // tests/ui/lint/unused/issue-81314-unused-span-ident.rs
    let source = r#"//@ run-rustfix
// Regression test for #81314: Unused variable lint should
// span only the identifier and not the rest of the pattern

#![deny(unused)]

fn main() {
    let [rest @ ..] = [1, 2, 3]; //~ ERROR unused variable
}

pub fn foo([rest @ ..]: &[i32]) { //~ ERROR unused variable
}
"#;
    let stderr = r#"error: unused variable: `rest`
  --> $DIR/issue-81314-unused-span-ident.rs:8:10
   |
LL |     let [rest @ ..] = [1, 2, 3];
   |          ^^^^ help: if this is intentional, prefix it with an underscore: `_rest`
   |
note: the lint level is defined here
  --> $DIR/issue-81314-unused-span-ident.rs:5:9
   |
LL | #![deny(unused)]
   |         ^^^^^^
   = note: `#[deny(unused_variables)]` implied by `#[deny(unused)]`

error: unused variable: `rest`
  --> $DIR/issue-81314-unused-span-ident.rs:11:13
   |
LL | pub fn foo([rest @ ..]: &[i32]) {
   |             ^^^^ help: if this is intentional, prefix it with an underscore: `_rest`

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("issue-81314-unused-span-ident.rs", "2015", source), stderr);
}

#[test]
fn only_a_shorthand_field_is_ignored() {
    // tests/ui/suggestions/try-removing-the-field.rs
    let source = r#"//@ run-pass

#![allow(dead_code)]

struct Foo {
    foo: i32,
    bar: (),
    baz: (),
}

fn use_foo(x: Foo) -> i32 {
    let Foo { foo, bar, .. } = x; //~ WARNING unused variable: `bar`
                                  //~| help: try ignoring the field
    return foo;
}

// issue #105028, suggest removing the field only for shorthand
fn use_match(x: Foo) {
    match x {
        Foo { foo: unused, .. } => { //~ WARNING unused variable
                                     //~| help: if this is intentional, prefix it with an underscore
        }
    }

    match x {
        Foo { foo, .. } => { //~ WARNING unused variable
                             //~| help: try ignoring the field
        }
    }
}

fn main() {}
"#;
    let stderr = r#"warning: unused variable: `bar`
  --> $DIR/try-removing-the-field.rs:12:20
   |
LL |     let Foo { foo, bar, .. } = x;
   |                    ^^^ help: try ignoring the field: `bar: _`
   |
   = note: `#[warn(unused_variables)]` (part of `#[warn(unused)]`) on by default

warning: unused variable: `unused`
  --> $DIR/try-removing-the-field.rs:20:20
   |
LL |         Foo { foo: unused, .. } => {
   |                    ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`

warning: unused variable: `foo`
  --> $DIR/try-removing-the-field.rs:26:15
   |
LL |         Foo { foo, .. } => {
   |               ^^^ help: try ignoring the field: `foo: _`

warning: 3 warnings emitted

"#;
    assert_eq!(crate::tests::ui_core_as(Mode::RunPass, "try-removing-the-field.rs", "2015", "copy", source, &Options::default()), stderr);
}

#[test]
fn a_nested_shorthand_field_is_ignored() {
    // tests/ui/suggestions/ignore-nested-field-binding.rs
    let source = r#"// Regression test for #88403, where prefixing with an underscore was
// erroneously suggested for a nested shorthand struct field binding.

//@ run-rustfix
#![allow(unused)]
#![forbid(unused_variables)]

struct Inner { i: i32 }
struct Outer { o: Inner }

fn foo(Outer { o: Inner { i } }: Outer) {}
//~^ ERROR: unused variable: `i`
//~| HELP: try ignoring the field

fn main() {
    let s = Outer { o: Inner { i: 42 } };
    let Outer { o: Inner { i } } = s;
    //~^ ERROR: unused variable: `i`
    //~| HELP: try ignoring the field
}
"#;
    let stderr = r#"error: unused variable: `i`
  --> $DIR/ignore-nested-field-binding.rs:11:27
   |
LL | fn foo(Outer { o: Inner { i } }: Outer) {}
   |                           ^ help: try ignoring the field: `i: _`
   |
note: the lint level is defined here
  --> $DIR/ignore-nested-field-binding.rs:6:11
   |
LL | #![forbid(unused_variables)]
   |           ^^^^^^^^^^^^^^^^

error: unused variable: `i`
  --> $DIR/ignore-nested-field-binding.rs:17:28
   |
LL |     let Outer { o: Inner { i } } = s;
   |                            ^ help: try ignoring the field: `i: _`

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("ignore-nested-field-binding.rs", "2015", source), stderr);
}

#[test]
fn a_parameter_is_not_suggested_to_be_a_constant() {
    // tests/ui/fn/invalid-sugg-for-unused-fn-arg-147303.rs
    let source = r#"// Regression test for <https://github.com/rust-lang/rust/issues/147303>.

#![deny(unused_assignments, unused_variables)]

mod m1 {
    const _MAX_FMTVER_X1X_EVENTNUM: i32 = 0;
}

mod m2 {
    fn fun(rough: i32) {} //~ERROR unused variable
}

fn main() {}
"#;
    let stderr = r#"error: unused variable: `rough`
  --> $DIR/invalid-sugg-for-unused-fn-arg-147303.rs:10:12
   |
LL |     fn fun(rough: i32) {}
   |            ^^^^^ help: if this is intentional, prefix it with an underscore: `_rough`
   |
note: the lint level is defined here
  --> $DIR/invalid-sugg-for-unused-fn-arg-147303.rs:3:29
   |
LL | #![deny(unused_assignments, unused_variables)]
   |                             ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("invalid-sugg-for-unused-fn-arg-147303.rs", "2015", source), stderr);
}

/// rustc's `check_liveness` returns at once for a function with the `naked` attribute, written
/// `#[unsafe(naked)]` or through `cfg_attr`, whatever its body is; another function's unused
/// parameter is still linted.
#[test]
fn a_naked_function_is_not_checked() {
    let source = r#"//- /main.rs crate:other:main edition:2024
#![warn(unused_variables)]
#[unsafe(naked)]
pub extern "C" fn naked(a: u32) {}
#[cfg_attr(all(), unsafe(naked))]
pub extern "C" fn by_cfg_attr(b: u32) {}
pub extern "C" fn plain(c: u32) {}
fn main() {}
"#;
    let options = compiletest(Mode::Other, &Options::default());
    let unused: Vec<String> = checked_with(source, "", &options)
        .messages()
        .into_iter()
        .filter(|it| it.starts_with("unused_variables: "))
        .collect();
    assert_eq!(unused, ["unused_variables: unused variable: `c`"]);
}

#[test]
fn an_automatically_derived_impl_is_not_checked() {
    // tests/ui/liveness/liveness-derive.rs
    let source = r#"// Test for interaction between #[automatically_derived] attribute used by
// built-in derives and lints generated by liveness pass.
//
//@ edition:2018
//@ check-pass
#![warn(unused)]

pub trait T: Sized {
    const N: usize;
    fn t(&self) -> Self;
}

impl T for u32 {
    const N: usize = {
        let a = 0; //~ WARN unused variable: `a`
        4
    };

    fn t(&self) -> Self {
        let b = 16; //~ WARN unused variable: `b`
        0
    }
}

#[automatically_derived]
impl T for i32 {
    const N: usize = {
        let c = 0;
        4
    };

    fn t(&self) -> Self {
        let d = 17;
        0
    }
}

fn main() {}
"#;
    let stderr = r#"warning: unused variable: `a`
  --> $DIR/liveness-derive.rs:15:13
   |
LL |         let a = 0;
   |             ^ help: if this is intentional, prefix it with an underscore: `_a`
   |
note: the lint level is defined here
  --> $DIR/liveness-derive.rs:6:9
   |
LL | #![warn(unused)]
   |         ^^^^^^
   = note: `#[warn(unused_variables)]` implied by `#[warn(unused)]`

warning: unused variable: `b`
  --> $DIR/liveness-derive.rs:20:13
   |
LL |         let b = 16;
   |             ^ help: if this is intentional, prefix it with an underscore: `_b`

warning: 2 warnings emitted

"#;
    assert_eq!(ui_core("liveness-derive.rs", "2018", "sized", source), stderr);
}

#[test]
fn a_string_naming_an_unused_parameter_suggests_format() {
    // tests/ui/type/issue-100584.rs
    let source = r#"#![deny(unused)]
fn foo(xyza: &str) {
//~^ ERROR unused variable: `xyza`
    let _ = "{xyza}";
}

fn foo3(xyza: &str) {
//~^ ERROR unused variable: `xyza`
    let _ = "aaa{xyza}bbb";
}

fn main() {
  foo("x");
  foo3("xx");
}
"#;
    let stderr = r#"error: unused variable: `xyza`
  --> $DIR/issue-100584.rs:2:8
   |
LL | fn foo(xyza: &str) {
   |        ^^^^
LL |
LL |     let _ = "{xyza}";
   |             -------- you might have meant to use string interpolation in this string literal
   |
note: the lint level is defined here
  --> $DIR/issue-100584.rs:1:9
   |
LL | #![deny(unused)]
   |         ^^^^^^
   = note: `#[deny(unused_variables)]` implied by `#[deny(unused)]`
help: string interpolation only works in `format!` invocations
   |
LL |     let _ = format!("{xyza}");
   |             ++++++++        +
help: if this is intentional, prefix it with an underscore
   |
LL | fn foo(_xyza: &str) {
   |        +

error: unused variable: `xyza`
  --> $DIR/issue-100584.rs:7:9
   |
LL | fn foo3(xyza: &str) {
   |         ^^^^
LL |
LL |     let _ = "aaa{xyza}bbb";
   |             -------------- you might have meant to use string interpolation in this string literal
   |
help: string interpolation only works in `format!` invocations
   |
LL |     let _ = format!("aaa{xyza}bbb");
   |             ++++++++              +
help: if this is intentional, prefix it with an underscore
   |
LL | fn foo3(_xyza: &str) {
   |         +

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("issue-100584.rs", "2015", source), stderr);
}

#[test]
fn a_field_a_pattern_macro_binds_is_prefixed() {
    // tests/ui/lint/issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs
    let source = r#"//@ run-pass
#![warn(unused)]
#![deny(non_shorthand_field_patterns)]

pub struct Value<A> { pub value: A }

#[macro_export]
macro_rules! pat {
    ($a:pat) => {
        Value { value: $a }
    };
}

fn main() {
    let pat!(value) = Value { value: () };
    //~^ WARN unused variable: `value`
}
"#;
    let stderr = r#"warning: unused variable: `value`
  --> $DIR/issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs:15:14
   |
LL |     let pat!(value) = Value { value: () };
   |              ^^^^^ help: if this is intentional, prefix it with an underscore: `_value`
   |
note: the lint level is defined here
  --> $DIR/issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs:2:9
   |
LL | #![warn(unused)]
   |         ^^^^^^
   = note: `#[warn(unused_variables)]` implied by `#[warn(unused)]`

warning: 1 warning emitted

"#;
    assert_eq!(
        ui_run_pass("issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs", "2015", source, &Options::default()),
        stderr
    );
}
