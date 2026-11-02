//! `blaze why`: what the last build computed again instead of reusing, and why
//! (docs/v2/plan/lanes/why.md). A sans-IO core (rule 12): [`Ledger`] takes the misses a
//! build gave back with what it built (`Ended::misses`, its own and no other build's), names
//! each one's cause against what it holds of the subject's last compute, and hands back the
//! answer, the history's rows and the record to append to its file; [`Answers`] keeps each
//! workspace's last answer for the loop to answer `blaze why` from. [`file`] drives the ledger
//! on the `blaze-history` thread: the restore, the explaining, the appends and the compaction
//! are all there, none on the daemon's start path or its loop.
//!
//! - **Keyed by subject** ([`blaze_kernel::Miss::subject`]): what an item is across edits,
//!   telling apart instances side by side (one function built for a test and for a program,
//!   one crate built in two workspaces). Its name is for a person only. Two different misses of
//!   one step under one subject in one build are refused ([`RebuildCause::Twice`]) and
//!   compared with nothing (rule 18).
//! - **A miss with parts** (a key made of named parts: a dev template's toolchain, code,
//!   settings and callees) is told apart against the parts the subject had when last computed
//!   by a build that ended well: the parts that changed by name ([`RebuildCause::Parts`]);
//!   the same key, so nothing is kept under it or what it is filed under past the key differs
//!   ([`RebuildCause::Lost`], [`RebuildCause::Beyond`]); a key that changed while no named part
//!   did ([`RebuildCause::Unnamed`]); or no record of it ([`RebuildCause::First`]). Builds
//!   between may have reused it under another key, so each cause says the build it is
//!   compared with. A record from before a build whose computes did not all reach the file (a
//!   gap: [`Ledger::behind`]) is compared with nothing, and says so.
//! - **A miss the kernel explained** (a read that changed) is its reason as the kernel gave it,
//!   with the build that kept the entry it was compared with.
//! - **The file** ([`LEDGER`]): a magic naming its version, then framed records (`u32`
//!   little-endian length, then deterministic CBOR), one a build that ended well: the subjects
//!   it computed with their parts. Only types this module owns are written, and the version is
//!   a hash of the encoding of a record with every field set and of [`NAMING`] ([`version`]),
//!   so a change of any of them is a new version and an old file is moved aside, never misread.
//!   Replayed in order, later records winning; compacted to one record once the appends pass
//!   the live size, the oldest subjects dropped past [`CAP_BYTES`] ([`Ledger::prune`]).

use std::collections::BTreeMap;

use blaze_api::{PartChanged, Rebuild, RebuildCause, RebuildGroup, Rebuilt};
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{Miss, Missed, Part, Seen};
use blaze_profile::facts::MissRow;
use blaze_protocol::cbor;
use serde::{Deserialize, Serialize};

pub mod file;

/// The ledger's file in the profile history's directory.
pub const LEDGER: &str = "why.ledger";

/// The first half of the file's magic; the second is its version ([`version`]).
const MAGIC: [u8; 16] = *b"blaze why ledger";

/// How many items a group lists unless every one is asked for.
const LISTED: usize = 8;

/// About how many bytes of subjects the ledger keeps ([`Ledger::prune`]): one per `BLAZE_HOME`
/// holds every workspace's, so it is bounded by size, the subjects last computed longest ago
/// dropped first.
pub const CAP_BYTES: usize = 64 << 20;

/// The file is compacted once it is past this and twice the size of its last whole write.
pub const COMPACT_MIN: u64 = 4 << 20;

/// How many workspaces' last answers the loop keeps ([`Answers`]), the oldest build's dropped.
const WORKSPACES: usize = 16;

/// What a subject's parts are named and what its subject is made of: bumped when a part's
/// name or a subject's makeup changes (`compile::unit_subject`, `FnNode::subject`, the part
/// names in `blaze-codegen`'s driver and `compile::RecordInputs::parts`), so an old file's
/// records are not compared with new ones under other names.
const NAMING: u32 = 2;

/// What the ledger holds of a subject: its key and the parts of it when it was last computed
/// by a build that ended well. Fields in the order deterministic CBOR sorts their names.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Last {
    key: ContentId,
    /// The entry the kernel kept for it then, for a step the kernel computes.
    kept: Option<ContentId>,
    step: String,
    build: u64,
    parts: Vec<Kept>,
    beyond: Option<String>,
    subject: ContentId,
}

/// A part of a key, as the file keeps it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Kept {
    hash: Option<ContentId>,
    name: String,
    names: Option<String>,
}

/// One record in the file: a build's subjects, or the whole ledger once compacted.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Record {
    /// The first build whose computes did not all reach the file ([`Ledger::behind`]).
    gap: Option<u64>,
    build: u64,
    items: Vec<Last>,
    /// The newest build a subject dropped to keep the ledger under its cap was last computed in.
    dropped: Option<u64>,
}

/// What `blaze why`'s answers are told apart against: each subject's last compute, by step and
/// subject. A `BTreeMap`, as the file is written in its order (rule 22).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ledger {
    items: BTreeMap<(String, ContentId), Last>,
    /// Records from builds before this one may lack a later compute of their subject: a build's
    /// record did not reach the file (an append failed, or the daemon stopped mid-append).
    gap: Option<u64>,
    dropped: Option<u64>,
}

/// A build's misses explained: what it computed again, sorted by item, then step, so an item's
/// reasons are found by a binary search; the history's rows; and the record to append, empty
/// when there is none.
pub struct Explained {
    pub rebuilt: Vec<Rebuild>,
    pub rows: Vec<MissRow>,
    pub record: Vec<u8>,
}

/// A file replayed: the ledger it holds, how many records it had, and what was wrong with it.
#[derive(Debug)]
pub struct Restored {
    pub ledger: Ledger,
    pub records: usize,
    pub fault: Option<Fault>,
}

/// What was wrong with a ledger file, and so what its driver does with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fault {
    /// None or part of the magic: a file made and never written whole. Read as empty and cut to
    /// nothing; nothing was lost.
    Unbegun { len: usize },
    /// The last record is cut short with nothing after it, or what follows the last whole
    /// record is all zeros, as a crash during an append leaves it: what came before was read,
    /// and the file is cut back to its first `keep` bytes, what was cut copied aside first.
    Tail { keep: usize, why: String },
    /// Another version's magic, or no ledger's: nothing of it was read, and it is moved aside.
    Version { why: String },
    /// A record whole by its length that does not decode: what came before was read, and the
    /// file is moved aside.
    Corrupt { why: String },
}

impl Ledger {
    /// `build`'s misses (sorted and each once), each told apart against the ledger as it was
    /// before the build. A build that `ended_well` moves the ledger on: its subjects' parts are
    /// what the next build is compared with, and the record to append to the file is given;
    /// any other build's computes may not have been kept, so it records nothing.
    pub fn explain(&mut self, build: u64, ended_well: bool, misses: Vec<Miss>) -> Explained {
        // How many different misses each (step, subject) has: more than one is refused. Only
        // a miss compared by its subject counts; one that failed has no subject to speak of.
        // alloc: once a build, an entry a subject that missed
        let mut twice: BTreeMap<(&str, ContentId), u32> = BTreeMap::new();
        for miss in misses.iter().filter(|it| compared(it)) {
            *twice.entry((miss.step, miss.subject)).or_default() += 1;
        }
        let count = |miss: &Miss| twice.get(&(miss.step, miss.subject)).copied().filter(|n| *n > 1);
        // alloc: once a build, an item each miss
        let mut rebuilt = Vec::with_capacity(misses.len());
        for miss in &misses {
            let cause = match count(miss).filter(|_| compared(miss)) {
                Some(count) => RebuildCause::Twice { count },
                None => self.cause(miss),
            };
            // clone: once a miss
            rebuilt.push(Rebuild { item: miss.item.clone(), step: miss.step.to_owned(), cause });
        }
        rebuilt.sort_by(|a, b| (&a.item, &a.step).cmp(&(&b.item, &b.step)));
        // alloc: as above
        let mut items = Vec::new();
        if ended_well {
            let refused: Vec<bool> = misses.iter().map(|it| count(it).is_some()).collect(); // alloc: once a build
            for (miss, refused) in misses.into_iter().zip(refused) {
                if refused || !compared(&miss) {
                    continue;
                }
                let parts = miss.parts.into_iter().map(|Part { name, hash, names }| Kept { hash, name, names }).collect();
                let last = Last {
                    key: miss.key,
                    kept: miss.kept,
                    step: miss.step.to_owned(),
                    build,
                    parts,
                    beyond: miss.beyond.map(str::to_owned),
                    subject: miss.subject,
                };
                // clone: the map's key and the record's copy, once a miss
                self.items.insert((last.step.clone(), last.subject), last.clone());
                items.push(last);
            }
        }
        let rows = rows(build, &rebuilt);
        let record = match items.is_empty() {
            true => Vec::new(),
            false => frame(&Record { gap: None, build, items, dropped: None }),
        };
        Explained { rebuilt, rows, record }
    }

    /// The ledger the file `bytes` holds, records replayed in order, and what was wrong with
    /// it ([`Fault`]): its driver cuts a file back or moves it aside, never writes over what it
    /// could not read.
    pub fn restore(bytes: &[u8]) -> Restored {
        let mut ledger = Ledger::default();
        let magic = magic();
        let Some(mut rest) = bytes.strip_prefix(&magic) else {
            let fault = match bytes {
                _ if magic.starts_with(bytes) => Fault::Unbegun { len: bytes.len() },
                _ if bytes.starts_with(&MAGIC) => Fault::Version { why: "it is another version's ledger".to_owned() }, // alloc: error path
                _ => Fault::Version {
                    // alloc: error path
                    why: format!("its first {} bytes are no ledger's magic", bytes.len().min(magic.len())),
                },
            };
            return Restored { ledger, records: 0, fault: Some(fault) };
        };
        let mut records = 0usize;
        while !rest.is_empty() {
            let keep = bytes.len() - rest.len();
            // alloc: error path
            let why = |why: String| format!("record {records}, {} bytes from the end: {why}", rest.len());
            // A file grown before its data landed (a crash during an append, on a file system
            // that extends first) reads as zeros: a torn append, not a corrupt record.
            if rest.iter().all(|it| *it == 0) {
                let why = why("it and all after it are zeros".to_owned()); // alloc: error path
                return Restored { ledger, records, fault: Some(Fault::Tail { keep, why }) };
            }
            let Some((len, body)) = rest.split_first_chunk::<4>() else {
                let why = why("its length is cut short".to_owned()); // alloc: error path
                return Restored { ledger, records, fault: Some(Fault::Tail { keep, why }) };
            };
            let len = u32::from_le_bytes(*len) as usize;
            let Some((item, after)) = body.split_at_checked(len) else {
                let why = why(format!("it says {len} bytes, and {} are left", body.len())); // alloc: error path
                return Restored { ledger, records, fault: Some(Fault::Tail { keep, why }) };
            };
            let record: Record = match cbor::from_slice(item) {
                Ok(it) => it,
                Err(error) => {
                    let why = why(format!("it does not decode: {error}")); // alloc: error path
                    return Restored { ledger, records, fault: Some(Fault::Corrupt { why }) };
                }
            };
            ledger.gap = ledger.gap.max(record.gap);
            ledger.dropped = ledger.dropped.max(record.dropped);
            for last in record.items {
                // clone: the map's key, once an item
                ledger.items.insert((last.step.clone(), last.subject), last);
            }
            rest = after;
            records += 1;
        }
        Restored { ledger, records, fault: None }
    }

    /// The whole ledger as a file: its magic and one record. What a compaction writes, so the
    /// file holds each subject once.
    pub fn file(&self) -> Vec<u8> {
        let mut out = magic().to_vec();
        if self.items.is_empty() && self.dropped.is_none() && self.gap.is_none() {
            return out;
        }
        let build = self.items.values().map(|it| it.build).chain(self.dropped).chain(self.gap).max().unwrap_or(1);
        // clone: once a compaction, on the history's thread
        let items = self.items.values().cloned().collect();
        out.extend(frame(&Record { gap: self.gap, build, items, dropped: self.dropped }));
        out
    }

    /// Notes that `build`'s computes did not all reach the file: every record from before it
    /// is compared with nothing from now on, as its subject may have been computed since. Of
    /// two gaps the later is kept: every record before it may be stale, which covers the
    /// earlier one's too.
    pub fn behind(&mut self, build: u64) {
        self.gap = self.gap.max(Some(build));
    }

    /// The build after the newest a record names: what a gap is when the build that missed the
    /// file is not known (a record cut short).
    pub fn after_newest(&self) -> u64 {
        self.items.values().map(|it| it.build).max().unwrap_or(0) + 1
    }

    /// The newest build a dropped subject was last computed in, if any was dropped.
    pub fn dropped(&self) -> Option<u64> {
        self.dropped
    }

    /// Whether the ledger holds no subject.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drops the subjects last computed longest ago (ties in key order) until the ledger is
    /// about three quarters of `cap` bytes, once it is past `cap`; how many it dropped. A
    /// dropped subject's next miss reads as computed first, which every answer notes.
    pub fn prune(&mut self, cap: usize) -> usize {
        let mut total: usize = self.items.values().map(weight).sum();
        if total <= cap {
            return 0;
        }
        // alloc: once a compaction past the cap, on the history's thread
        let mut order: Vec<(u64, &(String, ContentId), usize)> =
            self.items.iter().map(|(key, last)| (last.build, key, weight(last))).collect();
        order.sort_unstable();
        let goal = cap / 4 * 3;
        // alloc: as above
        let mut drop = Vec::new();
        for (build, key, weight) in order {
            if total <= goal {
                break;
            }
            total -= weight;
            self.dropped = self.dropped.max(Some(build));
            drop.push(key.clone()); // clone: a key dropped
        }
        for key in &drop {
            self.items.remove(key);
        }
        drop.len()
    }

    fn cause(&self, miss: &Miss) -> RebuildCause {
        match &miss.missed {
            Missed::New => self.against(miss),
            Missed::Read { read, names, was, now, changed, reads } => RebuildCause::Read {
                now: seen(*now),
                was: seen(*was),
                read: read.clone(),   // clone: once a miss
                names: names.clone(), // clone: once a miss
                reads: *reads,
                // alloc: a lookup's key, once a miss
                since: self.items.get(&(miss.step.to_owned(), miss.subject)).map(|it| it.build),
                changed: *changed,
            },
            Missed::Unrecorded => RebuildCause::Unrecorded,
            Missed::Collected => RebuildCause::Collected,
            Missed::Unshaped => RebuildCause::Unshaped,
            Missed::Always(why) => RebuildCause::Always { why: why.clone() }, // clone: once a miss
            Missed::NotExplained(what) => RebuildCause::NotExplained { what: what.clone() }, // clone: once a miss
            Missed::Failed(why) => RebuildCause::Failed { why: why.clone() }, // clone: once a miss
        }
    }

    /// A miss with nothing kept under its key, against the subject's last compute.
    fn against(&self, miss: &Miss) -> RebuildCause {
        // alloc: a lookup's key, once a miss
        let Some(last) = self.items.get(&(miss.step.to_owned(), miss.subject)) else { return RebuildCause::First };
        let since = last.build;
        if let Some(gap) = self.gap.filter(|gap| since < *gap) {
            return RebuildCause::NotExplained {
                // alloc: once a miss
                what: format!(
                    "its last record is build {since}'s, from before build {gap}, whose computes did not all reach blaze why's ledger (an append failed, or the daemon stopped during one): it may have been computed since"
                ),
            };
        }
        if miss.parts.is_empty() {
            // alloc: once a miss
            return RebuildCause::NotExplained { what: format!("{}: its key names no parts", miss.step) };
        }
        if last.key == miss.key {
            return match miss.beyond {
                Some(what) => RebuildCause::Beyond { what: what.to_owned(), since },
                None => RebuildCause::Lost { since },
            };
        }
        let changed = changed(&last.parts, &miss.parts);
        match changed.is_empty() {
            true => RebuildCause::Unnamed { since },
            false => RebuildCause::Parts { parts: u32::try_from(miss.parts.len()).unwrap_or(u32::MAX), since, changed },
        }
    }
}

/// Whether the file, `len` bytes now and `live` bytes when last written whole, is due to be
/// compacted: past [`COMPACT_MIN`] and twice its live size.
pub fn compact_due(len: u64, live: u64) -> bool {
    len > COMPACT_MIN && len > live.saturating_mul(2)
}

/// About how many bytes `last` takes in the file: the bound [`Ledger::prune`] keeps.
fn weight(last: &Last) -> usize {
    let part = |it: &Kept| 48 + it.name.len() + it.names.as_ref().map_or(0, String::len);
    128 + last.step.len() + last.beyond.as_ref().map_or(0, String::len) + last.parts.iter().map(part).sum::<usize>()
}

/// A build's answer as the loop keeps it: whose build it was and what it computed again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Answer {
    pub build: u64,
    /// The build's workspace, as the history's `builds` row names it.
    pub workspace: String,
    pub command: String,
    /// Sorted by item, then step ([`Explained::rebuilt`]).
    pub rebuilt: Vec<Rebuild>,
}

/// What the loop answers `blaze why` from: each workspace's last answer, the newest, and what
/// the ledger's restore said.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Answers {
    by: BTreeMap<String, Answer>,
    newest: Option<String>,
    /// Why the ledger did not restore whole, said with every answer.
    torn: Option<String>,
    /// Whether the ledger's restore is still running on the history's thread.
    restoring: bool,
    dropped: Option<u64>,
}

impl Answers {
    /// No answer yet; `restoring` while the history's thread restores the ledger.
    pub fn new(restoring: bool) -> Answers {
        Answers { restoring, ..Answers::default() }
    }

    /// Whether the ledger's restore is still running.
    pub fn restoring(&self) -> bool {
        self.restoring
    }

    /// The ledger restored, short for `torn` when it was.
    pub fn restored(&mut self, torn: Option<String>, dropped: Option<u64>) {
        (self.restoring, self.torn, self.dropped) = (false, torn, dropped);
    }

    /// The history's thread is gone: no restore is running any more, and what the one that
    /// ran said (`torn`, `dropped`) is still said.
    pub fn gone(&mut self) {
        self.restoring = false;
    }

    /// A build's answer, its workspace's last from now on. Past [`WORKSPACES`] workspaces, the
    /// one whose last build is oldest is let go.
    pub fn add(&mut self, answer: Answer, dropped: Option<u64>) {
        self.dropped = dropped;
        self.newest = Some(answer.workspace.clone()); // clone: once a build
        self.by.insert(answer.workspace.clone(), answer); // clone: once a build
        if self.by.len() > WORKSPACES
            && let Some(oldest) = self.by.values().min_by_key(|it| it.build).map(|it| it.workspace.clone())
        {
            self.by.remove(&oldest);
        }
    }

    /// The answer to `blaze why` asked from `cwd` (the last build of the workspace holding it,
    /// the deepest if several do), or with no `cwd` the newest build's: every thing it computed
    /// again in groups by step and reason (each group's first [`LISTED`] items, or every one
    /// with `all`), or with `item` its chain of reasons (its own, then each item one of them
    /// names, in the order reached, each once). An `item` no item is named is matched by the
    /// items whose name starts with it, in groups.
    pub fn rebuilt(&self, cwd: Option<&str>, item: Option<&str>, all: bool) -> Rebuilt {
        let _span = tracing::info_span!(target: "blaze", "answer blaze why", item = item.unwrap_or("")).entered();
        let answer = match cwd {
            Some(cwd) => self.by.values().filter(|it| holds(&it.workspace, cwd)).max_by_key(|it| it.workspace.len()),
            None => self.newest.as_ref().and_then(|it| self.by.get(it)),
        };
        let mut out = Rebuilt {
            torn: self.torn.clone(), // clone: an answer
            build: None,
            chain: false,
            count: 0,
            groups: Vec::new(), // alloc: none
            command: None,
            dropped: self.dropped,
            rebuilt: Vec::new(), // alloc: none
            restoring: self.restoring,
            workspace: None,
        };
        let Some(answer) = answer else { return out };
        out.build = Some(answer.build);
        (out.command, out.workspace) = (Some(answer.command.clone()), Some(answer.workspace.clone())); // clone: an answer
        let Some(item) = item else {
            (out.count, out.groups) = groups(answer.rebuilt.iter(), all);
            return out;
        };
        if of(&answer.rebuilt, item).is_empty() {
            (out.count, out.groups) = groups(answer.rebuilt.iter().filter(|it| it.item.starts_with(item)), all);
            return out;
        }
        // alloc: once a request
        let mut chain: Vec<Rebuild> = of(&answer.rebuilt, item).to_vec();
        // alloc: once a request, the chain's items
        let mut seen: blaze_hash::HashSet<&str, std::alloc::Global> = blaze_hash::HashSet::default();
        seen.insert(item);
        // Each item enters the chain once and each lookup is a binary search, so the walk is
        // bounded by the answer's length (rule 15).
        let mut at = 0;
        while at < chain.len() && chain.len() <= answer.rebuilt.len() {
            // alloc: once a request, the names a reason gives
            let named: Vec<String> = named(&chain[at].cause).map(str::to_owned).collect();
            for name in named {
                let found = of(&answer.rebuilt, &name);
                let Some(first) = found.first() else { continue };
                if seen.insert(first.item.as_str()) {
                    chain.extend_from_slice(found);
                }
            }
            at += 1;
        }
        out.count = u32::try_from(chain.len()).unwrap_or(u32::MAX);
        (out.chain, out.rebuilt) = (true, chain);
        out
    }
}

/// Whether the directory `workspace` is `cwd` or holds it.
fn holds(workspace: &str, cwd: &str) -> bool {
    let workspace = workspace.trim_end_matches('/');
    cwd.strip_prefix(workspace).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `miss` is compared with its subject's last compute, and so recorded as it: one told
/// by its parts, or one the kernel computed and kept an entry for.
fn compared(miss: &Miss) -> bool {
    (miss.missed == Missed::New && !miss.parts.is_empty()) || miss.kept.is_some()
}

/// The file's magic: [`MAGIC`], then the first 16 bytes of [`version`].
pub fn magic() -> [u8; 32] {
    let mut out = [0; 32];
    out[..16].copy_from_slice(&MAGIC);
    out[16..].copy_from_slice(&version().0[..16]);
    out
}

/// The version of the file's records: a hash of the encoding of a record with every field
/// set, and of [`NAMING`]. A field added, removed, renamed or of another type, or parts and
/// subjects made another way, changes it, so a file written by a blaze whose records differ is
/// another version, moved aside rather than misread (rule 23).
pub fn version() -> ContentId {
    let id = ContentId([7; 32]);
    let part = |names: Option<String>| Kept { hash: Some(id), name: "the toolchain".to_owned(), names };
    let last = Last {
        key: id,
        kept: Some(id),
        step: "unit".to_owned(),
        build: 1,
        parts: vec![part(None), part(Some("app::render".to_owned()))],
        beyond: Some("what it read".to_owned()),
        subject: id,
    };
    let record = Record { gap: Some(1), build: 1, items: vec![last], dropped: Some(1) };
    let bytes = cbor::to_vec(&record).expect("a ledger record is plain data, which CBOR encodes");
    Hasher::new("blaze v2 why ledger version 2").part(&bytes).part(&NAMING.to_le_bytes()).finish()
}

/// The parts whose hashes differ between `was` and `now`, by name, in name order: both are
/// sorted by name, and each name is once in `now` ([`Miss::of_parts`]) and in `was` (kept
/// from a miss that was).
fn changed(was: &[Kept], now: &[Part]) -> Vec<PartChanged> {
    // alloc: once a miss, the names either side has
    let mut names: Vec<&str> = was.iter().map(|it| it.name.as_str()).chain(now.iter().map(|it| it.name.as_str())).collect();
    names.sort_unstable();
    names.dedup();
    // alloc: once a miss
    let mut out = Vec::new();
    for name in names {
        let then = was.iter().find(|it| it.name == name);
        let is = now.iter().find(|it| it.name == name);
        let (old, new) = (then.and_then(|it| it.hash), is.and_then(|it| it.hash));
        if old == new {
            continue;
        }
        // clone: once a part that changed
        let names = is.and_then(|it| it.names.clone()).or_else(|| then.and_then(|it| it.names.clone()));
        out.push(PartChanged { now: new, was: old, part: name.to_owned(), names });
    }
    out
}

fn seen(it: Seen) -> Option<ContentId> {
    match it {
        Seen::Is(hash) => Some(hash),
        Seen::Gone => None,
    }
}

/// The reasons in `rebuilt` (sorted by item) of the item named `name`: a binary search.
fn of<'a>(rebuilt: &'a [Rebuild], name: &str) -> &'a [Rebuild] {
    let start = rebuilt.partition_point(|it| it.item.as_str() < name);
    let end = start + rebuilt[start..].partition_point(|it| it.item == name);
    &rebuilt[start..end]
}

/// The items `cause` names, which a chain follows.
fn named(cause: &RebuildCause) -> impl Iterator<Item = &str> {
    let (read, parts) = match cause {
        RebuildCause::Read { names, .. } => (names.as_deref(), &[][..]),
        RebuildCause::Parts { changed, .. } => (None, changed.as_slice()),
        _ => (None, &[][..]),
    };
    read.into_iter().chain(parts.iter().filter_map(|it| it.names.as_deref()))
}

/// What the items of one group share of their reasons: its kind and what it names (the parts
/// and whether each was added or removed, the read, the text), not the build it is compared
/// with or the hashes.
fn group_of(cause: &RebuildCause) -> String {
    // alloc: once a miss, a group's key
    match cause {
        RebuildCause::First => "first".to_owned(),
        RebuildCause::Parts { changed, .. } => {
            let mut key = "parts".to_owned();
            for it in changed {
                let mark = match (it.was, it.now) {
                    (None, Some(_)) => '+',
                    (Some(_), None) => '-',
                    _ => '~',
                };
                key.push('\n');
                key.push(mark);
                key.push_str(&it.part);
            }
            key
        }
        RebuildCause::Lost { .. } => "lost".to_owned(),
        RebuildCause::Beyond { what, .. } => format!("beyond\n{what}"),
        RebuildCause::Unnamed { .. } => "unnamed".to_owned(),
        RebuildCause::Read { read, .. } => format!("read\n{read}"),
        RebuildCause::Unrecorded => "unrecorded".to_owned(),
        RebuildCause::Collected => "collected".to_owned(),
        RebuildCause::Unshaped => "unshaped".to_owned(),
        RebuildCause::Twice { .. } => "twice".to_owned(),
        RebuildCause::Always { why } => format!("always\n{why}"),
        RebuildCause::NotExplained { what } => format!("not explained\n{what}"),
        RebuildCause::Failed { why } => format!("failed\n{why}"),
    }
}

/// `rebuilt` in groups by step and reason, the most items first, then by step and reason;
/// each lists its first [`LISTED`] items, or every one with `all`. With how many there are.
fn groups<'a>(rebuilt: impl Iterator<Item = &'a Rebuild>, all: bool) -> (u32, Vec<RebuildGroup>) {
    // alloc: once a request, a group a reason
    let mut by: BTreeMap<(&str, String), RebuildGroup> = BTreeMap::new();
    let mut count = 0u32;
    for it in rebuilt {
        count = count.saturating_add(1);
        let group = by.entry((it.step.as_str(), group_of(&it.cause))).or_insert_with(|| RebuildGroup {
            step: it.step.clone(),   // clone: once a group
            cause: it.cause.clone(), // clone: once a group
            count: 0,
            // alloc: once a group
            items: Vec::new(),
        });
        group.count = group.count.saturating_add(1);
        if all || group.items.len() < LISTED {
            group.items.push(it.item.clone()); // clone: once an item listed
        }
    }
    // alloc: once a request
    let mut groups: Vec<((&str, String), RebuildGroup)> = by.into_iter().collect();
    groups.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
    (count, groups.into_iter().map(|(_, it)| it).collect())
}

/// `record` framed: its length, then its deterministic CBOR.
fn frame(record: &Record) -> Vec<u8> {
    let body = cbor::to_vec(record).expect("a ledger record is plain data, which CBOR encodes");
    // alloc: once a record, on the history's thread
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&u32::try_from(body.len()).expect("a ledger record under 4 GiB").to_le_bytes());
    out.extend(body);
    out
}

/// `build`'s answer `rebuilt` as the history's `misses` rows, in its order.
fn rows(build: u64, rebuilt: &[Rebuild]) -> Vec<MissRow> {
    // alloc: a row's text
    let hex = |it: &Option<ContentId>| it.map(|id| format!("{id:x}"));
    let row = |it: &Rebuild| {
        let mut row = MissRow {
            build_id: build,
            step: it.step.clone(), // clone: once a miss
            item: it.item.clone(), // clone: once a miss
            cause: "",
            what: None,
            names: None,
            was: None,
            now: None,
            changed: None,
            of: None,
            since: None,
        };
        match &it.cause {
            RebuildCause::First => row.cause = "first",
            RebuildCause::Parts { parts, since, changed } => {
                row.cause = "parts";
                // alloc: a row's text
                row.what = Some(changed.iter().map(|it| it.part.as_str()).collect::<Vec<_>>().join("; "));
                if let [one] = changed.as_slice() {
                    (row.names, row.was, row.now) = (one.names.clone(), hex(&one.was), hex(&one.now));
                }
                (row.changed, row.of, row.since) =
                    (Some(u32::try_from(changed.len()).unwrap_or(u32::MAX)), Some(*parts), Some(*since));
            }
            RebuildCause::Lost { since } => (row.cause, row.since) = ("lost", Some(*since)),
            RebuildCause::Beyond { what, since } => (row.cause, row.what, row.since) = ("beyond", Some(what.clone()), Some(*since)),
            RebuildCause::Unnamed { since } => (row.cause, row.since) = ("unnamed", Some(*since)),
            RebuildCause::Read { now, was, read, names, reads, since, changed } => {
                row.cause = "read";
                (row.what, row.names, row.was, row.now) = (Some(read.clone()), names.clone(), hex(was), hex(now));
                (row.changed, row.of, row.since) = (Some(*changed), Some(*reads), *since);
            }
            RebuildCause::Unrecorded => row.cause = "unrecorded",
            RebuildCause::Collected => row.cause = "collected",
            RebuildCause::Unshaped => row.cause = "unshaped",
            RebuildCause::Twice { count } => (row.cause, row.changed) = ("twice", Some(*count)),
            RebuildCause::Always { why } => (row.cause, row.what) = ("always", Some(why.clone())),
            RebuildCause::NotExplained { what } => (row.cause, row.what) = ("not_explained", Some(what.clone())),
            RebuildCause::Failed { why } => (row.cause, row.what) = ("failed", Some(why.clone())),
        }
        row
    };
    // alloc: once a build, a row a miss
    rebuilt.iter().map(row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ContentId {
        ContentId([n; 32])
    }

    fn miss(item: &str, subject: u8, key: u8, parts: &[(&str, u8)]) -> Miss {
        let parts = parts.iter().map(|(name, hash)| Part::new(*name, Some(id(*hash)))).collect();
        Miss::of_parts("codegen dev template", id(subject), item.to_owned(), id(key), parts)
    }

    /// `build`'s answer as the loop keeps it, of the workspace `/w`.
    fn answered(answers: &mut Answers, ledger: &mut Ledger, build: u64, well: bool, misses: Vec<Miss>) -> Explained {
        let explained = ledger.explain(build, well, misses);
        let answer = Answer { build, workspace: "/w".to_owned(), command: "build".to_owned(), rebuilt: explained.rebuilt.clone() };
        answers.add(answer, ledger.dropped());
        explained
    }

    /// Where the last record of `file` starts.
    fn torn_at(file: &[u8]) -> usize {
        match Ledger::restore(&file[..file.len() - 1]).fault {
            Some(Fault::Tail { keep, .. }) => keep,
            other => panic!("a file cut by a byte is a torn tail, not {other:?}"),
        }
    }

    fn causes(answers: &Answers, item: &str) -> Vec<RebuildCause> {
        answers.rebuilt(Some("/w/crates/app"), Some(item), false).rebuilt.into_iter().map(|it| it.cause).collect()
    }

    /// A key part that changed between two builds is named, with the build it is compared
    /// with; two instances of one name are compared each with its own last compute; a build
    /// that did not end well moves nothing on; a ledger written and replayed answers the same;
    /// and a file cut short or of another version is told apart.
    #[test]
    fn a_changed_key_part_is_named_by_subject_and_survives_a_restore() {
        let (mut answers, mut ledger) = (Answers::new(false), Ledger::default());
        assert_eq!(answers.rebuilt(None, None, false).build, None, "an answer before any build");
        let mut file = magic().to_vec();
        let explained = answered(
            &mut answers,
            &mut ledger,
            3,
            true,
            vec![
                miss("app::render", 1, 1, &[("its code", 7), ("the toolchain", 9)]),
                miss("app::render", 2, 3, &[("its code", 5), ("the toolchain", 9)]),
            ],
        );
        file.extend(explained.record);
        assert_eq!(causes(&answers, "app::render"), [RebuildCause::First, RebuildCause::First]);
        assert_eq!(answers.rebuilt(Some("/elsewhere"), None, false).build, None, "another workspace's build");
        // A failed build's computes may not have been kept: it is compared with build 3, and
        // compares the next one with build 3 too.
        let explained = answered(&mut answers, &mut ledger, 4, false, vec![miss("app::render", 1, 4, &[("its code", 6), ("the toolchain", 9)])]);
        assert!(explained.record.is_empty(), "a failed build gave a record");
        let explained = answered(&mut answers, &mut ledger, 5, true, vec![miss("app::render", 1, 2, &[("its code", 8), ("the toolchain", 9)])]);
        file.extend(explained.record);
        let changed = vec![PartChanged { now: Some(id(8)), was: Some(id(7)), part: "its code".to_owned(), names: None }];
        assert_eq!(causes(&answers, "app::render"), [RebuildCause::Parts { parts: 2, since: 3, changed }]);
        assert_eq!((explained.rows[0].cause, explained.rows[0].since), ("parts", Some(3)));
        let restored = Ledger::restore(&file);
        assert_eq!((restored.fault, restored.records), (None, 2));
        assert_eq!(restored.ledger.items, ledger.items);
        assert_eq!(Ledger::restore(&restored.ledger.file()).ledger.items, ledger.items);
        // Written whole and read back, byte for byte (rule 23).
        let whole = restored.ledger.file();
        assert_eq!(Ledger::restore(&whole).ledger.file(), whole);
        let torn = Ledger::restore(&file[..file.len() - 1]);
        assert!(matches!(&torn.fault, Some(Fault::Tail { why, .. }) if why.contains("record 1")), "{:?}", torn.fault);
        assert_eq!(torn.ledger.items.len(), 2, "what came before the torn record was not kept");
        // A torn append the file system grew with zeros is a tail too, cut at the same place.
        let mut zeros = file[..torn_at(&file)].to_vec();
        zeros.resize(file.len(), 0);
        let zeroed = Ledger::restore(&zeros);
        assert!(matches!(&zeroed.fault, Some(Fault::Tail { keep, why }) if *keep == torn_at(&file) && why.contains("zeros")), "{:?}", zeroed.fault);
        assert_eq!(Ledger::restore(&file[..20]).fault, Some(Fault::Unbegun { len: 20 }));
        // Another version's file is not read at all.
        let mut other = file.clone();
        other[16] ^= 1;
        let unread = Ledger::restore(&other);
        assert!(matches!(unread.fault, Some(Fault::Version { .. })) && unread.ledger.items.is_empty());
    }

    /// Two different misses of one subject in one build are refused, each, and neither is
    /// recorded as its last compute.
    #[test]
    fn two_misses_of_one_subject_are_refused() {
        let (mut answers, mut ledger) = (Answers::new(false), Ledger::default());
        let explained =
            answered(&mut answers, &mut ledger, 1, true, vec![miss("a", 1, 1, &[("its code", 7)]), miss("b", 1, 2, &[("its code", 8)])]);
        assert!(explained.record.is_empty());
        assert_eq!(causes(&answers, "a"), [RebuildCause::Twice { count: 2 }]);
        assert_eq!(causes(&answers, "b"), [RebuildCause::Twice { count: 2 }]);
        assert!(ledger.items.is_empty());
    }
}
