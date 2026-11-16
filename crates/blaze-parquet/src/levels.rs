//! The RLE/bit-packed hybrid (https://parquet.apache.org/docs/file-format/data-pages/encodings/,
//! "Run Length Encoding / Bit-Packing Hybrid"): definition and repetition levels, a dictionary
//! page's indices and a BOOLEAN column's RLE values.
//!
//! A run is a varint header and its values: `count << 1` and one value in `ceil(width / 8)`
//! little-endian bytes (a repeated run), or `groups << 1 | 1` and `groups * 8` values packed
//! `width` bits each, least significant bit first (a bit-packed run). The encoder writes a
//! repeated run wherever eight or more equal values follow each other, and packs the rest in
//! runs of at most 63 groups (the most parquet-mr writes; every reader reads them).
//!
//! A bit-packed run is blaze-columns' bit-packing layout (`enc::bits`, value `i` at bit
//! `i * width`, least significant first): it packs and unpacks them, the one copy of the kernel.

use core::alloc::Allocator;

use blaze_columns::enc::bits;
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

/// The allocator refused the room of a packed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refused;

/// `n` values (value `i` is `value(i)`, each below `1 << width`) in the hybrid, appended to
/// `out`.
pub(crate) fn encode<A: Allocator>(
    out: &mut Vec<u8, A>,
    n: usize,
    width: u8,
    value: impl Fn(usize) -> u32,
) -> Result<(), Refused> {
    if width == 0 {
        // Every value is 0: one repeated run of no value bytes says so.
        if n > 0 {
            varint(out, n.widen_to::<u64>() << 1);
        }
        return Ok(());
    }
    let (mut at, mut packed) = (0, 0);
    while at < n {
        let first = value(at);
        let mut run = 1;
        while at + run < n && value(at + run) == first {
            run += 1;
        }
        // A repeated run may start only where the packed values before it fill whole groups:
        // the first few of the run's values fill the last group.
        let fill = (8 - (at - packed) % 8) % 8;
        if run >= fill + 8 {
            pack(out, packed, at + fill, width, &value)?;
            repeat(out, first, run - fill, width);
            packed = at + run;
        }
        at += run;
    }
    pack(out, packed, n, width, &value)
}

fn repeat<A: Allocator>(out: &mut Vec<u8, A>, value: u32, count: usize, width: u8) {
    varint(out, count.widen_to::<u64>() << 1);
    out.extend_from_slice(&value.to_le_bytes()[..usize::from(width).div_ceil(8)]);
}

/// Values `from..to` as bit-packed runs, the last group filled with zeros.
fn pack<A: Allocator>(
    out: &mut Vec<u8, A>,
    from: usize,
    to: usize,
    width: u8,
    value: &impl Fn(usize) -> u32,
) -> Result<(), Refused> {
    let mut start = from;
    while start < to {
        let len = (to - start).min(GROUPS * 8);
        let groups = len.div_ceil(8);
        varint(out, (groups.widen_to::<u64>() << 1) | 1);
        // `groups * 8 * width` bits are whole bytes: the fill packs as zeros.
        bits::pack(groups * 8, width, out, |j| if j < len { u64::from(value(start + j)) } else { 0 })
            .map_err(|_| Refused)?;
        start += len;
    }
    Ok(())
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

/// Values unpacked at once: 64 groups.
const PIECE: usize = 512;

/// `count` values of `width` bits from the hybrid in `bytes`, each given to `each`. Values a
/// last run has past `count` (a group's filling) are not read; a run that says more values than
/// are left is held to `count`. The bytes read.
pub(crate) fn decode(bytes: &[u8], width: u8, count: usize, mut each: impl FnMut(u32)) -> Result<usize, LevelError> {
    if width > 32 {
        return Err(LevelError::Width);
    }
    let value_bytes = usize::from(width).div_ceil(8);
    let mut at = 0;
    let mut left = count;
    let mut unpacked = [0u64; PIECE];
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
            let end = at.checked_add(len).ok_or(LevelError::Header)?;
            if values == 0 {
                return Err(LevelError::Header);
            }
            if end > bytes.len() {
                return Err(LevelError::Truncated);
            }
            let wanted = values.min(left);
            unpack(&bytes[at..], len, width, wanted, &mut unpacked, |it| each(it.truncate_to::<u32>()));
            at = end;
            left -= wanted;
        }
    }
    Ok(at)
}

/// The first `count` values of width `w` packed in the first `len` bytes of `data` (which may
/// go on past them), given to `each`, a piece of [`PIECE`] at a time through `bits::unpack`:
/// straight from `data` where its eight-byte loads stay inside it, else from a padded copy.
pub(crate) fn unpack(data: &[u8], len: usize, w: u8, count: usize, scratch: &mut [u64; PIECE], mut each: impl FnMut(u64)) {
    let mut done = 0;
    while done < count {
        let take = (count - done).min(PIECE);
        // Pieces start on whole bytes: `PIECE * w` bits are.
        let from = done * usize::from(w) / 8;
        let bytes = bits::packed_len(take, w);
        let out = &mut scratch[..take];
        if from + bytes + bits::PAD + 1 <= data.len() {
            bits::unpack(&data[from..], w, out);
        } else {
            let mut copy = [0u8; PIECE * 8 + bits::PAD + 1];
            let have = len.min(data.len()).saturating_sub(from).min(bytes);
            copy[..have].copy_from_slice(&data[from..from + have]);
            bits::unpack(&copy, w, out);
        }
        out.iter().copied().for_each(&mut each);
        done += take;
    }
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
