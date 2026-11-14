//! HTTP/3 and WebTransport, sans-IO: [`Endpoint`], a synchronous state machine over
//! quinn-proto's QUIC core, and its driver on blaze-core's effects ([`driver`]: datagram
//! batches and a timer, on io_uring, kqueue or the simulation). The design is
//! docs/v2/plan/design/http3-sans-io.md.
//!
//! # The core
//!
//! [`Endpoint`] serves WebTransport sessions, connects to them, or both. On each QUIC
//! connection it speaks the HTTP/3 a WebTransport session needs and nothing more: the control
//! streams and their SETTINGS, QPACK with the static table only (a dynamic table is refused by
//! advertising none), the extended CONNECT that opens a session,
//! the session's bidirectional and unidirectional streams, its datagrams, and its close
//! capsule. It speaks both WebTransport drafts Chrome ships (02 and 07) with the settings
//! wtransport 0.7.2 sent. A request that is not a WebTransport CONNECT is answered 400 or 501
//! and reported ([`Event::Refused`]), unless the server was made to take requests
//! ([`ServerConfig::requests`]).
//!
//! It also makes plain requests (GET, HEAD, with a range) and answers them, with session
//! resumption and 0-RTT early data for the requests that may be replayed: the [`request`]
//! module says what, and the floor each is held to. That is what the remote cache reads public
//! objects with (`blaze-remote`'s `http3`).
//!
//! # The driver contract
//!
//! This is the seam a driver is written against; [`driver`] is the one in the tree.
//!
//! 1. **Clock.** [`Endpoint::new`] takes the instant the driver's clock counts from. Before
//!    handing the core anything, and whenever [`Endpoint::next_deadline`] passes, call
//!    [`Endpoint::advance_clock`] with the time since then, from a monotonic clock.
//! 2. **Datagrams in.** Each UDP datagram read goes to [`Endpoint::received`] with the address
//!    it came from. The socket is the driver's; one socket serves every connection.
//! 3. **Datagrams out.** After every call into the core, drain [`Endpoint::next_transmit`] and
//!    send each [`Transmit`] to its address. A send that fails is dropped, as the network
//!    may: QUIC retransmits.
//! 4. **Asks.** [`Endpoint::accept`] or [`Endpoint::refuse`] each [`Event::SessionRequest`];
//!    [`Endpoint::connect`], [`Endpoint::open`], [`Endpoint::send`], [`Endpoint::finish`],
//!    [`Endpoint::reset`], [`Endpoint::stop`], [`Endpoint::send_datagram`],
//!    [`Endpoint::close`], [`Endpoint::set_identity`] and [`Endpoint::close_all`] as the
//!    asker wants; [`Endpoint::consumed`] as the asker takes each [`Event::StreamData`] (the
//!    core reads no stream more than [`WINDOW`] bytes ahead of its asker). For requests:
//!    [`Endpoint::connect_requests`], [`Endpoint::request`], [`Endpoint::request_early`],
//!    [`Endpoint::cancel`], and [`Endpoint::consumed_response`] as each
//!    [`Event::ResponseData`] is taken; a server [`Endpoint::respond`]s to each
//!    [`Event::Request`] and may [`Endpoint::go_away`].
//! 5. **Events.** Drain [`Endpoint::next_event`] and hand each to the asker of its session,
//!    stream or request.
//! 6. **Tickets.** Before connecting, [`Endpoint::add_ticket`] each ticket kept from an
//!    earlier process; keep each [`Event::Ticket`] (its bytes are a secret: owner-only, as
//!    credentials are).
//! 7. **Waits.** Sleep until the next datagram, ask or [`Endpoint::next_deadline`], whichever
//!    is first. Every connection ends by its idle timeout ([`Transport::idle`]), a session
//!    whose peer sends no SETTINGS fails at [`SETTINGS_WAIT`], and a request ends by its own
//!    deadline ([`Request::deadline`]), so no wait is open.
//!
//! A caller that runs the core inside its own state machine needs only datagrams and a clock
//! from its driver: the [`Wire`] trait is that seam, and the caller does the steps above.
//!
//! [`driver`]'s one wait is its poll of blaze-core's driver, to the endpoint's next deadline
//! (armed as a timer); a caller blocking on a handle's future parks in
//! [`crate::mailbox::block_on`], named by what it waits for (every-wait-says-what).
//!
//! # What the core reads that is not its input
//!
//! quinn-proto draws connection IDs, tokens and its packet-number skips from `rand`'s thread
//! RNG, and rustls draws its handshake randomness and reads the wall clock through its own
//! `TimeProvider`. So the bytes on the wire differ from run to run; what the core decides
//! (sessions, streams, events, errors, deadlines) does not, given the same inputs and clock.
//! [`check_pinned`] takes the time as an argument; the client's verifier reads rustls's clock
//! to give it one.

// Each core module (`endpoint` and its `requests`, `request`, `frame`, `qpack`, `huffman`,
// `varint`, `config`) is marked
// `#![blaze::sans_io]` itself: this module is not, since its children are part of a marked module
// and `driver` is the driver.

mod config;
pub mod driver;
mod endpoint;
mod frame;
mod huffman;
mod qpack;
pub mod request;
mod varint;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;

pub use config::{
    ALPN, ClientConfig, Identity, MOST_TICKET_SERVERS, MOST_TICKETS_PER_SERVER, MOST_VALID,
    ServerConfig, Ticket, Transport, Trust, check_pinned, server_tls,
};
pub use endpoint::{
    Closed, ConnId, Direction, Endpoint, Event, LINGER, MOST_HELD, SETTINGS_WAIT, SessionId,
    SessionRequest, StreamRef, Transmit, WINDOW,
};
pub use frame::Settings;
pub use request::{
    DATA_FRAME, Early, Length, Range, Request, RequestEnd, RequestId, ResponseHead, Served,
    USER_AGENT,
};

/// A step of an endpoint's work: what an [`Error`] was about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Making the endpoint's TLS or QUIC config.
    Config,
    /// The QUIC and TLS handshake, the certificate check included.
    Handshake,
    /// The control streams and their SETTINGS.
    Settings,
    /// A session's CONNECT and its answer.
    Connect,
    /// A session after it opened, its close included.
    Session,
    /// A stream: its header, its frames, or an ask about it.
    Stream,
    /// A datagram.
    Datagram,
    /// A plain request and its answer.
    Request,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Step::Config => "the endpoint's config",
            Step::Handshake => "the QUIC handshake",
            Step::Settings => "the HTTP/3 SETTINGS",
            Step::Connect => "the WebTransport CONNECT",
            Step::Session => "the WebTransport session",
            Step::Stream => "a WebTransport stream",
            Step::Datagram => "a WebTransport datagram",
            Step::Request => "an HTTP/3 request",
        })
    }
}

/// What went wrong: with which peer, at which step, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The peer, when the error is about one.
    pub peer: Option<SocketAddr>,
    pub step: Step,
    pub why: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.peer {
            Some(peer) => write!(f, "{} with {peer}: {}", self.step, self.why),
            None => write!(f, "{}: {}", self.step, self.why),
        }
    }
}

impl std::error::Error for Error {}

/// What a wait on a [`Wire`] ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waited {
    /// At least one datagram came, and each was handed over.
    Received,
    /// The wait's deadline came first.
    Deadline,
}

/// The network and the clock, and nothing else: what a caller that runs an [`Endpoint`] in
/// its own state machine (blaze-remote's HTTP/3 bucket) needs from a driver. A driver
/// implements it over its UDP socket and monotonic clock; a test implements it over a
/// simulated network and a virtual clock, with the peer endpoint inside.
///
/// The caller does the rest of the driver contract itself: it feeds [`Wire::wait`]'s datagrams
/// to [`Endpoint::received`], moves [`Endpoint::next_transmit`] onto [`Wire::send`], and calls
/// [`Endpoint::advance_clock`] with [`Wire::now`].
pub trait Wire {
    /// The instant the clock counts from: what the caller's [`Endpoint::new`] is given.
    fn epoch(&self) -> std::time::Instant;
    /// The time since [`Wire::epoch`], monotonic.
    fn now(&mut self) -> std::time::Duration;
    /// Sends one datagram. One that cannot be sent is dropped, as the network may drop it:
    /// QUIC sends it again.
    fn send(&mut self, transmit: Transmit);
    /// Waits until a datagram comes or the clock reaches `until`, whichever is first, and
    /// hands each datagram that came (its sender and bytes) to `received`. `until` is the
    /// wait's bound: a wire never waits past it. (A driver opens a span naming the wait's
    /// peer for it, every-wait-says-what.)
    fn wait(
        &mut self,
        until: std::time::Duration,
        received: &mut dyn FnMut(SocketAddr, &[u8]),
    ) -> Waited;
}
