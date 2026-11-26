//! `BZR1`: selective fast Zstandard (`bstd::zstd`) envelope for large decoded-on-read CAS records.
//!
//! Why here (and not inside `blaze-cas` `BLPS` segments, rules 28 & 51):
//! - `BLPS` segment mappings lend zero-copy `Aligned16<'m>` slices directly from read-only `mmap`s
//!   for hot in-place tables (`FileItems`, `BEP6` pages, `Names`, `KeptBody`, `Code` records).
//! - Decoded-on-read records (such as `TypedBody` checked columns in `engine_body_record.rs`)
//!   already deserialize into caller-owned tables, so transparently compressing large (`>= 4 KiB`)
//!   compressible payloads at `Level::Fast` cuts on-disk footprint and cold read bandwidth by 50%+
//!   without touching any zero-copy mmap path or adding any buffer cache.
//! - Small (`< 4 KiB`) or incompressible payloads (`>= 85%` of raw size after framing) stay raw
//!   with zero wrapper overhead and zero decompression CPU on read.
//! - Deterministic RFC 8878 output (`blaze-zstd`, rule 25) with `XXH64` content checksum and
//!   strict caller `max_bytes` bounds checked before allocation (rules 18, 21, 26).

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, Vec};
use bstd::zstd::{CompressOpts, Level, ZstdError, ZstdLimits};

/// Wire magic for selective Zstd-compressed CAS record envelopes (`BZR1`).
pub(crate) const MAGIC: [u8; 4] = *b"BZR1";

/// Wire format version for `BZR1` envelopes.
pub(crate) const VERSION: u8 = 1;

/// Fixed `BZR1` envelope header length in bytes:
/// `0..4` magic (`BZR1`), `4` version (`1`), `5` codec (`0 = Raw`, `1 = Zstd`),
/// `6..8` reserved zero, `8..12` `raw_len: u32` (little-endian).
pub(crate) const HEADER_LEN: usize = 12;

/// Minimum raw payload size (4 KiB) before attempting Zstd compression.
pub(crate) const COMPRESS_MIN_BYTES: usize = 4096;

/// Maximum compressed-plus-header percentage of raw size (85%) to accept compression; payloads that
/// do not shrink by at least 15% stay raw.
pub(crate) const COMPRESS_MAX_RATIO_PCT: usize = 85;

/// Default maximum decompressed record size (64 MiB), matching `ZstdLimits::USUAL`.
pub(crate) const DEFAULT_MAX_DECODED_BYTES: usize = 64 << 20;

/// Maximum Zstd window size accepted on decode (8 MiB, matching `ZstdLimits::USUAL`).
const MAX_WINDOW_BYTES: usize = 8 << 20;

/// Fast single-probe Zstd with RFC 8878 32-bit `XXH64` content checksum for corruption detection.
const FAST_ZSTD_OPTS: CompressOpts = CompressOpts { level: Level::Fast, checksum: true };

/// Per-record codec byte stored at header offset `5`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Codec {
    Raw = 0,
    Zstd = 1,
}

impl Codec {
    const fn to_byte(self) -> u8 {
        match self {
            Codec::Raw => 0,
            Codec::Zstd => 1,
        }
    }

    const fn from_byte(byte: u8) -> Option<Codec> {
        match byte {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Zstd),
            _ => None,
        }
    }
}

/// Output of [`encode_selective_in`]: either the untouched raw slice (zero copy when small or
/// incompressible) or a newly allocated `BZR1` compressed envelope in the caller's allocator.
pub(crate) enum Encoded<'a, A: Allocator> {
    Raw(&'a [u8]),
    Compressed(Vec<u8, A>),
}

impl<A: Allocator> Encoded<'_, A> {
    /// Stored bytes to hash and put into the CAS.
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Encoded::Raw(bytes) => bytes,
            Encoded::Compressed(bytes) => bytes.as_slice(),
        }
    }

    /// Whether the record was compressed into a `BZR1` envelope.
    #[cfg(test)]
    pub(crate) fn is_compressed(&self) -> bool {
        matches!(self, Encoded::Compressed(_))
    }
}

/// Output of [`decode_envelope_in`] / [`decode_if_envelope_in`]: zero-copy borrow when raw, or
/// decompressed bytes in the caller's scratch allocator (e.g. `cx.frame()`).
pub(crate) enum Decoded<'a, A: Allocator> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8, A>),
}

impl<A: Allocator> Decoded<'_, A> {
    /// Original uncompressed record bytes.
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Decoded::Borrowed(bytes) => bytes,
            Decoded::Owned(bytes) => bytes.as_slice(),
        }
    }

    /// Whether decompression ran.
    #[cfg(test)]
    pub(crate) fn was_compressed(&self) -> bool {
        matches!(self, Decoded::Owned(_))
    }
}

/// Typed failure encoding or decoding a `BZR1` record envelope (rules 3, 19, 26).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RecordCodecError {
    /// Stored envelope was shorter than [`HEADER_LEN`].
    TooShort { len: usize },
    /// Stored envelope did not begin with [`MAGIC`] (`BZR1`).
    BadMagic { found: [u8; 4] },
    /// Stored envelope carried an unsupported format version.
    UnsupportedVersion { found: u8, wanted: u8 },
    /// Stored envelope carried an unknown codec byte.
    UnknownCodec { codec: u8 },
    /// Reserved header bytes `6..8` were non-zero.
    ReservedNonZero { at: usize },
    /// Raw payload exceeded `u32::MAX` bytes.
    InputTooLarge { len: usize },
    /// Declared uncompressed length exceeded the caller's `max_bytes` bound before allocation.
    OutputTooLarge { declared: usize, limit: usize },
    /// Decoded byte length did not match the header's declared `raw_len`.
    LengthMismatch { declared: usize, actual: usize },
    /// Underlying `bstd::zstd` compression or decompression failure (including `XXH64` mismatch).
    Zstd(ZstdError),
    /// Caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<ZstdError> for RecordCodecError {
    fn from(error: ZstdError) -> Self {
        RecordCodecError::Zstd(error)
    }
}

impl From<AllocFailure> for RecordCodecError {
    fn from(error: AllocFailure) -> Self {
        RecordCodecError::Alloc(error)
    }
}

impl fmt::Display for RecordCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecordCodecError::TooShort { len } => {
                write!(f, "BZR1 record envelope is {len} bytes, shorter than the {HEADER_LEN}-byte header (store/record_codec.rs)")
            }
            RecordCodecError::BadMagic { found } => {
                write!(f, "BZR1 record envelope expected magic {MAGIC:?}, found {found:?} (store/record_codec.rs)")
            }
            RecordCodecError::UnsupportedVersion { found, wanted } => {
                write!(f, "BZR1 record envelope version {found} is unsupported; this blaze reads version {wanted} (store/record_codec.rs)")
            }
            RecordCodecError::UnknownCodec { codec } => {
                write!(f, "BZR1 record envelope codec byte {codec} is neither 0 (raw) nor 1 (zstd) (store/record_codec.rs)")
            }
            RecordCodecError::ReservedNonZero { at } => {
                write!(f, "BZR1 record envelope has non-zero reserved byte at offset {at} (store/record_codec.rs)")
            }
            RecordCodecError::InputTooLarge { len } => {
                write!(f, "BZR1 record payload of {len} bytes exceeds u32::MAX (store/record_codec.rs)")
            }
            RecordCodecError::OutputTooLarge { declared, limit } => {
                write!(f, "BZR1 record declares {declared} uncompressed bytes, past limit {limit} (store/record_codec.rs)")
            }
            RecordCodecError::LengthMismatch { declared, actual } => {
                write!(f, "BZR1 record declared {declared} bytes but decoded {actual} bytes (store/record_codec.rs)")
            }
            RecordCodecError::Zstd(error) => {
                write!(f, "BZR1 record zstd codec failed: {error} (store/record_codec.rs)")
            }
            RecordCodecError::Alloc(error) => {
                write!(f, "BZR1 record allocation failed: {error} (store/record_codec.rs)")
            }
        }
    }
}

impl core::error::Error for RecordCodecError {}

/// Exact overflow-free ceiling `floor(raw_len * COMPRESS_MAX_RATIO_PCT / 100)`.
const fn max_accepted_bytes(raw_len: usize) -> usize {
    (raw_len / 100)
        .saturating_mul(COMPRESS_MAX_RATIO_PCT)
        .saturating_add(((raw_len % 100).saturating_mul(COMPRESS_MAX_RATIO_PCT)) / 100)
}

/// Returns `true` if `bytes` begins with the `BZR1` envelope magic.
#[inline]
pub(crate) fn is_envelope(bytes: &[u8]) -> bool {
    bytes.get(..4).is_some_and(|head| head == MAGIC.as_slice())
}

/// Writes a `BZR1` header + `body` into a freshly allocated `Vec<u8, A>`.
fn write_envelope<A: Allocator + Copy>(
    codec: Codec,
    raw_len: u32,
    body: &[u8],
    alloc: A,
) -> Result<Vec<u8, A>, RecordCodecError> {
    let total = HEADER_LEN.checked_add(body.len()).ok_or(RecordCodecError::InputTooLarge { len: body.len() })?;
    let [m0, m1, m2, m3] = MAGIC;
    let [l0, l1, l2, l3] = raw_len.to_le_bytes();
    let header: [u8; HEADER_LEN] = [m0, m1, m2, m3, VERSION, codec.to_byte(), 0, 0, l0, l1, l2, l3];
    let mut out = Vec::try_with_capacity_in(total, alloc)?;
    out.try_extend_from_slice(&header)?;
    out.try_extend_from_slice(body)?;
    Ok(out)
}

/// Selectively compresses `raw` into a `BZR1` Zstd envelope when `raw.len() >= 4 KiB` and the
/// framed output is strictly smaller than `85%` of `raw.len()`; otherwise returns
/// [`Encoded::Raw`] with zero framing overhead.
///
/// Use for record types whose raw bytes already carry a distinct magic that cannot collide with
/// [`MAGIC`] (`b"BZR1"`), such as `TypedBody` (`BZCHKD07`).
pub(crate) fn encode_selective_in<'a, A: Allocator + Copy>(
    raw: &'a [u8],
    alloc: A,
) -> Result<Encoded<'a, A>, RecordCodecError> {
    if raw.len() < COMPRESS_MIN_BYTES {
        return Ok(Encoded::Raw(raw));
    }
    let raw_len = u32::try_from(raw.len()).map_err(|_| RecordCodecError::InputTooLarge { len: raw.len() })?;
    let compressed = bstd::zstd::compress_in(raw, FAST_ZSTD_OPTS, alloc)?;
    let framed_len = HEADER_LEN.saturating_add(compressed.len());
    if framed_len < max_accepted_bytes(raw.len()) {
        let out = write_envelope(Codec::Zstd, raw_len, compressed.as_slice(), alloc)?;
        Ok(Encoded::Compressed(out))
    } else {
        Ok(Encoded::Raw(raw))
    }
}

/// Always wraps `raw` in a `BZR1` envelope (`Codec::Zstd` when `>= 4 KiB` and `< 85%`, else
/// `Codec::Raw`), suitable for arbitrary payloads without an inner magic.
#[cfg(test)]
pub(crate) fn encode_envelope_in<A: Allocator + Copy>(
    raw: &[u8],
    alloc: A,
) -> Result<Vec<u8, A>, RecordCodecError> {
    let raw_len = u32::try_from(raw.len()).map_err(|_| RecordCodecError::InputTooLarge { len: raw.len() })?;
    if raw.len() >= COMPRESS_MIN_BYTES {
        let compressed = bstd::zstd::compress_in(raw, FAST_ZSTD_OPTS, alloc)?;
        if HEADER_LEN.saturating_add(compressed.len()) < max_accepted_bytes(raw.len()) {
            return write_envelope(Codec::Zstd, raw_len, compressed.as_slice(), alloc);
        }
    }
    write_envelope(Codec::Raw, raw_len, raw, alloc)
}

/// Planted bug controls for DST verification (rule 43).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DecodePlant {
    None,
    /// Skips version check, proving the oracle catches previous/future version acceptance.
    IgnoreVersion,
    /// Skips final `raw_len` equality check, proving the oracle catches length spoofing.
    IgnoreLengthMismatch,
}

/// Decodes a `BZR1` envelope into `alloc` (or borrows zero-copy when `Codec::Raw`), enforcing
/// `max_bytes` before allocation and verifying magic, version, reserved bits, checksum, and exact
/// uncompressed length.
pub(crate) fn decode_envelope_in<'a, A: Allocator + Copy>(
    stored: &'a [u8],
    max_bytes: usize,
    alloc: A,
) -> Result<Decoded<'a, A>, RecordCodecError> {
    decode_envelope_impl(
        stored,
        max_bytes,
        alloc,
        #[cfg(test)]
        DecodePlant::None,
    )
}

fn decode_envelope_impl<'a, A: Allocator + Copy>(
    stored: &'a [u8],
    max_bytes: usize,
    alloc: A,
    #[cfg(test)] plant: DecodePlant,
) -> Result<Decoded<'a, A>, RecordCodecError> {
    let Some((header, body)) = stored.split_at_checked(HEADER_LEN) else {
        return Err(RecordCodecError::TooShort { len: stored.len() });
    };
    let found_magic: [u8; 4] = header
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(RecordCodecError::TooShort { len: stored.len() })?;
    if found_magic != MAGIC {
        return Err(RecordCodecError::BadMagic { found: found_magic });
    }
    let version = header.get(4).copied().ok_or(RecordCodecError::TooShort { len: stored.len() })?;
    #[cfg(test)]
    let check_version = plant != DecodePlant::IgnoreVersion;
    #[cfg(not(test))]
    let check_version = true;
    if check_version && version != VERSION {
        return Err(RecordCodecError::UnsupportedVersion { found: version, wanted: VERSION });
    }
    let codec_byte = header.get(5).copied().ok_or(RecordCodecError::TooShort { len: stored.len() })?;
    let codec = Codec::from_byte(codec_byte).ok_or(RecordCodecError::UnknownCodec { codec: codec_byte })?;
    if header.get(6).copied().unwrap_or(1) != 0 {
        return Err(RecordCodecError::ReservedNonZero { at: 6 });
    }
    if header.get(7).copied().unwrap_or(1) != 0 {
        return Err(RecordCodecError::ReservedNonZero { at: 7 });
    }
    let len_bytes: [u8; 4] = header
        .get(8..12)
        .and_then(|s| s.try_into().ok())
        .ok_or(RecordCodecError::TooShort { len: stored.len() })?;
    let declared_u32 = u32::from_le_bytes(len_bytes);
    let declared = usize::try_from(declared_u32).map_err(|_| RecordCodecError::OutputTooLarge { declared: usize::MAX, limit: max_bytes })?;
    if declared > max_bytes {
        return Err(RecordCodecError::OutputTooLarge { declared, limit: max_bytes });
    }
    #[cfg(test)]
    let check_len = plant != DecodePlant::IgnoreLengthMismatch;
    #[cfg(not(test))]
    let check_len = true;
    match codec {
        Codec::Raw => {
            if check_len && body.len() != declared {
                return Err(RecordCodecError::LengthMismatch { declared, actual: body.len() });
            }
            Ok(Decoded::Borrowed(body))
        }
        Codec::Zstd => {
            let limits = ZstdLimits {
                most_output_bytes: declared,
                most_window_bytes: MAX_WINDOW_BYTES,
                most_frames: 1,
            };
            let decoded = bstd::zstd::decompress_in(body, limits, alloc)?;
            if check_len && decoded.len() != declared {
                return Err(RecordCodecError::LengthMismatch { declared, actual: decoded.len() });
            }
            Ok(Decoded::Owned(decoded))
        }
    }
}

/// Decodes `stored` if it begins with `BZR1`; otherwise returns a zero-copy [`Decoded::Borrowed`].
pub(crate) fn decode_if_envelope_in<'a, A: Allocator + Copy>(
    stored: &'a [u8],
    max_bytes: usize,
    alloc: A,
) -> Result<Decoded<'a, A>, RecordCodecError> {
    if is_envelope(stored) {
        decode_envelope_in(stored, max_bytes, alloc)
    } else {
        Ok(Decoded::Borrowed(stored))
    }
}

#[cfg(test)]
mod tests {
    use blaze_sim::Config;

    use super::{
        COMPRESS_MIN_BYTES, DEFAULT_MAX_DECODED_BYTES, DecodePlant, HEADER_LEN, MAGIC, RecordCodecError, VERSION,
        decode_envelope_impl, decode_envelope_in, decode_if_envelope_in, encode_envelope_in, encode_selective_in,
    };

    /// Rule 26: a file/record of a previous layout (`BZR1` version 0, future version 2, or legacy
    /// `BZR0` magic) and non-zero reserved header bytes are refused by name.
    #[test]
    fn previous_bzr1_layout_is_refused_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let raw = b"BZCHKD07 sample record payload";
        let valid = encode_envelope_in(raw, alloc).expect("encode");

        // Version 0 (previous layout) and version 2 (future layout) refused.
        for bad_ver in [0u8, 2u8, 255u8] {
            let mut mutated = valid.clone();
            mutated[4] = bad_ver;
            assert_eq!(
                decode_envelope_in(mutated.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).err(),
                Some(RecordCodecError::UnsupportedVersion { found: bad_ver, wanted: VERSION }),
                "version {bad_ver} must be refused"
            );
            // Planted-bug control: ignoring version accepts version 0!
            assert!(
                decode_envelope_impl(mutated.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc, DecodePlant::IgnoreVersion).is_ok(),
                "planted IgnoreVersion must be caught by version refusal check"
            );
        }

        // Previous/foreign magic refused.
        let mut bad_magic = valid.clone();
        bad_magic[0..4].copy_from_slice(b"BZR0");
        assert_eq!(
            decode_envelope_in(bad_magic.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).err(),
            Some(RecordCodecError::BadMagic { found: *b"BZR0" })
        );

        // Non-zero reserved bytes refused.
        for at in [6usize, 7usize] {
            let mut bad_res = valid.clone();
            bad_res[at] = 1;
            assert_eq!(
                decode_envelope_in(bad_res.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).err(),
                Some(RecordCodecError::ReservedNonZero { at })
            );
        }
    }

    #[test]
    fn selective_compression_shrinks_large_structured_records_and_skips_small_or_incompressible() {
        let alloc = blaze_own::testing::heap().alloc();

        // 1. Small (< 4 KiB) record stays untouched Raw (zero overhead, zero copy on decode).
        let mut small = blaze_own::Vec::with_capacity_in(2048, alloc);
        small.extend_from_slice(b"BZCHKD07");
        for i in 0..2040usize {
            small.push(u8::try_from(i % 17).unwrap_or(0));
        }
        let enc_small = encode_selective_in(small.as_slice(), alloc).expect("encode small");
        assert!(!enc_small.is_compressed(), "records under 4 KiB must stay raw");
        assert_eq!(enc_small.as_slice(), small.as_slice());
        let dec_small = decode_if_envelope_in(enc_small.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).expect("decode small");
        assert!(!dec_small.was_compressed());
        assert_eq!(dec_small.as_slice(), small.as_slice());

        // 2. Large structured record (32 KiB, realistic TypedBody-like column layout) compresses > 50%.
        let mut structured = blaze_own::Vec::with_capacity_in(32768, alloc);
        structured.extend_from_slice(b"BZCHKD07");
        for i in 0..8190u32 {
            let tag = i % 12;
            let ty_id = (i / 16) % 64;
            let word = (tag << 24) | ty_id;
            structured.extend_from_slice(&word.to_le_bytes());
        }
        assert!(structured.len() >= COMPRESS_MIN_BYTES);
        let enc_struct = encode_selective_in(structured.as_slice(), alloc).expect("encode structured");
        assert!(enc_struct.is_compressed(), "structured 32 KiB record must compress");
        assert!(
            enc_struct.as_slice().len() * 2 < structured.len(),
            "expected >50% compression on structured record: {} -> {} bytes",
            structured.len(),
            enc_struct.as_slice().len()
        );
        let dec_struct = decode_if_envelope_in(enc_struct.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).expect("decode structured");
        assert!(dec_struct.was_compressed());
        assert_eq!(dec_struct.as_slice(), structured.as_slice());

        // 3. Large incompressible random-like payload (16 KiB BLAKE3 stream) stays raw (<85% gate).
        let mut incomp = blaze_own::Vec::with_capacity_in(16384, alloc);
        incomp.extend_from_slice(b"BZCHKD07");
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        while incomp.len() < 16384 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            incomp.extend_from_slice(&v.to_le_bytes());
        }
        let enc_incomp = encode_selective_in(incomp.as_slice(), alloc).expect("encode incompressible");
        assert!(!enc_incomp.is_compressed(), "incompressible record must stay raw (85% gate)");
        assert_eq!(enc_incomp.as_slice(), incomp.as_slice());
    }

    /// The invariant (rules 21, 25, 26, 43): for any payload drawn from the tape, `BZR1` encoding
    /// is byte-for-byte deterministic across runs, round-trips byte-identically, rejects bit-flips
    /// and truncations loudly, and respects `max_bytes` before allocation.
    #[test]
    fn bzr1_deterministic_roundtrip_and_corruption_rejection_dst() {
        let alloc = blaze_own::testing::heap().alloc();
        blaze_sim::check(Config::cases(128), |tape| {
            let kind = tape.below(4, "payload kind");
            let len = usize::try_from(tape.int_in(0..=16_384, "payload len")).unwrap_or(0);
            let mut raw = blaze_own::Vec::with_capacity_in(len, alloc);
            let mut x = tape.next_u64("seed") | 1;
            match kind {
                0 => {
                    // Structured repetitive u32 columns (high compressibility).
                    for i in 0..len {
                        let b = u8::try_from((i / 4) % 19).unwrap_or(0);
                        raw.push(b);
                    }
                }
                1 => {
                    // Constant run (RLE zstd block).
                    let byte = u8::try_from(tape.below(256, "fill byte")).unwrap_or(0);
                    raw.resize(len, byte);
                }
                2 => {
                    // Mixed headers + pseudo-random regions.
                    for i in 0..len {
                        if (i % 64) < 40 {
                            raw.push(u8::try_from(i % 8).unwrap_or(0));
                        } else {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            raw.push(u8::try_from(x & 0xff).unwrap_or(0));
                        }
                    }
                }
                _ => {
                    // Incompressible random bytes.
                    for _ in 0..len {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        raw.push(u8::try_from(x & 0xff).unwrap_or(0));
                    }
                }
            }

            // Determinism (rule 25): two encodes produce identical bytes.
            let enc1 = encode_envelope_in(raw.as_slice(), alloc).expect("encode 1");
            let enc2 = encode_envelope_in(raw.as_slice(), alloc).expect("encode 2");
            assert_eq!(enc1.as_slice(), enc2.as_slice(), "BZR1 encode must be deterministic");
            assert_eq!(&enc1.as_slice()[..4], MAGIC.as_slice());

            // Exact byte-for-byte round-trip (rule 26).
            let dec = decode_envelope_in(enc1.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).expect("decode");
            assert_eq!(dec.as_slice(), raw.as_slice(), "BZR1 decode must match raw bytes");

            // Decompression bomb ceiling enforced before allocation.
            if len > 0 {
                let too_small = decode_envelope_in(enc1.as_slice(), len - 1, alloc);
                assert_eq!(too_small.err(), Some(RecordCodecError::OutputTooLarge { declared: len, limit: len - 1 }));
            }

            // Corruption rejection (rule 21): flip any bit in a compressed envelope or header.
            let compressed_flag = enc1.as_slice()[5] == 1;
            if compressed_flag && enc1.len() > HEADER_LEN {
                let flip_at = usize::try_from(tape.below(u64::try_from(enc1.len()).unwrap_or(1), "flip byte")).unwrap_or(0);
                let flip_bit = 1u8 << u8::try_from(tape.below(8, "flip bit")).unwrap_or(0);
                let mut corrupt = enc1.clone();
                corrupt[flip_at] ^= flip_bit;
                let res = decode_envelope_in(corrupt.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc);
                assert!(res.is_err(), "corrupted compressed BZR1 envelope at byte {flip_at} must be rejected");
            }

            // Truncation rejection.
            if enc1.len() > 1 {
                let cut = usize::try_from(tape.below(u64::try_from(enc1.len()).unwrap_or(1), "truncate at")).unwrap_or(0);
                let res = decode_envelope_in(&enc1.as_slice()[..cut], DEFAULT_MAX_DECODED_BYTES, alloc);
                assert!(res.is_err(), "truncated BZR1 envelope ({cut}/{}) must be rejected", enc1.len());
            }

            // Planted length-spoofing control: claiming +1 byte in header must fail normally,
            // while IgnoreLengthMismatch misses it on raw envelopes.
            if !compressed_flag && len > 0 {
                let mut spoofed = enc1.clone();
                let fake_len = u32::try_from(len + 1).unwrap_or(u32::MAX);
                spoofed[8..12].copy_from_slice(&fake_len.to_le_bytes());
                assert!(decode_envelope_in(spoofed.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc).is_err());
                assert!(decode_envelope_impl(spoofed.as_slice(), DEFAULT_MAX_DECODED_BYTES, alloc, DecodePlant::IgnoreLengthMismatch).is_ok());
            }
        });
    }
}
