//! rustc's UI tests of the errors of members and struct literals: E0599 (an associated item a
//! path names that its type lacks), E0609, E0610 and E0615 (a field access), E0425 and E0424
//! (a value name resolve can't find), E0616 and E0624 (a private field or method), E0560,
//! E0559, E0026 and E0451 (a field a struct expression or pattern names), E0062 and E0025 (a
//! field named twice), E0063 and its private-fields form (the fields an expression leaves
//! out), E0027 and its form for inaccessible fields (those a pattern leaves out), E0797, E0436
//! and the errors of destructuring assignments; and #1522, an E0063 wherever the crate's
//! modules put it. Each test's file is the UI test's source byte for byte; a file that needs
//! what the crate without std lacks (`String`, `Box`) has it added after its last line, which
//! no diagnostic shows.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked, checked_with, compiletest, error, normalized, ui},
};

/// [`ui`] of a fixture of several files or crates, or with minicore: each file named
/// `$DIR/<path>`, checked as compiletest runs a UI test.
fn ui_fixture(fixture: &str) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// [`ui_fixture`] of one file with minicore's items for `flags`
/// (`crates/test-utils/src/minicore.rs`).
fn ui_core(name: &str, flags: &str, source: &str) -> String {
    ui_fixture(&format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:2015\n{source}"))
}

#[test]
fn the_fields_an_expression_leaves_out_are_listed_sorted_and_cut_after_three() {
    // tests/ui/error-codes/E0063.rs
    let source = r#"struct SingleFoo {
    x: i32
}

struct PluralFoo {
    x: i32,
    y: i32,
    z: i32
}

struct TruncatedFoo {
    a: i32,
    b: i32,
    x: i32,
    y: i32,
    z: i32
}

struct TruncatedPluralFoo {
    a: i32,
    b: i32,
    c: i32,
    x: i32,
    y: i32,
    z: i32
}


fn main() {
    let w = SingleFoo { };
    //~^ ERROR missing field `x` in initializer of `SingleFoo`
    let x = PluralFoo {x: 1};
    //~^ ERROR missing fields `y` and `z` in initializer of `PluralFoo`
    let y = TruncatedFoo{x:1};
    //~^ ERROR missing fields `a`, `b`, `y` and 1 other field in initializer of `TruncatedFoo`
    let z = TruncatedPluralFoo{x:1};
    //~^ ERROR missing fields `a`, `b`, `c` and 2 other fields in initializer of `TruncatedPluralFoo`
}
"#;
    assert_eq!(ui("E0063.rs", "2015", source), r#"error[E0063]: missing field `x` in initializer of `SingleFoo`
  --> $DIR/E0063.rs:30:13
   |
LL |     let w = SingleFoo { };
   |             ^^^^^^^^^ missing `x`

error[E0063]: missing fields `y` and `z` in initializer of `PluralFoo`
  --> $DIR/E0063.rs:32:13
   |
LL |     let x = PluralFoo {x: 1};
   |             ^^^^^^^^^ missing `y` and `z`

error[E0063]: missing fields `a`, `b`, `y` and 1 other field in initializer of `TruncatedFoo`
  --> $DIR/E0063.rs:34:13
   |
LL |     let y = TruncatedFoo{x:1};
   |             ^^^^^^^^^^^^ missing `a`, `b`, `y` and 1 other field

error[E0063]: missing fields `a`, `b`, `c` and 2 other fields in initializer of `TruncatedPluralFoo`
  --> $DIR/E0063.rs:36:13
   |
LL |     let z = TruncatedPluralFoo{x:1};
   |             ^^^^^^^^^^^^^^^^^^ missing `a`, `b`, `c` and 2 other fields

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0063`.
"#);
}

#[test]
fn a_missing_field_is_reported_at_the_path_of_a_literal_over_lines() {
    // tests/ui/structs/struct-fields-missing.rs
    let source = r#"struct BuildData {
    foo: isize,
    bar: Box<isize>,
}

fn main() {
    let foo = BuildData { //~ ERROR missing field `bar` in initializer of `BuildData`
        foo: 0
    };
}
struct Box<T>(T);
"#;
    assert_eq!(ui("struct-fields-missing.rs", "2015", source), r#"error[E0063]: missing field `bar` in initializer of `BuildData`
  --> $DIR/struct-fields-missing.rs:7:15
   |
LL |     let foo = BuildData {
   |               ^^^^^^^^^ missing `bar`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0063`.
"#);
}

#[test]
fn a_variant_left_short_names_its_enum_and_private_fields_take_the_place_of_e0063() {
    // tests/ui/privacy/private-field-struct-construction-76077.rs
    let source = r#"// https://github.com/rust-lang/rust/issues/76077
pub mod foo {
    pub struct Foo {
        you_cant_use_this_field: bool,
    }
}

fn main() {
    foo::Foo {};
    //~^ ERROR cannot construct `Foo` with struct literal syntax due to private fields
}
"#;
    assert_eq!(ui("private-field-struct-construction-76077.rs", "2015", source), r#"error: cannot construct `Foo` with struct literal syntax due to private fields
  --> $DIR/private-field-struct-construction-76077.rs:9:5
   |
LL |     foo::Foo {};
   |     ^^^^^^^^
   |
   = note: private field `you_cant_use_this_field` that was not provided

error: aborting due to 1 previous error

"#);
}

#[test]
fn a_struct_with_a_private_field_is_not_built_by_its_public_one() {
    // tests/ui/typeck/issue-87872-missing-inaccessible-field-literal.rs
    let source = r#"pub mod foo {
    pub struct Foo {
        pub you_can_use_this_field: bool,
        you_cant_use_this_field: bool,
    }
}

fn main() {
    foo::Foo {};
    //~^ ERROR cannot construct `Foo` with struct literal syntax due to private fields
}
"#;
    assert_eq!(ui("issue-87872-missing-inaccessible-field-literal.rs", "2015", source), r#"error: cannot construct `Foo` with struct literal syntax due to private fields
  --> $DIR/issue-87872-missing-inaccessible-field-literal.rs:9:5
   |
LL |     foo::Foo {};
   |     ^^^^^^^^
   |
   = note: private field `you_cant_use_this_field` that was not provided

error: aborting due to 1 previous error

"#);
}

#[test]
fn the_private_fields_given_are_labelled_and_the_others_listed() {
    // tests/ui/typeck/missing-private-fields-in-struct-literal.rs
    let source = r#"pub mod m {
    pub struct S {
        pub visible: bool,
        a: (),
        b: (),
        c: (),
        d: (),
        e: (),
    }
}

fn main() {
    let _ = m::S { //~ ERROR cannot construct `S` with struct literal syntax due to private fields
        visible: true,
        a: (),
        b: (),
    };
}
"#;
    assert_eq!(ui("missing-private-fields-in-struct-literal.rs", "2015", source), r#"error: cannot construct `S` with struct literal syntax due to private fields
  --> $DIR/missing-private-fields-in-struct-literal.rs:13:13
   |
LL |     let _ = m::S {
   |             ^^^^
LL |         visible: true,
LL |         a: (),
   |         ----- private field
LL |         b: (),
   |         ----- private field
   |
   = note: ...and other private fields `c`, `d` and `e` that were not provided

error: aborting due to 1 previous error

"#);
}

/// rustc gives no E0063 of a literal with a field its struct lacks (`error_happened`).
#[test]
fn an_unknown_field_leaves_out_the_missing_ones() {
    // tests/ui/structs/suggest-private-fields.rs
    let fixture = r#"//- /auxiliary/struct_field_privacy.rs crate:struct_field_privacy edition:2015 library
pub struct A {
    a: isize,
    pub b: isize,
}

pub struct B {
    pub a: isize,
    b: isize,
}
//- /suggest-private-fields.rs crate:other:main edition:2015 deps:struct_field_privacy
//@ aux-build:struct_field_privacy.rs

extern crate struct_field_privacy as xc;

use xc::B;

struct A {
    pub a: u32,
    b: u32,
}

fn main () {
    // external crate struct
    let k = B {
        aa: 20,
        //~^ ERROR struct `B` has no field named `aa`
        bb: 20,
        //~^ ERROR struct `B` has no field named `bb`
    };
    // local crate struct
    let l = A {
        aa: 20,
        //~^ ERROR struct `A` has no field named `aa`
        bb: 20,
        //~^ ERROR struct `A` has no field named `bb`
    };
}
"#;
    assert_eq!(ui_fixture(fixture), r#"error[E0560]: struct `B` has no field named `aa`
  --> $DIR/suggest-private-fields.rs:15:9
   |
LL |         aa: 20,
   |         ^^ unknown field
   |
help: a field with a similar name exists
   |
LL -         aa: 20,
LL +         a: 20,
   |

error[E0560]: struct `B` has no field named `bb`
  --> $DIR/suggest-private-fields.rs:17:9
   |
LL |         bb: 20,
   |         ^^ `B` does not have this field
   |
   = note: available fields are: `a`

error[E0560]: struct `A` has no field named `aa`
  --> $DIR/suggest-private-fields.rs:22:9
   |
LL |         aa: 20,
   |         ^^ unknown field
   |
help: a field with a similar name exists
   |
LL -         aa: 20,
LL +         a: 20,
   |

error[E0560]: struct `A` has no field named `bb`
  --> $DIR/suggest-private-fields.rs:24:9
   |
LL |         bb: 20,
   |         ^^ unknown field
   |
help: a field with a similar name exists
   |
LL -         bb: 20,
LL +         b: 20,
   |

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0560`.
"#);
}

/// A struct of another crate marked `#[non_exhaustive]` gives E0639, not the fields it lacks.
#[test]
fn a_foreign_non_exhaustive_struct_gives_no_missing_fields() {
    // tests/ui/privacy/non-exhaustive-with-private-fields-147513.rs
    let fixture = r#"//- /auxiliary/non_exhaustive_with_private.rs crate:non_exhaustive_with_private edition:2015 library
// Auxiliary crate for testing non-exhaustive struct with private fields

#[non_exhaustive]
pub struct Foo {
    pub my_field: u32,
    private_field: i32,
}

#[non_exhaustive]
pub struct Bar {
    pub my_field: u32,
    pub missing_field: i32,
}
//- /non-exhaustive-with-private-fields-147513.rs crate:other:main edition:2015 deps:non_exhaustive_with_private
//@ aux-build:non_exhaustive_with_private.rs

extern crate non_exhaustive_with_private;

use non_exhaustive_with_private::{Bar, Foo};

fn main() {
    let foo = Foo {
        //~^ ERROR cannot create non-exhaustive struct using struct expression
        my_field: 10,
    };

    let bar = Bar {
        //~^ ERROR cannot create non-exhaustive struct using struct expression
        my_field: 10,
    };
}
"#;
    assert_eq!(ui_fixture(fixture), r#"error[E0639]: cannot create non-exhaustive struct using struct expression
  --> $DIR/non-exhaustive-with-private-fields-147513.rs:8:15
   |
LL |       let foo = Foo {
   |  _______________^
LL | |
LL | |         my_field: 10,
LL | |     };
   | |_____^

error[E0639]: cannot create non-exhaustive struct using struct expression
  --> $DIR/non-exhaustive-with-private-fields-147513.rs:13:15
   |
LL |       let bar = Bar {
   |  _______________^
LL | |
LL | |         my_field: 10,
LL | |     };
   | |_____^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0639`.
"#);
}

/// The parser's recovery in a struct literal or pattern leaves out its missing fields.
#[test]
fn a_syntax_error_in_the_fields_leaves_out_the_missing_ones() {
    // tests/ui/structs/syntax-error-not-missing-field.rs
    let source = r#"// Check that a syntax error inside a struct literal does not also report missing fields,
// because the field might be present but hidden by the syntax error.
//
// The stderr for this test should contain ONLY one syntax error per struct literal,
// and not any errors about missing fields.

struct Foo { a: isize, b: isize }

enum Bar {
    Baz { a: isize, b: isize },
}

fn make_a() -> isize { 1234 }

fn expr_wrong_separator() {
    let f = Foo { a: make_a(); b: 2 }; //~ ERROR found `;`
}

fn expr_missing_separator() {
    let f = Foo { a: make_a() b: 2 }; //~ ERROR found `b`
}

fn expr_rest_trailing_comma() {
    let f = Foo { a: make_a(), ..todo!(), }; //~ ERROR cannot use a comma
}

fn expr_missing_field_name() {
    let f = Foo { make_a(), b: 2, }; //~ ERROR found `(`
}

fn pat_wrong_separator(Foo { a; b }: Foo) { //~ ERROR expected `,`
    let _ = (a, b);
}

fn pat_missing_separator(Foo { a b }: Foo) { //~ ERROR expected `,`
    let _ = (a, b);
}

fn pat_rest_trailing_comma(Foo { a, .., }: Foo) { //~ ERROR expected `}`, found `,`
}

fn enum_expr_wrong_separator() {
    let e = Bar::Baz { a: make_a(); b: 2 }; //~ ERROR found `;`
}

fn enum_expr_missing_separator() {
    let e = Bar::Baz { a: make_a() b: 2 }; //~ ERROR found `b`
}

// Should error but not then ICE due to lack of type checking
fn regression_test_for_issue_153388_a() {
    struct TheStruct;
    struct MyStruct {
        value: i32,
        s: TheStruct,
    }
    static A: MyStruct = MyStruct { ,s: TheStruct }; //~ ERROR expected identifier, found `,`
}

fn regression_test_for_issue_153388_b() {
    struct MyStruct {
        value: i32,
    }
    static A: MyStruct = MyStruct {,}; //~ ERROR expected identifier, found `,`
}

fn main() {}
"#;
    let messages: Vec<String> = checked(&format!("//- /main.rs crate:other:main\n{source}"))
        .messages()
        .into_iter()
        .filter(|it| it.starts_with("E0063") || it.starts_with("E0027"))
        .collect();
    assert_eq!(messages, Vec::<String>::new());
}

#[test]
fn a_pattern_names_the_fields_it_leaves_out_with_three_ways_to_mention_them() {
    // tests/ui/error-codes/E0027.rs
    let source = r#"struct Dog {
    name: String,
    age: u32,
}


fn main() {
    let d = Dog { name: "Rusty".to_string(), age: 8 };

    match d {
        Dog { age: x } => {} //~ ERROR pattern does not mention field `name`
    }
    match d {
        // trailing comma
        Dog { name: x, } => {} //~ ERROR pattern does not mention field `age`
    }
    match d {
        // trailing comma with weird whitespace
        Dog { name: x  , } => {} //~ ERROR pattern does not mention field `age`
    }
    match d {
        Dog {} => {} //~ ERROR pattern does not mention fields `name`, `age`
    }
}
struct String;
trait ToString { fn to_string(&self) -> String; }
impl ToString for str { fn to_string(&self) -> String { String } }
"#;
    assert_eq!(ui_core("E0027.rs", "copy", source), r#"error[E0027]: pattern does not mention field `name`
  --> $DIR/E0027.rs:11:9
   |
LL |         Dog { age: x } => {}
   |         ^^^^^^^^^^^^^^ missing field `name`
   |
help: include the missing field in the pattern
   |
LL |         Dog { age: x, name } => {}
   |                     ++++++
help: if you don't care about this missing field, you can explicitly ignore it
   |
LL |         Dog { age: x, name: _ } => {}
   |                     +++++++++
help: or always ignore missing fields here
   |
LL |         Dog { age: x, .. } => {}
   |                     ++++

error[E0027]: pattern does not mention field `age`
  --> $DIR/E0027.rs:15:9
   |
LL |         Dog { name: x, } => {}
   |         ^^^^^^^^^^^^^^^^ missing field `age`
   |
help: include the missing field in the pattern
   |
LL |         Dog { name: x, age } => {}
   |                        +++
help: if you don't care about this missing field, you can explicitly ignore it
   |
LL |         Dog { name: x, age: _ } => {}
   |                        ++++++
help: or always ignore missing fields here
   |
LL |         Dog { name: x, .. } => {}
   |                        ++

error[E0027]: pattern does not mention field `age`
  --> $DIR/E0027.rs:19:9
   |
LL |         Dog { name: x  , } => {}
   |         ^^^^^^^^^^^^^^^^^^ missing field `age`
   |
help: include the missing field in the pattern
   |
LL |         Dog { name: x, age } => {}
   |                      ~~~~~~~
help: if you don't care about this missing field, you can explicitly ignore it
   |
LL |         Dog { name: x, age: _ } => {}
   |                      ~~~~~~~~~~
help: or always ignore missing fields here
   |
LL |         Dog { name: x, .. } => {}
   |                      ~~~~~~

error[E0027]: pattern does not mention fields `name`, `age`
  --> $DIR/E0027.rs:22:9
   |
LL |         Dog {} => {}
   |         ^^^^^^ missing fields `name`, `age`
   |
help: include the missing fields in the pattern
   |
LL |         Dog { name, age } => {}
   |               +++++++++
help: if you don't care about these missing fields, you can explicitly ignore them
   |
LL |         Dog { name: _, age: _ } => {}
   |               +++++++++++++++
help: or always ignore missing fields here
   |
LL |         Dog { .. } => {}
   |               ++

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0027`.
"#);
}

#[test]
fn an_unmentioned_tuple_index_is_named_with_a_pattern() {
    // tests/ui/structs/struct-pat-unmentioned-tuple-indices.rs
    let source = r#"// On unmentioned tuple indices in struct patterns, don't suggest turning the pattern into a
// tuple struct pattern and keep the struct pattern in the suggestion.
// issue: <https://github.com/rust-lang/rust/issues/108284>

struct S(i32, f32);
enum E { V(i32, f32) }

fn main() {
    let S { 0: _ } = S(1, 2.2); //~ ERROR: pattern does not mention field `1`
    let E::V { 0: _ } = E::V(1, 2.2); //~ ERROR: pattern does not mention field `1`
}
"#;
    assert_eq!(ui("struct-pat-unmentioned-tuple-indices.rs", "2015", source), r#"error[E0027]: pattern does not mention field `1`
  --> $DIR/struct-pat-unmentioned-tuple-indices.rs:9:9
   |
LL |     let S { 0: _ } = S(1, 2.2);
   |         ^^^^^^^^^^ missing field `1`
   |
help: include the missing field in the pattern
   |
LL |     let S { 0: _, 1: _ } = S(1, 2.2);
   |                 ++++++
help: if you don't care about this missing field, you can explicitly ignore it
   |
LL |     let S { 0: _, 1: _ } = S(1, 2.2);
   |                 ++++++
help: or always ignore missing fields here
   |
LL |     let S { 0: _, .. } = S(1, 2.2);
   |                 ++++

error[E0027]: pattern does not mention field `1`
  --> $DIR/struct-pat-unmentioned-tuple-indices.rs:10:9
   |
LL |     let E::V { 0: _ } = E::V(1, 2.2);
   |         ^^^^^^^^^^^^^ missing field `1`
   |
help: include the missing field in the pattern
   |
LL |     let E::V { 0: _, 1: _ } = E::V(1, 2.2);
   |                    ++++++
help: if you don't care about this missing field, you can explicitly ignore it
   |
LL |     let E::V { 0: _, 1: _ } = E::V(1, 2.2);
   |                    ++++++
help: or always ignore missing fields here
   |
LL |     let E::V { 0: _, .. } = E::V(1, 2.2);
   |                    ++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0027`.
"#);
}

#[test]
fn a_pattern_leaving_out_only_inaccessible_fields_requires_dots() {
    // tests/ui/privacy/inaccessible-private-fields-76077.rs
    let source = r#"// https://github.com/rust-lang/rust/issues/76077
//@ run-rustfix
#![allow(dead_code, unused_variables)]

pub mod foo {
    #[derive(Default)]
    pub struct Foo { invisible: bool, }

    #[derive(Default)]
    pub struct Bar { pub visible: bool, invisible: bool, }
}

fn main() {
    let foo::Foo {} = foo::Foo::default();
    //~^ ERROR pattern requires `..` due to inaccessible fields

    let foo::Bar { visible } = foo::Bar::default();
    //~^ ERROR pattern requires `..` due to inaccessible fields
}
"#;
    assert_eq!(ui_core("inaccessible-private-fields-76077.rs", "default, derive", source), r#"error: pattern requires `..` due to inaccessible fields
  --> $DIR/inaccessible-private-fields-76077.rs:14:9
   |
LL |     let foo::Foo {} = foo::Foo::default();
   |         ^^^^^^^^^^^
   |
help: ignore the inaccessible and unused fields
   |
LL |     let foo::Foo { .. } = foo::Foo::default();
   |                    ++

error: pattern requires `..` due to inaccessible fields
  --> $DIR/inaccessible-private-fields-76077.rs:17:9
   |
LL |     let foo::Bar { visible } = foo::Bar::default();
   |         ^^^^^^^^^^^^^^^^^^^^
   |
help: ignore the inaccessible and unused fields
   |
LL |     let foo::Bar { visible, .. } = foo::Bar::default();
   |                           ++++

error: aborting due to 2 previous errors

"#);
}

#[test]
fn the_dots_for_inaccessible_fields_go_after_a_trailing_comma() {
    // tests/ui/privacy/inaccessible-private-fields-trailing-comma-149787.rs
    let source = r#"#![allow(dead_code)]
#![allow(unused)]
//@ run-rustfix

mod m {
    pub(crate) struct S {
        pub(crate) visible: u64,
        hidden: u64,
    }

    impl S {
        pub(crate) fn new() -> Self {
            loop {}
        }
    }
}

fn main() {
    let m::S {
        //~^ ERROR pattern requires `..` due to inaccessible fields
        visible,
    } = m::S::new();
}
"#;
    assert_eq!(ui("inaccessible-private-fields-trailing-comma-149787.rs", "2015", source), r#"error: pattern requires `..` due to inaccessible fields
  --> $DIR/inaccessible-private-fields-trailing-comma-149787.rs:19:9
   |
LL |       let m::S {
   |  _________^
LL | |
LL | |         visible,
LL | |     } = m::S::new();
   | |_____^
   |
help: ignore the inaccessible and unused fields
   |
LL |         visible, ..
   |                  ++

error: aborting due to 1 previous error

"#);
}

/// A destructuring assignment is a pattern: E0026 and E0027 of it, its `..base` refused, and
/// `..` with no base in an expression E0797.
#[test]
fn a_destructuring_assignment_checks_its_fields_as_a_pattern() {
    // tests/ui/destructuring-assignment/struct_destructure_fail.rs
    let source = r#"struct Struct<S, T> {
    a: S,
    b: T,
}

fn main() {
    let (mut a, b);
    let mut c;
    let d = Struct { a: 0, b: 1 };
    Struct { a, b, c } = Struct { a: 0, b: 1 }; //~ ERROR does not have a field named `c`
    Struct { a, _ } = Struct { a: 1, b: 2 }; //~ ERROR pattern does not mention field `b`
    //~| ERROR expected identifier, found reserved identifier `_`
    Struct { a, ..d } = Struct { a: 1, b: 2 };
    //~^ ERROR functional record updates are not allowed in destructuring assignments
    Struct { a, .. }; //~ ERROR base expression required after `..`
}
"#;
    assert_reports(
        &ui("struct_destructure_fail.rs", "2015", source),
        &[
            error(11, &[]),
            error(13, &[]),
            error(10, &["`Struct`", "`c`"]),
            error(11, &["`b`"]),
            error(15, &["`..`"]),
        ],
    );
}

#[test]
fn a_field_given_twice() {
    // tests/ui/error-codes/E0062.rs
    let source = r#"struct Foo {
    x: i32
}

fn main() {
    let x = Foo {
        x: 0,
        x: 0,
        //~^ ERROR E0062
    };
}
"#;
    assert_eq!(ui("E0062.rs", "2015", source), r#"error[E0062]: field `x` specified more than once
  --> $DIR/E0062.rs:8:9
   |
LL |         x: 0,
   |         ---- first use of `x`
LL |         x: 0,
   |         ^ used more than once

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0062`.
"#);
}

#[test]
fn a_field_bound_twice() {
    // tests/ui/error-codes/E0025.rs
    let source = r#"struct Foo {
    a: u8,
    b: u8,
}

fn main() {
    let x = Foo { a:1, b:2 };
    let Foo { a: x, a: y, b: 0 } = x;
    //~^ ERROR field `a` bound multiple times in the pattern
}
"#;
    assert_reports(&ui("E0025.rs", "2015", source), &[error(8, &["`a`"])]);
}

#[test]
fn a_field_a_struct_lacks() {
    // tests/ui/error-codes/E0560.rs
    let source = r#"struct Simba {
    mother: u32,
}

fn main() {
    let s = Simba { mother: 1, father: 0 };
    //~^ ERROR E0560
}
"#;
    assert_eq!(ui("E0560.rs", "2015", source), r#"error[E0560]: struct `Simba` has no field named `father`
  --> $DIR/E0560.rs:6:32
   |
LL |     let s = Simba { mother: 1, father: 0 };
   |                                ^^^^^^ `Simba` does not have this field
   |
   = note: all struct fields are already assigned

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0560`.
"#);
}

#[test]
fn a_field_a_variant_lacks() {
    // tests/ui/error-codes/E0559.rs
    let source = r#"enum Field {
    Fool { x: u32 },
}

fn main() {
    let s = Field::Fool { joke: 0 };
    //~^ ERROR E0559
}
"#;
    assert_eq!(ui("E0559.rs", "2015", source), r#"error[E0559]: variant `Field::Fool` has no field named `joke`
  --> $DIR/E0559.rs:6:27
   |
LL |     let s = Field::Fool { joke: 0 };
   |                           ^^^^ `Field::Fool` does not have this field
   |
   = note: available fields are: `x`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0559`.
"#);
}

#[test]
fn a_pattern_field_a_struct_lacks() {
    // tests/ui/error-codes/E0026.rs
    let source = r#"struct Thing {
    x: u32,
    y: u32
}

fn main() {
    let thing = Thing { x: 0, y: 0 };
    match thing {
        Thing { x, y, z } => {}
        //~^ ERROR struct `Thing` does not have a field named `z` [E0026]
    }
}
"#;
    assert_eq!(ui("E0026.rs", "2015", source), r#"error[E0026]: struct `Thing` does not have a field named `z`
  --> $DIR/E0026.rs:9:23
   |
LL |         Thing { x, y, z } => {}
   |                       ^ struct `Thing` does not have this field

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0026`.
"#);
}

#[test]
fn a_private_field_named_in_a_pattern_and_a_literal() {
    // tests/ui/error-codes/E0451.rs
    let source = r#"mod bar {
    pub struct Foo {
        pub a: isize,
        b: isize,
    }

    pub struct FooTuple (
        pub isize,
        isize,
    );
}

fn pat_match(foo: bar::Foo) {
    let bar::Foo{a, b} = foo; //~ ERROR E0451
}

fn main() {
    let f = bar::Foo{ a: 0, b: 0 }; //~ ERROR E0451
}
"#;
    assert_eq!(ui("E0451.rs", "2015", source), r#"error[E0451]: field `b` of struct `Foo` is private
  --> $DIR/E0451.rs:14:21
   |
LL |     let bar::Foo{a, b} = foo;
   |                     ^ private field

error[E0451]: field `b` of struct `Foo` is private
  --> $DIR/E0451.rs:18:29
   |
LL |     let f = bar::Foo{ a: 0, b: 0 };
   |                             ^ private field

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0451`.
"#);
}

#[test]
fn functional_record_update_of_a_variant() {
    // tests/ui/functional-struct-update/fru-on-enum-variant.rs
    let source = r#"//! Regression test for <https://github.com/rust-lang/rust/issues/26948>.

fn main() {
    enum Foo { A { x: u32 } }
    let orig = Foo::A { x: 5 };
    Foo::A { x: 6, ..orig };
    //~^ ERROR functional record update syntax requires a struct
}
"#;
    assert_eq!(ui("fru-on-enum-variant.rs", "2015", source), r#"error[E0436]: functional record update syntax requires a struct
  --> $DIR/fru-on-enum-variant.rs:6:22
   |
LL |     Foo::A { x: 6, ..orig };
   |                      ^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0436`.
"#);
}

#[test]
fn functional_record_update_of_a_variant_in_a_closure() {
    // tests/ui/typeck/ice-with-expr-not-struct-127332.rs
    let source = r#"// Regression test for ICE #127332

// Tests that we do not ICE when a with expr is
// not a struct but something else like an enum

fn main() {
    let x = || {
        enum Foo {
            A { x: u32 },
        }
        let orig = Foo::A { x: 5 };
        Foo::A { x: 6, ..orig };
        //~^ ERROR functional record update syntax requires a struct
    };
}
"#;
    assert_eq!(ui("ice-with-expr-not-struct-127332.rs", "2015", source), r#"error[E0436]: functional record update syntax requires a struct
  --> $DIR/ice-with-expr-not-struct-127332.rs:12:26
   |
LL |         Foo::A { x: 6, ..orig };
   |                          ^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0436`.
"#);
}

#[test]
fn an_associated_item_a_pattern_path_names_that_its_type_lacks() {
    // tests/ui/error-codes/E0599.rs
    let source = r#"struct Foo;

fn main() {
    || if let Foo::NotEvenReal() = Foo {}; //~ ERROR E0599
}
"#;
    assert_eq!(ui("E0599.rs", "2015", source), r#"error[E0599]: no associated function or constant named `NotEvenReal` found for struct `Foo` in the current scope
  --> $DIR/E0599.rs:4:20
   |
LL | struct Foo;
   | ---------- associated function or constant `NotEvenReal` not found for this struct
...
LL |     || if let Foo::NotEvenReal() = Foo {};
   |                    ^^^^^^^^^^^ associated function or constant not found in `Foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0599`.
"#);
}

#[test]
fn a_field_a_struct_lacks_on_access() {
    // tests/ui/error-codes/E0609.rs
    let source = r#"struct Foo {
    x: u32,
}
struct Bar;

fn main() {
    let x = Foo { x: 0 };
    let _ = x.foo; //~ ERROR E0609

    let y = Bar;
    y.1; //~ ERROR E0609
}
"#;
    assert_eq!(ui("E0609.rs", "2015", source), r#"error[E0609]: no field `foo` on type `Foo`
  --> $DIR/E0609.rs:8:15
   |
LL |     let _ = x.foo;
   |               ^^^ unknown field
   |
   = note: available field is: `x`

error[E0609]: no field `1` on type `Bar`
  --> $DIR/E0609.rs:11:7
   |
LL |     y.1;
   |       ^ unknown field

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0609`.
"#);
}

#[test]
fn a_field_of_a_primitive() {
    // tests/ui/error-codes/E0610.rs
    let source = r#"fn main() {
    let x = 0;
    let _ = x.foo; //~ ERROR E0610
}
"#;
    assert_reports(&ui("E0610.rs", "2015", source), &[error(3, &[])]);
}

#[test]
fn a_method_taken_as_a_field() {
    // tests/ui/error-codes/E0615.rs
    let source = r#"struct Foo {
    x: u32,
}

impl Foo {
    fn method(&self) {}
}

fn main() {
    let f = Foo { x: 0 };
    f.method; //~ ERROR E0615
}
"#;
    assert_eq!(ui("E0615.rs", "2015", source), r#"error[E0615]: attempted to take value of method `method` on type `Foo`
  --> $DIR/E0615.rs:11:7
   |
LL |     f.method;
   |       ^^^^^^ method, not a field
   |
help: use parentheses to call the method
   |
LL |     f.method();
   |             ++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0615`.
"#);
}

#[test]
fn a_private_field_accessed() {
    // tests/ui/error-codes/E0616.rs
    let source = r#"mod a {
    pub struct Foo {
        x: u32,
    }

    impl Foo {
        pub fn new() -> Foo { Foo { x: 0 } }
    }
}

fn main() {
    let f = a::Foo::new();
    f.x; //~ ERROR E0616
}
"#;
    assert_eq!(ui("E0616.rs", "2015", source), r#"error[E0616]: field `x` of struct `Foo` is private
  --> $DIR/E0616.rs:13:7
   |
LL |     f.x;
   |       ^ private field

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0616`.
"#);
}

#[test]
fn a_private_method_called() {
    // tests/ui/error-codes/E0624.rs
    let source = r#"mod inner {
    pub struct Foo;

    impl Foo {
        fn method(&self) {}
    }
}

fn main() {
    let foo = inner::Foo;
    foo.method(); //~ ERROR method `method` is private [E0624]
}
"#;
    assert_eq!(ui("E0624.rs", "2015", source), r#"error[E0624]: method `method` is private
  --> $DIR/E0624.rs:11:9
   |
LL |         fn method(&self) {}
   |         ---------------- private method defined here
...
LL |     foo.method();
   |         ^^^^^^ private method

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0624`.
"#);
}

#[test]
fn a_value_resolve_cannot_find() {
    // tests/ui/error-codes/E0425.rs
    let source = r#"trait Foo {
    fn bar() {
        elf; //~ ERROR E0425
    }
}

fn main () {
}
"#;
    assert_eq!(ui("E0425.rs", "2015", source), r#"error[E0425]: cannot find value `elf` in this scope
  --> $DIR/E0425.rs:3:9
   |
LL |         elf;
   |         ^^^ not found in this scope

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0425`.
"#);
}

#[test]
fn self_in_a_function_without_a_self_parameter() {
    // tests/ui/error-codes/E0424.rs
    let source = r#"struct Foo;

impl Foo {
    fn bar(self) {}

    fn foo() {
        self.bar(); //~ ERROR E0424
    }

    fn baz(_: i32) {
        self.bar(); //~ ERROR E0424
    }

    fn qux() {
        let _ = || self.bar(); //~ ERROR E0424
    }
}

fn main () {
    let self = "self"; //~ ERROR E0424
}
"#;
    assert_eq!(ui("E0424.rs", "2015", source), r#"error[E0424]: cannot find value `self` in this scope
  --> $DIR/E0424.rs:7:9
   |
LL |     fn foo() {
   |        --- this function doesn't have a `self` parameter
LL |         self.bar();
   |         ^^^^ `self` value is a keyword only available in methods with a `self` parameter
   |
   = note: a module named `self` exists in another namespace
help: add a `self` receiver parameter to make the associated `fn` a method
   |
LL |     fn foo(&self) {
   |            +++++

error[E0424]: cannot find value `self` in this scope
  --> $DIR/E0424.rs:11:9
   |
LL |     fn baz(_: i32) {
   |        --- this function doesn't have a `self` parameter
LL |         self.bar();
   |         ^^^^ `self` value is a keyword only available in methods with a `self` parameter
   |
   = note: a module named `self` exists in another namespace
help: add a `self` receiver parameter to make the associated `fn` a method
   |
LL |     fn baz(&self, _: i32) {
   |            ++++++

error[E0424]: cannot find value `self` in this scope
  --> $DIR/E0424.rs:15:20
   |
LL |     fn qux() {
   |        --- this function doesn't have a `self` parameter
LL |         let _ = || self.bar();
   |                    ^^^^ `self` value is a keyword only available in methods with a `self` parameter
   |
   = note: a module named `self` exists in another namespace
help: add a `self` receiver parameter to make the associated `fn` a method
   |
LL |     fn qux(&self) {
   |            +++++

error[E0424]: cannot find unit struct, unit variant or constant `self` in this scope
  --> $DIR/E0424.rs:20:9
   |
LL |     let self = "self";
   |         ^^^^ `self` value is a keyword and may not be bound to variables or shadowed
   |
   = note: a module named `self` exists in another namespace

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0424`.
"#);
}

#[test]
fn a_name_is_not_suggested_as_its_underscore() {
    // tests/ui/resolve/typo-suggestion-named-underscore.rs
    let source = r#"const _: () = ();

fn main() {
    a // Shouldn't suggest underscore
    //~^ ERROR: cannot find value `a` in this scope
}

trait Unknown {}

#[allow(unused_imports)]
use Unknown as _;

fn foo<T: A>(x: T) {} // Shouldn't suggest underscore
//~^ ERROR: cannot find trait `A` in this scope
"#;
    assert_eq!(ui("typo-suggestion-named-underscore.rs", "2015", source), r#"error[E0425]: cannot find value `a` in this scope
  --> $DIR/typo-suggestion-named-underscore.rs:4:5
   |
LL |     a // Shouldn't suggest underscore
   |     ^ not found in this scope

error[E0405]: cannot find trait `A` in this scope
  --> $DIR/typo-suggestion-named-underscore.rs:13:11
   |
LL | fn foo<T: A>(x: T) {} // Shouldn't suggest underscore
   |           ^ not found in this scope

error: aborting due to 2 previous errors

Some errors have detailed explanations: E0405, E0425.
For more information about an error, try `rustc --explain E0405`.
"#);
}

/// A name a format string captures is reported at its place in the string.
#[test]
fn a_value_a_format_string_captures() {
    // tests/ui/fmt/format-args-capture-missing-variables.rs
    let source = r###"fn main() {
    format!("{} {foo} {} {bar} {}", 1, 2, 3);
    //~^ ERROR: cannot find value `foo` in this scope
    //~^^ ERROR: cannot find value `bar` in this scope

    format!("{foo}"); //~ ERROR: cannot find value `foo` in this scope

    format!("{valuea} {valueb}", valuea=5, valuec=7);
    //~^ ERROR cannot find value `valueb` in this scope
    //~^^ ERROR named argument never used

    format!(r##"

        {foo}

    "##);
    //~^^^ ERROR: cannot find value `foo` in this scope

    panic!("{foo} {bar}", bar=1); //~ ERROR: cannot find value `foo` in this scope
}
"###;
    let fixture = format!(
        "//- minicore: fmt, panic\n//- /std.rs crate:std\n\
         pub mod prelude {{ pub mod rust_2015 {{ pub use core::prelude::rust_2015::*; pub use crate::{{format, panic}}; }} }}\n\
         #[macro_export]\nmacro_rules! format {{ ($($arg:tt)*) => {{ $crate::fmt::format(::core::format_args!($($arg)*)) }} }}\n\
         #[macro_export]\nmacro_rules! panic {{ ($($arg:tt)*) => {{ ::core::panic!($($arg)*) }} }}\n\
         pub mod fmt {{ pub fn format(_: ::core::fmt::Arguments<'_>) {{}} }}\n\
         //- /format-args-capture-missing-variables.rs crate:other:main edition:2015 deps:std\n{source}"
    );
    assert_reports(
        &ui_fixture(&fixture),
        &[
            error(8, &[]),
            error(2, &["`foo`"]),
            error(2, &["`bar`"]),
            error(6, &["`foo`"]),
            error(8, &["`valueb`"]),
            error(14, &["`foo`"]),
            error(19, &["`foo`"]),
        ],
    );
}

const E0063_ONLY: [&str; 1] = [
    "E0063: missing field `dir` in initializer of `WorkerConfig`",
];

/// #1522: the case it was found in, a literal inside another in a closure of a
/// `#[cfg(test)]` module, is an error of the crate built for tests.
#[test]
fn a_missing_field_in_a_closure_of_a_test_module() {
    let checked = checked(
        r#"
//- minicore: fn
//- /lib.rs crate:main cfg:test
pub struct WorkerConfig { pub jobs: u32, pub dir: u32 }
pub enum Ahead { Config { config: WorkerConfig } }

#[cfg(test)]
mod tests {
    use super::*;

    fn ahead() -> Ahead {
        let make = || Ahead::Config { config: WorkerConfig { jobs: 1 } };
        make()
    }
}
"#,
    );
    assert_eq!(checked.messages(), E0063_ONLY);
}

/// #1522: an inline module inside another is checked as the file's own items are.
#[test]
fn a_missing_field_in_a_nested_inline_module() {
    let checked = checked(
        r#"
//- /lib.rs crate:main cfg:test
pub struct WorkerConfig { pub jobs: u32, pub dir: u32 }

pub mod outer {
    pub mod inner {
        pub fn config() -> crate::WorkerConfig {
            crate::WorkerConfig { jobs: 1 }
        }
    }
}
"#,
    );
    assert_eq!(checked.messages(), E0063_ONLY);
}

/// #1522: a literal a `macro_rules!` expansion writes, in a `#[cfg(test)]` module.
#[test]
fn a_missing_field_in_a_macro_expansion() {
    let checked = checked(
        r#"
//- /lib.rs crate:main cfg:test
pub struct WorkerConfig { pub jobs: u32, pub dir: u32 }

macro_rules! config {
    ($jobs:expr) => {
        WorkerConfig { jobs: $jobs }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WorkerConfig {
        config!(1)
    }
}
"#,
    );
    assert_eq!(checked.messages(), E0063_ONLY);
}

#[test]
fn a_range_typo_in_the_last_field_takes_the_place_of_its_mismatch() {
    // tests/ui/structs/struct-record-suggestion.rs, its `a`: the E0308 of `5..Default::default()`
    // is stashed and the E0063 with the range note takes its place. `#[derive]` keeps only
    // `Default` and `assert_eq!` is a `let`, which leaves the lines where they are; `b` (the
    // E0308 that gets the note itself, which prints `std::ops::Range`) needs std.
    let source = r#"//@ run-rustfix
#[derive(Default)]
struct A {
    b: u32,
    c: u64,
    d: usize,
}

fn a() {
    let q = A { c: 5..Default::default() };
    //~^ ERROR missing fields
    //~| HELP separate the last named field with a comma
    let r = A { c: 5, ..Default::default() };
    let _ = (q, r);
}

fn main() {
    a();
}
"#;
    assert_eq!(
        ui_core("struct-record-suggestion.rs", "default, derive, builtin_impls, range", source),
        r#"error[E0063]: missing fields `b` and `d` in initializer of `A`
  --> $DIR/struct-record-suggestion.rs:10:13
   |
LL |     let q = A { c: 5..Default::default() };
   |             ^ missing `b` and `d`
   |
note: this expression may have been misinterpreted as a `..` range expression
  --> $DIR/struct-record-suggestion.rs:10:20
   |
LL |     let q = A { c: 5..Default::default() };
   |                    ^^^^^^^^^^^^^^^^^^^^^
help: to set the remaining fields from `Default::default()`, separate the last named field with a comma
   |
LL |     let q = A { c: 5, ..Default::default() };
   |                     +

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0063`.
"#
    );
}
