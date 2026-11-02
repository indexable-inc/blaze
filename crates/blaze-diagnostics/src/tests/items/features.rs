//! rustc's UI tests of the crate's features: `#![feature]`'s own errors (E0554, E0557, E0635,
//! E0711), `stable_features`, `duplicate_features`, the feature an `implied_by` names; the feature
//! gates of syntax (`gates`) and of attributes (`attributes`). A test whose crate uses std's
//! library features has a stand-in std ([`STD`]) with the stability attributes it reads.

mod attributes;
mod gates;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, compiletest},
};

/// [`crate::tests::ui`], as rustc's UI tests are.
fn ui(name: &str, edition: &str, source: &str) -> String {
    crate::tests::ui(name, edition, source)
}

/// A UI test `name` with minicore's `flags` as its core, checked with compiletest's flags.
fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    ui_fixture(&format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}"))
}

/// The `.stderr` of a fixture whose workspace crate is the UI test and whose `library` crates are
/// what it builds with (an `aux-build`, [`STD`]), checked with compiletest's flags.
fn ui_fixture(fixture: &str) -> String {
    let options = compiletest(Mode::Other, &Options::default());
    let checked = crate::tests::checked_with(fixture, "$DIR/", &options);
    crate::tests::normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// A UI test `name` with [`STD`] as its std.
fn ui_std(name: &str, edition: &str, source: &str) -> String {
    ui_fixture(&format!("//- /{name} crate:other:main deps:std edition:{edition}\n{source}{STD}"))
}

/// The stability attributes of std (and the core it re-exports) the tests read: `rust1`, stable
/// since 1.0.0; `core_intrinsics`, unstable; `try_trait_v2`, once `try_trait`; the removed
/// `concat_idents`; and the `Vec` of the prelude.
const STD: &str = r#"
//- /std.rs crate:std library
#![feature(staged_api)]
#![stable(feature = "rust1", since = "1.0.0")]
#![unstable_removed(
    feature = "concat_idents",
    reason = "Replaced by the macro_metavar_expr_concat feature",
    link = "https://github.com/rust-lang/rust/issues/29599#issuecomment-2986866250",
    since = "1.90.0"
)]

#[stable(feature = "rust1", since = "1.0.0")]
pub mod vec {
    #[stable(feature = "rust1", since = "1.0.0")]
    pub struct Vec<T>(T);

    impl<T> Vec<T> {
        #[stable(feature = "rust1", since = "1.0.0")]
        pub const fn new() -> Vec<T> {
            loop {}
        }
    }
}

#[stable(feature = "rust1", since = "1.0.0")]
pub mod intrinsics {
    #[unstable(feature = "core_intrinsics", issue = "none")]
    pub fn abort() -> ! {
        loop {}
    }
}

#[stable(feature = "rust1", since = "1.0.0")]
pub mod ops {
    #[unstable(feature = "try_trait_v2", issue = "84277", old_name = "try_trait")]
    pub trait Try {}
}

#[stable(feature = "rust1", since = "1.0.0")]
pub mod prelude {
    #[stable(feature = "rust1", since = "1.0.0")]
    pub mod rust_2015 {
        #[stable(feature = "rust1", since = "1.0.0")]
        pub use crate::vec::Vec;
    }
}
"#;

#[test]
fn feature_off_the_nightly_channel_is_e0554() {
    // compiler/rustc_error_codes/src/error_codes/E0554.md, with `RUSTC_BOOTSTRAP=-1` making the
    // compiler a stable one (tests/ui/bootstrap/rustc_bootstrap.rs)
    let fixture = "//- /E0554.rs crate:other:main edition:2015 env:RUSTC_BOOTSTRAP=-1
#![feature(lang_items)]

fn main() {}
";
    let stderr = r#"error[E0554]: `#![feature]` may not be used on the nightly release channel
  --> $DIR/E0554.rs:1:1
   |
LL | #![feature(lang_items)]
   | ^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0554`.
"#;
    assert_eq!(ui_fixture(fixture), stderr);
}

#[test]
fn a_bootstrap_naming_the_crate_or_anything_but_minus_one_keeps_nightly() {
    // tests/ui/bootstrap/rustc_bootstrap.rs, revisions cheat, cheat_single_crate, invalid_zero
    // and invalid_junk: check-pass (a fixture's `env:` takes no `,` in a value: the list of
    // crates is `nightly_build`'s unit test)
    for value in ["1", "main", "0", "*"] {
        let fixture = format!(
            "//- /rustc_bootstrap.rs crate:other:main edition:2015 env:RUSTC_BOOTSTRAP={value}
#![crate_type = \"lib\"]
#![feature(rustc_attrs)]
"
        );
        assert_eq!(ui_fixture(&fixture), "", "RUSTC_BOOTSTRAP={value}");
    }
}

#[test]
fn a_removed_or_unknown_feature_and_malformed_feature_attributes() {
    // tests/ui/feature-gates/gated-bad-feature.rs
    let source = r#"#![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
//~^ ERROR malformed `feature`
//~| ERROR malformed `feature`
//~| ERROR unknown feature `foo`
//~| ERROR unknown feature `foo_bar_baz`
#![feature] //~ ERROR malformed `feature` attribute
#![feature = "foo"] //~ ERROR malformed `feature` attribute
#![feature(test_removed_feature)] //~ ERROR: feature has been removed

fn main() {}
"#;
    let stderr = r#"error[E0557]: feature has been removed
  --> $DIR/gated-bad-feature.rs:8:12
   |
LL | #![feature(test_removed_feature)]
   |            ^^^^^^^^^^^^^^^^^^^^ feature has been removed
   |
   = note: removed in 1.0.0

error[E0565]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:1:4
   |
LL | #![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
   |    ^^^^^^^^^^^^^^^^^^^^^^^^-----^^^^^^^^^^^^^^^^^^^
   |                            |
   |                            didn't expect any arguments here
   |
help: must be of the form
   |
LL - #![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
LL + #![feature(feature1, feature2, ...)]
   |

error[E0565]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:1:4
   |
LL | #![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
   |    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^-------^^^^^^
   |                                       |
   |                                       didn't expect any arguments here
   |
help: must be of the form
   |
LL - #![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
LL + #![feature(feature1, feature2, ...)]
   |

error[E0539]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:6:4
   |
LL | #![feature]
   |    ^^^^^^^ expected this to be a list
   |
help: must be of the form
   |
LL | #![feature(feature1, feature2, ...)]
   |           +++++++++++++++++++++++++

error[E0539]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:7:4
   |
LL | #![feature = "foo"]
   |    ^^^^^^^^-------
   |            |
   |            expected this to be a list
   |
help: must be of the form
   |
LL - #![feature = "foo"]
LL + #![feature(feature1, feature2, ...)]
   |

error[E0635]: unknown feature `foo_bar_baz`
  --> $DIR/gated-bad-feature.rs:1:12
   |
LL | #![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
   |            ^^^^^^^^^^^

error[E0635]: unknown feature `foo`
  --> $DIR/gated-bad-feature.rs:1:48
   |
LL | #![feature(foo_bar_baz, foo(bar), foo = "baz", foo)]
   |                                                ^^^

error: aborting due to 7 previous errors

Some errors have detailed explanations: E0539, E0557, E0565, E0635.
For more information about an error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui_std("gated-bad-feature.rs", "2015", source), stderr);
}

#[test]
fn a_removed_language_feature_notes_its_version_pull_and_reason() {
    // tests/ui/feature-gates/removed-features-note-version-and-pr-issue-141619.rs
    let source = r#"#![deny(invalid_doc_attributes)]
#![feature(external_doc)] //~ ERROR feature has been removed
#![doc(include("README.md"))] //~ ERROR unknown `doc` attribute `include`

fn main(){}
"#;
    let stderr = r#"error[E0557]: feature has been removed
  --> $DIR/removed-features-note-version-and-pr-issue-141619.rs:2:12
   |
LL | #![feature(external_doc)]
   |            ^^^^^^^^^^^^ feature has been removed
   |
   = note: removed in 1.54.0; see <https://github.com/rust-lang/rust/pull/85457> for more information
   = note: use #[doc = include_str!("filename")] instead, which handles macro invocations

error: unknown `doc` attribute `include`
  --> $DIR/removed-features-note-version-and-pr-issue-141619.rs:3:8
   |
LL | #![doc(include("README.md"))]
   |        ^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/removed-features-note-version-and-pr-issue-141619.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0557`.
"#;
    assert_eq!(ui("removed-features-note-version-and-pr-issue-141619.rs", "2015", source), stderr);
}

#[test]
fn an_unknown_feature_suggests_a_language_or_library_feature_of_a_similar_name() {
    // tests/ui/feature-gates/unknown-feature.rs
    let source = r#"#![feature(
    unknown_rust_feature,
    //~^ ERROR unknown feature

    // Typo for lang feature
    associated_types_default,
    //~^ ERROR unknown feature
    //~| HELP there is a feature with a similar name

    // Typo for lib feature
    core_intrnisics,
    //~^ ERROR unknown feature
    //~| HELP there is a feature with a similar name
)]

fn main() {}
"#;
    let stderr = r#"error[E0635]: unknown feature `unknown_rust_feature`
  --> $DIR/unknown-feature.rs:2:5
   |
LL |     unknown_rust_feature,
   |     ^^^^^^^^^^^^^^^^^^^^

error[E0635]: unknown feature `associated_types_default`
  --> $DIR/unknown-feature.rs:6:5
   |
LL |     associated_types_default,
   |     ^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: there is a feature with a similar name: `associated_type_defaults`
   |
LL -     associated_types_default,
LL +     associated_type_defaults,
   |

error[E0635]: unknown feature `core_intrnisics`
  --> $DIR/unknown-feature.rs:11:5
   |
LL |     core_intrnisics,
   |     ^^^^^^^^^^^^^^^
   |
help: there is a feature with a similar name: `core_intrinsics`
   |
LL -     core_intrnisics,
LL +     core_intrinsics,
   |

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0635`.
"#;
    assert_eq!(ui_std("unknown-feature.rs", "2015", source), stderr);
}

#[test]
fn a_renamed_library_feature_names_its_new_name() {
    // tests/ui/stability-attribute/renamed_feature.rs
    let source = "#![feature(try_trait)] //~ ERROR feature `try_trait` has been renamed to `try_trait_v2` [E0635]

fn main() {}
";
    let stderr = r#"error[E0635]: feature `try_trait` has been renamed to `try_trait_v2`
  --> $DIR/renamed_feature.rs:1:12
   |
LL | #![feature(try_trait)]
   |            ^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0635`.
"#;
    assert_eq!(ui_std("renamed_feature.rs", "2015", source), stderr);
}

#[test]
fn a_library_feature_a_dependency_removed_is_e0557() {
    // tests/ui/attributes/unstable_removed.rs, with
    // tests/ui/attributes/auxiliary/unstable_removed_feature.rs
    let fixture = r#"//- /unstable_removed.rs crate:other:main deps:std,unstable_removed_feature edition:2015
//@ aux-build:unstable_removed_feature.rs

#![feature(old_feature)]
//~^ ERROR: feature `old_feature` has been removed

#![feature(concat_idents)]
//~^ ERROR: feature `concat_idents` has been removed

#![unstable_removed(
//~^ ERROR: stability attributes may not be used outside of the standard library
    feature = "old_feature",
    reason = "a good one",
    link = "https://github.com/rust-lang/rust/issues/141617",
    since="1.92.0"
)]

extern crate unstable_removed_feature;

fn main() {}
//- /unstable_removed_feature.rs crate:unstable_removed_feature library
#![feature(staged_api)]
#![stable(feature = "unstable_removed_test", since = "1.0.0")]

#![unstable_removed(
    feature="old_feature",
    reason="deprecated",
    link="https://github.com/rust-lang/rust/issues/141617",
    since="1.92.0"
)]
"#;
    let stderr = r#"error[E0658]: stability attributes may not be used outside of the standard library
  --> $DIR/unstable_removed.rs:9:4
   |
LL | #![unstable_removed(
   |    ^^^^^^^^^^^^^^^^

error[E0557]: feature `old_feature` has been removed
  --> $DIR/unstable_removed.rs:3:12
   |
LL | #![feature(old_feature)]
   |            ^^^^^^^^^^^ feature has been removed
   |
   = note: removed in 1.92.0; see <https://github.com/rust-lang/rust/issues/141617> for more information
   = note: deprecated

error[E0557]: feature `concat_idents` has been removed
  --> $DIR/unstable_removed.rs:6:12
   |
LL | #![feature(concat_idents)]
   |            ^^^^^^^^^^^^^ feature has been removed
   |
   = note: removed in 1.90.0; see <https://github.com/rust-lang/rust/issues/29599#issuecomment-2986866250> for more information
   = note: Replaced by the macro_metavar_expr_concat feature

error: aborting due to 3 previous errors

Some errors have detailed explanations: E0557, E0658.
For more information about an error, try `rustc --explain E0557`.
"#;
    assert_eq!(ui_fixture(&format!("{fixture}{STD}")), stderr);
}

#[test]
fn stable_language_and_library_features_are_linted() {
    // tests/ui/feature-gates/stable-features.rs
    let source = r#"// Testing that the stable_features lint catches use of stable
// language and lib features.

#![deny(stable_features)]

#![feature(test_accepted_feature)]
//~^ ERROR the feature `test_accepted_feature` has been stable since 1.0.0

#![feature(rust1)]
//~^ ERROR the feature `rust1` has been stable since 1.0.0

fn main() {
    let _foo: Vec<()> = Vec::new();
}
"#;
    let stderr = r#"error: the feature `test_accepted_feature` has been stable since 1.0.0 and no longer requires an attribute to enable
  --> $DIR/stable-features.rs:6:12
   |
LL | #![feature(test_accepted_feature)]
   |            ^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/stable-features.rs:4:9
   |
LL | #![deny(stable_features)]
   |         ^^^^^^^^^^^^^^^

error: the feature `rust1` has been stable since 1.0.0 and no longer requires an attribute to enable
  --> $DIR/stable-features.rs:9:12
   |
LL | #![feature(rust1)]
   |            ^^^^^

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui_std("stable-features.rs", "2015", source), stderr);
}

#[test]
fn a_feature_enabled_twice_is_linted_language_features_first() {
    // tests/ui/feature-gates/duplicate-features.rs
    let source = r#"#![allow(stable_features)]

#![feature(rust1)]
#![feature(rust1)] //~ ERROR the feature `rust1` has already been enabled

#![feature(if_let)]
#![feature(if_let)] //~ ERROR the feature `if_let` has already been enabled

fn main() {}
"#;
    let stderr = r#"error: the feature `if_let` has already been enabled
  --> $DIR/duplicate-features.rs:7:12
   |
LL | #![feature(if_let)]
   |            ^^^^^^
   |
   = note: `#[deny(duplicate_features)]` on by default

error: the feature `rust1` has already been enabled
  --> $DIR/duplicate-features.rs:4:12
   |
LL | #![feature(rust1)]
   |            ^^^^^

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui_std("duplicate-features.rs", "2015", source), stderr);
}

/// tests/ui/stability-attribute/auxiliary/stability-attribute-implies.rs
const IMPLIES: &str = r#"
//- /stability_attribute_implies.rs crate:stability_attribute_implies library
#![feature(staged_api)]
#![stable(feature = "stability_attribute_implies", since = "1.0.0")]

#[stable(feature = "foo", since = "1.62.0")]
pub fn foo() {}

#[unstable(feature = "foobar", issue = "1", implied_by = "foo")]
pub fn foobar() {}
"#;

#[test]
fn a_partially_stabilized_feature_suggests_its_successor_or_removing_it() {
    // tests/ui/stability-attribute/stability-attribute-implies-using-stable.rs
    let fixture = r#"//- /stability-attribute-implies-using-stable.rs crate:other:main deps:stability_attribute_implies edition:2015
//@ aux-build:stability-attribute-implies.rs
#![deny(stable_features)]
#![feature(foo)]
//~^ ERROR the feature `foo` has been partially stabilized since 1.62.0 and is succeeded by the feature `foobar`

// Tests that the use of `implied_by` in the `#[unstable]` attribute results in a diagnostic
// mentioning partial stabilization, and that given the implied unstable feature is unused (there
// is no `foobar` call), that the compiler suggests removing the flag.

extern crate stability_attribute_implies;
use stability_attribute_implies::foo;

fn main() {
    foo();
}
"#;
    let stderr = r#"error: the feature `foo` has been partially stabilized since 1.62.0 and is succeeded by the feature `foobar`
  --> $DIR/stability-attribute-implies-using-stable.rs:3:12
   |
LL | #![feature(foo)]
   |            ^^^
   |
note: the lint level is defined here
  --> $DIR/stability-attribute-implies-using-stable.rs:2:9
   |
LL | #![deny(stable_features)]
   |         ^^^^^^^^^^^^^^^
help: if you are using features which are still unstable, change to using `foobar`
   |
LL | #![feature(foobar)]
   |               +++
help: if you are using features which are now stable, remove this line
   |
LL - #![feature(foo)]
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui_fixture(&format!("{fixture}{IMPLIES}")), stderr);
}

#[test]
fn an_implied_by_naming_no_feature_is_an_error() {
    // tests/ui/stability-attribute/stability-attribute-implies-missing.rs
    let source = r#"#![feature(staged_api)]
#![stable(feature = "stability_attribute_implies", since = "1.0.0")]

// Tests that `implied_by = "bar"` results in an error being emitted if `bar` does not exist.

#[unstable(feature = "foobar", issue = "1", implied_by = "bar")]
//~^ ERROR feature `bar` implying `foobar` does not exist
pub fn foobar() {}

fn main() {}
"#;
    let stderr = r#"error: feature `bar` implying `foobar` does not exist
  --> $DIR/stability-attribute-implies-missing.rs:6:1
   |
LL | #[unstable(feature = "foobar", issue = "1", implied_by = "bar")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("stability-attribute-implies-missing.rs", "2015", source), stderr);
}

#[test]
fn a_feature_declared_stable_twice_or_stable_and_unstable_is_e0711() {
    // tests/ui/feature-gates/stability-attribute-consistency.rs
    let source = r#"#![stable(feature = "stable_test_feature", since = "1.0.0")]

#![feature(staged_api)]

#[stable(feature = "foo", since = "1.0.0")]
fn foo_stable_1_0_0() {}

#[stable(feature = "foo", since = "1.29.0")]
//~^ ERROR feature `foo` is declared stable since 1.29.0
fn foo_stable_1_29_0() {}

#[unstable(feature = "foo", issue = "none")]
//~^ ERROR feature `foo` is declared unstable
fn foo_unstable() {}

fn main() {}
"#;
    let stderr = r#"error[E0711]: feature `foo` is declared stable since 1.29.0, but was previously declared stable since 1.0.0
  --> $DIR/stability-attribute-consistency.rs:8:1
   |
LL | #[stable(feature = "foo", since = "1.29.0")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0711]: feature `foo` is declared unstable, but was previously declared stable
  --> $DIR/stability-attribute-consistency.rs:12:1
   |
LL | #[unstable(feature = "foo", issue = "none")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0711`.
"#;
    assert_eq!(ui("stability-attribute-consistency.rs", "2015", source), stderr);
}
