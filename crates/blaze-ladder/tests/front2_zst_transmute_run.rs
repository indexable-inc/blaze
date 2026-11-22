//! A zero-sized transmute whose types need drop, BUILT AND RUN on the new front end. tempo
//! t1545: 9 tests NOT RUN at core's `<[T; N] as IntoIterator>::into_iter` at
//! `[clap_builder::util::id::Id, N = 0]` (core/src/array/iter.rs:48:5): "a malformed body or
//! record: an expression with no value where its type has one". The body is
//! `let data: [MaybeUninit<T>; N] = unsafe { transmute_unchecked(self) };`, then `data` moves
//! into the iterator.
//!
//! The rule. The Reference, "Type layout": an array's size is `size_of::<T>() * N`, so
//! `[Id; 0]` has size 0. `transmute` "Reinterprets the bits of a value of one type as another
//! type" (core's doc), so a zero-sized value has no bits to copy. Its result is still a value
//! of the target type, and it can be bound and moved.
//!
//! The cause (handoff m3-zero-sized-placed-intrinsic-result.txt):
//! - blaze-build's walk keeps a zero-sized type in memory when a part of it needs drop
//!   (`Layout::placed`). Clap's `Id` holds a `Box<str>`.
//! - The transmute intrinsic answers `Out::None` for any zero-sized result.
//! - The walk turned that answer into `V::None` whatever the result's representation was.
//!   The binding then asked a place of it.
//!
//! The site is `Loud`, which has a `Drop`. The twin is `u8`, which has no drop glue: both sides
//! hold no value, and it ran before the fix too. CONTROL (rule 23): with walk.rs's
//! `Out::None => V::None` back, the site fails with the refusal above and the twin still runs.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct Loud {
    v: u8,
}

impl Drop for Loud {
    fn drop(&mut self) {}
}

/// core's `MaybeUninit<T>` stand-in: a wrapper of `T` with no drop of its own, so it is held in
/// memory exactly when `T` needs drop.
struct Held<T> {
    v: T,
}

/// core's `IntoIter { inner: .. }` stand-in: the zero-sized array moved in beside a length.
struct Iter<T> {
    len: usize,
    data: [Held<T>; 0],
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The site: `[Loud; 0]` to `[Held<Loud>; 0]`, bound, moved into `Iter`, then dropped.
#[test]
fn a_zero_sized_transmute_of_a_type_that_needs_drop_runs() {
    let got = run("
fn site(a: [Loud; 0]) -> Iter<Loud> {
    let data: [Held<Loud>; 0] = unsafe { transmute(a) };
    Iter { len: 5, data }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let it = site([]);
    it.len as i32
}
");
    println!("ZST TRANSMUTE (needs drop): {got:?}");
    assert_eq!(got, Ok(5), "a zero-sized transmute result held in memory is a value the body binds and moves");
}

/// The twin: the same at `u8`, which has no drop glue. Both sides hold no value.
#[test]
fn twin_a_zero_sized_transmute_of_plain_data_runs() {
    let got = run("
fn site(a: [u8; 0]) -> Iter<u8> {
    let data: [Held<u8>; 0] = unsafe { transmute(a) };
    Iter { len: 5, data }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let it = site([]);
    it.len as i32
}
");
    println!("ZST TRANSMUTE (plain): {got:?}");
    assert_eq!(got, Ok(5));
}
