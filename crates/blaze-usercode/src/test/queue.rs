//! One global queue of tests (#1443): a sans-IO state machine over every test binary of a
//! request, running children from every binary on all cores.
//!
//! - **A process per test, from a fork server.** Each binary starts once, in its sandbox
//!   (tracing what it reads), as a fork server ([`super::fork_server`]): blaze writes an empty
//!   record for its sandbox, listens on a Unix socket of its own (`Effect::Listen`), and
//!   spawns the binary with the socket's path in [`wire::SOCKET`] and the record's in
//!   [`confine::LIVE`]. A binary outside the sandbox ([`Binary::sandboxed`]) is spawned with
//!   no sandbox, no record and no [`confine::LIVE`], and what its tests read is not known
//!   ([`OUTSIDE`]). The server connects and says hello with its tests; for each test the
//!   queue sends a run, the server forks a child that marks its scope and runs the one test,
//!   and answers how it ended with its output and what it executed ([`wire`]). The lines the
//!   sandbox appended to the record under the test's scope (and under none) are what the test
//!   read ([`Trace::Lines`]); the record is read after each answer, so every line of the child
//!   is in it.
//! - **Replicas.** One server forks one test at a time, so it starts at most one test per
//!   fork. After the first hello the queue asks for `K - 1` copies ([`Frame::Replicate`],
//!   `K` from the cores, the fork the server measured and the binary's median test:
//!   `replicas`), and each copy connects on the same socket and says its own hello. The
//!   queue keeps a connection per replica and hands each test, in the one order, to the first
//!   replica with room (the cores over the replicas that said hello). A replica that closes its socket or breaks the
//!   protocol loses only the tests it runs, with why; the others carry on, and the server is
//!   killed only when none is left.
//! - **Critical first.** Every test of every binary is in one order, the longest expected
//!   first ([`Job::expected`], from the durations kept with the passes), and the next test
//!   whose server is ready runs whenever a core is free ([`Config::jobs`]).
//! - **Timeouts, retries, fail-fast.** A test past its [`Job::timeout`] is killed through its
//!   server (`Frame::Kill`); a server that does not end it within [`Config::kill_grace`] is
//!   killed itself. A failed test runs again only up to its [`Job::retries`] (non-zero only
//!   for tests named flaky). With [`Config::fail_fast`], the first failure stops new tests;
//!   those not started are [`Outcome::NotRun`].
//! - **libtest's threads.** A binary of a package that opts out ([`Mode::Threads`]) runs as
//!   one spawn with libtest's own arguments and as many threads as it is given cores; its
//!   result is per binary ([`Finished::test`] is `None`), and what it read is the whole
//!   process's ([`Trace::Whole`]).
//! - **Outside after inside.** A binary outside the sandbox starts only once every test of
//!   every sandboxed binary of the queue has its result: what an outside test writes is
//!   written for real, and a sandboxed test's reads are hashed as its result comes
//!   ([`Trace::traced`]), so the two never run at once.
//!
//! The hot path, a test started and ended, allocates nothing past its result: a test is a
//! `u32` unit indexing columns, the order is bitsets over ranks, attempts and tags are slabs
//! reused through free lists, a record's paths and variables are interned once per server and
//! its lines kept as `u32`s, and every deadline is one timer wheel (`blaze_core::wheel`) behind one
//! driver timer at its soonest deadline.
//!
//! The queue asks with tags `tags..tags + 2^32` ([`Config::tags`]). It reads no clock: the
//! driver's `now` comes with each call. It is [`Queue::finished`] once every test has a result
//! and every process it started has ended; timers still in flight then are stale and their
//! completions are dropped.

use std::{borrow::Cow, ffi::OsString, io::Write as _, path::PathBuf, sync::Arc, time::Duration};

use blaze_hash::HashMap;

use blaze_core::{
    Address, Command, Completion, Conn, Done, Effect, Exit, FileName, Sandbox, Tag, Traced,
    wheel::{Handle, Wheel},
};
use blaze_ids::{ContentId, bits::BitSet, radix};
use blaze_inproc::host::{Program, Started};

use super::wire::{self, End, Executed, Frame, Frames};
use crate::{
    confine,
    trace::{self, BadRecord, Inputs, Seen},
};

mod names;

use names::Names;

/// How a binary's tests run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A process per test, from a fork server: the default.
    Process,
    /// libtest's threads in one process, for a package whose manifest asks for it: `args` are
    /// what the user gave libtest, after the names; the whole run is killed past `timeout`.
    Threads { args: Vec<OsString>, timeout: Duration },
}

/// A test to run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Job {
    pub name: String,
    /// It is `#[ignore]`d and was asked for anyway.
    pub ignore: bool,
    /// How long it took when it last ran; zero for a test never run.
    pub expected: Duration,
    pub timeout: Duration,
    /// How many times it runs again after a failure: non-zero only for a test named flaky.
    pub retries: u32,
}

/// A test binary and the tests of it to run.
#[derive(PartialEq, Eq, Debug)]
pub struct Binary {
    /// [`crate::Reached::binary`].
    pub id: ContentId,
    /// For people: `blaze-usercode lib`.
    pub name: String,
    pub program: PathBuf,
    /// The whole environment the tests see; what of it they read is traced.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub network: bool,
    pub memory: Option<u64>,
    pub mode: Mode,
    pub jobs: Vec<Job>,
    /// The binary as blaze holds it in memory: [`Program::command`] writes its executable at
    /// `program`, once per binary, and that file is what is spawned.
    pub loaded: Option<Loaded>,
    /// Whether it runs in a sandbox that traces what each test reads (docs/cargo.md, "Sandboxed
    /// tests"): only where the workspace opts into `sandboxed-tests` or the package says
    /// `tests = true` under `[package.metadata.blaze.sandbox]`, and neither the package says
    /// `tests = false` nor the command `BLAZE_SANDBOX=off`. Outside, it runs as cargo runs it,
    /// with [`Sandbox::Off`]: no record, no [`confine::LIVE`], each test still a process of its
    /// own from the fork server, and every result's trace [`Trace::Unknown`] ([`OUTSIDE`]), so
    /// no pass is kept. `read`, `write` and `network` are the sandbox's, unused outside.
    ///
    /// The tests of a sandboxed package that `unsandboxed-tests` names are a second `Binary`
    /// of the same program (the same `id`, `program` and `loaded`), outside, with those tests
    /// as its `jobs` and the rest in the sandboxed one's: libtest's `--exact` split of one
    /// executable into two runs. Each binary has files and a socket of its own, by its place
    /// in the queue, and the outside one starts once the sandboxed tests have their results.
    /// No binary goes outside but by this field: one sandboxed whose sandbox can't be made
    /// loses its tests, saying why.
    pub sandboxed: bool,
}

/// A binary blaze holds in memory. Two are the same when they are one program.
#[derive(Clone)]
pub struct Loaded(pub Arc<Program>);

impl PartialEq for Loaded {
    fn eq(&self, other: &Loaded) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Loaded {}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Loaded")
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    /// The driver's root, as the processes it starts see it.
    pub root: PathBuf,
    /// The request's directory under the root, where the sockets and records go.
    pub dir: FileName,
    /// How many tests (children, or libtest threads) run at once: the cores.
    pub jobs: usize,
    pub fail_fast: bool,
    pub kill_grace: Duration,
    pub tags: u64,
}

/// Why a test failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    /// It exited with this code: it failed as libtest judges, and its output says why.
    Code(i32),
    /// A signal ended it: an abort, a crash.
    Signal(i32),
    /// It ran past its timeout and was killed.
    TimedOut(Duration),
    /// It could not be run or its result could not be had, and why: the binary could not be
    /// started in its sandbox, its server ended, it has no test of that name.
    Lost(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Passed,
    Failed(Failure),
    /// Not run: an earlier failure stopped the run (fail-fast), or the request was cancelled.
    NotRun,
}

/// What a test read at run time.
#[derive(PartialEq, Eq, Debug)]
pub enum Trace {
    /// The lines of the sandbox's record of its scope and of none, without blaze's own (its
    /// socket, its record, the variables it set for the server).
    Lines(Lines),
    /// What a binary run whole read, from its sandbox.
    Whole(Traced),
    /// Not known, and why: no pass can be kept. [`OUTSIDE`] for every test of a binary outside
    /// the sandbox.
    Unknown(String),
}

/// Why what a test of a binary outside the sandbox read is not known.
pub const OUTSIDE: &str = "it ran outside the sandbox, which alone records what a test reads";

impl Trace {
    /// What was read, hashed as `inputs` has it now: what [`super::select::record`] keeps a
    /// pass by. To be asked as the result comes, before anything else changes the files.
    pub fn traced<I: Inputs>(&self, inputs: &mut I) -> Result<Option<Traced>, I::Error> {
        match self {
            Trace::Lines(lines) => trace::assemble_seen(lines.iter(), inputs).map(Some),
            Trace::Whole(traced) => Ok(Some(traced.clone())), // clone: a binary run whole, once per binary: the caller owns its trace
            Trace::Unknown(_) => Ok(None),
        }
    }
}

/// The lines a test of a fork server read, each its kind and what it names: those of every
/// scope first, each kind and name once, then the test's own. Scopes are left out: every line
/// is of the test or of none, and [`trace::assemble`] reads both. The lines of every scope are
/// one buffer shared by every result of the server until a new one comes ([`Lines::common`]),
/// so a result costs its own lines, not a copy of what every test read.
#[derive(Default, Debug)]
pub struct Lines {
    common: Arc<Run>,
    own: Run,
}

/// Lines in one buffer of names: each line's kind and the end of what it names in `text`.
#[derive(PartialEq, Eq, Default, Debug)]
struct Run {
    text: String,
    lines: Vec<(Seen, u32)>,
}

impl Run {
    /// `false`, and nothing pushed, past 4 GiB of names.
    fn try_push(&mut self, seen: Seen, what: &str) -> bool {
        let Ok(end) = u32::try_from(self.text.len() + what.len()) else { return false };
        self.text.push_str(what);
        self.lines.push((seen, end));
        true
    }

    fn iter(&self) -> impl Iterator<Item = (Seen, &str)> + '_ {
        let mut start = 0;
        self.lines.iter().map(move |&(seen, end)| {
            let what = &self.text[start..end as usize];
            start = end as usize;
            (seen, what)
        })
    }
}

impl Lines {
    pub fn new() -> Lines {
        Lines::default()
    }

    /// A line of `seen` naming `what`, after the rest.
    ///
    /// # Panics
    ///
    /// Past 4 GiB of names.
    pub fn push(&mut self, seen: Seen, what: &str) {
        assert!(self.own.try_push(seen, what), "the lines of a test are past 4 GiB");
    }

    pub fn len(&self) -> usize {
        self.common.lines.len() + self.own.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Each line, borrowed, in order.
    pub fn iter(&self) -> impl Iterator<Item = (Seen, &str)> + '_ {
        self.common.iter().chain(self.own.iter())
    }

    /// Whether `self` and `other` share their lines of every scope, the same buffer: what a
    /// caller hashing many results of one server can hash once.
    pub fn common_is(&self, other: &Lines) -> bool {
        Arc::ptr_eq(&self.common, &other.common)
    }

    /// How many of the lines, from the first, are of every scope.
    pub fn common(&self) -> usize {
        self.common.lines.len()
    }
}

impl PartialEq for Lines {
    fn eq(&self, other: &Lines) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for Lines {}

impl<'a> FromIterator<(Seen, &'a str)> for Lines {
    fn from_iter<T: IntoIterator<Item = (Seen, &'a str)>>(iter: T) -> Lines {
        let mut lines = Lines::new();
        for (seen, what) in iter {
            lines.push(seen, what);
        }
        lines
    }
}

/// A test's result.
#[derive(PartialEq, Eq, Debug)]
pub struct Finished {
    /// Its binary, by its place in the queue's binaries.
    pub binary: usize,
    /// `None` for a binary run whole as libtest's threads.
    pub test: Option<String>,
    pub outcome: Outcome,
    /// How many times it ran: more than one only for a retried test.
    pub attempts: u32,
    /// What it wrote, standard output and error together; each earlier attempt's first.
    pub output: Vec<u8>,
    /// How long its last attempt took, by the driver's clock.
    pub took: Duration,
    pub trace: Trace,
    /// The jump-table slots its last attempt executed, as its fork server read them, or why
    /// they are not known.
    pub executed: Executed,
}

/// The size of each read of a record.
const CHUNK: usize = 1 << 20;
/// The job of a unit that is a binary run whole.
const WHOLE: u32 = u32::MAX;
/// The least a receive from a fork server asks for; more when a longer frame is coming.
const RECV: usize = 1 << 16;
/// A job its server's hello did not name.
const NO_TEST: u32 = u32::MAX;
/// What a binary run whole executed: no fork server, so no region.
const WHOLE_UNRECORDED: &str = "run whole as libtest's threads";
/// Said of every test a stop leaves unstarted.
const STOPPED: &str = "not run: an earlier failure or a cancel stopped the run";
/// The wheel's tick, in nanoseconds.
const TICK: u128 = 1_000_000;

/// What a tag of the queue is for, by its slot in the tag slab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum For {
    /// A slot not in use.
    Free,
    Write(u32),
    Listen(u32),
    Spawn(u32),
    /// A binary's replica, by its place in the server's.
    Recv(u32, u32),
    Send(u32, u32),
    Hangup,
    Read(u32),
    /// The driver timer at the wheel's soonest deadline.
    Tick,
    Whole(u32),
}

/// What a timer of the wheel is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Due {
    /// An attempt past its timeout, by its id.
    Deadline(u64),
    /// An attempt not ended within the kill grace.
    Grace(u64),
    /// A binary run whole past its timeout.
    Whole(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Its record is being made.
    Writing,
    /// Spawned and listened for; not yet said hello.
    Starting,
    /// Said its first hello: its replicas run what they are sent.
    Ready,
    /// Told to quit; its exit is awaited.
    Quitting,
    Ended,
}

/// A test that ended, waiting for the record to be read past its lines.
struct Ended {
    id: u64,
    end: End,
    output: Vec<u8>,
    executed: Executed,
    at: Duration,
}

/// A connection of a binary's fork server: the server, or a copy it forked when asked
/// ([`Frame::Replicate`]), each a process forking tests of its own.
struct Replica {
    conn: Conn,
    /// Its process id, once it said hello.
    pid: Option<u32>,
    /// Closed by either side: nothing more is sent to it.
    hung_up: bool,
    /// Closed, lost or quit: it runs nothing more, and its tests were let go.
    ended: bool,
    /// The tests it runs now.
    running: u32,
    frames: Frames,
    /// Frames for it not yet given to the driver: what one call to the queue says to a replica
    /// goes as one send.
    outbox: Vec<u8>,
    dirty: bool,
}

impl Replica {
    fn new(conn: Conn) -> Replica {
        Replica {
            conn,
            pid: None,
            hung_up: false,
            ended: false,
            running: 0,
            frames: Frames::new(),
            outbox: Vec::new(), // alloc: empty, allocates nothing
            dirty: false,
        }
    }

    /// It said hello and runs what it is sent.
    fn live(&self) -> bool {
        self.pid.is_some() && !self.ended
    }
}

struct Server {
    phase: Phase,
    socket: FileName,
    record: FileName,
    spawn: Option<Tag>,
    /// Its start through [`Program::command`], said to have exited when its spawn completes.
    loaded: Option<Started>,
    /// Its listen, until the queue asks to end it.
    listen: Option<Tag>,
    /// Its connections in the order accepted, by replica.
    replicas: Vec<Replica>,
    /// How many connections it may make: one until its first hello, then the replicas that
    /// hello chose ([`replicas`]).
    want: u32,
    /// Hellos said, the lost replicas' among them: the first sets the tests up and is asked to
    /// replicate.
    hellos: u32,
    /// The first hello's tests, which every replica's must be.
    tests: Vec<String>,
    /// Replicas that said hello and have not ended.
    live: u32,
    /// The most tests a replica runs at once: the cores over the live replicas, rounded up, so
    /// the live replicas together always have room for every core.
    room: u32,
    /// The live replicas running fewer than `room`, by replica.
    open: BitSet<u32>,
    /// Why it is being killed, said to the tests it loses.
    lost: Option<String>,
    /// Each job's index in the server's hello, [`NO_TEST`] for one it did not name: what a
    /// run is sent with, by direct index.
    job_index: Vec<u32>,
    /// The hello's names that are jobs', and the job each is, by its ID: a record line's
    /// scope is one hash of the borrowed text.
    scopes: Names,
    scope_job: Vec<u32>,
    /// What the record's lines name, interned.
    names: Names,
    /// The IDs in `names` of its socket's and its record's paths (as the record names them)
    /// and of the two variables blaze set for it: the lines that are blaze's own.
    ours: [u32; 4],
    /// The tests its replicas run now, by id, in the order they started: those a lost server
    /// or replica takes end in that order.
    running: Vec<u64>,
    /// How far the record has been read, and a last line not yet whole.
    at: u64,
    partial: Vec<u8>,
    /// Each job's lines ([`pack`]), by job, taken by its last attempt's result.
    scoped: Vec<Vec<u32>>,
    /// The lines of every scope ([`pack`]), each once in the order first seen, and the kinds
    /// of [`Seen`] each name has among them as bits, by name: a line every test is given is
    /// kept only once however often the server repeats it.
    common: Vec<u32>,
    common_kinds: Vec<u8>,
    /// `common` as the results have it, made when the first result after a change asks.
    shared: Option<Arc<Run>>,
    reading: bool,
    waiting: Vec<Ended>,
    next: Vec<Ended>,
    /// Why the record can't be read: every test's trace is unknown.
    broken: Option<String>,
}

/// One run of a test, in the attempt slab: its id is its generation and its slot.
#[derive(Clone, Copy)]
struct Attempt {
    unit: u32,
    number: u32,
    generation: u32,
    /// The replica of its binary's server it was sent to.
    replica: u32,
    live: bool,
    /// Sent to its server, and not yet said to have ended.
    running: bool,
    timed_out: bool,
    started: Duration,
    /// Its timers in the wheel, while set.
    deadline: Option<Handle>,
    grace: Option<Handle>,
}

struct Whole {
    spawn: Tag,
    /// Its start through [`Program::command`], said to have exited when its spawn completes.
    loaded: Option<Started>,
    started: Duration,
    slots: usize,
    timed_out: bool,
    deadline: Option<Handle>,
}

struct Bin {
    plan: Binary,
    /// Units without a final result.
    left: usize,
    server: Option<Server>,
    whole: Option<Whole>,
    /// Its units: `first..first + units`.
    first: u32,
    units: u32,
}

/// A set of `u32` positions, with the lowest word that may have one, so the first is found by
/// a scan that only moves forward until a lower bit is set again.
#[derive(Clone, Debug)]
struct Ranks {
    bits: BitSet<u32>,
    low: usize,
}

impl Ranks {
    fn empty(len: usize) -> Ranks {
        let bits = BitSet::<u32>::new(len); // alloc: setup, once per run
        Ranks { low: bits.words().len(), bits }
    }

    fn full(len: usize) -> Ranks {
        Ranks { bits: BitSet::<u32>::filled(len), low: 0 } // alloc: setup, once per run
    }

    fn has(&self, at: u32) -> bool {
        self.bits.contains(at)
    }

    fn set(&mut self, at: u32) {
        self.bits.insert(at);
        self.low = self.low.min(at as usize / 64);
    }

    fn clear(&mut self, at: u32) {
        self.bits.remove(at);
    }

    fn clear_all(&mut self) {
        self.bits.clear();
        self.low = self.bits.words().len();
    }

    fn first(&mut self) -> Option<u32> {
        let words = self.bits.words();
        while let Some(&word) = words.get(self.low) {
            if word != 0 {
                return Some((self.low * 64) as u32 + word.trailing_zeros());
            }
            self.low += 1;
        }
        None
    }

    /// The positions set, in order.
    fn ones(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits.iter()
    }
}

/// How a unit's last attempt went, for its result.
struct Went {
    outcome: Outcome,
    attempts: u32,
    output: Vec<u8>,
    took: Duration,
    trace: Trace,
    executed: Executed,
}

impl Went {
    /// Lost before it ran, and why.
    fn lost(why: String, output: Vec<u8>) -> Went {
        Went {
            outcome: Outcome::Failed(Failure::Lost(why.clone())), // clone: error path
            attempts: 1,
            output,
            took: Duration::ZERO,
            trace: Trace::Unknown(why.clone()), // clone: error path
            executed: Executed::Not(why),
        }
    }
}

pub struct Queue {
    config: Config,
    bins: Vec<Bin>,
    /// Each unit's binary and job ([`WHOLE`] for a binary run whole): a unit is a job of a
    /// binary run as a process per test, or a binary run whole, numbered binary by binary.
    unit_bin: Vec<u32>,
    unit_job: Vec<u32>,
    /// Every unit in the order they start, the longest expected first and then by unit, and
    /// each unit's place in it: `order[rank[unit]] == unit`.
    order: Vec<u32>,
    rank: Vec<u32>,
    /// By rank: not started yet; and of those, what can start now (its server said hello, or
    /// it is a binary run whole).
    pending: Ranks,
    ready: Ranks,
    /// By unit, cold: how many earlier attempts it had (retries), and their output.
    tries: Vec<u32>,
    tries_output: Vec<Vec<u8>>,
    attempts: Vec<Attempt>,
    free_attempts: Vec<u32>,
    /// What each tag is for, by `tag - tags`, and the slots free.
    tags: Vec<For>,
    free_tags: Vec<u32>,
    wheel: Wheel<Due>,
    fired: Vec<Due>,
    /// The driver timer at the wheel's soonest deadline, and the tick it is for.
    armed: Option<(Tag, u64)>,
    /// Replicas with frames in their outbox, by binary and replica.
    dirty: Vec<(u32, u32)>,
    /// Binaries left with nothing to run, to let go.
    quit: Vec<u32>,
    /// The tests of sandboxed binaries without a result, and the binaries outside the sandbox
    /// waiting for them to be none, in order.
    inside: usize,
    held: Vec<u32>,
    busy: usize,
    stopping: bool,
    cancelled: bool,
    now: Duration,
    results: Vec<Finished>,
}

impl Queue {
    /// A queue of `binaries`' jobs; asks to start what it can.
    pub fn new(config: Config, binaries: Vec<Binary>, now: Duration, effects: &mut Vec<Effect>) -> Queue {
        // Every column at the size the run needs, made once: a unit per test (per binary run
        // whole), and per core an attempt, its two timers and its tags.
        let count: usize = binaries
            .iter()
            .map(|plan| match plan.mode {
                Mode::Threads { .. } => usize::from(!plan.jobs.is_empty()),
                Mode::Process => plan.jobs.len(),
            })
            .sum();
        let (cores, bins) = (config.jobs.max(1), binaries.len());
        let mut queue = Queue {
            config,
            bins: Vec::with_capacity(bins),
            unit_bin: Vec::with_capacity(count),
            unit_job: Vec::with_capacity(count),
            order: Vec::with_capacity(count),
            rank: Vec::with_capacity(count),
            pending: Ranks::empty(0),
            ready: Ranks::empty(0),
            tries: Vec::with_capacity(count),
            tries_output: Vec::with_capacity(count),
            attempts: Vec::with_capacity(cores),
            free_attempts: Vec::with_capacity(cores),
            tags: Vec::with_capacity(2 * cores + 4 * bins),
            free_tags: Vec::with_capacity(2 * cores + 4 * bins),
            wheel: {
                let mut wheel = Wheel::new();
                wheel.advance(tick(now), |_, _: Due| {});
                wheel
            },
            fired: Vec::with_capacity(cores),
            armed: None,
            dirty: Vec::with_capacity(bins),
            quit: Vec::with_capacity(bins),
            inside: 0,
            held: Vec::with_capacity(bins),
            busy: 0,
            stopping: false,
            cancelled: false,
            now,
            results: Vec::with_capacity(cores),
        };
        // Sorted on the longest first: the complement of each unit's expected nanoseconds.
        let mut keyed: Vec<(u64, u32)> = Vec::with_capacity(count);
        for (b, plan) in binaries.into_iter().enumerate() {
            let first = queue.unit_bin.len() as u32;
            let b = u32::try_from(b).expect("at most 2^32 binaries");
            match plan.mode {
                Mode::Threads { .. } if !plan.jobs.is_empty() => {
                    let expected: Duration = plan.jobs.iter().map(|it| it.expected).sum();
                    keyed.push((!nanos(expected), queue.unit_bin.len() as u32));
                    queue.unit_bin.push(b);
                    queue.unit_job.push(WHOLE);
                }
                Mode::Threads { .. } => {}
                Mode::Process => {
                    for (j, job) in plan.jobs.iter().enumerate() {
                        keyed.push((!nanos(job.expected), queue.unit_bin.len() as u32));
                        queue.unit_bin.push(b);
                        queue.unit_job.push(j as u32);
                    }
                }
            }
            assert!(queue.unit_bin.len() < WHOLE as usize, "at most 2^32 - 1 tests");
            let units = queue.unit_bin.len() as u32 - first;
            if plan.sandboxed {
                queue.inside += units as usize;
            }
            queue.bins.push(Bin { plan, left: units as usize, server: None, whole: None, first, units });
        }
        radix::sort_pairs(&mut keyed);
        queue.order.extend(keyed.iter().map(|it| it.1));
        queue.rank.resize(count, 0);
        for (at, &unit) in queue.order.iter().enumerate() {
            queue.rank[unit as usize] = at as u32;
        }
        queue.pending = Ranks::full(count);
        queue.ready = Ranks::empty(count);
        queue.tries.resize(count, 0);
        queue.tries_output.resize_with(count, Vec::new);
        for b in 0..queue.bins.len() {
            if queue.bins[b].left == 0 {
                continue;
            }
            match queue.bins[b].plan.mode {
                Mode::Process if !cfg!(unix) => {
                    let why = format!( // alloc: error path
                        "{}: this platform has no fork, so no fork server: its package must run its tests as libtest's threads",
                        queue.bins[b].plan.name
                    );
                    let Bin { first, units, .. } = queue.bins[b];
                    for unit in first..first + units {
                        queue.pending.clear(queue.rank[unit as usize]);
                        queue.finish(unit, Went::lost(why.clone(), Vec::new())); // alloc: error path
                    }
                }
                _ if !queue.bins[b].plan.sandboxed && queue.inside > 0 => queue.held.push(b as u32),
                _ => queue.begin(b, effects),
            }
        }
        queue.schedule(effects);
        queue.flush(effects);
        queue
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.config.tags) < 1 << 32
    }

    /// The results that came since the last call, the queue keeping its buffer of them: what
    /// a driver loop calls.
    pub fn drain(&mut self) -> std::vec::Drain<'_, Finished> {
        self.results.drain(..)
    }

    /// [`Queue::drain`] as a `Vec` of its own.
    pub fn take(&mut self) -> Vec<Finished> {
        std::mem::take(&mut self.results) // alloc: the results' next buffer; a driver loop calls `drain`
    }

    /// The binaries, by the index a [`Finished`] names.
    pub fn binary(&self, at: usize) -> &Binary {
        &self.bins[at].plan
    }

    /// Every test has a result and every process the queue started has ended.
    pub fn finished(&self) -> bool {
        self.bins.iter().all(|bin| {
            bin.left == 0
                && bin.whole.is_none()
                && bin.server.as_ref().is_none_or(|it| it.spawn.is_none() && it.phase != Phase::Writing)
        })
    }

    /// Stops the run: what has not started is not run, and what runs is killed and reported
    /// not run.
    pub fn cancel(&mut self, now: Duration, effects: &mut Vec<Effect>) {
        self.now = now;
        self.cancelled = true;
        self.stop();
        for b in 0..self.bins.len() {
            if let Some(whole) = &self.bins[b].whole {
                effects.push(Effect::Kill { tag: whole.spawn });
            }
            self.kill_server(b, "the run was cancelled".to_owned(), effects); // alloc: error path
        }
        self.let_go(effects);
        self.flush(effects);
    }

    /// Takes a completion of one of its tags.
    pub fn complete(&mut self, completion: Completion, now: Duration, effects: &mut Vec<Effect>) {
        self.completed(completion, now, effects);
        self.flush(effects);
    }

    fn completed(&mut self, completion: Completion, now: Duration, effects: &mut Vec<Effect>) {
        self.now = now;
        let tag = completion.tag;
        let Some(purpose) = self.purpose(tag) else { return };
        if !matches!(purpose, For::Listen(_)) {
            self.release_tag(tag);
        }
        match purpose {
            For::Free => {}
            For::Write(b) => self.written(b as usize, completion.done, effects),
            For::Listen(b) => self.listened(b as usize, tag, completion.done, effects),
            For::Spawn(b) => {
                let b = b as usize;
                let server = self.server(b);
                server.spawn = None;
                // The spawn completes once its sandbox ended every process of it, the
                // children the server forked among them.
                if let Some(started) = server.loaded.take()
                    && let Some(loaded) = &self.bins[b].plan.loaded
                {
                    loaded.0.exited(started);
                }
                let server = self.server(b);
                let (why, output) = match completion.done {
                    Done::Exited(exit) => {
                        let why = server.lost.take().unwrap_or_else(|| ended(&exit));
                        (why, [exit.stdout, exit.stderr].concat())
                    }
                    Done::Failed(error) => (
                        format!("{} could not be started{}: {error}", self.bins[b].plan.program.display(), started_in(&self.bins[b].plan)), // alloc: error path
                        Vec::new(), // alloc: error path
                    ),
                    other => (format!("its spawn completed as {other:?}"), Vec::new()), // alloc: error path
                };
                self.server_ended(b, why, output, effects);
            }
            For::Recv(b, r) => self.received(b as usize, r, completion.done, effects),
            For::Send(b, r) => match completion.done {
                // The buffer sent is the replica's outbox again, its room kept.
                Done::Sent(spare) => {
                    if let Some(server) = self.bins[b as usize].server.as_mut()
                        && let Some(replica) = server.replicas.get_mut(r as usize)
                        && replica.outbox.capacity() == 0
                    {
                        replica.outbox = spare;
                    }
                }
                Done::Broken { error, .. } | Done::Failed(error) => {
                    self.lose(b as usize, r, format!("could not be sent to: {error}"), effects); // alloc: error path
                }
                _ => {}
            },
            For::Hangup => {}
            For::Read(b) => self.read(b as usize, completion.done, effects),
            For::Tick => {
                if self.armed.is_some_and(|it| it.0 == tag) {
                    self.armed = None;
                }
            }
            For::Whole(b) => self.whole_ended(b as usize, completion.done),
        }
        self.expire(effects);
        self.schedule(effects);
    }

    fn tag(&mut self, purpose: For) -> Tag {
        let at = match self.free_tags.pop() {
            Some(at) => {
                self.tags[at as usize] = purpose;
                at
            }
            None => {
                let at = u32::try_from(self.tags.len()).expect("at most 2^32 tags in flight");
                self.tags.push(purpose);
                at
            }
        };
        Tag(self.config.tags.wrapping_add(u64::from(at)))
    }

    fn purpose(&self, tag: Tag) -> Option<For> {
        let at = usize::try_from(tag.0.wrapping_sub(self.config.tags)).ok()?;
        self.tags.get(at).copied().filter(|it| *it != For::Free)
    }

    fn release_tag(&mut self, tag: Tag) {
        let at = tag.0.wrapping_sub(self.config.tags) as u32;
        self.tags[at as usize] = For::Free;
        self.free_tags.push(at);
    }

    fn server(&mut self, b: usize) -> &mut Server {
        self.bins[b].server.as_mut().expect("a binary run as a process per test has a server")
    }

    fn file(&self, name: String) -> FileName {
        FileName::new(&format!("{}/{name}", self.config.dir)).expect("a name under the request's directory") // alloc: setup, once per server
    }

    fn path(&self, file: &FileName) -> PathBuf {
        self.config.root.join(file.as_str())
    }

    /// The slot of the live attempt `id`, if it is one.
    fn attempt(&self, id: u64) -> Option<usize> {
        let at = id as u32 as usize;
        let attempt = self.attempts.get(at)?;
        (attempt.live && attempt.generation == (id >> 32) as u32).then_some(at)
    }

    /// A new attempt of `unit`, by its id.
    fn new_attempt(&mut self, unit: u32, number: u32) -> u64 {
        let at = match self.free_attempts.pop() {
            Some(at) => at,
            None => {
                let at = u32::try_from(self.attempts.len()).expect("at most 2^32 tests at once");
                self.attempts.push(Attempt {
                    unit,
                    number,
                    generation: 0,
                    replica: 0,
                    live: false,
                    running: false,
                    timed_out: false,
                    started: self.now,
                    deadline: None,
                    grace: None,
                });
                at
            }
        };
        let attempt = &mut self.attempts[at as usize];
        *attempt = Attempt {
            unit,
            number,
            generation: attempt.generation,
            replica: 0,
            live: true,
            running: false,
            timed_out: false,
            started: self.now,
            deadline: None,
            grace: None,
        };
        (u64::from(attempt.generation) << 32) | u64::from(at)
    }

    /// The attempt in slot `at` let go, with its timers.
    fn free_attempt(&mut self, at: usize) {
        let attempt = &mut self.attempts[at];
        attempt.live = false;
        attempt.generation = attempt.generation.wrapping_add(1);
        for timer in [attempt.deadline, attempt.grace].into_iter().flatten() {
            self.wheel.cancel(timer);
        }
        self.free_attempts.push(at as u32);
    }

    /// The timers due by now, acted on.
    fn expire(&mut self, effects: &mut Vec<Effect>) {
        let mut fired = std::mem::take(&mut self.fired);
        self.wheel.advance(tick(self.now), |_, due| fired.push(due));
        for due in fired.drain(..) {
            match due {
                Due::Deadline(id) => {
                    let Some(at) = self.attempt(id) else { continue };
                    let attempt = &mut self.attempts[at];
                    attempt.deadline = None;
                    if !attempt.running || attempt.timed_out {
                        continue;
                    }
                    attempt.timed_out = true;
                    let (b, r) = (self.unit_bin[attempt.unit as usize] as usize, attempt.replica);
                    self.send(b, r, &Frame::Kill { id });
                    let grace = self.wheel.insert(due_tick(self.now, self.config.kill_grace), Due::Grace(id));
                    self.attempts[at].grace = Some(grace);
                }
                Due::Grace(id) => {
                    let Some(at) = self.attempt(id) else { continue };
                    self.attempts[at].grace = None;
                    if !self.attempts[at].running {
                        continue;
                    }
                    let unit = self.attempts[at].unit as usize;
                    let (b, j) = (self.unit_bin[unit] as usize, self.unit_job[unit] as usize);
                    let why = format!( // alloc: error path
                        "{} did not end {:?} after it was killed, so its fork server was killed",
                        self.bins[b].plan.jobs[j].name, self.config.kill_grace
                    );
                    self.kill_server(b, why, effects);
                }
                Due::Whole(b) => {
                    if let Some(whole) = &mut self.bins[b as usize].whole
                        && !whole.timed_out
                    {
                        whole.deadline = None;
                        whole.timed_out = true;
                        effects.push(Effect::Kill { tag: whole.spawn });
                    }
                }
            }
        }
        self.fired = fired;
    }

    /// `b`'s tests made able to start: its server started, or its run whole made ready.
    fn begin(&mut self, b: usize, effects: &mut Vec<Effect>) {
        match self.bins[b].plan.mode {
            Mode::Threads { .. } => {
                let rank = self.rank[self.bins[b].first as usize];
                self.ready.set(rank);
            }
            Mode::Process => self.start_server(b, effects),
        }
    }

    /// The binaries outside the sandbox begun, once no sandboxed test is without a result;
    /// those a stop left with nothing to run are let go unstarted.
    fn release(&mut self, effects: &mut Vec<Effect>) {
        if self.inside > 0 || self.held.is_empty() {
            return;
        }
        let mut held = std::mem::take(&mut self.held);
        for &b in &held {
            if self.bins[b as usize].left > 0 && !self.stopping {
                self.begin(b as usize, effects);
            }
        }
        held.clear();
        self.held = held;
    }

    fn start_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let socket = self.file(format!("t{b}")); // alloc: setup, once per server
        let record = self.file(format!("r{b}")); // alloc: setup, once per server
        let socket_path = self.path(&socket).to_string_lossy().into_owned();
        let record_path = self.path(&record).to_string_lossy().into_owned();
        let mut names = Names::new();
        let ours = [socket_path.as_str(), record_path.as_str(), wire::SOCKET, confine::LIVE]
            .map(|it| names.intern(it).expect("an empty table has room for four names"));
        let server = Server {
            phase: Phase::Writing,
            socket,
            record: record.clone(), // clone: setup, once per server
            spawn: None,
            loaded: None,
            listen: None,
            replicas: Vec::new(), // alloc: empty, allocates nothing
            want: 1,
            hellos: 0,
            tests: Vec::new(), // alloc: empty, allocates nothing
            live: 0,
            room: 0,
            open: BitSet::<u32>::new(0),
            lost: None,
            job_index: Vec::new(), // alloc: empty, allocates nothing
            scopes: Names::new(),
            scope_job: Vec::new(), // alloc: empty, allocates nothing
            names,
            ours,
            running: Vec::with_capacity(self.config.jobs),
            at: 0,
            partial: Vec::new(), // alloc: empty, allocates nothing
            scoped: Vec::new(), // alloc: empty, allocates nothing
            common: Vec::new(), // alloc: empty, allocates nothing
            common_kinds: Vec::new(), // alloc: empty, allocates nothing
            shared: None,
            reading: false,
            waiting: Vec::with_capacity(self.config.jobs),
            next: Vec::with_capacity(self.config.jobs),
            broken: None,
        };
        self.bins[b].server = Some(server);
        if !self.bins[b].plan.sandboxed {
            self.spawn_server(b, effects);
            return;
        }
        let tag = self.tag(For::Write(b as u32));
        // Made empty, with its directory, before the sandbox appends to it.
        effects.push(Effect::Write { file: record, bytes: Vec::new(), tag }); // alloc: setup, once per server
    }

    fn written(&mut self, b: usize, done: Done, effects: &mut Vec<Effect>) {
        if let Done::Failed(error) = done {
            let why = format!("making the record of its sandbox, {}: {error}", self.server(b).record); // alloc: setup, once per server: the driver's command owns its parts
            self.server(b).phase = Phase::Ended;
            self.server_ended(b, why, Vec::new(), effects); // alloc: empty, allocates nothing
            return;
        }
        self.spawn_server(b, effects);
    }

    /// The server spawned and listened for: in its sandbox once its record is made, or at once
    /// outside, with no record and no [`confine::LIVE`].
    fn spawn_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        if self.bins[b].left == 0 {
            self.server(b).phase = Phase::Ended;
            return;
        }
        let plan = &self.bins[b].plan;
        let server = self.bins[b].server.as_ref().expect("a server");
        let [socket_path, record_path, ..] = server.ours.map(|it| server.names.get(it));
        let mut env = plan.env.clone(); // clone: setup, once per server: the driver's command owns its parts
        env.push((wire::SOCKET.into(), socket_path.into()));
        let sandbox = match plan.sandboxed {
            true => {
                env.push((confine::LIVE.into(), record_path.into()));
                let mut read = plan.read.clone(); // clone: setup, once per server: the driver's command owns its parts
                read.push(PathBuf::from(socket_path));
                Sandbox::On { read, write: plan.write.clone(), network: plan.network, trace: true, services: Vec::new() } // clone: setup, once per server: the driver's command owns its parts
            }
            false => Sandbox::Off,
        };
        let command = Command {
            program: plan.program.clone(), // clone: setup, once per server: the driver's command owns its parts
            args: Vec::new(), // alloc: empty, allocates nothing
            env,
            dir: plan.dir.clone(), // clone: setup, once per server: the driver's command owns its parts
            stdin: Vec::new(), // alloc: empty, allocates nothing
            sandbox,
            memory: plan.memory,
        };
        let at = Address::Unix(server.socket.clone()); // clone: setup, once per server: the driver's command owns its parts
        let (command, loaded) = match &plan.loaded {
            None => (command, None),
            Some(loaded) => match loaded.0.command(command, true) {
                Ok((command, started)) => (command, Some(started)),
                Err(error) => {
                    let why = format!("{} could not be made ready to start: {error}", plan.program.display()); // alloc: error path
                    self.server(b).phase = Phase::Ended;
                    self.server_ended(b, why, Vec::new(), effects); // alloc: empty, allocates nothing
                    return;
                }
            },
        };
        let listen = self.tag(For::Listen(b as u32));
        let spawn = self.tag(For::Spawn(b as u32));
        let server = self.server(b);
        server.phase = Phase::Starting;
        server.listen = Some(listen);
        server.spawn = Some(spawn);
        server.loaded = loaded;
        effects.push(Effect::Listen { at, tag: listen });
        effects.push(Effect::Spawn { command: Box::new(command), tag: spawn }); // alloc: setup, once per server: the driver's command owns its parts
    }

    fn listened(&mut self, b: usize, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        match done {
            Done::Accepted { conn } => {
                let server = self.server(b);
                let room = matches!(server.phase, Phase::Starting | Phase::Ready | Phase::Quitting)
                    && server.replicas.len() < server.want as usize;
                if !room {
                    let hangup = self.tag(For::Hangup);
                    effects.push(Effect::Hangup { conn, tag: hangup });
                    return;
                }
                let r = server.replicas.len() as u32;
                server.replicas.push(Replica::new(conn));
                let recv = self.tag(For::Recv(b as u32, r));
                effects.push(Effect::Recv { conn, most: RECV, into: Vec::new(), tag: recv }); // alloc: none, empty: the frames keep each receive's bytes (`push_owned`)
                self.unlisten_when_whole(b, effects);
            }
            Done::Unlistened => self.release_tag(tag),
            Done::Failed(error) => {
                self.release_tag(tag);
                self.server(b).listen = None;
                if self.server(b).replicas.is_empty() {
                    let socket = self.server(b).socket.clone(); // clone: error path
                    self.kill_server(b, format!("listening on {socket}: {error}"), effects); // alloc: error path
                }
            }
            _ => {}
        }
    }

    /// The listen ended once every connection the server may make was accepted.
    fn unlisten_when_whole(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let server = self.server(b);
        if server.hellos > 0 && server.replicas.len() >= server.want as usize {
            self.unlisten(b, effects);
        }
    }

    fn unlisten(&mut self, b: usize, effects: &mut Vec<Effect>) {
        if let Some(listen) = self.server(b).listen.take() {
            effects.push(Effect::Unlisten { tag: listen });
        }
    }

    fn replica(&mut self, b: usize, r: u32) -> &mut Replica {
        &mut self.server(b).replicas[r as usize]
    }

    fn received(&mut self, b: usize, r: u32, done: Done, effects: &mut Vec<Effect>) {
        let bytes = match done {
            Done::Read(bytes) => bytes,
            Done::Broken { error, .. } | Done::Failed(error) => {
                self.lose(b, r, format!("could not be read from: {error}"), effects); // alloc: error path
                return;
            }
            _ => return,
        };
        if bytes.is_empty() {
            self.replica(b, r).hung_up = true;
            self.lose(b, r, "closed its socket".to_owned(), effects); // alloc: error path
            return;
        }
        self.replica(b, r).frames.push_owned(bytes);
        loop {
            let frame = match self.replica(b, r).frames.pop() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(why) => {
                    self.lose(b, r, format!("broke the protocol: {why}"), effects); // alloc: error path
                    return;
                }
            };
            if let Err(why) = self.frame(b, r, frame, effects) {
                self.lose(b, r, format!("broke the protocol: {why}"), effects); // alloc: error path
                return;
            }
        }
        let phase = self.server(b).phase;
        let replica = self.replica(b, r);
        if !replica.hung_up && phase != Phase::Ended {
            let (conn, most) = (replica.conn, replica.frames.needed().max(RECV));
            let recv = self.tag(For::Recv(b as u32, r));
            effects.push(Effect::Recv { conn, most, into: Vec::new(), tag: recv }); // alloc: none, empty: the frames keep each receive's bytes (`push_owned`)
        }
    }

    /// Replica `r` of `b`'s server closed, failed or broke the protocol (`what`): it is hung up
    /// and the tests it runs are lost with why, the others carrying on. With no replica left
    /// the server is killed. Nothing is lost of a server told to quit, which runs nothing.
    fn lose(&mut self, b: usize, r: u32, what: String, effects: &mut Vec<Effect>) {
        let server = self.server(b);
        let phase = server.phase;
        let replica = &mut server.replicas[r as usize];
        if replica.ended {
            return;
        }
        let was_live = replica.live();
        replica.ended = true;
        replica.outbox.clear();
        let (conn, hang_up, pid) = (replica.conn, !replica.hung_up, replica.pid);
        replica.hung_up = true;
        if was_live {
            server.live -= 1;
        }
        if hang_up {
            let tag = self.tag(For::Hangup);
            effects.push(Effect::Hangup { conn, tag });
        }
        if matches!(phase, Phase::Quitting | Phase::Ended) {
            return;
        }
        let who = match pid {
            Some(pid) => format!("replica {r} of its fork server (pid {pid}) {what}"), // alloc: error path
            None => format!("connection {r} of its fork server, before its hello, {what}"), // alloc: error path
        };
        let why = format!("{}: {who}", self.bins[b].plan.name); // alloc: error path
        let mut running = std::mem::take(&mut self.server(b).running);
        let mut at = 0;
        while at < running.len() {
            let id = running[at];
            match self.attempt(id) {
                Some(slot) if self.attempts[slot].replica == r => {
                    running.remove(at);
                    self.busy -= 1;
                    self.lose_attempt(slot, &why, &[]);
                }
                _ => at += 1,
            }
        }
        self.server(b).running = running;
        if self.server(b).live == 0 {
            // Its tests wait for a replica that says hello, and are lost if none does.
            for rank in self.pending_of(b) {
                self.ready.clear(rank);
            }
        }
        if self.server(b).replicas.iter().all(|it| it.ended) {
            self.kill_server(b, who, effects);
            return;
        }
        self.refresh(b);
    }

    /// The room of each replica, and which have it, after the live ones changed.
    fn refresh(&mut self, b: usize) {
        let cores = u32::try_from(self.config.jobs.max(1)).unwrap_or(u32::MAX);
        let server = self.server(b);
        server.room = match server.live {
            0 => 0,
            live => cores.div_ceil(live),
        };
        for (r, replica) in server.replicas.iter().enumerate() {
            server.open.set_growing(r as u32, replica.live() && replica.running < server.room);
        }
    }

    /// The first replica of `b`'s server with room for a test.
    fn first_open(&self, b: usize) -> Option<u32> {
        self.bins[b].server.as_ref()?.open.first()
    }

    fn frame(&mut self, b: usize, r: u32, frame: Frame, effects: &mut Vec<Effect>) -> Result<(), String> {
        match frame {
            Frame::Hello { version, pid, fork, tests } => {
                if self.replica(b, r).pid.is_some() {
                    return Err("a second hello".to_owned()); // alloc: error path
                }
                if version != wire::VERSION {
                    return Err(format!("its version is {version}, and blaze speaks {}", wire::VERSION)); // alloc: error path
                }
                if self.server(b).hellos == 0 {
                    self.first_hello(b, r, fork, tests)?;
                } else if tests != self.server(b).tests {
                    return Err("its tests are not those of the first hello".to_owned()); // alloc: error path
                }
                let server = self.server(b);
                server.hellos += 1;
                server.replicas[r as usize].pid = Some(pid);
                let phase = server.phase;
                match phase {
                    Phase::Ready => {
                        server.live += 1;
                        let first = server.live == 1;
                        if first {
                            self.ready_all(b);
                        }
                        self.refresh(b);
                    }
                    // Nothing was left for it by the time it came.
                    Phase::Quitting => {
                        self.replica(b, r).ended = true;
                        self.send(b, r, &Frame::Quit);
                    }
                    Phase::Writing | Phase::Starting | Phase::Ended => {}
                }
                self.unlisten_when_whole(b, effects);
                self.maybe_quit(b, effects);
                Ok(())
            }
            Frame::Ended { id, end, output, executed } => {
                let at = self
                    .attempt(id)
                    .filter(|&at| {
                        let attempt = &self.attempts[at];
                        attempt.running && attempt.replica == r && self.unit_bin[attempt.unit as usize] as usize == b
                    })
                    .ok_or_else(|| format!("an end of {id}, which it does not run"))?; // alloc: error path
                let server = self.server(b);
                if let Some(place) = server.running.iter().position(|it| *it == id) {
                    server.running.remove(place);
                }
                let replica = &mut server.replicas[r as usize];
                replica.running -= 1;
                if replica.live() && replica.running < server.room {
                    server.open.insert(r);
                }
                self.busy -= 1;
                let attempt = &mut self.attempts[at];
                attempt.running = false;
                for timer in [attempt.deadline.take(), attempt.grace.take()].into_iter().flatten() {
                    self.wheel.cancel(timer);
                }
                let Attempt { unit, number, .. } = self.attempts[at];
                let outcome = self.outcome(at, &end);
                if outcome != Outcome::Passed {
                    // Decided before a core is given to another test: a retry keeps its place in
                    // the order, and fail-fast starts nothing more.
                    let j = self.unit_job[unit as usize] as usize;
                    if number <= self.bins[b].plan.jobs[j].retries && !self.stopping {
                        self.free_attempt(at);
                        self.tries[unit as usize] = number;
                        let tries = &mut self.tries_output[unit as usize];
                        _ = writeln!(tries, "--- attempt {number} ---");
                        tries.extend_from_slice(&output);
                        let rank = self.rank[unit as usize];
                        self.pending.set(rank);
                        if self.accepting(b) {
                            self.ready.set(rank);
                        }
                        return Ok(());
                    }
                    if self.config.fail_fast {
                        self.stop();
                    }
                }
                let ended = Ended { id, end, output, executed, at: self.now };
                if !self.bins[b].plan.sandboxed {
                    self.attempt_ended(b, ended, Trace::Unknown(OUTSIDE.to_owned()), effects); // alloc: once per test outside: its result owns why its reads are not known
                    return Ok(());
                }
                let server = self.server(b);
                match server.reading {
                    true => server.next.push(ended),
                    false => {
                        server.waiting.push(ended);
                        self.start_read(b, effects);
                    }
                }
                Ok(())
            }
            other => Err(format!("it sent {other:?}, which only blaze sends")), // alloc: error path
        }
    }

    /// Whether `b`'s tests can be sent now: its server said hello and a replica of it is live.
    fn accepting(&self, b: usize) -> bool {
        self.bins[b].server.as_ref().is_some_and(|it| it.phase == Phase::Ready && it.live > 0)
    }

    /// The tests the server's first hello named, set up for every replica: each job's index
    /// and scope, the replicas to make (asked of the replica that said it, before anything
    /// else is sent to it), and the server ready.
    fn first_hello(&mut self, b: usize, r: u32, fork: u64, tests: Vec<String>) -> Result<(), String> {
        let cores = self.config.jobs;
        let bin = &mut self.bins[b];
        let jobs: HashMap<&str, u32> =
            bin.plan.jobs.iter().enumerate().map(|(j, it)| (it.name.as_str(), j as u32)).collect(); // alloc: setup, once per server: the hello
        let mut expected: Vec<u64> = Vec::with_capacity(bin.plan.jobs.len()); // alloc: setup, once per server: the hello
        expected.extend(bin.plan.jobs.iter().map(|it| nanos(it.expected)));
        let server = bin.server.as_mut().expect("a server");
        server.job_index = vec![NO_TEST; bin.plan.jobs.len()]; // alloc: setup, once per server: the hello
        server.scoped = vec![Vec::new(); bin.plan.jobs.len()]; // alloc: setup, once per server: the hello
        for (at, name) in tests.iter().enumerate() {
            let Some(&j) = jobs.get(name.as_str()) else { continue };
            server.job_index[j as usize] = at as u32;
            let id = server.scopes.intern(name).ok_or("it named more tests than blaze holds")?;
            if id as usize == server.scope_job.len() {
                server.scope_job.push(j);
            }
        }
        let want = replicas(cores, fork, median(&mut expected), bin.units as usize);
        server.want = want;
        server.open = BitSet::<u32>::new(want as usize); // alloc: setup, once per server: the hello
        server.tests = tests;
        server.phase = Phase::Ready;
        if want > 1 {
            self.send(b, r, &Frame::Replicate { count: want - 1 });
        }
        Ok(())
    }

    /// Every test of `b` not started made ready, now that a replica of its server is live; a
    /// test its hello did not name is lost.
    fn ready_all(&mut self, b: usize) {
        for rank in self.pending_of(b) {
            let unit = self.order[rank as usize];
            let j = self.unit_job[unit as usize] as usize;
            if self.server(b).job_index[j] != NO_TEST {
                self.ready.set(rank);
                continue;
            }
            self.pending.clear(rank);
            let why = format!("{} has no test named {}", self.bins[b].plan.name, self.bins[b].plan.jobs[j].name); // alloc: error path
            self.finish(unit, Went::lost(why, Vec::new())); // alloc: empty, allocates nothing
        }
    }

    fn start_read(&mut self, b: usize, effects: &mut Vec<Effect>) {
        if let Some(why) = self.server(b).broken.clone() { // clone: error path
            let mut waiting = std::mem::take(&mut self.server(b).waiting);
            for ended in waiting.drain(..) {
                self.attempt_ended(b, ended, Trace::Unknown(why.clone()), effects); // clone: error path
            }
            self.server(b).waiting = waiting;
            return;
        }
        let tag = self.tag(For::Read(b as u32));
        let server = self.server(b);
        server.reading = true;
        effects.push(Effect::Read { file: server.record.clone(), at: server.at, len: CHUNK, tag }); // clone: the edge: the driver's read owns its file name
    }

    fn read(&mut self, b: usize, done: Done, effects: &mut Vec<Effect>) {
        let server = self.server(b);
        server.reading = false;
        match done {
            Done::Read(bytes) => {
                let len = bytes.len();
                server.at += len as u64;
                match server.partial.is_empty() {
                    true => server.partial = bytes,
                    false => server.partial.extend_from_slice(&bytes),
                }
                if let Some(end) = server.partial.iter().rposition(|it| *it == b'\n') {
                    let partial = std::mem::take(&mut server.partial);
                    if let Err(why) = take_lines(server, &partial[..=end]) {
                        server.broken = Some(format!("the sandbox's record {}: {why}", server.record)); // alloc: error path
                    }
                    server.partial = partial;
                    server.partial.drain(..=end);
                }
                if len == CHUNK && server.broken.is_none() {
                    self.start_read(b, effects);
                    return;
                }
            }
            Done::Failed(error) => {
                server.broken = Some(format!("reading the sandbox's record {}: {error}", server.record)); // alloc: error path
            }
            _ => {}
        }
        let broken = self.server(b).broken.clone(); // clone: error path
        let mut waiting = std::mem::take(&mut self.server(b).waiting);
        for ended in waiting.drain(..) {
            let Some(at) = self.attempt(ended.id) else { continue };
            let trace = match &broken {
                Some(why) => Trace::Unknown(why.clone()), // clone: error path
                None => self.lines(b, self.attempts[at].unit),
            };
            self.attempt_ended(b, ended, trace, effects);
        }
        let server = self.server(b);
        server.waiting = waiting;
        if !server.next.is_empty() {
            std::mem::swap(&mut server.waiting, &mut server.next);
            self.start_read(b, effects);
        }
    }

    /// The lines of the record a unit of `b` read: those of every scope (shared with the
    /// server's other results), then its own, which are let go (only its last attempt's result
    /// asks).
    fn lines(&mut self, b: usize, unit: u32) -> Trace {
        const PAST: &str = "the lines the test read are past 4 GiB";
        let j = self.unit_job[unit as usize] as usize;
        let server = self.server(b);
        let common = match server.shared.as_ref().map(Arc::clone) {
            Some(common) => common,
            None => {
                let Some(common) = run(&server.names, &server.common) else { return Trace::Unknown(PAST.to_owned()) }; // alloc: error path
                let common = Arc::new(common); // alloc: once per new line of every scope, not per test
                server.shared = Some(Arc::clone(&common));
                common
            }
        };
        let own = server.scoped.get_mut(j).map(std::mem::take).unwrap_or_default();
        let Some(own) = run(&server.names, &own) else { return Trace::Unknown(PAST.to_owned()) }; // alloc: error path
        Trace::Lines(Lines { common, own })
    }

    /// How a run, in attempt slot `at`, of a test that ended as `end` went.
    fn outcome(&self, at: usize, end: &End) -> Outcome {
        let attempt = &self.attempts[at];
        let unit = attempt.unit as usize;
        let timeout = self.bins[self.unit_bin[unit] as usize].plan.jobs[self.unit_job[unit] as usize].timeout;
        match (end, attempt.timed_out) {
            (_, true) => Outcome::Failed(Failure::TimedOut(timeout)),
            (End::Exited(0), false) => Outcome::Passed,
            (End::Exited(code), false) => Outcome::Failed(Failure::Code(*code)),
            (End::Signaled(signal), false) => Outcome::Failed(Failure::Signal(*signal)),
            (End::NotStarted(why), false) => Outcome::Failed(Failure::Lost(why.clone())), // clone: error path
        }
    }

    /// A test whose run ended and is not run again, once what it read is known.
    fn attempt_ended(&mut self, b: usize, ended: Ended, trace: Trace, effects: &mut Vec<Effect>) {
        let Some(at) = self.attempt(ended.id) else { return };
        let Attempt { unit, number, started, .. } = self.attempts[at];
        let outcome = self.outcome(at, &ended.end);
        self.free_attempt(at);
        let took = ended.at.saturating_sub(started);
        let output = match self.tries[unit as usize] {
            0 => ended.output,
            _ => {
                self.tries[unit as usize] = 0;
                let mut output = std::mem::take(&mut self.tries_output[unit as usize]);
                _ = writeln!(output, "--- attempt {number} ---");
                output.extend_from_slice(&ended.output);
                output
            }
        };
        let outcome = if self.cancelled && outcome != Outcome::Passed { Outcome::NotRun } else { outcome };
        self.finish(unit, Went { outcome, attempts: number, output, took, trace, executed: ended.executed });
        self.maybe_quit(b, effects);
    }

    fn finish(&mut self, unit: u32, went: Went) {
        let (b, j) = (self.unit_bin[unit as usize] as usize, self.unit_job[unit as usize]);
        let failed = matches!(went.outcome, Outcome::Failed(_));
        let attempts = match went.outcome {
            Outcome::NotRun => went.attempts.saturating_sub(1),
            _ => went.attempts,
        };
        self.results.push(Finished {
            binary: b,
            test: (j != WHOLE).then(|| self.bins[b].plan.jobs[j as usize].name.clone()), // clone: the edge: a result owns its test's name
            outcome: went.outcome,
            attempts,
            output: went.output,
            took: went.took,
            trace: went.trace,
            executed: went.executed,
        });
        self.bins[b].left -= 1;
        if self.bins[b].plan.sandboxed {
            self.inside -= 1;
        }
        if self.bins[b].left == 0 {
            self.quit.push(b as u32);
        }
        if failed && self.config.fail_fast {
            self.stop();
        }
    }

    /// No test starts from now on: those not started are not run.
    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.ready.clear_all();
        let pending = std::mem::replace(&mut self.pending, Ranks::empty(self.order.len()));
        for rank in pending.ones() {
            let unit = self.order[rank as usize];
            let done = std::mem::take(&mut self.tries[unit as usize]);
            let output = std::mem::take(&mut self.tries_output[unit as usize]);
            self.finish(
                unit,
                Went {
                    outcome: Outcome::NotRun,
                    attempts: done + 1,
                    output,
                    took: Duration::ZERO,
                    trace: Trace::Unknown(STOPPED.to_owned()), // alloc: error path
                    executed: Executed::Not(STOPPED.to_owned()), // alloc: error path
                },
            );
        }
    }

    /// A server with nothing left to run is told to quit; one not yet ready is killed.
    fn maybe_quit(&mut self, b: usize, effects: &mut Vec<Effect>) {
        if self.bins[b].left != 0 {
            return;
        }
        let Some(server) = &self.bins[b].server else { return };
        match server.phase {
            // A replica that has not said hello yet is told on its hello.
            Phase::Ready => {
                for r in 0..server.replicas.len() as u32 {
                    if self.replica(b, r).live() {
                        self.send(b, r, &Frame::Quit);
                    }
                }
                self.server(b).phase = Phase::Quitting;
            }
            Phase::Starting if server.lost.is_none() => {
                if let Some(spawn) = server.spawn {
                    effects.push(Effect::Kill { tag: spawn });
                    self.server(b).lost = Some("nothing was left for it to run".to_owned()); // alloc: once per server, at its end
                }
            }
            Phase::Starting => {}
            Phase::Writing | Phase::Quitting | Phase::Ended => {}
        }
    }

    /// The binaries left with nothing to run, let go.
    fn let_go(&mut self, effects: &mut Vec<Effect>) {
        let mut quit = std::mem::take(&mut self.quit);
        for &b in &quit {
            self.maybe_quit(b as usize, effects);
        }
        quit.clear();
        quit.append(&mut self.quit);
        std::mem::swap(&mut self.quit, &mut quit);
    }

    /// `frame` to replica `r` of the server, in its outbox until the call ends
    /// ([`Queue::flush`]).
    fn send(&mut self, b: usize, r: u32, frame: &Frame) {
        let replica = self.replica(b, r);
        if replica.hung_up {
            return;
        }
        frame.encode(&mut replica.outbox);
        if !replica.dirty {
            replica.dirty = true;
            self.dirty.push((b as u32, r));
        }
    }

    /// What this call said to each replica, as one send of all its frames; then the driver
    /// timer moved earlier if the wheel's soonest deadline is.
    fn flush(&mut self, effects: &mut Vec<Effect>) {
        let mut dirty = std::mem::take(&mut self.dirty);
        for &(b, r) in &dirty {
            self.flush_replica(b as usize, r, effects);
        }
        dirty.clear();
        self.dirty = dirty;
        let Some(next) = self.wheel.next_deadline() else { return };
        if self.armed.is_some_and(|it| it.1 <= next) {
            return;
        }
        let tag = self.tag(For::Tick);
        let after = Duration::from_millis(next).saturating_sub(self.now);
        effects.push(Effect::Timer { after, tag });
        self.armed = Some((tag, next));
    }

    fn flush_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let Some(server) = self.bins[b].server.as_ref() else { return };
        for r in 0..server.replicas.len() as u32 {
            self.flush_replica(b, r, effects);
        }
    }

    fn flush_replica(&mut self, b: usize, r: u32, effects: &mut Vec<Effect>) {
        let Some(server) = self.bins[b].server.as_mut() else { return };
        let Some(replica) = server.replicas.get_mut(r as usize) else { return };
        replica.dirty = false;
        if replica.hung_up || replica.outbox.is_empty() {
            replica.outbox.clear();
            return;
        }
        let (conn, bytes) = (replica.conn, std::mem::take(&mut replica.outbox));
        let tag = self.tag(For::Send(b as u32, r));
        effects.push(Effect::Send { conn, bytes, tag });
    }

    /// Kills the server, so its tests are lost with `why` once its spawn ends.
    fn kill_server(&mut self, b: usize, why: String, effects: &mut Vec<Effect>) {
        let Some(server) = &mut self.bins[b].server else { return };
        if server.phase == Phase::Ended {
            return;
        }
        server.lost.get_or_insert(why.clone()); // clone: error path
        match server.spawn {
            Some(spawn) => effects.push(Effect::Kill { tag: spawn }),
            // Its record is still being made: nothing was started.
            None => {
                server.phase = Phase::Ended;
                self.server_ended(b, why, Vec::new(), effects); // alloc: error path
            }
        }
    }

    /// The server's process ended (or never started): what it ran and what was left for it
    /// are lost with `why`.
    fn server_ended(&mut self, b: usize, why: String, output: Vec<u8>, effects: &mut Vec<Effect>) {
        // What was said to it goes before the hang-up, as it was said.
        self.flush_server(b, effects);
        let server = self.server(b);
        let was = server.phase;
        server.phase = Phase::Ended;
        for r in 0..server.replicas.len() as u32 {
            let replica = self.replica(b, r);
            replica.ended = true;
            if !replica.hung_up {
                replica.hung_up = true;
                let conn = replica.conn;
                let tag = self.tag(For::Hangup);
                effects.push(Effect::Hangup { conn, tag });
            }
        }
        self.server(b).live = 0;
        self.unlisten(b, effects);
        let running = std::mem::take(&mut self.server(b).running);
        let why = match was {
            Phase::Quitting if running.is_empty() => return,
            _ => format!("{}: {why}", self.bins[b].plan.name), // alloc: error path
        };
        for id in running {
            self.busy -= 1;
            let Some(at) = self.attempt(id) else { continue };
            self.lose_attempt(at, &why, &output);
        }
        for rank in self.pending_of(b) {
            let unit = self.order[rank as usize];
            self.pending.clear(rank);
            self.ready.clear(rank);
            self.tries[unit as usize] = 0;
            let earlier = std::mem::take(&mut self.tries_output[unit as usize]);
            self.finish(unit, Went::lost(why.clone(), [earlier, output.clone()].concat())); // alloc: error path
        }
    }

    /// The running attempt in slot `at` lost with `why`, `output` after its earlier attempts'.
    fn lose_attempt(&mut self, at: usize, why: &str, output: &[u8]) {
        let Attempt { unit, number, started, .. } = self.attempts[at];
        self.free_attempt(at);
        let outcome = match self.cancelled {
            true => Outcome::NotRun,
            false => Outcome::Failed(Failure::Lost(why.to_owned())), // alloc: error path
        };
        self.tries[unit as usize] = 0;
        let mut all = std::mem::take(&mut self.tries_output[unit as usize]);
        all.extend_from_slice(output);
        let took = self.now.saturating_sub(started);
        self.finish(
            unit,
            Went {
                outcome,
                attempts: number,
                output: all,
                took,
                trace: Trace::Unknown(why.to_owned()), // alloc: error path
                executed: Executed::Not(why.to_owned()), // alloc: error path
            },
        );
    }

    /// The ranks of `b`'s units not started, in order.
    fn pending_of(&self, b: usize) -> Vec<u32> {
        let Bin { first, units, .. } = self.bins[b];
        let mut ranks: Vec<u32> =
            (first..first + units).map(|unit| self.rank[unit as usize]).filter(|&rank| self.pending.has(rank)).collect(); // alloc: error path
        radix::sort(&mut ranks);
        ranks
    }

    /// Starts the first tests in the order whose servers are ready, while cores are free.
    /// Servers with nothing left to run are let go.
    fn schedule(&mut self, effects: &mut Vec<Effect>) {
        self.release(effects);
        while !self.stopping && self.busy < self.config.jobs.max(1) {
            let Some(rank) = self.ready.first() else { break };
            self.ready.clear(rank);
            self.pending.clear(rank);
            let unit = self.order[rank as usize];
            match self.unit_job[unit as usize] {
                WHOLE => self.start_whole(self.unit_bin[unit as usize] as usize, effects),
                _ => self.start_job(unit),
            }
        }
        self.let_go(effects);
    }

    fn start_job(&mut self, unit: u32) {
        let (b, j) = (self.unit_bin[unit as usize] as usize, self.unit_job[unit as usize] as usize);
        let number = self.tries[unit as usize] + 1;
        let timeout = self.bins[b].plan.jobs[j].timeout;
        // A test is ready only while a replica is live, and the live replicas have room for
        // every core together, so with a core free one of them has room.
        let r = self.first_open(b).expect("a live replica with room while a core is free");
        let server = self.server(b);
        let index = server.job_index[j];
        let room = server.room;
        let replica = &mut server.replicas[r as usize];
        replica.running += 1;
        if replica.running >= room {
            server.open.remove(r);
        }
        let id = self.new_attempt(unit, number);
        self.server(b).running.push(id);
        self.busy += 1;
        self.send(b, r, &Frame::Run { id, index });
        let deadline = self.wheel.insert(due_tick(self.now, timeout), Due::Deadline(id));
        let attempt = &mut self.attempts[id as u32 as usize];
        attempt.replica = r;
        attempt.running = true;
        attempt.deadline = Some(deadline);
    }

    fn start_whole(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let plan = &self.bins[b].plan;
        let Mode::Threads { args: given, timeout } = &plan.mode else { unreachable!("a binary run whole is in threads mode") };
        let slots = self.config.jobs.max(1).saturating_sub(self.busy).min(plan.jobs.len()).max(1);
        let mut args: Vec<OsString> = plan.jobs.iter().map(|it| OsString::from(&it.name)).collect(); // alloc: setup, once per binary run whole: the driver's command owns its parts
        args.push("--exact".into());
        args.push(format!("--test-threads={slots}").into()); // alloc: setup, once per binary run whole: the driver's command owns its parts
        if plan.jobs.iter().any(|it| it.ignore) {
            args.push("--include-ignored".into());
        }
        args.extend(given.iter().cloned());
        let sandbox = match plan.sandboxed {
            true => Sandbox::On { read: plan.read.clone(), write: plan.write.clone(), network: plan.network, trace: true, services: Vec::new() }, // clone: setup, once per binary run whole: the driver's command owns its parts
            false => Sandbox::Off,
        };
        let command = Command {
            program: plan.program.clone(), // clone: setup, once per binary run whole: the driver's command owns its parts
            args,
            env: plan.env.clone(), // clone: setup, once per binary run whole: the driver's command owns its parts
            dir: plan.dir.clone(), // clone: setup, once per binary run whole: the driver's command owns its parts
            stdin: Vec::new(), // alloc: empty, allocates nothing
            sandbox,
            memory: plan.memory,
        };
        let timeout = *timeout;
        let (command, loaded) = match &plan.loaded {
            None => (command, None),
            Some(loaded) => match loaded.0.command(command, false) {
                Ok((command, started)) => (command, Some(started)),
                Err(error) => {
                    let why = format!("{} could not be made ready to start: {error}", plan.program.display()); // alloc: once per binary run whole
                    let unit = self.bins[b].first;
                    self.finish(
                        unit,
                        Went {
                            outcome: Outcome::Failed(Failure::Lost(why.clone())), // clone: error path
                            attempts: 1,
                            output: Vec::new(), // alloc: empty, allocates nothing
                            took: Duration::ZERO,
                            trace: Trace::Unknown(why),
                            executed: Executed::Not(WHOLE_UNRECORDED.to_owned()), // alloc: once per binary run whole
                        },
                    );
                    return;
                }
            },
        };
        let spawn = self.tag(For::Whole(b as u32));
        let deadline = self.wheel.insert(due_tick(self.now, timeout), Due::Whole(b as u32));
        self.busy += slots;
        self.bins[b].whole = Some(Whole { spawn, loaded, started: self.now, slots, timed_out: false, deadline: Some(deadline) });
        effects.push(Effect::Spawn { command: Box::new(command), tag: spawn }); // alloc: setup, once per binary run whole: the driver's command owns its parts
    }

    fn whole_ended(&mut self, b: usize, done: Done) {
        let Some(whole) = self.bins[b].whole.take() else { return };
        if let Some(started) = whole.loaded
            && let Some(loaded) = &self.bins[b].plan.loaded
        {
            loaded.0.exited(started);
        }
        if let Some(deadline) = whole.deadline {
            self.wheel.cancel(deadline);
        }
        self.busy -= whole.slots;
        let took = self.now.saturating_sub(whole.started);
        let timeout = match &self.bins[b].plan.mode {
            Mode::Threads { timeout, .. } => *timeout,
            Mode::Process => Duration::ZERO,
        };
        let (outcome, output, trace) = match done {
            Done::Exited(exit) => {
                let exit = *exit;
                let outcome = match (whole.timed_out, exit.code, exit.signal) {
                    (true, _, _) => Outcome::Failed(Failure::TimedOut(timeout)),
                    (false, Some(0), _) => Outcome::Passed,
                    (false, Some(code), _) => Outcome::Failed(Failure::Code(code)),
                    (false, None, signal) => Outcome::Failed(Failure::Signal(signal.unwrap_or(0))),
                };
                let trace = match (self.bins[b].plan.sandboxed, exit.traced) {
                    (false, _) => Trace::Unknown(OUTSIDE.to_owned()), // alloc: once per binary run whole
                    (true, Some(traced)) => Trace::Whole(traced),
                    (true, None) => Trace::Unknown("its sandbox kept no record of what it read".to_owned()), // alloc: once per binary run whole
                };
                (outcome, [exit.stdout, exit.stderr].concat(), trace)
            }
            Done::Failed(error) => {
                let plan = &self.bins[b].plan;
                let why = format!("{} could not be started{}: {error}", plan.program.display(), started_in(plan)); // alloc: once per binary run whole
                (Outcome::Failed(Failure::Lost(why.clone())), Vec::new(), Trace::Unknown(why)) // alloc: once per binary run whole
            }
            other => {
                let why = format!("its spawn completed as {other:?}"); // alloc: once per binary run whole
                (Outcome::Failed(Failure::Lost(why.clone())), Vec::new(), Trace::Unknown(why)) // alloc: once per binary run whole
            }
        };
        let outcome = if self.cancelled && outcome != Outcome::Passed { Outcome::NotRun } else { outcome };
        let executed = Executed::Not(WHOLE_UNRECORDED.to_owned()); // alloc: once per binary run whole
        let unit = self.bins[b].first;
        self.finish(unit, Went { outcome, attempts: 1, output, took, trace, executed });
    }
}

/// How many replicas a binary's fork server runs as, itself among them: `ceil(C × F / (F + T))`
/// for `C` cores, a fork of `F` nanoseconds (the server's own measure) and a median test of
/// `T`, at least one and at most the cores and the tests. A replica starts a test per fork,
/// and a core takes about `F + T` per test, so `C` cores need `C × F / (F + T)` forks going
/// at once (docs/v2/plan/lanes/usercode/speed-of-light.md, "Tests"). A fork that costs
/// nothing needs one.
fn replicas(cores: usize, fork: u64, median: u64, tests: usize) -> u32 {
    let cores = cores.max(1) as u128;
    let most = cores.min(tests.max(1) as u128).min(u128::from(u32::MAX));
    let fork = u128::from(fork);
    let wanted = match fork {
        0 => 1,
        _ => (cores * fork).div_ceil(fork + u128::from(median)),
    };
    wanted.clamp(1, most) as u32
}

/// The median of `values`, reordering them; zero for none.
fn median(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let at = values.len() / 2;
    *values.select_nth_unstable(at).1
}

/// `duration` in nanoseconds, saturating.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The wheel's tick `now` is in.
fn tick(now: Duration) -> u64 {
    u64::try_from(now.as_nanos() / TICK).unwrap_or(u64::MAX)
}

/// The first tick at or past `now + after`: a timer never fires early.
fn due_tick(now: Duration, after: Duration) -> u64 {
    u64::try_from(now.saturating_add(after).as_nanos().div_ceil(TICK)).unwrap_or(u64::MAX)
}

/// `packed` lines ([`pack`]) as a [`Run`], what they name looked up in `names`, in one
/// allocation of each buffer; `None` past 4 GiB of names.
fn run(names: &Names, packed: &[u32]) -> Option<Run> {
    let text: usize = packed.iter().map(|&it| names.get(unpack(it).0).len()).sum();
    let mut run = Run { text: String::with_capacity(text), lines: Vec::with_capacity(packed.len()) };
    for &it in packed {
        let (id, seen) = unpack(it);
        if !run.try_push(seen, names.get(id)) {
            return None;
        }
    }
    Some(run)
}

/// Each kind of [`Seen`] by its number, as [`pack`] keeps it.
const SEEN: [Seen; 8] = [
    Seen::Read,
    Seen::Looked,
    Seen::Listed,
    Seen::Env,
    Seen::Untraceable,
    Seen::RefusedWrite,
    Seen::RefusedNet,
    Seen::Incomplete,
];

/// A line as a `u32`: the ID of what it names (at most [`names::MOST`]) and its kind.
fn pack(id: u32, seen: Seen) -> u32 {
    (id << 3) | seen as u32
}

fn unpack(packed: u32) -> (u32, Seen) {
    (packed >> 3, SEEN[(packed & 7) as usize])
}

/// Whether a line of the record is of blaze's own doing, not the tests': its socket, its
/// record, the variables it set for the server.
fn ours(server: &Server, seen: Seen, id: u32) -> bool {
    let [socket, record, socket_var, live_var] = server.ours;
    match seen {
        Seen::Env => id == socket_var || id == live_var,
        Seen::Read | Seen::Looked | Seen::Listed => id == socket || id == record,
        _ => false,
    }
}

/// A line of the record without its line break, borrowed: the parts [`trace::Line::parse`] reads,
/// its kind by its first byte.
fn record_line(text: &str) -> Option<(Seen, &str, &str)> {
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b'\t' {
        return None;
    }
    let seen = match bytes[0] {
        b'R' => Seen::Read,
        b'L' => Seen::Looked,
        b'D' => Seen::Listed,
        b'E' => Seen::Env,
        b'T' => Seen::Untraceable,
        b'W' => Seen::RefusedWrite,
        b'N' => Seen::RefusedNet,
        b'X' => Seen::Incomplete,
        _ => return None,
    };
    let rest = &text[2..];
    let tab = rest.find('\t')?;
    Some((seen, &rest[..tab], &rest[tab + 1..]))
}

/// The whole lines of `text` (which ends with a line break) into the server's lines, as
/// [`trace::parse`] of its lossy text reads them, but in place: each line is read borrowed,
/// and what it names interned. A line not UTF-8 is read lossily alone, which is the same: a
/// replaced sequence never spans a line break.
fn take_lines(server: &mut Server, text: &[u8]) -> Result<(), String> {
    for (number, piece) in text.split_inclusive(|it| *it == b'\n').enumerate() {
        let Some(piece) = piece.strip_suffix(b"\n") else { break };
        let piece = match std::str::from_utf8(piece) {
            Ok(it) => Cow::Borrowed(it),
            Err(_) => Cow::Owned(String::from_utf8_lossy(piece).into_owned()), // alloc: error path
        };
        let Some((seen, scope, what)) = record_line(&piece) else {
            return Err(BadRecord { line: number + 1, text: piece.chars().take(200).collect() }.to_string()); // alloc: error path
        };
        let job = match scope.is_empty() {
            true => None,
            false => match server.scopes.find(scope) {
                Some(id) => Some(server.scope_job[id as usize]),
                // A scope no job of this server has is no test's to be given.
                None => continue,
            },
        };
        let id = server
            .names
            .intern(what)
            .ok_or_else(|| format!("it names more than {} paths and variables, or 4 GiB of them", names::MOST))?; // alloc: error path
        if ours(server, seen, id) {
            continue;
        }
        match job {
            Some(j) => server.scoped[j as usize].push(pack(id, seen)),
            None => {
                let at = id as usize;
                if at >= server.common_kinds.len() {
                    server.common_kinds.resize(at + 1, 0);
                }
                let bit = 1 << seen as u8;
                if server.common_kinds[at] & bit != 0 {
                    continue;
                }
                server.common_kinds[at] |= bit;
                server.common.push(pack(id, seen));
                server.shared = None;
            }
        }
    }
    Ok(())
}

/// Where a binary that could not be started was started: ` in its sandbox`, or nothing outside.
fn started_in(plan: &Binary) -> &'static str {
    match plan.sandboxed {
        true => " in its sandbox",
        false => "",
    }
}

/// How a server's process ended, for the tests it loses.
fn ended(exit: &Exit) -> String {
    match (exit.code, exit.signal) {
        (Some(code), _) => format!("its fork server exited with code {code}"), // alloc: once per server, at its end
        (None, Some(signal)) => format!("its fork server was ended by signal {signal}"), // alloc: once per server, at its end
        (None, None) => "its fork server ended".to_owned(), // alloc: once per server, at its end
    }
}

#[cfg(test)]
mod tests;
