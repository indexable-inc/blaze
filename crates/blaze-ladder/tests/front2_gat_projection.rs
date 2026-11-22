//! A struct field whose type is a projection on a generic associated type, at an instance:
//! core's `CharPredicateSearcher<'a, F>(<MultiCharEqPattern<F> as Pattern>::Searcher<'a>)`
//! (library/core/src/str/pattern.rs), laid out at `F = IsWhitespace` (a named ZST whose
//! `Fn*` impls core's `impl_fn_for_zst!` writes by hand), at a closure and at a fn pointer.
//!
//! The census of 2026-11-17 03:32 (port27/tempo-run/build-census-g033201.txt) refused 37
//! instances with "Projection: `core::str::pattern::Pattern`'s associated type .. at Self =
//! `core::str::pattern::MultiCharEqPattern`: the solver's step gave no type" (e.g. `<impl Debug
//! for SplitWhitespace>::fmt`), 35 with "the associated type .. of trait .. at this instance
//! was not normalized by the seam's `normalize`" (`<impl Iterator for SplitWhitespace>::
//! size_hint`, the same associated type), and 6 with "`FnOnce`'s associated type .. at Self =
//! FnPtr: the solver's step gave no type".
//!
//! The rules (the Rust Reference): "Associated types" (a generic associated type's projection
//! `<T as Trait>::Assoc<'a>` is the impl's type at the impl's and the item's own arguments);
//! "Implementations" (`impl<F> MultiCharEq for F where F: FnMut(char) -> bool` holds for any
//! `F` whose `FnMut<(char,)>` impl exists and whose `<F as FnOnce<(char,)>>::Output` is
//! `bool`); "Function pointer types" and "Closure types" (both implement the `Fn*` traits).
//!
//! The programs are `#![no_core]` (no toolchain), built and run in this process. Each prints
//! one line, `GAT FIELD <case>: <result>`, before any assertion.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items and core's pattern shapes, cut to what the layout reads.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs, fn_traits)]
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
impl Copy for char {}
impl Copy for bool {}
impl<'a, T: ?Sized> Copy for &'a T {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver {}

impl<T: ?Sized> LegacyReceiver for &T {}
impl<T: ?Sized> LegacyReceiver for &mut T {}

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

trait MultiCharEq {
    fn matches(&mut self, c: char) -> bool;
}

impl<F> MultiCharEq for F
where
    F: FnMut(char) -> bool,
{
    fn matches(&mut self, c: char) -> bool {
        (*self)(c)
    }
}

trait Searcher<'a> {
    fn count(&self) -> u32;
}

trait Pattern: Sized {
    type Searcher<'a>: Searcher<'a>;
    fn into_searcher(self, haystack: &u32) -> Self::Searcher<'_>;
}

struct MultiCharEqPattern<C: MultiCharEq>(C);

struct MultiCharEqSearcher<'a, C: MultiCharEq> {
    char_eq: C,
    haystack: &'a u32,
    n: u32,
}

impl<'a, C: MultiCharEq> Searcher<'a> for MultiCharEqSearcher<'a, C> {
    fn count(&self) -> u32 {
        self.n + *self.haystack
    }
}

impl<C: MultiCharEq> Pattern for MultiCharEqPattern<C> {
    type Searcher<'a> = MultiCharEqSearcher<'a, C>;

    fn into_searcher(self, haystack: &u32) -> MultiCharEqSearcher<'_, C> {
        MultiCharEqSearcher { char_eq: self.0, haystack, n: 7 }
    }
}

struct CharPredicateSearcher<'a, F>(<MultiCharEqPattern<F> as Pattern>::Searcher<'a>)
where
    F: FnMut(char) -> bool;

fn make<'a, F: FnMut(char) -> bool>(f: F, h: &'a u32) -> CharPredicateSearcher<'a, F> {
    CharPredicateSearcher(MultiCharEqPattern(f).into_searcher(h))
}

fn read<'a, F: FnMut(char) -> bool>(s: CharPredicateSearcher<'a, F>) -> u32 {
    let mut s = s;
    let hit = s.0.char_eq.matches(' ');
    s.0.count() + if hit { 100 } else { 0 }
}

struct IsWhitespace;

impl Fn<(char,)> for IsWhitespace {
    extern \"rust-call\" fn call(&self, (c,): (char,)) -> bool {
        c as u32 == 32
    }
}

impl FnMut<(char,)> for IsWhitespace {
    extern \"rust-call\" fn call_mut(&mut self, (c,): (char,)) -> bool {
        c as u32 == 32
    }
}

impl FnOnce<(char,)> for IsWhitespace {
    type Output = bool;
    extern \"rust-call\" fn call_once(self, (c,): (char,)) -> bool {
        c as u32 == 32
    }
}

fn is_space(c: char) -> bool {
    c as u32 == 32
}
";

/// Builds and runs `HEAD` + `main`: the status, or the refusal on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// `make(<f>, &5)` read back: the searcher's 7 plus the haystack's 5, plus 100 when `<f>`
/// matches a space: 112 for each case.
fn case(f: &str) -> String {
    std::format!(
        "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    let h = 5u32;
    read(make({f}, &h)) as i32
}}
"
    )
}

/// The census's shape: `F` a named ZST with hand-written `Fn*` impls (core's `IsWhitespace`).
/// Prints `GAT FIELD named zst: Ok(112)`.
#[test]
fn a_gat_projection_field_at_a_named_fn_zst() {
    let got = run(&case("IsWhitespace"));
    println!("GAT FIELD named zst: {got:?}");
    assert_eq!(got, Ok(112));
}

/// `F` a closure. Prints `GAT FIELD closure: Ok(112)`.
#[test]
fn a_gat_projection_field_at_a_closure() {
    let got = run(&case("|c: char| c as u32 == 32"));
    println!("GAT FIELD closure: {got:?}");
    assert_eq!(got, Ok(112));
}

/// The field read in a generic body (`rd<'a, F>`) at `F = IsWhitespace`, never touched in
/// `main`: the build asks the projection at the instance, and `IsWhitespace: MultiCharEq` was
/// never asked while checking. Prints `GAT FIELD generic reader: Ok(12)`.
#[test]
fn a_gat_projection_field_read_in_a_generic_body() {
    let main = "fn rd<'a, F: FnMut(char) -> bool>(s: &CharPredicateSearcher<'a, F>) -> u32 { s.0.n }
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let h = 5u32;
    let s = make(IsWhitespace, &h);
    (rd(&s) + 5) as i32
}
";
    let got = run(main);
    println!("GAT FIELD generic reader: {got:?}");
    assert_eq!(got, Ok(12));
}

/// `F` a fn pointer (the census's `FnOnce::Output` at Self = FnPtr). Prints
/// `GAT FIELD fn pointer: Ok(112)`.
#[test]
fn a_gat_projection_field_at_a_fn_pointer() {
    let got = run(&case("is_space as fn(char) -> bool"));
    println!("GAT FIELD fn pointer: {got:?}");
    assert_eq!(got, Ok(112));
}
