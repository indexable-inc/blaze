//! The core: quinn-proto's endpoint and connections, and on each connection HTTP/3's control
//! streams, the extended CONNECT of WebTransport sessions, their streams and their datagrams.
//! A synchronous state machine: datagrams, asks and the time go in; datagrams, events and the
//! next deadline come out (the driver contract is in [`super`]).

#![blaze::sans_io]

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use quinn_proto::{
    ConnectionError, ConnectionHandle, DatagramEvent, Dir, EndpointConfig, FinishError, ReadError,
    Side, StreamEvent, StreamId, VarInt, WriteError,
};

use super::{
    ClientConfig, Error, ServerConfig, Step, config,
    frame::{self, Settings, code},
    qpack, varint,
};

/// How long after the handshake the peer's SETTINGS may take.
pub const SETTINGS_WAIT: Duration = Duration::from_secs(10);
/// How long after a session closes its connection waits for the close to be acknowledged
/// before it is closed anyway.
pub const LINGER: Duration = Duration::from_secs(1);
/// The most bytes of a WebTransport stream the core reads ahead of its asker.
pub const WINDOW: usize = 256 << 10;
/// The most WebTransport streams one connection holds for sessions not yet open.
pub const MOST_HELD: usize = 16;

/// A connection, numbered by the core; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// A WebTransport session: its connection and the ID of its CONNECT stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId {
    pub conn: ConnId,
    pub stream: u64,
}

/// A stream: its connection and its QUIC stream ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamRef {
    pub conn: ConnId,
    pub stream: u64,
}

impl StreamRef {
    /// Which ways the stream carries bytes.
    pub fn direction(self) -> Direction {
        if self.stream & 0x2 == 0 { Direction::Bi } else { Direction::Uni }
    }
}

/// Which ways a stream carries bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Both ways.
    Bi,
    /// From its opener to its peer only.
    Uni,
}

impl Direction {
    fn quic(self) -> Dir {
        match self {
            Direction::Bi => Dir::Bi,
            Direction::Uni => Dir::Uni,
        }
    }
}

/// A UDP datagram to send.
#[derive(Debug)]
pub struct Transmit {
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

/// A WebTransport CONNECT as the server's asker sees it.
#[derive(Clone, Debug)]
pub struct SessionRequest {
    pub peer: SocketAddr,
    pub authority: String,
    /// `:path`, with the query.
    pub path: String,
    pub origin: Option<String>,
    pub user_agent: Option<String>,
    /// Every field, pseudo-fields too, in the order sent.
    pub headers: Vec<(String, String)>,
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The server answered the CONNECT with this status.
    Refused { status: u16 },
    /// The peer closed it (CLOSE_WEBTRANSPORT_SESSION, or FIN as code 0).
    ByPeer { code: u32, reason: String },
    /// This side closed it.
    Local { code: u32, reason: String },
    /// Its connection or its CONNECT stream failed.
    Failed(Error),
}

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Closed::Refused { status } => {
                write!(f, "the server refused the session with status {status}")
            }
            Closed::ByPeer { code, reason } => {
                write!(f, "the peer closed the session with code {code}: {reason:?}")
            }
            Closed::Local { code, reason } => {
                write!(f, "the session was closed here with code {code}: {reason:?}")
            }
            Closed::Failed(error) => write!(f, "{error}"),
        }
    }
}

/// What the core tells its asker.
#[derive(Debug)]
pub enum Event {
    /// A WebTransport CONNECT: answer it with [`Endpoint::accept`] or [`Endpoint::refuse`].
    SessionRequest { session: SessionId, request: SessionRequest },
    /// The server accepted this client's session.
    SessionOpened { session: SessionId },
    /// The session ended; its streams have had their last event.
    SessionClosed { session: SessionId, closed: Closed },
    /// The peer opened a stream in an open session.
    StreamOpened { session: SessionId, stream: StreamRef },
    /// Bytes of a stream: say them taken with [`Endpoint::consumed`].
    StreamData { stream: StreamRef, bytes: Vec<u8> },
    /// The peer finished its side of the stream.
    StreamFinished { stream: StreamRef },
    /// The peer reset its side, with its WebTransport code; `None` when the code is not one
    /// (or the session went away).
    StreamReset { stream: StreamRef, code: Option<u32> },
    /// The peer stopped this side's sending, with its WebTransport code; `None` as above.
    StreamStopped { stream: StreamRef, code: Option<u32> },
    /// Everything given to [`Endpoint::send`] on the stream went into QUIC's send buffer.
    StreamSent { stream: StreamRef },
    /// The peer allows more streams: an [`Endpoint::open`] that was refused may now work.
    StreamsAvailable { session: SessionId },
    /// A datagram of an open session.
    Datagram { session: SessionId, bytes: Vec<u8> },
    /// A request the core answered itself (not a WebTransport CONNECT, or a refused handshake).
    Refused { peer: SocketAddr, error: Error },
    /// A connection is gone, and with it its sessions; `error` when it failed.
    ConnectionClosed { conn: ConnId, peer: SocketAddr, error: Option<Error> },
}

/// The core.
pub struct Endpoint {
    quic: quinn_proto::Endpoint,
    /// The server's transport limits, kept for a new identity.
    serving: Option<config::Transport>,
    client: Option<quinn_proto::ClientConfig>,
    epoch: Instant,
    now: Duration,
    next_conn: u64,
    conns: BTreeMap<ConnId, Conn>,
    handles: BTreeMap<usize, ConnId>,
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

/// What a connection's handlers write to.
struct Cx<'a> {
    events: &'a mut VecDeque<Event>,
    now: Duration,
    at: Instant,
    id: ConnId,
}

struct Conn {
    quic: quinn_proto::Connection,
    handle: ConnectionHandle,
    peer: SocketAddr,
    side: Side,
    connected: bool,
    peer_control: Option<StreamId>,
    peer_settings: Option<Settings>,
    settings_due: Option<Duration>,
    streams: BTreeMap<StreamId, Stream>,
    sessions: BTreeMap<StreamId, Session>,
    /// The CONNECT this client sends once the server's SETTINGS come.
    connect: Option<Connect>,
    /// When the connection is closed though its last close is not acknowledged.
    linger: Option<Duration>,
    /// The CONNECT stream whose FIN, once acknowledged, closes the connection.
    closing: Option<StreamId>,
    /// The error that closed it, this side's.
    failed: Option<Error>,
    lost: Option<Error>,
}

struct Connect {
    authority: String,
    path: String,
    headers: Vec<(String, String)>,
}

enum State {
    /// A server's CONNECT that came before the client's SETTINGS.
    Held(SessionRequest),
    /// A server's CONNECT handed to the asker.
    Requested {
        draft02: bool,
    },
    /// A client's session whose CONNECT waits for the server's SETTINGS.
    Waiting,
    /// A client's CONNECT sent, its answer not yet come.
    Connecting,
    Open,
}

struct Session {
    state: State,
    /// The CONNECT stream's DATA bytes, read as capsules.
    capsules: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A peer's unidirectional stream whose type is not read yet.
    PeerUni,
    /// A peer's bidirectional stream whose first integer is not read yet.
    PeerBidi,
    /// This side's control stream.
    Control,
    /// The peer's control stream.
    PeerControl,
    /// The peer's QPACK streams: with no dynamic table, nothing on them matters.
    Drain,
    /// A CONNECT stream, this side's or the peer's.
    Request,
    /// A WebTransport stream; `open` once its session is open and the asker told.
    Wt { session: StreamId, open: bool },
}

struct Stream {
    kind: Kind,
    /// Bytes read and not yet parsed, or held for a session not yet open.
    inbuf: Vec<u8>,
    /// Bytes to send, from `out_at` on.
    out: Vec<u8>,
    out_at: usize,
    /// Finish once `out` is sent.
    fin: bool,
    finished: bool,
    /// Bytes handed to the asker not yet taken.
    unconsumed: usize,
    recv_open: bool,
    /// How the peer's side ended, kept until the asker hears it.
    ended: Option<End>,
    /// The asker queued bytes not yet all handed to QUIC: [`Event::StreamSent`] is owed.
    asked: bool,
}

/// How the peer's side of a stream ended.
#[derive(Clone, Copy)]
enum End {
    Fin,
    Reset(u64),
}

impl Stream {
    /// A stream of `kind`, open to receive (one this side only sends on sets `recv_open`).
    fn new(kind: Kind) -> Stream {
        Stream {
            kind,
            inbuf: Vec::new(),
            out: Vec::new(),
            out_at: 0,
            fin: false,
            finished: false,
            unconsumed: 0,
            recv_open: true,
            ended: None,
            asked: false,
        }
    }
}

fn quic_id(stream: u64) -> Option<StreamId> {
    VarInt::from_u64(stream).ok().map(StreamId::from)
}

fn h3_code(code: u64) -> VarInt {
    VarInt::from_u64(code).unwrap_or(VarInt::from_u32(0x102))
}

fn gone(stream: u64, conn: ConnId) -> Error {
    // alloc: an error's message, made only when it fails.
    Error {
        peer: None,
        step: Step::Stream,
        why: format!("stream {stream} of connection {} is not open here", conn.0),
    }
}

impl Endpoint {
    /// An endpoint that serves with `server`, connects with `client`, or both; `epoch` is the
    /// instant the driver's clock counts from.
    pub fn new(
        epoch: Instant,
        server: Option<ServerConfig>,
        client: Option<ClientConfig>,
    ) -> Result<Endpoint, Error> {
        let serving = server.as_ref().map(|it| it.transport);
        let server = server.map(|it| config::server(&it)).transpose()?;
        let client = client.map(|it| config::client(&it)).transpose()?;
        // alloc: one per endpoint.
        let quic =
            quinn_proto::Endpoint::new(Arc::new(EndpointConfig::default()), server, true, None);
        Ok(Endpoint {
            quic,
            serving,
            client,
            epoch,
            now: Duration::ZERO,
            next_conn: 0,
            conns: BTreeMap::new(),
            handles: BTreeMap::new(),
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        })
    }

    fn at(&self) -> Instant {
        self.epoch + self.now
    }

    /// Moves the core's time to `since` the epoch, and does what fell due.
    pub fn advance_clock(&mut self, since: Duration) {
        self.now = self.now.max(since);
        let at = self.at();
        // alloc: the connection ids, once per tick.
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for id in ids {
            let Some(conn) = self.conns.get_mut(&id) else { continue };
            if conn.quic.poll_timeout().is_some_and(|due| due <= at) {
                conn.quic.handle_timeout(at);
            }
            let mut cx = Cx { events: &mut self.events, now: self.now, at, id };
            if conn.settings_due.is_some_and(|due| due <= self.now) {
                conn.settings_due = None;
                // alloc: an error's message, made only when it fails.
                let why =
                    format!("no SETTINGS within {} s of the handshake", SETTINGS_WAIT.as_secs());
                conn.fail(code::H3_MISSING_SETTINGS, Step::Settings, why, &mut cx);
            }
            if conn.linger.is_some_and(|due| due <= self.now) {
                conn.linger = None;
                conn.quic.close(at, h3_code(code::H3_NO_ERROR), Bytes::new());
            }
            self.drive(id);
        }
    }

    /// When the core next needs [`Endpoint::advance_clock`], on the epoch's scale.
    pub fn next_deadline(&mut self) -> Option<Duration> {
        let epoch = self.epoch;
        let mut next: Option<Duration> = None;
        for conn in self.conns.values_mut() {
            let quic = conn.quic.poll_timeout().map(|it| it.saturating_duration_since(epoch));
            for due in [quic, conn.settings_due, conn.linger].into_iter().flatten() {
                next = Some(next.map_or(due, |it| it.min(due)));
            }
        }
        next
    }

    /// The next datagram to send.
    pub fn next_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// The next event for the asker.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Whether every connection is closed (its CONNECTION_CLOSE made), as a driver stopping
    /// after [`Endpoint::close_all`] waits for.
    pub fn all_closed(&self) -> bool {
        self.conns.values().all(|it| it.quic.is_closed())
    }

    /// Takes a UDP datagram from `from`.
    pub fn received(&mut self, from: SocketAddr, bytes: &[u8]) {
        let at = self.at();
        // alloc: one per datagram, quinn-proto's answer to it when it has one.
        let mut buf = Vec::new();
        match self.quic.handle(at, from, None, None, BytesMut::from(bytes), &mut buf) {
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                let Some(&id) = self.handles.get(&handle.0) else { return };
                if let Some(conn) = self.conns.get_mut(&id) {
                    conn.quic.handle_event(event);
                }
                self.drive(id);
            }
            Some(DatagramEvent::NewConnection(incoming)) => {
                if self.serving.is_none() {
                    let transmit = self.quic.refuse(incoming, &mut buf);
                    buf.truncate(transmit.size);
                    self.transmits.push_back(Transmit { to: transmit.destination, bytes: buf });
                    return;
                }
                match self.quic.accept(incoming, at, &mut buf, None) {
                    Ok((handle, quic)) => {
                        let id = self.add(handle, quic, from, Side::Server, None);
                        self.drive(id);
                    }
                    Err(refused) => {
                        if let Some(transmit) = refused.response {
                            buf.truncate(transmit.size);
                            self.transmits
                                .push_back(Transmit { to: transmit.destination, bytes: buf });
                        }
                        // alloc: an error's message, made only when it fails.
                        let error = Error {
                            peer: Some(from),
                            step: Step::Handshake,
                            why: refused.cause.to_string(),
                        };
                        self.events.push_back(Event::Refused { peer: from, error });
                    }
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                buf.truncate(transmit.size);
                self.transmits.push_back(Transmit { to: transmit.destination, bytes: buf });
            }
            None => {}
        }
    }

    fn add(
        &mut self,
        handle: ConnectionHandle,
        quic: quinn_proto::Connection,
        peer: SocketAddr,
        side: Side,
        connect: Option<Connect>,
    ) -> ConnId {
        let id = ConnId(self.next_conn);
        self.next_conn += 1;
        let conn = Conn {
            quic,
            handle,
            peer,
            side,
            connected: false,
            peer_control: None,
            peer_settings: None,
            settings_due: None,
            streams: BTreeMap::new(),
            sessions: BTreeMap::new(),
            connect,
            linger: None,
            closing: None,
            failed: None,
            lost: None,
        };
        self.conns.insert(id, conn);
        self.handles.insert(handle.0, id);
        id
    }

    /// Runs connection `id` until it has nothing more to say: its endpoint events, its
    /// events, its transmits; and forgets it once it is drained.
    fn drive(&mut self, id: ConnId) {
        let at = self.at();
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let mut cx = Cx { events: &mut self.events, now: self.now, at, id };
        loop {
            let mut busy = false;
            while let Some(event) = conn.quic.poll_endpoint_events() {
                busy = true;
                if let Some(back) = self.quic.handle_event(conn.handle, event) {
                    conn.quic.handle_event(back);
                }
            }
            while let Some(event) = conn.quic.poll() {
                busy = true;
                conn.on_quic(event, &mut cx);
            }
            if !busy {
                break;
            }
        }
        loop {
            // alloc: one per datagram sent, handed to the driver.
            let mut buf = Vec::new();
            let Some(transmit) = conn.quic.poll_transmit(at, 1, &mut buf) else { break };
            buf.truncate(transmit.size);
            self.transmits.push_back(Transmit { to: transmit.destination, bytes: buf });
        }
        if conn.quic.is_drained() {
            let peer = conn.peer;
            let error = conn.failed.take().or(conn.lost.take());
            let handle = conn.handle.0;
            self.conns.remove(&id);
            self.handles.remove(&handle);
            self.events.push_back(Event::ConnectionClosed { conn: id, peer, error });
        }
    }

    fn conn(&mut self, id: ConnId) -> Result<&mut Conn, Error> {
        // alloc: an error's message, made only when it fails.
        self.conns.get_mut(&id).ok_or_else(|| Error {
            peer: None,
            step: Step::Session,
            why: format!("connection {} is gone", id.0),
        })
    }

    /// Runs `f` on connection `id` with a context, then drives it.
    fn with<T>(
        &mut self,
        id: ConnId,
        f: impl FnOnce(&mut Conn, &mut Cx<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let (now, at) = (self.now, self.at());
        let conn = self.conns.get_mut(&id);
        // alloc: an error's message, made only when it fails.
        let conn = conn.ok_or_else(|| Error {
            peer: None,
            step: Step::Session,
            why: format!("connection {} is gone", id.0),
        })?;
        let mut cx = Cx { events: &mut self.events, now, at, id };
        let out = f(conn, &mut cx);
        self.drive(id);
        out
    }

    /// Opens a WebTransport session to `to` at `url` (`https://host:port/path`), with `headers`
    /// (lowercase names, no pseudo-fields) on its CONNECT. Its outcome is
    /// [`Event::SessionOpened`] or [`Event::SessionClosed`].
    pub fn connect(
        &mut self,
        to: SocketAddr,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<SessionId, Error> {
        // alloc: an error's message, made only when it fails.
        let fail = |why: String| Error { peer: Some(to), step: Step::Connect, why };
        let Some(client) = self.client.clone() else {
            // alloc: an error's message, made only when it fails.
            return Err(fail("this endpoint was made without a ClientConfig".to_owned()));
        };
        // alloc: an error's message, made only when it fails.
        let parsed =
            url::Url::parse(url).map_err(|why| fail(format!("`{url}` is not a URL: {why}")))?;
        if parsed.scheme() != "https" {
            // alloc: an error's message, made only when it fails.
            return Err(fail(format!("`{url}` is not https, which WebTransport needs")));
        }
        let name = match parsed.host() {
            // alloc: the server's name, once a connection.
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            // alloc: as above.
            Some(host) => host.to_string(),
            // alloc: an error's message, made only when it fails.
            None => return Err(fail(format!("`{url}` has no host"))),
        };
        for (name, _) in headers {
            if name.starts_with(':')
                || name.bytes().any(|it| it.is_ascii_uppercase())
                || name.is_empty()
            {
                // alloc: an error's message, made only when it fails.
                return Err(fail(format!(
                    "the header `{name}` is a pseudo-field or not lowercase"
                )));
            }
        }
        // alloc: the CONNECT's fields, once a session.
        let path = match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_owned(),
        };
        let connect = Connect {
            // alloc: once a session.
            authority: parsed.authority().to_owned(),
            path,
            // alloc: once a session.
            headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
        };
        // alloc: an error's message, made only when it fails.
        let (handle, quic) = self
            .quic
            .connect(self.at(), client, to, &name)
            .map_err(|why| Error { peer: Some(to), step: Step::Handshake, why: why.to_string() })?;
        let id = self.add(handle, quic, to, Side::Client, Some(connect));
        let stream = StreamId::new(Side::Client, Dir::Bi, 0);
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.sessions.insert(stream, Session { state: State::Waiting, capsules: Vec::new() });
        }
        self.drive(id);
        Ok(SessionId { conn: id, stream: u64::from(stream) })
    }

    /// Accepts a session the peer asked for ([`Event::SessionRequest`]): its CONNECT is
    /// answered 200 and its streams start coming.
    pub fn accept(&mut self, session: SessionId) -> Result<(), Error> {
        let sid = quic_id(session.stream).ok_or_else(|| gone(session.stream, session.conn))?;
        self.with(session.conn, |conn, cx| {
            let Some(entry) = conn.sessions.get_mut(&sid) else {
                return Err(gone(session.stream, session.conn));
            };
            let State::Requested { draft02 } = entry.state else {
                // alloc: an error's message, made only when it fails.
                return Err(conn.error(
                    Step::Session,
                    format!("session {} was not asked for, or was answered", session.stream),
                ));
            };
            entry.state = State::Open;
            // alloc: one answer's head, once a session.
            let mut head = Vec::new();
            if draft02 {
                qpack::encode(
                    &[(":status", "200"), ("sec-webtransport-http3-draft", "draft02")],
                    &mut head,
                );
            } else {
                qpack::encode(&[(":status", "200")], &mut head);
            }
            conn.send_frame(sid, frame::HEADERS, &head, cx);
            conn.release(sid, cx);
            Ok(())
        })
    }

    /// Refuses a session the peer asked for with `status` (300 to 599): its CONNECT is
    /// answered and finished, and the streams it held are reset.
    pub fn refuse(&mut self, session: SessionId, status: u16) -> Result<(), Error> {
        let sid = quic_id(session.stream).ok_or_else(|| gone(session.stream, session.conn))?;
        self.with(session.conn, |conn, cx| {
            if !(300..=599).contains(&status) {
                // alloc: an error's message, made only when it fails.
                return Err(conn.error(
                    Step::Session,
                    format!("a refusal's status is 300 to 599, and {status} is not"),
                ));
            }
            match conn.sessions.get(&sid).map(|it| &it.state) {
                Some(State::Requested { .. } | State::Held(_)) => {}
                // alloc: an error's message, made only when it fails.
                _ => {
                    return Err(conn.error(
                        Step::Session,
                        format!("session {} was not asked for, or was answered", session.stream),
                    ));
                }
            }
            conn.sessions.remove(&sid);
            conn.answer(sid, status, cx);
            conn.drop_session_streams(sid, cx);
            Ok(())
        })
    }

    /// Opens a stream in an open session: `Ok(None)` when the peer allows no more streams now
    /// ([`Event::StreamsAvailable`] says when to ask again).
    pub fn open(
        &mut self,
        session: SessionId,
        direction: Direction,
    ) -> Result<Option<StreamRef>, Error> {
        let sid = quic_id(session.stream).ok_or_else(|| gone(session.stream, session.conn))?;
        self.with(session.conn, |conn, cx| {
            if !matches!(conn.sessions.get(&sid).map(|it| &it.state), Some(State::Open)) {
                // alloc: an error's message, made only when it fails.
                return Err(
                    conn.error(Step::Session, format!("session {} is not open", session.stream))
                );
            }
            let Some(id) = conn.quic.streams().open(direction.quic()) else { return Ok(None) };
            let mut stream = Stream::new(Kind::Wt { session: sid, open: true });
            let signal = match direction {
                Direction::Bi => frame::WEBTRANSPORT_STREAM,
                Direction::Uni => {
                    stream.recv_open = false;
                    frame::STREAM_WEBTRANSPORT
                }
            };
            varint::put(&mut stream.out, signal);
            varint::put(&mut stream.out, session.stream);
            conn.streams.insert(id, stream);
            conn.flush(id, cx);
            Ok(Some(StreamRef { conn: cx.id, stream: u64::from(id) }))
        })
    }

    /// Queues `bytes` on a stream this side may send on; [`Event::StreamSent`] says when they
    /// all went to QUIC.
    pub fn send(&mut self, stream: StreamRef, bytes: &[u8]) -> Result<(), Error> {
        let id = quic_id(stream.stream).ok_or_else(|| gone(stream.stream, stream.conn))?;
        self.with(stream.conn, |conn, cx| {
            conn.may_send(id)?;
            let entry = conn.wt_stream(id).ok_or_else(|| gone(stream.stream, stream.conn))?;
            if entry.fin || entry.finished {
                // alloc: an error's message, made only when it fails.
                return Err(
                    conn.error(Step::Stream, format!("stream {} was finished", stream.stream))
                );
            }
            entry.out.extend_from_slice(bytes);
            entry.asked = true;
            conn.flush(id, cx);
            Ok(())
        })
    }

    /// Finishes a stream once what was queued on it is sent.
    pub fn finish(&mut self, stream: StreamRef) -> Result<(), Error> {
        let id = quic_id(stream.stream).ok_or_else(|| gone(stream.stream, stream.conn))?;
        self.with(stream.conn, |conn, cx| {
            conn.may_send(id)?;
            let entry = conn.wt_stream(id).ok_or_else(|| gone(stream.stream, stream.conn))?;
            entry.fin = true;
            conn.flush(id, cx);
            Ok(())
        })
    }

    /// Resets this side of a stream with a WebTransport `code`, dropping what was queued.
    pub fn reset(&mut self, stream: StreamRef, code: u32) -> Result<(), Error> {
        let id = quic_id(stream.stream).ok_or_else(|| gone(stream.stream, stream.conn))?;
        self.with(stream.conn, |conn, _| {
            conn.may_send(id)?;
            let entry = conn.wt_stream(id).ok_or_else(|| gone(stream.stream, stream.conn))?;
            entry.out.clear();
            entry.out_at = 0;
            entry.finished = true;
            // A stream already reset or stopped has nothing left to reset.
            _ = conn.quic.send_stream(id).reset(h3_code(frame::wt_to_h3(code)));
            conn.forget_if_done(id);
            Ok(())
        })
    }

    /// Stops the peer's sending on a stream with a WebTransport `code`.
    pub fn stop(&mut self, stream: StreamRef, code: u32) -> Result<(), Error> {
        let id = quic_id(stream.stream).ok_or_else(|| gone(stream.stream, stream.conn))?;
        self.with(stream.conn, |conn, _| {
            if !conn.receives(id) {
                // alloc: an error's message, made only when it fails.
                return Err(conn.error(
                    Step::Stream,
                    format!(
                        "stream {id} is this side's unidirectional stream: there is nothing to stop"
                    ),
                ));
            }
            let entry = conn.wt_stream(id).ok_or_else(|| gone(stream.stream, stream.conn))?;
            if !entry.recv_open {
                return Ok(());
            }
            entry.recv_open = false;
            entry.inbuf.clear();
            // A stream the peer already finished or reset has nothing left to stop.
            _ = conn.quic.recv_stream(id).stop(h3_code(frame::wt_to_h3(code)));
            conn.forget_if_done(id);
            Ok(())
        })
    }

    /// Says `bytes` of a stream's [`Event::StreamData`] were taken, so more may be read.
    pub fn consumed(&mut self, stream: StreamRef, bytes: usize) {
        let Some(id) = quic_id(stream.stream) else { return };
        _ = self.with(stream.conn, |conn, cx| {
            if let Some(entry) = conn.streams.get_mut(&id) {
                entry.unconsumed = entry.unconsumed.saturating_sub(bytes);
                conn.read(id, cx);
            }
            Ok(())
        });
    }

    /// Sends a datagram in an open session.
    pub fn send_datagram(&mut self, session: SessionId, bytes: &[u8]) -> Result<(), Error> {
        let sid = quic_id(session.stream).ok_or_else(|| gone(session.stream, session.conn))?;
        self.with(session.conn, |conn, _| {
            if !matches!(conn.sessions.get(&sid).map(|it| &it.state), Some(State::Open)) {
                // alloc: an error's message, made only when it fails.
                return Err(
                    conn.error(Step::Datagram, format!("session {} is not open", session.stream))
                );
            }
            if conn.peer_settings.is_some_and(|it| !it.h3_datagram) {
                // alloc: an error's message, made only when it fails.
                return Err(conn.error(
                    Step::Datagram,
                    "the peer's SETTINGS have no SETTINGS_H3_DATAGRAM = 1".to_owned(),
                ));
            }
            // alloc: one datagram, handed to quinn-proto.
            let mut payload = Vec::with_capacity(8 + bytes.len());
            varint::put(&mut payload, session.stream / 4);
            payload.extend_from_slice(bytes);
            conn.quic
                .datagrams()
                .send(Bytes::from(payload), true)
                // alloc: an error's message, made only when it fails.
                .map_err(|why| Error {
                    peer: Some(conn.peer),
                    step: Step::Datagram,
                    why: why.to_string(),
                })
        })
    }

    /// Closes a session with `code` and `reason` (CLOSE_WEBTRANSPORT_SESSION, then FIN); its
    /// connection closes once that is acknowledged, or after [`LINGER`].
    pub fn close(&mut self, session: SessionId, code: u32, reason: &str) -> Result<(), Error> {
        let sid = quic_id(session.stream).ok_or_else(|| gone(session.stream, session.conn))?;
        self.with(session.conn, |conn, cx| {
            conn.close_session(sid, code, reason, cx);
            Ok(())
        })
    }

    /// Closes every session with `code` and `reason`, and then every connection; a driver
    /// that is stopping runs until [`Endpoint::all_closed`].
    pub fn close_all(&mut self, code: u32, reason: &str) {
        // alloc: the connection ids, once as the endpoint stops.
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for id in ids {
            _ = self.with(id, |conn, cx| {
                // alloc: the session ids, once as the endpoint stops.
                let sessions: Vec<StreamId> = conn.sessions.keys().copied().collect();
                for sid in sessions {
                    conn.close_session(sid, code, reason, cx);
                }
                if conn.linger.is_none() {
                    conn.quic.close(
                        cx.at,
                        h3_code(code::H3_NO_ERROR),
                        Bytes::copy_from_slice(reason.as_bytes()),
                    );
                }
                Ok(())
            });
        }
    }

    /// Serves `identity` to connections from now on; open ones keep theirs.
    pub fn set_identity(&mut self, identity: config::Identity) -> Result<(), Error> {
        let Some(transport) = self.serving else {
            // alloc: an error's message, made only when it fails.
            return Err(Error {
                peer: None,
                step: Step::Config,
                why: "this endpoint does not serve".to_owned(),
            });
        };
        let server = config::server(&ServerConfig { identity, transport })?;
        self.quic.set_server_config(Some(server));
        Ok(())
    }

    /// Opens a bidirectional stream on connection `conn` and writes `bytes` on it, with no
    /// session: a test's hand-made request.
    #[cfg(test)]
    pub(crate) fn raw_request(&mut self, conn: ConnId, bytes: &[u8]) {
        _ = self.with(conn, |conn, cx| {
            let id = conn.quic.streams().open(Dir::Bi).expect("the peer allows a stream");
            let mut stream = Stream::new(Kind::Request);
            stream.out.extend_from_slice(bytes);
            conn.streams.insert(id, stream);
            conn.flush(id, cx);
            Ok(())
        });
    }

    /// The peer of connection `conn`, while it is open.
    pub fn peer(&self, conn: ConnId) -> Option<SocketAddr> {
        self.conns.get(&conn).map(|it| it.peer)
    }

    /// The SETTINGS the peer of connection `conn` sent, once they came.
    pub fn peer_settings(&mut self, conn: ConnId) -> Option<Settings> {
        self.conn(conn).ok()?.peer_settings
    }
}

impl Conn {
    fn error(&self, step: Step, why: String) -> Error {
        Error { peer: Some(self.peer), step, why }
    }

    /// Whether this side sends on stream `id`: a bidirectional one, or its own.
    fn sends(&self, id: StreamId) -> bool {
        id.dir() == Dir::Bi || id.initiator() == self.side
    }

    /// Whether this side receives on stream `id`: a bidirectional one, or the peer's.
    fn receives(&self, id: StreamId) -> bool {
        id.dir() == Dir::Bi || id.initiator() != self.side
    }

    fn may_send(&self, id: StreamId) -> Result<(), Error> {
        if self.sends(id) {
            return Ok(());
        }
        // alloc: an error's message, made only when it fails.
        Err(self.error(
            Step::Stream,
            format!("stream {id} is the peer's unidirectional stream: this side cannot send on it"),
        ))
    }

    /// Tells the asker how the peer's side of stream `id` ended, once.
    fn tell_end(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        let at = StreamRef { conn: cx.id, stream: u64::from(id) };
        match stream.ended.take() {
            Some(End::Fin) => cx.events.push_back(Event::StreamFinished { stream: at }),
            Some(End::Reset(code)) => {
                cx.events.push_back(Event::StreamReset { stream: at, code: frame::h3_to_wt(code) })
            }
            None => {}
        }
    }

    /// Stream `id` of session `sid` is the asker's now: it hears of it, its bytes so far, and
    /// its end if that came.
    fn opened(&mut self, sid: StreamId, id: StreamId, cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        stream.kind = Kind::Wt { session: sid, open: true };
        let session = SessionId { conn: cx.id, stream: u64::from(sid) };
        cx.events.push_back(Event::StreamOpened {
            session,
            stream: StreamRef { conn: cx.id, stream: u64::from(id) },
        });
        self.deliver(id, cx);
        self.tell_end(id, cx);
        self.forget_if_done(id);
        self.read(id, cx);
    }

    fn wt_stream(&mut self, id: StreamId) -> Option<&mut Stream> {
        self.streams.get_mut(&id).filter(|it| matches!(it.kind, Kind::Wt { open: true, .. }))
    }

    /// Fails the connection with an HTTP/3 error: it is closed with `code`, and its sessions
    /// end with the error.
    fn fail(&mut self, h3: u64, step: Step, why: String, cx: &mut Cx<'_>) {
        if self.failed.is_some() {
            return;
        }
        let error = self.error(step, why);
        let mut reason = error.why.as_bytes();
        reason = &reason[..reason.len().min(256)];
        self.quic.close(cx.at, h3_code(h3), Bytes::copy_from_slice(reason));
        self.end_sessions(&Closed::Failed(error.clone()), cx);
        self.failed = Some(error);
    }

    fn end_sessions(&mut self, closed: &Closed, cx: &mut Cx<'_>) {
        // alloc: the session ids, once as a connection ends.
        let sessions: Vec<StreamId> = self.sessions.keys().copied().collect();
        for sid in sessions {
            self.end_session(sid, closed.clone(), cx);
        }
    }

    /// Ends session `sid`: its open streams get their last event and are reset, and the asker
    /// hears `closed`.
    fn end_session(&mut self, sid: StreamId, closed: Closed, cx: &mut Cx<'_>) {
        if self.sessions.remove(&sid).is_none() {
            return;
        }
        // Nothing more of the CONNECT stream matters: what the peer still sends on it is not
        // read.
        if let Some(stream) = self.streams.get_mut(&sid)
            && stream.recv_open
        {
            stream.recv_open = false;
            stream.inbuf.clear();
            _ = self.quic.recv_stream(sid).stop(h3_code(code::H3_NO_ERROR));
        }
        self.drop_session_streams(sid, cx);
        let session = SessionId { conn: cx.id, stream: u64::from(sid) };
        cx.events.push_back(Event::SessionClosed { session, closed });
    }

    /// Resets and forgets every stream of session `sid`, telling the asker of each it knew.
    fn drop_session_streams(&mut self, sid: StreamId, cx: &mut Cx<'_>) {
        // alloc: the stream ids, once as a session ends.
        let ids: Vec<StreamId> = self
            .streams
            .iter()
            .filter(|(_, it)| matches!(it.kind, Kind::Wt { session, .. } if session == sid))
            .map(|(id, _)| *id)
            .collect();
        let gone = h3_code(code::WEBTRANSPORT_SESSION_GONE);
        for id in ids {
            let Some(stream) = self.streams.remove(&id) else { continue };
            let told = matches!(stream.kind, Kind::Wt { open: true, .. });
            let (sends, receives) = (self.sends(id), self.receives(id));
            let at = StreamRef { conn: cx.id, stream: u64::from(id) };
            if receives && stream.recv_open {
                _ = self.quic.recv_stream(id).stop(gone);
                if told {
                    cx.events.push_back(Event::StreamReset { stream: at, code: None });
                }
            }
            if sends && !stream.finished {
                _ = self.quic.send_stream(id).reset(gone);
                if told {
                    cx.events.push_back(Event::StreamStopped { stream: at, code: None });
                }
            }
        }
    }

    fn close_session(&mut self, sid: StreamId, code: u32, reason: &str, cx: &mut Cx<'_>) {
        let Some(session) = self.sessions.get(&sid) else { return };
        let open = matches!(session.state, State::Open);
        let closed = Closed::Local {
            code,
            // alloc: the reason, once as a session closes.
            reason: reason.to_owned(),
        };
        if open && self.streams.contains_key(&sid) {
            // alloc: one capsule, as a session closes.
            let mut capsule = Vec::new();
            frame::put_close(&mut capsule, code, reason);
            if let Some(stream) = self.streams.get_mut(&sid) {
                stream.out.extend_from_slice(&capsule);
                stream.fin = true;
            }
            self.flush(sid, cx);
            self.closing = Some(sid);
            self.linger = Some(cx.now + LINGER);
        } else {
            // Not open: no capsule to send, so the connection closes now.
            self.linger = Some(cx.now);
        }
        self.end_session(sid, closed, cx);
    }

    /// Writes a frame on stream `id`.
    fn send_frame(&mut self, id: StreamId, ty: u64, payload: &[u8], cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        frame::put(&mut stream.out, ty, payload);
        self.flush(id, cx);
    }

    /// Answers the CONNECT on `id` with `status` and finishes it, reading no more of it.
    fn answer(&mut self, id: StreamId, status: u16, cx: &mut Cx<'_>) {
        // alloc: the status as text, once a refusal.
        let status = status.to_string();
        // alloc: one answer's head.
        let mut head = Vec::new();
        qpack::encode(&[(":status", &status)], &mut head);
        if let Some(stream) = self.streams.get_mut(&id) {
            frame::put(&mut stream.out, frame::HEADERS, &head);
            stream.fin = true;
            stream.recv_open = false;
            stream.inbuf.clear();
        }
        self.flush(id, cx);
        _ = self.quic.recv_stream(id).stop(h3_code(code::H3_NO_ERROR));
    }

    /// Hands what is queued on stream `id` to QUIC, as far as it takes it.
    fn flush(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        while stream.out_at < stream.out.len() {
            match self.quic.send_stream(id).write(&stream.out[stream.out_at..]) {
                Ok(n) => stream.out_at += n,
                Err(WriteError::Blocked) => return,
                Err(WriteError::Stopped(_) | WriteError::ClosedStream) => {
                    // The peer's STOP_SENDING comes as its own event, which tells the asker.
                    stream.out.clear();
                    stream.out_at = 0;
                    stream.finished = true;
                    return;
                }
            }
        }
        stream.out.clear();
        stream.out_at = 0;
        if std::mem::take(&mut stream.asked) {
            cx.events.push_back(Event::StreamSent {
                stream: StreamRef { conn: cx.id, stream: u64::from(id) },
            });
        }
        if stream.fin && !stream.finished {
            stream.finished = true;
            match self.quic.send_stream(id).finish() {
                Ok(()) | Err(FinishError::Stopped(_) | FinishError::ClosedStream) => {}
            }
        }
        self.forget_if_done(id);
    }

    /// Forgets a WebTransport stream both of whose sides are over.
    fn forget_if_done(&mut self, id: StreamId) {
        let Some(stream) = self.streams.get(&id) else { return };
        if !matches!(stream.kind, Kind::Wt { .. }) {
            return;
        }
        if (!self.sends(id) || stream.finished) && (!self.receives(id) || !stream.recv_open) {
            self.streams.remove(&id);
        }
    }

    fn on_quic(&mut self, event: quinn_proto::Event, cx: &mut Cx<'_>) {
        match event {
            quinn_proto::Event::HandshakeDataReady | quinn_proto::Event::DatagramsUnblocked => {}
            quinn_proto::Event::Connected => self.connected(cx),
            quinn_proto::Event::ConnectionLost { reason } => {
                let clean = match &reason {
                    ConnectionError::LocallyClosed => self.failed.is_none(),
                    ConnectionError::ApplicationClosed(close) => {
                        close.error_code.into_inner() == code::H3_NO_ERROR
                    }
                    _ => false,
                };
                if clean {
                    // The peer closed the connection with no error and no capsule first: its
                    // sessions end as a close with code 0 would end them.
                    self.end_sessions(&Closed::ByPeer { code: 0, reason: String::new() }, cx);
                    return;
                }
                let step = if self.connected { Step::Session } else { Step::Handshake };
                let error = match (&self.failed, &reason) {
                    (Some(error), ConnectionError::LocallyClosed) => error.clone(),
                    // alloc: an error's message, made only when it fails.
                    _ => self.error(step, reason.to_string()),
                };
                self.end_sessions(&Closed::Failed(error.clone()), cx);
                self.lost = Some(error);
            }
            quinn_proto::Event::Stream(StreamEvent::Opened { dir }) => loop {
                let Some(id) = self.quic.streams().accept(dir) else { break };
                let kind = if dir == Dir::Uni { Kind::PeerUni } else { Kind::PeerBidi };
                self.streams.insert(id, Stream::new(kind));
                self.read(id, cx);
            },
            quinn_proto::Event::Stream(StreamEvent::Readable { id }) => self.read(id, cx),
            quinn_proto::Event::Stream(StreamEvent::Writable { id }) => self.flush(id, cx),
            quinn_proto::Event::Stream(StreamEvent::Finished { id }) => {
                if self.closing == Some(id) {
                    self.closing = None;
                    self.linger = Some(cx.now);
                }
            }
            quinn_proto::Event::Stream(StreamEvent::Stopped { id, error_code }) => {
                self.stopped(id, error_code, cx)
            }
            quinn_proto::Event::Stream(StreamEvent::Available { .. }) => {
                for (sid, session) in &self.sessions {
                    if matches!(session.state, State::Open) {
                        let session = SessionId { conn: cx.id, stream: u64::from(*sid) };
                        cx.events.push_back(Event::StreamsAvailable { session });
                    }
                }
            }
            quinn_proto::Event::DatagramReceived => {
                loop {
                    let Some(bytes) = self.quic.datagrams().recv() else { break };
                    let Some((quarter, n)) =
                        varint::get(&bytes).filter(|(it, _)| *it <= varint::MAX / 4)
                    else {
                        // alloc: an error's message, made only when it fails.
                        let why = "a datagram with no quarter stream ID (RFC 9297, 2.1)".to_owned();
                        self.fail(code::H3_DATAGRAM_ERROR, Step::Datagram, why, cx);
                        return;
                    };
                    let Some(sid) = quic_id(quarter * 4) else { continue };
                    // A datagram of a session not open (yet, or any more) is dropped, as
                    // datagrams may be.
                    if matches!(self.sessions.get(&sid).map(|it| &it.state), Some(State::Open)) {
                        let session = SessionId { conn: cx.id, stream: u64::from(sid) };
                        // alloc: one datagram's payload, handed to the asker.
                        cx.events
                            .push_back(Event::Datagram { session, bytes: bytes[n..].to_vec() });
                    }
                }
            }
        }
    }

    /// The handshake is done: this side's control stream and SETTINGS go out.
    fn connected(&mut self, cx: &mut Cx<'_>) {
        self.connected = true;
        let Some(id) = self.quic.streams().open(Dir::Uni) else {
            // alloc: an error's message, made only when it fails.
            let why =
                "the peer allows no unidirectional stream for HTTP/3's control stream".to_owned();
            self.fail(code::H3_STREAM_CREATION_ERROR, Step::Settings, why, cx);
            return;
        };
        let mut control = Stream::new(Kind::Control);
        control.recv_open = false;
        varint::put(&mut control.out, frame::STREAM_CONTROL);
        frame::put_settings(&mut control.out);
        self.streams.insert(id, control);
        self.flush(id, cx);
        if self.peer_settings.is_none() {
            self.settings_due = Some(cx.now + SETTINGS_WAIT);
        }
    }

    fn stopped(&mut self, id: StreamId, code: VarInt, cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        stream.out.clear();
        stream.out_at = 0;
        stream.finished = true;
        match stream.kind {
            Kind::Control => {
                // alloc: an error's message, made only when it fails.
                let why = "the peer stopped this side's control stream".to_owned();
                self.fail(code::H3_CLOSED_CRITICAL_STREAM, Step::Stream, why, cx);
            }
            Kind::Request => {
                // H3_NO_ERROR only says the peer wants no more of this side's request (RFC 9114
                // 4.1): its answer, a close capsule or its FIN still comes on the stream and ends
                // the session, and may already be here, behind this event. Read it first.
                self.read(id, cx);
                if code.into_inner() != code::H3_NO_ERROR && self.sessions.contains_key(&id) {
                    // alloc: an error's message, made only when it fails.
                    let error = self.error(
                        Step::Session,
                        format!("the peer stopped the session's CONNECT stream with code {code}"),
                    );
                    self.end_session(id, Closed::Failed(error), cx);
                }
            }
            Kind::Wt { open: true, .. } => {
                let at = StreamRef { conn: cx.id, stream: u64::from(id) };
                cx.events.push_back(Event::StreamStopped {
                    stream: at,
                    code: frame::h3_to_wt(code.into_inner()),
                });
                self.forget_if_done(id);
            }
            _ => {}
        }
    }

    /// Reads stream `id` as far as quinn-proto has it and its window allows.
    fn read(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        loop {
            let Some(stream) = self.streams.get(&id) else { return };
            if !stream.recv_open {
                return;
            }
            let room = match stream.kind {
                Kind::Wt { open: true, .. } => WINDOW.saturating_sub(stream.unconsumed),
                // Held for a session not yet open: read no further than its header.
                Kind::Wt { open: false, .. } => 0,
                _ => WINDOW,
            };
            if room == 0 {
                return;
            }
            // alloc: one read's bytes, at most WINDOW.
            let mut got = Vec::new();
            let mut end = None;
            {
                let mut recv = self.quic.recv_stream(id);
                let Ok(mut chunks) = recv.read(true) else {
                    if let Some(stream) = self.streams.get_mut(&id) {
                        stream.recv_open = false;
                    }
                    return;
                };
                while got.len() < room {
                    match chunks.next(room - got.len()) {
                        Ok(Some(chunk)) => got.extend_from_slice(&chunk.bytes),
                        Ok(None) => {
                            end = Some(End::Fin);
                            break;
                        }
                        Err(ReadError::Blocked) => break,
                        Err(ReadError::Reset(code)) => {
                            end = Some(End::Reset(code.into_inner()));
                            break;
                        }
                    }
                }
                // Whether a frame must go out for the credit freed: transmits are polled after
                // every call anyway.
                _ = chunks.finalize();
            }
            let more = got.len() >= room && end.is_none();
            self.took(id, &got, end, cx);
            if !more {
                return;
            }
        }
    }

    /// Takes bytes read from stream `id`, and its end when it came.
    fn took(&mut self, id: StreamId, got: &[u8], end: Option<End>, cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        if end.is_some() {
            stream.ended = end;
            stream.recv_open = false;
        }
        stream.inbuf.extend_from_slice(got);
        if let Kind::Wt { open: true, .. } = stream.kind {
            self.deliver(id, cx);
            self.tell_end(id, cx);
            self.forget_if_done(id);
            return;
        }
        // A stream that turns into an open WebTransport stream here is told its end by
        // `opened`; a held one keeps it until its session opens.
        self.parse(id, cx);
        let Some(stream) = self.streams.get(&id) else { return };
        let Some(end) = end else { return };
        match stream.kind {
            Kind::PeerControl | Kind::Drain => {
                // alloc: an error's message, made only when it fails.
                let why =
                    "the peer closed one of its critical streams (control or QPACK)".to_owned();
                self.fail(code::H3_CLOSED_CRITICAL_STREAM, Step::Stream, why, cx);
            }
            Kind::Request => {
                if self.sessions.contains_key(&id) {
                    let closed = match end {
                        // alloc: an error's message, made only when it fails.
                        End::Reset(code) => Closed::Failed(self.error(
                            Step::Session,
                            format!(
                                "the peer reset the session's CONNECT stream with code {code:#x}"
                            ),
                        )),
                        // A FIN with no capsule is a close with code 0 (the draft, 6).
                        End::Fin => Closed::ByPeer { code: 0, reason: String::new() },
                    };
                    self.peer_closed(id, closed, cx);
                } else {
                    self.streams.remove(&id);
                }
            }
            // A stream that ended before its header: nothing to say.
            Kind::PeerUni | Kind::PeerBidi => {
                self.streams.remove(&id);
            }
            Kind::Wt { open: false, .. } | Kind::Control | Kind::Wt { open: true, .. } => {}
        }
    }

    /// The peer closed session `sid`: this side finishes its CONNECT stream and closes the
    /// connection once that is acknowledged.
    fn peer_closed(&mut self, sid: StreamId, closed: Closed, cx: &mut Cx<'_>) {
        if let Some(stream) = self.streams.get_mut(&sid) {
            stream.fin = true;
            stream.recv_open = false;
        }
        self.flush(sid, cx);
        self.closing = Some(sid);
        self.linger = Some(cx.now + LINGER);
        self.end_session(sid, closed, cx);
    }

    /// Hands an open WebTransport stream's buffered bytes to the asker.
    fn deliver(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        let Some(stream) = self.streams.get_mut(&id) else { return };
        if stream.inbuf.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut stream.inbuf);
        stream.unconsumed += bytes.len();
        cx.events.push_back(Event::StreamData {
            stream: StreamRef { conn: cx.id, stream: u64::from(id) },
            bytes,
        });
    }

    /// Parses what stream `id` has buffered, as its kind reads it.
    fn parse(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        loop {
            let Some(stream) = self.streams.get_mut(&id) else { return };
            match stream.kind {
                Kind::PeerUni => {
                    let Some((ty, n)) = varint::get(&stream.inbuf) else { return };
                    match ty {
                        frame::STREAM_CONTROL => {
                            if self.peer_control.is_some() {
                                // alloc: an error's message, made only when it fails.
                                let why = "the peer opened a second control stream".to_owned();
                                self.fail(code::H3_STREAM_CREATION_ERROR, Step::Settings, why, cx);
                                return;
                            }
                            stream.inbuf.drain(..n);
                            stream.kind = Kind::PeerControl;
                            self.peer_control = Some(id);
                        }
                        frame::STREAM_QPACK_ENCODER | frame::STREAM_QPACK_DECODER => {
                            stream.inbuf.clear();
                            stream.kind = Kind::Drain;
                        }
                        frame::STREAM_PUSH => {
                            // No MAX_PUSH_ID is ever sent, so no push ID is allowed (RFC 9114,
                            // 4.6), and a server never takes one.
                            // alloc: an error's message, made only when it fails.
                            let why = format!(
                                "the peer opened push stream {id}, and no push was allowed"
                            );
                            let code = if self.side == Side::Server {
                                code::H3_STREAM_CREATION_ERROR
                            } else {
                                code::H3_ID_ERROR
                            };
                            self.fail(code, Step::Stream, why, cx);
                            return;
                        }
                        frame::STREAM_WEBTRANSPORT => {
                            let Some((session, m)) = varint::get(&stream.inbuf[n..]) else {
                                return;
                            };
                            stream.inbuf.drain(..n + m);
                            self.wt_header(id, session, cx);
                        }
                        // A type this endpoint does not know, GREASE included (RFC 9114, 6.2).
                        _ => {
                            self.streams.remove(&id);
                            _ = self
                                .quic
                                .recv_stream(id)
                                .stop(h3_code(code::H3_STREAM_CREATION_ERROR));
                            return;
                        }
                    }
                }
                Kind::PeerBidi => {
                    let Some((first, n)) = varint::get(&stream.inbuf) else { return };
                    if first == frame::WEBTRANSPORT_STREAM {
                        let Some((session, m)) = varint::get(&stream.inbuf[n..]) else { return };
                        stream.inbuf.drain(..n + m);
                        self.wt_header(id, session, cx);
                    } else if self.side == Side::Server {
                        stream.kind = Kind::Request;
                    } else {
                        // alloc: an error's message, made only when it fails.
                        let why = format!(
                            "the server opened bidirectional stream {id} that is not WebTransport's"
                        );
                        self.fail(code::H3_STREAM_CREATION_ERROR, Step::Stream, why, cx);
                        return;
                    }
                }
                Kind::PeerControl => return self.control_frames(id, cx),
                Kind::Request => return self.request_frames(id, cx),
                Kind::Drain => {
                    stream.inbuf.clear();
                    return;
                }
                Kind::Wt { open: true, .. } => return self.deliver(id, cx),
                Kind::Wt { open: false, .. } | Kind::Control => return,
            }
        }
    }

    /// A WebTransport stream named its session: it is the session's if that is open, held if
    /// the session may yet open, refused past [`MOST_HELD`].
    fn wt_header(&mut self, id: StreamId, session: u64, cx: &mut Cx<'_>) {
        let Some(sid) =
            quic_id(session).filter(|it| it.dir() == Dir::Bi && it.initiator() == Side::Client)
        else {
            // alloc: an error's message, made only when it fails.
            let why = format!(
                "stream {id} names session {session}, which is not a client's bidirectional stream"
            );
            self.streams.remove(&id);
            self.fail(code::H3_ID_ERROR, Step::Stream, why, cx);
            return;
        };
        let open = matches!(self.sessions.get(&sid).map(|it| &it.state), Some(State::Open));
        if !open {
            let held = self
                .streams
                .values()
                .filter(|it| matches!(it.kind, Kind::Wt { open: false, .. }))
                .count();
            if held >= MOST_HELD {
                self.streams.remove(&id);
                let refused = h3_code(code::WEBTRANSPORT_BUFFERED_STREAM_REJECTED);
                _ = self.quic.recv_stream(id).stop(refused);
                if id.dir() == Dir::Bi {
                    _ = self.quic.send_stream(id).reset(refused);
                }
                return;
            }
        }
        if open {
            self.opened(sid, id, cx);
        } else if let Some(stream) = self.streams.get_mut(&id) {
            // Held: a stream may outrun its session's answer (the draft, 4.6), and one whose
            // session never opens stays held until its connection ends, at most MOST_HELD.
            stream.kind = Kind::Wt { session: sid, open: false };
        }
    }

    /// Session `sid` opened: the streams held for it are its now.
    fn release(&mut self, sid: StreamId, cx: &mut Cx<'_>) {
        // alloc: the held stream ids, once a session opens.
        let held: Vec<StreamId> = self
            .streams
            .iter()
            .filter(|(_, it)| it.kind == Kind::Wt { session: sid, open: false })
            .map(|(id, _)| *id)
            .collect();
        for id in held {
            self.opened(sid, id, cx);
        }
    }

    fn control_frames(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        loop {
            let Some(stream) = self.streams.get_mut(&id) else { return };
            let (ty, len, settings) = match frame::next(&stream.inbuf) {
                Ok(None) => return,
                Ok(Some(found)) => {
                    let settings =
                        (found.ty == frame::SETTINGS).then(|| frame::parse_settings(found.payload));
                    (found.ty, found.len, settings)
                }
                Err(error) => return self.fail(error.code, Step::Settings, error.why, cx),
            };
            stream.inbuf.drain(..len);
            match (self.peer_settings.is_some(), ty, settings) {
                (false, _, Some(Ok(settings))) => {
                    self.peer_settings = Some(settings);
                    self.settings_due = None;
                    self.settings_came(cx);
                }
                (_, _, Some(Err(error))) => {
                    return self.fail(error.code, Step::Settings, error.why, cx);
                }
                (false, ty, None) => {
                    // alloc: an error's message, made only when it fails.
                    let why = format!("the control stream's first frame is {ty:#x}, not SETTINGS");
                    return self.fail(code::H3_MISSING_SETTINGS, Step::Settings, why, cx);
                }
                (true, frame::SETTINGS | frame::DATA | frame::HEADERS | frame::PUSH_PROMISE, _) => {
                    // alloc: an error's message, made only when it fails.
                    let why = format!("frame {ty:#x} on the control stream after SETTINGS");
                    return self.fail(code::H3_FRAME_UNEXPECTED, Step::Settings, why, cx);
                }
                // GOAWAY, MAX_PUSH_ID, CANCEL_PUSH and unknown frames: nothing this endpoint
                // acts on.
                (true, _, _) => {}
            }
        }
    }

    /// The peer's SETTINGS came: a server hands over the CONNECTs that waited for them, and a
    /// client sends its CONNECT if the server takes WebTransport.
    fn settings_came(&mut self, cx: &mut Cx<'_>) {
        // alloc: the held session ids, once a connection.
        let held: Vec<StreamId> = self
            .sessions
            .iter()
            .filter(|(_, it)| matches!(it.state, State::Held(_)))
            .map(|(id, _)| *id)
            .collect();
        for sid in held {
            let Some(session) = self.sessions.get_mut(&sid) else { continue };
            let State::Held(request) =
                std::mem::replace(&mut session.state, State::Requested { draft02: false })
            else {
                continue;
            };
            let draft02 = draft02(&request.headers);
            session.state = State::Requested { draft02 };
            cx.events.push_back(Event::SessionRequest {
                session: SessionId { conn: cx.id, stream: u64::from(sid) },
                request,
            });
        }
        let Some(connect) = self.connect.take() else { return };
        let sid = StreamId::new(Side::Client, Dir::Bi, 0);
        let settings = self.peer_settings.unwrap_or_default();
        if let Err(missing) = settings.serves_webtransport() {
            // alloc: an error's message, made only when it fails.
            let error = self
                .error(Step::Settings, format!("the server does not take WebTransport: {missing}"));
            self.linger = Some(cx.now);
            self.end_session(sid, Closed::Failed(error), cx);
            return;
        }
        match self.quic.streams().open(Dir::Bi) {
            Some(id) if id == sid => {}
            _ => {
                // alloc: an error's message, made only when it fails.
                let error = self.error(
                    Step::Connect,
                    "the CONNECT's stream could not be opened as the connection's first".to_owned(),
                );
                self.linger = Some(cx.now);
                self.end_session(sid, Closed::Failed(error), cx);
                return;
            }
        }
        // alloc: the CONNECT's fields, once a session.
        let mut fields: Vec<(&str, &str)> = vec![
            (":method", "CONNECT"),
            (":scheme", "https"),
            (":authority", &connect.authority),
            (":path", &connect.path),
            (":protocol", "webtransport"),
        ];
        fields.extend(connect.headers.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        // alloc: the CONNECT's head, once a session.
        let mut head = Vec::new();
        qpack::encode(&fields, &mut head);
        self.streams.insert(sid, Stream::new(Kind::Request));
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.state = State::Connecting;
        }
        self.send_frame(sid, frame::HEADERS, &head, cx);
    }

    fn request_frames(&mut self, id: StreamId, cx: &mut Cx<'_>) {
        loop {
            let Some(stream) = self.streams.get_mut(&id) else { return };
            if !stream.recv_open && stream.inbuf.is_empty() {
                return;
            }
            let (ty, len, payload) = match frame::next(&stream.inbuf) {
                Ok(None) => return,
                // alloc: one frame's payload, at most MOST_FRAME.
                Ok(Some(found)) => (found.ty, found.len, found.payload.to_vec()),
                Err(error) => return self.fail(error.code, Step::Connect, error.why, cx),
            };
            stream.inbuf.drain(..len);
            let state = self.sessions.get(&id).map(|it| match it.state {
                State::Connecting => 1,
                State::Open => 2,
                _ => 0,
            });
            if state.is_none() && self.side == Side::Client {
                // An answer for no session of this client's (one that ended, or a test's raw
                // request): skipped.
                continue;
            }
            match (state, ty) {
                (None, frame::HEADERS) => self.connect_came(id, &payload, cx),
                (Some(1), frame::HEADERS) => self.answer_came(id, &payload, cx),
                (Some(2), frame::DATA) => self.capsules(id, &payload, cx),
                (None | Some(1), frame::DATA)
                | (_, frame::SETTINGS | frame::GOAWAY | frame::MAX_PUSH_ID | frame::CANCEL_PUSH) => {
                    // alloc: an error's message, made only when it fails.
                    let why = format!("frame {ty:#x} on a CONNECT stream where it may not be");
                    return self.fail(code::H3_FRAME_UNEXPECTED, Step::Connect, why, cx);
                }
                // Trailers, a held CONNECT's later frames, unknown and GREASE frames: skipped.
                _ => {}
            }
        }
    }

    /// A CONNECT's field section came on stream `id`.
    fn connect_came(&mut self, id: StreamId, block: &[u8], cx: &mut Cx<'_>) {
        let fields = match qpack::decode(block) {
            Ok(it) => it,
            Err(why) => return self.fail(code::QPACK_DECOMPRESSION_FAILED, Step::Connect, why, cx),
        };
        let field = |name: &str| fields.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        let refused = match (
            field(":method"),
            field(":protocol"),
            field(":scheme"),
            field(":authority"),
            field(":path"),
        ) {
            (Some("CONNECT"), Some("webtransport"), Some("https"), Some(_), Some(_)) => None,
            (Some("CONNECT"), Some(protocol), ..) if protocol != "webtransport" => {
                // alloc: an error's message, made only when it fails.
                Some((
                    501,
                    format!(
                        "a CONNECT for `{protocol}`, which this endpoint does not speak (only webtransport)"
                    ),
                ))
            }
            // alloc: an error's message, made only when it fails.
            (method, protocol, scheme, authority, path) => Some((
                400,
                format!(
                    "a request that is not a WebTransport CONNECT: :method {method:?}, :protocol {protocol:?}, :scheme {scheme:?}, :authority {authority:?}, :path {path:?}"
                ),
            )),
        };
        if let Some((status, why)) = refused {
            let error = self.error(Step::Connect, why);
            self.answer(id, status, cx);
            self.streams.remove(&id);
            cx.events.push_back(Event::Refused { peer: self.peer, error });
            return;
        }
        let request = SessionRequest {
            peer: self.peer,
            // alloc: once a session.
            authority: field(":authority").unwrap_or_default().to_owned(),
            // alloc: once a session.
            path: field(":path").unwrap_or_default().to_owned(),
            origin: field("origin").map(str::to_owned),
            user_agent: field("user-agent").map(str::to_owned),
            headers: fields.clone(),
        };
        let session = SessionId { conn: cx.id, stream: u64::from(id) };
        let state = if self.peer_settings.is_some() {
            cx.events.push_back(Event::SessionRequest { session, request: request.clone() });
            State::Requested { draft02: draft02(&fields) }
        } else {
            State::Held(request)
        };
        self.sessions.insert(id, Session { state, capsules: Vec::new() });
    }

    /// The answer to this client's CONNECT came.
    fn answer_came(&mut self, id: StreamId, block: &[u8], cx: &mut Cx<'_>) {
        let fields = match qpack::decode(block) {
            Ok(it) => it,
            Err(why) => return self.fail(code::QPACK_DECOMPRESSION_FAILED, Step::Connect, why, cx),
        };
        let status =
            fields.iter().find(|(n, _)| n == ":status").and_then(|(_, v)| v.parse::<u16>().ok());
        match status {
            Some(100..=199) => {}
            Some(200..=299) => {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.state = State::Open;
                }
                cx.events.push_back(Event::SessionOpened {
                    session: SessionId { conn: cx.id, stream: u64::from(id) },
                });
                self.release(id, cx);
            }
            Some(status) => {
                self.linger = Some(cx.now);
                self.end_session(id, Closed::Refused { status }, cx);
            }
            None => {
                // alloc: an error's message, made only when it fails.
                let why = "the answer to the CONNECT has no :status".to_owned();
                self.fail(code::H3_MESSAGE_ERROR, Step::Connect, why, cx);
            }
        }
    }

    /// DATA on an open session's CONNECT stream: its capsules.
    fn capsules(&mut self, id: StreamId, payload: &[u8], cx: &mut Cx<'_>) {
        let Some(session) = self.sessions.get_mut(&id) else { return };
        session.capsules.extend_from_slice(payload);
        loop {
            let Some(session) = self.sessions.get_mut(&id) else { return };
            let Some((ty, body, len)) = frame::next_capsule(&session.capsules) else {
                if session.capsules.len() as u64 > frame::MOST_FRAME {
                    // alloc: an error's message, made only when it fails.
                    let why = format!(
                        "a capsule past {} bytes on the session's CONNECT stream",
                        frame::MOST_FRAME
                    );
                    self.fail(code::H3_EXCESSIVE_LOAD, Step::Session, why, cx);
                }
                return;
            };
            if ty == frame::DRAIN_WEBTRANSPORT_SESSION {
                // The peer will close soon; nothing here starts new work on its own, so
                // there is nothing to wind down.
                session.capsules.drain(..len);
                continue;
            }
            let close = (ty == frame::CLOSE_WEBTRANSPORT_SESSION).then(|| {
                let code =
                    body.get(..4).map_or(0, |it| u32::from_be_bytes([it[0], it[1], it[2], it[3]]));
                let reason =
                    String::from_utf8_lossy(body.get(4..).unwrap_or_default()).into_owned();
                (code, reason, body.len() >= 4)
            });
            session.capsules.drain(..len);
            match close {
                Some((code, reason, true)) => {
                    return self.peer_closed(id, Closed::ByPeer { code, reason }, cx);
                }
                Some((_, _, false)) => {
                    // alloc: an error's message, made only when it fails.
                    let why = "a CLOSE_WEBTRANSPORT_SESSION capsule shorter than its 4-byte code"
                        .to_owned();
                    return self.fail(code::H3_MESSAGE_ERROR, Step::Session, why, cx);
                }
                // Unknown capsules: skipped (RFC 9297, 3.2).
                None => {}
            }
        }
    }
}

/// Whether a CONNECT is draft-02's, whose answer must name the draft.
fn draft02(fields: &[(String, String)]) -> bool {
    fields.iter().any(|(n, v)| n == "sec-webtransport-http3-draft02" && v == "1")
}
