use std::{ffi::OsString, path::PathBuf};

use blaze_api::{Request, Response, Table, ViewRef, client::ClientError};
use blaze_ids::ContentId;

use super::*;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// Answers every request with `responses`.
struct Answering(Vec<Response>);

impl Api for Answering {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, _: Request, on: F) -> Result<bool, ClientError> {
        Ok(crate::cli::daemon::tests::hand(&self.0, on))
    }
}

fn targets() -> blaze_api::Targets {
    blaze_api::Targets {
        cargo: None,
        target: None,
        profile: "dev".to_owned(),
        features: Vec::new(),
        packages: Vec::new(),
        all_features: false,
        no_default_features: false,
    }
}

fn tables() -> Request {
    Request::Tables { view: ViewRef::Head("/w".to_owned()) }
}

#[test]
fn its_own_flags_are_read_and_the_rest_is_the_builds() {
    let read = own(Inspected::CodegenInputs, os(&["--format=json", "-p", "foo", "--release"])).unwrap();
    assert_eq!(read, Own { out: None, build: os(&["-p", "foo", "--release"]) });
    let read = own(Inspected::Tables, os(&["--format", "json"])).unwrap();
    assert_eq!(read, Own { out: None, build: Vec::new() });
    let trace = own(Inspected::Trace, os(&["--out", "t.cbor", "--target", "x"])).unwrap();
    assert_eq!(trace, Own { out: Some(PathBuf::from("t.cbor")), build: os(&["--target", "x"]) });
    assert_eq!(own(Inspected::Trace, os(&["--out=t"])).unwrap().out, Some(PathBuf::from("t")));
}

#[test]
fn what_it_does_not_take_is_refused_naming_it() {
    assert!(own(Inspected::Tables, os(&["--format=human"])).unwrap_err().contains("JSON only"));
    assert!(own(Inspected::Trace, os(&[])).unwrap_err().contains("--out PATH"));
    assert!(own(Inspected::Trace, os(&["--format=json", "--out", "t"])).unwrap_err().contains("takes no `--format`"));
    assert!(own(Inspected::CodegenInputs, os(&["--out", "t"])).unwrap_err().contains("takes no `--out`"));
    assert!(own(Inspected::Trace, os(&["--out"])).unwrap_err().contains("takes a value"));
}

#[test]
fn the_tables_are_one_json_object() {
    let answers = vec![
        Response::Tables(vec![
            Table { name: "token".to_owned(), elements: 10, bytes: 40 },
            Table { name: "memo".to_owned(), elements: 2, bytes: 96 },
        ]),
        Response::Done { ok: true },
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = answer(&mut Answering(answers), Inspected::Tables, tables(), None, &mut out, &mut err, false).unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "{\"token\": {\"elements\": 10, \"bytes\": 40}, \"memo\": {\"elements\": 2, \"bytes\": 96}}\n"
    );
    assert!(err.is_empty());
}

#[test]
fn the_codegen_inputs_are_hex_to_hex() {
    let (key, code) = (ContentId::of("test", b"key"), ContentId::of("test", b"code"));
    let answers = vec![Response::CodegenInputs(vec![(key, code)]), Response::Done { ok: true }];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let request = Request::CodegenInputs { view: ViewRef::Head("/w".to_owned()), targets: targets() };
    let code_of = answer(&mut Answering(answers), Inspected::CodegenInputs, request, None, &mut out, &mut err, false);
    assert_eq!(code_of.unwrap(), 0);
    assert_eq!(String::from_utf8(out).unwrap(), blaze_ui::api::codegen_inputs_json(&[(key, code)]));
}

#[test]
fn a_failed_or_empty_answer_fails() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let refused = vec![
        Response::Error(blaze_api::ApiError { at: None, what: "no build of /w yet".to_owned() }),
        Response::Done { ok: false },
    ];
    assert_eq!(answer(&mut Answering(refused), Inspected::Tables, tables(), None, &mut out, &mut err, false).unwrap(), FAILED);
    assert_eq!(String::from_utf8(std::mem::take(&mut err)).unwrap(), "error: no build of /w yet\n");

    let nothing = vec![Response::Done { ok: true }];
    assert_eq!(answer(&mut Answering(nothing), Inspected::Tables, tables(), None, &mut out, &mut err, false).unwrap(), FAILED);
    assert!(String::from_utf8(err).unwrap().contains("no answer of it"));
    assert!(out.is_empty());
}

#[test]
fn the_trace_is_written_where_it_was_asked() {
    let dir = tempfile::tempdir().unwrap();
    let to = dir.path().join("trace.cbor");
    let answers = vec![Response::Trace(Vec::new()), Response::Done { ok: true }];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let request = Request::Trace { view: ViewRef::Head("/w".to_owned()), targets: targets() };
    let code = answer(&mut Answering(answers), Inspected::Trace, request, Some(&to), &mut out, &mut err, false).unwrap();
    assert_eq!(code, 0);
    assert_eq!(std::fs::read(&to).unwrap(), blaze_ui::api::trace_cbor(&[]).unwrap());
}

fn pack_request() -> Request {
    Request::Pack { view: ViewRef::Head("/w".to_owned()), targets: targets(), program: "app".to_owned() }
}

#[test]
fn a_pack_takes_its_out_and_names_its_own_target_and_profile() {
    let read = own(Inspected::Pack, os(&["-p", "app", "--out", "app.pack"])).unwrap();
    assert_eq!(read, Own { out: Some(PathBuf::from("app.pack")), build: os(&["-p", "app"]) });
    assert!(own(Inspected::Pack, os(&["-p", "app"])).unwrap_err().contains("--out PATH"));
    for flag in ["--release", "-r", "--target=x86_64-unknown-linux-gnu", "--profile"] {
        let refused = own(Inspected::Pack, os(&["--out", "p", flag])).unwrap_err();
        assert!(refused.contains(PACK_TARGET) && refused.contains("takes no"), "{flag}: {refused}");
    }
    assert!(own(Inspected::Pack, os(&["--out", "p", "--format=json"])).unwrap_err().contains("takes no `--format`"));
}

#[test]
fn the_pack_is_written_where_it_was_asked_and_its_checks_diagnostics_said() {
    let dir = tempfile::tempdir().unwrap();
    let to = dir.path().join("app.pack");
    let warned = blaze_api::Diagnostic {
        code: None,
        unit: None,
        facts: Vec::new(),
        level: blaze_api::Level::Warning,
        spans: Vec::new(),
        message: "unused".to_owned(),
        children: Vec::new(),
        rendered: "warning: unused\n".to_owned(),
        explanation: None,
    };
    let answers = vec![
        Response::Diagnostic(warned),
        Response::Packed { pack: b"{\"crates\": []}".to_vec() },
        Response::Done { ok: true },
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = answer(&mut Answering(answers), Inspected::Pack, pack_request(), Some(&to), &mut out, &mut err, false);
    assert_eq!(code.unwrap(), 0);
    assert_eq!(std::fs::read(&to).unwrap(), b"{\"crates\": []}");
    assert_eq!(String::from_utf8(err).unwrap(), "warning: unused\n");
    assert!(out.is_empty());

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let none = vec![Response::Done { ok: true }];
    let code = answer(&mut Answering(none), Inspected::Pack, pack_request(), Some(&to), &mut out, &mut err, false);
    assert_eq!(code.unwrap(), FAILED);
    assert!(String::from_utf8(err).unwrap().contains("no pack"));
}
