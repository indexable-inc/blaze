//! The ad-hoc code signature arm64 macOS demands before it runs a page of a file
//! (`LC_CODE_SIGNATURE`), in the form a linker writes: one `CS_SuperBlob` holding one
//! `CS_CodeDirectory`, the SHA-256 of every 4 KiB page of the file before the signature.
//! Structures and constants: `<kern/cs_blobs.h>` (`CS_SuperBlob`, `CS_BlobIndex`,
//! `CS_CodeDirectory` at version `CS_SUPPORTSEXECSEG` 0x20400, `CSMAGIC_EMBEDDED_SIGNATURE`,
//! `CSMAGIC_CODEDIRECTORY`, `CS_ADHOC`, `CS_LINKER_SIGNED`, `CS_EXECSEG_MAIN_BINARY`,
//! `CS_HASHTYPE_SHA256`); every field is big-endian.
//!
//! No special slots: a linker's signature has no Info.plist, requirements or entitlements
//! (to verify by the run test: macOS starts the file with exactly this directory).

use blaze_own::AVec;

use crate::bytes;
use crate::par::{ExportRunner, SequentialExportRunner};

pub const SUPER_BLOB: u32 = 0xFADE_0CC0;
pub const CODE_DIRECTORY: u32 = 0xFADE_0C02;
/// `CS_SUPPORTSEXECSEG`: the directory carries the executable segment's fields.
pub const VERSION: u32 = 0x0002_0400;
const ADHOC: u32 = 0x2;
const LINKER_SIGNED: u32 = 0x2_0000;
const EXECSEG_MAIN_BINARY: u64 = 0x1;
const HASH_SHA256: u8 = 2;
pub const HASH: usize = 32;
const PAGE_SHIFT: u8 = 12;
/// The page the directory hashes: 4 KiB whatever the file's segments' pages.
pub const PAGE: usize = 1 << PAGE_SHIFT;
/// The super blob's header (magic, length, count) and its one `CS_BlobIndex`.
pub const BLOB_HEADER: usize = 12 + 8;
/// `CS_CodeDirectory`'s fields through `execSegFlags`.
pub const DIRECTORY_HEADER: usize = 88;

/// SHA-256 digest of a single code page for a `CS_CodeDirectory` slot.
pub fn page_digest(page: &[u8]) -> [u8; HASH] {
    blaze_sha256::digest(page)
}

/// How long the signature of a file of `code_limit` bytes before it is, 16-aligned.
pub fn len(identifier: &str, code_limit: u64) -> u64 {
    let pages = (code_limit as usize).div_ceil(PAGE);
    (BLOB_HEADER + DIRECTORY_HEADER + identifier.len() + 1 + pages * HASH).next_multiple_of(16) as u64
}

/// Appends the signature of `file` (every byte of it: `file.len()` is the code limit) to it.
/// `text_len` is `__TEXT`'s file size (it starts at offset 0); `main` marks an executable.
pub fn sign(file: &mut AVec<'_, u8>, identifier: &str, text_len: u64, main: bool) {
    sign_par(file, identifier, text_len, main, &SequentialExportRunner);
}

/// Parallel [`sign`]: hashes every 4 KiB code page in `file[..limit]` across `runner` directly
/// into the pre-allocated `CS_CodeDirectory` hash slots (bit-for-bit identical to [`sign`], Rule 25).
pub fn sign_par<R: ExportRunner>(file: &mut AVec<'_, u8>, identifier: &str, text_len: u64, main: bool, runner: &R) {
    let limit = file.len();
    let pages = limit.div_ceil(PAGE);
    let hashes_at = DIRECTORY_HEADER + identifier.len() + 1;
    let directory_len = hashes_at + pages * HASH;
    let total = len(identifier, limit as u64) as usize;
    let start = file.len();

    file.reserve(total);
    bytes::be32(file, SUPER_BLOB);
    bytes::be32(file, (BLOB_HEADER + directory_len) as u32);
    bytes::be32(file, 1);
    // `CS_BlobIndex`: the code directory's slot (`CSSLOT_CODEDIRECTORY`, 0) and its offset.
    bytes::be32(file, 0);
    bytes::be32(file, BLOB_HEADER as u32);

    bytes::be32(file, CODE_DIRECTORY);
    bytes::be32(file, directory_len as u32);
    bytes::be32(file, VERSION);
    bytes::be32(file, ADHOC | LINKER_SIGNED);
    bytes::be32(file, hashes_at as u32); // hashOffset
    bytes::be32(file, DIRECTORY_HEADER as u32); // identOffset
    bytes::be32(file, 0); // nSpecialSlots
    bytes::be32(file, pages as u32); // nCodeSlots
    bytes::be32(file, limit as u32); // codeLimit
    file.extend_from_slice(&[HASH as u8, HASH_SHA256, 0, PAGE_SHIFT]); // hashSize, hashType, platform, pageSize
    bytes::be32(file, 0); // spare2
    bytes::be32(file, 0); // scatterOffset
    bytes::be32(file, 0); // teamOffset
    bytes::be32(file, 0); // spare3
    bytes::be64(file, 0); // codeLimit64: the 32-bit limit says it
    bytes::be64(file, 0); // execSegBase: `__TEXT` is at file offset 0
    bytes::be64(file, text_len); // execSegLimit
    bytes::be64(file, if main { EXECSEG_MAIN_BINARY } else { 0 }); // execSegFlags
    file.extend_from_slice(identifier.as_bytes());
    file.push(0);

    let hash_start = file.len();
    file.resize(start + total, 0);
    if pages > 0 {
        let (before_hashes, sig_tail) = file.split_at_mut(hash_start);
        let payload = &before_hashes[..limit];
        let hash_bytes = pages * HASH;
        if let Some(dst) = sig_tail.get_mut(..hash_bytes) {
            let (hash_slots, _) = dst.as_chunks_mut::<HASH>();
            runner.for_each_chunk_mut(hash_slots, 64, |first_page, chunk| {
                for (k, slot) in chunk.iter_mut().enumerate() {
                    let page = first_page + k;
                    let from = page * PAGE;
                    let to = (from + PAGE).min(limit);
                    *slot = blaze_sha256::digest(&payload[from..to]);
                }
            });
        }
    }
}
