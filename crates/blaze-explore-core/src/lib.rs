// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core`; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// Only `abi` reads linear memory the page wrote; everything else is safe code.
#![deny(unsafe_code)]
//! The explorer's core (docs/v2/plan/design/explore-core.md): bytes from the daemon in, view
//! models out, the clock handed in. The page (indexable-inc/ix, packages/web) loads it compiled
//! to `wasm32-unknown-unknown` and keeps only the shell; the host runs the same code in its
//! tests.
//!
//! - [`wire`]: the daemon's framing, decoded exactly as blaze-explore's `frame` writes it: a
//!   `u32 LE` header length, the header's JSON padded with spaces, then an Arrow IPC stream or
//!   nothing; the live stream's frames are each a `u32 LE` length and then such an answer.
//!   `hello`, answers and live headers come out typed, ids, hashes and `_ns` as `u64`/`i64`.
//! - [`live`]: each live table's one long-lived IPC stream for the session, kept across frames
//!   and begun anew after the end-of-stream marker, and its rows as typed structs
//!   ([`live::OpenRow`], [`live::SpanRow`], [`live::CounterRow`], [`live::TaskRow`],
//!   [`live::EdgeRow`]).
//! - [`state`]: the live state the page needs: each thread's open span chain, its name and
//!   whether it is a pool worker, recent closed spans, the counters' latest values, and the
//!   live tasks; [`state::Session::look`] is the one model of the moment
//!   (`blaze_protocol::now::Now`, folded by `now::Fold`, as `blaze status` folds the plane).
//! - [`tasks`]: the `tasks` and `task_edges` rows, upserted, evicted 60 s after they settle,
//!   and folded through `now::Fold`; downloads as the look's downloads.
//! - [`view`]: view models, display-ready, in deterministic CBOR (blaze-protocol's codec):
//!   ids as decimal strings, durations as integer milliseconds.
//! - [`abi`]: the C ABI the page calls over linear memory, no wasm-bindgen.
//! - [`ipc`]: arrow-ipc's `StreamDecoder` as [`live::Stream`], the IPC decoder the drivers
//!   hand the cores, which take it as a type parameter.
//!
//! Every error names the message and the field it is about (`live frame for table `open`,
//! column `thread`: ...`), and nothing guesses a value the daemon did not send.

// The one module with unsafe code: it reads the linear memory the page wrote (`abi`'s own
// `#![allow(unsafe_code)]`, each use with its invariant).
pub mod abi;
pub mod ipc;
pub mod live;
pub mod state;
pub mod tasks;
pub mod view;
pub mod wire;

#[cfg(test)]
mod tests;

/// Why bytes from the daemon were refused: the message they came in, the part of it, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The message: `hello`, ``answer to `builds` ``, ``live frame for table `open` ``, `the
    /// live stream`, or a view's name.
    pub message: String,
    /// The part of it: ``field `at_ns` ``, ``column `thread` ``, `the IPC (stream 2)`.
    pub part: String,
    pub why: String,
}

impl Error {
    pub fn new(message: impl Into<String>, part: impl Into<String>, why: impl Into<String>) -> Error {
        Error { message: message.into(), part: part.into(), why: why.into() }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.part.is_empty() {
            write!(f, "{}: {}", self.message, self.why)
        } else {
            write!(f, "{}, {}: {}", self.message, self.part, self.why)
        }
    }
}

impl std::error::Error for Error {}

/// `field `name``, as errors name a header's field.
pub(crate) fn field(name: &str) -> String {
    // alloc: an error's text
    format!("field `{name}`")
}

/// `column `name``, as errors name a table's column.
pub(crate) fn column(name: &str) -> String {
    // alloc: an error's text
    format!("column `{name}`")
}
