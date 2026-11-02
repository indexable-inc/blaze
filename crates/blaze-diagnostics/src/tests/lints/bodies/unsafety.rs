//! `unused_unsafe`, from rustc's UI tests (`tests/ui/inline-const/`, `tests/ui/unsafe/`) with
//! their `.stderr`.

use super::types::ui_std;
use crate::tests::{assert_reports, error, ui};

#[test]
fn an_unsafe_block_in_an_inline_const_in_an_unsafe_block() {
    // tests/ui/inline-const/expr-unsafe.rs
    let source = r##"//@ check-pass

#![warn(unused_unsafe)]

const unsafe fn require_unsafe() -> usize { 1 }

fn main() {
    unsafe {
        const {
            require_unsafe();
            unsafe {}
            //~^ WARNING unnecessary `unsafe` block
        }
    }
}
"##;
    let stderr = r##"warning: unnecessary `unsafe` block
  --> $DIR/expr-unsafe.rs:11:13
   |
LL |             unsafe {}
   |             ^^^^^^ unnecessary `unsafe` block
   |
note: the lint level is defined here
  --> $DIR/expr-unsafe.rs:3:9
   |
LL | #![warn(unused_unsafe)]
   |         ^^^^^^^^^^^^^

warning: 1 warning emitted

"##;
    assert_eq!(ui("expr-unsafe.rs", "2015", source), stderr);
}

#[test]
fn an_unsafe_block_in_a_fn_nested_in_an_unsafe_fn_or_block() {
    // tests/ui/unsafe/unused-unsafe-in-nested-fn-lint.rs
    let std_items = "pub mod mem {
    pub const unsafe fn zeroed<T>() -> T {
        loop {}
    }
}
";
    let source = r##"//! Regression test for <https://github.com/rust-lang/rust/issues/48131>.

// This note is annotated because the purpose of the test
// is to ensure that certain other notes are not generated.
#![deny(unused_unsafe)]


// (test that no note is generated on this unsafe fn)
pub unsafe fn a() {
    fn inner() {
        unsafe { /* unnecessary */ } //~ ERROR unnecessary `unsafe`
    }

    inner()
}

pub fn b() {
    // (test that no note is generated on this unsafe block)
    unsafe {
        fn inner() {
            unsafe { /* unnecessary */ } //~ ERROR unnecessary `unsafe`
        }
        // `()` is fine to zero-initialize as it is zero sized and inhabited.
        let () = ::std::mem::zeroed();

        inner()
    }
}

fn main() {}
"##;
    let stderr = r##"error: unnecessary `unsafe` block
  --> $DIR/unused-unsafe-in-nested-fn-lint.rs:11:9
   |
LL |         unsafe { /* unnecessary */ }
   |         ^^^^^^ unnecessary `unsafe` block
   |
note: the lint level is defined here
  --> $DIR/unused-unsafe-in-nested-fn-lint.rs:5:9
   |
LL | #![deny(unused_unsafe)]
   |         ^^^^^^^^^^^^^

error: unnecessary `unsafe` block
  --> $DIR/unused-unsafe-in-nested-fn-lint.rs:21:13
   |
LL |             unsafe { /* unnecessary */ }
   |             ^^^^^^ unnecessary `unsafe` block

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui_std("unused-unsafe-in-nested-fn-lint.rs", "2015", "sized", ("", std_items), source), stderr);
}

#[test]
fn unsafe_blocks_in_unsafe_fns_under_unsafe_op_in_unsafe_fn() {
    // tests/ui/unsafe/unsafe_op_in_unsafe_fn/rfc-2585-unsafe_op_in_unsafe_fn.rs: each body's
    // E0133 as its operations are checked, then its `unused_unsafe`.
    let std_items = "pub mod ptr {
    pub const fn null<T>() -> *const T {
        0 as *const T
    }
}
";
    let source = r##"#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_unsafe)]

unsafe fn unsf() {}
const PTR: *const () = std::ptr::null();
static mut VOID: () = ();

unsafe fn deny_level() {
    unsf();
    //~^ ERROR call to unsafe function `unsf` is unsafe and requires unsafe block
    *PTR;
    //~^ ERROR dereference of raw pointer is unsafe and requires unsafe block
    VOID = ();
    //~^ ERROR use of mutable static is unsafe and requires unsafe block

    unsafe {}
    //~^ ERROR unnecessary `unsafe` block
}

// Check that `unsafe_op_in_unsafe_fn` works starting from the `warn` level.
#[warn(unsafe_op_in_unsafe_fn)]
#[deny(warnings)]
unsafe fn warning_level() {
    unsf();
    //~^ ERROR call to unsafe function `unsf` is unsafe and requires unsafe block
    *PTR;
    //~^ ERROR dereference of raw pointer is unsafe and requires unsafe block
    VOID = ();
    //~^ ERROR use of mutable static is unsafe and requires unsafe block
    unsafe {}
    //~^ ERROR unnecessary `unsafe` block
}

unsafe fn explicit_block() {
    // no error
    unsafe {
        unsf();
        *PTR;
        VOID = ();
    }
}

unsafe fn two_explicit_blocks() {
    unsafe { unsafe { unsf() } }
    //~^ ERROR unnecessary `unsafe` block
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn allow_level() {
    // lint allowed -> no error
    unsf();
    *PTR;
    VOID = ();

    unsafe { unsf() }
}

unsafe fn nested_allow_level() {
    #[allow(unsafe_op_in_unsafe_fn)]
    {
        // lint allowed -> no error
        unsf();
        *PTR;
        VOID = ();

        unsafe { unsf() }
    }
}

fn main() {
    unsf();
    //~^ ERROR call to unsafe function `unsf` is unsafe and requires unsafe block
    #[allow(unsafe_op_in_unsafe_fn)]
    {
        unsf();
        //~^ ERROR call to unsafe function `unsf` is unsafe and requires unsafe function or block
    }
}
"##;
    assert_reports(
        &ui_std("rfc-2585-unsafe_op_in_unsafe_fn.rs", "2015", "sized, copy", ("", std_items), source),
        &[
            error(9, &["`unsf`"]),
            error(11, &[]),
            error(13, &[]),
            error(16, &["`unsafe`"]),
            error(24, &["`unsf`"]),
            error(26, &[]),
            error(28, &[]),
            error(30, &["`unsafe`"]),
            error(44, &["`unsafe`"]),
            error(71, &["`unsf`"]),
            error(75, &["`unsf`"]),
        ],
    );
}
