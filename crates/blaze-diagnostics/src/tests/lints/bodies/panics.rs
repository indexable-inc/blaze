//! `arithmetic_overflow` and `unconditional_panic`, the known-panics lint, from rustc's UI tests
//! with their `.stderr`. A test that uses std has the items of std it uses written after its
//! source, so that the lines and columns are its `.stderr`'s.

use crate::tests::{assert_reports, error, warning};

fn ui(name: &str, source: &str) -> String {
    crate::tests::ui(name, "2015", source)
}

/// [`ui`] with core's indexing, unsizing and negation, which rustc's type check reaches only
/// through their lang items, as the UI test has them from std.
fn ui_core(name: &str, source: &str) -> String {
    const FLAGS: &str = "sized, copy, unsize, coerce_unsized, index, slice, unary_ops, builtin_impls, int_bounds";
    crate::tests::items::ui_core(name, "2015", FLAGS, source)
}

#[test]
fn an_index_of_an_array_literal_by_a_cast() {
    // tests/ui/consts/issue-54348.rs
    let source = r##"//@ build-fail

fn main() {
    [1][0u64 as usize];
    [1][1.5 as usize]; //~ ERROR operation will panic
    [1][1u64 as usize]; //~ ERROR operation will panic
}
"##;
    assert_reports(
        &ui_core("issue-54348.rs", source),
        &[error(5, &[]), error(6, &[])],
    );
}

#[test]
fn an_index_of_a_borrowed_array() {
    // tests/ui/lint/unconditional_panic_98444.rs
    let source = r##"//@ build-fail

fn main() {
    let xs: [i32; 5] = [1, 2, 3, 4, 5];
    let _ = &xs;
    let _ = xs[7]; //~ ERROR: this operation will panic at runtime [unconditional_panic]
}
"##;
    assert_reports(
        &ui_core("unconditional_panic_98444.rs", source),
        &[error(6, &[])],
    );
}

#[test]
fn an_index_through_a_reference_to_a_promoted_array() {
    // tests/ui/lint/unconditional_panic_promoted.rs
    let source = r##"//@ build-fail

fn main() {
    // MIR encodes this as a reborrow from a promoted constant.
    // But the array length can still be gotten from the type.
    let slice = &[0, 1];
    let _ = slice[2]; //~ ERROR: this operation will panic at runtime [unconditional_panic]
}
"##;
    assert_reports(
        &ui_core("unconditional_panic_promoted.rs", source),
        &[error(7, &[])],
    );
}

#[test]
fn a_warning_where_the_lint_is_warn() {
    // tests/ui/consts/array-literal-index-oob.rs
    let source = r##"//@ build-pass
//@ no-pass-override (test emits codegen-time warnings and verifies that they are not errors)

#![warn(unconditional_panic)]

fn main() {
    &{ [1, 2, 3][4] };
    //~^ WARN operation will panic
}
"##;
    assert_reports(
        &ui_core("array-literal-index-oob.rs", source),
        &[warning(7, &[])],
    );
}

#[test]
fn an_index_of_a_repeat_expression() {
    // tests/ui/const_prop/const-prop-ice.rs
    let source = r##"//@ build-fail

fn main() {
    [0; 3][3u64 as usize]; //~ ERROR this operation will panic at runtime
}
"##;
    assert_reports(
        &ui_core("const-prop-ice.rs", source),
        &[error(4, &[])],
    );
}

#[test]
fn division_and_remainder_by_a_local_that_is_zero_but_not_an_index_of_a_raw_slice() {
    // tests/ui/mir/mir_detects_invalid_ops.rs
    let source = r##"//@ build-fail

fn main() {
    divide_by_zero();
    mod_by_zero();
    oob_error_for_slices();
}

fn divide_by_zero() {
    let y = 0;
    let _z = 1 / y; //~ ERROR this operation will panic at runtime [unconditional_panic]
}

fn mod_by_zero() {
    let y = 0;
    let _z = 1 % y; //~ ERROR this operation will panic at runtime [unconditional_panic]
}

fn oob_error_for_slices() {
    let a: *const [_] = &[1, 2, 3];
    unsafe {
        let _b = (*a)[3];
    }
}
"##;
    assert_reports(&ui_core("mir_detects_invalid_ops.rs", source), &[error(11, &[]), error(16, &[])]);
}

#[test]
fn an_index_near_u32_max() {
    // tests/ui/indexing/index-bounds.rs, with std's `u32::MAX`
    let source = r##"//@ build-fail

fn main() {
    let _n = [64][200];
    //~^ ERROR this operation will panic at runtime [unconditional_panic]

    // issue #121126, test index value between 0xFFFF_FF00 and u32::MAX
    let _n = [64][u32::MAX as usize - 1];
    //~^ ERROR this operation will panic at runtime [unconditional_panic]
}
"##;
    assert_reports(
        &ui_core("index-bounds.rs", source),
        &[error(4, &[]), error(8, &[])],
    );
}

#[test]
fn associated_consts_of_a_generic_impl() {
    // tests/ui/associated-consts/issue-69020-assoc-const-arith-overflow.rs, revision `noopt`,
    // with std's `i32::MIN`, `i32::MAX` and a `Vec`
    let source = r##"//@ revisions: noopt opt opt_with_overflow_checks
//@[noopt]compile-flags: -C opt-level=0
//@[opt]compile-flags: -O
//@[opt_with_overflow_checks]compile-flags: -C overflow-checks=on -O

#![crate_type="lib"]

pub trait Foo {
    const NEG: i32;
    const NEG_REV: i32;

    const ADD: i32;
    const ADD_REV: i32;

    const DIV: i32;
    const DIV_REV: i32;

    const OOB: i32;
    const OOB_REV: i32;
}

// These constants cannot be evaluated already (they depend on `T::N`), so they can just be linted
// like normal run-time code. But codegen works a bit different in const context, so this test
// makes sure that we still catch overflow. Also make sure we emit the same lints if we reverse the
// operands (so that the generic operand comes first).
impl<T: Foo> Foo for Vec<T> {
    const NEG: i32 = -i32::MIN + T::NEG;
    //~^ ERROR arithmetic operation will overflow
    const NEG_REV: i32 = T::NEG + (-i32::MIN);
    //~^ ERROR arithmetic operation will overflow

    const ADD: i32 = (i32::MAX+1) + T::ADD;
    //~^ ERROR arithmetic operation will overflow
    const ADD_REV: i32 =  T::ADD + (i32::MAX+1);
    //~^ ERROR arithmetic operation will overflow

    const DIV: i32 = (1/0) + T::DIV;
    //~^ ERROR operation will panic
    const DIV_REV: i32 = T::DIV + (1/0);
    //~^ ERROR operation will panic

    const OOB: i32 = [1][1] + T::OOB;
    //~^ ERROR operation will panic
    const OOB_REV: i32 = T::OOB + [1][1];
    //~^ ERROR operation will panic
}
pub struct Vec<T>(T);
fn main() {}
"##;
    let stderr = r##"error: this arithmetic operation will overflow
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:27:22
   |
LL |     const NEG: i32 = -i32::MIN + T::NEG;
   |                      ^^^^^^^^^ attempt to negate `i32::MIN`, which would overflow
   |
   = note: `#[deny(arithmetic_overflow)]` on by default

error: this arithmetic operation will overflow
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:29:35
   |
LL |     const NEG_REV: i32 = T::NEG + (-i32::MIN);
   |                                   ^^^^^^^^^^^ attempt to negate `i32::MIN`, which would overflow

error: this arithmetic operation will overflow
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:32:22
   |
LL |     const ADD: i32 = (i32::MAX+1) + T::ADD;
   |                      ^^^^^^^^^^^^ attempt to compute `i32::MAX + 1_i32`, which would overflow

error: this arithmetic operation will overflow
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:34:36
   |
LL |     const ADD_REV: i32 =  T::ADD + (i32::MAX+1);
   |                                    ^^^^^^^^^^^^ attempt to compute `i32::MAX + 1_i32`, which would overflow

error: this operation will panic at runtime
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:37:22
   |
LL |     const DIV: i32 = (1/0) + T::DIV;
   |                      ^^^^^ attempt to divide `1_i32` by zero
   |
   = note: `#[deny(unconditional_panic)]` on by default

error: this operation will panic at runtime
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:39:35
   |
LL |     const DIV_REV: i32 = T::DIV + (1/0);
   |                                   ^^^^^ attempt to divide `1_i32` by zero

error: this operation will panic at runtime
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:42:22
   |
LL |     const OOB: i32 = [1][1] + T::OOB;
   |                      ^^^^^^ index out of bounds: the length is 1 but the index is 1

error: this operation will panic at runtime
  --> $DIR/issue-69020-assoc-const-arith-overflow.rs:44:35
   |
LL |     const OOB_REV: i32 = T::OOB + [1][1];
   |                                   ^^^^^^ index out of bounds: the length is 1 but the index is 1

error: aborting due to 8 previous errors

"##;
    assert_eq!(ui_core("issue-69020-assoc-const-arith-overflow.rs", source), stderr);
}

#[test]
fn a_division_by_zero_of_an_unknown_value_in_a_const_fn() {
    // tests/ui/const_prop/ice-assert-fail-div-by-zero.rs, a library: with a `main`
    let source = r##"//@ check-pass

// need to emit MIR, because const prop (which emits `unconditional_panic`) only runs if
// the `optimized_mir` query is run, which it isn't in check-only mode.
//@ compile-flags: --crate-type lib --emit=mir,link

#![warn(unconditional_panic)]

pub struct Fixed64(i64);

// HACK: this test passes only because this is a const fn that is written to metadata
pub const fn div(f: Fixed64) {
    f.0 / 0; //~ WARN will panic at runtime
}
fn main() {}
"##;
    let stderr = r##"warning: this operation will panic at runtime
  --> $DIR/ice-assert-fail-div-by-zero.rs:13:5
   |
LL |     f.0 / 0;
   |     ^^^^^^^ attempt to divide `_` by zero
   |
note: the lint level is defined here
  --> $DIR/ice-assert-fail-div-by-zero.rs:7:9
   |
LL | #![warn(unconditional_panic)]
   |         ^^^^^^^^^^^^^^^^^^^

warning: 1 warning emitted

"##;
    assert_eq!(ui("ice-assert-fail-div-by-zero.rs", source), stderr);
}

#[test]
fn nothing_where_the_lint_is_allowed() {
    // tests/ui/consts/issue-69312.rs
    let source = r##"//@ build-pass

// Verify that the compiler doesn't ICE during const prop while evaluating the index operation.

#![allow(unconditional_panic)]

fn main() {
    let cols = [0u32; 0];
    cols[0];
}
"##;
    assert_eq!(ui_core("issue-69312.rs", source), "");
}

#[test]
fn nothing_of_an_index_of_an_empty_array_where_the_lint_is_allowed() {
    // tests/ui/consts/empty-array-index-in-struct-init.rs, a library: with a `main`
    let source = r##"//! Regression test for https://github.com/rust-lang/rust/issues/53275

//@ build-pass

#![crate_type = "lib"]
#![allow(unconditional_panic)]
struct S(u8);

pub fn ice() {
    S([][0]);
}
fn main() {}
"##;
    assert_eq!(ui_core("empty-array-index-in-struct-init.rs", source), "");
}

#[test]
fn nothing_of_a_division_by_zero_allowed_on_the_function() {
    // tests/ui/numbers-arithmetic/divide-by-zero.rs
    let source = r##"//@ run-fail
//@ error-pattern:attempt to divide by zero
//@ needs-subprocess

#[allow(unconditional_panic)]
fn main() {
    let y = 0;
    let _z = 1 / y;
}
"##;
    assert_eq!(ui("divide-by-zero.rs", source), "");
}

#[test]
fn nothing_of_an_index_of_a_raw_slice() {
    // tests/ui/array-slice-vec/dst-raw-slice.rs
    let source = r##"// Test bounds checking for DST raw slices

//@ run-fail
//@ error-pattern:index out of bounds
//@ needs-subprocess

#[allow(unconditional_panic)]
fn main() {
    let a: *const [_] = &[1, 2, 3];
    unsafe {
        let _b = (*a)[3];
    }
}
"##;
    assert_reports(&ui_core("dst-raw-slice.rs", source), &[]);
}

#[test]
fn shifts_by_the_width_of_the_left_side() {
    // As tests/ui/numbers-arithmetic/overflowing-lsh-4.rs and overflowing-rsh-4.rs, without
    // their `assert_eq!`s of std.
    let source = r##"fn main() {
    let _x = 1_i8 << 17;
    let _y = 2_i8 >> 17;
    let _z = 1_u64 << 63;
}
"##;
    assert_reports(
        &ui("shifts.rs", source),
        &[error(2, &[]), error(3, &[])],
    );
}

#[test]
fn a_const_parameter_marked_to_panic_when_zero() {
    // As tests/ui/lint/const-n-is-zero.rs, of a function of its own rather than core's.
    let source = r##"#![feature(rustc_attrs)]

const ZERO: usize = 0;

fn chunks<#[rustc_panics_when_zero] const N: usize>() {}

fn main() {
    chunks::<0>();
    chunks::<{ 1 - 1 }>();
    chunks::<ZERO>();
    chunks::<1>();
}
"##;
    assert_reports(
        &ui("const-n-is-zero.rs", source),
        &[error(8, &[]), error(9, &[]), error(10, &[])],
    );
}
