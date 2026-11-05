//! The history's files core under blaze-core's simulator (`testing` skill, dst-not-review): one
//! Hegel test draws flushes, build ends, crashes at any completion, crashes during the reopen,
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
//! Replay: a control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints
//! its whole plan, the simulator's seed in it.

use std::{fmt, sync::Arc};

use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, cast::AsArray, types::Int64Type};
use arrow_schema::{DataType, Field, Schema};
use blaze_core::{
    Driver,
    sim::{Faults, Rng, Sim},
};
use hegel::{TestCase, generators as gs};
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

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 128;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

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

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.int(0, n - 1) == 0
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for Rng {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let swarm = Swarm {
        fail: d.coin(),
        fill: d.coin(),
        fail_syncs: d.one_in(4),
        contend: d.coin(),
        records: d.coin(),
    };
    let fail_one_in = match swarm.fail {
        true => d.int(20, 200) as u32,
        false => 0,
    };
    let latency_us = d.int(0, 500);
    let n = d.int(1, MOST_ACTS);
    // alloc: a plan, once a case
    let acts = (0..n).map(|_| act(d, &swarm)).collect();
    Plan { seed, fail_one_in, latency_us, acts }
}

fn act(d: &mut impl Draw, swarm: &Swarm) -> Act {
    match d.int(0, 9) {
        0 => Act::Crash(crash_point(d, swarm)),
        _ => Act::Flush {
            rows: d.int(0, 5) as u8,
            end: d.one_in(4),
            fault: fault(d, swarm),
            crash: d.one_in(4).then(|| crash_point(d, swarm)),
        },
    }
}

fn crash_point(d: &mut impl Draw, swarm: &Swarm) -> Crash {
    Crash {
        at: d.int(0, 63) as u32,
        record: swarm.records && d.coin(),
        reopen_at: d.one_in(4).then(|| d.int(0, 63) as u32),
    }
}

fn fault(d: &mut impl Draw, swarm: &Swarm) -> Fault {
    match d.int(0, 5) {
        0 if swarm.fill => Fault::Fill { room: d.int(0, 4096) },
        1 if swarm.fail_syncs => Fault::FailSyncs,
        2 if swarm.contend => {
            Fault::Contend { release_at: d.int(0, 40) as u32, timeout: d.one_in(3) }
        }
        _ => Fault::None,
    }
}

/// The plan seed `seed` makes: what the controls search and [`REPLAY`] replays.
fn plan_of(seed: u64) -> Plan {
    plan(&mut Rng::new(seed))
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

/// What broke a run: the oracle (a bug of the core, or a plant), or the test itself.
enum Broke {
    Oracle(String),
    Harness(String),
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle failed: {why}"),
            Broke::Harness(why) => write!(f, "the test broke: {why}"),
        }
    }
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
#[hegel::test(test_cases = 200)]
fn every_row_is_in_the_history_once_after_any_crash(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nreplay: simulator seed {}, plan {plan:?}", plan.seed);
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself): a green DST with a blind oracle proves nothing.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        match run(&plan_of(seed), plant) {
            Err(Broke::Oracle(why)) => {
                eprintln!(
                    "{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {why}"
                );
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke}"),
            Ok(()) => {}
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_a_rename_with_no_sync_before() {
    control(Plant::NoSyncBeforeRename);
}

#[test]
fn the_oracle_catches_what_a_flush_consumed_marked_after_its_commit() {
    control(Plant::MarkAfterCommit);
}

/// The plan Hegel shrank on 2026-11-04, written out: pid 106's flush of row 5 fails to place
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

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}
