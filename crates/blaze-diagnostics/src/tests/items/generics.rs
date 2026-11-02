//! rustc's UI tests of the errors of generic arguments and placeholders in signatures: counts
//! (E0107), order (E0747), arguments on segments that take none (E0109), elided lifetimes in
//! paths (E0726), `Self` in defaults (E0735), `_` in item signatures (E0121), return type
//! notation out of place, const parameters of a type a value tree can't hold (E0741). Each test's file is the UI test's source byte for byte. Patterns in
//! foreign functions (E0130) and items without bodies are AST validation's
//! (`tests/items/ast_validation.rs`).

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, items::ui_core, normalized, ui},
};

#[test]
fn type_arguments_on_a_primitive() {
    // tests/ui/error-codes/E0109.rs
    let source = r#"type X = u32<i32>; //~ ERROR E0109

fn main() {
}
"#;
    let stderr = r#"error[E0109]: type arguments are not allowed on builtin type `u32`
  --> $DIR/E0109.rs:1:14
   |
LL | type X = u32<i32>;
   |          --- ^^^ type argument not allowed
   |          |
   |          not allowed on builtin type `u32`
   |
help: primitive type `u32` doesn't have generic parameters
   |
LL - type X = u32<i32>;
LL + type X = u32;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0109`.
"#;
    assert_eq!(ui("E0109.rs", "2015", source), stderr);
}

#[test]
fn wrong_numbers_of_lifetime_and_generic_arguments() {
    // tests/ui/error-codes/E0107.rs
    let source = r#"struct Foo<'a>(&'a str);
struct Buzz<'a, 'b>(&'a str, &'b str);
struct Qux<'a, T>(&'a T);
struct Quux<T>(T);

enum Bar {
    A,
    B,
    C,
}

struct Baz<'a, 'b, 'c> {
    buzz: Buzz<'a>,
    //~^ ERROR struct takes 2 lifetime arguments
    //~| HELP add missing lifetime argument

    bar: Bar<'a>,
    //~^ ERROR enum takes 0 lifetime arguments
    //~| HELP remove the unnecessary generics

    foo2: Foo<'a, 'b, 'c>,
    //~^ ERROR struct takes 1 lifetime argument
    //~| HELP remove the lifetime arguments

    qux1: Qux<'a, 'b, i32>,
    //~^ ERROR struct takes 1 lifetime argument
    //~| HELP remove the lifetime argument

    qux2: Qux<'a, i32, 'b>,
    //~^ ERROR struct takes 1 lifetime argument
    //~| HELP remove the lifetime argument

    qux3: Qux<'a, 'b, 'c, i32>,
    //~^ ERROR struct takes 1 lifetime argument
    //~| HELP remove the lifetime arguments

    qux4: Qux<'a, i32, 'b, 'c>,
    //~^ ERROR struct takes 1 lifetime argument
    //~| HELP remove the lifetime arguments

    qux5: Qux<'a, 'b, i32, 'c>,
    //~^ ERROR struct takes 1 lifetime argument
    //~| HELP remove the lifetime argument

    quux: Quux<'a, i32, 'b>,
    //~^ ERROR struct takes 0 lifetime arguments
    //~| HELP remove the lifetime argument
}

pub trait T {
    type A;
    type B;
}

fn trait_bound_generic<I: T<u8, u16>>(_i: I) {
    //~^ ERROR trait takes 0 generic arguments
    //~| HELP turn the generic arguments into associated item bindings
}

fn main() {}
"#;
    let stderr = r#"error[E0107]: struct takes 2 lifetime arguments but 1 lifetime argument was supplied
  --> $DIR/E0107.rs:13:11
   |
LL |     buzz: Buzz<'a>,
   |           ^^^^ -- supplied 1 lifetime argument
   |           |
   |           expected 2 lifetime arguments
   |
note: struct defined here, with 2 lifetime parameters: `'a`, `'b`
  --> $DIR/E0107.rs:2:8
   |
LL | struct Buzz<'a, 'b>(&'a str, &'b str);
   |        ^^^^ --  --
help: add missing lifetime argument
   |
LL |     buzz: Buzz<'a, 'a>,
   |                  ++++

error[E0107]: enum takes 0 lifetime arguments but 1 lifetime argument was supplied
  --> $DIR/E0107.rs:17:10
   |
LL |     bar: Bar<'a>,
   |          ^^^---- help: remove the unnecessary generics
   |          |
   |          expected 0 lifetime arguments
   |
note: enum defined here, with 0 lifetime parameters
  --> $DIR/E0107.rs:6:6
   |
LL | enum Bar {
   |      ^^^

error[E0107]: struct takes 1 lifetime argument but 3 lifetime arguments were supplied
  --> $DIR/E0107.rs:21:11
   |
LL |     foo2: Foo<'a, 'b, 'c>,
   |           ^^^   -------- help: remove the lifetime arguments
   |           |
   |           expected 1 lifetime argument
   |
note: struct defined here, with 1 lifetime parameter: `'a`
  --> $DIR/E0107.rs:1:8
   |
LL | struct Foo<'a>(&'a str);
   |        ^^^ --

error[E0107]: struct takes 1 lifetime argument but 2 lifetime arguments were supplied
  --> $DIR/E0107.rs:25:11
   |
LL |     qux1: Qux<'a, 'b, i32>,
   |           ^^^   ---- help: remove the lifetime argument
   |           |
   |           expected 1 lifetime argument
   |
note: struct defined here, with 1 lifetime parameter: `'a`
  --> $DIR/E0107.rs:3:8
   |
LL | struct Qux<'a, T>(&'a T);
   |        ^^^ --

error[E0107]: struct takes 1 lifetime argument but 2 lifetime arguments were supplied
  --> $DIR/E0107.rs:29:11
   |
LL |     qux2: Qux<'a, i32, 'b>,
   |           ^^^   --------- help: remove the lifetime argument
   |           |
   |           expected 1 lifetime argument
   |
note: struct defined here, with 1 lifetime parameter: `'a`
  --> $DIR/E0107.rs:3:8
   |
LL | struct Qux<'a, T>(&'a T);
   |        ^^^ --

error[E0107]: struct takes 1 lifetime argument but 3 lifetime arguments were supplied
  --> $DIR/E0107.rs:33:11
   |
LL |     qux3: Qux<'a, 'b, 'c, i32>,
   |           ^^^   -------- help: remove the lifetime arguments
   |           |
   |           expected 1 lifetime argument
   |
note: struct defined here, with 1 lifetime parameter: `'a`
  --> $DIR/E0107.rs:3:8
   |
LL | struct Qux<'a, T>(&'a T);
   |        ^^^ --

error[E0107]: struct takes 1 lifetime argument but 3 lifetime arguments were supplied
  --> $DIR/E0107.rs:37:11
   |
LL |     qux4: Qux<'a, i32, 'b, 'c>,
   |           ^^^   ------------- help: remove the lifetime arguments
   |           |
   |           expected 1 lifetime argument
   |
note: struct defined here, with 1 lifetime parameter: `'a`
  --> $DIR/E0107.rs:3:8
   |
LL | struct Qux<'a, T>(&'a T);
   |        ^^^ --

error[E0107]: struct takes 1 lifetime argument but 3 lifetime arguments were supplied
  --> $DIR/E0107.rs:41:11
   |
LL |     qux5: Qux<'a, 'b, i32, 'c>,
   |           ^^^   ---- help: remove the lifetime argument
   |           |
   |           expected 1 lifetime argument
   |
note: struct defined here, with 1 lifetime parameter: `'a`
  --> $DIR/E0107.rs:3:8
   |
LL | struct Qux<'a, T>(&'a T);
   |        ^^^ --

error[E0107]: struct takes 0 lifetime arguments but 2 lifetime arguments were supplied
  --> $DIR/E0107.rs:45:11
   |
LL |     quux: Quux<'a, i32, 'b>,
   |           ^^^^ -- help: remove the lifetime argument
   |           |
   |           expected 0 lifetime arguments
   |
note: struct defined here, with 0 lifetime parameters
  --> $DIR/E0107.rs:4:8
   |
LL | struct Quux<T>(T);
   |        ^^^^

error[E0107]: trait takes 0 generic arguments but 2 generic arguments were supplied
  --> $DIR/E0107.rs:55:27
   |
LL | fn trait_bound_generic<I: T<u8, u16>>(_i: I) {
   |                           ^ expected 0 generic arguments
   |
note: trait defined here, with 0 generic parameters
  --> $DIR/E0107.rs:50:11
   |
LL | pub trait T {
   |           ^
help: turn the generic arguments into associated item bindings
   |
LL | fn trait_bound_generic<I: T<A = u8, B = u16>>(_i: I) {
   |                             +++     +++

error: aborting due to 10 previous errors

For more information about this error, try `rustc --explain E0107`.
"#;
    assert_eq!(ui("E0107.rs", "2015", source), stderr);
}

#[test]
fn array_types_given_for_a_const_parameter() {
    // tests/ui/const-generics/suggest_const_for_array.rs
    let source = r#"#![crate_type = "lib"]

fn example<const N: usize>() {}

fn other() {
    example::<[usize; 3]>();
    //~^ ERROR type provided when a const
    example::<[usize; 4 + 5]>();
    //~^ ERROR type provided when a const
}
"#;
    let stderr = r#"error[E0747]: type provided when a constant was expected
  --> $DIR/suggest_const_for_array.rs:6:15
   |
LL |     example::<[usize; 3]>();
   |               ^^^^^^^^^^ help: array type provided where a `usize` was expected, try: `{ 3 }`

error[E0747]: type provided when a constant was expected
  --> $DIR/suggest_const_for_array.rs:8:15
   |
LL |     example::<[usize; 4 + 5]>();
   |               ^^^^^^^^^^^^^^ help: array type provided where a `usize` was expected, try: `{ 4 + 5 }`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0747`.
"#;
    assert_eq!(ui("suggest_const_for_array.rs", "2015", source), stderr);
}

#[test]
fn an_elided_lifetime_in_an_impl_header() {
    // tests/ui/impl-header-lifetime-elision/path-elided.rs
    let source = r#"#![allow(warnings)]

trait MyTrait { }

struct Foo<'a> { x: &'a u32 }

impl MyTrait for Foo {
    //~^ ERROR implicit elided lifetime not allowed here
}

fn main() {}
"#;
    let stderr = r#"error[E0726]: implicit elided lifetime not allowed here
  --> $DIR/path-elided.rs:7:18
   |
LL | impl MyTrait for Foo {
   |                  ^^^ expected lifetime parameter
   |
help: indicate the anonymous lifetime
   |
LL | impl MyTrait for Foo<'_> {
   |                     ++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0726`.
"#;
    assert_eq!(ui("path-elided.rs", "2015", source), stderr);
}

#[test]
fn self_in_the_default_of_a_type_parameter() {
    // tests/ui/generics/issue-61631-default-type-param-cannot-reference-self.rs
    let source = r#"#![crate_type="lib"]

// rust-lang/rust#61631: Uses of `Self` in the defaults of generic
// types for ADT's are not allowed. We justify this because the `Self`
// type could be considered the "final" type parameter, that is only
// well-defined after all of the other type parameters on the ADT have
// been instantiated.
//
// These were previously were ICE'ing at the usage point anyway (see
// `demo_usages` below), so there should not be any backwards
// compatibility concern.

struct Snobound<'a, P = Self> { x: Option<&'a P> }
//~^ ERROR generic parameters cannot use `Self` in their defaults [E0735]

enum Enobound<'a, P = Self> { A, B(Option<&'a P>) }
//~^ ERROR generic parameters cannot use `Self` in their defaults [E0735]

union Unobound<'a, P = Self> { x: i32, y: Option<&'a P> }
//~^ ERROR generic parameters cannot use `Self` in their defaults [E0735]

// Disallowing `Self` in defaults sidesteps need to check the bounds
// on the defaults in cases like these.

struct Ssized<'a, P: Sized = [Self]> { x: Option<&'a P> }
//~^ ERROR generic parameters cannot use `Self` in their defaults [E0735]

enum Esized<'a, P: Sized = [Self]> { A, B(Option<&'a P>) }
//~^ ERROR generic parameters cannot use `Self` in their defaults [E0735]

union Usized<'a, P: Sized = [Self]> { x: i32, y: Option<&'a P> }
//~^ ERROR generic parameters cannot use `Self` in their defaults [E0735]

fn demo_usages() {
    // An ICE means you only get the error from the first line of the
    // demo; comment each out to observe the other ICEs when trying
    // this out on older versions of Rust.

    let _ice: Snobound;
    let _ice: Enobound;
    let _ice: Unobound;
    let _ice: Ssized;
    let _ice: Esized;
    let _ice: Usized;
}
"#;
    let stderr = r#"error[E0735]: generic parameters cannot use `Self` in their defaults
  --> $DIR/issue-61631-default-type-param-cannot-reference-self.rs:13:25
   |
LL | struct Snobound<'a, P = Self> { x: Option<&'a P> }
   |                         ^^^^

error[E0735]: generic parameters cannot use `Self` in their defaults
  --> $DIR/issue-61631-default-type-param-cannot-reference-self.rs:16:23
   |
LL | enum Enobound<'a, P = Self> { A, B(Option<&'a P>) }
   |                       ^^^^

error[E0735]: generic parameters cannot use `Self` in their defaults
  --> $DIR/issue-61631-default-type-param-cannot-reference-self.rs:19:24
   |
LL | union Unobound<'a, P = Self> { x: i32, y: Option<&'a P> }
   |                        ^^^^

error[E0735]: generic parameters cannot use `Self` in their defaults
  --> $DIR/issue-61631-default-type-param-cannot-reference-self.rs:25:31
   |
LL | struct Ssized<'a, P: Sized = [Self]> { x: Option<&'a P> }
   |                               ^^^^

error[E0735]: generic parameters cannot use `Self` in their defaults
  --> $DIR/issue-61631-default-type-param-cannot-reference-self.rs:28:29
   |
LL | enum Esized<'a, P: Sized = [Self]> { A, B(Option<&'a P>) }
   |                             ^^^^

error[E0735]: generic parameters cannot use `Self` in their defaults
  --> $DIR/issue-61631-default-type-param-cannot-reference-self.rs:31:30
   |
LL | union Usized<'a, P: Sized = [Self]> { x: i32, y: Option<&'a P> }
   |                              ^^^^

error: aborting due to 6 previous errors

For more information about this error, try `rustc --explain E0735`.
"#;
    assert_eq!(
        ui_core(
            "issue-61631-default-type-param-cannot-reference-self.rs",
            "2015",
            "option, sized",
            source
        ),
        stderr
    );
}

#[test]
fn placeholders_in_a_return_type_and_a_static() {
    // tests/ui/error-codes/E0121.rs
    let source = r#"fn foo() -> _ { 5 } //~ ERROR E0121

static BAR: _ = "test"; //~ ERROR E0121

fn main() {}
"#;
    assert_reports(&ui("E0121.rs", "2015", source), &[error(1, &["`_`"]), error(3, &["`_`"])]);
}

#[test]
fn return_type_notation_on_a_constant_and_as_a_type() {
    // tests/ui/associated-type-bounds/return-type-notation/bare-path.rs
    let source = r#"#![feature(return_type_notation)]

trait Tr {
    const CONST: usize;

    fn method() -> impl Sized;
}

fn foo<T: Tr>()
where
    T::method(..): Send,
    <T as Tr>::method(..): Send,
{
    let _ = T::CONST::(..);
    //~^ ERROR return type notation not allowed in this position yet
    let _: T::method(..);
    //~^ ERROR return type notation not allowed in this position yet
}

fn main() {}
"#;
    assert_reports(&ui_core("bare-path.rs", "2015", "send, sized", source), &[error(14, &[]), error(16, &[])]);
}

/// [`ui_core`] of a revision: `--cfg {revision}`, as compiletest passes it.
fn ui_revision(name: &str, revision: &str, flags: &str, source: &str) -> String {
    let fixture =
        format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:2015 cfg:{revision}\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

// tests/ui/const-generics/float-generic.rs
const FLOAT_GENERIC: &str = r#"//@ revisions: simple adt_const_params full
#![cfg_attr(full, feature(adt_const_params, unsized_const_params))]
#![cfg_attr(full, allow(incomplete_features))]
#![cfg_attr(adt_const_params, feature(adt_const_params))]
#![cfg_attr(adt_const_params, allow(incomplete_features))]

fn foo<const F: f32>() {}
//~^ ERROR `f32` is forbidden as the type of a const generic parameter

const C: f32 = 1.0;

fn main() {
    foo::<C>();
}
"#;

#[test]
fn a_float_const_parameter_without_the_feature() {
    let stderr = r#"error: `f32` is forbidden as the type of a const generic parameter
  --> $DIR/float-generic.rs:7:17
   |
LL | fn foo<const F: f32>() {}
   |                 ^^^
   |
   = note: the only supported types are integers, `bool`, and `char`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui_revision("float-generic.rs", "simple", "sized", FLOAT_GENERIC), stderr);
}

#[test]
fn a_float_const_parameter_with_adt_const_params_is_e0741() {
    let stderr = r#"error[E0741]: `f32` is forbidden as the type of a const generic parameter
  --> $DIR/float-generic.rs:7:17
   |
LL | fn foo<const F: f32>() {}
   |                 ^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0741`.
"#;
    assert_eq!(
        ui_revision("float-generic.rs", "adt_const_params", "sized, const_param_ty", FLOAT_GENERIC),
        stderr
    );
}
