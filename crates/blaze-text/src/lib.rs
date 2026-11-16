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
//!
//! # Measured
//!
//! `examples/cost.rs`, release, 2026-11-15, one run, this machine (an Apple-silicon Mac); ns
//! per operation with the loop's own cost subtracted, before the fixes listed under it:
//!
//! | operation | blaze-text | core | ratio |
//! |---|---|---|---|
//! | u64 decimal, 1 digit | 3.3 | 5.6 | 1.71x |
//! | u64 decimal, 5 digits | 5.3 | 6.9 | 1.30x |
//! | u64 decimal, 10 digits | 6.3 | 6.9 | 1.10x |
//! | u64 decimal, 20 digits | 10.7 | 9.3 | 0.87x |
//! | u64 decimal, every length | 7.6 | 10.0 | 1.32x |
//! | u64 hex | 7.4 | 9.8 | 1.32x |
//! | f64 shortest | 33.9 | 51.3 | 1.51x |
//! | f64 fixed, 3 places | 16.5 | 18.0 | 1.09x |
//! | `"{}: {} of {} ({:.1}%)"` | 19.0 | 47.2 | 2.49x |
//! | u64 parse, 1 digit | 2.8 | 1.0 | 0.35x |
//! | u64 parse, 5 digits | 4.6 | 2.3 | 0.49x |
//! | u64 parse, 10 digits | 3.9 | 3.6 | 0.92x |
//! | u64 parse, 20 digits | 6.5 | 13.0 | 1.99x |
//! | u64 parse, every length | 5.9 | 5.6 | 0.95x |
//! | f64 parse | 21.0 | 12.5 | 0.59x |
//! | vector digit run against its scalar twin, 1 to 15 digits | | | 1.1x to 1.7x |
//! | the same, 20 digits | | | 0.86x |
//! | next escape byte after 4 / 16 / 64 / 256 plain bytes, vector against scalar | | | 0.75x / 6.2x / 11.1x / 13.5x |
//!
//! Fixed since, not yet measured: short integer parses read one word and convert under eight
//! digits without the vector; `parse_all` of a float hands a plain decimal to `core` without
//! scanning it first; `u64` writes take eight digits at a time in 32-bit halves; the escape
//! search takes the byte loop under 16 bytes; `String`'s `put_byte` pushes the byte directly.

#![no_std]
// blaze's lints (`blaze::trust`, `blaze::sans_io`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
// `core::simd`: digit runs, escape scanning and hex encoding (`simd`).
#![feature(portable_simd)]
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
mod simd;
mod write;

/// The vector routines and their scalar twins side by side, for `examples/cost.rs` to time
/// the crossover; not an interface.
#[doc(hidden)]
pub mod twins {
    /// A decimal `u64` at the front of `bytes`: `(value, digits)`, or `None`.
    pub fn decimal_u64(bytes: &[u8]) -> Option<(u64, usize)> {
        match crate::parse::decimal_u64(bytes) {
            crate::parse::Decimal64::Value(v, n) => Some((v, n)),
            _ => None,
        }
    }

    pub fn decimal_u64_scalar(bytes: &[u8]) -> Option<(u64, usize)> {
        match crate::parse::decimal_u64_scalar(bytes) {
            crate::parse::Decimal64::Value(v, n) => Some((v, n)),
            _ => None,
        }
    }

    pub fn next_special(bytes: &[u8]) -> usize {
        crate::simd::next_special(bytes, 0)
    }

    pub fn next_special_scalar(bytes: &[u8]) -> usize {
        crate::simd::next_special_scalar(bytes, 0)
    }
}

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
