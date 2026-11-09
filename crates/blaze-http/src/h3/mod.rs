//! HTTP/3 and WebTransport, sans-IO: [`Endpoint`], a synchronous state machine over
//! quinn-proto's QUIC core, and drivers that own the UDP socket and the timers (`tokio_driver`
//! today; io_uring later, with no change here). The design is
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
//! and reported ([`Event::Refused`]); an HTTP/3 page server is not this core's job.
//!
//! # The driver contract
//!
//! This is the seam another driver is written against; `tokio_driver` (the `tokio` feature) is
//! the reference.
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
//!    core reads no stream more than [`WINDOW`] bytes ahead of its asker).
//! 5. **Events.** Drain [`Endpoint::next_event`] and hand each to the asker of its session
//!    or stream.
//! 6. **Waits.** Sleep until the next datagram, ask or [`Endpoint::next_deadline`], whichever
//!    is first. Every connection ends by its idle timeout ([`Transport::idle`]), and a session
//!    whose peer sends no SETTINGS fails at [`SETTINGS_WAIT`], so no wait is open.
//!
//! The waits a driver does open no span yet: blaze-trace is not in the graph
//! (docs/v2/plan/design/no-tracing.md). When it lands, the driver opens one per wait, named
//! by the peer and the session or stream (every-wait-says-what).
//!
//! # What the core reads that is not its input
//!
//! quinn-proto draws connection IDs, tokens and its packet-number skips from `rand`'s thread
//! RNG, and rustls draws its handshake randomness and reads the wall clock through its own
//! `TimeProvider`. So the bytes on the wire differ from run to run; what the core decides
//! (sessions, streams, events, errors, deadlines) does not, given the same inputs and clock.
//! [`check_pinned`] takes the time as an argument; the client's verifier reads rustls's clock
//! to give it one.

// Each core module (`endpoint`, `frame`, `qpack`, `huffman`, `varint`, `config`) is marked
// `#![blaze::sans_io]` itself: this module is not, since its children are part of a marked module
// and `tokio_driver` is the driver.

mod config;
mod endpoint;
mod frame;
mod huffman;
mod qpack;
#[cfg(feature = "tokio")]
pub mod tokio_driver;
mod varint;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;

pub use config::{
    ALPN, ClientConfig, Identity, MOST_VALID, ServerConfig, Transport, check_pinned, server_tls,
};
pub use endpoint::{
    Closed, ConnId, Direction, Endpoint, Event, LINGER, MOST_HELD, SETTINGS_WAIT, SessionId,
    SessionRequest, StreamRef, Transmit, WINDOW,
};
pub use frame::Settings;

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
