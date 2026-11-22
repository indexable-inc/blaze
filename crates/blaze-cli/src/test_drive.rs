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
use bstd::procs::{ChannelError, Exit, GRACE, Held, Message, OutPipe, ParentDeath, ParentSignals, Read as Piped, Relay, RelayAct, RunChild, RunError as ProcError, RunFault, RunSpec, SignalError, Spawner, Started, Streams, Waited};

use crate::code_store::HomeCodeStore;
use crate::run_names::{Found, TestRows, TestUnit};
use crate::run_ids::RunIds;
use crate::run_loop::LoopError as RunLoopError;
use crate::run_parent::{self, Front, Gate, Lazy, Needs, ParentError, RECORDS, RecordsDir, Resolve, Resolved, RunMaker, Start as _, WRITE_BOUND};
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
        write!(f, "{}", InstanceName(&self.instance, self.symbol))?;
        match &self.site {
            Some(InstanceSite::Source(site)) => write!(f, " at {}:{}:{}", site.file, site.line, site.column)?,
            Some(InstanceSite::Item { item, byte }) => write!(f, " at byte {byte} of item {:#x}", item.0)?,
            None => {}
        }
        let RefusalCause::Build(cause) = &self.cause else {
            return f.write_str(": the first-call stub requested this symbol, but the program has no callable record for it");
        };
        match cause {
            ParentError::Walk {
                why: blaze_ladder::body_ids::WalkError::Make { why: blaze_ladder::run::RunError::Build { error, .. }, .. }, ..
            } => write!(f, ": {error}"),
            ParentError::Walk { why: blaze_ladder::body_ids::WalkError::Make { why, .. }, .. } => write!(f, ": {why}"),
            ParentError::Special { why, .. } => write!(f, ": {why}"),
            cause => write!(f, ": {cause}"),
        }
    }
}

/// Only a refusal of the requested instance belongs to its test. A broken shared record, a
/// wire error, or an invariant of the parent stops the command; a names-record read that did
/// not answer while one instance was made (`LazyRefusal::RecordsVoid`) is that test's. Keep the walk alive until the instance and item-relative site have been named.
fn test_parent_error<F: Front, D: run_parent::Folder>(lazy: &mut Lazy<'_, '_, F, D>, cause: ParentError) -> OneError {
    let Some((symbol, location)) = refused_instance(&cause) else { return OneError::Parent(cause) };
    let site = location.map(|(item, byte)| match lazy.site_of(item, byte) {
        Some(shown) => InstanceSite::Source(shown),
        None => InstanceSite::Item { item, byte },
    });
    OneError::CannotBuild(InstanceRefusal { instance: lazy.name_of(symbol), symbol, site, cause: RefusalCause::Build(cause) })
}

/// The test whose instance could not be made, and where (`refused_at`), or `None` for a fault
/// that is the suite's own. INVERTED (wave-1119, handoff c8): every failure of making ONE
/// instance is that test's NOT RUN (rule 20), and only the classes named here and in
/// [`stops_suite`] stop the suite (rule 19). Before, the test's classes were listed and every
/// class not on the list stopped the suite: three new front-end classes stopped whole suites in
/// one day that way (`LazyRefusal::RecordsVoid` at tempo a1425; `LazyRefusal::NotFound` at
/// tempo-primitives 15:06, "the suite stopped" with 165 tests passing at 14:41).
fn refused_instance(cause: &ParentError) -> Option<([u8; 32], Option<(ItemKey, u32)>)> {
    use blaze_ladder::body_ids::WalkError;

    Some(match cause {
        // Every variant, no `_`: a new step of the walk is sorted when it is added (rule 73).
        ParentError::Walk { symbol, why } => match why {
            // The suite's: the store refused a read or a write, a stored entry does not read,
            // one symbol is tabled under two fns (an entry's declaration), or the walk's own
            // bound on its instances (rule 18).
            WalkError::Store(_) | WalkError::Entry { .. } | WalkError::TwoTabled { .. } | WalkError::Cap { .. } => return None,
            WalkError::Make { tabled, why } => (*symbol, Some((*tabled, refused_at(why)?))),
            // That instance's: a bound the test's code passes (an instance name past its cap,
            // the walk's names past their arena, an item instantiating itself past
            // `#![recursion_limit]`, rustc's "reached the recursion limit while
            // instantiating"), a fn or crate fact the command lacks, a name that does not list.
            // The error names the declarer, the depth or the size.
            WalkError::NoBody { tabled }
            | WalkError::NoCrateFact { tabled, .. }
            | WalkError::Name { tabled, .. }
            | WalkError::NameTooLarge { tabled, .. }
            | WalkError::ArenaFull { tabled, .. }
            | WalkError::RecursionLimit { tabled, .. } => (*symbol, Some((*tabled, 0))),
        },
        ParentError::Special { symbol, why } if !record_stops_suite(why) => (*symbol, None),
        ParentError::NotChecked { function, symbol, .. } => (*symbol, Some((*function, 0))),
        ParentError::NoPersonality { function, symbol, .. } => (*symbol, function.map(|item| (item, 0))),
        // The wire and the protocol of the run, its roots, the names records it opened, a
        // special record's store fault: the suite's. Never a failure inside one make.
        _ => return None,
    })
}

/// The byte of the body where making one instance refused (`Some(0)`: the body, no place), or
/// `None` for one of the faults [`stops_suite`] names. A refusal a later instance carried
/// (`NotBuilt`) is judged by the refusal it holds.
fn refused_at(why: &blaze_ladder::run::RunError) -> Option<u32> {
    use blaze_ladder::front2::program::Front2Refusal;
    use blaze_ladder::run::RunError;
    if stops_suite(why) {
        return None;
    }
    Some(match why {
        RunError::Build { range, .. } => range.map_or(0, |range| range.start),
        RunError::NotBuilt { refusal, .. } => return refused_at(refusal),
        RunError::Front2(Front2Refusal::Literal { range, .. }) => range.start,
        _ => 0,
    })
}

/// The few failures of making an instance that are the suite's own, never one test's: the
/// store (a code record that would not be stored, sealed or entered), a bound of the run
/// itself (its instance cap), and the run's setup and start (its crates, edition, manifest,
/// names, roots, the start's shape and process; the program's start rule, queue cap and crate
/// set, [`front_stops_suite`]). Anything else is that test's NOT RUN. A refusal `NotBuilt`
/// carries is judged by what it carries.
fn stops_suite(why: &blaze_ladder::run::RunError) -> bool {
    use blaze_ladder::run::RunError;
    match why {
        RunError::NotBuilt { refusal, .. } => stops_suite(refusal),
        RunError::CodeRecord { error, .. } => record_stops_suite(error),
        RunError::Front2(front) => front_stops_suite(front),
        RunError::TooManyInstances
        | RunError::OverflowDisagrees { .. }
        | RunError::Crates(_)
        | RunError::Edition { .. }
        | RunError::Names { .. }
        | RunError::Manifest { .. }
        | RunError::Roots(_)
        | RunError::NoMain
        | RunError::MainShape { .. }
        | RunError::NoHost { .. }
        | RunError::NoArgs { .. }
        | RunError::Args(_)
        | RunError::OwnProcess { .. } => true,
        _ => false,
    }
}

/// A code record's fault that is the store's: it would not be stored, sealed or entered. An
/// emit, a symbol, a routine, a shim or a datum that did not make is the instance's.
fn record_stops_suite(error: &blaze_ladder::code_records::CodeRecordError) -> bool {
    use blaze_ladder::code_records::CodeRecordError;
    matches!(error, CodeRecordError::Store(_) | CodeRecordError::Seal(_) | CodeRecordError::Entry(_))
}

/// The front end's refusals that are the suite's: the engine's own check state (the tables did
/// not fork, a record's reads differ from the list it was checked against), the program's queue
/// cap, its start (the start rule, the crate list) and its crate set (`Front2Refusal::Front`: a
/// program file no `mod` reaches, two values under one key (rule 21), a lang item defined twice,
/// roots that name no crate set; none is one instance's). A row's refusal is judged by what it
/// carries. Every other refusal, `LazyRefusal::NotFound`, `Unanswered` and `RecordsVoid` among
/// them, is the instance's.
fn front_stops_suite(why: &blaze_ladder::front2::program::Front2Refusal) -> bool {
    use blaze_ladder::front2::check::CheckFail;
    use blaze_ladder::front2::program::{Front2Refusal as Front, lazy::LazyRefusal};
    match why {
        Front::Lazy(LazyRefusal::Row { refusal, .. }) => front_stops_suite(refusal),
        Front::Lazy(LazyRefusal::Check { error, .. }) | Front::Asked { error, .. } | Front::Check(error) =>
            matches!(error, CheckFail::Tables { .. } | CheckFail::ReadsDiffer { .. }),
        Front::Front(_)
        | Front::Capped { .. }
        | Front::Unlisted { .. }
        | Front::NoMain
        | Front::MainTakenBy { .. }
        | Front::MainNotWritten { .. }
        | Front::NoLangStart { .. }
        | Front::Start { .. }
        | Front::NoHostedStart
        | Front::StartFact { .. }
        | Front::TwoEntries { .. } => true,
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
        assert_eq!(refused_instance(&ParentError::Walk { symbol, why: WalkError::Store(store()) }), None);
        assert_eq!(refused_instance(&make(RunError::CodeRecord { function: named(), error: CodeRecordError::Store(store()) })), None);
        assert_eq!(refused_instance(&ParentError::UndeclaredSlot { slot: 4 }), None);
        let range = blaze_typeck::Range { start: 15, end: 22 };
        let why = RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Literal { function: "lib::callee".into(), what: "an unsupported literal", range });
        assert_eq!(refused_instance(&make(why)), Some((symbol, Some((item, range.start)))));
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
        assert_eq!(refused_instance(&make(RunError::UnwindRaised { function: named() })), Some((symbol, Some((item, 0)))));
        assert_eq!(refused_instance(&make(RunError::Lower { refusals: std::vec::Vec::new(), shown: "a lowering".into() })), Some((symbol, Some((item, 0)))));
        let literal = RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Literal { function: "lib::callee".into(), what: "an unsupported literal", range });
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(literal), not_built: 1, reached: 2 };
        assert_eq!(refused_instance(&make(carried)), Some((symbol, Some((item, range.start)))), "a carried refusal is judged by what it carries");
        let store = RunError::CodeRecord { function: named(), error: CodeRecordError::Store(StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) }) };
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(store), not_built: 1, reached: 2 };
        assert_eq!(refused_instance(&make(carried)), None, "planted: a carried store fault stops the suite");
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
        assert_eq!(refused_instance(&make(void())), Some((symbol, Some((item, 0)))));
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(void()), not_built: 1, reached: 2 };
        assert_eq!(refused_instance(&make(carried)), Some((symbol, Some((item, 0)))), "twin: a carried void is the test's too");
        let store = StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) };
        assert_eq!(refused_instance(&ParentError::Walk { symbol, why: WalkError::Store(store) }), None, "control: a store fault stops the suite");
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
        assert_eq!(refused_instance(&walk(recursion)), Some((symbol, Some((item, 0)))));
        let large = WalkError::NameTooLarge { tabled: item, declarer: Some(declarer), bytes: 1 << 30, most: 1 << 29, heads: [Some(item), None, None] };
        assert_eq!(refused_instance(&walk(large)), Some((symbol, Some((item, 0)))));
        let arena = WalkError::ArenaFull { tabled: item, bytes: 1 << 32, adding: 20 };
        assert_eq!(refused_instance(&walk(arena)), Some((symbol, Some((item, 0)))));
        assert_eq!(refused_instance(&walk(WalkError::Cap { most: 4 })), None, "control: the walk's own cap stops the suite");
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
        assert_eq!(refused_instance(&make(RunError::Front2(Front2Refusal::Lazy(not_found())))), Some((symbol, Some((item, 0)))));
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(RunError::Front2(Front2Refusal::Lazy(not_found()))), not_built: 1, reached: 2 };
        assert_eq!(refused_instance(&make(carried)), Some((symbol, Some((item, 0)))), "twin: carried by `NotBuilt`");
        let row = Front2Refusal::Lazy(LazyRefusal::Row { function: "lib::callee".into(), refusal: std::boxed::Box::new(Front2Refusal::Lazy(not_found())) });
        assert_eq!(refused_instance(&make(RunError::Front2(row))), Some((symbol, Some((item, 0)))), "twin: through a row's refusal");
        let unanswered = Front2Refusal::Lazy(LazyRefusal::Unanswered { question: "each_item", item: "crate 91".into() });
        assert_eq!(refused_instance(&make(RunError::Front2(unanswered))), Some((symbol, Some((item, 0)))), "twin: a class no list names is the test's");
        let store = RunError::CodeRecord { function: named(), error: CodeRecordError::Store(StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) }) };
        let carried = RunError::NotBuilt { refusal: std::boxed::Box::new(store), not_built: 1, reached: 2 };
        assert_eq!(refused_instance(&make(carried)), None, "planted: a carried store fault stops the suite");
        let capped = Front2Refusal::Capped { entry: "lib::main".into(), queued: 9, cap: 8, items: 9, forced: false, report: "".into() };
        assert_eq!(refused_instance(&make(RunError::Front2(capped))), None, "control: the program's queue cap stops the suite");
        assert_eq!(refused_instance(&make(RunError::TooManyInstances)), None, "control: the run's instance bound stops the suite");
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
    /// Why its suite's native image could not be had ([`Suite::native`]): every row's refusal.
    pub image: Result<(), &'c str>,
    said: AString<'c>,
}

impl Refuse for EntryCheck<'_, '_> {
    fn refusal(&mut self, row: u32) -> Result<Option<&str>, OneError> {
        if let Err(why) = self.image {
            self.said.clear();
            write!(self.said, "its test program's native image: {why}").map_err(|_| OneError::Profile { why: Box::from("a refusal's words did not fit in memory") })?;
            return Ok(Some(self.said.as_str()));
        }
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
    /// Its suite's native library ([`Suite::native`]), as `blaze run` starts its program
    /// (own_process.rs `run_own`): named in the start (`BLAZE_LAUNCH_NATIVE`), told as the first
    /// frame (`Down::Native`, before the personality and any `Declare`), and the one file the
    /// profile lets the launcher map executable. `None` sends neither.
    pub native: Option<crate::engine_native::NativeSend<'c>>,
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

impl RunOne for Launched<'_, '_> {
    async fn run(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals) -> Result<How, OneError> {
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
    Ready(Symbol),
    Unresolved(Symbol),
}

impl<R: Resolve> Resolve for TestResolve<'_, R> {
    #[cfg(test)]
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        self.parent.resolve(symbol)
    }

    fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
        let answer = self.parent.publication(symbol, values)?;
        self.requested = Some(match answer {
            Resolved::Record { .. } => TestAnswer::Ready(*symbol),
            Resolved::Unknown => TestAnswer::Unresolved(*symbol),
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

impl Launched<'_, '_> {
    async fn run_captured(&self, row: u32, budget: Nanos, output: &mut Output<'_>, signals: &Signals, stderr: &mut StderrCapture<'_>, frame: &Frame) -> Result<How, OneError> {
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
        let front = TestFront { names: self.names, ids: self.ids, builder: self.builder, tables: self.tables, alloc, test, harness: self.tests.harnessed(), watch: self.watch };
        let mut codes = HomeCodeStore::new(self.lent);
        let mut lazy = Lazy::new(cx, front, RecordsDir { cx, dir: self.records }, &mut codes, self.builder, alloc);
        let mut needs = Needs::new(alloc);
        let mut ahead = std::vec::Vec::new_in(alloc);
        let mut placed = Vec::new_in(alloc);
        // The native library first, before any `Declare`: the child hands its entry over only
        // once it has loaded it (own_process.rs `run_own`, the same order).
        if let Some(send) = self.native {
            needs.tell_native(send.library, send.words, &mut ahead).map_err(|cause| test_parent_error(&mut lazy, cause))?;
        }
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
        let Started { child: mut run, pipes: given } = self.spawner.start_served(cx, &spec).map_err(OneError::Start)?;
        let until = Deadline::after(run.now(), Duration::from_nanos(budget.0));
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
        let child_error = |error| OneError::Loop(RunLoopError::Child(error));
        // Start the other ready children before building any first-call answer. The loop ends
        // with the child's end and what it left (its channel, still read for what it wrote).
        let (ended, mut over) = loop {
            next_turn().await;
            let _span = cx.span("blaze test: service one child's pipes, requests and deadline");
            let now = run.now();
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
            if open {
                match run.recv_message(&mut inbox, blaze_launch::wire::LONGEST_FRAME, poll) {
                    Ok(Message::Whole { len }) => {
                        self.watch.see(blaze_screen::Step::Waits);
                        let serving = run.now();
                        let before = lazy.tally();
                        let mut resolver = TestResolve { parent: &mut lazy, requested: None };
                        let taken = match needs.serve(&mut resolver, &inbox[..len], &mut reply) {
                            Ok(taken) => taken,
                            Err(cause) => {
                                // This group is waiting for code it will never receive.
                                // Kill and reap before returning its typed outcome to the plan.
                                run.kill().map_err(child_error)?;
                                return Err(test_parent_error(&mut lazy, cause));
                            }
                        };
                        let elapsed = run.now().saturating_sub(serving);
                        service_time += elapsed;
                        match resolver.requested {
                            Some(TestAnswer::Unresolved(Symbol(symbol))) => {
                                // Sending Failed would turn the unavailable instance into a launcher exit.
                                run.kill().map_err(child_error)?;
                                return Err(OneError::CannotBuild(InstanceRefusal {
                                    instance: lazy.name_of(symbol), symbol, site: None, cause: RefusalCause::Unresolved,
                                }));
                            }
                            Some(TestAnswer::Ready(symbol)) => {
                                let after = lazy.tally();
                                calls.push(FirstCall {
                                    symbol, elapsed, made: after.made - before.made,
                                    found: after.found - before.found, unkeyed: after.unkeyed - before.unkeyed,
                                });
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
                    Ok(Message::Closed) => open = false,
                    Err(error @ ProcError { fault: RunFault::Channel(ChannelError::Torn { .. }), .. }) => {
                        torn = Some(error);
                        open = false;
                    }
                    Ok(Message::Deadline) => {}
                    Err(error) => return Err(child_error(error)),
                }
            }
            let look = if open { Deadline::PASSED } else { poll };
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
                    Some(spot) if spot.placed == blaze_launch::wire::fault::Placed::Code => Some(lazy.folder().read(&spot.record, &mut bytes).map(|()| bytes.as_slice())),
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
        Ok(ended)
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
    pub watch: &'s dyn Watch,
}

impl<'s, 'h, 'r> Front for TestFront<'s, 'h, 'r> {
    type Start = Roots<'r>;
    type Ids = &'s RunIds<'h>;
    type Maker = RunMaker<'h>;

    fn start(&mut self) -> Result<Roots<'r>, ParentError> {
        entry_roots(self.harness, self.names, self.test, self.builder, self.tables, self.alloc).map_err(ParentError::Roots)
    }

    fn ids(&mut self) -> Result<&'s RunIds<'h>, ParentError> {
        Ok(self.ids)
    }

    fn maker(&mut self, start: &Roots<'r>) -> Result<RunMaker<'h>, ParentError> {
        // The test's roots start its program at the test fn (`MakerStart::Direct`), not at `main`.
        // The typing is the run's (run_parent.rs `RunMaker`: the flip changes both).
        Ok(Maker::of_roots(self.names, start.maker_entry(), start, SecondCheck, &self.builder, self.tables))
    }

    /// As run_parent.rs `CommandFront::path_of`: the one printer over the names records.
    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        use blaze_ladder::front2::source::NamesSource;
        let lib = self.names.records.key_beginning(item.0.to_be_bytes())?;
        blaze_ladder::front2::item_shown::ItemShown::of_lib(self.names.records, lib).path().map(Box::from)
    }

    fn named(&mut self, maker: Option<&RunMaker<'h>>, tabled: ItemKey, name: &[u8]) -> crate::run_fault::Named {
        use crate::run_fault::Named;
        if let Some(shown) = maker.and_then(|maker| maker.instance_shown(tabled, name)) {
            return Named::Shown { item: shown.item, args: shown.args };
        }
        match self.path_of(tabled) {
            Some(path) => Named::Path(path),
            None => Named::Item(tabled),
        }
    }

    fn site_shown(&mut self, maker: Option<&RunMaker<'h>>, item: ItemKey, site: u32) -> Option<crate::run_fault::SiteShown> {
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

/// `text` copied into `alloc`, borrowed for its life.
fn kept<'a>(text: &str, alloc: Alloc<'a>) -> &'a str {
    let bytes: &'a [u8] = Vec::from_slice_in(text.as_bytes(), alloc).leak();
    // A copy of a `str`'s bytes is UTF-8: the check cannot fail.
    core::str::from_utf8(bytes).unwrap_or_default()
}

/// The run place of the crate whose environment's facts are `facts` (the crate list's
/// `env_facts` of its row) in the checkout whose absolute root is `checkout`, in `alloc`
/// ([`PackageRun`]). Sans-IO: the root is read at the edge (engine_check.rs `Served::root_text`).
pub(crate) fn package_run<'a>(checkout: Option<&str>, facts: blaze_scope::crate_env::EnvFacts<'a>, alloc: Alloc<'a>) -> Result<PackageRun<'a>, PackageRunError<'a>> {
    let krate = facts.crate_name;
    let blaze_scope::crate_env::SeenRoot::Member { dir: member } = facts.seen else { return Err(PackageRunError::NotMember { krate }) };
    let Some(root) = checkout else { return Err(PackageRunError::NoRoot { krate }) };
    let mut dir: AString<'a> = AString::from_str_in(root.trim_end_matches('/'), alloc);
    if !member.is_empty() {
        dir.push_str("/");
        dir.push_str(member);
    }
    let mut manifest: AString<'a> = AString::from_str_in(dir.as_str(), alloc);
    manifest.push_str("/Cargo.toml");
    let env = blaze_scope::crate_env::CrateEnv::of(facts, alloc).map_err(PackageRunError::Env)?;
    let dir = kept(dir.as_str(), alloc);
    let mut vars: AVec<'a, (&'a str, &'a str)> = Vec::new_in(alloc);
    vars.push(("CARGO_MANIFEST_DIR", dir));
    vars.push(("CARGO_MANIFEST_PATH", kept(manifest.as_str(), alloc)));
    // The package's own facts; a build script's `rustc-env` pairs are the compile's alone.
    for (name, value) in env.vars().filter(|(name, _)| name.starts_with("CARGO_PKG_")) {
        vars.push((kept(name, alloc), kept(value, alloc)));
    }
    vars.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let vars: &'a [(&'a str, &'a str)] = vars.leak();
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
            builder: suite.builder, lent, tables, watch,
            args: if tests.harnessed() { &[] } else { args },
            native: suite.native.ok().flatten(),
        });
        checks.push(EntryCheck { tests, names: suite.names, builder: suite.builder, tables, image: suite.native.map(|_| ()), said: String::new_in(alloc) });
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
        let alloc = blaze_own::testing::heap().alloc();
        let member = SeenRoot::Member { dir: "crates/hardfork" };
        let run = package_run(Some("/w/tempo/"), facts(member), alloc).unwrap_or_else(|why| panic!("{why}"));
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

        let root = package_run(Some("/w/one"), facts(SeenRoot::Member { dir: "" }), alloc).unwrap_or_else(|why| panic!("{why}"));
        assert_eq!(root.dir, "/w/one", "the workspace's root package runs in the checkout's root");
        assert!(matches!(package_run(Some("/w/one"), facts(SeenRoot::Package), alloc), Err(PackageRunError::NotMember { krate: "tempo_hardfork" })));
        assert!(matches!(package_run(None, facts(member), alloc), Err(PackageRunError::NoRoot { krate: "tempo_hardfork" })));
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::engine_native::NativeSend;

    const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn send(hex: &'static str) -> NativeSend<'static> {
        NativeSend { library: blaze_launch::wire::RecordId([7; 32]), hex, words: &[] }
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
