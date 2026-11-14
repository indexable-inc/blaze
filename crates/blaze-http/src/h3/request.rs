//! Plain HTTP/3 requests: what a client asks ([`Request`], [`Early`]) and hears
//! ([`ResponseHead`], [`RequestEnd`]), and what a server's asker is handed ([`Served`]). The
//! machinery is the endpoint's (`endpoint/requests.rs`); this module is the vocabulary.
//!
//! # What the core does
//!
//! A client's request is one bidirectional stream: a HEADERS frame (QPACK with the static table
//! and literals only, so no dynamic table is ever needed: SETTINGS advertise none) and its FIN.
//! The fields are `:method`, `:scheme` (`https`), `:authority`, `:path`, `user-agent`
//! ([`USER_AGENT`], the same for every machine and user) and, for a ranged read, `range`. The
//! answer comes as events: [`super::Event::ResponseHead`] once (its status, content-length,
//! content-range and etag), [`super::Event::ResponseData`] as the body comes, read no more than
//! [`super::WINDOW`] bytes ahead of the asker ([`super::Endpoint::consumed_response`]), and one
//! [`super::Event::RequestEnded`]. A body past what the asker said it expects ([`Length`]), or
//! short of it at FIN, ends the request [`RequestEnd::Failed`] naming the object.
//!
//! Every request ends exactly once: [`RequestEnd`] says how, and which ends mean the server
//! never started it ([`RequestEnd::NotStarted`]: a GOAWAY that excludes its stream,
//! `H3_REQUEST_REJECTED`, or a connection that ended before it was sent). Asking again is the
//! asker's decision; the core never does.
//!
//! # Early data
//!
//! With a session ticket ([`super::Ticket`], [`super::Endpoint::add_ticket`]) a connection
//! resumes, and requests asked with [`super::Endpoint::request_early`] before its handshake
//! completes go in its first flight, as 0-RTT early data. Early data can be replayed by anyone
//! who saw it, so only a request that is safe to replay may go: [`Early`] is made only by
//! [`Early::get`] and [`Early::head`], and [`super::Endpoint::request`] (any method) holds its
//! request until the handshake is done. If the server refuses the early data, QUIC discards
//! every stream that carried it, and the core sends each of those requests again once, after
//! the handshake: that is QUIC's own rule for rejected 0-RTT (RFC 9001, 4.6.2), not a retry;
//! the server never saw them.
//!
//! # The floor, and this design's cost (rule 67)
//!
//! - **Round trips to the first byte, from a cold process.** The floor is 0 with a resumed
//!   session (the request rides the first flight; the answer comes in the first round trip,
//!   which no design avoids) and 1 without (the handshake's). This design: 0 with a ticket the
//!   driver kept from an earlier process and a server that takes early data, 1 without, which
//!   is the floor in both cases. A ticket the server refuses costs the same 1, the floor
//!   without one.
//! - **Framing bytes per small ranged read.** The floor is what HTTP/3 itself needs to name an
//!   object and a range: a HEADERS frame of the method, scheme, authority, path and range, and
//!   on the answer a HEADERS frame of the status and length and one DATA frame header; QUIC's
//!   own stream framing is the same for any design. Here a GET of `/<64 hex>` with a range is
//!   HEADERS of 2 (type, length) + 2 (QPACK prefix) + 1 (`:method GET`, static) + 1 (`:scheme
//!   https`, static) + about 2 + authority (literal, static name) + about 2 + path (literal) +
//!   about 2 + 22 (`range`, literal, static name) + about 2 + 5 (`user-agent: blaze`): about 40
//!   bytes past the authority and path, which any HTTP/3 client sends. The answer's HEADERS is
//!   the server's; each DATA frame adds 2 to 9 bytes. With the dynamic table (refused here,
//!   for its state and its blocked streams) the authority and user-agent would cost 1 byte each
//!   from the second request on: at most the authority's length per request above the floor,
//!   under 10% of a request for a 64-hex path and a short host, and nothing next to the bytes
//!   of any object read.

#![blaze::sans_io]

use std::{net::SocketAddr, time::Duration};

use super::{ConnId, Error};

/// The `user-agent` of every request: no version, machine or user, so a request says nothing
/// about who sent it beyond its address.
pub const USER_AGENT: &str = "blaze";
/// The most bytes in one DATA frame a server's answer is cut into. 16 KiB keeps the frame's
/// header at 3 bytes in 5 thousand and is well under the window, so a body flows as soon as the
/// client takes its first frame.
pub const DATA_FRAME: usize = 16 << 10;

/// A request, numbered by its connection; never reused on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId {
    pub conn: ConnId,
    pub n: u64,
}

/// Bytes `at..at + len` of an object; `len` is at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub at: u64,
    pub len: u64,
}

impl Range {
    /// The `range` field's value.
    pub fn header(self) -> String {
        format!("bytes={}-{}", self.at, self.at + self.len - 1)
    }

    /// The range a `range` field names, if it is one closed byte range (`bytes=a-b`, `a <= b`).
    pub fn parse(text: &str) -> Option<Range> {
        let (first, last) = text.strip_prefix("bytes=")?.split_once('-')?;
        let (at, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
        (at <= last).then_some(Range { at, len: last - at + 1 })
    }
}

/// How long the asker expects a 2xx answer's body to be: the bound the core holds it to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Length {
    /// Exactly this many bytes: fewer at FIN is an error.
    Exactly(u64),
    /// At most this many.
    AtMost(u64),
}

/// A request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: http::Method,
    pub authority: String,
    /// `:path`, with the query.
    pub path: String,
    pub range: Option<Range>,
    /// The body's bound; a HEAD's body is empty whatever this says.
    pub length: Length,
    /// When the request ends [`RequestEnd::TimedOut`] if it has not ended, on the endpoint's
    /// clock (since its epoch).
    pub deadline: Duration,
}

impl Request {
    /// What errors about the request name: the object, as a URL.
    pub fn object(&self) -> String {
        format!("https://{}{}", self.authority, self.path)
    }
}

/// A request that may go as 0-RTT early data: a GET or a HEAD, the only methods made here,
/// since early data can be replayed (RFC 8470, RFC 9114 4.1.1 on idempotent requests).
#[derive(Clone, Debug)]
pub struct Early(Request);

impl Early {
    /// A GET of `path` at `authority`, of `range` if given, its body bounded by `length`.
    pub fn get(
        authority: String,
        path: String,
        range: Option<Range>,
        length: Length,
        deadline: Duration,
    ) -> Early {
        Early(Request { method: http::Method::GET, authority, path, range, length, deadline })
    }

    /// A HEAD of `path` at `authority`.
    pub fn head(authority: String, path: String, deadline: Duration) -> Early {
        Early(Request {
            method: http::Method::HEAD,
            authority,
            path,
            range: None,
            length: Length::Exactly(0),
            deadline,
        })
    }

    pub fn request(&self) -> &Request {
        &self.0
    }

    /// The same request with its deadline moved (a driver that counts from its own epoch).
    pub fn with_deadline(mut self, deadline: Duration) -> Early {
        self.0.deadline = deadline;
        self
    }

    pub(crate) fn into_request(self) -> Request {
        self.0
    }
}

/// The head of an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
    pub etag: Option<String>,
}

/// How a request ended: exactly one per request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestEnd {
    /// The whole answer came, its body within its bounds. A status that is not 2xx ends here
    /// right after its head; its body is not read.
    Done,
    /// The answer broke HTTP/3 or its bounds: names the object and the bound.
    Failed(Error),
    /// The asker cancelled it ([`super::Endpoint::cancel`]).
    Cancelled,
    /// Its deadline passed.
    TimedOut,
    /// The server reset the answer with this HTTP/3 code; it may have acted on the request.
    Reset { code: u64 },
    /// The server did not start it, and says so (a GOAWAY that excludes its stream,
    /// `H3_REQUEST_REJECTED`), or it was never sent: asking again is safe.
    NotStarted { why: String },
    /// Its connection ended after it was sent: the server may or may not have started it.
    Lost(Error),
}

/// A request a server's asker is handed ([`super::Event::Request`]); answer it with
/// [`super::Endpoint::respond`].
#[derive(Clone, Debug)]
pub struct Served {
    pub peer: SocketAddr,
    pub method: String,
    pub authority: String,
    pub path: String,
    /// The `range` field, as sent ([`Range::parse`] reads the one form a client here sends).
    pub range: Option<String>,
    /// Whether it came before this server's handshake completed: in 0-RTT early data.
    pub early: bool,
}
