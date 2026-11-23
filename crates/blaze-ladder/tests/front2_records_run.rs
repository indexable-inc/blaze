//! Structural anonymous records (`#![blaze::records]`) end-to-end native compile-and-run tests.
//!
//! Compiles real Blaze source code to native machine code via `blaze-ladder` (`FrontChoice::New`)
//! and executes it in-process, verifying record literals, canonical field-order normalization,
//! functional update (`..r`), destructuring (`let { a, b }`, `let { a, .. }`), autoderef field
//! access and reference-mode `match`, and nested records.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront {
    package: "ladder",
    edition: "blaze-2026.11",
    shelf: None,
    overflow: blaze_build::OverflowChecks::Off,
};

const HEAD: &str = "#![blaze::records]
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

/// Test 1: Anonymous record literal `{ b: 10u32, a: 32u32 }` passed to `fn sum(r: { a: u32, b: u32 }) -> u32`.
#[test]
fn record_literal_and_field_access() {
    let got = run("
fn sum(r: { a: u32, b: u32 }) -> u32 {
    r.a + r.b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    sum({ b: 10u32, a: 32u32 }) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Test 2: Record functional update `let r2 = { a: 50, ..r };` -> `r2.a + r2.b` -> exit 60.
#[test]
fn record_functional_update() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: { a: u32, b: u32 } = { a: 5u32, b: 10u32 };
    let r2 = { a: 50u32, ..r };
    (r2.a + r2.b) as i32
}
");
    assert_eq!(got, Ok(60));
}

/// Test 3: Record destructuring `let { a, b } = r;`, `let { a, .. } = r;`, and fn param `{ a, b }` -> exit 42.
#[test]
fn record_destructuring() {
    let got = run("
fn unpack({ a, b }: { a: u32, b: u32 }) -> u32 {
    let r2: { a: u32, b: u32 } = { a: a, b: b };
    let { a: x, b: y } = r2;
    let { a: z, .. } = { a: 2u32, b: 99u32 };
    x + y + z
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    unpack({ b: 10u32, a: 30u32 }) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Test 4: Autoderef field access on `&{ a: u32, b: u32 }` and `match &r { { a, b } => *a + *b }` -> exit 42.
#[test]
fn record_autoderef_and_ref_match() {
    let got = run("
fn get_a(r: &{ a: u32, b: u32 }) -> u32 {
    r.a
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: { a: u32, b: u32 } = { a: 10u32, b: 22u32 };
    let x = get_a(&r);
    let y = match &r {
        { a, b } => *a + *b,
    };
    (x + y) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Test 5: Nested records `{ inner: { x: 20u32, y: 22u32 }, tag: 1u8 }` -> exit 43.
#[test]
fn record_nested() {
    let got = run("
fn total(r: { inner: { x: u32, y: u32 }, tag: u8 }) -> u32 {
    r.inner.x + r.inner.y + r.tag as u32
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    total({ inner: { x: 20u32, y: 22u32 }, tag: 1u8 }) as i32
}
");
    assert_eq!(got, Ok(43));
}
