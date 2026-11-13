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
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use blaze_core::{Completion, Done, Effect, Extent, FileName, LockMode, Mapping, Tag};
use blaze_ids::{ContentId, crc32c};

use super::format::{
    self, ALIGN, ENTRY, Entry, FOOTER, HeadRead, Manifest, OFFSET_MOST, Retired, Root, SLOT_AT,
    Segment, Slot, aligned, record_crc,
};
use crate::{Corrupt, Key, Kind, Mismatch, content_of};

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
const SEGMENT_DOMAIN: &str = "blaze plain store segment 1";
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
}

/// A record read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Got<'a> {
    pub kind: Kind,
    pub bytes: &'a [u8],
}

/// Where a committed record lies, for a read into the caller's buffer ([`Plain::check`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Located {
    pub key: Key,
    pub kind: Kind,
    pub file: FileName,
    pub at: u64,
    pub len: u32,
    crc: u32,
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
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
}

/// A completed commit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Committed {
    pub generation: u64,
    /// The segment it wrote, when it held a record.
    pub segment: Option<u64>,
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
}

/// Puts one worker holds, laid out as they will lie in a segment: each payload at a multiple
/// of [`ALIGN`] of `bytes`. A worker fills its own and hands it to [`Plain::commit`]: no lock
/// and no shared word between workers.
#[derive(Default, Debug)]
pub struct Batch {
    bytes: Vec<u8>,
    entries: Vec<Entry>,
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    /// Keeps `value` under `key`. Two puts of one key with other bytes are refused at the
    /// commit, naming the key.
    pub fn put(&mut self, key: Key, kind: Kind, value: &[u8]) -> Result<(), PutError> {
        let at = self.bytes.len() as u64;
        let len = value.len() as u64;
        if len > u64::from(u32::MAX) || at + len > OFFSET_MOST {
            return Err(PutError::TooLong { key, len });
        }
        // alloc: the batch's buffer, which the commit hands to the write whole
        self.bytes.extend_from_slice(value);
        self.bytes.resize(aligned(at + len) as usize, 0);
        let crc = record_crc(&key, kind, value);
        self.entries.push(Entry { key, kind, at, len: len as u32, crc });
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

/// One segment this process mapped: kept mapped for the life of the process, never unmapped
/// during a run (an unmap is a TLB shootdown on every core).
struct Mapped {
    seg: Segment,
    file: FileName,
    map: Mapping,
}

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
    Commit { started_ns: u64, roots: Vec<RootEdit> },
    Compact { candidates: Vec<u64>, stats: Sliced },
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
    Commit { seg: Option<(Segment, FileName)>, started_ns: u64, roots: Vec<RootEdit> },
    /// Only what an open found (segments checked or lost): [`Plain::pending_stamps`].
    Stamp,
    Compact { seg: Option<(Segment, FileName)>, candidates: Vec<u64>, orphans: Vec<(u64, bool)>, stats: Sliced },
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
    /// The writes of a fresh head and the cache tag, both awaited.
    left: u32,
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
    /// The open fell back: which files no generation names, and what their footers hold.
    Orphans { left: u32 },
    /// Compaction's look at the directory.
    Listing,
    Mapping { left: u32, then: Then },
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
    Commit { segment: Option<u64> },
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
    maps: Vec<Mapped>,
    by_id: BTreeMap<u64, usize>,
    /// What [`Plain::get`] searches: this generation's mapped segments, newest first.
    view: Vec<usize>,
    /// The store's own batch, behind [`Plain::put`]: read back by [`Plain::get`] until the
    /// commit that writes it completes.
    held: Batch,
    held_at: BTreeMap<Key, usize>,
    /// A held batch being committed, still read by [`Plain::get`].
    sealed: Batch,
    sealed_at: BTreeMap<Key, usize>,
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
    /// The planted [`Plant::FlipBeforeSync`]'s segment, synced after the flip.
    late_sync: Option<FileName>,
}

fn tag_of(tags: u64, op: u8, index: u32) -> Tag {
    Tag(tags + (u64::from(op) << 32) + u64::from(index))
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
        let name = |leaf: &str| {
            // alloc: the store's names, once
            FileName::new(&format!("{}/{leaf}", cfg.dir.as_str())).expect("a store's directory names files under it")
        };
        Plain {
            head: name(HEAD),
            flip: name(FLIP),
            cfg,
            #[cfg(test)]
            plant: Plant::None,
            phase: Phase::New,
            slot: None,
            manifest: Manifest::default(),
            maps: Vec::new(),
            by_id: BTreeMap::new(),
            view: Vec::new(),
            held: Batch::new(),
            held_at: BTreeMap::new(),
            sealed: Batch::new(),
            sealed_at: BTreeMap::new(),
            roots_pending: Vec::new(),
            checked: BTreeSet::new(),
            dropped: BTreeSet::new(),
            orphans: Vec::new(),
            report: Report::default(),
            committed: None,
            sliced: None,
            broken: None,
            seq: 0,
            listed: Vec::new(),
            flip_parked: None,
            orphans_for_compact: Vec::new(),
            late_sync: None,
        }
    }

    fn name(&self, leaf: &str) -> FileName {
        // alloc: a file's name, once a file
        FileName::new(&format!("{}/{leaf}", self.cfg.dir.as_str())).expect("a store's directory names files under it")
    }

    fn segment_file(&self, id: u64) -> FileName {
        // alloc: as above
        self.name(&format!("seg-{id:016x}"))
    }

    fn log_file(&self, id: u64) -> FileName {
        // alloc: as above
        self.name(&format!("manifests-{id:016x}"))
    }

    fn tag(&self, op: u8, index: u32) -> Tag {
        tag_of(self.cfg.tags, op, index)
    }

    /// Whether `tag` is one of this store's.
    pub fn owns(&self, tag: Tag) -> bool {
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
        self.held.records()
    }

    /// The bytes this generation's segments hold.
    pub fn bytes(&self) -> u64 {
        self.manifest.bytes()
    }

    /// The segments this generation names, oldest first: `(id, records)`.
    pub fn segments(&self) -> Vec<(u64, u32)> {
        // alloc: a report
        self.manifest.segments.iter().map(|it| (it.id, it.count)).collect()
    }

    /// The roots this generation names: `(name, record key)`.
    pub fn roots(&self) -> Vec<(String, Key)> {
        // alloc: a report
        self.manifest.roots.iter().map(|it| (it.name.clone(), it.key)).collect()
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

    fn find_committed(&self, key: &Key) -> Result<Option<(usize, Entry)>, Corrupt> {
        for &at in &self.view {
            let mapped = &self.maps[at];
            let run = mapped.seg.run(mapped.map.bytes());
            if let Some(i) = format::find(run, key) {
                let entry = Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY])
                    .map_err(|what| Corrupt { key: *key, pack: mapped.file.clone(), what })?;
                if entry.at + u64::from(entry.len) > mapped.seg.data_len {
                    return Err(Corrupt {
                        key: *key,
                        pack: mapped.file.clone(),
                        // alloc: a refusal
                        what: format!("its entry places {} bytes at {}, past the segment's {} bytes of data", entry.len, entry.at, mapped.seg.data_len),
                    });
                }
                return Ok(Some((at, entry)));
            }
        }
        Ok(None)
    }

    fn checked_payload<'a>(&self, key: &Key, mapped: &'a Mapped, entry: &Entry) -> Result<&'a [u8], Corrupt> {
        let payload = &mapped.map.bytes()[entry.at as usize..(entry.at + u64::from(entry.len)) as usize];
        if record_crc(key, entry.kind, payload) != entry.crc {
            return Err(Corrupt {
                key: *key,
                pack: mapped.file.clone(),
                // alloc: a refusal
                what: format!("its {} bytes at {} fail their CRC-32C {:08x}: the record is refused, never returned", entry.len, entry.at, entry.crc),
            });
        }
        Ok(payload)
    }

    fn held_get(&self, key: &Key) -> Option<Got<'_>> {
        for (batch, at) in [(&self.held, &self.held_at), (&self.sealed, &self.sealed_at)] {
            if let Some(&i) = at.get(key) {
                let entry = &batch.entries[i];
                return Some(Got { kind: entry.kind, bytes: batch.payload(entry) });
            }
        }
        None
    }

    /// The record under `key`, read in place from its segment's mapping and checked against
    /// its CRC-32C; a record that fails it is refused, naming the key and the segment.
    pub fn get(&self, key: &Key) -> Result<Option<Got<'_>>, Corrupt> {
        if let Some(got) = self.held_get(key) {
            return Ok(Some(got));
        }
        match self.find_committed(key)? {
            None => Ok(None),
            Some((at, entry)) => {
                let bytes = self.checked_payload(key, &self.maps[at], &entry)?;
                Ok(Some(Got { kind: entry.kind, bytes }))
            }
        }
    }

    /// Where the committed record under `key` lies, for a read into the caller's buffer
    /// (`pread`), which [`Plain::check`] then checks. Records put and not committed are
    /// [`Plain::get`]'s.
    pub fn locate(&self, key: &Key) -> Result<Option<Located>, Corrupt> {
        Ok(self.find_committed(key)?.map(|(at, entry)| Located {
            key: *key,
            kind: entry.kind,
            file: self.maps[at].file.clone(),
            at: entry.at,
            len: entry.len,
            crc: entry.crc,
        }))
    }

    /// Checks bytes read from where [`Plain::locate`] said.
    pub fn check(located: &Located, bytes: &[u8]) -> Result<(), Corrupt> {
        if bytes.len() != located.len as usize || record_crc(&located.key, located.kind, bytes) != located.crc {
            return Err(Corrupt {
                key: located.key,
                pack: located.file.clone(),
                // alloc: a refusal
                what: format!("the {} bytes read at {} are not the {} its entry's CRC-32C {:08x} covers", bytes.len(), located.at, located.len, located.crc),
            });
        }
        Ok(())
    }

    pub fn has(&self, key: &Key) -> Result<bool, Corrupt> {
        Ok(self.held_get(key).is_some() || self.find_committed(key)?.is_some())
    }

    // ---- puts ----

    /// Keeps `value` under `key` in the store's own batch, written by the next commit. A key
    /// already held with the same record is a no-op; with other bytes, a refusal naming it.
    pub fn put(&mut self, key: Key, kind: Kind, value: &[u8]) -> Result<(), PutError> {
        let crc = record_crc(&key, kind, value);
        if let Some(got) = self.held_get(&key) {
            if got.kind == kind && got.bytes == value {
                return Ok(());
            }
            return Err(PutError::Mismatch(Mismatch { key, kept: content_of(got.bytes), given: content_of(value) }));
        }
        if let Some((at, entry)) = self.find_committed(&key).map_err(PutError::Corrupt)? {
            if entry.kind == kind && entry.len as usize == value.len() && entry.crc == crc {
                return Ok(());
            }
            let kept = self.checked_payload(&key, &self.maps[at], &entry).map_err(PutError::Corrupt)?;
            return Err(PutError::Mismatch(Mismatch { key, kept: content_of(kept), given: content_of(value) }));
        }
        self.held.put(key, kind, value)?;
        self.held_at.insert(key, self.held.entries.len() - 1);
        Ok(())
    }

    /// Names `keys` as kept under `name` at the next commit: a [`Kind::Pin`] record of the
    /// sorted keys, and the root. Compaction keeps every record a root names.
    pub fn root(&mut self, name: &str, keys: &[Key]) -> Result<Key, PutError> {
        // alloc: the root's record, once a root
        let mut sorted = keys.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        // alloc: as above
        let mut bytes = Vec::with_capacity(sorted.len() * 32);
        for key in &sorted {
            bytes.extend_from_slice(&key.0.0);
        }
        let key = Key(ContentId::of(ROOT_DOMAIN, &bytes));
        self.put(key, Kind::Pin, &bytes)?;
        // alloc: one a root
        self.roots_pending.push(RootEdit::Set { name: name.to_owned(), key });
        Ok(key)
    }

    /// Lets go of the root `name` at the next commit.
    pub fn unroot(&mut self, name: &str) {
        // alloc: as above
        self.roots_pending.push(RootEdit::Unset(name.to_owned()));
    }

    // ---- the commit ----

    /// Writes the store's batch and `batches` (the workers', handed off) as one new segment,
    /// with no lock, then flips the head to a generation naming it. The outcome is
    /// [`Plain::take_committed`]'s once the store is ready again.
    pub fn commit(&mut self, batches: Vec<Batch>, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), PutError> {
        if !self.ready() {
            return Err(PutError::Busy(self.what()));
        }
        // The held batch stays readable while it is written: sealed until the commit ends.
        self.sealed = std::mem::take(&mut self.held);
        self.sealed_at = std::mem::take(&mut self.held_at);
        let laid = self.lay_out(batches, now_ns, effects);
        if laid.is_err() {
            self.sealed_back();
        }
        laid
    }

    fn lay_out(&mut self, batches: Vec<Batch>, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), PutError> {
        let mut sources: Vec<&Batch> = Vec::with_capacity(batches.len() + 1);
        sources.push(&self.sealed);
        sources.extend(batches.iter());
        // alloc: once a commit
        let mut entries: Vec<Entry> = Vec::new();
        // alloc: as above
        let mut pieces: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut base = 0u64;
        for (n, batch) in sources.iter().enumerate() {
            for entry in &batch.entries {
                if let Some((at, kept)) = self.find_committed(&entry.key).map_err(PutError::Corrupt)? {
                    if kept.same_record(entry) {
                        // Its bytes stay in the segment unreferenced; compaction drops them.
                        continue;
                    }
                    let kept = self.checked_payload(&entry.key, &self.maps[at], &kept).map_err(PutError::Corrupt)?;
                    return Err(PutError::Mismatch(Mismatch { key: entry.key, kept: content_of(kept), given: content_of(batch.payload(entry)) }));
                }
                let mut moved = *entry;
                moved.at += base;
                if self.plant() == Plant::IndexOffByOne {
                    moved.at += ALIGN;
                }
                entries.push(moved);
            }
            if !batch.bytes.is_empty() {
                // The store's own batch is copied (it is read until the commit ends); a
                // worker's is moved.
                let bytes = if n == 0 { batch.bytes.clone() } else { Vec::new() };
                pieces.push((base, bytes));
            }
            base += batch.bytes.len() as u64;
            if base > OFFSET_MOST {
                return Err(PutError::TooLong { key: batch.entries.last().map(|it| it.key).unwrap_or(Key(ContentId([0; 32]))), len: base });
            }
        }
        // Move the workers' buffers into their pieces, in the order they were laid out.
        let mut moved = batches.into_iter().filter(|it| !it.bytes.is_empty());
        for piece in pieces.iter_mut().skip(usize::from(!self.sealed.bytes.is_empty())) {
            piece.1 = moved.next().expect("a piece for each batch with bytes").bytes;
        }
        let stamps = self.pending_stamps();
        if entries.is_empty() {
            if self.roots_pending.is_empty() && !stamps {
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segment: None }));
                self.sealed = Batch::new();
                self.sealed_at.clear();
                return Ok(());
            }
            let roots = std::mem::take(&mut self.roots_pending);
            self.start_flip(Change::Commit { seg: None, started_ns: now_ns, roots }, effects);
            return Ok(());
        }
        let run = format::sorted_run(entries).map_err(PutError::TwoValues)?;
        // The roots this commit's records were put with: one set after it started is the next's.
        let roots = std::mem::take(&mut self.roots_pending);
        self.write_segment(pieces, base, run, Why::Commit { started_ns: now_ns, roots }, now_ns, effects);
        Ok(())
    }

    /// Whether an open found what the next flip must write.
    fn pending_stamps(&self) -> bool {
        !self.checked.is_empty() || !self.dropped.is_empty() || !self.orphans.is_empty()
    }

    fn what(&self) -> &'static str {
        match self.phase {
            Phase::New => "not opened",
            Phase::ReadHead(_) | Phase::ReadManifest { .. } | Phase::Orphans { .. } => "opening",
            Phase::Mapping { .. } | Phase::Removing { .. } => "mapping a new generation",
            Phase::Ready => "ready",
            Phase::Listing => "compacting",
            Phase::Writing(..) | Phase::SyncingSegment(..) => "writing a segment",
            Phase::Flipping(_) => "flipping its head",
            Phase::Broken => "broken",
        }
    }

    /// Lays out a segment (payloads at `pieces`, `data_len` of them, then `run` and the
    /// footer), and starts its write: the file made at its length, then each piece written
    /// at its offset, in parallel.
    fn write_segment(&mut self, mut pieces: Vec<(u64, Vec<u8>)>, data_len: u64, run: Vec<u8>, why: Why, now_ns: u64, effects: &mut Vec<Effect>) {
        let count = (run.len() / ENTRY) as u32;
        let footer = format::footer(count, data_len);
        let mut crc = 0u32;
        let mut at = 0u64;
        for (piece_at, bytes) in &pieces {
            debug_assert_eq!(*piece_at, at, "pieces lie end to end");
            crc = crc32c::extend(crc, bytes);
            at += bytes.len() as u64;
        }
        crc = crc32c::extend(crc32c::extend(crc, &run), &footer);
        self.seq += 1;
        let id = format::id_of(SEGMENT_DOMAIN, &[&self.cfg.nonce.to_le_bytes(), &self.seq.to_le_bytes(), &now_ns.to_le_bytes()]);
        let seg = Segment { id, data_len, count, crc, generation: 0, boot: self.cfg.boot };
        pieces.push((data_len, run));
        pieces.push((data_len + u64::from(count) * ENTRY as u64, footer.to_vec()));
        let file = self.segment_file(id);
        effects.push(Effect::Truncate { file: file.clone(), len: seg.file_len(), tag: self.tag(SEG_TRUNCATE, 0) });
        self.phase = Phase::Writing(Box::new(Writing { seg, file, pieces, left: 0 }), Box::new(why));
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
        let old_enough = |it: &&Segment| !over || it.generation + WINDOW <= current;
        let wanted = if over {
            1
        } else if self.manifest.segments.len() > SEGMENTS_MOST {
            2
        } else {
            return (Vec::new(), false);
        };
        // alloc: once a slice
        let mut out = Vec::new();
        let mut bytes = 0u64;
        for seg in self.manifest.segments.iter().filter(old_enough) {
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
    fn rooted(&self) -> Result<BTreeSet<Key>, Corrupt> {
        let mut out = BTreeSet::new();
        for root in &self.manifest.roots {
            out.insert(root.key);
            let Some((at, entry)) = self.find_committed(&root.key)? else {
                return Err(Corrupt {
                    key: root.key,
                    pack: self.head.clone(),
                    // alloc: a refusal
                    what: format!("the root `{}` names a record no segment holds", root.name),
                });
            };
            let bytes = self.checked_payload(&root.key, &self.maps[at], &entry)?;
            for key in bytes.chunks_exact(32) {
                out.insert(Key(ContentId(key.try_into().expect("32 bytes"))));
            }
        }
        Ok(out)
    }

    fn listed_compact(&mut self, listed: Vec<(FileName, blaze_core::Meta)>, now_ns: u64, effects: &mut Vec<Effect>) {
        let current_log = self.slot.map(|it| it.log);
        // alloc: once a slice
        let mut orphans = Vec::new();
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
            self.start_flip(Change::Compact { seg: None, candidates: Vec::new(), orphans, stats }, effects);
            return;
        }
        let live = match prune.then(|| self.rooted()).transpose() {
            Ok(live) => live,
            Err(corrupt) => return self.stop(corrupt.to_string()),
        };
        let current = self.manifest.generation;
        let mut out = Batch::new();
        let mut seen = BTreeSet::new();
        for seg in &candidates {
            let mapped = &self.maps[self.by_id[&seg.id]];
            stats.bytes_in += seg.file_len();
            let young = seg.generation + WINDOW > current;
            let run = seg.run(mapped.map.bytes());
            for i in 0..seg.count as usize {
                let entry = match Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY]) {
                    Ok(entry) => entry,
                    // alloc: a refusal
                    Err(what) => return self.stop(format!("{}: entry {i}: {what}", mapped.file)),
                };
                let keep = live.as_ref().is_none_or(|live| young || live.contains(&entry.key));
                if !keep {
                    stats.dropped += 1;
                    continue;
                }
                if !seen.insert(entry.key) {
                    continue;
                }
                let payload = match self.checked_payload(&entry.key, mapped, &entry) {
                    Ok(payload) => payload,
                    Err(corrupt) => return self.stop(corrupt.to_string()),
                };
                if let Err(error) = out.put(entry.key, entry.kind, payload) {
                    return self.stop(error.to_string());
                }
                stats.kept += 1;
            }
        }
        stats.segments = candidates.len() as u32;
        // alloc: once a slice
        let ids: Vec<u64> = candidates.iter().map(|it| it.id).collect();
        if out.is_empty() {
            self.start_flip(Change::Compact { seg: None, candidates: ids, orphans, stats }, effects);
            return;
        }
        let run = match format::sorted_run(std::mem::take(&mut out.entries)) {
            Ok(run) => run,
            Err(why) => return self.stop(why),
        };
        let data_len = out.bytes.len() as u64;
        stats.bytes_out = data_len + run.len() as u64 + FOOTER as u64;
        self.orphans_for_compact = orphans;
        self.write_segment(vec![(0, out.bytes)], data_len, run, Why::Compact { candidates: ids, stats }, now_ns, effects);
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
            manifest_bytes: Vec::new(),
            removals: Vec::new(),
            left: 0,
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
            if self.checked.contains(&seg.id) {
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
            Change::Commit { seg, started_ns, roots } => {
                if now_ns.saturating_sub(*started_ns) > GRACE_NS / 2 {
                    // alloc: a refusal
                    refused = Some(format!(
                        "the commit started {} s ago, past half the store's grace period of {} s: a compaction may have retired its segment as named by no generation; it is not flipped",
                        now_ns.saturating_sub(*started_ns) / 1_000_000_000,
                        GRACE_NS / 1_000_000_000
                    ));
                } else {
                    if let Some((seg, _)) = seg {
                        m.retired.retain(|it| !(it.id == seg.id && !it.log));
                        seg.generation = generation;
                        m.segments.push(*seg);
                    }
                    for edit in roots.iter() {
                        match edit {
                            RootEdit::Set { name, key } => {
                                m.roots.retain(|it| it.name != *name);
                                m.roots.push(Root { name: name.clone(), key: *key, generation });
                            }
                            RootEdit::Unset(name) => m.roots.retain(|it| it.name != *name),
                        }
                    }
                }
            }
            Change::Stamp => {}
            Change::Compact { seg, candidates, orphans, .. } => {
                if candidates.iter().any(|id| !m.segments.iter().any(|it| it.id == *id)) {
                    aborted = true;
                } else {
                    let first = m.segments.iter().position(|it| candidates.first() == Some(&it.id));
                    m.segments.retain(|it| !candidates.contains(&it.id));
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
            effects.push(Effect::Unlock { lock: self.tag(LOCK, 0), tag: self.tag(UNLOCK, 0) });
            return;
        }
        // Files retired past their grace go now: out of the list, and unlinked once unlocked.
        let mut removals = Vec::new();
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

    fn write_slot(&mut self, effects: &mut Vec<Effect>) {
        let Phase::Flipping(flip) = &mut self.phase else { unreachable!("a slot is a flip's") };
        let slot = flip.next.as_ref().expect("built").0;
        let fresh = flip.fresh;
        flip.left = if fresh { 2 } else { 1 };
        if fresh {
            let head = format::head_with(&slot);
            let tag_file = self.name(CACHEDIR_TAG);
            if self.cfg.durable {
                effects.push(Effect::Write { file: self.head.clone(), bytes: head, tag: self.tag(SLOT_WRITE, 0) });
            } else {
                effects.push(Effect::WriteUnsynced { file: self.head.clone(), bytes: head, tag: self.tag(SLOT_WRITE, 0) });
            }
            effects.push(Effect::WriteUnsynced { file: tag_file, bytes: CACHEDIR_SIGNATURE.to_vec(), tag: self.tag(SLOT_WRITE, 1) });
        } else {
            let at = SLOT_AT[(slot.generation % 2) as usize];
            effects.push(Effect::WriteAt { file: self.head.clone(), at, bytes: slot.encode().to_vec(), tag: self.tag(SLOT_WRITE, 0) });
        }
    }

    /// The flip's writes are done (or it was refused): let go of the lock.
    fn unlock(&mut self, effects: &mut Vec<Effect>) {
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
            Change::Commit { seg, .. } => Then::Commit { segment: seg.map(|(it, _)| it.id) },
            Change::Stamp => Then::Stamp,
            Change::Compact { mut stats, .. } => {
                stats.removed = flip.removals.len() as u32;
                Then::Compact(stats)
            }
        };
        self.checked.clear();
        self.dropped.clear();
        self.slot = Some(slot);
        self.manifest = manifest;
        if self.cfg.durable && self.plant() == Plant::FlipBeforeSync {
            if let Some(file) = self.late_sync.take() {
                effects.push(Effect::Sync { file, tag: self.tag(LATE_SYNC, 0) });
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
        self.sealed_at.clear();
        for entry in &sealed.entries {
            if self.held_at.contains_key(&entry.key) {
                continue;
            }
            self.held.put(entry.key, entry.kind, sealed.payload(entry)).expect("it fit before");
            self.held_at.insert(entry.key, self.held.entries.len() - 1);
        }
    }

    // ---- adopting a generation ----

    /// Maps each segment of this process's manifest it has not mapped; then what `then` says.
    fn map_new(&mut self, then: Then, effects: &mut Vec<Effect>) {
        let mut left = 0;
        for (i, seg) in self.manifest.segments.iter().enumerate() {
            if self.by_id.contains_key(&seg.id) || self.dropped.contains(&seg.id) {
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
        let check_crc = earlier_boot && self.plant() != Plant::TrustBoot;
        let map = match done {
            Done::Mapped(map) => map,
            done => {
                let why = failed(&done).unwrap_or_else(|| format!("{done:?}"));
                if earlier_boot {
                    // alloc: a report
                    let why = format!("generation {generation} names it, written in an earlier boot, and it cannot be opened: {why}");
                    self.lose(seg, file, why);
                    return Ok(());
                }
                // alloc: a refusal
                return Err(format!("{file}: generation {generation} names it and it cannot be mapped: {why}; something outside blaze removed it"));
            }
        };
        let len = map.bytes().len() as u64;
        let whole = len == seg.file_len() && (!check_crc || crc32c::crc32c(map.bytes()) == seg.crc);
        if !whole {
            if earlier_boot {
                // alloc: a report
                let why = format!("it was written in an earlier boot and did not survive its end whole ({len} bytes, {} named, or its CRC-32C fails): a power cut before the kernel wrote it back; its records are dropped", seg.file_len());
                self.lose(seg, file, why);
                return Ok(());
            }
            // alloc: a refusal
            return Err(format!("{file}: it is {len} bytes and generation {generation} names {}: changed outside blaze", seg.file_len()));
        }
        let bytes = map.bytes();
        if seg.boot != self.cfg.boot {
            self.checked.insert(seg.id);
            self.report.checked += 1;
        }
        // Touch the index a page at a time, so no probe faults (the payloads are not touched).
        let run = seg.run(bytes);
        let mut touched = 0u8;
        for at in (0..run.len()).step_by(self.cfg.page.max(4096) as usize) {
            touched ^= run[at];
        }
        std::hint::black_box(touched);
        self.by_id.insert(seg.id, self.maps.len());
        self.maps.push(Mapped { seg, file, map });
        Ok(())
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
            Then::Commit { segment } => {
                self.committed = Some(Ok(Committed { generation: self.manifest.generation, segment }));
                self.sealed = Batch::new();
                self.sealed_at.clear();
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
        if let Err(why) = self.step(op, index, completion.done, now_ns, effects) {
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
                match format::read_head(&bytes) {
                    HeadRead::Refused(why) => Err(format!("{}: {why}", self.head)),
                    HeadRead::Fresh => self.resolved(ctx, Base { slot: None, manifest: Manifest::default() }, true, false, now_ns, effects),
                    HeadRead::Committed(newest, older) => {
                        if ctx == Ctx::Flip && self.plant() == Plant::StaleBase && self.slot.is_some() {
                            let base = Base { slot: self.slot, manifest: self.manifest.clone() };
                            return self.resolved(ctx, base, false, false, now_ns, effects);
                        }
                        if ctx == Ctx::Flip && Some(newest) == self.slot {
                            // Nothing flipped since this process last read: its manifest is the base.
                            let base = Base { slot: self.slot, manifest: self.manifest.clone() };
                            return self.resolved(ctx, base, false, false, now_ns, effects);
                        }
                        self.read_manifest(ctx, [Some(newest), older], 0, false, effects);
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
                    // A log that is gone after a power cut reads as nothing: the same as torn.
                    done => fail("reading a manifest", &log, &done).map(|_| Vec::new()).ok_or_else(|| unexpected("reading a manifest"))?,
                };
                let manifest = (bytes.len() == slot.manifest_len as usize && crc32c::crc32c(&bytes) == slot.manifest_crc)
                    .then(|| Manifest::decode(&bytes))
                    .transpose();
                match manifest {
                    Ok(Some(manifest)) if manifest.generation == slot.generation => {
                        self.resolved(ctx, Base { slot: Some(slot), manifest }, false, fell_back, now_ns, effects)
                    }
                    Err(why) => Err(format!("{log}: generation {}'s manifest at {}: {why}", slot.generation, slot.manifest_at)),
                    _ => {
                        if ctx == Ctx::Open {
                            self.report.lost.push(Lost {
                                file: log,
                                segment: None,
                                records: None,
                                // alloc: a report
                                why: format!("generation {}'s manifest at {} is not whole: a power cut before the kernel wrote it back; the generation before is read", slot.generation, slot.manifest_at),
                            });
                        }
                        if at == 0 && slots[1].is_some() {
                            self.read_manifest(ctx, slots, 1, true, effects);
                            return Ok(());
                        }
                        // Neither generation survived: everything is lost, said by name below,
                        // and the next generation follows the newest number so its slot is right.
                        let newest = slots[0].expect("read");
                        let manifest = Manifest { generation: newest.generation, ..Manifest::default() };
                        self.resolved(ctx, Base { slot: Some(newest), manifest }, false, true, now_ns, effects)
                    }
                }
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
                    self.mapped(then, effects);
                }
                Ok(())
            }
            (REMOVE, Phase::Removing { left, then }) => {
                *left -= 1;
                let (left, then) = (*left, *then);
                if let Some(why) = failed(&done) {
                    // alloc: a refusal
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
                    self.late_sync = Some(writing.file.clone());
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
                    self.phase_ctx_read_head(effects);
                    Ok(())
                }
                Done::Contended => {
                    flip.waits += 1;
                    if flip.waits > LOCK_STEPS_MOST {
                        // alloc: a refusal
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
            (SLOT_WRITE, Phase::Flipping(flip)) => {
                if let Some(why) = fail("writing the head", &self.head, &done) {
                    return Err(why);
                }
                flip.left -= 1;
                if flip.left > 0 {
                    return Ok(());
                }
                if self.cfg.durable && !flip.fresh {
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

    fn segment_written(&mut self, writing: Writing, why: Why, effects: &mut Vec<Effect>) {
        let seg = Some((writing.seg, writing.file));
        let change = match why {
            Why::Commit { started_ns, roots } => Change::Commit { seg, started_ns, roots },
            Why::Compact { candidates, stats } => {
                Change::Compact { seg, candidates, orphans: std::mem::take(&mut self.orphans_for_compact), stats }
            }
        };
        self.start_flip(change, effects);
    }
}

fn lost_orphan() -> String {
    // alloc: a report
    "no whole generation names it: the generation that did was lost in a power cut, and its records with it".to_owned()
}
