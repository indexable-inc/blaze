//! A `#[track_caller]` `Location`'s line is placed, not built (rule 71): the code record of the
//! caller holds the call's item and its byte in the item's text, and placement fills the line
//! and the column from where the item is today (blaze-emit `PositionFix`, place.rs
//! `resolve_positions`, `FrontEnd::source_position`).
//!
//! The program is `#![no_core]` (no toolchain): a tracked `here()` returns its caller's line
//! through the `caller_location` intrinsic, and `main` returns what `here()` gives. Called from
//! line N of the file, the run's status is N; with two blank lines inserted above `main`, it
//! is N + 2, and `main`'s code record (the id placement binds it under and its bytes) is the
//! same: an edit above an item moves no record.
//!
//! It passes once `FrontAdts::track_caller` (run.rs) answers from the front end; while that is
//! held at `None` the build refuses `here()`'s own `caller_location` ("not said").

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, build_new, run_with};

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[track_caller]
fn here() -> u32 {
    caller_location().line
}
";

const MAIN: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    here() as i32
}
";

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The line (from 1) of `here()`'s call in `text`.
fn call_line(text: &str) -> u8 {
    let n = text.lines().position(|l| l.contains("here() as i32")).expect("the call") + 1;
    u8::try_from(n).expect("the call is on a line below 256")
}

/// `main`'s status and its code record (slot 0, the entry's) when the file is `text`.
fn run_and_record(text: &str) -> (u8, blaze_launch::wire::RecordId, Vec<u8>) {
    let files = [SourceFile { path: "src/main.rs", text }];
    let status = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("TRACK CALLER: refused: {e}"));
    let built = build_new(&files, "src/main.rs", NEW, blaze_own::testing::heap().shared().local())
        .unwrap_or_else(|e| panic!("TRACK CALLER: not built: {e}"));
    let (id, bytes) = built.code_record(0).expect("the entry's slot");
    (status, id, bytes.to_vec())
}

/// Called from line N, `here()` gives N; two blank lines above `main` give N + 2, and `main`'s
/// record id and bytes are equal across the edit. A builder that wrote the line into the code
/// fails the record assert; a placer that did not fill it fails the status assert (0).
///
/// Prints: `TRACK CALLER: line <N> then <N+2>; main's record equal across the edit`
#[test]
fn a_tracked_callers_line_is_placed_and_its_record_does_not_move() {
    let before = format!("{HEAD}{MAIN}");
    let after = format!("{HEAD}\n\n{MAIN}");
    let n = call_line(&before);
    assert_eq!(call_line(&after), n + 2);
    let (status, id, bytes) = run_and_record(&before);
    assert_eq!(status, n, "the status is the call's line");
    let (status2, id2, bytes2) = run_and_record(&after);
    assert_eq!(status2, n + 2, "two lines above `main` move the line by two");
    assert!(id == id2 && bytes == bytes2, "`main`'s code record moved with a line inserted above it");
    println!("TRACK CALLER: line {n} then {}; main's record equal across the edit", n + 2);
}
