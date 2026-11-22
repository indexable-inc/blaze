//! The size and alignment of a `repr(simd)` vector from its lane count and element size
//! ([`vector_shape`]); `Layouts::simd` (layout/simd.rs) lays the vector out with them.
//!
//! The Reference has no section on `repr(simd)`; the rule is the pinned compiler's
//! (rustc 1.101.0-nightly 16d1ffac1, 2026-10-26, the `nightly-2026-10-27` toolchain), read
//! from `compiler/rustc_abi/src/layout.rs` at that commit, `vector_type_layout`, lines
//! 1500-1517:
//!
//! ```text
//! let size = elt.size.checked_mul(count.as_u64(), dl)...;
//! let (repr, size, align) = match kind {
//!     // Non-power-of-two vectors have padding up to the next power-of-two.
//!     // If we're a packed repr, remove the padding while keeping the alignment as close
//!     // to a vector as possible.
//!     SimdVectorKind::PackedFixed if !count.is_power_of_two() => {
//!         (BackendRepr::Memory { sized: true }, size, Align::max_aligned_factor(size))
//!     }
//!     SimdVectorKind::PackedFixed | SimdVectorKind::Fixed => {
//!         (BackendRepr::SimdVector { element, count }, size, dl.rust_vector_align(size))
//!     }
//! };
//! let size = size.align_to(align);
//! ```
//!
//! with `rustc_abi/src/lib.rs:737-740`: `rust_vector_align(vec_size) =
//! self.c_vector_align(vec_size).unwrap_or(Align::from_bytes(vec_size.bytes()
//! .next_power_of_two()))`, and `lib.rs:1208-1209`: `max_aligned_factor(size) = Align { pow2:
//! size.bytes().trailing_zeros() }`. The pinned library agrees:
//! `library/stdarch/crates/core_arch/src/macros.rs` says "repr(simd) adds padding to make the
//! total size a power of two", and `library/portable-simd/crates/core_simd/src/vector.rs`
//! calls `Simd<T, N>` (declared `#[repr(simd, packed)]`) "just an overaligned `[T; N]` with
//! potential padding at the end".
//!
//! The target's table: `c_vector_align` reads the data layout's `v<size>` entries, whose
//! defaults (`lib.rs:372-375`) are 64 bits at 64 and 128 bits at 128. The aarch64-apple-darwin
//! data layout (`compiler/rustc_target/src/spec/targets/aarch64_apple_darwin.rs:15`, `"e-m:o-
//! p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-n32:64-S128-Fn32"`) adds or changes no
//! `v` entry, and both defaults equal `next_power_of_two` of their size, so on this target a
//! vector's alignment is its unpadded size rounded up to a power of two, with NO cap: a
//! 32-byte vector is 32-aligned and a 64-byte one 64-aligned. (AAPCS64's 8- and 16-byte short
//! vectors are two instances of it.)
//!
//! So, for `N` lanes of `s` bytes, `bytes = N * s`:
//! - `N` a power of two, or not packed: alignment `bytes.next_power_of_two()`, size `bytes`
//!   rounded up to it (three `f32` lanes: 16 and 16);
//! - packed and `N` not a power of two: size `bytes` (no padding), alignment the largest power
//!   of two dividing `bytes` (three `f32` lanes: 12 and 4). That is never below the element's
//!   own alignment, which divides `s`.
//!
//! Lane count: rustc refuses 0 lanes (`BackendLaneCount::new`, `lib.rs:273-282`) and more than
//! `MAX_SIMD_LANES` (`lib.rs:265`, `1 << 0xF`); core_simd's `Simd` adds
//! `#[rustc_simd_monomorphize_lane_limit = "64"]`. blaze lays out 1 to 64 lanes and refuses
//! any other count by name ([`SimdShapeRefusal::LaneCount`]): no vector in the pinned library
//! or in tempo's dependencies on aarch64 has more than 64 lanes, and a refusal by name builds
//! nothing wrong.

use core::fmt;

use super::Refusal;

/// The most lanes blaze lays out: core_simd's `rustc_simd_monomorphize_lane_limit = "64"`.
pub(super) const MAX_LANES: u64 = 64;

/// The largest alignment a type may have, in bytes: `rustc_abi/src/lib.rs:1138-1139`, "LLVM
/// has a maximal supported alignment of 2^29, we inherit that." `pub const MAX: Align = Align
/// { pow2: 29 };`. The `max_align` `Layouts::simd` passes to [`vector_shape`].
pub(super) const MAX_ALIGN: u64 = 1 << 29;

/// Why a vector has no shape: the count, element size or result that refused it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimdShapeRefusal {
    /// The lane count is 0 or above [`MAX_LANES`].
    LaneCount { count: u64 },
    /// The element is zero-sized (a vector's element is a scalar: never zero bytes).
    ZeroSizedElement,
    /// `count * elem_size` (or its rounding up to a power of two) overflows a `u64`.
    TooLarge { count: u64, elem_size: u64 },
    /// The rule's alignment is above the `max_align` the caller lays out.
    AlignAboveMax { align: u64, max_align: u64 },
}

impl fmt::Display for SimdShapeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SimdShapeRefusal::LaneCount { count } => {
                write!(f, "a `repr(simd)` type of {count} lanes (blaze lays out 1 to {MAX_LANES}: core_simd's `rustc_simd_monomorphize_lane_limit`)")
            }
            SimdShapeRefusal::ZeroSizedElement => write!(f, "a `repr(simd)` type whose element is zero-sized"),
            SimdShapeRefusal::TooLarge { count, elem_size } => {
                write!(f, "a `repr(simd)` type of {count} lanes of {elem_size} bytes: its size overflows")
            }
            SimdShapeRefusal::AlignAboveMax { align, max_align } => {
                write!(f, "a `repr(simd)` type aligned to {align} bytes, above the most ({max_align})")
            }
        }
    }
}

impl SimdShapeRefusal {
    /// The layout's refusal for it (`Refusal` holds a static text; the count is in
    /// [`SimdShapeRefusal`]'s own `Display`).
    pub(super) fn refusal(self) -> Refusal {
        match self {
            SimdShapeRefusal::LaneCount { count: 0 } => Refusal::Malformed("a `repr(simd)` type of zero lanes (rustc_abi `BackendLaneCount::new`: `ZeroLengthSimdType`)"),
            SimdShapeRefusal::LaneCount { .. } => {
                Refusal::Unsupported("a `repr(simd)` type of more than 64 lanes (core_simd's `rustc_simd_monomorphize_lane_limit`)")
            }
            SimdShapeRefusal::ZeroSizedElement => Refusal::Malformed("a `repr(simd)` type whose element is zero-sized"),
            SimdShapeRefusal::TooLarge { .. } => Refusal::TooLarge,
            SimdShapeRefusal::AlignAboveMax { .. } => Refusal::Unsupported("a `repr(simd)` type aligned above 2^29 bytes (rustc_abi `Align::MAX`)"),
        }
    }
}

/// The size and the alignment, both in bytes, of a `repr(simd)` vector of `count` lanes of
/// `elem_size` bytes each, `packed` when it is `repr(simd, packed)`; an alignment above
/// `max_align` (bytes) is refused. The rule is the module's (rustc_abi `vector_type_layout`).
pub(super) fn vector_shape(count: u64, elem_size: u64, packed: bool, max_align: u64) -> Result<(u64 /*size*/, u64 /*align*/), SimdShapeRefusal> {
    if count == 0 || count > MAX_LANES {
        return Err(SimdShapeRefusal::LaneCount { count });
    }
    if elem_size == 0 {
        return Err(SimdShapeRefusal::ZeroSizedElement);
    }
    let too_large = SimdShapeRefusal::TooLarge { count, elem_size };
    let bytes = count.checked_mul(elem_size).ok_or(too_large)?;
    let (size, align) = if packed && !count.is_power_of_two() {
        // `Align::max_aligned_factor(size)`: the largest power of two dividing `bytes`
        // (`bytes` is not 0: both factors are not), and `bytes` is a multiple of it.
        (bytes, 1u64 << bytes.trailing_zeros())
    } else {
        // `rust_vector_align(size)`: `next_power_of_two`; then `size.align_to(align)`, which
        // for a power-of-two `align` at least `bytes` is `align` itself.
        let align = bytes.checked_next_power_of_two().ok_or(too_large)?;
        (align, align)
    };
    if align > max_align {
        return Err(SimdShapeRefusal::AlignAboveMax { align, max_align });
    }
    Ok((size, align))
}

#[cfg(test)]
mod tests {
    //! The rule of the module's doc: rustc_abi `vector_type_layout` at the pinned commit
    //! (`layout.rs:1500-1517`), with `rust_vector_align` (`lib.rs:737-740`) and
    //! `max_aligned_factor` (`lib.rs:1208-1209`); the lane limit of core_simd
    //! (`rustc_simd_monomorphize_lane_limit = "64"`).
    //!
    //! Expected: `test layout::simd_rule::tests::packed_and_padded_shapes ... ok`.
    //! A fix that pads a packed non-power-of-two vector fails on `(3, 4, true)`; one that keeps
    //! the element's alignment fails on `(16, 1, true)`; one that caps the alignment at 16
    //! fails on `(32, 1, false)` and `(48, 16, true)`; one that lays out any lane count fails
    //! on 0 and 65.

    use super::{MAX_ALIGN, SimdShapeRefusal, vector_shape};

    #[test]
    fn packed_and_padded_shapes() {
        // `Simd<u8, 16>` (packed, a power of two): as unpacked, 16 bytes at 16.
        assert_eq!(vector_shape(16, 1, true, MAX_ALIGN), Ok((16, 16)));
        // `Simd<f32, 3>` (packed): `[f32; 3]`'s 12 bytes, at 4 (largest power of two dividing 12).
        assert_eq!(vector_shape(3, 4, true, MAX_ALIGN), Ok((12, 4)));
        // Three `f32` lanes, not packed: padded to 16, at 16.
        assert_eq!(vector_shape(3, 4, false, MAX_ALIGN), Ok((16, 16)));
        // 32 `u8` lanes: `rust_vector_align(32)`, no `v256` entry on aarch64-apple-darwin, so
        // `next_power_of_two(32)` = 32. (The planner's prompt said (32, 16): rustc has no cap.)
        assert_eq!(vector_shape(32, 1, false, MAX_ALIGN), Ok((32, 32)));
        // 64 `u8` lanes, the lane limit itself: 64 at 64.
        assert_eq!(vector_shape(64, 1, false, MAX_ALIGN), Ok((64, 64)));
        // `Simd<u64, 3>` (packed): 24 bytes at 8.
        assert_eq!(vector_shape(3, 8, true, MAX_ALIGN), Ok((24, 8)));
        // 48 `u128` lanes, packed: 768 bytes, at 256 (768 = 3 * 256): `max_aligned_factor`
        // has no cap.
        assert_eq!(vector_shape(48, 16, true, MAX_ALIGN), Ok((768, 256)));
        // 65 lanes and 0 lanes: refused by name.
        assert_eq!(vector_shape(65, 1, false, MAX_ALIGN), Err(SimdShapeRefusal::LaneCount { count: 65 }));
        assert_eq!(vector_shape(0, 4, true, MAX_ALIGN), Err(SimdShapeRefusal::LaneCount { count: 0 }));
        // A caller's lower ceiling refuses, never clamps.
        assert_eq!(vector_shape(32, 1, false, 16), Err(SimdShapeRefusal::AlignAboveMax { align: 32, max_align: 16 }));
    }
}
