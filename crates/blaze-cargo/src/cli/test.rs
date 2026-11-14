//! `test` and `bench` after the build, as cargo's `ops::run_tests` and `run_benches` run them
//! (`ops/cargo_test.rs`): each test executable in cargo's order, then the doc tests, stopping
//! every one by default, the failures listed at the end with each test executable that never
//! built and the failure that held it back; `--fail-fast` stops at the first that fails, as
//! cargo does; `--no-run` lists them. The tests the daemon runs and selects (only a test whose
//! executed code or reads changed reruns; `--run-all` runs every one asked): its
//! [`TestResult`]s in libtest's format, each cached test marked `(cached)` and each pass not
//! kept with why ([`from_daemon`]).
//!
//! A run that ran no test is refused (`useful-not-parity`; cargo and libtest end 0 with
//! `test result: ok. 0 passed`): every path counts what ran into one [`Tally`], [`refusal`]
//! is the one rule over it, and [`conclude`] says the run's end with the counts, so a pass
//! always says how many tests it is a pass of. A test executable's stdout comes through this
//! process ([`Spawned::run`]), which is how the counts are known off a terminal; on one the
//! runner reads libtest's JSON (`cli::live`).

use std::{
    alloc::Global,
    ffi::OsString,
    fmt::Write as _,
    io::{self, IsTerminal as _, Read as _, Write},
    path::{Path, PathBuf},
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

use blaze_api::{Artifact, ArtifactKind, TargetKind, TestOutcome, TestResult, Unbuilt};
use blaze_core::exit::Exit;
use blaze_screen::Tally;
use crossbeam_channel::RecvTimeoutError;

use super::{
    args::{Command, Invocation},
    run::{Exec, caused},
    say,
};

/// How long a test executable's output is read after its process exited. Whatever it wrote
/// is in the pipe by then, so only a process it started and left running, which inherited its
/// stdout or stderr, holds the pipe open past this; the reader then stops waiting and says so.
pub const DRAIN: Duration = Duration::from_secs(2);

/// How a test executable ended, and what it said it ran.
#[derive(Clone, Copy)]
pub struct Ran {
    pub ended: say::Ended,
    /// The sum of the result lines it printed ([`counts_of`]); `None` when it printed none.
    pub counts: Option<Tally>,
}

/// What runs a test executable: the process itself, or the tests' fake.
pub trait Programs {
    /// Runs `exec` to its end, its stdout written through to this process's and read for
    /// libtest's result lines. `harness`: it is libtest's, which colours by whether its
    /// stdout is a terminal.
    fn run(&mut self, exec: &Exec, harness: bool) -> io::Result<Ran>;

    /// The tests `exec` (a test executable run with [`LIST`]) lists, for a refusal's help.
    fn names(&mut self, exec: &Exec) -> io::Result<Vec<String>>;

    /// Readies `exec` to start `artifact`'s program, asking the daemon whether it runs on its
    /// code memory ([`super::run::launch`]); the tests' fake asks no daemon.
    fn launch(&mut self, artifact: &Artifact, exec: &mut Exec) -> Result<(), String>;

    /// What starts test executable `artifact`, whose command is `exec`, as a child: on the
    /// daemon's code memory when the daemon holds it as a dev executable, never from its file
    /// ([`super::run::TRAMPOLINE`]); the tests' fake starts `exec`.
    fn started(&mut self, artifact: &Artifact, exec: &Exec) -> Result<Exec, String>;
}

/// Runs each program as a process of its own: its stdin and stderr this process's, its stdout
/// through this process, which counts what it says it ran.
pub struct Spawned;

impl Programs for Spawned {
    /// The executable's stdout is a pipe to this process, which writes each chunk on at once
    /// and reads it for result lines; its stdin and stderr are this process's own. Its exit is
    /// waited for apart from its output, which is read for at most [`DRAIN`] after it.
    fn run(&mut self, exec: &Exec, harness: bool) -> io::Result<Ran> {
        let mut command = exec.command();
        let told = exec.args.iter().any(|it| it.to_string_lossy().starts_with("--color"));
        if harness && !told && io::stdout().is_terminal() {
            command.arg("--color=always");
        }
        command.stdout(Stdio::piped());
        let mut out = own_stdout()?;
        let mut child = command.spawn()?;
        let Some(mut from) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            let what = format!("{}: its stdout was asked for as a pipe and none came", exec.shown());
            return Err(io::Error::other(what));
        };
        let (counted, counts) = crossbeam_channel::unbounded::<io::Result<Tally>>();
        let reader = thread::Builder::new().name("blaze test output".into()).spawn(move || {
            let mut scan = Scan::default();
            let mut found = |tally: Tally| {
                let _ = counted.send(Ok(tally));
            };
            let mut chunk = [0; 8192];
            loop {
                match from.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(error) = out.write_all(&chunk[..n]) {
                            let _ = counted.send(Err(error));
                            return;
                        }
                        scan.feed(&chunk[..n], &mut found);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let _ = counted.send(Err(error));
                        return;
                    }
                }
            }
            scan.end(&mut found);
        });
        if let Err(error) = reader {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let status = {
            let _waiting = tracing::info_span!(
                target: "blaze",
                "test.wait_exit",
                program = %exec.program.display(),
            )
            .entered();
            child.wait()?
        };
        let _draining = tracing::info_span!(
            target: "blaze",
            "test.read_output",
            program = %exec.program.display(),
        )
        .entered();
        let deadline = Instant::now() + DRAIN;
        let mut sum = None;
        loop {
            match counts.recv_deadline(deadline) {
                Ok(Ok(tally)) => sum.get_or_insert_with(Tally::default).add(&tally),
                Ok(Err(error)) => return Err(error),
                // The reader ended: all the executable wrote was written on.
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let what = format!(
                        "{} exited, but a process it started still holds its stdout open; what that \
                         process prints from now on is still written, and is not read for test results",
                        exec.shown()
                    );
                    io::stderr().write_all(say::warning(&what, false).as_bytes())?;
                    break;
                }
            }
        }
        Ok(Ran { ended: say::Ended::of(status), counts: sum })
    }

    fn names(&mut self, exec: &Exec) -> io::Result<Vec<String>> {
        let _listing = tracing::info_span!(
            target: "blaze",
            "test.list",
            program = %exec.program.display(),
        )
        .entered();
        let output = exec.command().stdin(Stdio::null()).output()?;
        if !output.status.success() {
            let what = format!("{} ended with {}", exec.shown(), say::ended(say::Ended::of(output.status)));
            return Err(io::Error::other(what));
        }
        Ok(listed(&String::from_utf8_lossy(&output.stdout)))
    }

    fn launch(&mut self, artifact: &Artifact, exec: &mut Exec) -> Result<(), String> {
        super::run::launch(artifact, exec)
    }

    /// On Unix through [`super::run::TRAMPOLINE`], which the daemon launches the program as.
    /// Elsewhere the launch is asked here, which a program the daemon runs on its code memory
    /// refuses: a child's process ID is known only once it is spawned.
    fn started(&mut self, artifact: &Artifact, exec: &Exec) -> Result<Exec, String> {
        // clone: once per test executable run, its command
        let mut exec = exec.clone();
        match cfg!(unix) {
            true => super::run::through_trampoline(artifact, exec),
            false => super::run::launch(artifact, &mut exec).map(|()| exec),
        }
    }
}

/// This process's stdout as a test executable had it when it inherited it: the descriptor
/// itself, unbuffered, and not std's handle, which the command's thread holds locked while the
/// command runs (`cli::command`), so another thread's write through it would never return.
fn own_stdout() -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;
        io::stdout().as_fd().try_clone_to_owned().map(std::fs::File::from)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle as _;
        io::stdout().as_handle().try_clone_to_owned().map(std::fs::File::from)
    }
}

/// cargo's `TestKind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Test,
    Bench,
    Doctest,
}

impl Kind {
    fn failed(self) -> &'static str {
        match self {
            Kind::Test => "test failed",
            Kind::Bench => "bench failed",
            Kind::Doctest => "doctest failed",
        }
    }
}

/// The order of cargo's `TargetKind`: lib, bin, test, bench, example, build script.
fn rank(artifact: &Artifact) -> u8 {
    match artifact.target.kind {
        TargetKind::Lib => 0,
        TargetKind::Bin => 1,
        TargetKind::Test => 2,
        TargetKind::Bench => 3,
        TargetKind::Example => 4,
        TargetKind::CustomBuild => 5,
    }
}

/// The test executables and the doc tests the build placed, each in cargo's order (its
/// `compilation.tests` sorted: by package, then target).
pub fn executables(artifacts: &[Artifact]) -> (Vec<&Artifact>, Vec<&Artifact>) {
    let mut tests: Vec<&Artifact> = artifacts
        .iter()
        .filter(|it| it.executable.is_some() && it.profile.test && it.kind != ArtifactKind::Doctest)
        .collect();
    let mut docs: Vec<&Artifact> = artifacts
        .iter()
        .filter(|it| it.kind == ArtifactKind::Doctest && it.executable.is_some())
        .collect();
    tests.sort_by_cached_key(|it| order(it));
    docs.sort_by_cached_key(|it| order(it));
    (tests, docs)
}

/// cargo's order of units: by package (name, version, id), kind, then target name; each
/// package id read once per executable, not once per comparison.
fn order(artifact: &Artifact) -> (&str, &str, &str, u8, &str) {
    let id = artifact.package.as_str();
    (
        say::package_name(id),
        say::package_version(id),
        id,
        rank(artifact),
        artifact.target.name.as_str(),
    )
}

/// The package's directory: cargo runs a test there.
fn package_root(artifact: &Artifact) -> PathBuf {
    Path::new(&artifact.manifest_path).parent().map_or_else(PathBuf::new, Path::to_owned)
}

/// `unittests src/lib.rs (target/debug/deps/foo-0123abcd)`, `tests/x.rs (target/debug/deps/x-4567)`:
/// cargo's `exe_display` of `cmd_builds`.
pub fn running(artifact: &Artifact, cwd: &Path, kind: Kind) -> String {
    let source = Path::new(&artifact.target.src_path);
    let root = package_root(artifact);
    let source = source.strip_prefix(&root).unwrap_or(source).display();
    let exe = say::shown(Path::new(artifact.executable.as_deref().unwrap_or(&artifact.path)), cwd);
    match artifact.target.kind {
        TargetKind::Test | TargetKind::Bench => format!("{source} ({exe})"),
        TargetKind::Lib | TargetKind::Bin | TargetKind::Example | TargetKind::CustomBuild => {
            let unit = if kind == Kind::Bench { "benches" } else { "unittests" };
            format!("{unit} {source} ({exe})")
        }
    }
}

/// What cargo says to pass to rerun one that failed (`UnitTestError::cli_args`).
pub fn rerun(artifact: &Artifact, kind: Kind, spec_flag: bool) -> String {
    // alloc: one failure's text.
    let mut args = String::new();
    if spec_flag {
        let _ = write!(args, "-p {} ", say::package_name(&artifact.package));
    }
    if kind == Kind::Doctest {
        args.push_str("--doc");
        return args;
    }
    let flag = match artifact.target.kind {
        TargetKind::Bin => "--bin",
        TargetKind::Test => "--test",
        TargetKind::Bench => "--bench",
        TargetKind::Example => "--example",
        TargetKind::Lib | TargetKind::CustomBuild => {
            args.push_str("--lib");
            return args;
        }
    };
    let _ = write!(args, "{flag} {}", artifact.target.name);
    args
}

/// How the command speaks.
#[derive(Clone, Copy, Debug)]
pub struct Speaking<'a> {
    pub cwd: &'a Path,
    pub color: bool,
    pub quiet: bool,
    pub verbose: u32,
    /// `--message-format` is one of the JSON ones.
    pub json: bool,
}

impl Speaking<'_> {
    // dyn: the CLI's output writer, a few writes per test executable run.
    fn status(&self, err: &mut dyn Write, word: &str, rest: &str) -> io::Result<()> {
        match self.quiet {
            true => Ok(()),
            false => err.write_all(say::status(word, rest, self.color).as_bytes()),
        }
    }

    /// `Running` or `Executable` and what runs: cargo's `exe_display` (`concise`), or under `-v`
    /// the command itself (`verbose`).
    // dyn: the CLI's output writer, a few writes per test executable run.
    fn running(
        &self,
        err: &mut dyn Write,
        word: &str,
        artifact: &Artifact,
        kind: Kind,
        shown: &str,
    ) -> io::Result<()> {
        match self.verbose {
            0 => self.status(err, word, &running(artifact, self.cwd, kind)),
            _ => self.status(err, word, shown),
        }
    }
}

/// One that failed: what reruns it, and the code it ended with.
struct Failed {
    rerun: String,
    code: Option<i32>,
}

/// cargo's `Packages::needs_spec_flag`: whether "to rerun pass" names the package. `members` is
/// the workspace's member count, which stands for cargo's default members too.
pub fn needs_spec_flag(asked: &Invocation, members: u32) -> bool {
    !asked.flags.packages.is_empty() || !asked.flags.excludes.is_empty() || members > 1
}

/// `test` or `bench` once the build placed `artifacts`, of a workspace of `members`, with the
/// test units the build left `unbuilt`: cargo's exit code.
pub fn run(
    asked: &Invocation,
    artifacts: &[Artifact],
    unbuilt: &[Unbuilt],
    members: u32,
    speaking: Speaking<'_>,
    err: &mut dyn Write, // dyn: the CLI's output writer, a few writes per test executable run.
    programs: &mut impl Programs,
    tally: &mut Tally,
) -> io::Result<i32> {
    tally.unbuilt = tally.unbuilt.saturating_add(u32::try_from(unbuilt.len()).unwrap_or(u32::MAX));
    let bench = asked.command() == Command::Bench;
    let unit_kind = if bench { Kind::Bench } else { Kind::Test };
    let (tests, docs) = executables(artifacts);
    let spec_flag = needs_spec_flag(asked, members);
    // cargo's `display_no_run_information`, which JSON leaves out: its artifacts name them.
    if asked.flags.no_run {
        if !speaking.json {
            for test in &tests {
                let exec = exec_of(test, asked, speaking);
                speaking.running(err, "Executable", test, unit_kind, &exec.shown())?;
            }
        }
        return Ok(0);
    }
    let mut failed = Vec::new();
    for test in &tests {
        let exec = exec_of(test, asked, speaking);
        let shown = exec.shown_at(speaking.verbose);
        speaking.running(err, "Running", test, unit_kind, &shown)?;
        if let Some(failure) =
            ran(test, unit_kind, &exec, &shown, asked, spec_flag, speaking, err, programs, tally)?
        {
            failed.push(failure);
            if asked.flags.fail_fast {
                return Ok(failed[0].code.unwrap_or(Exit::Failed.code()));
            }
        }
    }
    if !bench && let Some(code) = docs_ran(&docs, asked, spec_flag, speaking, err, programs, tally, &mut failed)? {
        return Ok(code);
    }
    // cargo's `no_fail_fast_err`: without `--fail-fast` any failure is its own error, code 101.
    if failed.is_empty() && unbuilt.is_empty() {
        return Ok(0);
    }
    let said = failed_summary(&failed, unbuilt);
    err.write_all(say::error(&said, speaking.color).as_bytes())?;
    Ok(Exit::Failed.code())
}

/// What a doc test's pass is not kept by: the daemon neither runs nor traces a doc test's
/// examples, so each runs every time (docs/cargo.md, "Which tests run").
pub const DOC_TESTS_NOT_KEPT: &str =
    "doc tests run every time: blaze does not trace a doc test's run, so its pass is not kept";

/// The doc tests of a `test` whose other tests the daemon ran ([`from_daemon`]), once its
/// results are said, as cargo runs them after the test executables: each runs, none is
/// selected or kept, and a note says so once before the first. Cargo's exit code of them
/// alone; their counts go into `tally`.
pub fn doc_tests(
    asked: &Invocation,
    artifacts: &[Artifact],
    members: u32,
    speaking: Speaking<'_>,
    err: &mut dyn Write, // dyn: the CLI's output writer, a few writes per doc test run.
    programs: &mut impl Programs,
    tally: &mut Tally,
) -> io::Result<i32> {
    let (_, docs) = executables(artifacts);
    if docs.is_empty() {
        return Ok(0);
    }
    if !speaking.quiet {
        let mut said = String::new(); // alloc: one note, once a run with doc tests
        say::note_into(&mut said, DOC_TESTS_NOT_KEPT, speaking.color);
        err.write_all(said.as_bytes())?;
    }
    let spec_flag = needs_spec_flag(asked, members);
    let mut failed = Vec::new_in(Global);
    if let Some(code) = docs_ran(&docs, asked, spec_flag, speaking, err, programs, tally, &mut failed)? {
        return Ok(code);
    }
    if failed.is_empty() {
        return Ok(0);
    }
    let said = failed_summary(&failed, &[]);
    err.write_all(say::error(&said, speaking.color).as_bytes())?;
    Ok(Exit::Failed.code())
}

/// Runs each of `docs` as cargo's `Doc-tests`, each failure pushed to `failed`; `Some(code)`
/// when `--fail-fast` stops at one.
fn docs_ran(
    docs: &[&Artifact],
    asked: &Invocation,
    spec_flag: bool,
    speaking: Speaking<'_>,
    err: &mut dyn Write, // dyn: the CLI's output writer, a few writes per doc test run.
    programs: &mut impl Programs,
    tally: &mut Tally,
    failed: &mut Vec<Failed>,
) -> io::Result<Option<i32>> {
    for doc in docs {
        let exec = exec_of(doc, asked, speaking);
        let shown = exec.shown_at(speaking.verbose);
        speaking.status(err, "Doc-tests", &doc.target.name)?;
        if speaking.verbose > 0 {
            speaking.status(err, "Running", &shown)?;
        }
        if let Some(failure) =
            ran(doc, Kind::Doctest, &exec, &shown, asked, spec_flag, speaking, err, programs, tally)?
        {
            failed.push(failure);
            if asked.flags.fail_fast {
                return Ok(Some(failed[0].code.unwrap_or(Exit::Failed.code())));
            }
        }
    }
    Ok(None)
}

/// The end of a run that failed: each target that failed, with what reruns it, then each test
/// executable that never built, with the failure that held it back.
fn failed_summary(failed: &[Failed], unbuilt: &[Unbuilt]) -> String {
    let mut said = String::new(); // alloc: once a failed run
    if !failed.is_empty() {
        let _ = match failed.len() {
            1 => write!(said, "1 target failed:"),
            n => write!(said, "{n} targets failed:"),
        };
        for failure in failed {
            said.push_str("\n    `");
            said.push_str(&failure.rerun);
            said.push('`');
        }
    }
    if !unbuilt.is_empty() {
        if !said.is_empty() {
            said.push('\n');
        }
        let _ = match unbuilt.len() {
            1 => write!(said, "1 test target never built, so never ran:"),
            n => write!(said, "{n} test targets never built, so never ran:"),
        };
        for it in unbuilt {
            let _ = match it.behind == it.unit {
                true => write!(said, "\n    {}: {}", it.unit, it.why),
                false => write!(said, "\n    {}: behind {} ({})", it.unit, it.behind, it.why),
            };
        }
    }
    said
}

/// A test executable as cargo runs it: in its package's directory, with the filter and what
/// follows `--`, and libtest's `--quiet` under `-q`.
fn exec_of(artifact: &Artifact, asked: &Invocation, speaking: Speaking<'_>) -> Exec {
    let mut args = asked.test_args();
    if speaking.quiet && artifact.harness {
        args.push(OsString::from("--quiet"));
    }
    let executable = PathBuf::from(artifact.executable.as_deref().unwrap_or(&artifact.path));
    Exec::of(artifact, executable, args, package_root(artifact))
}

/// Runs one, `shown` as the command; what failed, said as cargo's `report_test_error` says it.
#[expect(
    clippy::too_many_arguments,
    reason = "one run of a test, and everything cargo's report of it names"
)]
fn ran(
    artifact: &Artifact,
    kind: Kind,
    exec: &Exec,
    shown: &str,
    asked: &Invocation,
    spec_flag: bool,
    speaking: Speaking<'_>,
    err: &mut dyn Write, // dyn: the CLI's output writer, a few writes per test executable run.
    programs: &mut impl Programs,
    tally: &mut Tally,
) -> io::Result<Option<Failed>> {
    let rerun = rerun(artifact, kind, spec_flag);
    // One that never started is a failure of the run, counted as one.
    let never_ran = Tally { binaries: 1, failed: 1, ..Tally::default() };
    let what = format!("{}, to rerun pass `{rerun}`", kind.failed());
    let never = |error: String| {
        caused(
            &what,
            &[format!("could not execute process {shown} (never executed)"), error],
            speaking.color,
        )
    };
    // A doctest's programs run under its runner, each a process of its own, never on the
    // daemon's code memory; a test executable starts as the daemon launches it.
    let started = match kind {
        Kind::Doctest => Ok(exec.clone()), // clone: once per doctest run, its command
        Kind::Test | Kind::Bench => programs.started(artifact, exec),
    };
    let started = match started {
        Ok(it) => it,
        Err(error) => {
            err.write_all(never(error).as_bytes())?;
            tally.add(&never_ran);
            return Ok(Some(Failed { rerun, code: None }));
        }
    };
    let libtest = artifact.harness && kind != Kind::Doctest;
    let Ran { ended, counts } = match programs.run(&started, libtest) {
        Ok(ran) => ran,
        Err(error) => {
            err.write_all(never(error.to_string()).as_bytes())?;
            tally.add(&never_ran);
            return Ok(Some(Failed { rerun, code: None }));
        }
    };
    tally.add(&counted(counts, matches!(ended, say::Ended::Code(0)), libtest));
    let code = match ended {
        say::Ended::Code(0) => return Ok(None),
        say::Ended::Code(code) => Some(code),
        say::Ended::Signal(_) => None,
    };
    if code == Some(Exit::Failed.code()) {
        err.write_all(say::error(&what, speaking.color).as_bytes())?;
        return Ok(Some(Failed { rerun, code }));
    }
    let cause = format!("process didn't exit successfully: {shown} ({})", say::ended(ended));
    err.write_all(caused(&what, &[cause], speaking.color).as_bytes())?;
    let nocapture = asked.program.iter().any(|it| it == "--nocapture" || it == "--no-capture");
    if code.is_some() && artifact.harness && !nocapture && !speaking.quiet {
        let mut said = String::new();
        say::note_into(
            &mut said,
            "test exited abnormally; to see the full output pass --no-capture to the harness.",
            speaking.color,
        );
        err.write_all(said.as_bytes())?;
    }
    Ok(Some(Failed { rerun, code }))
}

/// One executable's counts in the run's tally. With result lines, their sum; one that ended
/// badly with no failing test among them is itself a failure. Without any: libtest's harness
/// always prints one when it runs tests, so a harness executable that printed none ran none
/// (`--list`, `--help`), and any other executable is one test whose exit status is its verdict.
pub fn counted(counts: Option<Tally>, ok: bool, libtest: bool) -> Tally {
    let mut one = match counts {
        Some(counts) => counts,
        None if libtest => Tally::default(),
        None => Tally { passed: u32::from(ok), ..Tally::default() },
    };
    one.binaries = 1;
    if !ok && one.failed == 0 {
        one.failed = 1;
    }
    one
}

/// libtest's count of a suite that ended, from the line it says it in: the pretty and terse
/// formats' `test result: ok. 7 passed; 1 failed; 1 ignored; 0 measured; 2 filtered out; ...`
/// (the `ok` coloured or not) and the JSON format's `{ "type": "suite", "event": "ok", ... }`.
/// `None` for any other line, and for a result line without every count.
pub fn counts_of(line: &str) -> Option<Tally> {
    let line = line.trim();
    if line.starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value.get("type")?.as_str()? != "suite"
            || !matches!(value.get("event")?.as_str()?, "ok" | "failed")
        {
            return None;
        }
        let count = |name: &str| {
            let count = value.get(name)?.as_u64()?;
            Some(u32::try_from(count).unwrap_or(u32::MAX))
        };
        return Some(Tally {
            passed: count("passed")?,
            failed: count("failed")?,
            ignored: count("ignored")?,
            measured: count("measured")?,
            filtered_out: count("filtered_out")?,
            ..Tally::default()
        });
    }
    let rest = line.strip_prefix("test result: ")?;
    // The number just before `label`.
    let count = |label: &str| {
        let before = &rest[..rest.find(label)?];
        let digits = before.bytes().rev().take_while(u8::is_ascii_digit).count();
        before[before.len() - digits..].parse::<u32>().ok()
    };
    Some(Tally {
        // ` passed;`, or blaze's ` passed (38 cached);`.
        passed: count(" passed")?,
        cached: count(" cached);").unwrap_or(0),
        failed: count(" failed;")?,
        ignored: count(" ignored;")?,
        measured: count(" measured;")?,
        filtered_out: count(" filtered out")?,
        ..Tally::default()
    })
}

/// The longest line read whole: a result line is far shorter, and a longer line is none.
const LINE: usize = 4096;

/// Finds the result lines ([`counts_of`]) in a test executable's stdout as it comes, in
/// chunks that end anywhere.
#[derive(Default)]
pub struct Scan {
    line: Vec<u8>,
    /// The line under way is past [`LINE`]: it is no result line, and is not kept.
    long: bool,
}

impl Scan {
    pub fn feed(&mut self, bytes: &[u8], found: &mut impl FnMut(Tally)) {
        for &byte in bytes {
            if byte == b'\n' {
                self.end(found);
            } else if self.line.len() < LINE {
                self.line.push(byte); // alloc: one line's bytes, at most `LINE`, reused
            } else {
                self.long = true;
            }
        }
    }

    /// The line under way ends: at a newline, and at the end of the output.
    pub fn end(&mut self, found: &mut impl FnMut(Tally)) {
        if !self.long
            && let Ok(line) = std::str::from_utf8(&self.line)
            && let Some(tally) = counts_of(line)
        {
            found(tally);
        }
        self.line.clear();
        self.long = false;
    }
}

/// What a test executable is run with to list its tests, one a line as `name: test`.
pub const LIST: [&str; 3] = ["--list", "--format", "terse"];

/// The names in libtest's terse listing: each `name: test` and `name: benchmark` line.
pub fn listed(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|it| it.strip_suffix(": test").or_else(|| it.strip_suffix(": benchmark")))
        .map(str::to_owned)
        .collect() // alloc: on a refusal's path, once
}

/// The tests that exist, for a refusal's help: the names listed, and for each executable
/// whose listing failed, why.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Names {
    pub names: Vec<String>,
    pub unlisted: Vec<String>,
}

/// The tests of every libtest executable among `artifacts`, each asked for its list.
pub fn names_of(artifacts: &[Artifact], programs: &mut impl Programs) -> Names {
    let (tests, _) = executables(artifacts);
    let mut names = Names::default();
    for test in tests.into_iter().filter(|it| it.harness) {
        let executable = PathBuf::from(test.executable.as_deref().unwrap_or(&test.path));
        let exec = Exec::of(test, executable, LIST.map(OsString::from), package_root(test));
        let listed = programs
            .started(test, &exec)
            .and_then(|exec| programs.names(&exec).map_err(|error| error.to_string()));
        match listed {
            Ok(listed) => names.names.extend(listed),
            Err(why) => names.unlisted.push(format!("{}: {why}", exec.shown())), // alloc: as above
        }
    }
    names
}

/// libtest's options that take a value as their next word: that word is no name filter.
const VALUED: [&str; 7] =
    ["--skip", "--test-threads", "--format", "--color", "--logfile", "--shuffle-seed", "-Z"];

/// What a `test` or `bench` command line selects, as far as the rule for a run of none
/// ([`refusal`]) reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The command as typed, for the refusal to name: `blaze test -p calc adds -- --exact`.
    pub typed: String,
    /// The name filters: `TESTNAME`, and each bare word after `--`.
    pub filters: Vec<String>,
    /// Nothing is meant to run: `--no-run`, libtest's `--list` and `--help`.
    pub lists: bool,
    /// `--no-tests pass`: a run of none is a pass.
    pub pass: bool,
}

impl Selection {
    pub fn of(asked: &Invocation) -> Selection {
        let mut typed = format!("blaze {}", asked.command().name()); // alloc: once a command
        for word in &asked.args {
            typed.push(' ');
            typed.push_str(word);
        }
        if !asked.program.is_empty() {
            typed.push_str(" --");
        }
        let mut filters: Vec<String> = asked.filter.iter().cloned().collect(); // alloc: as above
        let mut lists = asked.flags.no_run;
        let mut words = asked.program.iter().map(|it| it.to_string_lossy());
        while let Some(word) = words.next() {
            typed.push(' ');
            typed.push_str(&word);
            if VALUED.contains(&&*word) {
                if let Some(value) = words.next() {
                    typed.push(' ');
                    typed.push_str(&value);
                }
                continue;
            }
            lists |= matches!(&*word, "--list" | "-h" | "--help");
            if !word.starts_with('-') {
                filters.push(word.into_owned()); // alloc: as above
            }
        }
        Selection { typed, filters, lists, pass: asked.flags.no_tests_pass }
    }
}

/// Why a run in which nothing failed is refused: what it selected and ran none of, then the
/// lines of help under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub what: String,
    pub help: Vec<String>,
}

/// The few words a refusal is said by where a line has room for no more: the `why=` of the
/// `blaze: tests` line and of the JSON summary.
pub const NO_TEST_RAN: &str = "no test ran";

/// How many of the tests that exist a refusal names as nearest its filter.
const NEAREST: usize = 3;

/// The rule for a run of none (`useful-not-parity`, where cargo and libtest end 0): a `test`
/// or `bench` that built, in which nothing failed, and that ran no test is refused, naming
/// what it selected and the nearest tests that exist. Not refused: a command that is meant to
/// run nothing ([`Selection::lists`]) and `--no-tests pass`. A cached test is a pass whose
/// kept run stands ([`Tally::cached`]), so a run whose tests were all cached is not one of
/// none. `names` is asked only when the run is refused.
pub fn refusal(selection: &Selection, tally: &Tally, names: impl FnOnce() -> Names) -> Option<Refusal> {
    if selection.lists || selection.pass {
        return None;
    }
    if tally.ran() > 0 || tally.cached > 0 || tally.failed > 0 || tally.unbuilt > 0 {
        return None;
    }
    let typed = &selection.typed;
    let executables = match tally.binaries {
        1 => "1 test executable".to_owned(), // alloc: a refusal's text, once
        n => format!("{n} test executables"),
    };
    let mut help = Vec::new(); // alloc: as above
    let what = if tally.ignored > 0 {
        help.push(
            "`-- --ignored` runs the ignored ones, `-- --include-ignored` them and the rest".to_owned(),
        );
        format!(
            "{NO_TEST_RAN}: every one of the {} `{typed}` selected is ignored (`#[ignore]`)",
            tally.ignored
        )
    } else if tally.filtered_out > 0 {
        let names = names();
        let near = nearest(&selection.filters, &names.names);
        if !near.is_empty() {
            help.push(format!("the nearest that exist: {}", near.join(", ")));
        } else if !names.names.is_empty() {
            help.push(format!(
                "none of the {} that exist is near it; `blaze test -- --list` lists them",
                names.names.len()
            ));
        }
        for unlisted in names.unlisted {
            help.push(format!("could not be listed: {unlisted}"));
        }
        format!(
            "{NO_TEST_RAN}: `{typed}` matched none of the {} tests in {executables}",
            tally.filtered_out
        )
    } else if tally.binaries == 0 {
        format!("{NO_TEST_RAN}: `{typed}` selected no test executable")
    } else {
        let hold = if tally.binaries == 1 { "holds" } else { "hold" };
        format!("{NO_TEST_RAN}: `{typed}` selected {executables}, which {hold} no test")
    };
    help.push("a run of no test is refused; `--no-tests pass` allows one, as cargo does".to_owned());
    Some(Refusal { what, help })
}

/// The tests nearest any of `filters`, nearest first, at most [`NEAREST`]: by rustc's edit
/// distance (`stdx::edit_distance`, the tree's one "did you mean") between a filter and a
/// test's whole path or its last segment, within a third of the filter's length.
fn nearest<'a>(filters: &[String], names: &'a [String]) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &str)> = Vec::new(); // alloc: on a refusal's path, once
    for name in names {
        let last = name.rsplit("::").next().unwrap_or(name);
        let score = filters
            .iter()
            .filter_map(|filter| {
                let limit = (filter.chars().count() / 3).max(1);
                let whole = stdx::edit_distance::edit_distance_with_substrings(filter, name, limit);
                let tail = stdx::edit_distance::edit_distance_with_substrings(filter, last, limit);
                whole.into_iter().chain(tail).min()
            })
            .min();
        if let Some(score) = score {
            scored.push((score, name.as_str()));
        }
    }
    scored.sort_unstable();
    scored.dedup();
    scored.into_iter().take(NEAREST).map(|(_, name)| name).collect()
}

/// The counts of the results the daemon sent: one executable for each index it numbered. A
/// cached test is a pass, and counted in [`Tally::cached`] too.
pub fn tally_of(results: &[TestResult]) -> Tally {
    let mut tally = Tally::default();
    let mut executables: Vec<u32> = results.iter().map(|it| it.executable).collect(); // alloc: once a request
    executables.sort_unstable();
    executables.dedup();
    tally.binaries = u32::try_from(executables.len()).unwrap_or(u32::MAX);
    for result in results {
        match result.outcome {
            TestOutcome::Passed => tally.passed += 1,
            TestOutcome::Failed => tally.failed += 1,
            TestOutcome::Ignored => tally.ignored += 1,
        }
        if result.cached.is_some() {
            tally.cached += 1;
        }
    }
    tally
}

/// The end of a `test` or `bench` whose tests ran with exit code `code` (`built`: the build
/// before them passed): the rule for a run of none ([`refusal`]), then the run's counts, as
/// one `blaze: tests` line on stderr (`blaze_screen::tests_line`; not under `-q`, which says
/// errors only) and with a JSON `--message-format` one `blaze-test-summary` line on stdout.
/// The command's exit code: `code`, or [`Exit::NoTests`] for a run refused. A command that is
/// meant to run nothing ([`Selection::lists`]) says nothing here.
#[expect(clippy::too_many_arguments, reason = "the run's end, and everything it is said from and to")]
pub fn conclude(
    selection: &Selection,
    tally: &Tally,
    code: i32,
    built: bool,
    took: Duration,
    names: impl FnOnce() -> Names,
    speaking: Speaking<'_>,
    out: &mut dyn Write, // dyn: the CLI's output writers, written once a run
    err: &mut dyn Write,
) -> io::Result<i32> {
    if selection.lists {
        return Ok(code);
    }
    let refused = match code == 0 && built {
        true => refusal(selection, tally, names),
        false => None,
    };
    let mut said = String::new(); // alloc: the run's last lines, once
    let code = match &refused {
        Some(refused) => {
            say::error_into(&mut said, &refused.what, speaking.color);
            for help in &refused.help {
                say::note_into(&mut said, help, speaking.color);
            }
            Exit::NoTests.code()
        }
        None => code,
    };
    let ok = code == 0 && built;
    let why = refused.is_some().then_some(NO_TEST_RAN);
    let took_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
    if !speaking.quiet {
        said.push_str(&blaze_screen::tests_line(tally, ok, why, took_ms));
        said.push('\n');
    }
    err.write_all(said.as_bytes())?;
    if speaking.json {
        said.clear();
        super::json::test_summary(&mut said, tally, ok, why, took_ms);
        said.push('\n');
        out.write_all(said.as_bytes())?;
        out.flush()?;
    }
    Ok(code)
}

/// The most test executables one request's results index: the daemon's indexes are dense from
/// 0, so one at or past this is its mistake, refused before a table that long is made.
const EXECUTABLES: u32 = 1 << 20;

/// No executable seen at this index yet.
const UNSEEN: u32 = u32::MAX;

/// The results the daemon sent, by test executable in the order they came, as libtest's pretty
/// format prints them (with a JSON `--message-format`, a `blaze-test-result` line each);
/// whether every one that ran passed. A cached test, whose kept pass stands, is marked
/// `(cached)` with the time its kept run took, and a pass not kept says why (blaze's words, not
/// libtest's). Last, on stderr, the tests that ran outside the sandbox (docs/cargo.md: their
/// passes are never kept).
// dyn: the CLI's output writers, written once per test result printed.
pub fn from_daemon(
    results: &[TestResult],
    speaking: Speaking<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<bool> {
    let Some(any) = results.first() else {
        out.flush()?;
        return Ok(true);
    };
    // A JSON `--message-format`: each result whole, as the daemon sent it, a
    // `blaze-test-result` line on stdout among the build's messages (`json::test_result`).
    if speaking.json {
        // alloc: one line's buffer, reused, once a request
        let mut text = String::new();
        let mut passed_all = true;
        for result in results {
            passed_all &= result.outcome != TestOutcome::Failed;
            text.clear();
            super::json::test_result(&mut text, result);
            text.push('\n');
            out.write_all(text.as_bytes())?;
        }
        out.flush()?;
        if !speaking.quiet {
            text.clear();
            outside(&mut text, results, speaking.color);
            err.write_all(text.as_bytes())?;
        }
        return Ok(passed_all);
    }
    // Each executable's place in the order executables first came, found by the daemon's dense
    // index (`TestResult::executable`) with no hash.
    // alloc: these, `starts`, `next`, `grouped` and `text`, once a request.
    let mut place: Vec<u32> = Vec::with_capacity(16);
    let mut binaries: Vec<&str> = Vec::with_capacity(16);
    let mut of: Vec<u32> = Vec::with_capacity(results.len());
    for result in results {
        if result.executable >= EXECUTABLES {
            let what = format!(
                "the daemon numbered the test executable {} as {}, past the {EXECUTABLES} a request's indexes stay under",
                result.binary, result.executable
            );
            err.write_all(say::error(&what, speaking.color).as_bytes())?;
            return Ok(false);
        }
        let at = result.executable as usize;
        if place.len() <= at {
            place.resize(at + 1, UNSEEN);
        }
        if place[at] == UNSEEN {
            place[at] = binaries.len() as u32;
            binaries.push(result.binary.as_str());
        }
        of.push(place[at]);
    }
    // A counting sort, stable: executable `b`'s results are `grouped[starts[b]..starts[b + 1]]`.
    let mut starts = vec![0u32; binaries.len() + 1];
    for &index in &of {
        starts[index as usize + 1] += 1;
    }
    for b in 0..binaries.len() {
        starts[b + 1] += starts[b];
    }
    let mut next = starts.clone();
    let mut grouped: Vec<&TestResult> = vec![any; results.len()];
    for (result, &index) in results.iter().zip(&of) {
        let at = &mut next[index as usize];
        grouped[*at as usize] = result;
        *at += 1;
    }
    let mut passed_all = true;
    let mut text = String::new();
    for (b, binary) in binaries.iter().enumerate() {
        speaking.status(err, "Running", &say::shown(Path::new(binary), speaking.cwd))?;
        text.clear();
        passed_all &= libtest(
            &mut text,
            &grouped[starts[b] as usize..starts[b + 1] as usize],
            speaking.color,
        );
        out.write_all(text.as_bytes())?;
    }
    out.flush()?;
    if !speaking.quiet {
        text.clear();
        outside(&mut text, results, speaking.color);
        err.write_all(text.as_bytes())?;
    }
    Ok(passed_all)
}

/// The command's summary of the tests that ran outside the sandbox (`TestResult::outside`), as
/// cargo's `Shell::note` writes a line, into `text`; nothing when none did. Such a test is not
/// hermetic: it runs every time and its pass is never kept (docs/cargo.md, "Sandboxed tests").
pub fn outside(text: &mut String, results: &[TestResult], color: bool) {
    let count = results.iter().filter(|it| it.outside).count();
    if count == 0 {
        return;
    }
    say::leveled_with(text, say::GOOD, "note", color, |text| {
        // Writing to a String cannot fail.
        let _ = match count {
            1 => write!(
                text,
                "1 test ran outside the sandbox, so it runs every time and its pass is never kept: "
            ),
            n => write!(
                text,
                "{n} tests ran outside the sandbox, so they run every time and their passes are never kept: "
            ),
        };
        for (at, result) in results.iter().filter(|it| it.outside).enumerate() {
            if at > 0 {
                text.push_str(", ");
            }
            text.push_str(&result.name);
        }
    });
}

/// One executable's results as libtest's `PrettyFormatter` writes them, into `text`; whether
/// none failed. A cached test's line is `test NAME ... ok <0.012s> (cached)`, with the time its
/// kept run took; a pass not kept ends `(not kept: WHY)`; the summary counts the cached among
/// the passed: `41 passed (38 cached)`.
pub fn libtest(text: &mut String, results: &[&TestResult], color: bool) -> bool {
    let paint = |text: &mut String, style: &str, said: &str| {
        if color {
            text.push_str(style);
        }
        text.push_str(said);
        if color {
            text.push_str("\x1b[0m");
        }
    };
    let (mut passed, mut failed, mut ignored, mut cached, mut millis) =
        (0usize, 0usize, 0usize, 0usize, 0u64);
    let mut names = 0;
    for result in results {
        match result.outcome {
            TestOutcome::Passed => passed += 1,
            TestOutcome::Failed => failed += 1,
            TestOutcome::Ignored => ignored += 1,
        }
        cached += usize::from(result.cached.is_some());
        millis += result.millis;
        names += result.name.len();
    }
    let noun = if results.len() == 1 { "test" } else { "tests" };
    text.reserve(names + results.len() * 48 + 160);
    // Writing to a String cannot fail.
    let _ = write!(text, "\nrunning {} {noun}\n", results.len());
    for result in results {
        let (style, said) = match result.outcome {
            TestOutcome::Passed => ("\x1b[32m", "ok"),
            TestOutcome::Failed => ("\x1b[31m", "FAILED"),
            TestOutcome::Ignored => ("\x1b[33m", "ignored"),
        };
        text.push_str("test ");
        text.push_str(&result.name);
        text.push_str(" ... ");
        paint(&mut *text, style, said);
        if let Some(kept) = &result.cached {
            let _ = write!(text, " <{:.3}s> (cached)", kept.millis as f64 / 1000.0);
        }
        if let Some(not) = &result.not_kept {
            text.push_str(" (not kept: ");
            text.push_str(&not.why);
            text.push(')');
        }
        text.push('\n');
    }
    if failed > 0 {
        text.push_str("\nfailures:\n");
        let failures = || results.iter().filter(|it| it.outcome == TestOutcome::Failed);
        if failures().any(|it| !it.output.is_empty()) {
            text.push('\n');
            for failure in failures().filter(|it| !it.output.is_empty()) {
                text.push_str("---- ");
                text.push_str(&failure.name);
                text.push_str(" stdout ----\n");
                text.push_str(&String::from_utf8_lossy(&failure.output));
                text.push('\n');
            }
        }
        text.push_str("\nfailures:\n");
        let mut names: Vec<&str> = failures().map(|it| it.name.as_str()).collect();
        names.sort_unstable();
        for name in names {
            text.push_str("    ");
            text.push_str(name);
            text.push('\n');
        }
    }
    let ok = failed == 0;
    text.push_str("\ntest result: ");
    match ok {
        true => paint(&mut *text, "\x1b[32m", "ok"),
        false => paint(&mut *text, "\x1b[31m", "FAILED"),
    }
    let _ = write!(text, ". {passed} passed");
    if cached > 0 {
        let _ = write!(text, " ({cached} cached)");
    }
    let _ = write!(
        text,
        "; {failed} failed; {ignored} ignored; 0 measured; 0 filtered out; finished in {:.2}s\n\n",
        millis as f64 / 1000.0
    );
    ok
}

#[cfg(test)]
pub(crate) mod tests;
