//! blaze's own HTTP/2 (RFC 9113) with HPACK (RFC 7541), as a sans-IO machine: no dependency but
//! blaze-own, `no_std`, no IO, no thread, no global state. Bytes go in ([`Connection::received`]),
//! bytes come out ([`Connection::transmit`]), events come out ([`Connection::poll_event`]) and the
//! time goes in: every timeout is a deadline the caller is told to wake at
//! ([`Connection::next_deadline`], then [`Connection::timed_out`]).
//!
//! The client role is the first user (the remote cache's many small record fetches over one TLS
//! connection where UDP, and so HTTP/3, is blocked); the server role is the same machine with the
//! other preface ([`Connection::server`]), which the tests wire back to back with a client.
//!
//! Layers, each its own module and each quoting the RFC sections it follows:
//! - [`hpack`]: prefix integers, the Huffman code, the static and dynamic tables, the decoder and
//!   the encoder (RFC 7541);
//! - [`frame`]: the frame header and every frame's payload (RFC 9113 section 4 and 6);
//! - [`settings`]: the SETTINGS values, the RFC's limits on them, and ours ([`Limits`]);
//! - [`stream`]: the stream state machine of RFC 9113 section 5.1, as a pure transition function;
//! - [`head`]: field names and values (section 8.2) and the pseudo-headers (section 8.3);
//! - [`conn`]: the connection: preface, SETTINGS exchange, flow control, the stream table, the
//!   caps that stop the known attacks, deadlines.
//!
//! Trust: every peer input is held to the caller's [`Limits`] before it is buffered; a peer that
//! exceeds one gets a typed stream or connection error with the RFC's error code, never unbounded
//! memory. Nothing here makes the bytes trusted: a record fetched over it is checked against its
//! digest by the caller.

#![no_std]
#![cfg_attr(test, feature(allocator_api))]
// `#![blaze::sans_io]` in the modules names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod conn;
mod error;
pub mod frame;
pub mod head;
pub mod hpack;
mod plant;
pub mod settings;
pub mod stream;
mod time;

#[cfg(test)]
mod tests;

pub use conn::{Connection, Event, ResetBy, Role, Windows};
pub use error::{ErrorCode, Fault, H2Error, Refusal, Rule};
pub use frame::{FrameKind, StreamId};
pub use head::{Field, Fields, HeadFault, HeadWhy, RequestHead, ResponseHead};
pub use settings::{Limits, LimitsFault, Settings};
pub use stream::State;
pub use time::Instant;
