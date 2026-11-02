//! rustc's UI tests of the errors `check_mod_attrs` gives (`tests/ui/repr/`,
//! `tests/ui/attributes/`, `tests/ui/traits/default-method/`, `tests/ui/object-lifetime/`,
//! `tests/ui/const-generics/`, `tests/ui/force-inlining/`, `tests/ui/linkage-attr/`,
//! `tests/ui/structs/`, `tests/ui/panic-handler/`).

use crate::tests::{assert_reports, error, items::ui_core, ui};

#[test]
fn transparent_takes_no_other_hint() {
    // tests/ui/repr/repr-transparent-other-reprs.rs
    let source = r#"// See also repr-transparent.rs

#[repr(transparent, C)] //~ ERROR cannot have other repr
struct TransparentPlusC {
    ptr: *const u8
}

#[repr(transparent, packed)] //~ ERROR cannot have other repr
struct TransparentPlusPacked(*const u8);

#[repr(transparent, align(2))] //~ ERROR cannot have other repr
struct TransparentPlusAlign(u8);

#[repr(transparent)] //~ ERROR cannot have other repr
#[repr(C)]
struct SeparateAttributes(*mut u8);

fn main() {}
"#;
    let stderr = r#"error[E0692]: transparent struct cannot have other repr hints
  --> $DIR/repr-transparent-other-reprs.rs:3:8
   |
LL | #[repr(transparent, C)]
   |        ^^^^^^^^^^^  ^

error[E0692]: transparent struct cannot have other repr hints
  --> $DIR/repr-transparent-other-reprs.rs:8:8
   |
LL | #[repr(transparent, packed)]
   |        ^^^^^^^^^^^  ^^^^^^

error[E0692]: transparent struct cannot have other repr hints
  --> $DIR/repr-transparent-other-reprs.rs:11:8
   |
LL | #[repr(transparent, align(2))]
   |        ^^^^^^^^^^^  ^^^^^^^^

error[E0692]: transparent struct cannot have other repr hints
  --> $DIR/repr-transparent-other-reprs.rs:14:8
   |
LL | #[repr(transparent)]
   |        ^^^^^^^^^^^
LL | #[repr(C)]
   |        ^

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0692`.
"#;
    assert_eq!(ui("repr-transparent-other-reprs.rs", "2015", source), stderr);
}

#[test]
fn an_explicit_rust_repr_takes_no_c_simd_or_integer() {
    // tests/ui/repr/explicit-rust-repr-conflicts.rs
    let source = r#"#[repr(C, Rust)] //~ ERROR conflicting representation hints
struct S {
    a: i32,
}


#[repr(Rust)] //~ ERROR conflicting representation hints
#[repr(C)]
struct T {
    a: i32,
}

#[repr(Rust, u64)] //~ ERROR conflicting representation hints
enum U {
    V,
}

#[repr(Rust, simd)]
//~^ ERROR conflicting representation hints
//~| ERROR SIMD types are experimental and possibly buggy
struct F32x4([f32; 4]);

fn main() {}
"#;
    let stderr = r#"error[E0658]: SIMD types are experimental and possibly buggy
  --> $DIR/explicit-rust-repr-conflicts.rs:18:14
   |
LL | #[repr(Rust, simd)]
   |              ^^^^
   |
   = note: see issue #27731 <https://github.com/rust-lang/rust/issues/27731> for more information
   = help: add `#![feature(repr_simd)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0566]: conflicting representation hints
  --> $DIR/explicit-rust-repr-conflicts.rs:1:8
   |
LL | #[repr(C, Rust)]
   |        ^  ^^^^

error[E0566]: conflicting representation hints
  --> $DIR/explicit-rust-repr-conflicts.rs:7:8
   |
LL | #[repr(Rust)]
   |        ^^^^
LL | #[repr(C)]
   |        ^

error[E0566]: conflicting representation hints
  --> $DIR/explicit-rust-repr-conflicts.rs:13:8
   |
LL | #[repr(Rust, u64)]
   |        ^^^^  ^^^

error[E0566]: conflicting representation hints
  --> $DIR/explicit-rust-repr-conflicts.rs:18:8
   |
LL | #[repr(Rust, simd)]
   |        ^^^^  ^^^^

error: aborting due to 5 previous errors

Some errors have detailed explanations: E0566, E0658.
For more information about an error, try `rustc --explain E0566`.
"#;
    assert_eq!(ui("explicit-rust-repr-conflicts.rs", "2015", source), stderr);
}

#[test]
fn must_implement_one_of_names_each_function_once() {
    // tests/ui/traits/default-method/rustc_must_implement_one_of_duplicates.rs
    let source = r#"#![feature(rustc_attrs)]

#[rustc_must_implement_one_of(a, a)]
//~^ ERROR functions names are duplicated
trait Trait {
    fn a() {}
}

#[rustc_must_implement_one_of(b, a, a, c, b, c)]
//~^ ERROR functions names are duplicated
//~| ERROR functions names are duplicated
//~| ERROR functions names are duplicated
trait Trait1 {
    fn a() {}
    fn b() {}
    fn c() {}
}

fn main() {}
"#;
    let stderr = r#"error: functions names are duplicated
  --> $DIR/rustc_must_implement_one_of_duplicates.rs:3:31
   |
LL | #[rustc_must_implement_one_of(a, a)]
   |                               ^  ^
   |
   = note: all `#[rustc_must_implement_one_of]` arguments must be unique

error: functions names are duplicated
  --> $DIR/rustc_must_implement_one_of_duplicates.rs:9:34
   |
LL | #[rustc_must_implement_one_of(b, a, a, c, b, c)]
   |                                  ^  ^
   |
   = note: all `#[rustc_must_implement_one_of]` arguments must be unique

error: functions names are duplicated
  --> $DIR/rustc_must_implement_one_of_duplicates.rs:9:31
   |
LL | #[rustc_must_implement_one_of(b, a, a, c, b, c)]
   |                               ^           ^
   |
   = note: all `#[rustc_must_implement_one_of]` arguments must be unique

error: functions names are duplicated
  --> $DIR/rustc_must_implement_one_of_duplicates.rs:9:40
   |
LL | #[rustc_must_implement_one_of(b, a, a, c, b, c)]
   |                                        ^     ^
   |
   = note: all `#[rustc_must_implement_one_of]` arguments must be unique

error: aborting due to 4 previous errors

"#;
    assert_eq!(ui("rustc_must_implement_one_of_duplicates.rs", "2015", source), stderr);
}

#[test]
fn the_object_lifetime_defaults_are_dumped() {
    // tests/ui/object-lifetime/object-lifetime-default.rs
    let source = r#"#![feature(rustc_attrs)]

#[rustc_dump_object_lifetime_defaults]
struct A<
    T, //~ ERROR Empty
>(T);

#[rustc_dump_object_lifetime_defaults]
struct B<
    'a,
    T, //~ ERROR Empty
>(&'a (), T);

#[rustc_dump_object_lifetime_defaults]
struct C<
    'a,
    T: 'a, //~ ERROR 'a
>(&'a T);

#[rustc_dump_object_lifetime_defaults]
struct D<
    'a,
    'b,
    T: 'a + 'b, //~ ERROR Ambiguous
>(&'a T, &'b T);

#[rustc_dump_object_lifetime_defaults]
struct E<
    'a,
    'b: 'a,
    T: 'b, //~ ERROR 'b
>(&'a T, &'b T);

#[rustc_dump_object_lifetime_defaults]
struct F<
    'a,
    'b,
    T: 'a, //~ ERROR 'a
    U: 'b, //~ ERROR 'b
>(&'a T, &'b U);

#[rustc_dump_object_lifetime_defaults]
struct G<
    'a,
    'b,
    T: 'a,      //~ ERROR 'a
    U: 'a + 'b, //~ ERROR Ambiguous
>(&'a T, &'b U);

// Check that we also dump the default for the implicit `Self` type param of traits.
#[rustc_dump_object_lifetime_defaults]
trait H< //~ ERROR 'a
    'a,
    'b,
    T: 'b, //~ ERROR 'b
>: 'a {}

fn main() {}
"#;
    let stderr = r#"error: Empty
  --> $DIR/object-lifetime-default.rs:5:5
   |
LL |     T,
   |     ^

error: Empty
  --> $DIR/object-lifetime-default.rs:11:5
   |
LL |     T,
   |     ^

error: 'a
  --> $DIR/object-lifetime-default.rs:17:5
   |
LL |     T: 'a,
   |     ^

error: Ambiguous
  --> $DIR/object-lifetime-default.rs:24:5
   |
LL |     T: 'a + 'b,
   |     ^

error: 'b
  --> $DIR/object-lifetime-default.rs:31:5
   |
LL |     T: 'b,
   |     ^

error: 'a
  --> $DIR/object-lifetime-default.rs:38:5
   |
LL |     T: 'a,
   |     ^

error: 'b
  --> $DIR/object-lifetime-default.rs:39:5
   |
LL |     U: 'b,
   |     ^

error: 'a
  --> $DIR/object-lifetime-default.rs:46:5
   |
LL |     T: 'a,
   |     ^

error: Ambiguous
  --> $DIR/object-lifetime-default.rs:47:5
   |
LL |     U: 'a + 'b,
   |     ^

error: 'a
  --> $DIR/object-lifetime-default.rs:52:1
   |
LL | / trait H<
LL | |     'a,
LL | |     'b,
LL | |     T: 'b,
LL | | >: 'a {}
   | |_____^

error: 'b
  --> $DIR/object-lifetime-default.rs:55:5
   |
LL |     T: 'b,
   |     ^

error: aborting due to 11 previous errors

"#;
    assert_eq!(ui("object-lifetime-default.rs", "2015", source), stderr);
}

#[test]
fn may_dangle_is_for_the_parameters_of_a_drop_impl() {
    // tests/ui/attributes/may_dangle.rs
    let source = r#"#![feature(dropck_eyepatch)]

struct Implee1<'a, T, const N: usize>(&'a T);
struct Implee2<'a, T, const N: usize>(&'a T);
struct Implee3<'a, T, const N: usize>(&'a T);
trait NotDrop {}

unsafe impl<#[may_dangle] 'a, T, const N: usize> NotDrop for Implee1<'a, T, N> {}
//~^ ERROR must be applied to a lifetime or type generic parameter in `Drop` impl

unsafe impl<'a, #[may_dangle] T, const N: usize> NotDrop for Implee2<'a, T, N> {}
//~^ ERROR must be applied to a lifetime or type generic parameter in `Drop` impl

unsafe impl<'a, T, #[may_dangle] const N: usize> Drop for Implee1<'a, T, N> {
    //~^ ERROR attribute cannot be used on
    fn drop(&mut self) {}
}

// Ok, lifetime param in a `Drop` impl.
unsafe impl<#[may_dangle] 'a, T, const N: usize> Drop for Implee2<'a, T, N> {
    fn drop(&mut self) {}
}

// Ok, type param in a `Drop` impl.
unsafe impl<'a, #[may_dangle] T, const N: usize> Drop for Implee3<'a, T, N> {
    fn drop(&mut self) {}
}

// Check that this check is not textual.
mod fake {
    trait Drop {
        fn drop(&mut self);
    }
    struct Implee<T>(T);

    unsafe impl<#[may_dangle] T> Drop for Implee<T> {
        //~^ ERROR must be applied to a lifetime or type generic parameter in `Drop` impl
        fn drop(&mut self) {}
    }
}

#[may_dangle] //~ ERROR attribute cannot be used on
struct Dangling;

#[may_dangle] //~ ERROR attribute cannot be used on
impl NotDrop for () {
}

#[may_dangle] //~ ERROR attribute cannot be used on
fn main() {
    #[may_dangle] //~ ERROR attribute cannot be used on
    let () = ();
}
"#;
    assert_reports(
        &ui_core("may_dangle.rs", "2015", "drop", source),
        &[
            error(14, &["`may_dangle`"]),
            error(42, &["`may_dangle`"]),
            error(45, &["`may_dangle`"]),
            error(49, &["`may_dangle`"]),
            error(51, &["`may_dangle`"]),
            error(8, &["`#[may_dangle]`", "`Drop`"]),
            error(11, &["`#[may_dangle]`", "`Drop`"]),
            error(36, &["`#[may_dangle]`", "`Drop`"]),
        ],
    );
}

#[test]
fn optimize_none_takes_no_inline_but_never() {
    // The last three functions of tests/ui/attributes/optimize.rs, whose `use std::future`
    // needs std; the lines are the test's own.
    let source = r#"#![feature(optimize_attribute)]

#[optimize(none)] //~ ERROR `#[optimize(none)]` cannot be used with `#[inline]` attributes
#[inline]
fn inline_conflict_a() {}

#[inline(always)]
#[optimize(none)] //~ ERROR `#[optimize(none)]` cannot be used with `#[inline]` attributes
fn inline_conflict_b() {}

#[inline(never)]
#[optimize(none)]
fn inline_conflict_c() {}
"#;
    let conflict = ["`#[optimize(none)]`", "`#[inline]`"];
    assert_reports(&ui("optimize.rs", "2018", source), &[error(3, &conflict), error(8, &conflict)]);
}

#[test]
fn legacy_const_generics_name_each_const_parameter_by_an_argument() {
    // The functions of tests/ui/const-generics/invalid-rustc_legacy_const_generics-arguments.rs
    // that only `check_mod_attrs` rejects (its `extern` block also gets E0044 of
    // `rustc_hir_analysis`); the lines are the test's own.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_legacy_const_generics(0)] //~ ERROR #[rustc_legacy_const_generics] must have one index for
fn foo1() {}

#[rustc_legacy_const_generics(1)] //~ ERROR index exceeds number of arguments
fn foo2<const X: usize>() {}

#[rustc_legacy_const_generics(2)] //~ ERROR index exceeds number of arguments
fn foo3<const X: usize>(_: u8) {}

#[rustc_legacy_const_generics(0)] //~ ERROR #[rustc_legacy_const_generics] functions must only have
fn foo8<X>() {}

fn main() {}
"#;
    let stderr = r#"error: #[rustc_legacy_const_generics] must have one index for each generic parameter
  --> $DIR/invalid-rustc_legacy_const_generics-arguments.rs:3:1
   |
LL | #[rustc_legacy_const_generics(0)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL | fn foo1() {}
   |        - generic parameters

error: index exceeds number of arguments
  --> $DIR/invalid-rustc_legacy_const_generics-arguments.rs:6:31
   |
LL | #[rustc_legacy_const_generics(1)]
   |                               ^ there is only 1 argument

error: index exceeds number of arguments
  --> $DIR/invalid-rustc_legacy_const_generics-arguments.rs:9:31
   |
LL | #[rustc_legacy_const_generics(2)]
   |                               ^ there are only 2 arguments

error: #[rustc_legacy_const_generics] functions must only have const generics
  --> $DIR/invalid-rustc_legacy_const_generics-arguments.rs:12:1
   |
LL | #[rustc_legacy_const_generics(0)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL | fn foo8<X>() {}
   |         - non-const generic parameter

error: aborting due to 4 previous errors

"#;
    assert_eq!(ui("invalid-rustc_legacy_const_generics-arguments.rs", "2015", source), stderr);
}

#[test]
fn force_inline_is_not_for_coroutines() {
    // The `async`, `gen` and `async gen` functions of tests/ui/force-inlining/invalid.rs
    // (edition 2024); the lines are the test's own.
    let source = r#"#![feature(gen_blocks)]
#![feature(rustc_attrs)]

#[rustc_force_inline]
//~^ ERROR attribute cannot be applied to a `async`, `gen` or `async gen` function
async fn async_foo() {}

#[rustc_force_inline]
//~^ ERROR attribute cannot be applied to a `async`, `gen` or `async gen` function
gen fn gen_foo() {}

#[rustc_force_inline]
//~^ ERROR attribute cannot be applied to a `async`, `gen` or `async gen` function
async gen fn async_gen_foo() {}
"#;
    assert_reports(&ui_core("invalid.rs", "2024", "future", source), &[error(4, &["`async`", "`gen`", "`async gen`"]), error(8, &["`async`", "`gen`", "`async gen`"]), error(12, &["`async`", "`gen`", "`async gen`"])]);
}

#[test]
fn an_extern_static_mut_takes_no_linkage() {
    // The `extern` block of tests/ui/linkage-attr/linkage-attr-mutable-static.rs, whose
    // `assert_eq!` needs std; the lines are the test's own.
    let source = r#"#![feature(linkage)]

fn main() {
    extern "C" {
        #[linkage = "extern_weak"]
        //~^ ERROR extern mutable statics are incompatible with the `linkage` attribute
        static mut EXTERN_WEAK: *const u8;
    }

    // static mut is fine here as this is a definition rather than declaration.
    #[linkage = "weak"]
    static mut WEAK_DEF: u8 = 42;
}
"#;
    let stderr = r#"error: extern mutable statics are incompatible with the `linkage` attribute
  --> $DIR/linkage-attr-mutable-static.rs:5:9
   |
LL |         #[linkage = "extern_weak"]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: the `linkage` attribute on extern statics generates a symbol that contains the address of another static. Making the extern static mutable would allow changing the address, rather than the static the address is pointing to

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("linkage-attr-mutable-static.rs", "2015", source), stderr);
}

#[test]
fn non_exhaustive_takes_no_default_field_values() {
    // The struct of tests/ui/structs/default-field-values-non_exhaustive.rs without its
    // `#[derive(Default)]`, whose derive needs core; the lines are the test's own.
    let source = r#"#![feature(default_field_values)]

#[non_exhaustive] //~ ERROR `#[non_exhaustive]` can't be used to annotate items with default field values
struct Foo {
    x: i32 = 42 + 3,
}

fn main () {}
"#;
    let stderr = r#"error: `#[non_exhaustive]` can't be used to annotate items with default field values
  --> $DIR/default-field-values-non_exhaustive.rs:3:1
   |
LL |   #[non_exhaustive]
   |   ^^^^^^^^^^^^^^^^^
LL | / struct Foo {
LL | |     x: i32 = 42 + 3,
LL | | }
   | |_- this struct has default field values

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("default-field-values-non_exhaustive.rs", "2015", source), stderr);
}

#[test]
fn an_inline_macro_rules_must_be_exported() {
    // tests/ui/attributes/inline/attr-usage-inline.rs
    let source = r#"//! Check that `#[inline]` attribute can only be applied to fn-like targets (e.g. function or
//! closure), and when misapplied to other targets an error is emitted.

#[inline]
fn f() {}

#[inline] //~ ERROR: attribute cannot be used on
struct S;

struct I {
    #[inline]
    //~^ WARN attribute cannot be used on
    //~| WARN previously accepted
    i: u8,
}

#[macro_export]
#[inline]
//~^ WARN attribute cannot be used on
//~| WARN previously accepted
macro_rules! m_e {
    () => {};
}

#[inline] //~ ERROR: attribute should be applied to function or closure
//~^ WARN attribute cannot be used on
//~| WARN previously accepted
macro_rules! m {
    () => {};
}

fn main() {}
"#;
    let stderr = r#"error: the `inline` attribute cannot be used on structs
  --> $DIR/attr-usage-inline.rs:7:3
   |
LL | #[inline]
   |   ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions

error[E0518]: attribute should be applied to function or closure
  --> $DIR/attr-usage-inline.rs:25:1
   |
LL | #[inline]
   | ^^^^^^^^^ not a function or closure

warning: the `inline` attribute cannot be used on struct fields
  --> $DIR/attr-usage-inline.rs:11:7
   |
LL |     #[inline]
   |       ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: requested on the command line with `-W unused-attributes`

warning: the `inline` attribute cannot be used on macro defs
  --> $DIR/attr-usage-inline.rs:18:3
   |
LL | #[inline]
   |   ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!

warning: the `inline` attribute cannot be used on macro defs
  --> $DIR/attr-usage-inline.rs:25:3
   |
LL | #[inline]
   |   ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!

error: aborting due to 2 previous errors; 3 warnings emitted

For more information about this error, try `rustc --explain E0518`.
"#;
    assert_eq!(ui("attr-usage-inline.rs", "2015", source), stderr);
}
