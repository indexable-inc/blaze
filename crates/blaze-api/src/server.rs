//! The daemon's listener (docs/v2/plan/lanes/ci.md; #277), a sans-IO state machine over [`blaze_core`]'s
//! sockets: it listens, cuts requests from what peers send, hands the session what
//! [`Arrived`], and sends each [`Reply`] where it goes.
//!
//! - **Unix socket**: frames both ways; many requests per connection, replies in any order,
//!   each naming its request's id.
//! - **HTTP** (remote clients, ix forks): `POST /api` with `Content-Type: application/cbor` and
//!   one unframed CBOR item of an [`Envelope`] of a [`Request`], carrying `Authorization:
//!   Bearer` with the token. The reply is 200, chunked: each response one frame in a chunk
//!   (`application/cbor`), or with `Accept: application/json` one line of [`json::response`]
//!   (`application/x-ndjson`), the body ending after [`Response::Done`]. One request at a time
//!   per connection, kept alive: a pipelined next request waits for the first's end.
//!
//! What a peer sends that is no request is answered here and never reaches the session: an
//! item that does not decode, or of another schema, gets [`Response::Error`] and then
//! [`Response::Done`] with `ok` false under its id (0 when unreadable), and the connection
//! carries on; a frame too long, or bytes that are no HTTP, are answered and then hung up on.
//!
//! One receive at a time per connection, of 64 KiB, or on the Unix socket up to what the frame
//! under way still lacks, at most as much as has come of it; and one send, later replies
//! queueing behind it. A peer that lets more than 64 MiB queue (it stopped reading) is hung up
//! on.
//!
//! The Unix socket is made at its name ([`Unix::Bind`]), or is the one systemd or launchd
//! passed a daemon they socket-activated ([`Unix::Passed`], [`crate::activation`]): the driver
//! listens on a copy of it once it is a listening Unix socket bound at the name, and fails the
//! listen naming it otherwise. Nothing is made in its place.

use std::{fmt, net::SocketAddr};

use blaze_core::{Address, Completion, Conn, Done, Effect, FileName, Tag};
use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_protocol::cbor;

use crate::{
    ApiError, Arrived, Envelope, LONGEST_REQUEST, Reply, Request, RequestRef, Response,
    frames::{self, Frames, Refused},
    http::{self, HttpRequestRef, RequestParser},
    json,
};

/// The most bytes one receive asks for.
const RECEIVE: usize = 64 << 10;
/// The most bytes a connection's replies may queue behind the send in flight.
const MOST_QUEUED: usize = 64 << 20;
/// The most bytes an HTTP connection may hold, pipelined behind the request being answered,
/// before the server stops reading it until that request is done.
const MOST_HELD: usize = LONGEST_REQUEST as usize + (1 << 20);
/// How many tags the server uses, from [`Config::tags`].
const TAGS: u64 = 1 << 32;
/// The most bytes [`Server::line`] keeps between chunks; a longer chunk's buffer is dropped.
const KEPT_LINE: usize = 1 << 20;
/// The room a connection's replies are first written into after a send took its queue: most
/// bursts of replies fit, so they cost one allocation.
const QUEUE: usize = 4 << 10;

pub struct Http {
    pub at: SocketAddr,
    /// Bearer token every HTTP request must carry; never logged or echoed.
    pub token: String,
}

impl fmt::Debug for Http {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http").field("at", &self.at).field("token", &"(hidden)").finish()
    }
}

/// Where the Unix socket is listened on.
#[derive(Debug)]
pub enum Unix {
    /// A socket made at the name, mode 0600.
    Bind(FileName),
    /// The socket a service manager passed, which must be bound at `at`. It is kept open
    /// while the server lives.
    #[cfg(unix)]
    Passed { socket: crate::activation::Passed, at: FileName },
}

#[derive(Debug)]
pub struct Config {
    pub unix: Option<Unix>,
    pub http: Option<Http>,
    /// The tags it uses are `tags..tags + 2^32`.
    pub tags: u64,
    pub most_connections: usize,
}

pub struct Server {
    tags: u64,
    next_tag: u64,
    token: Option<String>,
    most_connections: usize,
    listeners: Vec<Listener>,
    pending: HashMap<Tag, Pending>,
    conns: HashMap<Conn, Connection>,
    failed: Option<String>,
    stopped: bool,
    /// What arrived, handed over at the end of the next [`Server::complete`].
    told: Vec<Arrived>,
    /// Every reply is encoded with it, onto its connection's queue.
    writer: cbor::Writer,
    /// An HTTP chunk's content, put together before its size is known.
    line: Vec<u8>,
    /// What [`Unix::Passed`] passed, open until the server is dropped.
    #[cfg(unix)]
    passed: Option<crate::activation::Passed>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("connections", &self.conns.len())
            .field("failed", &self.failed)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Unix,
    Http,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Listening {
    Yes,
    /// An unlisten is asked; its end is coming.
    Ending,
    Ended,
}

struct Listener {
    side: Side,
    at: Address,
    tag: Tag,
    listening: Listening,
}

/// What a tag of the server's was asked for.
enum Pending {
    Listen(usize),
    Recv(Conn),
    Send(Conn),
    Hangup,
}

struct Connection {
    reader: Reader,
    receiving: Option<Tag>,
    sending: Option<Tag>,
    /// Bytes waiting for the send in flight; empty while none is in flight, so a reply is
    /// encoded where it is sent from.
    queue: Vec<u8>,
    /// Hang up once everything queued is sent; nothing more is read.
    closing: bool,
}

enum Reader {
    Unix(Frames),
    Http { parser: RequestParser, answering: Option<Answering> },
}

/// The HTTP request whose responses a connection is sending.
#[derive(Clone, Copy, Debug)]
struct Answering {
    id: u64,
    json: bool,
    /// The client asked for the connection to close after it.
    close: bool,
}

impl Server {
    /// Asks to listen on each address.
    pub fn new(config: Config, effects: &mut Vec<Effect>) -> Server {
        // alloc: once per server, at start; each grows to its peak and is kept.
        let (listeners, told, line) = (Vec::with_capacity(2), Vec::new(), Vec::new());
        let mut server = Server {
            tags: config.tags,
            next_tag: 0,
            token: None,
            most_connections: config.most_connections,
            listeners,
            pending: HashMap::default(),
            conns: HashMap::default(),
            failed: None,
            stopped: false,
            told,
            writer: cbor::Writer::new(),
            line,
            #[cfg(unix)]
            passed: None,
        };
        if config.unix.is_none() && config.http.is_none() {
            // alloc: error path, at start.
            server.fail("the API server was given no address to listen on".to_owned());
        }
        match config.unix {
            Some(Unix::Bind(name)) => server.listen(Side::Unix, Address::Unix(name), effects),
            #[cfg(unix)]
            Some(Unix::Passed { socket, at }) => {
                server.listen(Side::Unix, Address::Inherited { fd: socket.fd(), at }, effects);
                server.passed = Some(socket);
            }
            None => {}
        }
        if let Some(Http { at, token }) = config.http {
            match token.is_empty() {
                // alloc: error path, at start.
                true => server.fail(format!("the API's HTTP listener at {at} has an empty token")),
                false => {
                    server.token = Some(token);
                    server.listen(Side::Http, Address::Tcp(at), effects);
                }
            }
        }
        server
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.tags) < TAGS
    }

    /// Why a listener failed, naming its address: the daemon must stop or say so, as a
    /// listener that failed is never listened on again.
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Takes a completion of its own; what peers asked goes in `arrived`.
    pub fn complete(
        &mut self,
        completion: Completion,
        effects: &mut Vec<Effect>,
        arrived: &mut Vec<Arrived>,
    ) {
        let Completion { tag, done } = completion;
        match self.pending.remove(&tag) {
            Some(Pending::Listen(at)) => self.listened(at, tag, done, effects),
            Some(Pending::Recv(conn)) => self.received(conn, tag, done, effects),
            Some(Pending::Send(conn)) => self.sent(conn, tag, done, effects),
            Some(Pending::Hangup) | None => {}
        }
        arrived.append(&mut self.told);
    }

    /// Whether `conn` has bytes not yet sent, queued or in flight: a stream that sends on a
    /// clock (the live feed) skips a tick then, so it never queues more than one tick's worth.
    /// A connection that is gone has nothing waiting.
    pub fn waiting(&self, conn: Conn) -> bool {
        self.conns.get(&conn).is_some_and(|it| it.sending.is_some() || !it.queue.is_empty())
    }

    /// Sends a reply; one to a connection that is gone is dropped.
    pub fn reply(&mut self, reply: Reply, effects: &mut Vec<Effect>) {
        let RequestRef { conn, id } = reply.to;
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        let Connection { reader, queue, closing, .. } = connection;
        if *closing {
            return;
        }
        match reader {
            Reader::Unix(_) => frame(&mut self.writer, room(queue), id, &reply.response),
            Reader::Http { answering, .. } => {
                let Some(now) = *answering else { return };
                if now.id != id {
                    return;
                }
                let queue = room(queue);
                rendered(&mut self.writer, &mut self.line, queue, id, &reply.response, now.json);
                if matches!(reply.response, Response::Done { .. }) {
                    queue.extend_from_slice(http::LAST_CHUNK);
                    *answering = None;
                    *closing = now.close;
                }
            }
        }
        self.flush(conn, effects);
    }

    /// Stops listening, its connections kept: a stopping daemon's first step, so its socket's
    /// name is free (and its file gone) before it writes its image, which a client that comes
    /// meanwhile need not wait on.
    pub fn unlisten(&mut self, effects: &mut Vec<Effect>) {
        for listener in &mut self.listeners {
            if listener.listening == Listening::Yes {
                listener.listening = Listening::Ending;
                effects.push(Effect::Unlisten { tag: listener.tag });
            }
        }
    }

    /// Unlistens, if it still listens, and hangs up every connection.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        self.stopped = true;
        self.unlisten(effects);
        // alloc: once, as the server stops.
        let mut conns: Vec<Conn> = self.conns.keys().copied().collect();
        radix::sort_copy_by_key(&mut conns, |it| it.0);
        for conn in conns {
            self.gone(conn, effects);
        }
    }

    fn tag(&mut self) -> Tag {
        let tag = Tag(self.tags.wrapping_add(self.next_tag % TAGS));
        self.next_tag += 1;
        tag
    }

    fn fail(&mut self, what: String) {
        match &mut self.failed {
            Some(failed) => {
                failed.push_str("; ");
                failed.push_str(&what);
            }
            None => self.failed = Some(what),
        }
    }

    fn listen(&mut self, side: Side, at: Address, effects: &mut Vec<Effect>) {
        let tag = self.tag();
        self.pending.insert(tag, Pending::Listen(self.listeners.len()));
        // clone: once per listener, at start; the listener keeps its address to name it.
        effects.push(Effect::Listen { at: at.clone(), tag });
        self.listeners.push(Listener { side, at, tag, listening: Listening::Yes });
    }

    fn listened(&mut self, at: usize, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        match done {
            Done::Accepted { conn } => {
                self.pending.insert(tag, Pending::Listen(at));
                self.accept(self.listeners[at].side, conn, effects);
            }
            Done::Unlistened => self.listeners[at].listening = Listening::Ended,
            Done::Failed(error) => {
                self.listeners[at].listening = Listening::Ended;
                let address = shown(&self.listeners[at].at);
                // alloc: error path, a listener that failed.
                let what = format!("listening on {address}: {error}");
                self.fail(what);
            }
            other => {
                self.listeners[at].listening = Listening::Ended;
                let address = shown(&self.listeners[at].at);
                // alloc: error path, a listener that failed.
                let what = format!("listening on {address}: the driver answered {other:?}");
                self.fail(what);
            }
        }
    }

    fn accept(&mut self, side: Side, conn: Conn, effects: &mut Vec<Effect>) {
        if self.stopped || self.conns.len() >= self.most_connections {
            self.hang_up(conn, effects);
            return;
        }
        let reader = match side {
            Side::Unix => Reader::Unix(Frames::new()),
            Side::Http => Reader::Http {
                parser: RequestParser::new(LONGEST_REQUEST as usize),
                answering: None,
            },
        };
        // alloc: none until a reply is queued; once per connection.
        let queue = Vec::new();
        let connection = Connection { reader, receiving: None, sending: None, queue, closing: false };
        self.conns.insert(conn, connection);
        self.receive(conn, effects);
    }

    /// Asks for the connection's next bytes, unless a receive is in flight or it should not
    /// read now.
    fn receive(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get(&conn) else { return };
        let (most, held) = match &connection.reader {
            Reader::Unix(frames) => (frames.wanted(RECEIVE), false),
            Reader::Http { parser, answering } => {
                (RECEIVE, answering.is_some() && parser.held() > MOST_HELD)
            }
        };
        if connection.receiving.is_some() || connection.closing || held {
            return;
        }
        let tag = self.tag();
        self.pending.insert(tag, Pending::Recv(conn));
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        let into = match &mut connection.reader {
            // The frames' own buffer, given back by the read: a warm receive allocates nothing.
            Reader::Unix(frames) => frames.lend(),
            // The parser is read while a receive is in flight (a pipelined request, once a send
            // ends), so it lends the buffer its last `push_vec` left free, not the one it holds.
            Reader::Http { parser, .. } => parser.lend_spare(),
        };
        effects.push(Effect::Recv { conn, most, into, tag });
        connection.receiving = Some(tag);
    }

    fn received(&mut self, conn: Conn, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        if connection.receiving != Some(tag) {
            return;
        }
        connection.receiving = None;
        let Done::Read(bytes) = done else { return self.gone(conn, effects) };
        let closing = connection.closing;
        match &mut connection.reader {
            Reader::Unix(frames) => {
                if !frames.give_back(bytes) {
                    return self.gone(conn, effects);
                }
                if closing {
                    return;
                }
                self.unix_items(conn, effects);
            }
            Reader::Http { parser, .. } => {
                if bytes.is_empty() {
                    return self.gone(conn, effects);
                }
                if closing {
                    return;
                }
                parser.push_vec(bytes);
                self.http_requests(conn, effects);
            }
        }
        self.receive(conn, effects);
    }

    fn sent(&mut self, conn: Conn, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        if connection.sending != Some(tag) {
            return;
        }
        connection.sending = None;
        let Done::Sent(spare) = done else {
            return self.gone(conn, effects);
        };
        // The buffer sent is the queue's next, its room kept.
        if !connection.queue.is_empty() {
            let bytes = std::mem::replace(&mut connection.queue, spare);
            return self.send(conn, bytes, effects);
        }
        if connection.queue.capacity() == 0 {
            connection.queue = spare;
        }
        if connection.closing {
            return self.gone(conn, effects);
        }
        self.http_requests(conn, effects);
        self.receive(conn, effects);
    }

    /// The whole items a Unix connection has sent, handed over or answered.
    fn unix_items(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        loop {
            let Some(Connection { reader: Reader::Unix(frames), closing: false, .. }) =
                self.conns.get_mut(&conn)
            else {
                return;
            };
            let decoded = match frames.next_ref() {
                Ok(None) => return,
                Ok(Some(item)) => Ok(frames::decode::<Request>(item)),
                Err(error) => Err(error),
            };
            match decoded {
                Ok(Ok(envelope)) => self.told.push(Arrived::Request {
                    from: RequestRef { conn, id: envelope.id },
                    request: envelope.body,
                }),
                Ok(Err(refused)) => self.refuse(conn, refused, effects),
                Err(error) => {
                    self.refuse(conn, Refused { id: None, error }, effects);
                    self.close(conn);
                    return;
                }
            }
        }
    }

    /// Answers an item that is no request of this schema, as its request would end.
    fn refuse(&mut self, conn: Conn, refused: Refused, effects: &mut Vec<Effect>) {
        let id = refused.id.unwrap_or(0);
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        for response in [Response::Error(refused.error), Response::Done { ok: false }] {
            frame(&mut self.writer, &mut connection.queue, id, &response);
        }
        self.flush(conn, effects);
    }

    /// The whole requests an HTTP connection has sent, while none is being answered. Each is
    /// read where it lies in the connection's parser, taken out of the connection meanwhile.
    fn http_requests(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        loop {
            let Some(Connection {
                reader: Reader::Http { parser, answering: None },
                closing: false,
                ..
            }) = self.conns.get_mut(&conn)
            else {
                return;
            };
            let mut parser = std::mem::replace(parser, RequestParser::new(0));
            let more = match parser.next_ref() {
                Ok(Some(request)) => {
                    self.http_request(conn, request, effects);
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    let close = Some(("Connection", "close"));
                    self.queue_plain(conn, error.status, close, &error.what, effects);
                    self.close(conn);
                    false
                }
            };
            if let Some(Connection { reader: Reader::Http { parser: kept, .. }, .. }) =
                self.conns.get_mut(&conn)
            {
                *kept = parser;
            }
            if !more {
                return;
            }
        }
    }

    fn http_request(&mut self, conn: Conn, request: HttpRequestRef<'_>, effects: &mut Vec<Effect>) {
        let close = request.header("Connection").is_some_and(|it| {
            it.split(',').any(|option| option.trim().eq_ignore_ascii_case("close"))
        });
        let authorized = self.token.as_deref().is_some_and(|token| authorized(&request, token));
        let missing: String;
        let refusal = if !authorized {
            let challenge = Some(("WWW-Authenticate", "Bearer"));
            Some((401, challenge, "a missing or wrong Authorization: Bearer token"))
        } else if request.path != "/api" {
            // alloc: error path, a request for another path.
            missing = format!("no such path as {}; the API is POST /api", request.path);
            Some((404, None, missing.as_str()))
        } else if request.method != "POST" {
            Some((405, Some(("Allow", "POST")), "the API is POST /api"))
        } else if !media_is(request.header("Content-Type"), &["application/cbor"]) {
            Some((415, None, "a body that is not application/cbor"))
        } else {
            None
        };
        if let Some((status, extra, what)) = refusal {
            self.queue_plain(conn, status, extra, what, effects);
            if close {
                self.close(conn);
            }
            return;
        }
        let json = request
            .header("Accept")
            .is_some_and(|accept| media_is(Some(accept), &["application/json", "application/x-ndjson"]));
        let kind = if json { "application/x-ndjson" } else { "application/cbor" };
        let Some(Connection { reader: Reader::Http { answering, .. }, queue, .. }) =
            self.conns.get_mut(&conn)
        else {
            return;
        };
        let queue = room(queue);
        http::chunked_into(queue, 200, &[("Content-Type", kind)]);
        match frames::decode::<Request>(request.body) {
            Ok(envelope) => {
                *answering = Some(Answering { id: envelope.id, json, close });
                self.told.push(Arrived::Request {
                    from: RequestRef { conn, id: envelope.id },
                    request: envelope.body,
                });
                self.flush(conn, effects);
            }
            Err(refused) => {
                let id = refused.id.unwrap_or(0);
                for response in [Response::Error(refused.error), Response::Done { ok: false }] {
                    rendered(&mut self.writer, &mut self.line, queue, id, &response, json);
                }
                queue.extend_from_slice(http::LAST_CHUNK);
                self.flush(conn, effects);
                if close {
                    self.close(conn);
                }
            }
        }
    }

    /// A whole plain-text response of `status` saying `what`, for a request that reaches no
    /// session, written onto the connection's queue and sent now or after the send in flight.
    fn queue_plain(
        &mut self,
        conn: Conn,
        status: u16,
        extra: Option<(&str, &str)>,
        what: &str,
        effects: &mut Vec<Effect>,
    ) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        plain_into(&mut connection.queue, status, extra, what);
        self.flush(conn, effects);
    }

    /// Sends what is queued now when no send is in flight; else it waits for that send, and a
    /// peer with too much waiting is hung up on.
    fn flush(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        let waiting = connection.queue.len();
        match connection.sending {
            None if waiting > 0 => {
                let bytes = std::mem::take(&mut connection.queue);
                self.send(conn, bytes, effects);
            }
            Some(_) if waiting > MOST_QUEUED => self.gone(conn, effects),
            _ => {}
        }
    }

    fn send(&mut self, conn: Conn, bytes: Vec<u8>, effects: &mut Vec<Effect>) {
        let tag = self.tag();
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        connection.sending = Some(tag);
        self.pending.insert(tag, Pending::Send(conn));
        effects.push(Effect::Send { conn, bytes, tag });
    }

    /// The connection is hung up on once what is queued for it is sent.
    fn close(&mut self, conn: Conn) {
        if let Some(connection) = self.conns.get_mut(&conn) {
            connection.closing = true;
        }
    }

    /// The connection is over: the session hears so, and it is hung up on.
    fn gone(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        if self.conns.remove(&conn).is_some() {
            self.told.push(Arrived::Gone { conn });
            self.hang_up(conn, effects);
        }
    }

    fn hang_up(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let tag = self.tag();
        self.pending.insert(tag, Pending::Hangup);
        effects.push(Effect::Hangup { conn, tag });
    }
}

/// A connection's queue, with room for a burst of replies when a send took it.
fn room(queue: &mut Vec<u8>) -> &mut Vec<u8> {
    if queue.capacity() == 0 {
        // alloc: one buffer for the replies queued until the next send, which takes it; the
        // send's end hands it back to the queue (`Server::sent`), so only the first allocates.
        queue.reserve(QUEUE);
    }
    queue
}

/// `response` to request `id` as one frame onto the end of `out`. One too long to frame is
/// sent as the error that says so.
fn frame(writer: &mut cbor::Writer, out: &mut Vec<u8>, id: u64, response: &Response) {
    let Err(error) = frames::encode_into(writer, out, &Envelope::new(id, response)) else {
        return;
    };
    // alloc: error path, a response with no frame.
    let what = format!("a response that could not be sent: {}", error.what);
    let error = Response::Error(ApiError::new(what, error.at));
    frames::encode_into(writer, out, &Envelope::new(id, &error))
        .expect("an error's envelope has a frame");
}

/// `response` to request `id` as one chunk of an HTTP reply onto the end of `out`, its
/// content put together in `line` first.
fn rendered(
    writer: &mut cbor::Writer,
    line: &mut Vec<u8>,
    out: &mut Vec<u8>,
    id: u64,
    response: &Response,
    json: bool,
) {
    line.clear();
    match json {
        true => {
            json::line_into(line, &Envelope::new(id, response));
            line.push(b'\n');
        }
        false => frame(writer, line, id, response),
    }
    http::chunk_into(out, line);
    if line.capacity() > KEPT_LINE {
        drop(std::mem::take(line));
    }
}

/// A whole response of `status` saying `what` onto the end of `out`: its type, then `extra`.
fn plain_into(out: &mut Vec<u8>, status: u16, extra: Option<(&str, &str)>, what: &str) {
    let kind = ("Content-Type", "text/plain; charset=utf-8");
    let both = [kind, extra.unwrap_or(kind)];
    let headers = if extra.is_some() { &both[..] } else { &both[..1] };
    http::response_into(out, status, headers, what.as_bytes());
}

fn authorized(request: &HttpRequestRef<'_>, token: &str) -> bool {
    let Some(value) = request.header("Authorization") else { return false };
    let Some((scheme, credentials)) = value.split_once(' ') else { return false };
    scheme.eq_ignore_ascii_case("bearer") && same(credentials.trim().as_bytes(), token.as_bytes())
}

/// Whether `given` is `secret`, in a time that depends only on `given`'s length.
fn same(given: &[u8], secret: &[u8]) -> bool {
    let mut differ = u8::from(given.len() != secret.len());
    for (at, &byte) in given.iter().enumerate() {
        differ |= byte ^ secret.get(at).copied().unwrap_or(0);
    }
    std::hint::black_box(differ) == 0
}

/// Whether a Content-Type or Accept header names one of `kinds`, parameters aside.
fn media_is(header: Option<&str>, kinds: &[&str]) -> bool {
    header.is_some_and(|header| {
        header.split(',').any(|range| {
            let kind = range.split(';').next().unwrap_or_default().trim();
            kinds.iter().any(|it| kind.eq_ignore_ascii_case(it))
        })
    })
}

/// How an address is named in an error, written where it goes.
fn shown(at: &Address) -> Shown<'_> {
    Shown(at)
}

struct Shown<'a>(&'a Address);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Address::Unix(name) => write!(f, "the socket {name}"),
            Address::Inherited { fd, at } => write!(f, "the socket {at} passed as fd {fd}"),
            Address::Tcp(at) => write!(f, "{at}"),
        }
    }
}

#[cfg(test)]
mod tests;
