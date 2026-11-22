//! A member a macro call writes in an impl's body is named in a refusal by its container and its
//! name (rule 19: an error names the thing), as a member written there by hand is: core's
//! `impl u32 { uint_impl! { .. } }` writes `MAX`, `BITS` and the fns whose heads name
//! `[u8; size_of::<Self>()]`. tempo's tests (2026-11-21, reports/tempo-tests/t0850, `blaze test
//! -p <package> --workers 16`) printed such items as "an item the names source holds no path for
//! (key a606c28d..)" (26 lines) in `Const { .. shown: .. }`: front2 check.rs showed a constant
//! named by its LIBRARY key as `key_shown(checker_of(lib))`, and `checker_of` of a member's (or a
//! not-yet-filed anonymous constant's) library key is no key the key map or the names'
//! `key_beginning` holds. check.rs `lib_shown` now names it through the member owners table
//! (its impl's header as written, then its name), and `key_shown` names a member by
//! item_shown.rs `lib_path` (`ladder::<impl S>::A`, not "`A` of ladder::<impl S> (key ..)").
//!
//! The rule (the Reference, "Constant evaluation", and rustc E0391): a constant that needs
//! itself has no value, so each program below is refused, naming the cycle. The refusal's text
//! is what is tested, not the refusal: both forms are refused today and before.
//!
//! - `a_macro_written_member_is_named_by_its_impl`: `A` and `B` written by a `macro_rules!`
//!   call in `impl S`; the cycle names `<impl S>::A`, never "holds no path for".
//! - CONTROL `a_hand_written_member_is_named_by_its_impl`: the same two constants written in the
//!   impl's body by hand: the same name, so the macro is the only difference.
//!
//! NOT YET RUN by its writer (2026-11-21): written with the fix; compiled and run by the
//! coordinator.

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

struct S;
";

const MAIN: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    S::A as i32
}
";

/// Builds and runs `HEAD` + `impl_` + `MAIN`: the status, or the refusal on one line.
fn run(impl_: &str) -> Result<u8, String> {
    let text = [HEAD, impl_, MAIN].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// What both tests assert of a refusal of the cycle `A` -> `B` -> `A`.
fn names_the_member(why: &str) {
    assert!(why.to_lowercase().contains("cycle"), "the refusal names a cycle (E0391): {why}");
    assert!(!why.contains("holds no path for"), "every item of the cycle has a path: {why}");
    assert!(why.contains("<impl S>::A"), "`A` by its impl's header and its name: {why}");
}

#[test]
fn a_macro_written_member_is_named_by_its_impl() {
    let got = run("
macro_rules! cycle_consts {
    () => {
        const A: usize = Self::B + 1;
        const B: usize = Self::A + 1;
    };
}

impl S {
    cycle_consts! {}
}
");
    println!("MACRO MEMBER SHOWN: {got:?}");
    let why = got.expect_err("a constant that needs itself has no value (E0391)");
    names_the_member(&why);
}

#[test]
fn control_a_hand_written_member_is_named_by_its_impl() {
    let got = run("
impl S {
    const A: usize = Self::B + 1;
    const B: usize = Self::A + 1;
}
");
    println!("HAND MEMBER SHOWN: {got:?}");
    let why = got.expect_err("a constant that needs itself has no value (E0391)");
    names_the_member(&why);
}
