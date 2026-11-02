//! rustc's UI tests of the errors type checking gives of calls, operators, casts and
//! assignments in bodies: E0061, E0057 and E0308's "arguments to this function are incorrect"
//! (a call with the wrong arguments), E0618 (a call of what is not a function), E0606, E0604,
//! E0054 and E0620 (casts), E0369 and E0600 (operators), E0614 (a dereference), E0608 (an
//! index), E0070 and E0067 (an assignment), E0040 (`Drop::drop`) and a method with a `Sized`
//! bound called on a trait object. Each test's file is the UI test's source byte for byte.

mod args;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

/// [`ui`] of a file with minicore's items for `flags` (`crates/test-utils/src/minicore.rs`),
/// under compiletest's flags.
fn ui_core(name: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:2015\n{source}");
    ui_fixture(&fixture)
}

/// [`ui_core`] of a file that uses std: a crate `std` of `std_items` over minicore, with its
/// prelude of core's and `prelude`, what std's adds that the test uses.
fn ui_std(name: &str, flags: &str, (prelude, std_items): (&str, &str), source: &str) -> String {
    let fixture = format!(
        "//- minicore: {flags}\n//- /std.rs crate:std\n\
         pub mod prelude {{ pub mod rust_2015 {{ pub use core::prelude::rust_2015::*; {prelude} }} }}\n\
         {std_items}\n//- /{name} crate:other:main edition:2015 deps:std\n{source}"
    );
    ui_fixture(&fixture)
}

fn ui_fixture(fixture: &str) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

#[test]
fn a_call_of_a_variant_or_a_local_that_is_not_a_function() {
    // tests/ui/error-codes/E0618.rs
    let source = "enum X {
    Entry,
}

fn main() {
    X::Entry();
    //~^ ERROR expected function, found enum variant `X::Entry` [E0618]
    let x = 0i32;
    x();
    //~^ ERROR expected function, found `i32` [E0618]
}
";
    let stderr = "error[E0618]: expected function, found enum variant `X::Entry`
  --> $DIR/E0618.rs:6:5
   |
LL |     Entry,
   |     ----- enum variant `X::Entry` defined here
...
LL |     X::Entry();
   |     ^^^^^^^^--
   |     |
   |     call expression requires function
   |
help: `X::Entry` is a unit enum variant, and does not take parentheses to be constructed
   |
LL -     X::Entry();
LL +     X::Entry;
   |

error[E0618]: expected function, found `i32`
  --> $DIR/E0618.rs:9:5
   |
LL |     let x = 0i32;
   |         - `x` has type `i32`
LL |     x();
   |     ^--
   |     |
   |     call expression requires function

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0618`.
";
    assert_eq!(ui_core("E0618.rs", "fn", source), stderr);
}

#[test]
fn a_cast_of_a_reference_borrows_too_much_or_is_not_dereferenced() {
    // tests/ui/error-codes/E0606.rs
    let source = "fn main() {
    let x = &(&0u8 as u8); //~ ERROR E0606
    x as u8; //~ ERROR casting `&u8` as `u8` is invalid [E0606]
}
";
    assert_reports(
        &ui_core("E0606.rs", "sized", source),
        &[
            error(2, &["`&u8`", "`u8`"]),
            error(3, &["`&u8`", "`u8`"]),
        ],
    );
}

#[test]
fn only_u8_is_cast_as_char() {
    // tests/ui/error-codes/E0604.rs
    let source = "fn main() {
    1u32 as char; //~ ERROR E0604
}
";
    let stderr = "error[E0604]: only `u8` can be cast as `char`, not `u32`
  --> $DIR/E0604.rs:2:5
   |
LL |     1u32 as char;
   |     ^^^^^^^^^^^^ invalid cast
   |
help: consider using `char::from_u32` instead
   |
LL -     1u32 as char;
LL +     char::from_u32(1u32);
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0604`.
";
    assert_eq!(ui_core("E0604.rs", "sized", source), stderr);
}

#[test]
fn an_integer_is_compared_with_zero_not_cast_as_bool() {
    // tests/ui/error-codes/E0054.rs
    let source = "fn main() {
    let x = 5;
    let x_is_nonzero = x as bool; //~ ERROR E0054
}
";
    let stderr = "error[E0054]: cannot cast `i32` as `bool`
  --> $DIR/E0054.rs:3:24
   |
LL |     let x_is_nonzero = x as bool;
   |                        ^^^^^^^^^
   |
help: compare with zero instead
   |
LL -     let x_is_nonzero = x as bool;
LL +     let x_is_nonzero = x != 0;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0054`.
";
    assert_eq!(ui_core("E0054.rs", "sized", source), stderr);
}

#[test]
fn a_cast_to_an_unsized_type_is_to_a_reference() {
    // tests/ui/error-codes/E0620.rs
    let source = "fn main() {
    let _foo = &[1_usize, 2] as [usize]; //~ ERROR E0620
}
";
    assert_reports(&ui_core("E0620.rs", "sized, slice", source), &[error(2, &["`&[usize; 2]`", "`[usize]`"])]);
}

#[test]
fn a_cast_to_a_pointer_of_an_unknown_kind() {
    // tests/ui/inference/order-dependent-cast-inference.rs
    let source = r#"fn main() {
    // Tests case where inference fails due to the order in which casts are checked.
    // Ideally this would compile, see #48270.
    let x = &"hello";
    let mut y = 0 as *const _;
    //~^ ERROR cannot cast to a pointer of an unknown kind
    y = x as *const _;
}
"#;
    let stderr = "error[E0641]: cannot cast to a pointer of an unknown kind
  --> $DIR/order-dependent-cast-inference.rs:5:22
   |
LL |     let mut y = 0 as *const _;
   |                      ^^^^^^^^ needs more type information
   |
   = note: the type information given here is insufficient to check whether the pointer cast is valid

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0641`.
";
    assert_eq!(ui_core("order-dependent-cast-inference.rs", "sized", source), stderr);
}

#[test]
fn an_enum_with_drop_is_not_cast_to_an_integer() {
    // tests/ui/enum/enum-drop-cast-error.rs
    let source = r#"//! Check that trying to cast an enum with a Drop impl to an integer is rejected.
//!
//! Issue: <https://github.com/rust-lang/rust/issues/35941>

enum E {
    A = 0,
}

impl Drop for E {
    fn drop(&mut self) {
        println!("Drop");
    }
}

fn main() {
    let e = E::A;
    let i = e as u32;
    //~^ ERROR cannot cast enum `E` into integer `u32` because it implements `Drop`
}
"#;
    let stderr = "error: cannot cast enum `E` into integer `u32` because it implements `Drop`
  --> $DIR/enum-drop-cast-error.rs:17:13
   |
LL |     let i = e as u32;
   |             ^^^^^^^^

error: aborting due to 1 previous error

";
    let println = r#"pub mod io {
    pub fn _print(_args: core::fmt::Arguments<'_>) {}
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print(core::format_args_nl!($($arg)*));
    }};
}
"#;
    assert_eq!(ui_std("enum-drop-cast-error.rs", "sized, drop, fmt", ("pub use crate::println;", println), source), stderr);
}

#[test]
fn a_foreign_enum_with_a_non_exhaustive_variant_is_not_cast() {
    // tests/ui/rfcs/rfc-2008-non-exhaustive/enum-as-cast.rs, with its auxiliary `enums.rs`'s
    // enum, and `assert_eq!` as std's.
    let fixture = r#"//- minicore: sized, default, eq, panic, fmt
//- /enums.rs crate:enums
#[repr(u8)]
pub enum FieldLessWithNonExhaustiveVariant {
    A,
    B,
    #[non_exhaustive]
    C,
}

impl Default for FieldLessWithNonExhaustiveVariant {
    fn default() -> Self { Self::A }
}
//- /std.rs crate:std
pub mod prelude { pub mod rust_2015 { pub use core::prelude::rust_2015::*; pub use crate::assert_eq; } }
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    core::panic!()
                }
            }
        }
    };
}
//- /enum-as-cast.rs crate:other:main edition:2015 deps:std,enums
//@ aux-build:enums.rs

extern crate enums;

use enums::FieldLessWithNonExhaustiveVariant;

fn main() {
    let e = FieldLessWithNonExhaustiveVariant::default();
    let d = e as u8; //~ ERROR casting `FieldLessWithNonExhaustiveVariant` as `u8` is invalid [E0606]
    assert_eq!(d, 0);
}
"#;
    assert_reports(&ui_fixture(fixture), &[error(9, &["`FieldLessWithNonExhaustiveVariant`", "`u8`"])]);
}

#[test]
fn a_binary_operator_of_types_without_its_impl() {
    // tests/ui/binop/binop-typeck.rs
    let source = "// issue #500

fn main() {
    let x = true;
    let y = 1;
    let z = x + y;
    //~^ ERROR cannot add `{integer}` to `bool`
}
";
    let stderr = "error[E0369]: cannot add `{integer}` to `bool`
  --> $DIR/binop-typeck.rs:6:15
   |
LL |     let z = x + y;
   |             - ^ - {integer}
   |             |
   |             bool

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0369`.
";
    assert_eq!(ui_core("binop-typeck.rs", "add", source), stderr);
}

#[test]
fn a_unary_operator_of_a_type_without_its_impl() {
    // tests/ui/error-codes/E0600.rs
    let source = "fn main() {
    !\"a\"; //~ ERROR E0600
}
";
    let stderr = "error[E0600]: cannot apply unary operator `!` to type `&'static str`
  --> $DIR/E0600.rs:2:5
   |
LL |     !\"a\";
   |     ^^^^ cannot apply unary operator `!`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0600`.
";
    assert_eq!(ui_core("E0600.rs", "unary_ops, str", source), stderr);
}

#[test]
fn a_dereference_of_an_integer() {
    // tests/ui/error-codes/E0614.rs
    let source = "fn main() {
    let y = 0u32;
    *y; //~ ERROR E0614
}
";
    let stderr = "error[E0614]: type `u32` cannot be dereferenced
  --> $DIR/E0614.rs:3:5
   |
LL |     *y;
   |     ^^ can't be dereferenced

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0614`.
";
    assert_eq!(ui_core("E0614.rs", "deref", source), stderr);
}

#[test]
fn an_index_into_an_integer() {
    // tests/ui/error-codes/E0608.rs
    let source = "fn main() {
    0u8[2]; //~ ERROR E0608
}
";
    let stderr = "error[E0608]: cannot index into a value of type `u8`
  --> $DIR/E0608.rs:2:8
   |
LL |     0u8[2];
   |        ^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0608`.
";
    assert_eq!(ui_core("E0608.rs", "index", source), stderr);
}

#[test]
fn an_assignment_to_a_constant_a_literal_and_a_call() {
    // tests/ui/error-codes/E0070.rs
    let source = "const SOME_CONST : i32 = 12;

fn some_other_func() {}

fn some_function() {
    SOME_CONST = 14; //~ ERROR E0070
    1 = 3; //~ ERROR E0070
    some_other_func() = 4; //~ ERROR E0070
}

fn main() {
}
";
    assert_reports(
        &ui("E0070.rs", "2015", source),
        &[error(6, &["assign"]), error(7, &["assign"]), error(8, &["assign"])],
    );
}

#[test]
fn a_compound_assignment_to_a_borrow_is_dereferenced() {
    // tests/ui/typeck/assign-non-lval-needs-deref.rs
    let source = "// issue #101376

use std::ops::AddAssign;
struct Foo;

impl AddAssign<()> for Foo {
    fn add_assign(&mut self, _: ()) {}
}

impl AddAssign<()> for &mut Foo {
    fn add_assign(&mut self, _: ()) {}
}

fn main() {
    (&mut Foo) += ();
    //~^ ERROR invalid left-hand side of assignment
    //~| NOTE cannot assign to this expression
    //~| HELP consider dereferencing the left-hand side of this operation
}
";
    let std_items = "pub use core::ops;";
    assert_reports(&ui_std("assign-non-lval-needs-deref.rs", "add", ("", std_items), source), &[error(15, &[])]);
}

#[test]
fn drop_called_as_a_path_is_the_drop_function() {
    // tests/ui/drop/explicit-drop-call-error.rs
    let source = "//! Test error for explicit destructor method calls via UFCS

//@ run-rustfix

#![allow(dropping_references)]

struct Foo;

impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    Drop::drop(&mut Foo) //~ ERROR explicit use of destructor method
}
";
    let stderr = "error[E0040]: explicit use of destructor method
  --> $DIR/explicit-drop-call-error.rs:14:5
   |
LL |     Drop::drop(&mut Foo)
   |     ^^^^^^^^^^ explicit destructor calls not allowed
   |
help: consider using `drop` function
   |
LL -     Drop::drop(&mut Foo)
LL +     drop(Foo)
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0040`.
";
    assert_eq!(ui_core("explicit-drop-call-error.rs", "drop", source), stderr);
}

#[test]
fn a_method_with_a_sized_bound_on_a_trait_object() {
    // tests/ui/illegal-sized-bound/regular.rs
    let source = "struct MutType;

pub trait MutTrait {
    fn function(&mut self)
    where
        Self: Sized;
    //~^ NOTE this has a `Sized` requirement
}

impl MutTrait for MutType {
    fn function(&mut self) {}
}

struct Type;

pub trait Trait {
    fn function(&self)
    where
        Self: Sized;
    //~^ NOTE this has a `Sized` requirement
}

impl Trait for Type {
    fn function(&self) {}
}

fn main() {
    (&mut MutType as &mut dyn MutTrait).function();
    //~^ ERROR the `function` method cannot be invoked on a trait object
    (&Type as &dyn Trait).function();
    //~^ ERROR the `function` method cannot be invoked on a trait object
}
";
    let stderr = "error: the `function` method cannot be invoked on a trait object
  --> $DIR/regular.rs:28:41
   |
LL |         Self: Sized;
   |               ----- this has a `Sized` requirement
...
LL |     (&mut MutType as &mut dyn MutTrait).function();
   |                                         ^^^^^^^^

error: the `function` method cannot be invoked on a trait object
  --> $DIR/regular.rs:30:27
   |
LL |         Self: Sized;
   |               ----- this has a `Sized` requirement
...
LL |     (&Type as &dyn Trait).function();
   |                           ^^^^^^^^

error: aborting due to 2 previous errors

";
    assert_eq!(ui_core("regular.rs", "sized, unsize, coerce_unsized", source), stderr);
}

#[test]
fn a_method_with_a_sized_bound_on_a_reference_of_the_other_mutability() {
    // tests/ui/illegal-sized-bound/mutability-mismatch.rs
    let source = r##"struct MutType;

pub trait MutTrait {
    fn function(&mut self)
    where
        Self: Sized;
}

impl MutTrait for MutType {
    fn function(&mut self) {}
}

struct Type;

pub trait Trait {
    fn function(&self)
    where
        Self: Sized;
}

impl Trait for Type {
    fn function(&self) {}
}

fn main() {
    (&MutType as &dyn MutTrait).function();
    //~^ ERROR the `function` method cannot be invoked on `&dyn MutTrait`
    //~| HELP you need `&mut dyn MutTrait` instead of `&dyn MutTrait`
    (&mut Type as &mut dyn Trait).function();
    //~^ ERROR the `function` method cannot be invoked on `&mut dyn Trait`
    //~| HELP you need `&dyn Trait` instead of `&mut dyn Trait`
}
"##;
    assert_reports(
        &ui_core("mutability-mismatch.rs", "sized, unsize, coerce_unsized", source),
        &[
            error(26, &["`&dyn MutTrait`", "`function`"]),
            error(29, &["`&mut dyn Trait`", "`function`"]),
        ],
    );
}

/// minicore's flags for a `&dyn Iterator`'s `min`: `Iterator::min` and `Ord` (`ord`), `[T]::iter`
/// (`slice`, `iterator`) and `Option::unwrap` (`fmt`).
const MIN_OF_A_DYN_ITERATOR: &str = "sized, unsize, coerce_unsized, iterator, ord, slice, fmt";

#[test]
fn a_method_with_a_sized_bound_on_a_shared_reference_to_a_dyn_iterator() {
    // tests/ui/suggestions/imm-ref-trait-object.rs
    let source = r##"fn test(t: &dyn Iterator<Item=&u64>) -> u64 {
     t.min().unwrap() //~ ERROR the `min` method cannot be invoked on `&dyn Iterator<Item = &u64>`
}

fn main() {
     let array = [0u64];
     test(&mut array.iter());
}
"##;
    assert_reports(
        &ui_core("imm-ref-trait-object.rs", MIN_OF_A_DYN_ITERATOR, source),
        &[
            error(2, &["`&dyn Iterator<Item = &u64>`", "`min`"]),
        ],
    );
}

#[test]
fn a_method_with_a_sized_bound_on_a_shared_reference_argument_dereferenced() {
    // tests/ui/illegal-sized-bound/mutability-mismatch-arg.rs
    let source = r##"//@ run-rustfix
fn test(t: &dyn Iterator<Item=&u64>) -> u64 {
     *t.min().unwrap() //~ ERROR the `min` method cannot be invoked on
}

fn main() {
     let array = [0u64];
     test(&mut array.iter());
}
"##;
    assert_reports(
        &ui_core("mutability-mismatch-arg.rs", MIN_OF_A_DYN_ITERATOR, source),
        &[
            error(3, &["`&dyn Iterator<Item = &u64>`", "`min`"]),
        ],
    );
}

#[test]
fn a_method_with_a_sized_bound_of_a_trait_not_in_scope() {
    // tests/ui/suggestions/trait-object-import-suggestion-on-method.rs, revision `unimported`
    let source = r##"//! Regression test for <https://github.com/rust-lang/rust/issues/35976>.
//! This used to emit spurious `Sized` bound unsatisfied error when trait
//! was not imported, instead of suggestion to import it.
//@ edition:2015
//@ revisions: imported unimported
//@[imported] check-pass

mod private {
    pub trait Future {
        //[unimported]~^^ HELP perhaps add a `use` for it
        fn wait(&self) where Self: Sized;
    }

    impl Future for Box<dyn Future> {
        fn wait(&self) { }
    }
}

#[cfg(imported)]
use private::Future;

fn bar(arg: Box<dyn private::Future>) {
    // Importing the trait means that we don't autoderef `Box<dyn Future>`
    arg.wait();
    //[unimported]~^ ERROR the `wait` method cannot be invoked on a trait object
}

fn main() {}
"##;
    let fixture = format!(
        "//- minicore: sized, unsize, coerce_unsized, deref\n//- /std.rs crate:std\n\
         pub mod prelude {{ pub mod rust_2015 {{ pub use core::prelude::rust_2015::*; pub use crate::boxed::Box; }} }}\n\
         pub mod boxed {{\n\
         #[lang = \"owned_box\"]\n\
         pub struct Box<T: ?Sized>(*mut T);\n\
         impl<T: ?Sized> core::ops::Deref for Box<T> {{ type Target = T; fn deref(&self) -> &T {{ loop {{}} }} }}\n\
         }}\n\
         //- /trait-object-import-suggestion-on-method.rs crate:other:main edition:2015 deps:std\n{source}"
    );
    assert_reports(&ui_fixture(&fixture), &[error(24, &["`wait`"])]);
}
