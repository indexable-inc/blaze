//! The RLE/bit-packed hybrid (https://parquet.apache.org/docs/file-format/data-pages/encodings/,
//! "Run Length Encoding / Bit-Packing Hybrid"): definition and repetition levels, and a
//! dictionary page's indices.
//!
//! A run is a varint header and its values: `count << 1` and one value in `ceil(width / 8)`
//! little-endian bytes (a repeated run), or `groups << 1 | 1` and `groups * 8` values packed
//! `width` bits each, least significant bit first (a bit-packed run). The encoder writes a
//! repeated run wherever eight or more equal values follow each other, and packs the rest in
//! runs of at most 63 groups (the most parquet-mr writes; every reader reads them).

use core::alloc::Allocator;

use blaze_int::Int;
use blaze_own::Vec;

use crate::thrift::varint;

/// The fewest bits that hold every value up to `max`.
pub(crate) const fn bit_width(max: u32) -> u8 {
    // At most 32: it fits.
    (u32::BITS - max.leading_zeros()).to_le_bytes()[0]
}

/// The most groups of eight one bit-packed run is written with.
const GROUPS: usize = 63;

/// `values` (each below `1 << width`) in the hybrid, appended to `out`.
pub(crate) fn encode<A: Allocator>(out: &mut Vec<u8, A>, values: &[u8], width: u8) {
    if width == 0 {
        // Every value is 0: one repeated run of no value bytes says so.
        if !values.is_empty() {
            varint(out, values.len().widen_to::<u64>() << 1);
        }
        return;
    }
    let (mut at, mut packed) = (0, 0);
    while at < values.len() {
        let value = values[at];
        let run = values[at..].iter().take_while(|&&it| it == value).count();
        // A repeated run may start only where the packed values before it fill whole groups:
        // the first few of the run's values fill the last group.
        let fill = (8 - (at - packed) % 8) % 8;
        if run >= fill + 8 {
            pack(out, &values[packed..at + fill], width);
            repeat(out, value, run - fill, width);
            packed = at + run;
        }
        at += run;
    }
    pack(out, &values[packed..], width);
}

fn repeat<A: Allocator>(out: &mut Vec<u8, A>, value: u8, count: usize, width: u8) {
    varint(out, count.widen_to::<u64>() << 1);
    out.push(value);
    // A width past eight bits takes more bytes; levels and this crate's values never do.
    for _ in 1..usize::from(width).div_ceil(8) {
        out.push(0);
    }
}

/// `values` as bit-packed runs, the last group filled with zeros.
fn pack<A: Allocator>(out: &mut Vec<u8, A>, values: &[u8], width: u8) {
    for run in values.chunks(GROUPS * 8) {
        let groups = run.len().div_ceil(8);
        varint(out, (groups.widen_to::<u64>() << 1) | 1);
        let (mut acc, mut bits) = (0u32, 0u8);
        for at in 0..groups * 8 {
            let value = run.get(at).copied().unwrap_or(0);
            acc |= u32::from(value) << bits;
            bits += width;
            while bits >= 8 {
                out.push(acc.truncate_to::<u8>());
                acc >>= 8;
                bits -= 8;
            }
        }
        // `groups * 8 * width` bits are whole bytes.
    }
}

/// Why levels or indices could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LevelError {
    /// The bytes end inside a run.
    Truncated,
    /// A width past 32 bits.
    Width,
    /// A run's header past what it could hold.
    Header,
}

/// `count` values of `width` bits from the hybrid in `bytes`, each given to `each`. Values a
/// last run has past `count` (a group's filling) are not read; a run that says more values than
/// are left is held to `count`.
pub(crate) fn decode(
    bytes: &[u8],
    width: u8,
    count: usize,
    mut each: impl FnMut(u32),
) -> Result<(), LevelError> {
    if width > 32 {
        return Err(LevelError::Width);
    }
    let value_bytes = usize::from(width).div_ceil(8);
    let mut at = 0;
    let mut left = count;
    while left > 0 {
        let header = read_varint(bytes, &mut at)?;
        let run = usize::try_from(header >> 1).map_err(|_| LevelError::Header)?;
        if header & 1 == 0 {
            let raw = bytes.get(at..at + value_bytes).ok_or(LevelError::Truncated)?;
            at += value_bytes;
            let mut value = 0u32;
            for (i, byte) in raw.iter().enumerate() {
                value |= u32::from(*byte) << (8 * i);
            }
            if run == 0 {
                return Err(LevelError::Header);
            }
            for _ in 0..run.min(left) {
                each(value);
            }
            left -= run.min(left);
        } else {
            let values = run.checked_mul(8).ok_or(LevelError::Header)?;
            let len = run.checked_mul(usize::from(width)).ok_or(LevelError::Header)?;
            let packed = bytes.get(at..at.checked_add(len).ok_or(LevelError::Header)?).ok_or(LevelError::Truncated)?;
            at += len;
            if values == 0 {
                return Err(LevelError::Header);
            }
            let mask = if width == 32 { u64::from(u32::MAX) } else { (1u64 << width) - 1 };
            let (mut acc, mut bits, mut next) = (0u64, 0u32, 0usize);
            for _ in 0..values.min(left) {
                while bits < u32::from(width) {
                    acc |= u64::from(packed[next]) << bits;
                    next += 1;
                    bits += 8;
                }
                each((acc & mask).truncate_to::<u32>());
                acc >>= width;
                bits -= u32::from(width);
            }
            left -= values.min(left);
        }
    }
    Ok(())
}

/// A varint at `at` in `bytes`, at most five bytes (a run's header fits 32 bits).
fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64, LevelError> {
    let mut value = 0u64;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*at).ok_or(LevelError::Truncated)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(LevelError::Header)
}
