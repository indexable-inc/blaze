//! blaze's own macros, the ones the paths `blaze::…` name that expand (the attribute
//! `blaze::contract_test`, the call `blaze::unreachable!()`): blaze's
//! extension, not the Rust project's (`library/UPSTREAM` lists this module among blaze's changes
//! to the standard library).
//!
//! A path `blaze::NAME` in an attribute resolves to `core::blaze::NAME` (blaze's name resolution,
//! `hir-def` `nameres/path_resolution.rs`), whatever else `blaze` names in the crate, so an
//! attribute here is found in every crate that has `core`. Every item is unstable under the
//! feature its extension is gated by, so a crate that does not enable it can use none of them, and
//! nothing here changes what any other item of `core` does. rustc has neither the feature nor the
//! builtin, so a program that uses one is refused there, never built differently.

/// `#[blaze::contract_test]` on a function with native contract clauses
/// (`fn f(x: u32) -> (r: u32) requires x < 10 ensures r > x { .. }`, `#![feature(blaze_verify)]`)
/// keeps the function as written and adds, under `cfg(test)`, a property test of its contract:
/// each case draws every argument with Hegel's default generator for its type, rejects the case
/// unless each `requires` holds, calls the function and checks each `ensures` on what it
/// returned. A failure names the function and the clause and prints the shrunk case as a plain
/// `#[test]` to paste and keep. The test calls the `hegel` crate (`hegeltest`), which the crate
/// names as a dev-dependency. `#[blaze::contract_test(skip)]` keeps the function and adds nothing
/// (docs/v2/plan/design/contracts.md, part 2).
#[unstable(feature = "blaze_verify", issue = "none")]
#[rustc_builtin_macro]
pub macro contract_test($item:item) {
    /* compiler built-in */
}

/// `blaze::unreachable!()` marks a place blaze must prove no path reaches
/// (docs/v2/plan/design/proved-unreachable.md): the lint `blaze::unreachable` refuses a call
/// its proof of the function (`hir_ty_pure::reach`: the variants, bounds, lengths and non-zero
/// divisors the checks that dominate the call establish) does not rule out, naming the
/// function and what could not be shown. A path `blaze::unreachable` that names no macro of
/// the crate's own resolves here. It expands to a panic, never to undefined behaviour, so a
/// build that did not run the check is still sound. rustc has no such builtin, so a program
/// that uses it is refused there, never built differently.
#[unstable(feature = "blaze_unreachable", issue = "none")]
#[rustc_builtin_macro(blaze_unreachable)]
pub macro unreachable() {
    /* compiler built-in */
}

/// What carries the name of blaze's generic const expressions,
/// `#![feature(blaze_const_exprs)]`, in the library's table of features: a const generic
/// argument or an array length that is an expression over generic parameters
/// (`Shares<T, K, { N + M }, D>`), evaluated per instance and unified by a normal form by
/// blaze's own front end (docs/v2/plan/design/ownership.md). The feature is the language's;
/// no program names this type. rustc has neither, so a crate that enables the feature is
/// refused there.
#[unstable(feature = "blaze_const_exprs", issue = "none")]
pub struct ConstExprs;

/// What carries the name of blaze's implied bounds, `#![feature(blaze_implied_bounds)]`, in the
/// library's table of features: the bounds a struct, enum, union or trait declares
/// (`struct C<A: X>`) hold in every function, impl and trait whose header names it
/// (`impl<A> C<A> {}` has `A: X`) without being written again
/// (docs/v2/plan/design/implied-bounds.md). The feature is the language's; no program names
/// this type. rustc has neither, so a crate that enables the feature is refused there.
#[unstable(feature = "blaze_implied_bounds", issue = "none")]
pub struct ImpliedBounds;

/// What carries the name of blaze's anonymous parameters, `#![feature(blaze_anon_params)]`,
/// in the library's table of features: a `_` type argument in a function's parameter types or
/// an impl's header is a parameter of its own that no one names (`fn len<T>(v: &Vec<T, _>)`),
/// bounded by what its position implies (docs/v2/plan/design/implied-bounds.md). The feature
/// is the language's; no program names this type. rustc has neither, so a crate that enables
/// the feature is refused there.
#[unstable(feature = "blaze_anon_params", issue = "none")]
pub struct AnonParams;
