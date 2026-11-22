//! Parquet's delta encodings (https://parquet.apache.org/docs/file-format/data-pages/encodings/):
//! DELTA_BINARY_PACKED for integers, DELTA_LENGTH_BYTE_ARRAY and DELTA_BYTE_ARRAY for text.
//!
//! DELTA_BINARY_PACKED is a header (the block's values, its miniblocks, the values in all, the
//! first value zigzagged) then blocks of [`BLOCK`] deltas: the block's least delta (zigzagged),
//! each miniblock's width (a byte each) and the miniblocks, [`MINI`] deltas less the least each,
//! bit-packed (blaze-columns' layout, which is Parquet's). An INT32 column's deltas are taken in
//! 32-bit wrapping arithmetic, as parquet-mr writes them, so a width is at most 32; a reader
//! that adds in 64 bits and keeps the low 32 gets the same values.
//!
//! DELTA_LENGTH_BYTE_ARRAY is the lengths in DELTA_BINARY_PACKED, then the bytes end to end.
//! DELTA_BYTE_ARRAY is each text's prefix shared with the one before (its length, delta-packed)
//! then the rest as DELTA_LENGTH_BYTE_ARRAY: sorted paths and names cost their new suffix.

use core::alloc::Allocator;

use blaze_columns::enc::bits;
use blaze_int::Int;
use blaze_own::Vec;

use crate::{
    levels::{self, Refused},
    thrift::{varint, zigzag},
};

/// Deltas a block holds.
pub(crate) const BLOCK: usize = 128;
/// Miniblocks a block holds.
pub(crate) const MINIS: usize = 4;
/// Deltas a miniblock holds.
pub(crate) const MINI: usize = BLOCK / MINIS;

/// `n` integers (value `i` is `value(i)`) as DELTA_BINARY_PACKED, appended to `out`; `narrow`
/// takes the deltas of INT32s.
pub(crate) fn encode<A: Allocator>(out: &mut Vec<u8, A>, n: usize, narrow: bool, value: impl Fn(usize) -> i64) -> Result<(), Refused> {
    let delta = |i: usize| -> i64 {
        if narrow {
            i64::from(value(i).truncate_to::<i32>().wrapping_sub(value(i - 1).truncate_to::<i32>()))
        } else {
            value(i).wrapping_sub(value(i - 1))
        }
    };
    varint(out, BLOCK.widen_to::<u64>());
    varint(out, MINIS.widen_to::<u64>());
    varint(out, n.widen_to::<u64>());
    varint(out, zigzag(if n == 0 { 0 } else { value(0) }));
    let mut start = 1;
    while start < n {
        let end = (start + BLOCK).min(n);
        let least = (start..end).map(delta).min().unwrap_or(0);
        // The delta past the least, as the miniblock packs it.
        let packed = |i: usize| -> u64 {
            if narrow {
                delta(i).truncate_to::<i32>().wrapping_sub(least.truncate_to::<i32>()).cast_unsigned().into()
            } else {
                delta(i).wrapping_sub(least).cast_unsigned()
            }
        };
        varint(out, zigzag(least));
        let mut widths = [0u8; MINIS];
        for (m, width) in widths.iter_mut().enumerate() {
            let from = start + m * MINI;
            let max = (from..(from + MINI).min(end)).map(packed).max().unwrap_or(0);
            *width = bits::width_of(max);
        }
        out.extend_from_slice(&widths);
        for (m, &width) in widths.iter().enumerate() {
            let from = start + m * MINI;
            if from >= end {
                // No deltas left: the miniblock's width is written (0), its body is not.
                break;
            }
            // A miniblock is packed whole: past the last delta, zeros.
            bits::pack(MINI, width, out, |j| if from + j < end { packed(from + j) } else { 0 }).map_err(|_| Refused)?;
        }
        start = end;
    }
    Ok(())
}

/// Why delta-encoded values could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeltaError {
    /// The bytes end inside the values.
    Truncated,
    /// A header or a width that says something impossible.
    Corrupt(&'static str),
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64, DeltaError> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*at).ok_or(DeltaError::Truncated)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(DeltaError::Corrupt("a varint past ten bytes"))
}

fn unzigzag(raw: u64) -> i64 {
    blaze_kit::codec::unzigzag(raw)
}

/// DELTA_BINARY_PACKED integers at the start of `bytes`, each given to `each` (in 64-bit
/// wrapping arithmetic: an INT32's are its low 32 bits). At most `most` are read; the values
/// the header says, and the bytes read.
pub(crate) fn decode(bytes: &[u8], most: usize, mut each: impl FnMut(i64)) -> Result<(usize, usize), DeltaError> {
    let mut at = 0;
    let block = usize::try_from(read_varint(bytes, &mut at)?).map_err(|_| DeltaError::Corrupt("a block past memory"))?;
    let minis = usize::try_from(read_varint(bytes, &mut at)?).map_err(|_| DeltaError::Corrupt("miniblocks past memory"))?;
    let total = usize::try_from(read_varint(bytes, &mut at)?).map_err(|_| DeltaError::Corrupt("values past memory"))?;
    let mut value = unzigzag(read_varint(bytes, &mut at)?);
    if block == 0 || block % 128 != 0 || minis == 0 || block % minis != 0 || (block / minis) % 32 != 0 || block > 1 << 16 {
        return Err(DeltaError::Corrupt("a block not of miniblocks of a multiple of 32 values"));
    }
    if total > most {
        return Err(DeltaError::Corrupt("more delta values than the page has room for"));
    }
    if total == 0 {
        return Ok((0, at));
    }
    each(value);
    let mini = block / minis;
    let mut left = total - 1;
    let mut scratch = [0u64; 512];
    while left > 0 {
        let least = unzigzag(read_varint(bytes, &mut at)?);
        let widths = bytes.get(at..at + minis).ok_or(DeltaError::Truncated)?;
        at += minis;
        for &width in widths {
            if left == 0 {
                break;
            }
            if width > 64 {
                return Err(DeltaError::Corrupt("a miniblock wider than 64 bits"));
            }
            let len = bits::packed_len(mini, width);
            let end = at.checked_add(len).filter(|&it| it <= bytes.len()).ok_or(DeltaError::Truncated)?;
            let take = mini.min(left);
            levels::unpack(&bytes[at..], len, width, take, &mut scratch, |packed| {
                value = value.wrapping_add(least).wrapping_add(packed.cast_signed());
                each(value);
            });
            at = end;
            left -= take;
        }
    }
    Ok((total, at))
}

/// `n` texts (text `i` is `text(i)`) as DELTA_LENGTH_BYTE_ARRAY.
pub(crate) fn encode_lengths<'t, A: Allocator>(out: &mut Vec<u8, A>, n: usize, text: impl Fn(usize) -> &'t [u8]) -> Result<(), Refused> {
    encode(out, n, true, |i| text(i).len().widen_to::<u64>().cast_signed())?;
    for i in 0..n {
        out.extend_from_slice(text(i));
    }
    Ok(())
}

/// The bytes `a` and `b` begin with alike.
fn shared(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// `n` texts as DELTA_BYTE_ARRAY.
pub(crate) fn encode_prefixed<'t, A: Allocator>(out: &mut Vec<u8, A>, n: usize, text: impl Fn(usize) -> &'t [u8]) -> Result<(), Refused> {
    let prefix = |i: usize| if i == 0 { 0 } else { shared(text(i - 1), text(i)) };
    encode(out, n, true, |i| prefix(i).widen_to::<u64>().cast_signed())?;
    encode_lengths(out, n, |i| &text(i)[prefix(i)..])
}

/// The bytes DELTA_BYTE_ARRAY would take for `n` texts, without writing them: the suffixes,
/// and about a byte a text for the two delta-packed lengths.
pub(crate) fn prefixed_size<'t>(n: usize, text: impl Fn(usize) -> &'t [u8]) -> usize {
    (0..n).map(|i| text(i).len() - if i == 0 { 0 } else { shared(text(i - 1), text(i)) }).sum::<usize>() + n
}
