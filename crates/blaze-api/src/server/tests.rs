use blaze_core::IoError;
use blaze_ids::rng::SplitMix64 as Rng;
use blaze_protocol::cbor;

use super::*;
use crate::{
    Status,
    http::{HttpResponse, ResponseParser},
};

const AUTHORIZATION: &str = "Bearer s3cret-token";

fn config(unix: bool, http: bool) -> Config {
    Config {
        unix: unix.then(|| Unix::Bind(FileName::new("run/blaze.sock").unwrap())),
        http: http.then(|| Http {
            at: "127.0.0.1:7878".parse().unwrap(),
            token: "s3cret-token".to_owned(),
        }),
        tags: 1 << 40,
        most_connections: 8,
    }
}

fn status() -> Status {
    Status { pid: 6, requests: vec![1, 2], views: 3, history: None, memory_bytes: 4, budget_bytes: 5, build: crate::Build::default() }
}

/// The server and a driver played by hand: what it asked for and has not been told of.
struct Rig {
    server: Server,
    in_flight: Vec<Effect>,
    arrived: Vec<Arrived>,
}

impl Rig {
    fn new(config: Config) -> Rig {
        let mut effects = Vec::new();
        let server = Server::new(config, &mut effects);
        let mut rig = Rig { server, in_flight: Vec::new(), arrived: Vec::new() };
        rig.took(effects);
        rig
    }

    fn took(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            assert!(self.server.owns(effect.tag()), "{effect:?}");
            self.in_flight.push(effect);
        }
    }

    fn complete(&mut self, tag: Tag, done: Done) {
        if !matches!(done, Done::Accepted { .. }) {
            self.in_flight.retain(|it| it.tag() != tag);
        }
        let mut effects = Vec::new();
        self.server.complete(Completion { tag, done }, &mut effects, &mut self.arrived);
        self.took(effects);
    }

    fn listen(&self, unix: bool) -> Option<Tag> {
        self.in_flight.iter().find_map(|it| match it {
            Effect::Listen { at: Address::Unix(_) | Address::Inherited { .. }, tag } if unix => Some(*tag),
            Effect::Listen { at: Address::Tcp(_), tag } if !unix => Some(*tag),
            _ => None,
        })
    }

    fn accept(&mut self, unix: bool, conn: u64) -> Conn {
        let tag = self.listen(unix).expect("a listen in flight");
        self.complete(tag, Done::Accepted { conn: Conn(conn) });
        Conn(conn)
    }

    fn receiving(&self, conn: Conn) -> Option<Tag> {
        self.in_flight.iter().find_map(|it| match it {
            Effect::Recv { conn: of, most, tag, .. } if *of == conn => {
                assert!((64 << 10..=LONGEST_REQUEST as usize).contains(most), "{most}");
                Some(*tag)
            }
            _ => None,
        })
    }

    fn deliver(&mut self, conn: Conn, bytes: &[u8]) {
        let (tag, into) = self.received_into(conn, bytes);
        self.complete(tag, Done::Read(into));
    }

    /// The receive in flight on `conn` taken out, its buffer with `bytes` appended, as the
    /// driver appends them: after what it held, in room reserved for `most`.
    fn received_into(&mut self, conn: Conn, bytes: &[u8]) -> (Tag, Vec<u8>) {
        let tag = self.receiving(conn).expect("a receive in flight");
        let at = self.in_flight.iter().position(|it| it.tag() == tag).expect("the receive");
        let Effect::Recv { most, mut into, .. } = self.in_flight.remove(at) else {
            unreachable!("found as a receive")
        };
        into.reserve(most.max(bytes.len()));
        into.extend_from_slice(bytes);
        (tag, into)
    }

    fn sends(&self, conn: Conn) -> Vec<(Tag, Vec<u8>)> {
        let sends = self.in_flight.iter().filter_map(|it| match it {
            Effect::Send { conn: of, bytes, tag } if *of == conn => Some((*tag, bytes.clone())),
            _ => None,
        });
        sends.collect()
    }

    /// Every byte sent on `conn`, each send completing, until none is in flight.
    fn sent(&mut self, conn: Conn) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let sends = self.sends(conn);
            assert!(sends.len() <= 1, "more than one send in flight");
            let Some((tag, bytes)) = sends.into_iter().next() else { return out };
            out.extend(bytes);
            self.complete(tag, Done::Sent(Vec::new()));
        }
    }

    fn hung_up(&self, conn: Conn) -> bool {
        self.in_flight.iter().any(|it| matches!(it, Effect::Hangup { conn: of, .. } if *of == conn))
    }

    /// The hangups asked for, done.
    fn hang_ups_done(&mut self) {
        let tags: Vec<Tag> = self
            .in_flight
            .iter()
            .filter(|it| matches!(it, Effect::Hangup { .. }))
            .map(Effect::tag)
            .collect();
        for tag in tags {
            self.complete(tag, Done::HungUp);
        }
    }

    fn reply(&mut self, conn: Conn, id: u64, response: Response) {
        let mut effects = Vec::new();
        self.server.reply(Reply { to: RequestRef { conn, id }, response }, &mut effects);
        self.took(effects);
    }

    fn arrived(&mut self) -> Vec<Arrived> {
        std::mem::take(&mut self.arrived)
    }
}

fn frame_of(id: u64, request: Request) -> Vec<u8> {
    frames::encode(&Envelope::new(id, request)).unwrap()
}

fn framed(item: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    blaze_protocol::write_frame(&mut bytes, item).unwrap();
    bytes
}

fn asked(conn: Conn, id: u64, request: Request) -> Arrived {
    Arrived::Request { from: RequestRef { conn, id }, request }
}

fn responses(bytes: &[u8]) -> Vec<Envelope<Response>> {
    let mut frames = Frames::new();
    frames.push(bytes);
    let mut out = Vec::new();
    while let Some(item) = frames.next().unwrap() {
        out.push(frames::decode(&item).unwrap());
    }
    assert_eq!(frames.held(), 0, "bytes after the last frame");
    out
}

fn post(item: &[u8], headers: &[(&str, &str)]) -> Vec<u8> {
    http::request("POST", "daemon", "/api", headers, item)
}

fn authorized_post(id: u64, request: Request) -> Vec<u8> {
    let item = cbor::to_vec(&Envelope::new(id, request)).unwrap();
    post(&item, &[("Authorization", AUTHORIZATION), ("Content-Type", "application/cbor")])
}

/// Whole HTTP responses, one after another.
fn http_responses(bytes: &[u8]) -> Vec<HttpResponse> {
    let mut out = Vec::new();
    let mut rest = bytes.to_vec();
    while !rest.is_empty() {
        let mut parser = ResponseParser::new(usize::MAX);
        parser.push(&rest).unwrap();
        assert!(parser.finished(), "a response cut short: {:?}", String::from_utf8_lossy(&rest));
        let (status, headers) = parser.head().unwrap();
        let headers = headers.to_vec();
        out.push(HttpResponse { status, headers, body: parser.take_body() });
        rest = parser.rest();
    }
    out
}

#[test]
fn it_listens_on_each_address_with_tags_of_its_own() {
    let rig = Rig::new(config(true, true));
    assert!(rig.listen(true).is_some() && rig.listen(false).is_some());
    assert_eq!(rig.server.failed(), None);
    assert!(rig.server.owns(Tag(1 << 40)) && rig.server.owns(Tag((1 << 40) + (1 << 32) - 1)));
    assert!(!rig.server.owns(Tag((1 << 40) - 1)) && !rig.server.owns(Tag((1 << 40) + (1 << 32))));
}

#[test]
fn a_request_in_one_frame_arrives_and_its_replies_go_back() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    rig.deliver(conn, &frame_of(5, Request::Status));
    assert_eq!(rig.arrived(), [asked(conn, 5, Request::Status)]);
    rig.reply(conn, 5, Response::Status(status()));
    rig.reply(conn, 5, Response::Done { ok: true });
    assert_eq!(responses(&rig.sent(conn)), [
        Envelope::new(5, Response::Status(status())),
        Envelope::new(5, Response::Done { ok: true }),
    ]);
    assert!(rig.receiving(conn).is_some());
}

#[test]
fn a_request_split_across_many_receives_arrives_once() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let bytes = frame_of(7, Request::Cancel { id: 3 });
    for byte in &bytes {
        assert!(rig.arrived().is_empty());
        rig.deliver(conn, std::slice::from_ref(byte));
    }
    assert_eq!(rig.arrived(), [asked(conn, 7, Request::Cancel { id: 3 })]);
}

#[test]
fn receives_of_a_large_frame_grow_with_what_has_come_of_it() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let ops = vec![blaze_vfs::Op::Write {
        path: "big.rs".to_owned(),
        bytes: vec![0; 4 << 20],
        mode: blaze_vfs::Mode::Regular,
    }];
    let request = Request::Commit { head: "main".to_owned(), base: Default::default(), ops };
    let bytes = frame_of(1, request.clone());
    let mut at = 0;
    let mut receives = 0;
    while at < bytes.len() {
        let most = rig
            .in_flight
            .iter()
            .find_map(|it| match it {
                Effect::Recv { conn: of, most, .. } if *of == conn => Some(*most),
                _ => None,
            })
            .unwrap();
        assert!(most <= (64 << 10).max(at), "{most} asked with {at} here");
        let here = most.min(bytes.len() - at);
        rig.deliver(conn, &bytes[at..at + here]);
        at += here;
        receives += 1;
    }
    assert!(receives < 16, "{receives} receives");
    assert_eq!(rig.arrived(), [asked(conn, 1, request)]);
    let most = rig.in_flight.iter().find_map(|it| match it {
        Effect::Recv { most, .. } => Some(*most),
        _ => None,
    });
    assert_eq!(most, Some(64 << 10), "back to the least once the frame is in");
}

#[test]
fn pipelined_requests_arrive_in_order_and_replies_go_in_any() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let mut bytes = frame_of(1, Request::Status);
    bytes.extend(frame_of(2, Request::Cancel { id: 1 }));
    rig.deliver(conn, &bytes);
    assert_eq!(rig.arrived(), [
        asked(conn, 1, Request::Status),
        asked(conn, 2, Request::Cancel { id: 1 }),
    ]);
    rig.reply(conn, 2, Response::Done { ok: true });
    rig.reply(conn, 1, Response::Done { ok: false });
    assert_eq!(responses(&rig.sent(conn)), [
        Envelope::new(2, Response::Done { ok: true }),
        Envelope::new(1, Response::Done { ok: false }),
    ]);
}

#[test]
fn another_schema_is_answered_under_its_id_and_the_connection_carries_on() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let item = cbor::to_vec(&Envelope { schema: 2, id: 4, body: Request::Status }).unwrap();
    rig.deliver(conn, &framed(&item));
    assert!(rig.arrived().is_empty());
    let out = responses(&rig.sent(conn));
    let [error, done] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(error.id, 4);
    assert!(
        matches!(&error.body, Response::Error(it) if it.what.starts_with("schema 2, this daemon speaks")),
        "{error:?}"
    );
    assert_eq!(*done, Envelope::new(4, Response::Done { ok: false }));
    rig.deliver(conn, &frame_of(5, Request::Status));
    assert_eq!(rig.arrived(), [asked(conn, 5, Request::Status)]);
}

#[test]
fn an_item_that_is_no_envelope_is_answered_under_id_0() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    rig.deliver(conn, &framed(&[0xff]));
    let out = responses(&rig.sent(conn));
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|it| it.id == 0));
    assert!(matches!(out[0].body, Response::Error(_)));
    assert_eq!(out[1].body, Response::Done { ok: false });
    assert!(!rig.hung_up(conn) && rig.receiving(conn).is_some());
}

#[test]
fn a_frame_too_long_is_answered_then_hung_up_on() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    rig.deliver(conn, &[0xff; 4]);
    assert!(rig.receiving(conn).is_none(), "it reads no more");
    let out = responses(&rig.sent(conn));
    assert!(matches!(&out[0].body, Response::Error(it) if it.what.contains("longer than")));
    assert_eq!(out[1], Envelope::new(0, Response::Done { ok: false }));
    assert!(rig.hung_up(conn));
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
}

#[test]
fn a_reply_to_a_connection_that_is_gone_is_dropped() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    rig.deliver(conn, &frame_of(1, Request::Status));
    rig.arrived();
    rig.deliver(conn, &[]);
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
    assert!(rig.hung_up(conn));
    rig.reply(conn, 1, Response::Done { ok: true });
    assert!(rig.sends(conn).is_empty());
}

#[test]
fn a_failed_receive_or_send_ends_the_connection() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let tag = rig.receiving(conn).unwrap();
    rig.complete(tag, Done::Failed(IoError("reset".to_owned())));
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);

    let conn = rig.accept(true, 2);
    rig.reply(conn, 1, Response::Done { ok: true });
    let (tag, _) = rig.sends(conn)[0].clone();
    rig.complete(tag, Done::Failed(IoError("broken pipe".to_owned())));
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
    assert!(rig.hung_up(conn));
}

#[test]
fn one_send_is_in_flight_and_later_replies_are_concatenated_behind_it() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    for id in 1..=3 {
        rig.reply(conn, id, Response::Done { ok: true });
    }
    let sends = rig.sends(conn);
    assert_eq!(sends.len(), 1);
    assert_eq!(responses(&sends[0].1), [Envelope::new(1, Response::Done { ok: true })]);
    rig.complete(sends[0].0, Done::Sent(Vec::new()));
    let sends = rig.sends(conn);
    assert_eq!(sends.len(), 1);
    assert_eq!(responses(&sends[0].1), [
        Envelope::new(2, Response::Done { ok: true }),
        Envelope::new(3, Response::Done { ok: true }),
    ]);
}

#[test]
fn a_peer_that_lets_more_than_64_mib_queue_is_hung_up_on() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let big = Response::Error(ApiError::new("x".repeat(8 << 20), None));
    for id in 0..9 {
        assert!(!rig.hung_up(conn), "hung up after {id} replies");
        rig.reply(conn, id, big.clone());
    }
    assert!(rig.hung_up(conn));
    rig.hang_ups_done();
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
    rig.reply(conn, 10, Response::Done { ok: true });
    assert_eq!(rig.sends(conn).len(), 1, "only the send in flight before");
}

#[test]
fn a_response_too_long_to_frame_is_sent_as_the_error_that_says_so() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 1);
    let huge = Response::Error(ApiError::new("x".repeat(17 << 20), None));
    rig.reply(conn, 6, huge);
    let out = responses(&rig.sent(conn));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, 6);
    assert!(matches!(&out[0].body, Response::Error(it) if it.what.contains("could not be sent")));
}

#[test]
fn a_connection_over_the_most_is_hung_up_on_at_once() {
    let mut rig = Rig::new(Config { most_connections: 1, ..config(true, false) });
    let first = rig.accept(true, 1);
    let second = rig.accept(true, 2);
    assert!(rig.receiving(first).is_some() && !rig.hung_up(first));
    assert!(rig.receiving(second).is_none() && rig.hung_up(second));
    assert!(rig.listen(true).is_some(), "it still listens");
}

#[test]
fn a_listen_that_fails_is_named_and_not_listened_on_again() {
    let mut rig = Rig::new(config(true, true));
    let tag = rig.listen(true).unwrap();
    rig.complete(tag, Done::Failed(IoError("address in use".to_owned())));
    let failed = rig.server.failed().unwrap();
    assert!(failed.contains("run/blaze.sock") && failed.contains("address in use"), "{failed}");
    assert!(rig.listen(true).is_none());
    assert!(rig.listen(false).is_some());
}

#[cfg(unix)]
#[test]
fn a_passed_socket_is_listened_on_as_inherited_and_kept_open() {
    use std::os::{fd::OwnedFd, unix::net::UnixListener};

    use crate::activation::{Manager, Passed};

    let dir = std::env::temp_dir().join(format!("blaze-server-passed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("blaze.sock");
    _ = std::fs::remove_file(&path);
    let socket = Passed::new(OwnedFd::from(UnixListener::bind(&path).unwrap()), Manager::Launchd);
    let fd = socket.fd();
    let at = FileName::new("run/blaze.sock").unwrap();
    let mut rig = Rig::new(Config { unix: Some(Unix::Passed { socket, at: at.clone() }), ..config(false, false) });
    assert!(rig.server.failed().is_none());
    assert!(
        rig.in_flight.iter().any(|it| matches!(it, Effect::Listen { at: Address::Inherited { fd: of, at: name }, .. } if *of == fd && *name == at)),
        "{:?}",
        rig.in_flight
    );
    assert!(std::os::unix::net::UnixStream::connect(&path).is_ok(), "the passed socket stays open");
    let conn = rig.accept(true, 1);
    assert!(rig.receiving(conn).is_some(), "an accepted connection is read as the Unix side's");
    let tag = rig.listen(true).unwrap();
    rig.complete(tag, Done::Failed(IoError("not bound at /run/blaze.sock".to_owned())));
    let failed = rig.server.failed().unwrap();
    assert!(failed.contains(&format!("the socket run/blaze.sock passed as fd {fd}")), "{failed}");
    drop(rig);
    assert!(std::os::unix::net::UnixStream::connect(&path).is_err(), "closed with the server");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_empty_token_or_no_address_is_a_failure() {
    let mut config = config(false, true);
    if let Some(http) = &mut config.http {
        http.token = String::new();
    }
    let rig = Rig::new(config);
    assert!(rig.server.failed().unwrap().contains("empty token"));
    assert!(rig.listen(false).is_none());
    let rig = Rig::new(Config { unix: None, http: None, tags: 0, most_connections: 1 });
    assert!(rig.server.failed().is_some());
}

#[test]
fn the_token_is_never_shown() {
    let http = config(false, true).http.unwrap();
    assert!(!format!("{http:?}").contains("s3cret"));
    assert!(!format!("{:?}", config(false, true)).contains("s3cret"));
}

#[test]
fn stop_unlistens_and_hangs_up_every_connection() {
    let mut rig = Rig::new(config(true, true));
    let unix = rig.accept(true, 1);
    let web = rig.accept(false, 2);
    let listens = [rig.listen(true).unwrap(), rig.listen(false).unwrap()];
    let mut effects = Vec::new();
    rig.server.stop(&mut effects);
    rig.took(effects);
    for tag in listens {
        assert!(rig.in_flight.iter().any(|it| *it == Effect::Unlisten { tag }));
    }
    assert!(rig.hung_up(unix) && rig.hung_up(web));
    rig.deliver(unix, &[]);
    assert_eq!(rig.arrived(), [Arrived::Gone { conn: unix }, Arrived::Gone { conn: web }]);
    rig.complete(listens[0], Done::Accepted { conn: Conn(3) });
    assert!(rig.hung_up(Conn(3)) && rig.receiving(Conn(3)).is_none());
    rig.complete(listens[0], Done::Unlistened);
    rig.complete(listens[1], Done::Unlistened);
    assert_eq!(rig.server.failed(), None);
}

#[test]
fn any_split_of_a_unix_stream_gives_the_same_arrivals_and_answers() {
    let mut stream = frame_of(1, Request::Status);
    stream.extend(framed(&cbor::to_vec(&Envelope { schema: 2, id: 4, body: Request::Status }).unwrap()));
    stream.extend(frame_of(2, Request::Cancel { id: 1 }));
    stream.extend(framed(&[0xa0]));
    stream.extend(frame_of(3, Request::Status));
    let run = |cuts: &mut dyn FnMut(usize) -> usize| {
        let mut rig = Rig::new(config(true, false));
        let conn = rig.accept(true, 1);
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let len = cuts(stream.len() - at);
            rig.deliver(conn, &stream[at..at + len]);
            at += len;
            if at % 3 == 0 {
                out.extend(rig.sent(conn));
            }
        }
        out.extend(rig.sent(conn));
        (rig.arrived(), out)
    };
    let (arrived, out) = run(&mut |left: usize| left);
    assert_eq!(arrived, [
        asked(Conn(1), 1, Request::Status),
        asked(Conn(1), 2, Request::Cancel { id: 1 }),
        asked(Conn(1), 3, Request::Status),
    ]);
    let ids: Vec<u64> = responses(&out).iter().map(|it| it.id).collect();
    assert_eq!(ids, [4, 4, 0, 0]);
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let got = run(&mut |left: usize| (1 + rng.below(40) as usize).min(left));
        assert_eq!(got, (arrived.clone(), out.clone()), "seed {seed}");
    }
}

#[test]
fn http_without_the_token_is_401_and_never_arrives() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let item = cbor::to_vec(&Envelope::new(1, Request::Status)).unwrap();
    rig.deliver(conn, &post(&item, &[("Content-Type", "application/cbor")]));
    rig.deliver(conn, &post(&item, &[
        ("Authorization", "Bearer wrong-token"),
        ("Content-Type", "application/cbor"),
    ]));
    rig.deliver(conn, &post(&item, &[
        ("Authorization", "Basic s3cret-token"),
        ("Content-Type", "application/cbor"),
    ]));
    rig.deliver(conn, &post(&item, &[
        ("Authorization", "Bearer s3cret-toke"),
        ("Content-Type", "application/cbor"),
    ]));
    assert!(rig.arrived().is_empty());
    let out = http_responses(&rig.sent(conn));
    assert_eq!(out.iter().map(|it| it.status).collect::<Vec<_>>(), [401; 4]);
    for response in &out {
        assert_eq!(response.header("WWW-Authenticate"), Some("Bearer"));
        let body = String::from_utf8_lossy(&response.body);
        assert!(!body.contains("s3cret") && !body.contains("wrong-token"), "{body}");
    }
    assert!(rig.receiving(conn).is_some(), "kept alive");
}

#[test]
fn http_with_the_token_arrives_and_is_answered_in_chunks_of_frames() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let item = cbor::to_vec(&Envelope::new(5, Request::Status)).unwrap();
    rig.deliver(conn, &post(&item, &[
        ("authorization", "bearer s3cret-token"),
        ("Content-Type", "application/cbor; charset=binary"),
    ]));
    assert_eq!(rig.arrived(), [asked(conn, 5, Request::Status)]);
    rig.reply(conn, 5, Response::Status(status()));
    rig.reply(conn, 5, Response::Done { ok: true });
    let out = http_responses(&rig.sent(conn));
    let [response] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Content-Type"), Some("application/cbor"));
    assert_eq!(response.header("Transfer-Encoding"), Some("chunked"));
    assert_eq!(responses(&response.body), [
        Envelope::new(5, Response::Status(status())),
        Envelope::new(5, Response::Done { ok: true }),
    ]);
    assert!(rig.receiving(conn).is_some() && !rig.hung_up(conn));
}

#[test]
fn http_asking_for_json_is_answered_in_lines() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let item = cbor::to_vec(&Envelope::new(8, Request::Status)).unwrap();
    rig.deliver(conn, &post(&item, &[
        ("Authorization", AUTHORIZATION),
        ("Content-Type", "application/cbor"),
        ("Accept", "text/html, application/json;q=0.9"),
    ]));
    rig.arrived();
    rig.reply(conn, 8, Response::Status(status()));
    rig.reply(conn, 8, Response::Done { ok: true });
    let out = http_responses(&rig.sent(conn));
    assert_eq!(out[0].header("Content-Type"), Some("application/x-ndjson"));
    let body = String::from_utf8(out[0].body.clone()).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines, [
        json::response(&Envelope::new(8, Response::Status(status()))),
        json::response(&Envelope::new(8, Response::Done { ok: true })),
    ]);
    assert!(body.ends_with('\n'));
}

#[test]
fn http_to_another_path_or_with_another_method_or_body_is_refused() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let auth = ("Authorization", AUTHORIZATION);
    rig.deliver(conn, &http::request("POST", "d", "/other", &[auth], b""));
    rig.deliver(conn, &http::request("GET", "d", "/api", &[auth], b""));
    rig.deliver(conn, &http::request("POST", "d", "/api", &[auth, ("Content-Type", "text/plain")], b"x"));
    assert!(rig.arrived().is_empty());
    let out = http_responses(&rig.sent(conn));
    assert_eq!(out.iter().map(|it| it.status).collect::<Vec<_>>(), [404, 405, 415]);
    assert_eq!(out[1].header("Allow"), Some("POST"));
}

#[test]
fn an_http_body_that_is_no_envelope_is_answered_and_the_connection_carries_on() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let headers = [("Authorization", AUTHORIZATION), ("Content-Type", "application/cbor")];
    rig.deliver(conn, &post(&[0xff], &headers));
    assert!(rig.arrived().is_empty());
    let out = http_responses(&rig.sent(conn));
    assert_eq!(out[0].status, 200);
    let answers = responses(&out[0].body);
    assert_eq!(answers.len(), 2);
    assert!(matches!(answers[0], Envelope { id: 0, body: Response::Error(_), .. }));
    assert_eq!(answers[1], Envelope::new(0, Response::Done { ok: false }));
    rig.deliver(conn, &authorized_post(2, Request::Status));
    assert_eq!(rig.arrived(), [asked(conn, 2, Request::Status)]);
}

#[test]
fn a_pipelined_http_request_waits_for_the_first_ones_end() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let mut bytes = authorized_post(1, Request::Status);
    bytes.extend(authorized_post(2, Request::Cancel { id: 1 }));
    rig.deliver(conn, &bytes);
    assert_eq!(rig.arrived(), [asked(conn, 1, Request::Status)]);
    rig.reply(conn, 2, Response::Done { ok: true });
    let mut out = rig.sent(conn);
    assert!(rig.arrived().is_empty(), "the second waits");
    rig.reply(conn, 1, Response::Done { ok: true });
    out.extend(rig.sent(conn));
    assert_eq!(rig.arrived(), [asked(conn, 2, Request::Cancel { id: 1 })]);
    rig.reply(conn, 2, Response::Done { ok: false });
    out.extend(rig.sent(conn));
    let out = http_responses(&out);
    assert_eq!(out.len(), 2);
    assert_eq!(responses(&out[0].body), [Envelope::new(1, Response::Done { ok: true })]);
    assert_eq!(responses(&out[1].body), [Envelope::new(2, Response::Done { ok: false })]);
}

#[test]
fn an_http_connection_that_asks_to_close_is_hung_up_on_after_its_answer() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    let item = cbor::to_vec(&Envelope::new(1, Request::Status)).unwrap();
    rig.deliver(conn, &post(&item, &[
        ("Authorization", AUTHORIZATION),
        ("Content-Type", "application/cbor"),
        ("Connection", "close"),
    ]));
    rig.arrived();
    rig.reply(conn, 1, Response::Done { ok: true });
    let out = http_responses(&rig.sent(conn));
    assert_eq!(out.len(), 1);
    assert!(rig.hung_up(conn));
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
}

#[test]
fn bytes_that_are_no_http_are_answered_then_hung_up_on() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    rig.deliver(conn, b"GARBAGE\r\n\r\n");
    assert!(rig.receiving(conn).is_none());
    let out = http_responses(&rig.sent(conn));
    assert_eq!(out[0].status, 400);
    assert_eq!(out[0].header("Connection"), Some("close"));
    assert!(rig.hung_up(conn));
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
}

#[test]
fn an_http_hangup_is_gone_and_a_reply_to_another_id_is_dropped() {
    let mut rig = Rig::new(config(false, true));
    let conn = rig.accept(false, 1);
    rig.deliver(conn, &authorized_post(1, Request::Status));
    rig.arrived();
    rig.sent(conn);
    rig.reply(conn, 9, Response::Done { ok: true });
    assert!(rig.sends(conn).is_empty());
    rig.deliver(conn, &[]);
    assert_eq!(rig.arrived(), [Arrived::Gone { conn }]);
    rig.reply(conn, 1, Response::Done { ok: true });
    assert!(rig.sends(conn).is_empty());
}

#[test]
fn any_split_of_an_http_stream_gives_the_same_arrivals_and_answers() {
    let headers = [("Authorization", AUTHORIZATION), ("Content-Type", "application/cbor")];
    let mut stream = authorized_post(1, Request::Status);
    stream.extend(post(&[0xff], &headers));
    stream.extend(authorized_post(3, Request::Cancel { id: 1 }));
    let run = |cuts: &mut dyn FnMut(usize) -> usize| {
        let mut rig = Rig::new(config(false, true));
        let conn = rig.accept(false, 1);
        let mut arrived = Vec::new();
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let len = cuts(stream.len() - at);
            rig.deliver(conn, &stream[at..at + len]);
            at += len;
            loop {
                out.extend(rig.sent(conn));
                let now = rig.arrived();
                if now.is_empty() {
                    break;
                }
                for it in &now {
                    if let Arrived::Request { from, .. } = it {
                        rig.reply(conn, from.id, Response::Done { ok: true });
                    }
                }
                arrived.extend(now);
            }
        }
        (arrived, out)
    };
    let (arrived, out) = run(&mut |left: usize| left);
    assert_eq!(arrived, [
        asked(Conn(1), 1, Request::Status),
        asked(Conn(1), 3, Request::Cancel { id: 1 }),
    ]);
    assert_eq!(http_responses(&out).len(), 3);
    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let got = run(&mut |left: usize| (1 + rng.below(50) as usize).min(left));
        assert_eq!(got, (arrived.clone(), out.clone()), "seed {seed}");
    }
}

/// A warm request on a Unix connection, read from the bytes received and handed over,
/// allocates nothing, and nor does its reply: the send buffer comes back with the send's end
/// and is the next reply's. On an HTTP connection kept alive the request's read also writes the
/// reply's head into a send buffer, and the reply is written while that send is in flight:
/// one allocation each. On both, a warm receive is lent a buffer with room for what it reads,
/// so the driver's read allocates nothing either.
#[test]
fn a_warm_request_and_its_reply_reuse_their_buffers() {
    for unix in [true, false] {
        let mut rig = Rig::new(config(unix, !unix));
        let conn = rig.accept(unix, 1);
        let (mut effects, mut arrived) = (Vec::with_capacity(16), Vec::with_capacity(16));
        for id in 0..20u64 {
            let warm = id >= 2;
            let bytes = match unix {
                true => frame_of(id, Request::Cancel { id }),
                false => authorized_post(id, Request::Cancel { id }),
            };
            if warm {
                let lent = rig.in_flight.iter().find_map(|it| match it {
                    Effect::Recv { conn: of, most, into, .. } if *of == conn => {
                        Some(into.capacity() >= into.len() + most)
                    }
                    _ => None,
                });
                assert_eq!(lent, Some(true), "{unix}: a warm receive is given room for what it reads");
            }
            let (tag, into) = rig.received_into(conn, &bytes);
            let (_, read) = blaze_alloc_count::measure(|| {
                rig.server.complete(Completion { tag, done: Done::Read(into) }, &mut effects, &mut arrived)
            });
            let head = u64::from(!unix);
            assert!(!warm || read.allocations <= head, "{unix}: a warm request read: {read:?}");
            assert!(matches!(arrived[..], [Arrived::Request { request: Request::Cancel { id: got }, .. }] if got == id));
            arrived.clear();
            rig.in_flight.extend(effects.drain(..));
            let to = RequestRef { conn, id };
            let (_, replied) = blaze_alloc_count::measure(|| {
                rig.server.reply(Reply { to, response: Response::Done { ok: true } }, &mut effects)
            });
            assert!(!warm || replied.allocations <= head, "{unix}: a warm reply: {replied:?}");
            rig.in_flight.extend(effects.drain(..));
            let sending = |rig: &mut Rig| {
                let at = rig.in_flight.iter().position(|it| matches!(it, Effect::Send { conn: of, .. } if *of == conn))?;
                match rig.in_flight.remove(at) {
                    Effect::Send { mut bytes, tag, .. } => {
                        bytes.clear();
                        Some((tag, bytes))
                    }
                    _ => unreachable!("found as a send"),
                }
            };
            let mut sends = 0;
            while let Some((sent, bytes)) = sending(&mut rig) {
                let (_, done) = blaze_alloc_count::measure(|| {
                    rig.server.complete(Completion { tag: sent, done: Done::Sent(bytes) }, &mut effects, &mut arrived)
                });
                assert!(!warm || done.allocations == 0, "{unix}: a warm send's end: {done:?}");
                rig.in_flight.extend(effects.drain(..));
                sends += 1;
            }
            assert_eq!(sends, 1 + head, "{unix}: the reply, and on HTTP its head before it");
            assert!(arrived.is_empty() && !rig.hung_up(conn), "{unix}");
        }
    }
}

/// A connection waits while a send of it is in flight, and not once it completed; a stream on a
/// clock skips the ticks it waits through.
#[test]
fn a_connection_waits_while_its_send_is_in_flight() {
    let mut rig = Rig::new(config(true, false));
    let conn = rig.accept(true, 7);
    assert!(!rig.server.waiting(conn), "nothing queued yet");
    rig.reply(conn, 1, Response::Done { ok: true });
    assert!(rig.server.waiting(conn), "a send in flight");
    let sent = rig.sent(conn);
    assert!(!sent.is_empty());
    assert!(!rig.server.waiting(conn), "the send completed");
    assert!(!rig.server.waiting(Conn(99)), "a connection the server has none of waits on nothing");
}
