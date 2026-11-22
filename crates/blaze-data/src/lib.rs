//! The one format-neutral value: what blaze's formats read into and write from.
//!
//! A [`Value`] is not JSON, CBOR or TOML: each format is a reader and a writer over it
//! (`blaze-json`, `blaze-toml`, and blaze-pack's deterministic CBOR for the agent API, rule 33).
//! It holds what all three can say, and each writer refuses, by a typed error naming the path,
//! what its format cannot (JSON has no bytes and no non-finite float, TOML no null):
//!
//! | variant | holds |
//! |---|---|
//! | `Null` | nothing |
//! | `Bool` | `bool` |
//! | `Int` | an [`Int`]: any `u64` or any `i64`, exactly, never through a float |
//! | `Float` | `f64`, any bits; equality is by bits, so `NaN == NaN` and `0.0 != -0.0` |
//! | `Text` | UTF-8 [`String`] |
//! | `Bytes` | [`Vec<u8>`](Vec) |
//! | `List` | values in order |
//! | `Map` | a [`Map`]: text keys, unique, in insertion order, with a sorted view |
//!
//! Reading a value into a type is [`FromValue`] over a [`Cursor`], whose errors name the path
//! (`a.b[3]: expected text, found integer`); writing a type into one is [`ToValue`]. Both are
//! written by hand today. They are shaped for docs/v2/plan/design/std.md's `bstd::data`, where
//! `Encode`/`Decode` come from a type's fields by reflection: a struct's `from_value` is one
//! [`Cursor::fields`] with one `take(name)` per field, and its `to_value` one `Map::insert` per
//! field, which is exactly what a field walk generates and what [`fields!`] writes today.
//! Reflection will implement these traits, not replace them.
//!
//! Reading is strict by default: a key no field takes, a kind read as another, an integer
//! outside its type and a name outside an enum's variants are each refused with the path;
//! opting out is a call named for it ([`Cursor::fields_allowing_extra`],
//! [`Cursor::f64_or_int`]). Every reader refuses a duplicate key, bytes after the value and
//! text that is not UTF-8.
//!
//! Nothing here recurses on a value's depth: [`Walk`] and [`Build`] keep their open containers
//! on a stack in a given allocator, every format's reader and writer goes through them, and
//! `Value`'s drop moves descendants into the dropping container's own storage. So a value at
//! any depth a reader accepts is read, written, copied and dropped in a small stack. `Clone`,
//! `==` and `Debug` still recurse (they have no allocator for a stack); they are for values a
//! program built, not ones read from untrusted bytes.
//!
//! [`value!`] builds a value from JSON-like syntax (`value!(in alloc; {"a": [1, x, null]})`),
//! taking the allocator first as everything here does.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// Each core module is marked `#![blaze::sans_io]`, not the crate: a tool attribute is an
// expansion, and one on the crate root makes the `#[macro_export]` macros macro-expanded, which
// the crate itself then cannot name by `$crate::` path (the macros module stays unmarked; it
// holds no code).
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

// First, and textually scoped, so the crate's own modules call its macros by name.
#[macro_use]
mod macros;
mod map;
mod path;
mod plant;
mod traits;
#[cfg(feature = "std")]
#[allow(blaze::std_use, reason = "the `std` feature alone: impls for std's `String`, `Vec` and `PathBuf`")]
mod std_types;
mod value;
mod walk;

// The `std` feature's impls name std's types; the tests' models are std's maps and vectors.
#[cfg(any(feature = "std", test))]
extern crate std;

#[cfg(test)]
mod tests;

pub use map::{Map, Taken};
pub use plant::DataPlant;
pub use path::{At, Cursor, Extra, Fields, Shape, ShapeError, Step, write_step};
pub use traits::{FromValue, ToValue};
pub use value::{Int, Kind, Value};
pub use walk::{Build, Order, Visit, Walk};

/// What `value!` expands to; not an interface.
#[doc(hidden)]
pub mod __private {
    use core::alloc::Allocator;

    pub use blaze_own::{String, Vec};

    use crate::{Map, Value};

    /// `value` as a value of `alloc`'s type.
    #[inline(always)]
    pub fn of<A: Allocator>(_alloc: &A, value: Value<A>) -> Value<A> {
        value
    }

    /// `key` into `map`, a later one replacing an earlier one as a literal's would.
    pub fn insert<A: Allocator + Clone, K: AsRef<str> + ?Sized>(
        map: &mut Map<A>,
        key: &K,
        value: Value<A>,
        alloc: &A,
    ) {
        map.insert(blaze_own::String::from_str_in(key.as_ref(), alloc.clone()), value);
    }
}
