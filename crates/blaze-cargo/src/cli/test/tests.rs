use std::{ffi::OsString, io, path::Path};

use blaze_api::{Artifact, ArtifactKind, TestOutcome, TestResult};

use super::*;
use crate::cli::{
    args::{Command, Invocation, command_line},
    run::{Exec, tests::artifact},
    say::Ended,
};

/// Ends each executable as `ends` says by its file name, and the rest with 0.
#[derive(Default)]
pub(crate) struct Fake {
    pub ends: Vec<(&'static str, Ended)>,
    pub ran: Vec<Exec>,
}

impl Programs for Fake {
    fn run(&mut self, exec: &Exec) -> io::Result<Ended> {
        self.ran.push(exec.clone());
        let name = exec.program.file_name().unwrap().to_str().unwrap().to_owned();
        Ok(self.ends.iter().find(|(it, _)| *it == name).map_or(Ended::Code(0), |(_, ended)| *ended))
    }
}

/// A package `foo` with a lib, a bin and a test `x`, each tested, and its doc tests; out of
/// cargo's order.
pub(crate) fn built() -> Vec<Artifact> {
    vec![
        artifact("foo", "test", "x", true, Some("deps/x-4567")),
        artifact("foo", "lib", "foo", false, None),
        Artifact { kind: ArtifactKind::Doctest, ..artifact("foo", "lib", "foo", true, Some("deps/foo-doctests")) },
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
    let code = run(asked, &built(), 1, speaking, &mut err, fake).unwrap();
    (code, String::from_utf8(err).unwrap())
}

#[test]
fn rerun_names_the_package_as_cargos_needs_spec_flag_does() {
    assert!(!needs_spec_flag(&asked(Command::Test, &[]), 1));
    assert!(needs_spec_flag(&asked(Command::Test, &[]), 2), "a workspace of two");
    assert!(needs_spec_flag(&asked(Command::Test, &["-p", "foo"]), 1));
    assert!(needs_spec_flag(&asked(Command::Test, &["--workspace", "--exclude", "bar"]), 1));

    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let mut err = Vec::new();
    let code = run(&asked(Command::Test, &[]), &built(), 3, speaking(), &mut err, &mut fake).unwrap();
    assert_eq!(code, 101);
    assert!(String::from_utf8(err).unwrap().ends_with("error: test failed, to rerun pass `-p foo --lib`\n"));
}

#[test]
fn executables_in_cargos_order() {
    let built = built();
    let (tests, docs) = executables(&built);
    let shown: Vec<String> = tests.iter().map(|it| running(it, Path::new("/w"), Kind::Test)).collect();
    assert_eq!(shown, [
        "unittests src/lib.rs (target/debug/deps/foo-0123abcd)",
        "unittests src/main.rs (target/debug/deps/foo-89ab)",
        "tests/x.rs (target/debug/deps/x-4567)",
    ]);
    assert_eq!(docs.len(), 1);
    assert_eq!(running(tests[0], Path::new("/w"), Kind::Bench), "benches src/lib.rs (target/debug/deps/foo-0123abcd)");
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

#[test]
fn the_first_failure_stops_the_run() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &[]), &mut fake, speaking());
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
    assert!(err.contains("error: test failed, to rerun pass `--test x`\n   Doc-tests foo\n"), "{err}");
    assert!(err.ends_with("error: 2 targets failed:\n    `--lib`\n    `--test x`\n"), "{err}");

    let mut fake = Fake { ends: vec![("foo-doctests", Ended::Code(101))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["-p", "foo"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert!(err.ends_with("error: doctest failed, to rerun pass `-p foo --doc`\n"), "{err}");
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
    let doc = Artifact { kind: ArtifactKind::Doctest, ..artifact("foo", "lib", "my-lib", true, Some("deps/my_lib-doctests")) };
    let mut err = Vec::new();
    let code = run(&asked(Command::Test, &[]), &[doc.clone()], 1, speaking(), &mut err, &mut Fake::default()).unwrap();
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(err).unwrap(), "   Doc-tests my-lib\n");

    let mut err = Vec::new();
    let verbose = Speaking { verbose: 1, ..speaking() };
    run(&asked(Command::Test, &[]), &[doc], 1, verbose, &mut err, &mut Fake::default()).unwrap();
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "   Doc-tests my-lib\n     Running `/w/target/debug/deps/my_lib-doctests`\n"
    );
}

#[cfg(unix)]
#[test]
fn extra_verbose_shows_the_variables_cargo_sets() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (_, err) = tested(&asked(Command::Test, &[]), &mut fake, Speaking { verbose: 2, ..speaking() });
    let shown = "`CARGO_PKG_NAME=foo /w/target/debug/deps/foo-0123abcd`";
    assert!(err.starts_with(&format!("     Running {shown}\n")), "{err}");
    assert!(err.contains(&format!("process didn't exit successfully: {shown} (exit status: 3)")), "{err}");

    let (_, err) = tested(&asked(Command::Test, &[]), &mut Fake::default(), Speaking { verbose: 1, ..speaking() });
    assert!(err.starts_with("     Running `/w/target/debug/deps/foo-0123abcd`\n"), "{err}");
}

#[test]
fn no_run_under_json_lists_nothing() {
    let (code, err) = tested(&asked(Command::Test, &["--no-run"]), &mut Fake::default(), Speaking { json: true, ..speaking() });
    assert_eq!(code, 0);
    assert_eq!(err, "");
}

#[cfg(unix)]
#[test]
fn quiet_keeps_the_error_and_drops_the_note() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &[]), &mut fake, Speaking { quiet: true, ..speaking() });
    assert_eq!(code, 3);
    assert!(err.starts_with("error: test failed, to rerun pass `--lib`\n\nCaused by:\n"), "{err}");
    assert!(!err.contains("note:"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_test_that_ended_otherwise() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &[]), &mut fake, speaking());
    assert_eq!(code, 3);
    assert!(
        err.ends_with(
            "error: test failed, to rerun pass `--lib`\n\nCaused by:\n  process didn't exit successfully: \
             `/w/target/debug/deps/foo-0123abcd` (exit status: 3)\nnote: test exited abnormally; to see the \
             full output pass --no-capture to the harness.\n"
        ),
        "{err}"
    );

    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Signal(libc::SIGSEGV))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--", "--nocapture"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert!(
        err.ends_with(
            "(signal: 11, SIGSEGV: invalid memory reference)\n"
        ),
        "{err}"
    );
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
    let (code, err) = tested(&asked(Command::Bench, &["-q"]), &mut fake, Speaking { quiet: true, ..speaking() });
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
    let outside = TestResult { name: "tests::writes_home".to_owned(), outside: true, ..inside.clone() };
    let mut text = String::new();
    super::outside(&mut text, std::slice::from_ref(&inside), false);
    assert_eq!(text, "", "nothing when every test ran inside");
    super::outside(&mut text, &[inside.clone(), outside.clone()], false);
    assert_eq!(
        text,
        "note: 1 test ran outside the sandbox, so it runs every time and its pass is never kept: tests::writes_home\n"
    );
    text.clear();
    let failed = TestResult { name: "tests::dials".to_owned(), outcome: TestOutcome::Failed, ..outside.clone() };
    super::outside(&mut text, &[outside, inside, failed], true);
    assert_eq!(
        text,
        "\x1b[1m\x1b[92mnote\x1b[0m: 2 tests ran outside the sandbox, so they run every time and their passes are \
         never kept: tests::writes_home, tests::dials\n"
    );
}

#[test]
fn an_executable_index_past_any_request_is_refused_naming_it() {
    let far = TestResult { executable: u32::MAX - 1, ..result("/w/target/debug/deps/foo-0123abcd", "a", TestOutcome::Passed, "") };
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
        result("/w/target/debug/deps/foo-0123abcd", "b", TestOutcome::Failed, "thread 'b' panicked\n"),
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
    let tests: Vec<&str> = out.lines().filter_map(|it| it.strip_prefix("test ")).filter(|it| it.ends_with("... ok")).collect();
    assert_eq!(tests, ["x1 ... ok", "x2 ... ok", "f1 ... ok", "f2 ... ok", "f3 ... ok"]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert!(reached(&[], speaking(), &mut out, &mut err).unwrap());
    assert!(out.is_empty() && err.is_empty());
}
