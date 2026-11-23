//! A relocation-free constant of pointer or float type builds (walk-AJ P4): blaze-build walk.rs
//! `const_record_value` builds such a constant by its scalar kind (walk/const_scalar.rs), not
//! as an integer whatever its type. The fault it guards: every such constant was `iconst` of
//! its type, which blaze-lir's builder refuses for a pointer or a float ("a type that is not a
//! scalar integer"); sample: libc's `pub const MAP_FAILED: *mut c_void = !0 as *mut c_void;`
//! read by blaze-mem os.rs `reserve_with`. It needs no toolchain: the program is `#![no_core]`.
//!
//! The rules: the Reference, "Type cast expressions", "Address to pointer cast": "Casting from
//! an integer to a raw pointer interprets the integer as a memory address and produces a
//! pointer referencing that memory"; "Numeric types", "Floating-point types": `f64` is IEEE
//! 754-2008 "binary64", so `0.5` is its bits. Both constants are values the program may use.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};

/// `main` reaches `p` (which reads a pointer constant holding an address with no allocation)
/// and `h` (which reads a float constant).
const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

const FAILED: *mut u8 = 18446744073709551615usize as *mut u8;
const HALF: f64 = 0.5;

fn p() -> *mut u8 {
    FAILED
}

fn h() -> f64 {
    HALF
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let _p = p();
    let _h = h();
    0
}
";

/// The survey of [`PROGRAM`] builds all three fns: `main`, `p` and `h`; nothing is refused.
/// Without the fix `p` and `h` are refused, each naming "`iconst` of a type that is not a
/// scalar integer".
///
/// Prints: `BUILDS: 3 of 3 instances (0 refused: )` and
/// `test pointer_and_float_constants_build ... ok`
#[test]
fn pointer_and_float_constants_build() {
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
    assert!(rows.is_empty(), "a pointer and a float constant build: {rows:?}");
    assert_eq!((counts.built, counts.tried), (3, 3), "`main`, `p` and `h` built: {rows:?}");
}
