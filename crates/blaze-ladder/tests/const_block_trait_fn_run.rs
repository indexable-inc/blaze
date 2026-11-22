//! A TRAIT'S FN named in an inline const block, evaluated at a generic fn's instance, BUILT AND
//! RUN on the new front end. tempo t1545: alloc's `in_place_collect`
//! `<impl SpecFromIter<T, I> for Vec<T>>::from_iter`
//! (`let fun: fn(I) -> Vec<T> = const { if in_place_collectible::<T, I::Src>(..) {
//! from_iter_in_place } else { SpecFromIterNested::<T, I>::from_iter } }`) at
//! `[Bytes, Map<IntoIter<Vec<u8>>, ..>]` was refused "the world has no impl for an associated
//! item": the const evaluator's world (front2/consts.rs `ConstsWorld::resolve`) answered `None`
//! for every trait fn, so the fn pointer of the `else` branch had no definition.
//!
//! The rule (the Reference, "Associated Items": an associated item named through a trait at a
//! type is the definition in that type's impl of the trait, or the trait's default where the
//! impl writes none; "Constant evaluation": an inline const block is a const context, evaluated
//! once per instance of its enclosing generic fn): the block's value is a fn pointer to the
//! SELECTED definition at the instance's arguments.

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

/// Whether the direct path applies to `Self` (the site's `in_place_collectible`).
trait Small {
    const SMALL: bool;
}

/// The fallback (the site's `SpecFromIterNested`).
trait Pick {
    fn pick(x: u8) -> u8;
    fn picked(x: u8) -> u8 {
        x + 3
    }
}

struct A;
struct B;

impl Small for A {
    const SMALL: bool = false;
}
impl Small for B {
    const SMALL: bool = true;
}
impl Pick for A {
    fn pick(x: u8) -> u8 {
        x + 1
    }
}
impl Pick for B {
    fn pick(x: u8) -> u8 {
        x + 2
    }
}

fn direct(x: u8) -> u8 {
    x + 30
}

fn other(x: u8) -> u8 {
    x + 20
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The site's shape: at `A` the block takes the trait's fn, selected to `A`'s impl (10 + 1);
/// at `B` the free fn (10 + 30): 51. Refused before the fix at the `A` instance.
#[test]
fn a_trait_fn_in_a_const_block_is_the_selected_impls() {
    let got = run("
fn choose<T: Small + Pick>(x: u8) -> u8 {
    let f: fn(u8) -> u8 = const { if T::SMALL { direct } else { <T as Pick>::pick } };
    f(x)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (choose::<A>(10) + choose::<B>(10)) as i32
}
");
    println!("TRAIT FN IN A CONST BLOCK: {got:?}");
    assert_eq!(got, Ok(51), "A's `pick` at A, the free fn at B");
}

/// The twin: the trait fn no impl writes (`picked`, the trait's default), named through
/// `T` the same way: the selection is the trait's own definition at each instance (10 + 3 at
/// `A`, 10 + 30 at `B`): 53.
#[test]
fn a_defaulted_trait_fn_in_a_const_block_is_the_traits_definition() {
    let got = run("
fn choose<T: Small + Pick>(x: u8) -> u8 {
    let f: fn(u8) -> u8 = const { if T::SMALL { direct } else { T::picked } };
    f(x)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (choose::<A>(10) + choose::<B>(10)) as i32
}
");
    println!("DEFAULTED TRAIT FN IN A CONST BLOCK: {got:?}");
    assert_eq!(got, Ok(53), "the trait's default at A, the free fn at B");
}

/// The control: the same block choosing between two free fns (no selection; passed before the
/// fix): 10 + 20 at `A`, 10 + 30 at `B`: 70.
#[test]
fn free_fns_in_a_const_block_choose_by_the_instance() {
    let got = run("
fn choose<T: Small>(x: u8) -> u8 {
    let f: fn(u8) -> u8 = const { if T::SMALL { direct } else { other } };
    f(x)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (choose::<A>(10) + choose::<B>(10)) as i32
}
");
    println!("FREE FNS IN A CONST BLOCK: {got:?}");
    assert_eq!(got, Ok(70), "the free fn each instance's constant selects");
}
