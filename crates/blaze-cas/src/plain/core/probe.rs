//! The lazy open's reads (drive.md, "The store under the command", point 7; D114 piece 3, with
//! the coordinator's three answers of 2026-11-16).
//!
//! The open reads the head and its manifest and maps NO segment. A segment is mapped at the
//! first probe that reaches it ([`Plain::find`]): one `Effect::Map` for that one row, its proof
//! made as the eager open made it (this boot's row, or a sealed row: no byte read), except an
//! unsealed segment of an earlier boot (the crash tail). That one is mapped and its length
//! compared with its row, then PARKED: its index is searched before the proof, only to decide
//! whether the one full check (`Verified::check`) is due, and the check runs whole at its FIRST
//! HIT, once a boot (recorded for the boot by the next flip, as the open's check was). A miss
//! in a parked run is a miss: its records count as lost until checked, and a key that matches
//! out of torn bytes is a 2^-256 event, never served unchecked.
//!
//! WHO CALLS WHAT. The main thread's gets go through [`Plain::find`] (in the command:
//! `Driven::get`): the nothing-changed command maps the one or two segments its certificate and
//! leaf table lie in, and no more. Pool tasks read through the lock-free handle (handle.rs),
//! which cannot map, so [`Plain::map_all`] is asked once, when the request's first pool task
//! is about to start (the certificate missed): every row mapped, every parked row checked
//! (a pool miss would recompute what the segment holds, which costs more than hashing it).
//! From then on the store is WHOLE (`eager`): every later generation is mapped as the eager open
//! mapped it, and the `&self` reads ([`Plain::get`], [`Plain::locate`]) answer for every row.
//! A commit, a compaction slice and a scrub need a whole store; `Driven` asks [`Plain::map_all`]
//! before each.
//!
//! PROBE ORDER (answer Q1): one order, largest segment (by records) first, ties newest first.
//! A root is a NAME in the newest manifest, never a segment probe, and records are keyed by
//! content (two values under one key are refused), so the order cannot change an answer, only
//! how many probes a get makes. Computed once a generation ([`Plain::order_view`]).
//!
//! THE GRACE. A file retired by another process's compaction is unlinked [`GRACE_NS`] after
//! its retirement, and a generation read at the open names rows retired after it; a mapping
//! outlives an unlink, a map asked after it does not. So a map is asked within [`GRACE_NS`] of
//! the open or refused by name ([`StoreError::Lost`]): never a lost row taken for a power cut's.
//! A short command cannot reach it. A long-lived holder (the `blaze run` parent, any helper)
//! calls [`Plain::map_all`] once its child has started, off the path to `main`, and re-opens
//! the head whenever it needs a record committed after its own open.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Allocator;

use blaze_core::Effect;
use blaze_own::Share;
use blaze_keyed::{
    kind::{Namespace, ReservedTag, Tag},
    store::StoreError,
};

use super::super::format::{Entry, LostKind};
use super::super::verified::{Unverified, Verified};
use super::{GRACE_NS, Got, Phase, Placed, Plain, Plant, Then};
use crate::Key;

/// Why a map asked late is refused ([`StoreError::Lost`]'s words).
const LATE_MAP: &str = "named by the generation this process opened, and its first probe came more than the grace period after the open: another process's compaction may have unlinked it since; a command maps every segment (Driven::map_all) before its first pool task";

/// What one call of [`Plain::find`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    /// Found: its bytes are [`Plain::got`]'s.
    Hit(Found),
    /// No namespace asked holds the key: every row of theirs was probed (mapped, or parked and
    /// missed) and nothing held uncommitted answers.
    Absent,
    /// The next row in probe order is not mapped: one `Effect::Map` was pushed for it alone.
    /// Run the effects, hand the completion to [`Plain::complete`], and ask again.
    Map,
}

/// A record found by [`Plain::find`]: read it with [`Plain::got`]. A mapped segment's place is
/// valid for the life of the store (maps are never removed); a record held uncommitted is found
/// again by its key, held or (after a commit) in the segment the commit wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    key: Key,
    tag: Tag,
    namespace: Namespace,
    at: At,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    Held,
    Mapped { map: usize, entry: Entry },
}

impl Found {
    pub fn key(&self) -> Key {
        self.key
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    pub fn namespace(&self) -> Namespace {
        self.namespace
    }
}

/// What the gets of one process probed, for `--stats` ("probes per get: mean, most").
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Probes {
    /// Gets asked through [`Plain::find`] to an answer ([`Plain::count_get`]).
    pub gets: u64,
    /// Runs probed by them, mapped or parked.
    pub probes: u64,
    /// The most runs one get probed.
    pub most: u32,
}

impl<A: Allocator + Clone> Plain<A> {
    /// The open, for a process that reads through the lock-free handle from its start (the
    /// handle's own open, a push of whole segments, the eager DST): every segment the
    /// generation names mapped at the open, an earlier boot's unsealed segment checked there,
    /// and every later generation mapped whole. The command's open is [`Plain::open`] (lazy).
    pub fn open_whole(&mut self, effects: &mut Vec<Effect>) {
        self.eager = true;
        self.open(effects);
    }

    /// Whether every row is mapped (and every parked row checked) from now on: after
    /// [`Plain::map_all`] or [`Plain::open_whole`].
    pub fn whole(&self) -> bool {
        self.eager
    }

    /// `(segments mapped, segments named)` of this generation's local rows, for `--stats`
    /// ("segments mapped N of M named"). A parked row counts as mapped; a lost one as neither.
    pub fn mapped_of_named(&self) -> (u32, u32) {
        let mut mapped = 0u32;
        let mut named = 0u32;
        for seg in self.manifest.segments.iter().filter(|it| it.local && !self.dropped.contains(&it.id)) {
            named += 1;
            if self.by_id.contains_key(&seg.id) || self.unchecked.contains_key(&seg.id) {
                mapped += 1;
            }
        }
        (mapped, named)
    }

    /// What this process's gets probed.
    pub fn probes(&self) -> Probes {
        self.probes
    }

    /// One get ended (an answer, hit or absent): `before` is [`Probes::probes`] as it began.
    pub fn count_get(&mut self, before: u64) {
        let this = self.probes.probes.saturating_sub(before);
        self.probes.gets += 1;
        self.probes.most = self.probes.most.max(u32::try_from(this).unwrap_or(u32::MAX));
    }

    /// The probe order of this generation: its local rows not lost, largest (records) first,
    /// ties newest (a later row) first. Called whenever a generation is adopted.
    pub(super) fn order_view(&mut self) {
        let segs = &self.manifest.segments;
        let dropped = &self.dropped;
        self.view.clear();
        self.view.extend((0..segs.len()).filter(|&row| segs[row].local && !dropped.contains(&segs[row].id)));
        self.view.sort_unstable_by(|&a, &b| segs[b].count.cmp(&segs[a].count).then(b.cmp(&a)));
        self.last_hit = None;
    }

    /// The mapped segment of `row`, when it is mapped, proved and not lost.
    pub(crate) fn mapped_row(&self, row: usize) -> Option<usize> {
        let seg = &self.manifest.segments[row];
        if self.dropped.contains(&seg.id) {
            return None;
        }
        self.by_id.get(&seg.id).copied()
    }

    /// [`Plain::find`] in [`super::PlainConfig::reads`], in their order.
    pub fn find_read(&mut self, key: &Key, now_ns: u64, effects: &mut Vec<Effect>) -> Result<Probe, StoreError> {
        // Moved out and back (no allocation): `find` takes `&mut self` and the namespaces.
        // An empty vector in the store's allocator holds no block (`new_in` does not allocate).
        let reads = std::mem::replace(&mut self.cfg.reads, Vec::new_in(self.alloc.clone()));
        let out = self.find(&reads, key, now_ns, effects);
        self.cfg.reads = reads;
        out
    }

    /// The record under `key` in `spaces` (asked in their order: in each, what this process
    /// holds uncommitted, then already-mapped rows, then an optional creation-generation hint,
    /// then remaining unmapped rows in probe order with verified Bloom filtering), mapping at
    /// most ONE segment a call: see the module's docs. The store must be ready (not mid-commit).
    pub fn find(&mut self, spaces: &[Namespace], key: &Key, now_ns: u64, effects: &mut Vec<Effect>) -> Result<Probe, StoreError> {
        if let Some(why) = &self.broken {
            return Err(why.clone());
        }
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        let hint_gen = self.hint_gen;
        let every = self.plant() == Plant::IgnoresNamespace;
        for space in spaces {
            if let Some((_, entry)) = self.held_in(*space, key) {
                return Ok(Probe::Hit(Found { key: *key, tag: entry.tag, namespace: *space, at: At::Held }));
            }
            // Pass 1: probe already-mapped segments first (zero syscalls / zero mmaps), starting
            // with the most recently hit segment for intra-commit locality (root -> pointer -> lines).
            let mut tried_last: Option<usize> = None;
            if let Some(last_row) = self.last_hit
                && let Some(seg) = self.manifest.segments.get(last_row).copied()
                && seg.local
                && (every || seg.namespace == *space)
                && !self.dropped.contains(&seg.id)
                && let Some(&map) = self.by_id.get(&seg.id)
            {
                tried_last = Some(last_row);
                if self.may_contain_row(last_row, key) {
                    self.probes.probes += 1;
                    if let Some((map, entry)) = self.probe(map, key) {
                        return Ok(Probe::Hit(Found { key: *key, tag: entry.tag, namespace: seg.namespace, at: At::Mapped { map, entry } }));
                    }
                }
            }
            for i in 0..self.view.len() {
                let Some(&row) = self.view.get(i) else {
                    break;
                };
                if tried_last == Some(row) {
                    continue;
                }
                let Some(&seg) = self.manifest.segments.get(row) else {
                    continue;
                };
                if (!every && seg.namespace != *space) || self.dropped.contains(&seg.id) {
                    continue;
                }
                let Some(&map) = self.by_id.get(&seg.id) else {
                    continue;
                };
                if !self.may_contain_row(row, key) {
                    continue;
                }
                self.probes.probes += 1;
                if let Some((map, entry)) = self.probe(map, key) {
                    self.last_hit = Some(row);
                    return Ok(Probe::Hit(Found { key: *key, tag: entry.tag, namespace: seg.namespace, at: At::Mapped { map, entry } }));
                }
            }
            // Pass 2: if a creation generation hint was supplied (e.g. from a manifest root),
            // probe unmapped segments of that generation before walking older/larger segments.
            if let Some(target_gen) = hint_gen {
                for i in 0..self.view.len() {
                    let Some(&row) = self.view.get(i) else {
                        break;
                    };
                    let Some(&seg) = self.manifest.segments.get(row) else {
                        continue;
                    };
                    if seg.generation != target_gen || (!every && seg.namespace != *space) || self.dropped.contains(&seg.id) {
                        continue;
                    }
                    if self.by_id.contains_key(&seg.id) || !self.may_contain_row(row, key) {
                        continue;
                    }
                    if let Some(probe) = self.probe_row(row, key, now_ns, effects)? {
                        if matches!(probe, Probe::Hit(_)) {
                            self.last_hit = Some(row);
                        }
                        return Ok(probe);
                    }
                }
            }
            // Pass 3: remaining unmapped / parked segments in probe order, skipping segments whose
            // verified Bloom filter proves `key` is absent.
            for i in 0..self.view.len() {
                let row = self.view[i];
                let seg = self.manifest.segments[row];
                if (!every && seg.namespace != *space) || self.dropped.contains(&seg.id) {
                    continue;
                }
                if self.by_id.contains_key(&seg.id) || !self.may_contain_row(row, key) {
                    continue;
                }
                if let Some(probe) = self.probe_row(row, key, now_ns, effects)? {
                    if matches!(probe, Probe::Hit(_)) {
                        self.last_hit = Some(row);
                    }
                    return Ok(probe);
                }
            }
            if every {
                break;
            }
        }
        if every
            && let Some((namespace, entry)) = self.held.iter().chain(self.sealed.iter()).find_map(|held| held.entry(key).map(|entry| (held.batch.namespace(), *entry)))
        {
            return Ok(Probe::Hit(Found { key: *key, tag: entry.tag, namespace, at: At::Held }));
        }
        Ok(Probe::Absent)
    }

    /// One row of a lookup, `row` not lost: `Some(Hit)`; `Some(Map)` when it is not mapped yet
    /// (one `Effect::Map` pushed for it alone); `None` when it does not hold `key` (a miss in a
    /// parked run is a miss; a parked run that names it is checked whole now, its first hit).
    fn probe_row(&mut self, row: usize, key: &Key, now_ns: u64, effects: &mut Vec<Effect>) -> Result<Option<Probe>, StoreError> {
        let seg = self.manifest.segments[row];
        let hit = |map: usize, entry: Entry| Probe::Hit(Found { key: *key, tag: entry.tag, namespace: seg.namespace, at: At::Mapped { map, entry } });
        if let Some(&map) = self.by_id.get(&seg.id) {
            self.probes.probes += 1;
            return Ok(self.probe(map, key).map(|(map, entry)| hit(map, entry)));
        }
        if let Some(parked) = self.unchecked.get(&seg.id) {
            self.probes.probes += 1;
            if !parked.names(key) {
                // A miss in an unproved run is a miss: nothing of it is served.
                return Ok(None);
            }
            // Its first hit: the one full check, now. Lost by its check (reported), or its
            // proved index does not hold the key: a miss.
            self.check_parked(seg.id);
            return Ok(self.mapped_row(row).and_then(|map| self.probe(map, key)).map(|(map, entry)| hit(map, entry)));
        }
        self.ask_map(row, now_ns, effects)?;
        Ok(Some(Probe::Map))
    }

    /// The rows a replaceable record's read walks: `space`'s rows held here, NEWEST first (the
    /// manifest's order is the commit order: a commit appends its rows, a commit of bytes named
    /// already moves that row to the end, and a merge replaces a contiguous run of a
    /// namespace's rows in place: `build` and `plan`). Not [`Plain::order_view`]'s order
    /// (largest first), which serves content-keyed records, equal in every row.
    fn newest_rows(&self, space: Namespace) -> impl Iterator<Item = usize> + '_ {
        let rows = self.manifest.segments.len();
        let oldest = self.plant() == Plant::PointerOldest;
        (0..rows).map(move |i| if oldest { i } else { rows - 1 - i }).filter(move |&row| {
            let seg = &self.manifest.segments[row];
            seg.local && seg.namespace == space && !self.dropped.contains(&seg.id)
        })
    }

    /// The NEWEST record under `key` in `space`: what this process holds uncommitted, then the
    /// rows of `space` newest first ([`Plain::newest_rows`]), the first that holds it answering;
    /// a row is mapped at its first probe, as [`Plain::find`] maps. What a pointer is read by
    /// ([`ReservedTag::Pointer`]): its newer value lies in a later row. At most one probe a row,
    /// so at most [`super::SEGMENTS_MOST`] (the tiered merge's bound: `Tiers`; and the
    /// rows committed since the last cleanup) a read.
    pub fn find_newest(&mut self, space: Namespace, key: &Key, now_ns: u64, effects: &mut Vec<Effect>) -> Result<Probe, StoreError> {
        if let Some(why) = &self.broken {
            return Err(why.clone());
        }
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        if let Some((_, entry)) = self.held_in(space, key) {
            return Ok(Probe::Hit(Found { key: *key, tag: entry.tag, namespace: space, at: At::Held }));
        }
        let mut rows: Vec<usize, A> = Vec::new_in(self.alloc.clone());
        rows.extend(self.newest_rows(space));
        for row in rows {
            if !self.may_contain_row(row, key) {
                continue;
            }
            if let Some(probe) = self.probe_row(row, key, now_ns, effects)? {
                return Ok(probe);
            }
        }
        Ok(Probe::Absent)
    }

    /// [`Plain::find_newest`] over the mapped rows alone, committed records only: the map index
    /// and the entry. The commit's layout and compaction ask it, over a whole store.
    pub(crate) fn newest_mapped(&self, space: Namespace, key: &Key) -> Option<(usize, Entry)> {
        self.newest_rows(space)
            .filter(|&row| self.may_contain_row(row, key))
            .filter_map(|row| self.mapped_row(row))
            .find_map(|at| self.probe(at, key))
    }

    /// The bytes of what [`Plain::find`] found, in place: no check (a mapped segment is
    /// proved; a held record is this process's own).
    pub fn got(&self, found: Found) -> Got<'_> {
        let (map, entry) = match found.at {
            At::Mapped { map, entry } => (map, entry),
            At::Held => {
                if let Some(got) = self.held_get(found.namespace, &found.key) {
                    return got;
                }
                // Committed since it was found: a commit maps what it wrote (a commit needs a
                // whole store, and a whole store maps every generation it adopts).
                // A pointer's value is its newest row's.
                let committed = if ReservedTag::Pointer.is(found.tag) { self.newest_mapped(found.namespace, &found.key) } else { self.find_in(std::slice::from_ref(&found.namespace), &found.key) };
                committed.expect("a record found held is still held, or committed into a segment this process mapped after its flip")
            }
        };
        let mut checks = self.checks.get();
        checks.reads += 1;
        self.checks.set(checks);
        let mapped = &self.maps[map];
        Got { tag: entry.tag, namespace: mapped.seg().namespace, bytes: mapped.payload(&entry) }
    }

    /// Where what [`Plain::find`] found lies, for a `pread` into the caller's buffer; `None`
    /// for a record held uncommitted (read it with [`Plain::got`]).
    pub fn placed(&self, found: Found) -> Option<Placed> {
        let At::Mapped { map, entry } = found.at else { return None };
        let mapped = &self.maps[map];
        Some(Placed { key: found.key, tag: entry.tag, namespace: mapped.seg().namespace, segment: mapped.seg().digest, file: mapped.file().share(), at: entry.at, len: entry.len })
    }

    /// One row's map asked, within the grace of the open (see the module's docs).
    fn ask_map(&mut self, row: usize, now_ns: u64, effects: &mut Vec<Effect>) -> Result<(), StoreError> {
        let seg = self.manifest.segments[row];
        if now_ns.saturating_sub(self.opened_ns) > GRACE_NS {
            return Err(StoreError::Lost { segment: seg.id, generation: self.manifest.generation, what: LATE_MAP });
        }
        self.queue_map(row, seg.id, effects);
        self.phase = Phase::Mapping { left: 1, then: Then::Probe };
        Ok(())
    }

    /// Whether a map completing now parks an earlier boot's unsealed segment (a probe's map)
    /// rather than checking it at once (the open's eager maps, [`Plain::map_all`]'s).
    pub(super) fn parks(&self) -> bool {
        matches!(self.phase, Phase::Mapping { then: Then::Probe, .. })
    }

    /// An earlier boot's unsealed segment, mapped by a probe: its length compared with its row
    /// (a number in hand), then parked until its first hit. A file of another length is lost by
    /// name, as the full check would lose it.
    pub(super) fn park(&mut self, unverified: Unverified) -> Result<(), StoreError> {
        let seg = *unverified.seg();
        if unverified.len() != seg.file_len() {
            let file = self.segment_file(seg.id);
            let why = format!(
                "it was written in an earlier boot and did not survive its end whole (its file is {} bytes, its generation names {}): a power cut before the kernel wrote it back; its records are dropped",
                unverified.len(),
                seg.file_len()
            );
            self.lose(seg, file, LostKind::NotWhole, why);
            return Ok(());
        }
        self.unchecked.insert(seg.id, unverified);
        Ok(())
    }

    /// The parked segment `id`'s one full check (its first hit, or [`Plain::map_all`]): proved
    /// into the maps and recorded for the boot by the next flip, or lost by name.
    pub(super) fn check_parked(&mut self, id: u64) {
        let Some(unverified) = self.unchecked.remove(&id) else { return };
        let proved = if self.plant() == Plant::TrustsUnchecked {
            // The planted bug: a hit in an unproved run served with no check.
            Ok(self.trust_for_a_plant(unverified))
        } else {
            let mut checks = self.checks.get();
            let proved = Verified::check(unverified, self.boot, &mut checks);
            self.checks.set(checks);
            self.checked_ids.push(id);
            proved
        };
        match proved {
            Ok(proved) => {
                if self.plant() != Plant::TrustsUnchecked {
                    self.checked.insert(id);
                    self.report.checked += 1;
                }
                self.by_id.insert(id, self.maps.len());
                self.maps.push(proved);
            }
            Err((seg, file, what)) => {
                let why = format!("it was written in an earlier boot and did not survive its end whole ({what}): a power cut before the kernel wrote it back; its records are dropped");
                self.lose(seg, file, LostKind::NotWhole, why);
            }
        }
    }

    /// Every row of this generation mapped and every parked row checked, then whole for good:
    /// asked once, when the request's first pool task is about to start (the handle cannot
    /// map), and before a commit, a compaction or a scrub. `false`: whole already, nothing
    /// asked. Within the grace of the open (see the module's docs).
    pub fn map_all(&mut self, now_ns: u64, effects: &mut Vec<Effect>) -> Result<bool, StoreError> {
        if let Some(why) = &self.broken {
            return Err(why.clone());
        }
        if !self.ready() {
            return Err(StoreError::Busy { phase: self.what() });
        }
        if self.eager {
            return Ok(false);
        }
        let late = now_ns.saturating_sub(self.opened_ns) > GRACE_NS;
        let mut to_map = 0usize;
        let mut sealed_to_map = 0usize;
        for seg in &self.manifest.segments {
            if !seg.local || self.by_id.contains_key(&seg.id) || self.dropped.contains(&seg.id) || self.unchecked.contains_key(&seg.id) {
                continue;
            }
            if late {
                return Err(StoreError::Lost { segment: seg.id, generation: self.manifest.generation, what: LATE_MAP });
            }
            to_map = to_map.saturating_add(1);
            if seg.boot != self.cfg.boot && seg.sealed {
                sealed_to_map = sealed_to_map.saturating_add(1);
            }
        }
        if to_map == 0 {
            self.mapped(Then::All, effects);
            return Ok(true);
        }
        self.reserve_maps(to_map, sealed_to_map, effects);
        let mut left = 0u32;
        let len = self.manifest.segments.len();
        for row in 0..len {
            let Some(seg) = self.manifest.segments.get(row).copied() else { continue };
            if !seg.local || self.by_id.contains_key(&seg.id) || self.dropped.contains(&seg.id) || self.unchecked.contains_key(&seg.id) {
                continue;
            }
            self.queue_map(row, seg.id, effects);
            left = left.saturating_add(1);
        }
        if left == 0 {
            self.mapped(Then::All, effects);
        } else {
            self.phase = Phase::Mapping { left, then: Then::All };
        }
        Ok(true)
    }

    /// [`Then::All`]'s end: every parked row checked, whole from now on.
    pub(super) fn all_mapped(&mut self) {
        let mut parked: Vec<u64, A> = Vec::new_in(self.alloc.clone());
        parked.extend(self.unchecked.keys().copied());
        for id in parked {
            self.check_parked(id);
        }
        self.eager = true;
    }
}
