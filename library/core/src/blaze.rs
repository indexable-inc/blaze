//! blaze's own attribute macros, the ones the attribute paths `blaze::…` name that expand: blaze's
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
