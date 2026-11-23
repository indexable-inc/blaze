//! The panic entry on the run path (front2/panic_entry.rs): a call of the declaration the
//! pinned core writes inside `panic_fmt`'s body is a call of the program's `#[panic_handler]`
//! fn. It needs no toolchain: the programs are `#![no_core]` and mirror core's text.
//!
//! The pinned core (library/core/src/panicking.rs, `panic_fmt`):
//! ```text
//! // NOTE This function never crosses the FFI boundary; it's a Rust-to-Rust call
//! // that gets resolved to the `#[panic_handler]` function.
//! unsafe extern "Rust" {
//!     #[lang = "panic_impl"]
//!     fn panic_impl(pi: &PanicInfo<'_>) -> !;
//! }
//! let pi = PanicInfo::new(&fmt, Location::caller(), /* can_unwind */ true, /* force_no_backtrace */ false);
//! // SAFETY: `panic_impl` is defined in safe Rust code and thus is safe to call.
//! unsafe { panic_impl(&pi) }
//! ```
//! The pinned std (library/std/src/panicking.rs) is the handler of a std program: "Entry point
//! of panics from the core crate (`panic_impl` lang item)": `#[panic_handler] pub(crate) fn
//! panic_handler(info: &core::panic::PanicInfo<'_>) -> !`. In a program without std it is the
//! user's, as here.
//!
//! The Reference, "Panic", "The `panic_handler` attribute" (the pinned nightly's copy,
//! share/doc/rust/html/reference/panic.html, read 2026-11-16):
//! [panic.panic_handler.intro] "The panic_handler attribute can be applied to a function to
//! define the behavior of panics." [panic.panic_handler.allowed-positions] "The panic_handler
//! attribute can only be applied to a function with signature fn(&PanicInfo) -> !."
//! [panic.panic_handler.unique] "There must be a single panic_handler function in the
//! dependency graph."
//!
//! The lines these tests print when they pass:
//! ```text
//! PANIC ENTRY: `on_panic` tabled with its body; no foreign symbol `panic_impl`; BUILDS: 3 of 3 instances (0 refused)
//! test a_call_of_panic_impl_reaches_and_binds_the_handler ... ok
//! NO HANDLER: front2: `..`, reached from `main`, is not checked (a call of the `#[lang = "panic_impl"]` declaration with no `#[panic_handler]` fn to bind it to (..)): ..
//! test a_panic_with_no_handler_is_refused_by_the_rule ... ok
//! TWO HANDLERS: front2 could not start: the lang item `panic_handler` is declared twice: .. and ..
//! test two_handlers_are_refused_by_name ... ok
//! ```
//! What a wrong fix fails: a declaration left with no row ("no body (front2's)") fails the
//! first test's `left out` assertion; one tabled as a foreign fn (looked up as the C symbol
//! `panic_impl`, which no library holds) fails its `no foreign symbol` assertion; one bound
//! but whose handler the walk did not queue fails `on_panic tabled`; a walk that queued the
//! handler but a call left unbound fails `0 refused` (`panic_fmt`'s callee has no instance).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front::FnKind;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};
use blaze_ladder::seam::FrontEnd;

/// The program's start: the lang items a `#![no_core]` program declares, and the type core's
/// declaration names.
const HEAD: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

#[lang = \"panic_info\"]
struct PanicInfo<'a> {
    code: &'a i32,
}
";

/// `panic_fmt` as core writes it (the declaration in its body, then the call), and `main`,
/// which reaches it.
const PANICS: &str = "
fn panic_fmt(code: &i32) -> ! {
    unsafe extern \"Rust\" {
        #[lang = \"panic_impl\"]
        fn panic_impl(pi: &PanicInfo<'_>) -> !;
    }
    let pi = PanicInfo { code };
    unsafe { panic_impl(&pi) }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let code = 7;
    panic_fmt(&code)
}
";

const HANDLER: &str = "
#[panic_handler]
fn on_panic(info: &PanicInfo<'_>) -> ! {
    loop {}
}
";

/// The same fn with no attribute: the graph then has no handler.
const NOT_A_HANDLER: &str = "
fn on_panic(info: &PanicInfo<'_>) -> ! {
    loop {}
}
";

const SECOND_HANDLER: &str = "
#[panic_handler]
fn on_panic_too(info: &PanicInfo<'_>) -> ! {
    loop {}
}
";

/// `f` over the program `text` made for a run (`Front2Program::new`): its refusal's text, or
/// `None` when it is made.
fn run_refusal(text: &str) -> Option<String> {
    let files = [SourceFile { path: "src/main.rs", text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the program's frame");
    match Front2Program::new(&names, &frame, &files, blaze_own::Alloc::global()) {
        Ok(_) => None,
        Err(e) => Some(format!("{e}")),
    }
}

/// The walk from `main` reaches `panic_fmt`, meets its call of the `panic_impl` declaration
/// and queues the program's `#[panic_handler]` fn: `on_panic` is tabled WITH ITS BODY, nothing
/// is left out, no row is a foreign fn (so none is the symbol `panic_impl`), and the back half
/// builds all three bodies with no refusal: `panic_fmt`'s call is an instance of `on_panic`.
///
/// Prints: `PANIC ENTRY: `on_panic` tabled with its body; no foreign symbol `panic_impl`;
/// BUILDS: 3 of 3 instances (0 refused)` and
/// `test a_call_of_panic_impl_reaches_and_binds_the_handler ... ok`
#[test]
fn a_call_of_panic_impl_reaches_and_binds_the_handler() {
    let text = [HEAD, HANDLER, PANICS].concat();
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
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn the walk reaches; left out: {left:?}");
    let rows: Vec<(String, bool, FnKind, String)> = (0..program.fn_count())
        .map(|i| {
            let f = program.function(i);
            (f.path.to_string(), f.body.is_some(), f.kind, f.symbol.to_string())
        })
        .collect();
    let handler = rows.iter().find(|(path, ..)| path.ends_with("on_panic"));
    assert!(matches!(handler, Some((_, true, FnKind::Free, _))), "`on_panic` tabled with its body: {rows:?}");
    let foreign: Vec<&(String, bool, FnKind, String)> = rows.iter().filter(|(_, _, kind, symbol)| *kind == FnKind::Foreign || symbol == "panic_impl").collect();
    assert!(foreign.is_empty(), "no row is a foreign fn, none the symbol `panic_impl`: {foreign:?}");
    let with_body = rows.iter().filter(|(_, body, ..)| *body).count();
    assert_eq!(with_body, 3, "`main`, `panic_fmt` and `on_panic` have bodies: {rows:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
    let refusals: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(refusals.is_empty(), "every body builds, `panic_fmt`'s call bound to `on_panic`: {refusals:?}");
    assert_eq!((counts.built, counts.tried), (3, 3), "`main`, `panic_fmt`, `on_panic`");
    println!("PANIC ENTRY: `on_panic` tabled with its body; no foreign symbol `panic_impl`; BUILDS: {} of {} instances ({} refused)", counts.built, counts.tried, refusals.len());
}

/// [panic.panic_handler.unique]: with no `#[panic_handler]` fn in the graph, a program whose
/// `main` reaches the call is refused by that rule, quoted, not by a missing symbol.
///
/// Prints: `NO HANDLER: front2: `..`, reached from `main`, is not checked (a call of the
/// `#[lang = "panic_impl"]` declaration with no `#[panic_handler]` fn to bind it to (..)): ..`
/// and `test a_panic_with_no_handler_is_refused_by_the_rule ... ok`
#[test]
fn a_panic_with_no_handler_is_refused_by_the_rule() {
    let refusal = run_refusal(&[HEAD, NOT_A_HANDLER, PANICS].concat()).expect("a program that panics with no handler is refused");
    println!("NO HANDLER: {refusal}");
    assert!(refusal.contains("no `#[panic_handler]` fn"), "{refusal}");
    assert!(refusal.contains("There must be a single panic_handler function in the dependency graph"), "{refusal}");
    // With its handler the same program is made.
    assert_eq!(run_refusal(&[HEAD, HANDLER, PANICS].concat()), None);
}

/// [panic.panic_handler.unique], the other way: two `#[panic_handler]` fns are refused by
/// the attribute's name before any body is read (the lang rows' one-item-per-name rule).
///
/// Prints: `TWO HANDLERS: front2 could not start: the lang item `panic_handler` is declared
/// twice: .. and ..` and `test two_handlers_are_refused_by_name ... ok`
#[test]
fn two_handlers_are_refused_by_name() {
    let refusal = run_refusal(&[HEAD, HANDLER, SECOND_HANDLER, PANICS].concat()).expect("two handlers are refused");
    println!("TWO HANDLERS: {refusal}");
    assert!(refusal.contains("`panic_handler` is declared twice"), "{refusal}");
}
