//! The explorer's daemon side: the daemon as a source of ix-prof's explorer
//! (`ix_prof::explore`, the HTTP/3 WebTransport server https://ix.dev/explore connects to on
//! 127.0.0.1; docs/v2/plan/observe/explore-generic.md). `blaze explore` starts it in the
//! daemon. This crate is the `blaze` layer alone: the transport, the certificate, the link, the
//! token, `hello` and the live stream's framing are ix-prof's.
//!
//! - [`wire`]: the `blaze` layer's ops as the page asks them, and its kinds of error.
//! - [`tiles`]: the spans of a range per (process, thread, depth, pixel-wide bucket).
//! - `ops`: the answer to each op, with no transport in it.
//! - `source`: the daemon's `ix_prof::explore::Source` over the history, the build world and
//!   blaze-profile's flight recorder, and [`start`].

#![forbid(unsafe_code)]
#![feature(allocator_api)]

mod ops;
mod source;
pub mod tiles;
pub mod wire;

pub use ix_prof::explore::{Explorer, StartError};
pub use ops::{Flush, History};
pub use source::{Config, LAYERS, NAME, start};
