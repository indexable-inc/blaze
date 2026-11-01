//! An item from a peer as blaze-api decodes it: `frames::decode::<Request>`, what the daemon
//! does with every item a client sends over its Unix socket or as an HTTP body, and
//! `frames::decode::<Response>`, what the client does with every item the daemon sends; then
//! a decoded envelope back out through `frames::encode`.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Decoding asks for at most `blaze_fuzz::allowed(len)` bytes at once.
//! - The fast decoder and the reference read an `Envelope<Request>`, an `Envelope<Response>`
//!   and an envelope's head (`Envelope<IgnoredAny>`) alike; an envelope that decodes is of
//!   bytes `cbor::check` passes, and bytes it refuses fail with its error.
//! - `decode` gives the envelope exactly when it decodes in this `SCHEMA`; otherwise it refuses
//!   it under the id its head has (none when the head does not decode), naming another schema
//!   when the head is of one and the decoder's error when it is not.
//! - A decoded envelope encodes (`frames::encode`) to a frame of its length and the bytes
//!   `cbor::to_vec` and the reference encoder give, which are canonical and decode back to it.
//!   Bytes are not compared with the input: serde reads a struct's field by its index or as a
//!   byte string, passes over unknown fields and takes a missing `Option` as `None`, so one
//!   envelope has more than one canonical item that reads as it.

#![forbid(unsafe_code)]

use std::{fmt, time::Duration};

use blaze_api::{
    ApiError, Applicability, Artifact, ArtifactKind, Basis, BuildScriptRan, Cached, Cargo,
    CrateType, Diagnostic, Envelope, FactRead, FetchVerb, Level, LinkHow, LinkKind, Linked,
    LiveFrame, Mismatch, Ranked, Reach, ReachedItem, Request, Response, SCHEMA, ServedCrate,
    ServedPart, Span, Status, Table, TargetKind, Targets, TestOutcome, TestResult, Traced, Ub,
    UnitOf, UnitProfile, UnitStarted, UnitTarget, Verb, ViewRef, Worked,
    frames::{self, Refused},
};
use blaze_fuzzer::Target;
use blaze_ids::{ContentId, Fingerprint};
use blaze_protocol::cbor::{self, reference};
use blaze_query::{Changed, Edge, Input, QueryKey, QueryKind, Why};
use blaze_vfs::{Change, ChangeKind, Mode, Op, View};
use serde::{Serialize, de::DeserializeOwned, de::IgnoredAny};

const TARGET: Target = Target {
    name: "api_envelope",
    check,
    seeds,
    dictionary: &[
        b"\xa3\x62id",
        b"\x64body",
        b"\x66schema\x07",
        b"\x66schema\x02",
        b"\x66Status",
        b"\x64Stop",
        b"\x68Snapshot",
        b"\x66Cancel",
        b"\x66Commit",
        b"\x65Check",
        b"\x65Build",
        b"\x64Test",
        b"\x63Run",
        b"\x64Diff",
        b"\x63Why",
        b"\x64Open",
        b"\x65Close",
        b"\x65Reach",
        b"\x66BestOf",
        b"\x66Verify",
        b"\x64Done",
        b"\x65Error",
        b"\x62Of",
        b"\x64Head",
        b"\x64Root",
        b"\x66WhatIf",
        b"\x64root",
        b"\x64path",
        b"\x65bytes",
        b"\x64mode",
        b"\x67Regular",
        b"\x67targets",
        b"\x64view",
        b"\x65cargo",
        b"\x98\x20",
        b"\x90",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(900),
};

fn check(data: &[u8]) {
    let checked = cbor::check(data);
    let head = cbor::from_slice::<Envelope<IgnoredAny>>(data);
    assert_eq!(
        head,
        reference::from_slice::<Envelope<IgnoredAny>>(data),
        "the two decoders read an envelope's head alike"
    );
    canonical(&head, &checked, "an envelope's head");
    envelope::<Request>(data, &checked, &head);
    envelope::<Response>(data, &checked, &head);
}

/// A read of bytes `check` refuses fails with its error; one that succeeds is of bytes it
/// passes.
#[track_caller]
fn canonical<T>(read: &Result<T, cbor::Error>, checked: &Result<(), cbor::Error>, what: &str) {
    match (read, checked) {
        (Ok(_), Err(error)) => panic!("{what} decoded from bytes check refuses: {error}"),
        (Err(error), Err(offence)) => {
            assert_eq!(error, offence, "{what} of bytes check refuses fails with its error");
        }
        (_, Ok(())) => {}
    }
}

fn envelope<T: Serialize + DeserializeOwned + PartialEq + fmt::Debug>(
    data: &[u8],
    checked: &Result<(), cbor::Error>,
    head: &Result<Envelope<IgnoredAny>, cbor::Error>,
) {
    let name = std::any::type_name::<T>();
    let (most, decoded) = blaze_fuzz::largest_allocation(|| frames::decode::<T>(data));
    assert!(most <= blaze_fuzz::allowed(data.len()), "decoding {} bytes as a {name} asked for {most} at once", data.len());
    let read = cbor::from_slice::<Envelope<T>>(data);
    assert_eq!(
        read,
        reference::from_slice::<Envelope<T>>(data),
        "the two decoders read an envelope of {name} alike"
    );
    canonical(&read, checked, name);

    let undecodable = |error: &cbor::Error| {
        ApiError::new(format!("an envelope that does not decode: {error}"), None)
    };
    let expected = match (&read, head) {
        (Ok(envelope), _) if envelope.schema == SCHEMA => None,
        (_, Err(error)) => Some(Refused { id: None, error: undecodable(error) }),
        (_, Ok(head)) if head.schema != SCHEMA => {
            let what = format!("schema {}, this daemon speaks {SCHEMA}", head.schema);
            Some(Refused { id: Some(head.id), error: ApiError::new(what, None) })
        }
        (Err(error), Ok(head)) => Some(Refused { id: Some(head.id), error: undecodable(error) }),
        (Ok(envelope), Ok(head)) => panic!(
            "an envelope of {name} read schema {} where its head read {}",
            envelope.schema, head.schema
        ),
    };
    match (decoded, expected) {
        (Ok(envelope), None) => {
            assert_eq!(read.as_ref(), Ok(&envelope), "decode reads what from_slice does");
            assert_eq!(head.as_ref().map(|head| head.id), Ok(envelope.id), "decode reads the id its head has");
        }
        (Err(refused), Some(expected)) => {
            assert_eq!(refused, expected, "decode refuses an envelope of {name} under its head's id, naming why");
        }
        (decoded, expected) => panic!("decode gave {decoded:?} where {expected:?} was due"),
    }

    if let Ok(envelope) = &read {
        let frame = frames::encode(envelope).expect("a decoded envelope has an encoding");
        let item = &frame[4..];
        assert_eq!(frame[..4], (item.len() as u32).to_be_bytes(), "a frame starts with its item's length");
        assert_eq!(cbor::to_vec(envelope).as_deref(), Ok(item), "encode writes what to_vec does");
        assert_eq!(reference::to_vec(envelope).as_deref(), Ok(item), "encode writes what the reference does");
        assert_eq!(cbor::check(item), Ok(()), "encode writes canonical bytes");
        assert_eq!(cbor::from_slice::<Envelope<T>>(item).as_ref(), Ok(envelope), "an encoded envelope decodes back to itself");
    }
}

fn id(byte: u8) -> ContentId {
    ContentId([byte; 32])
}

fn key(kind: u16, byte: u8) -> QueryKey {
    QueryKey { kind: QueryKind(kind), input: id(byte) }
}

fn targets(cargo: bool) -> Targets {
    Targets {
        cargo: cargo.then(|| Cargo {
            cwd: "/w".to_owned(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            args: vec!["--release".to_owned()],
            root: "/w".to_owned(),
            color: true,
            files: vec![("/home/a/.cargo/config.toml".to_owned(), b"[build]\n".to_vec())],
            width: Some(100),
            command: "build".to_owned(),
            verbose: 2,
            refused: Some(16),
        }),
        target: Some("x86_64-unknown-linux-gnu".to_owned()),
        profile: "dev".to_owned(),
        features: vec!["std".to_owned()],
        packages: vec!["blaze".to_owned()],
        all_features: false,
        no_default_features: true,
    }
}

fn unit_target() -> UnitTarget {
    UnitTarget {
        doc: true,
        kind: TargetKind::Lib,
        name: "a".to_owned(),
        test: true,
        doctest: true,
        edition: "2024".to_owned(),
        src_path: "/w/src/lib.rs".to_owned(),
        crate_types: vec![CrateType::Lib, CrateType::Cdylib],
    }
}

fn unit_of() -> UnitOf {
    UnitOf {
        id: 0,
        test: false,
        target: unit_target(),
        package_id: "path+file:///w#a@0.1.0".to_owned(),
        manifest_path: "/w/Cargo.toml".to_owned(),
    }
}

fn fact() -> FactRead {
    FactRead { key: key(4, 5), what: "the signature of `a::f`".to_owned() }
}

/// Every request and every response, each as the client or the daemon writes its item.
fn seeds() -> Vec<Vec<u8>> {
    let ops = vec![
        Op::Write { mode: Mode::Regular, path: "src/lib.rs".to_owned(), bytes: b"fn f() {}\n".to_vec() },
        Op::Edit { at: 3, len: 1, path: "src/lib.rs".to_owned(), bytes: b"g".to_vec() },
        Op::Delete { path: "src/old.rs".to_owned() },
        Op::Rename { to: "src/b.rs".to_owned(), from: "src/a.rs".to_owned() },
    ];
    let head = || ViewRef::Head("main".to_owned());
    let what_if = ViewRef::WhatIf { ops: ops.clone(), base: Box::new(head()) };
    let requests = vec![
        Request::Status,
        Request::Stop,
        Request::Snapshot,
        Request::Cancel { id: 3 },
        Request::Commit { ops: ops.clone(), base: id(1), head: "main".to_owned() },
        Request::Check { view: head(), targets: targets(false) },
        Request::Build { view: ViewRef::Root(View { root: id(2) }), targets: targets(true) },
        Request::Test {
            ub: Some(Ub { seed: Some(9), schedules: None }),
            all: true,
            args: vec!["--exact".to_owned()],
            view: what_if.clone(),
            filter: vec!["tests::a".to_owned()],
            targets: targets(false),
        },
        Request::Run { bin: Some("a".to_owned()), args: Vec::new(), view: head(), targets: targets(true) },
        Request::Diff { to: head(), from: ViewRef::Root(View { root: id(3) }) },
        Request::Why { view: head(), query: key(3, 4) },
        Request::Open { view: what_if.clone() },
        Request::Close { root: id(5) },
        Request::Reach { to: what_if, from: head(), targets: targets(false) },
        Request::BestOf {
            base: head(),
            test: true,
            filter: Vec::new(),
            targets: targets(false),
            candidates: vec![ops.clone(), Vec::new()],
        },
        Request::Verify { view: head(), targets: targets(false) },
        Request::CodegenInputs { view: head(), targets: targets(false) },
        Request::Trace { view: head(), targets: targets(false) },
        Request::Tables { view: head() },
        Request::Pack { view: head(), targets: targets(false), program: "app".to_owned() },
        Request::Explore { dev: true },
        Request::Profile { seconds: 60 },
        Request::Live,
        Request::Sql { sql: "select 1".to_owned(), last_s: 30 },
    ];
    let span = Span {
        file: "src/lib.rs".to_owned(),
        line: 1,
        label: Some("expected `u8`".to_owned()),
        column: 5,
        primary: true,
        byte_end: 9,
        end_line: 1,
        byte_start: 4,
        end_column: 10,
        replacement: None,
        applicability: Some(Applicability::MachineApplicable),
    };
    let diagnostic = Diagnostic {
        code: Some("E0308".to_owned()),
        unit: Some(unit_of()),
        facts: vec![fact()],
        level: Level::Error,
        spans: vec![span],
        message: "mismatched types".to_owned(),
        children: vec![Diagnostic {
            code: None,
            unit: None,
            facts: Vec::new(),
            level: Level::Help,
            spans: Vec::new(),
            message: "try".to_owned(),
            children: Vec::new(),
            rendered: String::new(),
            explanation: None,
        }],
        rendered: "error[E0308]: mismatched types\n".to_owned(),
        explanation: Some("Expected type did not match the received type.\n".to_owned()),
    };
    let artifact = Artifact {
        env: vec![("CARGO_PKG_NAME".to_owned(), "a".to_owned())],
        kind: ArtifactKind::Bin,
        path: "/w/target/debug/a".to_owned(),
        fresh: false,
        runner: Vec::new(),
        target: unit_target(),
        content: id(6),
        harness: true,
        package: "path+file:///w#a@0.1.0".to_owned(),
        profile: UnitProfile {
            test: false,
            debuginfo: 2,
            opt_level: "0".to_owned(),
            overflow_checks: true,
            debug_assertions: true,
        },
        features: vec!["default".to_owned()],
        filenames: vec!["/w/target/debug/a".to_owned()],
        executable: Some("/w/target/debug/a".to_owned()),
        manifest_path: "/w/Cargo.toml".to_owned(),
    };
    let responses = vec![
        Response::Committed { root: id(7), rebased: true },
        Response::Diagnostic(diagnostic),
        Response::Built { members: 2, profile: "`dev` profile".to_owned(), artifacts: vec![artifact] },
        Response::Unit(UnitStarted {
            name: "a".to_owned(),
            verb: Verb::Compiling,
            source: Some("(/w)".to_owned()),
            package: 1,
            version: "0.1.0".to_owned(),
            package_id: "path+file:///w#a@0.1.0".to_owned(),
        }),
        Response::UnitEnded(unit_of()),
        Response::Fetched { verb: FetchVerb::Downloaded, what: "serde v1.0.219".to_owned() },
        Response::BuildScript(BuildScriptRan {
            env: vec![("OUT_DIR".to_owned(), "/o".to_owned())],
            cfgs: vec!["has_x".to_owned()],
            out_dir: "/o".to_owned(),
            package_id: "a".to_owned(),
            linked_libs: vec!["z".to_owned()],
            linked_paths: Vec::new(),
        }),
        Response::Work(Worked {
            link: LinkKind::Patched,
            found: 1,
            green: 2,
            links: vec![Linked { how: LinkHow::Spliced, path: "target/debug/a".to_owned() }],
            reran: 3,
            served: vec![ServedCrate {
                why: Some("a body changed".to_owned()),
                bodies: ServedPart::Served,
                outcome: ServedPart::Refused,
                package: "a".to_owned(),
            }],
            compiled: 4,
            critical_path_us: u64::MAX,
            steps_us: vec![(blaze_api::NodeKind::Codegen, u64::MAX)],
        }),
        Response::TestResult(TestResult {
            name: "tests::a".to_owned(),
            binary: "a".to_owned(),
            cached: None,
            millis: 12,
            output: b"ok\n".to_vec(),
            outcome: TestOutcome::Passed,
            outside: true,
            executable: 0,
        }),
        Response::TestResult(TestResult {
            name: "tests::b".to_owned(),
            binary: "a".to_owned(),
            cached: Some(Cached { basis: Basis::Executed, millis: 3, outcome: TestOutcome::Failed }),
            millis: 0,
            output: Vec::new(),
            outcome: TestOutcome::Unreached,
            outside: false,
            executable: 0,
        }),
        Response::TestResult(TestResult {
            name: "tests::c".to_owned(),
            binary: "b".to_owned(),
            cached: Some(Cached {
                basis: Basis::Reached { why: "its record was cut short".to_owned() },
                millis: 5,
                outcome: TestOutcome::Ignored,
            }),
            millis: 0,
            output: Vec::new(),
            outcome: TestOutcome::Unreached,
            outside: false,
            executable: 1,
        }),
        Response::Ran { code: Some(-1), stderr: b"e".to_vec(), stdout: Vec::new() },
        Response::Diff { changes: vec![Change { kind: ChangeKind::Modified, path: "a".to_owned() }] },
        Response::Why(Why {
            query: key(1, 1),
            chain: vec![Changed { key: key(2, 2), was: Fingerprint(1), now: Fingerprint(u128::MAX) }],
            input: Some(Input::File("src/lib.rs".to_owned())),
        }),
        Response::Why(Why { query: key(1, 2), chain: Vec::new(), input: Some(Input::Config) }),
        Response::Why(Why { query: key(1, 3), chain: Vec::new(), input: Some(Input::Dir("src".to_owned())) }),
        Response::Status(Status {
            pid: 4242,
            views: 1,
            requests: vec![1, 2],
            budget_bytes: 1 << 30,
            memory_bytes: 0,
            history: None,
            build: blaze_api::Build::default(),
        }),
        Response::Status(Status {
            pid: 1,
            views: 0,
            requests: Vec::new(),
            budget_bytes: 0,
            memory_bytes: 0,
            history: Some("the disk is full".to_owned()),
            build: blaze_api::Build::default(),
        }),
        Response::Explored { link: "https://ix.dev/explore#x".to_owned() },
        Response::Profiled { trace: b"{}".to_vec() },
        Response::Live(LiveFrame {
            lost: 1,
            open: Vec::new(),
            at_ns: -1,
            spans: vec![0xff],
            skipped: u64::MAX,
            counters: Vec::new(),
        }),
        Response::Queried { ipc: vec![1, 2, 3], rows: 3 },
        Response::Done { ok: false },
        Response::Error(ApiError::new("no such head as `x`", Some("x".to_owned()))),
        Response::Opened { root: id(8) },
        Response::Reached(Reach {
            items: vec![ReachedItem { path: "a::f".to_owned(), facts: vec![fact()] }],
            tests: vec!["tests::a".to_owned()],
            changes: Vec::new(),
            compiled: vec!["a::f".to_owned()],
        }),
        Response::Of { response: Box::new(Response::Done { ok: true }), candidate: 1 },
        Response::Ranked(vec![Ranked {
            ok: true,
            root: id(9),
            errors: 0,
            failed: 0,
            passed: 3,
            warnings: 1,
            candidate: 0,
        }]),
        Response::Verified {
            mismatches: vec![Mismatch {
                fact: Some(fact()),
                query: key(6, 6),
                scratch: Fingerprint(1 << 100),
                incremental: Fingerprint(5),
            }],
        },
        Response::Snapshotted { heads: vec![("main".to_owned(), id(10))] },
        Response::CodegenInputs(vec![(id(11), id(12))]),
        Response::Trace(vec![Traced {
            key: key(7, 7),
            ran: true,
            edges: vec![Edge { read: key(8, 8), saw: Fingerprint(3) }],
            fingerprint: Fingerprint(4),
        }]),
        Response::Tables(vec![Table { name: "memo".to_owned(), bytes: 1 << 20, elements: 7 }]),
        Response::Packed { pack: b"{\"crates\": []}".to_vec() },
    ];
    let mut seeds: Vec<Vec<u8>> = Vec::new();
    for (at, request) in requests.into_iter().enumerate() {
        seeds.push(item(&Envelope::new(at as u64, request)));
    }
    for (at, response) in responses.into_iter().enumerate() {
        seeds.push(item(&Envelope::new(u64::MAX - at as u64, response)));
    }
    seeds.push(item(&Envelope { schema: 2, id: 1, body: Request::Status }));
    seeds.push(item(&Envelope::new(1, "Nope")));
    seeds
}

fn item<T: Serialize>(envelope: &Envelope<T>) -> Vec<u8> {
    cbor::to_vec(envelope).expect("a seed has an encoding")
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
