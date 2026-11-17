//! The stream state machine of RFC 9113 section 5.1, as one pure function ([`next`]).
//!
//! The section's figure ("Figure 2: Stream States"), transcribed as a table (TO VERIFY AGAINST
//! THE RFC TEXT: transcribed from memory of the figure; each row below is also stated in the
//! section's prose, which the table's notes quote). "send" is this endpoint sending the frame,
//! "recv" receiving it; H is a HEADERS frame (with its CONTINUATIONs), ES the END_STREAM flag, R
//! a RST_STREAM frame, PP a PUSH_PROMISE (whose transitions are the promised stream's):
//!
//! | from              | on                 | to                 |
//! |-------------------|--------------------|--------------------|
//! | idle              | send H, recv H     | open               |
//! | idle              | send PP            | reserved (local)   |
//! | idle              | recv PP            | reserved (remote)  |
//! | reserved (local)  | send H             | half-closed (remote) |
//! | reserved (local)  | send R, recv R     | closed             |
//! | reserved (remote) | recv H             | half-closed (local) |
//! | reserved (remote) | send R, recv R     | closed             |
//! | open              | send ES            | half-closed (local) |
//! | open              | recv ES            | half-closed (remote) |
//! | open              | send R, recv R     | closed             |
//! | half-closed (local)  | recv ES, send R, recv R | closed     |
//! | half-closed (remote) | send ES, send R, recv R | closed     |
//!
//! A HEADERS carrying END_STREAM is H then ES. A HEADERS on an open or half-closed stream (an
//! informational response, the final one, trailers) changes no state but by its ES.
//!
//! The prose of the section, for what the table does not draw:
//! - idle: "Receiving any frame other than HEADERS or PRIORITY on a stream in this state MUST be
//!   treated as a connection error of type PROTOCOL_ERROR."
//! - half-closed (local): "An endpoint can receive any type of frame in this state."
//! - half-closed (remote): "If an endpoint receives additional frames, other than
//!   WINDOW_UPDATE, PRIORITY, or RST_STREAM, for a stream that is in this state, it MUST respond
//!   with a stream error of type STREAM_CLOSED."
//! - closed: "An endpoint MUST NOT send frames other than PRIORITY on a closed stream. ... An
//!   endpoint MUST minimally process and then discard any frames it receives in this state. This
//!   means updating header compression state for HEADERS and PUSH_PROMISE frames. ... DATA frames
//!   ... count toward the connection flow-control window ... An endpoint can perform this minimal
//!   processing for all streams that are in the "closed" state." This crate does that for every
//!   closed stream, so it keeps no record of closed streams.

#![blaze::sans_io]

/// A stream's state (section 5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Idle,
    ReservedLocal,
    ReservedRemote,
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

/// What happens to a stream, by this endpoint's view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    SendHeaders,
    RecvHeaders,
    SendEnd,
    RecvEnd,
    SendReset,
    RecvReset,
    SendPushPromise,
    RecvPushPromise,
}

/// Every step, for the exhaustive test.
pub const STEPS: [Step; 8] = [
    Step::SendHeaders,
    Step::RecvHeaders,
    Step::SendEnd,
    Step::RecvEnd,
    Step::SendReset,
    Step::RecvReset,
    Step::SendPushPromise,
    Step::RecvPushPromise,
];

/// Every state, for the exhaustive test.
pub const STATES: [State; 7] = [
    State::Idle,
    State::ReservedLocal,
    State::ReservedRemote,
    State::Open,
    State::HalfClosedLocal,
    State::HalfClosedRemote,
    State::Closed,
];

/// The state after `step`, or None when the table has no such edge (the connection decides
/// which error that is, by the prose above).
pub const fn next(state: State, step: Step) -> Option<State> {
    use State::*;
    use Step::*;
    Some(match (state, step) {
        (Idle, SendHeaders | RecvHeaders) => Open,
        (Idle, SendPushPromise) => ReservedLocal,
        (Idle, RecvPushPromise) => ReservedRemote,
        (ReservedLocal, SendHeaders) => HalfClosedRemote,
        (ReservedRemote, RecvHeaders) => HalfClosedLocal,
        (ReservedLocal | ReservedRemote, SendReset | RecvReset) => Closed,
        (Open, SendEnd) => HalfClosedLocal,
        (Open, RecvEnd) => HalfClosedRemote,
        (Open, SendHeaders | RecvHeaders) => Open,
        (Open, SendReset | RecvReset) => Closed,
        (HalfClosedLocal, RecvEnd | SendReset | RecvReset) => Closed,
        (HalfClosedLocal, RecvHeaders) => HalfClosedLocal,
        (HalfClosedRemote, SendEnd | SendReset | RecvReset) => Closed,
        (HalfClosedRemote, SendHeaders) => HalfClosedRemote,
        _ => return None,
    })
}

/// Whether this endpoint may still send DATA or HEADERS on it.
pub const fn sends(state: State) -> bool {
    matches!(state, State::Open | State::HalfClosedRemote | State::ReservedLocal)
}

/// Whether the peer may still send DATA or HEADERS on it.
pub const fn receives(state: State) -> bool {
    matches!(state, State::Open | State::HalfClosedLocal | State::ReservedRemote)
}
