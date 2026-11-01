//! The lints of `#[doc]` and doc comments (`invalid_doc_attributes`, `unused_attributes` of
//! `#[doc(alias)]`, `unused_doc_comments`) against rustc's UI tests (`tests/ui/lint/unused`,
//! `tests/ui/attributes`, `tests/ui/rustdoc`) and rustdoc's (`tests/rustdoc-ui`), whose lints
//! rustc gives too. A test whose `.stderr` has more than these lints, or what only rustdoc
//! says, is compared on the one lint.

use crate::{
    Options,
    emit::{self, Style},
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized},
};

/// What rustc writes compiling the UI test `source`, named `$DIR/<name>`, of edition 2015.
fn ui(name: &str, source: &str) -> String {
    crate::tests::ui(name, "2015", source)
}

/// What rustc writes of the lint `lint` compiling the UI test `source` as [`ui`] does: its
/// diagnostics alone.
fn ui_lint(name: &str, source: &str, lint: &str) -> String {
    let fixture = format!("//- /{name} crate:other:main edition:2015\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    let sources = checked.sources();
    let style = Style { ui_testing: true, ..Style::default() };
    let written: String = checked
        .diags
        .iter()
        .filter(|it| it.code.as_ref().is_some_and(|code| code.as_str() == lint))
        .map(|it| emit::written(it, &sources, style).unwrap())
        .collect();
    normalized(&written)
}

#[test]
fn useless_comments_are_linted() {
    let source = r#"#![feature(stmt_expr_attributes)]

#![deny(unused_doc_comments)]
#![deny(unused_attributes)]

macro_rules! mac {
    () => {}
}

/// foo //~ ERROR unused doc comment
mac!();

/// a //~ ERROR unused doc comment
#[doc(test(attr(allow(dead_code))))] //~ ERROR unused doc comment
unsafe extern "C" { }

fn foo() {
    /// a //~ ERROR unused doc comment
    #[doc(test(attr(allow(dead_code))))]
    //~^ ERROR unused doc comment
    let x = 12;

    /// multi-line //~ ERROR unused doc comment
    /// doc comment
    /// that is unused
    match x {
        /// c //~ ERROR unused doc comment
        1 => {},
        #[doc(test(attr(allow(dead_code))))]
        //~^ ERROR unused doc comment
        _ => {}
    }

    /// foo //~ ERROR unused doc comment
    unsafe {}

    #[doc = "foo"] //~ ERROR unused doc comment
    #[doc = "bar"] //~ ERROR unused doc comment
    3;

    /// bar //~ ERROR unused doc comment
    mac!();

    #[doc(test(attr(allow(dead_code))))]
    //~^ ERROR unused doc comment
    let x = /** comment */ 47; //~ ERROR unused doc comment

    /// dox //~ ERROR unused doc comment
    {

    }
}

fn main() {
    foo();
}
"#;
    let expected = r#"error: unused doc comment
  --> $DIR/useless-comment.rs:10:1
   |
LL | /// foo
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ rustdoc does not generate documentation for macro invocations
   |
   = help: to document an item produced by a macro, the macro must produce the documentation as part of its expansion
note: the lint level is defined here
  --> $DIR/useless-comment.rs:3:9
   |
LL | #![deny(unused_doc_comments)]
   |         ^^^^^^^^^^^^^^^^^^^

error: unused doc comment
  --> $DIR/useless-comment.rs:13:1
   |
LL | /// a
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL | #[doc(test(attr(allow(dead_code))))]
LL | unsafe extern "C" { }
   | --------------------- rustdoc does not generate documentation for extern blocks
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:14:1
   |
LL | #[doc(test(attr(allow(dead_code))))]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL | unsafe extern "C" { }
   | --------------------- rustdoc does not generate documentation for extern blocks
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:41:5
   |
LL |     /// bar
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ rustdoc does not generate documentation for macro invocations
   |
   = help: to document an item produced by a macro, the macro must produce the documentation as part of its expansion

error: unused doc comment
  --> $DIR/useless-comment.rs:18:5
   |
LL |     /// a
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
...
LL |     let x = 12;
   |     ----------- rustdoc does not generate documentation for statements
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:19:5
   |
LL |     #[doc(test(attr(allow(dead_code))))]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL |
LL |     let x = 12;
   |     ----------- rustdoc does not generate documentation for statements
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:23:5
   |
LL | /     /// multi-line
LL | |     /// doc comment
LL | |     /// that is unused
   | |______________________^
LL | /     match x {
LL | |         /// c
LL | |         1 => {},
LL | |         #[doc(test(attr(allow(dead_code))))]
LL | |
LL | |         _ => {}
LL | |     }
   | |_____- rustdoc does not generate documentation for expressions
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:27:9
   |
LL |         /// c
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL |         1 => {},
   |         ------- rustdoc does not generate documentation for match arms
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:29:9
   |
LL |         #[doc(test(attr(allow(dead_code))))]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL |
LL |         _ => {}
   |         ------- rustdoc does not generate documentation for match arms
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:34:5
   |
LL |     /// foo
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL |     unsafe {}
   |     --------- rustdoc does not generate documentation for expressions
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:37:5
   |
LL |     #[doc = "foo"]
   |     ^^^^^^^^^^^^^^
LL |     #[doc = "bar"]
LL |     3;
   |     - rustdoc does not generate documentation for expressions
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:38:5
   |
LL |     #[doc = "bar"]
   |     ^^^^^^^^^^^^^^
LL |     3;
   |     - rustdoc does not generate documentation for expressions
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:44:5
   |
LL |     #[doc(test(attr(allow(dead_code))))]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL |
LL |     let x = /** comment */ 47;
   |     -------------------------- rustdoc does not generate documentation for statements
   |
   = help: use `//` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:46:13
   |
LL |     let x = /** comment */ 47;
   |             ^^^^^^^^^^^^^^ -- rustdoc does not generate documentation for expressions
   |
   = help: use `/* */` for a plain comment

error: unused doc comment
  --> $DIR/useless-comment.rs:48:5
   |
LL |       /// dox
   |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL | /     {
LL | |
LL | |     }
   | |_____- rustdoc does not generate documentation for expressions
   |
   = help: use `//` for a plain comment

error: aborting due to 15 previous errors

"#;
    assert_eq!(ui("useless-comment.rs", source), expected);
}

#[test]
fn a_run_of_doc_comments_on_a_macro_call_is_one_lint() {
    let source = r#"#![deny(unused_doc_comments)]
#![feature(rustc_attrs)]

macro_rules! foo { () => {}; }

fn main() {
    /// line1 //~ ERROR: unused doc comment
    /// line2
    /// line3
    foo!();

    // Ensure we still detect another doc-comment block.
    /// line1 //~ ERROR: unused doc comment
    /// line2
    /// line3
    foo!();
}
"#;
    let expected = r#"error: unused doc comment
  --> $DIR/unused-doc-comments-for-macros.rs:7:5
   |
LL | /     /// line1
LL | |     /// line2
LL | |     /// line3
   | |_____--------^
   |       |
   |       rustdoc does not generate documentation for macro invocations
   |
   = help: to document an item produced by a macro, the macro must produce the documentation as part of its expansion
note: the lint level is defined here
  --> $DIR/unused-doc-comments-for-macros.rs:1:9
   |
LL | #![deny(unused_doc_comments)]
   |         ^^^^^^^^^^^^^^^^^^^

error: unused doc comment
  --> $DIR/unused-doc-comments-for-macros.rs:13:5
   |
LL | /     /// line1
LL | |     /// line2
LL | |     /// line3
   | |_____--------^
   |       |
   |       rustdoc does not generate documentation for macro invocations
   |
   = help: to document an item produced by a macro, the macro must produce the documentation as part of its expansion

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("unused-doc-comments-for-macros.rs", source), expected);
}

#[test]
fn doc_comments_on_fields_generics_blocks_and_extern_blocks_are_linted() {
    let source = r#"#![deny(unused_doc_comments)]

fn doc_comment_on_match_arms(num: u8) -> bool {
    match num {
        3 => true,
        /// useless doc comment
        //~^ ERROR: unused doc comment
        _ => false,
    }
}

fn doc_comment_between_if_else(num: u8) -> bool {
    if num == 3 {
        true //~ ERROR: mismatched types
    }
    /// useless doc comment
    else { //~ ERROR: expected expression, found keyword `else`
        false
    }
}

fn doc_comment_on_expr(num: u8) -> bool {
    /// useless doc comment
    //~^ ERROR: attributes on expressions are experimental
    //~| ERROR: unused doc comment
    num == 3
}

fn doc_comment_on_expr_field() -> bool {
    struct S { foo: i32 }

    let x = S {
        /// useless doc comment
        //~^ ERROR: unused doc comment
        foo: 3
    };

    true
}

fn doc_comment_on_pat_field() -> bool {
    struct S { foo: i32 }

    let S {
        /// useless doc comment
        //~^ ERROR: unused doc comment
        foo
    } = S {
        foo: 3
    };

    true
}

fn doc_comment_on_generic<#[doc = "x"] T>(val: T) {}
//~^ ERROR: unused doc comment

fn doc_comment_on_block() {
    /// unused doc comment
    //~^ ERROR: unused doc comment
    {
        let x = 12;
    }
}

/// unused doc comment
//~^ ERROR: unused doc comment
extern "C" {
    fn foo();
}

fn main() {}
"#;
    assert_reports(
        &ui_lint("unused-doc-comments-edge-cases.rs", source, "unused_doc_comments"),
        &[error(6, &[]), error(23, &[]), error(33, &[]), error(45, &[]), error(55, &[]), error(59, &[]), error(66, &[])],
    );
}

#[test]
fn unknown_doc_attributes_and_literals_are_linted() {
    let source = r#"#![deny(invalid_doc_attributes)]
#![crate_type = "lib"]
#![doc(as_ptr)]
//~^ ERROR unknown `doc` attribute

#[doc(as_ptr)]
//~^ ERROR unknown `doc` attribute
pub fn foo() {}

#[doc(123)]
//~^ ERROR
//~| WARN
#[doc("hello", "bar")]
//~^ ERROR
//~| ERROR
//~| WARN
//~| WARN
#[doc(foo::bar, crate::bar::baz = "bye")]
//~^ ERROR unknown `doc` attribute
//~| ERROR unknown `doc` attribute
fn bar() {}
"#;
    assert_reports(
        &ui("doc-attr.rs", source),
        &[
            error(3, &["`as_ptr`", "`doc`"]),
            error(6, &["`as_ptr`", "`doc`"]),
            error(10, &["`... = \"...\"`"]),
            error(13, &["`... = \"...\"`"]),
            error(13, &["`... = \"...\"`"]),
            error(18, &["`doc`", "`foo::bar`"]),
            error(18, &["`crate::bar::baz`", "`doc`"]),
        ],
    );
}

#[test]
fn a_literal_in_doc_test_is_malformed() {
    let source = r#"#![deny(invalid_doc_attributes)]

#![doc(test(""))]
//~^ ERROR
//~| WARN

fn main() {}
"#;
    let expected = r#"error: malformed `doc` attribute input
  --> $DIR/doc-test-literal.rs:3:13
   |
LL | #![doc(test(""))]
   |             ^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
note: the lint level is defined here
  --> $DIR/doc-test-literal.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("doc-test-literal.rs", source), expected);
}

#[test]
fn an_unknown_crate_doc_attribute_is_denied() {
    let source = r#"#![deny(invalid_doc_attributes)]
//~^ NOTE defined here
#![doc(x)]
//~^ ERROR unknown `doc` attribute `x`
fn main() {}
"#;
    let expected = r#"error: unknown `doc` attribute `x`
  --> $DIR/deny-invalid-doc-attrs.rs:3:8
   |
LL | #![doc(x)]
   |        ^
   |
note: the lint level is defined here
  --> $DIR/deny-invalid-doc-attrs.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("deny-invalid-doc-attrs.rs", source), expected);
}

#[test]
fn a_duplicate_doc_alias_is_an_unused_attribute() {
    let source = r#"#![deny(unused_attributes)]

#[doc(alias = "A")]
#[doc(alias = "A")] //~ ERROR
#[doc(alias = "B")]
#[doc(alias("B"))] //~ ERROR
pub struct Foo;

fn main() {}
"#;
    let expected = r#"error: doc alias is duplicated
  --> $DIR/duplicate_doc_alias.rs:4:15
   |
LL | #[doc(alias = "A")]
   |               --- first defined here
LL | #[doc(alias = "A")]
   |               ^^^
   |
note: the lint level is defined here
  --> $DIR/duplicate_doc_alias.rs:1:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: doc alias is duplicated
  --> $DIR/duplicate_doc_alias.rs:6:13
   |
LL | #[doc(alias = "B")]
   |               --- first defined here
LL | #[doc(alias("B"))]
   |             ^^^

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("duplicate_doc_alias.rs", source), expected);
}

#[test]
fn doc_test_takes_a_list_of_known_attributes() {
    let source = r#"#![crate_type = "lib"]
#![deny(invalid_doc_attributes)]

#![doc(test)]
//~^ ERROR `#[doc(test(...)]` takes a list of attributes
#![doc(test = "hello")]
//~^ ERROR `#[doc(test(...)]` takes a list of attributes
#![doc(test(a))]
//~^ ERROR unknown `doc(test)` attribute `a`

pub fn foo() {}
"#;
    let expected = r#"error: `#[doc(test(...)]` takes a list of attributes
  --> $DIR/doc-test-attr.rs:4:8
   |
LL | #![doc(test)]
   |        ^^^^
   |
note: the lint level is defined here
  --> $DIR/doc-test-attr.rs:2:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: `#[doc(test(...)]` takes a list of attributes
  --> $DIR/doc-test-attr.rs:6:13
   |
LL | #![doc(test = "hello")]
   |             ^^^^^^^^^

error: unknown `doc(test)` attribute `a`
  --> $DIR/doc-test-attr.rs:8:13
   |
LL | #![doc(test(a))]
   |             ^

error: aborting due to 3 previous errors

"#;
    assert_eq!(ui("doc-test-attr.rs", source), expected);
}

#[test]
fn no_crate_inject_is_only_for_the_crate() {
    let source = r#"#![deny(invalid_doc_attributes)]
#![crate_type = "lib"]

#[doc(test(no_crate_inject))]
//~^ ERROR can only be applied at the crate level

pub mod bar {
    #![doc(test(no_crate_inject))]
    //~^ ERROR can only be applied at the crate level

    #[doc(test(no_crate_inject))]
    //~^ ERROR can only be applied at the crate level
    fn foo() {}
}
"#;
    let expected = r#"error: this attribute can only be applied at the crate level
  --> $DIR/invalid-crate-level-lint.rs:4:12
   |
LL | #[doc(test(no_crate_inject))]
   |            ^^^^^^^^^^^^^^^
   |
   = note: read <https://doc.rust-lang.org/nightly/rustdoc/the-doc-attribute.html#at-the-crate-level> for more information
note: the lint level is defined here
  --> $DIR/invalid-crate-level-lint.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: this attribute can only be applied at the crate level
  --> $DIR/invalid-crate-level-lint.rs:8:17
   |
LL |     #![doc(test(no_crate_inject))]
   |                 ^^^^^^^^^^^^^^^
   |
   = note: read <https://doc.rust-lang.org/nightly/rustdoc/the-doc-attribute.html#at-the-crate-level> for more information

error: this attribute can only be applied at the crate level
  --> $DIR/invalid-crate-level-lint.rs:11:16
   |
LL |     #[doc(test(no_crate_inject))]
   |                ^^^^^^^^^^^^^^^
   |
   = note: read <https://doc.rust-lang.org/nightly/rustdoc/the-doc-attribute.html#at-the-crate-level> for more information

error: aborting due to 3 previous errors

"#;
    assert_eq!(ui("invalid-crate-level-lint.rs", source), expected);
}

#[test]
fn a_bare_doc_lists_the_valid_forms() {
    let source = r#"#![deny(invalid_doc_attributes)]

#![doc("other attribute")]
//~^ ERROR
//~| WARN
#![doc]
//~^ ERROR
"#;
    let expected = r#"error: expected this to be of the form `... = "..."`
  --> $DIR/invalid-doc-attr-2.rs:3:8
   |
LL | #![doc("other attribute")]
   |        ^^^^^^^^^^^^^^^^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
note: the lint level is defined here
  --> $DIR/invalid-doc-attr-2.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: valid forms for the attribute are `doc = "string"`, `doc(alias)`, `doc(attribute)`, `doc(auto_cfg)`, `doc(cfg)`, `doc(fake_variadic)`, `doc(hidden)`, `doc(html_favicon_url)`, `doc(html_logo_url)`, `doc(html_no_source)`, `doc(html_playground_url)`, `doc(html_root_url)`, `doc(include)`, `doc(inline)`, `doc(issue_tracker_base_url)`, `doc(keyword)`, `doc(masked)`, `doc(no_default_passes)`, `doc(no_inline)`, `doc(notable_trait)`, `doc(passes)`, `doc(plugins)`, `doc(rust_logo)`, `doc(search_unbox)`, `doc(spotlight)`, and `doc(test)`
  --> $DIR/invalid-doc-attr-2.rs:6:4
   |
LL | #![doc]
   |    ^^^

"#;
    assert_eq!(ui_lint("invalid-doc-attr-2.rs", source, "invalid_doc_attributes"), expected);
}

#[test]
fn arguments_where_none_are_taken_are_linted() {
    let source = r#"#![deny(invalid_doc_attributes)]

#![doc(test(no_crate_inject = 1))]
//~^ ERROR
//~| WARN
#![doc(test(attr = 1))]
//~^ ERROR
//~| WARN

#[doc(hidden = true)]
//~^ ERROR
//~| WARN
#[doc(hidden("or you will be fired"))]
//~^ ERROR
//~| WARN
#[doc(hidden = "handled transparently by codegen")]
//~^ ERROR
//~| WARN
#[doc = 1]
//~^ ERROR
//~| WARN
pub struct X;
"#;
    assert_reports(
        &ui_lint("invalid-doc-attr-3.rs", source, "invalid_doc_attributes"),
        &[
            error(3, &[]),
            error(6, &["`doc`"]),
            error(10, &[]),
            error(13, &[]),
            error(16, &[]),
            error(19, &["`doc`"]),
        ],
    );
}

#[test]
fn inline_and_masked_off_their_items_are_linted() {
    let source = r#"#![deny(invalid_doc_attributes)]
#![crate_type = "lib"]
#![feature(doc_masked)]

#![doc(masked)]
//~^ ERROR this attribute can only be applied to an `extern crate` item

#[doc(inline)]
//~^ ERROR can only be applied to a `use` item
pub fn foo() {}

pub mod bar {
    #[doc(inline)]
    //~^ ERROR can only be applied to a `use` item
    pub fn baz() {}
}

#[doc(inline)]
#[doc(no_inline)]
//~^^ ERROR conflicting doc inlining attributes
//~|  HELP remove one of the conflicting attributes
pub use bar::baz;

#[doc(masked)]
//~^ ERROR this attribute can only be applied to an `extern crate` item
pub struct Masked;

#[doc(masked)]
//~^ ERROR this attribute cannot be applied to an `extern crate self` item
pub extern crate self as reexport;
"#;
    assert_reports(
        &ui("invalid-doc-attr.rs", source),
        &[
            error(8, &["`use`"]),
            error(18, &[]),
            error(24, &["`extern crate`"]),
            error(28, &["`extern crate self`"]),
            error(5, &["`extern crate`"]),
            error(13, &["`use`"]),
        ],
    );
}

#[test]
fn spotlight_suggests_notable_trait() {
    let source = r#"//@ run-rustfix
#![feature(doc_notable_trait)]
#![deny(invalid_doc_attributes)]

#[doc(spotlight)]
//~^ ERROR unknown `doc` attribute `spotlight`
trait MyTrait {}
"#;
    let expected = r#"error: unknown `doc` attribute `spotlight`
  --> $DIR/doc-spotlight.rs:5:7
   |
LL | #[doc(spotlight)]
   |       ^^^^^^^^^ help: use `notable_trait` instead
   |
   = note: `doc(spotlight)` was renamed to `doc(notable_trait)`
   = note: `doc(spotlight)` is now a no-op
note: the lint level is defined here
  --> $DIR/doc-spotlight.rs:3:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

"#;
    assert_eq!(ui_lint("doc-spotlight.rs", source, "invalid_doc_attributes"), expected);
}

#[test]
fn include_suggests_include_str() {
    let source = r#"#![deny(invalid_doc_attributes)]
//~^ NOTE

#[doc(include = "external-cross-doc.md")]
//~^ ERROR unknown `doc` attribute `include`
//~| HELP use `doc = include_str!` instead
// FIXME(#85497): make this a deny instead so it's more clear what's happening
pub struct NeedMoreDocs;
"#;
    let expected = r#"error: unknown `doc` attribute `include`
  --> $DIR/doc-include-suggestion.rs:4:7
   |
LL | #[doc(include = "external-cross-doc.md")]
   |       ^^^^^^^ help: use `doc = include_str!` instead: `#[doc = include_str!("external-cross-doc.md")]`
   |
note: the lint level is defined here
  --> $DIR/doc-include-suggestion.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

"#;
    assert_eq!(ui_lint("doc-include-suggestion.rs", source, "invalid_doc_attributes"), expected);
}

#[test]
fn passes_and_plugins_no_longer_function() {
    let source = r#"//@ compile-flags: --passes unknown-pass

#![deny(invalid_doc_attributes)]
//~^ NOTE

#![doc(no_default_passes)]
//~^ ERROR unknown `doc` attribute `no_default_passes`
//~| NOTE no longer functions
//~| NOTE see issue #44136
//~| NOTE `doc(no_default_passes)` is now a no-op
#![doc(passes = "collapse-docs unindent-comments")]
//~^ ERROR unknown `doc` attribute `passes`
//~| NOTE no longer functions
//~| NOTE see issue #44136
//~| NOTE `doc(passes)` is now a no-op
#![doc(plugins = "xxx")]
//~^ ERROR unknown `doc` attribute `plugins`
//~| NOTE see issue #44136
//~| NOTE no longer functions
//~| NOTE `doc(plugins)` is now a no-op

//~? WARN the `passes` flag no longer functions
//~? NOTE see issue #44136
//~? HELP you may want to use --document-private-items
"#;
    let expected = r#"error: unknown `doc` attribute `no_default_passes`
  --> $DIR/deprecated-attrs.rs:6:8
   |
LL | #![doc(no_default_passes)]
   |        ^^^^^^^^^^^^^^^^^ no longer functions
   |
   = note: `doc` attribute `no_default_passes` no longer functions; see issue #44136 <https://github.com/rust-lang/rust/issues/44136>
   = note: `doc(no_default_passes)` is now a no-op
note: the lint level is defined here
  --> $DIR/deprecated-attrs.rs:3:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: unknown `doc` attribute `passes`
  --> $DIR/deprecated-attrs.rs:11:8
   |
LL | #![doc(passes = "collapse-docs unindent-comments")]
   |        ^^^^^^ no longer functions
   |
   = note: `doc` attribute `passes` no longer functions; see issue #44136 <https://github.com/rust-lang/rust/issues/44136>
   = note: `doc(passes)` is now a no-op

error: unknown `doc` attribute `plugins`
  --> $DIR/deprecated-attrs.rs:16:8
   |
LL | #![doc(plugins = "xxx")]
   |        ^^^^^^^ no longer functions
   |
   = note: `doc` attribute `plugins` no longer functions; see issue #44136 <https://github.com/rust-lang/rust/issues/44136> and CVE-2018-1000622 <https://nvd.nist.gov/vuln/detail/CVE-2018-1000622>
   = note: `doc(plugins)` is now a no-op

"#;
    assert_eq!(ui_lint("deprecated-attrs.rs", source, "invalid_doc_attributes"), expected);
}

#[test]
fn a_duplicate_no_crate_inject_is_unused() {
    let source = r#"#![deny(invalid_doc_attributes)]
#![expect(unused_attributes)]
#![doc(test(no_crate_inject))]
#![doc(test(no_crate_inject))]
//~^ ERROR
//~| WARN
"#;
    let expected = r#"error: unused attribute
  --> $DIR/duplicated-attr.rs:4:13
   |
LL | #![doc(test(no_crate_inject))]
   |             ^^^^^^^^^^^^^^^ help: remove this attribute
   |
note: attribute also specified here
  --> $DIR/duplicated-attr.rs:3:13
   |
LL | #![doc(test(no_crate_inject))]
   |             ^^^^^^^^^^^^^^^
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
note: the lint level is defined here
  --> $DIR/duplicated-attr.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

"#;
    assert_eq!(ui_lint("duplicated-attr.rs", source, "invalid_doc_attributes"), expected);
}

#[test]
fn auto_cfg_takes_hide_and_show_of_identifiers() {
    let source = r#"#![deny(invalid_doc_attributes)]
#![feature(doc_cfg)]

#[doc(cfg(foo), cfg(bar))]
//~^ WARN unexpected `cfg` condition name: `foo`
//~| WARN unexpected `cfg` condition name: `bar`
#[doc(auto_cfg(42))] //~ ERROR
#[doc(auto_cfg(hide(true)))] //~ ERROR
#[doc(auto_cfg(hide(42)))] //~ ERROR
#[doc(auto_cfg(hide("a")))] //~ ERROR
#[doc(auto_cfg = 42)] //~ ERROR
#[doc(auto_cfg = "a")] //~ ERROR
// Shouldn't lint
#[doc(auto_cfg(hide(windows)))]
#[doc(auto_cfg(hide(feature = "windows")))]
//~^ ERROR `#![doc(auto_cfg(hide(...)))]` only accepts identifiers or `values(...)`
pub fn foo() {}
"#;
    let expected = r#"error: only `hide` or `show` are allowed in `#[doc(auto_cfg(...))]`
  --> $DIR/doc-cfg-2.rs:7:16
   |
LL | #[doc(auto_cfg(42))]
   |                ^^
   |
note: the lint level is defined here
  --> $DIR/doc-cfg-2.rs:1:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: `#![doc(auto_cfg(hide(...)))]` only accepts identifiers or `values(...)`
  --> $DIR/doc-cfg-2.rs:8:21
   |
LL | #[doc(auto_cfg(hide(true)))]
   |                     ^^^^

error: `#![doc(auto_cfg(hide(...)))]` only accepts identifiers or `values(...)`
  --> $DIR/doc-cfg-2.rs:9:21
   |
LL | #[doc(auto_cfg(hide(42)))]
   |                     ^^

error: `#![doc(auto_cfg(hide(...)))]` only accepts identifiers or `values(...)`
  --> $DIR/doc-cfg-2.rs:10:21
   |
LL | #[doc(auto_cfg(hide("a")))]
   |                     ^^^

error: expected boolean for `#[doc(auto_cfg = ...)]`
  --> $DIR/doc-cfg-2.rs:11:18
   |
LL | #[doc(auto_cfg = 42)]
   |                  ^^

error: expected boolean for `#[doc(auto_cfg = ...)]`
  --> $DIR/doc-cfg-2.rs:12:18
   |
LL | #[doc(auto_cfg = "a")]
   |                  ^^^

error: `#![doc(auto_cfg(hide(...)))]` only accepts identifiers or `values(...)`
  --> $DIR/doc-cfg-2.rs:15:21
   |
LL | #[doc(auto_cfg(hide(feature = "windows")))]
   |                     ^^^^^^^^^^^^^^^^^^^

"#;
    assert_eq!(ui_lint("doc-cfg-2.rs", source, "invalid_doc_attributes"), expected);
}

#[test]
fn any_with_other_values_is_linted() {
    let source = r#"// Checks that you cannot have `any()` and values at the same time.

#![deny(invalid_doc_attributes)]
#![feature(doc_cfg)]

#![doc(auto_cfg(hide(target_os, values(any(), "linux"))))] //~ ERROR
#![doc(auto_cfg(hide(target_os, values("linux", any()))))] //~ ERROR
"#;
    let expected = r#"error: `any()` was used when other values were provided
  --> $DIR/doc-cfg-3.rs:6:40
   |
LL | #![doc(auto_cfg(hide(target_os, values(any(), "linux"))))]
   |                                        ^^^^^  ------- value declared here
   |
note: the lint level is defined here
  --> $DIR/doc-cfg-3.rs:3:9
   |
LL | #![deny(invalid_doc_attributes)]
   |         ^^^^^^^^^^^^^^^^^^^^^^

error: `any()` was used when other values were provided
  --> $DIR/doc-cfg-3.rs:7:49
   |
LL | #![doc(auto_cfg(hide(target_os, values("linux", any()))))]
   |                                        -------  ^^^^^
   |                                        |
   |                                        value declared here

"#;
    assert_eq!(ui_lint("doc-cfg-3.rs", source, "invalid_doc_attributes"), expected);
}
