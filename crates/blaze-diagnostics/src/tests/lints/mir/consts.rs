//! rustc's const checking (`tests/ui/consts/`, `tests/ui/error-codes/E0015.rs`), and blaze's
//! own `blaze::const_impure`, given only in a crate that uses cached functions.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked, checked_with, compiletest, error, items::ui_core, normalized, ui},
};

#[test]
fn a_call_of_a_function_that_is_not_const_in_a_constant() {
    // tests/ui/error-codes/E0015.rs, with an `Option` of its own after `main` for core's:
    // the lines and columns are the test's.
    let source = r#"fn create_some() -> Option<u8> {
    Some(1)
}

const FOO: Option<u8> = create_some();
//~^ ERROR cannot call non-const function `create_some` in constants [E0015]

fn main() {}
enum Option<T> { Some(T), None }
use Option::*;
"#;
    let stderr = r#"error[E0015]: cannot call non-const function `create_some` in constants
  --> $DIR/E0015.rs:5:25
   |
LL | const FOO: Option<u8> = create_some();
   |                         ^^^^^^^^^^^^^
   |
note: function `create_some` is not const
  --> $DIR/E0015.rs:1:1
   |
LL | fn create_some() -> Option<u8> {
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: calls in constants are limited to constant functions, tuple structs and tuple variants

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui("E0015.rs", "2015", source), stderr);
}

#[test]
fn a_call_of_a_function_that_is_not_const_in_a_const_fn() {
    // tests/ui/consts/const-fn-not-safe-for-const.rs, its `use std::mem::transmute;` made a
    // comment and `sub`'s body one that needs no std: the lines of the error are the test's.
    let source = r#"// Test that we can't call random fns in a const fn or do other bad things.

// use std::mem::transmute;

fn random() -> u32 {
    0
}

const fn sub(x: &u32) -> usize {
    { let _ = x; 0 }
}

const fn sub1() -> u32 {
    random() //~ ERROR E0015
}

static Y: u32 = 0;

const fn get_Y() -> u32 {
    Y
}

const fn get_Y_addr() -> &'static u32 {
    &Y
}

const fn get() -> u32 {
    let x = 22;
    let y = 44;
    x + y
}

fn main() {}
"#;
    let stderr = r#"error[E0015]: cannot call non-const function `random` in constant functions
  --> $DIR/const-fn-not-safe-for-const.rs:14:5
   |
LL |     random()
   |     ^^^^^^^^
   |
note: function `random` is not const
  --> $DIR/const-fn-not-safe-for-const.rs:5:1
   |
LL | fn random() -> u32 {
   | ^^^^^^^^^^^^^^^^^^
   = note: calls in constant functions are limited to constant functions, tuple structs and tuple variants

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui_core("const-fn-not-safe-for-const.rs", "2015", "copy", source), stderr);
}

/// In a `static`, rustc's const checking runs as it evaluates the static, in its type checking
/// of the crate: before its unsafety checking, whose errors come after.
#[test]
fn a_call_in_a_static_comes_before_its_unsafety() {
    // tests/ui/consts/issue-16538.rs
    let source = r#"mod Y {
    pub type X = usize;
    extern "C" {
        pub static x: *const usize;
    }
    pub fn foo(value: *const X) -> *const X {
        value
    }
}

static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
//~^ ERROR dereference of raw pointer
//~| ERROR E0015
//~| ERROR use of extern static is unsafe and requires

fn main() {}
"#;
    let stderr = r#"error[E0015]: cannot call non-const function `Y::foo` in statics
  --> $DIR/issue-16538.rs:11:23
   |
LL | static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
   |                       ^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
note: function `foo` is not const
  --> $DIR/issue-16538.rs:6:5
   |
LL |     pub fn foo(value: *const X) -> *const X {
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: calls in statics are limited to constant functions, tuple structs and tuple variants
   = note: consider wrapping this expression in `std::sync::LazyLock::new(|| ...)`

error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/issue-16538.rs:11:22
   |
LL | static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
   |                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error[E0133]: use of extern static is unsafe and requires unsafe function or block
  --> $DIR/issue-16538.rs:11:30
   |
LL | static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
   |                              ^^^^ use of extern static
   |
   = note: extern statics are not controlled by the Rust type system: invalid data, aliasing violations or data races will cause undefined behavior

error: aborting due to 3 previous errors

Some errors have detailed explanations: E0015, E0133.
For more information about an error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui_core("issue-16538.rs", "2015", "copy", source), stderr);
}

#[test]
fn a_pointer_cast_to_an_integer_in_a_constant() {
    // tests/ui/consts/issue-18294.rs, its `println!("{}", Y);` written `let _ = Y;`, which
    // needs no std.
    let source = r#"fn main() {
    const X: u32 = 1;
    const Y: usize = unsafe { &X as *const u32 as usize }; //~ ERROR pointers cannot be cast to integers
    let _ = Y;
}
"#;
    let stderr = r#"error: pointers cannot be cast to integers during const eval
  --> $DIR/issue-18294.rs:3:31
   |
LL |     const Y: usize = unsafe { &X as *const u32 as usize };
   |                               ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: at compile-time, pointers do not have an integer value
   = note: avoiding this restriction via `transmute`, `union`, or raw pointers leads to compile-time undefined behavior

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-18294.rs", "2015", source), stderr);
}

#[test]
fn a_closure_call_and_a_function_pointer_call_in_a_const_fn() {
    // tests/ui/consts/issue-56164.rs
    let source = r#"const fn foo() { (||{})() }
//~^ ERROR cannot call non-const closure

const fn bad(input: fn()) {
    input()
    //~^ ERROR function pointer calls are not allowed
}

fn main() {
}
"#;
    assert_reports(
        &ui_core("issue-56164.rs", "2015", "fn", source),
        &[error(1, &["closure"]), error(5, &["function pointer"])],
    );
}

#[test]
fn a_method_of_a_trait_that_is_not_const_in_a_const_fn() {
    // tests/ui/traits/const-traits/issue-88155.rs, with minicore's `Sized` for `T`'s bound.
    let source = r#"#![feature(const_trait_impl)]

pub trait A {
    fn assoc() -> bool;
}

pub const fn foo<T: A>() -> bool {
    T::assoc()
    //~^ ERROR cannot call non-const associated function
}

fn main() {}
"#;
    assert_reports(&ui_core("issue-88155.rs", "2015", "sized", source), &[error(8, &["`<T as A>::assoc`"])]);
}

#[test]
#[ignore = "needs a core whose `PartialEq` is a `const trait` under `#[rustc_const_unstable(feature = \"const_cmp\")]` with `const` impls for references and primitives, as rustc's is; minicore's is neither, so the call is `cannot call non-const operator` (E0015) without the `const_cmp` error"]
fn a_comparison_of_references_in_a_const_fn() {
    // tests/ui/consts/issue-90870.rs
    let source = r#"// Regression test for issue #90870.

#![allow(dead_code)]

const fn f(a: &u8, b: &u8) -> bool {
    //~^ HELP: add `#![feature(const_cmp)]` to the crate attributes to enable
    //~| HELP: add `#![feature(const_cmp)]` to the crate attributes to enable
    //~| HELP: add `#![feature(const_cmp)]` to the crate attributes to enable
    a == b
    //~^ ERROR: cannot call conditionally-const operator in constant functions
    //~| ERROR: `PartialEq` is not yet stable as a const trait
    //~| HELP: consider dereferencing here
    //~| HELP: add `#![feature(const_trait_impl)]` to the crate attributes to enable
}

const fn g(a: &&&&i64, b: &&&&i64) -> bool {
    a == b
    //~^ ERROR: cannot call conditionally-const operator in constant functions
    //~| ERROR: `PartialEq` is not yet stable as a const trait
    //~| HELP: consider dereferencing here
    //~| HELP: add `#![feature(const_trait_impl)]` to the crate attributes to enable
}

const fn h(mut a: &[u8], mut b: &[u8]) -> bool {
    while let ([l, at @ ..], [r, bt @ ..]) = (a, b) {
        if l == r {
        //~^ ERROR: cannot call conditionally-const operator in constant functions
        //~| ERROR: `PartialEq` is not yet stable as a const trait
        //~| HELP: consider dereferencing here
        //~| HELP: add `#![feature(const_trait_impl)]` to the crate attributes to enable
            a = at;
            b = bt;
        } else {
            return false;
        }
    }

    a.is_empty() && b.is_empty()
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: cannot call conditionally-const operator in constant functions
  --> $DIR/issue-90870.rs:9:5
   |
LL |     a == b
   |     ^^^^^^
   |
   = note: calls in constant functions are limited to constant functions, tuple structs and tuple variants
   = note: see issue #143874 <https://github.com/rust-lang/rust/issues/143874> for more information
   = help: add `#![feature(const_trait_impl)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
help: consider dereferencing here
   |
LL |     *a == *b
   |     +     +

error: `PartialEq` is not yet stable as a const trait
  --> $DIR/issue-90870.rs:9:5
   |
LL |     a == b
   |     ^^^^^^
   |
help: add `#![feature(const_cmp)]` to the crate attributes to enable
   |
LL + #![feature(const_cmp)]
   |

error[E0658]: cannot call conditionally-const operator in constant functions
  --> $DIR/issue-90870.rs:17:5
   |
LL |     a == b
   |     ^^^^^^
   |
   = note: calls in constant functions are limited to constant functions, tuple structs and tuple variants
   = note: see issue #143874 <https://github.com/rust-lang/rust/issues/143874> for more information
   = help: add `#![feature(const_trait_impl)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
help: consider dereferencing here
   |
LL |     ****a == ****b
   |     ++++     ++++

error: `PartialEq` is not yet stable as a const trait
  --> $DIR/issue-90870.rs:17:5
   |
LL |     a == b
   |     ^^^^^^
   |
help: add `#![feature(const_cmp)]` to the crate attributes to enable
   |
LL + #![feature(const_cmp)]
   |

error[E0658]: cannot call conditionally-const operator in constant functions
  --> $DIR/issue-90870.rs:26:12
   |
LL |         if l == r {
   |            ^^^^^^
   |
   = note: calls in constant functions are limited to constant functions, tuple structs and tuple variants
   = note: see issue #143874 <https://github.com/rust-lang/rust/issues/143874> for more information
   = help: add `#![feature(const_trait_impl)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
help: consider dereferencing here
   |
LL |         if *l == *r {
   |            +     +

error: `PartialEq` is not yet stable as a const trait
  --> $DIR/issue-90870.rs:26:12
   |
LL |         if l == r {
   |            ^^^^^^
   |
help: add `#![feature(const_cmp)]` to the crate attributes to enable
   |
LL + #![feature(const_cmp)]
   |

error: aborting due to 6 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui_core("issue-90870.rs", "2015", "eq, slice", source), stderr);
}

#[test]
#[ignore = "needs the desugaring's type printed as rustc's diagnostics print it (`std::ops::Range<i32>`; the checker's message spells it with `type_name`, `core::ops::range::Range<i32>`: v2/requests/codegen-rq-mir-constcheck.md R2's printer, R8) and std's `Range`, which minicore's stands in for"]
fn a_for_loop_in_a_constant() {
    // tests/ui/consts/const-for-feature-gate.rs
    let source = r#"// gate-test-const_for

const _: () = {
    for _ in 0..5 {}
    //~^ ERROR cannot use `for`
    //~| ERROR cannot use `for`
};

fn main() {}
"#;
    let stderr = r#"error[E0015]: cannot use `for` loop on `std::ops::Range<i32>` in constants
  --> $DIR/const-for-feature-gate.rs:4:14
   |
LL |     for _ in 0..5 {}
   |              ^^^^
   |
   = note: calls in constants are limited to constant functions, tuple structs and tuple variants

error[E0015]: cannot use `for` loop on `std::ops::Range<i32>` in constants
  --> $DIR/const-for-feature-gate.rs:4:14
   |
LL |     for _ in 0..5 {}
   |              ^^^^
   |
   = note: calls in constants are limited to constant functions, tuple structs and tuple variants
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui_core("const-for-feature-gate.rs", "2015", "iterator, range", source), stderr);
}

#[test]
#[ignore = "needs rustc's `def_path_str` of an item nested in a function's body (`copy::copy`; the checker writes `copy`: v2/requests/codegen-rq-mir-constcheck.md R8)"]
fn unstable_intrinsics_in_a_const_fn_exposed_to_stable() {
    // tests/ui/consts/const-unstable-intrinsic.rs, with its auxiliary/unstable_intrinsic.rs
    let source = r#"//! Ensure that unstable intrinsics can actually not be called,
//! neither within a crate nor cross-crate.
//@ aux-build:unstable_intrinsic.rs
#![feature(staged_api, rustc_attrs, intrinsics)]
#![stable(since = "1.0.0", feature = "stable")]
#![feature(local)]

extern crate unstable_intrinsic;

fn main() {
    const_main();
}

const fn const_main() {
    let x = 42;
    unsafe {
        unstable_intrinsic::size_of_val(&x);
        //~^ERROR: unstable library feature `unstable`
        //~|ERROR: not yet stable as a const intrinsic
        unstable_intrinsic::align_of_val(&x);
        //~^ERROR: unstable library feature `unstable`
        //~|ERROR: not yet stable as a const intrinsic

        size_of_val(&x);
        //~^ERROR: cannot use `#[feature(local)]`
        align_of_val(&x);
        //~^ERROR: cannot use `#[feature(local)]`
    }
}

#[unstable(feature = "local", issue = "42")]
#[rustc_intrinsic]
pub const unsafe fn size_of_val<T>(x: *const T) -> usize;

#[unstable(feature = "local", issue = "42")]
#[rustc_const_unstable(feature = "local", issue = "42")]
#[rustc_intrinsic]
pub const unsafe fn align_of_val<T>(x: *const T) -> usize;

#[stable(feature = "rust1", since = "1.0.0")]
#[rustc_const_stable(feature = "const_intrinsic_copy", since = "1.63.0")]
#[inline]
pub const unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize) {
    // Const stability attributes are not inherited from parent items.
    #[rustc_intrinsic]
    const unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize);

    unsafe { copy(src, dst, count) }
    //~^ ERROR cannot be (indirectly) exposed to stable
}

// Ensure that a fallback body is recursively-const-checked.
mod fallback {
    #[rustc_intrinsic]
    const unsafe fn copy<T>(src: *const T, _dst: *mut T, _count: usize) {
        super::size_of_val(src);
        //~^ ERROR cannot use `#[feature(local)]`
    }
}
"#;
    let aux = r#"#![feature(staged_api, rustc_attrs, intrinsics)]
#![stable(since="1.0.0", feature = "stable")]

#[unstable(feature = "unstable", issue = "42")]
#[rustc_intrinsic]
pub const unsafe fn size_of_val<T>(x: *const T) -> usize;

#[unstable(feature = "unstable", issue = "42")]
#[rustc_const_unstable(feature = "unstable", issue = "42")]
#[rustc_intrinsic]
pub const unsafe fn align_of_val<T>(x: *const T) -> usize;
"#;
    let stderr = r#"error[E0658]: use of unstable library feature `unstable`
  --> $DIR/const-unstable-intrinsic.rs:17:9
   |
LL |         unstable_intrinsic::size_of_val(&x);
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #42 <https://github.com/rust-lang/rust/issues/42> for more information
   = help: add `#![feature(unstable)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: use of unstable library feature `unstable`
  --> $DIR/const-unstable-intrinsic.rs:20:9
   |
LL |         unstable_intrinsic::align_of_val(&x);
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #42 <https://github.com/rust-lang/rust/issues/42> for more information
   = help: add `#![feature(unstable)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: `size_of_val` is not yet stable as a const intrinsic
  --> $DIR/const-unstable-intrinsic.rs:17:9
   |
LL |         unstable_intrinsic::size_of_val(&x);
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: add `#![feature(unstable)]` to the crate attributes to enable
   |
LL + #![feature(unstable)]
   |

error: `align_of_val` is not yet stable as a const intrinsic
  --> $DIR/const-unstable-intrinsic.rs:20:9
   |
LL |         unstable_intrinsic::align_of_val(&x);
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: add `#![feature(unstable)]` to the crate attributes to enable
   |
LL + #![feature(unstable)]
   |

error: const function that might be (indirectly) exposed to stable cannot use `#[feature(local)]`
  --> $DIR/const-unstable-intrinsic.rs:24:9
   |
LL |         size_of_val(&x);
   |         ^^^^^^^^^^^^^^^
   |
help: if the function is not (yet) meant to be exposed to stable const contexts, add `#[rustc_const_unstable]`
   |
LL + #[rustc_const_unstable(feature = "...", issue = "...")]
LL | const fn const_main() {
   |

error: const function that might be (indirectly) exposed to stable cannot use `#[feature(local)]`
  --> $DIR/const-unstable-intrinsic.rs:26:9
   |
LL |         align_of_val(&x);
   |         ^^^^^^^^^^^^^^^^
   |
help: if the function is not (yet) meant to be exposed to stable const contexts, add `#[rustc_const_unstable]`
   |
LL + #[rustc_const_unstable(feature = "...", issue = "...")]
LL | const fn const_main() {
   |

error: intrinsic `copy::copy` cannot be (indirectly) exposed to stable
  --> $DIR/const-unstable-intrinsic.rs:48:14
   |
LL |     unsafe { copy(src, dst, count) }
   |              ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: mark the caller as `#[rustc_const_unstable]`, or mark the intrinsic `#[rustc_intrinsic_const_stable_indirect]` (but this requires team approval)

error: const function that might be (indirectly) exposed to stable cannot use `#[feature(local)]`
  --> $DIR/const-unstable-intrinsic.rs:56:9
   |
LL |         super::size_of_val(src);
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   |
help: if the function is not (yet) meant to be exposed to stable const contexts, add `#[rustc_const_unstable]`
   |
LL +     #[rustc_const_unstable(feature = "...", issue = "...")]
LL |     const unsafe fn copy<T>(src: *const T, _dst: *mut T, _count: usize) {
   |

error: aborting due to 8 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    let fixture = format!(
        "//- /const-unstable-intrinsic.rs crate:other:main deps:unstable_intrinsic edition:2015\n{source}\
         //- /auxiliary/unstable_intrinsic.rs crate:unstable_intrinsic library edition:2015\n{aux}"
    );
    assert_eq!(ui_fixture(&fixture), stderr);
}

/// The `.stderr` of a fixture whose workspace crate is the UI test and whose `library` crates
/// are its `aux-build`s, as [`ui`] writes it.
fn ui_fixture(fixture: &str) -> String {
    let options = compiletest(Mode::Other, &Options::default());
    let checked = checked_with(fixture, "$DIR/", &options);
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

const IMPURE: &str = r#"
static mut COUNT: u32 = 0;

pub const fn count() -> u32 {
    unsafe { COUNT }
}
"#;

/// The lints of a fixture's crates, as `name: message`.
fn lints(fixture: &str) -> Vec<String> {
    let checked = checked(fixture);
    checked
        .diags
        .iter()
        .filter_map(|it| Some(format!("{}: {}", it.lint_name()?, it.message)))
        .collect()
}

#[test]
fn an_impure_const_fn_is_told_where_cached_functions_are_used() {
    let fixture = format!("//- /main.rs crate:other:main deps:blaze\n{IMPURE}\n//- /blaze.rs crate:blaze\n");
    assert_eq!(lints(&fixture), [
        "blaze::const_impure: const fn reads or writes mutable state: `COUNT` is a `static mut`"
    ]);
}

#[test]
fn an_impure_const_fn_is_not_told_where_rustc_would_not() {
    let fixture = format!("//- /main.rs crate:other:main\n{IMPURE}");
    assert_eq!(lints(&fixture), Vec::<String>::new());
}
