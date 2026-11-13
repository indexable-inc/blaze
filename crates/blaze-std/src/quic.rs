//! QUIC as a sans-IO core (docs/v2/plan/design/std.md, `quic`; docs/v2/plan/design/http3-sans-io.md):
//! blaze-http's [`Endpoint`], quinn-proto's QUIC under rustls with the HTTP/3 a WebTransport
//! session needs on top, re-exported. It owns no socket, thread, timer or clock: it takes no
//! `Cx` and no capability, so it cannot wait or touch the world.
//!
//! - **In:** the time, as the duration since the epoch the endpoint was made with
//!   ([`Endpoint::advance_clock`]); each UDP datagram with its sender ([`Endpoint::received`]);
//!   and the asker's asks (accept, refuse, connect, open, send, finish, reset, stop, consumed,
//!   send a datagram, close).
//! - **Out:** datagrams to send ([`Transmit`]), events for the asker ([`Event`]: sessions,
//!   streams and their bytes, datagrams), and when to advance the clock next
//!   ([`Endpoint::next_deadline`]). [`drain`] takes all three after a step.
//!
//! What blaze-http has is this endpoint: QUIC carrying WebTransport over HTTP/3. Bare QUIC
//! streams, with no HTTP/3 under them, are quinn-proto's and are not exposed by blaze-http, so
//! not here either. The driver that runs an endpoint over a `Net` datagram socket and the
//! `Clock` waits on blaze-core's datagram effects, which do not exist yet ([`crate::net`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::{alloc::Allocator, time::Duration};

use crate::mem::{Named, OutOfMemory, Vec};

pub use blaze_http::h3::{
    ALPN, ClientConfig, Closed, ConnId, Direction, Endpoint, Error, Event, Identity, LINGER,
    MOST_HELD, MOST_VALID, SETTINGS_WAIT, ServerConfig, SessionId, SessionRequest, Settings, Step,
    StreamRef, Transmit, Transport, WINDOW, check_pinned,
};

/// Everything `endpoint` has to send into `transmits` and every event into `events`, in the
/// order it made them; then when it next needs [`Endpoint::advance_clock`], on its epoch's
/// scale (`None`: not until a datagram or an ask comes). A driver calls it after each call into
/// the core; the containers, and so their allocator, are the caller's.
///
/// Room for each is made before it is taken from the endpoint, so when a container has no
/// room ([`OutOfMemory`]) nothing is lost: what was not taken stays in the endpoint for the
/// next drain, and what was taken is in the containers.
pub fn drain<A: Allocator + Named, B: Allocator + Named>(
    endpoint: &mut Endpoint,
    transmits: &mut Vec<Transmit, A>,
    events: &mut Vec<Event, B>,
) -> Result<Option<Duration>, OutOfMemory> {
    loop {
        transmits.reserve(1)?;
        let Some(transmit) = endpoint.next_transmit() else { break };
        // Within the room just made: it does not grow.
        transmits.push(transmit)?;
    }
    loop {
        events.reserve(1)?;
        let Some(event) = endpoint.next_event() else { break };
        // Within the room just made: it does not grow.
        events.push(event)?;
    }
    Ok(endpoint.next_deadline())
}
