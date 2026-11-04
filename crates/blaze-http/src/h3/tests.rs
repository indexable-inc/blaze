//! The core against itself, in memory: a server [`Endpoint`] and a client one (or a bare
//! quinn-proto client, for a peer that never speaks HTTP/3) handing each other their datagrams
//! directly, on one fake clock that moves only when the test moves it. No socket, no thread,
//! no sleep.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::BytesMut;
use quinn_proto::{DatagramEvent, EndpointConfig};

use super::{
    ClientConfig, Closed, Direction, Endpoint, Event, Identity, SETTINGS_WAIT, ServerConfig,
    SessionId, Step, StreamRef, Transport, config, frame,
};

const SERVER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4433);
const CLIENT: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 50_000);
const URL: &str = "https://localhost:4433/explore/v1?x=1";
/// The most steps of the clock one wait takes before the test fails as stuck.
const MOST_STEPS: usize = 10_000;

/// A P-256 certificate valid from a minute ago for 13 days (rustls checks it against the wall
/// clock), and its SHA-256.
fn identity() -> (Identity, [u8; 32]) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let epoch = rcgen::date_time_ymd(1970, 1, 1);
    params.not_before = epoch + (now - Duration::from_secs(60));
    params.not_after = epoch + (now + Duration::from_secs(13 * 86_400));
    let cert = params.self_signed(&key).unwrap();
    let der = cert.der().to_vec();
    let hash = blaze_sha256::digest(&der);
    (Identity { cert_der: der, key_der: key.serialize_der() }, hash)
}

/// The server's endpoint, and what a client pins to take its certificate.
fn server(epoch: Instant) -> (Endpoint, [u8; 32]) {
    let (identity, hash) = identity();
    let server = Endpoint::new(
        epoch,
        Some(ServerConfig { identity, transport: Transport::default() }),
        None,
    )
    .unwrap();
    (server, hash)
}

/// Either end of the pair: the core, or a bare quinn-proto client that never speaks HTTP/3.
trait Peer {
    fn advance(&mut self, now: Duration);
    fn deadline(&mut self) -> Option<Duration>;
    fn transmit(&mut self) -> Option<Vec<u8>>;
    fn take(&mut self, from: SocketAddr, bytes: &[u8]);
}

impl Peer for Endpoint {
    fn advance(&mut self, now: Duration) {
        self.advance_clock(now);
    }

    fn deadline(&mut self) -> Option<Duration> {
        self.next_deadline()
    }

    fn transmit(&mut self) -> Option<Vec<u8>> {
        self.next_transmit().map(|it| it.bytes)
    }

    fn take(&mut self, from: SocketAddr, bytes: &[u8]) {
        self.received(from, bytes);
    }
}

/// A quinn-proto client with ALPN `h3` and the pin, which finishes the handshake and then says
/// nothing: no control stream, no SETTINGS.
struct Mute {
    epoch: Instant,
    now: Duration,
    endpoint: quinn_proto::Endpoint,
    conn: quinn_proto::Connection,
    handle: quinn_proto::ConnectionHandle,
    lost: Option<quinn_proto::ConnectionError>,
}

impl Mute {
    fn new(epoch: Instant, pin: [u8; 32]) -> Mute {
        let client =
            config::client(&ClientConfig { pins: vec![pin], transport: Transport::default() })
                .unwrap();
        let mut endpoint =
            quinn_proto::Endpoint::new(Arc::new(EndpointConfig::default()), None, true, None);
        let (handle, conn) = endpoint.connect(epoch, client, SERVER, "localhost").unwrap();
        Mute { epoch, now: Duration::ZERO, endpoint, conn, handle, lost: None }
    }

    fn poll(&mut self) {
        while let Some(event) = self.conn.poll() {
            if let quinn_proto::Event::ConnectionLost { reason } = event {
                self.lost = Some(reason);
            }
        }
    }
}

impl Peer for Mute {
    fn advance(&mut self, now: Duration) {
        self.now = now;
        let at = self.epoch + now;
        if self.conn.poll_timeout().is_some_and(|due| due <= at) {
            self.conn.handle_timeout(at);
        }
        self.poll();
    }

    fn deadline(&mut self) -> Option<Duration> {
        self.conn.poll_timeout().map(|it| it.saturating_duration_since(self.epoch))
    }

    fn transmit(&mut self) -> Option<Vec<u8>> {
        let mut buf = Vec::new();
        let transmit = self.conn.poll_transmit(self.epoch + self.now, 1, &mut buf)?;
        buf.truncate(transmit.size);
        Some(buf)
    }

    fn take(&mut self, from: SocketAddr, bytes: &[u8]) {
        let mut buf = Vec::new();
        let at = self.epoch + self.now;
        if let Some(DatagramEvent::ConnectionEvent(_, event)) =
            self.endpoint.handle(at, from, None, None, BytesMut::from(bytes), &mut buf)
        {
            self.conn.handle_event(event);
        }
        while let Some(event) = self.conn.poll_endpoint_events() {
            if let Some(back) = self.endpoint.handle_event(self.handle, event) {
                self.conn.handle_event(back);
            }
        }
        self.poll();
    }
}

/// A server and a client on one fake clock, and the events each said.
struct Pair<C: Peer> {
    server: Endpoint,
    client: C,
    now: Duration,
    server_events: Vec<Event>,
    client_events: Vec<Event>,
}

impl<C: Peer> Pair<C> {
    /// Moves datagrams both ways until neither side has one to send.
    fn pump(&mut self) {
        for _ in 0..MOST_STEPS {
            let mut moved = false;
            while let Some(transmit) = self.server.next_transmit() {
                assert_eq!(transmit.to, CLIENT, "the server sends only to its one client");
                self.client.take(SERVER, &transmit.bytes);
                moved = true;
            }
            while let Some(bytes) = self.client.transmit() {
                self.server.received(CLIENT, &bytes);
                moved = true;
            }
            self.drain();
            if !moved {
                return;
            }
        }
        panic!("the pair still sends after {MOST_STEPS} rounds at {:?}", self.now);
    }

    fn drain(&mut self) {
        self.server_events.extend(std::iter::from_fn(|| self.server.next_event()));
    }

    fn set(&mut self, now: Duration) {
        self.now = now;
        self.server.advance_clock(now);
        self.client.advance(now);
        self.pump();
    }

    /// Moves the clock to `until` through every deadline before it, pumping at each.
    fn run_until(&mut self, until: Duration) {
        self.pump();
        for _ in 0..MOST_STEPS {
            let next =
                [self.server.next_deadline(), self.client.deadline()].into_iter().flatten().min();
            match next {
                Some(at) if at <= until => self.set(at.max(self.now)),
                _ => return self.set(until),
            }
        }
        panic!("the clock took {MOST_STEPS} steps before {until:?}");
    }
}

impl Pair<Endpoint> {
    fn new() -> Pair<Endpoint> {
        let epoch = Instant::now();
        let (server, pin) = server(epoch);
        let client = Endpoint::new(
            epoch,
            None,
            Some(ClientConfig { pins: vec![pin], transport: Transport::default() }),
        )
        .unwrap();
        Pair {
            server,
            client,
            now: Duration::ZERO,
            server_events: Vec::new(),
            client_events: Vec::new(),
        }
    }

    fn settle(&mut self) {
        self.pump();
        self.client_events.extend(std::iter::from_fn(|| self.client.next_event()));
    }

    /// A session the server accepted: its ID on each side.
    fn open(&mut self) -> (SessionId, SessionId) {
        let client = self.client.connect(SERVER, URL, &[("origin", "https://ix.dev")]).unwrap();
        self.settle();
        let server = self.server_events.iter().find_map(|it| match it {
            Event::SessionRequest { session, .. } => Some(*session),
            _ => None,
        });
        let server =
            server.unwrap_or_else(|| panic!("no SessionRequest: {:?}", self.server_events));
        self.server.accept(server).unwrap();
        self.settle();
        assert!(
            self.client_events
                .iter()
                .any(|it| matches!(it, Event::SessionOpened { session } if *session == client)),
            "{:?}",
            self.client_events
        );
        (server, client)
    }
}

fn data_of(events: &[Event], stream: StreamRef) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        if let Event::StreamData { stream: at, bytes } = event
            && *at == stream
        {
            out.extend_from_slice(bytes);
        }
    }
    out
}

fn finished(events: &[Event], stream: StreamRef) -> bool {
    events.iter().any(|it| matches!(it, Event::StreamFinished { stream: at } if *at == stream))
}

fn opened(events: &[Event], session: SessionId, direction: Direction) -> Option<StreamRef> {
    events.iter().find_map(|it| match it {
        Event::StreamOpened { session: at, stream }
            if *at == session && stream.direction() == direction =>
        {
            Some(*stream)
        }
        _ => None,
    })
}

#[test]
fn settings_are_exchanged_and_the_connect_is_accepted() {
    let mut pair = Pair::new();
    let client = pair
        .client
        .connect(SERVER, URL, &[("origin", "https://ix.dev"), ("user-agent", "test")])
        .unwrap();
    pair.settle();
    let request = pair.server_events.iter().find_map(|it| match it {
        Event::SessionRequest { request, .. } => Some(request.clone()),
        _ => None,
    });
    let request = request.unwrap_or_else(|| panic!("{:?}", pair.server_events));
    assert_eq!(request.path, "/explore/v1?x=1");
    assert_eq!(request.authority, "localhost:4433");
    assert_eq!(request.origin.as_deref(), Some("https://ix.dev"));
    assert_eq!(request.user_agent.as_deref(), Some("test"));
    assert_eq!(request.peer, CLIENT);
    assert!(request.headers.iter().any(|(n, v)| n == ":protocol" && v == "webtransport"));

    // Each side read the other's SETTINGS: the ones wtransport sent, which Chrome takes.
    for (side, conn) in [("server", 0), ("client", 0)] {
        let endpoint = if side == "server" { &mut pair.server } else { &mut pair.client };
        let settings = endpoint
            .peer_settings(super::ConnId(conn))
            .unwrap_or_else(|| panic!("{side} has no SETTINGS"));
        assert_eq!(settings.serves_webtransport(), Ok(()), "{side}");
        assert_eq!(settings.webtransport_max_sessions, 1, "{side}");
        assert_eq!(settings.qpack_max_table_capacity, 0, "{side}");
    }

    let session = pair.server_events.iter().find_map(|it| match it {
        Event::SessionRequest { session, .. } => Some(*session),
        _ => None,
    });
    pair.server.accept(session.unwrap()).unwrap();
    pair.settle();
    assert!(
        pair.client_events
            .iter()
            .any(|it| matches!(it, Event::SessionOpened { session } if *session == client))
    );
    assert_eq!(
        session.unwrap().stream,
        0,
        "the session is the connection's first bidirectional stream"
    );
}

#[test]
fn a_bidirectional_stream_is_echoed() {
    let mut pair = Pair::new();
    let (server, client) = pair.open();
    let stream =
        pair.client.open(client, Direction::Bi).unwrap().expect("the server allows streams");
    pair.client.send(stream, b"ping").unwrap();
    pair.client.finish(stream).unwrap();
    pair.settle();
    assert!(
        pair.client_events
            .iter()
            .any(|it| matches!(it, Event::StreamSent { stream: at } if *at == stream))
    );

    let at = opened(&pair.server_events, server, Direction::Bi)
        .unwrap_or_else(|| panic!("{:?}", pair.server_events));
    assert_eq!(at.stream, stream.stream, "one stream, seen from each side");
    assert_eq!(data_of(&pair.server_events, at), b"ping");
    assert!(finished(&pair.server_events, at));
    pair.server.consumed(at, 4);

    pair.server.send(at, b"ping").unwrap();
    pair.server.finish(at).unwrap();
    pair.settle();
    assert_eq!(data_of(&pair.client_events, stream), b"ping");
    assert!(finished(&pair.client_events, stream));
}

#[test]
fn a_unidirectional_stream_is_echoed_on_one_the_server_opens() {
    let mut pair = Pair::new();
    let (server, client) = pair.open();
    let stream =
        pair.client.open(client, Direction::Uni).unwrap().expect("the server allows streams");
    pair.client.send(stream, b"one way").unwrap();
    pair.client.finish(stream).unwrap();
    pair.settle();
    let at = opened(&pair.server_events, server, Direction::Uni)
        .unwrap_or_else(|| panic!("{:?}", pair.server_events));
    assert_eq!(data_of(&pair.server_events, at), b"one way");
    assert!(finished(&pair.server_events, at));
    assert!(
        pair.server.send(at, b"x").is_err_and(|it| it.step == Step::Stream),
        "the peer's unidirectional stream takes no bytes"
    );

    let back =
        pair.server.open(server, Direction::Uni).unwrap().expect("the client allows streams");
    pair.server.send(back, b"one way").unwrap();
    pair.server.finish(back).unwrap();
    pair.settle();
    let seen = opened(&pair.client_events, client, Direction::Uni)
        .unwrap_or_else(|| panic!("{:?}", pair.client_events));
    assert_eq!(data_of(&pair.client_events, seen), b"one way");
    assert!(finished(&pair.client_events, seen));
}

#[test]
fn a_datagram_crosses_with_its_session() {
    let mut pair = Pair::new();
    let (server, client) = pair.open();
    pair.client.send_datagram(client, b"dgram").unwrap();
    pair.settle();
    let got = pair.server_events.iter().find_map(|it| match it {
        Event::Datagram { session, bytes } if *session == server => Some(bytes.clone()),
        _ => None,
    });
    assert_eq!(got.as_deref(), Some(&b"dgram"[..]), "{:?}", pair.server_events);
}

#[test]
fn a_refused_session_ends_with_its_status() {
    let mut pair = Pair::new();
    let client = pair.client.connect(SERVER, "https://localhost:4433/elsewhere", &[]).unwrap();
    pair.settle();
    let session = pair.server_events.iter().find_map(|it| match it {
        Event::SessionRequest { session, .. } => Some(*session),
        _ => None,
    });
    pair.server.refuse(session.unwrap(), 404).unwrap();
    pair.settle();
    let closed = pair.client_events.iter().find_map(|it| match it {
        Event::SessionClosed { session, closed } if *session == client => Some(closed.clone()),
        _ => None,
    });
    assert_eq!(closed, Some(Closed::Refused { status: 404 }), "{:?}", pair.client_events);
    assert_eq!(closed.unwrap().to_string(), "the server refused the session with status 404");
    assert!(pair.server.refuse(session.unwrap(), 404).is_err(), "a session is answered once");
}

#[test]
fn a_certificate_not_pinned_fails_the_handshake_naming_the_server() {
    let epoch = Instant::now();
    let (server, _) = server(epoch);
    let client = Endpoint::new(
        epoch,
        None,
        Some(ClientConfig { pins: vec![[7; 32]], transport: Transport::default() }),
    )
    .unwrap();
    let mut pair = Pair {
        server,
        client,
        now: Duration::ZERO,
        server_events: Vec::new(),
        client_events: Vec::new(),
    };
    let session = pair.client.connect(SERVER, URL, &[]).unwrap();
    pair.settle();
    let closed = pair.client_events.iter().find_map(|it| match it {
        Event::SessionClosed { session: at, closed: Closed::Failed(error) } if *at == session => {
            Some(error.clone())
        }
        _ => None,
    });
    let error = closed.unwrap_or_else(|| panic!("{:?}", pair.client_events));
    assert_eq!((error.peer, error.step), (Some(SERVER), Step::Handshake), "{error}");
    assert!(error.to_string().starts_with("the QUIC handshake with 127.0.0.1:4433: "), "{error}");
}

#[test]
fn a_closed_session_tells_the_peer_its_code_and_reason() {
    let mut pair = Pair::new();
    let (server, client) = pair.open();
    pair.server.close(server, 7, "bye").unwrap();
    pair.settle();
    let closed = |events: &[Event], session: SessionId| {
        events.iter().find_map(|it| match it {
            Event::SessionClosed { session: at, closed } if *at == session => Some(closed.clone()),
            _ => None,
        })
    };
    assert_eq!(
        closed(&pair.client_events, client),
        Some(Closed::ByPeer { code: 7, reason: "bye".to_owned() })
    );
    assert_eq!(
        closed(&pair.server_events, server),
        Some(Closed::Local { code: 7, reason: "bye".to_owned() })
    );
    assert!(pair.client.open(client, Direction::Bi).is_err(), "a closed session opens no stream");

    // Then the connection closes, cleanly, and is forgotten by both.
    pair.run_until(pair.now + Duration::from_secs(5));
    pair.client_events.extend(std::iter::from_fn(|| pair.client.next_event()));
    for (side, events) in [("server", &pair.server_events), ("client", &pair.client_events)] {
        assert!(
            events.iter().any(|it| matches!(it, Event::ConnectionClosed { error: None, .. })),
            "{side}: {events:?}"
        );
    }
}

#[test]
fn a_peer_with_no_settings_fails_at_exactly_the_settings_wait() {
    let epoch = Instant::now();
    let (server, pin) = server(epoch);
    let client = Mute::new(epoch, pin);
    let mut pair = Pair {
        server,
        client,
        now: Duration::ZERO,
        server_events: Vec::new(),
        client_events: Vec::new(),
    };
    pair.pump();
    assert!(!pair.client.conn.is_handshaking(), "the handshake is done at {:?}", pair.now);
    let connected = pair.now;

    pair.run_until(connected + SETTINGS_WAIT - Duration::from_millis(1));
    assert!(pair.client.lost.is_none(), "closed before the wait ended: {:?}", pair.client.lost);

    pair.run_until(connected + SETTINGS_WAIT);
    match &pair.client.lost {
        Some(quinn_proto::ConnectionError::ApplicationClosed(close)) => {
            assert_eq!(close.error_code.into_inner(), frame::code::H3_MISSING_SETTINGS);
            let reason = String::from_utf8_lossy(&close.reason);
            assert!(reason.contains("no SETTINGS within 10 s"), "{reason}");
        }
        other => panic!("the server did not close for missing SETTINGS: {other:?}"),
    }

    pair.run_until(connected + SETTINGS_WAIT + Duration::from_secs(5));
    let error = pair.server_events.iter().find_map(|it| match it {
        Event::ConnectionClosed { error: Some(error), .. } => Some(error.clone()),
        _ => None,
    });
    let error = error.unwrap_or_else(|| panic!("{:?}", pair.server_events));
    assert_eq!((error.peer, error.step), (Some(CLIENT), Step::Settings), "{error}");
}

#[test]
fn a_request_that_is_not_webtransport_is_answered_and_reported() {
    let mut pair = Pair::new();
    pair.open();
    // A second CONNECT on the same connection, for another protocol, written by hand.
    let conn = super::ConnId(0);
    let mut head = Vec::new();
    super::qpack::encode(
        &[
            (":method", "CONNECT"),
            (":protocol", "websocket"),
            (":scheme", "https"),
            (":authority", "x"),
            (":path", "/"),
        ],
        &mut head,
    );
    let mut bytes = Vec::new();
    frame::put(&mut bytes, frame::HEADERS, &head);
    pair.client.raw_request(conn, &bytes);
    pair.settle();
    let refused = pair.server_events.iter().find_map(|it| match it {
        Event::Refused { error, .. } => Some(error.clone()),
        _ => None,
    });
    let error = refused.unwrap_or_else(|| panic!("{:?}", pair.server_events));
    assert_eq!(error.step, Step::Connect);
    assert!(error.why.contains("websocket"), "{error}");
}
