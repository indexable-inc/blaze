//! THE DRIVER OF `blaze test` (docs/v2/plan/design/test.md, "End state"): test_loop.rs's
//! `TestLoop` (the sans-IO loop) driven on the command's edge, every `Effect::Run` and
//! `Effect::Kill` handed to test_run.rs's `Runner` (deadlines, kills, verdicts, filing), and the
//! runner's `Zygotes` seam answered by [`OneEach`]: EACH TEST IN A PROCESS OF ITS OWN, started
//! the way `blaze run` starts a program (rule 30): the lazy parent (run_parent.rs `Lazy`, its
//! first calls answered over descriptor 3 by `Needs::serve`), the launcher beside this
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
//! Up to the plan's width of children run at once. Their parents take turns serving first
//! calls on the command's thread. The floor of one test's run is a fork of a warm zygote plus its first
//! calls' answers found in the store; today's cost is an exec of the launcher per test, which
//! the zygote (piece 9) removes. The time of each failing test is in its failure line.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write as _};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
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
use blaze_scope::tests_record::TestsRecordError;
use blaze_screen::Watch;
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::Deadline;
use bstd::fs::{Dir, Durable, FsError, FsKind, ReadWrite};
use bstd::io::{Io, Missing};
use bstd::mem::Frame;
use bstd::path::{Path, PathError};
use bstd::procs::{ChannelError, Ended, GRACE, Held, Message, OutPipe, ParentDeath, ParentSignals, Read as Piped, Relay, RelayAct, RunChild, RunError as ProcError, RunFault, RunSpec, SignalError, Spawner, Streams};

use crate::code_store::HomeCodeStore;
use crate::run_names::{Found, TestRows, TestUnit};
use crate::run_ids::{Checked, Grain, RunIds};
use crate::run_loop::LoopError as RunLoopError;
use crate::run_parent::{self, Front, Gate, Lazy, Needs, ParentError, RECORDS, RecordsDir, Start as _, WRITE_BOUND};
use crate::store::CommandLent;
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
/// Beneath `<HOME>`: the parent of each command's suite and row folders.
const TEMPS: &str = ".blaze/v2/test/tmp";

/// Why the driver stopped the command: each names the step and the thing (rule 19).
#[derive(Debug)]
pub(crate) enum DriveError {
    Loop(LoopError),
    Run(RunError),
    /// A test's run failed in blaze (not in the test), or the run's setup did.
    One(OneError),
    /// A test's own process could not be run, in blaze: the line above names the test and the
    /// cause ([`run_suites`] prints it).
    OneStopped,
    /// A capability the command needs is not in its `Io`.
    Missing(Missing),
    /// The loop asked for a head's answers, but every suite is opened without a head.
    NoHead { effect: &'static str },
    /// The loop asked to open a suite this request does not have.
    NoSuite { suite: SuiteId },
    TooManyRows { count: usize },
    /// The tests record could not be read.
    Record(TestsRecordError),
    /// Nothing runs, nothing is owed and no status came.
    Stalled,
    /// More steps than [`STEPS_PER_TEST`] per test.
    Steps { most: usize },
    /// The output stream refused a write.
    Print,
    /// A pinned child's storage must come from the caller's process heap.
    ChildHeap,
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
            DriveError::NoSuite { suite } => write!(f, "blaze test: suite {} is outside the selected inventory", suite.0),
            DriveError::TooManyRows { count } => write!(f, "blaze test: {count} test rows exceed the runner's u32 row limit"),
            DriveError::Record(error) => write!(f, "blaze test: {error}"),
            DriveError::Stalled => f.write_str("blaze test: the loop waits for nothing, runs nothing and gave no status (rule 18)"),
            DriveError::Steps { most } => write!(f, "blaze test: the loop took more than {most} steps (rule 18)"),
            DriveError::Print => f.write_str("blaze test: the output stream refused a write"),
            DriveError::ChildHeap => f.write_str("blaze test: the child pool needs the process heap to pin its lazy parents"),
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
    CannotBuild(InstanceRefusal),
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
            OneError::CannotBuild(refusal) => refusal.fmt(f),
            OneError::Launcher(error) => write!(f, "the launcher: {error}"),
            OneError::Profile { why } => write!(f, "a test's sandbox profile: {why}"),
            OneError::Start(error) => write!(f, "a test's launcher did not start under {SANDBOX_EXEC}: {error}"),
            OneError::Pipe(error) => write!(f, "a test's output: {error}"),
            OneError::Loop(error) => write!(f, "a test's run: {error}"),
            OneError::Signals(error) => write!(f, "a test's run: the parent's signals: {error}"),
        }
    }
}

/// A particular first call for which the parent could not produce code. Kept typed until
/// the plan has matched the answer to the flight for this suite and tests-record row.
#[derive(Debug)]
pub(crate) struct InstanceRefusal {
    instance: crate::run_fault::Named,
    symbol: [u8; 32],
    site: Option<InstanceSite>,
    cause: ParentError,
}

#[derive(Debug)]
enum InstanceSite {
    Source(crate::run_fault::SiteShown),
    Item { item: ItemKey, byte: u32 },
}

impl fmt::Display for InstanceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::run_fault::Named;
        match &self.instance {
            Named::Shown { item, args } => {
                write!(f, "{item}")?;
                if let Some(args) = args {
                    write!(f, " {args}")?;
                }
            }
            Named::Path(path) => f.write_str(path)?,
            Named::Item(item) => write!(f, "fn {:#x}", item.0)?,
            Named::NoRow => {
                f.write_str("symbol ")?;
                for byte in self.symbol {
                    write!(f, "{byte:02x}")?;
                }
            }
        }
        match &self.site {
            Some(InstanceSite::Source(site)) => write!(f, " at {}:{}:{}", site.file, site.line, site.column)?,
            Some(InstanceSite::Item { item, byte }) => write!(f, " at byte {byte} of item {:#x}", item.0)?,
            None => {}
        }
        match &self.cause {
            ParentError::Walk {
                why: blaze_ladder::body_ids::WalkError::Make { why: blaze_ladder::run::RunError::Build { error, .. }, .. }, ..
            } => write!(f, ": {error}"),
            ParentError::Walk { why: blaze_ladder::body_ids::WalkError::Make { why, .. }, .. } => write!(f, ": {why}"),
            ParentError::Special { why, .. } => write!(f, ": {why}"),
            cause => write!(f, ": {cause}"),
        }
    }
}

/// Only a refusal of the requested instance belongs to its test. A broken shared record,
/// an unanswered names-record read, a wire error, or an invariant of the parent stops the
/// command. Keep the walk alive until the instance and item-relative site have been named.
fn test_parent_error<F: Front, D: run_parent::Folder>(lazy: &mut Lazy<'_, '_, F, D>, cause: ParentError) -> OneError {
    let Some((symbol, location)) = refused_instance(&cause) else { return OneError::Parent(cause) };
    let site = location.map(|(item, byte)| match lazy.site_of(item, byte) {
        Some(shown) => InstanceSite::Source(shown),
        None => InstanceSite::Item { item, byte },
    });
    OneError::CannotBuild(InstanceRefusal { instance: lazy.name_of(symbol), symbol, site, cause })
}

fn refused_instance(cause: &ParentError) -> Option<([u8; 32], Option<(ItemKey, u32)>)> {
    use blaze_ladder::body_ids::WalkError;
    use blaze_ladder::code_records::CodeRecordError;
    use blaze_ladder::run::RunError;

    Some(match cause {
        ParentError::Walk { symbol, why: WalkError::Make { tabled, why } } => {
            let byte = match why {
                RunError::Build { range, .. } => range.map_or(0, |range| range.start),
                RunError::ItemRefused { .. } | RunError::NoAdt { .. } | RunError::Emit { .. }
                | RunError::Callee { .. } | RunError::Projection { .. } | RunError::LeftOut { .. }
                | RunError::ConstAt { .. } | RunError::Resolve { .. } | RunError::NoBuiltinCall { .. }
                | RunError::Vtable { .. } | RunError::StaticRecord { .. }
                | RunError::Check { .. } | RunError::Types { .. } | RunError::Const { .. } | RunError::Need { .. }
                | RunError::OneBody { .. }
                | RunError::CodeRecord { error: CodeRecordError::Emit(_) | CodeRecordError::Routine(_), .. } => 0,
                RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Literal { range, .. }) => range.start,
                RunError::Front2(why) if instance_front_refusal(why) => 0,
                _ => return None,
            };
            (*symbol, Some((*tabled, byte)))
        }
        ParentError::Walk { symbol, why: WalkError::NoBody { tabled } } => (*symbol, Some((*tabled, 0))),
        ParentError::Special {
            symbol,
            why: CodeRecordError::Emit(_) | CodeRecordError::Getter(_) | CodeRecordError::Ender(_) | CodeRecordError::Shim(_) | CodeRecordError::Routine(_),
        } => (*symbol, None),
        ParentError::NotChecked { function, symbol, .. } => (*symbol, Some((*function, 0))),
        ParentError::NoPersonality { function, symbol, .. } => (*symbol, function.map(|item| (item, 0))),
        _ => return None,
    })
}

fn instance_front_refusal(why: &blaze_ladder::front2::program::Front2Refusal) -> bool {
    use blaze_ladder::front2::check::CheckFail;
    use blaze_ladder::front2::program::{Front2Refusal as Front, lazy::LazyRefusal};
    match why {
        Front::Lazy(LazyRefusal::Row { refusal, .. }) => instance_front_refusal(refusal),
        Front::Lazy(LazyRefusal::Check { error, .. }) | Front::Asked { error, .. } | Front::Check(error) =>
            !matches!(error, CheckFail::Tables { .. } | CheckFail::ReadsDiffer { .. }),
        Front::Lazy(LazyRefusal::Lower { .. } | LazyRefusal::Spell { .. }
            | LazyRefusal::LocalNotBuilt { .. } | LazyRefusal::Diagnostic { .. }
            | LazyRefusal::Head { .. } | LazyRefusal::Adt { .. } | LazyRefusal::Const { .. }
            | LazyRefusal::NotLazyYet { .. } | LazyRefusal::NeedsSolvedNames { .. } | LazyRefusal::FmtArgument { .. })
        | Front::Unchecked { .. } | Front::NoBody { .. } | Front::Const { .. }
        | Front::Static { .. } | Front::Abi { .. } | Front::Literal { .. } | Front::Intrinsic { .. }
        | Front::TargetIntrinsic { .. } | Front::AdtRow { .. } | Front::ObjectTrait { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod refusal_tests {
    use super::*;
    use blaze_ladder::body_ids::WalkError;
    use blaze_ladder::code_records::{CodeKey, CodeRecordError, StoreError};
    use blaze_ladder::run::{Named, RunError};

    pub(crate) fn missing_code() -> InstanceRefusal {
        let symbol = [17; 32];
        let item = ItemKey(3);
        InstanceRefusal {
            instance: crate::run_fault::Named::Path("lib::callee".into()),
            symbol, site: Some(InstanceSite::Item { item, byte: 15 }),
            cause: ParentError::NotChecked { function: item, symbol, why: None },
        }
    }

    #[test]
    fn a_store_or_wire_error_is_never_a_test_refusal() {
        let symbol = [17; 32];
        let item = ItemKey(3);
        let named = || Named { path: "lib::callee".into(), at: blaze_ladder::front::At { file: 0, place: 80 } };
        let make = |why| ParentError::Walk { symbol, why: WalkError::Make { tabled: item, why } };
        let store = || StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) };
        assert_eq!(refused_instance(&ParentError::Walk { symbol, why: WalkError::Store(store()) }), None);
        assert_eq!(refused_instance(&make(RunError::CodeRecord { function: named(), error: CodeRecordError::Store(store()) })), None);
        assert_eq!(refused_instance(&ParentError::UndeclaredSlot { slot: 4 }), None);
        let range = blaze_typeck::Range { start: 15, end: 22 };
        let why = RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Literal { function: "lib::callee".into(), what: "an unsupported literal", range });
        assert_eq!(refused_instance(&make(why)), Some((symbol, Some((item, range.start)))));
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

fn asked<'s>(unit: TestUnit<'s>, row: blaze_scope::tests_record::TestRow<'s>, args: &[&str]) -> blaze_run::plan::Asked<'s> {
    use blaze_scope::tests_record::ShouldPanic;
    let mut id = Hasher::new("blaze test: package target and test 1");
    id.part(unit.package.as_bytes()).part(unit.target.as_bytes()).id(&unit.key.0).id(&row.key.0);
    if !unit.tests.harnessed() {
        for arg in args {
            id.part(arg.as_bytes());
        }
    }
    let should_panic = match row.should_panic {
        ShouldPanic::No => blaze_run::plan::ShouldPanic::No,
        ShouldPanic::Yes => blaze_run::plan::ShouldPanic::Yes,
        ShouldPanic::WithMessage(text) => blaze_run::plan::ShouldPanic::WithText(text),
    };
    blaze_run::plan::Asked { test: TestId(id.finish().0), path: row.path, ignore: row.ignore, bench: row.bench, should_panic }
}

fn record_row<'s>(tests: TestRows<'s>, at: usize) -> Result<blaze_scope::tests_record::TestRow<'s>, DriveError> {
    tests.row(at).map_err(DriveError::Record)?.ok_or(DriveError::Record(TestsRecordError::Row { row: at, what: "no row inside the count" }))
}

/// Why a row cannot be entered, asked before its run.
pub(crate) trait Refuse {
    /// The refusal's words (they name the test), or `None`.
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError>;
}

/// Drive the selected rows through one bounded pool, retaining their owning suites.
#[expect(clippy::too_many_arguments, reason = "the driver's edge: each argument one seam of it")]
fn drive<'a, 'r, Z: Zygotes>(
    config: LoopConfig<'r>,
    asked: AVec<'a, blaze_run::plan::Asked<'r>>,
    owners: &'r [SuiteId],
    filtered: u32,
    runner: &mut Runner<'a, Z>,
    refuse: &mut dyn Refuse,
    filed: &mut dyn Filed,
    print: &mut dyn FnMut(&[u8]) -> Result<(), DriveError>,
    alloc: Alloc<'a>,
) -> Result<Status, DriveError> {
    let most = STEPS_FIXED.saturating_add(STEPS_PER_TEST.saturating_mul(asked.len()));
    let mut machine = TestLoop::new(config, alloc)?;
    let mut out: AVec<'a, Effect<'a>> = Vec::new_in(alloc);
    let mut effects: AVec<'a, Effect<'a>> = Vec::new_in(alloc);
    machine.begin_inventory(asked, owners, filtered, runner.zygotes().now(), &mut out)?;
    machine.step(Event::Checked { errors: 0 }, runner.zygotes().now(), &mut out)?;
    let mut answers = Untraced;
    for _ in 0..most {
        core::mem::swap(&mut out, &mut effects);
        let mut done = None;
        for effect in effects.drain(..) {
            match &effect {
                Effect::Open { suite } => return Err(DriveError::NoSuite { suite: *suite }),
                Effect::Moved { .. } => return Err(DriveError::NoHead { effect: "the moved asks" }),
                Effect::Look { .. } => return Err(DriveError::NoHead { effect: "a look-up of a kept pass" }),
                Effect::Run { test, row, .. } => match refuse.refusal(*row).map_err(DriveError::One)? {
                    Some(why) => {
                        let mut line = String::new_in(alloc);
                        writeln!(line, "blaze test: {why}").map_err(|_| DriveError::Print)?;
                        machine.row_detail(*row, line.into_bytes());
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
    async fn run(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<How, OneError>;
}

/// Process-wide dispositions belong to the pool. A SIGTERM is observed by every child,
/// each of which keeps its own grace deadline. No borrow lives across a poll.
#[derive(Default)]
pub(crate) struct Signals {
    held: RefCell<Option<Held>>,
    terminate: Cell<bool>,
    /// Only one child may block per sweep, for at most one drain interval in total.
    wait: Cell<Duration>,
}

impl Signals {
    fn terminate(&self, now: Duration) -> Result<bool, OneError> {
        let mut slot = self.held.borrow_mut();
        let held = match &mut *slot {
            Some(held) => held,
            empty @ None => empty.insert(Held::hold().map_err(OneError::Signals)?),
        };
        if held.take_terminate(now) {
            self.terminate.set(true);
        }
        Ok(self.terminate.get())
    }
}

/// Every pending future is polled once each sweep. Suspension happens only at an explicit
/// turn boundary, after the child has had a chance to drain, answer and check its deadline.
async fn next_turn() {
    let mut yielded = false;
    poll_fn(|cx| match core::mem::replace(&mut yielded, true) {
        false => {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        true => Poll::Ready(()),
    }).await
}

/// What [`OneEach`] heard, kept until the runner reads it.
enum Item {
    Ready,
    Ended { row: u32, test: TestId, micros: u64, how: How, output: Output<'static> },
    CannotBuild { row: u32, test: TestId, why: InstanceRefusal },
    Closed,
    Exited(ZygoteExit),
}

struct Running<'a> {
    row: u32,
    test: TestId,
    began: Nanos,
    child: Pin<std::boxed::Box<dyn Future<Output = Result<(How, Output<'static>), OneError>> + 'a>>,
}

/// One pinned parent per sandbox process. The command thread owns the front end, the lazy
/// resolvers and the store; children execute in parallel while that thread services them.
pub(crate) struct OneEach<'b, R: RunOne, C: Fn() -> Nanos> {
    runs: &'b R,
    signals: &'b Signals,
    now: C,
    heap: Alloc<'static>,
    suite: SuiteId,
    width: usize,
    running: std::vec::Vec<Running<'b>>,
    pending: AVec<'b, Item>,
    batch: AVec<'b, Item>,
    /// The error that stopped a run in blaze, and the tests record row it was of: printed by
    /// [`run_suites`] as one line naming the test, before the command's own error line.
    pub failed: Option<(u32, OneError)>,
}

impl<'b, R: RunOne, C: Fn() -> Nanos> OneEach<'b, R, C> {
    pub(crate) fn new(runs: &'b R, signals: &'b Signals, now: C, alloc: Alloc<'b>, heap: Alloc<'static>) -> Result<OneEach<'b, R, C>, DriveError> {
        // Pinning needs proof that this handle's blocks cannot be reset. Validate the
        // caller's heap once, with a zero-sized vector, and retain its typed allocator.
        let proof = Vec::<(), _>::new_in(heap).into_global().map_err(|_| DriveError::ChildHeap)?;
        let running = std::vec::Vec::new_in(*proof.allocator());
        Ok(OneEach {
            runs, signals, now, heap, suite: SuiteId(0), width: 0,
            pending: Vec::new_in(alloc), batch: Vec::new_in(alloc), running, failed: None,
        })
    }
}

impl<R: RunOne, C: Fn() -> Nanos> Zygotes for OneEach<'_, R, C> {
    fn start(&mut self, suite: SuiteId, width: u32) -> Result<Start, RunError> {
        self.suite = suite;
        self.width = width as usize;
        self.pending.push(Item::Ready);
        Ok(Start::Running)
    }

    fn ask(&mut self, row: u32, test: TestId, budget: Nanos) -> Result<(), RunError> {
        if self.running.len() >= self.width {
            return Err(RunError::Seam { call: "asking for a child beyond the selected worker count", errno: 0, suite: Some(self.suite) });
        }
        let (runs, signals, heap) = (self.runs, self.signals, self.heap);
        let began = (self.now)();
        let child = std::boxed::Box::pin_in(async move {
            let mut output = Output::new(heap);
            let how = runs.run(row, budget, &mut output, signals).await?;
            Ok((how, output))
        }, *self.running.allocator());
        self.running.push(Running { row, test, began, child });
        Ok(())
    }

    fn end(&mut self) -> Result<(), RunError> {
        self.pending.push(Item::Closed);
        self.pending.push(Item::Exited(ZygoteExit::Exited { code: 0 }));
        Ok(())
    }

    fn kill(&mut self) -> Result<(), RunError> {
        // Dropping a live Run kills and reaps its process.
        self.running.clear();
        Ok(())
    }

    fn wait(&mut self, until: Nanos) -> Result<(), RunError> {
        self.batch.append(&mut self.pending);
        if self.batch.is_empty() && self.running.is_empty() {
            return Err(RunError::Seam { call: "waiting on an empty test pool", errno: 0, suite: Some(self.suite) });
        }
        let mut context = Context::from_waker(Waker::noop());
        let mut interval = Duration::ZERO;
        while self.batch.is_empty() && (self.now)() < until {
            let left = Duration::from_nanos(until.0.saturating_sub((self.now)().0));
            self.signals.wait.set(interval.min(left));
            let mut at = 0;
            while at < self.running.len() {
                let Poll::Ready(result) = self.running[at].child.as_mut().poll(&mut context) else { at += 1; continue };
                let Running { row, test, began, .. } = self.running.swap_remove(at);
                let item = match result {
                    Ok((how, output)) => Item::Ended { row, test, micros: (self.now)().0.saturating_sub(began.0) / 1_000, how, output },
                    Err(OneError::CannotBuild(why)) => Item::CannotBuild { row, test, why },
                    Err(error) => {
                        self.failed = Some((row, error));
                        return Err(RunError::Seam { call: "serving a test process", errno: 0, suite: Some(self.suite) });
                    }
                };
                self.batch.push(item);
            }
            interval = DRAIN_EVERY;
        }
        Ok(())
    }

    fn heard_len(&self) -> usize {
        self.batch.len()
    }

    fn heard(&self, at: usize) -> Option<Heard<'_>> {
        Some(match self.batch.get(at)? {
            Item::Ready => Heard::Ready,
            Item::Ended { row, test, micros, how, output } => Heard::Ended(ChildEnd {
                row: *row,
                test: *test,
                micros: *micros,
                how: *how,
                // Nothing it entered or read was seen: its pass is never kept.
                keep: Err(NotKept::Untraced),
                text: "",
                entered: &[],
                reads: &[],
                output: output.captured(),
            }),
            Item::CannotBuild { row, test, why } => Heard::CannotBuild { row: *row, test: *test, why },
            Item::Closed => Heard::Closed,
            Item::Exited(status) => Heard::Exited(*status),
        })
    }

    fn clear(&mut self) {
        self.batch.clear();
    }

    fn now(&self) -> Nanos {
        (self.now)()
    }
}

fn key_of(tests: &TestRows<'_>, row: u32) -> Result<LibKey, OneError> {
    let found = tests.row(row as usize).map_err(OneError::Record)?;
    found.map(|row| row.key).ok_or(OneError::NoRow { row })
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    struct Children {
        started: Cell<u32>,
        live: Cell<u32>,
        peak: Cell<u32>,
    }

    impl RunOne for Children {
        async fn run(&self, row: u32, _: Nanos, output: &mut Output<'_>, _: &Signals) -> Result<How, OneError> {
            self.started.set(self.started.get() + 1);
            self.live.set(self.live.get() + 1);
            self.peak.set(self.peak.get().max(self.live.get()));
            output.push(&row.to_le_bytes());
            next_turn().await;
            self.live.set(self.live.get() - 1);
            Ok(How::Reported(Said::Returned))
        }
    }

    /// Each child's first turn occurs before any child's second. A serial adapter, an
    /// over-width adapter or sharing the most recent child's output breaks this oracle.
    #[test]
    fn overlapping_children_keep_their_own_capture() {
        let heap = blaze_own::testing::heap().alloc();
        let signals = Signals::default();
        let children = Children { started: Cell::new(0), live: Cell::new(0), peak: Cell::new(0) };
        let tick = Cell::new(0);
        let clock = || {
            tick.set(tick.get() + 1);
            Nanos(tick.get())
        };
        let mut pool = OneEach::new(&children, &signals, clock, heap, heap).expect("the process heap");
        pool.start(SuiteId(4), 2).expect("two slots");
        pool.ask(3, TestId([3; 32]), Nanos(100)).expect("first");
        pool.ask(7, TestId([7; 32]), Nanos(100)).expect("second");
        assert!(pool.ask(9, TestId([9; 32]), Nanos(100)).is_err());
        pool.wait(Nanos(100)).expect("ready");
        assert!(matches!(pool.heard(0), Some(Heard::Ready)));
        pool.clear();
        pool.wait(Nanos(100)).expect("both children end");
        assert_eq!((children.started.get(), children.peak.get(), children.live.get()), (2, 2, 0));
        assert_eq!(pool.heard_len(), 2);
        for at in 0..2 {
            let Some(Heard::Ended(end)) = pool.heard(at) else { panic!("a child's result") };
            assert_eq!(end.output.tail, &end.row.to_le_bytes());
            assert_eq!(end.test, TestId([end.row as u8; 32]));
        }
    }
}

fn entry_roots<'a>(harness: bool, names: NamesRecords<'_>, test: LibKey, builder: Builder, tables: Alloc<'static>, alloc: Alloc<'a>)
    -> Result<Roots<'a>, blaze_ladder::run::special::RootsError>
{
    let panic = builder.settings.panic;
    if harness {
        test_roots(names.records, test, names.lang_crates, panic, names.frame, tables, alloc)
    } else {
        blaze_ladder::run::special::run_roots(names.records, names.lang_crates, panic, names.frame, tables, alloc)
    }
}

/// [`Refuse`] over the test roots: each row's roots made in a scratch frame, the words of a
/// refusal kept for the caller. The run makes them again in its own frame: one head lowered
/// twice, until the zygote's protocol carries a refusal (NOT DONE, piece 9).
pub(crate) struct EntryCheck<'c, 'h> {
    pub tests: TestRows<'c>,
    pub names: NamesRecords<'h>,
    pub builder: Builder,
    pub tables: Alloc<'static>,
    said: AString<'c>,
}

impl Refuse for EntryCheck<'_, '_> {
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError> {
        let test = key_of(&self.tests, row)?;
        let frame = Frame::new(bstd::mem::Tag::Other).map_err(OneError::Frame)?;
        let erased = blaze_own::Erased::new(&frame);
        let made = entry_roots(self.tests.harnessed(), self.names, test, self.builder, self.tables, erased.handle());
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
    /// Arguments of a harnessless target's main. A harnessed entry takes none.
    pub args: &'c [&'c str],
    pub tests: TestRows<'c>,
    pub names: NamesRecords<'h>,
    pub ids: &'c RunIds<'h>,
    pub builder: Builder,
    pub lent: &'c CommandLent,
    pub tables: Alloc<'static>,
    pub watch: &'c dyn Watch,
}

impl RunOne for Launched<'_, '_> {
    async fn run(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<How, OneError> {
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
        let front = TestFront { names: self.names, ids: self.ids, builder: self.builder, tables: self.tables, alloc, test, harness: self.tests.harnessed(), watch: self.watch };
        let mut codes = HomeCodeStore::new(self.lent);
        let mut lazy = Lazy::new(cx, front, RecordsDir { cx, dir: self.records }, &mut codes, self.builder, alloc);
        let mut needs = Needs::new(alloc);
        let mut ahead = std::vec::Vec::new_in(alloc);
        let mut placed = Vec::new_in(alloc);
        let head = run_parent::ahead_of(None, &mut lazy, &mut needs, &mut placed, &mut ahead)
            .map_err(|cause| test_parent_error(&mut lazy, cause))?;
        let line = head.line;
        let entry = bstd::mem::format_in!(&frame, "{line}");
        // 3. Its environment: its own `TMPDIR`, then the command's, then the run mode's names.
        let mut vars: Vec<(&str, &str), &Frame> = Vec::with_capacity_in(self.env.len() + 8, &frame);
        vars.push(("TMPDIR", temp.as_str()));
        run_parent::launch_env(self.env.iter().copied().filter(|(name, _)| *name != "TMPDIR"), self.store, entry.as_str(), None, &mut vars);
        // 4. The profile (blaze-run child.rs, the one test profile), the launcher under it.
        let profile = blaze_run::child::seatbelt(temp.as_str(), None, alloc).map_err(|why| OneError::Profile { why: Box::from(bstd::mem::format_in!(&frame, "{why}").as_str()) })?;
        let mut args = Vec::with_capacity_in(self.args.len() + 3, &frame);
        args.extend_from_slice(&["-p", profile.as_str(), self.launcher]);
        args.extend_from_slice(self.args);
        let spec = RunSpec { program: SANDBOX_EXEC, arg0: None, args: args.as_slice(), env: vars.as_slice(), dir: ".", streams: Streams::PIPED, channel: true, own_group: true, parent_death: ParentDeath::STRONGEST };
        let mut run = self.spawner.start_run(cx, &spec).map_err(OneError::Start)?;
        let until = Deadline::after(run.now(), Duration::from_nanos(budget.0));
        let mut pipes = [run.take_stdout().map_err(OneError::Pipe)?, run.take_stderr().map_err(OneError::Pipe)?];
        let mut relay = Relay::new(GRACE);
        let mut inbox = std::vec::Vec::new_in(alloc);
        let mut reply = ahead;
        let mut open = true;
        let child_error = |error| OneError::Loop(RunLoopError::Child(error));
        // Start the other ready children before building any first-call answer.
        let ended = loop {
            next_turn().await;
            let _span = cx.span("blaze test: service one child's pipes, requests and deadline");
            let now = run.now();
            if until.passed(now) {
                run.kill().map_err(child_error)?;
                break How::TimedOut { after: budget };
            }
            match relay.turn(now, signals.terminate(now)?) {
                RelayAct::Forward => run.terminate().map_err(child_error)?,
                RelayAct::Kill => {
                    if let Some(how) = child_outcome(run.kill().map_err(child_error)?) {
                        break how;
                    }
                }
                RelayAct::Nothing => {}
            }
            let bound = until.min(relay.bound());
            drain_pipes(&mut pipes, output, Deadline::PASSED)?;
            if open && !reply.is_empty() {
                let by = Deadline::after(run.now(), WRITE_BOUND).min(bound);
                match run.send(&reply, by) {
                    Ok(()) => {}
                    Err(ProcError { fault: RunFault::Channel(ChannelError::Hangup), .. }) => open = false,
                    Err(error) => return Err(child_error(error)),
                }
                reply.clear();
            }
            let poll = Deadline::after(run.now(), signals.wait.replace(Duration::ZERO)).min(bound);
            if open {
                match run.recv_message(&mut inbox, blaze_launch::wire::LONGEST_FRAME, poll) {
                    Ok(Message::Whole { .. }) => {
                        self.watch.see(blaze_screen::Step::Waits);
                        let taken = match needs.serve(&mut lazy, &inbox, &mut reply) {
                            Ok(taken) => taken,
                            Err(cause) => {
                                // This group is waiting for code it will never receive.
                                // Kill and reap before returning its typed outcome to the plan.
                                run.kill().map_err(child_error)?;
                                return Err(test_parent_error(&mut lazy, cause));
                            }
                        };
                        self.watch.see(blaze_screen::Step::Answers);
                        // The served prefix out, the rest kept in order, as run_loop.rs does
                        // (`drain` wants the nightly-only allocator trait of the vec's allocator).
                        let left = inbox.len() - taken;
                        inbox.copy_within(taken.., 0);
                        inbox.truncate(left);
                        continue;
                    }
                    Ok(Message::Closed) => open = false,
                    Ok(Message::Deadline) => {}
                    Err(error) => return Err(child_error(error)),
                }
            }
            let look = if open { Deadline::PASSED } else { poll };
            if let Some(how) = child_outcome(run.wait(look).map_err(child_error)?) {
                break how;
            }
        };
        // The reaped child's remaining output belongs to its own capture.
        let drain_until = Deadline::after(run.now(), DRAIN_AFTER);
        while pipes.iter().any(Option::is_some) && !drain_until.passed(run.now()) {
            let poll = Deadline::after(run.now(), signals.wait.replace(Duration::ZERO)).min(drain_until);
            drain_pipes(&mut pipes, output, poll)?;
            if pipes.iter().any(Option::is_some) {
                next_turn().await;
            }
        }
        Ok(ended)
    }
}

fn child_outcome(ended: Ended) -> Option<How> {
    match ended {
        Ended::Exited(0) => Some(How::Reported(Said::Returned)),
        Ended::Exited(code) => Some(How::NoReport { code, torn: 0 }),
        Ended::Signaled(number) => Some(How::Signal { number }),
        Ended::Deadline => None,
    }
}

/// At most a pipeful from each stream per turn, so a writer that never stops cannot starve
/// another child's first-call channel or deadline.
fn drain_pipes(pipes: &mut [Option<OutPipe>; 2], output: &mut Output<'_>, by: Deadline) -> Result<(), OneError> {
    let mut chunk = [0u8; 16 * 1024];
    for slot in pipes {
        for _ in 0..4 {
            let Some(pipe) = slot.as_ref() else { break };
            match pipe.read(&mut chunk, by).map_err(OneError::Pipe)? {
                Piped::Bytes(n) => output.push(&chunk[..n]),
                Piped::Closed => *slot = None,
                Piped::Deadline => break,
            }
        }
    }
    Ok(())
}

/// The front of one test's run: the command's names records and body ids (made once for the
/// suite, shared by every test), the roots of row n's test fn (test_roots.rs).
pub(crate) struct TestFront<'s, 'h, 'r> {
    pub harness: bool,
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
        entry_roots(self.harness, self.names, self.test, self.builder, self.tables, self.alloc).map_err(ParentError::Roots)
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

    fn named(&mut self, maker: Option<&Maker<'h>>, tabled: ItemKey, name: &[u8]) -> crate::run_fault::Named {
        use crate::run_fault::Named;
        if let Some(shown) = maker.and_then(|maker| maker.instance_shown(tabled, name)) {
            return Named::Shown { item: shown.item, args: shown.args };
        }
        match self.path_of(tabled) {
            Some(path) => Named::Path(path),
            None => Named::Item(tabled),
        }
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

struct JsonString<'a>(&'a str);

impl fmt::Display for JsonString<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        for c in self.0.chars() {
            match c {
                '"' | '\\' => write!(f, "\\{c}")?,
                c if c < ' ' => write!(f, "\\u{:04x}", c as u32)?,
                c => f.write_char(c)?,
            }
        }
        f.write_char('"')
    }
}

/// Listing reads test rows in certificate inventory order without starting a launcher.
pub(crate) fn list(cx: &Cx, suites: &[(Found<'_>, TestUnit<'_>)], config: LoopConfig<'_>, out: &bstd::out::Out, frame: &Frame) -> Result<Status, DriveError> {
    use blaze_run::report::Format;
    let erased = blaze_own::Erased::new(frame);
    let plan = blaze_run::plan::Plan::new(config.plan, erased.handle()).map_err(LoopError::from)?;
    let (mut tests, mut benches) = (0usize, 0usize);
    for (_, unit) in suites {
        let record = unit.tests;
        if record.len() == 0 {
            match config.style.format {
                Format::Text => out.line(cx, format_args!("{}/{}: 0 tests", unit.package, unit.target)),
                Format::Json => out.line(cx, format_args!("{{\"event\":\"empty_suite\",\"package\":{},\"target\":{}}}", JsonString(unit.package), JsonString(unit.target))),
            }.map_err(|_| DriveError::Print)?;
        }
        for at in 0..record.len() {
            let row = asked(*unit, record_row(record, at)?, &[]);
            if record.harnessed() && !plan.lets_through(&row) {
                continue;
            }
            let kind = if row.bench { benches += 1; "bench" } else { tests += 1; "test" };
            match config.style.format {
                Format::Text => out.line(cx, format_args!("{}/{}/{}: {kind}", unit.package, unit.target, row.path)),
                Format::Json => out.line(cx, format_args!("{{\"event\":\"listed\",\"package\":{},\"target\":{},\"path\":{},\"kind\":\"{kind}\"}}", JsonString(unit.package), JsonString(unit.target), JsonString(row.path))),
            }.map_err(|_| DriveError::Print)?;
        }
    }
    match config.style.format {
        Format::Text => out.line(cx, format_args!("{tests} tests, {benches} benchmarks")),
        Format::Json => out.line(cx, format_args!("{{\"event\":\"listed_summary\",\"tests\":{tests},\"benches\":{benches}}}")),
    }.map_err(|_| DriveError::Print)?;
    Ok(Status { failed: 0, not_run: 0, check_errors: 0 })
}

pub(crate) struct Suite<'s, 'h> {
    pub unit: TestUnit<'s>,
    pub names: NamesRecords<'h>,
    pub builder: Builder,
}

/// The child pool addresses rows across the request. Each launch and precheck uses
/// the row's own target, frontend and temporary directory.
#[derive(Clone, Copy)]
struct Location {
    suite: SuiteId,
    row: u32,
}

struct Selected<'r, T> {
    suites: &'r mut [T],
    rows: &'r [Location],
}

impl Refuse for Selected<'_, EntryCheck<'_, '_>> {
    fn refusal(&mut self, flat: u32) -> Result<Option<&str>, OneError> {
        let loc = self.rows.get(flat as usize).ok_or(OneError::NoRow { row: flat })?;
        self.suites[loc.suite.0 as usize].refusal(loc.row)
    }
}

impl RunOne for Selected<'_, Launched<'_, '_>> {
    async fn run(&self, flat: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<How, OneError> {
        let loc = self.rows.get(flat as usize).ok_or(OneError::NoRow { row: flat })?;
        self.suites[loc.suite.0 as usize].run(loc.row, budget, output, signals).await
    }
}

#[expect(clippy::too_many_arguments, reason = "the command supplies the selected records and runtime capabilities")]
pub(crate) fn run_suites<'h>(
    cx: &Cx, io: &Io, suites: &[Suite<'_, 'h>], ids: &RunIds<'h>, config: LoopConfig<'_>,
    args: &[&str], lent: &CommandLent, tables: Alloc<'static>, watch: &dyn Watch, frame: &Frame,
) -> Result<Status, DriveError> {
    let cx = cx.span("blaze test: selected suites");
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
    let command = bstd::mem::format_in!(frame, "{}-{}", host.pid(), clock.now().since_start().as_nanos());
    let command_path = Path::new(command.as_str()).map_err(|why| one(OneError::Path(why)))?;
    let mine = temps.make_dir(&cx, &command_path, Durable::No).map_err(|why| one(OneError::Temp(why)))?;
    let store = bstd::mem::format_in!(frame, "{home}/{RECORDS}");
    // 2. The launcher beside this binary, the spawner, the command's environment.
    let exe = host.exe().map_err(DriveError::Missing)?;
    let launcher = crate::own_process::launcher_beside(&exe, frame).map_err(|why| one(OneError::Launcher(why)))?;
    let spawner = host.spawner().map_err(DriveError::Missing)?;
    let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(frame);
    vars.extend(env.each());
    let select = blaze_run::plan::Plan::new(config.plan, alloc).map_err(LoopError::from)?;
    let mut dirs = Vec::with_capacity_in(suites.len(), frame);
    for index in 0..suites.len() {
        let named = bstd::mem::format_in!(frame, "{index}");
        let suite_path = Path::new(named.as_str()).map_err(|why| one(OneError::Path(why)))?;
        let dir = mine.make_dir(&cx, &suite_path, Durable::No).map_err(|why| one(OneError::Temp(why)))?;
        let temp = bstd::mem::format_in!(frame, "{home}/{TEMPS}/{}/{index}", command.as_str());
        dirs.push((dir, temp));
    }
    let mut launches = Vec::with_capacity_in(suites.len(), alloc);
    let mut checks = Vec::with_capacity_in(suites.len(), alloc);
    let mut locations = Vec::new_in(alloc);
    let mut owners = Vec::new_in(alloc);
    let mut rows = Vec::new_in(alloc);
    let mut filtered = 0u32;
    for (index, suite) in suites.iter().enumerate() {
        let unit = suite.unit;
        let tests = unit.tests;
        let (dir, temp) = &dirs[index];
        let suite_id = SuiteId(u32::try_from(index).map_err(|_| DriveError::TooManyRows { count: suites.len() })?);
        launches.push(Launched {
            cx: &cx, spawner: &spawner, launcher: launcher.as_str(), records: &records, store: store.as_str(),
            temps: dir, temp: temp.as_str(), env: vars.as_slice(), tests, names: suite.names, ids,
            builder: suite.builder, lent, tables, watch,
            args: if tests.harnessed() { &[] } else { args },
        });
        checks.push(EntryCheck { tests, names: suite.names, builder: suite.builder, tables, said: String::new_in(alloc) });
        for at in 0..tests.len() {
            let mut row = asked(unit, record_row(tests, at)?, args);
            // Custom harnesses receive the raw arguments themselves.
            if tests.harnessed() && !select.lets_through(&row) {
                filtered = filtered.checked_add(1).ok_or(DriveError::TooManyRows { count: u32::MAX as usize + 1 })?;
                continue;
            }
            if suites.len() > 1 {
                let path = bstd::mem::format_in!(frame, "{}/{}/{}", unit.package, unit.target, row.path);
                row.path = crate::work::held_str(path.as_str(), frame);
            }
            rows.push(row);
            owners.push(suite_id);
            locations.push(Location { suite: suite_id, row: u32::try_from(at).map_err(|_| DriveError::TooManyRows { count: tests.len() })? });
        }
    }
    let now = || Nanos(u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX));
    let launched = Selected { suites: launches.as_mut_slice(), rows: &locations };
    let mut refuse = Selected { suites: checks.as_mut_slice(), rows: &locations };
    let signals = Signals::default();
    let zygotes = OneEach::new(&launched, &signals, now, alloc, tables)?;
    let mut runner = Runner::new(zygotes, &config.plan, TestHost::THIS, alloc);
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
    let status = drive(config, rows, &owners, filtered, &mut runner, &mut refuse, &mut unkept, &mut print, alloc);
    // A run that stopped in blaze: ONE line naming the test, what failed and its cause, then
    // the command's own error (`OneStopped`) says the line above is why (rule 19). The
    // runner's `Seam` error says only that one stopped, so it is not shown.
    if let (Err(DriveError::Run(_)), Some((row, failed))) = (&status, runner.zygotes().failed.as_ref()) {
        let location = locations.get(*row as usize).ok_or(one(OneError::NoRow { row: *row }))?;
        let unit = suites[location.suite.0 as usize].unit;
        let tests = unit.tests;
        let path = record_row(tests, location.row as usize)?.path;
        err.line(&cx, format_args!("blaze test: the test `{}/{}/{path}` could not be run: {failed}", unit.package, unit.target)).map_err(|_| DriveError::Print)?;
        return Err(DriveError::OneStopped);
    }
    let status = status?;
    if unkept != Unkept::default() {
        err.line(&cx, format_args!("{unkept}")).map_err(|_| DriveError::Print)?;
    }
    Ok(status)
}
