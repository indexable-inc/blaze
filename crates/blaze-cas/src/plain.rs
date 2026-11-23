//! The plain store: content-keyed records in a few immutable segment files, a generation
//! manifest, and a two-slot head whose flip is the commit. It is the store in the form the
//! outside audit of 2026-11-13 asked for (step 3, "the store in Relax's form, behind the
//! existing get/put"), shaped by the owner's targets (128-core servers, 8-16-core Macs):
//! readers take no lock, each process writes its own segment, the flip is the only lock, no
//! sync on the edit path, compaction never stops a build. Chosen per store root by
//! `<root>/store.json` ([`settings`]) sets its cap, durability and namespaces.
//!
//! # Allocation (rule 75)
//!
//! The store names no heap of its own: what it keeps takes its allocator as a type parameter
//! with no default (`A: Allocator + Clone`, as [`Lent`] and [`Snapshot`] do, `Send + Sync` where
//! a pool shares it), handed to its constructor by the caller; scratch that dies within one call
//! takes `alloc: A` as an argument. No type here has `Default`, an allocator-less `new` or an
//! impl fixed to one allocator, and a test makes its allocator at its top and hands it down.
//!
//! # On disk (`<root>/plain/`), little-endian
//!
//! - `head`, 4160 bytes: two 64-byte slots at 0 and 4096 (a sector apart, so a torn write of
//!   one never touches the other; nothing assumes a page size). Generation `g` lives in slot
//!   `g % 2`. A slot: 0..4 `BLPH`, 4..8 format (2), 8..16 generation, 16..24 the manifest
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
//!   published, bit 2 sealed; 8 namespace, 8 generation added, 8 boot written or checked); `r` rows of 24
//!   (8 id, 4 one for a log, 4 zero, 8 retired at, wall ns); `t` roots (32 key, 8 generation,
//!   2 name length, name, zeros to 8). A segment's id is the first 8 bytes of its digest.
//! - `seg-<id:016x>`, written once, never changed, and the wire format of the shared cache as
//!   it lies (nothing of the machine in it): a 128-byte header (`BLPS`, format u32, namespace
//!   u64, count u32, zero u32, index offset u64, lowest key, highest key, 32 zero); payloads
//!   from 128 at 16-aligned offsets; `count` sorted 48-byte entries (32 key; 8 place:
//!   offset/16 in bits 0..40, blaze-keyed's `Tag` (u16) in 40..56, zero in 56..64; 4 length;
//!   4 CRC-32C of key, tag and payload); a 48-byte footer (`BLPE`, count u32, index offset
//!   u64, the BLAKE3 digest of every byte before the digest). Format 1 (a kind's byte in bits
//!   56..64) is refused by name, every file of it, never read as empty. Zero records is a valid
//!   segment (176 bytes). The geometry is blaze-keyed's (`store::{HEADER, FOOTER, ENTRY,
//!   ALIGN}`); [`seal_segment`] and the commit write it through one function, [`check_segment`]
//!   reads it. A segment never mixes namespaces; one no manifest names
//!   still says what it held; its index is an immutable object a remote source serves by range
//!   (`format::find_ranged`: about one 64 KiB read per lookup in a shard of a million).
//! - `flip`: the lock file. `CACHEDIR.TAG`: the cache directory tag; on macOS the directory
//!   also carries Time Machine's exclusion attribute ([`Driven`]).
//!
//! # Durable names
//!
//! THE INVARIANT: the store's folder and every folder above it that the store made are
//! durable before the first head slot is. The first effect a store asks in a folder whose
//! head is short or missing is the durable `Effect::Write` of `CACHEDIR.TAG` (core.rs
//! `HEAD_MAKE` 0), which makes each missing folder of its path, from the top, and syncs each
//! one's name into its parent (blaze-core `make_parent_durably`): a store three levels down
//! (`.blaze/v2/store/f5s3`) a fresh home makes whole is as durable as one in a folder its
//! caller made. Only then is the head made (a Truncate, which then makes no folder), synced,
//! and its name synced into the store's folder; a SyncDir of a folder makes its CONTENTS'
//! names durable, never its own, which is why the tag's write comes first. Below that, every
//! name the store makes is a file in its own folder (segments, `manifests-<id>`, the lock, the
//! tag), and a durable flip syncs the folder before its slot (`DIR_SYNC`), so a slot never
//! names a file a power cut can take. No control of the store's plants this: since blaze-core's
//! folders rule (2026-11-20, on `Effect`: every effect that makes a folder, `WriteUnsynced` and
//! `Truncate` too, syncs its name into its parent before it completes) no effect the store can
//! ask makes a folder a power cut takes, so the store's former `Plant::FolderNotDurable` (the
//! tag through `WriteUnsynced`) was harmless and caught nothing. The rule's control is
//! blaze-core's (`sim/host.rs`, `Plant::MadeParentsNotSynced`, with `sim/host/dir_names/tests.rs`).
//!
//! # A commit, in order
//!
//! 1. The store's batches and the workers' [`Batch`]es (each filled by one worker for one
//!    namespace, no shared word) are laid end to end as one segment per namespace that holds
//!    records: no copy of a worker's bytes, which move into their writes. Each index is sorted
//!    once (n log n on its entries).
//! 2. Each `seg-<id>` in turn is made at its length and every piece written at its offset, in
//!    parallel. No lock is held. No sync (durable mode: one sync of each segment here).
//! 3. The flip lock is taken (`flock`, never waiting in the kernel: a timer between asks,
//!    jittered and doubling from 100 us to 2 ms, stopped by name once the steps sum to 60 s:
//!    core.rs `lock_step`). Under it: the head is read again, and the manifest
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
//! it, a process crash loses none of it); a segment of an earlier boot whose row is sealed is
//! proved by the row too ([`Driven::seal`], the exit tail: a barrier ordered its bytes before
//! the flip that sealed it); an unsealed segment of an earlier boot is checked in full
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
//! Each segment is mapped once per process and never unmapped during a run; no page of it is
//! touched at the open (store-next.md: a repeat reads the pages its lookups probe). A lookup names the namespaces it asks, in order
//! (this machine's, the organisation's, the public one: [`Settings::reads`]), and searches no
//! other; within each it probes the index of each segment, newest first, with no lock and no
//! write to anything shared ([`PlainStore`], blaze-keyed's `Store`): up to three
//! interpolation steps on the key's first 8 bytes between bounds the probes narrow (no read
//! of the run's ends), then bisection. [`Driven::get`] returns the payload in place (the
//! mapping); [`Driven::get_into`] reads it with one `pread` into the caller's buffer. Per
//! lookup: 0 syscalls; a fault for each index page a probe lands on first (one or two a
//! segment, from the page cache when warm), plus for `get_into` one `pread`, for `get` the
//! payload's pages faulted on first touch; no checksum of any kind.
//!
//! A pool reads through a [`Snapshot`] ([`Lent::snapshot_in`], after a commit): the
//! generation's mapped segments, their mappings shared, `Send + Sync`, read with `&self` alone;
//! what is committed after it, and the command's puts not yet committed, are not in it.
//!
//! # Compaction ([`Driven::compact`])
//!
//! Run by a command after it has answered, or by its own invocation, on a lowered thread
//! (utility QoS on macOS; nice 19 and the idle IO class on Linux); never before a build's
//! first read. A slice: the directory is listed (files no generation names are retired, so
//! what a crash left is swept a grace period later); while the segments hold more than the
//! cap, the oldest segments older than [`WINDOW`] generations, at most [`SLICE_BYTES`], are
//! copied keeping what roots name (pruning and sweeping are all [`SLICE_BYTES`] bounds); else
//! a namespace with a tier over `K` segments gets the size-tiered merge (`core::Tiers`,
//! keeping everything), which [`SLICE_BYTES`] does not cut: the invariant needs every row of a
//! tier. Tier `t` holds segments of `[B * G^t, B * G^(t+1))` (tier 0 from 0; `B` 1 MiB, `G` 4,
//! `K` 4: `TIER_FLOOR_BYTES`, `TIER_GROWTH`, `TIER_SEGMENTS_MOST`). THE BOUND: after a cleanup,
//! a namespace of `S` bytes names at most `K` segments in each of its open tiers (those under
//! 1 GiB), one closed row (1 to 4 GiB, never merged again: no segment passes the format's
//! 4 GiB, and no merge is planned that could) for each GiB, and the flip's commit
//! (`segments_most`; [`SEGMENTS_MOST`], 276, under 256 GiB), what a miss probes; the bytes merges copy are
//! amortised to at most `16 W + 1.25 MiB * C` over `C` commits of `W` bytes, all after the
//! answer. The copy is written with no lock; the
//! flip replaces them only if they are all still named (else the slice aborts and removes its
//! copy), and retires them. A retired file is unlinked [`GRACE_NS`] after: a process that read
//! a head maps what it names within milliseconds, and a mapping outlives an unlink. A
//! published segment is never rewritten: a merge leaves it, and pruning only stops holding it
//! here.
//!
//! A [`Liveness::Reached`] store keeps only what reaches from its roots, each
//! with an expiry ([`ROOT_LIFE_NS`] for a checkout's answer), and what a running command's
//! lease keeps ([`LEASE_NS`] past its last commit, so a process that dies holding one is let
//! go of by the clock). A slice rewrites a run from a segment under half live, so the store
//! holds at most twice its live bytes and a slice. A command's slice is folded into its own
//! commit after the answer ([`Driven::fold_slice`]): written with no lock, named by the flip
//! the command makes anyway, so no command ever waits on cleanup. A retired file also waits
//! for every live lease whose holder opened before it was retired. A commit whose roots name
//! a record a slice swept since it was read carries the record forward from its own mapping
//! into a segment of its own and flips again (at most [`CARRY_ROUNDS`]), so no command's
//! work after its answer is refused or lost.
//!
//! A root may name INDEXES instead of records ([`Plain::root_through`], its record filed under
//! blaze-keyed's `ReservedTag::IndexedRoot`): an index ([`Plain::index`], `ReservedTag::Index`) is one record of sorted
//! keys, keyed by its bytes, so a program that reached 100,000 records is kept by a root of
//! one key, and a set many roots share (a toolchain's compiled code) is one record. Reach
//! follows one level: the root's record, each index, every key each index holds.
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

pub mod bottom;
mod arena;
mod bloom;
mod core;
mod driven;
mod format;
mod handle;
mod image;
mod lent;
mod snapshot;
mod verified;

#[cfg(test)]
mod aligned_tests;
#[cfg(test)]
mod arena_tests;
#[cfg(test)]
mod cleanup_tests;
#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod beside_tests;
#[cfg(test)]
mod bloom_tests;
#[cfg(test)]
mod bottom_tests;
#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod lazy_tests;
#[cfg(test)]
mod lock_wait_tests;
#[cfg(test)]
mod phase_commit_tests;
#[cfg(test)]
mod pointer_tests;
#[cfg(test)]
mod reserved_tag_tests;
#[cfg(test)]
mod root_commit_tests;
#[cfg(test)]
mod tests;

/// A test's allocator: a frame the test makes at its top and owns, handed down to every helper
/// that builds the test's world (rule 75: no test reaches for the process's heap). Its blocks
/// come from blaze-mem's own regions and go back when the frame drops, after everything made in
/// it; a bump frame frees nothing before that, which no test's assertion reads.
#[cfg(test)]
pub(crate) fn test_root() -> blaze_mem::Frame {
    blaze_mem::Frame::new(blaze_mem::Tag::Store).expect("a test's frame")
}

pub use self::core::{
    Batch, CACHEDIR_SIGNATURE, CACHEDIR_TAG, CARRY_ROUNDS, Committed, DEFAULT_MAX_STORE_BYTES, Found, GRACE_NS, GcPlan, Got, INDEX_KEYS_MOST, LEASE_NS, LentRecord, Liveness, LockStep, LockWaited,
    Lost, MIN_MAX_STORE_BYTES, Named, POINTER_LEN, Placed, Plain, PlainConfig, Probe, Probes, PutError, PutPath, ROOT_INDEXES_MOST, ROOT_LIFE_NS, Renewed, Report, SEGMENTS_MOST, SLICE_BYTES,
    SWEEP_BYTES, Scrubbed, Sliced, TAGS, TOUCH_NS, WATERMARK_HIGH_PCT, WATERMARK_LOW_PCT, WINDOW, beside_key, store_leaf, watermark_bytes,
};
pub use self::arena::Visibility;
pub use self::lent::{Aligned16, Lend, Lent, LentCounts, LentError, Misaligned, NotInPlace, PutTally, first_difference};
pub use self::snapshot::{Proved, SNAPSHOT_SEGMENTS_MOST, Snapshot, SnapshotError};
pub use self::driven::{Driven, ExclusiveLock, HeadIdentity, LocalDir, lower_this_thread};
pub use self::format::{Described, Entry, FORMAT, FORMS, PlaceName, SEGMENT_FORMAT, Segment, check_segment, describe, find_many, record_crc, seal_segment};
pub use self::handle::{LANES_MOST, LaneId, PlainStore};
pub use self::image::{ImageError, Placement, StartupImage};
pub use self::settings::Settings;
pub use self::verified::{Checks, Proof};
// The vocabulary is blaze-keyed's (rule 30): re-exported where the remote lane looks for it.
pub use blaze_keyed::{
    kind::{Namespace, Tag},
    store::{ALIGN, ENTRY, FOOTER, HEADER, SEGMENT_MOST, Source, Verifier},
};
