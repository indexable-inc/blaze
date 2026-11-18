//! The plain store's state machine ([`super`] has the layout and the costs): opening (the
//! head and the manifest; a segment mapped at its first probe, an unsealed segment of another
//! boot checked whole at its first hit: `probe.rs`),
//! reads in place with no lock and no shared write, puts into per-worker [`Batch`]es, the
//! commit (one segment written without a lock, then the flip under the one lock), and
//! compaction in bounded slices. It owns no IO: it hands back [`Effect`]s and takes their
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
use blaze_ids::{ContentId, crc32c};
use blaze_keyed::{
    kind::{Namespace, Tag},
    store::{ALIGN, ENTRY, FOOTER, HEADER, Holder, Put, SegmentId, StoreError, StoreFile, TwoValues},
};

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

/// The tag of the store's own root records (a root's sorted keys), always under
/// [`Namespace::LOCAL`]: reserved, no kind of blaze-keyed's takes it.
pub const ROOT_TAG: Tag = Tag(u16::MAX);

/// The tag of a POINTER: a record whose key is a name and whose payload is the 32-byte key of
/// another record, its target ([`super::Lent::point`]). The one record a newer commit may
/// REPLACE: two values under one name are the old and the new, never refused, and the value
/// in the newest row of the manifest holding the name is the pointer's
/// ([`Plain::find_newest`]). Every other tag keeps the two-values refusal (rule 21). A pointer
/// lives in [`Namespace::LOCAL`] only; compaction keeps only its newest value. Reserved: apart
/// from the roots' ([`ROOT_TAG`]), the startup image's (`u16::MAX - 1`), the drive's records
/// (blaze-cli `DRIVE_TAG`, `u16::MAX - 2`) and code entries (blaze-cli `CODE_TAG`,
/// `u16::MAX - 3`).
pub const POINTER_TAG: Tag = Tag(u16::MAX - 4);

/// A pointer's payload: its target's key.
pub const POINTER_LEN: usize = 32;

/// The tag of an INDEX ([`Plain::index`]): a record of sorted 32-byte keys under
/// [`Namespace::LOCAL`], keyed by its bytes. It keeps nothing by itself; reached through an
/// indexed root ([`INDEXED_ROOT_TAG`]), it and every key it holds are live. One level: an index
/// naming an index keeps the inner index's record, not what that holds. Reserved, after the
/// pointers' ([`POINTER_TAG`]) and blaze-cli's start lists (`START_LIST_TAG`, `u16::MAX - 5`).
pub const INDEX_TAG: Tag = Tag(u16::MAX - 6);

/// The tag of an indexed root's record ([`Plain::root_through`]): sorted keys, each an index
/// ([`INDEX_TAG`]). A slice reads a root's record anyway; its tag says whether to follow it.
pub const INDEXED_ROOT_TAG: Tag = Tag(u16::MAX - 7);

/// The most keys one index holds (128 MiB of keys): a program reaching more is refused by
/// name ([`StoreError::IndexOver`]), never cut.
pub const INDEX_KEYS_MOST: usize = 1 << 22;

/// The most indexes one indexed root names.
pub const ROOT_INDEXES_MOST: usize = 1 << 16;

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

/// Compaction merges the oldest segments while a generation names more than this many: the
/// bound on index probes a miss costs.
pub const SEGMENTS_MOST: usize = 16;

/// What one compaction slice reads and writes at most, unless one segment alone is larger
/// (then that segment is the slice).
pub const SLICE_BYTES: u64 = 64 << 20;

/// A manifest log takes no more manifests past this; the next flip starts another.
#[cfg(not(test))]
const LOG_MOST: u64 = 4 << 20;
#[cfg(test)]
const LOG_MOST: u64 = 16 << 10;

/// How long a lock wait sleeps between asks: [`Effect::Lock`] never waits itself.
const LOCK_STEP: Duration = Duration::from_millis(2);

/// How many steps a lock wait takes before it stops the store, naming the lock: 60 s.
const LOCK_STEPS_MOST: u32 = 30_000;

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
}

// By hand: a derive would ask the allocator for `PartialEq` and `Debug`, which a handle lacks.
impl<A: Allocator + Clone> Clone for PlainConfig<A> {
    fn clone(&self) -> PlainConfig<A> {
        PlainConfig {
            dir: self.dir.clone(),
            tags: self.tags,
            cap_bytes: self.cap_bytes,
            boot: self.boot,
            nonce: self.nonce,
            durable: self.durable,
            machine: self.machine.clone(),
            writes: self.writes.clone(),
            reads: self.reads.clone(),
            liveness: self.liveness,
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
    /// Another process changed the candidates first: nothing flipped.
    pub aborted: bool,
    /// There is more to do: another slice would do it.
    pub more: bool,
}

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
/// The index the planted [`Plant::UnlinkEarly`]'s unlinks start at, apart from a flip's own.
const EARLY_REMOVE: u32 = 1 << 30;

/// What a commit changes of what keeps records: roots, and this process's lease.
enum RootEdit {
    /// `name` names `key`, for `life_ns` from the flip ([`NEVER`]: until unset).
    Set { name: String, key: Key, life_ns: u64 },
    Unset(String),
    /// This process's lease set or renewed: segments of generation `since` on are kept whole
    /// until [`LEASE_NS`] from the flip.
    Lease { since: u64, opened_ns: u64 },
    /// This process's lease let go of: its records are rooted by now.
    Release,
}

/// A compaction slice written with no lock and flipped by the command's next commit
/// ([`Plain::fold_slice`]): its copy (none when the slice only drops), the segments it
/// replaces, the orphans it retires.
struct Folded<A: Allocator + Clone> {
    seg: Option<(Segment, FileName)>,
    candidates: Vec<u64, A>,
    orphans: Vec<(u64, bool), A>,
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

/// A segment being written, before its flip.
struct Writing<A: Allocator + Clone> {
    seg: Segment,
    file: FileName,
    /// The writes at offsets, laid out by [`Plain::commit`], each moved into its effect.
    pieces: Vec<(u64, Vec<u8>), A>,
    left: u32,
}

/// What a flip adds to the generation it builds on.
enum Change<A: Allocator + Clone> {
    Commit { segs: Vec<(Segment, FileName), A>, started_ns: u64, roots: Vec<RootEdit, A> },
    /// Only what an open found (segments checked or lost): [`Plain::pending_stamps`].
    Stamp,
    Compact { seg: Option<(Segment, FileName)>, candidates: Vec<u64, A>, orphans: Vec<(u64, bool), A>, stats: Sliced },
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
    head: FileName,
    flip: FileName,
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
    by_id: BTreeMap<u64, usize>,
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
    /// The rounds the commit in flight carried records forward: at most [`CARRY_ROUNDS`].
    carry_rounds: u32,
    /// Records carried forward by this process's commits, ever ([`Plain::carried`]).
    carried: u64,
    /// The indexes this process made, with how many keys each holds: what an indexed root
    /// that names one counts ([`Plain::named`]).
    indexes_made: Vec<(Key, u64), A>,
    /// The roots this process set and the keys they name ([`Plain::named`]).
    named: Named,
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

    /// The store `cfg` names, what it keeps in `alloc` (the caller's: rule 75).
    pub fn new(cfg: PlainConfig<A>, alloc: A) -> Plain<A> {
        assert!(cfg.writes.contains(&Namespace::LOCAL), "a plain store writes the local namespace: its roots live there");
        let name = |leaf: &str| FileName::new(&format!("{}/{leaf}", cfg.dir.as_str())).expect("a store's directory names files under it");
        let mut held = Vec::with_capacity_in(cfg.writes.len(), alloc.clone());
        held.extend(cfg.writes.iter().map(|it| Held::new(*it)));
        Plain {
            head: name(HEAD),
            flip: name(FLIP),
            boot: Boot(cfg.boot),
            checks: std::cell::Cell::new(Checks::default()),
            checked_ids: Vec::new_in(alloc.clone()),
            sealed_ids: Vec::new_in(alloc.clone()),
            scrubbing: None,
            held,
            sealed: Vec::new_in(alloc.clone()),
            committing: Vec::new_in(alloc.clone()),
            cfg,
            #[cfg(test)]
            plant: Plant::None,
            phase: Phase::New,
            slot: None,
            manifest: Manifest::default(),
            maps: Vec::new_in(alloc.clone()),
            by_id: BTreeMap::new(),
            view: Vec::new_in(alloc.clone()),
            unchecked: BTreeMap::new(),
            eager: false,
            opened_ns: 0,
            probes: Probes::default(),
            roots_pending: Vec::new_in(alloc.clone()),
            checked: BTreeSet::new(),
            dropped: BTreeSet::new(),
            orphans: Vec::new_in(alloc.clone()),
            marks: Vec::new_in(alloc.clone()),
            unreported: Vec::new_in(alloc.clone()),
            clearable: Vec::new_in(alloc.clone()),
            report: Report::default(),
            committed: None,
            sliced: None,
            broken: None,
            removing: Vec::new_in(alloc.clone()),
            seq: 0,
            listed: Vec::new_in(alloc.clone()),
            flip_parked: None,
            orphans_for_compact: Vec::new_in(alloc.clone()),
            late_sync: Vec::new_in(alloc.clone()),
            unopened: Vec::new_in(alloc.clone()),
            head_made: false,
            log_lost: false,
            holds_lock: false,
            head_torn: None,
            torn_stamp: false,
            opened_generation: 0,
            now_ns: 0,
            leased: false,
            released: false,
            folding: None,
            fold_asked: false,
            touches: Vec::new_in(alloc.clone()),
            carrying: Vec::new_in(alloc.clone()),
            carry_rounds: 0,
            carried: 0,
            indexes_made: Vec::new_in(alloc.clone()),
            named: Named::default(),
            alloc,
        }
    }

    /// The allocator the caller handed this store: what it keeps, and what its lists are
    /// answered in.
    pub fn alloc(&self) -> &A {
        &self.alloc
    }

    fn name(&self, leaf: &str) -> FileName {
        FileName::new(&format!("{}/{leaf}", self.cfg.dir.as_str())).expect("a store's directory names files under it")
    }

    fn segment_file(&self, id: u64) -> FileName {
        self.name(&format!("seg-{id:016x}"))
    }

    fn log_file(&self, id: u64) -> FileName {
        self.name(&format!("manifests-{id:016x}"))
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

    /// Compaction prunes while the segments hold more than `cap_bytes`.
    pub fn set_cap(&mut self, cap_bytes: u64) {
        self.cfg.cap_bytes = cap_bytes;
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
        effects.push(Effect::ReadAll { file: self.head.clone(), tag: self.tag(HEAD_READ, 0) });
    }

    // ---- reads: no lock, no write to anything shared ----

    /// Whether a lookup that names `spaces` may answer from a segment of `namespace`: only
    /// when it names it (the planted bug answers from any).
    fn asks(&self, spaces: &[Namespace], namespace: Namespace) -> bool {
        self.plant() == Plant::IgnoresNamespace || spaces.contains(&namespace)
    }

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
            return self.view.iter().filter_map(|&row| self.mapped_row(row)).find_map(|at| self.probe(at, key));
        }
        spaces.iter().find_map(|space| {
            self.view.iter().filter(|&&row| self.manifest.segments[row].namespace == *space).filter_map(|&row| self.mapped_row(row)).find_map(|at| self.probe(at, key))
        })
    }

    fn probe(&self, at: usize, key: &Key) -> Option<(usize, Entry)> {
        let run = self.maps[at].run();
        format::find(run, &key.0).map(|i| (at, Entry::read(&run[i * ENTRY..(i + 1) * ENTRY])))
    }

    fn find_committed(&self, key: &Key) -> Option<(usize, Entry)> {
        self.find_in(&self.cfg.reads, key)
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
            Placed { key: *key, tag: entry.tag, namespace: mapped.seg().namespace, segment: mapped.seg().digest, file: mapped.file().clone(), at: entry.at, len: entry.len }
        })
    }

    pub fn locate(&self, key: &Key) -> Option<Placed> {
        self.locate_in(&self.cfg.reads, key)
    }

    /// Where `key` is in `spaces`: its proved segment's index in this process's maps and its
    /// entry, for the store handle's lock-free read.
    pub(crate) fn find_mapped(&self, spaces: &[Namespace], key: &Key) -> Option<(usize, Entry)> {
        self.find_in(spaces, key)
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

    /// Names `keys` as kept under `name` at the next commit: a [`ROOT_TAG`] record of the
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
        let bytes = sorted_keys(keys, self.alloc.clone());
        let key = self.set_root(name, &bytes, life_ns, ROOT_TAG, ROOT_DOMAIN)?;
        self.named.roots += 1;
        self.named.keys += (bytes.len() / 32) as u64;
        Ok(key)
    }

    /// The roots this process set so far and the keys they name (an index counted by its keys
    /// when this process made it, else as one key): the command's cleanup line.
    pub fn named(&self) -> Named {
        self.named
    }

    /// An INDEX of `keys` ([`INDEX_TAG`]): one record of their sorted 32-byte keys under
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
        self.put(key, Namespace::LOCAL, INDEX_TAG, &bytes).map_err(|it| it.into_store(INDEX_TAG))?;
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
        self.put(key, Namespace::LOCAL, INDEX_TAG, &bytes).map_err(|it| it.into_store(INDEX_TAG))?;
        if !self.indexes_made.iter().any(|(it, _)| *it == key) {
            self.indexes_made.push((key, count));
        }
        Ok(key)
    }

    /// [`Plain::root_for`] of INDEXES: the root `name` names the index records `indexes` (each
    /// made by [`Plain::index`], in this process or before), kept for `life_ns`; reach keeps the
    /// root's record, each index and every key each index holds. Its record is filed under
    /// [`INDEXED_ROOT_TAG`], so a slice tells it from a plain root by the entry it already
    /// reads, with no probe of the keys it names. Over [`ROOT_INDEXES_MOST`] indexes is refused
    /// by name.
    pub fn root_through(&mut self, name: &str, indexes: &[Key], life_ns: u64) -> Result<Key, StoreError> {
        let bytes = sorted_keys(indexes, self.alloc.clone());
        let count = (bytes.len() / 32) as u64;
        if count > ROOT_INDEXES_MOST as u64 {
            return Err(StoreError::IndexOver { keys: count, most: ROOT_INDEXES_MOST as u64 });
        }
        let key = self.set_root(name, &bytes, life_ns, INDEXED_ROOT_TAG, INDEXED_ROOT_DOMAIN)?;
        self.named.roots += 1;
        for index in bytes.chunks_exact(32) {
            let index = Key(ContentId(index.try_into().expect("32 bytes")));
            let held = self.indexes_made.iter().find(|(it, _)| *it == index).map_or(1, |(_, n)| *n);
            self.named.keys += held;
            self.named.indexed += held;
        }
        Ok(key)
    }

    /// The root `name` set to the record `bytes` (sorted keys) under `tag`, its key derived
    /// under `domain`: the one body of [`Plain::root_for`] and [`Plain::root_through`].
    fn set_root(&mut self, name: &str, bytes: &[u8], life_ns: u64, tag: Tag, domain: &'static str) -> Result<Key, StoreError> {
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
        self.roots_pending.push(RootEdit::Set { name: name.to_owned(), key, life_ns });
        Ok(key)
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

    // ---- the commit ----

    /// Writes the store's batches and `batches` (the workers', borrowed: their bytes are copied
    /// into the layout here, so a worker's batch stays readable by its worker until the flip
    /// lands) as one new segment for each namespace that holds records, one after another,
    /// with no lock, then flips the head once to a generation naming them all. The outcome is
    /// [`Plain::take_committed`]'s once the store is ready again.
    pub fn commit(&mut self, batches: &[&Batch], now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        // The layout refuses two values under one key against every committed row: a whole
        // store (rule 21). A commit with nothing to lay out (stamps alone) needs none.
        assert!(
            self.eager || (!self.holds_uncommitted() && batches.iter().all(|it| it.is_empty())),
            "Plain::commit lays out against a whole store: Plain::map_all first"
        );
        if let Some(batch) = batches.iter().find(|it| !self.cfg.writes.contains(&it.namespace)) {
            return Err(StoreError::NotWritable { namespace: batch.namespace });
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
        // The held batches stay readable while they are written: sealed until the commit ends.
        let mut fresh = Vec::with_capacity_in(self.cfg.writes.len(), self.alloc.clone());
        fresh.extend(self.cfg.writes.iter().map(|it| Held::new(*it)));
        self.sealed = std::mem::replace(&mut self.held, fresh);
        let laid = self.lay_out(batches);
        let laid = match laid {
            Ok(laid) => laid,
            Err(why) => {
                self.sealed_back();
                return Err(why);
            }
        };
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
            None if roots.is_empty() && !self.pending_stamps() && self.folding.is_none() => {
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segments: Vec::new_in(self.alloc.clone()) }));
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
    fn lay_out(&self, batches: &[&Batch]) -> Result<Vec<Laid<A>, A>, StoreError> {
        let mut out = Vec::with_capacity_in(self.cfg.writes.len(), self.alloc.clone());
        for &namespace in self.cfg.writes.iter() {
            let own = self.sealed.iter().find(|it| it.batch.namespace == namespace).map(|it| &it.batch);
            // Every record of the namespace, the store's own batch's and each worker's: in the
            // store's allocator, freed when the commit's layout is made.
            let mut all: Vec<(Entry, &[u8]), A> = Vec::new_in(self.alloc.clone());
            for batch in own.into_iter().chain(batches.iter().copied().filter(|it| it.namespace == namespace)) {
                all.extend(batch.entries.iter().map(|it| (*it, batch.payload(it))));
            }
            all.sort_unstable_by(|a, b| a.0.key.cmp(&b.0.key));
            let mut entries: Vec<Entry, A> = Vec::with_capacity_in(all.len(), self.alloc.clone());
            // The payloads from [`HEADER`] on, each at a multiple of [`ALIGN`] of the segment:
            // the process heap, as the segment's write takes it whole (blaze-core's effect).
            let mut bytes: Vec<u8> = Vec::new_in(Global);
            let mut last: Option<(Entry, &[u8])> = None;
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
                if entry.tag == POINTER_TAG {
                    // A pointer replaces, so it is always laid out, even when this process's view
                    // holds its value: that view may be older than another process's commit of
                    // another value, and this commit's row, after every row the generation it
                    // flips on names, must make this value the newest. A key held under another
                    // tag is two values (rule 21).
                    if let Some((mapped, kept)) = self.newest_mapped(namespace, &Key(entry.key))
                        && kept.tag != POINTER_TAG
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
                let at = HEADER as u64 + bytes.len() as u64;
                let end = at + u64::from(entry.len);
                if end > OFFSET_MOST {
                    return Err(StoreError::TooLong { tag: entry.tag, key: entry.key, len: end });
                }
                bytes.extend_from_slice(payload);
                let padded = if self.plant() == Plant::Unpadded { end } else { aligned(end) };
                bytes.resize((padded - HEADER as u64) as usize, 0);
                let mut placed = entry;
                placed.at = at;
                if self.plant() == Plant::IndexOffByOne {
                    placed.at += ALIGN;
                }
                entries.push(placed);
            }
            if entries.is_empty() {
                continue;
            }
            let index_at = HEADER as u64 + bytes.len() as u64;
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
            Phase::Writing(..) | Phase::SyncingSegment(..) => "writing a segment",
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
        let Laid { mut pieces, index_at, run, namespace } = laid;
        debug_assert!(pieces.iter().scan(HEADER as u64, |at, (piece_at, bytes)| {
            let ok = *piece_at == *at;
            *at += bytes.len() as u64;
            Some(ok)
        }).all(|it| it), "pieces lie end to end");
        let sealed = format::seal_parts(namespace, &run, index_at, pieces.iter().map(|(_, bytes)| bytes.as_slice()));
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
        if self.by_id.contains_key(&seg.id) {
            // These bytes are here already, under this name: flip without writing them.
            self.segment_written(Writing { seg, file, pieces: Vec::new_in(self.alloc.clone()), left: 0 }, why, effects);
            return;
        }
        let mut header = Vec::with_capacity_in(HEADER, Global);
        header.extend_from_slice(&sealed.header);
        let mut footer = Vec::with_capacity_in(FOOTER, Global);
        footer.extend_from_slice(&sealed.footer);
        pieces.insert(0, (0, header));
        pieces.push((index_at, run));
        pieces.push((index_at + u64::from(count) * ENTRY as u64, footer));
        effects.push(Effect::Truncate { file: file.clone(), len, tag: self.tag(SEG_TRUNCATE, 0) });
        self.phase = Phase::Writing(Box::new(Writing { seg, file, pieces, left: 0 }), Box::new(why));
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
    /// then at most [`SLICE_BYTES`] of the oldest segments copied (what is live of them) into
    /// one new segment, with no lock, and a flip that names it in their place. Returns whether
    /// it started; the outcome is [`Plain::take_sliced`]'s once the store is ready again.
    pub fn compact(&mut self, effects: &mut Vec<Effect>) -> bool {
        if !self.ready() {
            return false;
        }
        assert!(self.eager, "Plain::compact copies from a whole store: Plain::map_all first");
        self.phase = Phase::Listing;
        effects.push(Effect::List { dir: self.cfg.dir.clone().into(), tag: self.tag(LIST, 0) });
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

    /// Which segments a slice replaces, and whether it prunes.
    fn plan(&self) -> (Vec<Segment, A>, bool) {
        let current = self.manifest.generation;
        let over = self.manifest.bytes() > self.cfg.cap_bytes;
        let local = self.manifest.segments.iter().filter(|it| it.local).count();
        let wanted = if over {
            1
        } else if local > SEGMENTS_MOST {
            2
        } else {
            return (Vec::new_in(self.alloc.clone()), false);
        };
        // Pruning reads published segments too (it lets go of this machine's copy, rewriting
        // nothing); merging never does. One namespace a slice: no segment mixes two.
        let eligible = |it: &&Segment| it.local && (if over { it.generation + WINDOW <= current } else { !it.published });
        let Some(namespace) = self.manifest.segments.iter().find(eligible).map(|it| it.namespace) else {
            return (Vec::new_in(self.alloc.clone()), false);
        };
        let mut out = Vec::new_in(self.alloc.clone());
        let mut bytes = 0u64;
        // A CONTIGUOUS run of the namespace's rows held here: the copy takes the first one's
        // place, so a row the slice skipped would move from before a pointer's newest value
        // to after it, and its older value would read as the newest.
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
        if out.len() < wanted {
            out.clear();
        }
        (out, over)
    }

    /// Which segments a [`Liveness::Reached`] slice replaces: a contiguous run of one
    /// namespace's rows, from the first found under half live (over the cap: holding any dead
    /// record) among at most [`SWEEP_BYTES`] of rows looked at from a place that moves with the
    /// generation, up to [`SLICE_BYTES`]; else, while more than [`SEGMENTS_MOST`] are named, a
    /// merge of two or more from the oldest. A row a live lease keeps, a published row and one
    /// not here are never in a run (a run stops at one).
    fn plan_reached(&self, now_ns: u64) -> Result<Vec<Segment, A>, StoreError> {
        let roots = self.live_roots(now_ns)?;
        let over = self.manifest.bytes() > self.cfg.cap_bytes;
        let segs = &self.manifest.segments;
        let eligible = |it: &Segment| it.local && !it.published && self.by_id.contains_key(&it.id) && !self.leased(it, now_ns);
        let mut sparse = None;
        if !segs.is_empty() {
            let start = (self.manifest.generation % segs.len() as u64) as usize;
            let mut looked = 0u64;
            for k in 0..segs.len() {
                let i = (start + k) % segs.len();
                if !eligible(&segs[i]) {
                    continue;
                }
                if looked >= SWEEP_BYTES {
                    break;
                }
                looked += segs[i].file_len();
                let (live, all) = self.live_of(&segs[i], &roots);
                if live * 2 < segs[i].file_len() || (over && live < all) {
                    sparse = Some(i);
                    break;
                }
            }
        }
        let from = match sparse {
            Some(i) => i,
            None if segs.iter().filter(|it| it.local).count() > SEGMENTS_MOST => match segs.iter().position(|it| eligible(it)) {
                Some(i) => i,
                None => return Ok(Vec::new_in(self.alloc.clone())),
            },
            None => return Ok(Vec::new_in(self.alloc.clone())),
        };
        let namespace = segs[from].namespace;
        let mut out = Vec::new_in(self.alloc.clone());
        let mut bytes = 0u64;
        // Contiguous, as [`Plain::plan`]'s: the copy takes the first one's place.
        for seg in segs[from..].iter().filter(|it| it.local && it.namespace == namespace) {
            if !eligible(seg) || (!out.is_empty() && bytes + seg.file_len() > SLICE_BYTES) {
                break;
            }
            bytes += seg.file_len();
            out.push(*seg);
        }
        if sparse.is_none() && out.len() < 2 {
            out.clear();
        }
        Ok(out)
    }

    /// A live lease keeps `seg` whole: it is of the lease's generation or newer.
    fn leased(&self, seg: &Segment, now_ns: u64) -> bool {
        self.manifest.leases.iter().any(|it| seg.generation >= it.since && (it.expires_ns > now_ns || self.plant() == Plant::LeaseNeverExpires))
    }

    /// Each unexpired root's key and its record's sorted keys, read in place: what
    /// [`reached`] searches. A root whose record no segment holds stops the slice by name.
    fn live_roots(&self, now_ns: u64) -> Result<Vec<(ContentId, &[u8]), A>, StoreError> {
        let mut out = Vec::with_capacity_in(self.manifest.roots.len(), self.alloc.clone());
        for (i, root) in self.manifest.roots.iter().enumerate() {
            if root.expires_ns <= now_ns || (i == 0 && self.plant() == Plant::ReachSkipsRoot) {
                continue;
            }
            let Some((at, entry)) = self.find_in(&[Namespace::LOCAL], &Key(root.key)) else {
                return Err(StoreError::RootGone { generation: self.manifest.generation, key: root.key });
            };
            let named = self.maps[at].payload(&entry);
            out.push((root.key, named));
            if entry.tag == INDEXED_ROOT_TAG && self.plant() != Plant::IndexNotFollowed {
                self.follow(root.key, named, &mut out)?;
            }
        }
        Ok(out)
    }

    /// Each index the indexed root `root` names (`named`, its record's sorted keys), pushed as
    /// `(its key, its sorted keys)`: what [`reached`] searches as it searches a root's, so the
    /// index's own record and every key it holds are live. One level: what an index names is
    /// not looked into. An index no segment holds, or a record there of another tag, stops the
    /// slice by name, as a root's missing record does.
    fn follow<'s>(&'s self, root: ContentId, named: &[u8], out: &mut Vec<(ContentId, &'s [u8]), A>) -> Result<(), StoreError> {
        for index in named.chunks_exact(32) {
            let index = ContentId(index.try_into().expect("32 bytes"));
            let Some((at, entry)) = self.find_in(&[Namespace::LOCAL], &Key(index)) else {
                return Err(StoreError::IndexGone { generation: self.manifest.generation, root, index });
            };
            if entry.tag != INDEX_TAG {
                return Err(StoreError::NotAnIndex { root, key: index, tag: entry.tag });
            }
            out.push((index, self.maps[at].payload(&entry)));
        }
        Ok(())
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
                RootEdit::Set { key, .. } => Some(*key),
                _ => None,
            }));
            it
        };
        if sets.is_empty() {
            return Ok(out);
        }
        let live = self.live_roots(now_ns)?;
        let look = |key: &ContentId, out: &mut Vec<(u64, ContentId), A>| {
            if reached(&live, key) {
                return;
            }
            if let Some((at, _)) = self.find_in(&[Namespace::LOCAL], &Key(*key)) {
                out.push((self.maps[at].seg().id, *key));
            }
        };
        for root in &sets {
            look(&root.0, &mut out);
            let Some(got) = self.get_in(&[Namespace::LOCAL], root) else {
                return Err(StoreError::RootGone { generation: self.manifest.generation, key: root.0 });
            };
            for key in got.bytes.chunks_exact(32) {
                look(&ContentId(key.try_into().expect("32 bytes")), &mut out);
            }
            if got.tag != INDEXED_ROOT_TAG || self.plant() == Plant::IndexNotFollowed {
                continue;
            }
            // An indexed root: what each index holds is named too, one level. An index this
            // view follows already (an entry of `live` of its own, as `follow` pushes it) keeps
            // what it holds here; only another's keys are looked at. (A plain root naming an
            // index keeps its record, not what it holds: not followed, so looked at.)
            for index in got.bytes.chunks_exact(32) {
                let index = ContentId(index.try_into().expect("32 bytes"));
                if live.iter().any(|(followed, _)| *followed == index) {
                    continue;
                }
                let Some(held) = self.get_in(&[Namespace::LOCAL], &Key(index)) else {
                    return Err(StoreError::IndexGone { generation: self.manifest.generation, root: root.0, index });
                };
                if held.tag != INDEX_TAG {
                    return Err(StoreError::NotAnIndex { root: root.0, key: index, tag: held.tag });
                }
                for key in held.bytes.chunks_exact(32) {
                    look(&ContentId(key.try_into().expect("32 bytes")), &mut out);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn live_of(&self, seg: &Segment, roots: &[(ContentId, &[u8])]) -> (u64, u64) {
        let Some(&at) = self.by_id.get(&seg.id) else { return (seg.file_len(), seg.file_len()) };
        let run = self.maps[at].run();
        let (mut live, mut all) = (0u64, 0u64);
        for i in 0..seg.count as usize {
            let entry = Entry::read(&run[i * ENTRY..(i + 1) * ENTRY]);
            let bytes = aligned(u64::from(entry.len)) + ENTRY as u64;
            all += bytes;
            let newest = entry.tag != POINTER_TAG || self.newest_mapped(seg.namespace, &Key(entry.key)).is_some_and(|(it, _)| it == at);
            if newest && reached(roots, &entry.key) {
                live += bytes;
            }
        }
        (live, all)
    }

    /// The keys roots name, and the roots' own records.
    /// An indexed root's indexes followed one level, as reach follows them ([`Plain::live_roots`]).
    fn rooted(&self, now_ns: u64) -> Result<BTreeSet<ContentId>, StoreError> {
        let mut out = BTreeSet::new();
        for (key, named) in self.live_roots(now_ns)? {
            out.insert(key);
            for key in named.chunks_exact(32) {
                out.insert(ContentId(key.try_into().expect("32 bytes")));
            }
        }
        Ok(out)
    }

    fn listed_compact(&mut self, listed: Vec<(FileName, blaze_core::Meta)>, now_ns: u64, effects: &mut Vec<Effect>) {
        let current_log = self.slot.map(|it| it.log);
        let mut orphans = Vec::new_in(self.alloc.clone());
        for (name, _) in &listed {
            let leaf = name.as_str().rsplit('/').next().unwrap_or(name.as_str());
            let Some((id, log)) = parse_leaf(leaf) else { continue };
            let named = if log { Some(id) == current_log } else { self.manifest.segments.iter().any(|it| it.id == id) };
            let retired = self.manifest.retired.iter().any(|it| it.id == id && it.log == log);
            if !named && !retired {
                orphans.push((id, log));
            }
        }
        let planned = match self.cfg.liveness {
            Liveness::Everything => Ok(self.plan()),
            Liveness::Reached => self.plan_reached(now_ns).map(|it| (it, true)),
        };
        let (candidates, prune) = match planned {
            Ok(it) => it,
            Err(why) => return self.stop(why),
        };
        let mut stats = Sliced { orphans: orphans.len() as u32, ..Sliced::default() };
        let expired = self.manifest.retired.iter().any(|it| it.lost.is_none() && it.since_ns.saturating_add(GRACE_NS) <= now_ns);
        if candidates.is_empty() {
            if orphans.is_empty() && !expired {
                self.fold_asked = false;
                self.sliced = Some(stats);
                self.phase = Phase::Ready;
                return;
            }
            self.slice_ready(None, Vec::new_in(self.alloc.clone()), orphans, stats, effects);
            return;
        }
        let copied = self.copy_live(candidates, prune, now_ns, &mut stats);
        let (out, ids) = match copied {
            Ok(it) => it,
            Err(why) => return self.stop(why),
        };
        if out.is_empty() {
            self.slice_ready(None, ids, orphans, stats, effects);
            return;
        }
        let laid = match laid_of(out, self.alloc.clone()) {
            Ok(laid) => laid,
            Err(why) => return self.stop(why),
        };
        stats.bytes_out = laid.index_at + laid.run.len() as u64 + FOOTER as u64;
        self.orphans_for_compact = orphans;
        self.write_segment(laid, Why::Compact { candidates: ids, stats }, effects);
    }

    /// A slice's change is ready: parked for the next commit's flip when
    /// [`Plain::fold_slice`] asked it, else flipped on its own ([`Plain::compact`]).
    fn slice_ready(&mut self, seg: Option<(Segment, FileName)>, candidates: Vec<u64, A>, orphans: Vec<(u64, bool), A>, stats: Sliced, effects: &mut Vec<Effect>) {
        if std::mem::take(&mut self.fold_asked) {
            self.folding = Some(Folded { seg, candidates, orphans, stats });
            self.phase = Phase::Ready;
            return;
        }
        self.start_flip(Change::Compact { seg, candidates, orphans, stats }, effects);
    }

    /// What of `candidates` a slice keeps, copied into one batch, and the candidates it
    /// replaces (a published one only when it holds nothing live: it is let go of whole, or
    /// kept whole, never rewritten).
    fn copy_live(&self, candidates: Vec<Segment, A>, prune: bool, now_ns: u64, stats: &mut Sliced) -> Result<(Batch, Vec<u64, A>), StoreError> {
        let live = match (prune, self.cfg.liveness) {
            (false, _) => Live::All,
            (true, Liveness::Everything) => Live::Rooted(self.rooted(now_ns)?),
            (true, Liveness::Reached) => Live::Reached(self.live_roots(now_ns)?),
        };
        let current = self.manifest.generation;
        let holds_live = |seg: &Segment| -> bool {
            if matches!(live, Live::All) {
                return true;
            }
            let Some(&at) = self.by_id.get(&seg.id) else { return true };
            let run = self.maps[at].run();
            (0..seg.count as usize).any(|i| live.keeps(&format::key_at(run, i), false))
        };
        let mut kept_candidates = Vec::with_capacity_in(candidates.len(), self.alloc.clone());
        kept_candidates.extend(candidates.into_iter().filter(|seg| !seg.published || !holds_live(seg)));
        let candidates = kept_candidates;
        let namespace = candidates.first().map_or(Namespace::LOCAL, |it| it.namespace);
        let mut out = Batch::new(namespace);
        let mut seen = BTreeSet::new();
        for seg in &candidates {
            let mapped = &self.maps[self.by_id[&seg.id]];
            stats.bytes_in += seg.file_len();
            if seg.published {
                stats.dropped += u64::from(seg.count);
                continue;
            }
            let young = seg.generation + WINDOW > current;
            let run = mapped.run();
            for i in 0..seg.count as usize {
                // A proved segment's entry: read, not checked (decision 18).
                let entry = Entry::read(&run[i * ENTRY..(i + 1) * ENTRY]);
                let keep = live.keeps(&entry.key, young);
                // A pointer keeps only its newest value: the one in the newest row holding its
                // name (in this slice or after it); an older value is let go.
                let newest = entry.tag != POINTER_TAG || self.newest_mapped(seg.namespace, &Key(entry.key)).is_some_and(|(at, _)| at == self.by_id[&seg.id]);
                if !keep || !newest {
                    stats.dropped += 1;
                    continue;
                }
                if !seen.insert(entry.key) {
                    continue;
                }
                // Copied from a proved segment with its CRC as recorded: nothing is checked
                // or computed again.
                out.put_known(&entry, mapped.payload(&entry)).map_err(|error| error.into_store(entry.tag))?;
                stats.kept += 1;
            }
        }
        stats.segments = candidates.len() as u32;
        let mut ids: Vec<u64, A> = Vec::with_capacity_in(candidates.len(), self.alloc.clone());
        ids.extend(candidates.iter().map(|it| it.id));
        Ok((out, ids))
    }

    // ---- the flip: the one lock, held for it alone ----

    fn start_flip(&mut self, change: Change<A>, effects: &mut Vec<Effect>) {
        self.phase = Phase::Flipping(Box::new(Flip {
            change,
            waits: 0,
            next: None,
            fresh: false,
            new_log: false,
            log: self.head.clone(),
            manifest_bytes: Vec::new_in(Global),
            removals: Vec::new_in(self.alloc.clone()),
            refused: None,
            aborted: false,
            carry: false,
            early: 0,
            deferred: None,
        }));
        effects.push(Effect::Lock { file: self.flip.clone(), mode: LockMode::Exclusive, tag: self.tag(LOCK, 0) });
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
        // replaces is no longer named as this process planned it: a row committed again since
        // moved to the end with a newer generation, and a pointer's newest value may be in it,
        // so the copy may not take its place).
        let apply_slice = |m: &mut Manifest, seg: &mut Option<(Segment, FileName)>, candidates: &[u64], orphans: &[(u64, bool)]| -> bool {
            // As planned: named, here, of the same generation, and touched by no commit since
            // (a root that reuses a record the plan judged dead: [`Plain::reused`]).
            let planned = |id: &u64| self.manifest.segments.iter().find(|it| it.id == *id).map(|it| (it.generation, it.touched));
            // The copy's bytes may equal a named segment's (a merge of pointers keeps each
            // name's newest value alone, which can be one candidate's whole content or an
            // older row's): that file is the copy, so it is never removed or retired here.
            let copy = seg.as_ref().map(|(it, _)| it.id);
            if candidates.iter().any(|id| !m.segments.iter().any(|it| it.id == *id && it.local && Some((it.generation, it.touched)) == planned(id))) {
                if copy.is_some_and(|id| m.segments.iter().any(|it| it.id == id)) {
                    *seg = None;
                }
                return true;
            }
            // A row holding the copy's bytes (a candidate, or an older row: a pointer's copied
            // value is its newest, so no later row holds it) gives way to the copy at the run's
            // place.
            if let Some(copy) = copy {
                m.segments.retain(|it| it.published || it.id != copy || candidates.contains(&it.id));
            }
            let first = m.segments.iter().position(|it| candidates.first() == Some(&it.id));
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
                let oldest = self.manifest.segments.iter().filter(|it| candidates.contains(&it.id)).map(|it| it.generation).min();
                seg.generation = oldest.unwrap_or(generation);
                m.segments.insert(first.unwrap_or(0).min(m.segments.len()), *seg);
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
        let mut early = Vec::new_in(self.alloc.clone());
        match &mut flip.change {
            Change::Commit { segs, started_ns, roots } => {
                if now_ns.saturating_sub(*started_ns) > GRACE_NS / 2 {
                    // A compaction may have retired its segment as named by no generation:
                    // it is not flipped.
                    refused = Some(StoreError::CommitTooSlow { took_s: now_ns.saturating_sub(*started_ns) / 1_000_000_000, grace_s: GRACE_NS / 1_000_000_000 });
                } else if let Some(&(segment, key)) = self.touches.iter().find(|(id, _)| !m.segments.iter().any(|it| it.id == *id))
                    && self.plant() != Plant::CarrySkipped
                {
                    // A record this commit's roots name, dead in its view, was in a segment a
                    // slice replaced since: the slice may have let it go. The command still
                    // holds its bytes (its mapping of the retired file stays valid: the unlink
                    // rule below keeps the file while this process's lease, opened before the
                    // retirement, lives, and a mapping outlives an unlink anyway), so the
                    // commit carries it forward: not flipped now; the records are written into
                    // a segment of the commit's own, outside the lock, and it flips again
                    // ([`Plain::carry`]). After [`CARRY_ROUNDS`] the refusal is the stop.
                    if self.carry_rounds >= CARRY_ROUNDS || !self.cfg.writes.contains(&Namespace::LOCAL) {
                        refused = Some(StoreError::SweptUnder { key, segment });
                    } else {
                        self.carrying.clear();
                        self.carrying.extend(self.touches.iter().filter(|(id, _)| !m.segments.iter().any(|it| it.id == *id)).copied());
                        carry = true;
                    }
                } else {
                    // Marked before the slice below: a slice of this process that would drop
                    // a record this commit's roots reuse aborts as another's would.
                    for (id, _) in self.touches.iter() {
                        if let Some(it) = m.segments.iter_mut().find(|it| it.id == *id) {
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
                            RootEdit::Set { name, key, life_ns } => {
                                m.roots.retain(|it| it.name != *name);
                                let expires_ns = if *life_ns == NEVER { NEVER } else { now_ns.saturating_add(*life_ns).min(NEVER - 1) };
                                m.roots.push(Root { name: name.clone(), key: key.0, generation, expires_ns });
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
                        slice.stats.aborted = apply_slice(&mut m, &mut slice.seg, &slice.candidates, &slice.orphans);
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
            Change::Compact { seg, candidates, orphans, .. } => {
                aborted = apply_slice(&mut m, seg, candidates, orphans);
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
            removals.push(file.clone());
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
        let estimate = m.encode().len() as u64 + 24;
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
        let bytes = m.encode();
        let slot = Slot { generation, log, manifest_at: at, manifest_len: bytes.len() as u32, manifest_crc: crc32c::crc32c(&bytes) };
        flip.log = self.log_file(log);
        flip.fresh = fresh;
        flip.removals = removals;
        flip.next = Some((slot, m));
        let first = if flip.new_log {
            flip.manifest_bytes = bytes;
            Effect::Truncate { file: flip.log.clone(), len: 0, tag: self.tag(LOG_TRUNCATE, 0) }
        } else {
            Effect::WriteAt { file: flip.log.clone(), at, bytes, tag: self.tag(LOG_WRITE, 0) }
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
    fn write_slot(&self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = &self.phase else { unreachable!("a slot is a flip's") };
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
        effects.push(Effect::WriteAt { file: self.head.clone(), at: other, bytes: vec![0u8; format::SLOT], tag: self.tag(SLOT_WRITE, 2) });
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
        effects.push(Effect::WriteAt { file: self.head.clone(), at, bytes: slot.encode().to_vec(), tag: self.tag(SLOT_WRITE, 0) });
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
            let roots = match flip.change {
                Change::Commit { roots, .. } => roots,
                _ => Vec::new_in(self.alloc.clone()),
            };
            self.commit_refused(why, roots);
            return;
        }
        if flip.carry {
            let Change::Commit { segs, started_ns, roots } = flip.change else { unreachable!("only a commit carries") };
            self.carry(segs, started_ns, roots, effects);
            return;
        }
        if flip.aborted {
            let Change::Compact { seg, mut stats, .. } = flip.change else { unreachable!("only a compaction aborts") };
            stats.aborted = true;
            self.sliced = Some(stats);
            match seg {
                Some((_, file)) => {
                    // Never named: this process's own, removed now.
                    self.removing.clear();
                    self.removing.push(file.clone());
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
        self.checked.clear();
        self.dropped.clear();
        self.torn_stamp = false;
        self.slot = Some(slot);
        self.manifest = manifest;
        self.leased = self.manifest.leases.iter().any(|it| it.holder == self.cfg.nonce);
        // The slot names the log this flip wrote its manifest to.
        self.log_lost = false;
        if self.cfg.durable && self.plant() == Plant::FlipBeforeSync {
            for (i, file) in std::mem::replace(&mut self.late_sync, Vec::new_in(self.alloc.clone())).into_iter().enumerate() {
                effects.push(Effect::Sync { file, tag: self.tag(LATE_SYNC, i as u32) });
            }
        }
        let left = flip.removals.len() as u32;
        for (i, file) in flip.removals.iter().enumerate() {
            effects.push(Effect::Remove { file: file.clone(), tag: self.tag(REMOVE, i as u32) });
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
            if entry.tag == POINTER_TAG {
                // A pointer's carried value would read as its newest: not carried.
                return self.commit_refused(StoreError::SweptUnder { key, segment }, roots);
            }
            if let Err(error) = out.put_known(&entry, self.maps[at].payload(&entry)) {
                return self.commit_refused(error.into_store(entry.tag), roots);
            }
        }
        self.touches.retain(|it| !carrying.contains(it));
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
        let mut left = 0;
        for (i, seg) in self.manifest.segments.iter().enumerate() {
            if !seg.local || self.by_id.contains_key(&seg.id) || self.dropped.contains(&seg.id) {
                continue;
            }
            left += 1;
            effects.push(Effect::Map { file: self.segment_file(seg.id), extent: Extent::Whole, tag: self.tag(MAP, i as u32) });
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
        let seg = self.manifest.segments[index as usize];
        let file = self.segment_file(seg.id);
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
                Err(unverified) => Err((seg, self.segment_file(seg.id), StoreError::Layout {
                    segment: SegmentId(seg.digest),
                    what: "it is sealed, and its file's length is not the one its generation names",
                    at: unverified.len(),
                })),
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
            effects.push(if published { Effect::Sync { file, tag } } else { Effect::Barrier { file, tag } });
        }
        self.phase = Phase::Sealing { left: ids.len() as u32, ids };
        true
    }

    /// The sealed segments of an earlier boot this boot has not walked yet: what the
    /// background pass has left ([`Plain::scrub_next`]), oldest generation first.
    pub fn unscrubbed(&self) -> Vec<u64, A> {
        let mut left: Vec<&Segment, A> = Vec::new_in(self.alloc.clone());
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
        self.order_view();
        match then {
            Then::Commit => {
                let segments = std::mem::replace(&mut self.committing, Vec::new_in(self.alloc.clone()));
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segments }));
                self.sealed.clear();
            }
            Then::Compact(mut stats) => {
                let next = match self.cfg.liveness {
                    Liveness::Everything => Ok(self.plan().0),
                    Liveness::Reached => self.plan_reached(self.now_ns),
                };
                match next {
                    Ok(next) => stats.more = !next.is_empty(),
                    Err(why) => return self.stop(why),
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
                    effects.push(Effect::Truncate { file: self.head.clone(), len: HEAD_LEN as u64, tag: self.tag(HEAD_MAKE, 0) });
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
                        file: self.head.clone(),
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
                        effects.push(Effect::List { dir: self.cfg.dir.clone().into(), tag: self.tag(LIST, 0) });
                        self.phase = Phase::CheckLog { ctx, slots, at, why };
                        return Ok(());
                    }
                };
                let manifest = (bytes.len() == slot.manifest_len as usize && crc32c::crc32c(&bytes) == slot.manifest_crc)
                    .then(|| Manifest::decode(&bytes))
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
                let (doing, file) = match index {
                    0 => ("making the head at its length", &self.head),
                    1 => ("syncing the head made", &self.head),
                    _ => ("syncing the store's directory after making the head", &self.cfg.dir),
                };
                if let Some(why) = fail(doing, file, &done) {
                    return Err(why);
                }
                if index == 0 {
                    // Synced once in the store's life, before its first segment: so an empty or
                    // short head never stands beside segments a power cut kept.
                    effects.push(Effect::Sync { file: self.head.clone(), tag: self.tag(HEAD_MAKE, 1) });
                } else if index == 1 {
                    // And its name with it: a power cut never keeps a segment's name and loses
                    // the head's.
                    effects.push(Effect::SyncDir { dir: self.cfg.dir.clone().into(), tag: self.tag(HEAD_MAKE, 2) });
                } else {
                    self.phase = Phase::ReadHead(Ctx::Open);
                    effects.push(Effect::ReadAll { file: self.head.clone(), tag: self.tag(HEAD_READ, 0) });
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
                    effects.push(Effect::Read { file: file.clone(), at: meta.len - FOOTER as u64, len: FOOTER, tag: self.tag(FOOTER_READ, self.listed.len() as u32) });
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
                let (file, id) = self.listed[index as usize].clone();
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
                        effects.push(Effect::List { dir: self.cfg.dir.clone().into(), tag: self.tag(LIST, 0) });
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
                let file = writing.file.clone();
                for (i, (at, bytes)) in pieces.into_iter().enumerate() {
                    effects.push(Effect::WriteAt { file: file.clone(), at, bytes, tag: self.tag(SEG_PIECE, i as u32) });
                }
                Ok(())
            }
            (SEG_PIECE, Phase::Writing(writing, _)) => {
                if let Some(why) = fail("writing a segment", &writing.file, &done) {
                    return Err(why);
                }
                writing.left -= 1;
                if writing.left > 0 {
                    return Ok(());
                }
                let Phase::Writing(writing, why) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                if self.cfg.durable && self.plant() != Plant::SkipDataSync && self.plant() != Plant::FlipBeforeSync {
                    effects.push(Effect::Sync { file: writing.file.clone(), tag: self.tag(SEG_SYNC, 0) });
                    self.phase = Phase::SyncingSegment(writing, why);
                    return Ok(());
                }
                if self.cfg.durable && self.plant() == Plant::FlipBeforeSync {
                    self.late_sync.push(writing.file.clone());
                }
                self.segment_written(*writing, *why, effects);
                Ok(())
            }
            (SEG_SYNC, Phase::SyncingSegment(writing, _)) => {
                if let Some(why) = fail("syncing a segment", &writing.file, &done) {
                    return Err(why);
                }
                let Phase::SyncingSegment(writing, why) = std::mem::replace(&mut self.phase, Phase::Broken) else { unreachable!() };
                self.segment_written(*writing, *why, effects);
                Ok(())
            }
            (LOCK, Phase::Flipping(flip)) => match done {
                Done::Locked => {
                    self.holds_lock = true;
                    self.phase_ctx_read_head(effects);
                    Ok(())
                }
                Done::Contended => {
                    flip.waits += 1;
                    if flip.waits > LOCK_STEPS_MOST {
                        // Another process held it past the bound, and a flip holds it for
                        // milliseconds: it is stuck.
                        const _: () = assert!(LOCK_STEP.as_millis() * LOCK_STEPS_MOST as u128 == 60_000, "the text below says 60 s");
                        return Err(StoreError::Stalled { waited_for: "the flip lock, which another process held for 60 s" });
                    }
                    effects.push(Effect::Timer { after: LOCK_STEP, tag: self.tag(LOCK_WAIT, 0) });
                    Ok(())
                }
                done => Err(fail("taking the flip lock", &self.flip, &done).unwrap_or_else(|| unexpected("taking the flip lock"))),
            },
            (LOCK_WAIT, Phase::Flipping(_)) => {
                effects.push(Effect::Lock { file: self.flip.clone(), mode: LockMode::Exclusive, tag: self.tag(LOCK, 0) });
                Ok(())
            }
            (LOG_TRUNCATE, Phase::Flipping(flip)) => {
                if let Some(why) = fail("making a manifest log", &flip.log, &done) {
                    return Err(why);
                }
                let bytes = std::mem::take(&mut flip.manifest_bytes);
                effects.push(Effect::WriteAt { file: flip.log.clone(), at: 0, bytes, tag: self.tag(LOG_WRITE, 0) });
                Ok(())
            }
            (LOG_WRITE, Phase::Flipping(flip)) => {
                if let Some(why) = fail("writing a manifest", &flip.log, &done) {
                    return Err(why);
                }
                if self.cfg.durable {
                    effects.push(Effect::Sync { file: flip.log.clone(), tag: self.tag(LOG_SYNC, 0) });
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
                    effects.push(Effect::SyncDir { dir: self.cfg.dir.clone().into(), tag: self.tag(DIR_SYNC, 0) });
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
            (SLOT_WRITE, Phase::Flipping(_)) => {
                // One write at a time, the slot last ([`Plain::write_slot`]): a failure here
                // stops the store with nothing of this flip in flight under the lock, and
                // before the slot, with nothing of it committed.
                let what = match index {
                    1 => "writing the cache directory tag beside the head",
                    2 => "blanking the head's other slot",
                    _ => "writing the head",
                };
                let file = if index == 1 { self.name(CACHEDIR_TAG) } else { self.head.clone() };
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
                    effects.push(Effect::Sync { file: self.head.clone(), tag: self.tag(HEAD_SYNC, 0) });
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
        effects.push(Effect::ReadAll { file: self.head.clone(), tag: self.tag(HEAD_READ, 0) });
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
                self.slot = base.slot;
                self.manifest = base.manifest;
                self.opened_ns = now_ns;
                self.opened_generation = self.manifest.generation;
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
                    effects.push(Effect::List { dir: self.cfg.dir.clone().into(), tag: self.tag(LIST, 0) });
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
    fn segment_written(&mut self, writing: Writing<A>, why: Why<A>, effects: &mut Vec<Effect>) {
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
                let orphans = std::mem::replace(&mut self.orphans_for_compact, Vec::new_in(self.alloc.clone()));
                self.slice_ready(Some(seg), candidates, orphans, stats, effects);
            }
        }
    }
}

/// A batch of copied records (a slice's, a carry's: each payload at a multiple of [`ALIGN`]
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
fn sorted_keys<A: Allocator + Clone>(keys: &[Key], alloc: A) -> Vec<u8, A> {
    let mut sorted = Vec::with_capacity_in(keys.len(), alloc.clone());
    sorted.extend_from_slice(keys);
    sorted.sort_unstable();
    sorted.dedup();
    let mut bytes = Vec::with_capacity_in(sorted.len() * 32, alloc);
    for key in &sorted {
        bytes.extend_from_slice(&key.0.0);
    }
    bytes
}

/// `key` is one of `roots`' own keys, or among the sorted 32-byte keys a root's record holds:
/// a binary search of each record in place, nothing collected.
fn reached(roots: &[(ContentId, &[u8])], key: &ContentId) -> bool {
    roots.iter().any(|(root, keys)| root == key || sorted_holds(keys, key))
}

fn sorted_holds(keys: &[u8], key: &ContentId) -> bool {
    let (mut lo, mut hi) = (0usize, keys.len() / 32);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match keys[mid * 32..mid * 32 + 32].cmp(&key.0[..]) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

/// What a slice's copy keeps.
enum Live<'a, A: Allocator + Clone> {
    /// Everything (a merge of a [`Liveness::Everything`] store).
    All,
    /// What roots name, or younger than [`WINDOW`] (its pruning over the cap).
    Rooted(BTreeSet<ContentId>),
    /// What reaches from an unexpired root ([`Liveness::Reached`]).
    Reached(Vec<(ContentId, &'a [u8]), A>),
}

impl<A: Allocator + Clone> Live<'_, A> {
    fn keeps(&self, key: &ContentId, young: bool) -> bool {
        match self {
            Live::All => true,
            Live::Rooted(set) => young || set.contains(key),
            Live::Reached(roots) => reached(roots, key),
        }
    }
}

fn lost_orphan() -> String {
    LostMark { kind: LostKind::Orphan, records: None }.why()
}
