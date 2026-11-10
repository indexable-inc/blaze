//! The client's core: one connection's requests and responses as a state machine that owns no
//! socket and reads no clock (rule sans-io-cores).
//!
//! Requests go in ([`Mux::send`]) and come out as the frames to write ([`Mux::outgoing`],
//! [`Mux::wrote`]); the daemon's bytes go in as they arrive, cut anywhere ([`Mux::received`]),
//! and come out as [`Event`]s ([`Mux::poll`]): each response under its request's id, then the
//! request's one end. Many requests are under way at once on the one connection, told apart by
//! id alone. Time goes in as the driver's clock reads it ([`Mux::tick`]), and the next moment
//! the core wants a turn comes out ([`Mux::next_deadline`]).
//!
//! A request ends once: with the daemon's `Done`, or with one [`Fault`]. A request whose
//! deadline passes ends with [`Fault::Deadline`] and is cancelled at the daemon
//! (`Request::Cancel`); what the daemon still says of it is dropped. A daemon of another schema,
//! a frame that is no envelope, a response to a request that is not under way, and a hang-up
//! each end every request under way with that fault, and the connection takes no more.
//!
//! `Request::Stop` is the one request the daemon says more of after its `Done`: each step of
//! its stop (`Response::Stopping`), then it hangs up. A stop that was told `Done { ok: true }`
//! ends when the daemon hangs up between frames, which is how a caller knows it is gone.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{collections::VecDeque, fmt};

use blaze_api::{
    ApiError, Envelope, Request, Response, SCHEMA,
    frames::{self, Frames},
};
use blaze_core::wait::Deadline;
use blaze_protocol::cbor;

/// What the core tells its driver.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// A response of request `id`, in the order the daemon sent that request's.
    Response { id: u64, response: Response },
    /// Request `id` is over: nothing more is told of it.
    Ended { id: u64, how: End },
}

/// How a request ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum End {
    /// The daemon's `Done`; `ok` false after an error or a failed build.
    Done { ok: bool },
    Failed(Fault),
}

/// Why a request, or the connection under it, ended with no `Done`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The daemon speaks another schema than this client: neither reads the other's bodies.
    Schema { ours: u32, theirs: u32 },
    /// Bytes refused: the daemon's that are no envelope of the API, or a request with no
    /// encoding.
    Refused(ApiError),
    /// The daemon answered request `id`, which is not under way on this connection.
    Unasked { id: u64 },
    /// The daemon hung up, `held` bytes into a frame (0 between frames).
    HungUp { held: usize },
    /// Request `id`, a `request` (the variant's name), had no end by its deadline.
    Deadline { id: u64, request: &'static str },
    /// The driver's read or write failed: what it said.
    Io { what: String },
    /// The client was closed with the request under way.
    Closed,
}

impl Fault {
    /// A number a foreign caller tells the faults apart by: stable, one a variant, from 1.
    pub fn code(&self) -> u32 {
        match self {
            Fault::Schema { .. } => 1,
            Fault::Refused(_) => 2,
            Fault::Unasked { .. } => 3,
            Fault::HungUp { .. } => 4,
            Fault::Deadline { .. } => 5,
            Fault::Io { .. } => 6,
            Fault::Closed => 7,
        }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Schema { ours, theirs } => write!(
                f,
                "the daemon speaks schema {theirs} of blaze's API and this client schema {ours}: they are of two \
                 builds of blaze, and neither reads the other (rule no-mcp: one versioned schema)"
            ),
            Fault::Refused(error) => match &error.at {
                Some(at) => write!(f, "{} (at {at})", error.what),
                None => f.write_str(&error.what),
            },
            Fault::Unasked { id } => write!(
                f,
                "the daemon sent a response to request {id}, which is not under way on this connection"
            ),
            Fault::HungUp { held: 0 } => f.write_str("the daemon hung up"),
            Fault::HungUp { held } => write!(f, "the daemon hung up {held} bytes into a frame"),
            Fault::Deadline { id, request } => {
                write!(f, "request {id} (`{request}`) had no end by its deadline, and was cancelled")
            }
            Fault::Io { what } => f.write_str(what),
            Fault::Closed => f.write_str("the client was closed with the request under way"),
        }
    }
}

impl std::error::Error for Fault {}

/// A request under way.
#[derive(Debug)]
struct Open {
    id: u64,
    deadline: Deadline,
    /// The variant's name, for [`Fault::Deadline`].
    request: &'static str,
    /// A `Request::Stop`: the daemon says more after its `Done`.
    stop: bool,
    /// A stop's `Done { ok: true }`, held until the daemon hangs up.
    stopped: bool,
}

/// A safeguard taken out, for the control of the test that proves the oracle.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A response is given to the oldest request under way, whatever id it names.
    ByArrival,
}

/// One connection's requests and responses.
#[derive(Debug)]
pub struct Mux {
    /// The daemon's bytes, cut into frames.
    frames: Frames,
    /// Every request is encoded with it, onto `out`.
    writer: cbor::Writer,
    /// The frames to write; `out[..written]` is written.
    out: Vec<u8>,
    written: usize,
    next_id: u64,
    /// The requests under way, in the order sent.
    open: Vec<Open>,
    /// Requests ended here before the daemon ended them (a deadline, a body refused) and the
    /// cancels sent for them: what the daemon says of each is dropped, up to its `Done`.
    quiet: Vec<u64>,
    events: VecDeque<Event>,
    /// Why the connection takes no more.
    broken: Option<Fault>,
    #[cfg(test)]
    plant: Plant,
}

impl Default for Mux {
    fn default() -> Mux {
        Mux::new()
    }
}

impl Mux {
    pub fn new() -> Mux {
        Mux {
            frames: Frames::new(),
            writer: cbor::Writer::new(),
            // alloc: none until the first request; kept for every one after.
            out: Vec::new(),
            written: 0,
            next_id: 1,
            // alloc: none until the first request.
            open: Vec::new(),
            // alloc: none until a request ends here first.
            quiet: Vec::new(),
            // alloc: none until the first event.
            events: VecDeque::new(),
            broken: None,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn planted(plant: Plant) -> Mux {
        Mux { plant, ..Mux::new() }
    }

    /// Takes `request`, to end by `deadline` on the driver's clock: its id (chosen here,
    /// increasing from 1), under which its responses and its end are told. Its frame is in
    /// [`Mux::outgoing`] from now. Refused, with nothing sent, on a connection that takes no
    /// more and for a request with no encoding.
    pub fn send(&mut self, request: &Request, deadline: Deadline) -> Result<u64, Fault> {
        if let Some(fault) = &self.broken {
            // clone: error path, a request to a connection that is over.
            return Err(fault.clone());
        }
        let id = self.next_id;
        frames::encode_into(&mut self.writer, &mut self.out, &Envelope::new(id, request))
            .map_err(Fault::Refused)?;
        self.next_id += 1;
        self.open.push(Open {
            id,
            deadline,
            request: named(request),
            stop: matches!(request, Request::Stop),
            stopped: false,
        });
        Ok(id)
    }

    /// The bytes to write to the daemon next; empty when every frame is written.
    pub fn outgoing(&self) -> &[u8] {
        &self.out[self.written..]
    }

    /// The first `count` bytes of [`Mux::outgoing`] were written.
    pub fn wrote(&mut self, count: usize) {
        self.written = (self.written + count).min(self.out.len());
        if self.written == self.out.len() {
            self.out.clear();
            self.written = 0;
        }
    }

    /// Takes the daemon's next bytes, cut anywhere: each whole frame in them becomes an event.
    pub fn received(&mut self, bytes: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        self.frames.push(bytes);
        // Bounded: each turn takes one whole frame of what is held, or ends.
        loop {
            let read = match self.frames.next_ref() {
                Ok(Some(item)) => frames::decode::<Response>(item).map_err(|refused| {
                    match frames::head(item) {
                        Ok((theirs, _)) if theirs != SCHEMA => {
                            (None, Fault::Schema { ours: SCHEMA, theirs })
                        }
                        _ => (refused.id, Fault::Refused(refused.error)),
                    }
                }),
                Ok(None) => return,
                Err(error) => Err((None, Fault::Refused(error))),
            };
            match read {
                Ok(envelope) => self.arrived(envelope.id, envelope.body),
                // A body that does not decode under an id that does: the frames go on, and
                // that request alone ends.
                Err((Some(id), fault)) if self.under_way(id).is_some() => {
                    self.quiet.push(id);
                    self.end(id, End::Failed(fault));
                }
                Err((_, fault)) => self.broke(fault),
            }
            if self.broken.is_some() {
                return;
            }
        }
    }

    /// The daemon hung up: every request under way ends, a stop that was told `Done` as done.
    pub fn hung_up(&mut self) {
        if self.broken.is_some() {
            return;
        }
        let held = self.frames.held();
        for open in std::mem::take(&mut self.open) {
            let how = match open.stopped && held == 0 {
                true => End::Done { ok: true },
                false => End::Failed(Fault::HungUp { held }),
            };
            self.events.push_back(Event::Ended { id: open.id, how });
        }
        self.over(Fault::HungUp { held });
    }

    /// The connection is over for `fault` (the driver's read or write failed, the client is
    /// closed, or what [`Mux::received`] found): every request under way ends with it.
    pub fn broke(&mut self, fault: Fault) {
        if self.broken.is_some() {
            return;
        }
        for open in std::mem::take(&mut self.open) {
            // clone: error path, once a request the fault ends.
            self.events.push_back(Event::Ended { id: open.id, how: End::Failed(fault.clone()) });
        }
        self.over(fault);
    }

    /// The driver's clock reads `now`: each request whose deadline has passed ends with
    /// [`Fault::Deadline`], and a cancel of it is in [`Mux::outgoing`].
    pub fn tick(&mut self, now: std::time::Duration) {
        if self.broken.is_some() {
            return;
        }
        // Bounded: each turn ends one request under way.
        while let Some(at) = self.open.iter().position(|it| it.deadline.passed(now)) {
            let Open { id, request, .. } = self.open.remove(at);
            self.events
                .push_back(Event::Ended { id, how: End::Failed(Fault::Deadline { id, request }) });
            self.quiet.push(id);
            let cancel = self.next_id;
            let sent = frames::encode_into(
                &mut self.writer,
                &mut self.out,
                &Envelope::new(cancel, Request::Cancel { id }),
            );
            match sent {
                Ok(()) => {
                    self.next_id += 1;
                    self.quiet.push(cancel);
                }
                Err(error) => return self.broke(Fault::Refused(error)),
            }
        }
    }

    /// When the core next wants [`Mux::tick`]: the first deadline of a request under way.
    pub fn next_deadline(&self) -> Option<Deadline> {
        self.open.iter().filter_map(|it| it.deadline.finite()).min()
    }

    /// The next event, in the order they came about.
    pub fn poll(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// How many requests are under way.
    pub fn open(&self) -> usize {
        self.open.len()
    }

    /// Why the connection takes no more, once it does not.
    pub fn broken(&self) -> Option<&Fault> {
        self.broken.as_ref()
    }

    fn under_way(&self, id: u64) -> Option<usize> {
        self.open.iter().position(|it| it.id == id)
    }

    /// A response the daemon sent under `id`.
    fn arrived(&mut self, id: u64, response: Response) {
        #[cfg(test)]
        let id = match (self.plant, self.open.first()) {
            (Plant::ByArrival, Some(oldest)) => oldest.id,
            _ => id,
        };
        if let Some(at) = self.quiet.iter().position(|&it| it == id) {
            if matches!(response, Response::Done { .. }) {
                self.quiet.swap_remove(at);
            }
            return;
        }
        let Some(at) = self.under_way(id) else { return self.broke(Fault::Unasked { id }) };
        match response {
            Response::Done { ok: true } if self.open[at].stop => self.open[at].stopped = true,
            Response::Done { ok } => self.end(id, End::Done { ok }),
            response => self.events.push_back(Event::Response { id, response }),
        }
    }

    /// Request `id`, under way, ends as `how`.
    fn end(&mut self, id: u64, how: End) {
        if let Some(at) = self.under_way(id) {
            self.open.remove(at);
            self.events.push_back(Event::Ended { id, how });
        }
    }

    /// Nothing more is taken or sent.
    fn over(&mut self, fault: Fault) {
        self.broken = Some(fault);
        self.quiet.clear();
        self.out.clear();
        self.written = 0;
    }
}

/// A request's variant, by name: what [`Fault::Deadline`] and a wait's span say it is.
pub fn named(request: &Request) -> &'static str {
    match request {
        Request::Commit { .. } => "Commit",
        Request::Check { .. } => "Check",
        Request::Build { .. } => "Build",
        Request::Test { .. } => "Test",
        Request::Run { .. } => "Run",
        Request::Diff { .. } => "Diff",
        Request::Why { .. } => "Why",
        Request::Status => "Status",
        Request::Cancel { .. } => "Cancel",
        Request::Open { .. } => "Open",
        Request::Close { .. } => "Close",
        Request::Reach { .. } => "Reach",
        Request::BestOf { .. } => "BestOf",
        Request::Verify { .. } => "Verify",
        Request::Snapshot => "Snapshot",
        Request::CodegenInputs { .. } => "CodegenInputs",
        Request::Trace { .. } => "Trace",
        Request::Tables { .. } => "Tables",
        Request::Stop => "Stop",
        Request::Edit { .. } => "Edit",
        Request::WillTest { .. } => "WillTest",
        Request::WillSave { .. } => "WillSave",
        Request::Memory => "Memory",
        Request::Doc { .. } => "Doc",
        Request::Explore { .. } => "Explore",
        Request::Profile { .. } => "Profile",
        Request::Live => "Live",
        Request::Flush => "Flush",
        Request::Hot { .. } => "Hot",
        Request::Gc { .. } => "Gc",
        Request::Rebuilt { .. } => "Rebuilt",
        Request::Launch { .. } => "Launch",
        Request::FetchUrl { .. } => "FetchUrl",
        Request::Lock { .. } => "Lock",
        Request::ClientRows { .. } => "ClientRows",
    }
}

#[cfg(test)]
mod tests;
