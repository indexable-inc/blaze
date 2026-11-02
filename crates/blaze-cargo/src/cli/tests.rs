use std::{ffi::OsString, io, path::PathBuf, time::Instant};

use blaze_api::{Request, Response, TestOutcome, TestResult, Ub, UnitStarted, Verb, client::ClientError};

use super::*;
use crate::cli::{
    args::{Asked, Invocation},
    say::Ended,
    test::tests::{Fake, built},
};

fn os(words: &[&str]) -> impl Iterator<Item = OsString> {
    words.iter().map(OsString::from).collect::<Vec<_>>().into_iter()
}

fn invocation(words: Vec<OsString>) -> Invocation {
    match args::parse(words).unwrap() {
        Asked::Cargo(asked) => *asked,
        other => panic!("not a build: {other:?}"),
    }
}

fn place() -> Place {
    Place { cwd: PathBuf::from("/w"), manifest: PathBuf::from("/w/Cargo.toml"), root: PathBuf::from("/w") }
}

fn request_of(asked: &Invocation) -> Request {
    let env = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
    request(asked, place::cargo(asked, &place(), env, Vec::new(), false, None).unwrap()).unwrap()
}

#[test]
fn the_width_and_the_verbosity_go_with_the_command_line() {
    let asked = invocation(words(os(&["blaze", "build", "-vv"])));
    let cargo = place::cargo(&asked, &place(), Vec::new(), Vec::new(), true, Some(120)).unwrap();
    assert_eq!((cargo.width, cargo.verbose, cargo.color), (Some(120), 2, true));
    let quiet = invocation(words(os(&["blaze", "build", "-q"])));
    assert_eq!(place::cargo(&quiet, &place(), Vec::new(), Vec::new(), false, None).unwrap().verbose, 0);
}

#[test]
fn cargo_blaze_and_blaze_are_the_same_command() {
    let through_cargo = words(os(&["/home/u/.cargo/bin/cargo-blaze", "blaze", "build", "--release", "-p", "foo"]));
    let direct = words(os(&["blaze", "build", "--release", "-p", "foo"]));
    assert_eq!(through_cargo, direct);
    let request = request_of(&invocation(direct));
    assert_eq!(request, request_of(&invocation(through_cargo)));
    let Request::Build { view, targets } = request else { panic!("not a build: {request:?}") };
    assert_eq!(view, ViewRef::Head("/w".to_owned()));
    assert_eq!(targets.packages, ["foo"]);
    assert_eq!(targets.profile, "release");
    let cargo = targets.cargo.unwrap();
    assert_eq!(cargo.command, "build");
    assert_eq!(cargo.args, ["--release", "-p", "foo"]);
    assert_eq!((cargo.root.as_str(), cargo.cwd.as_str()), ("/w", "/w"));
}

#[test]
fn each_command_its_request() {
    let check = request_of(&invocation(words(os(&["blaze", "check", "--features", "a,b"]))));
    let Request::Check { targets, .. } = check else { panic!("{check:?}") };
    assert_eq!(targets.features, ["a", "b"]);
    assert_eq!(targets.profile, "dev");

    let test = request_of(&invocation(words(os(&["blaze", "t", "parse", "--", "--exact"]))));
    let Request::Build { targets, .. } = test else { panic!("{test:?}") };
    assert_eq!(targets.profile, "test");

    let reached = request_of(&invocation(words(os(&["blaze", "test", "--reached", "parse"]))));
    let Request::Test { filter, all, targets, .. } = reached else { panic!("{reached:?}") };
    assert_eq!((filter, all), (vec!["parse".to_owned()], false));
    // `--reached` is blaze's: cargo's command line does not have it.
    assert_eq!(targets.cargo.unwrap().args, ["parse"]);

    let check = request_of(&invocation(words(os(&["blaze", "check", "--baselines", "b", "--fix"]))));
    let Request::Check { targets, .. } = check else { panic!("{check:?}") };
    let cargo = targets.cargo.unwrap();
    assert_eq!(cargo.command, "check");
    assert_eq!(cargo.args, ["--all-targets"], "`--fix` checks every target; blaze's flags stay with the client");
}

#[test]
fn ub_runs_every_selected_test_in_the_daemon() {
    let ub = request_of(&invocation(words(os(&["blaze", "test", "--ub-seed", "7", "parse", "--", "--exact", "--ignored"]))));
    let Request::Test { filter, all, args, ub, targets, .. } = ub else { panic!("{ub:?}") };
    assert_eq!((filter, all), (vec!["parse".to_owned()], true));
    assert_eq!(args, ["--exact", "--ignored"], "libtest's selection, which the daemon applies");
    assert_eq!(ub, Some(Ub { seed: Some(7), schedules: None }));
    assert_eq!(targets.cargo.unwrap().args, ["parse"]);

    let both = request_of(&invocation(words(os(&["blaze", "test", "--ub", "--reached"]))));
    let Request::Test { all, ub, .. } = both else { panic!("{both:?}") };
    assert_eq!((all, ub), (false, Some(Ub { seed: None, schedules: None })));

    let reached = request_of(&invocation(words(os(&["blaze", "test", "--reached"]))));
    let Request::Test { ub, .. } = reached else { panic!("{reached:?}") };
    assert_eq!(ub, None, "opt-in");

    let asked = invocation(words(os(&["blaze", "test", "--ub", "--no-run"])));
    let cargo = place::cargo(&asked, &place(), Vec::new(), Vec::new(), false, None).unwrap();
    assert!(request(&asked, cargo).unwrap_err().contains("--no-run"));
}

#[test]
fn ub_results_print_as_libtest_prints_them() {
    let failed = TestResult {
        binary: "/w/target/debug/deps/foo-1".to_owned(),
        name: "tests::aliasing".to_owned(),
        outcome: TestOutcome::Failed,
        output: b"undefined behavior: a write through a reference that was retagged as shared\nseed: 7".to_vec(),
        millis: 0,
        cached: None,
        outside: false,
        executable: 0,
    };
    let passed = TestResult { name: "tests::fine".to_owned(), outcome: TestOutcome::Passed, output: Vec::new(), ..failed.clone() };
    let mut api = FakeApi::answering(vec![Response::TestResult(passed), Response::TestResult(failed), Response::Done { ok: true }]);
    let (then, out, err) = driven(&mut api, &["blaze", "test", "--ub", "-q"], &mut Fake::default());
    assert_eq!(then, Then::Exit(FAILED));
    assert!(out.contains("test tests::aliasing ... FAILED\n"), "{out}");
    assert!(out.contains("---- tests::aliasing stdout ----\nundefined behavior:"), "{out}");
    assert!(out.contains("seed: 7"), "{out}");
    assert_eq!(err, "", "nothing compiled, nothing run on this machine");
}

#[test]
fn the_summary_names_the_tests_that_ran_outside_the_sandbox() {
    let result = |name: &str, outside: bool| TestResult {
        binary: "/w/target/debug/deps/foo-1".to_owned(),
        name: name.to_owned(),
        outcome: TestOutcome::Passed,
        output: Vec::new(),
        millis: 1,
        cached: None,
        outside,
        executable: 0,
    };
    let responses = vec![
        Response::TestResult(result("tests::inside", false)),
        Response::TestResult(result("tests::writes_home", true)),
        Response::TestResult(result("tests::dials", true)),
        Response::Done { ok: true },
    ];
    let (then, out, err) = driven(&mut FakeApi::answering(responses.clone()), &["blaze", "test", "--reached"], &mut Fake::default());
    assert_eq!(then, Then::Exit(0));
    assert!(out.contains("test tests::writes_home ... ok\n"), "{out}");
    assert!(
        err.ends_with(
            "note: 2 tests ran outside the sandbox, so they run every time and their passes are never kept: \
             tests::writes_home, tests::dials\n"
        ),
        "{err}"
    );
    let (_, _, quiet) = driven(&mut FakeApi::answering(responses), &["blaze", "test", "--reached", "-q"], &mut Fake::default());
    assert!(!quiet.contains("note:"), "cargo's notes are silent under -q: {quiet}");
}

#[test]
fn usage_errors_exit_1() {
    assert_eq!(command(words(os(&["blaze", "build", "--frobnicate"]))), USAGE);
    assert_eq!(command(words(os(&["blaze", "build", "--jobs"]))), USAGE);
    assert_eq!(command(words(os(&["blaze", "test", "--message-format", "xml"]))), USAGE);
    assert_eq!(command(words(os(&["blaze", "frobnicate"]))), FAILED);
}

/// Answers every request with `responses`.
struct FakeApi {
    responses: Vec<Response>,
    asked: Vec<Request>,
    hangs_up: bool,
}

impl FakeApi {
    fn answering(responses: Vec<Response>) -> FakeApi {
        FakeApi { responses, asked: Vec::new(), hangs_up: false }
    }
}

impl daemon::Api for FakeApi {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, request: Request, on: F) -> Result<bool, ClientError> {
        self.asked.push(request);
        let ok = daemon::tests::hand(&self.responses, on);
        match self.hangs_up {
            true => Err(ClientError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "the daemon hung up"))),
            false => Ok(ok),
        }
    }
}

fn foo_compiling() -> Response {
    Response::Unit(UnitStarted {
        package_id: "path+file:///w/foo#0.1.0".to_owned(),
        name: "foo".to_owned(),
        version: "0.1.0".to_owned(),
        source: Some("(/w/foo)".to_owned()),
        verb: Verb::Compiling,
        package: 0,
    })
}

fn tests_built() -> Vec<Response> {
    vec![
        foo_compiling(),
        Response::Built { members: 1, artifacts: built(), profile: "`test` profile [unoptimized + debuginfo]".to_owned() },
        Response::Done { ok: true },
    ]
}

fn driven(api: &mut FakeApi, words: &[&str], programs: &mut Fake) -> (Then, String, String) {
    let asked = invocation(self::words(os(words)));
    let request = request_of(&asked);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let shown = Shown { quiet: asked.flags.quiet, verbose: asked.flags.verbose, ..Shown::default() };
    let then = drive(api, &asked, &place(), request, shown, Instant::now(), &mut out, &mut err, programs).unwrap();
    (then, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn a_failing_test_executable_stops_the_tests() {
    let mut api = FakeApi::answering(tests_built());
    let mut programs = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let (then, out, err) = driven(&mut api, &["blaze", "test"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert_eq!(out, "");
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines[0], "   Compiling foo v0.1.0 (/w/foo)");
    assert!(lines[1].starts_with("    Finished `test` profile [unoptimized + debuginfo] target(s) in "), "{err}");
    assert_eq!(&lines[2..], [
        "     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)",
        "error: test failed, to rerun pass `--lib`",
    ]);
    assert_eq!(programs.ran.len(), 1);
    assert!(matches!(api.asked.as_slice(), [Request::Build { .. }]));

    let mut programs = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let (then, _, err) = driven(&mut api, &["blaze", "test", "--no-fail-fast"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert_eq!(programs.ran.len(), 4);
    assert!(err.ends_with("   Doc-tests foo\nerror: 1 target failed:\n    `--lib`\n"), "{err}");
}

#[test]
fn a_failed_build_runs_nothing() {
    let mut api = FakeApi::answering(vec![foo_compiling(), Response::Done { ok: false }]);
    let mut programs = Fake::default();
    let (then, _, err) = driven(&mut api, &["blaze", "test"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert!(programs.ran.is_empty());
    assert_eq!(err, "   Compiling foo v0.1.0 (/w/foo)\n");

    let mut api = FakeApi { hangs_up: true, ..FakeApi::answering(vec![foo_compiling()]) };
    let (then, _, err) = driven(&mut api, &["blaze", "build"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert!(err.ends_with("error: the daemon's connection: the daemon hung up\n"), "{err}");
}

#[test]
fn run_becomes_its_program() {
    let foo = crate::cli::run::tests::artifact("foo", "bin", "foo", false, Some("foo"));
    let built = Response::Built { members: 1, artifacts: vec![foo], profile: "`dev` profile [unoptimized + debuginfo]".to_owned() };
    let mut api = FakeApi::answering(vec![built, Response::Done { ok: true }]);
    let (then, _, err) = driven(&mut api, &["blaze", "run", "--release", "a", "--", "b"], &mut Fake::default());
    let Then::Exec(exec) = then else { panic!("{then:?}") };
    assert_eq!(exec.program, PathBuf::from("target/debug/foo"));
    assert_eq!(exec.args, [OsString::from("a"), OsString::from("b")]);
    assert_eq!(exec.cwd, PathBuf::from("/w"));
    assert!(err.ends_with("     Running `target/debug/foo a b`\n"), "{err}");

    let two = [
        crate::cli::run::tests::artifact("foo", "bin", "foo", false, Some("foo")),
        crate::cli::run::tests::artifact("foo", "bin", "bar", false, Some("bar")),
    ];
    let built = Response::Built { members: 1, artifacts: two.to_vec(), profile: "`dev` profile [unoptimized + debuginfo]".to_owned() };
    let mut api = FakeApi::answering(vec![built, Response::Done { ok: true }]);
    let (then, _, err) = driven(&mut api, &["blaze", "run", "-q"], &mut Fake::default());
    assert_eq!(then, Then::Exit(101));
    assert!(err.starts_with("error: `cargo run` could not determine which binary to run."), "{err}");
}

#[test]
fn doc_writes_its_pages_and_says_what_cargo_doc_says() {
    let dir = std::env::temp_dir().join(format!("blaze-cli-doc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("old")).unwrap();
    std::fs::write(dir.join("old/index.html"), "").unwrap();
    std::fs::write(dir.join("crates.js"), "window.BLAZE_CRATES = [\"gone\",\"old\"];\n").unwrap();
    let documented = |dir: &std::path::Path| {
        let crates = vec![
            blaze_api::DocumentedCrate { name: "foo".into(), package: "foo v0.1.0 (/w/foo)".into(), asked: true },
            blaze_api::DocumentedCrate { name: "dep".into(), package: "dep v1.0.0".into(), asked: false },
        ];
        Response::Documented(blaze_api::Documented { dir: dir.display().to_string(), crates, pages: 2, source_pages: 0 })
    };
    let built = || Response::Built { members: 1, artifacts: Vec::new(), profile: "`dev` profile [unoptimized + debuginfo]".to_owned() };
    let responses = vec![
        built(),
        documented(&dir),
        Response::DocPage { path: "foo/index.html".into(), page: b"<html>foo</html>".to_vec() },
        Response::DocPage { path: "dep/index.html".into(), page: b"dep".to_vec() },
        Response::Done { ok: true },
    ];
    let (then, out, err) = driven(&mut FakeApi::answering(responses.clone()), &["blaze", "doc"], &mut Fake::default());
    assert_eq!(then, Then::Exit(0));
    assert_eq!(out, "");
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines[..2], [" Documenting foo v0.1.0 (/w/foo)", " Documenting dep v1.0.0"], "{err}");
    assert!(lines[2].starts_with("    Finished `dev` profile [unoptimized + debuginfo] target(s) in "), "{err}");
    assert_eq!(lines[3], format!("   Generated {}", dir.join("foo/index.html").display()), "{err}");
    assert_eq!(std::fs::read_to_string(dir.join("foo/index.html")).unwrap(), "<html>foo</html>");
    assert_eq!(std::fs::read_to_string(dir.join("dep/index.html")).unwrap(), "dep");
    let listed = std::fs::read_to_string(dir.join("crates.js")).unwrap();
    assert_eq!(listed, "window.BLAZE_CRATES = [\"dep\",\"foo\",\"old\"];\n", "an earlier crate whose pages are gone is not listed");

    let (_, _, quiet) = driven(&mut FakeApi::answering(responses), &["blaze", "doc", "-q"], &mut Fake::default());
    assert_eq!(quiet, "", "cargo's status lines are silent under -q");

    let out_of_it = dir.join("inner");
    let escaping = vec![
        built(),
        documented(&out_of_it),
        Response::DocPage { path: "../escaped.html".into(), page: b"x".to_vec() },
        Response::Done { ok: true },
    ];
    let (then, _, err) = driven(&mut FakeApi::answering(escaping), &["blaze", "doc"], &mut Fake::default());
    assert_eq!(then, Then::Exit(FAILED));
    assert!(err.contains("the daemon sent the page `../escaped.html`, which is not under"), "{err}");
    assert!(!dir.join("escaped.html").exists(), "nothing is written outside the documentation's directory");
    std::fs::remove_dir_all(&dir).unwrap();
}
