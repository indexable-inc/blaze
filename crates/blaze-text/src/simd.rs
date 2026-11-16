//! The vector routines, in `core::simd` (docs/v2/plan/design/std.md, "SIMD"), each with its
//! scalar twin; the tests hold the two equal on random input at every alignment.
//!
//! - [`digit_run`]: how many ASCII digits start a 16-byte window (one compare, one bitmask).
//! - [`next_special`]: the next byte an escape rewrites (`"`, `\`, below 0x20, DEL).
//! - [`hex16`]: sixteen bytes as 32 hex digits by a table lookup (`swizzle_dyn`).

use core::simd::Simd;
use core::simd::cmp::{SimdPartialEq, SimdPartialOrd};

type V16 = Simd<u8, 16>;

#[inline(always)]
fn load16(bytes: &[u8]) -> V16 {
    V16::from_slice(&bytes[..16])
}

/// How many of the first 16 bytes of `bytes` (at least 16 of them) are ASCII digits, in a run
/// from the first.
#[inline(always)]
pub(crate) fn digit_run(bytes: &[u8]) -> u32 {
    let digits = (load16(bytes) - V16::splat(b'0')).simd_lt(V16::splat(10));
    digits.to_bitmask().trailing_ones()
}

/// The scalar twin of [`digit_run`].
#[cfg(test)]
pub(crate) fn digit_run_scalar(bytes: &[u8]) -> u32 {
    let run = bytes[..16].iter().take_while(|b| b.is_ascii_digit()).count();
    blaze_int::Int::narrow_or_stop::<u32>(run)
}

/// The 16-byte window's escape bits: bit `i` set when byte `i` is `"`, `\`, below 0x20 or DEL.
#[inline(always)]
fn special16(bytes: &[u8]) -> u64 {
    let v = load16(bytes);
    let special = v.simd_lt(V16::splat(0x20))
        | v.simd_eq(V16::splat(b'"'))
        | v.simd_eq(V16::splat(b'\\'))
        | v.simd_eq(V16::splat(0x7F));
    special.to_bitmask()
}

#[inline(always)]
pub(crate) const fn is_special(b: u8) -> bool {
    b < 0x20 || b == b'"' || b == b'\\' || b == 0x7F
}

/// The index of the first escape byte in `bytes[from..]`, or `bytes.len()`: sixteen at a time,
/// then byte by byte for the tail.
#[inline]
pub(crate) fn next_special(bytes: &[u8], mut from: usize) -> usize {
    while from + 16 <= bytes.len() {
        let mask = special16(&bytes[from..]);
        if mask != 0 {
            return from + blaze_int::idx(mask.trailing_zeros());
        }
        from += 16;
    }
    next_special_scalar(bytes, from)
}

/// The scalar twin of [`next_special`].
#[inline]
pub(crate) fn next_special_scalar(bytes: &[u8], from: usize) -> usize {
    bytes[from..].iter().position(|&b| is_special(b)).map_or(bytes.len(), |at| from + at)
}

/// Sixteen bytes as 32 lowercase hex digits, high nibble first.
#[inline(always)]
pub(crate) fn hex16(bytes: &[u8]) -> [u8; 32] {
    let table = V16::from_array(*b"0123456789abcdef");
    let v = load16(bytes);
    let high = table.swizzle_dyn(v >> V16::splat(4));
    let low = table.swizzle_dyn(v & V16::splat(15));
    let (first, second) = high.interleave(low);
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(first.as_array());
    out[16..].copy_from_slice(second.as_array());
    out
}

/// The scalar twin of [`hex16`].
#[cfg(test)]
pub(crate) fn hex16_scalar(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, &b) in bytes[..16].iter().enumerate() {
        out[2 * i] = crate::ascii::LOWER[usize::from(b >> 4)];
        out[2 * i + 1] = crate::ascii::LOWER[usize::from(b & 15)];
    }
    out
}
