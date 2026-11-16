//! Text out and in without `core::fmt` (docs/v2/plan/design/std.md, "What Rust's std got
//! wrong": "typed writers straight into a string", and "`FromStr` cannot borrow from its
//! input").
//!
//! Every `{}` in Rust goes through `fmt::Arguments`, a `dyn Write` and a function pointer per
//! argument: slow, large, and opaque to the optimizer. Here a line is a chain of direct calls
//! the compiler sees through:
//!
//! ```text
//! out.str("x = ").int(x).str(", took ").duration(ns).byte(b'\n');
//! ```
//!
//! | piece | what |
//! |---|---|
//! | [`Put`] | a sink of UTF-8: [`String<A>`](blaze_own::String), [`Buf<N>`] (fixed, refuses when full, never truncates), [`Count`] (only measures, for count-then-fill) |
//! | [`Writes`] | the chainable writers, on every `Put`: integers, hex, binary, floats, durations, byte counts, escapes, columns |
//! | [`Show`] | a type's human form, in place of `Display` for blaze's own types; [`Fixed`], [`Hex`], [`Nanos`], [`Bytes`], [`Quoted`] make a writer's form a value, for columns |
//! | [`Parse<'a>`] | a value and the rest of the text, the value free to borrow from it; combinators in [`parse`] |
//! | [`adapt`] | the one module that names `core::fmt`: [`Shown`] makes a `Show` a `Display`; [`PutDisplay::put_display`] writes a foreign `Display` through `fmt`, named so its cost shows |
//!
//! # Output is deterministic
//!
//! Every writer gives the same bytes for the same value on every machine (rule 25): no
//! locale, no hash order, no platform float formatting. Integers, hex, binary, `{:?}`-form
//! floats and fixed-place floats are byte-equal to `core::fmt`'s, which the tests hold.
//!
//! # The floor
//!
//! For a `u64` of `d` digits the work is `ceil(d / 2)` multiply-shift divisions by 100 and two
//! byte loads each from a 200-byte table, into a 48-byte stack buffer, then one copy into the
//! sink: about 1 ns per pair on a 2026 core, so 2 to 8 ns for a whole call into a `String` with
//! room, against `core::fmt`'s 15 to 30 ns (an estimate, not a measurement; `examples/cost.rs`
//! measures it). Floats are Ryu (Adams 2018) with tables computed at compile time: 20 to 40 ns.

#![no_std]
// blaze's lints (`blaze::trust`, `blaze::sans_io`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![blaze::trust(
    unsafe,
    reason = "one block: ASCII bytes the crate wrote from its own digit tables viewed as `str` (`ascii::Ascii::as_str`)"
)]
#![deny(unsafe_op_in_unsafe_fn)]

// The tests compare against `core::fmt` through std's `format!`, and plant bugs in a
// thread-local.
#[cfg(test)]
extern crate std;

pub mod adapt;
mod ascii;
mod big;
pub mod escape;
pub mod float;
pub mod human;
pub mod int;
pub mod parse;
pub mod put;
pub mod show;
mod write;

#[cfg(test)]
mod plant;
#[cfg(test)]
mod tests;

pub use adapt::{PutDisplay, Shown};
pub use float::Float;
pub use int::Integer;
pub use parse::{IntSyntax, Parse, ParseError, Unquoted, parse_all, parse_int, parse_quoted_in};
pub use put::{Buf, Count, Overflow, Put};
pub use show::{Bytes, Fixed, Hex, Nanos, Quoted, Show};
pub use write::Writes;
