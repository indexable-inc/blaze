//! RFC 2229 uncaptured outer locals in non-reading place contexts inside closures and async
//! blocks, BUILT AND RUN natively on the new front end (no `rustc` invocation).
//!
//! Covers the `askama_parser::char_lit` pattern (`enum CharPrefix { Binary }` matched inside a
//! `move ||` closure that captures another variable so `prefix` is omitted from `EnvField`s),
//! non-ZST `Drop` locals in `let _ = outer;`, `_ = outer;`, `let _ = pair.b;` (with sibling
//! `pair.a` captured), `match outer { _ => ... }`, `match outer_wrap { Wrap(_) => ... }`,
//! `match outer_one { One::A(_) => ... }`, `if let`, `let`, reference-peeled 1-variant enum
//! matches, nested closures, and `async move` blocks.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> =
    NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const FUTURE: &str = include_str!("../../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use prelude::*;

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

enum CharPrefix {
    Binary,
}

struct Wrap(Counted);

enum One {
    A(Counted),
}
";

fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
        SourceFile { path: "src/future.rs", text: FUTURE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}").replace('\n', " "))
}

/// Exact `askama_parser::char_lit` shape: a 1-variant ZST enum `CharPrefix::Binary` matched
/// inside a `move ||` closure alongside another captured variable `bonus` (so `capture_places`
/// is non-empty and `prefix` is genuinely omitted from `EnvField`s), plus standalone `move ||`.
#[test]
fn char_prefix_single_variant_zst_enum_match_in_move_closure_runs() {
    let got = run("
fn call<F: FnOnce() -> i32>(f: F) -> i32 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let prefix = CharPrefix::Binary;
    let bonus = 5i32;
    let a = call(move || match prefix {
        CharPrefix::Binary => 7 + bonus,
    });
    let b = call(move || match prefix {
        CharPrefix::Binary => 3,
    });
    a + b
}
");
    assert_eq!(got, Ok(15));
}

/// Non-ZST `Drop` local in `let _ = outer;` and `_ = outer;` inside `move ||`: `outer` is NOT
/// captured, NOT moved, NOT dropped by the closure, and remains usable after the closure runs.
#[test]
fn uncaptured_drop_local_in_let_wildcard_is_not_moved_or_dropped() {
    let got = run("
fn call<F: FnOnce() -> u8>(f: F) -> u8 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut after_closures = 0u8;
    let mut sum = 0u8;
    {
        let outer = Counted { p, v: 6 };
        let k = 10u8;
        let a = call(move || {
            let _ = outer;
            42
        });
        let b = call(move || {
            _ = outer;
            k
        });
        after_closures = drops;
        sum = a + b + outer.v;
    }
    // `drops` must be 0 right after the closures and 1 after the inner block ends:
    // sum = 42 + 10 + 6 = 58, after_closures = 0, drops = 1 -> 58 + 0 + 100 = 158.
    (sum + after_closures * 50 + drops * 100) as i32
}
");
    assert_eq!(got, Ok(158));
}

/// Disjoint field discard: `pair.a` is captured while `pair.b` (a `Drop` field) is discarded
/// via `let _ = pair.b;`. Must not fail with `Cover::Above` or move `pair.b`.
#[test]
fn uncaptured_sibling_drop_field_discard_with_other_field_captured_runs() {
    let got = run("
struct Pair {
    a: u8,
    b: Counted,
}

fn call<F: FnOnce() -> u8>(f: F) -> u8 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut mid_drops = 0u8;
    let mut res = 0u8;
    {
        let pair = Pair { a: 17, b: Counted { p, v: 4 } };
        let from_clo = call(move || {
            let _ = pair.b;
            pair.a
        });
        mid_drops = drops;
        res = from_clo + pair.b.v;
    }
    // res = 17 + 4 = 21, mid_drops = 0, drops = 1 -> 21 + 10 = 31.
    (res + mid_drops * 50 + drops * 10) as i32
}
");
    assert_eq!(got, Ok(31));
}

/// 1-variant non-ZST `Drop` struct/enum matched with wildcard (`Wrap(_)`, `One::A(_)`, `_`),
/// plus `let Wrap(_) = ...` and `if let One::A(_) = ...`: outer bindings are NOT captured and
/// remain alive and usable after the closures execute.
#[test]
fn uncaptured_one_variant_drop_adt_matched_with_wildcard_stays_alive() {
    let got = run("
fn call<F: FnOnce() -> u8>(f: F) -> u8 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut mid_drops = 0u8;
    let mut total = 0u8;
    {
        let w = Wrap(Counted { p, v: 3 });
        let o = One::A(Counted { p, v: 4 });
        let c = Counted { p, v: 5 };
        let k = 9u8;
        let r1 = call(move || match w {
            Wrap(_) => k,
        });
        let r2 = call(move || match o {
            One::A(_) => k + 1,
        });
        let r3 = call(move || match c {
            _ => k + 2,
        });
        let r4 = call(move || {
            let Wrap(_) = w;
            if let One::A(_) = o { k + 3 } else { 0 }
        });
        mid_drops = drops;
        let One::A(inner_o) = o;
        total = r1 + r2 + r3 + r4 + w.0.v + inner_o.v + c.v;
    }
    // r1=9, r2=10, r3=11, r4=12 -> 42; values 3+4+5=12 -> total=54; mid_drops=0; drops=3 -> 54 + 30 = 84.
    (total + mid_drops * 50 + drops * 10) as i32
}
");
    assert_eq!(got, Ok(84));
}

/// Reference-peeled and `&`-matched uncaptured outer reference to a 1-variant enum: must NOT
/// dereference an uninitialized pointer.
#[test]
fn uncaptured_ref_to_single_variant_enum_match_does_not_deref() {
    let got = run("
fn call<F: FnOnce() -> i32>(f: F) -> i32 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let prefix = CharPrefix::Binary;
    let r = &prefix;
    let rr = &r;
    let k = 13i32;
    let a = call(move || match rr {
        CharPrefix::Binary => k,
    });
    let b = call(move || match r {
        &CharPrefix::Binary => k + 1,
    });
    a + b
}
");
    assert_eq!(got, Ok(27));
}

/// Nested closures `move || move || match prefix { CharPrefix::Binary => ... }` and nested
/// `let _ = outer_drop;`: neither outer nor inner closure captures the uncaptured local.
#[test]
fn nested_move_closures_with_uncaptured_zst_and_drop_locals_run() {
    let got = run("
fn call<F: FnOnce() -> u8>(f: F) -> u8 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut mid_drops = 0u8;
    let mut total = 0u8;
    {
        let prefix = CharPrefix::Binary;
        let outer_drop = Counted { p, v: 8 };
        let k = 11u8;
        let a = call(move || {
            call(move || match prefix {
                CharPrefix::Binary => k,
            })
        });
        let b = call(move || {
            call(move || {
                let _ = outer_drop;
                k + 2
            })
        });
        mid_drops = drops;
        total = a + b + outer_drop.v;
    }
    // a=11, b=13, outer_drop.v=8 -> total=32, mid_drops=0, drops=1 -> 32 + 10 = 42.
    (total + mid_drops * 50 + drops * 10) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Async block (`async move`) with an uncaptured outer `Drop` local in `let _ = outer_drop;`:
/// `coroutine.rs` must neither reject (`uncaptured`) nor move/drop `outer_drop`.
#[test]
fn async_block_with_uncaptured_outer_places_runs() {
    let got = run("
fn block_on<F: future::Future<Output = u8>>(mut fut: F) -> u8 {
    let mut wakes = 0u32;
    let mut cx = future::Context { wakes: &mut wakes };
    loop {
        match future::Future::poll(future::Pin { pointer: &mut fut }, &mut cx) {
            future::Poll::Ready(v) => return v,
            future::Poll::Pending => {}
        }
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut mid_drops = 0u8;
    let mut total = 0u8;
    {
        let outer_drop = Counted { p, v: 5 };
        let k = 19u8;
        let fut = async move {
            let _ = outer_drop;
            k
        };
        let r = block_on(fut);
        mid_drops = drops;
        total = r + outer_drop.v;
    }
    // total = 19 + 5 = 24, mid_drops = 0, drops = 1 -> 24 + 10 = 34.
    (total + mid_drops * 50 + drops * 10) as i32
}
");
    assert_eq!(got, Ok(34));
}
