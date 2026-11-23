//! A closure's or a fn item's `Fn*` method as a WORD of a `dyn Fn*` vtable: the closure's body,
//! the fn item's shim, never a selected impl. Surveyed, then BUILT AND RUN.
//!
//! The census of 2026-11-17 09:12 (port27/tempo-run/build-census-g091226.txt) refused 467
//! instances with "callee: resolve NoImpl at a closure: `core::ops::function::FnMut::call_mut`"
//! (example: std's `_print_fmt`, which hands `&mut print_path`, a closure, to a
//! `&mut dyn FnMut(..)`). A call of a closure is tabled as its owner's body (run.rs
//! `closure_owner`) and a fn item's or pointer's as a shim (fn_shim.rs), but run.rs made a
//! vtable's method words by selecting an impl, and no written impl of `FnMut` holds at a
//! closure: the Reference, "Closure types": closures implement `Fn*` by the language.
//!
//! The rules (the Reference, "Trait objects"): a call through `&dyn Fn() -> u32` calls the
//! method of the value's type, found in the vtable; "Function item types": a fn item
//! implements `Fn`, `FnMut`, `FnOnce`. The programs are `#![no_core]` (no toolchain).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, build_survey, run_with};

/// No arithmetic below: overflow checks off, no `panic_const_*` item asked.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items (core's marker.rs, ops/unsize.rs, ops/function.rs), as the pinned library's.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> LegacyReceiver for &mut T {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"unsize\"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a mut U> for &'a mut T {}

#[lang = \"dispatch_from_dyn\"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a mut U> for &'a mut T {}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}
";

/// `_print_fmt`'s shape: a closure handed to a `dyn Fn*` reference, called through it; the
/// vtable's words are `call_once`, `call_mut` and `call` at the closure (`n` is `Copy`, so the
/// closure is `Fn`). 7.
const CLOSURE_AS_DYN_FN: &str = "
fn through(f: &dyn Fn() -> u32) -> u32 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let n = 7u32;
    let c = move || n;
    through(&c) as i32
}
";

/// A fn item handed to `&dyn Fn`, called through it. 9.
const FN_ITEM_AS_DYN_FN: &str = "
fn nine() -> u32 {
    9
}

fn through(f: &dyn Fn() -> u32) -> u32 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let f = nine;
    through(&f) as i32
}
";

/// Surveys `HEAD` + `tail` on the census's path: the refused rows, and `(built, tried)`.
fn survey(tail: &str) -> (Vec<String>, usize, usize) {
    let text = [HEAD, tail].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let texts = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::Off, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("SURVEY: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    (rows, counts.built, counts.tried)
}

/// Builds and runs `HEAD` + `tail`: the status, or the refusal on one line.
fn run(tail: &str) -> Result<u8, String> {
    let text = [HEAD, tail].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(400).map_or(said.clone(), |(i, _)| said[..i].to_string())
    })
}

/// Before the fix: a row "NoImpl at a closure" for a vtable word.
/// Prints `SURVEY: n of n instances (0 refused: )` and `CLOSURE AS DYN FN: Ok(7)`.
#[test]
fn a_closure_is_a_dyn_fns_word() {
    let got = run(CLOSURE_AS_DYN_FN);
    println!("CLOSURE AS DYN FN: {got:?}");
    assert_eq!(got, Ok(7));
    let (rows, built, tried) = survey(CLOSURE_AS_DYN_FN);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
}

/// Prints `SURVEY: n of n instances (0 refused: )` and `FN ITEM AS DYN FN: Ok(9)`.
#[test]
fn a_fn_item_is_a_dyn_fns_word() {
    let (rows, built, tried) = survey(FN_ITEM_AS_DYN_FN);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
    let got = run(FN_ITEM_AS_DYN_FN);
    println!("FN ITEM AS DYN FN: {got:?}");
    assert_eq!(got, Ok(9));
}
