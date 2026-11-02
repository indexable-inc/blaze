//! rustc's UI tests of the errors of the codegen attribute parsers (`repr`, `link`,
//! `export_name`, `ffi_const`).

use crate::tests::ui;

#[test]
fn repr_takes_no_literal() {
    // tests/ui/error-codes/E0565.rs
    let source = r#"// repr currently doesn't support literals
#[repr("C")] //~ ERROR E0565
struct A {}

fn main() {}
"#;
    let stderr = r#"error[E0565]: malformed `repr` attribute input
  --> $DIR/E0565.rs:2:3
   |
LL | #[repr("C")]
   |   ^^^^^---^
   |        |
   |        expected a valid identifier here
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/type-layout.html#representations>

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0565`.
"#;
    assert_eq!(ui("E0565.rs", "2015", source), stderr);
}

#[test]
fn an_alignment_is_a_power_of_two_up_to_2_29() {
    // tests/ui/repr/repr-align.rs
    let source = r#"#![allow(dead_code)]

#[repr(align(16.0))] //~ ERROR: not an unsuffixed integer
struct S0(i32);

#[repr(align(15))] //~ ERROR: not a power of two
struct S1(i32);

#[repr(align(4294967296))] //~ ERROR: larger than 2^29
struct S2(i32);

#[repr(align(536870912))] // ok: this is the largest accepted alignment
struct S3(i32);

#[repr(align(0))] //~ ERROR: not a power of two
struct S4(i32);

#[repr(align(16.0))] //~ ERROR: not an unsuffixed integer
enum E0 { A, B }

#[repr(align(15))] //~ ERROR: not a power of two
enum E1 { A, B }

#[repr(align(4294967296))] //~ ERROR: larger than 2^29
enum E2 { A, B }

#[repr(align(536870912))] // ok: this is the largest accepted alignment
enum E3 { A, B }

#[repr(align(0))] //~ ERROR: not a power of two
enum E4 { A, B }

fn main() {}
"#;
    let stderr = r#"error[E0589]: invalid alignment value: not an unsuffixed integer
  --> $DIR/repr-align.rs:3:14
   |
LL | #[repr(align(16.0))]
   |              ^^^^

error[E0589]: invalid alignment value: not a power of two
  --> $DIR/repr-align.rs:6:14
   |
LL | #[repr(align(15))]
   |              ^^

error[E0589]: invalid alignment value: larger than 2^29
  --> $DIR/repr-align.rs:9:14
   |
LL | #[repr(align(4294967296))]
   |              ^^^^^^^^^^

error[E0589]: invalid alignment value: not a power of two
  --> $DIR/repr-align.rs:15:14
   |
LL | #[repr(align(0))]
   |              ^

error[E0589]: invalid alignment value: not an unsuffixed integer
  --> $DIR/repr-align.rs:18:14
   |
LL | #[repr(align(16.0))]
   |              ^^^^

error[E0589]: invalid alignment value: not a power of two
  --> $DIR/repr-align.rs:21:14
   |
LL | #[repr(align(15))]
   |              ^^

error[E0589]: invalid alignment value: larger than 2^29
  --> $DIR/repr-align.rs:24:14
   |
LL | #[repr(align(4294967296))]
   |              ^^^^^^^^^^

error[E0589]: invalid alignment value: not a power of two
  --> $DIR/repr-align.rs:30:14
   |
LL | #[repr(align(0))]
   |              ^

error: aborting due to 8 previous errors

For more information about this error, try `rustc --explain E0589`.
"#;
    assert_eq!(ui("repr-align.rs", "2015", source), stderr);
}

#[test]
fn a_link_name_is_not_empty() {
    // tests/ui/error-codes/E0454.rs
    let source = r#"#[link(name = "")] extern "C" {}
//~^ ERROR E0454

fn main() {
}
"#;
    let stderr = r#"error[E0454]: link name must not be empty
  --> $DIR/E0454.rs:1:15
   |
LL | #[link(name = "")] extern "C" {}
   |               ^^ empty link name

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0454`.
"#;
    assert_eq!(ui("E0454.rs", "2015", source), stderr);
}

#[test]
fn link_needs_a_name() {
    // tests/ui/error-codes/E0459.rs
    let source = r#"#[link(kind = "dylib")] extern "C" {} //~ ERROR E0459

fn main() {
}
"#;
    let stderr = r#"error[E0459]: `#[link]` attribute requires a `name = "string"` argument
  --> $DIR/E0459.rs:1:1
   |
LL | #[link(kind = "dylib")] extern "C" {}
   | ^^^^^^^^^^^^^^^^^^^^^^^ missing `name` argument

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0459`.
"#;
    assert_eq!(ui("E0459.rs", "2015", source), stderr);
}

#[test]
fn link_takes_each_key_once() {
    // tests/ui/link-native-libs/modifiers-override-4.rs
    let source = r#"#[link(name = "foo")]
#[link(
//~^ ERROR malformed `link` attribute input
    name = "bar",
    kind = "static",
    modifiers = "+whole-archive,-whole-archive",
    //~^ ERROR multiple `whole-archive` modifiers in a single `modifiers` argument
    modifiers = "+bundle"
)]
extern "C" {}

fn main() {}
"#;
    let stderr = r#"error[E0538]: malformed `link` attribute input
  --> $DIR/modifiers-override-4.rs:2:3
   |
LL |   #[link(
   |  ___^
LL | |
LL | |     name = "bar",
LL | |     kind = "static",
...  |
LL | |     modifiers = "+bundle"
   | |     --------------------- found `modifiers` used as a key more than once
LL | | )]
   | |_^
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/items/external-blocks.html#the-link-attribute>

error: multiple `whole-archive` modifiers in a single `modifiers` argument
  --> $DIR/modifiers-override-4.rs:6:17
   |
LL |     modifiers = "+whole-archive,-whole-archive",
   |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0538`.
"#;
    assert_eq!(ui("modifiers-override-4.rs", "2015", source), stderr);
}

#[test]
fn an_export_name_has_no_null() {
    // tests/ui/error-codes/E0648.rs
    let source = r#"#[export_name="\0foo"] //~ ERROR E0648
pub fn bar() {}

fn main() {}
"#;
    let stderr = r#"error[E0648]: `export_name` may not contain null characters
  --> $DIR/E0648.rs:1:1
   |
LL | #[export_name="\0foo"]
   | ^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0648`.
"#;
    assert_eq!(ui("E0648.rs", "2015", source), stderr);
}

#[test]
fn ffi_const_is_not_ffi_pure() {
    // tests/ui/ffi-attrs/ffi_const2.rs
    let source = r#"#![feature(ffi_const, ffi_pure)]

extern "C" {
    #[unsafe(ffi_pure)] //~ ERROR `#[ffi_const]` function cannot be `#[ffi_pure]`
    #[unsafe(ffi_const)]
    pub fn baz();
}

fn main() {
    unsafe { baz() };
}
"#;
    let stderr = r#"error[E0757]: `#[ffi_const]` function cannot be `#[ffi_pure]`
  --> $DIR/ffi_const2.rs:4:5
   |
LL |     #[unsafe(ffi_pure)]
   |     ^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0757`.
"#;
    assert_eq!(ui("ffi_const2.rs", "2015", source), stderr);
}
