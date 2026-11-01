//! rustc's UI tests of `..` with no base in a struct expression: E0797 without
//! `#![feature(default_field_values)]`, and with it the struct that has no field to default.
//! Each test's file is the UI test's source byte for byte; a test whose file has other kinds'
//! errors too keeps only this kind's diagnostics.

use crate::{
    Options,
    emit::{Style, written},
    model::Diag,
    tests::{Mode, checked_with, compiletest, normalized, ui},
};

/// The stderr of `source` with only the diagnostics `keep` takes, without the closing ones, ending
/// at the last one's text: the empty line rustc writes after it comes before the ones left out.
fn ui_only(name: &str, source: &str, keep: impl Fn(&Diag) -> bool) -> String {
    let fixture = format!("//- /{name} crate:other:main edition:2015\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    let sources = checked.sources();
    let mut stderr: String = checked
        .diags
        .iter()
        .filter(|it| keep(it))
        .map(|it| written(it, &sources, Style { ui_testing: true, ..Style::default() }).unwrap())
        .collect();
    if stderr.ends_with("\n\n") {
        stderr.pop();
    }
    normalized(&stderr)
}

fn code<'a>(codes: &'a [&'a str]) -> impl Fn(&Diag) -> bool + 'a {
    move |it| it.code.as_ref().is_some_and(|it| codes.contains(&it.as_str()))
}

#[test]
fn dots_on_a_struct_with_no_fields_have_no_default_to_take() {
    // tests/ui/structs/default-field-values/empty-struct.rs
    let source = r#"#![feature(default_field_values)]

// If an API wants users to always use `..` even if they specify all the fields, they should use a
// sentinel field. As of now, that field can't be made private so it is only constructable with this
// syntax, but this might change in the future.

struct A {}
struct B();
struct C;
struct D {
    x: i32,
}
struct E(i32);

fn main() {
    let _ = A { .. }; //~ ERROR has no fields
    let _ = B { .. }; //~ ERROR has no fields
    let _ = C { .. }; //~ ERROR has no fields
    let _ = D { x: 0, .. };
    let _ = E { 0: 0, .. };
}
"#;
    let stderr = r#"error: `A` has no fields, `..` needs at least one default field in the struct definition
  --> $DIR/empty-struct.rs:16:17
   |
LL |     let _ = A { .. };
   |             -   ^^
   |             |
   |             this type has no fields

error: `B` has no fields, `..` needs at least one default field in the struct definition
  --> $DIR/empty-struct.rs:17:17
   |
LL |     let _ = B { .. };
   |             -   ^^
   |             |
   |             this type has no fields

error: `C` has no fields, `..` needs at least one default field in the struct definition
  --> $DIR/empty-struct.rs:18:17
   |
LL |     let _ = C { .. };
   |             -   ^^
   |             |
   |             this type has no fields

error: aborting due to 3 previous errors

"#;
    assert_eq!(ui("empty-struct.rs", "2015", source), stderr);
}

#[test]
fn dots_with_no_base_suggest_the_feature_when_every_field_left_has_a_default() {
    // tests/ui/structs/default-field-values/non-exhaustive-ctor.rs, revision `disabled`, its
    // E0797 and E0063 (the E0658 of each default value is the feature gate's)
    let source = r#"//@ revisions: enabled disabled
//@[enabled] run-rustfix
#![allow(private_interfaces, dead_code)]
#![cfg_attr(enabled, feature(default_field_values))]
use m::S;

mod m {
    pub struct S {
        pub field: () = (),
        //[disabled]~^ ERROR default values on fields are experimental
        pub field1: Priv = Priv,
        //[disabled]~^ ERROR default values on fields are experimental
        pub field2: Priv = Priv,
        //[disabled]~^ ERROR default values on fields are experimental
    }
    struct Priv;
}

fn main() {
    let _ = S { .. }; // ok
    //[disabled]~^ ERROR base expression required after `..`
    let _ = S { field: (), .. }; // ok
    //[disabled]~^ ERROR base expression required after `..`
    let _ = S { };
    //~^ ERROR missing fields `field`, `field1` and `field2`
    let _ = S { field: () };
    //~^ ERROR missing fields `field1` and `field2`
}
"#;
    let stderr = r#"error[E0797]: base expression required after `..`
  --> $DIR/non-exhaustive-ctor.rs:20:19
   |
LL |     let _ = S { .. }; // ok
   |                   ^
   |
help: add `#![feature(default_field_values)]` to the crate attributes to enable default values on `struct` fields
   |
LL + #![feature(default_field_values)]
   |
help: add a base expression here
   |
LL |     let _ = S { ../* expr */ }; // ok
   |                   ++++++++++

error[E0797]: base expression required after `..`
  --> $DIR/non-exhaustive-ctor.rs:22:30
   |
LL |     let _ = S { field: (), .. }; // ok
   |                              ^
   |
help: add `#![feature(default_field_values)]` to the crate attributes to enable default values on `struct` fields
   |
LL + #![feature(default_field_values)]
   |
help: add a base expression here
   |
LL |     let _ = S { field: (), ../* expr */ }; // ok
   |                              ++++++++++

error[E0063]: missing fields `field`, `field1` and `field2` in initializer of `S`
  --> $DIR/non-exhaustive-ctor.rs:24:13
   |
LL |     let _ = S { };
   |             ^ missing `field`, `field1` and `field2`
   |
help: all remaining fields have default values, if you added `#![feature(default_field_values)]` to your crate you could use those values with `..`
   |
LL |     let _ = S { .. };
   |                 ++

error[E0063]: missing fields `field1` and `field2` in initializer of `S`
  --> $DIR/non-exhaustive-ctor.rs:26:13
   |
LL |     let _ = S { field: () };
   |             ^ missing `field1` and `field2`
   |
help: all remaining fields have default values, if you added `#![feature(default_field_values)]` to your crate you could use those values with `..`
   |
LL |     let _ = S { field: (), .. };
   |                          ++++
"#;
    assert_eq!(ui_only("non-exhaustive-ctor.rs", source, code(&["E0797", "E0063"])), stderr);
}
