//! A reported bug is counted where it happened, a test's expected one is its own, and a
//! process that reported one exits failing, naming it, unless it keeps its exit status.

use std::process::Command;

use super::{expecting, keep_exit_status, report, sites, total};

/// Runs this test binary's ignored test `name` alone, in a process of its own.
fn alone(name: &str) -> std::process::Output {
    let exe = std::env::current_exe().expect("the test binary's path");
    Command::new(exe)
        .args(["--exact", name, "--ignored"])
        .output()
        .unwrap_or_else(|error| panic!("running the test binary for `{name}`: {error}"))
}

#[test]
fn a_bug_a_test_expects_is_its_own_and_not_counted() {
    let before = total();
    let ((), seen) = expecting("expected kind", || {
        report("expected kind", "reported where the test expects it");
        report("expected kind", "reported where the test expects it");
    });
    assert_eq!(seen, 2);
    assert_eq!(total(), before, "an expected report counted in the process's");
    assert!(sites().iter().all(|it| it.kind != "expected kind"));
}

/// Run alone by [`a_process_that_reported_a_bug_exits_failing_naming_it`]: reports twice
/// from one site and passes.
#[test]
#[ignore = "run in a process of its own by a_process_that_reported_a_bug_exits_failing_naming_it"]
fn reports_a_bug_and_passes() {
    for _ in 0..2 {
        report("planted", "reported by a test");
    }
    assert_eq!(total(), 2);
    let [site] = &sites()[..] else { panic!("one site: {:?}", sites()) };
    assert_eq!((site.kind, site.what, site.count), ("planted", "reported by a test", 2));
    assert_eq!(site.at.file(), file!());
    assert_eq!(site.thread.as_deref(), Some("assert::reported::tests::reports_a_bug_and_passes"));
    assert!(site.backtrace.is_some());
}

/// Run alone by [`a_process_that_keeps_its_exit_status_exits_as_it_would`].
#[test]
#[ignore = "run in a process of its own by a_process_that_keeps_its_exit_status_exits_as_it_would"]
fn keeps_its_exit_status_and_reports_a_bug() {
    keep_exit_status();
    report("planted", "reported by a process that keeps its exit status");
    assert_eq!(total(), 1);
}

/// The test harness's half: a test that reported a bug passed as libtest judged it, and the
/// process that ran it fails at exit, naming the bug, its site and the test.
#[test]
fn a_process_that_reported_a_bug_exits_failing_naming_it() {
    let out = alone("assert::reported::tests::reports_a_bug_and_passes");
    let (stdout, stderr) =
        (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        stdout.contains("1 passed"),
        "the test itself passes\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(out.status.code(), Some(101), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("this process reported 2 blaze bug(s)"), "{stderr}");
    let site = format!("a blaze bug: `planted` reported by a test at {}:", file!());
    assert!(stderr.contains(&site), "{site} in\n{stderr}");
    assert!(
        stderr.contains(
            ", 2 times, first on thread `assert::reported::tests::reports_a_bug_and_passes`"
        ),
        "{stderr}"
    );
}

/// The blaze binary keeps its exit status: its daemon says its bugs in `blaze status`.
#[test]
fn a_process_that_keeps_its_exit_status_exits_as_it_would() {
    let out = alone("assert::reported::tests::keeps_its_exit_status_and_reports_a_bug");
    let (stdout, stderr) =
        (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("1 passed"), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stderr.contains("blaze bug"), "{stderr}");
}

/// `never!` in a release build reports at its own line and goes on; in a debug build it
/// panics, as it always did.
#[test]
fn a_failed_never_reports_in_release_and_panics_in_debug() {
    if cfg!(debug_assertions) {
        assert!(std::panic::catch_unwind(|| crate::never!(true, "planted never")).is_err());
        return;
    }
    let (fired, seen) = expecting("assertion", || crate::never!(true, "planted never"));
    assert!(fired);
    assert_eq!(seen, 1);
}
