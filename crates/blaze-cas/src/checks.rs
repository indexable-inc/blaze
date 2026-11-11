//! A pack's block checks: the file `checks-<n>` beside `pack-<n>`
//! (docs/v2/plan/design/store-pool.md, "Verification on load").
//!
//! One CRC-32C per 4 KiB block ([`BLOCK`]) of the pack, 4 bytes each, little-endian, in the
//! pack's order, with no header: the check of block `b` is at byte `4 * b`. A block never changes
//! once it is committed (every commit pads its packs to a block), so the file is only ever
//! appended to, and what it must hold is the pack's committed length over [`BLOCK`]: the head
//! needs no field for it.
//!
//! A commit cuts the file back to the checks of what is committed, writes the checks of the
//! blocks it adds, and syncs it, all before the head that names the new length. So after a crash
//! the file holds at least the checks of every block the head names; anything after is a torn
//! append, which nothing reads and the next commit cuts.
//!
//! What they are for: bytes read into the store's buffer pool are read again after every
//! eviction, and hashing a record again on every reload would cost as much as the read. A reload
//! is checked by block instead, and BLAKE3 stays once a record a process. The open checks the
//! blocks its recovery scan reads (from a pack's indexed length to its committed length).

use std::ops::Range;

use crate::{crc, disk::BLOCK};

/// The bytes one block's check takes in the file.
pub(crate) const CHECK: u64 = 4;

/// The check of one block (a file's last may be short).
pub(crate) fn of_block(bytes: &[u8]) -> u32 {
    crc::crc32c(bytes)
}

/// The check of every block of `bytes`, which start at a block's edge.
pub(crate) fn of(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks(BLOCK as usize).map(of_block).collect() // alloc: a commit's new checks, or a file's as it opens
}

/// How long the file is for a pack that commits `committed` bytes.
pub(crate) fn len_of(committed: u64) -> u64 {
    committed.div_ceil(BLOCK) * CHECK
}

/// `checks` as the file holds them.
pub(crate) fn encode(checks: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(checks.len() * CHECK as usize); // alloc: a commit's write of them, which the effect owns
    for check in checks {
        out.extend_from_slice(&check.to_le_bytes());
    }
    out
}

/// The checks of a pack that commits `committed` bytes, from its file's `bytes`. What the file
/// holds past them (a torn append) is left out.
///
/// # Errors
///
/// The file holds fewer checks than the head's committed length has blocks: the head names
/// bytes whose checks were never made durable, which no commit of this store leaves.
pub(crate) fn decode(bytes: &[u8], committed: u64) -> Result<Vec<u32>, String> {
    let want = len_of(committed);
    let Some(held) = usize::try_from(want).ok().and_then(|want| bytes.get(..want)) else {
        // alloc: the error that stops the opening
        return Err(format!(
            "it holds {} checks, fewer than the {} blocks of its pack that its head names",
            bytes.len() as u64 / CHECK,
            want / CHECK
        ));
    };
    let checks = held
        .chunks_exact(CHECK as usize)
        .map(|it| u32::from_le_bytes(it.try_into().expect("4 bytes")))
        .collect(); // alloc: a pack's checks, kept for the pack's life
    Ok(checks)
}

/// The first of `blocks` of a pack whose bytes in `pack` (its committed bytes, from its first)
/// do not match `checks`, or that either does not hold.
pub(crate) fn first_bad(checks: &[u32], pack: &[u8], blocks: Range<u64>) -> Option<u64> {
    blocks.into_iter().find(|&block| {
        let from = (block * BLOCK) as usize;
        let bytes = pack.get(from..).map(|rest| &rest[..rest.len().min(BLOCK as usize)]);
        match (bytes, checks.get(block as usize)) {
            (Some(bytes), Some(&check)) if !bytes.is_empty() => of_block(bytes) != check,
            _ => true,
        }
    })
}
