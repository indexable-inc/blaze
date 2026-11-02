//! rustc's UI tests of the lifetimes a signature leaves out or may not use (E0106, E0637, the
//! associated type with no lifetime of an impl), lifetime arguments given to a late-bound
//! lifetime parameter (E0794, `late_bound_lifetime_arguments`) and a path that names both a
//! variant and an associated type (`ambiguous_associated_items`). Each test's file is the UI
//! test's source byte for byte.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked_with, compiletest, normalized, ui},
};

#[test]
fn a_lifetime_a_macro_emits_among_an_impls_items_is_the_impls() {
    // Our own: serde's `forward_to_deserialize_any!` in an `impl<'de> Deserializer<'de>`
    // (blaze-protocol's cbor/de.rs, sonic-rs), through a second macro as serde's is. The same
    // methods emitted outside any impl have no `'de`.
    let fixture = r#"//- minicore: sized
//- /main.rs crate:other:main edition:2021
pub trait Visitor<'de> {}
pub trait Deserializer<'de> {
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V);
}
macro_rules! method {
    ($name:ident) => {
        fn $name<V: Visitor<'de>>(self, visitor: V) {}
    };
}
macro_rules! forward {
    ($($name:ident)*) => { $(method! { $name })* };
}
struct D;
impl<'de> Deserializer<'de> for D {
    forward! { deserialize_bool }
}
forward! { outside }
fn main() {}
"#;
    let messages = crate::tests::checked(fixture).messages();
    let undeclared: Vec<&String> = messages.iter().filter(|it| it.starts_with("E0261")).collect();
    assert_eq!(undeclared.len(), 1, "only `outside`'s `'de` is undeclared: {messages:?}");
}

/// [`ui`] of a file with minicore's items for `flags` (`crates/test-utils/src/minicore.rs`), as
/// compiletest runs a test of `mode`.
fn ui_core(mode: Mode, name: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:2015\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(mode, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

#[test]
fn a_reference_or_path_in_an_item_with_no_lifetime_to_take() {
    // tests/ui/error-codes/E0106.rs
    let source = r#"//@ dont-require-annotations: NOTE

struct Foo {
    x: &bool,
    //~^ ERROR E0106
}
enum Bar {
    A(u8),
    B(&bool),
   //~^ ERROR E0106
}
type MyStr = &str;
        //~^ ERROR E0106

struct Baz<'a>(&'a str);
struct Buzz<'a, 'b>(&'a str, &'b str);

struct Quux {
    baz: Baz,
    //~^ ERROR E0106
    //~| NOTE expected named lifetime parameter
    buzz: Buzz,
    //~^ ERROR E0106
    //~| NOTE expected 2 lifetime parameters
}

fn main() {
}
"#;
    let stderr = r#"error[E0106]: missing lifetime specifier
  --> $DIR/E0106.rs:4:8
   |
LL |     x: &bool,
   |        ^ expected named lifetime parameter
   |
help: consider introducing a named lifetime parameter
   |
LL ~ struct Foo<'a> {
LL ~     x: &'a bool,
   |

error[E0106]: missing lifetime specifier
  --> $DIR/E0106.rs:9:7
   |
LL |     B(&bool),
   |       ^ expected named lifetime parameter
   |
help: consider introducing a named lifetime parameter
   |
LL ~ enum Bar<'a> {
LL |     A(u8),
LL ~     B(&'a bool),
   |

error[E0106]: missing lifetime specifier
  --> $DIR/E0106.rs:12:14
   |
LL | type MyStr = &str;
   |              ^ expected named lifetime parameter
   |
help: consider introducing a named lifetime parameter
   |
LL | type MyStr<'a> = &'a str;
   |           ++++    ++

error[E0106]: missing lifetime specifier
  --> $DIR/E0106.rs:19:10
   |
LL |     baz: Baz,
   |          ^^^ expected named lifetime parameter
   |
help: consider introducing a named lifetime parameter
   |
LL ~ struct Quux<'a> {
LL ~     baz: Baz<'a>,
   |

error[E0106]: missing lifetime specifiers
  --> $DIR/E0106.rs:22:11
   |
LL |     buzz: Buzz,
   |           ^^^^ expected 2 lifetime parameters
   |
help: consider introducing a named lifetime parameter
   |
LL ~ struct Quux<'a> {
LL |     baz: Baz,
LL |
LL |
LL ~     buzz: Buzz<'a, 'a>,
   |

error: aborting due to 5 previous errors

For more information about this error, try `rustc --explain E0106`.
"#;
    assert_eq!(ui("E0106.rs", "2015", source), stderr);
}

#[test]
fn an_associated_type_of_an_impl_takes_no_lifetime_from_the_header() {
    // tests/ui/impl-header-lifetime-elision/assoc-type.rs
    let source = r#"// Test that we do not yet support elision in associated types, even
// when there is just one name we could take from the impl header.

#![allow(warnings)]

trait MyTrait {
    type Output;
}

impl MyTrait for &i32 {
    type Output = &i32;
    //~^ ERROR missing lifetime in associated type
}

impl MyTrait for &u32 {
    type Output = &'_ i32;
    //~^ ERROR `'_` cannot be used here
}

impl<'a> MyTrait for &f64 {
    type Output = &f64;
    //~^ ERROR missing lifetime in associated type
}

trait OtherTrait<'a> {
    type Output;
}
impl OtherTrait<'_> for f64 {
    type Output = &f64;
    //~^ ERROR missing lifetime in associated type
}

// This is what you have to do:
impl<'a> MyTrait for &'a f32 {
    type Output = &'a f32;
}

fn main() { }
"#;
    let stderr = r#"error: missing lifetime in associated type
  --> $DIR/assoc-type.rs:11:19
   |
LL |     type Output = &i32;
   |                   ^ this lifetime must come from the implemented type
   |
   = note: in the trait the associated type is declared without lifetime parameters, so using a borrowed type for them requires that lifetime to come from the implemented type
help: add a lifetime to the impl block and use it in the self type and associated type
   |
LL ~ impl<'a> MyTrait for &'a i32 {
LL ~     type Output = &'a i32;
   |

error[E0637]: `'_` cannot be used here
  --> $DIR/assoc-type.rs:16:20
   |
LL |     type Output = &'_ i32;
   |                    ^^ `'_` is a reserved lifetime name

error: missing lifetime in associated type
  --> $DIR/assoc-type.rs:21:19
   |
LL | impl<'a> MyTrait for &f64 {
   |     ---- there is a named lifetime specified on the impl block you could use
LL |     type Output = &f64;
   |                   ^ this lifetime must come from the implemented type
   |
   = note: in the trait the associated type is declared without lifetime parameters, so using a borrowed type for them requires that lifetime to come from the implemented type
help: consider using the lifetime from the impl block
   |
LL |     type Output = &'a f64;
   |                    ++

error: missing lifetime in associated type
  --> $DIR/assoc-type.rs:29:19
   |
LL |     type Output = &f64;
   |                   ^ this lifetime must come from the implemented type
   |
   = note: in the trait the associated type is declared without lifetime parameters, so using a borrowed type for them requires that lifetime to come from the implemented type
help: add a lifetime to the impl block and use it in the trait and associated type
   |
LL ~ impl<'a> OtherTrait<'a> for f64 {
LL ~     type Output = &'a f64;
   |

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0637`.
"#;
    assert_eq!(ui("assoc-type.rs", "2015", source), stderr);
}

#[test]
fn an_anonymous_lifetime_may_not_bound_a_lifetime_parameter() {
    // tests/ui/underscore-lifetime/underscore-outlives-bounds.rs
    let source = r#"// Regression test to check that `'b: '_` gets an error, because it's
// basically useless.
//
// #54902

trait Foo<'a> {}
impl<'b: '_> Foo<'b> for i32 {} //~ ERROR `'_` cannot be used here
fn main() { }
"#;
    let stderr = r#"error[E0637]: `'_` cannot be used here
  --> $DIR/underscore-outlives-bounds.rs:7:10
   |
LL | impl<'b: '_> Foo<'b> for i32 {}
   |          ^^ `'_` is a reserved lifetime name

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0637`.
"#;
    assert_eq!(ui("underscore-outlives-bounds.rs", "2015", source), stderr);
}

#[test]
fn a_lifetime_argument_to_a_function_path_with_a_late_bound_lifetime() {
    // tests/ui/late-bound-lifetimes/issue-80618.rs
    let source = r#"fn foo<'a>(x: &'a str) -> &'a str {
    x
}

fn main() {
    let _ = foo::<'static>;
//~^ ERROR cannot specify lifetime arguments explicitly if late bound lifetime parameters are present [E0794]
}
"#;
    let stderr = r#"error[E0794]: cannot specify lifetime arguments explicitly if late bound lifetime parameters are present
  --> $DIR/issue-80618.rs:6:19
   |
LL |     let _ = foo::<'static>;
   |                   ^^^^^^^
   |
note: the late bound lifetime parameter is introduced here
  --> $DIR/issue-80618.rs:1:8
   |
LL | fn foo<'a>(x: &'a str) -> &'a str {
   |        ^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0794`.
"#;
    assert_eq!(ui("issue-80618.rs", "2015", source), stderr);
}

#[test]
fn a_lifetime_argument_to_a_method_call_with_a_late_bound_lifetime_is_a_lint() {
    // tests/ui/lifetimes/late-bound-lifetime-arguments-warning-72278.rs
    let source = r#"// https://github.com/rust-lang/rust/issues/72278
// and https://github.com/rust-lang/rust/issues/42868
//@ run-pass

#![allow(unused)]

struct S;

impl S {
    fn func<'a, U>(&'a self) -> U {
        todo!()
    }
}

fn dont_crash<'a, U>() -> U {
    S.func::<'a, U>()
    //~^ WARN cannot specify lifetime arguments explicitly
    //~| WARN this was previously accepted
}

fn main() {}
"#;
    let stderr = r#"warning: cannot specify lifetime arguments explicitly if late bound lifetime parameters are present
  --> $DIR/late-bound-lifetime-arguments-warning-72278.rs:16:14
   |
LL |     fn func<'a, U>(&'a self) -> U {
   |             -- the late bound lifetime parameter is introduced here
...
LL |     S.func::<'a, U>()
   |              ^^
   |
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #42868 <https://github.com/rust-lang/rust/issues/42868>
   = note: `#[warn(late_bound_lifetime_arguments)]` (part of `#[warn(future_incompatible)]`) on by default

warning: 1 warning emitted

"#;
    assert_eq!(ui_core(Mode::RunPass, "late-bound-lifetime-arguments-warning-72278.rs", "todo", source), stderr);
}

#[test]
fn self_v_naming_both_a_variant_and_an_associated_type_is_ambiguous() {
    // tests/ui/type-alias-enum-variants/enum-variant-priority-lint-ambiguous_associated_items.rs
    let source = r#"// Check that a projection `Self::V` in a trait implementation,
// with an associated type named `V`, for an `enum` with a variant named `V`,
// results in triggering the deny-by-default lint `ambiguous_associated_items`.
// The lint suggests that qualified syntax should be used instead.
// That is, the user would write `<Self as Tr>::V`.
//
// The rationale for this is that while `enum` variants do currently
// not exist in the type namespace but solely in the value namespace,
// RFC #2593 "Enum variant types", would add enum variants to the type namespace.
// However, currently `enum` variants are resolved with high priority as
// they are resolved as inherent associated items.
// Should #2953 therefore be implemented, `Self::V` would suddenly switch
// from referring to the associated type `V` instead of the variant `V`.
// The lint exists to keep us forward compatible with #2593.
//
// As a closing note, provided that #2933 was implemented and
// if `enum` variants were given lower priority than associated types,
// it would be impossible to refer to the `enum` variant `V` whereas
// the associated type could be referred to with qualified syntax as seen above.

enum E {
    V
}

trait Tr {
    type V;
    fn f() -> Self::V;
}

impl Tr for E {
    type V = u8;
    fn f() -> Self::V { 0 }
    //~^ ERROR ambiguous associated item
    //~| WARN this was previously accepted
    //~| HELP use fully-qualified syntax
}

fn main() {}
"#;
    let stderr = r#"error: ambiguous associated item
  --> $DIR/enum-variant-priority-lint-ambiguous_associated_items.rs:32:15
   |
LL |     fn f() -> Self::V { 0 }
   |               ^^^^^^^ help: use fully-qualified syntax: `<E as Tr>::V`
   |
note: `V` could refer to the variant defined here
  --> $DIR/enum-variant-priority-lint-ambiguous_associated_items.rs:22:5
   |
LL |     V
   |     ^
note: `V` could also refer to the associated type defined here
  --> $DIR/enum-variant-priority-lint-ambiguous_associated_items.rs:26:5
   |
LL |     type V;
   |     ^^^^^^
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: for more information, see issue #57644 <https://github.com/rust-lang/rust/issues/57644>
   = note: `#[deny(ambiguous_associated_items)]` (part of `#[deny(future_incompatible)]`) on by default

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("enum-variant-priority-lint-ambiguous_associated_items.rs", "2015", source), stderr);
}
