//! Pure `#![no_std]` sans-IO RFC 6455 WebSocket framing, message reassembly, and handshake core.
//!
//! - Encodes and decodes 7-bit, 16-bit, and 64-bit extended payload lengths with optional
//!   client masking ([`encode_frame_into`], [`WsDecoder`]).
//! - Streaming [`WsDecoder`] handles arbitrary byte-chunk boundaries, reassembles fragmented
//!   `Text`/`Binary` messages, permits interleaved control frames per RFC 6455 §5.4, validates
//!   UTF-8 on `Text` and `Close` reasons, and enforces [`WsLimits`] before allocating payloads.
//! - Computes the RFC 6455 §1.3 `Sec-WebSocket-Accept` header via [`accept_key`] using a
//!   self-contained pure-core SHA-1 and `blaze_base64::STANDARD`.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

const WS_ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// RFC 6455 §5.2 WebSocket frame opcode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opcode {
    /// `0x0`: Continuation frame.
    Continuation,
    /// `0x1`: UTF-8 text data frame.
    Text,
    /// `0x2`: Binary data frame.
    Binary,
    /// `0x8`: Connection close control frame.
    Close,
    /// `0x9`: Ping control frame.
    Ping,
    /// `0xA`: Pong control frame.
    Pong,
}

impl Opcode {
    /// Decode a 4-bit wire opcode.
    #[inline]
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0x0 => Some(Self::Continuation),
            0x1 => Some(Self::Text),
            0x2 => Some(Self::Binary),
            0x8 => Some(Self::Close),
            0x9 => Some(Self::Ping),
            0xA => Some(Self::Pong),
            _ => None,
        }
    }

    /// Encode as a 4-bit wire opcode.
    #[inline]
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Continuation => 0x0,
            Self::Text => 0x1,
            Self::Binary => 0x2,
            Self::Close => 0x8,
            Self::Ping => 0x9,
            Self::Pong => 0xA,
        }
    }

    /// True for control opcodes (`Close`, `Ping`, `Pong`; MSB `0x8` set).
    #[inline]
    #[must_use]
    pub const fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

/// RFC 6455 §7.4 WebSocket close status code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CloseCode(pub u16);

impl CloseCode {
    /// 1000: Normal closure.
    pub const NORMAL: Self = Self(1000);
    /// 1001: Endpoint going away.
    pub const GOING_AWAY: Self = Self(1001);
    /// 1002: Protocol error.
    pub const PROTOCOL_ERROR: Self = Self(1002);
    /// 1003: Unsupported data type.
    pub const UNSUPPORTED: Self = Self(1003);
    /// 1007: Invalid frame payload data (e.g. non-UTF-8 in text message).
    pub const INVALID_PAYLOAD: Self = Self(1007);
    /// 1008: Policy violation.
    pub const POLICY: Self = Self(1008);
    /// 1009: Message too big.
    pub const TOO_BIG: Self = Self(1009);
    /// 1011: Unexpected server condition / internal error.
    pub const INTERNAL: Self = Self(1011);

    /// Return the raw 16-bit status code.
    #[inline]
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// True if this close code is permitted on the wire per RFC 6455 §7.4.
    #[inline]
    #[must_use]
    pub const fn is_valid_wire(self) -> bool {
        matches!(self.0, 1000..=1003 | 1007..=1014 | 3000..=4999)
    }
}

impl fmt::Display for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Bounds enforced while decoding untrusted WebSocket streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WsLimits {
    /// Maximum payload bytes permitted in a single wire frame.
    pub most_frame_bytes: usize,
    /// Maximum reassembled payload bytes permitted in a single message.
    pub most_message_bytes: usize,
    /// Maximum number of fragments permitted in a single fragmented message.
    pub most_fragments: usize,
}

impl WsLimits {
    /// Generous default bounds (16 MiB per frame, 64 MiB per message, 4,096 fragments).
    pub const USUAL: Self = Self {
        most_frame_bytes: 16 << 20,
        most_message_bytes: 64 << 20,
        most_fragments: 4096,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// A single decoded RFC 6455 WebSocket wire frame (unmasked).
pub struct Frame<A: Allocator> {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: Vec<u8, A>,
}

impl<A: Allocator> fmt::Debug for Frame<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("fin", &self.fin)
            .field("opcode", &self.opcode)
            .field("payload", &self.payload.as_slice())
            .finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Frame<B>> for Frame<A> {
    fn eq(&self, other: &Frame<B>) -> bool {
        self.fin == other.fin
            && self.opcode == other.opcode
            && self.payload.as_slice() == other.payload.as_slice()
    }
}

impl<A: Allocator> Eq for Frame<A> {}

/// A complete, reassembled RFC 6455 WebSocket message.
pub enum Message<A: Allocator> {
    Text(String<A>),
    Binary(Vec<u8, A>),
    Ping(Vec<u8, A>),
    Pong(Vec<u8, A>),
    Close { code: Option<CloseCode>, reason: String<A> },
}

impl<A: Allocator> fmt::Debug for Message<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::Text(s) => f.debug_tuple("Text").field(&s.as_str()).finish(),
            Message::Binary(b) => f.debug_tuple("Binary").field(&b.as_slice()).finish(),
            Message::Ping(b) => f.debug_tuple("Ping").field(&b.as_slice()).finish(),
            Message::Pong(b) => f.debug_tuple("Pong").field(&b.as_slice()).finish(),
            Message::Close { code, reason } => f
                .debug_struct("Close")
                .field("code", code)
                .field("reason", &reason.as_str())
                .finish(),
        }
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Message<B>> for Message<A> {
    fn eq(&self, other: &Message<B>) -> bool {
        match (self, other) {
            (Message::Text(a), Message::Text(b)) => a.as_str() == b.as_str(),
            (Message::Binary(a), Message::Binary(b))
            | (Message::Ping(a), Message::Ping(b))
            | (Message::Pong(a), Message::Pong(b)) => a.as_slice() == b.as_slice(),
            (
                Message::Close { code: ca, reason: ra },
                Message::Close { code: cb, reason: rb },
            ) => ca == cb && ra.as_str() == rb.as_str(),
            _ => false,
        }
    }
}

impl<A: Allocator> Eq for Message<A> {}

/// Why WebSocket frame encoding or stream decoding failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WsError {
    /// One or more RSV1/RSV2/RSV3 header bits were non-zero without a negotiated extension.
    NonZeroRsv { rsv: u8 },
    /// Frame header carried an unknown or reserved 4-bit opcode.
    InvalidOpcode { opcode: u8 },
    /// Control frame had `FIN = 0` (forbidden by RFC 6455 §5.5).
    FragmentedControlFrame { opcode: Opcode },
    /// Control frame payload exceeded 125 bytes (forbidden by RFC 6455 §5.5).
    ControlFrameTooLarge { len: usize },
    /// Received a continuation frame while no fragmented message was open.
    UnexpectedContinuation,
    /// Received a new data frame while a fragmented message was still open.
    ExpectedContinuation { opcode: Opcode },
    /// 64-bit payload length had its most significant bit set (forbidden by RFC 6455 §5.2).
    InvalidPayloadLength,
    /// Single wire frame exceeded [`WsLimits::most_frame_bytes`].
    FrameTooLarge { len: u64, limit: usize },
    /// Reassembled message exceeded [`WsLimits::most_message_bytes`].
    MessageTooLarge { len: usize, limit: usize },
    /// Fragmented message exceeded [`WsLimits::most_fragments`].
    TooManyFragments { limit: usize },
    /// Text message or Close reason payload was not valid UTF-8.
    InvalidUtf8,
    /// Close frame payload had length 1 (RFC 6455 §5.5.1 requires 0 or >= 2 bytes).
    InvalidClosePayload,
    /// Close frame carried a status code forbidden on the wire per RFC 6455 §7.4.
    InvalidCloseCode { code: u16 },
    /// Caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for WsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WsError::NonZeroRsv { rsv } => {
                write!(f, "bstd::ws: non-zero RSV bits (0b{rsv:03b}) without extension")
            }
            WsError::InvalidOpcode { opcode } => {
                write!(f, "bstd::ws: reserved or unknown opcode 0x{opcode:x}")
            }
            WsError::FragmentedControlFrame { opcode } => {
                write!(f, "bstd::ws: control frame {opcode:?} must not be fragmented")
            }
            WsError::ControlFrameTooLarge { len } => {
                write!(f, "bstd::ws: control frame payload length {len} exceeds 125 bytes")
            }
            WsError::UnexpectedContinuation => {
                f.write_str("bstd::ws: unexpected continuation frame with no open message")
            }
            WsError::ExpectedContinuation { opcode } => {
                write!(f, "bstd::ws: expected continuation frame, got {opcode:?}")
            }
            WsError::InvalidPayloadLength => {
                f.write_str("bstd::ws: 64-bit payload length has most significant bit set")
            }
            WsError::FrameTooLarge { len, limit } => {
                write!(f, "bstd::ws: frame payload {len} bytes exceeds limit {limit}")
            }
            WsError::MessageTooLarge { len, limit } => {
                write!(f, "bstd::ws: message payload {len} bytes exceeds limit {limit}")
            }
            WsError::TooManyFragments { limit } => {
                write!(f, "bstd::ws: fragmented message exceeds limit of {limit} fragments")
            }
            WsError::InvalidUtf8 => f.write_str("bstd::ws: invalid UTF-8 in text or close reason"),
            WsError::InvalidClosePayload => {
                f.write_str("bstd::ws: close frame payload cannot be 1 byte")
            }
            WsError::InvalidCloseCode { code } => {
                write!(f, "bstd::ws: invalid wire close status code {code}")
            }
            WsError::Alloc(err) => write!(f, "bstd::ws: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for WsError {}

/// Encode a single RFC 6455 WebSocket frame and append it to `out`.
pub fn encode_frame_into<A: Allocator>(
    fin: bool,
    opcode: Opcode,
    payload: &[u8],
    mask: Option<[u8; 4]>,
    out: &mut Vec<u8, A>,
) -> Result<(), WsError> {
    if opcode.is_control() {
        if !fin {
            return Err(WsError::FragmentedControlFrame { opcode });
        }
        if payload.len() > 125 {
            return Err(WsError::ControlFrameTooLarge { len: payload.len() });
        }
    }

    let b0 = (if fin { 0x80u8 } else { 0u8 }) | opcode.to_u8();
    let mask_bit = if mask.is_some() { 0x80u8 } else { 0u8 };

    let header_extra = if payload.len() <= 125 {
        2usize
    } else if payload.len() <= usize::from(u16::MAX) {
        4usize
    } else {
        10usize
    };
    let mask_len = if mask.is_some() { 4usize } else { 0usize };
    let needed = header_extra
        .saturating_add(mask_len)
        .saturating_add(payload.len());
    out.try_reserve(needed).map_err(WsError::Alloc)?;

    out.try_push(b0).map_err(|r| WsError::Alloc(r.why))?;

    if payload.len() <= 125 {
        let len7 = u8::try_from(payload.len()).map_err(|_| WsError::InvalidPayloadLength)?;
        out.try_push(mask_bit | len7)
            .map_err(|r| WsError::Alloc(r.why))?;
    } else if payload.len() <= usize::from(u16::MAX) {
        let len16 = u16::try_from(payload.len()).map_err(|_| WsError::InvalidPayloadLength)?;
        out.try_push(mask_bit | 126)
            .map_err(|r| WsError::Alloc(r.why))?;
        out.try_extend_from_slice(&len16.to_be_bytes())
            .map_err(WsError::Alloc)?;
    } else {
        let len64 = u64::try_from(payload.len()).map_err(|_| WsError::InvalidPayloadLength)?;
        if (len64 & (1u64 << 63)) != 0 {
            return Err(WsError::InvalidPayloadLength);
        }
        out.try_push(mask_bit | 127)
            .map_err(|r| WsError::Alloc(r.why))?;
        out.try_extend_from_slice(&len64.to_be_bytes())
            .map_err(WsError::Alloc)?;
    }

    match mask {
        Some([k0, k1, k2, k3]) => {
            out.try_extend_from_slice(&[k0, k1, k2, k3])
                .map_err(WsError::Alloc)?;
            for (idx, &byte) in payload.iter().enumerate() {
                let k = match idx & 3 {
                    0 => k0,
                    1 => k1,
                    2 => k2,
                    _ => k3,
                };
                out.try_push(byte ^ k)
                    .map_err(|r| WsError::Alloc(r.why))?;
            }
        }
        None => {
            out.try_extend_from_slice(payload)
                .map_err(WsError::Alloc)?;
        }
    }

    Ok(())
}

/// Encode a `Close` frame payload (`code` + optional UTF-8 `reason`) and append the framed bytes.
pub fn encode_close_frame_into<A: Allocator>(
    code: Option<CloseCode>,
    reason: &str,
    mask: Option<[u8; 4]>,
    out: &mut Vec<u8, A>,
) -> Result<(), WsError> {
    let mut buf = [0u8; 125];
    let payload_len = match code {
        None => {
            if !reason.is_empty() {
                return Err(WsError::InvalidClosePayload);
            }
            0usize
        }
        Some(c) => {
            if !c.is_valid_wire() {
                return Err(WsError::InvalidCloseCode { code: c.0 });
            }
            let total = 2usize.saturating_add(reason.len());
            if total > 125 {
                return Err(WsError::ControlFrameTooLarge { len: total });
            }
            let [hi, lo] = c.0.to_be_bytes();
            if let Some(slot) = buf.get_mut(0) {
                *slot = hi;
            }
            if let Some(slot) = buf.get_mut(1) {
                *slot = lo;
            }
            if let Some(dst) = buf.get_mut(2..total) {
                dst.copy_from_slice(reason.as_bytes());
            }
            total
        }
    };
    let slice = buf.get(..payload_len).unwrap_or(&[]);
    encode_frame_into(true, Opcode::Close, slice, mask, out)
}

/// Incremental sans-IO RFC 6455 WebSocket frame and message decoder.
pub struct WsDecoder<A: Allocator + Copy> {
    limits: WsLimits,
    alloc: A,
    buf: Vec<u8, A>,
    read_pos: usize,
    frag_opcode: Option<Opcode>,
    frag_buf: Vec<u8, A>,
    frag_count: usize,
}

impl<A: Allocator + Copy> WsDecoder<A> {
    /// Create a new streaming decoder using `limits` and `alloc`.
    #[must_use]
    pub fn new_in(limits: WsLimits, alloc: A) -> Self {
        Self {
            limits,
            alloc,
            buf: Vec::new_in(alloc),
            read_pos: 0,
            frag_opcode: None,
            frag_buf: Vec::new_in(alloc),
            frag_count: 0,
        }
    }

    /// Append incoming wire bytes to the decoder's buffer.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), WsError> {
        if self.read_pos > 0 && self.read_pos == self.buf.len() {
            self.buf.clear();
            self.read_pos = 0;
        }
        self.buf
            .try_extend_from_slice(bytes)
            .map_err(WsError::Alloc)
    }

    /// Parse the next single wire frame if all its bytes have arrived.
    pub fn next_frame(&mut self) -> Result<Option<Frame<A>>, WsError> {
        self.next_frame_planted(false)
    }

    fn next_frame_planted(&mut self, plant_skip_unmask: bool) -> Result<Option<Frame<A>>, WsError> {
        let avail = self.buf.as_slice().get(self.read_pos..).unwrap_or(&[]);
        let (Some(&b0), Some(&b1)) = (avail.first(), avail.get(1)) else {
            return Ok(None);
        };

        let fin = (b0 & 0x80) != 0;
        let rsv = (b0 >> 4) & 0x07;
        if rsv != 0 {
            return Err(WsError::NonZeroRsv { rsv });
        }

        let opcode_raw = b0 & 0x0F;
        let opcode = Opcode::from_u8(opcode_raw)
            .ok_or(WsError::InvalidOpcode { opcode: opcode_raw })?;
        let masked = (b1 & 0x80) != 0;
        let len7 = b1 & 0x7F;

        let (header_base, payload_len_u64) = if len7 <= 125 {
            (2usize, u64::from(len7))
        } else if len7 == 126 {
            let Some(ext) = avail.get(2..4) else {
                return Ok(None);
            };
            let Ok(arr) = <[u8; 2]>::try_from(ext) else {
                return Ok(None);
            };
            (4usize, u64::from(u16::from_be_bytes(arr)))
        } else {
            let Some(ext) = avail.get(2..10) else {
                return Ok(None);
            };
            let Ok(arr) = <[u8; 8]>::try_from(ext) else {
                return Ok(None);
            };
            let len64 = u64::from_be_bytes(arr);
            if (len64 & (1u64 << 63)) != 0 {
                return Err(WsError::InvalidPayloadLength);
            }
            (10usize, len64)
        };

        if opcode.is_control() {
            if !fin {
                return Err(WsError::FragmentedControlFrame { opcode });
            }
            if payload_len_u64 > 125 {
                let len = usize::try_from(payload_len_u64).unwrap_or(usize::MAX);
                return Err(WsError::ControlFrameTooLarge { len });
            }
        }

        let limit_u64 = u64::try_from(self.limits.most_frame_bytes).unwrap_or(u64::MAX);
        if payload_len_u64 > limit_u64 {
            return Err(WsError::FrameTooLarge {
                len: payload_len_u64,
                limit: self.limits.most_frame_bytes,
            });
        }
        let payload_len = usize::try_from(payload_len_u64).map_err(|_| WsError::FrameTooLarge {
            len: payload_len_u64,
            limit: self.limits.most_frame_bytes,
        })?;

        let mask_len = if masked { 4usize } else { 0usize };
        let header_len = header_base + mask_len;
        let total_len = header_len.checked_add(payload_len).ok_or(WsError::FrameTooLarge {
            len: payload_len_u64,
            limit: self.limits.most_frame_bytes,
        })?;

        if avail.len() < total_len {
            return Ok(None);
        }

        let mask_key = if masked {
            let Some(slice) = avail.get(header_base..header_len) else {
                return Ok(None);
            };
            let Ok(key) = <[u8; 4]>::try_from(slice) else {
                return Ok(None);
            };
            Some(key)
        } else {
            None
        };

        let raw_payload = avail.get(header_len..total_len).unwrap_or(&[]);
        let mut payload =
            Vec::try_with_capacity_in(payload_len, self.alloc).map_err(WsError::Alloc)?;
        match mask_key {
            Some([k0, k1, k2, k3]) if !plant_skip_unmask => {
                for (idx, &byte) in raw_payload.iter().enumerate() {
                    let k = match idx & 3 {
                        0 => k0,
                        1 => k1,
                        2 => k2,
                        _ => k3,
                    };
                    payload
                        .try_push(byte ^ k)
                        .map_err(|r| WsError::Alloc(r.why))?;
                }
            }
            _ => {
                payload
                    .try_extend_from_slice(raw_payload)
                    .map_err(WsError::Alloc)?;
            }
        }

        self.read_pos += total_len;
        if self.read_pos == self.buf.len() {
            self.buf.clear();
            self.read_pos = 0;
        } else if self.read_pos >= 4096 {
            let cut = self.read_pos;
            self.buf.drain(..cut);
            self.read_pos = 0;
        }

        Ok(Some(Frame { fin, opcode, payload }))
    }

    /// Pull the next complete message, reassembling fragments and delivering interleaved control
    /// frames immediately per RFC 6455 §5.4.
    pub fn next_message(&mut self) -> Result<Option<Message<A>>, WsError> {
        self.next_message_planted(false)
    }

    fn next_message_planted(
        &mut self,
        plant_skip_unmask: bool,
    ) -> Result<Option<Message<A>>, WsError> {
        loop {
            let Some(frame) = self.next_frame_planted(plant_skip_unmask)? else {
                return Ok(None);
            };

            match frame.opcode {
                Opcode::Ping => return Ok(Some(Message::Ping(frame.payload))),
                Opcode::Pong => return Ok(Some(Message::Pong(frame.payload))),
                Opcode::Close => {
                    let msg = decode_close_payload(frame.payload.as_slice(), self.alloc)?;
                    return Ok(Some(msg));
                }
                Opcode::Text | Opcode::Binary => {
                    if self.frag_opcode.is_some() {
                        return Err(WsError::ExpectedContinuation { opcode: frame.opcode });
                    }
                    if frame.payload.len() > self.limits.most_message_bytes {
                        return Err(WsError::MessageTooLarge {
                            len: frame.payload.len(),
                            limit: self.limits.most_message_bytes,
                        });
                    }
                    if frame.fin {
                        return match frame.opcode {
                            Opcode::Text => {
                                let text = String::from_utf8(frame.payload)
                                    .map_err(|_| WsError::InvalidUtf8)?;
                                Ok(Some(Message::Text(text)))
                            }
                            Opcode::Binary => Ok(Some(Message::Binary(frame.payload))),
                            Opcode::Continuation | Opcode::Close | Opcode::Ping | Opcode::Pong => {
                                Ok(None)
                            }
                        };
                    }
                    if self.limits.most_fragments == 0 {
                        return Err(WsError::TooManyFragments { limit: 0 });
                    }
                    self.frag_opcode = Some(frame.opcode);
                    self.frag_buf = frame.payload;
                    self.frag_count = 1;
                }
                Opcode::Continuation => {
                    let Some(open_op) = self.frag_opcode else {
                        return Err(WsError::UnexpectedContinuation);
                    };
                    self.frag_count += 1;
                    if self.frag_count > self.limits.most_fragments {
                        return Err(WsError::TooManyFragments {
                            limit: self.limits.most_fragments,
                        });
                    }
                    let new_len = self
                        .frag_buf
                        .len()
                        .checked_add(frame.payload.len())
                        .ok_or(WsError::MessageTooLarge {
                            len: usize::MAX,
                            limit: self.limits.most_message_bytes,
                        })?;
                    if new_len > self.limits.most_message_bytes {
                        return Err(WsError::MessageTooLarge {
                            len: new_len,
                            limit: self.limits.most_message_bytes,
                        });
                    }
                    self.frag_buf
                        .try_extend_from_slice(frame.payload.as_slice())
                        .map_err(WsError::Alloc)?;

                    if frame.fin {
                        self.frag_opcode = None;
                        self.frag_count = 0;
                        let data =
                            core::mem::replace(&mut self.frag_buf, Vec::new_in(self.alloc));
                        return match open_op {
                            Opcode::Text => {
                                let text =
                                    String::from_utf8(data).map_err(|_| WsError::InvalidUtf8)?;
                                Ok(Some(Message::Text(text)))
                            }
                            Opcode::Binary => Ok(Some(Message::Binary(data))),
                            Opcode::Continuation | Opcode::Close | Opcode::Ping | Opcode::Pong => {
                                Ok(None)
                            }
                        };
                    }
                }
            }
        }
    }
}

fn decode_close_payload<A: Allocator + Copy>(
    bytes: &[u8],
    alloc: A,
) -> Result<Message<A>, WsError> {
    if bytes.is_empty() {
        return Ok(Message::Close { code: None, reason: String::new_in(alloc) });
    }
    if bytes.len() == 1 {
        return Err(WsError::InvalidClosePayload);
    }
    let (Some(&hi), Some(&lo)) = (bytes.first(), bytes.get(1)) else {
        return Err(WsError::InvalidClosePayload);
    };
    let code = CloseCode(u16::from_be_bytes([hi, lo]));
    if !code.is_valid_wire() {
        return Err(WsError::InvalidCloseCode { code: code.0 });
    }
    let reason_bytes = bytes.get(2..).unwrap_or(&[]);
    let reason_str = core::str::from_utf8(reason_bytes).map_err(|_| WsError::InvalidUtf8)?;
    let reason = String::try_from_str_in(reason_str, alloc).map_err(WsError::Alloc)?;
    Ok(Message::Close { code: Some(code), reason })
}

/// Compute the 28-byte ASCII `Sec-WebSocket-Accept` header value per RFC 6455 §1.3:
/// `Base64(SHA-1(client_key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`.
#[must_use]
pub fn accept_key(client_key: &str) -> [u8; 28] {
    let mut sha1 = Sha1::new();
    sha1.update(client_key.as_bytes());
    sha1.update(WS_ACCEPT_GUID);
    let digest = sha1.finalize();
    let mut out = [0u8; 28];
    let Ok(_) = blaze_base64::STANDARD.encode_into(&digest, &mut out) else {
        return out;
    };
    out
}

/// Pure-core FIPS 180-4 SHA-1 used solely for the RFC 6455 §1.3 WebSocket handshake.
struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    block_len: usize,
    total_bytes: u64,
}

impl Sha1 {
    const fn new() -> Self {
        Self {
            state: [
                0x6745_2301,
                0xEFCD_AB89,
                0x98BA_DCFE,
                0x1032_5476,
                0xC3D2_E1F0,
            ],
            block: [0; 64],
            block_len: 0,
            total_bytes: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        let len_u64 = u64::try_from(data.len()).unwrap_or(u64::MAX);
        self.total_bytes = self.total_bytes.wrapping_add(len_u64);
        for &b in data {
            if let Some(slot) = self.block.get_mut(self.block_len) {
                *slot = b;
            }
            self.block_len += 1;
            if self.block_len == 64 {
                self.compress();
                self.block_len = 0;
            }
        }
    }

    fn compress(&mut self) {
        let mut w = [0u32; 80];
        for (i, chunk) in self.block.chunks_exact(4).enumerate() {
            if let (Ok(arr), Some(slot)) = (<[u8; 4]>::try_from(chunk), w.get_mut(i)) {
                *slot = u32::from_be_bytes(arr);
            }
        }
        let mut i = 16usize;
        while i < 80 {
            let v = w.get(i - 3).copied().unwrap_or(0)
                ^ w.get(i - 8).copied().unwrap_or(0)
                ^ w.get(i - 14).copied().unwrap_or(0)
                ^ w.get(i - 16).copied().unwrap_or(0);
            if let Some(slot) = w.get_mut(i) {
                *slot = v.rotate_left(1);
            }
            i += 1;
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (round, &wi) in w.iter().enumerate() {
            let (f, k) = if round < 20 {
                ((b & c) | ((!b) & d), 0x5A82_7999u32)
            } else if round < 40 {
                (b ^ c ^ d, 0x6ED9_EBA1u32)
            } else if round < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32)
            } else {
                (b ^ c ^ d, 0xCA62_C1D6u32)
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        let [h0, h1, h2, h3, h4] = self.state;
        self.state = [
            h0.wrapping_add(a),
            h1.wrapping_add(b),
            h2.wrapping_add(c),
            h3.wrapping_add(d),
            h4.wrapping_add(e),
        ];
    }

    fn finalize(mut self) -> [u8; 20] {
        let total_bits = self.total_bytes.wrapping_mul(8);

        if let Some(slot) = self.block.get_mut(self.block_len) {
            *slot = 0x80;
        }
        self.block_len += 1;

        if self.block_len > 56 {
            if let Some(tail) = self.block.get_mut(self.block_len..64) {
                tail.fill(0);
            }
            self.compress();
            self.block_len = 0;
        }
        if let Some(pad) = self.block.get_mut(self.block_len..56) {
            pad.fill(0);
        }
        if let Some(len_slot) = self.block.get_mut(56..64) {
            len_slot.copy_from_slice(&total_bits.to_be_bytes());
        }
        self.compress();

        let mut digest = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            if let Some(dst) = digest.get_mut(i * 4..i * 4 + 4) {
                dst.copy_from_slice(&word.to_be_bytes());
            }
        }
        digest
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::{
        CloseCode, Message, Opcode, WsDecoder, WsError, WsLimits, accept_key,
        encode_close_frame_into, encode_frame_into,
    };

    #[test]
    fn rfc6455_accept_key_vector() {
        let got = accept_key("dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(&got, b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn fragmented_message_with_interleaved_ping_and_close() {
        let h = heap();
        let alloc = h.alloc();
        let mut wire = OwnVec::new_in(alloc);

        encode_frame_into(false, Opcode::Text, b"Hel", Some([0x37, 0xFA, 0x21, 0x3D]), &mut wire)
            .expect("frag 1");
        encode_frame_into(true, Opcode::Ping, b"ping!", None, &mut wire).expect("ping");
        encode_frame_into(true, Opcode::Continuation, b"lo, world!", Some([1, 2, 3, 4]), &mut wire)
            .expect("frag 2");
        encode_close_frame_into(Some(CloseCode::NORMAL), "bye", None, &mut wire).expect("close");

        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        // Feed 2 bytes at a time to test streaming reassembly.
        let mut msgs = OwnVec::new_in(alloc);
        for chunk in wire.as_slice().chunks(2) {
            dec.feed(chunk).expect("feed");
            while let Some(msg) = dec.next_message().expect("next_message") {
                msgs.push(msg);
            }
        }

        assert_eq!(msgs.len(), 3);
        let mut ping_expected = OwnVec::new_in(alloc);
        ping_expected.extend_from_slice(b"ping!");
        assert_eq!(msgs[0], Message::Ping(ping_expected));
        assert_eq!(msgs[1], Message::Text(OwnString::from_str_in("Hello, world!", alloc)));
        assert_eq!(
            msgs[2],
            Message::Close {
                code: Some(CloseCode::NORMAL),
                reason: OwnString::from_str_in("bye", alloc),
            }
        );
    }

    #[test]
    fn protocol_violations_refused() {
        let h = heap();
        let alloc = h.alloc();

        // Non-zero RSV1 bit.
        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        dec.feed(&[0xC1, 0x00]).expect("feed");
        assert_eq!(dec.next_message(), Err(WsError::NonZeroRsv { rsv: 0b100 }));

        // Fragmented control frame (Ping with FIN=0).
        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        dec.feed(&[0x09, 0x00]).expect("feed");
        assert_eq!(
            dec.next_message(),
            Err(WsError::FragmentedControlFrame { opcode: Opcode::Ping })
        );

        // Control frame > 125 bytes (len7 = 126, extended 126).
        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        dec.feed(&[0x89, 126, 0x00, 126]).expect("feed");
        assert_eq!(dec.next_message(), Err(WsError::ControlFrameTooLarge { len: 126 }));

        // Invalid UTF-8 in Text frame.
        let mut wire = OwnVec::new_in(alloc);
        encode_frame_into(true, Opcode::Text, &[0xFF, 0xFE], None, &mut wire).expect("enc");
        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        dec.feed(wire.as_slice()).expect("feed");
        assert_eq!(dec.next_message(), Err(WsError::InvalidUtf8));

        // 16-bit extended length payload (> 125 bytes).
        let mut big = OwnVec::new_in(alloc);
        big.resize(300, b'x');
        let mut wire = OwnVec::new_in(alloc);
        encode_frame_into(true, Opcode::Binary, big.as_slice(), Some([9, 8, 7, 6]), &mut wire)
            .expect("enc big");
        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        dec.feed(wire.as_slice()).expect("feed");
        assert_eq!(dec.next_message().expect("ok"), Some(Message::Binary(big)));
    }

    fn draw_messages<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        alloc: A,
    ) -> OwnVec<Message<A>, A> {
        const TEXTS: &[&str] = &["", "hi", "hello world", "café 🚀", "longer payload repeated repeated repeated repeated"];
        let count = tape.int_in(1usize..=6, "msgs");
        let mut msgs = OwnVec::new_in(alloc);
        for _ in 0..count {
            let kind = tape.int_in(0u8..=4, "kind");
            let msg = match kind {
                0 => {
                    let s = TEXTS[tape.index(TEXTS.len(), "text")];
                    Message::Text(OwnString::from_str_in(s, alloc))
                }
                1 => {
                    let len = if tape.coin("extended_len") {
                        tape.int_in(126usize..=300, "bin_len")
                    } else {
                        tape.int_in(0usize..=40, "bin_len")
                    };
                    let mut buf = OwnVec::with_capacity_in(len, alloc);
                    for _ in 0..len {
                        buf.push(tape.int_in(0u8..=255, "byte"));
                    }
                    Message::Binary(buf)
                }
                2 => {
                    let len = tape.int_in(0usize..=16, "ping_len");
                    let mut buf = OwnVec::with_capacity_in(len, alloc);
                    for _ in 0..len {
                        buf.push(tape.int_in(0u8..=255, "byte"));
                    }
                    Message::Ping(buf)
                }
                3 => {
                    let len = tape.int_in(0usize..=16, "pong_len");
                    let mut buf = OwnVec::with_capacity_in(len, alloc);
                    for _ in 0..len {
                        buf.push(tape.int_in(0u8..=255, "byte"));
                    }
                    Message::Pong(buf)
                }
                _ => {
                    if tape.coin("close_with_code") {
                        Message::Close {
                            code: Some(CloseCode::NORMAL),
                            reason: OwnString::from_str_in("done", alloc),
                        }
                    } else {
                        Message::Close { code: None, reason: OwnString::new_in(alloc) }
                    }
                }
            };
            msgs.push(msg);
        }
        msgs
    }

    fn check_ws_round_trip<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        messages: &[Message<A>],
        plant_skip_unmask: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut wire = OwnVec::new_in(alloc);
        for msg in messages {
            let mask = if tape.coin("masked") {
                Some([
                    tape.int_in(1u8..=255, "m0"),
                    tape.int_in(0u8..=255, "m1"),
                    tape.int_in(0u8..=255, "m2"),
                    tape.int_in(0u8..=255, "m3"),
                ])
            } else {
                None
            };
            match msg {
                Message::Text(s) => {
                    let bytes = s.as_bytes();
                    if bytes.len() >= 2 && tape.coin("fragment") {
                        let split = tape.int_in(1usize..=(bytes.len() - 1), "split");
                        encode_frame_into(false, Opcode::Text, &bytes[..split], mask, &mut wire)
                            .expect("enc f1");
                        encode_frame_into(true, Opcode::Continuation, &bytes[split..], mask, &mut wire)
                            .expect("enc f2");
                    } else {
                        encode_frame_into(true, Opcode::Text, bytes, mask, &mut wire)
                            .expect("enc text");
                    }
                }
                Message::Binary(b) => {
                    let bytes = b.as_slice();
                    if bytes.len() >= 2 && tape.coin("fragment") {
                        let split = tape.int_in(1usize..=(bytes.len() - 1), "split");
                        encode_frame_into(false, Opcode::Binary, &bytes[..split], mask, &mut wire)
                            .expect("enc b1");
                        encode_frame_into(true, Opcode::Continuation, &bytes[split..], mask, &mut wire)
                            .expect("enc b2");
                    } else {
                        encode_frame_into(true, Opcode::Binary, bytes, mask, &mut wire)
                            .expect("enc bin");
                    }
                }
                Message::Ping(p) => {
                    encode_frame_into(true, Opcode::Ping, p.as_slice(), mask, &mut wire)
                        .expect("enc ping");
                }
                Message::Pong(p) => {
                    encode_frame_into(true, Opcode::Pong, p.as_slice(), mask, &mut wire)
                        .expect("enc pong");
                }
                Message::Close { code, reason } => {
                    encode_close_frame_into(*code, reason.as_str(), mask, &mut wire)
                        .expect("enc close");
                }
            }
        }

        let mut dec = WsDecoder::new_in(WsLimits::SHIPPED, alloc);
        let mut decoded = OwnVec::new_in(alloc);
        let raw = wire.as_slice();
        let mut pos = 0usize;
        while pos < raw.len() {
            let max_step = (raw.len() - pos).min(19);
            let step = tape.int_in(1usize..=max_step, "chunk");
            let end = (pos + step).min(raw.len());
            dec.feed(&raw[pos..end])
                .map_err(|e| Broke::Oracle(format_in!(alloc, "feed failed: {e}").as_str().into()))?;
            while let Some(msg) = dec
                .next_message_planted(plant_skip_unmask)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "decode failed: {e}").as_str().into()))?
            {
                decoded.push(msg);
            }
            pos = end;
        }

        if decoded.len() != messages.len() {
            return Err(Broke::Oracle(
                format_in!(alloc, "count mismatch: expected {}, got {}", messages.len(), decoded.len())
                    .as_str()
                    .into(),
            ));
        }
        for (i, (got, exp)) in decoded.iter().zip(messages.iter()).enumerate() {
            if got != exp {
                return Err(Broke::Oracle(
                    format_in!(alloc, "msg {i} mismatch: expected {exp:?}, got {got:?}")
                        .as_str()
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// The invariant: chunked WebSocket frame encode/decode round-trips masked and unmasked frames accurately.
    #[test]
    fn property_ws_chunked_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let msgs = draw_messages(tape, h.alloc());
            check_ws_round_trip(tape, msgs.as_slice(), false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_unmasked_decode_bug() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let msgs = draw_messages(tape, h.alloc());
            check_ws_round_trip(tape, msgs.as_slice(), plant, h.alloc())
        });
    }
}
