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
//! `cursor.field(name)` per field and one `deny_unknown` of the field names, and its `to_value`
//! one `Map::insert` per field, which is exactly what a field walk generates. Reflection will
//! implement these traits, not replace them.
//!
//! [`value!`] builds a value from JSON-like syntax (`value!(in alloc; {"a": [1, x, null]})`),
//! taking the allocator first as everything here does.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

mod macros;
mod map;
mod path;
mod traits;
mod value;

#[cfg(test)]
mod plant;
#[cfg(test)]
mod tests;

/// The planted bug the map's control asks for, compiled away outside tests.
#[cfg(not(test))]
mod plant {
    #[inline(always)]
    pub(crate) fn replace_on_hit() -> bool {
        true
    }
}

pub use map::{Map, Taken};
pub use path::{At, Cursor, Shape, ShapeError, Step};
pub use traits::{FromValue, ToValue};
pub use value::{Int, Kind, Value};

/// What `value!` expands to; not an interface.
#[doc(hidden)]
pub mod __private {
    use core::alloc::Allocator;

    pub use blaze_own::Vec;

    use crate::{Map, Value};

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
