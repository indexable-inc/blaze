use std::{ffi::OsString, io, path::PathBuf, time::Instant};

use blaze_api::{
    Request, Response, TestOutcome, TestResult, Ub, UnitStarted, Verb, client::ClientError,
};

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
    Place {
        cwd: PathBuf::from("/w"),
        manifest: PathBuf::from("/w/Cargo.toml"),
        root: PathBuf::from("/w"),
    }
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
    assert_eq!(
        place::cargo(&quiet, &place(), Vec::new(), Vec::new(), false, None).unwrap().verbose,
        0
    );
}

#[test]
fn cargo_blaze_and_blaze_are_the_same_command() {
    let through_cargo =
        words(os(&["/home/u/.cargo/bin/cargo-blaze", "blaze", "build", "--release", "-p", "foo"]));
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

    let check =
        request_of(&invocation(words(os(&["blaze", "check", "--baselines", "b", "--fix"]))));
    let Request::Check { targets, .. } = check else { panic!("{check:?}") };
    let cargo = targets.cargo.unwrap();
    assert_eq!(cargo.command, "check");
    assert_eq!(
        cargo.args,
        ["--all-targets"],
        "`--fix` checks every target; blaze's flags stay with the client"
    );
}

#[test]
fn ub_runs_every_selected_test_in_the_daemon() {
    let ub = request_of(&invocation(words(os(&[
        "blaze",
        "test",
        "--ub-seed",
        "7",
        "parse",
        "--",
        "--exact",
        "--ignored",
    ]))));
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
        output:
            b"undefined behavior: a write through a reference that was retagged as shared\nseed: 7"
                .to_vec(),
        millis: 0,
        cached: None,
        outside: false,
        executable: 0,
    };
    let passed = TestResult {
        name: "tests::fine".to_owned(),
        outcome: TestOutcome::Passed,
        output: Vec::new(),
        ..failed.clone()
    };
    let mut api = FakeApi::answering(vec![
        Response::TestResult(passed),
        Response::TestResult(failed),
        Response::Done { ok: true },
    ]);
    let (then, out, err) = driven(&mut api, &["blaze", "test", "--ub", "-q"], &mut Fake::default());
    assert_eq!(then, Then::Exit(Exit::Failed.code()));
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
    let (then, out, err) = driven(
        &mut FakeApi::answering(responses.clone()),
        &["blaze", "test", "--reached"],
        &mut Fake::default(),
    );
    assert_eq!(then, Then::Exit(0));
    assert!(out.contains("test tests::writes_home ... ok\n"), "{out}");
    assert!(
        err.ends_with(
            "note: 2 tests ran outside the sandbox, so they run every time and their passes are never kept: \
             tests::writes_home, tests::dials\n"
        ),
        "{err}"
    );
    let (_, _, quiet) = driven(
        &mut FakeApi::answering(responses),
        &["blaze", "test", "--reached", "-q"],
        &mut Fake::default(),
    );
    assert!(!quiet.contains("note:"), "cargo's notes are silent under -q: {quiet}");
}

#[test]
fn usage_errors_exit_1() {
    assert_eq!(command(words(os(&["blaze", "build", "--frobnicate"]))), Exit::Usage.code());
    assert_eq!(command(words(os(&["blaze", "build", "--jobs"]))), Exit::Usage.code());
    assert_eq!(
        command(words(os(&["blaze", "test", "--message-format", "xml"]))),
        Exit::Usage.code()
    );
    assert_eq!(command(words(os(&["blaze", "frobnicate"]))), Exit::Failed.code());
}

/// The bug this guards (2026-11-10): lane scripts ran `blaze daemon stop`, which is no command,
/// for hours; the refusal listed thirty commands and never said that `blaze stop` is the one.
#[test]
fn a_command_that_does_not_exist_names_the_one_meant_and_still_fails() {
    let said = no_such_command("daemon stop");
    assert!(said.starts_with("no such command: `blaze daemon stop`"), "{said}");
    assert!(said.contains("did you mean `blaze stop`?"), "{said}");
    assert_eq!(command(words(os(&["blaze", "daemon", "stop"]))), Exit::Failed.code());
    for (typed, meant) in [
        ("daemon status", "`blaze status`"),
        ("daemon start", "`blaze serve`"),
        ("daemon instal", "`blaze daemon install`"),
        ("daemon", "`blaze daemon install`"),
        ("biuld", "`blaze build`"),
        ("chekc", "`blaze check`"),
        ("Status", "`blaze status`"),
        ("clippy", "`blaze check`"),
    ] {
        let said = no_such_command(typed);
        assert!(said.contains(&format!("did you mean {meant}?")), "{typed}: {said}");
    }
    assert_eq!(command(words(os(&["blaze", "biuld"]))), Exit::Failed.code());
}

#[test]
fn a_word_far_from_every_command_is_given_no_guess_and_the_whole_list() {
    for typed in ["frobnicate", "daemon frobnicate", "xy"] {
        let said = no_such_command(typed);
        assert!(!said.contains("did you mean"), "{typed}: {said}");
        assert!(said.contains("blaze's commands are build, check, run, test"), "{said}");
    }
}

#[test]
fn every_command_suggested_is_one() {
    // `blaze daemon` alone is no command: it takes a verb.
    for name in COMMANDS.iter().filter(|it| **it != "daemon") {
        assert_ne!(
            args::parse(vec![OsString::from(*name)]),
            Ok(Asked::Unknown((*name).to_owned())),
            "`{name}` is in COMMANDS and is no command"
        );
    }
    for (typed, command, _) in ALIASES {
        let first = command.split(' ').nth(1).unwrap();
        assert!(COMMANDS.contains(&first), "`{typed}` points at `{command}`, which is no command");
        let typed: Vec<OsString> = typed.split(' ').map(OsString::from).collect();
        assert!(
            matches!(args::parse(typed.clone()), Ok(Asked::Unknown(_))),
            "{typed:?} is a command, so its alias is never read"
        );
    }
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
    fn call<F: FnMut(ResponseRef<'_>, bool)>(
        &mut self,
        request: Request,
        on: F,
    ) -> Result<bool, ClientError> {
        self.asked.push(request);
        let ok = daemon::tests::hand(&self.responses, on);
        match self.hangs_up {
            true => Err(ClientError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the daemon hung up",
            ))),
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
        Response::Built {
            members: 1,
            artifacts: built(),
            profile: "`test` profile [unoptimized + debuginfo]".to_owned(),
        },
        Response::Done { ok: true },
    ]
}

fn driven(api: &mut FakeApi, words: &[&str], programs: &mut Fake) -> (Then, String, String) {
    let asked = invocation(self::words(os(words)));
    let request = request_of(&asked);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let shown =
        Shown { quiet: asked.flags.quiet, verbose: asked.flags.verbose, ..Shown::default() };
    let options = blaze_screen::Options {
        quiet: shown.quiet,
        verbose: shown.verbose > 0,
        ..blaze_screen::Options::of(match asked.command() {
            Command::Run => blaze_screen::Command::Run,
            Command::Test => blaze_screen::Command::Test,
            Command::Check => blaze_screen::Command::Check,
            _ => blaze_screen::Command::Build,
        })
    };
    let mut screen = show::Show::piped(options, &mut err);
    let then =
        drive(api, &asked, &place(), request, shown, Instant::now(), &mut out, &mut screen, programs)
            .unwrap();
    drop(screen);
    (then, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn under_fail_fast_a_failing_test_executable_stops_the_tests() {
    let mut api = FakeApi::answering(tests_built());
    let mut programs = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let (then, out, err) = driven(&mut api, &["blaze", "test", "--fail-fast"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert_eq!(out, "");
    let lines: Vec<&str> = err.lines().collect();
    assert!(lines[0].starts_with("blaze: ok built=0 cached=0 warnings=0 elapsed_ms=0"), "{err}");
    assert_eq!(
        &lines[1..],
        [
            "     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)",
            "error: test failed, to rerun pass `--lib`",
        ]
    );
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
    let (then, _, err) = driven(&mut api, &["blaze", "test", "--fail-fast"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert!(programs.ran.is_empty());
    assert_eq!(err, "blaze: failed errors=0 warnings=0 unbuilt=0 elapsed_ms=0\n");

    let mut api = FakeApi { hangs_up: true, ..FakeApi::answering(vec![foo_compiling()]) };
    let (then, _, err) = driven(&mut api, &["blaze", "build"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert!(err.ends_with("error: the daemon's connection: the daemon hung up\n"), "{err}");
}

/// A build whose dependency failed exits nonzero and names what was not built, for every
/// command that builds, whatever the daemon's `Done` says: 0.20261028.7's daemon ended
/// `blaze build -p blaze-engine --release` ok with blaze-profile failed and both binaries behind
/// it, and the client exited 0 with nothing placed.
#[test]
fn a_build_that_left_units_behind_a_failure_exits_nonzero_and_names_them() {
    let unchecked = || {
        Response::Unchecked(blaze_api::Unchecked {
            open: Vec::new(),
            tests: Vec::new(),
            behind: vec![blaze_api::Behind {
                id: None,
                why: "errors in its code".to_owned(),
                unit: "dep v0.1.0 (lib)".to_owned(),
                units: vec!["foo v0.1.0 (lib)".to_owned(), "foo v0.1.0 (bin \"foo\")".to_owned()],
            }],
            stopped: false,
        })
    };
    for words in [
        &["blaze", "build"][..],
        &["blaze", "build", "--release"],
        &["blaze", "check"],
        &["blaze", "test"],
        &["blaze", "test", "--fail-fast"],
        &["blaze", "run"],
    ] {
        // The daemon as it is: a failed build's `Done` is not ok. And the planted bug: a daemon
        // that says ok after it named what it left.
        for done in [false, true] {
            let mut api = FakeApi::answering(vec![
                foo_compiling(),
                unchecked(),
                Response::Done { ok: done },
            ]);
            let mut programs = Fake::default();
            let (then, _, err) = driven(&mut api, words, &mut programs);
            assert_eq!(then, Then::Exit(101), "{words:?}, Done {{ ok: {done} }}: {err}");
            assert!(programs.ran.is_empty(), "{words:?}: nothing built, nothing runs");
            assert!(
                err.contains("2 units not checked, behind what failed:\n")
                    && err.contains("behind dep v0.1.0 (lib) (errors in its code): 2 units\n")
                    && err.contains("foo v0.1.0 (bin \"foo\")"),
                "{words:?}: {err}"
            );
            assert_eq!(
                err.contains("the daemon said the build passed after it left 2 unit(s)"),
                done,
                "{words:?}: {err}"
            );
        }
    }
}

#[test]
fn by_default_the_tests_that_built_run_past_a_failed_unit() {
    // A unit failed (the daemon said which, by name) and the rest built: their tests still run,
    // and the run fails. `--keep-going` is the same, a no-op.
    let failed = |it: Vec<Response>| {
        it.into_iter()
            .map(|it| {
                if matches!(it, Response::Done { .. }) { Response::Done { ok: false } } else { it }
            })
            .collect()
    };
    for words in [&["blaze", "test"][..], &["blaze", "test", "--keep-going"]] {
        let mut api = FakeApi::answering(failed(tests_built()));
        let mut programs = Fake::default();
        let (then, _, err) = driven(&mut api, words, &mut programs);
        assert_eq!(then, Then::Exit(101), "{words:?}");
        assert_eq!(programs.ran.len(), 4, "{words:?}: {err}");
        assert!(
            err.contains("     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)"),
            "{err}"
        );
    }

    // Under `--fail-fast` nothing runs.
    let mut api = FakeApi::answering(failed(tests_built()));
    let mut programs = Fake::default();
    let (then, _, _) = driven(&mut api, &["blaze", "test", "--fail-fast"], &mut programs);
    assert_eq!(then, Then::Exit(101));
    assert!(programs.ran.is_empty());
}

#[test]
fn run_becomes_its_program() {
    let foo = crate::cli::run::tests::artifact("foo", "bin", "foo", false, Some("foo"));
    let built = Response::Built {
        members: 1,
        artifacts: vec![foo],
        profile: "`dev` profile [unoptimized + debuginfo]".to_owned(),
    };
    let mut api = FakeApi::answering(vec![built, Response::Done { ok: true }]);
    let (then, _, err) =
        driven(&mut api, &["blaze", "run", "--release", "a", "--", "b"], &mut Fake::default());
    let Then::Exec(exec) = then else { panic!("{then:?}: {err}") };
    assert_eq!(exec.program, PathBuf::from("target/debug/foo"));
    assert_eq!(exec.args, [OsString::from("a"), OsString::from("b")]);
    assert_eq!(exec.cwd, PathBuf::from("/w"));
    // The program's command line is said only under `-v`: what a `run` shows is the build's
    // one line, which names the program it placed.
    assert_eq!(err, "blaze: ok built=0 cached=0 warnings=0 elapsed_ms=0 program=/w/target/debug/foo\n");

    let two = [
        crate::cli::run::tests::artifact("foo", "bin", "foo", false, Some("foo")),
        crate::cli::run::tests::artifact("foo", "bin", "bar", false, Some("bar")),
    ];
    let built = Response::Built {
        members: 1,
        artifacts: two.to_vec(),
        profile: "`dev` profile [unoptimized + debuginfo]".to_owned(),
    };
    let mut api = FakeApi::answering(vec![built, Response::Done { ok: true }]);
    let (then, _, err) = driven(&mut api, &["blaze", "run", "-q"], &mut Fake::default());
    assert_eq!(then, Then::Exit(101));
    assert!(
        err.starts_with("error: `cargo run` could not determine which binary to run."),
        "{err}"
    );
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
            blaze_api::DocumentedCrate {
                name: "foo".into(),
                package: "foo v0.1.0 (/w/foo)".into(),
                asked: true,
            },
            blaze_api::DocumentedCrate {
                name: "dep".into(),
                package: "dep v1.0.0".into(),
                asked: false,
            },
        ];
        Response::Documented(blaze_api::Documented {
            dir: dir.display().to_string(),
            crates,
            pages: 2,
            source_pages: 0,
        })
    };
    let built = || Response::Built {
        members: 1,
        artifacts: Vec::new(),
        profile: "`dev` profile [unoptimized + debuginfo]".to_owned(),
    };
    let responses = vec![
        built(),
        documented(&dir),
        Response::DocPage { path: "foo/index.html".into(), page: b"<html>foo</html>".to_vec() },
        Response::DocPage { path: "dep/index.html".into(), page: b"dep".to_vec() },
        Response::Done { ok: true },
    ];
    let (then, out, err) =
        driven(&mut FakeApi::answering(responses.clone()), &["blaze", "doc"], &mut Fake::default());
    assert_eq!(then, Then::Exit(0));
    assert_eq!(out, "");
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(
        lines[..2],
        [" Documenting foo v0.1.0 (/w/foo)", " Documenting dep v1.0.0"],
        "{err}"
    );
    assert!(lines[2].starts_with("blaze: ok built=0 cached=0 "), "{err}");
    assert_eq!(lines[3], format!("   Generated {}", dir.join("foo/index.html").display()), "{err}");
    assert_eq!(std::fs::read_to_string(dir.join("foo/index.html")).unwrap(), "<html>foo</html>");
    assert_eq!(std::fs::read_to_string(dir.join("dep/index.html")).unwrap(), "dep");
    let listed = std::fs::read_to_string(dir.join("crates.js")).unwrap();
    assert_eq!(
        listed, "window.BLAZE_CRATES = [\"dep\",\"foo\",\"old\"];\n",
        "an earlier crate whose pages are gone is not listed"
    );

    let (_, _, quiet) =
        driven(&mut FakeApi::answering(responses), &["blaze", "doc", "-q"], &mut Fake::default());
    assert_eq!(quiet, "", "cargo's status lines are silent under -q");

    let out_of_it = dir.join("inner");
    let escaping = vec![
        built(),
        documented(&out_of_it),
        Response::DocPage { path: "../escaped.html".into(), page: b"x".to_vec() },
        Response::Done { ok: true },
    ];
    let (then, _, err) =
        driven(&mut FakeApi::answering(escaping), &["blaze", "doc"], &mut Fake::default());
    assert_eq!(then, Then::Exit(Exit::Failed.code()));
    assert!(
        err.contains("the daemon sent the page `../escaped.html`, which is not under"),
        "{err}"
    );
    assert!(
        !dir.join("escaped.html").exists(),
        "nothing is written outside the documentation's directory"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The words of `typed` (after the program's name) as the thin `blaze` reads them.
fn thin_asked(typed: &[&str]) -> blaze_thin::route::Asked {
    let words: Vec<OsString> = typed.iter().map(OsString::from).collect();
    match blaze_thin::route::words(&words) {
        blaze_thin::route::Route::Cargo(asked) => asked,
        other => panic!("{typed:?} is not the thin blaze's: {other:?}"),
    }
}

/// The invariant: for every command line the thin `blaze` sends itself (blaze-thin's
/// `route`), its request is this CLI's, byte for byte in the API's deterministic CBOR, and it
/// runs the program with the arguments this CLI's parser gives it. So the daemon builds one
/// thing whichever sent it, and the engine can go on with an answer the thin `blaze` began
/// (`resume`, which refuses any other request).
#[test]
fn the_thin_blaze_sends_this_clis_request_byte_for_byte() {
    let env = vec![("HOME".to_owned(), "/u".to_owned()), ("PATH".to_owned(), "/usr/bin".to_owned())];
    let files = vec![("/u/.cargo/config.toml".to_owned(), b"[build]\n".to_vec())];
    let table: &[&[&str]] = &[
        &["build"],
        &["b"],
        &["build", "-q"],
        &["build", "--quiet", "-r"],
        &["build", "--release"],
        &["run"],
        &["r", "-q"],
        &["run", "-q", "a", "--", "b"],
        &["run", "--release", "--", "--x", "--", "y"],
        &["run", "-r", "-q", "-"],
    ];
    let frame = |it: &Request| blaze_api::frames::encode(&blaze_api::Envelope::new(2, it)).unwrap();
    for typed in table {
        let thin = thin_asked(typed);
        let asked = invocation(typed.iter().map(OsString::from).collect());
        for (color, width) in [(false, None), (true, Some(100))] {
            let cargo =
                place::cargo(&asked, &place(), env.clone(), files.clone(), color, width).unwrap();
            let cli = request(&asked, cargo).unwrap();
            let sent = blaze_thin::request::Sent {
                cwd: "/w".to_owned(),
                root: "/w".to_owned(),
                env: env.clone(),
                files: files.clone(),
                color,
                width,
            };
            let thin_request = blaze_thin::request::request(&thin, sent);
            assert_eq!(frame(&thin_request), frame(&cli), "{typed:?}");
        }
        assert_eq!(thin.program, asked.program, "{typed:?}");
        assert_eq!(thin.quiet, asked.flags.quiet, "{typed:?}");
        assert_eq!(thin.command.name(), asked.command().name(), "{typed:?}");
    }
}

/// The control of the test above: a request that differs in one word is other bytes, so the
/// equality there is of the whole request.
#[test]
fn a_request_of_other_words_is_other_bytes() {
    let frame = |it: &Request| blaze_api::frames::encode(&blaze_api::Envelope::new(2, it)).unwrap();
    let quiet = request_of(&invocation(os(&["build", "-q"]).collect()));
    let long = request_of(&invocation(os(&["build", "--quiet"]).collect()));
    assert_ne!(frame(&quiet), frame(&long));
}

/// Every setting the daemon keeps for its life is read from a variable the thin `blaze`
/// leaves to the engine (blaze-thin's `route::refuses`): the engine's check that the daemon
/// keeps what the command sets is never skipped for a command that sets one.
#[test]
fn every_setting_the_daemon_keeps_is_a_variable_the_thin_blaze_leaves_to_the_engine() {
    for name in crate::serve::settings::STARTED_WITH {
        assert!(blaze_thin::route::refuses(name), "{name}");
    }
    for name in [client_rows::PROFILE, client_rows::LAUNCHED, blaze_core::cores::VAR, "BLAZE_STATS", "BLAZE_UI"] {
        assert!(blaze_thin::route::refuses(name), "{name}");
    }
}

/// A line of cargo's with its time taken out: `Finished ... in <time>` and what follows it.
fn untimed(said: &str) -> String {
    let lines = said.lines().map(|line| match line.split_once(" target(s) in ") {
        Some((before, after)) => {
            let rest = after.split_once('s').map_or("", |(_, rest)| rest);
            format!("{before} target(s) in <time>{rest}")
        }
        None => line.to_owned(),
    });
    lines.collect::<Vec<_>>().join("\n")
}

/// A build's answer with nothing to do: every unit fresh, the outputs, the work, the `Done`.
fn plain_answer() -> Vec<Response> {
    let foo = crate::cli::run::tests::artifact("foo", "bin", "foo", false, Some("foo"));
    let fresh = Response::Unit(UnitStarted {
        package_id: "path+file:///w/foo#0.1.0".to_owned(),
        name: "foo".to_owned(),
        version: "0.1.0".to_owned(),
        source: Some("(/w/foo)".to_owned()),
        verb: Verb::Fresh,
        package: 0,
    });
    let built = Response::Built {
        members: 1,
        artifacts: vec![foo],
        profile: "`dev` profile [unoptimized + debuginfo]".to_owned(),
    };
    let worked = Response::Work(blaze_api::Worked {
        why: None,
        storage: None,
        tiers: Some(blaze_api::Tiers::of(blaze_api::Goal::Edit).with(38, 100)),
        link: blaze_api::LinkKind::Patched,
        found: 1,
        green: 4,
        links: vec![blaze_api::Linked {
            how: blaze_api::LinkHow::Unchanged,
            path: "/w/target/debug/foo".to_owned(),
        }],
        reran: 0,
        misses: 0,
        served: Vec::new(),
        build_id: None,
        compiled: 0,
        critical_path_us: 0,
        steps_us: Vec::new(),
        placed: blaze_api::PlacedBytes::default(),
    });
    vec![fresh, built, worked, Response::Done { ok: true }]
}

/// The invariant: what the thin `blaze` prints of an answer it takes as plain
/// (blaze-thin's `answer`) is what this CLI prints of the same responses, and the program it
/// runs is the one this CLI runs, from the same directory with the same arguments and
/// variables. Anything it does not take as plain it hands to this CLI, so the two never print
/// one answer two ways.
#[test]
fn the_thin_blaze_prints_a_plain_answer_as_this_cli_does() {
    use blaze_thin::answer::{Exec, Plain, Step};
    let table: &[&[&str]] =
        &[&["build"], &["build", "-q"], &["run"], &["run", "-q"], &["run", "a", "--", "b=1"]];
    for typed in table {
        let thin = thin_asked(typed);
        let mut plain = Plain::new();
        let steps: Vec<Step> = plain_answer().into_iter().map(|it| plain.take(it)).collect();
        assert_eq!(steps, [Step::More, Step::More, Step::More, Step::Done], "{typed:?}");
        let options = blaze_screen::Options {
            quiet: thin.quiet,
            ..blaze_screen::Options::of(match thin.command {
                blaze_thin::route::Command::Build => blaze_screen::Command::Build,
                blaze_thin::route::Command::Run => blaze_screen::Command::Run,
            })
        };
        let said = plain.shown(
            options,
            blaze_screen::Caps::PIPE,
            &blaze_report::terminal::Terminal::PIPE,
            std::time::Duration::ZERO,
            None,
        );

        let mut api = FakeApi::answering(plain_answer());
        let mut words = vec!["blaze"];
        words.extend(typed.iter());
        let (then, out, err) = driven(&mut api, &words, &mut Fake::default());
        assert_eq!(out, "", "{typed:?}");
        match thin.command {
            blaze_thin::route::Command::Build => assert_eq!(then, Then::Exit(0), "{typed:?}"),
            blaze_thin::route::Command::Run => {
                let program = plain.program().expect("the one bin runs");
                let exec = Exec::of(
                    program.executable.as_deref().unwrap(),
                    &program.env,
                    &thin.program,
                    std::path::Path::new("/w"),
                );
                let Then::Exec(cli) = then else { panic!("{typed:?}: {then:?}: {err}") };
                assert_eq!(
                    (exec.program, exec.args, exec.env, exec.cwd),
                    (cli.program, cli.args, cli.env, cli.cwd),
                    "{typed:?}"
                );
            }
        }
        assert_eq!(untimed(&said), untimed(&err), "{typed:?}");
    }
}

/// An edit's answer: a unit compiled and ended with no diagnostic, the outputs, the work with
/// a step computed again and its image spliced, the `Done`.
fn built_answer() -> Vec<Response> {
    let mut answer = plain_answer();
    answer[0] = foo_compiling();
    let Response::Work(worked) = &mut answer[2] else { panic!("the work is third") };
    worked.misses = 1;
    worked.found = 0;
    worked.compiled = 1;
    worked.links[0].how = blaze_api::LinkHow::Spliced;
    let foo = crate::cli::run::tests::artifact("foo", "bin", "foo", false, Some("foo"));
    let ended = Response::UnitEnded(blaze_api::UnitOf {
        id: 0,
        test: false,
        target: foo.target,
        doctest: false,
        package_id: "path+file:///w/foo#0.1.0".to_owned(),
        manifest_path: "/w/foo/Cargo.toml".to_owned(),
    });
    answer.insert(1, ended);
    answer
}

/// The invariant of the test above for an edit's answer (a unit compiled, its image spliced):
/// for a `run` and a quiet `build`, which say nothing of what the build recomputed, the thin
/// `blaze` prints what this CLI prints of the same responses and runs the program it runs.
/// The control: a `build` that is not quiet says why it recomputed, which is this CLI's
/// printer's, and the thin `blaze` hands that answer over at its first unit.
#[test]
fn the_thin_blaze_prints_an_edits_answer_as_this_cli_does() {
    use blaze_thin::answer::{Exec, Plain, Step};
    let table: &[&[&str]] = &[&["build", "-q"], &["run"], &["run", "-q"], &["run", "a", "--", "b=1"]];
    for typed in table {
        let thin = thin_asked(typed);
        let reports = thin.command == blaze_thin::route::Command::Build && !thin.quiet;
        let mut plain = Plain::of(reports);
        let steps: Vec<Step> = built_answer().into_iter().map(|it| plain.take(it)).collect();
        assert_eq!(steps, [Step::More, Step::More, Step::More, Step::More, Step::Done], "{typed:?}");
        let mut said = plain.said(thin.quiet, false, std::time::Duration::ZERO);

        let mut api = FakeApi::answering(built_answer());
        let mut words = vec!["blaze"];
        words.extend(typed.iter());
        let (then, out, err) = driven(&mut api, &words, &mut Fake::default());
        assert_eq!(out, "", "{typed:?}");
        match thin.command {
            blaze_thin::route::Command::Build => assert_eq!(then, Then::Exit(0), "{typed:?}"),
            blaze_thin::route::Command::Run => {
                let program = plain.program().expect("the one bin runs");
                let exec = Exec::of(
                    program.executable.as_deref().unwrap(),
                    &program.env,
                    &thin.program,
                    std::path::Path::new("/w"),
                );
                if !thin.quiet {
                    said.push_str(&exec.running(false));
                }
                let Then::Exec(cli) = then else { panic!("{typed:?}: {then:?}: {err}") };
                assert_eq!(
                    (exec.program, exec.args, exec.env, exec.cwd),
                    (cli.program, cli.args, cli.env, cli.cwd),
                    "{typed:?}"
                );
            }
        }
        assert_eq!(untimed(&said), untimed(&err), "{typed:?}");
        if !thin.quiet {
            assert!(said.starts_with("   Compiling foo v0.1.0 (/w/foo)\nlink: /w/target/debug/foo (spliced)\n"), "{said}");
        }
    }
    let build = thin_asked(&["build"]);
    assert!(build.command == blaze_thin::route::Command::Build && !build.quiet);
    let first = built_answer().into_iter().next().expect("a unit");
    assert!(matches!(Plain::of(true).take(first), Step::HandOver(_)));
}

/// The control of the tests above, and the other half of the thin `blaze`'s rule: each response
/// this CLI's printer says more of than cargo's status lines is one the thin `blaze` does not
/// take, so it is this CLI that prints it.
#[test]
fn what_this_clis_printer_says_more_of_the_thin_blaze_hands_over() {
    use blaze_thin::answer::{Plain, Step};
    let handed = |response: Response| matches!(Plain::new().take(response), Step::HandOver(_));
    assert!(matches!(Plain::of(true).take(foo_compiling()), Step::HandOver(_)));
    assert!(handed(Response::Done { ok: false }));
    assert!(handed(Response::Error(blaze_api::ApiError::new("no", None))));
    assert!(handed(Response::Diagnostic(blaze_api::Diagnostic::note("said".to_owned()))));
    let mut recomputed = plain_answer();
    let Response::Work(worked) = &mut recomputed[2] else { panic!("the work is third") };
    worked.misses = 1;
    let mut plain = Plain::of(true);
    let steps: Vec<bool> =
        recomputed.into_iter().map(|it| matches!(plain.take(it), Step::HandOver(_))).collect();
    assert_eq!(steps[..3], [false, false, true]);
}
