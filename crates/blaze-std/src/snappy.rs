//! Pure `#![no_std]`, strictly bounded Google Snappy Raw Block and Snappy Framing Format codec.
//!
//! - **Snappy Raw Block** (`compress_in`, `decompress_in`): LEB128 uncompressed length varint
//!   checked against `max_output_bytes` before allocation, 4096-entry 4-byte hash table emitting
//!   all four Snappy element tags (literal `0b00`, 1-byte copy `0b01`, 2-byte copy `0b10`, and
//!   4-byte copy `0b11`), and a safe decoder supporting overlapping matches and enforcing exact
//!   decoded length.
//! - **Snappy Framing Format** (`compress_framed_in`, `decompress_framed_in`): stream identifier
//!   chunk (`0xFF 0x06 0x00 0x00 sNaPpY`), compressed (`0x00`) and uncompressed (`0x01`) data
//!   chunks up to 64 KiB each verified with hardware-accelerated [`crate::digest::crc32c`] using
//!   Snappy's masked CRC-32C (`((crc >> 15) | (crc << 17)).wrapping_add(0xa282_ead8)`), skippable
//!   chunks (`0x80..=0xFD`) and padding (`0xFE`), and strict refusal of reserved unskippable
//!   chunks (`0x02..=0x7F`).
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexes, and
//!   zero recursion.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, ByteCursor, Vec};

use crate::digest::crc32c;

const HASH_LOG: u32 = 12;
const HASH_SIZE: usize = 1 << HASH_LOG;
const HASH_SHIFT: u32 = 32 - HASH_LOG;
const HASH_PRIME: u32 = 0x1E35_A7BD;
const SKIP_INIT: usize = 32;

const TAG_LITERAL: u8 = 0b00;
const TAG_COPY_1: u8 = 0b01;
const TAG_COPY_2: u8 = 0b10;
const TAG_COPY_4: u8 = 0b11;

const CHUNK_COMPRESSED: u8 = 0x00;
const CHUNK_UNCOMPRESSED: u8 = 0x01;
const CHUNK_STREAM_ID: u8 = 0xFF;

const STREAM_IDENTIFIER_PAYLOAD: [u8; 6] = *b"sNaPpY";
const STREAM_IDENTIFIER_CHUNK: [u8; 10] = [
    CHUNK_STREAM_ID,
    0x06,
    0x00,
    0x00,
    b's',
    b'N',
    b'a',
    b'P',
    b'p',
    b'Y',
];
const MAX_CHUNK_UNCOMPRESSED: usize = 65_536;
const MAX_EXACT_PREALLOC: usize = 16 << 20;

/// Caller-supplied bounds enforced while decoding untrusted Snappy framed streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SnappyLimits {
    /// Maximum total uncompressed bytes allowed across the entire stream.
    pub most_output_bytes: usize,
    /// Maximum number of chunks allowed in one framed stream.
    pub most_chunks: usize,
}

impl SnappyLimits {
    /// Generous default bounds (64 MiB output, 65,536 chunks).
    pub const USUAL: Self = Self { most_output_bytes: 64 << 20, most_chunks: 65_536 };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed error returned while compressing or decompressing Snappy raw blocks or framed streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnappyError {
    /// Input ended before a varint, tag, literal, copy offset, chunk header, or payload completed.
    Truncated,
    /// Raw block LEB128 uncompressed length varint overflowed `u32`.
    InvalidVarint,
    /// Copy tag offset was zero or reached past the available decoded output.
    InvalidOffset { offset: usize, available: usize },
    /// Decoded raw block byte count did not equal its declared varint length.
    LengthMismatch { expected: usize, found: usize },
    /// Framed stream did not begin with a Stream Identifier chunk (`0xFF`).
    MissingStreamIdentifier,
    /// Stream Identifier chunk did not contain the exact 6-byte `"sNaPpY"` payload.
    InvalidStreamIdentifier,
    /// Framed stream contained a reserved unskippable chunk (`0x02..=0x7F`).
    UnsupportedChunk { chunk_type: u8 },
    /// Chunk masked CRC-32C did not match the decoded chunk payload.
    ChecksumMismatch { expected: u32, found: u32 },
    /// Single framed chunk exceeded the 65,536-byte Snappy framing uncompressed maximum.
    ChunkTooLarge { bytes: usize, limit: usize },
    /// Declared or decoded output exceeded the caller's maximum byte bound.
    OutputLimitExceeded { bytes: usize, limit: usize },
    /// Framed stream chunk count exceeded [`SnappyLimits::most_chunks`].
    TooManyChunks { count: usize, limit: usize },
    /// Raw block input exceeded the 32-bit Snappy raw block length limit.
    InputTooLarge { bytes: usize, limit: usize },
    /// Memory allocation in the caller's allocator failed.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for SnappyError {
    #[inline]
    fn from(failure: AllocFailure) -> Self {
        SnappyError::Alloc(failure)
    }
}

impl fmt::Display for SnappyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SnappyError::Truncated => f.write_str("bstd::snappy: truncated block or framed stream"),
            SnappyError::InvalidVarint => {
                f.write_str("bstd::snappy: uncompressed length varint overflows u32")
            }
            SnappyError::InvalidOffset { offset, available } => {
                write!(
                    f,
                    "bstd::snappy: invalid copy offset {offset} with only {available} decoded bytes"
                )
            }
            SnappyError::LengthMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::snappy: decoded length {found} does not match declared length {expected}"
                )
            }
            SnappyError::MissingStreamIdentifier => {
                f.write_str("bstd::snappy: framed stream must begin with stream identifier chunk")
            }
            SnappyError::InvalidStreamIdentifier => {
                f.write_str("bstd::snappy: invalid stream identifier payload (expected \"sNaPpY\")")
            }
            SnappyError::UnsupportedChunk { chunk_type } => {
                write!(
                    f,
                    "bstd::snappy: unsupported unskippable chunk type 0x{chunk_type:02x}"
                )
            }
            SnappyError::ChecksumMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::snappy: masked CRC-32C mismatch (expected 0x{expected:08x}, computed 0x{found:08x})"
                )
            }
            SnappyError::ChunkTooLarge { bytes, limit } => {
                write!(
                    f,
                    "bstd::snappy: framed chunk uncompressed size {bytes} exceeds limit {limit}"
                )
            }
            SnappyError::OutputLimitExceeded { bytes, limit } => {
                write!(f, "bstd::snappy: output size {bytes} exceeds caller limit {limit}")
            }
            SnappyError::TooManyChunks { count, limit } => {
                write!(f, "bstd::snappy: framed stream chunk count {count} exceeds limit {limit}")
            }
            SnappyError::InputTooLarge { bytes, limit } => {
                write!(f, "bstd::snappy: raw block input size {bytes} exceeds maximum {limit}")
            }
            SnappyError::Alloc(failure) => write!(f, "bstd::snappy: {failure}"),
        }
    }
}

impl core::error::Error for SnappyError {}

/// Snappy Framing Format masked CRC-32C transform:
/// `((crc >> 15) | (crc << 17)).wrapping_add(0xa282_ead8)`.
#[inline]
#[must_use]
pub const fn mask_crc32c(crc: u32) -> u32 {
    ((crc >> 15) | (crc << 17)).wrapping_add(0xa282_ead8)
}

#[inline]
fn alloc_hash_table<A: Allocator>(alloc: A) -> Result<Vec<u32, A>, SnappyError> {
    let mut table = Vec::try_with_capacity_in(HASH_SIZE, alloc)?;
    let zeros = [0u32; 256];
    for _ in 0..(HASH_SIZE / zeros.len()) {
        table.try_extend_from_slice(&zeros)?;
    }
    Ok(table)
}

#[inline]
fn read_u16_le_fast(bytes: &[u8], offset: usize) -> Option<u16> {
    let arr = bytes.get(offset..)?.first_chunk::<2>()?;
    Some(u16::from_le_bytes(*arr))
}

#[inline]
fn read_u24_le(bytes: &[u8], offset: usize) -> Result<u32, SnappyError> {
    let end = offset.checked_add(3).ok_or(SnappyError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(SnappyError::Truncated)?;
    let [b0, b1, b2]: [u8; 3] = slice.try_into().map_err(|_| SnappyError::Truncated)?;
    Ok(u32::from_le_bytes([b0, b1, b2, 0]))
}

#[inline]
fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, SnappyError> {
    let end = offset.checked_add(4).ok_or(SnappyError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(SnappyError::Truncated)?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| SnappyError::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

#[inline]
fn read_u32_le_fast(bytes: &[u8], offset: usize) -> Option<u32> {
    let arr = bytes.get(offset..)?.first_chunk::<4>()?;
    Some(u32::from_le_bytes(*arr))
}

#[inline]
fn snappy_hash(seq: u32) -> usize {
    let shifted = seq.wrapping_mul(HASH_PRIME) >> HASH_SHIFT;
    usize::try_from(shifted).unwrap_or(0) & (HASH_SIZE - 1)
}

#[inline]
fn write_varint_u32<A: Allocator>(
    cur: &mut ByteCursor<'_, A>,
    mut value: u32,
) -> Result<(), SnappyError> {
    cur.try_reserve(5)?;
    loop {
        let low = u8::try_from(value & 0x7F).unwrap_or(0);
        value >>= 7;
        if value == 0 {
            let _ = cur.push_within_cap(low);
            return Ok(());
        }
        let _ = cur.push_within_cap(low | 0x80);
    }
}

fn decode_varint_u32(input: &[u8]) -> Result<(u32, usize), SnappyError> {
    let mut value: u32 = 0;
    let mut shift: u32 = 0;
    let mut pos: usize = 0;
    loop {
        let byte = *input.get(pos).ok_or(SnappyError::Truncated)?;
        pos += 1;
        if shift == 28 {
            if byte > 0x0F {
                return Err(SnappyError::InvalidVarint);
            }
            value |= u32::from(byte) << 28;
            return Ok((value, pos));
        }
        value |= u32::from(byte & 0x7F) << shift;
        if (byte & 0x80) == 0 {
            return Ok((value, pos));
        }
        shift += 7;
    }
}

#[inline]
fn count_match(a: &[u8], b: &[u8]) -> usize {
    let limit = a.len().min(b.len());
    let (Some(sa), Some(sb)) = (a.get(..limit), b.get(..limit)) else {
        return 0;
    };
    let mut matched: usize = 0;
    let chunks_a = sa.chunks_exact(8);
    let chunks_b = sb.chunks_exact(8);
    let rem_a = chunks_a.remainder();
    let rem_b = chunks_b.remainder();
    for (ca, cb) in chunks_a.zip(chunks_b) {
        let Ok(aa) = <[u8; 8]>::try_from(ca) else {
            break;
        };
        let Ok(bb) = <[u8; 8]>::try_from(cb) else {
            break;
        };
        let diff = u64::from_le_bytes(aa) ^ u64::from_le_bytes(bb);
        if diff != 0 {
            let tz = usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(0);
            return matched + tz;
        }
        matched += 8;
    }
    for (&ba, &bb) in rem_a.iter().zip(rem_b.iter()) {
        if ba != bb {
            break;
        }
        matched += 1;
    }
    matched
}

#[inline]
const fn literal_encoded_len(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let n = len - 1;
    let hdr = if n < 60 {
        1
    } else if n <= 0xFF {
        2
    } else if n <= 0xFFFF {
        3
    } else if n <= 0xFF_FFFF {
        4
    } else {
        5
    };
    hdr + len
}

#[inline]
const fn copy_chunk_encoded_len(offset: usize, chunk_len: usize) -> usize {
    if offset < 2_048 && chunk_len >= 4 && chunk_len <= 11 {
        2
    } else if offset <= 65_535 {
        3
    } else {
        5
    }
}

#[inline]
const fn copy_encoded_len(offset: usize, mut len: usize) -> usize {
    if len <= 64 {
        return copy_chunk_encoded_len(offset, len);
    }
    let mut total: usize = 0;
    while len >= 68 {
        total += copy_chunk_encoded_len(offset, 64);
        len -= 64;
    }
    if len > 64 {
        total += copy_chunk_encoded_len(offset, 60);
        len -= 60;
    }
    if len > 0 {
        total += copy_chunk_encoded_len(offset, len);
    }
    total
}

#[inline]
fn write_literal<A: Allocator>(
    cur: &mut ByteCursor<'_, A>,
    input: &[u8],
    src_pos: usize,
    lit_len: usize,
) {
    if lit_len == 0 {
        return;
    }
    let n = lit_len - 1;
    if n < 60 {
        let tag = (u8::try_from(n).unwrap_or(0) << 2) | TAG_LITERAL;
        let _ = cur.push_within_cap(tag);
    } else {
        write_long_literal_hdr(cur, n);
    }
    let _ = cur.write_lits_wild(input, src_pos, lit_len);
}

#[cold]
#[inline(never)]
fn write_long_literal_hdr<A: Allocator>(cur: &mut ByteCursor<'_, A>, n: usize) {
    if n <= 0xFF {
        let b0 = u8::try_from(n).unwrap_or(0);
        let _ = cur.write_array_within_cap([(60u8 << 2) | TAG_LITERAL, b0]);
    } else if n <= 0xFFFF {
        let [b0, b1] = u16::try_from(n).unwrap_or(0).to_le_bytes();
        let _ = cur.write_array_within_cap([(61u8 << 2) | TAG_LITERAL, b0, b1]);
    } else if n <= 0xFF_FFFF {
        let [b0, b1, b2, _] = u32::try_from(n).unwrap_or(0).to_le_bytes();
        let _ = cur.write_array_within_cap([(62u8 << 2) | TAG_LITERAL, b0, b1, b2]);
    } else {
        let [b0, b1, b2, b3] = u32::try_from(n).unwrap_or(0).to_le_bytes();
        let _ = cur.write_array_within_cap([(63u8 << 2) | TAG_LITERAL, b0, b1, b2, b3]);
    }
}

#[inline]
fn write_copy_chunk<A: Allocator>(cur: &mut ByteCursor<'_, A>, offset: usize, chunk_len: usize) {
    if offset < 2_048 && (4..=11).contains(&chunk_len) {
        let off_hi = u8::try_from(offset >> 8).unwrap_or(0);
        let off_lo = u8::try_from(offset & 0xFF).unwrap_or(0);
        let len_bits = u8::try_from(chunk_len - 4).unwrap_or(0);
        let tag = (off_hi << 5) | (len_bits << 2) | TAG_COPY_1;
        let _ = cur.write_array_within_cap([tag, off_lo]);
    } else if offset <= 65_535 {
        let len_bits = u8::try_from(chunk_len - 1).unwrap_or(0);
        let tag = (len_bits << 2) | TAG_COPY_2;
        let [o0, o1] = u16::try_from(offset).unwrap_or(0).to_le_bytes();
        let _ = cur.write_array_within_cap([tag, o0, o1]);
    } else {
        let len_bits = u8::try_from(chunk_len - 1).unwrap_or(0);
        let tag = (len_bits << 2) | TAG_COPY_4;
        let [o0, o1, o2, o3] = u32::try_from(offset).unwrap_or(0).to_le_bytes();
        let _ = cur.write_array_within_cap([tag, o0, o1, o2, o3]);
    }
}

#[inline]
fn write_copy<A: Allocator>(cur: &mut ByteCursor<'_, A>, offset: usize, mut len: usize) {
    if len <= 64 {
        write_copy_chunk(cur, offset, len);
        return;
    }
    while len >= 68 {
        write_copy_chunk(cur, offset, 64);
        len -= 64;
    }
    if len > 64 {
        write_copy_chunk(cur, offset, 60);
        len -= 60;
    }
    if len > 0 {
        write_copy_chunk(cur, offset, len);
    }
}

fn compress_raw_into_cursor<A: Allocator>(
    input: &[u8],
    cur: &mut ByteCursor<'_, A>,
    table: &mut [u32; HASH_SIZE],
    abort_limit: Option<usize>,
) -> Result<bool, SnappyError> {
    let mut ip: usize = 1;
    let mut anchor: usize = 0;
    let input_limit = input.len().saturating_sub(4);

    while ip <= input_limit {
        let mut skip: usize = SKIP_INIT;
        let mut found_prev: usize = 0;
        let mut found = false;

        while ip <= input_limit {
            let Some(cur_seq) = read_u32_le_fast(input, ip) else {
                break;
            };
            let h = snappy_hash(cur_seq);
            let prev = match table.get_mut(h) {
                Some(slot) => {
                    let prev_u32 = *slot;
                    *slot = u32::try_from(ip).unwrap_or(0);
                    usize::try_from(prev_u32).unwrap_or(0)
                }
                None => break,
            };

            if prev < ip && read_u32_le_fast(input, prev) == Some(cur_seq) {
                found_prev = prev;
                found = true;
                break;
            }

            let step = skip >> 5;
            ip = ip.saturating_add(step);
            skip = skip.saturating_add(1);
        }

        if !found {
            break;
        }

        let mut match_len: usize = 4;
        let max_extra = input.len().saturating_sub(ip + 4);
        if max_extra > 0
            && let (Some(a), Some(b)) = (
                input.get(found_prev + 4..found_prev + 4 + max_extra),
                input.get(ip + 4..input.len()),
            )
        {
            match_len += count_match(a, b);
        }

        let lit_len = ip - anchor;
        let offset = ip - found_prev;
        let seq_bytes = literal_encoded_len(lit_len) + copy_encoded_len(offset, match_len);
        let next_op = cur.len().saturating_add(seq_bytes);

        if let Some(lim) = abort_limit
            && next_op > lim
        {
            return Ok(false);
        }
        if cur.spare() < seq_bytes.saturating_add(32) {
            let extra = seq_bytes.max(cur.len() / 2).saturating_add(64);
            cur.try_reserve(extra)?;
        }

        write_literal(cur, input, anchor, lit_len);
        write_copy(cur, offset, match_len);

        ip += match_len;
        anchor = ip;

        if let Some(prev_idx) = ip.checked_sub(1)
            && prev_idx <= input_limit
            && let Some(prev_seq) = read_u32_le_fast(input, prev_idx)
            && let Some(slot) = table.get_mut(snappy_hash(prev_seq))
        {
            *slot = u32::try_from(prev_idx).unwrap_or(0);
        }
    }

    let tail_len = input.len().saturating_sub(anchor);
    let tail_bytes = literal_encoded_len(tail_len);
    let final_op = cur.len().saturating_add(tail_bytes);
    if let Some(lim) = abort_limit
        && final_op > lim
    {
        return Ok(false);
    }
    if cur.spare() < tail_bytes.saturating_add(32) {
        cur.try_reserve(tail_bytes.saturating_add(32))?;
    }

    write_literal(cur, input, anchor, tail_len);
    Ok(true)
}

/// Compress `input` into a Google Snappy raw block allocated in `alloc`.
///
/// Writes the LEB128 uncompressed length varint followed by literal (`0b00`) and copy
/// (`0b01`, `0b10`, `0b11`) tags produced by a 4096-entry 4-byte hash table.
pub fn compress_in<A: Allocator + Copy>(
    input: &[u8],
    alloc: A,
) -> Result<Vec<u8, A>, SnappyError> {
    let max_u32 = usize::try_from(u32::MAX).unwrap_or(usize::MAX);
    let len_u32 = u32::try_from(input.len()).map_err(|_| SnappyError::InputTooLarge {
        bytes: input.len(),
        limit: max_u32,
    })?;

    if input.len() < 4 {
        let cap = 6 + input.len() + 32;
        let mut out = Vec::try_with_capacity_in(cap, alloc)?;
        let mut cur = out.byte_cursor();
        write_varint_u32(&mut cur, len_u32)?;
        write_literal(&mut cur, input, 0, input.len());
        cur.finish();
        return Ok(out);
    }

    let init_size = input.len().saturating_add(input.len() / 6).saturating_add(64);
    let mut out = Vec::try_with_capacity_in(init_size, alloc)?;

    let mut table_vec = alloc_hash_table(alloc)?;
    let table_arr: &mut [u32; HASH_SIZE] =
        table_vec.as_mut_slice().try_into().map_err(|_| SnappyError::Truncated)?;

    if let Some(seq0) = read_u32_le_fast(input, 0)
        && let Some(slot) = table_arr.get_mut(snappy_hash(seq0))
    {
        *slot = 0;
    }

    let mut cur = out.byte_cursor();
    write_varint_u32(&mut cur, len_u32)?;
    let _ = compress_raw_into_cursor(input, &mut cur, table_arr, None)?;
    cur.finish();
    Ok(out)
}

#[cold]
#[inline(never)]
fn read_long_lit_len(
    input: &[u8],
    pos: &mut usize,
    code: u8,
    declared_len: usize,
) -> Result<usize, SnappyError> {
    match code {
        60 => {
            let extra = *input.get(*pos).ok_or(SnappyError::Truncated)?;
            *pos += 1;
            Ok(usize::from(extra) + 1)
        }
        61 => {
            let extra = read_u16_le_fast(input, *pos).ok_or(SnappyError::Truncated)?;
            *pos += 2;
            Ok(usize::from(extra) + 1)
        }
        62 => {
            let extra = read_u24_le(input, *pos)?;
            *pos += 3;
            usize::try_from(extra)
                .ok()
                .and_then(|v| v.checked_add(1))
                .ok_or(SnappyError::LengthMismatch { expected: declared_len, found: usize::MAX })
        }
        _ => {
            let extra = read_u32_le(input, *pos)?;
            *pos += 4;
            usize::try_from(extra)
                .ok()
                .and_then(|v| v.checked_add(1))
                .ok_or(SnappyError::LengthMismatch { expected: declared_len, found: usize::MAX })
        }
    }
}

#[cold]
#[inline(never)]
fn reserve_for_decode<A: Allocator>(
    cur: &mut ByteCursor<'_, A>,
    base_len: usize,
    needed_end: usize,
    target_end: usize,
) -> Result<(), SnappyError> {
    let cur_len = cur.len();
    let written = cur_len.saturating_sub(base_len);
    let grow_to = needed_end
        .max(cur_len.saturating_add(written.max(4096)))
        .min(target_end);
    let additional = grow_to.saturating_sub(cur_len).saturating_add(32);
    cur.try_reserve(additional)?;
    Ok(())
}

fn decompress_raw_into<A: Allocator>(
    input: &[u8],
    cur: &mut ByteCursor<'_, A>,
    declared_len: usize,
    start_pos: usize,
) -> Result<(), SnappyError> {
    let base_len = cur.len();
    let target_end = base_len.checked_add(declared_len).ok_or(SnappyError::LengthMismatch {
        expected: declared_len,
        found: usize::MAX,
    })?;

    // Every 3 bytes of Snappy payload produce at most 64 bytes (21.33x), so a valid block never
    // exceeds `22 * payload_len`; capping by this prevents a tiny hostile block from OOMing while
    // still reserving the exact output size (plus 32-byte wildcopy slack) in one shot without
    // pre-zeroing for all valid blocks up to 16 MiB.
    if declared_len > 0 {
        let max_possible = input.len().saturating_sub(start_pos).saturating_mul(22);
        let initial_reserve = declared_len.min(MAX_EXACT_PREALLOC).min(max_possible.max(64));
        cur.try_reserve(initial_reserve.saturating_add(32))?;
    }

    let mut pos = start_pos;

    while pos < input.len() {
        let tag = *input.get(pos).ok_or(SnappyError::Truncated)?;
        pos += 1;
        let dst = cur.len();

        match tag & 0b11 {
            TAG_LITERAL => {
                let code = tag >> 2;
                let lit_len: usize = if code < 60 {
                    usize::from(code) + 1
                } else {
                    read_long_lit_len(input, &mut pos, code, declared_len)?
                };

                let rem_out = target_end.saturating_sub(dst);
                if lit_len > rem_out {
                    let found = dst
                        .saturating_sub(base_len)
                        .checked_add(lit_len)
                        .unwrap_or(usize::MAX);
                    return Err(SnappyError::LengthMismatch { expected: declared_len, found });
                }
                let lit_end = pos.checked_add(lit_len).ok_or(SnappyError::Truncated)?;
                if lit_end > input.len() {
                    return Err(SnappyError::Truncated);
                }
                let new_dst = dst + lit_len;
                if cur.spare() < lit_len.saturating_add(32) {
                    reserve_for_decode(cur, base_len, new_dst, target_end)?;
                }
                if !cur.write_lits_wild(input, pos, lit_len) {
                    return Err(SnappyError::Truncated);
                }
                pos = lit_end;
            }
            TAG_COPY_1 => {
                let copy_len = usize::from((tag >> 2) & 0x07) + 4;
                let off_lo = *input.get(pos).ok_or(SnappyError::Truncated)?;
                pos += 1;
                let offset = (usize::from(tag >> 5) << 8) | usize::from(off_lo);
                let available = dst.saturating_sub(base_len);
                if offset == 0 || offset > available {
                    return Err(SnappyError::InvalidOffset { offset, available });
                }
                if copy_len > target_end.saturating_sub(dst) {
                    return Err(SnappyError::LengthMismatch {
                        expected: declared_len,
                        found: available.saturating_add(copy_len),
                    });
                }
                let new_dst = dst + copy_len;
                if cur.spare() < copy_len.saturating_add(16) {
                    reserve_for_decode(cur, base_len, new_dst, target_end)?;
                }
                if !cur.copy_match_within_cap(offset, copy_len) {
                    return Err(SnappyError::InvalidOffset { offset, available });
                }
            }
            TAG_COPY_2 => {
                let copy_len = usize::from(tag >> 2) + 1;
                let offset =
                    usize::from(read_u16_le_fast(input, pos).ok_or(SnappyError::Truncated)?);
                pos += 2;
                let available = dst.saturating_sub(base_len);
                if offset == 0 || offset > available {
                    return Err(SnappyError::InvalidOffset { offset, available });
                }
                if copy_len > target_end.saturating_sub(dst) {
                    return Err(SnappyError::LengthMismatch {
                        expected: declared_len,
                        found: available.saturating_add(copy_len),
                    });
                }
                let new_dst = dst + copy_len;
                if cur.spare() < copy_len.saturating_add(16) {
                    reserve_for_decode(cur, base_len, new_dst, target_end)?;
                }
                if !cur.copy_match_within_cap(offset, copy_len) {
                    return Err(SnappyError::InvalidOffset { offset, available });
                }
            }
            _ => {
                let copy_len = usize::from(tag >> 2) + 1;
                let offset_u32 = read_u32_le_fast(input, pos).ok_or(SnappyError::Truncated)?;
                pos += 4;
                let available = dst.saturating_sub(base_len);
                let offset = usize::try_from(offset_u32)
                    .map_err(|_| SnappyError::InvalidOffset { offset: usize::MAX, available })?;
                if offset == 0 || offset > available {
                    return Err(SnappyError::InvalidOffset { offset, available });
                }
                if copy_len > target_end.saturating_sub(dst) {
                    return Err(SnappyError::LengthMismatch {
                        expected: declared_len,
                        found: available.saturating_add(copy_len),
                    });
                }
                let new_dst = dst + copy_len;
                if cur.spare() < copy_len.saturating_add(16) {
                    reserve_for_decode(cur, base_len, new_dst, target_end)?;
                }
                if !cur.copy_match_within_cap(offset, copy_len) {
                    return Err(SnappyError::InvalidOffset { offset, available });
                }
            }
        }
    }

    let dst = cur.len();
    if dst != target_end {
        return Err(SnappyError::LengthMismatch {
            expected: declared_len,
            found: dst.saturating_sub(base_len),
        });
    }
    Ok(())
}

/// Decompress a Google Snappy raw block into `alloc`.
///
/// Decodes and checks the LEB128 uncompressed length varint against `max_output_bytes` before
/// allocating, decodes all four tag types (`0b00`/`0b01`/`0b10`/`0b11`), and verifies that the
/// decoded byte count exactly matches the declared length.
pub fn decompress_in<A: Allocator + Copy>(
    input: &[u8],
    max_output_bytes: usize,
    alloc: A,
) -> Result<Vec<u8, A>, SnappyError> {
    let (declared_u32, start_pos) = decode_varint_u32(input)?;
    let declared_len = usize::try_from(declared_u32).map_err(|_| {
        SnappyError::OutputLimitExceeded { bytes: usize::MAX, limit: max_output_bytes }
    })?;
    if declared_len > max_output_bytes {
        return Err(SnappyError::OutputLimitExceeded {
            bytes: declared_len,
            limit: max_output_bytes,
        });
    }

    let mut out = if declared_len > 0 {
        let max_possible = input.len().saturating_sub(start_pos).saturating_mul(22);
        let prealloc = declared_len.min(MAX_EXACT_PREALLOC).min(max_possible.max(64));
        Vec::try_with_capacity_in(prealloc.saturating_add(32), alloc)?
    } else {
        Vec::new_in(alloc)
    };
    {
        let mut cur = out.byte_cursor();
        decompress_raw_into(input, &mut cur, declared_len, start_pos)?;
        cur.finish();
    }
    Ok(out)
}

/// Compress `input` into the Google Snappy Framing Format allocated in `alloc`.
///
/// Emits the stream identifier chunk followed by compressed (`0x00`) or uncompressed (`0x01`)
/// 64 KiB chunks, each protected by a 4-byte little-endian masked CRC-32C computed with
/// [`crate::digest::crc32c`].
pub fn compress_framed_in<A: Allocator + Copy>(
    input: &[u8],
    alloc: A,
) -> Result<Vec<u8, A>, SnappyError> {
    if input.is_empty() {
        let mut out = Vec::try_with_capacity_in(STREAM_IDENTIFIER_CHUNK.len(), alloc)?;
        out.try_extend_from_slice(&STREAM_IDENTIFIER_CHUNK)?;
        return Ok(out);
    }

    let num_chunks = (input.len() - 1) / MAX_CHUNK_UNCOMPRESSED + 1;
    let max_framed_len = STREAM_IDENTIFIER_CHUNK
        .len()
        .saturating_add(input.len())
        .saturating_add(num_chunks.saturating_mul(8))
        .saturating_add(64);
    let mut out = Vec::try_with_capacity_in(max_framed_len, alloc)?;
    out.try_extend_from_slice(&STREAM_IDENTIFIER_CHUNK)?;

    let mut table_vec = if input.len() >= 4 { Some(alloc_hash_table(alloc)?) } else { None };

    for chunk in input.chunks(MAX_CHUNK_UNCOMPRESSED) {
        let masked_crc = mask_crc32c(crc32c(chunk));
        let header_pos = out.len();
        out.try_extend_from_slice(&[0u8; 8])?;
        let payload_start = out.len();

        let compressed_len = if chunk.len() >= 4
            && let Some(ref mut tvec) = table_vec
            && let Ok(table_arr) = <&mut [u32; HASH_SIZE]>::try_from(tvec.as_mut_slice())
        {
            table_arr.fill(0);
            if let Some(seq0) = read_u32_le_fast(chunk, 0)
                && let Some(slot) = table_arr.get_mut(snappy_hash(seq0))
            {
                *slot = 0;
            }
            let chunk_len_u32 = u32::try_from(chunk.len()).unwrap_or(0);
            let abort_at = payload_start + chunk.len() - 1;
            let mut cur = out.byte_cursor();
            write_varint_u32(&mut cur, chunk_len_u32)?;
            let fitted = compress_raw_into_cursor(chunk, &mut cur, table_arr, Some(abort_at))?;
            let end_len = cur.len();
            cur.finish();
            if fitted { Some(end_len.saturating_sub(payload_start)) } else { None }
        } else {
            None
        };

        let (chunk_type, payload_len) = match compressed_len {
            Some(clen) if clen < chunk.len() => (CHUNK_COMPRESSED, clen),
            _ => {
                out.truncate(payload_start);
                out.try_extend_from_slice(chunk)?;
                (CHUNK_UNCOMPRESSED, chunk.len())
            }
        };

        let chunk_body_len = payload_len.checked_add(4).ok_or(SnappyError::ChunkTooLarge {
            bytes: usize::MAX,
            limit: MAX_CHUNK_UNCOMPRESSED,
        })?;
        let body_u32 = u32::try_from(chunk_body_len).map_err(|_| SnappyError::ChunkTooLarge {
            bytes: chunk_body_len,
            limit: 0x00FF_FFFF,
        })?;
        if body_u32 > 0x00FF_FFFF {
            return Err(SnappyError::ChunkTooLarge {
                bytes: chunk_body_len,
                limit: 0x00FF_FFFF,
            });
        }
        let [l0, l1, l2, _] = body_u32.to_le_bytes();
        let [c0, c1, c2, c3] = masked_crc.to_le_bytes();
        if let Some(d8) =
            out.as_mut_slice().get_mut(header_pos..).and_then(|s| s.first_chunk_mut::<8>())
        {
            *d8 = [chunk_type, l0, l1, l2, c0, c1, c2, c3];
        }
    }

    Ok(out)
}

/// Decompress a Google Snappy Framing Format stream into `alloc`, enforcing `limits` and
/// verifying the masked CRC-32C of every data chunk.
pub fn decompress_framed_in<A: Allocator + Copy>(
    input: &[u8],
    limits: SnappyLimits,
    alloc: A,
) -> Result<Vec<u8, A>, SnappyError> {
    decompress_framed_in_planted(input, limits, alloc, false)
}

fn decompress_framed_in_planted<A: Allocator + Copy>(
    input: &[u8],
    limits: SnappyLimits,
    alloc: A,
    plant_skip_crc: bool,
) -> Result<Vec<u8, A>, SnappyError> {
    if input.is_empty() {
        return Err(SnappyError::MissingStreamIdentifier);
    }

    let est = input.len().min(limits.most_output_bytes).min(MAX_EXACT_PREALLOC);
    let mut out = if est > 0 {
        Vec::try_with_capacity_in(est.saturating_add(32), alloc)?
    } else {
        Vec::new_in(alloc)
    };
    {
        let mut cur = out.byte_cursor();
        let mut pos: usize = 0;
        let mut chunks: usize = 0;
        let mut saw_stream_id = false;

        while pos < input.len() {
            chunks = chunks.saturating_add(1);
            if chunks > limits.most_chunks {
                return Err(SnappyError::TooManyChunks {
                    count: chunks,
                    limit: limits.most_chunks,
                });
            }

            let chunk_type = *input.get(pos).ok_or(SnappyError::Truncated)?;
            let chunk_len_u32 = read_u24_le(input, pos + 1)?;
            pos += 4;

            let chunk_len = usize::try_from(chunk_len_u32).map_err(|_| SnappyError::Truncated)?;
            let body_end = pos.checked_add(chunk_len).ok_or(SnappyError::Truncated)?;
            let body = input.get(pos..body_end).ok_or(SnappyError::Truncated)?;
            pos = body_end;

            if !saw_stream_id {
                if chunk_type != CHUNK_STREAM_ID {
                    return Err(SnappyError::MissingStreamIdentifier);
                }
                if body != STREAM_IDENTIFIER_PAYLOAD.as_slice() {
                    return Err(SnappyError::InvalidStreamIdentifier);
                }
                saw_stream_id = true;
                continue;
            }

            match chunk_type {
                CHUNK_STREAM_ID => {
                    if body != STREAM_IDENTIFIER_PAYLOAD.as_slice() {
                        return Err(SnappyError::InvalidStreamIdentifier);
                    }
                }
                CHUNK_COMPRESSED => {
                    let expected_masked = read_u32_le(body, 0)?;
                    let block_bytes = body.get(4..).ok_or(SnappyError::Truncated)?;
                    let (declared_u32, data_start) = decode_varint_u32(block_bytes)?;
                    let declared_len = usize::try_from(declared_u32).map_err(|_| {
                        SnappyError::ChunkTooLarge {
                            bytes: usize::MAX,
                            limit: MAX_CHUNK_UNCOMPRESSED,
                        }
                    })?;
                    if declared_len > MAX_CHUNK_UNCOMPRESSED {
                        return Err(SnappyError::ChunkTooLarge {
                            bytes: declared_len,
                            limit: MAX_CHUNK_UNCOMPRESSED,
                        });
                    }
                    let total_after = cur.len().checked_add(declared_len).ok_or(
                        SnappyError::OutputLimitExceeded {
                            bytes: usize::MAX,
                            limit: limits.most_output_bytes,
                        },
                    )?;
                    if total_after > limits.most_output_bytes {
                        return Err(SnappyError::OutputLimitExceeded {
                            bytes: total_after,
                            limit: limits.most_output_bytes,
                        });
                    }

                    let chunk_out_start = cur.len();
                    decompress_raw_into(block_bytes, &mut cur, declared_len, data_start)?;

                    if !plant_skip_crc {
                        let decoded =
                            cur.as_slice().get(chunk_out_start..).ok_or(SnappyError::Truncated)?;
                        let actual_masked = mask_crc32c(crc32c(decoded));
                        if actual_masked != expected_masked {
                            return Err(SnappyError::ChecksumMismatch {
                                expected: expected_masked,
                                found: actual_masked,
                            });
                        }
                    }
                }
                CHUNK_UNCOMPRESSED => {
                    let expected_masked = read_u32_le(body, 0)?;
                    let payload = body.get(4..).ok_or(SnappyError::Truncated)?;
                    if payload.len() > MAX_CHUNK_UNCOMPRESSED {
                        return Err(SnappyError::ChunkTooLarge {
                            bytes: payload.len(),
                            limit: MAX_CHUNK_UNCOMPRESSED,
                        });
                    }
                    let total_after = cur.len().checked_add(payload.len()).ok_or(
                        SnappyError::OutputLimitExceeded {
                            bytes: usize::MAX,
                            limit: limits.most_output_bytes,
                        },
                    )?;
                    if total_after > limits.most_output_bytes {
                        return Err(SnappyError::OutputLimitExceeded {
                            bytes: total_after,
                            limit: limits.most_output_bytes,
                        });
                    }

                    if !plant_skip_crc {
                        let actual_masked = mask_crc32c(crc32c(payload));
                        if actual_masked != expected_masked {
                            return Err(SnappyError::ChecksumMismatch {
                                expected: expected_masked,
                                found: actual_masked,
                            });
                        }
                    }
                    cur.try_write_slice(payload)?;
                }
                // Reserved skippable chunks (0x80..=0xFD) and padding (0xFE).
                0x80..=0xFE => {}
                // Reserved unskippable chunks (0x02..=0x7F) must be refused.
                0x02..=0x7F => {
                    return Err(SnappyError::UnsupportedChunk { chunk_type });
                }
            }
        }

        cur.finish();
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::{
        CHUNK_COMPRESSED, CHUNK_UNCOMPRESSED, STREAM_IDENTIFIER_CHUNK, SnappyError, SnappyLimits,
        TAG_COPY_4, compress_framed_in, compress_in, decompress_framed_in,
        decompress_framed_in_planted, decompress_in, mask_crc32c,
    };
    use crate::digest::crc32c;

    #[test]
    fn raw_block_and_framing_round_trip_all_shapes() {
        let h = heap();
        let alloc = h.alloc();

        // Empty
        let raw_empty = compress_in(b"", alloc).expect("raw empty");
        assert_eq!(raw_empty.as_slice(), &[0x00]);
        assert_eq!(decompress_in(raw_empty.as_slice(), 1024, alloc).expect("dec empty").as_slice(), b"");

        let framed_empty = compress_framed_in(b"", alloc).expect("framed empty");
        assert_eq!(framed_empty.as_slice(), &STREAM_IDENTIFIER_CHUNK);
        assert_eq!(
            decompress_framed_in(framed_empty.as_slice(), SnappyLimits::SHIPPED, alloc)
                .expect("dec framed empty")
                .as_slice(),
            b""
        );

        // Tiny
        let tiny = b"hi!";
        let raw_tiny = compress_in(tiny, alloc).expect("raw tiny");
        assert_eq!(decompress_in(raw_tiny.as_slice(), 1024, alloc).expect("dec tiny").as_slice(), tiny);
        let framed_tiny = compress_framed_in(tiny, alloc).expect("framed tiny");
        assert_eq!(
            decompress_framed_in(framed_tiny.as_slice(), SnappyLimits::SHIPPED, alloc)
                .expect("dec framed tiny")
                .as_slice(),
            tiny
        );

        // Repetitive (triggers overlapping 1-byte and 2-byte copies)
        let mut rep = OwnVec::new_in(alloc);
        rep.resize(16_384, b'R');
        let raw_rep = compress_in(rep.as_slice(), alloc).expect("raw rep");
        assert!(raw_rep.len() < 1_024);
        assert_eq!(
            decompress_in(raw_rep.as_slice(), 32_768, alloc).expect("dec rep").as_slice(),
            rep.as_slice()
        );

        // Multi-chunk (> 64 KiB) + 4-byte copy tag (offset > 65_535)
        let marker = b"SNAPPY_LONG_DISTANCE_MATCH_01234";
        let mut long_dist = OwnVec::with_capacity_in(70_000, alloc);
        long_dist.extend_from_slice(marker);
        long_dist.resize(68_000, 0u8);
        long_dist.extend_from_slice(marker);
        let raw_long = compress_in(long_dist.as_slice(), alloc).expect("raw long");
        // Verify the final 5 bytes are an exact 4-byte copy tag (len=32, offset=68_000 > 65_535).
        let expected_copy4_tag: u8 = ((32u8 - 1) << 2) | TAG_COPY_4;
        let [o0, o1, o2, o3] = 68_000u32.to_le_bytes();
        let expected_tail = [expected_copy4_tag, o0, o1, o2, o3];
        let tail_start = raw_long.len().saturating_sub(5);
        assert_eq!(raw_long.get(tail_start..), Some(expected_tail.as_slice()));
        assert_eq!(
            decompress_in(raw_long.as_slice(), 100_000, alloc).expect("dec long").as_slice(),
            long_dist.as_slice()
        );

        let framed_long = compress_framed_in(long_dist.as_slice(), alloc).expect("framed long");
        assert_eq!(
            decompress_framed_in(framed_long.as_slice(), SnappyLimits::SHIPPED, alloc)
                .expect("dec framed long")
                .as_slice(),
            long_dist.as_slice()
        );

        // Incompressible pseudo-random payload
        let mut incomp = OwnVec::with_capacity_in(4_096, alloc);
        let mut st: u32 = 0xCAFE_BABE;
        for _ in 0..4_096 {
            st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [_, _, _, hi] = st.to_le_bytes();
            incomp.push(hi);
        }
        let framed_incomp = compress_framed_in(incomp.as_slice(), alloc).expect("framed incomp");
        assert_eq!(framed_incomp.get(10).copied(), Some(CHUNK_UNCOMPRESSED));
        assert_eq!(
            decompress_framed_in(framed_incomp.as_slice(), SnappyLimits::SHIPPED, alloc)
                .expect("dec framed incomp")
                .as_slice(),
            incomp.as_slice()
        );

        // Skippable chunks (0x80..=0xFD), padding (0xFE), and unskippable refusal (0x02..=0x7F)
        let mut with_skip = OwnVec::new_in(alloc);
        with_skip.extend_from_slice(&STREAM_IDENTIFIER_CHUNK);
        with_skip.extend_from_slice(&[0x85, 0x04, 0x00, 0x00, b'S', b'K', b'I', b'P']);
        with_skip.extend_from_slice(&[0xFE, 0x02, 0x00, 0x00, 0x00, 0x00]);
        let payload = b"after skippable";
        let masked = mask_crc32c(crc32c(payload));
        let body_len = u8::try_from(payload.len() + 4).unwrap_or(0);
        with_skip.extend_from_slice(&[CHUNK_UNCOMPRESSED, body_len, 0x00, 0x00]);
        with_skip.extend_from_slice(&masked.to_le_bytes());
        with_skip.extend_from_slice(payload);
        assert_eq!(
            decompress_framed_in(with_skip.as_slice(), SnappyLimits::SHIPPED, alloc)
                .expect("skippable")
                .as_slice(),
            payload
        );

        let mut bad_unskip = OwnVec::new_in(alloc);
        bad_unskip.extend_from_slice(&STREAM_IDENTIFIER_CHUNK);
        bad_unskip.extend_from_slice(&[0x02, 0x01, 0x00, 0x00, 0xAA]);
        assert_eq!(
            decompress_framed_in(bad_unskip.as_slice(), SnappyLimits::SHIPPED, alloc),
            Err(SnappyError::UnsupportedChunk { chunk_type: 0x02 })
        );
    }

    #[test]
    fn masked_crc32c_corruption_and_varint_bomb_cap_refusal() {
        let h = heap();
        let alloc = h.alloc();

        // Standard CRC-32C check value: crc32c("123456789") == 0xE306_9283
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(mask_crc32c(crc32c(b"")), 0xa282_ead8);

        // Corrupt masked CRC-32C in a compressed chunk
        let mut rep = OwnVec::new_in(alloc);
        rep.resize(256, b'Q');
        let mut framed = compress_framed_in(rep.as_slice(), alloc).expect("framed");
        assert_eq!(framed.get(10).copied(), Some(CHUNK_COMPRESSED));
        // Masked CRC sits at bytes 14..18 (10-byte stream id + 4-byte chunk header)
        if let Some(slot) = framed.get_mut(15) {
            *slot ^= 0x80;
        }
        assert!(matches!(
            decompress_framed_in(framed.as_slice(), SnappyLimits::SHIPPED, alloc),
            Err(SnappyError::ChecksumMismatch { .. })
        ));

        // Zip-bomb varint cap refusal: varint claiming 100 MiB (104_857_600 = 0x0640_0000)
        // LEB128 of 0x0640_0000: [0x80, 0x80, 0x80, 0x32]
        let bomb_varint: [u8; 4] = [0x80, 0x80, 0x80, 0x32];
        assert!(matches!(
            decompress_in(&bomb_varint, 1_024, alloc),
            Err(SnappyError::OutputLimitExceeded { bytes: 104_857_600, limit: 1_024 })
        ));

        // Framed output byte limit refusal
        let mut big = OwnVec::new_in(alloc);
        big.resize(16_384, b'B');
        let framed_big = compress_framed_in(big.as_slice(), alloc).expect("framed big");
        let strict = SnappyLimits { most_output_bytes: 1_024, most_chunks: 64 };
        assert!(matches!(
            decompress_framed_in(framed_big.as_slice(), strict, alloc),
            Err(SnappyError::OutputLimitExceeded { limit: 1_024, .. })
        ));
    }

    fn draw_payload<A: Allocator + Copy>(tape: &Tape, min_len: usize, alloc: A) -> OwnVec<u8, A> {
        let len = tape.int_in(min_len..=512usize, "payload_len");
        let pattern = tape.int_in(0u8..=4, "pattern");
        let mut data = OwnVec::with_capacity_in(len, alloc);
        for i in 0..len {
            let b = match pattern {
                0 => tape.int_in(0u8..=255, "byte"),
                1 => b'S',
                2 => u8::try_from(i % 5).unwrap_or(0),
                3 => u8::try_from((i / 3) % 11).unwrap_or(0),
                _ => u8::try_from((i * 41 + 7) % 251).unwrap_or(0),
            };
            data.push(b);
        }
        data
    }

    /// The invariant: Snappy raw blocks and framed streams round-trip with verified masked CRC-32C and hostile bytes never panic.
    #[test]
    fn property_snappy_round_trip_and_hostile_bytes_never_panic() {
        let h = heap();
        check(Config::cases(160), |tape| {
            let alloc = h.alloc();
            let payload = draw_payload(tape, 0, alloc);

            // Raw block round-trip
            let raw = compress_in(payload.as_slice(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "raw compress: {e}").as_str().into()))?;
            let decoded_raw = decompress_in(raw.as_slice(), 4_096, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "raw decompress: {e}").as_str().into()))?;
            if decoded_raw.as_slice() != payload.as_slice() {
                return Err(Broke::Oracle("raw block round-trip mismatch".into()));
            }

            // Framed stream round-trip
            let framed = compress_framed_in(payload.as_slice(), alloc).map_err(|e| {
                Broke::Oracle(format_in!(alloc, "framed compress: {e}").as_str().into())
            })?;
            let decoded_framed =
                decompress_framed_in(framed.as_slice(), SnappyLimits::SHIPPED, alloc).map_err(
                    |e| Broke::Oracle(format_in!(alloc, "framed decompress: {e}").as_str().into()),
                )?;
            if decoded_framed.as_slice() != payload.as_slice() {
                return Err(Broke::Oracle("framed round-trip mismatch".into()));
            }

            // Hostile mutated bytes must never panic (raw + framed)
            let mut hostile = OwnVec::new_in(alloc);
            hostile.extend_from_slice(framed.as_slice());
            let muts = tape.int_in(1usize..=8usize, "mutations");
            for _ in 0..muts {
                if !hostile.is_empty() {
                    let idx = tape.index(hostile.len(), "mut_idx");
                    let xor = tape.int_in(1u8..=255, "mut_xor");
                    if let Some(slot) = hostile.get_mut(idx) {
                        *slot ^= xor;
                    }
                }
            }
            let tiny_limits = SnappyLimits { most_output_bytes: 2_048, most_chunks: 16 };
            drop(decompress_framed_in(hostile.as_slice(), tiny_limits, alloc));
            drop(decompress_in(hostile.as_slice(), 2_048, alloc));

            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_crc32c() {
        let h = heap();
        control(Config::cases(128), false, true, "", |tape, plant| {
            let alloc = h.alloc();
            let payload = draw_payload(tape, 1, alloc);
            let mut framed = compress_framed_in(payload.as_slice(), alloc).map_err(|e| {
                Broke::Oracle(format_in!(alloc, "compress_framed_in: {e}").as_str().into())
            })?;

            // Corrupt 1 byte inside the first data chunk's 4-byte masked CRC-32C (bytes 14..18).
            let crc_off = tape.int_in(0usize..=3usize, "crc_byte");
            let xor = tape.int_in(1u8..=255, "crc_xor");
            if let Some(slot) = framed.get_mut(14 + crc_off) {
                *slot ^= xor;
            }

            match decompress_framed_in_planted(
                framed.as_slice(),
                SnappyLimits::SHIPPED,
                alloc,
                plant,
            ) {
                Err(SnappyError::ChecksumMismatch { .. }) => Ok(()),
                Ok(_) => Err(Broke::Oracle(
                    "corrupted Snappy masked CRC-32C was accepted without ChecksumMismatch".into(),
                )),
                Err(other) => Err(Broke::Oracle(
                    format_in!(alloc, "expected ChecksumMismatch, got {other}").as_str().into(),
                )),
            }
        });
    }
}
