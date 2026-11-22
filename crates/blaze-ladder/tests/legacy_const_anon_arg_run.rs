//! A `#[rustc_legacy_const_generics]` argument written as an EXPRESSION, BUILT AND RUN on the new
//! front end. tempo t1545: chacha20 0.10.2's `backends::neon::add_xor_rot` was refused "a legacy
//! const generic argument that is no literal, const parameter or constant (an anonymous constant
//! in a body)" (blaze-typeck rules/legacy_const.rs) at its `rotate_left!` arm
//! `($v:expr, $r:literal) => { $v = vorrq_u32(vshlq_n_u32($v, $r), vshrq_n_u32($v, 32 - $r)) }`:
//! `32 - 12` is arithmetic.
//!
//! The rule (rustc, compiler/rustc_ast_lowering/src/expr.rs `lower_legacy_const_generics`):
//! the argument at each listed position becomes the fn's next own const generic argument, an
//! anonymous constant, so `shr(256, 10 - 2)` is `shr::<{ 10 - 2 }>(256)`. A local there is
//! E0435 (an anonymous constant cannot name a local).

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

/// stdarch's shape: `vshrq_n_u32<const N: i32>(a)` called as `vshrq_n_u32(a, n)`.
#[rustc_legacy_const_generics(1)]
fn shr<const N: u32>(a: u32) -> u32 {
    a >> N
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The reduced site: arithmetic at the listed position, `256 >> (10 - 2)` = 1, plus 40.
/// Refused before the fix.
#[test]
fn an_arithmetic_legacy_const_argument_is_its_anonymous_constant() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (shr(256, 10 - 2) + 40) as i32
}
");
    println!("ARITHMETIC LEGACY ARGUMENT: {got:?}");
    assert_eq!(got, Ok(41), "`shr(256, 10 - 2)` is `shr::<{{ 10 - 2 }}>(256)`");
}

/// The site's own form: the expression written by a macro from a `literal` fragment
/// (chacha20's `rotate_left!`): `256 >> (12 - 4)` = 1, plus 50.
#[test]
fn a_macro_written_legacy_const_argument_is_its_anonymous_constant() {
    let got = run("
macro_rules! shr_by {
    ($v:expr, $r:literal) => {
        shr($v, 12 - $r)
    };
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (shr_by!(256, 4) + 50) as i32
}
");
    println!("MACRO-WRITTEN LEGACY ARGUMENT: {got:?}");
    assert_eq!(got, Ok(51), "`12 - $r` at `$r` = 4 is the constant 8");
}

/// The macro-written twin that NAMES an item: `BASE - $r` has a path, so its marks keep
/// placing `BASE` where the macro wrote it (front2 body.rs `body_anon`): 1 + 55.
#[test]
fn a_macro_written_legacy_const_argument_naming_a_constant_runs() {
    let got = run("
const BASE: u32 = 12;

macro_rules! shr_by {
    ($v:expr, $r:literal) => {
        shr($v, BASE - $r)
    };
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (shr_by!(256, 4) + 55) as i32
}
");
    println!("MACRO-WRITTEN NAMING LEGACY ARGUMENT: {got:?}");
    assert_eq!(got, Ok(56), "`BASE - $r` at `$r` = 4 is the constant 8");
}

/// The twin: a block at the listed position, `{ 4 + 4 }`: 1 + 60.
#[test]
fn a_block_legacy_const_argument_is_its_anonymous_constant() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (shr(256, { 4 + 4 }) + 60) as i32
}
");
    println!("BLOCK LEGACY ARGUMENT: {got:?}");
    assert_eq!(got, Ok(61), "`shr(256, {{ 4 + 4 }})` is `shr::<{{ 4 + 4 }}>(256)`");
}

/// The control: a literal (read by the checker itself, passed before the fix): 1 + 70.
#[test]
fn a_literal_legacy_const_argument_runs() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (shr(256, 8) + 70) as i32
}
");
    println!("LITERAL LEGACY ARGUMENT: {got:?}");
    assert_eq!(got, Ok(71), "the literal is the const argument");
}

/// A local at the listed position is no constant (rustc E0435): never run.
#[test]
fn a_local_legacy_const_argument_is_refused() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let k = 8u32;
    (shr(256, k) + 70) as i32
}
");
    println!("LOCAL LEGACY ARGUMENT: {got:?}");
    assert!(got.is_err(), "a local is no anonymous constant (E0435): {got:?}");
}
