use blaze_ids::ContentId;
use blaze_query::{QueryKey, QueryKind};

use super::*;
use crate::{
    Envelope, LONGEST_REQUEST, Response,
    frames::{self, Frames},
};

fn target() -> UnitTarget {
    UnitTarget {
        doc: true,
        kind: TargetKind::Lib,
        name: "app".to_owned(),
        test: true,
        doctest: true,
        edition: "2024".to_owned(),
        src_path: "/w/src/lib.rs".to_owned(),
        crate_types: vec![CrateType::Rlib, CrateType::Cdylib],
    }
}

fn span(file: &str, applicability: Option<Applicability>) -> Span {
    Span {
        file: file.to_owned(),
        line: 3,
        label: Some("expected `u32`".to_owned()),
        column: 9,
        primary: true,
        byte_end: 40,
        end_line: 3,
        byte_start: 35,
        end_column: 14,
        replacement: applicability.map(|_| "1u32".to_owned()),
        applicability,
    }
}

/// An error as a build streams it: a unit, facts, spans, a child with a suggestion.
fn diagnostic() -> Diagnostic {
    let help = Diagnostic {
        code: None,
        unit: None,
        facts: Vec::new(),
        level: Level::Help,
        spans: vec![span("src/lib.rs", Some(Applicability::MachineApplicable))],
        message: "change the type".to_owned(),
        children: Vec::new(),
        rendered: String::new(),
        explanation: None,
    };
    Diagnostic {
        code: Some("E0308".to_owned()),
        unit: Some(UnitOf {
            id: 4,
            test: false,
            target: target(),
            package_id: "path+file:///w#app@0.1.0".to_owned(),
            manifest_path: "/w/Cargo.toml".to_owned(),
        }),
        facts: vec![FactRead {
            key: QueryKey { kind: QueryKind(0x1000), input: ContentId([7; 32]) },
            what: "the signature of `app::f`".to_owned(),
        }],
        level: Level::Error,
        spans: vec![span("src/lib.rs", None), span("src/main.rs", None)],
        message: "mismatched types".to_owned(),
        children: vec![help],
        rendered: "error[E0308]: mismatched types\n".to_owned(),
        explanation: Some("Expected type did not match the received type.\n".to_owned()),
    }
}

/// A response's strings are read from the frame's bytes, and its lists from their items as
/// they are iterated.
#[test]
fn a_diagnostic_is_read_where_it_lies() {
    let sent = Envelope::new(5, Response::Diagnostic(diagnostic()));
    let item = blaze_protocol::cbor::to_vec(&sent).unwrap();
    let read = frames::decode::<ResponseRef<'_>>(&item).unwrap();
    assert_eq!(read.id, 5);
    let ResponseRef::Diagnostic(lent) = read.body else { panic!("{:?}", read.body) };
    let within = item.as_ptr_range();
    let unit = lent.unit.unwrap();
    for text in [lent.message, lent.rendered, unit.package_id, unit.target.name, lent.explanation.unwrap()] {
        assert!(within.contains(&text.as_ptr()), "{text}");
    }
    assert_eq!(lent.spans.len(), 2);
    assert!(lent.spans.iter().all(|it| within.contains(&it.file.as_ptr())));
    assert_eq!(unit.target.kinds().collect::<Vec<_>>(), ["rlib", "cdylib"]);
    let help = lent.children.first().unwrap();
    assert_eq!(help.spans.first().unwrap().applicability, Some(Applicability::MachineApplicable));
    assert_eq!(read.body.into_owned(), sent.body);
}

/// The way from a frame's bytes to the last field a printer reads, warm, allocates nothing:
/// the cut, the decode, each list's items and the strings.
#[test]
fn a_streamed_response_read_in_place_allocates_nothing() {
    let responses = [
        Response::Diagnostic(diagnostic()),
        Response::UnitEnded(diagnostic().unit.unwrap()),
        Response::Unit(UnitStarted {
            name: "app".to_owned(),
            verb: Verb::Compiling,
            source: Some("(/w)".to_owned()),
            package: 2,
            version: "0.1.0".to_owned(),
            package_id: "path+file:///w#app@0.1.0".to_owned(),
        }),
        Response::Fetched { verb: FetchVerb::Downloaded, what: "serde v1.0.219".to_owned() },
        Response::BuildScript(BuildScriptRan {
            env: vec![("A".to_owned(), "1".to_owned())],
            cfgs: vec!["has_x".to_owned()],
            out_dir: "/t/out".to_owned(),
            package_id: "path+file:///w#app@0.1.0".to_owned(),
            linked_libs: vec!["z".to_owned()],
            linked_paths: Vec::new(),
        }),
        Response::TestResult(TestResult {
            name: "tests::parses".to_owned(),
            binary: "/t/debug/deps/app-1234".to_owned(),
            cached: Some(Cached {
                basis: Basis::Reached { why: "its code did not change".to_owned() },
                millis: 3,
                outcome: TestOutcome::Passed,
            }),
            millis: 0,
            output: b"ok\n".to_vec(),
            outcome: TestOutcome::Unreached,
            outside: false,
            executable: 1,
        }),
        Response::Ran { code: Some(0), stderr: b"warn\n".to_vec(), stdout: b"hi\n".to_vec() },
        Response::Error(ApiError::new("no such head", Some("main".to_owned()))),
        Response::Done { ok: false },
    ];
    let mut stream = Vec::new();
    for (id, response) in responses.into_iter().enumerate() {
        stream.extend(frames::encode(&Envelope::new(id as u64, response)).unwrap());
    }
    let mut frames = Frames::new();
    let mut round = || {
        frames.push(&stream);
        let mut read = 0usize;
        while let Some(item) = frames.next_ref().unwrap() {
            let envelope = frames::decode::<ResponseRef<'_>>(item).unwrap();
            read += match envelope.body {
                ResponseRef::Diagnostic(it) => {
                    let spans: usize = it.spans.iter().map(|span| span.file.len()).sum();
                    let children: usize = it.children.iter().map(|child| child.spans.len()).sum();
                    let facts: usize = it.facts.iter().map(|fact| fact.what.len()).sum();
                    let kinds = it.unit.map_or(0, |unit| unit.target.kinds().count());
                    it.rendered.len() + spans + children + facts + kinds
                }
                ResponseRef::UnitEnded(it) => it.package_id.len() + it.target.crate_types.iter().count(),
                ResponseRef::Unit(it) => it.name.len() + it.package as usize,
                ResponseRef::Fetched { what, .. } => what.len(),
                ResponseRef::BuildScript(it) => it.env.iter().count() + it.cfgs.iter().map(str::len).sum::<usize>(),
                ResponseRef::TestResult(it) => it.output.len() + it.executable as usize,
                ResponseRef::Ran { stderr, stdout, .. } => stderr.len() + stdout.len(),
                ResponseRef::Error(it) => it.what.len(),
                ResponseRef::Done { ok } => usize::from(ok),
                other => panic!("{other:?}"),
            };
        }
        read
    };
    let first = round();
    let again = blaze_alloc_count::assert_at_most(0, "a warm read of streamed responses in place", || {
        (0..50).map(|_| round()).sum::<usize>()
    });
    assert_eq!(again, 50 * first);
}

/// Nesting past the decoder's depth is refused in place as it is owned, at the same depth.
#[test]
fn nesting_is_refused_in_place_where_it_is_owned() {
    // A 300-deep value is built, encoded, compared and dropped by recursion: in a debug build
    // that needs more than a test thread's 2 MiB, so it runs on a worker's stack (64 MiB,
    // `blaze-graph`'s pool).
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(nesting_at_each_depth)
        .unwrap()
        .join()
        .unwrap();
}

fn nesting_at_each_depth() {
    for depth in [100usize, 250, 255, 256, 300] {
        let mut response = Response::Done { ok: true };
        for candidate in 0..depth as u32 {
            response = Response::Of { response: Box::new(response), candidate };
        }
        let item = blaze_protocol::cbor::to_vec(&Envelope::new(1, response)).unwrap();
        let owned = frames::decode::<Response>(&item);
        let lent = frames::decode::<ResponseRef<'_>>(&item);
        match (owned, lent) {
            (Ok(owned), Ok(lent)) => assert_eq!(lent.body.into_owned(), owned.body, "{depth}"),
            (Err(owned), Err(lent)) => assert_eq!(lent, owned, "{depth}"),
            (owned, lent) => panic!("{depth}: owned {owned:?}, in place {lent:?}"),
        }
    }
}

/// A frame over the longest is refused before a byte of it is decoded, either way.
#[test]
fn a_frame_too_long_is_refused_before_either_read() {
    let mut frames = Frames::new();
    frames.push(&(LONGEST_REQUEST + 1).to_be_bytes());
    assert!(frames.ready());
    assert!(frames.next_ref().is_err());
}
