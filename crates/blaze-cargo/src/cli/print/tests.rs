use std::time::Duration;

use blaze_api::{
    ApiError, Artifact, ArtifactKind, Diagnostic, Level, Response, Span, UnitOf, UnitProfile, UnitStarted, Verb,
};
use blaze_ids::ContentId;

use super::*;
use crate::cli::say::tests::target;

/// `response` printed as the client reads it: from the bytes the daemon sends, in place.
fn on<O: Write, E: Write>(printer: &mut Printer<'_, O, E>, response: &Response) -> io::Result<()> {
    let bytes = blaze_protocol::cbor::to_vec(response).unwrap();
    printer.on(blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(&bytes).unwrap())
}

/// The daemon's index of the package and of its lib unit, by name: `foo` 0, `bar` 1, `p<n>`
/// `n + 2`.
fn id_of(name: &str) -> u32 {
    match name {
        "foo" => 0,
        "bar" => 1,
        other => other[1..].parse::<u32>().unwrap() + 2,
    }
}

#[test]
fn message_formats_as_cargo_reads_them() {
    let format = |values: &[&str]| super::format(&values.iter().map(|it| it.to_string()).collect::<Vec<_>>());
    assert_eq!(format(&[]), Ok(Format::Human));
    assert_eq!(format(&["short"]), Ok(Format::Short));
    assert_eq!(format(&["JSON"]), Ok(Format::Json { short: false, ansi: false, render_diagnostics: false }));
    assert_eq!(
        format(&["json-diagnostic-short,json-render-diagnostics"]),
        Ok(Format::Json { short: true, ansi: false, render_diagnostics: true })
    );
    assert_eq!(
        format(&["json", "json-diagnostic-rendered-ansi"]),
        Ok(Format::Json { short: false, ansi: true, render_diagnostics: false })
    );
    let two = Err("cannot specify two kinds of `message-format` arguments".to_owned());
    assert_eq!(format(&["json,human"]), two);
    assert_eq!(format(&["short", "json-render-diagnostics"]), two);
    assert_eq!(format(&["xml"]), Err("invalid message format specifier: `xml`".to_owned()));
}

fn started(name: &str, verb: Verb) -> Response {
    Response::Unit(UnitStarted {
        package_id: format!("path+file:///w/{name}#0.1.0"),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        source: Some(format!("(/w/{name})")),
        verb,
        package: id_of(name),
    })
}

fn lib_of(name: &str) -> UnitOf {
    UnitOf {
        id: id_of(name),
        package_id: format!("path+file:///w/{name}#0.1.0"),
        manifest_path: format!("/w/{name}/Cargo.toml"),
        target: target("lib", name),
        test: false,
    }
}

fn diagnostic(level: Level, message: &str, rendered: &str, unit: Option<UnitOf>) -> Diagnostic {
    Diagnostic {
        level,
        code: None,
        message: message.to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered: rendered.to_owned(),
        unit,
        explanation: None,
    }
}

fn fixable(mut warning: Diagnostic) -> Diagnostic {
    let span = Span {
        file: "src/lib.rs".to_owned(),
        line: 1,
        column: 5,
        end_line: 1,
        end_column: 6,
        label: None,
        byte_start: 4,
        byte_end: 5,
        primary: true,
        replacement: Some("_x".to_owned()),
        applicability: Some(Applicability::MachineApplicable),
    };
    let mut help = diagnostic(Level::Help, "prefix it with an underscore", "", None);
    help.spans.push(span);
    warning.children.push(help);
    warning
}

fn built() -> Response {
    let artifact = Artifact {
        package: "path+file:///w/foo#0.1.0".to_owned(),
        kind: ArtifactKind::Lib,
        path: "/w/foo/target/debug/libfoo.rlib".to_owned(),
        content: ContentId([0; 32]),
        manifest_path: "/w/foo/Cargo.toml".to_owned(),
        target: target("lib", "foo"),
        profile: UnitProfile {
            opt_level: "0".to_owned(),
            debuginfo: 2,
            debug_assertions: true,
            overflow_checks: true,
            test: false,
        },
        features: Vec::new(),
        filenames: vec!["/w/foo/target/debug/libfoo.rlib".to_owned()],
        executable: None,
        fresh: false,
        runner: Vec::new(),
        env: Vec::new(),
        harness: true,
    };
    Response::Built { members: 1, artifacts: vec![artifact], profile: "`dev` profile [unoptimized + debuginfo]".to_owned() }
}

/// What the printer wrote to stdout and stderr of `responses`, ended with `ok`.
fn printed(shown: Shown, responses: &[Response], ok: bool) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut printer = Printer::new(shown, Path::new("/w"), &mut out, &mut err);
    for response in responses {
        on(&mut printer, response).unwrap();
    }
    printer.finish(ok, Duration::from_millis(1234)).unwrap();
    drop(printer);
    (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

const UNUSED: &str = "warning: unused variable: `x`\n --> src/lib.rs:1:5\n\n";

#[test]
fn a_build_with_warnings_as_cargo_prints_it() {
    let warning = fixable(diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo"))));
    let emitted = diagnostic(Level::Warning, "2 warnings emitted", "warning: 2 warnings emitted\n\n", Some(lib_of("foo")));
    let responses = [
        started("bar", Verb::Fresh),
        started("foo", Verb::Compiling),
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning.clone()),
        Response::Diagnostic(warning),
        Response::Diagnostic(emitted),
        built(),
        Response::Done { ok: true },
    ];
    let (out, err) = printed(Shown::default(), &responses, true);
    assert_eq!(out, "");
    assert_eq!(
        err,
        format!(
            "   Compiling foo v0.1.0 (/w/foo)\n{UNUSED}warning: `foo` (lib) generated 2 warnings (1 duplicate) \
             (run `cargo fix --lib -p foo` to apply 1 suggestion)\n    Finished `dev` profile \
             [unoptimized + debuginfo] target(s) in 1.23s\n"
        )
    );

    let (_, verbose) = printed(Shown { verbose: 1, ..Shown::default() }, &responses, true);
    assert!(verbose.starts_with("       Fresh bar v0.1.0 (/w/bar)\n   Compiling foo v0.1.0 (/w/foo)\n"), "{verbose}");

    let (_, quiet) = printed(Shown { quiet: true, ..Shown::default() }, &responses, true);
    assert_eq!(quiet, UNUSED);
}

#[test]
fn a_units_summary_comes_as_it_ends_and_once() {
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    let responses = [
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning),
        Response::UnitEnded(lib_of("foo")),
        started("bar", Verb::Compiling),
        Response::UnitEnded(lib_of("bar")),
        built(),
        Response::Done { ok: true },
    ];
    let (_, err) = printed(Shown::default(), &responses, true);
    assert_eq!(
        err,
        format!(
            "   Compiling foo v0.1.0 (/w/foo)\n{UNUSED}warning: `foo` (lib) generated 1 warning\n   Compiling bar \
             v0.1.0 (/w/bar)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s\n"
        )
    );
}

#[test]
fn what_a_request_did_is_one_json_line_of_blazes() {
    let worked = blaze_api::Worked {
        link: blaze_api::LinkKind::Patched,
        found: 3,
        green: 40,
        links: vec![blaze_api::Linked { how: blaze_api::LinkHow::Spliced, path: "target/debug/foo".to_owned() }],
        reran: 2,
        served: Vec::new(),
        compiled: 1,
        critical_path_us: 1500,
        steps_us: vec![(blaze_api::NodeKind::Link, 40)],
    };
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let (out, err) = printed(json, &[Response::Work(worked.clone()), Response::Done { ok: true }], true);
    assert_eq!(
        out,
        "{\"reason\": \"blaze-work\", \"reran\": 2, \"green\": 40, \"compiled\": 1, \"found\": 3, \"link\": \
         \"patched\", \"critical_path_us\": 1500, \"links\": [{\"path\": \"target/debug/foo\", \"how\": \
         \"spliced\", \"why\": []}], \"steps_us\": {\"link\": 40}}\n{\"reason\":\"build-finished\",\"success\":true}\n"
    );
    assert!(err.starts_with("link: target/debug/foo (spliced)\n"), "{err}");
    let (out, err) = printed(Shown::default(), &[Response::Work(worked.clone()), Response::Done { ok: true }], true);
    assert_eq!(out, "", "under the human format the work line is not printed");
    assert!(err.starts_with("link: target/debug/foo (spliced)\n"), "but how it linked is: {err}");
    assert!(!err.contains("timeline-us:"), "{err}");
    let stats = Shown { stats: true, ..Shown::default() };
    let (_, err) = printed(stats, &[Response::Work(worked), Response::Done { ok: true }], true);
    assert!(err.starts_with("link: target/debug/foo (spliced)\ntimeline-us: link=40\n"), "{err}");
}

#[test]
fn each_dependency_crate_is_one_served_line_after_the_work() {
    use blaze_api::{ServedCrate, ServedPart};
    let worked = blaze_api::Worked {
        link: blaze_api::LinkKind::Placed,
        found: 0,
        green: 0,
        links: Vec::new(),
        reran: 1,
        served: vec![
            ServedCrate { why: None, bodies: ServedPart::Served, outcome: ServedPart::Served, package: "itoa".to_owned() },
            ServedCrate {
                why: Some("the \"std\" feature".to_owned()),
                bodies: ServedPart::Refused,
                outcome: ServedPart::Checked,
                package: "serde".to_owned(),
            },
        ],
        compiled: 0,
        critical_path_us: 7,
        steps_us: Vec::new(),
    };
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let (out, _) = printed(json, &[Response::Work(worked.clone()), Response::Done { ok: true }], true);
    assert_eq!(
        out,
        concat!(
            "{\"reason\": \"blaze-work\", \"reran\": 1, \"green\": 0, \"compiled\": 0, \"found\": 0, \"link\": \
             \"placed\", \"critical_path_us\": 7, \"links\": [], \"steps_us\": {}}\n",
            "{\"reason\": \"blaze-served\", \"package\": \"itoa\", \"outcome\": \"served\", \"bodies\": \"served\", \
             \"why\": null}\n",
            "{\"reason\": \"blaze-served\", \"package\": \"serde\", \"outcome\": \"checked\", \"bodies\": \"refused\", \
             \"why\": \"the \\\"std\\\" feature\"}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
        )
    );
    let (out, _) = printed(Shown::default(), &[Response::Work(worked), Response::Done { ok: true }], true);
    assert_eq!(out, "", "under the human format none is printed");
}

#[test]
fn a_unit_that_failed() {
    let error = diagnostic(Level::Error, "mismatched types", "error[E0308]: mismatched types\n\n", Some(lib_of("foo")));
    let warning = fixable(diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo"))));
    let aborting = diagnostic(Level::Error, "aborting due to 1 previous error", "error: aborting due to 1 previous error\n\n", Some(lib_of("foo")));
    let explain = diagnostic(
        Level::FailureNote,
        "For more information about this error, try `rustc --explain E0308`.",
        "For more information about this error, try `rustc --explain E0308`.\n",
        Some(lib_of("foo")),
    );
    let responses = [
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning),
        Response::Diagnostic(error),
        Response::Diagnostic(aborting),
        Response::Diagnostic(explain),
        Response::Done { ok: false },
    ];
    let (_, err) = printed(Shown { color: false, ..Shown::default() }, &responses, false);
    assert_eq!(
        err,
        format!(
            "   Compiling foo v0.1.0 (/w/foo)\n{UNUSED}error[E0308]: mismatched types\n\nFor more information \
             about this error, try `rustc --explain E0308`.\nwarning: `foo` (lib) generated 1 warning\nerror: \
             could not compile `foo` (lib) due to 1 previous error; 1 warning emitted\n"
        )
    );
    let (_, quiet) = printed(Shown { color: false, quiet: true, ..Shown::default() }, &responses, false);
    assert!(quiet.ends_with("`rustc --explain E0308`.\nerror: could not compile `foo` (lib) due to 1 previous error; 1 warning emitted\n"), "{quiet}");
}

#[test]
fn levels_in_color_as_cargos_shell_writes_them() {
    let error = diagnostic(Level::Error, "mismatched types", "error[E0308]: mismatched types\n", Some(lib_of("foo")));
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    let responses = [Response::Diagnostic(warning), Response::Diagnostic(error), Response::Done { ok: false }];
    let (_, err) = printed(Shown { color: true, ..Shown::default() }, &responses, false);
    assert!(
        err.ends_with(&format!(
            "{}warning{}: `foo` (lib) generated 1 warning\n{}error{}: could not compile `foo` (lib) due to 1 \
             previous error; 1 warning emitted\n",
            say::WARN,
            say::RESET,
            say::ERROR,
            say::RESET
        )),
        "{err:?}"
    );
}

#[test]
fn rustcs_summary_is_skipped_under_json_too() {
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let emitted = diagnostic(Level::Warning, "1 warning emitted", "warning: 1 warning emitted\n\n", Some(lib_of("foo")));
    let (out, _) = printed(json, &[Response::Diagnostic(emitted), Response::Done { ok: true }], true);
    assert_eq!(out, "{\"reason\":\"build-finished\",\"success\":true}\n");
}

#[test]
fn the_note_to_explain_that_short_leaves_empty_is_not_written() {
    let explain = "For more information about this error, try `rustc --explain E0425`.";
    let short_json = Shown { format: Format::Json { short: true, ansi: false, render_diagnostics: false }, ..Shown::default() };
    for shown in [Shown { format: Format::Short, ..Shown::default() }, short_json] {
        let note = diagnostic(Level::FailureNote, explain, "", Some(lib_of("foo")));
        assert_eq!(printed(shown, &[Response::Diagnostic(note)], true), printed(shown, &[], true));
    }
}

#[test]
fn one_rendered_text_is_printed_once_with_or_without_its_newline() {
    let first = diagnostic(Level::Warning, "unused variable: `x`", "warning: a\n", Some(lib_of("foo")));
    let second = diagnostic(Level::Warning, "unused variable: `x`", "warning: a", Some(lib_of("foo")));
    let responses = [Response::Diagnostic(first), Response::Diagnostic(second), Response::Done { ok: true }];
    let (_, err) = printed(Shown::default(), &responses, true);
    assert_eq!(err, "warning: a\nwarning: `foo` (lib) generated 2 warnings (1 duplicate)\n");
}

#[test]
fn units_of_one_package_are_told_apart() {
    let lib = diagnostic(Level::Warning, "a", "warning: a\n", Some(lib_of("foo")));
    let test = diagnostic(Level::Warning, "a", "warning: b\n", Some(UnitOf { id: 100, test: true, ..lib_of("foo") }));
    let responses = [Response::Diagnostic(lib), Response::Diagnostic(test), Response::Done { ok: true }];
    let (_, err) = printed(Shown::default(), &responses, true);
    assert_eq!(
        err,
        "warning: a\nwarning: b\nwarning: `foo` (lib) generated 1 warning\nwarning: `foo` (lib test) generated 1 warning\n"
    );
}

/// Each `write` call, with whether it went to stdout.
struct Logged<'a> {
    out: bool,
    log: &'a std::cell::RefCell<Vec<(bool, String)>>,
}

impl Write for Logged<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.log.borrow_mut().push((self.out, String::from_utf8(bytes.to_vec()).unwrap()));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn lines_of_one_stream_in_one_write_and_both_in_the_order_made() {
    let log = std::cell::RefCell::new(Vec::new());
    let (mut out, mut err) = (Logged { out: true, log: &log }, Logged { out: false, log: &log });
    let shown = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: true }, ..Shown::default() };
    let mut printer = Printer::new(shown, Path::new("/w"), &mut out, &mut err);
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    for response in [started("foo", Verb::Compiling), started("bar", Verb::Compiling), built(), Response::Diagnostic(warning)] {
        on(&mut printer, &response).unwrap();
    }
    printer.finish(true, Duration::from_millis(1234)).unwrap();
    drop(printer);
    let log = log.into_inner();
    let streams: Vec<bool> = log.iter().map(|it| it.0).collect();
    assert_eq!(streams, [false, true, false, true], "{log:#?}");
    assert_eq!(log[0].1, "   Compiling foo v0.1.0 (/w/foo)\n   Compiling bar v0.1.0 (/w/bar)\n");
    assert!(log[1].1.starts_with("{\"reason\":\"compiler-artifact\""), "{}", log[1].1);
    assert!(log[2].1.starts_with("warning: unused variable") && log[2].1.ends_with("Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s\n"), "{}", log[2].1);
    assert_eq!(log[3].1, "{\"reason\":\"build-finished\",\"success\":true}\n");
}

/// A build's responses: each unit starting, warnings, artifacts, a unit ending.
fn a_build(n: usize) -> Vec<Response> {
    let mut responses = Vec::new();
    for at in 0..n {
        let name = format!("p{at}");
        responses.push(started(&name, Verb::Compiling));
        let warning = fixable(diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of(&name))));
        responses.push(Response::Diagnostic(warning.clone()));
        responses.push(Response::Diagnostic(warning));
        responses.push(Response::UnitEnded(lib_of(&name)));
    }
    responses.push(Response::Fetched { verb: FetchVerb::Downloaded, what: "serde v1.0.219".to_owned() });
    responses.push(Response::Error(ApiError::new("no package `nope` in the workspace", None)));
    responses
}

/// The printer allocates per request, not per response: once its buffers have grown and the
/// units are known, reading a build's responses where they lie in the daemon's bytes and
/// printing them allocates nothing, in every format.
#[test]
fn printing_responses_warm_allocates_nothing() {
    let formats = [
        Format::Human,
        Format::Short,
        Format::Json { short: false, ansi: false, render_diagnostics: false },
        Format::Json { short: false, ansi: true, render_diagnostics: true },
    ];
    let sent: Vec<Vec<u8>> = a_build(64).iter().map(|it| blaze_protocol::cbor::to_vec(it).unwrap()).collect();
    for format in formats {
        for verbose in [0, 1] {
            let shown = Shown { format, verbose, color: true, ..Shown::default() };
            let (mut out, mut err) = (Vec::with_capacity(1 << 20), Vec::with_capacity(1 << 20));
            let mut printer = Printer::new(shown, Path::new("/w"), &mut out, &mut err);
            for bytes in &sent {
                printer.on(blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(bytes).unwrap()).unwrap();
            }
            printer.flush().unwrap();
            printer.printed.clear();
            printer.packages.clear();
            for counted in &mut printer.counted {
                *counted = Counted::default();
            }
            blaze_alloc_count::assert_at_most(0, "printing a warm build's responses", || {
                for bytes in &sent {
                    printer.on(blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(bytes).unwrap()).unwrap();
                }
            });
            assert!(!printer.refused());
        }
    }
}

#[test]
fn a_unit_or_package_numbered_past_any_request_fails_the_build_naming_it() {
    let far = UnitOf { id: u32::MAX, ..lib_of("foo") };
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(far));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut printer = Printer::new(Shown::default(), Path::new("/w"), &mut out, &mut err);
    on(&mut printer, &Response::Diagnostic(warning)).unwrap();
    assert!(printer.refused());
    printer.finish(false, Duration::from_millis(1)).unwrap();
    drop(printer);
    let err = String::from_utf8(err).unwrap();
    assert!(err.ends_with("error: the daemon numbered a unit 4294967295, past the 16777216 a request's indexes stay under\n"), "{err}");

    let Response::Unit(unit) = started("foo", Verb::Compiling) else { unreachable!() };
    let (_, err) = printed(Shown::default(), &[Response::Unit(UnitStarted { package: 1 << 24, ..unit })], false);
    assert_eq!(err, "error: the daemon numbered a package 16777216, past the 16777216 a request's indexes stay under\n");
}

#[test]
fn json_on_stdout_and_status_on_stderr() {
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    let responses = [
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning.clone()),
        built(),
        Response::Done { ok: true },
    ];
    let (out, err) = printed(json, &responses, true);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with(r#"{"reason":"compiler-message","package_id":"path+file:///w/foo#0.1.0","#), "{}", lines[0]);
    assert!(lines[0].contains(r#""message":{"rendered":"warning: unused variable: `x`\n --> src/lib.rs:1:5\n\n","#));
    assert!(lines[1].starts_with(r#"{"reason":"compiler-artifact","#));
    assert_eq!(lines[2], r#"{"reason":"build-finished","success":true}"#);
    // No "generated" line: cargo counts a unit's warnings where it renders them.
    assert_eq!(err, "   Compiling foo v0.1.0 (/w/foo)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s\n");

    let rendering = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: true }, ..Shown::default() };
    let (out, err) = printed(rendering, &responses, true);
    assert!(!out.contains("compiler-message"));
    assert!(err.contains(UNUSED));
    assert!(err.contains("warning: `foo` (lib) generated 1 warning\n"));
}

#[test]
fn an_error_of_the_daemon() {
    let responses = [Response::Error(ApiError::new("no package `nope` in the workspace", None)), Response::Done { ok: false }];
    let (out, err) = printed(Shown::default(), &responses, false);
    assert_eq!(out, "");
    assert_eq!(err, "error: no package `nope` in the workspace\n");
}

#[test]
fn what_the_daemon_fetched() {
    let responses = [
        Response::Fetched { verb: FetchVerb::Updating, what: "crates.io index".to_owned() },
        Response::Fetched { verb: FetchVerb::Downloaded, what: "serde v1.0.219".to_owned() },
        Response::Done { ok: false },
    ];
    let (out, err) = printed(Shown::default(), &responses, false);
    assert_eq!(out, "");
    assert_eq!(err, "    Updating crates.io index\n  Downloaded serde v1.0.219\n");
    let (_, quiet) = printed(Shown { quiet: true, ..Shown::default() }, &responses, false);
    assert_eq!(quiet, "");
}
