//! Why a frame, a stream or a call was refused (rule 19): the RFC's error code, the frame it was
//! found in, the stream, and the rule that refused it, each rule naming its RFC section.
//!
//! RFC 9113 section 5.4: "A stream error is an error related to a specific stream that does not
//! affect processing of other streams" (a RST_STREAM, here an [`crate::Event::Reset`]); "a
//! connection error is any error that prevents further processing of the frame layer or corrupts
//! any connection state" (a GOAWAY, here [`H2Error::Connection`]). Section 7 lists the codes.

#![blaze::sans_io]

use core::fmt;

use blaze_own::AllocFailure;

use crate::{
    conn::Role,
    frame::{FrameKind, StreamId},
    head::{HeadFault, HeadWhy},
    hpack::HpackFault,
    settings::LimitsFault,
    stream::State,
};

/// An error code of RFC 9113 section 7. A code this crate does not know is kept as it came: the
/// section says "Unknown or unsupported error codes MUST NOT trigger any special behavior".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrorCode(pub u32);

impl ErrorCode {
    pub const NO_ERROR: ErrorCode = ErrorCode(0x0);
    pub const PROTOCOL_ERROR: ErrorCode = ErrorCode(0x1);
    pub const INTERNAL_ERROR: ErrorCode = ErrorCode(0x2);
    pub const FLOW_CONTROL_ERROR: ErrorCode = ErrorCode(0x3);
    pub const SETTINGS_TIMEOUT: ErrorCode = ErrorCode(0x4);
    pub const STREAM_CLOSED: ErrorCode = ErrorCode(0x5);
    pub const FRAME_SIZE_ERROR: ErrorCode = ErrorCode(0x6);
    pub const REFUSED_STREAM: ErrorCode = ErrorCode(0x7);
    pub const CANCEL: ErrorCode = ErrorCode(0x8);
    pub const COMPRESSION_ERROR: ErrorCode = ErrorCode(0x9);
    pub const CONNECT_ERROR: ErrorCode = ErrorCode(0xa);
    pub const ENHANCE_YOUR_CALM: ErrorCode = ErrorCode(0xb);
    pub const INADEQUATE_SECURITY: ErrorCode = ErrorCode(0xc);
    pub const HTTP_1_1_REQUIRED: ErrorCode = ErrorCode(0xd);

    /// The RFC's name of the code, or `None` for one it does not define.
    pub const fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0x0 => "NO_ERROR",
            0x1 => "PROTOCOL_ERROR",
            0x2 => "INTERNAL_ERROR",
            0x3 => "FLOW_CONTROL_ERROR",
            0x4 => "SETTINGS_TIMEOUT",
            0x5 => "STREAM_CLOSED",
            0x6 => "FRAME_SIZE_ERROR",
            0x7 => "REFUSED_STREAM",
            0x8 => "CANCEL",
            0x9 => "COMPRESSION_ERROR",
            0xa => "CONNECT_ERROR",
            0xb => "ENHANCE_YOUR_CALM",
            0xc => "INADEQUATE_SECURITY",
            0xd => "HTTP_1_1_REQUIRED",
            _ => return None,
        })
    }
}

impl fmt::Debug for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "ErrorCode({:#x})", self.0),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// The rule a frame or a stream broke. Each names its section of RFC 9113 (or RFC 7541, or, for
/// a cap of ours, the section that allows the limit) in [`Rule::section`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rule {
    /// The client's 24 bytes were not `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` (server role).
    Preface,
    /// The peer's first frame was not a SETTINGS frame.
    FirstNotSettings,
    /// A frame longer than the SETTINGS_MAX_FRAME_SIZE we sent.
    FrameOverMax { len: u32, most: u32 },
    /// A frame whose payload has a fixed length (PRIORITY 5, RST_STREAM 4, PING 8,
    /// WINDOW_UPDATE 4, a SETTINGS ACK 0, SETTINGS a multiple of 6, GOAWAY at least 8) had another.
    Length { len: u32 },
    /// A frame that belongs to a stream came on stream 0.
    StreamZero,
    /// A frame that belongs to the connection came on a stream other than 0.
    StreamNotZero,
    /// A pad length not less than what follows it.
    Padding { pad: u8, len: u32 },
    /// A field block not ended by END_HEADERS was followed by a frame other than CONTINUATION
    /// on its stream.
    ContinuationExpected,
    /// A CONTINUATION that follows no open field block.
    ContinuationUnexpected,
    /// A setting's value outside the RFC's range.
    SettingsValue { id: u16, value: u32 },
    /// A SETTINGS ACK when no SETTINGS of ours waited for one.
    SettingsAckUnasked,
    /// A PUSH_PROMISE, which we refused by sending SETTINGS_ENABLE_PUSH = 0.
    PushRefused,
    /// A WINDOW_UPDATE whose increment is 0.
    WindowIncrementZero,
    /// A WINDOW_UPDATE that takes a window past 2^31-1.
    WindowOverflow,
    /// DATA past the window we gave.
    FlowControl { len: u32, window: i64 },
    /// A frame other than HEADERS or PRIORITY on an idle stream, or a HEADERS on an idle stream
    /// only we may open.
    Idle,
    /// A new stream whose identifier is not above every one the peer opened before.
    StreamIdOrder { id: u32, highest: u32 },
    /// A new stream whose identifier has the parity of the other role.
    StreamIdParity { id: u32 },
    /// DATA or HEADERS after the peer's END_STREAM.
    HalfClosedRemote,
    /// A stream made to depend on itself.
    SelfDependency,
    /// A field block HPACK could not decode.
    Compression(HpackFault),
    /// A malformed request or response (section 8.1.1).
    Malformed(HeadWhy),
    /// A body whose length is not its `content-length`.
    ContentLength { said: u64, got: u64 },
    /// A GOAWAY whose last stream identifier is above an earlier GOAWAY's.
    GoAwayLastIncreased { before: u32, now: u32 },
    /// A stream past the SETTINGS_MAX_CONCURRENT_STREAMS we sent.
    StreamsOverCap { most: u32 },
    /// A decoded field list past our cap (RFC 7541 accounting: name, value and 32 per field).
    HeaderListOverCap { most: u32 },
    /// A field block (HEADERS and its CONTINUATIONs) past our cap on encoded bytes.
    HeaderBlockOverCap { most: u32 },
    /// More CONTINUATION frames in one field block than our cap.
    ContinuationsOverCap { most: u16 },
    /// More RST_STREAM frames from the peer than our budget allows: the rapid reset.
    RapidReset { most: u16 },
    /// More SETTINGS and PING acknowledgements owed and not yet transmitted than our cap.
    AcksOverCap { most: u16 },
    /// More RST_STREAM frames owed to the peer and not yet transmitted than our cap.
    ResetsOverCap { most: u16 },
    /// More informational (1xx) responses on one stream than our cap.
    InformationalOverCap { most: u8 },
    /// Our SETTINGS were not acknowledged by their deadline.
    SettingsTimeout,
    /// A stream had bytes to send and no window for longer than our stall deadline.
    SendStalled,
}

impl Rule {
    /// The RFC section the rule comes from.
    pub const fn section(self) -> &'static str {
        match self {
            Rule::Preface | Rule::FirstNotSettings => "RFC 9113 section 3.4",
            Rule::FrameOverMax { .. } => "RFC 9113 section 4.2",
            Rule::Length { .. } => "RFC 9113 sections 6.3 to 6.9",
            Rule::StreamZero | Rule::StreamNotZero => "RFC 9113 section 6",
            Rule::Padding { .. } => "RFC 9113 sections 6.1 and 6.2",
            Rule::ContinuationExpected | Rule::ContinuationUnexpected => "RFC 9113 section 6.10",
            Rule::SettingsValue { .. } => "RFC 9113 section 6.5.2",
            Rule::SettingsAckUnasked => "RFC 9113 section 6.5",
            Rule::PushRefused => "RFC 9113 sections 6.6 and 8.4",
            Rule::WindowIncrementZero => "RFC 9113 section 6.9",
            Rule::WindowOverflow | Rule::FlowControl { .. } => "RFC 9113 section 6.9.1",
            Rule::Idle | Rule::HalfClosedRemote => "RFC 9113 section 5.1",
            Rule::StreamIdOrder { .. } | Rule::StreamIdParity { .. } => "RFC 9113 section 5.1.1",
            Rule::SelfDependency => "RFC 9113 section 5.3.1",
            Rule::Compression(_) => "RFC 9113 section 4.3; RFC 7541",
            Rule::Malformed(_) | Rule::ContentLength { .. } => "RFC 9113 section 8.1.1",
            Rule::GoAwayLastIncreased { .. } => "RFC 9113 section 6.8",
            Rule::StreamsOverCap { .. } => "RFC 9113 section 5.1.2",
            Rule::HeaderListOverCap { .. } => "RFC 9113 section 10.5.1",
            Rule::HeaderBlockOverCap { .. }
            | Rule::ContinuationsOverCap { .. }
            | Rule::RapidReset { .. }
            | Rule::AcksOverCap { .. }
            | Rule::ResetsOverCap { .. }
            | Rule::InformationalOverCap { .. } => "RFC 9113 section 10.5",
            Rule::SettingsTimeout => "RFC 9113 section 6.5.3",
            Rule::SendStalled => "RFC 9113 section 5.2.2",
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Rule::Preface => f.write_str("the client's connection preface is not `PRI * HTTP/2.0`"),
            Rule::FirstNotSettings => f.write_str("the peer's first frame is not SETTINGS"),
            Rule::FrameOverMax { len, most } => write!(f, "a frame of {len} bytes, past the {most} we allow"),
            Rule::Length { len } => write!(f, "a payload of {len} bytes where the frame's length is fixed"),
            Rule::StreamZero => f.write_str("a stream's frame on stream 0"),
            Rule::StreamNotZero => f.write_str("a connection's frame on a stream other than 0"),
            Rule::Padding { pad, len } => write!(f, "a pad length of {pad} in a payload of {len} bytes"),
            Rule::ContinuationExpected => f.write_str("a frame other than CONTINUATION inside a field block"),
            Rule::ContinuationUnexpected => f.write_str("a CONTINUATION with no field block open"),
            Rule::SettingsValue { id, value } => write!(f, "setting {id:#x} = {value}, outside its range"),
            Rule::SettingsAckUnasked => f.write_str("a SETTINGS ACK with no SETTINGS of ours unacknowledged"),
            Rule::PushRefused => f.write_str("a PUSH_PROMISE after we sent SETTINGS_ENABLE_PUSH = 0"),
            Rule::WindowIncrementZero => f.write_str("a WINDOW_UPDATE of 0"),
            Rule::WindowOverflow => f.write_str("a WINDOW_UPDATE that takes the window past 2^31-1"),
            Rule::FlowControl { len, window } => write!(f, "DATA of {len} bytes into a window of {window}"),
            Rule::Idle => f.write_str("a frame on an idle stream the peer may not open"),
            Rule::StreamIdOrder { id, highest } => write!(f, "new stream {id} is not above {highest}"),
            Rule::StreamIdParity { id } => write!(f, "new stream {id} has the other role's parity"),
            Rule::HalfClosedRemote => f.write_str("DATA or HEADERS after the peer's END_STREAM"),
            Rule::SelfDependency => f.write_str("a stream made to depend on itself"),
            Rule::Compression(fault) => write!(f, "HPACK: {fault}"),
            Rule::Malformed(why) => write!(f, "a malformed message: {why}"),
            Rule::ContentLength { said, got } => write!(f, "content-length {said}, body of {got} bytes"),
            Rule::GoAwayLastIncreased { before, now } => write!(f, "GOAWAY's last stream rose from {before} to {now}"),
            Rule::StreamsOverCap { most } => write!(f, "past the {most} concurrent streams we allow"),
            Rule::HeaderListOverCap { most } => write!(f, "a field list past the {most} bytes we allow"),
            Rule::HeaderBlockOverCap { most } => write!(f, "a field block past the {most} encoded bytes we allow"),
            Rule::ContinuationsOverCap { most } => write!(f, "more than {most} CONTINUATION frames in one block"),
            Rule::RapidReset { most } => write!(f, "more than {most} RST_STREAM frames from the peer in the window"),
            Rule::AcksOverCap { most } => write!(f, "more than {most} SETTINGS or PING acknowledgements owed"),
            Rule::ResetsOverCap { most } => write!(f, "more than {most} RST_STREAM frames owed to the peer"),
            Rule::InformationalOverCap { most } => write!(f, "more than {most} informational responses"),
            Rule::SettingsTimeout => f.write_str("our SETTINGS were not acknowledged in time"),
            Rule::SendStalled => f.write_str("no window to send for longer than the stall deadline"),
        }?;
        write!(f, " ({})", self.section())
    }
}

/// A broken rule: the code sent with it, the frame it was found in (none for a deadline), the
/// stream (0 for the connection), and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fault {
    pub code: ErrorCode,
    pub frame: Option<FrameKind>,
    pub stream: StreamId,
    pub rule: Rule,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on stream {}", self.code, self.stream.0)?;
        if let Some(frame) = self.frame {
            write!(f, " in a {frame:?} frame")?;
        }
        write!(f, ": {}", self.rule)
    }
}

/// Something the caller asked for that the protocol, the peer's settings or the caller's own
/// limits do not allow now. Nothing was sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// A call only the other role makes (a request on a server, a response on a client).
    WrongRole { needs: Role },
    /// As many streams open as the peer's SETTINGS_MAX_CONCURRENT_STREAMS allows: wait for one
    /// to end.
    StreamsFull { most: u32 },
    /// Every stream identifier of this role is used (2^31 - 1 is the last).
    IdsExhausted,
    /// The peer sent GOAWAY, or we did: no new stream.
    GoingAway,
    /// No such stream open (never opened, or ended and gone).
    NoSuchStream { stream: StreamId },
    /// The stream cannot carry this now (its state says why).
    NotSendable { stream: StreamId, state: State },
    /// A response's body or trailers before its head (server role).
    HeadFirst { stream: StreamId },
    /// A head the RFC does not allow.
    Head(HeadFault),
    /// A head larger than the peer's SETTINGS_MAX_HEADER_LIST_SIZE (RFC 7541 section 4.1
    /// accounting).
    HeadOverPeerLimit { size: u64, most: u32 },
    /// More PINGs of ours unanswered than the cap.
    PingsFull { most: u16 },
    /// Limits outside what the RFC allows.
    Limits(LimitsFault),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refusal::WrongRole { needs } => write!(f, "only the {needs:?} role may do this"),
            Refusal::StreamsFull { most } => write!(f, "the peer allows {most} concurrent streams, all open"),
            Refusal::IdsExhausted => f.write_str("every stream identifier is used"),
            Refusal::GoingAway => f.write_str("the connection is going away: no new stream"),
            Refusal::NoSuchStream { stream } => write!(f, "no stream {} is open", stream.0),
            Refusal::NotSendable { stream, state } => write!(f, "stream {} is {state:?}", stream.0),
            Refusal::HeadFirst { stream } => write!(f, "stream {}'s response head is not sent yet", stream.0),
            Refusal::Head(fault) => write!(f, "{fault}"),
            Refusal::HeadOverPeerLimit { size, most } => {
                write!(f, "a head of {size} bytes, past the peer's SETTINGS_MAX_HEADER_LIST_SIZE of {most}")
            }
            Refusal::PingsFull { most } => write!(f, "{most} PINGs unanswered"),
            Refusal::Limits(fault) => write!(f, "{fault}"),
        }
    }
}

/// Why a call on a [`crate::Connection`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum H2Error {
    /// The peer broke a rule that ends the connection, or a connection deadline passed: a GOAWAY
    /// with the fault's code is queued for [`crate::Connection::transmit`], and the connection
    /// takes nothing more.
    Connection(Fault),
    /// The connection ended before this call (an earlier fault, or both sides' GOAWAY done).
    Closed,
    /// The caller asked for something not allowed now.
    Refused(Refusal),
    /// A buffer could not be had from the allocator passed in.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for H2Error {
    fn from(failure: AllocFailure) -> H2Error {
        H2Error::Alloc(failure)
    }
}

impl From<Refusal> for H2Error {
    fn from(refusal: Refusal) -> H2Error {
        H2Error::Refused(refusal)
    }
}

impl fmt::Display for H2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("blaze_h2: ")?;
        match self {
            H2Error::Connection(fault) => write!(f, "connection error {fault}"),
            H2Error::Closed => f.write_str("the connection is closed"),
            H2Error::Refused(refusal) => write!(f, "refused: {refusal}"),
            H2Error::Alloc(failure) => write!(f, "{failure}"),
        }
    }
}

impl core::error::Error for H2Error {}
