//! THE DRIVER OF `blaze test` (docs/v2/plan/design/test.md, "End state"): test_loop.rs's
//! `TestLoop` (the sans-IO loop) driven on the command's edge, every `Effect::Run` and
//! `Effect::Kill` handed to test_run.rs's `Runner` (deadlines, kills, verdicts, filing), and the
//! runner's `Zygotes` seam answered by [`OneEach`]: EACH TEST IN A PROCESS OF ITS OWN, started
//! the way `blaze run` starts a program (rule 30): the lazy parent (run_parent.rs `Lazy`, its
//! first calls answered over descriptor 3 by `run_loop::drive`), the launcher beside this
//! binary, the records folder of own_process.rs, the code store the command holds. Only the
//! roots differ: row n's test fn as a `fn()` entry ([`TestFront`] over blaze-ladder
//! run/special/test_roots.rs).
//!
//! Per test, in order: its roots are checked first ([`EntryCheck`]: a test whose entry blaze
//! refuses, one returning a value among them, is `NOT RUN` with its reason printed, never a pass
//! and never retried, rule 3); then a temporary folder of its own, the Seatbelt profile (blaze-run
//! child.rs `seatbelt`, the one test profile) applied by `/usr/bin/sandbox-exec` to the launcher,
//! its own process group, standard output and error piped and kept ([`Output`]: the last
//! `2 * OUTPUT_CAP` bytes, all of it hashed and counted), and a deadline (rule 18: the plan's
//! allowance, `--deadline`; past it the group is killed and the run is `TimedOut`). An exit with
//! status 0 is a pass; any other end is a failure. A panic ends the process by the runtime's
//! abort (it unwinds out of an entry no frame catches), which is that test's failure (rule 20)
//! and never the command's.
//!
//! What this cannot say yet, and says: the launcher's run mode writes no descriptor-3 REPORT
//! (test.md piece 10's tracer and report, and the zygote of piece 9, are the next pieces), so a
//! pass is never kept (`NotKept::Untraced`: nothing it entered was seen), a panic is told by its
//! signal and its output, not by its message, and `#[should_panic]` is judged by the plan's own
//! rule on a signal (no panic seen), so such a test FAILS rather than passing falsely. With no
//! tracer there is no head: every suite is opened with no head and no postings, so nothing kept
//! is reused, and the records the loop files ([`Filed`]) are counted and named in one closing line
//! on the error stream ([`Unkept`]), not written (NOT DONE: the store's test records and roots).
//!
//! Linux has no test sandbox: the runner answers every run there `CannotRun::NoSandbox`
//! (test_run.rs `Host::has_sandbox`) before this file starts anything.
//!
//! One test runs at a time ([`OneEach`]: the plan's width is set to 1 here; the zygote's pool
//! runs `width` at once). The floor of one test's run is a fork of a warm zygote plus its first
//! calls' answers found in the store; today's cost is an exec of the launcher per test, which
//! the zygote (piece 9) removes. The time of each failing test is in its failure line.

use core::fmt::{self, Write as _};
use core::time::Duration;

use blaze_id::ItemKey as LibKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, CodeClass};
use blaze_ladder::run::make_one::{Maker, NamesRecords};
use blaze_ladder::run::special::Roots;
use blaze_ladder::run::special::test_roots::test_roots;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_run::head::ReadDigest;
use blaze_run::plan::{Address, NotKept};
use blaze_run::postings::{FnName, ReadName};
use blaze_run::result::{AtAddress, Captured, OUTPUT_CAP};
use blaze_run::zygote::{How, Said, Status as ZygoteExit};
use blaze_run::{Nanos, TestId};
use blaze_scope::tests_record::{TestsRecord, TestsRecordError};
use blaze_screen::Watch;
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::Deadline;
use bstd::fs::{Dir, Durable, FsError, FsKind, ReadWrite};
use bstd::io::{Io, Missing};
use bstd::mem::Frame;
use bstd::path::{Path, PathError};
use bstd::procs::{Ended, GRACE, Held, Message, OutPipe, ParentDeath, Read as Piped, Run, RunChild, RunError as ProcError, RunSpec, SignalError, Spawner, Streams};

use crate::body_types::Kept;
use crate::code_store::HomeCodeStore;
use crate::engine_records::EngineRecords;
use crate::run_names::{self, Found, Owner, RunNamesError, TestUnit, Walked};
use crate::run_ids::{Checked, Grain, RunIds};
use crate::run_loop::{self, Bounds, LoopError as RunLoopError};
use crate::run_parent::{self, Front, Gate, Lazy, Needs, ParentError, RECORDS, RecordsDir, Start as _, TICK, WRITE_BOUND};
use crate::store::{CommandLent, Store};
use crate::test_loop::{CannotRun, Effect, Event, LoopConfig, LoopError, Status, SuiteId, TestLoop};
use crate::test_run::{Answers, ChildEnd, Heard, Host as TestHost, RunError, Runner, Start, Zygotes};

/// The system's launcher of `sandbox_init` (blaze-proc-macro native_sandbox.rs uses the same):
/// it applies the profile, then executes the launcher in its place, so the launcher's process is
/// the test's and its end is the test's end.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// How often a running test's pipes are emptied while its channel is quiet: a writer blocks on
/// a full pipe (64 KiB) for at most this long.
const DRAIN_EVERY: Duration = Duration::from_millis(5);
/// After a test's process ended, how long its pipes may stay open (a child it left holds them)
/// before the rest is not read: a bound, not a wait for anything owed.
const DRAIN_AFTER: Duration = Duration::from_secs(2);
/// The output kept per test: twice what a result holds; the rest is hashed and counted.
const KEEP: usize = 2 * OUTPUT_CAP;
/// The loop's steps per test, plus a fixed allowance: past it the command stops naming the
/// bound (rule 18; a step is one batch of effects).
const STEPS_PER_TEST: usize = 64;
const STEPS_FIXED: usize = 256;
/// A test's address while it is not traced: H(test) alone (no asks are known).
const ADDRESS_DOMAIN: &str = "blaze test address of an untraced run 1";
/// The domain of a test output's digest.
const OUTPUT_DOMAIN: &str = "blaze test output 1";
/// Beneath `<HOME>`: the folder each command's test folders are made in (`<pid>/<row>`).
const TEMPS: &str = ".blaze/v2/test/tmp";
/// The most things one wait can hear: a start, one end, a close and an exit.
const MOST_HEARD: usize = 4;

/// Why the driver stopped the command: each names the step and the thing (rule 19).
#[derive(Debug)]
pub(crate) enum DriveError {
    Loop(LoopError),
    Run(RunError),
    /// A test's run failed in blaze (not in the test), or the run's setup did.
    One(OneError),
    /// A test's own process could not be run, in blaze: the line above names the test and the
    /// cause ([`run_suite`] prints it; the cause borrows nothing it could keep here).
    OneStopped,
    /// A capability the command needs is not in its `Io`.
    Missing(Missing),
    /// The loop asked for a head's answers, but every suite is opened without a head.
    NoHead { effect: &'static str },
    /// The loop asked to open a suite this request does not have.
    NoSuite { suite: SuiteId },
    /// The tests record could not be read.
    Record(TestsRecordError),
    /// Nothing runs, nothing is owed and no status came.
    Stalled,
    /// More steps than [`STEPS_PER_TEST`] per test.
    Steps { most: usize },
    /// The output stream refused a write.
    Print,
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::Loop(error) => write!(f, "blaze test: {error}"),
            DriveError::Run(error) => write!(f, "blaze test: {error}"),
            DriveError::One(error) => write!(f, "blaze test: {error}"),
            DriveError::OneStopped => f.write_str("blaze test: the suite stopped: a test's own process could not be run (the line above names the test and why)"),
            DriveError::Missing(missing) => write!(f, "blaze test: {missing}"),
            DriveError::NoHead { effect } => write!(f, "blaze test: the loop asked {effect}, but no suite has a head yet (test_drive.rs opens every suite without one)"),
            DriveError::NoSuite { suite } => write!(f, "blaze test: the loop asked for suite {}, and this request has one suite (0)", suite.0),
            DriveError::Record(error) => write!(f, "blaze test: {error}"),
            DriveError::Stalled => f.write_str("blaze test: the loop waits for nothing, runs nothing and gave no status (rule 18)"),
            DriveError::Steps { most } => write!(f, "blaze test: the loop took more than {most} steps (rule 18)"),
            DriveError::Print => f.write_str("blaze test: the output stream refused a write"),
        }
    }
}

impl From<LoopError> for DriveError {
    fn from(error: LoopError) -> DriveError {
        DriveError::Loop(error)
    }
}

impl From<RunError> for DriveError {
    fn from(error: RunError) -> DriveError {
        DriveError::Run(error)
    }
}

/// Why one test's run failed in blaze, by step.
#[derive(Debug)]
pub(crate) enum OneError {
    NoRow { row: u32 },
    Record(TestsRecordError),
    Frame(blaze_mem::MemError),
    Path(PathError),
    Temp(FsError),
    Home(bstd::fs::HomeError),
    NoHome,
    Parent(ParentError),
    Launcher(crate::own_process::OwnError),
    /// The Seatbelt profile could not say a path, or words did not fit: in words.
    Profile { why: Box<str> },
    Start(ProcError),
    Pipe(ProcError),
    Loop(RunLoopError),
    Signals(SignalError),
}

impl fmt::Display for OneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OneError::NoRow { row } => write!(f, "the tests record has no row {row}"),
            OneError::Record(error) => write!(f, "{error}"),
            OneError::Frame(error) => write!(f, "a test's frame: {error:?}"),
            OneError::Path(error) => write!(f, "a test's folder name: {error:?}"),
            OneError::Temp(error) => write!(f, "a test's temporary folder: {error}"),
            OneError::Home(error) => write!(f, "the home's folders: {error}"),
            OneError::NoHome => f.write_str("the command's environment names no `HOME`"),
            OneError::Parent(error) => write!(f, "a test's run parent: {error}"),
            OneError::Launcher(error) => write!(f, "the launcher: {error}"),
            OneError::Profile { why } => write!(f, "a test's sandbox profile: {why}"),
            OneError::Start(error) => write!(f, "a test's launcher did not start under {SANDBOX_EXEC}: {error}"),
            OneError::Pipe(error) => write!(f, "a test's output: {error}"),
            OneError::Loop(error) => write!(f, "a test's run: {error}"),
            OneError::Signals(error) => write!(f, "a test's run: the parent's signals: {error}"),
        }
    }
}

/// What the loop files: blaze-run result.rs records, the head and the postings.
pub(crate) trait Filed {
    fn file(&mut self, suite: SuiteId, test: TestId, address: Address, bytes: &[u8]) -> Result<(), DriveError>;
    fn flaky(&mut self, suite: SuiteId, test: TestId) -> Result<(), DriveError>;
    fn head(&mut self, suite: SuiteId, bytes: &[u8]) -> Result<(), DriveError>;
    fn postings(&mut self, suite: SuiteId, bytes: &[u8]) -> Result<(), DriveError>;
}

/// This build's [`Filed`]: nothing is written (the store's test records and their roots are a
/// next piece); each record asked is counted, and the driver says so in one closing line.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Unkept {
    pub results: u32,
    pub flaky: u32,
    pub heads: u32,
    pub postings: u32,
}

impl Filed for Unkept {
    fn file(&mut self, _suite: SuiteId, _test: TestId, _address: Address, _bytes: &[u8]) -> Result<(), DriveError> {
        self.results += 1;
        Ok(())
    }
    fn flaky(&mut self, _suite: SuiteId, _test: TestId) -> Result<(), DriveError> {
        self.flaky += 1;
        Ok(())
    }
    fn head(&mut self, _suite: SuiteId, _bytes: &[u8]) -> Result<(), DriveError> {
        self.heads += 1;
        Ok(())
    }
    fn postings(&mut self, _suite: SuiteId, _bytes: &[u8]) -> Result<(), DriveError> {
        self.postings += 1;
        Ok(())
    }
}

impl fmt::Display for Unkept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze test: nothing of this run is kept yet ({} results, {} flaky findings, {} heads, {} postings not written; test.md, the store's test records)",
            self.results, self.flaky, self.heads, self.postings
        )
    }
}

/// A test's id: its function's library key (the tests record row's `key`), its identity and
/// nothing its code is.
pub(crate) fn test_id(key: LibKey) -> TestId {
    TestId(key.0.0)
}

/// Each row's [`TestId`], in the record's order: the loop's `Opened.ids`.
pub(crate) fn ids_of<'a>(record: &TestsRecord<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, TestId>, TestsRecordError> {
    let mut ids = Vec::with_capacity_in(record.len(), alloc);
    for at in 0..record.len() {
        let row = record.row(at)?.ok_or(TestsRecordError::Row { row: at, what: "no row inside the count" })?;
        ids.push(test_id(row.key));
    }
    Ok(ids)
}

/// Why a row cannot be entered, asked before its run.
pub(crate) trait Refuse {
    /// The refusal's words (they name the test), or `None`.
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError>;
}

/// THE LOOP, driven: one suite (`tests`, the unit's tests record, 16-aligned, `ids` its rows'
/// ids), the check's error count, the runner over its zygotes, `refuse` asked before each run,
/// `filed` for what the loop files, `print` for what it prints. The loop's status, or what
/// stopped it.
#[expect(clippy::too_many_arguments, reason = "the driver's edge: each argument one seam of it")]
pub(crate) fn drive<'a, 'r, Z: Zygotes>(
    config: LoopConfig<'r>,
    tests: &'r [u8],
    ids: &'r [TestId],
    check_errors: u32,
    runner: &mut Runner<'a, Z>,
    refuse: &mut dyn Refuse,
    filed: &mut dyn Filed,
    print: &mut dyn FnMut(&[u8]) -> Result<(), DriveError>,
    alloc: Alloc<'a>,
) -> Result<Status, DriveError> {
    let most = STEPS_FIXED + STEPS_PER_TEST * ids.len();
    let mut machine = TestLoop::new(config, alloc)?;
    let mut out: AVec<'a, Effect<'a>> = Vec::new_in(alloc);
    let mut effects: AVec<'a, Effect<'a>> = Vec::new_in(alloc);
    let mut line: AString<'a> = String::new_in(alloc);
    machine.begin(&mut out)?;
    machine.step(Event::Checked { errors: check_errors }, runner.zygotes().now(), &mut out)?;
    let mut answers = Untraced;
    for _ in 0..most {
        core::mem::swap(&mut out, &mut effects);
        let (mut open, mut done) = (None, None);
        for effect in effects.drain(..) {
            match &effect {
                Effect::Open { suite } => open = Some(*suite),
                Effect::Moved { .. } => return Err(DriveError::NoHead { effect: "the moved asks" }),
                Effect::Look { .. } => return Err(DriveError::NoHead { effect: "a look-up of a kept pass" }),
                Effect::Run { test, row, .. } => match refuse.refusal(*row).map_err(DriveError::One)? {
                    Some(why) => {
                        line.clear();
                        writeln!(line, "blaze test: {why}").map_err(|_| DriveError::Print)?;
                        print(line.as_bytes())?;
                        machine.step(Event::CannotRun { test: *test, why: CannotRun::EntryRefused }, runner.zygotes().now(), &mut out)?;
                    }
                    None => runner.effect(&effect)?,
                },
                Effect::Kill { .. } => runner.effect(&effect)?,
                Effect::File { suite, test, address, bytes, .. } => filed.file(*suite, *test, *address, bytes.as_slice())?,
                Effect::Flaky { suite, finding } => filed.flaky(*suite, finding.flaky.test)?,
                Effect::WriteHead { suite, bytes } => filed.head(*suite, bytes.as_slice())?,
                Effect::WritePostings { suite, bytes } => filed.postings(*suite, bytes.as_slice())?,
                Effect::Print { text } => print(text.as_slice())?,
                Effect::Done { status } => done = Some(*status),
            }
        }
        if let Some(status) = done {
            runner.finish()?;
            return Ok(status);
        }
        if let Some(suite) = open {
            if suite != SuiteId(0) {
                return Err(DriveError::NoSuite { suite });
            }
            let event = Event::Opened { suite, tests, ids, head: None, postings: None };
            machine.step(event, runner.zygotes().now(), &mut out)?;
            continue;
        }
        if !out.is_empty() {
            continue;
        }
        if !runner.busy() {
            return Err(DriveError::Stalled);
        }
        runner.step(&mut machine, &mut answers, &mut out)?;
    }
    Err(DriveError::Steps { most })
}

/// The answers of an untraced run: no asks, so the address is H(test) and holds nothing.
struct Untraced;

impl Answers for Untraced {
    fn answer<'s>(&'s mut self, _suite: SuiteId, test: TestId, _entered: &[FnName], _reads: &[ReadName], _digests: &mut AVec<'_, (ReadName, ReadDigest)>) -> Result<(Address, AtAddress<'s>), RunError> {
        Ok((Address(ContentId::of(ADDRESS_DOMAIN, &test.0).0), AtAddress::EMPTY))
    }
}

/// A test's output: the last [`KEEP`] bytes of its standard output and error (the two
/// interleaved in the order they were read), and a hash and count of all of it.
pub(crate) struct Output<'o> {
    tail: AVec<'o, u8>,
    total: u64,
    hasher: Hasher,
}

impl<'o> Output<'o> {
    pub(crate) fn new(alloc: Alloc<'o>) -> Output<'o> {
        Output { tail: Vec::new_in(alloc), total: 0, hasher: Hasher::new(OUTPUT_DOMAIN) }
    }

    /// Emptied for the next test; the buffer is kept.
    pub(crate) fn reset(&mut self) {
        self.tail.clear();
        self.total = 0;
        self.hasher = Hasher::new(OUTPUT_DOMAIN);
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.total += bytes.len() as u64;
        let bytes = &bytes[bytes.len().saturating_sub(KEEP)..];
        let over = (self.tail.len() + bytes.len()).saturating_sub(KEEP);
        self.tail.drain(..over);
        self.tail.extend_from_slice(bytes);
    }

    /// As a result's capture: the digest of all of it, its count, its tail.
    pub(crate) fn captured(&self) -> Captured<'_> {
        let tail = self.tail.as_slice();
        Captured { digest: self.hasher.finish().0, total: self.total, tail: &tail[tail.len().saturating_sub(OUTPUT_CAP)..] }
    }
}

/// One test's run to its end: [`OneEach`]'s seam.
pub(crate) trait RunOne {
    /// Runs row `row` in a process of its own, killed `budget` after its start: how it ended,
    /// its output into `output`.
    fn run(&mut self, row: u32, budget: Nanos, output: &mut Output<'_>) -> Result<How, OneError>;
}

/// What [`OneEach`] heard, kept until the runner reads it.
#[derive(Clone, Copy)]
enum Item {
    Ready,
    Ended { row: u32, test: TestId, micros: u64, how: How },
    Closed,
    Exited(ZygoteExit),
}

/// The runner's zygote seam over one process per test: `ask` runs the test to its end (bounded
/// by its budget) before it returns, so a wait has its batch ready and never blocks; one test at
/// a time (a second ask before the first end is heard is `RunError::TwoRuns`). `now` is the
/// command's clock.
pub(crate) struct OneEach<'b, R: RunOne, C: Fn() -> Nanos> {
    pub runs: R,
    now: C,
    pending: AVec<'b, Item>,
    batch: AVec<'b, Item>,
    output: Output<'b>,
    /// The error that stopped a run in blaze, and the tests record row it was of: printed by
    /// [`run_suite`] as one line naming the test, before the command's own error line.
    pub failed: Option<(u32, OneError)>,
}

impl<'b, R: RunOne, C: Fn() -> Nanos> OneEach<'b, R, C> {
    pub(crate) fn new(runs: R, now: C, alloc: Alloc<'b>) -> OneEach<'b, R, C> {
        OneEach { runs, now, pending: Vec::with_capacity_in(MOST_HEARD, alloc), batch: Vec::with_capacity_in(MOST_HEARD, alloc), output: Output::new(alloc), failed: None }
    }

    fn hear(&mut self, item: Item) -> Result<(), RunError> {
        if self.pending.len() >= MOST_HEARD {
            return Err(RunError::Seam { call: "more than four things heard before one wait (test_drive.rs `OneEach`)", errno: 0, suite: None });
        }
        self.pending.push(item);
        Ok(())
    }
}

impl<R: RunOne, C: Fn() -> Nanos> Zygotes for OneEach<'_, R, C> {
    fn start(&mut self, _suite: SuiteId, _width: u32) -> Result<Start, RunError> {
        self.hear(Item::Ready)?;
        Ok(Start::Running)
    }

    fn ask(&mut self, row: u32, test: TestId, budget: Nanos) -> Result<(), RunError> {
        let unheard = self.pending.iter().chain(self.batch.iter()).any(|item| matches!(item, Item::Ended { .. }));
        if unheard {
            return Err(RunError::TwoRuns { suite: SuiteId(0), row });
        }
        self.output.reset();
        let began = (self.now)();
        match self.runs.run(row, budget, &mut self.output) {
            Ok(how) => {
                let micros = (self.now)().0.saturating_sub(began.0) / 1_000;
                self.hear(Item::Ended { row, test, micros, how })
            }
            Err(error) => {
                self.failed = Some((row, error));
                // Never shown when `failed` is set: run_suite prints the cause and returns
                // `DriveError::OneStopped` in its place. No errno: the cause is `failed`.
                Err(RunError::Seam { call: "a test's own process (test_drive.rs `OneEach::ask`)", errno: 0, suite: Some(SuiteId(0)) })
            }
        }
    }

    fn end(&mut self) -> Result<(), RunError> {
        self.hear(Item::Closed)?;
        self.hear(Item::Exited(ZygoteExit::Exited { code: 0 }))
    }

    fn kill(&mut self) -> Result<(), RunError> {
        // Every run ended inside its `ask`, by its deadline: nothing is running to kill.
        Ok(())
    }

    fn wait(&mut self, _until: Nanos) -> Result<(), RunError> {
        if self.pending.is_empty() {
            return Err(RunError::Seam { call: "a wait with nothing to hear (test_drive.rs runs each test to its end in `ask`)", errno: 0, suite: None });
        }
        self.batch.append(&mut self.pending);
        Ok(())
    }

    fn heard_len(&self) -> usize {
        self.batch.len()
    }

    fn heard(&self, at: usize) -> Option<Heard<'_>> {
        Some(match *self.batch.get(at)? {
            Item::Ready => Heard::Ready,
            Item::Ended { row, test, micros, how } => Heard::Ended(ChildEnd {
                row,
                test,
                micros,
                how,
                // Nothing it entered or read was seen: its pass is never kept.
                keep: Err(NotKept::Untraced),
                text: "",
                entered: &[],
                reads: &[],
                output: self.output.captured(),
            }),
            Item::Closed => Heard::Closed,
            Item::Exited(status) => Heard::Exited(status),
        })
    }

    fn clear(&mut self) {
        self.batch.clear();
    }

    fn now(&self) -> Nanos {
        (self.now)()
    }
}

fn key_of(tests: &TestsRecord<'_>, row: u32) -> Result<LibKey, OneError> {
    let found = tests.row(row as usize).map_err(OneError::Record)?;
    found.map(|row| row.key).ok_or(OneError::NoRow { row })
}

/// [`Refuse`] over the test roots: each row's roots made in a scratch frame, the words of a
/// refusal kept for the caller. The run makes them again in its own frame: one head lowered
/// twice, until the zygote's protocol carries a refusal (NOT DONE, piece 9).
pub(crate) struct EntryCheck<'c, 'h> {
    pub tests: TestsRecord<'c>,
    pub names: NamesRecords<'h>,
    pub builder: Builder,
    pub tables: Alloc<'static>,
    said: AString<'c>,
}

impl<'c, 'h> EntryCheck<'c, 'h> {
    pub(crate) fn new(tests: TestsRecord<'c>, names: NamesRecords<'h>, builder: Builder, tables: Alloc<'static>, alloc: Alloc<'c>) -> EntryCheck<'c, 'h> {
        EntryCheck { tests, names, builder, tables, said: String::new_in(alloc) }
    }
}

impl Refuse for EntryCheck<'_, '_> {
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError> {
        let test = key_of(&self.tests, row)?;
        let frame = Frame::new(bstd::mem::Tag::Other).map_err(OneError::Frame)?;
        let erased = blaze_own::Erased::new(&frame);
        let made = test_roots(self.names.records, test, self.names.lang_crates, self.builder.settings.panic, self.names.frame, self.tables, erased.handle());
        let Err(why) = made else { return Ok(None) };
        self.said.clear();
        write!(self.said, "{why}").map_err(|_| OneError::Profile { why: Box::from("a refusal's words did not fit in memory") })?;
        Ok(Some(self.said.as_str()))
    }
}

/// A test's run under the lazy parent, as `blaze run`'s program runs (own_process.rs
/// `run_own`), with the test's roots, its own folder and profile, piped output and a deadline.
pub(crate) struct Launched<'c, 'h> {
    pub cx: &'c Cx,
    pub spawner: &'c Spawner,
    /// `blaze-launcher` beside this binary (own_process.rs `launcher_beside`).
    pub launcher: &'c str,
    /// The records folder (run_parent.rs `RECORDS` under the home), and its absolute path: the
    /// launcher's `BLAZE_LAUNCH_STORE`.
    pub records: &'c Dir<ReadWrite>,
    pub store: &'c str,
    /// This command's folder of test folders, and its absolute path.
    pub temps: &'c Dir<ReadWrite>,
    pub temp: &'c str,
    /// The command's environment.
    pub env: &'c [(&'c str, &'c str)],
    pub tests: TestsRecord<'c>,
    pub names: NamesRecords<'h>,
    pub ids: &'c RunIds<'h>,
    pub builder: Builder,
    pub lent: &'c CommandLent,
    pub tables: Alloc<'static>,
    pub watch: &'c dyn Watch,
}

impl RunOne for Launched<'_, '_> {
    fn run(&mut self, row: u32, budget: Nanos, output: &mut Output<'_>) -> Result<How, OneError> {
        let cx = self.cx;
        let test = key_of(&self.tests, row)?;
        // The test's own frame: everything of its run, dropped with it.
        let frame = Frame::new(bstd::mem::Tag::Other).map_err(OneError::Frame)?;
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        // 1. Its temporary folder, made empty: one already there is refused by name, so
        // nothing another run left is read as this one's.
        let name = bstd::mem::format_in!(&frame, "{row}");
        let named = Path::new(name.as_str()).map_err(OneError::Path)?;
        self.temps.make_dir(cx, &named, Durable::No).map_err(OneError::Temp)?;
        let temp = bstd::mem::format_in!(&frame, "{}/{row}", self.temp);
        // 2. The lazy parent with the test's roots; nothing built yet.
        let front = TestFront { names: self.names, ids: self.ids, builder: self.builder, tables: self.tables, alloc, test, watch: self.watch };
        let mut codes = HomeCodeStore::new(self.lent);
        let mut lazy = Lazy::new(cx, front, RecordsDir { cx, dir: self.records }, &mut codes, self.builder, alloc);
        let mut needs = Needs::new(alloc);
        let mut ahead = std::vec::Vec::new_in(alloc);
        let mut placed = Vec::new_in(alloc);
        let head = run_parent::ahead_of(None, &mut lazy, &mut needs, &mut placed, &mut ahead).map_err(OneError::Parent)?;
        let line = head.line;
        let entry = bstd::mem::format_in!(&frame, "{line}");
        // 3. Its environment: its own `TMPDIR`, then the command's, then the run mode's names.
        let mut vars: Vec<(&str, &str), &Frame> = Vec::with_capacity_in(self.env.len() + 8, &frame);
        vars.push(("TMPDIR", temp.as_str()));
        run_parent::launch_env(self.env.iter().copied().filter(|(name, _)| *name != "TMPDIR"), self.store, entry.as_str(), None, &mut vars);
        // 4. The profile (blaze-run child.rs, the one test profile), the launcher under it.
        let profile = blaze_run::child::seatbelt(temp.as_str(), None, alloc).map_err(|why| OneError::Profile { why: Box::from(bstd::mem::format_in!(&frame, "{why}").as_str()) })?;
        let args = ["-p", profile.as_str(), self.launcher];
        let spec = RunSpec { program: SANDBOX_EXEC, arg0: None, args: &args, env: vars.as_slice(), dir: ".", streams: Streams::PIPED, channel: true, own_group: true, parent_death: ParentDeath::STRONGEST };
        let mut run = self.spawner.start_run(cx, &spec).map_err(OneError::Start)?;
        let pipes = [run.take_stdout().map_err(OneError::Pipe)?, run.take_stderr().map_err(OneError::Pipe)?];
        let mut child = Drained { run, pipes, output };
        let bounds = Bounds { tick: TICK, write: WRITE_BOUND, until: Deadline::after(child.now(), Duration::from_nanos(budget.0)), grace: GRACE };
        let ran = {
            // A hold refused returns here: the child's drop kills and reaps it (bstd `Run`).
            let mut held = Held::hold().map_err(OneError::Signals)?;
            let mut inbox = std::vec::Vec::new_in(alloc);
            let mut out = std::vec::Vec::new_in(alloc);
            let _span = cx.span("blaze test: one test's process, to its end or its deadline");
            let ran = run_loop::drive(&mut child, &mut needs, &mut lazy, &ahead, &bounds, &mut held, &mut inbox, &mut out);
            drop(held);
            ran
        };
        drop(lazy);
        // What it wrote after its last message, bounded: its group is gone, so the pipes close.
        let until = Deadline::after(child.now(), DRAIN_AFTER);
        child.drain_until(until).map_err(OneError::Pipe)?;
        Ok(match ran {
            Ok(ran) => match (ran.status, ran.torn) {
                (0, None) => How::Reported(Said::Returned),
                (status @ 0..=127, torn) => How::NoReport { code: i32::from(status), torn: torn.map_or(0, |(came, _)| came as u64) },
                (status, _) => How::Signal { number: i32::from(status - 128) },
            },
            Err(RunLoopError::Deadline { .. }) => How::TimedOut { after: budget },
            Err(error) => return Err(OneError::Loop(error)),
        })
    }
}

/// A run child whose piped output is emptied into an [`Output`] at least every
/// [`DRAIN_EVERY`] while the run loop waits on its channel or its end.
struct Drained<'o, 'p> {
    run: Run,
    pipes: [Option<OutPipe>; 2],
    output: &'o mut Output<'p>,
}

impl Drained<'_, '_> {
    /// Each pipe read until it closes or `until` passes with nothing in it.
    fn drain_until(&mut self, until: Deadline) -> Result<(), ProcError> {
        let mut chunk = [0u8; 16 * 1024];
        for slot in self.pipes.iter_mut() {
            while let Some(pipe) = slot.as_ref() {
                match pipe.read(&mut chunk, until)? {
                    Piped::Bytes(n) => self.output.push(&chunk[..n]),
                    Piped::Closed => *slot = None,
                    Piped::Deadline => break,
                }
            }
        }
        Ok(())
    }
}

impl RunChild for Drained<'_, '_> {
    fn now(&self) -> Duration {
        self.run.now()
    }

    fn recv_message<A: core::alloc::Allocator>(&mut self, into: &mut std::vec::Vec<u8, A>, most: u32, deadline: Deadline) -> Result<Message, ProcError> {
        loop {
            self.drain_until(Deadline::PASSED)?;
            let slice = Deadline::after(self.run.now(), DRAIN_EVERY).min(deadline);
            match self.run.recv_message(into, most, slice)? {
                Message::Deadline if !deadline.passed(self.run.now()) => continue,
                said => return Ok(said),
            }
        }
    }

    fn send(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), ProcError> {
        self.run.send(bytes, deadline)
    }

    fn wait(&mut self, deadline: Deadline) -> Result<Ended, ProcError> {
        loop {
            self.drain_until(Deadline::PASSED)?;
            let slice = Deadline::after(self.run.now(), DRAIN_EVERY).min(deadline);
            match self.run.wait(slice)? {
                Ended::Deadline if !deadline.passed(self.run.now()) => continue,
                ended => return Ok(ended),
            }
        }
    }

    fn kill(&mut self) -> Result<Ended, ProcError> {
        self.run.kill()
    }

    fn terminate(&mut self) -> Result<(), ProcError> {
        self.run.terminate()
    }
}

/// The front of one test's run: the command's names records and body ids (made once for the
/// suite, shared by every test), the roots of row n's test fn (test_roots.rs).
pub(crate) struct TestFront<'s, 'h, 'r> {
    pub names: NamesRecords<'h>,
    pub ids: &'s RunIds<'h>,
    pub builder: Builder,
    pub tables: Alloc<'static>,
    pub alloc: Alloc<'r>,
    pub test: LibKey,
    pub watch: &'s dyn Watch,
}

impl<'s, 'h, 'r> Front for TestFront<'s, 'h, 'r> {
    type Start = Roots<'r>;
    type Ids = &'s RunIds<'h>;
    type Maker = Maker<'h>;

    fn start(&mut self) -> Result<Roots<'r>, ParentError> {
        test_roots(self.names.records, self.test, self.names.lang_crates, self.builder.settings.panic, self.names.frame, self.tables, self.alloc).map_err(ParentError::Roots)
    }

    fn ids(&mut self) -> Result<&'s RunIds<'h>, ParentError> {
        Ok(self.ids)
    }

    fn maker(&mut self, start: &Roots<'r>) -> Result<Maker<'h>, ParentError> {
        // The test's roots start its program at the test fn (`MakerStart::Direct`), not at `main`.
        Ok(Maker::of_roots(self.names, start.maker_entry(), start, &self.builder, self.tables))
    }

    /// As run_parent.rs `CommandFront::path_of`: the one printer over the names records.
    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        use blaze_ladder::front2::source::NamesSource;
        let lib = self.names.records.key_beginning(item.0.to_be_bytes())?;
        blaze_ladder::front2::item_shown::ItemShown::of_lib(self.names.records, lib).path().map(Box::from)
    }

    fn site_shown(&mut self, maker: Option<&Maker<'h>>, item: ItemKey, site: u32) -> Option<crate::run_fault::SiteShown> {
        let (file, line, column) = maker?.site_shown(item, site)?;
        Some(crate::run_fault::SiteShown { file: Box::from(file), line, column })
    }

    fn watch(&self) -> &dyn Watch {
        self.watch
    }
}

/// The body ids one suite's tests share: each test's lazy parent borrows them.
impl BodyIds for &RunIds<'_> {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        (**self).body_check(item)
    }
    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (**self).today(fact)
    }
    fn crate_fact(&self, item: ItemKey) -> Option<Read> {
        (**self).crate_fact(item)
    }
    fn class(&self, item: ItemKey) -> CodeClass {
        (**self).class(item)
    }
}

impl Gate for &RunIds<'_> {
    fn grain(&self, item: ItemKey) -> Option<Grain> {
        (**self).grain(item)
    }
    fn checked(&self, item: ItemKey) -> Option<Checked> {
        (**self).checked(item)
    }
}

/// `name` beneath `dir`, made; one already there taken as it is (a folder is the state wanted);
/// anything else fails naming it.
fn made_or_there(cx: &Cx, dir: &Dir<ReadWrite>, name: &str) -> Result<Dir<ReadWrite>, OneError> {
    let named = Path::new(name).map_err(OneError::Path)?;
    match dir.make_dir(cx, &named, Durable::No) {
        Ok(made) => Ok(made),
        Err(error) if error.kind == FsKind::Exists => Ok(dir.subdir(&named)),
        Err(error) => Err(OneError::Temp(error)),
    }
}

/// What one `blaze test` request hands the driver: the test unit's names records and body ids
/// (main.rs's test arm makes them as the StartProgram arm does for `blaze run`), the builder,
/// the command's store, the unit's tests record and the loop's config.
/// Where the suite's names rows are, and its test unit, for the request `req` at its
/// `Effect::StartTests` (main.rs), as run_names.rs `found` finds a program's: on a walk the
/// answer's test unit ([`run_names::test_unit`]: a library and its test unit share a root, so
/// the unit is found by its tests record, not by a path), its crates, and their rows written at
/// the certificate; on a hit the certificate's rows, the unit found among them. `None`: the
/// request stood and the store holds no rows, so the command answers in full.
pub(crate) fn found_tests<'m>(
    cx: &Cx,
    req: &blaze_drive::Request<'_>,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    read: &mut EngineRecords<'m>,
    heap: &'static blaze_own::Heap,
    scratch: &Frame,
) -> Result<Option<(Found<'m>, TestUnit<'m>)>, RunNamesError> {
    if crate::run_ids::walked(req) {
        let unit = run_names::test_unit(cx, &Walked(req), store, kept, read, heap, scratch)?;
        let program = run_names::program_of(cx, &Walked(req), Owner::At(unit.at), store, kept, read, heap, scratch)?;
        run_names::put(cx, &program, req.certificate(), store, heap)?;
        return Ok(Some((Found::Walked(program), unit)));
    }
    let Some(rows) = run_names::stood(cx, req.certificate(), store)? else { return Ok(None) };
    let unit = run_names::test_unit(cx, &rows, store, kept, read, heap, scratch)?;
    Ok(Some((Found::Stood(rows), unit)))
}

pub(crate) struct Suite<'s, 'h> {
    pub io: &'s Io,
    pub names: NamesRecords<'h>,
    pub ids: &'s RunIds<'h>,
    pub builder: Builder,
    pub lent: &'s CommandLent,
    /// The unit's tests record (blaze-scope tests_record.rs), 16-aligned.
    pub tests: &'s [u8],
    pub config: LoopConfig<'s>,
    /// The check's errors: a suite whose check failed runs nothing (test_loop.rs `Checked`).
    pub check_errors: u32,
    pub tables: Alloc<'static>,
    pub watch: &'s dyn Watch,
}

/// THE COMMAND: one suite run, test by test, the report's lines on the output stream as they
/// come (each failure, each `not kept <test>: <why>` line, the summary). On the error stream:
/// a run's own error (blaze's, not a test's), and the one closing [`Unkept`] line counting the
/// records filed but not written. The loop's status (main.rs ends
/// with `FAILED` unless `failed`, `not_run` and `check_errors` are all 0), or what stopped it.
pub(crate) fn run_suite(cx: &Cx, suite: Suite<'_, '_>, frame: &Frame) -> Result<Status, DriveError> {
    let cx = cx.span("blaze test: the suite");
    let io = suite.io;
    let env = io.need_env().map_err(DriveError::Missing)?;
    let fs = io.need_fs().map_err(DriveError::Missing)?;
    let host = io.need_host().map_err(DriveError::Missing)?;
    let out = io.need_out().map_err(DriveError::Missing)?;
    let err = io.need_err().map_err(DriveError::Missing)?;
    let clock = io.need_clock().map_err(DriveError::Missing)?;
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let one = DriveError::One;
    // 1. The records folder and this command's test folders, under the command's own home
    // (rule 75: read here, at the edge).
    let home_name = bstd::env::VarName::new("HOME").map_err(|_| one(OneError::NoHome))?;
    let home = env.get(&home_name).ok().flatten().ok_or(one(OneError::NoHome))?;
    let home = home.trim_end_matches('/');
    let records = fs.home(&cx, home, &Path::new(RECORDS).map_err(|why| one(OneError::Path(why)))?).map_err(|why| one(OneError::Home(why)))?;
    // The test folders, each level made or taken as it is beneath the home's `.blaze/v2` (which
    // holds the toolchain this command has read, so it is there): a home folder's handle makes
    // nothing until a file is made in it (bstd fs.rs `home`), and `make_dir` makes one level
    // (first real run, 2026-11-18: "make_dir of .blaze/v2/test/tmp/<pid>: No such file").
    let v2 = fs.home(&cx, home, &Path::new(".blaze/v2").map_err(|why| one(OneError::Path(why)))?).map_err(|why| one(OneError::Home(why)))?;
    let test = made_or_there(&cx, &v2, "test").map_err(one)?;
    let temps = made_or_there(&cx, &test, "tmp").map_err(one)?;
    let pid = bstd::mem::format_in!(frame, "{}", host.pid());
    let mine = made_or_there(&cx, &temps, pid.as_str()).map_err(one)?;
    let temp = bstd::mem::format_in!(frame, "{home}/{TEMPS}/{}", pid.as_str());
    let store = bstd::mem::format_in!(frame, "{home}/{RECORDS}");
    // 2. The launcher beside this binary, the spawner, the command's environment.
    let exe = host.exe().map_err(DriveError::Missing)?;
    let launcher = crate::own_process::launcher_beside(&exe, frame).map_err(|why| one(OneError::Launcher(why)))?;
    let spawner = host.spawner().map_err(DriveError::Missing)?;
    let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(frame);
    vars.extend(env.each());
    // 3. The runner over one process per test, one at a time.
    let tests = TestsRecord::open(suite.tests).map_err(DriveError::Record)?;
    let ids = ids_of(&tests, alloc).map_err(DriveError::Record)?;
    let mut config = suite.config;
    config.plan.width = 1;
    let launched = Launched {
        cx: &cx,
        spawner: &spawner,
        launcher: launcher.as_str(),
        records: &records,
        store: store.as_str(),
        temps: &mine,
        temp: temp.as_str(),
        env: vars.as_slice(),
        tests,
        names: suite.names,
        ids: suite.ids,
        builder: suite.builder,
        lent: suite.lent,
        tables: suite.tables,
        watch: suite.watch,
    };
    let now = || Nanos(u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX));
    let zygotes = OneEach::new(launched, now, alloc);
    let mut runner = Runner::new(zygotes, &config.plan, TestHost::THIS, alloc);
    let mut refuse = EntryCheck::new(tests, suite.names, suite.builder, suite.tables, alloc);
    let mut unkept = Unkept::default();
    let mut print = |bytes: &[u8]| -> Result<(), DriveError> {
        for chunk in bytes.utf8_chunks() {
            out.write(&cx, chunk.valid()).map_err(|_| DriveError::Print)?;
            if !chunk.invalid().is_empty() {
                out.write(&cx, "\u{FFFD}").map_err(|_| DriveError::Print)?;
            }
        }
        Ok(())
    };
    let status = drive(config, suite.tests, ids.as_slice(), suite.check_errors, &mut runner, &mut refuse, &mut unkept, &mut print, alloc);
    // A run that stopped in blaze: ONE line naming the test, what failed and its cause, then
    // the command's own error (`OneStopped`) says the line above is why (rule 19). The
    // runner's `Seam` error says only that one stopped, so it is not shown.
    if let (Err(DriveError::Run(_)), Some((row, failed))) = (&status, runner.zygotes().failed.as_ref()) {
        match tests.row(*row as usize) {
            Ok(Some(test)) => err.line(&cx, format_args!("blaze test: the test `{}` could not be run: {failed}", test.path)),
            _ => err.line(&cx, format_args!("blaze test: the test at row {row} of the tests record could not be run: {failed}")),
        }
        .map_err(|_| DriveError::Print)?;
        return Err(DriveError::OneStopped);
    }
    let status = status?;
    if unkept != Unkept::default() {
        err.line(&cx, format_args!("{unkept}")).map_err(|_| DriveError::Print)?;
    }
    Ok(status)
}
