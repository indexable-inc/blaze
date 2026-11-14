//! The explorer's server as one synchronous state machine over blaze-http's HTTP/3 core
//! ([`Endpoint`]): every session, every request stream, every live stream and the certificate's
//! rotation, with no socket, thread, timer or runtime in it. Its inputs are the endpoint's
//! events, the time ([`Clock`], the driver's), its own timers' completions
//! ([`Serving::complete`]) and the results of the work it asked for ([`Serving::done`]); its
//! outputs are what it does to the endpoint, a timer effect ([`Effect::Timer`]) for its next
//! deadline, the work to run ([`Job`]: a source's answer, a live tick, a new certificate) and
//! lines to say ([`Serving::take_said`]). A driver (`super::server` on the native driver, the
//! tests on [`blaze_core::sim::Sim`]) runs the work off its loop and hands back each result.
//!
//! What a session answers is [`session`]'s; this machine carries out its decisions:
//!
//! - **CONNECT.** A session is refused before it opens unless its CONNECT is to [`link::PATH`]
//!   (404) from an allowed Origin (403); while the server stops, with 503.
//! - **Requests.** Each bidirectional stream the page opens takes a [`Slot`] of its session
//!   ([`session::Session::opened`]; one over [`session::MAX_IN_FLIGHT`] is answered
//!   `overloaded` and its reading stopped) and is read whole: at most [`session::MAX_REQUEST`]
//!   bytes within [`session::REQUEST_WAIT`] of its opening (else `bad_args`, its reading
//!   stopped). Until a good hello the session's requests are decided one at a time, in the
//!   order they opened, each once the one before it is answered; after it, each as it arrives.
//!   An answer is its bytes and FIN handed to the endpoint; the slot is given back once QUIC
//!   took them ([`Event::StreamSent`]), when the page stops the stream, or when the session
//!   closes. A request the page stops is dropped where it waits, and its [`super::Stop`]'s
//!   sender with it, so the source's work ends with it and is thrown away.
//! - **Close.** A `bad_token` answer closes its session once the page acknowledged all of it,
//!   FIN included ([`Event::StreamAcked`]), not when QUIC took it ([`Event::StreamSent`]): a
//!   session's close resets every stream of it at the page, so a close on `StreamSent` races
//!   the answer that says why and can lose it. A page that stops the answer has the close at
//!   once; a page that vanished never acknowledges, and its connection's idle bound
//!   (`server::IDLE`, 30 s with nothing heard) ends the session as [`blaze_http::h3::Closed::Failed`].
//!   Until then the request keeps its slot and the session core refuses whatever else comes.
//! - **Live.** A session's first good hello opens its live stream (unidirectional, again on
//!   [`Event::StreamsAvailable`] when the page allows none yet). It ticks every
//!   [`Setup::every`] from its opening: a tick is a job on the session's own feed, and its
//!   frames go out as it comes back. A page that reads slower than the frames come falls
//!   behind by at most one tick: while a tick's frames are not all taken by QUIC, or the feed is
//!   out on a job, ticks are skipped, not queued, and the next tick takes everything since the
//!   session's last one. A `cert` frame goes to every live stream when the certificate is made
//!   anew. A frame that cannot be sent ends the live stream; the session goes on.
//! - **Certificate.** Every [`CHECK`] of the driver's clock the wall clock ([`Clock::wall_s`])
//!   is asked whether the certificate is due ([`Rotation::due`]); a new one is a job, served
//!   from then on ([`Endpoint::set_identity`]). One not made is said, and tried again at the
//!   next check; the one served keeps being served until then, and until it expires.
//! - **Stop.** [`Serving::stop`] closes every session with [`session::CLOSE_STOPPED`]; the
//!   machine is [`Over`] once every connection is closed, or [`SHUTDOWN`] after the stop.
//!
//! # The floor, and what this costs against it
//!
//! The floor of a request is reading its bytes once, deciding it once and handing its answer
//! to the endpoint once. A step costs the events it drains, each handled in O(log n) map
//! lookups (n: the open requests and sessions, at most [`session::MAX_IN_FLIGHT`] requests a
//! session), plus one pass over the session's at most [`session::MAX_IN_FLIGHT`] requests when
//! one changes state, plus one pass over the open requests and sessions to arm the one timer:
//! O(events x log n + n) a step, against the floor's O(events). A request's bytes are copied
//! once into its buffer (the endpoint hands them over owned, as one `Vec` a piece) and parsed
//! twice (the session core reads its `op`; the source's job parses the whole for the source).
//! The machine asks for no thread and no wake per input: its one timer is re-armed only when
//! its next deadline moves earlier, and the loop that runs it adds nothing to the h3 driver's
//! own poll but one wake per finished job.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    alloc::Global,
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use blaze_core::{Completion, Effect, Tag, wait::Deadline};
use blaze_http::h3::{Direction, Endpoint, Event, Identity, SessionId, SessionRequest, StreamRef};
use serde::Serialize;

use super::{
    CoreWasm, Hello, Live, OpError, Stop, Tick,
    cert::{CertError, Rotation, Validity},
    frame,
    link::{self, Origins, Token},
    session::{self, Emit, Feed, Phase, Served, Slot, Step},
};

/// How often the wall clock is asked whether the certificate is due.
pub const CHECK: Duration = Duration::from_secs(600);
/// How long a stopping server waits for its sessions' closes to be acknowledged.
pub const SHUTDOWN: Duration = Duration::from_secs(1);
/// How many tags one [`Serving`] names its timers with, from its base.
pub const TAGS: u64 = 1 << 40;

/// The time at a step, on the clocks the machine reads; the driver reads them, the machine
/// never does.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// The driver's monotonic clock: every deadline is on it.
    pub now: Duration,
    /// The source's clock ([`crate::clock::now_ns`]): a slot's start and a tick's `at_ns`.
    pub now_ns: i64,
    /// The wall clock, in Unix seconds: what a certificate is dated by.
    pub wall_s: u64,
}

/// What a [`Serving`] starts with.
pub struct Setup {
    /// The link's token, which a session's hello must carry.
    pub token: Token,
    pub origins: Origins,
    /// The command that prints the server's link: `bad_token` names it, and so do the
    /// certificate's common name, the close reason and the lines said.
    pub name: &'static str,
    /// How often a live stream ticks; never zero.
    pub every: Duration,
    /// The certificate served at the start.
    pub served: Served,
    pub rotation: Rotation,
    /// The explorer core served the page, or why there is none.
    pub core: Result<CoreWasm, String>,
}

/// A job's name, unique for the machine's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// Work the machine asks its driver to run off the loop, and to hand back by [`Serving::done`]
/// with its [`JobId`].
pub struct Job {
    pub id: JobId,
    pub work: Work,
}

/// What a [`Job`] is.
pub enum Work {
    /// The source's hello fields ([`super::Source::hello`]); handed back as [`Done::Hello`].
    Hello,
    /// Op `name` of the source with the request whole (`request`, a JSON object the session
    /// core read the `op` of), answered as [`frame::answer_bytes`] with its `elapsed_ns` from
    /// `started_ns` (the source's clock); handed back as [`Done::Op`]. `stop` is closed when the
    /// page drops the request.
    Op { name: String, request: Vec<u8>, started_ns: i64, stop: Stop },
    /// A tick of a session's live feed at `at_ns`: the feed made by [`super::Source::live`]
    /// when there is none yet, its tick cleared ([`Tick::clear`]) and read; handed back as
    /// [`Done::Tick`] with the feeding.
    Tick { feeding: Box<Feeding>, at_ns: i64 },
    /// A certificate for `validity`, named `name` ([`super::cert::Cert::make`]), and its
    /// identity; handed back as [`Done::Cert`].
    Cert { validity: Validity, name: &'static str },
}

/// A session's live feed and the tick it reads into, which go out on each tick's job and come
/// back with it.
#[derive(Default)]
pub struct Feeding {
    /// `None` until the first tick's job makes it.
    pub live: Option<Box<dyn Live>>, // dyn: the source's own feed
    pub tick: Tick,
}

/// A job's result.
pub enum Done {
    Hello(Hello),
    /// The answer's bytes.
    Op(Vec<u8>),
    Tick { feeding: Box<Feeding>, read: Result<(), OpError> },
    /// The certificate's identity and its hash ([`link::certhash`]).
    Cert(Result<(Identity, String), CertError>),
    /// The job did not run to its end (it panicked, or its thread did not start): what to
    /// answer, or say, in its place.
    Failed(OpError),
}

/// Why a stopping machine is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Over {
    /// Every connection is closed.
    Closed,
    /// [`SHUTDOWN`] passed first: a page did not acknowledge its close.
    Bound,
}

/// A test-only switch: a safeguard removed, which the simulation's oracle must catch.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// The `nth` job result (from 1) is dropped: what waited on it waits on.
    DropDone { nth: u32 },
    /// No timer is ever armed: the machine's deadlines are reached only when something else
    /// steps it.
    NoRearm,
}

/// Where a request is.
enum At {
    /// Its bytes coming; whole by `due`.
    Reading { bytes: Vec<u8, Global>, due: Duration },
    /// Read (or refused as it was read), waiting to be decided.
    Read(Result<Vec<u8, Global>, OpError>),
    /// Its answer is a job's. Dropping `stop` tells the source to stop.
    Asked { job: JobId, stop: Option<crossbeam_channel::Sender<()>> },
    /// Its answer handed to the endpoint. With `close`, it ends at the answer's
    /// acknowledgement (or the page's stop) and its session is closed then; without, it ends
    /// when QUIC took the answer.
    Writing { close: Option<(u32, &'static str)> },
}

/// A request stream of an open session.
struct Request {
    session: SessionId,
    /// `None` for a stream refused `overloaded`, which took none.
    slot: Option<Slot>,
    at: At,
}

/// Where a session's live stream is.
enum LaneAt {
    /// Asked for; the page allows no stream yet.
    Wanted,
    Open(StreamRef),
    /// Ended: a frame was not sent, or the page stopped it.
    Over,
}

/// A session's live stream.
struct Lane {
    at: LaneAt,
    feed: Feed,
    /// `None` while it is out on a tick's job.
    feeding: Option<Box<Feeding>>,
    /// When it next ticks.
    next: Duration,
    /// Frames handed to the endpoint that QUIC has not all taken.
    unsent: bool,
    /// A `cert` frame is owed once it opens.
    cert_owed: bool,
    /// A frame's head, its buffer kept.
    head: Vec<u8, Global>,
}

/// An open session.
struct Open {
    core: session::Session,
    /// Its requests in the order they opened, until each ends.
    order: VecDeque<StreamRef, Global>,
    live: Option<Lane>,
}

/// What a job's result is for.
enum For {
    Hello { stream: StreamRef, session: SessionId, first: bool },
    Op { stream: StreamRef },
    Tick { session: SessionId },
    Cert { validity: Validity },
}

/// The jobs asked for and not yet back.
struct Jobs {
    waiting: BTreeMap<JobId, For, Global>,
    next: u64,
    asked: Vec<Job, Global>,
}

impl Jobs {
    fn ask(&mut self, work: Work, what: For) -> JobId {
        let id = JobId(self.next);
        self.next += 1;
        self.waiting.insert(id, what);
        self.asked.push(Job { id, work });
        id
    }
}

struct Stopping {
    reason: String,
    by: Duration,
    closed: bool,
}

/// The explorer's server: see the module's doc.
pub struct Serving {
    token: Token,
    origins: Origins,
    name: &'static str,
    every: Duration,
    core: Result<CoreWasm, String>,
    served: Arc<Served>,
    rotation: Rotation,
    now: Duration,
    sessions: BTreeMap<SessionId, Open, Global>,
    requests: BTreeMap<StreamRef, Request, Global>,
    /// Each open live stream's session.
    lanes: BTreeMap<StreamRef, SessionId, Global>,
    jobs: Jobs,
    finished: VecDeque<(JobId, Done), Global>,
    said: Vec<String, Global>,
    check_at: Duration,
    certifying: bool,
    stopping: Option<Stopping>,
    base: u64,
    next_tag: u64,
    /// Timers in flight: each one's due time.
    timers: BTreeMap<u64, Duration, Global>,
    armed: Option<Duration>,
    /// Streams found late, its buffer kept.
    late: Vec<StreamRef, Global>,
    #[cfg(test)]
    plant: Plant,
    #[cfg(test)]
    dones: u32,
}

impl Serving {
    /// The server of `setup`, naming its timers from `base`, started at `clock`.
    pub fn new(setup: Setup, base: u64, clock: Clock) -> Serving {
        let Setup { token, origins, name, every, served, rotation, core } = setup;
        assert!(
            !every.is_zero(),
            "the explorer's live period (`explore::Config::every`) is zero: a live stream would tick without end"
        );
        Serving {
            token,
            origins,
            name,
            every,
            core,
            served: Arc::new_in(served, Global),
            rotation,
            now: clock.now,
            sessions: BTreeMap::new_in(Global),
            requests: BTreeMap::new_in(Global),
            lanes: BTreeMap::new_in(Global),
            jobs: Jobs { waiting: BTreeMap::new_in(Global), next: 0, asked: Vec::new_in(Global) },
            finished: VecDeque::new_in(Global),
            said: Vec::new_in(Global),
            check_at: clock.now.saturating_add(CHECK),
            certifying: false,
            stopping: None,
            base,
            next_tag: 0,
            timers: BTreeMap::new_in(Global),
            armed: None,
            late: Vec::new_in(Global),
            #[cfg(test)]
            plant: Plant::None,
            #[cfg(test)]
            dones: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    /// Whether `tag` names one of this machine's timers.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0 >= self.base && tag.0 - self.base < TAGS
    }

    /// Takes the completion of one of its timers ([`Serving::owns`]).
    pub fn complete(&mut self, completion: Completion) {
        let Completion { tag, done } = completion;
        let Some(due) = self.timers.remove(&tag.0) else {
            panic!("the explorer's serving core: {tag:?} names no timer of its in flight")
        };
        match done {
            blaze_core::Done::Fired => {
                if self.armed == Some(due) {
                    self.armed = None;
                }
            }
            done => panic!(
                "the explorer's serving core: its timer {tag:?} completed with {done:?}, which blaze-core's timer never gives"
            ),
        }
    }

    /// Takes job `id`'s result; the next step carries it out.
    pub fn done(&mut self, id: JobId, done: Done) {
        self.finished.push_back((id, done));
    }

    /// Stops the server at `now`: the next step closes every session with `reason`.
    pub fn stop(&mut self, now: Duration, reason: String) {
        if self.stopping.is_none() {
            self.stopping = Some(Stopping { reason, by: now.saturating_add(SHUTDOWN), closed: false });
        }
    }

    /// The certificate served now.
    pub fn served(&self) -> &Arc<Served> {
        &self.served
    }

    /// Moves the jobs asked for to `out`.
    pub fn take_jobs(&mut self, out: &mut Vec<Job>) {
        out.append(&mut self.jobs.asked);
    }

    /// Moves the lines to say (on stderr) to `out`.
    pub fn take_said(&mut self, out: &mut Vec<String>) {
        out.append(&mut self.said);
    }

    /// Until when a poll may wait for this machine: its armed timer, or none.
    pub fn deadline(&self) -> Deadline {
        self.armed.map_or(Deadline::NEVER, Deadline::at)
    }

    /// Whether a stopping machine is done, and why.
    pub fn over(&self, endpoint: &Endpoint) -> Option<Over> {
        let stopping = self.stopping.as_ref()?;
        if !stopping.closed {
            return None;
        }
        if endpoint.all_closed() {
            Some(Over::Closed)
        } else if self.now >= stopping.by {
            Some(Over::Bound)
        } else {
            None
        }
    }

    /// The time is `clock`: the jobs' results are carried out, the endpoint's events taken,
    /// late requests refused, the live streams ticked, the certificate checked, and the next
    /// deadline armed as a timer in `out`. The endpoint's clock is the caller's to advance.
    pub fn step(&mut self, clock: Clock, endpoint: &mut Endpoint, out: &mut Vec<Effect>) {
        self.now = self.now.max(clock.now);
        if let Some(stopping) = &mut self.stopping
            && !stopping.closed
        {
            stopping.closed = true;
            endpoint.close_all(session::CLOSE_STOPPED, &stopping.reason);
        }
        while let Some((id, done)) = self.finished.pop_front() {
            self.finish(id, done, endpoint);
        }
        self.events(clock, endpoint);
        self.expire(endpoint);
        self.feed(clock);
        self.check(clock);
        // What this step's sends made the endpoint say (a stream sent, a session closed).
        self.events(clock, endpoint);
        self.arm(out);
    }

    /// Every event the endpoint has, including those the handling of one makes.
    fn events(&mut self, clock: Clock, endpoint: &mut Endpoint) {
        while let Some(event) = endpoint.next_event() {
            match event {
                Event::SessionRequest { session, request } => self.asked_for(session, &request, endpoint),
                Event::StreamOpened { session, stream } => self.opened(session, stream, clock, endpoint),
                Event::StreamData { stream, bytes } => {
                    endpoint.consumed(stream, bytes.len());
                    self.data(stream, &bytes, endpoint);
                }
                Event::StreamFinished { stream } => self.read(stream, None, endpoint),
                Event::StreamReset { stream, code } => self.read(stream, Some(code), endpoint),
                Event::StreamStopped { stream, .. } => self.stopped(stream, endpoint),
                Event::StreamSent { stream } => self.sent(stream, endpoint),
                Event::StreamAcked { stream } => self.acked(stream, endpoint),
                Event::StreamsAvailable { session } => self.open_lane(session, endpoint),
                Event::SessionClosed { session, .. } => self.closed(session),
                // The page sends no datagram, and a server is told of no session it opened;
                // a connection's end comes as its sessions' ends, and a request the core
                // refused itself (a bad handshake, a plain request) was answered by it.
                Event::Datagram { .. }
                | Event::SessionOpened { .. }
                | Event::ConnectionClosed { .. }
                | Event::Refused { .. }
                | Event::Ticket { .. } => {}
                Event::Request { .. }
                | Event::ResponseHead { .. }
                | Event::ResponseData { .. }
                | Event::RequestEnded { .. } => unreachable!(
                    "the explorer's endpoint takes no plain request (`ServerConfig::requests` is false) and asks none"
                ),
            }
        }
    }

    fn asked_for(&mut self, session: SessionId, request: &SessionRequest, endpoint: &mut Endpoint) {
        let status = if self.stopping.is_some() {
            Some(503)
        } else if request.path != link::PATH {
            Some(404)
        } else if !self.origins.allows(request.origin.as_deref()) {
            Some(403)
        } else {
            None
        };
        if let Some(status) = status {
            // A session already gone (its connection closed) needs no answer.
            _ = endpoint.refuse(session, status);
            return;
        }
        match endpoint.accept(session) {
            Ok(()) => {
                let open = Open { core: session::Session::new(self.name), order: VecDeque::new_in(Global), live: None };
                self.sessions.insert(session, open);
            }
            Err(why) => self.said.push(format!("{}: a session from {} was not accepted: {why}", self.name, request.peer)),
        }
    }

    fn opened(&mut self, session: SessionId, stream: StreamRef, clock: Clock, endpoint: &mut Endpoint) {
        let Some(open) = self.sessions.get_mut(&session) else {
            // A stream of a session not accepted: nothing of it is read.
            _ = endpoint.stop(stream, 0);
            return;
        };
        if stream.direction() == Direction::Uni {
            // The page opens no unidirectional stream: one is stopped, not read.
            _ = endpoint.stop(stream, 0);
            return;
        }
        match open.core.opened(clock.now_ns) {
            Ok(slot) => {
                open.order.push_back(stream);
                let due = self.now.saturating_add(session::REQUEST_WAIT);
                let at = At::Reading { bytes: Vec::new_in(Global), due };
                self.requests.insert(stream, Request { session, slot: Some(slot), at });
            }
            Err(refused) => {
                _ = endpoint.stop(stream, 0);
                let at = At::Writing { close: None };
                self.requests.insert(stream, Request { session, slot: None, at });
                self.write(stream, &session::error_bytes(&refused), endpoint);
            }
        }
    }

    fn data(&mut self, stream: StreamRef, bytes: &[u8], endpoint: &mut Endpoint) {
        let Some(request) = self.requests.get_mut(&stream) else { return };
        let At::Reading { bytes: read, .. } = &mut request.at else { return };
        if read.len() + bytes.len() <= session::MAX_REQUEST {
            read.extend_from_slice(bytes);
            return;
        }
        request.at = At::Read(Err(session::too_long()));
        _ = endpoint.stop(stream, 0);
        let session = request.session;
        self.advance(session, endpoint);
    }

    /// The page ended its side of `stream`: finished (`reset` `None`), or reset with the code.
    fn read(&mut self, stream: StreamRef, reset: Option<Option<u32>>, endpoint: &mut Endpoint) {
        let Some(request) = self.requests.get_mut(&stream) else { return };
        let At::Reading { bytes, .. } = &mut request.at else { return };
        let bytes = std::mem::replace(bytes, Vec::new_in(Global));
        request.at = At::Read(match reset {
            None => Ok(bytes),
            Some(Some(code)) => Err(session::unread(format_args!("the page reset it with code {code}"))),
            Some(None) => Err(session::unread("the page reset it")),
        });
        let session = request.session;
        self.advance(session, endpoint);
    }

    fn stopped(&mut self, stream: StreamRef, endpoint: &mut Endpoint) {
        if let Some(session) = self.lanes.remove(&stream) {
            if let Some(lane) = self.sessions.get_mut(&session).and_then(|it| it.live.as_mut()) {
                lane.at = LaneAt::Over;
            }
            return;
        }
        // The page dropped the request: its answer is not written, and its source's work is
        // told to stop (the `Stop`'s sender goes with it).
        _ = endpoint.stop(stream, 0);
        self.end(stream, endpoint);
    }

    fn sent(&mut self, stream: StreamRef, endpoint: &mut Endpoint) {
        if let Some(session) = self.lanes.get(&stream) {
            if let Some(lane) = self.sessions.get_mut(session).and_then(|it| it.live.as_mut()) {
                lane.unsent = false;
            }
            return;
        }
        // An answer that closes its session ends when the page has all of it ([`Serving::acked`]).
        if matches!(self.requests.get(&stream).map(|it| &it.at), Some(At::Writing { close: None })) {
            self.end(stream, endpoint);
        }
    }

    /// The page acknowledged every byte of `stream` and its FIN: an answer that closes its
    /// session ends now, and the close goes out after it.
    fn acked(&mut self, stream: StreamRef, endpoint: &mut Endpoint) {
        if matches!(self.requests.get(&stream).map(|it| &it.at), Some(At::Writing { close: Some(_) })) {
            self.end(stream, endpoint);
        }
    }

    fn closed(&mut self, session: SessionId) {
        let Some(open) = self.sessions.remove(&session) else { return };
        // Its streams have had their last event: what is left of its requests ends with it
        // (their slots with its core, their stops with them).
        self.requests.retain(|_, it| it.session != session);
        if let Some(Lane { at: LaneAt::Open(stream), .. }) = open.live {
            self.lanes.remove(&stream);
        }
    }

    /// Request `stream` ended: its slot given back, and its session closed if its answer said
    /// to (by then the page has the answer, or stopped it: see [`Serving::sent`]).
    fn end(&mut self, stream: StreamRef, endpoint: &mut Endpoint) {
        let Some(Request { session, slot, at }) = self.requests.remove(&stream) else { return };
        if let Some(open) = self.sessions.get_mut(&session) {
            if let Some(slot) = slot {
                open.core.ended(slot);
            }
            if let Some(at) = open.order.iter().position(|it| *it == stream) {
                open.order.remove(at);
            }
        }
        if let At::Writing { close: Some((code, reason)) } = at {
            // A session already gone needs no close.
            _ = endpoint.close(session, code, reason);
        }
        self.advance(session, endpoint);
    }

    /// Decides what of `session`'s requests can be: before a good hello only its oldest, after
    /// it every one read.
    fn advance(&mut self, session: SessionId, endpoint: &mut Endpoint) {
        // Bounded by the session's requests (at most MAX_IN_FLIGHT): each turn moves on one.
        let mut at = 0;
        loop {
            let Some(open) = self.sessions.get(&session) else { return };
            let Some(&stream) = open.order.get(at) else { return };
            let oldest_only = open.core.phase() == Phase::Hello;
            if matches!(self.requests.get(&stream).map(|it| &it.at), Some(At::Read(_))) {
                self.decide(stream, endpoint);
            }
            if oldest_only {
                return;
            }
            at += 1;
        }
    }

    /// Carries out the session core's step for request `stream`, which is read.
    fn decide(&mut self, stream: StreamRef, endpoint: &mut Endpoint) {
        let Some(request) = self.requests.get_mut(&stream) else { return };
        let At::Read(read) = std::mem::replace(&mut request.at, At::Writing { close: None }) else {
            unreachable!("the explorer's serving core decides only a request it read")
        };
        let session = request.session;
        let started_ns = request.slot.as_ref().map_or(0, Slot::started_ns);
        let Some(open) = self.sessions.get_mut(&session) else { return };
        let (step, bytes) = match read {
            Ok(bytes) => (open.core.request(&self.token, &bytes), bytes),
            Err(why) => (Step::Refuse(why), Vec::new_in(Global)),
        };
        match step {
            Step::Hello { first } => {
                let job = self.jobs.ask(Work::Hello, For::Hello { stream, session, first });
                self.set(stream, At::Asked { job, stop: None });
            }
            Step::Op { name } => {
                let (stop, stopping) = crossbeam_channel::bounded::<()>(0);
                let work = Work::Op { name, request: bytes, started_ns, stop: stopping };
                let job = self.jobs.ask(work, For::Op { stream });
                self.set(stream, At::Asked { job, stop: Some(stop) });
            }
            Step::Core => {
                let bytes = session::core_bytes(&self.core);
                self.write(stream, &bytes, endpoint);
            }
            Step::Refuse(error) => self.write(stream, &session::error_bytes(&error), endpoint),
            Step::Close { error, code } => {
                self.set(stream, At::Writing { close: Some((code, error.kind)) });
                self.write(stream, &session::error_bytes(&error), endpoint);
            }
        }
    }

    fn set(&mut self, stream: StreamRef, at: At) {
        if let Some(request) = self.requests.get_mut(&stream) {
            request.at = at;
        }
    }

    /// Hands `bytes` and FIN of request `stream` (which is [`At::Writing`]) to the endpoint;
    /// the request ends at [`Event::StreamSent`], or now when the stream is gone.
    fn write(&mut self, stream: StreamRef, bytes: &[u8], endpoint: &mut Endpoint) {
        let written = endpoint.send(stream, bytes).and_then(|()| endpoint.finish(stream));
        if written.is_err() {
            self.end(stream, endpoint);
        }
    }

    /// Whether request `stream` still waits on job `id`; if it does, it is now writing.
    fn take_answer(&mut self, stream: StreamRef, id: JobId) -> bool {
        let Some(request) = self.requests.get_mut(&stream) else { return false };
        if !matches!(request.at, At::Asked { job, .. } if job == id) {
            return false;
        }
        request.at = At::Writing { close: None };
        true
    }

    fn finish(&mut self, id: JobId, done: Done, endpoint: &mut Endpoint) {
        let Some(what) = self.jobs.waiting.remove(&id) else {
            panic!("the explorer's serving core: {id:?} names no job of its in flight")
        };
        #[cfg(test)]
        {
            self.dones += 1;
            if self.plant == (Plant::DropDone { nth: self.dones }) {
                return;
            }
        }
        match (what, done) {
            (For::Hello { stream, session, first }, done) => {
                let bytes = match done {
                    Done::Hello(hello) => {
                        session::hello_bytes(&hello, &self.served, self.every, self.core.as_ref().ok())
                    }
                    Done::Failed(error) => session::error_bytes(&error),
                    done => mismatch("hello", &done),
                };
                if self.take_answer(stream, id) {
                    self.write(stream, &bytes, endpoint);
                }
                if first {
                    self.want_lane(session, endpoint);
                }
            }
            (For::Op { stream }, done) => {
                let bytes = match done {
                    Done::Op(bytes) => bytes,
                    Done::Failed(error) => session::error_bytes(&error),
                    done => mismatch("an op's", &done),
                };
                if self.take_answer(stream, id) {
                    self.write(stream, &bytes, endpoint);
                }
            }
            (For::Tick { session }, Done::Tick { feeding, read }) => self.frames(session, feeding, read, endpoint),
            (For::Tick { session }, Done::Failed(error)) => {
                if let Some(lane) = self.sessions.get_mut(&session).and_then(|it| it.live.as_mut()) {
                    if let LaneAt::Open(stream) = lane.at {
                        self.lanes.remove(&stream);
                        _ = endpoint.reset(stream, 0);
                    }
                    lane.at = LaneAt::Over;
                }
                self.said.push(format!("{}: a session's live stream ended: {}", self.name, error.message));
            }
            (For::Cert { validity }, Done::Cert(made)) => {
                self.certifying = false;
                match made.map_err(|why| why.to_string()) {
                    Ok((identity, hash)) => match endpoint.set_identity(identity) {
                        Ok(()) => {
                            self.rotation.rotated(validity);
                            let served = Served { hash, not_after_s: validity.not_after_s };
                            self.served = Arc::new_in(served, Global);
                            self.tell_cert(endpoint);
                        }
                        Err(why) => self.not_rotated(&why.to_string()),
                    },
                    Err(why) => self.not_rotated(&why),
                }
            }
            (For::Cert { .. }, Done::Failed(error)) => {
                self.certifying = false;
                self.not_rotated(&error.message);
            }
            (For::Tick { .. }, done) => mismatch("a tick's", &done),
            (For::Cert { .. }, done) => mismatch("a certificate's", &done),
        }
    }

    fn not_rotated(&mut self, why: &str) {
        self.said.push(format!(
            "{}: the certificate was not made anew ({why}); the one served expires at {} (Unix seconds) and a new one is tried in {} minutes",
            self.name,
            self.rotation.current().not_after_s,
            CHECK.as_secs() / 60
        ));
    }

    /// The frames of a tick of `session`'s feed.
    fn frames(&mut self, session: SessionId, feeding: Box<Feeding>, read: Result<(), OpError>, endpoint: &mut Endpoint) {
        let Some(lane) = self.sessions.get_mut(&session).and_then(|it| it.live.as_mut()) else { return };
        let LaneAt::Open(stream) = lane.at else { return };
        let (mut sent, mut failed) = (false, false);
        match &read {
            Ok(()) => {
                for table in &feeding.tick.tables {
                    let ipc: &[u8] = match lane.feed.emit(table.name, table.rule, table.rows) {
                        Emit::Nothing => continue,
                        Emit::Rows => &table.ipc,
                        Emit::Cleared => &[],
                    };
                    let header = session::live_header(table.name, table.rows, &feeding.tick);
                    sent = true;
                    if !send_frame(endpoint, stream, &mut lane.head, &header, ipc) {
                        failed = true;
                        break;
                    }
                }
            }
            Err(error) => {
                sent = true;
                failed = !send_frame(endpoint, stream, &mut lane.head, &session::live_error_header(error), &[]);
            }
        }
        lane.feeding = Some(feeding);
        lane.unsent |= sent;
        if failed {
            lane.at = LaneAt::Over;
            self.lanes.remove(&stream);
        }
    }

    /// A `cert` frame to every live stream, owed to each not yet open.
    fn tell_cert(&mut self, endpoint: &mut Endpoint) {
        let header = session::cert_header(&self.served);
        for open in self.sessions.values_mut() {
            let Some(lane) = &mut open.live else { continue };
            match lane.at {
                LaneAt::Wanted => lane.cert_owed = true,
                LaneAt::Open(stream) => {
                    lane.unsent = true;
                    if !send_frame(endpoint, stream, &mut lane.head, &header, &[]) {
                        lane.at = LaneAt::Over;
                        self.lanes.remove(&stream);
                    }
                }
                LaneAt::Over => {}
            }
        }
    }

    fn want_lane(&mut self, session: SessionId, endpoint: &mut Endpoint) {
        let Some(open) = self.sessions.get_mut(&session) else { return };
        if open.live.is_some() {
            return;
        }
        open.live = Some(Lane {
            at: LaneAt::Wanted,
            feed: Feed::default(),
            feeding: Some(Box::new_in(Feeding::default(), Global)),
            next: self.now,
            unsent: false,
            cert_owed: false,
            head: Vec::new_in(Global),
        });
        self.open_lane(session, endpoint);
    }

    fn open_lane(&mut self, session: SessionId, endpoint: &mut Endpoint) {
        let Some(lane) = self.sessions.get_mut(&session).and_then(|it| it.live.as_mut()) else { return };
        if !matches!(lane.at, LaneAt::Wanted) {
            return;
        }
        match endpoint.open(session, Direction::Uni) {
            Ok(Some(stream)) => {
                lane.at = LaneAt::Open(stream);
                lane.next = self.now;
                self.lanes.insert(stream, session);
                if std::mem::take(&mut lane.cert_owed) {
                    lane.unsent = true;
                    if !send_frame(endpoint, stream, &mut lane.head, &session::cert_header(&self.served), &[]) {
                        lane.at = LaneAt::Over;
                        self.lanes.remove(&stream);
                    }
                }
            }
            // Event::StreamsAvailable says when to ask again.
            Ok(None) => {}
            Err(_) => lane.at = LaneAt::Over,
        }
    }

    /// Refuses each request not whole by its due.
    fn expire(&mut self, endpoint: &mut Endpoint) {
        let mut late = std::mem::replace(&mut self.late, Vec::new_in(Global));
        late.clear();
        late.extend(self.requests.iter().filter_map(|(stream, request)| match &request.at {
            At::Reading { due, .. } if *due <= self.now => Some(*stream),
            _ => None,
        }));
        for stream in late.drain(..) {
            _ = endpoint.stop(stream, 0);
            let Some(request) = self.requests.get_mut(&stream) else { continue };
            request.at = At::Read(Err(session::late()));
            let session = request.session;
            self.advance(session, endpoint);
        }
        self.late = late;
    }

    /// A tick's job for each live stream that is due, open, sent and home.
    fn feed(&mut self, clock: Clock) {
        for (session, open) in &mut self.sessions {
            let Some(lane) = &mut open.live else { continue };
            if !matches!(lane.at, LaneAt::Open(_)) || lane.unsent || lane.next > self.now {
                continue;
            }
            let Some(feeding) = lane.feeding.take() else { continue };
            lane.next = next_tick(lane.next, self.now, self.every);
            self.jobs.ask(Work::Tick { feeding, at_ns: clock.now_ns }, For::Tick { session: *session });
        }
    }

    fn check(&mut self, clock: Clock) {
        if self.certifying || self.stopping.is_some() || self.now < self.check_at {
            return;
        }
        self.check_at = self.now.saturating_add(CHECK);
        if let Some(validity) = self.rotation.due(clock.wall_s) {
            self.certifying = true;
            self.jobs.ask(Work::Cert { validity, name: self.name }, For::Cert { validity });
        }
    }

    /// Arms a timer for the next deadline, unless one no later is armed.
    fn arm(&mut self, out: &mut Vec<Effect>) {
        #[cfg(test)]
        if self.plant == Plant::NoRearm {
            return;
        }
        let mut due = self.check_at;
        if let Some(stopping) = &self.stopping {
            due = due.min(stopping.by);
        }
        for request in self.requests.values() {
            if let At::Reading { due: by, .. } = &request.at {
                due = due.min(*by);
            }
        }
        for open in self.sessions.values() {
            if let Some(Lane { at: LaneAt::Open(_), unsent: false, feeding: Some(_), next, .. }) = &open.live {
                due = due.min(*next);
            }
        }
        if self.armed.is_some_and(|armed| armed <= due) {
            return;
        }
        assert!(self.next_tag < TAGS, "the explorer's serving core named {TAGS} timers from base {}: its tags are spent", self.base);
        let tag = self.base + self.next_tag;
        self.next_tag += 1;
        self.timers.insert(tag, due);
        out.push(Effect::Timer { after: due.saturating_sub(self.now), tag: Tag(tag) });
        self.armed = Some(due);
    }
}

/// The first multiple of `every` after `next` that is past `now`: the ticks missed between are
/// skipped.
fn next_tick(next: Duration, now: Duration, every: Duration) -> Duration {
    let behind = now.saturating_sub(next).as_nanos() / every.as_nanos();
    let at = next.as_nanos().saturating_add((behind + 1).saturating_mul(every.as_nanos()));
    Duration::from_nanos(u64::try_from(at).unwrap_or(u64::MAX))
}

/// One live frame on `stream`, its head made in `head`: whether the endpoint took it.
fn send_frame<H: Serialize + ?Sized>(
    endpoint: &mut Endpoint,
    stream: StreamRef,
    head: &mut Vec<u8, Global>,
    header: &H,
    ipc: &[u8],
) -> bool {
    head.clear();
    frame::live_head(head, header, ipc.len()).expect("a live frame's header of strings and integers always serializes");
    endpoint.send(stream, head).is_ok() && (ipc.is_empty() || endpoint.send(stream, ipc).is_ok())
}

fn mismatch(what: &str, done: &Done) -> ! {
    let got = match done {
        Done::Hello(_) => "hello",
        Done::Op(_) => "an answer",
        Done::Tick { .. } => "a tick",
        Done::Cert(_) => "a certificate",
        Done::Failed(_) => "a failure",
    };
    panic!("the explorer's serving core: {what} job came back as {got}, which its driver never gives")
}
