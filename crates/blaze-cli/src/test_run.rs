//! `blaze test`'s runner: what the drive loop's [`Effect::Run`] and [`Effect::Kill`] need, between
//! the loop (test_loop.rs, a sans-IO machine) and one zygote process per suite
//! (docs/v2/plan/design/test.md, "End state", "One process per test"; decision D81; bottom.md
//! section 11: a test run is a program run, code is records, no executable is written). This is
//! the zygote half of piece 16 (the drive loop), over pieces 9 (the zygote: blaze-run zygote.rs
//! `Pool`, driven in the zygote by blaze-launch-rt zygote.rs `run`) and 10 (the child's report).
//!
//! Per suite, once: the zygote is started for the suite's program ([`Zygotes::start`]: the
//! launcher with the suite placed, one thread; it forks ONE child per test). For each run the
//! plan starts: one ask down to the zygote (row, test, the per-test allowance from
//! `Config::deadline`, counted from the fork). The zygote's pool keeps every child's deadline
//! (stacks asked by `SIGQUIT`, then the child's process group killed), captures its output,
//! reads its descriptor-3 report and reaps it; each result comes up as one [`Heard::Ended`].
//! Here each result becomes the loop's [`Event::Ended`]: its exit as the plan reads it (passed,
//! panicked, refused, died by signal N, no report, killed at its deadline), its captured output,
//! and, for a run whose report counted, its asks (each function it entered, each outside read)
//! answered now by the driver's [`Answers`] into its address and what that address holds, which
//! the loop files by result.rs's rule. Nothing is retried (rule 3): a row is asked once
//! ([`RunError::TwoRuns`]).
//!
//! **Deadlines (rule 18).** The pool kills a child at its allowance from its FORK, so a slow
//! placement or a queued fork never eats a test's time. When a [`How::TimedOut`] result comes,
//! [`Event::Tick`] is stepped at the plan's own deadline for that run (test_loop.rs
//! `Effect::Run`: "never one made from `after`"), the loop's [`Effect::Kill`] is taken here, and
//! the run is answered with `Exit::Killed`: the report reads `TimedOut` after exactly the
//! allowance. The zygote itself is watched: it must say it is ready within [`START_DEADLINE`],
//! answer each run within its allowance plus [`ANSWER_SLACK`] (two of the pool's grace periods
//! and a second), let its channel close within [`CHANNEL_GRACE`] of its exit, and exit within
//! [`END_DEADLINE`] of being told the suite is over. Past any of these it is killed and the
//! command stops with a [`RunError`] naming the suite, the row and the bound: a hang is never
//! waited out, and the zygote dying is a typed error naming it ([`RunError::ZygoteDied`]), never
//! a hang and never a quiet "not run". Each such error is blaze failing to run the tests, so the
//! command exits [`RunError::STATUS`] (125, D86; program.rs `NOT_RUN`).
//!
//! **Drain before exit.** The zygote's exit and its channel's end are separate facts (a reap and
//! an end of file) and come in either order. A death is decided only when BOTH came, and in one
//! wake every result is stepped before a close or an exit, so a result the zygote sent before it
//! died always reaches the loop. The simulation's planted bug ([`RunPlant::ExitBeforeDrain`])
//! acts on the exit first and loses those results.
//!
//! **Deterministic output.** The summary, and the failures it prints again, are in path order
//! (blaze-run report.rs `summary_with`), so they do not depend on which child ended first. The
//! failure lines printed AS THEY END follow end order by design (test.md: "printed for a failure
//! as it ends, then again in path order with the summary"); results that come in one wake are
//! stepped in row order (a timed-out one after the others, by its plan deadline), so a batch
//! never prints in an order the pipe's timing chose.
//!
//! **Refused by name, never guessed.** On a host with no test sandbox (Linux: blaze-launch-rt
//! zygote.rs `run` stops with `NoSandbox`), every run is answered [`CannotRun::NoSandbox`]
//! without starting anything (rule 9). A suite whose program the code builder cannot build is
//! answered per test by what [`Zygotes::start`] says ([`Unbuilt`], this build's only zygote
//! source: [`CannotRun::BodiesNotBuilt`], today's `NOT RUN <path>: cannot be run yet: ...`
//! line). `--nocapture` with more than one thread is refused before this (test_args.rs
//! `TestArgsError::NocaptureThreads`).
//!
//! **The process seam.** [`Zygotes`] is the IO: start, ask, end, kill, a deadline-bounded wait
//! that opens a span naming what it waits for (rule 15), and what the wait heard. The real
//! implementation needs a primitive bstd does not have yet (a process started with a socket on
//! its descriptor 3, its own process group, a bounded wait on both, the kill of its group); until
//! it exists, [`Unbuilt`] answers every suite as not buildable, which is also the truth today.
//!
//! Floor (rule 67): per test this adds one message each way and one sort of a wake's batch to the
//! fork the design needs anyway (0.3 ms Linux, 1 ms macOS): ratio about 1.0.

#![cfg_attr(not(test), allow(dead_code, reason = "wired by the driver's lane: main.rs `command`'s test arm hands the loop's runs to a `Runner` over `Unbuilt`"))]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};
use blaze_run::head::ReadDigest;
use blaze_run::plan::{Address, Config, Exit, NotKept, Ran};
use blaze_run::postings::{Ask, FnName, ReadName};
use blaze_run::result::{AtAddress, Captured};
use blaze_run::zygote::{Finished, GRACE, How, Said, Status as ZygoteExit};
use blaze_run::{Nanos, TestId};

use crate::test_loop::{CannotRun, Detail, Effect, Event, LoopError, SuiteId, TestEnded, TestLoop};

/// How long the zygote may take to place the suite's program and say it is ready.
pub(crate) const START_DEADLINE: Nanos = Nanos(60_000_000_000);
/// Past a run's allowance (from the later of its ask and the zygote's ready), how long the
/// zygote has to answer it: the pool's grace for the stacks, its grace after the kill, a second.
pub(crate) const ANSWER_SLACK: Nanos = Nanos(2 * GRACE.0 + 1_000_000_000);
/// After its exit, how long the zygote's channel may stay open (a process it left holds it).
pub(crate) const CHANNEL_GRACE: Nanos = GRACE;
/// After the suite is over, how long the zygote has to exit.
pub(crate) const END_DEADLINE: Nanos = Nanos(5_000_000_000);

/// The host's operating system, passed in (rule 75): which one decides whether a test sandbox
/// exists here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Host {
    Macos,
    Linux,
}

impl Host {
    /// The host this blaze was built for.
    pub(crate) const THIS: Host = if cfg!(target_os = "linux") { Host::Linux } else { Host::Macos };

    /// A test sandbox exists here (macOS `sandbox_init`; Linux's is not built: test.md piece 9).
    pub(crate) const fn has_sandbox(self) -> bool {
        match self {
            Host::Macos => true,
            Host::Linux => false,
        }
    }
}

/// One run's end as the zygote said it: blaze-run's `zygote::Finished`, read from its message.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildEnd<'e> {
    pub row: u32,
    pub test: TestId,
    pub micros: u64,
    pub how: How,
    pub keep: Result<(), NotKept>,
    /// The panic message or what was refused; empty otherwise.
    pub text: &'e str,
    /// The functions it entered, sorted, once each (empty unless its report counted).
    pub entered: &'e [FnName],
    /// Its outside reads, sorted, once each.
    pub reads: &'e [ReadName],
    pub output: Captured<'e>,
}

impl<'e> ChildEnd<'e> {
    /// The zygote's own result, as its message carries it.
    pub(crate) fn of(finished: &'e Finished<'_>) -> ChildEnd<'e> {
        ChildEnd {
            row: finished.row,
            test: finished.test,
            micros: finished.micros,
            how: finished.how,
            keep: finished.keep,
            text: finished.text(),
            entered: finished.entered.as_slice(),
            reads: finished.reads.as_slice(),
            output: finished.captured(),
        }
    }

    /// How it ended, as the plan reads it (blaze-run zygote.rs `Finished::exit`).
    fn exit(&self) -> Exit<'e> {
        match self.how {
            How::Reported(Said::Returned) => Exit::Returned,
            How::Reported(Said::Panicked { .. }) => Exit::Panicked { text: self.text },
            How::Reported(Said::Refused { .. }) => Exit::Refused,
            How::Signal { number } => Exit::Signal { number },
            How::TimedOut { .. } => Exit::Killed,
            How::NoReport { code, .. } => Exit::NoReport { code },
        }
    }

    /// What a failure's report says beyond the exit. The printed place (`src/lib.rs:3:5`) is
    /// made where the report is printed from the item-relative one (rule 71); not here.
    fn detail(&self) -> Detail<'e> {
        match self.how {
            How::Reported(Said::Panicked { at }) => Detail { place: at, ..Detail::NONE },
            How::Reported(Said::Refused { refusal }) => Detail { refusal: Some((refusal, self.text)), ..Detail::NONE },
            How::NoReport { torn, .. } => Detail { torn, ..Detail::NONE },
            How::Reported(Said::Returned) | How::Signal { .. } | How::TimedOut { .. } => Detail::NONE,
        }
    }
}

/// What one wait heard from the zygote.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Heard<'e> {
    /// The suite is placed; it forks from now on.
    Ready,
    Ended(ChildEnd<'e>),
    /// The child was reaped after a first call whose instance the parent could not build.
    CannotBuild { row: u32, test: TestId, why: &'e crate::test_drive::InstanceRefusal },
    /// The zygote's channel reached its end.
    Closed,
    /// The zygote process was reaped.
    Exited(ZygoteExit),
}

/// What [`Zygotes::start`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Start {
    /// A zygote is starting for the suite.
    Running,
    /// No zygote can run this suite: every run of it is answered so.
    Cannot(CannotRun),
}

/// The zygote processes, one suite's at a time: the IO under [`Runner`]. Every time is on the
/// clock [`Zygotes::now`] reads (passed in by the caller, rule 75).
pub(crate) trait Zygotes {
    /// Starts the zygote of `suite`: the launcher with the suite's program placed, one thread,
    /// its own process group, its channel on descriptor 3, forking at most `width` children.
    fn start(&mut self, suite: SuiteId, width: u32) -> Result<Start, RunError>;
    /// Asks the zygote to run tests-record row `row` as `test`, killed `budget` after its fork.
    fn ask(&mut self, row: u32, test: TestId, budget: Nanos) -> Result<(), RunError>;
    /// Tells the zygote the suite is over: it exits once its last child is reaped.
    fn end(&mut self) -> Result<(), RunError>;
    /// Kills the zygote and every child it started.
    fn kill(&mut self) -> Result<(), RunError>;
    /// Waits until something is heard or `until` comes, whichever is first; reads everything
    /// ready (the batch [`Zygotes::heard`] gives). Opens a span naming what it waits for.
    fn wait(&mut self, until: Nanos) -> Result<(), RunError>;
    /// How many things the last wait heard.
    fn heard_len(&self) -> usize;
    /// The `at`th thing the last wait heard, in the order it was read.
    fn heard(&self, at: usize) -> Option<Heard<'_>>;
    /// Forgets the last wait's batch.
    fn clear(&mut self);
    fn now(&self) -> Nanos;
    /// How long the run of `row` has been parked waiting for its code (made off the command
    /// thread, D194): its allowance does not run then, so [`answer_due`] moves by it. Zero for
    /// a row not running and wherever code is made inside the run's own turn.
    fn parked(&self, row: u32) -> Nanos;
}

/// The driver's answers for a run whose report counted.
pub(crate) trait Answers {
    /// The run's asks answered now: each outside read's digest into `digests`, and the address
    /// H(test, asks, answers now) with what it holds (`AtAddress::EMPTY` when nothing).
    fn answer<'s>(
        &'s mut self,
        suite: SuiteId,
        test: TestId,
        entered: &[FnName],
        reads: &[ReadName],
        digests: &mut AVec<'_, (ReadName, ReadDigest)>,
    ) -> Result<(Address, AtAddress<'s>), RunError>;
}

/// This build's zygotes: none can be started, because the code builder cannot build test
/// bodies yet (test.md piece 8). Every suite is answered [`CannotRun::BodiesNotBuilt`], which
/// keeps today's `NOT RUN` line per test; a zygote's work asked of it is an error by name.
pub(crate) struct Unbuilt<C: Fn() -> Nanos> {
    /// The driver's clock.
    pub now: C,
}

impl<C: Fn() -> Nanos> Zygotes for Unbuilt<C> {
    fn start(&mut self, _suite: SuiteId, _width: u32) -> Result<Start, RunError> {
        Ok(Start::Cannot(CannotRun::BodiesNotBuilt))
    }
    fn ask(&mut self, _row: u32, _test: TestId, _budget: Nanos) -> Result<(), RunError> {
        Err(RunError::NotStarted { call: "ask" })
    }
    fn end(&mut self) -> Result<(), RunError> {
        Err(RunError::NotStarted { call: "end" })
    }
    fn kill(&mut self) -> Result<(), RunError> {
        Err(RunError::NotStarted { call: "kill" })
    }
    fn wait(&mut self, _until: Nanos) -> Result<(), RunError> {
        Err(RunError::NotStarted { call: "wait" })
    }
    fn heard_len(&self) -> usize {
        0
    }
    fn heard(&self, _at: usize) -> Option<Heard<'_>> {
        None
    }
    fn clear(&mut self) {}
    fn now(&self) -> Nanos {
        (self.now)()
    }
    fn parked(&self, _row: u32) -> Nanos {
        Nanos(0)
    }
}

/// A planted bug for the simulation's control; [`RunPlant::None`] in every real run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RunPlant {
    None,
    /// Advances all flights' plan clocks at the first actual expiration.
    TickBeforeSibling,
    /// Takes a wake's batch in the order it was read and decides the zygote's death at its exit,
    /// before its channel is drained.
    ExitBeforeDrain,
}

/// Why the runner stopped the command. Each names the suite and the row or test where there is
/// one, and the bound that was passed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum RunError {
    Loop(LoopError),
    /// An effect of the loop that is not the runner's was handed to it.
    NotMine { effect: &'static str },
    /// A zygote's work was asked of [`Unbuilt`], which starts none.
    NotStarted { call: &'static str },
    /// A row asked while it runs: nothing runs twice (rule 3).
    TwoRuns { suite: SuiteId, row: u32 },
    /// The loop asked to kill a test that is not running.
    KillUnasked { suite: SuiteId, test: TestId },
    /// The zygote answered a row no run is out for.
    Unasked { suite: SuiteId, row: u32 },
    /// The zygote answered a row with another test.
    WrongTest { suite: SuiteId, row: u32 },
    /// The pool killed a run at its allowance, and the plan's tick at the run's deadline asked
    /// no kill of it: the two deadlines disagree.
    NotKilled { suite: SuiteId, row: u32 },
    /// The zygote said it was ready twice.
    ReadyTwice { suite: SuiteId },
    /// The process seam gave no `at`th thing of its batch although it counted it.
    HeardPast { at: usize },
    /// A wait was asked with nothing owed.
    NothingOwed,
    /// The suite was ended with runs still out.
    EndWithRuns { suite: SuiteId, runs: u32 },
    /// The zygote exited (or its channel ended and it exited) with runs not answered, or with a
    /// failing status after the suite was over. `first`: the lowest row still out, and its test.
    ZygoteDied { suite: SuiteId, status: Option<ZygoteExit>, waiting: u32, first: Option<(u32, TestId)> },
    /// The zygote did not say it was ready within [`START_DEADLINE`]; it was killed.
    ZygoteNotReady { suite: SuiteId, waited: Nanos },
    /// The zygote did not answer a run within its allowance and [`ANSWER_SLACK`]; it was killed.
    ZygoteSilent { suite: SuiteId, row: u32, test: TestId },
    /// The zygote exited and its channel stayed open past [`CHANNEL_GRACE`]; it was killed.
    ChannelHeld { suite: SuiteId },
    /// The zygote did not exit within [`END_DEADLINE`] after the suite was over; it was killed.
    ZygoteStayed { suite: SuiteId, waited: Nanos },
    /// A call of the process seam failed.
    Seam { call: &'static str, errno: i32, suite: Option<SuiteId> },
}

impl RunError {
    /// The command's status when the runner stops it: blaze could not run the tests (D86).
    pub(crate) const STATUS: u8 = crate::program::NOT_RUN;
}

impl From<LoopError> for RunError {
    fn from(error: LoopError) -> RunError {
        RunError::Loop(error)
    }
}

/// A test id as printed: its first six bytes in hex.
struct Short(TestId);

impl fmt::Display for Short {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Seconds, one decimal.
struct Secs(Nanos);

impl fmt::Display for Secs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{} s", self.0.0 / 1_000_000_000, self.0.0 % 1_000_000_000 / 100_000_000)
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Loop(error) => write!(f, "{error}"),
            RunError::NotMine { effect } => write!(f, "the test runner was handed the loop's `{effect}`, which is not a run's"),
            RunError::NotStarted { call } => write!(f, "the test runner asked `{call}` of a zygote this build cannot start (test.md, piece 8)"),
            RunError::TwoRuns { suite, row } => write!(f, "suite {}: test row {row} asked to run while it runs; nothing runs twice (rule 3)", suite.0),
            RunError::KillUnasked { suite, test } => write!(f, "suite {}: a kill of test {}, which is not running", suite.0, Short(*test)),
            RunError::Unasked { suite, row } => write!(f, "suite {}: the test zygote answered row {row}, which no run is out for", suite.0),
            RunError::WrongTest { suite, row } => write!(f, "suite {}: the test zygote answered row {row} with another test", suite.0),
            RunError::NotKilled { suite, row } => write!(
                f,
                "suite {}: test row {row} was killed at its allowance, and the plan's deadline for it asked no kill",
                suite.0
            ),
            RunError::ReadyTwice { suite } => write!(f, "suite {}: the test zygote said it was ready twice", suite.0),
            RunError::HeardPast { at } => write!(f, "the test zygote's batch has no message {at}"),
            RunError::NothingOwed => write!(f, "the test runner was asked to wait with no run out"),
            RunError::EndWithRuns { suite, runs } => write!(f, "suite {}: ended with {runs} runs still out", suite.0),
            RunError::ZygoteDied { suite, status, waiting, first } => {
                write!(f, "suite {}: the test zygote ", suite.0)?;
                match status {
                    Some(ZygoteExit::Exited { code }) => write!(f, "exited with status {code}")?,
                    Some(ZygoteExit::Signaled { number }) => write!(f, "died by signal {number}")?,
                    None => write!(f, "closed its channel and was not reaped")?,
                }
                match first {
                    Some((row, test)) => write!(f, " with {waiting} runs not answered (the first: row {row}, test {})", Short(*test)),
                    None => write!(f, " with no run out"),
                }
            }
            RunError::ZygoteNotReady { suite, waited } => {
                write!(f, "suite {}: the test zygote did not place the suite within {}; it was killed", suite.0, Secs(*waited))
            }
            RunError::ZygoteSilent { suite, row, test } => write!(
                f,
                "suite {}: the test zygote did not answer row {row} (test {}) within its allowance and {}; it was killed",
                suite.0,
                Short(*test),
                Secs(ANSWER_SLACK)
            ),
            RunError::ChannelHeld { suite } => write!(
                f,
                "suite {}: the test zygote exited and something it started held its channel past {}; it was killed",
                suite.0,
                Secs(CHANNEL_GRACE)
            ),
            RunError::ZygoteStayed { suite, waited } => {
                write!(f, "suite {}: the test zygote did not exit within {} after its suite ended; it was killed", suite.0, Secs(*waited))
            }
            RunError::Seam { call, errno, suite: Some(suite) } => write!(f, "suite {}: the test zygote's `{call}` failed: errno {errno}", suite.0),
            RunError::Seam { call, errno, suite: None } => write!(f, "the test zygote's `{call}` failed: errno {errno}"),
        }
    }
}

impl core::error::Error for RunError {}

/// The zygote's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// No zygote.
    Idle,
    /// Started at `at`; not yet ready.
    Starting { at: Nanos },
    /// Ready at `at`.
    Ready { at: Nanos },
    /// The suite cannot run: each run is answered so.
    Cannot(CannotRun),
    /// Told the suite is over at `at`.
    Ending { at: Nanos },
}

/// A run out at the zygote.
#[derive(Clone, Copy, Debug)]
struct Flight {
    test: TestId,
    row: u32,
    /// The plan's deadline, absolute.
    deadline: Nanos,
    asked: Nanos,
    /// The loop asked its kill.
    killed: bool,
    /// The child was killed at its own allowance. The global plan clock can advance to its
    /// deadline once the other flights with an earlier or equal deadline have answered.
    expired: bool,
}

struct Timeout<'b> {
    row: u32,
    test: TestId,
    deadline: Nanos,
    after: Nanos,
    keep: Result<(), NotKept>,
    micros: u64,
    digest: [u8; 32],
    total: u64,
    tail: AVec<'b, u8>,
}

/// The latest of (the zygote's ready, the run's ask) plus the allowance, the time the run was
/// parked waiting for its code ([`Zygotes::parked`], which its allowance does not count: the
/// code wait has its own deadline and error, D194) and [`ANSWER_SLACK`], which bounds only the
/// relay's own lateness.
fn answer_due(ready: Nanos, flight: &Flight, budget: Nanos, parked: Nanos) -> Nanos {
    ready.max(flight.asked).after(budget).after(parked).after(ANSWER_SLACK)
}

fn earliest(wake: Option<Nanos>, at: Nanos) -> Option<Nanos> {
    Some(wake.map_or(at, |wake| wake.min(at)))
}

const fn effect_name(effect: &Effect<'_>) -> &'static str {
    match effect {
        Effect::Open { .. } => "Open",
        Effect::Moved { .. } => "Moved",
        Effect::Look { .. } => "Look",
        Effect::Run { .. } => "Run",
        Effect::Kill { .. } => "Kill",
        Effect::File { .. } => "File",
        Effect::Flaky { .. } => "Flaky",
        Effect::Print { .. } => "Print",
        Effect::WriteHead { .. } => "WriteHead",
        Effect::WritePostings { .. } => "WritePostings",
        Effect::Done { .. } => "Done",
    }
}

/// The loop's runs, performed: one zygote per suite, one child per run.
///
/// The driver hands it every [`Effect::Run`] and [`Effect::Kill`] ([`Runner::effect`]); while
/// [`Runner::busy`], it calls [`Runner::step`], which answers owed refusals at once or waits
/// (bounded) on the zygote and steps the loop with what it heard; at [`Effect::Done`] it calls
/// [`Runner::finish`]. Any [`RunError`] stops the command with [`RunError::STATUS`].
pub(crate) struct Runner<'b, Z: Zygotes> {
    zygotes: Z,
    width: u32,
    budget: Nanos,
    host: Host,
    alloc: Alloc<'b>,
    suite: Option<SuiteId>,
    phase: Phase,
    closed: bool,
    exited: Option<(ZygoteExit, Nanos)>,
    flights: AVec<'b, Flight>,
    timeouts: AVec<'b, Timeout<'b>>,
    /// Runs to answer "cannot be run", in the order the plan started them.
    owed: AVec<'b, (TestId, CannotRun)>,
    asks: AVec<'b, Ask>,
    digests: AVec<'b, (ReadName, ReadDigest)>,
    /// One wake's batch: (rank, plan deadline, row, index heard).
    order: AVec<'b, (u8, u64, u32, usize)>,
    answered: u32,
    pub(crate) plant: RunPlant,
}

impl<'b, Z: Zygotes> Runner<'b, Z> {
    pub(crate) fn new(zygotes: Z, plan: &Config<'_>, host: Host, alloc: Alloc<'b>) -> Runner<'b, Z> {
        Runner {
            zygotes,
            width: plan.width,
            budget: plan.deadline,
            host,
            alloc,
            suite: None,
            phase: Phase::Idle,
            closed: false,
            exited: None,
            flights: Vec::new_in(alloc),
            timeouts: Vec::new_in(alloc),
            owed: Vec::new_in(alloc),
            asks: Vec::new_in(alloc),
            digests: Vec::new_in(alloc),
            order: Vec::new_in(alloc),
            answered: 0,
            plant: RunPlant::None,
        }
    }

    /// How many runs were answered to the loop (ended or not run).
    pub(crate) const fn answered(&self) -> u32 {
        self.answered
    }

    /// The zygote source, for the simulation's oracle.
    pub(crate) const fn zygotes(&self) -> &Z {
        &self.zygotes
    }

    /// A run is out or an answer is owed: the driver must [`Runner::step`].
    pub(crate) fn busy(&self) -> bool {
        !self.owed.is_empty() || !self.flights.is_empty()
    }

    /// Performs one of the loop's [`Effect::Run`] or [`Effect::Kill`].
    pub(crate) fn effect(&mut self, effect: &Effect<'_>) -> Result<(), RunError> {
        match effect {
            Effect::Run { suite, test, row, deadline } => self.run(*suite, *test, *row, *deadline),
            Effect::Kill { suite, test } => self.kill_asked(*suite, *test),
            other => Err(RunError::NotMine { effect: effect_name(other) }),
        }
    }

    /// At [`Effect::Done`]: the last zygote is told the suite is over and waited for (bounded).
    pub(crate) fn finish(&mut self) -> Result<(), RunError> {
        if let (Some(suite), false) = (self.suite, self.owed.is_empty() && self.flights.is_empty()) {
            return Err(RunError::EndWithRuns { suite, runs: (self.owed.len() + self.flights.len()) as u32 });
        }
        self.end_zygote()
    }

    fn run(&mut self, suite: SuiteId, test: TestId, row: u32, deadline: Nanos) -> Result<(), RunError> {
        if self.suite != Some(suite) {
            self.end_zygote()?;
            self.begin_suite(suite)?;
        }
        if let Phase::Cannot(why) = self.phase {
            self.owed.push((test, why));
            return Ok(());
        }
        if self.closed {
            // Its channel ended: nothing more can come from it, so its death is decided now.
            return Err(self.died(suite));
        }
        if self.flights.iter().any(|flight| flight.row == row) {
            return Err(RunError::TwoRuns { suite, row });
        }
        if self.exited.is_some() {
            // Reaped, but its channel is not drained: results it sent before it died may still be
            // in it ("Drain before exit"). This run is not asked (no process reads the ask); it is
            // held out so the death, decided at the channel's end ([`Runner::take`]) or past
            // [`CHANNEL_GRACE`] ([`Runner::watch`]), counts it as not answered.
            let asked = self.zygotes.now();
            self.flights.push(Flight { test, row, deadline, asked, killed: false, expired: false });
            return Ok(());
        }
        self.zygotes.ask(row, test, self.budget)?;
        let asked = self.zygotes.now();
        self.flights.push(Flight { test, row, deadline, asked, killed: false, expired: false });
        Ok(())
    }

    fn begin_suite(&mut self, suite: SuiteId) -> Result<(), RunError> {
        self.reset();
        self.suite = Some(suite);
        if !self.host.has_sandbox() {
            self.phase = Phase::Cannot(CannotRun::NoSandbox);
            return Ok(());
        }
        self.phase = match self.zygotes.start(suite, self.width)? {
            Start::Running => Phase::Starting { at: self.zygotes.now() },
            Start::Cannot(why) => Phase::Cannot(why),
        };
        Ok(())
    }

    fn kill_asked(&mut self, suite: SuiteId, test: TestId) -> Result<(), RunError> {
        if self.suite != Some(suite) {
            return Err(RunError::KillUnasked { suite, test });
        }
        match self.flights.iter_mut().find(|flight| flight.test == test) {
            Some(flight) => {
                // The pool kills it at its allowance from its fork; the answer says `Killed`.
                flight.killed = true;
                Ok(())
            }
            None => Err(RunError::KillUnasked { suite, test }),
        }
    }

    fn reset(&mut self) {
        self.suite = None;
        self.phase = Phase::Idle;
        self.closed = false;
        self.exited = None;
    }

    fn died(&self, suite: SuiteId) -> RunError {
        let first = self.flights.iter().min_by_key(|flight| flight.row).map(|flight| (flight.row, flight.test));
        RunError::ZygoteDied { suite, status: self.exited.map(|(status, _)| status), waiting: self.flights.len() as u32, first }
    }

    /// The earliest bound the runner keeps.
    fn next_wake(&self) -> Option<Nanos> {
        let mut wake = None;
        match self.phase {
            Phase::Starting { at } => wake = earliest(wake, at.after(START_DEADLINE)),
            Phase::Ready { at } => {
                for flight in self.flights.iter().filter(|flight| !flight.expired) {
                    wake = earliest(wake, answer_due(at, flight, self.budget, self.zygotes.parked(flight.row)));
                }
            }
            Phase::Ending { at } => wake = earliest(wake, at.after(END_DEADLINE)),
            Phase::Idle | Phase::Cannot(_) => {}
        }
        if let (Some((_, at)), false) = (self.exited, self.closed) {
            wake = earliest(wake, at.after(CHANNEL_GRACE));
        }
        wake
    }

    /// Answers what is owed, or waits (bounded) on the zygote and steps the loop with what it
    /// heard. The loop's new effects go to `out`, in order, for the driver.
    pub(crate) fn step<'a, 'r>(&mut self, machine: &mut TestLoop<'a, 'r>, answers: &mut dyn Answers, out: &mut AVec<'_, Effect<'a>>) -> Result<(), RunError> {
        if !self.owed.is_empty() {
            let now = self.zygotes.now();
            let mut owed = core::mem::replace(&mut self.owed, Vec::new_in(self.alloc));
            for &(test, why) in owed.iter() {
                machine.step(Event::CannotRun { test, why }, now, out)?;
                self.answered += 1;
            }
            owed.clear();
            // Runs the steps started went to `out`, not here: the buffer is reused.
            if self.owed.is_empty() {
                self.owed = owed;
            }
            return Ok(());
        }
        let (Some(suite), Some(until)) = (self.suite, self.next_wake()) else { return Err(RunError::NothingOwed) };
        if self.flights.is_empty() {
            return Err(RunError::NothingOwed);
        }
        self.zygotes.wait(until)?;
        let mut now = self.zygotes.now();
        let mut heard = self.zygotes.heard_len();
        let taken = self.take(suite, machine, answers, out, now);
        self.zygotes.clear();
        taken?;
        // Drain any already-ready completions at `now` before declaring a remaining flight silent:
        // a sibling's batch may have returned right as another flight finished or updated its
        // parked credit. Bounded by the remaining flight count plus one (rule 18).
        let max_drains = self.flights.len().saturating_add(1);
        for _ in 0..max_drains {
            if heard == 0 || !self.any_silent(now) {
                break;
            }
            self.zygotes.wait(now)?;
            now = self.zygotes.now();
            heard = self.zygotes.heard_len();
            let taken = self.take(suite, machine, answers, out, now);
            self.zygotes.clear();
            taken?;
        }
        self.watch(suite, now)
    }

    fn any_silent(&self, now: Nanos) -> bool {
        let Phase::Ready { at } = self.phase else { return false };
        let budget = self.budget;
        self.flights
            .iter()
            .any(|flight| !flight.expired && now >= answer_due(at, flight, budget, self.zygotes.parked(flight.row)))
    }

    /// One wake's batch: ready first, then the results by row (a timed-out one after the
    /// others, by its plan deadline), then the channel's end, then the exit.
    fn take<'a, 'r>(&mut self, suite: SuiteId, machine: &mut TestLoop<'a, 'r>, answers: &mut dyn Answers, out: &mut AVec<'_, Effect<'a>>, now: Nanos) -> Result<(), RunError> {
        self.order.clear();
        for at in 0..self.zygotes.heard_len() {
            let Some(heard) = self.zygotes.heard(at) else { return Err(RunError::HeardPast { at }) };
            let (rank, deadline, row) = match heard {
                Heard::Ready => (0, 0, 0),
                Heard::Ended(end) => match end.how {
                    How::TimedOut { .. } => {
                        let deadline = self.flights.iter().find(|flight| flight.row == end.row).map_or(u64::MAX, |flight| flight.deadline.0);
                        (2, deadline, end.row)
                    }
                    _ => (1, 0, end.row),
                },
                Heard::CannotBuild { row, .. } => (1, 0, row),
                Heard::Closed => (3, 0, 0),
                Heard::Exited(_) => (4, 0, 0),
            };
            self.order.push((rank, deadline, row, at));
        }
        let drain_first = self.plant != RunPlant::ExitBeforeDrain;
        if drain_first {
            self.order.sort_unstable();
        }
        for k in 0..self.order.len() {
            let at = self.order[k].3;
            let Some(heard) = self.zygotes.heard(at) else { return Err(RunError::HeardPast { at }) };
            match heard {
                Heard::Ready => match self.phase {
                    Phase::Starting { .. } => self.phase = Phase::Ready { at: now },
                    _ => return Err(RunError::ReadyTwice { suite }),
                },
                Heard::Ended(end @ ChildEnd { how: How::TimedOut { after }, .. }) => {
                    let Some(flight) = self.flights.iter_mut().find(|f| f.row == end.row && !f.expired) else {
                        return Err(RunError::Unasked { suite, row: end.row });
                    };
                    if flight.test != end.test {
                        return Err(RunError::WrongTest { suite, row: end.row });
                    }
                    flight.expired = true;
                    self.timeouts.push(Timeout {
                        row: end.row, test: end.test, deadline: flight.deadline, after, keep: end.keep,
                        micros: end.micros, digest: end.output.digest, total: end.output.total,
                        tail: Vec::from_slice_in(end.output.tail, self.alloc),
                    });
                }
                Heard::Ended(end) => {
                    ended(&end, &mut self.flights, &mut self.asks, &mut self.digests, suite, machine, answers, out, now, self.alloc)?;
                    self.answered += 1;
                }
                Heard::CannotBuild { row, test, why } => {
                    take_flight(&mut self.flights, suite, row, test)?;
                    machine.step(Event::CannotBuild { suite, test, why }, now, out)?;
                    self.answered += 1;
                }
                Heard::Closed => {
                    self.closed = true;
                    self.flush_timeouts(suite, machine, answers, out, now)?;
                    if self.exited.is_some() {
                        return Err(self.died(suite));
                    }
                }
                Heard::Exited(status) => {
                    self.exited = Some((status, now));
                    if self.closed || !drain_first {
                        return Err(self.died(suite));
                    }
                }
            }
            self.flush_timeouts(suite, machine, answers, out, now)?;
        }
        Ok(())
    }

    /// A tick applies to *all* flights at its deadline. Children started in one plan step
    /// have that same deadline even when their real forks are apart. Apply the tick only
    /// once every such flight has either returned or reported its own timeout. After EOF
    /// there can be no more child reports; drain the last receipts before reporting a death.
    fn flush_timeouts<'a, 'r>(&mut self, suite: SuiteId, machine: &mut TestLoop<'a, 'r>, answers: &mut dyn Answers, out: &mut AVec<'_, Effect<'a>>, now: Nanos) -> Result<(), RunError> {
        while let Some((at, due)) = self.timeouts.iter().enumerate().min_by_key(|(_, due)| (due.deadline, due.row)) {
            if !self.closed && self.plant != RunPlant::TickBeforeSibling && self.flights.iter().any(|flight| !flight.expired && flight.deadline <= due.deadline) {
                break;
            }
            let due = self.timeouts.remove_swap(at);
            let Some(flight) = self.flights.iter_mut().find(|flight| flight.row == due.row) else {
                return Err(RunError::Unasked { suite, row: due.row });
            };
            flight.expired = false;
            let end = ChildEnd {
                row: due.row, test: due.test, micros: due.micros, how: How::TimedOut { after: due.after }, keep: due.keep,
                text: "", entered: &[], reads: &[], output: Captured { digest: due.digest, total: due.total, tail: &due.tail },
            };
            ended(&end, &mut self.flights, &mut self.asks, &mut self.digests, suite, machine, answers, out, now, self.alloc)?;
            self.answered += 1;
        }
        Ok(())
    }

    /// The zygote's bounds: past one it is killed and the command stops naming it.
    fn watch(&mut self, suite: SuiteId, now: Nanos) -> Result<(), RunError> {
        let budget = self.budget;
        let mut due = match self.phase {
            Phase::Starting { at } if now >= at.after(START_DEADLINE) => Some(RunError::ZygoteNotReady { suite, waited: START_DEADLINE }),
            Phase::Ready { at } => self
                .flights
                .iter()
                .filter(|flight| !flight.expired && now >= answer_due(at, flight, budget, self.zygotes.parked(flight.row)))
                .min_by_key(|flight| flight.row)
                .map(|flight| RunError::ZygoteSilent { suite, row: flight.row, test: flight.test }),
            _ => None,
        };
        if let (None, Some((_, at)), false) = (&due, self.exited, self.closed)
            && now >= at.after(CHANNEL_GRACE)
        {
            due = Some(RunError::ChannelHeld { suite });
        }
        match due {
            Some(error) => {
                self.zygotes.kill()?;
                self.reset();
                Err(error)
            }
            None => Ok(()),
        }
    }

    /// Tells the zygote its suite is over and waits for its exit, bounded by [`END_DEADLINE`].
    fn end_zygote(&mut self) -> Result<(), RunError> {
        let Some(suite) = self.suite else { return Ok(()) };
        if matches!(self.phase, Phase::Idle | Phase::Cannot(_)) {
            self.reset();
            return Ok(());
        }
        if !self.flights.is_empty() {
            return Err(RunError::EndWithRuns { suite, runs: self.flights.len() as u32 });
        }
        if self.exited.is_some() || self.closed {
            // It exited, or let its channel go, before it was told the suite was over.
            let died = self.died(suite);
            self.reset();
            return Err(died);
        }
        self.zygotes.end()?;
        let at = self.zygotes.now();
        self.phase = Phase::Ending { at };
        let until = at.after(END_DEADLINE);
        // Each pass waits until `until` at most; the loop ends at the exit or past `until`.
        loop {
            if let (true, Some((status, _))) = (self.closed, self.exited) {
                self.reset();
                return match status {
                    ZygoteExit::Exited { code: 0 } => Ok(()),
                    status => Err(RunError::ZygoteDied { suite, status: Some(status), waiting: 0, first: None }),
                };
            }
            if self.zygotes.now() >= until {
                self.zygotes.kill()?;
                self.reset();
                return Err(RunError::ZygoteStayed { suite, waited: END_DEADLINE });
            }
            self.zygotes.wait(until)?;
            let now = self.zygotes.now();
            let mut heard_end = None;
            for at in 0..self.zygotes.heard_len() {
                match self.zygotes.heard(at) {
                    // A zygote told to end while it still placed the suite says it is ready first.
                    Some(Heard::Ready) => {}
                    Some(Heard::Closed) => self.closed = true,
                    Some(Heard::Exited(status)) => self.exited = Some((status, now)),
                    Some(Heard::Ended(end)) => heard_end = Some(RunError::Unasked { suite, row: end.row }),
                    Some(Heard::CannotBuild { row, .. }) => heard_end = Some(RunError::Unasked { suite, row }),
                    None => heard_end = Some(RunError::HeardPast { at }),
                }
            }
            self.zygotes.clear();
            if let Some(error) = heard_end {
                return Err(error);
            }
        }
    }
}

fn take_flight(flights: &mut AVec<'_, Flight>, suite: SuiteId, row: u32, test: TestId) -> Result<Flight, RunError> {
    let Some(at) = flights.iter().position(|flight| flight.row == row && !flight.expired) else { return Err(RunError::Unasked { suite, row }) };
    if flights[at].test != test {
        return Err(RunError::WrongTest { suite, row });
    }
    Ok(flights.remove_swap(at))
}

/// One run's end, stepped into the loop.
#[expect(clippy::too_many_arguments, reason = "the runner's fields, borrowed apart from the zygote's batch")]
fn ended<'a, 'r>(
    end: &ChildEnd<'_>,
    flights: &mut AVec<'_, Flight>,
    asks: &mut AVec<'_, Ask>,
    digests: &mut AVec<'_, (ReadName, ReadDigest)>,
    suite: SuiteId,
    machine: &mut TestLoop<'a, 'r>,
    answers: &mut dyn Answers,
    out: &mut AVec<'_, Effect<'a>>,
    now: Nanos,
    alloc: Alloc<'_>,
) -> Result<(), RunError> {
    let flight = take_flight(flights, suite, end.row, end.test)?;
    let mut killed = flight.killed;
    if !killed && matches!(end.how, How::TimedOut { .. }) {
        // The pool kept the allowance from the fork; the plan kills at its own deadline.
        let mut kills: AVec<'_, Effect<'a>> = Vec::new_in(alloc);
        machine.step(Event::Tick, flight.deadline, &mut kills)?;
        kills.reverse();
        while let Some(effect) = kills.pop() {
            match effect {
                Effect::Kill { test, .. } if test == end.test => killed = true,
                Effect::Kill { test, .. } => match flights.iter_mut().find(|flight| flight.test == test) {
                    Some(other) => other.killed = true,
                    None => return Err(RunError::KillUnasked { suite, test }),
                },
                other => out.push(other),
            }
        }
        if !killed {
            return Err(RunError::NotKilled { suite, row: end.row });
        }
    }
    let exit = if killed { Exit::Killed } else { end.exit() };
    let ran = Ran { exit, keep: end.keep, micros: end.micros };
    asks.clear();
    digests.clear();
    if !killed && matches!(end.how, How::Reported(_)) {
        for name in end.entered {
            asks.push(Ask::Fn(*name));
        }
        for name in end.reads {
            asks.push(Ask::Read(*name));
        }
        let (address, held) = answers.answer(suite, end.test, end.entered, end.reads, digests)?;
        let ended = TestEnded {
            test: end.test,
            ran,
            detail: end.detail(),
            asks: asks.as_slice(),
            reads: digests.as_slice(),
            address: Some(address),
            held,
            output: end.output,
        };
        machine.step(Event::Ended(ended), now, out)?;
        return Ok(());
    }
    // Its marks were not read (killed, a signal, no whole report): reported, not filed.
    let detail = if killed { Detail::NONE } else { end.detail() };
    let ended = TestEnded { test: end.test, ran, detail, asks: &[], reads: &[], address: None, held: AtAddress::EMPTY, output: end.output };
    machine.step(Event::Ended(ended), now, out)?;
    Ok(())
}

#[cfg(test)]
#[path = "tests/test_run_tests.rs"]
mod tests;
