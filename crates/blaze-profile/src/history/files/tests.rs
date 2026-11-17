//! The history's files core under blaze-core's simulator (`testing` skill, dst-not-review): one
//! property test draws flushes, build ends, crashes at any completion, crashes during the reopen,
//! crash records, and a swarm of fault classes (failed effects, a full disk, failing syncs,
//! another writer holding the manifest's lock), runs the real core against [`Sim`], and after
//! every crash and reopen checks one invariant: every row ever handed to the history is, exactly
//! once, in a file the manifest lists, in a crash row the open handed back, or still on disk in
//! what a later open reads (a dead daemon's mapping, a crash record) when this open said why it
//! could not read it. Two planted bugs ([`Plant`]) prove the oracle catches what it claims to.
//!
//! What the test plays, beyond the core:
//! - Each process has a pid ([`TestHost`]); a crash is a new process, and only the current one
//!   is alive.
//! - Each process's flight recorder ([`Live::ring`]) holds every row it handed to a flush, each
//!   with the flush it is pending at. At a crash it is written whole as `flight-<pid>.map`
//!   (`flight <nonce>` then `<row> <seq>` lines); [`TestHost::recover`] reads back the rows
//!   past what the manifest says that recorder's flushes committed ([`owed`]).
//! - A crash may leave a crash record `crash.<pid>` (`crash <pid> <t_ns> <row>`), one row; an
//!   open hands it back ([`Opened::crashes`]) and the next flush writes it in a `panics` file.
//! - A table's file that fails to place keeps its rows for the next flush, re-marked in the
//!   recorder as that flush's, as the daemon does (history.rs keeps a failed table's rows).
//!
//! Replay: a failure prints its least tape and the line that replays it; the plan, the
//! simulator's seed in it, is a note of the report.

use std::sync::Arc;

use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, cast::AsArray, types::Int64Type};
use arrow_schema::{DataType, Field, Schema};
use blaze_core::{
    Driver,
    sim::{Faults, Sim},
};
use blaze_sim::{Broke, Config, Tape};
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};

use super::*;
use crate::{panics::Kind as PanicKind, schema};

/// The simulated root and the history's directory under it.
const ROOT: &str = "/sim";
const DIR: &str = "history";

/// The first process's pid; each crash, the next.
const FIRST_PID: u32 = 100;

/// Most acts a plan draws: a case runs in milliseconds.
const MOST_ACTS: u64 = 40;

/// Polls one job may take before it is a core that does not progress (bounded-progress). A
/// flush that waits out another writer's hold of the manifest's lock takes about 250.
const STEP_CAP: u32 = 10_000;

/// Opens in a row that may fail with an injected error before the history is said not to open.
const OPEN_TRIES: u32 = 200;

/// The cases a control searches for a plan its plant breaks.
const CONTROL: Config = Config::cases(128);

// --- The plan ------------------------------------------------------------------------------

/// What a case does, all drawn up front.
#[derive(Debug)]
struct Plan {
    /// The simulator's seed: completion order, latencies, injected failures, torn writes.
    seed: u64,
    /// An effect fails once in this many (0: never).
    fail_one_in: u32,
    latency_us: u64,
    acts: Vec<Act>,
}

#[derive(Debug, Clone, Copy)]
enum Act {
    /// A flush of `rows` new rows (and the crash rows the open handed back), the build's end
    /// when `end` (its tables merged), under `fault`, crashed after `crash.at` completions.
    Flush { rows: u8, end: bool, fault: Fault, crash: Option<Crash> },
    /// A crash with nothing in flight.
    Crash(Crash),
}

#[derive(Debug, Clone, Copy)]
struct Crash {
    /// After this many completions of the job under way (past its end: no crash in it).
    at: u32,
    /// The dying process leaves a crash record.
    record: bool,
    /// The reopen's first try crashes after this many completions.
    reopen_at: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
enum Fault {
    None,
    /// The disk has `room` bytes left for the flush.
    Fill {
        room: u64,
    },
    /// Every sync of the history fails, until the next crash.
    FailSyncs,
    /// Another process holds the manifest's lock until `release_at` completions in; with
    /// `timeout`, the clock jumps past [`manifest::LOCK_WAIT`] then and the hold stays.
    Contend {
        release_at: u32,
        timeout: bool,
    },
}

/// The fault classes a case turns on: a swarm, so a rare class is not drowned by a common one.
struct Swarm {
    fail: bool,
    fill: bool,
    fail_syncs: bool,
    contend: bool,
    records: bool,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let swarm = Swarm {
        fail: tape.coin("failed effects"),
        fill: tape.coin("full disks"),
        fail_syncs: tape.one_in(4, "failing syncs"),
        contend: tape.coin("contended locks"),
        records: tape.coin("crash records"),
    };
    let fail_one_in = match swarm.fail {
        true => tape.int_in(20..=200u32, "fail one in"),
        false => 0,
    };
    let latency_us = tape.int_in(0..=500u64, "latency us");
    let n = tape.int_in(1..=MOST_ACTS, "acts");
    // alloc: a plan, once a case
    let acts = (0..n).map(|_| tape.span("act", |tape| act(tape, &swarm))).collect();
    Plan { seed, fail_one_in, latency_us, acts }
}

fn act(tape: &Tape, swarm: &Swarm) -> Act {
    // Calm is a flush: a crash with nothing in flight is the act that is not quiet.
    match tape.int_in(0..=9u8, "act") {
        9 => Act::Crash(crash_point(tape, swarm)),
        _ => Act::Flush {
            rows: tape.int_in(0..=5u8, "rows"),
            end: tape.one_in(4, "the build ends"),
            fault: fault(tape, swarm),
            crash: tape.one_in(4, "the flush crashes").then(|| crash_point(tape, swarm)),
        },
    }
}

fn crash_point(tape: &Tape, swarm: &Swarm) -> Crash {
    Crash {
        at: tape.int_in(0..=63u32, "crash at"),
        record: swarm.records && tape.coin("leaves a crash record"),
        reopen_at: tape.one_in(4, "the reopen crashes").then(|| tape.int_in(0..=63u32, "reopen crash at")),
    }
}

fn fault(tape: &Tape, swarm: &Swarm) -> Fault {
    // Calm is no fault: the classes are 1..=3.
    match tape.int_in(0..=5u8, "fault") {
        1 if swarm.fill => Fault::Fill { room: tape.int_in(0..=4096u64, "room") },
        2 if swarm.fail_syncs => Fault::FailSyncs,
        3 if swarm.contend => Fault::Contend {
            release_at: tape.int_in(0..=40u32, "release at"),
            timeout: tape.one_in(3, "the lock wait times out"),
        },
        _ => Fault::None,
    }
}

// --- Rows as Parquet ------------------------------------------------------------------------

fn table(name: &str) -> usize {
    match schema::TABLES.iter().position(|it| it.name == name) {
        Some(at) => at,
        None => unreachable!("`{name}` is a table of the history"),
    }
}

/// The rows' ids as a Parquet file of one `Int64` column, written with `properties`: every file
/// of the test has this schema, so any of one table merge.
fn parquet_of(rows: &[i64], properties: &WriterProperties) -> Vec<u8> {
    // alloc: a test's file
    let schema = Arc::new(Schema::new(vec![Field::new("row", DataType::Int64, false)]));
    // alloc: a test's file
    let column: ArrayRef = Arc::new(Int64Array::from(rows.to_vec()));
    let batch = match RecordBatch::try_new(Arc::clone(&schema), vec![column]) {
        Ok(it) => it,
        Err(error) => panic!("a batch of {} rows: {error}", rows.len()),
    };
    // clone: writer properties, once a test's file
    let written = ArrowWriter::try_new(Vec::new(), schema, Some(properties.clone()))
        .and_then(|mut writer| writer.write(&batch).and_then(|()| writer.into_inner()));
    match written {
        Ok(bytes) => bytes,
        Err(error) => panic!("writing a test's file of {} rows: {error}", rows.len()),
    }
}

/// Every row of a Parquet file, or why it does not decode whole.
fn rows_of(bytes: Vec<u8>) -> Result<Vec<i64>, String> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))
        .and_then(|it| it.build())
        .map_err(|error| error.to_string())?;
    // alloc: a file's rows, once an oracle
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|error| error.to_string())?;
        let Some(column) = batch.column(0).as_primitive_opt::<Int64Type>() else {
            // alloc: an oracle's finding
            return Err(format!("its column is {}, not Int64", batch.column(0).data_type()));
        };
        rows.extend(column.values().iter().copied());
    }
    Ok(rows)
}

fn entry(rows: usize, pid: u32) -> Entry {
    Entry {
        version: schema::VERSION,
        rows: rows as u64,
        builds: None,
        t_ns: None,
        boot: None,
        pid: Some(pid),
    }
}

fn placed(name: Name, rows: &[i64], pid: u32, properties: &[WriterProperties]) -> Placed {
    Placed {
        file: name.file_name(),
        bytes: parquet_of(rows, &properties[name.table]),
        entry: entry(rows.len(), pid),
    }
}

// --- The recorder's mapping and the crash records, as the test writes them ------------------

fn nonce_of(pid: u32) -> u64 {
    0x5eed_0000 + u64::from(pid)
}

fn encode_mapping(nonce: u64, ring: &BTreeMap<i64, u64>) -> Vec<u8> {
    // alloc: a mapping, once a crash
    let mut text = format!("flight {nonce}\n");
    for (row, seq) in ring {
        text.push_str(&format!("{row} {seq}\n"));
    }
    text.into_bytes()
}

fn decode_mapping(bytes: &[u8]) -> Option<(u64, Vec<(i64, u64)>)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    let nonce = lines.next()?.strip_prefix("flight ")?.parse().ok()?;
    // alloc: a mapping's rows, once a read
    let mut ring = Vec::new();
    for line in lines {
        let (row, seq) = line.split_once(' ')?;
        ring.push((row.parse().ok()?, seq.parse().ok()?));
    }
    Some((nonce, ring))
}

/// What a dead daemon's mapping still owes the history, given what the manifest says the
/// history consumed of that pid's recorder.
enum Owed {
    /// Its rows are listed: deleted unread.
    Already,
    /// The rows past the last flush of this mapping the manifest says committed.
    Rows { nonce: u64, rows: Vec<i64> },
}

fn owed(bytes: &[u8], recorder: Option<Recorder>) -> Option<Owed> {
    let (nonce, ring) = decode_mapping(bytes)?;
    let mine = recorder.filter(|it| it.nonce == nonce);
    if mine.is_some_and(|it| it.recovered) {
        return Some(Owed::Already);
    }
    let flushed = mine.map_or(0, |it| it.flushed);
    // alloc: a mapping's rows, once a read
    let rows = ring.into_iter().filter(|&(_, seq)| seq > flushed).map(|(row, _)| row).collect();
    Some(Owed::Rows { nonce, rows })
}

/// A crash record's process, the instant of its signal, and its row.
fn decode_record(bytes: &[u8]) -> Option<(u32, i64, i64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut words = text.split_whitespace();
    (words.next()? == "crash").then_some(())?;
    let pid = words.next()?.parse().ok()?;
    let t_ns = words.next()?.parse().ok()?;
    let row = words.next()?.parse().ok()?;
    words.next().is_none().then_some((pid, t_ns, row))
}

/// A crash record's `panics` row, its row id in `job`.
fn panic_row(pid: u32, t_ns: i64, row: i64) -> PanicRow {
    PanicRow {
        build_id: None,
        process: pid,
        thread: 0,
        // alloc: none, empty
        thread_name: String::new(),
        t_ns,
        kind: PanicKind::Signal,
        job: Some(row as u64),
        span_id: None,
        message: None,
        location: None,
        // alloc: none, empty
        stack: Vec::new(),
        // alloc: none, empty
        stack_pcs: Vec::new(),
        boot: None,
        caught: false,
        // alloc: none, empty
        open_spans: Vec::new(),
        signal: Some("SIGSEGV"),
        fault_addr: None,
    }
}

// --- The host -------------------------------------------------------------------------------

/// A process of the simulation: alive while it is the newest.
struct TestHost {
    pid: u32,
    /// What a postmortem's files are written with (the `spans` table's).
    properties: Vec<WriterProperties>,
}

impl Host for TestHost {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn alive(&self, pid: u32) -> bool {
        pid == self.pid
    }

    fn crash(&mut self, _path: &Path, bytes: &[u8]) -> CrashRead {
        match decode_record(bytes) {
            Some((pid, t_ns, row)) => {
                CrashRead::Row(CrashRow { pid, t_ns, row: panic_row(pid, t_ns, row), image: None })
            }
            None => CrashRead::Nothing,
        }
    }

    fn recover(&mut self, dead: Dead) -> Recovery {
        match owed(dead.mapping.bytes(), dead.recorder) {
            Some(Owed::Already) => Recovery::Already,
            Some(Owed::Rows { nonce, rows }) => {
                // alloc: a postmortem's files
                let mut files = Vec::new();
                if !rows.is_empty() {
                    let name = Name {
                        build: 0,
                        table: table("spans"),
                        first: dead.seq,
                        last: dead.seq,
                        level: 0,
                    };
                    files.push(placed(name, &rows, dead.pid, &self.properties));
                }
                Recovery::Files { nonce, files }
            }
            None => Recovery::Refused(HistoryError::Io {
                path: dead.path,
                doing: "reading a test's mapping",
                error: io::Error::other("not `flight <nonce>` then `<row> <seq>` lines"),
            }),
        }
    }
}

// --- Driving the core -----------------------------------------------------------------------

/// How a job's drive ended.
enum Driven {
    Done(Outcome),
    Crashed,
}

/// Another process holding the manifest's lock while a job runs ([`Fault::Contend`]).
struct Contention {
    lock: Option<FileName>,
    release_at: u32,
    timeout: bool,
}

impl Contention {
    fn none() -> Contention {
        Contention { lock: None, release_at: 0, timeout: false }
    }

    fn step(&mut self, sim: &mut Sim, delivered: u32) {
        let Some(lock) = self.lock.as_ref() else { return };
        if delivered < self.release_at {
            return;
        }
        if self.timeout {
            // Past the writer's bound, and still held: its wait for the lock fails.
            sim.advance(manifest::LOCK_WAIT + Duration::from_secs(1));
            self.timeout = false;
            self.release_at = u32::MAX;
            return;
        }
        sim.release_lock(lock);
        self.lock = None;
    }
}

/// Runs the core's job under way on the simulator until it ends, or until `crash_at`
/// completions in, when the machine loses power.
fn drive(
    sim: &mut Sim,
    files: &mut Files<TestHost>,
    crash_at: Option<u32>,
    contention: &mut Contention,
    job: &str,
) -> Result<Driven, Broke> {
    // alloc: a drive's effects and completions, once a job
    let mut effects = Vec::new();
    // alloc: as above
    let mut done = Vec::new();
    let mut delivered = 0u32;
    for _ in 0..STEP_CAP {
        files.effects(&mut effects);
        sim.submit(&mut effects);
        if !files.busy() {
            return match files.outcome() {
                Some(outcome) => Ok(Driven::Done(outcome)),
                // alloc: an oracle's finding
                None => Err(Broke::Oracle(format!("{job}: the core is idle and told no outcome"))),
            };
        }
        contention.step(sim, delivered);
        if sim.poll(&mut done, blaze_core::wait::Deadline::NEVER) == 0 {
            let waits = files.waits_on();
            // alloc: an oracle's finding
            return Err(Broke::Oracle(format!(
                "{job}: the core waits ({waits:?}) with nothing in flight"
            )));
        }
        for completion in done.drain(..) {
            if crash_at == Some(delivered) {
                sim.crash();
                return Ok(Driven::Crashed);
            }
            files.complete(completion, sim.now());
            delivered += 1;
        }
    }
    // alloc: an oracle's finding
    Err(Broke::Oracle(format!(
        "{job}: not done after {STEP_CAP} polls ({delivered} completions): bounded-progress"
    )))
}

fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Opened(_) => "Opened",
        Outcome::Flushed(_) => "Flushed",
        Outcome::Recounted(_) => "Recounted",
        Outcome::WroteListed(_) => "WroteListed",
        Outcome::Read(_) => "Read",
        Outcome::Schemas(_) => "Schemas",
        Outcome::RecoveredDead(_) => "RecoveredDead",
        Outcome::Pinned(_) => "Pinned",
        Outcome::Unpinned => "Unpinned",
    }
}

fn into_opened(outcome: Outcome, job: &str) -> Result<Result<Opened, HistoryError>, Broke> {
    match outcome {
        Outcome::Opened(it) => Ok(it),
        other @ (Outcome::Flushed(_)
        | Outcome::Recounted(_)
        | Outcome::WroteListed(_)
        | Outcome::Read(_)
        | Outcome::Schemas(_)
        | Outcome::RecoveredDead(_)
        | Outcome::Pinned(_)
        | Outcome::Unpinned) => {
            // alloc: an oracle's finding
            Err(Broke::Oracle(format!("{job}: told {}, not Opened", outcome_name(&other))))
        }
    }
}

fn into_flushed(outcome: Outcome, job: &str) -> Result<Flushed, Broke> {
    match outcome {
        Outcome::Flushed(it) => Ok(it),
        other @ (Outcome::Opened(_)
        | Outcome::Recounted(_)
        | Outcome::WroteListed(_)
        | Outcome::Read(_)
        | Outcome::Schemas(_)
        | Outcome::RecoveredDead(_)
        | Outcome::Pinned(_)
        | Outcome::Unpinned) => {
            // alloc: an oracle's finding
            Err(Broke::Oracle(format!("{job}: told {}, not Flushed", outcome_name(&other))))
        }
    }
}

/// An error the simulator injected (`Faults::fail_one_in`): an open that fails with one is
/// asked again; any other is the history not opening.
fn injected(error: &HistoryError) -> bool {
    format!("{error:?}").contains("injected by the simulation")
}

// --- The world ------------------------------------------------------------------------------

/// The live process: its core, its recorder, and what it holds for its next flush.
struct Live {
    pid: u32,
    files: Files<TestHost>,
    /// Every row this process handed to a flush, and the flush it is pending at: its flight
    /// recorder, written as its mapping at a crash.
    ring: BTreeMap<i64, u64>,
    /// Rows in no placed file yet: the next flush's.
    held: Vec<i64>,
    /// Crash rows the open handed back and no placed file holds yet: each record's name, id
    /// and row.
    crashes: Vec<(String, (u32, i64), i64)>,
    /// The next flush's number.
    seq: u64,
}

struct World {
    sim: Sim,
    dir: FileName,
    plant: Plant,
    properties: Vec<WriterProperties>,
    /// Every row ever handed to the history or left in a crash record: what is owed, once.
    rows: BTreeSet<i64>,
    next_row: i64,
    next_pid: u32,
    /// The build flushes are named for: the next after each end.
    build: u64,
}

impl World {
    fn new(plan: &Plan, plant: Plant) -> World {
        let faults = Faults {
            fail_one_in: plan.fail_one_in,
            max_latency: Duration::from_micros(plan.latency_us),
            no_clone: false,
            lose_change_one_in: 0,
            // Half the runs wait under interrupted system calls (rule `deadline-waits`).
            interrupt_one_in: if plan.seed & 1 == 0 { 0 } else { 3 },
        };
        let Some(dir) = FileName::new(DIR) else { unreachable!("`{DIR}` is a name") };
        World {
            sim: Sim::new(plan.seed, faults),
            dir,
            plant,
            // alloc: each table's properties, once a run
            properties: schema::TABLES.iter().map(|it| super::super::properties(it)).collect(),
            rows: BTreeSet::new(),
            next_row: 1,
            next_pid: FIRST_PID,
            build: 1,
        }
    }

    fn file(&self, name: &str) -> FileName {
        // alloc: a file's name, once a look
        match FileName::new(&format!("{}/{name}", self.dir)) {
            Some(it) => it,
            None => unreachable!("`{name}` is a file's name"),
        }
    }

    fn row(&mut self) -> i64 {
        let row = self.next_row;
        self.next_row += 1;
        self.rows.insert(row);
        row
    }

    /// A new process opens the history, crashing `crash_at` completions into its first try;
    /// then the oracle.
    fn open(&mut self, mut crash_at: Option<u32>) -> Result<Live, Broke> {
        let mut tries = 0;
        'process: loop {
            let pid = self.next_pid;
            self.next_pid += 1;
            let host = TestHost { pid, properties: self.properties.clone() }; // clone: once a process
            // clone: the directory's name and the properties, once a process
            let mut files =
                Files::new(host, PathBuf::from(ROOT), self.dir.clone(), self.properties.clone());
            files.plant(self.plant);
            loop {
                // alloc: the job's name, once an open
                let job = format!("the open of pid {pid}");
                files.open(u64::MAX, Vec::new());
                let opened = match drive(
                    &mut self.sim,
                    &mut files,
                    crash_at.take(),
                    &mut Contention::none(),
                    &job,
                )? {
                    Driven::Crashed => {
                        // alloc: none, empty
                        self.record_crash(pid, &BTreeMap::new(), false);
                        continue 'process;
                    }
                    Driven::Done(outcome) => into_opened(outcome, &job)?,
                };
                let opened = match opened {
                    Ok(it) => it,
                    Err(error) if injected(&error) && tries < OPEN_TRIES => {
                        tries += 1;
                        continue;
                    }
                    Err(error) => {
                        // alloc: an oracle's finding
                        return Err(Broke::Oracle(format!(
                            "{job}: the history did not open ({tries} injected failures before): {error:?}"
                        )));
                    }
                };
                oracle(self, pid, &opened)
                    .map_err(|why| Broke::Oracle(format!("after {job}: {why}")))?;
                // alloc: the crash rows handed back, once an open
                let mut crashes = Vec::new();
                let returned =
                    opened.crashes.rows.iter().zip(&opened.crashes.ids).zip(&opened.crashes.files);
                for ((row, &id), name) in returned {
                    let Some(job) = row.job else {
                        // alloc: a test's finding
                        return Err(Broke::Harness(format!(
                            "{name}: a crash row with no row id in `job`"
                        )));
                    };
                    // clone: a record's name, once an open
                    crashes.push((name.clone(), id, job as i64));
                }
                return Ok(Live {
                    pid,
                    files,
                    ring: BTreeMap::new(),
                    // alloc: none, empty
                    held: Vec::new(),
                    crashes,
                    seq: opened.seq,
                });
            }
        }
    }

    fn fault(&mut self, fault: Fault) -> Contention {
        match fault {
            Fault::None => Contention::none(),
            Fault::Fill { room } => {
                // clone: a refcount
                self.sim.fill(self.dir.clone(), room);
                Contention::none()
            }
            Fault::FailSyncs => {
                // clone: a refcount
                self.sim.fail_syncs(self.dir.clone());
                Contention::none()
            }
            Fault::Contend { release_at, timeout } => {
                let lock = self.file(manifest::LOCK);
                self.sim.hold_lock(&lock, LockMode::Exclusive);
                Contention { lock: Some(lock), release_at, timeout }
            }
        }
    }

    /// What a fault left for the flush alone let go of (a failing sync stays until a crash).
    fn unfault(&mut self, fault: Fault) {
        match fault {
            Fault::None | Fault::FailSyncs => {}
            // clone: a refcount
            Fault::Fill { .. } => self.sim.fill(self.dir.clone(), u64::MAX),
            Fault::Contend { .. } => {
                let lock = self.file(manifest::LOCK);
                self.sim.release_lock(&lock);
            }
        }
    }

    /// One flush of `live`: the rows it holds and `rows` new ones, and the crash rows its open
    /// handed back. Whether the machine crashed during it.
    fn flush(
        &mut self,
        live: &mut Live,
        rows: u8,
        end: bool,
        fault: Fault,
        crash: Option<Crash>,
    ) -> Result<bool, Broke> {
        let seq = live.seq;
        live.seq += 1;
        for _ in 0..rows {
            let row = self.row();
            live.held.push(row);
        }
        for &row in &live.held {
            live.ring.insert(row, seq);
        }
        // alloc: a flush's files
        let mut files = Vec::new();
        let mut spans = None;
        if !live.held.is_empty() {
            spans = Some(files.len());
            let name =
                Name { build: self.build, table: table("spans"), first: seq, last: seq, level: 0 };
            files.push(placed(name, &live.held, live.pid, &self.properties));
        }
        let mut panics = None;
        let mut consumed = None;
        if !live.crashes.is_empty() {
            let at = files.len();
            panics = Some(at);
            let name =
                Name { build: self.build, table: table("panics"), first: seq, last: seq, level: 0 };
            // alloc: a flush's crash rows
            let rows: Vec<i64> = live.crashes.iter().map(|it| it.2).collect();
            files.push(placed(name, &rows, live.pid, &self.properties));
            let written = crash::Written {
                // clone: a record's name, once a flush
                files: live.crashes.iter().map(|it| it.0.clone()).collect(),
                // alloc: a flush's crash ids
                ids: live.crashes.iter().map(|it| it.1).collect(),
            };
            consumed = Some((at, written));
        }
        live.files.flush(Flush {
            seq,
            files,
            recorder: Some((live.pid, nonce_of(live.pid))),
            crashes: consumed,
            ended: end.then_some(self.build),
        });
        let mut contention = self.fault(fault);
        // alloc: the job's name, once a flush
        let job = format!("flush {seq} of pid {}", live.pid);
        let driven =
            drive(&mut self.sim, &mut live.files, crash.map(|it| it.at), &mut contention, &job);
        self.unfault(fault);
        if end {
            self.build += 1;
        }
        let flushed = match driven? {
            Driven::Crashed => return Ok(true),
            Driven::Done(outcome) => into_flushed(outcome, &job)?,
        };
        let was_placed = |at: usize| match flushed.placed.get(at) {
            Some(it) => Ok(it.is_none()),
            // alloc: an oracle's finding
            None => Err(Broke::Oracle(format!(
                "{job}: its outcome says nothing of file {at} of {}",
                flushed.placed.len()
            ))),
        };
        if let Some(at) = spans {
            match was_placed(at)? {
                true => live.held.clear(),
                // Kept for the next flush, and marked in the recorder as its.
                false => {
                    for &row in &live.held {
                        live.ring.insert(row, live.seq);
                    }
                }
            }
        }
        if let Some(at) = panics
            && was_placed(at)?
        {
            live.crashes.clear();
        }
        Ok(false)
    }
}

// --- The oracle -----------------------------------------------------------------------------

/// After the open of `pid` that came to `opened`, with no job running: the manifest decodes;
/// every file it lists is there and decodes whole, holding the rows its entry says; and every
/// row ever handed to the history is, exactly once, in a listed file, in a crash row the open
/// handed back, or still in a dead mapping or crash record the open said it could not read.
/// An open that said no postmortem error leaves no dead mapping owing rows; one that refused
/// no crash record leaves none unread.
fn oracle(world: &World, pid: u32, opened: &Opened) -> Result<(), String> {
    let manifest_file = world.file(manifest::FILE);
    let manifest = Manifest::decode(&world.sim.contents(&manifest_file))
        .map_err(|why| format!("the manifest does not decode: {why:?}"))?;
    // Each row, and everywhere it is.
    // alloc: an oracle's map
    let mut seen: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for name in manifest.all() {
        let file = world.file(name);
        if !world.sim.exists(&file) {
            return Err(format!("{name}: the manifest lists it and it is not there"));
        }
        let rows = rows_of(world.sim.contents(&file))
            .map_err(|why| format!("{name}: listed, and torn: {why}"))?;
        if let Some(entry) = manifest.entry(name)
            && entry.rows != rows.len() as u64
        {
            return Err(format!(
                "{name}: its entry says {} rows and it holds {}",
                entry.rows,
                rows.len()
            ));
        }
        for row in rows {
            // alloc: an oracle's note
            seen.entry(row).or_default().push(name.to_owned());
        }
    }
    // alloc: an oracle's set
    let mut returned = BTreeSet::new();
    for (row, id) in opened.crashes.rows.iter().zip(&opened.crashes.ids) {
        let Some(job) = row.job else {
            return Err(format!("crash {id:?}: handed back with no row id"));
        };
        returned.insert(job as i64);
        // alloc: an oracle's note
        seen.entry(job as i64)
            .or_default()
            .push(format!("the crash row {id:?} the open handed back"));
    }
    let consumed = manifest.consumed();
    for dead in FIRST_PID..pid {
        let mapping = world.file(&postmortem::file_name(dead));
        if world.sim.exists(&mapping) {
            let bytes = world.sim.contents(&mapping);
            match owed(&bytes, consumed.recorders.get(&dead).copied()) {
                None => return Err(format!("flight-{dead}.map: not a mapping the test wrote")),
                Some(Owed::Already) => {}
                Some(Owed::Rows { nonce, rows }) => {
                    if opened.postmortem.is_none() {
                        return Err(format!(
                            "flight-{dead}.map (nonce {nonce}) still owes {} rows, and the open said no postmortem error",
                            rows.len()
                        ));
                    }
                    for row in rows {
                        // alloc: an oracle's note
                        seen.entry(row)
                            .or_default()
                            .push(format!("flight-{dead}.map, left for the next open"));
                    }
                }
            }
        }
        let record = world.file(&crash::file_name(dead));
        if world.sim.exists(&record) {
            let Some((of, t_ns, row)) = decode_record(&world.sim.contents(&record)) else {
                return Err(format!("crash.{dead}: not a record the test wrote"));
            };
            if consumed.crashes.contains(&(of, t_ns)) || returned.contains(&row) {
                continue;
            }
            if opened.crashes.refused.is_empty() {
                return Err(format!(
                    "crash.{dead}: neither consumed nor handed back, and the open refused no record"
                ));
            }
            // alloc: an oracle's note
            seen.entry(row).or_default().push(format!("crash.{dead}, left for the next open"));
        }
    }
    if let Some((row, places)) = seen.iter().find(|(_, places)| places.len() > 1) {
        return Err(format!("row {row} is there {} times: {}", places.len(), places.join("; ")));
    }
    if let Some(row) = seen.keys().find(|row| !world.rows.contains(row)) {
        return Err(format!("row {row} was never handed to the history, and is in {:?}", seen[row]));
    }
    if let Some(row) = world.rows.iter().find(|row| !seen.contains_key(row)) {
        let all = world.rows.len();
        return Err(format!(
            "row {row} is lost: in no listed file, crash row or file left to read ({} of {all} rows found)",
            seen.len()
        ));
    }
    Ok(())
}

// --- A run ----------------------------------------------------------------------------------

/// `plan` run with `plant` planted: the oracle after every reopen and at the end, which
/// crashes and reopens once more.
fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new(plan, plant);
    let mut live = world.open(None)?;
    for act in &plan.acts {
        let crash = match *act {
            Act::Flush { rows, end, fault, crash } => {
                if !world.flush(&mut live, rows, end, fault, crash)? {
                    continue;
                }
                // `drive` lost the power at the flush's crash point.
                let Some(crash) = crash else {
                    return Err(Broke::Harness("a flush crashed with no crash point drawn".into()));
                };
                crash
            }
            Act::Crash(crash) => {
                world.sim.crash();
                crash
            }
        };
        let Live { pid, ring, .. } = live;
        world.record_crash(pid, &ring, crash.record);
        live = world.open(crash.reopen_at)?;
    }
    world.sim.crash();
    let Live { pid, ring, .. } = live;
    world.record_crash(pid, &ring, false);
    world.open(None)?;
    Ok(())
}

impl World {
    /// What a process that lost power left: its mapping and maybe a crash record (the power
    /// is out already).
    fn record_crash(&mut self, pid: u32, ring: &BTreeMap<i64, u64>, record: bool) {
        let mapping = self.file(&postmortem::file_name(pid));
        self.sim.change(&mapping, &encode_mapping(nonce_of(pid), ring));
        if record {
            let row = self.row();
            let file = self.file(&crash::file_name(pid));
            self.sim.change(&file, format!("crash {pid} {row} {row}").as_bytes());
        }
    }
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: whatever the faults, wherever the power goes out, every row handed to the
/// history is listed, handed back or left to read, exactly once, and the history opens.
#[test]
fn every_row_is_in_the_history_once_after_any_crash() {
    blaze_sim::check(Config::cases(200), |tape| {
        let plan = plan(tape);
        tape.note(format_args!("plan {plan:?}"));
        run(&plan, Plant::None)
    });
}

/// A plant the oracle must catch in one of [`CONTROL`]'s plans (test-the-cause on the test
/// itself): a green DST with a blind oracle proves nothing. Any of the oracle's findings is the
/// catch, as before; the test's own trouble fails the control.
fn control(plant: Plant) {
    blaze_sim::control(CONTROL, Plant::None, plant, "", |tape, plant| run(&plan(tape), plant));
}

#[test]
fn the_oracle_catches_a_rename_with_no_sync_before() {
    control(Plant::NoSyncBeforeRename);
}

#[test]
fn the_oracle_catches_what_a_flush_consumed_marked_after_its_commit() {
    control(Plant::MarkAfterCommit);
}

/// The plan the property test shrank to on 2026-11-04, written out: pid 106's flush of row 5 fails to place
/// its file (an injected failure), so the row is kept for the next flush; that flush's file
/// fails to place too, its commit is done, and the power goes out on the next completion,
/// before the test can re-mark the row as the next flush's. The core used to commit a flush
/// with a file not placed, saying the recorder's flush listed, so the postmortem read past row
/// 5 and it was lost. A flush with a file not placed is now not committed
/// ([`HistoryError::Unplaced`]).
#[test]
fn a_row_whose_file_was_not_placed_survives_a_crash_after_that_flush() {
    let crash = |at, record, reopen_at| Crash { at, record, reopen_at };
    let flush = |rows, crash| Act::Flush { rows, end: true, fault: Fault::None, crash };
    let plan = Plan {
        seed: 5_047_471_726,
        fail_one_in: 20,
        latency_us: 1,
        // alloc: a test's plan
        acts: vec![
            Act::Crash(crash(0, false, Some(0))),
            Act::Crash(crash(0, true, None)),
            flush(0, None),
            flush(0, Some(crash(8, false, Some(0)))),
            flush(1, None),
            flush(1, Some(crash(0, true, None))),
            flush(0, None),
            flush(0, None),
            flush(1, None),
            flush(0, Some(crash(8, false, None))),
        ],
    };
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

// --- The cap --------------------------------------------------------------------------------

/// A listed `spans` file of flush `seq`, named for `named`, holding rows of `builds`.
fn capped_file(named: u64, seq: u64, builds: Option<[u64; 2]>, bytes: u64) -> Kept {
    let name = Name { build: named, table: table("spans"), first: seq, last: seq, level: 0 };
    Kept {
        name,
        file: name.file_name(),
        bytes,
        entry: Entry { builds, ..entry(10, FIRST_PID) },
        ours: Ours::Ours,
        listed: Listed::Listed,
    }
}

/// The builds the cap must keep whole, worked out apart from the core: every running build and
/// the `keep` highest of the others at an end of a file's range. A number strictly inside a
/// range may be no build at all (build ids skip), so it is not counted; a build there is in
/// one unit with both ends. The simulation below holds the core to the stronger promise, every
/// build that had rows counted, which this definition implies.
fn kept_builds_reference(kept: &[Kept], running: &BTreeSet<u64>, keep: usize) -> BTreeSet<u64> {
    let builds: BTreeSet<u64> = kept.iter().filter_map(builds_of).flatten().collect();
    let mut held = running.clone();
    held.extend(builds.iter().rev().filter(|it| !running.contains(*it)).take(keep).copied());
    held
}

/// What the cap must hold of the files `gone` of `kept`, or what it broke: no build keeps a
/// part of its rows, nothing of a kept build goes (every running one and the last `keep`
/// finished), and the newest flush's file of no build stays.
fn cap_oracle(
    kept: &[Kept],
    gone: &BTreeSet<usize>,
    running: &BTreeSet<u64>,
    keep: usize,
) -> Result<(), String> {
    let held = kept_builds_reference(kept, running, keep);
    let builds: BTreeSet<u64> =
        kept.iter().filter_map(builds_of).flat_map(|[lo, hi]| lo..=hi).collect();
    for build in builds {
        let of = |at: &usize| builds_of(&kept[*at]).is_some_and(|[lo, hi]| lo <= build && build <= hi);
        let holding: Vec<usize> = (0..kept.len()).filter(of).collect();
        let went = holding.iter().filter(|at| gone.contains(*at)).count();
        if went != 0 && went != holding.len() {
            return Err(format!(
                "build {build} lost {went} of its {} files and kept the rest: {gone:?} of {kept:?}",
                holding.len()
            ));
        }
        if went != 0 && running.contains(&build) {
            return Err(format!("build {build} is running and lost its files: {gone:?}"));
        }
        if went != 0 && held.contains(&build) {
            return Err(format!(
                "build {build} is one of the last {keep} finished and lost its files: {gone:?}"
            ));
        }
    }
    if let Some(newest) = kept.len().checked_sub(1)
        && builds_of(&kept[newest]).is_none()
        && gone.contains(&newest)
    {
        return Err(format!("the newest flush's file of no build went: {gone:?}"));
    }
    Ok(())
}

/// What the cap's order promises beyond the kept builds: no unit goes while an older one the
/// rules let go stays. A unit is the files whose build ranges chain together (a file of no
/// build alone), as old as its newest flush; units of no build come before units of builds.
fn oldest_first_oracle(
    kept: &[Kept],
    gone: &BTreeSet<usize>,
    running: &BTreeSet<u64>,
    keep: usize,
) -> Result<(), String> {
    let held = kept_builds_reference(kept, running, keep);
    // Each file's unit, by the least file in it: files chained by overlapping ranges.
    let mut unit: Vec<usize> = (0..kept.len()).collect();
    loop {
        let mut changed = false;
        for a in 0..kept.len() {
            for b in 0..kept.len() {
                let overlaps = match (builds_of(&kept[a]), builds_of(&kept[b])) {
                    (Some([lo, hi]), Some([c, d])) => c <= hi && lo <= d,
                    _ => false,
                };
                if overlaps && unit[b] < unit[a] {
                    unit[a] = unit[b];
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let newest = kept.len().saturating_sub(1);
    // Each unit: (of a build, its newest flush, went, may go).
    let mut units: BTreeMap<usize, (bool, u64, bool, bool)> = BTreeMap::new();
    for (at, it) in kept.iter().enumerate() {
        let entry = units.entry(unit[at]).or_insert((false, 0, false, true));
        let builds = builds_of(it);
        entry.0 |= builds.is_some();
        entry.1 = entry.1.max(it.name.last);
        entry.2 |= gone.contains(&at);
        let kept_build = builds.is_some_and(|[lo, hi]| held.range(lo..=hi).next().is_some());
        let newest_idle = at == newest && builds.is_none();
        entry.3 &= !kept_build && !newest_idle;
    }
    let order = |it: &(bool, u64, bool, bool)| (it.0, it.1);
    for (first, a) in &units {
        for (second, b) in &units {
            if a.2 && !b.2 && b.3 && order(b) < order(a) {
                return Err(format!(
                    "the unit of file {first} went before the older unit of file {second}, which \
                     the rules let go: {gone:?} of {kept:?}"
                ));
            }
        }
    }
    Ok(())
}

/// The cap as it was until 2026-11-11, the planted bug of [`cap_oracle`]'s control: the unit a
/// file goes with is the build it is named for alone, whatever builds its rows are of.
fn past_cap_by_name_alone(kept: &[Kept], total: u64, cap: u64) -> BTreeSet<usize> {
    let mut gone = BTreeSet::new();
    let newest = kept.last().map_or(0, |it| it.name.last);
    let mut units: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    for it in kept {
        let unit = units.entry(it.name.build).or_insert((0, 0));
        unit.0 = unit.0.max(it.name.last);
        unit.1 += it.bytes;
    }
    let mut order: Vec<(u64, u64, u64)> =
        units.into_iter().map(|(build, (last, bytes))| (last, build, bytes)).collect();
    order.sort_unstable();
    let mut left = total;
    for (last, build, bytes) in order {
        if left <= cap {
            break;
        }
        if last == newest {
            continue;
        }
        left -= bytes;
        gone.extend((0..kept.len()).filter(|at| kept[*at].name.build == build));
    }
    gone
}

/// The invariant: past the cap no build loses a part of its rows, a running build and the last
/// `keep` finished lose none, the newest flush's file of no build stays, and beyond those the
/// oldest unit goes first, whichever builds ran at once and whatever each flush was named for.
#[test]
fn the_cap_retires_whole_builds_and_none_that_runs() {
    blaze_sim::check(Config::cases(300), cap_retires_whole_builds);
}

fn cap_retires_whole_builds(tape: &Tape) {
    let files = tape.int_in(1..=24u64, "files");
    let mut kept = Vec::new();
    for seq in 1..=files {
        // A flush holds rows of builds `lo..=hi` and is named for the highest, or holds none.
        let builds = match tape.one_in(6, "of no build") {
            true => None,
            false => {
                let lo = tape.int_in(1..=12u64, "first build");
                Some([lo, lo + tape.int_in(0..=3u64, "more builds")])
            }
        };
        let named = builds.map_or(0, |[_, hi]| hi);
        kept.push(capped_file(named, seq, builds, tape.int_in(1..=100u64, "bytes")));
    }
    let running: BTreeSet<u64> =
        (0..tape.int_in(0..=3u64, "running builds")).map(|_| tape.int_in(1..=15u64, "running")).collect();
    let total: u64 = kept.iter().map(|it| it.bytes).sum();
    let cap = tape.int_in(0..=total, "cap");
    let keep = usize::from(tape.int_in(0..=3u8, "kept builds"));
    let gone = past_cap(kept.iter(), total, cap, &running, keep);
    if let Err(broke) = cap_oracle(&kept, &gone, &running, keep)
        .and_then(|()| oldest_first_oracle(&kept, &gone, &running, keep))
    {
        panic!("{broke}\ncap {cap} of {total}, running {running:?}, keep {keep}");
    }
    // What stays is under the cap, or every file left is one the rules keep: a part of a kept
    // build's unit, or the newest flush's file of no build.
    let left: u64 = (0..kept.len()).filter(|at| !gone.contains(at)).map(|at| kept[at].bytes).sum();
    if left > cap {
        let rest: Vec<usize> = (0..kept.len()).filter(|at| !gone.contains(at)).collect();
        // Retiring any one more unit breaks a rule: try each file left with every file that
        // shares a build with it, transitively, and see the oracle refuse it.
        for at in rest {
            let mut more = gone.clone();
            let mut range = builds_of(&kept[at]);
            more.insert(at);
            loop {
                let before = more.len();
                for (other, it) in kept.iter().enumerate() {
                    let overlaps = match (range, builds_of(it)) {
                        (Some([lo, hi]), Some([a, b])) => a <= hi && lo <= b,
                        _ => false,
                    };
                    if overlaps {
                        more.insert(other);
                        let [a, b] = builds_of(it).unwrap();
                        range = range.map(|[lo, hi]| [lo.min(a), hi.max(b)]);
                    }
                }
                if more.len() == before {
                    break;
                }
            }
            assert!(
                cap_oracle(&kept, &more, &running, keep).is_err(),
                "over the cap ({left} of {cap}) with a unit that could go: file {at} of {kept:?}, \
                 gone {gone:?}, running {running:?}, keep {keep}"
            );
        }
    }
}

/// What the cap's order promises: a build's file goes only once every file of no build has
/// gone, the newest flush's aside (which never goes).
fn no_build_first_oracle(kept: &[Kept], gone: &BTreeSet<usize>) -> Result<(), String> {
    let Some(build) = gone.iter().find(|at| builds_of(&kept[**at]).is_some()) else { return Ok(()) };
    let newest = kept.len() - 1;
    match (0..newest).find(|at| builds_of(&kept[*at]).is_none() && !gone.contains(at)) {
        Some(idle) => Err(format!(
            "file {build}, a build's, went while file {idle}, of no build, stays: {gone:?} of {kept:?}"
        )),
        None => Ok(()),
    }
}

/// The invariant: past the cap, rows of no build (a daemon's own between builds) go before
/// any build's rows do, whatever their ages.
#[test]
fn the_cap_retires_rows_of_no_build_before_any_build_s() {
    blaze_sim::check(Config::cases(300), |tape| {
        let files = tape.int_in(1..=24u64, "files");
        let mut kept = Vec::new();
        for seq in 1..=files {
            let builds = match tape.coin("of no build") {
                true => None,
                false => {
                    let lo = tape.int_in(1..=12u64, "first build");
                    Some([lo, lo + tape.int_in(0..=3u64, "more builds")])
                }
            };
            kept.push(capped_file(builds.map_or(0, |[_, hi]| hi), seq, builds, tape.int_in(1..=100u64, "bytes")));
        }
        let running: BTreeSet<u64> =
            (0..tape.int_in(0..=3u64, "running builds")).map(|_| tape.int_in(1..=15u64, "running")).collect();
        let total: u64 = kept.iter().map(|it| it.bytes).sum();
        let cap = tape.int_in(0..=total, "cap");
        let keep = usize::from(tape.int_in(0..=3u8, "kept builds"));
        let gone = past_cap(kept.iter(), total, cap, &running, keep);
        if let Err(broke) = no_build_first_oracle(&kept, &gone) {
            panic!("{broke}\ncap {cap} of {total}, running {running:?}, keep {keep}");
        }
        cap_oracle(&kept, &gone, &running, keep).unwrap();
    });
}

/// A day of builds, then a daemon left idle: builds 1 and 2, then four flushes of no build.
/// The cap as it was (age alone) retired the builds and kept the idle rows; the oracle
/// catches that, and holds for the cap as it is, which retires the idle rows and keeps both
/// builds.
#[test]
fn the_oracle_catches_a_build_retired_while_rows_of_no_build_stay() {
    let kept = [
        capped_file(1, 1, Some([1, 1]), 100),
        capped_file(2, 2, Some([2, 2]), 100),
        capped_file(0, 3, None, 100),
        capped_file(0, 4, None, 100),
        capped_file(0, 5, None, 100),
        capped_file(0, 6, None, 100),
    ];
    // No finished build kept by count: what is tested is the order alone.
    let none = BTreeSet::new();
    let planted = past_cap_in(kept.iter(), 600, 400, &none, 0, CapRule::OldestFirst);
    assert_eq!(planted, BTreeSet::from([0, 1]), "age alone retired both builds");
    let caught = no_build_first_oracle(&kept, &planted).unwrap_err();
    assert!(caught.contains("a build's, went while file 2, of no build, stays"), "{caught}");

    let gone = past_cap(kept.iter(), 600, 400, &none, 0);
    assert_eq!(gone, BTreeSet::from([2, 3]), "the oldest rows of no build go, and no build's");
    no_build_first_oracle(&kept, &gone).unwrap();
    cap_oracle(&kept, &gone, &none, 0).unwrap();
}

/// The 80-minute build of 2026-11-11: build 7 runs for the whole history while builds 8 and 9
/// run beside it and end. The flushes that held build 8's or 9's rows were named for them,
/// with build 7's rows inside. The cap as it was retired "build 8" and "build 9" whole, and
/// with them build 7's rows of those stretches, leaving its last flushes: the oracle catches
/// that (the control), and holds for the cap as it is, which retires none of them while build
/// 7 runs and all of them at once after it ended. One finished build is kept by count (build
/// 10, the last), so builds 7 to 9 are what the cap may take.
#[test]
fn a_long_build_keeps_the_rows_it_shares_files_with_shorter_builds_in() {
    let kept = [
        capped_file(7, 1, Some([7, 7]), 100),
        capped_file(8, 2, Some([7, 8]), 100),
        capped_file(8, 3, Some([7, 8]), 100),
        capped_file(9, 4, Some([7, 9]), 100),
        capped_file(7, 5, Some([7, 7]), 100),
        capped_file(10, 6, Some([10, 10]), 100),
    ];
    let total = 600;
    let cap = 300;

    let planted = past_cap_by_name_alone(&kept, total, cap);
    assert_eq!(planted, BTreeSet::from([1, 2, 3]), "the old cap retired builds 8 and 9");
    let running = BTreeSet::from([7]);
    let caught = cap_oracle(&kept, &planted, &running, 1).unwrap_err();
    assert!(caught.contains("build 7 lost 3 of its 5 files"), "{caught}");

    // While build 7 runs, nothing of it goes, though the history stays over its cap.
    let gone = past_cap(kept.iter(), total, cap, &running, 1);
    assert_eq!(gone, BTreeSet::new());
    cap_oracle(&kept, &gone, &running, 1).unwrap();

    // Once it ended, builds 7 to 9 are one unit and go together; build 10, the last, stays.
    let gone = past_cap(kept.iter(), total, cap, &BTreeSet::new(), 1);
    assert_eq!(gone, BTreeSet::from([0, 1, 2, 3, 4]));
    cap_oracle(&kept, &gone, &BTreeSet::new(), 1).unwrap();

    // What went is counted for the WARN event that says it; what is left fits, so nothing is
    // over (the trim fills `over` in; adding files leaves it 0).
    let mut capped = Capped::default();
    for at in &gone {
        capped.add(&kept[*at]);
    }
    assert_eq!(capped, Capped { files: 5, rows: 50, bytes: 500, builds: Some([7, 9]), over: 0 });
}

// --- The kept builds, through flushes, pins and restarts ------------------------------------

/// The pid of the outside readers' pins (`<pid>-<n>`, as [`retire`] names them): a process the
/// core never asks about, played by the test through [`Sim::hold_lock`].
const READER_PID: u32 = 9_000;

/// Most acts a case of the kept builds' test draws.
const KEPT_ACTS: u64 = 40;

/// An outside reader's pin, laid out as [`retire`] lays one: its lock file, held by another
/// process, the generation it read said beside it, and the files the manifest it read listed.
struct ReaderPin {
    lock: FileName,
    read: Vec<String>,
}

/// One case of the kept builds' test: the core of the live process, and the model it is held
/// to.
struct KeptWorld {
    sim: Sim,
    dir: FileName,
    properties: Vec<WriterProperties>,
    files: Files<TestHost>,
    pid: u32,
    keep: usize,
    cap: u64,
    rule: CapRule,
    seq: u64,
    next_row: i64,
    next_build: u64,
    running: BTreeSet<u64>,
    /// Every row handed to the history and the builds it is of (empty: of no build).
    rows: BTreeMap<i64, Vec<u64>>,
    pins: Vec<ReaderPin>,
    next_pin: u32,
}

/// A process `pid` opens the history at `dir`, the cap keeping `keep` finished builds by
/// `rule`, holding `cap` bytes: its core and the next flush's number.
fn opened_core(
    sim: &mut Sim,
    dir: &FileName,
    properties: &[WriterProperties],
    pid: u32,
    keep: usize,
    cap: u64,
    rule: CapRule,
) -> Result<(Files<TestHost>, u64), Broke> {
    let host = TestHost { pid, properties: properties.to_vec() }; // clone: once a process
    // clone: the directory's name and the properties, once a process
    let mut files = Files::new(host, PathBuf::from(ROOT), dir.clone(), properties.to_vec());
    files.keep_builds(keep);
    files.cap_rule(rule);
    files.open(cap, Vec::new());
    let job = format!("the open of pid {pid}");
    let opened = match drive(sim, &mut files, None, &mut Contention::none(), &job)? {
        Driven::Done(outcome) => into_opened(outcome, &job)?,
        Driven::Crashed => return Err(Broke::Harness(format!("{job}: crashed with no crash drawn"))),
    };
    match opened {
        Ok(opened) => Ok((files, opened.seq)),
        Err(error) => Err(Broke::Oracle(format!("{job}: the history did not open: {error:?}"))),
    }
}

impl KeptWorld {
    fn new(seed: u64, keep: usize, cap: u64, rule: CapRule) -> Result<KeptWorld, Broke> {
        // No injected faults: the crash-safety of every commit is the other DST's; this one is
        // about what the cap keeps.
        let faults = Faults {
            fail_one_in: 0,
            max_latency: Duration::from_micros(50),
            no_clone: false,
            lose_change_one_in: 0,
            interrupt_one_in: 0,
        };
        let mut sim = Sim::new(seed, faults);
        let Some(dir) = FileName::new(DIR) else { unreachable!("`{DIR}` is a name") };
        let properties: Vec<WriterProperties> =
            schema::TABLES.iter().map(|it| super::super::properties(it)).collect();
        let (files, seq) = opened_core(&mut sim, &dir, &properties, FIRST_PID, keep, cap, rule)?;
        Ok(KeptWorld {
            sim,
            dir,
            properties,
            files,
            pid: FIRST_PID,
            keep,
            cap,
            rule,
            seq,
            next_row: 1,
            next_build: 1,
            running: BTreeSet::new(),
            rows: BTreeMap::new(),
            pins: Vec::new_in(Global),
            next_pin: 1,
        })
    }

    fn file(&self, name: &str) -> FileName {
        match FileName::new(&format!("{}/{name}", self.dir)) {
            Some(it) => it,
            None => unreachable!("`{name}` is a file's name"),
        }
    }

    fn manifest(&self) -> Result<Manifest, Broke> {
        let file = self.file(manifest::FILE);
        if !self.sim.exists(&file) {
            return Ok(Manifest::new());
        }
        Manifest::decode(&self.sim.contents(&file))
            .map_err(|why| Broke::Oracle(format!("the manifest does not decode: {why:?}")))
    }

    /// A build starts: the core keeps every file holding a row of it while it runs.
    fn start(&mut self) {
        let build = self.next_build;
        self.next_build += 1;
        self.running.insert(build);
        self.files.running(build, true);
    }

    /// A flush of `rows` new rows, of every running build and of `ended` (a build whose end
    /// this flush is for, noted ended before it as the daemon does), or of no build. Its files
    /// are named as the daemon names them: for the highest build of their rows.
    fn flush(&mut self, rows: u8, ended: Option<u64>) -> Result<(), Broke> {
        if let Some(build) = ended {
            self.running.remove(&build);
            self.files.running(build, false);
        }
        let seq = self.seq;
        self.seq += 1;
        let mut of: Vec<u64> = self.running.iter().copied().chain(ended).collect();
        of.sort_unstable();
        of.dedup();
        let mut placing = Vec::new_in(Global);
        if rows > 0 {
            let mut ids = Vec::new_in(Global);
            for _ in 0..rows {
                let row = self.next_row;
                self.next_row += 1;
                self.rows.insert(row, of.clone()); // clone: a row's builds, once a row
                ids.push(row);
            }
            let named = of.last().copied().unwrap_or(0);
            let name = Name { build: named, table: table("spans"), first: seq, last: seq, level: 0 };
            let mut file = placed(name, &ids, self.pid, &self.properties);
            file.entry.builds = of.first().zip(of.last()).map(|(&lo, &hi)| [lo, hi]);
            placing.push(file);
        }
        self.files.flush(Flush { seq, files: placing, recorder: None, crashes: None, ended });
        let job = format!("flush {seq} of pid {}", self.pid);
        let flushed = match drive(&mut self.sim, &mut self.files, None, &mut Contention::none(), &job)? {
            Driven::Done(outcome) => into_flushed(outcome, &job)?,
            Driven::Crashed => return Err(Broke::Harness(format!("{job}: crashed with no crash drawn"))),
        };
        if let Err(error) = &flushed.committed {
            return Err(Broke::Oracle(format!("{job}: not committed: {error:?}")));
        }
        if let Some(error) = flushed.placed.iter().flatten().next() {
            return Err(Broke::Oracle(format!("{job}: a file not placed: {error:?}")));
        }
        if let Some(error) = flushed.deferred.first() {
            return Err(Broke::Oracle(format!("{job}: failed after its commit: {error:?}")));
        }
        Ok(())
    }

    /// An outside reader pins the history and reads the manifest, as [`super::super::pin`]
    /// does: its lock file made and held by another process, then the generation it read said.
    fn pin(&mut self) -> Result<(), Broke> {
        let id = format!("{READER_PID}-{}", self.next_pin);
        self.next_pin += 1;
        let lock = self.file(&format!("{PINS}/{}", PinFile::Lock { id: &id }.file_name()));
        self.sim.change(&lock, b"");
        self.sim.hold_lock(&lock, LockMode::Exclusive);
        let manifest = self.manifest()?;
        let generation = manifest.retired().generation;
        let said = self.file(&format!("{PINS}/{}", PinFile::Said { id: &id, generation }.file_name()));
        self.sim.change(&said, b"");
        let read = manifest.all().map(str::to_owned).collect();
        self.pins.push(ReaderPin { lock, read });
        Ok(())
    }

    /// The reader at `at` ends without a word, as a killed `blaze slow` would: its lock goes,
    /// its files stay for the writer's reap to find unlocked.
    fn unpin(&mut self, at: usize) {
        let pin = self.pins.remove(at);
        self.sim.release_lock(&pin.lock);
    }

    /// The daemon stops and a new one opens the history: every lock goes (the readers' too),
    /// and the builds that were running are builds that ended.
    fn restart(&mut self) -> Result<(), Broke> {
        let before = (self.running.is_empty()).then(|| self.manifest()).transpose()?;
        self.sim.crash();
        self.pins.clear();
        self.running.clear();
        self.pid += 1;
        let (files, seq) =
            opened_core(&mut self.sim, &self.dir, &self.properties, self.pid, self.keep, self.cap, self.rule)?;
        self.files = files;
        self.seq = seq;
        // With nothing running the open's trim has nothing new to decide: the same files.
        if let Some(before) = before {
            let after = self.manifest()?;
            let listed = |it: &Manifest| it.all().map(str::to_owned).collect::<BTreeSet<_>>();
            if listed(&before) != listed(&after) {
                return Err(Broke::Oracle(format!(
                    "the restart of pid {} read other files back: {:?} before, {:?} after",
                    self.pid,
                    listed(&before),
                    listed(&after)
                )));
            }
        }
        Ok(())
    }

    /// The invariant, after every act: each row of a kept build (every running build and the
    /// last `keep` finished, of every build that ever had rows) is in a listed file; every file
    /// a live pin read is on disk; after a flush with no pin held, no retired file is left.
    fn check(&self, after: &str, flushed: bool) -> Result<(), Broke> {
        let manifest = self.manifest()?;
        let mut listed = BTreeSet::new();
        for name in manifest.all() {
            let file = self.file(name);
            if !self.sim.exists(&file) {
                return Err(Broke::Oracle(format!("after {after}: {name} is listed and not there")));
            }
            let rows = rows_of(self.sim.contents(&file))
                .map_err(|why| Broke::Oracle(format!("after {after}: {name} is torn: {why}")))?;
            listed.extend(rows);
        }
        let known: BTreeSet<u64> = self.rows.values().flatten().copied().collect();
        let finished: Vec<u64> =
            known.iter().rev().filter(|it| !self.running.contains(*it)).take(self.keep).copied().collect();
        for (row, of) in &self.rows {
            let kept = of.iter().find(|it| self.running.contains(*it) || finished.contains(*it));
            if let Some(build) = kept
                && !listed.contains(row)
            {
                return Err(Broke::Oracle(format!(
                    "after {after}: build {build} is kept (running {:?}, the last {} finished \
                     {finished:?}) and lost row {row}, under a cap of {} bytes",
                    self.running, self.keep, self.cap
                )));
            }
        }
        for pin in &self.pins {
            if let Some(name) = pin.read.iter().find(|it| !self.sim.exists(&self.file(it))) {
                return Err(Broke::Oracle(format!(
                    "after {after}: {name} was deleted under a pinned reader"
                )));
            }
        }
        if flushed && self.pins.is_empty() && !manifest.retired().files.is_empty() {
            return Err(Broke::Oracle(format!(
                "after {after}: no reader pins the history and retired files are left: {:?}",
                manifest.retired().files
            )));
        }
        Ok(())
    }
}

/// A case: a seed, a small cap and a small count of kept builds, then acts drawn one at a time.
fn kept_case(tape: &Tape, rule: CapRule) -> Result<(), Broke> {
    let seed = tape.next_u64("sim seed");
    let keep = usize::from(tape.int_in(0..=3u8, "kept builds"));
    // A flush's file is about a kilobyte (its footer mostly), so the cap meets builds within a
    // few flushes, and is often under one build alone.
    let cap = tape.int_in(0..=8_192u64, "cap");
    let mut world = KeptWorld::new(seed, keep, cap, rule)?;
    let acts = tape.int_in(1..=KEPT_ACTS, "acts");
    for at in 0..acts {
        let (after, flushed) = tape.span("act", |tape| -> Result<(String, bool), Broke> {
            Ok(match tape.int_in(0..=9u8, "act") {
                0 | 1 if world.running.len() < 3 => {
                    world.start();
                    (format!("act {at}, a start"), false)
                }
                5 | 6 if !world.running.is_empty() => {
                    let running: Vec<u64> = world.running.iter().copied().collect();
                    let which = usize::from(tape.int_in(0..=2u8, "which build ends")) % running.len();
                    let build = running[which];
                    world.flush(tape.int_in(0..=3u8, "rows"), Some(build))?;
                    (format!("act {at}, the end of build {build}"), true)
                }
                7 => {
                    world.pin()?;
                    (format!("act {at}, a pin"), false)
                }
                8 if !world.pins.is_empty() => {
                    let which = usize::from(tape.int_in(0..=3u8, "which pin goes")) % world.pins.len();
                    world.unpin(which);
                    (format!("act {at}, an unpin"), false)
                }
                9 => {
                    world.restart()?;
                    (format!("act {at}, a restart"), false)
                }
                // A flush: of the running builds' rows, or of no build's when none runs (a
                // timer's, or the one a reader asks for before it reads); 0 rows is an empty one.
                _ => {
                    world.flush(tape.int_in(0..=4u8, "rows"), None)?;
                    (format!("act {at}, a flush"), true)
                }
            })
        })?;
        world.check(&after, flushed)?;
    }
    Ok(())
}

/// The invariant: the history holds, whole, every running build and the last `keep` finished
/// builds, whatever the cap and whatever flushes come after (a flush with no build protects
/// nothing by itself, and moves nothing out); a file a reader pinned stays on disk until the
/// pin goes, and goes at the next flush after; a restart with nothing running reads the same
/// files back.
#[test]
fn the_last_builds_stay_whole_through_any_flushes_pins_and_restarts() {
    blaze_sim::check(Config::cases(200), |tape| kept_case(tape, CapRule::Kept));
}

/// The cap as it was until 2026-11-14 (the newest flush's unit and the running builds kept, no
/// finished build by count): a flush with no build retires the last build, the reproduction's
/// "0 rows". The oracle catches it.
#[test]
fn the_oracle_catches_a_flush_with_no_build_retiring_the_last_build() {
    blaze_sim::control(CONTROL, CapRule::Kept, CapRule::NewestFlushOnly, "is kept", kept_case);
}

/// The cap checked before the kept builds are set aside: past the cap, a kept build that is
/// not running goes too. The oracle catches it.
#[test]
fn the_oracle_catches_the_cap_checked_before_the_kept_builds_are_set_aside() {
    blaze_sim::control(CONTROL, CapRule::Kept, CapRule::CapBeforeKept, "is kept", kept_case);
}
