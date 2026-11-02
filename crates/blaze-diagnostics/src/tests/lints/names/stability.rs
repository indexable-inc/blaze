//! The stability lints (`deprecated`, `deprecated_in_future`, `useless_deprecated`, the
//! `unused_attributes` of a `#[deprecated]` with no effect, `#[macro_escape]`) against rustc's
//! UI tests.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked, checked_with, compiletest, error, normalized, warning},
};

/// What rustc writes compiling `fixture` (its files named `$DIR/<path>`) as compiletest runs a
/// UI test of pass mode `mode`.
fn ui(fixture: &str, mode: Mode) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(mode, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// The UI test `source`, named `$DIR/<name>`, of edition 2015 with no other crate.
fn one(name: &str, source: &str, mode: Mode) -> String {
    ui(&format!("//- /{name} crate:other:main edition:2015\n{source}"), mode)
}

#[test]
fn deprecation_in_future() {
    let source = include_str!("stability/deprecation-in-future.rs");
    let expected = include_str!("stability/deprecation-in-future.stderr");
    assert_eq!(one("deprecation-in-future.rs", source, Mode::Other), expected);
}

#[test]
fn deprecated_macro_escape() {
    let source = include_str!("stability/deprecated-macro_escape.rs");
    let expected = include_str!("stability/deprecated-macro_escape.stderr");
    assert_eq!(one("deprecated-macro_escape.rs", source, Mode::RunPass), expected);
}

#[test]
fn deprecated_macro_escape_inner() {
    let source = include_str!("stability/deprecated-macro_escape-inner.rs");
    let expected = include_str!("stability/deprecated-macro_escape-inner.stderr");
    assert_eq!(one("deprecated-macro_escape-inner.rs", source, Mode::RunPass), expected);
}

#[test]
fn unit_and_tuple_struct() {
    let source = include_str!("stability/unit_and_tuple_struct.rs");
    let expected = include_str!("stability/unit_and_tuple_struct.stderr");
    assert_eq!(one("unit_and_tuple_struct.rs", source, Mode::Other), expected);
}

#[test]
fn deprecation_lint_nested() {
    let source = include_str!("stability/deprecation-lint-nested.rs");
    let expected = include_str!("stability/deprecation-lint-nested.stderr");
    let fixture = format!("//- minicore: fmt\n//- /deprecation-lint-nested.rs crate:other:main edition:2015\n{source}");
    assert_eq!(ui(&fixture, Mode::Other), expected);
}

/// `auxiliary/deprecation-lint.rs` as the crate `deprecation_lint`, with the UI test `source`
/// named `$DIR/<name>` depending on it.
fn with_deprecation_lint(name: &str, source: &str, header: &str, deps: &str) -> String {
    let aux = include_str!("stability/auxiliary/deprecation-lint.rs");
    format!(
        "{header}//- /auxiliary/deprecation-lint.rs crate:deprecation_lint edition:2015 library\n{aux}\
         //- /{name} crate:other:main edition:2015 deps:deprecation_lint{deps}\n{source}"
    )
}

#[test]
fn deprecation_lint() {
    let source = include_str!("stability/deprecation-lint.rs");
    // The test's `Option<String>` and `#[derive(Debug)]`: the prelude of a `std` with them.
    let std = "//- minicore: option, fmt, derive
//- /std.rs crate:std edition:2015 library
pub mod string {
    pub struct String;
}
pub mod prelude {
    pub mod rust_2015 {
        pub use core::prelude::rust_2015::*;
        pub use crate::string::String;
    }
}
";
    let fixture = with_deprecation_lint("deprecation-lint.rs", source, std, ",std");
    assert_reports(
        &ui(&fixture, Mode::Other),
        &[
            error(16, &["`deprecation_lint::deprecated`"]),
            error(21, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(23, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(25, &["`deprecation_lint::deprecated_text`"]),
            error(30, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(32, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(34, &["`deprecation_lint::DeprecatedStruct`"]),
            error(38, &["`deprecation_lint::DeprecatedUnitStruct`"]),
            error(40, &["`deprecation_lint::Enum::DeprecatedVariant`"]),
            error(42, &["`deprecation_lint::DeprecatedTupleStruct`"]),
            error(44, &["`deprecation_lint::nested::DeprecatedStruct`"]),
            error(48, &["`deprecation_lint::nested::DeprecatedUnitStruct`"]),
            error(50, &["`deprecation_lint::nested::Enum::DeprecatedVariant`"]),
            error(52, &["`deprecation_lint::nested::DeprecatedTupleStruct`"]),
            error(59, &["`deprecation_lint::deprecated_text`"]),
            error(60, &["`deprecation_lint::deprecated_text`"]),
            error(65, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(67, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(69, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(71, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(81, &["`deprecation_lint::DeprecatedTrait`"]),
            error(82, &["`deprecation_lint::DeprecatedTrait`"]),
            error(113, &["`deprecation_lint::Deprecated`"]),
            error(122, &["`deprecation_lint::Deprecated`"]),
            error(128, &["`deprecation_lint::Deprecated`"]),
            error(132, &["`deprecation_lint::Deprecated2`"]),
            error(142, &["`deprecation_lint::Deprecated2`"]),
            error(151, &["`deprecation_lint::Deprecated2`"]),
            error(162, &["`deprecation_lint::deprecated_mod::deprecated`"]),
            error(245, &["`this_crate::deprecated`"]),
            error(250, &["`this_crate::Trait::trait_deprecated`"]),
            error(252, &["`this_crate::Trait::trait_deprecated`"]),
            error(254, &["`this_crate::deprecated_text`"]),
            error(259, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(261, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(264, &["`this_crate::deprecated_future`"]),
            error(265, &["`this_crate::deprecated_future_text`"]),
            error(267, &["`this_crate::DeprecatedStruct`"]),
            error(272, &["`this_crate::DeprecatedUnitStruct`"]),
            error(274, &["`this_crate::Enum::DeprecatedVariant`"]),
            error(276, &["`this_crate::DeprecatedTupleStruct`"]),
            error(278, &["`this_crate::nested::DeprecatedStruct`"]),
            error(284, &["`this_crate::nested::DeprecatedUnitStruct`"]),
            error(286, &["`this_crate::nested::Enum::DeprecatedVariant`"]),
            error(288, &["`this_crate::nested::DeprecatedTupleStruct`"]),
            error(293, &["`this_crate::Trait::trait_deprecated`"]),
            error(295, &["`this_crate::Trait::trait_deprecated`"]),
            error(297, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(299, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(317, &["`this_crate::test_fn_closure_body::{closure#0}::bar`"]),
            error(336, &["`this_crate::DeprecatedTrait`"]),
            error(338, &["`this_crate::DeprecatedTrait`"]),
            error(390, &["`this_crate2::Deprecated`"]),
            error(399, &["`this_crate2::Deprecated`"]),
            error(405, &["`this_crate2::Deprecated`"]),
            error(410, &["`this_crate2::Deprecated2`"]),
            error(420, &["`this_crate2::Deprecated2`"]),
            error(429, &["`this_crate2::Deprecated2`"]),
            error(17, &["`deprecation_lint::MethodTester::method_deprecated`"]),
            error(18, &["`deprecation_lint::MethodTester::method_deprecated`"]),
            error(19, &["`deprecation_lint::MethodTester::method_deprecated`"]),
            error(20, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(22, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(26, &["`deprecation_lint::MethodTester::method_deprecated_text`"]),
            error(27, &["`deprecation_lint::MethodTester::method_deprecated_text`"]),
            error(28, &["`deprecation_lint::MethodTester::method_deprecated_text`"]),
            error(29, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(31, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(35, &["`deprecation_lint::DeprecatedStruct::i`"]),
            error(45, &["`deprecation_lint::nested::DeprecatedStruct::i`"]),
            error(64, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(66, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(68, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(70, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(75, &["`deprecation_lint::Trait::trait_deprecated`"]),
            error(76, &["`deprecation_lint::Trait::trait_deprecated_text`"]),
            error(86, &["`deprecation_lint::Stable::override2`"]),
            error(90, &["`deprecation_lint::Stable::override2`"]),
            error(94, &["`deprecation_lint::Stable::override2`"]),
            error(102, &["`deprecation_lint::Stable2::2`"]),
            error(107, &["`deprecation_lint::Stable2::2`"]),
            error(115, &["`deprecation_lint::Deprecated::inherit`"]),
            error(119, &["`deprecation_lint::Deprecated::inherit`"]),
            error(124, &["`deprecation_lint::Deprecated::inherit`"]),
            error(135, &["`deprecation_lint::Deprecated2::0`"]),
            error(137, &["`deprecation_lint::Deprecated2::1`"]),
            error(139, &["`deprecation_lint::Deprecated2::2`"]),
            error(144, &["`deprecation_lint::Deprecated2::0`"]),
            error(146, &["`deprecation_lint::Deprecated2::1`"]),
            error(148, &["`deprecation_lint::Deprecated2::2`"]),
            error(246, &["`this_crate::MethodTester::method_deprecated`"]),
            error(247, &["`this_crate::MethodTester::method_deprecated`"]),
            error(248, &["`this_crate::MethodTester::method_deprecated`"]),
            error(249, &["`this_crate::Trait::trait_deprecated`"]),
            error(251, &["`this_crate::Trait::trait_deprecated`"]),
            error(255, &["`this_crate::MethodTester::method_deprecated_text`"]),
            error(256, &["`this_crate::MethodTester::method_deprecated_text`"]),
            error(257, &["`this_crate::MethodTester::method_deprecated_text`"]),
            error(258, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(260, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(269, &["`this_crate::DeprecatedStruct::i`"]),
            error(280, &["`this_crate::nested::DeprecatedStruct::i`"]),
            error(292, &["`this_crate::Trait::trait_deprecated`"]),
            error(294, &["`this_crate::Trait::trait_deprecated`"]),
            error(296, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(298, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(303, &["`this_crate::Trait::trait_deprecated`"]),
            error(304, &["`this_crate::Trait::trait_deprecated_text`"]),
            error(363, &["`this_crate2::Stable::override2`"]),
            error(367, &["`this_crate2::Stable::override2`"]),
            error(371, &["`this_crate2::Stable::override2`"]),
            error(379, &["`this_crate2::Stable2::2`"]),
            error(384, &["`this_crate2::Stable2::2`"]),
            error(392, &["`this_crate2::Deprecated::inherit`"]),
            error(396, &["`this_crate2::Deprecated::inherit`"]),
            error(401, &["`this_crate2::Deprecated::inherit`"]),
            error(413, &["`this_crate2::Deprecated2::0`"]),
            error(415, &["`this_crate2::Deprecated2::1`"]),
            error(417, &["`this_crate2::Deprecated2::2`"]),
            error(422, &["`this_crate2::Deprecated2::0`"]),
            error(424, &["`this_crate2::Deprecated2::1`"]),
            error(426, &["`this_crate2::Deprecated2::2`"]),
            error(280, &["private"]),
        ],
    );
}

#[test]
fn deprecation_lint_2() {
    let source = include_str!("stability/deprecation-lint-2.rs");
    let expected = include_str!("stability/deprecation-lint-2.stderr");
    assert_eq!(ui(&with_deprecation_lint("deprecation-lint-2.rs", source, "", ""), Mode::Other), expected);
}

#[test]
fn deprecation_lint_3() {
    let source = include_str!("stability/deprecation-lint-3.rs");
    let expected = include_str!("stability/deprecation-lint-3.stderr");
    assert_eq!(ui(&with_deprecation_lint("deprecation-lint-3.rs", source, "", ""), Mode::Other), expected);
}

#[test]
fn lint_stability_deprecated() {
    let source = include_str!("stability/lint-stability-deprecated.rs");
    let fixture = format!(
        "//- /auxiliary/lint_stability.rs crate:lint_stability edition:2015 library\n{}\
         //- /auxiliary/inherited_stability.rs crate:inherited_stability edition:2015 library\n{}\
         //- /auxiliary/stability_cfg1.rs crate:stability_cfg1 edition:2015 library\n{}\
         //- /auxiliary/stability-cfg2.rs crate:stability_cfg2 edition:2015 library cfg:foo\n{}\
         //- /lint-stability-deprecated.rs crate:other:main edition:2015 \
         deps:lint_stability,inherited_stability,stability_cfg1,stability_cfg2\n{source}",
        include_str!("stability/auxiliary/lint_stability.rs"),
        include_str!("stability/auxiliary/inherited_stability.rs"),
        include_str!("stability/auxiliary/stability_cfg1.rs"),
        include_str!("stability/auxiliary/stability-cfg2.rs"),
    );
    assert_reports(
        &ui(&fixture, Mode::Other),
        &[
            warning(24, &["`lint_stability::deprecated`"]),
            warning(29, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(31, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(33, &["`lint_stability::deprecated_text`"]),
            warning(38, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(40, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(42, &["`lint_stability::deprecated_unstable`"]),
            warning(47, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(49, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(51, &["`lint_stability::deprecated_unstable_text`"]),
            warning(56, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(58, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(105, &["`lint_stability::DeprecatedStruct`"]),
            warning(108, &["`lint_stability::DeprecatedUnstableStruct`"]),
            warning(115, &["`lint_stability::DeprecatedUnitStruct`"]),
            warning(116, &["`lint_stability::DeprecatedUnstableUnitStruct`"]),
            warning(120, &["`lint_stability::Enum::DeprecatedVariant`"]),
            warning(121, &["`lint_stability::Enum::DeprecatedUnstableVariant`"]),
            warning(125, &["`lint_stability::DeprecatedTupleStruct`"]),
            warning(126, &["`lint_stability::DeprecatedUnstableTupleStruct`"]),
            warning(135, &["`lint_stability::deprecated_text`"]),
            warning(136, &["`lint_stability::deprecated_unstable_text`"]),
            warning(137, &["`lint_stability::deprecated_text`"]),
            warning(142, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(144, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(146, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(148, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(150, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(152, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(154, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(156, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(184, &["`lint_stability::DeprecatedTrait`"]),
            warning(186, &["`lint_stability::DeprecatedTrait`"]),
            warning(205, &[]),
            warning(327, &["`this_crate::deprecated`"]),
            warning(332, &["`this_crate::Trait::trait_deprecated`"]),
            warning(334, &["`this_crate::Trait::trait_deprecated`"]),
            warning(336, &["`this_crate::deprecated_text`"]),
            warning(341, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(343, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(381, &["`this_crate::DeprecatedStruct`"]),
            warning(388, &["`this_crate::DeprecatedUnitStruct`"]),
            warning(392, &["`this_crate::Enum::DeprecatedVariant`"]),
            warning(396, &["`this_crate::DeprecatedTupleStruct`"]),
            warning(403, &["`this_crate::Trait::trait_deprecated`"]),
            warning(405, &["`this_crate::Trait::trait_deprecated`"]),
            warning(407, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(409, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(436, &["`this_crate::test_fn_body::fn_in_body`"]),
            warning(456, &["`this_crate::DeprecatedTrait`"]),
            warning(458, &["`this_crate::DeprecatedTrait`"]),
            warning(444, &["`this_crate::MethodTester::test_method_body::fn_in_body`"]),
            warning(97, &["`lint_stability::TraitWithAssociatedTypes::TypeDeprecated`"]),
            warning(101, &["`lint_stability::TraitWithAssociatedTypes::TypeDeprecated`"]),
            warning(25, &["`lint_stability::MethodTester::method_deprecated`"]),
            warning(26, &["`lint_stability::MethodTester::method_deprecated`"]),
            warning(27, &["`lint_stability::MethodTester::method_deprecated`"]),
            warning(28, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(30, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(34, &["`lint_stability::MethodTester::method_deprecated_text`"]),
            warning(35, &["`lint_stability::MethodTester::method_deprecated_text`"]),
            warning(36, &["`lint_stability::MethodTester::method_deprecated_text`"]),
            warning(37, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(39, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(43, &["`lint_stability::MethodTester::method_deprecated_unstable`"]),
            warning(44, &["`lint_stability::MethodTester::method_deprecated_unstable`"]),
            warning(45, &["`lint_stability::MethodTester::method_deprecated_unstable`"]),
            warning(46, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(48, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(52, &["`lint_stability::MethodTester::method_deprecated_unstable_text`"]),
            warning(53, &["`lint_stability::MethodTester::method_deprecated_unstable_text`"]),
            warning(54, &["`lint_stability::MethodTester::method_deprecated_unstable_text`"]),
            warning(55, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(57, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(106, &["`lint_stability::DeprecatedStruct::i`"]),
            warning(110, &["`lint_stability::DeprecatedUnstableStruct::i`"]),
            warning(141, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(143, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(145, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(147, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(149, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(151, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(153, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(155, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(172, &["`lint_stability::Trait::trait_deprecated`"]),
            warning(173, &["`lint_stability::Trait::trait_deprecated_text`"]),
            warning(174, &["`lint_stability::Trait::trait_deprecated_unstable`"]),
            warning(175, &["`lint_stability::Trait::trait_deprecated_unstable_text`"]),
            warning(328, &["`this_crate::MethodTester::method_deprecated`"]),
            warning(329, &["`this_crate::MethodTester::method_deprecated`"]),
            warning(330, &["`this_crate::MethodTester::method_deprecated`"]),
            warning(331, &["`this_crate::Trait::trait_deprecated`"]),
            warning(333, &["`this_crate::Trait::trait_deprecated`"]),
            warning(337, &["`this_crate::MethodTester::method_deprecated_text`"]),
            warning(338, &["`this_crate::MethodTester::method_deprecated_text`"]),
            warning(339, &["`this_crate::MethodTester::method_deprecated_text`"]),
            warning(340, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(342, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(383, &["`this_crate::DeprecatedStruct::i`"]),
            warning(402, &["`this_crate::Trait::trait_deprecated`"]),
            warning(404, &["`this_crate::Trait::trait_deprecated`"]),
            warning(406, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(408, &["`this_crate::Trait::trait_deprecated_text`"]),
            warning(425, &["`this_crate::Trait::trait_deprecated`"]),
            warning(426, &["`this_crate::Trait::trait_deprecated_text`"]),
        ],
    );
}

/// `deprecation-sanity.rs`'s `useless_deprecated`, as its `.stderr` has it. The test's other
/// errors are those of the parser of `#[deprecated]`.
#[test]
fn deprecated_on_a_trait_impl_is_useless() {
    let source = include_str!("stability/deprecation-sanity.rs");
    let expected = "error: the `deprecated` attribute cannot be used on trait impl blocks
  --> $DIR/deprecation-sanity.rs:40:3
   |
LL | #[deprecated = \"hello\"]
   |   ^^^^^^^^^^
   |
   = help: the `deprecated` attribute can be applied to associated consts, associated types, constants, crates, data types, enum variants, foreign statics, functions, inherent impl blocks, macro defs, modules, statics, struct fields, traits, type aliases, and use statements
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: `#[deny(useless_deprecated)]` on by default
";
    let stderr = one("deprecation-sanity.rs", source, Mode::Other);
    assert!(stderr.contains(expected), "{stderr}");
}

#[test]
fn deprecated_on_an_item_of_a_trait_impl_has_no_effect() {
    let checked = checked(
        r#"//- /main.rs crate:other:main edition:2021
trait Tr {
    const C: u8;
    type T;
}
struct S;
impl Tr for S {
    #[deprecated]
    const C: u8 = 0;
    #[deprecated]
    type T = ();
}
fn main() {}
"#,
    );
    let messages: Vec<String> =
        checked.messages().into_iter().filter(|it| it.contains("`#[deprecated]` annotation")).collect();
    assert_eq!(
        messages,
        [
            "unused_attributes: this `#[deprecated]` annotation has no effect",
            "unused_attributes: this `#[deprecated]` annotation has no effect",
        ]
    );
}

#[test]
fn a_use_in_a_definition_deprecated_by_the_same_attribute_is_not_linted() {
    let checked = checked(
        r#"//- /main.rs crate:other:main edition:2021
#[deprecated]
mod old {
    pub fn f() {}
    pub fn g() { f(); }
}
#[deprecated(note = "use h")]
fn e() {}
#[deprecated]
fn d() { e(); }
fn main() {
    old::g();
}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("deprecated")).collect();
    assert_eq!(
        messages,
        ["deprecated: use of deprecated function `e`: use h", "deprecated: use of deprecated function `old::g`"]
    );
}

/// `stability-attribute-trait-impl`'s `ineffective_unstable_trait_impl`s and its "module has
/// missing stability attribute" (the attribute checks'), each block as its `.stderr` has it.
/// rustc gives the module's missing stability after the lints of `check_mod_unstable_api_usage`;
/// this checks each block, not that order, which is the driver's.
#[test]
fn an_unstable_trait_impl_of_stable_types_and_traits_has_no_effect() {
    let source = include_str!("stability/stability-attribute-trait-impl.rs");
    let stderr = one("stability-attribute-trait-impl.rs", source, Mode::Other);
    let no_effect = ["`#[unstable]`", "no effect"];
    assert_reports(
        &stderr,
        &[error(25, &no_effect), error(33, &no_effect), error(37, &no_effect), error(44, &no_effect), error(1, &["stability attribute"])],
    );
}

#[test]
fn an_unstable_feature_bound_an_unresolved_trait_and_a_nested_unstable_type_keep_the_lint_off() {
    let checked = checked(
        r#"//- /main.rs crate:other:main edition:2021
#![feature(staged_api)]
#![stable(feature = "main", since = "1.0.0")]
#[stable(feature = "a", since = "1.0.0")]
pub struct S;
#[stable(feature = "a", since = "1.0.0")]
pub struct W<X>(X);
#[unstable(feature = "b", issue = "none")]
pub struct U;
#[stable(feature = "c", since = "1.0.0")]
pub trait T {}
#[unstable_feature_bound(d)]
#[unstable(feature = "d", issue = "none")]
impl T for S {}
#[unstable(feature = "e", issue = "none")]
impl Unresolved for S {}
#[unstable(feature = "f", issue = "none")]
impl T for W<U> {}
#[unstable(feature = "g", issue = "none")]
impl T for W<S> {}
fn main() {}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.contains("has no effect")).collect();
    assert_eq!(messages, ["ineffective_unstable_trait_impl: an `#[unstable]` annotation here has no effect"]);
}

#[test]
fn typeck_checks_a_receiver_before_its_method_and_an_initializer_before_its_pattern() {
    let checked = checked(
        r#"//- /main.rs crate:other:main edition:2021
pub struct P(#[deprecated] pub u8);
pub struct T;
impl T {
    #[deprecated]
    pub fn a(&self) -> T { T }
    #[deprecated]
    pub fn b(&self) -> P { P(0) }
}
fn main() {
    let P(_x) = T.a().b();
}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("deprecated")).collect();
    assert_eq!(
        messages,
        [
            "deprecated: use of deprecated method `T::a`",
            "deprecated: use of deprecated method `T::b`",
            "deprecated: use of deprecated field `P::0`",
        ]
    );
}

#[test]
fn what_a_macro_expands_to_is_checked_where_it_is_called() {
    let checked = checked(
        r#"//- /main.rs crate:other:main edition:2021
#[deprecated]
pub fn d1() {}
#[deprecated]
pub fn d2() {}
#[deprecated]
pub fn d3() {}
macro_rules! call {
    () => {
        d2()
    };
}
fn main() {
    d1();
    call!();
    d3();
}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("deprecated")).collect();
    assert_eq!(
        messages,
        [
            "deprecated: use of deprecated function `d1`",
            "deprecated: use of deprecated function `d2`",
            "deprecated: use of deprecated function `d3`",
        ]
    );
}

/// A feature name is an identifier as rustc's `is_ident` has it (XID), not only letters.
#[test]
fn a_feature_named_with_any_xid_identifier_gates_its_item() {
    let checked = checked(
        r#"//- /dep.rs crate:dep edition:2021 library
#![feature(staged_api)]
#![stable(feature = "dep", since = "1.0.0")]
#[unstable(feature = "fé", issue = "none")]
pub fn f() {}
//- /main.rs crate:other:main edition:2021 deps:dep
fn main() {
    dep::f();
}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("E0658")).collect();
    assert_eq!(messages, ["E0658: use of unstable library feature `fé`"]);
}

/// `since = "CURRENT_RUSTC_VERSION"` in a staged crate is the graph's toolchain release
/// (`1.76.0` in a fixture), so the deprecation is in effect: `deprecated`, not
/// `deprecated_in_future`.
#[test]
fn a_deprecation_since_the_current_rustc_version_is_in_effect() {
    let checked = checked(
        r#"//- /dep.rs crate:dep edition:2021 library
#![feature(staged_api)]
#![stable(feature = "dep", since = "1.0.0")]
#[stable(feature = "a", since = "1.0.0")]
#[deprecated(since = "CURRENT_RUSTC_VERSION", note = "n")]
pub fn f() {}
//- /main.rs crate:other:main edition:2021 deps:dep
fn main() {
    dep::f();
}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("deprecated")).collect();
    assert_eq!(messages, ["deprecated: use of deprecated function `dep::f`: n"]);
}
