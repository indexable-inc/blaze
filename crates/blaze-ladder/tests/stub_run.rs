//! A run that starts although a reached function cannot be built (`run::run_program`,
//! `run::build_stubbed`): the function's slot holds a stub (src/stub.rs), a program that never
//! calls it runs to its own status, and one that calls it is stopped at that call with the
//! function's refusal by name. docs/v2/plan/design/bottom.md, section 11: "a function not yet
//! compiled is a stub". It needs no toolchain: the programs are `#![no_core]`.
//!
//! The function the builder refuses here holds an `f16` local: blaze-build's `float_ty`
//! (walk/numbers.rs) refuses `f16` and `f128` by name ("`f16` or `f128`"). The Reference,
//! "Floating-point types", lists `f32` and `f64` only; `f16` is the unstable feature `f16`.
//! When blaze builds `f16`, pick another form the builder refuses by name.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, RunError, run_program, run_with};

/// The lang items a `#![no_core]` program declares, and `half`, which the builder refuses.
const HEAD: &str = "#![feature(no_core, lang_items, f16)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

fn half() -> i32 {
    let h: f16 = 1.0;
    5
}
";

/// `main` reaches `half` and never calls it: `code` is 7.
const NEVER_CALLED: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let code = 7;
    match code {
        7 => 3,
        _ => half(),
    }
}
";

/// `main` calls `half`.
const CALLED: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let code = 7;
    match code {
        7 => half(),
        _ => 3,
    }
}
";

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None };

/// A crate whose root `fn main` takes a parameter and whose C entry is a `#[no_mangle]` fn in a
/// module: the shape of every hosted program (the appended entry module's `bstd_hosted_start`
/// beside the user's `main`).
const ENTRY_IN_A_MODULE: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

fn main(code: i32) -> i32 {
    code
}

mod entry {
    #[unsafe(no_mangle)]
    pub extern \"C\" fn main() -> i32 {
        super::main(42)
    }
}
";

/// THE ENTRY RULE reads a `#[no_mangle]` fn's export as its own NAME (the Reference, "The
/// `no_mangle` attribute": the symbol \"will be the same as the item's name\"), so the exported
/// `entry::main` is the entry and the root's `fn main(code)` is an ordinary function it calls.
/// When front2/program.rs `exported_entry` hands `symbols_of` anything but the bare name, the
/// export is never `main`, the root's `fn main` becomes the entry and the run is refused with
/// "`main`: it takes parameters": the line this test fails with without the fix.
///
/// Prints: `NO_MANGLE ENTRY: status 42` and
/// `test a_no_mangle_fn_in_a_module_is_the_entry ... ok`
#[test]
fn a_no_mangle_fn_in_a_module_is_the_entry() {
    let files = [SourceFile { path: "src/main.rs", text: ENTRY_IN_A_MODULE }];
    let status = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW)).unwrap_or_else(|e| panic!("NO_MANGLE ENTRY: refused: {e}"));
    println!("NO_MANGLE ENTRY: status {status}");
    assert_eq!(status, 42);
}

/// A reached function that is not built and never called costs a count, not the run: the
/// program ends with its own status (3), and the answer says 1 of its 2 reached functions is
/// not built. The strict path (`run_with`, every reached function built or the build stops)
/// refuses the same program before it starts: the line this test fails with without the stub
/// build.
///
/// Prints: `STUB RUN: status 3; 1 of 2 reachable functions are not built; none was called`,
/// `STRICT: refused before the run: ..half..` and
/// `test a_function_not_built_and_never_called_costs_a_count ... ok`
#[test]
fn a_function_not_built_and_never_called_costs_a_count() {
    let text = [HEAD, NEVER_CALLED].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let ran = run_program(&files, "src/main.rs", None, NEW).unwrap_or_else(|e| panic!("the stub run stopped: {e}"));
    println!("STUB RUN: status {}; {} of {} reachable functions are not built; none was called", ran.status, ran.not_built, ran.reached);
    assert_eq!((ran.status, ran.not_built, ran.reached), (3, 1, 2));
    let strict = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW));
    let refused = match strict {
        Err(e) => format!("{e}"),
        Ok(status) => panic!("the strict run built `half` (status {status}): this test needs a form the builder refuses"),
    };
    println!("STRICT: refused before the run: {refused}");
    assert!(refused.contains("half"), "the strict refusal names `half`: {refused}");
}

/// The same function CALLED stops the program at that call: the answer is
/// `RunError::NotBuilt` with `half`'s refusal and the two counts, never a status.
///
/// Prints: `STUB RUN: stopped: the program called a function this build of blaze cannot build
/// yet and was stopped there: ..half.. (1 of 2 reachable functions are not built yet)` and
/// `test a_called_function_that_is_not_built_stops_the_program_by_name ... ok`
#[test]
fn a_called_function_that_is_not_built_stops_the_program_by_name() {
    let text = [HEAD, CALLED].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let stopped = match run_program(&files, "src/main.rs", None, NEW) {
        Err(e) => e,
        Ok(ran) => panic!("the program ran to status {} through a function that is not built", ran.status),
    };
    println!("STUB RUN: stopped: {stopped}");
    let RunError::NotBuilt { refusal, not_built, reached } = &stopped else { panic!("not `NotBuilt`: {stopped}") };
    assert_eq!((*not_built, *reached), (1, 2));
    assert!(format!("{refusal}").contains("half"), "the refusal names `half`: {refusal}");
}
