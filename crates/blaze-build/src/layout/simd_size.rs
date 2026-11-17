//! The size and alignment of a `repr(simd)` vector from its lane count and element size
//! ([`vector_size_align`]); `Layouts::simd` (layout/simd.rs) lays the vector out with them.
//!
//! The Reference (1.88) has no section on `repr(simd)`; the pinned library is the evidence.
//! `library/portable-simd/crates/core_simd/src/vector.rs:51`: "`Simd<T, N>` has a layout
//! similar to `[T; N]` (identical "shapes"), with a greater alignment. `[T; N]` is aligned to
//! `T`, but `Simd<T, N>` will have an alignment based on both `T` and `N`." Line 56: "`N`
//! cannot be 0 and may be at most 64." Line 106 declares it `#[repr(simd, packed)]` with
//! `#[rustc_simd_monomorphize_lane_limit = "64"]`, and line 168 (`as_array`): "`Simd<T, N>` is
//! just an overaligned `[T; N]` with potential padding at the end".
//!
//! The rule, for `N` lanes of `s` bytes each, `bytes = N * s`:
//! - an unpacked vector, or a packed one whose `N` is a power of two: its size is `bytes`
//!   rounded up to a power of two, and its alignment that size (the natural vector alignment;
//!   AAPCS64's 8- and 16-byte short vectors are two instances of it);
//! - a packed vector whose `N` is not a power of two: its size is `bytes` (no padding: the
//!   "identical shape" of `[T; N]`), its alignment the largest power of two dividing `bytes`,
//!   at most 16. That is never below the element's own alignment, which divides `s`.
//! - `N` is 1 to 64; zero lanes is a malformed type, more than 64 is refused by name.

use super::Refusal;

/// The most lanes a vector may have: core_simd's `rustc_simd_monomorphize_lane_limit`.
const MAX_LANES: u64 = 64;

/// log2 of the most alignment a packed vector with a non-power-of-two lane count takes.
const PACKED_MAX_ALIGN_LOG2: u32 = 4;

/// The size in bytes and log2 of the alignment of a `repr(simd)` vector of `lanes` lanes of
/// `elem_size` bytes, `packed` when it is `repr(simd, packed)`; the rule is the module's.
pub(super) fn vector_size_align(lanes: u64, elem_size: u64, packed: bool) -> Result<(u64, u8), Refusal> {
    if lanes == 0 {
        return Err(Refusal::Malformed("a `repr(simd)` type of zero lanes (core_simd: \"`N` cannot be 0\")"));
    }
    if lanes > MAX_LANES {
        return Err(Refusal::Unsupported("a `repr(simd)` type of more than 64 lanes (core_simd's `rustc_simd_monomorphize_lane_limit`)"));
    }
    if elem_size == 0 {
        return Err(Refusal::Malformed("a `repr(simd)` type whose element is zero-sized"));
    }
    let bytes = lanes.checked_mul(elem_size).ok_or(Refusal::TooLarge)?;
    if packed && !lanes.is_power_of_two() {
        let align = bytes.trailing_zeros().min(PACKED_MAX_ALIGN_LOG2) as u8;
        return Ok((bytes, align));
    }
    let size = bytes.checked_next_power_of_two().ok_or(Refusal::TooLarge)?;
    Ok((size, size.trailing_zeros() as u8))
}

#[cfg(test)]
mod tests {
    //! The rule of the module's doc (core_simd vector.rs:51, :56, :106-107, :168).
    //!
    //! Expected: `test layout::simd_size::tests::vectors_take_the_rule_s_size_and_alignment ... ok`
    //! (one test, eight asserts). A fix that pads a packed non-power-of-two vector fails on
    //! `(3, 4, true)`; one that keeps the element's alignment fails on `(16, 1, false)`; one that
    //! lays out any lane count fails on `(65, 1, false)`.

    use super::{Refusal, vector_size_align};

    #[test]
    fn vectors_take_the_rule_s_size_and_alignment() {
        // `u8x16`: 16 bytes, aligned to 16 (log2 4), not to its `u8`.
        // (`Refusal` has no `PartialEq`: an `Ok` is compared through `.ok()`.)
        assert_eq!(vector_size_align(16, 1, false).ok(), Some((16, 4)));
        // `Simd<u32, 4>` (packed, four lanes: a power of two): as unpacked.
        assert_eq!(vector_size_align(4, 4, true).ok(), Some((16, 4)));
        // `Simd<u32, 3>` (packed, three lanes): `[u32; 3]`'s 12 bytes, aligned to 4.
        assert_eq!(vector_size_align(3, 4, true).ok(), Some((12, 2)));
        // An unpacked vector of three `u32`: padded to 16, aligned to 16.
        assert_eq!(vector_size_align(3, 4, false).ok(), Some((16, 4)));
        // `Simd<u8, 64>`: the lane limit itself, 64 bytes aligned to 64.
        assert_eq!(vector_size_align(64, 1, false).ok(), Some((64, 6)));
        // `Simd<u64, 3>` (packed): 24 bytes, aligned to 8 (the largest power of two dividing 24).
        assert_eq!(vector_size_align(3, 8, true).ok(), Some((24, 3)));
        // 65 lanes: past `rustc_simd_monomorphize_lane_limit = "64"`, refused by name.
        assert!(matches!(vector_size_align(65, 1, false), Err(Refusal::Unsupported(what)) if what.contains("more than 64 lanes")));
        // Zero lanes: "`N` cannot be 0".
        assert!(matches!(vector_size_align(0, 4, true), Err(Refusal::Malformed(_))));
    }
}
