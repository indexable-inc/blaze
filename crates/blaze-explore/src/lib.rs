//! The explorer's daemon side: the daemon as a source of ix-prof's explorer
//! (`ix_prof::explore`, the HTTP/3 WebTransport server https://ix.dev/explore connects to on
//! 127.0.0.1; docs/v2/plan/design/explore-generic.md). `blaze explore` starts it in the
//! daemon. This crate is the `blaze` layer alone: the transport, the certificate, the link, the
//! token, `hello` and the live stream's framing are ix-prof's.
//!
//! - [`wire`]: the `blaze` layer's ops as the page asks them, and its kinds of error.
//! - [`tiles`]: the spans of a range per (process, thread, depth, pixel-wide bucket).
//! - `ops`: the answer to each op, with no transport in it.
//! - `source`: the daemon's `ix_prof::explore::Source` over the history, the build world,
//!   blaze-profile's flight recorder and the daemon's tasks, and [`start`].
//! - [`tasks`]: the plane's tasks and the downloads as the `tasks` and `task_edges` tables.

#![forbid(unsafe_code)]
#![feature(allocator_api)]

mod ops;
mod source;
pub mod tasks;
pub mod tiles;
pub mod wire;

pub use ix_prof::explore::{CoreWasm, Explorer, StartError};
pub use ops::{Flush, History};
pub use source::{CORE_BUILD, CORE_FILE, Config, LAYERS, NAME, core_file, start};
pub use tasks::Tasks;
