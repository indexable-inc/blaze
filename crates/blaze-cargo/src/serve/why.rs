//! `blaze why`: what the last build computed again instead of reusing, and why
//! (docs/v2/plan/design/why.md). A sans-IO core (rule 14): [`Ledger`] takes the misses a
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
//!   compared with nothing (rule 21).
//! - **A miss with parts** (a key made of named parts: a dev template's toolchain, code,
//!   settings and callees) is told apart against the parts the subject had when last computed,
//!   by any build, one that failed or was stopped too (a keep-going build that failed still
//!   computed what it computed, under the key it names): the parts that changed by name ([`RebuildCause::Parts`]);
//!   the same key, so nothing is kept under it or what it is filed under past the key differs
//!   ([`RebuildCause::Lost`], [`RebuildCause::Beyond`]); a key that changed while no named part
//!   did ([`RebuildCause::Unnamed`]); or no record of it ([`RebuildCause::First`]). Builds
//!   between may have reused it under another key, so each cause says the build it is
//!   compared with. A record from before a build whose computes did not all reach the file (a
//!   gap: [`Ledger::behind`]) is compared with nothing, and says so.
//! - **A miss the kernel explained** (a read that changed) is its reason as the kernel gave it,
//!   with the build that kept the entry it was compared with.
//! - **The file** ([`LEDGER`]): a magic naming its version, then framed records (`u32`
//!   little-endian length, then deterministic CBOR), one a build, however it ended: the
//!   subjects it computed with their parts. Only types this module owns are written, and the version is
//!   a hash of the encoding of a record with every field set and of [`NAMING`] ([`version`]),
//!   so a change of any of them is a new version and an old file is moved aside, never misread.
//!   Replayed in order, later records winning; compacted to one record once the appends pass
//!   the live size, the oldest subjects dropped past [`CAP_BYTES`] ([`Ledger::prune`]).

use std::collections::{BTreeMap, BTreeSet};

use blaze_api::{
    EvictedBy, PartChanged, PartIs, Reason, Rebuild, RebuildCause, RebuildGroup, Rebuilt, Report,
    Text,
};
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{Miss, Missed, Part, PartKind, Seen};
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

/// About how many bytes of subjects the ledger keeps whole ([`Ledger::prune`]): one per
/// `BLAZE_HOME` holds every workspace's, so it is bounded by size, the subjects last computed
/// longest ago shrunk to tombstones first (no parts: their next miss is
/// [`Reason::Evicted`] by the cap, never read as a first compute).
pub const CAP_BYTES: usize = 64 << 20;

/// The most tombstones the ledger keeps ([`Ledger::prune`]): past it the oldest are removed,
/// which every answer notes ([`Ledger::dropped`]), as their next miss reads as a first compute.
pub const TOMBS: usize = 1_000_000;

/// The most terminal reasons a step keeps ([`Rebuild::ends`]): the first in their order.
pub const ENDS: usize = 8;

/// The most root causes a build's summary names ([`summary`]); the rest are one line.
pub const CAUSES: usize = 4;

/// The most inputs a cause's headline, and items a step's line, name; the rest are a count.
const NAMED: usize = 3;

/// The file is compacted once it is past this and twice the size of its last whole write.
pub const COMPACT_MIN: u64 = 4 << 20;

/// How many workspaces' last answers the loop keeps ([`Answers`]), the oldest build's dropped.
const WORKSPACES: usize = 16;

/// What a subject's parts are named and what its subject is made of: bumped when a part's
/// name or a subject's makeup changes (`compile::unit_subject`, `FnNode::subject`, the part
/// names in `blaze-codegen`'s driver and `compile::RecordInputs::parts`), so an old file's
/// records are not compared with new ones under other names. 3: a record's "the blaze
/// executable" part became its three makers' code identities (`RecordMakers`). 4: a dev
/// template's callees that share a path and an origin are told apart by their subject, and
/// one its build did not walk by its key. 5: a build script's run that ended has each read it
/// traced as a part (`script::with_reads`), and its key covers them. 6: a part has a kind and
/// a file's code text, a unit's crate identity is the parts it is made of
/// (`blaze_lower::Lowered::identity_parts`), and a dev template's subject is its function and
/// settings, of no unit.
const NAMING: u32 = 6;

/// What the ledger holds of a subject: its key and the parts of it when it was last computed,
/// by a build however it ended. Fields in the order deterministic CBOR sorts their names.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Last {
    key: ContentId,
    /// What its chain of reasons ended in when it was computed ([`Rebuild::ends`]): what a
    /// step that names it, computed in a later build it took no part in, ends in too.
    ends: Vec<Reason>,
    /// Its name, for a person and for a part that names it ([`Part::names`]).
    item: String,
    /// The entry the kernel kept for it then, for a step the kernel computes.
    kept: Option<ContentId>,
    step: String,
    /// Whether its parts, ends and kept entry were dropped to keep the ledger under its cap
    /// ([`Ledger::prune`]): it was computed in `build`, and that is all that is known of it.
    tomb: bool,
    build: u64,
    parts: Vec<Kept>,
    beyond: Option<String>,
    subject: ContentId,
    /// Whether the build that computed it did not end well (failed, or was stopped): what a
    /// miss of the same key with nothing kept says ([`RebuildCause::Unfinished`]). Whether the
    /// compute itself failed is not recorded here; that build's `errors` rows say.
    unfinished: bool,
}

/// A part of a key, as the file keeps it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Kept {
    /// A file's code text ([`Part::code`]).
    code: Option<ContentId>,
    hash: Option<ContentId>,
    kind: PartIs,
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
/// subject. A `BTreeMap`, as the file is written in its order (rule 25).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ledger {
    /// A bug a test plants to show its oracle catches it ([`Plant`]).
    #[cfg(test)]
    plant: Plant,
    items: BTreeMap<(String, ContentId), Last>,
    /// Records from builds before this one may lack a later compute of their subject: a build's
    /// record did not reach the file (an append failed, or the daemon stopped mid-append).
    gap: Option<u64>,
    dropped: Option<u64>,
}

/// A bug the tests plant to show their oracles catch it: never in a build of blaze.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Plant {
    #[default]
    None = 0,
    /// Every part of a key is said to have changed, whether its hash did.
    BlameEveryPart = 1,
    /// A part that is another step's result is not followed.
    FollowNothing = 2,
    /// A step's ends are left in the order they were reached in, each as often as reached.
    EndsAsReached = 3,
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
    /// before the build. Every build moves the ledger on, one that failed or was stopped too:
    /// its subjects' parts are what the next build is compared with, and the record to append
    /// to the file is given. A build that did not `ended_well` is recorded as such: a compute
    /// of it that failed or was stopped kept nothing under its key, which the next build's miss
    /// of the same key says ([`RebuildCause::Unfinished`]). Recording only builds that ended
    /// well left every subject of a run of failed builds with no record at all, each miss read
    /// as computed first, and so explained nothing of why it was computed again.
    pub fn explain(&mut self, build: u64, ended_well: bool, misses: Vec<Miss>) -> Explained {
        // The different keys each (step, subject) missed under: more than one is refused. Only
        // a miss compared by its subject counts; one that failed has no subject to speak of.
        // Two misses of one key are one compute said twice (two units that both built a shared
        // generic before either's put landed): the first in the misses' order is kept.
        // alloc: once a build, an entry a subject that missed
        let mut keys: BTreeSet<(&str, ContentId, ContentId)> = BTreeSet::new();
        let misses: Vec<Miss> = misses // alloc: once a build
            .into_iter()
            .filter(|miss| !compared(miss) || keys.insert((miss.step, miss.subject, miss.key)))
            .collect();
        // alloc: as above
        let mut twice: BTreeMap<(&str, ContentId), u32> = BTreeMap::new();
        for (step, subject, _) in &keys {
            *twice.entry((*step, *subject)).or_default() += 1;
        }
        let count = |miss: &Miss| {
            twice.get(&(miss.step, miss.subject)).copied().filter(|n| *n > 1 && compared(miss))
        };
        // alloc: once a build, a cause each miss
        let causes: Vec<RebuildCause> = misses
            .iter()
            .map(|miss| match count(miss) {
                Some(count) => RebuildCause::Twice { count },
                None => self.cause(miss),
            })
            .collect();
        let ends = self.ends(&misses, &causes);
        // alloc: once a build, an item each miss
        let mut rebuilt = Vec::with_capacity(misses.len());
        for ((miss, cause), ends) in misses.iter().zip(&causes).zip(&ends) {
            // clone: once a miss
            rebuilt.push(Rebuild {
                ends: ends.clone(),
                item: miss.item.clone(),
                step: miss.step.to_owned(),
                cause: cause.clone(),
            });
        }
        rebuilt.sort_by(|a, b| (&a.item, &a.step).cmp(&(&b.item, &b.step)));
        // alloc: as above
        let mut items = Vec::new();
        let refused: Vec<bool> = misses.iter().map(|it| count(it).is_some()).collect(); // alloc: once a build
        for ((miss, refused), ends) in misses.into_iter().zip(refused).zip(ends) {
            if refused || !compared(&miss) {
                continue;
            }
            let parts = miss
                .parts
                .into_iter()
                .map(|Part { name, hash, names, kind, code }| Kept {
                    code,
                    hash,
                    kind: part_is(kind),
                    name,
                    names,
                })
                .collect();
            let last = Last {
                key: miss.key,
                ends,
                item: miss.item,
                kept: miss.kept,
                step: miss.step.to_owned(),
                tomb: false,
                build,
                parts,
                beyond: miss.beyond.map(str::to_owned),
                subject: miss.subject,
                unfinished: !ended_well,
            };
            // clone: the map's key and the record's copy, once a miss
            self.items.insert((last.step.clone(), last.subject), last.clone());
            items.push(last);
        }
        let rows = rows(build, &rebuilt);
        let record = match items.is_empty() {
            true => Vec::new(),
            false => frame(&Record { gap: None, build, items, dropped: None }),
        };
        Explained { rebuilt, rows, record }
    }

    /// What each of `misses`' chains ends in ([`Rebuild::ends`]), in their order, each caused
    /// as `causes` says, against the ledger as it was before their build: a miss's own
    /// terminal reasons, and those of each step of the build one of its changed parts names
    /// (or, for a result that names none, of the step that computed it, [`Miss::via`]). Each
    /// step is visited once and each edge once, with a stack of its own, so the walk is
    /// bounded by the build's misses and the parts they name (rule 18); a step that names one
    /// that names it back takes nothing from it.
    fn ends(&self, misses: &[Miss], causes: &[RebuildCause]) -> Vec<Vec<Reason>> {
        // alloc: once a build, the misses by item
        let mut by_item: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (at, miss) in misses.iter().enumerate() {
            by_item.entry(miss.item.as_str()).or_default().push(at);
        }
        // The ledger's records by item, made the first time a part names a step this build
        // did not compute.
        let mut recorded: Option<BTreeMap<&str, Vec<&Last>>> = None;
        // alloc: once a build, each miss's own reasons and the misses it follows
        let mut own: Vec<Vec<Reason>> = Vec::with_capacity(misses.len());
        let mut follows: Vec<Vec<usize>> = Vec::with_capacity(misses.len());
        for (at, (miss, cause)) in misses.iter().zip(causes).enumerate() {
            let (mut mine, mut theirs) = (Vec::new(), Vec::new()); // alloc: as above
            // A part that is another step's result: that step's reasons, in this build or as
            // the ledger recorded them in a build since `since`.
            let mut follow = |name: Option<&str>, part: &str, since: u64, mine: &mut Vec<Reason>| {
                if self.planted(2) {
                    return;
                }
                let Some(name) = name.or(miss.via.as_deref()) else {
                    // alloc: once a miss
                    return mine.push(Reason::Unexplained {
                        what: format!(
                            "`{part}` of {} is another step's result, which blaze does not record yet, and nothing says which step computed it",
                            miss.item
                        ),
                    });
                };
                if let Some(found) = by_item.get(name) {
                    theirs.extend(found.iter().copied().filter(|it| *it != at));
                    if found.iter().any(|it| *it != at) {
                        return;
                    }
                }
                let recorded = recorded.get_or_insert_with(|| {
                    // alloc: once a build that names a step it did not compute
                    let mut by: BTreeMap<&str, Vec<&Last>> = BTreeMap::new();
                    for last in self.items.values() {
                        by.entry(last.item.as_str()).or_default().push(last);
                    }
                    by
                });
                let newer: Vec<&Last> = recorded // alloc: once a part followed into the ledger
                    .get(name)
                    .map(|it| it.iter().copied().filter(|it| it.build > since && !it.tomb).collect())
                    .unwrap_or_default();
                match newer.is_empty() {
                    // clone: once a part followed into the ledger
                    false => mine.extend(newer.iter().flat_map(|it| it.ends.iter().cloned())),
                    // alloc: once a miss
                    true => mine.push(Reason::Unexplained {
                        what: format!(
                            "`{part}` of {} changed, yet blaze has no record of {name} being computed since build {since}: it was reused under another key (hits are not recorded yet), or it came out different from the same inputs",
                            miss.item
                        ),
                    }),
                }
            };
            // alloc: once a miss
            let bug = |what: String| Reason::Unexplained { what };
            match cause {
                RebuildCause::First => mine.push(Reason::FirstTime),
                RebuildCause::Dropped { since } => {
                    mine.push(Reason::Evicted { by: EvictedBy::Cap, when: *since });
                }
                RebuildCause::Unfinished { since } => {
                    mine.push(Reason::PreviousFailed { since: *since });
                }
                RebuildCause::Parts { since, changed, .. } => {
                    for it in changed {
                        match it.kind {
                            // clone: once a part that changed
                            PartIs::Input | PartIs::File => mine.push(Reason::InputChanged {
                                file: it.kind == PartIs::File,
                                input: it.part.clone(),
                                trivia: it.trivia,
                            }),
                            PartIs::Result => {
                                follow(it.names.as_deref(), &it.part, *since, &mut mine);
                            }
                        }
                    }
                }
                RebuildCause::Read { read, names, since, .. } => match names {
                    Some(name) => follow(Some(name.as_str()), read, since.unwrap_or(0), &mut mine),
                    // clone: once a miss
                    None => mine.push(Reason::InputChanged {
                        file: false,
                        input: read.clone(),
                        trivia: false,
                    }),
                },
                // A build script named to run outside the sandbox runs every build, as asked.
                RebuildCause::Always { why } if miss.step == "build script run" => {
                    mine.push(Reason::Forced { how: why.clone() }); // clone: once a miss
                }
                RebuildCause::Always { why } => mine.push(bug(why.clone())), // clone: once a miss
                RebuildCause::Lost { since } => mine.push(bug(format!(
                    "{}: nothing is kept under its key, the one it had when last computed in build {since}, and what dropped it did not record doing so",
                    miss.item
                ))),
                RebuildCause::Beyond { what, since } => mine.push(bug(format!(
                    "{}: its key is the one it had when last computed in build {since}, so {what} differ, whose values blaze does not keep yet",
                    miss.item
                ))),
                RebuildCause::Unnamed { since } => mine.push(bug(format!(
                    "{}: its key changed since build {since} though every part it names is the same, so its key has a part it does not name",
                    miss.item
                ))),
                RebuildCause::Twice { count } => mine.push(bug(format!(
                    "{}: this build computed it {count} times under different keys, so blaze told the instances apart by less than their keys",
                    miss.item
                ))),
                RebuildCause::Unrecorded => mine.push(bug(format!(
                    "{}: shapes of its reads are kept, but blaze has no record of the entry it kept last",
                    miss.item
                ))),
                RebuildCause::Collected => mine.push(bug(format!(
                    "{}: the entry blaze recorded as kept last is not in the store, and what dropped it did not record doing so",
                    miss.item
                ))),
                RebuildCause::Unshaped => mine.push(bug(format!(
                    "{}: every read of the entry kept last hashes as it did, yet no shape of reads names it",
                    miss.item
                ))),
                RebuildCause::NotExplained { what } => {
                    mine.push(bug(format!("{}: {what}", miss.item)));
                }
                RebuildCause::Failed { why } => mine.push(bug(format!(
                    "{}: the reason could not be worked out: {why}",
                    miss.item
                ))),
            }
            theirs.sort_unstable();
            theirs.dedup();
            own.push(mine);
            follows.push(theirs);
        }
        // alloc: once a build
        let mut ends: Vec<Option<Vec<Reason>>> = vec![None; misses.len()];
        let mut reached = vec![false; misses.len()]; // alloc: once a build
        let mut stack: Vec<(usize, usize)> = Vec::new(); // alloc: once a build
        for root in 0..misses.len() {
            if reached[root] {
                continue;
            }
            reached[root] = true;
            stack.push((root, 0));
            while let Some(top) = stack.last_mut() {
                let at = top.0;
                if let Some(&to) = follows[at].get(top.1) {
                    top.1 += 1;
                    if !reached[to] {
                        reached[to] = true;
                        stack.push((to, 0));
                    }
                    continue;
                }
                let mut all = std::mem::take(&mut own[at]);
                for &to in &follows[at] {
                    // One still on the stack names this one back: nothing of it is known yet.
                    if let Some(theirs) = &ends[to] {
                        all.extend(theirs.iter().cloned()); // clone: once an edge
                    }
                }
                if !self.planted(3) {
                    all.sort();
                    all.dedup();
                }
                if all.is_empty() {
                    // alloc: once a miss
                    all.push(Reason::Unexplained {
                        what: format!(
                            "{}: every step its changed parts name names it back",
                            misses[at].item
                        ),
                    });
                }
                if all.len() > ENDS {
                    // An unexplained end sorts last: one is kept, so a cut never hides a bug.
                    let bug = all[ENDS..]
                        .iter()
                        .find(|it| matches!(it, Reason::Unexplained { .. }))
                        .cloned(); // clone: once a miss past its ends
                    all.truncate(ENDS);
                    if let Some(bug) = bug {
                        all[ENDS - 1] = bug;
                    }
                }
                ends[at] = Some(all);
                stack.pop();
            }
        }
        ends.into_iter().map(Option::unwrap_or_default).collect()
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
                _ if bytes.starts_with(&MAGIC) => {
                    Fault::Version { why: "it is another version's ledger".to_owned() }
                } // alloc: error path
                _ => Fault::Version {
                    // alloc: error path
                    why: format!(
                        "its first {} bytes are no ledger's magic",
                        bytes.len().min(magic.len())
                    ),
                },
            };
            return Restored { ledger, records: 0, fault: Some(fault) };
        };
        let mut records = 0usize;
        while !rest.is_empty() {
            let keep = bytes.len() - rest.len();
            // alloc: error path
            let why =
                |why: String| format!("record {records}, {} bytes from the end: {why}", rest.len());
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
        let build = self
            .items
            .values()
            .map(|it| it.build)
            .chain(self.dropped)
            .chain(self.gap)
            .max()
            .unwrap_or(1);
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

    /// Once the ledger is past `cap` bytes, shrinks the subjects last computed longest ago
    /// (ties in key order) to tombstones, their parts, ends and kept entry dropped, until it
    /// is about three quarters of `cap`: a tombstone's next miss is [`Reason::Evicted`] by the
    /// cap, never read as a first compute. Past [`TOMBS`] tombstones the oldest are removed
    /// down to three quarters of that, which every answer notes ([`Ledger::dropped`]). How
    /// many it shrank or removed.
    pub fn prune(&mut self, cap: usize) -> usize {
        self.prune_to(cap, TOMBS)
    }

    /// [`Ledger::prune`] with at most `tombs` tombstones kept.
    ///
    /// The cap is of what shrinking frees: the parts, ends and kept entries of the subjects
    /// still whole. A tombstone's own bytes are bounded by `tombs`, not by `cap`. Until
    /// 2026-11-11 the cap was held against every subject's weight, tombstones included, which
    /// shrinking cannot lower: in a home whose tombstones alone weighed more than the goal,
    /// every compaction shrank every whole subject, the ones the last build had just
    /// computed too, so each build's misses read "blaze why's ledger dropped its records"
    /// for subjects of the open workspace (ripgrep's seven build script runs, every build).
    fn prune_to(&mut self, cap: usize, tombs: usize) -> usize {
        let mut total: usize =
            self.items.values().filter(|it| !it.tomb).map(|it| weight(it) - bare(it)).sum();
        let mut changed = 0;
        if total > cap {
            // alloc: once a compaction past the cap, on the history's thread
            let mut order: Vec<(u64, (String, ContentId))> = self
                .items
                .iter()
                .filter(|(_, last)| !last.tomb)
                .map(|(key, last)| (last.build, key.clone())) // clone: as above
                .collect();
            order.sort_unstable();
            let goal = cap / 4 * 3;
            for (_, key) in order {
                if total <= goal {
                    break;
                }
                let Some(last) = self.items.get_mut(&key) else { continue };
                let was = weight(last);
                last.parts = Vec::new(); // alloc: none
                last.ends = Vec::new(); // alloc: none
                (last.kept, last.beyond, last.tomb) = (None, None, true);
                total = total.saturating_sub(was.saturating_sub(weight(last)));
                changed += 1;
            }
        }
        let held = self.items.values().filter(|it| it.tomb).count();
        if held > tombs {
            // alloc: once a compaction past the tombstones' cap
            let mut order: Vec<(u64, (String, ContentId))> = self
                .items
                .iter()
                .filter(|(_, last)| last.tomb)
                .map(|(key, last)| (last.build, key.clone())) // clone: as above
                .collect();
            order.sort_unstable();
            for (build, key) in order.into_iter().take(held - tombs / 4 * 3) {
                self.dropped = self.dropped.max(Some(build));
                self.items.remove(&key);
                changed += 1;
            }
        }
        changed
    }

    /// Whether a test planted bug `plant` ([`Plant`], by its number): never in a build of
    /// blaze.
    #[cfg(test)]
    fn planted(&self, plant: u8) -> bool {
        self.plant as u8 == plant
    }

    #[cfg(not(test))]
    fn planted(&self, _plant: u8) -> bool {
        false
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
        let Some(last) = self.items.get(&(miss.step.to_owned(), miss.subject)) else {
            return RebuildCause::First;
        };
        let since = last.build;
        if last.tomb {
            return RebuildCause::Dropped { since };
        }
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
            return RebuildCause::NotExplained {
                what: format!("{}: its key names no parts", miss.step),
            };
        }
        if last.key == miss.key {
            if last.unfinished {
                return RebuildCause::Unfinished { since };
            }
            return match miss.beyond {
                Some(what) => RebuildCause::Beyond { what: what.to_owned(), since },
                None => RebuildCause::Lost { since },
            };
        }
        let changed = changed(&last.parts, &miss.parts, self.planted(1));
        match changed.is_empty() {
            true => RebuildCause::Unnamed { since },
            false => RebuildCause::Parts {
                parts: u32::try_from(miss.parts.len()).unwrap_or(u32::MAX),
                since,
                changed,
            },
        }
    }
}

/// Whether the file, `len` bytes now and `live` bytes when last written whole, is due to be
/// compacted: past [`COMPACT_MIN`] and twice its live size.
pub fn compact_due(len: u64, live: u64) -> bool {
    len > COMPACT_MIN && len > live.saturating_mul(2)
}

/// What a subject weighs as a tombstone: [`weight`] with no part, end or `beyond`.
fn bare(last: &Last) -> usize {
    160 + last.step.len() + last.item.len()
}

/// About how many bytes `last` takes in the file: the bound [`Ledger::prune`] keeps.
fn weight(last: &Last) -> usize {
    let part = |it: &Kept| {
        56 + it.name.len()
            + it.names.as_ref().map_or(0, String::len)
            + it.code.map_or(0, |_| 34)
    };
    let end = |it: &Reason| match it {
        Reason::InputChanged { input: text, .. }
        | Reason::Forced { how: text }
        | Reason::KeyUnstable { part: text }
        | Reason::Unexplained { what: text } => 32 + text.len(),
        Reason::FirstTime | Reason::Evicted { .. } | Reason::PreviousFailed { .. } => 32,
    };
    160 + last.step.len()
        + last.item.len()
        + last.beyond.as_ref().map_or(0, String::len)
        + last.parts.iter().map(part).sum::<usize>()
        + last.ends.iter().map(end).sum::<usize>()
}

/// A part's kind as the file and the API say it.
fn part_is(kind: PartKind) -> PartIs {
    match kind {
        PartKind::Input => PartIs::Input,
        PartKind::File => PartIs::File,
        PartKind::Result => PartIs::Result,
    }
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
    /// What the build says of itself once it ends ([`summary`] of `rebuilt`), made once,
    /// where the build is explained.
    pub summary: Vec<Report>,
}

impl Answer {
    /// The answer of `build`, `command` in `workspace`, which computed `rebuilt` again
    /// (sorted by item, then step), with its summary.
    pub fn new(build: u64, workspace: String, command: String, rebuilt: Vec<Rebuild>) -> Answer {
        let summary = summary(&rebuilt);
        Answer { build, workspace, command, rebuilt, summary }
    }
}

/// What a `blaze why` asks (`blaze_api::Request::Rebuilt`).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ask {
    pub all: bool,
    pub cwd: Option<String>,
    pub item: Option<String>,
    /// The one build asked of, whatever `cwd`: its own client's ask, once it ended.
    pub build: Option<u64>,
    /// Only the steps whose chain ends in [`Reason::Unexplained`].
    pub unexplained: bool,
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
            && let Some(oldest) =
                self.by.values().min_by_key(|it| it.build).map(|it| it.workspace.clone())
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
        // clone: an ask's own text, once a request
        self.asked(&Ask {
            all,
            cwd: cwd.map(str::to_owned),
            item: item.map(str::to_owned),
            build: None,
            unexplained: false,
        })
    }

    /// [`Answers::rebuilt`] of all an ask can say: with [`Ask::build`], of that build alone
    /// (no build in the answer when the reasons kept are not its), and with
    /// [`Ask::unexplained`], each step whose chain ends in [`Reason::Unexplained`]. Without an
    /// item, the answer carries the build's summary.
    pub fn asked(&self, ask: &Ask) -> Rebuilt {
        let (cwd, item, all) = (ask.cwd.as_deref(), ask.item.as_deref(), ask.all);
        let _span =
            tracing::info_span!(target: "blaze", "answer blaze why", item = item.unwrap_or(""))
                .entered();
        let answer = match (ask.build, cwd) {
            (Some(build), _) => self.by.values().find(|it| it.build == build),
            (None, Some(cwd)) => self
                .by
                .values()
                .filter(|it| holds(&it.workspace, cwd))
                .max_by_key(|it| it.workspace.len()),
            (None, None) => self.newest.as_ref().and_then(|it| self.by.get(it)),
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
            summary: Vec::new(), // alloc: none
            restoring: self.restoring,
            workspace: None,
        };
        let Some(answer) = answer else { return out };
        out.build = Some(answer.build);
        (out.command, out.workspace) =
            (Some(answer.command.clone()), Some(answer.workspace.clone())); // clone: an answer
        if ask.unexplained {
            // clone: once a request, the steps blaze has no reason for
            out.rebuilt = answer
                .rebuilt
                .iter()
                .filter(|it| item.is_none_or(|item| it.item.starts_with(item)))
                .filter(|it| it.ends.iter().any(|end| matches!(end, Reason::Unexplained { .. })))
                .cloned()
                .collect();
            out.count = u32::try_from(out.rebuilt.len()).unwrap_or(u32::MAX);
            return out;
        }
        let Some(item) = item else {
            (out.count, out.groups) = groups(answer.rebuilt.iter(), all);
            out.summary = answer.summary.clone(); // clone: an answer
            return out;
        };
        if of(&answer.rebuilt, item).is_empty() {
            (out.count, out.groups) =
                groups(answer.rebuilt.iter().filter(|it| it.item.starts_with(item)), all);
            return out;
        }
        // alloc: once a request
        let mut chain: Vec<Rebuild> = of(&answer.rebuilt, item).to_vec();
        // alloc: once a request, the chain's items
        let mut seen: blaze_hash::HashSet<&str, std::alloc::Global> =
            blaze_hash::HashSet::default();
        seen.insert(item);
        // Each item enters the chain once and each lookup is a binary search, so the walk is
        // bounded by the answer's length (rule 18).
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
/// another version, moved aside rather than misread (rule 26).
pub fn version() -> ContentId {
    let id = ContentId([7; 32]);
    let part = |names: Option<String>, kind: PartIs| Kept {
        code: Some(id),
        hash: Some(id),
        kind,
        name: "the toolchain".to_owned(),
        names,
    };
    let text = || "app::render".to_owned();
    // Every kind of end: a change of any one's encoding is a new version.
    let ends = vec![
        Reason::InputChanged { file: true, input: text(), trivia: true },
        Reason::FirstTime,
        Reason::Evicted { by: EvictedBy::Memory, when: 1 },
        Reason::Evicted { by: EvictedBy::Age, when: 1 },
        Reason::Evicted { by: EvictedBy::Cap, when: 1 },
        Reason::PreviousFailed { since: 1 },
        Reason::Forced { how: text() },
        Reason::KeyUnstable { part: text() },
        Reason::Unexplained { what: text() },
    ];
    let last = Last {
        key: id,
        ends,
        item: text(),
        kept: Some(id),
        step: "unit".to_owned(),
        tomb: true,
        build: 1,
        parts: vec![
            part(None, PartIs::Input),
            part(Some(text()), PartIs::Result),
            part(None, PartIs::File),
        ],
        beyond: Some("what it read".to_owned()),
        subject: id,
        unfinished: true,
    };
    let record = Record { gap: Some(1), build: 1, items: vec![last], dropped: Some(1) };
    let bytes = cbor::to_vec(&record).expect("a ledger record is plain data, which CBOR encodes");
    Hasher::new("blaze v2 why ledger version 3").part(&bytes).part(&NAMING.to_le_bytes()).finish()
}

/// The parts whose hashes differ between `was` and `now`, by name, in name order: both are
/// sorted by name, and each name is once in `now` ([`Miss::of_parts`]) and in `was` (kept
/// from a miss that was).
fn changed(was: &[Kept], now: &[Part], blame_all: bool) -> Vec<PartChanged> {
    // alloc: once a miss, the names either side has
    let mut names: Vec<&str> = was
        .iter()
        .map(|it| it.name.as_str())
        .chain(now.iter().map(|it| it.name.as_str()))
        .collect();
    names.sort_unstable();
    names.dedup();
    // alloc: once a miss
    let mut out = Vec::new();
    for name in names {
        let then = was.iter().find(|it| it.name == name);
        let is = now.iter().find(|it| it.name == name);
        let (old, new) = (then.and_then(|it| it.hash), is.and_then(|it| it.hash));
        if old == new && !blame_all {
            continue;
        }
        // clone: once a part that changed
        let names =
            is.and_then(|it| it.names.clone()).or_else(|| then.and_then(|it| it.names.clone()));
        let kind = match (is, then) {
            (Some(is), _) => part_is(is.kind),
            (None, Some(then)) => then.kind,
            (None, None) => PartIs::Input,
        };
        // A file whose bytes changed and whose code text, known both times, did not.
        let trivia = kind == PartIs::File
            && matches!((is, then), (Some(is), Some(then))
                if is.hash.is_some() && then.hash.is_some() && is.code.is_some() && is.code == then.code);
        out.push(PartChanged { now: new, was: old, kind, part: name.to_owned(), names, trivia });
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
        RebuildCause::Unfinished { .. } => "unfinished".to_owned(),
        RebuildCause::Dropped { .. } => "dropped".to_owned(),
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

/// The step of what a build's checkout lost of its last build's results (blaze-cargo's
/// `Jobs::lose`): said before every other group and root cause, as the daemon lowered the
/// build's front end anew for it, and most of what else was computed again follows from that.
const NOT_KEPT: &str = blaze_profile::slow::NOT_KEPT_STEP;

/// The step of what a build ran because its checkout's last build did not end well
/// (blaze-cargo's `job::Unfinished`): said with [`NOT_KEPT`] before every other group and root
/// cause, as the work under them is that build's remainder, not something this one's inputs
/// changed.
pub(super) const UNFINISHED: &str = blaze_profile::slow::UNFINISHED_STEP;

/// Whether `step` is said before every other: what the last build lost, then what it left
/// unfinished. 0 and 1 for those, 2 for any other, to sort by.
fn said_first(step: &str) -> u8 {
    match step {
        NOT_KEPT => 0,
        UNFINISHED => 1,
        _ => 2,
    }
}

/// `rebuilt` in groups by step and reason: what the last build lost first ([`NOT_KEPT`]), then
/// the most items first, then by step and reason; each lists its first [`LISTED`] items, or
/// every one with `all`. With how many there are.
fn groups<'a>(rebuilt: impl Iterator<Item = &'a Rebuild>, all: bool) -> (u32, Vec<RebuildGroup>) {
    // alloc: once a request, a group a reason
    let mut by: BTreeMap<(&str, String), RebuildGroup> = BTreeMap::new();
    let mut count = 0u32;
    for it in rebuilt {
        count = count.saturating_add(1);
        let group =
            by.entry((it.step.as_str(), group_of(&it.cause))).or_insert_with(|| RebuildGroup {
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
    groups.sort_by(|a, b| {
        said_first(&a.1.step)
            .cmp(&said_first(&b.1.step))
            .then(b.1.count.cmp(&a.1.count))
            .then_with(|| a.0.cmp(&b.0))
    });
    (count, groups.into_iter().map(|(_, it)| it).collect())
}

/// What a build that computed `rebuilt` again says of itself once it ends
/// (docs/v2/plan/design/why.md, "What is printed"): a report a root cause (the ends its steps
/// share), what the last build lost first ([`NOT_KEPT`]), then the most steps first and then
/// in the ends' order, the first [`CAUSES`], and one more counting the rest. Nothing for a build that computed nothing again. A function of
/// `rebuilt`'s steps and their ends alone, whatever order they are in (rule 25).
pub fn summary(rebuilt: &[Rebuild]) -> Vec<Report> {
    // alloc: once a build, a group a root cause
    let mut by: BTreeMap<&[Reason], Vec<&Rebuild>> = BTreeMap::new();
    for it in rebuilt {
        by.entry(it.ends.as_slice()).or_default().push(it);
    }
    let mut groups: Vec<(&[Reason], Vec<&Rebuild>)> = by.into_iter().collect(); // alloc: as above
    // What the last build lost is said first, however many steps share its cause: with more
    // than `CAUSES` root causes it would be counted among "the rest" and never read.
    let first = |steps: &[&Rebuild]| steps.iter().map(|it| said_first(&it.step)).min();
    groups.sort_by(|a, b| {
        first(&a.1)
            .cmp(&first(&b.1))
            .then(b.1.len().cmp(&a.1.len()))
            .then_with(|| a.0.cmp(b.0))
    });
    // alloc: once a build, a report a cause said
    let mut out: Vec<Report> =
        groups.iter().take(CAUSES).map(|(ends, steps)| cause(ends, steps)).collect();
    if groups.len() > CAUSES {
        let steps: usize = groups[CAUSES..].iter().map(|it| it.1.len()).sum();
        let text = Text::new()
            .words("recomputed ")
            .count(steps as u64)
            .words(" more for ")
            .count((groups.len() - CAUSES) as u64)
            .words(" other causes: ")
            .code("blaze why")
            .words(" lists every one");
        out.push(Report::note(text).rule("why:more"));
    }
    out
}

/// One root cause of a build's summary: the `steps` whose chains end in `ends`.
fn cause(ends: &[Reason], steps: &[&Rebuild]) -> Report {
    let count = steps.len() as u64;
    let steps_word = if count == 1 { " step" } else { " steps" };
    let is_bug = |it: &Reason| matches!(it, Reason::Unexplained { .. });
    let is_first = |it: &Reason| matches!(it, Reason::FirstTime);
    // A cold build is one line: no list of everything it made.
    if ends.iter().all(is_first) {
        let text = Text::new().words("computed ").count(count).words(steps_word).words(" for the first time");
        return Report::note(text).rule("why:first");
    }
    let report = if ends.iter().all(is_bug) {
        let text = Text::new()
            .count(count)
            .words(steps_word)
            .words(" recomputed for no recorded reason: a blaze bug (")
            .code("blaze why --unexplained")
            .words(")");
        Report::warning(text).rule("why:unexplained")
    } else {
        let (text, severity_rule) = because(ends, count, steps_word);
        match severity_rule {
            (true, rule) => Report::warning(text).rule(rule),
            (false, rule) => Report::note(text).rule(rule),
        }
    };
    // Its steps by kind, the most first, each naming its first items.
    // alloc: once a cause said
    let mut kinds: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for it in steps {
        kinds.entry(it.step.as_str()).or_default().push(it.item.as_str());
    }
    let mut kinds: Vec<(&str, Vec<&str>)> = kinds.into_iter().collect(); // alloc: as above
    kinds.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    kinds.into_iter().fold(report, |report, (step, mut items)| {
        items.sort_unstable();
        let mut text = Text::new().words(step).words(": ").words(&items[..items.len().min(NAMED)].join(", "));
        if items.len() > NAMED {
            text = text.words(" and ").count((items.len() - NAMED) as u64).words(" more");
        }
        report.fact(step, text)
    })
}

/// The headline of a cause that is not one kind alone: `recomputed N steps because ...`, a
/// clause each kind of end joined by `and`; and whether it is a warning (wasted work, or a
/// bug in blaze), with its rule.
fn because(ends: &[Reason], count: u64, steps_word: &str) -> (Text, (bool, &'static str)) {
    let mut text = Text::new().words("recomputed ").count(count).words(steps_word).words(" because ");
    let mut clauses = 0;
    let join = |text: Text, clauses: &mut u32| {
        *clauses += 1;
        if *clauses > 1 { text.words(" and ") } else { text }
    };
    // The inputs that changed, named: files by path, the rest as code.
    let inputs: Vec<(&str, bool, bool)> = ends // alloc: once a cause said
        .iter()
        .filter_map(|it| match it {
            Reason::InputChanged { file, input, trivia } => Some((input.as_str(), *file, *trivia)),
            _ => None,
        })
        .collect();
    let wasted = !inputs.is_empty() && inputs.iter().all(|it| it.2);
    if !inputs.is_empty() {
        text = join(text, &mut clauses);
        for (at, (input, file, trivia)) in inputs.iter().take(NAMED).enumerate() {
            if at > 0 {
                text = text.words(if at + 1 == inputs.len() { " and " } else { ", " });
            }
            text = if *file { text.path(input) } else { text.code(input) };
            if *trivia && !wasted {
                text = text.words(" (comments only)");
            }
        }
        if inputs.len() > NAMED {
            text = text.words(" and ").count((inputs.len() - NAMED) as u64).words(" more inputs");
        }
        text = text.words(" changed");
        if wasted {
            text = text.words(", though no item in ").words(if inputs.len() == 1 { "it" } else { "them" }).words(
                " did (comments only): wasted work, a bug in blaze's keys",
            );
        }
    }
    let mut rule = match (wasted, inputs.is_empty()) {
        (true, _) => (true, "why:wasted"),
        (false, false) => (false, "why:input"),
        (false, true) => (false, "why:first"),
    };
    let mut said = [false; 5];
    for end in ends {
        let (at, words, code, is) = match end {
            Reason::InputChanged { .. } => continue,
            Reason::FirstTime => (0, "some were never computed before", None, (false, "why:first")),
            Reason::Evicted { by: EvictedBy::Cap, .. } => (
                1,
                "blaze why's ledger dropped its records of them to stay under its cap",
                None,
                (false, "why:evicted"),
            ),
            Reason::Evicted { .. } => {
                (1, "their results were dropped from the store", None, (false, "why:evicted"))
            }
            Reason::PreviousFailed { .. } => {
                (2, "the build that last computed each did not end well and kept nothing of it", None, (false, "why:failed"))
            }
            Reason::Forced { how } => {
                (3, "that was asked for: ", Some(how.as_str()), (false, "why:forced"))
            }
            Reason::KeyUnstable { part } => (
                4,
                " came out different from the same inputs (a determinism bug in blaze)",
                Some(part.as_str()),
                (true, "why:unstable"),
            ),
            Reason::Unexplained { .. } => continue,
        };
        // One clause a kind of end, but each forced or unstable one by its name.
        if said[at] && code.is_none() {
            continue;
        }
        said[at] = true;
        text = join(text, &mut clauses);
        text = match (end, code) {
            (Reason::KeyUnstable { .. }, Some(part)) => text.code(part).words(words),
            (_, Some(how)) => text.words(words).words(how),
            (_, None) => text.words(words),
        };
        // The first rule stands unless a later end is a bug, or nothing set one yet.
        if is.0 || (inputs.is_empty() && !rule.0 && clauses == 1) {
            rule = is;
        }
    }
    if ends.iter().any(|it| matches!(it, Reason::Unexplained { .. })) {
        text = join(text, &mut clauses)
            .words("of something blaze did not record (a blaze bug: ")
            .code("blaze why --unexplained")
            .words(")");
        rule = (true, "why:unexplained");
    }
    (text, rule)
}

/// `record` framed: its length, then its deterministic CBOR.
fn frame(record: &Record) -> Vec<u8> {
    let body = cbor::to_vec(record).expect("a ledger record is plain data, which CBOR encodes");
    // alloc: once a record, on the history's thread
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(
        &u32::try_from(body.len()).expect("a ledger record under 4 GiB").to_le_bytes(),
    );
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
                row.what =
                    Some(changed.iter().map(|it| it.part.as_str()).collect::<Vec<_>>().join("; "));
                if let [one] = changed.as_slice() {
                    (row.names, row.was, row.now) =
                        (one.names.clone(), hex(&one.was), hex(&one.now));
                }
                (row.changed, row.of, row.since) = (
                    Some(u32::try_from(changed.len()).unwrap_or(u32::MAX)),
                    Some(*parts),
                    Some(*since),
                );
            }
            RebuildCause::Lost { since } => (row.cause, row.since) = ("lost", Some(*since)),
            RebuildCause::Unfinished { since } => {
                (row.cause, row.since) = ("unfinished", Some(*since))
            }
            RebuildCause::Dropped { since } => (row.cause, row.since) = ("dropped", Some(*since)),
            RebuildCause::Beyond { what, since } => {
                (row.cause, row.what, row.since) = ("beyond", Some(what.clone()), Some(*since))
            }
            RebuildCause::Unnamed { since } => (row.cause, row.since) = ("unnamed", Some(*since)),
            RebuildCause::Read { now, was, read, names, reads, since, changed } => {
                row.cause = "read";
                (row.what, row.names, row.was, row.now) =
                    (Some(read.clone()), names.clone(), hex(was), hex(now));
                (row.changed, row.of, row.since) = (Some(*changed), Some(*reads), *since);
            }
            RebuildCause::Unrecorded => row.cause = "unrecorded",
            RebuildCause::Collected => row.cause = "collected",
            RebuildCause::Unshaped => row.cause = "unshaped",
            RebuildCause::Twice { count } => (row.cause, row.changed) = ("twice", Some(*count)),
            RebuildCause::Always { why } => (row.cause, row.what) = ("always", Some(why.clone())),
            RebuildCause::NotExplained { what } => {
                (row.cause, row.what) = ("not_explained", Some(what.clone()))
            }
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

    /// What the last build lost is the first group `blaze why` prints and the first root
    /// cause a build says of itself, though one row among hundreds: on 2026-11-10 it was the
    /// last of both, under every unit it had made the build compute again.
    #[test]
    fn what_the_last_build_lost_is_said_first() {
        let lost = RebuildCause::Always { why: "the item store: two images were made".to_owned() };
        let step = |step: &str, item: &str, cause: &RebuildCause| Rebuild {
            ends: Vec::new(),
            item: item.to_owned(),
            step: step.to_owned(),
            cause: cause.clone(),
        };
        let mut rebuilt: Vec<Rebuild> =
            (0..5).map(|n| step("unit", &format!("crate-{n}"), &RebuildCause::First)).collect();
        rebuilt.push(step(NOT_KEPT, "the last build of /w", &lost));
        let (count, groups) = groups(rebuilt.iter(), false);
        assert_eq!(count, 6);
        assert_eq!((groups[0].step.as_str(), groups[0].count), (NOT_KEPT, 1), "{groups:#?}");
        assert_eq!((groups[1].step.as_str(), groups[1].count), ("unit", 5), "{groups:#?}");
        // The control: with no such row the most items are first, as before.
        let (_, without) = groups_of(&rebuilt[..5]);
        assert_eq!(without[0].step, "unit");
    }

    /// What a build ran because the last one failed is said before the units it built and
    /// after what the last build lost, in the groups and in the build's own summary: a build
    /// finishing a failed one's work never reads as a slow build of nothing (2026-11-10).
    #[test]
    fn what_the_last_build_left_unfinished_is_said_before_the_work() {
        let left = RebuildCause::Always {
            why: "the last build of /w (build 41, `build`) failed with 212 of its 340 units not built".to_owned(),
        };
        let lost = RebuildCause::Always { why: "the item store: two images were made".to_owned() };
        let step = |step: &str, item: &str, cause: &RebuildCause| Rebuild {
            ends: Vec::new(),
            item: item.to_owned(),
            step: step.to_owned(),
            cause: cause.clone(),
        };
        let mut rebuilt: Vec<Rebuild> =
            (0..5).map(|n| step("unit", &format!("crate-{n}"), &RebuildCause::First)).collect();
        rebuilt.push(step(UNFINISHED, "the last build of /w", &left));
        let (_, groups) = groups(rebuilt.iter(), false);
        assert_eq!((groups[0].step.as_str(), groups[0].count), (UNFINISHED, 1), "{groups:#?}");
        assert_eq!(groups[0].cause, left, "its reason is the build's own words");
        rebuilt.push(step(NOT_KEPT, "the last build of /w", &lost));
        let (_, groups) = groups_of(&rebuilt);
        let order: Vec<&str> = groups.iter().map(|it| it.step.as_str()).collect();
        assert_eq!(order, [NOT_KEPT, UNFINISHED, "unit"]);
        assert_eq!(said_first(NOT_KEPT), 0);
        assert_eq!(said_first("unit"), 2);
    }

    fn groups_of(rebuilt: &[Rebuild]) -> (u32, Vec<RebuildGroup>) {
        groups(rebuilt.iter(), false)
    }

    fn miss(item: &str, subject: u8, key: u8, parts: &[(&str, u8)]) -> Miss {
        let parts = parts.iter().map(|(name, hash)| Part::new(*name, Some(id(*hash)))).collect();
        Miss::of_parts("codegen dev template", id(subject), item.to_owned(), id(key), parts)
    }

    /// `build`'s answer as the loop keeps it, of the workspace `/w`.
    fn answered(
        answers: &mut Answers,
        ledger: &mut Ledger,
        build: u64,
        well: bool,
        misses: Vec<Miss>,
    ) -> Explained {
        let explained = ledger.explain(build, well, misses);
        let answer = Answer::new(
            build,
            "/w".to_owned(),
            "build".to_owned(),
            explained.rebuilt.clone(),
        );
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
        answers
            .rebuilt(Some("/w/crates/app"), Some(item), false)
            .rebuilt
            .into_iter()
            .map(|it| it.cause)
            .collect()
    }

    /// A key part that changed between two builds is named, with the build it is compared
    /// with; two instances of one name are compared each with its own last compute; a build
    /// that failed moves the ledger on like any other; a ledger written and replayed answers the same;
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
        assert_eq!(
            answers.rebuilt(Some("/elsewhere"), None, false).build,
            None,
            "another workspace's build"
        );
        // A failed build (keep-going: what it computed, it computed) is compared with build 3,
        // and the next one is compared with it.
        let explained = answered(
            &mut answers,
            &mut ledger,
            4,
            false,
            vec![miss("app::render", 1, 4, &[("its code", 6), ("the toolchain", 9)])],
        );
        assert!(!explained.record.is_empty(), "a failed build gave no record");
        file.extend(explained.record);
        // The same key again after the failed build: its compute there kept nothing.
        let mut again = Ledger::default();
        again.explain(
            4,
            false,
            vec![miss("app::render", 1, 4, &[("its code", 6), ("the toolchain", 9)])],
        );
        let unfinished = again.explain(
            5,
            true,
            vec![miss("app::render", 1, 4, &[("its code", 6), ("the toolchain", 9)])],
        );
        assert_eq!(unfinished.rebuilt[0].cause, RebuildCause::Unfinished { since: 4 });
        let explained = answered(
            &mut answers,
            &mut ledger,
            5,
            true,
            vec![miss("app::render", 1, 2, &[("its code", 8), ("the toolchain", 9)])],
        );
        file.extend(explained.record);
        let changed = vec![PartChanged {
            now: Some(id(8)),
            was: Some(id(6)),
            kind: PartIs::Input,
            part: "its code".to_owned(),
            names: None,
            trivia: false,
        }];
        assert_eq!(
            causes(&answers, "app::render"),
            [RebuildCause::Parts { parts: 2, since: 4, changed }]
        );
        assert_eq!((explained.rows[0].cause, explained.rows[0].since), ("parts", Some(4)));
        let restored = Ledger::restore(&file);
        assert_eq!((restored.fault, restored.records), (None, 3));
        assert_eq!(restored.ledger.items, ledger.items);
        assert_eq!(Ledger::restore(&restored.ledger.file()).ledger.items, ledger.items);
        // Written whole and read back, byte for byte (rule 26).
        let whole = restored.ledger.file();
        assert_eq!(Ledger::restore(&whole).ledger.file(), whole);
        let torn = Ledger::restore(&file[..file.len() - 1]);
        assert!(
            matches!(&torn.fault, Some(Fault::Tail { why, .. }) if why.contains("record 1")),
            "{:?}",
            torn.fault
        );
        assert_eq!(torn.ledger.items.len(), 2, "what came before the torn record was not kept");
        // A torn append the file system grew with zeros is a tail too, cut at the same place.
        let mut zeros = file[..torn_at(&file)].to_vec();
        zeros.resize(file.len(), 0);
        let zeroed = Ledger::restore(&zeros);
        assert!(
            matches!(&zeroed.fault, Some(Fault::Tail { keep, why }) if *keep == torn_at(&file) && why.contains("zeros")),
            "{:?}",
            zeroed.fault
        );
        assert_eq!(Ledger::restore(&file[..20]).fault, Some(Fault::Unbegun { len: 20 }));
        // Another version's file is not read at all.
        let mut other = file.clone();
        other[16] ^= 1;
        let unread = Ledger::restore(&other);
        assert!(
            matches!(unread.fault, Some(Fault::Version { .. })) && unread.ledger.items.is_empty()
        );
    }

    /// Two different misses of one subject in one build are refused, each, and neither is
    /// recorded as its last compute.
    #[test]
    fn two_misses_of_one_subject_are_refused() {
        let (mut answers, mut ledger) = (Answers::new(false), Ledger::default());
        let explained = answered(
            &mut answers,
            &mut ledger,
            1,
            true,
            vec![miss("a", 1, 1, &[("its code", 7)]), miss("b", 1, 2, &[("its code", 8)])],
        );
        assert!(explained.record.is_empty());
        assert_eq!(causes(&answers, "a"), [RebuildCause::Twice { count: 2 }]);
        assert_eq!(causes(&answers, "b"), [RebuildCause::Twice { count: 2 }]);
        assert!(ledger.items.is_empty());
    }
    /// A workspace as its producers see it, shaped as the ripgrep edits that began lane
    /// why-always: each file's bytes and what code can observe of it.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct World {
        files: BTreeMap<&'static str, (u8, u8)>,
    }

    const MATCHER_RS: &str = "/w/crates/matcher/src/lib.rs";
    const GREP_RS: &str = "/w/crates/grep/src/lib.rs";
    const MAIN_RS: &str = "/w/crates/core/main.rs";
    const MATCHER: &str = "grep-matcher v0.1.8 (lib)";
    const GREP: &str = "grep v0.4.1 (lib)";
    const RG: &str = "ripgrep v15.1.0 (bin \"rg\")";
    const TEMPLATE: &str = "codegen dev template";

    impl World {
        fn new() -> World {
            World { files: [(MATCHER_RS, (1, 1)), (GREP_RS, (2, 2)), (MAIN_RS, (3, 3))].into() }
        }

        /// `path` edited: its bytes move, and with `code` what code can observe of it too.
        fn edit(&mut self, path: &str, code: bool) {
            let file = self.files.get_mut(path).unwrap();
            file.0 += 100;
            if code {
                file.1 += 100;
            }
        }

        fn file(&self, path: &str) -> Part {
            let (bytes, code) = self.files[path];
            Part::file(path, Some(id(bytes)), Some(id(code)))
        }

        /// Every step of a build of it, each as the miss its producer would give: three units
        /// (the program depends on both libraries, one of which depends on the other) and the
        /// program's two functions, whose code is what code can observe of its file.
        fn steps(&self) -> Vec<Miss> {
            let key = |parts: &[Part]| {
                let mut hasher = Hasher::new("blaze v2 test why 1");
                for part in parts {
                    hasher.part(part.name.as_bytes()).opt_part(part.hash.map(|it| it.0));
                }
                hasher.finish()
            };
            let unit = |item: &str, subject: u8, parts: Vec<Part>| {
                Miss::of_parts("unit", id(subject), item.to_owned(), key(&parts), parts)
            };
            let dep = |name: &str, item: &str, identity: ContentId| {
                Part::result(format!("the crate `{name}` it depends on"), Some(identity), Some(item.to_owned()))
            };
            let toolchain = || Part::new("the toolchain", Some(id(9)));
            let matcher = unit(MATCHER, 1, vec![toolchain(), self.file(MATCHER_RS)]);
            let grep = unit(
                GREP,
                2,
                vec![toolchain(), self.file(GREP_RS), dep("grep_matcher", MATCHER, matcher.key)],
            );
            let rg = unit(
                RG,
                3,
                vec![
                    toolchain(),
                    self.file(MAIN_RS),
                    dep("grep", GREP, grep.key),
                    dep("grep_matcher", MATCHER, matcher.key),
                ],
            );
            let template = |item: &str, subject: u8| {
                let code = id(self.files[MAIN_RS].1 ^ subject);
                let parts = vec![toolchain(), Part::result("its code", Some(code), None)];
                let mut miss =
                    Miss::of_parts(TEMPLATE, id(subject), item.to_owned(), key(&parts), parts);
                miss.via = Some(RG.to_owned());
                miss
            };
            let mut steps = vec![matcher, grep, rg, template("main", 4), template("rg::main", 5)];
            steps.sort();
            steps
        }
    }

    /// A daemon as `blaze why` sees it: the ledger, and the keys whose values the store holds.
    #[derive(Default)]
    struct Daemon {
        ledger: Ledger,
        store: BTreeSet<ContentId>,
        builds: u64,
    }

    impl Daemon {
        /// A build of `world`: the steps whose keys the store lacks are computed and kept,
        /// their misses given in `order` (sorted, as a job gives them, or turned by a number).
        fn build(&mut self, world: &World, order: usize) -> Explained {
            let mut misses: Vec<Miss> =
                world.steps().into_iter().filter(|it| self.store.insert(it.key)).collect();
            if !misses.is_empty() {
                let by = order % misses.len();
                misses.rotate_left(by);
                if order % 2 == 1 {
                    misses.reverse();
                }
            }
            self.builds += 1;
            self.ledger.explain(self.builds, true, misses)
        }
    }

    /// A cold build of the workspace, then `edit` and a second build: the worlds before and
    /// after, and the second build explained.
    fn edited(plant: Plant, order: usize, edit: impl FnOnce(&mut World)) -> (World, World, Explained) {
        let mut daemon = Daemon::default();
        daemon.ledger.plant = plant;
        let before = World::new();
        daemon.build(&before, 0);
        let mut after = before.clone();
        edit(&mut after);
        let explained = daemon.build(&after, order);
        (before, after, explained)
    }

    /// The edits a seed draws: each file left alone, its comments edited, or its code.
    fn drawn(seed: u64) -> impl FnOnce(&mut World) {
        move |world: &mut World| {
            for (at, path) in [MATCHER_RS, GREP_RS, MAIN_RS].into_iter().enumerate() {
                match (seed >> (2 * at)) % 3 {
                    0 => {}
                    1 => world.edit(path, false),
                    _ => world.edit(path, true),
                }
            }
        }
    }

    /// Every way the three files can each be left, commented or edited, and then some orders.
    const SEEDS: u64 = 64;

    /// Whether every step `explained` says was computed again has a chain that ends, and in
    /// no bug of blaze's.
    fn ends_explained(explained: &Explained) -> Result<(), String> {
        for it in &explained.rebuilt {
            if it.ends.is_empty() {
                return Err(format!("{} ({}) has no end", it.item, it.step));
            }
            for end in &it.ends {
                if let Reason::Unexplained { what } = end {
                    return Err(format!("{} ({}) is unexplained: {what}", it.item, it.step));
                }
            }
        }
        Ok(())
    }

    /// Whether every input `explained` blames is a file whose bytes differ between `before`
    /// and `after`, said to be trivia exactly when what code observes of it is the same.
    fn blames_what_changed(
        before: &World,
        after: &World,
        explained: &Explained,
    ) -> Result<(), String> {
        for it in &explained.rebuilt {
            for end in &it.ends {
                let Reason::InputChanged { input, trivia, .. } = end else { continue };
                let (Some(was), Some(now)) =
                    (before.files.get(input.as_str()), after.files.get(input.as_str()))
                else {
                    return Err(format!("{} blames `{input}`, which is no file of the build", it.item));
                };
                if was.0 == now.0 {
                    return Err(format!("{} blames {input}, which did not change", it.item));
                }
                if *trivia != (was.1 == now.1) {
                    return Err(format!("{}: {input} trivia {trivia}, wrongly", it.item));
                }
            }
        }
        Ok(())
    }

    /// The invariant: every recomputed step of a build has a reason whose chain ends in a
    /// terminal reason other than `Unexplained`.
    #[test]
    fn every_recomputed_step_ends_in_a_terminal_reason() {
        for seed in 0..SEEDS {
            let (_, _, explained) = edited(Plant::None, seed as usize, drawn(seed));
            ends_explained(&explained).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        }
    }

    /// The control of [`every_recomputed_step_ends_in_a_terminal_reason`]: with a result part
    /// not followed, a library's edit leaves the units that depend on it with no end.
    #[test]
    fn the_oracle_catches_a_result_part_not_followed() {
        let edit = |world: &mut World| world.edit(MATCHER_RS, true);
        ends_explained(&edited(Plant::None, 0, edit).2).unwrap();
        let planted = ends_explained(&edited(Plant::FollowNothing, 0, edit).2);
        assert!(planted.is_err_and(|why| why.contains(GREP) || why.contains(RG)), "the plant was missed");
    }

    /// The invariant: a chain's input is one the build's inputs really changed between the
    /// two builds, and it is called trivia exactly when no code changed in it.
    #[test]
    fn a_chain_blames_only_inputs_that_changed() {
        for seed in 0..SEEDS {
            let (before, after, explained) = edited(Plant::None, seed as usize, drawn(seed));
            blames_what_changed(&before, &after, &explained)
                .unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        }
    }

    /// The control of [`a_chain_blames_only_inputs_that_changed`]: with every part said to
    /// have changed, an edit of the program's file blames the toolchain too.
    #[test]
    fn the_oracle_catches_an_unchanged_part_blamed() {
        let edit = |world: &mut World| world.edit(MAIN_RS, true);
        let (before, after, explained) = edited(Plant::None, 0, edit);
        blames_what_changed(&before, &after, &explained).unwrap();
        let (before, after, explained) = edited(Plant::BlameEveryPart, 0, edit);
        let planted = blames_what_changed(&before, &after, &explained);
        assert!(planted.is_err_and(|why| why.contains("the toolchain")), "the plant was missed");
    }

    /// The invariant: the summary is deterministic for a given pair of trees: the same
    /// reports, and the same reasons, whatever order the build's misses arrive in.
    #[test]
    fn the_summary_is_the_same_whatever_order_the_misses_arrive_in() {
        for seed in 0..SEEDS {
            let sorted = edited(Plant::None, 0, drawn(seed)).2;
            for order in 1..6 {
                let turned = edited(Plant::None, order, drawn(seed)).2;
                assert_eq!(turned.rebuilt, sorted.rebuilt, "seed {seed}, order {order}");
                assert_eq!(summary(&turned.rebuilt), summary(&sorted.rebuilt), "seed {seed}");
            }
        }
    }

    /// The control of [`the_summary_is_the_same_whatever_order_the_misses_arrive_in`]: with a
    /// step's ends left as reached, the program, which follows both libraries, ends in another
    /// list when their misses arrive the other way round.
    #[test]
    fn the_oracle_catches_ends_in_the_order_reached() {
        let edit = |world: &mut World| {
            world.edit(MATCHER_RS, true);
            world.edit(GREP_RS, true);
        };
        let same = |plant: Plant| {
            let sorted = summary(&edited(plant, 0, edit).2.rebuilt);
            (1..6).all(|order| summary(&edited(plant, order, edit).2.rebuilt) == sorted)
        };
        assert!(same(Plant::None));
        assert!(!same(Plant::EndsAsReached), "the plant was missed");
    }

    /// A summary as a person reads it piped, paths from the workspace `/w`.
    fn said(explained: &Explained) -> String {
        let roots = blaze_report::Roots { workspace: Some("/w"), home: None };
        summary(&explained.rebuilt)
            .iter()
            .map(|it| blaze_report::render(it, blaze_report::Caps::PLAIN, &roots))
            .collect()
    }

    /// The three edits of ripgrep that began the lane, each summed up in one report that names
    /// the file: a body edit recomputes the program's unit and the functions whose code
    /// changed; a comment in the same file recomputes the unit alone, and one in a leaf crate
    /// every unit above it, both said to have changed no item, as wasted work.
    #[test]
    fn the_three_edits_name_the_file_and_say_when_no_item_changed() {
        let body = edited(Plant::None, 0, |world| world.edit(MAIN_RS, true)).2;
        let text = said(&body);
        assert_eq!(summary(&body.rebuilt).len(), 1, "{text}");
        assert!(text.starts_with("note: recomputed 3 steps because crates/core/main.rs changed"), "{text}");
        assert!(text.contains("codegen dev template: main, rg::main"), "{text}");
        assert!(text.contains("unit: ripgrep v15.1.0 (bin \"rg\")"), "{text}");
        assert!(!text.contains("comments only"), "{text}");

        let comment = edited(Plant::None, 0, |world| world.edit(MAIN_RS, false)).2;
        let text = said(&comment);
        assert_eq!(summary(&comment.rebuilt).len(), 1, "{text}");
        assert!(
            text.starts_with("warning: recomputed 1 step because crates/core/main.rs changed, though no item in it did (comments only): wasted work"),
            "{text}"
        );
        assert!(!text.contains(TEMPLATE), "no function was compiled again: {text}");

        let leaf = edited(Plant::None, 0, |world| world.edit(MATCHER_RS, false)).2;
        let text = said(&leaf);
        assert_eq!(summary(&leaf.rebuilt).len(), 1, "{text}");
        assert!(
            text.starts_with("warning: recomputed 3 steps because crates/matcher/src/lib.rs changed, though no item in it did (comments only)"),
            "{text}"
        );
        assert!(
            text.contains("unit: grep v0.4.1 (lib), grep-matcher v0.1.8 (lib), ripgrep v15.1.0 (bin \"rg\")"),
            "{text}"
        );
        // The chain of the program's unit ends at the leaf's file, through both libraries.
        let rg = leaf.rebuilt.iter().find(|it| it.item == RG).unwrap();
        assert_eq!(
            rg.ends,
            [Reason::InputChanged { file: true, input: MATCHER_RS.to_owned(), trivia: true }]
        );
    }

    /// Each terminal reason from the case that makes it (all but `KeyUnstable` and an eviction
    /// by memory or age, which nothing records yet), and what the summary says of it.
    #[test]
    fn each_terminal_reason_is_told_from_the_others() {
        let world = World::new();
        let ends = |explained: &Explained, item: &str| {
            explained.rebuilt.iter().find(|it| it.item == item).unwrap().ends.clone()
        };

        // A cold build: every step for the first time, in one line.
        let mut daemon = Daemon::default();
        let cold = daemon.build(&world, 0);
        assert!(cold.rebuilt.iter().all(|it| it.ends == [Reason::FirstTime]), "{:?}", cold.rebuilt);
        assert_eq!(summary(&cold.rebuilt).len(), 1);
        assert!(said(&cold).starts_with("note: computed 5 steps for the first time"), "{}", said(&cold));
        // A build that computes nothing again says nothing.
        assert!(said(&daemon.build(&world, 0)).is_empty());

        // The cap shrank the records to tombstones: evicted by it, never a first compute.
        let mut edited = world.clone();
        edited.edit(MAIN_RS, true);
        assert_eq!(daemon.ledger.prune_to(0, TOMBS), 5);
        let evicted = daemon.build(&edited, 0);
        let by_cap = [Reason::Evicted { by: EvictedBy::Cap, when: 1 }];
        assert_eq!(ends(&evicted, RG), by_cap);
        assert_eq!(ends(&evicted, "main"), by_cap);
        assert!(said(&evicted).contains("ledger dropped its records"), "{}", said(&evicted));
        // The cap is of what shrinking frees: tombstones, however many, shrink no subject
        // that is whole. The build above computed some again; with the cap at exactly what
        // those hold beyond a tombstone's bytes, nothing goes, though the ledger's whole
        // weight (the tombstones' with it) is past that cap.
        let whole = |ledger: &Ledger| -> usize {
            ledger.items.values().filter(|it| !it.tomb).map(|it| weight(it) - bare(it)).sum()
        };
        let all: usize = daemon.ledger.items.values().map(weight).sum();
        let held = whole(&daemon.ledger);
        assert!(held > 0 && all > held, "some subjects are whole and some are tombstones");
        assert_eq!(daemon.ledger.prune_to(held, TOMBS), 0, "the last build's records stay whole");
        assert_eq!(whole(&daemon.ledger), held);
        // Written whole and read back, a tombstone is one still.
        let restored = Ledger::restore(&daemon.ledger.file()).ledger;
        assert_eq!(restored.items, daemon.ledger.items);
        // Past the tombstones' own cap the oldest go, which is said.
        assert!(daemon.ledger.prune_to(0, 0) > 0 && daemon.ledger.dropped().is_some());

        // A compute in a build that failed, and the same key again.
        let mut ledger = Ledger::default();
        let rg = || world.steps().into_iter().filter(|it| it.item == RG).collect::<Vec<_>>();
        ledger.explain(1, false, rg());
        let failed = ledger.explain(2, true, rg());
        assert_eq!(ends(&failed, RG), [Reason::PreviousFailed { since: 1 }]);

        // The same key after a build that ended well: nothing recorded dropped its value.
        let lost = ledger.explain(3, true, rg());
        assert!(
            matches!(ends(&lost, RG).as_slice(), [Reason::Unexplained { .. }]),
            "{:?}",
            lost.rebuilt
        );
        let text = said(&lost);
        assert!(
            text.starts_with("warning: 1 step recomputed for no recorded reason: a blaze bug (`blaze why --unexplained`)"),
            "{text}"
        );
        let mut answers = Answers::new(false);
        answers.add(Answer::new(3, "/w".to_owned(), "build".to_owned(), lost.rebuilt.clone()), None);
        let asked = answers.asked(&Ask { unexplained: true, build: Some(3), ..Ask::default() });
        assert_eq!((asked.count, asked.rebuilt.len()), (1, 1));
        assert_eq!(answers.asked(&Ask { build: Some(2), ..Ask::default() }).build, None);

        // A build script that runs outside the sandbox runs every build, as asked.
        let mut run = Miss::of_parts("build script run", id(7), "z v1 (build script run)".to_owned(), id(0), Vec::new());
        run.missed = Missed::Always("it runs outside the sandbox".to_owned());
        let forced = Ledger::default().explain(1, true, vec![run]);
        assert_eq!(
            forced.rebuilt[0].ends,
            [Reason::Forced { how: "it runs outside the sandbox".to_owned() }]
        );
        assert!(said(&forced).contains("that was asked for: it runs outside the sandbox"));
    }
}
