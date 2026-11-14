//! The driver of [`Endpoint`] on blaze-core's effects: one UDP socket and the timer, nothing
//! else, and no runtime of its own.
//!
//! [`Loop`] is the driver, a synchronous state machine over the endpoint: it binds its socket
//! ([`Effect::Bind`] with [`Options::QUIC`]: ECN read, the address each datagram arrived at,
//! no fragmentation, received runs coalesced), keeps one batch receive in flight
//! ([`Effect::RecvFrom`] into [`RECV_SLOTS`] slots of a run each) and hands each datagram of
//! what comes to [`Endpoint::received`], moves the endpoint's transmits out as datagram
//! batches ([`Effect::SendTo`]: datagrams to one peer of one length after another are one run,
//! sent with segmentation offload), and arms the endpoint's next deadline as an
//! [`Effect::Timer`] so its driver (the native one, or [`blaze_core::sim::Sim`]) reaches it. It
//! names its effects from a base ([`Loop::new`], [`Loop::owns`]), so a test runs a server's
//! loop and a client's on one simulation.
//!
//! [`Driver`] runs one [`Loop`] on the native driver on a thread of its own, with handles
//! ([`Request`], [`Session`], [`SendStream`], [`RecvStream`]) that post asks to its inbox and
//! wait for its answers on [`crate::mailbox`] channels: futures any executor polls. A
//! [`RecvStream`] says each piece taken ([`Endpoint::consumed`]) as its reader takes the last
//! of it, so a slow reader holds its stream back rather than the driver buffering it; a
//! [`SendStream::write_all`] returns once QUIC took all of its bytes ([`Event::StreamSent`]).
//! Datagrams for a reader that is behind by [`DATAGRAMS_HELD`] are dropped, as the network may
//! drop them. The thread stops when the last [`Driver`] clone is dropped; every handle left
//! then reads the driver's end.
//!
//! [`Wired`] is the same socket work as [`super::Wire`], for a caller that runs its endpoint in
//! a state machine of its own on its own thread (blaze-remote's HTTP/3 bucket).
//!
//! # The floor, and what this costs against it
//!
//! The floor of moving datagrams is one system call per batch the endpoint has ready at once
//! each way. A send here is one `sendmmsg` (Linux: up to [`BATCH_MOST`] runs, each up to
//! [`SEGMENTS_MOST`] datagrams by `UDP_SEGMENT`) or one `sendmsg_x` (macOS's private batch
//! call, one message per datagram), for every transmit the endpoint had queued: a ratio of 1.
//! A receive is one `recvmmsg` (Linux, [`RECV_SLOTS`] runs, each a `UDP_GRO` run of up to 64
//! KiB) or one `recvmsg_x` (macOS, one datagram a slot) per wake; blaze-core's host waits for
//! the socket on a thread of its own (one `poll`) and hands the batch to the driver (its wake
//! and its wait, two calls), so a receive costs about 4 calls against the floor's 2 (the wait
//! and the read): 2x, which a ring-native receive (io_uring's multishot `RECVMSG`, kqueue's
//! `EVFILT_READ` on the driver's own queue) brings to the floor. Datagrams per system call: up
//! to `BATCH_MOST x SEGMENTS_MOST` sent and `RECV_SLOTS` runs received on Linux; one per
//! message, in one call, on macOS. By construction from the effects asked; not measured.
//!
//! # The waits
//!
//! The driver's one wait is its poll, to the endpoint's next deadline (armed as a timer) or the
//! next completion or wake. Every connection ends by its idle timeout, and with it every
//! session and stream, whose handles then fail: no wait of a handle's is open.

use std::{
    alloc::Global,
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_core::{
    self as effects, Dgram, Done, Driver as _, Effect, Tag,
    dgram::{BATCH_MOST, Batch, Ecn, Options, RUN_MOST, SEGMENTS_MOST, Slots},
};

use super::{
    ClientConfig, Closed, Direction, Endpoint, Error, Event, Identity, ServerConfig, SessionId,
    SessionRequest, Step, StreamRef, Transmit, Waited, Wire,
};
use crate::mailbox::{self, Latch, Rx, Setter, Tx};

/// How many runs one receive takes: each slot one run of up to [`RUN_MOST`] bytes, so a
/// receive holds about a mebibyte, enough for a full congestion window's burst at a time.
pub const RECV_SLOTS: usize = 16;
/// The most datagrams held for a session whose reader is behind.
pub const DATAGRAMS_HELD: usize = 256;
/// How many tags one [`Loop`] names its effects with, from its base.
pub const TAGS: u64 = 1 << 40;
/// How long [`Driver::spawn`] waits for its thread to make its driver and bind its socket:
/// both take milliseconds; past this the thread is stuck and the spawn says so.
const START_WAIT: Duration = Duration::from_secs(10);

/// A test-only switch in [`Loop`]: a safeguard removed, which the simulation's oracle must
/// catch.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The `nth` receive's completion (from 1) is dropped: its datagrams never reach the
    /// endpoint, and no receive is asked again.
    DropCompletion { nth: u32 },
    /// The endpoint's deadline is never armed: no timer is asked, the loop's wait has none.
    NoDeadline,
}

/// What an effect in flight was asked for.
#[derive(Debug)]
enum Op {
    Bind,
    Send,
    Recv,
    Timer { due: Duration },
}

/// The driver of one [`Endpoint`], on whichever blaze-core driver its caller polls.
pub struct Loop {
    endpoint: Endpoint,
    base: u64,
    next: u64,
    flying: BTreeMap<u64, Op>,
    /// Where it was asked to bind.
    at: SocketAddr,
    sock: Option<(Dgram, SocketAddr)>,
    effects: Vec<Effect, Global>,
    /// Batches back from their effects, cleared, their capacity kept for the next.
    spare: Vec<Batch, Global>,
    receiving: bool,
    armed: Option<Duration>,
    now: Duration,
    /// Why the socket can no longer be used: its bind failed, or a receive did.
    failed: Option<Error>,
    /// Batches whose send failed, dropped as the network may drop them (QUIC sends again).
    sends_failed: u64,
    #[cfg(test)]
    plant: Plant,
    #[cfg(test)]
    receives: u32,
}

impl Loop {
    /// The driver of `endpoint`, its socket bound at `at`, naming its effects from `base`.
    pub fn new(endpoint: Endpoint, base: u64, at: SocketAddr) -> Loop {
        let mut it = Loop {
            endpoint,
            base,
            next: 0,
            flying: BTreeMap::new_in(Global),
            at,
            sock: None,
            effects: Vec::new_in(Global),
            spare: Vec::new_in(Global),
            receiving: false,
            armed: None,
            now: Duration::ZERO,
            failed: None,
            sends_failed: 0,
            #[cfg(test)]
            plant: Plant::None,
            #[cfg(test)]
            receives: 0,
        };
        let tag = it.tag(Op::Bind);
        it.effects.push(Effect::Bind { at, options: Options::QUIC, tag });
        it
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    pub fn endpoint(&mut self) -> &mut Endpoint {
        &mut self.endpoint
    }

    /// The address its socket is bound at, once it is.
    pub fn local(&self) -> Option<SocketAddr> {
        self.sock.map(|(_, at)| at)
    }

    /// Why its socket can no longer be used, if it cannot.
    pub fn failed(&self) -> Option<&Error> {
        self.failed.as_ref()
    }

    /// How many batches' sends failed and were dropped.
    pub fn sends_failed(&self) -> u64 {
        self.sends_failed
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0 >= self.base && tag.0 - self.base < TAGS
    }

    fn tag(&mut self, op: Op) -> Tag {
        assert!(self.next < TAGS, "blaze-http's HTTP/3 driver named {TAGS} effects from base {}: its tags are spent", self.base);
        let tag = self.base + self.next;
        self.next += 1;
        self.flying.insert(tag, op);
        Tag(tag)
    }

    /// The time is `now` on the driver's clock: the endpoint is told, its transmits go out, a
    /// receive is kept in flight, its deadline is armed, and the effects are moved to `out`.
    pub fn step(&mut self, now: Duration, out: &mut Vec<Effect>) {
        self.now = self.now.max(now);
        self.endpoint.advance_clock(self.now);
        self.transmit();
        if let Some((sock, _)) = self.sock
            && !self.receiving
            && self.failed.is_none()
        {
            let into = self.spare.pop().unwrap_or_else(|| Batch::with_capacity(RECV_SLOTS, RECV_SLOTS * RUN_MOST));
            let slots = Slots { most: RECV_SLOTS as u16, size: RUN_MOST as u32 };
            let tag = self.tag(Op::Recv);
            self.effects.push(Effect::RecvFrom { sock, into, slots, tag });
            self.receiving = true;
        }
        self.arm();
        out.append(&mut self.effects);
    }

    /// The endpoint's transmits, as batches of at most [`BATCH_MOST`] runs: a datagram to the
    /// peer of the run before it, no longer than that run's datagrams, which are all whole,
    /// joins it (segmentation offload), up to [`RUN_MOST`] bytes and [`SEGMENTS_MOST`]
    /// datagrams. Held in the endpoint until the socket is bound.
    fn transmit(&mut self) {
        let Some((sock, _)) = self.sock else { return };
        if self.failed.is_some() {
            return;
        }
        loop {
            let mut batch = self.spare.pop().unwrap_or_default();
            batch.clear();
            let mut full = false;
            while let Some(transmit) = self.endpoint.next_transmit() {
                if pack(&mut batch, transmit.to, &transmit.bytes) {
                    full = true;
                    break;
                }
            }
            if batch.is_empty() {
                self.spare.push(batch);
                return;
            }
            let tag = self.tag(Op::Send);
            self.effects.push(Effect::SendTo { sock, batch, tag });
            if !full {
                return;
            }
        }
    }

    fn arm(&mut self) {
        #[cfg(test)]
        if self.plant == Plant::NoDeadline {
            return;
        }
        let Some(due) = self.endpoint.next_deadline() else { return };
        if self.armed.is_some_and(|armed| armed <= due) {
            return;
        }
        let tag = self.tag(Op::Timer { due });
        self.effects.push(Effect::Timer { after: due.saturating_sub(self.now), tag });
        self.armed = Some(due);
    }
}

/// Puts the datagram `bytes` to `to` in `batch`: joining its last run (segmentation offload)
/// when that run is to the same peer, its datagrams are all whole and no shorter than this
/// one, and the run stays within [`RUN_MOST`] bytes and [`SEGMENTS_MOST`] datagrams; else as a
/// run of its own. Whether the batch is now full ([`BATCH_MOST`] runs). An empty datagram is
/// no packet, and is not put.
fn pack(batch: &mut Batch, to: SocketAddr, bytes: &[u8]) -> bool {
    let len = bytes.len() as u32;
    if len == 0 {
        return false;
    }
    let joins = batch.heads.last().is_some_and(|run| {
        run.peer == to
            && run.len % run.segment == 0
            && len <= run.segment
            && (run.len + len) as usize <= RUN_MOST
            && run.datagrams() < SEGMENTS_MOST
    });
    if joins {
        let run = batch.heads.last_mut().expect("`joins` looked at the last run");
        run.len += len;
        batch.bytes.extend_from_slice(bytes);
    } else {
        // The core says no codepoint: ECN-Capable Transport is not claimed.
        batch.push(to, Ecn::NotEct, bytes);
    }
    batch.heads.len() == BATCH_MOST
}

impl Loop {
    /// Until when a poll of the driver may wait: the armed deadline, or none.
    pub fn deadline(&self) -> effects::wait::Deadline {
        #[cfg(test)]
        if self.plant == Plant::NoDeadline {
            return effects::wait::Deadline::NEVER;
        }
        self.armed.map_or(effects::wait::Deadline::NEVER, effects::wait::Deadline::at)
    }

    /// Takes the completion of one of this loop's effects ([`Loop::owns`]).
    pub fn complete(&mut self, completion: effects::Completion) {
        let effects::Completion { tag, done } = completion;
        let Some(op) = self.flying.remove(&tag.0) else {
            panic!("blaze-http's HTTP/3 driver: {tag:?} names no effect of its in flight")
        };
        #[cfg(test)]
        if matches!(op, Op::Recv) {
            self.receives += 1;
            if self.plant == (Plant::DropCompletion { nth: self.receives }) {
                return;
            }
        }
        match (op, done) {
            (Op::Bind, Done::Bound { sock, local }) => self.sock = Some((sock, local)),
            (Op::Bind, Done::Failed(error)) => {
                let why = format!("binding the UDP socket at {}: {error}", self.at);
                self.failed = Some(Error { peer: None, step: Step::Config, why });
            }
            (Op::Recv, Done::Datagrams(mut batch)) => {
                self.receiving = false;
                for (head, bytes) in batch.each() {
                    // A datagram cut short is not a QUIC packet: it goes, as the network may
                    // drop it.
                    if !head.truncated {
                        self.endpoint.received(head.peer, bytes);
                    }
                }
                batch.clear();
                self.spare.push(batch);
            }
            (Op::Recv, Done::DatagramsFailed { mut batch, error }) => {
                self.receiving = false;
                batch.clear();
                self.spare.push(batch);
                let refused = error.errno.map(std::io::Error::from_raw_os_error).is_some_and(|it| {
                    matches!(it.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset)
                });
                // An ICMP error a peer's datagram caused (macOS reports them on the next read):
                // that peer's connection times out on its own; the next step receives again.
                if !refused {
                    self.failed = Some(Error { peer: None, step: Step::Session, why: format!("the UDP socket failed: {error}") });
                }
            }
            (Op::Send, Done::SentDatagrams(mut batch)) => {
                batch.clear();
                self.spare.push(batch);
            }
            (Op::Send, Done::DatagramsFailed { mut batch, .. }) => {
                self.sends_failed += 1;
                batch.clear();
                self.spare.push(batch);
            }
            (Op::Timer { due }, Done::Fired) => {
                if self.armed == Some(due) {
                    self.armed = None;
                }
            }
            (op, done) => panic!("blaze-http's HTTP/3 driver: {op:?} completed with {done:?}, which blaze-core's effect never gives"),
        }
    }
}

/// What the driver's thread is told by the handles.
enum Message {
    Connect { to: SocketAddr, url: String, headers: Vec<(String, String)>, reply: Tx<Result<Session, Closed>> },
    Accept { session: SessionId, reply: Tx<Result<Session, Closed>> },
    Refuse { session: SessionId, status: u16 },
    Open { session: SessionId, open: Opening },
    Send { stream: StreamRef, bytes: Vec<u8>, reply: Tx<Result<(), StreamError>> },
    Finish { stream: StreamRef },
    Reset { stream: StreamRef, code: u32 },
    Stop { stream: StreamRef, code: u32 },
    Consumed { stream: StreamRef, bytes: usize },
    Datagram { session: SessionId, bytes: Vec<u8>, reply: Tx<Result<(), Error>> },
    Close { session: SessionId, code: u32, reason: String },
    SetIdentity { identity: Identity, reply: Tx<Result<(), Error>> },
    CloseAll { code: u32, reason: String, reply: Tx<()> },
    /// The last [`Driver`] is gone.
    Quit,
}

type Inbox = mailbox::Inbox<Message>;

/// A stream asked for, waiting for the peer to allow one.
enum Opening {
    Bi(Tx<Result<(SendStream, RecvStream), Closed>>),
    Uni(Tx<Result<SendStream, Closed>>),
}

impl Opening {
    fn direction(&self) -> Direction {
        match self {
            Opening::Bi(_) => Direction::Bi,
            Opening::Uni(_) => Direction::Uni,
        }
    }

    fn fail(self, closed: Closed) {
        // An asker that went away wants no answer.
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
    Closed::Failed(Error { peer, step: Step::Session, why: "the HTTP/3 driver stopped".to_owned() })
}

/// An answer the loop gives once: `None` (the loop stopped first) is `gone`.
async fn answer<T>(answer: Rx<T>, gone: impl FnOnce() -> T) -> T {
    answer.recv().await.unwrap_or_else(gone)
}

/// The last clone dropped stops the thread.
struct Owner {
    inbox: Arc<Inbox, Global>,
    local: SocketAddr,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.inbox.post(Message::Quit);
    }
}

/// A handle on a running driver, cheap to clone into any task or thread.
#[derive(Clone)] // clone: a handle on the one thread, once per asker
pub struct Driver {
    owner: Arc<Owner, Global>,
}

/// The sessions peers ask for, as they come.
pub type Requests = Rx<Request>;

impl Driver {
    /// Starts the loop on a thread of its own, its socket bound at `at` (port 0: one the system
    /// picks, [`Driver::local_addr`]), serving with `server`, connecting with `client`, or
    /// both. The receiver gets each session a peer asks for; a request it does not take (the
    /// receiver gone) is refused with 503. A server that takes plain requests
    /// ([`ServerConfig::requests`]) is refused: these handles serve WebTransport only.
    pub fn spawn(at: SocketAddr, server: Option<ServerConfig>, client: Option<ClientConfig>) -> Result<(Driver, Requests), Error> {
        let config = |why: String| Error { peer: None, step: Step::Config, why };
        if server.as_ref().is_some_and(|it| it.requests) {
            return Err(config("the HTTP/3 driver's handles serve WebTransport sessions only: a server that takes plain requests runs its Endpoint in a loop of its own (Loop)".to_owned()));
        }
        let (ready_to, ready) = mailbox::channel::<Result<(Arc<Inbox, Global>, SocketAddr), Error>>(Some(1));
        let (incoming, requests) = mailbox::channel(None);
        std::thread::Builder::new()
            .name("blaze-http h3".to_owned())
            .spawn(move || {
                // No file effect is asked: the root is never read.
                let mut driver = match effects::native(Path::new("/"), 1) {
                    Ok(it) => it,
                    Err(why) => {
                        _ = ready_to.send(Err(config(format!("blaze-core's native driver: {why}"))));
                        return;
                    }
                };
                let Some(waker) = driver.waker() else {
                    _ = ready_to.send(Err(config("blaze-core's native driver gives no waker: a handle could not end its wait".to_owned())));
                    return;
                };
                // quinn-proto's clock is an `Instant`: the epoch is read once, right after the
                // native driver whose clock (`Driver::now`, from its start) it stands for.
                let epoch = Instant::now();
                let endpoint = match Endpoint::new(epoch, server, client) {
                    Ok(it) => it,
                    Err(error) => {
                        _ = ready_to.send(Err(error));
                        return;
                    }
                };
                let mut core = Loop::new(endpoint, 0, at);
                let mut effects = Vec::new_in(Global);
                let mut done = Vec::new_in(Global);
                // Bound before any handle asks: the driver's own bind, bounded by START_WAIT.
                let local = loop {
                    if let Some(local) = core.local() {
                        break local;
                    }
                    if let Some(error) = core.failed() {
                        _ = ready_to.send(Err(error.clone()));
                        return;
                    }
                    if driver.now() > START_WAIT {
                        _ = ready_to.send(Err(config(format!("binding the UDP socket at {at}: not bound in {START_WAIT:?}"))));
                        return;
                    }
                    core.step(driver.now(), &mut effects);
                    driver.submit(&mut effects);
                    driver.poll(&mut done, effects::wait::Deadline::after(driver.now(), START_WAIT));
                    for completion in done.drain(..) {
                        core.complete(completion);
                    }
                };
                let inbox = Arc::new_in(Inbox::new(waker), Global);
                if ready_to.send(Ok((Arc::clone(&inbox), local))).is_err() {
                    return;
                }
                drop(ready_to);
                let routes = Routes::new(Arc::clone(&inbox), incoming);
                serve(&mut driver, core, &inbox, routes);
            })
            .map_err(|why| config(format!("starting the HTTP/3 driver's thread: {why}")))?;
        let started = mailbox::block_on("the HTTP/3 driver's thread to bind its socket", Some(START_WAIT), ready.recv());
        let (inbox, local) = match started {
            Ok(Some(started)) => started?,
            Ok(None) => return Err(config("the HTTP/3 driver's thread ended before it bound its socket".to_owned())),
            Err(blocked) => return Err(config(blocked.to_string())),
        };
        Ok((Driver { owner: Arc::new_in(Owner { inbox, local }, Global) }, requests))
    }

    fn post(&self, message: Message) {
        self.owner.inbox.post(message);
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.owner.local
    }

    /// Opens a session to `to` at `url` (`https://host:port/path`), sending `headers`
    /// (lowercase names, no pseudo-fields) with its CONNECT.
    pub async fn connect(&self, to: SocketAddr, url: &str, headers: &[(&str, &str)]) -> Result<Session, Closed> {
        let (reply, answered) = mailbox::channel(Some(1));
        let headers = headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect();
        self.post(Message::Connect { to, url: url.to_owned(), headers, reply });
        answer(answered, || Err(driver_gone(Some(to)))).await
    }

    /// Serves `identity` to connections from now on; open ones keep theirs.
    pub async fn set_identity(&self, identity: Identity) -> Result<(), Error> {
        let (reply, answered) = mailbox::channel(Some(1));
        self.post(Message::SetIdentity { identity, reply });
        answer(answered, || Err(Error { peer: None, step: Step::Config, why: "the HTTP/3 driver stopped".to_owned() })).await
    }

    /// Closes every session with `code` and `reason`, then every connection; returns once each
    /// connection's close is sent (at most [`super::LINGER`] after the sessions' closes, when
    /// a peer does not acknowledge them).
    pub async fn close(&self, code: u32, reason: &str) {
        let (reply, answered) = mailbox::channel(Some(1));
        self.post(Message::CloseAll { code, reason: reason.to_owned(), reply });
        _ = answered.recv().await;
    }
}

/// A session a peer asked for: [`Request::accept`] or [`Request::refuse`] it. Dropped
/// unanswered, it is refused with 500.
pub struct Request {
    session: SessionId,
    request: SessionRequest,
    inbox: Arc<Inbox, Global>,
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
        let (reply, answered) = mailbox::channel(Some(1));
        self.inbox.post(Message::Accept { session: self.session, reply });
        answer(answered, || Err(driver_gone(Some(peer)))).await
    }

    /// Refuses the session with `status` (300 to 599).
    pub fn refuse(mut self, status: u16) {
        self.answered = true;
        self.inbox.post(Message::Refuse { session: self.session, status });
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if !self.answered {
            self.inbox.post(Message::Refuse { session: self.session, status: 500 });
        }
    }
}

/// An open session, cheap to clone; the last clone dropped closes it with code 0.
#[derive(Clone)] // clone: a handle on one session, once per task reading it
pub struct Session {
    inner: Arc<SessionInner, Global>,
}

struct SessionInner {
    id: SessionId,
    peer: SocketAddr,
    inbox: Arc<Inbox, Global>,
    bi: Rx<(SendStream, RecvStream)>,
    uni: Rx<RecvStream>,
    datagrams: Rx<Vec<u8>>,
    closed: Latch<Closed>,
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
        self.inner.closed.wait().await.unwrap_or_else(|| driver_gone(Some(self.inner.peer)))
    }

    /// The next bidirectional stream the peer opens.
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream), Closed> {
        match self.inner.bi.recv().await {
            Some(it) => Ok(it),
            None => Err(self.closed().await),
        }
    }

    /// The next unidirectional stream the peer opens.
    pub async fn accept_uni(&self) -> Result<RecvStream, Closed> {
        match self.inner.uni.recv().await {
            Some(it) => Ok(it),
            None => Err(self.closed().await),
        }
    }

    /// The next datagram.
    pub async fn receive_datagram(&self) -> Result<Vec<u8>, Closed> {
        match self.inner.datagrams.recv().await {
            Some(it) => Ok(it),
            None => Err(self.closed().await),
        }
    }

    /// Opens a bidirectional stream, waiting while the peer allows no more.
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream), Closed> {
        let (reply, answered) = mailbox::channel(Some(1));
        self.inner.inbox.post(Message::Open { session: self.inner.id, open: Opening::Bi(reply) });
        answer(answered, || Err(driver_gone(Some(self.inner.peer)))).await
    }

    /// Opens a unidirectional stream, waiting while the peer allows no more.
    pub async fn open_uni(&self) -> Result<SendStream, Closed> {
        let (reply, answered) = mailbox::channel(Some(1));
        self.inner.inbox.post(Message::Open { session: self.inner.id, open: Opening::Uni(reply) });
        answer(answered, || Err(driver_gone(Some(self.inner.peer)))).await
    }

    /// Sends a datagram; it may be lost, as datagrams may.
    pub async fn send_datagram(&self, bytes: &[u8]) -> Result<(), Error> {
        let (reply, answered) = mailbox::channel(Some(1));
        self.inner.inbox.post(Message::Datagram { session: self.inner.id, bytes: bytes.to_vec(), reply });
        let peer = self.inner.peer;
        answer(answered, || Err(Error { peer: Some(peer), step: Step::Datagram, why: "the HTTP/3 driver stopped".to_owned() })).await
    }

    /// Closes the session with `code` and `reason`.
    pub fn close(&self, code: u32, reason: &str) {
        self.inner.inbox.post(Message::Close { session: self.inner.id, code, reason: reason.to_owned() });
    }
}

impl Drop for SessionInner {
    /// The last handle gone closes the session, as nothing could read it any more.
    fn drop(&mut self) {
        self.inbox.post(Message::Close { session: self.id, code: 0, reason: String::new() });
    }
}

/// This side's half of a stream. Dropped, it is finished.
pub struct SendStream {
    stream: StreamRef,
    inbox: Arc<Inbox, Global>,
    stopped: Latch<Option<u32>>,
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
        let (reply, answered) = mailbox::channel(Some(1));
        self.inbox.post(Message::Send { stream: self.stream, bytes: bytes.to_vec(), reply });
        answer(answered, || Err(StreamError::Gone)).await
    }

    /// Finishes the stream once what was written is sent.
    pub fn finish(&mut self) {
        if !std::mem::replace(&mut self.done, true) {
            self.inbox.post(Message::Finish { stream: self.stream });
        }
    }

    /// Resets the stream with `code`, dropping what was not sent.
    pub fn reset(&mut self, code: u32) {
        if !std::mem::replace(&mut self.done, true) {
            self.inbox.post(Message::Reset { stream: self.stream, code });
        }
    }

    /// Waits for the peer to stop the stream: its WebTransport code, if it was one. Never
    /// returns while the stream lives and is not stopped; `None` too when it is gone.
    pub async fn stopped(&mut self) -> Option<u32> {
        self.stopped.wait().await.flatten()
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
    inbox: Arc<Inbox, Global>,
    pieces: Rx<Piece>,
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
        // Each pass either hands out bytes, or takes the next piece; a piece that is data has
        // bytes, so the second pass hands them out.
        loop {
            if self.at < self.piece.len() {
                let n = out.len().min(self.piece.len() - self.at);
                out[..n].copy_from_slice(&self.piece[self.at..self.at + n]);
                self.at += n;
                if self.at == self.piece.len() {
                    let bytes = self.piece.len();
                    self.piece.clear();
                    self.at = 0;
                    self.inbox.post(Message::Consumed { stream: self.stream, bytes });
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
            self.inbox.post(Message::Stop { stream: self.stream, code });
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
    bi: Tx<(SendStream, RecvStream)>,
    uni: Tx<RecvStream>,
    datagrams: Tx<Vec<u8>>,
    closed: Setter<Closed>,
    opening: VecDeque<Opening, Global>,
}

/// The loop's side of a stream.
struct StreamTx {
    pieces: Option<Tx<Piece>>,
    stopped: Option<Setter<Option<u32>>>,
    sent: Option<Tx<Result<(), StreamError>>>,
}

impl StreamTx {
    fn over(&self) -> bool {
        self.pieces.is_none() && self.stopped.is_none()
    }
}

/// Where the endpoint's events go: the handles' channels.
struct Routes {
    inbox: Arc<Inbox, Global>,
    incoming: Tx<Request>,
    connects: BTreeMap<SessionId, Tx<Result<Session, Closed>>, Global>,
    sessions: BTreeMap<SessionId, SessionTx, Global>,
    streams: BTreeMap<StreamRef, StreamTx, Global>,
    closing: Vec<Tx<()>, Global>,
}

/// The thread's loop: the handles' asks in, the endpoint's events to the handles, the loop's
/// effects out, until the last [`Driver`] is gone or the socket fails.
fn serve(driver: &mut impl effects::Driver, mut core: Loop, inbox: &Inbox, mut routes: Routes) {
    let mut messages = Vec::new_in(Global);
    let mut effects = Vec::new_in(Global);
    let mut done = Vec::new_in(Global);
    loop {
        if let Some(error) = core.failed() {
            let error = error.clone();
            routes.fail_all(&error);
            return;
        }
        let now = driver.now();
        core.endpoint().advance_clock(now);
        inbox.take(&mut messages);
        for message in messages.drain(..) {
            if matches!(message, Message::Quit) {
                return;
            }
            routes.ask(core.endpoint(), message);
        }
        routes.events(core.endpoint());
        core.step(now, &mut effects);
        if !routes.closing.is_empty() && core.endpoint().all_closed() {
            for closed in routes.closing.drain(..) {
                _ = closed.send(());
            }
        }
        driver.submit(&mut effects);
        driver.poll(&mut done, core.deadline());
        // What came is received at the time it came, not at the time the poll began.
        core.endpoint().advance_clock(driver.now());
        for completion in done.drain(..) {
            core.complete(completion);
        }
    }
}

impl Routes {
    fn new(inbox: Arc<Inbox, Global>, incoming: Tx<Request>) -> Routes {
        Routes {
            inbox,
            incoming,
            connects: BTreeMap::new_in(Global),
            sessions: BTreeMap::new_in(Global),
            streams: BTreeMap::new_in(Global),
            closing: Vec::new_in(Global),
        }
    }

    /// Ends every session and handle with `error`, as the loop stops.
    fn fail_all(&mut self, error: &Error) {
        for (_, reply) in std::mem::take(&mut self.connects) {
            _ = reply.send(Err(Closed::Failed(error.clone())));
        }
        for (_, session) in std::mem::take(&mut self.sessions) {
            session.closed.set(Closed::Failed(error.clone()));
            for open in session.opening {
                open.fail(Closed::Failed(error.clone()));
            }
        }
        self.streams.clear();
    }

    fn ask(&mut self, endpoint: &mut Endpoint, message: Message) {
        match message {
            Message::Connect { to, url, headers, reply } => {
                let fields: Vec<(&str, &str), Global> = {
                    let mut it = Vec::with_capacity_in(headers.len(), Global);
                    it.extend(headers.iter().map(|(n, v)| (n.as_str(), v.as_str())));
                    it
                };
                match endpoint.connect(to, &url, &fields) {
                    Ok(session) => {
                        self.connects.insert(session, reply);
                    }
                    Err(error) => _ = reply.send(Err(Closed::Failed(error))),
                }
            }
            Message::Accept { session, reply } => match endpoint.accept(session) {
                Ok(()) => match self.session(endpoint, session) {
                    Some(handle) => _ = reply.send(Ok(handle)),
                    None => _ = reply.send(Err(gone_session(endpoint, session))),
                },
                Err(error) => _ = reply.send(Err(Closed::Failed(error))),
            },
            Message::Refuse { session, status } => {
                // A session already gone (its connection closed) needs no answer.
                _ = endpoint.refuse(session, status);
            }
            Message::Open { session, open } => match self.sessions.get_mut(&session) {
                Some(tx) if tx.opening.is_empty() => {
                    tx.opening.push_back(open);
                    self.open(endpoint, session);
                }
                Some(tx) => tx.opening.push_back(open),
                None => open.fail(gone_session(endpoint, session)),
            },
            Message::Send { stream, bytes, reply } => match endpoint.send(stream, &bytes) {
                Ok(()) => match self.streams.get_mut(&stream) {
                    Some(tx) => tx.sent = Some(reply),
                    None => _ = reply.send(Err(StreamError::Gone)),
                },
                Err(error) => _ = reply.send(Err(StreamError::Refused(error))),
            },
            Message::Finish { stream } => {
                // A stream already reset, stopped or gone has nothing to finish.
                _ = endpoint.finish(stream);
                self.send_over(stream);
            }
            Message::Reset { stream, code } => {
                _ = endpoint.reset(stream, code);
                self.send_over(stream);
            }
            Message::Stop { stream, code } => {
                _ = endpoint.stop(stream, code);
                if let Some(tx) = self.streams.get_mut(&stream) {
                    tx.pieces = None;
                }
                self.forget(stream);
            }
            Message::Consumed { stream, bytes } => endpoint.consumed(stream, bytes),
            Message::Datagram { session, bytes, reply } => _ = reply.send(endpoint.send_datagram(session, &bytes)),
            Message::Close { session, code, reason } => {
                // A session already gone needs no close.
                _ = endpoint.close(session, code, &reason);
            }
            Message::SetIdentity { identity, reply } => _ = reply.send(endpoint.set_identity(identity)),
            Message::CloseAll { code, reason, reply } => {
                endpoint.close_all(code, &reason);
                self.closing.push(reply);
            }
            Message::Quit => {}
        }
    }

    /// The handle of a session that just opened, and the loop's side of it; `None` when its
    /// connection is gone.
    fn session(&mut self, endpoint: &Endpoint, id: SessionId) -> Option<Session> {
        let peer = endpoint.peer(id.conn)?;
        let (bi, bi_rx) = mailbox::channel(None);
        let (uni, uni_rx) = mailbox::channel(None);
        let (datagrams, datagrams_rx) = mailbox::channel(Some(DATAGRAMS_HELD));
        let (closed, closed_rx) = mailbox::latch();
        self.sessions.insert(id, SessionTx { bi, uni, datagrams, closed, opening: VecDeque::new_in(Global) });
        let inner = SessionInner { id, peer, inbox: Arc::clone(&self.inbox), bi: bi_rx, uni: uni_rx, datagrams: datagrams_rx, closed: closed_rx };
        Some(Session { inner: Arc::new_in(inner, Global) })
    }

    /// The handles of a stream, and the loop's side of it: a [`SendStream`] unless it is the
    /// peer's unidirectional one, a [`RecvStream`] unless it is this side's.
    fn stream(&mut self, stream: StreamRef, opener: Opener) -> (Option<SendStream>, Option<RecvStream>) {
        let (sends, receives) = match (stream.direction(), opener) {
            (Direction::Bi, _) => (true, true),
            (Direction::Uni, Opener::Here) => (true, false),
            (Direction::Uni, Opener::Peer) => (false, true),
        };
        let mut tx = StreamTx { pieces: None, stopped: None, sent: None };
        let send = sends.then(|| {
            let (stopped, stopped_rx) = mailbox::latch();
            tx.stopped = Some(stopped);
            SendStream { stream, inbox: Arc::clone(&self.inbox), stopped: stopped_rx, done: false }
        });
        let recv = receives.then(|| {
            let (pieces, pieces_rx) = mailbox::channel(None);
            tx.pieces = Some(pieces);
            RecvStream { stream, inbox: Arc::clone(&self.inbox), pieces: pieces_rx, piece: Vec::new_in(Global), at: 0, ended: false }
        });
        self.streams.insert(stream, tx);
        (send, recv)
    }

    /// Opens the streams asked for in `session`, in order, as far as the peer allows.
    fn open(&mut self, endpoint: &mut Endpoint, session: SessionId) {
        // Each pass opens one stream or stops: at most as many passes as asks queued.
        loop {
            let Some(tx) = self.sessions.get_mut(&session) else { return };
            let Some(open) = tx.opening.pop_front() else { return };
            match endpoint.open(session, open.direction()) {
                Ok(Some(stream)) => match (open, self.stream(stream, Opener::Here)) {
                    (Opening::Bi(reply), (Some(send), Some(recv))) => _ = reply.send(Ok((send, recv))),
                    (Opening::Uni(reply), (Some(send), _)) => _ = reply.send(Ok(send)),
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
    fn events(&mut self, endpoint: &mut Endpoint) {
        while let Some(event) = endpoint.next_event() {
            match event {
                Event::SessionRequest { session, request } => {
                    let request = Request { session, request, inbox: Arc::clone(&self.inbox), answered: false };
                    if let Err(mut unsent) = self.incoming.send(request) {
                        unsent.answered = true;
                        _ = endpoint.refuse(session, 503);
                    }
                }
                Event::SessionOpened { session } => {
                    let Some(reply) = self.connects.remove(&session) else { continue };
                    match self.session(endpoint, session) {
                        Some(handle) => {
                            if reply.send(Ok(handle)).is_err() {
                                // The asker went away: the session is not wanted (the handle
                                // handed back closes it as it drops).
                            }
                        }
                        None => _ = endpoint.close(session, 0, ""),
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
                        tx.closed.set(closed);
                    }
                }
                Event::StreamOpened { session, stream } => {
                    if !self.sessions.contains_key(&session) {
                        continue;
                    }
                    let handles = self.stream(stream, Opener::Peer);
                    let Some(tx) = self.sessions.get(&session) else { continue };
                    match handles {
                        (Some(send), Some(recv)) => _ = tx.bi.send((send, recv)),
                        (None, Some(recv)) => _ = tx.uni.send(recv),
                        _ => {}
                    }
                }
                Event::StreamData { stream, bytes } => {
                    let len = bytes.len();
                    let taken = self.streams.get(&stream).and_then(|it| it.pieces.as_ref()).is_some_and(|it| it.send(Piece::Data(bytes)).is_ok());
                    if !taken {
                        // No reader: its drop stops the stream, and what came meanwhile is let go.
                        endpoint.consumed(stream, len);
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
                            stopped.set(code);
                        }
                    }
                    self.forget(stream);
                }
                Event::StreamSent { stream } => {
                    if let Some(sent) = self.streams.get_mut(&stream).and_then(|it| it.sent.take()) {
                        _ = sent.send(Ok(()));
                    }
                }
                // A handle's `finish` promises the FIN handed to QUIC, not its acknowledgement:
                // no handle waits on it.
                Event::StreamAcked { .. } => {}
                Event::StreamsAvailable { session } => self.open(endpoint, session),
                Event::Datagram { session, bytes } => {
                    if let Some(tx) = self.sessions.get(&session) {
                        // Full (the reader is behind) or gone: dropped, as datagrams may be.
                        _ = tx.datagrams.send(bytes);
                    }
                }
                // The core answered it; the explorer's peers are browsers, whose errors are
                // theirs to show.
                Event::Refused { .. } => {}
                Event::ConnectionClosed { conn, .. } => {
                    // Every session of it was closed first; what is left of its streams goes.
                    self.streams.retain(|stream, _| stream.conn != conn);
                }
                // The handles keep no tickets: a process that resumes sessions runs its
                // Endpoint in a loop of its own and keeps them (blaze-remote's HTTP/3 reads).
                Event::Ticket { .. } => {}
                event @ (Event::Request { .. } | Event::ResponseHead { .. } | Event::ResponseData { .. } | Event::RequestEnded { .. }) => {
                    panic!("blaze-http's HTTP/3 driver: {event:?} of a plain request, which its handles never ask and its servers never take (Driver::spawn refuses one that would)")
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

fn gone_session(endpoint: &Endpoint, session: SessionId) -> Closed {
    let why = format!("session {} of connection {} is not open", session.stream, session.conn.0);
    Closed::Failed(Error { peer: endpoint.peer(session.conn), step: Step::Session, why })
}

/// What an effect of a [`Wired`] in flight was asked for.
#[derive(Debug)]
enum WireOp {
    Send,
    Recv,
    Timer { due: Duration },
}

/// [`Wire`] on blaze-core's driver, for a caller that runs an [`Endpoint`] in its own state
/// machine on its own thread (blaze-remote's HTTP/3 bucket): one UDP socket, bound with
/// [`Options::QUIC`]. Sends are gathered into one batch ([`pack`], segmentation offload) and
/// go out as the wait begins, or as the batch fills; the wait keeps one batch receive in
/// flight and arms a timer at its bound, so it ends at the first datagrams or the bound, on
/// the native driver and the simulation alike. Its floor and cost are [`Loop`]'s, less the
/// handles' thread hops: one `sendmmsg`/`sendmsg_x` per wait, one receive per wake.
pub struct Wired<D: effects::Driver> {
    driver: D,
    epoch: Instant,
    sock: Dgram,
    local: SocketAddr,
    out: Batch,
    spare: Vec<Batch, Global>,
    next: u64,
    flying: BTreeMap<u64, WireOp, Global>,
    receiving: bool,
    armed: Option<Duration>,
    done: Vec<effects::Completion, Global>,
    effects: Vec<Effect, Global>,
}

impl Wired<effects::Native> {
    /// A wire on the native driver, its socket bound at `at` (port 0: one the system picks,
    /// [`Wired::local`]).
    pub fn bind(at: SocketAddr) -> Result<Wired<effects::Native>, Error> {
        let config = |why: String| Error { peer: None, step: Step::Config, why };
        // No file effect is asked: the root is never read.
        let driver = effects::native(Path::new("/"), 1).map_err(|why| config(format!("blaze-core's native driver: {why}")))?;
        // quinn-proto's clock is an `Instant`: read once, right after the driver whose clock
        // (`Driver::now`, from its start) it stands for.
        let epoch = Instant::now();
        Wired::on(driver, epoch, at)
    }
}

impl<D: effects::Driver> Wired<D> {
    /// A wire on `driver`, whose clock started at `epoch`, its socket bound at `at`; waits at
    /// most [`START_WAIT`] for the bind.
    pub fn on(mut driver: D, epoch: Instant, at: SocketAddr) -> Result<Wired<D>, Error> {
        let config = |why: String| Error { peer: None, step: Step::Config, why };
        let mut effects = Vec::new_in(Global);
        effects.push(Effect::Bind { at, options: Options::QUIC, tag: Tag(0) });
        let until = effects::wait::Deadline::after(driver.now(), START_WAIT);
        effects.push(Effect::Timer { after: START_WAIT, tag: Tag(1) });
        driver.submit(&mut effects);
        let mut done = Vec::new_in(Global);
        // Bounded: each poll waits at most to `until`, and the timer armed at it ends the
        // last; past it the bind is said stuck.
        let (sock, local) = loop {
            driver.poll(&mut done, until);
            if let Some(at_bind) = done.iter().position(|it| it.tag == Tag(0)) {
                match done.swap_remove(at_bind).done {
                    Done::Bound { sock, local } => break (sock, local),
                    Done::Failed(why) => return Err(config(format!("binding the UDP socket at {at}: {why}"))),
                    other => panic!("blaze-http's HTTP/3 wire: a bind completed with {other:?}, which blaze-core's effect never gives"),
                }
            }
            if until.passed(driver.now()) {
                return Err(config(format!("binding the UDP socket at {at}: not bound in {START_WAIT:?}")));
            }
        };
        let mut flying = BTreeMap::new_in(Global);
        // The bind's timer is still in flight unless it fired: it completes in a later wait.
        if !done.iter().any(|it| it.tag == Tag(1)) {
            flying.insert(1, WireOp::Timer { due: START_WAIT });
        }
        done.clear();
        Ok(Wired {
            driver,
            epoch,
            sock,
            local,
            out: Batch::new(),
            spare: Vec::new_in(Global),
            next: 2,
            flying,
            receiving: false,
            armed: None,
            done,
            effects,
        })
    }

    /// The address its socket is bound at.
    pub fn local(&self) -> SocketAddr {
        self.local
    }

    fn tag(&mut self, op: WireOp) -> Tag {
        let tag = self.next;
        self.next += 1;
        self.flying.insert(tag, op);
        Tag(tag)
    }

    /// The batch gathered so far, sent.
    fn flush(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let next = self.spare.pop().unwrap_or_default();
        let batch = std::mem::replace(&mut self.out, next);
        let tag = self.tag(WireOp::Send);
        self.effects.push(Effect::SendTo { sock: self.sock, batch, tag });
    }

    fn recycle(&mut self, mut batch: Batch) {
        batch.clear();
        self.spare.push(batch);
    }
}

impl<D: effects::Driver> Wire for Wired<D> {
    fn epoch(&self) -> Instant {
        self.epoch
    }

    fn now(&mut self) -> Duration {
        self.driver.now()
    }

    fn send(&mut self, transmit: Transmit) {
        if pack(&mut self.out, transmit.to, &transmit.bytes) {
            self.flush();
            self.driver.submit(&mut self.effects);
        }
    }

    fn wait(&mut self, until: Duration, received: &mut dyn FnMut(SocketAddr, &[u8])) -> Waited {
        self.flush();
        // Bounded: each pass polls to `until` at most, and the timer armed at it ends that
        // poll; the loop ends at the first datagrams, or once `until` has passed.
        loop {
            if !self.receiving {
                let into = self.spare.pop().unwrap_or_else(|| Batch::with_capacity(RECV_SLOTS, RECV_SLOTS * RUN_MOST));
                let slots = Slots { most: RECV_SLOTS as u16, size: RUN_MOST as u32 };
                let tag = self.tag(WireOp::Recv);
                self.effects.push(Effect::RecvFrom { sock: self.sock, into, slots, tag });
                self.receiving = true;
            }
            if self.armed.is_none_or(|armed| armed > until) && until != Duration::MAX {
                let tag = self.tag(WireOp::Timer { due: until });
                self.effects.push(Effect::Timer { after: until.saturating_sub(self.driver.now()), tag });
                self.armed = Some(until);
            }
            self.driver.submit(&mut self.effects);
            self.driver.poll(&mut self.done, effects::wait::Deadline::at(until));
            let mut came = false;
            let mut done = std::mem::take(&mut self.done);
            for completion in done.drain(..) {
                let Some(op) = self.flying.remove(&completion.tag.0) else {
                    panic!("blaze-http's HTTP/3 wire: {:?} names no effect of its in flight", completion.tag)
                };
                match (op, completion.done) {
                    (WireOp::Recv, Done::Datagrams(batch)) => {
                        self.receiving = false;
                        for (head, bytes) in batch.each() {
                            // A datagram cut short is not a QUIC packet: it goes, as the
                            // network may drop it.
                            if !head.truncated {
                                received(head.peer, bytes);
                                came = true;
                            }
                        }
                        self.recycle(batch);
                    }
                    (WireOp::Recv, Done::DatagramsFailed { batch, error }) => {
                        self.receiving = false;
                        self.recycle(batch);
                        let refused = error.errno.map(std::io::Error::from_raw_os_error).is_some_and(|it| {
                            matches!(it.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset)
                        });
                        // An ICMP error a peer's datagram caused: that peer's connection times
                        // out on its own, and the next pass receives again. Any other is the
                        // socket's, which `Wire` has no error to say with: loud, never quiet.
                        if !refused {
                            panic!("blaze-http's HTTP/3 wire: the UDP socket at {} failed: {error}", self.local);
                        }
                    }
                    // A send that failed is dropped, as the network may drop it: QUIC sends again.
                    (WireOp::Send, Done::SentDatagrams(batch) | Done::DatagramsFailed { batch, .. }) => self.recycle(batch),
                    (WireOp::Timer { due }, Done::Fired) => {
                        if self.armed == Some(due) {
                            self.armed = None;
                        }
                    }
                    (op, done) => panic!("blaze-http's HTTP/3 wire: {op:?} completed with {done:?}, which blaze-core's effect never gives"),
                }
            }
            self.done = done;
            if came {
                return Waited::Received;
            }
            if self.driver.now() >= until {
                return Waited::Deadline;
            }
        }
    }
}

#[cfg(test)]
mod tests;
