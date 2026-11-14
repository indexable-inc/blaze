//! blaze's Postgres client, sans-IO: the frontend/backend protocol 3.0 as a synchronous state
//! machine ([`Connection`]) that owns no socket, thread, timer, clock or randomness, for a
//! driver to do the IO for (one on bstd's `net` is to be written against the contract below;
//! none is here). It is blaze-sql's Postgres engine (docs/v2/plan/design/sql.md): a checked
//! statement is prepared once per connection and the server's own description of it is held to
//! the checker's; a statement whose description differs is never executed.
//!
//! # The core
//!
//! [`Connection`] writes the startup message (protocol 3.0, `user`, `database`,
//! `client_encoding=UTF8`) as it is made; answers `AuthenticationOk`, a cleartext password
//! request, or SASL with SCRAM-SHA-256 ([`scram`]: client-first with the caller's nonce,
//! server-first, client-final with the proof, and the server's signature verified); refuses
//! MD5, Kerberos, GSSAPI, SSPI and channel binding by name; and says [`Event::Ready`] at the
//! first `ReadyForQuery`. Then three extended-protocol rounds pipeline, each behind the ones
//! before it. [`Connection::prepare`] sends `Parse` under a server-side name, `Describe` and
//! `Sync`; the server's `ParameterDescription` and `RowDescription` are compared with the
//! statement's parameter types ([`ParamType`]) and expected columns ([`Expect`]), and the
//! outcome is [`Event::Prepared`] or [`Event::PrepareFailed`], a difference naming the
//! parameter or column and both types. [`Connection::query`] executes a prepared statement:
//! `Bind` (binary both ways), `Execute`, `Sync`, each row checked against the prepared columns.
//! [`Connection::close_statement`] sends `Close` and `Sync`. Values go both ways in
//! [`codec`]'s binary format: `boolean`, `bigint`, `double precision`, `text`, `bytea` and
//! their one-dimensional arrays. Bytes and time go in; bytes to send, [`Event`]s and the next
//! deadline come out.
//!
//! Every wait is a deadline on the injected clock: startup's ([`Config::startup_deadline`]) and
//! each prepare's and query's. One not ended when the clock reaches its deadline fails, and
//! the connection closes, since it may hold half that round's answer; every round behind it
//! fails naming that cause. Every buffer has a bound: a message is at most [`MESSAGE_MOST`]
//! (lower before startup ends, [`STARTUP_MESSAGE_MOST`]), the pipeline at most
//! [`PIPELINE_MOST`] rounds, and the bytes held for events not taken at most [`HELD_MOST`]
//! beyond the message still arriving. The statements a connection holds have no bound here;
//! see step 6.
//!
//! # The driver contract
//!
//! 1. **Connect.** Open a TCP (or Unix socket) connection to the server, then make the
//!    [`Connection`] with the [`Config`] and 18 bytes from the system's random source (SCRAM's
//!    nonce; the core has no randomness of its own).
//! 2. **Clock.** Before handing the core anything, and whenever [`Connection::next_deadline`]
//!    passes, call [`Connection::advance_clock`] with the time since the driver started, from a
//!    monotonic clock. The core never reads a clock; a deadline never reached is a query that
//!    never ends.
//! 3. **Send.** After every call into the core, while [`Connection::next_send`] has bytes,
//!    write them and say how many went with [`Connection::sent`] (any number up to all of them).
//! 4. **Receive.** Keep one read in flight; give what it read to [`Connection::received`] as it
//!    came (any split is fine), and a read of nothing (the peer closed) or a broken socket to
//!    [`Connection::closed`].
//! 5. **Events.** After every call into the core, drain [`Connection::next_event`] and hand each
//!    event to its asker: [`Event::Ready`] once; per prepare one [`Event::Prepared`] or
//!    [`Event::PrepareFailed`]; per query its [`Event::Row`]s and one [`Event::Done`] or
//!    [`Event::Failed`]; all in the order they were sent; [`Event::Notice`] to whoever logs
//!    them. An event borrows the connection until the next call into it. On [`Event::Close`],
//!    drop the socket: nothing comes after it, and the core takes nothing more (what it is
//!    then given is dropped, a call is refused).
//! 6. **Asks.** After [`Event::Ready`]: [`Connection::prepare`] each checked statement once
//!    per connection under a [`StatementId`] the driver picks, unique among those the
//!    connection holds; after its [`Event::Prepared`], [`Connection::query`] it any number of
//!    times with a [`QueryId`] unique among the queries in flight (a query on a statement whose
//!    prepare has not ended, or failed, is refused). A prepare may be sent while queries on
//!    other statements are in flight. A statement whose prepare failed is never executed; it
//!    holds its id until [`Connection::close_statement`]. The core keeps no bound on the
//!    statements a connection holds: what each costs is the server's memory (a parsed plan per
//!    statement per backend), which the core cannot see, and which ones to keep is the
//!    driver's policy (most recently used, or every statement of a program), so the driver
//!    bounds them and closes the ones it lets go.
//! 7. **Waits.** Sleep until the socket is readable or writable, an ask comes, or
//!    [`Connection::next_deadline`], whichever is first. Every query ends by its deadline, so
//!    no wait of an asker's is open. The waits open no span yet: when `blaze-trace` lands
//!    (docs/v2/plan/design/no-tracing.md) the driver opens one per wait, named by the server
//!    and the query (every-wait-says-what).
//!
//! # Not here
//!
//! TLS: the connection speaks the protocol in the clear (fine on a Unix socket or loopback).
//! The plan is blaze-http's: rustls's own sans-IO `Connection` between the driver's socket and
//! this core, after Postgres's `SSLRequest` (or a direct TLS start, Postgres 17's
//! `sslnegotiation=direct`), with SCRAM-SHA-256-PLUS channel binding over it. Until then a
//! server that offers only channel binding is refused by name. Also not here: SASLprep (a
//! password that is not printable ASCII is refused naming the rule), cancel requests (the
//! [`BackendKey`] is kept for them), `COPY`, `LISTEN`, and the simple query protocol.

#![no_std]
#![feature(allocator_ext)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod codec;
mod conn;
pub mod error;
mod proto;
pub mod scram;

#[cfg(test)]
mod tests;

pub use conn::{
    BackendKey, Bound, Columns, Config, Connection, Event, Expect, HELD_MOST, PIPELINE_MOST,
    ParamType, Values,
};
pub use error::{
    AuthError, Error, Fatal, ProtocolError, QueryId, Refused, Returns, Round, ServerError, Stage,
    State, StatementId, Text, What,
};
pub use proto::{MESSAGE_MOST, STARTUP_MESSAGE_MOST, backend_name};
