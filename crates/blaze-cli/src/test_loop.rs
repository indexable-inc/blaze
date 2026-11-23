//! `blaze test`'s drive loop (docs/v2/plan/design/test.md, "End state", piece 16): one request's
//! test suites, from their records to the command's status, as a sans-IO state machine. Events
//! in ([`Event`]), effects out ([`Effect`]), the form of blaze-drive's request machine
//! (`Request::step`): the loop reads no clock (each step is given `now`), opens no file, starts
//! no process and hashes nothing. Its driver maps a suite to its store keys, answers asks, starts
//! and kills test processes (the zygote and child: blaze-run `zygote`, `child`) and writes what
//! the effects say, in their order.
//!
//! Per suite (a test unit), in the driver's order:
//! 1. [`Effect::Open`]: the suite's tests record (the fifth section of its `CrateScope` payload,
//!    names/sections.rs), each test's id (its `TestRun` base key, computed by the driver), its
//!    head record and its postings record (none: the suite never ran). Answered by
//!    [`Event::Opened`].
//! 2. With a head, [`Effect::Moved`]: the asks whose answers moved since the head (the
//!    functions whose code identity moved, from the check's settled records; the reads through
//!    `Head::moved_reads`). Answered by [`Event::Moved`]. Without a head nothing is kept, so
//!    nothing is asked.
//! 3. The suite's [`Plan`] decides: kept passes are reported at once, candidates are looked up
//!    ([`Effect::Look`], answered by [`Event::Looked`]), the rest run ([`Effect::Run`], answered
//!    by [`Event::Ended`] or [`Event::CannotRun`]), each with a deadline the driver keeps by
//!    calling [`TestLoop::next_deadline`] and stepping [`Event::Tick`]; a run past it is killed
//!    ([`Effect::Kill`]) and fails naming the deadline; the others go on.
//! 4. Each run's result is filed by blaze-run result.rs's rule ([`result::file`]): a kept pass
//!    in its address's pass slot, a failure in the failure slot BESIDE it, never over it; both at
//!    one address is one `Flaky` finding ([`Effect::Flaky`]). Nothing is run twice (rule 3).
//! 5. When a row moved (a run, an unsettled pass), the new head and postings are written
//!    ([`Effect::WriteHead`], [`Effect::WritePostings`]) after every [`Effect::File`] before them.
//!
//! The live driver supplies all selected rows to one bounded plan with [`TestLoop::begin_inventory`].
//! Each row keeps its suite and its position for reporting. A first-call code refusal clears
//! that row's previous pass and leaves the rest eligible. Reports are printed in inventory
//! order. [`Effect::Done`] includes test failures, tests that could not run and check errors.

#![cfg_attr(not(test), allow(dead_code, reason = "wired by the driver's lane: main.rs `command`'s test arm builds a `TestLoop` and performs its effects"))]

use core::fmt::{self, Write as _};

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_run::head::{self, Head, HeadError, HeadRow, ReadDigest};
use blaze_run::plan::{self, Address, Asked, Config, Exit, Failure, Kept, Last, Outcome, Plan, PlanError, Ran};
use blaze_run::postings::{self, Ask, Postings, PostingsError, ReadName};
use blaze_run::report::{self, Ending, Entry, Format, NotRun, ReportError, Style};
use blaze_run::result::{self, AtAddress, Captured, Ended, Failed, Filing, Finding, Passed, Place, Refusal, ResultError, Slot, Verdict};
use blaze_run::{CheckoutKey, Nanos, TestId};
use blaze_scope::tests_record::{self, TestsRecord, TestsRecordError};

/// A suite, by its place in the request's list of test units (the driver's order).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct SuiteId(pub u32);

/// The request's settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LoopConfig<'r> {
    /// From test_args.rs `TestArgs::config`.
    pub plan: Config<'r>,
    pub style: Style,
    /// `--list`: list what the filters let through; run nothing.
    pub list: bool,
    /// The checkout this request runs on: how a result and a finding name the run.
    pub run: CheckoutKey,
    /// How many suites the request asks.
    pub suites: u32,
}

/// Why a run the plan wanted could not be started at all. Never a pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CannotRun {
    /// No code for test bodies: the builder does not build them yet (test.md piece 8).
    BodiesNotBuilt,
    /// No zygote runs tests on this host yet (test.md piece 9).
    NoRunner,
    /// No test sandbox is built for this host (Linux), and no test runs unsandboxed (rule 9):
    /// test_run.rs `Runner` answers every run so without starting anything.
    NoSandbox,
    /// blaze refused the test's entry (blaze-ladder run/special/test_roots.rs): its words are
    /// printed on the line before the `NOT RUN` line (test_drive.rs `drive`).
    EntryRefused,
}

impl CannotRun {
    pub(crate) const fn words(self) -> &'static str {
        match self {
            CannotRun::BodiesNotBuilt => "the code builder cannot build test bodies yet (test.md, piece 8)",
            CannotRun::NoRunner => "no zygote runs tests on this host yet (test.md, piece 9)",
            CannotRun::NoSandbox => "no test sandbox is built for Linux yet, and blaze runs no test unsandboxed (rule 9; test.md, piece 9)",
            CannotRun::EntryRefused => "blaze refuses its entry: the line before this one says why",
        }
    }
}

/// A test's process ended: what blaze-run's `Plan::ended` needs (`ran`), what the report needs
/// beyond it (`detail`, `output`), and what filing needs (`asks`, `reads`, `address`, `held`).
/// The zygote reads the child's descriptor-3 report (`TestEnded{test, verdict, entered}` on the
/// wire) and the driver answers the entered asks now to compute the address and read what it
/// holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TestEnded<'e> {
    pub test: TestId,
    /// How it ended (`Exit::Killed` only after an [`Effect::Kill`]), whether its pass may be
    /// kept (the launcher's wrappers and the sandbox), its micros.
    pub ran: Ran<'e>,
    pub detail: Detail<'e>,
    /// Every function it entered (its mark stubs) and every outside read it made, in any order.
    pub asks: &'e [Ask],
    /// Each outside read it made and the digest it answered.
    pub reads: &'e [(ReadName, ReadDigest)],
    /// H(test, asks, answers now); `None` when the run's marks were not read (a kill before its
    /// report): such a failure is reported and not filed.
    pub address: Option<Address>,
    /// What `address` holds now: its pass and failure slots, opened (`AtAddress::EMPTY` when
    /// none).
    pub held: AtAddress<'e>,
    /// Its captured output.
    pub output: Captured<'e>,
}

/// What a failure's report says beyond the exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Detail<'e> {
    /// A panic's item-relative place (rule 71).
    pub place: Option<Place>,
    /// The same place as printed (`src/lib.rs:3:5`), made by the driver.
    pub at: Option<&'e str>,
    /// With `Exit::Refused`: what the sandbox refused, by kind, and the refused thing.
    pub refusal: Option<(Refusal, &'e str)>,
    /// With `Exit::NoReport`: how many bytes of a report came before it tore.
    pub torn: u64,
}

impl Detail<'_> {
    pub(crate) const NONE: Detail<'static> = Detail { place: None, at: None, refusal: None, torn: 0 };
}

/// What happened. `'r`: records the driver keeps mapped for the whole command; `'e`: this
/// event's own data.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Event<'r, 'e> {
    /// [`Effect::Open`]'s answer: the tests record's bytes (16-aligned), each test's id in its
    /// row order, the head and postings records (`None`: the store has none).
    Opened { suite: SuiteId, tests: &'r [u8], ids: &'r [TestId], head: Option<&'r [u8]>, postings: Option<&'r [u8]> },
    /// [`Effect::Moved`]'s answer: the asks whose answers moved since the head, in any order.
    Moved { suite: SuiteId, moved: &'e [Ask] },
    /// [`Effect::Look`]'s answer: whether a pass is filed at the address the test's last asks
    /// answer now.
    Looked { test: TestId, found: bool },
    /// [`Effect::Run`]'s answer: the run ended.
    Ended(TestEnded<'e>),
    /// [`Effect::Run`]'s answer: the run could not be started.
    CannotRun { test: TestId, why: CannotRun },
    /// A first call could not be built. The process has been killed and reaped.
    CannotBuild { suite: SuiteId, test: TestId, why: &'e crate::test_drive::InstanceRefusal },
    /// The driver's clock reached a deadline [`TestLoop::next_deadline`] named.
    Tick,
    /// The program's check ended with this many error findings (zero: it passed). Once.
    Checked { errors: u32 },
}

/// What the driver is to do, in order.
#[derive(Debug)]
pub(crate) enum Effect<'a> {
    /// Open the suite's records; answer [`Event::Opened`].
    Open { suite: SuiteId },
    /// Answer the head's asks now; answer [`Event::Moved`].
    Moved { suite: SuiteId },
    /// Answer the test's last asks (its head row's) now, look the address up; answer
    /// [`Event::Looked`].
    Look { suite: SuiteId, test: TestId, row: u32 },
    /// Run row `row` of the suite's tests record as its own process. Answer [`Event::Ended`] or
    /// [`Event::CannotRun`]. Runs started in one step may be forked as one batch. `deadline` is
    /// the plan's, absolute: the driver hands the pool (`zygote::Pool::ask`) the per-test
    /// allowance (`Config::deadline`), counted from the fork. On the pool's `TimedOut` the driver
    /// steps [`Event::Tick`] at this `deadline` (never one made from `after`) before the end.
    Run { suite: SuiteId, test: TestId, row: u32, deadline: Nanos },
    /// The run is past its deadline: write its threads' stacks, kill its process group, then
    /// answer [`Event::Ended`] with `Exit::Killed`.
    Kill { suite: SuiteId, test: TestId },
    /// Write `bytes` (a result record) into `slot` of `address`; a slot is written once.
    File { suite: SuiteId, test: TestId, address: Address, slot: Slot, bytes: AVec<'a, u8> },
    /// File this `Flaky` finding (rule 81) on the test's item.
    Flaky { suite: SuiteId, finding: Finding },
    /// Print these bytes on the command's output.
    Print { text: AVec<'a, u8> },
    /// Replace the suite's head record, after every `File` before it.
    WriteHead { suite: SuiteId, bytes: AVec<'a, u8> },
    /// Replace the suite's postings record, after every `File` before it.
    WritePostings { suite: SuiteId, bytes: AVec<'a, u8> },
    /// The request is answered.
    Done { status: Status },
}

/// The command's verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Status {
    /// Tests that passed (ran or cached).
    pub passed: u32,
    /// Tests that ran and failed.
    pub failed: u32,
    /// Tests ignored or benches skipped.
    pub ignored: u32,
    /// Tests the plan wanted run that could not be.
    pub not_run: u32,
    /// The check's error findings.
    pub check_errors: u32,
}

impl Status {
    /// Passed only when nothing failed, nothing was left unrun and the check found no error.
    pub(crate) const fn passed(&self) -> bool {
        self.failed == 0 && self.not_run == 0 && self.check_errors == 0
    }
}

/// A protocol or record error: the loop stops. Each names the suite or the test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum LoopError {
    /// An event that does not answer what the loop waits for.
    OutOfTurn { got: &'static str, waiting: &'static str },
    Tests { suite: SuiteId, error: TestsRecordError },
    /// The ids given are not one per tests-record row.
    Ids { suite: SuiteId, ids: usize, rows: usize },
    TwoIds { suite: SuiteId, test: TestId },
    Head { suite: SuiteId, error: HeadError },
    Postings { suite: SuiteId, error: PostingsError },
    Plan(PlanError),
    Report(ReportError),
    /// The shared report did not write its documented closing counts line.
    SummaryShape,
    Result { test: TestId, error: ResultError },
    /// An event names a test the current suite does not hold.
    UnknownTest { test: TestId },
    /// A kept pass ended with no address to file it at.
    NoAddress { test: TestId },
    /// `Exit::Refused` with no refusal named.
    RefusedUnnamed { test: TestId },
    /// The plan's verdict does not fit the exit (a panic verdict without a panic).
    Verdict { test: TestId },
    /// [`Event::Checked`] twice.
    CheckedTwice,
}

/// The first eight bytes of an id, in hex: how an error names a test.
struct Short<'b>(&'b [u8; 32]);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..8] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopError::OutOfTurn { got, waiting } => write!(f, "blaze test's loop got {got} while it waits for {waiting} (rule: test_loop.rs)"),
            LoopError::Tests { suite, error } => write!(f, "test suite {}: {error}", suite.0),
            LoopError::Ids { suite, ids, rows } => write!(f, "test suite {}: {ids} test ids for {rows} tests-record rows", suite.0),
            LoopError::TwoIds { suite, test } => write!(f, "test suite {}: two tests have the id {}", suite.0, Short(&test.0)),
            LoopError::Head { suite, error } => write!(f, "test suite {}: its head record: {error}", suite.0),
            LoopError::Postings { suite, error } => write!(f, "test suite {}: its postings record: {error}", suite.0),
            LoopError::Plan(error) => write!(f, "blaze test's plan: {error}"),
            LoopError::Report(error) => write!(f, "blaze test's report: {error}"),
            LoopError::SummaryShape => f.write_str("blaze test: the report is missing its closing test-result counts"),
            LoopError::Result { test, error } => write!(f, "test {}'s result record: {error}", Short(&test.0)),
            LoopError::UnknownTest { test } => write!(f, "an answer names test {}, which the current suite does not hold", Short(&test.0)),
            LoopError::NoAddress { test } => write!(f, "test {} passed and is kept, but its end gave no address to file it at", Short(&test.0)),
            LoopError::RefusedUnnamed { test } => write!(f, "test {} was refused by the sandbox, and its end names no refusal", Short(&test.0)),
            LoopError::Verdict { test } => write!(f, "test {}'s verdict does not fit how it ended", Short(&test.0)),
            LoopError::CheckedTwice => f.write_str("blaze test's loop was told the check's result twice"),
        }
    }
}

impl core::error::Error for LoopError {}

impl From<PlanError> for LoopError {
    fn from(error: PlanError) -> LoopError {
        LoopError::Plan(error)
    }
}

impl From<ReportError> for LoopError {
    fn from(error: ReportError) -> LoopError {
        LoopError::Report(error)
    }
}

/// A safeguard taken out, for the simulation's planted-bug controls. Always `None` outside tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A head row's last failure is given to the plan as a kept pass.
    KeepFailures,
    /// A run that could not be started is reported as a pass.
    NotRunAsPass,
}

/// A head row moved in this request.
#[derive(Clone, Copy, Debug)]
struct Change {
    test: TestId,
    /// None removes a row whose code did not build. Its former pass and asks cannot
    /// validate another run, even if an older pass still occupies the same address.
    last: Option<Last>,
    micros: u64,
    /// Its asks' range in the pool; `None`: the old row's asks stand.
    asks: Option<(usize, usize)>,
}

/// The suite being run.
struct Current<'a, 'r> {
    suite: SuiteId,
    /// A live request shares one pool across all suites. Its flat dispatch rows
    /// retain their actual suite for filing results and writing suite heads.
    owners: Option<&'r [SuiteId]>,
    /// Per-suite previous heads when running a shared inventory pool across suites.
    suite_heads: Option<&'r [Head<'r>]>,
    /// The tests record's rows, in row (path) order.
    asked: AVec<'a, Asked<'r>>,
    /// (test id, row), sorted by id.
    by_id: AVec<'a, (TestId, u32)>,
    head: Head<'r>,
    postings: Postings<'r>,
    plan: Plan<'a, 'r>,
    changes: AVec<'a, Change>,
    pool: AVec<'a, Ask>,
    reads: AVec<'a, (ReadName, ReadDigest)>,
}

impl<'r> Current<'_, 'r> {
    fn row_of(&self, test: TestId) -> Result<u32, LoopError> {
        match self.by_id.binary_search_by(|(id, _)| id.cmp(&test)) {
            Ok(at) => Ok(self.by_id[at].1),
            Err(_) => Err(LoopError::UnknownTest { test }),
        }
    }

    fn path_of(&self, test: TestId) -> Result<&'r str, LoopError> {
        Ok(self.asked[self.row_of(test)? as usize].path)
    }

    fn owner(&self, test: TestId) -> Result<SuiteId, LoopError> {
        match self.owners {
            Some(owners) => Ok(owners[self.row_of(test)? as usize]),
            None => Ok(self.suite),
        }
    }

    fn head_of(&self, suite: SuiteId) -> Head<'r> {
        self.suite_heads
            .and_then(|heads| usize::try_from(suite.0).ok().and_then(|i| heads.get(i)).copied())
            .unwrap_or(self.head)
    }

    fn head_row(&self, test: TestId) -> Result<Option<head::RowRef>, LoopError> {
        let suite = self.owner(test)?;
        Ok(self.head_of(suite).row(test))
    }

    /// The run's asks and reads, kept for the new head.
    fn keep_asks(&mut self, ended: &TestEnded<'_>) -> (usize, usize) {
        let start = self.pool.len();
        self.pool.extend_from_slice(ended.asks);
        self.reads.extend_from_slice(ended.reads);
        (start, ended.asks.len())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Not begun.
    Idle,
    Opening(SuiteId),
    Moving(SuiteId),
    Running(SuiteId),
    /// Every suite is done; the check's result is awaited.
    Suites,
    Finished,
}

impl Phase {
    const fn says(self) -> &'static str {
        match self {
            Phase::Idle => "`begin`",
            Phase::Opening(_) => "a suite's records (`Opened`)",
            Phase::Moving(_) => "the moved asks (`Moved`)",
            Phase::Running(_) => "a look-up, a run's end or a tick",
            Phase::Suites => "the check's result (`Checked`)",
            Phase::Finished => "nothing: the request is answered",
        }
    }
}

impl Event<'_, '_> {
    const fn says(&self) -> &'static str {
        match self {
            Event::Opened { .. } => "`Opened`",
            Event::Moved { .. } => "`Moved`",
            Event::Looked { .. } => "`Looked`",
            Event::Ended(_) => "`Ended`",
            Event::CannotRun { .. } => "`CannotRun`",
            Event::CannotBuild { .. } => "`CannotBuild`",
            Event::Tick => "`Tick`",
            Event::Checked { .. } => "`Checked`",
        }
    }
}

/// Text into bytes in the loop's allocator.
struct Text<'v, 'a>(&'v mut AVec<'a, u8>);

enum RefusalReason<'e> {
    BeforeStart(CannotRun),
    FirstCall(&'e crate::test_drive::InstanceRefusal),
}

impl fmt::Write for Text<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// One `blaze test` request. See the module's documentation.
pub(crate) struct TestLoop<'a, 'r> {
    config: LoopConfig<'r>,
    alloc: Alloc<'a>,
    phase: Phase,
    current: Option<Current<'a, 'r>>,
    /// The finished live plan's (test, inventory ordinal) index. The ordinal orders package,
    /// target and path independently, including when one package name prefixes another.
    inventory_rows: Option<AVec<'a, (TestId, u32)>>,
    entries: AVec<'a, Entry<'r>>,
    details: AVec<'a, (u32, AVec<'a, u8>)>,
    not_run: AVec<'a, NotRun<'r>>,
    /// The reason borrows the stopped child's instance only while it is printed into this
    /// request's frame. The report can then outlive every child, even when they finish early.
    not_built: AVec<'a, (SuiteId, TestId, &'r str, AString<'a>)>,
    flaky: AVec<'a, Finding>,
    filtered: u32,
    failed: u32,
    /// `--list`: tests and benches listed.
    listed: (u32, u32),
    checked: Option<u32>,
    /// The plan's effects of one call, translated at once.
    scratch: AVec<'a, plan::Effect>,
    pub(crate) plant: Plant,
}

impl<'a, 'r> TestLoop<'a, 'r> {
    /// A request over `config.suites` suites. Refuses a plan of width 0.
    pub(crate) fn new(config: LoopConfig<'r>, alloc: Alloc<'a>) -> Result<TestLoop<'a, 'r>, LoopError> {
        // The plan's own check, once, before any suite: width 0 would start nothing.
        Plan::new(config.plan, alloc)?;
        Ok(TestLoop {
            config,
            alloc,
            phase: Phase::Idle,
            current: None,
            inventory_rows: None,
            entries: Vec::new_in(alloc),
            details: Vec::new_in(alloc),
            not_run: Vec::new_in(alloc),
            not_built: Vec::new_in(alloc),
            flaky: Vec::new_in(alloc),
            filtered: 0,
            failed: 0,
            listed: (0, 0),
            checked: None,
            scratch: Vec::new_in(alloc),
            plant: Plant::None,
        })
    }

    /// Starts: opens the first suite (or, with none, waits for the check).
    pub(crate) fn begin(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        if self.phase != Phase::Idle {
            return Err(LoopError::OutOfTurn { got: "`begin`", waiting: self.phase.says() });
        }
        self.open(SuiteId(0), out)
    }

    /// Select each raw test path before qualifying its display name, then schedule all selected
    /// rows at one width using any previously persisted suite heads, postings, and moved asks.
    #[expect(clippy::too_many_arguments, reason = "inventory rows, suite heads, postings, and moved asks")]
    pub(crate) fn begin_inventory(
        &mut self,
        asked: AVec<'a, Asked<'r>>,
        owners: &'r [SuiteId],
        suite_heads: &'r [Head<'r>],
        postings: Postings<'r>,
        moved: &[Ask],
        filtered: u32,
        now: Nanos,
        out: &mut AVec<'_, Effect<'a>>,
    ) -> Result<(), LoopError> {
        let pool = SuiteId(u32::MAX);
        if asked.len() != owners.len() {
            return Err(LoopError::Ids { suite: pool, ids: owners.len(), rows: asked.len() });
        }
        let mut by_id = Vec::with_capacity_in(asked.len(), self.alloc);
        for (row, one) in asked.iter().enumerate() {
            let row = u32::try_from(row).map_err(|_| ReportError::TooMany { count: asked.len() })?;
            by_id.push((one.test, row));
        }
        by_id.sort_unstable();
        for pair in by_id.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(LoopError::TwoIds { suite: owners[pair[0].1 as usize], test: pair[0].0 });
            }
        }
        let mut config = self.config.plan;
        config.filter = plan::Filter::All;
        config.skip = &[];
        if config.ignored == plan::Ignored::Only {
            config.ignored = plan::Ignored::Include;
        }
        self.filtered = filtered;
        self.current = Some(Current {
            suite: pool, owners: Some(owners), suite_heads: Some(suite_heads), asked, by_id,
            plan: Plan::new(config, self.alloc)?, head: Head::EMPTY, postings,
            changes: Vec::new_in(self.alloc), pool: Vec::new_in(self.alloc), reads: Vec::new_in(self.alloc),
        });
        self.decide(moved, now, out)
    }

    pub(crate) fn row_detail(&mut self, row: u32, text: AVec<'a, u8>) {
        self.details.push((row, text));
    }

    /// When the driver must step [`Event::Tick`]: the earliest deadline of a run not yet killed.
    /// `None` while no run is out; every other wait is the driver's, each with its own deadline.
    pub(crate) fn next_deadline(&self) -> Option<Nanos> {
        self.current.as_ref().and_then(|current| current.plan.next_deadline())
    }

    /// The request is answered ([`Effect::Done`] was given).
    pub(crate) fn finished(&self) -> bool {
        self.phase == Phase::Finished
    }

    /// One event at the driver's clock `now`.
    pub(crate) fn step(&mut self, event: Event<'r, '_>, now: Nanos, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let out_of_turn = |phase: Phase| LoopError::OutOfTurn { got: event.says(), waiting: phase.says() };
        match (event, self.phase) {
            (Event::Checked { errors }, _) => {
                if self.checked.is_some() {
                    return Err(LoopError::CheckedTwice);
                }
                self.checked = Some(errors);
                self.try_finish(out)
            }
            (Event::Opened { suite, tests, ids, head, postings }, Phase::Opening(want)) if suite == want => {
                self.opened(suite, tests, ids, head, postings, now, out)
            }
            (Event::Moved { suite, moved }, Phase::Moving(want)) if suite == want => self.decide(moved, now, out),
            (Event::Looked { test, found }, Phase::Running(_)) => {
                let current = current_of(&mut self.current, self.phase)?;
                current.plan.looked(test, found, now, &mut self.scratch)?;
                self.apply(None, out)?;
                self.after_run_step(out)
            }
            (Event::Ended(ended), Phase::Running(_)) => {
                let current = current_of(&mut self.current, self.phase)?;
                current.row_of(ended.test)?;
                current.plan.ended(ended.test, ended.ran, now, &mut self.scratch)?;
                self.apply(Some(&ended), out)?;
                self.after_run_step(out)
            }
            (Event::CannotRun { test, why }, Phase::Running(_)) => {
                self.cannot_run(test, RefusalReason::BeforeStart(why), now, out)?;
                self.after_run_step(out)
            }
            (Event::CannotBuild { suite, test, why }, Phase::Running(want)) if suite == want => {
                self.cannot_run(test, RefusalReason::FirstCall(why), now, out)?;
                self.after_run_step(out)
            }
            (Event::Tick, Phase::Running(_)) => {
                let current = current_of(&mut self.current, self.phase)?;
                current.plan.tick(now, &mut self.scratch);
                self.apply(None, out)?;
                self.after_run_step(out)
            }
            (_, phase) => Err(out_of_turn(phase)),
        }
    }

    /// Opens `suite`, or ends the suites when it is past the last.
    fn open(&mut self, suite: SuiteId, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        if suite.0 >= self.config.suites {
            self.phase = Phase::Suites;
            return self.try_finish(out);
        }
        self.phase = Phase::Opening(suite);
        out.push(Effect::Open { suite });
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "one event's fields, each read once")]
    fn opened(
        &mut self,
        suite: SuiteId,
        tests: &'r [u8],
        ids: &'r [TestId],
        head: Option<&'r [u8]>,
        postings: Option<&'r [u8]>,
        now: Nanos,
        out: &mut AVec<'_, Effect<'a>>,
    ) -> Result<(), LoopError> {
        let record = TestsRecord::open(tests).map_err(|error| LoopError::Tests { suite, error })?;
        if ids.len() != record.len() {
            return Err(LoopError::Ids { suite, ids: ids.len(), rows: record.len() });
        }
        let mut asked: AVec<'a, Asked<'r>> = Vec::with_capacity_in(record.len(), self.alloc);
        let mut by_id: AVec<'a, (TestId, u32)> = Vec::with_capacity_in(record.len(), self.alloc);
        for (at, id) in ids.iter().enumerate() {
            let Some(row) = record.row(at).map_err(|error| LoopError::Tests { suite, error })? else {
                return Err(LoopError::Ids { suite, ids: ids.len(), rows: at });
            };
            let should_panic = match row.should_panic {
                tests_record::ShouldPanic::No => plan::ShouldPanic::No,
                tests_record::ShouldPanic::Yes => plan::ShouldPanic::Yes,
                tests_record::ShouldPanic::WithMessage(text) => plan::ShouldPanic::WithText(text),
            };
            asked.push(Asked { test: *id, path: row.path, ignore: row.ignore, should_panic, bench: row.bench });
            by_id.push((*id, at as u32));
        }
        by_id.sort_unstable();
        for pair in by_id.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(LoopError::TwoIds { suite, test: pair[0].0 });
            }
        }
        let plan = Plan::new(self.config.plan, self.alloc)?;
        if self.config.list {
            return self.list(suite, &asked, &plan, out);
        }
        let head = match head {
            Some(bytes) => Head::open(bytes).map_err(|error| LoopError::Head { suite, error })?,
            None => Head::EMPTY,
        };
        let postings = match postings {
            Some(bytes) => Postings::open(bytes).map_err(|error| LoopError::Postings { suite, error })?,
            None => Postings::EMPTY,
        };
        let has_head = !head.is_empty();
        self.current = Some(Current {
            suite,
            owners: None,
            suite_heads: None,
            asked,
            by_id,
            head,
            postings,
            plan,
            changes: Vec::new_in(self.alloc),
            pool: Vec::new_in(self.alloc),
            reads: Vec::new_in(self.alloc),
        });
        if has_head {
            self.phase = Phase::Moving(suite);
            out.push(Effect::Moved { suite });
            return Ok(());
        }
        // Nothing kept: nothing moved that matters.
        self.decide(&[], now, out)
    }

    /// `--list`: each test the filters let through, in path order; nothing runs.
    fn list(&mut self, suite: SuiteId, asked: &[Asked<'r>], plan: &Plan<'_, '_>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let mut text = Vec::new_in(self.alloc);
        let mut sink = Text(&mut text);
        for one in asked {
            if !plan.lets_through(one) {
                continue;
            }
            let kind = if one.bench { "bench" } else { "test" };
            match one.bench {
                true => self.listed.1 += 1,
                false => self.listed.0 += 1,
            }
            // A path is identifiers joined by `::`: nothing in it needs a JSON escape.
            let written = match self.config.style.format {
                Format::Text => fmt::write(&mut sink, format_args!("{}: {kind}\n", one.path)),
                Format::Json => fmt::write(&mut sink, format_args!("{{\"event\":\"listed\",\"path\":\"{}\",\"kind\":\"{kind}\"}}\n", one.path)),
            };
            written.map_err(|_| LoopError::Report(ReportError::Write))?;
        }
        if !text.is_empty() {
            out.push(Effect::Print { text });
        }
        self.open(SuiteId(suite.0 + 1), out)
    }

    /// Hands the plan the asked tests, the head's rows and what moved.
    fn decide(&mut self, moved: &[Ask], now: Nanos, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let plant = self.plant;
        let alloc = self.alloc;
        let current = current_of(&mut self.current, self.phase)?;
        let mut kept: AVec<'_, Kept> = Vec::new_in(alloc);
        if let Some(heads) = current.suite_heads {
            for head in heads {
                head.kept_into(&mut kept);
            }
            kept.sort_unstable_by_key(|row| row.test);
        } else {
            current.head.kept_into(&mut kept);
        }
        if plant == Plant::KeepFailures {
            for row in kept.iter_mut() {
                if row.last == Last::Failed {
                    row.last = Last::Passed { address: Address([0; 32]) };
                }
            }
        }
        let suite = current.suite;
        current.plan.decide(&current.asked, &kept, moved, &current.postings, now, &mut self.scratch)?;
        self.phase = Phase::Running(suite);
        self.apply(None, out)?;
        self.after_run_step(out)
    }

    /// A run that could not be started: reported as not run, never as a pass; its kept pass,
    /// if any, is unsettled (its asks moved and no pass stands at their new answers).
    fn cannot_run(&mut self, test: TestId, why: RefusalReason<'_>, now: Nanos, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let plant = self.plant;
        let current = current_of(&mut self.current, self.phase)?;
        let path = current.path_of(test)?;
        current.plan.not_run(test, now, &mut self.scratch)?;
        if let Some(row) = current.head_row(test)? {
            let last = match why {
                RefusalReason::BeforeStart(_) => Some(head::unsettle(row.kept.last)),
                RefusalReason::FirstCall(_) => None,
            };
            if last != Some(row.kept.last) {
                current.changes.push(Change { test, last, micros: row.kept.micros, asks: None });
            }
        }
        match plant {
            Plant::NotRunAsPass => self.entries.push(Entry { test, path, outcome: Outcome::Passed { ran: plan::Missed::NoRecord, kept: Ok(()) } }),
            Plant::None | Plant::KeepFailures => match why {
                RefusalReason::BeforeStart(why) => self.not_run.push(NotRun { test, path, why: why.words() }),
                RefusalReason::FirstCall(cause) => {
                    let mut words = String::new_in(self.alloc);
                    write!(words, "{cause}").map_err(|_| LoopError::Report(ReportError::Write))?;
                    self.not_built.push((current.owner(test)?, test, path, words));
                }
            },
        }
        self.apply(None, out)
    }

    /// Translates the plan's effects of one call. `ended` is the run whose end the plan judged.
    fn apply(&mut self, ended: Option<&TestEnded<'_>>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let mut effects = core::mem::replace(&mut self.scratch, Vec::new_in(self.alloc));
        let mut result = Ok(());
        for effect in effects.iter() {
            result = self.one(*effect, ended, out);
            if result.is_err() {
                break;
            }
        }
        effects.clear();
        self.scratch = effects;
        result
    }

    fn one(&mut self, effect: plan::Effect, ended: Option<&TestEnded<'_>>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let alloc = self.alloc;
        let run = self.config.run;
        let style = self.config.style;
        let current = current_of(&mut self.current, self.phase)?;
        let suite = current.suite;
        match effect {
            plan::Effect::Look { test } => {
                let row = current.row_of(test)?;
                let owner = current.owner(test)?;
                out.push(Effect::Look { suite: owner, test, row });
            }
            plan::Effect::Start { test, deadline } => {
                let row = current.row_of(test)?;
                out.push(Effect::Run { suite, test, row, deadline });
            }
            plan::Effect::Kill { test } => out.push(Effect::Kill { suite, test }),
            plan::Effect::Unsettle { test } => {
                if let Some(row) = current.head_row(test)? {
                    current.changes.push(Change { test, last: Some(head::unsettle(row.kept.last)), micros: row.kept.micros, asks: None });
                }
            }
            plan::Effect::File { test } => {
                // A kept pass: filed in its address's pass slot.
                let Some(ended) = ended.filter(|ended| ended.test == test) else { return Err(LoopError::NoAddress { test }) };
                let Some(address) = ended.address else { return Err(LoopError::NoAddress { test }) };
                let pass = Verdict::Pass(Passed { run, micros: ended.ran.micros, kept: Ok(()) });
                let asks = current.keep_asks(ended);
                current.changes.push(Change { test, last: Some(Last::Passed { address }), micros: ended.ran.micros, asks: Some(asks) });
                file(current.owner(test)?, test, address, &ended.held, &pass, alloc, out, &mut self.flaky)?;
            }
            plan::Effect::Report { test, outcome } => {
                let path = current.path_of(test)?;
                match (outcome, ended.filter(|ended| ended.test == test)) {
                    (Outcome::Failed { why, .. }, Some(ended)) => {
                        let failed = failed_of(ended, why, run)?;
                        let mut text = Vec::new_in(alloc);
                        report::failure(&mut Text(&mut text), style, &Ending { test, path, failed, at: ended.detail.at })?;
                        if current.owners.is_some() {
                            self.details.push((current.row_of(test)?, text));
                        } else {
                            out.push(Effect::Print { text });
                        }
                        let asks = current.keep_asks(ended);
                        current.changes.push(Change { test, last: Some(Last::Failed), micros: ended.ran.micros, asks: Some(asks) });
                        // Filed BESIDE any pass at its address; a kill before the marks were read
                        // has no address and is reported only.
                        if let Some(address) = ended.address {
                            file(current.owner(test)?, test, address, &ended.held, &Verdict::Fail(failed), alloc, out, &mut self.flaky)?;
                        }
                        self.failed += 1;
                    }
                    (Outcome::Failed { .. }, None) => return Err(LoopError::Verdict { test }),
                    (Outcome::Passed { kept: Err(why), .. }, Some(ended)) => {
                        let asks = current.keep_asks(ended);
                        current.changes.push(Change { test, last: Some(Last::NotKept(why)), micros: ended.ran.micros, asks: Some(asks) });
                    }
                    (Outcome::Passed { .. }, _) | (Outcome::Cached(_) | Outcome::Ignored | Outcome::Bench, _) => {}
                }
                self.entries.push(Entry { test, path, outcome });
            }
        }
        Ok(())
    }

    /// After a step of the running suite: when every test has its outcome, the suite's records
    /// are written and the next suite is opened.
    fn after_run_step(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let done = matches!(&self.current, Some(current) if current.plan.done());
        if !done {
            return Ok(());
        }
        let Some(mut current) = self.current.take() else { return Ok(()) };
        self.filtered += current.plan.filtered_out();
        if current.owners.is_some() {
            let mut changes = Vec::with_capacity_in(current.changes.len(), self.alloc);
            for change in &current.changes {
                changes.push((current.owner(change.test)?, *change));
            }
            changes.sort_unstable_by_key(|(suite, change)| (*suite, change.test));
            for group in changes.chunk_by(|a, b| a.0 == b.0) {
                let owner = group[0].0;
                current.suite = owner;
                current.head = current.head_of(owner);
                current.changes.clear();
                current.changes.extend(group.iter().map(|(_, change)| *change));
                write_records(&mut current, self.alloc, out)?;
            }
            self.details.sort_unstable_by_key(|(row, _)| *row);
            for (_, text) in self.details.drain(..) {
                out.push(Effect::Print { text });
            }
            self.inventory_rows = Some(current.by_id);
            self.phase = Phase::Suites;
            return self.try_finish(out);
        } else if !current.changes.is_empty() {
            write_records(&mut current, self.alloc, out)?;
        }
        self.open(SuiteId(current.suite.0 + 1), out)
    }

    /// Interleave the shared reporter's ordinary results with first-call refusals in
    /// identity order. The ordinary result lines and accounting fields stay verbatim.
    fn ordered_summary(&self, out: &mut AVec<'_, u8>) -> Result<(), LoopError> {
        let style = self.config.style;
        let json = style.format == Format::Json;
        let mut refused = Vec::with_capacity_in(self.not_run.len() + self.not_built.len(), self.alloc);
        let mut order = Vec::with_capacity_in(refused.capacity() + self.entries.len(), self.alloc);
        for &row in &self.not_run {
            refused.push(row);
            order.push((row.path, row.test, Some(false), ""));
        }
        for (_, test, path, why) in &self.not_built {
            // In the text form the driver prints this typed cause itself, which may contain
            // several lines. In JSON the report escapes it into one event.
            refused.push(NotRun { test: *test, path, why: if json { why.as_str() } else { "" } });
            order.push((*path, *test, Some(true), why.as_str()));
        }
        for entry in &self.entries {
            if json || matches!(entry.outcome, Outcome::Failed { .. } | Outcome::Passed { kept: Err(_), .. }) {
                order.push((entry.path, entry.test, None, ""));
            }
        }
        let mut rendered = String::new_in(self.alloc);
        report::summary_with(&mut rendered, style, &self.entries, self.filtered, &self.flaky, &refused, self.alloc)?;
        // summary_with emits the refusals first and the ordinary rows second, each sorted
        // by (path, test), then flaky findings and one closing line. Both iterators borrow
        // the same buffer. Consume one line per typed row without inspecting child output.
        let mut refusals = rendered.as_str().split_inclusive('\n');
        let mut ordinary = refusals.clone().skip(refused.len());
        order.sort_unstable_by_key(|&(path, test, _, _)| (path, test));
        let mut lines = Vec::with_capacity_in(order.len(), self.alloc);
        for (path, test, refusal, why) in order {
            let line = match refusal {
                Some(_) => refusals.next(),
                None => ordinary.next(),
            }.ok_or(LoopError::SummaryShape)?;
            lines.push((self.ordinal(test)?, path, test, line, if !json && refusal == Some(true) { Some(why) } else { None }));
        }
        lines.sort_unstable_by_key(|&(ordinal, path, test, _, _)| (ordinal, path, test));
        for (_, path, _, line, why) in lines {
            if let Some(why) = why {
                let label = if style.colour { "\x1b[1;31mNOT RUN\x1b[0m" } else { "NOT RUN" };
                writeln!(Text(out), "{label} {path}: {why}").map_err(ReportError::from)?;
            } else {
                out.extend_from_slice(line.as_bytes());
            }
        }
        let mut findings = Vec::with_capacity_in(self.flaky.len(), self.alloc);
        findings.extend(self.flaky.iter().map(|finding| finding.flaky.test));
        findings.sort_unstable();
        let mut flaky_lines = Vec::with_capacity_in(findings.len(), self.alloc);
        for test in findings {
            let line = ordinary.next().ok_or(LoopError::SummaryShape)?;
            flaky_lines.push((self.ordinal(test)?, test, line));
        }
        flaky_lines.sort_by_key(|&(ordinal, test, _)| (ordinal, test));
        for (_, _, line) in flaky_lines {
            out.extend_from_slice(line.as_bytes());
        }
        let counts = ordinary.next().ok_or(LoopError::SummaryShape)?;
        let count = self.not_built.len();
        if count == 0 {
            out.extend_from_slice(counts.as_bytes());
        } else if json {
            let counts = counts.strip_suffix("}\n").ok_or(LoopError::SummaryShape)?;
            writeln!(Text(out), "{counts},\"not_built\":{count}}}").map_err(ReportError::from)?;
        } else {
            let (ran, accounting) = counts.split_once("; ").ok_or(LoopError::SummaryShape)?;
            let (accounting, _) = accounting.rsplit_once("; ").ok_or(LoopError::SummaryShape)?;
            write!(Text(out), "{ran}, {count} not run (code not built); {accounting}").map_err(ReportError::from)?;
            if !self.not_run.is_empty() {
                write!(Text(out), "; {} could not be run yet", self.not_run.len()).map_err(ReportError::from)?;
            }
            out.push(b'\n');
        }
        if ordinary.next().is_some() {
            return Err(LoopError::SummaryShape);
        }
        Ok(())
    }

    fn ordinal(&self, test: TestId) -> Result<u32, LoopError> {
        let Some(rows) = &self.inventory_rows else { return Ok(0) };
        rows.binary_search_by_key(&test, |&(id, _)| id)
            .map(|index| rows[index].1)
            .map_err(|_| LoopError::UnknownTest { test })
    }

    /// After the last suite and the check: the summary and the status.
    fn try_finish(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
        let (Phase::Suites, Some(check_errors)) = (self.phase, self.checked) else { return Ok(()) };
        let mut text = Vec::new_in(self.alloc);
        if self.config.list {
            let (tests, benches) = self.listed;
            let written = match self.config.style.format {
                Format::Text => fmt::write(&mut Text(&mut text), format_args!("{tests} tests, {benches} benchmarks\n")),
                Format::Json => fmt::write(&mut Text(&mut text), format_args!("{{\"event\":\"listed_summary\",\"tests\":{tests},\"benches\":{benches}}}\n")),
            };
            written.map_err(|_| LoopError::Report(ReportError::Write))?;
        } else if self.inventory_rows.is_none() && self.not_built.is_empty() {
            report::summary_with(&mut Text(&mut text), self.config.style, &self.entries, self.filtered, &self.flaky, &self.not_run, self.alloc)?;
        } else {
            self.ordered_summary(&mut text)?;
        }
        out.push(Effect::Print { text });
        let mut passed = 0u32;
        let mut ignored = 0u32;
        for entry in self.entries.iter() {
            match entry.outcome {
                Outcome::Cached(_) | Outcome::Passed { .. } => passed = passed.saturating_add(1),
                Outcome::Ignored | Outcome::Bench => ignored = ignored.saturating_add(1),
                Outcome::Failed { .. } => {}
            }
        }
        let not_run = self.not_run.len() + self.not_built.len();
        let status = Status {
            passed,
            failed: self.failed,
            ignored,
            not_run: u32::try_from(not_run).map_err(|_| ReportError::TooMany { count: not_run })?,
            check_errors,
        };
        self.phase = Phase::Finished;
        out.push(Effect::Done { status });
        Ok(())
    }
}

/// The suite being run, borrowing only that field (so the loop's other fields stay free).
fn current_of<'c, 'a, 'r>(current: &'c mut Option<Current<'a, 'r>>, phase: Phase) -> Result<&'c mut Current<'a, 'r>, LoopError> {
    current.as_mut().ok_or(LoopError::OutOfTurn { got: "a run's answer", waiting: phase.says() })
}

/// The failure value of a run the plan judged failed: its ending from the verdict and the exit.
fn failed_of<'e>(ended: &TestEnded<'e>, why: Failure, run: CheckoutKey) -> Result<Failed<'e>, LoopError> {
    let test = ended.test;
    let detail = ended.detail;
    let ending = match (why, ended.ran.exit) {
        (Failure::Panicked, Exit::Panicked { text }) => Ended::Panicked { message: result::capped(text), at: detail.place },
        (Failure::PanicTextMissing, Exit::Panicked { text }) => Ended::PanicTextMissing { message: result::capped(text), at: detail.place },
        (Failure::Panicked | Failure::PanicTextMissing, _) => return Err(LoopError::Verdict { test }),
        (Failure::DidNotPanic, _) => Ended::DidNotPanic,
        (Failure::Signal { number }, _) => Ended::Signal { number },
        (Failure::TimedOut { after }, _) => Ended::TimedOut { after },
        (Failure::Refused, _) => match detail.refusal {
            Some((refusal, what)) => Ended::Refused { refusal, what: result::capped(what) },
            None => return Err(LoopError::RefusedUnnamed { test }),
        },
        (Failure::NoReport { code }, _) => Ended::NoReport { code, torn: detail.torn },
    };
    Ok(Failed { run, micros: ended.ran.micros, ended: ending, output: ended.output })
}

/// Files `verdict` at `address` by result.rs's rule; a new `Flaky` finding is kept and filed.
#[expect(clippy::too_many_arguments, reason = "the filing's parts, each read once")]
fn file<'a>(
    suite: SuiteId,
    test: TestId,
    address: Address,
    held: &AtAddress<'_>,
    verdict: &Verdict<'_>,
    alloc: Alloc<'a>,
    out: &mut AVec<'_, Effect<'a>>,
    flaky: &mut AVec<'_, Finding>,
) -> Result<(), LoopError> {
    let filed = match result::file(test, address, held, verdict) {
        Ok(filed) => filed,
        // A pass not kept never reaches here: the plan files only kept passes.
        Err(result::FileError::NotKept { .. }) => return Err(LoopError::Verdict { test }),
    };
    if let Filing::Write(slot) = filed.filing {
        let bytes = verdict.encode(alloc).map_err(|error| LoopError::Result { test, error })?;
        out.push(Effect::File { suite, test, address, slot, bytes });
    }
    if let Some(finding) = filed.flaky {
        flaky.push(finding);
        out.push(Effect::Flaky { suite, finding });
    }
    Ok(())
}

/// Writes the suite's new head and postings: every old row that did not move, every row that
/// did, and the reads they ask (a read's newest digest wins).
fn write_records<'a>(current: &mut Current<'_, '_>, alloc: Alloc<'a>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), LoopError> {
    let suite = current.suite;
    current.changes.sort_by_key(|change| change.test);
    let mut all: AVec<'_, Ask> = Vec::new_in(alloc);
    let mut spec: AVec<'_, (TestId, Last, u64, usize, usize)> = Vec::new_in(alloc);
    let mut olds = current.head.rows().peekable();
    let mut next_change = 0;
    loop {
        let old = olds.peek().copied();
        let change = current.changes.get(next_change).copied();
        let (row, change) = match (old, change) {
            (None, None) => break,
            (Some(row), Some(change)) if row.kept.test == change.test => {
                olds.next();
                next_change += 1;
                (Some(row), Some(change))
            }
            (Some(row), Some(change)) if row.kept.test < change.test => {
                olds.next();
                (Some(row), None)
            }
            (Some(row), None) => {
                olds.next();
                (Some(row), None)
            }
            (_, Some(change)) => {
                next_change += 1;
                (None, Some(change))
            }
        };
        let start = all.len();
        let (test, last, micros) = match (row, change) {
            (_, Some(Change { last: None, .. })) => continue,
            (_, Some(Change { test, last: Some(last), micros, .. })) => (test, last, micros),
            (Some(row), None) => (row.kept.test, row.kept.last, row.kept.micros),
            (None, None) => break,
        };
        match (change.and_then(|change| change.asks), row) {
            (Some((from, len)), _) => all.extend_from_slice(&current.pool[from..from + len]),
            (None, Some(row)) => {
                for ask in current.head.asks(row) {
                    all.push(ask);
                }
            }
            // An unsettled row with no old row cannot be: an unsettle names a head row.
            (None, None) => return Err(LoopError::UnknownTest { test }),
        }
        spec.push((test, last, micros, start, all.len() - start));
    }
    let mut rows: AVec<'_, HeadRow<'_>> = Vec::with_capacity_in(spec.len(), alloc);
    let mut pairs: AVec<'_, (Ask, TestId)> = Vec::with_capacity_in(all.len(), alloc);
    for (test, last, micros, start, len) in spec.iter().copied() {
        let asks = &all[start..start + len];
        rows.push(HeadRow { test, last, micros, asks });
        for ask in asks {
            pairs.push((*ask, test));
        }
    }
    // The reads: this request's first (newest), then the head's; one per name; only those asked.
    let mut reads: AVec<'_, (ReadName, u8, ReadDigest)> = Vec::new_in(alloc);
    for (name, digest) in current.reads.iter() {
        reads.push((*name, 0, *digest));
    }
    for (name, digest) in current.head.reads() {
        reads.push((name, 1, digest));
    }
    reads.sort_unstable();
    reads.dedup_by(|later, first| later.0 == first.0);
    let mut asked_reads: AVec<'_, ReadName> = Vec::new_in(alloc);
    for ask in all.iter() {
        if let Ask::Read(name) = ask {
            asked_reads.push(*name);
        }
    }
    asked_reads.sort_unstable();
    asked_reads.dedup();
    let mut kept_reads: AVec<'_, (ReadName, ReadDigest)> = Vec::with_capacity_in(reads.len(), alloc);
    for (name, _, digest) in reads.iter() {
        if asked_reads.binary_search(name).is_ok() {
            kept_reads.push((*name, *digest));
        }
    }
    let head_bytes = head::encode(&rows, &kept_reads, alloc).map_err(|error| LoopError::Head { suite, error })?;
    let postings_bytes = postings::encode(&pairs, alloc).map_err(|error| LoopError::Postings { suite, error })?;
    out.push(Effect::WriteHead { suite, bytes: head_bytes });
    out.push(Effect::WritePostings { suite, bytes: postings_bytes });
    Ok(())
}

#[cfg(test)]
#[path = "tests/test_loop_tests.rs"]
mod tests;
