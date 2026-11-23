//! The builds survey (`Front2Program::survey`, then `run::build_survey`: what the walk of the
//! owner's hello world prints as `BUILDS:`) COUNTS AND GOES ON: a form the back half cannot
//! build in one reachable fn is that fn's instances' verdict, filed with its cause, and every
//! other fn is still tried. It needs no toolchain: the program is `#![no_core]`.
//!
//! The fault this guards (2026-11-16, port27/front2/list-AG.txt line 24): "BUILDS: no survey:
//! front2: `..::default` at 94..100: this float literal (front2's)": core's `0.0f16` (library/
//! core default.rs, `default_impl! { f16, 0.0f16, .. }`) was refused while one fn's row was
//! made and the refusal returned from the whole survey (front2/program/tabled.rs says how
//! that class is closed; its own test covers a refusal raised while a row is made).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};

/// `main` reaches `fine` (which the builder builds) and `half` (which holds an `f16` literal:
/// the builder has no form for `f16`, blaze-build walk/numbers.rs `float_repr`: "`f16` or
/// `f128`"). The Reference, "Floating-point literal expressions": a suffixed literal has its
/// suffix's type, so `0.5f16` is an `f16` whatever is around it.
const PROGRAM: &str = "#![feature(no_core, lang_items, f16)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

fn fine() -> i32 {
    7
}

fn half() {
    let _h = 0.5f16;
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    half();
    fine()
}
";

/// The survey of [`PROGRAM`] ends with counts: `main` and `fine` built, `half` refused with
/// the builder's cause naming `f16`; nothing is left out by front2 and the survey returns no
/// error of its own.
///
/// Prints: `BUILDS: 2 of 3 instances (1 refused: half: ..`f16` or `f128`..)` and
/// `test one_refused_fn_is_counted_and_the_survey_goes_on ... ok`
#[test]
fn one_refused_fn_is_counted_and_the_survey_goes_on() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    assert_eq!(rows.len(), 1, "one refused instance: {rows:?}");
    assert!(refused[0].0.contains("half"), "the refused fn is `half`: {rows:?}");
    assert!(refused[0].1.contains("NoLiteralValue"), "the cause names the missing literal value: {rows:?}");
    assert_eq!((counts.built, counts.tried), (2, 3), "`main` and `fine` built, `half` tried and refused: {rows:?}");
}
