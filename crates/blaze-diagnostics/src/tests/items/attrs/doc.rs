//! rustc's UI tests of the errors of the `#[doc]` parser and of `check_doc_attrs`, and that
//! the diagnostic attributes' parsers give none. The `tests/rustdoc-ui/` ones are rustdoc's,
//! whose errors come from the attribute parser and `check_mod_attrs` rustc shares; the ones
//! without a path are ours, of what no UI test covers.

use crate::tests::{assert_reports, error, ui};

#[test]
fn an_alias_is_a_string_of_no_quote_and_no_whitespace_but_inner_spaces() {
    // tests/ui/rustdoc/check-doc-alias-attr.rs
    let source = r##"#![crate_type = "lib"]

#[doc(alias = "foo")] // ok!
#[doc(alias("bar", "baz"))] // ok!
pub struct Bar;

#[doc(alias)] //~ ERROR
#[doc(alias = 0)] //~ ERROR
#[doc(alias = "\"")] //~ ERROR
#[doc(alias = "\n")] //~ ERROR
#[doc(alias = "
")] //~^ ERROR
#[doc(alias = "\t")] //~ ERROR
#[doc(alias = " hello")] //~ ERROR
#[doc(alias = "hello ")] //~ ERROR
#[doc(alias = "")] //~ ERROR
pub struct Foo;

#[doc(alias(0))] //~ ERROR
#[doc(alias("\""))] //~ ERROR
#[doc(alias("\n"))] //~ ERROR
#[doc(alias("
"))] //~^ ERROR
#[doc(alias("\t"))] //~ ERROR
#[doc(alias(" hello"))] //~ ERROR
#[doc(alias("hello "))] //~ ERROR
#[doc(alias(""))] //~ ERROR
pub struct Foo2;
"##;
    let stderr = r##"error: doc alias attribute expects a string `#[doc(alias = "a")]` or a list of strings `#[doc(alias("a", "b"))]`
  --> $DIR/check-doc-alias-attr.rs:7:7
   |
LL | #[doc(alias)]
   |       ^^^^^

error[E0539]: malformed `doc` attribute input
  --> $DIR/check-doc-alias-attr.rs:8:3
   |
LL | #[doc(alias = 0)]
   |   ^^^^^^^^^^^^-^
   |               |
   |               expected a string literal here

error: '"' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:9:15
   |
LL | #[doc(alias = "\"")]
   |               ^^^^

error: '\n' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:10:15
   |
LL | #[doc(alias = "\n")]
   |               ^^^^

error: '\n' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:11:15
   |
LL |   #[doc(alias = "
   |  _______________^
LL | | ")]
   | |_^

error: '\t' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:13:15
   |
LL | #[doc(alias = "\t")]
   |               ^^^^

error: `#[doc(alias = "...")]` cannot start or end with ' '
  --> $DIR/check-doc-alias-attr.rs:14:15
   |
LL | #[doc(alias = " hello")]
   |               ^^^^^^^^

error: `#[doc(alias = "...")]` cannot start or end with ' '
  --> $DIR/check-doc-alias-attr.rs:15:15
   |
LL | #[doc(alias = "hello ")]
   |               ^^^^^^^^

error: `#[doc(alias = "...")]` attribute cannot have empty value
  --> $DIR/check-doc-alias-attr.rs:16:15
   |
LL | #[doc(alias = "")]
   |               ^^

error[E0539]: malformed `doc` attribute input
  --> $DIR/check-doc-alias-attr.rs:19:3
   |
LL | #[doc(alias(0))]
   |   ^^^^^^^^^^-^^
   |             |
   |             expected a string literal here

error: '"' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:20:13
   |
LL | #[doc(alias("\""))]
   |             ^^^^

error: '\n' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:21:13
   |
LL | #[doc(alias("\n"))]
   |             ^^^^

error: '\n' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:22:13
   |
LL |   #[doc(alias("
   |  _____________^
LL | | "))]
   | |_^

error: '\t' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/check-doc-alias-attr.rs:24:13
   |
LL | #[doc(alias("\t"))]
   |             ^^^^

error: `#[doc(alias = "...")]` cannot start or end with ' '
  --> $DIR/check-doc-alias-attr.rs:25:13
   |
LL | #[doc(alias(" hello"))]
   |             ^^^^^^^^

error: `#[doc(alias = "...")]` cannot start or end with ' '
  --> $DIR/check-doc-alias-attr.rs:26:13
   |
LL | #[doc(alias("hello "))]
   |             ^^^^^^^^

error: `#[doc(alias = "...")]` attribute cannot have empty value
  --> $DIR/check-doc-alias-attr.rs:27:13
   |
LL | #[doc(alias(""))]
   |             ^^

error: aborting due to 17 previous errors

For more information about this error, try `rustc --explain E0539`.
"##;
    assert_eq!(ui("check-doc-alias-attr.rs", "2015", source), stderr);
}

#[test]
fn an_alias_is_not_of_the_crate() {
    // tests/ui/rustdoc/doc-alias-crate-level.rs
    let source = r##"//@ compile-flags: -Zdeduplicate-diagnostics=no

#![crate_type = "lib"]

#![doc(alias = "not working!")] //~ ERROR

#[doc(alias = "shouldn't work!")] //~ ERROR
pub struct Foo;
"##;
    let stderr = r##"error: `#![doc(alias = "...")]` isn't allowed as a crate-level attribute
  --> $DIR/doc-alias-crate-level.rs:5:16
   |
LL | #![doc(alias = "not working!")]
   |                ^^^^^^^^^^^^^^

error: '\'' character isn't allowed in `#[doc(alias = "...")]`
  --> $DIR/doc-alias-crate-level.rs:7:15
   |
LL | #[doc(alias = "shouldn't work!")]
   |               ^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui("doc-alias-crate-level.rs", "2015", source), stderr);
}

#[test]
fn an_alias_is_not_the_items_name() {
    // tests/ui/rustdoc/doc-alias-same-name.rs
    let source = r##"#![crate_type = "lib"]

#[doc(alias = "Foo")] //~ ERROR
pub struct Foo;
"##;
    let stderr = r##"error: `#[doc(alias = "Foo"]` is the same as the item's name
  --> $DIR/doc-alias-same-name.rs:3:15
   |
LL | #[doc(alias = "Foo")]
   |               ^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("doc-alias-same-name.rs", "2015", source), stderr);
}

#[test]
fn an_alias_is_not_of_an_impl_a_foreign_module_a_statement_an_expression_or_an_arm() {
    // tests/ui/rustdoc/check-doc-alias-attr-location.rs; the parameter's error is
    // `rustc_ast_passes`'.
    let source = r##"#![crate_type = "lib"]

pub struct Bar;
pub trait Foo {
    type X;
    fn foo(x: u32) -> Self::X;
}

#[doc(alias = "foo")] //~ ERROR
extern "C" {}

#[doc(alias = "bar")] //~ ERROR
impl Bar {
    #[doc(alias = "const")]
    const A: u32 = 0;
}

#[doc(alias = "foobar")] //~ ERROR
impl Foo for Bar {
    #[doc(alias = "assoc")] //~ ERROR
    type X = i32;
    fn foo(#[doc(alias = "qux")] _x: u32) -> Self::X {
        //~^ ERROR
        #[doc(alias = "stmt")]
        //~^ ERROR
        let x = 0;
        #[doc(alias = "expr")]
        //~^ ERROR
        match x {
            #[doc(alias = "arm")]
            //~^ ERROR
            _ => 0
        }
    }
}
"##;
    let stderr = r##"error: allow, cfg, cfg_attr, deny, expect, forbid, and warn are the only allowed built-in attributes in function parameters
  --> $DIR/check-doc-alias-attr-location.rs:22:12
   |
LL |     fn foo(#[doc(alias = "qux")] _x: u32) -> Self::X {
   |            ^^^^^^^^^^^^^^^^^^^^^

error: `#[doc(alias = "...")]` isn't allowed on foreign module
  --> $DIR/check-doc-alias-attr-location.rs:9:15
   |
LL | #[doc(alias = "foo")]
   |               ^^^^^

error: `#[doc(alias = "...")]` isn't allowed on implementation block
  --> $DIR/check-doc-alias-attr-location.rs:12:15
   |
LL | #[doc(alias = "bar")]
   |               ^^^^^

error: `#[doc(alias = "...")]` isn't allowed on implementation block
  --> $DIR/check-doc-alias-attr-location.rs:18:15
   |
LL | #[doc(alias = "foobar")]
   |               ^^^^^^^^

error: `#[doc(alias = "...")]` isn't allowed on type alias in implementation block
  --> $DIR/check-doc-alias-attr-location.rs:20:19
   |
LL |     #[doc(alias = "assoc")]
   |                   ^^^^^^^

error: `#[doc(alias = "...")]` isn't allowed on statement
  --> $DIR/check-doc-alias-attr-location.rs:24:23
   |
LL |         #[doc(alias = "stmt")]
   |                       ^^^^^^

error: `#[doc(alias = "...")]` isn't allowed on expression
  --> $DIR/check-doc-alias-attr-location.rs:27:23
   |
LL |         #[doc(alias = "expr")]
   |                       ^^^^^^

error: `#[doc(alias = "...")]` isn't allowed on match arm
  --> $DIR/check-doc-alias-attr-location.rs:30:27
   |
LL |             #[doc(alias = "arm")]
   |                           ^^^^^

error: aborting due to 8 previous errors

"##;
    assert_eq!(ui("check-doc-alias-attr-location.rs", "2015", source), stderr);
}

#[test]
fn an_alias_is_not_of_an_associated_constant_of_a_trait_impl() {
    // tests/rustdoc-ui/doc-alias-assoc-const.rs
    let source = r##"pub struct Foo;

pub trait Bar {
    const BAZ: u8;
}

impl Bar for Foo {
    #[doc(alias = "CONST_BAZ")] //~ ERROR
    const BAZ: u8 = 0;
}

impl Foo {
    #[doc(alias = "CONST_FOO")] // ok!
    pub const FOO: u8 = 0;

    pub fn bar() -> u8 {
        Self::FOO
    }
}
"##;
    let stderr = r##"error: `#[doc(alias = "...")]` isn't allowed on associated constant in trait implementation block
  --> $DIR/doc-alias-assoc-const.rs:8:19
   |
LL |     #[doc(alias = "CONST_BAZ")]
   |                   ^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("doc-alias-assoc-const.rs", "2015", source), stderr);
}

#[test]
fn an_alias_is_not_of_a_where_predicate() {
    // tests/ui/attributes/where-doc.rs; the errors of attributes in `where` are the driver's.
    let source = r##"#![feature(where_clause_attrs)]
#![allow(invalid_doc_attributes)]

fn test()
where
#[doc(alias = ":(")]
//~^ ERROR most attributes are not supported in `where` clauses
//~| ERROR `#[doc(alias = "...")]` isn't allowed on where predicate
():,

#[doc(hidden)]
//~^ ERROR most attributes are not supported in `where` clauses
():,

#[doc = ""]
//~^ ERROR most attributes are not supported in `where` clauses
():,

// == That the doc attributes below don't trigger the error is a bug
#[doc()]
():,

#[doc(5)]
():,

#[doc]
():,

#[doc = 5]
():,

{ }

fn main() {}
"##;
    assert_reports(
        &ui("where-doc.rs", "2015", source),
        &[
            error(6, &[]),
            error(11, &["`where`"]),
            error(15, &["`where`"]),
            error(6, &["`#[doc(alias = \"...\")]`"]),
        ],
    );
}

#[test]
fn a_keyword_is_one_documented_on_an_empty_module() {
    // tests/ui/rustdoc/doc_keyword.rs
    let source = r##"#![crate_type = "lib"]
#![feature(rustdoc_internals)]

#![doc(keyword = "match")]
//~^ ERROR `#![doc(keyword = "...")]` isn't allowed as a crate-level attribute
#[doc(keyword = "match")] //~ ERROR `#[doc(keyword = "...")]` should be used on empty modules
mod foo {
    fn hell() {}
}

#[doc(keyword = "match")] //~ ERROR `#[doc(keyword = "...")]` should be used on modules
fn foo() {}


// Regression test for the ICE described in #83512.
trait Foo {
    #[doc(keyword = "match")]
    //~^ ERROR: `#[doc(keyword = "...")]` should be used on modules
    fn quux() {}
}

#[doc(keyword = "tadam")] //~ ERROR nonexistent keyword `tadam`
mod tadam {}
"##;
    let stderr = r##"error: `#![doc(keyword = "...")]` isn't allowed as a crate-level attribute
  --> $DIR/doc_keyword.rs:4:8
   |
LL | #![doc(keyword = "match")]
   |        ^^^^^^^

error: nonexistent keyword `tadam` used in `#[doc(keyword = "...")]`
  --> $DIR/doc_keyword.rs:22:17
   |
LL | #[doc(keyword = "tadam")]
   |                 ^^^^^^^
   |
   = help: only existing keywords are allowed in core/std

error: `#[doc(keyword = "...")]` should be used on empty modules
  --> $DIR/doc_keyword.rs:6:7
   |
LL | #[doc(keyword = "match")]
   |       ^^^^^^^

error: `#[doc(keyword = "...")]` should be used on modules
  --> $DIR/doc_keyword.rs:11:7
   |
LL | #[doc(keyword = "match")]
   |       ^^^^^^^

error: `#[doc(keyword = "...")]` should be used on modules
  --> $DIR/doc_keyword.rs:17:11
   |
LL |     #[doc(keyword = "match")]
   |           ^^^^^^^

error: aborting due to 5 previous errors

"##;
    assert_eq!(ui("doc_keyword.rs", "2015", source), stderr);
}

#[test]
fn a_keyword_is_a_keyword() {
    // tests/rustdoc-ui/invalid-keyword.rs
    let source = r##"#![feature(rustdoc_internals)]

#[doc(keyword = "foo df")] //~ ERROR
mod foo {}
"##;
    let stderr = r##"error: nonexistent keyword `foo df` used in `#[doc(keyword = "...")]`
  --> $DIR/invalid-keyword.rs:3:17
   |
LL | #[doc(keyword = "foo df")]
   |                 ^^^^^^^^
   |
   = help: only existing keywords are allowed in core/std

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("invalid-keyword.rs", "2015", source), stderr);
}

#[test]
fn an_attribute_is_a_builtin_attribute() {
    // tests/rustdoc-ui/invalid-attribute.rs
    let source = r##"// Testing the output when an invalid builtin attribute is passed as value
// to `doc(attribute = "...")`.

#![feature(rustdoc_internals)]

#[doc(attribute = "foo df")] //~ ERROR
mod foo {}

#[doc(attribute = "fooyi")] //~ ERROR
mod foo2 {}
"##;
    let stderr = r##"error: nonexistent builtin attribute `foo df` used in `#[doc(attribute = "...")]`
  --> $DIR/invalid-attribute.rs:6:19
   |
LL | #[doc(attribute = "foo df")]
   |                   ^^^^^^^^
   |
   = help: only existing builtin attributes are allowed in core/std

error: nonexistent builtin attribute `fooyi` used in `#[doc(attribute = "...")]`
  --> $DIR/invalid-attribute.rs:9:19
   |
LL | #[doc(attribute = "fooyi")]
   |                   ^^^^^^^
   |
   = help: only existing builtin attributes are allowed in core/std

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui("invalid-attribute.rs", "2015", source), stderr);
}

#[test]
fn an_attribute_is_of_one_segment() {
    // tests/rustdoc-ui/doc-attribute-unsupported.rs
    let source = r##"// This is currently not supported but should be!

#![feature(rustdoc_internals)]

#[doc(attribute = "diagnostic::do_not_recommend")] //~ ERROR
/// bla
mod yup {}
"##;
    let stderr = r##"error: nonexistent builtin attribute `diagnostic::do_not_recommend` used in `#[doc(attribute = "...")]`
  --> $DIR/doc-attribute-unsupported.rs:5:19
   |
LL | #[doc(attribute = "diagnostic::do_not_recommend")]
   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: only existing builtin attributes are allowed in core/std

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("doc-attribute-unsupported.rs", "2015", source), stderr);
}

#[test]
fn a_keyword_is_given_once_and_fake_variadic_is_not_of_the_crate() {
    let source = r##"#![feature(rustdoc_internals)]
#![crate_type = "lib"]
#![doc(fake_variadic)] //~ ERROR

#[doc(keyword = "match", keyword = "if")] //~ ERROR
mod match_keyword {}
"##;
    let stderr = r##"error: `#![doc(fake_variadic = "...")]` isn't allowed as a crate-level attribute
  --> $DIR/doc-keyword-duplicate.rs:3:8
   |
LL | #![doc(fake_variadic)]
   |        ^^^^^^^^^^^^^

error[E0538]: malformed `doc` attribute input
  --> $DIR/doc-keyword-duplicate.rs:5:3
   |
LL | #[doc(keyword = "match", keyword = "if")]
   |   ^^^^^^^^^^^^^^^^^^^^^^^-------^^^^^^^^
   |                          |
   |                          found `keyword` used as a key more than once

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0538`.
"##;
    assert_eq!(ui("doc-keyword-duplicate.rs", "2015", source), stderr);
}

#[test]
fn fake_variadic_is_of_the_first_impl_of_a_tuple_or_fn_pointer() {
    // tests/rustdoc-ui/tuple-variadic-check.rs
    let source = r##"#![feature(rustdoc_internals)]

trait Mine {}

// This one is fine
#[doc(fake_variadic)]
impl<T> Mine for (T,) {}

trait Mine2 {}

// This one is not
#[doc(fake_variadic)] //~ ERROR
impl<T, U> Mine for (T,U) {}

fn main() {}
"##;
    let stderr = r##"error: `#[doc(fake_variadic)]` must be used on the first of a set of tuple or fn pointer trait impls with varying arity
  --> $DIR/tuple-variadic-check.rs:12:7
   |
LL | #[doc(fake_variadic)]
   |       ^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("tuple-variadic-check.rs", "2015", source), stderr);
}

#[test]
fn fake_variadic_is_of_an_impl_and_search_unbox_of_a_generic_type() {
    let source = r##"#![feature(rustdoc_internals)]
#![crate_type = "lib"]

#[doc(fake_variadic)] //~ ERROR should be used on impl blocks
pub struct S;

#[doc(search_unbox)] //~ ERROR should be used on generic structs and enums
pub struct T;

#[doc(search_unbox)]
pub struct U<V>(V);

#[doc(search_unbox)]
pub trait W {
    type X;
}
"##;
    let stderr = r##"error: `#[doc(keyword = "...")]` should be used on impl blocks
  --> $DIR/doc-fake-variadic-search-unbox.rs:4:7
   |
LL | #[doc(fake_variadic)]
   |       ^^^^^^^^^^^^^

error: `#[doc(search_unbox)]` should be used on generic structs and enums
  --> $DIR/doc-fake-variadic-search-unbox.rs:7:7
   |
LL | #[doc(search_unbox)]
   |       ^^^^^^^^^^^^

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui("doc-fake-variadic-search-unbox.rs", "2015", source), stderr);
}

#[test]
fn inline_and_no_inline_conflict() {
    let source = r##"#![crate_type = "lib"]

mod m {
    pub struct S;
}

#[doc(inline)] //~ ERROR conflicting doc inlining attributes
#[doc(no_inline)]
pub use m::S;
"##;
    let stderr = r##"error: conflicting doc inlining attributes
  --> $DIR/doc-inline-conflict.rs:7:7
   |
LL | #[doc(inline)]
   |       ^^^^^^ this attribute...
LL | #[doc(no_inline)]
   |       ^^^^^^^^^ ...conflicts with this attribute
   |
   = help: remove one of the conflicting attributes

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("doc-inline-conflict.rs", "2015", source), stderr);
}

#[test]
fn rust_logo_is_gated() {
    // tests/ui/feature-gates/doc-rust-logo.rs
    let source = r##"#![doc(rust_logo)]
//~^ ERROR the `#[doc(rust_logo)]` attribute is used for Rust branding
//! This is not an official rust crate

fn main() {}
"##;
    assert_reports(&ui("doc-rust-logo.rs", "2015", source), &[error(1, &["`#[doc(rust_logo)]`", "rustdoc_internals"])]);
}

#[test]
fn a_cfg_is_a_list_of_one_predicate() {
    // tests/rustdoc-ui/invalid-cfg.rs
    let source = r##"#![feature(doc_cfg)]
#[doc(cfg = "x")] //~ ERROR malformed `doc` attribute input
#[doc(cfg(x, y))] //~ ERROR malformed `doc` attribute input
pub struct S {}

// We check it also fails on private items.
#[doc(cfg = "x")] //~ ERROR malformed `doc` attribute input
#[doc(cfg(x, y))] //~ ERROR malformed `doc` attribute input
struct X {}

// We check it also fails on hidden items.
#[doc(cfg = "x")] //~ ERROR malformed `doc` attribute input
#[doc(cfg(x, y))] //~ ERROR malformed `doc` attribute input
#[doc(hidden)]
pub struct Y {}

// We check it also fails on hidden AND private items.
#[doc(cfg = "x")] //~ ERROR malformed `doc` attribute input
#[doc(cfg(x, y))] //~ ERROR malformed `doc` attribute input
#[doc(hidden)]
struct Z {}
"##;
    let pair = |line: usize| {
        format!(
            r##"error[E0539]: malformed `doc` attribute input
  --> $DIR/invalid-cfg.rs:{line}:3
   |
LL | #[doc(cfg = "x")]
   |   ^^^^^^^^-----^
   |           |
   |           expected this to be a list

error[E0805]: malformed `doc` attribute input
  --> $DIR/invalid-cfg.rs:{}:3
   |
LL | #[doc(cfg(x, y))]
   |   ^^^^^^^------^
   |          |
   |          expected a single argument here
   |
help: if the struct should be enabled when all these predicates are, wrap them in `all`
   |
LL - #[doc(cfg(x, y))]
LL + #[doc(cfg(all(x, y)))]
   |
help: alternately, if the struct should be enabled when any of these predicates are, wrap them in `any`
   |
LL - #[doc(cfg(x, y))]
LL + #[doc(cfg(any(x, y)))]
   |

"##,
            line + 1
        )
    };
    let stderr = format!(
        "{}{}{}{}error: aborting due to 8 previous errors

Some errors have detailed explanations: E0539, E0805.
For more information about an error, try `rustc --explain E0539`.
",
        pair(2),
        pair(7),
        pair(12),
        pair(18)
    );
    assert_eq!(ui("invalid-cfg.rs", "2015", source), stderr);
}

#[test]
fn a_cfg_has_one_predicate_and_auto_cfg_hides_identifiers() {
    // tests/rustdoc-ui/doc-cfg.rs
    let source = r##"#![deny(invalid_doc_attributes)]
#![feature(doc_cfg)]

#[doc(cfg(), cfg(foo, bar))]
//~^ ERROR
//~| ERROR
#[doc(cfg())] //~ ERROR
#[doc(cfg(foo, bar))] //~ ERROR
#[doc(auto_cfg(hide(foo::bar)))] //~ ERROR
pub fn foo() {}
"##;
    let stderr = r##"error[E0805]: malformed `doc` attribute input
  --> $DIR/doc-cfg.rs:4:3
   |
LL | #[doc(cfg(), cfg(foo, bar))]
   |   ^^^^^^^--^^^^^^^^^^^^^^^^
   |          |
   |          expected an argument here
   |
help: if the function should be disabled, use `#[cfg(false)]`
   |
LL | #[doc(cfg(false), cfg(foo, bar))]
   |           +++++

error[E0805]: malformed `doc` attribute input
  --> $DIR/doc-cfg.rs:4:3
   |
LL | #[doc(cfg(), cfg(foo, bar))]
   |   ^^^^^^^^^^^^^^----------^
   |                 |
   |                 expected a single argument here
   |
help: if the function should be enabled when all these predicates are, wrap them in `all`
   |
LL - #[doc(cfg(), cfg(foo, bar))]
LL + #[doc(cfg(), cfg(all(foo, bar)))]
   |
help: alternately, if the function should be enabled when any of these predicates are, wrap them in `any`
   |
LL - #[doc(cfg(), cfg(foo, bar))]
LL + #[doc(cfg(), cfg(any(foo, bar)))]
   |

error[E0805]: malformed `doc` attribute input
  --> $DIR/doc-cfg.rs:7:3
   |
LL | #[doc(cfg())]
   |   ^^^^^^^--^
   |          |
   |          expected an argument here
   |
help: if the function should be disabled, use `#[cfg(false)]`
   |
LL | #[doc(cfg(false))]
   |           +++++

error[E0805]: malformed `doc` attribute input
  --> $DIR/doc-cfg.rs:8:3
   |
LL | #[doc(cfg(foo, bar))]
   |   ^^^^^^^----------^
   |          |
   |          expected a single argument here
   |
help: if the function should be enabled when all these predicates are, wrap them in `all`
   |
LL - #[doc(cfg(foo, bar))]
LL + #[doc(cfg(all(foo, bar)))]
   |
help: alternately, if the function should be enabled when any of these predicates are, wrap them in `any`
   |
LL - #[doc(cfg(foo, bar))]
LL + #[doc(cfg(any(foo, bar)))]
   |

error[E0565]: malformed `doc` attribute input
  --> $DIR/doc-cfg.rs:9:3
   |
LL | #[doc(auto_cfg(hide(foo::bar)))]
   |   ^^^^^^^^^^^^^^^^^^--------^^^
   |                     |
   |                     expected a valid identifier here

error: aborting due to 5 previous errors

Some errors have detailed explanations: E0565, E0805.
For more information about an error, try `rustc --explain E0565`.
"##;
    assert_eq!(ui("doc-cfg.rs", "2015", source), stderr);
}

#[test]
fn auto_cfg_values_are_identifiers() {
    // tests/rustdoc-ui/doc-auto-cfg-values-non-ident.rs
    let source = r##"// Regression test for https://github.com/rust-lang/rust/issues/158744.

#![feature(doc_cfg)]

#[doc(auto_cfg(hide(a, values(::b))))]
//~^ ERROR malformed `doc` attribute input
fn f() {}
"##;
    assert_reports(&ui("doc-auto-cfg-values-non-ident.rs", "2015", source), &[error(5, &["`doc`"])]);
}

#[test]
fn a_doc_cfg_predicate_is_gated() {
    // tests/rustdoc-ui/doc-cfg-unstable.rs
    let source = r##"// #138113: rustdoc didn't gate unstable predicates inside `doc(cfg(..))`
#![feature(doc_cfg)]

// `cfg_version`
#[doc(cfg(sanitize = "thread"))] //~ ERROR `cfg(sanitize)` is experimental and subject to change
pub fn cfg_sanitize() {}
"##;
    let stderr = r##"error[E0658]: `cfg(sanitize)` is experimental and subject to change
  --> $DIR/doc-cfg-unstable.rs:5:11
   |
LL | #[doc(cfg(sanitize = "thread"))]
   |           ^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #39699 <https://github.com/rust-lang/rust/issues/39699> for more information
   = help: add `#![feature(cfg_sanitize)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"##;
    assert_eq!(ui("doc-cfg-unstable.rs", "2015", source), stderr);
}

#[test]
fn a_doc_cfg_version_is_one_string_and_a_target_value_a_string() {
    let source = r##"#![feature(doc_cfg, cfg_version, cfg_target_compact)]
#![crate_type = "lib"]

#[doc(cfg(version(1)))] //~ ERROR expected a version literal
pub fn a() {}

#[doc(cfg(version("1.2", "1.3")))] //~ ERROR expected single version literal
pub fn b() {}

#[doc(cfg(target(os = 1)))] //~ ERROR malformed `doc` attribute input
pub fn c() {}
"##;
    let stderr = r##"error: expected a version literal
  --> $DIR/doc-cfg-version-target.rs:4:19
   |
LL | #[doc(cfg(version(1)))]
   |                   ^

error: expected single version literal
  --> $DIR/doc-cfg-version-target.rs:7:18
   |
LL | #[doc(cfg(version("1.2", "1.3")))]
   |                  ^^^^^^^^^^^^^^

error[E0539]: malformed `doc` attribute input
  --> $DIR/doc-cfg-version-target.rs:10:3
   |
LL | #[doc(cfg(target(os = 1)))]
   |   ^^^^^^^^^^^^^^^^^^^^-^^^
   |                       |
   |                       expected a string literal here

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0539`.
"##;
    assert_eq!(ui("doc-cfg-version-target.rs", "2015", source), stderr);
}

#[test]
fn the_diagnostic_attributes_give_no_error() {
    let source = r##"#![crate_type = "lib"]

#[diagnostic::on_unimplemented(message = "x", label = "y", note = "z")]
pub trait T {}

pub struct S;

#[diagnostic::do_not_recommend]
impl T for S {}
"##;
    assert_eq!(ui("diagnostic-attributes.rs", "2015", source), "");
}
