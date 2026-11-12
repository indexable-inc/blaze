use std::{ffi::OsString, io, path::Path};

use blaze_api::{Artifact, ArtifactKind, TestOutcome, TestResult, Unbuilt};
use blaze_screen::Tally;

use super::*;
use crate::cli::{
    args::{Command, Invocation, command_line},
    run::{Exec, tests::artifact},
    say::Ended,
};

/// Ends each executable as `ends` says by its file name, and the rest with 0. Each says it
/// ran one test, which its exit status is the verdict of, unless `counts` is what every one
/// says it ran; `names` is what each lists.
#[derive(Default)]
pub(crate) struct Fake {
    pub ends: Vec<(&'static str, Ended)>,
    pub ran: Vec<Exec>,
    pub counts: Option<Tally>,
    pub names: Vec<&'static str>,
    pub listed: Vec<Exec>,
}

impl Programs for Fake {
    fn run(&mut self, exec: &Exec, _harness: bool) -> io::Result<Ran> {
        self.ran.push(exec.clone());
        let name = exec.program.file_name().unwrap().to_str().unwrap().to_owned();
        let ended =
            self.ends.iter().find(|(it, _)| *it == name).map_or(Ended::Code(0), |(_, ended)| *ended);
        let ok = ended.success();
        let one = Tally { passed: u32::from(ok), failed: u32::from(!ok), ..Tally::default() };
        Ok(Ran { ended, counts: Some(self.counts.unwrap_or(one)) })
    }

    fn names(&mut self, exec: &Exec) -> io::Result<Vec<String>> {
        self.listed.push(exec.clone());
        Ok(self.names.iter().map(|it| (*it).to_owned()).collect())
    }

    /// No daemon: every program runs its file.
    fn launch(&mut self, _: &Artifact, _: &mut Exec) -> Result<(), String> {
        Ok(())
    }

    /// No daemon: every test executable starts as its command says.
    fn started(&mut self, _: &Artifact, exec: &Exec) -> Result<Exec, String> {
        Ok(exec.clone())
    }
}

/// A package `foo` with a lib, a bin and a test `x`, each tested, and its doc tests; out of
/// cargo's order.
pub(crate) fn built() -> Vec<Artifact> {
    vec![
        artifact("foo", "test", "x", true, Some("deps/x-4567")),
        artifact("foo", "lib", "foo", false, None),
        Artifact {
            kind: ArtifactKind::Doctest,
            ..artifact("foo", "lib", "foo", true, Some("deps/foo-doctests"))
        },
        artifact("foo", "bin", "foo", true, Some("deps/foo-89ab")),
        artifact("foo", "lib", "foo", true, Some("deps/foo-0123abcd")),
        artifact("foo", "bin", "foo", false, Some("foo")),
    ]
}

fn asked(command: Command, words: &[&str]) -> Invocation {
    command_line(command, words.iter().map(OsString::from).collect()).unwrap()
}

fn speaking() -> Speaking<'static> {
    Speaking { cwd: Path::new("/w"), color: false, quiet: false, verbose: 0, json: false }
}

fn tested(asked: &Invocation, fake: &mut Fake, speaking: Speaking<'_>) -> (i32, String) {
    let mut err = Vec::new();
    let code = run(asked, &built(), &[], 1, speaking, &mut err, fake, &mut Tally::default()).unwrap();
    (code, String::from_utf8(err).unwrap())
}

/// A whole run as the command does it: the executables, then the run's end ([`conclude`]).
/// The exit code, the run's counts, stdout and stderr.
fn concluded(asked: &Invocation, fake: &mut Fake, speaking: Speaking<'_>) -> (i32, Tally, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut tally = Tally::default();
    let built = built();
    let code = run(asked, &built, &[], 1, speaking, &mut err, fake, &mut tally).unwrap();
    let code = conclude(
        &Selection::of(asked),
        &tally,
        code,
        true,
        std::time::Duration::from_millis(12),
        || names_of(&built, fake),
        speaking,
        &mut out,
        &mut err,
    )
    .unwrap();
    (code, tally, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn rerun_names_the_package_as_cargos_needs_spec_flag_does() {
    assert!(!needs_spec_flag(&asked(Command::Test, &[]), 1));
    assert!(needs_spec_flag(&asked(Command::Test, &[]), 2), "a workspace of two");
    assert!(needs_spec_flag(&asked(Command::Test, &["-p", "foo"]), 1));
    assert!(needs_spec_flag(&asked(Command::Test, &["--workspace", "--exclude", "bar"]), 1));

    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let mut err = Vec::new();
    let code = run(
        &asked(Command::Test, &["--fail-fast"]),
        &built(),
        &[],
        3,
        speaking(),
        &mut err,
        &mut fake,
        &mut Tally::default(),
    )
    .unwrap();
    assert_eq!(code, 101);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .ends_with("error: test failed, to rerun pass `-p foo --lib`\n")
    );
}

#[test]
fn executables_in_cargos_order() {
    let built = built();
    let (tests, docs) = executables(&built);
    let shown: Vec<String> =
        tests.iter().map(|it| running(it, Path::new("/w"), Kind::Test)).collect();
    assert_eq!(
        shown,
        [
            "unittests src/lib.rs (target/debug/deps/foo-0123abcd)",
            "unittests src/main.rs (target/debug/deps/foo-89ab)",
            "tests/x.rs (target/debug/deps/x-4567)",
        ]
    );
    assert_eq!(docs.len(), 1);
    assert_eq!(
        running(tests[0], Path::new("/w"), Kind::Bench),
        "benches src/lib.rs (target/debug/deps/foo-0123abcd)"
    );
}

#[test]
fn every_one_passes() {
    let mut fake = Fake::default();
    let asked = asked(Command::Test, &["filt", "--", "--exact"]);
    let (code, err) = tested(&asked, &mut fake, speaking());
    assert_eq!(code, 0);
    assert_eq!(
        err,
        "     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)\n     Running unittests \
         src/main.rs (target/debug/deps/foo-89ab)\n     Running tests/x.rs (target/debug/deps/x-4567)\n   \
         Doc-tests foo\n"
    );
    let first = &fake.ran[0];
    assert_eq!(first.program, Path::new("/w/target/debug/deps/foo-0123abcd"));
    assert_eq!(first.args, [OsString::from("filt"), OsString::from("--exact")]);
    assert_eq!(first.cwd, Path::new("/w/foo"));
    assert_eq!(first.env, [("CARGO_PKG_NAME".to_owned(), "foo".to_owned())]);
    assert_eq!(fake.ran.len(), 4);
}

/// `--fail-fast` stops at the first test executable that fails, as cargo does, with its code.
#[test]
fn fail_fast_stops_at_the_first_failed_test_executable() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--fail-fast"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert_eq!(fake.ran.len(), 1);
    assert_eq!(
        err,
        "     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)\nerror: test failed, to rerun pass `--lib`\n"
    );
}

#[test]
fn no_fail_fast_runs_them_all() {
    let ends = vec![("foo-0123abcd", Ended::Code(101)), ("x-4567", Ended::Code(101))];
    let mut fake = Fake { ends, ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--no-fail-fast"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert_eq!(fake.ran.len(), 4);
    assert!(
        err.contains("error: test failed, to rerun pass `--test x`\n   Doc-tests foo\n"),
        "{err}"
    );
    assert!(err.ends_with("error: 2 targets failed:\n    `--lib`\n    `--test x`\n"), "{err}");

    let mut fake = Fake { ends: vec![("foo-doctests", Ended::Code(101))], ..Fake::default() };
    let (code, err) =
        tested(&asked(Command::Test, &["-p", "foo", "--fail-fast"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert!(err.ends_with("error: doctest failed, to rerun pass `-p foo --doc`\n"), "{err}");
}

/// With no flag, every test executable and the doc tests run past a failed one, and the end
/// lists what failed (Andrew's rule: every error in one pass). `--keep-going` and
/// `--no-fail-fast` are accepted and change nothing.
#[test]
fn by_default_every_test_executable_runs_past_a_failed_one() {
    for words in [&[][..], &["--keep-going"], &["--no-fail-fast"]] {
        let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
        let (code, err) = tested(&asked(Command::Test, words), &mut fake, speaking());
        assert_eq!(code, 101, "{words:?}");
        let ran: Vec<&str> =
            fake.ran.iter().map(|it| it.program.file_name().unwrap().to_str().unwrap()).collect();
        assert_eq!(ran, ["foo-0123abcd", "foo-89ab", "x-4567", "foo-doctests"], "{words:?}: {err}");
        assert!(err.ends_with("error: 1 target failed:\n    `--lib`\n"), "{words:?}: {err}");
    }
}

/// The end of the run names each test executable that never built and the failure that held
/// it back, after the targets that failed; a run whose only failure is a test that never
/// built still fails.
#[test]
fn the_end_lists_each_test_that_never_built_and_what_held_it_back() {
    let unbuilt = [
        Unbuilt {
            unit: "foo v0.1.0 (test \"y\")".into(),
            behind: "foo v0.1.0 (test \"y\")".into(),
            why: "errors in its items".into(),
        },
        Unbuilt {
            unit: "bar v0.1.0 (lib test)".into(),
            behind: "bar v0.1.0 (build script run)".into(),
            why: "the build script failed".into(),
        },
    ];
    let mut fake = Fake { ends: vec![("x-4567", Ended::Code(101))], ..Fake::default() };
    let mut err = Vec::new();
    let mut tally = Tally::default();
    let code = run(
        &asked(Command::Test, &[]),
        &built(),
        &unbuilt,
        1,
        speaking(),
        &mut err,
        &mut fake,
        &mut tally,
    )
    .unwrap();
    assert_eq!((tally.unbuilt, tally.binaries, tally.passed, tally.failed), (2, 4, 3, 1));
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 101);
    assert_eq!(fake.ran.len(), 4, "{err}");
    assert!(
        err.ends_with(
            "error: 1 target failed:\n    `--test x`\n2 test targets never built, so never ran:\n    \
             foo v0.1.0 (test \"y\"): errors in its items\n    \
             bar v0.1.0 (lib test): behind bar v0.1.0 (build script run) (the build script failed)\n"
        ),
        "{err}"
    );

    let mut err = Vec::new();
    let code = run(
        &asked(Command::Test, &[]),
        &built(),
        &unbuilt[1..],
        1,
        speaking(),
        &mut err,
        &mut Fake::default(),
        &mut Tally::default(),
    )
    .unwrap();
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 101, "{err}");
    assert!(err.ends_with("error: 1 test target never built, so never ran:\n    bar v0.1.0 (lib test): behind bar v0.1.0 (build script run) (the build script failed)\n"), "{err}");
}

#[test]
fn no_fail_fast_with_one_failure_is_its_own_error() {
    let mut fake = Fake { ends: vec![("x-4567", Ended::Code(2))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--no-fail-fast"]), &mut fake, speaking());
    assert_eq!(code, 101, "cargo's `no_fail_fast_err` is a plain error, not the test's code");
    assert_eq!(fake.ran.len(), 4);
    assert!(err.ends_with("error: 1 target failed:\n    `--test x`\n"), "{err}");
}

#[test]
fn doc_tests_by_the_targets_own_name_and_command_under_verbose() {
    let doc = Artifact {
        kind: ArtifactKind::Doctest,
        ..artifact("foo", "lib", "my-lib", true, Some("deps/my_lib-doctests"))
    };
    let mut err = Vec::new();
    let code = run(
        &asked(Command::Test, &[]),
        &[doc.clone()],
        &[],
        1,
        speaking(),
        &mut err,
        &mut Fake::default(),
        &mut Tally::default(),
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(err).unwrap(), "   Doc-tests my-lib\n");

    let mut err = Vec::new();
    let verbose = Speaking { verbose: 1, ..speaking() };
    run(
        &asked(Command::Test, &[]),
        &[doc],
        &[],
        1,
        verbose,
        &mut err,
        &mut Fake::default(),
        &mut Tally::default(),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "   Doc-tests my-lib\n     Running `/w/target/debug/deps/my_lib-doctests`\n"
    );
}

#[cfg(unix)]
#[test]
fn extra_verbose_shows_the_variables_cargo_sets() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (_, err) = tested(
        &asked(Command::Test, &["--fail-fast"]),
        &mut fake,
        Speaking { verbose: 2, ..speaking() },
    );
    let shown = "`CARGO_PKG_NAME=foo /w/target/debug/deps/foo-0123abcd`";
    assert!(err.starts_with(&format!("     Running {shown}\n")), "{err}");
    assert!(
        err.contains(&format!("process didn't exit successfully: {shown} (exit status: 3)")),
        "{err}"
    );

    let (_, err) = tested(
        &asked(Command::Test, &[]),
        &mut Fake::default(),
        Speaking { verbose: 1, ..speaking() },
    );
    assert!(err.starts_with("     Running `/w/target/debug/deps/foo-0123abcd`\n"), "{err}");
}

#[test]
fn no_run_under_json_lists_nothing() {
    let (code, err) = tested(
        &asked(Command::Test, &["--no-run"]),
        &mut Fake::default(),
        Speaking { json: true, ..speaking() },
    );
    assert_eq!(code, 0);
    assert_eq!(err, "");
}

#[cfg(unix)]
#[test]
fn quiet_keeps_the_error_and_drops_the_note() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (code, err) = tested(
        &asked(Command::Test, &["--fail-fast"]),
        &mut fake,
        Speaking { quiet: true, ..speaking() },
    );
    assert_eq!(code, 3);
    assert!(err.starts_with("error: test failed, to rerun pass `--lib`\n\nCaused by:\n"), "{err}");
    assert!(!err.contains("note:"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_test_that_ended_otherwise() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--fail-fast"]), &mut fake, speaking());
    assert_eq!(code, 3);
    assert!(
        err.ends_with(
            "error: test failed, to rerun pass `--lib`\n\nCaused by:\n  process didn't exit successfully: \
             `/w/target/debug/deps/foo-0123abcd` (exit status: 3)\nnote: test exited abnormally; to see the \
             full output pass --no-capture to the harness.\n"
        ),
        "{err}"
    );

    let mut fake =
        Fake { ends: vec![("foo-0123abcd", Ended::Signal(libc::SIGSEGV))], ..Fake::default() };
    let (code, err) =
        tested(&asked(Command::Test, &["--fail-fast", "--", "--nocapture"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert!(err.ends_with("(signal: 11, SIGSEGV: invalid memory reference)\n"), "{err}");
}

#[test]
fn no_run_lists_the_executables() {
    let mut fake = Fake::default();
    let (code, err) = tested(&asked(Command::Test, &["--no-run"]), &mut fake, speaking());
    assert_eq!(code, 0);
    assert!(fake.ran.is_empty());
    assert_eq!(
        err,
        "  Executable unittests src/lib.rs (target/debug/deps/foo-0123abcd)\n  Executable unittests \
         src/main.rs (target/debug/deps/foo-89ab)\n  Executable tests/x.rs (target/debug/deps/x-4567)\n"
    );
}

#[test]
fn quiet_and_bench() {
    let mut fake = Fake::default();
    let (code, err) =
        tested(&asked(Command::Bench, &["-q"]), &mut fake, Speaking { quiet: true, ..speaking() });
    assert_eq!(code, 0);
    assert_eq!(err, "");
    // No doc tests for bench; libtest's `--bench`, and `--quiet` to a harness.
    assert_eq!(fake.ran.len(), 3);
    assert_eq!(fake.ran[0].args, [OsString::from("--bench"), OsString::from("--quiet")]);
}

/// A result of `binary`, whose index is the daemon's: `foo`'s unit tests are executable 3 and
/// `x` 1, so the order printed is the order they first came, not the indexes'.
fn result(binary: &str, name: &str, outcome: TestOutcome, output: &str) -> TestResult {
    let millis = if outcome == TestOutcome::Ignored { 0 } else { 5 };
    let executable = if binary.ends_with("x-4567") { 1 } else { 3 };
    TestResult {
        binary: binary.to_owned(),
        name: name.to_owned(),
        outcome,
        output: output.as_bytes().to_vec(),
        millis,
        cached: None,
        outside: false,
        executable,
    }
}

#[test]
fn the_tests_that_ran_outside_named_in_one_note_as_cargos_shell_writes_it() {
    let inside = result("/w/target/debug/deps/foo-0123abcd", "a", TestOutcome::Passed, "");
    let outside =
        TestResult { name: "tests::writes_home".to_owned(), outside: true, ..inside.clone() };
    let mut text = String::new();
    super::outside(&mut text, std::slice::from_ref(&inside), false);
    assert_eq!(text, "", "nothing when every test ran inside");
    super::outside(&mut text, &[inside.clone(), outside.clone()], false);
    assert_eq!(
        text,
        "note: 1 test ran outside the sandbox, so it runs every time and its pass is never kept: tests::writes_home\n"
    );
    text.clear();
    let failed = TestResult {
        name: "tests::dials".to_owned(),
        outcome: TestOutcome::Failed,
        ..outside.clone()
    };
    super::outside(&mut text, &[outside, inside, failed], true);
    assert_eq!(
        text,
        "\x1b[1m\x1b[92mnote\x1b[0m: 2 tests ran outside the sandbox, so they run every time and their passes are \
         never kept: tests::writes_home, tests::dials\n"
    );
}

#[test]
fn an_executable_index_past_any_request_is_refused_naming_it() {
    let far = TestResult {
        executable: u32::MAX - 1,
        ..result("/w/target/debug/deps/foo-0123abcd", "a", TestOutcome::Passed, "")
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert!(!reached(&[far], speaking(), &mut out, &mut err).unwrap());
    let err = String::from_utf8(err).unwrap();
    assert!(err.starts_with("error: the daemon numbered the test executable /w/target/debug/deps/foo-0123abcd as 4294967294"), "{err}");
    assert!(out.is_empty());
}

#[test]
fn reached_results_in_libtests_format() {
    let results = [
        result("/w/target/debug/deps/foo-0123abcd", "a", TestOutcome::Passed, ""),
        result(
            "/w/target/debug/deps/foo-0123abcd",
            "b",
            TestOutcome::Failed,
            "thread 'b' panicked\n",
        ),
        result("/w/target/debug/deps/foo-0123abcd", "c", TestOutcome::Unreached, ""),
        result("/w/target/debug/deps/x-4567", "d", TestOutcome::Ignored, ""),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let passed = reached(&results, speaking(), &mut out, &mut err).unwrap();
    assert!(!passed);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "     Running target/debug/deps/foo-0123abcd\n     Running target/debug/deps/x-4567\n"
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\nthread 'b' \
         panicked\n\n\nfailures:\n    b\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 \
         filtered out; 1 unreached; finished in 0.01s\n\n\nrunning 1 test\ntest d ... ignored\n\ntest result: \
         ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n"
    );
}

#[test]
fn reached_results_of_executables_interleaved_by_their_first_and_in_the_order_they_came() {
    let results = [
        result("/w/target/debug/deps/x-4567", "x1", TestOutcome::Passed, ""),
        result("/w/target/debug/deps/foo-0123abcd", "f1", TestOutcome::Passed, ""),
        result("/w/target/debug/deps/x-4567", "x2", TestOutcome::Passed, ""),
        result("/w/target/debug/deps/foo-0123abcd", "f2", TestOutcome::Passed, ""),
        result("/w/target/debug/deps/foo-0123abcd", "f3", TestOutcome::Passed, ""),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert!(reached(&results, speaking(), &mut out, &mut err).unwrap());
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "     Running target/debug/deps/x-4567\n     Running target/debug/deps/foo-0123abcd\n"
    );
    let out = String::from_utf8(out).unwrap();
    let tests: Vec<&str> = out
        .lines()
        .filter_map(|it| it.strip_prefix("test "))
        .filter(|it| it.ends_with("... ok"))
        .collect();
    assert_eq!(tests, ["x1 ... ok", "x2 ... ok", "f1 ... ok", "f2 ... ok", "f3 ... ok"]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert!(reached(&[], speaking(), &mut out, &mut err).unwrap());
    assert!(out.is_empty() && err.is_empty());
}

/// libtest's result line in each form it has: pretty and terse (the `ok` coloured or not), a
/// failed suite, a bench's, the JSON format's, and blaze's own of `--reached`.
#[test]
fn libtests_result_line_is_read_in_every_format() {
    let seven = Tally { passed: 7, failed: 1, ignored: 2, filtered_out: 30, ..Tally::default() };
    for line in [
        "test result: FAILED. 7 passed; 1 failed; 2 ignored; 0 measured; 30 filtered out; finished in 0.31s",
        "test result: \x1b[31mFAILED\x1b[0m. 7 passed; 1 failed; 2 ignored; 0 measured; 30 filtered out; finished in 0.31s",
        "  test result: FAILED. 7 passed; 1 failed; 2 ignored; 0 measured; 30 filtered out; 4 unreached; finished in 0.31s\r",
        r#"{ "type": "suite", "event": "failed", "passed": 7, "failed": 1, "ignored": 2, "measured": 0, "filtered_out": 30, "exec_time": 0.31 }"#,
    ] {
        assert_eq!(counts_of(line), Some(seven), "{line}");
    }
    assert_eq!(
        counts_of("test result: ok. 0 passed; 0 failed; 0 ignored; 3 measured; 0 filtered out; finished in 2.10s"),
        Some(Tally { measured: 3, ..Tally::default() })
    );
    for line in [
        "running 7 tests",
        "test a::b ... ok",
        "test result: ok.",
        "test result: ok. 7 passed; 1 failed",
        "the test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
        r#"{ "type": "suite", "event": "started", "test_count": 7 }"#,
        r#"{ "type": "test", "event": "ok", "name": "a" }"#,
        r#"{ "type": "suite", "event": "ok", "passed": 7 }"#,
        "{ not json",
    ] {
        assert_eq!(counts_of(line), None, "{line}");
    }
}

/// The result lines are found whatever chunks the output comes in, the last line with or
/// without its newline; a line too long to be one is none, and does not hide the next.
#[test]
fn the_scan_finds_result_lines_across_any_chunks() {
    let line = "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 5 filtered out; finished in 0.00s";
    let long = "x".repeat(3 * LINE);
    let text = format!("running 4 tests\n{long}\n{line}\ntest a ... ok\n{long}{line}\n\n{line}");
    for size in [1, 2, 7, 64, 4096, text.len()] {
        let mut scan = Scan::default();
        let mut found = Vec::new();
        for chunk in text.as_bytes().chunks(size) {
            scan.feed(chunk, &mut |tally| found.push(tally));
        }
        scan.end(&mut |tally| found.push(tally));
        let one = Tally { passed: 3, ignored: 1, filtered_out: 5, ..Tally::default() };
        assert_eq!(found, [one, one], "in chunks of {size}: the line after a long line's start is part of it");
    }
}

/// What each executable's counts are in the run's: its result lines' sum; a failure of its own
/// when it ended badly with no failing test; none for a libtest executable that printed no
/// result line; one test by its exit status for any other executable.
#[test]
fn an_executables_counts_in_the_runs() {
    let said = Tally { passed: 2, filtered_out: 9, ..Tally::default() };
    assert_eq!(counted(Some(said), true, true), Tally { binaries: 1, ..said });
    assert_eq!(counted(Some(said), false, true), Tally { binaries: 1, failed: 1, ..said });
    assert_eq!(counted(None, true, true), Tally { binaries: 1, ..Tally::default() });
    assert_eq!(counted(None, false, true), Tally { binaries: 1, failed: 1, ..Tally::default() });
    assert_eq!(counted(None, true, false), Tally { binaries: 1, passed: 1, ..Tally::default() });
    assert_eq!(counted(None, false, false), Tally { binaries: 1, failed: 1, ..Tally::default() });
}

#[test]
fn the_selection_reads_the_filters_and_what_runs_nothing() {
    let it = Selection::of(&asked(
        Command::Test,
        &["-p", "foo", "adds", "--", "--skip", "slow", "--exact", "wrong", "--test-threads", "2"],
    ));
    assert_eq!(it.typed, "blaze test -p foo adds -- --skip slow --exact wrong --test-threads 2");
    assert_eq!(it.filters, ["adds", "wrong"], "a value of libtest's is no filter");
    assert!(!it.lists && !it.pass && !it.reached);
    assert!(Selection::of(&asked(Command::Test, &["--no-run"])).lists);
    assert!(Selection::of(&asked(Command::Test, &["--", "--list"])).lists);
    assert!(Selection::of(&asked(Command::Bench, &["--", "--help"])).lists);
    assert!(Selection::of(&asked(Command::Test, &["--no-tests", "pass"])).pass);
    assert!(!Selection::of(&asked(Command::Test, &["--no-tests=fail"])).pass);
    assert!(Selection::of(&asked(Command::Bench, &["--no-tests=pass"])).pass);
    assert!(Selection::of(&asked(Command::Test, &["--reached"])).reached);
}

/// What a command's exit code may be for a run's counts: 0 only when a test ran, or the
/// command said a run of none is meant.
fn a_pass_is_of_something(selection: &Selection, tally: &Tally, code: i32) -> bool {
    code != 0
        || tally.ran() > 0
        || selection.lists
        || selection.pass
        || (selection.reached && tally.unreached > 0)
}

/// Every selection and every small tally: each count 0, 1 or 3.
fn every_run() -> Vec<(Selection, Tally)> {
    let mut runs = Vec::new();
    let selections = [
        Selection { typed: "blaze test".into(), ..Selection::default() },
        Selection { typed: "blaze test nope".into(), filters: vec!["nope".into()], ..Selection::default() },
        Selection { typed: "blaze test --no-run".into(), lists: true, ..Selection::default() },
        Selection { typed: "blaze test".into(), pass: true, ..Selection::default() },
        Selection { typed: "blaze test".into(), reached: true, ..Selection::default() },
    ];
    let counts = [0, 1, 3];
    for selection in &selections {
        for binaries in counts {
            for passed in counts {
                for failed in counts {
                    for ignored in counts {
                        for filtered_out in counts {
                            for unreached in counts {
                                for unbuilt in [0, 1] {
                                    let tally = Tally {
                                        binaries,
                                        passed,
                                        failed,
                                        ignored,
                                        measured: 0,
                                        filtered_out,
                                        unreached,
                                        unbuilt,
                                    };
                                    runs.push((selection.clone(), tally));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    runs
}

/// The invariant: a `test` or `bench` that exits 0 ran at least one test, unless its command
/// line says a run of none is meant (`--no-run`, `--list`, `--no-tests pass`, or `--reached`
/// with tests selected and unreached). Checked for every selection and small tally, through
/// [`conclude`], with the exit code the executables gave: cargo's (0 unless one failed or
/// never built).
#[test]
fn a_run_that_exits_0_ran_a_test_or_was_told_none_is_fine() {
    for (selection, tally) in every_run() {
        let cargos = if tally.failed > 0 || tally.unbuilt > 0 { 101 } else { 0 };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = conclude(
            &selection,
            &tally,
            cargos,
            true,
            std::time::Duration::ZERO,
            Names::default,
            speaking(),
            &mut out,
            &mut err,
        )
        .unwrap();
        let err = String::from_utf8(err).unwrap();
        assert!(a_pass_is_of_something(&selection, &tally, code), "{selection:?} {tally:?}: {code}\n{err}");
        // A run that failed keeps its code; only a pass of nothing becomes a refusal.
        assert!(code == cargos || (cargos == 0 && code == Exit::NoTests.code()), "{selection:?} {tally:?}");
        // And every run that says anything says its counts, with the verdict the code has.
        if !selection.lists {
            let word = if code == 0 { "ok" } else { "failed" };
            let line = format!("blaze: tests {word} selected={} ran={} ", tally.selected(), tally.ran());
            assert!(err.contains(&line), "{selection:?} {tally:?}: {err}");
            assert_eq!(err.contains("error: no test ran"), code == Exit::NoTests.code(), "{err}");
        }
    }
}

/// The planted bug: cargo's and libtest's rule, 0 whenever nothing failed. The oracle of the
/// test above refuses it, on the run this whole rule exists for: a filter that matched none.
#[test]
fn the_oracle_catches_a_run_of_none_that_exits_0() {
    let caught: Vec<(Selection, Tally)> = every_run()
        .into_iter()
        .filter(|(selection, tally)| {
            let cargos = if tally.failed > 0 || tally.unbuilt > 0 { 101 } else { 0 };
            !a_pass_is_of_something(selection, tally, cargos)
        })
        .collect();
    let filtered = Tally { binaries: 3, filtered_out: 3, ..Tally::default() };
    assert!(
        caught.iter().any(|(selection, tally)| selection.filters == ["nope"] && *tally == filtered),
        "the oracle passes `blaze test nope` with every test filtered out and exit 0"
    );
    assert!(caught.iter().all(|(_, tally)| tally.ran() == 0));
}

/// The instance (agents.json A39): a name filter that matches no test. Every executable says
/// `0 passed; 5 filtered out` and exits 0, which cargo reports as a pass. blaze refuses it
/// with a code of its own, names the filter and the nearest tests that exist, and its last
/// line and its JSON say that nothing ran.
#[test]
fn a_filter_that_matches_no_test_is_refused_naming_it_and_the_nearest_tests() {
    let nothing = Tally { filtered_out: 5, ..Tally::default() };
    let mut fake = Fake {
        counts: Some(nothing),
        names: vec!["tests::adds_wrong", "tests::subtracts", "parse::reads_a_number"],
        ..Fake::default()
    };
    let asked = asked(Command::Test, &["adds_wrng"]);
    let (code, tally, out, err) = concluded(&asked, &mut fake, speaking());
    assert_eq!(code, Exit::NoTests.code());
    assert_eq!(code, 4, "the number scripts read; nextest's for the same end");
    assert_eq!((tally.selected(), tally.ran(), tally.filtered_out, tally.binaries), (0, 0, 20, 4));
    assert_eq!(out, "");
    assert!(
        err.ends_with(
            "error: no test ran: `blaze test adds_wrng` matched none of the 20 tests in 4 test executables\n\
             note: the nearest that exist: tests::adds_wrong\n\
             note: a run of no test is refused; `--no-tests pass` allows one, as cargo does\n\
             blaze: tests failed selected=0 ran=0 passed=0 failed=0 ignored=0 measured=0 filtered_out=20 \
             unreached=0 binaries=4 unbuilt=0 elapsed_ms=12 why=\"no test ran\"\n"
        ),
        "{err}"
    );
    // Each libtest executable was asked for its list, as the daemon would start it.
    assert_eq!(fake.listed.len(), 3, "the three test executables, not the doc tests");
    assert_eq!(fake.listed[0].args, LIST.map(OsString::from));

    // The control, and what depends on cargo's behaviour: `--no-tests pass` is exit 0, and
    // the line still says that nothing ran.
    let mut fake = Fake { counts: Some(nothing), ..Fake::default() };
    let allowed = self::asked(Command::Test, &["adds_wrng", "--no-tests", "pass"]);
    let (code, _, _, err) = concluded(&allowed, &mut fake, speaking());
    assert_eq!(code, 0);
    assert!(err.ends_with("blaze: tests ok selected=0 ran=0 passed=0 failed=0 ignored=0 measured=0 filtered_out=20 unreached=0 binaries=4 unbuilt=0 elapsed_ms=12\n"), "{err}");
    assert!(fake.listed.is_empty(), "nothing is listed for a run that is not refused");

    // With a JSON `--message-format`, the same on stdout as one line of fields.
    let mut fake = Fake { counts: Some(nothing), ..Fake::default() };
    let (code, _, out, _) = concluded(&asked, &mut fake, Speaking { json: true, ..speaking() });
    assert_eq!(code, 4);
    assert_eq!(
        out,
        "{\"reason\":\"blaze-test-summary\",\"ok\":false,\"selected\":0,\"ran\":0,\"passed\":0,\"failed\":0,\
         \"ignored\":0,\"measured\":0,\"filtered_out\":20,\"unreached\":0,\"binaries\":4,\"unbuilt\":0,\
         \"elapsed_ms\":12,\"why\":\"no test ran\"}\n"
    );
}

/// A run that ran tests ends as before, and its last line says how many: the filter that
/// matched in one executable of four is a pass of that one.
#[test]
fn a_run_that_ran_tests_says_how_many() {
    let mut fake = Fake::default();
    let (code, tally, out, err) = concluded(&asked(Command::Test, &[]), &mut fake, speaking());
    assert_eq!((code, out.as_str()), (0, ""));
    assert_eq!((tally.selected(), tally.ran(), tally.binaries), (4, 4, 4));
    assert!(err.ends_with("   Doc-tests foo\nblaze: tests ok selected=4 ran=4 passed=4 failed=0 ignored=0 measured=0 filtered_out=0 unreached=0 binaries=4 unbuilt=0 elapsed_ms=12\n"), "{err}");

    // `-q` says errors only: no line for a pass, the refusal for a run of none.
    let quiet = Speaking { quiet: true, ..speaking() };
    let (code, _, _, err) = concluded(&asked(Command::Test, &["-q"]), &mut Fake::default(), quiet);
    assert_eq!((code, err.as_str()), (0, ""));
    let mut fake = Fake { counts: Some(Tally { filtered_out: 2, ..Tally::default() }), ..Fake::default() };
    let (code, _, _, err) = concluded(&asked(Command::Test, &["-q", "nope"]), &mut fake, quiet);
    assert_eq!(code, 4);
    assert!(err.starts_with("error: no test ran: `blaze test -q nope` matched none of the 8 tests"), "{err}");
    assert!(!err.contains("blaze: tests"), "{err}");

    // `--no-run` runs nothing on purpose and says nothing here.
    let (code, tally, out, err) =
        concluded(&asked(Command::Test, &["--no-run"]), &mut Fake::default(), speaking());
    assert_eq!((code, tally.ran(), out.as_str()), (0, 0, ""));
    assert!(!err.contains("blaze: tests") && !err.contains("no test ran"), "{err}");
}

/// What the refusal says for each way a run comes to nothing.
#[test]
fn the_refusal_names_why_nothing_ran() {
    let typed = |words: &[&str]| Selection::of(&asked(Command::Test, words));
    let no_names = Names::default;
    let ignored = Tally { binaries: 1, ignored: 3, filtered_out: 9, ..Tally::default() };
    let said = refusal(&typed(&["slow"]), &ignored, no_names).unwrap();
    assert_eq!(said.what, "no test ran: every one of the 3 `blaze test slow` selected is ignored (`#[ignore]`)");
    assert!(said.help[0].contains("--ignored"), "{said:?}");

    let empty = Tally { binaries: 1, ..Tally::default() };
    let said = refusal(&typed(&["-p", "foo"]), &empty, no_names).unwrap();
    assert_eq!(said.what, "no test ran: `blaze test -p foo` selected 1 test executable, which holds no test");

    let said = refusal(&typed(&["--doc"]), &Tally::default(), no_names).unwrap();
    assert_eq!(said.what, "no test ran: `blaze test --doc` selected no test executable");

    // No test near the filter: how many exist, and how to list them; and a listing that failed.
    let filtered = Tally { binaries: 2, filtered_out: 40, ..Tally::default() };
    let names = || Names {
        names: vec!["a::b".to_owned(), "c::d".to_owned()],
        unlisted: vec!["`/w/x --list`: exit status: 1".to_owned()],
    };
    let said = refusal(&typed(&["zzzzzzzz"]), &filtered, names).unwrap();
    assert_eq!(
        said.help,
        [
            "none of the 2 that exist is near it; `blaze test -- --list` lists them",
            "could not be listed: `/w/x --list`: exit status: 1",
            "a run of no test is refused; `--no-tests pass` allows one, as cargo does",
        ]
    );

    // `--reached`: tests selected and none reached is that flag's answer; none selected is not.
    let unreached = Tally { binaries: 1, unreached: 4, ..Tally::default() };
    assert_eq!(refusal(&typed(&["--reached"]), &unreached, no_names), None);
    assert!(refusal(&typed(&["--reached", "nope"]), &Tally::default(), no_names).is_some());
    assert!(refusal(&typed(&[]), &unreached, no_names).is_some(), "only under `--reached`");
}

#[test]
fn the_daemons_results_are_counted_by_outcome_and_executable() {
    let results = [
        result("/w/target/debug/deps/foo-0123abcd", "a", TestOutcome::Passed, ""),
        result("/w/target/debug/deps/foo-0123abcd", "b", TestOutcome::Failed, ""),
        result("/w/target/debug/deps/foo-0123abcd", "c", TestOutcome::Unreached, ""),
        result("/w/target/debug/deps/x-4567", "d", TestOutcome::Ignored, ""),
    ];
    assert_eq!(
        tally_of(&results),
        Tally { binaries: 2, passed: 1, failed: 1, ignored: 1, unreached: 1, ..Tally::default() }
    );
    assert_eq!(tally_of(&[]), Tally::default());
}

/// A libtest name listing, as `--list --format terse` prints it.
#[test]
fn the_names_of_a_terse_listing() {
    let text = "tests::adds: test\ntests::fast: benchmark\n\n2 tests, 1 benchmarks\nsrc/lib.rs - add (line 3): test\n";
    assert_eq!(listed(text), ["tests::adds", "tests::fast", "src/lib.rs - add (line 3)"]);
}
