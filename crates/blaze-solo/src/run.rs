//! One command as a state machine (rule 14): it asks for every look at the disk as an
//! [`Effect`] and takes each [`Completion`] its driver hands back; the store it is handed a
//! call at a time ([`Put`]), the clock too. No IO of its own.
//!
//! In order:
//!
//! 1. **Load**: the leaf table and the slot file read whole; the table decoded (an empty file
//!    is no table; one that does not decode is an error naming it), the newest slot taken, and
//!    what to open decided ([`root::decide`]): a slot whose root the store lacks is
//!    [`Opened::Lost`], never an empty session said to be open.
//! 2. **Sweep** ([`crate::sweep`]): every row stamped in batches of [`STAMP_BATCH`], the
//!    directories whose stamp moved listed, the tags probed.
//! 3. **Hash**: every file a step will read this command, and every hashed row whose stamp
//!    moved or is racy, read through [`CHUNK`]-byte reads and hashed as read, then stamped
//!    again: a stamp after unequal to the sweep's fails naming the file. [`READS_AT_ONCE`] files
//!    at a time. A file no step reads is never hashed.
//! 4. **Carry**: an opened session carried from its pin to the world's by the exact change,
//!    `FILE` for each path added, removed or changed and `EXISTS` for each added or removed,
//!    every kind said exact, as the daemon's carry says it
//!    (crates/blaze-cargo/src/serve/front_carry.rs:508-526).
//! 5. **Ask**: `items` of each `.rs` file added or changed (every `.rs` file when the session
//!    is new, or [`Ask::All`]).
//! 6. **Save**: the session's records put ([`Session::save`]), then the store flushed
//!    ([`Poll::Flush`], the driver's to carry out), and only then, under an exclusive lock of
//!    the slot file and only if its newest sequence is still the one this command opened, the
//!    table written (to its `.part`, ordered by a barrier, renamed) and the slot of the next
//!    sequence written. A command that finds another's save in between drops its own and says
//!    `raced`. No slot ever names a record that is not durable in the store.

#![blaze::sans_io]

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::Arc,
    time::Duration,
};

use blaze_cas::{Key, Kind as StoreKind, Mismatch};
use blaze_core::{Completion, Done, Effect, FileName, LockMode, Names, Stamp, Tag};
use blaze_front_items::{
    Reading,
    steps::{FileKey, Items, Lines, Modules, Parse, reads},
};
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{Changed, CodeId, Get, Kernel, KernelError, Pin, Put, Read, Session, World as _};
use blaze_store_check::Setting;
use serde::Serialize;

use crate::{
    Plant, planted,
    root::{self, Opened, Slot},
    sweep::{self, SIGNATURE, Sweep, SweepError, Walk},
    table::{self, Kind, Row, Table},
    world::{SoloWorld, pin_of},
};

/// The bytes one read of a file asks for.
pub const CHUNK: usize = 64 << 10;
/// The names one stamp effect carries.
pub const STAMP_BATCH: usize = 4096;
/// Files read at once.
pub const READS_AT_ONCE: usize = 64;
/// The tags a command asks with: `tags..tags + TAG_SPAN`.
pub const TAG_SPAN: u64 = 1 << 32;
/// The domain of an answer's hash in an [`Outcome`]: the hash of `items(file)`'s value.
pub(crate) const ANSWER_DOMAIN: &str = "blaze solo answer 1";

const RULE_TABLE: &str = "a leaf table decodes whole (blaze-solo's table layout)";
const RULE_STAMP: &str = "stamp, read, stamp: a file is hashed only when its stamp after the read is the sweep's";
const RULE_IO: &str = "every look at the checkout and the state succeeds";
const RULE_KERNEL: &str = "the kernel answers every step this command asks";
const RULE_STORE: &str = "the run's puts are durable before the session root names them";
const RULE_DRIVER: &str = "a driver answers each effect once, with its own completion";

/// The code identity of each front-end step, which the caller gives: the daemon's carry gives
/// its steps `front_carry::code` (crates/blaze-cargo/src/serve/front_carry.rs:66-77, private
/// there), and a command that registers other codes keeps entries the daemon never hits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Codes {
    pub parse: CodeId,
    pub items: CodeId,
    pub lines: CodeId,
    pub modules: CodeId,
}

/// Which files a command asks `items` of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask {
    /// Each `.rs` file added or changed since the table (every one when the session is new).
    Changed,
    /// Every `.rs` file of the view.
    All,
}

pub struct Config {
    /// The checkout's directory, under the driver's root.
    pub checkout: FileName,
    /// This checkout's state directory (the table, the slot files), under the driver's root.
    pub state: FileName,
    /// How every file is read: the daemon reads a crate's files with its edition and its
    /// root's extensions; with no crate graph a command takes one reading for all.
    pub reading: Reading,
    pub ask: Ask,
    pub codes: Codes,
    /// The command's tags are `tags..tags + TAG_SPAN`.
    pub tags: u64,
}

/// Nanoseconds of each phase, by the injected clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize)]
pub struct Phases {
    pub load: u64,
    pub sweep: u64,
    pub hash: u64,
    pub open: u64,
    pub carry: u64,
    pub ask: u64,
    pub save: u64,
}

/// What a command did, in a fixed order of fields (rule 25).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize)]
pub struct Report {
    /// `new`, `open`, `lost` or `behind` ([`Opened::name`]).
    pub opened: &'static str,
    /// `unchanged` (nothing to write), `wrote` (the slot, and the table where it moved),
    /// `table` (the table alone: its pin did not move), or `raced` (another command saved
    /// since this one opened: its delta dropped).
    pub saved: &'static str,
    /// Rows of the table now that are files.
    pub files: u64,
    pub stated: u64,
    pub listed: u64,
    pub probed: u64,
    pub hashed: u64,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    pub asked: u64,
    pub session_entries: u64,
    pub repaired: u64,
    pub cut: u64,
    pub session_hits: u64,
    pub store_lookups: u64,
    pub store_bytes_read: u64,
    /// The slot's sequence after this command: the one it wrote, else the one it opened.
    pub sequence: u64,
    pub nanos: Phases,
}

/// A command's result, for its caller and a test's oracle.
#[derive(Debug)]
pub struct Outcome {
    pub report: Report,
    /// Each file asked, with the hash of its `items` value, by path.
    pub answers: Vec<(String, ContentId)>,
    /// The paths hashed (read whole), by path.
    pub hashed: Vec<String>,
    /// The newest slot's sequence as the command opened it, 0 for none.
    pub opened_sequence: u64,
    /// The sequence of the slot it wrote, if it wrote one.
    pub wrote_sequence: Option<u64>,
    pub opened: Opened,
}

/// Why a command stopped: the thing (a file, a path, a step), the rule that refused it, and
/// what was found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SoloError {
    pub thing: String,
    pub rule: &'static str,
    pub why: String,
}

impl SoloError {
    fn new(thing: impl fmt::Display, rule: &'static str, why: impl fmt::Display) -> SoloError {
        // alloc: an error
        SoloError { thing: thing.to_string(), rule, why: why.to_string() }
    }
}

impl fmt::Display for SoloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (refused by: {})", self.thing, self.why, self.rule)
    }
}

/// What a completion left the command waiting for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Poll {
    /// More effects (in the vector it was handed) or completions.
    Pending,
    /// The store must be made durable now: the driver flushes it and hands the result to
    /// [`Command::flushed`].
    Flush,
    /// Finished: [`Command::finish`] gives the result.
    Done,
}

/// The store as the kernel sees it, with the bytes it reads counted.
struct Counted<'p, P: ?Sized> {
    store: &'p mut P,
    read: Cell<u64>,
}

impl<'p, P: Put + ?Sized> Counted<'p, P> {
    fn new(store: &'p mut P) -> Counted<'p, P> {
        Counted { store, read: Cell::new(0) }
    }
}

impl<P: Put + ?Sized> Get for Counted<'_, P> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        let got = self.store.get(key)?;
        if let Some(bytes) = got {
            self.read.set(self.read.get() + bytes.len() as u64);
        }
        Ok(got)
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        self.store.has(key)
    }
}

impl<P: Put + ?Sized> Put for Counted<'_, P> {
    fn put(&mut self, key: Key, kind: StoreKind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        self.store.put(key, kind, bytes)
    }
}

/// What an effect in flight is for.
enum Pending {
    Table,
    Slot,
    Stamps { from: usize, len: usize },
    List { dir: String },
    Probe { path: String },
    Chunk { job: usize },
    After { job: usize },
    Lock,
    Reslot,
    WritePart,
    Barrier,
    Rename,
    WriteSlot,
    Unlock,
}

/// The steps of a save under the slot file's lock, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Write {
    /// The slot file read again under the lock: the sequence checked.
    Checked,
    Part,
    Barrier,
    Rename,
    Slot,
    Unlock,
}

/// One file being read and hashed.
struct Job {
    path: String,
    file: FileName,
    /// The sweep's stamp: the one before the read.
    before: Stamp,
    hasher: Hasher,
    bytes: Vec<u8>,
}

struct Files {
    table: FileName,
    part: FileName,
    slot: FileName,
}

pub struct Command {
    config: Config,
    kernel: Kernel<SoloWorld>,
    files: Files,
    next: u64,
    pending: BTreeMap<u64, Pending>,
    table_bytes: Option<Vec<u8>>,
    slot_bytes: Option<Vec<u8>>,
    loaded: bool,
    had_table: bool,
    newest: Option<Slot>,
    opened_sequence: u64,
    opened: Opened,
    session: Option<Session>,
    /// The wall clock before this command's first stat: the new table's instant.
    instant_ns: u64,
    sweep: Option<Sweep>,
    stamps: Vec<Stamp>,
    stamps_left: usize,
    walking: usize,
    old: Vec<Row>,
    rows: Vec<Row>,
    racy: u64,
    jobs: Vec<Job>,
    queue: VecDeque<usize>,
    running: usize,
    texts: Vec<(String, Vec<u8>)>,
    hashed: BTreeSet<String>,
    carried: bool,
    world: Option<SoloWorld>,
    answers: Vec<(String, ContentId)>,
    table_out: Option<Vec<u8>>,
    slot_out: Option<Slot>,
    lock: Option<Tag>,
    slot_written: bool,
    failing: Option<SoloError>,
    result: Option<Result<Outcome, SoloError>>,
    report: Report,
    last: Duration,
}

impl Command {
    /// A command over `config`, its instant `instant_ns` (the wall clock before any look) and
    /// its clock at `now`: its first effects pushed to `effects`.
    pub fn start(
        config: Config,
        instant_ns: u64,
        now: Duration,
        effects: &mut Vec<Effect>,
    ) -> Result<Command, SoloError> {
        let kernel = kernel(&config.codes)
            .map_err(|e| SoloError::new("the front end's steps", "each step registers once", e))?;
        let registry = format!("{:x}", kernel.registry()); // alloc: once a command
        let name = |leaf: &str| {
            // alloc: once a command, a file of the state
            FileName::new(&format!("{}/{leaf}", config.state)).expect("a name under the state directory")
        };
        let files = Files {
            table: name("table"),
            part: name("table.part"),
            slot: name(&format!("root-{}", &registry[..16.min(registry.len())])),
        };
        let _ = blaze_kernel::take_lookups();
        let mut command = Command {
            next: config.tags,
            config,
            kernel,
            files,
            pending: BTreeMap::new(),
            table_bytes: None,
            slot_bytes: None,
            loaded: false,
            had_table: false,
            newest: None,
            opened_sequence: 0,
            opened: Opened::New,
            session: None,
            instant_ns,
            sweep: None,
            stamps: Vec::new(),
            stamps_left: 0,
            walking: 0,
            old: Vec::new(),
            rows: Vec::new(),
            racy: 0,
            jobs: Vec::new(),
            queue: VecDeque::new(),
            running: 0,
            texts: Vec::new(),
            hashed: BTreeSet::new(),
            carried: false,
            world: None,
            answers: Vec::new(),
            table_out: None,
            slot_out: None,
            lock: None,
            slot_written: false,
            failing: None,
            result: None,
            report: Report::default(),
            last: now,
        };
        let tag = command.ask(Pending::Table);
        effects.push(Effect::ReadAll { file: command.files.table.clone(), tag });
        let tag = command.ask(Pending::Slot);
        effects.push(Effect::ReadAll { file: command.files.slot.clone(), tag });
        Ok(command)
    }

    /// Whether `tag` is one of this command's.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0 >= self.config.tags && tag.0 - self.config.tags < TAG_SPAN
    }

    /// The file of the slots, and the table: for a test that tears them.
    pub fn slot_file(&self) -> &FileName {
        &self.files.slot
    }

    pub fn table_file(&self) -> &FileName {
        &self.files.table
    }

    /// The result, once [`Poll::Done`].
    pub fn finish(&mut self) -> Result<Outcome, SoloError> {
        self.result.take().expect("finish is asked once, after Poll::Done")
    }

    fn ask(&mut self, pending: Pending) -> Tag {
        let tag = self.next;
        assert!(tag - self.config.tags < TAG_SPAN, "a command asks fewer than 2^32 effects");
        self.next += 1;
        self.pending.insert(tag, pending);
        Tag(tag)
    }

    fn lap(&mut self, now: Duration) -> u64 {
        let took = now.saturating_sub(self.last);
        self.last = now;
        u64::try_from(took.as_nanos()).unwrap_or(u64::MAX)
    }

    /// The name of `path` (relative to the checkout) under the driver's root.
    fn full(&self, path: &str) -> FileName {
        if path.is_empty() {
            // clone: a refcount
            return self.config.checkout.clone();
        }
        // alloc: a name a look asks with
        FileName::new(&format!("{}/{path}", self.config.checkout)).expect("a row's path is a relative name")
    }

    /// One completion of an effect this command asked for.
    pub fn complete<P: Put + ?Sized>(
        &mut self,
        completion: Completion,
        store: &mut P,
        clock: impl Fn() -> Duration,
        effects: &mut Vec<Effect>,
    ) -> Poll {
        let Some(pending) = self.pending.remove(&completion.tag.0) else {
            let why = format!("tag {} names no effect in flight", completion.tag.0); // alloc: an error
            return self.fail(SoloError::new("a completion", RULE_DRIVER, why), effects);
        };
        match self.on(pending, completion.done, store, &clock, effects) {
            Ok(poll) => poll,
            Err(e) => self.fail(e, effects),
        }
    }

    /// The store's flush, asked by [`Poll::Flush`], done.
    pub fn flushed(&mut self, flushed: Result<(), String>, clock: impl Fn() -> Duration, effects: &mut Vec<Effect>) -> Poll {
        if let Err(why) = flushed {
            return self.fail(SoloError::new("the store", RULE_STORE, why), effects);
        }
        if self.slot_written {
            // The planted order: the slot first, the flush after.
            return self.done(&clock);
        }
        self.take_lock(effects);
        Poll::Pending
    }

    fn fail(&mut self, error: SoloError, effects: &mut Vec<Effect>) -> Poll {
        // The first error is the one the command stops with; an unlock's on the way out
        // follows it.
        let error = self.failing.take().unwrap_or(error);
        if let Some(lock) = self.lock.take() {
            self.failing = Some(error);
            let tag = self.ask(Pending::Unlock);
            effects.push(Effect::Unlock { lock, tag });
            return Poll::Pending;
        }
        self.result = Some(Err(error));
        Poll::Done
    }

    fn on<P: Put + ?Sized>(
        &mut self,
        pending: Pending,
        done: Done,
        store: &mut P,
        clock: &impl Fn() -> Duration,
        effects: &mut Vec<Effect>,
    ) -> Result<Poll, SoloError> {
        match (pending, done) {
            (Pending::Table, Done::Read(bytes)) => self.table_bytes = Some(bytes),
            (Pending::Slot, Done::Read(bytes)) => self.slot_bytes = Some(bytes),
            (Pending::Stamps { from, len }, Done::Stamped(stamps)) => {
                if stamps.len() != len {
                    let why = format!("{} stamps for {len} names", stamps.len()); // alloc: an error
                    return Err(SoloError::new(&self.config.checkout, RULE_DRIVER, why));
                }
                self.stamps[from..from + len].copy_from_slice(&stamps);
                self.stamps_left -= 1;
                if self.stamps_left == 0 {
                    let stamps = std::mem::take(&mut self.stamps);
                    let walk = self.sweep.as_mut().expect("sweeping").stamped(&stamps).map_err(|e| self.swept_wrong(&e))?;
                    self.walk(walk, effects);
                }
            }
            (Pending::List { dir }, Done::Entries(entries)) => {
                let entries: Vec<(String, Stamp)> = entries
                    .iter()
                    // alloc: one name a listed entry
                    .map(|(name, stamp)| (name.as_str().rsplit('/').next().unwrap_or_default().to_owned(), *stamp))
                    .collect();
                let walk = self.sweep.as_mut().expect("sweeping").listed(&dir, &entries);
                self.walking -= 1;
                self.walk(walk, effects);
            }
            (Pending::Probe { path }, Done::Read(bytes)) => {
                let walk = self.sweep.as_mut().expect("sweeping").probed(&path, &bytes);
                self.walking -= 1;
                self.walk(walk, effects);
            }
            (Pending::Chunk { job }, Done::Read(bytes)) => {
                let it = &mut self.jobs[job];
                it.hasher.update(&bytes);
                it.bytes.extend_from_slice(&bytes);
                if bytes.len() < CHUNK {
                    // alloc: one name, the stamp after the read
                    let mut names = Names::with_capacity(1, it.file.as_str().len() + 1);
                    names.push(&it.file);
                    let tag = self.ask(Pending::After { job });
                    effects.push(Effect::Stamp { files: Arc::new(names), tag });
                } else {
                    let (file, at) = (it.file.clone(), it.bytes.len() as u64);
                    let tag = self.ask(Pending::Chunk { job });
                    effects.push(Effect::Read { file, at, len: CHUNK, tag });
                }
            }
            (Pending::After { job }, Done::Stamped(stamps)) => {
                let it = &mut self.jobs[job];
                let after = stamps.first().copied().unwrap_or(Stamp::UNKNOWN);
                if after != it.before {
                    // alloc: an error
                    let why = format!("stamped {:?} by the sweep and {after:?} after its read: it changed while read", it.before);
                    return Err(SoloError::new(&it.file, RULE_STAMP, why));
                }
                let id = it.hasher.finish();
                let path = std::mem::take(&mut it.path);
                let bytes = std::mem::take(&mut it.bytes);
                let at = self.rows.binary_search_by(|row| row.path.as_str().cmp(&path)).expect("a job is of a row");
                self.rows[at].id = Some(id);
                // clone: one path a file hashed
                self.hashed.insert(path.clone());
                self.texts.push((path, bytes));
                self.running -= 1;
                self.start_jobs(effects);
                if self.running == 0 {
                    return self.work(store, clock, effects);
                }
            }
            (Pending::Lock, Done::Locked) => {
                let tag = self.ask(Pending::Reslot);
                effects.push(Effect::ReadAll { file: self.files.slot.clone(), tag });
            }
            (Pending::Lock, Done::Contended) => {
                // Another command is saving: this one lost the race.
                self.lock = None;
                self.report.saved = "raced";
                return Ok(self.done(clock));
            }
            (Pending::Reslot, Done::Read(bytes)) => {
                let now = root::newest(&bytes).map_or(0, |slot| slot.sequence);
                if now != self.opened_sequence && !planted(Plant::SkipsSequence) {
                    self.report.saved = "raced";
                    self.table_out = None;
                    self.slot_out = None;
                }
                self.write_next(Write::Checked, effects);
            }
            (Pending::WritePart, Done::Written) => self.write_next(Write::Part, effects),
            (Pending::Barrier, Done::Ordered) => self.write_next(Write::Barrier, effects),
            (Pending::Rename, Done::Renamed) => self.write_next(Write::Rename, effects),
            (Pending::WriteSlot, Done::Written) => {
                self.slot_written = true;
                self.write_next(Write::Slot, effects);
            }
            (Pending::Unlock, Done::Unlocked) => {
                if let Some(error) = self.failing.take() {
                    self.result = Some(Err(error));
                    return Ok(Poll::Done);
                }
                if self.slot_written && planted(Plant::SlotBeforeFlush) {
                    return Ok(Poll::Flush);
                }
                return Ok(self.done(clock));
            }
            (pending, Done::Failed(error)) => {
                if matches!(pending, Pending::Lock | Pending::Unlock) {
                    // Not held: nothing to unlock on the way out.
                    self.lock = None;
                }
                return Err(SoloError::new(self.what(&pending), RULE_IO, error.0));
            }
            (pending, done) => {
                // alloc: an error
                let why = format!("{done:?}, which does not answer the effect asked");
                return Err(SoloError::new(self.what(&pending), RULE_DRIVER, why));
            }
        }
        if !self.loaded && self.table_bytes.is_some() && self.slot_bytes.is_some() {
            self.loaded = true;
            self.load_state(store, clock, effects)?;
        }
        if self.sweep.is_some() && self.stamps_left == 0 && self.walking == 0 {
            self.swept(store, clock, effects)
        } else {
            Ok(Poll::Pending)
        }
    }

    /// What an effect in flight was about, for its error.
    fn what(&self, pending: &Pending) -> String {
        match pending {
            Pending::Table | Pending::WritePart | Pending::Barrier | Pending::Rename => self.files.table.to_string(),
            Pending::Slot | Pending::Lock | Pending::Reslot | Pending::WriteSlot | Pending::Unlock => {
                self.files.slot.to_string()
            }
            Pending::Stamps { .. } => self.config.checkout.to_string(),
            Pending::List { dir } => self.full(dir).to_string(),
            Pending::Probe { path } => self.full(path).to_string(),
            Pending::Chunk { job } | Pending::After { job } => self.jobs[*job].file.to_string(),
        }
    }

    fn swept_wrong(&self, error: &SweepError) -> SoloError {
        match error {
            SweepError::NotADirectory { stamp } => SoloError::new(
                &self.config.checkout,
                "a checkout is a directory",
                format_args!("stamped {stamp:?}"),
            ),
            SweepError::Stamps { asked, got } => {
                SoloError::new(&self.config.checkout, RULE_DRIVER, format_args!("{got} stamps for {asked} names"))
            }
        }
    }

    /// The table and the slot file read: decided what to open, and the sweep started.
    fn load_state<P: Put + ?Sized>(
        &mut self,
        store: &mut P,
        clock: &impl Fn() -> Duration,
        effects: &mut Vec<Effect>,
    ) -> Result<(), SoloError> {
        let bytes = self.table_bytes.take().expect("read");
        let table = match bytes.is_empty() {
            true => Table::default(),
            false => Table::decode(&bytes).map_err(|why| SoloError::new(&self.files.table, RULE_TABLE, why))?,
        };
        self.had_table = !bytes.is_empty();
        let slots = self.slot_bytes.take().expect("read");
        self.newest = root::newest(&slots);
        self.opened_sequence = self.newest.map_or(0, |slot| slot.sequence);
        self.report.nanos.load = self.lap(clock());
        let counted = Counted::new(store);
        self.opened = root::decide(self.newest.as_ref(), pin_of(&table.rows), |key| counted.has(key))
            .map_err(|why| SoloError::new(&self.files.slot, RULE_STORE, why))?;
        if self.opened == Opened::Open {
            let slot = self.newest.expect("an open slot");
            let session = Session::open(&counted, slot.root)
                .map_err(|e| SoloError::new(&self.files.slot, "a saved session opens from its root", e))?;
            if session.pin() != slot.pin {
                return Err(SoloError::new(
                    &self.files.slot,
                    "a slot's pin is its session's",
                    format_args!("the slot says {:x}, its session {:x}", slot.pin.0, session.pin().0),
                ));
            }
            self.session = Some(session);
        }
        self.report.store_bytes_read += counted.read.get();
        self.report.nanos.open = self.lap(clock());
        let sweep = Sweep::new(table.rows, table.instant_ns);
        let names: Vec<FileName> = sweep.names().map(|path| self.full(path)).collect();
        // alloc: the stamps of one sweep, once a command
        self.stamps = vec![Stamp::GONE; names.len()];
        for (n, batch) in names.chunks(STAMP_BATCH).enumerate() {
            let bytes = batch.iter().map(|name| name.as_str().len() + 1).sum();
            let mut list = Names::with_capacity(batch.len(), bytes);
            for name in batch {
                list.push(name);
            }
            let tag = self.ask(Pending::Stamps { from: n * STAMP_BATCH, len: batch.len() });
            effects.push(Effect::Stamp { files: Arc::new(list), tag });
            self.stamps_left += 1;
        }
        self.sweep = Some(sweep);
        Ok(())
    }

    fn walk(&mut self, walk: Walk, effects: &mut Vec<Effect>) {
        for dir in walk.lists {
            let file = self.full(&dir);
            let tag = self.ask(Pending::List { dir });
            effects.push(Effect::Entries { dir: file, tag });
            self.walking += 1;
        }
        for path in walk.probes {
            let file = self.full(&path);
            let tag = self.ask(Pending::Probe { path });
            effects.push(Effect::Read { file, at: 0, len: SIGNATURE.len(), tag });
            self.walking += 1;
        }
    }

    /// Whether every `.rs` file is asked: [`Ask::All`], or a session that is not one carried.
    fn asks_all(&self) -> bool {
        self.config.ask == Ask::All || self.opened != Opened::Open || self.session.is_none()
    }

    /// The sweep done: the files to read hashed.
    fn swept<P: Put + ?Sized>(
        &mut self,
        store: &mut P,
        clock: &impl Fn() -> Duration,
        effects: &mut Vec<Effect>,
    ) -> Result<Poll, SoloError> {
        let swept = self.sweep.take().expect("sweeping").finish();
        self.report.stated = swept.counts.stated;
        self.report.listed = swept.counts.listed;
        self.report.probed = swept.counts.probed;
        self.racy = swept.counts.racy;
        self.old = swept.old;
        self.rows = swept.rows;
        self.report.nanos.sweep = self.lap(clock());
        let mut load: BTreeSet<String> = swept.dirty.into_iter().collect();
        match self.asks_all() {
            true => load.extend(self.rows.iter().filter(|row| asked(row)).map(|row| row.path.clone())),
            false => {
                let added = sweep::changed(&self.old, &self.rows).added;
                load.extend(added.into_iter().filter(|path| self.row(path).is_some_and(asked)));
            }
        }
        self.load(load, effects);
        if self.running == 0 {
            return self.work(store, clock, effects);
        }
        Ok(Poll::Pending)
    }

    fn row(&self, path: &str) -> Option<&Row> {
        self.rows.binary_search_by(|row| row.path.as_str().cmp(path)).ok().map(|at| &self.rows[at])
    }

    /// Reads and hashes of each of `paths` not read yet this command.
    fn load(&mut self, paths: BTreeSet<String>, effects: &mut Vec<Effect>) {
        for path in paths {
            if self.hashed.contains(&path) {
                continue;
            }
            let row = self.row(&path).expect("a load is of a row");
            let before = row.stamp;
            let file = self.full(&path);
            // alloc: the file's bytes, kept for its parse; at most its size, as stamped
            let bytes = Vec::with_capacity(usize::try_from(before.size).unwrap_or(0).min(1 << 30));
            self.jobs.push(Job { path, file, before, hasher: Hasher::new(blaze_vfs::FILE_DOMAIN), bytes });
            self.queue.push_back(self.jobs.len() - 1);
        }
        self.start_jobs(effects);
    }

    fn start_jobs(&mut self, effects: &mut Vec<Effect>) {
        while self.running < READS_AT_ONCE {
            let Some(job) = self.queue.pop_front() else { break };
            let file = self.jobs[job].file.clone();
            let tag = self.ask(Pending::Chunk { job });
            effects.push(Effect::Read { file, at: 0, len: CHUNK, tag });
            self.running += 1;
        }
    }

    /// Every file read: the session carried, the files asked, the session saved into the
    /// store, and the writes decided.
    fn work<P: Put + ?Sized>(
        &mut self,
        store: &mut P,
        clock: &impl Fn() -> Duration,
        effects: &mut Vec<Effect>,
    ) -> Result<Poll, SoloError> {
        self.report.nanos.hash += self.lap(clock());
        let changes = sweep::changed(&self.old, &self.rows);
        let mut counted = Counted::new(store);
        if self.session.is_some() && !self.carried {
            self.carried = true;
            // alloc: the world's texts, moved
            let world = SoloWorld::new(&self.rows, sorted(std::mem::take(&mut self.texts)));
            let session = self.session.as_mut().expect("checked");
            if session.pin() != world.pin() {
                let mut changed = Changed::between(session.pin(), world.pin());
                changed.exact(reads::FILE);
                changed.exact(reads::EXISTS);
                changed.exact(reads::CFG);
                for path in changes.added.iter().chain(&changes.removed) {
                    changed.differs(Read { kind: reads::FILE, key: path.as_bytes() });
                    changed.differs(Read { kind: reads::EXISTS, key: path.as_bytes() });
                }
                for path in &changes.changed {
                    changed.differs(Read { kind: reads::FILE, key: path.as_bytes() });
                }
                match self.kernel.carry(session, &mut counted, &world, &changed, &Setting::OFF) {
                    Ok(carried) => {
                        self.report.repaired = carried.repaired as u64;
                        self.report.cut = carried.cut as u64;
                        if carried.dropped.is_some() {
                            // It holds nothing at the new pin: every file is asked of a new one.
                            self.session = None;
                        }
                    }
                    // A record the saved session names is not in the store: lost, as a
                    // missing root is ([`Opened::Lost`]); a new session.
                    Err(KernelError::Store { key, .. }) => {
                        self.opened = Opened::Lost { missing: key };
                        self.session = None;
                    }
                    Err(e) => return Err(SoloError::new("the session's carry", RULE_KERNEL, e)),
                }
            }
            self.world = Some(world);
            self.report.nanos.carry = self.lap(clock());
        }
        let asks: BTreeSet<String> = match self.asks_all() {
            // clone: one path a file asked
            true => self.rows.iter().filter(|row| asked(row)).map(|row| row.path.clone()).collect(),
            false => changes
                .added
                .iter()
                .chain(&changes.changed)
                .filter(|path| self.row(path).is_some_and(asked))
                .cloned()
                .collect(),
        };
        let missing: BTreeSet<String> = asks.iter().filter(|path| !self.hashed.contains(*path)).cloned().collect();
        if !missing.is_empty() {
            // The session was lost or dropped in the carry: the files it no longer answers
            // are read, and the world made again over them.
            if let Some(world) = self.world.take() {
                self.texts = world.into_texts();
            }
            self.load(missing, effects);
            return Ok(Poll::Pending);
        }
        let world = match self.world.take() {
            Some(world) => world,
            None => SoloWorld::new(&self.rows, sorted(std::mem::take(&mut self.texts))),
        };
        // Held here, not borrowed from `self`, while the asks run; put back once saved. An ask or
        // a save that fails ends the command, and its session with it.
        let mut session = match self.session.take() {
            Some(session) => session,
            None => Session::new(&world),
        };
        self.report.asked = asks.len() as u64;
        for path in asks {
            let input = FileKey { name: path, reading: self.config.reading };
            let got = self
                .kernel
                .get_in::<Items<SoloWorld>, _>(&mut session, &mut counted, &world, &input, &Setting::OFF)
                .map_err(|e| SoloError::new(&input.name, RULE_KERNEL, e))?;
            self.report.session_hits += u64::from(got.is_hit());
            let answer = ContentId::of(ANSWER_DOMAIN, got.value());
            self.answers.push((input.name, answer));
        }
        self.report.nanos.ask = self.lap(clock());
        let root = session.save(&mut counted).map_err(|e| SoloError::new("the session's save", RULE_KERNEL, e))?;
        self.report.session_entries = session.len() as u64;
        self.session = Some(session);
        self.report.store_bytes_read += counted.read.get();
        let pin = world.pin();
        self.world = Some(world);
        self.report.added = changes.added.len() as u64;
        self.report.removed = changes.removed.len() as u64;
        self.report.changed = changes.changed.len() as u64;
        self.save(root, pin, clock, effects)
    }

    /// The session saved under `root` at `pin`: what to write, and the flush before it.
    fn save(&mut self, root: Key, pin: Pin, clock: &impl Fn() -> Duration, effects: &mut Vec<Effect>) -> Result<Poll, SoloError> {
        let table_moved = !self.had_table || self.rows != self.old || self.racy > 0;
        let slot_moved = self.newest.is_none_or(|slot| slot.root != root || slot.pin != pin);
        if table_moved {
            self.table_out = Some(table::encode(self.instant_ns, &self.rows));
        }
        if slot_moved {
            self.slot_out = Some(Slot { sequence: self.opened_sequence + 1, root, pin });
        }
        self.report.saved = match (slot_moved, table_moved) {
            (false, false) => "unchanged",
            (false, true) => "table",
            (true, _) => "wrote",
        };
        if !slot_moved && !table_moved {
            return Ok(self.done(clock));
        }
        if slot_moved && !planted(Plant::SlotBeforeFlush) {
            return Ok(Poll::Flush);
        }
        self.take_lock(effects);
        Ok(Poll::Pending)
    }

    fn take_lock(&mut self, effects: &mut Vec<Effect>) {
        let tag = self.ask(Pending::Lock);
        self.lock = Some(tag);
        effects.push(Effect::Lock { file: self.files.slot.clone(), mode: LockMode::Exclusive, tag });
    }

    /// The write after `after`, under the lock: the table's part, its barrier, its rename,
    /// the slot, the unlock; each only where there is one to write. One at a time, in this
    /// order: the barrier orders the part's bytes before its new name.
    fn write_next(&mut self, after: Write, effects: &mut Vec<Effect>) {
        let next = match after {
            Write::Checked if self.table_out.is_some() => Write::Part,
            Write::Part => Write::Barrier,
            Write::Barrier => Write::Rename,
            Write::Checked | Write::Rename if self.slot_out.is_some() => Write::Slot,
            _ => Write::Unlock,
        };
        let (pending, effect): (Pending, fn(&mut Command, Tag) -> Effect) = match next {
            Write::Part => (Pending::WritePart, |it: &mut Command, tag: Tag| Effect::WriteUnsynced {
                file: it.files.part.clone(),
                bytes: it.table_out.take().expect("a table to write"),
                tag,
            }),
            Write::Barrier => (Pending::Barrier, |it: &mut Command, tag: Tag| Effect::Barrier { file: it.files.part.clone(), tag }),
            Write::Rename => (Pending::Rename, |it: &mut Command, tag: Tag| Effect::RenameUnsynced {
                from: it.files.part.clone(),
                to: it.files.table.clone(),
                tag,
            }),
            Write::Slot => (Pending::WriteSlot, |it: &mut Command, tag: Tag| {
                let slot = it.slot_out.expect("a slot to write");
                Effect::WriteAt { file: it.files.slot.clone(), at: Slot::offset(slot.sequence), bytes: slot.encode().to_vec(), tag }
            }),
            Write::Checked | Write::Unlock => (Pending::Unlock, |it: &mut Command, tag: Tag| Effect::Unlock {
                lock: it.lock.take().expect("the slot file's lock, held"),
                tag,
            }),
        };
        let tag = self.ask(pending);
        let effect = effect(self, tag);
        effects.push(effect);
    }

    /// Finished well: the report and the outcome.
    fn done(&mut self, clock: &impl Fn() -> Duration) -> Poll {
        self.report.nanos.save = self.lap(clock());
        self.report.opened = self.opened.name();
        self.report.files = self.rows.iter().filter(|row| row.kind == Kind::File).count() as u64;
        self.report.hashed = self.hashed.len() as u64;
        self.report.store_lookups = blaze_kernel::take_lookups().lookups;
        let wrote = self.slot_out.filter(|_| self.slot_written).map(|slot| slot.sequence);
        self.report.sequence = wrote.unwrap_or(self.opened_sequence);
        self.result = Some(Ok(Outcome {
            report: std::mem::take(&mut self.report),
            answers: std::mem::take(&mut self.answers),
            hashed: std::mem::take(&mut self.hashed).into_iter().collect(),
            opened_sequence: self.opened_sequence,
            wrote_sequence: wrote,
            opened: self.opened,
        }));
        Poll::Done
    }
}

/// The front end's kernel over the solo world: the four steps the daemon's carry registers,
/// in its order and under the codes given (`front_carry::kernel`,
/// crates/blaze-cargo/src/serve/front_carry.rs:80-88), so the registry and every entry's
/// identity are the daemon's.
pub fn kernel(codes: &Codes) -> Result<Kernel<SoloWorld>, KernelError> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Parse<SoloWorld>>(codes.parse)?;
    kernel.register_ask::<Items<SoloWorld>>(codes.items)?;
    kernel.register_ask::<Lines<SoloWorld>>(codes.lines)?;
    kernel.register_ask::<Modules<SoloWorld>>(codes.modules)?;
    Ok(kernel)
}

/// Whether `items` is asked of the row: a `.rs` file.
fn asked(row: &Row) -> bool {
    row.kind == Kind::File && row.path.ends_with(".rs")
}

fn sorted(mut texts: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    texts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    texts
}
