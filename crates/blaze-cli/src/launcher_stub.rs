//! Shared self-describing `blaze-launcher` binary stub patching, Mach-O ad-hoc re-signing,
//! and `mode=jit-exec` manifest codec (Rule 30: one concept, one home).
//!
//! Used by both:
//! - `test_bins.rs` (`CARGO_BIN_EXE_*` launcher stubs for `blaze test`), and
//! - `engine_export.rs` + `jit_exec.rs` (default JIT-on-call `blaze build` executables).

use core::fmt::{self, Write as _};

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Capacity of the in-binary stub configuration slot in `blaze-launcher` (16 KiB, 4 KiB-aligned
/// so macOS Mach-O ad-hoc page re-hashing touches at most 4 code pages).
pub(crate) const STUB_SLOT_SIZE: usize = 16 * 1024;
pub(crate) const STUB_SLOT_HEADER: usize = 36; // 16-byte magic + 16-byte state tag + 4-byte LE length
pub(crate) const STUB_SLOT_MAGIC: &[u8; 16] = b"BZSTUB_SLOT_0001";
pub(crate) const STUB_SLOT_UNPATCHED: &[u8; 16] = b"UNPATCHED_CFG_01";
pub(crate) const STUB_SLOT_PATCHED: &[u8; 16] = b"PATCHED_STUB_001";

/// Encodes `bytes` as lowercase hex onto `out`.
pub(crate) fn encode_hex(bytes: &[u8], out: &mut AString<'_>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in bytes {
        if let (Some(&hi), Some(&lo)) = (HEX.get(usize::from(b >> 4)), HEX.get(usize::from(b & 0x0f))) {
            out.push(char::from(hi));
            out.push(char::from(lo));
        }
    }
}

/// Decodes an even-length lowercase/uppercase hex string into `out`.
pub(crate) fn decode_hex_into(hex: &str, out: &mut AVec<'_, u8>) -> Result<(), &'static str> {
    let digits = hex.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err("hex string has odd length");
    }
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        b'A'..=b'F' => Some(it - b'A' + 10),
        _ => None,
    };
    out.reserve(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        let high = digit(pair[0]).ok_or("invalid hex digit")?;
        let low = digit(pair[1]).ok_or("invalid hex digit")?;
        out.push((high << 4) | low);
    }
    Ok(())
}

/// Decodes a 64-character hex string into a fixed 32-byte digest.
pub(crate) fn decode_hex32(hex: &str) -> Result<[u8; 32], &'static str> {
    if hex.len() != 64 {
        return Err("expected 64 hex digits");
    }
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        b'A'..=b'F' => Some(it - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let high = digit(pair[0]).ok_or("invalid hex digit")?;
        let low = digit(pair[1]).ok_or("invalid hex digit")?;
        *slot = (high << 4) | low;
    }
    Ok(out)
}

/// Decodes a hex-encoded UTF-8 string into an allocated `AString`.
pub(crate) fn decode_hex_str<'a>(hex: &str, alloc: Alloc<'a>) -> Result<AString<'a>, &'static str> {
    let mut raw = Vec::new_in(alloc);
    decode_hex_into(hex, &mut raw)?;
    let text = core::str::from_utf8(raw.as_slice()).map_err(|_| "hex-decoded bytes are not valid UTF-8")?;
    Ok(String::from_str_in(text, alloc))
}

/// Why reading a launcher or built executable file via POSIX `openat` + `read` failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadFileError {
    InvalidPath,
    Os(blaze_sys::Errno),
}

impl fmt::Display for ReadFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadFileError::InvalidPath => write!(f, "path is too long or contains a NUL byte"),
            ReadFileError::Os(errno) => write!(f, "{errno}"),
        }
    }
}

/// Reads an entire file at `path` via safe `blaze_sys::posix::io` calls (`openat` + `read`),
/// retrying only `EINTR` (shared by `test_bins.rs` and `engine_export.rs`, Rule 30).
pub(crate) fn read_file_bytes<'a>(path: &str, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ReadFileError> {
    use blaze_sys::Errno;
    use blaze_sys::posix::io::{CWD, OpenFlags, openat, read};

    let bytes = path.as_bytes();
    let mut buf = [0u8; 4096];
    if bytes.len() >= buf.len() || bytes.contains(&0) {
        return Err(ReadFileError::InvalidPath);
    }
    if let Some(dst) = buf.get_mut(..bytes.len()) {
        dst.copy_from_slice(bytes);
    }
    let Some(with_nul) = buf.get(..=bytes.len()) else {
        return Err(ReadFileError::InvalidPath);
    };
    let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(with_nul) else {
        return Err(ReadFileError::InvalidPath);
    };
    let fd = openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
        .map_err(ReadFileError::Os)?;
    let mut out = Vec::new_in(alloc);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match read(fd.fd(), &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(slice) = chunk.get(..n) {
                    out.extend_from_slice(slice);
                }
            }
            Err(Errno::Intr) => continue,
            Err(errno) => return Err(ReadFileError::Os(errno)),
        }
    }
    Ok(out)
}

/// Locates the unique unpatched `EMBEDDED_STUB_SLOT` inside `blaze-launcher`'s binary bytes.
pub(crate) fn find_stub_slot(bytes: &[u8]) -> Result<usize, &'static str> {
    find_slot_with_tag(bytes, STUB_SLOT_UNPATCHED)
}

/// Locates a unique patched `EMBEDDED_STUB_SLOT` and returns its embedded payload slice.
pub(crate) fn read_patched_slot(bytes: &[u8]) -> Option<&[u8]> {
    let off = find_slot_with_tag(bytes, STUB_SLOT_PATCHED).ok()?;
    let len_bytes = bytes.get(off + 32..off + 36)?;
    let len = usize::try_from(u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]])).ok()?;
    let end = STUB_SLOT_HEADER.checked_add(len)?;
    if end > STUB_SLOT_SIZE {
        return None;
    }
    bytes.get(off + STUB_SLOT_HEADER..off + end)
}

fn find_slot_with_tag(bytes: &[u8], tag: &[u8; 16]) -> Result<usize, &'static str> {
    let mut needle = [0u8; 32];
    needle[..16].copy_from_slice(STUB_SLOT_MAGIC);
    needle[16..32].copy_from_slice(tag);
    if bytes.len() < STUB_SLOT_SIZE {
        return Err("launcher binary is smaller than EMBEDDED_STUB_SLOT");
    }
    let last = bytes.len() - STUB_SLOT_HEADER;
    let mut found: Option<usize> = None;
    let mut i = 0usize;
    while i <= last {
        if bytes.get(i).copied() == Some(STUB_SLOT_MAGIC[0])
            && bytes.get(i..i + 32) == Some(needle.as_slice())
            && (tag != STUB_SLOT_UNPATCHED || bytes.get(i + 32..i + 36) == Some(&[0u8; 4][..]))
        {
            if found.is_some() {
                return Err("multiple EMBEDDED_STUB_SLOT markers found in launcher binary");
            }
            found = Some(i);
            i = i.saturating_add(STUB_SLOT_HEADER);
            continue;
        }
        i += 1;
    }
    let Some(off) = found else {
        return Err("EMBEDDED_STUB_SLOT marker not found in launcher binary");
    };
    let end = off.checked_add(STUB_SLOT_SIZE).ok_or("EMBEDDED_STUB_SLOT offset overflow")?;
    if end > bytes.len() {
        return Err("EMBEDDED_STUB_SLOT extends past end of launcher binary");
    }
    Ok(off)
}

/// Patches `EMBEDDED_STUB_SLOT` at `slot_offset` with the active tag, LE length, and UTF-8
/// `payload`, zeroing any remaining bytes in the 16 KiB slot.
pub(crate) fn patch_stub_slot(bytes: &mut [u8], slot_offset: usize, payload: &[u8]) -> Result<(), &'static str> {
    let cap = STUB_SLOT_SIZE - STUB_SLOT_HEADER;
    if payload.len() > cap {
        return Err("stub configuration exceeds 16 KiB embedded slot capacity");
    }
    let len_u32 = u32::try_from(payload.len()).map_err(|_| "stub configuration length exceeds u32")?;
    let end = slot_offset.checked_add(STUB_SLOT_SIZE).ok_or("slot offset overflow")?;
    let slot = bytes.get_mut(slot_offset..end).ok_or("slot slice out of bounds")?;
    if let Some(dst) = slot.get_mut(..16) {
        dst.copy_from_slice(STUB_SLOT_MAGIC);
    }
    if let Some(dst) = slot.get_mut(16..32) {
        dst.copy_from_slice(STUB_SLOT_PATCHED);
    }
    if let Some(dst) = slot.get_mut(32..36) {
        dst.copy_from_slice(&len_u32.to_le_bytes());
    }
    let payload_end = STUB_SLOT_HEADER + payload.len();
    if let Some(dst) = slot.get_mut(STUB_SLOT_HEADER..payload_end) {
        dst.copy_from_slice(payload);
    }
    if let Some(rest) = slot.get_mut(payload_end..) {
        rest.fill(0);
    }
    Ok(())
}

pub(crate) fn read_le32(bytes: &[u8], off: usize) -> Option<u32> {
    let b = bytes.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_le64(bytes: &[u8], off: usize) -> Option<u64> {
    let b = bytes.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

pub(crate) fn read_be32(bytes: &[u8], off: usize) -> Option<u32> {
    let b = bytes.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_be64(bytes: &[u8], off: usize) -> Option<u64> {
    let b = bytes.get(off..off.checked_add(8)?)?;
    Some(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Keeps a patched thin 64-bit Mach-O `blaze-launcher` validly ad-hoc code-signed on macOS:
/// - Fast path (standard 1-blob ad-hoc `LC_CODE_SIGNATURE` from Apple `ld` / `blaze_export::sign`):
///   recomputes SHA-256 only for the 4 KiB pages overlapping `[slot_offset .. slot_offset + slot_len]`
///   and writes those hashes in place (~10 µs).
/// - General path (multi-blob or CMS-signed `LC_CODE_SIGNATURE` at EOF): replaces the trailing
///   signature with a fresh canonical ad-hoc `LC_CODE_SIGNATURE` via `blaze_export::sign::sign`
///   and updates `LC_CODE_SIGNATURE.datasize`, `__LINKEDIT.filesize`, and `__LINKEDIT.vmsize`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn resign_macho_after_patch(
    bytes: &mut AVec<'_, u8>,
    slot_offset: usize,
    slot_len: usize,
) -> Result<(), &'static str> {
    const MH_MAGIC_64: u32 = 0xFEED_FACF;
    const LC_SEGMENT_64: u32 = 0x19;
    const LC_CODE_SIGNATURE: u32 = 0x1D;
    const CS_ADHOC: u32 = 0x2;
    const CS_HASHTYPE_SHA256: u8 = 2;
    const ARM64_PAGE: u64 = 16 * 1024;

    if read_le32(bytes.as_slice(), 0) != Some(MH_MAGIC_64) {
        return Err("launcher is not a thin 64-bit Mach-O executable");
    }
    let ncmds = read_le32(bytes.as_slice(), 16).ok_or("truncated Mach-O header")?;
    let sizeofcmds = read_le32(bytes.as_slice(), 20).ok_or("truncated Mach-O header")?;
    let cmds_end = 32usize
        .checked_add(usize::try_from(sizeofcmds).map_err(|_| "sizeofcmds overflow")?)
        .ok_or("Mach-O load commands overflow")?;
    if cmds_end > bytes.len() {
        return Err("Mach-O load commands extend past EOF");
    }

    let mut text_filesize: Option<u64> = None;
    let mut linkedit: Option<(usize, u64)> = None;
    let mut sig_cmd: Option<(usize, usize, usize)> = None;
    let mut cursor = 32usize;
    for _ in 0..ncmds {
        let cmd = read_le32(bytes.as_slice(), cursor).ok_or("truncated load command")?;
        let cmdsize = read_le32(bytes.as_slice(), cursor + 4).ok_or("truncated load command size")?;
        let cmdsize_usize = usize::try_from(cmdsize).map_err(|_| "cmdsize overflow")?;
        if cmdsize_usize < 8 {
            return Err("Mach-O load command smaller than 8 bytes");
        }
        let next = cursor.checked_add(cmdsize_usize).ok_or("load command cursor overflow")?;
        if next > cmds_end {
            return Err("Mach-O load command extends past sizeofcmds");
        }
        if cmd == LC_SEGMENT_64 && cmdsize_usize >= 72 {
            let segname = bytes.get(cursor + 8..cursor + 24).ok_or("truncated segname")?;
            if segname.starts_with(b"__TEXT\0") {
                text_filesize = read_le64(bytes.as_slice(), cursor + 48);
            } else if segname.starts_with(b"__LINKEDIT\0") {
                let fileoff = read_le64(bytes.as_slice(), cursor + 40).ok_or("truncated __LINKEDIT")?;
                linkedit = Some((cursor, fileoff));
            }
        } else if cmd == LC_CODE_SIGNATURE && cmdsize_usize >= 16 {
            let dataoff = read_le32(bytes.as_slice(), cursor + 8).ok_or("truncated LC_CODE_SIGNATURE")?;
            let datasize = read_le32(bytes.as_slice(), cursor + 12).ok_or("truncated LC_CODE_SIGNATURE")?;
            sig_cmd = Some((
                cursor,
                usize::try_from(dataoff).map_err(|_| "sig dataoff overflow")?,
                usize::try_from(datasize).map_err(|_| "sig datasize overflow")?,
            ));
        }
        cursor = next;
    }

    let (sig_cmd_off, sig_off, sig_size) = sig_cmd.ok_or("launcher Mach-O has no LC_CODE_SIGNATURE")?;
    let sig_end = sig_off.checked_add(sig_size).ok_or("LC_CODE_SIGNATURE bounds overflow")?;
    if sig_end > bytes.len() {
        return Err("LC_CODE_SIGNATURE extends past EOF");
    }
    let patch_end = slot_offset.checked_add(slot_len).ok_or("slot end overflow")?;
    if patch_end > sig_off {
        return Err("EMBEDDED_STUB_SLOT is not inside signed code region");
    }

    // Fast path: 1-blob ad-hoc SHA-256 CS_CodeDirectory covering `0..sig_off`.
    let can_rehash_in_place = (|| -> Option<(usize, usize, usize)> {
        let super_magic = read_be32(bytes.as_slice(), sig_off)?;
        let count = read_be32(bytes.as_slice(), sig_off + 8)?;
        if super_magic != blaze_export::sign::SUPER_BLOB || count != 1 {
            return None;
        }
        let blob_type = read_be32(bytes.as_slice(), sig_off + 12)?;
        let blob_rel = usize::try_from(read_be32(bytes.as_slice(), sig_off + 16)?).ok()?;
        if blob_type != 0 {
            return None;
        }
        let cd_off = sig_off.checked_add(blob_rel)?;
        if cd_off.checked_add(44)? > sig_end {
            return None;
        }
        let cd_magic = read_be32(bytes.as_slice(), cd_off)?;
        let flags = read_be32(bytes.as_slice(), cd_off + 12)?;
        let hash_offset = usize::try_from(read_be32(bytes.as_slice(), cd_off + 16)?).ok()?;
        let n_code_slots = usize::try_from(read_be32(bytes.as_slice(), cd_off + 28)?).ok()?;
        let code_limit_32 = u64::from(read_be32(bytes.as_slice(), cd_off + 32)?);
        let hash_size = *bytes.get(cd_off + 36)?;
        let hash_type = *bytes.get(cd_off + 37)?;
        let page_shift = *bytes.get(cd_off + 39)?;
        if cd_magic != blaze_export::sign::CODE_DIRECTORY
            || (flags & CS_ADHOC) == 0
            || hash_type != CS_HASHTYPE_SHA256
            || usize::from(hash_size) != blaze_export::sign::HASH
            || !(10..=16).contains(&page_shift)
        {
            return None;
        }
        let code_limit = if code_limit_32 != 0 {
            usize::try_from(code_limit_32).ok()?
        } else {
            let cd_len = usize::try_from(read_be32(bytes.as_slice(), cd_off + 4)?).ok()?;
            if cd_len < 64 {
                return None;
            }
            usize::try_from(read_be64(bytes.as_slice(), cd_off + 56)?).ok()?
        };
        if code_limit != sig_off {
            return None;
        }
        let page = 1usize << usize::from(page_shift);
        if code_limit.div_ceil(page) != n_code_slots {
            return None;
        }
        let hashes_start = cd_off.checked_add(hash_offset)?;
        let hashes_end = hashes_start.checked_add(n_code_slots.checked_mul(blaze_export::sign::HASH)?)?;
        if hashes_end > sig_end {
            return None;
        }
        Some((hashes_start, page, code_limit))
    })();

    if let Some((hashes_start, page, code_limit)) = can_rehash_in_place {
        let first_page = slot_offset / page;
        let last_page = patch_end.saturating_sub(1) / page;
        for p in first_page..=last_page {
            let from = p.checked_mul(page).ok_or("page offset overflow")?;
            let to = from.saturating_add(page).min(code_limit);
            let slice = bytes.get(from..to).ok_or("code page out of bounds")?;
            let digest = blaze_export::sign::page_digest(slice);
            let dst_off = hashes_start
                .checked_add(p.checked_mul(blaze_export::sign::HASH).ok_or("hash slot overflow")?)
                .ok_or("hash dst overflow")?;
            let dst = bytes
                .get_mut(dst_off..dst_off + blaze_export::sign::HASH)
                .ok_or("hash slot out of bounds")?;
            dst.copy_from_slice(&digest);
        }
        return Ok(());
    }

    // General path: replace trailing signature blob with a fresh ad-hoc CS_CodeDirectory.
    if sig_end != bytes.len() {
        return Err("non-ad-hoc LC_CODE_SIGNATURE is not at EOF");
    }
    let text_len = text_filesize.ok_or("launcher Mach-O has no __TEXT segment")?;
    let (linkedit_off, linkedit_fileoff) = linkedit.ok_or("launcher Mach-O has no __LINKEDIT segment")?;
    let sig_off_u64 = u64::try_from(sig_off).map_err(|_| "sig_off exceeds u64")?;
    let new_sig_len_u64 = blaze_export::sign::len("blaze-launcher", sig_off_u64);
    let new_sig_u32 = u32::try_from(new_sig_len_u64).map_err(|_| "new signature size exceeds u32")?;
    let new_file_len = sig_off_u64.checked_add(new_sig_len_u64).ok_or("new file size overflow")?;
    let new_linkedit_filesize = new_file_len
        .checked_sub(linkedit_fileoff)
        .ok_or("__LINKEDIT fileoff exceeds file size")?;
    let new_linkedit_vmsize = new_linkedit_filesize.next_multiple_of(ARM64_PAGE);
    if let Some(dst) = bytes.get_mut(sig_cmd_off + 12..sig_cmd_off + 16) {
        dst.copy_from_slice(&new_sig_u32.to_le_bytes());
    }
    if let Some(dst) = bytes.get_mut(linkedit_off + 32..linkedit_off + 40) {
        dst.copy_from_slice(&new_linkedit_vmsize.to_le_bytes());
    }
    if let Some(dst) = bytes.get_mut(linkedit_off + 48..linkedit_off + 56) {
        dst.copy_from_slice(&new_linkedit_filesize.to_le_bytes());
    }
    bytes.truncate(sig_off);
    blaze_export::sign::sign(bytes, "blaze-launcher", text_len, true);
    Ok(())
}

/// Which `CheckScope` variant produced the pinned engine answer embedded in a JIT executable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JitScopeKind {
    Whole,
    Bin,
}

impl JitScopeKind {
    fn as_str(self) -> &'static str {
        match self {
            JitScopeKind::Whole => "whole",
            JitScopeKind::Bin => "bin",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "whole" => Some(JitScopeKind::Whole),
            "bin" => Some(JitScopeKind::Bin),
            _ => None,
        }
    }
}

/// Current schema version of `mode=jit-exec` manifests embedded in default `blaze build` binaries.
pub(crate) const JIT_MANIFEST_VERSION: u32 = 1;

/// Sans-IO self-describing boot manifest embedded inside `blaze-launcher`'s `EMBEDDED_STUB_SLOT`
/// for default JIT-on-call `blaze build` executables.
///
/// Covers every input needed to boot the exact checked program directly from the CAS store with
/// zero workspace re-check and zero ambient environment assumptions (Rules 14, 21, 75).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct JitBuildManifest<'a> {
    pub blaze: AString<'a>,
    pub checkout: AString<'a>,
    pub store_home: AString<'a>,
    pub cargo_home: Option<AString<'a>>,
    pub sdk_root: Option<AString<'a>>,
    pub cert: [u8; 32],
    pub package: AString<'a>,
    pub bin: AString<'a>,
    pub manifest: AString<'a>,
    pub root: AString<'a>,
    pub edition: AString<'a>,
    pub scope_kind: JitScopeKind,
    pub toolchain: [u8; 32],
    pub pgo_key: [u8; 32],
    /// Digest of the template `blaze-launcher` binary so no-op rebuild detection invalidates if
    /// `blaze-launcher` itself was rebuilt (Rule 21).
    pub launcher_digest: [u8; 32],
}

/// Why decoding a `JitBuildManifest` failed (Rule 3: explicit field/cause).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JitManifestError {
    NotUtf8,
    BadMode,
    BadVersion,
    MissingField(&'static str),
    InvalidField { field: &'static str, why: &'static str },
}

impl fmt::Display for JitManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JitManifestError::NotUtf8 => f.write_str("JIT manifest bytes are not valid UTF-8"),
            JitManifestError::BadMode => f.write_str("JIT manifest is missing `mode=jit-exec`"),
            JitManifestError::BadVersion => write!(f, "JIT manifest version is not {JIT_MANIFEST_VERSION}; rebuild with `blaze build`"),
            JitManifestError::MissingField(name) => write!(f, "JIT manifest is missing required field `{name}`"),
            JitManifestError::InvalidField { field, why } => write!(f, "JIT manifest field `{field}` is invalid: {why}"),
        }
    }
}

impl<'a> JitBuildManifest<'a> {
    /// Serializes this manifest into deterministic `key=value\n` UTF-8 text (all paths hex-encoded
    /// so arbitrary spaces/newlines never corrupt line parsing).
    pub(crate) fn encode(&self, out: &mut AString<'_>) {
        out.clear();
        let _ = writeln!(out, "mode=jit-exec");
        let _ = writeln!(out, "version={JIT_MANIFEST_VERSION}");
        out.push_str("blaze_hex=");
        encode_hex(self.blaze.as_bytes(), out);
        out.push('\n');
        out.push_str("checkout_hex=");
        encode_hex(self.checkout.as_bytes(), out);
        out.push('\n');
        out.push_str("store_home_hex=");
        encode_hex(self.store_home.as_bytes(), out);
        out.push('\n');
        if let Some(cargo_home) = self.cargo_home.as_ref() {
            out.push_str("cargo_home_hex=");
            encode_hex(cargo_home.as_bytes(), out);
            out.push('\n');
        }
        if let Some(sdk_root) = self.sdk_root.as_ref() {
            out.push_str("sdk_root_hex=");
            encode_hex(sdk_root.as_bytes(), out);
            out.push('\n');
        }
        out.push_str("cert=");
        encode_hex(&self.cert, out);
        out.push('\n');
        out.push_str("package_hex=");
        encode_hex(self.package.as_bytes(), out);
        out.push('\n');
        out.push_str("bin_hex=");
        encode_hex(self.bin.as_bytes(), out);
        out.push('\n');
        out.push_str("manifest_hex=");
        encode_hex(self.manifest.as_bytes(), out);
        out.push('\n');
        out.push_str("root_hex=");
        encode_hex(self.root.as_bytes(), out);
        out.push('\n');
        out.push_str("edition_hex=");
        encode_hex(self.edition.as_bytes(), out);
        out.push('\n');
        let _ = writeln!(out, "scope_kind={}", self.scope_kind.as_str());
        out.push_str("toolchain=");
        encode_hex(&self.toolchain, out);
        out.push('\n');
        out.push_str("pgo_key=");
        encode_hex(&self.pgo_key, out);
        out.push('\n');
        out.push_str("launcher_digest=");
        encode_hex(&self.launcher_digest, out);
        out.push('\n');
    }

    /// Parses and strictly validates a `JitBuildManifest` from raw UTF-8 manifest bytes.
    pub(crate) fn decode(bytes: &[u8], alloc: Alloc<'a>) -> Result<JitBuildManifest<'a>, JitManifestError> {
        let text = core::str::from_utf8(bytes).map_err(|_| JitManifestError::NotUtf8)?;
        let line = |name: &'static str| {
            text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
        };
        let req = |name: &'static str| line(name).ok_or(JitManifestError::MissingField(name));

        if line("mode") != Some("jit-exec") {
            return Err(JitManifestError::BadMode);
        }
        let ver_raw = req("version")?;
        let ver = ver_raw.parse::<u32>().map_err(|_| JitManifestError::BadVersion)?;
        if ver != JIT_MANIFEST_VERSION {
            return Err(JitManifestError::BadVersion);
        }
        let dec_str = |field: &'static str| -> Result<AString<'a>, JitManifestError> {
            let raw = req(field)?;
            decode_hex_str(raw, alloc).map_err(|why| JitManifestError::InvalidField { field, why })
        };
        let dec_32 = |field: &'static str| -> Result<[u8; 32], JitManifestError> {
            let raw = req(field)?;
            decode_hex32(raw).map_err(|why| JitManifestError::InvalidField { field, why })
        };

        let blaze = dec_str("blaze_hex")?;
        let checkout = dec_str("checkout_hex")?;
        let store_home = dec_str("store_home_hex")?;
        let cargo_home = match line("cargo_home_hex") {
            Some(raw) => Some(decode_hex_str(raw, alloc).map_err(|why| JitManifestError::InvalidField { field: "cargo_home_hex", why })?),
            None => None,
        };
        let sdk_root = match line("sdk_root_hex") {
            Some(raw) => Some(decode_hex_str(raw, alloc).map_err(|why| JitManifestError::InvalidField { field: "sdk_root_hex", why })?),
            None => None,
        };
        let cert = dec_32("cert")?;
        let package = dec_str("package_hex")?;
        let bin = dec_str("bin_hex")?;
        let manifest = dec_str("manifest_hex")?;
        let root = dec_str("root_hex")?;
        let edition = dec_str("edition_hex")?;
        let scope_raw = req("scope_kind")?;
        let scope_kind = JitScopeKind::parse(scope_raw).ok_or(JitManifestError::InvalidField {
            field: "scope_kind",
            why: "expected `whole` or `bin`",
        })?;
        let toolchain = dec_32("toolchain")?;
        let pgo_key = dec_32("pgo_key")?;
        let launcher_digest = dec_32("launcher_digest")?;

        Ok(JitBuildManifest {
            blaze,
            checkout,
            store_home,
            cargo_home,
            sdk_root,
            cert,
            package,
            bin,
            manifest,
            root,
            edition,
            scope_kind,
            toolchain,
            pgo_key,
            launcher_digest,
        })
    }

    /// Decodes a hex-encoded `JitBuildManifest` (passed by `blaze-launcher` via `--manifest <hex>`).
    pub(crate) fn decode_hex(hex: &str, alloc: Alloc<'a>) -> Result<JitBuildManifest<'a>, JitManifestError> {
        let mut raw = Vec::new_in(alloc);
        decode_hex_into(hex, &mut raw).map_err(|why| JitManifestError::InvalidField { field: "manifest", why })?;
        Self::decode(raw.as_slice(), alloc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_manifest_round_trips_and_refuses_corrupt_or_wrong_version() {
        let alloc = blaze_own::testing::heap().alloc();
        let original = JitBuildManifest {
            blaze: String::from_str_in("/opt/blaze/bin/blaze", alloc),
            checkout: String::from_str_in("/Users/dev/my project/hello", alloc),
            store_home: String::from_str_in("/Users/dev", alloc),
            cargo_home: Some(String::from_str_in("/Users/dev/.cargo", alloc)),
            sdk_root: Some(String::from_str_in("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk", alloc)),
            cert: [0x11; 32],
            package: String::from_str_in("hello", alloc),
            bin: String::from_str_in("hello", alloc),
            manifest: String::from_str_in("Cargo.toml", alloc),
            root: String::from_str_in("src/main.rs", alloc),
            edition: String::from_str_in("2024", alloc),
            scope_kind: JitScopeKind::Bin,
            toolchain: [0x22; 32],
            pgo_key: [0x33; 32],
            launcher_digest: [0x44; 32],
        };
        let mut encoded = String::new_in(alloc);
        original.encode(&mut encoded);
        assert!(encoded.len() < STUB_SLOT_SIZE - STUB_SLOT_HEADER);

        let decoded = JitBuildManifest::decode(encoded.as_bytes(), alloc).expect("round trip");
        assert_eq!(decoded, original);

        // Hex round-trip as passed from blaze-launcher to `blaze jit-exec`.
        let mut hex = String::new_in(alloc);
        encode_hex(encoded.as_bytes(), &mut hex);
        let decoded_hex = JitBuildManifest::decode_hex(hex.as_str(), alloc).expect("hex round trip");
        assert_eq!(decoded_hex, original);

        // Read back through patched stub slot.
        let mut fake = Vec::new_in(alloc);
        fake.resize(STUB_SLOT_SIZE + 8192, 0u8);
        let off = 4096usize;
        fake[off..off + 16].copy_from_slice(STUB_SLOT_MAGIC);
        fake[off + 16..off + 32].copy_from_slice(STUB_SLOT_UNPATCHED);
        assert_eq!(find_stub_slot(fake.as_slice()), Ok(off));
        assert_eq!(read_patched_slot(fake.as_slice()), None);
        patch_stub_slot(fake.as_mut_slice(), off, encoded.as_bytes()).expect("patch");
        assert_eq!(read_patched_slot(fake.as_slice()), Some(encoded.as_bytes()));

        // Refusal checks (Rule 3).
        let bad_ver = encoded.as_str().replace("version=1", "version=99");
        assert_eq!(JitBuildManifest::decode(bad_ver.as_bytes(), alloc), Err(JitManifestError::BadVersion));
        let bad_mode = encoded.as_str().replace("mode=jit-exec", "mode=run");
        assert_eq!(JitBuildManifest::decode(bad_mode.as_bytes(), alloc), Err(JitManifestError::BadMode));
    }
}
