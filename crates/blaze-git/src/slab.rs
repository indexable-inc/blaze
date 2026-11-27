//! Native `.bgit` on-disk binary formats — pure sans-IO encoders and zero-copy mmap readers.
//!
//! Designed for the macOS / APFS hardware floor:
//! - [`SlabWriter`] / [`SlabReader`] (`BGOB` v1): 8-byte-aligned append-only object slab storing
//!   objects either `Raw` (zero-copy `&[u8]` slice directly from mmap!) or `Zstd` (`blaze-zstd`,
//!   ~10x faster than zlib on Apple Silicon), with per-record CRC-32 integrity checks.
//! - [`SlabIndexWriter`] / [`SlabIndex`] (`BGIX` v1): 256-entry fanout + sorted 20-byte OIDs +
//!   packed `(slab_id: u16, offset: u48)` + `raw_len: u32` + CRC-32 trailer. O(log N) binary
//!   search directly over mmap with zero heap allocations.
//! - [`RefTableWriter`] / [`RefTable`] (`BGRF` v1): single atomic binary file holding `HEAD` +
//!   sorted refs + symrefs + pre-peeled tag OIDs + CRC-32 trailer.
//! - [`CommitGraphWriter`] (`CGPH` v1): standard Git binary commit-graph writer with exact
//!   topological generation numbers, directly readable by [`crate::commit_graph::CommitGraphLayer`]
//!   and standard `git`.

#![blaze::sans_io]

use blaze_deflate::crc32;
use blaze_git_fetch::{Kind as ObjectKind, Oid, sha1::sha1};
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_own::{
    AVec, Alloc, String, Vec,
    planted::{NoPlant, Planted},
};
use blaze_zstd::{CompressOpts, ZstdLimits, compress_in, decompress_in};

use crate::{
    ErrorKind, GitError, GitFile,
    plant::GitEnginePlant,
    storage::{ObjectStore, RefEntry, RefTarget},
};

pub const BGOB_MAGIC: &[u8; 4] = b"BGOB";
pub const BGIX_MAGIC: &[u8; 4] = b"BGIX";
pub const BGRF_MAGIC: &[u8; 4] = b"BGRF";
pub const SLAB_HEADER_LEN: usize = 16;
pub const RECORD_HEADER_LEN: usize = 36;

/// Storage compression codec for one record in a `BGOB` slab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SlabCodec {
    Raw = 0,
    Zstd = 1,
}

impl SlabCodec {
    fn from_u8(v: u8) -> Option<SlabCodec> {
        match v {
            0 => Some(SlabCodec::Raw),
            1 => Some(SlabCodec::Zstd),
            _ => None,
        }
    }
}

fn kind_to_u8(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

fn kind_from_u8(v: u8) -> Option<ObjectKind> {
    match v {
        1 => Some(ObjectKind::Commit),
        2 => Some(ObjectKind::Tree),
        3 => Some(ObjectKind::Blob),
        4 => Some(ObjectKind::Tag),
        _ => None,
    }
}

#[inline]
fn align8(n: usize) -> usize {
    (n + 7) & !7
}

#[inline]
fn u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[inline]
fn u32_be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[inline]
fn u64_le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

/// Compression policy for [`SlabWriter`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlabCompression {
    /// Store every object uncompressed (fastest zero-copy mmap reads).
    RawOnly,
    /// Store small/incompressible objects `Raw` and compressible objects >= 128B with `blaze-zstd`.
    AutoZstd,
}

/// One index entry emitted by [`SlabWriter::append`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlabIndexEntry {
    pub oid: Oid,
    pub slab_id: u16,
    pub offset: u64,
    pub raw_len: u32,
}

/// Metrics returned by [`encode_slab_record_with_oid`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EncodedSlabRecord {
    pub oid: Oid,
    pub raw_len: u32,
    pub stored_len: u32,
}

/// Writes the 16-byte `BGOB` v1 file header into `out`.
pub fn write_slab_header<'a>(out: &mut AVec<'a, u8>) {
    out.extend_from_slice(BGOB_MAGIC);
    out.extend_from_slice(&1u32.to_le_bytes()); // version 1
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
}

/// Encodes one standalone 8-byte-aligned `BGOB` v1 object record into `out` (appending).
///
/// Pure and self-contained so a multi-core driver can compress many objects in parallel and
/// concatenate the resulting slices in deterministic OID order after [`write_slab_header`],
/// producing byte-for-byte identical slabs to [`SlabWriter`].
pub fn encode_slab_record_with_oid<'a>(
    oid: Oid,
    kind: ObjectKind,
    body: &[u8],
    compression: SlabCompression,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<EncodedSlabRecord, GitError> {
    let raw_len = u32::try_from(body.len()).map_err(|_| GitError {
        file: GitFile::Slab,
        at: out.len() as u64,
        kind: ErrorKind::BudgetExceeded,
    })?;

    let mut zstd_buf: Option<AVec<'a, u8>> = None;
    let codec = match compression {
        SlabCompression::RawOnly => SlabCodec::Raw,
        SlabCompression::AutoZstd if body.len() >= 128 => {
            match compress_in(body, CompressOpts::FAST, alloc) {
                Ok(c) if c.len() + 16 < body.len() => {
                    zstd_buf = Some(c);
                    SlabCodec::Zstd
                }
                _ => SlabCodec::Raw,
            }
        }
        SlabCompression::AutoZstd => SlabCodec::Raw,
    };
    let payload: &[u8] = zstd_buf.as_deref().unwrap_or(body);
    let stored_len = u32::try_from(payload.len()).map_err(|_| GitError {
        file: GitFile::Slab,
        at: out.len() as u64,
        kind: ErrorKind::BudgetExceeded,
    })?;

    let mut hdr = [0u8; RECORD_HEADER_LEN];
    hdr[0..20].copy_from_slice(&oid.0);
    hdr[20] = kind_to_u8(kind);
    hdr[21] = codec as u8;
    hdr[22..24].copy_from_slice(&0u16.to_le_bytes());
    hdr[24..28].copy_from_slice(&raw_len.to_le_bytes());
    hdr[28..32].copy_from_slice(&stored_len.to_le_bytes());
    let mut crc = blaze_deflate::Crc32::new();
    crc.update(&hdr[0..32]);
    crc.update(payload);
    hdr[32..36].copy_from_slice(&crc.finish().to_le_bytes());

    let start = out.len();
    out.extend_from_slice(&hdr);
    out.extend_from_slice(payload);
    let rec_len = out.len() - start;
    let padded_rec_len = align8(rec_len);
    for _ in rec_len..padded_rec_len {
        out.push(0);
    }

    Ok(EncodedSlabRecord { oid, raw_len, stored_len })
}

/// Append-only encoder for a `BGOB` v1 object slab.
pub struct SlabWriter<'a> {
    slab_id: u16,
    compression: SlabCompression,
    buf: AVec<'a, u8>,
    entries: AVec<'a, SlabIndexEntry>,
    raw_bytes_total: u64,
    stored_bytes_total: u64,
    alloc: Alloc<'a>,
}

impl<'a> SlabWriter<'a> {
    pub fn new(slab_id: u16, compression: SlabCompression, alloc: Alloc<'a>) -> SlabWriter<'a> {
        let mut buf = Vec::new_in(alloc);
        write_slab_header(&mut buf);
        SlabWriter {
            slab_id,
            compression,
            buf,
            entries: Vec::new_in(alloc),
            raw_bytes_total: 0,
            stored_bytes_total: 0,
            alloc,
        }
    }

    /// Appends `(kind, body)` to the slab and returns its [`SlabIndexEntry`].
    pub fn append(&mut self, kind: ObjectKind, body: &[u8]) -> Result<SlabIndexEntry, GitError> {
        let oid = Oid::of(kind, body);
        self.append_with_oid(oid, kind, body)
    }

    /// Appends an object whose SHA-1 `oid` is already known.
    pub fn append_with_oid(
        &mut self,
        oid: Oid,
        kind: ObjectKind,
        body: &[u8],
    ) -> Result<SlabIndexEntry, GitError> {
        let offset = self.buf.len() as u64;
        if offset >= (1u64 << 48) {
            return Err(GitError {
                file: GitFile::Slab,
                at: offset,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        let rec = encode_slab_record_with_oid(
            oid,
            kind,
            body,
            self.compression,
            self.alloc,
            &mut self.buf,
        )?;
        self.raw_bytes_total = self.raw_bytes_total.saturating_add(u64::from(rec.raw_len));
        self.stored_bytes_total = self.stored_bytes_total.saturating_add(u64::from(rec.stored_len));

        let entry = SlabIndexEntry { oid, slab_id: self.slab_id, offset, raw_len: rec.raw_len };
        self.entries.push(entry);
        Ok(entry)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn entries(&self) -> &[SlabIndexEntry] {
        &self.entries
    }

    pub fn raw_bytes_total(&self) -> u64 {
        self.raw_bytes_total
    }

    pub fn stored_bytes_total(&self) -> u64 {
        self.stored_bytes_total
    }

    pub fn into_parts(self) -> (AVec<'a, u8>, AVec<'a, SlabIndexEntry>) {
        (self.buf, self.entries)
    }
}

/// Decoded payload from [`SlabReader::read_at`]: either a zero-copy borrow into the mmap (`Raw`)
/// or decompressed into the caller's `out` buffer (`Zstd`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlabPayload<'b> {
    /// Zero-copy slice directly inside the slab bytes!
    Borrowed(&'b [u8]),
    /// Decompressed bytes were written into the caller's `out` buffer.
    InOut,
}

/// Record returned by [`SlabReader::read_at`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlabRecord<'b> {
    pub oid: Oid,
    pub kind: ObjectKind,
    pub codec: SlabCodec,
    pub payload: SlabPayload<'b>,
}

/// Zero-copy reader over a mapped `BGOB` v1 slab.
#[derive(Clone, Copy)]
pub struct SlabReader<'b> {
    bytes: &'b [u8],
}

impl<'b> SlabReader<'b> {
    pub fn parse(bytes: &'b [u8]) -> Result<SlabReader<'b>, GitError> {
        if bytes.len() < SLAB_HEADER_LEN || &bytes[0..4] != BGOB_MAGIC {
            return Err(GitError { file: GitFile::Slab, at: 0, kind: ErrorKind::Malformed });
        }
        let version = u32_le(bytes, 4);
        if version != 1 {
            return Err(GitError {
                file: GitFile::Slab,
                at: 4,
                kind: ErrorKind::UnsupportedVersion(version),
            });
        }
        Ok(SlabReader { bytes })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Reads the record at `offset` using production checks (`NoPlant`).
    pub fn read_at<'a>(
        &self,
        offset: u64,
        max_bytes: usize,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<SlabRecord<'b>, GitError> {
        self.read_at_planted(offset, max_bytes, alloc, out, NoPlant)
    }

    /// Reads the record at `offset`, allowing a planted bug in DST oracle controls.
    pub fn read_at_planted<'a, P: Planted<GitEnginePlant>>(
        &self,
        offset: u64,
        max_bytes: usize,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
        plant: P,
    ) -> Result<SlabRecord<'b>, GitError> {
        let off = usize::try_from(offset).map_err(|_| GitError {
            file: GitFile::Slab,
            at: offset,
            kind: ErrorKind::Malformed,
        })?;
        if off < SLAB_HEADER_LEN || off + RECORD_HEADER_LEN > self.bytes.len() {
            return Err(GitError { file: GitFile::Slab, at: offset, kind: ErrorKind::Malformed });
        }
        let hdr = &self.bytes[off..off + RECORD_HEADER_LEN];
        let oid = Oid(hdr[0..20].try_into().expect("20 bytes"));
        let kind = kind_from_u8(hdr[20]).ok_or(GitError {
            file: GitFile::Slab,
            at: offset + 20,
            kind: ErrorKind::Malformed,
        })?;
        let codec = SlabCodec::from_u8(hdr[21]).ok_or(GitError {
            file: GitFile::Slab,
            at: offset + 21,
            kind: ErrorKind::Malformed,
        })?;
        let raw_len = u32_le(hdr, 24) as usize;
        let stored_len = u32_le(hdr, 28) as usize;
        let want_crc = u32_le(hdr, 32);

        if raw_len > max_bytes {
            return Err(GitError {
                file: GitFile::Slab,
                at: offset + 24,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        let payload_start = off + RECORD_HEADER_LEN;
        let payload_end = payload_start.checked_add(stored_len).ok_or(GitError {
            file: GitFile::Slab,
            at: offset + 28,
            kind: ErrorKind::Malformed,
        })?;
        if payload_end > self.bytes.len() {
            return Err(GitError { file: GitFile::Slab, at: offset, kind: ErrorKind::Malformed });
        }
        let payload = &self.bytes[payload_start..payload_end];

        if !plant.has(GitEnginePlant::SkipSlabCrcCheck) {
            let mut crc = blaze_deflate::Crc32::new();
            crc.update(&hdr[0..32]);
            crc.update(payload);
            if crc.finish() != want_crc {
                return Err(GitError {
                    file: GitFile::Slab,
                    at: offset + 32,
                    kind: ErrorKind::ChecksumMismatch,
                });
            }
        }

        match codec {
            SlabCodec::Raw => {
                if payload.len() != raw_len {
                    return Err(GitError {
                        file: GitFile::Slab,
                        at: offset,
                        kind: ErrorKind::Malformed,
                    });
                }
                Ok(SlabRecord { oid, kind, codec, payload: SlabPayload::Borrowed(payload) })
            }
            SlabCodec::Zstd => {
                let limits = ZstdLimits {
                    most_output_bytes: max_bytes,
                    most_window_bytes: 8 << 20,
                    most_frames: 4,
                };
                let dec = decompress_in(payload, limits, alloc).map_err(|_| GitError {
                    file: GitFile::Slab,
                    at: offset,
                    kind: ErrorKind::Compression,
                })?;
                if dec.len() != raw_len {
                    return Err(GitError {
                        file: GitFile::Slab,
                        at: offset,
                        kind: ErrorKind::Malformed,
                    });
                }
                out.clear();
                out.extend_from_slice(&dec);
                Ok(SlabRecord { oid, kind, codec, payload: SlabPayload::InOut })
            }
        }
    }

    /// Reads the record at `offset` directly into `out` (always populated regardless of codec).
    pub fn read_into<'a>(
        &self,
        offset: u64,
        max_bytes: usize,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(Oid, ObjectKind), GitError> {
        let rec = self.read_at(offset, max_bytes, alloc, out)?;
        if let SlabPayload::Borrowed(slice) = rec.payload {
            out.clear();
            out.extend_from_slice(slice);
        }
        Ok((rec.oid, rec.kind))
    }

    /// Advances an iterator cursor through all `(offset, oid, kind, raw_len)` records in the slab.
    pub fn scan_headers<'a>(
        &self,
        slab_id: u16,
        out: &mut AVec<'a, SlabIndexEntry>,
    ) -> Result<(), GitError> {
        let mut pos = SLAB_HEADER_LEN;
        while pos < self.bytes.len() {
            if pos + RECORD_HEADER_LEN > self.bytes.len() {
                return Err(GitError {
                    file: GitFile::Slab,
                    at: pos as u64,
                    kind: ErrorKind::Malformed,
                });
            }
            let hdr = &self.bytes[pos..pos + RECORD_HEADER_LEN];
            let oid = Oid(hdr[0..20].try_into().expect("20 bytes"));
            let _kind = kind_from_u8(hdr[20]).ok_or(GitError {
                file: GitFile::Slab,
                at: (pos + 20) as u64,
                kind: ErrorKind::Malformed,
            })?;
            let raw_len = u32_le(hdr, 24);
            let stored_len = u32_le(hdr, 28) as usize;
            let end = pos
                .checked_add(RECORD_HEADER_LEN)
                .and_then(|v| v.checked_add(stored_len))
                .ok_or(GitError {
                    file: GitFile::Slab,
                    at: pos as u64,
                    kind: ErrorKind::Malformed,
                })?;
            if end > self.bytes.len() {
                return Err(GitError {
                    file: GitFile::Slab,
                    at: pos as u64,
                    kind: ErrorKind::Malformed,
                });
            }
            out.push(SlabIndexEntry { oid, slab_id, offset: pos as u64, raw_len });
            pos = align8(end);
        }
        Ok(())
    }
}

/// Location of an object returned by [`SlabIndex::lookup`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlabLoc {
    pub slab_id: u16,
    pub offset: u64,
    pub raw_len: u32,
}

const BGIX_HEADER_LEN: usize = 16;
const BGIX_FANOUT_LEN: usize = 256 * 4;
const BGIX_OIDS_OFF: usize = BGIX_HEADER_LEN + BGIX_FANOUT_LEN;

/// Encodes a sorted `BGIX` v1 binary fanout index from `entries` (deduplicating identical OIDs).
pub fn encode_slab_index<'a>(
    entries: &[SlabIndexEntry],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<u32, GitError> {
    out.clear();
    let mut sorted: AVec<'a, SlabIndexEntry> = Vec::with_capacity_in(entries.len(), alloc);
    sorted.extend_from_slice(entries);
    sorted.sort_by(|a, b| a.oid.cmp(&b.oid));
    // Deduplicate keeping the first occurrence of each OID.
    let mut unique: AVec<'a, SlabIndexEntry> = Vec::with_capacity_in(sorted.len(), alloc);
    for e in sorted.iter().copied() {
        if unique.last().is_some_and(|prev| prev.oid == e.oid) {
            continue;
        }
        unique.push(e);
    }
    let count = u32::try_from(unique.len()).map_err(|_| GitError {
        file: GitFile::SlabIndex,
        at: 0,
        kind: ErrorKind::BudgetExceeded,
    })?;

    out.extend_from_slice(BGIX_MAGIC);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    // 256-entry big-endian fanout table.
    let mut fanout = [0u32; 256];
    let mut running = 0u32;
    let mut idx = 0usize;
    for byte in 0..256usize {
        while idx < unique.len() && usize::from(unique[idx].oid.0[0]) <= byte {
            running += 1;
            idx += 1;
        }
        fanout[byte] = running;
        out.extend_from_slice(&running.to_be_bytes());
    }

    for e in &unique {
        out.extend_from_slice(&e.oid.0);
    }
    for e in &unique {
        let packed = (u64::from(e.slab_id) << 48) | (e.offset & 0x0000_FFFF_FFFF_FFFF);
        out.extend_from_slice(&packed.to_le_bytes());
    }
    for e in &unique {
        out.extend_from_slice(&e.raw_len.to_le_bytes());
    }

    let trailer = crc32(out);
    out.extend_from_slice(&trailer.to_le_bytes());
    Ok(count)
}

/// Proof token returned once when a `BGIX` buffer passes header, fanout, and CRC-32 verification
/// (rule `verify-once`: prevents re-scanning the entire index CRC on every object lookup).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifiedIndex {
    len: usize,
    count: u32,
}

/// Zero-copy `BGIX` v1 binary index reader over mmap'd bytes.
#[derive(Clone, Copy)]
pub struct SlabIndex<'b> {
    bytes: &'b [u8],
    count: u32,
}

impl<'b> SlabIndex<'b> {
    /// Parses a mapped `BGIX` v1 file in O(1) touching ONLY the 1,040-byte header and fanout
    /// table (1 page even on a 1,000,000-object monorepo).
    ///
    /// Use this on the hot read-only mmap open path; use [`SlabIndex::parse`] for full-file
    /// CRC-32 verification (`fsck` / tests). Every object read from the slab still verifies its
    /// own per-record CRC-32 inside [`SlabReader::read_at`].
    pub fn parse_header_fast(bytes: &'b [u8], max_entries: u32) -> Result<SlabIndex<'b>, GitError> {
        if bytes.len() < BGIX_OIDS_OFF + 4 || &bytes[0..4] != BGIX_MAGIC {
            return Err(GitError { file: GitFile::SlabIndex, at: 0, kind: ErrorKind::Malformed });
        }
        let version = u32_le(bytes, 4);
        if version != 1 {
            return Err(GitError {
                file: GitFile::SlabIndex,
                at: 4,
                kind: ErrorKind::UnsupportedVersion(version),
            });
        }
        let count = u32_le(bytes, 8);
        if count > max_entries {
            return Err(GitError {
                file: GitFile::SlabIndex,
                at: 8,
                kind: ErrorKind::TooManyEntries { count, most: max_entries },
            });
        }
        let expected = BGIX_OIDS_OFF + (count as usize) * (20 + 8 + 4) + 4;
        if bytes.len() != expected {
            return Err(GitError { file: GitFile::SlabIndex, at: 0, kind: ErrorKind::Malformed });
        }
        let mut prev = 0u32;
        for b in 0..256usize {
            let cur = u32_be(bytes, BGIX_HEADER_LEN + b * 4);
            if cur < prev || cur > count {
                return Err(GitError {
                    file: GitFile::SlabIndex,
                    at: (BGIX_HEADER_LEN + b * 4) as u64,
                    kind: ErrorKind::Malformed,
                });
            }
            prev = cur;
        }
        if prev != count {
            return Err(GitError {
                file: GitFile::SlabIndex,
                at: (BGIX_OIDS_OFF - 4) as u64,
                kind: ErrorKind::Malformed,
            });
        }
        Ok(SlabIndex { bytes, count })
    }

    pub fn parse(bytes: &'b [u8], max_entries: u32) -> Result<SlabIndex<'b>, GitError> {
        let idx = SlabIndex::parse_header_fast(bytes, max_entries)?;
        let want_crc = u32_le(bytes, bytes.len() - 4);
        if crc32(&bytes[..bytes.len() - 4]) != want_crc {
            return Err(GitError {
                file: GitFile::SlabIndex,
                at: (bytes.len() - 4) as u64,
                kind: ErrorKind::ChecksumMismatch,
            });
        }
        Ok(idx)
    }

    /// Mints a [`VerifiedIndex`] proof from this already-verified [`SlabIndex`].
    #[inline]
    pub const fn proof(&self) -> VerifiedIndex {
        VerifiedIndex { len: self.bytes.len(), count: self.count }
    }

    /// Re-borrows an immutable `BGIX` buffer in O(1) using a [`VerifiedIndex`] proof minted on open.
    #[inline]
    pub fn from_verified(bytes: &'b [u8], proof: VerifiedIndex) -> Result<SlabIndex<'b>, GitError> {
        if bytes.len() != proof.len {
            return Err(GitError { file: GitFile::SlabIndex, at: 0, kind: ErrorKind::Malformed });
        }
        Ok(SlabIndex { bytes, count: proof.count })
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    #[inline]
    pub fn oid_at(&self, idx: u32) -> &'b [u8] {
        let start = BGIX_OIDS_OFF + (idx as usize) * 20;
        &self.bytes[start..start + 20]
    }

    #[inline]
    pub fn loc_at(&self, idx: u32) -> SlabLoc {
        let locs_off = BGIX_OIDS_OFF + (self.count as usize) * 20;
        let lens_off = locs_off + (self.count as usize) * 8;
        let packed = u64_le(self.bytes, locs_off + (idx as usize) * 8);
        let raw_len = u32_le(self.bytes, lens_off + (idx as usize) * 4);
        SlabLoc {
            slab_id: (packed >> 48) as u16,
            offset: packed & 0x0000_FFFF_FFFF_FFFF,
            raw_len,
        }
    }

    /// O(log N) lookup by OID using the 256-entry fanout table, zero allocations.
    pub fn lookup(&self, oid: &Oid) -> Option<SlabLoc> {
        let first = usize::from(oid.0[0]);
        let mut lo = if first == 0 { 0 } else { u32_be(self.bytes, BGIX_HEADER_LEN + (first - 1) * 4) };
        let mut hi = u32_be(self.bytes, BGIX_HEADER_LEN + first * 4);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.oid_at(mid).cmp(&oid.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(self.loc_at(mid)),
            }
        }
        None
    }
}

/// Encodes `HEAD` and a list of refs into a single atomic `BGRF` v1 binary reftable.
pub fn encode_reftable<'a>(
    head: Option<&RefTarget<'_>>,
    refs: &[RefEntry<'_>],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    out.clear();
    let mut order: AVec<'a, usize> = Vec::with_capacity_in(refs.len(), alloc);
    for i in 0..refs.len() {
        order.push(i);
    }
    order.sort_by(|&a, &b| refs[a].name.as_str().cmp(refs[b].name.as_str()));

    let count = u32::try_from(order.len()).map_err(|_| GitError {
        file: GitFile::RefTable,
        at: 0,
        kind: ErrorKind::BudgetExceeded,
    })?;
    out.extend_from_slice(BGRF_MAGIC);
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());

    match head {
        None => {
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        Some(RefTarget::Direct(oid)) => {
            out.extend_from_slice(&[1, 0, 0, 0]);
            out.extend_from_slice(&20u32.to_le_bytes());
            out.extend_from_slice(&oid.0);
        }
        Some(RefTarget::Symbolic(s)) => {
            let bytes = s.as_str().as_bytes();
            let len = u32::try_from(bytes.len()).map_err(|_| GitError {
                file: GitFile::RefTable,
                at: 16,
                kind: ErrorKind::BudgetExceeded,
            })?;
            out.extend_from_slice(&[2, 0, 0, 0]);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }

    let mut offsets: AVec<'a, u32> = Vec::with_capacity_in(order.len(), alloc);
    for idx in order {
        let entry_off = u32::try_from(out.len()).map_err(|_| GitError {
            file: GitFile::RefTable,
            at: out.len() as u64,
            kind: ErrorKind::BudgetExceeded,
        })?;
        offsets.push(entry_off);
        let r = &refs[idx];
        let name_b = r.name.as_str().as_bytes();
        let name_len = u16::try_from(name_b.len()).map_err(|_| GitError {
            file: GitFile::RefTable,
            at: out.len() as u64,
            kind: ErrorKind::BudgetExceeded,
        })?;
        let (kind_b, target_b): (u8, &[u8]) = match &r.target {
            RefTarget::Direct(oid) => (1, &oid.0),
            RefTarget::Symbolic(s) => (2, s.as_str().as_bytes()),
        };
        let target_len = u16::try_from(target_b.len()).map_err(|_| GitError {
            file: GitFile::RefTable,
            at: out.len() as u64,
            kind: ErrorKind::BudgetExceeded,
        })?;
        out.extend_from_slice(&name_len.to_le_bytes());
        out.push(kind_b);
        out.push(u8::from(r.peeled.is_some()));
        out.extend_from_slice(&target_len.to_le_bytes());
        out.extend_from_slice(name_b);
        out.extend_from_slice(target_b);
        if let Some(peeled) = r.peeled {
            out.extend_from_slice(&peeled.0);
        }
    }

    // BGRF v2 trailing O(log R) binary-search offset table (`[u32; count]`) right before CRC-32.
    for off in offsets {
        out.extend_from_slice(&off.to_le_bytes());
    }

    let crc = crc32(out);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Zero-copy borrowed ref target read directly from an mmap'd `BGRF` reftable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorrowedRefTarget<'b> {
    Direct(Oid),
    Symbolic(&'b str),
}

/// Zero-copy borrowed ref entry read directly from an mmap'd `BGRF` reftable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BorrowedRefEntry<'b> {
    pub name: &'b str,
    pub target: BorrowedRefTarget<'b>,
    pub peeled: Option<Oid>,
}

/// Zero-copy view over a `BGRF` v1 or v2 binary reftable.
///
/// Opening is strictly O(1) (verifies the 20-byte header and offset-table bounds with zero heap
/// allocation and zero full-file CRC scan); lookups on v2 are O(log R) binary searches directly
/// over the mapped bytes!
#[derive(Clone, Copy, Debug)]
pub struct RefTableView<'b> {
    bytes: &'b [u8],
    version: u32,
    count: u32,
    head_kind: u8,
    head_len: usize,
    entries_start: usize,
    entries_end: usize,
    offsets_start: usize,
}

impl<'b> RefTableView<'b> {
    /// Parses header and table bounds in O(1) without scanning all refs or allocating.
    pub fn parse_fast(bytes: &'b [u8], max_refs: u32) -> Result<RefTableView<'b>, GitError> {
        if bytes.len() < 24 || &bytes[0..4] != BGRF_MAGIC {
            return Err(GitError { file: GitFile::RefTable, at: 0, kind: ErrorKind::Malformed });
        }
        let version = u32_le(bytes, 4);
        if version != 1 && version != 2 {
            return Err(GitError {
                file: GitFile::RefTable,
                at: 4,
                kind: ErrorKind::UnsupportedVersion(version),
            });
        }
        let count = u32_le(bytes, 8);
        if count > max_refs {
            return Err(GitError {
                file: GitFile::RefTable,
                at: 8,
                kind: ErrorKind::TooManyEntries { count, most: max_refs },
            });
        }
        let head_kind = bytes[12];
        let head_len = u32_le(bytes, 16) as usize;
        let body_end = bytes.len() - 4;
        let entries_start = 20usize.checked_add(head_len).ok_or(GitError {
            file: GitFile::RefTable,
            at: 16,
            kind: ErrorKind::Malformed,
        })?;
        let offset_table_bytes = if version == 2 {
            (count as usize).checked_mul(4).ok_or(GitError {
                file: GitFile::RefTable,
                at: 8,
                kind: ErrorKind::Malformed,
            })?
        } else {
            0
        };
        if entries_start + offset_table_bytes > body_end {
            return Err(GitError {
                file: GitFile::RefTable,
                at: 20,
                kind: ErrorKind::Malformed,
            });
        }
        let entries_end = body_end - offset_table_bytes;
        let offsets_start = entries_end;
        Ok(RefTableView {
            bytes,
            version,
            count,
            head_kind,
            head_len,
            entries_start,
            entries_end,
            offsets_start,
        })
    }

    /// Number of refs stored in this table.
    #[inline]
    pub const fn len(&self) -> u32 {
        self.count
    }

    /// Reads `HEAD` directly from the mapped header in O(1).
    pub fn head(&self) -> Result<Option<BorrowedRefTarget<'b>>, GitError> {
        let pos = 20usize;
        match self.head_kind {
            0 => Ok(None),
            1 => {
                if self.head_len != 20 {
                    return Err(GitError {
                        file: GitFile::RefTable,
                        at: pos as u64,
                        kind: ErrorKind::Malformed,
                    });
                }
                Ok(Some(BorrowedRefTarget::Direct(Oid(
                    self.bytes[pos..pos + 20].try_into().expect("20 bytes"),
                ))))
            }
            2 => {
                let s = core::str::from_utf8(&self.bytes[pos..pos + self.head_len]).map_err(|_| {
                    GitError { file: GitFile::RefTable, at: pos as u64, kind: ErrorKind::Malformed }
                })?;
                Ok(Some(BorrowedRefTarget::Symbolic(s)))
            }
            _ => Err(GitError { file: GitFile::RefTable, at: 12, kind: ErrorKind::Malformed }),
        }
    }

    fn entry_at_offset(&self, pos: usize) -> Result<(BorrowedRefEntry<'b>, usize), GitError> {
        if pos + 6 > self.entries_end {
            return Err(GitError {
                file: GitFile::RefTable,
                at: pos as u64,
                kind: ErrorKind::Malformed,
            });
        }
        let name_len = u16::from_le_bytes([self.bytes[pos], self.bytes[pos + 1]]) as usize;
        let kind_b = self.bytes[pos + 2];
        let has_peeled = self.bytes[pos + 3];
        let target_len = u16::from_le_bytes([self.bytes[pos + 4], self.bytes[pos + 5]]) as usize;
        let mut cur = pos + 6;
        let extra_peeled = if has_peeled != 0 { 20 } else { 0 };
        if cur + name_len + target_len + extra_peeled > self.entries_end {
            return Err(GitError {
                file: GitFile::RefTable,
                at: cur as u64,
                kind: ErrorKind::Malformed,
            });
        }
        let name = core::str::from_utf8(&self.bytes[cur..cur + name_len]).map_err(|_| GitError {
            file: GitFile::RefTable,
            at: cur as u64,
            kind: ErrorKind::Malformed,
        })?;
        cur += name_len;
        let target = match kind_b {
            1 => {
                if target_len != 20 {
                    return Err(GitError {
                        file: GitFile::RefTable,
                        at: cur as u64,
                        kind: ErrorKind::Malformed,
                    });
                }
                BorrowedRefTarget::Direct(Oid(
                    self.bytes[cur..cur + 20].try_into().expect("20 bytes"),
                ))
            }
            2 => {
                let s = core::str::from_utf8(&self.bytes[cur..cur + target_len]).map_err(|_| {
                    GitError { file: GitFile::RefTable, at: cur as u64, kind: ErrorKind::Malformed }
                })?;
                BorrowedRefTarget::Symbolic(s)
            }
            _ => {
                return Err(GitError {
                    file: GitFile::RefTable,
                    at: (pos + 2) as u64,
                    kind: ErrorKind::Malformed,
                });
            }
        };
        cur += target_len;
        let peeled = if has_peeled != 0 {
            let p = Oid(self.bytes[cur..cur + 20].try_into().expect("20 bytes"));
            cur += 20;
            Some(p)
        } else {
            None
        };
        Ok((BorrowedRefEntry { name, target, peeled }, cur))
    }

    #[inline]
    fn offset_of(&self, idx: u32) -> usize {
        u32_le(self.bytes, self.offsets_start + (idx as usize) * 4) as usize
    }

    fn name_at_index(&self, idx: u32) -> Result<&'b str, GitError> {
        let pos = self.offset_of(idx);
        if pos + 6 > self.entries_end {
            return Err(GitError {
                file: GitFile::RefTable,
                at: pos as u64,
                kind: ErrorKind::Malformed,
            });
        }
        let name_len = u16::from_le_bytes([self.bytes[pos], self.bytes[pos + 1]]) as usize;
        let start = pos + 6;
        if start + name_len > self.entries_end {
            return Err(GitError {
                file: GitFile::RefTable,
                at: start as u64,
                kind: ErrorKind::Malformed,
            });
        }
        core::str::from_utf8(&self.bytes[start..start + name_len]).map_err(|_| GitError {
            file: GitFile::RefTable,
            at: start as u64,
            kind: ErrorKind::Malformed,
        })
    }

    /// Lower bound index for `key` using the v2 offset table (`O(log R)`).
    fn lower_bound(&self, key: &str) -> Result<u32, GitError> {
        let mut lo = 0u32;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let name = self.name_at_index(mid)?;
            if name < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Looks up one ref by exact name (`O(log R)` on v2, `O(R)` on v1, zero heap allocations).
    pub fn get(&self, name: &str) -> Result<Option<BorrowedRefEntry<'b>>, GitError> {
        if self.version == 2 {
            let idx = self.lower_bound(name)?;
            if idx < self.count {
                let (entry, _) = self.entry_at_offset(self.offset_of(idx))?;
                if entry.name == name {
                    return Ok(Some(entry));
                }
            }
            return Ok(None);
        }
        let mut pos = self.entries_start;
        for _ in 0..self.count {
            let (entry, next) = self.entry_at_offset(pos)?;
            if entry.name == name {
                return Ok(Some(entry));
            }
            if entry.name > name {
                break;
            }
            pos = next;
        }
        Ok(None)
    }

    /// Resolves `"HEAD"` or any ref name to its final direct [`Oid`] (`O(log R)` on v2, zero allocs).
    pub fn resolve(&self, name: &str) -> Result<Option<Oid>, GitError> {
        let mut cur = name;
        for _ in 0..8 {
            let target = if cur == "HEAD" {
                self.head()?
            } else {
                self.get(cur)?.map(|e| e.target)
            };
            match target {
                None => return Ok(None),
                Some(BorrowedRefTarget::Direct(oid)) => return Ok(Some(oid)),
                Some(BorrowedRefTarget::Symbolic(next)) => cur = next,
            }
        }
        Ok(None)
    }

    /// Appends all refs starting with `prefix` into `out` (`O(log R + K)` on v2!).
    pub fn list_prefix<'a>(
        &self,
        prefix: &str,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, RefEntry<'a>>,
    ) -> Result<(), GitError> {
        if self.version == 2 {
            let mut idx = if prefix.is_empty() { 0 } else { self.lower_bound(prefix)? };
            while idx < self.count {
                let (entry, _) = self.entry_at_offset(self.offset_of(idx))?;
                if !entry.name.starts_with(prefix) {
                    break;
                }
                let target = match entry.target {
                    BorrowedRefTarget::Direct(o) => RefTarget::Direct(o),
                    BorrowedRefTarget::Symbolic(s) => RefTarget::Symbolic(String::from_str_in(s, alloc)),
                };
                out.push(RefEntry {
                    name: String::from_str_in(entry.name, alloc),
                    target,
                    peeled: entry.peeled,
                });
                idx += 1;
            }
            return Ok(());
        }
        let mut pos = self.entries_start;
        for _ in 0..self.count {
            let (entry, next) = self.entry_at_offset(pos)?;
            if entry.name.starts_with(prefix) {
                let target = match entry.target {
                    BorrowedRefTarget::Direct(o) => RefTarget::Direct(o),
                    BorrowedRefTarget::Symbolic(s) => RefTarget::Symbolic(String::from_str_in(s, alloc)),
                };
                out.push(RefEntry {
                    name: String::from_str_in(entry.name, alloc),
                    target,
                    peeled: entry.peeled,
                });
            } else if !prefix.is_empty() && entry.name > prefix {
                break;
            }
            pos = next;
        }
        Ok(())
    }
}

/// Decoded single-file `BGRF` v1/v2 reftable.
pub struct DecodedRefTable<'a> {
    pub head: Option<RefTarget<'a>>,
    pub refs: AVec<'a, RefEntry<'a>>,
}

/// Parses and CRC-verifies a `BGRF` v1 or v2 binary reftable.
pub fn decode_reftable<'a>(
    bytes: &[u8],
    max_refs: u32,
    alloc: Alloc<'a>,
) -> Result<DecodedRefTable<'a>, GitError> {
    let view = RefTableView::parse_fast(bytes, max_refs)?;
    let body_end = bytes.len() - 4;
    let want_crc = u32_le(bytes, body_end);
    if crc32(&bytes[..body_end]) != want_crc {
        return Err(GitError {
            file: GitFile::RefTable,
            at: body_end as u64,
            kind: ErrorKind::ChecksumMismatch,
        });
    }
    let head = view.head()?.map(|t| match t {
        BorrowedRefTarget::Direct(o) => RefTarget::Direct(o),
        BorrowedRefTarget::Symbolic(s) => RefTarget::Symbolic(String::from_str_in(s, alloc)),
    });
    let mut refs = Vec::with_capacity_in(view.len() as usize, alloc);
    view.list_prefix("", alloc, &mut refs)?;
    Ok(DecodedRefTable { head, refs })
}

/// One commit's metadata fed into [`encode_commit_graph`].
pub struct GraphCommitInput<'a> {
    pub oid: Oid,
    pub tree: Oid,
    pub commit_time: u64,
    pub parents: AVec<'a, Oid>,
}

const PARENT_NONE: u32 = 0x7000_0000;
const EDGE_MASK: u32 = 0x8000_0000;

/// Writes a standard Git `CGPH` v1 binary commit-graph (`OIDF`, `OIDL`, `CDAT`, optional `EDGE`,
/// trailing SHA-1 checksum) with exact topological generation numbers (`gen >= 1`).
///
/// Directly readable by both [`crate::commit_graph::CommitGraphLayer::parse`] and standard `git`.
pub fn encode_commit_graph<'a>(
    commits: &[GraphCommitInput<'a>],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<u32, GitError> {
    out.clear();
    if commits.is_empty() {
        return Ok(0);
    }

    // Sort indices by commit OID and deduplicate.
    let mut order: AVec<'a, usize> = Vec::with_capacity_in(commits.len(), alloc);
    for i in 0..commits.len() {
        order.push(i);
    }
    order.sort_by(|&a, &b| commits[a].oid.cmp(&commits[b].oid));
    let mut unique: AVec<'a, usize> = Vec::with_capacity_in(order.len(), alloc);
    for idx in order {
        if unique.last().is_some_and(|&prev| commits[prev].oid == commits[idx].oid) {
            continue;
        }
        unique.push(idx);
    }
    let count = u32::try_from(unique.len()).map_err(|_| GitError {
        file: GitFile::CommitGraph,
        at: 0,
        kind: ErrorKind::BudgetExceeded,
    })?;

    let mut pos_by_oid: Map<Oid, u32, BuildKeyHasher, Alloc<'a>> = Map::new(MapOpts {
        hasher: BuildKeyHasher::default(),
        alloc,
        capacity: Some(unique.len()),
    });
    for (gpos, &src_idx) in unique.iter().enumerate() {
        pos_by_oid.insert(commits[src_idx].oid, gpos as u32);
    }

    // Compute exact topological generation numbers iteratively (explicit stack, no recursion).
    let mut gens: AVec<'a, u32> = Vec::with_capacity_in(unique.len(), alloc);
    for _ in 0..unique.len() {
        gens.push(0);
    }
    let mut stack: AVec<'a, (u32, bool)> = Vec::new_in(alloc);
    for start in 0..count {
        if gens[start as usize] != 0 {
            continue;
        }
        stack.push((start, false));
        while let Some((node, expanded)) = stack.pop() {
            if gens[node as usize] != 0 {
                continue;
            }
            let c = &commits[unique[node as usize]];
            if !expanded {
                stack.push((node, true));
                for p in &c.parents {
                    if let Some(&p_pos) = pos_by_oid.get(p)
                        && gens[p_pos as usize] == 0
                    {
                        stack.push((p_pos, false));
                    }
                }
            } else {
                let mut max_p = 0u32;
                for p in &c.parents {
                    if let Some(&p_pos) = pos_by_oid.get(p) {
                        max_p = max_p.max(gens[p_pos as usize]);
                    }
                }
                // Cap at 30-bit max (0x3FFF_FFFF) per Git commit-graph format spec.
                gens[node as usize] = max_p.saturating_add(1).min(0x3FFF_FFFF);
            }
        }
    }

    // Build EDGE chunk if any commit has >= 3 parents.
    let mut edge_chunk: AVec<'a, u8> = Vec::new_in(alloc);
    let mut cdat_chunk: AVec<'a, u8> = Vec::with_capacity_in(unique.len() * 36, alloc);
    for (gpos, &src_idx) in unique.iter().enumerate() {
        let c = &commits[src_idx];
        cdat_chunk.extend_from_slice(&c.tree.0);

        let mut p_indices: AVec<'a, u32> = Vec::with_capacity_in(c.parents.len(), alloc);
        for p in &c.parents {
            if let Some(&p_pos) = pos_by_oid.get(p) {
                p_indices.push(p_pos);
            }
        }
        let (p1, p2) = match p_indices.len() {
            0 => (PARENT_NONE, PARENT_NONE),
            1 => (p_indices[0], PARENT_NONE),
            2 => (p_indices[0], p_indices[1]),
            _ => {
                let edge_idx = (edge_chunk.len() / 4) as u32;
                for (k, &p_pos) in p_indices[1..].iter().enumerate() {
                    let is_last = k + 1 == p_indices.len() - 1;
                    let word = if is_last { p_pos | EDGE_MASK } else { p_pos };
                    edge_chunk.extend_from_slice(&word.to_be_bytes());
                }
                (p_indices[0], edge_idx | EDGE_MASK)
            }
        };
        cdat_chunk.extend_from_slice(&p1.to_be_bytes());
        cdat_chunk.extend_from_slice(&p2.to_be_bytes());
        let generation = u64::from(gens[gpos]);
        let time = c.commit_time & 0x0000_0003_FFFF_FFFF;
        let gen_time = (generation << 34) | time;
        cdat_chunk.extend_from_slice(&gen_time.to_be_bytes());
    }

    let has_edge = !edge_chunk.is_empty();
    let num_chunks: u8 = if has_edge { 4 } else { 3 };
    let header_len = 8 + (usize::from(num_chunks) + 1) * 12;
    let oidf_off = header_len as u64;
    let oidl_off = oidf_off + 256 * 4;
    let cdat_off = oidl_off + u64::from(count) * 20;
    let edge_off = cdat_off + cdat_chunk.len() as u64;
    let end_off = edge_off + edge_chunk.len() as u64;

    out.extend_from_slice(b"CGPH");
    out.push(1); // version 1
    out.push(1); // SHA-1
    out.push(num_chunks);
    out.push(0); // base graphs = 0

    // Chunk lookup table.
    out.extend_from_slice(b"OIDF");
    out.extend_from_slice(&oidf_off.to_be_bytes());
    out.extend_from_slice(b"OIDL");
    out.extend_from_slice(&oidl_off.to_be_bytes());
    out.extend_from_slice(b"CDAT");
    out.extend_from_slice(&cdat_off.to_be_bytes());
    if has_edge {
        out.extend_from_slice(b"EDGE");
        out.extend_from_slice(&edge_off.to_be_bytes());
    }
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&end_off.to_be_bytes());

    // OIDF
    let mut running = 0u32;
    let mut idx = 0usize;
    for b in 0..256usize {
        while idx < unique.len() && usize::from(commits[unique[idx]].oid.0[0]) <= b {
            running += 1;
            idx += 1;
        }
        out.extend_from_slice(&running.to_be_bytes());
    }
    // OIDL
    for &src_idx in &unique {
        out.extend_from_slice(&commits[src_idx].oid.0);
    }
    // CDAT + EDGE
    out.extend_from_slice(&cdat_chunk);
    if has_edge {
        out.extend_from_slice(&edge_chunk);
    }
    // Trailing 20-byte SHA-1 checksum.
    let digest = sha1(out);
    out.extend_from_slice(&digest);
    Ok(count)
}

/// Builds a `CGPH` v1 commit-graph directly from every commit object in `store`.
pub fn build_commit_graph_from_store<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<u32, GitError> {
    let mut oids = Vec::new_in(alloc);
    store.list_oids(&mut oids)?;
    let mut obj_buf = Vec::new_in(alloc);
    let mut commits: AVec<'a, GraphCommitInput<'a>> = Vec::new_in(alloc);
    for oid in oids {
        let Some(kind) = store.read_object(&oid, &mut obj_buf)? else {
            continue;
        };
        if kind != ObjectKind::Commit {
            continue;
        }
        let parsed = blaze_git_fetch::store::Commit::parse(&obj_buf).map_err(|(at, why)| GitError {
            file: GitFile::CommitObject(oid),
            at,
            kind: ErrorKind::CommitParse(why),
        })?;
        let mut parents = Vec::new_in(alloc);
        for p in parsed.parents() {
            parents.push(p);
        }
        commits.push(GraphCommitInput {
            oid,
            tree: parsed.tree,
            commit_time: u64::try_from(parsed.committed).unwrap_or(0),
            parents,
        });
    }
    encode_commit_graph(&commits, alloc, out)
}
