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
//! `2 * OUTPUT_CAP` bytes, all of it hashed and counted; [`StderrCapture`] also keeps fd 2
//! in full for a signal report), and a deadline (rule 18: the plan's
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
use blaze_ladder::run::make_one::{Maker, NamesRecords, SecondCheck};
use blaze_ladder::run::special::Roots;
use blaze_ladder::run::special::test_roots::test_roots;
use blaze_launch::wire::run::Symbol;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_run::head::{Head, ReadDigest};
use blaze_run::plan::{Address, NotKept};
use blaze_run::postings::{Ask, FnName, Postings, ReadName};
use blaze_run::result::{AtAddress, Captured, OUTPUT_CAP, Slot};
use blaze_run::zygote::{How, Said, Status as ZygoteExit};
use blaze_run::{Nanos, TestId};
use blaze_scope::tests_record::TestsRecordError;
use blaze_screen::Watch;
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::Deadline;
use bstd::fs::{Dir, Durable, FsError, FsKind, ReadWrite};
use bstd::io::Missing;
use bstd::mem::Frame;
use bstd::path::{Path, PathError};
use bstd::procs::{ChannelError, Exit, GRACE, Held, Message, OutPipe, ParentDeath, ParentSignals, Read as Piped, Relay, RelayAct, RunChild, RunError as ProcError, RunFault, RunSpec, SignalError, Spawner, Started, Streams, Waited};

use crate::code_store::HomeCodeStore;
use crate::run_names::{Found, TestRows, TestUnit};
use crate::run_ids::RunIds;
use crate::run_loop::LoopError as RunLoopError;
use crate::run_bytes::{Chosen, Delivery};
use crate::run_parent::{self, CodeRefusal, Codes, Front, Gate, Lazy, Needs, ParentError, RECORDS, RecordsDir, Resolve, Resolved, Start as _, WRITE_BOUND};

/// THE TEST'S MAKER: every body checked by the run, as before D197 root 1, for now. `run`
/// reads bodies from the check's records by kind (run_parent.rs `RunMaker`); a test's code
/// workers cannot yet: the command's store handle (`CommandLent`) is a `RefCell`, not `Sync`,
/// and the shared snapshot does not hold this command's own puts. What a worker needs: a `Sync`
/// view that sees this command's puts, or the relay serving the record reads. The serial
/// step's maker and the workers' are one type, so they flip together.
pub(crate) type TestMaker<'h> = Maker<'h, crate::run_ids::Runnable, SecondCheck>;
use blaze_engine::SVec;
use blaze_engine::code_pool::relay::{self as code_relay, Relay as PoolRelay, RelayError, Reply};
use blaze_engine::code_pool::{Child, Time, Worker};
use blaze_ladder::run::special::MakerStart;
use blaze_own::SharedAlloc;
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
/// The domain of a test output's digest.
const OUTPUT_DOMAIN: &str = "blaze test output 1";
/// Beneath `<HOME>`: the parent of each command's suite and row folders (layout.rs's row).
pub(crate) const TEMPS: &str = ".blaze/v2/test/tmp";

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
    /// [`CODES`] is `Pooled` and the command's pool gave no code worker (D194): one worker, or
    /// none idle at the start. Refused by name, never made serially in its place (rule 3).
    NoCodeWorkers { have: u32 },
    /// The code relay could not be made (its own words, D194).
    Relay(RelayError),
    /// A code worker failed, or did not end by [`END_WAIT`] after the run (D194).
    CodeWorkers(bstd::task::TaskError),
    /// The CAS store refused reading or writing incremental test cache records.
    CacheStore { what: &'static str, why: crate::test_cache::CacheError },
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
            DriveError::NoCodeWorkers { have } => write!(f, "blaze test: code made by the command's workers (test_drive.rs `CODES` = `Pooled`, D194) needs at least one worker beside the command thread, and the command's pool of {have} gave none (bstd task.rs `must_overlap`)"),
            DriveError::Relay(error) => write!(f, "blaze test: {error}"),
            DriveError::CodeWorkers(error) => write!(f, "blaze test: the command's code workers (D194): {error}"),
            DriveError::CacheStore { what, why } => write!(f, "blaze test: {what}: {why}"),
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
    /// No file at `launcher`, the launcher's path beside this binary `exe`
    /// (own_process.rs `launcher_beside`): no test can start.
    NoLauncher { launcher: Box<str>, exe: Box<str> },
    /// Whether the launcher is at `launcher` could not be looked at: the refusal's words.
    LauncherNotLooked { launcher: Box<str>, why: Box<str> },
    /// The Seatbelt profile could not say a path, or words did not fit: in words.
    Profile { why: Box<str> },
    Start(ProcError),
    Pipe(ProcError),
    MissingPipe { stream: &'static str },
    Capture(FsError),
    ShortCapture { read: u64, written: u64 },
    Diagnostic(bstd::out::OutError),
    Loop(RunLoopError),
    Signals(SignalError),
    /// The code relay refused a step (D194: its own words name the child, worker or key).
    Relay(RelayError),
    /// The test's code for `instance` was asked of the command's workers and did not come
    /// within `waited` nanoseconds ([`CODE_WAIT`], D194): the test is stopped, never waited out.
    CodeLate { instance: crate::run_fault::Named, symbol: [u8; 32], waited: u64 },
    /// A make, or a worker's ask for the proc-macro runner, could not be handed over the code
    /// workers' channel `chan` (D194): the worker `worker` is gone, or the hand-over stopped so.
    CodeHandOff { chan: &'static str, worker: usize, why: &'static str },
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
            OneError::NoLauncher { launcher, exe } => write!(
                f,
                "no `{}` beside `{exe}` (looked for at `{launcher}`): every test starts in it; it is built with blaze and installed next to it",
                crate::own_process::LAUNCHER
            ),
            OneError::LauncherNotLooked { launcher, why } => write!(f, "whether the launcher is at `{launcher}` could not be looked at: {why}"),
            OneError::Profile { why } => write!(f, "a test's sandbox profile: {why}"),
            OneError::Start(error) => write!(f, "a test's launcher did not start under {SANDBOX_EXEC}: {error}"),
            OneError::Pipe(error) => write!(f, "a test's output: {error}"),
            OneError::MissingPipe { stream } => write!(f, "the test launcher was started with piped {stream} but supplied no read end"),
            OneError::Capture(error) => write!(f, "the test's stderr capture: {error}"),
            OneError::ShortCapture { read, written } => write!(f, "the test's stderr capture ended after {read} of {written} recorded bytes"),
            OneError::Diagnostic(error) => write!(f, "writing the test's signal report: {error}"),
            OneError::Loop(error) => write!(f, "a test's run: {error}"),
            OneError::Signals(error) => write!(f, "a test's run: the parent's signals: {error}"),
            OneError::Relay(error) => write!(f, "a test's code: {error}"),
            OneError::CodeHandOff { chan, worker, why } => write!(f, "a test's code: code worker {worker}'s channel `{chan}` {why} (test_drive.rs `CodePool`, D194)"),
            OneError::CodeLate { instance, symbol, waited } => write!(
                f,
                "a test's code: {} was asked of the command's workers and did not come in {}.{} s (test_drive.rs `CODE_WAIT`, D194; rule 18)",
                InstanceName(instance, *symbol),
                waited / 1_000_000_000,
                waited % 1_000_000_000 / 100_000_000,
            ),
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
    cause: RefusalCause,
}

#[derive(Debug)]
enum RefusalCause {
    Build(ParentError),
    Unresolved,
}

#[derive(Debug)]
enum InstanceSite {
    Source(crate::run_fault::SiteShown),
    Item { item: ItemKey, byte: u32 },
}

struct InstanceName<'n>(&'n crate::run_fault::Named, [u8; 32]);

impl fmt::Display for InstanceName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::run_fault::Named;
        match self.0 {
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
                for byte in self.1 {
                    write!(f, "{byte:02x}")?;
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for InstanceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A refusal made on a code worker is named and placed by that worker's maker (D194): the
        // command thread's walk has no program of its own to show or place it by.
        let crossed = match &self.cause {
            RefusalCause::Build(ParentError::CodeRefused { why, .. }) => Some(why),
            _ => None,
        };
        match crossed.and_then(|why| why.instance.as_ref()) {
            Some(shown) => f.write_str(shown.as_str())?,
            None => write!(f, "{}", InstanceName(&self.instance, self.symbol))?,
        }
        match (crossed.and_then(|why| why.site.as_ref()), &self.site) {
            (Some(site), _) => write!(f, " at {}:{}:{}", site.file.as_str(), site.line, site.column)?,
            (None, Some(InstanceSite::Source(site))) => write!(f, " at {}:{}:{}", site.file, site.line, site.column)?,
            (None, Some(InstanceSite::Item { item, byte })) => write!(f, " at byte {byte} of item {:#x}", item.0)?,
            (None, None) => {}
        }
        let cause = match &self.cause {
            RefusalCause::Build(cause) => cause,
            RefusalCause::Unresolved => return f.write_str(": the first-call stub requested this symbol, but the program has no callable record for it"),
        };
        match cause {
            ParentError::Walk {
                why: blaze_ladder::body_ids::WalkError::Make { why: blaze_ladder::run::RunError::Build { error, .. }, .. }, ..
            } => write!(f, ": {error}"),
            ParentError::Walk { why: blaze_ladder::body_ids::WalkError::Make { why, .. }, .. } => write!(f, ": {why}"),
            // The two arms above, as the refusal's worker printed them (test_drive.rs `crossing`).
            ParentError::CodeRefused { why: CodeRefusal { cause: Some(cause), whole, .. }, .. } => {
                write!(f, ": {cause}")?;
                if !whole {
                    f.write_str(run_parent::WORDS_CUT)?;
                }
                Ok(())
            }
            ParentError::Special { why, .. } => write!(f, ": {why}"),
            cause => write!(f, ": {cause}"),
        }
    }
}

/// Only a refusal of the requested instance belongs to its test. A broken shared record, a
/// wire error, or an invariant of the parent stops the command; a names-record read that did
/// not answer while one instance was made (`LazyRefusal::RecordsVoid`) is that test's. Keep the walk alive until the instance and item-relative site have been named.
fn test_parent_error<F: Front, D: run_parent::Folder>(lazy: &mut Lazy<'_, '_, F, D>, cause: ParentError) -> OneError {
    let Some((symbol, location)) = instance_site(&cause) else { return OneError::Parent(cause) };
    let site = location.map(|(item, byte)| match lazy.site_of(item, byte) {
        Some(shown) => InstanceSite::Source(shown),
        None => InstanceSite::Item { item, byte },
    });
    OneError::CannotBuild(InstanceRefusal { instance: lazy.name_of(symbol), symbol, site, cause: RefusalCause::Build(cause) })
}

/// The test whose instance could not be made, and where (its tabled fn and [`refused_byte`]),
/// or `None` for a fault that is the suite's own: whose it is is [`ParentError::scope`]'s
/// answer alone (blaze-ladder scope.rs, the one home of that sorting); this says only where.
/// INVERTED (wave-1119, handoff c8): every failure of making ONE instance is that test's NOT RUN
/// (rule 20), and only the classes scope.rs names stop the suite (rule 19). Before, the test's
/// classes were listed and every class not on the list stopped the suite: three new front-end
/// classes stopped whole suites in one day that way (`LazyRefusal::RecordsVoid` at tempo a1425;
/// `LazyRefusal::NotFound` at tempo-primitives 15:06, "the suite stopped" with 165 tests passing
/// at 14:41).
fn instance_site(cause: &ParentError) -> Option<([u8; 32], Option<(ItemKey, u32)>)> {
    use blaze_ladder::body_ids::WalkError;
    use blaze_ladder::scope::Scope;

    if cause.scope() == Scope::Suite {
        return None;
    }
    Some(match cause {
        ParentError::Walk { symbol, why: WalkError::Make { tabled, why } } => (*symbol, Some((*tabled, refused_byte(why)))),
        ParentError::Walk {
            symbol,
            why:
                WalkError::NoBody { tabled }
                | WalkError::NoCrateFact { tabled, .. }
                | WalkError::Name { tabled, .. }
                | WalkError::NameTooLarge { tabled, .. }
                | WalkError::ArenaFull { tabled, .. }
                | WalkError::RecursionLimit { tabled, .. },
        } => (*symbol, Some((*tabled, 0))),
        // Sorted on its worker by this function (test_drive.rs `crossing`).
        ParentError::CodeRefused { symbol, why } => (*symbol, why.at),
        ParentError::Special { symbol, .. } => (*symbol, None),
        ParentError::NotChecked { function, symbol, .. } => (*symbol, Some((*function, 0))),
        ParentError::NoPersonality { function, symbol, .. } => (*symbol, function.map(|item| (item, 0))),
        // `ParentError::scope` gives every other variant (and the walk's other steps) to the
        // suite, so none is here; were one given to a test with no site written above, the
        // suite stops naming it (rule 3: said, never one test's quietly).
        _ => return None,
    })
}

/// The byte of the body where making one instance refused (`0`: the body, no place). A refusal
/// a later instance carried (`NotBuilt`) is placed by the refusal it holds. Whose it is is
/// `RunError::scope`'s.
fn refused_byte(why: &blaze_ladder::run::RunError) -> u32 {
    use blaze_ladder::front2::program::Front2Refusal;
    use blaze_ladder::run::RunError;
    match why {
        RunError::Build { range, .. } => range.map_or(0, |range| range.start),
        RunError::NotBuilt { refusal, .. } => refused_byte(refusal),
        RunError::Front2(Front2Refusal::Literal { range, .. }) => range.start,
        // A place is written only for the classes above; every other is the body's.
        _ => 0,
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
            cause: RefusalCause::Build(ParentError::NotChecked { function: item, symbol, why: crate::run_ids::NotRunnable::Checked(crate::run_ids::Checked::Absent(crate::run_ids::Missing::NoRecord)) }),
        }
    }

    #[test]
    fn a_store_or_wire_error_is_never_a_test_refusal() {
        let symbol = [17; 32];
        let item = ItemKey(3);
        let named = || Named { path: "lib::callee".into(), at: blaze_ladder::front::At { file: 0, place: 80 } };
        let make = |why| ParentError::Walk { symbol, why: WalkError::Make { tabled: item, why } };
        let store = || StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) };
        assert_eq!(instance_site(&ParentError::Walk { symbol, why: WalkError::Store(store()) }), None);
        assert_eq!(instance_site(&make(RunError::CodeRecord { function: named(), error: CodeRecordError::Store(store()) })), None);
        assert_eq!(instance_site(&ParentError::UndeclaredSlot { slot: 4 }), None);
        let range = blaze_typeck::Range { start: 15, end: 22 };
        let why = RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Literal { function: "lib::callee".into(), what: "an unsupported literal", range });
        assert_eq!(instance_site(&make(why)), Some((symbol, Some((item, range.start)))));
    }

    /// A test's code blaze refuses (a static's data, a thread local, a lowering, an unwinding
    /// raise) is that test's NOT RUN, and a refusal carried by `NotBuilt` is judged by what it
    /// carries. PLANTED: `NotBuilt` carrying a store's fault stays the suite's stop (rule 19),
    /// never one test's refusal.
    #[test]
    fn a_refusal_of_the_tests_code_is_its_own_and_a_carried_fault_is_not() {
        let symbol = [17; 32];
        let item = ItemKey(3);
        let named = || Named { path: "lib::callee".into(), at: blaze_ladder::front::At { file: 0, place: 80 } };
        let make = |why| ParentError::Walk { symbol, why: WalkError::Make { tabled: item, why } };
        let range = blaze_typeck::Range { start: 15, end: 22 };
        assert_eq!(instance_site(&make(RunError::UnwindRaised { function: named() })), Some((symbol, Some((item, 0)))));
        assert_eq!(instance_site(&make(RunError::Lower { refusals: std::vec::Vec::new(), shown: "a lowering".into() })), Some((symbol, Some((item, 0)))));
        let literal = RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Literal { function: "lib::callee".into(), what: "an unsupported literal", range });
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(literal), not_built: 1, reached: 2 };
        assert_eq!(instance_site(&make(carried)), Some((symbol, Some((item, range.start)))), "a carried refusal is judged by what it carries");
        let store = RunError::CodeRecord { function: named(), error: CodeRecordError::Store(StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) }) };
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(store), not_built: 1, reached: 2 };
        assert_eq!(instance_site(&make(carried)), None, "planted: a carried store fault stops the suite");
    }

    /// Before (tempo a1425, 2026-11-21): a make the names records voided fell to `_` and stopped
    /// the suite. Now it is that test's NOT RUN at the instance stepped. CONTROL: the same void
    /// carried with a store's fault beside it in the walk (`WalkError::Store`) still stops the
    /// suite; TWIN: the void carried by `NotBuilt` is judged by what it carries, the same.
    #[test]
    fn a_make_the_names_records_voided_is_that_tests_not_run() {
        use blaze_ladder::front2::program::{Front2Refusal, lazy::LazyRefusal};
        let symbol = [17; 32];
        let item = ItemKey(3);
        let make = |why| ParentError::Walk { symbol, why: WalkError::Make { tabled: item, why } };
        let void = || RunError::Front2(Front2Refusal::Lazy(LazyRefusal::RecordsVoid { tabled: item, first: "no record answers yet".into(), kept: 1, dropped: 0 }));
        assert_eq!(instance_site(&make(void())), Some((symbol, Some((item, 0)))));
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(void()), not_built: 1, reached: 2 };
        assert_eq!(instance_site(&make(carried)), Some((symbol, Some((item, 0)))), "twin: a carried void is the test's too");
        let store = StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) };
        assert_eq!(instance_site(&ParentError::Walk { symbol, why: WalkError::Store(store) }), None, "control: a store fault stops the suite");
    }

    /// A walk bound the test's code passes (a name past its cap, the names' arena full, an item
    /// past `#![recursion_limit]`) is that test's NOT RUN, at the fn it reached. CONTROL: the
    /// walk's instance cap (`Cap`, blaze's own bound on the whole walk) stays the suite's stop.
    #[test]
    fn a_walk_bound_the_tests_code_passes_is_its_own() {
        let symbol = [17; 32];
        let (item, declarer) = (ItemKey(3), ItemKey(4));
        let walk = |why| ParentError::Walk { symbol, why };
        let recursion = WalkError::RecursionLimit { tabled: item, item, declarer: Some(declarer), depth: 129, limit: 128 };
        assert_eq!(instance_site(&walk(recursion)), Some((symbol, Some((item, 0)))));
        let large = WalkError::NameTooLarge { tabled: item, declarer: Some(declarer), bytes: 1 << 30, most: 1 << 29, heads: [Some(item), None, None] };
        assert_eq!(instance_site(&walk(large)), Some((symbol, Some((item, 0)))));
        let arena = WalkError::ArenaFull { tabled: item, bytes: 1 << 32, adding: 20 };
        assert_eq!(instance_site(&walk(arena)), Some((symbol, Some((item, 0)))));
        assert_eq!(instance_site(&walk(WalkError::Cap { most: 4 })), None, "control: the walk's own cap stops the suite");
    }

    /// INVERTED (wave-1119, handoff c8): a failure of making an instance that no stopping class
    /// names is that test's NOT RUN. tempo-primitives 15:06: `LazyRefusal::NotFound` (a fn the
    /// cold search did not find) fell to `_` and stopped the suite. TWINS: `NotFound` carried by
    /// `NotBuilt` and through a row's refusal, and `Unanswered`, which no list names, are the
    /// test's too. PLANTED: a store fault carried by `NotBuilt` still stops the suite. CONTROLS:
    /// the program's queue cap (`Front2Refusal::Capped`) and the run's instance bound
    /// (`TooManyInstances`) stop it.
    #[test]
    fn every_failure_to_make_an_instance_is_its_tests_unless_named() {
        use blaze_ladder::front2::program::{Front2Refusal, lazy::LazyRefusal};
        let symbol = [17; 32];
        let item = ItemKey(3);
        let named = || Named { path: "lib::callee".into(), at: blaze_ladder::front::At { file: 0, place: 80 } };
        let make = |why| ParentError::Walk { symbol, why: WalkError::Make { tabled: item, why } };
        let not_found = || LazyRefusal::NotFound { key: item, krate: "crate 91".into(), searched: "its item, module and variant rows".into() };
        assert_eq!(instance_site(&make(RunError::Front2(Front2Refusal::Lazy(not_found())))), Some((symbol, Some((item, 0)))));
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(RunError::Front2(Front2Refusal::Lazy(not_found()))), not_built: 1, reached: 2 };
        assert_eq!(instance_site(&make(carried)), Some((symbol, Some((item, 0)))), "twin: carried by `NotBuilt`");
        let row = Front2Refusal::Lazy(LazyRefusal::Row { function: "lib::callee".into(), refusal: std::boxed::Box::new(Front2Refusal::Lazy(not_found())) });
        assert_eq!(instance_site(&make(RunError::Front2(row))), Some((symbol, Some((item, 0)))), "twin: through a row's refusal");
        let unanswered = Front2Refusal::Lazy(LazyRefusal::Unanswered { question: "each_item", item: "crate 91".into() });
        assert_eq!(instance_site(&make(RunError::Front2(unanswered))), Some((symbol, Some((item, 0)))), "twin: a class no list names is the test's");
        let store = RunError::CodeRecord { function: named(), error: CodeRecordError::Store(StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) }) };
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(store), not_built: 1, reached: 2 };
        assert_eq!(instance_site(&make(carried)), None, "planted: a carried store fault stops the suite");
        let capped = Front2Refusal::Capped { entry: "lib::main".into(), queued: 9, cap: 8, items: 9, forced: false, report: "".into() };
        assert_eq!(instance_site(&make(RunError::Front2(capped))), None, "control: the program's queue cap stops the suite");
        assert_eq!(instance_site(&make(RunError::TooManyInstances)), None, "control: the run's instance bound stops the suite");
    }
}

/// What the loop files: blaze-run result.rs records, the head and the postings.
pub(crate) trait Filed {
    fn file(&mut self, suite: SuiteId, test: TestId, address: Address, slot: Slot, bytes: &[u8]) -> Result<(), DriveError>;
    fn flaky(&mut self, suite: SuiteId, test: TestId) -> Result<(), DriveError>;
    fn head(&mut self, suite: SuiteId, bytes: &[u8]) -> Result<(), DriveError>;
    fn postings(&mut self, suite: SuiteId, bytes: &[u8]) -> Result<(), DriveError>;
}

fn asked<'s>(unit: TestUnit<'s>, row: blaze_scope::tests_record::TestRow<'s>, builder: &Builder, args: &[&str]) -> blaze_run::plan::Asked<'s> {
    let test = crate::test_cache::test_id(unit, row, builder, args);
    asked_with_id(row, test)
}

fn asked_with_id<'s>(row: blaze_scope::tests_record::TestRow<'s>, test: TestId) -> blaze_run::plan::Asked<'s> {
    use blaze_scope::tests_record::ShouldPanic;
    let should_panic = match row.should_panic {
        ShouldPanic::No => blaze_run::plan::ShouldPanic::No,
        ShouldPanic::Yes => blaze_run::plan::ShouldPanic::Yes,
        ShouldPanic::WithMessage(text) => blaze_run::plan::ShouldPanic::WithText(text),
    };
    blaze_run::plan::Asked { test, path: row.path, ignore: row.ignore, bench: row.bench, should_panic }
}

fn record_row<'s>(tests: TestRows<'s>, at: usize) -> Result<blaze_scope::tests_record::TestRow<'s>, DriveError> {
    tests.row(at).map_err(DriveError::Record)?.ok_or(DriveError::Record(TestsRecordError::Row { row: at, what: "no row inside the count" }))
}

/// Why a row cannot be entered, asked before its run.
pub(crate) trait Refuse {
    /// The refusal's words (they name the test), or `None`.
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError>;
}

/// Bridges `TestCache` with the command-thread `entered_sink` so newly resolved function metadata
/// is flushed into `TestCache` before `Answers::answer` computes the run's address.
struct CacheAnswers<'a, 'c, 'm, D: blaze_core::Driver> {
    cache: &'c mut crate::test_cache::TestCache<'a, 'm, D>,
    sink: &'c RefCell<AVec<'a, (SuiteId, crate::test_cache::EnteredFn<'a>)>>,
}

impl<'a, D: blaze_core::Driver> CacheAnswers<'a, '_, '_, D> {
    fn drain_sink(&mut self) {
        let mut drained = self.sink.borrow_mut();
        for (suite, entry) in drained.drain(..) {
            self.cache.record_entered(suite, core::slice::from_ref(&entry));
        }
    }
}

impl<D: blaze_core::Driver> Answers for CacheAnswers<'_, '_, '_, D> {
    fn answer<'s>(&'s mut self, suite: SuiteId, test: TestId, entered: &[FnName], reads: &[ReadName], digests: &mut AVec<'_, (ReadName, ReadDigest)>) -> Result<(Address, AtAddress<'s>), RunError> {
        self.drain_sink();
        self.cache.answer(suite, test, entered, reads, digests)
    }
}

/// Drive the selected rows through one bounded pool, retaining their owning suites.
#[expect(clippy::too_many_arguments, reason = "the driver's edge: each argument one seam of it")]
fn drive<'a, 'r, 'm, Z: Zygotes, D: blaze_core::Driver>(
    config: LoopConfig<'r>,
    asked: AVec<'a, blaze_run::plan::Asked<'r>>,
    owners: &'r [SuiteId],
    suite_heads: &'r [Head<'r>],
    postings: Postings<'r>,
    moved: &[Ask],
    filtered: u32,
    runner: &mut Runner<'a, Z>,
    refuse: &mut dyn Refuse,
    cache: &mut crate::test_cache::TestCache<'a, 'm, D>,
    entered_sink: &RefCell<AVec<'a, (SuiteId, crate::test_cache::EnteredFn<'a>)>>,
    print: &mut dyn FnMut(&[u8]) -> Result<(), DriveError>,
    alloc: Alloc<'a>,
) -> Result<Status, DriveError> {
    let most = STEPS_FIXED.saturating_add(STEPS_PER_TEST.saturating_mul(asked.len()));
    let mut machine = TestLoop::new(config, alloc)?;
    let mut out: AVec<'a, Effect<'a>> = Vec::new_in(alloc);
    let mut effects: AVec<'a, Effect<'a>> = Vec::new_in(alloc);
    machine.begin_inventory(asked, owners, suite_heads, postings, moved, filtered, runner.zygotes().now(), &mut out)?;
    machine.step(Event::Checked { errors: 0 }, runner.zygotes().now(), &mut out)?;
    for _ in 0..most {
        core::mem::swap(&mut out, &mut effects);
        let mut done = None;
        for effect in effects.drain(..) {
            match &effect {
                Effect::Open { suite } => return Err(DriveError::NoSuite { suite: *suite }),
                Effect::Moved { .. } => return Err(DriveError::NoHead { effect: "the moved asks" }),
                Effect::Look { suite, test, .. } => {
                    let found = cache.look(*suite, *test)?;
                    machine.step(Event::Looked { test: *test, found }, runner.zygotes().now(), &mut out)?;
                }
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
                Effect::File { suite, test, address, slot, bytes } => cache.file(*suite, *test, *address, *slot, bytes.as_slice())?,
                Effect::Flaky { suite, finding } => cache.flaky(*suite, finding.flaky.test)?,
                Effect::WriteHead { suite, bytes } => cache.head(*suite, bytes.as_slice())?,
                Effect::WritePostings { suite, bytes } => cache.postings(*suite, bytes.as_slice())?,
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
        let mut answers = CacheAnswers { cache, sink: entered_sink };
        runner.step(&mut machine, &mut answers, &mut out)?;
    }
    Err(DriveError::Steps { most })
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
/// What one child's run produced: its exit, whether its pass is provably pure/kept, and the
/// sorted list of walked Rust functions/statics it entered.
pub(crate) struct RunOutcome {
    pub how: How,
    pub keep: Result<(), NotKept>,
    pub entered: AVec<'static, FnName>,
}

pub(crate) trait RunOne {
    /// Runs row `row` in a process of its own, killed `budget` after its start: how it ended,
    /// its purity verdict, its entered functions, and its output into `output`.
    async fn run(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<RunOutcome, OneError>;
}

/// Process-wide dispositions belong to the pool. A SIGTERM is observed by every child,
/// each of which keeps its own grace deadline. No borrow lives across a poll.
#[derive(Default)]
pub(crate) struct Signals {
    held: RefCell<Option<Held>>,
    terminate: Cell<bool>,
    /// Only one child may block per sweep, for at most one drain interval in total.
    wait: Cell<Duration>,
    /// What the child being polled last began ([`Doing`]): set by its run, read by
    /// [`OneEach`] after each poll, so a turn that held the command thread is named by it.
    doing: Cell<Doing>,
    /// Exact host-work nanoseconds recorded inside the polled turn (e.g. `needs.serve` inside
    /// `Doing::Serving`, excluding any `recv_message` wait before the frame arrived). `None`
    /// means the turn's whole elapsed time is used whenever [`Doing::holds_command_thread`].
    host_nanos: Cell<Option<u64>>,
    /// The polled child's time parked waiting for its code so far (D194): set from its
    /// [`Running`] before its poll, raised by its run while parked, kept back after; nanoseconds.
    parked: Cell<u64>,
    /// Whether the polled child is currently parked waiting for a code worker (D194), so
    /// [`OneEach`] knows whether a sibling's stall is already covered by its open code wait.
    parked_now: Cell<bool>,
    /// Whether the polled child has started its sandboxed process (`started_at` recorded).
    started: Cell<bool>,
    /// Cumulative host command-thread stall time (nanoseconds) accumulated for the polled child
    /// after its process started: passed in by [`OneEach`] before each poll so `run_captured`
    /// extends its own `until` deadline by the same amount the runner credits.
    stalled: Cell<u64>,
}

/// What one child's run was doing on the command thread. Every child is served by that one
/// thread (its code made at its first calls, its pipes read), so while one turn runs no other
/// child is answered and the runner's watch (test_run.rs `watch`) is not stepped: a turn longer
/// than another run's allowance makes that run's zygote "silent". This names the turn.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum Doing {
    #[default]
    Idle,
    /// Making the code of its roots before its process starts (run_parent.rs `ahead_of`): NOT
    /// in the child's own allowance (counted from its start), but in the runner's (from the ask).
    Roots,
    /// Starting its sandboxed process.
    Starting,
    /// Answering a request of its process: a first call's code found or made (run_parent.rs
    /// `Lazy::publication`).
    Serving,
    /// Reading its pipes and its deadline.
    Relaying,
}

impl Doing {
    /// Whether this phase performs host-side work (building roots, launching the sandbox, or
    /// compiling/publishing code) that holds the single command thread rather than waiting on
    /// running test code.
    const fn holds_command_thread(self) -> bool {
        matches!(self, Doing::Roots | Doing::Starting | Doing::Serving)
    }
}

impl fmt::Display for Doing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Doing::Idle => "nothing named",
            Doing::Roots => "making the code of its roots before its process started (run_parent.rs `ahead_of`)",
            Doing::Starting => "starting its sandboxed process",
            Doing::Serving => "making the code its process asked for at a first call (run_parent.rs `Lazy::publication`)",
            Doing::Relaying => "reading its pipes",
        })
    }
}

/// The longest single poll of one child: the command thread's turn, which no other child's
/// answer and no runner's watch could interrupt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Turn {
    /// The runner's (flat) row.
    pub row: u32,
    pub nanos: u64,
    pub doing: Doing,
}

impl Signals {
    /// The child being polled began `doing`. A poll holds at most one making of code (the
    /// first holds `ahead_of`, a later one at most one served request), so that one is kept
    /// over the light steps around it: it is what a long turn spent its time on.
    fn begin(&self, doing: Doing) {
        let heavy = |d: Doing| matches!(d, Doing::Roots | Doing::Serving);
        if heavy(doing) || !heavy(self.doing.get()) {
            self.doing.set(doing);
        }
    }

    fn add_host_nanos(&self, elapsed: Duration) {
        let add = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let cur = self.host_nanos.get().unwrap_or(0);
        self.host_nanos.set(Some(cur.saturating_add(add)));
    }

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
    Ended { row: u32, test: TestId, micros: u64, how: How, keep: Result<(), NotKept>, entered: AVec<'static, FnName>, output: Output<'static> },
    CannotBuild { row: u32, test: TestId, why: InstanceRefusal },
    Closed,
    Exited(ZygoteExit),
}

struct Running<'a> {
    row: u32,
    test: TestId,
    began: Nanos,
    /// Whether the child's process has started (`Signals::started`).
    started: bool,
    /// Time elapsed before the child's process started (queued behind sibling turns, `Doing::Roots`,
    /// `Doing::Starting`). Aligns `Runner`'s `answer_due` (from `asked`) with `run_captured`'s
    /// allowance (from `started_at`).
    pre_start: Nanos,
    /// How long its run was parked waiting for its code so far, as its last poll said
    /// ([`Signals::parked`]) plus any unpolled sibling stall while `parked_now` is true.
    parked: Nanos,
    /// Whether the child was parked waiting for a code worker at the end of its last poll.
    parked_now: bool,
    /// Command-thread host stall time accumulated after the child started: its own
    /// `Doing::Serving` turns plus siblings' host turns while not parked on a code worker.
    stalled: Nanos,
    child: Pin<std::boxed::Box<dyn Future<Output = Result<(RunOutcome, Output<'static>), OneError>> + 'a>>,
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
    /// The longest turn any child's poll took ([`Turn`]): printed by [`run_suites`] when the
    /// runner stops, so a zygote "silent" past an allowance says what held the thread.
    pub longest: Option<Turn>,
}

impl<'b, R: RunOne, C: Fn() -> Nanos> OneEach<'b, R, C> {
    pub(crate) fn new(runs: &'b R, signals: &'b Signals, now: C, alloc: Alloc<'b>, heap: Alloc<'static>) -> Result<OneEach<'b, R, C>, DriveError> {
        // Pinning needs proof that this handle's blocks cannot be reset. Validate the
        // caller's heap once, with a zero-sized vector, and retain its typed allocator.
        let proof = Vec::<(), _>::new_in(heap).into_global().map_err(|_| DriveError::ChildHeap)?;
        let running = std::vec::Vec::new_in(*proof.allocator());
        Ok(OneEach {
            runs, signals, now, heap, suite: SuiteId(0), width: 0,
            pending: Vec::new_in(alloc), batch: Vec::new_in(alloc), running, failed: None, longest: None,
        })
    }

    fn sweep(&mut self, context: &mut Context<'_>) -> Result<(), RunError> {
        let mut at = 0;
        while let Some(current) = self.running.get_mut(at) {
            self.signals.doing.set(Doing::Idle);
            self.signals.host_nanos.set(None);
            self.signals.parked.set(current.parked.0);
            self.signals.parked_now.set(current.parked_now);
            self.signals.started.set(current.started);
            self.signals.stalled.set(current.stalled.0);
            let turn = (self.now)();
            let polled = current.child.as_mut().poll(context);
            let after = (self.now)();
            let nanos = after.0.saturating_sub(turn.0);
            let doing = self.signals.doing.get();
            let stall = if doing.holds_command_thread() {
                self.signals.host_nanos.get().map_or(nanos, |reported| reported.min(nanos))
            } else {
                0
            };
            let was_started = current.started;
            let is_started = self.signals.started.get();
            current.started = is_started;
            if !was_started && is_started {
                // Align `Runner`'s `answer_due` (counted from `asked`) with `run_captured`'s
                // `until` (counted from `started_at`) without double-crediting `stalled`.
                let pre = Nanos(after.0.saturating_sub(current.began.0));
                current.pre_start = current.pre_start.max(pre);
            } else if stall > 0 {
                if !was_started {
                    current.pre_start = Nanos(current.pre_start.0.saturating_add(stall));
                } else {
                    current.stalled = Nanos(current.stalled.0.saturating_add(stall));
                }
            }
            current.parked_now = self.signals.parked_now.get();
            current.parked = current.parked.max(Nanos(self.signals.parked.get()));
            let row = current.row;
            if self.longest.is_none_or(|longest| nanos > longest.nanos) {
                self.longest = Some(Turn { row, nanos, doing });
            }
            if stall > 0 {
                // While `at` held the single command thread doing host work (building roots,
                // starting its sandbox, or compiling/publishing code), no sibling could be polled,
                // reaped, or served. Credit every sibling so neither its own deadline nor
                // `Runner::watch` charges it for command-thread stall time.
                for (index, sibling) in self.running.iter_mut().enumerate() {
                    if index == at {
                        continue;
                    }
                    if !sibling.started {
                        sibling.pre_start = Nanos(sibling.pre_start.0.saturating_add(stall));
                    } else if sibling.parked_now {
                        sibling.parked = Nanos(sibling.parked.0.saturating_add(stall));
                    } else {
                        sibling.stalled = Nanos(sibling.stalled.0.saturating_add(stall));
                    }
                }
            }
            let Poll::Ready(result) = polled else { at += 1; continue };
            let Running { row, test, began, .. } = self.running.swap_remove(at);
            let item = match result {
                Ok((outcome, output)) => Item::Ended {
                    row,
                    test,
                    micros: after.0.saturating_sub(began.0) / 1_000,
                    how: outcome.how,
                    keep: outcome.keep,
                    entered: outcome.entered,
                    output,
                },
                Err(OneError::CannotBuild(why)) => Item::CannotBuild { row, test, why },
                Err(error) => {
                    self.failed = Some((row, error));
                    return Err(RunError::Seam { call: "serving a test process", errno: 0, suite: Some(self.suite) });
                }
            };
            self.batch.push(item);
        }
        Ok(())
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
        self.running.push(Running {
            row,
            test,
            began,
            started: false,
            pre_start: Nanos(0),
            parked: Nanos(0),
            parked_now: false,
            stalled: Nanos(0),
            child,
        });
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
        let mut swept = false;
        let mut crossed_until = false;
        while self.batch.is_empty() && (!swept || (self.now)() < until) {
            let before_sweep = (self.now)();
            let left = Duration::from_nanos(until.0.saturating_sub(before_sweep.0));
            self.signals.wait.set(interval.min(left));
            self.sweep(&mut context)?;
            swept = true;
            crossed_until = before_sweep < until && (self.now)() >= until;
            interval = DRAIN_EVERY;
        }
        // If `until` was crossed mid-sweep, give every remaining child one non-blocking poll so
        // earlier-indexed children can report ready exits, enforce their own timeout, or refresh
        // their parked credit before `Runner::watch` checks for silence.
        if crossed_until && !self.running.is_empty() {
            self.signals.wait.set(Duration::ZERO);
            self.sweep(&mut context)?;
        }
        Ok(())
    }

    fn heard_len(&self) -> usize {
        self.batch.len()
    }

    fn heard(&self, at: usize) -> Option<Heard<'_>> {
        Some(match self.batch.get(at)? {
            Item::Ready => Heard::Ready,
            Item::Ended { row, test, micros, how, keep, entered, output } => Heard::Ended(ChildEnd {
                row: *row,
                test: *test,
                micros: *micros,
                how: *how,
                keep: *keep,
                text: "",
                entered: entered.as_slice(),
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

    fn parked(&self, row: u32) -> Nanos {
        self.running.iter().find(|running| running.row == row).map_or(Nanos(0), |running| {
            Nanos(running.pre_start.0.saturating_add(running.parked.0).saturating_add(running.stalled.0))
        })
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
        async fn run(&self, row: u32, _: Nanos, output: &mut Output<'_>, _: &Signals) -> Result<RunOutcome, OneError> {
            self.started.set(self.started.get() + 1);
            self.live.set(self.live.get() + 1);
            self.peak.set(self.peak.get().max(self.live.get()));
            output.push(&row.to_le_bytes());
            next_turn().await;
            self.live.set(self.live.get() - 1);
            Ok(RunOutcome { how: How::Reported(Said::Returned), keep: Err(NotKept::Untraced), entered: Vec::new_in(blaze_own::testing::heap().alloc()) })
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

    /// A child whose first turn makes its roots' code for a long time (the clock moved 5000 in
    /// that one poll), then starts its process: the pool's longest turn is that child's, named
    /// by the code making, not by the light step after it. Twin: a light child's turns are
    /// never the longest.
    struct Slow<'t> {
        tick: &'t Cell<u64>,
    }

    impl RunOne for Slow<'_> {
        async fn run(&self, row: u32, _: Nanos, _: &mut Output<'_>, signals: &Signals) -> Result<RunOutcome, OneError> {
            if row == 5 {
                signals.begin(Doing::Roots);
                self.tick.set(self.tick.get() + 5_000);
                signals.begin(Doing::Starting);
            }
            signals.begin(Doing::Relaying);
            next_turn().await;
            Ok(RunOutcome { how: How::Reported(Said::Returned), keep: Err(NotKept::Untraced), entered: Vec::new_in(blaze_own::testing::heap().alloc()) })
        }
    }

    #[test]
    fn a_long_turn_is_named_by_what_held_the_thread() {
        let heap = blaze_own::testing::heap().alloc();
        let signals = Signals::default();
        let tick = Cell::new(0u64);
        let children = Slow { tick: &tick };
        let clock = || {
            tick.set(tick.get() + 1);
            Nanos(tick.get())
        };
        let mut pool = OneEach::new(&children, &signals, clock, heap, heap).expect("the process heap");
        pool.start(SuiteId(0), 2).expect("two slots");
        pool.ask(2, TestId([2; 32]), Nanos(100_000)).expect("light");
        pool.ask(5, TestId([5; 32]), Nanos(100_000)).expect("slow");
        pool.wait(Nanos(100_000)).expect("ready");
        pool.clear();
        pool.wait(Nanos(100_000)).expect("both end");
        let turn = pool.longest.expect("a turn was timed");
        assert_eq!((turn.row, turn.doing), (5, Doing::Roots));
        assert!(turn.nanos >= 5_000, "the turn's length is not the clock's: {}", turn.nanos);
    }

    struct NoAnswers;

    impl Answers for NoAnswers {
        fn answer<'s>(
            &'s mut self,
            _suite: SuiteId,
            test: TestId,
            _entered: &[FnName],
            _reads: &[ReadName],
            _digests: &mut AVec<'_, (ReadName, ReadDigest)>,
        ) -> Result<(Address, AtAddress<'s>), RunError> {
            Ok((Address(test.0), AtAddress::EMPTY))
        }
    }

    fn drive_one_each<'a, R: RunOne, C: Fn() -> Nanos>(
        pool: OneEach<'a, R, C>,
        width: u32,
        budget: Nanos,
        count: usize,
    ) -> Result<(Status, AString<'static>), RunError> {
        let g = blaze_own::testing::heap().alloc();
        let config = LoopConfig {
            plan: blaze_run::plan::Config {
                width,
                deadline: budget,
                run_all: false,
                ignored: blaze_run::plan::Ignored::Skip,
                benches: false,
                filter: blaze_run::plan::Filter::All,
                skip: &[],
            },
            style: blaze_run::report::Style::text(80, false),
            list: false,
            run: blaze_run::CheckoutKey([1; 32]),
            suites: 1,
        };
        let mut asked: AVec<'static, blaze_run::plan::Asked<'static>> = Vec::new_in(g);
        let mut owners: AVec<'static, SuiteId> = Vec::new_in(g);
        for row in 0..count {
            let mut bytes = [0u8; 32];
            bytes[0] = 0x55;
            bytes[1] = u8::try_from(row).unwrap_or(u8::MAX);
            let path = match row {
                0 => "m::sibling",
                1 => "m::staller",
                _ => "m::other",
            };
            asked.push(blaze_run::plan::Asked {
                test: TestId(bytes),
                path,
                ignore: None,
                bench: false,
                should_panic: blaze_run::plan::ShouldPanic::No,
            });
            owners.push(SuiteId(0));
        }
        let owners_slice = owners.as_slice();
        let heads = [Head::EMPTY];
        let mut runner = Runner::new(pool, &config.plan, TestHost::Macos, g);
        let mut machine = TestLoop::new(config, g).expect("test loop");
        let mut out: AVec<'static, Effect<'static>> = Vec::new_in(g);
        let mut effects: AVec<'static, Effect<'static>> = Vec::new_in(g);
        machine
            .begin_inventory(asked, owners_slice, &heads, Postings::EMPTY, &[], 0, runner.zygotes().now(), &mut out)
            .expect("begin inventory");
        machine.step(Event::Checked { errors: 0 }, runner.zygotes().now(), &mut out).expect("checked");
        let mut text = String::new_in(g);
        let mut answers = NoAnswers;
        for _ in 0..20_000 {
            core::mem::swap(&mut out, &mut effects);
            let mut done = None;
            for effect in effects.drain(..) {
                match &effect {
                    Effect::Run { .. } | Effect::Kill { .. } => runner.effect(&effect)?,
                    Effect::Print { text: bytes } => {
                        text.push_str(core::str::from_utf8(bytes.as_slice()).expect("utf8 output"));
                    }
                    Effect::Done { status } => done = Some(*status),
                    _ => {}
                }
            }
            if let Some(status) = done {
                runner.finish()?;
                return Ok((status, text));
            }
            if !out.is_empty() {
                continue;
            }
            runner.step(&mut machine, &mut answers, &mut out)?;
        }
        panic!("drive_one_each did not finish within step bound");
    }

    /// Regression for tempo `ZygoteSilent`: row 1 holds the single command thread in
    /// `Doing::Serving` (`Lazy::publication`) for 8.0 s — far past row 0's 1.0 s allowance plus
    /// 1.4 s `ANSWER_SLACK` — and finishes on that same turn, returning a non-empty batch to
    /// `Runner::step` -> `Runner::watch` while row 0 is still running.
    /// Row 0 must be credited with row 1's 8.0 s host stall (both in `OneEach::parked` and in
    /// `Signals::stalled`), so it is NOT falsely killed with `ZygoteSilent` or `TimedOut`.
    #[test]
    fn sibling_host_stall_past_allowance_does_not_cause_zygote_silent() {
        const BUDGET: Nanos = Nanos(1_000_000_000);
        const STALL_NS: u64 = 8_000_000_000;

        struct StallingPair<'t> {
            tick: &'t Cell<u64>,
            sibling_saw_stall: Cell<u64>,
        }

        impl RunOne for StallingPair<'_> {
            async fn run(&self, row: u32, budget: Nanos, _: &mut Output<'_>, signals: &Signals) -> Result<RunOutcome, OneError> {
                signals.begin(Doing::Roots);
                self.tick.set(self.tick.get() + 1_000_000);
                signals.begin(Doing::Starting);
                let started_at = self.tick.get();
                signals.started.set(true);
                next_turn().await;
                if row == 1 {
                    // Hold the command thread in `Lazy::publication` past sibling's allowance + ANSWER_SLACK,
                    // then finish immediately so `OneEach::wait` returns to `Runner::watch` while row 0 runs.
                    signals.begin(Doing::Serving);
                    self.tick.set(self.tick.get() + STALL_NS);
                    return Ok(RunOutcome {
                        how: How::Reported(Said::Returned),
                        keep: Err(NotKept::Untraced),
                        entered: Vec::new_in(blaze_own::testing::heap().alloc()),
                    });
                }
                // Row 0: sibling runs for two turns while row 1 stalls the command thread.
                signals.begin(Doing::Relaying);
                self.tick.set(self.tick.get() + 5_000_000);
                next_turn().await;
                signals.begin(Doing::Relaying);
                let credited = signals.stalled.get();
                self.sibling_saw_stall.set(credited);
                let now = self.tick.get();
                assert!(
                    now < started_at.saturating_add(budget.0).saturating_add(credited),
                    "sibling's own deadline would falsely expire without stall credit: now={now}, started={started_at}, budget={}, credited={credited}",
                    budget.0
                );
                Ok(RunOutcome {
                    how: How::Reported(Said::Returned),
                    keep: Err(NotKept::Untraced),
                    entered: Vec::new_in(blaze_own::testing::heap().alloc()),
                })
            }
        }

        let heap = blaze_own::testing::heap().alloc();
        let signals = Signals::default();
        let tick = Cell::new(0u64);
        let children = StallingPair { tick: &tick, sibling_saw_stall: Cell::new(0) };
        let clock = || Nanos(tick.get());
        let pool = OneEach::new(&children, &signals, clock, heap, heap).expect("process heap");
        let (status, text) = drive_one_each(pool, 2, BUDGET, 2).expect("sibling must not trigger ZygoteSilent");
        assert_eq!((status.passed, status.failed), (2, 0), "{text}");
        assert!(
            children.sibling_saw_stall.get() >= STALL_NS,
            "expected sibling to be credited at least {STALL_NS} ns of host stall, got {}",
            children.sibling_saw_stall.get()
        );
    }

    /// Control for genuine hang detection: while the command thread is idle (`Doing::Relaying`),
    /// NO stall time is credited, so a genuinely hung child still times out right at its allowance
    /// (`How::TimedOut`), and a child future that ignores its own deadline still triggers
    /// `RunError::ZygoteSilent` at `allowance + ANSWER_SLACK`.
    #[test]
    fn genuine_hang_with_idle_command_thread_still_times_out_and_silent_child_still_fails() {
        const BUDGET: Nanos = Nanos(1_000_000_000);
        const STEP_NS: u64 = 100_000_000;

        struct Hung {
            tick: Cell<u64>,
            honor_deadline: bool,
            observed_stall: Cell<u64>,
            timed_out_at: Cell<u64>,
        }

        impl RunOne for Hung {
            async fn run(&self, _row: u32, budget: Nanos, _: &mut Output<'_>, signals: &Signals) -> Result<RunOutcome, OneError> {
                let started_at = self.tick.get();
                signals.started.set(true);
                loop {
                    next_turn().await;
                    signals.begin(Doing::Relaying);
                    let stalled = signals.stalled.get();
                    self.observed_stall.set(stalled);
                    let now = self.tick.get();
                    if self.honor_deadline && now >= started_at.saturating_add(budget.0).saturating_add(stalled) {
                        self.timed_out_at.set(now);
                        return Ok(RunOutcome {
                            how: How::TimedOut { after: budget },
                            keep: Err(NotKept::Untraced),
                            entered: Vec::new_in(blaze_own::testing::heap().alloc()),
                        });
                    }
                    self.tick.set(now.saturating_add(STEP_NS));
                }
            }
        }

        // 1. Genuinely hung child honoring its allowance times out cleanly at `BUDGET` with 0 stall credit.
        let heap = blaze_own::testing::heap().alloc();
        let signals = Signals::default();
        let hung = Hung { tick: Cell::new(0), honor_deadline: true, observed_stall: Cell::new(u64::MAX), timed_out_at: Cell::new(0) };
        let clock = || Nanos(hung.tick.get());
        let pool = OneEach::new(&hung, &signals, clock, heap, heap).expect("process heap");
        let (status, text) = drive_one_each(pool, 1, BUDGET, 1).expect("hung child reports TimedOut, not ZygoteSilent");
        assert_eq!((status.passed, status.failed), (0, 1), "{text}");
        assert!(text.contains("killed past its deadline"), "{text}");
        assert_eq!(hung.observed_stall.get(), 0, "idle Doing::Relaying must never credit stall time");
        assert_eq!(hung.timed_out_at.get(), BUDGET.0, "hung child must time out right at its allowance");

        // 2. A broken child future that refuses to honor its own deadline still trips `ZygoteSilent`.
        let signals = Signals::default();
        let silent = Hung { tick: Cell::new(0), honor_deadline: false, observed_stall: Cell::new(u64::MAX), timed_out_at: Cell::new(0) };
        let clock = || Nanos(silent.tick.get());
        let pool = OneEach::new(&silent, &signals, clock, heap, heap).expect("process heap");
        let stopped = drive_one_each(pool, 1, BUDGET, 1).expect_err("silent child must trip ZygoteSilent");
        assert!(matches!(stopped, RunError::ZygoteSilent { row: 0, .. }), "expected ZygoteSilent, got {stopped:?}");
        assert_eq!(silent.observed_stall.get(), 0, "idle Doing::Relaying must never credit stall time");
    }
}

fn entry_roots<'a>(harness: bool, names: NamesRecords<'_>, test: LibKey, exports: &[LibKey], builder: Builder, tables: Alloc<'static>, alloc: Alloc<'a>)
    -> Result<Roots<'a>, blaze_ladder::run::special::RootsError>
{
    let panic = builder.settings.panic;
    if harness {
        test_roots(names.records, test, names.lang_crates, panic, exports, names.frame, tables, alloc)
    } else {
        blaze_ladder::run::special::run_roots(names.records, names.lang_crates, panic, exports, names.frame, tables, alloc)
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
    /// Its suite's native image's exports (`NativeSend::items`, roots of every row), else why the
    /// image could not be had ([`Suite::native`]): every row's refusal.
    pub image: Result<&'c [LibKey], &'c str>,
    said: AString<'c>,
}

impl Refuse for EntryCheck<'_, '_> {
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError> {
        let exports = match self.image {
            Ok(exports) => exports,
            Err(why) => {
                self.said.clear();
                write!(self.said, "its test program's native image: {why}").map_err(|_| OneError::Profile { why: Box::from("a refusal's words did not fit in memory") })?;
                return Ok(Some(self.said.as_str()));
            }
        };
        let test = key_of(&self.tests, row)?;
        let frame = Frame::new(bstd::mem::Tag::Other).map_err(OneError::Frame)?;
        let erased = blaze_own::Erased::new(&frame);
        let made = entry_roots(self.tests.harnessed(), self.names, test, exports, self.builder, self.tables, erased.handle());
        let Err(why) = made else { return Ok(None) };
        self.said.clear();
        write!(self.said, "{why}").map_err(|_| OneError::Profile { why: Box::from("a refusal's words did not fit in memory") })?;
        Ok(Some(self.said.as_str()))
    }
}

/// Where `blaze test`'s code is made (D194, rule 78: THE seam, one line to flip). `Serial`: in
/// the turn of the test that asked, on the command thread (today's; one long make holds every
/// other test's answer). `Pooled`: each make is handed to a worker through [`CodeRelay`], the
/// command thread only relays, and the asking test is parked, its allowance stopped, until its
/// code comes or [`CODE_WAIT`] passes. `Pooled` for the real command needs [`CodeWorkers`] made
/// on the command's pool (piece 2 of D194, [`pooled`]); with no worker beside the command
/// thread, [`run_suites`] refuses it by name (`DriveError::NoCodeWorkers`), never runs serially.
pub(crate) const CODES: Codes = Codes::Serial;

/// How long one test's ask for its code may wait for a worker's make (rule 18): past it the test
/// is stopped with [`OneError::CodeLate`] naming the instance. The longest make met on tempo was
/// 77 s (the first real run's stop 3); this is about four of those.
pub(crate) const CODE_WAIT: Duration = Duration::from_secs(300);

/// The most asks one command's relay takes over its life: the pool's demand budget (rule 18).
const MOST_ASKS: usize = 1 << 20;

/// The most bytes of made entries the relay keeps at once for later askers (rule 18).
const MOST_KEPT: usize = 1 << 30;

/// One make handed to a worker (D194): all it needs, owned, so it crosses to the worker's thread.
pub(crate) struct Job<'a> {
    /// The suite whose names records the make reads.
    pub suite: SuiteId,
    /// The row's tabled fn and kind (body_ids.rs `Order`).
    pub order: blaze_ladder::body_ids::Order,
    /// The row's instance name, copied out of the asking test's walk.
    pub name: SVec<'a, u8>,
    /// The gate's proof for the row's fn, which the make takes (D163).
    pub proof: crate::run_ids::Runnable,
    /// The asking test's roots as its maker is made of them ([`TestFront::maker`]).
    pub entry: Option<ItemKey>,
    pub start: MakerStart,
}

/// A worker's refusal of one make: the walk's error of it, as the serial step's would be.
pub(crate) type Unmade = blaze_ladder::body_ids::WalkError<blaze_ladder::run::RunError>;

/// The command's workers as the relay sees them (D194): each is handed at most one make at a
/// time by the relay, and nothing here waits (the command thread only relays).
pub(crate) trait CodeWorkers<'a> {
    /// How many workers: their ids are dense from zero.
    fn count(&self) -> usize;
    /// Hands `job` to `worker`, which the relay gave no other make in flight.
    fn give(&self, worker: Worker, job: Job<'a>) -> Result<(), OneError>;
    /// One make finished since the last call, if any: the entry's bytes, or the make's refusal;
    /// `Err`: what the workers asked of the command thread meanwhile could not be answered.
    fn finished(&self) -> Result<Option<(Worker, Result<SVec<'a, u8>, CodeRefusal>)>, OneError>;
}

/// The relay of D194 over the command's [`CodeWorkers`], shared by every test's run on the
/// command thread. Its state is borrowed only inside one call, never across a turn (rule 27).
///
/// What it holds (its asks, the made entries kept for later askers, at most [`MOST_KEPT`] bytes:
/// past it a make is the relay's `Full`) is in the command's own heap, handed in (`heap.shared()`),
/// and freed with the relay at the command's end. Its data is `'static` for that reason, and its
/// one lifetime is its borrows', so a run may borrow it for less than it lives (`Launched::codes`).
pub(crate) struct CodeRelay<'a> {
    relay: RefCell<PoolRelay<'static, Job<'static>, CodeRefusal>>,
    workers: &'a dyn CodeWorkers<'static>,
    now: &'a dyn Fn() -> Nanos,
    alloc: SharedAlloc<'static>,
    /// A step refused where no error could be returned (a run's end, [`Asking`]'s drop): said by
    /// the next [`CodeRelay::pump`], so nothing is quietly dropped (rule 3).
    fault: Cell<Option<RelayError>>,
}

impl<'a> CodeRelay<'a> {
    /// A relay for `children` tests (ids dense from zero, [`Launched::child_base`]) over `workers`,
    /// on the clock `now`; `hash_seed` is the command's (rule 75).
    pub(crate) fn new(children: usize, workers: &'a dyn CodeWorkers<'static>, now: &'a dyn Fn() -> Nanos, alloc: SharedAlloc<'static>, hash_seed: u64) -> Result<CodeRelay<'a>, RelayError> {
        let opts = code_relay::Options {
            alloc,
            hash_seed,
            children,
            workers: workers.count(),
            asks: MOST_ASKS,
            // Each ask's own `code_wait` bounds every wait a test makes on the relay; the makes
            // themselves are bounded by the command's deadline, which the task carries.
            deadline: Time(u64::MAX),
            code_wait: u64::try_from(CODE_WAIT.as_nanos()).unwrap_or(u64::MAX),
            most_bytes: MOST_KEPT,
        };
        let relay = PoolRelay::new(Time(now().0), opts)?;
        Ok(CodeRelay { relay: RefCell::new(relay), workers, now, alloc, fault: Cell::new(None) })
    }

    fn time(&self) -> Time {
        Time((self.now)().0)
    }

    /// One turn of relaying, never waiting: every finished make given back, the asks' deadlines
    /// ticked, every make the pool wants handed to its worker.
    pub(crate) fn pump(&self) -> Result<(), OneError> {
        if let Some(fault) = self.fault.take() {
            return Err(OneError::Relay(fault));
        }
        let now = self.time();
        let mut relay = self.relay.borrow_mut();
        while let Some((worker, result)) = self.workers.finished()? {
            match result {
                Ok(bytes) => relay.made(now, worker, bytes),
                Err(why) => relay.refused(now, worker, why),
            }
            .map_err(OneError::Relay)?;
        }
        relay.tick(now).map_err(OneError::Relay)?;
        while let Some((worker, _, job)) = relay.job() {
            self.workers.give(worker, job)?;
        }
        Ok(())
    }

    /// Child `child` asks for the code of `key` (`None`: unkeyed, its own), to be made by `job`.
    pub(crate) fn ask(&self, child: Child, key: Option<blaze_ladder::code_records::CodeKey>, job: Job<'static>) -> Result<(), OneError> {
        let key = key.map(|key| blaze_engine::code_pool::CodeKey(key.0));
        self.relay.borrow_mut().ask(self.time(), child, key, job).map_err(OneError::Relay)?;
        self.pump()
    }

    /// Child `child`'s answer through `with`, once it came; `None` while it has not.
    pub(crate) fn answer<R>(&self, child: Child, with: impl FnOnce(Reply<'_, CodeRefusal>) -> R) -> Result<Option<R>, OneError> {
        self.relay.borrow_mut().answer(child, with).map_err(OneError::Relay)
    }
}

/// A test's place in the relay for as long as its run lives: its end tells the relay, however
/// the run ends (its own end, an error, or its future dropped by [`OneEach::kill`]).
struct Asking<'r, 'a> {
    code: &'r CodeRelay<'a>,
    child: Child,
}

impl Drop for Asking<'_, '_> {
    fn drop(&mut self) {
        let now = self.code.time();
        if let Err(fault) = self.code.relay.borrow_mut().exited(now, self.child)
            && self.code.fault.get().is_none()
        {
            self.code.fault.set(Some(fault));
        }
    }
}

/// How long a hand-over on a code workers' channel may wait (rule 18): a make to its worker's
/// inbox, a made entry to the done channel, an answer of the proc-macro runner. Each has room by
/// construction (the doc of [`Lanes`]), so the wait is for the channel's lock alone, held for a
/// move.
const HAND_WAIT: Duration = Duration::from_secs(5);

/// How long the command waits, after its run, for its code workers to end (rule 18): a make in
/// flight is not cancelled inside the maker, so this is a make's bound ([`CODE_WAIT`]). Past it
/// the command fails naming the workers ([`DriveError::CodeWorkers`]), never waits on.
const END_WAIT: Duration = CODE_WAIT;

/// The most makers one code worker keeps (rule 18: a table has a cap): one a (suite, entry,
/// start) its makes asked, the oldest dropped first. A test's roots start at its fn, so this is
/// about the tests whose code one worker made lately; a maker dropped is made again, cheaply
/// (make_one.rs `Maker::of_start` reads nothing), its program state lowered again by need.
const MOST_MAKERS: usize = 64;

/// The scratch of a code worker's task (bstd `Scope::must_overlap`): its waits and spans; its
/// names records and makers are in a frame of its own ([`work`]).
const WORKER_SCRATCH: usize = 1 << 20;

/// The words of a make that panicked on its worker (rule 20): that make alone fails, typed, and
/// the worker forgets what it kept for the suite ([`KeptMakers::forget`]).
const PANICKED: &str = "the make of this instance panicked on its code worker; the worker forgot this suite's names records and makers (test_drive.rs `make_one`, D194)";

/// The words of a make handed a maker of another start (the seam test's control, rule 78).
const ANOTHER_START: &str = "the make was handed a maker made for another entry or start than its test's roots say (test_drive.rs `Makers::of`, D194)";

/// What a code worker sends back: its id, and the entry it made or the make's refusal.
/// The refusal crosses as a [`CodeRefusal`]: `Unmade` is not `Send`.
pub(crate) type Done = (Worker, Result<SVec<'static, u8>, CodeRefusal>);

/// The command's proc-macro runner as a worker may reach it (D194 piece 2): the runner itself,
/// made already (`Sync`: check's stretches share it, engine_bodies.rs `Shared::procs`); asked of
/// the command thread at a worker's first call, as the command's by-need maker is the command
/// thread's (engine_start.rs `RunProcs`, over its context and frame); or none.
#[derive(Clone, Copy)]
pub(crate) enum ProcsSide<'p> {
    None,
    Made(&'p crate::proc_runner::ProcRunner<'static>),
    ByNeed,
}

impl<'p> ProcsSide<'p> {
    pub(crate) fn of(procs: crate::job_texts::Procs<'p>) -> ProcsSide<'p> {
        match procs {
            crate::job_texts::Procs::None => ProcsSide::None,
            crate::job_texts::Procs::Made(made) => ProcsSide::Made(made),
            crate::job_texts::Procs::ByNeed(_) => ProcsSide::ByNeed,
        }
    }
}

/// The channels between the command thread and its code workers (D194 piece 2), in the
/// command's heap (handed in), every one bounded and every one with room by construction, so
/// a hand-over waits only for a channel's lock ([`HAND_WAIT`]):
/// - each worker's inbox holds one make: the relay hands a worker its next make only once its
///   last came back (code_pool_relay.rs `Relay::job`);
/// - the done channel holds one entry a worker: a worker sends one, then waits for its next;
/// - a worker asks for the proc-macro runner at most once (its memo, [`WorkerProcs`]), and its
///   answer has one place.
///
/// No lock is held across a wait: a channel's lock is held for a move (bstd sync/chan.rs). Each
/// wait is named by a span and bounded by a deadline (rules 15, 18). [`Lanes::close`] ends them
/// all: a worker waiting for its next make sees the close and ends.
pub(crate) struct Lanes<'p> {
    inboxes: SVec<'static, bstd::sync::chan::OwnedChan<Job<'static>, SharedAlloc<'static>>>,
    done: bstd::sync::chan::OwnedChan<Done, SharedAlloc<'static>>,
    procs_asked: bstd::sync::chan::OwnedChan<Worker, SharedAlloc<'static>>,
    procs_answers: SVec<'static, bstd::sync::chan::OwnedChan<crate::job_texts::MadeProcs<'p>, SharedAlloc<'static>>>,
}

impl<'p> Lanes<'p> {
    /// The channels of `count` workers, in `heap`.
    pub(crate) fn new(count: core::num::NonZeroUsize, heap: &'static blaze_own::Heap) -> Lanes<'p> {
        use bstd::sync::chan::owned;
        let alloc = heap.shared();
        let one = core::num::NonZeroUsize::MIN;
        let mut inboxes = SVec::with_capacity_in(count.get(), alloc);
        let mut procs_answers = SVec::with_capacity_in(count.get(), alloc);
        for _ in 0..count.get() {
            inboxes.push(owned("blaze test: a code worker's inbox", one, alloc));
            procs_answers.push(owned("blaze test: a code worker's proc-macro runner", one, alloc));
        }
        Lanes {
            inboxes,
            done: owned("blaze test: the code workers' made entries", count, alloc),
            procs_asked: owned("blaze test: the code workers' asks for the proc-macro runner", count, alloc),
            procs_answers,
        }
    }

    /// Worker `worker`'s ends.
    pub(crate) fn lane(&self, worker: usize) -> WorkerLane<'_, 'p> {
        WorkerLane { me: Worker(worker), lanes: self }
    }

    /// Every channel closed: each worker's next wait ends, and a worker's late send is refused
    /// as `Closed` (the command ended: its entry is dropped, [`WorkerLane::answer`]).
    pub(crate) fn close(&self) {
        for inbox in self.inboxes.iter() {
            inbox.close();
        }
        for answer in self.procs_answers.iter() {
            answer.close();
        }
        self.done.close();
        self.procs_asked.close();
    }

    /// What was sent and not taken, dropped once the workers ended: entries made for tests that
    /// ended (rule 3: by design, said here; the relay drops a result for a child that exited the
    /// same way, code_pool_relay.rs `Relay::made`). How many.
    pub(crate) fn drain(&self) -> usize {
        let mut dropped = 0;
        while self.done.try_recv().is_ok() {
            dropped += 1;
        }
        dropped
    }
}

/// Why a code worker ended other than by the command's close (each names its wait, rule 19).
#[derive(Debug)]
pub(crate) enum WorkerEnd {
    /// Its frame for its names records could not be made.
    Frame(blaze_mem::MemError),
    /// The wait `what` stopped so (its deadline, or the scope's cancel).
    Stopped { what: &'static str, stopped: bstd::Stopped },
    /// No inbox for this worker: the pool made fewer lanes than workers.
    NoLane { worker: usize },
    /// The channel `chan` answered what its use rules out ([`Lanes`]): no room for a send, or
    /// empty after a wait that ends only with a value, the close, or a stop.
    Full { chan: &'static str },
}

impl fmt::Display for WorkerEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkerEnd::Frame(error) => write!(f, "a code worker's frame could not be made: {error:?}"),
            WorkerEnd::Stopped { what, stopped } => write!(f, "a code worker's wait ({what}) stopped: {stopped}"),
            WorkerEnd::NoLane { worker } => write!(f, "code worker {worker} has no channels (test_drive.rs `Lanes`)"),
            WorkerEnd::Full { chan } => write!(f, "the code workers' channel `{chan}` had no room, which its construction rules out (test_drive.rs `Lanes`)"),
        }
    }
}

/// One worker's makers (rule 78: the command's suites' [`SuiteMakers`], or a test's): each make
/// handed to the maker of its own suite and start.
pub(crate) trait KeptMakers {
    /// The entry of `job` appended to `entry`.
    fn make(&mut self, job: &Job<'static>, entry: &mut AVec<'_, u8>) -> Result<(), Unmade>;
    /// Everything kept for `suite` forgotten (after one of its makes panicked).
    fn forget(&mut self, suite: SuiteId);
    /// `job`'s refused instance named, and `refusal.at` placed in its file, by the kept maker
    /// that refused it, into `refusal` (in the command's heap): what the asking test's NOT RUN
    /// prints, as the serial step's names and places it (run_parent.rs `Lazy::name_of`,
    /// `Lazy::site_of`). A maker never made (the make refused before it) names nothing. Read
    /// only on a refusal's path.
    fn shown(&mut self, job: &Job<'static>, refusal: &mut CodeRefusal);
}

/// The one make of a worker (sans-IO, rule 14): `job` made into `scratch` by `makers`, copied
/// into the command's heap (`alloc`, handed in); a panic in it fails this make alone, typed,
/// naming the instance's fn (rule 20; the asking test's NOT RUN names the instance, run_parent.rs
/// `Lazy::refused`), and the suite's state is forgotten, as it may be half made. The refusal
/// crosses as a [`CodeRefusal`] ([`crossing`]).
pub(crate) fn make_one(makers: &mut dyn KeptMakers, job: &Job<'static>, scratch: &mut AVec<'_, u8>, alloc: SharedAlloc<'static>) -> Result<SVec<'static, u8>, CodeRefusal> {
    scratch.clear();
    match crate::work::unwinding(|| makers.make(job, scratch)) {
        Ok(Ok(())) => {
            let mut made = SVec::with_capacity_in(scratch.len(), alloc);
            made.extend_from_slice(scratch.as_slice());
            Ok(made)
        }
        Ok(Err(why)) => {
            let mut refusal = crossing(why, alloc);
            makers.shown(job, &mut refusal);
            Err(refusal)
        }
        // The suite's makers are forgotten (half made): nothing names the instance but its fn.
        Err(_panicked) => {
            makers.forget(job.suite);
            Err(crossing(refused(job, PANICKED), alloc))
        }
    }
}

/// `why` as it crosses to the command thread (run_parent.rs [`CodeRefusal`]): whose and where by
/// [`instance_site`] over [`ParentError::scope`], the serial path's own sorting, and its words as [`InstanceRefusal`] and
/// `ParentError::Walk` print them, in `alloc` (the command's heap).
pub(crate) fn crossing(why: Unmade, alloc: SharedAlloc<'static>) -> CodeRefusal {
    use blaze_ladder::body_ids::WalkError;

    let mut whole = true;
    let walk = words(&why, alloc, &mut whole);
    let cause = match &why {
        WalkError::Make { why: blaze_ladder::run::RunError::Build { error, .. }, .. } => Some(words(error, alloc, &mut whole)),
        WalkError::Make { why, .. } => Some(words(why, alloc, &mut whole)),
        _ => None,
    };
    // The symbol is the command thread's (its asking row's), and the sorting does not read it.
    let at = instance_site(&ParentError::Walk { symbol: [0; 32], why }).and_then(|(_, at)| at);
    // Named and placed by the maker that refused it ([`KeptMakers::shown`]).
    CodeRefusal { at, cause, walk, instance: None, site: None, whole }
}

/// `shown`'s words in `alloc`, at most [`run_parent::MOST_REFUSAL_WORDS`] bytes (cut at a char's
/// edge); `whole` cleared when they were cut there or the heap refused part of them.
fn words(shown: &dyn fmt::Display, alloc: SharedAlloc<'static>, whole: &mut bool) -> String<SharedAlloc<'static>> {
    let mut words = Capped { into: String::new_in(alloc) };
    if write!(words, "{shown}").is_err() {
        *whole = false;
    }
    words.into
}

/// Words written up to [`run_parent::MOST_REFUSAL_WORDS`] bytes: past it the write fails, as
/// the heap's refusal does, so [`words`] says the cut.
struct Capped {
    into: String<SharedAlloc<'static>>,
}

impl fmt::Write for Capped {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = run_parent::MOST_REFUSAL_WORDS.saturating_sub(self.into.len());
        if text.len() <= room {
            return self.into.try_push_str(text).map_err(|_| fmt::Error);
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.into.try_push_str(&text[..end]).map_err(|_| fmt::Error)?;
        Err(fmt::Error)
    }
}

/// `job`'s instance and the place `refusal.at`, as `maker` shows them, into `refusal` (in its
/// words' heap): the serial NOT RUN's printers ([`InstanceName`], lazy fill.rs `site_shown`).
fn shown_by(maker: &TestMaker<'_>, job: &Job<'static>, refusal: &mut CodeRefusal) {
    let alloc = *refusal.walk.allocator();
    let mut whole = refusal.whole;
    if let Some(shown) = maker.instance_shown(job.order.tabled, job.name.as_slice()) {
        let named = crate::run_fault::Named::Shown { item: shown.item, args: shown.args };
        // The symbol is printed only for a row-less name, which `Shown` is not.
        refusal.instance = Some(words(&InstanceName(&named, [0; 32]), alloc, &mut whole));
    }
    if let Some((item, byte)) = refusal.at
        && let Some((file, line, column)) = maker.site_shown(item, byte)
    {
        refusal.site = Some(run_parent::CrossedSite { file: words(&file, alloc, &mut whole), line, column });
    }
    refusal.whole = whole;
}

/// `job`'s refusal for `why`, as a make's (`Order::make`'s `WalkError::Make`).
fn refused(job: &Job<'_>, why: &'static str) -> Unmade {
    let tabled = job.order.tabled;
    blaze_ladder::body_ids::WalkError::Make { tabled, why: blaze_ladder::run::RunError::OneBody { key: tabled, why } }
}

/// Worker `me`'s ends of [`Lanes`].
#[derive(Clone, Copy)]
pub(crate) struct WorkerLane<'l, 'p> {
    me: Worker,
    lanes: &'l Lanes<'p>,
}

/// A worker's wait for its next make.
const WAIT_JOB: &str = "blaze test: a code worker waits for its next make";
/// A worker's hand-over of what it made.
const WAIT_DONE: &str = "blaze test: a code worker hands back what it made";
/// A worker's wait for the command's proc-macro runner.
const WAIT_PROCS: &str = "blaze test: a code worker waits for the command's proc-macro runner";

impl<'l, 'p> WorkerLane<'l, 'p> {
    fn inbox(&self) -> Result<&'l bstd::sync::chan::OwnedChan<Job<'static>, SharedAlloc<'static>>, WorkerEnd> {
        self.lanes.inboxes.get(self.me.0).ok_or(WorkerEnd::NoLane { worker: self.me.0 })
    }

    /// The worker's loop: each make waited for (named, until `cx`'s deadline, which is the
    /// command's, and ended by the close), made and handed back; `Ok` once the command closed it.
    pub(crate) fn run(self, cx: &Cx, clock: &bstd::clock::Clock, makers: &mut dyn KeptMakers, heap: &'static blaze_own::Heap) -> Result<(), WorkerEnd> {
        use bstd::sync::RecvError;
        let inbox = self.inbox()?;
        let mut scratch: AVec<'static, u8> = Vec::new_in(heap.alloc());
        loop {
            let waiting = cx.span(WAIT_JOB);
            let job = match inbox.recv(&waiting) {
                Ok(job) => job,
                Err(RecvError::Closed { .. }) => return Ok(()),
                Err(RecvError::Empty { chan }) => return Err(WorkerEnd::Full { chan }),
                Err(RecvError::Stopped(stopped)) => return Err(WorkerEnd::Stopped { what: WAIT_JOB, stopped }),
            };
            drop(waiting);
            self.answer(cx, clock, makers, &job, &mut scratch, heap)?;
        }
    }

    /// The seam test's turn of this worker (rule 78: the same make and hand-back as [`run`]'s,
    /// driven inline): the make in its inbox now, if one is, made and handed back. Whether one
    /// was.
    #[cfg_attr(not(test), expect(dead_code, reason = "the seam test's inline worker (run_parent_tests.rs `pooled_codes_equal_serial`)"))]
    pub(crate) fn step(self, cx: &Cx, clock: &bstd::clock::Clock, makers: &mut dyn KeptMakers, scratch: &mut AVec<'_, u8>, heap: &'static blaze_own::Heap) -> Result<bool, WorkerEnd> {
        match self.inbox()?.try_recv() {
            Ok(job) => {
                self.answer(cx, clock, makers, &job, scratch, heap)?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    /// `job` made and handed back. A done channel closed is the command's end: what was made
    /// is dropped (its asker is gone), by design and said here, never an error of the worker.
    fn answer(self, cx: &Cx, clock: &bstd::clock::Clock, makers: &mut dyn KeptMakers, job: &Job<'static>, scratch: &mut AVec<'_, u8>, heap: &'static blaze_own::Heap) -> Result<(), WorkerEnd> {
        use bstd::sync::SendError;
        let made = {
            let _making = cx.span("blaze test: a code worker makes one entry");
            make_one(makers, job, scratch, heap.shared())
        };
        let by = cx.until(Deadline::after(clock.now().since_start(), HAND_WAIT));
        let handing = by.span(WAIT_DONE);
        match self.lanes.done.send(&handing, (self.me, made)) {
            Ok(()) | Err(SendError::Closed(_)) => Ok(()),
            Err(SendError::Full { chan, .. }) => Err(WorkerEnd::Full { chan }),
            Err(SendError::Stopped { stopped, .. }) => Err(WorkerEnd::Stopped { what: WAIT_DONE, stopped }),
        }
    }

    /// The command's proc-macro runner, asked of the command thread ([`CodePool::finished`]
    /// answers it): named, and bounded by [`CODE_WAIT`] (making the runner is the command's
    /// longest one-off, engine_start.rs `RunProcs::make`).
    fn ask_procs(self, cx: &Cx, clock: &bstd::clock::Clock) -> crate::job_texts::MadeProcs<'p> {
        let Some(answer) = self.lanes.procs_answers.get(self.me.0) else {
            return Err("no proc-macro runner: this code worker has no channel to ask for it (test_drive.rs `Lanes`)");
        };
        let by = cx.until(Deadline::after(clock.now().since_start(), CODE_WAIT));
        let waiting = by.span(WAIT_PROCS);
        if self.lanes.procs_asked.send(&waiting, self.me).is_err() {
            return Err("no proc-macro runner: the command ended, or its hand-over stopped, before this code worker's ask for it was taken (test_drive.rs `WorkerLane::ask_procs`)");
        }
        match answer.recv(&waiting) {
            Ok(made) => made,
            Err(_) => Err("no proc-macro runner: the command ended, or `CODE_WAIT` passed, before it answered this code worker's ask (test_drive.rs `WorkerLane::ask_procs`)"),
        }
    }
}

/// A worker's proc-macro runner by need (job_texts.rs `ProcsByNeed`): asked of the command
/// thread at its first call, once (the same answer on every later call, as `MadeOnce`).
pub(crate) struct WorkerProcs<'w, 'p> {
    cx: &'w Cx,
    clock: &'w bstd::clock::Clock,
    lane: WorkerLane<'w, 'p>,
    got: core::cell::OnceCell<crate::job_texts::MadeProcs<'p>>,
}

impl<'w, 'p: 'w> crate::job_texts::ProcsByNeed<'w> for WorkerProcs<'w, 'p> {
    fn procs(&self) -> Result<&'static crate::proc_runner::ProcRunner<'static>, &'w str> {
        *self.got.get_or_init(|| self.lane.ask_procs(self.cx, self.clock))
    }
}

/// What a maker was made for (make_one.rs `Maker::made_for`): its entry and start.
pub(crate) trait MadeFor {
    fn made_for(&self) -> (Option<ItemKey>, MakerStart);
}

impl MadeFor for TestMaker<'_> {
    fn made_for(&self) -> (Option<ItemKey>, MakerStart) {
        Maker::made_for(self)
    }
}

/// A PLANTED BUG for the seam test's control (skill `testing`): `BySuiteAlone` finds a kept
/// maker by its suite alone, so a make may be handed a maker of another test's start, which
/// [`Makers::of`] must catch. Always `None` outside the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MakersPlant {
    None,
    BySuiteAlone,
}

/// One worker's makers, one a (suite, entry, start) its makes asked, made at the first and kept
/// (at most [`MOST_MAKERS`], the oldest dropped first), in the command's heap.
pub(crate) struct Makers<M> {
    kept: AVec<'static, (SuiteId, Option<ItemKey>, MakerStart, M)>,
    plant: MakersPlant,
}

impl<M: MadeFor> Makers<M> {
    pub(crate) fn new(alloc: Alloc<'static>, plant: MakersPlant) -> Makers<M> {
        Makers { kept: Vec::new_in(alloc), plant }
    }

    /// The maker of `job`'s suite, entry and start: kept, or made by `make` and kept. A kept
    /// maker made for another entry or start than `job`'s is refused by name (its control:
    /// [`MakersPlant::BySuiteAlone`]), never used.
    pub(crate) fn of(&mut self, job: &Job<'_>, make: impl FnOnce() -> Result<M, Unmade>) -> Result<&mut M, Unmade> {
        let plant = self.plant;
        let found = self.kept.iter().position(|(suite, entry, start, _)| *suite == job.suite && (plant == MakersPlant::BySuiteAlone || (*entry == job.entry && *start == job.start)));
        let at = match found {
            Some(at) => at,
            None => {
                let maker = make()?;
                if self.kept.len() >= MOST_MAKERS {
                    // The oldest first: at most `MOST_MAKERS` moved, once a maker is made.
                    drop(self.kept.remove_shift(0));
                }
                self.kept.push((job.suite, job.entry, job.start, maker));
                self.kept.len() - 1
            }
        };
        let Some((_, _, _, maker)) = self.kept.get_mut(at) else { return Err(refused(job, ANOTHER_START)) };
        if maker.made_for() != (job.entry, job.start) {
            return Err(refused(job, ANOTHER_START));
        }
        Ok(maker)
    }

    pub(crate) fn forget(&mut self, suite: SuiteId) {
        self.kept.retain(|(kept, ..)| *kept != suite);
    }

    /// The kept maker of `job`'s suite, entry and start, never made here: `None` when none is kept.
    pub(crate) fn kept(&self, job: &Job<'_>) -> Option<&M> {
        let (_, _, _, maker) = self.kept.iter().find(|(suite, entry, start, _)| *suite == job.suite && *entry == job.entry && *start == job.start)?;
        Some(maker)
    }
}

/// What one suite's makes read, shared read only by every worker (run_names.rs `RunCrates`):
/// its crates and its builder.
#[derive(Clone, Copy)]
pub(crate) struct SuiteSide<'s> {
    pub crates: crate::run_names::RunCrates<'s>,
    pub builder: Builder,
    /// The command's make watch ([`Launched::make_watch`]): every worker's makers are handed it.
    pub make_watch: blaze_ladder::run::make_one::MakeWatch<'static>,
}

/// The words of a make whose suite's names records could not be made on its worker.
const NO_NAMES: &str = "its suite's names records could not be made on the code worker (run_names.rs `names_in`, D194)";
/// The words of a make naming a suite the command does not have.
const NO_SUITE: &str = "the make names a suite this command does not have (test_drive.rs `SuiteMakers`, D194)";

/// THE [`KeptMakers`] of the command (D194 piece 2): on one worker, each suite's names records
/// made at its first make, of the suite's shared [`RunCrates`](crate::run_names::RunCrates),
/// in the worker's own frame (its trees, environments and records' memo its own), and each
/// (suite, entry, start)'s maker over them, made as the asking test's serial step would make it
/// (`TestFront::maker`: the same entry, start, typing and builder).
pub(crate) struct SuiteMakers<'w> {
    cx: &'w Cx,
    root: &'w Dir<ReadWrite>,
    sides: &'w [SuiteSide<'w>],
    procs: crate::job_texts::Procs<'w>,
    frame: &'w Frame,
    tables: Alloc<'static>,
    names: AVec<'static, Option<NamesRecords<'w>>>,
    makers: Makers<TestMaker<'w>>,
}

impl<'w> SuiteMakers<'w> {
    pub(crate) fn new(cx: &'w Cx, root: &'w Dir<ReadWrite>, sides: &'w [SuiteSide<'w>], procs: crate::job_texts::Procs<'w>, frame: &'w Frame, heap: &'static blaze_own::Heap) -> SuiteMakers<'w> {
        let mut names = Vec::with_capacity_in(sides.len(), heap.alloc());
        names.resize(sides.len(), None);
        SuiteMakers { cx, root, sides, procs, frame, tables: heap.alloc(), names, makers: Makers::new(heap.alloc(), MakersPlant::None) }
    }
}

impl KeptMakers for SuiteMakers<'_> {
    fn make(&mut self, job: &Job<'static>, entry: &mut AVec<'_, u8>) -> Result<(), Unmade> {
        let at = job.suite.0 as usize;
        let side = *self.sides.get(at).ok_or_else(|| refused(job, NO_SUITE))?;
        let names = match self.names.get(at).copied().flatten() {
            Some(names) => names,
            None => {
                let frame = self.frame;
                // Its own count of the trees it opened: the command's closing line counts the
                // command thread's (run_names.rs `NamesRead`), a worker's are its own.
                let seen: &crate::body_types::TreesSeen = frame.alloc(crate::body_types::TreesSeen::default());
                let names = crate::run_names::names_in(self.cx, self.root, side.crates, self.procs, seen, frame).map_err(|_| refused(job, NO_NAMES))?;
                if let Some(slot) = self.names.get_mut(at) {
                    *slot = Some(names);
                }
                names
            }
        };
        let tables = self.tables;
        let maker = self.makers.of(job, || {
            let mut maker = Maker::of_start(names, job.entry, job.start, SecondCheck, &side.builder, tables);
            maker.watch(side.make_watch);
            Ok(maker)
        })?;
        job.order.make(maker, &job.proof, job.name.as_slice(), entry)
    }

    fn forget(&mut self, suite: SuiteId) {
        self.makers.forget(suite);
        if let Some(slot) = self.names.get_mut(suite.0 as usize) {
            *slot = None;
        }
    }

    fn shown(&mut self, job: &Job<'static>, refusal: &mut CodeRefusal) {
        if let Some(maker) = self.makers.kept(job) {
            shown_by(maker, job, refusal);
        }
    }
}

/// One code worker's task (D194 piece 2): its frame, its proc-macro runner by need, its
/// makers over every suite, then its loop until the command closes its lane.
pub(crate) fn work(cx: &Cx, lane: WorkerLane<'_, '_>, clock: &bstd::clock::Clock, sides: &[SuiteSide<'_>], root: &Dir<ReadWrite>, procs: ProcsSide<'_>, heap: &'static blaze_own::Heap) -> Result<(), WorkerEnd> {
    let frame = Frame::new(bstd::mem::Tag::Other).map_err(WorkerEnd::Frame)?;
    let by_need = WorkerProcs { cx, clock, lane, got: core::cell::OnceCell::new() };
    let procs = match procs {
        ProcsSide::None => crate::job_texts::Procs::None,
        ProcsSide::Made(made) => crate::job_texts::Procs::Made(made),
        ProcsSide::ByNeed => crate::job_texts::Procs::ByNeed(&by_need),
    };
    let mut makers = SuiteMakers::new(cx, root, sides, procs, &frame, heap);
    lane.run(cx, clock, &mut makers, heap)
}

/// THE [`CodeWorkers`] of the command (D194 piece 2), on the command thread: a make handed to
/// its worker's inbox, a finished one taken from the done channel, and, at each turn, every
/// worker's ask for the proc-macro runner answered from the command's own (`procs`, made by need
/// on this thread at the first ask, engine_start.rs `RunProcs`). Never waits but for a
/// channel's lock ([`HAND_WAIT`]).
pub(crate) struct CodePool<'l, 'p> {
    pub lanes: &'l Lanes<'p>,
    /// How many workers run: the lanes beyond are unused.
    pub count: usize,
    pub cx: &'l Cx,
    pub clock: &'l bstd::clock::Clock,
    pub procs: crate::job_texts::Procs<'p>,
}

impl CodePool<'_, '_> {
    fn hand_off(&self, chan: &'static str, worker: usize, why: &'static str) -> OneError {
        OneError::CodeHandOff { chan, worker, why }
    }

    /// Every worker's ask for the proc-macro runner answered.
    fn serve_procs(&self) -> Result<(), OneError> {
        use bstd::sync::SendError;
        while let Ok(worker) = self.lanes.procs_asked.try_recv() {
            let made = match self.procs {
                crate::job_texts::Procs::None => Err("no proc-macro runner: this command has none (job_texts.rs `Procs::None`)"),
                // Handed made to every worker (`ProcsSide::Made`), which then never asks.
                crate::job_texts::Procs::Made(_) => Err("no proc-macro runner by ask: this command handed its runner made to every code worker, and a worker asked anyway (test_drive.rs `ProcsSide`, a bug)"),
                crate::job_texts::Procs::ByNeed(maker) => maker.procs(),
            };
            let chan = "blaze test: a code worker's proc-macro runner";
            let answer = self.lanes.procs_answers.get(worker.0).ok_or_else(|| self.hand_off(chan, worker.0, "does not exist"))?;
            let by = self.cx.until(Deadline::after(self.clock.now().since_start(), HAND_WAIT));
            let handing = by.span("blaze test: answer a code worker's ask for the proc-macro runner");
            match answer.send(&handing, made) {
                Ok(()) => {}
                Err(SendError::Closed(_)) => return Err(self.hand_off(chan, worker.0, "was closed")),
                Err(SendError::Full { .. }) => return Err(self.hand_off(chan, worker.0, "had no room")),
                Err(SendError::Stopped { .. }) => return Err(self.hand_off(chan, worker.0, "stopped its hand-over (`HAND_WAIT`)")),
            }
        }
        Ok(())
    }
}

impl CodeWorkers<'static> for CodePool<'_, '_> {
    fn count(&self) -> usize {
        self.count
    }

    fn give(&self, worker: Worker, job: Job<'static>) -> Result<(), OneError> {
        use bstd::sync::SendError;
        let chan = "blaze test: a code worker's inbox";
        let inbox = self.lanes.inboxes.get(worker.0).filter(|_| worker.0 < self.count).ok_or_else(|| self.hand_off(chan, worker.0, "does not exist"))?;
        let by = self.cx.until(Deadline::after(self.clock.now().since_start(), HAND_WAIT));
        let handing = by.span("blaze test: hand a make to its code worker");
        match inbox.send(&handing, job) {
            Ok(()) => Ok(()),
            Err(SendError::Closed(_)) => Err(self.hand_off(chan, worker.0, "was closed")),
            Err(SendError::Full { .. }) => Err(self.hand_off(chan, worker.0, "had no room")),
            Err(SendError::Stopped { .. }) => Err(self.hand_off(chan, worker.0, "stopped its hand-over (`HAND_WAIT`)")),
        }
    }

    fn finished(&self) -> Result<Option<Done>, OneError> {
        self.serve_procs()?;
        // Empty now (or the channel's lock taken this instant): the next turn takes it.
        Ok(self.lanes.done.try_recv().ok())
    }
}

/// A test's run parked on its code (D194): since when (the run's clock) and for which symbol.
#[derive(Clone, Copy)]
struct Parked {
    since: Duration,
    symbol: [u8; 32],
}

/// What a parked run's answer was, taken out of the relay's borrow.
enum Came {
    Made,
    Failed(ParentError),
    Late { waited: u64 },
}

/// A test's run under the lazy parent, as `blaze run`'s program runs (own_process.rs
/// `run_own`), with the test's roots, its own folder and profile, piped output and a deadline.
pub(crate) struct Launched<'c, 'h, 'a> {
    pub cx: &'c Cx,
    pub err: &'c bstd::out::Out,
    pub package: &'c str,
    pub target: &'c str,
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
    /// The package's root (each test's working directory) and its run-time variables
    /// ([`PackageRun`]).
    pub place: PackageRun<'c>,
    /// Arguments of a harnessless target's main. A harnessed entry takes none.
    pub args: &'c [&'c str],
    pub tests: TestRows<'c>,
    pub names: NamesRecords<'h>,
    pub ids: &'c RunIds<'h>,
    pub builder: Builder,
    pub lent: &'c CommandLent,
    pub tables: Alloc<'static>,
    pub watch: &'c dyn Watch,
    /// What its makers report their large or slow makes with: the command's one clock
    /// (run_parent.rs `command_make_watch`, made in [`run_suites`]).
    pub make_watch: blaze_ladder::run::make_one::MakeWatch<'static>,
    /// Its suite's native library ([`Suite::native`]), as `blaze run` starts its program
    /// (own_process.rs `run_own`): named in the start (`BLAZE_LAUNCH_NATIVE`), told as the first
    /// frame (`Down::Native`, before the personality and any `Declare`), and the one file the
    /// profile lets the launcher map executable. `None` sends neither.
    pub native: Option<crate::engine_native::NativeSend<'c>>,
    /// Where its code is made ([`CODES`]): `None`, in its own turn (`Serial`); else the
    /// command's relay (`Pooled`, D194).
    pub codes: Option<&'c CodeRelay<'c>>,
    /// Its suite, named in each make it hands a worker ([`Job::suite`]).
    pub suite: SuiteId,
    /// Its suite's first child id in the relay: row `n`'s test is child `child_base + n`.
    pub child_base: usize,
    /// Command-thread sink for walked function metadata observed during runs; borrowed only at
    /// run completion, never across an `.await` (rule 27).
    pub entered_sink: &'c RefCell<AVec<'a, (SuiteId, crate::test_cache::EnteredFn<'a>)>>,
    /// Allocator matching `entered_sink`'s lifetime.
    pub cache_alloc: Alloc<'a>,
}

/// The test profile with the one grant a native image needs: `dlopen` maps `<store>/<64 hex>`
/// executable, which `(deny default)` refuses (blaze-launch-rt native_lib.rs, step 2). That one
/// file, by its literal path, nothing else: it is the parent's own record, named by its key.
fn with_native<'a>(mut profile: AString<'a>, store: &str, native: Option<crate::engine_native::NativeSend<'_>>) -> Result<AString<'a>, OneError> {
    let Some(send) = native else { return Ok(profile) };
    if store.contains(['"', '\\', '\n']) || !send.hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(OneError::Profile { why: Box::from("the native library's path holds a quote, a backslash or a line break, which a profile string cannot hold") });
    }
    profile.push_str("(allow file-map-executable (literal \"");
    profile.push_str(store);
    profile.push_str("/");
    profile.push_str(send.hex);
    profile.push_str("\"))\n");
    Ok(profile)
}

impl RunOne for Launched<'_, '_, '_> {
    async fn run(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<RunOutcome, OneError> {
        let frame = Frame::new(bstd::mem::Tag::Other).map_err(OneError::Frame)?;
        let name = bstd::mem::format_in!(&frame, "stderr-{row}");
        let path = Path::new(name.as_str()).map_err(OneError::Path)?;
        // The parent owns this file beside the child's writable directory. A child cannot
        // replace the evidence. Quiet tests create no file.
        let mut stderr = StderrCapture { dir: self.temps, path, prefix: Vec::new_in(&frame), spilled: false, bytes: 0 };
        let result = self.run_captured(row, budget, output, signals, &mut stderr, &frame).await;
        if stderr.spilled {
            stderr.dir.remove(self.cx, &stderr.path, Durable::No).map_err(OneError::Capture)?;
        }
        result
    }
}

/// The result record has a bounded tail. A live signal diagnostic also needs the complete
/// byte stream from descriptor 2, including non-UTF-8 output and anything before that tail.
struct StderrCapture<'d> {
    dir: &'d Dir<ReadWrite>,
    path: Path,
    prefix: Vec<u8, &'d Frame>,
    spilled: bool,
    bytes: u64,
}

impl StderrCapture<'_> {
    fn push(&mut self, cx: &Cx, bytes: &[u8]) -> Result<(), OneError> {
        if !self.spilled && self.prefix.len() + bytes.len() <= OUTPUT_CAP {
            self.prefix.extend_from_slice(bytes);
        } else {
            let file = if self.spilled {
                self.dir.open(cx, &self.path)
            } else {
                self.dir.create(cx, &self.path)
            }.map_err(OneError::Capture)?;
            self.spilled = true;
            let at = self.bytes - self.prefix.len() as u64;
            let wrote = file.write_at(cx, at, bytes, Durable::No);
            file.close(cx, Durable::No).map_err(OneError::Capture)?;
            wrote.map_err(OneError::Capture)?;
        }
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn print(&self, cx: &Cx, err: &bstd::out::Out) -> Result<(), OneError> {
        err.write_all(cx, &self.prefix).map_err(OneError::Diagnostic)?;
        if !self.spilled {
            return Ok(());
        }
        let file = self.dir.open_read(cx, &self.path).map_err(OneError::Capture)?;
        let mut chunk = [0; 16 * 1024];
        let mut at = 0;
        let spilled = self.bytes - self.prefix.len() as u64;
        while at < spilled {
            let n = file.read_into(&mut chunk, at).wait(cx).map_err(OneError::Capture)?;
            if n == 0 {
                return Err(OneError::ShortCapture { read: self.prefix.len() as u64 + at, written: self.bytes });
            }
            err.write_all(cx, &chunk[..n]).map_err(OneError::Diagnostic)?;
            at += n as u64;
        }
        Ok(())
    }
}

/// One fd 3 request is served per turn. The resolver sees the symbol even for a request
/// by slot, after Needs has looked the slot up in its own declaration table.
struct TestResolve<'r, R> {
    parent: &'r mut R,
    requested: Option<TestAnswer>,
}

enum TestAnswer {
    Ready { symbol: Symbol, record: blaze_launch::wire::RecordId },
    Unresolved(Symbol),
    /// Its row waits for its code, made by a worker (run_parent.rs `Codes::Pooled`, D194): the
    /// need is not taken, and the run parks until the relay answers it.
    Waiting(Symbol),
}

impl<R: Resolve> Resolve for TestResolve<'_, R> {
    #[cfg(test)]
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        self.parent.resolve(symbol)
    }

    fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
        let answer = self.parent.publication(symbol, values)?;
        self.requested = Some(match answer {
            Resolved::Record { record, .. } => TestAnswer::Ready { symbol: *symbol, record },
            Resolved::Unknown => TestAnswer::Unresolved(*symbol),
            Resolved::Waiting => TestAnswer::Waiting(*symbol),
        });
        Ok(answer)
    }
}

struct FirstCall {
    symbol: Symbol,
    elapsed: Duration,
    // Includes any static data this function needed before publication.
    made: usize,
    found: usize,
    unkeyed: usize,
}

impl<'c> Launched<'c, '_, '_> {
    /// The make `lazy`'s waiting row asked for, handed to the relay as child `child`'s ask: its
    /// order, name and proof, and the maker its roots make ([`TestFront::maker`]).
    fn ask_code<'r, D: run_parent::Folder>(&self, code: &'c CodeRelay<'c>, child: Child, lazy: &Lazy<'_, '_, TestFront<'_, '_, 'r>, D>) -> Result<(), OneError> {
        let start = lazy.start().ok_or(OneError::Parent(ParentError::NothingAsked))?;
        let ask = lazy.asked().ok_or(OneError::Parent(ParentError::NothingAsked))?;
        let mut name = SVec::new_in(code.alloc);
        name.extend_from_slice(ask.name);
        let job = Job { suite: self.suite, order: ask.order, name, proof: ask.proof, entry: start.maker_entry(), start: start.maker_start };
        code.ask(child, ask.key, job)
    }

    async fn run_captured(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals, stderr: &mut StderrCapture<'_>, frame: &Frame) -> Result<RunOutcome, OneError> {
        let cx = self.cx;
        let test = key_of(&self.tests, row)?;
        // The test's own frame: everything of its run, dropped with it.
        let erased = blaze_own::Erased::new(frame);
        let alloc = erased.handle();
        // 1. Its temporary folder, made empty: one already there is refused by name, so
        // nothing another run left is read as this one's.
        let name = bstd::mem::format_in!(frame, "{row}");
        let named = Path::new(name.as_str()).map_err(OneError::Path)?;
        self.temps.make_dir(cx, &named, Durable::No).map_err(OneError::Temp)?;
        let temp = bstd::mem::format_in!(frame, "{}/{row}", self.temp);
        // 2. The lazy parent with the test's roots; nothing built yet.
        let exports = self.native.map_or(&[][..], |send| send.items);
        let front = TestFront { names: self.names, ids: self.ids, builder: self.builder, tables: self.tables, alloc, test, exports, harness: self.tests.harnessed(), watch: self.watch, make_watch: self.make_watch };
        let mut codes = HomeCodeStore::new(self.lent);
        // THE SEAM LINE: how the records reach the program (run_bytes.rs `Delivery`; `Inline`
        // sends them over descriptor 3 and files none).
        let mut lazy = Lazy::new(cx, front, Chosen::of(Delivery::Inline, RecordsDir { cx, dir: self.records }, alloc), &mut codes, self.builder, alloc);
        // Its place in the relay, told its end however the run ends (D194).
        let child = Child(self.child_base + row as usize);
        let _asking = self.codes.map(|code| Asking { code, child });
        if self.codes.is_some() {
            lazy.make_by(Codes::Pooled);
        }
        let mut needs = Needs::new(alloc);
        let mut ahead = std::vec::Vec::new_in(alloc);
        let mut placed = Vec::new_in(alloc);
        // The native library first, before any `Declare`: the child hands its entry over only
        // once it has loaded it (own_process.rs `run_own`, the same order).
        if let Some(send) = self.native {
            needs.tell_native(send.library, send.words, &mut ahead).map_err(|cause| test_parent_error(&mut lazy, cause))?;
        }
        signals.begin(Doing::Roots);
        let head = run_parent::ahead_of(None, &mut lazy, &mut needs, &mut placed, &mut ahead)
            .map_err(|cause| test_parent_error(&mut lazy, cause))?;
        let line = head.line;
        let entry = bstd::mem::format_in!(frame, "{line}");
        // 3. Its environment: its own `TMPDIR`, its package's variables (`PackageRun`), then the
        // command's (less the names those set), then the run mode's names.
        let mut vars: Vec<(&str, &str), &Frame> = Vec::with_capacity_in(self.env.len() + self.place.vars.len() + 8, frame);
        vars.push(("TMPDIR", temp.as_str()));
        vars.extend_from_slice(self.place.vars);
        let package = self.place.vars;
        let command = self.env.iter().copied().filter(|(name, _)| *name != "TMPDIR" && package.binary_search_by(|(set, _)| set.cmp(name)).is_err());
        run_parent::launch_env(command, self.store, entry.as_str(), None, &mut vars);
        // `launch_env` left out any `BLAZE_LAUNCH_NATIVE` of the command's own: the parent's word.
        if let Some(send) = self.native {
            vars.push(("BLAZE_LAUNCH_NATIVE", send.hex));
        }
        // 4. The profile (blaze-run child.rs, the one test profile), the launcher under it.
        let profile = blaze_run::child::seatbelt(temp.as_str(), None, alloc).map_err(|why| OneError::Profile { why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) })?;
        let profile = with_native(profile, self.store, self.native)?;
        let mut args = Vec::with_capacity_in(self.args.len() + 3, frame);
        args.extend_from_slice(&["-p", profile.as_str(), self.launcher]);
        args.extend_from_slice(self.args);
        // Its working directory: its package's root, as `cargo test` runs it (`PackageRun`).
        let spec = RunSpec { program: SANDBOX_EXEC, arg0: None, args: args.as_slice(), env: vars.as_slice(), dir: self.place.dir, streams: Streams::PIPED, own_group: true, parent_death: ParentDeath::STRONGEST };
        signals.begin(Doing::Starting);
        let Started { child: mut run, pipes: given } = self.spawner.start_served(cx, &spec).map_err(OneError::Start)?;
        // Its allowance runs from its start, less the time it was parked waiting for its code
        // (D194) or stalled behind command-thread host work: those waits never eat the test's own
        // execution budget.
        let started_at = run.now();
        signals.started.set(true);
        let allowance = Duration::from_nanos(budget.0);
        let mut parked_for = Duration::ZERO;
        let mut parked: Option<Parked> = None;
        // Its code came: the inbox kept from the waiting frame on is served again, unread.
        let mut again = false;
        let stdout = given.stdout.ok_or(OneError::MissingPipe { stream: "stdout" })?;
        let stderr_pipe = given.stderr.ok_or(OneError::MissingPipe { stream: "stderr" })?;
        let mut pipes = [Some(stdout), Some(stderr_pipe)];
        let mut relay = Relay::new(GRACE);
        let mut inbox = std::vec::Vec::new_in(alloc);
        let mut reply = ahead;
        let mut open = true;
        let mut calls = Vec::new_in(alloc);
        let mut published = 0;
        let mut service_time = Duration::ZERO;
        let mut torn = None;
        let mut keep: Result<(), NotKept> = if !self.tests.harnessed() {
            Err(NotKept::Untraced)
        } else if self.native.is_some() {
            Err(NotKept::Foreign)
        } else {
            Ok(())
        };
        let mut entered_fns: AVec<'_, crate::test_cache::EnteredFn<'_>> = Vec::new_in(self.cache_alloc);
        let mut rec_bytes = bstd::mem::Bytes::new_in(frame, "the code record inspected for test purity");
        let child_error = |error| OneError::Loop(RunLoopError::Child(error));
        // Start the other ready children before building any first-call answer. The loop ends
        // with the child's end and what it left (its channel, still read for what it wrote).
        let (ended, mut over) = loop {
            next_turn().await;
            let _span = cx.span("blaze test: service one child's pipes, requests and deadline");
            signals.begin(Doing::Relaying);
            let now = run.now();
            let open_park = parked.map_or(Duration::ZERO, |park| now.saturating_sub(park.since));
            let host_stalled = Duration::from_nanos(signals.stalled.get());
            let until = Deadline::after(started_at, allowance.saturating_add(parked_for).saturating_add(open_park).saturating_add(host_stalled));
            if let Some(code) = self.codes {
                let pumping = run.now();
                code.pump()?;
                let pumped = run.now().saturating_sub(pumping);
                if pumped >= DRAIN_EVERY {
                    signals.begin(Doing::Serving);
                    signals.add_host_nanos(pumped);
                }
                // The runner's watch moves its answer's due by it (test_run.rs `answer_due`).
                signals.parked.set(u64::try_from(parked_for.saturating_add(open_park).as_nanos()).unwrap_or(u64::MAX));
                signals.parked_now.set(parked.is_some());
            }
            if let (Some(park), Some(code)) = (parked, self.codes) {
                let came = code.answer(child, |reply| match reply {
                    Reply::Made(entry) => match lazy.made(entry) {
                        Ok(()) => Came::Made,
                        Err(cause) => Came::Failed(cause),
                    },
                    // Every asker of the key is handed the same refusal (rule 19).
                    Reply::Refused(why) => Came::Failed(lazy.refused(why.for_asker())),
                    Reply::Late { waited, .. } => Came::Late { waited },
                })?;
                if let Some(came) = came {
                    parked_for = parked_for.saturating_add(open_park);
                    parked = None;
                    signals.parked.set(u64::try_from(parked_for.as_nanos()).unwrap_or(u64::MAX));
                    signals.parked_now.set(false);
                    let stop = match came {
                        Came::Made => None,
                        Came::Failed(cause) => Some(test_parent_error(&mut lazy, cause)),
                        Came::Late { waited } => Some(match lazy.withdraw() {
                            Ok(()) => OneError::CodeLate { instance: lazy.name_of(park.symbol), symbol: park.symbol, waited },
                            Err(cause) => OneError::Parent(cause),
                        }),
                    };
                    if let Some(error) = stop {
                        // Its process waits for code it will never receive.
                        run.kill().map_err(child_error)?;
                        return Err(error);
                    }
                    again = true;
                }
            }
            if until.passed(now) {
                let killed = run.kill().map_err(child_error)?;
                break (How::TimedOut { after: budget }, killed.left);
            }
            match relay.turn(now, signals.terminate(now)?) {
                RelayAct::Forward => run.terminate().map_err(child_error)?,
                RelayAct::Kill => {
                    // A kill reaps: its end is always known.
                    let killed = run.kill().map_err(child_error)?;
                    break (child_outcome(killed.exit), killed.left);
                }
                RelayAct::Nothing => {}
            }
            let bound = until.min(relay.bound());
            drain_pipes(cx, &mut pipes, output, stderr, Deadline::PASSED)?;
            if open && !reply.is_empty() {
                let by = Deadline::after(run.now(), WRITE_BOUND).min(bound);
                match run.send(&reply, by) {
                    Ok(()) => published = calls.len(),
                    Err(ProcError { fault: RunFault::Channel(ChannelError::Hangup), .. }) => open = false,
                    Err(error) => return Err(child_error(error)),
                }
                reply.clear();
            }
            let poll = Deadline::after(run.now(), signals.wait.replace(Duration::ZERO)).min(bound);
            // Parked, nothing is read or served: its frames wait in the channel, in order.
            if open && parked.is_none() {
                let got = if core::mem::take(&mut again) {
                    // Its code came: the frames kept from the waiting one on, served again.
                    Some(inbox.len())
                } else {
                    match run.recv_message(&mut inbox, blaze_launch::wire::LONGEST_FRAME, poll) {
                        Ok(Message::Whole { len }) => Some(len),
                        Ok(Message::Closed) => {
                            open = false;
                            None
                        }
                        Err(error @ ProcError { fault: RunFault::Channel(ChannelError::Torn { .. }), .. }) => {
                            torn = Some(error);
                            open = false;
                            None
                        }
                        Ok(Message::Deadline) => None,
                        Err(error) => return Err(child_error(error)),
                    }
                };
                if let Some(len) = got {
                        self.watch.see(blaze_screen::Step::Waits);
                        signals.begin(Doing::Serving);
                        let serving = run.now();
                        let before = lazy.tally();
                        let mut resolver = TestResolve { parent: &mut lazy, requested: None };
                        let taken = match needs.serve(&mut resolver, &inbox[..len], &mut reply) {
                            Ok(taken) => taken,
                            Err(cause) => {
                                signals.add_host_nanos(run.now().saturating_sub(serving));
                                // This group is waiting for code it will never receive.
                                // Kill and reap before returning its typed outcome to the plan.
                                run.kill().map_err(child_error)?;
                                return Err(test_parent_error(&mut lazy, cause));
                            }
                        };
                        let elapsed = run.now().saturating_sub(serving);
                        service_time += elapsed;
                        signals.add_host_nanos(elapsed);
                        match resolver.requested {
                            Some(TestAnswer::Unresolved(Symbol(symbol))) => {
                                // Sending Failed would turn the unavailable instance into a launcher exit.
                                run.kill().map_err(child_error)?;
                                return Err(OneError::CannotBuild(InstanceRefusal {
                                    instance: lazy.name_of(symbol), symbol, site: None, cause: RefusalCause::Unresolved,
                                }));
                            }
                            Some(TestAnswer::Ready { symbol, record }) => {
                                let after = lazy.tally();
                                let unkeyed = after.unkeyed - before.unkeyed;
                                if keep.is_ok() {
                                    if unkeyed > 0 {
                                        keep = Err(NotKept::Untraced);
                                    } else {
                                        rec_bytes.clear();
                                        match lazy.read_record(&record, &mut rec_bytes) {
                                            Ok(()) => {
                                                if let Err(why) = crate::test_cache::check_record_purity(rec_bytes.as_slice()) {
                                                    keep = Err(why);
                                                }
                                            }
                                            Err(_) => keep = Err(NotKept::Untraced),
                                        }
                                    }
                                    if keep.is_ok() {
                                        let cache_alloc = self.cache_alloc;
                                        lazy.walked_entered(&symbol, |sym, tabled, name, key| {
                                            if key.is_none() {
                                                keep = Err(NotKept::Untraced);
                                                return;
                                            }
                                            match blaze_ladder::body_ids::instance_key(
                                                blaze_code::record::Symbol(sym.0),
                                                tabled,
                                                name,
                                                self.ids,
                                                &self.builder,
                                                &self.builder.producer,
                                                cache_alloc,
                                            ) {
                                                Some(inst_key) => {
                                                    let mut name_copy = Vec::with_capacity_in(name.len(), cache_alloc);
                                                    name_copy.extend_from_slice(name);
                                                    entered_fns.push(crate::test_cache::EnteredFn {
                                                        symbol: FnName(sym.0),
                                                        code_key: inst_key.0.0,
                                                        tabled,
                                                        name: name_copy,
                                                    });
                                                }
                                                None => keep = Err(NotKept::Untraced),
                                            }
                                        });
                                    }
                                }
                                calls.push(FirstCall {
                                    symbol, elapsed, made: after.made - before.made,
                                    found: after.found - before.found, unkeyed,
                                });
                            }
                            // Its row waits for its code, made by a worker (D194): the run parks.
                            Some(TestAnswer::Waiting(Symbol(symbol))) => {
                                let asked = match self.codes {
                                    Some(code) => self.ask_code(code, child, &lazy),
                                    // `Serial` makes inside the step and never waits.
                                    None => Err(OneError::Parent(ParentError::NotReady { symbol })),
                                };
                                if let Err(error) = asked {
                                    run.kill().map_err(child_error)?;
                                    return Err(error);
                                }
                                parked = Some(Parked { since: run.now(), symbol });
                                signals.parked_now.set(true);
                            }
                            // Faults and requests for an already answered slot need no resolution.
                            None => {}
                        }
                        self.watch.see(blaze_screen::Step::Answers);
                        // The served prefix out, the rest kept in order, as run_loop.rs does
                        // (`drain` wants the nightly-only allocator trait of the vec's allocator).
                        let left = inbox.len() - taken;
                        inbox.copy_within(taken.., 0);
                        inbox.truncate(left);
                        continue;
                }
            }
            // Parked, the turn's wait is the sweep's interval (as a read's would be).
            let look = if open && parked.is_none() { Deadline::PASSED } else { poll };
            run = match run.wait(look).map_err(child_error)? {
                Waited::Ended(reaped) => break (child_outcome(reaped.exit), reaped.left),
                Waited::Running(running) => running,
            };
        };
        // The reaped child's remaining output belongs to its own capture.
        let drain_until = Deadline::after(over.now(), DRAIN_AFTER);
        while pipes.iter().any(Option::is_some) && !drain_until.passed(over.now()) {
            let poll = Deadline::after(over.now(), signals.wait.replace(Duration::ZERO)).min(drain_until);
            drain_pipes(cx, &mut pipes, output, stderr, poll)?;
            if pipes.iter().any(Option::is_some) {
                next_turn().await;
            }
        }
        if let How::Signal { number } = ended {
            let path = self.tests.row(row as usize).map_err(OneError::Record)?.ok_or(OneError::NoRow { row })?.path;
            let err = self.err;
            let diagnostic = OneError::Diagnostic;
            err.line(cx, format_args!("---- {}/{}/{path} signal {number}: stderr ({} bytes) ----", self.package, self.target, stderr.bytes)).map_err(diagnostic)?;
            let mut first = [0u8; 4000];
            let wanted = stderr.bytes.min(first.len() as u64) as usize;
            let held = wanted.min(stderr.prefix.len());
            first[..held].copy_from_slice(&stderr.prefix[..held]);
            if held < wanted {
                // The spill starts immediately after the in-memory prefix, even when
                // the first pipe read was too large to keep any prefix.
                let file = stderr.dir.open_read(cx, &stderr.path).map_err(OneError::Capture)?;
                let mut read = held;
                while read < wanted {
                    let n = file.read_into(&mut first[read..wanted], (read - held) as u64).wait(cx).map_err(OneError::Capture)?;
                    if n == 0 {
                        return Err(OneError::ShortCapture { read: read as u64, written: stderr.bytes });
                    }
                    read += n;
                }
            }
            let mut line = [0u8; 101];
            let mut width = 0;
            for &byte in &first[..wanted] {
                let escaped = core::ascii::escape_default(byte);
                if width + escaped.len() > 100 {
                    line[width] = b'\n';
                    err.write_all(cx, &line[..=width]).map_err(diagnostic)?;
                    width = 0;
                }
                for ascii in escaped {
                    line[width] = ascii;
                    width += 1;
                }
            }
            if width != 0 {
                line[width] = b'\n';
                err.write_all(cx, &line[..=width]).map_err(diagnostic)?;
            }
            err.line(cx, "---- end escaped stderr ----").map_err(diagnostic)?;
            if pipes[1].is_some() {
                err.line(cx, "\nstderr was still open at the post-exit drain deadline").map_err(diagnostic)?;
            }
            err.line(cx, format_args!("\nfirst calls: {}; parent serving requests: {service_time:?}; {}", calls.len(), lazy.tally())).map_err(diagnostic)?;
            match calls[..published].last() {
                Some(call) => err.line(cx, format_args!("last published: {}", InstanceName(&lazy.name_of(call.symbol.0), call.symbol.0))),
                None => err.line(cx, "last published: none"),
            }.map_err(diagnostic)?;
            for call in &calls[published..] {
                err.line(cx, format_args!("pending need: {}", InstanceName(&lazy.name_of(call.symbol.0), call.symbol.0))).map_err(diagnostic)?;
            }
            // A last request or fault may have reached fd 3 between the final poll and
            // waitpid. Read already-queued messages without building anything for a dead child.
            let mut fault = needs.fault();
            let mut queued = false;
            let wire_error = |why| OneError::Parent(ParentError::Wire(why));
            let read_until = Deadline::after(over.now(), DRAIN_AFTER);
            while torn.is_none() && !read_until.passed(over.now()) {
                match over.recv_message(&mut inbox, blaze_launch::wire::LONGEST_FRAME, Deadline::PASSED) {
                    Ok(Message::Whole { len }) => {
                        let (_, up) = blaze_launch::wire::run::Up::decode(&inbox[4..len]).map_err(wire_error)?;
                        match up {
                            blaze_launch::wire::run::Up::Need { need, .. } => {
                                queued = true;
                                err.line(cx, format_args!("pending need (received after exit): {need:?}")).map_err(diagnostic)?;
                            }
                            blaze_launch::wire::run::Up::Fault { fault: last } => {
                                if fault.is_none() {
                                    fault = Some(last);
                                }
                            }
                        }
                        let left = inbox.len() - len;
                        inbox.copy_within(len.., 0);
                        inbox.truncate(left);
                    }
                    Ok(Message::Closed | Message::Deadline) => break,
                    Err(error @ ProcError { fault: RunFault::Channel(ChannelError::Torn { .. }), .. }) => torn = Some(error),
                    Err(error) => return Err(child_error(error)),
                }
            }
            if !queued && calls.len() == published && inbox.is_empty() {
                err.line(cx, "pending need: none").map_err(diagnostic)?;
            }
            if let Some(error) = torn {
                err.line(cx, format_args!("incomplete first-call channel message: {error}")).map_err(diagnostic)?;
            } else if !inbox.is_empty() {
                err.line(cx, format_args!("unprocessed first-call channel bytes at exit: {}", inbox.len())).map_err(diagnostic)?;
            }
            err.line(cx, "refusal stub: no refused first call observed by the parent").map_err(diagnostic)?;
            if let Some(fault) = fault {
                let mut bytes = bstd::mem::Bytes::new_in(frame, "the code record a test struck in");
                let record = match fault.at {
                    Some(spot) if spot.placed == blaze_launch::wire::fault::Placed::Code => Some(lazy.read_record(&spot.record, &mut bytes).map(|()| bytes.as_slice())),
                    _ => None,
                };
                err.line(cx, format_args!("{}", crate::run_fault::report(fault, &mut lazy, record))).map_err(diagnostic)?;
            } else {
                err.line(cx, format_args!("the launcher sent no fault location for signal {number}")).map_err(diagnostic)?;
            }
            for call in &calls[..published] {
                err.line(cx, format_args!("published: {} in {:?} (records made {}, found {}, unkeyed {})",
                    InstanceName(&lazy.name_of(call.symbol.0), call.symbol.0), call.elapsed, call.made, call.found, call.unkeyed)).map_err(diagnostic)?;
            }
            err.line(cx, "---- end signal report ----").map_err(diagnostic)?;
            err.flush(cx).map_err(diagnostic)?;
        } else if let Some(error) = torn {
            return Err(child_error(error));
        }
        if entered_fns.is_empty() && keep.is_ok() {
            keep = Err(NotKept::Untraced);
        }
        entered_fns.sort_unstable_by_key(|e| e.symbol);
        entered_fns.dedup_by_key(|e| e.symbol);
        let mut entered = Vec::with_capacity_in(entered_fns.len(), self.tables);
        for e in entered_fns.iter() {
            entered.push(e.symbol);
        }
        if !entered_fns.is_empty() {
            let mut sink = self.entered_sink.borrow_mut();
            for e in entered_fns {
                sink.push((self.suite, e));
            }
        }
        Ok(RunOutcome { how: ended, keep, entered })
    }
}

fn child_outcome(exit: Exit) -> How {
    match exit {
        Exit::Exited(0) => How::Reported(Said::Returned),
        Exit::Exited(code) => How::NoReport { code, torn: 0 },
        Exit::Signaled(number) => How::Signal { number },
    }
}

/// At most a pipeful from each stream per turn, so a writer that never stops cannot starve
/// another child's first-call channel or deadline.
fn drain_pipes(cx: &Cx, pipes: &mut [Option<OutPipe>; 2], output: &mut Output<'_>, stderr: &mut StderrCapture<'_>, by: Deadline) -> Result<(), OneError> {
    let mut chunk = [0u8; 16 * 1024];
    for (stream, slot) in pipes.iter_mut().enumerate() {
        for _ in 0..4 {
            let Some(pipe) = slot.as_ref() else { break };
            match pipe.read(&mut chunk, by).map_err(OneError::Pipe)? {
                Piped::Bytes(n) => {
                    output.push(&chunk[..n]);
                    if stream == 1 {
                        stderr.push(cx, &chunk[..n])?;
                    }
                }
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
    /// The Rust fns the test binary's C calls (its image's words, `NativeSend::items`): roots.
    pub exports: &'s [LibKey],
    pub watch: &'s dyn Watch,
    /// The command's make watch ([`Launched::make_watch`]), handed to each maker made.
    pub make_watch: blaze_ladder::run::make_one::MakeWatch<'static>,
}

impl<'s, 'h, 'r> Front for TestFront<'s, 'h, 'r> {
    type Start = Roots<'r>;
    type Ids = &'s RunIds<'h>;
    type Maker = TestMaker<'h>;

    fn start(&mut self) -> Result<Roots<'r>, ParentError> {
        entry_roots(self.harness, self.names, self.test, self.exports, self.builder, self.tables, self.alloc).map_err(ParentError::Roots)
    }

    fn ids(&mut self) -> Result<&'s RunIds<'h>, ParentError> {
        Ok(self.ids)
    }

    fn maker(&mut self, start: &Roots<'r>, _ids: &&'s RunIds<'h>) -> Result<TestMaker<'h>, ParentError> {
        // The test's roots start its program at the test fn (`MakerStart::Direct`), not at `main`.
        // The typing is the test's ([`TestMaker`]: the workers' too).
        let mut maker = Maker::of_roots(self.names, start.maker_entry(), start, SecondCheck, &self.builder, self.tables);
        maker.watch(self.make_watch);
        Ok(maker)
    }

    /// As run_parent.rs `CommandFront::path_of`: the one printer over the names records.
    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        use blaze_ladder::front2::source::NamesSource;
        let lib = self.names.records.key_beginning(item.0.to_be_bytes())?;
        blaze_ladder::front2::item_shown::ItemShown::of_lib(self.names.records, lib).path().map(Box::from)
    }

    fn named(&mut self, maker: Option<&TestMaker<'h>>, tabled: ItemKey, name: &[u8]) -> crate::run_fault::Named {
        use crate::run_fault::Named;
        if let Some(shown) = maker.and_then(|maker| maker.instance_shown(tabled, name)) {
            return Named::Shown { item: shown.item, args: shown.args };
        }
        match self.path_of(tabled) {
            Some(path) => Named::Path(path),
            None => Named::Item(tabled),
        }
    }

    fn site_shown(&mut self, maker: Option<&TestMaker<'h>>, item: ItemKey, site: u32) -> Option<crate::run_fault::SiteShown> {
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
    fn runnable(&self, item: ItemKey) -> Result<crate::run_ids::Runnable, crate::run_ids::NotRunnable> {
        RunIds::runnable(self, item)
    }
}

/// `name` beneath `dir`, made; one already there taken as it is (a folder is the state wanted);
/// anything else fails naming it.
/// Whether a file is at `launcher` (an absolute path, own_process.rs `launcher_beside`): its
/// folder is opened as a tree of its own (bstd `Fs::home` over the folder's parent, the folder's
/// name beneath it) and the file's name is stat'ed there; one stat per command.
fn launcher_there(cx: &Cx, fs: &bstd::fs::Fs, launcher: &str, frame: &Frame) -> Result<bool, OneError> {
    let not_looked = |why: &dyn fmt::Display| OneError::LauncherNotLooked { launcher: Box::from(launcher), why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) };
    let Some((folder, file)) = launcher.rsplit_once('/') else { return Err(not_looked(&"the path has no folder")) };
    let Some((parent, name)) = folder.rsplit_once('/') else { return Err(not_looked(&"the folder is not an absolute path")) };
    let parent = if parent.is_empty() { "/" } else { parent };
    let name = Path::new(name).map_err(OneError::Path)?;
    let file = Path::new(file).map_err(OneError::Path)?;
    let dir = fs.home(cx, parent, &name).map_err(|why| not_looked(&why))?;
    match dir.stat(cx, &file) {
        Ok(found) => Ok(found.is_some()),
        Err(why) => Err(not_looked(&why)),
    }
}

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
            let row = asked_with_id(record_row(record, at)?, TestId([0; 32]));
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
    Ok(Status { passed: 0, failed: 0, ignored: 0, not_run: 0, check_errors: 0 })
}

pub(crate) struct Suite<'s, 'h> {
    pub unit: TestUnit<'s>,
    pub names: NamesRecords<'h>,
    /// What its names records were made of (run_names.rs `RunCrates`): each code worker makes
    /// its own of them under `Codes::Pooled` ([`SuiteMakers`]).
    pub crates: crate::run_names::RunCrates<'h>,
    pub builder: Builder,
    /// Where its tests run and what of their package they are told ([`package_run`]).
    pub place: PackageRun<'s>,
    /// Its test program's native image (engine_native.rs `image_for`, engine_start.rs
    /// `run_tests`): `Ok(Some(..))` when the C of a crate it links is in it, told to every test's
    /// start before any of its code ([`Launched::native`]); `Err(why)`: the image could not be
    /// had, and each of its tests is NOT RUN with `why` ([`EntryCheck`]), never started without
    /// the C it may call (rule 3).
    pub native: Result<Option<crate::engine_native::NativeSend<'s>>, &'s str>,
}

/// Where a package's tests run and what of the package they are told at run time, as
/// `cargo test` starts one: its working directory is the package's root (the folder of its
/// `Cargo.toml`), `CARGO_MANIFEST_DIR` names that folder and `CARGO_MANIFEST_PATH` its manifest,
/// and every `CARGO_PKG_*` the package has is set, from the unit graph's package record (the
/// crate list's row: blaze-scope crate_env.rs `CrateEnv::of`, the one construction `env!` is
/// answered by, rule 30).
///
/// TWO VALUES OF ONE NAME, BY DESIGN: `env!("CARGO_MANIFEST_DIR")` compiled in is the root the
/// crate SEES (D96: `/workspace/<member dir>`, never the machine's path, so keys hold nothing
/// of it, rule 25); the run's `CARGO_MANIFEST_DIR` is the real folder, which only the run
/// reads, so no key holds it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PackageRun<'s> {
    /// The package's root, absolute: the tests' working directory.
    pub dir: &'s str,
    /// Each variable, sorted by name, each name once.
    pub vars: &'s [(&'s str, &'s str)],
}

/// Why a test unit's run place could not be made: each names the crate (rule 19).
#[derive(Debug)]
pub(crate) enum PackageRunError<'a> {
    /// The crate is no workspace member's (`SeenRoot::Package`): only members' tests run.
    NotMember { krate: &'a str },
    /// The checkout's root has no absolute path as text (a simulated world, or a path that is
    /// not UTF-8): no folder to run in.
    NoRoot { krate: &'a str },
    /// The package's environment could not be made.
    Env(blaze_scope::crate_env::CrateEnvError<'a>),
}

impl fmt::Display for PackageRunError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageRunError::NotMember { krate } => write!(f, "crate `{krate}` is no workspace member's: its tests have no package root in the checkout to run in (test_drive.rs `package_run`)"),
            PackageRunError::NoRoot { krate } => write!(f, "crate `{krate}`: the checkout's root has no absolute path as text, so its tests have no folder to run in (test_drive.rs `package_run`)"),
            PackageRunError::Env(why) => write!(f, "{why} (test_drive.rs `package_run`)"),
        }
    }
}

/// The run place of the crate whose environment's facts are `facts` (the crate list's
/// `env_facts` of its row) in the checkout whose absolute root is `checkout`, in `bump`
/// ([`PackageRun`]). Sans-IO: the root is read at the edge (engine_check.rs `Served::root_text`).
pub(crate) fn package_run<'a>(checkout: Option<&str>, facts: blaze_scope::crate_env::EnvFacts<'a>, bump: blaze_own::Bump<'a>) -> Result<PackageRun<'a>, PackageRunError<'a>> {
    let krate = facts.crate_name;
    let blaze_scope::crate_env::SeenRoot::Member { dir: member } = facts.seen else { return Err(PackageRunError::NotMember { krate }) };
    let Some(root) = checkout else { return Err(PackageRunError::NoRoot { krate }) };
    let mut dir: String<blaze_own::Bump<'a>> = String::from_str_in(root.trim_end_matches('/'), bump);
    if !member.is_empty() {
        dir.push_str("/");
        dir.push_str(member);
    }
    let mut manifest: String<blaze_own::Bump<'a>> = String::from_str_in(dir.as_str(), bump);
    manifest.push_str("/Cargo.toml");
    let env = blaze_scope::crate_env::CrateEnv::of(facts, bump.alloc()).map_err(PackageRunError::Env)?;
    let dir: &'a str = dir.into_bump_str();
    let mut vars: Vec<(&'a str, &'a str), blaze_own::Bump<'a>> = Vec::new_in(bump);
    vars.push(("CARGO_MANIFEST_DIR", dir));
    vars.push(("CARGO_MANIFEST_PATH", manifest.into_bump_str()));
    // The package's own facts; a build script's `rustc-env` pairs are the compile's alone.
    for (name, value) in env.vars().filter(|(name, _)| name.starts_with("CARGO_PKG_")) {
        vars.push((bump.alloc_str(name), bump.alloc_str(value)));
    }
    vars.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let vars: &'a [(&'a str, &'a str)] = vars.into_bump_slice();
    Ok(PackageRun { dir, vars })
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

impl RunOne for Selected<'_, Launched<'_, '_, '_>> {
    async fn run(&self, flat: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<RunOutcome, OneError> {
        let loc = self.rows.get(flat as usize).ok_or(OneError::NoRow { row: flat })?;
        self.suites[loc.suite.0 as usize].run(loc.row, budget, output, signals).await
    }
}

/// The selected suites run, their code made where [`CODES`] says (D194, THE seam): `Serial`, in
/// each test's own turn; `Pooled`, by the command's code workers ([`pooled`]), the relay handed
/// to every test's run. `root` is the checkout's (each worker's names records read its trees
/// through it); `procs` is the command's proc-macro runner (by need: a worker asks for it,
/// [`CodePool`]); `heap` is the command's, the relay's and the workers' tables in it.
#[expect(clippy::too_many_arguments, reason = "the command supplies the selected records and specific runtime capabilities")]
pub(crate) fn run_suites<'h>(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &bstd::out::Out,
    err: &bstd::out::Out,
    suites: &[Suite<'_, 'h>], ids: &RunIds<'h>, config: LoopConfig<'_>,
    args: &[&str], lent: &CommandLent, root: &Dir<ReadWrite>, procs: crate::job_texts::Procs<'_>, heap: &'static blaze_own::Heap, watch: &dyn Watch, frame: &Frame,
) -> Result<Status, DriveError> {
    let tables = heap.alloc();
    // The command's one make watch (rule 75: its clock started here, at the edge), handed to
    // every maker, the serial turns' and each code worker's (rule 16: a large or slow make's line).
    let make_watch = crate::run_parent::command_make_watch();
    match CODES {
        Codes::Serial => run_selected(cx, fs, env, host, clock, out, err, suites, ids, config, args, lent, tables, watch, make_watch, frame, None),
        Codes::Pooled => pooled(cx, clock, rng, suites, root, procs, heap, make_watch, |relay| run_selected(cx, fs, env, host, clock, out, err, suites, ids, config, args, lent, tables, watch, make_watch, frame, Some(relay))),
    }
}

/// [`run_suites`] under `Codes::Pooled` (D194 piece 2): the command's code workers started (one
/// a worker of the pool beside the command thread, as many as are idle now: the pool's size is
/// the machine's, never waited for; none is a refusal by name, never a serial run in its place,
/// rule 3), the relay made over them, `run` handed it; then every lane closed and each worker
/// joined within [`END_WAIT`] (a make in flight ends, its entry dropped: its asker is gone).
///
/// What is shared and what is not (the doc of run_names.rs `RunCrates`): each worker reads the
/// suites' crates in place and makes its own names records, trees, environments and makers; the
/// proc-macro runner, made by need on this thread, is relayed ([`CodePool::finished`]).
#[expect(clippy::too_many_arguments, reason = "the command supplies the suites, the workers' inputs and the run")]
fn pooled<'h>(
    cx: &Cx, clock: &bstd::clock::Clock, rng: &mut bstd::rng::Rng, suites: &[Suite<'_, 'h>], root: &Dir<ReadWrite>, procs: crate::job_texts::Procs<'_>, heap: &'static blaze_own::Heap,
    make_watch: blaze_ladder::run::make_one::MakeWatch<'static>,
    run: impl FnOnce(&CodeRelay<'_>) -> Result<Status, DriveError>,
) -> Result<Status, DriveError> {
    let cx = cx.span("blaze test: the command's code workers");
    let hash_seed = rng.next_u64();
    let have = cx.workers();
    let Some(most) = core::num::NonZeroUsize::new(usize::try_from(have.saturating_sub(1)).unwrap_or(0)) else {
        return Err(DriveError::NoCodeWorkers { have });
    };
    let lanes = Lanes::new(most, heap);
    let mut sides: AVec<'static, SuiteSide<'_>> = Vec::with_capacity_in(suites.len(), heap.alloc());
    sides.extend(suites.iter().map(|suite| SuiteSide { crates: suite.crates, builder: suite.builder, make_watch }));
    let sides = sides.as_slice();
    let side = ProcsSide::of(procs);
    let children = suites.iter().map(|suite| suite.unit.tests.len()).sum::<usize>();
    let now = || Nanos(u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX));
    // The scope is linear: nothing returns between it and its join.
    let scope = cx.scope();
    let mut tasks = Vec::with_capacity_in(most.get(), heap.alloc());
    for at in 0..most.get() {
        let lane = lanes.lane(at);
        let clock = clock.clone();
        match scope.must_overlap("blaze test: a code worker", WORKER_SCRATCH, move |cx: &Cx| work(cx, lane, &clock, sides, root, side, heap)) {
            Ok(task) => tasks.push(task),
            // No worker idle now: the workers are the ones the pool gave at the start.
            Err(_) => break,
        }
    }
    let ran = match core::num::NonZeroUsize::new(tasks.len()) {
        None => Err(DriveError::NoCodeWorkers { have }),
        Some(count) => {
            let pool = CodePool { lanes: &lanes, count: count.get(), cx: &cx, clock, procs };
            match CodeRelay::new(children, &pool, &now, heap.shared(), hash_seed) {
                Ok(relay) => run(&relay),
                Err(error) => Err(DriveError::Relay(error)),
            }
        }
    };
    // The end: every lane closed, so a worker waiting for its next make ends; one making ends
    // after its make, its entry refused by the closed channel and dropped (its asker is gone).
    lanes.close();
    let ending = cx.until(Deadline::after(clock.now().since_start(), END_WAIT));
    let mut ended = Ok(());
    for task in tasks {
        if let Err(error) = task.join(&ending)
            && ended.is_ok()
        {
            ended = Err(error);
        }
    }
    let joined = scope.join(&ending);
    let _dropped = lanes.drain();
    let status = ran?;
    ended.map_err(DriveError::CodeWorkers)?;
    joined.map_err(DriveError::CodeWorkers)?;
    Ok(status)
}

#[expect(clippy::too_many_arguments, reason = "the command supplies the selected records and specific runtime capabilities")]
fn run_selected<'h>(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    out: &bstd::out::Out,
    err: &bstd::out::Out,
    suites: &[Suite<'_, 'h>], ids: &RunIds<'h>, config: LoopConfig<'_>,
    args: &[&str], lent: &CommandLent, tables: Alloc<'static>, watch: &dyn Watch, make_watch: blaze_ladder::run::make_one::MakeWatch<'static>, frame: &Frame,
    relay: Option<&CodeRelay<'_>>,
) -> Result<Status, DriveError> {
    let cx = cx.span("blaze test: selected suites");
    let env = env.ok_or(DriveError::Missing(bstd::io::Missing { capability: "env" }))?;
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
    // Looked at once, before any test starts: a missing launcher under `sandbox-exec` is the
    // sandbox's own exit 71 for every test, which names no cause (rule 19; seen 2026-11-21 in a
    // cargo target folder holding `blaze` and no `blaze-launcher`, rounds/wave-1119/t82). A host
    // with no test sandbox starts no test (each is NOT RUN: test_run.rs `Host::has_sandbox`).
    if crate::test_run::Host::THIS.has_sandbox() && !launcher_there(&cx, &fs, launcher.as_str(), frame).map_err(one)? {
        return Err(one(OneError::NoLauncher { launcher: Box::from(launcher.as_str()), exe: Box::from(&*exe) }));
    }
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
    // THE seam of D194 ([`CODES`]): `None` makes each test's code in its own turn; `Some`, the
    // command's relay over its code workers ([`pooled`]), which lives in the caller's frame and
    // outlives every run borrowing it (`Launched::codes`; E0597 when it was owned here, a1850).
    let mut child_base = 0usize;
    let entered_sink: RefCell<AVec<'_, (SuiteId, crate::test_cache::EnteredFn<'_>)>> = RefCell::new(Vec::new_in(alloc));
    let mut suite_specs = Vec::with_capacity_in(suites.len(), alloc);
    for suite in suites {
        suite_specs.push((suite.unit, suite.builder));
    }
    let (mut cache, heads, postings_list, moved) = crate::test_cache::TestCache::open(lent, suite_specs.as_slice(), ids, alloc);
    let merged_postings_bytes: AVec<'_, u8>;
    let postings = if postings_list.len() == 1 {
        postings_list[0]
    } else if postings_list.is_empty() {
        Postings::EMPTY
    } else {
        let mut pairs: AVec<'_, (Ask, TestId)> = Vec::new_in(alloc);
        for head in heads.iter() {
            for row in head.rows() {
                for ask in head.asks(row) {
                    pairs.push((ask, row.kept.test));
                }
            }
        }
        if pairs.is_empty() {
            Postings::EMPTY
        } else {
            merged_postings_bytes = blaze_run::postings::encode(pairs.as_slice(), alloc)
                .map_err(|error| LoopError::Postings { suite: SuiteId(u32::MAX), error })?;
            Postings::open(merged_postings_bytes.as_slice())
                .map_err(|error| LoopError::Postings { suite: SuiteId(u32::MAX), error })?
        }
    };
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
            err: &err, package: unit.package, target: unit.target,
            cx: &cx, spawner: &spawner, launcher: launcher.as_str(), records: &records, store: store.as_str(),
            temps: dir, temp: temp.as_str(), env: vars.as_slice(), place: suite.place, tests, names: suite.names, ids,
            builder: suite.builder, lent, tables, watch, make_watch,
            args: if tests.harnessed() { &[] } else { args },
            native: suite.native.ok().flatten(),
            codes: relay,
            suite: suite_id,
            child_base,
            entered_sink: &entered_sink,
            cache_alloc: alloc,
        });
        child_base += tests.len();
        checks.push(EntryCheck { tests, names: suite.names, builder: suite.builder, tables, image: suite.native.map(|native| native.map_or(&[][..], |send| send.items)), said: String::new_in(alloc) });
        for at in 0..tests.len() {
            let mut row = asked(unit, record_row(tests, at)?, &suite.builder, args);
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
    cache.set_owners(rows.as_slice(), owners.as_slice());
    let now = || Nanos(u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX));
    let launched = Selected { suites: launches.as_mut_slice(), rows: &locations };
    let mut refuse = Selected { suites: checks.as_mut_slice(), rows: &locations };
    let signals = Signals::default();
    let zygotes = OneEach::new(&launched, &signals, now, alloc, tables)?;
    let mut runner = Runner::new(zygotes, &config.plan, TestHost::THIS, alloc);
    let mut print = |bytes: &[u8]| -> Result<(), DriveError> {
        watch.see(blaze_screen::Step::Says);
        for chunk in bytes.utf8_chunks() {
            out.write(&cx, chunk.valid()).map_err(|_| DriveError::Print)?;
            if !chunk.invalid().is_empty() {
                out.write(&cx, "\u{FFFD}").map_err(|_| DriveError::Print)?;
            }
        }
        Ok(())
    };
    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Testing));
    let status = drive(
        config,
        rows,
        &owners,
        heads.as_slice(),
        postings,
        moved.as_slice(),
        filtered,
        &mut runner,
        &mut refuse,
        &mut cache,
        &entered_sink,
        &mut print,
        alloc,
    );
    watch.see(blaze_screen::Step::Says);
    // Clean up this command's per-run `$TMPDIR` tree (`v2/test/tmp/<pid>-<ns>`) on both Ok and Err
    // paths now that all child test processes have exited.
    let _ = crate::gc::remove_tree_best_effort(&cx, &temps, &command_path);
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
    // A zygote "silent" past a run's allowance: what held the command thread, which serves every
    // child, said before the runner's own line (stop 3 of the first real run: tempo's
    // `tempo-precompiles-macros` row 7, five runs in a row). The longest turn names its test,
    // its length and what it was doing; it may be another test's turn than the silent row's.
    if let (Err(DriveError::Run(RunError::ZygoteSilent { .. })), Some(turn)) = (&status, runner.zygotes().longest) {
        let location = locations.get(turn.row as usize).ok_or(one(OneError::NoRow { row: turn.row }))?;
        let unit = suites[location.suite.0 as usize].unit;
        let path = record_row(unit.tests, location.row as usize)?.path;
        err.line(&cx, format_args!(
            "blaze test: the command thread, which serves every test's process, was held {}.{} s in one turn of `{}/{}/{path}` (row {}): {}; while a turn runs no test is answered (test_drive.rs `OneEach`)",
            turn.nanos / 1_000_000_000, turn.nanos % 1_000_000_000 / 100_000_000, unit.package, unit.target, turn.row, turn.doing,
        )).map_err(|_| DriveError::Print)?;
    }
    let status = status?;
    cache.commit()?;
    Ok(status)
}

#[cfg(test)]
mod package_run_tests {
    use super::*;
    use blaze_scope::crate_env::{CrateEnv, EnvFacts, SeenRoot};

    fn facts(seen: SeenRoot<'static>) -> EnvFacts<'static> {
        EnvFacts {
            crate_name: "tempo_hardfork",
            package: Some(("tempo-hardfork", "1.16.0")),
            bin: None,
            seen,
            read_folder: "/0/crates/hardfork/",
            build_script: false,
            script: None,
            test_or_bench: false,
            bin_exes: blaze_scope::crate_env::BinNames::NONE,
            meta: None,
        }
    }

    fn get<'a>(run: &PackageRun<'a>, name: &str) -> Option<&'a str> {
        run.vars.iter().find(|(set, _)| *set == name).map(|(_, value)| *value)
    }

    /// A member's tests run in its package's root under the checkout, told its package's
    /// variables: `CARGO_MANIFEST_DIR` is that real folder, `CARGO_MANIFEST_PATH` its manifest,
    /// `CARGO_PKG_*` the package's, sorted, each once. The workspace's root package runs in the
    /// checkout's root. PLANTED (the variable as compiled, D96): the compile-time
    /// `CARGO_MANIFEST_DIR` of the same facts is the seen root, never the machine's folder, so the
    /// run's value is not the compile's. A dependency's crate and a checkout with no root as text
    /// are refused by name.
    #[test]
    fn a_members_tests_run_in_its_package_root_told_its_package() {
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let member = SeenRoot::Member { dir: "crates/hardfork" };
        let run = package_run(Some("/w/tempo/"), facts(member), bump).unwrap_or_else(|why| panic!("{why}"));
        assert_eq!(run.dir, "/w/tempo/crates/hardfork", "the package's root under the checkout");
        assert_eq!(get(&run, "CARGO_MANIFEST_DIR"), Some("/w/tempo/crates/hardfork"));
        assert_eq!(get(&run, "CARGO_MANIFEST_PATH"), Some("/w/tempo/crates/hardfork/Cargo.toml"));
        assert_eq!(get(&run, "CARGO_PKG_NAME"), Some("tempo-hardfork"));
        assert_eq!(get(&run, "CARGO_PKG_VERSION"), Some("1.16.0"));
        assert_eq!((get(&run, "CARGO_PKG_VERSION_MAJOR"), get(&run, "CARGO_PKG_VERSION_MINOR"), get(&run, "CARGO_PKG_VERSION_PATCH")), (Some("1"), Some("16"), Some("0")));
        assert_eq!(get(&run, "CARGO_CRATE_NAME"), None, "only the package's facts and its folder: the crate's name is the compile's");
        assert!(run.vars.windows(2).all(|pair| pair[0].0 < pair[1].0), "sorted, each name once: {:?}", run.vars);

        // PLANTED: the compiled value of the same name is the seen root (D96), not this folder.
        let compiled = CrateEnv::of(facts(member), alloc).unwrap_or_else(|why| panic!("{why}"));
        let seen = compiled.get("CARGO_MANIFEST_DIR").unwrap_or_else(|_| panic!("known")).unwrap_or_else(|| panic!("set"));
        assert_ne!(Some(seen), get(&run, "CARGO_MANIFEST_DIR"), "planted: the compile's value would send a test's reads to {seen}");

        let root = package_run(Some("/w/one"), facts(SeenRoot::Member { dir: "" }), bump).unwrap_or_else(|why| panic!("{why}"));
        assert_eq!(root.dir, "/w/one", "the workspace's root package runs in the checkout's root");
        assert!(matches!(package_run(Some("/w/one"), facts(SeenRoot::Package), bump), Err(PackageRunError::NotMember { krate: "tempo_hardfork" })));
        assert!(matches!(package_run(None, facts(member), bump), Err(PackageRunError::NoRoot { krate: "tempo_hardfork" })));
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::engine_native::NativeSend;

    const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn send(hex: &'static str) -> NativeSend<'static> {
        NativeSend { library: blaze_launch::wire::RecordId([7; 32]), hex, words: &[], items: &[] }
    }

    /// A test program whose image holds C gets one grant more than the test profile: its
    /// library, by literal path, mapped executable (what `dlopen` needs under `deny default`;
    /// t1545's `blst_p2_uncompress` was a test started without it). Without an image the profile
    /// is the test profile byte for byte; a path a profile string cannot hold is refused by name.
    #[test]
    fn the_native_library_is_the_one_file_mapped_executable() {
        let alloc = blaze_own::testing::heap().alloc();
        let plain = || blaze_run::child::seatbelt("/h/.blaze/v2/test/tmp/1/0/3", None, alloc).unwrap_or_else(|why| panic!("{why}"));
        let bare = plain();
        let kept = with_native(plain(), "/h/.blaze/v2/records", None).unwrap_or_else(|why| panic!("{why}"));
        assert_eq!(kept.as_str(), bare.as_str(), "no image: the test profile unchanged");

        let granted = with_native(plain(), "/h/.blaze/v2/records", Some(send(HEX))).unwrap_or_else(|why| panic!("{why}"));
        let line = "(allow file-map-executable (literal \"/h/.blaze/v2/records/00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\"))\n";
        assert_eq!(granted.as_str().strip_prefix(bare.as_str()), Some(line), "the test profile, then the one grant");
        assert_eq!(granted.as_str().matches("file-map-executable").count(), 1, "one file, nothing wider");

        // PLANTED: a store path with a quote would end the profile's string early.
        assert!(matches!(with_native(plain(), "/h/\"/records", Some(send(HEX))), Err(OneError::Profile { .. })));
        assert!(matches!(with_native(plain(), "/h/records", Some(send("../x"))), Err(OneError::Profile { .. })));
    }
}
