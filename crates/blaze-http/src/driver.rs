//! The driver of [`Client`] on blaze-core's effects: sockets, names and timers, nothing else,
//! and no runtime of its own.
//!
//! [`Loop`] is the driver, a synchronous state machine over the client: it turns the client's
//! [`Io`] into blaze-core [`Effect`]s (a connect is [`Effect::Resolve`], then
//! [`Effect::Connect`] to each address the name gave in turn; a send [`Effect::Send`]; a
//! receive [`Effect::Recv`] of at most [`RECV_MOST`]; a close [`Effect::Hangup`]), turns their
//! completions back into the client's [`Completion`]s, and arms the client's next deadline as
//! an [`Effect::Timer`], so a driver (the native one, or [`blaze_core::sim::Sim`]) reaches it
//! on its own clock. It names its effects with tags from a base the caller gives
//! ([`Loop::new`], [`Loop::owns`]), so one driver can carry it beside other machines: a test's
//! scripted server on the same simulation, or the daemon's loop. It holds at most `most`
//! requests in the client at once and queues the rest in order, and ends a request whose asker
//! gave it a deadline of its own ([`Told::TimedOut`]).
//!
//! [`Handle`] runs one [`Loop`] on blaze-core's native driver on a thread of its own, for
//! callers that have no loop: askers on any thread post to its inbox and wake its driver
//! ([`blaze_core::Waker`]); [`serve`] is that thread's loop, written once as a plain blocking
//! loop: drain the inbox, step the machine, submit its effects, wait on the driver to the
//! machine's deadline, hand the completions back. An asker reads its [`Answer`] with
//! [`Answer::next`], one timed receive on blaze-core's channel ([`crate::mailbox::answer`]):
//! no future, waker or executor is anywhere in it. Taking the next event says the last piece
//! of the body was taken ([`Client::consumed`]), so a slow asker holds the transfer back
//! rather than the driver buffering it; dropping an answer before its end cancels the request
//! ([`Client::cancel`]), closing its connection.
//!
//! TLS stays the client's: the driver moves bytes.
//!
//! # The floor, and what this costs against it
//!
//! A small GET on a warm (kept-alive, idle) connection needs, at the floor, one system call
//! that sends the request and one that receives the answer. Driven by [`Loop`] on a caller's
//! own driver, this costs the floor plus the driver's wait: the client keeps a receive asked of
//! every idle connection, so the GET asks one [`Effect::Send`], and the answer completes the
//! receive already in flight. On io_uring the send and the wait for both completions are one
//! `io_uring_enter` (a ratio of 1 against the floor of two operations, both on the ring); on
//! kqueue the send is one `send`, the answer one `kevent` and one `recv` (3 against 2). Through
//! [`Handle`] each ask also costs the hand-off to the driver's thread, its wake (one `kevent`
//! trigger, or one ring message) and the asker's park and unpark (one futex wait and wake),
//! about 2 to 3 calls more: 2.5x the floor on kqueue, so a caller that makes many small
//! requests drives [`Loop`] on its own loop. A deadline re-armed earlier than the one in flight
//! costs one more timer (`EVFILT_TIMER`, or a ring timeout). By construction from the effects
//! asked; not measured.
//!
//! # The waits
//!
//! The driver's one wait is [`Driver::poll`], to the client's next deadline (armed as a timer)
//! or the next completion or wake. An asker's wait on a [`Handle`] is a receive on blaze-core's
//! channel that takes how long it may wait and ends in [`Unanswered::TimedOut`] naming what
//! it waited for; every request also ends by the client's deadlines (its timeout).

use std::{
    alloc::Global,
    collections::{BTreeMap, VecDeque},
    fmt::Write as _,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use blaze_core::{
    self as effects, Address, Conn, Dgram, Done, Driver, Effect, Tag,
    chan::{self, Receiver, Sender},
    dgram::{Batch, Ecn, Options as DgramOptions, Slots},
    sockopt::{Ask as SockAsk, UdpOptions},
};
use blaze_own::{Alloc, Heap, SortedMap, Vec as OwnVec};

use crate::{
    Client, Completion, ConnId, Error, Event, Io, Kind, Request, RequestId,
    mailbox::{self, Unanswered},
};

/// What [`Answer::next`] says it waits for.
pub const NEXT_EVENT: &str = "the next event of an HTTP request";

/// The most bytes one receive reads: enough for a TLS record's worth of an answer many times
/// over, and the bound of the buffer each receive holds.
pub const RECV_MOST: usize = 64 << 10;

/// How many tags one [`Loop`] names its effects with, from its base: far more than one process
/// asks in its life, so a loop never runs out, and few enough that many loops share a driver.
pub const TAGS: u64 = 1 << 40;

/// How many requests a [`Handle`] made by [`Handle::spawn`] holds in the client at once, each
/// on a connection of its own (HTTP/1), when its caller says no other bound.
pub const IN_FLIGHT_MOST: usize = 64;

/// How long [`Handle::spawn`] waits for its thread to make its driver: the native driver is
/// made in a few milliseconds; past this the thread is stuck and the spawn says so.
const START_WAIT: Duration = Duration::from_secs(10);

/// A test-only switch in [`Loop`]: a safeguard removed, which the simulation's oracle must
/// catch (`the_oracle_catches_*` in the tests).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The `nth` receive's completion (from 1) is dropped, never handed to the client.
    DropCompletion { nth: u64 },
    /// The client's deadline is never armed: no timer is asked, and the loop's wait has none.
    NoDeadline,
    /// The order before the fix of `agents/annoyances/uring-waker-lost-without-offloaded-work.json`:
    /// [`Loop::ask`] starts the request in the client at once, on the client's clock as the
    /// last step left it, and [`serve`] does not advance the loop as each wait ends.
    StaleClock,
}

/// What an effect in flight was asked for.
#[derive(Debug)]
enum Op {
    /// The name of `conn`'s connect (`connect`), or of a proxy's [`Io::Resolve`].
    Resolve { conn: ConnId, connect: bool },
    /// `conn`'s connect to `at`, then to each of `rest` while they fail; `tried` says how each
    /// tried before failed.
    Connect { conn: ConnId, at: SocketAddr, rest: VecDeque<SocketAddr, Global>, tried: String },
    Send { conn: ConnId },
    Recv { conn: ConnId },
    /// The socket of a connection let go of.
    Hangup,
    /// The client's deadline at `due`.
    Timer { due: Duration },
    /// An asker's sleep ([`Loop::sleep`]).
    Sleep { key: u64 },
}

/// A connection the client asked for, as the driver holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Being resolved or connected.
    Opening,
    Open(Conn),
}

/// What a [`Loop`] tells its caller, in order.
#[derive(Debug)]
pub enum Told {
    /// One of a request's events, from the client.
    Event(Event),
    /// A request's asker's own deadline ([`Loop::ask`]'s `until`) passed first: it is
    /// cancelled, and tells nothing more.
    TimedOut(RequestId),
    /// An asker's sleep ([`Loop::sleep`]) is over.
    Woke(u64),
}

/// The driver of one [`Client`], on whichever blaze-core [`Driver`] its caller polls.
pub struct Loop {
    client: Client,
    /// Its tags are `base..base + TAGS`.
    base: u64,
    /// The next tag, from `base`.
    next: u64,
    flying: BTreeMap<u64, Op>,
    conns: BTreeMap<ConnId, Slot>,
    effects: Vec<Effect, Global>,
    told: VecDeque<Told, Global>,
    /// At most this many requests in the client at once.
    most: usize,
    /// The requests in the client, each with its asker's deadline, if it gave one.
    running: BTreeMap<RequestId, Option<Duration>>,
    /// The requests past `most`, in the order asked.
    queued: VecDeque<(RequestId, Request, Option<Duration>), Global>,
    /// The deadline of the timer in flight, if one is.
    armed: Option<Duration>,
    now: Duration,
    #[cfg(test)]
    plant: Plant,
    #[cfg(test)]
    reads: u64,
}

impl Loop {
    /// The driver of `client`, naming its effects with tags from `base` ([`TAGS`] of them),
    /// holding at most `most` requests in the client at once (at least one).
    pub fn new(client: Client, base: u64, most: usize) -> Loop {
        Loop {
            client,
            base,
            next: 0,
            flying: BTreeMap::new_in(Global),
            conns: BTreeMap::new_in(Global),
            effects: Vec::new_in(Global),
            told: VecDeque::new_in(Global),
            most: most.max(1),
            running: BTreeMap::new_in(Global),
            queued: VecDeque::new_in(Global),
            armed: None,
            now: Duration::ZERO,
            #[cfg(test)]
            plant: Plant::None,
            #[cfg(test)]
            reads: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    /// Whether `tag` is one of this loop's: its completion goes to [`Loop::complete`].
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0 >= self.base && tag.0 - self.base < TAGS
    }

    /// The client, for what it says (its pool, its config).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Requests in the client or queued.
    pub fn requests(&self) -> usize {
        self.running.len() + self.queued.len()
    }

    fn tag(&mut self, op: Op) -> Tag {
        assert!(
            self.next < TAGS,
            "blaze-http's driver named {TAGS} effects from base {}: its tags are spent",
            self.base
        );
        let tag = self.base + self.next;
        self.next += 1;
        self.flying.insert(tag, op);
        Tag(tag)
    }

    /// Queues `request` as `id`, unique among this loop's requests; with `until`, a deadline of
    /// its asker's on the driver's clock, past which it ends in [`Told::TimedOut`]. An id in
    /// use ends it at once in [`Event::Failed`] ([`Kind::InUse`]), leaving the other be.
    ///
    /// It starts in the client at the next [`Loop::step`], after the client's clock is moved to
    /// that step's `now`, never here: the client times a request from its own clock, which
    /// stands where the last step left it, and a driver that waited since (to its deadline, or
    /// until an asker woke it) would start the request already late by that wait, its timeout
    /// spent before it asked anything.
    pub fn ask(&mut self, id: RequestId, request: Request, until: Option<Duration>) {
        if self.running.contains_key(&id) || self.queued.iter().any(|(it, ..)| *it == id) {
            let error = Error { url: request.url, kind: Kind::InUse };
            self.told.push_back(Told::Event(Event::Failed { request: id, error }));
            return;
        }
        #[cfg(test)]
        if self.plant == Plant::StaleClock {
            match self.client.request(id, request) {
                Ok(()) => _ = self.running.insert(id, until),
                Err(error) => self.told.push_back(Told::Event(Event::Failed { request: id, error })),
            }
            return;
        }
        self.queued.push_back((id, request, until));
    }

    /// The asker of `id` took `bytes` of its body.
    pub fn consumed(&mut self, id: RequestId, bytes: usize) {
        self.client.consumed(id, bytes);
    }

    /// The asker of `id` went away: it tells nothing more. A queued request that takes its
    /// place starts at the next [`Loop::step`] ([`Loop::ask`]).
    pub fn cancel(&mut self, id: RequestId) {
        self.queued.retain(|(it, ..)| *it != id);
        if self.running.remove(&id).is_some() {
            self.client.cancel(id);
        }
    }

    /// [`Told::Woke`] of `key` once `after` has passed.
    pub fn sleep(&mut self, key: u64, after: Duration) {
        let tag = self.tag(Op::Sleep { key });
        self.effects.push(Effect::Timer { after, tag });
    }

    /// Starts queued requests while fewer than `most` are in the client: whether it started any.
    /// Called only from [`Loop::step`], after the client's clock is moved to now.
    fn fill(&mut self) -> bool {
        let mut any = false;
        while self.running.len() < self.most {
            let Some((id, request, until)) = self.queued.pop_front() else { break };
            any = true;
            match self.client.request(id, request) {
                Ok(()) => {
                    self.running.insert(id, until);
                }
                Err(error) => self.told.push_back(Told::Event(Event::Failed { request: id, error })),
            }
        }
        any
    }

    /// The time is `now` on the driver's clock: the client's clock moves to it and what was due
    /// by then happens (a request past its deadline ends). A driver calls it as each wait ends,
    /// before it hands the loop what came in ([`Loop::complete`], [`Loop::consumed`]), so that
    /// what those start (a redirect's hop, a retry, a stall's clock) is timed from now and not
    /// from before the wait; [`Loop::step`] calls it first.
    pub fn advance(&mut self, now: Duration) {
        self.now = self.now.max(now);
        self.client.advance_clock(self.now);
        let now = self.now;
        let expired: Vec<RequestId, Global> = {
            let mut it = Vec::new_in(Global);
            it.extend(self.running.iter().filter(|(_, until)| until.is_some_and(|at| at <= now)).map(|(id, _)| *id));
            it
        };
        for id in expired {
            self.running.remove(&id);
            self.client.cancel(id);
            self.told.push_back(Told::TimedOut(id));
        }
        self.queued.retain(|(id, _, until)| {
            let over = until.is_some_and(|at| at <= now);
            if over {
                self.told.push_back(Told::TimedOut(*id));
            }
            !over
        });
    }

    /// The time is `now` on the driver's clock: what was due happens ([`Loop::advance`]),
    /// queued requests start on the client's clock as it now stands, the client's IO is turned
    /// into effects and moved to `out` (with this loop's own), and its deadline is armed.
    pub fn step(&mut self, now: Duration, out: &mut Vec<Effect>) {
        self.advance(now);
        // Each pass drains what the client asked; doing it may tell the client more (a send on
        // a connection not open is broken at once), and an end lets a queued request start.
        loop {
            let mut any = false;
            while let Some(io) = self.client.next_io() {
                any = true;
                self.io(io);
            }
            while let Some(event) = self.client.next_event() {
                any = true;
                if event.ends() {
                    self.running.remove(&event.request());
                }
                self.told.push_back(Told::Event(event));
            }
            if !self.fill() && !any {
                break;
            }
        }
        self.arm();
        out.append(&mut self.effects);
    }

    /// The first of the client's deadline and the askers' own.
    fn due(&self) -> Option<Duration> {
        let asked = self.running.values().chain(self.queued.iter().map(|(_, _, until)| until)).filter_map(|it| *it).min();
        match (self.client.next_deadline(), asked) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// A timer for the next deadline, unless one in flight comes first: a driver's clock
    /// reaches a deadline only through an effect due then (the simulation's does not move
    /// otherwise), and a poll's wait ends at it ([`Loop::deadline`]).
    fn arm(&mut self) {
        #[cfg(test)]
        if self.plant == Plant::NoDeadline {
            return;
        }
        let Some(due) = self.due() else { return };
        if self.armed.is_some_and(|armed| armed <= due) {
            return;
        }
        let tag = self.tag(Op::Timer { due });
        self.effects.push(Effect::Timer { after: due.saturating_sub(self.now), tag });
        self.armed = Some(due);
    }

    /// Until when a poll of the driver may wait: the armed deadline, or none.
    pub fn deadline(&self) -> effects::wait::Deadline {
        #[cfg(test)]
        if self.plant == Plant::NoDeadline {
            return effects::wait::Deadline::NEVER;
        }
        self.armed.map_or(effects::wait::Deadline::NEVER, effects::wait::Deadline::at)
    }

    /// What it has to tell, in order.
    pub fn next_told(&mut self) -> Option<Told> {
        self.told.pop_front()
    }

    fn hangup(&mut self, conn: Conn) {
        let tag = self.tag(Op::Hangup);
        self.effects.push(Effect::Hangup { conn, tag });
    }

    fn opening(&self, conn: ConnId) -> bool {
        self.conns.get(&conn) == Some(&Slot::Opening)
    }

    fn open(&self, conn: ConnId) -> Option<Conn> {
        match self.conns.get(&conn) {
            Some(Slot::Open(it)) => Some(*it),
            _ => None,
        }
    }

    /// Connects `conn` to the first of `rest`, the rest after it while it fails; with none
    /// left, the connect failed, saying how each tried.
    fn connect(&mut self, conn: ConnId, mut rest: VecDeque<SocketAddr, Global>, tried: String) {
        match rest.pop_front() {
            Some(at) => {
                let tag = self.tag(Op::Connect { conn, at, rest, tried });
                self.effects.push(Effect::Connect { at: Address::Tcp(at), options: blaze_core::sockopt::Tcp::DEFAULT, tag });
            }
            None => {
                self.conns.remove(&conn);
                self.client.complete(Completion::ConnectFailed { conn, why: tried });
            }
        }
    }

    /// Does `io`, the client's ask.
    fn io(&mut self, io: Io) {
        match io {
            Io::Connect { conn, host, port } => {
                self.conns.insert(conn, Slot::Opening);
                match host.parse::<IpAddr>() {
                    Ok(ip) => {
                        let mut at = VecDeque::new_in(Global);
                        at.push_back(SocketAddr::new(ip, port));
                        self.connect(conn, at, String::new());
                    }
                    Err(_) => {
                        let tag = self.tag(Op::Resolve { conn, connect: true });
                        self.effects.push(Effect::Resolve { name: host, port, tag });
                    }
                }
            }
            Io::Resolve { conn, host } => {
                self.conns.entry(conn).or_insert(Slot::Opening);
                let tag = self.tag(Op::Resolve { conn, connect: false });
                self.effects.push(Effect::Resolve { name: host, port: 0, tag });
            }
            Io::Send { conn, bytes } => match self.open(conn) {
                Some(socket) => {
                    let tag = self.tag(Op::Send { conn });
                    self.effects.push(Effect::Send { conn: socket, bytes, tag });
                }
                None => self.client.complete(Completion::Broken { conn, why: not_open(conn, "send") }),
            },
            Io::Recv { conn } => match self.open(conn) {
                Some(socket) => {
                    let tag = self.tag(Op::Recv { conn });
                    self.effects.push(Effect::Recv { conn: socket, most: RECV_MOST, into: Vec::new_in(Global), tag });
                }
                None => self.client.complete(Completion::Broken { conn, why: not_open(conn, "receive") }),
            },
            Io::Close { conn } => {
                // What is in flight on it completes later, into nothing: its id is not reused.
                if let Some(Slot::Open(socket)) = self.conns.remove(&conn) {
                    self.hangup(socket);
                }
            }
        }
    }

    /// Takes the completion of one of this loop's effects ([`Loop::owns`]).
    pub fn complete(&mut self, completion: effects::Completion) {
        let effects::Completion { tag, done } = completion;
        let Some(op) = self.flying.remove(&tag.0) else {
            panic!(
                "blaze-http's driver: {tag:?} names no effect of its in flight (blaze-core told a completion twice, or of an effect never asked)"
            )
        };
        #[cfg(test)]
        if let (Plant::DropCompletion { nth }, Op::Recv { .. }) = (self.plant, &op) {
            self.reads += 1;
            if self.reads == nth {
                return;
            }
        }
        match (op, done) {
            (Op::Resolve { conn, connect: true }, done) => {
                if !self.opening(conn) {
                    // Closed while it resolved.
                    return;
                }
                let why = match done {
                    Done::Resolved(at) if !at.is_empty() => {
                        let mut rest = VecDeque::with_capacity_in(at.len(), Global);
                        rest.extend(at);
                        return self.connect(conn, rest, String::new());
                    }
                    Done::Resolved(_) => "the name resolved to no address".to_owned(),
                    Done::Unresolved(unresolved) => unresolved.to_string(),
                    Done::Failed(error) => error.0,
                    done => unexpected("a connect's resolve", &done),
                };
                self.conns.remove(&conn);
                self.client.complete(Completion::ConnectFailed { conn, why });
            }
            (Op::Resolve { conn, connect: false }, done) => {
                if !self.conns.contains_key(&conn) {
                    return;
                }
                let completion = match done {
                    Done::Resolved(at) => Completion::Resolved { conn, addresses: at.iter().map(SocketAddr::ip).collect() },
                    Done::Unresolved(unresolved) => Completion::ResolveFailed { conn, why: unresolved.to_string() },
                    Done::Failed(error) => Completion::ResolveFailed { conn, why: error.0 },
                    done => unexpected("a proxy's resolve", &done),
                };
                self.client.complete(completion);
            }
            (Op::Connect { conn, at, rest, mut tried }, done) => match done {
                Done::Connected { conn: socket } => {
                    if self.opening(conn) {
                        self.conns.insert(conn, Slot::Open(socket));
                        self.client.complete(Completion::Connected { conn });
                    } else {
                        // Closed while it connected: the socket goes.
                        self.hangup(socket);
                    }
                }
                Done::Failed(error) => {
                    if !self.opening(conn) {
                        return;
                    }
                    if !tried.is_empty() {
                        tried.push_str("; ");
                    }
                    // Writing to a `String` never fails.
                    _ = write!(tried, "{at}: {error}");
                    self.connect(conn, rest, tried);
                }
                done => unexpected("a connect", &done),
            },
            (Op::Send { conn }, done) => {
                if self.open(conn).is_none() {
                    return;
                }
                let completion = match done {
                    Done::Sent(_) => Completion::Sent { conn },
                    Done::Broken { error, .. } | Done::Failed(error) => Completion::Broken { conn, why: error.0 },
                    done => unexpected("a send", &done),
                };
                self.client.complete(completion);
            }
            (Op::Recv { conn }, done) => {
                if self.open(conn).is_none() {
                    return;
                }
                let completion = match done {
                    Done::Read(bytes) => Completion::Received { conn, bytes },
                    Done::Broken { error, .. } | Done::Failed(error) => Completion::Broken { conn, why: error.0 },
                    done => unexpected("a receive", &done),
                };
                self.client.complete(completion);
            }
            // A hangup's one failure is a socket already gone, which is what it asked.
            (Op::Hangup, Done::HungUp | Done::Failed(_)) => {}
            (Op::Timer { due }, Done::Fired) => {
                if self.armed == Some(due) {
                    self.armed = None;
                }
            }
            (Op::Sleep { key }, Done::Fired) => self.told.push_back(Told::Woke(key)),
            (op, done) => panic!("blaze-http's driver: {op:?} completed with {done:?}, which blaze-core's effect never gives"),
        }
    }
}

fn not_open(conn: ConnId, what: &str) -> String {
    format!(
        "{conn:?} is not open in blaze-http's driver: a {what} asked before its connect completed or after its close (the client's driver contract)"
    )
}

fn unexpected(what: &str, done: &Done) -> ! {
    panic!("blaze-http's driver: {what} completed with {done:?}, which blaze-core's effect never gives")
}

/// Maximum payload size accepted by [`Handle::send_udp`] (fits a `BLZP` `HAVE` datagram).
pub const MAX_UDP_PAYLOAD: usize = 1040;
/// Fixed byte size forwarded as [`FetchEvent::Datagram`] (matches a `BLZP` `HAS` datagram).
pub const MAX_UDP_REPLY: usize = 24;

/// Tag space for the shared UDP probe socket on the fetch driver thread (disjoint from the WAN
/// loop at `0..TAGS` and the optional LAN peer loop at `TAGS..2*TAGS`).
const UDP_BASE: u64 = 2 * TAGS;
const TAG_UDP_BIND: Tag = Tag(UDP_BASE);
const TAG_UDP_RECV: Tag = Tag(UDP_BASE + 1);
const TAG_UDP_SEND_BASE: u64 = UDP_BASE + 2;

/// One request's answer, collected whole on the driver's thread ([`Handle::fetch`]).
#[derive(Debug)]
pub enum Fetched {
    /// `url` answered (after redirects) with `status`, `headers` and all of `body`.
    Answered { url: String, status: u16, headers: Vec<(String, String)>, body: Vec<u8> },
    Failed(Error),
    /// The asker's deadline passed first.
    TimedOut,
}

/// Unified event delivered by a [`Handle`] spawned with [`Handle::spawn_with_peer`].
#[derive(Debug)]
pub enum FetchEvent {
    /// An HTTP answer (from either WAN or LAN peer) for `key`.
    Http { key: u64, fetched: Fetched },
    /// A fixed-size UDP reply datagram received on the shared probe socket.
    Datagram { from: SocketAddr, bytes: [u8; MAX_UDP_REPLY] },
}

/// Optional LAN peer HTTP client + shared UDP probe socket driven on the same `blaze-core`
/// native driver thread as the primary WAN [`Loop`].
pub struct PeerDriver {
    /// Plain-HTTP LAN peer client (caller configures `redirects: 0` and `Proxies::none()`).
    pub client: Client,
    /// Local UDP address to bind for non-blocking peer probes (`None` disables UDP probing).
    pub udp_bind: Option<SocketAddr>,
    /// Unified unbounded channel receiving both WAN/peer HTTP answers and UDP datagrams.
    pub events: Sender<FetchEvent>,
    /// Explicit heap capability for the peer driver's internal tables (no global heap).
    pub heap: &'static Heap,
}

/// Inline UDP send packet (no heap allocation per probe).
struct UdpOut {
    targets: [Option<SocketAddr>; 2],
    len: u16,
    bytes: [u8; MAX_UDP_PAYLOAD],
}

/// Where a collected request's [`Fetched`] answer goes.
enum CollectTo {
    Direct(Sender<(u64, Fetched)>),
    Unified,
}

/// What an asker of a [`Handle`] gets its request's events by.
enum Asker {
    Events(Sender<Event>),
    Collect { key: u64, to: CollectTo, head: Option<(String, u16, Vec<(String, String)>)>, body: Vec<u8> },
}

/// What a [`Handle`]'s askers post to its thread.
enum Message {
    /// `within`: from when the thread takes it, on the driver's clock, so it is never early.
    Ask { id: RequestId, request: Request, within: Option<Duration>, asker: Asker },
    /// Unified WAN or LAN peer fetch tracked by `key` so it can be cancelled via [`Message::CancelKey`].
    AskPeer { id: RequestId, key: u64, peer: bool, request: Request, within: Duration },
    Consumed { id: RequestId, bytes: usize },
    Cancel { id: RequestId },
    CancelKey { key: u64 },
    SendUdp(UdpOut),
    Sleep { after: Duration, woke: Sender<()> },
    Stop,
}

type Inbox = mailbox::Inbox<Message>;
type SpawnResult = std::io::Result<Handle>;

/// The askers' side of a [`Handle`]; its drop stops the thread.
struct Owner {
    inbox: Arc<Inbox, Global>,
    ids: AtomicU64,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.inbox.post(Message::Stop);
    }
}

/// A [`Loop`] on the native driver, on a thread of its own, asked from any thread; cheap to
/// clone. The last clone dropped stops it, and every answer still waiting reads its end.
#[derive(Clone)] // clone: a handle on the one thread, once per asker
pub struct Handle {
    owner: Arc<Owner, Global>,
}

impl Handle {
    /// Starts the thread driving `client`, at most `most` requests in it at once.
    pub fn spawn(client: Client, most: usize) -> SpawnResult {
        Self::spawn_inner(client, most, None)
    }

    /// Starts the thread driving `wan`, a second LAN peer [`Loop`], and an optional non-blocking
    /// UDP probe socket on the same single `blaze-core` native driver.
    pub fn spawn_with_peer(wan: Client, most: usize, peer: PeerDriver) -> SpawnResult {
        Self::spawn_inner(wan, most, Some(peer))
    }

    fn spawn_inner(client: Client, most: usize, peer: Option<PeerDriver>) -> SpawnResult {
        let (ready_to, ready) = chan::bounded::<std::io::Result<Arc<Inbox, Global>>>(1);
        std::thread::Builder::new().name("blaze-http h1".to_owned()).spawn(move || {
            // No file effect is asked: the root is never read.
            let mut driver = match effects::native(Path::new("/"), 1) {
                Ok(it) => it,
                Err(error) => {
                    _ = ready_to.send(Err(error));
                    return;
                }
            };
            let Some(waker) = driver.waker() else {
                _ = ready_to.send(Err(std::io::Error::other("blaze-core's native driver gives no waker: an asker could not end its wait")));
                return;
            };
            let inbox = Arc::new_in(Inbox::new(waker), Global);
            if ready_to.send(Ok(Arc::clone(&inbox))).is_err() {
                return;
            }
            drop(ready_to);
            let peer_state = peer.map(|cfg| PeerState::new(cfg, most));
            serve_with_peer(&mut driver, Loop::new(client, 0, most), peer_state, &inbox);
        })?;
        let inbox = match mailbox::answer("blaze-http's driver thread to make its driver", &ready, START_WAIT) {
            Ok(Some(made)) => made?,
            Ok(None) => return Err(std::io::Error::other("blaze-http's driver thread ended before it made its driver")),
            Err(unanswered) => return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, unanswered)),
        };
        let owner = Owner { inbox, ids: AtomicU64::new(0) };
        Ok(Handle { owner: Arc::new_in(owner, Global) })
    }

    fn id(&self) -> RequestId {
        RequestId(self.owner.ids.fetch_add(1, Ordering::Relaxed))
    }

    /// Starts `request`: its events come from the [`Answer`].
    pub fn ask(&self, request: Request) -> Answer {
        let id = self.id();
        // Not bounded: the body's pieces in it are held to the client's window
        // ([`crate::WINDOW`]) ahead of what the asker took.
        let (events, from_loop) = chan::unbounded();
        self.owner.inbox.post(Message::Ask { id, request, within: None, asker: Asker::Events(events) });
        Answer { id, from_loop, inbox: Arc::clone(&self.owner.inbox), taken: 0, over: false }
    }

    /// Starts `request`, collected whole on the driver's thread and handed to `to` as `key`'s
    /// [`Fetched`], [`Fetched::TimedOut`] once `within` passes first. `to` is an unbounded
    /// channel (`blaze_core::chan::unbounded`): the driver's thread never waits to hand over.
    pub fn fetch(&self, request: Request, within: Duration, key: u64, to: &Sender<(u64, Fetched)>) {
        let id = self.id();
        let asker = Asker::Collect { key, to: CollectTo::Direct(to.clone()), head: None, body: Vec::new_in(Global) };
        self.owner.inbox.post(Message::Ask { id, request, within: Some(within), asker });
    }

    /// Starts a WAN or LAN peer request on a [`Handle`] spawned with [`Handle::spawn_with_peer`],
    /// delivering [`FetchEvent::Http`] to the unified event channel and enabling cancellation via
    /// [`Handle::cancel_fetch`].
    pub fn fetch_for_race(&self, request: Request, within: Duration, key: u64, peer: bool) {
        let id = self.id();
        self.owner.inbox.post(Message::AskPeer { id, key, peer, request, within });
    }

    /// Cancels an in-flight request previously started with [`Handle::fetch_for_race`].
    pub fn cancel_fetch(&self, key: u64) {
        self.owner.inbox.post(Message::CancelKey { key });
    }

    /// Sends `payload` (at most [`MAX_UDP_PAYLOAD`] bytes) to up to two UDP destinations on the
    /// shared probe socket. Silently ignored if UDP probing is disabled or unavailable.
    pub fn send_udp(&self, targets: &[SocketAddr], payload: &[u8]) {
        let Ok(len) = u16::try_from(payload.len()) else { return };
        if payload.is_empty() || payload.len() > MAX_UDP_PAYLOAD {
            return;
        }
        let mut bytes = [0u8; MAX_UDP_PAYLOAD];
        if let Some(dst) = bytes.get_mut(..payload.len()) {
            dst.copy_from_slice(payload);
        }
        let t0 = targets.first().copied();
        let t1 = targets.get(1).copied();
        if t0.is_none() {
            return;
        }
        self.owner.inbox.post(Message::SendUdp(UdpOut { targets: [t0, t1], len, bytes }));
    }

    /// A timer on the driver's clock: the receiver gets `()` once `after` has passed there, and
    /// reads its end ([`blaze_core::chan::RecvTimeoutError::Disconnected`]) if the driver stopped first. The
    /// caller's receive on it takes its own bound, as every wait does.
    pub fn sleep(&self, after: Duration) -> Receiver<()> {
        let (woke, waking) = chan::bounded(1);
        self.owner.inbox.post(Message::Sleep { after, woke });
        waking
    }
}

/// One request's events, as they come.
pub struct Answer {
    id: RequestId,
    from_loop: Receiver<Event>,
    inbox: Arc<Inbox, Global>,
    /// The bytes of the last piece of the body handed out, said taken at the next ask.
    taken: usize,
    /// Whether its last event (`Done` or `Failed`) came.
    over: bool,
}

impl Answer {
    /// The request's next event, waiting at most `within` on this thread ([`NEXT_EVENT`] in
    /// [`Unanswered::TimedOut`] once it passes; the request goes on, and the next call waits
    /// again); `Ok(None)` when the driver stopped first (its handle dropped, or its thread
    /// panicked), which no deadline of the client's can then end.
    pub fn next(&mut self, within: Duration) -> Result<Option<Event>, Unanswered> {
        if self.taken > 0 {
            let bytes = std::mem::take(&mut self.taken);
            self.inbox.post(Message::Consumed { id: self.id, bytes });
        }
        let Some(event) = mailbox::answer(NEXT_EVENT, &self.from_loop, within)? else { return Ok(None) };
        match &event {
            Event::Body { bytes, .. } => self.taken = bytes.len(),
            Event::Done { .. } | Event::Failed { .. } => self.over = true,
            Event::Head { .. } => {}
        }
        Ok(Some(event))
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if !self.over {
            self.inbox.post(Message::Cancel { id: self.id });
        }
    }
}

enum UdpState {
    Disabled,
    Binding { at: SocketAddr, pending: OwnVec<UdpOut, Alloc<'static>> },
    Ready { sock: Dgram },
}

struct PeerState {
    peer_loop: Loop,
    events: Sender<FetchEvent>,
    key_to_req: SortedMap<u64, (bool, RequestId), Alloc<'static>>,
    udp: UdpState,
    udp_bind_sent: bool,
    next_send_tag: u64,
}

impl PeerState {
    fn new(cfg: PeerDriver, most: usize) -> PeerState {
        let alloc = cfg.heap.alloc();
        let udp = match cfg.udp_bind {
            Some(at) => UdpState::Binding { at, pending: OwnVec::new_in(alloc) },
            None => UdpState::Disabled,
        };
        PeerState {
            peer_loop: Loop::new(cfg.client, TAGS, most),
            events: cfg.events,
            key_to_req: SortedMap::new_in(alloc),
            udp,
            udp_bind_sent: false,
            next_send_tag: TAG_UDP_SEND_BASE,
        }
    }

    fn push_udp_sends(sock: Dgram, out: &UdpOut, next_send_tag: &mut u64, effects: &mut impl Extend<Effect>) {
        let len = usize::from(out.len);
        let Some(payload) = out.bytes.get(..len) else { return };
        for dest in out.targets.into_iter().flatten() {
            let mut batch = Batch::new();
            batch.push(dest, Ecn::NotEct, payload);
            let tag = Tag(*next_send_tag);
            *next_send_tag = next_send_tag.saturating_add(1);
            effects.extend([Effect::SendTo { sock, batch, tag }]);
        }
    }
}

fn udp_probe_options() -> DgramOptions {
    DgramOptions {
        tuning: UdpOptions {
            broadcast: SockAsk::WhereAvailable(true),
            multicast_ttl: SockAsk::WhereAvailable(1),
            multicast_loop: SockAsk::WhereAvailable(true),
            ..UdpOptions::DEFAULT
        },
        ..DgramOptions::default()
    }
}

/// The thread's loop: the askers' messages in, the loop's effects out, its tellings to their
/// askers, until the handle is dropped.
#[cfg(test)]
fn serve(driver: &mut impl Driver, core: Loop, inbox: &Inbox) {
    serve_with_peer(driver, core, None, inbox);
}

fn serve_with_peer(driver: &mut impl Driver, mut core: Loop, mut peer: Option<PeerState>, inbox: &Inbox) {
    let mut askers: BTreeMap<RequestId, Asker> = BTreeMap::new_in(Global);
    let mut sleeps: BTreeMap<u64, Sender<()>> = BTreeMap::new_in(Global);
    let mut next_sleep = 0u64;
    let mut messages = Vec::new_in(Global);
    let mut effects = Vec::new_in(Global);
    let mut done = Vec::new_in(Global);
    if let Some(ps) = peer.as_mut()
        && let UdpState::Binding { at, .. } = ps.udp
        && !ps.udp_bind_sent
    {
        ps.udp_bind_sent = true;
        effects.push(Effect::Bind { at, options: udp_probe_options(), tag: TAG_UDP_BIND });
    }
    loop {
        inbox.take(&mut messages);
        for message in messages.drain(..) {
            match message {
                Message::Ask { id, request, within, asker } => {
                    askers.insert(id, asker);
                    core.ask(id, request, within.map(|it| driver.now().saturating_add(it)));
                }
                Message::AskPeer { id, key, peer: is_peer, request, within } => {
                    let until = Some(driver.now().saturating_add(within));
                    askers.insert(id, Asker::Collect { key, to: CollectTo::Unified, head: None, body: Default::default() });
                    if let Some(ps) = peer.as_mut() {
                        ps.key_to_req.insert(key, (is_peer, id));
                        if is_peer {
                            ps.peer_loop.ask(id, request, until);
                        } else {
                            core.ask(id, request, until);
                        }
                    } else {
                        core.ask(id, request, until);
                    }
                }
                Message::Consumed { id, bytes } => core.consumed(id, bytes),
                Message::Cancel { id } => {
                    askers.remove(&id);
                    core.cancel(id);
                }
                Message::CancelKey { key } => {
                    if let Some(ps) = peer.as_mut()
                        && let Some((is_peer, id)) = ps.key_to_req.remove(&key)
                    {
                        askers.remove(&id);
                        if is_peer {
                            ps.peer_loop.cancel(id);
                        } else {
                            core.cancel(id);
                        }
                    }
                }
                Message::SendUdp(out) => {
                    if let Some(ps) = peer.as_mut() {
                        match &mut ps.udp {
                            UdpState::Disabled => {}
                            UdpState::Binding { pending, .. } => pending.push(out),
                            UdpState::Ready { sock } => {
                                PeerState::push_udp_sends(*sock, &out, &mut ps.next_send_tag, &mut effects);
                            }
                        }
                    }
                }
                Message::Sleep { after, woke } => {
                    sleeps.insert(next_sleep, woke);
                    core.sleep(next_sleep, after);
                    next_sleep += 1;
                }
                Message::Stop => {
                    if let Some(ps) = peer.as_ref()
                        && let UdpState::Ready { sock } = ps.udp
                    {
                        effects.push(Effect::Unbind { sock, tag: Tag(ps.next_send_tag) });
                        driver.submit(&mut effects);
                    }
                    return;
                }
            }
        }
        let now = driver.now();
        core.step(now, &mut effects);
        while let Some(told) = core.next_told() {
            let (unified, keys) = match peer.as_mut() {
                Some(ps) => (Some(&ps.events), Some(&mut ps.key_to_req)),
                None => (None, None),
            };
            tell(&mut core, &mut askers, &mut sleeps, unified, keys, told);
        }
        if let Some(ps) = peer.as_mut() {
            ps.peer_loop.step(now, &mut effects);
            while let Some(told) = ps.peer_loop.next_told() {
                tell(&mut ps.peer_loop, &mut askers, &mut sleeps, Some(&ps.events), Some(&mut ps.key_to_req), told);
            }
        }
        driver.submit(&mut effects);
        let deadline = match peer.as_ref() {
            Some(ps) => core.deadline().min(ps.peer_loop.deadline()),
            None => core.deadline(),
        };
        driver.poll(&mut done, deadline);
        // The wait just ended: what came in during it (these completions, then the messages at
        // the top of the next turn) is timed from now.
        #[cfg(test)]
        let advance = core.plant != Plant::StaleClock;
        #[cfg(not(test))]
        let advance = true;
        if advance {
            let after = driver.now();
            core.advance(after);
            if let Some(ps) = peer.as_mut() {
                ps.peer_loop.advance(after);
            }
        }
        for completion in done.drain(..) {
            if core.owns(completion.tag) {
                core.complete(completion);
            } else if let Some(ps) = peer.as_mut() {
                if ps.peer_loop.owns(completion.tag) {
                    ps.peer_loop.complete(completion);
                } else if completion.tag == TAG_UDP_BIND {
                    match completion.done {
                        Done::Bound { sock, .. } => {
                            let prev = core::mem::replace(&mut ps.udp, UdpState::Ready { sock });
                            effects.push(Effect::RecvFrom {
                                sock,
                                into: Batch::new(),
                                slots: Slots { most: 16, size: 64 },
                                tag: TAG_UDP_RECV,
                            });
                            if let UdpState::Binding { pending, .. } = prev {
                                for out in pending.iter() {
                                    PeerState::push_udp_sends(sock, out, &mut ps.next_send_tag, &mut effects);
                                }
                            }
                        }
                        _ => {
                            // Restricted sandbox or network without UDP: silently disable LAN probing.
                            ps.udp = UdpState::Disabled;
                        }
                    }
                } else if completion.tag == TAG_UDP_RECV {
                    match completion.done {
                        Done::Datagrams(mut batch) => {
                            for (head, raw) in batch.each() {
                                if let Ok(arr) = <&[u8; MAX_UDP_REPLY]>::try_from(raw) {
                                    _ = ps.events.try_send(FetchEvent::Datagram { from: head.peer, bytes: *arr });
                                }
                            }
                            batch.clear();
                            if let UdpState::Ready { sock } = ps.udp {
                                effects.push(Effect::RecvFrom {
                                    sock,
                                    into: batch,
                                    slots: Slots { most: 16, size: 64 },
                                    tag: TAG_UDP_RECV,
                                });
                            }
                        }
                        _ => {
                            ps.udp = UdpState::Disabled;
                        }
                    }
                }
            }
        }
    }
}

/// Hands `told` to whoever waits for it.
fn tell(
    core: &mut Loop,
    askers: &mut BTreeMap<RequestId, Asker>, sleeps: &mut BTreeMap<u64, Sender<()>>,
    unified: Option<&Sender<FetchEvent>>,
    key_to_req: Option<&mut SortedMap<u64, (bool, RequestId), Alloc<'static>>>,
    told: Told,
) {
    let id = match &told {
        Told::Event(event) => event.request(),
        Told::TimedOut(id) => *id,
        Told::Woke(key) => {
            if let Some(woke) = sleeps.remove(key) {
                // A sleeper that went away has nothing to wake; its one slot is empty, so the
                // loop's thread never waits here.
                _ = woke.try_send(());
            }
            return;
        }
    };
    let ends = !matches!(&told, Told::Event(event) if !event.ends());
    let Some(asker) = askers.get_mut(&id) else { return };
    let mut ended_key = None;
    match (asker, told) {
        // An asker that went away has posted its cancel, which the loop reads next.
        (Asker::Events(events), Told::Event(event)) => _ = events.send(event),
        (Asker::Events(_), _) => {}
        (Asker::Collect { key, to, head, body }, told) => {
            let fetched = match told {
                Told::Event(Event::Head { url, status, headers, .. }) => {
                    *head = Some((url, status, headers));
                    None
                }
                Told::Event(Event::Body { bytes, .. }) => {
                    body.extend_from_slice(&bytes);
                    core.consumed(id, bytes.len());
                    None
                }
                Told::Event(Event::Done { .. }) => match head.take() {
                    Some((url, status, headers)) => Some(Fetched::Answered { url, status, headers, body: std::mem::take(body) }),
                    None => panic!("blaze-http's client ended {id:?} with no head: its event order is Head, Body..., Done"),
                },
                Told::Event(Event::Failed { error, .. }) => Some(Fetched::Failed(error)),
                Told::TimedOut(_) => Some(Fetched::TimedOut),
                Told::Woke(_) => None,
            };
            if let Some(fetched) = fetched {
                match to {
                    CollectTo::Direct(to) => match to.try_send((*key, fetched)) {
                        // A reader that is gone wants nothing more.
                        Ok(()) | Err(chan::TrySendError::Disconnected(_)) => {}
                        Err(chan::TrySendError::Full(_)) => panic!(
                            "blaze-http's driver: the channel {id:?}'s answer goes to is full; `Handle::fetch` takes an unbounded one (`chan::unbounded`), so the driver's thread never waits on an asker"
                        ),
                    },
                    CollectTo::Unified => {
                        if let Some(events) = unified {
                            _ = events.try_send(FetchEvent::Http { key: *key, fetched });
                        }
                    }
                }
            }
            if ends {
                ended_key = Some(*key);
            }
        }
    }
    if ends {
        askers.remove(&id);
        if let (Some(key), Some(map)) = (ended_key, key_to_req) {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests;
