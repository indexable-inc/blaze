//! The frame layer (RFC 9113 sections 4 and 6): the 9-byte header, each frame's payload read
//! into a typed view of the input (no copy), and each frame written.
//!
//! Section 4.1, the frame format:
//!
//! ```text
//! HTTP Frame {
//!   Length (24),
//!   Type (8),
//!   Flags (8),
//!   Reserved (1),
//!   Stream Identifier (31),
//!   Frame Payload (..),
//! }
//! ```
//!
//! "Reserved: A reserved 1-bit field. The semantics of this bit are undefined, and the bit MUST
//! remain unset (0x00) when sending and MUST be ignored when receiving." "Implementations MUST
//! ignore and discard frames of unknown types" (section 5.5), and "Flags that have no defined
//! semantics for a particular frame type MUST be ignored and MUST be left unset (0x00) when
//! sending" (section 4.1).
//!
//! Frame types (section 6): DATA 0x00, HEADERS 0x01, PRIORITY 0x02, RST_STREAM 0x03, SETTINGS
//! 0x04, PUSH_PROMISE 0x05, PING 0x06, GOAWAY 0x07, WINDOW_UPDATE 0x08, CONTINUATION 0x09. Flags:
//! END_STREAM 0x01 (DATA, HEADERS), ACK 0x01 (SETTINGS, PING), END_HEADERS 0x04 (HEADERS,
//! PUSH_PROMISE, CONTINUATION), PADDED 0x08 (DATA, HEADERS, PUSH_PROMISE), PRIORITY 0x20
//! (HEADERS).

#![blaze::sans_io]

use blaze_own::{AVec, AllocFailure};

use crate::error::{ErrorCode, Rule};

/// The client's connection preface (section 3.4): "the string PRI *
/// HTTP/2.0\r\n\r\nSM\r\n\r\n", 24 octets, then a SETTINGS frame.
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// A frame header's length.
pub const HEADER_LEN: usize = 9;

/// The largest value of a window, a stream identifier and SETTINGS_INITIAL_WINDOW_SIZE: 2^31-1.
pub const MAX_31: u32 = (1 << 31) - 1;

/// A stream identifier (31 bits).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct StreamId(pub u32);

impl StreamId {
    /// The connection's own: frames that concern the whole connection.
    pub const ZERO: StreamId = StreamId(0);

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether a client opened it (section 5.1.1: "Streams initiated by a client MUST use
    /// odd-numbered stream identifiers").
    pub const fn by_client(self) -> bool {
        self.0 % 2 == 1
    }
}

/// A frame's type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameKind {
    Data,
    Headers,
    Priority,
    RstStream,
    Settings,
    PushPromise,
    Ping,
    GoAway,
    WindowUpdate,
    Continuation,
    /// A type this crate does not know: ignored (section 5.5).
    Unknown(u8),
}

impl FrameKind {
    pub const fn from_code(code: u8) -> FrameKind {
        match code {
            0x0 => FrameKind::Data,
            0x1 => FrameKind::Headers,
            0x2 => FrameKind::Priority,
            0x3 => FrameKind::RstStream,
            0x4 => FrameKind::Settings,
            0x5 => FrameKind::PushPromise,
            0x6 => FrameKind::Ping,
            0x7 => FrameKind::GoAway,
            0x8 => FrameKind::WindowUpdate,
            0x9 => FrameKind::Continuation,
            other => FrameKind::Unknown(other),
        }
    }

    pub const fn code(self) -> u8 {
        match self {
            FrameKind::Data => 0x0,
            FrameKind::Headers => 0x1,
            FrameKind::Priority => 0x2,
            FrameKind::RstStream => 0x3,
            FrameKind::Settings => 0x4,
            FrameKind::PushPromise => 0x5,
            FrameKind::Ping => 0x6,
            FrameKind::GoAway => 0x7,
            FrameKind::WindowUpdate => 0x8,
            FrameKind::Continuation => 0x9,
            FrameKind::Unknown(code) => code,
        }
    }
}

/// Frame flags (section 6).
pub mod flags {
    pub const END_STREAM: u8 = 0x01;
    pub const ACK: u8 = 0x01;
    pub const END_HEADERS: u8 = 0x04;
    pub const PADDED: u8 = 0x08;
    pub const PRIORITY: u8 = 0x20;
}

/// SETTINGS identifiers (section 6.5.2).
pub mod setting {
    pub const HEADER_TABLE_SIZE: u16 = 0x1;
    pub const ENABLE_PUSH: u16 = 0x2;
    pub const MAX_CONCURRENT_STREAMS: u16 = 0x3;
    pub const INITIAL_WINDOW_SIZE: u16 = 0x4;
    pub const MAX_FRAME_SIZE: u16 = 0x5;
    pub const MAX_HEADER_LIST_SIZE: u16 = 0x6;
}

/// A frame's header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub len: u32,
    pub kind: FrameKind,
    pub flags: u8,
    pub stream: StreamId,
}

impl Header {
    /// Whether `flag` is set.
    pub const fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// The header in `bytes`, the reserved bit dropped.
pub fn parse_header(bytes: &[u8; HEADER_LEN]) -> Header {
    let len = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
    let stream = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) & MAX_31;
    Header { len, kind: FrameKind::from_code(bytes[3]), flags: bytes[4], stream: StreamId(stream) }
}

/// Appends a frame header.
pub fn write_header(
    out: &mut AVec<'_, u8>,
    len: u32,
    kind: FrameKind,
    flags: u8,
    stream: StreamId,
) -> Result<(), AllocFailure> {
    let l = len.to_be_bytes();
    let s = (stream.0 & MAX_31).to_be_bytes();
    out.try_extend_from_slice(&[l[1], l[2], l[3], kind.code(), flags, s[0], s[1], s[2], s[3]])
}

/// The bytes of a payload past its padding (sections 6.1, 6.2): "Pad Length: An 8-bit field
/// containing the length of the frame padding in units of octets. ... If the length of the
/// padding is the length of the frame payload or greater, the recipient MUST treat this as a
/// connection error of type PROTOCOL_ERROR." Returns the bytes after the pad length byte and
/// before the padding.
pub fn unpad(header: Header, payload: &[u8]) -> Result<&[u8], Rule> {
    if !header.has(flags::PADDED) {
        return Ok(payload);
    }
    let Some((&pad, rest)) = payload.split_first() else {
        return Err(Rule::Padding { pad: 0, len: header.len });
    };
    let pad_len = usize::from(pad);
    if pad_len >= payload.len() {
        return Err(Rule::Padding { pad, len: header.len });
    }
    Ok(&rest[..rest.len() - pad_len])
}

/// A priority signal (section 6.3; parsed and then ignored, as section 5.3.2 allows: "the
/// prioritization signaling in RFC 7540 ... is deprecated").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Priority {
    pub exclusive: bool,
    pub depends_on: StreamId,
    pub weight: u8,
}

/// The 5 bytes of a priority.
pub fn parse_priority(bytes: [u8; 5]) -> Priority {
    let word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Priority { exclusive: word >> 31 == 1, depends_on: StreamId(word & MAX_31), weight: bytes[4] }
}

/// A HEADERS payload (section 6.2): padding off, the priority (when the PRIORITY flag says)
/// taken off the front, the field block fragment left.
pub fn headers_payload(header: Header, payload: &[u8]) -> Result<(Option<Priority>, &[u8]), Rule> {
    let rest = unpad(header, payload)?;
    if !header.has(flags::PRIORITY) {
        return Ok((None, rest));
    }
    let Some((five, fragment)) = rest.split_first_chunk::<5>() else {
        return Err(Rule::Length { len: header.len });
    };
    Ok((Some(parse_priority(*five)), fragment))
}

/// One 6-byte SETTINGS entry (section 6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SettingEntry {
    pub id: u16,
    pub value: u32,
}

/// The entries of a SETTINGS payload whose length is a multiple of 6.
pub fn settings_entries(payload: &[u8]) -> impl Iterator<Item = SettingEntry> + '_ {
    payload.chunks_exact(6).map(|c| SettingEntry {
        id: u16::from_be_bytes([c[0], c[1]]),
        value: u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
    })
}

/// A big-endian u32 from 4 bytes at `at` (the caller checked the length).
pub fn be32(payload: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([payload[at], payload[at + 1], payload[at + 2], payload[at + 3]])
}

/// Appends a DATA frame.
pub fn write_data(out: &mut AVec<'_, u8>, stream: StreamId, bytes: &[u8], end: bool) -> Result<(), AllocFailure> {
    out.try_reserve(HEADER_LEN + bytes.len())?;
    let flags = if end { flags::END_STREAM } else { 0 };
    write_header(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX), FrameKind::Data, flags, stream)?;
    out.try_extend_from_slice(bytes)
}

/// Appends a field block as one HEADERS frame and as many CONTINUATION frames as `max_frame`
/// needs (section 4.3: "Field blocks MUST be transmitted as a contiguous sequence of frames, with
/// no interleaved frames of any other type or from any other stream").
pub fn write_headers(
    out: &mut AVec<'_, u8>,
    stream: StreamId,
    block: &[u8],
    end_stream: bool,
    max_frame: u32,
) -> Result<(), AllocFailure> {
    let max = usize::try_from(max_frame).unwrap_or(usize::MAX).max(1);
    let pieces = block.len().div_ceil(max).max(1);
    out.try_reserve(block.len() + pieces * HEADER_LEN)?;
    let mut rest = block;
    let mut first = true;
    loop {
        let take = rest.len().min(max);
        let (piece, after) = rest.split_at(take);
        let last = after.is_empty();
        let mut f = if last { flags::END_HEADERS } else { 0 };
        let kind = if first {
            if end_stream {
                f |= flags::END_STREAM;
            }
            FrameKind::Headers
        } else {
            FrameKind::Continuation
        };
        write_header(out, u32::try_from(take).unwrap_or(u32::MAX), kind, f, stream)?;
        out.try_extend_from_slice(piece)?;
        if last {
            return Ok(());
        }
        rest = after;
        first = false;
    }
}

/// Appends a SETTINGS frame of `entries`.
pub fn write_settings(out: &mut AVec<'_, u8>, entries: &[SettingEntry]) -> Result<(), AllocFailure> {
    out.try_reserve(HEADER_LEN + entries.len() * 6)?;
    write_header(out, u32::try_from(entries.len() * 6).unwrap_or(u32::MAX), FrameKind::Settings, 0, StreamId::ZERO)?;
    for entry in entries {
        out.try_extend_from_slice(&entry.id.to_be_bytes())?;
        out.try_extend_from_slice(&entry.value.to_be_bytes())?;
    }
    Ok(())
}

/// Appends a SETTINGS ACK (section 6.5: "the ACK flag ... the frame's payload MUST be empty").
pub fn write_settings_ack(out: &mut AVec<'_, u8>) -> Result<(), AllocFailure> {
    write_header(out, 0, FrameKind::Settings, flags::ACK, StreamId::ZERO)
}

/// Appends a PING (section 6.7: 8 octets of opaque data).
pub fn write_ping(out: &mut AVec<'_, u8>, data: [u8; 8], ack: bool) -> Result<(), AllocFailure> {
    write_header(out, 8, FrameKind::Ping, if ack { flags::ACK } else { 0 }, StreamId::ZERO)?;
    out.try_extend_from_slice(&data)
}

/// Appends a GOAWAY with no debug data (section 6.8).
pub fn write_goaway(out: &mut AVec<'_, u8>, last: StreamId, code: ErrorCode) -> Result<(), AllocFailure> {
    write_header(out, 8, FrameKind::GoAway, 0, StreamId::ZERO)?;
    out.try_extend_from_slice(&(last.0 & MAX_31).to_be_bytes())?;
    out.try_extend_from_slice(&code.0.to_be_bytes())
}

/// Appends a RST_STREAM (section 6.4).
pub fn write_rst(out: &mut AVec<'_, u8>, stream: StreamId, code: ErrorCode) -> Result<(), AllocFailure> {
    write_header(out, 4, FrameKind::RstStream, 0, stream)?;
    out.try_extend_from_slice(&code.0.to_be_bytes())
}

/// Appends a WINDOW_UPDATE (section 6.9).
pub fn write_window_update(out: &mut AVec<'_, u8>, stream: StreamId, increment: u32) -> Result<(), AllocFailure> {
    write_header(out, 4, FrameKind::WindowUpdate, 0, stream)?;
    out.try_extend_from_slice(&(increment & MAX_31).to_be_bytes())
}

/// Appends a PRIORITY frame (section 6.3); only tests send one.
pub fn write_priority(out: &mut AVec<'_, u8>, stream: StreamId, priority: Priority) -> Result<(), AllocFailure> {
    write_header(out, 5, FrameKind::Priority, 0, stream)?;
    let word = (priority.depends_on.0 & MAX_31) | if priority.exclusive { 1 << 31 } else { 0 };
    out.try_extend_from_slice(&word.to_be_bytes())?;
    out.try_extend_from_slice(&[priority.weight])
}
