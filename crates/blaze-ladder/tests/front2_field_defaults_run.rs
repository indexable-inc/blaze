//! Default field values BUILD AND RUN (RFC 3681, `default_field_values`; D126, rule 83
//! `options-struct`): `S { a: 1, .. }` is built as `S { a: 1, b: <default of b> }` would be.
//! The pieces: front2/const_at.rs `needs_of` pulls each default (`Checked::default_fields`) as
//! a constant at the struct type's ground arguments; blaze-build walk.rs `init_defaults` stores
//! each into the literal; blaze-consteval machine/code.rs builds a `Defaults` base inside a
//! constant the same way. The checker side is tests/front2_field_defaults.rs.
//!
//! The programs are `#![no_core]` on the blaze edition (the feature on unasked), built and run
//! in this process with no toolchain (`NewFront::shelf` is `None`). `&'static str`'s length is
//! read through a union of the wide pointer and its two words (the layout `str`'s references
//! have: data pointer, then length), as `core` has no `len` here.
//!
//! The values are arithmetic on the defaults as declared (rustc nightly gives the same):
//! - `show(Opts { name: "x", .. })` = 80 + 0 + 1 = 81, `show(Opts { width: 3, name: "yy", .. })`
//!   = 3 + 0 + 2 = 5: status 86;
//! - `const O: Opts = Opts { name: "x", .. };` read by `show(O)`: 81 (consteval's build);
//! - `Buf<const N: usize> { len: usize = N, data: [u8; N] = [0; N] }` at N = 4 and N = 8: the
//!   defaults at each instance's arguments, 4 + 8 = 12.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

/// The blaze edition: field defaults are on unasked. Overflow wraps (the `release` profile's):
/// a `no_core` program declares no `panic_const_add_overflow`, and no sum here overflows.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "blaze-2026.11", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items, `Copy` for what the union holds, and `len` of a `&'static str`.
const HEAD: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for usize {}

#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}

impl Copy for Words {}

union Wide {
    s: &'static str,
    words: Words,
}

fn len(s: &'static str) -> u32 {
    unsafe { Wide { s }.words.len as u32 }
}

pub struct Opts {
    pub width: u32 = 80,
    pub wrap: bool = false,
    pub name: &'static str,
}

fn show(o: Opts) -> u32 {
    o.width + o.wrap as u32 + len(o.name)
}
";

/// Builds and runs `HEAD` + `main`: the status, or the refusal on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// Two base-less literals of a three-field options struct, each unwritten field its default.
/// Prints `DEFAULTS RUN: status 86`.
#[test]
fn a_defaulted_literal_builds_and_runs() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (show(Opts { name: \"x\", .. }) + show(Opts { width: 3, name: \"yy\", .. })) as i32
}
");
    println!("DEFAULTS RUN: {got:?}");
    assert_eq!(got, Ok(86));
}

/// A base-less literal inside a constant: evaluated by consteval, read by path.
/// Prints `DEFAULTS IN A CONST: status 81`.
#[test]
fn a_defaulted_literal_in_a_constant() {
    let got = run("
const O: Opts = Opts { name: \"x\", .. };

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    show(O) as i32
}
");
    println!("DEFAULTS IN A CONST: {got:?}");
    assert_eq!(got, Ok(81));
}

/// Defaults naming the ADT's const parameter, at two arguments: each its own value.
/// Prints `GENERIC DEFAULTS: status 12`.
#[test]
fn generic_defaults_at_two_arguments() {
    let got = run("
pub struct Buf<const N: usize> {
    pub len: usize = N,
    pub data: [u8; N] = [0; N],
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let four: Buf<4> = Buf { .. };
    let eight: Buf<8> = Buf { .. };
    (four.len + eight.len) as i32
}
");
    println!("GENERIC DEFAULTS: {got:?}");
    assert_eq!(got, Ok(12));
}
