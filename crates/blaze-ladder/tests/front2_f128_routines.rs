//! An `f128` operation builds through the census's path (`Front2Program::survey_from`, then
//! `run::build_survey`, the eager build): its soft-float routine is bound as the record walk
//! binds it (run/make_one.rs, front2/builtins_shim.rs `Bound::bind`).
//!
//! The fault this guards (lane TE, 2026-11-17; census g051728: 43 tier refusals "emit:
//! Legalize: the routine `__fixtfti` is not bound", `__addtf3`, `__eqtf2`, ..): the eager build
//! emitted with no routine bound (run.rs `build_from` passed `RoutinePlaces::NONE`), so every
//! `f128` operation was refused, while the record walk bound the program's compiler_builtins
//! exports. One rule now: the eager program reaches each found routine beside its roots
//! (program.rs `make_from`) and `build_from` binds them in every instance that legalizes.
//!
//! The rule (INTRINSICS.md 3.5): an `f128` operation is a call of the program's own
//! compiler_builtins routine for it, the fn of that crate exported (`#[no_mangle]`) under the
//! routine's symbol (`__addtf3` for an add). The fixture's one crate IS `compiler_builtins`
//! (its package name), so it exports the routine itself; its body is a stand-in (the test
//! builds, it does not run).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const PROGRAM: &str = "#![feature(no_core, lang_items, f128)]
#![no_core]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for f128 {}

/// The routine an `f128` add is legalized to (a stand-in body).
#[unsafe(no_mangle)]
pub extern \"C\" fn __addtf3(a: f128, _b: f128) -> f128 {
    a
}

pub fn add(a: f128, b: f128) -> f128 {
    a + b
}
";

/// Surveys [`PROGRAM`] as the crate `package` from its fn `add`: the refused rows and the
/// counts (built, tried).
fn survey(package: &'static str) -> (Vec<String>, usize, usize) {
    let files = [SourceFile { path: "src/lib.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package, edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/lib.rs", new, &tc, blaze_build::Panic::Abort).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let segments: &[&str] = &["add"];
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments }, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("`add` by path: {e:?}"));
    let add = found.whole(1).unwrap_or_else(|| panic!("`add` resolves whole")).key;
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey_from(&names, &frame, &files, &[add], &mut left_out, None, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::Off, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("F128 SURVEY ({package}): built {} of {} ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    (rows, counts.built, counts.tried)
}

/// As `compiler_builtins`: `add` legalizes to a call of `__addtf3`, bound at the routine's
/// instance (reached beside the root): both build, none refused.
/// Prints `F128 SURVEY (compiler_builtins): built 2 of 2 (0 refused: )`.
#[test]
fn an_f128_add_builds_through_the_survey_with_its_routine_bound() {
    let (rows, built, tried) = survey("compiler_builtins");
    assert!(rows.is_empty(), "no instance refused: {rows:?}");
    assert_eq!((built, tried), (2, 2), "`add` and `__addtf3` built: {rows:?}");
}

/// The control: the same program as a crate of another name has no compiler_builtins in its
/// graph, so `add`'s add has no routine, refused naming `__addtf3` and why (no crate); the
/// routine-shaped fn is no routine then and is not reached.
/// Prints `F128 SURVEY (ladder): built 0 of 1 (1 refused: add: ..__addtf3..)`.
#[test]
fn an_f128_add_with_no_compiler_builtins_is_refused_naming_its_routine() {
    let (rows, built, tried) = survey("ladder");
    assert_eq!(rows.len(), 1, "one refusal: {rows:?}");
    assert!(rows[0].contains("add") && rows[0].contains("__addtf3") && rows[0].contains("compiler_builtins"), "names the routine and the crate: {rows:?}");
    assert_eq!((built, tried), (0, 1), "`add` tried and refused: {rows:?}");
}
