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
        on(blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(&bytes).unwrap(), at + 1 < responses.len());
    }
    ok
}

#[test]
fn a_response_handed_in_place_is_the_one_sent() {
    let sent = [Response::Fetched { verb: blaze_api::FetchVerb::Updating, what: "crates.io index".to_owned() }, Response::Done {
        ok: true,
    }];
    let mut got = Vec::new();
    assert!(hand(&sent, |response, more| got.push((response.into_owned(), more))));
    assert_eq!(got, [(sent[0].clone(), true), (sent[1].clone(), false)]);
}

#[test]
fn a_start_that_never_listens_names_the_socket() {
    let said = not_listening(Path::new("/run/blaze/v2.sock"), Path::new("/c/blaze/v2/serve.log"), 41, Phase::Setup);
    assert_eq!(
        said,
        "the daemon (`blaze serve`, pid 41) was started and did not listen at /run/blaze/v2.sock: it was \
         installing its store and the toolchain's proc macros for 60s; its log is /c/blaze/v2/serve.log"
    );
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
        build: blaze_api::Build::default(),
    };
    assert_eq!(status_lines(&status), "pid: 41\nrequests: 2 (3, 7)\nviews: 2\nmemory: 1.5 GiB of 8.0 GiB\n");
    let idle = Status {
        pid: 7,
        requests: Vec::new(),
        views: 0,
        history: None,
        memory_bytes: 512,
        budget_bytes: 1536,
        build: blaze_api::Build::default(),
    };
    assert_eq!(status_lines(&idle), "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\n");
    let failed = Status { history: Some("the profile history: writing /h/0-00000003.spans.parquet: disk full".to_owned()), ..idle };
    assert!(status_lines(&failed).ends_with("history: the profile history: writing /h/0-00000003.spans.parquet: disk full\n"));
}

/// A daemon of one build: its Status says `build`; every request it is sent is kept, and each
/// other than Status is answered Done.
struct OfBuild {
    build: blaze_api::Build,
    asked: Vec<Request>,
}

impl Api for OfBuild {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, request: Request, on: F) -> Result<bool, ClientError> {
        let answer = match request {
            Request::Status => vec![
                Response::Status(Status {
                    pid: 77,
                    requests: Vec::new(),
                    views: 0,
                    history: None,
                    memory_bytes: 0,
                    budget_bytes: 0,
                    build: self.build.clone(),
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
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &b.exe, || Ok(b.clone())).unwrap();
    assert_eq!(got, Builds::Other { pid: 77, theirs: a, ours: b });
    assert_eq!(daemon.asked, [Request::Status], "nothing but its Status is asked of another build's daemon");
}

/// The daemon's executable, unchanged, is this blaze: nothing is hashed.
#[test]
fn a_daemon_of_this_very_file_is_this_build_with_nothing_hashed() {
    let a = build(1, "/opt/blaze", 10);
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &a.exe, || panic!("hashed this blaze for an unchanged file"));
    assert_eq!(got.unwrap(), Builds::Same);
}

/// Another file of the same bytes (a copy, a link made again) is the same build: its hash says so.
#[test]
fn a_daemon_of_another_file_with_the_same_bytes_is_this_build() {
    let a = build(1, "/opt/blaze", 10);
    let copy = build(1, "/tmp/blaze", 11);
    let mut daemon = OfBuild { build: a, asked: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &copy.exe, || Ok(copy.clone())).unwrap();
    assert_eq!(got, Builds::Same);
}

/// A daemon of another schema can't be told to stop by this client: the error names both.
#[test]
fn a_daemon_of_another_schema_is_refused_naming_both() {
    struct OtherSchema;
    impl Api for OtherSchema {
        fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, _: Request, _: F) -> Result<bool, ClientError> {
            Err(ClientError::Schema { theirs: blaze_api::SCHEMA - 1 })
        }
    }
    let ours = build(2, "/new/blaze", 20);
    let why = builds(&mut OtherSchema, Path::new(SOCKET), &ours.exe, || Ok(ours.clone())).unwrap_err().to_string();
    let schemas = format!("speaks API schema {}, this blaze {}", blaze_api::SCHEMA - 1, blaze_api::SCHEMA);
    assert!(why.contains(SOCKET) && why.contains(&schemas), "{why}");
}
