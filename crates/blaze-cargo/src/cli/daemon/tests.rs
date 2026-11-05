use std::path::Path;

use blaze_api::{Response, Status};

use super::*;

/// Hands `on` each of `responses` as [`Client::call_ref`] does: encoded as the daemon sends it,
/// read where it lies, with whether another follows it; the last Done's `ok`. The fakes of
/// [`Api`] answer through it.
pub(crate) fn hand(responses: &[Response], mut on: impl FnMut(ResponseRef<'_>, bool)) -> bool {
    let mut ok = false;
    for (at, response) in responses.iter().enumerate() {
        if let Response::Done { ok: it } = response {
            ok = *it;
        }
        let bytes = blaze_protocol::cbor::to_vec(response).unwrap();
        on(
            blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(&bytes).unwrap(),
            at + 1 < responses.len(),
        );
    }
    ok
}

#[test]
fn a_response_handed_in_place_is_the_one_sent() {
    let sent = [
        Response::Fetched {
            verb: blaze_api::FetchVerb::Updating,
            what: "crates.io index".to_owned(),
        },
        Response::Done { ok: true },
    ];
    let mut got = Vec::new();
    assert!(hand(&sent, |response, more| got.push((response.into_owned(), more))));
    assert_eq!(got, [(sent[0].clone(), true), (sent[1].clone(), false)]);
}

#[test]
fn a_start_that_never_listens_names_the_socket() {
    let said = not_listening(
        Path::new("/run/blaze/v2.sock"),
        Path::new("/c/blaze/v2/serve.log"),
        41,
        Phase::Setup,
    );
    assert_eq!(
        said,
        "the daemon (`blaze serve`, pid 41) was started and did not listen at /run/blaze/v2.sock: it was \
         installing its store and the toolchain's proc macros for 60s; its log is /c/blaze/v2/serve.log"
    );
}

#[test]
fn the_status_as_json_has_every_field_with_or_without_a_daemon() {
    let place = StatusPlace {
        home: PathBuf::from("/h"),
        state: PathBuf::from("/h/v2"),
        socket: PathBuf::from("/h/v2.sock"),
    };
    let none: serde_json::Value = serde_json::from_str(&status_json(&place, None).unwrap()).unwrap();
    assert_eq!(none["daemon"], serde_json::Value::Null);
    assert_eq!(none["api_schema"], blaze_api::SCHEMA);
    assert_eq!(none["format"], 1);
    assert_eq!((none["home"].as_str(), none["socket"].as_str()), (Some("/h"), Some("/h/v2.sock")));
    let bug = |count| blaze_api::Bug {
        kind: "AdtDef".to_owned(),
        what: "formatted with no database attached".to_owned(),
        at: "a.rs:1:1".to_owned(),
        count,
        thread: None,
    };
    let status = Status {
        pid: 41,
        requests: vec![3, 7],
        views: 2,
        history: None,
        memory_bytes: 3 << 29,
        budget_bytes: 8 << 30,
        uptime_ms: 9000,
        build: blaze_api::Build::default(),
        bugs: vec![bug(2), bug(1)],
        settings: vec![
            blaze_api::Setting { name: "BLAZE_OPT".to_owned(), value: None },
            blaze_api::Setting {
                name: "BLAZE_DAEMON_MEMORY_GB".to_owned(),
                value: Some("80".to_owned()),
            },
        ],
        builds: Vec::new(),
        downloads: Vec::new(),
    };
    let line = status_json(&place, Some(&status)).unwrap();
    assert!(line.ends_with("}\n") && line.matches('\n').count() == 1, "{line}");
    let some: serde_json::Value = serde_json::from_str(&line).unwrap();
    let daemon = &some["daemon"];
    assert_eq!((daemon["pid"].as_u64(), daemon["uptime_ms"].as_u64()), (Some(41), Some(9000)));
    assert_eq!(daemon["bugs"], 3, "the bugs of every site, summed");
    assert_eq!(daemon["requests"], serde_json::json!([3, 7]));
    assert_eq!(
        daemon["settings"],
        serde_json::json!({"BLAZE_DAEMON_MEMORY_GB": "80", "BLAZE_OPT": null})
    );
    assert_eq!(daemon["build"].as_str().map(str::len), Some(64), "the build's hash in hex");
}

#[test]
fn the_status_for_people() {
    let status = Status {
        pid: 41,
        requests: vec![3, 7],
        views: 2,
        history: None,
        memory_bytes: 3 << 29,
        budget_bytes: 8 << 30,
        uptime_ms: 40,
        build: blaze_api::Build::default(),
        bugs: Vec::new(),
        settings: Vec::new(),
        builds: Vec::new(),
        downloads: Vec::new(),
    };
    assert_eq!(
        status_lines(&status),
        "pid: 41\nrequests: 2 (3, 7)\nviews: 2\nmemory: 1.5 GiB of 8.0 GiB\nbugs: 0\n"
    );
    let idle = Status {
        pid: 7,
        requests: Vec::new(),
        views: 0,
        history: None,
        memory_bytes: 512,
        budget_bytes: 1536,
        uptime_ms: 40,
        build: blaze_api::Build::default(),
        bugs: Vec::new(),
        settings: Vec::new(),
        builds: Vec::new(),
        downloads: Vec::new(),
    };
    assert_eq!(
        status_lines(&idle),
        "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\nbugs: 0\n"
    );
    let told = Status {
        settings: vec![
            blaze_api::Setting {
                name: "BLAZE_DAEMON_MEMORY_GB".to_owned(),
                value: Some("80".to_owned()),
            },
            blaze_api::Setting { name: "BLAZE_OPT".to_owned(), value: None },
        ],
        ..idle.clone()
    };
    assert_eq!(
        status_lines(&told),
        "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\nsettings: BLAZE_DAEMON_MEMORY_GB=80\nbugs: 0\n"
    );
    let failed = Status {
        history: Some(
            "the profile history: writing /h/0-00000003.spans.parquet: disk full".to_owned(),
        ),
        ..idle.clone()
    };
    assert!(status_lines(&failed).ends_with(
        "history: the profile history: writing /h/0-00000003.spans.parquet: disk full\nbugs: 0\n"
    ));
    let bug = |count, thread: Option<&str>| blaze_api::Bug {
        kind: "AdtDef".to_owned(),
        what: "formatted with no database attached".to_owned(),
        at: "crates/hir-ty/src/next_solver/interner.rs:951:9".to_owned(),
        count,
        thread: thread.map(str::to_owned),
    };
    let buggy = Status { bugs: vec![bug(2, Some("blaze-worker-3")), bug(1, None)], ..idle };
    assert!(
        status_lines(&buggy).ends_with(
            "bugs: 3\n  a blaze bug: `AdtDef` formatted with no database attached at \
             crates/hir-ty/src/next_solver/interner.rs:951:9, 2 times, first on thread `blaze-worker-3`\n  \
             a blaze bug: `AdtDef` formatted with no database attached at \
             crates/hir-ty/src/next_solver/interner.rs:951:9, 1 time\n"
        ),
        "{}",
        status_lines(&buggy)
    );
    let building = Status {
        builds: vec![blaze_api::Building {
            id: 12,
            what: "test -p blaze-api".to_owned(),
            phase: "builds 412 units, 1 running".to_owned(),
            spans: vec!["blaze-worker-3: task (2 min 05 s) > unit bodies `app::main` in app (4 s)".to_owned()],
            for_ms: 125_400,
        }],
        downloads: vec![blaze_api::Downloading {
            url: "https://example.com/llvm.tar.xz".to_owned(),
            done: 34_000_000,
            rate: None,
            what: "llvm.tar.xz".to_owned(),
            total: Some(410_000_000),
            idle_ms: 0,
            elapsed_ms: 9_000,
        }],
        ..buggy
    };
    assert!(
        status_lines(&building).ends_with(
            "builds: 1\n  #12 test -p blaze-api (2 min 05 s): builds 412 units, 1 running\n    \
             blaze-worker-3: task (2 min 05 s) > unit bodies `app::main` in app (4 s)\n\
             downloads: 1\n  llvm.tar.xz: 34.0 of 410.0 MB (8%)\n    from https://example.com/llvm.tar.xz\n"
        ),
        "{}",
        status_lines(&building)
    );
}

/// A daemon of one build: its Status says `build`; every request it is sent is kept, and each
/// other than Status is answered Done.
struct OfBuild {
    build: blaze_api::Build,
    asked: Vec<Request>,
    /// What its Status says it was started with.
    settings: Vec<blaze_api::Setting>,
}

impl Api for OfBuild {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(
        &mut self,
        request: Request,
        on: F,
    ) -> Result<bool, ClientError> {
        let answer = match request {
            Request::Status => vec![
                Response::Status(Status {
                    pid: 77,
                    requests: Vec::new(),
                    settings: self.settings.clone(),
                    views: 0,
                    history: None,
                    memory_bytes: 0,
                    budget_bytes: 0,
                    uptime_ms: 40,
                    build: self.build.clone(),
                    bugs: Vec::new(),
                    builds: Vec::new(),
                    downloads: Vec::new(),
                }),
                Response::Done { ok: true },
            ],
            _ => vec![Response::Done { ok: true }],
        };
        self.asked.push(request);
        Ok(hand(&answer, on))
    }
}

fn build(id: u8, path: &str, inode: u64) -> blaze_api::Build {
    blaze_api::Build {
        id: blaze_ids::ContentId([id; 32]),
        exe: blaze_api::Executable { path: path.to_owned(), len: 1 << 20, modified_ns: 5, inode },
    }
}

const SOCKET: &str = "/run/blaze/v2.sock";

/// A client of build B finds a daemon of build A: only Status is sent to it, and the answer
/// names both builds and the daemon's pid; no request of B's runs on A.
#[test]
fn a_client_of_another_build_sends_the_daemon_only_its_status() {
    let a = build(1, "/old/blaze", 10);
    let b = build(2, "/new/blaze", 20);
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new(), settings: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &b.exe, &[], || Ok(b.clone())).unwrap();
    assert_eq!(got, Builds::Other { pid: 77, theirs: a, ours: b });
    assert_eq!(
        daemon.asked,
        [Request::Status],
        "nothing but its Status is asked of another build's daemon"
    );
}

/// The daemon's executable, unchanged, is this blaze: nothing is hashed.
#[test]
fn a_daemon_of_this_very_file_is_this_build_with_nothing_hashed() {
    let a = build(1, "/opt/blaze", 10);
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new(), settings: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &a.exe, &[], || {
        panic!("hashed this blaze for an unchanged file")
    });
    assert_eq!(got.unwrap(), Builds::Same);
}

/// Another file of the same bytes (a copy, a link made again) is the same build: its hash says so.
#[test]
fn a_daemon_of_another_file_with_the_same_bytes_is_this_build() {
    let a = build(1, "/opt/blaze", 10);
    let copy = build(1, "/tmp/blaze", 11);
    let mut daemon = OfBuild { build: a, asked: Vec::new(), settings: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &copy.exe, &[], || Ok(copy.clone())).unwrap();
    assert_eq!(got, Builds::Same);
}

/// A daemon of this build told another memory limit than this command sets (the 80 GB daemon
/// a client with `BLAZE_DAEMON_MEMORY_GB=100` built on, failing ten minutes in) is found from its
/// Status alone, with the setting and both values, for the CLI to refuse before any request. A
/// command that leaves the limit unset builds with the daemon's, `--memory-gb 80` included.
#[test]
fn a_daemon_started_with_another_limit_is_found_before_any_request() {
    let a = build(1, "/opt/blaze", 10);
    let theirs = crate::serve::settings::of_daemon(
        Some(&(80u64 << 30, "--memory-gb=80".to_owned())),
        |_| None,
    );
    let ours = crate::serve::settings::of_client(|name| {
        (name == crate::serve::MEMORY_VAR).then(|| "100".into())
    });
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new(), settings: theirs.clone() };
    let got = builds(&mut daemon, Path::new(SOCKET), &a.exe, &ours, || {
        panic!("hashed this blaze for an unchanged file")
    })
    .unwrap();
    let drift = vec![crate::serve::settings::Drift {
        name: crate::serve::MEMORY_VAR.to_owned(),
        theirs: Some("80".to_owned()),
        ours: "100".to_owned(),
    }];
    assert_eq!(got, Builds::Settings { pid: 77, drift });
    assert_eq!(
        daemon.asked,
        [Request::Status],
        "nothing but its Status is asked before the settings agree"
    );
    for asked in [Some("80"), None] {
        let mut same = OfBuild { build: a.clone(), asked: Vec::new(), settings: theirs.clone() };
        let ours = crate::serve::settings::of_client(|name| {
            (name == crate::serve::MEMORY_VAR).then_some(asked).flatten().map(Into::into)
        });
        assert_eq!(
            builds(&mut same, Path::new(SOCKET), &a.exe, &ours, || panic!("hashed")).unwrap(),
            Builds::Same,
            "{asked:?}"
        );
    }
}

/// A daemon of another schema can't be told to stop by this client: the error names both.
#[test]
fn a_daemon_of_another_schema_is_refused_naming_both() {
    struct OtherSchema;
    impl Api for OtherSchema {
        fn call<F: FnMut(ResponseRef<'_>, bool)>(
            &mut self,
            _: Request,
            _: F,
        ) -> Result<bool, ClientError> {
            Err(ClientError::Schema { theirs: blaze_api::SCHEMA - 1 })
        }
    }
    let ours = build(2, "/new/blaze", 20);
    let why = builds(&mut OtherSchema, Path::new(SOCKET), &ours.exe, &[], || Ok(ours.clone()))
        .unwrap_err()
        .to_string();
    let schemas =
        format!("speaks API schema {}, this blaze {}", blaze_api::SCHEMA - 1, blaze_api::SCHEMA);
    assert!(why.contains(SOCKET) && why.contains(&schemas), "{why}");
}

/// The daemon answers Stop's Done at once and says each step of its stop on the same request
/// right after, so one read often holds the Done and a step: `blaze stop` (and the replacing of
/// another build's daemon) takes them as the steps of the wait, never as a protocol error.
#[cfg(unix)]
#[test]
fn a_stops_steps_read_with_its_done_are_the_waits() {
    use std::io::{Read as _, Write as _};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("s");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 64];
            _ = stream.read(&mut request).unwrap();
            let mut burst = blaze_api::frames::encode(&blaze_api::Envelope::new(
                1,
                Response::Done { ok: true },
            ))
            .unwrap();
            let step = Response::Stopping { doing: "writing the daemon image".to_owned() };
            burst.extend(blaze_api::frames::encode(&blaze_api::Envelope::new(1, step)).unwrap());
            stream.write_all(&burst).unwrap();
        }
    });
    let mut client = Client::unix(&socket).unwrap();
    assert!(told_to_stop(&mut client, |_, _| {}).unwrap());
    client.until_hung_up(std::time::Duration::from_secs(10)).unwrap();
    // What `Api::call` makes of the same burst: the error `blaze stop` failed with at random.
    let mut client = Client::unix(&socket).unwrap();
    assert!(Api::call(&mut client, Request::Stop, |_, _| {}).is_err());
    daemon.join().unwrap();
}
