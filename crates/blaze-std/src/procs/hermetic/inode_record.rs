//! Immutable write-once on-disk ELF inode records (AGENTS.md Rule 51: no process-memory cache;
//! rely on the kernel page cache for fast reads across tasks and commands).
//!
//! Each record caches the pure-content facts of one regular file inode (its ELF64 header/dynamic
//! section metadata and optionally its full-file SHA-256 + length), keyed strictly by BOTH the
//! parser code identity (`parser_identity()` over `landlock.rs` + `inode_record.rs`) AND the live
//! `fstat(fd)` [`InodeStamp`], guarded by a 2-second racy-clean window so same-tick in-place
//! writes never read or write stale records.
//!
//! Durability & corruption policy:
//! - Records are published atomically with `Durable::Yes` (sibling temp file -> flush -> rename ->
//!   sync directory) only on cold misses, so even a sudden power cut never leaves a torn record
//!   at the final path.
//! - Any existing record that is oversized, truncated, fails its BLAKE3 seal, has bad magic/
//!   version/flags/layout, or mismatches the expected [`InodeStamp`] is reported loudly as a typed
//!   corruption error (`EBADMSG`, code 74) instead of silently falling back to live hashing
//!   (AGENTS.md Rule 3). Only genuine `FsKind::NotFound`, racy-clean timestamps, or valid
//!   metadata-only records upgraded to require a full-file digest return `Ok(None)`.
//! - Write-side directory bounding evicts oldest-mtime records when the directory exceeds
//!   [`MAX_INODE_RECORD_ENTRIES`], complementing `blaze gc` expiry/LRU sweeps.
//!
//! Layout (all integers little-endian):
//! - `[0..8]`   magic `b"BZELFIN1"`
//! - `[8..12]`  format version `1u32`
//! - `[12..72]` `InodeStamp` (60 bytes: dev, ino, mode, size, mtime_sec, mtime_nsec, ctime_sec, ctime_nsec)
//! - `[72]`     flags (`bit 0` = has_digest, `bit 1` = has_dyn, `bit 2` = has_runpath, `bit 3` = has_rpath)
//! - `[73..75]` `e_machine: u16`
//! - `[75..83]` `file_len: u64` (0 when `!has_digest`)
//! - `[83..115]` `sha256: [u8; 32]` (zeros when `!has_digest`)
//! - `[115..119]` `interp_len: u32`
//! - `[119..123]` `runpath_len: u32`
//! - `[123..127]` `rpath_len: u32`
//! - `[127..131]` `needed_len: u32`
//! - `[131..N-32]` payload bytes (`interp`, `runpath`, `rpath`, `needed_blob`)
//! - `[N-32..N]` trailing 32-byte BLAKE3 self-checksum over
//!   `RECORD_SEAL_DOMAIN || parser_identity || bytes[..N-32]`.

use blaze_own::{SharedAlloc, Vec as OwnVec};

use super::Fault;

/// Subdirectory under `<home>/.blaze/v2` holding immutable write-once ELF inode records.
pub const ELF_INODE_RECORD_SUBDIR: &str = "build-script/records/elf-inodes-v1";

/// Maximum serialized ELF inode record size accepted on read or write (64 KiB).
/// Checked via file metadata BEFORE allocating a buffer.
pub const MAX_INODE_RECORD_BYTES: usize = 64 << 10;

/// Maximum number of on-disk ELF inode record files retained in `elf-inodes-v1` before cold-miss
/// publishes prune oldest-mtime entries down to a 75% low watermark.
pub const MAX_INODE_RECORD_ENTRIES: usize = 512;

/// Upper bound on directory entries scanned during one bounded write-side prune pass (Rule 18).
const PRUNE_SCAN_MOST: usize = 4096;

/// Linux/POSIX `EBADMSG` errno reported for corrupt on-disk ELF inode records.
const EBADMSG: u16 = 74;

const MAGIC: [u8; 8] = *b"BZELFIN1";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 131;
const CHECKSUM_LEN: usize = 32;
const PARSER_ID_DOMAIN: &[u8] = b"blaze-elf-inode-parser-id-v1\0";
const RECORD_KEY_DOMAIN: &[u8] = b"blaze-elf-inode-record-v1\0";
const RECORD_SEAL_DOMAIN: &[u8] = b"blaze-elf-inode-record-seal-v1\0";

const FLAG_HAS_DIGEST: u8 = 1 << 0;
const FLAG_HAS_DYN: u8 = 1 << 1;
const FLAG_HAS_RUNPATH: u8 = 1 << 2;
const FLAG_HAS_RPATH: u8 = 1 << 3;
const VALID_FLAGS_MASK: u8 = FLAG_HAS_DIGEST | FLAG_HAS_DYN | FLAG_HAS_RUNPATH | FLAG_HAS_RPATH;

const MAX_INTERP_LEN: usize = 512;
const MAX_RPATH_LEN: usize = 2048;
const MAX_NEEDED_BLOB_LEN: usize = 128 * 320;

/// Content digest of the ELF parser (`landlock.rs`) and inode-record codec (`inode_record.rs`)
/// source bytes. Mixed into every record filename AND record seal so any change to ELF parsing or
/// decoding automatically rolls every on-disk inode record key (Rule 21 & Rule 69).
pub fn parser_identity() -> [u8; 32] {
    let mut h = crate::digest::Blake3::new();
    h.update(PARSER_ID_DOMAIN);
    h.update(include_bytes!("landlock.rs"));
    h.update(include_bytes!("inode_record.rs"));
    h.finalize()
}

/// Typed corruption reason reported loudly (`EBADMSG`, code 74) whenever an existing on-disk ELF
/// inode record fails validation (never silently downgraded to a cache miss, Rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordCorrupt {
    Oversized,
    Truncated,
    BadChecksum,
    BadMagic,
    BadVersion,
    StampMismatch,
    InvalidFlags,
    InvalidLayout,
}

impl RecordCorrupt {
    /// Converts this corruption reason into a descriptive [`Fault::Host`] (`EBADMSG`).
    pub const fn into_fault(self) -> Fault {
        let call = match self {
            RecordCorrupt::Oversized => "elf inode record corrupt (oversized)",
            RecordCorrupt::Truncated => "elf inode record corrupt (truncated)",
            RecordCorrupt::BadChecksum => "elf inode record corrupt (bad checksum)",
            RecordCorrupt::BadMagic => "elf inode record corrupt (bad magic)",
            RecordCorrupt::BadVersion => "elf inode record corrupt (bad version)",
            RecordCorrupt::StampMismatch => "elf inode record corrupt (stamp mismatch)",
            RecordCorrupt::InvalidFlags => "elf inode record corrupt (invalid flags)",
            RecordCorrupt::InvalidLayout => "elf inode record corrupt (invalid layout)",
        };
        Fault::Host { call, code: EBADMSG }
    }
}

/// Kernel inode metadata stamp of a live opened regular file descriptor (`fstat(fd)`):
/// `(st_dev, st_ino, st_mode, st_size, st_mtime, st_ctime)`. Any write, truncation, attribute
/// change, or atomic replacement changes at least one field and immediately selects a different
/// record key (Rule 21 + TOCTOU safe).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InodeStamp {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub size: i64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
}

impl InodeStamp {
    /// Classic racy-clean margin (seconds): files modified or created within the last 2 seconds
    /// bypass on-disk records so same-size in-place writes within a coarse filesystem timestamp
    /// tick can never collide with a persisted record. Immutable `/nix/store` and installed
    /// system libraries always pass immediately.
    pub const RACY_CLEAN_SECS: i64 = 2;

    #[cfg(target_os = "linux")]
    const S_IFMT: u32 = 0o170_000;
    #[cfg(target_os = "linux")]
    const S_IFREG: u32 = 0o100_000;

    /// Stats the already-opened live descriptor `fd`. Returns `Ok(None)` for non-regular files.
    #[cfg(target_os = "linux")]
    pub fn from_fd(fd: blaze_sys::Fd) -> Result<Option<Self>, Fault> {
        let st = blaze_sys::linux::task::fstat(fd)
            .map_err(|errno| Fault::Host { call: "fstat (nix runtime closure)", code: errno.raw() })?;
        if (st.st_mode & Self::S_IFMT) != Self::S_IFREG || st.st_size < 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            dev: st.st_dev,
            ino: st.st_ino,
            mode: st.st_mode,
            size: st.st_size,
            mtime_sec: st.st_mtime,
            mtime_nsec: st.st_mtime_nsec,
            ctime_sec: st.st_ctime,
            ctime_nsec: st.st_ctime_nsec,
        }))
    }

    /// Whether this stamp is safely outside the racy-clean window for wall-clock second `now_sec`.
    pub const fn is_cacheable(&self, now_sec: i64) -> bool {
        if now_sec < Self::RACY_CLEAN_SECS {
            return false;
        }
        let cutoff = now_sec - Self::RACY_CLEAN_SECS;
        self.size >= 0
            && self.mtime_sec >= 0
            && self.ctime_sec >= 0
            && self.mtime_sec <= cutoff
            && self.ctime_sec <= cutoff
    }

    /// Deterministic 64-byte lowercase hex record filename derived from
    /// `blake3("blaze-elf-inode-record-v1\0" || parser_identity() || stamp_le_bytes)`.
    pub fn record_key_hex(&self) -> [u8; 64] {
        let pid = parser_identity();
        self.record_key_hex_with_parser(&pid)
    }

    /// Deterministic 64-byte lowercase hex record filename parameterized by explicit `parser_id`
    /// so tests can prove that parser code changes roll every on-disk key.
    pub fn record_key_hex_with_parser(&self, parser_id: &[u8; 32]) -> [u8; 64] {
        let mut h = crate::digest::Blake3::new();
        h.update(RECORD_KEY_DOMAIN);
        h.update(parser_id);
        h.update(&self.dev.to_le_bytes());
        h.update(&self.ino.to_le_bytes());
        h.update(&self.mode.to_le_bytes());
        h.update(&self.size.to_le_bytes());
        h.update(&self.mtime_sec.to_le_bytes());
        h.update(&self.mtime_nsec.to_le_bytes());
        h.update(&self.ctime_sec.to_le_bytes());
        h.update(&self.ctime_nsec.to_le_bytes());
        let dig = h.finalize();
        let mut hex = [0u8; 64];
        for (i, &b) in dig.iter().enumerate() {
            if let Some(hi) = hex.get_mut(i * 2) {
                *hi = hex_digit(b >> 4);
            }
            if let Some(lo) = hex.get_mut(i * 2 + 1) {
                *lo = hex_digit(b & 0x0f);
            }
        }
        hex
    }
}

const fn hex_digit(nibble: u8) -> u8 {
    match nibble & 0x0f {
        d @ 0..=9 => b'0' + d,
        d => b'a' + (d - 10),
    }
}

/// Immutable per-inode ELF64 summary + optional verified full-file SHA-256 digest.
///
/// `$ORIGIN`/`$LIB`/`$PLATFORM` expansion always happens live from the resolved path on every
/// walk, so hardlinks at distinct paths behave identically to an uncached walk.
#[derive(PartialEq, Eq, Debug)]
pub struct InodeRecord {
    pub stamp: InodeStamp,
    /// `Some((file_len, sha256))` when full-file SHA-256 was computed; `None` for
    /// ELF-metadata-only records (e.g. primary executable `clang-23`).
    pub digest: Option<(u64, [u8; 32])>,
    pub e_machine: u16,
    pub has_dyn: bool,
    pub interp: OwnVec<u8, SharedAlloc<'static>>,
    pub runpath: Option<OwnVec<u8, SharedAlloc<'static>>>,
    pub rpath: Option<OwnVec<u8, SharedAlloc<'static>>>,
    /// Flat NUL-separated raw `DT_NEEDED` SONAME bytes in ELF order.
    pub needed_blob: OwnVec<u8, SharedAlloc<'static>>,
}

/// Planted bugs for deterministic simulation tests (AGENTS.md Rules 23 & 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InodeRecordPlant {
    None,
    /// Control 1: skips trailing 32-byte BLAKE3 checksum verification on read.
    SkipChecksum,
    /// Control 2: skips the 2-second racy-clean guard on read and publish.
    SkipRacyClean,
    /// Control 3: writes records directly (non-atomically) to the final path instead of synced temp+rename.
    DirectWrite,
    /// Control 4: skips checking that the decoded record's `InodeStamp` matches the asked stamp.
    SkipStampCheck,
}

/// Result of reading a bounded on-disk inode record file.
pub enum RecordRead {
    Absent,
    Oversized,
    Bytes(OwnVec<u8, SharedAlloc<'static>>),
}

/// Sans-IO filesystem seam for immutable write-once ELF inode records, implemented in production
/// by [`DirInodeRecordIo`] over `bstd::fs::Dir<ReadWrite>`.
pub trait InodeRecordIo {
    /// Reads `key_hex` (64 ASCII hex chars), capping size at `max_bytes` BEFORE allocating.
    fn read_bounded(&self, key_hex: &str, max_bytes: usize) -> Result<RecordRead, Fault>;

    /// Publishes `bytes` atomically and durably at `key_hex` (unique sibling temp file + flush +
    /// rename + dir sync).
    fn publish_atomic(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault>;

    /// Non-atomic direct write at `key_hex` (invoked ONLY by [`InodeRecordPlant::DirectWrite`]
    /// in DST controls).
    fn write_direct_planted(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault>;

    /// Prunes oldest-mtime entries when the record directory exceeds `max_entries`, preserving
    /// `keep_key`. Invoked only on cold-miss publishes (never on warm lookups).
    fn prune_to_cap(&self, max_entries: usize, keep_key: &str) -> Result<(), Fault>;
}

/// Encodes `record` into canonical self-checksummed bytes bound to [`parser_identity()`].
/// Returns `None` if the encoded record would exceed [`MAX_INODE_RECORD_BYTES`].
pub fn encode_record(record: &InodeRecord) -> Option<OwnVec<u8, SharedAlloc<'static>>> {
    let pid = parser_identity();
    encode_record_with_parser(record, &pid)
}

/// Encodes `record` into canonical self-checksummed bytes bound to explicit `parser_id`.
pub fn encode_record_with_parser(
    record: &InodeRecord,
    parser_id: &[u8; 32],
) -> Option<OwnVec<u8, SharedAlloc<'static>>> {
    let runpath_bytes = record.runpath.as_ref().map_or(&[][..], OwnVec::as_slice);
    let rpath_bytes = record.rpath.as_ref().map_or(&[][..], OwnVec::as_slice);
    let interp_bytes = record.interp.as_slice();
    let needed_bytes = record.needed_blob.as_slice();

    if interp_bytes.len() > MAX_INTERP_LEN
        || runpath_bytes.len() > MAX_RPATH_LEN
        || rpath_bytes.len() > MAX_RPATH_LEN
        || needed_bytes.len() > MAX_NEEDED_BLOB_LEN
    {
        return None;
    }
    let body_len = HEADER_LEN
        .checked_add(interp_bytes.len())?
        .checked_add(runpath_bytes.len())?
        .checked_add(rpath_bytes.len())?
        .checked_add(needed_bytes.len())?;
    let total_len = body_len.checked_add(CHECKSUM_LEN)?;
    if total_len > MAX_INODE_RECORD_BYTES {
        return None;
    }

    let mut flags = 0u8;
    let (file_len, sha256) = match record.digest {
        Some((len, dig)) => {
            flags |= FLAG_HAS_DIGEST;
            (len, dig)
        }
        None => (0u64, [0u8; 32]),
    };
    if record.has_dyn {
        flags |= FLAG_HAS_DYN;
    }
    if record.runpath.is_some() {
        flags |= FLAG_HAS_RUNPATH;
    }
    if record.rpath.is_some() {
        flags |= FLAG_HAS_RPATH;
    }

    let interp_len = u32::try_from(interp_bytes.len()).ok()?;
    let runpath_len = u32::try_from(runpath_bytes.len()).ok()?;
    let rpath_len = u32::try_from(rpath_bytes.len()).ok()?;
    let needed_len = u32::try_from(needed_bytes.len()).ok()?;

    let mut out = OwnVec::with_capacity_in(total_len, SharedAlloc::global());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&record.stamp.dev.to_le_bytes());
    out.extend_from_slice(&record.stamp.ino.to_le_bytes());
    out.extend_from_slice(&record.stamp.mode.to_le_bytes());
    out.extend_from_slice(&record.stamp.size.to_le_bytes());
    out.extend_from_slice(&record.stamp.mtime_sec.to_le_bytes());
    out.extend_from_slice(&record.stamp.mtime_nsec.to_le_bytes());
    out.extend_from_slice(&record.stamp.ctime_sec.to_le_bytes());
    out.extend_from_slice(&record.stamp.ctime_nsec.to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&record.e_machine.to_le_bytes());
    out.extend_from_slice(&file_len.to_le_bytes());
    out.extend_from_slice(&sha256);
    out.extend_from_slice(&interp_len.to_le_bytes());
    out.extend_from_slice(&runpath_len.to_le_bytes());
    out.extend_from_slice(&rpath_len.to_le_bytes());
    out.extend_from_slice(&needed_len.to_le_bytes());
    out.extend_from_slice(interp_bytes);
    out.extend_from_slice(runpath_bytes);
    out.extend_from_slice(rpath_bytes);
    out.extend_from_slice(needed_bytes);

    let mut seal = crate::digest::Blake3::new();
    seal.update(RECORD_SEAL_DOMAIN);
    seal.update(parser_id);
    seal.update(out.as_slice());
    let checksum = seal.finalize();
    out.extend_from_slice(&checksum);
    Some(out)
}

/// Verifies and decodes a serialized ELF inode record against `expected_stamp` and
/// [`parser_identity()`], returning a typed [`RecordCorrupt`] on any validation failure.
pub fn decode_record(
    bytes: &[u8],
    expected_stamp: InodeStamp,
    plant: InodeRecordPlant,
) -> Result<InodeRecord, RecordCorrupt> {
    let pid = parser_identity();
    decode_record_with_parser(bytes, expected_stamp, &pid, plant)
}

/// Verifies and decodes a serialized ELF inode record against `expected_stamp` and explicit
/// `parser_id`, returning a typed [`RecordCorrupt`] on any validation failure.
pub fn decode_record_with_parser(
    bytes: &[u8],
    expected_stamp: InodeStamp,
    parser_id: &[u8; 32],
    plant: InodeRecordPlant,
) -> Result<InodeRecord, RecordCorrupt> {
    if bytes.len() > MAX_INODE_RECORD_BYTES {
        return Err(RecordCorrupt::Oversized);
    }
    if bytes.len() < HEADER_LEN + CHECKSUM_LEN {
        return Err(RecordCorrupt::Truncated);
    }
    let body_len = bytes.len().checked_sub(CHECKSUM_LEN).ok_or(RecordCorrupt::Truncated)?;
    let (body, trailing) = bytes.split_at_checked(body_len).ok_or(RecordCorrupt::Truncated)?;
    if plant != InodeRecordPlant::SkipChecksum {
        let mut seal = crate::digest::Blake3::new();
        seal.update(RECORD_SEAL_DOMAIN);
        seal.update(parser_id);
        seal.update(body);
        if seal.finalize().as_slice() != trailing {
            return Err(RecordCorrupt::BadChecksum);
        }
    }
    if body.get(..8) != Some(MAGIC.as_slice()) {
        return Err(RecordCorrupt::BadMagic);
    }
    if read_u32(body, 8).ok_or(RecordCorrupt::Truncated)? != VERSION {
        return Err(RecordCorrupt::BadVersion);
    }
    let stamp = InodeStamp {
        dev: read_u64(body, 12).ok_or(RecordCorrupt::Truncated)?,
        ino: read_u64(body, 20).ok_or(RecordCorrupt::Truncated)?,
        mode: read_u32(body, 28).ok_or(RecordCorrupt::Truncated)?,
        size: read_i64(body, 32).ok_or(RecordCorrupt::Truncated)?,
        mtime_sec: read_i64(body, 40).ok_or(RecordCorrupt::Truncated)?,
        mtime_nsec: read_i64(body, 48).ok_or(RecordCorrupt::Truncated)?,
        ctime_sec: read_i64(body, 56).ok_or(RecordCorrupt::Truncated)?,
        ctime_nsec: read_i64(body, 64).ok_or(RecordCorrupt::Truncated)?,
    };
    if plant != InodeRecordPlant::SkipStampCheck && stamp != expected_stamp {
        return Err(RecordCorrupt::StampMismatch);
    }
    let flags = *body.get(72).ok_or(RecordCorrupt::Truncated)?;
    if (flags & !VALID_FLAGS_MASK) != 0 {
        return Err(RecordCorrupt::InvalidFlags);
    }
    let e_machine = read_u16(body, 73).ok_or(RecordCorrupt::Truncated)?;
    let file_len = read_u64(body, 75).ok_or(RecordCorrupt::Truncated)?;
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(body.get(83..115).ok_or(RecordCorrupt::Truncated)?);
    let digest = if (flags & FLAG_HAS_DIGEST) != 0 {
        Some((file_len, sha256))
    } else {
        None
    };
    let has_dyn = (flags & FLAG_HAS_DYN) != 0;
    let has_runpath = (flags & FLAG_HAS_RUNPATH) != 0;
    let has_rpath = (flags & FLAG_HAS_RPATH) != 0;

    let interp_len = usize::try_from(read_u32(body, 115).ok_or(RecordCorrupt::Truncated)?)
        .map_err(|_| RecordCorrupt::InvalidLayout)?;
    let runpath_len = usize::try_from(read_u32(body, 119).ok_or(RecordCorrupt::Truncated)?)
        .map_err(|_| RecordCorrupt::InvalidLayout)?;
    let rpath_len = usize::try_from(read_u32(body, 123).ok_or(RecordCorrupt::Truncated)?)
        .map_err(|_| RecordCorrupt::InvalidLayout)?;
    let needed_len = usize::try_from(read_u32(body, 127).ok_or(RecordCorrupt::Truncated)?)
        .map_err(|_| RecordCorrupt::InvalidLayout)?;

    if interp_len > MAX_INTERP_LEN
        || runpath_len > MAX_RPATH_LEN
        || rpath_len > MAX_RPATH_LEN
        || needed_len > MAX_NEEDED_BLOB_LEN
        || (!has_runpath && runpath_len != 0)
        || (!has_rpath && rpath_len != 0)
    {
        return Err(RecordCorrupt::InvalidLayout);
    }
    let end_interp = HEADER_LEN.checked_add(interp_len).ok_or(RecordCorrupt::InvalidLayout)?;
    let end_runpath = end_interp.checked_add(runpath_len).ok_or(RecordCorrupt::InvalidLayout)?;
    let end_rpath = end_runpath.checked_add(rpath_len).ok_or(RecordCorrupt::InvalidLayout)?;
    let end_needed = end_rpath.checked_add(needed_len).ok_or(RecordCorrupt::InvalidLayout)?;
    if end_needed != body.len() {
        return Err(RecordCorrupt::InvalidLayout);
    }

    let shared = SharedAlloc::global();
    let mut interp = OwnVec::with_capacity_in(interp_len, shared);
    interp.extend_from_slice(body.get(HEADER_LEN..end_interp).ok_or(RecordCorrupt::InvalidLayout)?);
    let runpath = if has_runpath {
        let mut v = OwnVec::with_capacity_in(runpath_len, shared);
        v.extend_from_slice(body.get(end_interp..end_runpath).ok_or(RecordCorrupt::InvalidLayout)?);
        Some(v)
    } else {
        None
    };
    let rpath = if has_rpath {
        let mut v = OwnVec::with_capacity_in(rpath_len, shared);
        v.extend_from_slice(body.get(end_runpath..end_rpath).ok_or(RecordCorrupt::InvalidLayout)?);
        Some(v)
    } else {
        None
    };
    let mut needed_blob = OwnVec::with_capacity_in(needed_len, shared);
    needed_blob.extend_from_slice(body.get(end_rpath..end_needed).ok_or(RecordCorrupt::InvalidLayout)?);

    Ok(InodeRecord { stamp, digest, e_machine, has_dyn, interp, runpath, rpath, needed_blob })
}

/// Looks up an immutable on-disk ELF inode record for `stamp` at wall-clock second `now_sec`.
///
/// - Returns `Ok(None)` strictly for genuine cache misses: racy-clean timestamps, absent files
///   (`FsKind::NotFound`), or valid metadata-only records when `need_digest` is true.
/// - Returns `Err(Fault::Host { code: 74, .. })` on ANY existing oversized, truncated,
///   checksum-corrupt, bad-magic/version/flags/layout, or wrong-stamp record (Rule 3: never
///   silently fall back past store corruption).
pub fn lookup_inode_record(
    io: &dyn InodeRecordIo,
    stamp: InodeStamp,
    now_sec: i64,
    need_digest: bool,
    plant: InodeRecordPlant,
) -> Result<Option<InodeRecord>, Fault> {
    if plant != InodeRecordPlant::SkipRacyClean && !stamp.is_cacheable(now_sec) {
        return Ok(None);
    }
    let key_buf = stamp.record_key_hex();
    let Ok(key_str) = core::str::from_utf8(&key_buf) else {
        return Err(Fault::Host { call: "elf inode record key utf8", code: 22 });
    };
    match io.read_bounded(key_str, MAX_INODE_RECORD_BYTES)? {
        RecordRead::Absent => Ok(None),
        RecordRead::Oversized => Err(RecordCorrupt::Oversized.into_fault()),
        RecordRead::Bytes(raw) => {
            let rec = decode_record(raw.as_slice(), stamp, plant).map_err(RecordCorrupt::into_fault)?;
            if need_digest && rec.digest.is_none() {
                return Ok(None);
            }
            Ok(Some(rec))
        }
    }
}

/// Publishes an immutable on-disk ELF inode record if `record.stamp` is outside the racy-clean
/// window and fits within [`MAX_INODE_RECORD_BYTES`], enforcing the default directory cap
/// [`MAX_INODE_RECORD_ENTRIES`]. Returns `Ok(true)` if published.
pub fn publish_inode_record(
    io: &dyn InodeRecordIo,
    record: &InodeRecord,
    now_sec: i64,
    plant: InodeRecordPlant,
) -> Result<bool, Fault> {
    publish_inode_record_capped(io, record, now_sec, MAX_INODE_RECORD_ENTRIES, plant)
}

/// Same as [`publish_inode_record`] with an explicit entry cap `max_entries` (for bounded-store
/// tests and DSTs).
pub fn publish_inode_record_capped(
    io: &dyn InodeRecordIo,
    record: &InodeRecord,
    now_sec: i64,
    max_entries: usize,
    plant: InodeRecordPlant,
) -> Result<bool, Fault> {
    if max_entries == 0 {
        return Ok(false);
    }
    if plant != InodeRecordPlant::SkipRacyClean && !record.stamp.is_cacheable(now_sec) {
        return Ok(false);
    }
    let Some(encoded) = encode_record(record) else {
        return Ok(false);
    };
    let key_buf = record.stamp.record_key_hex();
    let Ok(key_str) = core::str::from_utf8(&key_buf) else {
        return Ok(false);
    };
    if plant == InodeRecordPlant::DirectWrite {
        io.write_direct_planted(key_str, encoded.as_slice())?;
    } else {
        io.publish_atomic(key_str, encoded.as_slice())?;
    }
    io.prune_to_cap(max_entries, key_str)?;
    Ok(true)
}

/// Production [`InodeRecordIo`] backed by a caller-provided capability `bstd::fs::Dir<ReadWrite>`
/// (zero ambient paths, zero in-process state — relies purely on the kernel page cache, Rule 51).
pub struct DirInodeRecordIo<'a> {
    dir: &'a crate::fs::Dir<crate::fs::ReadWrite>,
    cx: &'a crate::cx::Cx,
}

impl<'a> DirInodeRecordIo<'a> {
    /// Creates a production on-disk inode record handle over `dir`, bounded by `cx`.
    pub fn new(dir: &'a crate::fs::Dir<crate::fs::ReadWrite>, cx: &'a crate::cx::Cx) -> Self {
        Self { dir, cx }
    }

    fn sub_cx(&self) -> Result<crate::cx::Cx, Fault> {
        let left = super::remaining_for_cx(self.cx)
            .map_err(|_| Fault::Host { call: "nix runtime closure (deadline/stopped)", code: 110 })?;
        let world = self.dir.world();
        let deadline = blaze_core::wait::Deadline::after(world.now(), left);
        Ok(self.cx.on_beside(blaze_own::Share::share(world), deadline))
    }
}

impl InodeRecordIo for DirInodeRecordIo<'_> {
    fn read_bounded(&self, key_hex: &str, max_bytes: usize) -> Result<RecordRead, Fault> {
        let rel = crate::path::Path::new(key_hex)
            .map_err(|_| Fault::Host { call: "elf inode record path", code: 22 })?;
        let sub = self.sub_cx()?;
        let file = match self.dir.open_read(&sub, &rel) {
            Ok(f) => f,
            Err(e) if e.kind == crate::fs::FsKind::NotFound => return Ok(RecordRead::Absent),
            Err(e) => return Err(super::fs_err_to_fault("open elf inode record", &e)),
        };
        let len_u64 = match file.len(&sub) {
            Ok(n) => n,
            Err(e) if e.kind == crate::fs::FsKind::NotFound => return Ok(RecordRead::Absent),
            Err(e) => return Err(super::fs_err_to_fault("stat elf inode record", &e)),
        };
        let max_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        if len_u64 > max_u64 {
            return Ok(RecordRead::Oversized);
        }
        let Ok(len) = usize::try_from(len_u64) else {
            return Ok(RecordRead::Oversized);
        };
        let Some(cap) = len.checked_add(1) else {
            return Ok(RecordRead::Oversized);
        };
        let mut buf = OwnVec::with_capacity_in(cap, SharedAlloc::global());
        buf.resize(cap, 0u8);
        let mut filled = 0usize;
        while filled < cap {
            let step_off = u64::try_from(filled)
                .map_err(|_| Fault::Host { call: "read elf inode record overflow", code: 75 })?;
            let Some(dst) = buf.as_mut_slice().get_mut(filled..) else {
                break;
            };
            let n = match file.read_into(dst, step_off).wait(&sub) {
                Ok(n) => n,
                Err(e) if e.kind == crate::fs::FsKind::NotFound => return Ok(RecordRead::Absent),
                Err(e) => return Err(super::fs_err_to_fault("read elf inode record", &e)),
            };
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled > len {
            return Ok(RecordRead::Oversized);
        }
        buf.truncate(filled);
        Ok(RecordRead::Bytes(buf))
    }

    fn publish_atomic(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault> {
        let rel = crate::path::Path::new(key_hex)
            .map_err(|_| Fault::Host { call: "elf inode record path", code: 22 })?;
        let sub = self.sub_cx()?;
        // `Durable::Yes` flushes the sibling temp file before atomic rename and syncs the
        // directory so even a sudden power cut never leaves a torn record at the final path.
        self.dir
            .publish(&sub, &rel, crate::fs::Content::bytes(bytes), crate::fs::Durable::Yes)
            .map(|_| ())
            .map_err(|e| super::fs_err_to_fault("publish elf inode record", &e))
    }

    fn write_direct_planted(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault> {
        // Non-atomic direct write in two chunks (invoked ONLY by `InodeRecordPlant::DirectWrite`
        // DST controls so simulated power cuts leave a genuinely torn file on disk).
        let rel = crate::path::Path::new(key_hex)
            .map_err(|_| Fault::Host { call: "elf inode record path", code: 22 })?;
        let sub = self.sub_cx()?;
        let file = self
            .dir
            .create(&sub, &rel)
            .map_err(|e| super::fs_err_to_fault("create elf inode record (planted)", &e))?;
        let mid = bytes.len() / 2;
        let first = bytes.get(..mid).unwrap_or(&[]);
        let second = bytes.get(mid..).unwrap_or(&[]);
        let appended = file
            .append(&sub, first, crate::fs::Durable::No)
            .and_then(|()| file.append(&sub, second, crate::fs::Durable::No));
        let closed = file.close(&sub, crate::fs::Durable::No).map(|_| ());
        appended
            .and(closed)
            .map_err(|e| super::fs_err_to_fault("direct write elf inode record (planted)", &e))
    }

    fn prune_to_cap(&self, max_entries: usize, keep_key: &str) -> Result<(), Fault> {
        let sub = self.sub_cx()?;
        let entries = match self.dir.own_entries(&sub) {
            Ok(e) => e,
            Err(e) if e.kind == crate::fs::FsKind::NotFound => return Ok(()),
            Err(e) => return Err(super::fs_err_to_fault("list elf inode records for prune", &e)),
        };
        let scan_len = entries.len().min(PRUNE_SCAN_MOST);
        let mut live_count = 0usize;
        for i in 0..scan_len {
            if entries.kind(i) == crate::fs::EntryKind::File && !entries.name(i).ends_with(".bstd-new") {
                live_count += 1;
            }
        }
        if live_count <= max_entries {
            return Ok(());
        }
        let target = ((max_entries.saturating_mul(3)) / 4).max(1).min(max_entries);
        let shared = SharedAlloc::global();
        let mut victims: OwnVec<(u64, crate::path::Path), SharedAlloc<'static>> =
            OwnVec::with_capacity_in(live_count, shared);
        for i in 0..scan_len {
            if entries.kind(i) != crate::fs::EntryKind::File {
                continue;
            }
            let name = entries.name(i);
            if name == keep_key || name.ends_with(".bstd-new") {
                continue;
            }
            let Ok(path) = entries.path(i) else {
                continue;
            };
            let mtime_ns = match self.dir.stat(&sub, &path) {
                Ok(Some(meta)) => meta.stamp.modified_ns,
                Ok(None) => continue,
                Err(e) if e.kind == crate::fs::FsKind::NotFound => continue,
                Err(_) => 0,
            };
            victims.push((mtime_ns, path));
        }
        victims.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        let to_remove = live_count.saturating_sub(target);
        for (_, path) in victims.iter().take(to_remove) {
            match self.dir.remove(&sub, path, crate::fs::Durable::No) {
                Ok(()) => {}
                Err(e) if e.kind == crate::fs::FsKind::NotFound => {}
                Err(e) => return Err(super::fs_err_to_fault("prune elf inode record", &e)),
            }
        }
        Ok(())
    }
}

fn read_u16(buf: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    let arr: [u8; 2] = buf.get(at..end)?.try_into().ok()?;
    Some(u16::from_le_bytes(arr))
}

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let arr: [u8; 4] = buf.get(at..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

fn read_u64(buf: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    let arr: [u8; 8] = buf.get(at..end)?.try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

fn read_i64(buf: &[u8], at: usize) -> Option<i64> {
    let end = at.checked_add(8)?;
    let arr: [u8; 8] = buf.get(at..end)?.try_into().ok()?;
    Some(i64::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_own::{SharedAlloc, Vec as OwnVec};

    use super::{
        DirInodeRecordIo, ELF_INODE_RECORD_SUBDIR, InodeRecord, InodeRecordPlant, InodeStamp,
        MAX_INODE_RECORD_BYTES, RecordCorrupt, decode_record_with_parser, encode_record,
        encode_record_with_parser, lookup_inode_record, parser_identity, publish_inode_record,
        publish_inode_record_capped,
    };
    use crate::cx::Cx;
    use crate::fs::{Content, Durable};
    use crate::io::Io;
    use crate::mem::TASK_FRAME;
    use crate::path::Path;
    use crate::procs::hermetic::Fault;
    use crate::test::{Given, Seeds, seeded};

    const KEYS: usize = 8;
    const NOW_SEC: i64 = 1_700_000_100;

    fn stamp_for(idx: usize) -> InodeStamp {
        InodeStamp {
            dev: 7,
            ino: 1000 + u64::try_from(idx).unwrap_or(0),
            mode: 0o100_644,
            size: 4096 + i64::try_from(idx).unwrap_or(0) * 512,
            mtime_sec: NOW_SEC - 3600 - i64::try_from(idx).unwrap_or(0) * 10,
            mtime_nsec: 123_456,
            ctime_sec: NOW_SEC - 3600 - i64::try_from(idx).unwrap_or(0) * 10,
            ctime_nsec: 123_456,
        }
    }

    fn record_for(idx: usize) -> InodeRecord {
        let st = stamp_for(idx);
        let shared = SharedAlloc::global();
        let mut interp = OwnVec::new_in(shared);
        interp.extend_from_slice(b"/lib/ld-linux-aarch64.so.1");
        let mut runpath = OwnVec::new_in(shared);
        runpath.extend_from_slice(b"/nix/store/demo-toolchain-closure/lib");
        // Make payload span > 1 KiB (multiple simulated disk sectors) so unsynced/direct writes
        // genuinely tear across power cuts in `bstd::test::seeded`!
        let mut needed = OwnVec::new_in(shared);
        for lib_i in 0..48usize {
            let name = std::format!("libblaze_sim_{idx}_{lib_i:02}.so.1\0");
            needed.extend_from_slice(name.as_bytes());
        }
        let tag = u8::try_from((idx * 17 + 5) & 0xff).unwrap_or(1);
        InodeRecord {
            stamp: st,
            digest: Some((u64::try_from(st.size).unwrap_or(0), [tag; 32])),
            e_machine: 183,
            has_dyn: true,
            interp,
            runpath: Some(runpath),
            rpath: None,
            needed_blob: needed,
        }
    }

    fn key_str(st: InodeStamp) -> std::string::String {
        let buf = st.record_key_hex();
        std::str::from_utf8(&buf).expect("hex utf8").to_owned()
    }

    #[test]
    fn parser_identity_rolls_record_key_and_rejects_cross_parser_seal() {
        let st = stamp_for(0);
        let rec = record_for(0);
        let pid_a = [0x11u8; 32];
        let pid_b = [0x22u8; 32];
        assert_ne!(
            st.record_key_hex_with_parser(&pid_a),
            st.record_key_hex_with_parser(&pid_b),
            "changing parser code identity must roll the on-disk record filename"
        );
        assert_ne!(
            parser_identity(),
            [0u8; 32],
            "parser_identity must be a non-zero content digest of landlock.rs + inode_record.rs"
        );
        let encoded_a = encode_record_with_parser(&rec, &pid_a).expect("encode with parser A");
        assert_eq!(
            decode_record_with_parser(encoded_a.as_slice(), st, &pid_a, InodeRecordPlant::None),
            Ok(rec)
        );
        assert_eq!(
            decode_record_with_parser(encoded_a.as_slice(), st, &pid_b, InodeRecordPlant::None),
            Err(RecordCorrupt::BadChecksum),
            "record sealed under parser A must fail verification under parser B"
        );
    }

    /// Production `DirInodeRecordIo` crash/fault/concurrency program driven by `bstd::test::seeded`
    /// (`blaze_core::sim::Sim`): exercises real `Dir::publish(Durable::Yes)` across power cuts,
    /// sync/write/rename faults, and two racing peer worlds sharing the simulated disk.
    fn dir_io_crash_program(io: &Io, plant: InodeRecordPlant) -> Result<(), Fault> {
        let root = io.need_fs().map_err(|_| Fault::Host { call: "sim fs", code: 5 })?.root();
        let sub_path = Path::new(ELF_INODE_RECORD_SUBDIR).map_err(|_| Fault::Host { call: "sub path", code: 22 })?;
        let rec_dir = root.subdir(&sub_path);
        let cx = io
            .cx(Duration::from_secs(30), TASK_FRAME)
            .map_err(|_| Fault::Host { call: "sim cx", code: 110 })?;
        let dir_io = DirInodeRecordIo::new(&rec_dir, &cx);

        // Post-crash oracle: every surviving record must be Absent (`Ok(None)`) or the exact
        // oracle record (`Ok(Some(want))`). A torn/corrupt record (`code == 74`) after a power cut
        // is a hard invariant violation!
        for k in 0..KEYS {
            let st = stamp_for(k);
            let want = record_for(k);
            match lookup_inode_record(&dir_io, st, NOW_SEC, true, plant) {
                Ok(None) => {}
                Ok(Some(got)) => {
                    assert_eq!(got, want, "inode record sim oracle: key {k} survived crash with mismatched record");
                }
                Err(Fault::Host { code: 74, call }) => {
                    panic!("inode record sim oracle: key {k} left torn/corrupt after power cut ({call})");
                }
                Err(other) => return Err(other),
            }
        }

        // Race two independent peer worlds (`peer_a`, `peer_b`, simulating concurrent commands
        // sharing the same on-disk `elf-inodes-v1` dir) publishing overlapping & distinct keys.
        let peer_a = io
            .peer_world_for_test(Some(0xaaaa_1111_0000_0001))
            .ok_or(Fault::Host { call: "peer a", code: 5 })?;
        let peer_b = io
            .peer_world_for_test(Some(0xbbbb_2222_0000_0002))
            .ok_or(Fault::Host { call: "peer b", code: 5 })?;
        let cx_a = peer_a
            .cx(Duration::from_secs(30), TASK_FRAME)
            .map_err(|_| Fault::Host { call: "cx a", code: 110 })?;
        let cx_b = peer_b
            .cx(Duration::from_secs(30), TASK_FRAME)
            .map_err(|_| Fault::Host { call: "cx b", code: 110 })?;
        let dir_a = peer_a.need_fs().map_err(|_| Fault::Host { call: "fs a", code: 5 })?.root().subdir(&sub_path);
        let dir_b = peer_b.need_fs().map_err(|_| Fault::Host { call: "fs b", code: 5 })?.root().subdir(&sub_path);
        let io_a = DirInodeRecordIo::new(&dir_a, &cx_a);
        let io_b = DirInodeRecordIo::new(&dir_b, &cx_b);

        for k in 0..KEYS {
            let rec = record_for(k);
            if k % 2 == 0 {
                let _ = publish_inode_record(&io_a, &rec, NOW_SEC, plant)?;
                let _ = publish_inode_record(&io_b, &rec, NOW_SEC, plant)?;
            } else {
                let _ = publish_inode_record(&io_b, &rec, NOW_SEC, plant)?;
                let _ = publish_inode_record(&io_a, &rec, NOW_SEC, plant)?;
            }
            let got_a = lookup_inode_record(&io_a, rec.stamp, NOW_SEC, true, plant)?;
            let got_b = lookup_inode_record(&io_b, rec.stamp, NOW_SEC, true, plant)?;
            assert_eq!(got_a.as_ref(), Some(&rec), "inode record sim oracle: peer A read mismatch on key {k}");
            assert_eq!(got_b.as_ref(), Some(&rec), "inode record sim oracle: peer B read mismatch on key {k}");
        }
        Ok(())
    }

    #[test]
    fn dir_inode_record_io_survives_sim_crashes_and_concurrent_peers() {
        match seeded(
            "blaze-std",
            module_path!(),
            "dir_inode_record_crash",
            Seeds::DEFAULT.from(0, 200),
            Given::NOTHING,
            |_: &Cx, io: Io| {
                dir_io_crash_program(&io, InodeRecordPlant::None).map_err(|e| std::format!("{e:?}"))
            },
        ) {
            Ok(passed) => {
                assert!(passed.counts.cuts > 50, "too few power cuts: {:?}", passed.counts);
                assert!(passed.counts.failures > 50, "too few failures: {:?}", passed.counts);
            }
            Err(failed) => panic!("{failed}"),
        }
    }

    #[test]
    fn dir_inode_record_io_sim_catches_planted_direct_write() {
        match seeded(
            "blaze-std",
            module_path!(),
            "dir_inode_record_direct_write_plant",
            Seeds::DEFAULT.from(0, 100),
            Given::NOTHING,
            |_: &Cx, io: Io| {
                dir_io_crash_program(&io, InodeRecordPlant::DirectWrite).map_err(|e| std::format!("{e:?}"))
            },
        ) {
            Ok(passed) => panic!("planted DirectWrite was not caught in {} starts", passed.starts),
            Err(failed) => {
                let said = std::format!("{failed}");
                assert!(
                    said.contains("inode record sim oracle"),
                    "planted DirectWrite failed for unexpected reason: {said}"
                );
            }
        }
    }

    fn run_dir_io_validation_and_cap_sim(plant: InodeRecordPlant) -> Result<(), std::string::String> {
        let io = Io::sim(42);
        let root = io.need_fs().map_err(|_| "no fs")?.root();
        let sub_path = Path::new(ELF_INODE_RECORD_SUBDIR).map_err(|_| "sub")?;
        let rec_dir = root.subdir(&sub_path);
        let cx = io.cx(Duration::from_secs(30), TASK_FRAME).map_err(|_| "cx")?;
        let dir_io = DirInodeRecordIo::new(&rec_dir, &cx);

        // 1. Short-reads simulation on production `DirInodeRecordIo::read_bounded`.
        let rec0 = record_for(0);
        publish_inode_record(&dir_io, &rec0, NOW_SEC, plant).map_err(|e| std::format!("pub0: {e:?}"))?;
        let short_dir = blaze_core::FileName::new(ELF_INODE_RECORD_SUBDIR).ok_or("short dir")?;
        io.with_sim(|sim| sim.short_reads(short_dir, 1)).ok_or("expected sim")?;
        let got0 = lookup_inode_record(&dir_io, rec0.stamp, NOW_SEC, true, plant)
            .map_err(|e| std::format!("short read lookup: {e:?}"))?;
        if got0.as_ref() != Some(&rec0) {
            return Err("short-read oracle: failed to read full record under Sim::short_reads".into());
        }

        // 2. Corrupt-on-disk records MUST return typed EBADMSG (`code: 74`), NEVER `Ok(None)`!
        let k0 = Path::new(&key_str(rec0.stamp)).map_err(|_| "k0")?;
        let raw0 = encode_record(&rec0).ok_or("encode0")?;
        // 2a. Payload bit-flip -> BadChecksum (catches `SkipChecksum`!).
        let mut flipped = raw0.as_slice().to_vec();
        flipped[85] ^= 0xff;
        rec_dir.publish(&cx, &k0, Content::bytes(&flipped), Durable::No).map_err(|_| "write flip")?;
        match lookup_inode_record(&dir_io, rec0.stamp, NOW_SEC, true, plant) {
            Err(Fault::Host { code: 74, .. }) => {}
            other => return Err(std::format!("corrupt checksum oracle: expected EBADMSG, got {other:?}")),
        }
        // 2b. Truncated file -> Truncated.
        rec_dir.publish(&cx, &k0, Content::bytes(b"BZELFIN1"), Durable::No).map_err(|_| "write trunc")?;
        match lookup_inode_record(&dir_io, rec0.stamp, NOW_SEC, true, plant) {
            Err(Fault::Host { code: 74, .. }) => {}
            other => return Err(std::format!("corrupt truncated oracle: expected EBADMSG, got {other:?}")),
        }
        // 2c. Oversized file (> 64 KiB) -> Oversized.
        let big = std::vec![0u8; MAX_INODE_RECORD_BYTES + 32];
        rec_dir.publish(&cx, &k0, Content::bytes(&big), Durable::No).map_err(|_| "write big")?;
        match lookup_inode_record(&dir_io, rec0.stamp, NOW_SEC, true, plant) {
            Err(Fault::Host { code: 74, .. }) => {}
            other => return Err(std::format!("corrupt oversized oracle: expected EBADMSG, got {other:?}")),
        }
        // 2d. Wrong-stamp valid record placed under key 0 -> StampMismatch (catches `SkipStampCheck`!).
        let rec1 = record_for(1);
        let raw1 = encode_record(&rec1).ok_or("encode1")?;
        rec_dir.publish(&cx, &k0, Content::bytes(raw1.as_slice()), Durable::No).map_err(|_| "write swap")?;
        match lookup_inode_record(&dir_io, rec0.stamp, NOW_SEC, true, plant) {
            Err(Fault::Host { code: 74, .. }) => {}
            other => return Err(std::format!("stamp mismatch oracle: expected EBADMSG, got {other:?}")),
        }
        // Clean up corrupted key 0.
        rec_dir.remove(&cx, &k0, Durable::No).map_err(|_| "rm k0")?;

        // 3. Racy-clean guard (catches `SkipRacyClean`!).
        let racy_st = InodeStamp { ctime_sec: NOW_SEC - 1, ..stamp_for(2) };
        let racy_rec = InodeRecord { stamp: racy_st, ..record_for(2) };
        let published = publish_inode_record(&dir_io, &racy_rec, NOW_SEC, plant)
            .map_err(|e| std::format!("racy pub: {e:?}"))?;
        if published {
            return Err("racy-clean oracle: published record within 2s racy-clean window".into());
        }

        // 4. Write-side bounded directory cap (`max_entries = 4` across 10 distinct stamps).
        let clock = io.need_clock().map_err(|_| "clock")?;
        for idx in 10..20usize {
            let rec = record_for(idx);
            let _ = publish_inode_record_capped(&dir_io, &rec, NOW_SEC, 4, plant)
                .map_err(|e| std::format!("cap pub {idx}: {e:?}"))?;
            let _ = clock.sleep(&cx, Duration::from_millis(5));
        }
        let entries = rec_dir.own_entries(&cx).map_err(|_| "list after cap")?;
        let files = (0..entries.len())
            .filter(|&i| entries.kind(i) == crate::fs::EntryKind::File && !entries.name(i).ends_with(".bstd-new"))
            .count();
        if files > 4 {
            return Err(std::format!("bounded cap oracle: expected <= 4 files, found {files}"));
        }
        Ok(())
    }

    #[test]
    fn dir_inode_record_io_reports_corruption_enforces_cap_and_catches_planted_bugs() {
        run_dir_io_validation_and_cap_sim(InodeRecordPlant::None)
            .expect("clean DirInodeRecordIo validation, short-reads, corruption, and cap sim must pass");

        let e_ck = run_dir_io_validation_and_cap_sim(InodeRecordPlant::SkipChecksum)
            .expect_err("must catch SkipChecksum");
        assert!(e_ck.contains("corrupt checksum oracle"), "unexpected: {e_ck}");

        let e_rc = run_dir_io_validation_and_cap_sim(InodeRecordPlant::SkipRacyClean)
            .expect_err("must catch SkipRacyClean");
        assert!(e_rc.contains("racy-clean oracle"), "unexpected: {e_rc}");

        let e_st = run_dir_io_validation_and_cap_sim(InodeRecordPlant::SkipStampCheck)
            .expect_err("must catch SkipStampCheck");
        assert!(e_st.contains("stamp mismatch oracle"), "unexpected: {e_st}");
    }
}
