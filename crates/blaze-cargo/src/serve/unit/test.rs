//! The tests of a build ([`Front::tests`](crate::serve::work::Front::tests)), as
//! `blaze_usercode::test` runs them, selected by blaze-testsel's machine
//! (docs/v2/plan/design/test-selection.md): per suite, the tests asked for ([`select::asked`])
//! go to a [`Selecting`] (the machine's selection) from the suite's index head
//! ([`image::Selectors`]); its effects are
//! performed on the store ([`image::Filed`]), the binary's image and the files now
//! ([`blaze_testsel::host::Driver`]), and the tests it runs go to one queue over every binary,
//! each in a process of its own forked from its binary's server: in the sandbox where the
//! workspace opts in, outside it otherwise (docs/cargo.md). Each test is reported as a
//! [`Response::TestResult`] by its verdict: a cached pass (with what settled it), a run kept,
//! or a run not kept with why. `--run-all` runs every test asked and still files what it
//! learns.
//!
//! `blaze test --ub` compiles nothing: [`ub_tests`] lists each test unit's tests as its harness
//! would, and [`evaluate`] runs one in blaze's MIR evaluator under the checker of undefined
//! behavior (`hir_ty_ub::run_test`).

#![forbid(unsafe_code)]

pub(crate) mod image;
mod traced;

#[cfg(test)]
mod tests;

use std::{
    alloc::Global,
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_api::{ApiError, Artifact, Level, Response, TestOutcome, TestResult, Ub};
use blaze_cas::Store;
use blaze_codegen::ExecTable;
use blaze_core::{Completion, Effect, Tag};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, ToolId},
    pool::Ticket,
    proof,
};
use blaze_lower::Db;
use blaze_testsel::{
    ask::Basis,
    host::{self, BaseOf, Driver, Drove, Ended, HostError, Out, Program},
    machine::{NotKept, Selection as Selecting, Start, Test as Asked, Verdict},
};
use blaze_units::schema::{Mode, Unit};
use blaze_usercode::test::{
    self, Attrs, Binary, Config, Filter, Finished, Ignored, Job as TestJob, Mode as TestMode,
    Outcome, Queue, Reported, ShouldPanic, Suite, Wanted, select,
};

use super::{
    Job, Real, UnitReader,
    compile::TestOf,
    error::{UnitError, manifest_dir},
    graph::flag,
    package_of,
};
use crate::serve::work::{Asks, Failed, RunPuts, Runs, Selection};

/// How long one test may run: libtest sets no limit, so a day, past which it is surely hung.
const TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a test's server is given to end a test it was told to kill.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// The inline asks a suite's index head carries before its postings are rewritten, at the
/// least (the machine's `Start::compact_at`).
const COMPACT_AT: u32 = 64;
/// A test's place among a suite's tests asked: none, for a test not asked.
const NOT_ASKED: u32 = u32::MAX;

/// The unit an artifact is of: the test of its package's target, by the graph's hash of it.
pub(crate) fn unit_of_artifact(job: &Job, artifact: &Artifact) -> Option<usize> {
    let all = &job.units().graph.units;
    let target = &artifact.target;
    let kinds: Vec<&str> = target.kinds().collect(); // alloc: once per artifact looked up, a word or two
    job.graph.find(all, &artifact.package, &target.name, &kinds, Mode::Test).map(|it| it as usize)
}

/// The target as selection names it: `lib`, `test:parse`, `bin:blaze`.
pub(crate) fn target_name(unit: &Unit) -> String {
    let kind = unit.target.kind.first().map(String::as_str).unwrap_or("");
    match kind {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => "lib".to_owned(),
        kind => format!("{kind}:{}", unit.target.name),
    }
}

pub(crate) fn attrs(test: &TestOf) -> Attrs {
    Attrs { ignore: test.ignore, should_panic: should_panic(&test.should_panic), bench: test.bench }
}

/// `#[should_panic]`, with its `expected` message if it gives one.
fn should_panic(of: &Option<Option<String>>) -> ShouldPanic {
    match of {
        None => ShouldPanic::No,
        Some(None) => ShouldPanic::Yes,
        Some(Some(message)) => ShouldPanic::WithMessage(message.clone()),
    }
}

/// The tests `selection` asks for, as libtest selects them: its names (those before `--` and
/// the free arguments after it), `--exact`, `--skip NAME`, `--ignored` and `--include-ignored`
/// (libtest's `cli::parse_opts`). Any other argument is refused, naming it: none is dropped.
pub(crate) fn filter(selection: &Selection<'_>) -> Result<Filter, ApiError> {
    let mut filter = Filter { names: selection.filter.to_vec(), ..Filter::default() };
    let (mut ignored, mut include) = (false, false);
    let mut args = selection.args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exact" => filter.exact = true,
            "--ignored" => ignored = true,
            "--include-ignored" => include = true,
            "--skip" => match args.next() {
                Some(name) => filter.skip.push(name.clone()),
                None => return Err(ApiError::new("argument to option 'skip' missing", None)),
            },
            arg => match arg.strip_prefix("--skip=") {
                Some(name) => filter.skip.push(name.to_owned()),
                None if !arg.starts_with('-') => filter.names.push(arg.to_owned()),
                None => {
                    return Err(ApiError::new(
                        format!(
                            "`{arg}` after `--`: blaze runs the tests itself and takes libtest's \
                             selection only (test names, `--exact`, `--skip NAME`, `--ignored`, \
                             `--include-ignored`)"
                        ),
                        None,
                    ));
                }
            },
        }
    }
    filter.ignored = match (include, ignored) {
        (true, true) => {
            return Err(ApiError::new(
                "the options --include-ignored and --ignored are mutually exclusive",
                None,
            ));
        }
        (true, false) => Ignored::Include,
        (false, true) => Ignored::Only,
        (false, false) => Ignored::Skip,
    };
    Ok(filter)
}

/// `blaze test --ub`'s tests, selected once by [`ub_tests`], each run by [`evaluate`].
pub(crate) struct UbTests {
    tests: Vec<UbTest>,
    /// Each test unit's would-be executable, as `Running` names it, by [`UbTest::binary`].
    binaries: Vec<String>,
    /// `tests` by key, sorted: [`evaluate`] finds its test in `O(log n)` with nothing allocated.
    by_key: Vec<u32>,
}

impl UbTests {
    fn find(&self, key: &str) -> Option<&UbTest> {
        let at = self
            .by_key
            .binary_search_by(|it| self.tests[*it as usize].key.as_str().cmp(key))
            .ok()?;
        Some(&self.tests[self.by_key[at] as usize])
    }
}

struct UbTest {
    /// What the daemon names it by: its package, its target as selection names it and its
    /// name, apart by spaces, which none of the three has.
    key: String,
    /// Its unit's executable, in [`UbTests::binaries`].
    binary: u32,
    name: String,
    function: hir_def::FunctionId,
    should_panic: ShouldPanic,
    bench: bool,
    /// `#[ignore]`d and not asked for: reported ignored, not run, as libtest does.
    ignored: bool,
}

/// The tests of every test unit `selection` asks for, by key ([`UbTest::key`]): unit by unit in
/// the build's order, each unit's by name as libtest sorts them. Its harness's listing
/// ([`super::codegen::test_roots`]) gives the same names and attributes as a compiled test
/// binary's.
pub(crate) fn ub_tests(
    job: &Job,
    reader: &UnitReader,
    selection: Selection<'_>,
) -> Result<Vec<String>, Failed> {
    if !selection.all {
        return Err(ApiError::new(
            "`blaze test --ub` runs every test it selects: which tests a change reached is known from \
             their compiled code, and `--ub` compiles nothing",
            None,
        )
        .into());
    }
    let filter = filter(&selection)?;
    let units = job.units();
    let (mut tests, mut binaries) = (Vec::new(), Vec::new());
    for at in 0..job.graph.len() {
        if !job.graph.has(at as u32, flag::TEST) {
            continue;
        }
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let unit = job.unit(at)?;
        let package = package_of(units, unit)?;
        let binary = u32::try_from(binaries.len()).expect("under 2^32 units");
        binaries.push(
            super::artifact::executable(unit, job.plan(at)?)
                .unwrap_or_else(|| unit.target.name.clone()),
        );
        let target = target_name(unit);
        let roots = super::codegen::test_roots(job, reader, at)
            .map_err(|what| ApiError::new(what, Some(unit.target.name.clone())))?;
        let first = tests.len();
        tests.reserve(roots.tests.len());
        for listed in &roots.tests {
            let attrs = Attrs {
                ignore: listed.ignore,
                should_panic: should_panic(&listed.should_panic),
                bench: listed.bench,
            };
            let ignored = match select::asked(&listed.name, &attrs, &filter) {
                Wanted::Filtered => continue,
                Wanted::Ignored => true,
                Wanted::Yes => false,
            };
            tests.push(UbTest {
                key: format!("{} {target} {}", package.name, listed.name),
                binary,
                name: listed.name.clone(),
                function: listed.function,
                should_panic: attrs.should_panic,
                bench: listed.bench,
                ignored,
            });
        }
        blaze_ids::names::sort_by_name(&mut tests[first..], |it| it.name.as_bytes());
    }
    let mut by_key: Vec<u32> = (0..u32::try_from(tests.len()).expect("under 2^32 tests")).collect();
    blaze_ids::names::sort_by_name_in(&mut by_key, |&it| tests[it as usize].key.as_bytes());
    if let Some(twice) =
        by_key.windows(2).find(|it| tests[it[0] as usize].key == tests[it[1] as usize].key)
    {
        let test = &tests[twice[0] as usize];
        let binary = &binaries[test.binary as usize];
        return Err(ApiError::new(
            format!("two test units list test `{}` of `{binary}`", test.name),
            None,
        )
        .into());
    }
    let keys = tests.iter().map(|it| it.key.clone()).collect();
    if job.ub.set(UbTests { tests, binaries, by_key }).is_err() {
        return Err(
            ApiError::new("`blaze test --ub` selected its tests twice in one job", None).into()
        );
    }
    Ok(keys)
}

/// Test `key` of [`ub_tests`] run in blaze's MIR evaluator under `ub` on `db`, the task's
/// shared front end, nothing compiled; a bench is refused, naming it (libtest runs one once with a
/// `Bencher`, which the evaluator has no part of).
pub(crate) fn evaluate(job: &Job, db: &Db, key: &str, ub: Ub) -> Result<TestResult, Failed> {
    let selected = job.ub.get();
    let test = selected.and_then(|it| it.find(key)).ok_or_else(|| {
        ApiError::new(format!("`blaze test --ub` selected no test `{key}`"), None)
    })?;
    let binary = selected.map_or("", |it| it.binaries[test.binary as usize].as_str());
    let result = |outcome, output: String, millis| TestResult {
        name: test.name.clone(),
        binary: binary.to_owned(),
        executable: test.binary,
        cached: None,
        not_kept: None,
        millis,
        output: output.into_bytes(),
        outcome,
        outside: false,
    };
    if test.ignored {
        return Ok(result(TestOutcome::Ignored, String::new(), 0));
    }
    if test.bench {
        let what = "a `#[bench]` function: blaze's MIR evaluator runs `#[test]` functions, and has no `Bencher`\n";
        return Ok(result(TestOutcome::Failed, what.to_owned(), 0));
    }
    let began = Instant::now();
    let outcome = hir_ty_ub::run_test(db, test.function, &ub_config(ub));
    let millis = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ran = match &outcome {
        hir_ty_ub::Outcome::Passed { .. } => Ran::Returned,
        hir_ty_ub::Outcome::Failed { message, .. } => Ran::Panicked(message),
        hir_ty_ub::Outcome::Ub { report, seed, .. } => Ran::Ub { report, seed: *seed },
        hir_ty_ub::Outcome::Unsupported { what } => Ran::Unsupported(what),
        hir_ty_ub::Outcome::Error { message, seed } => Ran::Stopped { message, seed: *seed },
    };
    let (outcome, output) = judged(ran, &test.should_panic);
    Ok(result(outcome, output, millis))
}

/// `blaze test --ub`'s settings as the checker takes them: what the request leaves out is the
/// checker's default.
pub(crate) fn ub_config(ub: Ub) -> hir_ty_ub::Config {
    let default = hir_ty_ub::Config::default();
    hir_ty_ub::Config {
        seed: ub.seed.unwrap_or(default.seed),
        schedules: ub.schedules.unwrap_or(default.schedules),
        ..default
    }
}

/// How a test ended in the evaluator.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Ran<'a> {
    Returned,
    /// It panicked with this message.
    Panicked(&'a str),
    /// Undefined behavior, as the checker reports it, and the seed that replays it.
    Ub {
        report: &'a str,
        seed: u64,
    },
    /// What the evaluator can't run.
    Unsupported(&'a str),
    /// The evaluator stopped with an error of its own (no layout, a MIR lowering error, a stack
    /// overflow, undefined behavior it found itself), on the schedule of `seed`.
    Stopped {
        message: &'a str,
        seed: u64,
    },
}

/// Whether a test that ran as `ran` passed, as libtest's `calc_result` judges it with its
/// `#[should_panic]`, and its output, in libtest's words. Undefined behavior and what the
/// evaluator can't run fail whatever the test expects: never a pass and never a run of
/// compiled code in its place.
pub(crate) fn judged(ran: Ran<'_>, expects: &ShouldPanic) -> (TestOutcome, String) {
    match (ran, expects) {
        (Ran::Returned, ShouldPanic::No) => (TestOutcome::Passed, String::new()),
        (Ran::Returned, _) => {
            (TestOutcome::Failed, "note: test did not panic as expected\n".to_owned())
        }
        (Ran::Panicked(message), ShouldPanic::No) => (TestOutcome::Failed, format!("{message}\n")),
        (Ran::Panicked(_), ShouldPanic::Yes) => (TestOutcome::Passed, String::new()),
        (Ran::Panicked(message), ShouldPanic::WithMessage(expected))
            if message.contains(expected.as_str()) =>
        {
            (TestOutcome::Passed, String::new())
        }
        (Ran::Panicked(message), ShouldPanic::WithMessage(expected)) => (
            TestOutcome::Failed,
            format!(
                "note: panic did not contain expected string\n      panic message: {message:?}\n expected substring: {expected:?}\n"
            ),
        ),
        (Ran::Ub { report, seed }, _) => (TestOutcome::Failed, format!("{report}\nseed: {seed}\n")),
        (Ran::Unsupported(what), _) => {
            (TestOutcome::Failed, format!("blaze's MIR evaluator can't run this test: {what}\n"))
        }
        (Ran::Stopped { message, seed }, _) => (
            TestOutcome::Failed,
            format!("blaze's MIR evaluator stopped: {message}\nseed: {seed}\n"),
        ),
    }
}

/// The result of `report`, a test of the run's executable `executable` (its index among the
/// run's test binaries), run outside the sandbox when `outside`, as its verdict says: `None`
/// for a test the machine did not judge (ignored, or a run whose keeping was refused).
fn result(report: test::Report, executable: u32, outside: bool, verdict: Option<Verdict>) -> TestResult {
    TestResult {
        binary: report.binary,
        executable,
        outside,
        name: report.name,
        cached: None,
        not_kept: verdict.and_then(not_kept),
        outcome: match report.outcome {
            Reported::Passed => TestOutcome::Passed,
            Reported::Failed => TestOutcome::Failed,
            Reported::Ignored => TestOutcome::Ignored,
        },
        output: report.output,
        millis: report.millis,
    }
}

/// Why a run that passed is not kept, as a result says it: `None` for a run kept, and for a
/// failure (a failure runs every time; its outcome says so).
fn not_kept(verdict: Verdict) -> Option<blaze_api::NotKept> {
    match verdict {
        Verdict::NotKept { why: NotKept::Failed, .. } | Verdict::Ran { .. } | Verdict::Cached { .. } => None,
        Verdict::NotKept { why, .. } => {
            // alloc: once per test not kept, its two words
            Some(blaze_api::NotKept { no: why.code().to_owned(), why: why.name().to_owned() })
        }
    }
}

/// What a cached result's basis says when its pass was kept by what its test reaches: the
/// index keeps the basis, not the run's own words.
const REACHED_WHY: &str = "its run's execution record was not whole, so it is kept by what it reaches";

/// The result of test `name` of `binary` (the run's executable `executable`), not run as a
/// kept pass of it holds now (kept by `by`, its last run taking `micros`): passed, in no
/// time, marked cached.
fn cached(binary: &str, executable: u32, name: &str, by: Basis, micros: Option<u64>) -> TestResult {
    TestResult {
        // clone: once per cached test, the strings its result owns
        name: name.to_owned(),
        binary: binary.to_owned(),
        executable,
        cached: Some(blaze_api::Cached {
            basis: match by {
                Basis::Executed => blaze_api::Basis::Executed,
                // alloc: once per test cached by reach
                Basis::Reached => blaze_api::Basis::Reached { why: REACHED_WHY.to_owned() },
            },
            millis: micros.map_or(0, |it| it / 1000),
            outcome: TestOutcome::Passed,
        }),
        not_kept: None,
        millis: 0,
        output: Vec::new(),
        outcome: TestOutcome::Passed,
        outside: false,
    }
}

/// One binary's tests as the queue and selection see them. Its tests are its unit's
/// ([`listing`]), by place in the harness's order.
struct Suited {
    name: String,
    /// Its unit, whose `Made` lists its tests and holds its execution table.
    unit: usize,
    /// Its binary's content: what its image is of.
    content: ContentId,
    /// The suite's settings ([`Suite::given`]): what its head is held by, and in each test's
    /// base key.
    given: ContentId,
    /// Its record makers and target: in every key of its selection.
    tool: ToolId,
    /// By test: what of its attributes selection reads.
    attrs: Vec<Attrs>,
    /// The tests by name, sorted: a finished test is found in `O(log n)` with nothing allocated.
    by_name: Vec<u32>,
    /// The binary's image now, the one its head was filed at and its head, taken from the
    /// daemon's for the run ([`image::Selectors::take`]); given back as the new head when its
    /// selection ends, and as it was taken when it does not.
    taken: Option<image::Taken>,
    /// Its tests' selection, from the first drive (which reads its head) until it ends.
    selection: Option<Selecting<'static, Global>>,
    driver: Driver,
    /// By test: its place among the tests asked ([`NOT_ASKED`] for one not asked).
    asked_at: Vec<u32>,
    /// By place among the tests asked: its test, and the ticket its run is given back with
    /// once the selection asked for it.
    asked: Vec<(u32, Option<Ticket>)>,
    /// Handed to the queue when it starts: sandboxed when the package's tests are.
    binary: Option<Binary>,
    /// The run of its `unsandboxed-tests`, outside, after every sandboxed test of the queue;
    /// none when `binary` is outside already.
    outside: Option<Binary>,
    /// By test: whether it runs in `binary` rather than in `outside`.
    inside: Vec<bool>,
    /// Why every test of it runs outside the sandbox, when they all do: said under `-v`.
    outside_why: Option<&'static str>,
    /// Under `-v`, the tests its package's `unsandboxed-tests` runs outside, by name.
    unsandboxed: Vec<String>,
    /// The machine's files and the binary's environment, as what a test read is checked
    /// against: made once per binary.
    disk: blaze_usercode::inputs::Disk,
    /// What the tests of its server all read, hashed once per buffer of it.
    common: traced::Common,
}

impl Suited {
    /// The place of the test `name` among `tests`, its unit's.
    fn find(&self, tests: &[TestOf], name: &str) -> Option<usize> {
        let at =
            self.by_name.binary_search_by(|it| tests[*it as usize].name.as_str().cmp(name)).ok()?;
        Some(self.by_name[at] as usize)
    }

    /// Its tests `filter` asks for, each ignored one reported to `out` as the run's executable
    /// `executable`, given to a new selection from its head and driven over `records` until it
    /// waits for its runs or ends: the runs it asks for into `runs`. `env` is what its binary
    /// gives each test. `None` when it asks for no test (its head is given back as taken).
    #[expect(clippy::too_many_arguments, reason = "one call, in `Runs::start`")]
    fn begin(
        &mut self,
        job: &Job,
        filter: &Filter,
        run_all: bool,
        env: &[(OsString, OsString)],
        records: &mut image::Filed<'_>,
        runs: &mut Vec<Out, Global>,
        executable: u32,
        out: &mut Vec<Response>,
    ) -> Result<Option<Drove>, String> {
        let (tests, _) = listing(job, self.unit);
        let mut asked: Vec<Asked<'_>, Global> = Vec::new_in(Global);
        self.asked.clear();
        for (n, test) in tests.iter().enumerate() {
            let attrs = &self.attrs[n];
            match select::asked(&test.name, attrs, filter) {
                Wanted::Filtered => continue,
                Wanted::Ignored => {
                    let report = test::report_unrun(&self.name, &test.name, Reported::Ignored, None);
                    out.push(Response::TestResult(result(report, executable, false, None)));
                    continue;
                }
                Wanted::Yes => {}
            }
            self.asked_at[n] = u32::try_from(self.asked.len()).expect("under 2^32 tests");
            self.asked.push((u32::try_from(n).expect("under 2^32 tests"), None));
            let base = host::base(&BaseOf {
                tool: self.tool,
                settings: self.given,
                env,
                name: &test.name,
                attrs: attrs.id(),
            });
            asked.push(Asked { base, name: &test.name });
        }
        if asked.is_empty() {
            self.give_back(&job.shared.selectors);
            return Ok(None);
        }
        let Some(taken) = self.taken.as_ref() else {
            // alloc: error path
            return Err(format!("the tests of `{}` were selected twice in one run", self.name));
        };
        let program = Program { now: &taken.now, before: taken.before.as_deref() };
        let (tool, suite, state, head) = (self.tool, self.given, taken.now.state(), taken.head);
        let workers = u32::try_from(asked.len()).expect("under 2^32 tests");
        let (driver, disk) = (&mut self.driver, &mut self.disk);
        // Every test asked can be out at once, so each lookup is answered before the first
        // wait and the selection is detached from the snapshot it read its head under.
        let begun = proof::scope(|snap| -> Result<(Selecting<'static, Global>, Drove), HostError> {
            let start = Start {
                tool,
                suite,
                program: state,
                head,
                certified: None,
                run_all,
                workers,
                compact_at: COMPACT_AT,
            };
            let mut selection = Selecting::new(snap, start, &asked, Global)?;
            let drove = driver.drive(&mut selection, records, program, disk, runs)?;
            Ok((selection.detach()?, drove))
        });
        let (selection, drove) = begun.map_err(|why| {
            // alloc: error path
            format!("selecting the tests of `{}` to run: {why}", self.name)
        })?;
        self.selection = Some(selection);
        Ok(Some(drove))
    }

    /// The run of its test `name` given to its selection, which then is driven over `records`
    /// until it waits or ends: the test's verdict, and where the drive stopped. `outside`: the
    /// trace says it ran outside the sandbox. `None` for the verdict once its selection was
    /// refused (said once, when it was).
    #[expect(clippy::too_many_arguments, reason = "one call, a finished test's parts")]
    fn ran(
        &mut self,
        job: &Job,
        records: &mut image::Filed<'_>,
        name: &str,
        passed: bool,
        took: Duration,
        executed: test::Executed,
        trace: test::Trace,
        outside: bool,
    ) -> Result<(Option<Verdict>, Option<Drove>), String> {
        let (tests, table) = listing(job, self.unit);
        let Some(n) = self.find(tests, name) else {
            // alloc: error path
            return Err(format!(
                "test `{name}` of `{}` ran, and is none of the tests its build listed: how it ran can't be kept",
                self.name
            ));
        };
        let place = self.asked_at[n];
        let Some(ticket) = self.asked.get(place as usize).and_then(|it| it.1) else {
            // alloc: error path
            return Err(format!(
                "test `{name}` of `{}` ran, and its selection did not ask for it",
                self.name
            ));
        };
        let (Some(selection), Some(taken)) = (self.selection.as_mut(), self.taken.as_ref()) else {
            return Ok((None, None));
        };
        let record = image::record(executed);
        let suite = self.name.as_str();
        // What the test reaches is walked only when its record is not whole and its pass can
        // be kept: never at link, as a binary of many tests would walk the program once per
        // test.
        let reached = || {
            let _span = tracing::info_span!(target: "blaze", "test reach", suite = %suite, test = %name).entered();
            table.reached(n)
        };
        let (micros, sandboxed) =
            (u64::try_from(took.as_micros()).unwrap_or(u64::MAX), self.inside[n] && !outside);
        let driver = &mut self.driver;
        let given = self
            .common
            .traced(trace, &mut self.disk, |traced| {
                let ended = Ended {
                    ticket,
                    name,
                    passed,
                    micros,
                    record: &record,
                    reached: &reached,
                    traced,
                    sandboxed,
                    foreign: tests[n].outside,
                };
                driver.ran(&mut *selection, &taken.now, &ended)
            })
            // alloc: error path
            .map_err(|why| format!("what test `{name}` of `{suite}` read can't be told, so how it ran can't be kept: {why}"))?;
        // alloc: error path
        given.map_err(|why| format!("keeping how test `{name}` of `{suite}` ran: {why}"))?;
        let verdict = selection.verdict(place);
        let program = Program { now: &taken.now, before: taken.before.as_deref() };
        let mut runs = Vec::new_in(Global);
        let drove = self
            .driver
            .drive(selection, records, program, &mut self.disk, &mut runs)
            // alloc: error path
            .map_err(|why| format!("selecting the tests of `{suite}` after `{name}` ran: {why}"))?;
        if let Some(late) = runs.first() {
            // alloc: error path
            return Err(format!("the tests of `{suite}`: {}", HostError::LateRun { test: late.test }));
        }
        Ok((verdict, Some(drove)))
    }

    /// Its selection ended: the new head given to `selectors` for its next run.
    fn ended(&mut self, selectors: &image::Selectors) -> Result<(), String> {
        let head = match self.selection.as_ref().and_then(|it| it.outcome()) {
            Some(Ok(report)) => report.head,
            Some(Err(why)) => {
                // alloc: error path
                let what = format!("selecting the tests of `{}`: {why}", self.name);
                self.give_back(selectors);
                return Err(what);
            }
            None => {
                self.give_back(selectors);
                // alloc: error path
                return Err(format!("the selection of the tests of `{}` ended with no outcome", self.name));
            }
        };
        self.selection = None;
        if let Some(taken) = self.taken.take() {
            selectors.give(self.given, image::Held { binary: self.content, image: taken.now, head: Some(head) });
        }
        Ok(())
    }

    /// Its selection dropped, and what it was taken from given back to `selectors` as it was:
    /// the head stands, as nothing replaced it.
    fn give_back(&mut self, selectors: &image::Selectors) {
        self.selection = None;
        if let Some(held) = self.taken.take().and_then(|it| it.held) {
            selectors.give(self.given, held);
        }
    }
}

/// The tests of `unit`, a suite's, in its harness's order, and its binary's execution table,
/// whose tests are in the same order: [`start`] checked both are there and agree.
fn listing(job: &Job, unit: usize) -> (&[TestOf], &ExecTable) {
    let made = job.made[unit].get().expect("a suite's unit was made: `start` checked");
    let table =
        made.table.as_ref().expect("a suite's binary has its execution table: `start` checked");
    (&made.tests, table)
}

/// The tests of `built`, filtered by `filter`, run on the loop.
pub(crate) struct TestRun {
    job: Arc<Job>,
    tags: u64,
    filter: Filter,
    all: bool,
    suites: Vec<Suited>,
    /// By the queue's index of a binary: the suite it is of. A suite gives the queue one
    /// binary, or two when some of its tests run outside the sandbox.
    suite_of: Vec<u32>,
    queue: Option<Queue>,
    ok: bool,
    /// What keeping how its tests ran put ([`Runs::take_puts`]).
    puts: RunPuts,
    /// The binaries whose tests can't run, each naming it and why ([`each`]): reported, and the
    /// run failed, when it starts.
    refused: Vec<ApiError>,
}

pub(crate) fn start(
    job: &Arc<Job>,
    built: &[Artifact],
    selection: Selection<'_>,
    tags: u64,
) -> Result<TestRun, ApiError> {
    if selection.ub.is_some() {
        return Err(ApiError::new(
            "`blaze test --ub` runs its tests in the MIR evaluator (`ub_tests`, `evaluate`), not as compiled binaries",
            None,
        ));
    }
    let filter = filter(&selection)?;
    let all = selection.all;
    // Keep-going: a binary that can't run is refused, naming it, and every other one runs.
    let (suites, refused) =
        each(built.iter().filter(|it| it.profile.test), |artifact| suited(job, artifact));
    Ok(TestRun {
        job: job.clone(),
        tags,
        filter,
        all,
        suites,
        suite_of: Vec::new(),
        queue: None,
        ok: true,
        puts: RunPuts::default(),
        refused,
    })
}

/// Each of `items` made by `make`: what each made (`None`: an item with nothing to run), and
/// every refusal, in order. One that is refused never keeps the rest from being made: a run
/// names every binary it can't run and runs every other one.
pub(crate) fn each<I, T>(
    items: impl Iterator<Item = I>,
    mut make: impl FnMut(I) -> Result<Option<T>, ApiError>,
) -> (Vec<T>, Vec<ApiError>) {
    // alloc: once per run, a suite and a refusal each
    let (mut made, mut refused) = (Vec::new(), Vec::new());
    for item in items {
        match make(item) {
            Ok(Some(it)) => made.push(it),
            Ok(None) => {}
            Err(error) => refused.push(error),
        }
    }
    (made, refused)
}

/// The suite of `artifact`, a test profile's: `None` for an artifact of no test unit, which
/// has no tests; an error naming the binary when its tests can't run.
fn suited(job: &Arc<Job>, artifact: &Artifact) -> Result<Option<Suited>, ApiError> {
    let units = job.units();
    let config = &job.input.config;
    let Some(program) = &artifact.executable else {
        // A test unit always makes its binary; an artifact of no test unit has no tests.
        if unit_of_artifact(job, artifact).is_some() {
            return Err(UnitError::NoTestExecutable { target: artifact.target.name.clone() }.into());
        }
        return Ok(None);
    };
    let Some(at) = unit_of_artifact(job, artifact) else {
        return Err(ApiError::new(
            format!("no test unit of `{}` is in the build", artifact.target.name),
            None,
        ));
    };
    let unit = &units.graph.units[at];
    let package = package_of(units, unit)?;
    let Some(made) = job.made[at].get() else {
        return Err(ApiError::new(
            format!("the test binary `{program}` was not compiled by this job"),
            None,
        ));
    };
    let Some(context) = made.context else {
        return Err(ApiError::new(
            format!("the test binary `{program}` was not compiled by this job"),
            None,
        ));
    };
    let suite = Suite { package: package.name.clone(), target: target_name(unit), context };
    let manifest_dir = manifest_dir(&package.name, &package.manifest_path)?;
    let sandboxed = !job.sandbox_off() && package.tests_sandboxed(units.sandboxed_tests);
    let outside_why = if job.sandbox_off() {
        Some("`BLAZE_SANDBOX=off` is set")
    } else if package.tests.all {
        Some("its package sets `tests = false` in `[package.metadata.blaze.sandbox]`")
    } else if !sandboxed {
        Some(
            "its package does not opt in (`tests = true` in `[package.metadata.blaze.sandbox]`, or the \
             workspace's `sandboxed-tests` experiment)",
        )
    } else {
        None
    };
    let unsandboxed = match sandboxed && job.input.verbose > 0 {
        // alloc: under `-v`, once per binary
        true => made
            .tests
            .iter()
            .filter(|it| package.tests.outside(&it.name))
            .map(|it| it.name.clone())
            .collect(),
        false => Vec::new(),
    };
    // alloc: one flag per test, once per binary
    let inside: Vec<bool> =
        made.tests.iter().map(|it| sandboxed && !package.tests.outside(&it.name)).collect();
    let Some(table) = &made.table else {
        return Err(ApiError::new(
            // alloc: error path
            format!(
                "the test binary `{program}` has no execution table: its link made none, so its tests can't be selected"
            ),
            None,
        ));
    };
    let agree = table.tests.len() == made.tests.len()
        && table.tests.iter().zip(&made.tests).all(|(kept, listed)| kept.0 == listed.name);
    if !agree {
        return Err(ApiError::new(
            // alloc: error path
            format!(
                "the execution table of the test binary `{program}` lists other tests than its harness"
            ),
            None,
        ));
    }
    if let Some(runner) = artifact.runner.first() {
        return Err(ApiError::new(
            format!(
                "the test binary `{program}` has the runner `{runner}` (`target.<triple>.runner`), and \
                 blaze's test queue runs a test binary itself, not under a runner"
            ),
            None,
        ));
    }
    let loaded: Option<test::Loaded> = match super::run::launch(job, at, false)
        .map_err(|what| ApiError::new(what, None))?
    {
        super::run::Launch::File => None,
        // Its processes start from the file its code memory writes, the start hook's variable
        // set, and its fork server attached as one that forks (`Program::command`).
        super::run::Launch::InProcess => {
            let held =
                job.shared.dev_runs.loaded(program).map_err(|what| ApiError::new(what, None))?;
            // clone: a count, once per binary
            Some(test::Loaded(held.program().clone()))
        }
    };
    let env: Vec<(OsString, OsString)> =
        super::run::process_env(&config.invocation.env, artifact, std::env::consts::OS);
    // `Binary` is not `Clone`: a suite with tests on both sides is given two.
    // clone: the binary's strings, once per binary and once more for one with tests outside
    let make = |sandboxed: bool| Binary {
        id: artifact.content,
        name: program.clone(),
        program: PathBuf::from(program),
        env: env.clone(),
        dir: manifest_dir.clone(),
        read: vec![
            PathBuf::from(&config.invocation.root),
            PathBuf::from(&units.target_dir),
            manifest_dir.clone(),
        ],
        write: Vec::new(),
        network: false,
        memory: None,
        mode: TestMode::Process,
        jobs: Vec::new(),
        loaded: loaded.clone(),
        sandboxed,
    };
    let binary = make(sandboxed);
    let outside = (sandboxed && !package.tests.tests.is_empty()).then(|| make(false));
    let tests = &made.tests;
    // alloc: once per binary, a place per test
    let mut by_name: Vec<u32> =
        (0..u32::try_from(tests.len()).expect("under 2^32 tests")).collect();
    blaze_ids::names::sort_by_name_in(&mut by_name, |&it| tests[it as usize].name.as_bytes());
    // clone: once per binary, the environment its tests' reads are checked in; each look at
    // the disk a check makes is the build's own ([`crate::serve::work::JobInput::looks`])
    let disk = blaze_usercode::inputs::Disk::new(binary.env.clone(), &job.input.looks);
    let tool = ToolId {
        tool: crate::serve::record_makers().map_err(|what| ApiError::new(what, None))?.id(),
        target: context,
        namespace: Namespace::LOCAL,
    };
    let given = suite.given();
    let taken = job
        .shared
        .selectors
        .take(given, artifact.content, || image::image(program, table))
        .map_err(|what| ApiError::new(what, None))?;
    Ok(Some(Suited {
        name: program.clone(),
        unit: at,
        content: artifact.content,
        given,
        tool,
        // alloc: once per binary, a test's attributes each
        attrs: tests.iter().map(attrs).collect(),
        by_name,
        taken: Some(taken),
        selection: None,
        driver: Driver::default(),
        // alloc: once per binary, a place per test
        asked_at: vec![NOT_ASKED; tests.len()],
        asked: Vec::new(),
        binary: Some(binary),
        outside,
        inside,
        outside_why,
        unsandboxed,
        disk,
        common: traced::Common::default(),
    }))
}

/// The history's row of a test the queue finished, `suite`'s: none for one not run. Its build
/// id is its job's, given as the job ends ([`crate::serve::work::RunPuts::test_runs`]). A binary
/// run whole is one row, named as its binary.
fn test_run_row(
    job: &Job,
    suite: &Suited,
    done: &Finished,
) -> Option<blaze_profile::facts::TestRunRow> {
    use blaze_profile::facts::TestOutcome as Ran;
    use blaze_usercode::test::Failure;
    let outcome = match &done.outcome {
        Outcome::Passed => Ran::Passed,
        Outcome::Failed(Failure::Code(_)) => Ran::Failed,
        Outcome::Failed(Failure::TimedOut(_)) => Ran::TimedOut,
        Outcome::Failed(Failure::Signal(_) | Failure::Lost(_)) => Ran::Crashed,
        Outcome::NotRun => return None,
    };
    let krate =
        job.unit(suite.unit).ok().and_then(|unit| job.package(unit).ok()).map(|it| it.name.clone()); // clone: the package's name, once per test
    Some(blaze_profile::facts::TestRunRow {
        build_id: 0,
        test: done.test.clone().unwrap_or_else(|| suite.name.clone()), // clone: the history's copy, once per test
        krate,
        binary: suite.name.clone(), // clone: as above
        outcome,
        dur_ns: i64::try_from(done.took.as_nanos()).unwrap_or(i64::MAX),
        retries: done.attempts.saturating_sub(1),
        ended_ns: blaze_profile::clock::mono_ns(),
    })
}

impl TestRun {
    fn now(&self) -> Duration {
        self.job.began.elapsed()
    }

    /// What finished: reported, and kept for selection.
    fn finished(
        &mut self,
        finished: impl Iterator<Item = Finished>,
        store: &mut Store,
        out: &mut Vec<Response>,
    ) {
        let color = super::compile::drawing(&self.job).color;
        let refused = |what: String| Response::Diagnostic(super::say::refused(what, None, color));
        // clone: a reference count, once per batch of results
        let job = self.job.clone();
        for done in finished {
            let Some((executable, suite)) = self
                .suite_of
                .get(done.binary)
                .and_then(|&at| Some((at, self.suites.get_mut(at as usize)?)))
            else {
                self.ok = false;
                out.push(refused(format!(
                    "the test queue gave a result of its binary {}, which this run did not give it",
                    done.binary
                )));
                continue;
            };
            if let Outcome::Failed(_) = done.outcome {
                self.ok = false;
            }
            if let Some(row) = test_run_row(&job, suite, &done) {
                self.puts.test_runs.push(row);
            }
            let outside = matches!(&done.trace, test::Trace::Unknown(why) if why == test::OUTSIDE);
            let passed = match done.outcome {
                Outcome::Passed => true,
                Outcome::Failed(_) => false,
                // Not run (the run was cancelled), which the API does not report: its selection
                // never ends, and its head is given back as it was taken ([`TestRun::end`]).
                Outcome::NotRun => continue,
            };
            let Some(report) = test::report(&suite.name, &done) else { continue };
            let Some(name) = done.test.as_deref() else {
                self.ok = false;
                out.push(Response::TestResult(result(report, executable, outside, None)));
                // alloc: error path
                out.push(refused(format!(
                    "the test binary `{}` was run whole, and blaze selects and keeps its tests one by one",
                    suite.name
                )));
                suite.give_back(&job.shared.selectors);
                continue;
            };
            let mut records = image::Filed { store: &mut *store, puts: &mut self.puts };
            let ran = suite.ran(&job, &mut records, name, passed, done.took, done.executed, done.trace, outside);
            match ran {
                Ok((verdict, drove)) => {
                    out.push(Response::TestResult(result(report, executable, outside, verdict)));
                    if drove == Some(Drove::Ended)
                        && let Err(why) = suite.ended(&job.shared.selectors)
                    {
                        self.ok = false;
                        out.push(refused(why));
                    }
                }
                Err(why) => {
                    self.ok = false;
                    out.push(Response::TestResult(result(report, executable, outside, None)));
                    out.push(refused(why));
                    suite.give_back(&job.shared.selectors);
                }
            }
        }
    }

    /// The run's verdict, each suite whose selection did not end given back what it took, so
    /// its head stands for the next run.
    fn end(&mut self) -> Option<bool> {
        for suite in &mut self.suites {
            if suite.selection.is_some() || suite.taken.is_some() {
                suite.give_back(&self.job.shared.selectors);
            }
        }
        Some(self.ok)
    }
}

impl Runs<Real> for TestRun {
    fn start(
        &mut self,
        store: &mut Store,
        _writer: Option<&mut dyn Asks>, // dyn: as `Runs`'s
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let mut binaries = Vec::with_capacity(self.suites.len());
        self.suite_of.clear();
        for refused in std::mem::take(&mut self.refused) {
            self.ok = false;
            let what = match refused.at {
                Some(at) => format!("the tests of `{at}` can't run: {}", refused.what),
                None => refused.what,
            };
            out.push(Response::Diagnostic(super::say::refused(what, None, super::compile::drawing(&self.job).color)));
        }
        if self.job.input.verbose > 0 {
            for suite in &self.suites {
                let said = match (suite.outside_why, suite.unsandboxed.as_slice()) {
                    (Some(why), _) => {
                        format!("the tests of `{}` run outside the sandbox: {why}", suite.name)
                    }
                    (None, []) => continue,
                    (None, names) => format!(
                        "{} of `{}` run outside the sandbox: its package's `unsandboxed-tests` names them",
                        names.iter().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", "),
                        suite.name
                    ),
                };
                // alloc: under `-v`, a note per binary with tests outside
                out.push(Response::Diagnostic(super::say::cargo(
                    Level::Note,
                    said,
                    None,
                    super::compile::drawing(&self.job).color,
                )));
            }
        }
        // clone: a reference count, once per run
        let job = self.job.clone();
        let color = super::compile::drawing(&job).color;
        let mut runs: Vec<Out, Global> = Vec::new_in(Global);
        for (at, suite) in self.suites.iter_mut().enumerate() {
            let Some(mut binary) = suite.binary.take() else { continue };
            let at = u32::try_from(at).expect("under 2^32 suites");
            let mut outside = suite.outside.take();
            let (tests, _) = listing(&job, suite.unit);
            runs.clear();
            let mut records = image::Filed { store: &mut *store, puts: &mut self.puts };
            let begun = suite.begin(
                &job,
                &self.filter,
                self.all,
                &binary.env,
                &mut records,
                &mut runs,
                at,
                out,
            );
            match begun {
                Err(why) => {
                    // Its tests asked are not run: nothing is run that its selection did not
                    // ask for, and the run fails, naming the binary.
                    self.ok = false;
                    out.push(Response::Diagnostic(super::say::refused(why, None, color)));
                    suite.give_back(&job.shared.selectors);
                }
                Ok(None) => {}
                Ok(Some(drove)) => {
                    for run in &runs {
                        let Some((n, ticket)) = suite.asked.get_mut(run.test as usize) else {
                            self.ok = false;
                            // alloc: error path
                            let what = format!(
                                "the selection of the tests of `{}` asked to run its test {}, which it was not given",
                                suite.name, run.test
                            );
                            out.push(Response::Diagnostic(super::say::refused(what, None, color)));
                            continue;
                        };
                        *ticket = Some(run.ticket);
                        let n = *n as usize;
                        let binary = match (suite.inside[n], &mut outside) {
                            (false, Some(outside)) => outside,
                            _ => &mut binary,
                        };
                        binary.jobs.push(TestJob {
                            // clone: the queue owns its jobs' names, once per test run
                            name: tests[n].name.clone(),
                            ignore: false,
                            expected: Duration::ZERO,
                            timeout: TIMEOUT,
                            retries: 0,
                        });
                    }
                    if let Some(selection) = &suite.selection {
                        for (place, &(n, _)) in suite.asked.iter().enumerate() {
                            let place = u32::try_from(place).expect("under 2^32 tests");
                            if let Some(Verdict::Cached { basis, micros, .. }) = selection.verdict(place) {
                                let name = &tests[n as usize].name;
                                out.push(Response::TestResult(cached(&suite.name, at, name, basis, micros)));
                            }
                        }
                    }
                    if drove == Drove::Ended
                        && let Err(why) = suite.ended(&job.shared.selectors)
                    {
                        self.ok = false;
                        out.push(Response::Diagnostic(super::say::refused(why, None, color)));
                    }
                }
            }
            binaries.push(binary);
            self.suite_of.push(at);
            if let Some(outside) = outside {
                binaries.push(outside);
                self.suite_of.push(at);
            }
        }
        // The run's sockets and records go under the daemon's state. The queue names them to the
        // driver from its root, `/` ([`crate::serve::file_name`]), and gives its processes that
        // root joined with the name, so both see one file; a name relative to the state would be
        // written at `/tests/..`.
        let path = self.job.shared.state.join(format!("tests/{:016x}", self.tags)); // alloc: once per test run
        let dir = match crate::serve::file_name(&path) {
            Ok(dir) => dir,
            Err(why) => {
                self.ok = false;
                // alloc: error path
                let what =
                    UnitError::TestDir { dir: path.display().to_string(), why: format!("{why:#}") }
                        .to_string();
                out.push(Response::Diagnostic(super::say::refused(
                    what,
                    None,
                    super::compile::drawing(&self.job).color,
                )));
                return self.end();
            }
        };
        // The daemon's budget, which the queue charges every test thread to: a binary run whole
        // gets `--test-threads` of what is free beside the others, a test in a process of its
        // own one, so the threads running at once never pass it ([`Config::jobs`]).
        let jobs = match blaze_core::cores::get() {
            Ok(cores) => cores.budget.get(),
            Err(why) => {
                self.ok = false;
                // alloc: error path
                let what = format!("the tests of this run are sized to blaze's core budget: {why}");
                out.push(Response::Diagnostic(super::say::refused(
                    what,
                    None,
                    super::compile::drawing(&self.job).color,
                )));
                return self.end();
            }
        };
        let config = Config {
            root: PathBuf::from("/"), // alloc: once per test run
            dir,
            jobs,
            fail_fast: false,
            kill_grace: KILL_GRACE,
            tags: self.tags,
        };
        let now = self.now();
        let queue = Queue::new(config, binaries, now, effects);
        let finished = queue.finished();
        self.queue = Some(queue);
        match finished {
            true => self.end(),
            false => None,
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        self.queue.as_ref().is_some_and(|it| it.owns(tag))
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        _writer: Option<&mut dyn Asks>, // dyn: as `Runs`'s
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let now = self.now();
        let mut queue = self.queue.take()?;
        queue.complete(completion, now, effects);
        self.finished(queue.drain(), store, out);
        let done = queue.finished();
        self.queue = Some(queue);
        match done {
            true => self.end(),
            false => None,
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        self.ok = false;
        let now = self.now();
        match &mut self.queue {
            Some(queue) => {
                queue.cancel(now, effects);
                match queue.finished() {
                    true => self.end(),
                    false => None,
                }
            }
            None => self.end(),
        }
    }

    fn take_puts(&mut self) -> RunPuts {
        std::mem::take(&mut self.puts)
    }
}
