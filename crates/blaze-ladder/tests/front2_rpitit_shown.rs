//! A trait fn's `impl Trait` return type as a refusal prints it (rule 19: an error names the
//! thing). RFC 3425: each such `impl Trait` is an associated type of the trait,
//! `<Self as Tr>::{synthetic}`, with no written name; blaze files it with the trait's synthetic
//! rows (front2 rpitit.rs, `Store::rpitit_traits`), not with the written members
//! (`Store::trait_defs[..].assoc`). front2 check.rs `assoc_shown` read only the written members, so
//! every such type printed "(an associated type of a trait whose members are not filed, so its
//! name is not known here)" though it was filed (tempo a0700, crates/node/src/executed_state.rs:123).
//! It now prints `{the impl Trait returned by <the trait fn's path>}`.
//!
//! The same type reaches a finding in two forms (front2 rpitit.rs's module doc): the trait fn's
//! return type keeps the OPAQUE `Opaque(o, ..)` at every use, and reveals the projection
//! `<S as Tr>::{o}`. Both print the one text (check.rs `synthetic_shown`, rule 30); the opaque
//! printed "impl Trait (the `impl Trait` type #0 of `ladder::Tr::make`)" before (run of
//! 2026-11-21 14:20), which read as a free fn's opaque.
//!
//! - `a_synthetic_projection_is_named_by_its_trait_fn`: in a generic body `s.make()` is the trait
//!   fn's `impl Copy` at `S`; used as `u32` it is E0308 (rustc: "expected `u32`, found
//!   associated type", the type printed as the `impl Copy` the trait fn wrote), the text naming
//!   `ladder::Tr::make` as the fn that returns it and never "not filed".
//! - CONTROL `a_written_associated_type_is_named_by_its_path`: a written `type A` in the same
//!   place prints by its own path, as before.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct X;
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal as printed.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}"))
}

#[test]
fn a_synthetic_projection_is_named_by_its_trait_fn() {
    let got = run("
trait Tr {
    fn make(&self) -> impl Copy;
}

impl Tr for X {
    fn make(&self) -> u8 {
        1
    }
}

fn bad<S: Tr>(s: &S) -> u32 {
    let v: u32 = s.make();
    v
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    bad(&X) as i32
}
");
    println!("RPITIT SHOWN: {got:?}");
    let Err(why) = got else { panic!("`<S as Tr>::{{synthetic}}` is not `u32` (E0308): never run, got {got:?}") };
    assert!(why.contains("E0308"), "the mismatch: {why}");
    assert!(!why.contains("members are not filed"), "the synthetic type IS filed (rpitit_traits): {why}");
    assert!(why.contains("{the impl Trait returned by `ladder::Tr::make`}"), "named by its trait fn: {why}");
    assert!(!why.contains("the `impl Trait` type #"), "a trait fn's `impl Trait` is its trait's associated type, not a free fn's opaque: {why}");
}

#[test]
fn control_a_written_associated_type_is_named_by_its_path() {
    let got = run("
trait Tw {
    type Out: Copy;
    fn get(&self) -> Self::Out;
}

impl Tw for X {
    type Out = u8;
    fn get(&self) -> u8 {
        1
    }
}

fn bad<S: Tw>(s: &S) -> u32 {
    let v: u32 = s.get();
    v
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    bad(&X) as i32
}
");
    println!("WRITTEN SHOWN: {got:?}");
    let Err(why) = got else { panic!("`<S as Tw>::Out` is not `u32` (E0308): never run, got {got:?}") };
    assert!(why.contains("E0308"), "the mismatch: {why}");
    assert!(why.contains("Out") && !why.contains("members are not filed") && !why.contains("the impl Trait returned by"), "by its written path: {why}");
}
