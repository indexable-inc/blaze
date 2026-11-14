//! Plain HTTP/3 requests on the endpoint: the asks ([`Endpoint::connect_requests`],
//! [`Endpoint::request`], [`Endpoint::request_early`], [`Endpoint::cancel`],
//! [`Endpoint::consumed_response`], [`Endpoint::respond`], [`Endpoint::go_away`],
//! [`Endpoint::add_ticket`]) and what each connection does for them. The vocabulary and the
//! rules are [`super::super::request`]'s.

#![blaze::sans_io]

use std::{alloc::Global, collections::BTreeMap, net::SocketAddr, time::Duration};

use quinn_proto::{ConnectionError, Dir, Side, StreamId, TransportErrorCode};

use super::{
    super::{
        Error, Step, Ticket,
        frame::{self, code},
        qpack,
        request::{
            DATA_FRAME, Early, Length, Request, RequestEnd, RequestId, ResponseHead, Served,
            USER_AGENT,
        },
        varint,
    },
    Conn, Cx, End, Endpoint, Event, Kind, Stream, h3_code,
};

/// The TLS alert `no_application_protocol` (RFC 8446, 6.2) as QUIC carries it: the server
/// does not speak ALPN `h3`.
const NO_APPLICATION_PROTOCOL: u8 = 0x78;

/// A connection's requests, either side's.
pub(super) struct Requests {
    /// Whether requests that are not a WebTransport CONNECT are the asker's (a server's).
    pub(super) serves: bool,
    /// The next request's number.
    next: u64,
    /// A client's requests not yet ended, by number.
    gets: BTreeMap<u64, Get>,
    /// A server's requests not yet answered, by number: their streams.
    served: BTreeMap<u64, StreamId>,
    /// The stream ID the server's GOAWAY carried: requests on it and past it were not started.
    goaway: Option<u64>,
    /// The stream ID this server's GOAWAY carried: requests from it on are refused.
    goaway_sent: Option<u64>,
    /// The first client bidirectional stream ID this server has not seen opened.
    unseen: u64,
    #[cfg(test)]
    pub(super) plant: Plant,
    /// Each request's HEADERS as it was put on a stream, and whether in early data.
    #[cfg(test)]
    pub(super) sent: Vec<(u64, bool), Global>,
}

impl Requests {
    pub(super) fn new(serves: bool) -> Requests {
        Requests {
            serves,
            next: 0,
            gets: BTreeMap::new(),
            served: BTreeMap::new(),
            goaway: None,
            goaway_sent: None,
            unseen: 0,
            #[cfg(test)]
            plant: Plant::None,
            #[cfg(test)]
            sent: Vec::new_in(Global),
        }
    }

    /// The earliest deadline of a request not yet ended.
    pub(super) fn next_deadline(&self) -> Option<Duration> {
        self.gets.values().map(|it| it.request.deadline).min()
    }
}

/// A bug the DST plants to show its oracle catches it (`the_oracle_catches_*`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// A body past its bound is delivered, not refused.
    NoBodyBound,
    /// A connection that ends ends none of its requests.
    NoOutcomeOnClose,
    /// A finished WebTransport stream's [`super::Event::StreamAcked`] comes as its FIN is
    /// handed to QUIC, not when the peer acknowledged it (`h3::tests`'s acknowledgement test).
    AckedAtSent,
}

/// A client's request.
struct Get {
    request: Request,
    /// Its stream, once sent; `None` while it waits for the handshake or a stream.
    stream: Option<StreamId>,
    /// Whether it may go before the handshake completes (it came as an [`Early`]).
    may_go_early: bool,
    /// Whether its HEADERS went as early data.
    early: bool,
    /// Whether it was sent again after the server refused the early data it went in.
    resent: bool,
    head: Option<ResponseHead>,
    /// Bytes of the frame being read still to come, and whether that frame is DATA.
    left: u64,
    data: bool,
    /// Body bytes delivered.
    body: u64,
    /// The most body bytes the answer may have, once its head came.
    most: u64,
    /// The body's exact length, when the asker or the content-length fixes it.
    exact: Option<u64>,
}

fn request_error(peer: SocketAddr, why: String) -> Error {
    Error { peer: Some(peer), step: Step::Request, why }
}

/// What a handshake that failed says, in the words an asker can act on.
pub(super) fn handshake_failure(reason: &ConnectionError) -> String {
    let alpn = TransportErrorCode::crypto(NO_APPLICATION_PROTOCOL);
    match reason {
        ConnectionError::TimedOut => {
            "the QUIC handshake timed out: nothing answered over UDP (the host may not serve HTTP/3, or UDP is blocked)".to_owned()
        }
        ConnectionError::VersionMismatch => {
            "QUIC version negotiation failed: the host speaks no QUIC version this client does".to_owned()
        }
        ConnectionError::TransportError(error) if error.code == alpn => {
            "the host refused ALPN h3 (TLS no_application_protocol): it does not serve HTTP/3".to_owned()
        }
        ConnectionError::ConnectionClosed(close) if close.error_code == alpn => {
            "the host refused ALPN h3 (TLS no_application_protocol): it does not serve HTTP/3".to_owned()
        }
        other => format!("the QUIC handshake failed: {other}"),
    }
}

impl Endpoint {
    /// Opens a connection for plain requests to `to`, named `server_name` (what TLS checks and
    /// tickets are kept by). A ticket for the name, if one was added, is used: requests asked
    /// with [`Endpoint::request_early`] before the handshake completes go as early data.
    pub fn connect_requests(&mut self, to: SocketAddr, server_name: &str) -> Result<super::ConnId, Error> {
        let Some(client) = &self.client else {
            return Err(Error {
                peer: Some(to),
                step: Step::Config,
                why: "this endpoint was made without a ClientConfig".to_owned(),
            });
        };
        let quic = client.quic.clone();
        let (handle, conn) = self
            .quic
            .connect(self.at(), quic, to, server_name)
            .map_err(|why| Error { peer: Some(to), step: Step::Handshake, why: why.to_string() })?;
        let id = self.add(handle, conn, to, Side::Client, None);
        self.drive(id);
        Ok(id)
    }

    /// Asks `request` on connection `conn`. It is sent once the handshake is done (never as
    /// early data: only an [`Early`] may go early), and ends in exactly one
    /// [`Event::RequestEnded`].
    pub fn request(&mut self, conn: super::ConnId, request: Request) -> Result<RequestId, Error> {
        self.ask(conn, request, false)
    }

    /// Asks a GET or HEAD on connection `conn`, as early data when the connection resumed with
    /// a ticket and its handshake is not done; otherwise as [`Endpoint::request`] would.
    pub fn request_early(&mut self, conn: super::ConnId, early: Early) -> Result<RequestId, Error> {
        self.ask(conn, early.into_request(), true)
    }

    /// A test's way to send any request early, past the type that forbids it: what the DST's
    /// third planted bug does.
    #[cfg(test)]
    pub(crate) fn plant_early(&mut self, conn: super::ConnId, request: Request) -> Result<RequestId, Error> {
        self.ask(conn, request, true)
    }

    /// Plants `plant` on connection `conn`.
    #[cfg(test)]
    pub(crate) fn plant(&mut self, conn: super::ConnId, plant: Plant) {
        if let Some(it) = self.conns.get_mut(&conn) {
            it.reqs.plant = plant;
        }
    }

    /// Each request's sends on connection `conn` so far: its number, and whether it went early.
    #[cfg(test)]
    pub(crate) fn sent_log(&self, conn: super::ConnId) -> Vec<(u64, bool), Global> {
        let mut out = Vec::new_in(Global);
        if let Some(it) = self.conns.get(&conn) {
            out.extend_from_slice(&it.reqs.sent);
        }
        out
    }

    fn ask(&mut self, conn: super::ConnId, request: Request, may_go_early: bool) -> Result<RequestId, Error> {
        let peer = self.peer(conn);
        let fail = |why: String| Error { peer, step: Step::Request, why };
        if !request.path.starts_with('/') || request.authority.is_empty() {
            return Err(fail(format!(
                "{}: a request's path starts with `/` and its authority is not empty",
                request.object()
            )));
        }
        if request.range.is_some_and(|it| it.len == 0 || it.at.checked_add(it.len).is_none()) {
            return Err(fail(format!("{}: a range is at least 1 byte and ends by 2^64", request.object())));
        }
        self.with(conn, |conn, cx| {
            if conn.side != Side::Client {
                return Err(conn.error(Step::Request, "a server does not ask requests".to_owned()));
            }
            let n = conn.reqs.next;
            conn.reqs.next += 1;
            let id = RequestId { conn: cx.id, n };
            let get = Get {
                request,
                stream: None,
                may_go_early,
                early: false,
                resent: false,
                head: None,
                left: 0,
                data: false,
                body: 0,
                most: 0,
                exact: None,
            };
            conn.reqs.gets.insert(n, get);
            if let Some(last) = conn.reqs.goaway {
                let why = format!("the server's GOAWAY (stream {last}) came before it was asked");
                conn.end_get(n, RequestEnd::NotStarted { why }, cx);
            } else if conn.quic.is_closed() {
                let why = "its connection was closing as it was asked".to_owned();
                conn.end_get(n, RequestEnd::NotStarted { why }, cx);
            } else if conn.connected || (may_go_early && conn.quic.has_0rtt()) {
                conn.send_get(n, cx);
            }
            Ok(id)
        })
    }

    /// Cancels a request: its stream is stopped and reset with `H3_REQUEST_CANCELLED`, and it
    /// ends [`RequestEnd::Cancelled`]. One that already ended is left as it ended.
    pub fn cancel(&mut self, request: RequestId) -> Result<(), Error> {
        self.with(request.conn, |conn, cx| {
            if conn.reqs.gets.contains_key(&request.n) {
                conn.end_get(request.n, RequestEnd::Cancelled, cx);
            }
            Ok(())
        })
    }

    /// Says `bytes` of a request's [`Event::ResponseData`] were taken, so more may be read.
    pub fn consumed_response(&mut self, request: RequestId, bytes: usize) {
        _ = self.with(request.conn, |conn, cx| {
            if let Some(id) = conn.reqs.gets.get(&request.n).and_then(|it| it.stream)
                && let Some(stream) = conn.streams.get_mut(&id)
            {
                stream.unconsumed = stream.unconsumed.saturating_sub(bytes);
                conn.read(id, cx);
            }
            Ok(())
        });
    }

    /// Answers a request this server was handed ([`Event::Request`]): `status`, `headers`
    /// (lowercase names, no pseudo-fields) and `body`, in DATA frames of at most
    /// [`DATA_FRAME`] bytes, then FIN.
    pub fn respond(
        &mut self,
        request: RequestId,
        status: u16,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(), Error> {
        self.with(request.conn, |conn, cx| {
            let Some(id) = conn.reqs.served.remove(&request.n) else {
                return Err(conn.error(
                    Step::Request,
                    format!("request {} was not handed to this server, or was answered", request.n),
                ));
            };
            let Some(stream) = conn.streams.get_mut(&id) else {
                return Err(conn.error(
                    Step::Request,
                    format!("request {}'s client cancelled it", request.n),
                ));
            };
            let status = status.to_string();
            let mut fields = Vec::new_in(Global);
            fields.push((":status", status.as_str()));
            fields.extend_from_slice(headers);
            let mut head = Vec::new_in(Global);
            qpack::encode(&fields, &mut head);
            frame::put(&mut stream.out, frame::HEADERS, &head);
            for chunk in body.chunks(DATA_FRAME) {
                frame::put(&mut stream.out, frame::DATA, chunk);
            }
            stream.fin = true;
            conn.flush(id, cx);
            Ok(())
        })
    }

    /// Sends a server's GOAWAY on connection `conn`: requests the client opened so far are
    /// still answered, later ones are refused with `H3_REQUEST_REJECTED`.
    pub fn go_away(&mut self, conn: super::ConnId) -> Result<(), Error> {
        self.with(conn, |conn, cx| {
            if conn.side != Side::Server {
                return Err(conn.error(Step::Request, "only a server sends GOAWAY here".to_owned()));
            }
            let last = conn.reqs.unseen;
            conn.reqs.goaway_sent = Some(last);
            let Some(control) =
                conn.streams.iter().find(|(_, it)| it.kind == Kind::Control).map(|(id, _)| *id)
            else {
                return Err(conn.error(Step::Request, "the control stream is not open yet".to_owned()));
            };
            let mut payload = Vec::new_in(Global);
            varint::put(&mut payload, last);
            conn.send_frame(control, frame::GOAWAY, &payload, cx);
            Ok(())
        })
    }

    /// A ticket a driver kept: connections to its server made from now on resume with it.
    pub fn add_ticket(&mut self, ticket: &Ticket) -> Result<(), Error> {
        let Some(client) = &self.client else {
            return Err(Error {
                peer: None,
                step: Step::Config,
                why: "this endpoint was made without a ClientConfig, so it keeps no ticket".to_owned(),
            });
        };
        client.tickets.add(&client.tls, ticket)
    }
}

impl Conn {
    /// Puts request `n` on a new stream, if the peer allows one now ([`Conn::send_waiting`]
    /// tries again as it does).
    fn send_get(&mut self, n: u64, cx: &mut Cx<'_>) {
        let Some(get) = self.reqs.gets.get(&n) else { return };
        if get.stream.is_some() {
            return;
        }
        let Some(id) = self.quic.streams().open(Dir::Bi) else { return };
        let early = !self.connected;
        let request = &get.request;
        let range = request.range.map(|it| it.header());
        let mut fields = Vec::new_in(Global);
        fields.push((":method", request.method.as_str()));
        fields.push((":scheme", "https"));
        fields.push((":authority", request.authority.as_str()));
        fields.push((":path", request.path.as_str()));
        if let Some(range) = &range {
            fields.push(("range", range.as_str()));
        }
        fields.push(("user-agent", USER_AGENT));
        let mut head = Vec::new_in(Global);
        qpack::encode(&fields, &mut head);
        let mut stream = Stream::new(Kind::Get(n));
        frame::put(&mut stream.out, frame::HEADERS, &head);
        stream.fin = true;
        self.streams.insert(id, stream);
        if let Some(get) = self.reqs.gets.get_mut(&n) {
            get.stream = Some(id);
            get.early = early;
        }
        #[cfg(test)]
        self.reqs.sent.push((n, early));
        self.flush(id, cx);
    }

    /// Sends the requests that wait, in the order asked, as far as the peer allows streams
    /// and the handshake allows them (only those that may go early go before it).
    pub(super) fn send_waiting(&mut self, cx: &mut Cx<'_>) {
        let mut waiting = Vec::new_in(Global);
        waiting.extend(
            self.reqs
                .gets
                .iter()
                .filter(|(_, it)| {
                    it.stream.is_none()
                        && (self.connected || (it.may_go_early && self.quic.has_0rtt()))
                })
                .map(|(n, _)| *n),
        );
        for n in waiting {
            self.send_get(n, cx);
            if self.reqs.gets.get(&n).is_some_and(|it| it.stream.is_none()) {
                // No stream allowed: the rest wait for StreamsAvailable.
                return;
            }
        }
    }

    /// The handshake completed. If the server refused this client's early data, quinn-proto
    /// has discarded every stream this side opened and will hand their IDs out again: each
    /// request that went early loses its stream and is sent again, once. This is QUIC's own
    /// rule for rejected 0-RTT (RFC 9001, 4.6.2: the client resends what the server never
    /// processed), not a retry: the server saw none of it, and no answer was lost.
    pub(super) fn requests_connected(&mut self) {
        if self.side != Side::Client || !self.quic.has_0rtt() || self.quic.accepted_0rtt() {
            return;
        }
        let side = self.side;
        self.streams.retain(|id, _| id.initiator() != side);
        for get in self.reqs.gets.values_mut() {
            if get.stream.take().is_some() {
                // A connection completes its handshake once, so a request is resent at most
                // once; `resent` says so to the DST.
                debug_assert!(!get.resent, "a request resent twice after rejected early data");
                get.resent = true;
                get.early = false;
            }
        }
    }

    /// Ends request `n` with `end`: its stream, if open, is stopped and reset with
    /// `H3_REQUEST_CANCELLED` (its answer is not wanted) and forgotten.
    pub(super) fn end_get(&mut self, n: u64, end: RequestEnd, cx: &mut Cx<'_>) {
        let Some(get) = self.reqs.gets.remove(&n) else { return };
        if let Some(id) = get.stream
            && let Some(stream) = self.streams.remove(&id)
        {
            let cancelled = h3_code(code::H3_REQUEST_CANCELLED);
            if stream.recv_open {
                _ = self.quic.recv_stream(id).stop(cancelled);
            }
            if !stream.finished || stream.out_at < stream.out.len() {
                _ = self.quic.send_stream(id).reset(cancelled);
            }
        }
        cx.events.push_back(Event::RequestEnded { request: RequestId { conn: cx.id, n }, end });
    }

    /// The connection is over: each request still in flight ends, as not started when it was
    /// never sent or the server's GOAWAY excluded it, and as lost otherwise.
    pub(super) fn end_requests(&mut self, error: &Error, cx: &mut Cx<'_>) {
        #[cfg(test)]
        if self.reqs.plant == Plant::NoOutcomeOnClose {
            return;
        }
        let mut numbers = Vec::new_in(Global);
        numbers.extend(self.reqs.gets.keys().copied());
        for n in numbers {
            let Some(get) = self.reqs.gets.get(&n) else { continue };
            let end = match get.stream {
                None => RequestEnd::NotStarted {
                    why: format!("{}: its connection ended before it was sent: {error}", get.request.object()),
                },
                Some(id) if self.reqs.goaway.is_some_and(|last| u64::from(id) >= last) => {
                    RequestEnd::NotStarted {
                        why: format!(
                            "{}: the server's GOAWAY said it did not start stream {id}, and then the connection ended",
                            get.request.object()
                        ),
                    }
                }
                Some(_) => RequestEnd::Lost(error.clone()),
            };
            self.end_get(n, end, cx);
        }
        self.reqs.served.clear();
    }

    /// Ends each request whose deadline is past.
    pub(super) fn request_deadlines(&mut self, cx: &mut Cx<'_>) {
        let mut due = Vec::new_in(Global);
        due.extend(
            self.reqs.gets.iter().filter(|(_, it)| it.request.deadline <= cx.now).map(|(n, _)| *n),
        );
        for n in due {
            self.end_get(n, RequestEnd::TimedOut, cx);
        }
    }

    /// The server's GOAWAY came, naming the first stream it did not start (`last`).
    pub(super) fn goaway_came(&mut self, last: u64, cx: &mut Cx<'_>) {
        if self.side != Side::Client {
            // A client's GOAWAY names push IDs, and no push is ever allowed here.
            return;
        }
        if self.reqs.goaway.is_some_and(|before| last > before) {
            let why = format!("a GOAWAY's stream ID rose from {:?} to {last}", self.reqs.goaway);
            return self.fail(code::H3_ID_ERROR, Step::Request, why, cx);
        }
        self.reqs.goaway = Some(last);
        let mut excluded = Vec::new_in(Global);
        excluded.extend(
            self.reqs
                .gets
                .iter()
                .filter(|(_, it)| it.stream.is_none_or(|id| u64::from(id) >= last))
                .map(|(n, _)| *n),
        );
        for n in excluded {
            let Some(get) = self.reqs.gets.get(&n) else { continue };
            let why = format!(
                "{}: the server's GOAWAY (stream {last}) says it did not start it",
                get.request.object()
            );
            self.end_get(n, RequestEnd::NotStarted { why }, cx);
        }
    }

    /// Parses what request stream `id` (request `n`'s) has buffered: frame headers, the
    /// answer's head, its DATA as it comes. Each turn takes at least one byte or returns, so
    /// the loop is bounded by the bytes buffered.
    pub(super) fn get_frames(&mut self, id: StreamId, n: u64, cx: &mut Cx<'_>) {
        loop {
            let Some(stream) = self.streams.get_mut(&id) else { return };
            let Some(get) = self.reqs.gets.get_mut(&n) else { return };
            if stream.inbuf.is_empty() {
                return;
            }
            if get.left > 0 {
                let take = (get.left.min(stream.inbuf.len() as u64)) as usize;
                get.left -= take as u64;
                if !get.data {
                    stream.inbuf.drain(..take);
                    continue;
                }
                let past = get.body + take as u64 > get.most;
                #[cfg(test)]
                let past = past && self.reqs.plant != Plant::NoBodyBound;
                if past {
                    let why = format!(
                        "{}: the body ran past the {} bytes it may have",
                        get.request.object(),
                        get.most
                    );
                    let error = request_error(self.peer, why);
                    return self.end_get(n, RequestEnd::Failed(error), cx);
                }
                let mut bytes = Vec::with_capacity_in(take, Global);
                bytes.extend(stream.inbuf.drain(..take));
                get.body += take as u64;
                stream.unconsumed += take;
                cx.events.push_back(Event::ResponseData { request: RequestId { conn: cx.id, n }, bytes });
                continue;
            }
            let Some((ty, a)) = varint::get(&stream.inbuf) else { return };
            let Some((len, b)) = varint::get(&stream.inbuf[a..]) else { return };
            match ty {
                frame::DATA => {
                    if get.head.is_none() {
                        let why = format!("{}: DATA before the answer's HEADERS", get.request.object());
                        return self.fail(code::H3_FRAME_UNEXPECTED, Step::Request, why, cx);
                    }
                    stream.inbuf.drain(..a + b);
                    get.left = len;
                    get.data = true;
                }
                frame::HEADERS => {
                    if len > frame::MOST_FRAME {
                        let why = format!(
                            "{}: an answer's HEADERS of {len} bytes, past the {} this endpoint reads",
                            get.request.object(),
                            frame::MOST_FRAME
                        );
                        return self.fail(code::H3_EXCESSIVE_LOAD, Step::Request, why, cx);
                    }
                    let end = a + b + len as usize;
                    if stream.inbuf.len() < end {
                        return;
                    }
                    let mut block = Vec::with_capacity_in(len as usize, Global);
                    block.extend_from_slice(&stream.inbuf[a + b..end]);
                    stream.inbuf.drain(..end);
                    self.head_came(n, &block, cx);
                }
                frame::SETTINGS
                | frame::GOAWAY
                | frame::MAX_PUSH_ID
                | frame::CANCEL_PUSH
                | frame::PUSH_PROMISE => {
                    let why = format!("frame {ty:#x} on a request stream, where it may not be");
                    return self.fail(code::H3_FRAME_UNEXPECTED, Step::Request, why, cx);
                }
                // Unknown and GREASE frames: skipped as they come (RFC 9114, 9).
                _ => {
                    stream.inbuf.drain(..a + b);
                    get.left = len;
                    get.data = false;
                }
            }
        }
    }

    /// A HEADERS frame came on request `n`'s stream: the answer's head (after any 1xx), or
    /// trailers, which nothing here reads.
    fn head_came(&mut self, n: u64, block: &[u8], cx: &mut Cx<'_>) {
        let fields = match qpack::decode(block) {
            Ok(it) => it,
            Err(why) => return self.fail(code::QPACK_DECOMPRESSION_FAILED, Step::Request, why, cx),
        };
        let Some(get) = self.reqs.gets.get_mut(&n) else { return };
        if get.head.is_some() {
            return;
        }
        let field = |name: &str| fields.iter().find(|(it, _)| it == name).map(|(_, v)| v.as_str());
        let object = get.request.object();
        let Some(status) = field(":status").and_then(|it| it.parse::<u16>().ok()) else {
            let why = format!("{object}: the answer has no :status");
            return self.fail(code::H3_MESSAGE_ERROR, Step::Request, why, cx);
        };
        if (100..=199).contains(&status) {
            return;
        }
        let content_length = match field("content-length").map(str::parse::<u64>) {
            None => None,
            Some(Ok(it)) => Some(it),
            Some(Err(_)) => {
                let why = format!("{object}: the answer's content-length is not a number");
                let error = request_error(self.peer, why);
                return self.end_get(n, RequestEnd::Failed(error), cx);
            }
        };
        let head = ResponseHead {
            status,
            content_length,
            content_range: field("content-range").map(str::to_owned),
            etag: field("etag").map(str::to_owned),
        };
        let request = RequestId { conn: cx.id, n };
        get.head = Some(head.clone());
        cx.events.push_back(Event::ResponseHead { request, head });
        if !(200..=299).contains(&status) {
            // Its body is not read: the status is the answer.
            return self.end_get(n, RequestEnd::Done, cx);
        }
        if get.request.method == http::Method::HEAD {
            get.most = 0;
            get.exact = None;
            return;
        }
        let (most, exact) = match get.request.length {
            Length::Exactly(it) => (it, Some(it)),
            Length::AtMost(it) => (it, None),
        };
        let refused = match (content_length, exact) {
            (Some(said), Some(asked)) if said != asked => {
                Some(format!("{object}: the answer's content-length is {said}, and {asked} bytes were asked"))
            }
            (Some(said), None) if said > most => {
                Some(format!("{object}: the answer's content-length is {said}, past the {most} bytes it may have"))
            }
            _ => None,
        };
        if let Some(why) = refused {
            let error = request_error(self.peer, why);
            return self.end_get(n, RequestEnd::Failed(error), cx);
        }
        get.most = content_length.unwrap_or(most);
        get.exact = content_length.or(exact);
    }

    /// Request `n`'s stream ended (`end`), after all it had was parsed.
    pub(super) fn get_ended(&mut self, n: u64, end: End, cx: &mut Cx<'_>) {
        let Some(get) = self.reqs.gets.get(&n) else { return };
        let object = get.request.object();
        let outcome = match end {
            End::Reset(code::H3_REQUEST_REJECTED) => RequestEnd::NotStarted {
                why: format!("{object}: the server refused it with H3_REQUEST_REJECTED"),
            },
            End::Reset(code) => RequestEnd::Reset { code },
            End::Fin => match (&get.head, get.left, get.exact) {
                (None, ..) => RequestEnd::Failed(request_error(
                    self.peer,
                    format!("{object}: the stream ended before the answer's head"),
                )),
                (Some(_), 1.., _) => RequestEnd::Failed(request_error(
                    self.peer,
                    format!("{object}: the stream ended inside a frame"),
                )),
                (Some(_), 0, Some(exact)) if get.body != exact => RequestEnd::Failed(request_error(
                    self.peer,
                    format!("{object}: the body ended at {} bytes, and {exact} were expected", get.body),
                )),
                (Some(_), 0, _) => RequestEnd::Done,
            },
        };
        self.end_get(n, outcome, cx);
    }

    /// A server's request stream sent HEADERS that are not a WebTransport CONNECT: it is the
    /// asker's ([`Event::Request`]), or refused when this server sent a GOAWAY that excludes
    /// it. `false` when this server does not serve requests (the caller answers 400 or 501).
    pub(super) fn serve_came(&mut self, id: StreamId, fields: &[(String, String)], cx: &mut Cx<'_>) -> bool {
        if !self.reqs.serves {
            return false;
        }
        let field = |name: &str| fields.iter().find(|(it, _)| it == name).map(|(_, v)| v.clone());
        let (Some(method), Some(_), Some(authority), Some(path)) =
            (field(":method"), field(":scheme"), field(":authority"), field(":path"))
        else {
            return false;
        };
        if self.reqs.goaway_sent.is_some_and(|last| u64::from(id) >= last) {
            let rejected = h3_code(code::H3_REQUEST_REJECTED);
            _ = self.quic.recv_stream(id).stop(rejected);
            _ = self.quic.send_stream(id).reset(rejected);
            self.streams.remove(&id);
            return true;
        }
        let n = self.reqs.next;
        self.reqs.next += 1;
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.kind = Kind::Serve(n);
            stream.inbuf.clear();
        }
        self.reqs.served.insert(n, id);
        let served = Served { peer: self.peer, method, authority, path, range: field("range"), early: !self.connected };
        cx.events.push_back(Event::Request { request: RequestId { conn: cx.id, n }, served });
        true
    }

    /// A client opened bidirectional stream `id` on this server.
    pub(super) fn seen_bidi(&mut self, id: StreamId) {
        self.reqs.unseen = self.reqs.unseen.max(u64::from(id) + 4);
    }

    /// A served stream both of whose sides are over is forgotten, and its request with it.
    pub(super) fn forget_served(&mut self, id: StreamId, n: u64) {
        let Some(stream) = self.streams.get(&id) else { return };
        if stream.finished && !stream.recv_open {
            self.streams.remove(&id);
            self.reqs.served.remove(&n);
        }
    }
}
