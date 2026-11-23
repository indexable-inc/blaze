//! Structural error unions (`#![blaze::error_unions]`) end-to-end native compile-and-run tests.
//!
//! Compiles real Blaze source code to native machine code via `blaze-ladder` (`FrontChoice::New`)
//! and executes it in-process, verifying member injection (`A -> A | B`), sub-union widening
//! (`A | B -> A | B | C`), typed pattern matching, single- and multi-member residual narrowing,
//! niche-optimized unions (`&u32 | ()`), and fallible `T ! E` return types.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront {
    package: "ladder",
    edition: "blaze-2026.11",
    shelf: None,
    overflow: blaze_build::OverflowChecks::Off,
};

const HEAD: &str = "#![blaze::error_unions]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}
";

fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}").replace('\n', " "))
}

/// Test 1: Union injection `A -> A | B`, typed pattern match, and single-member residual narrowing.
#[test]
fn union_inject_and_single_residual() {
    let body = "
fn eval(x: u32 | bool) -> u32 {
    match x {
        n: u32 => n + 2,
        b => if b { 99 } else { 0 },
    }
}
";
    let got_u32 = run(&format!(
        "{body}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    eval(40u32) as i32
}}
"
    ));
    assert_eq!(got_u32, Ok(42));

    let got_bool = run(&format!(
        "{body}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    eval(true) as i32
}}
"
    ));
    assert_eq!(got_bool, Ok(99));
}

/// Test 2: Union widening `A | B -> A | B | C`, 3-way typed matching, and multi-member residual narrowing.
#[test]
fn union_widen_and_multi_residual() {
    let defs = "
struct ErrA(u32);
struct ErrB(u32);
struct ErrC(u32);

fn make_ab(which: u32) -> ErrA | ErrB {
    if which == 0 {
        ErrA(11)
    } else {
        ErrB(22)
    }
}

fn widen_abc(which: u32) -> ErrA | ErrB | ErrC {
    if which == 2 {
        ErrC(33)
    } else {
        make_ab(which)
    }
}

fn handle(x: ErrA | ErrB | ErrC) -> u32 {
    match x {
        a: ErrA => a.0 + 1,
        rest => match rest {
            b: ErrB => b.0 + 2,
            c => c.0 + 3,
        },
    }
}
";
    for (which, expected) in [(0u32, 12u8), (1, 24), (2, 36)] {
        let got = run(&format!(
            "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    handle(widen_abc({which})) as i32
}}
"
        ));
        assert_eq!(got, Ok(expected), "branch {which}");
    }
}

/// Test 3: Niche-optimized union `&u32 | ()` (non-null reference vs unit).
#[test]
fn union_niche_ref_or_unit() {
    let defs = "
fn unwrap_or_seven(opt: &u32 | ()) -> u32 {
    match opt {
        r: &u32 => *r,
        _ => 7,
    }
}
";
    let got_ref = run(&format!(
        "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    let x: u32 = 55;
    let opt: &u32 | () = &x;
    unwrap_or_seven(opt) as i32
}}
"
    ));
    assert_eq!(got_ref, Ok(55));

    let got_unit = run(&format!(
        "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    let opt: &u32 | () = ();
    unwrap_or_seven(opt) as i32
}}
"
    ));
    assert_eq!(got_unit, Ok(7));
}

/// Test 4: Fallible `T ! E` (`Result<T, E>`) with automatic error injection `ErrA -> ErrA | ErrB`.
#[test]
fn union_fallible_result_sugar() {
    let defs = "
enum Result<T, E> {
    Ok(T),
    Err(E),
}

struct ErrA(u32);
struct ErrB(u32);

fn check(x: u32) -> u32 ! ErrA | ErrB {
    if x == 0 {
        Result::Err(ErrA(10))
    } else if x == 1 {
        Result::Err(ErrB(20))
    } else {
        Result::Ok(x)
    }
}

fn dispatch(x: u32) -> u32 {
    match check(x) {
        Result::Ok(v) => v + 1,
        Result::Err(e) => match e {
            a: ErrA => a.0 + 3,
            b => b.0 + 5,
        },
    }
}
";
    for (input, expected) in [(0u32, 13u8), (1, 25), (41, 42)] {
        let got = run(&format!(
            "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    dispatch({input}) as i32
}}
"
        ));
        assert_eq!(got, Ok(expected), "input {input}");
    }
}
