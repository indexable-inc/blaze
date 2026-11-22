//! One crate's built outputs as ONE record (design A, c.md D174): what `run` and `test` link
//! after a script's deferred C requests ran, found on a repeat by one get.
//!
//! [`crate::c_tasks`] keeps each object and archive as a record of its own, each valid by its
//! key; reaching an archive through them asks today's digest of every header each object read.
//! For a crate whose sources cannot change under one key that work is not needed again (rule
//! 77): the BUILT record is keyed by what the archives are a function of, known before any work
//! ([`built_key`]):
//!
//! - the script's run, by its out (the outputs' id: every request's argv, every file the script
//!   left in `OUT_DIR`, and each dependency's `links` values it was given);
//! - the toolchain ([`Toolchain`]: the strategy, the compiler's digest, the SDK's identity, the
//!   target);
//! - the package's files, by the sources store's key and the BLAKE3 of the files' listing
//!   ([`Pinned`]): a registry `.crate` the lock's checksum pins, or a git commit;
//! - the archives asked for (the demand), in order.
//!
//! A dependency's `OUT_DIR` a compile reads (`DEP_*_INCLUDE`) is not named again: its values are
//! in the script's out, and its files are its own script's outputs, whose out moves with them
//! under the same pinned package.
//!
//! A crate whose files can change (a workspace member, a path dependency) has no pinned key and
//! no built record: its objects are found through their path sets, against today's files.
//!
//! Nothing here reads a clock, a path of the host or the environment (rule 75).

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::AVec;

use crate::c_tasks::{FORM, Toolchain};

/// A package whose files cannot change while these are the same: the sources store's key and
/// the BLAKE3 of its files' listing (blaze-fetch tree.rs: every file by its BLAKE3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pinned {
    pub source: [u8; 32],
    pub files: [u8; 32],
}

/// The built record's key: the module's list, in that order.
pub fn built_key(script_out: ContentId, toolchain: &Toolchain<'_>, pinned: Pinned, demand: &[&str]) -> ContentId {
    let mut hasher = Hasher::new(FORM);
    hasher
        .part(b"built")
        .id(&script_out)
        .update(&[toolchain.kind as u8])
        .part(&toolchain.compiler)
        .part(toolchain.sdk)
        .part(toolchain.target.as_bytes())
        .part(&pinned.source)
        .part(&pinned.files)
        .update(&(demand.len() as u64).to_le_bytes());
    for name in demand {
        hasher.part(name.as_bytes());
    }
    hasher.finish()
}

/// A built record: `BCBT1`, then the bytes a script run's record holds as its payload (the
/// command's scripts.rs `payload_parts` form: how the script ran, one byte, then
/// [`crate::Outputs::encode`] of [`crate::c_tasks::built`]'s outputs). So [`open_built`] gives
/// what the link takes in place of the deferred record's payload, with no copy.
pub const BUILT_MAGIC: &[u8; 5] = b"BCBT1";

/// Encodes a built record of `payload` (a script run's payload form, above) into `out`.
pub fn encode_built(payload: &[u8], out: &mut AVec<'_, u8>) {
    out.clear();
    out.extend_from_slice(BUILT_MAGIC);
    out.extend_from_slice(payload);
}

/// The payload of a built record (a script run's payload form); `None` when `bytes` is not
/// one.
pub fn open_built(bytes: &[u8]) -> Option<&[u8]> {
    bytes.strip_prefix(&BUILT_MAGIC[..])
}

/// What one crate's C tasks came to, as the command reports it (the first real run reads it):
/// every count is of this crate alone.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Report {
    /// Every recorded request.
    pub requests: u32,
    /// Objects the demanded archives need.
    pub planned: u32,
    /// Of those, found by key.
    pub found: u32,
    /// Compiled now and recorded.
    pub compiled: u32,
    /// Compiles the compiler refused (the first is the error).
    pub failed: u32,
    /// The script's assumptions discharged: compiles it branched on that no archive names
    /// (feature checks, answered "exit 0" when deferred), each compiled or found like an
    /// object; counted in `found`/`compiled`/`failed` too.
    pub assumed: u32,
    /// Archives asked for, and how many were written now.
    pub archives: u32,
    pub archives_made: u32,
    /// The archives' bytes, together.
    pub archive_bytes: u64,
    /// Found whole by [`built_key`]: one get, nothing planned.
    pub built_hit: bool,
    /// Wall time of the crate's tasks, in milliseconds (the driver's clock).
    pub ms: u64,
}
