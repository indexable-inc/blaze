//! A closure whose environment is zero-sized yet holds captures, BUILT AND RUN on the new front
//! end (lane CF, tempo's build census g054702, `Malformed .. a closure that captures with no
//! environment` at blaze-build closure.rs, 23 rows, e.g. `OrErrno::true_val_or_errno`):
//! - interprocess's shape: an `FnOnce` closure returning its one capture, a unit struct at the
//!   instance: its environment is that zero-sized capture, passed by value as nothing;
//! - a control: two zero-sized captures, one of them `Drop`, whose drop runs once, at the
//!   body's end (an environment holding a `Drop` value is passed in memory, so this one took
//!   the sized path before the fix too: trial 082256-CF and after);
//! - the control: an `FnOnce` closure moving a `u32` beside a unit struct (a sized environment,
//!   the path that always ran).
//!
//! The rules (the Rust Reference): "Closure types": a closure's type holds its captures, so a
//! closure capturing only zero-sized values by value is itself zero-sized; "Capture modes": a
//! `move` closure captures each place by value; "Call traits and coercions": a closure that
//! moves a captured value out of its body is `FnOnce` only, its `call_once` taking the
//! environment by value. "Destructors": a captured value moved into the closure is dropped
//! with it, here when the body that consumed the environment ends.
//!
//! Without the fix (closure.rs refusing every capture when the environment is passed as
//! nothing) the first is refused before it is built, `val`: "a malformed body or record", and
//! the two controls run (runner trials 082256-CF onward, 2026-11-17): the census's refusal was
//! the walk asking the absent environment for each capture's place.
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

/// interprocess's `OrErrno::true_val_or_errno` shape: `self.true_or_errno(|| value)` with
/// `value: T`, the closure returning its capture (so `FnOnce`), at `T = Unit`. 7.
#[test]
fn an_fn_once_closure_with_one_zero_sized_capture_runs() {
    let got = run("
struct Unit;

impl Unit {
    fn seven(self) -> i32 {
        7
    }
}

fn or_else<T>(f: impl FnOnce() -> T) -> T {
    f()
}

fn val<T>(value: T) -> T {
    or_else(|| value)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    val(Unit).seven()
}
");
    println!("ONE ZERO-SIZED CAPTURE: {got:?}");
    assert_eq!(got, Ok(7));
}

/// Two zero-sized captures, the second `Drop` (its drop adds 10 to a counter): the first is
/// consumed (5), the second dropped once when the body ends. 5 + 10 = 15.
#[test]
fn an_fn_once_closure_with_a_dropping_zero_sized_capture_runs() {
    let got = run("
static mut DROPS: i32 = 0;

struct Unit;

impl Unit {
    fn five(self) -> i32 {
        5
    }
}

struct Noisy;

impl Drop for Noisy {
    fn drop(&mut self) {
        unsafe { DROPS = DROPS + 10 };
    }
}

fn or_else<T>(f: impl FnOnce() -> T) -> T {
    f()
}

fn val<T, D>(value: T, held: D) -> T {
    or_else(|| {
        let _h = held;
        value
    })
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r = val(Unit, Noisy).five();
    r + unsafe { DROPS }
}
");
    println!("A DROPPING ZERO-SIZED CAPTURE: {got:?}");
    assert_eq!(got, Ok(15));
}

/// The control: a `u32` beside the unit struct makes the environment sized. 7 + 4 = 11.
#[test]
fn an_fn_once_closure_with_a_sized_environment_runs() {
    let got = run("
struct Unit;

impl Unit {
    fn seven(self) -> i32 {
        7
    }
}

fn once<F: FnOnce() -> i32>(f: F) -> i32 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let u = Unit;
    let k = 4u32;
    once(move || u.seven() + k as i32)
}
");
    println!("A SIZED ENVIRONMENT: {got:?}");
    assert_eq!(got, Ok(11));
}
