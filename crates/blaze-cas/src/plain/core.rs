//! The plain store's state machine ([`super`] has the layout and the costs): opening (the
//! head and the manifest; a segment mapped at its first probe, an unsealed segment of another
//! boot checked whole at its first hit: `probe.rs`),
//! reads in place with no lock and no shared write, puts into per-worker [`Batch`]es, the
//! commit (one segment written without a lock, then the flip under the one lock), and
//! compaction: pruning and sweeping in bounded slices, and the size-tiered merge whose one
//! invariant ([`Tiers`]: no tier over `K` segments after a cleanup, so a namespace of `S` bytes
//! names at most [`segments_most`]`(S)`) the simulation checks after every cleanup. Beside it
//! the merge's ONE bound on memory, [`merge_holds_most`]`(k)`: whatever the tier and however
//! many records, a compaction's copy holds at most [`MERGE_BUFFER`] (8 MiB) of records in
//! flight, ONE [`INDEX_CHUNK`] (1 MiB) of its index, and [`CURSOR_BYTES`] for each of its `k`
//! inputs (a cursor over the input's mapped sorted run, and its place in the merge's heap),
//! `k` the slice's rows: at most [`MERGE_INPUTS_MOST`] (26) for a namespace that kept the
//! tier invariant, so 9 MiB and under 1.2 KiB. No merge's output passes the segment cap
//! ([`Tiers`], THE CAP), and no manifest names a row its open would refuse (`format::Row`). Nothing of it grows with the records: the copy is a
//! k-way merge of the inputs' runs, its records found in key order one at a time, its payloads
//! and then its index streamed into a temporary file, renamed to its digest's name before the
//! sync and the flip (`Plain::stream_segment`). The simulated driver checks the records and
//! the index in flight on every compaction's writes, and the core what it built at once.
//! Three walks of the merge (count, payloads, index) because the digest hashes the header
//! first; one walk needs format 3 (`H(payloads) ‖ H(run) ‖ header ‖ footer head`): decided
//! 2026-11-20 to take it with the per-worker writer (D159), so one format bump, not two. It
//! owns no IO: it hands back [`Effect`]s and takes their
//! [`Completion`]s, so the simulation runs this very code (`super::tests`), and
//! [`super::Driven`] carries the effects out on a real driver.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

// The process heap only where bytes cross into blaze-core's effects, which own process-heap
// vectors (`Effect::Append { bytes: Vec<u8> }`): a batch's payloads, a segment's pieces, a
// manifest's encoding. Everything else the store keeps is in the caller's `A` (rule 75).
use std::alloc::Allocator;
use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use blaze_core::{Completion, Done, Effect, Extent, FileName, LockMode, Tag as EffectTag};
use blaze_hash::{BuildKeyHasher, Map, MapOpts, hash_map::Entry as HashEntry};
use blaze_ids::{ContentId, crc32c};
use blaze_mem::Kept;
use blaze_own::Share;
use blaze_keyed::{
    kind::{Namespace, ReservedTag, Tag, TagReach, reach_of},
    store::{ALIGN, ENTRY, FOOTER, HEADER, Holder, Put, RowRefused, SEGMENT_MOST, SegmentId, StoreError, StoreFile, TwoValues},
};

use super::bloom::{BLOOM_ALLOC, BloomBuilder, BloomIndex, BloomMapped, BloomRef};
use super::format::{
    self, Entry, FORMAT, HEAD_LEN, HeadRead, Lease, LostKind, LostMark, Manifest, NEVER, OFFSET_MOST, Retired,
    Root, SLOT_AT, Segment, Slot, aligned, record_crc,
};
use super::verified::{Boot, Checks, Unverified, Verified};
use super::lent::two_values;
use crate::{Corrupt, Key};

/// The lazy open's reads: [`Plain::find`], [`Plain::map_all`], the probe order.
mod probe;
pub use probe::{Found, Probe, Probes};

// The store's own tags are blaze-keyed's reservations ([`ReservedTag`], the one registry):
// - a root's record ([`ReservedTag::Root`]): a root's sorted keys, always under
//   [`Namespace::LOCAL`];
// - a POINTER ([`ReservedTag::Pointer`]): a record whose key is a name and whose payload is the
//   32-byte key of another record, its target ([`super::Lent::point`]). The one record a newer
//   commit may REPLACE: two values under one name are the old and the new, never refused, and
//   the value in the newest row of the manifest holding the name is the pointer's
//   ([`Plain::find_newest`]). Every other tag keeps the two-values refusal (rule 21). A pointer
//   lives in [`Namespace::LOCAL`] only; compaction keeps only its newest value;
// - an INDEX ([`ReservedTag::Index`], [`Plain::index`]): sorted 32-byte keys under
//   [`Namespace::LOCAL`], keyed by its bytes. It keeps nothing by itself; reached through an
//   indexed root, it and every key it holds are live. One level: an index naming an index
//   keeps the inner index's record, not what that holds;
// - an indexed root's record ([`ReservedTag::IndexedRoot`], [`Plain::root_through`]): sorted
//   keys, each an index. A slice reads a root's record anyway; what its tag is to reach
//   ([`reach_of`]) says whether to follow it, and a record followed must be an index by tag.

/// A pointer's payload: its target's key.
pub const POINTER_LEN: usize = 32;

/// The most keys one index holds (128 MiB of keys): a program reaching more is refused by
/// name ([`StoreError::IndexOver`]), never cut.
pub const INDEX_KEYS_MOST: usize = 1 << 22;

/// The most indexes one indexed root names.
pub const ROOT_INDEXES_MOST: usize = 1 << 16;

/// The most records one plain root names directly (as many as an indexed root names indexes):
/// each is looked for when the root is set (super::driven `Driven::root_names_held`, D200), so
/// a set past it goes through an index ([`Plain::root_through`]). Over it is refused by name
/// ([`StoreError::IndexOver`]), never cut.
pub const ROOT_KEYS_MOST: usize = ROOT_INDEXES_MOST;

/// The tags a store asks with: `tags .. tags + TAGS`.
pub const TAGS: u64 = 1 << 40;

/// A file no generation names is unlinked this long after it left the last manifest that
/// named it (or was found named by none): the bound on how long a process may take from
/// reading the head to mapping what it names, and on a commit from starting its segment to
/// its flip (half of it: [`Plain::commit`]). Mappings outlive an unlink, so a process that
/// mapped a file keeps reading it whatever happens after.
pub const GRACE_NS: u64 = 10 * 60 * 1_000_000_000;

/// Records no root names are kept for this many generations after the one that put them
/// ([`Liveness::Everything`]; a [`Liveness::Reached`] store keeps what a lease keeps instead).
pub const WINDOW: u64 = 16;

/// How long a checkout's root keeps what it names after it was last set
/// ([`Plain::root_for`]): a checkout not built for this long lets go of its records, and the
/// slices after reclaim them.
pub const ROOT_LIFE_NS: u64 = 14 * DAY_NS;

/// A root set again with the same key is written again (one flip) only when less than its
/// life less this is left: a checkout built a hundred times a day touches its root once.
pub const TOUCH_NS: u64 = DAY_NS;

const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// Default maximum byte budget for a developer's global store (32 GiB).
pub const DEFAULT_MAX_STORE_BYTES: u64 = 32 << 30;

/// Minimum sensible non-zero store byte budget enforced at the CLI/env edge (1 GiB).
pub const MIN_MAX_STORE_BYTES: u64 = 1 << 30;

/// Soft/high watermark percentage of the byte budget (85%): root-LRU eviction begins once
/// committed segment bytes and reachable live bytes exceed this watermark.
pub const WATERMARK_HIGH_PCT: u64 = 85;

/// Low watermark percentage of the byte budget (75%): root-LRU eviction expires oldest roots
/// down toward this watermark for hysteresis, preventing thrashing on every command.
pub const WATERMARK_LOW_PCT: u64 = 75;

/// Computes `pct`% of `cap_bytes` using exact integer arithmetic with zero narrowing casts.
/// `u64::MAX` (unlimited) remains `u64::MAX`; tiny test caps under 100 bytes return `cap_bytes`.
pub const fn watermark_bytes(cap_bytes: u64, pct: u64) -> u64 {
    if cap_bytes == u64::MAX || cap_bytes < 100 {
        return cap_bytes;
    }
    (cap_bytes / 100).saturating_mul(pct).saturating_add((cap_bytes % 100).saturating_mul(pct) / 100)
}

/// How long a running command's lease ([`format::Lease`]) lasts past its last commit: a
/// process that dies holding one is let go of this long after, by the clock the flips pass in.
pub const LEASE_NS: u64 = 60 * 60 * 1_000_000_000;

/// The bytes of segments one [`Liveness::Reached`] slice looks at for their live share, from
/// a place that moves with each generation: what bounds a slice's reading of indexes.
pub const SWEEP_BYTES: u64 = 2 * SLICE_BYTES;

/// How many times one commit carries forward records its roots name whose segment a slice
/// replaced since the command read it ([`Plain::carried`]), before
/// [`StoreError::SweptUnder`] stops it (rule 18: a retry has a count). Why 3: a round is owed
/// only when another process's slice flipped between this commit's read and its flip, and a
/// carried record lies in a segment of this commit's, which no slice can have planned on;
/// what can still go is a record of ANOTHER segment this commit reuses, swept by a second
/// slice landing in the next few milliseconds (each command folds at most one slice, after
/// its answer, from a place that moves with the generation). One round is the expected case,
/// two is two commands' slices meeting one commit, and a third means cleanup lands faster
/// than one commit can flip: that is a store being swept by many processes at once, named
/// by the refusal rather than chased. The rounds cost at most 3 x the carried bytes.
pub const CARRY_ROUNDS: u32 = 3;

/// The tiers' floor `B`: tier 0 holds segments under `B * G` (4 MiB), tier `t >= 1` those of
/// `[B * G^t, B * G^(t+1))`. Why 1 MiB: the small commits of an edit loop (a cleanup commit's
/// records and roots) share tier 0 and are merged at most 4 MiB at a time (the amortisation's
/// `B` a commit, [`Tiers`]); a smaller floor adds a tier (`K` more segments a miss probes) for
/// each factor `G` it goes down, a larger one more bytes merged a commit. Not measured: a
/// choice to revisit with the profile of a real edit loop.
pub const TIER_FLOOR_BYTES: u64 = 1 << 20;

/// The tiers' growth `G`: each tier's segments are `G` times the size of the one below. Why 4:
/// with `K = G` the bound `K * log_G(S / B)` is near its least over `G` (`G / ln G` is least at
/// `e`; 4 is within 6% of it), and a merge of `K + 1` rows of one tier always lands above it.
pub const TIER_GROWTH: u64 = 4;

/// The most segments one tier of a namespace holds after a cleanup (`K`): a tier at `K + 1`
/// is merged.
pub const TIER_SEGMENTS_MOST: usize = 4;

/// What one namespace's offsets reach: 16 TiB (`format::OFFSET_MOST`), the tiers the
/// amortisation counts ([`Tiers::merged_most`]).
pub const NAMESPACE_BYTES_MOST: u64 = OFFSET_MOST;

const _: () = assert!(NAMESPACE_BYTES_MOST == TIER_FLOOR_BYTES << 24, "16 TiB is B * G^12: twelve tiers, 0 to 11");

/// The namespace size [`SEGMENTS_MOST`] is stated for: 256 GiB, past the stores of 25 to
/// 100 GB a home is sized for. A larger namespace keeps the invariant as stated, with
/// [`segments_most`] of its size (a closed row more for each GiB: [`Tiers`], THE CAP).
pub const NAMESPACE_BYTES_STATED: u64 = 256 << 30;

/// THE BOUND (the invariant's, [`Tiers`]): the most segments one namespace of `bytes` names
/// after a cleanup completes: `K` a tier for each OPEN tier a segment of at most `bytes` can
/// be in, one CLOSED row (at least 1 GiB) for each GiB of `bytes`, and the one segment the
/// flip that names the cleanup commits.
pub const fn segments_most(bytes: u64) -> usize {
    Tiers::STORE.segments_most(bytes)
}

/// [`segments_most`] of a namespace under [`NAMESPACE_BYTES_STATED`]: 276 (five open tiers of
/// four, 255 closed rows, and the commit's own). The one cap of a namespace's segments
/// (blaze-cli's "compaction is behind" line counts against it, through [`Plain::named_most`],
/// and the snapshot's table cap is a multiple of it); a miss probes at most this many segments
/// a namespace of that size once cleanup has run (about 27 at 25 GB and 45 at 100 GB in
/// practice: at most 20 open rows, and a split lands near the 4 GiB cap).
pub const SEGMENTS_MOST: usize = segments_most(NAMESPACE_BYTES_STATED - 1);

const _: () = assert!(SEGMENTS_MOST == 276);

/// The most rows one tiered merge of a namespace that kept the invariant takes: `K + 1` in
/// each open tier (five at 4 GiB) and the flip's commit. Closed rows are never an input.
pub const MERGE_INPUTS_MOST: usize = (TIER_SEGMENTS_MOST + 1) * (Tiers::STORE.tier(SEGMENT_MOST) - 1) + 1;

const _: () = assert!(MERGE_INPUTS_MOST == 26);

/// What one pruning or sweeping slice reads and writes at most, unless one segment alone is
/// larger (then that segment is the slice). A tiered merge (`Tiers`) is not cut by it: the
/// invariant needs every row of a tier, and the amortisation bounds its bytes instead.
pub const SLICE_BYTES: u64 = 64 << 20;

/// THE MERGE'S MEMORY BOUND: the most bytes of copied records a compaction's copy (a tiered
/// merge's, a pruning or sweeping slice's) holds at once, whatever its tier or size. The copy
/// is streamed from the inputs' mappings into its file in chunks of half this
/// ([`MERGE_CHUNKS_IN_FLIGHT`] in flight, so the next chunk is copied while the last is
/// written); its index and footer are written after the last chunk, then the sync, then the
/// flip. Why 8 MiB: a cleanup runs after the command's answer and must never be its peak
/// (memory is the goal's second term), and a command's own working set is hundreds of MiB, so
/// 8 MiB is noise beside it; and a chunk of 4 MiB is a write whose fixed cost (one effect's
/// round trip, tens of microseconds) is under 2% of its copy into the page cache at a few
/// GB/s, so the stream runs within 10% of one whole write's speed (the floor: the bytes
/// copied once, at memory bandwidth, and synced once). The simulation's driver checks it
/// (`super::tests`, the merge meter). The whole bound, the index's part included, is
/// [`merge_holds_most`].
pub const MERGE_BUFFER: u64 = 8 << 20;

/// The most bytes of a streamed copy's index alive at once: the one chunk of entries the core
/// builds, hashes and hands to its write, the next built only once that write completed. Why
/// 1 MiB: 21 845 entries a write, so a merge of ten million records writes its 460 MiB index in
/// 440 writes, each a round trip of tens of microseconds against a copy into the page cache of
/// a few hundred, under 10% over one whole write (the floor) as for [`MERGE_BUFFER`]'s chunks;
/// and an eighth of that buffer, so the index is never the copy's peak.
pub const INDEX_CHUNK: u64 = 1 << 20;

/// What one input of a streamed copy costs while it runs: its cursor over its mapped sorted
/// run and its place in the merge's heap.
pub const CURSOR_BYTES: u64 = (size_of::<Cursor>() + size_of::<u32>()) as u64;

/// THE MERGE'S ONE MEMORY BOUND, for a copy of `inputs` rows: the records in flight
/// ([`MERGE_BUFFER`]), one chunk of its index ([`INDEX_CHUNK`]), and a cursor an input
/// ([`CURSOR_BYTES`]). `inputs` is the slice's rows: at most [`MERGE_INPUTS_MOST`] for a
/// namespace that kept the tier invariant, and a split merge's at most what fits the segment
/// cap (a namespace that fell behind, one row a commit since
/// its last cleanup, still costs only 20 bytes a row). Beside it only the stream's fixed state
/// (its digest's hasher, under 2 KiB), whatever the merge; and what a PRUNING slice keeps by
/// (a [`Liveness::Reached`] sweep's roots, where they lie: a few words a root; a
/// [`Liveness::Everything`] store's pruning over its cap, the set of keys its roots name, which
/// is not bounded here). The simulated driver checks the first two on every compaction's writes
/// and the core what it built at once (`super::tests`, the merge meter).
pub const fn merge_holds_most(inputs: usize) -> u64 {
    MERGE_BUFFER + INDEX_CHUNK + inputs as u64 * CURSOR_BYTES
}

const _: () = assert!(CURSOR_BYTES <= 24 && merge_holds_most(MERGE_INPUTS_MOST) <= MERGE_BUFFER + INDEX_CHUNK + 1200, "the module doc says 1.2 KiB of cursors");

/// The chunks of a streamed copy in flight at once, each at most `MERGE_BUFFER / 2` bytes:
/// two, so the core copies one while the driver writes the other.
pub const MERGE_CHUNKS_IN_FLIGHT: u32 = 2;

const _: () = assert!(MERGE_BUFFER % (MERGE_CHUNKS_IN_FLIGHT as u64 * ALIGN) == 0, "a chunk is whole multiples of ALIGN");

/// The tiers a length is sorted into, at most: `G^64` is past `u64`.
const TIERS_MOST: usize = 64;

/// One row of a namespace as the tier policy sees it: its file's length, and whether a merge
/// may take it (held here, mapped, not published, kept whole by no lease).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TierRow {
    pub len: u64,
    pub mergeable: bool,
}

/// What the tier policy merges: the rows (indices into the list it was given, in its order),
/// the output's length if nothing is dropped, and whether a tier would still be over `K` after
/// it (another slice has work: [`Sliced::more`]). The store reads the rows; the simulation all.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct TierMerge<A: Allocator> {
    pub rows: Vec<usize, A>,
    pub out_len: u64,
    pub more: bool,
}

/// The planted bug of the tier policy's control (`super::tests`): outside tests, none.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TierPlant {
    None,
    /// Today's (before D159) refusal: a merge whose second segment would carry it past
    /// [`SLICE_BYTES`] is thrown away, so once a namespace's oldest pair passes 64 MiB nothing
    /// is merged again.
    RefusedPastSlice,
    /// The policy as it was until 2026-11-20 (the cold tempo check's store): blind to the
    /// segment cap, so a namespace behind by many large rows is planned as ONE merge whose
    /// output passes [`Tiers::segment_most`], and a row at the cap is merged again (tests,
    /// `the_cap_oracle_catches_a_merge_past_the_cap`).
    PastCap,
}

/// THE MERGE POLICY: size-tiered, with one invariant (rule 53).
///
/// THE INVARIANT. Tier `t` of a namespace holds its segments of length in
/// `[B * G^t, B * G^(t+1))` (tier 0 from 0): `B` = [`TIER_FLOOR_BYTES`] (1 MiB), `G` =
/// [`TIER_GROWTH`] (4). After a cleanup completes (the flip that names its slice has landed,
/// the slice not refused), no tier of the namespace it merged (of every namespace, when it
/// merged none) holds more than `K` =
/// [`TIER_SEGMENTS_MOST`] (4) mergeable segments, apart from the segment that same flip
/// commits there; a cleanup that stopped with a slice still to run (on its work budget or
/// slice count, or after a pruning slice, which ends a cleanup: [`Plain::cleanup_goes_on`])
/// leaves what is over to the next. A [`Liveness::Reached`] store plans the merge before any
/// sweep, and goes on to it after a sweep that left a namespace over, and after a first slice
/// that replaced nothing because the command's own lease kept its rows (the answer's commit
/// lets the lease go), so neither leaves a remainder. A store that writes one namespace (every machine but a trusted writer:
/// `Settings::writes`) so keeps it for every namespace after every cleanup. So a namespace of
/// `S` bytes names at most `K * (tier(S) + 1) + 1` segments ([`segments_most`]; 49 under the
/// 16 TiB of [`NAMESPACE_BYTES_MOST`], 33 at 20 GiB), what a miss probes. Rows a merge may not
/// take (published, not held here, kept whole by a live lease) are outside the count: a lease
/// ends with its command, and the next cleanup counts them.
///
/// THE MERGE. When a tier is over `K`, every tier from the lowest that holds `K` (`lo`: a copy
/// that drops much may land under the tier over, and must find room) to the highest over `K`
/// (`hi`) is merged whole, all its mergeable rows, into ONE segment; when it may land in a
/// tier above `hi` that already holds `K` (any tier up to its estimate's: a copy only drops),
/// that tier joins the merge (and those between), until every tier it may land in has room.
/// One slice so restores the invariant for its namespace however far behind it was (a crashed
/// command's commits, a cold check's hundred), and one slice a command (blaze-cli
/// `AFTER_ANSWER_SLICES`) keeps it. A slice writes one namespace's segment: with
/// several namespaces over, the one whose merge starts at the oldest row is planned first, so
/// each is restored within one cleanup for each namespace over before it.
///
/// THE AMORTISATION (bytes merged against bytes committed, `W`, over `C` commits). Let a byte's
/// potential be `11 - its tier` (twelve tiers). A merge's input is at least `5 * B * G^hi`, a
/// fourth past `B * G^(hi + 1)`: `K + 1` rows of tier `hi`, or, when the cascade made `hi` the
/// tier it joined, that tier's `K` rows and a lower part whose estimate reached it. So
/// unless it drops more than a fifth of its input (a pointer's older values, a duplicate), its
/// output lands above `hi` and every byte it keeps climbs a tier, so its kept bytes cost at
/// most the potential they spend (`11 W` over all merges). Its dropped bytes, and every byte of
/// a merge that drops over a fifth (at most five times what that merge dropped), cost at most
/// `5 W` together: a byte is dropped once. Tier 0 starts at 0: five rows there can total under
/// `B * G`, so a merge of tier 0 alone may stay in it dropping under a fifth; its input is then
/// under `5/4 B G`, and it removes `K` of the rows commits add one at a time, so such merges
/// cost under `5/4 B G / K` = `5/4 B` a commit. So bytes merged <= 16 W + 1.25 MiB * C
/// ([`Tiers::merged_most`]), all in the cleanup after the answer (`Plain::fold_slice`): no
/// answer waits on a merge.
///
/// THE CAP (2026-11-20; the cold tempo check's store, whose one merge of ~88 commit segments
/// of 256 MiB wrote a 22 GiB segment that every later open refused). A segment is at most `C`
/// = [`Tiers::segment_most`] bytes ([`SEGMENT_MOST`], 4 GiB; the format's, checked by
/// `format::Row::new` at every commit and open). So:
/// - A row of tier `tier(C) - 1` or above (at least `C / G`, 1 GiB: [`Tiers::closed`]) is
///   CLOSED: never merged again and outside every tier's count, as a published row is. Five
///   rows of that tier could not fit one segment.
/// - NO MERGE IS PLANNED WHOSE OUTPUT CAN PASS `C`: its output's estimate (the most it can be:
///   a copy only drops) is computed from its inputs before it starts; over `C`, the merge is
///   SPLIT: its rows taken oldest first while the estimate stays at most `C` (first fit), the
///   rest left for the next slice ([`TierMerge::more`]). Two open rows always fit (each under
///   `C / G`), and a split's output with nothing dropped is past `C - C / G`, so it lands in a
///   closed tier: every byte it keeps climbs, and the amortisation above holds.
/// - THE BOUND becomes `K * (open tiers used) + S / closed_from + 1` ([`Tiers::segments_most`]):
///   the open tiers, `tier(C) - 1` of them (five at 4 GiB: 0 to 4, under 1 GiB), keep `K`
///   each, and the closed rows are each at least `closed_from` (1 GiB). At 4 GiB: 25 GiB names
///   at most 20 + 25 + 1 = 46 (a split lands near `C`, so about 7 closed rows in practice),
///   100 GiB at most 121 (about 25 closed), 256 GiB at most 276 ([`SEGMENTS_MOST`]).
/// - [`merge_holds_most`] is unchanged in form: a split takes fewer rows, never more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Tiers {
    floor: u64,
    growth: u64,
    most: usize,
    /// `C`: the most bytes one segment may be.
    cap: u64,
    plant: TierPlant,
}

impl Tiers {
    /// The store's geometry: `B`, `G`, `K` as above, `C` the format's [`SEGMENT_MOST`].
    pub(crate) const STORE: Tiers = Tiers { floor: TIER_FLOOR_BYTES, growth: TIER_GROWTH, most: TIER_SEGMENTS_MOST, cap: SEGMENT_MOST, plant: TierPlant::None };

    /// A test's geometry: the store's `G` and `K` over another floor (a simulation of real
    /// bytes reaches several tiers in KiBs), the format's cap, with `plant`.
    #[cfg(test)]
    pub(crate) const fn planted(floor: u64, plant: TierPlant) -> Tiers {
        Tiers::capped(floor, SEGMENT_MOST, plant)
    }

    /// A test's geometry with its own segment cap `cap` (a simulation drives merges into a
    /// 64 KiB cap within a few hundred commits). At least one open tier: `cap >= B * G^2`.
    #[cfg(test)]
    pub(crate) const fn capped(floor: u64, cap: u64, plant: TierPlant) -> Tiers {
        assert!(cap >= floor * TIER_GROWTH * TIER_GROWTH, "a cap leaves one open tier at least");
        Tiers { floor, growth: TIER_GROWTH, most: TIER_SEGMENTS_MOST, cap, plant }
    }

    /// `C`: the most bytes one segment of this store may be, what `format::Row::new` checks.
    pub(crate) const fn segment_most(self) -> u64 {
        self.cap
    }

    /// The least length of a CLOSED row: `B * G^(tier(C) - 1)`, at most `C / G`.
    pub(crate) const fn closed_from(self) -> u64 {
        let mut at = self.floor;
        let mut tier = 1;
        while tier < self.tier(self.cap) {
            at = at.saturating_mul(self.growth);
            tier += 1;
        }
        at
    }

    /// A row of `len` bytes is closed: never merged again, outside the tiers' counts.
    pub(crate) const fn closed(self, len: u64) -> bool {
        len >= self.closed_from()
    }

    /// The most a copy of `n` rows of `bytes` bytes in all can be, nothing dropped: the
    /// payloads (each padded alone) and entries carry over, one header and footer replace each
    /// input's, and the index's padding is at most one `ALIGN`.
    const fn estimate(bytes: u64, n: u64) -> u64 {
        bytes.saturating_sub(n.saturating_sub(1) * (HEADER + FOOTER) as u64).saturating_add(ALIGN)
    }

    /// `K`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn most(self) -> usize {
        self.most
    }

    /// The tier of a segment of `len` bytes.
    pub(crate) const fn tier(self, len: u64) -> usize {
        let mut tier = 0;
        let mut top = self.floor.saturating_mul(self.growth);
        while tier + 1 < TIERS_MOST && len >= top {
            tier += 1;
            top = top.saturating_mul(self.growth);
        }
        tier
    }

    /// The invariant's bound for a namespace of `bytes`: `K` a tier for each OPEN tier up to
    /// that of `bytes`, a closed row for each [`Tiers::closed_from`] of `bytes`, and the flip's
    /// own commit.
    pub(crate) const fn segments_most(self, bytes: u64) -> usize {
        let open = self.tier(self.cap) - 1;
        let used = if self.tier(bytes) + 1 < open { self.tier(bytes) + 1 } else { open };
        self.most * used + (bytes / self.closed_from()) as usize + 1
    }

    /// The amortisation's bound: the most bytes merges may have taken after commits of
    /// `written` bytes in `commits` commits (`(tiers - 1 + 5) W + 5/4 B G / K C`).
    pub(crate) const fn merged_most(self, written: u64, commits: u64) -> u64 {
        let tiers = self.tier(NAMESPACE_BYTES_MOST - 1) as u64 + 1;
        let tier_zero = self.floor * self.growth * 5 / (4 * self.most as u64) + 1;
        (tiers - 1 + 5).saturating_mul(written).saturating_add(tier_zero.saturating_mul(commits))
    }

    /// The merge that restores the invariant over `rows` (one namespace's, in the manifest's
    /// order), none when it holds: see [`Tiers`]. The list in `alloc`, the caller's.
    pub(crate) fn merge<A: Allocator>(self, rows: &[TierRow], alloc: A) -> TierMerge<A> {
        let none = |alloc: A| TierMerge { rows: Vec::new_in(alloc), out_len: 0, more: false };
        // A closed row is outside the merge and the counts ([`Tiers`], THE CAP); the planted
        // policy of before is blind to the cap.
        let blind = self.plant == TierPlant::PastCap;
        let open = move |it: &TierRow| it.mergeable && (blind || !self.closed(it.len));
        let mut count = [0usize; TIERS_MOST];
        for row in rows.iter().filter(|it| open(it)) {
            count[self.tier(row.len)] += 1;
        }
        let Some(over) = (0..TIERS_MOST).find(|&t| count[t] > self.most) else { return none(alloc) };
        // From the lowest FULL tier: a copy that drops much may land under `over`, in a tier
        // that must then have room.
        let lo = (0..=over).find(|&t| count[t] >= self.most).expect("the tier over is full");
        let mut hi = (0..TIERS_MOST).rev().find(|&t| count[t] > self.most).expect("a tier is over");
        let taken = |lo: usize, hi: usize| rows.iter().filter(move |it| open(it) && (lo..=hi).contains(&self.tier(it.len)));
        // The output, nothing dropped ([`Tiers::estimate`]).
        let out_of = |lo: usize, hi: usize| {
            let (bytes, n) = taken(lo, hi).fold((0u64, 0u64), |(bytes, n), it| (bytes.saturating_add(it.len), n + 1));
            Tiers::estimate(bytes, n)
        };
        // The estimate is the most the output can be (a copy drops a pointer's older values and
        // re-lays one index, never adds), so the output lands in a tier at or under the
        // estimate's: every tier above `hi` up to it must have room, and the highest full one
        // joins the merge (with every tier under it) until all have.
        let mut out_len = out_of(lo, hi);
        while let Some(full) = (hi + 1..=self.tier(out_len)).rev().find(|&t| count[t] + 1 > self.most) {
            hi = full;
            out_len = out_of(lo, hi);
        }
        let mut picked = Vec::new_in(alloc);
        picked.extend(rows.iter().enumerate().filter(|(_, it)| open(it) && (lo..=hi).contains(&self.tier(it.len))).map(|(i, _)| i));
        if self.plant == TierPlant::RefusedPastSlice && picked.len() >= 2 && rows[picked[0]].len + rows[picked[1]].len > SLICE_BYTES {
            picked.clear();
            return TierMerge { rows: picked, out_len: 0, more: false };
        }
        if !blind && out_len > self.cap {
            // SPLIT ([`Tiers`], THE CAP): the planned rows oldest first, each taken while the
            // estimate stays at most `C` (first fit); the rest is the next slice's. Every open
            // row is under `C / G`, so the first two always fit and the split makes progress.
            let (mut bytes, mut n) = (0u64, 0u64);
            picked.retain(|&i| {
                let with = bytes.saturating_add(rows[i].len);
                if Tiers::estimate(with, n + 1) > self.cap {
                    return false;
                }
                bytes = with;
                n += 1;
                true
            });
            debug_assert!(n >= 2, "two open rows always fit one segment");
            return TierMerge { rows: picked, out_len: Tiers::estimate(bytes, n), more: true };
        }
        let mut after = count;
        for slot in after.iter_mut().take(hi + 1).skip(lo) {
            *slot = 0;
        }
        if blind || !self.closed(out_len) {
            after[self.tier(out_len)] += 1;
        }
        let more = after.iter().any(|&c| c > self.most);
        TierMerge { rows: picked, out_len, more }
    }
}

/// A manifest log takes no more manifests past this; the next flip starts another.
#[cfg(not(test))]
const LOG_MOST: u64 = 4 << 20;
#[cfg(test)]
const LOG_MOST: u64 = 16 << 10;

/// The flip lock's wait (thoughts2 5.3): [`Effect::Lock`] never waits in the kernel, so a
/// contended ask is asked again after a timer on the driver's wheel, which is the driver's
/// deadline wait (blaze-core `wait`, rule 44); the core only says how long. Until 2026-11-21
/// every step was a fixed 2 ms: a flip holds the lock for tens of microseconds, so a waiter
/// lost up to 2 ms to a lock let go after 50 us, and waiters that met one hold re-asked in
/// step with each other. The schedule now ([`lock_step`]): the `k`th contended ask of one flip
/// waits a jittered `min(FIRST << k, MOST)`: 50 to 100 us first, doubling to the cap after five
/// asks. The cap is today's 2 ms, so a long hold (a durable flip's syncs) is never asked of
/// later than it was; the jitter (half the step, drawn from the process's `nonce` and `k`: no
/// randomness of the core's own, rule 75, and equal under one seed in the simulation) parts
/// waiters that met one hold. The wait ends by its sum, not its count: past
/// [`LOCK_WAIT_MOST_NS`] summed over one flip's steps it stops the store, naming the lock.
const LOCK_STEP_FIRST_NS: u64 = 100_000;
/// The longest step ([`LOCK_STEP_FIRST_NS`]'s doc): the old fixed step.
const LOCK_STEP_MOST_NS: u64 = 2_000_000;
/// How long one flip's lock wait may sum to before it stops the store, naming the lock: 60 s.
const LOCK_WAIT_MOST_NS: u64 = 60_000_000_000;

/// How a flip waits between asks of a contended flip lock ([`PlainConfig::lock_step`]): one
/// seam, both strategies under the same tests (rule 78). Either one's wait ends at the same
/// summed bound, [`LOCK_WAIT_MOST_NS`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockStep {
    /// The jittered doubling from 100 us to 2 ms ([`lock_step`]): the default.
    Backoff,
    /// The old fixed 2 ms step (30,000 asks to the bound): the control a test compares the
    /// backoff against.
    Fixed,
}

impl LockStep {
    /// The `k`th (from 0) contended ask's wait of a flip in the process whose nonce is `nonce`.
    pub fn step_ns(self, nonce: u64, k: u32) -> u64 {
        match self {
            LockStep::Backoff => lock_step(nonce, k),
            LockStep::Fixed => LOCK_STEP_MOST_NS,
        }
    }
}

/// The `k`th (from 0) contended ask's wait of a flip in the process whose nonce is `nonce`: a
/// pure function (sans-IO), in `[base / 2, base]` with `base = min(FIRST << k, MOST)`.
pub(super) fn lock_step(nonce: u64, k: u32) -> u64 {
    let base = LOCK_STEP_FIRST_NS.checked_shl(k.min(32)).unwrap_or(u64::MAX).min(LOCK_STEP_MOST_NS);
    // splitmix64 of the pair: a fixed mix, no hasher's seed (rule 75).
    let mut z = nonce ^ u64::from(k).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    base / 2 + z % (base / 2 + 1)
}

/// The sum of a flip's first `asks` waits ([`lock_step`]): the tests' oracle for the running
/// sum a flip keeps (`Flip::waited_ns`), which the bound is checked against.
#[cfg(test)]
pub(super) fn lock_waited_ns(nonce: u64, asks: u32) -> u64 {
    (0..asks).map(|k| lock_step(nonce, k)).fold(0u64, u64::saturating_add)
}

/// What this process's flips waited on the flip lock ([`Plain::lock_waited`]): the contended
/// asks and the steps' summed time, for the command's profile line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LockWaited {
    pub asks: u32,
    pub ns: u64,
}

/// What a root's record key is derived under.
const ROOT_DOMAIN: &str = "blaze plain store root 1";
const LOG_DOMAIN: &str = "blaze plain store log 1";
/// What an index's record key is derived under ([`Plain::index`]).
const INDEX_DOMAIN: &str = "blaze plain store index 1";
/// What an indexed root's record key is derived under: never a plain root's key for the same
/// bytes ([`Plain::root_through`]).
const INDEXED_ROOT_DOMAIN: &str = "blaze plain store indexed root 1";
/// The domain of the key an index beside a record is filed under ([`Plain::index_beside`]).
const BESIDE_DOMAIN: &str = "blaze plain store index beside 1";

/// The key of the index filed beside `owner` ([`Plain::index_beside`]).
pub fn beside_key(owner: Key) -> Key {
    Key(ContentId::of(BESIDE_DOMAIN, &owner.0.0))
}

const HEAD: &str = "head";
const FLIP: &str = "flip";
/// Optional rebuildable per-segment Bloom filter sidecar beside `head` and `flip`.
const BLOOM: &str = "bloom";
/// The cache directory tag's file in the store's directory: also how `blaze clean --store`
/// tells a directory is blaze's store (blaze-cli clean.rs).
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// What [`StoreError::Io`] says a failed map was doing.
const MAPPING: &str = "mapping a segment its generation names";

/// The cache directory tag (bford.info/cachedir): backup and indexing tools that honour it
/// skip the store. Its first two lines (the standard signature, and "created by blaze") are
/// what `blaze clean --store` requires before it removes a directory.
pub const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by blaze: its plain store.\n# For information about cache directory tags see https://bford.info/cachedir/\n";

/// What a store is opened on, its lists in the caller's allocator `A` (the store's own).
pub struct PlainConfig<A: Allocator + Clone> {
    /// Its directory, under the driver's root.
    pub dir: FileName,
    /// Its tags are `tags .. tags + TAGS`.
    pub tags: u64,
    /// Compaction lets go of records no root names, older than [`WINDOW`] generations, while
    /// the segments hold more than this. `u64::MAX` prunes nothing.
    pub cap_bytes: u64,
    /// The machine's boot, as the driver read it (a hash of Linux's `boot_id`, macOS's
    /// `kern.bootsessionuuid`): a segment of another boot is checked whole once.
    pub boot: u64,
    /// This process's, drawn at random: two processes never name a segment alike.
    pub nonce: u64,
    /// Sync each segment, log and head before a commit completes. Off on the edit path: the
    /// store is a cache rebuilt from what made it; a caller that needs a commit to survive a
    /// power cut (none today) turns it on.
    pub durable: bool,
    /// What names this machine, as bytes a record could hold (the checkout's, the home's and
    /// the store's paths, in each spelling the caller gives): a put into any namespace but
    /// [`Namespace::LOCAL`] whose bytes hold one is refused, naming the key and the offset
    /// (rule 25, sealed sharing: nothing of the machine leaves it). Empty: nothing is scanned.
    pub machine: Vec<Vec<u8, A>, A>,
    /// The namespaces a put may name, [`Namespace::LOCAL`] always among them (the store's roots
    /// live there): only `LOCAL` on a machine that is not a trusted writer (every developer's);
    /// a trusted builder (CI) adds [`Namespace::PUBLIC`] and its organisation's. A commit writes
    /// one segment for each that has records.
    pub writes: Vec<Namespace, A>,
    /// The namespaces [`Plain::get`] answers from, in the order it asks them; no other is
    /// searched.
    pub reads: Vec<Namespace, A>,
    /// What a compaction slice keeps.
    pub liveness: Liveness,
    /// How a flip waits on a contended flip lock: [`LockStep::Backoff`] everywhere but a test's
    /// control.
    pub lock_step: LockStep,
}

impl<A: Allocator + Clone> PlainConfig<A> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn empty_machine(alloc: A) -> Vec<Vec<u8, A>, A> { Vec::new_in(alloc) }
}

// By hand: a derive would ask the allocator for `PartialEq` and `Debug`, which a handle lacks.
impl<A: Allocator + Clone> Clone for PlainConfig<A> {
    fn clone(&self) -> PlainConfig<A> {
        PlainConfig {
            dir: self.dir.share(),
            tags: self.tags,
            cap_bytes: self.cap_bytes,
            boot: self.boot,
            nonce: self.nonce,
            durable: self.durable,
            machine: self.machine.clone(),
            writes: self.writes.clone(),
            reads: self.reads.clone(),
            liveness: self.liveness,
            lock_step: self.lock_step,
        }
    }
}

impl<A: Allocator + Clone> PartialEq for PlainConfig<A> {
    fn eq(&self, other: &PlainConfig<A>) -> bool {
        self.dir == other.dir
            && self.tags == other.tags
            && self.cap_bytes == other.cap_bytes
            && self.boot == other.boot
            && self.nonce == other.nonce
            && self.durable == other.durable
            && self.machine == other.machine
            && self.writes == other.writes
            && self.reads == other.reads
            && self.liveness == other.liveness
            && self.lock_step == other.lock_step
    }
}

impl<A: Allocator + Clone> Eq for PlainConfig<A> {}

impl<A: Allocator + Clone> std::fmt::Debug for PlainConfig<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainConfig")
            .field("dir", &self.dir)
            .field("tags", &self.tags)
            .field("cap_bytes", &self.cap_bytes)
            .field("boot", &self.boot)
            .field("nonce", &self.nonce)
            .field("durable", &self.durable)
            .field("machine", &self.machine)
            .field("writes", &self.writes)
            .field("reads", &self.reads)
            .field("liveness", &self.liveness)
            .field("lock_step", &self.lock_step)
            .finish()
    }
}

/// What a compaction slice counts as live.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    /// Everything, but what pruning over [`PlainConfig::cap_bytes`] lets go of: records no root
    /// names, older than [`WINDOW`] generations. Merges copy dead records forward: a store whose
    /// callers root nothing (the keyed handle's, the tests').
    Everything,
    /// Only what reaches from the store's roots: each key an unexpired root names, the roots'
    /// own records, and every segment a live lease keeps (a running command's, not yet rooted).
    /// A slice rewrites a run of segments from one under half live, so the store holds at most
    /// twice its live bytes and a slice; over the cap, from one with any dead record. A
    /// pointer row is live at its newest value only, and only when a root reaches it. For a
    /// store whose callers root everything a later process reads by key (the command's, once
    /// its code store roots its entries: blaze-cli plain_place.rs).
    Reached,
}

/// A record read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Got<'a> {
    pub tag: Tag,
    pub namespace: Namespace,
    pub bytes: &'a [u8],
}

/// Where a committed record lies in a proved segment, for a read into the caller's buffer:
/// the bytes read there are the record, with nothing to check.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Placed {
    pub key: Key,
    pub tag: Tag,
    pub namespace: Namespace,
    /// Its segment's digest.
    pub segment: ContentId,
    pub file: FileName,
    pub at: u64,
    pub len: u32,
}

/// Why a put was refused. A commit's, a compaction's and the store's own stops are
/// [`StoreError`]s (blaze-keyed's, the one home: rule 30); a put keeps its own type only
/// because a mismatch is an answer there ([`Put::Other`]), not a failure.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PutError {
    /// The key holds other bytes: both sides and where they first differ.
    Mismatch(TwoValues),
    /// A value of 4 GiB or more, or a batch past what an entry's offset holds.
    TooLong { key: Key, len: u64 },
    /// A put names a namespace this store does not write ([`PlainConfig::writes`]): a machine
    /// that is not a trusted writer writes only [`Namespace::LOCAL`].
    NotWritable { namespace: Namespace },
    /// A record for a shared namespace holds a path of this machine
    /// ([`PlainConfig::machine`]): its bytes at `at` begin needle `needle` of the list.
    Machine { key: Key, namespace: Namespace, at: u64, needle: u32 },
}

/// Where the first of `needles` occurs in `bytes`, and which one: the scan of a record before
/// it may go to a shared namespace (rule 25). Each needle is looked for from each place its
/// first byte occurs; none empty is ever found.
pub(crate) fn machine_in<B: Allocator>(needles: &[Vec<u8, B>], bytes: &[u8]) -> Option<(u64, u32)> {
    let mut first: Option<(u64, u32)> = None;
    for (i, needle) in needles.iter().enumerate() {
        let Some(&lead) = needle.first() else { continue };
        let found = bytes.iter().enumerate().filter(|(_, b)| **b == lead).map(|(at, _)| at).find(|&at| bytes[at..].starts_with(needle));
        if let Some(at) = found
            && first.is_none_or(|(was, _)| (at as u64) < was)
        {
            first = Some((at as u64, i as u32));
        }
    }
    first
}

impl PutError {
    /// The refusal of a put of `tag`, as the store's error: a mismatch outside a put (a commit
    /// laying out two batches, a root) is two values under one key (rule 21).
    pub fn into_store(self, tag: Tag) -> StoreError {
        match self {
            PutError::Mismatch(two) => StoreError::TwoValues(two),
            PutError::TooLong { key, len } => StoreError::TooLong { tag, key: key.0, len },
            PutError::NotWritable { namespace } => StoreError::NotWritable { namespace },
            // blaze-keyed's vocabulary has no row for it yet (store-next.md lists it): the
            // put's own error names the key and offset; the store's says the rule.
            PutError::Machine { .. } => StoreError::Other { what: "a record for a shared namespace holds a path of this machine (rule 25): it stays local" },
        }
    }
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutError::NotWritable { namespace } => write!(f, "a put names namespace {:x}, which this store does not write: a machine that is not a trusted writer writes only the local namespace", namespace.0),
            PutError::Mismatch(two) => write!(f, "{two}"),
            PutError::TooLong { key, len } => write!(f, "the value of {:x} is {len} bytes: a plain store's record is shorter than 4 GiB and a segment shorter than 16 TiB", key.0),
            PutError::Machine { key, namespace, at, needle } => write!(
                f,
                "the value of {:x}, put under namespace {:x}, holds this machine's path {needle} of the store's list at byte {at}: a record that names the machine stays local (rule 25)",
                key.0, namespace.0
            ),
        }
    }
}

/// What a power cut lost, found at an open: never read as empty without a word.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lost {
    /// The file it was in: a segment, or the manifest log of a generation that did not survive.
    pub file: FileName,
    /// The segment's id, when it is one.
    pub segment: Option<u64>,
    /// How many records it held, when that can be read (a segment's manifest row or footer).
    pub records: Option<u64>,
    pub why: String,
}

/// What an open found.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Report {
    pub lost: Vec<Lost>,
    /// Segments of another boot checked whole.
    pub checked: u32,
    /// Segments of another boot proved by their sealed rows, with no byte read
    /// ([`Plain::proved_by_seal`]).
    pub sealed: u32,
    /// The generation whose manifest the open read whole: everything committed at or before
    /// it reads back or is in `lost` by name. None when no generation survived. What was
    /// committed after it, with no sync, a power cut may have taken with no trace to name
    /// (durable mode syncs so nothing is).
    pub whole: Option<u64>,
}

/// A completed commit, its list in the store's allocator.
pub struct Committed<A: Allocator + Clone> {
    pub generation: u64,
    /// The segments it wrote, one for each namespace that held a record, in the order of
    /// [`PlainConfig::writes`]; none when it held no record.
    pub segments: Vec<u64, A>,
}

// By hand, as [`PlainConfig`]'s: no bound on the allocator but `Clone`.
impl<A: Allocator + Clone> Clone for Committed<A> {
    fn clone(&self) -> Committed<A> {
        Committed { generation: self.generation, segments: self.segments.clone() }
    }
}

impl<A: Allocator + Clone> PartialEq for Committed<A> {
    fn eq(&self, other: &Committed<A>) -> bool {
        self.generation == other.generation && self.segments == other.segments
    }
}

impl<A: Allocator + Clone> Eq for Committed<A> {}

impl<A: Allocator + Clone> std::fmt::Debug for Committed<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Committed").field("generation", &self.generation).field("segments", &self.segments).finish()
    }
}

/// What one compaction slice did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sliced {
    /// Segments it replaced.
    pub segments: u32,
    pub kept: u64,
    pub dropped: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Files found named by no generation, retired (unlinked a grace period later).
    pub orphans: u32,
    /// Files unlinked, a grace period after they were retired.
    pub removed: u32,
    /// Roots expired early by LRU budget eviction in this slice.
    pub evicted_roots: u32,
    /// Another process changed the candidates first: nothing flipped.
    pub aborted: bool,
    /// There is more to do: another slice would do it.
    pub more: bool,
    /// It was a tiered merge (`Tiers`), not a pruning or sweeping run: only a merge carries a
    /// cleanup past its first slice ([`Plain::cleanup_goes_on`]).
    pub merged: bool,
}

impl Sliced {
    /// One command's slices as one: their counts summed, `next`'s abort, `more` and `merged`.
    pub fn and(self, next: Sliced) -> Sliced {
        Sliced {
            segments: self.segments + next.segments,
            kept: self.kept + next.kept,
            dropped: self.dropped + next.dropped,
            bytes_in: self.bytes_in + next.bytes_in,
            bytes_out: self.bytes_out + next.bytes_out,
            orphans: self.orphans + next.orphans,
            removed: self.removed + next.removed,
            evicted_roots: self.evicted_roots + next.evicted_roots,
            aborted: next.aborted,
            more: next.more,
            merged: next.merged,
        }
    }
}

/// Read-only inspection of what a GC pass would reclaim at a point in time ([`Plain::plan_gc`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct GcPlan {
    pub total_bytes: u64,
    pub cap_bytes: u64,
    pub high_watermark_bytes: u64,
    pub low_watermark_bytes: u64,
    pub live_bytes: u64,
    pub reclaimable_bytes: u64,
    pub total_roots: u32,
    pub expired_roots: u32,
    pub lru_evict_roots: u32,
    pub segments: u32,
    pub retired_ready: u32,
}

/// The most slices one command's cleanup runs ([`Plain::cleanup_goes_on`]; rule 18, a loop has
/// a count). Why 8: one tiered merge restores a namespace's invariant whole, and a store writes
/// a handful of namespaces (this machine's, the public one, an organisation's), so a cleanup
/// needs one slice a namespace over its bound; the rest is room, which the byte budget bounds
/// first. Only merges go on: a pruning slice ends the cleanup it runs in, and so does a sweep
/// unless a merge is due next.
pub const CLEANUP_SLICES_MOST: u32 = 8;

/// A bug planted for the oracle's controls (`super::tests`): each removes one safeguard.
/// Outside tests a store is always [`Plant::None`].
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// Durable: the segment is not synced before the flip.
    SkipDataSync,
    /// Durable: the flip is written before the segment's sync, which comes after.
    FlipBeforeSync,
    /// Each new entry's offset is one unit past its payload.
    IndexOffByOne,
    /// A commit's layout lays each payload right after the last, not at the next multiple of
    /// [`ALIGN`]: a record no longer starts aligned (aligned_tests).
    Unpadded,
    /// The flip builds on the generation this process saw last, not the head it holds the
    /// lock over: a lost update when another process flipped in between.
    StaleBase,
    /// A segment of another boot is read unchecked.
    TrustBoot,
    /// A full check is made and not recorded in the manifest: the next open of the boot checks
    /// the segment again.
    ForgetCheck,
    /// A lookup searches every namespace, not only those it names.
    IgnoresNamespace,
    /// The flip that retires a loss clears its "lost, not yet reported" mark at once: a
    /// report that dies with its process is never made again.
    ClearMarkAtOnce,
    /// [`Plain::seal`] flips without its barriers: a generation names a segment sealed whose
    /// bytes a power cut may still tear.
    SealBeforeBarrier,
    /// [`Plain::scrub_next`] records a segment scrubbed this boot when its first slice is
    /// walked, before the rest is: what the slices after would have found is never looked at.
    ScrubMarkedEarly,
    /// Durable: the flip writes its slot without syncing the store's directory first: the
    /// names of the segments and the log it made may not survive a power cut the slot does.
    SkipDirSync,
    /// A flip after an open that found no generation's manifest appends to the log the newest
    /// slot names, which a power cut may have taken.
    AppendToLostLog,
    /// A hit in an earlier boot's unsealed segment, parked by a probe, is served without its
    /// full check: a torn record served.
    TrustsUnchecked,
    /// The lazy open maps (and checks) every segment its generation names, as the old open did.
    MapsAllAtOpen,
    /// A pointer's read walks the rows OLDEST first: the first value ever committed of a name
    /// answers, not the last (pointer_tests).
    PointerOldest,
    /// Reach skips the generation's first root: what only it names is dropped (cleanup_tests).
    ReachSkipsRoot,
    /// A folded slice unlinks the segments it replaces before its flip's slot lands, while
    /// the generation on disk still names them (cleanup_tests).
    UnlinkEarly,
    /// A lease is never let go of by the clock: a process that died holding one keeps every
    /// segment since its open for good (cleanup_tests).
    LeaseNeverExpires,
    /// [`Plain::fold_slice`] flips its slice on its own, under the flip lock, as the old
    /// compaction did, instead of in the command's commit: another command's commit waits on
    /// cleanup (cleanup_tests).
    SliceFlipsAlone,
    /// A commit whose roots name a record a slice swept since flips anyway without carrying
    /// it forward: the generation's root names a record no live segment holds (cleanup_tests).
    CarrySkipped,
    /// Reach takes an indexed root's record as a plain root's: the indexes it names are live,
    /// the keys they hold are not (index_tests).
    IndexNotFollowed,
    /// Durable: a MERGE's segment is handed to the flip before its sync, which comes after the
    /// flip (a commit's segments are synced first, as they should be): a power cut between them
    /// leaves a generation naming a torn merge whose inputs it retired (tests,
    /// `the_crash_oracle_catches_a_merge_flipped_before_its_sync`).
    MergeFlipsBeforeSync,
    /// A compaction's copy is held whole, as the old copy into one `Batch` did: its payloads
    /// written as one chunk, every record of the merge in memory at once, whatever its size
    /// (tests, `the_merge_meter_catches_a_whole_batch_copy`).
    MergeWholeBatch,
    /// A compaction's index is held whole, as the stream before the k-way merge did (every
    /// entry built and sorted in memory, then written at once): its index written as one
    /// chunk, O(records) of index alive (tests, `the_merge_meter_catches_a_whole_index`).
    MergeWholeIndex,
    /// A [`Liveness::Reached`] slice plans a sparse row's sweep BEFORE the tiered merge, and a
    /// cleanup never goes on after a sweep (it still goes on after a first slice its lease
    /// emptied), as before 2026-11-20: a sparse row always present
    /// takes every cleanup's one slice and the merge starves (tests,
    /// `the_tier_oracle_catches_a_sweep_that_starves_the_merge`).
    SweepBeforeMerge,
    /// [`Plain::cleanup_goes_on`] stops after the first slice, as a cleanup of one namespace
    /// a command did: a namespace still behind with budget left (tests,
    /// `the_tier_oracle_catches_a_cleanup_of_one_namespace`).
    CleanupOneSlice,
    /// The open after a boot change keeps a root that reaches a lost record (D200's unrooting
    /// skipped): a reader finds a root with a hole (tests, arena_tests.rs
    /// `the_oracle_catches_an_open_that_keeps_a_root_with_a_hole`).
    KeepsHoledRoot,
    /// Root-LRU eviction evicts newest roots first instead of oldest (DST control).
    LruEvictsNewest,
    /// Root-LRU eviction ignores roots touched by the current command (DST control).
    LruEvictsTouched,
    /// Root-LRU eviction treats the active toolchain root as a regular root instead of
    /// preserving it last (DST control).
    LruEvictsToolchainFirst,
    /// Skips CRC verification on mapped `bloom` records so corrupt/torn filters can cause
    /// false negatives (bloom_tests DST control).
    TrustsCorruptBloom,
}

/// What one slice of the background pass did ([`Plain::scrub_next`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Scrubbed {
    /// The segment walked.
    pub segment: u64,
    /// The bytes of it walked in this slice (payloads and entries).
    pub bytes: u64,
    /// The segment's walk ended in this slice: whole (recorded for this boot by a flip), or rotted.
    pub done: bool,
    /// The first record whose bytes fail their check: the segment is lost by name, as the
    /// open's whole check loses one, and retired by the flip that follows.
    pub rot: Option<Corrupt>,
}

/// Where the background pass stands in this process: the segment and the next entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ScrubAt {
    id: u64,
    generation: u64,
    next: usize,
}

/// Puts one worker holds for one namespace, laid out as they will lie in a segment: each
/// payload at a multiple of [`ALIGN`] of `bytes`. A worker fills its own and hands it to
/// [`Plain::commit`]: no lock and no shared word between workers.
#[derive(Debug)]
pub struct Batch {
    namespace: Namespace,
    /// The global heap: the batch outlives the worker's task, and the commit hands it to the
    /// segment's write whole.
    bytes: Vec<u8>,
    entries: Vec<Entry>,
}

impl Batch {
    pub fn new(namespace: Namespace) -> Batch {
        Batch { namespace, bytes: Vec::new_in(Global), entries: Vec::new_in(Global) }
    }

    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// Keeps `value` under `key`. Two puts of one key with other bytes are refused at the
    /// commit, naming the key.
    pub fn put(&mut self, key: Key, tag: Tag, value: &[u8]) -> Result<(), PutError> {
        let crc = record_crc(&key.0, tag, value);
        self.put_crc(key, tag, value, crc)
    }

    /// A record copied from a proved segment, with the CRC its entry recorded.
    fn put_known(&mut self, entry: &Entry, value: &[u8]) -> Result<(), PutError> {
        self.put_crc(Key(entry.key), entry.tag, value, entry.crc)
    }

    fn put_crc(&mut self, key: Key, tag: Tag, value: &[u8], crc: u32) -> Result<(), PutError> {
        let at = self.bytes.len() as u64;
        let len = value.len() as u64;
        if len > u64::from(u32::MAX) || at + len > OFFSET_MOST {
            return Err(PutError::TooLong { key, len });
        }
        self.bytes.extend_from_slice(value);
        self.bytes.resize(aligned(at + len) as usize, 0);
        self.entries.push(Entry { key: key.0, tag, at, len: len as u32, crc });
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn records(&self) -> usize {
        self.entries.len()
    }

    /// The bytes its payloads take, padding included.
    pub fn bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn payload(&self, entry: &Entry) -> &[u8] {
        &self.bytes[entry.at as usize..(entry.at + u64::from(entry.len)) as usize]
    }
}

/// A record lent to a commit's layout where it lies (lent.rs under `Visibility::InArena`: the
/// shared arena), never copied into a [`Batch`] first: its key, tag and namespace, the CRC made
/// at its put (while its bytes were in cache, `format::record_crc`), and its bytes, borrowed for
/// the [`Plain::commit_records`] call alone. The layout copies them once, into the segment's
/// piece, and hashes them once, in its seal.
#[derive(Clone, Copy, Debug)]
pub struct LentRecord<'b> {
    pub key: Key,
    pub tag: Tag,
    pub namespace: Namespace,
    pub crc: u32,
    pub bytes: &'b [u8],
}

impl LentRecord<'_> {
    /// Its entry as a batch's would be, `at` 0 (the layout places it).
    fn entry(&self) -> Entry {
        Entry { key: self.key.0, tag: self.tag, at: 0, len: self.bytes.len() as u32, crc: self.crc }
    }
}

// What each effect is for: the tag's bits 32..40 (the low 32 an index).
const HEAD_READ: u8 = 1;
const MANIFEST_READ: u8 = 2;
const LIST: u8 = 3;
const FOOTER_READ: u8 = 4;
const MAP: u8 = 5;
const SEG_TRUNCATE: u8 = 6;
const SEG_PIECE: u8 = 7;
const SEG_SYNC: u8 = 8;
const LOCK: u8 = 9;
const LOCK_WAIT: u8 = 10;
const LOG_TRUNCATE: u8 = 11;
const LOG_WRITE: u8 = 12;
const LOG_SYNC: u8 = 13;
const SLOT_WRITE: u8 = 14;
const HEAD_SYNC: u8 = 15;
const UNLOCK: u8 = 16;
const REMOVE: u8 = 17;
const LATE_SYNC: u8 = 18;
const HEAD_MAKE: u8 = 19;
const BARRIER: u8 = 20;
/// The store's directory synced: the names of the files made before it (the head at the first
/// open; durable: a commit's segments and its log, before the slot that names them) survive a
/// power cut, as a file's own sync does not make its name do.
const DIR_SYNC: u8 = 21;
/// A streamed copy's temporary file renamed to its digest's name, before its sync.
const SEG_RENAME: u8 = 22;
/// A streamed copy's temporary file removed: its bytes are a segment named already.
const SEG_UNWRITE: u8 = 23;
/// Optional map of the `bloom` sidecar file at open.
const BLOOM_MAP: u8 = 24;
/// Optional append or compact rewrite of the `bloom` sidecar under the flip lock.
const BLOOM_WRITE: u8 = 25;
/// A streamed copy's writes, by their [`SEG_PIECE`] index: 0 its header, then a payload chunk
/// each from 1, an index chunk each from this, and its footer at [`FOOTER_PIECE`].
const INDEX_PIECE: u32 = 1 << 30;
const FOOTER_PIECE: u32 = u32::MAX;
/// The index the planted [`Plant::UnlinkEarly`]'s unlinks start at, apart from a flip's own.
const EARLY_REMOVE: u32 = 1 << 30;

/// Outcome of [`Plain::renew_root`]: O(1) lease check or refresh on an already-live root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Renewed {
    /// The root is not live (absent, expired, or pending unset).
    Missing,
    /// The root is already live with enough remaining life (within [`TOUCH_NS`] of `life_ns`):
    /// nothing queued and no flip owed.
    Fresh(Key),
    /// The root is live and queued to extend its lease at the next commit without rewriting its
    /// record.
    Extended(Key),
}

impl Renewed {
    /// The root's record key when live (`Fresh` or `Extended`).
    pub const fn key(self) -> Option<Key> {
        match self {
            Renewed::Missing => None,
            Renewed::Fresh(key) | Renewed::Extended(key) => Some(key),
        }
    }

    /// Whether the root was live (already fresh or queued for renewal).
    pub const fn is_live(self) -> bool {
        self.key().is_some()
    }
}

/// What a commit changes of what keeps records: roots, and this process's lease.
enum RootEdit {
    /// `name` names `key`, for `life_ns` from the flip ([`NEVER`]: until unset). When
    /// `renew_only` is true, extends an already-live root still naming `key` at the flip without
    /// rewriting its record or scanning its held keys.
    Set { name: String, key: Key, life_ns: u64, renew_only: bool },
    Unset(String),
    /// This process's lease set or renewed: segments of generation `since` on are kept whole
    /// until [`LEASE_NS`] from the flip.
    Lease { since: u64, opened_ns: u64 },
    /// This process's lease let go of: its records are rooted by now.
    Release,
}

/// One root scheduled for LRU eviction under the byte budget, with the generation and expiry
/// observed at plan time so the flip aborts if a concurrent command renewed or updated it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct EvictRow {
    start: usize,
    len: usize,
    generation: u64,
    expires_ns: u64,
}

/// Roots selected for LRU eviction during one compaction slice, stored in the store's allocator.
struct EvictPlan<A: Allocator> {
    names: blaze_own::Vec<u8, A>,
    rows: blaze_own::Vec<EvictRow, A>,
}

impl<A: Allocator> EvictPlan<A> {
    fn new(names: blaze_own::Vec<u8, A>, rows: blaze_own::Vec<EvictRow, A>) -> EvictPlan<A> {
        EvictPlan { names, rows }
    }

    fn push(&mut self, name: &str, generation: u64, expires_ns: u64) {
        let start = self.names.len();
        self.names.extend_from_slice(name.as_bytes());
        self.rows.push(EvictRow { start, len: name.len(), generation, expires_ns });
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn len(&self) -> usize {
        self.rows.len()
    }

    fn name_of(&self, row: &EvictRow) -> &str {
        self.names
            .get(row.start..row.start.saturating_add(row.len))
            .and_then(|slice| core::str::from_utf8(slice).ok())
            .unwrap_or("")
    }

    fn contains(&self, name: &str) -> bool {
        self.rows.iter().any(|row| self.name_of(row) == name)
    }

    fn iter(&self) -> impl Iterator<Item = (&str, u64, u64)> {
        self.rows.iter().map(|row| (self.name_of(row), row.generation, row.expires_ns))
    }
}

/// A compaction slice written with no lock and flipped by the command's next commit
/// ([`Plain::fold_slice`]): its copy (none when the slice only drops), the segments it
/// replaces, the orphans it retires, and any roots LRU-evicted under the byte budget.
struct Folded<A: Allocator + Clone> {
    seg: Option<(Segment, FileName)>,
    candidates: Vec<u64, A>,
    orphans: Vec<(u64, bool), A>,
    evict: Option<EvictPlan<A>>,
    stats: Sliced,
}

/// Why a head is being read: at the open, or under the lock for a flip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctx {
    Open,
    Flip,
}

/// What a write of a segment is for.
enum Why<A: Allocator + Clone> {
    /// A commit's segments are written one after another, one for each namespace with
    /// records, then flipped at once: `rest` still to write, `done` written.
    Commit { started_ns: u64, roots: Vec<RootEdit, A>, rest: Vec<Laid<A>, A>, done: Vec<(Segment, FileName), A> },
    Compact { candidates: Vec<u64, A>, stats: Sliced },
}

/// One segment laid out and not yet written: the payloads at their offsets, from [`HEADER`]
/// to `index_at`, and the sorted run. The list is the store's; each piece's bytes and the run
/// are the process heap, moved whole into their writes (blaze-core's effects own them).
struct Laid<A: Allocator + Clone> {
    pieces: Vec<(u64, Vec<u8>), A>,
    index_at: u64,
    run: Vec<u8>,
    namespace: Namespace,
}

/// A compaction's copy planned and not yet read ([`Plain::copy_live`]): its k-way merge over
/// the inputs' sorted runs, and what its first walk counted, all its header says (the count,
/// where the payloads end, the lowest and the highest key). O(inputs): no record is held.
struct Copied<A: Allocator + Clone> {
    merge: Merge<A>,
    count: u32,
    index_at: u64,
    lowest: ContentId,
    highest: ContentId,
}

/// One input of a streamed copy: a cursor over its proved, mapped sorted run, read in place.
#[derive(Clone, Copy)]
struct Cursor {
    /// The index into `Plain::maps`, which only grows (a mapping is never dropped while the
    /// store lives), so it names the same proved file for the whole copy.
    map: u32,
    /// The next entry, of `count`.
    next: u32,
    count: u32,
    /// Younger than [`WINDOW`]: a pruning slice keeps all of it.
    young: bool,
}

/// Keep-priority tiers for root-level LRU eviction (lower value = kept longer).
const KEEP_TIER_PROTECTED: u8 = 0;
const KEEP_TIER_ACTIVE_TOOLCHAIN: u8 = 1;
const KEEP_TIER_OLD_TOOLCHAIN: u8 = 2;
const KEEP_TIER_REGULAR: u8 = 3;

/// High bit on a `ReachCache::owner` slot value marking that the key's byte length has already
/// been added to its owning root's marginal bytes and `total_live`.
const SIZED_BIT: u32 = 1u32 << 31;

/// Shared reachability map type (store-tagged [`Kept`] allocator so `Arc::share()` works across
/// any store allocator `A` with zero `Global` heap).
type ReachOwner = blaze_own::Arc<Map<[u8; 32], u32, BuildKeyHasher, Kept>, Kept>;

/// Cached root reachability and marginal live-byte accounting for one `(roots, leases, touched)`
/// state and wall-clock interval `[valid_from_ns, valid_until_ns)`. Reused across
/// `live_and_evictions`, `plan_reached`, `copy_live`, and consecutive compaction slices whenever
/// the manifest's roots and leases have not changed.
struct ReachCache<A: Allocator + Clone> {
    epoch: u64,
    roots_hash: ContentId,
    leases_hash: ContentId,
    valid_from_ns: u64,
    valid_until_ns: u64,
    sized: bool,
    /// Unexpired roots sorted from highest keep priority to lowest:
    /// `(manifest_root_idx, keep_tier, marginal_bytes)`.
    order: blaze_own::Vec<(usize, u8, u64), A>,
    total_live: u64,
    /// Maps every reachable key to its first claiming index in `order` (OR'd with [`SIZED_BIT`]
    /// once sized). Shared zero-copy via `Arc` with [`Keep`] during streamed compaction copies.
    owner: ReachOwner,
}

/// What a slice's copy keeps. Owned (it borrows nothing of the store), so a streamed copy
/// carries it across its writes.
enum Keep {
    /// Everything (a merge of a [`Liveness::Everything`] store).
    All,
    /// What roots name, or younger than [`WINDOW`] (its pruning over the cap): O(1) lookup in
    /// the shared reachability map (live iff owning slot `< kept_count`).
    Rooted { owner: ReachOwner, kept_count: u32 },
    /// What reaches from an unexpired surviving root ([`Liveness::Reached`]): O(1) lookup in
    /// the shared reachability map (live iff owning slot `< kept_count`).
    Reached { owner: ReachOwner, kept_count: u32 },
}

impl Keep {
    #[inline]
    fn keeps(&self, key: &ContentId, young: bool) -> bool {
        match self {
            Keep::All => true,
            Keep::Rooted { owner, kept_count } => young || owner.get(&key.0).is_some_and(|&slot| (slot & !SIZED_BIT) < *kept_count),
            Keep::Reached { owner, kept_count } => owner.get(&key.0).is_some_and(|&slot| (slot & !SIZED_BIT) < *kept_count),
        }
    }
}

/// A compaction's copy as a K-WAY MERGE of its inputs' sorted runs, read in place where they
/// are mapped: each step takes the least key at any cursor (a tie to the input first in the
/// slice's order, the manifest's), decides its liveness as it is met, and keeps the first live
/// value of a key (a later input's is the same record: one key names one value, rule 21; a
/// pointer's only at its newest row). Its records so come in key order, which is the copy's
/// index order: its payloads are laid out in that order too, so payloads and index are one
/// order and both are streamed. It holds its cursors and a heap of them ([`CURSOR_BYTES`] an
/// input) and what it keeps by ([`Keep`]); no record. A walk is deterministic, so a second
/// ([`Merge::rewind`]) meets the same records: a copy walks three times, to count (its header),
/// to place its payloads, to write its index (the digest takes every payload byte before the
/// first index byte).
struct Merge<A: Allocator + Clone> {
    keep: Keep,
    namespace: Namespace,
    cursors: Vec<Cursor, A>,
    /// The cursors with entries left, a binary min-heap by (the key at the cursor, its index).
    heap: Vec<u32, A>,
    /// The key last kept: a later input's value of it is a duplicate.
    last: Option<ContentId>,
    /// The entries this walk let go of (dead, or a pointer's older value).
    dropped: u64,
}

impl<A: Allocator + Clone> Merge<A> {
    fn key_at<'m>(&self, maps: &'m [Verified], cursor: u32) -> &'m [u8] {
        let cursor = &self.cursors[cursor as usize];
        let at = cursor.next as usize * ENTRY;
        &maps[cursor.map as usize].run()[at..at + 32]
    }

    fn before(&self, maps: &[Verified], a: u32, b: u32) -> bool {
        (self.key_at(maps, a), a) < (self.key_at(maps, b), b)
    }

    fn sift_down(&mut self, maps: &[Verified], mut at: usize) {
        loop {
            let mut least = at;
            for child in [2 * at + 1, 2 * at + 2] {
                if child < self.heap.len() && self.before(maps, self.heap[child], self.heap[least]) {
                    least = child;
                }
            }
            if least == at {
                return;
            }
            self.heap.swap(at, least);
            at = least;
        }
    }

    /// Back to every input's first entry: a walk from the start.
    fn rewind(&mut self, maps: &[Verified]) {
        self.heap.clear();
        self.last = None;
        self.dropped = 0;
        for (i, cursor) in self.cursors.iter_mut().enumerate() {
            cursor.next = 0;
            if cursor.count > 0 {
                self.heap.push(i as u32);
            }
        }
        for at in (0..self.heap.len() / 2).rev() {
            self.sift_down(maps, at);
        }
    }

    /// The next record the copy keeps, in key order: its input's mapping and its entry there;
    /// `None` once every input is walked. O(log k) a record, nothing allocated.
    fn next(&mut self, store: &Plain<A>) -> Option<(u32, Entry)> {
        let maps = &store.maps;
        while let Some(&top) = self.heap.first() {
            let cursor = self.cursors[top as usize];
            let at = cursor.next as usize * ENTRY;
            // A proved segment's entry: read, not checked (decision 18).
            let entry = Entry::read(&maps[cursor.map as usize].run()[at..at + ENTRY]);
            self.cursors[top as usize].next += 1;
            if cursor.next + 1 == cursor.count {
                self.heap.swap_remove(0);
            }
            self.sift_down(maps, 0);
            if !self.keep.keeps(&entry.key, cursor.young) {
                self.dropped += 1;
                continue;
            }
            // A pointer keeps only its newest value: the one in the newest row holding its
            // name (in this slice or after it); an older value is let go.
            let newest = !ReservedTag::Pointer.is(entry.tag) || store.newest_mapped(self.namespace, &Key(entry.key)).is_some_and(|(at, _)| at == cursor.map as usize);
            if !newest {
                self.dropped += 1;
                continue;
            }
            if self.last == Some(entry.key) {
                continue;
            }
            self.last = Some(entry.key);
            return Some((cursor.map, entry));
        }
        None
    }
}

/// The BLAKE3 domain of a streamed copy's temporary file's id.
const MERGE_TEMP_DOMAIN: &str = "blaze plain store merge temporary 1";

/// A segment being written, before its flip.
struct Writing<A: Allocator + Clone> {
    /// Its row. A streamed copy's, while it streams, has no digest (zero) and so no id yet:
    /// [`Plain::named_copy`] makes its row once the stream sealed it.
    seg: Segment,
    /// Its file: a streamed copy's temporary one while it streams.
    file: FileName,
    /// The writes at offsets, laid out by [`Plain::commit`] (a streamed copy's: its header),
    /// each moved into its effect.
    pieces: Vec<(u64, Vec<u8>), A>,
    /// The writes in flight.
    left: u32,
    /// A compaction's copy, streamed from the inputs' mappings after `pieces`: `None` for a
    /// commit's segment, whose payloads are its own batches'.
    stream: Option<Stream<A>>,
}

/// One record of a streamed copy: where its payload lies in a mapping of this process.
#[derive(Clone, Copy)]
struct Pick {
    /// The index into `Plain::maps` (its [`Cursor::map`]).
    map: u32,
    len: u32,
    /// The payload's offset in that file.
    from: u64,
}

/// Which part of a streamed copy its next writes are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Leg {
    /// Its payloads, from [`HEADER`] to `index_at`, a chunk at a time, as the merge walks.
    Payloads,
    /// Its index, from `index_at`, a chunk at a time, as the merge walks again.
    Index,
    /// Its footer asked: its digest, and so its name, known.
    Sealed,
}

/// A compaction's copy being streamed into its temporary file ([`Plain::stream_segment`]).
/// What it holds at once is its merge's cursors and the chunks in flight, at most
/// [`MERGE_BUFFER`] of records and one [`INDEX_CHUNK`] of index: [`merge_holds_most`]; and its
/// fixed state, the digest's hasher among it (under 2 KiB).
struct Stream<A: Allocator + Clone> {
    merge: Merge<A>,
    /// The digest of every byte asked so far, in the file's order ([`format::seal_parts`]'s:
    /// the header, the payloads, the index, the footer's head).
    hasher: blaze_ids::Hasher,
    leg: Leg,
    /// The record the next payload chunk goes on with, and how far into its padded span.
    current: Option<(Pick, u64)>,
    /// Where the next write starts.
    at: u64,
    /// The index leg: where the next entry's payload lies in the copy.
    placed: u64,
    index_at: u64,
    count: u32,
    /// The index leg's entries asked.
    indexed: u32,
    /// The most bytes one payload chunk takes: `MERGE_BUFFER / MERGE_CHUNKS_IN_FLIGHT`.
    chunk: u64,
    /// The most entries one index chunk takes: `INDEX_CHUNK / ENTRY`.
    index_entries: u32,
    /// Payload chunks asked (each its effect's tag index past the header's), and in flight.
    asked: u32,
    flying: u32,
    /// Index chunks asked, and whether one is in flight (the next is built only once it
    /// completed: one chunk of index alive).
    index_asked: u32,
    index_flying: bool,
    /// Per-segment Bloom filter populated while streaming the index leg.
    bloom: Option<BloomBuilder>,
    /// What its footer says, once sealed.
    sealed: Option<format::Described>,
}

impl<A: Allocator + Clone> Stream<A> {
    /// The next payload chunk: its offset and its bytes (the process heap: the effect owns
    /// them), copied in place from the proved mappings of `store` as the merge meets each
    /// record, each padded to [`ALIGN`], and hashed; `None` once every payload byte was asked.
    fn next_chunk(&mut self, store: &Plain<A>) -> Option<(u64, Vec<u8>)> {
        if self.at == self.index_at {
            return None;
        }
        let take = (self.index_at - self.at).min(self.chunk);
        let mut bytes = Vec::with_capacity_in(take as usize, Global);
        while (bytes.len() as u64) < take {
            let (pick, within) = match self.current.take() {
                Some(it) => it,
                None => {
                    let (map, entry) = self.merge.next(store).expect("a copy's second walk meets the records its first counted: the store does nothing else while it streams");
                    (Pick { map, len: entry.len, from: entry.at }, 0)
                }
            };
            let span = aligned(u64::from(pick.len));
            let step = (span - within).min(take - bytes.len() as u64);
            let real = u64::from(pick.len).min(within + step).saturating_sub(within);
            if real > 0 {
                let from = (pick.from + within) as usize;
                bytes.extend_from_slice(&store.maps[pick.map as usize].bytes()[from..from + real as usize]);
            }
            bytes.resize(bytes.len() + (step - real) as usize, 0);
            if within + step < span {
                self.current = Some((pick, within + step));
            }
        }
        self.hasher.update(&bytes);
        let at = self.at;
        self.at += take;
        self.asked += 1;
        Some((at, bytes))
    }

    /// The next index chunk: its offset and at most `index_entries` entries (every one left,
    /// `whole`, under [`Plant::MergeWholeIndex`]), each placing its payload where the payload
    /// leg laid it, its CRC as recorded (nothing is checked or computed again), hashed; `None`
    /// once every entry was asked.
    fn next_index(&mut self, store: &Plain<A>, whole: bool) -> Option<(u64, Vec<u8>)> {
        let left = self.count - self.indexed;
        if left == 0 {
            return None;
        }
        let fit = if whole { left } else { left.min(self.index_entries) };
        let mut bytes = Vec::with_capacity_in(fit as usize * ENTRY, Global);
        for _ in 0..fit {
            let (_, entry) = self.merge.next(store).expect("a copy's third walk meets the records its first counted: the store does nothing else while it streams");
            if let Some(bloom) = &mut self.bloom {
                bloom.insert(&entry.key.0);
            }
            Entry { at: self.placed, ..entry }.encode_into(&mut bytes);
            self.placed = aligned(self.placed + u64::from(entry.len));
        }
        self.hasher.update(&bytes);
        let at = self.at;
        self.at += bytes.len() as u64;
        self.indexed += fit;
        self.index_asked += 1;
        Some((at, bytes))
    }

    /// The footer, its digest of every byte before it: the copy sealed, its row known.
    fn seal(&mut self) -> (u64, Vec<u8>) {
        debug_assert!(self.at == self.index_at + u64::from(self.count) * ENTRY as u64 && self.placed == self.index_at, "the index follows the payloads it places");
        let foot = format::footer_head(self.count, self.index_at);
        self.hasher.update(&foot);
        let digest: ContentId = self.hasher.finish();
        let mut footer = Vec::with_capacity_in(FOOTER, Global);
        footer.extend_from_slice(&foot);
        footer.extend_from_slice(&digest.0);
        self.sealed = Some(format::Described { namespace: self.merge.namespace, count: self.count, index_at: self.index_at, digest });
        self.leg = Leg::Sealed;
        (self.at, footer)
    }
}

/// The proof a written segment reaches its flip with (states.md item 21).
mod proof {
    use std::alloc::Allocator;

    use super::Writing;

    /// A segment every piece of which was written, and, in a durable store, whose sync
    /// completed: what [`super::Plain::segment_written`] takes, so no path hands a flip a
    /// durable store's segment before its sync. Made by [`Synced::by_sync`] at the sync's
    /// completion, by [`Synced::not_durable`] at the last write's when the store syncs nothing,
    /// by [`Synced::already_named`] for bytes a row names already, and by
    /// [`Synced::for_a_plant`] for a planted control alone.
    pub(super) struct Synced<A: Allocator + Clone>(Writing<A>);

    impl<A: Allocator + Clone> Synced<A> {
        /// The segment's sync completed (its failure was taken first).
        pub(super) fn by_sync(writing: Writing<A>) -> Synced<A> {
            Synced(writing)
        }

        /// Bytes a row of this generation names already, under this name, so nothing is
        /// written: the row's own write was synced before the flip that named it.
        pub(super) fn already_named(writing: Writing<A>) -> Synced<A> {
            debug_assert!(writing.pieces.is_empty() && writing.stream.is_none(), "nothing of a named segment is written again");
            Synced(writing)
        }

        /// Every piece is written and the store is not durable: no sync is owed.
        pub(super) fn not_durable(writing: Writing<A>, durable: bool) -> Result<Synced<A>, Writing<A>> {
            if durable { Err(writing) } else { Ok(Synced(writing)) }
        }

        /// A planted control's segment, handed on before (or without) its sync: reached only
        /// when [`super::Plain::plant`] names one, which is never outside tests.
        pub(super) fn for_a_plant(writing: Writing<A>) -> Synced<A> {
            Synced(writing)
        }

        pub(super) fn into_writing(self) -> Writing<A> {
            self.0
        }
    }
}
use proof::Synced;

/// What a flip adds to the generation it builds on.
enum Change<A: Allocator + Clone> {
    Commit { segs: Vec<(Segment, FileName), A>, started_ns: u64, roots: Vec<RootEdit, A> },
    /// Only what an open found (segments checked or lost): [`Plain::pending_stamps`].
    Stamp,
    Compact { seg: Option<(Segment, FileName)>, candidates: Vec<u64, A>, orphans: Vec<(u64, bool), A>, evict: Option<EvictPlan<A>>, stats: Sliced },
    /// Segments fetched from a source, written here: named here and published.
    Adopt { segs: Vec<Segment, A> },
    /// Segments pushed to a shared bucket.
    Publish { ids: Vec<u64, A> },
    /// The exit tail sealed (store-next.md): each `(id, generation added)` had a barrier
    /// complete on its file before this flip was asked. A row is sealed only while it is still
    /// that one (named here, with that generation): a file removed and written again since
    /// is another's bytes, and stays for the next seal.
    Seal { ids: Vec<(u64, u64), A> },
}

/// The generation a flip builds on, read under the lock.
struct Base {
    /// The slot the new generation's number and log follow, `None` when nothing was ever
    /// committed.
    slot: Option<Slot>,
    manifest: Manifest,
}

struct Flip<A: Allocator + Clone> {
    change: Change<A>,
    waits: u32,
    /// The sum of this flip's lock-wait steps so far ([`lock_step`]), kept as it goes so the
    /// bound's check is one comparison, not a sum over every earlier ask.
    waited_ns: u64,
    next: Option<(Slot, Manifest)>,
    /// The head file did not exist: the first flip writes it whole.
    fresh: bool,
    new_log: bool,
    log: FileName,
    /// The process heap: moved whole into the log's append (blaze-core's effect owns it).
    manifest_bytes: Vec<u8>,
    removals: Vec<FileName, A>,
    /// The commit was refused under the lock, or the compaction aborted: unlock and stop.
    refused: Option<StoreError>,
    aborted: bool,
    /// The commit found a record its roots reuse swept since: not flipped; unlock, write
    /// what `carrying` lists into a segment of the commit's own, and flip again.
    carry: bool,
    /// [`Plant::UnlinkEarly`]: unlinks in flight before the log's write, which waits here.
    early: u32,
    deferred: Option<Effect>,
    /// Optional sidecar Bloom append or compact rewrite performed under the flip lock before
    /// `SLOT_WRITE`. Its failure never aborts the flip.
    bloom_effect: Option<Effect>,
    /// `(replaced, written_bytes)` for `bloom_effect`, cleared if `BLOOM_WRITE` fails and applied
    /// to `BloomIndex::mapped_bytes` when the flip commits.
    bloom_written: Option<(bool, u64)>,
}

/// Parked `Ctx::Open` resolution waiting for the parallel `BLOOM_MAP` effect to settle.
struct OpenParked {
    base: Base,
    fell_back: bool,
}

enum Phase<A: Allocator + Clone> {
    New,
    /// A head read, at the open or for a flip.
    ReadHead(Ctx),
    /// A slot's manifest read: `slots[at]`, the other the fallback.
    ReadManifest { ctx: Ctx, slots: [Option<Slot>; 2], at: usize, fell_back: bool },
    /// A manifest's read failed: the directory is listed to tell a log a power cut took (not
    /// there: the same as torn) from a read that failed (there: the error, by name). A failed
    /// read is never taken for a lost generation.
    CheckLog { ctx: Ctx, slots: [Option<Slot>; 2], at: usize, why: StoreError },
    /// The head was short of its length (never made, or made and not synced before a power
    /// cut): it is cut to [`HEAD_LEN`] and synced, then read again.
    MakeHead,
    /// The open fell back: which files no generation names, and what their footers hold.
    Orphans { left: u32 },
    /// Compaction's look at the directory.
    Listing,
    Mapping { left: u32, then: Then },
    /// The maps are in and some of an earlier boot failed: the directory is listed to tell a
    /// file a power cut took from one that failed to map.
    CheckMaps { then: Then },
    Ready,
    Writing(Box<Writing<A>>, Box<Why<A>>),
    /// A streamed copy's temporary file renamed to its digest's name, before its sync.
    Renaming(Box<Writing<A>>, Box<Why<A>>),
    /// A streamed copy's temporary file (`temp`) removed: its bytes are named already.
    Unwriting { writing: Box<Writing<A>>, why: Box<Why<A>>, temp: FileName },
    /// The segment's sync, before the flip.
    SyncingSegment(Box<Writing<A>>, Box<Why<A>>),
    Flipping(Box<Flip<A>>),
    Removing { left: u32, then: Then },
    /// [`Plain::seal`]'s barriers, `left` of them in flight; the flip follows the last.
    Sealing { left: u32, ids: Vec<(u64, u64), A> },
    Broken,
}

/// What comes after the maps of a new generation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Then {
    Open,
    /// The segments it wrote are [`Plain::committing`].
    Commit,
    Stamp,
    Compact(Sliced),
    /// One row a probe asked ([`Plain::find`]).
    Probe,
    /// Every row ([`Plain::map_all`]): whole after it.
    All,
}

/// The plain store's state machine: one per process (or per test actor). What it keeps is in
/// `alloc`, its caller's, handed to [`Plain::new`] (rule 75).
pub struct Plain<A: Allocator + Clone> {
    alloc: A,
    cfg: PlainConfig<A>,
    #[cfg(test)]
    plant: Plant,
    /// A test's merge geometry ([`Tiers::planted`]); outside tests, [`Tiers::STORE`].
    #[cfg(test)]
    tiers: Tiers,
    /// A test's merge buffer; outside tests, [`MERGE_BUFFER`].
    #[cfg(test)]
    merge_buffer: u64,
    /// A test's index chunk; outside tests, [`INDEX_CHUNK`].
    #[cfg(test)]
    index_chunk: u64,
    /// The most bytes a streamed copy built at once, its index chunk and its cursors: what the
    /// merge meter checks beside the writes in flight (tests).
    #[cfg(test)]
    merge_held_most: u64,
    head: FileName,
    flip: FileName,
    bloom_file: FileName,
    bloom: BloomIndex,
    bloom_loading: bool,
    open_parked: Option<OpenParked>,
    /// Framed Bloom records built for segments written by this process, awaiting flip.
    pending_bloom: blaze_own::Vec<(ContentId, blaze_own::Vec<u8, Kept>), Kept>,
    /// Most recently hit segment row index in `manifest.segments` (for zero-mmap locality).
    pub(super) last_hit: Option<usize>,
    /// Optional creation-generation locality hint for the current `Plain::find` call.
    pub(super) hint_gen: Option<u64>,
    phase: Phase<A>,
    /// The newest generation this process saw, and its manifest.
    slot: Option<Slot>,
    manifest: Manifest,
    /// Every segment this process mapped, each through its proof: kept mapped for the life of
    /// the process, never unmapped during a run (an unmap is a TLB shootdown on every core).
    maps: Vec<Verified, A>,
    /// The boot every proof here holds for.
    boot: Boot,
    /// Full checks made by this process, and reads (the per-read count stays zero).
    checks: std::cell::Cell<Checks>,
    /// Each segment this process checked in full, in order: for the oracle, which fails on a
    /// segment checked twice in one boot.
    checked_ids: Vec<u64, A>,
    /// Each segment of another boot this process proved by its sealed row, in order.
    sealed_ids: Vec<u64, A>,
    /// The background pass's place ([`Plain::scrub_next`]); lost with the process, so a
    /// crash re-walks only the segment it was in.
    scrubbing: Option<ScrubAt>,
    by_id: Map<u64, usize, BuildKeyHasher, A>,
    /// File names already constructed for in-flight `Effect::Map` requests, indexed by manifest
    /// row so `on_map` reuses the `FileName` without re-formatting per segment.
    pending_map_files: Vec<Option<FileName>, A>,
    /// Monotonic epoch bumped whenever non-compaction state changes may affect reachability.
    reach_epoch: u64,
    /// Cached root reachability & marginal byte accounting reused across compaction slices.
    reach_cache: Option<ReachCache<A>>,
    /// This generation's local rows not lost, as indices into `manifest.segments`, in probe
    /// order (largest first, ties newest first: [`Plain::order_view`]). A row may be unmapped.
    pub(super) view: Vec<usize, A>,
    /// Earlier-boot unsealed segments a probe mapped, length checked, not yet proved: each is
    /// checked whole at its first hit, or by [`Plain::map_all`] (probe.rs).
    unchecked: BTreeMap<u64, Unverified>,
    /// Whole: every row is mapped (and none parked) from now on, every later generation mapped
    /// as it is adopted: after [`Plain::map_all`] or [`Plain::open_whole`].
    eager: bool,
    /// The wall clock when the open read its generation: a map is asked within [`GRACE_NS`]
    /// of it (probe.rs, "THE GRACE").
    opened_ns: u64,
    /// What this process's gets probed, for `--stats`.
    probes: Probes,
    /// The store's own batches, one for each namespace of [`PlainConfig::writes`] in its order,
    /// behind [`Plain::put`]: read back by [`Plain::get`] until the commit that writes them
    /// completes.
    held: Vec<Held, A>,
    /// Held batches being committed, still read by [`Plain::get`].
    sealed: Vec<Held, A>,
    /// The segments the commit in flight wrote, for its [`Committed`].
    committing: Vec<u64, A>,
    roots_pending: Vec<RootEdit, A>,
    /// Found at the open, written at the next flip: segments of another boot checked whole,
    /// segments lost, files named by no generation.
    checked: BTreeSet<u64>,
    dropped: BTreeSet<u64>,
    orphans: Vec<(u64, bool), A>,
    /// The segments this open found lost, with why: the next flip marks the rows that retire
    /// them "lost, not yet reported" in the same manifest ([`Retired::lost`]).
    marks: Vec<(u64, LostMark), A>,
    /// Marked rows this process's report names, its report not yet handed to its caller.
    unreported: Vec<u64, A>,
    /// Marked rows whose report was handed to the caller ([`Plain::handed`]): the next flip
    /// this process makes clears them.
    clearable: Vec<u64, A>,
    report: Report,
    committed: Option<Result<Committed<A>, StoreError>>,
    sliced: Option<Sliced>,
    /// The first stop, typed: each reason is a [`StoreError`] variant naming the file, the
    /// step and what was found, never text.
    broken: Option<StoreError>,
    /// The files a flip retired past their grace, being unlinked: by the index of their
    /// effects, so a failed unlink names its file.
    removing: Vec<FileName, A>,
    seq: u64,
    /// Lists and footers of the open's fallback, by tag index.
    listed: Vec<(FileName, u64), A>,
    /// A flip's state while its head is read under the lock.
    flip_parked: Option<Box<Flip<A>>>,
    /// What a compaction's directory look found, for its flip.
    orphans_for_compact: Vec<(u64, bool), A>,
    /// Roots scheduled for LRU eviction by the in-flight compaction slice.
    evict_for_compact: Option<EvictPlan<A>>,
    /// Length-prefixed UTF-8 names of roots set or renewed by this process (`u16` LE + bytes),
    /// including fresh no-op renewals: never evicted by this process's LRU budget pass.
    touched_roots: Option<blaze_own::Vec<u8, A>>,
    /// The planted [`Plant::FlipBeforeSync`]'s segments, synced after the flip.
    late_sync: Vec<FileName, A>,
    /// Segments of an earlier boot whose map failed, with why: lost only if the directory no
    /// longer holds them (a power cut took the file); a failed map of one it holds is an error.
    unopened: Vec<(Segment, FileName, StoreError), A>,
    /// This open made the head at its length already (it is made once, never twice).
    head_made: bool,
    /// The log the current slot names gave no generation's manifest back (a power cut tore it,
    /// or took its name before its directory was synced): the next flip starts a new log, as
    /// an append to it may find no file there.
    log_lost: bool,
    /// The flip lock is held (taken, and its unlock not yet asked): a step that fails under it
    /// lets it go as it stops, so a broken store never holds every other process's commit.
    holds_lock: bool,
    /// What this process's flips waited on the flip lock ([`Plain::lock_waited`]).
    lock_waited: LockWaited,
    /// The torn slot a short head showed before this open made it whole length.
    head_torn: Option<usize>,
    /// A torn slot, or a slot whose manifest is not whole, was reported at this open: the next
    /// flip writes past it (over the torn slot, which the newest does not hold; a generation
    /// whose manifest is whole beside the other), so it is reported once, not at every open.
    torn_stamp: bool,
    /// The generation the open read: this process's lease keeps every segment newer.
    opened_generation: u64,
    /// The wall clock of the last completion taken, as the driver passed it: what a root set
    /// again is measured against ([`Plain::root_for`]) and a mapped generation's next slice
    /// planned with.
    now_ns: u64,
    /// A generation this process flipped names its lease, and no later flip let it go.
    leased: bool,
    /// [`Plain::release_lease`] was asked: no later commit sets the lease again.
    released: bool,
    /// The slice [`Plain::fold_slice`] wrote, for the next commit's flip; then, being flipped.
    folding: Option<Folded<A>>,
    /// The slice in flight is [`Plain::fold_slice`]'s: it parks in `folding`, not flipped.
    fold_asked: bool,
    /// The commit in flight's reused records, by segment ([`Plain::reused`]).
    touches: Vec<(u64, ContentId), A>,
    /// The commit in flight's reused records whose segment the flip found replaced: what its
    /// next round carries forward ([`Plain::carry`]).
    carrying: Vec<(u64, ContentId), A>,
    /// Keys already carried into `segs` during earlier rounds of the current commit so
    /// `missing_from_eviction` never carries the same key twice.
    carried_in_commit: Option<blaze_own::Vec<ContentId, A>>,
    /// The rounds the commit in flight carried records forward: at most [`CARRY_ROUNDS`].
    carry_rounds: u32,
    /// Records carried forward by this process's commits, ever ([`Plain::carried`]).
    carried: u64,
    /// The bytes of the segments this process's commits named, and how many: what its
    /// cleanup's budget is ([`Plain::cleanup_goes_on`]).
    written_here: u64,
    segments_here: u64,
    /// The indexes this process made, with how many keys each holds: what an indexed root
    /// that names one counts ([`Plain::named`]).
    indexes_made: Vec<(Key, u64), A>,
    /// The roots this process set and the keys they name ([`Plain::named`]).
    named: Named,
    /// What this process's commits did on the disk side of the put path ([`Plain::put_path`]).
    put_path: PutPath,
}

/// The roots one process set and the keys they name: the command's cleanup line.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Named {
    /// Roots set (each `root_for` or `root_through` call that named one).
    pub roots: u32,
    /// Keys named, directly or through an index this process made.
    pub keys: u64,
    /// Of them, through index records.
    pub indexed: u64,
}

/// What this process's commits did on the disk side of the put path (rule 16: the measure before
/// any change of it; rule 67: the floor it is held against, said in [`super::lent::PutTally`]):
/// counted where each step runs, never estimated.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PutPath {
    /// Commits that laid out at least one segment ([`Plain::commit`]).
    pub laid_commits: u32,
    /// Bytes copied into a commit's layout ([`Plain::lay_out`]: payloads and their padding).
    pub laid_bytes: u64,
    /// Bytes a commit's or a carry's segment digest read ([`format::seal_parts`]: header,
    /// payloads, index, footer head), on the thread that commits.
    pub digested_bytes: u64,
    /// Segments a commit or a carry wrote (bytes already under a named segment write none).
    pub segments_written: u32,
    /// Bytes of the segments a compaction's merge streamed (each byte hashed as it is written).
    pub merged_bytes: u64,
    /// `Effect::Barrier`s the seal issued, and `Effect::Sync`s of a segment (the seal's of a
    /// published one, a durable store's of every one).
    pub barriers: u32,
    pub syncs: u32,
    /// Keys a root record names directly, each looked for when the root was set (super::driven
    /// `Driven::root_names_held`, D200's set-time guard): its cost, visible.
    pub root_probes: u64,
}

/// One namespace's held puts, and where each key is in them.
pub(crate) struct Held {
    batch: Batch,
    at: BTreeMap<Key, usize>,
}

impl Held {
    pub(crate) fn new(namespace: Namespace) -> Held {
        Held { batch: Batch::new(namespace), at: BTreeMap::new() }
    }

    fn entry(&self, key: &Key) -> Option<&Entry> {
        self.at.get(key).map(|&i| &self.batch.entries[i])
    }

    pub(crate) fn batch(&self) -> &Batch {
        &self.batch
    }

    /// The record under `key` held here: its tag and payload.
    pub(crate) fn get(&self, key: &Key) -> Option<(Tag, &[u8])> {
        self.entry(key).map(|entry| (entry.tag, self.batch.payload(entry)))
    }

    /// `value` under `key`, unless held already: [`Put::Same`] for the same record, a
    /// [`PutError::Mismatch`] naming both for other bytes.
    pub(crate) fn put(&mut self, key: Key, tag: Tag, value: &[u8]) -> Result<Put, PutError> {
        if let Some(entry) = self.entry(&key) {
            if entry.tag == tag && entry.len as usize == value.len() && entry.crc == record_crc(&key.0, tag, value) {
                return Ok(Put::Same);
            }
            let kept = self.batch.payload(entry);
            return Err(PutError::Mismatch(two_values(&key, Holder::ThisProcess, entry.tag, kept, tag, value)));
        }
        self.batch.put(key, tag, value)?;
        self.at.insert(key, self.batch.entries.len() - 1);
        Ok(Put::Written)
    }

    /// Every record of `other` not held here, copied in after this one's (a commit that failed
    /// gives its records back ahead of the puts made while it ran; a put checked both, so no
    /// key is held with two values).
    pub(crate) fn absorb(&mut self, other: &Held) {
        for entry in &other.batch.entries {
            let key = Key(entry.key);
            if self.at.contains_key(&key) {
                continue;
            }
            self.batch.put_known(entry, other.batch.payload(entry)).expect("it fit before");
            self.at.insert(key, self.batch.entries.len() - 1);
        }
    }
}

fn tag_of(tags: u64, op: u8, index: u32) -> EffectTag {
    EffectTag(tags + (u64::from(op) << 32) + u64::from(index))
}

fn failed(done: &Done) -> Option<String> {
    match done {
        Done::Failed(error) => Some(error.0.clone()),
        Done::Broken { error, .. } => Some(error.0.clone()),
        _ => None,
    }
}

/// Which of the store's files `file` (the store's directory, or a file in it) is, for an
/// error that names it by role and id.
fn store_file(file: &FileName) -> StoreFile {
    let Some((_, leaf)) = file.as_str().rsplit_once('/') else { return StoreFile::Directory };
    match parse_leaf(leaf) {
        Some((id, true)) => StoreFile::Log { id },
        Some((id, false)) => StoreFile::Segment { id },
        None if leaf == HEAD => StoreFile::Head,
        None if leaf == FLIP => StoreFile::FlipLock,
        None if leaf == CACHEDIR_TAG => StoreFile::CacheTag,
        None => unreachable!("{file}: the store asks effects only on the files it names"),
    }
}

/// `done` failed `doing` on `file`: the driver's error (its text: blaze-core's `IoError`
/// carries no errno), with the file by role.
fn io(doing: &'static str, file: &FileName, done: &Done) -> Option<StoreError> {
    failed(done).map(|error| StoreError::Io { doing, file: store_file(file), error })
}

/// Whether `leaf` is the name of one of a plain store's own files, the cache tag apart: its
/// head, its flip lock, a segment (`seg-<16 hex>`, a merge's temporary copy too) or a log
/// (`manifests-<16 hex>`). What a folder of another form is told by (blaze-cli's note of other
/// forms, and the legacy layout of format 4 and before, whose files lay directly in the
/// store's root).
pub fn store_leaf(leaf: &str) -> bool {
    leaf == HEAD || leaf == FLIP || leaf == BLOOM || parse_leaf(leaf).is_some()
}

/// The id of a file named `seg-<16 hex>` or `manifests-<16 hex>`, and whether it is a log.
fn parse_leaf(leaf: &str) -> Option<(u64, bool)> {
    let (hex, log) = match (leaf.strip_prefix("seg-"), leaf.strip_prefix("manifests-")) {
        (Some(hex), _) => (hex, false),
        (_, Some(hex)) => (hex, true),
        _ => return None,
    };
    (hex.len() == 16).then(|| u64::from_str_radix(hex, 16).ok()).flatten().map(|id| (id, log))
}

impl<A: Allocator + Clone> Plain<A> {
    #[cfg(test)]
    pub(crate) fn planted(cfg: PlainConfig<A>, alloc: A, plant: Plant) -> Plain<A> {
        let mut it = Plain::new(cfg, alloc);
        it.plant = plant;
        it
    }

    #[cfg(test)]
    fn plant(&self) -> Plant {
        self.plant
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn plant(&self) -> Plant {
        Plant::None
    }

    /// A store merging by `tiers` (a test's geometry, or its planted policy), its copies
    /// streamed through a buffer of `merge_buffer` bytes and their indexes in chunks of
    /// `index_chunk` (a test's, so KiB merges take many chunks).
    #[cfg(test)]
    pub(crate) fn tiered(cfg: PlainConfig<A>, alloc: A, plant: Plant, tiers: Tiers, merge_buffer: u64, index_chunk: u64) -> Plain<A> {
        let mut it = Plain::planted(cfg, alloc, plant);
        it.tiers = tiers;
        it.merge_buffer = merge_buffer;
        it.index_chunk = index_chunk;
        it
    }

    #[cfg(test)]
    pub(crate) fn merge_buffer(&self) -> u64 {
        self.merge_buffer
    }

    /// The bytes of records a compaction's copy holds at once: [`MERGE_BUFFER`].
    #[cfg(not(test))]
    #[inline(always)]
    pub(crate) fn merge_buffer(&self) -> u64 {
        MERGE_BUFFER
    }

    #[cfg(test)]
    pub(crate) fn index_chunk(&self) -> u64 {
        self.index_chunk
    }

    /// The most bytes of index a compaction's copy holds at once: [`INDEX_CHUNK`].
    #[cfg(not(test))]
    #[inline(always)]
    pub(crate) fn index_chunk(&self) -> u64 {
        INDEX_CHUNK
    }

    /// The most bytes a streamed copy built at once since the last call (its index chunk and
    /// its cursors), cleared: what the merge meter checks beside the writes in flight.
    #[cfg(test)]
    pub(crate) fn take_merge_held_most(&mut self) -> u64 {
        std::mem::take(&mut self.merge_held_most)
    }

    #[cfg(test)]
    pub(crate) fn tiers(&self) -> Tiers {
        self.tiers
    }

    /// The merge policy's geometry: [`Tiers::STORE`].
    #[cfg(not(test))]
    #[inline(always)]
    pub(crate) fn tiers(&self) -> Tiers {
        Tiers::STORE
    }

    /// The most segments this generation may name once cleanup has run: [`SEGMENTS_MOST`]
    /// (the invariant's bound at [`NAMESPACE_BYTES_STATED`]) for each namespace it names. What
    /// blaze-cli's "its compaction is behind" line counts against.
    pub fn named_most(&self) -> u32 {
        let mut namespaces: Vec<Namespace, &A> = Vec::new_in(&self.alloc);
        for seg in &self.manifest.segments {
            if !namespaces.contains(&seg.namespace) {
                namespaces.push(seg.namespace);
            }
        }
        (namespaces.len().max(1) * SEGMENTS_MOST) as u32
    }

    /// Whether one command's cleanup runs another slice after `so_far` (its slices summed,
    /// [`Sliced::and`]) of `slices` slices, the last of which was `last`: the last was a TIERED
    /// MERGE ([`Sliced::merged`]: a pruning or sweeping run ends it, so a cold build's thousands
    /// of dead records never buy a long cleanup after the answer, each command sweeping one
    /// slice), replaced something and was not aborted (a slice that finds nothing to do ends
    /// it, and so does another process's), fewer than [`CLEANUP_SLICES_MOST`] ran, and the bytes they read are
    /// under THE WORK BUDGET: the amortisation's bound (`Tiers::merged_most`) of what this
    /// process committed, the bytes of its commits' segments and their count. So the first
    /// slice always runs (what a command did before, amortised over the history), and the
    /// slices after it read at most the budget and one slice more; what is left over, a
    /// namespace still over its bound, is the next command's. One namespace's invariant is
    /// restored by one merge, so a command that commits enough restores every namespace.
    ///
    /// Under [`Liveness::Reached`] it also goes on whenever a MERGE IS DUE next (a namespace
    /// breaks the invariant over the rows a slice may take; `plan_reached` plans that merge
    /// first): after a sweep whose copy landed in a full tier, and after a FIRST slice that
    /// replaced nothing because the command's own rows were still leased when it was planned
    /// (the answer's commit, which named that slice, let the lease go: `release_lease`), so the
    /// command's own commits are merged in its own cleanup. A slice that replaced nothing goes
    /// on only while the cleanup has read nothing at all (`so_far.bytes_in == 0`: every
    /// planned slice reads its candidates), so a merge planned and then found to change nothing
    /// ends it (rule 18: no loop of empty slices). Still only merges go on, and the invariant
    /// holds after every cleanup in sweep mode too.
    pub fn cleanup_goes_on(&self, so_far: &Sliced, slices: u32, last: &Sliced) -> bool {
        if self.plant() == Plant::CleanupOneSlice {
            return false;
        }
        let merge_due = || self.cfg.liveness == Liveness::Reached && !self.plan_tiered(|it| self.sweepable(it, self.now_ns)).is_empty();
        let replaced = last.segments > 0;
        // The planted starvation keeps the old stop after a sweep, not the lease's case.
        let after_sweep = replaced && self.plant() != Plant::SweepBeforeMerge;
        let on = (last.merged && replaced) || ((after_sweep || so_far.bytes_in == 0) && merge_due());
        on && !last.aborted && slices < CLEANUP_SLICES_MOST && so_far.bytes_in < self.cleanup_budget()
    }

    /// The budget [`Plain::cleanup_goes_on`] holds a cleanup to, in bytes read.
    pub fn cleanup_budget(&self) -> u64 {
        self.tiers().merged_most(self.written_here, self.segments_here)
    }

    /// The rows this generation names, in its order: what the tier oracle counts.
    #[cfg(test)]
    pub(crate) fn named_rows(&self) -> &[Segment] {
        &self.manifest.segments
    }

    /// The store `cfg` names, what it keeps in `alloc` (the caller's: rule 75).
    pub fn new(cfg: PlainConfig<A>, alloc: A) -> Plain<A> {
        assert!(cfg.writes.contains(&Namespace::LOCAL), "a plain store writes the local namespace: its roots live there");
        let name = |leaf: &str| FileName::new(&format!("{}/{leaf}", cfg.dir.as_str())).expect("a store's directory names files under it");
        let a = || alloc.clone();
        let mut held = Vec::with_capacity_in(cfg.writes.len(), a());
        held.extend(cfg.writes.iter().map(|it| Held::new(*it)));
        Plain {
            head: name(HEAD),
            flip: name(FLIP),
            bloom_file: name(BLOOM),
            bloom: BloomIndex::new(),
            bloom_loading: false,
            open_parked: None,
            pending_bloom: blaze_own::Vec::new_in(BLOOM_ALLOC),
            last_hit: None,
            hint_gen: None,
            boot: Boot(cfg.boot),
            checks: std::cell::Cell::new(Checks::default()),
            checked_ids: Vec::new_in(a()),
            sealed_ids: Vec::new_in(a()),
            scrubbing: None,
            held,
            sealed: Vec::new_in(a()),
            committing: Vec::new_in(a()),
            cfg,
            #[cfg(test)]
            plant: Plant::None,
            #[cfg(test)]
            tiers: Tiers::STORE,
            #[cfg(test)]
            merge_buffer: MERGE_BUFFER,
            #[cfg(test)]
            index_chunk: INDEX_CHUNK,
            #[cfg(test)]
            merge_held_most: 0,
            phase: Phase::New,
            slot: None,
            manifest: Manifest::default(),
            maps: Vec::new_in(a()),
            by_id: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: a(), capacity: None }),
            pending_map_files: Vec::new_in(a()),
            reach_epoch: 0,
            reach_cache: None,
            view: Vec::new_in(a()),
            unchecked: BTreeMap::new(),
            eager: false,
            opened_ns: 0,
            probes: Probes::default(),
            roots_pending: Vec::new_in(a()),
            checked: BTreeSet::new(),
            dropped: BTreeSet::new(),
            orphans: Vec::new_in(a()),
            marks: Vec::new_in(a()),
            unreported: Vec::new_in(a()),
            clearable: Vec::new_in(a()),
            report: Report::default(),
            committed: None,
            sliced: None,
            broken: None,
            removing: Vec::new_in(a()),
            seq: 0,
            listed: Vec::new_in(a()),
            flip_parked: None,
            orphans_for_compact: Vec::new_in(a()),
            evict_for_compact: None,
            touched_roots: None,
            late_sync: Vec::new_in(a()),
            unopened: Vec::new_in(a()),
            head_made: false,
            log_lost: false,
            holds_lock: false,
            lock_waited: LockWaited::default(),
            head_torn: None,
            torn_stamp: false,
            opened_generation: 0,
            now_ns: 0,
            leased: false,
            released: false,
            folding: None,
            fold_asked: false,
            touches: Vec::new_in(a()),
            carrying: Vec::new_in(a()),
            carried_in_commit: None,
            carry_rounds: 0,
            carried: 0,
            written_here: 0,
            segments_here: 0,
            indexes_made: Vec::new_in(a()),
            named: Named::default(),
            put_path: PutPath::default(),
            alloc,
        }
    }

    /// What this process's commits did on the disk side of the put path, so far.
    pub fn put_path(&self) -> PutPath {
        self.put_path
    }

    /// The allocator the caller handed this store: what it keeps, and what its lists are
    /// answered in.
    pub fn alloc(&self) -> &A {
        &self.alloc
    }

    fn name(&self, leaf: &str) -> FileName {
        FileName::new(&format!("{}/{leaf}", self.cfg.dir.as_str())).expect("a store's directory names files under it")
    }

    fn prefixed_hex_file(&self, prefix: &[u8], id: u64) -> FileName {
        let dir = self.cfg.dir.as_str().as_bytes();
        let total = dir.len().saturating_add(prefix.len()).saturating_add(16);
        let mut buf = [0u8; 128];
        if let Some(target) = buf.get_mut(..total) {
            let (head, rest) = target.split_at_mut(dir.len());
            head.copy_from_slice(dir);
            let (mid, tail) = rest.split_at_mut(prefix.len());
            mid.copy_from_slice(prefix);
            if let Ok(hex_dst) = <&mut [u8; 16]>::try_from(tail) {
                write_hex_u64(id, hex_dst);
                if let Ok(text) = core::str::from_utf8(target)
                    && let Some(file) = FileName::new(text)
                {
                    return file;
                }
            }
        }
        let mut hex_buf = [0u8; 16];
        write_hex_u64(id, &mut hex_buf);
        let mut path = blaze_own::String::new_in(&self.alloc);
        path.push_str(self.cfg.dir.as_str());
        if let Ok(p) = core::str::from_utf8(prefix) {
            path.push_str(p);
        }
        if let Ok(h) = core::str::from_utf8(&hex_buf) {
            path.push_str(h);
        }
        FileName::new(path.as_str()).expect("a store's directory names files under it")
    }

    fn segment_file(&self, id: u64) -> FileName {
        self.prefixed_hex_file(b"/seg-", id)
    }

    fn log_file(&self, id: u64) -> FileName {
        self.prefixed_hex_file(b"/manifests-", id)
    }

    /// Formats `seg-<id>` once into a [`FileName`], caches a shared handle in
    /// `self.pending_map_files[row]` so [`Plain::on_map`] avoids re-allocating it, and appends
    /// the [`Effect::Map`].
    pub(super) fn queue_map<E: Allocator>(&mut self, row: usize, id: u64, effects: &mut Vec<Effect, E>) {
        let file = self.segment_file(id);
        if self.pending_map_files.len() <= row {
            self.pending_map_files.resize_with(row.saturating_add(1), || None);
        }
        if let Some(slot) = self.pending_map_files.get_mut(row) {
            *slot = Some(file.share());
        }
        let idx = u32::try_from(row).unwrap_or(u32::MAX);
        effects.push(Effect::Map { file, extent: Extent::Whole, tag: self.tag(MAP, idx) });
    }

    /// Pre-reserves per-segment tables for `additional` upcoming `Effect::Map`s so `map_all` /
    /// `map_new` avoid repeated reallocations.
    pub(super) fn reserve_maps<E: Allocator>(&mut self, additional: usize, sealed_additional: usize, effects: &mut Vec<Effect, E>) {
        if additional == 0 {
            return;
        }
        effects.reserve(additional);
        self.maps.reserve(additional);
        self.by_id.reserve(additional);
        if sealed_additional > 0 {
            self.sealed_ids.reserve(sealed_additional);
        }
        let need_slots = self.manifest.segments.len();
        if self.pending_map_files.len() < need_slots {
            self.pending_map_files.resize_with(need_slots, || None);
        }
    }

    /// Namespace of manifest row `row`, if in bounds (used by `Snapshot::of`).
    #[inline]
    pub(crate) fn row_namespace(&self, row: usize) -> Option<Namespace> {
        self.manifest.segments.get(row).map(|seg| seg.namespace)
    }


    fn tag(&self, op: u8, index: u32) -> EffectTag {
        tag_of(self.cfg.tags, op, index)
    }

    /// Whether `tag` is one of this store's.
    pub fn owns(&self, tag: EffectTag) -> bool {
        tag.0 >= self.cfg.tags && tag.0 < self.cfg.tags + TAGS
    }

    pub fn ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }

    /// What this process's flips waited on the flip lock so far: contended asks and the summed
    /// steps they were asked again after (thoughts2 5.3; the command's store line prints it).
    pub fn lock_waited(&self) -> LockWaited {
        self.lock_waited
    }

    pub fn broken(&self) -> Option<&StoreError> {
        self.broken.as_ref()
    }

    /// The generation this process reads.
    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }

    pub fn report(&self) -> &Report {
        &self.report
    }

    pub fn dir(&self) -> &FileName {
        &self.cfg.dir
    }

    pub(crate) fn head_file(&self) -> &FileName {
        &self.head
    }

    /// The last commit's outcome, once.
    pub fn take_committed(&mut self) -> Option<Result<Committed<A>, StoreError>> {
        self.committed.take()
    }

    /// The last compaction slice's outcome, once.
    pub fn take_sliced(&mut self) -> Option<Sliced> {
        self.sliced.take()
    }

    /// Records put and not yet committed.
    pub fn unsynced(&self) -> usize {
        self.held.iter().map(|it| it.batch.records()).sum()
    }

    /// The namespaces puts may name.
    pub fn writes(&self) -> &[Namespace] {
        &self.cfg.writes
    }

    /// Allocates an empty `blaze_own::Vec` in the store's allocator.
    fn own_vec<T>(&self) -> blaze_own::Vec<T, A> {
        blaze_own::Vec::new_in(self.alloc.clone())
    }

    /// Invalidates the cached root reachability map whenever non-compaction state changes.
    fn invalidate_reach(&mut self) {
        self.reach_epoch = self.reach_epoch.wrapping_add(1);
        self.reach_cache = None;
    }

    /// Allocates an empty vector in the store's allocator.
    fn alloc_vec<T>(&self) -> Vec<T, A> {
        Vec::new_in(self.alloc.clone())
    }

    /// Compaction prunes while the segments hold more than `cap_bytes`.
    pub fn set_cap(&mut self, cap_bytes: u64) {
        self.cfg.cap_bytes = cap_bytes;
    }

    /// The store's configured byte budget (`u64::MAX` = unlimited).
    pub fn cap_bytes(&self) -> u64 {
        self.cfg.cap_bytes
    }

    /// The high watermark (`WATERMARK_HIGH_PCT`% of `cap_bytes`): root-LRU eviction triggers
    /// once committed and live bytes exceed this threshold.
    pub fn high_watermark_bytes(&self) -> u64 {
        watermark_bytes(self.cfg.cap_bytes, WATERMARK_HIGH_PCT)
    }

    /// The low watermark (`WATERMARK_LOW_PCT`% of `cap_bytes`): root-LRU eviction expires
    /// oldest roots down toward this threshold for hysteresis.
    pub fn low_watermark_bytes(&self) -> u64 {
        watermark_bytes(self.cfg.cap_bytes, WATERMARK_LOW_PCT)
    }

    /// Whether total committed segment bytes exceed the high watermark of a finite budget.
    pub fn over_budget(&self) -> bool {
        self.cfg.cap_bytes != u64::MAX && self.manifest.bytes() > self.high_watermark_bytes()
    }

    /// Records `name` as touched by this process so LRU eviction never drops it.
    fn mark_touched_root(&mut self, name: &str) {
        if self.is_touched_root(name) {
            return;
        }
        let Ok(len) = u16::try_from(name.len()) else { return };
        if self.touched_roots.is_none() {
            self.touched_roots = Some(self.own_vec());
        }
        if let Some(buf) = &mut self.touched_roots {
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
        }
        self.invalidate_reach();
    }

    /// Whether `name` was set or renewed by this process (including fresh no-op renewals).
    fn is_touched_root(&self, name: &str) -> bool {
        let Some(buf) = &self.touched_roots else { return false };
        let mut at = 0usize;
        while let Some(head) = buf.get(at..at + 2) {
            let Ok(raw) = <[u8; 2]>::try_from(head) else { break };
            let len = usize::from(u16::from_le_bytes(raw));
            let end = at + 2 + len;
            let Some(entry) = buf.get(at + 2..end) else { break };
            if entry == name.as_bytes() {
                return true;
            }
            at = end;
        }
        false
    }

    /// The bytes this generation's segments hold.
    pub fn bytes(&self) -> u64 {
        self.manifest.bytes()
    }

    /// The segments this generation names, oldest first: `(id, records)`.
    pub fn segments(&self) -> Vec<(u64, u32), A> {
        let mut out = Vec::with_capacity_in(self.manifest.segments.len(), self.alloc.clone());
        out.extend(self.manifest.segments.iter().map(|it| (it.id, it.count)));
        out
    }

    /// The roots this generation names: `(name, record key)`.
    pub fn roots(&self) -> Vec<(String, Key), A> {
        let mut out = Vec::with_capacity_in(self.manifest.roots.len(), self.alloc.clone());
        out.extend(self.manifest.roots.iter().map(|it| (it.name.clone(), Key(it.key))));
        out
    }

    /// The record the root `name` names, when the head names it unexpired at the store's clock
    /// (its last completion's wall time): what a writer renewing a root reads first, so an
    /// expired root, whose records a slice may have swept, is never named again.
    pub fn root_named(&self, name: &str) -> Option<Key> {
        self.manifest.roots.iter().find(|it| it.name == name && it.expires_ns > self.now_ns).map(|it| Key(it.key))
    }

    /// The roots this generation names with their expiry: `(name, record key, expires ns)`
    /// ([`format::NEVER`]: none).
    pub fn roots_expiring(&self) -> Vec<(String, Key, u64), A> {
        let mut out = Vec::with_capacity_in(self.manifest.roots.len(), self.alloc.clone());
        out.extend(self.manifest.roots.iter().map(|it| (it.name.clone(), Key(it.key), it.expires_ns)));
        out
    }

    /// Zero-allocation lookup of root `name` in the current manifest: returns its record key and
    /// the generation that wrote/updated it (used as a segment-locality probe hint).
    pub fn root_target(&self, name: &str) -> Option<(Key, u64)> {
        self.manifest.roots.iter().find(|it| it.name == name).map(|it| (Key(it.key), it.generation))
    }

    /// Number of active leases named by the current manifest, without allocating.
    pub fn leases_len(&self) -> usize {
        self.manifest.leases.len()
    }

    /// Sets an optional segment-generation locality hint consulted by [`Plain::find`].
    pub fn set_probe_hint(&mut self, generation: Option<u64>) {
        self.hint_gen = generation;
    }

    /// Whether [`Plain::commit`] / [`Plain::commit_records`] with empty inputs has any pending
    /// work (held records, root edits, open stamps, acknowledged loss marks, or a folded
    /// compaction slice).
    pub fn needs_commit(&self) -> bool {
        self.holds_uncommitted() || !self.roots_pending.is_empty() || self.pending_stamps() || self.folding.is_some() || !self.clearable.is_empty()
    }

    /// Whether [`Plain::seal`] has any unsealed local segments of this boot or pending stamps.
    pub fn needs_seal(&self) -> bool {
        self.pending_stamps() || self.manifest.segments.iter().any(|it| it.local && !it.sealed && it.boot == self.cfg.boot)
    }

    /// Whether row `row` of `self.manifest.segments` may contain `key` according to its
    /// verified per-segment Bloom filter (never returns `false` for earlier-boot unsealed
    /// crash-tail segments or segments without a valid filter).
    #[inline]
    pub(super) fn may_contain_row(&self, row: usize, key: &Key) -> bool {
        let Some(seg) = self.manifest.segments.get(row) else {
            return true;
        };
        if seg.count == 0 {
            return false;
        }
        if seg.boot != self.cfg.boot && !seg.sealed {
            return true;
        }
        self.bloom.may_contain(seg, &key.0.0, self.plant() == Plant::TrustsCorruptBloom)
    }

    /// Shared mapped `bloom` sidecar handle for [`super::snapshot::Snapshot`] (one clone per
    /// snapshot, zero per-segment `Mapping` clones).
    pub(crate) fn bloom_shared_mapped(&self) -> Option<BloomMapped> {
        self.bloom.shared_mapped()
    }

    /// Lightweight `Send + Sync` Bloom handle for `seg` (for [`super::snapshot::Snapshot`]);
    /// mapped-record CRC verification is deferred lazily to the first probe of `seg`.
    pub(crate) fn bloom_ref_for(&self, seg: &Segment) -> Option<BloomRef> {
        if seg.boot != self.cfg.boot && !seg.sealed {
            return None;
        }
        self.bloom.snapshot_ref(seg, self.plant() == Plant::TrustsCorruptBloom)
    }

    /// Records a newly built framed Bloom record for `digest`, awaiting the next flip.
    fn record_pending_bloom(&mut self, digest: ContentId, record: blaze_own::Vec<u8, Kept>) {
        if let Some((_, existing)) = self.pending_bloom.iter_mut().find(|(d, _)| *d == digest) {
            *existing = record;
        } else {
            self.pending_bloom.push((digest, record));
        }
    }

    /// Plans the optional `bloom` sidecar write for a flip whose resulting manifest is `m`.
    /// When the store is whole (`self.eager`) and any live local segment lacks a filter (e.g.
    /// upgrading an existing store) or dead bytes exceed 2x live + 64 KiB, writes a compact
    /// replacement via atomic `WriteUnsynced`; otherwise appends only newly built records.
    /// Returns `(effect, replaced, written_bytes)`.
    fn plan_bloom_effect(&mut self, m: &Manifest) -> Option<(Effect, bool, u64)> {
        let has_pending = |pending: &[(ContentId, blaze_own::Vec<u8, Kept>)], digest: &ContentId| pending.iter().any(|(d, _)| d == digest);
        let needs_backfill = self.eager
            && m.segments
                .iter()
                .any(|seg| seg.local && !self.bloom.has(seg) && !has_pending(&self.pending_bloom, &seg.digest) && self.by_id.contains_key(&seg.id));
        let needs_compact = self.eager && self.bloom.needs_compact(&m.segments);
        if needs_backfill || needs_compact {
            let mut bytes: blaze_own::Vec<u8, Kept> = blaze_own::Vec::new_in(BLOOM_ALLOC);
            let mut backfilled: blaze_own::Vec<(ContentId, blaze_own::Vec<u8, Kept>), Kept> = blaze_own::Vec::new_in(BLOOM_ALLOC);
            for seg in m.segments.iter().filter(|it| it.local) {
                if let Some((_, rec)) = self.pending_bloom.iter().find(|(d, _)| *d == seg.digest) {
                    bytes.extend_from_slice(rec);
                } else if let Some(raw) = self.bloom.raw_record(seg) {
                    bytes.extend_from_slice(raw);
                } else if let Some(&at) = self.by_id.get(&seg.id)
                    && let Some(mapped) = self.maps.get(at)
                    && mapped.seg().digest == seg.digest
                {
                    let rec = BloomBuilder::from_run(mapped.run(), &seg.digest);
                    bytes.extend_from_slice(&rec);
                    backfilled.push((seg.digest, rec));
                }
            }
            for (digest, rec) in backfilled {
                self.record_pending_bloom(digest, rec);
            }
            if bytes.is_empty() {
                return None;
            }
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            return Some((Effect::WriteUnsynced { file: self.bloom_file.share(), bytes: bytes.into_iter().collect(), tag: self.tag(BLOOM_WRITE, 0) }, true, len));
        }
        if self.pending_bloom.is_empty() {
            return None;
        }
        let mut bytes: blaze_own::Vec<u8, Kept> = blaze_own::Vec::new_in(BLOOM_ALLOC);
        for (digest, rec) in self.pending_bloom.iter() {
            if m.segments.iter().any(|seg| seg.local && seg.digest == *digest) {
                bytes.extend_from_slice(rec);
            }
        }
        if bytes.is_empty() {
            return None;
        }
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Some((Effect::Append { file: self.bloom_file.share(), bytes: bytes.into_iter().collect(), tag: self.tag(BLOOM_WRITE, 0) }, false, len))
    }

    /// The leases this generation names: `(holder's nonce, expires ns)`.
    pub fn leases(&self) -> Vec<(u64, u64), A> {
        let mut out = Vec::with_capacity_in(self.manifest.leases.len(), self.alloc.clone());
        out.extend(self.manifest.leases.iter().map(|it| (it.holder, it.expires_ns)));
        out
    }

    /// This process holds the flip lock for a flip of cleanup's own (a slice not folded into a
    /// commit): what another process's wait on the lock then waits for (cleanup_tests counts
    /// these; the folded slice makes none).
    #[cfg(test)]
    pub(crate) fn cleanup_holds_lock(&self) -> bool {
        self.holds_lock && matches!(&self.phase, Phase::Flipping(flip) if matches!(flip.change, Change::Compact { .. }))
    }

    fn stop(&mut self, why: StoreError) {
        self.broken = Some(why);
        self.phase = Phase::Broken;
    }

    /// Starts the open: the head, its manifest, every segment it names mapped. No lock.
    pub fn open(&mut self, effects: &mut Vec<Effect>) {
        assert!(matches!(self.phase, Phase::New), "a plain store is opened once");
        self.phase = Phase::ReadHead(Ctx::Open);
        effects.push(Effect::ReadAll { file: self.head.share(), tag: self.tag(HEAD_READ, 0) });
    }

    // ---- reads: no lock, no write to anything shared ----

    /// The newest committed entry of `key` in the mapped segments of `spaces`, asked in the
    /// order given (each namespace's segments newest first): one probe of each segment's index
    /// until one holds it. Each segment here is proved ([`Verified`]): the probe reads the entry
    /// it lands on and checks nothing (its layout was checked once, with every byte), so a
    /// lookup cannot fail.
    ///
    /// The MAPPED rows alone, in probe order: every row once the store is whole
    /// ([`Plain::map_all`]); before that only [`Plain::find`] reaches the rest, and the `&self`
    /// reads that call this assert a whole store. (A put asks it before whole: it sees what is
    /// mapped, and the commit, which needs a whole store, refuses two values under one key.)
    fn find_in(&self, spaces: &[Namespace], key: &Key) -> Option<(usize, Entry)> {
        if self.plant() == Plant::IgnoresNamespace {
            return self
                .view
                .iter()
                .filter(|&&row| self.may_contain_row(row, key))
                .filter_map(|&row| self.mapped_row(row))
                .find_map(|at| self.probe(at, key));
        }
        spaces.iter().find_map(|space| {
            self.view
                .iter()
                .filter(|&&row| self.manifest.segments[row].namespace == *space && self.may_contain_row(row, key))
                .filter_map(|&row| self.mapped_row(row))
                .find_map(|at| self.probe(at, key))
        })
    }

    fn probe(&self, at: usize, key: &Key) -> Option<(usize, Entry)> {
        let run = self.maps[at].run();
        format::find(run, &key.0).map(|i| (at, Entry::read(&run[i * ENTRY..(i + 1) * ENTRY])))
    }

    /// [`Plain::get`] in `spaces` alone, asked in their order (the caller's, never more): for
    /// each, what this process holds uncommitted under it, then its committed segments.
    pub fn get_in(&self, spaces: &[Namespace], key: &Key) -> Option<Got<'_>> {
        assert!(self.eager, "Plain::get_in reads a whole store: Plain::map_all (or open_whole) first, or Plain::find, which maps at a first probe");
        for space in spaces {
            if let Some(got) = self.held_get(*space, key) {
                return Some(got);
            }
            if let Some((at, entry)) = self.find_in(std::slice::from_ref(space), key) {
                let mut checks = self.checks.get();
                checks.reads += 1;
                self.checks.set(checks);
                let mapped = &self.maps[at];
                return Some(Got { tag: entry.tag, namespace: mapped.seg().namespace, bytes: mapped.payload(&entry) });
            }
        }
        if self.plant() == Plant::IgnoresNamespace {
            return self.held.iter().chain(self.sealed.iter()).find_map(|held| held.entry(key).map(|entry| Got { tag: entry.tag, namespace: held.batch.namespace, bytes: held.batch.payload(entry) }));
        }
        None
    }

    /// The proved segment `at` of this process's maps, for the store handle's table.
    pub(crate) fn mapped_at(&self, at: usize) -> &Verified {
        &self.maps[at]
    }

    /// How many segments this process has mapped: the handle's table grows to it.
    pub(crate) fn mapped_count(&self) -> usize {
        self.maps.len()
    }

    /// How many full checks this process made and records it read; reads check nothing.
    pub fn checks(&self) -> Checks {
        self.checks.get()
    }

    /// The ids of the segments this process checked in full: each at most once a boot.
    pub fn fully_checked(&self) -> &[u64] {
        &self.checked_ids
    }

    /// The ids of the segments of another boot this process proved by their sealed rows: no
    /// byte of them read at the open.
    pub fn proved_by_seal(&self) -> &[u64] {
        &self.sealed_ids
    }

    /// The ids of the segments the generation this process holds names sealed.
    pub fn sealed_segments(&self) -> Vec<u64, A> {
        let mut out = Vec::new_in(self.alloc.clone());
        out.extend(self.manifest.segments.iter().filter(|it| it.sealed).map(|it| it.id));
        out
    }

    /// The caller's explicit ask (a `scrub` command), never a read's: every record of every
    /// segment mapped here checked against its CRC-32C as its bytes lie now. A byte that changed
    /// on the disk after its segment was proved is found here, or by the consumer that fails on
    /// it; a read never looks.
    pub fn scrub(&self) -> Vec<Corrupt, A> {
        assert!(self.eager, "Plain::scrub walks a whole store: Plain::map_all first");
        let mut out = Vec::new_in(self.alloc.clone());
        for map in self.maps.iter() {
            map.scrub_into(&mut out);
        }
        out
    }

    /// The segments of `spaces` this generation names that are not here: what a source is
    /// asked to fetch from on a miss.
    pub fn remote(&self, spaces: &[Namespace]) -> Vec<Segment, A> {
        let mut out = Vec::new_in(self.alloc.clone());
        out.extend(self.manifest.segments.iter().filter(|it| !it.local && spaces.contains(&it.namespace)).copied());
        out
    }

    /// The namespaces reads answer from.
    pub fn reads(&self) -> &[Namespace] {
        &self.cfg.reads
    }

    /// The files of the segments this process has mapped (each once, for its life).
    pub fn mapped_files(&self) -> impl Iterator<Item = &FileName> {
        self.maps.iter().map(|it| it.file())
    }

    /// What this process holds uncommitted under `key` in `namespace`: held, or sealed by the
    /// commit in flight.
    fn held_in(&self, namespace: Namespace, key: &Key) -> Option<(&Held, Entry)> {
        self.held.iter().chain(self.sealed.iter()).filter(|it| it.batch.namespace == namespace).find_map(|held| held.entry(key).map(|entry| (held, *entry)))
    }

    fn held_get(&self, namespace: Namespace, key: &Key) -> Option<Got<'_>> {
        self.held_in(namespace, key).map(|(held, entry)| Got { tag: entry.tag, namespace, bytes: held.batch.payload(&entry) })
    }

    /// Whether any record is held uncommitted: a reader that finds none asks only the mapped
    /// segments.
    pub(crate) fn holds_uncommitted(&self) -> bool {
        self.held.iter().chain(self.sealed.iter()).any(|it| !it.batch.is_empty())
    }

    /// The record under `key`, read in place from its proved segment's mapping: no check (the
    /// segment's bytes were proved once, `Verified`).
    pub fn get(&self, key: &Key) -> Option<Got<'_>> {
        self.get_in(&self.cfg.reads, key)
    }

    /// Where the committed record under `key` lies in `spaces` (asked in their order), for a
    /// read into the caller's buffer (`pread`) of a proved segment. Records put and not
    /// committed are [`Plain::get`]'s.
    pub fn locate_in(&self, spaces: &[Namespace], key: &Key) -> Option<Placed> {
        assert!(self.eager, "Plain::locate_in reads a whole store: Plain::map_all (or open_whole) first, or Plain::find and Plain::placed");
        self.find_in(spaces, key).map(|(at, entry)| {
            let mut checks = self.checks.get();
            checks.reads += 1;
            self.checks.set(checks);
            let mapped = &self.maps[at];
            Placed { key: *key, tag: entry.tag, namespace: mapped.seg().namespace, segment: mapped.seg().digest, file: mapped.file().share(), at: entry.at, len: entry.len }
        })
    }

    pub fn locate(&self, key: &Key) -> Option<Placed> {
        self.locate_in(&self.cfg.reads, key)
    }

    pub fn has(&self, key: &Key) -> bool {
        self.get(key).is_some()
    }

    // ---- puts ----

    /// Keeps `value` under `key` in the store's own batch, written by the next commit. A key
    /// already held with the same record is a no-op; with other bytes, a refusal naming it.
    ///
    /// A put of what the store holds writes nothing, decided from the entry alone: the key covers
    /// every input of its value, so an equal key names an equal value, and the entry's kind,
    /// length and CRC-32C (computed once from the put's bytes, which the record needs anyway)
    /// are compared with the put's, with no read of the payload kept. A difference there is two
    /// values under one key, refused; only then are the kept bytes read, to name both.
    pub fn put(&mut self, key: Key, namespace: Namespace, tag: Tag, value: &[u8]) -> Result<Put, PutError> {
        let Some(at) = self.cfg.writes.iter().position(|it| *it == namespace) else {
            return Err(PutError::NotWritable { namespace });
        };
        if namespace != Namespace::LOCAL
            && let Some((at, needle)) = self.machine_at(value)
        {
            return Err(PutError::Machine { key, namespace, at, needle });
        }
        let crc = record_crc(&key.0, tag, value);
        let same = |entry: &Entry| entry.tag == tag && entry.len as usize == value.len() && entry.crc == crc;
        if let Some((held, entry)) = self.held_in(namespace, &key) {
            if same(&entry) {
                return Ok(Put::Same);
            }
            let kept = held.batch.payload(&entry);
            return Err(PutError::Mismatch(two_values(&key, Holder::ThisProcess, entry.tag, kept, tag, value)));
        }
        if let Some((mapped, entry)) = self.find_in(std::slice::from_ref(&namespace), &key) {
            if same(&entry) {
                return Ok(Put::Same);
            }
            let kept = self.maps[mapped].payload(&entry);
            return Err(PutError::Mismatch(two_values(&key, Holder::Committed, entry.tag, kept, tag, value)));
        }
        let held = &mut self.held[at];
        held.batch.put(key, tag, value)?;
        held.at.insert(key, held.batch.entries.len() - 1);
        Ok(Put::Written)
    }

    /// Names `keys` as kept under `name` at the next commit: a [`ReservedTag::Root`] record of the
    /// sorted keys under [`Namespace::LOCAL`], and the root. Compaction keeps every record a
    /// root names.
    pub fn root(&mut self, name: &str, keys: &[Key]) -> Result<Key, StoreError> {
        self.root_for(name, keys, NEVER)
    }

    /// [`Plain::root`], kept for `life_ns` from the flip that sets it ([`NEVER`]: until it is
    /// unset): a checkout's answer, [`ROOT_LIFE_NS`]. A root already named with these keys and
    /// more than its life less [`TOUCH_NS`] left is not set again, so a build that changed
    /// nothing flips nothing for it.
    pub fn root_for(&mut self, name: &str, keys: &[Key], life_ns: u64) -> Result<Key, StoreError> {
        if keys.len() > ROOT_KEYS_MOST {
            return Err(StoreError::IndexOver { keys: keys.len() as u64, most: ROOT_KEYS_MOST as u64 });
        }
        let bytes = sorted_keys(keys, self.alloc.clone());
        let key = self.set_root(name, &bytes, life_ns, ReservedTag::Root.tag(), ROOT_DOMAIN)?;
        self.named.roots += 1;
        self.named.keys += (bytes.len() / 32) as u64;
        Ok(key)
    }

    /// The roots this process set so far and the keys they name (an index counted by its keys
    /// when this process made it, else as one key): the command's cleanup line.
    pub fn named(&self) -> Named {
        self.named
    }

    /// An INDEX of `keys` ([`ReservedTag::Index`]): one record of their sorted 32-byte keys under
    /// [`Namespace::LOCAL`], keyed by its bytes, so equal sets are one record that many roots
    /// share. It keeps nothing by itself: a root that names it through
    /// [`Plain::root_through`] keeps it and every key it holds (reach follows one level).
    /// Over [`INDEX_KEYS_MOST`] keys is refused by name ([`StoreError::IndexOver`]).
    pub fn index(&mut self, keys: &[Key]) -> Result<Key, StoreError> {
        let bytes = sorted_keys(keys, self.alloc.clone());
        let count = (bytes.len() / 32) as u64;
        if count > INDEX_KEYS_MOST as u64 {
            return Err(StoreError::IndexOver { keys: count, most: INDEX_KEYS_MOST as u64 });
        }
        let key = Key(ContentId::of(INDEX_DOMAIN, &bytes));
        let index = ReservedTag::Index.tag();
        self.put(key, Namespace::LOCAL, index, &bytes).map_err(|it| it.into_store(index))?;
        if !self.indexes_made.iter().any(|(it, _)| *it == key) {
            self.indexes_made.push((key, count));
        }
        Ok(key)
    }

    /// An index of `keys` filed BESIDE the record `owner`: under [`beside_key`]`(owner)`, not
    /// under its content, so a writer that names `owner` in a root finds the index again from
    /// the owner's key alone (`owner`'s records made with it: a names record's expansion
    /// records). The owner's key must cover what `keys` are (its record is a function of them):
    /// other keys under one owner are the store's two-values refusal (rule 21).
    pub fn index_beside(&mut self, owner: Key, keys: &[Key]) -> Result<Key, StoreError> {
        let bytes = sorted_keys(keys, self.alloc.clone());
        let count = (bytes.len() / 32) as u64;
        if count > INDEX_KEYS_MOST as u64 {
            return Err(StoreError::IndexOver { keys: count, most: INDEX_KEYS_MOST as u64 });
        }
        let key = beside_key(owner);
        let index = ReservedTag::Index.tag();
        self.put(key, Namespace::LOCAL, index, &bytes).map_err(|it| it.into_store(index))?;
        if !self.indexes_made.iter().any(|(it, _)| *it == key) {
            self.indexes_made.push((key, count));
        }
        Ok(key)
    }

    /// [`Plain::root_for`] of INDEXES: the root `name` names the index records `indexes` (each
    /// made by [`Plain::index`], in this process or before), kept for `life_ns`; reach keeps the
    /// root's record, each index and every key each index holds. Its record is filed under
    /// [`ReservedTag::IndexedRoot`], so a slice tells it from a plain root by the entry it already
    /// reads, with no probe of the keys it names. Over [`ROOT_INDEXES_MOST`] indexes is refused
    /// by name.
    pub fn root_through(&mut self, name: &str, indexes: &[Key], life_ns: u64) -> Result<Key, StoreError> {
        let bytes = sorted_keys(indexes, self.alloc.clone());
        let count = (bytes.len() / 32) as u64;
        if count > ROOT_INDEXES_MOST as u64 {
            return Err(StoreError::IndexOver { keys: count, most: ROOT_INDEXES_MOST as u64 });
        }
        let key = self.set_root(name, &bytes, life_ns, ReservedTag::IndexedRoot.tag(), INDEXED_ROOT_DOMAIN)?;
        self.named.roots += 1;
        for index in bytes.chunks_exact(32) {
            let index = Key(ContentId(index.try_into().expect("32 bytes")));
            let held = self.indexes_made.iter().find(|(it, _)| *it == index).map_or(1, |(_, n)| *n);
            self.named.keys += held;
            self.named.indexed += held;
        }
        Ok(key)
    }

    /// Queues a root set or lease-only renewal for the next commit.
    fn push_root_set(&mut self, name: &str, key: Key, life_ns: u64, renew_only: bool) {
        self.roots_pending.push(RootEdit::Set { name: name.to_owned(), key, life_ns, renew_only });
    }

    /// The root `name` set to the record `bytes` (sorted keys) under `tag`, its key derived
    /// under `domain`: the one body of [`Plain::root_for`] and [`Plain::root_through`].
    fn set_root(&mut self, name: &str, bytes: &[u8], life_ns: u64, tag: Tag, domain: &'static str) -> Result<Key, StoreError> {
        self.mark_touched_root(name);
        let key = Key(ContentId::of(domain, bytes));
        let pending = self.roots_pending.iter().any(|it| matches!(it, RootEdit::Set { name: n, .. } | RootEdit::Unset(n) if n == name));
        let fresh = life_ns != NEVER
            && !pending
            && self.manifest.roots.iter().any(|it| it.name == name && it.key == key.0 && it.expires_ns != NEVER && it.expires_ns >= self.now_ns.saturating_add(life_ns.saturating_sub(TOUCH_NS)));
        if fresh {
            // Named so already, long enough: nothing to write (its record is committed).
            return Ok(key);
        }
        self.put(key, Namespace::LOCAL, tag, bytes).map_err(|it| it.into_store(tag))?;
        self.push_root_set(name, key, life_ns, false);
        Ok(key)
    }

    /// O(1) lease renewal for an already-live root `name` at `now_ns`: checks the manifest's root
    /// entry without reading, probing, sorting, or hashing the keys it holds. Returns
    /// [`Renewed::Fresh`] when the root still has at least `life_ns - TOUCH_NS` remaining (no flip
    /// owed), [`Renewed::Extended`] when a metadata-only lease extension is queued for the next
    /// commit, and [`Renewed::Missing`] when `name` is absent, expired, or pending unset.
    pub fn renew_root(&mut self, name: &str, life_ns: u64, now_ns: u64) -> Renewed {
        for edit in self.roots_pending.iter_mut().rev() {
            match edit {
                RootEdit::Unset(n) if n == name => return Renewed::Missing,
                RootEdit::Set { name: n, key, life_ns: existing, .. } if n == name => {
                    let key = *key;
                    self.named.roots += 1;
                    let needs_longer = (life_ns == NEVER && *existing != NEVER)
                        || (life_ns != NEVER && *existing != NEVER && life_ns > *existing);
                    if needs_longer {
                        *existing = life_ns;
                        self.mark_touched_root(name);
                        return Renewed::Extended(key);
                    }
                    self.mark_touched_root(name);
                    return Renewed::Fresh(key);
                }
                _ => {}
            }
        }
        let Some(root) = self.manifest.roots.iter().find(|it| it.name == name && it.expires_ns > now_ns) else {
            return Renewed::Missing;
        };
        let key = Key(root.key);
        let root_expires = root.expires_ns;
        self.mark_touched_root(name);
        self.named.roots += 1;
        let fresh = (life_ns == NEVER && root_expires == NEVER)
            || (life_ns != NEVER && root_expires != NEVER && root_expires >= now_ns.saturating_add(life_ns.saturating_sub(TOUCH_NS)));
        if fresh {
            return Renewed::Fresh(key);
        }
        self.push_root_set(name, key, life_ns, true);
        Renewed::Extended(key)
    }

    /// O(segments + roots + retired) check of whether real compaction or GC housekeeping is due
    /// at `now_ns`, using only the in-memory manifest (no `map_all`, no index probes):
    /// - total committed bytes exceed the budget high watermark;
    /// - any tier of any namespace exceeds `K` open mergeable segments;
    /// - any root has expired (`expires_ns <= now_ns`);
    /// - any retired file is past `GRACE_NS` and ready to unlink.
    pub fn needs_housekeeping(&self, now_ns: u64) -> bool {
        if self.over_budget() {
            return true;
        }
        if self.manifest.roots.iter().any(|it| it.expires_ns != NEVER && it.expires_ns <= now_ns) {
            return true;
        }
        if self.manifest.retired.iter().any(|it| it.lost.is_none() && it.since_ns.saturating_add(GRACE_NS) <= now_ns) {
            return true;
        }
        let tiers = self.tiers();
        let segs = &self.manifest.segments;
        let blind = tiers.plant == TierPlant::PastCap;
        let mergeable = |seg: &Segment| {
            seg.local
                && !seg.published
                && (blind || !tiers.closed(seg.file_len()))
                && (self.cfg.liveness == Liveness::Everything || !self.leased(seg, now_ns))
        };
        for (idx, first) in segs.iter().enumerate() {
            if segs.iter().take(idx).any(|prev| prev.namespace == first.namespace) {
                continue;
            }
            let mut count = [0usize; TIERS_MOST];
            for seg in segs.iter().skip(idx).filter(|it| it.namespace == first.namespace && mergeable(it)) {
                let tier = tiers.tier(seg.file_len());
                if let Some(slot) = count.get_mut(tier) {
                    *slot += 1;
                    if *slot > tiers.most() {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Lets go of the root `name` at the next commit.
    pub fn unroot(&mut self, name: &str) {
        self.roots_pending.push(RootEdit::Unset(name.to_owned()));
    }

    /// Lets go of this process's lease at the next commit: the command's records are rooted by
    /// then (its answer's commit). No later commit sets it again. Nothing is written for it
    /// when no generation names it.
    pub fn release_lease(&mut self) {
        self.released = true;
        if self.leased && !self.roots_pending.iter().any(|it| matches!(it, RootEdit::Release)) {
            self.roots_pending.push(RootEdit::Release);
        }
    }

    /// Re-enables automatic lease acquisition on the next commit that writes segments, after a
    /// previous request released its lease ([`Plain::release_lease`]).
    ///
    /// Only takes effect when no lease for this process is currently held (`!self.leased`), so a
    /// resident daemon holds zero leases while idle (never blocking `blaze gc`) yet acquires fresh
    /// lease protection starting from the current generation on the next multi-phase request.
    pub fn rearm_lease(&mut self, now_ns: u64) {
        if !self.leased && self.released {
            self.released = false;
            self.opened_generation = self.manifest.generation;
            self.opened_ns = now_ns;
        }
    }

    // ---- the commit ----

    /// Writes the store's batches and `batches` (the workers', borrowed: their bytes are copied
    /// into the layout here, so a worker's batch stays readable by its worker until the flip
    /// lands) as one new segment for each namespace that holds records, one after another,
    /// with no lock, then flips the head once to a generation naming them all. The outcome is
    /// [`Plain::take_committed`]'s once the store is ready again.
    pub fn commit(&mut self, batches: &[&Batch], now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        self.commit_records(batches, &[], now_ns, effects)
    }

    /// [`Plain::commit`] with `records` lent where they lie besides the batches (a `Lent`'s
    /// shared arena, lent.rs `Visibility::InArena`): each laid out by its entry, copied once
    /// into the segment's piece, its CRC the one made at its put. The same segments as the
    /// same records put into a batch (arena_tests.rs, the seam's equality).
    fn empty_committed(&self) -> Committed<A> {
        Committed { generation: self.manifest.generation, segments: Vec::new_in(self.alloc.clone()) }
    }

    pub fn commit_records(&mut self, batches: &[&Batch], records: &[LentRecord<'_>], now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        // The layout refuses two values under one key against every committed row: a whole
        // store (rule 21). A commit with nothing to lay out (stamps alone) needs none.
        assert!(
            self.eager || (!self.holds_uncommitted() && batches.iter().all(|it| it.is_empty()) && records.is_empty()),
            "Plain::commit lays out against a whole store: Plain::map_all first"
        );
        if let Some(batch) = batches.iter().find(|it| !self.cfg.writes.contains(&it.namespace)) {
            return Err(StoreError::NotWritable { namespace: batch.namespace });
        }
        if let Some(record) = records.iter().find(|it| !self.cfg.writes.contains(&it.namespace)) {
            return Err(StoreError::NotWritable { namespace: record.namespace });
        }
        // Fast path: nothing to lay out, no pending roots/stamps/folded slice — skip batch
        // replacement and layout allocations completely.
        if batches.iter().all(|it| it.is_empty()) && records.is_empty() && !self.needs_commit() {
            self.committed = Some(Ok(self.empty_committed()));
            return Ok(());
        }
        // A worker's batch for a shared namespace is scanned as a put is (rule 25), before
        // anything of the commit is laid out.
        for batch in batches.iter().filter(|it| it.namespace != Namespace::LOCAL) {
            for entry in &batch.entries {
                if let Some((at, needle)) = self.machine_at(batch.payload(entry)) {
                    return Err(PutError::Machine { key: Key(entry.key), namespace: batch.namespace, at, needle }.into_store(entry.tag));
                }
            }
        }
        for record in records.iter().filter(|it| it.namespace != Namespace::LOCAL) {
            if let Some((at, needle)) = self.machine_at(record.bytes) {
                return Err(PutError::Machine { key: record.key, namespace: record.namespace, at, needle }.into_store(record.tag));
            }
        }
        // The held batches stay readable while they are written: sealed until the commit ends.
        let mut fresh = Vec::with_capacity_in(self.cfg.writes.len(), self.alloc.clone());
        fresh.extend(self.cfg.writes.iter().map(|it| Held::new(*it)));
        self.sealed = std::mem::replace(&mut self.held, fresh);
        let laid = self.lay_out(batches, records);
        let laid = match laid {
            Ok(laid) => laid,
            Err(why) => {
                self.sealed_back();
                return Err(why);
            }
        };
        // The measure (rule 16): what the layout copied, counted once it is made.
        if !laid.is_empty() {
            self.put_path.laid_commits += 1;
        }
        self.put_path.laid_bytes += laid.iter().flat_map(|it| it.pieces.iter()).map(|(_, bytes)| bytes.len() as u64).sum::<u64>();
        // A request larger than the cap alone can never fit under it: refused by name, as no
        // reclamation could make room for it (a commit is never refused for the cap otherwise:
        // over it, reclamation is owed, and the next slices pay it).
        let request: u64 = laid.iter().map(|it| it.index_at + it.run.len() as u64 + FOOTER as u64).sum();
        if request > self.cfg.cap_bytes {
            self.sealed_back();
            return Err(StoreError::OverCap { request_bytes: request, cap_bytes: self.cfg.cap_bytes });
        }
        self.committing.clear();
        self.carrying.clear();
        if let Some(carried) = &mut self.carried_in_commit {
            carried.clear();
        }
        self.carry_rounds = 0;
        // The roots this commit's records were put with: one set after it started is the next's.
        let mut roots = std::mem::replace(&mut self.roots_pending, Vec::new_in(self.alloc.clone()));
        // The segments of records this commit's roots name but did not write, dead in this
        // view: computed here, before the lock; the flip marks them touched.
        self.touches = match self.cfg.liveness {
            Liveness::Everything => Vec::new_in(self.alloc.clone()),
            Liveness::Reached => match self.reused(&roots, now_ns) {
                Ok(it) => it,
                Err(why) => {
                    self.roots_pending = roots;
                    self.sealed_back();
                    return Err(why);
                }
            },
        };
        if self.cfg.liveness == Liveness::Reached && !laid.is_empty() && !self.released {
            // Records no root names yet: this process's lease keeps them, and every segment
            // newer than the generation it opened on, until its answer roots them.
            roots.push(RootEdit::Lease { since: self.opened_generation + 1, opened_ns: self.opened_ns });
        }
        let mut rest = laid.into_iter();
        match rest.next() {
            None if roots.is_empty() && !self.pending_stamps() && self.folding.is_none() && self.clearable.is_empty() => {
                self.committed = Some(Ok(self.empty_committed()));
                self.sealed.clear();
            }
            None => self.start_flip(Change::Commit { segs: Vec::new_in(self.alloc.clone()), started_ns: now_ns, roots }, effects),
            Some(first) => {
                let mut left = Vec::with_capacity_in(rest.len(), self.alloc.clone());
                left.extend(rest);
                self.write_segment(first, Why::Commit { started_ns: now_ns, roots, rest: left, done: Vec::new_in(self.alloc.clone()) }, effects);
            }
        }
        Ok(())
    }

    /// One segment laid out for each namespace of [`PlainConfig::writes`] (in its order) that
    /// holds a record not already committed under it.
    ///
    /// Laid out in key order, whatever order the puts came in and whichever worker's batch
    /// holds each: two builders of the same records write equal bytes (rule 25), however
    /// their threads interleaved. A key in two batches with equal records is laid once (the
    /// workers converge on one record); with other bytes it is [`StoreError::TwoValues`]. The
    /// payloads are copied into one piece, so no batch is kept by the commit.
    fn lay_out<'b>(&'b self, batches: &[&'b Batch], records: &[LentRecord<'b>]) -> Result<Vec<Laid<A>, A>, StoreError> {
        let mut out = Vec::with_capacity_in(self.cfg.writes.len(), self.alloc.clone());
        for &namespace in self.cfg.writes.iter() {
            let own = self.sealed.iter().find(|it| it.batch.namespace == namespace).map(|it| &it.batch);
            // Every record of the namespace, the store's own batch's, each worker's and each
            // lent one's (where it lies: no copy before this layout's one): in the store's
            // allocator, freed when the commit's layout is made.
            let mut all: Vec<(Entry, &[u8]), &A> = Vec::new_in(&self.alloc);
            for batch in own.into_iter().chain(batches.iter().copied().filter(|it| it.namespace == namespace)) {
                all.extend(batch.entries.iter().map(|it| (*it, batch.payload(it))));
            }
            all.extend(records.iter().filter(|it| it.namespace == namespace).map(|it| (it.entry(), it.bytes)));
            all.sort_unstable_by(|a, b| a.0.key.cmp(&b.0.key));
            let mut entries: Vec<Entry, &A> = Vec::with_capacity_in(all.len(), &self.alloc);
            let mut kept_payloads: blaze_own::Vec<(&[u8], usize), &A> = blaze_own::Vec::with_capacity_in(all.len(), &self.alloc);
            let mut payload_bytes: u64 = 0;
            let mut last: Option<(Entry, &[u8])> = None;
            let unpadded = self.plant() == Plant::Unpadded;
            let off_by_one = self.plant() == Plant::IndexOffByOne;
            for (entry, payload) in all {
                if let Some((kept, kept_payload)) = last
                    && kept.key == entry.key
                {
                    if !kept.same_record(&entry) {
                        // Two workers put other bytes under one key: two values (rule 21).
                        return Err(StoreError::TwoValues(two_values(&Key(entry.key), Holder::ThisProcess, kept.tag, kept_payload, entry.tag, payload)));
                    }
                    continue;
                }
                last = Some((entry, payload));
                if ReservedTag::Pointer.is(entry.tag) {
                    // A pointer replaces, so it is always laid out, even when this process's view
                    // holds its value: that view may be older than another process's commit of
                    // another value, and this commit's row, after every row the generation it
                    // flips on names, must make this value the newest. A key held under another
                    // tag is two values (rule 21).
                    if let Some((mapped, kept)) = self.newest_mapped(namespace, &Key(entry.key))
                        && !ReservedTag::Pointer.is(kept.tag)
                    {
                        let kept_payload = self.maps[mapped].payload(&kept);
                        return Err(StoreError::TwoValues(two_values(&Key(entry.key), Holder::Committed, kept.tag, kept_payload, entry.tag, payload)));
                    }
                } else if let Some((mapped, kept)) = self.find_in(std::slice::from_ref(&namespace), &Key(entry.key)) {
                    if kept.same_record(&entry) {
                        // Committed already: not laid again.
                        continue;
                    }
                    // A record committed under the key with other bytes (rule 21).
                    let kept_payload = self.maps[mapped].payload(&kept);
                    return Err(StoreError::TwoValues(two_values(&Key(entry.key), Holder::Committed, kept.tag, kept_payload, entry.tag, payload)));
                }
                let at = HEADER as u64 + payload_bytes;
                let end = at + u64::from(entry.len);
                if end > OFFSET_MOST {
                    return Err(StoreError::TooLong { tag: entry.tag, key: entry.key, len: end });
                }
                let padded = if unpadded { end } else { aligned(end) };
                let pad_len = usize::try_from(padded - end).unwrap_or(0);
                payload_bytes = padded - HEADER as u64;
                let mut placed = entry;
                placed.at = at;
                if off_by_one {
                    placed.at += ALIGN;
                }
                entries.push(placed);
                kept_payloads.push((payload, pad_len));
            }
            if entries.is_empty() {
                continue;
            }
            let index_at = HEADER as u64 + payload_bytes;
            // THE CAP, before a byte is allocated or written: a commit's segment past it would be a row
            // every later open refuses (`format::Row`), so the commit is refused here, naming
            // its namespace and the lengths. (Alignment and the index's end are the layout's
            // own, `seal_parts`; the row's whole check is at the flip, `Manifest::encode`.)
            let len = index_at.saturating_add(entries.len() as u64 * ENTRY as u64 + FOOTER as u64);
            let most = self.tiers().segment_most();
            if len > most {
                return Err(StoreError::Shape { namespace, why: RowRefused::TooLong { len, most } });
            }
            // The payloads from [`HEADER`] on, each at a multiple of [`ALIGN`] of the segment:
            // exact single allocation on the process heap (no doubling reallocs / full memcpys).
            let cap = usize::try_from(payload_bytes).unwrap_or(usize::MAX);
            let mut bytes: Vec<u8> = Vec::with_capacity_in(cap, Global);
            for (payload, pad_len) in kept_payloads {
                bytes.extend_from_slice(payload);
                if pad_len > 0 {
                    bytes.resize(bytes.len() + pad_len, 0);
                }
            }
            let mut pieces: Vec<(u64, Vec<u8>), A> = Vec::with_capacity_in(1, self.alloc.clone());
            if !bytes.is_empty() {
                pieces.push((HEADER as u64, bytes));
            }
            let run = format::sorted_run(entries)?;
            out.push(Laid { pieces, index_at, run, namespace });
        }
        Ok(out)
    }

    /// Whether an open found what the next flip must write.
    fn pending_stamps(&self) -> bool {
        !self.checked.is_empty() || !self.dropped.is_empty() || !self.orphans.is_empty() || self.torn_stamp
    }

    pub(crate) fn what(&self) -> &'static str {
        match self.phase {
            Phase::New => "not opened",
            Phase::ReadHead(_) | Phase::MakeHead | Phase::ReadManifest { .. } | Phase::CheckLog { .. } | Phase::Orphans { .. } => "opening",
            Phase::Mapping { .. } | Phase::CheckMaps { .. } | Phase::Removing { .. } => "mapping a new generation",
            Phase::Ready => "ready",
            Phase::Listing => "compacting",
            Phase::Writing(..) | Phase::Renaming(..) | Phase::Unwriting { .. } | Phase::SyncingSegment(..) => "writing a segment",
            Phase::Flipping(_) => "flipping its head",
            Phase::Sealing { .. } => "sealing the exit tail",
            Phase::Broken => "broken",
        }
    }

    /// Lays out a segment through the one writer of the layout ([`format::seal_parts`]: its
    /// header; the payloads of `pieces`, from [`HEADER`] to `index_at`; `run`; the footer and
    /// the digest of all before it), names it by its digest, and starts its write: the file
    /// made at its length, then each piece written at its offset, in parallel. A segment of the
    /// same bytes already here is not written again (a published file is never rewritten).
    fn write_segment(&mut self, laid: Laid<A>, why: Why<A>, effects: &mut Vec<Effect>) {
        let Laid { pieces, index_at, run, namespace } = laid;
        debug_assert!(pieces.iter().scan(HEADER as u64, |at, (piece_at, bytes)| {
            let ok = *piece_at == *at;
            *at += bytes.len() as u64;
            Some(ok)
        }).all(|it| it), "pieces lie end to end");
        let sealed = format::seal_parts(namespace, &run, index_at, pieces.iter().map(|(_, bytes)| bytes.as_slice()));
        // The measure: every byte before the digest itself was read by it.
        self.put_path.digested_bytes += sealed.len.saturating_sub(32);
        self.start_write(sealed, pieces, run, why, effects);
    }

    /// A streamed copy's next writes ([`Plain::stream_segment`]): payload chunks while fewer
    /// than [`MERGE_CHUNKS_IN_FLIGHT`] are in flight; once the last is asked, index chunks,
    /// each built only once the one before completed (one [`INDEX_CHUNK`] alive); once the last
    /// is asked, the footer. Nothing for a commit's segment. The effects' tag indices:
    /// [`INDEX_PIECE`] and [`FOOTER_PIECE`].
    fn stream_on(&mut self, effects: &mut Vec<Effect>) {
        let tags = self.cfg.tags;
        let whole_index = self.plant() == Plant::MergeWholeIndex;
        let Phase::Writing(writing, _) = &mut self.phase else { return };
        let Some(mut stream) = writing.stream.take() else { return };
        let file = writing.file.share();
        let (mut asked, mut built) = (0u32, 0u64);
        loop {
            match stream.leg {
                Leg::Payloads if stream.flying < MERGE_CHUNKS_IN_FLIGHT => match stream.next_chunk(self) {
                    Some((at, bytes)) => {
                        effects.push(Effect::WriteAt { file: file.share(), at, bytes, tag: tag_of(tags, SEG_PIECE, stream.asked) });
                        stream.flying += 1;
                        asked += 1;
                    }
                    None => {
                        stream.leg = Leg::Index;
                        stream.merge.rewind(&self.maps);
                    }
                },
                Leg::Index if !stream.index_flying => match stream.next_index(self, whole_index) {
                    Some((at, bytes)) => {
                        built = built.max(bytes.len() as u64);
                        effects.push(Effect::WriteAt { file: file.share(), at, bytes, tag: tag_of(tags, SEG_PIECE, INDEX_PIECE + stream.index_asked) });
                        stream.index_flying = true;
                        asked += 1;
                    }
                    None => {
                        let (at, bytes) = stream.seal();
                        effects.push(Effect::WriteAt { file: file.share(), at, bytes, tag: tag_of(tags, SEG_PIECE, FOOTER_PIECE) });
                        asked += 1;
                    }
                },
                _ => break,
            }
        }
        #[cfg(test)]
        {
            self.merge_held_most = self.merge_held_most.max(built + stream.merge.cursors.len() as u64 * CURSOR_BYTES);
        }
        #[cfg(not(test))]
        let _ = built;
        let Phase::Writing(writing, _) = &mut self.phase else { unreachable!("streaming keeps the phase") };
        writing.left += asked;
        writing.stream = Some(stream);
    }

    /// A compaction's copy as a segment, STREAMED within [`merge_holds_most`]: its header from
    /// its merge's first walk ([`Plain::copy_live`]); a TEMPORARY file made at the copy's
    /// length in the store's directory, `seg-<id>` with an id of this process's nonce, its
    /// sequence and the clock (fresh for each copy: no other copy's name, and no name a
    /// generation names or retired); its header written; its payloads copied into it from the
    /// inputs' mappings chunk by chunk as the merge walks again ([`Stream::next_chunk`], at
    /// most [`MERGE_CHUNKS_IN_FLIGHT`] in flight); its index a chunk at a time as the merge
    /// walks a third time ([`Stream::next_index`], one in flight); every byte hashed as it is
    /// asked, in the file's order; then its footer, which holds the digest. The digest names the
    /// segment and the header it hashes first needs the count and the keys, so the name is
    /// known only after the last byte: once every write completed the file is RENAMED to
    /// `seg-<digest id>`, then synced, then flipped, as a commit's segment
    /// ([`Plain::named_copy`]).
    ///
    /// A crash before the rename leaves the temporary file, which no generation names: an
    /// ORPHAN, as every segment file no generation names is (the next open's listing,
    /// `Phase::Orphans`, reports it and its flip retires it in [`Plain::build`]; or the next
    /// slice's listing, [`Plain::listed_compact`], hands it to its flip, which retires it
    /// there), unlinked past [`GRACE_NS`]. A crash after the rename and before the flip leaves
    /// the renamed file, the same kind of orphan. A copy whose bytes are a named segment's, or
    /// one of its own inputs', removes its temporary file and writes nothing more.
    fn stream_segment(&mut self, copied: Copied<A>, why: Why<A>, now_ns: u64, effects: &mut Vec<Effect>) {
        let Copied { mut merge, count, index_at, lowest, highest } = copied;
        let namespace = merge.namespace;
        let header = format::header_of(namespace, count, lowest, highest, index_at);
        let len = index_at + u64::from(count) * ENTRY as u64 + FOOTER as u64;
        self.put_path.merged_bytes += len;
        self.seq += 1;
        let temp = format::id_of(MERGE_TEMP_DOMAIN, &[&self.cfg.nonce.to_le_bytes(), &self.seq.to_le_bytes(), &now_ns.to_le_bytes()]);
        if self.by_id.contains_key(&temp) || self.manifest.segments.iter().any(|it| it.id == temp) || self.manifest.retired.iter().any(|it| it.id == temp && !it.log) {
            return self.stop(StoreError::Unfinished { what: "a merge's copy, whose fresh temporary name a named or retired segment holds (one 64-bit id met twice)" });
        }
        let file = self.segment_file(temp);
        merge.rewind(&self.maps);
        let mut hasher = blaze_ids::Hasher::new(format::SEGMENT_DIGEST);
        hasher.update(&header);
        let chunk = if self.plant() == Plant::MergeWholeBatch { index_at } else { self.merge_buffer() / u64::from(MERGE_CHUNKS_IN_FLIGHT) };
        let index_entries = (self.index_chunk() / ENTRY as u64).max(1) as u32;
        let stream = Stream {
            merge,
            hasher,
            leg: Leg::Payloads,
            current: None,
            at: HEADER as u64,
            placed: HEADER as u64,
            index_at,
            count,
            indexed: 0,
            chunk: chunk.max(ALIGN),
            index_entries,
            asked: 0,
            flying: 0,
            index_asked: 0,
            index_flying: false,
            bloom: Some(BloomBuilder::new(count)),
            sealed: None,
        };
        // Its row until the stream seals it: no digest yet, so no id.
        let mut seg = Segment::of(&format::Described { namespace, count, index_at, digest: ContentId([0; 32]) }, len);
        seg.boot = self.cfg.boot;
        let mut head = Vec::with_capacity_in(HEADER, Global);
        head.extend_from_slice(&header);
        let mut pieces = Vec::with_capacity_in(1, self.alloc.clone());
        pieces.push((0, head));
        effects.push(Effect::Truncate { file: file.share(), len, tag: self.tag(SEG_TRUNCATE, 0) });
        self.phase = Phase::Writing(Box::new(Writing { seg, file, pieces, left: 0, stream: Some(stream) }), Box::new(why));
    }

    /// A streamed copy every write of which completed ([`Plain::stream_segment`]): its row
    /// made from what its footer says. Bytes one of its own inputs holds (the slice would write
    /// back what it read) or a named segment holds are here already: its temporary file is
    /// removed, nothing else written ([`Phase::Unwriting`]). Else the file is renamed to its
    /// digest's name ([`Phase::Renaming`]), then synced and flipped as a commit's segment
    /// ([`Plain::segment_done`]).
    fn named_copy(&mut self, writing: Writing<A>, why: Why<A>, effects: &mut Vec<Effect>) {
        let Writing { seg: unnamed, file: temp, stream, .. } = writing;
        let (described, bloom) = stream
            .and_then(|it| it.sealed.map(|described| (described, it.bloom)))
            .expect("a copy's writes all completed only once its footer, which seals it, did");
        let mut seg = Segment::of(&described, unnamed.file_len());
        seg.boot = self.cfg.boot;
        if let Some(builder) = bloom {
            self.record_pending_bloom(seg.digest, builder.finish(&seg.digest));
        }
        let file = self.segment_file(seg.id);
        let back = matches!(&why, Why::Compact { candidates, .. } if candidates.contains(&seg.id));
        let named = Writing { seg, file: file.share(), pieces: Vec::new_in(self.alloc.clone()), left: 0, stream: None };
        if back || self.by_id.contains_key(&seg.id) {
            effects.push(Effect::Remove { file: temp.share(), tag: self.tag(SEG_UNWRITE, 0) });
            self.phase = Phase::Unwriting { writing: Box::new(named), why: Box::new(why), temp };
            return;
        }
        // Unsynced: its bytes are made durable by its own sync next, and its name by the
        // flip's sync of the directory before the slot that names it (`DIR_SYNC`), as a
        // commit's segment's are; a durable rename would flush the drive a second time. A
        // crash before that may put it back under its temporary name: an orphan either way.
        effects.push(Effect::RenameUnsynced { from: temp, to: file, tag: self.tag(SEG_RENAME, 0) });
        self.phase = Phase::Renaming(Box::new(named), Box::new(why));
    }

    /// A segment every byte of which is written under its name: synced, then flipped; or,
    /// not durable, flipped (the planted controls hand some to the flip before their sync).
    fn segment_done(&mut self, writing: Writing<A>, why: Why<A>, effects: &mut Vec<Effect>) {
        // The planted controls: a segment handed to the flip before its sync (every
        // segment's, or a merge's alone), or never synced.
        let merge_early = self.plant() == Plant::MergeFlipsBeforeSync && matches!(why, Why::Compact { .. });
        let planted = self.plant() == Plant::SkipDataSync || self.plant() == Plant::FlipBeforeSync || merge_early;
        let writing = match Synced::not_durable(writing, self.cfg.durable) {
            Ok(synced) => synced,
            Err(writing) if planted => {
                if self.plant() != Plant::SkipDataSync {
                    self.late_sync.push(writing.file.share());
                }
                Synced::for_a_plant(writing)
            }
            Err(writing) => {
                self.put_path.syncs += 1;
                effects.push(Effect::Sync { file: writing.file.share(), tag: self.tag(SEG_SYNC, 0) });
                self.phase = Phase::SyncingSegment(Box::new(writing), Box::new(why));
                return;
            }
        };
        self.segment_written(writing, why, effects);
    }

    /// The write of a segment `sealed` describes (a commit's, or a carry's): `pieces` (its
    /// payloads), then `run` and the footer.
    fn start_write(&mut self, sealed: format::Sealed, mut pieces: Vec<(u64, Vec<u8>), A>, run: Vec<u8>, why: Why<A>, effects: &mut Vec<Effect>) {
        let index_at = sealed.described.index_at;
        let (count, len) = (sealed.described.count, sealed.len);
        let mut seg = Segment::of(&sealed.described, len);
        seg.boot = self.cfg.boot;
        let file = self.segment_file(seg.id);
        if let Why::Compact { candidates, stats } = &why
            && candidates.contains(&seg.id)
        {
            // The slice would write back what it read: nothing to do.
            self.fold_asked = false;
            self.sliced = Some(Sliced { segments: 0, ..*stats });
            self.phase = Phase::Ready;
            return;
        }
        let bloom_record = BloomBuilder::from_run(&run, &sealed.described.digest);
        self.record_pending_bloom(sealed.described.digest, bloom_record);
        if self.by_id.contains_key(&seg.id) {
            // These bytes are here already, under this name: flip without writing them. They
            // are a named segment's, proved as its row is (synced before the flip that named
            // them, when durable): nothing of them is written here, so no sync is owed.
            self.segment_written(Synced::already_named(Writing { seg, file, pieces: Vec::new_in(self.alloc.clone()), left: 0, stream: None }), why, effects);
            return;
        }
        self.put_path.segments_written += 1;
        let mut header = Vec::with_capacity_in(HEADER, Global);
        header.extend_from_slice(&sealed.header);
        let mut footer = Vec::with_capacity_in(FOOTER, Global);
        footer.extend_from_slice(&sealed.footer);
        pieces.insert(0, (0, header));
        let footer_at = index_at + u64::from(count) * ENTRY as u64;
        pieces.push((index_at, run));
        pieces.push((footer_at, footer));
        effects.push(Effect::Truncate { file: file.share(), len, tag: self.tag(SEG_TRUNCATE, 0) });
        self.phase = Phase::Writing(Box::new(Writing { seg, file, pieces, left: 0, stream: None }), Box::new(why));
    }

    /// Names segments fetched from a source (their files already written here, unchanged,
    /// each checked by [`super::format::check_segment`] and its statement verified by the
    /// driver) in a new generation: here, published.
    pub fn adopt(&mut self, fetched: Vec<Segment, A>, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        self.start_flip(Change::Adopt { segs: fetched }, effects);
        Ok(())
    }

    /// Marks segments as published (pushed to a shared bucket): compaction never rewrites them.
    pub fn publish(&mut self, ids: Vec<u64, A>, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        self.start_flip(Change::Publish { ids }, effects);
        Ok(())
    }

    /// The segment `id` of this generation, and its bytes when it is mapped here: what a push
    /// uploads unchanged.
    pub fn segment(&self, id: u64) -> Option<(Segment, Option<&[u8]>)> {
        let seg = *self.manifest.segments.iter().find(|it| it.id == id)?;
        Some((seg, self.by_id.get(&id).map(|at| self.maps[*at].bytes())))
    }

    // ---- compaction ----

    /// Starts one compaction slice: the directory looked at for files no generation names,
    /// then the segments `Plain::plan` picks (a pruning run of at most [`SLICE_BYTES`], or a
    /// namespace's tiered merge) copied (what is live of them) into one new segment, with no
    /// lock, and a flip that names it in their place. Returns whether
    /// it started; the outcome is [`Plain::take_sliced`]'s once the store is ready again.
    pub fn compact(&mut self, effects: &mut Vec<Effect>) -> bool {
        if !self.ready() {
            return false;
        }
        assert!(self.eager, "Plain::compact copies from a whole store: Plain::map_all first");
        self.phase = Phase::Listing;
        effects.push(Effect::List { dir: self.cfg.dir.share().into(), tag: self.tag(LIST, 0) });
        true
    }

    /// Cleanup that no command waits for: one slice listed, planned and its copy written with
    /// no lock, as [`Plain::compact`]'s, then parked for this process's next [`Plain::commit`],
    /// whose flip (made anyway, for the command's own records and roots) names it. The lock
    /// is never taken for cleanup, so no other process's flip waits on it; one that finds
    /// cleanup running elsewhere reads its own generation and goes on (a reader takes no
    /// lock). The commit's flip applies the slice only while every segment it replaces is
    /// named as it planned; else the commit lands alone and the copy, named by no generation,
    /// is removed. Whether it started (not when one is parked already); the store is ready
    /// again with the slice parked ([`Plain::slice_parked`]) or with nothing to do (its
    /// [`Plain::take_sliced`] then), and the commit carries a parked one to
    /// [`Plain::take_sliced`].
    pub fn fold_slice(&mut self, effects: &mut Vec<Effect>) -> Result<bool, StoreError> {
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        if self.folding.is_some() {
            return Ok(false);
        }
        self.compact(effects);
        self.fold_asked = self.plant() != Plant::SliceFlipsAlone;
        Ok(true)
    }

    /// A slice [`Plain::fold_slice`] wrote waits for the next commit's flip.
    pub fn slice_parked(&self) -> bool {
        self.folding.is_some()
    }

    /// Which segments a slice replaces, and whether it prunes: over the budget high watermark,
    /// a pruning run from the oldest prunable row (bounded by [`SLICE_BYTES`]); else, or when
    /// nothing is old enough to prune, the tiered merge ([`Tiers`]) of the first namespace that
    /// needs one.
    fn plan(&self) -> (Vec<Segment, A>, bool) {
        let current = self.manifest.generation;
        // Merging never reads a published segment, nor one not here.
        let merge = || (self.plan_tiered(|it| it.local && !it.published), false);
        if !self.over_budget() {
            return merge();
        }
        // Pruning reads published segments too (it lets go of this machine's copy, rewriting
        // nothing). One namespace a slice: no segment mixes two.
        let eligible = |it: &&Segment| it.local && it.generation + WINDOW <= current;
        let Some(namespace) = self.manifest.segments.iter().find(eligible).map(|it| it.namespace) else {
            return merge();
        };
        let mut out = self.alloc_vec();
        let mut bytes = 0u64;
        // A CONTIGUOUS run of the namespace's rows held here: a pruned copy keeps a row's
        // records whole, so a row the slice skipped stays where it was against the copy.
        for seg in self.manifest.segments.iter().filter(|it| it.local && it.namespace == namespace) {
            if !eligible(&seg) {
                if out.is_empty() {
                    continue;
                }
                break;
            }
            if !out.is_empty() && bytes + seg.file_len() > SLICE_BYTES {
                break;
            }
            bytes += seg.file_len();
            out.push(*seg);
        }
        (out, true)
    }

    /// Builds the root reachability map and (when `need_sizes` is true) per-root marginal live
    /// byte accounting for `now_ns`.
    ///
    /// Pass A walks unexpired roots (and their one-level index records) in keep-priority order,
    /// recording each key's first claiming root slot in an O(1) hash map (`owner`).
    /// Pass B (only when `need_sizes` is true) sizes all uncounted keys in one sequential scan of
    /// mapped segment runs in `(reads, self.view)` order (or direct probes when very few keys are
    /// live), avoiding O(live_keys * segments) per-key binary searches.
    fn compute_reach(&self, now_ns: u64, need_sizes: bool) -> Result<ReachCache<A>, StoreError> {
        let roots_hash = hash_roots(&self.manifest.roots);
        let leases_hash = hash_leases(&self.manifest.leases);
        let mut valid_from_ns = 0u64;
        let mut valid_until_ns = u64::MAX;
        let mut min_other_lease_since: Option<u64> = None;
        let lease_never_expires = self.plant() == Plant::LeaseNeverExpires;
        for lease in &self.manifest.leases {
            if lease.holder == self.cfg.nonce {
                continue;
            }
            if !lease_never_expires {
                if lease.expires_ns <= now_ns {
                    valid_from_ns = valid_from_ns.max(lease.expires_ns);
                    continue;
                }
                valid_until_ns = valid_until_ns.min(lease.expires_ns);
            }
            min_other_lease_since = Some(match min_other_lease_since {
                None => lease.since,
                Some(cur) => cur.min(lease.since),
            });
        }
        let mut active_toolchain: Option<(usize, u64, u64)> = None;
        for (idx, root) in self.manifest.roots.iter().enumerate() {
            if root.expires_ns <= now_ns {
                valid_from_ns = valid_from_ns.max(root.expires_ns);
                continue;
            }
            valid_until_ns = valid_until_ns.min(root.expires_ns);
            if !root.name.starts_with("tool.") {
                continue;
            }
            let better = match active_toolchain {
                None => true,
                Some((_, best_exp, best_gen)) => (root.expires_ns, root.generation) > (best_exp, best_gen),
            };
            if better {
                active_toolchain = Some((idx, root.expires_ns, root.generation));
            }
        }
        let active_toolchain_idx = active_toolchain.map(|(idx, _, _)| idx);
        let mut order: blaze_own::Vec<(usize, u8, u64), A> = self.own_vec();
        let skip_first = self.plant() == Plant::ReachSkipsRoot;
        for (idx, root) in self.manifest.roots.iter().enumerate() {
            if root.expires_ns <= now_ns || (idx == 0 && skip_first) {
                continue;
            }
            let touched = self.plant() != Plant::LruEvictsTouched && self.is_touched_root(&root.name);
            let leased = min_other_lease_since.is_some_and(|since| root.generation >= since);
            let tier = if root.expires_ns == NEVER || touched || leased {
                KEEP_TIER_PROTECTED
            } else if active_toolchain_idx == Some(idx) {
                if self.plant() == Plant::LruEvictsToolchainFirst {
                    KEEP_TIER_REGULAR
                } else {
                    KEEP_TIER_ACTIVE_TOOLCHAIN
                }
            } else if root.name.starts_with("tool.") || root.name.starts_with("pubf.") {
                KEEP_TIER_OLD_TOOLCHAIN
            } else {
                KEEP_TIER_REGULAR
            };
            order.push((idx, tier, 0));
        }
        let reverse_lru = self.plant() == Plant::LruEvictsNewest;
        order.sort_unstable_by(|a, b| {
            let ra = self.manifest.roots.get(a.0);
            let rb = self.manifest.roots.get(b.0);
            a.1.cmp(&b.1).then_with(|| match (ra, rb) {
                (Some(ra), Some(rb)) if reverse_lru => {
                    (ra.expires_ns, ra.generation, ra.name.as_str()).cmp(&(rb.expires_ns, rb.generation, rb.name.as_str()))
                }
                (Some(ra), Some(rb)) => (core::cmp::Reverse(ra.expires_ns), core::cmp::Reverse(ra.generation), ra.name.as_str()).cmp(&(
                    core::cmp::Reverse(rb.expires_ns),
                    core::cmp::Reverse(rb.generation),
                    rb.name.as_str(),
                )),
                _ => a.0.cmp(&b.0),
            })
        });
        let entry_overhead = u64::try_from(ENTRY).unwrap_or(0);
        let mut owner: Map<[u8; 32], u32, BuildKeyHasher, Kept> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: BLOOM_ALLOC, capacity: None });
        let mut followed: Map<[u8; 32], (), BuildKeyHasher, &A> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &self.alloc, capacity: None });
        let mut total_live = 0u64;
        let mut uncounted = 0usize;
        for (pos, slot) in order.iter_mut().enumerate() {
            let slot_idx = u32::try_from(pos).unwrap_or(u32::MAX & !SIZED_BIT);
            let Some(root) = self.manifest.roots.get(slot.0) else { continue };
            let Some((at, entry)) = self.find_in(&[Namespace::LOCAL], &Key(root.key)) else {
                return Err(StoreError::RootGone { generation: self.manifest.generation, key: root.key });
            };
            let mut marginal = 0u64;
            if let HashEntry::Vacant(vacant) = owner.entry(root.key.0) {
                vacant.insert(slot_idx | SIZED_BIT);
                marginal = marginal.saturating_add(aligned(u64::from(entry.len)).saturating_add(entry_overhead));
            }
            let Some(map) = self.maps.get(at) else { continue };
            let payload = map.payload(&entry);
            match reach_of(entry.tag) {
                TagReach::Indexes if self.plant() != Plant::IndexNotFollowed => {
                    for chunk in payload.chunks_exact(32) {
                        let Ok(raw) = <[u8; 32]>::try_from(chunk) else { continue };
                        if followed.insert(raw, ()).is_some() {
                            continue;
                        }
                        let index = ContentId(raw);
                        let Some((idx_at, idx_entry)) = self.find_in(&[Namespace::LOCAL], &Key(index)) else {
                            return Err(StoreError::IndexGone { generation: self.manifest.generation, root: root.key, index });
                        };
                        if reach_of(idx_entry.tag) != TagReach::IndexKeys {
                            return Err(StoreError::NotAnIndex { root: root.key, key: index, tag: idx_entry.tag });
                        }
                        if let HashEntry::Vacant(vacant) = owner.entry(raw) {
                            vacant.insert(slot_idx | SIZED_BIT);
                            marginal = marginal.saturating_add(aligned(u64::from(idx_entry.len)).saturating_add(entry_overhead));
                        }
                        let Some(idx_map) = self.maps.get(idx_at) else { continue };
                        for key_chunk in idx_map.payload(&idx_entry).chunks_exact(32) {
                            let Ok(key_raw) = <[u8; 32]>::try_from(key_chunk) else { continue };
                            if let HashEntry::Vacant(vacant) = owner.entry(key_raw) {
                                vacant.insert(slot_idx);
                                uncounted = uncounted.saturating_add(1);
                            }
                        }
                    }
                }
                TagReach::RootKeys | TagReach::Indexes => {
                    for chunk in payload.chunks_exact(32) {
                        let Ok(key_raw) = <[u8; 32]>::try_from(chunk) else { continue };
                        if let HashEntry::Vacant(vacant) = owner.entry(key_raw) {
                            vacant.insert(slot_idx);
                            uncounted = uncounted.saturating_add(1);
                        }
                    }
                }
                TagReach::Opaque | TagReach::IndexKeys | TagReach::Target => {}
            }
            slot.2 = marginal;
            total_live = total_live.saturating_add(marginal);
        }
        if need_sizes && uncounted > 0 {
            let mut total_entries = 0u64;
            for &row in &self.view {
                if let Some(seg) = self.manifest.segments.get(row) {
                    total_entries = total_entries.saturating_add(u64::from(seg.count));
                }
            }
            let reads: &[Namespace] = &self.cfg.reads;
            let every = self.plant() == Plant::IgnoresNamespace;
            let probe_cost = u64::try_from(uncounted)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::try_from(self.view.len()).unwrap_or(u64::MAX));
            if probe_cost <= total_entries {
                for (key_raw, val) in owner.iter_mut() {
                    if (*val & SIZED_BIT) != 0 {
                        continue;
                    }
                    let slot_idx = *val;
                    *val = slot_idx | SIZED_BIT;
                    if let Some((_, rec_entry)) = self.find_in(reads, &Key(ContentId(*key_raw))) {
                        let bytes = aligned(u64::from(rec_entry.len)).saturating_add(entry_overhead);
                        if let Some(slot) = order.get_mut(usize::try_from(slot_idx).unwrap_or(usize::MAX)) {
                            slot.2 = slot.2.saturating_add(bytes);
                        }
                        total_live = total_live.saturating_add(bytes);
                    }
                }
            } else {
                for space in reads {
                    if uncounted == 0 {
                        break;
                    }
                    for &row in &self.view {
                        if uncounted == 0 {
                            break;
                        }
                        let Some(seg) = self.manifest.segments.get(row) else { continue };
                        if !every && seg.namespace != *space {
                            continue;
                        }
                        let Some(at) = self.mapped_row(row) else { continue };
                        let Some(mapped) = self.maps.get(at) else { continue };
                        let run = mapped.run();
                        let mut off = 0usize;
                        while let Some(chunk) = run.get(off..off.saturating_add(ENTRY)) {
                            off = off.saturating_add(ENTRY);
                            let Some(key_slice) = chunk.get(..32) else { continue };
                            let Ok(key_raw) = <[u8; 32]>::try_from(key_slice) else { continue };
                            if let Some(val) = owner.get_mut(&key_raw)
                                && (*val & SIZED_BIT) == 0
                            {
                                let slot_idx = *val;
                                *val = slot_idx | SIZED_BIT;
                                let len = chunk
                                    .get(40..44)
                                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                                    .map_or(0, u32::from_le_bytes);
                                let bytes = aligned(u64::from(len)).saturating_add(entry_overhead);
                                if let Some(slot) = order.get_mut(usize::try_from(slot_idx).unwrap_or(usize::MAX)) {
                                    slot.2 = slot.2.saturating_add(bytes);
                                }
                                total_live = total_live.saturating_add(bytes);
                                uncounted -= 1;
                                if uncounted == 0 {
                                    break;
                                }
                            }
                        }
                    }
                    if every {
                        break;
                    }
                }
            }
        }
        let owner = blaze_own::Arc::new_in(owner, BLOOM_ALLOC);
        Ok(ReachCache {
            epoch: self.reach_epoch,
            roots_hash,
            leases_hash,
            valid_from_ns,
            valid_until_ns,
            sized: need_sizes,
            order,
            total_live,
            owner,
        })
    }

    /// Ensures `self.reach_cache` is valid for the current `(manifest.roots, manifest.leases,
    /// now_ns, need_sizes)`, reusing the cached reachability map across compaction slices when
    /// roots and leases have not changed.
    fn ensure_reach(&mut self, now_ns: u64, need_sizes: bool) -> Result<(), StoreError> {
        let hit = self.reach_cache.as_ref().is_some_and(|cache| {
            cache.epoch == self.reach_epoch
                && (!need_sizes || cache.sized)
                && now_ns >= cache.valid_from_ns
                && now_ns < cache.valid_until_ns
                && cache.roots_hash == hash_roots(&self.manifest.roots)
                && cache.leases_hash == hash_leases(&self.manifest.leases)
        });
        if !hit {
            let built = self.compute_reach(now_ns, need_sizes)?;
            self.reach_cache = Some(built);
        }
        Ok(())
    }

    /// Derives `(total_live, Option<EvictPlan>, kept_count)` from a sized [`ReachCache`] in
    /// O(roots). Surviving roots are `cache.order[..kept_count]`.
    fn evict_from_reach(&self, cache: &ReachCache<A>) -> (u64, Option<EvictPlan<A>>, u32) {
        let total_live = cache.total_live;
        let kept_all = u32::try_from(cache.order.len()).unwrap_or(u32::MAX);
        let framing = u64::try_from(HEADER + FOOTER).unwrap_or(0);
        if !self.over_budget() || total_live.saturating_add(framing) <= self.high_watermark_bytes() {
            return (total_live, None, kept_all);
        }
        let target = self.low_watermark_bytes().saturating_sub(framing);
        let mut remaining = total_live;
        let mut plan = EvictPlan::new(self.own_vec(), self.own_vec());
        let mut evicted = 0u32;
        for &(idx, tier, marginal) in cache.order.iter().rev() {
            if tier == KEEP_TIER_PROTECTED || remaining <= target {
                break;
            }
            let Some(root) = self.manifest.roots.get(idx) else { continue };
            plan.push(&root.name, root.generation, root.expires_ns);
            remaining = remaining.saturating_sub(marginal);
            evicted = evicted.saturating_add(1);
        }
        let kept_count = kept_all.saturating_sub(evicted);
        let out = (!plan.is_empty()).then_some(plan);
        (total_live, out, kept_count)
    }

    /// Read-only GC plan at `now_ns` (requires `map_all` first, writes nothing).
    pub fn plan_gc(&self, now_ns: u64) -> Result<GcPlan, StoreError> {
        let total_bytes = self.manifest.bytes();
        let cap_bytes = self.cfg.cap_bytes;
        let high_watermark_bytes = self.high_watermark_bytes();
        let low_watermark_bytes = self.low_watermark_bytes();
        let (live_bytes, evict) = if self.manifest.roots.is_empty() {
            (0, None)
        } else {
            let reach = self.compute_reach(now_ns, true)?;
            let (live, plan, _) = self.evict_from_reach(&reach);
            (live, plan)
        };
        let expired_roots = u32::try_from(self.manifest.roots.iter().filter(|it| it.expires_ns != NEVER && it.expires_ns <= now_ns).count()).unwrap_or(u32::MAX);
        let lru_evict_roots = evict.as_ref().map_or(0, |it| u32::try_from(it.len()).unwrap_or(u32::MAX));
        let retired_ready = u32::try_from(self.manifest.retired.iter().filter(|it| it.lost.is_none() && it.since_ns.saturating_add(GRACE_NS) <= now_ns).count()).unwrap_or(u32::MAX);
        let surviving_live = if evict.is_some() {
            live_bytes.min(low_watermark_bytes)
        } else {
            live_bytes
        };
        let reclaimable_bytes = total_bytes.saturating_sub(surviving_live);
        Ok(GcPlan {
            total_bytes,
            cap_bytes,
            high_watermark_bytes,
            low_watermark_bytes,
            live_bytes,
            reclaimable_bytes,
            total_roots: u32::try_from(self.manifest.roots.len()).unwrap_or(u32::MAX),
            expired_roots,
            lru_evict_roots,
            segments: u32::try_from(self.manifest.segments.len()).unwrap_or(u32::MAX),
            retired_ready,
        })
    }

    /// The tiered merge, over the rows `mergeable` admits, of the namespace whose merge starts
    /// at the oldest row (so a namespace left over by one slice is the next slice's before any
    /// namespace that went over since); none when every namespace keeps the invariant.
    /// (Whether another slice has work is planned again after the flip: [`Sliced::more`].) A
    /// merge's rows need not be contiguous: its copy takes the NEWEST one's place
    /// ([`Plain::build`]) and holds a pointer only at its newest value (`copy_live`), so no row
    /// the merge skipped moves past a pointer's newest value.
    fn plan_tiered(&self, mergeable: impl Fn(&Segment) -> bool) -> Vec<Segment, A> {
        let tiers = self.tiers();
        let segs = &self.manifest.segments;
        let mut out = Vec::new_in(self.alloc.clone());
        let mut oldest = usize::MAX;
        let mut looked: Vec<Namespace, &A> = Vec::new_in(&self.alloc);
        for first in segs {
            if looked.contains(&first.namespace) {
                continue;
            }
            looked.push(first.namespace);
            let namespace = first.namespace;
            let mut rows: Vec<TierRow, &A> = Vec::new_in(&self.alloc);
            let mut at: Vec<usize, &A> = Vec::new_in(&self.alloc);
            for (i, seg) in segs.iter().enumerate().filter(|(_, it)| it.namespace == namespace) {
                rows.push(TierRow { len: seg.file_len(), mergeable: mergeable(seg) });
                at.push(i);
            }
            let merge = tiers.merge(&rows, &self.alloc);
            let Some(&start) = merge.rows.first() else { continue };
            if at[start] < oldest {
                oldest = at[start];
                out.clear();
                out.extend(merge.rows.iter().map(|&row| segs[at[row]]));
            }
        }
        out
    }

    /// Which segments a [`Liveness::Reached`] slice replaces: the tiered merge ([`Tiers`]) of
    /// the first namespace that breaks the invariant, FIRST, so the invariant holds after every
    /// cleanup in sweep mode too (a sparse row always present would else take every first
    /// slice, and only merges go on: [`Plain::cleanup_goes_on`]); else a contiguous run of one
    /// namespace's rows, from the first found under half live (over the cap: holding any dead
    /// record) among at most [`SWEEP_BYTES`] of rows looked at from a place that moves with the
    /// generation, up to [`SLICE_BYTES`]. A row a live lease keeps, a published row and one
    /// not here are never in a run (a run stops at one) nor in a merge. And whether it is the
    /// tiered merge ([`Sliced::merged`]).
    fn plan_reached(&mut self, now_ns: u64, over_or_evict: bool, kept_count: Option<u32>) -> Result<(Vec<Segment, A>, bool), StoreError> {
        let merge = self.plan_tiered(|it| self.sweepable(it, now_ns));
        if !merge.is_empty() && self.plant() != Plant::SweepBeforeMerge {
            return Ok((merge, true));
        }
        self.ensure_reach(now_ns, false)?;
        let (owner, kept) = match self.reach_cache.as_ref() {
            Some(cache) => (cache.owner.share(), kept_count.unwrap_or_else(|| u32::try_from(cache.order.len()).unwrap_or(u32::MAX))),
            None => return Ok((merge, true)),
        };
        let segs = &self.manifest.segments;
        let mut sparse = None;
        if !segs.is_empty() {
            let start = usize::try_from(self.manifest.generation % u64::try_from(segs.len()).unwrap_or(1)).unwrap_or(0);
            let mut looked = 0u64;
            for k in 0..segs.len() {
                let i = (start + k) % segs.len();
                let Some(seg) = segs.get(i) else { continue };
                if !self.sweepable(seg, now_ns) {
                    continue;
                }
                if looked >= SWEEP_BYTES {
                    break;
                }
                looked = looked.saturating_add(seg.file_len());
                let (live, all) = self.live_of(seg, &owner, kept);
                if live.saturating_mul(2) < seg.file_len() || (over_or_evict && live < all) {
                    sparse = Some(i);
                    break;
                }
            }
        }
        // No sparse row: the tiered merge (empty here unless planted to sweep first).
        let Some(from) = sparse else { return Ok((merge, true)) };
        let Some(first) = segs.get(from) else { return Ok((merge, true)) };
        let namespace = first.namespace;
        let mut out = self.alloc_vec();
        let mut bytes = 0u64;
        // A contiguous run from the sparse row: its copy takes the newest one's place, past no
        // row it skipped.
        if let Some(tail) = segs.get(from..) {
            for seg in tail.iter().filter(|it| it.local && it.namespace == namespace) {
                if !self.sweepable(seg, now_ns) || (!out.is_empty() && bytes.saturating_add(seg.file_len()) > SLICE_BYTES) {
                    break;
                }
                bytes = bytes.saturating_add(seg.file_len());
                out.push(*seg);
            }
        }
        Ok((out, false))
    }

    /// A [`Liveness::Reached`] slice may take `seg`, in a sweep's run or a merge: held here,
    /// not published, mapped, and kept whole by no live lease.
    fn sweepable(&self, seg: &Segment, now_ns: u64) -> bool {
        seg.local && !seg.published && self.by_id.contains_key(&seg.id) && !self.leased(seg, now_ns)
    }

    /// A live lease keeps `seg` whole: it is of the lease's generation or newer.
    fn leased(&self, seg: &Segment, now_ns: u64) -> bool {
        self.manifest.leases.iter().any(|it| seg.generation >= it.since && (it.expires_ns > now_ns || self.plant() == Plant::LeaseNeverExpires))
    }

    /// O(total unique reached keys) sets of all keys reached by unexpired roots in this view
    /// (`keys`) and all index records followed by unexpired indexed roots (`followed`), built in a
    /// single pass with O(1) index deduplication so [`Plain::reused`] probes in O(1) per key
    /// instead of scanning every root/index linearly.
    fn live_key_sets(&self, now_ns: u64) -> Result<(Map<[u8; 32], (), BuildKeyHasher, &A>, Map<[u8; 32], (), BuildKeyHasher, &A>), StoreError> {
        let mut keys: Map<[u8; 32], (), BuildKeyHasher, &A> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &self.alloc, capacity: None });
        let mut followed: Map<[u8; 32], (), BuildKeyHasher, &A> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &self.alloc, capacity: None });
        for (i, root) in self.manifest.roots.iter().enumerate() {
            if root.expires_ns <= now_ns || (i == 0 && self.plant() == Plant::ReachSkipsRoot) {
                continue;
            }
            let Some((at, entry)) = self.find_in(&[Namespace::LOCAL], &Key(root.key)) else {
                return Err(StoreError::RootGone { generation: self.manifest.generation, key: root.key });
            };
            keys.insert(root.key.0, ());
            let Some(map) = self.maps.get(at) else { continue };
            let payload = map.payload(&entry);
            for chunk in payload.chunks_exact(32) {
                let Ok(raw) = <[u8; 32]>::try_from(chunk) else { continue };
                keys.insert(raw, ());
            }
            if reach_of(entry.tag) == TagReach::Indexes && self.plant() != Plant::IndexNotFollowed {
                for chunk in payload.chunks_exact(32) {
                    let Ok(raw) = <[u8; 32]>::try_from(chunk) else { continue };
                    if followed.insert(raw, ()).is_some() {
                        continue;
                    }
                    let index = ContentId(raw);
                    let Some((idx_at, idx_entry)) = self.find_in(&[Namespace::LOCAL], &Key(index)) else {
                        return Err(StoreError::IndexGone { generation: self.manifest.generation, root: root.key, index });
                    };
                    if reach_of(idx_entry.tag) != TagReach::IndexKeys {
                        return Err(StoreError::NotAnIndex { root: root.key, key: index, tag: idx_entry.tag });
                    }
                    let Some(idx_map) = self.maps.get(idx_at) else { continue };
                    for key_chunk in idx_map.payload(&idx_entry).chunks_exact(32) {
                        let Ok(key_raw) = <[u8; 32]>::try_from(key_chunk) else { continue };
                        keys.insert(key_raw, ());
                    }
                }
            }
        }
        Ok((keys, followed))
    }

    /// Whether this generation names a segment an earlier boot wrote and never sealed: the
    /// tail a power cut may have torn, known from the rows alone, no byte read. Only then can
    /// an open have lost a segment a root reaches (this boot's writes are in the page cache;
    /// a sealed row's bytes reached the media before the flip that sealed it), so only then
    /// does [`super::Driven`]'s open check the tail and walk the roots (D200).
    pub(crate) fn earlier_tail(&self) -> bool {
        self.manifest.segments.iter().any(|it| it.local && !it.sealed && it.boot != self.cfg.boot)
    }

    /// Whether this generation names any root: with none, no root can have a hole, and an
    /// open keeps its lazy maps whatever its tail ([`super::Driven`]'s open, D200).
    pub(crate) fn names_a_root(&self) -> bool {
        !self.manifest.roots.is_empty()
    }

    /// `keys` more root names looked for when a root was set ([`PutPath::root_probes`]).
    pub(crate) fn count_root_probes(&mut self, keys: u64) {
        self.put_path.root_probes += keys;
    }

    /// [`Plant::KeepsHoledRoot`]: the open's unrooting skipped (a control).
    pub(crate) fn keeps_holed_root(&self) -> bool {
        self.plant() == Plant::KeepsHoledRoot
    }

    /// The roots this generation names (every one, expired too) that reach a record no mapped
    /// segment holds: the root's own record, an index an indexed root names, or a key a root
    /// or an index names (looked for in every namespace this store reads). Asked of a whole
    /// store, after an open that lost a segment: each is dropped by one flip before any reader
    /// sees it (D200), so a reader finds a root whole or no root, never a root with a hole.
    /// The walk: one find for each root, each index and each key they name (index pages
    /// probed; no payload read but the roots' and the indexes' own); nothing when the store
    /// lost nothing (the caller asks only then).
    pub(crate) fn holed_roots(&self) -> Vec<String, A> {
        let reads: &[Namespace] = &self.cfg.reads;
        let held = |key: &[u8]| self.find_in(reads, &Key(ContentId(key.try_into().expect("32 bytes")))).is_some();
        let mut out = Vec::new_in(self.alloc.clone());
        for root in self.manifest.roots.iter() {
            let whole = match self.find_in(&[Namespace::LOCAL], &Key(root.key)) {
                None => false,
                Some((at, entry)) => {
                    let named = self.maps[at].payload(&entry);
                    match reach_of(entry.tag) {
                        TagReach::RootKeys => named.chunks_exact(32).all(held),
                        TagReach::Indexes => named.chunks_exact(32).all(|index| match self.find_in(&[Namespace::LOCAL], &Key(ContentId(index.try_into().expect("32 bytes")))) {
                            Some((at, entry)) if reach_of(entry.tag) == TagReach::IndexKeys => self.maps[at].payload(&entry).chunks_exact(32).all(held),
                            _ => false,
                        }),
                        // A root names a root's or an indexed root's record (`root_for`,
                        // `root_through`): any other is its record alone, held.
                        TagReach::Opaque | TagReach::IndexKeys | TagReach::Target => true,
                    }
                }
            };
            if !whole {
                out.push(root.name.clone());
            }
        }
        out
    }

    /// `(live, all)`: the bytes `seg`'s records take (each payload aligned, and its entry),
    /// those reached from `roots` (a pointer's only in the row of its newest value) and all.
    /// The committed segment of each record a root in `edits` names (the root's own record
    /// too) that this view holds and no unexpired root of this view reaches: a slice planned
    /// from a view like this one may have judged it dead, so the flip marks the segment
    /// touched ([`Segment::touched`]) and a slice that replaces it aborts. Sorted, each once.
    fn reused(&self, edits: &[RootEdit], now_ns: u64) -> Result<Vec<(u64, ContentId), A>, StoreError> {
        let mut out = Vec::new_in(self.alloc.clone());
        let sets: Vec<Key, A> = {
            let mut it = Vec::new_in(self.alloc.clone());
            it.extend(edits.iter().filter_map(|it| match it {
                RootEdit::Set { key, renew_only: false, .. } => Some(*key),
                _ => None,
            }));
            it
        };
        if sets.is_empty() {
            return Ok(out);
        }
        let mut live = None;
        let mut checked_indexes: Map<[u8; 32], (), BuildKeyHasher, &A> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &self.alloc, capacity: None });
        macro_rules! look_key {
            ($key:expr) => {{
                let key: &ContentId = $key;
                let skip = live.as_ref().is_some_and(|(keys, _): &(Map<[u8; 32], (), BuildKeyHasher, &A>, Map<[u8; 32], (), BuildKeyHasher, &A>)| keys.contains_key(&key.0));
                if !skip && let Some((at, _)) = self.find_in(&[Namespace::LOCAL], &Key(*key)) {
                    let already = if live.is_none() {
                        let loaded = self.live_key_sets(now_ns)?;
                        let hit = loaded.0.contains_key(&key.0);
                        live = Some(loaded);
                        hit
                    } else {
                        false
                    };
                    if !already && let Some(map) = self.maps.get(at) {
                        out.push((map.seg().id, *key));
                    }
                }
            }};
        }
        for root in &sets {
            look_key!(&root.0);
            let Some(got) = self.get_in(&[Namespace::LOCAL], root) else {
                return Err(StoreError::RootGone { generation: self.manifest.generation, key: root.0 });
            };
            for key in got.bytes.chunks_exact(32) {
                let Ok(bytes) = key.try_into() else { continue };
                look_key!(&ContentId(bytes));
            }
            if reach_of(got.tag) != TagReach::Indexes || self.plant() == Plant::IndexNotFollowed {
                continue;
            }
            // An indexed root: what each index holds is named too, one level. An index this
            // view follows already (`live.1`, built by `live_key_sets`) keeps what it holds here;
            // only another's keys are looked at, each unique index at most once. (A plain root
            // naming an index keeps its record, not what it holds: not followed, so looked at.)
            for index in got.bytes.chunks_exact(32) {
                let Ok(bytes) = index.try_into() else { continue };
                if live.as_ref().is_some_and(|(_, followed)| followed.contains_key(&bytes)) || checked_indexes.insert(bytes, ()).is_some() {
                    continue;
                }
                let index = ContentId(bytes);
                let Some(held) = self.get_in(&[Namespace::LOCAL], &Key(index)) else {
                    return Err(StoreError::IndexGone { generation: self.manifest.generation, root: root.0, index });
                };
                if reach_of(held.tag) != TagReach::IndexKeys {
                    return Err(StoreError::NotAnIndex { root: root.0, key: index, tag: held.tag });
                }
                for key in held.bytes.chunks_exact(32) {
                    let Ok(bytes) = key.try_into() else { continue };
                    look_key!(&ContentId(bytes));
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn live_of(&self, seg: &Segment, owner: &Map<[u8; 32], u32, BuildKeyHasher, Kept>, kept_count: u32) -> (u64, u64) {
        let Some(&at) = self.by_id.get(&seg.id) else { return (seg.file_len(), seg.file_len()) };
        let Some(mapped) = self.maps.get(at) else { return (seg.file_len(), seg.file_len()) };
        let run = mapped.run();
        let (mut live, mut all) = (0u64, 0u64);
        let entry_overhead = u64::try_from(ENTRY).unwrap_or(0);
        let mut off = 0usize;
        while let Some(chunk) = run.get(off..off.saturating_add(ENTRY)) {
            off = off.saturating_add(ENTRY);
            let entry = Entry::read(chunk);
            let bytes = aligned(u64::from(entry.len)).saturating_add(entry_overhead);
            all = all.saturating_add(bytes);
            if owner.get(&entry.key.0).is_some_and(|&slot| (slot & !SIZED_BIT) < kept_count) {
                let newest = !ReservedTag::Pointer.is(entry.tag) || self.newest_mapped(seg.namespace, &Key(entry.key)).is_some_and(|(it, _)| it == at);
                if newest {
                    live = live.saturating_add(bytes);
                }
            }
        }
        (live, all)
    }

    /// Keys named by `edits` (new/updated roots) whose committed segment in `self.maps` is no
    /// longer in `m.segments`, used when a concurrent command LRU-evicted a root and swept its
    /// segment between this process's open and its commit flip.
    fn missing_from_eviction(&self, edits: &[RootEdit], m: &Manifest) -> Result<Vec<(u64, ContentId), A>, StoreError> {
        let mut present: Vec<u64, &A> = Vec::with_capacity_in(m.segments.len(), &self.alloc);
        present.extend(m.segments.iter().map(|it| it.id));
        present.sort_unstable();
        let mut seen_indexes: Map<[u8; 32], (), BuildKeyHasher, &A> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &self.alloc, capacity: None });
        let mut out = self.alloc_vec();
        for edit in edits {
            let RootEdit::Set { key: root, renew_only: false, .. } = edit else { continue };
            let mut check = |k: ContentId| {
                if self.carried_in_commit.as_ref().is_some_and(|done| done.binary_search(&k).is_ok()) {
                    return;
                }
                if let Some((at, _)) = self.find_in(&[Namespace::LOCAL], &Key(k))
                    && let Some(map) = self.maps.get(at)
                {
                    let seg_id = map.seg().id;
                    if present.binary_search(&seg_id).is_err() {
                        out.push((seg_id, k));
                    }
                }
            };
            check(root.0);
            let Some(got) = self.get_in(&[Namespace::LOCAL], root) else { continue };
            for chunk in got.bytes.chunks_exact(32) {
                let Ok(bytes) = <[u8; 32]>::try_from(chunk) else { continue };
                check(ContentId(bytes));
            }
            if reach_of(got.tag) != TagReach::Indexes || self.plant() == Plant::IndexNotFollowed {
                continue;
            }
            for chunk in got.bytes.chunks_exact(32) {
                let Ok(bytes) = <[u8; 32]>::try_from(chunk) else { continue };
                if seen_indexes.insert(bytes, ()).is_some() {
                    continue;
                }
                let index = ContentId(bytes);
                let Some(held) = self.get_in(&[Namespace::LOCAL], &Key(index)) else { continue };
                if reach_of(held.tag) != TagReach::IndexKeys {
                    continue;
                }
                for key_chunk in held.bytes.chunks_exact(32) {
                    let Ok(key_bytes) = <[u8; 32]>::try_from(key_chunk) else { continue };
                    check(ContentId(key_bytes));
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn listed_compact(&mut self, listed: Vec<(FileName, blaze_core::Meta)>, now_ns: u64, effects: &mut Vec<Effect>) {
        let current_log = self.slot.map(|it| it.log);
        let mut orphans = self.alloc_vec();
        for (name, _) in &listed {
            let leaf = name.as_str().rsplit('/').next().unwrap_or(name.as_str());
            let Some((id, log)) = parse_leaf(leaf) else { continue };
            let named = if log {
                Some(id) == current_log
            } else {
                self.by_id.contains_key(&id) || self.manifest.segments.iter().any(|it| it.id == id)
            };
            let retired = self.manifest.retired.iter().any(|it| it.id == id && it.log == log);
            if !named && !retired {
                orphans.push((id, log));
            }
        }
        let over = self.over_budget();
        let (evict, kept_count) = if over && !self.manifest.roots.is_empty() {
            if let Err(why) = self.ensure_reach(now_ns, true) {
                return self.stop(why);
            }
            match self.reach_cache.as_ref() {
                Some(cache) => {
                    let (_, plan, kept) = self.evict_from_reach(cache);
                    (plan, Some(kept))
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let over_or_evict = over || evict.is_some();
        // The candidates, whether the copy prunes, and whether it is the tiered merge.
        let planned = match self.cfg.liveness {
            Liveness::Everything => Ok(self.plan()).map(|(it, prune)| (it, prune, !prune)),
            Liveness::Reached => self.plan_reached(now_ns, over_or_evict, kept_count).map(|(it, merged)| (it, true, merged)),
        };
        let (candidates, prune, merged) = match planned {
            Ok(it) => it,
            Err(why) => return self.stop(why),
        };
        let mut stats = Sliced { orphans: orphans.len() as u32, merged, ..Sliced::default() };
        let expired = self.manifest.retired.iter().any(|it| it.lost.is_none() && it.since_ns.saturating_add(GRACE_NS) <= now_ns);
        let expired_roots = self.manifest.roots.iter().any(|it| it.expires_ns != NEVER && it.expires_ns <= now_ns);
        if candidates.is_empty() {
            if orphans.is_empty() && !expired && !expired_roots && evict.is_none() {
                self.fold_asked = false;
                self.sliced = Some(stats);
                self.phase = Phase::Ready;
                return;
            }
            let empty_ids = self.alloc_vec();
            self.slice_ready(None, empty_ids, orphans, evict, stats, effects);
            return;
        }
        let copied = self.copy_live(candidates, prune, now_ns, kept_count, &mut stats);
        let (out, ids) = match copied {
            Ok(it) => it,
            Err(why) => return self.stop(why),
        };
        if out.count == 0 {
            self.slice_ready(None, ids, orphans, evict, stats, effects);
            return;
        }
        stats.bytes_out = out.index_at + u64::from(out.count) * ENTRY as u64 + FOOTER as u64;
        self.orphans_for_compact = orphans;
        self.evict_for_compact = evict;
        self.stream_segment(out, Why::Compact { candidates: ids, stats }, now_ns, effects);
    }

    /// A slice's change is ready: parked for the next commit's flip when
    /// [`Plain::fold_slice`] asked it, else flipped on its own ([`Plain::compact`]).
    fn slice_ready(
        &mut self,
        seg: Option<(Segment, FileName)>,
        candidates: Vec<u64, A>,
        orphans: Vec<(u64, bool), A>,
        evict: Option<EvictPlan<A>>,
        stats: Sliced,
        effects: &mut Vec<Effect>,
    ) {
        if std::mem::take(&mut self.fold_asked) {
            self.folding = Some(Folded { seg, candidates, orphans, evict, stats });
            self.phase = Phase::Ready;
            return;
        }
        self.start_flip(Change::Compact { seg, candidates, orphans, evict, stats }, effects);
    }

    /// What of `candidates` a slice keeps, as its copy's k-way merge ([`Merge`]) over their
    /// mapped sorted runs, walked once here to count it (its header: the count, where its
    /// payloads end, its lowest and highest key), holding nothing of a record: no payload byte
    /// is copied and no entry kept ([`Plain::stream_segment`] walks it again for both). And the
    /// candidates it replaces (a published one only when it holds nothing live: it is let go
    /// of whole, or kept whole, never rewritten).
    fn copy_live(&mut self, candidates: Vec<Segment, A>, prune: bool, now_ns: u64, kept_count: Option<u32>, stats: &mut Sliced) -> Result<(Copied<A>, Vec<u64, A>), StoreError> {
        let keep = if !prune {
            Keep::All
        } else {
            self.ensure_reach(now_ns, false)?;
            let (owner, kept) = match self.reach_cache.as_ref() {
                Some(cache) => (cache.owner.share(), kept_count.unwrap_or_else(|| u32::try_from(cache.order.len()).unwrap_or(u32::MAX))),
                None => (blaze_own::Arc::new_in(Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: BLOOM_ALLOC, capacity: None }), BLOOM_ALLOC), 0),
            };
            match self.cfg.liveness {
                Liveness::Everything => Keep::Rooted { owner, kept_count: kept },
                Liveness::Reached => Keep::Reached { owner, kept_count: kept },
            }
        };
        let current = self.manifest.generation;
        let holds_live = |seg: &Segment| -> bool {
            if matches!(keep, Keep::All) {
                return true;
            }
            let Some(&at) = self.by_id.get(&seg.id) else { return true };
            let Some(mapped) = self.maps.get(at) else { return true };
            let run = mapped.run();
            (0..seg.count as usize).any(|i| keep.keeps(&format::key_at(run, i), false))
        };
        let mut kept_candidates = Vec::with_capacity_in(candidates.len(), &self.alloc);
        kept_candidates.extend(candidates.into_iter().filter(|seg| !seg.published || !holds_live(seg)));
        let candidates = kept_candidates;
        let namespace = candidates.first().map_or(Namespace::LOCAL, |it| it.namespace);
        let mut cursors = Vec::with_capacity_in(candidates.len(), self.alloc.clone());
        for seg in &candidates {
            stats.bytes_in += seg.file_len();
            if seg.published {
                stats.dropped += u64::from(seg.count);
                continue;
            }
            let Some(&map_idx) = self.by_id.get(&seg.id) else { continue };
            cursors.push(Cursor { map: u32::try_from(map_idx).unwrap_or(u32::MAX), next: 0, count: seg.count, young: seg.generation + WINDOW > current });
        }
        let heap = Vec::with_capacity_in(cursors.len(), self.alloc.clone());
        let mut merge = Merge { keep, namespace, cursors, heap, last: None, dropped: 0 };
        merge.rewind(&self.maps);
        let (mut count, mut index_at) = (0u32, HEADER as u64);
        let (mut lowest, mut highest) = (ContentId([0; 32]), ContentId([0; 32]));
        while let Some((_, entry)) = merge.next(self) {
            // Where the stream will place it: the payloads end to end from [`HEADER`] in key
            // order, each at a multiple of [`ALIGN`].
            let end = index_at + u64::from(entry.len);
            if end > OFFSET_MOST {
                return Err(StoreError::TooLong { tag: entry.tag, key: entry.key, len: end });
            }
            // A segment's count is a u32: the record past it would be one more than its
            // index can say, refused by name as a payload past its offsets is.
            let Some(more) = count.checked_add(1) else {
                return Err(StoreError::TooLong { tag: entry.tag, key: entry.key, len: u64::from(u32::MAX) + 1 });
            };
            if count == 0 {
                lowest = entry.key;
            }
            highest = entry.key;
            count = more;
            index_at = aligned(end);
        }
        // THE CAP, before a byte of the copy is written: its [`format::shape`] is what its row
        // will be checked against at the flip and at every open. The policy never plans past it
        // ([`Tiers`], THE CAP); a copy that would pass it is that policy's bug, so the store
        // stops here naming the namespace and the lengths, and the command that ran the slice
        // fails, before the manifest could name a row no open reads.
        if count > 0 {
            let len = index_at + u64::from(count) * ENTRY as u64 + FOOTER as u64;
            format::shape(len, index_at, count, self.tiers().segment_most()).map_err(|why| StoreError::Shape { namespace, why })?;
        }
        stats.kept += u64::from(count);
        stats.dropped += merge.dropped;
        stats.segments = candidates.len() as u32;
        let mut ids: Vec<u64, A> = Vec::with_capacity_in(candidates.len(), self.alloc.clone());
        ids.extend(candidates.iter().map(|it| it.id));
        Ok((Copied { merge, count, index_at, lowest, highest }, ids))
    }

    // ---- the flip: the one lock, held for it alone ----

    fn start_flip(&mut self, change: Change<A>, effects: &mut Vec<Effect>) {
        self.phase = Phase::Flipping(Box::new(Flip {
            change,
            waits: 0,
            waited_ns: 0,
            next: None,
            fresh: false,
            new_log: false,
            log: self.head.share(),
            manifest_bytes: Vec::new_in(Global),
            removals: Vec::new_in(self.alloc.clone()),
            refused: None,
            aborted: false,
            carry: false,
            early: 0,
            deferred: None,
            bloom_effect: None,
            bloom_written: None,
        }));
        effects.push(Effect::Lock { file: self.flip.share(), mode: LockMode::Exclusive, tag: self.tag(LOCK, 0) });
    }

    /// A built generation whose manifest would not encode (`why`: a row `format::Row::new`
    /// refuses): not flipped, nothing written, the lock let go; [`Plain::flipped`] then
    /// refuses the change and stops the store with `why`.
    fn refuse_built(&mut self, mut flip: Box<Flip<A>>, folded: Option<Folded<A>>, why: StoreError, effects: &mut Vec<Effect>) {
        self.folding = folded;
        flip.refused = Some(why);
        self.phase = Phase::Flipping(flip);
        self.unlock(effects);
    }

    /// The new generation, built on `base` (read under the lock): the change, what the open
    /// found, and what is retired past its grace.
    fn build(&mut self, base: Base, fresh: bool, now_ns: u64, effects: &mut Vec<Effect>) {
        let Phase::Flipping(mut flip) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!("a build is a flip's") };
        let generation = base.slot.map_or(1, |it| it.generation + 1);
        let mut m = base.manifest;
        m.generation = generation;
        let retire = |m: &mut Manifest, id: u64, log: bool| {
            if !m.retired.iter().any(|it| it.id == id && it.log == log) {
                m.retired.push(Retired { id, log, since_ns: now_ns, lost: None });
            }
        };
        for seg in m.segments.iter_mut() {
            if self.checked.contains(&seg.id) && self.plant() != Plant::ForgetCheck {
                // Not sealed by the check: a segment only an open's check proved is checked
                // again at the next boot, which finds a byte that rotted on the disk since
                // (`a_loss_an_open_retires_is_reported_whatever_fails_after_its_slot_lands`).
                // Only the exit tail's barrier seals (store-next.md).
                seg.boot = self.cfg.boot;
            }
        }
        for id in &self.dropped {
            m.segments.retain(|it| it.id != *id);
            retire(&mut m, *id, false);
        }
        let current_log = base.slot.map(|it| it.log);
        let orphans: Vec<(u64, bool), A> = std::mem::replace(&mut self.orphans, Vec::new_in(self.alloc.clone()));
        let adopt_orphans = |m: &mut Manifest, list: &[(u64, bool)]| {
            for &(id, log) in list {
                let named = if log { Some(id) == current_log } else { m.segments.iter().any(|it| it.id == id) };
                if !named {
                    retire(m, id, log);
                }
            }
        };
        adopt_orphans(&mut m, &orphans);
        let mut refused = None;
        let mut aborted = false;
        let mut carry = false;
        // A slice's change, on the manifest being built: whether it aborted (a segment it
        // replaces is no longer named as this process planned it, or a root it planned to
        // LRU-evict was concurrently renewed/updated or is now covered by an active lease).
        let own_nonce = self.cfg.nonce;
        let lease_never_expires = self.plant() == Plant::LeaseNeverExpires;
        let own_segments = &self.manifest.segments;
        let apply_slice = |m: &mut Manifest, seg: &mut Option<(Segment, FileName)>, candidates: &[u64], orphans: &[(u64, bool)], evict: Option<&EvictPlan<A>>, stats: &mut Sliced| -> bool {
            // As planned: named, here, of the same generation, and touched by no commit since
            // (a root that reuses a record the plan judged dead: [`Plain::reused`]).
            let planned = |id: &u64| own_segments.iter().find(|it| it.id == *id).map(|it| (it.generation, it.touched));
            // The copy's bytes may equal a named segment's (a merge of pointers keeps each
            // name's newest value alone, which can be one candidate's whole content or an
            // older row's): that file is the copy, so it is never removed or retired here.
            let copy = seg.as_ref().map(|(it, _)| it.id);
            let evict_conflict = evict.is_some_and(|plan| {
                let min_lease = m
                    .leases
                    .iter()
                    .filter(|it| it.holder != own_nonce && (it.expires_ns > now_ns || lease_never_expires))
                    .map(|it| it.since)
                    .min();
                plan.iter().any(|(name, exp_gen, exp_ns)| {
                    if min_lease.is_some_and(|since| exp_gen >= since) {
                        return true;
                    }
                    m.roots.iter().any(|it| it.name == name && (it.generation != exp_gen || it.expires_ns != exp_ns))
                })
            });
            if evict_conflict || candidates.iter().any(|id| !m.segments.iter().any(|it| it.id == *id && it.local && Some((it.generation, it.touched)) == planned(id))) {
                if copy.is_some_and(|id| m.segments.iter().any(|it| it.id == id)) {
                    *seg = None;
                }
                return true;
            }
            if let Some(plan) = evict {
                let before = m.roots.len();
                m.roots.retain(|it| !plan.contains(&it.name));
                stats.evicted_roots = u32::try_from(before.saturating_sub(m.roots.len())).unwrap_or(u32::MAX);
            }
            // A row holding the copy's bytes (a candidate, or an older row: a pointer's copied
            // value is its newest, so no later row holds it) gives way to the copy at the run's
            // place.
            if let Some(copy) = copy {
                m.segments.retain(|it| it.published || it.id != copy || candidates.contains(&it.id));
            }
            // The copy takes the NEWEST candidate's place: a tiered merge's candidates need not
            // be contiguous, and a row it skipped must stay on its side of every pointer's
            // newest value. The copy holds a pointer only at its newest value (`copy_live`), so
            // no row after the newest candidate holds a newer one, and a skipped row before it
            // stays before it. (At the first candidate's place, a skipped row's older value
            // would move after the copy's newer one and read as the newest.)
            let newest = m.segments.iter().rposition(|it| candidates.contains(&it.id));
            let place = newest.map(|at| m.segments[..at].iter().filter(|it| it.published || !candidates.contains(&it.id)).count());
            // A published candidate stays named, as not here: a source has it.
            for it in m.segments.iter_mut().filter(|it| it.published && candidates.contains(&it.id)) {
                it.local = false;
                it.sealed = false;
            }
            m.segments.retain(|it| it.published || !candidates.contains(&it.id));
            if let Some((seg, _)) = seg {
                m.retired.retain(|it| !(it.id == seg.id && !it.log));
                // The copy is as old as the oldest it replaced: pruning sees its records' age,
                // not the slice's, and no lease taken since keeps it.
                let oldest = own_segments.iter().filter(|it| candidates.contains(&it.id)).map(|it| it.generation).min();
                seg.generation = oldest.unwrap_or(generation);
                m.segments.insert(place.unwrap_or(0).min(m.segments.len()), *seg);
            }
            for id in candidates.iter().filter(|id| copy != Some(**id)) {
                retire(m, *id, false);
            }
            adopt_orphans(m, orphans);
            false
        };
        // The slice [`Plain::fold_slice`] parked rides this commit's flip.
        let mut folded = if matches!(flip.change, Change::Commit { .. }) { self.folding.take() } else { None };
        // Unlinked before the slot lands ([`Plant::UnlinkEarly`]).
        let mut early = self.alloc_vec();
        match &mut flip.change {
            Change::Commit { segs, started_ns, roots } => {
                let evicted_under = self.cfg.liveness == Liveness::Reached
                    && self.plant() != Plant::CarrySkipped
                    && !roots.is_empty()
                    && self.manifest.roots.iter().any(|r| r.expires_ns > now_ns && !m.roots.iter().any(|cur| cur.name == r.name))
                    && self.manifest.segments.iter().any(|s| s.local && !m.segments.iter().any(|cur| cur.id == s.id));
                let extra_carry = if evicted_under {
                    match self.missing_from_eviction(roots, &m) {
                        Ok(list) => list,
                        Err(why) => {
                            refused = Some(why);
                            self.alloc_vec()
                        }
                    }
                } else {
                    self.alloc_vec()
                };
                let mut present_segs: Vec<u64, &A> = Vec::with_capacity_in(m.segments.len(), &self.alloc);
                present_segs.extend(m.segments.iter().map(|it| it.id));
                present_segs.sort_unstable();
                if refused.is_some() {
                } else if now_ns.saturating_sub(*started_ns) > GRACE_NS / 2 {
                    // A compaction may have retired its segment as named by no generation:
                    // it is not flipped.
                    refused = Some(StoreError::CommitTooSlow { took_s: now_ns.saturating_sub(*started_ns) / 1_000_000_000, grace_s: GRACE_NS / 1_000_000_000 });
                } else if let Some(&(segment, key)) = self.touches.iter().find(|(id, _)| present_segs.binary_search(id).is_err()).or_else(|| extra_carry.first())
                    && self.plant() != Plant::CarrySkipped
                {
                    // A record this commit's roots name, dead in its view (or whose protecting
                    // root was concurrently LRU-evicted), was in a segment a slice replaced
                    // since: the slice may have let it go. The command still holds its bytes
                    // (its mapping of the retired file stays valid: the unlink rule below keeps
                    // the file while this process's lease, opened before the retirement, lives,
                    // and a mapping outlives an unlink anyway), so the commit carries it
                    // forward: not flipped now; the records are written into a segment of the
                    // commit's own, outside the lock, and it flips again ([`Plain::carry`]).
                    // After [`CARRY_ROUNDS`] the refusal is the stop.
                    if self.carry_rounds >= CARRY_ROUNDS || !self.cfg.writes.contains(&Namespace::LOCAL) {
                        refused = Some(StoreError::SweptUnder { key, segment });
                    } else {
                        self.carrying.clear();
                        self.carrying.extend(self.touches.iter().filter(|(id, _)| present_segs.binary_search(id).is_err()).copied());
                        self.carrying.extend(extra_carry);
                        self.carrying.sort_unstable();
                        self.carrying.dedup();
                        carry = true;
                    }
                } else {
                    // Marked before the slice below: a slice of this process that would drop
                    // a record this commit's roots reuse aborts as another's would. `self.touches`
                    // is sorted by `(seg_id, key)`, so each distinct `seg_id` is marked once.
                    let mut prev_id = None;
                    for &(id, _) in self.touches.iter() {
                        if prev_id == Some(id) {
                            continue;
                        }
                        prev_id = Some(id);
                        if let Some(it) = m.segments.iter_mut().find(|it| it.id == id) {
                            it.touched = generation;
                        }
                    }
                    for (seg, _) in segs.iter_mut() {
                        m.retired.retain(|it| !(it.id == seg.id && !it.log));
                        seg.generation = generation;
                        match m.segments.iter().position(|it| it.id == seg.id) {
                            // The same bytes, named already: here again if they were not. A
                            // file written again is unsealed, so a seal asked for the old file
                            // (by its generation) never seals the new one. Either way the row is
                            // this generation's and moves to the END: the manifest's order is the
                            // commit order a pointer's read walks newest first (a pointer set back
                            // to a value an older row holds writes that row's bytes again), and
                            // generations stay in row order for pruning's oldest-first slices.
                            Some(at) => {
                                let mut named = m.segments.remove(at);
                                if !named.local {
                                    named.sealed = false;
                                }
                                named.generation = generation;
                                named.local = true;
                                m.segments.push(named);
                            }
                            None => m.segments.push(*seg),
                        }
                    }
                    for edit in roots.iter() {
                        match edit {
                            RootEdit::Set { name, key, life_ns, renew_only: false } => {
                                m.roots.retain(|it| it.name != *name);
                                let expires_ns = if *life_ns == NEVER { NEVER } else { now_ns.saturating_add(*life_ns).min(NEVER - 1) };
                                m.roots.push(Root { name: name.clone(), key: key.0, generation, expires_ns });
                            }
                            RootEdit::Set { name, key, life_ns, renew_only: true } => {
                                if let Some(root) = m.roots.iter_mut().find(|it| it.name == *name && it.key == key.0 && it.expires_ns > now_ns) {
                                    let expires_ns = if *life_ns == NEVER { NEVER } else { now_ns.saturating_add(*life_ns).min(NEVER - 1) };
                                    root.expires_ns = root.expires_ns.max(expires_ns);
                                    root.generation = generation;
                                }
                            }
                            RootEdit::Unset(name) => m.roots.retain(|it| it.name != *name),
                            RootEdit::Lease { since, opened_ns } => {
                                let holder = self.cfg.nonce;
                                let since = m.leases.iter().find(|it| it.holder == holder).map_or(*since, |it| it.since.min(*since));
                                m.leases.retain(|it| it.holder != holder);
                                m.leases.push(Lease { holder, since, opened_ns: *opened_ns, expires_ns: now_ns.saturating_add(LEASE_NS) });
                            }
                            RootEdit::Release => m.leases.retain(|it| it.holder != self.cfg.nonce),
                        }
                    }
                    if let Some(slice) = folded.as_mut() {
                        slice.stats.aborted = apply_slice(&mut m, &mut slice.seg, &slice.candidates, &slice.orphans, slice.evict.as_ref(), &mut slice.stats);
                        if !slice.stats.aborted && self.plant() == Plant::UnlinkEarly {
                            let copy = slice.seg.as_ref().map(|(it, _)| it.id);
                            for id in slice.candidates.iter().copied().filter(|id| copy != Some(*id)) {
                                m.retired.retain(|it| !(it.id == id && !it.log));
                                early.push(self.segment_file(id));
                            }
                        }
                    }
                }
            }
            Change::Stamp => {}
            Change::Adopt { segs } => {
                for seg in segs.iter() {
                    m.retired.retain(|it| !(it.id == seg.id && !it.log));
                    let mut seg = *seg;
                    seg.generation = generation;
                    seg.boot = self.cfg.boot;
                    seg.local = true;
                    seg.published = true;
                    match m.segments.iter_mut().find(|it| it.id == seg.id) {
                        // A file fetched again where none was is unsealed and named anew (see
                        // the commit's); one that was here keeps its row's seal.
                        Some(named) => {
                            *named = match named.local {
                                true => Segment { generation: named.generation, sealed: named.sealed, ..seg },
                                false => seg,
                            }
                        }
                        None => m.segments.push(seg),
                    }
                }
            }
            Change::Publish { ids } => {
                for seg in m.segments.iter_mut().filter(|it| ids.contains(&it.id)) {
                    seg.published = true;
                }
            }
            Change::Seal { ids } => {
                for seg in m.segments.iter_mut().filter(|it| it.local && ids.contains(&(it.id, it.generation))) {
                    seg.sealed = true;
                }
            }
            Change::Compact { seg, candidates, orphans, evict, stats } => {
                aborted = apply_slice(&mut m, seg, candidates, orphans, evict.as_ref(), stats);
            }
        }
        // What expired by the clock this flip passes in goes: a root past its life, and a lease
        // whose holder neither renewed nor let go of it (it died, or ran past [`LEASE_NS`]
        // between two commits).
        m.roots.retain(|it| it.expires_ns > now_ns);
        if self.plant() != Plant::LeaseNeverExpires {
            m.leases.retain(|it| it.expires_ns > now_ns);
        }
        if refused.is_some() || carry {
            // Not flipped: the slice waits for the next commit (or the carry's flip), unchanged.
            self.folding = folded.take();
        }
        if refused.is_some() || aborted || carry {
            flip.refused = refused;
            flip.aborted = aborted;
            flip.carry = carry;
            self.phase = Phase::Flipping(flip);
            self.unlock(effects);
            return;
        }
        // Marks of losses whose report this process handed to its caller are cleared (rows
        // another process cleared already, or that a commit named again, need nothing); a
        // mark not yet written stays clearable for the flip after the one that writes it.
        let clearable = std::mem::replace(&mut self.clearable, Vec::new_in(self.alloc.clone()));
        for id in clearable {
            if self.marks.iter().any(|(it, _)| *it == id) {
                self.clearable.push(id);
                continue;
            }
            if let Some(row) = m.retired.iter_mut().find(|it| it.id == id && !it.log) {
                row.lost = None;
            }
        }
        // The losses this open found are marked in the rows this same manifest retires them
        // with: on disk with the retirement, so a failure or a crash anywhere after the slot
        // lands leaves them for the next open to report.
        for (id, mark) in std::mem::replace(&mut self.marks, Vec::new_in(self.alloc.clone())) {
            if let Some(row) = m.retired.iter_mut().find(|it| it.id == id && !it.log) {
                row.lost = if self.plant() == Plant::ClearMarkAtOnce { None } else { Some(mark) };
            }
        }
        // Files retired past their grace go now: out of the list, and unlinked once unlocked;
        // a row marked lost stays until its report is handed, whatever its age.
        //
        // THE UNLINK RULE. A file is unlinked only when no generation a process may still
        // read names it: it left the manifest a grace period ago, and no live lease's holder
        // opened before that (a holder reads the generation it opened on, lazily, for as long
        // as it runs). The one assumption, which the platform gives: a file mapped before its
        // unlink stays readable through its mapping until it is unmapped (POSIX `munmap`; the
        // kernel drops the pages only with the last reference), so a reader that mapped a
        // segment is never hurt by the unlink, and one that has not is held off by the rule.
        let mut removals = Vec::new_in(self.alloc.clone());
        let mut holders = Vec::with_capacity_in(m.leases.len(), self.alloc.clone());
        holders.extend(m.leases.iter().map(|it| it.opened_ns));
        if let Some(slice) = folded.as_ref()
            && slice.stats.aborted
            && let Some((_, file)) = slice.seg.as_ref()
        {
            // The slice did not land: its copy, named by no generation, is this process's own.
            removals.push(file.share());
        }
        m.retired.retain(|it| {
            let held = holders.iter().any(|opened| *opened < it.since_ns.saturating_add(GRACE_NS));
            let keep = it.lost.is_some() || it.since_ns.saturating_add(GRACE_NS) > now_ns || held;
            if !keep {
                removals.push(if it.log { self.log_file(it.id) } else { self.segment_file(it.id) });
            }
            keep
        });
        // The log: a new one past its bound, the old one retired.
        //
        // Every segment row goes through `format::Row::new`, the check every later open reads
        // it through: a manifest no open would read is never written. The flip is refused
        // before the log or the slot is touched, and the store stops naming the segment
        // ([`Plain::flipped`]): the command that built it fails, not the next one.
        let most = self.tiers().segment_most();
        let estimate = match m.encode(most) {
            Ok(bytes) => bytes.len() as u64 + 24,
            Err(why) => return self.refuse_built(flip, folded, why, effects),
        };
        // A log no generation's manifest was read from may be gone: never appended to.
        let appendable = !self.log_lost || self.plant() == Plant::AppendToLostLog;
        let (log, at) = match base.slot {
            Some(slot) if appendable && slot.log_end() + estimate <= LOG_MOST => (slot.log, slot.log_end()),
            _ => {
                self.seq += 1;
                let id = format::id_of(LOG_DOMAIN, &[&self.cfg.nonce.to_le_bytes(), &self.seq.to_le_bytes(), &now_ns.to_le_bytes()]);
                if let Some(old) = base.slot {
                    retire(&mut m, old.log, true);
                }
                flip.new_log = true;
                (id, 0)
            }
        };
        let bytes = match m.encode(most) {
            Ok(bytes) => bytes,
            Err(why) => return self.refuse_built(flip, folded, why, effects),
        };
        let slot = Slot { generation, log, manifest_at: at, manifest_len: bytes.len() as u32, manifest_crc: crc32c::crc32c(&bytes) };
        flip.log = self.log_file(log);
        flip.fresh = fresh;
        flip.removals = removals;
        if let Some((effect, replaced, len)) = self.plan_bloom_effect(&m) {
            flip.bloom_effect = Some(effect);
            flip.bloom_written = Some((replaced, len));
        } else {
            flip.bloom_effect = None;
            flip.bloom_written = None;
        }
        flip.next = Some((slot, m));
        let first = if flip.new_log {
            flip.manifest_bytes = bytes;
            Effect::Truncate { file: flip.log.share(), len: 0, tag: self.tag(LOG_TRUNCATE, 0) }
        } else {
            Effect::WriteAt { file: flip.log.share(), at, bytes, tag: self.tag(LOG_WRITE, 0) }
        };
        if early.is_empty() {
            effects.push(first);
        } else {
            // The planted bug: what the slice replaced is unlinked first, while the generation
            // on disk names it, and the log waits for the unlinks.
            flip.early = early.len() as u32;
            flip.deferred = Some(first);
            for (i, file) in early.into_iter().enumerate() {
                effects.push(Effect::Remove { file, tag: self.tag(REMOVE, EARLY_REMOVE + i as u32) });
            }
        }
        // The folded slice's outcome, for [`Plain::flipped`].
        self.folding = folded;
        self.phase = Phase::Flipping(flip);
    }

    /// The flip's writes to the head, one after another, the slot last. The slot landing is
    /// the commit point: everything else a flip writes (a fresh head's cache tag and its
    /// other slot blanked) is written, and has succeeded, before it, so no write of a flip can
    /// fail once its generation is on disk. A flip that commits and then stops would leave a
    /// durable generation whose news (the losses an open found and retired, a commit's
    /// outcome) dies with the process (seed 875559 of the property tests' former engine: an open's fresh flip retired the
    /// orphans a power cut left, its slot landed beside a failed cache tag write, it stopped,
    /// and the next open found them retired and reported none).
    fn write_slot(&mut self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = &mut self.phase else { unreachable!("a slot is a flip's") };
        if let Some(bloom_effect) = flip.bloom_effect.take() {
            effects.push(bloom_effect);
            return;
        }
        if flip.fresh {
            let tag_file = self.name(CACHEDIR_TAG);
            effects.push(Effect::WriteUnsynced { file: tag_file, bytes: CACHEDIR_SIGNATURE.to_vec(), tag: self.tag(SLOT_WRITE, 1) });
        } else {
            self.write_the_slot(effects);
        }
    }

    /// No slot was whole, so the other may hold a torn one: blanked in place (it holds nothing
    /// to lose), so the trace is reported once, at the open that found it.
    fn blank_other_slot(&self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = &self.phase else { unreachable!("a slot is a flip's") };
        let slot = flip.next.as_ref().expect("built").0;
        let other = SLOT_AT[((slot.generation + 1) % 2) as usize];
        effects.push(Effect::WriteAt { file: self.head.share(), at: other, bytes: vec![0u8; format::SLOT], tag: self.tag(SLOT_WRITE, 2) });
    }

    /// The commit point. Every flip, the first included, writes one 64-byte slot in place in
    /// the head the open made at its full length: inside one sector, never extending or
    /// rewriting the file, so a tear can destroy only the slot being written, never the one
    /// before it. The first flip has no slot before it: a tear of it leaves none whole, and
    /// the next open reads the directory's segments from their footers and reports them.
    fn write_the_slot(&self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = &self.phase else { unreachable!("a slot is a flip's") };
        let slot = flip.next.as_ref().expect("built").0;
        let at = SLOT_AT[(slot.generation % 2) as usize];
        effects.push(Effect::WriteAt { file: self.head.share(), at, bytes: slot.encode().to_vec(), tag: self.tag(SLOT_WRITE, 0) });
    }

    /// The flip's writes are done (or it was refused): let go of the lock.
    fn unlock(&mut self, effects: &mut Vec<Effect>) {
        self.holds_lock = false;
        effects.push(Effect::Unlock { lock: self.tag(LOCK, 0), tag: self.tag(UNLOCK, 0) });
    }

    /// Unlocked: the new generation is this process's; files past their grace are unlinked,
    /// then what it names and this process has not mapped is mapped.
    fn flipped(&mut self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!("an unlock is a flip's") };
        let flip = *flip;
        if let Some(why) = flip.refused {
            self.pending_bloom.clear();
            let roots = match flip.change {
                Change::Commit { roots, .. } => roots,
                _ => Vec::new_in(self.alloc.clone()),
            };
            // A row no open would read is a bug of this blaze, not of the change: the store
            // stops, so every later ask of this command (its cleanup, its seal) fails with it.
            let stops = matches!(why, StoreError::Row { .. });
            self.commit_refused(why.clone(), roots);
            if stops {
                self.stop(why);
            }
            return;
        }
        if flip.carry {
            let Change::Commit { segs, started_ns, roots } = flip.change else { unreachable!("only a commit carries") };
            self.carry(segs, started_ns, roots, effects);
            return;
        }
        if flip.aborted {
            self.pending_bloom.clear();
            let Change::Compact { seg, mut stats, .. } = flip.change else { unreachable!("only a compaction aborts") };
            stats.aborted = true;
            self.sliced = Some(stats);
            match seg {
                Some((_, file)) => {
                    // Never named: this process's own, removed now.
                    self.removing.clear();
                    self.removing.push(file.share());
                    effects.push(Effect::Remove { file, tag: self.tag(REMOVE, 0) });
                    self.phase = Phase::Removing { left: 1, then: Then::Compact(stats) };
                }
                None => self.phase = Phase::Ready,
            }
            return;
        }
        let (slot, manifest) = flip.next.expect("built");
        let then = match flip.change {
            Change::Commit { segs, .. } => {
                self.committing.clear();
                self.committing.extend(segs.iter().map(|(it, _)| it.id));
                if let Some(slice) = self.folding.take() {
                    self.sliced = Some(Sliced { removed: flip.removals.len() as u32, segments: if slice.stats.aborted { 0 } else { slice.stats.segments }, ..slice.stats });
                }
                Then::Commit
            }
            Change::Stamp | Change::Adopt { .. } | Change::Publish { .. } | Change::Seal { .. } => Then::Stamp,
            Change::Compact { mut stats, .. } => {
                stats.removed = flip.removals.len() as u32;
                Then::Compact(stats)
            }
        };
        let prev_gen = self.manifest.generation;
        let compact_only = matches!(then, Then::Compact(stats) if !stats.aborted)
            && manifest.generation == prev_gen.saturating_add(1)
            && self.dropped.is_empty();
        self.checked.clear();
        self.dropped.clear();
        self.torn_stamp = false;
        self.slot = Some(slot);
        self.manifest = manifest;
        if !compact_only {
            self.invalidate_reach();
        }
        let pending_bloom = core::mem::replace(&mut self.pending_bloom, blaze_own::Vec::new_in(BLOOM_ALLOC));
        for (digest, rec) in pending_bloom {
            if self.manifest.segments.iter().any(|seg| seg.local && seg.digest == digest) {
                self.bloom.insert_owned(rec);
            }
        }
        self.bloom.prune(&self.manifest.segments);
        if let Some((replaced, len)) = flip.bloom_written {
            self.bloom.note_written(replaced, len);
        }
        self.leased = self.manifest.leases.iter().any(|it| it.holder == self.cfg.nonce);
        // The slot names the log this flip wrote its manifest to.
        self.log_lost = false;
        if self.cfg.durable && (self.plant() == Plant::FlipBeforeSync || self.plant() == Plant::MergeFlipsBeforeSync) {
            for (i, file) in std::mem::replace(&mut self.late_sync, Vec::new_in(self.alloc.clone())).into_iter().enumerate() {
                effects.push(Effect::Sync { file, tag: self.tag(LATE_SYNC, i as u32) });
            }
        }
        let left = flip.removals.len() as u32;
        for (i, file) in flip.removals.iter().enumerate() {
            effects.push(Effect::Remove { file: file.share(), tag: self.tag(REMOVE, i as u32) });
        }
        self.removing = flip.removals;
        if left == 0 {
            self.map_new(then, effects);
        } else {
            self.phase = Phase::Removing { left, then };
        }
    }

    /// A refused commit's outcome: `why`, its roots and held records kept for the next.
    fn commit_refused(&mut self, why: StoreError, roots: Vec<RootEdit, A>) {
        self.committed = Some(Err(why));
        let later = std::mem::replace(&mut self.roots_pending, roots);
        self.roots_pending.extend(later);
        self.sealed_back();
        self.phase = Phase::Ready;
    }

    /// One round of a commit carrying forward the records its roots name whose segment a
    /// slice replaced since this process read it (`carrying`, listed by the flip that found
    /// them, which unlocked without flipping): each copied from this process's own mapping of
    /// the retired file (valid: the unlink rule keeps the file while the lease of a holder
    /// that opened before its retirement lives, and a mapping outlives an unlink) with its CRC
    /// as recorded, into one segment of [`Namespace::LOCAL`] written with no lock; the flip is
    /// then asked again, naming the commit's segments and it. The records are the commit's
    /// own from then on: no longer reused, so nothing more is checked of them. Counted in
    /// [`Plain::carried`].
    fn carry(&mut self, segs: Vec<(Segment, FileName), A>, started_ns: u64, roots: Vec<RootEdit, A>, effects: &mut Vec<Effect>) {
        self.carry_rounds += 1;
        let carrying = std::mem::replace(&mut self.carrying, Vec::new_in(self.alloc.clone()));
        let mut out = Batch::new(Namespace::LOCAL);
        for &(segment, key) in carrying.iter() {
            // Each key once: `reused` lists it with the one segment its lookup found.
            let Some((at, entry)) = self.find_in(&[Namespace::LOCAL], &Key(key)) else {
                // Not mapped here any more: nothing to carry from. The typed stop.
                return self.commit_refused(StoreError::SweptUnder { key, segment }, roots);
            };
            if ReservedTag::Pointer.is(entry.tag) {
                // A pointer's carried value would read as its newest: not carried.
                return self.commit_refused(StoreError::SweptUnder { key, segment }, roots);
            }
            if let Err(error) = out.put_known(&entry, self.maps[at].payload(&entry)) {
                return self.commit_refused(error.into_store(entry.tag), roots);
            }
        }
        self.touches.retain(|it| carrying.binary_search(it).is_err());
        if self.carried_in_commit.is_none() {
            self.carried_in_commit = Some(self.own_vec());
        }
        if let Some(done) = &mut self.carried_in_commit {
            for &(_, key) in carrying.iter() {
                done.push(key);
            }
            done.sort_unstable();
            done.dedup();
        }
        self.carried += out.entries.len() as u64;
        match laid_of(out, self.alloc.clone()) {
            Ok(laid) => self.write_segment(laid, Why::Commit { started_ns, roots, rest: Vec::new_in(self.alloc.clone()), done: segs }, effects),
            Err(why) => self.commit_refused(why, roots),
        }
    }

    /// Records this process's commits carried forward, ever ([`CARRY_ROUNDS`]): the store's
    /// count of commits that met a slice landing under them and lost nothing.
    pub fn carried(&self) -> u64 {
        self.carried
    }

    /// A refused commit's held records are held again, for the next.
    fn sealed_back(&mut self) {
        let sealed = std::mem::replace(&mut self.sealed, Vec::new_in(self.alloc.clone()));
        for old in &sealed {
            let held = self.held.iter_mut().find(|it| it.batch.namespace == old.batch.namespace).expect("one held batch for each namespace written");
            for entry in &old.batch.entries {
                let key = Key(entry.key);
                if held.at.contains_key(&key) {
                    continue;
                }
                held.batch.put_known(entry, old.batch.payload(entry)).expect("it fit before");
                held.at.insert(key, held.batch.entries.len() - 1);
            }
        }
    }

    // ---- adopting a generation ----

    /// Maps each segment of this process's manifest it has not mapped; then what `then` says.
    fn map_new(&mut self, then: Then, effects: &mut Vec<Effect>) {
        // Lazy until whole: no row is mapped before a probe reaches it (probe.rs). The plant
        // maps every row at the open, as the open did before piece 3.
        if !self.eager && !(then == Then::Open && self.plant() == Plant::MapsAllAtOpen) {
            self.mapped(then, effects);
            return;
        }
        let mut to_map = 0usize;
        let mut sealed_to_map = 0usize;
        for seg in &self.manifest.segments {
            if !seg.local || self.by_id.contains_key(&seg.id) || self.dropped.contains(&seg.id) {
                continue;
            }
            to_map = to_map.saturating_add(1);
            if seg.boot != self.cfg.boot && seg.sealed {
                sealed_to_map = sealed_to_map.saturating_add(1);
            }
        }
        if to_map == 0 {
            self.mapped(then, effects);
            return;
        }
        self.reserve_maps(to_map, sealed_to_map, effects);
        let mut left = 0u32;
        let len = self.manifest.segments.len();
        for i in 0..len {
            let Some(seg) = self.manifest.segments.get(i).copied() else { continue };
            if !seg.local || self.by_id.contains_key(&seg.id) || self.dropped.contains(&seg.id) {
                continue;
            }
            left = left.saturating_add(1);
            self.queue_map(i, seg.id, effects);
        }
        if left == 0 {
            self.mapped(then, effects);
        } else {
            self.phase = Phase::Mapping { left, then };
        }
    }

    /// One segment's map came back: checked (its length always; its CRC once, when it is of
    /// another boot and not sealed); no page of it is touched.
    fn on_map(&mut self, index: u32, done: Done) -> Result<(), StoreError> {
        let row = usize::try_from(index).unwrap_or(usize::MAX);
        let seg = self.manifest.segments[row];
        let file = self
            .pending_map_files
            .get_mut(row)
            .and_then(Option::take)
            .unwrap_or_else(|| self.segment_file(seg.id));
        let generation = self.manifest.generation;
        let earlier_boot = seg.boot != self.cfg.boot;
        let map = match done {
            Done::Mapped(map) => map,
            done => {
                // A segment this boot's generation names that cannot be mapped: something
                // outside blaze removed it.
                let why = io(MAPPING, &file, &done).unwrap_or(StoreError::Unasked { op: MAP, index, phase: "mapping a segment" });
                if earlier_boot {
                    // Decided once every map is in, by whether the directory still holds it.
                    self.unopened.push((seg, file, why));
                    return Ok(());
                }
                return Err(why);
            }
        };
        // The proof: this boot's row needs no byte read; nor does a sealed row of an earlier
        // boot (its bytes reached the media before the generation that says so); an earlier
        // boot's unsealed segment, the tail, is checked in full once, here, and the open's next
        // flip records the boot (and seals it) so no process checks it again.
        let unverified = Unverified::new(seg, file, map);
        let mut sealed = false;
        let proved = match Verified::recorded(unverified, self.boot) {
            Ok(proved) => Ok(proved),
            Err(unverified) if !earlier_boot => {
                return Err(StoreError::Changed { segment: seg.id, generation, len: unverified.len(), named: unverified.seg().file_len() });
            }
            Err(unverified) if seg.sealed => match Verified::sealed(unverified, self.boot) {
                Ok(proved) => {
                    sealed = true;
                    Ok(proved)
                }
                // Sealed and of another length than its row: something outside blaze changed
                // the file; it is lost, by name, as a torn one is.
                Err(unverified) => {
                    let at = unverified.len();
                    Err((seg, unverified.into_file(), StoreError::Layout {
                        segment: SegmentId(seg.digest),
                        what: "it is sealed, and its file's length is not the one its generation names",
                        at,
                    }))
                }
            },
            // A probe's map: length checked and parked; the full check waits for its first hit.
            Err(unverified) if self.parks() => return self.park(unverified),
            Err(unverified) if self.plant() == Plant::TrustBoot => Ok(self.trust_for_a_plant(unverified)),
            Err(unverified) => {
                let mut checks = self.checks.get();
                let proved = Verified::check(unverified, self.boot, &mut checks);
                self.checks.set(checks);
                self.checked_ids.push(seg.id);
                proved
            }
        };
        let proved = match proved {
            Ok(proved) => proved,
            Err((seg, file, what)) => {
                let why = format!("it was written in an earlier boot and did not survive its end whole ({what}): a power cut before the kernel wrote it back; its records are dropped");
                self.lose(seg, file, LostKind::NotWhole, why);
                return Ok(());
            }
        };
        if earlier_boot && !sealed {
            self.checked.insert(seg.id);
            self.report.checked += 1;
        }
        if sealed {
            self.sealed_ids.push(seg.id);
            self.report.sealed += 1;
        }
        // No page of it is touched here (store-next.md): a repeat with nothing changed reads
        // the index pages its lookups probe and no other, and a segment no lookup reaches is
        // never read from the disk.
        self.by_id.insert(seg.id, self.maps.len());
        self.maps.push(proved);
        Ok(())
    }

    #[cfg(test)]
    fn trust_for_a_plant(&self, it: Unverified) -> Verified {
        Verified::unchecked_for_a_plant(it, self.boot)
    }

    #[cfg(not(test))]
    fn trust_for_a_plant(&self, _: Unverified) -> Verified {
        unreachable!("outside tests a store's plant is always none")
    }

    /// A segment its generation names, lost: dropped from the next generation, reported with
    /// `why` (the open's own words, with what it saw), and marked as `kind` in the row that
    /// retires it (a later open's report of the mark says [`LostMark::why`]).
    fn lose(&mut self, seg: Segment, file: FileName, kind: LostKind, why: String) {
        self.dropped.insert(seg.id);
        self.invalidate_reach();
        self.marks.push((seg.id, LostMark { kind, records: Some(seg.count) }));
        self.unreported.push(seg.id);
        self.report.lost.push(Lost { file, segment: Some(seg.id), records: Some(u64::from(seg.count)), why });
    }

    /// A segment named by no generation, found at an open's fallback: reported, and marked in
    /// the row its next flip retires it with.
    fn lose_orphan(&mut self, file: FileName, id: u64, records: Option<u64>) {
        self.marks.push((id, LostMark { kind: LostKind::Orphan, records: records.and_then(|it| u32::try_from(it).ok()) }));
        self.unreported.push(id);
        self.report.lost.push(Lost { file, segment: Some(id), records, why: lost_orphan() });
    }

    /// The open's report was handed to its caller (for [`super::Driven`], its open returning
    /// with [`Plain::report`] holding the losses): from now on the marks it reported may go,
    /// at the next flip this process makes (a commit's or a compaction's own, or
    /// [`Plain::acknowledge`]'s). Until then a crash or a failure anywhere leaves them for the
    /// next open, which reports them again: a loss is reported at least once, never zero
    /// times, and more than once only across a crash or a failure between its report and the
    /// flip that clears it.
    pub fn handed(&mut self) {
        let reported = std::mem::replace(&mut self.unreported, Vec::new_in(self.alloc.clone()));
        for id in reported {
            if !self.clearable.contains(&id) {
                self.clearable.push(id);
            }
        }
    }

    /// For a process that makes no flip of its own: one flip that clears the marks its handed
    /// report named. `false` when there is none to clear or the store is not ready.
    pub fn acknowledge(&mut self, effects: &mut Vec<Effect>) -> bool {
        if !self.ready() || self.clearable.is_empty() {
            return false;
        }
        self.start_flip(Change::Stamp, effects);
        true
    }

    /// Adds `needle` to what names this machine ([`PlainConfig::machine`]); an empty one is
    /// not added (it would be found everywhere). Copied into the store's allocator.
    pub fn machine(&mut self, needle: &[u8]) {
        if !needle.is_empty() && !self.cfg.machine.iter().any(|it| it.as_slice() == needle) {
            let mut kept = Vec::with_capacity_in(needle.len(), self.alloc.clone());
            kept.extend_from_slice(needle);
            self.cfg.machine.push(kept);
        }
    }

    /// Where this machine's first path occurs in `bytes`, when it does ([`machine_in`]).
    fn machine_at(&self, bytes: &[u8]) -> Option<(u64, u32)> {
        machine_in(&self.cfg.machine, bytes)
    }

    /// What names this machine ([`PlainConfig::machine`] and [`Plain::machine`]'s).
    pub(crate) fn needles(&self) -> &[Vec<u8, A>] {
        &self.cfg.machine
    }

    /// The exit tail sealed (store-next.md), asked by a command after it answered, as
    /// [`Plain::compact`] is: one barrier ([`Effect::Barrier`]: `F_BARRIERFSYNC` on macOS,
    /// `fdatasync` on Linux) on each segment this boot wrote or fetched and no generation
    /// names sealed, then a flip that seals the ones still named so. No wait is on the edit
    /// path: the commit before it took no sync, and a power cut before this flip lands leaves
    /// them unsealed, checked whole at the next boot's open as before. `false`: nothing to
    /// seal, or the store is busy.
    pub fn seal(&mut self, effects: &mut Vec<Effect>) -> bool {
        if !self.ready() {
            return false;
        }
        let mut ids = Vec::new_in(self.alloc.clone());
        ids.extend(self.manifest.segments.iter().filter(|it| it.local && !it.sealed && it.boot == self.cfg.boot).map(|it| (it.id, it.generation)));
        if ids.is_empty() {
            // Nothing to seal, but rows this process checked at a first hit (or lost) after
            // its open: one flip records them for the boot, so no later process checks again.
            if self.pending_stamps() {
                self.start_flip(Change::Stamp, effects);
                return true;
            }
            return false;
        }
        if self.plant() == Plant::SealBeforeBarrier {
            self.start_flip(Change::Seal { ids }, effects);
            return true;
        }
        for (i, (id, _)) in ids.iter().enumerate() {
            let (file, tag) = (self.segment_file(*id), self.tag(BARRIER, i as u32));
            // A fetched segment is written whole with no sync (`Effect::WriteUnsynced`, by
            // `Driven::fetch_missing`), which a power cut may undo until a sync covers it: a
            // barrier orders writes, not a name's replacement, so a published row (fetched or
            // pushed: few, and off the edit path) is synced.
            let published = self.manifest.segments.iter().any(|it| it.id == *id && it.published);
            if published {
                self.put_path.syncs += 1;
            } else {
                self.put_path.barriers += 1;
            }
            effects.push(if published { Effect::Sync { file, tag } } else { Effect::Barrier { file, tag } });
        }
        self.phase = Phase::Sealing { left: ids.len() as u32, ids };
        true
    }

    /// The sealed segments of an earlier boot this boot has not walked yet: what the
    /// background pass has left ([`Plain::scrub_next`]), oldest generation first.
    pub fn unscrubbed(&self) -> Vec<u64, A> {
        let mut left: Vec<&Segment, &A> = Vec::new_in(&self.alloc);
        // Walked whole or lost by this process, its flip not landed yet: not walked again.
        let pending = |id: u64| self.checked.contains(&id) || self.dropped.contains(&id);
        left.extend(self.manifest.segments.iter().filter(|it| it.local && it.sealed && it.boot != self.cfg.boot && self.by_id.contains_key(&it.id) && !pending(it.id)));
        left.sort_unstable_by_key(|it| (it.generation, it.id));
        let mut out = Vec::with_capacity_in(left.len(), self.alloc.clone());
        out.extend(left.iter().map(|it| it.id));
        out
    }

    /// One slice of the background pass (store-next.md, "the bound"): run after the answer by
    /// the helper a command leaves, never before it. A sealed segment is proved at a boot's
    /// open with no byte read; this walks each once a boot, at most `budget` bytes a slice
    /// (at least one record), checking every record's CRC-32C as [`Plain::scrub`] does. A
    /// segment walked whole is recorded for this boot (its row's boot, by a flip: the same
    /// record as the open's check), so no later slice of this boot, in this process or
    /// another, walks it again. A segment with a record that fails is lost by name and retired
    /// by a flip, exactly as the open's whole check loses one: the next lookup misses, and the
    /// record is rebuilt or fetched. Nothing blocks: the walk reads the mapping, and the flip
    /// is the store's ordinary one. `None`: nothing left this boot, or the store is busy.
    pub fn scrub_next(&mut self, budget: u64, effects: &mut Vec<Effect>) -> Option<Scrubbed> {
        if !self.ready() {
            return None;
        }
        let left = self.unscrubbed();
        let at = match self.scrubbing {
            // The segment this process was in, still left and still that row.
            Some(at) if left.contains(&at.id) && self.manifest.segments.iter().any(|it| it.id == at.id && it.generation == at.generation) => at,
            _ => {
                let id = *left.first()?;
                let generation = self.manifest.segments.iter().find(|it| it.id == id).map_or(0, |it| it.generation);
                ScrubAt { id, generation, next: 0 }
            }
        };
        let map = *self.by_id.get(&at.id)?;
        let mut corrupt = Vec::new_in(self.alloc.clone());
        let (next, bytes) = self.maps[map].scrub_from(at.next, budget, &mut corrupt);
        let count = self.maps[map].seg().count as usize;
        let seg = *self.maps[map].seg();
        if let Some(rot) = corrupt.into_iter().next() {
            self.scrubbing = None;
            let why = format!("a sealed segment's record rotted on the disk after its seal ({}): found by this boot's background pass; its records are dropped", rot.what);
            let file = self.segment_file(at.id);
            self.lose(seg, file, LostKind::NotWhole, why);
            // Out of every lookup now, before its flip lands (`lose` marks it dropped, which
            // every probe skips): no lookup is served from it again. The mapping stays
            // (mappings live as long as the process), so a reader holding its bytes keeps them.
            self.start_flip(Change::Stamp, effects);
            return Some(Scrubbed { segment: at.id, bytes, done: true, rot: Some(rot) });
        }
        if next >= count {
            self.scrubbing = None;
            self.checked.insert(at.id);
            self.start_flip(Change::Stamp, effects);
            return Some(Scrubbed { segment: at.id, bytes, done: true, rot: None });
        }
        self.scrubbing = Some(ScrubAt { next, ..at });
        if self.plant() == Plant::ScrubMarkedEarly {
            self.checked.insert(at.id);
            self.start_flip(Change::Stamp, effects);
        }
        Some(Scrubbed { segment: at.id, bytes, done: false, rot: None })
    }

    fn mapped(&mut self, then: Then, effects: &mut Vec<Effect>) {
        self.pending_map_files.clear();
        self.order_view();
        match then {
            Then::Commit => {
                let segments = std::mem::replace(&mut self.committing, Vec::new_in(self.alloc.clone()));
                for id in segments.iter() {
                    if let Some(row) = self.manifest.segments.iter().find(|it| it.id == *id) {
                        self.written_here += row.file_len();
                        self.segments_here += 1;
                    }
                }
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segments }));
                self.sealed.clear();
            }
            Then::Compact(mut stats) => {
                let retired_ready = self.manifest.retired.iter().any(|it| it.lost.is_none() && it.since_ns.saturating_add(GRACE_NS) <= self.now_ns);
                if retired_ready {
                    stats.more = true;
                } else {
                    let over = self.over_budget();
                    let (next_evict, kept_count) = if over && !self.manifest.roots.is_empty() {
                        if let Err(why) = self.ensure_reach(self.now_ns, true) {
                            return self.stop(why);
                        }
                        match self.reach_cache.as_ref() {
                            Some(cache) => {
                                let (_, plan, kept) = self.evict_from_reach(cache);
                                (plan, Some(kept))
                            }
                            None => (None, None),
                        }
                    } else {
                        (None, None)
                    };
                    if next_evict.is_some() {
                        stats.more = true;
                    } else {
                        let next = match self.cfg.liveness {
                            Liveness::Everything => Ok(self.plan().0),
                            Liveness::Reached => self.plan_reached(self.now_ns, over, kept_count).map(|it| it.0),
                        };
                        match next {
                            Ok(next) => stats.more = !next.is_empty(),
                            Err(why) => return self.stop(why),
                        }
                    }
                }
                self.sliced = Some(stats);
            }
            Then::Open | Then::Stamp | Then::Probe => {}
            Then::All => self.all_mapped(),
        }
        if then == Then::Open && self.pending_stamps() {
            self.start_flip(Change::Stamp, effects);
            return;
        }
        self.phase = Phase::Ready;
    }

    // ---- completions ----

    /// Takes one of this store's completions; `now_ns` is the wall clock as the driver reads it
    /// (the simulation's own in tests).
    pub fn complete(&mut self, completion: Completion, now_ns: u64, effects: &mut Vec<Effect>) {
        debug_assert!(self.owns(completion.tag));
        let op = ((completion.tag.0 - self.cfg.tags) >> 32) as u8;
        let index = completion.tag.0 as u32;
        self.now_ns = now_ns;
        if op == LATE_SYNC {
            return;
        }
        if matches!(self.phase, Phase::Broken) {
            // Stopped already, with the first failure named ([`Plain::broken`]): what was in
            // flight as it stopped (the unlock it asked as it stopped; never a write under the
            // lock, which a flip asks one at a time) completes here and changes nothing;
            // the first reason is the one kept.
            return;
        }
        if let Err(why) = self.step(op, index, completion.done, now_ns, effects) {
            if self.holds_lock {
                // A failure under the flip lock lets the lock go as the store stops, with
                // nothing this process wrote under it still in flight (a flip writes under it
                // one effect at a time, [`Plain::write_slot`]): a process
                // that lives on with a broken store (a daemon) must not hold every other
                // process's commit for good, and the kernel would let it go only at exit.
                self.unlock(effects);
            }
            self.stop(why);
        }
    }

    fn step(&mut self, op: u8, index: u32, done: Done, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        let unexpected = |phase: &'static str| StoreError::Unasked { op, index, phase };
        let fail = |doing: &'static str, file: &FileName, done: &Done| io(doing, file, done);
        let unlink_early = self.plant() == Plant::UnlinkEarly;
        match (op, &mut self.phase) {
            (BLOOM_MAP, _) if self.bloom_loading => {
                self.bloom_loading = false;
                if let Done::Mapped(map) = done {
                    self.bloom.set_mapping(map);
                }
                if let Some(parked) = self.open_parked.take() {
                    return self.resolved(Ctx::Open, parked.base, false, parked.fell_back, now_ns, effects);
                }
                Ok(())
            }
            (REMOVE, Phase::Flipping(flip)) if unlink_early && index >= EARLY_REMOVE && flip.early > 0 => {
                flip.early -= 1;
                if flip.early == 0
                    && let Some(first) = flip.deferred.take()
                {
                    effects.push(first);
                }
                Ok(())
            }
            (HEAD_READ, Phase::ReadHead(ctx)) => {
                let ctx = *ctx;
                let Done::Read(bytes) = done else {
                    return Err(fail("reading the head", &self.head, &done).unwrap_or_else(|| unexpected("reading the head")));
                };
                let read = format::read_head(&bytes);
                if let HeadRead::Refused(why) = &read {
                    return Err(why.clone());
                }
                if let HeadRead::Version { found, .. } = read {
                    // A store of the previous layout (rule 26): refused by name, never read as
                    // empty; there is no migration, the store is rebuilt from what made it.
                    return Err(StoreError::Format { what: format::HEAD_SLOT, found, wanted: FORMAT });
                }
                if bytes.len() < HEAD_LEN {
                    // A plain head (or nothing) short of its length: made at its full length
                    // and synced before anything else is written, so no flip ever extends or
                    // rewrites it. The cut extends with zeros and never touches a byte there.
                    if ctx == Ctx::Flip || self.head_made {
                        return Err(StoreError::Head { what: format::HEAD_CUT, at: bytes.len() as u64 });
                    }
                    self.head_made = true;
                    // The trace a cut head holds is kept: the zeros the cut adds are not one.
                    self.head_torn = match read {
                        HeadRead::Fresh { torn } | HeadRead::Committed(_, _, torn) => torn,
                        HeadRead::Refused(_) | HeadRead::Version { .. } => None,
                    };
                    // THE FOLDERS FIRST (the module's "Durable names"): the cache directory tag
                    // written durably, which makes the store's folder and every folder above it
                    // that is missing, each one's name synced into its parent (blaze-core
                    // `Effect::Write`, `make_parent_durably`), before anything else is made in
                    // it. The head is made after, so its Truncate makes no folder.
                    let tag_file = self.name(CACHEDIR_TAG);
                    let bytes = CACHEDIR_SIGNATURE.to_vec();
                    let tag = self.tag(HEAD_MAKE, 0);
                    effects.push(Effect::Write { file: tag_file, bytes, tag });
                    self.phase = Phase::MakeHead;
                    return Ok(());
                }
                let torn = match read {
                    HeadRead::Fresh { torn } | HeadRead::Committed(_, _, torn) => torn.or(self.head_torn.take()),
                    HeadRead::Refused(_) | HeadRead::Version { .. } => None,
                };
                if let Some(at) = torn
                    && ctx == Ctx::Open
                {
                    self.torn_stamp = true;
                    self.report.lost.push(Lost {
                        file: self.head.share(),
                        segment: None,
                        records: None,
                        why: format!("its slot {at} is torn: a flip a power cut cut short; what no whole generation names is read from the segments' own footers and reported"),
                    });
                }
                match read {
                    HeadRead::Refused(_) | HeadRead::Version { .. } => unreachable!("refused above"),
                    // No whole slot: at an open, what the directory holds is read from the
                    // files' own footers and reported (nothing, for a store never written).
                    HeadRead::Fresh { .. } => self.resolved(ctx, Base { slot: None, manifest: Manifest::default() }, true, ctx == Ctx::Open, now_ns, effects),
                    HeadRead::Committed(newest, older, _) => {
                        if ctx == Ctx::Flip && self.plant() == Plant::StaleBase && self.slot.is_some() {
                            let base = Base { slot: self.slot, manifest: self.manifest.clone() };
                            return self.resolved(ctx, base, false, false, now_ns, effects);
                        }
                        if ctx == Ctx::Flip && Some(newest) == self.slot {
                            // Nothing flipped since this process last read: its manifest is the base.
                            let base = Base { slot: self.slot, manifest: self.manifest.clone() };
                            return self.resolved(ctx, base, false, false, now_ns, effects);
                        }
                        // A torn slot beside it: a newer flip was cut, and the segments it
                        // named are found and reported by the directory's look.
                        if ctx == Ctx::Open && !self.bloom_loading {
                            self.bloom_loading = true;
                            effects.push(Effect::Map { file: self.bloom_file.share(), extent: Extent::Whole, tag: self.tag(BLOOM_MAP, 0) });
                        }
                        self.read_manifest(ctx, [Some(newest), older], 0, ctx == Ctx::Open && torn.is_some(), effects);
                        Ok(())
                    }
                }
            }
            (MANIFEST_READ, Phase::ReadManifest { ctx, slots, at, fell_back }) => {
                let (ctx, slots, at, fell_back) = (*ctx, *slots, *at, *fell_back);
                let slot = slots[at].expect("read");
                let log = self.log_file(slot.log);
                let bytes = match done {
                    Done::Read(bytes) => bytes,
                    done => {
                        // Whether the log is there decides what the failure means: one the
                        // directory still holds failed to read (an error); one a power cut took
                        // is the same as torn.
                        let why = fail("reading a manifest", &log, &done).unwrap_or_else(|| unexpected("reading a manifest"));
                        effects.push(Effect::List { dir: self.cfg.dir.share().into(), tag: self.tag(LIST, 0) });
                        self.phase = Phase::CheckLog { ctx, slots, at, why };
                        return Ok(());
                    }
                };
                let most = self.tiers().segment_most();
                let manifest = (bytes.len() == slot.manifest_len as usize && crc32c::crc32c(&bytes) == slot.manifest_crc)
                    .then(|| Manifest::decode(&bytes, most))
                    .transpose();
                match manifest {
                    Ok(Some(manifest)) if manifest.generation == slot.generation => {
                        // Read back whole: the log is there to append to.
                        self.log_lost = false;
                        if ctx == Ctx::Open {
                            self.report.whole = Some(manifest.generation);
                        }
                        self.resolved(ctx, Base { slot: Some(slot), manifest }, false, fell_back, now_ns, effects)
                    }
                    Err(why) => Err(why),
                    _ => self.manifest_torn(ctx, slots, at, now_ns, effects),
                }
            }
            (HEAD_MAKE, Phase::MakeHead) => {
                let tag_file = self.name(CACHEDIR_TAG);
                let (doing, file) = match index {
                    0 => ("writing the cache directory tag, which makes the store's folders durably", &tag_file),
                    1 => ("making the head at its length", &self.head),
                    2 => ("syncing the head made", &self.head),
                    _ => ("syncing the store's directory after making the head", &self.cfg.dir),
                };
                if let Some(why) = fail(doing, file, &done) {
                    return Err(why);
                }
                if index == 0 {
                    // The folders are durable: the head now, a Truncate that keeps what a cut
                    // head holds and extends it with zeros (never a whole-file write: another
                    // process may have made the head and flipped since this one read it short,
                    // and a Truncate to the length it has is no change).
                    effects.push(Effect::Truncate { file: self.head.share(), len: HEAD_LEN as u64, tag: self.tag(HEAD_MAKE, 1) });
                } else if index == 1 {
                    // Synced once in the store's life, before its first segment: so an empty or
                    // short head never stands beside segments a power cut kept.
                    effects.push(Effect::Sync { file: self.head.share(), tag: self.tag(HEAD_MAKE, 2) });
                } else if index == 2 {
                    // And its name with it: a power cut never keeps a segment's name and loses
                    // the head's.
                    effects.push(Effect::SyncDir { dir: self.cfg.dir.share().into(), tag: self.tag(HEAD_MAKE, 3) });
                } else {
                    self.phase = Phase::ReadHead(Ctx::Open);
                    effects.push(Effect::ReadAll { file: self.head.share(), tag: self.tag(HEAD_READ, 0) });
                }
                Ok(())
            }
            (LIST, Phase::CheckLog { .. }) => {
                let Phase::CheckLog { ctx, slots, at, why } = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                let Done::Listed(listed) = done else {
                    return Err(fail("listing", &self.cfg.dir, &done).unwrap_or_else(|| unexpected("listing")));
                };
                let log = self.log_file(slots[at].expect("read").log);
                let leaf = log.as_str().rsplit('/').next().unwrap_or(log.as_str());
                if listed.iter().any(|(name, _)| name.as_str().rsplit('/').next() == Some(leaf)) {
                    return Err(why);
                }
                self.manifest_torn(ctx, slots, at, now_ns, effects)
            }
            (LIST, Phase::Orphans { .. }) => {
                let Done::Listed(listed) = done else {
                    return Err(fail("listing", &self.cfg.dir, &done).unwrap_or_else(|| unexpected("listing")));
                };
                let current_log = self.slot.map(|it| it.log);
                let mut left = 0;
                self.listed.clear();
                for (name, meta) in listed {
                    let leaf = name.as_str().rsplit('/').next().unwrap_or(name.as_str());
                    let Some((id, log)) = parse_leaf(leaf) else { continue };
                    let named = if log { Some(id) == current_log } else { self.manifest.segments.iter().any(|it| it.id == id) };
                    if named || self.manifest.retired.iter().any(|it| it.id == id && it.log == log) {
                        continue;
                    }
                    self.orphans.push((id, log));
                    if log {
                        continue;
                    }
                    let file = self.segment_file(id);
                    if meta.len < FOOTER as u64 {
                        self.lose_orphan(file, id, None);
                        continue;
                    }
                    effects.push(Effect::Read { file: file.share(), at: meta.len - FOOTER as u64, len: FOOTER, tag: self.tag(FOOTER_READ, self.listed.len() as u32) });
                    self.listed.push((file, id));
                    left += 1;
                }
                if left == 0 {
                    self.map_new(Then::Open, effects);
                } else {
                    self.phase = Phase::Orphans { left };
                }
                Ok(())
            }
            (FOOTER_READ, Phase::Orphans { left }) => {
                *left -= 1;
                let left = *left;
                let (ref file, id) = self.listed[index as usize];
                let file = file.share();
                let records = match &done {
                    Done::Read(bytes) => format::footer_count(bytes).map(u64::from),
                    _ => None,
                };
                self.lose_orphan(file, id, records);
                if left == 0 {
                    self.map_new(Then::Open, effects);
                }
                Ok(())
            }
            (MAP, Phase::Mapping { left, then }) => {
                *left -= 1;
                let (left, then) = (*left, *then);
                self.on_map(index, done)?;
                if left == 0 {
                    if self.unopened.is_empty() {
                        self.mapped(then, effects);
                    } else {
                        effects.push(Effect::List { dir: self.cfg.dir.share().into(), tag: self.tag(LIST, 0) });
                        self.phase = Phase::CheckMaps { then };
                    }
                }
                Ok(())
            }
            (LIST, Phase::CheckMaps { then }) => {
                let then = *then;
                let Done::Listed(listed) = done else {
                    return Err(fail("listing", &self.cfg.dir, &done).unwrap_or_else(|| unexpected("listing")));
                };
                let generation = self.manifest.generation;
                for (seg, file, why) in std::mem::replace(&mut self.unopened, Vec::new_in(self.alloc.clone())) {
                    let leaf = file.as_str().rsplit('/').next().unwrap_or(file.as_str());
                    if listed.iter().any(|(name, _)| name.as_str().rsplit('/').next() == Some(leaf)) {
                        // Here, and its map failed: the failure, by name, never a loss.
                        return Err(why);
                    }
                    let why = format!("generation {generation} names it, written in an earlier boot, and it is gone: a power cut before the kernel wrote its directory entry back; its records are dropped");
                    self.lose(seg, file, LostKind::Gone, why);
                }
                self.mapped(then, effects);
                Ok(())
            }
            (BARRIER, Phase::Sealing { left, ids }) => {
                let id = ids.get(index as usize).map(|it| it.0).ok_or_else(|| unexpected("sealing"))?;
                *left -= 1;
                let left = *left;
                if let Some(why) = fail("ordering a segment before the flip that seals it", &self.segment_file(id), &done) {
                    return Err(why);
                }
                if !matches!(done, Done::Ordered | Done::Synced) {
                    return Err(unexpected("sealing"));
                }
                if left == 0 {
                    let Phase::Sealing { ids, .. } = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!("sealing") };
                    self.start_flip(Change::Seal { ids }, effects);
                }
                Ok(())
            }
            (REMOVE, Phase::Removing { left, then }) => {
                *left -= 1;
                let (left, then) = (*left, *then);
                if let Some(why) = fail("unlinking a file retired past its grace period", &self.removing[index as usize], &done) {
                    return Err(why);
                }
                if left == 0 {
                    match then {
                        Then::Compact(stats) if self.sliced.is_some_and(|it| it.aborted) => {
                            let _ = stats;
                            self.phase = Phase::Ready;
                        }
                        then => self.map_new(then, effects),
                    }
                }
                Ok(())
            }
            (LIST, Phase::Listing) => {
                let Done::Listed(listed) = done else {
                    return Err(fail("listing", &self.cfg.dir, &done).unwrap_or_else(|| unexpected("listing")));
                };
                self.listed_compact(listed, now_ns, effects);
                Ok(())
            }
            (SEG_TRUNCATE, Phase::Writing(writing, _)) => {
                if let Some(why) = fail("making a segment", &writing.file, &done) {
                    return Err(why);
                }
                let pieces = std::mem::replace(&mut writing.pieces, Vec::new_in(self.alloc.clone()));
                writing.left = pieces.len() as u32;
                let file = writing.file.share();
                for (i, (at, bytes)) in pieces.into_iter().enumerate() {
                    effects.push(Effect::WriteAt { file: file.share(), at, bytes, tag: self.tag(SEG_PIECE, i as u32) });
                }
                // A streamed copy's first chunks, behind its header.
                self.stream_on(effects);
                Ok(())
            }
            (SEG_PIECE, Phase::Writing(writing, _)) => {
                if let Some(why) = fail("writing a segment", &writing.file, &done) {
                    return Err(why);
                }
                writing.left -= 1;
                if let Some(stream) = &mut writing.stream {
                    match index {
                        0 | FOOTER_PIECE => {}
                        index if index >= INDEX_PIECE => stream.index_flying = false,
                        _ => stream.flying -= 1,
                    }
                }
                // A streamed copy's next chunk, as one completes; its index chunks once every
                // payload chunk is asked, its footer after the last.
                self.stream_on(effects);
                let Phase::Writing(writing, _) = &self.phase else { unreachable!("streaming keeps the phase") };
                if writing.left > 0 {
                    return Ok(());
                }
                let Phase::Writing(writing, why) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                if writing.stream.is_some() {
                    // A copy, under its temporary name: named by its digest now.
                    self.named_copy(*writing, *why, effects);
                } else {
                    self.segment_done(*writing, *why, effects);
                }
                Ok(())
            }
            (SEG_RENAME, Phase::Renaming(writing, _)) => {
                if let Some(why) = fail("renaming a merge's copy to its digest's name", &writing.file, &done) {
                    return Err(why);
                }
                let Phase::Renaming(writing, why) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                self.segment_done(*writing, *why, effects);
                Ok(())
            }
            (SEG_UNWRITE, Phase::Unwriting { temp, .. }) => {
                if let Some(why) = fail("removing a merge's copy whose bytes are named already", temp, &done) {
                    return Err(why);
                }
                let Phase::Unwriting { writing, why, .. } = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                if let Why::Compact { candidates, stats } = &*why
                    && candidates.contains(&writing.seg.id)
                {
                    let evict = self.evict_for_compact.take();
                    let empty_orphans = self.alloc_vec();
                    let orphans = core::mem::replace(&mut self.orphans_for_compact, empty_orphans);
                    if evict.is_some() || !orphans.is_empty() {
                        let empty_ids = self.alloc_vec();
                        self.slice_ready(None, empty_ids, orphans, evict, Sliced { segments: 0, ..*stats }, effects);
                        return Ok(());
                    }
                    // The slice would write back what it read: nothing to do.
                    self.fold_asked = false;
                    self.sliced = Some(Sliced { segments: 0, ..*stats });
                    self.phase = Phase::Ready;
                    return Ok(());
                }
                // These bytes are here already, under this name: flipped without writing them.
                // They are a named segment's, proved as its row is (synced before the flip that
                // named them, when durable): nothing of them was written under it, so no sync
                // is owed.
                self.segment_written(Synced::already_named(*writing), *why, effects);
                Ok(())
            }
            (SEG_SYNC, Phase::SyncingSegment(writing, _)) => {
                if let Some(why) = fail("syncing a segment", &writing.file, &done) {
                    return Err(why);
                }
                let Phase::SyncingSegment(writing, why) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                self.segment_written(Synced::by_sync(*writing), *why, effects);
                Ok(())
            }
            (LOCK, Phase::Flipping(flip)) => match done {
                Done::Locked => {
                    self.holds_lock = true;
                    self.phase_ctx_read_head(effects);
                    Ok(())
                }
                Done::Contended => {
                    // This flip's earlier asks, then this one's step ([`lock_step`]).
                    let earlier = flip.waits;
                    flip.waits += 1;
                    let nonce = self.cfg.nonce;
                    if flip.waited_ns >= LOCK_WAIT_MOST_NS {
                        // Another process held it past the bound, and a flip holds it for
                        // milliseconds: it is stuck.
                        const _: () = assert!(LOCK_WAIT_MOST_NS == 60_000_000_000, "the text below says 60 s");
                        return Err(StoreError::Stalled { waited_for: "the flip lock, which another process held for 60 s" });
                    }
                    let step = self.cfg.lock_step.step_ns(nonce, earlier);
                    flip.waited_ns = flip.waited_ns.saturating_add(step);
                    self.lock_waited.asks = self.lock_waited.asks.saturating_add(1);
                    self.lock_waited.ns = self.lock_waited.ns.saturating_add(step);
                    effects.push(Effect::Timer { after: Duration::from_nanos(step), tag: self.tag(LOCK_WAIT, 0) });
                    Ok(())
                }
                done => Err(fail("taking the flip lock", &self.flip, &done).unwrap_or_else(|| unexpected("taking the flip lock"))),
            },
            (LOCK_WAIT, Phase::Flipping(_)) => {
                effects.push(Effect::Lock { file: self.flip.share(), mode: LockMode::Exclusive, tag: self.tag(LOCK, 0) });
                Ok(())
            }
            (LOG_TRUNCATE, Phase::Flipping(flip)) => {
                if let Some(why) = fail("making a manifest log", &flip.log, &done) {
                    return Err(why);
                }
                let bytes = std::mem::take(&mut flip.manifest_bytes);
                effects.push(Effect::WriteAt { file: flip.log.share(), at: 0, bytes, tag: self.tag(LOG_WRITE, 0) });
                Ok(())
            }
            (LOG_WRITE, Phase::Flipping(flip)) => {
                if let Some(why) = fail("writing a manifest", &flip.log, &done) {
                    return Err(why);
                }
                if self.cfg.durable {
                    effects.push(Effect::Sync { file: flip.log.share(), tag: self.tag(LOG_SYNC, 0) });
                } else {
                    self.write_slot(effects);
                }
                Ok(())
            }
            (LOG_SYNC, Phase::Flipping(flip)) => {
                if let Some(why) = fail("syncing a manifest log", &flip.log, &done) {
                    return Err(why);
                }
                if self.plant() == Plant::SkipDirSync {
                    self.write_slot(effects);
                } else {
                    // Durable: the names of the segments and the log made for this generation
                    // are made durable before the slot that names them.
                    effects.push(Effect::SyncDir { dir: self.cfg.dir.share().into(), tag: self.tag(DIR_SYNC, 0) });
                }
                Ok(())
            }
            (DIR_SYNC, Phase::Flipping(_)) => {
                if let Some(why) = fail("syncing the store's directory", &self.cfg.dir, &done) {
                    return Err(why);
                }
                self.write_slot(effects);
                Ok(())
            }
            (BLOOM_WRITE, Phase::Flipping(flip)) => {
                // `bloom` is a rebuildable acceleration sidecar: an I/O error writing or
                // appending to it never aborts the generation flip, but we clear `bloom_written`
                // so `BloomIndex::mapped_bytes` is only updated when the write actually succeeded.
                if matches!(done, Done::Failed(_) | Done::Broken { .. }) {
                    flip.bloom_written = None;
                }
                self.write_slot(effects);
                Ok(())
            }
            (SLOT_WRITE, Phase::Flipping(_)) => {
                // One write at a time, the slot last ([`Plain::write_slot`]): a failure here
                // stops the store with nothing of this flip in flight under the lock, and
                // before the slot, with nothing of it committed.
                let what = match index {
                    1 => "writing the cache directory tag beside the head",
                    2 => "blanking the head's other slot",
                    _ => "writing the head",
                };
                let file = if index == 1 { self.name(CACHEDIR_TAG) } else { self.head.share() };
                if let Some(why) = fail(what, &file, &done) {
                    return Err(why);
                }
                match index {
                    1 => {
                        self.blank_other_slot(effects);
                        return Ok(());
                    }
                    2 => {
                        self.write_the_slot(effects);
                        return Ok(());
                    }
                    _ => {}
                }
                if self.cfg.durable {
                    effects.push(Effect::Sync { file: self.head.share(), tag: self.tag(HEAD_SYNC, 0) });
                } else {
                    self.unlock(effects);
                }
                Ok(())
            }
            (HEAD_SYNC, Phase::Flipping(_)) => {
                if let Some(why) = fail("syncing the head", &self.head, &done) {
                    return Err(why);
                }
                self.unlock(effects);
                Ok(())
            }
            (UNLOCK, Phase::Flipping(_)) => {
                if let Some(why) = fail("letting go of the flip lock", &self.flip, &done) {
                    return Err(why);
                }
                self.flipped(effects);
                Ok(())
            }
            _ => Err(unexpected(self.what())),
        }
    }

    /// The flip lock is held: the head is read again, under it.
    fn phase_ctx_read_head(&mut self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
        self.flip_parked = Some(flip);
        self.phase = Phase::ReadHead(Ctx::Flip);
        effects.push(Effect::ReadAll { file: self.head.share(), tag: self.tag(HEAD_READ, 0) });
    }

    fn read_manifest(&mut self, ctx: Ctx, slots: [Option<Slot>; 2], at: usize, fell_back: bool, effects: &mut Vec<Effect>) {
        let slot = slots[at].expect("a slot to read");
        effects.push(Effect::Read { file: self.log_file(slot.log), at: slot.manifest_at, len: slot.manifest_len as usize, tag: self.tag(MANIFEST_READ, 0) });
        self.phase = Phase::ReadManifest { ctx, slots, at, fell_back };
    }

    /// `slots[at]`'s manifest is not whole (torn, cut short, or its log gone): said by name at
    /// an open, then the older slot's generation is read, or, with none, nothing survived.
    fn manifest_torn(&mut self, ctx: Ctx, slots: [Option<Slot>; 2], at: usize, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        let slot = slots[at].expect("read");
        if ctx == Ctx::Open {
            // Reported once: the next flip writes a generation past it, whose manifest is whole,
            // so no later open reads this one again.
            self.torn_stamp = true;
            self.report.lost.push(Lost {
                file: self.log_file(slot.log),
                segment: None,
                records: None,
                why: format!("generation {}'s manifest at {} is not whole: a power cut before the kernel wrote it back; the generation before is read", slot.generation, slot.manifest_at),
            });
        }
        if at == 0 && slots[1].is_some() {
            self.read_manifest(ctx, slots, 1, true, effects);
            return Ok(());
        }
        // Neither generation survived: everything is lost, said by name below, and the next
        // generation follows the newest number so its slot is right.
        let newest = slots[0].expect("read");
        let manifest = Manifest { generation: newest.generation, ..Manifest::default() };
        // The newest slot's number is followed; its log, which gave nothing back, is not.
        self.log_lost = true;
        self.resolved(ctx, Base { slot: Some(newest), manifest }, false, true, now_ns, effects)
    }

    /// A generation to read (at the open) or to build on (under the lock).
    fn resolved(&mut self, ctx: Ctx, base: Base, fresh: bool, fell_back: bool, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        match ctx {
            Ctx::Open => {
                if self.bloom_loading {
                    self.open_parked = Some(OpenParked { base, fell_back });
                    return Ok(());
                }
                self.slot = base.slot;
                self.manifest = base.manifest;
                self.invalidate_reach();
                self.opened_ns = now_ns;
                self.opened_generation = self.manifest.generation;
                self.bloom.index_manifest(&self.manifest.segments);
                // Rows retired as lost and not yet reported to a caller who got the report
                // (whoever wrote them: this open's process before a crash, or another): this
                // open reports them, and clears them only after its own report is handed.
                for row in self.manifest.retired.iter().filter(|it| !it.log) {
                    let Some(mark) = row.lost else { continue };
                    self.report.lost.push(Lost { file: self.segment_file(row.id), segment: Some(row.id), records: mark.records.map(u64::from), why: mark.why() });
                    if !self.unreported.contains(&row.id) {
                        self.unreported.push(row.id);
                    }
                }
                if fell_back {
                    self.phase = Phase::Orphans { left: 0 };
                    effects.push(Effect::List { dir: self.cfg.dir.share().into(), tag: self.tag(LIST, 0) });
                } else {
                    self.map_new(Then::Open, effects);
                }
            }
            Ctx::Flip => {
                let flip = self.flip_parked.take().expect("a flip's head read");
                self.phase = Phase::Flipping(flip);
                self.build(base, fresh, now_ns, effects);
            }
        }
        Ok(())
    }

    /// A segment is written (and synced, when durable): a commit's next is written, or, with
    /// none left, the flip names them all.
    fn segment_written(&mut self, synced: Synced<A>, why: Why<A>, effects: &mut Vec<Effect>) {
        let writing = synced.into_writing();
        let seg = (writing.seg, writing.file);
        match why {
            Why::Commit { started_ns, roots, mut rest, mut done } => {
                done.push(seg);
                if !rest.is_empty() {
                    let next = rest.remove(0);
                    self.write_segment(next, Why::Commit { started_ns, roots, rest, done }, effects);
                    return;
                }
                self.start_flip(Change::Commit { segs: done, started_ns, roots }, effects);
            }
            Why::Compact { candidates, stats } => {
                let empty_orphans = self.alloc_vec();
                let orphans = core::mem::replace(&mut self.orphans_for_compact, empty_orphans);
                let evict = self.evict_for_compact.take();
                self.slice_ready(Some(seg), candidates, orphans, evict, stats, effects);
            }
        }
    }
}

/// A batch of copied records (a carry's, bounded by the records its commit's roots reuse; a
/// slice's copy is streamed instead, [`Plain::stream_segment`]: each payload at a multiple of [`ALIGN`]
/// of its bytes) as a segment's layout: its entries placed after the [`HEADER`] and sorted.
/// The list of pieces in `alloc`, the store's.
fn laid_of<A: Allocator + Clone>(mut out: Batch, alloc: A) -> Result<Laid<A>, StoreError> {
    let namespace = out.namespace;
    let mut entries = std::mem::take(&mut out.entries);
    for entry in entries.iter_mut() {
        entry.at += HEADER as u64;
    }
    let run = format::sorted_run(entries)?;
    let index_at = HEADER as u64 + out.bytes.len() as u64;
    let mut pieces = Vec::with_capacity_in(1, alloc);
    pieces.push((HEADER as u64, out.bytes));
    Ok(Laid { pieces, index_at, run, namespace })
}

/// `keys` sorted and each once, as the 32-byte array a root's or an index's record holds: in
/// `alloc`, scratch of the one put that copies it into its batch.
fn sorted_keys<A: Allocator>(keys: &[Key], alloc: A) -> Vec<u8, A> {
    let mut bytes = Vec::with_capacity_in(keys.len() * 32, alloc);
    for key in keys {
        bytes.extend_from_slice(&key.0.0);
    }
    let (chunks, _) = bytes.as_chunks_mut::<32>();
    chunks.sort_unstable();
    let mut kept = 0usize;
    for i in 0..chunks.len() {
        let Some(&cur) = chunks.get(i) else { break };
        let dup = kept > 0 && chunks.get(kept - 1).is_some_and(|prev| *prev == cur);
        if !dup {
            if let Some(dst) = chunks.get_mut(kept) {
                *dst = cur;
                kept += 1;
            }
        }
    }
    bytes.truncate(kept * 32);
    bytes
}

fn lost_orphan() -> String {
    LostMark { kind: LostKind::Orphan, records: None }.why()
}

/// Formats `id` as 16 lowercase hex ASCII bytes into `dst` with zero allocations.
fn write_hex_u64(id: u64, dst: &mut [u8; 16]) {
    const fn hex(n: u8) -> u8 {
        if n < 10 { b'0' + n } else { b'a' + (n - 10) }
    }
    for (chunk, byte) in dst.chunks_exact_mut(2).zip(id.to_be_bytes()) {
        if let [hi, lo] = chunk {
            *hi = hex(byte >> 4);
            *lo = hex(byte & 0x0f);
        }
    }
}

/// Content fingerprint of `roots` for [`ReachCache`] validity checks.
fn hash_roots(roots: &[Root]) -> ContentId {
    let mut h = blaze_ids::Hasher::new("blaze plain reach roots 1");
    h.update(&u64::try_from(roots.len()).unwrap_or(u64::MAX).to_le_bytes());
    for root in roots {
        h.update(&u64::try_from(root.name.len()).unwrap_or(u64::MAX).to_le_bytes());
        h.update(root.name.as_bytes());
        h.update(&root.key.0);
        h.update(&root.generation.to_le_bytes());
        h.update(&root.expires_ns.to_le_bytes());
    }
    h.finish()
}

/// Content fingerprint of `leases` for [`ReachCache`] validity checks.
fn hash_leases(leases: &[Lease]) -> ContentId {
    let mut h = blaze_ids::Hasher::new("blaze plain reach leases 1");
    h.update(&u64::try_from(leases.len()).unwrap_or(u64::MAX).to_le_bytes());
    for lease in leases {
        h.update(&lease.holder.to_le_bytes());
        h.update(&lease.since.to_le_bytes());
        h.update(&lease.expires_ns.to_le_bytes());
    }
    h.finish()
}
