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
use crate::work::Record;

/// The domain of the engine's per-kind tools: every kind's tool (home.rs `tools`) hashed under
/// it, so an edit of one producer still moves only its kinds' keys (rule 52).
/// 2: an outline record lists its parts, paths and findings (`write_listed`), other bytes
/// than form 1's under the same task key, so every key moves (rule 21).
const ENGINE_FORM: &str = "blaze-cli engine record form 2";

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
    /// A file's outline: its parts, paths and findings too, so a later request that finds the
    /// file unchanged reads the whole outline back ([`outline_from`]) and never the file.
    pub(crate) listing: Option<Listing<'r>>,
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

/// Puts every record of `puts` in key order (rule 25), its head written into `alloc` and handed
/// to the store with its payload, each counted in `ledger` under `stage`; returns how many.
pub(crate) fn put_all(cx: &Cx, store: &mut dyn Store<'_>, puts: &mut [Put<'_>], stage: Stage, ledger: &mut Ledger, alloc: blaze_own::Alloc<'_>) -> Result<u32, CliError> {
    let cx = cx.span("engine: a phase's records put");
    puts.sort_unstable_by_key(|put| put.key);
    for put in puts.iter() {
        // The head (its name, reads, listing and the payload's length) and the payload as two
        // slices: the payload goes to the store from the buffer its task made it in, never joined.
        let head = match &put.listing {
            None => blaze_drive::record::write_plain_head(put.name, put.source, put.out, put.reads, put.payload.len(), alloc),
            Some(listing) => {
                let mut paths = std::vec::Vec::with_capacity_in(listing.paths.len(), alloc);
                paths.extend(listing.paths.iter().map(|&(name, at, to, module)| PathOf { name, path: &listing.path_bytes[at as usize..to as usize], module }));
                let mut findings = std::vec::Vec::with_capacity_in(listing.findings.len(), alloc);
                findings.extend(listing.findings.iter().map(|&(rule, start, end, at, to)| Finding { rule, start, end, message: &listing.messages[at..to] }));
                let listed = blaze_drive::record::Listed { parts: listing.parts, paths: &paths, findings: &findings };
                blaze_drive::record::write_listed_head(put.name, put.source, put.out, put.reads, &listed, put.payload.len(), alloc)
            }
        };
        let parts: [&[u8]; 2] = [&head, put.payload];
        ledger.put(stage, put.key, &parts, put.payload.len());
        store.put_parts(&cx, put.key, &parts)?;
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
    made.parts.extend(view.parts());
    for path in view.paths() {
        let at = made.path_bytes.len() as u32;
        made.path_bytes.extend_from_slice(path.path);
        made.paths.push((path.name, at, made.path_bytes.len() as u32, path.module));
    }
    for finding in view.findings() {
        let at = made.messages.len();
        made.messages.extend_from_slice(finding.message);
        made.findings.push((finding.rule, finding.start, finding.end, at, made.messages.len()));
    }
    made.reads.extend(view.reads());
    made.payload.extend_from_slice(view.payload);
    Ok(made)
}

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

/// What the engine's records cost this process, stage by stage (rules 50, 51, 53): counted at
/// the seams, never by a scan: a put taken ([`Ledger::put`], its payload still in its result), a
/// copy made (the store's, read from [`Store::kept`] at the merge: the put path itself joins
/// nothing), a result's payload freed ([`Ledger::released`]). Also the digest of every record put
/// (key and bytes, in put order), so a change to how a record is written is seen to keep its
/// bytes (rule 25).
pub(crate) struct Ledger {
    pub(crate) lines: [RecordLine; STAGES],
    /// Payload bytes each stage's results hold after their put.
    results: [u64; STAGES],
    /// The store's counts at the last merge.
    seen: crate::store::StoreKept,
    digest: blaze_ids::Hasher,
}

impl Ledger {
    pub(crate) fn new() -> Ledger {
        Ledger { lines: [RecordLine::default(); STAGES], results: [0; STAGES], seen: crate::store::StoreKept::default(), digest: blaze_ids::Hasher::new(PUTS_DIGEST) }
    }

    /// A record of `stage` put under `key`, its bytes the concatenation of `parts`, its payload
    /// (`payload` bytes) still in its result's buffer until [`Ledger::released`].
    pub(crate) fn put(&mut self, stage: Stage, key: Key, parts: &[&[u8]], payload: usize) {
        let len: usize = parts.iter().map(|part| part.len()).sum();
        let line = &mut self.lines[stage.index()];
        line.puts += 1;
        line.bytes += len as u64;
        self.results[stage.index()] += payload as u64;
        self.digest.update(&key.0.0).update(&(len as u64).to_le_bytes());
        for part in parts {
            self.digest.update(part);
        }
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
        line.copies += store.copies.saturating_sub(self.seen.copies);
        line.copy_bytes += store.copy_bytes.saturating_sub(self.seen.copy_bytes);
        line.held = self.results[stage.index()] + store.held_bytes;
        self.seen = store;
    }

    /// The digest of every record put so far (key, length, bytes, in put order).
    pub(crate) fn digest(&self) -> ContentId {
        self.digest.finish()
    }

    /// One line a stage that put a record, for the profile.
    pub(crate) fn write(&self, text: &mut impl core::fmt::Write) {
        // A report: a full frame cuts it short where it stood (engine_check.rs `profile_text`).
        for stage in Stage::ALL {
            let line = self.lines[stage.index()];
            if line.puts > 0 {
                let _ = writeln!(text, "records ({stage:?}): {line}");
            }
        }
    }
}
