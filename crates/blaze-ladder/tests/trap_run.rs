//! A trap in the program's placed code ends the run BY NAME (src/trap.rs): `boom` reaches
//! `unreachable` and the run ends as `RunError::Trapped`, whose one line names the trap, `boom`
//! and its caller `main`, in place of the whole test process dying with SIGTRAP (status 133)
//! and nothing said (port27/hello-1116/stops-3-briefs.md, "Class M").
//!
//! The program is `#![no_core]`, so it needs no toolchain: `boom` calls the intrinsic that
//! `core::hint::unreachable_unchecked` ends in. The pinned library (library/core/src/hint.rs):
//!
//!   pub const unsafe fn unreachable_unchecked() -> ! {
//!       ub_checks::assert_unsafe_precondition!(check_language_ub, ..);
//!       unsafe { intrinsics::unreachable() }
//!   }
//!
//! and library/core/src/intrinsics/mod.rs: "it is *undefined behavior* to reach code marked
//! with this function", declared `#[rustc_nounwind] #[rustc_intrinsic] pub const unsafe fn
//! unreachable() -> !;`. The intrinsic is called directly so that the trap is IN `boom` (a
//! call of the library's wrapper would trap in the wrapper, with `boom` its caller, and with
//! UB checks on the wrapper panics before it reaches the intrinsic). blaze-emit makes LIR's
//! `unreachable` a `brk #0xff02` (blaze-emit pass.rs `BRK_UNREACHABLE`).
//!
//! What is right: the program did something undefined at a known instruction; the run says
//! which function and from where, and ends as the PROGRAM's failure (trap.rs
//! `PROGRAM_TRAPPED`, 101; D86), never as blaze's own (125) and never silently.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError, run_program};
use blaze_ladder::trap::{PROGRAM_TRAPPED, TrapCode};

const PROGRAM: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

#[rustc_nounwind]
#[rustc_intrinsic]
pub const unsafe fn unreachable() -> !;

fn boom() -> i32 {
    unsafe { unreachable() }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    boom()
}
";

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

/// `main` calls `boom`, which reaches `unreachable`: the run ends as `RunError::Trapped` with
/// the one line naming the `brk`, `boom` at an offset, and `main` at its call, and nothing
/// after `main` (its caller is blaze's own `enter`, outside the placed code).
///
/// Prints: `TRAP: the program trapped (`brk #0xff02`, unreachable) in `boom` +0x.., called
/// from `main` +0x..` (each path as the build names it, ending in `boom` and `main`) and
/// `test a_trap_in_placed_code_names_its_function_and_caller ... ok`
#[test]
fn a_trap_in_placed_code_names_its_function_and_caller() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let ended = match run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()) {
        Err(e) => e,
        Ok(ran) => panic!("the program returned status {} past an `unreachable`", ran.status),
    };
    let RunError::Trapped { trap } = &ended else { panic!("not `Trapped`: {ended}") };
    let line = trap.line();
    println!("TRAP: {line}");
    assert_eq!(trap.code(), TrapCode::Unreachable);
    assert!(trap.function().ends_with("boom"), "the trapping function is `boom`: {line}");
    let head = "the program trapped (`brk #0xff02`, unreachable) in `";
    assert!(line.starts_with(head), "the line's form: {line}");
    // `<path>` +0x<offset>, called from `<path>` +0x<offset>: exactly one caller, `main`.
    let (trapped, callers) = line[head.len()..].split_once(", called from `").unwrap_or_else(|| panic!("no caller named: {line}"));
    let (path, offset) = trapped.split_once("` +0x").unwrap_or_else(|| panic!("no offset: {line}"));
    assert!(path.ends_with("boom") && u64::from_str_radix(offset, 16).is_ok(), "`boom` +0x..: {line}");
    let (caller, at) = callers.split_once("` +0x").unwrap_or_else(|| panic!("no caller offset: {line}"));
    assert!(caller.ends_with("main"), "the caller is `main`: {line}");
    assert!(u64::from_str_radix(at, 16).is_ok(), "one caller, then the end of the line: {line}");
    assert_eq!(PROGRAM_TRAPPED, 101);
}
