//! The core driven by hand: scripted bytes in, a clock the test sets, and what it asks and
//! tells checked. No socket, runtime or real clock.

use std::{
    io::{Read as _, Write as _},
    sync::Arc,
    time::Duration,
};

use crate::{
    Client, Completion, Config, ConnId, Error, Event, IDLE_FOR, Io, Kind, Request, RequestId, Step,
    client::redirect_to,
};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A client for plain HTTP: its TLS has no roots, and no test of it reaches TLS.
fn client() -> Client {
    let tls = crate::roots::with_roots(rustls::RootCertStore::empty()).unwrap();
    Client::new(Config::new(tls, "blaze-test", TIMEOUT))
}

fn ios(client: &mut Client) -> Vec<Io> {
    std::iter::from_fn(|| client.next_io()).collect()
}

fn events(client: &mut Client) -> Vec<Event> {
    std::iter::from_fn(|| client.next_event()).collect()
}

/// Every send the client asks of `conn` until it asks none, each completed: their bytes. The
/// other IO it asks meanwhile is dropped.
fn sent(client: &mut Client, conn: ConnId) -> Vec<u8> {
    let mut written = Vec::new();
    for _ in 0..16 {
        let mut any = false;
        for io in ios(client) {
            if let Io::Send { conn: on, bytes } = io {
                assert_eq!(on, conn, "a send on another connection");
                written.extend_from_slice(&bytes);
                any = true;
            }
        }
        if !any {
            return written;
        }
        client.complete(Completion::Sent { conn });
    }
    panic!("the client kept sending on {conn:?}");
}

/// Starts `request` as `id` on a new plain connection, connected and its request sent: the
/// connection, and the request as written.
fn open(client: &mut Client, id: u64, request: Request) -> (ConnId, Vec<u8>) {
    client.request(RequestId(id), request).unwrap();
    let Some(Io::Connect { conn, .. }) = client.next_io() else { panic!("no connect asked") };
    client.complete(Completion::Connected { conn });
    let written = sent(client, conn);
    (conn, written)
}

fn received(client: &mut Client, conn: ConnId, bytes: &[u8]) {
    client.complete(Completion::Received { conn, bytes: bytes.to_vec() });
}

/// `events` as status, body and whether they ended in `Done`, for a request that did not fail.
fn answer(events: &[Event]) -> (u16, Vec<u8>, bool) {
    let mut status = 0;
    let mut body = Vec::new();
    let mut done = false;
    for event in events {
        match event {
            Event::Head { status: it, .. } => status = *it,
            Event::Body { bytes, .. } => body.extend_from_slice(bytes),
            Event::Done { .. } => done = true,
            Event::Failed { error, .. } => panic!("failed: {error:?}"),
        }
    }
    (status, body, done)
}

fn failed(events: &[Event]) -> &Error {
    match events.last() {
        Some(Event::Failed { error, .. }) => error,
        other => panic!("not a failure: {other:?}"),
    }
}

/// A chunked body, with an extension and a trailer, reads whole however its bytes are cut on
/// the way; one whose chunk size is not hex fails naming the step and the host.
#[test]
fn a_chunked_body_is_read_whole_however_it_is_cut() {
    let answer_bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
        4;ext=1\r\nWiki\r\n6\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\nTrailer: x\r\n\r\n";
    for cut in 1..=answer_bytes.len() {
        let mut client = client();
        let (conn, _) = open(&mut client, 1, Request::get("http://example.test/a"));
        for piece in answer_bytes.chunks(cut) {
            received(&mut client, conn, piece);
        }
        let got = answer(&events(&mut client));
        assert_eq!(
            got,
            (200, b"Wikipedia in \r\n\r\nchunks.".to_vec(), true),
            "cut every {cut} bytes"
        );
    }

    let mut client = client();
    let (conn, _) = open(&mut client, 1, Request::get("http://example.test/a"));
    received(
        &mut client,
        conn,
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nab\r\n",
    );
    let got = events(&mut client);
    let error = failed(&got);
    let Kind::Io { step: Step::Body, host, port: 80, why } = &error.kind else {
        panic!("{error:?}")
    };
    assert_eq!(host, "example.test");
    assert!(why.contains("not hex"), "{why}");
    assert!(ios(&mut client).contains(&Io::Close { conn }));
}

/// A kept-alive connection takes the next request to its origin with no new connect, a
/// request to another origin gets one of its own, and an idle connection closes when it has
/// been idle for `IDLE_FOR`, not before.
#[test]
fn a_kept_alive_connection_is_reused_until_it_idles_out() {
    let mut client = client();
    let (conn, written) = open(&mut client, 1, Request::get("http://example.test/a?x=1"));
    assert_eq!(
        String::from_utf8(written).unwrap(),
        "GET /a?x=1 HTTP/1.1\r\nhost: example.test\r\nuser-agent: blaze-test\r\naccept-encoding: gzip\r\n\r\n"
    );
    received(&mut client, conn, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    assert_eq!(answer(&events(&mut client)), (200, b"ok".to_vec(), true));
    ios(&mut client);

    let at = Duration::from_secs(1);
    client.advance_clock(at);
    client.request(RequestId(2), Request::get("http://example.test/b")).unwrap();
    let asked = ios(&mut client);
    assert!(!asked.iter().any(|it| matches!(it, Io::Connect { .. })), "{asked:?}");
    let Some(Io::Send { conn: on, bytes }) = asked.first() else { panic!("{asked:?}") };
    assert_eq!(*on, conn);
    assert!(bytes.starts_with(b"GET /b HTTP/1.1\r\n"));
    client.complete(Completion::Sent { conn });
    received(&mut client, conn, b"HTTP/1.1 204 No Content\r\n\r\n");
    assert_eq!(answer(&events(&mut client)), (204, Vec::new(), true));

    client.request(RequestId(3), Request::get("http://other.test:8080/")).unwrap();
    let asked = ios(&mut client);
    assert!(
        asked
            .iter()
            .any(|it| matches!(it, Io::Connect { host, port: 8080, .. } if host == "other.test")),
        "{asked:?}"
    );
    client.cancel(RequestId(3));
    ios(&mut client);

    assert_eq!(client.next_deadline(), Some(at + IDLE_FOR));
    client.advance_clock(at + IDLE_FOR - Duration::from_millis(1));
    assert!(!ios(&mut client).contains(&Io::Close { conn }));
    client.advance_clock(at + IDLE_FOR);
    assert!(ios(&mut client).contains(&Io::Close { conn }));
    assert_eq!(client.next_deadline(), None);
}

/// A pooled connection the server closed as it was taken (no byte of the answer came) gets
/// the request asked once more, on a new connection; `Connection: close` keeps a connection
/// out of the pool.
#[test]
fn a_request_its_pooled_connection_closed_under_is_asked_again() {
    let mut client = client();
    let (first, _) = open(&mut client, 1, Request::get("http://example.test/a"));
    received(&mut client, first, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    events(&mut client);
    client.request(RequestId(2), Request::get("http://example.test/b")).unwrap();
    sent(&mut client, first);
    received(&mut client, first, b"");
    let asked = ios(&mut client);
    assert!(asked.contains(&Io::Close { conn: first }), "{asked:?}");
    let Some(Io::Connect { conn: second, .. }) =
        asked.iter().find(|it| matches!(it, Io::Connect { .. })).cloned()
    else {
        panic!("{asked:?}")
    };
    client.complete(Completion::Connected { conn: second });
    assert!(sent(&mut client, second).starts_with(b"GET /b HTTP/1.1\r\n"));
    received(
        &mut client,
        second,
        b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 1\r\n\r\nk",
    );
    assert_eq!(answer(&events(&mut client)), (200, b"k".to_vec(), true));
    assert!(ios(&mut client).contains(&Io::Close { conn: second }));
}

/// A redirect is followed to its new origin, the address that answered is told with the head,
/// and the redirect's connection reads its body to the end and is kept for its origin.
#[test]
fn a_redirect_is_followed_and_its_connection_kept() {
    let mut client = client();
    let (one, _) = open(&mut client, 1, Request::get("http://one.test/a"));
    received(&mut client, one, b"HTTP/1.1 302 Found\r\nLocation: http://two.test/b?sig=1\r\nContent-Length: 5\r\n\r\nmoved");
    assert_eq!(events(&mut client), []);
    let asked = ios(&mut client);
    let Some(Io::Connect { conn: two, host, port: 80 }) =
        asked.iter().find(|it| matches!(it, Io::Connect { .. })).cloned()
    else {
        panic!("{asked:?}")
    };
    assert_eq!(host, "two.test");
    client.complete(Completion::Connected { conn: two });
    assert!(sent(&mut client, two).starts_with(b"GET /b?sig=1 HTTP/1.1\r\nhost: two.test\r\n"));
    received(&mut client, two, b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone");
    let got = events(&mut client);
    let Some(Event::Head { url, .. }) = got.first() else { panic!("{got:?}") };
    assert_eq!(url, "http://two.test/b?sig=1");
    assert_eq!(answer(&got), (200, b"done".to_vec(), true));

    client.request(RequestId(2), Request::get("http://one.test/c")).unwrap();
    let asked = ios(&mut client);
    assert!(
        asked.iter().any(|it| matches!(it, Io::Send { conn, .. } if *conn == one)),
        "{asked:?}"
    );
    assert!(!asked.iter().any(|it| matches!(it, Io::Connect { .. })), "{asked:?}");
}

/// More redirects than the client follows fail the request, naming how many.
#[test]
fn a_request_past_its_redirects_fails() {
    let tls = crate::roots::with_roots(rustls::RootCertStore::empty()).unwrap();
    let mut client =
        Client::new(Config { redirects: 1, ..Config::new(tls, "blaze-test", TIMEOUT) });
    let (one, _) = open(&mut client, 1, Request::get("http://one.test/a"));
    received(&mut client, one, b"HTTP/1.1 301 Moved\r\nLocation: /b\r\nContent-Length: 0\r\n\r\n");
    sent(&mut client, one);
    received(&mut client, one, b"HTTP/1.1 301 Moved\r\nLocation: /c\r\nContent-Length: 0\r\n\r\n");
    let got = events(&mut client);
    assert_eq!(
        *failed(&got),
        Error { url: "http://one.test/b".to_owned(), kind: Kind::Redirects { most: 1 } }
    );
}

/// With HTTPS only a redirect may move within HTTPS but never off it, to an absolute or a
/// scheme-relative `http:` address; without it, any redirect is followed.
#[test]
fn an_https_only_redirect_to_plain_http_is_refused() {
    let from = url::Url::parse("https://github.com/o/r/releases/download/v1/a.tar.gz").unwrap();
    let to = |location: &str, https_only: bool| {
        redirect_to(&from, location, https_only).map(String::from)
    };
    assert_eq!(
        to("https://objects.example/a?sig=1", true),
        Ok("https://objects.example/a?sig=1".to_owned())
    );
    assert_eq!(to("/b", true), Ok("https://github.com/b".to_owned()));
    for location in ["http://objects.example/a", "HTTP://objects.example/a"] {
        assert_eq!(
            to(location, true),
            Err(Kind::Downgraded { to: "http://objects.example/a".to_owned() }),
            "{location}"
        );
    }
    assert_eq!(to("http://objects.example/a", false), Ok("http://objects.example/a".to_owned()));
}

/// No answer within the timeout fails the request at its deadline, naming the host; a body
/// that stops coming fails as stalled; both close their connections.
#[test]
fn a_request_with_no_answer_fails_at_its_deadline() {
    let mut client = client();
    let (conn, _) = open(&mut client, 1, Request::get("http://example.test/a"));
    assert_eq!(client.next_deadline(), Some(TIMEOUT));
    client.advance_clock(TIMEOUT - Duration::from_millis(1));
    assert_eq!(events(&mut client), []);
    client.advance_clock(TIMEOUT);
    let got = events(&mut client);
    let kind = Kind::NoAnswer { after: TIMEOUT, host: "example.test".to_owned(), port: 80 };
    assert_eq!(*failed(&got), Error { url: "http://example.test/a".to_owned(), kind });
    assert!(ios(&mut client).contains(&Io::Close { conn }));

    let (conn, _) = open(&mut client, 2, Request::get("http://example.test/b"));
    received(&mut client, conn, b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc");
    let at = TIMEOUT + Duration::from_secs(5);
    client.advance_clock(at);
    received(&mut client, conn, b"de");
    client.advance_clock(at + TIMEOUT);
    let got = events(&mut client);
    assert!(
        matches!(&failed(&got).kind, Kind::Stalled { after, .. } if *after == TIMEOUT),
        "{got:?}"
    );
    let bodies: Vec<u8> = got
        .iter()
        .filter_map(|it| match it {
            Event::Body { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(bodies, b"abcde");
    assert!(ios(&mut client).contains(&Io::Close { conn }));
}

/// An answer that is not HTTP fails its request with an error naming the step and the host,
/// and is not asked again (bytes of it came).
#[test]
fn a_malformed_answer_fails_naming_the_host_and_step() {
    let mut client = client();
    let (conn, _) = open(&mut client, 1, Request::get("http://example.test/a"));
    received(&mut client, conn, b"HTTP/1.1 2x0 OK\r\n\r\n");
    let got = events(&mut client);
    let error = failed(&got);
    assert!(matches!(&error.kind, Kind::Io { step: Step::Head, .. }), "{error:?}");
    let said = error.to_string();
    assert!(
        said.starts_with("reading the answer's head from example.test:80: not an HTTP/1 answer"),
        "{said}"
    );
    assert!(!said.contains("/a"), "the message names no path: {said}");
    let asked = ios(&mut client);
    assert!(
        asked.contains(&Io::Close { conn })
            && !asked.iter().any(|it| matches!(it, Io::Connect { .. })),
        "{asked:?}"
    );
}

/// A gzip body is decoded when gzip was asked for, held to the bound once decoded, and comes
/// as sent when it was not asked for; a `Content-Length` past the bound is refused before any
/// of the body is read.
#[test]
fn gzip_is_decoded_where_asked_and_bodies_are_held_to_their_bound() {
    let plain = b"hello ".repeat(100);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&plain).unwrap();
    let zipped = encoder.finish().unwrap();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        zipped.len()
    );
    let answered = [head.as_bytes(), &zipped[..]].concat();

    let mut client = client();
    let (conn, _) = open(&mut client, 1, Request::get("http://example.test/a"));
    for piece in answered.chunks(7) {
        received(&mut client, conn, piece);
    }
    assert_eq!(answer(&events(&mut client)), (200, plain.clone(), true));

    let raw = Request { gzip: false, ..Request::get("http://example.test/a") };
    client.request(RequestId(2), raw).unwrap();
    let written = sent(&mut client, conn);
    assert!(!String::from_utf8_lossy(&written).contains("accept-encoding"));
    received(&mut client, conn, &answered);
    assert_eq!(answer(&events(&mut client)), (200, zipped.clone(), true));

    let bounded = Request { most: 100, ..Request::get("http://example.test/a") };
    client.request(RequestId(3), bounded).unwrap();
    sent(&mut client, conn);
    received(&mut client, conn, &answered);
    let got = events(&mut client);
    assert_eq!(failed(&got).kind, Kind::TooBig { most: 100, said: None, decoded: true });

    let mut client = self::client();
    let (conn, _) =
        open(&mut client, 1, Request { most: 3, ..Request::get("http://example.test/a") });
    received(&mut client, conn, b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
    let got = events(&mut client);
    assert_eq!(failed(&got).kind, Kind::TooBig { most: 3, said: Some(4), decoded: false });
}

/// A body read ahead of its asker stops being asked for at `WINDOW` bytes, and its stall's
/// clock stops with it, until the asker takes some.
#[test]
fn a_slow_asker_holds_the_transfer_back() {
    let mut client = client();
    let (conn, _) = open(&mut client, 1, Request::get("http://example.test/big"));
    let piece = vec![b'x'; crate::WINDOW];
    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", 2 * crate::WINDOW);
    received(&mut client, conn, head.as_bytes());
    ios(&mut client);
    received(&mut client, conn, &piece);
    assert!(!ios(&mut client).contains(&Io::Recv { conn }), "asked for more past the window");
    assert_eq!(client.next_deadline(), None);
    client.advance_clock(TIMEOUT * 10);
    assert!(!events(&mut client).iter().any(|it| matches!(it, Event::Failed { .. })));
    client.consumed(RequestId(1), crate::WINDOW);
    assert!(ios(&mut client).contains(&Io::Recv { conn }));
    assert_eq!(client.next_deadline(), Some(TIMEOUT * 11));
    received(&mut client, conn, &piece);
    let got = events(&mut client);
    assert!(matches!(got.last(), Some(Event::Done { .. })), "{got:?}");
}

/// The answer's events, and what the server read, for `url` asked of an in-memory rustls server
/// holding a self-signed certificate for `localhost` that the client trusts as its one root.
fn tls_exchange(url: &str) -> (Vec<Event>, Vec<u8>) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let der = cert.der().clone();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.clone()).unwrap();
    let mut client =
        Client::new(Config::new(crate::roots::with_roots(roots).unwrap(), "blaze-test", TIMEOUT));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![der],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    let mut server = rustls::ServerConnection::new(Arc::new(config)).unwrap();

    client.request(RequestId(1), Request::get(url)).unwrap();
    let Some(Io::Connect { conn, port: 443, .. }) = client.next_io() else {
        panic!("no connect to port 443")
    };
    client.complete(Completion::Connected { conn });
    let mut asked = Vec::new();
    let mut answered = false;
    let mut got = Vec::new();
    for _ in 0..64 {
        for io in ios(&mut client) {
            match io {
                Io::Send { bytes, .. } => {
                    let mut rest = &bytes[..];
                    while !rest.is_empty() {
                        server.read_tls(&mut rest).unwrap();
                        server.process_new_packets().unwrap();
                    }
                    client.complete(Completion::Sent { conn });
                }
                Io::Recv { .. } | Io::Close { .. } => {}
                Io::Connect { .. } => panic!("a second connect"),
            }
        }
        let mut plain = [0u8; 4096];
        match server.reader().read(&mut plain) {
            Ok(n) => asked.extend_from_slice(&plain[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        }
        if !answered && asked.windows(4).any(|it| it == b"\r\n\r\n") {
            server
                .writer()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecret")
                .unwrap();
            answered = true;
        }
        let mut out = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut out).unwrap();
        }
        if !out.is_empty() {
            client.complete(Completion::Received { conn, bytes: out });
        }
        got.extend(events(&mut client));
        if got.iter().any(Event::ends) {
            return (got, asked);
        }
    }
    panic!("no end after 64 rounds: {got:?}");
}

/// TLS through rustls's sans-IO connection: the handshake against an in-memory rustls server,
/// the request sealed and the answer opened; a certificate for another name fails in the TLS
/// step, naming the host.
#[test]
fn tls_is_spoken_through_rustls_sans_io() {
    let (got, asked) = tls_exchange("https://localhost/secret");
    assert!(
        asked.starts_with(b"GET /secret HTTP/1.1\r\nhost: localhost\r\n"),
        "{}",
        String::from_utf8_lossy(&asked)
    );
    assert_eq!(answer(&got), (200, b"secret".to_vec(), true));

    let (got, _) = tls_exchange("https://other.test/secret");
    let error = failed(&got);
    let Kind::Io { step: Step::Tls, host, port: 443, why } = &error.kind else {
        panic!("{error:?}")
    };
    assert_eq!(host, "other.test");
    assert!(why.contains("certificate"), "{why}");
}
