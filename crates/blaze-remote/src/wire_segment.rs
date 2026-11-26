//! `BZS1`: OpenZL-style structured, columnar, tag-partitioned wire transport for plain store
//! segments (`BLPS` format 3).
//!
//! Why not monolithic Zstd over raw `BLPS`:
//! - `BLPS` lays payloads in ascending 32-byte BLAKE3 key order, shuffling unrelated record types
//!   (`ItemsRecord`, `NamesRecord`, `CodeRecord`, `Expansion`, ...) together and destroying
//!   cross-record LZ77 locality.
//! - Each record carries up to 15 bytes of 16-byte alignment zero padding.
//! - Each 48-byte index entry carries 32 bytes of incompressible BLAKE3 key, an 8-byte monotonic
//!   payload offset derivable from lengths, and a 4-byte CRC-32C derivable from `(key, tag,
//!   payload)` at hardware CRC-32C speed.
//!
//! `BZS1` is a deterministic, lossless byte-for-byte bijection for canonical `BLPS` segments
//! (`decode(encode(seg)) == seg`):
//! 1. 64-byte header (`BZS1`, version 1, namespace, record count, tag stream count, exact raw
//!    `BLPS` length, and claimed 32-byte `BLPS` digest).
//! 2. Raw sorted 32-byte keys (incompressible BLAKE3 hashes stored uncompressed: zero wasted
//!    compression CPU).
//! 3. Compressed `tags` (`[u16]`) and `lens` (`[u32]`) columns (each independently `Raw` or
//!    `Zstd`, picking `Raw` when Zstd would not shrink).
//! 4. Sorted tag stream directory (strictly ascending by `Tag`, 24 bytes per distinct tag) and
//!    one homogeneous `blaze-zstd` (or `Raw`) stream per tag, restoring cross-record LZ77
//!    locality within every record schema.
//! 5. Decoding reconstructs the exact `BLPS` bytes under strict caller bounds without hashing;
//!    the caller verifies every reconstructed byte ONCE via `blaze_cas::plain::check_segment`
//!    against the signed manifest row's `segment` digest (rule 74).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::alloc::Allocator;
use core::fmt;

use blaze_cas::plain::{ALIGN, Described, ENTRY, Entry, FOOTER, HEADER, Namespace, SEGMENT_FORMAT, SEGMENT_MOST, check_segment, record_crc};
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::kind::Tag;
use blaze_keyed::store::StoreError;
use blaze_own::{AllocFailure, Vec};
use blaze_zstd::{CompressOpts, Level, ZstdDecoder, ZstdEncoder, ZstdError, ZstdLimits};

/// Wire segment magic (`BZS1`).
pub const MAGIC: [u8; 4] = *b"BZS1";
/// Wire segment format version.
pub const VERSION: u32 = 1;
/// Fixed header size in bytes.
pub const WIRE_HEADER: usize = 64;
/// Per-tag stream directory entry size in bytes.
pub const STREAM_DIR_ENTRY: usize = 24;
/// Minimum valid wire segment length (empty segment: 64B header + 5B tags + 5B lens).
pub const WIRE_MIN_LEN: u64 = 74;
/// Maximum valid wire segment length bounded above `SEGMENT_MOST` for raw fallback framing.
pub const WIRE_MOST: u64 = SEGMENT_MOST + (64 << 20);
/// Maximum Zstd window accepted on decode (8 MiB, matching `ZstdLimits::USUAL`).
const WINDOW_MOST: usize = 8 << 20;
/// Maximum record count accepted on decode (matching `read::form::COUNT_MOST`).
const COUNT_MOST: u32 = 1 << 26;
/// Zero padding bytes for 16-byte `BLPS` payload alignment.
const ZEROS: [u8; 16] = [0; 16];
/// Domain tag of `blaze_cas::plain::check_segment`'s segment BLAKE3 digest (format 3).
use blaze_cas::plain::SEGMENT_DIGEST as BLPS_SEGMENT_DIGEST;

/// Per-stream codec selector: `Raw` avoids expansion on incompressible or tiny streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Codec {
    Raw = 0,
    Zstd = 1,
}

impl Codec {
    const fn to_byte(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Zstd => 1,
        }
    }

    fn decode(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Raw),
            1 => Some(Self::Zstd),
            _ => None,
        }
    }
}

/// Typed failure encoding or decoding a `BZS1` wire segment.
#[derive(Debug)]
pub enum WireSegmentError {
    /// Input `BLPS` segment failed `check_segment` during encoding.
    Segment(StoreError),
    /// Input `BLPS` segment is valid but not canonically packed (non-monotonic payloads, non-zero
    /// padding, non-standard CRC, or non-zero reserved header bytes).
    NonCanonical { what: &'static str, at: u64 },
    /// Input wire bytes did not start with `BZS1`.
    BadMagic { found: [u8; 4] },
    /// Wire format version is not supported by this blaze.
    UnsupportedVersion { found: u32, wanted: u32 },
    /// A declared length exceeded the caller's bound before allocation.
    TooLarge { what: &'static str, len: u64, most: u64 },
    /// Wire structure, lengths, or stream directory failed validation.
    Malformed { what: &'static str, at: u64 },
    /// `blaze-zstd` failed compressing or decompressing a column or tag stream.
    Codec { stream: &'static str, error: ZstdError },
    /// Reconstructed canonical `BLPS` BLAKE3 digest did not match the segment's header digest.
    DigestMismatch { claimed: ContentId, computed: ContentId },
    /// Allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<StoreError> for WireSegmentError {
    fn from(error: StoreError) -> Self {
        Self::Segment(error)
    }
}

impl From<AllocFailure> for WireSegmentError {
    fn from(error: AllocFailure) -> Self {
        Self::Alloc(error)
    }
}

impl fmt::Display for WireSegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Segment(error) => write!(f, "wire segment encode: {error}"),
            Self::NonCanonical { what, at } => write!(f, "wire segment encode at byte {at}: {what}"),
            Self::BadMagic { found } => write!(f, "wire segment: expected magic `BZS1`, found {found:?}"),
            Self::UnsupportedVersion { found, wanted } => write!(f, "wire segment: version {found}, this blaze reads {wanted}"),
            Self::TooLarge { what, len, most } => write!(f, "wire segment: {what} is {len}, past limit {most}"),
            Self::Malformed { what, at } => write!(f, "wire segment at byte {at}: {what}"),
            Self::Codec { stream, error } => write!(f, "wire segment {stream} stream: {error}"),
            Self::DigestMismatch { claimed, computed } => write!(f, "wire segment digest mismatch: claimed {claimed}, computed {computed}"),
            Self::Alloc(error) => write!(f, "wire segment allocation failed: {error}"),
        }
    }
}

impl core::error::Error for WireSegmentError {}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, WireSegmentError> {
    let slice = bytes.get(at..at + 2).ok_or(WireSegmentError::Malformed { what: "truncated u16", at: u64::try_from(at).unwrap_or(u64::MAX) })?;
    let arr: [u8; 2] = slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "u16 slice", at: u64::try_from(at).unwrap_or(u64::MAX) })?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, WireSegmentError> {
    let slice = bytes.get(at..at + 4).ok_or(WireSegmentError::Malformed { what: "truncated u32", at: u64::try_from(at).unwrap_or(u64::MAX) })?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "u32 slice", at: u64::try_from(at).unwrap_or(u64::MAX) })?;
    Ok(u32::from_le_bytes(arr))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, WireSegmentError> {
    let slice = bytes.get(at..at + 8).ok_or(WireSegmentError::Malformed { what: "truncated u64", at: u64::try_from(at).unwrap_or(u64::MAX) })?;
    let arr: [u8; 8] = slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "u64 slice", at: u64::try_from(at).unwrap_or(u64::MAX) })?;
    Ok(u64::from_le_bytes(arr))
}

fn align_up(at: u64) -> Option<u64> {
    at.checked_add(ALIGN - 1).map(|v| (v / ALIGN) * ALIGN)
}

#[derive(Clone, Copy)]
struct TagMeta {
    tag: u16,
    record_count: u32,
    raw_bytes: u64,
    group_start: usize,
    cursor: usize,
}

#[derive(Clone, Copy)]
struct StreamDir {
    tag: u16,
    codec: Codec,
    record_count: u32,
    raw_bytes: u64,
    wire_bytes: u64,
    wire_start: usize,
}

#[derive(Clone, Copy)]
enum StreamSlice {
    Raw { wire_start: usize, len: usize },
    Decoded { buf_start: usize, len: usize },
}

fn compress_column_lazy<A: Allocator + Copy>(
    encoder: &mut Option<ZstdEncoder<A>>,
    alloc: A,
    raw: &[u8],
    stream_name: &'static str,
    scratch: &mut Vec<u8, A>,
    out: &mut Vec<u8, A>,
) -> Result<(), WireSegmentError> {
    let opts = CompressOpts { level: Level::Default, checksum: false };
    scratch.clear();
    let use_zstd = if raw.len() >= 16 {
        let enc = match encoder.as_mut() {
            Some(enc) => enc,
            None => encoder.insert(ZstdEncoder::try_new_in(alloc).map_err(|error| WireSegmentError::Codec { stream: "encoder init", error })?),
        };
        enc.compress_into(raw, opts, scratch).map_err(|error| WireSegmentError::Codec { stream: stream_name, error })?;
        scratch.len() < raw.len()
    } else {
        false
    };
    let (codec, bytes) = if use_zstd { (Codec::Zstd, scratch.as_slice()) } else { (Codec::Raw, raw) };
    let wire_len = u32::try_from(bytes.len()).map_err(|_| WireSegmentError::TooLarge {
        what: "compressed column length",
        len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        most: u64::from(u32::MAX),
    })?;
    out.try_push(codec.to_byte()).map_err(|r| r.why)?;
    out.try_extend_from_slice(&wire_len.to_le_bytes())?;
    out.try_extend_from_slice(bytes)?;
    Ok(())
}

fn decode_column<A: Allocator + Copy>(
    decoder: &mut ZstdDecoder<A>,
    codec: Codec,
    wire: &[u8],
    expected_len: usize,
    stream_name: &'static str,
    out: &mut Vec<u8, A>,
) -> Result<(), WireSegmentError> {
    out.clear();
    match codec {
        Codec::Raw => {
            if wire.len() != expected_len {
                return Err(WireSegmentError::Malformed { what: "raw column length disagrees with record count", at: 0 });
            }
            out.try_extend_from_slice(wire)?;
        }
        Codec::Zstd => {
            let limits = ZstdLimits { most_output_bytes: expected_len, most_window_bytes: WINDOW_MOST, most_frames: 1 };
            decoder.decompress_into(wire, limits, out).map_err(|error| WireSegmentError::Codec { stream: stream_name, error })?;
            if out.len() != expected_len {
                return Err(WireSegmentError::Malformed { what: "decompressed column length disagrees with record count", at: 0 });
            }
        }
    }
    Ok(())
}

/// Reusable `BZS1` wire segment encoder: allocates its `ZstdEncoder` hash table and working
/// scratch buffers at most once across all segments in a publish batch (zero per-segment
/// allocation churn on bump allocators).
pub struct WireSegmentEncoder<A: Allocator + Copy> {
    alloc: A,
    zstd: Option<ZstdEncoder<A>>,
    keys_raw: Vec<u8, A>,
    tags_raw: Vec<u8, A>,
    lens_raw: Vec<u8, A>,
    tag_table: Vec<TagMeta, A>,
    record_stream: Vec<u16, A>,
    record_ranges: Vec<(usize, usize), A>,
    perm: Vec<usize, A>,
    sorted_tags: Vec<TagMeta, A>,
    old_to_new: Vec<u16, A>,
    grouped: Vec<u8, A>,
    scratch: Vec<u8, A>,
    dir_bytes: Vec<u8, A>,
    stream_blobs: Vec<u8, A>,
}

impl<A: Allocator + Copy> WireSegmentEncoder<A> {
    /// Create a reusable encoder backed by `alloc` (Zstd tables allocated lazily on first
    /// compressible stream).
    pub fn new_in(alloc: A) -> Self {
        Self {
            alloc,
            zstd: None,
            keys_raw: Vec::new_in(alloc),
            tags_raw: Vec::new_in(alloc),
            lens_raw: Vec::new_in(alloc),
            tag_table: Vec::new_in(alloc),
            record_stream: Vec::new_in(alloc),
            record_ranges: Vec::new_in(alloc),
            perm: Vec::new_in(alloc),
            sorted_tags: Vec::new_in(alloc),
            old_to_new: Vec::new_in(alloc),
            grouped: Vec::new_in(alloc),
            scratch: Vec::new_in(alloc),
            dir_bytes: Vec::new_in(alloc),
            stream_blobs: Vec::new_in(alloc),
        }
    }

    /// Encode a canonical `BLPS` format-3 plain segment into `BZS1` columnar wire bytes.
    ///
    /// Deterministic: identical `BLPS` bytes always produce byte-identical `BZS1` output.
    pub fn encode(&mut self, raw_blps: &[u8], out: &mut Vec<u8, A>) -> Result<(), WireSegmentError> {
        let described = check_segment(raw_blps)?;
        if raw_blps.get(20..24) != Some(&[0u8; 4]) {
            return Err(WireSegmentError::NonCanonical { what: "header reserved bytes 20..24 are non-zero", at: 20 });
        }
        if raw_blps.get(96..128) != Some(&[0u8; 32]) {
            return Err(WireSegmentError::NonCanonical { what: "header reserved bytes 96..128 are non-zero", at: 96 });
        }

        let count = usize::try_from(described.count).map_err(|_| WireSegmentError::TooLarge {
            what: "record count",
            len: u64::from(described.count),
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let index_at = usize::try_from(described.index_at).map_err(|_| WireSegmentError::TooLarge {
            what: "index offset",
            len: described.index_at,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let run_len = count.checked_mul(ENTRY).ok_or(WireSegmentError::Malformed { what: "index size overflow", at: described.index_at })?;
        let run = raw_blps.get(index_at..index_at.saturating_add(run_len)).ok_or(WireSegmentError::Malformed { what: "index slice out of bounds", at: described.index_at })?;

        self.keys_raw.clear();
        self.tags_raw.clear();
        self.lens_raw.clear();
        self.tag_table.clear();
        self.record_stream.clear();
        self.record_ranges.clear();
        self.keys_raw.try_reserve(count.saturating_mul(32))?;
        self.tags_raw.try_reserve(count.saturating_mul(2))?;
        self.lens_raw.try_reserve(count.saturating_mul(4))?;
        self.record_stream.try_reserve(count)?;
        self.record_ranges.try_reserve(count)?;

        let mut expected_at = u64::try_from(HEADER).unwrap_or(128);
        let mut total_payload_bytes: usize = 0;

        for i in 0..count {
            let entry_off = i.saturating_mul(ENTRY);
            let raw_entry = run.get(entry_off..entry_off.saturating_add(ENTRY)).ok_or(WireSegmentError::Malformed { what: "entry slice out of bounds", at: described.index_at })?;
            let entry = Entry::decode(raw_entry).map_err(|what| WireSegmentError::NonCanonical { what, at: described.index_at.saturating_add(u64::try_from(entry_off).unwrap_or(0)) })?;
            if entry.at != expected_at {
                return Err(WireSegmentError::NonCanonical {
                    what: "payloads are not canonically packed in ascending key order",
                    at: entry.at,
                });
            }
            let start = usize::try_from(entry.at).map_err(|_| WireSegmentError::NonCanonical { what: "payload offset exceeds usize", at: entry.at })?;
            let len = usize::try_from(entry.len).map_err(|_| WireSegmentError::NonCanonical { what: "payload length exceeds usize", at: entry.at })?;
            let end = start.checked_add(len).ok_or(WireSegmentError::NonCanonical { what: "payload end overflow", at: entry.at })?;
            let payload = raw_blps.get(start..end).ok_or(WireSegmentError::NonCanonical { what: "payload out of bounds", at: entry.at })?;

            let next_at = align_up(entry.at.saturating_add(u64::from(entry.len))).ok_or(WireSegmentError::NonCanonical { what: "aligned offset overflow", at: entry.at })?;
            let next_at_usize = usize::try_from(next_at).map_err(|_| WireSegmentError::NonCanonical { what: "aligned offset exceeds usize", at: next_at })?;
            let pad = raw_blps.get(end..next_at_usize).ok_or(WireSegmentError::NonCanonical { what: "padding out of bounds", at: next_at })?;
            if pad.iter().any(|&b| b != 0) {
                return Err(WireSegmentError::NonCanonical { what: "non-zero alignment padding between payloads", at: u64::try_from(end).unwrap_or(0) });
            }
            if entry.crc != record_crc(&entry.key, entry.tag, payload) {
                return Err(WireSegmentError::NonCanonical { what: "entry CRC-32C does not match record_crc(key, tag, payload)", at: entry.at });
            }

            self.keys_raw.try_extend_from_slice(&entry.key.0)?;
            self.tags_raw.try_extend_from_slice(&entry.tag.0.to_le_bytes())?;
            self.lens_raw.try_extend_from_slice(&entry.len.to_le_bytes())?;
            self.record_ranges.try_push((start, end)).map_err(|r| r.why)?;
            total_payload_bytes = total_payload_bytes.checked_add(len).ok_or(WireSegmentError::TooLarge {
                what: "total payload bytes",
                len: u64::MAX,
                most: SEGMENT_MOST,
            })?;

            let tag_val = entry.tag.0;
            let slot = match self.tag_table.as_slice().iter().position(|m| m.tag == tag_val) {
                Some(idx) => idx,
                None => {
                    let idx = self.tag_table.len();
                    self.tag_table.try_push(TagMeta { tag: tag_val, record_count: 0, raw_bytes: 0, group_start: 0, cursor: 0 }).map_err(|r| r.why)?;
                    idx
                }
            };
            if let Some(meta) = self.tag_table.as_mut_slice().get_mut(slot) {
                meta.record_count = meta.record_count.saturating_add(1);
                meta.raw_bytes = meta.raw_bytes.saturating_add(u64::from(entry.len));
            }
            let slot_u16 = u16::try_from(slot).map_err(|_| WireSegmentError::TooLarge {
                what: "distinct tag count",
                len: u64::try_from(slot).unwrap_or(u64::MAX),
                most: u64::from(u16::MAX),
            })?;
            self.record_stream.try_push(slot_u16).map_err(|r| r.why)?;
            expected_at = next_at;
        }

        if expected_at != described.index_at {
            return Err(WireSegmentError::NonCanonical { what: "trailing unreferenced bytes before segment index", at: expected_at });
        }

        // Sort tag streams canonically by Tag ascending (rule 25) and remap `record_stream` indices.
        let stream_count = self.tag_table.len();
        self.perm.clear();
        self.perm.try_reserve(stream_count)?;
        for i in 0..stream_count {
            self.perm.try_push(i).map_err(|r| r.why)?;
        }
        let tag_table_slice = self.tag_table.as_slice();
        self.perm.as_mut_slice().sort_unstable_by_key(|&old_idx| tag_table_slice.get(old_idx).map_or(0, |m| m.tag));

        self.sorted_tags.clear();
        self.sorted_tags.try_reserve(stream_count)?;
        self.old_to_new.clear();
        self.old_to_new.try_reserve(stream_count)?;
        self.old_to_new.resize(stream_count, 0);

        let mut group_offset: usize = 0;
        for (new_idx, &old_idx) in self.perm.as_slice().iter().enumerate() {
            let Some(&meta) = self.tag_table.as_slice().get(old_idx) else {
                return Err(WireSegmentError::Malformed { what: "tag permutation index", at: 0 });
            };
            let raw_len = usize::try_from(meta.raw_bytes).map_err(|_| WireSegmentError::TooLarge {
                what: "tag stream bytes",
                len: meta.raw_bytes,
                most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            let new_u16 = u16::try_from(new_idx).map_err(|_| WireSegmentError::TooLarge {
                what: "stream count",
                len: u64::try_from(new_idx).unwrap_or(u64::MAX),
                most: u64::from(u16::MAX),
            })?;
            if let Some(dst) = self.old_to_new.as_mut_slice().get_mut(old_idx) {
                *dst = new_u16;
            }
            self.sorted_tags.try_push(TagMeta {
                tag: meta.tag,
                record_count: meta.record_count,
                raw_bytes: meta.raw_bytes,
                group_start: group_offset,
                cursor: group_offset,
            }).map_err(|r| r.why)?;
            group_offset = group_offset.saturating_add(raw_len);
        }

        // Partition all payloads into contiguous homogeneous per-tag runs in ONE flat buffer.
        self.grouped.clear();
        self.grouped.try_reserve(total_payload_bytes)?;
        self.grouped.resize(total_payload_bytes, 0);
        for i in 0..count {
            let Some(&old_slot) = self.record_stream.as_slice().get(i) else { continue };
            let Some(&new_slot) = self.old_to_new.as_slice().get(usize::from(old_slot)) else { continue };
            let Some(&(start, end)) = self.record_ranges.as_slice().get(i) else { continue };
            let Some(payload) = raw_blps.get(start..end) else { continue };
            if let Some(meta) = self.sorted_tags.as_mut_slice().get_mut(usize::from(new_slot)) {
                let dst_start = meta.cursor;
                let dst_end = dst_start.saturating_add(payload.len());
                if let Some(dst) = self.grouped.as_mut_slice().get_mut(dst_start..dst_end) {
                    dst.copy_from_slice(payload);
                }
                meta.cursor = dst_end;
            }
        }

        // Emit header + keys + framed tags + framed lens.
        out.clear();
        let stream_count_u32 = u32::try_from(stream_count).map_err(|_| WireSegmentError::TooLarge {
            what: "stream count",
            len: u64::try_from(stream_count).unwrap_or(u64::MAX),
            most: u64::from(u32::MAX),
        })?;
        let raw_len = u64::try_from(raw_blps.len()).map_err(|_| WireSegmentError::TooLarge {
            what: "raw segment length",
            len: u64::MAX,
            most: SEGMENT_MOST,
        })?;

        out.try_extend_from_slice(&MAGIC)?;
        out.try_extend_from_slice(&VERSION.to_le_bytes())?;
        out.try_extend_from_slice(&described.namespace.0.to_le_bytes())?;
        out.try_extend_from_slice(&described.count.to_le_bytes())?;
        out.try_extend_from_slice(&stream_count_u32.to_le_bytes())?;
        out.try_extend_from_slice(&raw_len.to_le_bytes())?;
        out.try_extend_from_slice(&described.digest.0)?;

        out.try_extend_from_slice(self.keys_raw.as_slice())?;
        compress_column_lazy(&mut self.zstd, self.alloc, self.tags_raw.as_slice(), "tags", &mut self.scratch, out)?;
        compress_column_lazy(&mut self.zstd, self.alloc, self.lens_raw.as_slice(), "lens", &mut self.scratch, out)?;

        // Compress each homogeneous tag stream into one payload block and record its directory entry.
        let opts = CompressOpts { level: Level::Default, checksum: false };
        self.dir_bytes.clear();
        self.dir_bytes.try_reserve(stream_count.saturating_mul(STREAM_DIR_ENTRY))?;
        self.stream_blobs.clear();

        for meta in self.sorted_tags.as_slice() {
            let len = usize::try_from(meta.raw_bytes).map_err(|_| WireSegmentError::TooLarge {
                what: "tag stream length",
                len: meta.raw_bytes,
                most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            let raw_stream = self.grouped.as_slice().get(meta.group_start..meta.group_start.saturating_add(len)).ok_or(WireSegmentError::Malformed { what: "grouped stream slice", at: 0 })?;
            self.scratch.clear();
            let use_zstd = if raw_stream.len() >= 16 {
                let enc = match self.zstd.as_mut() {
                    Some(enc) => enc,
                    None => self.zstd.insert(ZstdEncoder::try_new_in(self.alloc).map_err(|error| WireSegmentError::Codec { stream: "encoder init", error })?),
                };
                enc.compress_into(raw_stream, opts, &mut self.scratch).map_err(|error| WireSegmentError::Codec { stream: "payload", error })?;
                self.scratch.len() < raw_stream.len()
            } else {
                false
            };
            let (codec, blob) = if use_zstd { (Codec::Zstd, self.scratch.as_slice()) } else { (Codec::Raw, raw_stream) };
            let wire_bytes = u64::try_from(blob.len()).map_err(|_| WireSegmentError::TooLarge {
                what: "compressed stream length",
                len: u64::MAX,
                most: WIRE_MOST,
            })?;

            self.dir_bytes.try_extend_from_slice(&meta.tag.to_le_bytes())?;
            self.dir_bytes.try_push(codec.to_byte()).map_err(|r| r.why)?;
            self.dir_bytes.try_push(0u8).map_err(|r| r.why)?;
            self.dir_bytes.try_extend_from_slice(&meta.record_count.to_le_bytes())?;
            self.dir_bytes.try_extend_from_slice(&meta.raw_bytes.to_le_bytes())?;
            self.dir_bytes.try_extend_from_slice(&wire_bytes.to_le_bytes())?;
            self.stream_blobs.try_extend_from_slice(blob)?;
        }

        out.try_extend_from_slice(self.dir_bytes.as_slice())?;
        out.try_extend_from_slice(self.stream_blobs.as_slice())?;
        Ok(())
    }
}

/// Encode a canonical `BLPS` format-3 plain segment into `BZS1` columnar wire bytes.
///
/// Deterministic: identical `BLPS` bytes always produce byte-identical `BZS1` output.
pub fn encode_wire_segment<A: Allocator + Copy>(raw_blps: &[u8], out: &mut Vec<u8, A>, alloc: A) -> Result<(), WireSegmentError> {
    let mut encoder = WireSegmentEncoder::new_in(alloc);
    encoder.encode(raw_blps, out)
}

/// Decode `BZS1` wire bytes back into the exact `BLPS` format-3 segment bytes, enforcing
/// `max_raw_len` BEFORE allocating output buffers.
///
/// The caller MUST run `blaze_cas::plain::check_segment(out)` once after decoding and compare
/// `described.digest` against the authenticated manifest row's `segment` digest (rule 74).
pub fn decode_wire_segment<A: Allocator + Copy>(
    wire: &[u8],
    max_raw_len: u64,
    out: &mut Vec<u8, A>,
    alloc: A,
) -> Result<(), WireSegmentError> {
    if wire.len() < WIRE_HEADER {
        return Err(WireSegmentError::Malformed { what: "wire segment shorter than 64-byte header", at: u64::try_from(wire.len()).unwrap_or(u64::MAX) });
    }
    let magic_slice = wire.get(0..4).ok_or(WireSegmentError::Malformed { what: "missing magic", at: 0 })?;
    let found_magic: [u8; 4] = magic_slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "magic bytes", at: 0 })?;
    if found_magic != MAGIC {
        return Err(WireSegmentError::BadMagic { found: found_magic });
    }
    let version = read_u32(wire, 4)?;
    if version != VERSION {
        return Err(WireSegmentError::UnsupportedVersion { found: version, wanted: VERSION });
    }

    let namespace = Namespace(read_u64(wire, 8)?);
    let count_u32 = read_u32(wire, 16)?;
    let stream_count_u32 = read_u32(wire, 20)?;
    let raw_len = read_u64(wire, 24)?;
    let digest_slice = wire.get(32..64).ok_or(WireSegmentError::Malformed { what: "missing digest", at: 32 })?;
    let digest_arr: [u8; 32] = digest_slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "digest bytes", at: 32 })?;

    let effective_max = max_raw_len.min(SEGMENT_MOST);
    let min_blps = u64::try_from(HEADER + FOOTER).unwrap_or(176);
    if raw_len > effective_max {
        return Err(WireSegmentError::TooLarge { what: "raw segment length", len: raw_len, most: effective_max });
    }
    if raw_len < min_blps {
        return Err(WireSegmentError::Malformed { what: "declared raw length smaller than BLPS header + footer", at: 24 });
    }
    if count_u32 > COUNT_MOST {
        return Err(WireSegmentError::TooLarge { what: "record count", len: u64::from(count_u32), most: u64::from(COUNT_MOST) });
    }
    if stream_count_u32 > count_u32 || stream_count_u32 > u32::from(u16::MAX) + 1 {
        return Err(WireSegmentError::Malformed { what: "stream count exceeds record count or Tag domain", at: 20 });
    }

    let count = usize::try_from(count_u32).map_err(|_| WireSegmentError::TooLarge {
        what: "record count",
        len: u64::from(count_u32),
        most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
    })?;
    let stream_count = usize::try_from(stream_count_u32).map_err(|_| WireSegmentError::TooLarge {
        what: "stream count",
        len: u64::from(stream_count_u32),
        most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
    })?;

    // Validate all framing offsets before allocating any decoded buffers.
    let mut cursor = WIRE_HEADER;
    let keys_bytes = count.checked_mul(32).ok_or(WireSegmentError::Malformed { what: "keys length overflow", at: 16 })?;
    let keys_end = cursor.checked_add(keys_bytes).ok_or(WireSegmentError::Malformed { what: "keys end overflow", at: 16 })?;
    let keys_slice = wire.get(cursor..keys_end).ok_or(WireSegmentError::Malformed { what: "wire truncated inside keys", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = keys_end;

    // Tags column frame.
    let tags_codec = Codec::decode(*wire.get(cursor).ok_or(WireSegmentError::Malformed { what: "missing tags codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?)
        .ok_or(WireSegmentError::Malformed { what: "unknown tags codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    let tags_wire_len = usize::try_from(read_u32(wire, cursor.saturating_add(1))?).map_err(|_| WireSegmentError::Malformed { what: "tags wire len", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = cursor.checked_add(5).ok_or(WireSegmentError::Malformed { what: "tags header overflow", at: 0 })?;
    let tags_end = cursor.checked_add(tags_wire_len).ok_or(WireSegmentError::Malformed { what: "tags end overflow", at: 0 })?;
    let tags_wire = wire.get(cursor..tags_end).ok_or(WireSegmentError::Malformed { what: "wire truncated inside tags column", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = tags_end;

    // Lengths column frame.
    let lens_codec = Codec::decode(*wire.get(cursor).ok_or(WireSegmentError::Malformed { what: "missing lens codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?)
        .ok_or(WireSegmentError::Malformed { what: "unknown lens codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    let lens_wire_len = usize::try_from(read_u32(wire, cursor.saturating_add(1))?).map_err(|_| WireSegmentError::Malformed { what: "lens wire len", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = cursor.checked_add(5).ok_or(WireSegmentError::Malformed { what: "lens header overflow", at: 0 })?;
    let lens_end = cursor.checked_add(lens_wire_len).ok_or(WireSegmentError::Malformed { what: "lens end overflow", at: 0 })?;
    let lens_wire = wire.get(cursor..lens_end).ok_or(WireSegmentError::Malformed { what: "wire truncated inside lens column", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = lens_end;

    // Stream directory.
    let dir_bytes = stream_count.checked_mul(STREAM_DIR_ENTRY).ok_or(WireSegmentError::Malformed { what: "stream directory overflow", at: 20 })?;
    let dir_end = cursor.checked_add(dir_bytes).ok_or(WireSegmentError::Malformed { what: "stream directory end overflow", at: 20 })?;
    if dir_end > wire.len() {
        return Err(WireSegmentError::Malformed { what: "wire truncated inside stream directory", at: u64::try_from(cursor).unwrap_or(u64::MAX) });
    }

    let mut streams: Vec<StreamDir, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    let mut stream_wire_cursor = dir_end;
    let mut dir_record_sum: u64 = 0;
    let mut dir_raw_sum: u64 = 0;
    let mut zstd_decoded_total: usize = 0;
    let max_payload_budget = raw_len.saturating_sub(min_blps);

    for s in 0..stream_count {
        let entry_at = cursor.saturating_add(s.saturating_mul(STREAM_DIR_ENTRY));
        let tag = read_u16(wire, entry_at)?;
        let codec_byte = *wire.get(entry_at.saturating_add(2)).ok_or(WireSegmentError::Malformed { what: "missing stream codec", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        let reserved = *wire.get(entry_at.saturating_add(3)).ok_or(WireSegmentError::Malformed { what: "missing stream reserved byte", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        let codec = Codec::decode(codec_byte).ok_or(WireSegmentError::Malformed { what: "unknown stream codec", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        if reserved != 0 {
            return Err(WireSegmentError::Malformed { what: "non-zero reserved byte in stream directory", at: u64::try_from(entry_at.saturating_add(3)).unwrap_or(u64::MAX) });
        }
        let record_count = read_u32(wire, entry_at.saturating_add(4))?;
        let raw_bytes = read_u64(wire, entry_at.saturating_add(8))?;
        let wire_bytes = read_u64(wire, entry_at.saturating_add(16))?;
        if record_count == 0 {
            return Err(WireSegmentError::Malformed { what: "empty tag stream in directory", at: u64::try_from(entry_at).unwrap_or(u64::MAX) });
        }
        if let Some(prev) = streams.as_slice().last()
            && prev.tag >= tag
        {
            return Err(WireSegmentError::Malformed { what: "stream directory tags are not strictly ascending", at: u64::try_from(entry_at).unwrap_or(u64::MAX) });
        }
        if codec == Codec::Raw && wire_bytes != raw_bytes {
            return Err(WireSegmentError::Malformed { what: "raw stream wire_bytes disagrees with raw_bytes", at: u64::try_from(entry_at).unwrap_or(u64::MAX) });
        }
        dir_record_sum = dir_record_sum.checked_add(u64::from(record_count)).ok_or(WireSegmentError::Malformed { what: "stream record sum overflow", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        dir_raw_sum = dir_raw_sum.checked_add(raw_bytes).ok_or(WireSegmentError::Malformed { what: "stream raw byte sum overflow", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        if dir_raw_sum > max_payload_budget {
            return Err(WireSegmentError::TooLarge { what: "declared tag streams raw bytes", len: dir_raw_sum, most: max_payload_budget });
        }
        let wire_bytes_usize = usize::try_from(wire_bytes).map_err(|_| WireSegmentError::TooLarge {
            what: "stream wire bytes",
            len: wire_bytes,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let wire_end = stream_wire_cursor.checked_add(wire_bytes_usize).ok_or(WireSegmentError::Malformed { what: "stream wire end overflow", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        if wire_end > wire.len() {
            return Err(WireSegmentError::Malformed { what: "wire truncated inside tag payload stream", at: u64::try_from(stream_wire_cursor).unwrap_or(u64::MAX) });
        }
        if codec == Codec::Zstd {
            let raw_usize = usize::try_from(raw_bytes).map_err(|_| WireSegmentError::TooLarge {
                what: "tag stream raw bytes",
                len: raw_bytes,
                most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            zstd_decoded_total = zstd_decoded_total.checked_add(raw_usize).ok_or(WireSegmentError::TooLarge {
                what: "decoded streams buffer",
                len: u64::MAX,
                most: max_payload_budget,
            })?;
        }
        streams.try_push(StreamDir { tag, codec, record_count, raw_bytes, wire_bytes, wire_start: stream_wire_cursor }).map_err(|r| r.why)?;
        stream_wire_cursor = wire_end;
    }

    if dir_record_sum != u64::from(count_u32) {
        return Err(WireSegmentError::Malformed { what: "stream directory record counts do not sum to segment count", at: 20 });
    }
    if stream_wire_cursor != wire.len() {
        return Err(WireSegmentError::Malformed { what: "trailing bytes after tag payload streams", at: u64::try_from(stream_wire_cursor).unwrap_or(u64::MAX) });
    }

    // Decode tags and lengths columns.
    let mut decoder = ZstdDecoder::new_in(alloc);
    let expected_tags_bytes = count.checked_mul(2).ok_or(WireSegmentError::Malformed { what: "tags byte length overflow", at: 16 })?;
    let expected_lens_bytes = count.checked_mul(4).ok_or(WireSegmentError::Malformed { what: "lens byte length overflow", at: 16 })?;

    let mut tags_buf: Vec<u8, A> = Vec::try_with_capacity_in(expected_tags_bytes, alloc)?;
    decode_column(&mut decoder, tags_codec, tags_wire, expected_tags_bytes, "tags", &mut tags_buf)?;

    let mut lens_buf: Vec<u8, A> = Vec::try_with_capacity_in(expected_lens_bytes, alloc)?;
    decode_column(&mut decoder, lens_codec, lens_wire, expected_lens_bytes, "lens", &mut lens_buf)?;

    // Validate tags + lengths against directory and compute exact `index_at`.
    let mut record_stream: Vec<u16, A> = Vec::try_with_capacity_in(count, alloc)?;
    let mut seen_records: Vec<u32, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    seen_records.resize(stream_count, 0);
    let mut seen_bytes: Vec<u64, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    seen_bytes.resize(stream_count, 0);

    let mut at = u64::try_from(HEADER).unwrap_or(128);
    for i in 0..count {
        let tag = read_u16(tags_buf.as_slice(), i.saturating_mul(2))?;
        let len = read_u32(lens_buf.as_slice(), i.saturating_mul(4))?;
        let stream_idx = streams.as_slice().binary_search_by_key(&tag, |s| s.tag).map_err(|_| WireSegmentError::Malformed {
            what: "record tag not present in stream directory",
            at: u64::try_from(i).unwrap_or(u64::MAX),
        })?;
        if let Some(rc) = seen_records.as_mut_slice().get_mut(stream_idx) {
            *rc = rc.saturating_add(1);
        }
        if let Some(sb) = seen_bytes.as_mut_slice().get_mut(stream_idx) {
            *sb = sb.saturating_add(u64::from(len));
        }
        let s_u16 = u16::try_from(stream_idx).map_err(|_| WireSegmentError::Malformed { what: "stream index", at: 0 })?;
        record_stream.try_push(s_u16).map_err(|r| r.why)?;
        at = align_up(at.saturating_add(u64::from(len))).ok_or(WireSegmentError::Malformed { what: "reconstructed payload offset overflow", at: 24 })?;
    }

    let index_at = at;
    let expected_raw_len = index_at
        .checked_add(u64::from(count_u32).saturating_mul(u64::try_from(ENTRY).unwrap_or(48)))
        .and_then(|v| v.checked_add(u64::try_from(FOOTER).unwrap_or(48)))
        .ok_or(WireSegmentError::Malformed { what: "reconstructed raw length overflow", at: 24 })?;
    if expected_raw_len != raw_len {
        return Err(WireSegmentError::Malformed { what: "reconstructed BLPS length disagrees with wire header raw_len", at: 24 });
    }
    for (s, dir) in streams.as_slice().iter().enumerate() {
        if seen_records.as_slice().get(s).copied() != Some(dir.record_count) || seen_bytes.as_slice().get(s).copied() != Some(dir.raw_bytes) {
            return Err(WireSegmentError::Malformed { what: "record tags/lengths disagree with stream directory totals", at: 20 });
        }
    }

    // Decompress all Zstd tag streams into one flat buffer (Raw streams stay zero-copy in `wire`).
    let mut decoded_streams: Vec<u8, A> = Vec::try_with_capacity_in(zstd_decoded_total, alloc)?;
    let mut stream_slices: Vec<StreamSlice, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    for dir in streams.as_slice() {
        let raw_usize = usize::try_from(dir.raw_bytes).map_err(|_| WireSegmentError::TooLarge {
            what: "tag stream raw bytes",
            len: dir.raw_bytes,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let wire_usize = usize::try_from(dir.wire_bytes).map_err(|_| WireSegmentError::TooLarge {
            what: "tag stream wire bytes",
            len: dir.wire_bytes,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        match dir.codec {
            Codec::Raw => {
                stream_slices.try_push(StreamSlice::Raw { wire_start: dir.wire_start, len: raw_usize }).map_err(|r| r.why)?;
            }
            Codec::Zstd => {
                let buf_start = decoded_streams.len();
                let wire_slice = wire
                    .get(dir.wire_start..dir.wire_start.saturating_add(wire_usize))
                    .ok_or(WireSegmentError::Malformed { what: "tag stream wire slice", at: u64::try_from(dir.wire_start).unwrap_or(u64::MAX) })?;
                let limits = ZstdLimits { most_output_bytes: raw_usize, most_window_bytes: WINDOW_MOST, most_frames: 1 };
                decoder.decompress_into(wire_slice, limits, &mut decoded_streams).map_err(|error| WireSegmentError::Codec { stream: "payload", error })?;
                let got = decoded_streams.len().saturating_sub(buf_start);
                if got != raw_usize {
                    return Err(WireSegmentError::Malformed { what: "decompressed tag stream length disagrees with directory", at: u64::try_from(dir.wire_start).unwrap_or(u64::MAX) });
                }
                stream_slices.try_push(StreamSlice::Decoded { buf_start, len: raw_usize }).map_err(|r| r.why)?;
            }
        }
    }

    // Reconstruct the exact `BLPS` format-3 segment in `out`.
    let raw_len_usize = usize::try_from(raw_len).map_err(|_| WireSegmentError::TooLarge {
        what: "raw segment length",
        len: raw_len,
        most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
    })?;
    out.clear();
    out.try_reserve(raw_len_usize)?;

    let mut blps_header = [0u8; HEADER];
    blps_header[0..4].copy_from_slice(b"BLPS");
    blps_header[4..8].copy_from_slice(&SEGMENT_FORMAT.to_le_bytes());
    blps_header[8..16].copy_from_slice(&namespace.0.to_le_bytes());
    blps_header[16..20].copy_from_slice(&count_u32.to_le_bytes());
    blps_header[24..32].copy_from_slice(&index_at.to_le_bytes());
    if count > 0 {
        if let Some(first_key) = keys_slice.get(0..32) {
            blps_header[32..64].copy_from_slice(first_key);
        }
        let last_off = count.saturating_sub(1).saturating_mul(32);
        if let Some(last_key) = keys_slice.get(last_off..last_off.saturating_add(32)) {
            blps_header[64..96].copy_from_slice(last_key);
        }
    }
    out.try_extend_from_slice(&blps_header)?;

    let mut index_buf: Vec<u8, A> = Vec::try_with_capacity_in(count.saturating_mul(ENTRY), alloc)?;
    let mut stream_pos: Vec<usize, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    stream_pos.resize(stream_count, 0);

    for i in 0..count {
        let key_off = i.saturating_mul(32);
        let key_slice = keys_slice.get(key_off..key_off.saturating_add(32)).ok_or(WireSegmentError::Malformed { what: "key slice", at: 0 })?;
        let key_arr: [u8; 32] = key_slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "key array", at: 0 })?;
        let key = ContentId(key_arr);
        let tag = Tag(read_u16(tags_buf.as_slice(), i.saturating_mul(2))?);
        let len_u32 = read_u32(lens_buf.as_slice(), i.saturating_mul(4))?;
        let len = usize::try_from(len_u32).map_err(|_| WireSegmentError::Malformed { what: "record length", at: 0 })?;

        let s = usize::from(*record_stream.as_slice().get(i).ok_or(WireSegmentError::Malformed { what: "stream index", at: 0 })?);
        let pos = *stream_pos.as_slice().get(s).ok_or(WireSegmentError::Malformed { what: "stream cursor", at: 0 })?;
        let next_pos = pos.checked_add(len).ok_or(WireSegmentError::Malformed { what: "stream cursor overflow", at: 0 })?;
        let slice_desc = *stream_slices.as_slice().get(s).ok_or(WireSegmentError::Malformed { what: "stream slice", at: 0 })?;
        let payload = match slice_desc {
            StreamSlice::Raw { wire_start, len: slen } => {
                if next_pos > slen {
                    return Err(WireSegmentError::Malformed { what: "record exceeds raw tag stream", at: 0 });
                }
                wire.get(wire_start.saturating_add(pos)..wire_start.saturating_add(next_pos))
                    .ok_or(WireSegmentError::Malformed { what: "raw payload slice", at: 0 })?
            }
            StreamSlice::Decoded { buf_start, len: slen } => {
                if next_pos > slen {
                    return Err(WireSegmentError::Malformed { what: "record exceeds decoded tag stream", at: 0 });
                }
                decoded_streams
                    .as_slice()
                    .get(buf_start.saturating_add(pos)..buf_start.saturating_add(next_pos))
                    .ok_or(WireSegmentError::Malformed { what: "decoded payload slice", at: 0 })?
            }
        };
        if let Some(p) = stream_pos.as_mut_slice().get_mut(s) {
            *p = next_pos;
        }

        let payload_at = u64::try_from(out.len()).map_err(|_| WireSegmentError::Malformed { what: "payload offset", at: 0 })?;
        out.try_extend_from_slice(payload)?;
        let align_usize = usize::try_from(ALIGN).unwrap_or(16);
        let rem = out.len() % align_usize;
        if rem != 0 {
            let pad = align_usize.saturating_sub(rem);
            if let Some(z) = ZEROS.get(..pad) {
                out.try_extend_from_slice(z)?;
            }
        }

        let entry = Entry { key, tag, at: payload_at, len: len_u32, crc: record_crc(&key, tag, payload) };
        index_buf.try_extend_from_slice(&entry.encode_array())?;
    }

    out.try_extend_from_slice(index_buf.as_slice())?;

    let mut blps_footer = [0u8; FOOTER];
    blps_footer[0..4].copy_from_slice(b"BLPE");
    blps_footer[4..8].copy_from_slice(&count_u32.to_le_bytes());
    blps_footer[8..16].copy_from_slice(&index_at.to_le_bytes());
    blps_footer[16..48].copy_from_slice(&digest_arr);
    out.try_extend_from_slice(&blps_footer)?;

    if u64::try_from(out.len()).unwrap_or(u64::MAX) != raw_len {
        return Err(WireSegmentError::Malformed { what: "reconstructed segment length mismatch", at: 24 });
    }
    Ok(())
}

/// Where a verified wire record's payload bytes live (zero-copy into the Zstd-decoded buffer or
/// the raw wire buffer, avoiding a second whole-segment `BLPS` RAM copy).
#[derive(Clone, Copy)]
enum PayloadLoc {
    Wire { start: usize, len: usize },
    Decoded { start: usize, len: usize },
}

/// One record entry inside a [`VerifiedWireSegment`].
#[derive(Clone, Copy)]
struct VerifiedRecord {
    key: ContentId,
    tag: Tag,
    loc: PayloadLoc,
}

/// A `BZS1` wire segment decoded and verified ONCE against the exact canonical `BLPS` format-3
/// BLAKE3 digest (streaming over header, payloads, alignment padding, sorted index entries, and
/// footer) WITHOUT materializing a redundant contiguous `BLPS` copy in RAM.
pub struct VerifiedWireSegment<'w, A: Allocator + Copy> {
    wire: &'w [u8],
    described: Described,
    decoded_streams: Vec<u8, A>,
    records: Vec<VerifiedRecord, A>,
}

impl<'w, A: Allocator + Copy> VerifiedWireSegment<'w, A> {
    /// Segment metadata verified against the streaming `BLPS` BLAKE3 digest.
    pub fn described(&self) -> Described {
        self.described
    }

    /// Number of verified records in this segment.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Iterates over every verified `(key, tag, payload)` in ascending key order.
    pub fn iter(&self) -> impl Iterator<Item = (ContentId, Tag, &[u8])> + '_ {
        self.records.as_slice().iter().filter_map(|rec| {
            let payload = match rec.loc {
                PayloadLoc::Wire { start, len } => self.wire.get(start..start.saturating_add(len))?,
                PayloadLoc::Decoded { start, len } => self.decoded_streams.as_slice().get(start..start.saturating_add(len))?,
            };
            Some((rec.key, rec.tag, payload))
        })
    }
}

/// Decode `BZS1` wire bytes and verify the exact canonical `BLPS` format-3 BLAKE3 digest and
/// sorted-key invariants in one pass, WITHOUT allocating a second whole-segment `BLPS` buffer.
///
/// The caller MUST still compare `verified.described().digest`, `count`, `index_at`, and
/// `namespace` against its authenticated manifest row or expected segment digest before trusting
/// the records.
pub fn decode_wire_segment_verified<'w, A: Allocator + Copy>(
    wire: &'w [u8],
    max_raw_len: u64,
    alloc: A,
) -> Result<VerifiedWireSegment<'w, A>, WireSegmentError> {
    if wire.len() < WIRE_HEADER {
        return Err(WireSegmentError::Malformed { what: "wire segment shorter than 64-byte header", at: u64::try_from(wire.len()).unwrap_or(u64::MAX) });
    }
    let magic_slice = wire.get(0..4).ok_or(WireSegmentError::Malformed { what: "missing magic", at: 0 })?;
    let found_magic: [u8; 4] = magic_slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "magic bytes", at: 0 })?;
    if found_magic != MAGIC {
        return Err(WireSegmentError::BadMagic { found: found_magic });
    }
    let version = read_u32(wire, 4)?;
    if version != VERSION {
        return Err(WireSegmentError::UnsupportedVersion { found: version, wanted: VERSION });
    }

    let namespace = Namespace(read_u64(wire, 8)?);
    let count_u32 = read_u32(wire, 16)?;
    let stream_count_u32 = read_u32(wire, 20)?;
    let raw_len = read_u64(wire, 24)?;
    let digest_slice = wire.get(32..64).ok_or(WireSegmentError::Malformed { what: "missing digest", at: 32 })?;
    let digest_arr: [u8; 32] = digest_slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "digest bytes", at: 32 })?;
    let claimed = ContentId(digest_arr);

    let effective_max = max_raw_len.min(SEGMENT_MOST);
    let min_blps = u64::try_from(HEADER + FOOTER).unwrap_or(176);
    if raw_len > effective_max {
        return Err(WireSegmentError::TooLarge { what: "raw segment length", len: raw_len, most: effective_max });
    }
    if raw_len < min_blps {
        return Err(WireSegmentError::Malformed { what: "declared raw length smaller than BLPS header + footer", at: 24 });
    }
    if count_u32 > COUNT_MOST {
        return Err(WireSegmentError::TooLarge { what: "record count", len: u64::from(count_u32), most: u64::from(COUNT_MOST) });
    }
    if stream_count_u32 > count_u32 || stream_count_u32 > u32::from(u16::MAX) + 1 {
        return Err(WireSegmentError::Malformed { what: "stream count exceeds record count or Tag domain", at: 20 });
    }

    let count = usize::try_from(count_u32).map_err(|_| WireSegmentError::TooLarge {
        what: "record count",
        len: u64::from(count_u32),
        most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
    })?;
    let stream_count = usize::try_from(stream_count_u32).map_err(|_| WireSegmentError::TooLarge {
        what: "stream count",
        len: u64::from(stream_count_u32),
        most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
    })?;

    let mut cursor = WIRE_HEADER;
    let keys_bytes = count.checked_mul(32).ok_or(WireSegmentError::Malformed { what: "keys length overflow", at: 16 })?;
    let keys_end = cursor.checked_add(keys_bytes).ok_or(WireSegmentError::Malformed { what: "keys end overflow", at: 16 })?;
    let keys_slice = wire.get(cursor..keys_end).ok_or(WireSegmentError::Malformed { what: "wire truncated inside keys", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = keys_end;

    // Enforce strictly ascending keys (identical to `check_segment`'s sorted-index invariant).
    for i in 1..count {
        let prev = keys_slice.get(i.saturating_sub(1).saturating_mul(32)..i.saturating_mul(32)).ok_or(WireSegmentError::Malformed { what: "prev key slice", at: 0 })?;
        let curr = keys_slice.get(i.saturating_mul(32)..i.saturating_add(1).saturating_mul(32)).ok_or(WireSegmentError::Malformed { what: "curr key slice", at: 0 })?;
        if prev >= curr {
            return Err(WireSegmentError::Malformed { what: "wire segment keys are not strictly ascending", at: u64::try_from(i).unwrap_or(u64::MAX) });
        }
    }

    let tags_codec = Codec::decode(*wire.get(cursor).ok_or(WireSegmentError::Malformed { what: "missing tags codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?)
        .ok_or(WireSegmentError::Malformed { what: "unknown tags codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    let tags_wire_len = usize::try_from(read_u32(wire, cursor.saturating_add(1))?).map_err(|_| WireSegmentError::Malformed { what: "tags wire len", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = cursor.checked_add(5).ok_or(WireSegmentError::Malformed { what: "tags header overflow", at: 0 })?;
    let tags_end = cursor.checked_add(tags_wire_len).ok_or(WireSegmentError::Malformed { what: "tags end overflow", at: 0 })?;
    let tags_wire = wire.get(cursor..tags_end).ok_or(WireSegmentError::Malformed { what: "wire truncated inside tags column", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = tags_end;

    let lens_codec = Codec::decode(*wire.get(cursor).ok_or(WireSegmentError::Malformed { what: "missing lens codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?)
        .ok_or(WireSegmentError::Malformed { what: "unknown lens codec", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    let lens_wire_len = usize::try_from(read_u32(wire, cursor.saturating_add(1))?).map_err(|_| WireSegmentError::Malformed { what: "lens wire len", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = cursor.checked_add(5).ok_or(WireSegmentError::Malformed { what: "lens header overflow", at: 0 })?;
    let lens_end = cursor.checked_add(lens_wire_len).ok_or(WireSegmentError::Malformed { what: "lens end overflow", at: 0 })?;
    let lens_wire = wire.get(cursor..lens_end).ok_or(WireSegmentError::Malformed { what: "wire truncated inside lens column", at: u64::try_from(cursor).unwrap_or(u64::MAX) })?;
    cursor = lens_end;

    let dir_bytes = stream_count.checked_mul(STREAM_DIR_ENTRY).ok_or(WireSegmentError::Malformed { what: "stream directory overflow", at: 20 })?;
    let dir_end = cursor.checked_add(dir_bytes).ok_or(WireSegmentError::Malformed { what: "stream directory end overflow", at: 20 })?;
    if dir_end > wire.len() {
        return Err(WireSegmentError::Malformed { what: "wire truncated inside stream directory", at: u64::try_from(cursor).unwrap_or(u64::MAX) });
    }

    let mut streams: Vec<StreamDir, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    let mut stream_wire_cursor = dir_end;
    let mut dir_record_sum: u64 = 0;
    let mut dir_raw_sum: u64 = 0;
    let mut zstd_decoded_total: usize = 0;
    let max_payload_budget = raw_len.saturating_sub(min_blps);

    for s in 0..stream_count {
        let entry_at = cursor.saturating_add(s.saturating_mul(STREAM_DIR_ENTRY));
        let tag = read_u16(wire, entry_at)?;
        let codec_byte = *wire.get(entry_at.saturating_add(2)).ok_or(WireSegmentError::Malformed { what: "missing stream codec", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        let reserved = *wire.get(entry_at.saturating_add(3)).ok_or(WireSegmentError::Malformed { what: "missing stream reserved byte", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        let codec = Codec::decode(codec_byte).ok_or(WireSegmentError::Malformed { what: "unknown stream codec", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        if reserved != 0 {
            return Err(WireSegmentError::Malformed { what: "non-zero reserved byte in stream directory", at: u64::try_from(entry_at.saturating_add(3)).unwrap_or(u64::MAX) });
        }
        let record_count = read_u32(wire, entry_at.saturating_add(4))?;
        let raw_bytes = read_u64(wire, entry_at.saturating_add(8))?;
        let wire_bytes = read_u64(wire, entry_at.saturating_add(16))?;
        if record_count == 0 {
            return Err(WireSegmentError::Malformed { what: "empty tag stream in directory", at: u64::try_from(entry_at).unwrap_or(u64::MAX) });
        }
        if let Some(prev) = streams.as_slice().last()
            && prev.tag >= tag
        {
            return Err(WireSegmentError::Malformed { what: "stream directory tags are not strictly ascending", at: u64::try_from(entry_at).unwrap_or(u64::MAX) });
        }
        if codec == Codec::Raw && wire_bytes != raw_bytes {
            return Err(WireSegmentError::Malformed { what: "raw stream wire_bytes disagrees with raw_bytes", at: u64::try_from(entry_at).unwrap_or(u64::MAX) });
        }
        dir_record_sum = dir_record_sum.checked_add(u64::from(record_count)).ok_or(WireSegmentError::Malformed { what: "stream record sum overflow", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        dir_raw_sum = dir_raw_sum.checked_add(raw_bytes).ok_or(WireSegmentError::Malformed { what: "stream raw byte sum overflow", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        if dir_raw_sum > max_payload_budget {
            return Err(WireSegmentError::TooLarge { what: "declared tag streams raw bytes", len: dir_raw_sum, most: max_payload_budget });
        }
        let wire_bytes_usize = usize::try_from(wire_bytes).map_err(|_| WireSegmentError::TooLarge {
            what: "stream wire bytes",
            len: wire_bytes,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let wire_end = stream_wire_cursor.checked_add(wire_bytes_usize).ok_or(WireSegmentError::Malformed { what: "stream wire end overflow", at: u64::try_from(entry_at).unwrap_or(u64::MAX) })?;
        if wire_end > wire.len() {
            return Err(WireSegmentError::Malformed { what: "wire truncated inside tag payload stream", at: u64::try_from(stream_wire_cursor).unwrap_or(u64::MAX) });
        }
        if codec == Codec::Zstd {
            let raw_usize = usize::try_from(raw_bytes).map_err(|_| WireSegmentError::TooLarge {
                what: "tag stream raw bytes",
                len: raw_bytes,
                most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            zstd_decoded_total = zstd_decoded_total.checked_add(raw_usize).ok_or(WireSegmentError::TooLarge {
                what: "decoded streams buffer",
                len: u64::MAX,
                most: max_payload_budget,
            })?;
        }
        streams.try_push(StreamDir { tag, codec, record_count, raw_bytes, wire_bytes, wire_start: stream_wire_cursor }).map_err(|r| r.why)?;
        stream_wire_cursor = wire_end;
    }

    if dir_record_sum != u64::from(count_u32) {
        return Err(WireSegmentError::Malformed { what: "stream directory record counts do not sum to segment count", at: 20 });
    }
    if stream_wire_cursor != wire.len() {
        return Err(WireSegmentError::Malformed { what: "trailing bytes after tag payload streams", at: u64::try_from(stream_wire_cursor).unwrap_or(u64::MAX) });
    }

    let mut decoder = ZstdDecoder::new_in(alloc);
    let expected_tags_bytes = count.checked_mul(2).ok_or(WireSegmentError::Malformed { what: "tags byte length overflow", at: 16 })?;
    let expected_lens_bytes = count.checked_mul(4).ok_or(WireSegmentError::Malformed { what: "lens byte length overflow", at: 16 })?;

    let mut tags_buf: Vec<u8, A> = Vec::try_with_capacity_in(expected_tags_bytes, alloc)?;
    decode_column(&mut decoder, tags_codec, tags_wire, expected_tags_bytes, "tags", &mut tags_buf)?;

    let mut lens_buf: Vec<u8, A> = Vec::try_with_capacity_in(expected_lens_bytes, alloc)?;
    decode_column(&mut decoder, lens_codec, lens_wire, expected_lens_bytes, "lens", &mut lens_buf)?;

    let mut record_stream: Vec<u16, A> = Vec::try_with_capacity_in(count, alloc)?;
    let mut seen_records: Vec<u32, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    seen_records.resize(stream_count, 0);
    let mut seen_bytes: Vec<u64, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    seen_bytes.resize(stream_count, 0);

    let mut at = u64::try_from(HEADER).unwrap_or(128);
    for i in 0..count {
        let tag = read_u16(tags_buf.as_slice(), i.saturating_mul(2))?;
        let len = read_u32(lens_buf.as_slice(), i.saturating_mul(4))?;
        let stream_idx = streams.as_slice().binary_search_by_key(&tag, |s| s.tag).map_err(|_| WireSegmentError::Malformed {
            what: "record tag not present in stream directory",
            at: u64::try_from(i).unwrap_or(u64::MAX),
        })?;
        if let Some(rc) = seen_records.as_mut_slice().get_mut(stream_idx) {
            *rc = rc.saturating_add(1);
        }
        if let Some(sb) = seen_bytes.as_mut_slice().get_mut(stream_idx) {
            *sb = sb.saturating_add(u64::from(len));
        }
        let s_u16 = u16::try_from(stream_idx).map_err(|_| WireSegmentError::Malformed { what: "stream index", at: 0 })?;
        record_stream.try_push(s_u16).map_err(|r| r.why)?;
        at = align_up(at.saturating_add(u64::from(len))).ok_or(WireSegmentError::Malformed { what: "reconstructed payload offset overflow", at: 24 })?;
    }

    let index_at = at;
    let expected_raw_len = index_at
        .checked_add(u64::from(count_u32).saturating_mul(u64::try_from(ENTRY).unwrap_or(48)))
        .and_then(|v| v.checked_add(u64::try_from(FOOTER).unwrap_or(48)))
        .ok_or(WireSegmentError::Malformed { what: "reconstructed raw length overflow", at: 24 })?;
    if expected_raw_len != raw_len {
        return Err(WireSegmentError::Malformed { what: "reconstructed BLPS length disagrees with wire header raw_len", at: 24 });
    }
    for (s, dir) in streams.as_slice().iter().enumerate() {
        if seen_records.as_slice().get(s).copied() != Some(dir.record_count) || seen_bytes.as_slice().get(s).copied() != Some(dir.raw_bytes) {
            return Err(WireSegmentError::Malformed { what: "record tags/lengths disagree with stream directory totals", at: 20 });
        }
    }

    let mut decoded_streams: Vec<u8, A> = Vec::try_with_capacity_in(zstd_decoded_total, alloc)?;
    let mut stream_slices: Vec<StreamSlice, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    for dir in streams.as_slice() {
        let raw_usize = usize::try_from(dir.raw_bytes).map_err(|_| WireSegmentError::TooLarge {
            what: "tag stream raw bytes",
            len: dir.raw_bytes,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let wire_usize = usize::try_from(dir.wire_bytes).map_err(|_| WireSegmentError::TooLarge {
            what: "tag stream wire bytes",
            len: dir.wire_bytes,
            most: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        match dir.codec {
            Codec::Raw => {
                stream_slices.try_push(StreamSlice::Raw { wire_start: dir.wire_start, len: raw_usize }).map_err(|r| r.why)?;
            }
            Codec::Zstd => {
                let buf_start = decoded_streams.len();
                let wire_slice = wire
                    .get(dir.wire_start..dir.wire_start.saturating_add(wire_usize))
                    .ok_or(WireSegmentError::Malformed { what: "tag stream wire slice", at: u64::try_from(dir.wire_start).unwrap_or(u64::MAX) })?;
                let limits = ZstdLimits { most_output_bytes: raw_usize, most_window_bytes: WINDOW_MOST, most_frames: 1 };
                decoder.decompress_into(wire_slice, limits, &mut decoded_streams).map_err(|error| WireSegmentError::Codec { stream: "payload", error })?;
                let got = decoded_streams.len().saturating_sub(buf_start);
                if got != raw_usize {
                    return Err(WireSegmentError::Malformed { what: "decompressed tag stream length disagrees with directory", at: u64::try_from(dir.wire_start).unwrap_or(u64::MAX) });
                }
                stream_slices.try_push(StreamSlice::Decoded { buf_start, len: raw_usize }).map_err(|r| r.why)?;
            }
        }
    }

    // Stream the exact canonical BLPS bytes through BLAKE3 without copying payloads into a second
    // whole-segment buffer.
    let mut hasher = Hasher::new(BLPS_SEGMENT_DIGEST);
    let mut blps_header = [0u8; HEADER];
    blps_header[0..4].copy_from_slice(b"BLPS");
    blps_header[4..8].copy_from_slice(&SEGMENT_FORMAT.to_le_bytes());
    blps_header[8..16].copy_from_slice(&namespace.0.to_le_bytes());
    blps_header[16..20].copy_from_slice(&count_u32.to_le_bytes());
    blps_header[24..32].copy_from_slice(&index_at.to_le_bytes());
    if count > 0 {
        if let Some(first_key) = keys_slice.get(0..32) {
            blps_header[32..64].copy_from_slice(first_key);
        }
        let last_off = count.saturating_sub(1).saturating_mul(32);
        if let Some(last_key) = keys_slice.get(last_off..last_off.saturating_add(32)) {
            blps_header[64..96].copy_from_slice(last_key);
        }
    }
    hasher.update(&blps_header);

    let mut index_buf: Vec<u8, A> = Vec::try_with_capacity_in(count.saturating_mul(ENTRY), alloc)?;
    let mut records: Vec<VerifiedRecord, A> = Vec::try_with_capacity_in(count, alloc)?;
    let mut stream_pos: Vec<usize, A> = Vec::try_with_capacity_in(stream_count, alloc)?;
    stream_pos.resize(stream_count, 0);

    let mut payload_at = u64::try_from(HEADER).unwrap_or(128);
    let align_usize = usize::try_from(ALIGN).unwrap_or(16);

    for i in 0..count {
        let key_off = i.saturating_mul(32);
        let key_slice = keys_slice.get(key_off..key_off.saturating_add(32)).ok_or(WireSegmentError::Malformed { what: "key slice", at: 0 })?;
        let key_arr: [u8; 32] = key_slice.try_into().map_err(|_| WireSegmentError::Malformed { what: "key array", at: 0 })?;
        let key = ContentId(key_arr);
        let tag = Tag(read_u16(tags_buf.as_slice(), i.saturating_mul(2))?);
        let len_u32 = read_u32(lens_buf.as_slice(), i.saturating_mul(4))?;
        let len = usize::try_from(len_u32).map_err(|_| WireSegmentError::Malformed { what: "record length", at: 0 })?;

        let s = usize::from(*record_stream.as_slice().get(i).ok_or(WireSegmentError::Malformed { what: "stream index", at: 0 })?);
        let pos = *stream_pos.as_slice().get(s).ok_or(WireSegmentError::Malformed { what: "stream cursor", at: 0 })?;
        let next_pos = pos.checked_add(len).ok_or(WireSegmentError::Malformed { what: "stream cursor overflow", at: 0 })?;
        let slice_desc = *stream_slices.as_slice().get(s).ok_or(WireSegmentError::Malformed { what: "stream slice", at: 0 })?;
        let (loc, payload) = match slice_desc {
            StreamSlice::Raw { wire_start, len: slen } => {
                if next_pos > slen {
                    return Err(WireSegmentError::Malformed { what: "record exceeds raw tag stream", at: 0 });
                }
                let start = wire_start.saturating_add(pos);
                let slice = wire.get(start..start.saturating_add(len)).ok_or(WireSegmentError::Malformed { what: "raw payload slice", at: 0 })?;
                (PayloadLoc::Wire { start, len }, slice)
            }
            StreamSlice::Decoded { buf_start, len: slen } => {
                if next_pos > slen {
                    return Err(WireSegmentError::Malformed { what: "record exceeds decoded tag stream", at: 0 });
                }
                let start = buf_start.saturating_add(pos);
                let slice = decoded_streams.as_slice().get(start..start.saturating_add(len)).ok_or(WireSegmentError::Malformed { what: "decoded payload slice", at: 0 })?;
                (PayloadLoc::Decoded { start, len }, slice)
            }
        };
        if let Some(p) = stream_pos.as_mut_slice().get_mut(s) {
            *p = next_pos;
        }

        hasher.update(payload);
        let entry_at = payload_at;
        payload_at = payload_at.saturating_add(u64::from(len_u32));
        let rem = usize::try_from(payload_at % ALIGN).unwrap_or(0);
        if rem != 0 {
            let pad = align_usize.saturating_sub(rem);
            if let Some(z) = ZEROS.get(..pad) {
                hasher.update(z);
                payload_at = payload_at.saturating_add(u64::try_from(pad).unwrap_or(0));
            }
        }

        let entry = Entry { key, tag, at: entry_at, len: len_u32, crc: record_crc(&key, tag, payload) };
        index_buf.try_extend_from_slice(&entry.encode_array())?;
        records.try_push(VerifiedRecord { key, tag, loc }).map_err(|r| r.why)?;
    }

    if payload_at != index_at {
        return Err(WireSegmentError::Malformed { what: "streamed payload end disagrees with index_at", at: 24 });
    }
    hasher.update(index_buf.as_slice());

    let mut blps_footer_prefix = [0u8; 16];
    blps_footer_prefix[0..4].copy_from_slice(b"BLPE");
    blps_footer_prefix[4..8].copy_from_slice(&count_u32.to_le_bytes());
    blps_footer_prefix[8..16].copy_from_slice(&index_at.to_le_bytes());
    hasher.update(&blps_footer_prefix);

    let computed = hasher.finish();
    if computed != claimed {
        return Err(WireSegmentError::DigestMismatch { claimed, computed });
    }

    Ok(VerifiedWireSegment {
        wire,
        described: Described { namespace, count: count_u32, index_at, digest: claimed },
        decoded_streams,
        records,
    })
}

#[cfg(test)]
mod tests {
    use blaze_cas::plain::{Namespace, check_segment, seal_segment};
    use blaze_ids::ContentId;
    use blaze_keyed::exec::RecordRef;
    use blaze_keyed::kind::Tag;
    use blaze_own::Vec as OwnVec;
    use blaze_sim::Config;

    use super::*;

    fn make_segment<A: Allocator + Copy>(records: &[(Tag, &[u8])], alloc: A) -> OwnVec<u8, A> {
        let mut refs: OwnVec<(RecordRef, &[u8]), A> = OwnVec::with_capacity_in(records.len(), alloc);
        for (i, &(tag, payload)) in records.iter().enumerate() {
            let mut seed = [0u8; 10];
            seed[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            seed[8..10].copy_from_slice(&tag.0.to_le_bytes());
            let key = ContentId::of("wire segment test key", &seed);
            refs.push((RecordRef { key, tag }, payload));
        }
        let mut raw = [0u8; 0].to_vec();
        seal_segment(Namespace::PUBLIC, refs.as_slice(), &mut raw).expect("seal segment");
        let mut out = OwnVec::with_capacity_in(raw.len(), alloc);
        out.extend_from_slice(&raw);
        out
    }

    #[test]
    fn empty_segment_round_trips_byte_for_byte() {
        let alloc = blaze_own::testing::heap().alloc();
        let raw = make_segment(&[], alloc);
        assert_eq!(raw.len(), HEADER + FOOTER);
        let mut wire = OwnVec::new_in(alloc);
        encode_wire_segment(raw.as_slice(), &mut wire, alloc).expect("encode empty");
        assert_eq!(wire.len() as u64, WIRE_MIN_LEN);
        let mut decoded = OwnVec::new_in(alloc);
        decode_wire_segment(wire.as_slice(), SEGMENT_MOST, &mut decoded, alloc).expect("decode empty");
        assert_eq!(decoded.as_slice(), raw.as_slice());
        check_segment(decoded.as_slice()).expect("check empty");
    }

    #[test]
    fn previous_magic_and_version_are_refused() {
        let alloc = blaze_own::testing::heap().alloc();
        let raw = make_segment(&[(Tag(7), b"hello blaze wire segment")], alloc);
        let mut wire = OwnVec::new_in(alloc);
        encode_wire_segment(raw.as_slice(), &mut wire, alloc).expect("encode");

        let mut bad_magic = OwnVec::new_in(alloc);
        bad_magic.extend_from_slice(wire.as_slice());
        bad_magic.as_mut_slice()[0..4].copy_from_slice(b"BZS0");
        let mut out = OwnVec::new_in(alloc);
        assert!(matches!(decode_wire_segment(bad_magic.as_slice(), SEGMENT_MOST, &mut out, alloc), Err(WireSegmentError::BadMagic { found: [b'B', b'Z', b'S', b'0'] })));

        let mut bad_v0 = OwnVec::new_in(alloc);
        bad_v0.extend_from_slice(wire.as_slice());
        bad_v0.as_mut_slice()[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(decode_wire_segment(bad_v0.as_slice(), SEGMENT_MOST, &mut out, alloc), Err(WireSegmentError::UnsupportedVersion { found: 0, wanted: VERSION })));

        let mut bad_v2 = OwnVec::new_in(alloc);
        bad_v2.extend_from_slice(wire.as_slice());
        bad_v2.as_mut_slice()[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(decode_wire_segment(bad_v2.as_slice(), SEGMENT_MOST, &mut out, alloc), Err(WireSegmentError::UnsupportedVersion { found: 2, wanted: VERSION })));
    }

    #[test]
    fn tag_partitioned_wire_beats_monolithic_zstd_and_round_trips() {
        let alloc = blaze_own::testing::heap().alloc();
        // Interleave three structurally distinct record schemas (sorted by random BLAKE3 keys in BLPS).
        let mut payloads: OwnVec<(Tag, OwnVec<u8, _>), _> = OwnVec::new_in(alloc);
        for i in 0..90u32 {
            // Schema A: Rust-like items text with shared symbols.
            let mut a = OwnVec::new_in(alloc);
            a.extend_from_slice(b"pub fn item_");
            a.extend_from_slice(&i.to_le_bytes());
            a.extend_from_slice(b"(ctx: &Context, alloc: Alloc<'_>) -> Result<ItemsRecord, Error> { /* body */ }\n");
            payloads.push((Tag(10), a));
            // Schema B: Structured columnar header + monotonic u32 table.
            let mut b = OwnVec::with_capacity_in(128, alloc);
            b.extend_from_slice(b"BLIT\x0e\x00\x00\x00");
            for j in 0..30u32 {
                b.extend_from_slice(&(i * 16 + j).to_le_bytes());
            }
            payloads.push((Tag(20), b));
            // Schema C: Repetitive name-path record.
            let mut c = OwnVec::new_in(alloc);
            c.extend_from_slice(b"core::ops::function::FnOnce::call_once::<crate::module_");
            c.push(b'0' + (i % 5) as u8);
            c.extend_from_slice(b"::Item_");
            c.extend_from_slice(&i.to_le_bytes());
            c.extend_from_slice(b">\0");
            payloads.push((Tag(30), c));
        }
        let mut borrowed: OwnVec<(Tag, &[u8]), _> = OwnVec::with_capacity_in(payloads.len(), alloc);
        for (t, b) in payloads.as_slice() {
            borrowed.push((*t, b.as_slice()));
        }
        let raw = make_segment(borrowed.as_slice(), alloc);

        let mut wire = OwnVec::new_in(alloc);
        encode_wire_segment(raw.as_slice(), &mut wire, alloc).expect("encode multi-tag");

        let mono = blaze_zstd::compress_in(raw.as_slice(), CompressOpts { level: Level::Default, checksum: false }, alloc).expect("mono zstd");
        assert!(wire.len() < raw.len() / 2, "BZS1 did not achieve >2x compression on structured records: wire={} raw={}", wire.len(), raw.len());
        assert!(wire.len() < mono.len(), "BZS1 ({}) should beat monolithic Zstd ({}) on multi-tag BLPS", wire.len(), mono.len());

        let mut decoded = OwnVec::new_in(alloc);
        decode_wire_segment(wire.as_slice(), SEGMENT_MOST, &mut decoded, alloc).expect("decode multi-tag");
        assert_eq!(decoded.as_slice(), raw.as_slice(), "BZS1 round-trip must be byte-identical to BLPS");
        check_segment(decoded.as_slice()).expect("decoded BLPS passes check_segment");
    }

    #[test]
    fn random_multi_tag_segments_round_trip_deterministically_and_refuse_bounds() {
        let alloc = blaze_own::testing::heap().alloc();
        blaze_sim::check(Config::cases(48), |tape| {
            let n = tape.below(32, "record count") as usize;
            let mut owned: OwnVec<(Tag, OwnVec<u8, _>), _> = OwnVec::with_capacity_in(n, alloc);
            for _ in 0..n {
                let tag = Tag(tape.below(6, "tag") as u16);
                let len = tape.below(256, "payload len") as usize;
                let structured = tape.one_in(2, "structured payload");
                let mut bytes = OwnVec::with_capacity_in(len, alloc);
                if structured {
                    let pat = b"blaze::items::record::column::";
                    for k in 0..len {
                        bytes.push(pat[k % pat.len()] ^ (tag.0 as u8));
                    }
                } else {
                    for _ in 0..len {
                        bytes.push(tape.below(256, "byte") as u8);
                    }
                }
                owned.push((tag, bytes));
            }
            let mut borrowed: OwnVec<(Tag, &[u8]), _> = OwnVec::with_capacity_in(owned.len(), alloc);
            for (t, b) in owned.as_slice() {
                borrowed.push((*t, b.as_slice()));
            }
            let raw = make_segment(borrowed.as_slice(), alloc);

            let mut wire_a = OwnVec::new_in(alloc);
            let mut wire_b = OwnVec::new_in(alloc);
            encode_wire_segment(raw.as_slice(), &mut wire_a, alloc).expect("encode a");
            encode_wire_segment(raw.as_slice(), &mut wire_b, alloc).expect("encode b");
            assert_eq!(wire_a.as_slice(), wire_b.as_slice(), "encoding must be deterministic");

            let mut decoded = OwnVec::new_in(alloc);
            decode_wire_segment(wire_a.as_slice(), raw.len() as u64, &mut decoded, alloc).expect("decode");
            assert_eq!(decoded.as_slice(), raw.as_slice());

            // Caller max_raw_len bound is enforced before allocating.
            if !raw.is_empty() {
                let mut too_small = OwnVec::new_in(alloc);
                assert!(matches!(
                    decode_wire_segment(wire_a.as_slice(), (raw.len() - 1) as u64, &mut too_small, alloc),
                    Err(WireSegmentError::TooLarge { what: "raw segment length", .. })
                ));
            }

            // Truncated wire is always refused.
            if wire_a.len() > 1 {
                let cut = tape.below(wire_a.len() as u64, "truncation point") as usize;
                let mut trunc_out = OwnVec::new_in(alloc);
                assert!(decode_wire_segment(&wire_a.as_slice()[..cut], SEGMENT_MOST, &mut trunc_out, alloc).is_err());
            }
        });
    }

    #[test]
    fn corrupt_wire_is_refused_by_decode_or_check_segment() {
        let alloc = blaze_own::testing::heap().alloc();
        let raw = make_segment(
            &[
                (Tag(1), b"alpha payload alpha payload alpha payload alpha payload"),
                (Tag(2), b"beta payload beta payload beta payload beta payload"),
                (Tag(1), b"gamma payload gamma payload gamma payload gamma payload"),
            ],
            alloc,
        );
        let mut wire = OwnVec::new_in(alloc);
        encode_wire_segment(raw.as_slice(), &mut wire, alloc).expect("encode");
        let want_digest = check_segment(raw.as_slice()).expect("raw check").digest;

        // Flip every byte of the wire buffer in turn: every single flip must either fail
        // `decode_wire_segment` OR fail `check_segment` / digest match!
        let mut decoded = OwnVec::new_in(alloc);
        let mut mutated = OwnVec::with_capacity_in(wire.len(), alloc);
        for pos in 0..wire.len() {
            mutated.clear();
            mutated.extend_from_slice(wire.as_slice());
            mutated.as_mut_slice()[pos] ^= 0x01;
            let accepted = match decode_wire_segment(mutated.as_slice(), SEGMENT_MOST, &mut decoded, alloc) {
                Err(_) => false,
                Ok(()) => match check_segment(decoded.as_slice()) {
                    Ok(desc) => desc.digest == want_digest,
                    Err(_) => false,
                },
            };
            assert!(!accepted, "bit flip at wire byte {pos} slipped past decode + check_segment!");
        }
    }

    #[test]
    fn decode_wire_segment_verified_matches_check_segment_and_rejects_every_bit_flip() {
        let alloc = blaze_own::testing::heap().alloc();
        let raw = make_segment(
            &[
                (Tag(1), b"alpha payload alpha payload alpha payload alpha payload"),
                (Tag(2), b"beta payload beta payload beta payload beta payload"),
                (Tag(1), b"gamma payload gamma payload gamma payload gamma payload"),
                (Tag(9), b"tiny"),
            ],
            alloc,
        );
        let mut wire = OwnVec::new_in(alloc);
        encode_wire_segment(raw.as_slice(), &mut wire, alloc).expect("encode");
        let want_desc = check_segment(raw.as_slice()).expect("raw check");

        let verified = decode_wire_segment_verified(wire.as_slice(), raw.len() as u64, alloc).expect("verified decode");
        assert_eq!(verified.described(), want_desc);
        assert_eq!(verified.len(), want_desc.count as usize);

        // Compare every record byte-for-byte with reconstructed BLPS.
        let mut blps = OwnVec::new_in(alloc);
        decode_wire_segment(wire.as_slice(), raw.len() as u64, &mut blps, alloc).expect("decode");
        assert_eq!(blps.as_slice(), raw.as_slice());
        for (i, (key, tag, payload)) in verified.iter().enumerate() {
            let at = want_desc.index_at as usize + i * ENTRY;
            let entry = Entry::decode(&blps.as_slice()[at..at + ENTRY]).expect("entry");
            assert_eq!(key, entry.key);
            assert_eq!(tag, entry.tag);
            assert_eq!(payload, &blps.as_slice()[entry.at as usize..entry.at as usize + entry.len as usize]);
        }

        // Flip every byte of `wire`: `decode_wire_segment_verified` must reject every single one.
        let mut mutated = OwnVec::with_capacity_in(wire.len(), alloc);
        for pos in 0..wire.len() {
            mutated.clear();
            mutated.extend_from_slice(wire.as_slice());
            mutated.as_mut_slice()[pos] ^= 0x01;
            assert!(
                decode_wire_segment_verified(mutated.as_slice(), SEGMENT_MOST, alloc).is_err(),
                "bit flip at wire byte {pos} slipped past decode_wire_segment_verified!"
            );
        }
    }
}
