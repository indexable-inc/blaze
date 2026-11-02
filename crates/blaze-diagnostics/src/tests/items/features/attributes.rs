//! rustc's UI tests of the feature gates of attributes (`tests/ui/feature-gates/
//! feature-gate-*.rs`, `tests/ui/attributes/`): an unstable attribute's E0658 with each of
//! `check_attribute_stability`'s kinds of text, and the gated `cfg` predicates expansion meets.

use super::{ui, ui_core};

#[test]
fn an_internal_attribute_has_its_notes() {
    // tests/ui/feature-gates/feature-gate-rustc-attrs-1.rs
    let source = r#"// Test that `#[rustc_*]` attributes are gated by `rustc_attrs` feature gate.

#[rustc_nonnull_optimization_guaranteed]
//~^ ERROR use of an internal attribute [E0658]
//~| NOTE the `rustc_nonnull_optimization_guaranteed` attribute is an internal implementation detail that will never be stable
//~| NOTE  the `rustc_nonnull_optimization_guaranteed` attribute is just used to document guaranteed niche optimizations in the standard library
//~| NOTE the compiler does not even check whether the type indeed is being non-null-optimized; it is your responsibility to ensure that the attribute is only used on types that are optimized
struct Foo {}

#[rustc_dump_variances]
//~^ ERROR use of an internal attribute [E0658]
//~| NOTE the `rustc_dump_variances` attribute is an internal implementation detail that will never be stable
//~| NOTE the `rustc_dump_variances` attribute is used for rustc unit tests
enum E {}

fn main() {}
"#;
    let stderr = r#"error[E0658]: use of an internal attribute
  --> $DIR/feature-gate-rustc-attrs-1.rs:3:3
   |
LL | #[rustc_nonnull_optimization_guaranteed]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(rustc_attrs)]` to the crate attributes to enable
   = note: the `rustc_nonnull_optimization_guaranteed` attribute is an internal implementation detail that will never be stable
   = note: the `rustc_nonnull_optimization_guaranteed` attribute is just used to document guaranteed niche optimizations in the standard library
   = note: the compiler does not even check whether the type indeed is being non-null-optimized; it is your responsibility to ensure that the attribute is only used on types that are optimized

error[E0658]: use of an internal attribute
  --> $DIR/feature-gate-rustc-attrs-1.rs:10:3
   |
LL | #[rustc_dump_variances]
   |   ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(rustc_attrs)]` to the crate attributes to enable
   = note: the `rustc_dump_variances` attribute is an internal implementation detail that will never be stable
   = note: the `rustc_dump_variances` attribute is used for rustc unit tests

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-rustc-attrs-1.rs", "2015", source), stderr);
}

#[test]
fn an_internal_attribute_is_gated_before_its_arguments_are_read() {
    // tests/ui/attributes/malformed-never-type-options.rs
    let source = r#"//! Regression test for #124352
//! The `rustc_*` attribute is malformed, but ICEing without a `feature(rustc_attrs)` is still bad.

#![rustc_never_type_options(: Unsize<U> = "hi")]
//~^ ERROR expected a literal
//~| ERROR use of an internal attribute

fn main() {}
"#;
    let stderr = r#"error[E0658]: use of an internal attribute
  --> $DIR/malformed-never-type-options.rs:4:4
   |
LL | #![rustc_never_type_options(: Unsize<U> = "hi")]
   |    ^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(rustc_attrs)]` to the crate attributes to enable
   = note: the `rustc_never_type_options` attribute is an internal implementation detail that will never be stable
   = note: `rustc_never_type_options` is used to experiment with never type fallback and work on never type stabilization

error: expected a literal (`1u8`, `1.0f32`, `"string"`, etc.) here, found `:`
  --> $DIR/malformed-never-type-options.rs:4:29
   |
LL | #![rustc_never_type_options(: Unsize<U> = "hi")]
   |                             ^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("malformed-never-type-options.rs", "2015", source), stderr);
}

#[test]
fn a_stability_attribute_has_no_help() {
    // tests/ui/feature-gates/feature-gate-staged_api.rs
    let source = r#"#![stable(feature = "a", since = "3.3.3")]
//~^ ERROR stability attributes may not be used outside of the standard library
mod inner_private_module {
    // UnnameableTypeAlias isn't marked as reachable, so no stability annotation is required here
    pub type UnnameableTypeAlias = u8;
}

#[stable(feature = "a", since = "3.3.3")]
//~^ ERROR stability attributes may not be used outside of the standard library
pub fn f() -> inner_private_module::UnnameableTypeAlias {
    0
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: stability attributes may not be used outside of the standard library
  --> $DIR/feature-gate-staged_api.rs:1:4
   |
LL | #![stable(feature = "a", since = "3.3.3")]
   |    ^^^^^^

error[E0658]: stability attributes may not be used outside of the standard library
  --> $DIR/feature-gate-staged_api.rs:8:3
   |
LL | #[stable(feature = "a", since = "3.3.3")]
   |   ^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-staged_api.rs", "2015", source), stderr);
}

#[test]
fn thread_local_says_it_does_not_handle_destructors() {
    // tests/ui/feature-gates/feature-gate-thread_local.rs
    let source = r#"// Test that `#[thread_local]` attribute is gated by `thread_local`
// feature gate.
//
// (Note that the `thread_local!` macro is explicitly *not* gated; it
// is given permission to expand into this unstable attribute even
// when the surrounding context does not have permission to use it.)

#[thread_local] //~ ERROR the `thread_local` attribute is an experimental feature
static FOO: i32 = 3;

pub fn main() {}
"#;
    let stderr = r#"error[E0658]: the `thread_local` attribute is an experimental feature, and does not currently handle destructors
  --> $DIR/feature-gate-thread_local.rs:8:3
   |
LL | #[thread_local]
   |   ^^^^^^^^^^^^
   |
   = note: see issue #29594 <https://github.com/rust-lang/rust/issues/29594> for more information
   = help: add `#![feature(thread_local)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-thread_local.rs", "2015", source), stderr);
}

#[test]
fn linkage_is_not_portable() {
    // tests/ui/feature-gates/feature-gate-linkage.rs
    let source = r#"extern "C" {
    #[linkage = "extern_weak"] static foo: *mut isize;
    //~^ ERROR: the `linkage` attribute is experimental and not portable
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `linkage` attribute is experimental and not portable across platforms
  --> $DIR/feature-gate-linkage.rs:2:7
   |
LL |     #[linkage = "extern_weak"] static foo: *mut isize;
   |       ^^^^^^^
   |
   = note: see issue #29603 <https://github.com/rust-lang/rust/issues/29603> for more information
   = help: add `#![feature(linkage)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-linkage.rs", "2015", source), stderr);
}

#[test]
fn an_unsafe_attribute_is_gated_at_its_path() {
    // tests/ui/feature-gates/feature-gate-ffi_const.rs
    let source = r#"#![crate_type = "lib"]

extern "C" {
    #[unsafe(ffi_const)] //~ ERROR the `ffi_const` attribute is an experimental feature
    pub fn foo();
}
"#;
    let stderr = r#"error[E0658]: the `ffi_const` attribute is an experimental feature
  --> $DIR/feature-gate-ffi_const.rs:4:14
   |
LL |     #[unsafe(ffi_const)]
   |              ^^^^^^^^^
   |
   = note: see issue #58328 <https://github.com/rust-lang/rust/issues/58328> for more information
   = help: add `#![feature(ffi_const)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-ffi_const.rs", "2015", source), stderr);
}

#[test]
fn allow_internal_unstable_side_steps_the_gates() {
    // tests/ui/feature-gates/feature-gate-allow-internal-unstable.rs
    let source = r#"#![allow(unused_macros)]

#[allow_internal_unstable()] //~ ERROR the `allow_internal_unstable` attribute side-steps
macro_rules! foo {
    () => {}
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `allow_internal_unstable` attribute side-steps feature gating and stability checks
  --> $DIR/feature-gate-allow-internal-unstable.rs:3:3
   |
LL | #[allow_internal_unstable()]
   |   ^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(allow_internal_unstable)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-allow-internal-unstable.rs", "2015", source), stderr);
}

#[test]
fn an_attribute_a_macro_writes_is_gated_in_its_expansion() {
    // tests/ui/feature-gates/feature-gate-allow-internal-unsafe-nested-macro.rs
    let source = r#"// gate-test-allow_internal_unsafe

#![allow(unused_macros)]

macro_rules! bar {
    () => {
        // more layers don't help:
        #[allow_internal_unsafe] //~ ERROR the `allow_internal_unsafe` attribute side-steps
        macro_rules! baz {
            () => {}
        }
    }
}

bar!();

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `allow_internal_unsafe` attribute side-steps the `unsafe_code` lint
  --> $DIR/feature-gate-allow-internal-unsafe-nested-macro.rs:8:11
   |
LL |         #[allow_internal_unsafe]
   |           ^^^^^^^^^^^^^^^^^^^^^
...
LL | bar!();
   | ------ in this macro invocation
   |
   = help: add `#![feature(allow_internal_unsafe)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = note: this error originates in the macro `bar` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-allow-internal-unsafe-nested-macro.rs", "2015", source), stderr);
}

#[test]
fn profiler_runtime_will_never_be_stable() {
    // tests/ui/feature-gates/feature-gate-profiler-runtime.rs
    let source = r#"#![profiler_runtime] //~ ERROR the `profiler_runtime` attribute is

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `profiler_runtime` attribute is used to identify the `profiler_builtins` crate which contains the profiler runtime and will never be stable
  --> $DIR/feature-gate-profiler-runtime.rs:1:4
   |
LL | #![profiler_runtime]
   |    ^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(profiler_runtime)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-profiler-runtime.rs", "2015", source), stderr);
}

#[test]
fn compiler_builtins_will_never_be_stable() {
    // tests/ui/feature-gates/feature-gate-compiler-builtins.rs
    let source = r#"#![compiler_builtins] //~ ERROR the `compiler_builtins` attribute is

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `compiler_builtins` attribute is used to identify the `compiler_builtins` crate which contains compiler-rt intrinsics and will never be stable
  --> $DIR/feature-gate-compiler-builtins.rs:1:4
   |
LL | #![compiler_builtins]
   |    ^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(compiler_builtins)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-compiler-builtins.rs", "2015", source), stderr);
}

#[test]
fn rustc_intrinsic_declares_intrinsics() {
    // tests/ui/feature-gates/feature-gate-intrinsics.rs
    let source = r#"#[rustc_intrinsic]
//~^ ERROR the `rustc_intrinsic` attribute is used to declare intrinsics as function items
fn bar();

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `rustc_intrinsic` attribute is used to declare intrinsics as function items
  --> $DIR/feature-gate-intrinsics.rs:1:3
   |
LL | #[rustc_intrinsic]
   |   ^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(intrinsics)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-intrinsics.rs", "2015", source), stderr);
}

#[test]
fn may_dangle_has_unstable_semantics() {
    // tests/ui/feature-gates/feature-gate-may-dangle.rs
    let source = r#"// gate-test-dropck_eyepatch

// Check that `may_dangle` is rejected if `dropck_eyepatch` feature gate is absent.

struct Pt<A>(A);
unsafe impl<#[may_dangle] A> Drop for Pt<A> {
    //~^ ERROR the `may_dangle` attribute has unstable semantics and may be removed in the future
    fn drop(&mut self) { }
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: the `may_dangle` attribute has unstable semantics and may be removed in the future
  --> $DIR/feature-gate-may-dangle.rs:6:15
   |
LL | unsafe impl<#[may_dangle] A> Drop for Pt<A> {
   |               ^^^^^^^^^^
   |
   = note: see issue #34761 <https://github.com/rust-lang/rust/issues/34761> for more information
   = help: add `#![feature(dropck_eyepatch)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui_core("feature-gate-may-dangle.rs", "2015", "drop", source), stderr);
}

#[test]
fn a_gated_cfg_name_value_is_gated_whole() {
    // tests/ui/feature-gates/feature-gate-cfg-relocation-model.rs
    let source = r#"#[cfg(relocation_model = "pic")] //~ ERROR
fn _foo() {}

fn main() {}
"#;
    let stderr = r#"error[E0658]: `cfg(relocation_model)` is experimental and subject to change
  --> $DIR/feature-gate-cfg-relocation-model.rs:1:7
   |
LL | #[cfg(relocation_model = "pic")]
   |       ^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #114929 <https://github.com/rust-lang/rust/issues/114929> for more information
   = help: add `#![feature(cfg_relocation_model)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-cfg-relocation-model.rs", "2015", source), stderr);
}

#[test]
fn a_gated_cfg_without_an_issue_has_no_issue_note() {
    // tests/ui/feature-gates/feature-gate-cfg-target-has-threads.rs
    let source = r#"#[cfg(target_has_threads)]
//~^ ERROR `cfg(target_has_threads)` is experimental and subject to change
fn bar() {}

fn main() {}
"#;
    let stderr = r#"error[E0658]: `cfg(target_has_threads)` is experimental and subject to change
  --> $DIR/feature-gate-cfg-target-has-threads.rs:1:7
   |
LL | #[cfg(target_has_threads)]
   |       ^^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(cfg_target_has_threads)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-cfg-target-has-threads.rs", "2015", source), stderr);
}

#[test]
fn a_gated_cfg_after_another_attribute_is_gated() {
    // tests/ui/feature-gates/feature-gate-cfg_target_object_format.rs
    let source = r#"#[allow(unused)]
#[cfg(target_object_format = "elf")]
//~^ ERROR `cfg(target_object_format)` is experimental
const X: () = ();

fn main() {}
"#;
    let stderr = r#"error[E0658]: `cfg(target_object_format)` is experimental and subject to change
  --> $DIR/feature-gate-cfg_target_object_format.rs:2:7
   |
LL | #[cfg(target_object_format = "elf")]
   |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #152586 <https://github.com/rust-lang/rust/issues/152586> for more information
   = help: add `#![feature(cfg_target_object_format)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-cfg_target_object_format.rs", "2015", source), stderr);
}

#[test]
fn compact_target_is_gated_in_cfg_cfg_attr_and_cfg_macro() {
    // tests/ui/feature-gates/feature-gate-cfg-target-compact.rs
    let source = r#"#[cfg(target(os = "linux"))] //~ ERROR compact `cfg(target(..))` is experimental
struct Foo(u64, u64);

#[cfg_attr(target(os = "linux"), non_exhaustive)] //~ ERROR compact `cfg(target(..))` is experimental
struct Bar(u64, u64);

#[cfg(not(any(all(target(os = "linux")))))] //~ ERROR compact `cfg(target(..))` is experimental
fn foo() {}

fn main() {
    cfg!(target(os = "linux"));
    //~^ ERROR compact `cfg(target(..))` is experimental and subject to change
}
"#;
    let stderr = r#"error[E0658]: compact `cfg(target(..))` is experimental and subject to change
  --> $DIR/feature-gate-cfg-target-compact.rs:1:7
   |
LL | #[cfg(target(os = "linux"))]
   |       ^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #96901 <https://github.com/rust-lang/rust/issues/96901> for more information
   = help: add `#![feature(cfg_target_compact)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: compact `cfg(target(..))` is experimental and subject to change
  --> $DIR/feature-gate-cfg-target-compact.rs:4:12
   |
LL | #[cfg_attr(target(os = "linux"), non_exhaustive)]
   |            ^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #96901 <https://github.com/rust-lang/rust/issues/96901> for more information
   = help: add `#![feature(cfg_target_compact)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: compact `cfg(target(..))` is experimental and subject to change
  --> $DIR/feature-gate-cfg-target-compact.rs:7:19
   |
LL | #[cfg(not(any(all(target(os = "linux")))))]
   |                   ^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #96901 <https://github.com/rust-lang/rust/issues/96901> for more information
   = help: add `#![feature(cfg_target_compact)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: compact `cfg(target(..))` is experimental and subject to change
  --> $DIR/feature-gate-cfg-target-compact.rs:11:10
   |
LL |     cfg!(target(os = "linux"));
   |          ^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #96901 <https://github.com/rust-lang/rust/issues/96901> for more information
   = help: add `#![feature(cfg_target_compact)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui_core("feature-gate-cfg-target-compact.rs", "2015", "cfg", source), stderr);
}

#[test]
fn a_gated_cfg_in_cfg_macro_is_gated() {
    // tests/ui/feature-gates/feature-gate-cfg_ub_checks.rs
    let source = r#"#![crate_type = "lib"]

pub fn ub_checks_are_enabled() -> bool {
    cfg!(ub_checks) //~ ERROR `cfg(ub_checks)` is experimental
}
"#;
    let stderr = r#"error[E0658]: `cfg(ub_checks)` is experimental and subject to change
  --> $DIR/feature-gate-cfg_ub_checks.rs:4:10
   |
LL |     cfg!(ub_checks)
   |          ^^^^^^^^^
   |
   = note: see issue #123499 <https://github.com/rust-lang/rust/issues/123499> for more information
   = help: add `#![feature(cfg_ub_checks)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui_core("feature-gate-cfg_ub_checks.rs", "2015", "cfg", source), stderr);
}

#[test]
#[ignore = "needs the parser to take a literal in a cfg predicate's list (`version(\"1.42\")`)"]
fn cfg_version_is_gated() {
    // tests/ui/feature-gates/feature-gate-cfg-version.rs
    let source = r#"//! Feature gate test for `cfg_version`.
//!
//! Tracking issue: #64796.

#[cfg(version("1.42"))]
//~^ ERROR `cfg(version)` is experimental and subject to change
fn bar() {}

fn main() {
    assert!(cfg!(version("1.42")));
    //~^ ERROR `cfg(version)` is experimental and subject to change
}
"#;
    let stderr = r#"error[E0658]: `cfg(version)` is experimental and subject to change
  --> $DIR/feature-gate-cfg-version.rs:5:7
   |
LL | #[cfg(version("1.42"))]
   |       ^^^^^^^^^^^^^^^
   |
   = note: see issue #64796 <https://github.com/rust-lang/rust/issues/64796> for more information
   = help: add `#![feature(cfg_version)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: `cfg(version)` is experimental and subject to change
  --> $DIR/feature-gate-cfg-version.rs:10:18
   |
LL |     assert!(cfg!(version("1.42")));
   |                  ^^^^^^^^^^^^^^^
   |
   = note: see issue #64796 <https://github.com/rust-lang/rust/issues/64796> for more information
   = help: add `#![feature(cfg_version)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui_core("feature-gate-cfg-version.rs", "2015", "assert, cfg", source), stderr);
}

/// Not a UI test of rustc's: what `flat_map_node` parses, by its rules. Nothing in a node `cfg`
/// removes, nor the attributes of a `cfg_attr` whose predicate does not hold, nor a `cfg`
/// after one that does not; the gates of an enabled feature are not given.
#[test]
fn what_cfg_removes_is_not_gated() {
    let fixture = r#"
//- /lib.rs crate:main
#![feature(cfg_ub_checks)]
#[cfg(false)]
mod off {
    #[cfg(overflow_checks)]
    fn f() {}
}
#[cfg_attr(false, cfg(overflow_checks))]
#[cfg(false)]
#[cfg(target_has_threads)]
fn g() {}
#[cfg(ub_checks)]
fn h() {}
#[cfg_attr(true, cfg_attr(sanitize = "address", allow()))]
fn i() {}
"#;
    let messages = crate::tests::checked(fixture).messages();
    let gates: Vec<&String> = messages.iter().filter(|it| it.starts_with("E0658")).collect();
    assert_eq!(gates, ["E0658: `cfg(sanitize)` is experimental and subject to change"]);
}

/// Not a UI test of rustc's: the attributes of a macro call are gated as expansion meets the
/// call (`check_attributes`), with the text of `check_attribute_stability`.
#[test]
fn the_attributes_of_a_macro_call_are_gated() {
    let source = r#"macro_rules! m { () => {} }
#[rustc_dummy]
m!();
fn main() {}
"#;
    let stderr = r#"error[E0658]: use of an internal attribute
  --> $DIR/macro-call-attribute.rs:2:3
   |
LL | #[rustc_dummy]
   |   ^^^^^^^^^^^
   |
   = help: add `#![feature(rustc_attrs)]` to the crate attributes to enable
   = note: the `rustc_dummy` attribute is an internal implementation detail that will never be stable
   = note: the `rustc_dummy` attribute is used for rustc unit tests

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("macro-call-attribute.rs", "2015", source), stderr);
}
