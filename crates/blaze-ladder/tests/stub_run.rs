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

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

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
    let status = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("NO_MANGLE ENTRY: refused: {e}"));
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
    let ran = run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("the stub run stopped: {e}"));
    println!("STUB RUN: status {}; {} of {} reachable functions are not built; none was called", ran.status, ran.not_built, ran.reached);
    assert_eq!((ran.status, ran.not_built, ran.reached), (3, 1, 2));
    let strict = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared());
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
    let stopped = match run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()) {
        Err(e) => e,
        Ok(ran) => panic!("the program ran to status {} through a function that is not built", ran.status),
    };
    println!("STUB RUN: stopped: {stopped}");
    let RunError::NotBuilt { refusal, not_built, reached } = &stopped else { panic!("not `NotBuilt`: {stopped}") };
    assert_eq!((*not_built, *reached), (1, 2));
    assert!(format!("{refusal}").contains("half"), "the refusal names `half`: {refusal}");
}

/// A program that raises an unwind by the Itanium ABI itself (`raise`, called when `go` is
/// 1): `main(go)` is its exported entry's body.
const RAISES: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

/// `_Unwind_Exception`'s four words (the Itanium ABI on arm64), zeroed: a foreign class.
#[repr(C)]
struct Exc {
    class: u64,
    cleanup: u64,
    private_1: u64,
    private_2: u64,
}

unsafe extern \"C\" {
    fn _Unwind_RaiseException(exc: *mut Exc) -> i32;
}

fn raise() -> i32 {
    let mut exc = Exc { class: 0, cleanup: 0, private_1: 0, private_2: 0 };
    unsafe { _Unwind_RaiseException(&raw mut exc) }
}

fn main(go: i32) -> i32 {
    match go {
        1 => raise(),
        _ => 3,
    }
}
";

/// D91: no panic strategy is swapped silently. Until each placed body's unwind entry is
/// registered (run.md piece 11, part C), a call of `_Unwind_RaiseException` (the Itanium C++
/// ABI's \"Base ABI\" 1.6.2: it starts phase 1 of the two-phase unwind, and returns only when no
/// handler is found) is a stub that stops the run with `RunError::UnwindRaised`, naming the
/// function that raised; one reached and never called costs nothing (it is not counted as a
/// function left unbuilt). Without it the placed code calls the system's unwinder, which reads
/// no frame of placed code and returns `_URC_END_OF_STACK` (5): the program goes on to a status,
/// and this test fails with "the raise ran to status ..".
///
/// Prints: `RAISE: stopped: the program panicked and `raise` raised the panic to unwind ..`,
/// `RAISE (strict): stopped: ..` and `test a_raised_unwind_stops_the_run_by_name ... ok`
#[test]
fn a_raised_unwind_stops_the_run_by_name() {
    // The entry is the exported `main` of a module (see `a_no_mangle_fn_in_a_module_is_the_entry`).
    let program = |go: &str| [RAISES, "\nmod entry {\n    #[unsafe(no_mangle)]\n    pub extern \"C\" fn main() -> i32 {\n        super::main(", go, ")\n    }\n}\n"].concat();
    let quiet = program("0");
    let files = [SourceFile { path: "src/main.rs", text: &quiet }];
    let ran = run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("the raise was never called, yet the run stopped: {e}"));
    assert_eq!((ran.status, ran.not_built), (3, 0), "a raise not called changes nothing and is not counted");
    let raising = program("1");
    let files = [SourceFile { path: "src/main.rs", text: &raising }];
    let stopped = match run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()) {
        Err(e) => e,
        Ok(ran) => panic!("the raise ran to status {}", ran.status),
    };
    println!("RAISE: stopped: {stopped}");
    let RunError::UnwindRaised { function } = &stopped else { panic!("not `UnwindRaised`: {stopped}") };
    assert!(format!("{function}").contains("raise"), "the stop names `raise`: {function}");
    let strict = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => e,
        Ok(status) => panic!("the strict run's raise ran to status {status}"),
    };
    println!("RAISE (strict): stopped: {strict}");
    assert!(matches!(strict, RunError::UnwindRaised { .. }), "the strict run stops the same way: {strict}");
}

/// The rung x0-static-03-fn-pointers (its header cites the Reference), with one change: `main`
/// also calls `forty` and `two` directly once, so the walk reaches them. (In the rung itself
/// no body calls them and front2's census never meets them: a static initializer's fn items
/// are not walked yet, so the rung stops at a `RunError::StaticFn` stub until they are.)
/// The run places the statics after the slots are declared, each fn word the thunk of the
/// slot of the instance the static's record names by key (place.rs `write_statics`, run.rs
/// `static_fn_slots`); the evaluator and the run key a fn's code by ONE identity
/// (front2/consts.rs `item_code_id`). Without the late write the run is refused before it
/// starts ("a static holds a fn pointer, which the ladder does not place yet"); with two code
/// identities the keys differ and the call through `TABLE` stops at a stub ("which no code of
/// the program reaches").
///
/// Prints: `STATIC FN POINTERS: status 42 (strict), status 42, 0 not built` and
/// `test a_static_of_fn_pointers_calls_through_its_words ... ok`
#[test]
fn a_static_of_fn_pointers_calls_through_its_words() {
    let rung = include_str!("../../blaze-cli/tests/ladder/front2/x0-static-03-fn-pointers/src/main.rs");
    let mut text = rung.replace("    let second = (PICK[1])();", "    let direct = forty() + two();\n    let second = (PICK[1])();\n    if direct != 42 {\n        return 3;\n    }");
    assert_ne!(text, rung, "the rung's `main` changed: this test's edit of it no longer applies");
    // The added `+` is checked where overflow checks are on (the strict run): a `no_core`
    // program declares what the check calls (the pinned core's `panic_const_add_overflow`).
    // Judge-changed 2026-11-16 18:2x (run-path integrator B): the check is now built, so the
    // edit that adds the `+` adds its lang fn too.
    text.push_str("\n#[lang = \"panic_const_add_overflow\"]\nfn panic_const_add_overflow() -> ! {\n    loop {}\n}\n");
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let strict = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("STATIC FN POINTERS (strict): {e}"));
    let ran = run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("STATIC FN POINTERS: {e}"));
    println!("STATIC FN POINTERS: status {strict} (strict), status {}, {} not built", ran.status, ran.not_built);
    assert_eq!((strict, ran.status, ran.not_built), (42, 42, 0));
}

/// `const F: fn() -> i32 = seven;` and `main` returns `F()`: no body names `seven` but the
/// constant's. The evaluator's record of `F` is one word relocated to `seven`'s instance key;
/// the builder asks the run's `Adts::const_fn_instance` which instance that is (run.rs
/// `FrontAdts`: the record names the instance it made the word from, blaze-consteval
/// `ConstRecord::fns`), files it in `main`'s `Built::refs`, and the loop reaches and builds it
/// from there. Without the answer `main` is refused ("... holds a fn pointer to instance ..,
/// which no record the build's `Adts` holds names") and the strict run fails; if the reach did not follow `Built::refs`,
/// `seven`'s slot would be a stub and the run would stop at the call.
///
/// Prints: `CONST FN POINTER: status 7 (strict), status 7, 0 not built` and
/// `test a_constants_fn_pointer_is_built_and_called ... ok`
#[test]
fn a_constants_fn_pointer_is_built_and_called() {
    let text = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

fn seven() -> i32 {
    7
}

const F: fn() -> i32 = seven;

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    F()
}
";
    let files = [SourceFile { path: "src/main.rs", text }];
    let strict = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("CONST FN POINTER (strict): {e}"));
    let ran = run_program(&files, "src/main.rs", None, NEW, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("CONST FN POINTER: {e}"));
    println!("CONST FN POINTER: status {strict} (strict), status {}, {} not built", ran.status, ran.not_built);
    assert_eq!((strict, ran.status, ran.not_built), (7, 7, 0));
}
