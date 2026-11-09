//! The explorer's serving side (feature `explore`): the HTTP/3 WebTransport server on 127.0.0.1
//! that https://ix.dev/explore connects to, for any program that has a [`Source`]
//! (docs/v2/plan/design/explore-generic.md). The blaze daemon is one source (crate
//! blaze-explore); this module knows nothing of builds.
//!
//! - [`link`]: the token, the origins accepted and the link the user opens, which carries the
//!   port, the token and the certificate's hash in its fragment (never sent to ix.dev).
//! - [`cert`]: the self-signed ECDSA P-256 certificate the page pins by hash, valid 13 days
//!   (WebTransport's `serverCertificateHashes` takes at most 14), and when it is made anew.
//! - [`frame`]: what the server sends: a header padded so the Arrow IPC stream after it starts
//!   8-aligned, then one IPC stream, then FIN; and the live stream's frames.
//! - [`session`]: the sans-IO core of a session: hello first, the token, the requests in
//!   flight, the live stream's emit rule, and the bytes of every header; an answer's Arrow IPC
//!   is [`frame`]'s ([`frame::answer_bytes`]), outside the core.
//! - [`Source`]: what a program serves: its `hello` fields, its live feed and its ops.
//! - [`wasm`]: the explorer core a program serves the page ([`CoreWasm`]): `hello`'s `core`
//!   and the `core` op's bytes.
//! - [`serve`]: the driver: the transport, the sessions and the live stream, on a thread of its
//!   own with a one-thread tokio runtime. A source's answers (and its live ticks) run on tokio's
//!   blocking pool, so a slow one never holds up the live stream or another request.

pub mod cert;
pub mod frame;
pub mod link;
mod server;
pub mod session;
mod source;
pub mod wasm;
#[cfg(test)]
mod tests;

pub use server::{CHECK, Config, Explorer, SHUTDOWN, StartError, serve};
pub use source::{
    Answer, Hello, Kind, Layer, Live, LiveTable, OpError, Rule, Source, Stop, Table, Tick,
};
pub use wasm::CoreWasm;

/// The version of the protocol this server speaks, as `hello`'s `protocol` says it: the core's
/// tables, frames and ops, apart from any layer's (each layer has its own version, in `hello`'s
/// `layers`). Adding a nullable column or a table changes no version.
pub const PROTOCOL: u32 = 1;
