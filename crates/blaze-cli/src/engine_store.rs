//! The puts of `blaze engine-check` (engine.md E6; "The design" 5): after each phase's merge,
//! the records its tasks made go to the store in bulk, sorted by key (rule 25), so the next
//! phase's tasks read them through the store as the old functions expect (`work::payload`).
//!
//! A record is put in the engine's form: one whole record (blaze-drive record.rs
//! `write_plain`) holding its name, source, out, read list and payload, under its task's key.
//! Its findings are in the engine's answer rows, not in the record (the persisted answer is the
//! next piece). The keys are the engine's own ([`tools`]): another form of record under its own
//! keys, so the old machine's record of one task and the engine's never meet under one key
//! (rule 21).

use blaze_drive::{Finding, Key, Name, Part, PathOf, Tool};
use blaze_engine::{STAGES, Stage, Tools};
use blaze_ids::ContentId;
use bstd::cx::Cx;

use crate::CliError;
use crate::store::Store;
use crate::work::{Extras, Record};

/// The domain of the engine's per-kind tools: every kind's tool (home.rs `tools`) hashed under
/// it, so an edit of one producer still moves only its kinds' keys (rule 52).
/// 2: an outline record lists its parts, paths and findings (`write_listed`), other bytes
/// than form 1's under the same task key, so every key moves (rule 21).
/// 3: a units record lists its parts too (the run's walk from `Name::units()` reads them:
/// engine_check.rs's put, run_ids.rs `workspace_of`), and the Bodies stage puts a body's run
/// facts (engine_body_facts.rs): other bytes under form 2's task keys.
const ENGINE_FORM: &str = "blaze-cli engine record form 3";

/// The engine's tool of each kind, from the kinds' tools (home.rs `tools`).
pub(crate) fn tools(kinds: &[Tool; blaze_drive::key::KINDS]) -> Tools {
    Tools(kinds.map(|tool| Tool(ContentId::of(ENGINE_FORM, &tool.0.0))))
}

const RECORD_FORM: &str = "blaze-cli engine record key 2";

/// The store key of the record a task's entry made: its task key (text, tool) and the record's
/// own bytes (out, read list, payload), so two entries of one task that made other bytes write
/// under two keys and two values never meet under one (rule 21), whatever the payload read that
/// its cells do not say. The stored answer keeps each row's key (pages.rs `records`), so a
/// request reads the record of a row that stands by it.
pub(crate) fn record_key(key: Key, out: ContentId, reads: &[(Name, ContentId)], payload: &[u8]) -> Key {
    let mut h = blaze_ids::Hasher::new(RECORD_FORM);
    h.update(&key.0.0).update(&out.0).update(&(reads.len() as u64).to_le_bytes());
    for (read, digest) in reads.iter() {
        h.update(&[read.kind as u8]).update(&read.subject.0).update(&digest.0);
    }
    h.update(&(payload.len() as u64).to_le_bytes()).update(payload);
    Key(h.finish())
}

/// One record a task of the merged phase made, to put under its task's key.
pub(crate) struct Put<'r> {
    pub(crate) key: Key,
    pub(crate) name: Name,
    pub(crate) source: ContentId,
    pub(crate) out: ContentId,
    pub(crate) reads: &'r [(Name, ContentId)],
    pub(crate) payload: &'r [u8],
    pub(crate) head: PutHead<'r>,
    /// The records the task made beside its own (a `CrateScope`'s expansion records,
    /// work.rs `Record::extras`), put right after it, in its namespace.
    pub(crate) extras: Option<&'r Extras>,
}

pub(crate) enum PutHead<'r> {
    /// The worker encoded this header alongside the key and payload.
    Prepared(&'r [u8]),
    /// Encode a plain header, or include an outline's parts, paths and findings for `outline_from`.
    Build(Option<Listing<'r>>),
}

/// A test's planted bug: every expansion record whose text holds `marker` is not put, so a
/// reader of it meets [`blaze_scope::expansion_records::ExpansionRecordsError::Missing`].
#[cfg(test)]
pub(crate) fn plant_missing_expansion(marker: Option<&'static str>) {
    MISSING_EXPANSION.with(|plant| plant.set(marker));
}

#[cfg(test)]
std::thread_local! {
    static MISSING_EXPANSION: core::cell::Cell<Option<&'static str>> = const { core::cell::Cell::new(None) };
}

/// Whether the plant skips the record `bytes` (never outside a test).
fn planted_missing(bytes: &[u8]) -> bool {
    #[cfg(test)]
    if let Some(marker) = MISSING_EXPANSION.with(|plant| plant.get()) {
        return bytes.windows(marker.len()).any(|window| window == marker.as_bytes());
    }
    let _ = bytes;
    false
}

/// What an outline record lists besides its payload, as `work::Record` holds it.
#[derive(Clone, Copy)]
pub(crate) struct Listing<'r> {
    pub(crate) parts: &'r [Part],
    /// (name, start, end, a module slot) into `path_bytes`.
    pub(crate) paths: &'r [(Name, u32, u32, bool)],
    pub(crate) path_bytes: &'r [u8],
    /// (rule, start, end, message start, message end) into `messages`.
    pub(crate) findings: &'r [(u16, u32, u32, usize, usize)],
    pub(crate) messages: &'r [u8],
}

impl<'r> Listing<'r> {
    pub(crate) fn of<A: core::alloc::Allocator + Copy>(record: &'r Record<A>) -> Listing<'r> {
        Listing { parts: &record.parts, paths: &record.paths, path_bytes: &record.path_bytes, findings: &record.findings, messages: &record.messages }
    }
}

/// Puts every record in key order, borrowing a prepared header or building one in `alloc`.
/// Counts each put in `ledger` under `stage` and returns the number of records.
pub(crate) fn put_all(cx: &Cx, store: &mut dyn Store<'_>, puts: &mut [Put<'_>], stage: Stage, ledger: &mut Ledger, alloc: blaze_own::Alloc<'_>) -> Result<u32, CliError> {
    let cx = cx.span("engine: a phase's records put");
    puts.sort_unstable_by_key(|put| put.key);
    for put in puts.iter() {
        // The head (its name, reads, listing and the payload's length) and the payload as two
        // slices: the payload goes to the store from the buffer its task made it in, never joined.
        let built;
        let head: &[u8] = match &put.head {
            PutHead::Prepared(bytes) => bytes,
            PutHead::Build(None) => {
                built = blaze_drive::record::write_plain_head(put.name, put.source, put.out, put.reads, put.payload.len(), alloc);
                &built
            }
            PutHead::Build(Some(listing)) => {
                let mut paths = std::vec::Vec::with_capacity_in(listing.paths.len(), alloc);
                paths.extend(listing.paths.iter().map(|&(name, at, to, module)| PathOf { name, path: &listing.path_bytes[at as usize..to as usize], module }));
                let mut findings = std::vec::Vec::with_capacity_in(listing.findings.len(), alloc);
                findings.extend(listing.findings.iter().map(|&(rule, start, end, at, to)| Finding { rule, start, end, message: &listing.messages[at..to] }));
                let listed = blaze_drive::record::Listed { parts: listing.parts, paths: &paths, findings: &findings };
                built = blaze_drive::record::write_listed_head(put.name, put.source, put.out, put.reads, &listed, put.payload.len(), alloc);
                &built
            }
        };
        let parts = [head, put.payload];
        ledger.put(stage, put.key, &parts, put.payload.len());
        store.put_parts(&cx, put.key, &parts)?;
        // Its extra records after it, in key order (sorted by their writer, expansion_records.rs
        // `write`), each in its owner's namespace (`parts`).
        if let Some(extras) = put.extras {
            let (mut written, mut skipped) = (0u32, 0u32);
            let mut beside = std::vec::Vec::with_capacity_in(extras.records.len(), alloc);
            for (key, bytes) in extras.records.iter() {
                if planted_missing(bytes) {
                    skipped += 1;
                    continue;
                }
                let key = Key(*key);
                let put = store.put_record(&cx, key, blaze_keyed::kind::ReservedTag::Expansion.tag(), &parts, bytes)?;
                written += u32::from(put == blaze_keyed::store::Put::Written);
                ledger.put_extra(stage, key, bytes, put);
                beside.push(key);
            }
            // Their keys filed beside their owner: a root naming the owner keeps them (store.rs
            // `CommandStore::pin_through`).
            if !beside.is_empty() {
                store.index_beside(&cx, put.key, &beside)?;
            }
            // A planted skip is no row served: the same count is the rows a record held served.
            ledger.extras_named(stage, extras.calls.saturating_sub(skipped), written, extras.bytes());
        }
    }
    Ok(u32::try_from(puts.len()).unwrap_or(u32::MAX))
}

/// Why a stored outline could not be read back: the request stops, naming it (rule 3; a row
/// that stands names a record its own request put, so its absence is a bug of the store or of
/// the stored answer, never a cue to outline the file again).
#[derive(Debug)]
pub enum OutlineBack {
    /// No record is at the key the stored answer names for the file's row.
    Missing { name: Name, key: Key },
    /// The bytes there are no drive record, or not a whole one.
    Unreadable { name: Name, key: Key, why: Option<blaze_drive::WireError> },
}

/// The outline record `bytes` of `name` (put by [`put_all`] with its listing) as the
/// planner's `work::Record`, in `alloc`: equal to the outline its file's bytes make (rule 26).
pub(crate) fn outline_from<A: core::alloc::Allocator + Copy>(name: Name, key: Key, bytes: &[u8], alloc: A) -> Result<Record<A>, OutlineBack> {
    let view = match blaze_drive::record::open(bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) if view.name == name => view,
        Ok(_) => return Err(OutlineBack::Unreadable { name, key, why: None }),
        Err(why) => return Err(OutlineBack::Unreadable { name, key, why: Some(why) }),
    };
    let mut made = Record::new_in(name, alloc);
    made.out = view.out;
    let parts = view.parts();
    if parts.len() > 0 {
        made.parts.reserve_exact(parts.len());
        made.parts.extend(parts);
    }
    let path_count = view.paths().size_hint().0;
    if path_count > 0 {
        made.paths.reserve_exact(path_count);
        let mut path_bytes = 0usize;
        for path in view.paths() {
            path_bytes += path.path.len();
        }
        made.path_bytes.reserve_exact(path_bytes);
        for path in view.paths() {
            let at = u32::try_from(made.path_bytes.len()).unwrap_or(u32::MAX);
            made.path_bytes.extend_from_slice(path.path);
            let end = u32::try_from(made.path_bytes.len()).unwrap_or(u32::MAX);
            made.paths.push((path.name, at, end, path.module));
        }
    }
    let finding_count = view.finding_count();
    if finding_count > 0 {
        made.findings.reserve_exact(finding_count);
        let mut msg_bytes = 0usize;
        for finding in view.findings() {
            msg_bytes += finding.message.len();
        }
        made.messages.reserve_exact(msg_bytes);
        for finding in view.findings() {
            let at = made.messages.len();
            made.messages.extend_from_slice(finding.message);
            made.findings.push((finding.rule, finding.start, finding.end, at, made.messages.len()));
        }
    }
    let reads = view.reads();
    if reads.len() > 0 {
        made.reads.reserve_exact(reads.len());
        made.reads.extend(reads);
    }
    if !view.payload.is_empty() {
        made.payload.reserve_exact(view.payload.len());
        made.payload.extend_from_slice(view.payload);
    }
    Ok(made)
}

#[cfg(test)]
const PUTS_DIGEST: &str = "blaze-cli engine records put 1";

/// One stage's records as the profile prints them: "records (Names): N put, B bytes; held after
/// the merge: H bytes; copies made: C (D bytes)".
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct RecordLine {
    pub(crate) puts: u64,
    /// The records' whole bytes (head and payload).
    pub(crate) bytes: u64,
    /// After the stage's last merge and put: the bytes of its records' payloads this process
    /// still holds in its heap or frames (results not yet released), and every byte the store
    /// holds copied in its heap (its arena), whichever stage put it.
    pub(crate) held: u64,
    /// Payload bytes of this stage's own results still held in its heap or frames after the merge
    /// (excluding the store's shared arena under `Visibility::InArena`).
    pub(crate) results_held: u64,
    /// Copies of a record's bytes made into this process's memory on the way to the store or
    /// back from it (a joined record, the store's arena), and their bytes.
    pub(crate) copies: u64,
    pub(crate) copy_bytes: u64,
}

impl core::fmt::Display for RecordLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} put, {} bytes; held after the merge: {} bytes; copies made: {} ({} bytes)", self.puts, self.bytes, self.held, self.copies, self.copy_bytes)
    }
}

/// The records one stage read of earlier stages' as the profile prints them: "records read
/// (Bodies): N, B bytes; copied into the heap: C bytes" (runtime.md C3: C is 0 on the engine's
/// path, engine_records.rs).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ReadLine {
    pub(crate) records: u64,
    pub(crate) bytes: u64,
    /// The bytes of the sections handed to the stage's readers (engine_records.rs
    /// `ReadCounts::sections`): what the copying route copies, twice for an exports section.
    pub(crate) sections: u64,
    pub(crate) copied: u64,
    /// Asks answered by a record the request opened before (no read).
    pub(crate) again: u64,
}

impl core::fmt::Display for ReadLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}, {} bytes; copied into the heap: {} bytes; asked again: {}", self.records, self.bytes, self.copied, self.again)
    }
}

/// What the engine's records cost this process, stage by stage (rules 50, 51, 53): counted at
/// the seams, never by a scan: a put taken ([`Ledger::put`], its payload still in its result), a
/// copy made (the store's, read from [`Store::kept`] at the merge: the put path itself joins
/// nothing), a result's payload freed ([`Ledger::released`]). Also the digest of every record put
/// (key and bytes, in put order), so a change to how a record is written is seen to keep its
/// bytes (rule 25).
pub(crate) struct Ledger {
    pub(crate) lines: [RecordLine; STAGES],
    /// What each stage read of earlier stages' records, counted where each was opened
    /// (engine_records.rs `EngineRecords::records`).
    pub(crate) reads: [ReadLine; STAGES],
    /// Each stage's records put beside its tasks' own (a `CrateScope`'s expansion records),
    /// apart from `lines`, which count the tasks' records.
    pub(crate) extras: [ExtraLine; STAGES],
    /// Payload bytes each stage's results hold after their put.
    results: [u64; STAGES],
    /// The store's counts at the last merge.
    seen: crate::store::StoreKept,
    /// The reader's counts at the last merge.
    seen_read: crate::engine_records::ReadCounts,
    /// The copies the extra records written since the last merge made in the store's arena
    /// (each one, its bytes in 16-byte blocks: blaze-cas plain/lent.rs `copy_in`): the stage's
    /// line counts its tasks' records' copies, so these are taken from it at the merge.
    extra_copies: [(u64, u64); STAGES],
    #[cfg(test)]
    digest: blaze_ids::Hasher,
    /// The process's memory at each stage's end (engine_memory.rs): the profile's alone, never
    /// in the digest (rule 25).
    pub(crate) memory: crate::engine_memory::Memory,
}

/// One stage's extra records as the profile prints them, after its records' line:
/// "expansions: N put, B bytes, S same".
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ExtraLine {
    /// Records written (new to the store).
    pub(crate) puts: u64,
    pub(crate) bytes: u64,
    /// Rows of the tasks' records that named a record already held: the calls a record served
    /// beside the one that wrote it, or every call of a record an earlier request wrote.
    pub(crate) same: u64,
}

impl core::fmt::Display for ExtraLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "expansions: {} put, {} bytes, {} same", self.puts, self.bytes, self.same)
    }
}

impl Ledger {
    pub(crate) fn new() -> Ledger {
        Ledger {
            lines: [RecordLine::default(); STAGES],
            reads: [ReadLine::default(); STAGES],
            extras: [ExtraLine::default(); STAGES],
            results: [0; STAGES],
            seen: crate::store::StoreKept::default(),
            seen_read: crate::engine_records::ReadCounts::default(),
            extra_copies: [(0, 0); STAGES],
            #[cfg(test)]
            digest: blaze_ids::Hasher::new(PUTS_DIGEST),
            memory: crate::engine_memory::Memory::new(),
        }
    }

    /// A phase of `stage` merged: the reader's reads since the last merge are this stage's.
    pub(crate) fn read(&mut self, stage: Stage, now: crate::engine_records::ReadCounts) {
        let line = &mut self.reads[stage.index()];
        line.records += now.records.saturating_sub(self.seen_read.records);
        line.bytes += now.bytes.saturating_sub(self.seen_read.bytes);
        line.sections += now.sections.saturating_sub(self.seen_read.sections);
        line.copied += now.copied.saturating_sub(self.seen_read.copied);
        line.again += now.hits.saturating_sub(self.seen_read.hits);
        self.seen_read = now;
    }

    /// An extra record of `stage`'s task put under `key` after its owner ([`put_all`]), the
    /// store's answer `put`: in the digest in put order, written or not; counted when written.
    pub(crate) fn put_extra(&mut self, stage: Stage, key: Key, bytes: &[u8], put: blaze_keyed::store::Put) {
        #[cfg(test)]
        self.digest.update(&key.0.0).update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
        #[cfg(not(test))]
        let _ = key;
        if put == blaze_keyed::store::Put::Written {
            let line = &mut self.extras[stage.index()];
            line.puts += 1;
            line.bytes += bytes.len() as u64;
            let copies = &mut self.extra_copies[stage.index()];
            copies.0 += 1;
            copies.1 += (bytes.len().div_ceil(16).max(1) * 16) as u64;
        }
    }

    /// A task of `stage` whose record's rows name its extras `calls` times, `written` of them
    /// new (every other row is served by a record already held), its extras `held` bytes still
    /// in its result until [`Ledger::released`] (work.rs `Record::release_handed` frees them
    /// with the payload).
    pub(crate) fn extras_named(&mut self, stage: Stage, calls: u32, written: u32, held: usize) {
        self.extras[stage.index()].same += u64::from(calls.saturating_sub(written));
        self.results[stage.index()] += held as u64;
    }

    /// A record of `stage` put under `key`, its bytes the concatenation of `parts`, its payload
    /// (`payload` bytes) still in its result's buffer until [`Ledger::released`].
    pub(crate) fn put(&mut self, stage: Stage, key: Key, parts: &[&[u8]], payload: usize) {
        let len: usize = parts.iter().map(|part| part.len()).sum();
        let line = &mut self.lines[stage.index()];
        line.puts += 1;
        line.bytes += len as u64;
        // The meter (D169): one put and its bytes, on the thread that put it.
        blaze_meter::add(blaze_meter::Count::StorePuts, 1);
        blaze_meter::add(blaze_meter::Count::StorePutBytes, len as u64);
        self.results[stage.index()] += payload as u64;
        #[cfg(test)]
        {
            self.digest.update(&key.0.0).update(&(len as u64).to_le_bytes());
            for part in parts {
                self.digest.update(part);
            }
        }
        #[cfg(not(test))]
        let _ = key;
    }

    /// `payload` bytes of a result of `stage` freed after its put.
    pub(crate) fn released(&mut self, stage: Stage, payload: usize) {
        let held = &mut self.results[stage.index()];
        *held = held.saturating_sub(payload as u64);
    }

    /// A phase of `stage` merged and put: the store's copies since the last merge are this
    /// stage's, and its line's `held` is the gauge now.
    pub(crate) fn merged(&mut self, stage: Stage, store: crate::store::StoreKept) {
        let line = &mut self.lines[stage.index()];
        // The extra records' own copies are theirs, not the tasks' records'.
        let (extra, extra_bytes) = core::mem::take(&mut self.extra_copies[stage.index()]);
        line.copies += store.copies.saturating_sub(self.seen.copies).saturating_sub(extra);
        line.copy_bytes += store.copy_bytes.saturating_sub(self.seen.copy_bytes).saturating_sub(extra_bytes);
        let results_held = self.results[stage.index()];
        line.results_held = results_held;
        line.held = results_held + store.held_bytes;
        self.seen = store;
        self.memory.merged(stage);
    }

    /// The digest of every record put so far (key, length, bytes, in put order).
    #[cfg(test)]
    pub(crate) fn digest(&self) -> ContentId {
        self.digest.finish()
    }

    /// One line a stage that put a record, for the profile.
    pub(crate) fn write(&self, text: &mut impl core::fmt::Write) {
        // A report: a full frame cuts it short where it stood (engine_check.rs `profile_text`).
        for stage in Stage::ALL {
            let (line, extras) = (self.lines[stage.index()], self.extras[stage.index()]);
            if line.puts > 0 && extras == ExtraLine::default() {
                let _ = writeln!(text, "records ({stage:?}): {line}");
            } else if line.puts > 0 {
                let _ = writeln!(text, "records ({stage:?}): {line}; {extras}");
            }
            let read = self.reads[stage.index()];
            if read.records + read.again > 0 {
                let _ = writeln!(text, "records read ({stage:?}): {read}");
            }
        }
    }
}
