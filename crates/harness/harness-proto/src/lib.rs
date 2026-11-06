//! The one schema harness speaks: between `harnessd` and every client (the native app's daemon link, the TUI and CLI,
//! the Bun ts kernel in `mcp/`), and in every chat's log. One version, [`SCHEMA`], for all of it.
//!
//! - [`cbor`]: deterministic CBOR, the only encoding: map keys sorted by their encoded bytes, shortest integers and
//!   floats, definite lengths only, and an integral float written as the integer it is (dCBOR's numeric reduction), so
//!   Rust and the kernel's JavaScript write the same bytes for the same value.
//! - [`frame`]: the socket's frames, `[type u8][len u32 BE][payload]`: [`frame::T_CBOR`] a message ([`msg::Up`] from a
//!   client, [`msg::Down`] from the daemon), [`frame::T_EVENT`] a chat event ([`event::Event`]), [`frame::T_DATA`] raw
//!   terminal bytes. Every connection starts with the client's [`msg::Up::Hello`] naming [`SCHEMA`]; the daemon answers
//!   with its own [`msg::Down::Hello`], or an error naming both versions, and closes.
//! - [`event`]: a chat's agent-neutral events (the table is in crates/harness/harness/src/chat/mod.rs).
//! - [`log`]: a chat's log, `objects/<id>/chat`: [`log::MAGIC`], then the [`frame::T_EVENT`] frames the socket carried,
//!   appended. A replay sends a log's frames as they are, so it is the same bytes as the live feed.
//! - [`migrate`]: the one-time conversion of a chat log from the NDJSON of before [`SCHEMA`] 1, as a sans-IO step list the
//!   daemon performs at start, proven by an exhaustive crash test.
//!
//! - [`browser`]: the embedded browser host's messages (harness-browser): its socket's for the kernel, its Mach
//!   channel's for the app.
//!
//! The kernel mirrors these types once, in `mcp/src/wire.ts`, and `mcp/src/wire.test.ts` round-trips this crate's
//! fixtures (`fixtures/`, written by this crate's `fixtures_are_current` test) through it byte for byte.
pub mod browser;
pub mod cbor;
pub mod event;
#[cfg(test)]
mod fixtures;
pub mod frame;
pub mod log;
pub mod migrate;
pub mod msg;
pub mod peek;

/// The schema's version: in every connection's hello and in every chat log's [`log::MAGIC`]. A change to any type here
/// that an older reader would decode differently bumps it (and the log magic with it, which makes the daemon convert old
/// logs at start, as [`migrate`] does for NDJSON).
pub const SCHEMA: u32 = 1;
