//! rustc's unsafety checking: E0133 for each kind of unsafe operation, the note on the
//! `unsafe` block an item does not inherit, `unsafe_op_in_unsafe_fn` with its note on the
//! function once per function, and `deprecated_safe_2024`, from rustc's UI tests
//! (`tests/ui/unsafe/`, `tests/ui/static/`, `tests/ui/union/`, `tests/ui/error-codes/`,
//! `tests/ui/traits/`, `tests/ui/inline-const/`) with their `.stderr`.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

#[test]
fn a_call_of_an_unsafe_function_from_a_safe_one() {
    // tests/ui/unsafe/unsafe-fn-called-from-safe.rs
    let source = r##"unsafe fn f() {
    return;
}

fn main() {
    f();
    //~^ ERROR call to unsafe function `f` is unsafe
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `f` is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-called-from-safe.rs:6:5
   |
LL |     f();
   |     ^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("unsafe-fn-called-from-safe.rs", "2015", source), stderr);
}

#[test]
fn the_words_follow_the_level_of_unsafe_op_in_unsafe_fn_at_the_operation() {
    // rustc's `unsafe_op_in_unsafe_fn_allowed`: the level there, not the edition's default.
    let error = |needs: &str| {
        format!(
            r##"error[E0133]: call to unsafe function `f` is unsafe and requires unsafe {needs}
  --> $DIR/levels.rs:6:5
   |
LL |     f();
   |     ^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##
        )
    };
    let source = |attr: &str| {
        format!("#![{attr}(unsafe_op_in_unsafe_fn)]\n\nunsafe fn f() {{}}\n\nfn main() {{\n    f();\n    //~^ ERROR E0133\n}}\n")
    };
    assert_eq!(ui("levels.rs", "2021", &source("deny")), error("block"));
    assert_eq!(ui("levels.rs", "2024", &source("allow")), error("function or block"));
}

#[test]
fn the_error_code_e0133() {
    // tests/ui/error-codes/E0133.rs
    let source = r##"unsafe fn f() { return; }

fn main() {
    f();
    //~^ ERROR E0133
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `f` is unsafe and requires unsafe function or block
  --> $DIR/E0133.rs:4:5
   |
LL |     f();
   |     ^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("E0133.rs", "2015", source), stderr);
}

#[test]
fn a_function_of_an_extern_block_is_named_by_its_path() {
    // tests/ui/unsafe/foreign-unsafe-fn-called.rs
    let source = r##"mod test {
    extern "C" {
        pub fn free();
    }
}

fn main() {
    test::free();
    //~^ ERROR call to unsafe function `test::free` is unsafe
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `test::free` is unsafe and requires unsafe function or block
  --> $DIR/foreign-unsafe-fn-called.rs:8:5
   |
LL |     test::free();
   |     ^^^^^^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("foreign-unsafe-fn-called.rs", "2015", source), stderr);
}

#[test]
fn an_unsafe_method() {
    // tests/ui/unsafe/issue-3080.rs
    let source = r##"struct X(());
impl X {
    pub unsafe fn with(&self) { }
}

fn main() {
    X(()).with(); //~ ERROR requires unsafe function or block
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `X::with` is unsafe and requires unsafe function or block
  --> $DIR/issue-3080.rs:7:5
   |
LL |     X(()).with();
   |     ^^^^^^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("issue-3080.rs", "2015", source), stderr);
}

#[test]
fn a_function_held_in_a_local_is_named() {
    // tests/ui/unsafe/unsafe-fn-used-as-value.rs
    let source = r##"unsafe fn f() {
    return;
}

fn main() {
    let x = f;
    x();
    //~^ ERROR call to unsafe function `f` is unsafe
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `f` is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-used-as-value.rs:7:5
   |
LL |     x();
   |     ^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("unsafe-fn-used-as-value.rs", "2015", source), stderr);
}

#[test]
fn a_call_in_an_inline_const() {
    // tests/ui/inline-const/expr-unsafe-err.rs
    let source = r##"const unsafe fn require_unsafe() -> usize {
    1
}

fn main() {
    const {
        require_unsafe();
        //~^ ERROR [E0133]
    }
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `require_unsafe` is unsafe and requires unsafe function or block
  --> $DIR/expr-unsafe-err.rs:7:9
   |
LL |         require_unsafe();
   |         ^^^^^^^^^^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("expr-unsafe-err.rs", "2015", source), stderr);
}

#[test]
fn dereferences_of_raw_pointers() {
    // tests/ui/unsafe/unsafe-fn-deref-ptr.rs
    let source = r##"fn f(p: *const u8) -> u8 {
    let _ = *p; //~ ERROR dereference of raw pointer is unsafe
    let _: u8 = *p; //~ ERROR dereference of raw pointer is unsafe
    _ = *p; //~ ERROR dereference of raw pointer is unsafe
    return *p; //~ ERROR dereference of raw pointer is unsafe
}

fn main() {
}
"##;
    let stderr = r##"error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-deref-ptr.rs:2:13
   |
LL |     let _ = *p;
   |             ^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-deref-ptr.rs:3:17
   |
LL |     let _: u8 = *p;
   |                 ^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-deref-ptr.rs:4:9
   |
LL |     _ = *p;
   |         ^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-deref-ptr.rs:5:12
   |
LL |     return *p;
   |            ^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(crate::tests::items::ui_core("unsafe-fn-deref-ptr.rs", "2015", "copy", source), stderr);
}

#[test]
fn an_assignment_through_a_raw_pointer() {
    // tests/ui/unsafe/unsafe-fn-assign-deref-ptr.rs
    let source = r##"fn f(p: *mut u8) {
    *p = 0; //~ ERROR dereference of raw pointer is unsafe
    return;
}

fn main() {
}
"##;
    let stderr = r##"error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/unsafe-fn-assign-deref-ptr.rs:2:5
   |
LL |     *p = 0;
   |     ^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("unsafe-fn-assign-deref-ptr.rs", "2015", source), stderr);
}

#[test]
fn an_unsafe_impl_does_not_make_its_methods_unsafe() {
    // tests/ui/traits/safety-fn-body.rs
    let source = r##"// Check that an unsafe impl does not imply that unsafe actions are
// legal in the methods.

unsafe trait UnsafeTrait : Sized {
    fn foo(self) { }
}

unsafe impl UnsafeTrait for *mut isize {
    fn foo(self) {
        // Unsafe actions are not made legal by taking place in an unsafe trait:
        *self += 1;
        //~^ ERROR E0133
    }
}

fn main() { }
"##;
    let stderr = r##"error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/safety-fn-body.rs:11:9
   |
LL |         *self += 1;
   |         ^^^^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(crate::tests::items::ui_core("safety-fn-body.rs", "2015", "sized, copy", source), stderr);
}

#[test]
fn uses_of_a_mutable_static() {
    // tests/ui/static/static-mut-requires-unsafe.rs
    let source = r##"static mut a: isize = 3;

fn main() {
    a += 3;         //~ ERROR: requires unsafe
    a = 4;          //~ ERROR: requires unsafe
    let _b = a;     //~ ERROR: requires unsafe
}
"##;
    let stderr = r##"error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/static-mut-requires-unsafe.rs:4:5
   |
LL |     a += 3;
   |     ^ use of mutable static
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/static-mut-requires-unsafe.rs:5:5
   |
LL |     a = 4;
   |     ^ use of mutable static
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/static-mut-requires-unsafe.rs:6:14
   |
LL |     let _b = a;
   |              ^ use of mutable static
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(crate::tests::items::ui_core("static-mut-requires-unsafe.rs", "2015", "copy", source), stderr);
}

#[test]
fn uses_of_a_mutable_extern_static() {
    // tests/ui/static/static-mut-foreign-requires-unsafe.rs
    let source = r##"extern "C" {
    static mut a: i32;
}

fn main() {
    a += 3; //~ ERROR: requires unsafe
    a = 4; //~ ERROR: requires unsafe
    let _b = a; //~ ERROR: requires unsafe
}
"##;
    let stderr = r##"error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/static-mut-foreign-requires-unsafe.rs:6:5
   |
LL |     a += 3;
   |     ^ use of mutable static
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/static-mut-foreign-requires-unsafe.rs:7:5
   |
LL |     a = 4;
   |     ^ use of mutable static
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/static-mut-foreign-requires-unsafe.rs:8:14
   |
LL |     let _b = a;
   |              ^ use of mutable static
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(crate::tests::items::ui_core("static-mut-foreign-requires-unsafe.rs", "2015", "copy", source), stderr);
}

#[test]
fn reads_of_union_fields() {
    // tests/ui/union/access_union_field.rs
    let source = r##"#![allow(unused_variables)]

union Foo {
    bar: i8,
    baz: u8,
}

fn main() {
    let foo = Foo { bar: 5 };
    let a = foo.bar; //~ ERROR access to union field is unsafe and requires unsafe function or block
    let b = foo.baz; //~ ERROR access to union field is unsafe and requires unsafe function or block
}
"##;
    let stderr = r##"error[E0133]: access to union field is unsafe and requires unsafe function or block
  --> $DIR/access_union_field.rs:10:13
   |
LL |     let a = foo.bar;
   |             ^^^^^^^ access to union field
   |
   = note: the field may not be properly initialized: using uninitialized data will cause undefined behavior

error[E0133]: access to union field is unsafe and requires unsafe function or block
  --> $DIR/access_union_field.rs:11:13
   |
LL |     let b = foo.baz;
   |             ^^^^^^^ access to union field
   |
   = note: the field may not be properly initialized: using uninitialized data will cause undefined behavior

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(crate::tests::items::ui_core("access_union_field.rs", "2015", "copy", source), stderr);
}

#[test]
fn items_do_not_inherit_the_unsafe_block_they_are_in() {
    // tests/ui/unsafe/unsafe-not-inherited.rs
    let source = r##"#![allow(unused, dead_code)]

static mut FOO: u64 = 0;

fn static_mod() {
    unsafe {static BAR: u64 = FOO;}
    //~^ ERROR: use of mutable static is unsafe
    //~| NOTE: use of mutable static
    //~| NOTE: mutable statics can be mutated by multiple threads
    //~| NOTE: items do not inherit unsafety
}

unsafe fn unsafe_call() {}
fn foo() {
    unsafe {
    //~^ NOTE: items do not inherit unsafety
        fn bar() {
            unsafe_call();
            //~^ ERROR: call to unsafe function
            //~| NOTE: call to unsafe function
            //~| NOTE: consult the function's documentation
        }
    }
}

fn main() {}
"##;
    let stderr = r##"error[E0133]: use of mutable static is unsafe and requires unsafe function or block
  --> $DIR/unsafe-not-inherited.rs:6:31
   |
LL |     unsafe {static BAR: u64 = FOO;}
   |     ------                    ^^^ use of mutable static
   |     |
   |     items do not inherit unsafety from separate enclosing items
   |
   = note: mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior

error[E0133]: call to unsafe function `unsafe_call` is unsafe and requires unsafe function or block
  --> $DIR/unsafe-not-inherited.rs:18:13
   |
LL |     unsafe {
   |     ------ items do not inherit unsafety from separate enclosing items
...
LL |             unsafe_call();
   |             ^^^^^^^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(crate::tests::items::ui_core("unsafe-not-inherited.rs", "2015", "copy", source), stderr);
}

#[test]
fn unsafe_op_in_unsafe_fn_warns_in_2024() {
    // tests/ui/unsafe/edition-2024-unsafe_op_in_unsafe_fn.rs
    let source = r##"//@ edition: 2024
//@ check-pass
#![crate_type = "lib"]
#![deny(unused_unsafe)]

unsafe fn unsf() {}

unsafe fn foo() {
    unsf();
    //~^ WARN call to unsafe function `unsf` is unsafe and requires unsafe block

    // no unused_unsafe
    unsafe {
        unsf();
    }
}
"##;
    let stderr = r##"warning[E0133]: call to unsafe function `unsf` is unsafe and requires unsafe block
  --> $DIR/edition-2024-unsafe_op_in_unsafe_fn.rs:9:5
   |
LL |     unsf();
   |     ^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior
note: an unsafe function restricts its caller, but its body is safe by default
  --> $DIR/edition-2024-unsafe_op_in_unsafe_fn.rs:8:1
   |
LL | unsafe fn foo() {
   | ^^^^^^^^^^^^^^^
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/unsafe-op-in-unsafe-fn.html>
   = note: `#[warn(unsafe_op_in_unsafe_fn)]` (part of `#[warn(rust_2024_compatibility)]`) on by default

warning: 1 warning emitted

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("edition-2024-unsafe_op_in_unsafe_fn.rs", "2024", source), stderr);
}

#[test]
fn unsafe_op_in_unsafe_fn_by_the_edition_2024_group() {
    // tests/ui/unsafe/unsafe_op_in_unsafe_fn/in_2024_compatibility.rs
    let source = r##"#![deny(rust_2024_compatibility)]
#![crate_type = "lib"]

unsafe fn unsf() {}

unsafe fn foo() {
    unsf();
    //~^ ERROR call to unsafe function `unsf` is unsafe and requires unsafe block
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `unsf` is unsafe and requires unsafe block
  --> $DIR/in_2024_compatibility.rs:7:5
   |
LL |     unsf();
   |     ^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior
note: an unsafe function restricts its caller, but its body is safe by default
  --> $DIR/in_2024_compatibility.rs:6:1
   |
LL | unsafe fn foo() {
   | ^^^^^^^^^^^^^^^
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/unsafe-op-in-unsafe-fn.html>
note: the lint level is defined here
  --> $DIR/in_2024_compatibility.rs:1:9
   |
LL | #![deny(rust_2024_compatibility)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(unsafe_op_in_unsafe_fn)]` implied by `#[deny(rust_2024_compatibility)]`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("in_2024_compatibility.rs", "2015", source), stderr);
}

#[test]
fn unsafe_op_in_unsafe_fn_notes_the_function_once() {
    // tests/ui/unsafe/unsafe_op_in_unsafe_fn/wrapping-unsafe-block-sugg.rs, with its
    // auxiliary/external_unsafe_macro.rs
    let source = r##"//@ run-rustfix
//@ aux-build:external_unsafe_macro.rs

#![deny(unsafe_op_in_unsafe_fn)] //~ NOTE
#![crate_name = "wrapping_unsafe_block_sugg"]

extern crate external_unsafe_macro;

unsafe fn unsf() {}

pub unsafe fn foo() {
    //~^ NOTE an unsafe function restricts its caller, but its body is safe by default
    unsf(); //~ ERROR call to unsafe function `unsf` is unsafe
    //~^ NOTE call to unsafe function
    //~| NOTE for more information, see
    //~| NOTE consult the function's documentation
    unsf(); //~ ERROR call to unsafe function `unsf` is unsafe
    //~^ NOTE call to unsafe function
    //~| NOTE for more information, see
    //~| NOTE consult the function's documentation
}

pub unsafe fn bar(x: *const i32) -> i32 {
    //~^ NOTE an unsafe function restricts its caller, but its body is safe by default
    let y = *x; //~ ERROR dereference of raw pointer is unsafe and requires unsafe block
    //~^ NOTE dereference of raw pointer
    //~| NOTE for more information, see
    //~| NOTE raw pointers may be null
    y + *x //~ ERROR dereference of raw pointer is unsafe and requires unsafe block
    //~^ NOTE dereference of raw pointer
    //~| NOTE for more information, see
    //~| NOTE raw pointers may be null
}

static mut BAZ: i32 = 0;
pub unsafe fn baz() -> i32 {
    //~^ NOTE an unsafe function restricts its caller, but its body is safe by default
    let y = BAZ; //~ ERROR use of mutable static is unsafe and requires unsafe block
    //~^ NOTE use of mutable static
    //~| NOTE for more information, see
    //~| NOTE mutable statics can be mutated by multiple threads
    y + BAZ //~ ERROR use of mutable static is unsafe and requires unsafe block
    //~^ NOTE use of mutable static
    //~| NOTE for more information, see
    //~| NOTE mutable statics can be mutated by multiple threads
}

macro_rules! unsafe_macro { () => (unsf()) }
//~^ ERROR call to unsafe function `unsf` is unsafe
//~| NOTE call to unsafe function
//~| NOTE for more information, see
//~| NOTE consult the function's documentation
//~| ERROR call to unsafe function `unsf` is unsafe
//~| NOTE call to unsafe function
//~| NOTE for more information, see
//~| NOTE consult the function's documentation

pub unsafe fn unsafe_in_macro() {
    //~^ NOTE an unsafe function restricts its caller, but its body is safe by default
    unsafe_macro!();
    //~^ NOTE in this expansion
    //~| NOTE in this expansion
    //~| NOTE in this expansion
    unsafe_macro!();
    //~^ NOTE in this expansion
    //~| NOTE in this expansion
    //~| NOTE in this expansion
}

pub unsafe fn unsafe_in_external_macro() {
    // FIXME: https://github.com/rust-lang/rust/issues/112504
    // FIXME: ~^ NOTE an unsafe function restricts its caller, but its body is safe by default
    external_unsafe_macro::unsafe_macro!();
    external_unsafe_macro::unsafe_macro!();
}

fn main() {}
"##;
    let aux = r##"pub unsafe fn unsf() {}

#[macro_export]
macro_rules! unsafe_macro { () => ($crate::unsf()) }
"##;
    let fixture = format!(
        "//- minicore: copy\n//- /wrapping-unsafe-block-sugg.rs crate:other:main edition:2015 deps:external_unsafe_macro\n{source}\
         //- /auxiliary/external_unsafe_macro.rs crate:external_unsafe_macro edition:2015\n{aux}"
    );
    let style = Style { ui_testing: true, ..Style::default() };
    let options = compiletest(Mode::Other, &Options::default());
    let written = checked_with(&fixture, "$DIR/", &options).stderr_with(style);
    assert_reports(
        &normalized(&written),
        &[
            error(13, &["`unsf`"]),
            error(17, &["`unsf`"]),
            error(25, &[]),
            error(29, &[]),
            error(38, &[]),
            error(42, &[]),
            error(48, &["`unsf`"]),
            error(48, &["`unsf`"]),
        ],
    );
}

/// tests/ui/rust-2024/unsafe-env-suggestion.rs needs `std::env::set_var`: the same call of a
/// function of the crate's own with rustc's attribute, as rustc gives it.
#[test]
fn a_call_of_a_function_unsafe_only_from_2024() {
    let source = r##"#![feature(rustc_attrs)]
#![allow(internal_features)]
#![deny(deprecated_safe_2024)]

#[rustc_deprecated_safe_2024(audit_that = "the environment access only happens in single-threaded code")]
pub unsafe fn set_var(_key: &str, _value: &str) {}

fn main() {
    set_var("FOO", "BAR");
    //~^ ERROR call to deprecated safe function
    //~| WARN this is accepted in the current edition

    unsafe {
        set_var("FOO", "BAR");
    }
}
"##;
    let stderr = r##"error: call to deprecated safe function `set_var` is unsafe and requires unsafe block
  --> $DIR/deprecated-safe.rs:9:5
   |
LL |     set_var("FOO", "BAR");
   |     ^^^^^^^^^^^^^^^^^^^^^ call to unsafe function
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/newly-unsafe-functions.html>
note: the lint level is defined here
  --> $DIR/deprecated-safe.rs:3:9
   |
LL | #![deny(deprecated_safe_2024)]
   |         ^^^^^^^^^^^^^^^^^^^^
help: you can wrap the call in an `unsafe` block if you can guarantee that the environment access only happens in single-threaded code
   |
LL +     // FIXME: Audit that the environment access only happens in single-threaded code.
LL ~     unsafe { set_var("FOO", "BAR") };
   |

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("deprecated-safe.rs", "2015", source), stderr);
}

/// In edition 2024 the same function is unsafe to call: the error of any unsafe call.
#[test]
fn a_function_deprecated_safe_is_unsafe_in_2024() {
    let source = r##"#![feature(rustc_attrs)]
#![allow(internal_features)]

#[rustc_deprecated_safe_2024(audit_that = "the environment access only happens in single-threaded code")]
pub unsafe fn set_var(_key: &str, _value: &str) {}

fn main() {
    set_var("FOO", "BAR");
}
"##;
    let stderr = r##"error[E0133]: call to unsafe function `set_var` is unsafe and requires unsafe block
  --> $DIR/deprecated-safe-2024.rs:8:5
   |
LL |     set_var("FOO", "BAR");
   |     ^^^^^^^^^^^^^^^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0133`.
"##;
    assert_eq!(ui("deprecated-safe-2024.rs", "2024", source), stderr);
}
