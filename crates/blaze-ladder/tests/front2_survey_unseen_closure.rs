//! A resolve at an instance whose arguments hold a closure the resolving context did not import
//! itself: no panic, the callee found, the program BUILT AND RUN.
//!
//! The census of 2026-11-17 08:37 (port27/tempo-run/build-census-g083729.err, tree db5a73a0c2)
//! panicked after reach, in `run::build_from`'s resolve of a callee: "blaze-typeck: a closure
//! this context did not make" (infer/mod.rs `view`, through instance.rs `export_list` of the
//! selected impl's arguments in `select_owning`). A closure term reaches a selection's answer
//! without passing `TyView::ClosureOf` (the one place a context files a closure of another
//! body) when the solver reveals an opaque type: its hidden type's terms are made from the
//! world's facts (facts.rs `closure_term`, `coroutine_term`) and instantiated by the solver at
//! the opaque's arguments (blaze-solve alias.rs `normalize_opaque_type`), never by typeck.
//!
//! The rules (the Rust Reference): "Closure types": a closure "has a unique, anonymous type";
//! "Impl trait": a function returning `impl Trait` returns its hidden type, which code after
//! monomorphization sees; `impl<F, B> Get for Mapper<F> where F: Fn() -> B` binds `B` by the
//! projection `<F as FnOnce<()>>::Output` (as core's `impl<B, I, F> Iterator for Map<I, F>
//! where F: FnMut(I::Item) -> B`).
//!
//! The programs are `#![no_core]` (no toolchain); each is surveyed (front2's `survey` and
//! run.rs `build_survey`, the census's path), then built and run.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, build_survey, run_with};

/// No arithmetic below: overflow checks off, no `panic_const_*` item asked.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items (core's marker.rs, clone.rs, ops/function.rs) and `Clone`/`Copy` of `u32`.
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

#[lang = \"clone\"]
trait Clone: Sized {
    fn clone(&self) -> Self;
}

#[lang = \"copy\"]
trait Copy: Clone {}

impl Clone for u32 {
    fn clone(&self) -> u32 {
        *self
    }
}
impl Copy for u32 {}

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

/// `main`'s closure passed to `dup` (`Clone` at it, resolved in `dup`'s world) and to `call`
/// (a written trait's method under a where-clause on `F`, resolved in `call`'s world). 7.
const ANOTHER_BODYS_CLOSURE: &str = "
fn dup<F: Clone>(x: F) -> F {
    x.clone()
}

trait Tr {
    fn go(&self) -> u32;
}

impl<F: Fn() -> u32> Tr for F {
    fn go(&self) -> u32 {
        (*self)()
    }
}

fn call<F>(x: &F) -> u32
where
    F: Tr,
{
    x.go()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let n = 7u32;
    let c = move || n;
    let d = dup(c);
    call(&d) as i32
}
";

/// `Get::get` at `Mapper<fn item make>`, resolved in `read`'s world: the impl's `B` is
/// `<make as FnOnce<()>>::Output`, `make`'s opaque, revealed to its hidden type, `make`'s
/// closure: the selected impl's arguments hold a closure only the solver made. 5.
const HIDDEN_CLOSURE: &str = "
struct Mapper<F>(F);

trait Get {
    fn get(&self) -> u32;
}

impl<F, B> Get for Mapper<F>
where
    F: Fn() -> B,
    B: Fn() -> u32,
{
    fn get(&self) -> u32 {
        let b = (self.0)();
        b()
    }
}

fn five() -> u32 {
    5
}

fn make() -> impl Fn() -> u32 {
    || five()
}

fn read<G: Get>(g: &G) -> u32 {
    g.get()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let m = Mapper(make);
    read(&m) as i32
}
";

/// `call` at `make`'s opaque itself and at `W<opaque>`: selection binds the impl's `F` to the
/// opaque, which the solver normalizes to its hidden type, `make`'s closure. 6.
const OPAQUE_AS_SELF: &str = "
trait Tr {
    fn go(&self) -> u32;
}

impl<F: Fn() -> u32> Tr for F {
    fn go(&self) -> u32 {
        (*self)()
    }
}

struct W<F>(F);

impl<F: Fn() -> u32> Tr for W<F> {
    fn go(&self) -> u32 {
        (self.0)()
    }
}

fn make(n: u32) -> impl Fn() -> u32 {
    move || n
}

fn call<T: Tr>(x: &T) -> u32 {
    x.go()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = make(5);
    let b = W(make(6));
    let _ = call(&a);
    call(&b) as i32
}
";

/// `call` at `wrap`'s closure, whose owner's argument is `make`'s opaque: the solver
/// normalizes that argument to its hidden type, `make`'s closure, so the closure it binds the
/// blanket impl's `F` to is a term the context did not import. 5.
const CLOSURE_OVER_AN_OPAQUE: &str = "
trait Tr {
    fn go(&self) -> u32;
}

impl<F: Fn() -> u32> Tr for F {
    fn go(&self) -> u32 {
        (*self)()
    }
}

fn make(n: u32) -> impl Fn() -> u32 {
    move || n
}

fn wrap<F: Fn() -> u32>(f: F) -> impl Fn() -> u32 {
    move || f()
}

fn call<T: Tr>(x: &T) -> u32 {
    x.go()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let w = wrap(make(5));
    call(&w) as i32
}
";

/// An array in a selected impl's arguments: its length, a const, is exported beside the closure
/// walk (`adopt` must pass a const by; census g090627 panicked "node 6 is not a type"). 4.
const ARRAY_IN_IMPL_ARGS: &str = "
trait Tr {
    fn go(&self) -> u32;
}

struct W<T>(T);

impl<T> Tr for W<T> {
    fn go(&self) -> u32 {
        4
    }
}

fn call<T: Tr>(x: &T) -> u32 {
    x.go()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    call(&W([1u32, 2u32])) as i32
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

/// Prints `SURVEY: n of n instances (0 refused: )` and `ANOTHER BODY'S CLOSURE: Ok(7)`.
#[test]
fn another_bodys_closure_resolves_in_a_generic_fns_world() {
    let (rows, built, tried) = survey(ANOTHER_BODYS_CLOSURE);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
    let got = run(ANOTHER_BODYS_CLOSURE);
    println!("ANOTHER BODY'S CLOSURE: {got:?}");
    assert_eq!(got, Ok(7));
}

/// Before the fix: the survey panics "a closure this context did not make".
/// Prints `SURVEY: n of n instances (0 refused: )` and `HIDDEN CLOSURE: Ok(5)`.
#[test]
fn a_closure_revealed_from_an_opaque_in_a_selected_impls_arguments() {
    let (rows, built, tried) = survey(HIDDEN_CLOSURE);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
    let got = run(HIDDEN_CLOSURE);
    println!("HIDDEN CLOSURE: {got:?}");
    assert_eq!(got, Ok(5));
}

/// Prints `SURVEY: n of n instances (0 refused: )` and `OPAQUE AS SELF: Ok(6)`.
#[test]
fn an_opaque_bound_to_an_impls_parameter_resolves() {
    let (rows, built, tried) = survey(OPAQUE_AS_SELF);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
    let got = run(OPAQUE_AS_SELF);
    println!("OPAQUE AS SELF: {got:?}");
    assert_eq!(got, Ok(6));
}

/// Prints `SURVEY: n of n instances (0 refused: )` and `CLOSURE OVER AN OPAQUE: Ok(5)`.
#[test]
fn a_closure_over_an_opaque_resolves() {
    let (rows, built, tried) = survey(CLOSURE_OVER_AN_OPAQUE);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
    let got = run(CLOSURE_OVER_AN_OPAQUE);
    println!("CLOSURE OVER AN OPAQUE: {got:?}");
    assert_eq!(got, Ok(5));
}

/// Prints `SURVEY: n of n instances (0 refused: )` and `ARRAY IN IMPL ARGS: Ok(4)`.
#[test]
fn an_array_in_a_selected_impls_arguments_exports_its_length() {
    let (rows, built, tried) = survey(ARRAY_IN_IMPL_ARGS);
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(tried > 0 && built == tried, "{built} of {tried}");
    let got = run(ARRAY_IN_IMPL_ARGS);
    println!("ARRAY IN IMPL ARGS: {got:?}");
    assert_eq!(got, Ok(4));
}
