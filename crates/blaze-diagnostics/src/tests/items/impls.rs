//! rustc's UI tests of the errors of impls: coherence (E0116, E0117, E0118, E0390, E0120,
//! E0204, E0184, E0206, E0366), the safety of impls (E0199, E0200), the items of a trait impl
//! against its trait's (E0046, E0049, E0050, E0185, E0186, E0407, E0437, E0438) and items
//! given twice (E0201, E0592). Each test's file is the UI test's source byte for byte. An
//! `unsafe` negative impl (E0198) is AST validation's (`tests/items/ast_validation.rs`).

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

/// [`ui`] of a file with minicore's items for `flags` (`crates/test-utils/src/minicore.rs`).
fn ui_core(name: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:2015\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// [`ui_core`] of a file that uses std: a crate `std` of `std_items` over minicore, whose
/// prelude is core's and `prelude`.
fn ui_std(name: &str, flags: &str, std_items: &str, prelude: &str, source: &str) -> String {
    let fixture = format!(
        "//- minicore: {flags}\n//- /std.rs crate:std\n\
         pub mod prelude {{ pub mod rust_2015 {{ pub use core::prelude::rust_2015::*; {prelude} }} }}\n\
         {std_items}//- /{name} crate:other:main edition:2015 deps:std\n{source}"
    );
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

#[test]
fn an_inherent_impl_of_another_crates_type() {
    // tests/ui/error-codes/E0116.rs
    let source = r#"impl Vec<u8> {}
//~^ ERROR E0116

fn main() {
}
"#;
    let stderr = r#"error[E0116]: cannot define inherent `impl` for a type outside of the crate where the type is defined
  --> $DIR/E0116.rs:1:1
   |
LL | impl Vec<u8> {}
   | ^^^^^^^^^^^^ impl for type defined outside of crate
   |
   = help: consider defining a trait and implementing it for the type or using a newtype wrapper like `struct MyType(ExternalType);` and implement it
   = note: for more details about the orphan rules, see <https://doc.rust-lang.org/reference/items/implementations.html?highlight=orphan#orphan-rules>

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0116`.
"#;
    assert_eq!(ui_std("E0116.rs", "sized", r#"pub struct Vec<T>(T);
"#, "pub use crate::Vec;", source), stderr);
}

#[test]
fn a_foreign_trait_for_a_primitive_and_drop_of_a_primitive() {
    // tests/ui/error-codes/E0117.rs
    let source = r#"impl Drop for u32 {} //~ ERROR E0117
//~| ERROR the `Drop` trait may only be implemented for local structs, enums, and unions

fn main() {}
"#;
    let stderr = r#"error[E0117]: only traits defined in the current crate can be implemented for primitive types
  --> $DIR/E0117.rs:1:1
   |
LL | impl Drop for u32 {}
   | ^^^^^^^^^^^^^^---
   |               |
   |               `u32` is not defined in the current crate
   |
   = note: impl doesn't have any local type before any uncovered type parameters
   = note: for more information see https://doc.rust-lang.org/reference/items/implementations.html#orphan-rules
   = note: define and implement a trait or new type instead

error[E0120]: the `Drop` trait may only be implemented for local structs, enums, and unions
  --> $DIR/E0117.rs:1:15
   |
LL | impl Drop for u32 {}
   |               ^^^ must be a struct, enum, or union in the current crate

error: aborting due to 2 previous errors

Some errors have detailed explanations: E0117, E0120.
For more information about an error, try `rustc --explain E0117`.
"#;
    assert_eq!(ui_core("E0117.rs", "drop", source), stderr);
}

#[test]
fn an_inherent_impl_of_a_type_parameter() {
    // tests/ui/error-codes/E0118.rs
    let source = r#"impl<T> T { //~ ERROR E0118
    fn get_state(&self) -> String {
       String::new()
    }
}

fn main() {}
"#;
    assert_reports(&ui_std("E0118.rs", "sized", r#"pub struct String;
impl String { pub fn new() -> String { String } }
"#, "pub use crate::String;", source), &[error(1, &[])]);
}

#[test]
fn inherent_impls_of_primitive_types() {
    // tests/ui/error-codes/E0390.rs
    let source = r#"struct Foo {
    x: i32
}

impl *mut Foo {} //~ ERROR E0390

impl fn(Foo) {} //~ ERROR E0390

fn main() {
}
"#;
    assert_reports(&ui("E0390.rs", "2015", source), &[error(5, &["`impl`"]), error(7, &["`impl`"])]);
}

#[test]
fn an_unsafe_impl_of_a_safe_trait() {
    // tests/ui/error-codes/E0199.rs
    let source = r#"#![feature(negative_impls)]

struct Foo;

trait Bar { }
unsafe impl Bar for Foo { } //~ ERROR implementing the trait `Bar` is not unsafe [E0199]

fn main() {
}
"#;
    let stderr = r#"error[E0199]: implementing the trait `Bar` is not unsafe
  --> $DIR/E0199.rs:6:1
   |
LL | unsafe impl Bar for Foo { }
   | ^^^^^^^^^^^^^^^^^^^^^^^
   |
help: remove `unsafe` from this trait implementation
   |
LL - unsafe impl Bar for Foo { }
LL + impl Bar for Foo { }
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0199`.
"#;
    assert_eq!(ui("E0199.rs", "2015", source), stderr);
}

#[test]
fn a_safe_impl_of_an_unsafe_trait() {
    // tests/ui/error-codes/E0200.rs
    let source = r#"struct Foo;

unsafe trait Bar { }

impl Bar for Foo { } //~ ERROR E0200

fn main() {
}
"#;
    let stderr = r#"error[E0200]: the trait `Bar` requires an `unsafe impl` declaration
  --> $DIR/E0200.rs:5:1
   |
LL | impl Bar for Foo { }
   | ^^^^^^^^^^^^^^^^
   |
   = note: the trait `Bar` enforces invariants that the compiler can't check. Review the trait documentation and make sure this implementation upholds those invariants before adding the `unsafe` keyword
help: add `unsafe` to this trait implementation
   |
LL | unsafe impl Bar for Foo { }
   | ++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0200`.
"#;
    assert_eq!(ui("E0200.rs", "2015", source), stderr);
}

#[test]
fn a_method_not_in_the_trait() {
    // tests/ui/error-codes/E0407.rs
    let source = r#"trait Foo {
    fn a();
}

struct Bar;

impl Foo for Bar {
    fn a() {}
    fn b() {}
    //~^ ERROR E0407
}

fn main() {
}
"#;
    let stderr = r#"error[E0407]: method `b` is not a member of trait `Foo`
  --> $DIR/E0407.rs:9:5
   |
LL |     fn b() {}
   |     ^^^-^^^^^
   |     |  |
   |     |  help: there is an associated function with a similar name: `a`
   |     not a member of trait `Foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0407`.
"#;
    assert_eq!(ui("E0407.rs", "2015", source), stderr);
}

#[test]
fn a_type_not_in_the_trait() {
    // tests/ui/error-codes/E0437.rs
    let source = r#"trait Foo {}

impl Foo for i32 {
    type Bar = bool; //~ ERROR E0437
}

fn main () {
}
"#;
    let stderr = r#"error[E0437]: type `Bar` is not a member of trait `Foo`
  --> $DIR/E0437.rs:4:5
   |
LL |     type Bar = bool;
   |     ^^^^^^^^^^^^^^^^ not a member of trait `Foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0437`.
"#;
    assert_eq!(ui("E0437.rs", "2015", source), stderr);
}

#[test]
fn a_const_not_in_the_trait() {
    // tests/ui/error-codes/E0438.rs
    let source = r#"trait Bar {}

impl Bar for i32 {
    const BAR: bool = true; //~ ERROR E0438
}

fn main () {
}
"#;
    let stderr = r#"error[E0438]: const `BAR` is not a member of trait `Bar`
  --> $DIR/E0438.rs:4:5
   |
LL |     const BAR: bool = true;
   |     ^^^^^^^^^^^^^^^^^^^^^^^ not a member of trait `Bar`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0438`.
"#;
    assert_eq!(ui("E0438.rs", "2015", source), stderr);
}

#[test]
fn a_missing_drop_method() {
    // tests/ui/drop/missing-drop-method.rs
    let source = r#"struct DropNoMethod;
impl Drop for DropNoMethod {} //~ ERROR not all trait items implemented, missing: `drop`

fn main() {}
"#;
    assert_reports(&ui_core("missing-drop-method.rs", "drop", source), &[error(2, &["`drop`"])]);
}

#[test]
fn fewer_type_parameters_than_the_trait() {
    // tests/ui/error-codes/E0049.rs
    let source = r#"trait Foo {
    fn foo<T: Default>(x: T) -> Self;
}

struct Bar;

impl Foo for Bar {
    fn foo(x: bool) -> Self { Bar } //~ ERROR E0049
}

trait Fuzz {
    fn fuzz<A: Default, B>(x: A, y: B) -> Self;
}

struct Baz;

impl Fuzz for Baz {
    fn fuzz(x: bool, y: bool) -> Self { Baz } //~ ERROR E0049
}

fn main() {
}
"#;
    assert_reports(&ui_core("E0049.rs", "default", source), &[error(8, &["`foo`"]), error(18, &["`fuzz`"])]);
}

#[test]
fn another_number_of_parameters_than_the_trait() {
    // tests/ui/error-codes/E0050.rs
    let source = r#"trait Foo {
    fn foo(&self, x: u8) -> bool;
    fn bar(&self, x: u8, y: u8, z: u8);
    fn less(&self);
}

struct Bar;

impl Foo for Bar {
    fn foo(&self) -> bool { true } //~ ERROR E0050
    fn bar(&self) { } //~ ERROR E0050
    fn less(&self, x: u8, y: u8, z: u8) { } //~ ERROR E0050
}

fn main() {
}
"#;
    let stderr = r#"error[E0050]: method `foo` has 1 parameter but the declaration in trait `Foo::foo` has 2
  --> $DIR/E0050.rs:10:12
   |
LL |     fn foo(&self, x: u8) -> bool;
   |            ------------ trait requires 2 parameters
...
LL |     fn foo(&self) -> bool { true }
   |            ^^^^^ expected 2 parameters, found 1

error[E0050]: method `bar` has 1 parameter but the declaration in trait `Foo::bar` has 4
  --> $DIR/E0050.rs:11:12
   |
LL |     fn bar(&self, x: u8, y: u8, z: u8);
   |            -------------------------- trait requires 4 parameters
...
LL |     fn bar(&self) { }
   |            ^^^^^ expected 4 parameters, found 1

error[E0050]: method `less` has 4 parameters but the declaration in trait `Foo::less` has 1
  --> $DIR/E0050.rs:12:13
   |
LL |     fn less(&self);
   |             ----- trait requires 1 parameter
...
LL |     fn less(&self, x: u8, y: u8, z: u8) { }
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^ expected 1 parameter, found 4

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0050`.
"#;
    assert_eq!(ui("E0050.rs", "2015", source), stderr);
}

#[test]
fn self_in_the_impl_but_not_the_trait() {
    // tests/ui/error-codes/E0185.rs
    let source = r#"trait Foo {
    fn foo();
    //~^ NOTE trait method declared without `&self`
}

struct Bar;

impl Foo for Bar {
    fn foo(&self) {}
    //~^ ERROR E0185
    //~| NOTE `&self` used in impl
}

fn main() {
}
"#;
    let stderr = r#"error[E0185]: method `foo` has a `&self` declaration in the impl, but not in the trait
  --> $DIR/E0185.rs:9:5
   |
LL |     fn foo();
   |     --------- trait method declared without `&self`
...
LL |     fn foo(&self) {}
   |     ^^^^^^^^^^^^^ `&self` used in impl

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0185`.
"#;
    assert_eq!(ui("E0185.rs", "2015", source), stderr);
}

#[test]
fn self_in_the_trait_but_not_the_impl() {
    // tests/ui/error-codes/E0186.rs
    let source = r#"trait Foo {
    fn foo(&self); //~ NOTE `&self` used in trait
}

struct Bar;

impl Foo for Bar {
    fn foo() {} //~ ERROR E0186
    //~^ NOTE expected `&self` in impl
}

fn main() {
}
"#;
    let stderr = r#"error[E0186]: method `foo` has a `&self` declaration in the trait, but not in the impl
  --> $DIR/E0186.rs:8:5
   |
LL |     fn foo(&self);
   |     -------------- `&self` used in trait
...
LL |     fn foo() {}
   |     ^^^^^^^^ expected `&self` in impl

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0186`.
"#;
    assert_eq!(ui("E0186.rs", "2015", source), stderr);
}

#[test]
fn items_given_twice_in_a_trait_impl_and_an_inherent_one() {
    // tests/ui/error-codes/E0201.rs
    let source = r#"struct Foo(u8);

impl Foo {
    fn bar(&self) -> bool { self.0 > 5 }
    fn bar() {} //~ ERROR E0592
}

trait Baz {
    type Quux;
    fn baz(&self) -> bool;
}

impl Baz for Foo {
    type Quux = u32;

    fn baz(&self) -> bool { true }
    fn baz(&self) -> bool { self.0 > 5 } //~ ERROR E0201
    type Quux = u32; //~ ERROR E0201
}

fn main() {
}
"#;
    let stderr = r#"error[E0201]: duplicate definitions with name `baz`:
  --> $DIR/E0201.rs:17:5
   |
LL |     fn baz(&self) -> bool;
   |     ---------------------- item in trait
...
LL |     fn baz(&self) -> bool { true }
   |     ------------------------------ previous definition here
LL |     fn baz(&self) -> bool { self.0 > 5 }
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ duplicate definition

error[E0201]: duplicate definitions with name `Quux`:
  --> $DIR/E0201.rs:18:5
   |
LL |     type Quux;
   |     ---------- item in trait
...
LL |     type Quux = u32;
   |     ---------------- previous definition here
...
LL |     type Quux = u32;
   |     ^^^^^^^^^^^^^^^^ duplicate definition

error[E0592]: duplicate definitions with name `bar`
  --> $DIR/E0201.rs:5:5
   |
LL |     fn bar(&self) -> bool { self.0 > 5 }
   |     --------------------- other definition for `bar`
LL |     fn bar() {}
   |     ^^^^^^^^ duplicate definitions for `bar`

error: aborting due to 3 previous errors

Some errors have detailed explanations: E0201, E0592.
For more information about an error, try `rustc --explain E0201`.
"#;
    assert_eq!(ui_core("E0201.rs", "copy", source), stderr);
}

#[test]
fn copy_of_a_type_with_a_destructor() {
    // tests/ui/error-codes/E0184.rs
    let source = r#"#[derive(Copy)]
struct Foo; //~ ERROR E0184

impl Drop for Foo {
    fn drop(&mut self) {
    }
}

fn main() {
}
"#;
    let stderr = r#"error[E0184]: the trait `Copy` cannot be implemented for this type; the type has a destructor
  --> $DIR/E0184.rs:2:8
   |
LL | #[derive(Copy)]
   |          ---- in this derive macro expansion
LL | struct Foo;
   |        ^^^ `Copy` not allowed on types with destructors
   |
note: destructor declared here
  --> $DIR/E0184.rs:5:5
   |
LL |     fn drop(&mut self) {
   |     ^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0184`.
"#;
    assert_eq!(ui_core("E0184.rs", "copy, drop, derive", source), stderr);
}

#[test]
fn copy_of_a_mutable_reference() {
    // tests/ui/error-codes/E0206.rs
    let source = r#"#[derive(Copy, Clone)]
struct Bar;

impl Copy for &'static mut Bar { }
//~^ ERROR the trait `Copy` cannot be implemented for this type

fn main() {
}
"#;
    let stderr = r#"error[E0206]: the trait `Copy` cannot be implemented for this type
  --> $DIR/E0206.rs:4:15
   |
LL | impl Copy for &'static mut Bar { }
   |               ^^^^^^^^^^^^^^^^ type is not a structure or enumeration

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0206`.
"#;
    assert_eq!(ui_core("E0206.rs", "copy, derive", source), stderr);
}

#[test]
fn drop_of_a_trait_object() {
    // tests/ui/error-codes/E0120.rs
    let source = r#"trait MyTrait { fn foo(&self) {} }

impl Drop for dyn MyTrait {
              //~^ ERROR E0120
    fn drop(&mut self) {}

}

fn main() {}
"#;
    let stderr = r#"error[E0120]: the `Drop` trait may only be implemented for local structs, enums, and unions
  --> $DIR/E0120.rs:3:15
   |
LL | impl Drop for dyn MyTrait {
   |               ^^^^^^^^^^^ must be a struct, enum, or union in the current crate

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0120`.
"#;
    assert_eq!(ui_core("E0120.rs", "drop", source), stderr);
}

#[test]
fn copy_with_a_field_of_a_parameter_without_copy() {
    // tests/ui/suggestions/missing-bound-in-manual-copy-impl.rs
    let source = r#"//@ run-rustfix
#![allow(dead_code)]

#[derive(Clone)]
struct Wrapper<T>(T);

impl<S> Copy for Wrapper<S> {}
//~^ ERROR the trait `Copy` cannot be implemented for this type

fn main() {}
"#;
    let stderr = r#"error[E0204]: the trait `Copy` cannot be implemented for this type
  --> $DIR/missing-bound-in-manual-copy-impl.rs:7:18
   |
LL | struct Wrapper<T>(T);
   |                   - this field does not implement `Copy`
LL |
LL | impl<S> Copy for Wrapper<S> {}
   |                  ^^^^^^^^^^
   |
help: consider restricting type parameter `S` with trait `Copy`
   |
LL | impl<S: Copy> Copy for Wrapper<S> {}
   |       ++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0204`.
"#;
    assert_eq!(ui_core("missing-bound-in-manual-copy-impl.rs", "copy, derive", source), stderr);
}

#[test]
fn drop_of_an_instantiation_of_its_type() {
    // tests/ui/dropck/issue-38868.rs
    let source = r#"pub struct List<T> {
    head: T,
}

impl Drop for List<i32> { //~ ERROR E0366
    fn drop(&mut self) {
        panic!()
    }
}

fn main() {
    List { head: 0 };
}
"#;
    assert_reports(&ui_core("issue-38868.rs", "drop, panic", source), &[error(5, &["`Drop`"])]);
}
