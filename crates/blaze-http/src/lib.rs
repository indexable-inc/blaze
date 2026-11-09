//! blaze's HTTP, sans-IO: the HTTP/1.1 client, a synchronous state machine ([`Client`]) that
//! owns no socket, runtime, thread, timer or clock, and drivers that do the IO for it
//! (`tokio_driver` today; an io_uring driver, or one on `blaze-core`'s effects, later, with no
//! change here); and HTTP/3 with WebTransport ([`h3`]), a core of the same shape over
//! quinn-proto with its own driver contract and tokio driver (`h3::tokio_driver`).
//!
//! # The core
//!
//! [`Client`] speaks HTTP/1.1 over TLS (rustls's own sans-IO connection) or plain TCP: it writes
//! requests ([`h1`]), parses answers' heads with `httparse`, reads bodies framed by
//! `Content-Length`, `chunked` or the connection's close, decodes gzip where it asked for it,
//! holds each body to its bound, follows redirects (at most [`REDIRECTS`]), keeps connections
//! between requests in a pool keyed by origin (scheme, host, port) with an idle expiry and an
//! idle cap, goes through the proxy its config names for an origin ([`proxy`]: an HTTP
//! `CONNECT` tunnel, a request in absolute form, or SOCKS5 or SOCKS4), and turns every timeout
//! into a deadline on the clock it is given ([`Client::advance_clock`]). Bytes and completions
//! go in; IO to do ([`Io`]), events for the asker ([`Event`]) and the next deadline come out.
//!
//! # The driver contract
//!
//! This is the seam another driver (io_uring on Linux, `blaze-core`'s `Effect`s) is written
//! against; `tokio_driver` (the `tokio` feature) is the reference.
//!
//! 1. **Clock.** Before handing the core anything, and whenever [`Client::next_deadline`]
//!    passes, call [`Client::advance_clock`] with the time since the driver started, from a
//!    monotonic clock. The core never reads a clock; a deadline that is never reached is a
//!    request that never ends.
//! 2. **Asks.** [`Client::request`] with a [`RequestId`] the driver picks, unique among the
//!    requests in flight; [`Client::consumed`] as the asker takes each [`Event::Body`] (the
//!    core asks for no more of that body than [`WINDOW`] bytes ahead of the asker);
//!    [`Client::cancel`] when the asker goes away.
//! 3. **IO.** After every call into the core, drain [`Client::next_io`] and do each [`Io`]:
//!    - [`Io::Connect`]: resolve the host and open a TCP connection to it, then
//!      [`Completion::Connected`] or [`Completion::ConnectFailed`] (with why, from the
//!      system). TLS is the core's, and so is a proxy's handshake: the driver only moves
//!      bytes, and `host` is the proxy's when the connection goes through one.
//!    - [`Io::Resolve`]: resolve the name, then [`Completion::Resolved`] with its addresses
//!      or [`Completion::ResolveFailed`] (with why). Asked only for a proxy given the origin
//!      by its address (`socks4://`, `socks5://`); the connection's [`Io::Connect`] follows.
//!    - [`Io::Send`]: write all of the bytes, then [`Completion::Sent`] or
//!      [`Completion::Broken`]. At most one send per connection is in flight.
//!    - [`Io::Recv`]: read what the peer sent, up to any amount the driver picks (the tokio
//!      driver reads at most `tokio_driver::RECV_MOST`), then [`Completion::Received`] (empty
//!      when the peer closed) or [`Completion::Broken`]. At most one receive per connection is
//!      in flight. The core keeps one asked of every idle connection, so it hears when a
//!      server closes one.
//!    - [`Io::Close`]: drop the connection, abandoning what is in flight on it. No completion;
//!      one that arrives for it anyway is ignored, as is any for a connection the core no
//!      longer knows. Connection ids are never reused.
//! 4. **Events.** Drain [`Client::next_event`] and hand each to the asker of its request:
//!    [`Event::Head`] once, then [`Event::Body`] pieces, then [`Event::Done`]; or
//!    [`Event::Failed`] at any point. After `Done` or `Failed` the request is gone.
//! 5. **Waits.** Sleep until the next completion, ask or [`Client::next_deadline`], whichever
//!    is first. Every request ends by its deadline, so no wait of the asker's is open.
//!
//! The core has no recorder: the waits a driver does (a connect, a receive, the asker's wait
//! for its next event) open no span yet. When `blaze-trace` lands
//! (docs/v2/plan/design/no-tracing.md), the driver opens one per wait, named by the
//! connection's origin and the request (every-wait-says-what).
//!
//! # What the core reads that is not its input
//!
//! rustls checks a server's certificate against the wall clock through its own
//! `TimeProvider`; that is the one clock read under the core, and it is rustls's.
//!
//! [`roots`] reads the machine's root certificates, outside the core, once per client;
//! [`proxy_env`] the proxy variables, the same way.

#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod client;
mod gzip;
pub mod h1;
pub mod h3;
mod pool;
pub mod proxy;
pub mod proxy_env;
pub mod roots;
mod tls;
#[cfg(feature = "tokio")]
pub mod tokio_driver;

#[cfg(test)]
mod tests;

pub use client::{
    Client, Completion, Config, ConnId, Error, Event, IDLE_FOR, IDLE_MOST, Io, Kind, REDIRECTS,
    Request, RequestId, Step, WINDOW,
};
pub use proxy::{Proxies, Proxy, ProxyError, ProxyKind};
