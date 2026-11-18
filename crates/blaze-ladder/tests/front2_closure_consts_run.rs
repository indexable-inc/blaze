//! Constants that meet a closure, BUILT AND RUN on the new front end (lane CF, tempo's build
//! census g054702, `CLASS builder 367 const at the instance: .. a closure whose facts the world
//! did not answer`, e.g. `<impl<T> Box<T>>::new`):
//! - `Box::new(x)` reads `<T as SizedTypeProperties>::LAYOUT`, whose value reads
//!   `size_of::<Self>()`: at `T` a closure the evaluator lays the closure out, from its facts.
//!   The evaluator's world answered no closure's facts at all; it now answers from the check
//!   record of the body that holds the closure, the build's one rule (run.rs
//!   `ClosureTable::add`): a fn the build checked (the closure among the instance's arguments)
//!   or a constant's body the evaluator checked (a closure written in the constant);
//! - an associated constant whose body makes a closure, read at two instances of a generic fn;
//! - a `const { .. }` block whose body makes a closure, at two instances;
//! - the control: a closure in a const block that captures the enclosing fn's parameter, which
//!   Rust refuses (E0435, "attempt to use a non-constant value in a constant").
//!
//! The rules (the Rust Reference): "Closure types": a closure's type holds its captures, so its
//! size is theirs (a `move` closure holds each captured value; one capturing nothing is
//! zero-sized). "Constant evaluation": a closure expression may be evaluated in a constant (a
//! call of a non-`const` closure may not, E0015, so none is called here). "Associated
//! constants": a trait's default is evaluated at the impl's `Self`. "Const blocks": an inline
//! constant may use the enclosing fn's generic parameters but not its locals.
//!
//! Without the fix (`ConstsWorld::closure` answering `None`) each of the first three is refused
//! with the census's words, "a closure whose facts the world did not answer" (runner trial
//! 081142-CF, 2026-11-17).
//!
//! The library is the async twin's (crates/blaze-cli/tests/ladder/front2/x0-async-01); the
//! programs are `#![no_core]`, built and run in this process with no toolchain.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const FUTURE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use prelude::*;
";

/// Builds and runs `HEAD` + `main` with the async twin's library: the status, or the refusal
/// on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
        SourceFile { path: "src/future.rs", text: FUTURE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// `Box::new`'s shape: a trait's default constant `SIZE = size_of::<Self>()` (core's
/// `SizedTypeProperties`), implemented for every `T`, read by a generic fn at three closure
/// types of `main`: one moving a `u64` (8 bytes), one capturing nothing (0), one moving a
/// `u32` (4). 8 + 0 + 4 = 12.
#[test]
fn a_constant_at_a_closure_type_runs() {
    let got = run("
trait Props: Sized {
    const SIZE: usize = size_of::<Self>();
}

impl<T> Props for T {}

fn size<T>(_: T) -> usize {
    <T as Props>::SIZE
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = 5u64;
    let b = 7u32;
    (size(move || a) + size(|| 1u8) + size(move |x: u32| x + b)) as i32
}
");
    println!("CONST AT A CLOSURE: {got:?}");
    assert_eq!(got, Ok(12));
}

/// An associated constant whose body makes a closure (never called), read by a generic fn at
/// `u8` and at `u64`: `size_of::<T>() + 1`, 2 + 9 = 11.
#[test]
fn an_associated_constant_holding_a_closure_runs() {
    let got = run("
trait Tag {
    const X: usize;
}

impl<T> Tag for T {
    const X: usize = {
        let _f = |a: usize| a + 1;
        size_of::<T>() + 1
    };
}

fn read<T>(_: T) -> usize {
    <T as Tag>::X
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (read(1u8) + read(1u64)) as i32
}
");
    println!("ASSOCIATED CONST WITH A CLOSURE: {got:?}");
    assert_eq!(got, Ok(11));
}

/// A `const { .. }` block whose body makes a closure, in a generic fn at `u16` and `u32`:
/// `size_of::<T>() + 2`, 4 + 6 = 10.
#[test]
fn a_const_block_holding_a_closure_runs() {
    let got = run("
fn block<T>(_: T) -> usize {
    const {
        let _f = |a: usize| a + 1;
        size_of::<T>() + 2
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (block(1u16) + block(1u32)) as i32
}
");
    println!("CONST BLOCK WITH A CLOSURE: {got:?}");
    assert_eq!(got, Ok(10));
}

/// core's `partition` (slice/sort/unstable/quicksort.rs, census class 32): a `const { .. }`
/// block calling a `const fn` generic over a closure type `F`, whose body holds a local
/// constant and returns one of two fn items at `<T, F>` as a fn pointer, then called. At `u8`
/// (1 byte, the first) the pointer's fn answers 3, at `[u64; 16]` (128 bytes) the second, 4.
///
/// The constant evaluates (its record holds a fn pointer, `DataTarget::Fn(key)`, a hash with no
/// inverse) and the record names the instance it keyed (blaze-consteval `ConstRecord::fns`:
/// `small::<u8, {closure}>`, generic), which the build's `Adts::const_fn_instance` (run.rs,
/// front2 consts.rs `Consts::fn_instance`) reads. Until the record named it this was refused
/// ("a constant's value holds a fn pointer to instance .., which the build's `Adts` does not
/// give"); now it runs: 3 * 10 + 4.
#[test]
fn a_const_block_choosing_a_fn_pointer_over_a_closure_type_runs() {
    let got = run("
const fn inst<T, F: FnMut(&T, &T) -> bool>() -> fn(&T, &mut F) -> usize {
    const MOST: usize = 96;
    if size_of::<T>() <= MOST { small::<T, F> } else { large::<T, F> }
}

fn small<T, F: FnMut(&T, &T) -> bool>(v: &T, less: &mut F) -> usize {
    if less(v, v) { 0 } else { 3 }
}

fn large<T, F: FnMut(&T, &T) -> bool>(v: &T, less: &mut F) -> usize {
    if less(v, v) { 0 } else { 4 }
}

fn partition<T, F: FnMut(&T, &T) -> bool>(v: &T, mut less: F) -> usize {
    (const { inst::<T, F>() })(v, &mut less)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = partition(&1u8, |_: &u8, _: &u8| false);
    let b = partition(&[0u64; 16], |_: &[u64; 16], _: &[u64; 16]| false);
    (a * 10 + b) as i32
}
");
    println!("PARTITION'S CONST BLOCK: {got:?}");
    assert_eq!(got, Ok(34), "partition's const block: the generic fn pointer's instance is named by the record");
}

/// The control: a closure in a const block capturing the fn's parameter `x`, a value no
/// constant may use (E0435): refused, never built.
#[test]
fn a_const_block_closure_capturing_a_local_is_refused() {
    let got = run("
fn bad(x: usize) -> usize {
    const {
        let _f = move |a: usize| a + x;
        3
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    bad(4) as i32
}
");
    println!("CONST BLOCK CAPTURING A LOCAL: {got:?}");
    // The check names the parameter as not in the const block's scope (front2's E0425 where
    // rustc says E0435): the fn is refused before any build.
    assert!(matches!(&got, Err(e) if e.contains("cannot find value `x`")), "a const block may not use the fn's parameter (E0435), got {got:?}");
}
