//! rustc's UI tests of `refining_impl_trait` (`refining_impl_trait_reachable` and
//! `refining_impl_trait_internal`): a trait impl's method that promises more than the
//! `impl Trait` its trait's method returns. Each test names the UI test it comes from
//! (`tests/ui/impl-trait/in-trait/`) and says where its source differs. Each checks the facts
//! a user acts on (which lint, at which line, the bound or the `use<..>` it blames and the type
//! it suggests), not rustc's text byte for byte.

use crate::tests::{Expect, assert_reports, error, warning};

const MESSAGE: &str = "impl method signature does not match trait method signature";
const CAPTURES: &str = "impl trait in impl method captures fewer lifetimes than in trait";

/// `refine.rs`, its source byte for byte: each impl refines the trait's `impl Sized`, by a
/// stronger bound (`impl Copy`) or a concrete type (`()`, or none written).
const REFINE: &str = r#"#![deny(refining_impl_trait)]

pub trait Foo {
    fn bar() -> impl Sized;
}

pub struct A;
impl Foo for A {
    fn bar() -> impl Copy {}
    //~^ ERROR impl method signature does not match trait method signature
}

pub struct B;
impl Foo for B {
    fn bar() {}
    //~^ ERROR impl method signature does not match trait method signature
}

pub struct C;
impl Foo for C {
    fn bar() -> () {}
    //~^ ERROR impl method signature does not match trait method signature
}

struct Private;
impl Foo for Private {
    fn bar() -> () {}
    //~^ ERROR impl method signature does not match trait method signature
}

pub trait Arg<A> {
    fn bar() -> impl Sized;
}
impl Arg<Private> for A {
    fn bar() -> () {}
    //~^ ERROR impl method signature does not match trait method signature
}

pub trait Late {
    fn bar<'a>(&'a self) -> impl Sized + 'a;
}

pub struct D;
impl Late for D {
    fn bar(&self) -> impl Copy + '_ {}
    //~^ ERROR impl method signature does not match trait method signature
}

mod unreachable {
    pub trait UnreachablePub {
        fn bar() -> impl Sized;
    }

    struct E;
    impl UnreachablePub for E {
        fn bar() {}
        //~^ ERROR impl method signature does not match trait method signature
    }
}

fn main() {}
"#;

fn refine_stderr(source: &str) -> String {
    super::ui_core("refine.rs", "2015", "sized, copy", source)
}

#[test]
fn an_impl_refining_the_trait_s_impl_trait_is_linted() {
    // tests/ui/impl-trait/in-trait/refine.rs
    let stderr = refine_stderr(REFINE);
    let stronger = "this bound is stronger than that defined on the trait";
    assert_reports(
        &stderr,
        &[
            error(
                9,
                &[
                    MESSAGE,
                    stronger,
                    "-> impl Sized {}",
                    "return type from trait method defined here",
                ],
            ),
            error(15, &[MESSAGE, "fn bar() -> impl Sized {}"]),
            error(21, &[MESSAGE, "fn bar() -> impl Sized {}"]),
            error(27, &[MESSAGE]),
            error(35, &[MESSAGE]),
            error(45, &[MESSAGE, stronger, "impl Sized + '_"]),
            error(56, &[MESSAGE]),
        ],
    );
    // `impl Copy` is blamed on its bound, `Copy`, not on the whole return type.
    assert!(stderr.contains("  --> $DIR/refine.rs:9:22\n"), "{stderr}");
    assert!(stderr.contains("  --> $DIR/refine.rs:45:27\n"), "{stderr}");
}

#[test]
fn which_lint_follows_who_can_rely_on_the_impl() {
    // tests/ui/impl-trait/in-trait/refine.rs denying `refining_impl_trait_reachable` alone:
    // an impl whose trait no other crate reaches (line 56) or whose trait reference names a
    // type that is not `pub` (`Private`, lines 27 and 35) is `refining_impl_trait_internal`,
    // still a warning.
    let source = REFINE.replacen(
        "#![deny(refining_impl_trait)]",
        "#![deny(refining_impl_trait_reachable)]",
        1,
    );
    let stderr = refine_stderr(&source);
    let internal: [Expect<'_>; 3] =
        [warning(27, &[MESSAGE]), warning(35, &[MESSAGE]), warning(56, &[MESSAGE])];
    let mut expected = vec![
        error(9, &[MESSAGE]),
        error(15, &[MESSAGE]),
        error(21, &[MESSAGE]),
        error(45, &[MESSAGE]),
    ];
    expected.extend(internal);
    assert_reports(&stderr, &expected);
    assert!(stderr.contains("`#[warn(refining_impl_trait_internal)]`"), "{stderr}");
}

#[test]
fn an_impl_trait_capturing_fewer_lifetimes_is_linted() {
    // tests/ui/impl-trait/in-trait/refine-captures.rs without its last impl, `impl<T>
    // TypeParam<T> for u64` with `use<>`: that is rustc's error "`impl Trait` must mention all
    // type parameters in scope in `use<...>`", which blaze does not give, and no refinement.
    let source = r#"trait LifetimeParam<'a> {
    fn test() -> impl Sized;
}
// Refining via capturing fewer lifetimes than the trait definition.
impl<'a> LifetimeParam<'a> for i32 {
    fn test() -> impl Sized + use<> {}
    //~^ WARN impl trait in impl method captures fewer lifetimes than in trait
}
// If the lifetime is substituted, then we don't refine anything.
impl LifetimeParam<'static> for u32 {
    fn test() -> impl Sized + use<> {}
    // Ok
}

trait TypeParam<T> {
    fn test() -> impl Sized;
}
// Indirectly capturing a lifetime param through a type param substitution.
impl<'a> TypeParam<&'a ()> for i32 {
    fn test() -> impl Sized + use<> {}
    //~^ WARN impl trait in impl method captures fewer lifetimes than in trait
}
// Two of them, but only one is captured...
impl<'a, 'b> TypeParam<(&'a (), &'b ())> for u32 {
    fn test() -> impl Sized + use<'b> {}
    //~^ WARN impl trait in impl method captures fewer lifetimes than in trait
}

fn main() {}
"#;
    let stderr = super::ui_core("refine-captures.rs", "2015", "sized", source);
    assert_reports(
        &stderr,
        &[
            warning(6, &[CAPTURES, "refining_impl_trait_internal", "use<'a>"]),
            warning(20, &[CAPTURES, "use<'a>"]),
            warning(25, &[CAPTURES, "use<'a, 'b>"]),
        ],
    );
    // `'static` for the trait's lifetime leaves nothing to capture (line 11).
    assert_eq!(stderr.matches(CAPTURES).count(), 3, "{stderr}");
    assert!(
        stderr.contains("  --> $DIR/refine-captures.rs:6:31\n"),
        "the `use<>` is blamed:\n{stderr}"
    );
}

/// The output of `source`, which must have no error and no `refining_impl_trait`.
fn assert_no_refinement(name: &str, edition: &str, flags: &str, source: &str) {
    let stderr = super::ui_core(name, edition, flags, source);
    assert_reports(&stderr, &[]);
    assert!(!stderr.contains(MESSAGE) && !stderr.contains(CAPTURES), "{stderr}");
}

#[test]
fn a_projection_the_impl_writes_normalized_is_no_refinement() {
    // tests/ui/impl-trait/in-trait/refine-normalize.rs (its `current` revision; blaze has one
    // solver): `Iterator<Item = Self::Item>` and `Iterator<Item = ()>` are the same bound once
    // `Self::Item` is normalized.
    let source = r#"//@ check-pass
//@ edition: 2021
//@ revisions: current next
//@ ignore-compare-mode-next-solver (explicit revisions)
//@[next] compile-flags: -Znext-solver

#![deny(refining_impl_trait)]

pub trait Foo {
    type Item;

    fn hello() -> impl Iterator<Item = Self::Item>;
}

impl Foo for () {
    type Item = ();

    fn hello() -> impl Iterator<Item = ()> { [()].into_iter() }
}

fn main() {}
"#;
    assert_no_refinement("refine-normalize.rs", "2021", "iterators_more", source);
}

#[test]
fn a_higher_ranked_bound_written_alike_is_no_refinement() {
    // tests/ui/impl-trait/in-trait/anonymize-binders-for-refine.rs, with a `main` (rustc builds
    // it as a library): the `for<'a>` of each side is its own, the same once anonymous.
    let source = r#"//@ compile-flags: --crate-type=lib
//@ check-pass

#![deny(refining_impl_trait)]

pub trait Tr<T> {
    fn foo() -> impl for<'a> Tr<&'a Self>;
}

impl<T> Tr<T> for () {
    fn foo() -> impl for<'a> Tr<&'a Self> {}
}

fn main() {}
"#;
    assert_no_refinement("anonymize-binders-for-refine.rs", "2015", "sized", source);
    // Named otherwise, the same bound still: a binder's names are not compared.
    let renamed =
        source.replacen("impl for<'a> Tr<&'a Self> {}", "impl for<'b> Tr<&'b Self> {}", 1);
    assert_ne!(renamed, source);
    assert_no_refinement("anonymize-binders-for-refine.rs", "2015", "sized", &renamed);
}

#[test]
fn another_crate_s_trait_is_linted_as_this_crate_s() {
    // tests/ui/impl-trait/in-trait/foreign.rs with its `auxiliary/rpitit.rs`, with
    // `&'static u8` for `Arc<String>` (the fixture has no std): refining another crate's
    // trait is internal when the impl's type is not `pub`, and the lint obeys the levels on
    // the method.
    let fixture = r#"//- minicore: sized, deref
//- /rpitit.rs crate:rpitit
use core::ops::Deref;

pub trait Foo {
    fn bar(self) -> impl Deref<Target = impl Sized>;
}

pub struct Foreign;
impl Foo for Foreign {
    #[expect(refining_impl_trait)]
    fn bar(self) -> &'static () {
        &()
    }
}
//- /foreign.rs crate:other:main edition:2015 deps:rpitit
extern crate rpitit;

use rpitit::{Foo, Foreign};

// Implement an RPITIT from another crate.
pub struct Local;
impl Foo for Local {
    #[expect(refining_impl_trait)]
    fn bar(self) -> &'static u8 {
        &0
    }
}

struct LocalOnlyRefiningA;
impl Foo for LocalOnlyRefiningA {
    #[warn(refining_impl_trait)]
    fn bar(self) -> &'static u8 {
        //~^ WARN impl method signature does not match trait method signature
        &0
    }
}

struct LocalOnlyRefiningB;
impl Foo for LocalOnlyRefiningB {
    #[warn(refining_impl_trait)]
    #[allow(refining_impl_trait_reachable)]
    fn bar(self) -> &'static u8 {
        //~^ WARN impl method signature does not match trait method signature
        &0
    }
}

struct LocalOnlyRefiningC;
impl Foo for LocalOnlyRefiningC {
    #[warn(refining_impl_trait)]
    #[allow(refining_impl_trait_internal)]
    fn bar(self) -> &'static u8 {
        &0
    }
}

fn main() {
    // Witness an RPITIT from another crate.
    let &() = Foreign.bar();
}
"#;
    let stderr = super::ui_fixture(fixture);
    assert_reports(
        &stderr,
        &[
            warning(17, &[MESSAGE, "refining_impl_trait_internal", "Deref<Target = impl Sized>"]),
            warning(27, &[MESSAGE]),
        ],
    );
    // `Local`'s is expected (no `unfulfilled_lint_expectations`) and `LocalOnlyRefiningC`'s
    // allowed; another crate's trait has no span of its own to show.
    assert_eq!(stderr.matches(MESSAGE).count(), 2, "{stderr}");
    assert!(
        !stderr.contains("unfulfilled") && !stderr.contains("return type from trait method"),
        "{stderr}"
    );
}

#[test]
fn a_method_that_does_not_compare_with_the_trait_s_is_not_linted() {
    // No rustc UI test: rustc checks `refining_impl_trait` only once `compare_impl_item` found
    // no error, so an impl method with a parameter too many has E0050 and nothing else.
    let source = r#"#![deny(refining_impl_trait)]

pub trait Foo {
    fn bar() -> impl Sized;
}

pub struct A;
impl Foo for A {
    fn bar(x: u8) {}
    //~^ ERROR E0050
}

fn main() {}
"#;
    let stderr = super::ui_core("refine-e0050.rs", "2015", "sized", source);
    assert_reports(&stderr, &[error(9, &["E0050"])]);
    assert!(!stderr.contains(MESSAGE), "{stderr}");
}
