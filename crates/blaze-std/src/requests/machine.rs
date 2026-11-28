//! Explicit sans-IO HTTP/1.1 exchange state machine over [`blaze_machine::Machine`] and
//! [`blaze_h1`].
//!
//! Decouples HTTP/1.1 request transmission, response framing (`Content-Length`, `Transfer-Encoding:
//! chunked`, close-delimited, empty/HEAD), byte-range verification (`206` + `Content-Range`),
//! bounded receive steps, and mandatory linear connection teardown (`HttpStepReq::Close`) from
//! blocking [`crate::net::Conn`] reads/writes:
//!
//! - Every state holds the opaque [`ConnHandle`] by value until transitioned through `Closing`.
//! - Every exit — `Ok`, protocol/timeout/transport `Err`, AND external [`blaze_machine::Stopped`]
//!   cancellation via [`Machine::on_cancel`] — emits [`HttpStepReq::Close`] before terminating.
//! - Zero ambient globals, `#![no_std]`, and explicit caller [`Alloc`] throughout (rule 11).

use core::{fmt, num::NonZeroU32, time::Duration};

use blaze_h1::{ContentRange, Range};
use blaze_machine::{
    CancelAction, Machine, RaceOutcome, RecordableRequest, Request as MachineRequest, Stopped, Turn,
};
use blaze_own::{Alloc, AllocFailure, Vec};

use super::Method;

/// Opaque generational handle to an open transport connection owned by an [`HttpExchangeMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnHandle {
    pub slot: u32,
    pub generation: NonZeroU32,
}

impl ConnHandle {
    #[inline]
    pub const fn new(slot: u32, generation: NonZeroU32) -> Self {
        Self { slot, generation }
    }
}

/// Transport-level failure reported by a [`blaze_machine::Handler`] driving [`HttpExchangeMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportError {
    /// Peer reset or closed the connection unexpectedly.
    Reset,
    /// Writing request bytes failed or made zero progress on a non-empty slice.
    WriteFailed,
    /// Reading response bytes failed.
    ReadFailed,
    /// Closing the connection handle failed.
    CloseFailed,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Reset => f.write_str("connection reset by peer"),
            TransportError::WriteFailed => f.write_str("transport write failed"),
            TransportError::ReadFailed => f.write_str("transport read failed"),
            TransportError::CloseFailed => f.write_str("transport close failed"),
        }
    }
}

impl core::error::Error for TransportError {}

/// Typed step request emitted by [`HttpExchangeMachine`].
#[must_use = "an HTTP step request must be performed by a Handler"]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpStepReq {
    /// Write bytes `[offset .. offset + len)` of the encoded request wire buffer to `conn`,
    /// bounded by `timeout`.
    Send {
        conn: ConnHandle,
        offset: usize,
        len: usize,
        timeout: Duration,
    },
    /// Receive up to `max_bytes` from `conn`, racing against `timeout`.
    RecvWithTimeout {
        conn: ConnHandle,
        max_bytes: usize,
        timeout: Duration,
    },
    /// Close `conn` linearly (emitted on `Ok`, `Err`, and `Stopped` cleanup paths).
    Close {
        conn: ConnHandle,
    },
}

/// Typed step answer delivered to [`HttpExchangeMachine::resume`].
///
/// `'s` borrows driver-owned scratch bytes for one receive step with zero extra copy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HttpStepAns<'s> {
    /// Outcome of [`HttpStepReq::Send`]: bytes written (may be short), transport error, or
    /// `RaceOutcome::Second(())` when the send timed out.
    Sent(RaceOutcome<Result<usize, TransportError>, ()>),
    /// Outcome of [`HttpStepReq::RecvWithTimeout`]: `First(Ok(chunk))` (`chunk.is_empty()` = peer
    /// EOF), `First(Err(err))` on read failure, or `Second(())` on receive timeout.
    RecvOrTimeout(RaceOutcome<Result<&'s [u8], TransportError>, ()>),
    /// Owned receive outcome replayed from a recorded [`HttpRecordedAns`] transcript without
    /// borrowing driver scratch.
    RecvOwned(RaceOutcome<Result<blaze_bytes::Bytes, TransportError>, ()>),
    /// Outcome of [`HttpStepReq::Close`].
    Closed(Result<(), TransportError>),
}

impl MachineRequest for HttpStepReq {
    type Answer<'s> = HttpStepAns<'s>;
}

/// Owned recorded form of [`HttpStepAns`] captured by [`blaze_machine::RecordHandler`] and
/// replayed deterministically through [`blaze_machine::Script`] with zero IO.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HttpRecordedAns {
    /// Recorded outcome of [`HttpStepReq::Send`].
    Sent(RaceOutcome<Result<usize, TransportError>, ()>),
    /// Recorded outcome of [`HttpStepReq::RecvWithTimeout`], retaining any received bytes in
    /// [`blaze_bytes::Bytes`].
    RecvOrTimeout(RaceOutcome<Result<blaze_bytes::Bytes, TransportError>, ()>),
    /// Recorded outcome of [`HttpStepReq::Close`].
    Closed(Result<(), TransportError>),
}

impl<'s> From<HttpRecordedAns> for HttpStepAns<'s> {
    #[inline]
    fn from(ans: HttpRecordedAns) -> Self {
        match ans {
            HttpRecordedAns::Sent(r) => HttpStepAns::Sent(r),
            HttpRecordedAns::RecvOrTimeout(r) => HttpStepAns::RecvOwned(r),
            HttpRecordedAns::Closed(r) => HttpStepAns::Closed(r),
        }
    }
}

impl RecordableRequest for HttpStepReq {
    type Recorded = HttpRecordedAns;

    fn record_answer(answer: &Self::Answer<'_>) -> Self::Recorded {
        match answer {
            HttpStepAns::Sent(r) => HttpRecordedAns::Sent(*r),
            HttpStepAns::RecvOrTimeout(r) => {
                let recorded = match r {
                    RaceOutcome::First(Ok(bytes)) if bytes.is_empty() => {
                        RaceOutcome::First(Ok(blaze_bytes::Bytes::from_static(b"")))
                    }
                    RaceOutcome::First(Ok(bytes)) => RaceOutcome::First(Ok(
                        blaze_bytes::Bytes::copy_kept(blaze_mem::Tag::Io, bytes),
                    )),
                    RaceOutcome::First(Err(err)) => RaceOutcome::First(Err(*err)),
                    RaceOutcome::Second(()) => RaceOutcome::Second(()),
                };
                HttpRecordedAns::RecvOrTimeout(recorded)
            }
            HttpStepAns::RecvOwned(r) => HttpRecordedAns::RecvOrTimeout(r.clone()),
            HttpStepAns::Closed(r) => HttpRecordedAns::Closed(*r),
        }
    }
}

/// Static-borrowed answer helper for driving [`HttpExchangeMachine`] with
/// [`blaze_machine::Script`], converting into [`HttpStepAns<'s>`] for any step lifetime `'s`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpScriptAns {
    Sent(RaceOutcome<Result<usize, TransportError>, ()>),
    RecvOrTimeout(RaceOutcome<Result<&'static [u8], TransportError>, ()>),
    Closed(Result<(), TransportError>),
}

impl HttpScriptAns {
    #[inline]
    pub const fn sent(bytes: usize) -> Self {
        Self::Sent(RaceOutcome::First(Ok(bytes)))
    }

    #[inline]
    pub const fn send_err(err: TransportError) -> Self {
        Self::Sent(RaceOutcome::First(Err(err)))
    }

    #[inline]
    pub const fn send_timeout() -> Self {
        Self::Sent(RaceOutcome::Second(()))
    }

    #[inline]
    pub const fn recv(bytes: &'static [u8]) -> Self {
        Self::RecvOrTimeout(RaceOutcome::First(Ok(bytes)))
    }

    #[inline]
    pub const fn recv_eof() -> Self {
        Self::RecvOrTimeout(RaceOutcome::First(Ok(b"")))
    }

    #[inline]
    pub const fn recv_err(err: TransportError) -> Self {
        Self::RecvOrTimeout(RaceOutcome::First(Err(err)))
    }

    #[inline]
    pub const fn recv_timeout() -> Self {
        Self::RecvOrTimeout(RaceOutcome::Second(()))
    }

    #[inline]
    pub const fn closed_ok() -> Self {
        Self::Closed(Ok(()))
    }

    #[inline]
    pub const fn closed_err(err: TransportError) -> Self {
        Self::Closed(Err(err))
    }
}

impl<'s> From<HttpScriptAns> for HttpStepAns<'s> {
    #[inline]
    fn from(ans: HttpScriptAns) -> Self {
        match ans {
            HttpScriptAns::Sent(r) => HttpStepAns::Sent(r),
            HttpScriptAns::RecvOrTimeout(r) => HttpStepAns::RecvOrTimeout(r),
            HttpScriptAns::Closed(r) => HttpStepAns::Closed(r),
        }
    }
}

/// Planted-bug selector for negative control tests (`HttpExchangePlant::None` in production).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum HttpExchangePlant {
    #[default]
    None,
    /// Planted bug: returns `Turn::Fail` / `CancelAction::Abandon` without emitting
    /// [`HttpStepReq::Close`].
    SkipCloseOnError,
    /// Planted bug: returns `Turn::Done` directly once the HTTP response finishes without emitting
    /// [`HttpStepReq::Close`].
    SkipCloseOnOk,
}

/// Named options for [`HttpExchangeMachine`] (`#[blaze::args]`, rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct HttpExchangeOpts<'a> {
    /// Open connection handle linearly owned by the machine until closed.
    pub conn: ConnHandle,
    /// HTTP method of the request (`Method::Head` suppresses response body framing).
    pub method: Method,
    /// Optional requested byte range (checked against `206` + `Content-Range` when head ends).
    pub range: Option<Range>,
    /// Maximum decoded body bytes accepted before failing with [`HttpExchangeError::TooBig`].
    pub body_most: usize,
    /// Maximum receive steps before failing with [`HttpExchangeError::Unending`].
    pub reads_most: usize,
    /// Per-step timeout for sends and receives.
    pub wait_most: Duration,
    /// Maximum bytes requested per [`HttpStepReq::RecvWithTimeout`] step.
    pub recv_chunk: usize,
    /// Explicit caller allocator for the response head, decoded body, and `Location` bytes.
    pub alloc: Alloc<'a>,
    /// Planted-bug control for negative tests (always `HttpExchangePlant::None` outside tests).
    pub plant: HttpExchangePlant = HttpExchangePlant::None,
}

/// Completed HTTP/1.1 response produced by [`HttpExchangeMachine`].
#[derive(Debug)]
pub struct HttpExchangeOut<'a> {
    pub status: u16,
    pub body: Vec<u8, Alloc<'a>>,
    pub range: Option<ContentRange>,
    pub location: Option<Vec<u8, Alloc<'a>>>,
}

/// Typed failure produced by [`HttpExchangeMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpExchangeError {
    /// HTTP/1.1 framing, status line, header, chunk, or range error from [`blaze_h1`].
    Protocol(blaze_h1::Error),
    /// Response body exceeded its configured limit (`most` bytes).
    TooBig { most: usize },
    /// `reads` receive steps completed without reaching the end of the response.
    Unending { reads: usize },
    /// A send or receive step timed out after `waited`.
    Silent { waited: Duration },
    /// Underlying transport error reported by the handler.
    Transport(TransportError),
    /// External cancellation or deadline expiry delivered via [`Machine::on_cancel`].
    Stopped(Stopped),
    /// Caller allocator refused growth for the request, response head, or body buffer.
    Alloc(AllocFailure),
}

impl fmt::Display for HttpExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpExchangeError::Protocol(err) => write!(f, "http exchange protocol error: {err}"),
            HttpExchangeError::TooBig { most } => {
                write!(f, "http exchange response body exceeded {most} bytes")
            }
            HttpExchangeError::Unending { reads } => {
                write!(f, "http exchange did not finish within {reads} receive steps")
            }
            HttpExchangeError::Silent { waited } => {
                write!(f, "http exchange peer silent for {waited:?}")
            }
            HttpExchangeError::Transport(err) => write!(f, "http exchange transport: {err}"),
            HttpExchangeError::Stopped(stopped) => {
                write!(f, "http exchange stopped ({:?})", stopped.reason)
            }
            HttpExchangeError::Alloc(err) => write!(f, "http exchange allocator: {err}"),
        }
    }
}

impl core::error::Error for HttpExchangeError {}

/// Encodes `head` and `body` into a wire HTTP/1.1 request buffer in `alloc` via
/// [`blaze_h1::write_request`].
pub fn encode_request_in<'a>(
    head: &blaze_h1::Request<'_>,
    body: &[u8],
    alloc: Alloc<'a>,
) -> Result<Vec<u8, Alloc<'a>>, HttpExchangeError> {
    let mut wire = Vec::new_in(alloc);
    blaze_h1::write_request(head, &mut wire).map_err(HttpExchangeError::Protocol)?;
    wire.try_extend_from_slice(body).map_err(HttpExchangeError::Alloc)?;
    Ok(wire)
}

/// Shared exchange fields carried while the connection handle is still open.
struct Active<'a> {
    conn: ConnHandle,
    req_len: usize,
    range: Option<Range>,
    most_body: u64,
    reads_most: usize,
    wait_most: Duration,
    recv_chunk: usize,
    alloc: Alloc<'a>,
    plant: HttpExchangePlant,
    parser: blaze_h1::Response<Alloc<'a>>,
    body: Vec<u8, Alloc<'a>>,
}

/// Pure sans-IO HTTP/1.1 exchange state machine implementing [`blaze_machine::Machine`].
///
/// Drives one HTTP/1.1 request/response exchange over [`blaze_h1::Response`], emitting
/// [`HttpStepReq::Send`], [`HttpStepReq::RecvWithTimeout`], and [`HttpStepReq::Close`].
pub enum HttpExchangeMachine<'a> {
    Init(Active<'a>),
    Sending {
        active: Active<'a>,
        sent: usize,
    },
    Receiving {
        active: Active<'a>,
        reads: usize,
        checked_range: bool,
        range_out: Option<ContentRange>,
    },
    Closing {
        outcome: Result<HttpExchangeOut<'a>, HttpExchangeError>,
    },
}

impl<'a> HttpExchangeMachine<'a> {
    /// Creates a new exchange machine for an already-encoded request of `req_len` wire bytes.
    pub fn new(req_len: usize, opts: HttpExchangeOpts<'a>) -> Self {
        let most_body = match opts.range {
            Some(Range { first, last: Some(last) }) if last >= first => {
                (last - first + 1).min(opts.body_most as u64)
            }
            Some(_) | None => opts.body_most as u64,
        };
        let limits = blaze_h1::Limits {
            most_head: 64 << 10,
            most_headers: 100,
            most_body,
            ..blaze_h1::Limits::SHIPPED
        };
        let h1_method = match opts.method {
            Method::Head => blaze_h1::Method::Head,
            _ => blaze_h1::Method::Other,
        };
        let parser = blaze_h1::Response::new(blaze_h1::ResponseOpts {
            limits,
            method: h1_method,
            alloc: opts.alloc,
        });
        let body = Vec::new_in(opts.alloc);
        Self::Init(Active {
            conn: opts.conn,
            req_len,
            range: opts.range,
            most_body,
            reads_most: opts.reads_most,
            wait_most: opts.wait_most,
            recv_chunk: opts.recv_chunk,
            alloc: opts.alloc,
            plant: opts.plant,
            parser,
            body,
        })
    }

    /// Encodes `head` + `body` with [`encode_request_in`] and builds the matching
    /// [`HttpExchangeMachine`], returning `(encoded_wire_bytes, machine)`.
    pub fn for_request(
        head: &blaze_h1::Request<'_>,
        body: &[u8],
        opts: HttpExchangeOpts<'a>,
    ) -> Result<(Vec<u8, Alloc<'a>>, Self), HttpExchangeError> {
        let wire = encode_request_in(head, body, opts.alloc)?;
        let machine = Self::new(wire.len(), opts);
        Ok((wire, machine))
    }

    fn close_with_err(
        conn: ConnHandle,
        err: HttpExchangeError,
        plant: HttpExchangePlant,
    ) -> Turn<Self, HttpStepReq, HttpExchangeOut<'a>, HttpExchangeError> {
        if plant == HttpExchangePlant::SkipCloseOnError {
            Turn::Fail(err)
        } else {
            Turn::Ask {
                request: HttpStepReq::Close { conn },
                next: HttpExchangeMachine::Closing { outcome: Err(err) },
            }
        }
    }

    fn close_with_ok(
        active: Active<'a>,
        range: Option<ContentRange>,
    ) -> Turn<Self, HttpStepReq, HttpExchangeOut<'a>, HttpExchangeError> {
        let conn = active.conn;
        let plant = active.plant;
        let status = active.parser.status();
        let location = match active.parser.header("location") {
            Some(raw) => {
                let mut loc = Vec::new_in(active.alloc);
                if let Err(err) = loc.try_extend_from_slice(raw) {
                    return Self::close_with_err(conn, HttpExchangeError::Alloc(err), plant);
                }
                Some(loc)
            }
            None => None,
        };
        let out = HttpExchangeOut { status, body: active.body, range, location };
        if plant == HttpExchangePlant::SkipCloseOnOk {
            Turn::Done(out)
        } else {
            Turn::Ask {
                request: HttpStepReq::Close { conn },
                next: HttpExchangeMachine::Closing { outcome: Ok(out) },
            }
        }
    }

    fn handle_recv(
        mut active: Active<'a>,
        reads: usize,
        mut checked_range: bool,
        mut range_out: Option<ContentRange>,
        outcome: RaceOutcome<Result<&[u8], TransportError>, ()>,
    ) -> Turn<Self, HttpStepReq, HttpExchangeOut<'a>, HttpExchangeError> {
        match outcome {
            RaceOutcome::Second(()) => Self::close_with_err(
                active.conn,
                HttpExchangeError::Silent { waited: active.wait_most },
                active.plant,
            ),
            RaceOutcome::First(Err(err)) => Self::close_with_err(
                active.conn,
                HttpExchangeError::Transport(err),
                active.plant,
            ),
            RaceOutcome::First(Ok(chunk)) => {
                let next_reads = reads + 1;
                if chunk.is_empty() {
                    return match active.parser.closed() {
                        Ok(()) => Self::close_with_ok(active, range_out),
                        Err(err) => Self::close_with_err(
                            active.conn,
                            HttpExchangeError::Protocol(err),
                            active.plant,
                        ),
                    };
                }

                let fed = active.parser.feed(chunk, &mut active.body);
                let head_over = matches!(
                    &fed,
                    Err(blaze_h1::Error { why: blaze_h1::Why::BodyOver { .. }, .. })
                );
                if !checked_range && (active.parser.has_head() || head_over) {
                    checked_range = true;
                    if let Some(asked) = active.range
                        && !blaze_h1::is_redirect(active.parser.status())
                    {
                        match active.parser.ranged(asked) {
                            Ok(cr) => range_out = Some(cr),
                            Err(err) => {
                                return Self::close_with_err(
                                    active.conn,
                                    HttpExchangeError::Protocol(err),
                                    active.plant,
                                );
                            }
                        }
                    }
                }

                match fed {
                    Err(err) => {
                        let mapped = match err.why {
                            blaze_h1::Why::BodyOver { .. } => {
                                HttpExchangeError::TooBig { most: active.most_body as usize }
                            }
                            _ => HttpExchangeError::Protocol(err),
                        };
                        Self::close_with_err(active.conn, mapped, active.plant)
                    }
                    Ok(fed) => {
                        if fed.consumed < chunk.len() {
                            let err = blaze_h1::Error {
                                at: active.parser.body_len(),
                                why: blaze_h1::Why::AfterEnd,
                            };
                            return Self::close_with_err(
                                active.conn,
                                HttpExchangeError::Protocol(err),
                                active.plant,
                            );
                        }
                        if fed.done {
                            Self::close_with_ok(active, range_out)
                        } else if next_reads >= active.reads_most {
                            Self::close_with_err(
                                active.conn,
                                HttpExchangeError::Unending { reads: active.reads_most },
                                active.plant,
                            )
                        } else {
                            let req = HttpStepReq::RecvWithTimeout {
                                conn: active.conn,
                                max_bytes: active.recv_chunk,
                                timeout: active.wait_most,
                            };
                            Turn::Ask {
                                request: req,
                                next: HttpExchangeMachine::Receiving {
                                    active,
                                    reads: next_reads,
                                    checked_range,
                                    range_out,
                                },
                            }
                        }
                    }
                }
            }
        }
    }
}

impl<'a> Machine for HttpExchangeMachine<'a> {
    type Req = HttpStepReq;
    type Out = HttpExchangeOut<'a>;
    type Err = HttpExchangeError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            HttpExchangeMachine::Init(active) => {
                if active.req_len == 0 {
                    let req = HttpStepReq::RecvWithTimeout {
                        conn: active.conn,
                        max_bytes: active.recv_chunk,
                        timeout: active.wait_most,
                    };
                    Turn::Ask {
                        request: req,
                        next: HttpExchangeMachine::Receiving {
                            active,
                            reads: 0,
                            checked_range: false,
                            range_out: None,
                        },
                    }
                } else {
                    let req = HttpStepReq::Send {
                        conn: active.conn,
                        offset: 0,
                        len: active.req_len,
                        timeout: active.wait_most,
                    };
                    Turn::Ask {
                        request: req,
                        next: HttpExchangeMachine::Sending { active, sent: 0 },
                    }
                }
            }
            _ => unreachable!("HttpExchangeMachine::start called on non-Init state"),
        }
    }

    fn resume(
        self,
        answer: HttpStepAns<'_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match (self, answer) {
            (HttpExchangeMachine::Sending { active, sent }, HttpStepAns::Sent(outcome)) => {
                match outcome {
                    RaceOutcome::First(Ok(0)) => Self::close_with_err(
                        active.conn,
                        HttpExchangeError::Transport(TransportError::WriteFailed),
                        active.plant,
                    ),
                    RaceOutcome::First(Ok(n)) => {
                        let next_sent = sent.saturating_add(n);
                        if next_sent < active.req_len {
                            let req = HttpStepReq::Send {
                                conn: active.conn,
                                offset: next_sent,
                                len: active.req_len - next_sent,
                                timeout: active.wait_most,
                            };
                            Turn::Ask {
                                request: req,
                                next: HttpExchangeMachine::Sending { active, sent: next_sent },
                            }
                        } else {
                            let req = HttpStepReq::RecvWithTimeout {
                                conn: active.conn,
                                max_bytes: active.recv_chunk,
                                timeout: active.wait_most,
                            };
                            Turn::Ask {
                                request: req,
                                next: HttpExchangeMachine::Receiving {
                                    active,
                                    reads: 0,
                                    checked_range: false,
                                    range_out: None,
                                },
                            }
                        }
                    }
                    RaceOutcome::First(Err(err)) => Self::close_with_err(
                        active.conn,
                        HttpExchangeError::Transport(err),
                        active.plant,
                    ),
                    RaceOutcome::Second(()) => Self::close_with_err(
                        active.conn,
                        HttpExchangeError::Silent { waited: active.wait_most },
                        active.plant,
                    ),
                }
            }

            (
                HttpExchangeMachine::Receiving { active, reads, checked_range, range_out },
                HttpStepAns::RecvOrTimeout(outcome),
            ) => Self::handle_recv(active, reads, checked_range, range_out, outcome),

            (
                HttpExchangeMachine::Receiving { active, reads, checked_range, range_out },
                HttpStepAns::RecvOwned(outcome),
            ) => {
                let borrowed = match &outcome {
                    RaceOutcome::First(Ok(bytes)) => RaceOutcome::First(Ok(bytes.as_ref())),
                    RaceOutcome::First(Err(err)) => RaceOutcome::First(Err(*err)),
                    RaceOutcome::Second(()) => RaceOutcome::Second(()),
                };
                Self::handle_recv(active, reads, checked_range, range_out, borrowed)
            }

            (HttpExchangeMachine::Closing { outcome }, HttpStepAns::Closed(close_res)) => {
                match (outcome, close_res) {
                    (Ok(out), Ok(())) => Turn::Done(out),
                    (Ok(_), Err(close_err)) => Turn::Fail(HttpExchangeError::Transport(close_err)),
                    (Err(primary), _) => Turn::Fail(primary),
                }
            }

            _ => unreachable!("HttpExchangeMachine::resume answer matched active state"),
        }
    }

    fn on_cancel(
        self,
        stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        match self {
            HttpExchangeMachine::Init(active)
            | HttpExchangeMachine::Sending { active, .. }
            | HttpExchangeMachine::Receiving { active, .. } => {
                if active.plant == HttpExchangePlant::SkipCloseOnError {
                    CancelAction::Abandon
                } else {
                    CancelAction::Cleanup(Turn::Ask {
                        request: HttpStepReq::Close { conn: active.conn },
                        next: HttpExchangeMachine::Closing {
                            outcome: Err(HttpExchangeError::Stopped(stopped)),
                        },
                    })
                }
            }
            HttpExchangeMachine::Closing { .. } => CancelAction::Abandon,
        }
    }
}

/// Handler-level error returned by [`NetHttpHandler`] on invalid handles, out-of-bounds sends,
/// or requests after close.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetHttpHandlerError {
    /// Request named a [`ConnHandle`] other than the one bound to this handler.
    UnknownHandle { expected: ConnHandle, got: ConnHandle },
    /// Request attempted I/O or a second close after the underlying connection was already closed.
    AlreadyClosed { conn: ConnHandle },
    /// [`HttpStepReq::Send`] requested a byte range outside the encoded request wire buffer.
    SendOutOfBounds { offset: usize, len: usize, wire_len: usize },
}

impl fmt::Display for NetHttpHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            NetHttpHandlerError::UnknownHandle { expected, got } => write!(
                f,
                "net http handler handle mismatch: expected {:?}, got {:?}",
                expected, got
            ),
            NetHttpHandlerError::AlreadyClosed { conn } => {
                write!(f, "net http handler connection {:?} is already closed", conn)
            }
            NetHttpHandlerError::SendOutOfBounds { offset, len, wire_len } => write!(
                f,
                "net http handler send range {offset}..+{len} out of bounds for wire len {wire_len}"
            ),
        }
    }
}

impl core::error::Error for NetHttpHandlerError {}

/// Failure returned by [`drive_exchange_over_net`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NetExchangeError {
    /// Establishing the underlying [`crate::net::Conn`] failed.
    Connect(crate::net::NetError),
    /// Driving [`HttpExchangeMachine`] failed (protocol, timeout, transport, or handler error).
    Drive(blaze_machine::DriveError<HttpExchangeError, NetHttpHandlerError>),
    /// Closing any remaining linear connection after driving failed.
    Finish(crate::net::NetError),
}

impl fmt::Display for NetExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetExchangeError::Connect(err) => write!(f, "net http connect failed: {err}"),
            NetExchangeError::Drive(err) => write!(f, "net http exchange failed: {err}"),
            NetExchangeError::Finish(err) => write!(f, "net http finish failed: {err}"),
        }
    }
}

impl core::error::Error for NetExchangeError {}

/// Real [`blaze_machine::Handler`]`<`[`HttpStepReq`]`>` adapter driving [`HttpExchangeMachine`]
/// directly over a [`crate::net::Conn`] and [`crate::cx::Cx`] with zero duplicate HTTP framing.
///
/// Linear (`#[blaze::linear]`): owns the open [`crate::net::Conn`] until [`HttpStepReq::Close`]
/// consumes it via [`crate::net::Conn::finish`], and must be finalized with
/// [`NetHttpHandler::finish`] so an abandoned machine can never leak its connection.
#[blaze::linear]
#[must_use = "a NetHttpHandler owns a linear Conn and must be finalized with finish()"]
pub struct NetHttpHandler<'cx, 'w> {
    cx: &'cx crate::cx::Cx,
    expected: ConnHandle,
    conn: Option<crate::net::Conn>,
    wire: &'w [u8],
    bytes_sent: usize,
    closed: bool,
}

impl<'cx, 'w> NetHttpHandler<'cx, 'w> {
    /// Wraps an already-connected [`crate::net::Conn`] and encoded HTTP/1.1 request wire slice.
    #[inline]
    pub const fn new(
        cx: &'cx crate::cx::Cx,
        expected: ConnHandle,
        conn: crate::net::Conn,
        wire: &'w [u8],
    ) -> Self {
        Self { cx, expected, conn: Some(conn), wire, bytes_sent: 0, closed: false }
    }

    /// Connects to `at` through `net` under `cx` and wraps the resulting [`crate::net::Conn`].
    pub fn connect(
        cx: &'cx crate::cx::Cx,
        net: &crate::net::Net,
        at: &crate::net::Addr,
        expected: ConnHandle,
        wire: &'w [u8],
    ) -> Result<Self, crate::net::NetError> {
        let conn = net.connect(cx, at)?;
        Ok(Self::new(cx, expected, conn, wire))
    }

    /// Returns `true` once [`HttpStepReq::Close`] has consumed and closed the underlying
    /// [`crate::net::Conn`].
    #[inline]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Total request wire bytes written through [`HttpStepReq::Send`].
    #[inline]
    pub const fn bytes_sent(&self) -> usize {
        self.bytes_sent
    }

    /// Consumes this linear handler, closing the underlying [`crate::net::Conn`] if the machine
    /// terminated without emitting [`HttpStepReq::Close`].
    #[blaze::consumes]
    pub fn finish(self) -> Result<(), crate::net::NetError> {
        let Self { cx, conn, .. } = self;
        match conn {
            Some(conn) => conn.finish(cx),
            None => Ok(()),
        }
    }

    fn step_cx(&self, timeout: Duration) -> crate::cx::Cx {
        let deadline = blaze_core::wait::Deadline::after(self.cx.io().now(), timeout);
        self.cx.until(deadline)
    }
}

impl blaze_machine::Handler<HttpStepReq> for NetHttpHandler<'_, '_> {
    type Error = NetHttpHandlerError;

    fn perform<Out>(
        &mut self,
        request: HttpStepReq,
        deliver: impl FnOnce(blaze_machine::Perform<HttpStepAns<'_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        use blaze_machine::{GraceBudget, Perform};

        match request {
            HttpStepReq::Send { conn, offset, len, timeout } => {
                if conn != self.expected {
                    return Err(NetHttpHandlerError::UnknownHandle {
                        expected: self.expected,
                        got: conn,
                    });
                }
                let Some(end) = offset.checked_add(len) else {
                    return Err(NetHttpHandlerError::SendOutOfBounds {
                        offset,
                        len,
                        wire_len: self.wire.len(),
                    });
                };
                let Some(slice) = self.wire.get(offset..end) else {
                    return Err(NetHttpHandlerError::SendOutOfBounds {
                        offset,
                        len,
                        wire_len: self.wire.len(),
                    });
                };
                let wait = self.step_cx(timeout);
                let own_timeout = wait.deadline() != self.cx.deadline();
                let Some(net_conn) = self.conn.as_ref() else {
                    return Err(NetHttpHandlerError::AlreadyClosed { conn });
                };
                match net_conn.send_all(&wait, slice) {
                    Ok(()) => {
                        self.bytes_sent = self.bytes_sent.saturating_add(slice.len());
                        Ok(deliver(Perform::Answered(HttpStepAns::Sent(
                            RaceOutcome::First(Ok(slice.len())),
                        ))))
                    }
                    Err(err) => match err.kind {
                        crate::net::NetKind::Stopped(crate::Stopped::Deadline { .. })
                            if own_timeout =>
                        {
                            Ok(deliver(Perform::Answered(HttpStepAns::Sent(
                                RaceOutcome::Second(()),
                            ))))
                        }
                        crate::net::NetKind::Stopped(crate::Stopped::Cancelled { .. }) => {
                            Ok(deliver(Perform::Stopped(Stopped::cancelled(
                                GraceBudget::DEFAULT,
                            ))))
                        }
                        crate::net::NetKind::Stopped(_) => Ok(deliver(Perform::Stopped(
                            Stopped::deadline(GraceBudget::DEFAULT),
                        ))),
                        _ => Ok(deliver(Perform::Answered(HttpStepAns::Sent(
                            RaceOutcome::First(Err(TransportError::WriteFailed)),
                        )))),
                    },
                }
            }
            HttpStepReq::RecvWithTimeout { conn, max_bytes, timeout } => {
                if conn != self.expected {
                    return Err(NetHttpHandlerError::UnknownHandle {
                        expected: self.expected,
                        got: conn,
                    });
                }
                let wait = self.step_cx(timeout);
                let own_timeout = wait.deadline() != self.cx.deadline();
                let Some(net_conn) = self.conn.as_ref() else {
                    return Err(NetHttpHandlerError::AlreadyClosed { conn });
                };
                let mut received = self.cx.bytes();
                match net_conn.recv(&wait, max_bytes, &mut received) {
                    Ok(_) => Ok(deliver(Perform::Answered(HttpStepAns::RecvOrTimeout(
                        RaceOutcome::First(Ok(received.as_slice())),
                    )))),
                    Err(err) => match err.kind {
                        crate::net::NetKind::Stopped(crate::Stopped::Deadline { .. })
                            if own_timeout =>
                        {
                            Ok(deliver(Perform::Answered(HttpStepAns::RecvOrTimeout(
                                RaceOutcome::Second(()),
                            ))))
                        }
                        crate::net::NetKind::Stopped(crate::Stopped::Cancelled { .. }) => {
                            Ok(deliver(Perform::Stopped(Stopped::cancelled(
                                GraceBudget::DEFAULT,
                            ))))
                        }
                        crate::net::NetKind::Stopped(_) => Ok(deliver(Perform::Stopped(
                            Stopped::deadline(GraceBudget::DEFAULT),
                        ))),
                        _ => Ok(deliver(Perform::Answered(HttpStepAns::RecvOrTimeout(
                            RaceOutcome::First(Err(TransportError::ReadFailed)),
                        )))),
                    },
                }
            }
            HttpStepReq::Close { conn } => {
                if conn != self.expected {
                    return Err(NetHttpHandlerError::UnknownHandle {
                        expected: self.expected,
                        got: conn,
                    });
                }
                let Some(net_conn) = self.conn.take() else {
                    return Err(NetHttpHandlerError::AlreadyClosed { conn });
                };
                self.closed = true;
                let res = match net_conn.finish(self.cx) {
                    Ok(()) => Ok(()),
                    Err(_) => Err(TransportError::CloseFailed),
                };
                Ok(deliver(Perform::Answered(HttpStepAns::Closed(res))))
            }
        }
    }
}

/// Connects to `at` over `net` and drives `machine` to completion through [`crate::cx::Cx::drive_machine`]
/// using [`NetHttpHandler`], guaranteeing linear teardown of the underlying [`crate::net::Conn`].
pub fn drive_exchange_over_net<'a>(
    cx: &crate::cx::Cx,
    net: &crate::net::Net,
    at: &crate::net::Addr,
    wire: &[u8],
    machine: HttpExchangeMachine<'a>,
    handle: ConnHandle,
    opts: blaze_machine::DriveOpts,
) -> Result<HttpExchangeOut<'a>, NetExchangeError> {
    let mut handler =
        NetHttpHandler::connect(cx, net, at, handle, wire).map_err(NetExchangeError::Connect)?;
    let driven = cx.drive_machine(
        "bstd.requests.net_exchange",
        &mut handler,
        &mut (),
        machine,
        opts,
    );
    let finish_res = handler.finish();
    match driven {
        Ok(out) => match finish_res {
            Ok(()) => Ok(out),
            Err(err) => Err(NetExchangeError::Finish(err)),
        },
        Err(err) => Err(NetExchangeError::Drive(err)),
    }
}

#[cfg(test)]
mod tests {
    use core::{num::NonZeroU32, time::Duration};

    use blaze_machine::{
        CancelReason, DriveError, DriveOpts, GraceBudget, Script, ScriptError, Stopped, drive,
    };

    use super::*;
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    const TIMEOUT: Duration = Duration::from_millis(500);
    const CHUNK: usize = 4096;

    fn test_conn() -> ConnHandle {
        ConnHandle::new(7, NonZeroU32::new(1).expect("non-zero generation"))
    }

    fn sample_head<'a>() -> blaze_h1::Request<'a> {
        blaze_h1::Request {
            method: "GET",
            target: "/pkg/record",
            host: "127.0.0.1:8080",
            range: None,
            headers: &[("user-agent", "bstd-machine-test")],
            body_len: None,
            close: true,
        }
    }

    /// Chunked + split HTTP/1.1 headers and body across multiple receives, partial request sends,
    /// driven through `Cx::drive_machine`, ending with mandatory linear `Close`.
    #[test]
    fn http_exchange_machine_handles_split_chunked_response_and_closes() {
        let alloc = blaze_own::testing::heap().alloc();
        let conn = test_conn();
        let head = sample_head();

        let (wire, machine) = HttpExchangeMachine::for_request(
            &head,
            b"",
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 64 << 10,
                reads_most: 16,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        )
        .expect("encode request");
        assert!(wire.as_slice().starts_with(b"GET /pkg/record HTTP/1.1\r\n"));

        let total = wire.len();
        let first_part = 12usize.min(total);
        let rest_part = total - first_part;

        let mut script = Script::new_in(alloc);
        script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: total, timeout: TIMEOUT },
            HttpScriptAns::sent(first_part),
        );
        if rest_part > 0 {
            script.expect_answer(
                HttpStepReq::Send { conn, offset: first_part, len: rest_part, timeout: TIMEOUT },
                HttpScriptAns::sent(rest_part),
            );
        }
        // Split status line + headers across two receives, then split chunked body across two more.
        script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"HTTP/1.1 200 OK\r\nTransfer-En"),
        );
        script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"coding: chunked\r\nX-Origin: test\r\n\r\n5\r\nhel"),
        );
        script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"lo\r\n7\r\n wo"),
        );
        script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"rld!\r\n0\r\n\r\n"),
        );
        script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let out = cx
            .drive_machine(
                "bstd.requests.http_exchange",
                &mut script,
                &mut (),
                machine,
                DriveOpts { max_steps: 16 },
            )
            .expect("chunked exchange succeeds");

        assert_eq!(out.status, 200);
        assert_eq!(out.body.as_slice(), b"hello world!");
        assert_eq!(out.range, None);
        assert_eq!(script.finish(), Ok(()));
    }

    /// Closes its handle linearly on malformed HTTP response, receive timeout, and external stop.
    #[test]
    fn http_exchange_machine_closes_handle_on_malformed_response_timeout_and_stop() {
        let alloc = blaze_own::testing::heap().alloc();
        let conn = test_conn();

        // 1. Malformed status line -> Close -> Fail(Protocol(StatusLine)).
        let mut malformed_script = Script::new_in(alloc);
        malformed_script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 16, timeout: TIMEOUT },
            HttpScriptAns::sent(16),
        );
        malformed_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"GARBAGE_NOT_HTTP\r\n\r\n"),
        );
        malformed_script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let machine = HttpExchangeMachine::new(
            16,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        );
        let res = drive(&mut malformed_script, &mut (), machine, DriveOpts { max_steps: 16 });
        assert!(
            matches!(
                res,
                Err(DriveError::Machine(HttpExchangeError::Protocol(blaze_h1::Error {
                    why: blaze_h1::Why::StatusLine,
                    ..
                })))
            ),
            "expected StatusLine protocol error, got {res:?}"
        );
        assert_eq!(malformed_script.finish(), Ok(()));

        // 2. Receive timeout -> Close -> Fail(Silent).
        let mut timeout_script = Script::new_in(alloc);
        timeout_script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 16, timeout: TIMEOUT },
            HttpScriptAns::sent(16),
        );
        timeout_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv_timeout(),
        );
        timeout_script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let machine = HttpExchangeMachine::new(
            16,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        );
        let res = drive(&mut timeout_script, &mut (), machine, DriveOpts { max_steps: 16 });
        assert!(
            matches!(res, Err(DriveError::Machine(HttpExchangeError::Silent { waited })) if waited == TIMEOUT),
            "expected Silent timeout error, got {res:?}"
        );
        assert_eq!(timeout_script.finish(), Ok(()));

        // 3. External cancellation (`Stopped`) -> on_cancel Cleanup -> Close -> Fail(Stopped).
        let mut cancel_script = Script::new_in(alloc);
        cancel_script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 16, timeout: TIMEOUT },
            HttpScriptAns::sent(16),
        );
        cancel_script.expect_stop(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            Stopped::cancelled(GraceBudget::DEFAULT),
        );
        cancel_script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let machine = HttpExchangeMachine::new(
            16,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        );
        let res = drive(&mut cancel_script, &mut (), machine, DriveOpts { max_steps: 16 });
        assert!(
            matches!(
                res,
                Err(DriveError::Machine(HttpExchangeError::Stopped(Stopped {
                    reason: CancelReason::Cancelled,
                    ..
                })))
            ),
            "expected Stopped(Cancelled), got {res:?}"
        );
        assert_eq!(cancel_script.finish(), Ok(()));
    }

    /// Planted-bug controls: skipping `Close` on malformed response, timeout, `Stopped`, or `Ok`
    /// is caught immediately by `Script::finish()` as an unconsumed `Close` step.
    #[test]
    fn http_exchange_machine_planted_bug_controls_catch_skipped_close() {
        let alloc = blaze_own::testing::heap().alloc();
        let conn = test_conn();

        // Plant 1: skip Close on malformed response.
        let mut s1 = Script::new_in(alloc);
        s1.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 8, timeout: TIMEOUT },
            HttpScriptAns::sent(8),
        );
        s1.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"BAD\r\n\r\n"),
        );
        s1.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let m1 = HttpExchangeMachine::new(
            8,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::SkipCloseOnError,
            },
        );
        let res1 = drive(&mut s1, &mut (), m1, DriveOpts { max_steps: 16 });
        assert!(matches!(res1, Err(DriveError::Machine(HttpExchangeError::Protocol(_)))));
        assert_eq!(s1.finish(), Err(ScriptError::UnconsumedSteps { step: 2, remaining: 1 }));

        // Plant 2: skip Close on receive timeout.
        let mut s2 = Script::new_in(alloc);
        s2.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 8, timeout: TIMEOUT },
            HttpScriptAns::sent(8),
        );
        s2.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv_timeout(),
        );
        s2.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let m2 = HttpExchangeMachine::new(
            8,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::SkipCloseOnError,
            },
        );
        let res2 = drive(&mut s2, &mut (), m2, DriveOpts { max_steps: 16 });
        assert!(matches!(res2, Err(DriveError::Machine(HttpExchangeError::Silent { .. }))));
        assert_eq!(s2.finish(), Err(ScriptError::UnconsumedSteps { step: 2, remaining: 1 }));

        // Plant 3: skip Close on Ok response.
        let mut s3 = Script::new_in(alloc);
        s3.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 8, timeout: TIMEOUT },
            HttpScriptAns::sent(8),
        );
        s3.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"),
        );
        s3.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let m3 = HttpExchangeMachine::new(
            8,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::SkipCloseOnOk,
            },
        );
        let res3 = drive(&mut s3, &mut (), m3, DriveOpts { max_steps: 16 });
        assert!(res3.is_ok());
        assert_eq!(s3.finish(), Err(ScriptError::UnconsumedSteps { step: 2, remaining: 1 }));
    }

    /// Verifies close-delimited responses, `body_most` enforcement, and `206 Content-Range`.
    #[test]
    fn http_exchange_machine_supports_close_delimited_and_ranged_and_body_limits() {
        let alloc = blaze_own::testing::heap().alloc();
        let conn = test_conn();

        // 1. Close-delimited response (completes on EOF, then closes).
        let mut close_script = Script::new_in(alloc);
        close_script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 8, timeout: TIMEOUT },
            HttpScriptAns::sent(8),
        );
        close_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"HTTP/1.0 200 OK\r\nConnection: close\r\n\r\nab"),
        );
        close_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"cd"),
        );
        close_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv_eof(),
        );
        close_script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let m = HttpExchangeMachine::new(
            8,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 64,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        );
        let out = drive(&mut close_script, &mut (), m, DriveOpts { max_steps: 16 })
            .expect("close-delimited succeeds");
        assert_eq!(out.status, 200);
        assert_eq!(out.body.as_slice(), b"abcd");
        assert_eq!(close_script.finish(), Ok(()));

        // 2. Ranged 206 response checks Content-Range and closes.
        let asked = Range { first: 10, last: Some(13) };
        let mut range_script = Script::new_in(alloc);
        range_script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 8, timeout: TIMEOUT },
            HttpScriptAns::sent(8),
        );
        range_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 10-13/100\r\nContent-Length: 4\r\n\r\nWXYZ",
            ),
        );
        range_script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let m = HttpExchangeMachine::new(
            8,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: Some(asked),
                body_most: 64,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        );
        let out = drive(&mut range_script, &mut (), m, DriveOpts { max_steps: 16 })
            .expect("ranged 206 succeeds");
        assert_eq!(out.status, 206);
        assert_eq!(out.body.as_slice(), b"WXYZ");
        assert_eq!(
            out.range,
            Some(ContentRange { first: 10, last: 13, complete: Some(100) })
        );
        assert_eq!(range_script.finish(), Ok(()));

        // 3. Body exceeding `body_most` returns TooBig and still closes linearly.
        let mut big_script = Script::new_in(alloc);
        big_script.expect_answer(
            HttpStepReq::Send { conn, offset: 0, len: 8, timeout: TIMEOUT },
            HttpScriptAns::sent(8),
        );
        big_script.expect_answer(
            HttpStepReq::RecvWithTimeout { conn, max_bytes: CHUNK, timeout: TIMEOUT },
            HttpScriptAns::recv(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"),
        );
        big_script.expect_answer(HttpStepReq::Close { conn }, HttpScriptAns::closed_ok());

        let m = HttpExchangeMachine::new(
            8,
            HttpExchangeOpts {
                conn,
                method: Method::Get,
                range: None,
                body_most: 10,
                reads_most: 8,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        );
        let res = drive(&mut big_script, &mut (), m, DriveOpts { max_steps: 16 });
        assert!(
            matches!(res, Err(DriveError::Machine(HttpExchangeError::TooBig { most: 10 }))),
            "expected TooBig(10), got {res:?}"
        );
        assert_eq!(big_script.finish(), Ok(()));
    }

    /// Records a split chunked HTTP/1.1 exchange through `RecordHandler` (over a handler reusing a
    /// non-`'static` scratch buffer), replays it deterministically via `RecordHandler::into_script`,
    /// and verifies `emit_script_source`.
    #[test]
    fn http_exchange_machine_records_and_replays_via_record_handler() {
        use blaze_machine::{Handler, Perform, RecordHandler, TranscriptEntry};
        use blaze_own::FixedString;

        struct ChunkedScratchHandler {
            chunks: [&'static [u8]; 4],
            next_chunk: usize,
            scratch: [u8; 128],
        }

        impl Handler<HttpStepReq> for ChunkedScratchHandler {
            type Error = ();

            fn perform<Out>(
                &mut self,
                request: HttpStepReq,
                deliver: impl FnOnce(Perform<HttpStepAns<'_>>) -> Out,
            ) -> Result<Out, Self::Error> {
                match request {
                    HttpStepReq::Send { len, .. } => {
                        let written = len.min(14);
                        Ok(deliver(Perform::Answered(HttpStepAns::Sent(RaceOutcome::First(Ok(
                            written,
                        ))))))
                    }
                    HttpStepReq::RecvWithTimeout { .. } => {
                        let chunk = if self.next_chunk < self.chunks.len() {
                            let c = self.chunks[self.next_chunk];
                            self.next_chunk += 1;
                            c
                        } else {
                            b""
                        };
                        let n = chunk.len();
                        self.scratch[..n].copy_from_slice(chunk);
                        Ok(deliver(Perform::Answered(HttpStepAns::RecvOrTimeout(
                            RaceOutcome::First(Ok(&self.scratch[..n])),
                        ))))
                    }
                    HttpStepReq::Close { .. } => {
                        Ok(deliver(Perform::Answered(HttpStepAns::Closed(Ok(())))))
                    }
                }
            }
        }

        let alloc = blaze_own::testing::heap().alloc();
        let conn = test_conn();
        let head = sample_head();

        let make_machine = || {
            let (_wire, machine) = HttpExchangeMachine::for_request(
                &head,
                b"",
                HttpExchangeOpts {
                    conn,
                    method: Method::Get,
                    range: None,
                    body_most: 64 << 10,
                    reads_most: 16,
                    wait_most: TIMEOUT,
                    recv_chunk: CHUNK,
                    alloc,
                    plant: HttpExchangePlant::None,
                },
            )
            .expect("encode request");
            machine
        };

        let mut live = ChunkedScratchHandler {
            chunks: [
                b"HTTP/1.1 200 OK\r\nTransfer-En",
                b"coding: chunked\r\nX-Recorded: yes\r\n\r\n6\r\nrec",
                b"ord\r\n8\r\n-replay!\r\n",
                b"0\r\n\r\n",
            ],
            next_chunk: 0,
            scratch: [0u8; 128],
        };

        let io = Io::simulated(SimWorld::new(29, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");

        let mut recorder = RecordHandler::new_in(&mut live, alloc);
        let live_out = cx
            .drive_machine(
                "bstd.requests.record",
                &mut recorder,
                &mut (),
                make_machine(),
                DriveOpts { max_steps: 24 },
            )
            .expect("recorded chunked exchange succeeds");

        assert_eq!(live_out.status, 200);
        assert_eq!(live_out.body.as_slice(), b"record-replay!");
        assert_eq!(live_out.range, None);
        assert!(recorder.len() >= 6, "expected multi-step recorded transcript, got {}", recorder.len());
        assert!(matches!(
            recorder.entries().first(),
            Some(TranscriptEntry::Answered {
                req: HttpStepReq::Send { offset: 0, .. },
                ans: HttpRecordedAns::Sent(RaceOutcome::First(Ok(_))),
            })
        ));
        assert!(matches!(
            recorder.entries().last(),
            Some(TranscriptEntry::Answered {
                req: HttpStepReq::Close { conn: c },
                ans: HttpRecordedAns::Closed(Ok(())),
            }) if *c == conn
        ));

        // Replay deterministically via zero-IO `Script` built from the recorded transcript.
        let mut replay = recorder.into_script(alloc);
        let replay_out = cx
            .drive_machine(
                "bstd.requests.replay",
                &mut replay,
                &mut (),
                make_machine(),
                DriveOpts { max_steps: 24 },
            )
            .expect("replayed chunked exchange succeeds");

        assert_eq!(replay_out.status, live_out.status);
        assert_eq!(replay_out.body.as_slice(), live_out.body.as_slice());
        assert_eq!(replay_out.range, live_out.range);
        assert_eq!(replay.finish(), Ok(()));

        // Emit valid Rust `Script` source reproducing the transcript.
        let mut src = FixedString::<4096>::new();
        assert_eq!(recorder.emit_script_source(&mut src), Ok(()));
        let text = src.text().expect("recorded script source fits in 4096 bytes");
        assert!(text.contains("let mut script = Script::new_in(alloc);"), "missing header:\n{text}");
        assert!(text.contains("script.expect_answer("), "missing expect_answer:\n{text}");
        assert!(text.contains("Send {"), "missing Send request:\n{text}");
        assert!(text.contains("RecvWithTimeout {"), "missing RecvWithTimeout request:\n{text}");
        assert!(text.contains("Close {"), "missing Close request:\n{text}");
        assert!(text.contains("RecvOrTimeout("), "missing RecvOrTimeout answer:\n{text}");
    }

    /// Drives [`HttpExchangeMachine`] over a real simulated TCP pair (`Io::sim()` + `bstd::net`)
    /// through [`NetHttpHandler`] and [`crate::cx::Cx::drive_machine`], verifying request wire
    /// delivery, chunked response parsing, and linear connection teardown.
    #[test]
    fn net_http_handler_drives_http_exchange_over_sim_tcp_peer() {
        use crate::net::{Addr, HostPort};

        let alloc = blaze_own::testing::heap().alloc();
        let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let net = io.need_net().expect("sim net");
        let addr = Addr::Tcp(HostPort::parse("127.0.0.1:18080").expect("valid host:port"));
        let mut listener = net.listen(&cx, &addr).expect("listen");

        let client_conn = net.connect(&cx, &addr).expect("connect");
        let server_conn = listener.accept(&cx).expect("accept");

        // Pre-stage a chunked HTTP/1.1 response on the peer end of the simulated TCP pair.
        server_conn
            .send_all(
                &cx,
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n-bstd!\r\n0\r\n\r\n",
            )
            .expect("server send response");

        let conn_handle = test_conn();
        let head = sample_head();
        let (wire, machine) = HttpExchangeMachine::for_request(
            &head,
            b"",
            HttpExchangeOpts {
                conn: conn_handle,
                method: Method::Get,
                range: None,
                body_most: 64 << 10,
                reads_most: 16,
                wait_most: TIMEOUT,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        )
        .expect("encode request");

        let mut handler = NetHttpHandler::new(&cx, conn_handle, client_conn, wire.as_slice());
        let out = cx
            .drive_machine(
                "bstd.requests.net_http_handler",
                &mut handler,
                &mut (),
                machine,
                DriveOpts { max_steps: 16 },
            )
            .expect("net exchange succeeds");

        assert!(handler.is_closed(), "HttpExchangeMachine must close NetHttpHandler linearly");
        assert_eq!(handler.bytes_sent(), wire.len());
        handler.finish().expect("finish already-closed NetHttpHandler");

        assert_eq!(out.status, 200);
        assert_eq!(out.body.as_slice(), b"hello-bstd!");
        assert_eq!(out.range, None);

        // Verify the simulated TCP server received the exact encoded HTTP/1.1 request wire bytes.
        let mut req_buf = cx.bytes();
        let mut read_total = 0usize;
        while read_total < wire.len() {
            let n = server_conn.recv(&cx, 4096, &mut req_buf).expect("server recv request");
            if n == 0 {
                break;
            }
            read_total += n;
        }
        assert_eq!(req_buf.as_slice(), wire.as_slice());

        server_conn.finish(&cx).expect("server finish");
        listener.close(&cx).expect("listener close");
    }

    /// Verifies [`NetHttpHandler`] translates per-step timeouts into [`HttpExchangeError::Silent`],
    /// closes the underlying [`crate::net::Conn`] linearly, and rejects mismatched handles.
    #[test]
    fn net_http_handler_reports_step_timeout_and_closes_handle_linearly() {
        use crate::net::{Addr, HostPort};

        let alloc = blaze_own::testing::heap().alloc();
        let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let net = io.need_net().expect("sim net");
        let addr = Addr::Tcp(HostPort::parse("127.0.0.1:18081").expect("valid host:port"));
        let mut listener = net.listen(&cx, &addr).expect("listen");

        // 1. Stalling peer: server accepts and never responds -> per-step timeout -> Silent + Close.
        let client_conn = net.connect(&cx, &addr).expect("connect");
        let server_conn = listener.accept(&cx).expect("accept");

        let conn_handle = test_conn();
        let short_timeout = Duration::from_millis(25);
        let head = sample_head();
        let (wire, machine) = HttpExchangeMachine::for_request(
            &head,
            b"",
            HttpExchangeOpts {
                conn: conn_handle,
                method: Method::Get,
                range: None,
                body_most: 4096,
                reads_most: 4,
                wait_most: short_timeout,
                recv_chunk: CHUNK,
                alloc,
                plant: HttpExchangePlant::None,
            },
        )
        .expect("encode request");

        let mut handler = NetHttpHandler::new(&cx, conn_handle, client_conn, wire.as_slice());
        let res = cx.drive_machine(
            "bstd.requests.net_http_timeout",
            &mut handler,
            &mut (),
            machine,
            DriveOpts { max_steps: 16 },
        );
        assert!(
            matches!(
                res,
                Err(DriveError::Machine(HttpExchangeError::Silent { waited }))
                    if waited == short_timeout
            ),
            "expected Silent timeout error, got {res:?}"
        );
        assert!(handler.is_closed(), "timeout path must still emit Close on NetHttpHandler");
        handler.finish().expect("finish closed NetHttpHandler");
        server_conn.finish(&cx).expect("server finish");

        // 2. Mismatched ConnHandle is refused with NetHttpHandlerError::UnknownHandle and finish()
        //    still closes the linear Conn cleanly.
        let client_conn2 = net.connect(&cx, &addr).expect("connect 2");
        let server_conn2 = listener.accept(&cx).expect("accept 2");
        let wrong_handle = ConnHandle::new(99, NonZeroU32::new(2).expect("non-zero generation"));
        let mut handler2 = NetHttpHandler::new(&cx, conn_handle, client_conn2, wire.as_slice());
        let bad_res = cx.run_with(
            "bstd.requests.net_http_bad_handle",
            &mut handler2,
            HttpStepReq::Close { conn: wrong_handle },
            |ans| matches!(ans, HttpStepAns::Closed(Ok(()))),
        );
        assert!(
            matches!(
                bad_res,
                Err(crate::cx::RunError::Handler(NetHttpHandlerError::UnknownHandle {
                    expected,
                    got,
                })) if expected == conn_handle && got == wrong_handle
            ),
            "expected UnknownHandle error, got {bad_res:?}"
        );
        assert!(!handler2.is_closed());
        handler2.finish().expect("finish unclosed NetHttpHandler");
        server_conn2.finish(&cx).expect("server finish 2");
        listener.close(&cx).expect("listener close");
    }
}
