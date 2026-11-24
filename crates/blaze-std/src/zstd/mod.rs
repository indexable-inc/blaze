//! Pure `#![no_std]`, zero-unsafe RFC 8878 Zstandard (`zstd`) compressor and decompressor.
//!
//! - Full RFC 8878 decoder: concatenated frames, skippable frames (`0x184D2A50..=0x184D2A5F`),
//!   Raw/RLE/Compressed blocks, Raw/RLE/1-stream and 4-stream Huffman Compressed & Treeless
//!   literals (direct 4-bit and FSE-compressed Huffman weights), Predefined/RLE/FSE/Repeat
//!   sequence tables, repeat-offset history (`[1, 4, 8]`), `Frame_Content_Size`, and 32-bit
//!   `XXH64` content checksum verification via [`crate::digest::xxh64`].
//! - Valid RFC 8878 compressor: single-segment or windowed frames, RLE blocks for constant runs,
//!   SWAR LZ77 match finding with repeat-offset preference, 1-stream and 4-stream Huffman literals
//!   (direct 4-bit and FSE-compressed weights), O(1) Predefined/Repeat-mode FSE sequence encoding,
//!   Raw fallback per block when incompressible, and optional `XXH64` content checksum.
//! - Caller-supplied [`ZstdLimits`] bound output size, window size, and frame count against
//!   decompression bombs.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexes, and
//!   zero recursion.

mod decode;
mod encode;
mod fse;
mod huffman;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, Vec};

pub use decode::ZstdDecoder;
pub use encode::ZstdEncoder;

pub(crate) const ZSTD_MAGIC: u32 = 0xFD2F_B528;
pub(crate) const SKIPPABLE_MAGIC_MIN: u32 = 0x184D_2A50;
pub(crate) const SKIPPABLE_MAGIC_MAX: u32 = 0x184D_2A5F;
pub(crate) const MAX_BLOCK_SIZE: usize = 128 * 1024;

/// Compression effort level for [`compress_in`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Faster single-probe match search with a 16K-entry hash table.
    Fast,
    /// Balanced match search with a 32K-entry hash table and one-step lazy probing.
    Default,
}

/// Options controlling [`compress_in`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CompressOpts {
    /// Match-finding effort level.
    pub level: Level,
    /// When `true`, append the RFC 8878 32-bit `XXH64` content checksum to the frame.
    pub checksum: bool,
}

impl CompressOpts {
    /// Balanced compression with content checksum enabled.
    pub const DEFAULT: Self = Self { level: Level::Default, checksum: true };

    /// Fast compression without a content checksum.
    pub const FAST: Self = Self { level: Level::Fast, checksum: false };
}

/// Caller-supplied safety bounds enforced by [`decompress_in`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ZstdLimits {
    /// Maximum total decompressed output bytes across all concatenated frames.
    pub most_output_bytes: usize,
    /// Maximum declared frame window size in bytes.
    pub most_window_bytes: usize,
    /// Maximum number of frames (including skippable frames) in one input stream.
    pub most_frames: usize,
}

impl ZstdLimits {
    /// Generous default bounds (64 MiB output, 8 MiB window, 1,024 frames).
    pub const USUAL: Self = Self {
        most_output_bytes: 64 << 20,
        most_window_bytes: 8 << 20,
        most_frames: 1_024,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed error returned while compressing, decompressing, or inspecting a Zstandard stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ZstdError {
    /// Frame did not begin with the Zstandard (`0xFD2FB528`) or skippable frame magic.
    BadMagic,
    /// Input ended before a frame header, block, payload, or checksum could be read.
    Truncated,
    /// A reserved bit in a frame header descriptor or sequence compression modes byte was set.
    ReservedBitSet,
    /// Frame requires an external dictionary, which is not supported.
    UnsupportedDictionary { dict_id: u32 },
    /// Frame declared a window larger than [`ZstdLimits::most_window_bytes`].
    WindowTooLarge { window_bytes: u64, limit: usize },
    /// Decompressed output would exceed [`ZstdLimits::most_output_bytes`].
    OutputTooLarge { limit: usize },
    /// Stream contained more frames than [`ZstdLimits::most_frames`].
    TooManyFrames { limit: usize },
    /// Block header used reserved block type `3`.
    BadBlockType,
    /// Block size or regenerated block size exceeded the RFC 8878 128 KiB maximum.
    BlockTooLarge { size: usize },
    /// Literals section header or payload was malformed.
    CorruptLiterals,
    /// Huffman tree description or Huffman-coded literals bitstream was malformed.
    CorruptHuffman,
    /// FSE distribution header, table, or bitstream was malformed.
    CorruptFse,
    /// Sequences section header, bitstream, or literal count was malformed.
    CorruptSequences,
    /// Sequence match offset was zero, exceeded available frame history, or exceeded window size.
    BadOffset { offset: usize, available: usize },
    /// Decoded frame byte count did not match the declared `Frame_Content_Size`.
    ContentSizeMismatch { expected: u64, actual: usize },
    /// Trailing 32-bit `XXH64` content checksum did not match the decoded frame bytes.
    ChecksumMismatch { expected: u32, actual: u32 },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for ZstdError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        ZstdError::Alloc(err)
    }
}

impl fmt::Display for ZstdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ZstdError::BadMagic => write!(f, "bstd::zstd: bad frame magic"),
            ZstdError::Truncated => write!(f, "bstd::zstd: unexpected end of input"),
            ZstdError::ReservedBitSet => write!(f, "bstd::zstd: reserved header bit is set"),
            ZstdError::UnsupportedDictionary { dict_id } => {
                write!(f, "bstd::zstd: unsupported dictionary id 0x{dict_id:08x}")
            }
            ZstdError::WindowTooLarge { window_bytes, limit } => write!(
                f,
                "bstd::zstd: frame window ({window_bytes} bytes) exceeds limit ({limit} bytes)"
            ),
            ZstdError::OutputTooLarge { limit } => {
                write!(f, "bstd::zstd: decompressed output exceeds limit ({limit} bytes)")
            }
            ZstdError::TooManyFrames { limit } => {
                write!(f, "bstd::zstd: frame count exceeds limit ({limit})")
            }
            ZstdError::BadBlockType => write!(f, "bstd::zstd: reserved block type 3"),
            ZstdError::BlockTooLarge { size } => {
                write!(f, "bstd::zstd: block size ({size} bytes) exceeds 128 KiB maximum")
            }
            ZstdError::CorruptLiterals => write!(f, "bstd::zstd: corrupt literals section"),
            ZstdError::CorruptHuffman => write!(f, "bstd::zstd: corrupt Huffman stream or table"),
            ZstdError::CorruptFse => write!(f, "bstd::zstd: corrupt FSE table or header"),
            ZstdError::CorruptSequences => write!(f, "bstd::zstd: corrupt sequences section"),
            ZstdError::BadOffset { offset, available } => write!(
                f,
                "bstd::zstd: invalid match offset {offset} with {available} bytes available"
            ),
            ZstdError::ContentSizeMismatch { expected, actual } => write!(
                f,
                "bstd::zstd: frame content size mismatch (declared {expected}, decoded {actual})"
            ),
            ZstdError::ChecksumMismatch { expected, actual } => write!(
                f,
                "bstd::zstd: content checksum mismatch (expected 0x{expected:08x}, got 0x{actual:08x})"
            ),
            ZstdError::Alloc(err) => write!(f, "bstd::zstd: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for ZstdError {}

/// Decompress one or more concatenated RFC 8878 Zstandard frames (including skippable frames)
/// into `alloc`, enforcing `limits`.
pub fn decompress_in<A: Allocator + Copy>(
    input: &[u8],
    limits: ZstdLimits,
    alloc: A,
) -> Result<Vec<u8, A>, ZstdError> {
    decode::decompress_in_inner(input, limits, alloc, false)
}

/// Compress `input` into a valid RFC 8878 Zstandard frame in `alloc`.
pub fn compress_in<A: Allocator + Copy>(
    input: &[u8],
    opts: CompressOpts,
    alloc: A,
) -> Result<Vec<u8, A>, ZstdError> {
    encode::compress_in_inner(input, opts, alloc)
}

/// Inspect the first Zstandard frame header in `input` and return its declared
/// `Frame_Content_Size` (if present) without decompressing any blocks.
///
/// Leading skippable frames are skipped; if `input` contains only skippable frames, returns
/// `Ok(Some(0))`.
pub fn frame_content_size(input: &[u8]) -> Result<Option<u64>, ZstdError> {
    decode::peek_frame_content_size(input)
}
