//! Braces around a value and around one import.

use crate::tests::ui;

#[test]
fn braces_around_an_argument_are_linted_and_a_borrowed_block_is_not() {
    let source = r#"//@ check-pass
//@ run-rustfix

#![warn(unused_braces)]

// changing `&{ expr }` to `&expr` changes the semantic of the program
// so we should not warn this case

#[repr(packed)]
pub struct A {
    pub a: u8,
    pub b: u32,
}

fn consume<T>(_: T) {}

fn main() {
    let a = A {
        a: 42,
        b: 1729,
    };

    consume(&{ a.b });
    consume({ a.b });
    //~^ WARN unnecessary braces
}
"#;
    let expected = r#"warning: unnecessary braces around function argument
  --> $DIR/unused_braces_borrow.rs:24:13
   |
LL |     consume({ a.b });
   |             ^^   ^^
   |
note: the lint level is defined here
  --> $DIR/unused_braces_borrow.rs:4:9
   |
LL | #![warn(unused_braces)]
   |         ^^^^^^^^^^^^^
help: remove these braces
   |
LL -     consume({ a.b });
LL +     consume(a.b);
   |

warning: 1 warning emitted

"#;
    assert_eq!(crate::tests::items::ui_core("unused_braces_borrow.rs", "2015", "copy", source), expected);
}

/// A const argument needs its braces unless it is a literal written where it is.
#[test]
fn braces_around_a_literal_const_argument_are_linted() {
    let source = r#"//@ check-pass
//@ run-rustfix
#![warn(unused_braces)]

macro_rules! make_1 {
    () => {
        1
    }
}

struct A<const N: usize>;

fn main() {
    let _: A<7>; // ok
    let _: A<{ 7 }>; //~ WARN unnecessary braces
    let _: A<{ 3 + 5 }>; // ok
    let _: A<{make_1!()}>; // ok
}
"#;
    let expected = r#"warning: unnecessary braces around const expression
  --> $DIR/unused_braces.rs:15:14
   |
LL |     let _: A<{ 7 }>;
   |              ^^ ^^
   |
note: the lint level is defined here
  --> $DIR/unused_braces.rs:3:9
   |
LL | #![warn(unused_braces)]
   |         ^^^^^^^^^^^^^
help: remove these braces
   |
LL -     let _: A<{ 7 }>;
LL +     let _: A<7>;
   |

warning: 1 warning emitted

"#;
    assert_eq!(ui("unused_braces.rs", "2015", source), expected);
}

#[test]
fn the_value_of_a_local_const_is_not_linted() {
    let source = r#"// regression test for https://github.com/rust-lang/rust/issues/69016
//@ check-pass

#![warn(unused)]
#![deny(warnings)]

fn _unused1(x: i32) -> i32 {
    const F: i32 = 2;
    let g = 1;
    x * F + g
}

pub struct Foo {}

impl Foo {
    fn _unused2(x: i32) -> i32 {
        const F: i32 = 2;
        let g = 1;
        x * F + g
    }
}

fn main() {}
"#;
    assert_eq!(ui("const-local-var.rs", "2015", source), "");
}

#[test]
fn braces_around_one_import_are_linted_and_around_self_are_not() {
    let source = r#"#![deny(unused_import_braces)]

use crate::test::{A}; //~ ERROR braces around A is unnecessary

mod test {
    use crate::test::{self}; // OK
    use crate::test::{self as rename}; // OK
    pub struct A;
}

fn main() {}
"#;
    let expected = r#"error: braces around A is unnecessary
  --> $DIR/lint-unnecessary-import-braces.rs:3:1
   |
LL | use crate::test::{A};
   | ^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/lint-unnecessary-import-braces.rs:1:9
   |
LL | #![deny(unused_import_braces)]
   |         ^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("lint-unnecessary-import-braces.rs", "2015", source), expected);
}
