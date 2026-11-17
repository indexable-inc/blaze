//! A tuple struct's or tuple variant's CTOR used as a function, not as the written callee of a
//! call: built as a function of its own (blaze-build ctor_fn.rs) and called, BUILT AND RUN.
//!
//! The census of 2026-11-17 03:32 (port27/tempo-run/build-census-g033201.txt) refused 283
//! instances with "callee 0x..: which is tabled with no body and is no outside symbol"; its
//! example is core's `<impl<'b> Pattern for &'b [char]>::is_prefix_of`, which core's
//! `pattern_methods!` writes as `($pmap)(self).is_prefix_of(haystack)` with `$pmap` the tuple
//! struct `MultiCharEqPattern`: a call whose callee is a PARENTHESIZED path. The walk builds a
//! ctor in place only when the callee expression is the ctor's path itself (walk.rs `construct`
//! by `expr_def(callee)`); every other use (`(W)(x)`, a fn item value `let f = E::A; f(x)`, a fn
//! pointer `let p: fn(..) -> W = W;`, a `Fn*` value `.map(Some)` / `map_err(Error::Io)`) calls
//! the ctor's fn, and the ctor's row has no body, so run.rs `build_from` refused it.
//!
//! The rules (the Rust Reference): "Structs": a tuple struct "also defines a constructor
//! function"; "Enumerations": a tuple-like variant's name likewise; "Function item types": the
//! ctor's name is a value of a fn item type, "Type coercions": it coerces to a fn pointer, and
//! "All function items implement `Fn`, `FnMut`, `FnOnce`". "Grouped expressions": `(expr)` has
//! the value of `expr`.
//!
//! The programs are `#![no_core]` (no toolchain), built and run in this process; the statuses
//! are the arithmetic the comments give.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

/// Overflow wraps: a `no_core` program declares no `panic_const_add_overflow`, and no sum here
/// overflows.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items, a struct and an enum with tuple ctors, and the readers of their values.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}
impl Copy for u8 {}

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

struct W(u32, u8);

enum E {
    A(u32),
    B(u8, u8),
    C,
}

fn get(w: W) -> u32 {
    w.0 + w.1 as u32
}

fn e(v: E) -> u32 {
    match v {
        E::A(x) => x,
        E::B(a, b) => a as u32 * 10 + b as u32,
        E::C => 100,
    }
}
";

/// Builds and runs `HEAD` + `main`: the status, or the refusal on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        // One short line: a refusal's whole text can run past what a log line shows.
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(400).map_or(said.clone(), |(i, _)| said[..i].to_string())
    })
}

/// The census's shape: a parenthesized ctor path as the callee, written directly and through a
/// `macro_rules!` `$p:expr` (core's `pattern_methods!`). `get(W(3, 4))` = 7, `e(E::A(5))` = 5,
/// `e(E::B(1, 2))` = 12: status 24.
/// Prints `CTOR THROUGH PARENS: Ok(24)`.
#[test]
fn a_parenthesized_ctor_callee_builds_and_runs() {
    let got = run("
macro_rules! call {
    ($p:expr, $x:expr) => {
        ($p)($x)
    };
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (get((W)(3, 4)) + e(call!(E::A, 5)) + e((E::B)(1, 2))) as i32
}
");
    println!("CTOR THROUGH PARENS: {got:?}");
    assert_eq!(got, Ok(24));
}

/// A ctor as a fn item VALUE and as a fn POINTER: `e(f(9))` = 9, `get(p(6, 1))` = 7: status 16.
/// Prints `CTOR AS A VALUE: Ok(16)`.
#[test]
fn a_ctor_as_a_value_and_a_pointer_builds_and_runs() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let f = E::A;
    let p: fn(u32, u8) -> W = W;
    (e(f(9)) + get(p(6, 1))) as i32
}
");
    println!("CTOR AS A VALUE: {got:?}");
    assert_eq!(got, Ok(16));
}

/// A ctor as an `Fn*` value (`.map(Some)`'s shape): the `FnOnce::call_once` shim at the ctor's
/// fn item calls the ctor's fn. `e(apply(E::A, 11))` = 11, `get(apply2(W, 2, 3))` = 5: status 16.
/// Prints `CTOR AS AN FN VALUE: Ok(16)`.
#[test]
fn a_ctor_as_an_fn_trait_value_builds_and_runs() {
    let got = run("
fn apply<F: FnOnce(u32) -> E>(f: F, x: u32) -> E {
    f(x)
}

fn apply2<F: Fn(u32, u8) -> W>(f: F, x: u32, y: u8) -> W {
    f(x, y)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (e(apply(E::A, 11)) + get(apply2(W, 2, 3))) as i32
}
");
    println!("CTOR AS AN FN VALUE: {got:?}");
    assert_eq!(got, Ok(16));
}
