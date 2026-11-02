//! The feature lints (`incomplete_features`, `internal_features`, `unstable_features`) against
//! rustc's UI tests.

use crate::{
    Options,
    emit::Style,
    tests::{LIBRARY_FEATURES, Mode, checked_with, compiletest, library_features, normalized, ui, ui_core_as},
};

/// [`crate::tests::ui`] of the revision of a UI test of edition 2015 that compiletest gives
/// `--cfg <revision>`.
fn ui_revision(name: &str, revision: &str, source: &str) -> String {
    let fixture = format!("//- /{name} crate:other:main edition:2015 cfg:{revision}\n{source}");
    let options = compiletest(Mode::Other, &Options::default());
    normalized(&checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// The messages of the feature lints `source` gets, with their lint names.
fn feature_lints(source: &str) -> Vec<String> {
    let fixture = format!("//- /main.rs crate:other:main edition:2021\n{source}");
    checked_with(&fixture, "", &Options::default())
        .messages()
        .into_iter()
        .filter(|it| ["incomplete_features:", "internal_features:", "unstable_features:"].iter().any(|lint| it.starts_with(lint)))
        .collect()
}

#[test]
fn internal_lang_and_lib_features_are_linted() {
    let source = r#"#![forbid(internal_features)]
// A lang feature and a lib feature.
#![feature(intrinsics, panic_internals)]
//~^ ERROR: internal
//~| ERROR: internal

#[rustc_intrinsic]
unsafe fn copy_nonoverlapping<T>(src: *const T, dst: *mut T, count: usize);

fn main() {}
"#;
    let expected = r#"error: the feature `intrinsics` is internal to the compiler or standard library
  --> $DIR/internal_features.rs:3:12
   |
LL | #![feature(intrinsics, panic_internals)]
   |            ^^^^^^^^^^
   |
   = note: using it is strongly discouraged
note: the lint level is defined here
  --> $DIR/internal_features.rs:1:11
   |
LL | #![forbid(internal_features)]
   |           ^^^^^^^^^^^^^^^^^

error: the feature `panic_internals` is internal to the compiler or standard library
  --> $DIR/internal_features.rs:3:24
   |
LL | #![feature(intrinsics, panic_internals)]
   |                        ^^^^^^^^^^^^^^^
   |
   = note: using it is strongly discouraged

error: aborting due to 2 previous errors

"#;
    let fixture = format!(
        "{}//- /internal_features.rs crate:other:main edition:2015 deps:{LIBRARY_FEATURES}\n{source}",
        library_features(&["panic_internals"])
    );
    let options = compiletest(Mode::Other, &Options::default());
    let stderr = checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    assert_eq!(normalized(&stderr), expected);
}

#[test]
fn an_incomplete_feature_is_linted() {
    let source = r#"//! Make sure that incomplete features emit the `incomplete_features` lint.

// gate-test-test_incomplete_feature

//@ check-pass
//@ revisions: warn expect

#![cfg_attr(warn, warn(incomplete_features))]
#![cfg_attr(expect, expect(incomplete_features))]

#![feature(test_incomplete_feature)] //[warn]~ WARN the feature `test_incomplete_feature` is incomplete

fn main() {}
"#;
    let expected = r#"warning: the feature `test_incomplete_feature` is incomplete and may not be safe to use and/or cause compiler crashes
  --> $DIR/incomplete-features.rs:11:12
   |
LL | #![feature(test_incomplete_feature)]
   |            ^^^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/incomplete-features.rs:8:24
   |
LL | #![cfg_attr(warn, warn(incomplete_features))]
   |                        ^^^^^^^^^^^^^^^^^^^

warning: 1 warning emitted

"#;
    assert_eq!(ui_revision("incomplete-features.rs", "warn", source), expected);
    assert_eq!(ui_revision("incomplete-features.rs", "expect", source), "");
}

#[test]
fn an_incomplete_feature_a_cfg_attr_enables_is_linted_with_its_issue() {
    let source = r#"//@ compile-flags: --crate-type=lib
//@ revisions: with_gate without_gate
//@ [with_gate] check-pass
#![warn(incomplete_features)]
#![cfg_attr(with_gate, feature(unsafe_fields))] //[with_gate]~ WARNING

#[cfg(false)]
struct Foo {
    unsafe field: (), //[without_gate]~ ERROR
}

// This should not parse as an unsafe field definition.
struct FooTuple(unsafe fn());

#[cfg(false)]
enum Bar {
    Variant { unsafe field: () }, //[without_gate]~ ERROR
    // This should not parse as an unsafe field definition.
    VariantTuple(unsafe fn()),
}

#[cfg(false)]
union Baz {
    unsafe field: (), //[without_gate]~ ERROR
}
"#;
    let expected = r#"warning: the feature `unsafe_fields` is incomplete and may not be safe to use and/or cause compiler crashes
  --> $DIR/feature-gate-unsafe_fields.rs:5:32
   |
LL | #![cfg_attr(with_gate, feature(unsafe_fields))]
   |                                ^^^^^^^^^^^^^
   |
   = note: see issue #132922 <https://github.com/rust-lang/rust/issues/132922> for more information
note: the lint level is defined here
  --> $DIR/feature-gate-unsafe_fields.rs:4:9
   |
LL | #![warn(incomplete_features)]
   |         ^^^^^^^^^^^^^^^^^^^

warning: 1 warning emitted

"#;
    assert_eq!(ui_revision("feature-gate-unsafe_fields.rs", "with_gate", source), expected);
}

#[test]
fn an_incomplete_feature_with_a_min_feature_suggests_it() {
    let source = r#"#![warn(incomplete_features)]
#![feature(specialization)]

fn main() {}
"#;
    let expected = r#"warning: the feature `specialization` is incomplete and may not be safe to use and/or cause compiler crashes
  --> $DIR/specialization.rs:2:12
   |
LL | #![feature(specialization)]
   |            ^^^^^^^^^^^^^^
   |
   = note: see issue #31844 <https://github.com/rust-lang/rust/issues/31844> for more information
   = help: consider using `min_specialization` instead, which is more stable and complete
note: the lint level is defined here
  --> $DIR/specialization.rs:1:9
   |
LL | #![warn(incomplete_features)]
   |         ^^^^^^^^^^^^^^^^^^^

warning: 1 warning emitted

"#;
    assert_eq!(ui("specialization.rs", "2015", source), expected);
}

#[test]
fn enabling_a_feature_is_an_unstable_feature() {
    let source = r#"#![forbid(unstable_features)]
#![feature(intrinsics)] //~ ERROR unstable feature

fn main() {}
"#;
    let expected = r#"error: use of an unstable feature
  --> $DIR/feature-gate-feature-gate.rs:2:12
   |
LL | #![feature(intrinsics)]
   |            ^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/feature-gate-feature-gate.rs:1:11
   |
LL | #![forbid(unstable_features)]
   |           ^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    // A core, as rustc has std: a fixture with none takes `#![feature(no_core)]`, which the lint
    // would name too.
    let stderr = ui_core_as(Mode::Other, "feature-gate-feature-gate.rs", "2015", "sized", source, &Options::default());
    assert_eq!(stderr, expected);
}

#[test]
fn language_features_are_linted_before_library_ones() {
    let lints = feature_lints(
        r#"
#![feature(panic_internals, generic_const_exprs, fmt_internals, intrinsics)]
fn main() {}
"#,
    );
    assert_eq!(lints, [
        "incomplete_features: the feature `generic_const_exprs` is incomplete and may not be safe to use and/or cause compiler crashes",
        "internal_features: the feature `intrinsics` is internal to the compiler or standard library",
        "internal_features: the feature `panic_internals` is internal to the compiler or standard library",
        "internal_features: the feature `fmt_internals` is internal to the compiler or standard library",
    ]);
}

#[test]
fn every_feature_attribute_of_the_crate_is_one_list() {
    let lints = feature_lints(
        r#"
#![feature(core_intrinsics)]
#![cfg_attr(all(), feature(lang_items))]
#![cfg_attr(any(), feature(rustc_attrs))]
#![feature(lang_items)]
fn main() {}
"#,
    );
    assert_eq!(lints, [
        "internal_features: the feature `lang_items` is internal to the compiler or standard library",
        "internal_features: the feature `lang_items` is internal to the compiler or standard library",
        "internal_features: the feature `core_intrinsics` is internal to the compiler or standard library",
    ]);
}

#[test]
fn stable_unstable_and_removed_features_are_neither_incomplete_nor_internal() {
    assert_eq!(feature_lints("#![feature(min_specialization, aarch64_target_feature, let_chains)]\nfn main() {}\n"), Vec::<String>::new());
    assert_eq!(feature_lints("#![feature(unsized_locals)]\nfn main() {}\n"), Vec::<String>::new());
}

#[test]
fn only_the_identifiers_of_a_parsed_list_are_features() {
    let lints = feature_lints(
        r#"
#![feature(intrinsics = "x", lang_items(a), core::rustc_attrs, "staged_api", r#prelude_import, custom_mir,)]
fn main() {}
"#,
    );
    assert_eq!(lints, [
        "internal_features: the feature `prelude_import` is internal to the compiler or standard library",
        "internal_features: the feature `custom_mir` is internal to the compiler or standard library",
    ]);
    assert_eq!(feature_lints("#![feature[intrinsics]]\nfn main() {}\n"), Vec::<String>::new());
    assert_eq!(feature_lints("#![feature = \"intrinsics\"]\nfn main() {}\n"), Vec::<String>::new());
    assert_eq!(feature_lints("#![feature(intrinsics, +)]\nfn main() {}\n"), Vec::<String>::new());
}

#[test]
fn a_feature_attribute_below_the_crate_root_is_only_an_unstable_feature() {
    let lints = feature_lints(
        r#"
#![deny(unstable_features)]
#[feature(intrinsics)]
fn f() {}
mod m {
    #![feature(lang_items)]
}
fn main() {}
"#,
    );
    assert_eq!(lints, ["unstable_features: use of an unstable feature", "unstable_features: use of an unstable feature"]);
}
