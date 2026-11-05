//! rustc's UI tests of a trait impl's method against its trait's: the impl's types for the
//! trait method's return `impl Trait`s (`collect_return_position_impl_trait_in_trait_tys`:
//! E0053 "incompatible return type", the bounds they must meet, E0277, and the lifetimes they
//! may name) and the types of the signatures (`compare_method_predicate_entailment`, E0053).
//! Each test's file is the UI test's source byte for byte, or the revision named.

use super::impls::{ui_core, ui_std};
use crate::tests::{assert_reports, error};

#[test]
fn a_return_type_with_no_type_for_the_traits_impl_trait() {
    // tests/ui/impl-trait/in-trait/deep-match.rs
    let source = "struct Wrapper<T>(T);

trait Foo {
    fn bar() -> Wrapper<impl Sized>;
}

impl Foo for () {
    fn bar() -> i32 {
        //~^ ERROR method `bar` has an incompatible return type for trait
        0
    }
}

fn main() {}
";
    let stderr = ui_core("deep-match.rs", "sized", source);
    assert_reports(
        &stderr,
        &[error(
            8,
            &[
                "E0053",
                "method `bar` has an incompatible return type for trait",
                "`Wrapper<_>`",
                "`i32`",
            ],
        )],
    );
}

#[test]
fn an_impls_type_that_fails_the_bound_of_the_traits_impl_trait() {
    // tests/ui/impl-trait/in-trait/doesnt-satisfy.rs
    let source = "trait Foo {
    fn bar() -> impl std::fmt::Display;
}

impl Foo for () {
    fn bar() -> () {}
    //~^ ERROR `()` doesn't implement `std::fmt::Display`
}

fn main() {}
";
    let stderr = ui_std("doesnt-satisfy.rs", "sized, fmt", "pub use core::fmt;\n", "", source);
    assert_reports(
        &stderr,
        &[error(
            6,
            &["E0277", "Display", "()", "required by a bound in `Foo::bar::{anon_assoc#0}`"],
        )],
    );
}

#[test]
fn the_traits_impl_trait_bound_with_the_impls_arguments() {
    // tests/ui/impl-trait/in-trait/return-dont-satisfy-bounds.rs. rustc also gives E0276 for
    // `F2: Foo<u8>`, and E0277 for `Bar: Foo<u8>`, the impl's own `impl Foo<u8>` not met by
    // its body's `self`; blaze reports neither yet, and neither is the trait's `impl Trait`.
    let source = "trait Foo<T> {
    fn foo<F2>(self) -> impl Foo<T>;
}

struct Bar;

impl Foo<char> for Bar {
    fn foo<F2: Foo<u8>>(self) -> impl Foo<u8> {
        //~^ ERROR: the trait bound `impl Foo<u8>: Foo<char>` is not satisfied [E0277]
        //~| ERROR: the trait bound `Bar: Foo<u8>` is not satisfied [E0277]
        //~| ERROR: impl has stricter requirements than trait
        self
    }
}

fn main() {}
";
    let stderr = ui_core("return-dont-satisfy-bounds.rs", "sized", source);
    assert_reports(
        &stderr,
        &[error(8, &["E0277", "`impl Foo<u8>: Foo<char>`", "Foo::foo::{anon_assoc#0}"])],
    );
}

#[test]
fn a_parameters_type_differs_where_the_trait_returns_impl_trait() {
    // tests/ui/impl-trait/in-trait/method-signature-matches.rs, revision `mismatch`
    let source = "//@ edition: 2021

trait Uwu {
    fn owo(x: ()) -> impl Sized;
}

impl Uwu for () {
    fn owo(_: u8) {}
    //[mismatch]~^ ERROR method `owo` has an incompatible type for trait
}

fn main() {}
";
    let stderr = ui_core("method-signature-matches.rs", "sized", source);
    assert_reports(
        &stderr,
        &[error(
            8,
            &[
                "E0053",
                "incompatible type for trait",
                "expected `()`, found `u8`",
                "fn(())",
                "fn(u8)",
            ],
        )],
    );
}

#[test]
fn a_lifetimes_type_differs_where_the_trait_returns_impl_trait() {
    // tests/ui/impl-trait/in-trait/method-signature-matches.rs, revision `lt`
    let source = "//@ edition: 2021

trait Lifetimes {
    fn early<'early, T>(x: &'early T) -> impl Sized;
}

impl Lifetimes for () {
    fn early<'late, T>(_: &'late ()) {}
    //[lt]~^ ERROR method `early` has an incompatible type for trait
}

fn main() {}
";
    let stderr = ui_core("method-signature-matches.rs", "sized", source);
    assert_reports(&stderr, &[error(8, &["E0053", "incompatible type for trait", "`T`", "`()`"])]);
}

#[test]
fn an_impls_type_that_captures_a_lifetime_the_trait_leaves_out() {
    // tests/ui/impl-trait/precise-capturing/rpitit-impl-captures-too-much.rs
    let source = "struct Invariant<'a>(&'a mut &'a mut ());

trait Trait {
    fn hello(self_: Invariant<'_>) -> impl Sized + use<Self>;
}

impl Trait for () {
    fn hello(self_: Invariant<'_>) -> impl Sized + use<'_> {}
    //~^ ERROR return type captures more lifetimes than trait definition
}

fn main() {}
";
    let stderr = ui_core("rpitit-impl-captures-too-much.rs", "sized", source);
    assert_reports(
        &stderr,
        &[error(8, &["return type captures more lifetimes than trait definition", "hidden type"])],
    );
}

/// An impl's `impl Trait` that leaves out a lifetime the trait's leaves out too names none it
/// does not capture: the lifetimes an opaque type does not capture are not remapped.
#[test]
fn an_impls_type_that_captures_no_more_than_the_traits() {
    let source = "struct Invariant<'a>(&'a mut &'a mut ());

trait Trait {
    fn hello(self_: Invariant<'_>) -> impl Sized + use<Self>;
}

impl Trait for () {
    fn hello(self_: Invariant<'_>) -> impl Sized + use<> {}
}

fn main() {}
";
    let stderr = ui_core("rpitit-impl-captures-no-more.rs", "sized", source);
    assert_reports(&stderr, &[]);
}

#[test]
fn each_type_that_differs_in_a_methods_signature() {
    // tests/ui/traits/issue-35869.rs
    let source = "trait Foo {
    fn foo(_: fn(u8) -> ());
    fn bar(_: Option<u8>);
    fn baz(_: (u8, u16));
    fn qux() -> u8;
}

struct Bar;

impl Foo for Bar {
    fn foo(_: fn(u16) -> ()) {}
    //~^ ERROR method `foo` has an incompatible type for trait
    fn bar(_: Option<u16>) {}
    //~^ ERROR method `bar` has an incompatible type for trait
    fn baz(_: (u16, u16)) {}
    //~^ ERROR method `baz` has an incompatible type for trait
    fn qux() -> u16 { 5u16 }
    //~^ ERROR method `qux` has an incompatible type for trait
}

fn main() {}
";
    let stderr = ui_core("issue-35869.rs", "sized, option", source);
    assert_reports(
        &stderr,
        &[
            error(11, &["E0053", "method `foo`", "fn(u8)", "fn(u16)", "type in trait"]),
            error(13, &["E0053", "method `bar`", "Option<u8>", "Option<u16>"]),
            error(15, &["E0053", "method `baz`", "(u8, u16)", "(u16, u16)"]),
            error(17, &["E0053", "method `qux`", "expected `u8`, found `u16`"]),
        ],
    );
}

#[test]
fn an_unsafe_method_for_a_safe_ones_signature() {
    // tests/ui/traits/impl-method-mismatch.rs
    let source = "//@ dont-require-annotations: NOTE

trait Mumbo {
    fn jumbo(&self, x: &usize) -> usize;
}

impl Mumbo for usize {
    // Cannot have a larger effect than the trait:
    unsafe fn jumbo(&self, x: &usize) { *self + *x; }
    //~^ ERROR method `jumbo` has an incompatible type for trait
    //~| NOTE expected signature `fn
    //~| NOTE found signature `unsafe fn
}

fn main() {}
";
    let stderr = ui_core("impl-method-mismatch.rs", "sized, add, deref, copy", source);
    assert_reports(
        &stderr,
        &[error(
            9,
            &[
                "E0053",
                "method `jumbo` has an incompatible type for trait",
                "expected signature `fn",
                "found signature `unsafe fn",
            ],
        )],
    );
}

/// The `[const]` bound of a const trait's `impl Trait` is the trait item's: a const caller
/// generic over `T: [const] Foo` calls `[const] Bar` methods on `T::method`'s value.
#[test]
fn a_const_bound_on_the_traits_impl_trait_holds_in_a_const_caller() {
    // tests/ui/traits/const-traits/const-cond-for-rpitit.rs, with a const caller added
    let source = "//@ compile-flags: -Znext-solver
//@ check-pass

#![feature(const_trait_impl)]
#![allow(refining_impl_trait)]

pub const trait Foo {
    fn method(self) -> impl [const] Bar;
}

pub const trait Bar {
    fn bar(self);
}

struct A<T>(T);
const impl<T> Foo for A<T>
where
    A<T>: [const] Bar,
{
    fn method(self) -> impl [const] Bar {
        self
    }
}

const fn call<T: [const] Foo>(t: T) {
    t.method().bar()
}

fn main() {}
";
    let stderr = ui_core("const-cond-for-rpitit.rs", "sized", source);
    assert_reports(&stderr, &[]);
}
