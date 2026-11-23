//! Pure `#![no_std]`, strictly bounded PKZIP archive reader and writer (Store and raw Deflate).
//!
//! - Supports PKZIP APPNOTE compression methods Store (`0`) and raw Deflate (`8`, via
//!   `blaze_deflate`), with IEEE CRC-32 (`crate::digest::Crc32`) verification on every read.
//! - Rejects Zip64 archives, multi-disk archives, encrypted entries, unsupported compression
//!   methods, invalid UTF-8 entry names, and zip-slip paths (leading `/`, `..` or `.` segments,
//!   `\`, NUL bytes, drive prefixes, or empty segments) with typed [`ZipError`] values.
//! - Enforces caller-supplied [`ZipLimits`] on entry count, name length, per-entry uncompressed
//!   bytes (both declared and actual inflated bytes against zip bombs), and total uncompressed
//!   archive bytes.
//! - Deterministic output from [`ZipWriter`]: fixed DOS timestamp (`1980-01-01 00:00:00`),
//!   UTF-8 language encoding flag (`0x0800`), and deterministic `blaze_deflate`.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexes, and
//!   zero recursion.

use core::alloc::Allocator;
use core::fmt;

use blaze_deflate::{DeflateError, Level, Limits as InflateLimits, MAX_RATIO, Why as InflateWhy};
use blaze_own::{AllocFailure, Vec};

use crate::digest::Crc32;

const LFH_SIG: u32 = 0x0403_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_LOCATOR_SIG: u32 = 0x0706_4b50;

const LFH_FIXED_LEN: usize = 30;
const CDH_FIXED_LEN: usize = 46;
const EOCD_FIXED_LEN: usize = 22;
const ZIP64_LOCATOR_LEN: usize = 20;
const MAX_EOCD_COMMENT: usize = 65_535;

const VERSION_NEEDED_20: u16 = 20;
const VERSION_MADE_BY_UNIX_20: u16 = (3u16 << 8) | 20u16;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
const FLAG_STRONG_ENCRYPTED: u16 = 0x0040;
const FLAG_UTF8: u16 = 0x0800;

const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// Fixed deterministic DOS date (`1980-01-01`) and time (`00:00:00`).
const DOS_TIME_ZERO: u16 = 0x0000;
const DOS_DATE_1980_01_01: u16 = 0x0021;

/// External file attributes: regular file `0644` (UNIX) / directory `0755` + DOS dir bit.
const EXT_ATTR_FILE: u32 = 0x81A4_0000;
const EXT_ATTR_DIR: u32 = 0x41ED_0010;

/// Compression method supported by this ZIP reader and writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compression {
    /// Method `0`: uncompressed stored payload.
    Store,
    /// Method `8`: raw RFC 1951 DEFLATE stream.
    Deflate,
}

impl Compression {
    #[inline]
    const fn method_code(self) -> u16 {
        match self {
            Compression::Store => METHOD_STORE,
            Compression::Deflate => METHOD_DEFLATE,
        }
    }
}

/// Caller-supplied bounds enforced while opening and extracting untrusted ZIP archives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ZipLimits {
    /// Maximum number of entries allowed in the Central Directory.
    pub most_entries: usize,
    /// Maximum uncompressed bytes allowed for any single entry (both declared and actual).
    pub most_entry_bytes: usize,
    /// Maximum sum of declared uncompressed bytes across all entries in the archive.
    pub most_total_bytes: usize,
    /// Maximum UTF-8 byte length of any single entry name.
    pub most_name_bytes: usize,
}

impl ZipLimits {
    /// Generous default bounds (65,536 entries, 64 MiB/entry, 256 MiB total, 4,096-byte names).
    pub const USUAL: Self = Self {
        most_entries: 65_536,
        most_entry_bytes: 64 << 20,
        most_total_bytes: 256 << 20,
        most_name_bytes: 4_096,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Validated metadata for one entry in a ZIP archive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryMeta<'a> {
    /// Validated relative UTF-8 entry path.
    pub name: &'a str,
    /// Compression method (`Store` or `Deflate`).
    pub compression: Compression,
    /// Expected IEEE CRC-32 of the uncompressed payload.
    pub crc32: u32,
    /// Stored or raw-deflated byte length in the archive.
    pub compressed_size: u32,
    /// Expected uncompressed byte length.
    pub uncompressed_size: u32,
    /// `true` when the entry name ends with `/`.
    pub is_dir: bool,
}

/// Typed error returned while reading or writing a ZIP archive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ZipError {
    /// Archive ended before a header, name, or payload could be read.
    Truncated,
    /// Could not locate a valid End of Central Directory record.
    MissingEocd,
    /// A PKZIP record signature did not match the expected magic number.
    BadSignature { expected: u32, found: u32 },
    /// Entry uses a compression method other than Store (`0`) or Deflate (`8`).
    UnsupportedMethod { method: u16 },
    /// Entry is encrypted (general purpose bit 0 or bit 6 set).
    UnsupportedEncrypted,
    /// Archive uses Zip64 extensions, exceeds 32-bit offsets/sizes, or spans multiple disks.
    UnsupportedZip64,
    /// Entry path failed zip-slip safety validation or did not match its Local File Header.
    UnsafePath,
    /// Entry name was not valid UTF-8.
    InvalidUtf8,
    /// Archive entry count exceeded [`ZipLimits::most_entries`].
    TooManyEntries { count: usize, limit: usize },
    /// Entry name or uncompressed payload exceeded [`ZipLimits::most_name_bytes`] or
    /// [`ZipLimits::most_entry_bytes`].
    EntryTooLarge { bytes: usize, limit: usize },
    /// Total declared uncompressed bytes exceeded [`ZipLimits::most_total_bytes`].
    TotalTooLarge { bytes: usize, limit: usize },
    /// Requested entry index was outside `0..reader.len()`.
    EntryOutOfRange { index: usize, len: usize },
    /// Computed payload CRC-32 did not match the archive header.
    CrcMismatch { expected: u32, found: u32 },
    /// Payload uncompressed or compressed size did not match the archive header.
    SizeMismatch { expected: usize, found: usize },
    /// Raw DEFLATE stream decompression failed.
    Decompress(blaze_deflate::Error),
    /// Raw DEFLATE stream compression failed.
    Compress(DeflateError),
    /// Memory allocation failed.
    Alloc(AllocFailure),
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ZipError::Truncated => f.write_str("bstd::zip: truncated archive or header"),
            ZipError::MissingEocd => {
                f.write_str("bstd::zip: missing End of Central Directory record")
            }
            ZipError::BadSignature { expected, found } => {
                write!(
                    f,
                    "bstd::zip: bad header signature 0x{found:08x} (expected 0x{expected:08x})"
                )
            }
            ZipError::UnsupportedMethod { method } => {
                write!(
                    f,
                    "bstd::zip: unsupported compression method {method} (only Store=0 and Deflate=8 supported)"
                )
            }
            ZipError::UnsupportedEncrypted => {
                f.write_str("bstd::zip: encrypted ZIP entries are not supported")
            }
            ZipError::UnsupportedZip64 => {
                f.write_str("bstd::zip: Zip64 and multi-disk ZIP archives are not supported")
            }
            ZipError::UnsafePath => {
                f.write_str("bstd::zip: unsafe or invalid archive entry path (zip-slip refused)")
            }
            ZipError::InvalidUtf8 => f.write_str("bstd::zip: entry name is not valid UTF-8"),
            ZipError::TooManyEntries { count, limit } => {
                write!(f, "bstd::zip: archive has {count} entries, exceeding limit {limit}")
            }
            ZipError::EntryTooLarge { bytes, limit } => {
                write!(f, "bstd::zip: entry size {bytes} bytes exceeds limit {limit}")
            }
            ZipError::TotalTooLarge { bytes, limit } => {
                write!(
                    f,
                    "bstd::zip: total uncompressed size {bytes} bytes exceeds limit {limit}"
                )
            }
            ZipError::EntryOutOfRange { index, len } => {
                write!(f, "bstd::zip: entry index {index} out of range (archive has {len} entries)")
            }
            ZipError::CrcMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::zip: CRC-32 mismatch (expected 0x{expected:08x}, computed 0x{found:08x})"
                )
            }
            ZipError::SizeMismatch { expected, found } => {
                write!(
                    f,
                    "bstd::zip: payload size mismatch (expected {expected} bytes, found {found})"
                )
            }
            ZipError::Decompress(err) => write!(f, "bstd::zip: deflate decompression failed: {err}"),
            ZipError::Compress(err) => write!(f, "bstd::zip: deflate compression failed: {err}"),
            ZipError::Alloc(failure) => write!(f, "bstd::zip: {failure}"),
        }
    }
}

impl core::error::Error for ZipError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CdEntry<'a> {
    meta: EntryMeta<'a>,
    local_header_offset: usize,
}

/// Strictly bounded reader over an in-memory PKZIP archive byte slice.
pub struct ZipReader<'a, A: Allocator> {
    bytes: &'a [u8],
    entries: Vec<CdEntry<'a>, A>,
    limits: ZipLimits,
}

impl<A: Allocator> fmt::Debug for ZipReader<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZipReader")
            .field("bytes_len", &self.bytes.len())
            .field("entries", &self.entries.as_slice())
            .field("limits", &self.limits)
            .finish()
    }
}

impl<A: Allocator> PartialEq for ZipReader<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
            && self.entries.as_slice() == other.entries.as_slice()
            && self.limits == other.limits
    }
}

impl<A: Allocator> Eq for ZipReader<'_, A> {}

impl<'a, A: Allocator> ZipReader<'a, A> {
    /// Parse and validate the End of Central Directory and every Central Directory entry in
    /// `bytes`, storing the entry index in `alloc`.
    pub fn open_in(bytes: &'a [u8], limits: ZipLimits, alloc: A) -> Result<Self, ZipError> {
        let eocd_pos = find_eocd(bytes)?;

        // Reject Zip64 if a Zip64 EOCD Locator immediately precedes EOCD.
        if let Some(locator_pos) = eocd_pos.checked_sub(ZIP64_LOCATOR_LEN)
            && read_u32_le(bytes, locator_pos)? == ZIP64_EOCD_LOCATOR_SIG
        {
            return Err(ZipError::UnsupportedZip64);
        }

        let disk_num = read_u16_le(bytes, eocd_pos + 4)?;
        let cd_disk = read_u16_le(bytes, eocd_pos + 6)?;
        let disk_entries = read_u16_le(bytes, eocd_pos + 8)?;
        let total_entries = read_u16_le(bytes, eocd_pos + 10)?;
        let cd_size_u32 = read_u32_le(bytes, eocd_pos + 12)?;
        let cd_offset_u32 = read_u32_le(bytes, eocd_pos + 16)?;

        if disk_num == u16::MAX
            || cd_disk == u16::MAX
            || disk_entries == u16::MAX
            || total_entries == u16::MAX
            || cd_size_u32 == u32::MAX
            || cd_offset_u32 == u32::MAX
            || disk_num != 0
            || cd_disk != 0
            || disk_entries != total_entries
        {
            return Err(ZipError::UnsupportedZip64);
        }

        let entry_count = usize::from(total_entries);
        if entry_count > limits.most_entries {
            return Err(ZipError::TooManyEntries {
                count: entry_count,
                limit: limits.most_entries,
            });
        }

        let cd_offset = usize::try_from(cd_offset_u32).map_err(|_| ZipError::Truncated)?;
        let cd_size = usize::try_from(cd_size_u32).map_err(|_| ZipError::Truncated)?;
        let cd_end = cd_offset.checked_add(cd_size).ok_or(ZipError::Truncated)?;
        if cd_end > eocd_pos {
            return Err(ZipError::Truncated);
        }

        let mut entries = Vec::try_with_capacity_in(entry_count, alloc).map_err(ZipError::Alloc)?;
        let mut cursor = cd_offset;
        let mut total_uncompressed: usize = 0;

        for _ in 0..entry_count {
            let fixed_end = cursor.checked_add(CDH_FIXED_LEN).ok_or(ZipError::Truncated)?;
            if fixed_end > cd_end {
                return Err(ZipError::Truncated);
            }

            let sig = read_u32_le(bytes, cursor)?;
            if sig != CDH_SIG {
                return Err(ZipError::BadSignature { expected: CDH_SIG, found: sig });
            }

            let flags = read_u16_le(bytes, cursor + 8)?;
            if (flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTED)) != 0 {
                return Err(ZipError::UnsupportedEncrypted);
            }

            let method_raw = read_u16_le(bytes, cursor + 10)?;
            let compression = parse_method(method_raw)?;

            let crc32 = read_u32_le(bytes, cursor + 16)?;
            let compressed_size = read_u32_le(bytes, cursor + 20)?;
            let uncompressed_size = read_u32_le(bytes, cursor + 24)?;
            let name_len = usize::from(read_u16_le(bytes, cursor + 28)?);
            let extra_len = usize::from(read_u16_le(bytes, cursor + 30)?);
            let comment_len = usize::from(read_u16_le(bytes, cursor + 32)?);
            let disk_start = read_u16_le(bytes, cursor + 34)?;
            let local_offset_u32 = read_u32_le(bytes, cursor + 42)?;

            if compressed_size == u32::MAX
                || uncompressed_size == u32::MAX
                || local_offset_u32 == u32::MAX
                || disk_start == u16::MAX
                || disk_start != 0
            {
                return Err(ZipError::UnsupportedZip64);
            }

            if name_len > limits.most_name_bytes {
                return Err(ZipError::EntryTooLarge {
                    bytes: name_len,
                    limit: limits.most_name_bytes,
                });
            }

            let uncomp_usize = usize::try_from(uncompressed_size).map_err(|_| {
                ZipError::EntryTooLarge { bytes: usize::MAX, limit: limits.most_entry_bytes }
            })?;
            if uncomp_usize > limits.most_entry_bytes {
                return Err(ZipError::EntryTooLarge {
                    bytes: uncomp_usize,
                    limit: limits.most_entry_bytes,
                });
            }

            total_uncompressed = total_uncompressed.checked_add(uncomp_usize).ok_or(
                ZipError::TotalTooLarge { bytes: usize::MAX, limit: limits.most_total_bytes },
            )?;
            if total_uncompressed > limits.most_total_bytes {
                return Err(ZipError::TotalTooLarge {
                    bytes: total_uncompressed,
                    limit: limits.most_total_bytes,
                });
            }

            let name_end = fixed_end.checked_add(name_len).ok_or(ZipError::Truncated)?;
            let extra_end = name_end.checked_add(extra_len).ok_or(ZipError::Truncated)?;
            let next_cursor = extra_end.checked_add(comment_len).ok_or(ZipError::Truncated)?;
            if next_cursor > cd_end {
                return Err(ZipError::Truncated);
            }

            let name_bytes = bytes.get(fixed_end..name_end).ok_or(ZipError::Truncated)?;
            let name = core::str::from_utf8(name_bytes).map_err(|_| ZipError::InvalidUtf8)?;
            validate_entry_name(name)?;
            let is_dir = name.ends_with('/');

            let local_header_offset =
                usize::try_from(local_offset_u32).map_err(|_| ZipError::Truncated)?;
            if local_header_offset >= cd_offset {
                return Err(ZipError::Truncated);
            }

            entries
                .try_push(CdEntry {
                    meta: EntryMeta {
                        name,
                        compression,
                        crc32,
                        compressed_size,
                        uncompressed_size,
                        is_dir,
                    },
                    local_header_offset,
                })
                .map_err(|refused| ZipError::Alloc(refused.why))?;

            cursor = next_cursor;
        }

        if cursor != cd_end {
            return Err(ZipError::Truncated);
        }

        Ok(Self { bytes, entries, limits })
    }

    /// Number of entries in the archive.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return `true` if the archive contains no entries.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return metadata for entry `index`, or `None` if `index >= self.len()`.
    #[inline]
    #[must_use]
    pub fn meta(&self, index: usize) -> Option<EntryMeta<'a>> {
        self.entries.get(index).map(|entry| entry.meta)
    }

    /// Locate the first entry with exact relative path `name`.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.meta.name == name)
    }

    /// Extract, verify Local File Header, decompress (if Deflate), check exact length, and
    /// verify IEEE CRC-32 for entry `index` into `alloc`.
    pub fn read_entry_in<B: Allocator + Copy>(
        &self,
        index: usize,
        alloc: B,
    ) -> Result<Vec<u8, B>, ZipError> {
        self.read_entry_in_planted(index, alloc, false)
    }

    fn read_entry_in_planted<B: Allocator + Copy>(
        &self,
        index: usize,
        alloc: B,
        plant_skip_crc: bool,
    ) -> Result<Vec<u8, B>, ZipError> {
        let entry = self.entries.get(index).ok_or(ZipError::EntryOutOfRange {
            index,
            len: self.entries.len(),
        })?;

        let off = entry.local_header_offset;
        let lfh_fixed_end = off.checked_add(LFH_FIXED_LEN).ok_or(ZipError::Truncated)?;
        if lfh_fixed_end > self.bytes.len() {
            return Err(ZipError::Truncated);
        }

        let sig = read_u32_le(self.bytes, off)?;
        if sig != LFH_SIG {
            return Err(ZipError::BadSignature { expected: LFH_SIG, found: sig });
        }

        let lfh_flags = read_u16_le(self.bytes, off + 6)?;
        if (lfh_flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTED)) != 0 {
            return Err(ZipError::UnsupportedEncrypted);
        }

        let lfh_method_raw = read_u16_le(self.bytes, off + 8)?;
        let lfh_compression = parse_method(lfh_method_raw)?;
        if lfh_compression != entry.meta.compression {
            return Err(ZipError::UnsupportedMethod { method: lfh_method_raw });
        }

        if (lfh_flags & FLAG_DATA_DESCRIPTOR) == 0 {
            let lfh_crc = read_u32_le(self.bytes, off + 14)?;
            let lfh_comp = read_u32_le(self.bytes, off + 18)?;
            let lfh_uncomp = read_u32_le(self.bytes, off + 22)?;
            if lfh_comp != entry.meta.compressed_size {
                return Err(ZipError::SizeMismatch {
                    expected: usize::try_from(entry.meta.compressed_size).unwrap_or(usize::MAX),
                    found: usize::try_from(lfh_comp).unwrap_or(usize::MAX),
                });
            }
            if lfh_uncomp != entry.meta.uncompressed_size {
                return Err(ZipError::SizeMismatch {
                    expected: usize::try_from(entry.meta.uncompressed_size).unwrap_or(usize::MAX),
                    found: usize::try_from(lfh_uncomp).unwrap_or(usize::MAX),
                });
            }
            if !plant_skip_crc && lfh_crc != entry.meta.crc32 {
                return Err(ZipError::CrcMismatch { expected: entry.meta.crc32, found: lfh_crc });
            }
        }

        let lfh_name_len = usize::from(read_u16_le(self.bytes, off + 26)?);
        let lfh_extra_len = usize::from(read_u16_le(self.bytes, off + 28)?);
        let lfh_name_end = lfh_fixed_end.checked_add(lfh_name_len).ok_or(ZipError::Truncated)?;
        let lfh_name_bytes = self
            .bytes
            .get(lfh_fixed_end..lfh_name_end)
            .ok_or(ZipError::Truncated)?;
        if lfh_name_bytes != entry.meta.name.as_bytes() {
            return Err(ZipError::UnsafePath);
        }

        let data_start = lfh_name_end.checked_add(lfh_extra_len).ok_or(ZipError::Truncated)?;
        let comp_len =
            usize::try_from(entry.meta.compressed_size).map_err(|_| ZipError::Truncated)?;
        let data_end = data_start.checked_add(comp_len).ok_or(ZipError::Truncated)?;
        let payload = self.bytes.get(data_start..data_end).ok_or(ZipError::Truncated)?;

        let expected_uncomp = usize::try_from(entry.meta.uncompressed_size).map_err(|_| {
            ZipError::EntryTooLarge { bytes: usize::MAX, limit: self.limits.most_entry_bytes }
        })?;
        if expected_uncomp > self.limits.most_entry_bytes {
            return Err(ZipError::EntryTooLarge {
                bytes: expected_uncomp,
                limit: self.limits.most_entry_bytes,
            });
        }

        let out = match entry.meta.compression {
            Compression::Store => {
                if payload.len() != expected_uncomp {
                    return Err(ZipError::SizeMismatch {
                        expected: expected_uncomp,
                        found: payload.len(),
                    });
                }
                let mut out =
                    Vec::try_with_capacity_in(expected_uncomp, alloc).map_err(ZipError::Alloc)?;
                out.try_extend_from_slice(payload).map_err(ZipError::Alloc)?;
                out
            }
            Compression::Deflate => {
                let mut out =
                    Vec::try_with_capacity_in(expected_uncomp, alloc).map_err(ZipError::Alloc)?;
                let inflate_limits = InflateLimits {
                    most_out: u64::try_from(self.limits.most_entry_bytes).unwrap_or(u64::MAX),
                    most_ratio: MAX_RATIO,
                    most_header: 0,
                };
                blaze_deflate::inflate(payload, inflate_limits, alloc, &mut out)
                    .map_err(|err| map_inflate_err(err, self.limits.most_entry_bytes))?;
                if out.len() != expected_uncomp {
                    return Err(ZipError::SizeMismatch {
                        expected: expected_uncomp,
                        found: out.len(),
                    });
                }
                out
            }
        };

        if !plant_skip_crc {
            let mut hasher = Crc32::new();
            hasher.update(out.as_slice());
            let computed = hasher.finish();
            if computed != entry.meta.crc32 {
                return Err(ZipError::CrcMismatch {
                    expected: entry.meta.crc32,
                    found: computed,
                });
            }
        }

        Ok(out)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct WrittenEntry {
    name_offset: usize,
    name_len: usize,
    compression: Compression,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u32,
    is_dir: bool,
}

/// Deterministic PKZIP archive writer supporting Store (`0`) and raw Deflate (`8`).
pub struct ZipWriter<A: Allocator> {
    alloc: A,
    buf: Vec<u8, A>,
    names: Vec<u8, A>,
    entries: Vec<WrittenEntry, A>,
}

impl<A: Allocator> fmt::Debug for ZipWriter<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZipWriter")
            .field("entries_len", &self.entries.len())
            .field("bytes_len", &self.buf.len())
            .finish()
    }
}

impl<A: Allocator> PartialEq for ZipWriter<A> {
    fn eq(&self, other: &Self) -> bool {
        self.buf.as_slice() == other.buf.as_slice()
            && self.names.as_slice() == other.names.as_slice()
            && self.entries.as_slice() == other.entries.as_slice()
    }
}

impl<A: Allocator> Eq for ZipWriter<A> {}

impl<A: Allocator + Copy> ZipWriter<A> {
    /// Start an empty ZIP archive in `alloc`.
    #[must_use]
    pub fn new_in(alloc: A) -> Self {
        Self {
            alloc,
            buf: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            entries: Vec::new_in(alloc),
        }
    }

    /// Number of entries written so far.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return `true` if no entries have been added yet.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add a file (or directory entry if `name` ends with `/` and `data` is empty) to the archive.
    ///
    /// Refuses unsafe paths (zip-slip) with [`ZipError::UnsafePath`].
    pub fn add_file(
        &mut self,
        name: &str,
        data: &[u8],
        compression: Compression,
    ) -> Result<(), ZipError> {
        validate_entry_name(name)?;
        let is_dir = name.ends_with('/');
        if is_dir && !data.is_empty() {
            return Err(ZipError::SizeMismatch { expected: 0, found: data.len() });
        }
        self.add_entry_parts(name, false, data, compression, is_dir)
    }

    /// Add a directory entry (`name`, appending a trailing `/` if absent).
    ///
    /// Refuses unsafe paths (zip-slip) with [`ZipError::UnsafePath`].
    pub fn add_dir(&mut self, name: &str) -> Result<(), ZipError> {
        validate_entry_name(name)?;
        let append_slash = !name.ends_with('/');
        self.add_entry_parts(name, append_slash, &[], Compression::Store, true)
    }

    fn add_entry_parts(
        &mut self,
        name: &str,
        append_slash: bool,
        data: &[u8],
        compression: Compression,
        is_dir: bool,
    ) -> Result<(), ZipError> {
        // Non-Zip64 EOCD reserves 0xFFFF as the Zip64 sentinel, so at most 65,534 entries fit.
        if self.entries.len() + 1 >= usize::from(u16::MAX) {
            return Err(ZipError::UnsupportedZip64);
        }

        let extra_slash: usize = if append_slash { 1 } else { 0 };
        let full_name_len = name.len().checked_add(extra_slash).ok_or(ZipError::EntryTooLarge {
            bytes: usize::MAX,
            limit: usize::from(u16::MAX),
        })?;
        let name_len_u16 = u16::try_from(full_name_len).map_err(|_| ZipError::EntryTooLarge {
            bytes: full_name_len,
            limit: usize::from(u16::MAX),
        })?;

        let uncompressed_size = u32::try_from(data.len())
            .ok()
            .filter(|&n| n < u32::MAX)
            .ok_or(ZipError::UnsupportedZip64)?;

        let local_header_offset = u32::try_from(self.buf.len())
            .ok()
            .filter(|&n| n < u32::MAX)
            .ok_or(ZipError::UnsupportedZip64)?;

        let mut hasher = Crc32::new();
        hasher.update(data);
        let crc32 = hasher.finish();

        let mut deflated = Vec::new_in(self.alloc);
        let payload: &[u8] = match compression {
            Compression::Store => data,
            Compression::Deflate => {
                blaze_deflate::deflate(data, Level::Default, self.alloc, &mut deflated)
                    .map_err(map_deflate_err)?;
                deflated.as_slice()
            }
        };

        let compressed_size = u32::try_from(payload.len())
            .ok()
            .filter(|&n| n < u32::MAX)
            .ok_or(ZipError::UnsupportedZip64)?;

        let needed = LFH_FIXED_LEN
            .checked_add(full_name_len)
            .and_then(|v| v.checked_add(payload.len()))
            .ok_or(ZipError::UnsupportedZip64)?;

        // Reserve all three buffers up front so an allocation refusal leaves writer state clean.
        self.buf.try_reserve(needed).map_err(ZipError::Alloc)?;
        self.names.try_reserve(full_name_len).map_err(ZipError::Alloc)?;
        self.entries.try_reserve(1).map_err(ZipError::Alloc)?;

        let name_offset = self.names.len();

        write_u32_le(&mut self.buf, LFH_SIG)?;
        write_u16_le(&mut self.buf, VERSION_NEEDED_20)?;
        write_u16_le(&mut self.buf, FLAG_UTF8)?;
        write_u16_le(&mut self.buf, compression.method_code())?;
        write_u16_le(&mut self.buf, DOS_TIME_ZERO)?;
        write_u16_le(&mut self.buf, DOS_DATE_1980_01_01)?;
        write_u32_le(&mut self.buf, crc32)?;
        write_u32_le(&mut self.buf, compressed_size)?;
        write_u32_le(&mut self.buf, uncompressed_size)?;
        write_u16_le(&mut self.buf, name_len_u16)?;
        write_u16_le(&mut self.buf, 0u16)?; // extra field length

        self.buf.try_extend_from_slice(name.as_bytes()).map_err(ZipError::Alloc)?;
        self.names.try_extend_from_slice(name.as_bytes()).map_err(ZipError::Alloc)?;
        if append_slash {
            self.buf.try_extend_from_slice(b"/").map_err(ZipError::Alloc)?;
            self.names.try_extend_from_slice(b"/").map_err(ZipError::Alloc)?;
        }
        self.buf.try_extend_from_slice(payload).map_err(ZipError::Alloc)?;

        self.entries
            .try_push(WrittenEntry {
                name_offset,
                name_len: full_name_len,
                compression,
                crc32,
                compressed_size,
                uncompressed_size,
                local_header_offset,
                is_dir,
            })
            .map_err(|refused| ZipError::Alloc(refused.why))?;

        Ok(())
    }

    /// Write the Central Directory and End of Central Directory record and return the finished
    /// ZIP archive bytes.
    pub fn finish(mut self) -> Result<Vec<u8, A>, ZipError> {
        let cd_offset_usize = self.buf.len();
        let cd_offset = u32::try_from(cd_offset_usize)
            .ok()
            .filter(|&n| n < u32::MAX)
            .ok_or(ZipError::UnsupportedZip64)?;

        let entry_count_u16 = u16::try_from(self.entries.len())
            .ok()
            .filter(|&n| n < u16::MAX)
            .ok_or(ZipError::UnsupportedZip64)?;

        for entry in self.entries.as_slice() {
            let name_end = entry
                .name_offset
                .checked_add(entry.name_len)
                .ok_or(ZipError::Truncated)?;
            let name_bytes = self
                .names
                .get(entry.name_offset..name_end)
                .ok_or(ZipError::Truncated)?;
            let name_len_u16 =
                u16::try_from(entry.name_len).map_err(|_| ZipError::EntryTooLarge {
                    bytes: entry.name_len,
                    limit: usize::from(u16::MAX),
                })?;
            let ext_attrs = if entry.is_dir { EXT_ATTR_DIR } else { EXT_ATTR_FILE };

            write_u32_le(&mut self.buf, CDH_SIG)?;
            write_u16_le(&mut self.buf, VERSION_MADE_BY_UNIX_20)?;
            write_u16_le(&mut self.buf, VERSION_NEEDED_20)?;
            write_u16_le(&mut self.buf, FLAG_UTF8)?;
            write_u16_le(&mut self.buf, entry.compression.method_code())?;
            write_u16_le(&mut self.buf, DOS_TIME_ZERO)?;
            write_u16_le(&mut self.buf, DOS_DATE_1980_01_01)?;
            write_u32_le(&mut self.buf, entry.crc32)?;
            write_u32_le(&mut self.buf, entry.compressed_size)?;
            write_u32_le(&mut self.buf, entry.uncompressed_size)?;
            write_u16_le(&mut self.buf, name_len_u16)?;
            write_u16_le(&mut self.buf, 0u16)?; // extra length
            write_u16_le(&mut self.buf, 0u16)?; // comment length
            write_u16_le(&mut self.buf, 0u16)?; // disk number start
            write_u16_le(&mut self.buf, 0u16)?; // internal file attributes
            write_u32_le(&mut self.buf, ext_attrs)?;
            write_u32_le(&mut self.buf, entry.local_header_offset)?;
            self.buf.try_extend_from_slice(name_bytes).map_err(ZipError::Alloc)?;
        }

        let cd_size_usize = self.buf.len().saturating_sub(cd_offset_usize);
        let cd_size = u32::try_from(cd_size_usize)
            .ok()
            .filter(|&n| n < u32::MAX)
            .ok_or(ZipError::UnsupportedZip64)?;

        write_u32_le(&mut self.buf, EOCD_SIG)?;
        write_u16_le(&mut self.buf, 0u16)?; // number of this disk
        write_u16_le(&mut self.buf, 0u16)?; // disk where CD starts
        write_u16_le(&mut self.buf, entry_count_u16)?;
        write_u16_le(&mut self.buf, entry_count_u16)?;
        write_u32_le(&mut self.buf, cd_size)?;
        write_u32_le(&mut self.buf, cd_offset)?;
        write_u16_le(&mut self.buf, 0u16)?; // comment length

        Ok(self.buf)
    }
}

fn find_eocd(bytes: &[u8]) -> Result<usize, ZipError> {
    if bytes.len() < EOCD_FIXED_LEN {
        return Err(ZipError::MissingEocd);
    }
    let max_pos = bytes.len() - EOCD_FIXED_LEN;
    let min_pos = bytes.len().saturating_sub(EOCD_FIXED_LEN + MAX_EOCD_COMMENT);
    let mut pos = max_pos;
    loop {
        if read_u32_le(bytes, pos)? == EOCD_SIG {
            let comment_len = usize::from(read_u16_le(bytes, pos + 20)?);
            let expected_end = pos
                .checked_add(EOCD_FIXED_LEN)
                .and_then(|v| v.checked_add(comment_len));
            if expected_end == Some(bytes.len()) {
                return Ok(pos);
            }
        }
        if pos == min_pos {
            break;
        }
        pos -= 1;
    }
    Err(ZipError::MissingEocd)
}

fn parse_method(method: u16) -> Result<Compression, ZipError> {
    match method {
        METHOD_STORE => Ok(Compression::Store),
        METHOD_DEFLATE => Ok(Compression::Deflate),
        other => Err(ZipError::UnsupportedMethod { method: other }),
    }
}

fn validate_entry_name(name: &str) -> Result<(), ZipError> {
    if name.is_empty() || name.starts_with('/') {
        return Err(ZipError::UnsafePath);
    }
    let raw = name.as_bytes();
    if let (Some(&b0), Some(&b1)) = (raw.first(), raw.get(1))
        && b0.is_ascii_alphabetic()
        && b1 == b':'
    {
        return Err(ZipError::UnsafePath);
    }
    for &b in raw {
        if b == 0 || b == b'\\' {
            return Err(ZipError::UnsafePath);
        }
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty() {
        return Err(ZipError::UnsafePath);
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ZipError::UnsafePath);
        }
    }
    Ok(())
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, ZipError> {
    let end = offset.checked_add(2).ok_or(ZipError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(ZipError::Truncated)?;
    let arr: [u8; 2] = slice.try_into().map_err(|_| ZipError::Truncated)?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, ZipError> {
    let end = offset.checked_add(4).ok_or(ZipError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(ZipError::Truncated)?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| ZipError::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

fn write_u16_le<A: Allocator>(buf: &mut Vec<u8, A>, val: u16) -> Result<(), ZipError> {
    buf.try_extend_from_slice(&val.to_le_bytes()).map_err(ZipError::Alloc)
}

fn write_u32_le<A: Allocator>(buf: &mut Vec<u8, A>, val: u32) -> Result<(), ZipError> {
    buf.try_extend_from_slice(&val.to_le_bytes()).map_err(ZipError::Alloc)
}

fn map_deflate_err(err: DeflateError) -> ZipError {
    match err {
        DeflateError::Alloc(failure) => ZipError::Alloc(failure),
        DeflateError::Finished => ZipError::Compress(err),
    }
}

fn map_inflate_err(err: blaze_deflate::Error, max_entry_bytes: usize) -> ZipError {
    match err.why {
        InflateWhy::OutputOver { .. } | InflateWhy::RatioOver { .. } => ZipError::EntryTooLarge {
            bytes: max_entry_bytes.saturating_add(1),
            limit: max_entry_bytes,
        },
        InflateWhy::Alloc(failure) => ZipError::Alloc(failure),
        InflateWhy::BlockType
        | InflateWhy::StoredLength { .. }
        | InflateWhy::TooManyCodes { .. }
        | InflateWhy::Oversubscribed(_)
        | InflateWhy::Incomplete(_)
        | InflateWhy::RepeatFirst
        | InflateWhy::RepeatPastEnd
        | InflateWhy::NoEndOfBlock
        | InflateWhy::BadCode(_)
        | InflateWhy::BadSymbol { .. }
        | InflateWhy::TooFar { .. }
        | InflateWhy::Truncated
        | InflateWhy::AfterEnd { .. }
        | InflateWhy::ZlibHeader { .. }
        | InflateWhy::ZlibDictionary
        | InflateWhy::Adler32 { .. }
        | InflateWhy::GzipMagic { .. }
        | InflateWhy::GzipMethod(_)
        | InflateWhy::GzipFlags(_)
        | InflateWhy::GzipHeaderOver { .. }
        | InflateWhy::GzipHeaderCrc { .. }
        | InflateWhy::Crc32 { .. }
        | InflateWhy::Size { .. } => ZipError::Decompress(err),
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;
    use core::fmt::Write;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape};

    use super::{
        Compression, LFH_FIXED_LEN, ZipError, ZipLimits, ZipReader, ZipWriter, validate_entry_name,
    };

    #[test]
    fn zip_store_deflate_empty_and_dir_round_trip() {
        let h = heap();
        let alloc = h.alloc();

        let mut writer = ZipWriter::new_in(alloc);
        writer.add_dir("docs/").expect("dir");
        writer.add_file("empty.txt", b"", Compression::Store).expect("empty store");
        writer
            .add_file("empty_deflate.txt", b"", Compression::Deflate)
            .expect("empty deflate");
        writer
            .add_file("docs/hello.txt", b"hello from bstd::zip!", Compression::Store)
            .expect("store file");

        let mut repeated = OwnVec::new_in(alloc);
        for _ in 0..256 {
            repeated.extend_from_slice(b"the quick brown fox jumps over the lazy dog\n");
        }
        writer
            .add_file("docs/fox.txt", repeated.as_slice(), Compression::Deflate)
            .expect("deflate file");

        let archive = writer.finish().expect("finish");
        let reader = ZipReader::open_in(archive.as_slice(), ZipLimits::SHIPPED, alloc).expect("open");

        assert_eq!(reader.len(), 5);
        assert!(!reader.is_empty());

        let m0 = reader.meta(0).expect("m0");
        assert_eq!(m0.name, "docs/");
        assert!(m0.is_dir);
        assert_eq!(reader.read_entry_in(0, alloc).expect("read dir").as_slice(), b"");

        let idx_empty = reader.find("empty.txt").expect("find empty");
        assert_eq!(reader.read_entry_in(idx_empty, alloc).expect("read empty").as_slice(), b"");

        let idx_ed = reader.find("empty_deflate.txt").expect("find empty_deflate");
        assert_eq!(reader.read_entry_in(idx_ed, alloc).expect("read ed").as_slice(), b"");

        let idx_hello = reader.find("docs/hello.txt").expect("find hello");
        assert_eq!(
            reader.read_entry_in(idx_hello, alloc).expect("read hello").as_slice(),
            b"hello from bstd::zip!"
        );

        let idx_fox = reader.find("docs/fox.txt").expect("find fox");
        let meta_fox = reader.meta(idx_fox).expect("meta fox");
        assert_eq!(meta_fox.compression, Compression::Deflate);
        assert!(meta_fox.compressed_size < meta_fox.uncompressed_size);
        assert_eq!(
            reader.read_entry_in(idx_fox, alloc).expect("read fox").as_slice(),
            repeated.as_slice()
        );
    }

    #[test]
    fn zip_slip_refusal_in_writer_and_reader() {
        let h = heap();
        let alloc = h.alloc();

        for bad in [
            "",
            "/",
            "/abs/path",
            "../etc/passwd",
            "a/../b",
            "a/..",
            "./rel",
            "a\\b",
            "a//b",
            "a\0b",
            "C:evil.txt",
        ] {
            assert_eq!(validate_entry_name(bad), Err(ZipError::UnsafePath));
            let mut w = ZipWriter::new_in(alloc);
            assert_eq!(
                w.add_file(bad, b"x", Compression::Store),
                Err(ZipError::UnsafePath)
            );
        }

        // Craft a raw archive whose Central Directory contains "../evil.txt" (same length as
        // "good_ok.txt" = 11 bytes) and verify `ZipReader::open_in` rejects it.
        let mut w = ZipWriter::new_in(alloc);
        w.add_file("good_ok.txt", b"payload", Compression::Store).expect("add");
        let mut raw = w.finish().expect("finish");
        let needle = b"good_ok.txt";
        let replacement = b"../evil.txt";
        let mut i = 0usize;
        while i + needle.len() <= raw.len() {
            if raw.get(i..i + needle.len()) == Some(needle.as_slice()) {
                if let Some(dst) = raw.get_mut(i..i + needle.len()) {
                    dst.copy_from_slice(replacement);
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
        assert_eq!(
            ZipReader::open_in(raw.as_slice(), ZipLimits::SHIPPED, alloc),
            Err(ZipError::UnsafePath)
        );
    }

    #[test]
    fn zip_crc_corruption_and_bomb_limits_refused() {
        let h = heap();
        let alloc = h.alloc();

        // 1. CRC corruption refusal.
        let mut w = ZipWriter::new_in(alloc);
        w.add_file("data.bin", b"integrity-check-payload", Compression::Store).expect("add");
        let mut raw = w.finish().expect("finish");
        let payload_offset = LFH_FIXED_LEN + "data.bin".len();
        if let Some(byte) = raw.get_mut(payload_offset) {
            *byte ^= 0xFF;
        }
        let reader = ZipReader::open_in(raw.as_slice(), ZipLimits::SHIPPED, alloc).expect("open");
        assert!(matches!(
            reader.read_entry_in(0, alloc),
            Err(ZipError::CrcMismatch { .. })
        ));

        // 2. Zip-bomb refusal via `most_entry_bytes` (both declared AND spoofed header).
        let mut bomb_plain = OwnVec::new_in(alloc);
        bomb_plain.resize(32_768, 0u8);
        let mut wb = ZipWriter::new_in(alloc);
        wb.add_file("bomb.bin", bomb_plain.as_slice(), Compression::Deflate).expect("add bomb");
        let bomb_raw = wb.finish().expect("finish bomb");

        let strict = ZipLimits {
            most_entries: 16,
            most_entry_bytes: 1_024,
            most_total_bytes: 4_096,
            most_name_bytes: 256,
        };
        assert!(matches!(
            ZipReader::open_in(bomb_raw.as_slice(), strict, alloc),
            Err(ZipError::EntryTooLarge { bytes: 32_768, limit: 1_024 })
        ));

        // Now spoof both LFH and CD uncompressed_size down to 16 bytes so `open_in` accepts,
        // and verify `read_entry_in` halts decompression at `most_entry_bytes = 1024`.
        let mut spoofed = OwnVec::new_in(alloc);
        spoofed.extend_from_slice(bomb_raw.as_slice());
        let orig_bytes = 32_768u32.to_le_bytes();
        let fake_bytes = 16u32.to_le_bytes();
        // LFH uncompressed size is at offset 22..26.
        if let Some(dst) = spoofed.get_mut(22..26) {
            assert_eq!(dst, orig_bytes.as_slice());
            dst.copy_from_slice(&fake_bytes);
        }
        // CDH uncompressed size is at cd_offset + 24..28.
        let eocd_pos = spoofed.len() - 22;
        let cd_off = u32::from_le_bytes(
            spoofed
                .get(eocd_pos + 16..eocd_pos + 20)
                .expect("cd_off")
                .try_into()
                .expect("arr"),
        );
        let cd_uncomp_pos = usize::try_from(cd_off).expect("usize") + 24;
        if let Some(dst) = spoofed.get_mut(cd_uncomp_pos..cd_uncomp_pos + 4) {
            assert_eq!(dst, orig_bytes.as_slice());
            dst.copy_from_slice(&fake_bytes);
        }

        let reader_spoofed =
            ZipReader::open_in(spoofed.as_slice(), strict, alloc).expect("open spoofed");
        assert!(matches!(
            reader_spoofed.read_entry_in(0, alloc),
            Err(ZipError::EntryTooLarge { limit: 1_024, .. })
        ));
    }

    struct DrawnFile<A: Allocator> {
        name: OwnString<A>,
        data: OwnVec<u8, A>,
        compression: Compression,
    }

    fn draw_files<A: Allocator + Copy>(
        tape: &Tape,
        force_nonempty_store: bool,
        alloc: A,
    ) -> OwnVec<DrawnFile<A>, A> {
        let min_count = if force_nonempty_store { 1usize } else { 0usize };
        let count = tape.int_in(min_count..=6usize, "file_count");
        let mut out = OwnVec::new_in(alloc);
        for idx in 0..count {
            let mut name = OwnString::new_in(alloc);
            if tape.coin("nested") {
                write!(&mut name, "dir_{idx}/file_{idx}.bin").expect("fmt");
            } else {
                write!(&mut name, "entry_{idx}.dat").expect("fmt");
            }
            let min_len = if force_nonempty_store && idx == 0 { 1usize } else { 0usize };
            let len = tape.int_in(min_len..=128usize, "data_len");
            let mut data = OwnVec::with_capacity_in(len, alloc);
            let pattern = tape.int_in(0u8..=3, "pattern");
            for pos in 0..len {
                let b = match pattern {
                    0 => tape.int_in(0u8..=255, "byte"),
                    1 => b'A',
                    2 => u8::try_from(pos % 16).unwrap_or(0),
                    _ => u8::try_from((pos * 31 + idx) % 251).unwrap_or(0),
                };
                data.push(b);
            }
            let compression = if force_nonempty_store && idx == 0 {
                Compression::Store
            } else if tape.coin("deflate") {
                Compression::Deflate
            } else {
                Compression::Store
            };
            out.push(DrawnFile { name, data, compression });
        }
        out
    }

    /// The invariant: Store and Deflate ZIP archives round-trip with verified CRC-32 and hostile archive bytes never panic.
    #[test]
    fn property_zip_round_trip_and_hostile_bytes_never_panic() {
        let h = heap();
        blaze_sim::check(Config::cases(160), |tape| {
            let alloc = h.alloc();
            let files = draw_files(tape, false, alloc);

            let mut writer = ZipWriter::new_in(alloc);
            for file in files.as_slice() {
                writer
                    .add_file(file.name.as_str(), file.data.as_slice(), file.compression)
                    .map_err(|e| Broke::Oracle(format_in!(alloc, "add_file failed: {e}").as_str().into()))?;
            }
            let archive = writer
                .finish()
                .map_err(|e| Broke::Oracle(format_in!(alloc, "finish failed: {e}").as_str().into()))?;

            let reader = ZipReader::open_in(archive.as_slice(), ZipLimits::SHIPPED, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "open failed: {e}").as_str().into()))?;
            if reader.len() != files.len() {
                return Err(Broke::Oracle("entry count mismatch".into()));
            }
            for (idx, file) in files.as_slice().iter().enumerate() {
                let meta = reader
                    .meta(idx)
                    .ok_or_else(|| Broke::Oracle("missing meta".into()))?;
                if meta.name != file.name.as_str() || meta.compression != file.compression {
                    return Err(Broke::Oracle("meta mismatch".into()));
                }
                let extracted = reader.read_entry_in(idx, alloc).map_err(|e| {
                    Broke::Oracle(format_in!(alloc, "read_entry failed: {e}").as_str().into())
                })?;
                if extracted.as_slice() != file.data.as_slice() {
                    return Err(Broke::Oracle("extracted payload mismatch".into()));
                }
            }

            // Hostile mutated bytes must never panic (only return Ok or typed Err).
            let mut hostile = OwnVec::new_in(alloc);
            hostile.extend_from_slice(archive.as_slice());
            let muts = tape.int_in(1usize..=8usize, "mutations");
            for _ in 0..muts {
                if !hostile.is_empty() {
                    let pos = tape.index(hostile.len(), "mut_pos");
                    let xor = tape.int_in(1u8..=255u8, "mut_xor");
                    if let Some(slot) = hostile.get_mut(pos) {
                        *slot ^= xor;
                    }
                }
            }
            let tiny_limits = ZipLimits {
                most_entries: 16,
                most_entry_bytes: 4_096,
                most_total_bytes: 16_384,
                most_name_bytes: 256,
            };
            if let Ok(fuzz_reader) = ZipReader::open_in(hostile.as_slice(), tiny_limits, alloc) {
                for idx in 0..fuzz_reader.len() {
                    drop(fuzz_reader.read_entry_in(idx, alloc));
                }
            }

            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_crc_verification() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let alloc = h.alloc();
            let files = draw_files(tape, true, alloc);

            let mut writer = ZipWriter::new_in(alloc);
            for file in files.as_slice() {
                writer
                    .add_file(file.name.as_str(), file.data.as_slice(), file.compression)
                    .map_err(|e| Broke::Oracle(format_in!(alloc, "add_file failed: {e}").as_str().into()))?;
            }
            let mut corrupted = writer
                .finish()
                .map_err(|e| Broke::Oracle(format_in!(alloc, "finish failed: {e}").as_str().into()))?;

            // Corrupt 1 byte inside entry 0's stored payload (header CRC remains untouched).
            let first = files
                .first()
                .ok_or_else(|| Broke::Oracle("missing first file".into()))?;
            let byte_idx = tape.index(first.data.len(), "corrupt_offset");
            let xor = tape.int_in(1u8..=255u8, "corrupt_xor");
            let abs_off = LFH_FIXED_LEN + first.name.len() + byte_idx;
            if let Some(slot) = corrupted.get_mut(abs_off) {
                *slot ^= xor;
            }

            let reader = ZipReader::open_in(corrupted.as_slice(), ZipLimits::SHIPPED, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "open failed: {e}").as_str().into()))?;
            match reader.read_entry_in_planted(0, alloc, plant) {
                Err(ZipError::CrcMismatch { .. }) => Ok(()),
                Ok(_) => Err(Broke::Oracle("corrupted payload was accepted without CrcMismatch".into())),
                Err(other) => Err(Broke::Oracle(
                    format_in!(alloc, "expected CrcMismatch, got {other}").as_str().into(),
                )),
            }
        });
    }
}
