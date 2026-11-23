//! A closure passed to ANOTHER body's generic fn (hello-d): `main` calls `run(|x: u8| x)` with
//! `fn run<F: Fn(u8) -> u8>(f: F) -> u8 { f(7) }`. Building `run::<{closure in main}>` resolves
//! `f(7)` as `Fn::call` on the closure type, and the checker asks `run`'s world for the
//! closure's facts (blaze-typeck instance.rs `Asked::closure_of`): they are `main`'s, filed
//! where `main`'s check finished clean (front2 foreign_closures.rs). Before the fix the world
//! answered a gap ("the body record Closure(..) .. is not made yet") and `run`'s instance was
//! refused.
//!
//! The rules the programs rest on (the Rust Reference):
//! - "Closure types": "A closure expression produces a closure value with a unique, anonymous
//!   type that cannot be written out"; "All closure types implement FnOnce"; a closure that
//!   does not move out of or mutate its captures (this one captures nothing) "implements Fn".
//!   So `{closure}: Fn(u8) -> u8` holds and `run::<{closure}>` is an instance to build.
//! - "Call expressions": for a value of a type implementing `Fn`, `f(7)` is `Fn::call(&f, (7,))`.
//! - "Functions": a body whose block does not coerce to the declared type is an error: `let
//!   wrong: u8 = true;` ("Let statements": the initializer must have the declared type), so
//!   `bad`'s check fails and its closures have no record.
//!
//! The lang traits mirror the pinned library's (library/core/src/ops/function.rs: `pub trait
//! FnOnce<Args: Tuple> { #[lang = "fn_once_output"] type Output; extern "rust-call" fn
//! call_once(self, args: Args) -> Self::Output; }`, `FnMut<Args: Tuple>: FnOnce<Args>`,
//! `Fn<Args: Tuple>: FnMut<Args>`; library/core/src/marker.rs `#[lang = "tuple_trait"] pub trait
//! Tuple {}`). It needs no toolchain: the programs are `#![no_core]` (as front2_survey.rs).
#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::foreign_closures::{ClosureRows, Missing, gap_text};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};
use blaze_typeck::ItemKey;

const PRELUDE: &str = r#"#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

#[lang = "tuple_trait"]
trait Tuple {}

#[lang = "fn_once"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = "fn_once_output"]
    type Output;
    extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = "fn_mut"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = "fn"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern "rust-call" fn call(&self, args: Args) -> Self::Output;
}

fn run<F: Fn(u8) -> u8>(f: F) -> u8 {
    f(7)
}
"#;

const CLEAN: &str = r#"
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _y = run(|x: u8| x);
    0
}
"#;

const OWNER_WITH_ERROR: &str = r#"
fn bad() -> u8 {
    let wrong: u8 = true;
    run(|x: u8| x)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _y = bad();
    0
}
"#;

/// Whether the survey's fn path `f` is the crate-root fn `name` (run.rs `Named`'s `path`).
fn is_fn(f: &str, name: &str) -> bool {
    f == name || f.ends_with(&*format!("::{name}"))
}

/// The survey of `PRELUDE + tail`: the left-out rows `(fn, cause)`, the refused build rows
/// `(fn, class)`, and the instances built and tried.
fn survey(tail: &str) -> (Vec<(String, String)>, Vec<(String, String)>, u64, u64) {
    let text = format!("{PRELUDE}{tail}");
    let files = [SourceFile { path: "src/main.rs", text: &text }];
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
    let left: Vec<(String, String)> = left_out.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    println!("LEFT OUT: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<(String, String)> = refused.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    let shown: Vec<String> = rows.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), shown.join("; "));
    (left, rows, counts.built as u64, counts.tried as u64)
}

/// `main` and `run` check clean, and `run::<{closure in main}>` builds: no row refuses `run`
/// or `main`, and none is the world's closure gap (the text before the fix, "BodyTypeInRecord"
/// or "Closure(", or the refusal's own "closure #").
///
/// Prints: `BUILDS: N of N instances (0 refused: )` and
/// `test a_closure_passed_to_another_bodys_generic_fn_builds ... ok`
#[test]
fn a_closure_passed_to_another_bodys_generic_fn_builds() {
    let (left, rows, built, tried) = survey(CLEAN);
    assert!(left.is_empty(), "every fn checks clean: {left:?}");
    for (f, class) in rows.iter() {
        assert!(!is_fn(f, "run") && !is_fn(f, "main"), "`run::<{{closure in main}}>` and `main` build: {rows:?}");
        assert!(
            !class.contains("BodyTypeInRecord") && !class.contains("Closure(") && !class.contains("closure #"),
            "the closure's facts are answered from `main`'s checked record: {rows:?}"
        );
    }
    assert!(built >= 2 && built == tried, "`main` and `run`'s instance built ({built} of {tried}): {rows:?}");
}

/// `bad` owns a closure but its check fails (E0308): its record cannot be had, and what is
/// refused is named by the OWNER: `bad` is left out with its check's cause, and `main` (its
/// caller) is refused naming that cause; nothing is refused with a nameless closure gap.
///
/// Prints: `LEFT OUT: [(".. bad", "check: Diags ..")]` and
/// `test a_closure_whose_owner_has_an_error_is_refused_by_its_owner ... ok`
#[test]
fn a_closure_whose_owner_has_an_error_is_refused_by_its_owner() {
    let (left, rows, _, _) = survey(OWNER_WITH_ERROR);
    let of_bad: Vec<&(String, String)> = left.iter().filter(|(f, _)| is_fn(f, "bad")).collect();
    assert_eq!(of_bad.len(), 1, "`bad`, the closure's owner, is left out with its check's cause: {left:?}");
    assert!(
        rows.iter().all(|(_, class)| !class.contains("BodyTypeInRecord") && !class.contains("Closure(")),
        "no row is a closure gap without its owner: {rows:?}"
    );
    // The world's own refusal, for an owner with no clean check: the owner by path, the ordinal.
    let rows_table = ClosureRows::new_in(Global);
    let why = rows_table.get(ItemKey(0x5eed), 2).expect_err("no owner was filed");
    assert_eq!(why, Missing::NotChecked);
    let text = format!("{}", gap_text(ItemKey(0x5eed), "`bad`", 2, why));
    println!("GAP: {text}");
    assert!(text.contains("closure #2 of `bad`") && text.contains("no clean check"), "the refusal names the owner and the ordinal: {text}");
}
