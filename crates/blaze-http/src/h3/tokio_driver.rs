//! The tokio driver of [`Endpoint`]: the UDP socket and the timer, nothing else (the `tokio`
//! feature).
//!
//! One task owns the endpoint and loops: it tells the endpoint the time, hands it each datagram
//! the socket reads and each ask that comes in, sends the datagrams it hands back, routes its
//! [`Event`]s to the handles they are for, and sleeps until the next datagram, ask or deadline
//! (the driver contract in [`super`]). The handles ([`Driver`], [`Request`], [`Session`],
//! [`SendStream`], [`RecvStream`]) post asks to the loop and wait for its answers on channels.
//!
//! A [`RecvStream`] says each piece taken ([`Endpoint::consumed`]) as its reader takes the
//! last of it, so a slow reader holds its stream back rather than the driver buffering it. A
//! [`SendStream::write_all`] returns once QUIC took all of its bytes ([`Event::StreamSent`]).
//! Datagrams for a reader that is behind by [`DATAGRAMS_HELD`] are dropped, as the network may
//! drop them.
//!
//! The waits here open no span yet: blaze-trace is not in the graph (the crate's
//! documentation). Each is bounded by the endpoint's deadlines: a connection ends by its idle
//! timeout, and with it every session and stream, whose handles then fail.

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
};

use tokio::{
    net::UdpSocket,
    sync::{Mutex, mpsc, oneshot, watch},
    time::Instant,
};

use super::{
    ClientConfig, Closed, Direction, Endpoint, Error, Event, Identity, ServerConfig, SessionId,
    SessionRequest, Step, StreamRef,
};

/// The largest UDP datagram read.
pub const RECV_MOST: usize = 64 << 10;
/// The most datagrams held for a session whose reader is behind.
pub const DATAGRAMS_HELD: usize = 256;

/// A handle on a running driver, cheap to clone into any task.
#[derive(Clone)]
pub struct Driver {
    to_loop: mpsc::UnboundedSender<Message>,
    local: SocketAddr,
}

/// What the loop is told by the handles.
enum Message {
    Connect {
        to: SocketAddr,
        url: String,
        headers: Vec<(String, String)>,
        reply: oneshot::Sender<Result<Session, Closed>>,
    },
    Accept {
        session: SessionId,
        reply: oneshot::Sender<Result<Session, Closed>>,
    },
    Refuse {
        session: SessionId,
        status: u16,
    },
    Open {
        session: SessionId,
        open: Opening,
    },
    Send {
        stream: StreamRef,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<(), StreamError>>,
    },
    Finish {
        stream: StreamRef,
    },
    Reset {
        stream: StreamRef,
        code: u32,
    },
    Stop {
        stream: StreamRef,
        code: u32,
    },
    Consumed {
        stream: StreamRef,
        bytes: usize,
    },
    Datagram {
        session: SessionId,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    Close {
        session: SessionId,
        code: u32,
        reason: String,
    },
    SetIdentity {
        identity: Identity,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    CloseAll {
        code: u32,
        reason: String,
        reply: oneshot::Sender<()>,
    },
}

/// A stream asked for, waiting for the peer to allow one.
enum Opening {
    Bi(oneshot::Sender<Result<(SendStream, RecvStream), Closed>>),
    Uni(oneshot::Sender<Result<SendStream, Closed>>),
}

impl Opening {
    fn direction(&self) -> Direction {
        match self {
            Opening::Bi(_) => Direction::Bi,
            Opening::Uni(_) => Direction::Uni,
        }
    }

    fn fail(self, closed: Closed) {
        match self {
            Opening::Bi(reply) => _ = reply.send(Err(closed)),
            Opening::Uni(reply) => _ = reply.send(Err(closed)),
        }
    }
}

/// What a stream's reader gets from the loop.
enum Piece {
    Data(Vec<u8>),
    Fin,
    Reset(Option<u32>),
}

/// Why a stream's read or write failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// The peer stopped this side's sending, with its WebTransport code if it was one.
    Stopped(Option<u32>),
    /// The peer reset its sending, with its WebTransport code if it was one.
    Reset(Option<u32>),
    /// The core refused the ask.
    Refused(Error),
    /// The stream's session or the driver is gone.
    Gone,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Stopped(code) => write!(f, "the peer stopped the stream (code {code:?})"),
            StreamError::Reset(code) => write!(f, "the peer reset the stream (code {code:?})"),
            StreamError::Refused(error) => write!(f, "{error}"),
            StreamError::Gone => f.write_str("the stream's session, or the HTTP/3 driver, is gone"),
        }
    }
}

impl std::error::Error for StreamError {}

fn driver_gone(peer: Option<SocketAddr>) -> Closed {
    // alloc: an error's message, made only when it fails.
    Closed::Failed(Error { peer, step: Step::Session, why: "the HTTP/3 driver stopped".to_owned() })
}

impl Driver {
    /// Starts the loop on `runtime` over `socket` (bound, blocking or not), serving with
    /// `server`, connecting with `client`, or both; it runs while any handle is held. The
    /// receiver gets each session a peer asks for; a request it does not take (the receiver
    /// gone) is refused with 503.
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        socket: std::net::UdpSocket,
        server: Option<ServerConfig>,
        client: Option<ClientConfig>,
    ) -> Result<(Driver, mpsc::UnboundedReceiver<Request>), Error> {
        // alloc: an error's message, made only when it fails.
        let io = |what: &str, why: std::io::Error| Error {
            peer: None,
            step: Step::Config,
            why: format!("{what}: {why}"),
        };
        let local = socket.local_addr().map_err(|why| io("the UDP socket's address", why))?;
        socket.set_nonblocking(true).map_err(|why| io("the UDP socket made non-blocking", why))?;
        let socket = {
            let _entered = runtime.enter();
            UdpSocket::from_std(socket).map_err(|why| io("the UDP socket given to tokio", why))?
        };
        let epoch = Instant::now();
        let endpoint = Endpoint::new(epoch.into_std(), server, client)?;
        let (to_loop, inbox) = mpsc::unbounded_channel();
        let (incoming, requests) = mpsc::unbounded_channel();
        let state = Loop {
            endpoint,
            socket,
            epoch,
            to_loop: to_loop.downgrade(),
            incoming,
            connects: BTreeMap::new(),
            sessions: BTreeMap::new(),
            streams: BTreeMap::new(),
            closing: Vec::new(),
        };
        runtime.spawn(state.run(inbox));
        Ok((Driver { to_loop, local }, requests))
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Opens a session to `to` at `url` (`https://host:port/path`), sending `headers`
    /// (lowercase names, no pseudo-fields) with its CONNECT.
    pub async fn connect(
        &self,
        to: SocketAddr,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Session, Closed> {
        let (reply, answer) = oneshot::channel();
        // alloc: the ask's fields, once a session.
        let headers = headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect();
        // alloc: as above.
        let url = url.to_owned();
        if self.to_loop.send(Message::Connect { to, url, headers, reply }).is_err() {
            return Err(driver_gone(Some(to)));
        }
        answer.await.unwrap_or_else(|_| Err(driver_gone(Some(to))))
    }

    /// Serves `identity` to connections from now on; open ones keep theirs.
    pub async fn set_identity(&self, identity: Identity) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        // alloc: an error's message, made only when it fails.
        let gone = || Error {
            peer: None,
            step: Step::Config,
            why: "the HTTP/3 driver stopped".to_owned(),
        };
        if self.to_loop.send(Message::SetIdentity { identity, reply }).is_err() {
            return Err(gone());
        }
        answer.await.unwrap_or_else(|_| Err(gone()))
    }

    /// Closes every session with `code` and `reason`, then every connection; returns once each
    /// connection's close is sent (at most [`super::LINGER`] after the sessions' closes, when
    /// a peer does not acknowledge them).
    pub async fn close(&self, code: u32, reason: &str) {
        let (reply, answer) = oneshot::channel();
        // alloc: the reason, once as the driver stops.
        if self.to_loop.send(Message::CloseAll { code, reason: reason.to_owned(), reply }).is_ok() {
            _ = answer.await;
        }
    }
}

/// A session a peer asked for: [`Request::accept`] or [`Request::refuse`] it. Dropped
/// unanswered, it is refused with 500.
pub struct Request {
    session: SessionId,
    request: SessionRequest,
    to_loop: mpsc::UnboundedSender<Message>,
    answered: bool,
}

impl Request {
    /// `:path`, with the query.
    pub fn path(&self) -> &str {
        &self.request.path
    }

    pub fn authority(&self) -> &str {
        &self.request.authority
    }

    pub fn origin(&self) -> Option<&str> {
        self.request.origin.as_deref()
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.request.user_agent.as_deref()
    }

    /// Every field of the CONNECT, pseudo-fields too, in the order sent.
    pub fn headers(&self) -> &[(String, String)] {
        &self.request.headers
    }

    pub fn peer(&self) -> SocketAddr {
        self.request.peer
    }

    /// Accepts the session: its CONNECT is answered 200.
    pub async fn accept(mut self) -> Result<Session, Closed> {
        self.answered = true;
        let peer = self.request.peer;
        let (reply, answer) = oneshot::channel();
        if self.to_loop.send(Message::Accept { session: self.session, reply }).is_err() {
            return Err(driver_gone(Some(peer)));
        }
        answer.await.unwrap_or_else(|_| Err(driver_gone(Some(peer))))
    }

    /// Refuses the session with `status` (300 to 599).
    pub fn refuse(mut self, status: u16) {
        self.answered = true;
        // A driver that stopped has refused it already, by closing its connection.
        _ = self.to_loop.send(Message::Refuse { session: self.session, status });
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if !self.answered {
            _ = self.to_loop.send(Message::Refuse { session: self.session, status: 500 });
        }
    }
}

/// An open session, cheap to clone; the last clone dropped closes it with code 0.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    id: SessionId,
    peer: SocketAddr,
    to_loop: mpsc::UnboundedSender<Message>,
    bi: Mutex<mpsc::UnboundedReceiver<(SendStream, RecvStream)>>,
    uni: Mutex<mpsc::UnboundedReceiver<RecvStream>>,
    datagrams: Mutex<mpsc::Receiver<Vec<u8>>>,
    closed: watch::Receiver<Option<Closed>>,
}

impl Session {
    pub fn id(&self) -> SessionId {
        self.inner.id
    }

    pub fn peer(&self) -> SocketAddr {
        self.inner.peer
    }

    /// How the session ended, once it has.
    pub async fn closed(&self) -> Closed {
        let mut closed = self.inner.closed.clone();
        match closed.wait_for(Option::is_some).await {
            Ok(it) => it.clone().unwrap_or_else(|| driver_gone(Some(self.inner.peer))),
            Err(_) => driver_gone(Some(self.inner.peer)),
        }
    }

    async fn ended(&self) -> Closed {
        self.closed().await
    }

    /// The next bidirectional stream the peer opens.
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream), Closed> {
        let next = self.inner.bi.lock().await.recv().await;
        match next {
            Some(it) => Ok(it),
            None => Err(self.ended().await),
        }
    }

    /// The next unidirectional stream the peer opens.
    pub async fn accept_uni(&self) -> Result<RecvStream, Closed> {
        let next = self.inner.uni.lock().await.recv().await;
        match next {
            Some(it) => Ok(it),
            None => Err(self.ended().await),
        }
    }

    /// The next datagram.
    pub async fn receive_datagram(&self) -> Result<Vec<u8>, Closed> {
        let next = self.inner.datagrams.lock().await.recv().await;
        match next {
            Some(it) => Ok(it),
            None => Err(self.ended().await),
        }
    }

    /// Opens a bidirectional stream, waiting while the peer allows no more.
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream), Closed> {
        let (reply, answer) = oneshot::channel();
        if self
            .inner
            .to_loop
            .send(Message::Open { session: self.inner.id, open: Opening::Bi(reply) })
            .is_err()
        {
            return Err(driver_gone(Some(self.inner.peer)));
        }
        answer.await.unwrap_or_else(|_| Err(driver_gone(Some(self.inner.peer))))
    }

    /// Opens a unidirectional stream, waiting while the peer allows no more.
    pub async fn open_uni(&self) -> Result<SendStream, Closed> {
        let (reply, answer) = oneshot::channel();
        if self
            .inner
            .to_loop
            .send(Message::Open { session: self.inner.id, open: Opening::Uni(reply) })
            .is_err()
        {
            return Err(driver_gone(Some(self.inner.peer)));
        }
        answer.await.unwrap_or_else(|_| Err(driver_gone(Some(self.inner.peer))))
    }

    /// Sends a datagram; it may be lost, as datagrams may.
    pub async fn send_datagram(&self, bytes: &[u8]) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        // alloc: one datagram, handed to the loop.
        let message = Message::Datagram { session: self.inner.id, bytes: bytes.to_vec(), reply };
        // alloc: an error's message, made only when it fails.
        let gone = || Error {
            peer: Some(self.inner.peer),
            step: Step::Datagram,
            why: "the HTTP/3 driver stopped".to_owned(),
        };
        if self.inner.to_loop.send(message).is_err() {
            return Err(gone());
        }
        answer.await.unwrap_or_else(|_| Err(gone()))
    }

    /// Closes the session with `code` and `reason`.
    pub fn close(&self, code: u32, reason: &str) {
        // alloc: the reason, once as a session closes.
        let reason = reason.to_owned();
        // A driver that stopped has closed it already.
        _ = self.inner.to_loop.send(Message::Close { session: self.inner.id, code, reason });
    }
}

impl Drop for SessionInner {
    /// The last handle gone closes the session, as nothing could read it any more.
    fn drop(&mut self) {
        _ = self.to_loop.send(Message::Close { session: self.id, code: 0, reason: String::new() });
    }
}

/// This side's half of a stream. Dropped, it is finished.
pub struct SendStream {
    stream: StreamRef,
    to_loop: mpsc::UnboundedSender<Message>,
    stopped: watch::Receiver<Option<Option<u32>>>,
    done: bool,
}

impl SendStream {
    pub fn id(&self) -> StreamRef {
        self.stream
    }

    /// Sends `bytes`, returning once QUIC took all of them.
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), StreamError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let (reply, answer) = oneshot::channel();
        // alloc: the bytes, handed to the loop.
        if self
            .to_loop
            .send(Message::Send { stream: self.stream, bytes: bytes.to_vec(), reply })
            .is_err()
        {
            return Err(StreamError::Gone);
        }
        answer.await.unwrap_or(Err(StreamError::Gone))
    }

    /// Finishes the stream once what was written is sent.
    pub fn finish(&mut self) {
        if !std::mem::replace(&mut self.done, true) {
            _ = self.to_loop.send(Message::Finish { stream: self.stream });
        }
    }

    /// Resets the stream with `code`, dropping what was not sent.
    pub fn reset(&mut self, code: u32) {
        if !std::mem::replace(&mut self.done, true) {
            _ = self.to_loop.send(Message::Reset { stream: self.stream, code });
        }
    }

    /// Waits for the peer to stop the stream: its WebTransport code, if it was one. Never
    /// returns while the stream lives and is not stopped; `None` too when it is gone.
    pub async fn stopped(&mut self) -> Option<u32> {
        match self.stopped.wait_for(Option::is_some).await {
            Ok(it) => it.flatten(),
            Err(_) => None,
        }
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The peer's half of a stream. Dropped before its end, it is stopped with code 0.
pub struct RecvStream {
    stream: StreamRef,
    to_loop: mpsc::UnboundedSender<Message>,
    pieces: mpsc::UnboundedReceiver<Piece>,
    piece: Vec<u8>,
    at: usize,
    ended: bool,
}

impl RecvStream {
    pub fn id(&self) -> StreamRef {
        self.stream
    }

    /// Reads into `out`: the bytes read, or `None` at the stream's end.
    pub async fn read(&mut self, out: &mut [u8]) -> Result<Option<usize>, StreamError> {
        loop {
            if self.at < self.piece.len() {
                let n = out.len().min(self.piece.len() - self.at);
                out[..n].copy_from_slice(&self.piece[self.at..self.at + n]);
                self.at += n;
                if self.at == self.piece.len() {
                    let bytes = self.piece.len();
                    self.piece.clear();
                    self.at = 0;
                    _ = self.to_loop.send(Message::Consumed { stream: self.stream, bytes });
                }
                return Ok(Some(n));
            }
            if self.ended {
                return Ok(None);
            }
            match self.pieces.recv().await {
                Some(Piece::Data(bytes)) => {
                    self.piece = bytes;
                    self.at = 0;
                }
                Some(Piece::Fin) => {
                    self.ended = true;
                    return Ok(None);
                }
                Some(Piece::Reset(code)) => {
                    self.ended = true;
                    return Err(StreamError::Reset(code));
                }
                None => {
                    self.ended = true;
                    return Err(StreamError::Gone);
                }
            }
        }
    }

    /// Asks the peer to stop sending, with `code`.
    pub fn stop(&mut self, code: u32) {
        if !std::mem::replace(&mut self.ended, true) {
            _ = self.to_loop.send(Message::Stop { stream: self.stream, code });
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        self.stop(0);
    }
}

/// Which side opened a stream.
#[derive(Clone, Copy)]
enum Opener {
    Here,
    Peer,
}

/// The loop's side of a session.
struct SessionTx {
    bi: mpsc::UnboundedSender<(SendStream, RecvStream)>,
    uni: mpsc::UnboundedSender<RecvStream>,
    datagrams: mpsc::Sender<Vec<u8>>,
    closed: watch::Sender<Option<Closed>>,
    opening: VecDeque<Opening>,
}

/// The loop's side of a stream.
struct StreamTx {
    pieces: Option<mpsc::UnboundedSender<Piece>>,
    stopped: Option<watch::Sender<Option<Option<u32>>>>,
    sent: Option<oneshot::Sender<Result<(), StreamError>>>,
}

impl StreamTx {
    fn over(&self) -> bool {
        self.pieces.is_none() && self.stopped.is_none()
    }
}

struct Loop {
    endpoint: Endpoint,
    socket: UdpSocket,
    epoch: Instant,
    /// Weak, so the loop ends once every handle is gone.
    to_loop: mpsc::WeakUnboundedSender<Message>,
    incoming: mpsc::UnboundedSender<Request>,
    connects: BTreeMap<SessionId, oneshot::Sender<Result<Session, Closed>>>,
    sessions: BTreeMap<SessionId, SessionTx>,
    streams: BTreeMap<StreamRef, StreamTx>,
    closing: Vec<oneshot::Sender<()>>,
}

impl Loop {
    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Message>) {
        // alloc: the receive buffer, once a driver.
        let mut buf = vec![0u8; RECV_MOST];
        loop {
            self.endpoint.advance_clock(self.epoch.elapsed());
            self.events();
            self.transmit().await;
            if !self.closing.is_empty() && self.endpoint.all_closed() {
                for done in self.closing.drain(..) {
                    _ = done.send(());
                }
            }
            let deadline = self.endpoint.next_deadline().map(|it| self.epoch + it);
            tokio::select! {
                got = self.socket.recv_from(&mut buf) => match got {
                    Ok((n, from)) => self.endpoint.received(from, &buf[..n]),
                    // An ICMP error a peer's datagram caused (macOS and Windows report them on
                    // the next read): that peer's connection times out on its own.
                    Err(why) if matches!(why.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => {}
                    Err(why) => {
                        // alloc: an error's message, made only when it fails.
                        let error = Error { peer: None, step: Step::Session, why: format!("the UDP socket failed: {why}") };
                        self.fail_all(&error);
                        return;
                    }
                },
                message = inbox.recv() => match message {
                    Some(message) => self.ask(message),
                    None => return,
                },
                () = sleep_until(deadline) => {}
            }
        }
    }

    /// Sends what the endpoint has to send; a send that fails is dropped, as the network may.
    async fn transmit(&mut self) {
        while let Some(transmit) = self.endpoint.next_transmit() {
            _ = self.socket.send_to(&transmit.bytes, transmit.to).await;
        }
    }

    /// Ends every session and handle with `error`, as the loop stops.
    fn fail_all(&mut self, error: &Error) {
        for (_, reply) in std::mem::take(&mut self.connects) {
            _ = reply.send(Err(Closed::Failed(error.clone())));
        }
        for (_, session) in std::mem::take(&mut self.sessions) {
            session.closed.send_replace(Some(Closed::Failed(error.clone())));
            for open in session.opening {
                open.fail(Closed::Failed(error.clone()));
            }
        }
        self.streams.clear();
    }

    fn handle(&self) -> Option<mpsc::UnboundedSender<Message>> {
        self.to_loop.upgrade()
    }

    fn ask(&mut self, message: Message) {
        let now = self.epoch.elapsed();
        self.endpoint.advance_clock(now);
        match message {
            Message::Connect { to, url, headers, reply } => {
                // alloc: the CONNECT's fields as the core takes them, once a session.
                let fields: Vec<(&str, &str)> =
                    headers.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
                match self.endpoint.connect(to, &url, &fields) {
                    Ok(session) => {
                        self.connects.insert(session, reply);
                    }
                    Err(error) => _ = reply.send(Err(Closed::Failed(error))),
                }
            }
            Message::Accept { session, reply } => match self.endpoint.accept(session) {
                Ok(()) => match self.session(session) {
                    Some(handle) => _ = reply.send(Ok(handle)),
                    None => _ = reply.send(Err(self.gone_session(session))),
                },
                Err(error) => _ = reply.send(Err(Closed::Failed(error))),
            },
            Message::Refuse { session, status } => {
                // A session already gone (its connection closed) needs no answer.
                _ = self.endpoint.refuse(session, status);
            }
            Message::Open { session, open } => match self.sessions.get_mut(&session) {
                Some(tx) if tx.opening.is_empty() => {
                    tx.opening.push_back(open);
                    self.open(session);
                }
                Some(tx) => tx.opening.push_back(open),
                None => open.fail(self.gone_session(session)),
            },
            Message::Send { stream, bytes, reply } => match self.endpoint.send(stream, &bytes) {
                Ok(()) => match self.streams.get_mut(&stream) {
                    Some(tx) => tx.sent = Some(reply),
                    None => _ = reply.send(Err(StreamError::Gone)),
                },
                Err(error) => _ = reply.send(Err(StreamError::Refused(error))),
            },
            Message::Finish { stream } => {
                // A stream already reset, stopped or gone has nothing to finish.
                _ = self.endpoint.finish(stream);
                self.send_over(stream);
            }
            Message::Reset { stream, code } => {
                _ = self.endpoint.reset(stream, code);
                self.send_over(stream);
            }
            Message::Stop { stream, code } => {
                _ = self.endpoint.stop(stream, code);
                if let Some(tx) = self.streams.get_mut(&stream) {
                    tx.pieces = None;
                }
                self.forget(stream);
            }
            Message::Consumed { stream, bytes } => self.endpoint.consumed(stream, bytes),
            Message::Datagram { session, bytes, reply } => {
                _ = reply.send(self.endpoint.send_datagram(session, &bytes))
            }
            Message::Close { session, code, reason } => {
                // A session already gone needs no close.
                _ = self.endpoint.close(session, code, &reason);
            }
            Message::SetIdentity { identity, reply } => {
                _ = reply.send(self.endpoint.set_identity(identity))
            }
            Message::CloseAll { code, reason, reply } => {
                self.endpoint.close_all(code, &reason);
                self.closing.push(reply);
            }
        }
    }

    fn gone_session(&self, session: SessionId) -> Closed {
        // alloc: an error's message, made only when it fails.
        let why =
            format!("session {} of connection {} is not open", session.stream, session.conn.0);
        Closed::Failed(Error { peer: self.endpoint.peer(session.conn), step: Step::Session, why })
    }

    /// The handle of a session that just opened, and the loop's side of it; `None` when its
    /// connection or every handle on the driver is gone.
    fn session(&mut self, id: SessionId) -> Option<Session> {
        let peer = self.endpoint.peer(id.conn)?;
        let to_loop = self.handle()?;
        let (bi, bi_rx) = mpsc::unbounded_channel();
        let (uni, uni_rx) = mpsc::unbounded_channel();
        let (datagrams, datagrams_rx) = mpsc::channel(DATAGRAMS_HELD);
        let (closed, closed_rx) = watch::channel(None);
        self.sessions
            .insert(id, SessionTx { bi, uni, datagrams, closed, opening: VecDeque::new() });
        let inner = SessionInner {
            id,
            peer,
            to_loop,
            bi: Mutex::new(bi_rx),
            uni: Mutex::new(uni_rx),
            datagrams: Mutex::new(datagrams_rx),
            closed: closed_rx,
        };
        // alloc: one per session.
        Some(Session { inner: Arc::new(inner) })
    }

    /// The handles of a stream, and the loop's side of it: a [`SendStream`] unless it is the
    /// peer's unidirectional one, a [`RecvStream`] unless it is this side's.
    fn stream(
        &mut self,
        stream: StreamRef,
        opener: Opener,
    ) -> Option<(Option<SendStream>, Option<RecvStream>)> {
        let to_loop = self.handle()?;
        let (sends, receives) = match (stream.direction(), opener) {
            (Direction::Bi, _) => (true, true),
            (Direction::Uni, Opener::Here) => (true, false),
            (Direction::Uni, Opener::Peer) => (false, true),
        };
        let mut tx = StreamTx { pieces: None, stopped: None, sent: None };
        let send = sends.then(|| {
            let (stopped, stopped_rx) = watch::channel(None);
            tx.stopped = Some(stopped);
            SendStream { stream, to_loop: to_loop.clone(), stopped: stopped_rx, done: false }
        });
        let recv = receives.then(|| {
            let (pieces, pieces_rx) = mpsc::unbounded_channel();
            tx.pieces = Some(pieces);
            RecvStream {
                stream,
                to_loop,
                pieces: pieces_rx,
                piece: Vec::new(),
                at: 0,
                ended: false,
            }
        });
        self.streams.insert(stream, tx);
        Some((send, recv))
    }

    /// Opens the streams asked for in `session`, in order, as far as the peer allows.
    fn open(&mut self, session: SessionId) {
        loop {
            let Some(tx) = self.sessions.get_mut(&session) else { return };
            let Some(open) = tx.opening.pop_front() else { return };
            match self.endpoint.open(session, open.direction()) {
                Ok(Some(stream)) => match (open, self.stream(stream, Opener::Here)) {
                    (Opening::Bi(reply), Some((Some(send), Some(recv)))) => {
                        _ = reply.send(Ok((send, recv)))
                    }
                    (Opening::Uni(reply), Some((Some(send), _))) => _ = reply.send(Ok(send)),
                    (open, _) => open.fail(driver_gone(None)),
                },
                Ok(None) => {
                    if let Some(tx) = self.sessions.get_mut(&session) {
                        tx.opening.push_front(open);
                    }
                    return;
                }
                Err(error) => open.fail(Closed::Failed(error)),
            }
        }
    }

    /// This side's half of `stream` is over: no `stopped` to tell.
    fn send_over(&mut self, stream: StreamRef) {
        if let Some(tx) = self.streams.get_mut(&stream) {
            tx.stopped = None;
        }
        self.forget(stream);
    }

    fn forget(&mut self, stream: StreamRef) {
        if self.streams.get(&stream).is_some_and(StreamTx::over) {
            self.streams.remove(&stream);
        }
    }

    /// Hands each of the endpoint's events to the handle it is for.
    fn events(&mut self) {
        while let Some(event) = self.endpoint.next_event() {
            match event {
                Event::SessionRequest { session, request } => {
                    let Some(to_loop) = self.handle() else {
                        _ = self.endpoint.refuse(session, 503);
                        continue;
                    };
                    let request = Request { session, request, to_loop, answered: false };
                    if let Err(unsent) = self.incoming.send(request) {
                        let mut request = unsent.0;
                        request.answered = true;
                        _ = self.endpoint.refuse(session, 503);
                    }
                }
                Event::SessionOpened { session } => {
                    let Some(reply) = self.connects.remove(&session) else { continue };
                    match self.session(session) {
                        Some(handle) => {
                            if let Err(unsent) = reply.send(Ok(handle)) {
                                // The asker went away: the session is not wanted.
                                drop(unsent);
                                _ = self.endpoint.close(session, 0, "");
                            }
                        }
                        None => _ = self.endpoint.close(session, 0, ""),
                    }
                }
                Event::SessionClosed { session, closed } => {
                    if let Some(reply) = self.connects.remove(&session) {
                        _ = reply.send(Err(closed.clone()));
                    }
                    if let Some(tx) = self.sessions.remove(&session) {
                        for open in tx.opening {
                            open.fail(closed.clone());
                        }
                        tx.closed.send_replace(Some(closed));
                    }
                }
                Event::StreamOpened { session, stream } => {
                    let Some(tx) = self.sessions.get(&session) else { continue };
                    let (bi, uni) = (tx.bi.clone(), tx.uni.clone());
                    match self.stream(stream, Opener::Peer) {
                        Some((Some(send), Some(recv))) => _ = bi.send((send, recv)),
                        Some((None, Some(recv))) => _ = uni.send(recv),
                        _ => {}
                    }
                }
                Event::StreamData { stream, bytes } => {
                    let len = bytes.len();
                    let taken = self
                        .streams
                        .get(&stream)
                        .and_then(|it| it.pieces.as_ref())
                        .is_some_and(|it| it.send(Piece::Data(bytes)).is_ok());
                    if !taken {
                        // No reader: its drop stops the stream, and what came meanwhile is let go.
                        self.endpoint.consumed(stream, len);
                    }
                }
                Event::StreamFinished { stream } => self.recv_end(stream, Piece::Fin),
                Event::StreamReset { stream, code } => self.recv_end(stream, Piece::Reset(code)),
                Event::StreamStopped { stream, code } => {
                    if let Some(tx) = self.streams.get_mut(&stream) {
                        if let Some(sent) = tx.sent.take() {
                            _ = sent.send(Err(StreamError::Stopped(code)));
                        }
                        if let Some(stopped) = tx.stopped.take() {
                            stopped.send_replace(Some(code));
                        }
                    }
                    self.forget(stream);
                }
                Event::StreamSent { stream } => {
                    if let Some(sent) = self.streams.get_mut(&stream).and_then(|it| it.sent.take())
                    {
                        _ = sent.send(Ok(()));
                    }
                }
                Event::StreamsAvailable { session } => self.open(session),
                Event::Datagram { session, bytes } => {
                    if let Some(tx) = self.sessions.get(&session) {
                        // Full (the reader is behind) or gone: dropped, as datagrams may be.
                        _ = tx.datagrams.try_send(bytes);
                    }
                }
                // The core answered it; the explorer's peers are browsers, whose errors are
                // theirs to show.
                Event::Refused { .. } => {}
                Event::ConnectionClosed { conn, .. } => {
                    // Every session of it was closed first; what is left of its streams goes.
                    self.streams.retain(|stream, _| stream.conn != conn);
                }
            }
        }
    }

    fn recv_end(&mut self, stream: StreamRef, piece: Piece) {
        if let Some(tx) = self.streams.get_mut(&stream)
            && let Some(pieces) = tx.pieces.take()
        {
            _ = pieces.send(piece);
        }
        self.forget(stream);
    }
}

/// Sleeps until `deadline`, or forever when there is none.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
