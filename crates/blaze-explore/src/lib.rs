//! The explorer's daemon side: the HTTP/3 WebTransport server `blaze explore` starts in the
//! daemon, which https://ix.dev/explore connects to on 127.0.0.1 (docs/v2/plan/lanes/observe/
//! explore.md; the wire is /tmp/blaze-lanes/explore-wire.md until it moves to docs/).
//!
//! - [`link`]: the token, the origins accepted and the link the user opens, which carries the
//!   port, the token and the certificate's hash in its fragment (never sent to ix.dev).
//! - [`cert`]: the self-signed ECDSA P-256 certificate the page pins by hash, valid 13 days
//!   (WebTransport's `serverCertificateHashes` takes at most 14), and when it is made anew.
//! - [`wire`]: what the page asks (one JSON object a bidirectional stream) and the typed errors.
//! - [`frame`]: what the daemon answers: a header padded so the Arrow IPC stream after it starts
//!   8-aligned, then one IPC stream, then FIN; and the live stream's frames.
//! - [`tiles`]: the spans of a range per (process, thread, depth, pixel-wide bucket).
//! - `ops`: the answer to each op, with no transport in it; [`server`]: the transport, the
//!   sessions and the live stream.
//!
//! The server runs on a thread of its own with a one-thread tokio runtime: the daemon's workers
//! never wait on a page, and the work of an answer (DuckDB, Arrow encoding) runs on tokio's
//! blocking pool, so a slow query never holds up the live stream or another query.

#![feature(random)]
#![forbid(unsafe_code)]

pub mod cert;
pub mod frame;
pub mod link;
mod ops;
pub mod server;
pub mod tiles;
pub mod wire;

pub use ops::History;
pub use server::{Config, Explorer, StartError, start};
