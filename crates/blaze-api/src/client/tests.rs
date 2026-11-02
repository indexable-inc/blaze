use std::{
    io::Write,
    net::TcpListener,
    os::unix::net::UnixStream,
    thread::{self, JoinHandle},
};

use super::*;
use crate::{Status, http::RequestParser};

fn status() -> Status {
    Status { pid: 4, requests: vec![7], views: 1, history: None, memory_bytes: 2, budget_bytes: 3, build: crate::Build::default() }
}

/// A client over one end of a socket pair, and a thread on the other that reads one request
/// and answers with `answers` (each envelope's id 0 is the request's).
fn unix_daemon(
    answers: Vec<Envelope<Response>>,
) -> (Client, JoinHandle<Envelope<Request>>) {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let daemon = thread::spawn(move || {
        let item = blaze_protocol::read_frame(&mut theirs).unwrap().unwrap();
        let request: Envelope<Request> = frames::decode(&item).unwrap();
        for mut answer in answers {
            if answer.id == 0 {
                answer.id = request.id;
            }
            theirs.write_all(&frames::encode(&answer).unwrap()).unwrap();
        }
        request
    });
    (Client::over(Link::Unix(ours)), daemon)
}

#[test]
fn a_call_hands_over_its_responses_and_keeps_the_others() {
    let (mut client, daemon) = unix_daemon(vec![
        Envelope::new(99, Response::Done { ok: true }),
        Envelope::new(0, Response::Status(status())),
        Envelope::new(0, Response::Done { ok: false }),
    ]);
    let mut seen = Vec::new();
    let ok = client.call(Request::Status, |it| seen.push(it.clone())).unwrap();
    assert!(!ok);
    assert_eq!(seen, [Response::Status(status()), Response::Done { ok: false }]);
    assert_eq!(client.receive().unwrap(), Envelope::new(99, Response::Done { ok: true }));
    assert_eq!(daemon.join().unwrap(), Envelope::new(1, Request::Status));
}

/// In place, a call hands over what [`Client::call`] does, says whether the next is already
/// read, and keeps another request's responses owned.
#[test]
fn a_call_in_place_hands_over_its_responses_and_keeps_the_others() {
    let error = ApiError::new("no such head", Some("main".to_owned()));
    let fetched = Response::Fetched { verb: crate::FetchVerb::Updating, what: "crates.io index".to_owned() };
    // Every answer is in the socket before the first read, so one read takes them all.
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    for answer in [
        Envelope::new(99, Response::Error(error.clone())),
        Envelope::new(1, Response::Status(status())),
        Envelope::new(1, fetched.clone()),
        Envelope::new(1, Response::Done { ok: true }),
    ] {
        theirs.write_all(&frames::encode(&answer).unwrap()).unwrap();
    }
    let mut client = Client::over(Link::Unix(ours));
    let mut seen = Vec::new();
    let ok = client.call_ref(Request::Status, |it, buffered| seen.push((it.into_owned(), buffered))).unwrap();
    assert!(ok);
    assert_eq!(seen, [
        (Response::Status(status()), true),
        (fetched, true),
        (Response::Done { ok: true }, false),
    ]);
    assert!(client.buffered(), "the other request's response is kept");
    assert_eq!(client.receive().unwrap(), Envelope::new(99, Response::Error(error)));
    assert!(!client.buffered());
    drop(theirs);
}

#[test]
fn a_call_in_place_refuses_another_schema_by_name() {
    let (mut client, _daemon) =
        unix_daemon(vec![Envelope { schema: 99, id: 0, body: Response::Done { ok: true } }]);
    let error = client.call_ref(Request::Status, |_, _| {}).unwrap_err();
    assert!(matches!(error, ClientError::Schema { theirs: 99 }), "{error:?}");
}

#[test]
fn over_http_a_call_in_place_reads_the_chunked_frames() {
    let (at, daemon) = http_daemon(2, None);
    let mut client = Client::http(at, "tok".to_owned()).unwrap();
    let mut seen = Vec::new();
    assert!(client.call_ref(Request::Status, |it, _| seen.push(it.into_owned())).unwrap());
    assert_eq!(seen, [Response::Status(status()), Response::Done { ok: true }]);
    assert!(client.call(Request::Status, |_| {}).unwrap(), "the connection is ready for the next");
    assert_eq!(daemon.join().unwrap().len(), 2);
}

#[test]
fn ids_are_chosen_by_the_client_and_increase() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let mut client = Client::over(Link::Unix(ours));
    assert_eq!(client.send(Request::Status).unwrap(), 1);
    assert_eq!(client.send(Request::Cancel { id: 1 }).unwrap(), 2);
}

#[test]
fn another_schema_is_refused_by_name() {
    let (mut client, _daemon) =
        unix_daemon(vec![Envelope { schema: 99, id: 0, body: Response::Done { ok: true } }]);
    let error = client.call(Request::Status, |_| {}).unwrap_err();
    assert!(matches!(error, ClientError::Schema { theirs: 99 }), "{error:?}");
    assert_eq!(error.to_string(), format!("the daemon speaks schema 99, this client {SCHEMA}"));
}

#[test]
fn a_daemon_that_hangs_up_is_an_io_error() {
    let (mut client, daemon) = unix_daemon(Vec::new());
    let error = client.call(Request::Status, |_| {}).unwrap_err();
    daemon.join().unwrap();
    assert!(matches!(&error, ClientError::Io(it) if it.kind() == io::ErrorKind::UnexpectedEof));
}

/// A daemon on loopback TCP that takes one connection and answers `requests` requests on it,
/// each with a status and its Done, or with `refuse` when given; it returns what it was sent.
fn http_daemon(
    requests: usize,
    refuse: Option<Vec<u8>>,
) -> (SocketAddr, JoinHandle<Vec<Envelope<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap();
    let daemon = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut parser = RequestParser::new(1 << 20);
        let mut seen = Vec::new();
        let mut buffer = [0; 4096];
        while seen.len() < requests {
            let request = loop {
                if let Some(request) = parser.next().unwrap() {
                    break request;
                }
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "the client hung up");
                parser.push(&buffer[..read]);
            };
            assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/api"));
            assert_eq!(request.header("Authorization"), Some("Bearer tok"));
            assert_eq!(request.header("Content-Type"), Some("application/cbor"));
            let envelope: Envelope<Request> = frames::decode(&request.body).unwrap();
            if let Some(refusal) = &refuse {
                stream.write_all(refusal).unwrap();
                seen.push(envelope);
                continue;
            }
            stream.write_all(&http::chunked(200, &[("Content-Type", "application/cbor")])).unwrap();
            for response in [Response::Status(status()), Response::Done { ok: true }] {
                let frame = frames::encode(&Envelope::new(envelope.id, response)).unwrap();
                stream.write_all(&http::chunk(&frame)).unwrap();
            }
            stream.write_all(&http::last_chunk()).unwrap();
            seen.push(envelope);
        }
        seen
    });
    (at, daemon)
}

#[test]
fn over_http_a_call_reads_the_chunked_frames() {
    let (at, daemon) = http_daemon(1, None);
    let mut client = Client::http(at, "tok".to_owned()).unwrap();
    let mut seen = Vec::new();
    assert!(client.call(Request::Status, |it| seen.push(it.clone())).unwrap());
    assert_eq!(seen, [Response::Status(status()), Response::Done { ok: true }]);
    assert_eq!(daemon.join().unwrap(), [Envelope::new(1, Request::Status)]);
}

#[test]
fn over_http_a_second_send_keeps_the_first_ones_rest_on_one_connection() {
    let (at, daemon) = http_daemon(3, None);
    let mut client = Client::http(at, "tok".to_owned()).unwrap();
    assert!(client.call(Request::Status, |_| {}).unwrap());
    assert_eq!(client.send(Request::Status).unwrap(), 2);
    assert_eq!(client.send(Request::Cancel { id: 2 }).unwrap(), 3);
    let got: Vec<Envelope<Response>> = (0..4).map(|_| client.receive().unwrap()).collect();
    assert_eq!(got, [
        Envelope::new(2, Response::Status(status())),
        Envelope::new(2, Response::Done { ok: true }),
        Envelope::new(3, Response::Status(status())),
        Envelope::new(3, Response::Done { ok: true }),
    ]);
    assert!(client.receive().is_err(), "nothing is outstanding");
    assert_eq!(daemon.join().unwrap().len(), 3);
}

#[test]
fn over_http_an_error_status_is_refused_with_its_body() {
    let refusal = http::response(401, &[("WWW-Authenticate", "Bearer")], b"a wrong token");
    let (at, daemon) = http_daemon(1, Some(refusal));
    let mut client = Client::http(at, "tok".to_owned()).unwrap();
    let error = client.call(Request::Status, |_| {}).unwrap_err();
    let ClientError::Refused(error) = error else { panic!("{error:?}") };
    assert_eq!(error.what, "HTTP 401: a wrong token");
    assert_eq!(error.at, Some(format!("http://{at}/api")));
    daemon.join().unwrap();
}

#[test]
fn errors_say_what_and_where() {
    let refused = ClientError::Refused(ApiError::new("no such head", Some("main".to_owned())));
    assert_eq!(refused.to_string(), "no such head (at main)");
    let io = ClientError::Io(io::Error::other("reset"));
    assert_eq!(io.to_string(), "the daemon's connection: reset");
    assert!(std::error::Error::source(&io).is_some());
}

#[test]
fn the_token_is_not_in_debug() {
    let (at, _daemon) = http_daemon(0, None);
    let client = Client::http(at, "hidden-token".to_owned()).unwrap();
    assert!(!format!("{client:?}").contains("hidden-token"));
}

#[test]
fn until_hung_up_waits_for_the_end_and_keeps_what_came_first() {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let daemon = thread::spawn(move || {
        theirs.write_all(&frames::encode(&Envelope::new(3, Response::Done { ok: true })).unwrap()).unwrap();
    });
    let mut client = Client::over(Link::Unix(ours));
    client.until_hung_up(std::time::Duration::from_secs(10)).unwrap();
    daemon.join().unwrap();
    assert_eq!(client.receive().unwrap(), Envelope::new(3, Response::Done { ok: true }));
}

#[test]
fn until_hung_up_gives_up_after_its_patience() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let mut client = Client::over(Link::Unix(ours));
    let error = client.until_hung_up(std::time::Duration::from_millis(20)).unwrap_err();
    assert!(error.to_string().contains("still holds the connection after 20 ms"), "{error}");
    let (at, _daemon) = http_daemon(0, None);
    let mut client = Client::http(at, "tok".to_owned()).unwrap();
    assert!(matches!(client.until_hung_up(std::time::Duration::from_millis(20)), Err(ClientError::Refused(_))));
}
