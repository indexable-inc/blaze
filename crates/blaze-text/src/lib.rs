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
//! | [`Put`] | a sink of UTF-8: [`String<A>`](blaze_own::String), blaze-own's [`FixedString<N>`](blaze_own::FixedString) (fixed, refuses when full, never truncates), [`Count`] (only measures, for count-then-fill) |
//! | [`Writes`] | the chainable writers, on every `Put`: integers, hex, binary, floats, durations, byte counts, escapes, columns |
//! | [`Show`] | a type's human form, in place of `Display` for blaze's own types; [`Fixed`], [`Hex`], [`Nanos`], [`Bytes`], [`Quoted`] make a writer's form a value, for columns |
//! | [`Parse<'a>`] | a value and the rest of the text, the value free to borrow from it; combinators in [`parse`] |
//! | [`adapt`] | the one module that names `core::fmt`: [`Shown`] makes a `Show` a `Display`; [`PutDisplay::put_display`] writes a foreign `Display` through `fmt`, named so its cost shows |
//! | [`Writes::debug`], [`Dbg`], [`DebugForm`] | `{:?}` for any type, read by compile-time reflection, with no derive and no bound; a type supplies its own form by `DebugForm` |
//! | [`show_with`] | a `Show` from a writing closure, for what was `format_args!` |
//! | [`text_in`], [`text_exact_in`] | a string in an allocator from one chain; the exact form counts first and allocates once |
//!
//! # Format sites, translated
//!
//! | was | is |
//! |---|---|
//! | `format!("x = {x}")` | `text_in(alloc, \|out\| out.str("x = ").int(x))` |
//! | `format_args!("x = {x}")` | `show_with(\|out\| _ = out.str("x = ").int(x))` |
//! | `{x}` of a float | `.float(x)`: `{}`'s form (`1`, `0.1`, `1e300` in full) |
//! | `{x:?}` | `.debug(&x)` (`.float_debug(x)` for a float alone: `1.0`, `1e16`) |
//! | `println!(..)` | bstd's `out.line_with(cx, \|o\| o.str(..))?` |
//!
//! # An error type: `impl Show`, and `Shown` at the border
//!
//! An error's text is a [`Show`] impl: a chain into the caller's sink, no `fmt::Formatter`, no
//! `write!`. A foreign API that wants `Display` (`core::error::Error`, `{}` in code not yet
//! moved) gets one through [`Shown`], one line that writes nothing of its own. Its `Debug`
//! (for `unwrap` and `assert_eq!`) is the derived one, or a [`DebugForm`] bridged the same
//! way. blaze-toml's `Error`, before:
//!
//! ```text
//! impl fmt::Display for Error {
//!     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//!         write!(f, "{} at byte {}", self.what, self.at)
//!     }
//! }
//! ```
//!
//! and after:
//!
//! ```text
//! impl Show for Error {
//!     fn show<P: Put + ?Sized>(&self, out: &mut P) {
//!         out.show(&self.what).str(" at byte ").int(self.at);
//!     }
//! }
//!
//! /// The bridge `core::error::Error` needs: the text is `Show`'s.
//! impl fmt::Display for Error {
//!     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//!         fmt::Display::fmt(&Shown(self), f)
//!     }
//! }
//! ```
//!
//! A message for a person that needs more than the error (`line L, column C: what`) is a
//! method returning `Shown<impl Show>`: `{}` users and chains both take it.
//!
//! # Output is deterministic
//!
//! Every writer gives the same bytes for the same value on every machine (rule 25): no
//! locale, no hash order, no platform float formatting. Integers, hex, binary, floats in both
//! `{}` and `{:?}` form, fixed-place floats and the debug form of derived-`Debug` types are
//! byte-equal to `core::fmt`'s, which the tests hold.
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
//! per operation with the loop's own cost subtracted (the second run of the day):
//!
//! | operation | blaze-text | core | ratio |
//! |---|---|---|---|
//! | u64 decimal, 1 digit | 3.3 | 5.9 | 1.79x |
//! | u64 decimal, 5 digits | 5.2 | 6.9 | 1.33x |
//! | u64 decimal, 10 digits | 6.4 | 7.0 | 1.09x |
//! | u64 decimal, 20 digits | 11.0 | 10.1 | 0.92x (within noise) |
//! | u64 decimal, every length | 7.3 | 9.9 | 1.35x |
//! | u64 hex | 7.3 | 9.8 | 1.34x |
//! | f64 shortest | 34.5 | 51.3 | 1.49x |
//! | f64 fixed, 3 places | 6.3 | 20.2 | 3.2x |
//! | `"{}: {} of {} ({:.1}%)"` | 17.8 | 47.7 | 2.68x |
//! | u64 parse, 1 digit | 2.8 | 0.8 | 0.27x |
//! | u64 parse, 5 digits | 3.6 | 2.0 | 0.54x |
//! | u64 parse, 10 digits | 4.7 | 3.7 | 0.78x |
//! | u64 parse, 20 digits | 7.3 | 13.0 | 1.79x |
//! | u64 parse, every length | 5.9 | 5.6 | 0.95x |
//! | f64 `parse_all` | 14.0 | 12.8 | 0.91x |
//! | f64 parse, a prefix | 21.1 | 12.5 | 0.59x |
//! | next escape byte after 4 / 16 / 64 / 256 plain bytes, vector against scalar | | | 0.81x / 5.07x / 10.05x / 13.74x |
//!
//! The writers stand. The parses were changed after this run, not yet measured: `parse_all`
//! of an integer is `core`'s parse straight; a prefix parse reads its first eight digits by a
//! plain byte loop and only longer ones eight at a time by SWAR (the vector digit run lost at
//! every length and is gone); a float prefix finds its token's end in one pass over a
//! 256-entry table and hands a plain decimal to `core` with no scan of ours.

#![no_std]
// blaze's lints (`blaze::trust`, `blaze::sans_io`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
// `core::simd`: digit runs, escape scanning and hex encoding (`simd`).
#![feature(portable_simd)]
// The debug form read by reflection (`debug`): `type_info` to read a type and find its own
// form, `core_intrinsics` for `discriminant_value` and the reflection intrinsics
// `type_name`, `rustc_attrs` for `#[rustc_comptime]` on what calls reflection's
// compile-time-only functions, `const_heap` to keep its tables as exact-size slices,
// `ptr_metadata` to call a field's type through its vtable.
#![feature(type_info, core_intrinsics, rustc_attrs, const_heap, ptr_metadata)]
#![allow(
    internal_features,
    reason = "`core_intrinsics` and `rustc_attrs` for the reflection the debug form is read by (`debug`)"
)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![blaze::trust(
    unsafe,
    reason = "two places: ASCII bytes the crate wrote from its own digit tables viewed as `str` \
              (`ascii::Ascii::as_str`), and the debug walk, which reads a value through the form \
              its type's reflection gave (`debug`)"
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]

// The tests compare against `core::fmt` through std's `format!`.
#[cfg(test)]
extern crate std;

pub mod adapt;
mod ascii;
mod big;
// `debug_variants!` in textual scope for the modules below (the tests use it).
#[macro_use]
mod debug;
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
    pub fn next_special(bytes: &[u8]) -> usize {
        crate::simd::next_special(bytes, 0)
    }

    pub fn next_special_scalar(bytes: &[u8]) -> usize {
        crate::simd::next_special_scalar(bytes, 0)
    }
}

mod plant;
#[cfg(test)]
mod tests;

pub use adapt::{PutDisplay, Shown};
pub use plant::TextPlant;
pub use debug::DebugForm;
pub use float::Float;
pub use int::Integer;
pub use parse::{IntSyntax, Parse, ParseError, Unquoted, parse_all, parse_int, parse_quoted_in};
pub use put::{Count, Put, text_exact_in, text_in};
pub use show::{Bytes, Dbg, Fixed, Hex, Nanos, Quoted, Show, ShowWith, show_with};
pub use write::Writes;
