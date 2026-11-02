//! The lints of the `#[diagnostic::..]` attributes (`lints::syntax::attrs::diagnostic`)
//! against rustc's UI tests (`tests/ui/diagnostic_namespace`), sources and `.stderr` verbatim.
//! Of a test whose later errors are other passes' (trait solving, type checking, borrow
//! checking), the `.stderr` before them.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked_with, compiletest, normalized, ui},
};

/// [`ui`] of a UI test that needs core's items: core is minicore with `flags`
/// (`crates/test-utils/src/minicore.rs`).
fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}");
    let options = compiletest(Mode::Other, &Options::default());
    normalized(&checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// Whether `stderr` starts with `expected`, the `.stderr` of a UI test up to its first error
/// of a later pass.
fn assert_starts_with(stderr: &str, expected: &str) {
    assert_eq!(stderr.get(..expected.len()).unwrap_or(stderr), expected);
}

/// `tests/ui/diagnostic_namespace/deny_malformed_attribute.rs`.
#[test]
fn unknown_attribute_denied_by_the_group() {
    let source = r#"//@ reference: attributes.diagnostic.namespace.unknown-invalid-syntax

#![deny(unknown_or_malformed_diagnostic_attributes)]

#[diagnostic::unknown_attribute]
//~^ERROR unknown diagnostic attribute
struct Foo;

fn main() {}
"#;
    let expected = r#"error: unknown diagnostic attribute
  --> $DIR/deny_malformed_attribute.rs:5:15
   |
LL | #[diagnostic::unknown_attribute]
   |               ^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/deny_malformed_attribute.rs:3:9
   |
LL | #![deny(unknown_or_malformed_diagnostic_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(unknown_diagnostic_attributes)]` implied by `#[deny(unknown_or_malformed_diagnostic_attributes)]`

error: aborting due to 1 previous error

"#;
    let stderr = ui("deny_malformed_attribute.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/non_existing_attributes_accepted.rs`.
#[test]
fn unknown_attributes_are_warned_of() {
    let source = r#"//@ check-pass
//@ reference: attributes.diagnostic.namespace.unknown-invalid-syntax
#[diagnostic::non_existing_attribute]
//~^WARN unknown diagnostic attribute
pub trait Bar {
}

#[diagnostic::non_existing_attribute(with_option = "foo")]
//~^WARN unknown diagnostic attribute
struct Foo;

fn main() {
}
"#;
    let expected = r#"warning: unknown diagnostic attribute
  --> $DIR/non_existing_attributes_accepted.rs:3:15
   |
LL | #[diagnostic::non_existing_attribute]
   |               ^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: `#[warn(unknown_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: unknown diagnostic attribute
  --> $DIR/non_existing_attributes_accepted.rs:8:15
   |
LL | #[diagnostic::non_existing_attribute(with_option = "foo")]
   |               ^^^^^^^^^^^^^^^^^^^^^^

warning: 2 warnings emitted

"#;
    let stderr = ui("non_existing_attributes_accepted.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/suggest_typos.rs`.
#[test]
fn unknown_attributes_suggest_a_known_one() {
    let source = r#"//@ reference: attributes.diagnostic.namespace.unknown-invalid-syntax
#![deny(unknown_diagnostic_attributes)]

#[diagnostic::onunimplemented]
//~^ERROR unknown diagnostic attribute
//~^^HELP an attribute with a similar name exists
trait X{}

#[diagnostic::un_onimplemented]
//~^ERROR unknown diagnostic attribute
//~^^HELP an attribute with a similar name exists
trait Y{}

#[diagnostic::on_implemented]
//~^ERROR unknown diagnostic attribute
//~^^HELP an attribute with a similar name exists
trait Z{}

#[diagnostic::dont_recommend]
//~^ERROR unknown diagnostic attribute
//~^^HELP an attribute with a similar name exists
impl X for u8 {}

fn main(){}
"#;
    let expected = r#"error: unknown diagnostic attribute
  --> $DIR/suggest_typos.rs:4:15
   |
LL | #[diagnostic::onunimplemented]
   |               ^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/suggest_typos.rs:2:9
   |
LL | #![deny(unknown_diagnostic_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
help: an attribute with a similar name exists
   |
LL | #[diagnostic::on_unimplemented]
   |                 +

error: unknown diagnostic attribute
  --> $DIR/suggest_typos.rs:9:15
   |
LL | #[diagnostic::un_onimplemented]
   |               ^^^^^^^^^^^^^^^^
   |
help: an attribute with a similar name exists
   |
LL - #[diagnostic::un_onimplemented]
LL + #[diagnostic::on_unimplemented]
   |

error: unknown diagnostic attribute
  --> $DIR/suggest_typos.rs:14:15
   |
LL | #[diagnostic::on_implemented]
   |               ^^^^^^^^^^^^^^
   |
help: an attribute with a similar name exists
   |
LL | #[diagnostic::on_unimplemented]
   |                  ++

error: unknown diagnostic attribute
  --> $DIR/suggest_typos.rs:19:15
   |
LL | #[diagnostic::dont_recommend]
   |               ^^^^^^^^^^^^^^
   |
help: an attribute with a similar name exists
   |
LL - #[diagnostic::dont_recommend]
LL + #[diagnostic::do_not_recommend]
   |

error: aborting due to 4 previous errors

"#;
    let stderr = ui("suggest_typos.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/multiline_spans.rs`. The checks of the parameters come before the
/// lints of parsing: rustc emits `check_mod_attrs`' lints before the delayed ones.
#[test]
fn parameters_and_specifiers_in_multiline_strings() {
    let source = r#"#![crate_type = "lib"]
#![deny(unknown_or_malformed_diagnostic_attributes)]


#[diagnostic::on_unimplemented(message = "here is a big \
                                         multiline string \
                                         {unknown}")]
//~^ ERROR there is no parameter `unknown` on trait `MultiLine` [malformed_diagnostic_format_literals]
pub trait MultiLine {}

#[diagnostic::on_unimplemented(message = "here is a big \
                                         multiline string {unknown}")]
//~^ ERROR there is no parameter `unknown` on trait `MultiLine2` [malformed_diagnostic_format_literals]
pub trait MultiLine2 {}

#[diagnostic::on_unimplemented(message = "here is a big \
    multiline string {unknown}")]
//~^ ERROR there is no parameter `unknown` on trait `MultiLine3` [malformed_diagnostic_format_literals]
pub trait MultiLine3 {}


#[diagnostic::on_unimplemented(message = "here is a big \
\
                \
                                \
                                                \
    multiline string {unknown}")]
//~^ ERROR there is no parameter `unknown` on trait `MultiLine4` [malformed_diagnostic_format_literals]
pub trait MultiLine4 {}

#[diagnostic::on_unimplemented(message = "here is a big \
                                         multiline string \
                                         {Self:+}")]
//~^ ERROR format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
pub trait MultiLineFmt {}

#[diagnostic::on_unimplemented(message = "here is a big \
                                         multiline string {Self:X}")]
//~^ ERROR format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
pub trait MultiLineFmt2 {}

#[diagnostic::on_unimplemented(message = "here is a big \
    multiline string {Self:#}")]
//~^ ERROR format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
pub trait MultiLineFmt3 {}


#[diagnostic::on_unimplemented(message = "here is a big \
\
                \
                                \
                                                \
    multiline string {Self:?}")]
//~^ ERROR format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
pub trait MultiLineFmt4 {}
"#;
    let expected = r#"error: there is no parameter `unknown` on trait `MultiLine`
  --> $DIR/multiline_spans.rs:7:43
   |
LL | ...                   {unknown}")]
   |                        ^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument
note: the lint level is defined here
  --> $DIR/multiline_spans.rs:2:9
   |
LL | #![deny(unknown_or_malformed_diagnostic_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(malformed_diagnostic_format_literals)]` implied by `#[deny(unknown_or_malformed_diagnostic_attributes)]`

error: there is no parameter `unknown` on trait `MultiLine2`
  --> $DIR/multiline_spans.rs:12:60
   |
LL | ...                   multiline string {unknown}")]
   |                                         ^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

error: there is no parameter `unknown` on trait `MultiLine3`
  --> $DIR/multiline_spans.rs:17:23
   |
LL |     multiline string {unknown}")]
   |                       ^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

error: there is no parameter `unknown` on trait `MultiLine4`
  --> $DIR/multiline_spans.rs:27:23
   |
LL |     multiline string {unknown}")]
   |                       ^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

error: format specifiers are not permitted in diagnostic attributes
  --> $DIR/multiline_spans.rs:33:47
   |
LL | ...                   {Self:+}")]
   |                            ^^ remove this format specifier

error: format specifiers are not permitted in diagnostic attributes
  --> $DIR/multiline_spans.rs:38:64
   |
LL | ...                   multiline string {Self:X}")]
   |                                             ^^ remove this format specifier

error: format specifiers are not permitted in diagnostic attributes
  --> $DIR/multiline_spans.rs:43:27
   |
LL |     multiline string {Self:#}")]
   |                           ^^ remove this format specifier

error: format specifiers are not permitted in diagnostic attributes
  --> $DIR/multiline_spans.rs:53:27
   |
LL |     multiline string {Self:?}")]
   |                           ^^ remove this format specifier

error: aborting due to 8 previous errors

"#;
    let stderr = ui("multiline_spans.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/opaque/duplicate.rs`.
#[test]
fn a_second_opaque_is_unused() {
    let source = r#"#![crate_type = "lib"]
#![feature(diagnostic_opaque)]
#![deny(unused_attributes)]
#[diagnostic::opaque]
#[diagnostic::opaque]
//~^ERROR unused attribute
macro_rules! m {
    () => {}
}
"#;
    let expected = r#"error: unused attribute
  --> $DIR/duplicate.rs:5:1
   |
LL | #[diagnostic::opaque]
   | ^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
note: attribute also specified here
  --> $DIR/duplicate.rs:4:1
   |
LL | #[diagnostic::opaque]
   | ^^^^^^^^^^^^^^^^^^^^^
note: the lint level is defined here
  --> $DIR/duplicate.rs:3:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    let stderr = ui("duplicate.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/do_not_recommend/malformed-diagnostic-attributes-if-expression.rs`.
#[test]
fn do_not_recommend_with_an_expression() {
    let source = r#"//@ check-pass
//@ reference: attributes.diagnostic.do_not_recommend.syntax

trait Foo {}

#[diagnostic::do_not_recommend(if not_accepted)]
//~^ WARNING `#[diagnostic::do_not_recommend]` does not expect any arguments
impl Foo for () {}

fn main() {}
"#;
    let expected = r#"warning: `#[diagnostic::do_not_recommend]` does not expect any arguments
  --> $DIR/malformed-diagnostic-attributes-if-expression.rs:6:1
   |
LL | #[diagnostic::do_not_recommend(if not_accepted)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: 1 warning emitted

"#;
    let stderr = ui("malformed-diagnostic-attributes-if-expression.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/do_not_recommend/does_not_acccept_args.rs`. Revision `current`.
#[test]
fn do_not_recommend_takes_no_arguments() {
    let source = r#"//@ check-pass
//@ revisions: current next
//@ ignore-compare-mode-next-solver (explicit revisions)
//@[next] compile-flags: -Znext-solver
//@ reference: attributes.diagnostic.do_not_recommend.syntax

trait Foo {}
trait Bar {}
trait Baz {}
trait Boo {}

#[diagnostic::do_not_recommend(if, crate, do yeet, false, dyn, abstract, gen, not_accepted)]
//~^ WARNING `#[diagnostic::do_not_recommend]` does not expect any arguments
impl<T> Foo for T where T: Send {}

#[diagnostic::do_not_recommend(not_accepted = "foo")]
//~^ WARNING `#[diagnostic::do_not_recommend]` does not expect any arguments
impl<T> Bar for T where T: Send {}

#[diagnostic::do_not_recommend(not_accepted(42))]
//~^ WARNING `#[diagnostic::do_not_recommend]` does not expect any arguments
impl<T> Baz for T where T: Send {}

#[diagnostic::do_not_recommend(x = y + z)]
//~^ WARNING `#[diagnostic::do_not_recommend]` does not expect any arguments
impl<T> Boo for T where T: Send {}

fn main() {}
"#;
    let expected = r#"warning: `#[diagnostic::do_not_recommend]` does not expect any arguments
  --> $DIR/does_not_acccept_args.rs:12:1
   |
LL | #[diagnostic::do_not_recommend(if, crate, do yeet, false, dyn, abstract, gen, not_accepted)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: `#[diagnostic::do_not_recommend]` does not expect any arguments
  --> $DIR/does_not_acccept_args.rs:16:1
   |
LL | #[diagnostic::do_not_recommend(not_accepted = "foo")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: `#[diagnostic::do_not_recommend]` does not expect any arguments
  --> $DIR/does_not_acccept_args.rs:20:1
   |
LL | #[diagnostic::do_not_recommend(not_accepted(42))]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: `#[diagnostic::do_not_recommend]` does not expect any arguments
  --> $DIR/does_not_acccept_args.rs:24:1
   |
LL | #[diagnostic::do_not_recommend(x = y + z)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: 4 warnings emitted

"#;
    let stderr = ui_core("does_not_acccept_args.rs", "2015", "send", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unmatched_args/report_warning_on_invalid_formats.rs`.
#[test]
fn on_unmatched_args_takes_only_this() {
    let source = r#"//@ check-pass
#![feature(diagnostic_on_unmatched_args)]

#[diagnostic::on_unmatched_args(
    message = "{T}! is missing arguments",
    //~^ WARN this format argument is not allowed in `#[diagnostic::on_unmatched_args]`
    //~| NOTE only `This` is allowed as a format argument
    //~| NOTE remove this format argument
    //~| NOTE `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default
)]
macro_rules! pair {
    ($ty:ty, $value:expr) => {};
}

fn main() {
    pair!(u8, 0);
}
"#;
    let expected = r#"warning: this format argument is not allowed in `#[diagnostic::on_unmatched_args]`
  --> $DIR/report_warning_on_invalid_formats.rs:5:17
   |
LL |     message = "{T}! is missing arguments",
   |                 ^ remove this format argument
   |
   = note: only `This` is allowed as a format argument, referring to the macro's name
   = note: `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: 1 warning emitted

"#;
    let stderr = ui("report_warning_on_invalid_formats.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unmatched_args/report_warning_on_invalid_meta_item_syntax.rs`.
#[test]
fn on_unmatched_args_with_a_value() {
    let source = r#"//@ check-pass
#![feature(diagnostic_on_unmatched_args)]

#[diagnostic::on_unmatched_args = "foo"]
//~^ WARN malformed `diagnostic::on_unmatched_args` attribute [malformed_diagnostic_attributes]
macro_rules! pair {
    ($ty:ty, $value:expr) => {};
}

fn main() {
    pair!(u8, 0);
}
"#;
    let expected = r#"warning: malformed `diagnostic::on_unmatched_args` attribute
  --> $DIR/report_warning_on_invalid_meta_item_syntax.rs:4:1
   |
LL | #[diagnostic::on_unmatched_args = "foo"]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `message`, `note` and `label` are allowed as options
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: 1 warning emitted

"#;
    let stderr = ui("report_warning_on_invalid_meta_item_syntax.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unmatched_args/report_warning_on_missing_options.rs`.
#[test]
fn on_unmatched_args_without_options() {
    let source = r#"//@ check-pass
#![feature(diagnostic_on_unmatched_args)]

#[diagnostic::on_unmatched_args]
//~^ WARN missing options for `diagnostic::on_unmatched_args` attribute [malformed_diagnostic_attributes]
macro_rules! pair {
    ($ty:ty, $value:expr) => {};
}

fn main() {
    pair!(u8, 0);
}
"#;
    let expected = r#"warning: missing options for `diagnostic::on_unmatched_args` attribute
  --> $DIR/report_warning_on_missing_options.rs:4:1
   |
LL | #[diagnostic::on_unmatched_args]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: at least one of the `message`, `note` and `label` options are expected
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: 1 warning emitted

"#;
    let stderr = ui("report_warning_on_missing_options.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unmatched_args/report_warning_on_unknown_options.rs`.
#[test]
fn on_unmatched_args_with_an_unknown_option() {
    let source = r#"//@ check-pass
#![feature(diagnostic_on_unmatched_args)]

#[diagnostic::on_unmatched_args(unsupported = "foo")]
//~^ WARN malformed `diagnostic::on_unmatched_args` attribute [malformed_diagnostic_attributes]
macro_rules! pair {
    ($ty:ty, $value:expr) => {};
}

fn main() {
    pair!(u8, 0);
}
"#;
    let expected = r#"warning: malformed `diagnostic::on_unmatched_args` attribute
  --> $DIR/report_warning_on_unknown_options.rs:4:33
   |
LL | #[diagnostic::on_unmatched_args(unsupported = "foo")]
   |                                 ^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `message`, `note` and `label` are allowed as options
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: 1 warning emitted

"#;
    let stderr = ui("report_warning_on_unknown_options.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unmatched_args/disabled-feature-issue-159886.rs`.
#[test]
fn on_unmatched_args_without_its_feature_is_not_parsed() {
    let source = r#"//@ check-pass
//@ needs-rustc-debug-assertions

#![allow(unknown_or_malformed_diagnostic_attributes)]
#![allow(unused_macros)]

#[diagnostic::on_unmatched_args(message)]
macro_rules! pair {
    () => {};
}

fn main() {}
"#;
    let expected = r#""#;
    let stderr = ui("disabled-feature-issue-159886.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unknown/incorrect_unstable_positions.rs`.
#[test]
fn on_unknown_on_an_expression_is_only_misplaced() {
    let source = r#"//@ check-pass

#![feature(custom_inner_attributes)]
#![feature(diagnostic_on_unknown)]
#![feature(stmt_expr_attributes)]

fn main() {
    #[diagnostic::on_unknown(message = "anonymous block")]
    //~^ WARN cannot be used on
    {}
}
"#;
    let expected = r#"warning: the `diagnostic::on_unknown` attribute cannot be used on expressions
  --> $DIR/incorrect_unstable_positions.rs:8:7
   |
LL |     #[diagnostic::on_unknown(message = "anonymous block")]
   |       ^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the `diagnostic::on_unknown` attribute can be applied to crates, modules, and use statements
   = note: `#[warn(misplaced_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: 1 warning emitted

"#;
    let stderr = ui("incorrect_unstable_positions.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unimplemented/broken_format.rs`.
#[test]
fn format_strings_that_do_not_parse_or_have_what_is_not_allowed() {
    let source = r#"//@ reference: attributes.diagnostic.on_unimplemented.invalid-string
#[diagnostic::on_unimplemented(message = "{{Test } thing")]
//~^WARN unmatched `}` found
trait ImportantTrait1 {}

#[diagnostic::on_unimplemented(message = "Test {}")]
//~^WARN positional arguments are not permitted in diagnostic attributes
trait ImportantTrait2 {}

#[diagnostic::on_unimplemented(message = "Test {1:}")]
//~^WARN indexed format arguments are not permitted in diagnostic attributes
//~|WARN format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
trait ImportantTrait3 {}

#[diagnostic::on_unimplemented(message = "Test {Self:123}")]
//~^WARN format specifiers are not permitted in diagnostic attributes
trait ImportantTrait4 {}

#[diagnostic::on_unimplemented(message = "Test {Self:!}")]
//~^WARN format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
trait ImportantTrait5 {}

#[diagnostic::on_unimplemented(message = "Test {Self:}")]
//~^WARN format specifiers are not permitted in diagnostic attributes [malformed_diagnostic_format_literals]
trait ImportantTrait6 {}


fn check_1(_: impl ImportantTrait1) {}
fn check_2(_: impl ImportantTrait2) {}
fn check_3(_: impl ImportantTrait3) {}
fn check_4(_: impl ImportantTrait4) {}
fn check_5(_: impl ImportantTrait5) {}
fn check_6(_: impl ImportantTrait6) {}

fn main() {
    check_1(());
    //~^ERROR {{Test } thing
    check_2(());
    //~^ERROR Test {}
    check_3(());
    //~^ERROR Test {1}
    check_4(());
    //~^ERROR Test ()
    check_5(());
    //~^ERROR Test ()
    check_6(());
    //~^ERROR Test ()
}
"#;
    let expected = r#"warning: unmatched `}` found
  --> $DIR/broken_format.rs:2:42
   |
LL | #[diagnostic::on_unimplemented(message = "{{Test } thing")]
   |                                          ^^^^^^^^-^^^^^^^
   |                                                  |
   |                                                  unmatched `}`
   |
   = note: `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: positional arguments are not permitted in diagnostic attributes
  --> $DIR/broken_format.rs:6:49
   |
LL | #[diagnostic::on_unimplemented(message = "Test {}")]
   |                                                 ^ remove this format argument
   |
   = help: you can print empty braces by escaping them

warning: indexed format arguments are not permitted in diagnostic attributes
  --> $DIR/broken_format.rs:10:49
   |
LL | #[diagnostic::on_unimplemented(message = "Test {1:}")]
   |                                                 ^ remove this format argument

warning: format specifiers are not permitted in diagnostic attributes
  --> $DIR/broken_format.rs:10:50
   |
LL | #[diagnostic::on_unimplemented(message = "Test {1:}")]
   |                                                  ^ remove this format specifier

warning: format specifiers are not permitted in diagnostic attributes
  --> $DIR/broken_format.rs:15:53
   |
LL | #[diagnostic::on_unimplemented(message = "Test {Self:123}")]
   |                                                     ^^^^ remove this format specifier

warning: format specifiers are not permitted in diagnostic attributes
  --> $DIR/broken_format.rs:19:53
   |
LL | #[diagnostic::on_unimplemented(message = "Test {Self:!}")]
   |                                                     ^^ remove this format specifier

warning: format specifiers are not permitted in diagnostic attributes
  --> $DIR/broken_format.rs:23:53
   |
LL | #[diagnostic::on_unimplemented(message = "Test {Self:}")]
   |                                                     ^ remove this format specifier

"#;
    let stderr = ui("broken_format.rs", "2015", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_unimplemented/do_not_accept_options_of_the_internal_rustc_attribute.rs`.
#[test]
fn options_of_rustc_on_unimplemented_are_malformed() {
    let source = r#"//@ reference: attributes.diagnostic.on_unimplemented.format-parameters
//@ reference: attributes.diagnostic.on_unimplemented.keys
//@ reference: attributes.diagnostic.on_unimplemented.syntax
//@ reference: attributes.diagnostic.on_unimplemented.invalid-formats
#[diagnostic::on_unimplemented(
    on(Self = "&str"),
    //~^WARN malformed `diagnostic::on_unimplemented` attribute
    message = "trait has `{Self}` and `{T}` as params",
    label = "trait has `{Self}` and `{T}` as params",
    note  = "trait has `{Self}` and `{T}` as params",
    parent_label = "in this scope",
    //~^WARN malformed `diagnostic::on_unimplemented` attribute
    append_const_msg
    //~^WARN malformed `diagnostic::on_unimplemented` attribute
)]
trait Foo<T> {}

#[diagnostic::on_unimplemented = "Message"]
//~^WARN malformed `diagnostic::on_unimplemented` attribute
trait Bar {}

#[diagnostic::on_unimplemented(message = "Not allowed to apply it on a impl")]
//~^WARN cannot be used on
impl Bar for i32 {}

// cannot use special rustc_on_unimplement symbols
// in the format string
#[diagnostic::on_unimplemented(
    message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
    //~^WARN there is no parameter `from_desugaring` on trait `Baz`
    //~|WARN there is no parameter `direct` on trait `Baz`
    //~|WARN there is no parameter `cause` on trait `Baz`
    //~|WARN there is no parameter `integral` on trait `Baz`
    //~|WARN there is no parameter `integer` on trait `Baz`
    //~|WARN there is no parameter `r#struct` on trait `Baz`
    //~|WARN there is no parameter `r#enum` on trait `Baz`
    //~|WARN there is no parameter `union` on trait `Baz`
    label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
    //~^WARN there is no parameter `float` on trait `Baz`
    //~|WARN there is no parameter `_Self` on trait `Baz`
    //~|WARN there is no parameter `crate_local` on trait `Baz`
    //~|WARN there is no parameter `ItemContext` on trait `Baz`
    //~|WARN there is no parameter `This` on trait `Baz`
    //~|WARN there is no parameter `This` on trait `Baz`
    //~|WARN there is no parameter `This` on trait `Baz`
    //~|WARN format specifiers are not permitted in diagnostic attributes
    //~|WARN format specifiers are not permitted in diagnostic attributes
)]
trait Baz {}

fn takes_foo(_: impl Foo<i32>) {}
fn takes_bar(_: impl Bar) {}
fn takes_baz(_: impl Baz) {}

fn main() {
    takes_foo(());
    //~^ERROR trait has `()` and `i32` as params
    takes_bar(());
    //~^ERROR the trait bound `(): Bar` is not satisfied
    takes_baz(());
    //~^ERROR {from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}
}
"#;
    let expected = r#"warning: there is no parameter `from_desugaring` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:17
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                 ^^^^^^^^^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument
   = note: `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: there is no parameter `direct` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:34
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                  ^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `cause` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:42
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                          ^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `integral` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:49
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                                 ^^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `integer` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:59
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                                           ^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `r#struct` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:68
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                                                    ^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `r#enum` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:76
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                                                            ^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `union` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:29:82
   |
LL |     message = "{from_desugaring}{direct}{cause}{integral}{integer}{struct}{enum}{union}",
   |                                                                                  ^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `float` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:15
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |               ^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `_Self` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:22
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                      ^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `crate_local` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:29
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                             ^^^^^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `This` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:42
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                                          ^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `ItemContext` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:48
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                                                ^^^^^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `This` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:61
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                                                             ^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: there is no parameter `This` on trait `Baz`
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:72
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                                                                        ^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: malformed `diagnostic::on_unimplemented` attribute
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:6:5
   |
LL |     on(Self = "&str"),
   |     ^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `message`, `note` and `label` are allowed as options
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: malformed `diagnostic::on_unimplemented` attribute
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:11:5
   |
LL |     parent_label = "in this scope",
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `message`, `note` and `label` are allowed as options

warning: malformed `diagnostic::on_unimplemented` attribute
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:13:5
   |
LL |     append_const_msg
   |     ^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `message`, `note` and `label` are allowed as options

warning: malformed `diagnostic::on_unimplemented` attribute
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:18:1
   |
LL | #[diagnostic::on_unimplemented = "Message"]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `message`, `note` and `label` are allowed as options

warning: the `diagnostic::on_unimplemented` attribute cannot be used on trait impl blocks
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:22:3
   |
LL | #[diagnostic::on_unimplemented(message = "Not allowed to apply it on a impl")]
   |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the `diagnostic::on_unimplemented` attribute can only be applied to traits
   = note: `#[warn(misplaced_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: format specifiers are not permitted in diagnostic attributes
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:65
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                                                                 ^^^^^ remove this format specifier

warning: format specifiers are not permitted in diagnostic attributes
  --> $DIR/do_not_accept_options_of_the_internal_rustc_attribute.rs:38:76
   |
LL |     label = "{float}{_Self}{crate_local}{This}{ItemContext}{This:path}{This:resolved}"
   |                                                                            ^^^^^^^^^ remove this format specifier

"#;
    let name = "do_not_accept_options_of_the_internal_rustc_attribute.rs";
    let stderr = ui(name, "2015", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_move/report_warning_on_invalid_formats.rs`.
#[test]
fn on_move_with_an_unknown_parameter() {
    let source = r#"#![feature(diagnostic_on_move)]

#[diagnostic::on_move(
    message = "Foo {Baz}",
    //~^WARN unknown parameter `Baz`
    label = "Bar",
)]
#[derive(Debug)]
struct Foo;

fn takes_foo(_: Foo) {}

fn main() {
    let foo = Foo;
    takes_foo(foo);
    let bar = foo;
    //~^ERROR Foo
}
"#;
    let expected = r#"warning: unknown parameter `Baz`
  --> $DIR/report_warning_on_invalid_formats.rs:4:21
   |
LL |     message = "Foo {Baz}",
   |                     ^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument
   = note: `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

"#;
    let stderr = ui_core("report_warning_on_invalid_formats.rs", "2015", "fmt", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_move/report_warning_on_duplicated_options.rs`.
#[test]
fn on_move_options_given_twice() {
    let source = r#"#![feature(diagnostic_on_move)]

#[diagnostic::on_move(
    message = "first message",
    label = "first label",
)]
#[diagnostic::on_move(
    message = "second message",
    //~^ WARN `message` is ignored due to previous definition of `message` [malformed_diagnostic_attributes]
    label = "second label",
    //~^ WARN `label` is ignored due to previous definition of `label` [malformed_diagnostic_attributes]
)]
struct Foo;

fn takes_foo(_: Foo) {}

fn main() {
    let foo = Foo;
    takes_foo(foo);
    let bar = foo;
    //~^ERROR first message
}
"#;
    let expected = r#"warning: `message` is ignored due to previous definition of `message`
  --> $DIR/report_warning_on_duplicated_options.rs:8:5
   |
LL |     message = "first message",
   |     ------------------------- `message` is first declared here
...
LL |     message = "second message",
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^ `message` is later redundantly declared here
   |
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: `label` is ignored due to previous definition of `label`
  --> $DIR/report_warning_on_duplicated_options.rs:10:5
   |
LL |     label = "first label",
   |     --------------------- `label` is first declared here
...
LL |     label = "second label",
   |     ^^^^^^^^^^^^^^^^^^^^^^ `label` is later redundantly declared here

"#;
    let stderr = ui("report_warning_on_duplicated_options.rs", "2015", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_type_error/on-type-error-invalid-placeholders.rs`.
#[test]
fn on_move_does_not_take_expected_and_found() {
    let source = r#"#![feature(diagnostic_on_move)]
#[diagnostic::on_move(message = "Foo", label = "Bar", note = "{Expected} {Found}")]
//~^ WARN unknown parameter `Expected` [malformed_diagnostic_format_literals]
//~| WARN unknown parameter `Found` [malformed_diagnostic_format_literals]
#[derive(Debug)]
struct Foo;
fn takes_foo(_: Foo) {}
fn main() {
    let foo = Foo;
    takes_foo(foo);
    let bar = foo;
    //~^ERROR Foo
}
"#;
    let expected = r#"warning: unknown parameter `Expected`
  --> $DIR/on-type-error-invalid-placeholders.rs:2:64
   |
LL | #[diagnostic::on_move(message = "Foo", label = "Bar", note = "{Expected} {Found}")]
   |                                                                ^^^^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument
   = note: `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: unknown parameter `Found`
  --> $DIR/on-type-error-invalid-placeholders.rs:2:75
   |
LL | #[diagnostic::on_move(message = "Foo", label = "Bar", note = "{Expected} {Found}")]
   |                                                                           ^^^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

"#;
    let stderr = ui_core("on-type-error-invalid-placeholders.rs", "2015", "fmt", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_type_error/on_type_error_with_too_many_generics.rs`.
#[test]
fn on_type_error_needs_one_generic_parameter() {
    let source = r#"#![feature(diagnostic_on_type_error)]

#[diagnostic::on_type_error(note = "custom on_type_error note: too many generics")]
struct TooMany<T, U>(T, U);
//~^ WARN `#[diagnostic::on_type_error]` only supports exactly one ADT generic parameter, but found `2`

fn main() {
    let _: TooMany<i32, i32> = TooMany(32, "test");
    //~^ ERROR mismatched types
}
"#;
    let expected = r#"warning: `#[diagnostic::on_type_error]` only supports exactly one ADT generic parameter, but found `2`
  --> $DIR/on_type_error_with_too_many_generics.rs:4:15
   |
LL | struct TooMany<T, U>(T, U);
   |               ^^^^^^
   |
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

"#;
    let stderr = ui("on_type_error_with_too_many_generics.rs", "2015", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_type_error/report_warning_on_unknown_options.rs`.
#[test]
fn on_type_error_takes_only_note() {
    let source = r#"#![feature(diagnostic_on_type_error)]

#[diagnostic::on_type_error(unknown = "custom on_type_error note: option")]
//~^ WARN malformed `diagnostic::on_type_error` attribute
struct UnknownOption<T>(T);

#[diagnostic::on_type_error(message = "custom on_type_error note: not allowed")]
//~^ WARN malformed `diagnostic::on_type_error` attribute
struct MessageOption<T>(T);

#[diagnostic::on_type_error(label = "custom on_type_error note: not allowed")]
//~^ WARN malformed `diagnostic::on_type_error` attribute
struct LabelOption<T>(T);

fn main() {
    // Create type errors
    let _: UnknownOption<i32> = MessageOption(43);
    //~^ ERROR mismatched types
    let _: MessageOption<i32> = UnknownOption(32);
    //~^ ERROR mismatched types
    let _: LabelOption<i32> = UnknownOption(32);
    //~^ ERROR mismatched types
}
"#;
    let expected = r#"warning: malformed `diagnostic::on_type_error` attribute
  --> $DIR/report_warning_on_unknown_options.rs:3:29
   |
LL | #[diagnostic::on_type_error(unknown = "custom on_type_error note: option")]
   |                             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `note` is allowed as option for `diagnostic::on_type_error`
   = note: `#[warn(malformed_diagnostic_attributes)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: malformed `diagnostic::on_type_error` attribute
  --> $DIR/report_warning_on_unknown_options.rs:7:29
   |
LL | #[diagnostic::on_type_error(message = "custom on_type_error note: not allowed")]
   |                             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `note` is allowed as option for `diagnostic::on_type_error`

warning: malformed `diagnostic::on_type_error` attribute
  --> $DIR/report_warning_on_unknown_options.rs:11:29
   |
LL | #[diagnostic::on_type_error(label = "custom on_type_error note: not allowed")]
   |                             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ invalid option found here
   |
   = help: only `note` is allowed as option for `diagnostic::on_type_error`

"#;
    let stderr = ui("report_warning_on_unknown_options.rs", "2015", source);
    assert_starts_with(&stderr, expected);
}

/// `tests/ui/diagnostic_namespace/on_const/malformed_format_literals.rs`.
#[test]
fn on_const_with_unknown_parameters() {
    let source = r#"#![crate_type = "lib"]
#![feature(diagnostic_on_const)]
#![feature(const_trait_impl)]

pub struct X;

const trait Y {
    fn blah(&self) {}
}

#[diagnostic::on_const(
    message = "my message {Foo}",
    //~^ WARN unknown parameter `Foo`
    label = "my label {Bar}",
    //~^ WARN unknown parameter `Bar`
    note = "my label {Baz}",
    //~^ WARN unknown parameter `Baz`
)]
impl Y for X {}

const _: () = {
    X {}.blah();
    //~^ ERROR my message {Foo} [E0277]

};
"#;
    let expected = r#"warning: unknown parameter `Foo`
  --> $DIR/malformed_format_literals.rs:12:28
   |
LL |     message = "my message {Foo}",
   |                            ^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument
   = note: `#[warn(malformed_diagnostic_format_literals)]` (part of `#[warn(unknown_or_malformed_diagnostic_attributes)]`) on by default

warning: unknown parameter `Bar`
  --> $DIR/malformed_format_literals.rs:14:24
   |
LL |     label = "my label {Bar}",
   |                        ^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

warning: unknown parameter `Baz`
  --> $DIR/malformed_format_literals.rs:16:23
   |
LL |     note = "my label {Baz}",
   |                       ^^^
   |
   = help: expect either a generic argument name or `{Self}` as format argument

"#;
    let stderr = ui("malformed_format_literals.rs", "2015", source);
    assert_starts_with(&stderr, expected);
}
