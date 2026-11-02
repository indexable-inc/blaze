//! `function_casts_as_integer`: rustc's `tests/ui/lint/function_casts_as_integer.rs`.

use super::{Ui, ui};

/// `String` with `String::len`, which `function_casts_as_integer.rs` names unqualified, from
/// std's prelude.
const STRING_STD: &str = r##"
pub mod string {
    #[rustc_diagnostic_item = "String"]
    pub struct String {
        vec: (),
    }

    impl String {
        #[inline]
        pub fn len(&self) -> usize {
            loop {}
        }
    }
}
"##;

#[test]
fn function_casts_as_integer() {
    let source = r##"//@ run-rustfix

#![deny(function_casts_as_integer)]
#![allow(unused_variables, dead_code)] // For the rustfix-ed code.

fn foo() {}

enum MyEnum {
    Variant(u32),
}

struct MyStruct(u32);

fn main() {
    let x = foo as usize; //~ ERROR: function_casts_as_integer
    let x = String::len as usize; //~ ERROR: function_casts_as_integer
    let x = MyEnum::Variant as usize; //~ ERROR: function_casts_as_integer
    let x = MyStruct as usize; //~ ERROR: function_casts_as_integer
    // Ok.
    let x = foo as fn() as usize;
    let x = foo as *const () as usize;
}
"##;
    let expected = r##"error: direct cast of function item into an integer
  --> $DIR/function_casts_as_integer.rs:15:17
   |
LL |     let x = foo as usize;
   |                 ^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/function_casts_as_integer.rs:3:9
   |
LL | #![deny(function_casts_as_integer)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^
help: first cast to a pointer `as *const ()`
   |
LL |     let x = foo as *const () as usize;
   |                 ++++++++++++

error: direct cast of function item into an integer
  --> $DIR/function_casts_as_integer.rs:16:25
   |
LL |     let x = String::len as usize;
   |                         ^^^^^^^^
   |
help: first cast to a pointer `as *const ()`
   |
LL |     let x = String::len as *const () as usize;
   |                         ++++++++++++

error: direct cast of function item into an integer
  --> $DIR/function_casts_as_integer.rs:17:29
   |
LL |     let x = MyEnum::Variant as usize;
   |                             ^^^^^^^^
   |
help: first cast to a pointer `as *const ()`
   |
LL |     let x = MyEnum::Variant as *const () as usize;
   |                             ++++++++++++

error: direct cast of function item into an integer
  --> $DIR/function_casts_as_integer.rs:18:22
   |
LL |     let x = MyStruct as usize;
   |                      ^^^^^^^^
   |
help: first cast to a pointer `as *const ()`
   |
LL |     let x = MyStruct as *const () as usize;
   |                      ++++++++++++

error: aborting due to 4 previous errors

"##;
    let stderr = ui(Ui {
        name: "function_casts_as_integer.rs",
        core: "sized",
        std: Some(("pub use crate::string::String;", STRING_STD)),
        source,
        ..Ui::default()
    });
    assert_eq!(stderr, expected);
}
