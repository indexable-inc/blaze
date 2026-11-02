//! rustc's UI tests of the names in signatures: types and traits that name nothing (E0425,
//! E0405) or something else (E0573, E0404), private items (E0603), parameters of an outer item
//! (E0401), undeclared lifetimes (E0261) and re-exports of private items (E0365).

use crate::tests::{assert_reports, checked, error, items::ui_core, ui};

#[test]
fn items_a_macro_makes_in_a_file_of_two_crates_see_their_impl_and_generics() {
    // Our own: llvm-sys's build script and blaze-mem (derives, `lazy_static!`, its `shelves!`),
    // whose files are in two crates; `b` mounts `a`'s root, so either crate is the other's first
    // module of the file.
    let fixture = r#"//- minicore: sized, deref, fn, derive, clone, eq
//- /a/main.rs crate:other:a edition:2021
#[derive(Clone, PartialEq)]
struct Pair(u32, u32);
macro_rules! wrapper {
    ($name:ident, $target:ty) => {
        struct $name($target);
        impl $name {
            fn new(it: $target) -> Self { Self(it) }
        }
        impl core::ops::Deref for $name {
            type Target = $target;
            fn deref(&self) -> &Self::Target { &self.0 }
        }
        fn with<R>(it: &$name, f: impl FnOnce(&$name) -> R) -> R { f(it) }
    };
}
wrapper!(Wrapper, u32);
fn main() {}
//- /b/main.rs crate:other:b edition:2021
#[path = "../a/main.rs"]
mod a;
fn main() {}
"#;
    let messages = checked(fixture).messages();
    let names = ["E0411", "E0425", "E0401", "E0433"];
    assert!(!messages.iter().any(|it| names.iter().any(|code| it.starts_with(code))), "{messages:?}");
}

#[test]
fn a_derive_and_a_macros_impl_in_a_file_of_two_crates_see_self_and_their_generics() {
    // Our own: blaze-mem's derives and llvm-sys's build script, at the root and in an inline
    // `mod`, whose module the walk out of the expansion comes to in the file.
    let fixture = r#"//- minicore: sized, clone, hash, derive
//- /a/main.rs crate:other:a edition:2021
pub trait Tr {
    fn f(&self) -> Self;
}
macro_rules! make {
    ($s:ident) => {
        impl<T: Clone> Tr for $s<T> {
            fn f(&self) -> Self { self.clone() }
        }
    };
}
#[derive(Clone, Hash)]
pub struct S<T>(T);
make!(S);
mod inner {
    use super::Tr;
    #[derive(Clone, Hash)]
    pub struct I<T>(T);
    make!(I);
}
fn main() {}
//- /b/main.rs crate:other:b edition:2021
#[path = "../a/main.rs"]
mod a;
fn main() {}
"#;
    let messages = checked(fixture).messages();
    let names = ["E0411", "E0425", "E0401", "E0433", "E0412"];
    assert!(!messages.iter().any(|it| names.iter().any(|code| it.starts_with(code))), "{messages:?}");
}

#[test]
fn a_signatures_parts_its_cfgs_remove_are_not_resolved() {
    // Our own: object 0.39's read/any.rs, a path crate whose variants name modules only its
    // off features declare. Only the field `cfg(not(..))` keeps is resolved.
    let fixture = r#"//- minicore: sized
//- /lib.rs crate:other:kinds edition:2024
#[cfg(feature = "big")]
pub mod big {
    pub struct Big;
}
pub enum Kind {
    Small(u8),
    #[cfg(feature = "big")]
    Big(big::Big),
    #[cfg_attr(all(), cfg(feature = "big"))]
    Wrapped { it: big::Big },
}
pub struct S {
    pub small: u8,
    #[cfg(feature = "big")]
    pub big: big::Big,
    #[cfg(not(feature = "big"))]
    pub kept: kept::Kept,
}
pub fn f<#[cfg(feature = "big")] T: big::Tr>(#[cfg(feature = "big")] _: big::Big) {}
"#;
    let messages = checked(fixture).messages();
    let unresolved: Vec<&String> = messages.iter().filter(|it| it.starts_with("E0433")).collect();
    assert_eq!(unresolved.len(), 1, "only `kept`, which no cfg removes, is unresolved: {messages:?}");
    assert!(unresolved[0].contains("`kept`"), "{messages:?}");
}

#[test]
fn a_field_a_cfg_removes_after_its_doc_comment_is_not_resolved() {
    // Our own: blaze-core's threads.rs, a field documented before its `#[cfg]` beside a twin of
    // the same name the other cfg keeps. A doc comment is the owner's first child, before the
    // `#[cfg]`.
    let fixture = r#"//- minicore: sized
//- /lib.rs crate:other:threads edition:2024
#[cfg(feature = "linux")]
mod linux {
    pub struct Control;
}
pub struct S {
    /// Stopped watches kept for the next.
    #[cfg(feature = "linux")]
    spare: linux::Control,
    #[cfg(not(feature = "linux"))]
    spare: u8,
    /// Documented, with no cfg.
    kept: kept::Kept,
}
"#;
    let messages = checked(fixture).messages();
    let unresolved: Vec<&String> = messages.iter().filter(|it| it.starts_with("E0433")).collect();
    assert_eq!(unresolved.len(), 1, "only `kept`, which no cfg removes, is unresolved: {messages:?}");
    assert!(unresolved[0].contains("`kept`"), "{messages:?}");
}

#[test]
fn an_impl_of_a_struct_is_not_a_trait() {
    // tests/ui/error-codes/E0404.rs
    let source = "struct Foo;\nstruct Bar;\n\nimpl Foo for Bar {} //~ ERROR E0404\n\nfn main() {}\n\nfn baz<T: Foo>(_: T) {} //~ ERROR E0404\n";
    assert_reports(&ui("E0404.rs", "2015", source), &[error(4, &["`Foo`"]), error(8, &["`Foo`"])]);
}

#[test]
fn an_impl_of_an_unknown_trait() {
    // tests/ui/error-codes/E0405.rs
    let source = "struct Foo;\n\nimpl SomeTrait for Foo {} //~ ERROR E0405\n\nfn main() {\n}\n";
    let stderr = r#"error[E0405]: cannot find trait `SomeTrait` in this scope
  --> $DIR/E0405.rs:3:6
   |
LL | impl SomeTrait for Foo {}
   |      ^^^^^^^^^ not found in this scope

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0405`.
"#;
    assert_eq!(ui("E0405.rs", "2015", source), stderr);
}

#[test]
fn an_impl_of_an_unknown_type() {
    // tests/ui/error-codes/E0412.rs
    let source = "impl Something {} //~ ERROR E0425\n\nfn main() {\n}\n";
    let stderr = r#"error[E0425]: cannot find type `Something` in this scope
  --> $DIR/E0412.rs:1:6
   |
LL | impl Something {}
   |      ^^^^^^^^^ not found in this scope

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0425`.
"#;
    assert_eq!(ui("E0412.rs", "2015", source), stderr);
}

#[test]
fn a_variant_is_not_a_type_but_its_enum_is() {
    // tests/ui/enum/enum-variant-type-2.rs
    let source = "//@ edition:2015\n// Test that enum variants are not actually types.\n\nenum Foo {\n    Bar\n}\n\nfn foo(x: Foo::Bar) {} //~ ERROR expected type, found variant `Foo::Bar`\n\nfn main() {}\n";
    let stderr = r#"error[E0573]: expected type, found variant `Foo::Bar`
  --> $DIR/enum-variant-type-2.rs:8:11
   |
LL | fn foo(x: Foo::Bar) {}
   |           ^^^^^^^^ not a type
   |
help: try using the variant's enum
   |
LL - fn foo(x: Foo::Bar) {}
LL + fn foo(x: Foo) {}
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0573`.
"#;
    assert_eq!(ui("enum-variant-type-2.rs", "2015", source), stderr);
}

#[test]
fn a_module_is_not_a_type_but_a_trait_of_a_similar_name_is() {
    // tests/ui/traits/impl-for-module.rs
    let source = "mod a {\n}\n\ntrait A {\n}\n\nimpl A for a { //~ ERROR expected type, found module\n}\n\nfn main() {\n}\n";
    let stderr = r#"error[E0573]: expected type, found module `a`
  --> $DIR/impl-for-module.rs:7:12
   |
LL | trait A {
   | ------- similarly named trait `A` defined here
...
LL | impl A for a {
   |            ^
   |
help: a trait with a similar name exists
   |
LL - impl A for a {
LL + impl A for A {
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0573`.
"#;
    assert_eq!(ui("impl-for-module.rs", "2015", source), stderr);
}

#[test]
fn a_const_parameter_is_not_a_type() {
    // tests/ui/const-generics/struct-with-invalid-const-param.rs
    let source = "// Checks that a const param cannot be stored in a struct.\n\nstruct S<const C: u8>(C); //~ ERROR cannot find type `C` in this scope\n\nfn main() {}\n";
    let stderr = r#"error[E0573]: cannot find type `C` in this scope
  --> $DIR/struct-with-invalid-const-param.rs:3:23
   |
LL | struct S<const C: u8>(C);
   |                       ^ not found in this scope
   |
   = note: a const parameter named `C` exists in another namespace

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0573`.
"#;
    assert_eq!(ui("struct-with-invalid-const-param.rs", "2015", source), stderr);
}

#[test]
fn a_private_struct_of_a_module_in_a_signature() {
    // tests/ui/privacy/struct-field-and-impl-expose-10545.rs
    let source = "//! Regression test for https://github.com/rust-lang/rust/issues/10545\n\nmod a {\n    struct S;\n    impl S { }\n}\n\nfn foo(_: a::S) { //~ ERROR: struct `S` is private\n}\n\nfn main() {}\n";
    assert_reports(&ui("struct-field-and-impl-expose-10545.rs", "2015", source), &[error(8, &["`S`"])]);
}

#[test]
fn an_inner_function_cannot_use_a_parameter_of_its_outer_one() {
    // tests/ui/resolve/bad-type-env-capture.rs
    let source = "fn foo<T>() {\n    fn bar(b: T) { } //~ ERROR can't use generic parameters from outer\n}\nfn main() { }\n";
    let stderr = r#"error[E0401]: can't use generic parameters from outer item
  --> $DIR/bad-type-env-capture.rs:2:15
   |
LL | fn foo<T>() {
   |        - type parameter from outer item
LL |     fn bar(b: T) { }
   |        ---    ^ use of generic parameter from outer item
   |        |
   |        generic parameter used in this inner function
   |
   = note: nested items are independent from their parent item for everything except for privacy and name resolution
help: try introducing a local generic parameter here
   |
LL |     fn bar<T>(b: T) { }
   |           +++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0401`.
"#;
    assert_eq!(ui("bad-type-env-capture.rs", "2015", source), stderr);
}

#[test]
fn an_inner_enum_cannot_use_a_parameter_of_its_outer_function() {
    // tests/ui/generics/enum-definition-with-outer-generic-parameter-5997.rs
    let source = r#"// https://github.com/rust-lang/rust/issues/5997
fn f<Z>() -> bool {
    enum E { V(Z) }
    //~^ ERROR can't use generic parameters from outer item
    true
}

fn main() {
    let b = f::<isize>();
    assert!(b);
}
"#;
    assert_reports(
        &ui_core("enum-definition-with-outer-generic-parameter-5997.rs", "2015", "assert, panic, unary_ops", source),
        &[
            error(3, &[]),
        ],
    );
}

#[test]
fn a_struct_in_a_closure_of_an_impl_cannot_use_the_impls_parameter() {
    // tests/ui/generics/issue-98432.rs
    let source = r#"struct Struct<T>(T);

impl<T> Struct<T> {
    const CONST: fn() = || {
        struct _Obligation where T:; //~ ERROR can't use generic parameters from outer item
    };
}

fn main() {}
"#;
    let stderr = r#"error[E0401]: can't use generic parameters from outer item
  --> $DIR/issue-98432.rs:5:34
   |
LL | impl<T> Struct<T> {
   |      - type parameter from outer item
LL |     const CONST: fn() = || {
LL |         struct _Obligation where T:;
   |                -----------       ^ use of generic parameter from outer item
   |                |
   |                generic parameter used in this inner struct
   |
   = note: nested items are independent from their parent item for everything except for privacy and name resolution
help: try introducing a local generic parameter here
   |
LL |         struct _Obligation<T> where T:;
   |                           +++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0401`.
"#;
    assert_eq!(ui("issue-98432.rs", "2015", source), stderr);
}

#[test]
fn a_crate_private_module_cannot_be_re_exported() {
    // tests/ui/error-codes/E0365.rs
    let source = "mod foo {\n    pub const X: u32 = 1;\n}\n\npub use foo as foo2;\n//~^ ERROR `foo` is only public within the crate, and cannot be re-exported outside [E0365]\n\nfn main() {}\n";
    let stderr = r#"error[E0365]: `foo` is only public within the crate, and cannot be re-exported outside
  --> $DIR/E0365.rs:5:9
   |
LL | pub use foo as foo2;
   |         ^^^^^^^^^^^ re-export of crate public `foo`
   |
   = note: consider declaring type or module `foo` with `pub`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0365`.
"#;
    assert_eq!(ui("E0365.rs", "2015", source), stderr);
}

#[test]
fn an_undeclared_lifetime_suggests_introducing_it_on_the_item() {
    // tests/ui/error-codes/E0261.rs
    let source = r#"fn foo(x: &'a str) { } //~ ERROR E0261
                       //~| NOTE undeclared lifetime

struct Foo {
    x: &'a str, //~ ERROR E0261
                //~| NOTE undeclared lifetime
}

fn main() {}
"#;
    let stderr = r#"error[E0261]: use of undeclared lifetime name `'a`
  --> $DIR/E0261.rs:1:12
   |
LL | fn foo(x: &'a str) { }
   |            ^^ undeclared lifetime
   |
help: consider introducing lifetime `'a` here
   |
LL | fn foo<'a>(x: &'a str) { }
   |       ++++

error[E0261]: use of undeclared lifetime name `'a`
  --> $DIR/E0261.rs:5:9
   |
LL |     x: &'a str,
   |         ^^ undeclared lifetime
   |
help: consider introducing lifetime `'a` here
   |
LL | struct Foo<'a> {
   |           ++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0261`.
"#;
    assert_eq!(ui("E0261.rs", "2015", source), stderr);
}
