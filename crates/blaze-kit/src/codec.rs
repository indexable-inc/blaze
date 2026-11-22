//! Integer column codecs blaze-parquet lacks: zigzag, unsigned LEB128 and a delta column on
//! them, for sorted or slowly changing integers (positions, ids, times in a profile).
//!
//! Bit-packing and Parquet's RLE/bit-packed hybrid are not here: their one home is
//! blaze-parquet (`src/levels.rs`), which writes them to the spec for its pages. The varint and
//! zigzag of blaze-parquet's Thrift writer (`src/thrift.rs`) are the same functions as these and
//! can move here.
//!
//! # Formats
//!
//! - Zigzag maps `0, -1, 1, -2, ..` to `0, 1, 2, 3, ..`, so a small magnitude of either sign is
//!   a small unsigned number.
//! - LEB128 writes 7 bits a byte, least significant first, the high bit set on every byte but
//!   the last: 1 byte below 128, at most 10 for a `u64`. A reader refuses an 11th byte and a
//!   10th byte with more than its one meaningful bit (an overlong or overflowing value), so one
//!   value has one encoding.
//! - A delta column writes each value's difference from the one before (the first's from 0),
//!   wrapping, zigzagged, as LEB128. The count is the caller's (a page header holds it).
//!
//! # Floor
//!
//! A delta column is `ceil(bits / 7)` bytes a value, where `bits` is the width of its zigzagged
//! difference: one byte a value for differences in `-64..64`. The encoder measures the exact
//! size first and reserves it once, so it writes in one pass with no growth; the decoder reads
//! each byte once. Both are byte at a time (no SIMD varint decode).
//!
//! # Measured (`examples/codec.rs`, release, one Mac run, 2026-11-15)
//!
//! Sorted positions with gaps below 300: the delta column is 4.5 times smaller than 8-byte
//! values, 3.8 ns a value to encode and 1.7 to decode.

use core::fmt;

use blaze_int::Int;
use blaze_own::{AllocFailure, Vec};

use crate::plant::{self, Plant};

/// Why bytes did not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The bytes ended inside a value: the value at byte `at`, the `index`th asked for.
    Truncated { at: usize, index: usize },
    /// The value at byte `at` has more bytes or bits than a `u64` holds.
    Overlong { at: usize },
    /// Room for the encoding could not be had; nothing was written.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for CodecError {
    fn from(failure: AllocFailure) -> CodecError {
        CodecError::Alloc(failure)
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated { at, index } => write!(f, "blaze_kit::codec: bytes end in value {index}, at byte {at}"),
            CodecError::Overlong { at } => write!(f, "blaze_kit::codec: the LEB128 value at byte {at} overflows a u64"),
            CodecError::Alloc(failure) => write!(f, "blaze_kit::codec: {failure}"),
        }
    }
}

/// `0, -1, 1, -2, ..` to `0, 1, 2, 3, ..`.
#[inline]
pub const fn zigzag(value: i64) -> u64 {
    (value.cast_unsigned() << 1) ^ (value >> 63).cast_unsigned()
}

/// The inverse of [`zigzag`].
#[inline]
pub const fn unzigzag(value: u64) -> i64 {
    (value >> 1).cast_signed() ^ (value & 1).cast_signed().wrapping_neg()
}

/// Bytes of `value` in LEB128: 1 to 10.
#[inline]
pub fn uleb128_len(value: u64) -> usize {
    let bits = 64 - (value | 1).leading_zeros();
    blaze_int::idx(bits.div_ceil(7))
}

/// `value` as LEB128 into `buf`; the bytes written.
#[inline]
fn uleb128(mut value: u64, buf: &mut [u8; 10]) -> usize {
    let mut n = 0;
    loop {
        let low = value.truncate_to::<u8>() & 0x7f;
        value >>= 7;
        if value == 0 {
            buf[n] = low;
            n += 1;
            break;
        }
        buf[n] = low | 0x80;
        n += 1;
    }
    if n == 10 && plant::on(Plant::LebDropsLastByte) {
        n -= 1;
    }
    n
}

/// Appends `value` as LEB128; refused with nothing written.
pub fn write_uleb128<A: core::alloc::Allocator>(value: u64, out: &mut Vec<u8, A>) -> Result<(), AllocFailure> {
    let mut buf = [0u8; 10];
    let n = uleb128(value, &mut buf);
    out.try_extend_from_slice(&buf[..n])
}

/// The LEB128 value at `*at` in `bytes`, moving `*at` past it; `index` names the value in an
/// error.
#[inline]
pub fn read_uleb128(bytes: &[u8], at: &mut usize, index: usize) -> Result<u64, CodecError> {
    let start = *at;
    let mut value = 0u64;
    for k in 0..10u32 {
        let Some(&byte) = bytes.get(start + blaze_int::idx(k)) else {
            return Err(CodecError::Truncated { at: start, index });
        };
        let bits = u64::from(byte & 0x7f);
        if k == 9 && byte > 1 {
            return Err(CodecError::Overlong { at: start });
        }
        value |= bits << (7 * k);
        if byte & 0x80 == 0 {
            *at = start + blaze_int::idx(k) + 1;
            return Ok(value);
        }
    }
    Err(CodecError::Overlong { at: start })
}

/// Appends `values` as a delta column (see the module); refused with nothing written.
pub fn delta_encode<A: core::alloc::Allocator>(values: &[i64], out: &mut Vec<u8, A>) -> Result<(), CodecError> {
    let mut size = 0usize;
    let mut last = 0i64;
    for &value in values {
        size += uleb128_len(zigzag(value.wrapping_sub(last)));
        last = value;
    }
    out.try_reserve(size)?;
    let mut buf = [0u8; 10];
    let mut last = 0i64;
    for &value in values {
        let n = uleb128(zigzag(value.wrapping_sub(last)), &mut buf);
        // Room was reserved above: this does not allocate.
        out.extend_from_slice(&buf[..n]);
        last = value;
    }
    Ok(())
}

/// Fills `out` from the delta column at the start of `bytes`; the bytes read. `out.len()` is
/// the count written.
pub fn delta_decode(bytes: &[u8], out: &mut [i64]) -> Result<usize, CodecError> {
    let mut at = 0;
    let mut last = 0i64;
    for (index, slot) in out.iter_mut().enumerate() {
        last = last.wrapping_add(unzigzag(read_uleb128(bytes, &mut at, index)?));
        *slot = last;
    }
    Ok(at)
}
