//! `test` and `bench` after the build, as cargo's `ops::run_tests` and `run_benches` run them
//! (`ops/cargo_test.rs`): each test executable in cargo's order, then the doc tests, stopping
//! at the first that fails unless `--no-fail-fast`; `--no-run` lists them. And blaze's
//! `--reached` (an extension, opt-in): the daemon's [`TestResult`]s in libtest's format.

use std::{
    ffi::OsString,
    fmt::Write as _,
    io::{self, Write},
    path::{Path, PathBuf},
};

use blaze_api::{Artifact, ArtifactKind, TargetKind, TestOutcome, TestResult};

use super::{
    FAILED,
    args::{Command, Invocation},
    run::{Exec, caused},
    say,
};

/// What runs a test executable: the process itself, or the tests' fake.
pub trait Programs {
    fn run(&mut self, exec: &Exec) -> io::Result<say::Ended>;
}

/// Runs each program on the terminal, its output inherited.
pub struct Spawned;

impl Programs for Spawned {
    fn run(&mut self, exec: &Exec) -> io::Result<say::Ended> {
        exec.command().status().map(say::Ended::of)
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
    let mut docs: Vec<&Artifact> =
        artifacts.iter().filter(|it| it.kind == ArtifactKind::Doctest && it.executable.is_some()).collect();
    tests.sort_by_cached_key(|it| order(it));
    docs.sort_by_cached_key(|it| order(it));
    (tests, docs)
}

/// cargo's order of units: by package (name, version, id), kind, then target name; each
/// package id read once per executable, not once per comparison.
fn order(artifact: &Artifact) -> (&str, &str, &str, u8, &str) {
    let id = artifact.package.as_str();
    (say::package_name(id), say::package_version(id), id, rank(artifact), artifact.target.name.as_str())
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
    fn running(&self, err: &mut dyn Write, word: &str, artifact: &Artifact, kind: Kind, shown: &str) -> io::Result<()> {
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

/// `test` or `bench` once the build placed `artifacts`, of a workspace of `members`: cargo's
/// exit code.
pub fn run(
    asked: &Invocation,
    artifacts: &[Artifact],
    members: u32,
    speaking: Speaking<'_>,
    err: &mut dyn Write, // dyn: the CLI's output writer, a few writes per test executable run.
    programs: &mut impl Programs,
) -> io::Result<i32> {
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
        if let Some(failure) = ran(test, unit_kind, &exec, &shown, asked, spec_flag, speaking, err, programs)? {
            failed.push(failure);
            if !asked.flags.no_fail_fast {
                return Ok(failed[0].code.unwrap_or(FAILED));
            }
        }
    }
    if !bench {
        for doc in &docs {
            let exec = exec_of(doc, asked, speaking);
            let shown = exec.shown_at(speaking.verbose);
            speaking.status(err, "Doc-tests", &doc.target.name)?;
            if speaking.verbose > 0 {
                speaking.status(err, "Running", &shown)?;
            }
            if let Some(failure) = ran(doc, Kind::Doctest, &exec, &shown, asked, spec_flag, speaking, err, programs)? {
                failed.push(failure);
                if !asked.flags.no_fail_fast {
                    return Ok(failed[0].code.unwrap_or(FAILED));
                }
            }
        }
    }
    // cargo's `no_fail_fast_err`: under `--no-fail-fast` any failure is its own error, code 101.
    if failed.is_empty() {
        return Ok(0);
    }
    let mut said = String::new();
    let _ = match failed.len() {
        1 => write!(said, "1 target failed:"),
        n => write!(said, "{n} targets failed:"),
    };
    for failure in &failed {
        said.push_str("\n    `");
        said.push_str(&failure.rerun);
        said.push('`');
    }
    err.write_all(say::error(&said, speaking.color).as_bytes())?;
    Ok(FAILED)
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
#[expect(clippy::too_many_arguments, reason = "one run of a test, and everything cargo's report of it names")]
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
) -> io::Result<Option<Failed>> {
    let rerun = rerun(artifact, kind, spec_flag);
    let what = format!("{}, to rerun pass `{rerun}`", kind.failed());
    let ended = match programs.run(exec) {
        Ok(ended) => ended,
        Err(error) => {
            let never = format!("could not execute process {shown} (never executed)");
            err.write_all(caused(&what, &[never, error.to_string()], speaking.color).as_bytes())?;
            return Ok(Some(Failed { rerun, code: None }));
        }
    };
    let code = match ended {
        say::Ended::Code(0) => return Ok(None),
        say::Ended::Code(code) => Some(code),
        say::Ended::Signal(_) => None,
    };
    if code == Some(FAILED) {
        err.write_all(say::error(&what, speaking.color).as_bytes())?;
        return Ok(Some(Failed { rerun, code }));
    }
    let cause = format!("process didn't exit successfully: {shown} ({})", say::ended(ended));
    err.write_all(caused(&what, &[cause], speaking.color).as_bytes())?;
    let nocapture = asked.program.iter().any(|it| it == "--nocapture" || it == "--no-capture");
    if code.is_some() && artifact.harness && !nocapture && !speaking.quiet {
        let mut said = String::new();
        say::note_into(&mut said, "test exited abnormally; to see the full output pass --no-capture to the harness.", speaking.color);
        err.write_all(said.as_bytes())?;
    }
    Ok(Some(Failed { rerun, code }))
}

/// The most test executables one request's results index: the daemon's indexes are dense from
/// 0, so one at or past this is its mistake, refused before a table that long is made.
const EXECUTABLES: u32 = 1 << 20;

/// No executable seen at this index yet.
const UNSEEN: u32 = u32::MAX;

/// `blaze test --reached`: the results the daemon sent, by test executable in the order they
/// came, as libtest's pretty format prints them; whether every one that ran passed. A test the
/// edit did not reach is not run, and is named as such (blaze's word, not libtest's). Last, on
/// stderr, the tests that ran outside the sandbox (docs/cargo.md: their passes are never kept).
// dyn: the CLI's output writers, written once per test result printed.
pub fn reached(results: &[TestResult], speaking: Speaking<'_>, out: &mut dyn Write, err: &mut dyn Write) -> io::Result<bool> {
    let Some(any) = results.first() else {
        out.flush()?;
        return Ok(true);
    };
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
        passed_all &= libtest(&mut text, &grouped[starts[b] as usize..starts[b + 1] as usize], speaking.color);
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
            1 => write!(text, "1 test ran outside the sandbox, so it runs every time and its pass is never kept: "),
            n => write!(text, "{n} tests ran outside the sandbox, so they run every time and their passes are never kept: "),
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
/// none failed.
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
    let (mut passed, mut failed, mut ignored, mut unreached, mut millis) = (0usize, 0usize, 0usize, 0usize, 0u64);
    let mut names = 0;
    for result in results {
        match result.outcome {
            TestOutcome::Passed => passed += 1,
            TestOutcome::Failed => failed += 1,
            TestOutcome::Ignored => ignored += 1,
            TestOutcome::Unreached => unreached += 1,
        }
        if result.outcome != TestOutcome::Unreached {
            millis += result.millis;
            names += result.name.len();
        }
    }
    let ran = results.len() - unreached;
    let noun = if ran == 1 { "test" } else { "tests" };
    text.reserve(names + ran * 32 + 160);
    // Writing to a String cannot fail.
    let _ = write!(text, "\nrunning {ran} {noun}\n");
    for result in results {
        let (style, said) = match result.outcome {
            TestOutcome::Passed => ("\x1b[32m", "ok"),
            TestOutcome::Failed => ("\x1b[31m", "FAILED"),
            TestOutcome::Ignored => ("\x1b[33m", "ignored"),
            TestOutcome::Unreached => continue,
        };
        text.push_str("test ");
        text.push_str(&result.name);
        text.push_str(" ... ");
        paint(&mut *text, style, said);
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
    let _ = write!(text, ". {passed} passed; {failed} failed; {ignored} ignored; 0 measured; 0 filtered out");
    if unreached > 0 {
        let _ = write!(text, "; {unreached} unreached");
    }
    let _ = write!(text, "; finished in {:.2}s\n\n", millis as f64 / 1000.0);
    ok
}

#[cfg(test)]
pub(crate) mod tests;
