//! rustc's UI tests of the errors of the parsers of stability attributes, `#[deprecated]` and
//! the rest of `checks/attrs/parsers/stability.rs`, and of the checks rustc's stability pass
//! makes of what they parse to: `tests/ui/stability-attribute/`, `tests/ui/deprecation/`,
//! `tests/ui/error-codes/E0789.rs` and `tests/ui/consts/rustc-const-stability-require-const.rs`
//! (MIT or Apache-2.0).

use crate::tests::ui;

#[test]
fn a_stability_attribute_takes_each_key_once_and_a_number_for_its_issue() {
    // tests/ui/stability-attribute/stability-attribute-sanity-2.rs
    let source = r##"// More checks that stability attributes are used correctly

#![feature(staged_api)]

#![stable(feature = "stable_test_feature", since = "1.0.0")]

#[stable(feature = "a", feature = "b", since = "1.0.0")] //~ ERROR malformed `stable` attribute input [E0538]
fn f1() { }

#[stable(feature = "a", sinse = "1.0.0")] //~ ERROR malformed `stable` attribute input [E0539]
fn f2() { }

#[unstable(feature = "a", issue = "no")]
//~^ ERROR `issue` must be a non-zero numeric string or "none"
fn f3() { }

fn main() { }
"##;
    let stderr = r##"error[E0538]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity-2.rs:7:3
   |
LL | #[stable(feature = "a", feature = "b", since = "1.0.0")]
   |   ^^^^^^^^^^^^^^^^^^^^^^-------^^^^^^^^^^^^^^^^^^^^^^^^
   |                         |
   |                         found `feature` used as a key more than once
   |
help: must be of the form
   |
LL - #[stable(feature = "a", feature = "b", since = "1.0.0")]
LL + #[stable(feature = "name", since = "version")]
   |

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity-2.rs:10:3
   |
LL | #[stable(feature = "a", sinse = "1.0.0")]
   |   ^^^^^^^^^^^^^^^^^^^^^^---------------^
   |                         |
   |                         valid arguments are `feature` or `since`
   |
help: must be of the form
   |
LL - #[stable(feature = "a", sinse = "1.0.0")]
LL + #[stable(feature = "name", since = "version")]
   |

error[E0545]: `issue` must be a non-zero numeric string or "none"
  --> $DIR/stability-attribute-sanity-2.rs:13:27
   |
LL | #[unstable(feature = "a", issue = "no")]
   |                           ^^^^^^^^----
   |                                   |
   |                                   invalid digit found in string

error: aborting due to 3 previous errors

Some errors have detailed explanations: E0538, E0539, E0545.
For more information about an error, try `rustc --explain E0538`.
"##;
    assert_eq!(ui("stability-attribute-sanity-2.rs", "2015", source), stderr);
}

#[test]
fn a_stability_attribute_is_a_list_and_a_staged_deprecation_has_since_and_note() {
    // tests/ui/stability-attribute/stability-attribute-sanity-4.rs
    let source = r##"// Various checks that stability attributes are used correctly, per RFC 507

#![feature(staged_api)]

#![stable(feature = "rust1", since = "1.0.0")]

mod bogus_attribute_types_2 {
    #[unstable] //~ ERROR malformed `unstable` attribute
    fn f1() { }

    #[unstable = "b"] //~ ERROR malformed `unstable` attribute
    fn f2() { }

    #[stable] //~ ERROR malformed `stable` attribute
    fn f3() { }

    #[stable = "a"] //~ ERROR malformed `stable` attribute
    fn f4() { }

    #[stable(feature = "a", since = "3.3.3")]
    #[deprecated] //~ ERROR missing 'since'
    //~^ ERROR missing 'note'
    fn f5() { }

    #[stable(feature = "a", since = "3.3.3")]
    #[deprecated = "a"] //~ ERROR missing 'since'
    fn f6() { }
}

fn main() { }
"##;
    let stderr = r##"error[E0539]: malformed `unstable` attribute input
  --> $DIR/stability-attribute-sanity-4.rs:8:7
   |
LL |     #[unstable]
   |       ^^^^^^^^ expected this to be a list
   |
help: must be of the form
   |
LL |     #[unstable(feature = "name", reason = "...", issue = "N")]
   |               +++++++++++++++++++++++++++++++++++++++++++++++

error[E0539]: malformed `unstable` attribute input
  --> $DIR/stability-attribute-sanity-4.rs:11:7
   |
LL |     #[unstable = "b"]
   |       ^^^^^^^^^-----
   |                |
   |                expected this to be a list
   |
help: must be of the form
   |
LL -     #[unstable = "b"]
LL +     #[unstable(feature = "name", reason = "...", issue = "N")]
   |

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity-4.rs:14:7
   |
LL |     #[stable]
   |       ^^^^^^ expected this to be a list
   |
help: must be of the form
   |
LL |     #[stable(feature = "name", since = "version")]
   |             +++++++++++++++++++++++++++++++++++++

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity-4.rs:17:7
   |
LL |     #[stable = "a"]
   |       ^^^^^^^-----
   |              |
   |              expected this to be a list
   |
help: must be of the form
   |
LL -     #[stable = "a"]
LL +     #[stable(feature = "name", since = "version")]
   |

error[E0542]: missing 'since'
  --> $DIR/stability-attribute-sanity-4.rs:21:5
   |
LL |     #[deprecated]
   |     ^^^^^^^^^^^^^

error[E0543]: missing 'note'
  --> $DIR/stability-attribute-sanity-4.rs:21:5
   |
LL |     #[deprecated]
   |     ^^^^^^^^^^^^^

error[E0542]: missing 'since'
  --> $DIR/stability-attribute-sanity-4.rs:26:5
   |
LL |     #[deprecated = "a"]
   |     ^^^^^^^^^^^^^^^^^^^

error: aborting due to 7 previous errors

Some errors have detailed explanations: E0539, E0542, E0543.
For more information about an error, try `rustc --explain E0539`.
"##;
    assert_eq!(ui("stability-attribute-sanity-4.rs", "2015", source), stderr);
}

#[test]
fn stability_attributes_are_checked_as_parsed_and_against_each_other() {
    // tests/ui/stability-attribute/stability-attribute-sanity.rs
    let source = r##"// Various checks that stability attributes are used correctly, per RFC 507

#![feature(staged_api)]

#![stable(feature = "rust1", since = "1.0.0")]

mod bogus_attribute_types_1 {
    #[stable(feature = "a", since = "4.4.4", reason)] //~ ERROR malformed `stable` attribute input [E0539]
    fn f1() { }

    #[stable(feature = "a", since)] //~ ERROR malformed `stable` attribute input [E0539]
    fn f2() { }

    #[stable(feature, since = "3.3.3")] //~ ERROR malformed `stable` attribute input [E0539]
    fn f3() { }

    #[stable(feature = "a", since(b))] //~ ERROR malformed `stable` attribute input [E0539]
    fn f5() { }

    #[stable(feature(b), since = "3.3.3")] //~ ERROR malformed `stable` attribute input [E0539]
    fn f6() { }
}

mod missing_feature_names {
    #[unstable(issue = "none")] //~ ERROR missing 'feature' [E0546]
    fn f1() { }

    #[unstable(feature = "b")] //~ ERROR missing 'issue' [E0547]
    fn f2() { }

    #[stable(since = "3.3.3")] //~ ERROR missing 'feature' [E0546]
    fn f3() { }
}

mod missing_version {
    #[stable(feature = "a")] //~ ERROR missing 'since' [E0542]
    fn f1() { }

    #[stable(feature = "a", since = "4.4.4")]
    #[deprecated(note = "a")] //~ ERROR missing 'since' [E0542]
    fn f2() { }

    #[stable(feature = "a", since = "4.4.4")]
    #[deprecated(since = "5.5.5")] //~ ERROR missing 'note' [E0543]
    fn f3() { }
}

#[unstable(feature = "b", issue = "none")]
#[stable(feature = "a", since = "4.4.4")] //~ ERROR multiple stability levels [E0544]
fn multiple1() { }

#[unstable(feature = "b", issue = "none")]
#[unstable(feature = "b", issue = "none")] //~ ERROR multiple stability levels [E0544]
fn multiple2() { }

#[stable(feature = "a", since = "4.4.4")]
#[stable(feature = "a", since = "4.4.4")] //~ ERROR multiple stability levels [E0544]
fn multiple3() { }

#[stable(feature = "e", since = "b")] //~ ERROR 'since' must be a Rust version number, such as "1.31.0"
#[deprecated(since = "5.5.5", note = "text")]
#[deprecated(since = "5.5.5", note = "text")] //~ ERROR multiple `deprecated` attributes
#[rustc_const_unstable(feature = "c", issue = "none")]
#[rustc_const_unstable(feature = "d", issue = "none")] //~ ERROR multiple stability levels
pub const fn multiple4() { }

#[stable(feature = "a", since = "1.0.0")] //~ ERROR feature `a` is declared stable since 1.0.0
#[deprecated(since = "invalid", note = "text")] //~ ERROR 'since' must be a Rust version number, such as "1.31.0"
fn invalid_deprecation_version() {}

#[deprecated(since = "5.5.5", note = "text")]
fn deprecated_without_unstable_or_stable() { }
//~^^ ERROR deprecated attribute must be paired with either stable or unstable attribute

fn main() { }
"##;
    let stderr = r##"error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity.rs:8:7
   |
LL |     #[stable(feature = "a", since = "4.4.4", reason)]
   |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^------^
   |                                              |
   |                                              valid arguments are `feature` or `since`
   |
help: must be of the form
   |
LL -     #[stable(feature = "a", since = "4.4.4", reason)]
LL +     #[stable(feature = "name", since = "version")]
   |

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity.rs:11:7
   |
LL |     #[stable(feature = "a", since)]
   |       ^^^^^^^^^^^^^^^^^^^^^^-----^
   |                             |
   |                             expected this to be of the form `since = "..."`
   |
help: must be of the form
   |
LL -     #[stable(feature = "a", since)]
LL +     #[stable(feature = "name", since = "version")]
   |

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity.rs:14:7
   |
LL |     #[stable(feature, since = "3.3.3")]
   |       ^^^^^^^-------^^^^^^^^^^^^^^^^^^
   |              |
   |              expected this to be of the form `feature = "..."`
   |
help: must be of the form
   |
LL -     #[stable(feature, since = "3.3.3")]
LL +     #[stable(feature = "name", since = "version")]
   |

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity.rs:17:7
   |
LL |     #[stable(feature = "a", since(b))]
   |       ^^^^^^^^^^^^^^^^^^^^^^--------^
   |                             |
   |                             expected this to be of the form `since = "..."`
   |
help: must be of the form
   |
LL -     #[stable(feature = "a", since(b))]
LL +     #[stable(feature = "name", since = "version")]
   |

error[E0539]: malformed `stable` attribute input
  --> $DIR/stability-attribute-sanity.rs:20:7
   |
LL |     #[stable(feature(b), since = "3.3.3")]
   |       ^^^^^^^----------^^^^^^^^^^^^^^^^^^
   |              |
   |              expected this to be of the form `feature = "..."`
   |
help: must be of the form
   |
LL -     #[stable(feature(b), since = "3.3.3")]
LL +     #[stable(feature = "name", since = "version")]
   |

error[E0546]: missing 'feature'
  --> $DIR/stability-attribute-sanity.rs:25:5
   |
LL |     #[unstable(issue = "none")]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0547]: missing 'issue'
  --> $DIR/stability-attribute-sanity.rs:28:5
   |
LL |     #[unstable(feature = "b")]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0546]: missing 'feature'
  --> $DIR/stability-attribute-sanity.rs:31:5
   |
LL |     #[stable(since = "3.3.3")]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0542]: missing 'since'
  --> $DIR/stability-attribute-sanity.rs:36:5
   |
LL |     #[stable(feature = "a")]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^

error[E0542]: missing 'since'
  --> $DIR/stability-attribute-sanity.rs:40:5
   |
LL |     #[deprecated(note = "a")]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0543]: missing 'note'
  --> $DIR/stability-attribute-sanity.rs:44:5
   |
LL |     #[deprecated(since = "5.5.5")]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0544]: multiple stability levels
  --> $DIR/stability-attribute-sanity.rs:49:1
   |
LL | #[stable(feature = "a", since = "4.4.4")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0544]: multiple stability levels
  --> $DIR/stability-attribute-sanity.rs:53:1
   |
LL | #[unstable(feature = "b", issue = "none")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0544]: multiple stability levels
  --> $DIR/stability-attribute-sanity.rs:57:1
   |
LL | #[stable(feature = "a", since = "4.4.4")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: 'since' must be a Rust version number, such as "1.31.0"
  --> $DIR/stability-attribute-sanity.rs:60:1
   |
LL | #[stable(feature = "e", since = "b")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: multiple `deprecated` attributes
  --> $DIR/stability-attribute-sanity.rs:62:1
   |
LL | #[deprecated(since = "5.5.5", note = "text")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
note: attribute also specified here
  --> $DIR/stability-attribute-sanity.rs:61:1
   |
LL | #[deprecated(since = "5.5.5", note = "text")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0544]: multiple stability levels
  --> $DIR/stability-attribute-sanity.rs:64:1
   |
LL | #[rustc_const_unstable(feature = "d", issue = "none")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: 'since' must be a Rust version number, such as "1.31.0"
  --> $DIR/stability-attribute-sanity.rs:68:1
   |
LL | #[deprecated(since = "invalid", note = "text")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0549]: deprecated attribute must be paired with either stable or unstable attribute
  --> $DIR/stability-attribute-sanity.rs:71:1
   |
LL | #[deprecated(since = "5.5.5", note = "text")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error[E0711]: feature `a` is declared stable since 1.0.0, but was previously declared stable since 4.4.4
  --> $DIR/stability-attribute-sanity.rs:67:1
   |
LL | #[stable(feature = "a", since = "1.0.0")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 20 previous errors

Some errors have detailed explanations: E0539, E0542, E0543, E0544, E0546, E0547, E0549, E0711.
For more information about an error, try `rustc --explain E0539`.
"##;
    assert_eq!(ui("stability-attribute-sanity.rs", "2015", source), stderr);
}

#[test]
fn allowed_through_unstable_modules_needs_a_stable_attribute() {
    // tests/ui/error-codes/E0789.rs
    let source = r##"//@ compile-flags: --crate-type lib

#![feature(rustc_attrs)]
#![feature(staged_api)]
#![unstable(feature = "foo_module", reason = "...", issue = "123")]

#[rustc_allowed_through_unstable_modules = "use stable path instead"]
// #[stable(feature = "foo", since = "1.0")]
struct Foo;
//~^ ERROR `rustc_allowed_through_unstable_modules` attribute must be paired with a `stable` attribute
"##;
    let stderr = r##"error[E0789]: `rustc_allowed_through_unstable_modules` attribute must be paired with a `stable` attribute
  --> $DIR/E0789.rs:9:1
   |
LL | struct Foo;
   | ^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0789`.
"##;
    assert_eq!(ui("E0789.rs", "2015", source), stderr);
}

#[test]
fn const_stability_needs_a_const_fn_and_const_stable_a_stable_one() {
    // tests/ui/consts/rustc-const-stability-require-const.rs
    let source = r##"#![crate_type = "lib"]
#![feature(staged_api, rustc_attrs)]
#![stable(feature = "foo", since = "1.0.0")]

#[stable(feature = "foo", since = "1.0.0")]
#[rustc_const_unstable(feature = "const_foo", issue = "none")]
pub fn foo() {}
//~^ ERROR require the function or method to be `const`

#[stable(feature = "bar", since = "1.0.0")]
#[rustc_const_stable(feature = "const_bar", since = "1.0.0")]
pub fn bar() {}
//~^ ERROR require the function or method to be `const`

#[stable(feature = "potato", since = "1.0.0")]
pub struct Potato;

impl Potato {
    #[stable(feature = "salad", since = "1.0.0")]
    #[rustc_const_unstable(feature = "const_salad", issue = "none")]
    pub fn salad(&self) -> &'static str { "mmmmmm" }
    //~^ ERROR require the function or method to be `const`

    #[stable(feature = "roasted", since = "1.0.0")]
    #[rustc_const_unstable(feature = "const_roasted", issue = "none")]
    pub fn roasted(&self) -> &'static str { "mmmmmmmmmm" }
    //~^ ERROR require the function or method to be `const`
}

#[stable(feature = "bar", since = "1.0.0")]
#[rustc_const_stable(feature = "const_bar", since = "1.0.0")]
pub extern "C" fn bar_c() {}
//~^ ERROR require the function or method to be `const`

#[stable(feature = "foo", since = "1.0.0")]
#[rustc_const_unstable(feature = "const_foo", issue = "none")]
pub extern "C" fn foo_c() {}
//~^ ERROR require the function or method to be `const`


#[stable(feature = "foobar", since = "1.0.0")]
#[rustc_const_unstable(feature = "foobar_const", issue = "none")]
pub const fn foobar() {}

#[stable(feature = "barfoo", since = "1.0.0")]
#[rustc_const_stable(feature = "barfoo_const", since = "1.0.0")]
pub const fn barfoo() {}

// `rustc_const_stable` also requires the function to be stable.

#[rustc_const_stable(feature = "barfoo_const", since = "1.0.0")]
const fn barfoo_unmarked() {}
//~^ ERROR can only be applied to functions marked with the `stable` attribute

#[unstable(feature = "unstable", issue = "none")]
#[rustc_const_stable(feature = "barfoo_const", since = "1.0.0")]
pub const fn barfoo_unstable() {}
//~^ ERROR can only be applied to functions marked with the `stable` attribute
"##;
    let stderr = r##"error: attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`
  --> $DIR/rustc-const-stability-require-const.rs:7:1
   |
LL | pub fn foo() {}
   | ^^^^^^^^^^^^
   |
help: make the function or method const
  --> $DIR/rustc-const-stability-require-const.rs:7:1
   |
LL | pub fn foo() {}
   | ^^^^^^^^^^^^

error: attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`
  --> $DIR/rustc-const-stability-require-const.rs:12:1
   |
LL | pub fn bar() {}
   | ^^^^^^^^^^^^
   |
help: make the function or method const
  --> $DIR/rustc-const-stability-require-const.rs:12:1
   |
LL | pub fn bar() {}
   | ^^^^^^^^^^^^

error: attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`
  --> $DIR/rustc-const-stability-require-const.rs:32:1
   |
LL | pub extern "C" fn bar_c() {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: make the function or method const
  --> $DIR/rustc-const-stability-require-const.rs:32:1
   |
LL | pub extern "C" fn bar_c() {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^

error: attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`
  --> $DIR/rustc-const-stability-require-const.rs:37:1
   |
LL | pub extern "C" fn foo_c() {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: make the function or method const
  --> $DIR/rustc-const-stability-require-const.rs:37:1
   |
LL | pub extern "C" fn foo_c() {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^

error: the `rustc_const_stable` attribute can only be applied to functions marked with the `stable` attribute
  --> $DIR/rustc-const-stability-require-const.rs:52:1
   |
LL | #[rustc_const_stable(feature = "barfoo_const", since = "1.0.0")]
   |   ------------------ attribute specified here
LL | const fn barfoo_unmarked() {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^

error: the `rustc_const_stable` attribute can only be applied to functions marked with the `stable` attribute
  --> $DIR/rustc-const-stability-require-const.rs:57:1
   |
LL | #[rustc_const_stable(feature = "barfoo_const", since = "1.0.0")]
   |   ------------------ attribute specified here
LL | pub const fn barfoo_unstable() {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`
  --> $DIR/rustc-const-stability-require-const.rs:21:5
   |
LL |     pub fn salad(&self) -> &'static str { "mmmmmm" }
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: make the function or method const
  --> $DIR/rustc-const-stability-require-const.rs:21:5
   |
LL |     pub fn salad(&self) -> &'static str { "mmmmmm" }
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`
  --> $DIR/rustc-const-stability-require-const.rs:26:5
   |
LL |     pub fn roasted(&self) -> &'static str { "mmmmmmmmmm" }
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: make the function or method const
  --> $DIR/rustc-const-stability-require-const.rs:26:5
   |
LL |     pub fn roasted(&self) -> &'static str { "mmmmmmmmmm" }
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 8 previous errors

"##;
    assert_eq!(ui("rustc-const-stability-require-const.rs", "2015", source), stderr);
}

#[test]
fn a_deprecated_note_is_a_plain_string() {
    // tests/ui/deprecation/invalid-literal.rs
    let source = r##"#[deprecated = b"test"] //~ ERROR malformed `deprecated` attribute
fn foo() {}

fn main() {}
"##;
    let stderr = r##"error[E0539]: malformed `deprecated` attribute input
  --> $DIR/invalid-literal.rs:1:3
   |
LL | #[deprecated = b"test"]
   |   ^^^^^^^^^^^^^-^^^^^^
   |                |
   |                help: consider removing the prefix
   |
   = note: expected a normal string literal, not a byte string literal

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"##;
    assert_eq!(ui("invalid-literal.rs", "2015", source), stderr);
}

#[test]
fn a_deprecated_suggestion_is_gated() {
    // tests/ui/deprecation/feature-gate-deprecated_suggestion.rs
    let source = r##"//@ compile-flags: --crate-type=lib

#![no_implicit_prelude]

#[deprecated(suggestion = "foo")] //~ ERROR suggestions on deprecated items are unstable
struct Foo {}
"##;
    let stderr = r##"error: suggestions on deprecated items are unstable
  --> $DIR/feature-gate-deprecated_suggestion.rs:5:14
   |
LL | #[deprecated(suggestion = "foo")]
   |              ^^^^^^^^^^^^^^^^^^
   |
   = help: add `#![feature(deprecated_suggestion)]` to the crate root
   = note: see #94785 for more details

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("feature-gate-deprecated_suggestion.rs", "2015", source), stderr);
}

#[test]
fn a_reachable_item_needs_a_stability_unless_it_inherits_an_unstable_one() {
    // tests/ui/missing/missing-stability.rs
    let source = r##"// Checks that exported items without stability attributes cause an error

#![crate_type="lib"]
#![feature(staged_api)]

#![stable(feature = "stable_test_feature", since = "1.0.0")]

pub fn unmarked() {
    //~^ ERROR function has missing stability attribute
    ()
}

#[unstable(feature = "unstable_test_feature", issue = "none")]
pub mod foo {
    // #[unstable] is inherited
    pub fn unmarked() {}
}

#[stable(feature = "stable_test_feature", since="1.0.0")]
pub mod bar {
    // #[stable] is not inherited
    pub fn unmarked() {}
    //~^ ERROR function has missing stability attribute
}
"##;
    let stderr = r##"error: function has missing stability attribute
  --> $DIR/missing-stability.rs:8:1
   |
LL | pub fn unmarked() {
   | ^^^^^^^^^^^^^^^^^

error: function has missing stability attribute
  --> $DIR/missing-stability.rs:22:5
   |
LL |     pub fn unmarked() {}
   |     ^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui("missing-stability.rs", "2015", source), stderr);
}

#[test]
fn an_exported_macro_needs_a_stability() {
    // tests/ui/stability-attribute/stability-attribute-sanity-3.rs
    let source = r##"// More checks that stability attributes are used correctly

#![feature(staged_api)]

#![stable(feature = "stable_test_feature", since = "1.0.0")]

#[macro_export]
macro_rules! mac { //~ ERROR macro has missing stability attribute
    () => ()
}

fn main() { }
"##;
    let stderr = r##"error: macro has missing stability attribute
  --> $DIR/stability-attribute-sanity-3.rs:8:1
   |
LL | macro_rules! mac {
   | ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("stability-attribute-sanity-3.rs", "2015", source), stderr);
}

#[test]
fn a_reachable_const_fn_or_const_trait_needs_a_const_stability() {
    // tests/ui/stability-attribute/missing-const-stability.rs
    let source = r##"//@ compile-flags: -Znext-solver
#![feature(staged_api)]
#![feature(const_trait_impl, rustc_attrs, intrinsics)]
#![stable(feature = "stable", since = "1.0.0")]

#[stable(feature = "stable", since = "1.0.0")]
pub const fn foo() {} //~ ERROR function has missing const stability attribute

#[unstable(feature = "unstable", issue = "none")]
pub const fn bar() {} // ok because function is unstable

#[stable(feature = "stable", since = "1.0.0")]
pub struct Foo;
impl Foo {
    #[stable(feature = "stable", since = "1.0.0")]
    pub const fn foo() {} //~ ERROR associated function has missing const stability attribute

    #[unstable(feature = "unstable", issue = "none")]
    pub const fn bar() {} // ok because function is unstable
}

#[stable(feature = "stable", since = "1.0.0")]
pub const trait Bar {
//~^ ERROR trait has missing const stability attribute
    #[stable(feature = "stable", since = "1.0.0")]
    fn fun();
}
#[stable(feature = "stable", since = "1.0.0")]
const impl Bar for Foo {
    // ok because all users must enable `const_trait_impl`
    fn fun() {}
}

#[stable(feature = "stable", since = "1.0.0")]
#[rustc_intrinsic]
pub const unsafe fn size_of_val<T>(x: *const T) -> usize { 42 }
//~^ ERROR function has missing const stability attribute

fn main() {}
"##;
    let stderr = r##"error: function has missing const stability attribute
  --> $DIR/missing-const-stability.rs:7:1
   |
LL | pub const fn foo() {}
   | ^^^^^^^^^^^^^^^^^^

error: trait has missing const stability attribute
  --> $DIR/missing-const-stability.rs:23:1
   |
LL | pub const trait Bar {
   | ^^^^^^^^^^^^^^^^^^^

error: function has missing const stability attribute
  --> $DIR/missing-const-stability.rs:36:1
   |
LL | pub const unsafe fn size_of_val<T>(x: *const T) -> usize { 42 }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: associated function has missing const stability attribute
  --> $DIR/missing-const-stability.rs:16:5
   |
LL |     pub const fn foo() {}
   |     ^^^^^^^^^^^^^^^^^^

error: aborting due to 4 previous errors

"##;
    assert_eq!(ui("missing-const-stability.rs", "2015", source), stderr);
}

#[test]
fn the_crate_root_needs_a_stability() {
    // tests/ui/stability-attribute/missing-stability-attr-at-top-level.rs
    let source = r##"#![feature(staged_api)]
//~^ ERROR module has missing stability attribute

fn main() {}
"##;
    let stderr = r##"error: module has missing stability attribute
  --> $DIR/missing-stability-attr-at-top-level.rs:1:1
   |
LL | / #![feature(staged_api)]
...  |
LL | | fn main() {}
   | |____________^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("missing-stability-attr-at-top-level.rs", "2015", source), stderr);
}

#[test]
fn a_trait_its_items_and_a_trait_impl_need_a_stability_in_rustcs_order() {
    // tests/ui/privacy/issue-113860.rs
    let source = r##"#![feature(staged_api)]
//~^ ERROR module has missing stability attribute

pub trait Trait {
    //~^ ERROR trait has missing stability attribute
    const X: u32;
    //~^ ERROR associated constant has missing stability attribute
}

impl Trait for u8 {
    //~^ ERROR implementation has missing stability attribute
    pub(self) const X: u32 = 3;
    //~^ ERROR visibility qualifiers are not permitted here [E0449]
}

fn main() {}
"##;
    let stderr = r##"error[E0449]: visibility qualifiers are not permitted here
  --> $DIR/issue-113860.rs:12:5
   |
LL |     pub(self) const X: u32 = 3;
   |     ^^^^^^^^^ help: remove the qualifier
   |
   = note: trait items always share the visibility of their trait

error: module has missing stability attribute
  --> $DIR/issue-113860.rs:1:1
   |
LL | / #![feature(staged_api)]
LL | |
LL | |
LL | | pub trait Trait {
...  |
LL | | fn main() {}
   | |____________^

error: trait has missing stability attribute
  --> $DIR/issue-113860.rs:4:1
   |
LL | pub trait Trait {
   | ^^^^^^^^^^^^^^^

error: implementation has missing stability attribute
  --> $DIR/issue-113860.rs:10:1
   |
LL | impl Trait for u8 {
   | ^^^^^^^^^^^^^^^^^

error: associated constant has missing stability attribute
  --> $DIR/issue-113860.rs:6:5
   |
LL |     const X: u32;
   |     ^^^^^^^^^^^^

error: aborting due to 5 previous errors

For more information about this error, try `rustc --explain E0449`.
"##;
    assert_eq!(ui("issue-113860.rs", "2015", source), stderr);
}

/// Not a rustc UI test: what `check_missing_stability` gives, as rustc's source has it
/// (`visit_variant`, `lookup_stability`'s `inherit_stability`, `def_span`), of an enum with
/// none: the enum, each variant, then a tuple or unit variant's constructor ("tuple variant",
/// "unit variant", spanned as the variant), then its fields.
#[test]
fn a_variant_and_its_constructor_each_need_a_stability() {
    let source = r##"#![feature(staged_api)]
#![stable(feature = "a", since = "1.0.0")]

pub enum E {
    T(u8),
    U,
    R { x: u8 },
}

fn main() {}
"##;
    let stderr = r##"error: enum has missing stability attribute
  --> $DIR/variants.rs:4:1
   |
LL | pub enum E {
   | ^^^^^^^^^^

error: variant has missing stability attribute
  --> $DIR/variants.rs:5:5
   |
LL |     T(u8),
   |     ^

error: tuple variant has missing stability attribute
  --> $DIR/variants.rs:5:5
   |
LL |     T(u8),
   |     ^

error: field has missing stability attribute
  --> $DIR/variants.rs:5:7
   |
LL |     T(u8),
   |       ^^

error: variant has missing stability attribute
  --> $DIR/variants.rs:6:5
   |
LL |     U,
   |     ^

error: unit variant has missing stability attribute
  --> $DIR/variants.rs:6:5
   |
LL |     U,
   |     ^

error: variant has missing stability attribute
  --> $DIR/variants.rs:7:5
   |
LL |     R { x: u8 },
   |     ^

error: field has missing stability attribute
  --> $DIR/variants.rs:7:9
   |
LL |     R { x: u8 },
   |         ^^^^^

error: aborting due to 8 previous errors

"##;
    assert_eq!(ui("variants.rs", "2015", source), stderr);
}
