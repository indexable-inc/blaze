//! Immutable write-once on-disk ELF inode records (AGENTS.md Rule 51: no process-memory cache;
//! rely on the kernel page cache for fast reads across tasks and commands).
//!
//! Each record caches the pure-content facts of one regular file inode (its ELF64 header/dynamic
//! section metadata and optionally its full-file SHA-256 + length), keyed strictly by its live
//! `fstat(fd)` [`InodeStamp`] and guarded by a 2-second racy-clean window so same-tick in-place
//! writes never read or write stale records.
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
//! - `[N-32..N]` trailing 32-byte BLAKE3 self-checksum over `RECORD_SEAL_DOMAIN || bytes[..N-32]`.

use blaze_own::{SharedAlloc, Vec as OwnVec};

use super::Fault;

/// Subdirectory under `<home>/.blaze/v2` holding immutable write-once ELF inode records.
pub const ELF_INODE_RECORD_SUBDIR: &str = "build-script/records/elf-inodes-v1";

/// Maximum serialized ELF inode record size accepted on read or write (64 KiB).
/// Checked via file metadata BEFORE allocating a buffer.
pub const MAX_INODE_RECORD_BYTES: usize = 64 << 10;

const MAGIC: [u8; 8] = *b"BZELFIN1";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 131;
const CHECKSUM_LEN: usize = 32;
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
    /// `blake3("blaze-elf-inode-record-v1\0" || stamp_le_bytes)`.
    pub fn record_key_hex(&self) -> [u8; 64] {
        let mut h = crate::digest::Blake3::new();
        h.update(RECORD_KEY_DOMAIN);
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
    /// Control 3: writes records directly to the final path instead of atomic temp+rename.
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
/// by [`DirInodeRecordIo`] over `bstd::fs::Dir<ReadWrite>` and in DSTs by `SimRecordIo`.
pub trait InodeRecordIo {
    /// Reads `key_hex` (64 ASCII hex chars), capping size at `max_bytes` BEFORE allocating.
    fn read_bounded(&self, key_hex: &str, max_bytes: usize) -> Result<RecordRead, Fault>;

    /// Publishes `bytes` atomically at `key_hex` (unique sibling temp file + rename, cleaning up
    /// temp on failure).
    fn publish_atomic(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault>;

    /// Non-atomic direct write at `key_hex` (invoked ONLY by [`InodeRecordPlant::DirectWrite`]
    /// in DST controls).
    fn write_direct_planted(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault>;
}

/// Encodes `record` into canonical self-checksummed bytes (`BZELFIN1` + trailing 32-byte BLAKE3).
/// Returns `None` if the encoded record would exceed [`MAX_INODE_RECORD_BYTES`].
pub fn encode_record(record: &InodeRecord) -> Option<OwnVec<u8, SharedAlloc<'static>>> {
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
    seal.update(out.as_slice());
    let checksum = seal.finalize();
    out.extend_from_slice(&checksum);
    Some(out)
}

/// Verifies and decodes a serialized ELF inode record against `expected_stamp`.
/// Returns `None` on any size overflow, checksum failure, magic/version mismatch, stamp mismatch,
/// invalid flags, or trailing bytes.
pub fn decode_record(
    bytes: &[u8],
    expected_stamp: InodeStamp,
    plant: InodeRecordPlant,
) -> Option<InodeRecord> {
    if bytes.len() > MAX_INODE_RECORD_BYTES || bytes.len() < HEADER_LEN + CHECKSUM_LEN {
        return None;
    }
    let body_len = bytes.len().checked_sub(CHECKSUM_LEN)?;
    let (body, trailing) = bytes.split_at_checked(body_len)?;
    if plant != InodeRecordPlant::SkipChecksum {
        let mut seal = crate::digest::Blake3::new();
        seal.update(RECORD_SEAL_DOMAIN);
        seal.update(body);
        if seal.finalize().as_slice() != trailing {
            return None;
        }
    }
    if body.get(..8) != Some(MAGIC.as_slice()) {
        return None;
    }
    if read_u32(body, 8)? != VERSION {
        return None;
    }
    let stamp = InodeStamp {
        dev: read_u64(body, 12)?,
        ino: read_u64(body, 20)?,
        mode: read_u32(body, 28)?,
        size: read_i64(body, 32)?,
        mtime_sec: read_i64(body, 40)?,
        mtime_nsec: read_i64(body, 48)?,
        ctime_sec: read_i64(body, 56)?,
        ctime_nsec: read_i64(body, 64)?,
    };
    if plant != InodeRecordPlant::SkipStampCheck && stamp != expected_stamp {
        return None;
    }
    let flags = *body.get(72)?;
    if (flags & !VALID_FLAGS_MASK) != 0 {
        return None;
    }
    let e_machine = read_u16(body, 73)?;
    let file_len = read_u64(body, 75)?;
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(body.get(83..115)?);
    let digest = if (flags & FLAG_HAS_DIGEST) != 0 {
        Some((file_len, sha256))
    } else {
        None
    };
    let has_dyn = (flags & FLAG_HAS_DYN) != 0;
    let has_runpath = (flags & FLAG_HAS_RUNPATH) != 0;
    let has_rpath = (flags & FLAG_HAS_RPATH) != 0;

    let interp_len = usize::try_from(read_u32(body, 115)?).ok()?;
    let runpath_len = usize::try_from(read_u32(body, 119)?).ok()?;
    let rpath_len = usize::try_from(read_u32(body, 123)?).ok()?;
    let needed_len = usize::try_from(read_u32(body, 127)?).ok()?;

    if interp_len > MAX_INTERP_LEN
        || runpath_len > MAX_RPATH_LEN
        || rpath_len > MAX_RPATH_LEN
        || needed_len > MAX_NEEDED_BLOB_LEN
        || (!has_runpath && runpath_len != 0)
        || (!has_rpath && rpath_len != 0)
    {
        return None;
    }
    let end_interp = HEADER_LEN.checked_add(interp_len)?;
    let end_runpath = end_interp.checked_add(runpath_len)?;
    let end_rpath = end_runpath.checked_add(rpath_len)?;
    let end_needed = end_rpath.checked_add(needed_len)?;
    if end_needed != body.len() {
        return None;
    }

    let shared = SharedAlloc::global();
    let mut interp = OwnVec::with_capacity_in(interp_len, shared);
    interp.extend_from_slice(body.get(HEADER_LEN..end_interp)?);
    let runpath = if has_runpath {
        let mut v = OwnVec::with_capacity_in(runpath_len, shared);
        v.extend_from_slice(body.get(end_interp..end_runpath)?);
        Some(v)
    } else {
        None
    };
    let rpath = if has_rpath {
        let mut v = OwnVec::with_capacity_in(rpath_len, shared);
        v.extend_from_slice(body.get(end_runpath..end_rpath)?);
        Some(v)
    } else {
        None
    };
    let mut needed_blob = OwnVec::with_capacity_in(needed_len, shared);
    needed_blob.extend_from_slice(body.get(end_rpath..end_needed)?);

    Some(InodeRecord { stamp, digest, e_machine, has_dyn, interp, runpath, rpath, needed_blob })
}

/// Looks up an immutable on-disk ELF inode record for `stamp` at wall-clock second `now_sec`.
/// Returns `Ok(None)` on racy-clean timestamps, missing/oversized/corrupt records, or when
/// `need_digest` is true and the record holds ELF metadata only.
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
        return Ok(None);
    };
    match io.read_bounded(key_str, MAX_INODE_RECORD_BYTES)? {
        RecordRead::Absent | RecordRead::Oversized => Ok(None),
        RecordRead::Bytes(raw) => {
            let Some(rec) = decode_record(raw.as_slice(), stamp, plant) else {
                return Ok(None);
            };
            if need_digest && rec.digest.is_none() {
                return Ok(None);
            }
            Ok(Some(rec))
        }
    }
}

/// Publishes an immutable on-disk ELF inode record if `record.stamp` is outside the racy-clean
/// window and fits within [`MAX_INODE_RECORD_BYTES`]. Returns `Ok(true)` if published.
pub fn publish_inode_record(
    io: &dyn InodeRecordIo,
    record: &InodeRecord,
    now_sec: i64,
    plant: InodeRecordPlant,
) -> Result<bool, Fault> {
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
    Ok(true)
}

/// Production [`InodeRecordIo`] backed by a caller-provided capability `bstd::fs::Dir<ReadWrite>`
/// (zero ambient paths, zero in-process state — relies purely on the kernel page cache, Rule 51).
#[cfg(target_os = "linux")]
pub(super) struct DirInodeRecordIo<'a> {
    dir: &'a crate::fs::Dir<crate::fs::ReadWrite>,
    cx: &'a crate::cx::Cx,
}

#[cfg(target_os = "linux")]
impl<'a> DirInodeRecordIo<'a> {
    pub(super) fn new(dir: &'a crate::fs::Dir<crate::fs::ReadWrite>, cx: &'a crate::cx::Cx) -> Self {
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

#[cfg(target_os = "linux")]
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
        self.dir
            .publish(&sub, &rel, crate::fs::Content::bytes(bytes), crate::fs::Durable::No)
            .map(|_| ())
            .map_err(|e| super::fs_err_to_fault("publish elf inode record", &e))
    }

    fn write_direct_planted(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault> {
        // Production always uses atomic publish; direct write exists only for DST controls.
        self.publish_atomic(key_hex, bytes)
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
    use super::{
        InodeRecord, InodeRecordIo, InodeRecordPlant, InodeStamp, MAX_INODE_RECORD_BYTES,
        RecordRead, decode_record, encode_record, lookup_inode_record, publish_inode_record,
    };
    use crate::procs::hermetic::Fault;
    use blaze_own::{SharedAlloc, Vec as OwnVec};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::string::String;
    use std::vec::Vec;

    fn make_record(stamp: InodeStamp, seed: u8, with_digest: bool) -> InodeRecord {
        let shared = SharedAlloc::global();
        let mut interp = OwnVec::new_in(shared);
        interp.extend_from_slice(b"/lib/ld-linux-aarch64.so.1");
        let mut runpath = OwnVec::new_in(shared);
        runpath.extend_from_slice(b"/nix/store/demo/lib");
        let mut needed = OwnVec::new_in(shared);
        needed.extend_from_slice(b"libc.so.6\0libm.so.6\0");
        InodeRecord {
            stamp,
            digest: with_digest.then_some((u64::try_from(stamp.size).unwrap_or(0), [seed; 32])),
            e_machine: 183,
            has_dyn: true,
            interp,
            runpath: Some(runpath),
            rpath: None,
            needed_blob: needed,
        }
    }

    struct SimFsState {
        files: BTreeMap<String, Vec<u8>>,
        orphaned_temps: usize,
        fail_next_publish: bool,
        fail_next_read: bool,
    }

    struct SimRecordIo {
        state: RefCell<SimFsState>,
    }

    impl SimRecordIo {
        fn new() -> Self {
            Self {
                state: RefCell::new(SimFsState {
                    files: BTreeMap::new(),
                    orphaned_temps: 0,
                    fail_next_publish: false,
                    fail_next_read: false,
                }),
            }
        }
    }

    impl InodeRecordIo for SimRecordIo {
        fn read_bounded(&self, key_hex: &str, max_bytes: usize) -> Result<RecordRead, Fault> {
            let mut st = self.state.borrow_mut();
            if st.fail_next_read {
                st.fail_next_read = false;
                return Err(Fault::Host { call: "sim read EIO", code: 5 });
            }
            let Some(bytes) = st.files.get(key_hex) else {
                return Ok(RecordRead::Absent);
            };
            if bytes.len() > max_bytes {
                return Ok(RecordRead::Oversized);
            }
            let mut out = OwnVec::with_capacity_in(bytes.len(), SharedAlloc::global());
            out.extend_from_slice(bytes);
            Ok(RecordRead::Bytes(out))
        }

        fn publish_atomic(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault> {
            let mut st = self.state.borrow_mut();
            if st.fail_next_publish {
                // Mid-write crash: temp file orphaned, final path completely untouched!
                st.fail_next_publish = false;
                st.orphaned_temps += 1;
                return Err(Fault::Host { call: "sim crash mid-atomic-publish", code: 5 });
            }
            st.files.insert(String::from(key_hex), bytes.to_vec());
            Ok(())
        }

        fn write_direct_planted(&self, key_hex: &str, bytes: &[u8]) -> Result<(), Fault> {
            let mut st = self.state.borrow_mut();
            if st.fail_next_publish {
                // Mid-write crash during non-atomic direct write leaves a torn prefix at final path!
                st.fail_next_publish = false;
                let half = bytes.len() / 2;
                st.files.insert(String::from(key_hex), bytes[..half].to_vec());
                return Err(Fault::Host { call: "sim crash mid-direct-write", code: 5 });
            }
            st.files.insert(String::from(key_hex), bytes.to_vec());
            Ok(())
        }
    }

    fn run_inode_record_dst(plant: InodeRecordPlant) -> Result<u64, &'static str> {
        let mut combined_trace = 0u64;
        for seed in 0u64..150 {
            let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(11);
            let mut next = || {
                rng = rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = rng;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            };

            let io = SimRecordIo::new();
            let now_sec: i64 = 1_700_000_100;
            let mut stamps: Vec<(InodeStamp, InodeRecord)> = Vec::new();
            for i in 0u8..8 {
                let st = InodeStamp {
                    dev: 1,
                    ino: 100 + u64::from(i),
                    mode: 0o100_644,
                    size: 4096 + i64::from(i) * 128,
                    mtime_sec: 1,
                    mtime_nsec: 0,
                    ctime_sec: now_sec - 10,
                    ctime_nsec: i64::from(i),
                };
                stamps.push((st, make_record(st, i.wrapping_add(1), true)));
            }

            // Track keys with intentionally injected external corruption vs clean atomic publishes.
            let mut externally_corrupted: BTreeMap<String, bool> = BTreeMap::new();

            for _step in 0..80 {
                let idx = usize::try_from(next()).unwrap_or(0) % stamps.len();
                let (st, expected) = &stamps[idx];
                let key_bytes = st.record_key_hex();
                let key_str = core::str::from_utf8(&key_bytes).expect("hex utf8");
                let op = next() % 8;
                match op {
                    0 | 1 => {
                        // Lookup and verify oracle agreement.
                        if let Ok(Some(got)) = lookup_inode_record(&io, *st, now_sec, true, plant) {
                            if got != *expected {
                                return Err("oracle mismatch: lookup returned stale or corrupted record");
                            }
                            combined_trace = combined_trace.rotate_left(5) ^ u64::from(got.e_machine);
                        }
                    }
                    2 | 3 => {
                        // Publish (possibly with simulated mid-write crash).
                        let crash = next() % 4 == 0;
                        io.state.borrow_mut().fail_next_publish = crash;
                        let res = publish_inode_record(&io, expected, now_sec, plant);
                        if crash {
                            if res.is_ok() {
                                return Err("simulated crash did not return Err");
                            }
                            // Atomic publish invariant: a mid-write crash MUST NOT leave a torn
                            // file at the final path!
                            if !externally_corrupted.get(key_str).copied().unwrap_or(false)
                                && let Some(raw) = io.state.borrow().files.get(key_str)
                                && decode_record(raw, *st, InodeRecordPlant::None).is_none()
                            {
                                return Err("non-atomic crash left torn record at final path");
                            }
                        } else {
                            let _ = res.map_err(|_| "unexpected publish failure")?;
                            externally_corrupted.insert(String::from(key_str), false);
                            let got = lookup_inode_record(&io, *st, now_sec, true, plant)
                                .map_err(|_| "lookup after publish failed")?;
                            if got.as_ref() != Some(expected) {
                                return Err("warm lookup after publish disagreed with oracle");
                            }
                        }
                    }
                    4 => {
                        // Corrupt one byte in the payload (not header magic/version) to test checksum!
                        let mut fs = io.state.borrow_mut();
                        if let Some(raw) = fs.files.get_mut(key_str)
                            && raw.len() > 100
                        {
                            // Flip a byte inside sha256 (offset 85) so magic/version/stamp still look valid!
                            raw[85] ^= 0xff;
                            externally_corrupted.insert(String::from(key_str), true);
                        }
                        drop(fs);
                        if let Ok(Some(got)) = lookup_inode_record(&io, *st, now_sec, true, plant)
                            && got != *expected
                        {
                            return Err("corrupted checksum was accepted by lookup");
                        }
                    }
                    5 => {
                        // Swap another stamp's valid self-checksummed record into this key's file!
                        let other_idx = (idx + 1) % stamps.len();
                        let other_bytes = encode_record(&stamps[other_idx].1).expect("encode other");
                        io.state.borrow_mut().files.insert(String::from(key_str), other_bytes.as_slice().to_vec());
                        externally_corrupted.insert(String::from(key_str), true);
                        if let Ok(Some(got)) = lookup_inode_record(&io, *st, now_sec, true, plant)
                            && got != *expected
                        {
                            return Err("mismatched stamp record was accepted by lookup");
                        }
                    }
                    6 => {
                        // Racy-clean test: file touched <2s ago, published with version A, then
                        // overwritten in-place at the exact same coarse timestamp tick with version B.
                        let racy_st = InodeStamp { ctime_sec: now_sec - 1, ..*st };
                        let rec_a = make_record(racy_st, 0x11, true);
                        let rec_b = make_record(racy_st, 0x22, true);
                        let _ = publish_inode_record(&io, &rec_a, now_sec, plant);
                        if let Ok(Some(got)) = lookup_inode_record(&io, racy_st, now_sec, true, plant)
                            && got != rec_b
                        {
                            return Err("racy-clean file returned stale cached record after same-tick mutation");
                        }
                    }
                    _ => {
                        // Oversized file + transient read I/O error.
                        io.state.borrow_mut().files.insert(String::from(key_str), vec![0u8; MAX_INODE_RECORD_BYTES + 16]);
                        externally_corrupted.insert(String::from(key_str), true);
                        let got = lookup_inode_record(&io, *st, now_sec, true, plant)
                            .map_err(|_| "oversized file should miss cleanly")?;
                        if got.is_some() {
                            return Err("oversized record was not rejected");
                        }
                        io.state.borrow_mut().fail_next_read = true;
                        if lookup_inode_record(&io, *st, now_sec, true, plant).is_ok() {
                            return Err("read I/O error was swallowed instead of propagated");
                        }
                    }
                }
            }
        }
        Ok(combined_trace)
    }

    #[test]
    fn inode_record_dst_passes_and_replays_deterministically() {
        let a = run_inode_record_dst(InodeRecordPlant::None).expect("clean DST must pass");
        let b = run_inode_record_dst(InodeRecordPlant::None).expect("second DST run must pass");
        assert_eq!(a, b, "DST must replay bit-for-bit identically");
    }

    #[test]
    fn inode_record_dst_catches_all_four_planted_bugs() {
        let e1 = run_inode_record_dst(InodeRecordPlant::SkipChecksum).expect_err("must catch SkipChecksum");
        assert!(e1.contains("checksum"), "unexpected: {e1}");
        let e2 = run_inode_record_dst(InodeRecordPlant::SkipRacyClean).expect_err("must catch SkipRacyClean");
        assert!(e2.contains("racy-clean"), "unexpected: {e2}");
        let e3 = run_inode_record_dst(InodeRecordPlant::DirectWrite).expect_err("must catch DirectWrite");
        assert!(e3.contains("torn"), "unexpected: {e3}");
        let e4 = run_inode_record_dst(InodeRecordPlant::SkipStampCheck).expect_err("must catch SkipStampCheck");
        assert!(e4.contains("stamp"), "unexpected: {e4}");
    }
}
