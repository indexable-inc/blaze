//! The plain store: content-keyed records in a few immutable segment files, a generation
//! manifest, and a two-slot head whose flip is the commit. It is the store in the form the
//! outside audit of 2026-11-13 asked for (step 3, "the store in Relax's form, behind the
//! existing get/put"), shaped by the owner's targets (128-core servers, 8-16-core Macs):
//! readers take no lock, each process writes its own segment, the flip is the only lock, no
//! sync on the edit path, compaction never stops a build. Chosen per store root by
//! `<root>/store.json` ([`settings`]); [`Opened`] is either form.
//!
//! # On disk (`<root>/plain/`), little-endian
//!
//! - `head`, 4160 bytes: two 64-byte slots at 0 and 4096 (a sector apart, so a torn write of
//!   one never touches the other; nothing assumes a page size). Generation `g` lives in slot
//!   `g % 2`. A slot: 0..4 `BLPH`, 4..8 format (1), 8..16 generation, 16..24 the manifest
//!   log's id, 24..32 the manifest's offset in it, 32..36 its length, 36..40 its CRC-32C,
//!   40..60 zero, 60..64 CRC-32C of 0..60. The newest whole slot whose manifest is whole is
//!   the committed generation. The file is made at its full length (zeros) and synced at the
//!   store's first open, before any segment; every flip, the first included, writes one slot
//!   in place inside one sector, so a tear can destroy only the slot being written. A short
//!   head is read as far as it goes (its missing slots torn); it is another store's file only
//!   when another magic stands where a slot begins. A torn slot is reported, and what no whole
//!   generation names is read from the segments' footers and reported.
//! - `manifests-<id:016x>`: manifests appended at 16-byte-aligned offsets, a new log past
//!   4 MiB (the old one retired). A manifest: 0..4 `BLPM`, 4..8 format, 8..16 generation,
//!   16..20 segments `s`, 20..24 retired `r`, 24..28 roots `t`, 28..32 zero; `s` rows of 80
//!   (32 digest, 8 file length, 8 index offset, 4 entries, 4 flags: bit 0 held here, bit 1
//!   published; 8 namespace, 8 generation added, 8 boot written or checked); `r` rows of 24
//!   (8 id, 4 one for a log, 4 zero, 8 retired at, wall ns); `t` roots (32 key, 8 generation,
//!   2 name length, name, zeros to 8). A segment's id is the first 8 bytes of its digest.
//! - `seg-<id:016x>`, written once, never changed, and the wire format of the shared cache as
//!   it lies (nothing of the machine in it): a 128-byte header (`BLPS`, format u32, namespace
//!   u64, count u32, zero u32, index offset u64, lowest key, highest key, 32 zero); payloads
//!   from 128 at 16-aligned offsets; `count` sorted 48-byte entries (32 key; 8 place:
//!   offset/16 in bits 0..40, kind in 56..64; 4 length; 4 CRC-32C of key, kind byte and
//!   payload); a 48-byte footer (`BLPE`, count u32, index offset u64, the BLAKE3 digest of
//!   every byte before the digest). A segment never mixes namespaces; one no manifest names
//!   still says what it held; its index is an immutable object a remote source serves by range
//!   (`format::find_ranged`: about one 64 KiB read per lookup in a shard of a million).
//! - `flip`: the lock file. `CACHEDIR.TAG`: the cache directory tag; on macOS the directory
//!   also carries Time Machine's exclusion attribute ([`Driven`]).
//!
//! # A commit, in order
//!
//! 1. The store's batch and the workers' [`Batch`]es (each filled by one worker, no shared
//!    word) are laid end to end as one segment: no copy of a worker's bytes, which move into
//!    their writes. The index is sorted once (n log n on the commit's entries).
//! 2. `seg-<id>` is made at its length and every piece written at its offset, in parallel. No
//!    lock is held. No sync (durable mode: one sync of the segment here).
//! 3. The flip lock is taken (`flock`, never waiting in the kernel: a 2 ms timer between
//!    asks, stopped by name after 60 s). Under it: the head is read again, and the manifest
//!    when another process flipped since (so no update is lost); the new manifest is built
//!    (this segment added, roots changed, files retired past the grace period dropped) and
//!    appended to the log; the inactive slot is written. No sync (durable: the log is synced
//!    before the slot is written and the head after). Unlock. Held for: one 4 KiB-ish head
//!    read, at most one manifest read, two small writes; tens of microseconds, never a copy
//!    of a record. A step that fails under the lock lets it go as the store stops, so a
//!    process living on with a broken store never holds every other process's commit.
//! 4. Files retired more than [`GRACE_NS`] ago are unlinked; new segments of other processes
//!    are mapped.
//!
//! # Integrity without syncs
//!
//! On APFS `fsync` does not reach the media and `F_FULLFSYNC` costs milliseconds; the store is
//! a cache. What a power cut can do is lose what the kernel had not written back, in any
//! order across files. So: every slot and manifest carries a CRC-32C and every segment a
//! BLAKE3 digest of all its bytes; a slot whose manifest is not whole falls back to the other
//! slot's generation; and every segment records the boot it was written in. Bytes are verified
//! once, and the type carries the fact (`verified::Verified`, decision 18): a segment of this
//! boot is proved by its row (written, checked or fetched in this boot: the page cache holds
//! it, a process crash loses none of it); a segment of an earlier boot is checked in full
//! (length, layout, digest) once, at the first open of the new boot, and the check is recorded
//! by a flip, so no process of that boot repeats it; a fetched segment's statement and digest
//! are checked once, at the fetch. A segment that fails is dropped and reported by name with
//! its record count ([`Report`], logged as a warning). A read checks nothing ([`Checks`]): a
//! byte that rots on the disk after its proof is found by [`Driven::scrub`] (the per-record
//! CRC-32Cs, evaluated only there), by the next boot's full check, or by the consumer failing
//! on it; the store is a cache, rebuilt from what made it. With no sync, the open guarantees
//! the generation it read whole ([`Report::whole`]) and everything at or before it; a power
//! cut may take a later commit without a trace (durable mode syncs so it cannot).
//!
//! # Reads
//!
//! Each segment is mapped once per process, never unmapped during a run, and its index
//! touched a page at a time once mapped. A lookup probes the index of each segment of the
//! generation, newest first, with no lock and no write to anything shared: up to three
//! interpolation steps on the key's first 8 bytes, then bisection. [`Driven::get`] returns
//! the payload in place (the mapping); [`Driven::get_into`] reads it with one `pread` into
//! the caller's buffer. Per lookup: 0 syscalls and 0 faults for the index (touched), plus
//! for `get_into` one `pread`, for `get` the payload's pages faulted on first touch; no
//! checksum of any kind.
//!
//! # Compaction ([`Driven::compact`])
//!
//! Run by a command after it has answered, or by its own invocation, on a lowered thread
//! (utility QoS on macOS; nice 19 and the idle IO class on Linux); never before a build's
//! first read. A slice: the directory is listed (files no generation names are retired, so
//! what a crash left is swept a grace period later); while the segments hold more than the
//! cap, the oldest segments older than [`WINDOW`] generations, at most [`SLICE_BYTES`], are
//! copied keeping what roots name; while a generation names more than [`SEGMENTS_MOST`]
//! segments, the oldest are merged keeping everything. The copy is written with no lock; the
//! flip replaces them only if they are all still named (else the slice aborts and removes its
//! copy), and retires them. A retired file is unlinked [`GRACE_NS`] after: a process that read
//! a head maps what it names within milliseconds, and a mapping outlives an unlink. A
//! published segment is never rewritten: a merge leaves it, and pruning only stops holding it
//! here.
//!
//! # The shared cache ([`Source`], [`Verifier`])
//!
//! A manifest may name segments not held here (published by a trusted builder). On a miss,
//! [`Driven::fetch_missing`] asks a [`Source`] where the keys are, in one batch, only in a
//! namespace this store reads; each segment found is refused above [`SEGMENT_MOST`], its
//! signed statement checked by the [`Verifier`] before a byte of it is fetched, then fetched
//! whole, checked against its digest, layout and namespace, written unchanged and adopted by
//! a flip. [`LocalDir`] is the only source built here.

pub mod settings;

mod core;
mod driven;
mod format;
mod verified;

#[cfg(test)]
mod tests;

pub use self::core::{
    Batch, Committed, GRACE_NS, Got, Located, Lost, Plain, PlainConfig, PutError, Report,
    SEGMENTS_MOST, SLICE_BYTES, Sliced, TAGS, WINDOW,
};
pub use self::driven::{Driven, Found, LocalDir, Opened, Source, Verifier, lower_this_thread};
pub use self::format::{Namespace, SEGMENT_MOST, Segment};
pub use self::settings::{Form, Settings};
pub use self::verified::Checks;
