//! Compile-time enum reflection traits and `const fn` introspection primitives.
//!
//! - [`Enum`] and [`UnitEnum`] are compiler-builtin marker traits automatically satisfied by
//!   every `enum` (and every fieldless `enum` for [`UnitEnum`]) under Blaze, and provably
//!   disjoint from structs and primitives during coherence.
//! - [`EnumMeta`] gives every enum zero-cost `as_str`, `as_lowercase_str`, `as_snake_str`,
//!   `variant_index`, and `variant_count` methods (without moving non-`Copy` payloads).
//! - [`FromVariantStr`] parses any fieldless enum by its exact Rust variant name (always
//!   unambiguous because Rust forbids duplicate variant identifiers).
//! - [`FromLowercaseStr`] and [`FromSnakeStr`] parse fieldless enums by ASCII-lowercase or
//!   `snake_case` variant name, gated by `where const { has_unique_lowercase_variants::<E>() }`
//!   and `where const { has_unique_snake_variants::<E>() }` so case-colliding enums (such as
//!   `enum Weird { SomeEnum, someenum }` or `enum Ambig { FooBar, foo_bar }`) fail to compile
//!   instead of silently shadowing a variant.
//!
//! Under `rustc` (`cfg(not(blaze))`), the traits and `const fn` helpers compile cleanly as plain
//! Rust ("Two builders"), while the compiler-backed `#[rustc_intrinsic]` declarations and
//! `inline for` / `where const` blanket impls live in `reflect/extended.rs` (`cfg(blaze)`).

#[cfg(blaze)]
mod extended;

/// Compiler-builtin marker trait satisfied by every `enum` type (unit or payload, including
/// uninhabited enums). Provably disjoint from structs, unions, and primitives.
///
/// Under Blaze (`cfg(blaze)`), this is `core::reflect::Enum` (`#[lang = "blaze_enum"]`).
#[cfg(blaze)]
pub use core::reflect::Enum;

/// Compiler-builtin marker trait satisfied by every `enum` type (unit or payload, including
/// uninhabited enums). Plain Rust stub under `rustc` (`cfg(not(blaze))`).
#[cfg(not(blaze))]
pub trait Enum {}

/// Compiler-builtin marker trait satisfied by every fieldless (`CtorKind::Unit`) `enum` type.
///
/// Under Blaze (`cfg(blaze)`), this is `core::reflect::UnitEnum` (`#[lang = "blaze_unit_enum"]`).
#[cfg(blaze)]
pub use core::reflect::UnitEnum;

/// Compiler-builtin marker trait satisfied by every fieldless (`CtorKind::Unit`) `enum` type.
/// Plain Rust stub under `rustc` (`cfg(not(blaze))`).
#[cfg(not(blaze))]
pub trait UnitEnum: Enum {}

/// Zero-cost compile-time metadata reflection for every enum (`E: Enum`).
pub trait EnumMeta: Enum {
    /// Exact Rust declaration name of the active variant (with any raw `r#` prefix stripped).
    fn as_str(&self) -> &'static str;
    /// ASCII-lowercased name of the active variant.
    fn as_lowercase_str(&self) -> &'static str;
    /// `snake_case` name of the active variant.
    fn as_snake_str(&self) -> &'static str;
    /// Zero-based declaration index of the active variant.
    fn variant_index(&self) -> usize;
    /// Total number of declared variants on `Self`.
    fn variant_count() -> usize;
}

/// Exact variant-name parsing for every fieldless enum (`E: UnitEnum`).
pub trait FromVariantStr: UnitEnum + Sized {
    /// Parses `s` against each variant's exact declaration name (with any `r#` prefix stripped).
    fn from_variant_str(s: &str) -> Option<Self>;
}

/// ASCII-lowercase variant-name parsing for fieldless enums with pairwise-distinct lowercase
/// variant names (`where const { has_unique_lowercase_variants::<Self>() }`).
pub trait FromLowercaseStr: UnitEnum + Sized {
    /// Parses `s` against each variant's ASCII-lowercased name.
    fn from_lowercase_str(s: &str) -> Option<Self>;
}

/// `snake_case` variant-name parsing for fieldless enums with pairwise-distinct `snake_case`
/// variant names (`where const { has_unique_snake_variants::<Self>() }`).
pub trait FromSnakeStr: UnitEnum + Sized {
    /// Parses `s` against each variant's `snake_case` name.
    fn from_snake_str(s: &str) -> Option<Self>;
}

/// Returns `true` if `T` is an `enum` type.
pub const fn is_enum<T: ?Sized>() -> bool {
    #[cfg(blaze)]
    {
        extended::blaze_is_enum::<T>()
    }
    #[cfg(not(blaze))]
    {
        false
    }
}

/// Returns `true` if `T` is a fieldless (`UnitEnum`) `enum` type.
pub const fn is_unit_enum<T: ?Sized>() -> bool {
    #[cfg(blaze)]
    {
        extended::blaze_is_unit_enum::<T>()
    }
    #[cfg(not(blaze))]
    {
        false
    }
}

/// Returns the number of declared variants of `T` (`0` for non-enums and uninhabited enums).
pub const fn variant_count<T: ?Sized>() -> usize {
    #[cfg(blaze)]
    {
        extended::blaze_variant_count::<T>()
    }
    #[cfg(not(blaze))]
    {
        0
    }
}

/// Returns the declared name of variant `index` of enum `T` (with any `r#` prefix stripped), or
/// `""` if `T` is not an enum or `index >= variant_count::<T>()`.
pub const fn variant_name<T: ?Sized>(index: usize) -> &'static str {
    #[cfg(blaze)]
    {
        extended::blaze_variant_name::<T>(index)
    }
    #[cfg(not(blaze))]
    {
        let _ = index;
        ""
    }
}

#[inline(always)]
const fn is_ascii_upper(b: u8) -> bool {
    b >= b'A' && b <= b'Z'
}

#[inline(always)]
const fn is_ascii_lower(b: u8) -> bool {
    b >= b'a' && b <= b'z'
}

#[inline(always)]
const fn is_ascii_digit(b: u8) -> bool {
    b >= b'0' && b <= b'9'
}

#[inline(always)]
const fn to_ascii_lower(b: u8) -> u8 {
    if is_ascii_upper(b) { b + 32 } else { b }
}

/// Streaming byte-level equality of `a` and `b` after ASCII lowercasing (UTF-8 safe).
pub const fn ascii_lower_eq(a: &str, b: &str) -> bool {
    let mut ab = a.as_bytes();
    let mut bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    while let (Some((&a0, a_rest)), Some((&b0, b_rest))) = (ab.split_first(), bb.split_first()) {
        if to_ascii_lower(a0) != to_ascii_lower(b0) {
            return false;
        }
        ab = a_rest;
        bb = b_rest;
    }
    true
}

/// Yields the next byte of `rest` converted to `snake_case` using the exact same boundary rule
/// as Blaze's `inline for` `snake` descriptor field (UTF-8 safe, zero allocation, zero indexing).
#[inline(always)]
const fn next_snake_byte(
    rest: &[u8],
    prev: Option<u8>,
    emitted_sep: bool,
) -> Option<(u8, &[u8], Option<u8>, bool)> {
    let Some((&b, tail)) = rest.split_first() else {
        return None;
    };
    if is_ascii_upper(b) {
        if let Some(p) = prev
            && !emitted_sep
        {
            let next = match tail.first() {
                Some(&n) => n,
                None => 0,
            };
            let boundary = p != b'_'
                && (is_ascii_lower(p)
                    || is_ascii_digit(p)
                    || p >= 0x80
                    || (is_ascii_upper(p) && is_ascii_lower(next)));
            if boundary {
                return Some((b'_', rest, prev, true));
            }
        }
        Some((b + 32, tail, Some(b), false))
    } else {
        Some((b, tail, Some(b), false))
    }
}

/// Streaming byte-level equality of `a` and `b` after `snake_case` conversion (UTF-8 safe).
pub const fn snake_eq(a: &str, b: &str) -> bool {
    let mut a_rest = a.as_bytes();
    let mut a_prev: Option<u8> = None;
    let mut a_sep = false;
    let mut b_rest = b.as_bytes();
    let mut b_prev: Option<u8> = None;
    let mut b_sep = false;
    loop {
        match (
            next_snake_byte(a_rest, a_prev, a_sep),
            next_snake_byte(b_rest, b_prev, b_sep),
        ) {
            (None, None) => return true,
            (Some((ac, na, npa, nas)), Some((bc, nb, npb, nbs))) => {
                if ac != bc {
                    return false;
                }
                a_rest = na;
                a_prev = npa;
                a_sep = nas;
                b_rest = nb;
                b_prev = npb;
                b_sep = nbs;
            }
            _ => return false,
        }
    }
}

/// Returns `true` iff `E` is an enum whose declared variant names are pairwise distinct after
/// ASCII lowercasing.
pub const fn has_unique_lowercase_variants<E: ?Sized>() -> bool {
    if !is_enum::<E>() {
        return false;
    }
    let n = variant_count::<E>();
    let mut i = 0;
    while i < n {
        let a = variant_name::<E>(i);
        let mut j = i + 1;
        while j < n {
            if ascii_lower_eq(a, variant_name::<E>(j)) {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

/// Returns `true` iff `E` is an enum whose declared variant names are pairwise distinct after
/// `snake_case` conversion.
pub const fn has_unique_snake_variants<E: ?Sized>() -> bool {
    if !is_enum::<E>() {
        return false;
    }
    let n = variant_count::<E>();
    let mut i = 0;
    while i < n {
        let a = variant_name::<E>(i);
        let mut j = i + 1;
        while j < n {
            if snake_eq(a, variant_name::<E>(j)) {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{ascii_lower_eq, snake_eq};

    #[test]
    fn ascii_lower_eq_detects_case_collisions() {
        assert!(ascii_lower_eq("SomeEnum", "someenum"));
        assert!(ascii_lower_eq("HTTPServer", "httpserver"));
        assert!(!ascii_lower_eq("Inspector", "Graph"));
        assert!(ascii_lower_eq("Café", "café"));
        assert!(!ascii_lower_eq("Café", "cafe"));
    }

    #[test]
    fn snake_eq_detects_snake_collisions_and_preserves_utf8() {
        assert!(snake_eq("FooBar", "foo_bar"));
        assert!(snake_eq("OnDeleteCascade", "on_delete_cascade"));
        assert!(snake_eq("HTTPServer", "http_server"));
        assert!(snake_eq("F32", "f32"));
        assert!(snake_eq("A1B", "a1_b"));
        assert!(snake_eq("A_B", "a_b"));
        assert!(!snake_eq("FooBar", "foobar"));
        assert!(snake_eq("CaféItem", "café_item"));
        assert!(!snake_eq("CaféItem", "cafe_item"));
    }
}
