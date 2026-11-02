//! rustc's UI tests of the errors of the parsers of the compiler's internal attributes
//! (`checks/attrs/parsers/internal.rs`): `tests/ui/error-codes/E0522.rs`, `E0718.rs`,
//! `E0264.rs`, `tests/ui/lang-items/`, `tests/ui/dep-graph/`, `tests/ui/internal-lints/`, the
//! `rustc_*` attributes' tests, and small ones in the same form where no UI test isolates an
//! error.

use crate::{
    Options,
    tests::{Mode, assert_reports, error, ui, ui_fixture_as},
};

#[test]
fn an_unknown_lang_item_is_an_error() {
    // tests/ui/error-codes/E0522.rs
    let source = r#"#![feature(lang_items)]

#[lang = "cookie"]
fn cookie() -> ! {
//~^^ ERROR definition of an unknown lang item: `cookie` [E0522]
    loop {}
}

fn main() {}
"#;
    let stderr = r#"error[E0522]: definition of an unknown lang item: `cookie`
  --> $DIR/E0522.rs:3:1
   |
LL | #[lang = "cookie"]
   | ^^^^^^^^^^^^^^^^^^ definition of unknown lang item `cookie`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0522`.
"#;
    assert_eq!(ui("E0522.rs", "2015", source), stderr);
}

#[test]
fn a_lang_item_on_the_wrong_target_names_the_one_it_takes() {
    // tests/ui/error-codes/E0718.rs
    let source = r#"#![feature(lang_items)]

// Box is expected to be a struct, so this will error.
#[lang = "owned_box"] //~ ERROR the `lang = "owned_box"` attribute cannot be used on statics
static X: u32 = 42;

fn main() {}
"#;
    let stderr = r#"error: the `lang = "owned_box"` attribute cannot be used on statics
  --> $DIR/E0718.rs:4:3
   |
LL | #[lang = "owned_box"]
   |   ^^^^^^^^^^^^^^^^^^
   |
   = help: the `lang = "owned_box"` attribute can only be applied to structs

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("E0718.rs", "2015", source), stderr);
}

#[test]
fn a_lang_item_on_a_foreign_item_must_be_weak() {
    // tests/ui/error-codes/E0264.rs
    let source = r#"#![feature(lang_items)]

extern "C" {
    #[lang = "copy"] //~ ERROR E0264
    fn copy();
}

fn main() {}
"#;
    let stderr = r#"error[E0264]: unknown external lang item: `copy`
  --> $DIR/E0264.rs:4:5
   |
LL |     #[lang = "copy"]
   |     ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0264`.
"#;
    assert_eq!(ui("E0264.rs", "2015", source), stderr);
}

#[test]
fn unknown_lang_items_on_associated_items() {
    // tests/ui/lang-items/assoc-lang-items.rs
    let source = r#"//! Check that associated items can be marked as lang items, so that they don't have to be looked up
//! by name or by definition order indirectly.
//!
//! This test is not *quite* high-fidelity: it checks that you can use lang items on associated
//! items by looking at the error message *as a proxy*. That is, the error message is about
//! undefined lang items and not invalid attribute target, indicating that it has reached lang item
//! machinery (which is relying on knowing the implementation detail). However, it's annoying to
//! write a full-fidelity test for this, so I think this is acceptable even though it's not *great*.
//!
//! This was implemented in <https://github.com/rust-lang/rust/pull/72559> to help with
//! <https://github.com/rust-lang/rust/issues/70718>, which is itself relevant for e.g. `Fn::Output`
//! or `Future::Output` or specific use cases like [Use `T`'s discriminant type in
//! `mem::Discriminant<T>` instead of `u64`](https://github.com/rust-lang/rust/pull/70705).

#![feature(lang_items)]

trait Foo {
    #[lang = "dummy_lang_item_1"] //~ ERROR definition
    fn foo() {}

    #[lang = "dummy_lang_item_2"] //~ ERROR definition
    fn bar();

    #[lang = "dummy_lang_item_3"] //~ ERROR definition
    type MyType;
}

struct Bar;

impl Bar {
    #[lang = "dummy_lang_item_4"] //~ ERROR definition
    fn test() {}
}

fn main() {}
"#;
    let stderr = r#"error[E0522]: definition of an unknown lang item: `dummy_lang_item_1`
  --> $DIR/assoc-lang-items.rs:18:5
   |
LL |     #[lang = "dummy_lang_item_1"]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ definition of unknown lang item `dummy_lang_item_1`

error[E0522]: definition of an unknown lang item: `dummy_lang_item_2`
  --> $DIR/assoc-lang-items.rs:21:5
   |
LL |     #[lang = "dummy_lang_item_2"]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ definition of unknown lang item `dummy_lang_item_2`

error[E0522]: definition of an unknown lang item: `dummy_lang_item_3`
  --> $DIR/assoc-lang-items.rs:24:5
   |
LL |     #[lang = "dummy_lang_item_3"]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ definition of unknown lang item `dummy_lang_item_3`

error[E0522]: definition of an unknown lang item: `dummy_lang_item_4`
  --> $DIR/assoc-lang-items.rs:31:5
   |
LL |     #[lang = "dummy_lang_item_4"]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ definition of unknown lang item `dummy_lang_item_4`

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0522`.
"#;
    assert_eq!(ui("assoc-lang-items.rs", "2015", source), stderr);
}

#[test]
fn an_unknown_lang_item_on_a_function() {
    // tests/ui/lang-items/lang-item-unknown-definition-error.rs
    let source = r#"//! Checks that compiler prevernt attempting to define an unrecognized or unknown lang item

#![allow(unused)]
#![feature(lang_items)]

#[lang = "foo"]
fn bar() -> ! {
    //~^^ ERROR definition of an unknown lang item: `foo`
    loop {}
}

fn main() {}
"#;
    let stderr = r#"error[E0522]: definition of an unknown lang item: `foo`
  --> $DIR/lang-item-unknown-definition-error.rs:6:1
   |
LL | #[lang = "foo"]
   | ^^^^^^^^^^^^^^^ definition of unknown lang item `foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0522`.
"#;
    assert_eq!(ui("lang-item-unknown-definition-error.rs", "2015", source), stderr);
}

#[test]
fn an_unknown_lang_item_is_said_after_the_feature_gate() {
    // tests/ui/feature-gates/feature-gate-lang-items.rs
    // E0658 is `checks::features`'.
    let source = r#"#[lang = "foo"] //~ ERROR lang items are subject to change
                //~^ ERROR definition of an unknown lang item: `foo`
trait Foo {}

fn main() {}
"#;
    let stderr = r#"error[E0658]: lang items are subject to change
  --> $DIR/feature-gate-lang-items.rs:1:3
   |
LL | #[lang = "foo"]
   |   ^^^^
   |
   = help: add `#![feature(lang_items)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0522]: definition of an unknown lang item: `foo`
  --> $DIR/feature-gate-lang-items.rs:1:1
   |
LL | #[lang = "foo"]
   | ^^^^^^^^^^^^^^^ definition of unknown lang item `foo`

error: aborting due to 2 previous errors

Some errors have detailed explanations: E0522, E0658.
For more information about an error, try `rustc --explain E0522`.
"#;
    assert_eq!(ui("feature-gate-lang-items.rs", "2015", source), stderr);
}

#[test]
fn the_incremental_test_attributes_need_query_dep_graph() {
    // tests/ui/dep-graph/dep-graph-check-attr.rs
    let source = r#"// Test that using rustc_clean/dirty/if_this_changed/then_this_would_need
// are forbidden when `-Z query-dep-graph` is not enabled.

#![feature(rustc_attrs)]
#![allow(dead_code)]
#![allow(unused_variables)]

#[rustc_clean(cfg = "foo")] //~ ERROR attribute requires -Z query-dep-graph
fn main() {}

#[rustc_if_this_changed] //~ ERROR attribute requires -Z query-dep-graph
struct Foo<T> {
    f: T,
}

#[rustc_clean(cfg = "foo")] //~ ERROR attribute requires -Z query-dep-graph
type TypeAlias<T> = Foo<T>;

#[rustc_then_this_would_need(variances_of)] //~ ERROR attribute requires -Z query-dep-graph
trait Use<T> {}
"#;
    let stderr = r#"error: attribute requires -Z query-dep-graph to be enabled
  --> $DIR/dep-graph-check-attr.rs:8:1
   |
LL | #[rustc_clean(cfg = "foo")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: attribute requires -Z query-dep-graph to be enabled
  --> $DIR/dep-graph-check-attr.rs:11:1
   |
LL | #[rustc_if_this_changed]
   | ^^^^^^^^^^^^^^^^^^^^^^^^

error: attribute requires -Z query-dep-graph to be enabled
  --> $DIR/dep-graph-check-attr.rs:16:1
   |
LL | #[rustc_clean(cfg = "foo")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: attribute requires -Z query-dep-graph to be enabled
  --> $DIR/dep-graph-check-attr.rs:19:1
   |
LL | #[rustc_then_this_would_need(variances_of)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 4 previous errors

"#;
    assert_eq!(ui("dep-graph-check-attr.rs", "2015", source), stderr);
}

#[test]
fn a_lint_attribute_of_no_arguments_and_its_target() {
    // tests/ui/internal-lints/query_stability_incorrect.rs
    let source = r#"//@ compile-flags: -Z unstable-options

#![feature(rustc_attrs)]

#[rustc_lint_query_instability]
//~^ ERROR the `rustc_lint_query_instability` attribute cannot be used on structs
struct Foo;

impl Foo {
    #[rustc_lint_query_instability(a)]
    //~^ ERROR malformed `rustc_lint_query_instability`
    fn bar() {}
}

fn main() {}
"#;
    let stderr = r#"error: the `rustc_lint_query_instability` attribute cannot be used on structs
  --> $DIR/query_stability_incorrect.rs:5:3
   |
LL | #[rustc_lint_query_instability]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the `rustc_lint_query_instability` attribute can only be applied to functions

error[E0565]: malformed `rustc_lint_query_instability` attribute input
  --> $DIR/query_stability_incorrect.rs:10:7
   |
LL |     #[rustc_lint_query_instability(a)]
   |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^---
   |                                   |
   |                                   didn't expect any arguments here
   |
help: must be of the form
   |
LL -     #[rustc_lint_query_instability(a)]
LL +     #[rustc_lint_query_instability]
   |

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0565`.
"#;
    assert_eq!(ui("query_stability_incorrect.rs", "2015", source), stderr);
}

#[test]
fn must_implement_one_of_takes_two_or_more_names() {
    // tests/ui/traits/default-method/rustc_must_implement_one_of_misuse.rs
    // The errors after the parser's are `check_mod_attrs`' (`checks/attrs/check.rs`), of
    // `Parsed::RustcMustImplementOneOf`.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_must_implement_one_of(a, b)]
//~^ ERROR function not found in this trait
//~| ERROR function not found in this trait
trait Tr0 {}

#[rustc_must_implement_one_of(a, b)]
//~^ ERROR function not found in this trait
trait Tr1 {
    fn a() {}
}

#[rustc_must_implement_one_of(a)]
//~^ ERROR malformed
trait Tr2 {
    fn a() {}
}

#[rustc_must_implement_one_of]
//~^ ERROR malformed `rustc_must_implement_one_of` attribute input
trait Tr3 {}

#[rustc_must_implement_one_of(A, B)]
trait Tr4 {
    const A: u8 = 1; //~ ERROR not a function

    type B; //~ ERROR not a function
}

#[rustc_must_implement_one_of(a, b)]
trait Tr5 {
    fn a(); //~ ERROR function doesn't have a default implementation

    fn b(); //~ ERROR function doesn't have a default implementation
}

#[rustc_must_implement_one_of(abc, xyz)]
//~^ ERROR the `rustc_must_implement_one_of` attribute cannot be used on functions
fn function() {}

#[rustc_must_implement_one_of(abc, xyz)]
//~^ ERROR the `rustc_must_implement_one_of` attribute cannot be used on structs
struct Struct {}

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `rustc_must_implement_one_of` attribute input
  --> $DIR/rustc_must_implement_one_of_misuse.rs:14:3
   |
LL | #[rustc_must_implement_one_of(a)]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^---
   |                              |
   |                              expected 2 or more items
   |
help: must be of the form
   |
LL - #[rustc_must_implement_one_of(a)]
LL + #[rustc_must_implement_one_of(function1, function2, ...)]
   |

error[E0539]: malformed `rustc_must_implement_one_of` attribute input
  --> $DIR/rustc_must_implement_one_of_misuse.rs:20:3
   |
LL | #[rustc_must_implement_one_of]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected this to be a list
   |
help: must be of the form
   |
LL | #[rustc_must_implement_one_of(function1, function2, ...)]
   |                              +++++++++++++++++++++++++++

error: the `rustc_must_implement_one_of` attribute cannot be used on functions
  --> $DIR/rustc_must_implement_one_of_misuse.rs:38:3
   |
LL | #[rustc_must_implement_one_of(abc, xyz)]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the `rustc_must_implement_one_of` attribute can only be applied to traits

error: the `rustc_must_implement_one_of` attribute cannot be used on structs
  --> $DIR/rustc_must_implement_one_of_misuse.rs:42:3
   |
LL | #[rustc_must_implement_one_of(abc, xyz)]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the `rustc_must_implement_one_of` attribute can only be applied to traits

error: function not found in this trait
  --> $DIR/rustc_must_implement_one_of_misuse.rs:3:31
   |
LL | #[rustc_must_implement_one_of(a, b)]
   |                               ^

error: function not found in this trait
  --> $DIR/rustc_must_implement_one_of_misuse.rs:3:34
   |
LL | #[rustc_must_implement_one_of(a, b)]
   |                                  ^

error: function not found in this trait
  --> $DIR/rustc_must_implement_one_of_misuse.rs:8:34
   |
LL | #[rustc_must_implement_one_of(a, b)]
   |                                  ^

error: not a function
  --> $DIR/rustc_must_implement_one_of_misuse.rs:26:5
   |
LL |     const A: u8 = 1;
   |     ^^^^^^^^^^^
   |
note: required by this annotation
  --> $DIR/rustc_must_implement_one_of_misuse.rs:24:1
   |
LL | #[rustc_must_implement_one_of(A, B)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: all `#[rustc_must_implement_one_of]` arguments must be associated function names

error: not a function
  --> $DIR/rustc_must_implement_one_of_misuse.rs:28:5
   |
LL |     type B;
   |     ^^^^^^
   |
note: required by this annotation
  --> $DIR/rustc_must_implement_one_of_misuse.rs:24:1
   |
LL | #[rustc_must_implement_one_of(A, B)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: all `#[rustc_must_implement_one_of]` arguments must be associated function names

error: function doesn't have a default implementation
  --> $DIR/rustc_must_implement_one_of_misuse.rs:33:5
   |
LL |     fn a();
   |     ^^^^^^^
   |
note: required by this annotation
  --> $DIR/rustc_must_implement_one_of_misuse.rs:31:1
   |
LL | #[rustc_must_implement_one_of(a, b)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: function doesn't have a default implementation
  --> $DIR/rustc_must_implement_one_of_misuse.rs:35:5
   |
LL |     fn b();
   |     ^^^^^^^
   |
note: required by this annotation
  --> $DIR/rustc_must_implement_one_of_misuse.rs:31:1
   |
LL | #[rustc_must_implement_one_of(a, b)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 11 previous errors

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("rustc_must_implement_one_of_misuse.rs", "2015", source), stderr);
}

#[test]
fn legacy_const_generics_takes_unsuffixed_integers() {
    // tests/ui/const-generics/invalid-rustc_legacy_const_generics-arguments.rs
    // The errors after the parser's are `check_mod_attrs`' (of `Parsed::RustcLegacyConstGenerics`)
    // and E0044 of the foreign function's const parameter.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_legacy_const_generics(0)] //~ ERROR #[rustc_legacy_const_generics] must have one index for
fn foo1() {}

#[rustc_legacy_const_generics(1)] //~ ERROR index exceeds number of arguments
fn foo2<const X: usize>() {}

#[rustc_legacy_const_generics(2)] //~ ERROR index exceeds number of arguments
fn foo3<const X: usize>(_: u8) {}

#[rustc_legacy_const_generics(a)] //~ ERROR  malformed `rustc_legacy_const_generics` attribute input
fn foo4<const X: usize>() {}

#[rustc_legacy_const_generics(1, a, 2, b)]
//~^ ERROR malformed `rustc_legacy_const_generics` attribute input
//~^^ ERROR malformed `rustc_legacy_const_generics` attribute input
fn foo5<const X: usize, const Y: usize, const Z: usize, const W: usize>() {}

#[rustc_legacy_const_generics(0)] //~ ERROR the `rustc_legacy_const_generics` attribute cannot be used on structs
struct S;

#[rustc_legacy_const_generics(0usize)]
//~^ ERROR suffixed literals are not allowed in attributes
//~^^ ERROR malformed `rustc_legacy_const_generics` attribute input
fn foo6<const X: usize>() {}

extern "C" {
    #[rustc_legacy_const_generics(1)] //~ ERROR the `rustc_legacy_const_generics` attribute cannot be used on foreign functions
    fn foo7<const X: usize>(); //~ ERROR foreign items may not have const parameters
}

#[rustc_legacy_const_generics(0)] //~ ERROR #[rustc_legacy_const_generics] functions must only have
fn foo8<X>() {}

impl S {
    #[rustc_legacy_const_generics(0)] //~ ERROR the `rustc_legacy_const_generics` attribute cannot be used on inherent methods
    fn foo9<const X: usize>() {}
}

#[rustc_legacy_const_generics] //~ ERROR malformed `rustc_legacy_const_generics` attribute input
fn bar1() {}

#[rustc_legacy_const_generics = 1] //~ ERROR malformed `rustc_legacy_const_generics` attribute input
fn bar2() {}

fn main() {}
"#;
    assert_reports(
        &ui("invalid-rustc_legacy_const_generics-arguments.rs", "2015", source),
        &[
            error(12, &["`rustc_legacy_const_generics`"]),
            error(15, &["`rustc_legacy_const_generics`"]),
            error(15, &["`rustc_legacy_const_generics`"]),
            error(20, &["`rustc_legacy_const_generics`"]),
            error(23, &[]),
            error(23, &["`rustc_legacy_const_generics`"]),
            error(29, &["`rustc_legacy_const_generics`"]),
            error(37, &["`rustc_legacy_const_generics`"]),
            error(41, &["`rustc_legacy_const_generics`"]),
            error(44, &["`rustc_legacy_const_generics`"]),
            error(3, &[]),
            error(6, &[]),
            error(9, &[]),
            error(33, &[]),
            error(30, &[]),
        ],
    );
}

#[test]
fn never_type_options_stops_at_an_ill_formed_list() {
    // tests/ui/attributes/malformed-never-type-options.rs
    // E0658 is `checks::features`', the literal error the arguments' (`checks/attrs/args.rs`).
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
fn never_type_options_takes_each_key_once() {
    // Written in the form of a UI test: none isolates a second `fallback`.
    let source = r#"#![feature(rustc_attrs)]
#![rustc_never_type_options(fallback = "unit", fallback = "no")]
//~^ ERROR found `fallback` used as a key more than once

fn main() {}
"#;
    let stderr = r#"error[E0538]: malformed `rustc_never_type_options` attribute input
  --> $DIR/never-type-options-duplicate.rs:2:4
   |
LL | #![rustc_never_type_options(fallback = "unit", fallback = "no")]
   |    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^--------^^^^^^^^
   |                                                |
   |                                                found `fallback` used as a key more than once
   |
help: try changing it to one of the following valid forms of the attribute
   |
LL - #![rustc_never_type_options(fallback = "unit", fallback = "no")]
LL + #![rustc_never_type_options(diverging_block_default = "unit", "never")]
   |
LL - #![rustc_never_type_options(fallback = "unit", fallback = "no")]
LL + #![rustc_never_type_options(fallback = "unit", "never", "no")]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0538`.
"#;
    assert_eq!(ui("never-type-options-duplicate.rs", "2015", source), stderr);
}

#[test]
fn never_type_options_names_the_diverging_defaults_as_rustc_does() {
    // Written in the form of a UI test: none isolates `diverging_block_default = "no"`, which
    // rustc refuses naming "no" among the valid ones.
    let source = r#"#![feature(rustc_attrs)]
#![rustc_never_type_options(diverging_block_default = "no")]
//~^ ERROR malformed `rustc_never_type_options` attribute input

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `rustc_never_type_options` attribute input
  --> $DIR/never-type-options-diverging-default.rs:2:4
   |
LL | #![rustc_never_type_options(diverging_block_default = "no")]
   |    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^----^
   |                                                       |
   |                                                       valid arguments are "unit" or "no"
   |
help: try changing it to one of the following valid forms of the attribute
   |
LL - #![rustc_never_type_options(diverging_block_default = "no")]
LL + #![rustc_never_type_options(diverging_block_default = "unit", "never")]
   |
LL - #![rustc_never_type_options(diverging_block_default = "no")]
LL + #![rustc_never_type_options(fallback = "unit", "never", "no")]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("never-type-options-diverging-default.rs", "2015", source), stderr);
}

#[test]
fn a_cgu_test_attribute_needs_cfg() {
    // Written in the form of a UI test: none isolates `rustc_partition_reused` without `cfg`.
    let source = r#"#![feature(rustc_attrs)]
#![rustc_partition_reused(module = "m")]
//~^ ERROR `#[rustc_partition_reused)]` is missing a `cfg` argument

fn main() {}
"#;
    let stderr = r#"error: `#[rustc_partition_reused)]` is missing a `cfg` argument
  --> $DIR/cgu-fields-missing.rs:2:26
   |
LL | #![rustc_partition_reused(module = "m")]
   |                          ^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("cgu-fields-missing.rs", "2015", source), stderr);
}

#[test]
fn expected_cgu_reuse_needs_a_kind() {
    // Written in the form of a UI test: none isolates `rustc_expected_cgu_reuse` without `kind`.
    let source = r#"#![feature(rustc_attrs)]
#![rustc_expected_cgu_reuse(cfg = "rpass2", module = "m")]
//~^ ERROR `#[rustc_expected_cgu_reuse)]` is missing a `kind` argument

fn main() {}
"#;
    let stderr = r#"error: `#[rustc_expected_cgu_reuse)]` is missing a `kind` argument
  --> $DIR/cgu-kind-missing.rs:2:28
   |
LL | #![rustc_expected_cgu_reuse(cfg = "rpass2", module = "m")]
   |                            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("cgu-kind-missing.rs", "2015", source), stderr);
}

#[test]
fn rustc_clean_needs_cfg() {
    // Written in the form of a UI test: none isolates `rustc_clean` without `cfg`.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_clean(except = "typeck")]
//~^ ERROR attribute requires -Z query-dep-graph
//~| ERROR malformed `rustc_clean` attribute input
fn f() {}

fn main() {}
"#;
    let stderr = r#"error: attribute requires -Z query-dep-graph to be enabled
  --> $DIR/rustc-clean-without-cfg.rs:3:1
   |
LL | #[rustc_clean(except = "typeck")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0539]: malformed `rustc_clean` attribute input
  --> $DIR/rustc-clean-without-cfg.rs:3:3
   |
LL | #[rustc_clean(except = "typeck")]
   |   ^^^^^^^^^^^-------------------
   |              |
   |              the only valid argument here is `cfg`
   |
help: must be of the form
   |
LL - #[rustc_clean(except = "typeck")]
LL + #[rustc_clean(cfg = "...", /*opt*/ label = "...", /*opt*/ except = "...")]
   |

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("rustc-clean-without-cfg.rs", "2015", source), stderr);
}

#[test]
fn must_implement_one_of_takes_names_of_one_segment() {
    // Written in the form of a UI test: none isolates a path of two segments among the names.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_must_implement_one_of(a::b, c)]
//~^ ERROR must be a name of an associated function
trait Tr {
    fn c() {}
}

fn main() {}
"#;
    let stderr = r#"error: must be a name of an associated function
  --> $DIR/must-implement-one-of-path.rs:3:31
   |
LL | #[rustc_must_implement_one_of(a::b, c)]
   |                               ^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("must-implement-one-of-path.rs", "2015", source), stderr);
}

#[test]
fn legacy_const_generics_needs_an_index() {
    // Written in the form of a UI test: none isolates an empty list.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_legacy_const_generics()]
//~^ ERROR malformed `rustc_legacy_const_generics` attribute input
fn f<const N: usize>() {}

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `rustc_legacy_const_generics` attribute input
  --> $DIR/legacy-const-generics-empty.rs:3:3
   |
LL | #[rustc_legacy_const_generics()]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^--
   |                              |
   |                              expected at least 1 argument here
   |
help: must be of the form
   |
LL | #[rustc_legacy_const_generics(N)]
   |                               +

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("legacy-const-generics-empty.rs", "2015", source), stderr);
}

#[test]
fn rustc_mir_takes_the_two_phase_graphviz_format() {
    // Written in the form of a UI test: none isolates a `borrowck_graphviz_format` rustc does
    // not know.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_mir(borrowck_graphviz_format = "gv")]
//~^ ERROR malformed `rustc_mir` attribute input
fn f() {}

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `rustc_mir` attribute input
  --> $DIR/rustc-mir-graphviz-format.rs:3:3
   |
LL | #[rustc_mir(borrowck_graphviz_format = "gv")]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^----^
   |                                        |
   |                                        the only valid argument here is `two_phase`
   |
help: must be of the form
   |
LL - #[rustc_mir(borrowck_graphviz_format = "gv")]
LL + #[rustc_mir(arg1, arg2, ...)]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("rustc-mir-graphviz-format.rs", "2015", source), stderr);
}

#[test]
fn deprecated_safe_2024_takes_audit_that() {
    // Written in the form of a UI test: none isolates a key other than `audit_that`.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_deprecated_safe_2024(audit = "x")]
//~^ ERROR malformed `rustc_deprecated_safe_2024` attribute input
fn f() {}

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `rustc_deprecated_safe_2024` attribute input
  --> $DIR/deprecated-safe-2024-key.rs:3:3
   |
LL | #[rustc_deprecated_safe_2024(audit = "x")]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^-----^^^^^^^
   |                              |
   |                              the only valid argument here is `audit_that`
   |
help: must be of the form
   |
LL - #[rustc_deprecated_safe_2024(audit = "x")]
LL + #[rustc_deprecated_safe_2024(audit_that = "...")]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("deprecated-safe-2024-key.rs", "2015", source), stderr);
}

#[test]
fn scalable_vector_takes_one_count_that_fits_a_u16() {
    // Written in the form of a UI test: none isolates the malformed attributes of
    // `tests/ui/scalable-vectors/invalid.rs` (which needs aarch64 and std); its text is theirs.
    let source = r#"#![feature(rustc_attrs)]

#[rustc_scalable_vector("4")]
//~^ ERROR: malformed `rustc_scalable_vector` attribute input
struct ArgNotLit(f32);

#[rustc_scalable_vector(4, 2)]
//~^ ERROR: malformed `rustc_scalable_vector` attribute input
struct ArgMultipleLits(f32);

#[rustc_scalable_vector(count = "4")]
//~^ ERROR: malformed `rustc_scalable_vector` attribute input
struct ArgKind(f32);

#[rustc_scalable_vector(65536)]
//~^ ERROR: element count in `rustc_scalable_vector` is too large: `65536`
struct CountTooLarge(f32);

fn main() {}
"#;
    let fixture = format!("//- /scalable-vector-count.rs crate:other:main edition:2015 cfg:target_arch=aarch64\n{source}");
    assert_reports(
        &ui_fixture_as(Mode::Other, &fixture, &Options::default()),
        &[
            error(3, &["`rustc_scalable_vector`"]),
            error(7, &["`rustc_scalable_vector`"]),
            error(11, &["`rustc_scalable_vector`"]),
            error(15, &["`65536`", "`rustc_scalable_vector`"]),
        ],
    );
}
