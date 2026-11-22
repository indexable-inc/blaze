//! An ASSIGNMENT to a captured binding whose type needs drop, BUILT AND RUN on the new front
//! end. tempo t1530: consensus-config's `read_secret_inner` closure 0 (`SecretBox::init_with_mut(
//! |buf: &mut String| { .. read_result = Err(..); .. })`, `read_result: io::Result<()>` captured
//! by `&mut`) was refused "an assignment to a binding that needs drop and is not owned"
//! (blaze-build walk.rs `Task::Assign`): the closure's body owns no captured binding but a
//! by-value capture of an `FnOnce` body, so a by-reference capture, and a by-value capture of
//! an `FnMut` body, fell to that refusal.
//!
//! The rule (the Reference, "Assignment expressions": "It then has the effect of first dropping
//! the value at the assigned place, unless the place is an uninitialized local variable or an
//! uninitialized field of a local variable", then the value is moved in; "Closure expressions",
//! "Capture modes": a binding assigned in the closure is captured by unique borrow, or by value
//! under `move`): the old value is dropped once at the assignment, wherever its place is, and
//! the new value is dropped by the place's owner (the enclosing body, or the environment).
//! Each row counts drops through a raw pointer, so a missed or doubled drop changes the status.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

/// A value whose drop counts itself at `p`.
struct Counted {
    p: *mut u8,
    v: u8,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            *self.p = *self.p + 1;
        }
    }
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The site's shape: `r` captured by `&mut` in an `FnOnce(&mut u8)` closure handed to a fn
/// (`init_with_mut`), assigned there. The old value drops at the assignment (1), the new one
/// at `r`'s scope end (2): `r.v + b` = 30 + 5, plus 10 per drop: 55. With no drop at the
/// assignment 45; refused before the fix.
#[test]
fn an_assignment_through_a_mut_capture_drops_the_old_value() {
    let got = run("
fn with_mut<F: FnOnce(&mut u8)>(f: F) -> u8 {
    let mut b = 2u8;
    f(&mut b);
    b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let got;
    {
        let mut r = Counted { p, v: 1 };
        let b = with_mut(|buf: &mut u8| {
            *buf = 5;
            r = Counted { p, v: 30 };
        });
        got = r.v + b;
    }
    (got + 10 * drops) as i32
}
");
    println!("ASSIGN THROUGH A MUT CAPTURE: {got:?}");
    assert_eq!(got, Ok(55), "old value dropped at the assignment, the new one at the scope's end");
}

/// The twin: `r` captured BY VALUE (`move`) in an `FnMut` body called twice: the environment
/// owns `r` and the body borrows it. Each call drops the old value (2), the environment's drop
/// at `twice`'s end drops the last (3): `11 + 21` = 32, plus 10 per drop: 62.
#[test]
fn an_assignment_to_a_by_value_capture_of_an_fn_mut_body_drops_the_old_value() {
    let got = run("
fn twice<F: FnMut() -> u8>(mut f: F) -> u8 {
    let a = f();
    let b = f();
    a + b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut r = Counted { p, v: 1 };
    let s = twice(move || {
        let old = r.v;
        r = Counted { p, v: old + 10 };
        r.v
    });
    (s + 10 * drops) as i32
}
");
    println!("ASSIGN TO A BY-VALUE CAPTURE OF AN FnMut BODY: {got:?}");
    assert_eq!(got, Ok(62), "two assignments drop two old values; the environment drops the last");
}

/// The control: the same assignment in the body that owns `r` (passed before the fix): one
/// drop at the assignment, one at the end: 7 + 20 = 27.
#[test]
fn an_assignment_to_an_owned_binding_drops_the_old_value() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let got;
    {
        let mut r = Counted { p, v: 1 };
        r = Counted { p, v: 7 };
        got = r.v;
    }
    (got + 10 * drops) as i32
}
");
    println!("ASSIGN TO AN OWNED BINDING: {got:?}");
    assert_eq!(got, Ok(27), "one drop at the assignment, one at the scope's end");
}
