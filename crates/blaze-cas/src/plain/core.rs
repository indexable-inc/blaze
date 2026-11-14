//! The plain store's state machine ([`super`] has the layout and the costs): opening (the
//! head, the manifest, every segment mapped once, a segment of another boot checked whole),
//! reads in place with no lock and no shared write, puts into per-worker [`Batch`]es, the
//! commit (one segment written without a lock, then the flip under the one lock), and
//! compaction in bounded slices. It owns no IO: it hands back [`Effect`]s and takes their
//! [`Completion`]s, so the simulation runs this very code (`super::tests`), and
//! [`super::Driven`] carries the effects out on a real driver.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use blaze_core::{Completion, Done, Effect, Extent, FileName, LockMode, Tag as EffectTag};
use blaze_ids::{ContentId, crc32c};
use blaze_keyed::{
    kind::{Namespace, Tag},
    store::{ALIGN, ENTRY, FOOTER, HEADER, Put},
};

use super::format::{
    self, Entry, FORMAT, HEAD_LEN, HeadRead, Manifest, OFFSET_MOST, Retired, Root, SLOT_AT, Segment,
    Slot, aligned, record_crc,
};
use super::verified::{Boot, Checks, Unverified, Verified};
use crate::{Corrupt, Key, Mismatch, content_of};

/// The tag of the store's own root records (a root's sorted keys), always under
/// [`Namespace::LOCAL`]: reserved, no kind of blaze-keyed's takes it.
pub const ROOT_TAG: Tag = Tag(u16::MAX);

/// The tags a store asks with: `tags .. tags + TAGS`.
pub const TAGS: u64 = 1 << 40;

/// A file no generation names is unlinked this long after it left the last manifest that
/// named it (or was found named by none): the bound on how long a process may take from
/// reading the head to mapping what it names, and on a commit from starting its segment to
/// its flip (half of it: [`Plain::commit`]). Mappings outlive an unlink, so a process that
/// mapped a file keeps reading it whatever happens after.
pub const GRACE_NS: u64 = 10 * 60 * 1_000_000_000;

/// Records no root names are kept for this many generations after the one that put them.
pub const WINDOW: u64 = 16;

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

const HEAD: &str = "head";
const FLIP: &str = "flip";
const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// The cache directory tag (bford.info/cachedir): backup and indexing tools that honour it
/// skip the store.
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by blaze: its plain store.\n# For information about cache directory tags see https://bford.info/cachedir/\n";

/// What a store is opened on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlainConfig {
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
    /// The machine's page size: the stride a segment's index is touched at once mapped, so a
    /// probe takes no fault. Nothing in the layout depends on it.
    pub page: u32,
    /// The namespaces a put may name, [`Namespace::LOCAL`] always among them (the store's roots
    /// live there): only `LOCAL` on a machine that is not a trusted writer (every developer's);
    /// a trusted builder (CI) adds [`Namespace::PUBLIC`] and its organisation's. A commit writes
    /// one segment for each that has records.
    pub writes: Vec<Namespace>,
    /// The namespaces [`Plain::get`] answers from, in the order it asks them; no other is
    /// searched.
    pub reads: Vec<Namespace>,
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

/// Why a put or a commit was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PutError {
    /// The key holds other bytes.
    Mismatch(Mismatch),
    /// A value of 4 GiB or more, or a batch past what an entry's offset holds.
    TooLong { key: Key, len: u64 },
    /// The key's committed record, read to compare, is corrupt.
    Corrupt(Corrupt),
    /// The store is not ready for it (opening, committing, compacting or broken).
    Busy(&'static str),
    /// Two batches of one commit hold one key with other values: why, naming the key.
    TwoValues(String),
    /// A put names a namespace this store does not write ([`PlainConfig::writes`]): a machine
    /// that is not a trusted writer writes only [`Namespace::LOCAL`].
    NotWritable { namespace: Namespace },
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutError::NotWritable { namespace } => write!(f, "a put names namespace {:x}, which this store does not write: a machine that is not a trusted writer writes only the local namespace", namespace.0),
            PutError::Mismatch(it) => write!(f, "{:x} holds other bytes ({:x}) than the put's ({:x}): a key covers every input its value depends on", it.key.0, it.kept, it.given),
            PutError::TooLong { key, len } => write!(f, "the value of {:x} is {len} bytes: a plain store's record is shorter than 4 GiB and a segment shorter than 16 TiB", key.0),
            PutError::Corrupt(it) => it.fmt(f),
            PutError::Busy(what) => write!(f, "the plain store is {what}: a commit waits for it to be ready"),
            PutError::TwoValues(why) => f.write_str(why),
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
    /// The generation whose manifest the open read whole: everything committed at or before
    /// it reads back or is in `lost` by name. None when no generation survived. What was
    /// committed after it, with no sync, a power cut may have taken with no trace to name
    /// (durable mode syncs so nothing is).
    pub whole: Option<u64>,
}

/// A completed commit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Committed {
    pub generation: u64,
    /// The segments it wrote, one for each namespace that held a record, in the order of
    /// [`PlainConfig::writes`]; none when it held no record.
    pub segments: Vec<u64>,
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

enum RootEdit {
    Set { name: String, key: Key },
    Unset(String),
}

/// Why a head is being read: at the open, or under the lock for a flip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctx {
    Open,
    Flip,
}

/// What a write of a segment is for.
enum Why {
    /// A commit's segments are written one after another, one for each namespace with
    /// records, then flipped at once: `rest` still to write, `done` written.
    Commit { started_ns: u64, roots: Vec<RootEdit>, rest: Vec<Laid>, done: Vec<(Segment, FileName)> },
    Compact { candidates: Vec<u64>, stats: Sliced },
}

/// One segment laid out and not yet written: the payloads at their offsets, from [`HEADER`]
/// to `index_at`, and the sorted run.
struct Laid {
    pieces: Vec<(u64, Vec<u8>)>,
    index_at: u64,
    run: Vec<u8>,
    namespace: Namespace,
}

/// A segment being written, before its flip.
struct Writing {
    seg: Segment,
    file: FileName,
    /// The writes at offsets, laid out by [`Plain::commit`], each moved into its effect.
    pieces: Vec<(u64, Vec<u8>)>,
    left: u32,
}

/// What a flip adds to the generation it builds on.
enum Change {
    Commit { segs: Vec<(Segment, FileName)>, started_ns: u64, roots: Vec<RootEdit> },
    /// Only what an open found (segments checked or lost): [`Plain::pending_stamps`].
    Stamp,
    Compact { seg: Option<(Segment, FileName)>, candidates: Vec<u64>, orphans: Vec<(u64, bool)>, stats: Sliced },
    /// Segments fetched from a source, written here: named here and published.
    Adopt { segs: Vec<Segment> },
    /// Segments pushed to a shared bucket.
    Publish { ids: Vec<u64> },
}

/// The generation a flip builds on, read under the lock.
struct Base {
    /// The slot the new generation's number and log follow, `None` when nothing was ever
    /// committed.
    slot: Option<Slot>,
    manifest: Manifest,
}

struct Flip {
    change: Change,
    waits: u32,
    next: Option<(Slot, Manifest)>,
    /// The head file did not exist: the first flip writes it whole.
    fresh: bool,
    new_log: bool,
    log: FileName,
    manifest_bytes: Vec<u8>,
    removals: Vec<FileName>,
    /// The commit was refused under the lock, or the compaction aborted: unlock and stop.
    refused: Option<String>,
    aborted: bool,
}

enum Phase {
    New,
    /// A head read, at the open or for a flip.
    ReadHead(Ctx),
    /// A slot's manifest read: `slots[at]`, the other the fallback.
    ReadManifest { ctx: Ctx, slots: [Option<Slot>; 2], at: usize, fell_back: bool },
    /// A manifest's read failed: the directory is listed to tell a log a power cut took (not
    /// there: the same as torn) from a read that failed (there: the error, by name). A failed
    /// read is never taken for a lost generation.
    CheckLog { ctx: Ctx, slots: [Option<Slot>; 2], at: usize, why: String },
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
    Writing(Box<Writing>, Box<Why>),
    /// The segment's sync, before the flip.
    SyncingSegment(Box<Writing>, Box<Why>),
    Flipping(Box<Flip>),
    Removing { left: u32, then: Then },
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
}

/// The plain store's state machine: one per process (or per test actor).
pub struct Plain {
    cfg: PlainConfig,
    #[cfg(test)]
    plant: Plant,
    head: FileName,
    flip: FileName,
    phase: Phase,
    /// The newest generation this process saw, and its manifest.
    slot: Option<Slot>,
    manifest: Manifest,
    /// Every segment this process mapped, each through its proof: kept mapped for the life of
    /// the process, never unmapped during a run (an unmap is a TLB shootdown on every core).
    maps: Vec<Verified>,
    /// The boot every proof here holds for.
    boot: Boot,
    /// Full checks made by this process, and reads (the per-read count stays zero).
    checks: std::cell::Cell<Checks>,
    /// Each segment this process checked in full, in order: for the oracle, which fails on a
    /// segment checked twice in one boot.
    checked_ids: Vec<u64>,
    by_id: BTreeMap<u64, usize>,
    /// What [`Plain::get`] searches: this generation's mapped segments, newest first.
    view: Vec<usize>,
    /// The store's own batches, one for each namespace of [`PlainConfig::writes`] in its order,
    /// behind [`Plain::put`]: read back by [`Plain::get`] until the commit that writes them
    /// completes.
    held: Vec<Held>,
    /// Held batches being committed, still read by [`Plain::get`].
    sealed: Vec<Held>,
    /// The segments the commit in flight wrote, for its [`Committed`].
    committing: Vec<u64>,
    roots_pending: Vec<RootEdit>,
    /// Found at the open, written at the next flip: segments of another boot checked whole,
    /// segments lost, files named by no generation.
    checked: BTreeSet<u64>,
    dropped: BTreeSet<u64>,
    orphans: Vec<(u64, bool)>,
    report: Report,
    committed: Option<Result<Committed, String>>,
    sliced: Option<Sliced>,
    broken: Option<String>,
    seq: u64,
    /// Lists and footers of the open's fallback, by tag index.
    listed: Vec<(FileName, u64)>,
    /// A flip's state while its head is read under the lock.
    flip_parked: Option<Box<Flip>>,
    /// What a compaction's directory look found, for its flip.
    orphans_for_compact: Vec<(u64, bool)>,
    /// The planted [`Plant::FlipBeforeSync`]'s segments, synced after the flip.
    late_sync: Vec<FileName>,
    /// Segments of an earlier boot whose map failed, with why: lost only if the directory no
    /// longer holds them (a power cut took the file); a failed map of one it holds is an error.
    unopened: Vec<(Segment, FileName, String)>,
    /// This open made the head at its length already (it is made once, never twice).
    head_made: bool,
    /// The flip lock is held (taken, and its unlock not yet asked): a step that fails under it
    /// lets it go as it stops, so a broken store never holds every other process's commit.
    holds_lock: bool,
    /// The torn slot a short head showed before this open made it whole length.
    head_torn: Option<usize>,
    /// A torn slot was reported at this open: the next flip writes over it (that slot is the
    /// one the newest does not hold), so it is reported once, not at every open.
    torn_stamp: bool,
}

/// One namespace's held puts, and where each key is in them.
struct Held {
    batch: Batch,
    at: BTreeMap<Key, usize>,
}

impl Held {
    fn new(namespace: Namespace) -> Held {
        Held { batch: Batch::new(namespace), at: BTreeMap::new() }
    }

    fn entry(&self, key: &Key) -> Option<&Entry> {
        self.at.get(key).map(|&i| &self.batch.entries[i])
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

/// The id of a file named `seg-<16 hex>` or `manifests-<16 hex>`, and whether it is a log.
fn parse_leaf(leaf: &str) -> Option<(u64, bool)> {
    let (hex, log) = match (leaf.strip_prefix("seg-"), leaf.strip_prefix("manifests-")) {
        (Some(hex), _) => (hex, false),
        (_, Some(hex)) => (hex, true),
        _ => return None,
    };
    (hex.len() == 16).then(|| u64::from_str_radix(hex, 16).ok()).flatten().map(|id| (id, log))
}

impl Plain {
    #[cfg(test)]
    pub(crate) fn planted(cfg: PlainConfig, plant: Plant) -> Plain {
        let mut it = Plain::new(cfg);
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

    pub fn new(cfg: PlainConfig) -> Plain {
        assert!(cfg.writes.contains(&Namespace::LOCAL), "a plain store writes the local namespace: its roots live there");
        let name = |leaf: &str| FileName::new(&format!("{}/{leaf}", cfg.dir.as_str())).expect("a store's directory names files under it");
        let mut held = Vec::with_capacity_in(cfg.writes.len(), Global);
        held.extend(cfg.writes.iter().map(|it| Held::new(*it)));
        Plain {
            head: name(HEAD),
            flip: name(FLIP),
            boot: Boot(cfg.boot),
            checks: std::cell::Cell::new(Checks::default()),
            checked_ids: Vec::new_in(Global),
            held,
            sealed: Vec::new_in(Global),
            committing: Vec::new_in(Global),
            cfg,
            #[cfg(test)]
            plant: Plant::None,
            phase: Phase::New,
            slot: None,
            manifest: Manifest::default(),
            maps: Vec::new_in(Global),
            by_id: BTreeMap::new(),
            view: Vec::new_in(Global),
            roots_pending: Vec::new_in(Global),
            checked: BTreeSet::new(),
            dropped: BTreeSet::new(),
            orphans: Vec::new_in(Global),
            report: Report::default(),
            committed: None,
            sliced: None,
            broken: None,
            seq: 0,
            listed: Vec::new_in(Global),
            flip_parked: None,
            orphans_for_compact: Vec::new_in(Global),
            late_sync: Vec::new_in(Global),
            unopened: Vec::new_in(Global),
            head_made: false,
            holds_lock: false,
            head_torn: None,
            torn_stamp: false,
        }
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

    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
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
    pub fn take_committed(&mut self) -> Option<Result<Committed, String>> {
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
    pub fn segments(&self) -> Vec<(u64, u32)> {
        let mut out = Vec::with_capacity_in(self.manifest.segments.len(), Global);
        out.extend(self.manifest.segments.iter().map(|it| (it.id, it.count)));
        out
    }

    /// The roots this generation names: `(name, record key)`.
    pub fn roots(&self) -> Vec<(String, Key)> {
        let mut out = Vec::with_capacity_in(self.manifest.roots.len(), Global);
        out.extend(self.manifest.roots.iter().map(|it| (it.name.clone(), Key(it.key))));
        out
    }

    fn stop(&mut self, why: String) {
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
    fn find_in(&self, spaces: &[Namespace], key: &Key) -> Option<(usize, Entry)> {
        if self.plant() == Plant::IgnoresNamespace {
            return self.view.iter().find_map(|&at| self.probe(at, key));
        }
        spaces.iter().find_map(|space| self.view.iter().filter(|&&at| self.maps[at].seg().namespace == *space).find_map(|&at| self.probe(at, key)))
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

    /// The caller's explicit ask (a `scrub` command), never a read's: every record of every
    /// segment mapped here checked against its CRC-32C as its bytes lie now. A byte that changed
    /// on the disk after its segment was proved is found here, or by the consumer that fails on
    /// it; a read never looks.
    pub fn scrub(&self) -> Vec<Corrupt> {
        let mut out = Vec::new_in(Global);
        out.extend(self.maps.iter().flat_map(Verified::scrub));
        out
    }

    /// The segments of `spaces` this generation names that are not here: what a source is
    /// asked to fetch from on a miss.
    pub fn remote(&self, spaces: &[Namespace]) -> Vec<Segment> {
        let mut out = Vec::new_in(Global);
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
        let crc = record_crc(&key.0, tag, value);
        let same = |entry: &Entry| entry.tag == tag && entry.len as usize == value.len() && entry.crc == crc;
        if let Some((held, entry)) = self.held_in(namespace, &key) {
            if same(&entry) {
                return Ok(Put::Same);
            }
            let kept = held.batch.payload(&entry);
            return Err(PutError::Mismatch(Mismatch { key, kept: content_of(kept), given: content_of(value) }));
        }
        if let Some((mapped, entry)) = self.find_in(std::slice::from_ref(&namespace), &key) {
            if same(&entry) {
                return Ok(Put::Same);
            }
            let kept = self.maps[mapped].payload(&entry);
            return Err(PutError::Mismatch(Mismatch { key, kept: content_of(kept), given: content_of(value) }));
        }
        let held = &mut self.held[at];
        held.batch.put(key, tag, value)?;
        held.at.insert(key, held.batch.entries.len() - 1);
        Ok(Put::Written)
    }

    /// Names `keys` as kept under `name` at the next commit: a [`ROOT_TAG`] record of the
    /// sorted keys under [`Namespace::LOCAL`], and the root. Compaction keeps every record a
    /// root names.
    pub fn root(&mut self, name: &str, keys: &[Key]) -> Result<Key, PutError> {
        let mut sorted = Vec::with_capacity_in(keys.len(), Global);
        sorted.extend_from_slice(keys);
        sorted.sort_unstable();
        sorted.dedup();
        let mut bytes = Vec::with_capacity_in(sorted.len() * 32, Global);
        for key in &sorted {
            bytes.extend_from_slice(&key.0.0);
        }
        let key = Key(ContentId::of(ROOT_DOMAIN, &bytes));
        self.put(key, Namespace::LOCAL, ROOT_TAG, &bytes)?;
        self.roots_pending.push(RootEdit::Set { name: name.to_owned(), key });
        Ok(key)
    }

    /// Lets go of the root `name` at the next commit.
    pub fn unroot(&mut self, name: &str) {
        self.roots_pending.push(RootEdit::Unset(name.to_owned()));
    }

    // ---- the commit ----

    /// Writes the store's batches and `batches` (the workers', handed off) as one new segment
    /// for each namespace that holds records, one after another, with no lock, then flips the
    /// head once to a generation naming them all. The outcome is [`Plain::take_committed`]'s
    /// once the store is ready again.
    pub fn commit(&mut self, batches: Vec<Batch>, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), PutError> {
        if !self.ready() {
            return Err(PutError::Busy(self.what()));
        }
        if let Some(batch) = batches.iter().find(|it| !self.cfg.writes.contains(&it.namespace)) {
            return Err(PutError::NotWritable { namespace: batch.namespace });
        }
        // The held batches stay readable while they are written: sealed until the commit ends.
        let mut fresh = Vec::with_capacity_in(self.cfg.writes.len(), Global);
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
        self.committing.clear();
        // The roots this commit's records were put with: one set after it started is the next's.
        let roots = std::mem::take(&mut self.roots_pending);
        let mut rest = laid.into_iter();
        match rest.next() {
            None if roots.is_empty() && !self.pending_stamps() => {
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segments: Vec::new_in(Global) }));
                self.sealed.clear();
            }
            None => self.start_flip(Change::Commit { segs: Vec::new_in(Global), started_ns: now_ns, roots }, effects),
            Some(first) => {
                let mut left = Vec::with_capacity_in(rest.len(), Global);
                left.extend(rest);
                self.write_segment(first, Why::Commit { started_ns: now_ns, roots, rest: left, done: Vec::new_in(Global) }, effects);
            }
        }
        Ok(())
    }

    /// One segment laid out for each namespace of [`PlainConfig::writes`] (in its order) that
    /// holds a record not already committed under it.
    fn lay_out(&self, batches: Vec<Batch>) -> Result<Vec<Laid>, PutError> {
        let mut out = Vec::with_capacity_in(self.cfg.writes.len(), Global);
        let mut batches = batches;
        for &namespace in &self.cfg.writes {
            let mut entries: Vec<Entry> = Vec::new_in(Global);
            let mut pieces: Vec<(u64, Vec<u8>)> = Vec::new_in(Global);
            let mut base = HEADER as u64;
            let own = self.sealed.iter().find(|it| it.batch.namespace == namespace).map(|it| &it.batch);
            // The store's own batch first (copied: it is read until the commit ends), then each
            // worker's of this namespace (moved).
            // In the order handed, so the same puts lay out the same bytes.
            let mut theirs = Vec::new_in(Global);
            theirs.extend(batches.extract_if(.., |it| it.namespace == namespace));
            // Lays out one batch's entries after what is laid: the offset its bytes go at.
            let mut lay = |batch: &Batch| -> Result<u64, PutError> {
                let at = base;
                for entry in &batch.entries {
                    if let Some((at, kept)) = self.find_in(std::slice::from_ref(&namespace), &Key(entry.key)) {
                        if kept.same_record(entry) {
                            // Its bytes stay in the segment unreferenced; compaction drops them.
                            continue;
                        }
                        let kept = self.maps[at].payload(&kept);
                        return Err(PutError::Mismatch(Mismatch { key: Key(entry.key), kept: content_of(kept), given: content_of(batch.payload(entry)) }));
                    }
                    let mut moved = *entry;
                    moved.at += base;
                    if self.plant() == Plant::IndexOffByOne {
                        moved.at += ALIGN;
                    }
                    entries.push(moved);
                }
                base += batch.bytes.len() as u64;
                if base > OFFSET_MOST {
                    return Err(PutError::TooLong { key: Key(batch.entries.last().map_or(ContentId([0; 32]), |it| it.key)), len: base });
                }
                Ok(at)
            };
            if let Some(own) = own {
                let at = lay(own)?;
                if !own.bytes.is_empty() {
                    pieces.push((at, own.bytes.clone()));
                }
            }
            for batch in theirs {
                let at = lay(&batch)?;
                if !batch.bytes.is_empty() {
                    pieces.push((at, batch.bytes));
                }
            }
            if entries.is_empty() {
                continue;
            }
            let run = format::sorted_run(entries).map_err(PutError::TwoValues)?;
            out.push(Laid { pieces, index_at: base, run, namespace });
        }
        Ok(out)
    }

    /// Whether an open found what the next flip must write.
    fn pending_stamps(&self) -> bool {
        !self.checked.is_empty() || !self.dropped.is_empty() || !self.orphans.is_empty() || self.torn_stamp
    }

    fn what(&self) -> &'static str {
        match self.phase {
            Phase::New => "not opened",
            Phase::ReadHead(_) | Phase::MakeHead | Phase::ReadManifest { .. } | Phase::CheckLog { .. } | Phase::Orphans { .. } => "opening",
            Phase::Mapping { .. } | Phase::CheckMaps { .. } | Phase::Removing { .. } => "mapping a new generation",
            Phase::Ready => "ready",
            Phase::Listing => "compacting",
            Phase::Writing(..) | Phase::SyncingSegment(..) => "writing a segment",
            Phase::Flipping(_) => "flipping its head",
            Phase::Broken => "broken",
        }
    }

    /// Lays out a segment through the one writer of the layout ([`format::seal_parts`]: its
    /// header; the payloads of `pieces`, from [`HEADER`] to `index_at`; `run`; the footer and
    /// the digest of all before it), names it by its digest, and starts its write: the file
    /// made at its length, then each piece written at its offset, in parallel. A segment of the
    /// same bytes already here is not written again (a published file is never rewritten).
    fn write_segment(&mut self, laid: Laid, why: Why, effects: &mut Vec<Effect>) {
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
            self.sliced = Some(Sliced { segments: 0, ..*stats });
            self.phase = Phase::Ready;
            return;
        }
        if self.by_id.contains_key(&seg.id) {
            // These bytes are here already, under this name: flip without writing them.
            self.segment_written(Writing { seg, file, pieces: Vec::new_in(Global), left: 0 }, why, effects);
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
    pub fn adopt(&mut self, fetched: Vec<Segment>, effects: &mut Vec<Effect>) -> Result<(), PutError> {
        if !self.ready() {
            return Err(PutError::Busy(self.what()));
        }
        self.start_flip(Change::Adopt { segs: fetched }, effects);
        Ok(())
    }

    /// Marks segments as published (pushed to a shared bucket): compaction never rewrites them.
    pub fn publish(&mut self, ids: Vec<u64>, effects: &mut Vec<Effect>) -> Result<(), PutError> {
        if !self.ready() {
            return Err(PutError::Busy(self.what()));
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
        self.phase = Phase::Listing;
        effects.push(Effect::List { dir: self.cfg.dir.clone(), tag: self.tag(LIST, 0) });
        true
    }

    /// Which segments a slice replaces, and whether it prunes.
    fn plan(&self) -> (Vec<Segment>, bool) {
        let current = self.manifest.generation;
        let over = self.manifest.bytes() > self.cfg.cap_bytes;
        let local = self.manifest.segments.iter().filter(|it| it.local).count();
        let wanted = if over {
            1
        } else if local > SEGMENTS_MOST {
            2
        } else {
            return (Vec::new_in(Global), false);
        };
        // Pruning reads published segments too (it lets go of this machine's copy, rewriting
        // nothing); merging never does. One namespace a slice: no segment mixes two.
        let eligible = |it: &&Segment| it.local && (if over { it.generation + WINDOW <= current } else { !it.published });
        let Some(namespace) = self.manifest.segments.iter().find(eligible).map(|it| it.namespace) else {
            return (Vec::new_in(Global), false);
        };
        let mut out = Vec::new_in(Global);
        let mut bytes = 0u64;
        for seg in self.manifest.segments.iter().filter(eligible).filter(|it| it.namespace == namespace) {
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

    /// The keys roots name, and the roots' own records.
    fn rooted(&self) -> Result<BTreeSet<ContentId>, Corrupt> {
        let mut out = BTreeSet::new();
        for root in &self.manifest.roots {
            out.insert(root.key);
            let Some((at, entry)) = self.find_in(&[Namespace::LOCAL], &Key(root.key)) else {
                return Err(Corrupt { key: Key(root.key), pack: self.head.clone(), what: format!("the root `{}` names a record no segment holds", root.name) });
            };
            let bytes = self.maps[at].payload(&entry);
            for key in bytes.chunks_exact(32) {
                out.insert(ContentId(key.try_into().expect("32 bytes")));
            }
        }
        Ok(out)
    }

    fn listed_compact(&mut self, listed: Vec<(FileName, blaze_core::Meta)>, now_ns: u64, effects: &mut Vec<Effect>) {
        let current_log = self.slot.map(|it| it.log);
        let mut orphans = Vec::new_in(Global);
        for (name, _) in &listed {
            let leaf = name.as_str().rsplit('/').next().unwrap_or(name.as_str());
            let Some((id, log)) = parse_leaf(leaf) else { continue };
            let named = if log { Some(id) == current_log } else { self.manifest.segments.iter().any(|it| it.id == id) };
            let retired = self.manifest.retired.iter().any(|it| it.id == id && it.log == log);
            if !named && !retired {
                orphans.push((id, log));
            }
        }
        let (candidates, prune) = self.plan();
        let mut stats = Sliced { orphans: orphans.len() as u32, ..Sliced::default() };
        let expired = self.manifest.retired.iter().any(|it| it.since_ns.saturating_add(GRACE_NS) <= now_ns);
        if candidates.is_empty() {
            if orphans.is_empty() && !expired {
                self.sliced = Some(stats);
                self.phase = Phase::Ready;
                return;
            }
            self.start_flip(Change::Compact { seg: None, candidates: Vec::new_in(Global), orphans, stats }, effects);
            return;
        }
        let live = match prune.then(|| self.rooted()).transpose() {
            Ok(live) => live,
            Err(corrupt) => return self.stop(corrupt.to_string()),
        };
        let current = self.manifest.generation;
        // A published segment is let go of whole, or kept whole: it is never rewritten.
        let holds_live = |seg: &Segment| -> bool {
            let (Some(live), Some(&at)) = (live.as_ref(), self.by_id.get(&seg.id)) else { return true };
            let run = self.maps[at].run();
            (0..seg.count as usize).any(|i| live.contains(&format::key_at(run, i)))
        };
        let mut kept_candidates = Vec::with_capacity_in(candidates.len(), Global);
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
                let keep = live.as_ref().is_none_or(|live| young || live.contains(&entry.key));
                if !keep {
                    stats.dropped += 1;
                    continue;
                }
                if !seen.insert(entry.key) {
                    continue;
                }
                // Copied from a proved segment with its CRC as recorded: nothing is checked
                // or computed again.
                if let Err(error) = out.put_known(&entry, mapped.payload(&entry)) {
                    return self.stop(error.to_string());
                }
                stats.kept += 1;
            }
        }
        stats.segments = candidates.len() as u32;
        let mut ids: Vec<u64> = Vec::with_capacity_in(candidates.len(), Global);
        ids.extend(candidates.iter().map(|it| it.id));
        if out.is_empty() {
            self.start_flip(Change::Compact { seg: None, candidates: ids, orphans, stats }, effects);
            return;
        }
        let mut entries = std::mem::take(&mut out.entries);
        for entry in entries.iter_mut() {
            entry.at += HEADER as u64;
        }
        let run = match format::sorted_run(entries) {
            Ok(run) => run,
            Err(why) => return self.stop(why),
        };
        let index_at = HEADER as u64 + out.bytes.len() as u64;
        stats.bytes_out = index_at + run.len() as u64 + FOOTER as u64;
        self.orphans_for_compact = orphans;
        let mut pieces = Vec::with_capacity_in(1, Global);
        pieces.push((HEADER as u64, out.bytes));
        self.write_segment(Laid { pieces, index_at, run, namespace }, Why::Compact { candidates: ids, stats }, effects);
    }

    // ---- the flip: the one lock, held for it alone ----

    fn start_flip(&mut self, change: Change, effects: &mut Vec<Effect>) {
        self.phase = Phase::Flipping(Box::new(Flip {
            change,
            waits: 0,
            next: None,
            fresh: false,
            new_log: false,
            log: self.head.clone(),
            manifest_bytes: Vec::new_in(Global),
            removals: Vec::new_in(Global),
            refused: None,
            aborted: false,
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
                m.retired.push(Retired { id, log, since_ns: now_ns });
            }
        };
        for seg in m.segments.iter_mut() {
            if self.checked.contains(&seg.id) && self.plant() != Plant::ForgetCheck {
                seg.boot = self.cfg.boot;
            }
        }
        for id in &self.dropped {
            m.segments.retain(|it| it.id != *id);
            retire(&mut m, *id, false);
        }
        let current_log = base.slot.map(|it| it.log);
        let orphans: Vec<(u64, bool)> = std::mem::take(&mut self.orphans);
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
        match &mut flip.change {
            Change::Commit { segs, started_ns, roots } => {
                if now_ns.saturating_sub(*started_ns) > GRACE_NS / 2 {
                    refused = Some(format!(
                        "the commit started {} s ago, past half the store's grace period of {} s: a compaction may have retired its segment as named by no generation; it is not flipped",
                        now_ns.saturating_sub(*started_ns) / 1_000_000_000,
                        GRACE_NS / 1_000_000_000
                    ));
                } else {
                    for (seg, _) in segs.iter_mut() {
                        m.retired.retain(|it| !(it.id == seg.id && !it.log));
                        seg.generation = generation;
                        match m.segments.iter_mut().find(|it| it.id == seg.id) {
                            // The same bytes, named already: here again if they were not.
                            Some(named) => named.local = true,
                            None => m.segments.push(*seg),
                        }
                    }
                    for edit in roots.iter() {
                        match edit {
                            RootEdit::Set { name, key } => {
                                m.roots.retain(|it| it.name != *name);
                                m.roots.push(Root { name: name.clone(), key: key.0, generation });
                            }
                            RootEdit::Unset(name) => m.roots.retain(|it| it.name != *name),
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
                        Some(named) => *named = Segment { generation: named.generation, ..seg },
                        None => m.segments.push(seg),
                    }
                }
            }
            Change::Publish { ids } => {
                for seg in m.segments.iter_mut().filter(|it| ids.contains(&it.id)) {
                    seg.published = true;
                }
            }
            Change::Compact { seg, candidates, orphans, .. } => {
                if candidates.iter().any(|id| !m.segments.iter().any(|it| it.id == *id && it.local)) {
                    aborted = true;
                } else {
                    let first = m.segments.iter().position(|it| candidates.first() == Some(&it.id));
                    // A published candidate stays named, as not here: a source has it.
                    for it in m.segments.iter_mut().filter(|it| it.published && candidates.contains(&it.id)) {
                        it.local = false;
                    }
                    m.segments.retain(|it| it.published || !candidates.contains(&it.id));
                    if let Some((seg, _)) = seg {
                        m.retired.retain(|it| !(it.id == seg.id && !it.log));
                        // The copy is as old as the oldest it replaced: pruning sees its
                        // records' age, not the slice's.
                        let oldest = self.manifest.segments.iter().filter(|it| candidates.contains(&it.id)).map(|it| it.generation).min();
                        seg.generation = oldest.unwrap_or(generation);
                        m.segments.insert(first.unwrap_or(0).min(m.segments.len()), *seg);
                    }
                    for id in candidates.iter() {
                        retire(&mut m, *id, false);
                    }
                    adopt_orphans(&mut m, orphans);
                }
            }
        }
        if refused.is_some() || aborted {
            flip.refused = refused;
            flip.aborted = aborted;
            self.phase = Phase::Flipping(flip);
            self.unlock(effects);
            return;
        }
        // Files retired past their grace go now: out of the list, and unlinked once unlocked.
        let mut removals = Vec::new_in(Global);
        m.retired.retain(|it| {
            let keep = it.since_ns.saturating_add(GRACE_NS) > now_ns;
            if !keep {
                removals.push(if it.log { self.log_file(it.id) } else { self.segment_file(it.id) });
            }
            keep
        });
        // The log: a new one past its bound, the old one retired.
        let estimate = m.encode().len() as u64 + 24;
        let (log, at) = match base.slot {
            Some(slot) if slot.log_end() + estimate <= LOG_MOST => (slot.log, slot.log_end()),
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
        if flip.new_log {
            effects.push(Effect::Truncate { file: flip.log.clone(), len: 0, tag: self.tag(LOG_TRUNCATE, 0) });
            flip.manifest_bytes = bytes;
        } else {
            effects.push(Effect::WriteAt { file: flip.log.clone(), at, bytes, tag: self.tag(LOG_WRITE, 0) });
        }
        self.phase = Phase::Flipping(flip);
    }

    /// The flip's writes to the head, one after another, the slot last. The slot landing is
    /// the commit point: everything else a flip writes (a fresh head's cache tag and its
    /// other slot blanked) is written, and has succeeded, before it, so no write of a flip can
    /// fail once its generation is on disk. A flip that commits and then stops would leave a
    /// durable generation whose news (the losses an open found and retired, a commit's
    /// outcome) dies with the process (Hegel's seed 875559: an open's fresh flip retired the
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
            self.committed = Some(Err(why));
            if let Change::Commit { roots, .. } = flip.change {
                let later = std::mem::replace(&mut self.roots_pending, roots);
                self.roots_pending.extend(later);
            }
            self.sealed_back();
            self.phase = Phase::Ready;
            return;
        }
        if flip.aborted {
            let Change::Compact { seg, mut stats, .. } = flip.change else { unreachable!("only a compaction aborts") };
            stats.aborted = true;
            self.sliced = Some(stats);
            match seg {
                Some((_, file)) => {
                    // Never named: this process's own, removed now.
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
                Then::Commit
            }
            Change::Stamp | Change::Adopt { .. } | Change::Publish { .. } => Then::Stamp,
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
        if self.cfg.durable && self.plant() == Plant::FlipBeforeSync {
            for (i, file) in std::mem::take(&mut self.late_sync).into_iter().enumerate() {
                effects.push(Effect::Sync { file, tag: self.tag(LATE_SYNC, i as u32) });
            }
        }
        let left = flip.removals.len() as u32;
        for (i, file) in flip.removals.into_iter().enumerate() {
            effects.push(Effect::Remove { file, tag: self.tag(REMOVE, i as u32) });
        }
        if left == 0 {
            self.map_new(then, effects);
        } else {
            self.phase = Phase::Removing { left, then };
        }
    }

    /// A refused commit's held records are held again, for the next.
    fn sealed_back(&mut self) {
        let sealed = std::mem::take(&mut self.sealed);
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
    /// another boot) and its index touched a page at a time.
    fn on_map(&mut self, index: u32, done: Done) -> Result<(), String> {
        let seg = self.manifest.segments[index as usize];
        let file = self.segment_file(seg.id);
        let generation = self.manifest.generation;
        let earlier_boot = seg.boot != self.cfg.boot;
        let map = match done {
            Done::Mapped(map) => map,
            done => {
                let why = failed(&done).unwrap_or_else(|| format!("{done:?}"));
                if earlier_boot {
                    // Decided once every map is in, by whether the directory still holds it.
                    self.unopened.push((seg, file, why));
                    return Ok(());
                }
                return Err(format!("{file}: generation {generation} names it and it cannot be mapped: {why}; something outside blaze removed it"));
            }
        };
        // The proof: this boot's row needs no byte read; an earlier boot's segment is checked
        // in full once, here, and the open's next flip records the boot so no process of this
        // boot checks it again.
        let unverified = Unverified::new(seg, file, map);
        let proved = match Verified::recorded(unverified, self.boot) {
            Ok(proved) => Ok(proved),
            Err(unverified) if !earlier_boot => {
                return Err(format!(
                    "{}: it is {} bytes and generation {generation} of this boot names {}: changed outside blaze",
                    unverified.file(),
                    unverified.len(),
                    unverified.seg().file_len()
                ));
            }
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
                self.lose(seg, file, why);
                return Ok(());
            }
        };
        if earlier_boot {
            self.checked.insert(seg.id);
            self.report.checked += 1;
        }
        // Touch the index a page at a time, so no probe faults (the payloads are not touched).
        let run = proved.run();
        let mut touched = 0u8;
        for at in (0..run.len()).step_by(self.cfg.page.max(1) as usize) {
            touched ^= run[at];
        }
        std::hint::black_box(touched);
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

    fn lose(&mut self, seg: Segment, file: FileName, why: String) {
        self.dropped.insert(seg.id);
        self.report.lost.push(Lost { file, segment: Some(seg.id), records: Some(u64::from(seg.count)), why });
    }

    fn mapped(&mut self, then: Then, effects: &mut Vec<Effect>) {
        self.view.clear();
        for seg in self.manifest.segments.iter().rev() {
            if let Some(&at) = self.by_id.get(&seg.id) {
                self.view.push(at);
            }
        }
        match then {
            Then::Commit => {
                let segments = std::mem::replace(&mut self.committing, Vec::new_in(Global));
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segments }));
                self.sealed.clear();
            }
            Then::Compact(mut stats) => {
                let (next, _) = self.plan();
                stats.more = !next.is_empty();
                self.sliced = Some(stats);
            }
            Then::Open | Then::Stamp => {}
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

    fn step(&mut self, op: u8, index: u32, done: Done, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), String> {
        let unexpected = |phase: &str| format!("a completion of {op}:{index} while {phase}: the store did not ask for it");
        let fail = |what: &str, file: &FileName, done: &Done| failed(done).map(|why| format!("{file}: {what}: {why}"));
        match (op, &mut self.phase) {
            (HEAD_READ, Phase::ReadHead(ctx)) => {
                let ctx = *ctx;
                let Done::Read(bytes) = done else {
                    return Err(fail("reading the head", &self.head, &done).unwrap_or_else(|| unexpected("reading the head")));
                };
                let read = format::read_head(&bytes);
                if let HeadRead::Refused(why) = &read {
                    return Err(format!("{}: {why}", self.head));
                }
                if let HeadRead::Version { slot, found } = read {
                    // A store of the previous layout (rule 26): refused by name, never read as
                    // empty; there is no migration, the store is rebuilt from what made it.
                    return Err(format!(
                        "{}: its slot {slot} is of format {found}, and this blaze reads format {FORMAT}: a store of another layout; remove the directory and blaze rebuilds it",
                        self.head
                    ));
                }
                if bytes.len() < HEAD_LEN {
                    // A plain head (or nothing) short of its length: made at its full length
                    // and synced before anything else is written, so no flip ever extends or
                    // rewrites it. The cut extends with zeros and never touches a byte there.
                    if ctx == Ctx::Flip || self.head_made {
                        return Err(format!("{}: it is {} bytes after this process made it {HEAD_LEN} and synced it: it was cut outside blaze", self.head, bytes.len()));
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
                        effects.push(Effect::List { dir: self.cfg.dir.clone(), tag: self.tag(LIST, 0) });
                        self.phase = Phase::CheckLog { ctx, slots, at, why };
                        return Ok(());
                    }
                };
                let manifest = (bytes.len() == slot.manifest_len as usize && crc32c::crc32c(&bytes) == slot.manifest_crc)
                    .then(|| Manifest::decode(&bytes))
                    .transpose();
                match manifest {
                    Ok(Some(manifest)) if manifest.generation == slot.generation => {
                        if ctx == Ctx::Open {
                            self.report.whole = Some(manifest.generation);
                        }
                        self.resolved(ctx, Base { slot: Some(slot), manifest }, false, fell_back, now_ns, effects)
                    }
                    Err(why) => Err(format!("{log}: generation {}'s manifest at {}: {why}", slot.generation, slot.manifest_at)),
                    _ => self.manifest_torn(ctx, slots, at, now_ns, effects),
                }
            }
            (HEAD_MAKE, Phase::MakeHead) => {
                if let Some(why) = fail(if index == 0 { "making the head at its length" } else { "syncing the head made" }, &self.head, &done) {
                    return Err(why);
                }
                if index == 0 {
                    // Synced once in the store's life, before its first segment: so an empty or
                    // short head never stands beside segments a power cut kept.
                    effects.push(Effect::Sync { file: self.head.clone(), tag: self.tag(HEAD_MAKE, 1) });
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
                        self.report.lost.push(Lost { file, segment: Some(id), records: None, why: lost_orphan() });
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
                self.report.lost.push(Lost { file, segment: Some(id), records, why: lost_orphan() });
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
                        effects.push(Effect::List { dir: self.cfg.dir.clone(), tag: self.tag(LIST, 0) });
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
                for (seg, file, why) in std::mem::take(&mut self.unopened) {
                    let leaf = file.as_str().rsplit('/').next().unwrap_or(file.as_str());
                    if listed.iter().any(|(name, _)| name.as_str().rsplit('/').next() == Some(leaf)) {
                        return Err(format!("{file}: generation {generation} names it and it is here, and it cannot be mapped: {why}"));
                    }
                    let why = format!("generation {generation} names it, written in an earlier boot, and it is gone: a power cut before the kernel wrote its directory entry back; its records are dropped");
                    self.lose(seg, file, why);
                }
                self.mapped(then, effects);
                Ok(())
            }
            (REMOVE, Phase::Removing { left, then }) => {
                *left -= 1;
                let (left, then) = (*left, *then);
                if let Some(why) = failed(&done) {
                    return Err(format!("unlinking a file retired past its grace period: {why}"));
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
                let pieces = std::mem::take(&mut writing.pieces);
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
                        return Err(format!("{}: another process held the flip lock for {} s, and a flip holds it for milliseconds: it is stuck", self.flip, LOCK_STEP.as_millis() as u64 * u64::from(LOCK_STEPS_MOST) / 1000));
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
    fn manifest_torn(&mut self, ctx: Ctx, slots: [Option<Slot>; 2], at: usize, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), String> {
        let slot = slots[at].expect("read");
        if ctx == Ctx::Open {
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
        self.resolved(ctx, Base { slot: Some(newest), manifest }, false, true, now_ns, effects)
    }

    /// A generation to read (at the open) or to build on (under the lock).
    fn resolved(&mut self, ctx: Ctx, base: Base, fresh: bool, fell_back: bool, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), String> {
        match ctx {
            Ctx::Open => {
                self.slot = base.slot;
                self.manifest = base.manifest;
                if fell_back {
                    self.phase = Phase::Orphans { left: 0 };
                    effects.push(Effect::List { dir: self.cfg.dir.clone(), tag: self.tag(LIST, 0) });
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
    fn segment_written(&mut self, writing: Writing, why: Why, effects: &mut Vec<Effect>) {
        let seg = (writing.seg, writing.file);
        let change = match why {
            Why::Commit { started_ns, roots, mut rest, mut done } => {
                done.push(seg);
                if !rest.is_empty() {
                    let next = rest.remove(0);
                    self.write_segment(next, Why::Commit { started_ns, roots, rest, done }, effects);
                    return;
                }
                Change::Commit { segs: done, started_ns, roots }
            }
            Why::Compact { candidates, stats } => {
                Change::Compact { seg: Some(seg), candidates, orphans: std::mem::take(&mut self.orphans_for_compact), stats }
            }
        };
        self.start_flip(change, effects);
    }
}

fn lost_orphan() -> String {
    "no whole generation names it: the generation that did was lost in a power cut, and its records with it".to_owned()
}
