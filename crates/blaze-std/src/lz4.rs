//! Pure `#![no_std]`, strictly bounded LZ4 Raw Block and LZ4 Frame Format v1.6.4 codec.
//!
//! - **LZ4 Raw Block** (`compress_block_in`, `decompress_block_in`): 4096-entry multiplicative
//!   hash-table encoder respecting `MFLIMIT = 12` and `LASTLITERALS = 5`, paired with a safe
//!   decoder handling arbitrary overlapping matches (including RLE `offset == 1`) under a strict
//!   caller-supplied output ceiling.
//! - **LZ4 Frame Format v1.6.4** (`compress_frame_in`, `decompress_frame_in`): supports
//!   independent and dependent blocks (64 KiB rolling intra-frame history), optional block
//!   checksums, optional content checksums, optional content sizes, skippable frames
//!   (`0x184D2A50..=0x184D2A5F`), concatenated frames, and header checksum verification using
//!   [`crate::digest::Xxh32`] / [`crate::digest::xxh32`].
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexes, and
//!   zero recursion.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, Vec};

use crate::digest::{Xxh32, xxh32};

const MINMATCH: usize = 4;
const MFLIMIT: usize = 12;
const LASTLITERALS: usize = 5;
const MAX_DISTANCE: usize = 65_535;
const MAX_BLOCK_INPUT_SIZE: usize = 0x7E00_0000;

const HASH_LOG: u32 = 12;
const HASH_SIZE: usize = 1 << HASH_LOG;
const HASH_SHIFT: u32 = 32 - HASH_LOG;
const HASH_PRIME: u32 = 2_654_435_761;
const ACCEL_SHIFT: u32 = 6;

const LZ4_FRAME_MAGIC: u32 = 0x184D_2204;
const LZ4_SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
const LZ4_SKIPPABLE_MAGIC_BASE: u32 = 0x184D_2A50;

const FLG_VERSION_MASK: u8 = 0xC0;
const FLG_VERSION_01: u8 = 0x40;
const FLG_BLOCK_INDEP: u8 = 0x20;
const FLG_BLOCK_CHECKSUM: u8 = 0x10;
const FLG_CONTENT_SIZE: u8 = 0x08;
const FLG_CONTENT_CHECKSUM: u8 = 0x04;
const FLG_RESERVED: u8 = 0x02;
const FLG_DICT_ID: u8 = 0x01;

const BD_RESERVED_MASK: u8 = 0x8F;
const BLOCK_UNCOMPRESSED_BIT: u32 = 0x8000_0000;
const BLOCK_SIZE_MASK: u32 = 0x7FFF_FFFF;

/// Maximum uncompressed block size encoded in the LZ4 Frame `BD` descriptor byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockMaxSize {
    /// Code `4`: 64 KiB (`65_536` bytes).
    Kb64,
    /// Code `5`: 256 KiB (`262_144` bytes).
    Kb256,
    /// Code `6`: 1 MiB (`1_048_576` bytes).
    Mb1,
    /// Code `7`: 4 MiB (`4_194_304` bytes).
    Mb4,
}

impl BlockMaxSize {
    /// Maximum uncompressed bytes per block.
    #[inline]
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        match self {
            BlockMaxSize::Kb64 => 64 * 1024,
            BlockMaxSize::Kb256 => 256 * 1024,
            BlockMaxSize::Mb1 => 1024 * 1024,
            BlockMaxSize::Mb4 => 4 * 1024 * 1024,
        }
    }

    /// 3-bit code stored in bits `6..=4` of the LZ4 Frame `BD` byte.
    #[inline]
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            BlockMaxSize::Kb64 => 4,
            BlockMaxSize::Kb256 => 5,
            BlockMaxSize::Mb1 => 6,
            BlockMaxSize::Mb4 => 7,
        }
    }

    #[inline]
    const fn from_code(code: u8) -> Result<Self, Lz4Error> {
        match code {
            4 => Ok(BlockMaxSize::Kb64),
            5 => Ok(BlockMaxSize::Kb256),
            6 => Ok(BlockMaxSize::Mb1),
            7 => Ok(BlockMaxSize::Mb4),
            _ => Err(Lz4Error::InvalidBlockMaxSize { code }),
        }
    }
}

/// Encoding options for an LZ4 v1.6.4 frame ([`compress_frame_in`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lz4FrameOpts {
    /// Maximum uncompressed size of each independent block.
    pub block_size: BlockMaxSize,
    /// Emit a 4-byte `XXH32` checksum over the full uncompressed content after `EndMark`.
    pub content_checksum: bool,
    /// Emit a 4-byte `XXH32` checksum after each data block.
    pub block_checksum: bool,
    /// Emit the 8-byte little-endian uncompressed content size in the frame header.
    pub content_size: bool,
}

impl Lz4FrameOpts {
    /// Standard defaults: 64 KiB independent blocks, content checksum and content size enabled,
    /// per-block checksums disabled.
    pub const DEFAULT: Self = Self {
        block_size: BlockMaxSize::Kb64,
        content_checksum: true,
        block_checksum: false,
        content_size: true,
    };
}

/// Caller-supplied bounds enforced while decoding untrusted LZ4 frame streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lz4Limits {
    /// Maximum total uncompressed bytes across all frames in the stream.
    pub most_output_bytes: usize,
    /// Maximum number of frames (standard + skippable) allowed in one concatenated stream.
    pub most_frames: usize,
}

impl Lz4Limits {
    /// Generous default bounds (64 MiB output, 1,024 concatenated frames).
    pub const USUAL: Self = Self { most_output_bytes: 64 << 20, most_frames: 1_024 };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed error returned while compressing or decompressing LZ4 raw blocks or frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lz4Error {
    /// Input ended before a token, length extension, literal run, offset, header, or trailer.
    Truncated,
    /// Frame did not begin with the LZ4 frame magic (`0x184D2204`) or a skippable magic.
    BadMagic { found: u32 },
    /// Frame `FLG` version bits were not `01`.
    UnsupportedVersion { version: u8 },
    /// A reserved bit in `FLG` or `BD` was non-zero.
    ReservedBitSet,
    /// Frame `BD` block max size code was outside `4..=7`.
    InvalidBlockMaxSize { code: u8 },
    /// Frame requests a preset dictionary (`DictID` flag), which is not supported.
    UnsupportedDictionary,
    /// Frame descriptor header checksum (`HC`) did not match `(xxh32(desc, 0) >> 8) & 0xFF`.
    HeaderChecksumMismatch { expected: u8, found: u8 },
    /// Per-block `XXH32` checksum did not match the block payload.
    BlockChecksumMismatch { expected: u32, found: u32 },
    /// Trailing frame `XXH32` content checksum did not match the decoded output.
    ContentChecksumMismatch { expected: u32, found: u32 },
    /// Decoded frame byte count did not match the header's declared `Content Size`.
    ContentSizeMismatch { expected: u64, found: u64 },
    /// A match offset was zero or reached before the available block/frame history.
    InvalidOffset { offset: usize, available: usize },
    /// A frame block's payload or decoded output exceeded the frame's `Block MaxSize`.
    BlockTooLarge { size: usize, limit: usize },
    /// Decoded or declared output size exceeded the caller's byte limit.
    OutputLimitExceeded { bytes: usize, limit: usize },
    /// Concatenated stream contained more frames than [`Lz4Limits::most_frames`].
    TooManyFrames { count: usize, limit: usize },
    /// Raw block input exceeded the LZ4 raw block maximum (`0x7E00_0000` bytes).
    InputTooLarge { bytes: usize, limit: usize },
    /// Memory allocation in the caller's allocator failed.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for Lz4Error {
    #[inline]
    fn from(failure: AllocFailure) -> Self {
        Lz4Error::Alloc(failure)
    }
}

impl fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Lz4Error::Truncated => f.write_str("bstd::lz4: truncated LZ4 block or frame"),
            Lz4Error::BadMagic { found } => {
                write!(f, "bstd::lz4: unrecognized frame magic 0x{found:08x}")
            }
            Lz4Error::UnsupportedVersion { version } => {
                write!(f, "bstd::lz4: unsupported frame version {version} (expected 1)")
            }
            Lz4Error::ReservedBitSet => {
                f.write_str("bstd::lz4: reserved bit set in LZ4 frame descriptor")
            }
            Lz4Error::InvalidBlockMaxSize { code } => {
                write!(f, "bstd::lz4: invalid block max size code {code} (expected 4..=7)")
            }
            Lz4Error::UnsupportedDictionary => {
                f.write_str("bstd::lz4: preset dictionaries (DictID) are not supported")
            }
            Lz4Error::HeaderChecksumMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::lz4: frame header checksum mismatch (expected 0x{expected:02x}, found 0x{found:02x})"
                )
            }
            Lz4Error::BlockChecksumMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::lz4: block XXH32 mismatch (expected 0x{expected:08x}, computed 0x{found:08x})"
                )
            }
            Lz4Error::ContentChecksumMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::lz4: content XXH32 mismatch (expected 0x{expected:08x}, computed 0x{found:08x})"
                )
            }
            Lz4Error::ContentSizeMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::lz4: content size mismatch (header declared {expected} bytes, decoded {found})"
                )
            }
            Lz4Error::InvalidOffset { offset, available } => {
                write!(
                    f,
                    "bstd::lz4: invalid match offset {offset} with only {available} history bytes available"
                )
            }
            Lz4Error::BlockTooLarge { size, limit } => {
                write!(f, "bstd::lz4: block size {size} exceeds frame block max size {limit}")
            }
            Lz4Error::OutputLimitExceeded { bytes, limit } => {
                write!(f, "bstd::lz4: output size {bytes} exceeds caller limit {limit}")
            }
            Lz4Error::TooManyFrames { count, limit } => {
                write!(f, "bstd::lz4: stream frame count {count} exceeds limit {limit}")
            }
            Lz4Error::InputTooLarge { bytes, limit } => {
                write!(f, "bstd::lz4: raw block input size {bytes} exceeds maximum {limit}")
            }
            Lz4Error::Alloc(failure) => write!(f, "bstd::lz4: {failure}"),
        }
    }
}

impl core::error::Error for Lz4Error {}

#[inline]
fn push_byte<A: Allocator>(out: &mut Vec<u8, A>, byte: u8) -> Result<(), Lz4Error> {
    out.try_push(byte).map_err(|refused| Lz4Error::Alloc(refused.why))
}

#[inline]
fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, Lz4Error> {
    let end = offset.checked_add(2).ok_or(Lz4Error::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(Lz4Error::Truncated)?;
    let arr: [u8; 2] = slice.try_into().map_err(|_| Lz4Error::Truncated)?;
    Ok(u16::from_le_bytes(arr))
}

#[inline]
fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, Lz4Error> {
    let end = offset.checked_add(4).ok_or(Lz4Error::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(Lz4Error::Truncated)?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| Lz4Error::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

#[inline]
fn read_u64_le(bytes: &[u8], offset: usize) -> Result<u64, Lz4Error> {
    let end = offset.checked_add(8).ok_or(Lz4Error::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(Lz4Error::Truncated)?;
    let arr: [u8; 8] = slice.try_into().map_err(|_| Lz4Error::Truncated)?;
    Ok(u64::from_le_bytes(arr))
}

#[inline]
fn read_u32_le_opt(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = bytes.get(offset..end)?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

#[inline]
fn lz4_hash(seq: u32) -> usize {
    let shifted = seq.wrapping_mul(HASH_PRIME) >> HASH_SHIFT;
    usize::try_from(shifted).unwrap_or(0)
}

#[inline]
fn header_checksum_byte(descriptor: &[u8]) -> u8 {
    let [_, hc, _, _] = xxh32(descriptor, 0).to_le_bytes();
    hc
}

fn emit_extra_len<A: Allocator>(out: &mut Vec<u8, A>, mut rem: usize) -> Result<(), Lz4Error> {
    while rem >= 255 {
        push_byte(out, 255)?;
        rem -= 255;
    }
    let tail = u8::try_from(rem).unwrap_or(0);
    push_byte(out, tail)
}

fn emit_final_literals<A: Allocator>(out: &mut Vec<u8, A>, lits: &[u8]) -> Result<(), Lz4Error> {
    let lit_len = lits.len();
    let token_lit = if lit_len >= 15 { 15u8 } else { u8::try_from(lit_len).unwrap_or(15) };
    push_byte(out, token_lit << 4)?;
    if lit_len >= 15 {
        emit_extra_len(out, lit_len - 15)?;
    }
    out.try_extend_from_slice(lits)?;
    Ok(())
}

/// Compress `input` into a raw LZ4 block allocated in `alloc`.
///
/// Uses a 4096-entry 4-byte hash table and honors `MFLIMIT = 12` and `LASTLITERALS = 5`.
pub fn compress_block_in<A: Allocator + Copy>(
    input: &[u8],
    alloc: A,
) -> Result<Vec<u8, A>, Lz4Error> {
    if input.is_empty() {
        return Ok(Vec::new_in(alloc));
    }
    if input.len() > MAX_BLOCK_INPUT_SIZE {
        return Err(Lz4Error::InputTooLarge { bytes: input.len(), limit: MAX_BLOCK_INPUT_SIZE });
    }

    let mut out = Vec::new_in(alloc);
    if input.len() <= MFLIMIT {
        emit_final_literals(&mut out, input)?;
        return Ok(out);
    }

    let mut table = Vec::try_with_capacity_in(HASH_SIZE, alloc)?;
    let zeros = [0u32; 256];
    for _ in 0..(HASH_SIZE / zeros.len()) {
        table.try_extend_from_slice(&zeros)?;
    }

    let mf_limit = input.len() - MFLIMIT;
    let match_limit = input.len() - LASTLITERALS;

    if let Some(seq0) = read_u32_le_opt(input, 0)
        && let Some(slot) = table.get_mut(lz4_hash(seq0))
    {
        *slot = 0;
    }

    let mut anchor: usize = 0;
    let mut ip: usize = 1;

    while ip <= mf_limit {
        let mut found: Option<usize> = None;
        let mut search_match_nb: usize = 1usize << ACCEL_SHIFT;

        while ip <= mf_limit {
            let Some(cur_seq) = read_u32_le_opt(input, ip) else {
                break;
            };
            let h = lz4_hash(cur_seq);
            let prev = match table.get_mut(h) {
                Some(slot) => {
                    let prev_u32 = *slot;
                    *slot = u32::try_from(ip).unwrap_or(0);
                    usize::try_from(prev_u32).unwrap_or(0)
                }
                None => break,
            };

            if prev < ip
                && ip - prev <= MAX_DISTANCE
                && read_u32_le_opt(input, prev) == Some(cur_seq)
            {
                found = Some(prev);
                break;
            }

            let step = search_match_nb >> ACCEL_SHIFT;
            search_match_nb = search_match_nb.saturating_add(1);
            ip = ip.saturating_add(step);
        }

        let Some(mut cand) = found else {
            break;
        };

        // Extend match backwards into pending literals while preserving distance.
        while ip > anchor && cand > 0 && input.get(ip - 1) == input.get(cand - 1) {
            ip -= 1;
            cand -= 1;
        }

        let lit_len = ip - anchor;
        let offset = ip - cand;
        let offset_u16 = u16::try_from(offset).map_err(|_| Lz4Error::InvalidOffset {
            offset,
            available: ip,
        })?;

        let mut match_len = MINMATCH;
        while ip + match_len < match_limit
            && input.get(cand + match_len) == input.get(ip + match_len)
        {
            match_len += 1;
        }

        let ml_code = match_len - MINMATCH;
        let token_lit = if lit_len >= 15 { 15u8 } else { u8::try_from(lit_len).unwrap_or(15) };
        let token_ml = if ml_code >= 15 { 15u8 } else { u8::try_from(ml_code).unwrap_or(15) };
        push_byte(&mut out, (token_lit << 4) | token_ml)?;

        if lit_len >= 15 {
            emit_extra_len(&mut out, lit_len - 15)?;
        }
        let lits = input.get(anchor..ip).ok_or(Lz4Error::Truncated)?;
        out.try_extend_from_slice(lits)?;

        out.try_extend_from_slice(&offset_u16.to_le_bytes())?;
        if ml_code >= 15 {
            emit_extra_len(&mut out, ml_code - 15)?;
        }

        ip += match_len;
        anchor = ip;
        if ip > mf_limit {
            break;
        }

        if let Some(prev_idx) = ip.checked_sub(2)
            && let Some(prev_seq) = read_u32_le_opt(input, prev_idx)
            && let Some(slot) = table.get_mut(lz4_hash(prev_seq))
        {
            *slot = u32::try_from(prev_idx).unwrap_or(0);
        }
    }

    let tail = input.get(anchor..).ok_or(Lz4Error::Truncated)?;
    emit_final_literals(&mut out, tail)?;
    Ok(out)
}

fn check_output_bounds(
    current_len: usize,
    add: usize,
    block_start: usize,
    block_max_size: Option<usize>,
    total_limit: usize,
) -> Result<(), Lz4Error> {
    let Some(new_len) = current_len.checked_add(add) else {
        return Err(Lz4Error::OutputLimitExceeded { bytes: usize::MAX, limit: total_limit });
    };
    if let Some(max_block) = block_max_size {
        let block_written = new_len.saturating_sub(block_start);
        if block_written > max_block {
            return Err(Lz4Error::BlockTooLarge { size: block_written, limit: max_block });
        }
    }
    if new_len > total_limit {
        return Err(Lz4Error::OutputLimitExceeded { bytes: new_len, limit: total_limit });
    }
    Ok(())
}

fn copy_match_safe<A: Allocator>(
    out: &mut Vec<u8, A>,
    offset: usize,
    match_len: usize,
) -> Result<(), Lz4Error> {
    out.try_reserve(match_len)?;
    if offset == 1 {
        let start = out.len().checked_sub(1).ok_or(Lz4Error::InvalidOffset {
            offset: 1,
            available: 0,
        })?;
        let byte = *out.get(start).ok_or(Lz4Error::InvalidOffset {
            offset: 1,
            available: 0,
        })?;
        let fill = [byte; 64];
        let mut remaining = match_len;
        while remaining >= fill.len() {
            out.try_extend_from_slice(&fill)?;
            remaining -= fill.len();
        }
        if remaining > 0 {
            let tail = fill.get(..remaining).ok_or(Lz4Error::Truncated)?;
            out.try_extend_from_slice(tail)?;
        }
        return Ok(());
    }

    let mut scratch = [0u8; 64];
    let mut remaining = match_len;
    while remaining > 0 {
        let step = remaining.min(offset).min(scratch.len());
        let src_start = out.len().checked_sub(offset).ok_or(Lz4Error::InvalidOffset {
            offset,
            available: out.len(),
        })?;
        let src_end = src_start.checked_add(step).ok_or(Lz4Error::Truncated)?;
        let src = out.get(src_start..src_end).ok_or(Lz4Error::InvalidOffset {
            offset,
            available: out.len(),
        })?;
        let dst = scratch.get_mut(..step).ok_or(Lz4Error::Truncated)?;
        dst.copy_from_slice(src);
        let chunk = scratch.get(..step).ok_or(Lz4Error::Truncated)?;
        out.try_extend_from_slice(chunk)?;
        remaining -= step;
    }
    Ok(())
}

fn decompress_block_into<A: Allocator>(
    input: &[u8],
    out: &mut Vec<u8, A>,
    history_base: usize,
    block_max_size: Option<usize>,
    total_limit: usize,
) -> Result<(), Lz4Error> {
    let block_start = out.len();
    let mut pos: usize = 0;

    while pos < input.len() {
        let token = *input.get(pos).ok_or(Lz4Error::Truncated)?;
        pos += 1;

        let mut lit_len = usize::from(token >> 4);
        if lit_len == 15 {
            loop {
                let extra = *input.get(pos).ok_or(Lz4Error::Truncated)?;
                pos += 1;
                lit_len = lit_len.checked_add(usize::from(extra)).ok_or(
                    Lz4Error::OutputLimitExceeded { bytes: usize::MAX, limit: total_limit },
                )?;
                check_output_bounds(out.len(), lit_len, block_start, block_max_size, total_limit)?;
                if extra < 255 {
                    break;
                }
            }
        } else {
            check_output_bounds(out.len(), lit_len, block_start, block_max_size, total_limit)?;
        }

        let lit_end = pos.checked_add(lit_len).ok_or(Lz4Error::Truncated)?;
        let lits = input.get(pos..lit_end).ok_or(Lz4Error::Truncated)?;
        out.try_extend_from_slice(lits)?;
        pos = lit_end;

        // Per LZ4 block spec, the final sequence ends immediately after its literals.
        if pos == input.len() {
            break;
        }

        let offset = usize::from(read_u16_le(input, pos)?);
        pos += 2;
        let available = out.len().saturating_sub(history_base);
        if offset == 0 || offset > available {
            return Err(Lz4Error::InvalidOffset { offset, available });
        }

        let ml_nibble = token & 0x0F;
        let mut match_len = usize::from(ml_nibble) + MINMATCH;
        if ml_nibble == 15 {
            loop {
                let extra = *input.get(pos).ok_or(Lz4Error::Truncated)?;
                pos += 1;
                match_len = match_len.checked_add(usize::from(extra)).ok_or(
                    Lz4Error::OutputLimitExceeded { bytes: usize::MAX, limit: total_limit },
                )?;
                check_output_bounds(
                    out.len(),
                    match_len,
                    block_start,
                    block_max_size,
                    total_limit,
                )?;
                if extra < 255 {
                    break;
                }
            }
        } else {
            check_output_bounds(out.len(), match_len, block_start, block_max_size, total_limit)?;
        }

        copy_match_safe(out, offset, match_len)?;
    }

    Ok(())
}

/// Decompress a raw LZ4 block into `alloc`, refusing any output larger than `max_output_bytes`.
pub fn decompress_block_in<A: Allocator + Copy>(
    input: &[u8],
    max_output_bytes: usize,
    alloc: A,
) -> Result<Vec<u8, A>, Lz4Error> {
    let mut out = Vec::new_in(alloc);
    decompress_block_into(input, &mut out, 0, None, max_output_bytes)?;
    Ok(out)
}

/// Compress `input` into a standard LZ4 v1.6.4 frame allocated in `alloc`.
///
/// Emits independent blocks, automatically choosing uncompressed storage (`0x8000_0000` flag)
/// whenever a block is incompressible, and computes all header, block, and content checksums
/// with [`crate::digest::xxh32`].
pub fn compress_frame_in<A: Allocator + Copy>(
    input: &[u8],
    opts: Lz4FrameOpts,
    alloc: A,
) -> Result<Vec<u8, A>, Lz4Error> {
    let mut out = Vec::new_in(alloc);
    out.try_extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes())?;

    let mut flg: u8 = FLG_VERSION_01 | FLG_BLOCK_INDEP;
    if opts.block_checksum {
        flg |= FLG_BLOCK_CHECKSUM;
    }
    if opts.content_size {
        flg |= FLG_CONTENT_SIZE;
    }
    if opts.content_checksum {
        flg |= FLG_CONTENT_CHECKSUM;
    }
    let bd: u8 = opts.block_size.code() << 4;

    let mut desc = [0u8; 10];
    let mut desc_len: usize = 2;
    if let Some(slot) = desc.get_mut(0) {
        *slot = flg;
    }
    if let Some(slot) = desc.get_mut(1) {
        *slot = bd;
    }
    if opts.content_size {
        let len_u64 = u64::try_from(input.len()).map_err(|_| Lz4Error::InputTooLarge {
            bytes: input.len(),
            limit: usize::MAX,
        })?;
        if let Some(dst) = desc.get_mut(2..10) {
            dst.copy_from_slice(&len_u64.to_le_bytes());
        }
        desc_len = 10;
    }

    let desc_slice = desc.get(..desc_len).ok_or(Lz4Error::Truncated)?;
    let hc = header_checksum_byte(desc_slice);
    out.try_extend_from_slice(desc_slice)?;
    push_byte(&mut out, hc)?;

    let block_max = opts.block_size.max_bytes();
    for chunk in input.chunks(block_max) {
        let compressed = compress_block_in(chunk, alloc)?;
        let (header_u32, payload): (u32, &[u8]) = if compressed.len() < chunk.len() {
            let size_u32 = u32::try_from(compressed.len()).map_err(|_| Lz4Error::BlockTooLarge {
                size: compressed.len(),
                limit: block_max,
            })?;
            (size_u32, compressed.as_slice())
        } else {
            let size_u32 = u32::try_from(chunk.len()).map_err(|_| Lz4Error::BlockTooLarge {
                size: chunk.len(),
                limit: block_max,
            })?;
            (BLOCK_UNCOMPRESSED_BIT | size_u32, chunk)
        };

        out.try_extend_from_slice(&header_u32.to_le_bytes())?;
        out.try_extend_from_slice(payload)?;
        if opts.block_checksum {
            let block_crc = xxh32(payload, 0);
            out.try_extend_from_slice(&block_crc.to_le_bytes())?;
        }
    }

    // EndMark
    out.try_extend_from_slice(&0u32.to_le_bytes())?;

    if opts.content_checksum {
        let content_crc = xxh32(input, 0);
        out.try_extend_from_slice(&content_crc.to_le_bytes())?;
    }

    Ok(out)
}

/// Decompress one or more concatenated LZ4 v1.6.4 frames (and skippable frames) into `alloc`,
/// enforcing `limits` and verifying every header, block, and content checksum.
pub fn decompress_frame_in<A: Allocator + Copy>(
    input: &[u8],
    limits: Lz4Limits,
    alloc: A,
) -> Result<Vec<u8, A>, Lz4Error> {
    decompress_frame_in_planted(input, limits, alloc, false)
}

fn decompress_frame_in_planted<A: Allocator + Copy>(
    input: &[u8],
    limits: Lz4Limits,
    alloc: A,
    plant_skip_checksum: bool,
) -> Result<Vec<u8, A>, Lz4Error> {
    if input.is_empty() {
        return Err(Lz4Error::Truncated);
    }

    let mut out = Vec::new_in(alloc);
    let mut pos: usize = 0;
    let mut frames: usize = 0;

    while pos < input.len() {
        frames = frames.saturating_add(1);
        if frames > limits.most_frames {
            return Err(Lz4Error::TooManyFrames { count: frames, limit: limits.most_frames });
        }

        let magic = read_u32_le(input, pos)?;
        pos += 4;

        if (magic & LZ4_SKIPPABLE_MAGIC_MASK) == LZ4_SKIPPABLE_MAGIC_BASE {
            let skip_len_u32 = read_u32_le(input, pos)?;
            pos += 4;
            let skip_len = usize::try_from(skip_len_u32).map_err(|_| Lz4Error::Truncated)?;
            let next_pos = pos.checked_add(skip_len).ok_or(Lz4Error::Truncated)?;
            if next_pos > input.len() {
                return Err(Lz4Error::Truncated);
            }
            pos = next_pos;
            continue;
        }

        if magic != LZ4_FRAME_MAGIC {
            return Err(Lz4Error::BadMagic { found: magic });
        }

        let desc_start = pos;
        let flg = *input.get(pos).ok_or(Lz4Error::Truncated)?;
        let bd = *input.get(pos + 1).ok_or(Lz4Error::Truncated)?;
        pos += 2;

        if (flg & FLG_VERSION_MASK) != FLG_VERSION_01 {
            return Err(Lz4Error::UnsupportedVersion { version: flg >> 6 });
        }
        if (flg & FLG_RESERVED) != 0 || (bd & BD_RESERVED_MASK) != 0 {
            return Err(Lz4Error::ReservedBitSet);
        }

        let block_indep = (flg & FLG_BLOCK_INDEP) != 0;
        let has_block_checksum = (flg & FLG_BLOCK_CHECKSUM) != 0;
        let has_content_size = (flg & FLG_CONTENT_SIZE) != 0;
        let has_content_checksum = (flg & FLG_CONTENT_CHECKSUM) != 0;
        let has_dict_id = (flg & FLG_DICT_ID) != 0;

        let block_max_size = BlockMaxSize::from_code((bd >> 4) & 0x07)?.max_bytes();

        let declared_content_size = if has_content_size {
            let sz = read_u64_le(input, pos)?;
            pos += 8;
            Some(sz)
        } else {
            None
        };

        if has_dict_id {
            // Advance past DictID before validating header checksum, then refuse preset dict.
            let _dict_id = read_u32_le(input, pos)?;
            pos += 4;
        }

        let desc_bytes = input.get(desc_start..pos).ok_or(Lz4Error::Truncated)?;
        let expected_hc = header_checksum_byte(desc_bytes);
        let found_hc = *input.get(pos).ok_or(Lz4Error::Truncated)?;
        pos += 1;
        if found_hc != expected_hc {
            return Err(Lz4Error::HeaderChecksumMismatch {
                expected: expected_hc,
                found: found_hc,
            });
        }

        if has_dict_id {
            return Err(Lz4Error::UnsupportedDictionary);
        }

        if let Some(declared_u64) = declared_content_size {
            let declared_usize =
                usize::try_from(declared_u64).map_err(|_| Lz4Error::OutputLimitExceeded {
                    bytes: usize::MAX,
                    limit: limits.most_output_bytes,
                })?;
            let total_needed = out.len().checked_add(declared_usize).ok_or(
                Lz4Error::OutputLimitExceeded {
                    bytes: usize::MAX,
                    limit: limits.most_output_bytes,
                },
            )?;
            if total_needed > limits.most_output_bytes {
                return Err(Lz4Error::OutputLimitExceeded {
                    bytes: total_needed,
                    limit: limits.most_output_bytes,
                });
            }
        }

        let frame_start = out.len();
        let mut content_hasher = Xxh32::new(0);

        loop {
            let block_header = read_u32_le(input, pos)?;
            pos += 4;
            if block_header == 0 {
                break;
            }

            let is_uncompressed = (block_header & BLOCK_UNCOMPRESSED_BIT) != 0;
            let block_len = usize::try_from(block_header & BLOCK_SIZE_MASK)
                .map_err(|_| Lz4Error::Truncated)?;
            if block_len > block_max_size {
                return Err(Lz4Error::BlockTooLarge { size: block_len, limit: block_max_size });
            }

            let block_end = pos.checked_add(block_len).ok_or(Lz4Error::Truncated)?;
            let block_data = input.get(pos..block_end).ok_or(Lz4Error::Truncated)?;
            pos = block_end;

            if has_block_checksum {
                let expected_block_crc = read_u32_le(input, pos)?;
                pos += 4;
                let actual_block_crc = xxh32(block_data, 0);
                if actual_block_crc != expected_block_crc {
                    return Err(Lz4Error::BlockChecksumMismatch {
                        expected: expected_block_crc,
                        found: actual_block_crc,
                    });
                }
            }

            let block_out_start = out.len();
            if is_uncompressed {
                check_output_bounds(
                    out.len(),
                    block_len,
                    block_out_start,
                    Some(block_max_size),
                    limits.most_output_bytes,
                )?;
                out.try_extend_from_slice(block_data)?;
            } else {
                let history_base = if block_indep { block_out_start } else { frame_start };
                decompress_block_into(
                    block_data,
                    &mut out,
                    history_base,
                    Some(block_max_size),
                    limits.most_output_bytes,
                )?;
            }

            if has_content_checksum {
                let written = out.get(block_out_start..).ok_or(Lz4Error::Truncated)?;
                content_hasher.update(written);
            }
        }

        if has_content_checksum {
            let expected_content_crc = read_u32_le(input, pos)?;
            pos += 4;
            let actual_content_crc = content_hasher.finish();
            if !plant_skip_checksum && actual_content_crc != expected_content_crc {
                return Err(Lz4Error::ContentChecksumMismatch {
                    expected: expected_content_crc,
                    found: actual_content_crc,
                });
            }
        }

        if let Some(declared_u64) = declared_content_size {
            let decoded_frame = out.len().saturating_sub(frame_start);
            let decoded_u64 = u64::try_from(decoded_frame).unwrap_or(u64::MAX);
            if decoded_u64 != declared_u64 {
                return Err(Lz4Error::ContentSizeMismatch {
                    expected: declared_u64,
                    found: decoded_u64,
                });
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::{
        BlockMaxSize, FLG_CONTENT_CHECKSUM, FLG_VERSION_01, LZ4_FRAME_MAGIC, Lz4Error,
        Lz4FrameOpts, Lz4Limits, compress_block_in, compress_frame_in, decompress_block_in,
        decompress_frame_in, decompress_frame_in_planted, header_checksum_byte,
    };
    use crate::digest::xxh32;

    #[test]
    fn raw_block_round_trip_and_rle_overlapping_match() {
        let h = heap();
        let alloc = h.alloc();

        // Empty
        let comp_empty = compress_block_in(b"", alloc).expect("compress empty");
        assert!(comp_empty.is_empty());
        let decomp_empty = decompress_block_in(comp_empty.as_slice(), 1024, alloc).expect("empty");
        assert!(decomp_empty.is_empty());

        // Tiny (< MFLIMIT)
        let tiny = b"hello lz4!";
        let comp_tiny = compress_block_in(tiny, alloc).expect("compress tiny");
        let decomp_tiny = decompress_block_in(comp_tiny.as_slice(), 1024, alloc).expect("tiny");
        assert_eq!(decomp_tiny.as_slice(), tiny);

        // Overlapping RLE match (offset == 1)
        let mut rle = OwnVec::new_in(alloc);
        rle.resize(8_192, b'Z');
        let comp_rle = compress_block_in(rle.as_slice(), alloc).expect("compress rle");
        assert!(comp_rle.len() < 128);
        let decomp_rle =
            decompress_block_in(comp_rle.as_slice(), 16_384, alloc).expect("decompress rle");
        assert_eq!(decomp_rle.as_slice(), rle.as_slice());

        // Hand-crafted RLE block: 1 literal 'X', offset 1, match_len = 4 + 15 + 255 + 10 = 284,
        // followed by 5 trailing literals "12345".
        let valid_rle: [u8; 12] = [
            0x1F, b'X', 0x01, 0x00, 0xFF, 10, 0x50, b'1', b'2', b'3', b'4', b'5',
        ];
        let decoded = decompress_block_in(&valid_rle, 1024, alloc).expect("hand rle");
        assert_eq!(decoded.len(), 1 + 284 + 5);
        assert!(decoded.get(..285).expect("x").iter().all(|&b| b == b'X'));
        assert_eq!(decoded.get(285..).expect("tail"), b"12345");
    }

    #[test]
    fn frame_all_flags_multiblock_incompressible_and_skippable() {
        let h = heap();
        let alloc = h.alloc();

        // Multi-block (> 64 KiB) repetitive payload
        let mut big = OwnVec::new_in(alloc);
        for i in 0usize..2_500 {
            big.extend_from_slice(b"blaze::lz4 frame compression test line ");
            let idx = u32::try_from(i).unwrap_or(0).to_le_bytes();
            big.extend_from_slice(&idx);
            big.push(b'\n');
        }
        assert!(big.len() > 65_536);

        // Pseudo-random incompressible payload (LCG)
        let mut incomp = OwnVec::with_capacity_in(4_096, alloc);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..4_096 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [_, _, _, hi] = state.to_le_bytes();
            incomp.push(hi);
        }

        let block_sizes = [
            BlockMaxSize::Kb64,
            BlockMaxSize::Kb256,
            BlockMaxSize::Mb1,
            BlockMaxSize::Mb4,
        ];
        for &block_size in &block_sizes {
            for &content_checksum in &[false, true] {
                for &block_checksum in &[false, true] {
                    for &content_size in &[false, true] {
                        let opts = Lz4FrameOpts {
                            block_size,
                            content_checksum,
                            block_checksum,
                            content_size,
                        };
                        for payload in [b"".as_slice(), incomp.as_slice(), big.as_slice()] {
                            let framed = compress_frame_in(payload, opts, alloc).expect("compress");
                            let out = decompress_frame_in(
                                framed.as_slice(),
                                Lz4Limits::SHIPPED,
                                alloc,
                            )
                            .expect("decompress");
                            assert_eq!(out.as_slice(), payload);
                        }
                    }
                }
            }
        }

        // Skippable frame + two concatenated standard frames
        let f1 = compress_frame_in(b"first frame ", Lz4FrameOpts::DEFAULT, alloc).expect("f1");
        let f2 = compress_frame_in(b"second frame!", Lz4FrameOpts::DEFAULT, alloc).expect("f2");
        let mut concat = OwnVec::new_in(alloc);
        concat.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
        concat.extend_from_slice(&5u32.to_le_bytes());
        concat.extend_from_slice(b"SKIP!");
        concat.extend_from_slice(f1.as_slice());
        concat.extend_from_slice(f2.as_slice());
        let joined = decompress_frame_in(concat.as_slice(), Lz4Limits::SHIPPED, alloc)
            .expect("concat frames");
        assert_eq!(joined.as_slice(), b"first frame second frame!");
    }

    #[test]
    fn dependent_block_uses_prior_frame_history_and_indep_refuses() {
        let h = heap();
        let alloc = h.alloc();

        // Build a 2-block frame where Block 1 emits "ABCDEFGH" (uncompressed) and Block 2 is a
        // compressed block whose match references offset 8 (inside Block 1) for 8 bytes!
        let build_frame = |indep: bool| {
            let mut frame = OwnVec::new_in(alloc);
            frame.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());
            let flg = FLG_VERSION_01
                | if indep { super::FLG_BLOCK_INDEP } else { 0 }
                | FLG_CONTENT_CHECKSUM;
            let bd = BlockMaxSize::Kb64.code() << 4;
            let desc = [flg, bd];
            let hc = header_checksum_byte(&desc);
            frame.extend_from_slice(&desc);
            frame.push(hc);

            // Block 1: uncompressed "ABCDEFGH" (8 bytes)
            frame.extend_from_slice(&(0x8000_0000u32 | 8u32).to_le_bytes());
            frame.extend_from_slice(b"ABCDEFGH");

            // Block 2: compressed block with 0 literals, offset = 8, match_len = 8 (nibble = 4)
            let block2: [u8; 3] = [0x04, 0x08, 0x00];
            frame.extend_from_slice(&3u32.to_le_bytes());
            frame.extend_from_slice(&block2);

            // EndMark
            frame.extend_from_slice(&0u32.to_le_bytes());
            // Content checksum of "ABCDEFGHABCDEFGH"
            let crc = xxh32(b"ABCDEFGHABCDEFGH", 0);
            frame.extend_from_slice(&crc.to_le_bytes());
            frame
        };

        let dep_frame = build_frame(false);
        let decoded =
            decompress_frame_in(dep_frame.as_slice(), Lz4Limits::SHIPPED, alloc).expect("dep");
        assert_eq!(decoded.as_slice(), b"ABCDEFGHABCDEFGH");

        let indep_frame = build_frame(true);
        assert!(matches!(
            decompress_frame_in(indep_frame.as_slice(), Lz4Limits::SHIPPED, alloc),
            Err(Lz4Error::InvalidOffset { offset: 8, available: 0 })
        ));
    }

    #[test]
    fn checksum_corruption_and_bomb_cap_refusal() {
        let h = heap();
        let alloc = h.alloc();

        let opts = Lz4FrameOpts {
            block_size: BlockMaxSize::Kb64,
            content_checksum: true,
            block_checksum: true,
            content_size: true,
        };
        let framed = compress_frame_in(b"integrity verified payload 1234567890", opts, alloc)
            .expect("compress");

        // Corrupt header checksum byte (offset 4 magic + 10 descriptor = 14)
        let mut bad_hc = OwnVec::new_in(alloc);
        bad_hc.extend_from_slice(framed.as_slice());
        if let Some(slot) = bad_hc.get_mut(14) {
            *slot ^= 0xFF;
        }
        assert!(matches!(
            decompress_frame_in(bad_hc.as_slice(), Lz4Limits::SHIPPED, alloc),
            Err(Lz4Error::HeaderChecksumMismatch { .. })
        ));

        // Corrupt block checksum (immediately precedes 4-byte EndMark + 4-byte content checksum)
        let mut bad_bc = OwnVec::new_in(alloc);
        bad_bc.extend_from_slice(framed.as_slice());
        let bc_pos = bad_bc.len().saturating_sub(9);
        if let Some(slot) = bad_bc.get_mut(bc_pos) {
            *slot ^= 0x01;
        }
        assert!(matches!(
            decompress_frame_in(bad_bc.as_slice(), Lz4Limits::SHIPPED, alloc),
            Err(Lz4Error::BlockChecksumMismatch { .. })
        ));

        // Corrupt content checksum (last byte)
        let mut bad_cc = OwnVec::new_in(alloc);
        bad_cc.extend_from_slice(framed.as_slice());
        if let Some(last) = bad_cc.last_mut() {
            *last ^= 0x01;
        }
        assert!(matches!(
            decompress_frame_in(bad_cc.as_slice(), Lz4Limits::SHIPPED, alloc),
            Err(Lz4Error::ContentChecksumMismatch { .. })
        ));

        // Bomb cap refusal (raw block AND frame)
        let mut bomb = OwnVec::new_in(alloc);
        bomb.resize(32_768, b'A');
        let comp_block = compress_block_in(bomb.as_slice(), alloc).expect("comp bomb block");
        assert!(matches!(
            decompress_block_in(comp_block.as_slice(), 1_024, alloc),
            Err(Lz4Error::OutputLimitExceeded { limit: 1_024, .. })
        ));

        let comp_frame = compress_frame_in(
            bomb.as_slice(),
            Lz4FrameOpts {
                block_size: BlockMaxSize::Kb64,
                content_checksum: true,
                block_checksum: false,
                content_size: false,
            },
            alloc,
        )
        .expect("comp bomb frame");
        let strict = Lz4Limits { most_output_bytes: 1_024, most_frames: 4 };
        assert!(matches!(
            decompress_frame_in(comp_frame.as_slice(), strict, alloc),
            Err(Lz4Error::OutputLimitExceeded { limit: 1_024, .. })
        ));
    }

    fn draw_payload<A: Allocator + Copy>(tape: &Tape, min_len: usize, alloc: A) -> OwnVec<u8, A> {
        let len = tape.int_in(min_len..=512usize, "payload_len");
        let pattern = tape.int_in(0u8..=4, "pattern");
        let mut data = OwnVec::with_capacity_in(len, alloc);
        for i in 0..len {
            let b = match pattern {
                0 => tape.int_in(0u8..=255, "byte"),
                1 => b'K',
                2 => u8::try_from(i % 7).unwrap_or(0),
                3 => u8::try_from((i / 4) % 13).unwrap_or(0),
                _ => u8::try_from((i * 37 + 11) % 251).unwrap_or(0),
            };
            data.push(b);
        }
        data
    }

    /// The invariant: LZ4 raw blocks and v1.6.4 frames round-trip with verified XXH32 checksums and hostile bytes never panic.
    #[test]
    fn property_lz4_round_trip_and_hostile_bytes_never_panic() {
        let h = heap();
        check(Config::cases(160), |tape| {
            let alloc = h.alloc();
            let payload = draw_payload(tape, 0, alloc);

            // Raw block round-trip
            let block = compress_block_in(payload.as_slice(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "block compress: {e}").as_str().into()))?;
            let decoded_block = decompress_block_in(block.as_slice(), 4_096, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "block decompress: {e}").as_str().into()))?;
            if decoded_block.as_slice() != payload.as_slice() {
                return Err(Broke::Oracle("raw block round-trip mismatch".into()));
            }

            // Frame round-trip with drawn options
            let opts = Lz4FrameOpts {
                block_size: match tape.int_in(0u8..=3, "block_size") {
                    0 => BlockMaxSize::Kb64,
                    1 => BlockMaxSize::Kb256,
                    2 => BlockMaxSize::Mb1,
                    _ => BlockMaxSize::Mb4,
                },
                content_checksum: tape.coin("content_checksum"),
                block_checksum: tape.coin("block_checksum"),
                content_size: tape.coin("content_size"),
            };
            let framed = compress_frame_in(payload.as_slice(), opts, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "frame compress: {e}").as_str().into()))?;
            let decoded_frame = decompress_frame_in(framed.as_slice(), Lz4Limits::SHIPPED, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "frame decompress: {e}").as_str().into()))?;
            if decoded_frame.as_slice() != payload.as_slice() {
                return Err(Broke::Oracle("frame round-trip mismatch".into()));
            }

            // Hostile mutated bytes must never panic (block + frame)
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
            let tiny_limits = Lz4Limits { most_output_bytes: 2_048, most_frames: 8 };
            drop(decompress_frame_in(hostile.as_slice(), tiny_limits, alloc));
            drop(decompress_block_in(hostile.as_slice(), 2_048, alloc));

            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_frame_checksum() {
        let h = heap();
        control(Config::cases(128), false, true, "", |tape, plant| {
            let alloc = h.alloc();
            let payload = draw_payload(tape, 1, alloc);
            let mut framed = compress_frame_in(payload.as_slice(), Lz4FrameOpts::DEFAULT, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "compress: {e}").as_str().into()))?;

            // Corrupt 1 byte of the trailing 4-byte XXH32 content checksum.
            let tail_offset = tape.int_in(0usize..=3usize, "crc_byte");
            let xor = tape.int_in(1u8..=255, "crc_xor");
            let pos = framed.len().saturating_sub(4).saturating_add(tail_offset);
            if let Some(slot) = framed.get_mut(pos) {
                *slot ^= xor;
            }

            match decompress_frame_in_planted(
                framed.as_slice(),
                Lz4Limits::SHIPPED,
                alloc,
                plant,
            ) {
                Err(Lz4Error::ContentChecksumMismatch { .. }) => Ok(()),
                Ok(_) => Err(Broke::Oracle(
                    "corrupted LZ4 frame checksum was accepted without ContentChecksumMismatch"
                        .into(),
                )),
                Err(other) => Err(Broke::Oracle(
                    format_in!(alloc, "expected ContentChecksumMismatch, got {other}")
                        .as_str()
                        .into(),
                )),
            }
        });
    }
}
