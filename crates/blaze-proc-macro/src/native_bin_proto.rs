//! Sans-IO binary wire protocol between the Blaze proc-macro host ([`crate::native_bin`]) and a
//! confined Tier-2 native proc-macro executable (`native.bin`) over inherited `CHILD_FD` (fd 3).
//!
//! Framing matches `blaze_core::linked::channel` (`PREFIX = 4`, big-endian `u32` body length
//! followed by body bytes). Every frame body starts with a 1-byte message tag and uses
//! little-endian fixed-width integers and length-prefixed slices/strings, with strict frame-size
//! and trailing-byte checks so no truncated, oversized, or malformed message is ever trusted.

#![blaze::sans_io]

use core::fmt;

use blaze_expand::ProcKind;
use blaze_own::{Alloc, SharedAlloc, Vec as OwnVec};

/// Explicit-allocator byte sink for encoding framed binary IPC messages without naming `std` or
/// `alloc::alloc::Allocator`.
pub trait FrameSink {
    fn len(&self) -> usize;
    fn try_reserve_extra(&mut self, additional: usize) -> Result<(), ProtoError>;
    fn push_slice(&mut self, bytes: &[u8]);
    fn truncate_to(&mut self, len: usize);
}

impl FrameSink for OwnVec<u8, Alloc<'_>> {
    fn len(&self) -> usize {
        OwnVec::len(self)
    }

    fn try_reserve_extra(&mut self, additional: usize) -> Result<(), ProtoError> {
        self.try_reserve(additional).map_err(|_| ProtoError::OutOfMemory)
    }

    fn push_slice(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    fn truncate_to(&mut self, len: usize) {
        self.truncate(len);
    }
}

impl FrameSink for OwnVec<u8, SharedAlloc<'_>> {
    fn len(&self) -> usize {
        OwnVec::len(self)
    }

    fn try_reserve_extra(&mut self, additional: usize) -> Result<(), ProtoError> {
        self.try_reserve(additional).map_err(|_| ProtoError::OutOfMemory)
    }

    fn push_slice(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    fn truncate_to(&mut self, len: usize) {
        self.truncate(len);
    }
}

/// 4-byte protocol magic sent in `Hello` and `HelloAck`.
pub const MAGIC: [u8; 4] = *b"BZPM";

/// Wire protocol version.
pub const PROTO_VERSION: u32 = 1;

/// Length of the big-endian frame-length prefix (`blaze_core::linked::channel::PREFIX`).
pub const FRAME_PREFIX_LEN: usize = 4;

/// Maximum body length of a single binary IPC frame (64 MiB, rule 18).
pub const MAX_FRAME_BYTES: u32 = 64 << 20;

/// Maximum bridge `Dispatch` round-trips permitted in a single expansion (rule 18).
pub const MAX_DISPATCHES: u64 = 1 << 20;

const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_ACK: u8 = 0x02;
const TAG_EXPAND: u8 = 0x10;
const TAG_REPLY: u8 = 0x11;
const TAG_ENV_VAL: u8 = 0x12;
const TAG_CURRENT_DIR_VAL: u8 = 0x13;
const TAG_SHUTDOWN: u8 = 0x14;

const TAG_DISPATCH: u8 = 0x20;
const TAG_ENV_VAR: u8 = 0x21;
const TAG_CURRENT_DIR: u8 = 0x22;
const TAG_OUTPUT: u8 = 0x23;
const TAG_PANIC: u8 = 0x24;

const KIND_DERIVE: u8 = 0;
const KIND_ATTR: u8 = 1;
const KIND_FN_LIKE: u8 = 2;

/// Host -> Child protocol message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostMsg<'a> {
    Hello { magic: [u8; 4], version: u32 },
    Expand { index: u32, kind: ProcKind, input: &'a [u8] },
    Reply { bytes: &'a [u8] },
    EnvVal { value: Option<&'a str> },
    CurrentDirVal { dir: &'a str },
    Shutdown,
}

/// Child -> Host protocol message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildMsg<'a> {
    HelloAck { magic: [u8; 4], version: u32 },
    Dispatch { bytes: &'a [u8] },
    EnvVar { name: &'a str },
    CurrentDir,
    Output { bytes: &'a [u8] },
    Panic { message: &'a str },
}

/// Why a frame could not be encoded, split, or decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProtoError {
    EmptyFrame,
    FrameTooLarge { len: u32, max: u32 },
    Truncated(&'static str),
    TrailingBytes(usize),
    UnknownTag(u8),
    UnknownKind(u8),
    InvalidBool(u8),
    InvalidUtf8,
    BadMagic([u8; 4]),
    VersionMismatch { expected: u32, found: u32 },
    OutOfMemory,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ProtoError::EmptyFrame => f.write_str("empty native proc-macro IPC frame"),
            ProtoError::FrameTooLarge { len, max } => {
                write!(f, "native proc-macro IPC frame of {len} bytes exceeds limit of {max} bytes")
            }
            ProtoError::Truncated(what) => write!(f, "truncated native proc-macro IPC frame while reading {what}"),
            ProtoError::TrailingBytes(count) => {
                write!(f, "native proc-macro IPC frame has {count} unexpected trailing byte(s)")
            }
            ProtoError::UnknownTag(tag) => write!(f, "unknown native proc-macro IPC message tag 0x{tag:02x}"),
            ProtoError::UnknownKind(tag) => write!(f, "unknown proc-macro kind tag {tag}"),
            ProtoError::InvalidBool(tag) => write!(f, "invalid boolean tag {tag} in native proc-macro IPC frame"),
            ProtoError::InvalidUtf8 => f.write_str("invalid UTF-8 string in native proc-macro IPC frame"),
            ProtoError::BadMagic(found) => {
                write!(f, "bad native proc-macro IPC magic {found:?} (expected {MAGIC:?})")
            }
            ProtoError::VersionMismatch { expected, found } => {
                write!(f, "native proc-macro IPC version mismatch: expected {expected}, got {found}")
            }
            ProtoError::OutOfMemory => f.write_str("out of memory encoding native proc-macro IPC frame"),
        }
    }
}

impl core::error::Error for ProtoError {}

/// Validates a handshake's magic bytes and protocol version.
pub const fn validate_hello(magic: [u8; 4], version: u32) -> Result<(), ProtoError> {
    if magic[0] != MAGIC[0] || magic[1] != MAGIC[1] || magic[2] != MAGIC[2] || magic[3] != MAGIC[3] {
        return Err(ProtoError::BadMagic(magic));
    }
    if version != PROTO_VERSION {
        return Err(ProtoError::VersionMismatch { expected: PROTO_VERSION, found: version });
    }
    Ok(())
}

/// Checks whether `buf` begins with a complete framed message (`[u32 BE body_len][body]`),
/// enforcing `1 <= body_len <= max_frame`. Returns `Ok(None)` if more bytes are needed, or
/// `Ok(Some((body, total_frame_bytes)))` when a complete frame is present.
pub fn split_frame(buf: &[u8], max_frame: u32) -> Result<Option<(&[u8], usize)>, ProtoError> {
    let Some((len_bytes, rest)) = buf.split_at_checked(FRAME_PREFIX_LEN) else {
        return Ok(None);
    };
    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| ProtoError::Truncated("frame length"))?;
    let body_len = u32::from_be_bytes(len_arr);
    if body_len == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    if body_len > max_frame {
        return Err(ProtoError::FrameTooLarge { len: body_len, max: max_frame });
    }
    let body_usize = usize::try_from(body_len).map_err(|_| ProtoError::FrameTooLarge { len: body_len, max: max_frame })?;
    let Some((body, _)) = rest.split_at_checked(body_usize) else {
        return Ok(None);
    };
    let total = FRAME_PREFIX_LEN.checked_add(body_usize).ok_or(ProtoError::FrameTooLarge { len: body_len, max: max_frame })?;
    Ok(Some((body, total)))
}

fn kind_to_u8(kind: ProcKind) -> u8 {
    match kind {
        ProcKind::Derive => KIND_DERIVE,
        ProcKind::Attr => KIND_ATTR,
        ProcKind::FnLike => KIND_FN_LIKE,
    }
}

fn kind_from_u8(tag: u8) -> Result<ProcKind, ProtoError> {
    match tag {
        KIND_DERIVE => Ok(ProcKind::Derive),
        KIND_ATTR => Ok(ProcKind::Attr),
        KIND_FN_LIKE => Ok(ProcKind::FnLike),
        other => Err(ProtoError::UnknownKind(other)),
    }
}

fn body_len_checked(body_len: usize, max_frame: u32) -> Result<u32, ProtoError> {
    let len = u32::try_from(body_len).map_err(|_| ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
    if len == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    if len > max_frame {
        return Err(ProtoError::FrameTooLarge { len, max: max_frame });
    }
    Ok(len)
}

fn push_bytes<S: FrameSink>(out: &mut S, bytes: &[u8]) -> Result<(), ProtoError> {
    out.try_reserve_extra(bytes.len())?;
    out.push_slice(bytes);
    Ok(())
}

fn push_u32_le<S: FrameSink>(out: &mut S, value: u32) -> Result<(), ProtoError> {
    push_bytes(out, &value.to_le_bytes())
}

fn push_len_bytes<S: FrameSink>(out: &mut S, bytes: &[u8], max_frame: u32) -> Result<(), ProtoError> {
    let len = u32::try_from(bytes.len()).map_err(|_| ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
    push_u32_le(out, len)?;
    push_bytes(out, bytes)
}

fn write_framed<S: FrameSink>(
    out: &mut S,
    body_len: usize,
    max_frame: u32,
    write_body: impl FnOnce(&mut S) -> Result<(), ProtoError>,
) -> Result<(), ProtoError> {
    let checked_len = body_len_checked(body_len, max_frame)?;
    let total = FRAME_PREFIX_LEN.checked_add(body_len).ok_or(ProtoError::FrameTooLarge { len: checked_len, max: max_frame })?;
    out.try_reserve_extra(total)?;
    let start = out.len();
    push_bytes(out, &checked_len.to_be_bytes())?;
    let body_start = out.len();
    if let Err(err) = write_body(out) {
        out.truncate_to(start);
        return Err(err);
    }
    debug_assert_eq!(out.len().saturating_sub(body_start), body_len);
    Ok(())
}

/// Encodes `msg` as a length-prefixed binary frame appended to `out`, enforcing `max_frame`.
pub fn encode_host_frame<S: FrameSink>(
    msg: &HostMsg<'_>,
    out: &mut S,
    max_frame: u32,
) -> Result<(), ProtoError> {
    match *msg {
        HostMsg::Hello { magic, version } => write_framed(out, 1 + 4 + 4, max_frame, |buf| {
            push_bytes(buf, &[TAG_HELLO])?;
            push_bytes(buf, &magic)?;
            push_u32_le(buf, version)
        }),
        HostMsg::Expand { index, kind, input } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(1))
                .and_then(|n| n.checked_add(4))
                .and_then(|n| n.checked_add(input.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_EXPAND])?;
                push_u32_le(buf, index)?;
                push_bytes(buf, &[kind_to_u8(kind)])?;
                push_len_bytes(buf, input, max_frame)
            })
        }
        HostMsg::Reply { bytes } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(bytes.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_REPLY])?;
                push_len_bytes(buf, bytes, max_frame)
            })
        }
        HostMsg::EnvVal { value } => match value {
            None => write_framed(out, 2, max_frame, |buf| push_bytes(buf, &[TAG_ENV_VAL, 0])),
            Some(text) => {
                let body_len = 2usize
                    .checked_add(4)
                    .and_then(|n| n.checked_add(text.len()))
                    .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
                write_framed(out, body_len, max_frame, |buf| {
                    push_bytes(buf, &[TAG_ENV_VAL, 1])?;
                    push_len_bytes(buf, text.as_bytes(), max_frame)
                })
            }
        },
        HostMsg::CurrentDirVal { dir } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(dir.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_CURRENT_DIR_VAL])?;
                push_len_bytes(buf, dir.as_bytes(), max_frame)
            })
        }
        HostMsg::Shutdown => write_framed(out, 1, max_frame, |buf| push_bytes(buf, &[TAG_SHUTDOWN])),
    }
}

/// Encodes `msg` as a length-prefixed binary frame appended to `out`, enforcing `max_frame`.
pub fn encode_child_frame<S: FrameSink>(
    msg: &ChildMsg<'_>,
    out: &mut S,
    max_frame: u32,
) -> Result<(), ProtoError> {
    match *msg {
        ChildMsg::HelloAck { magic, version } => write_framed(out, 1 + 4 + 4, max_frame, |buf| {
            push_bytes(buf, &[TAG_HELLO_ACK])?;
            push_bytes(buf, &magic)?;
            push_u32_le(buf, version)
        }),
        ChildMsg::Dispatch { bytes } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(bytes.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_DISPATCH])?;
                push_len_bytes(buf, bytes, max_frame)
            })
        }
        ChildMsg::EnvVar { name } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(name.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_ENV_VAR])?;
                push_len_bytes(buf, name.as_bytes(), max_frame)
            })
        }
        ChildMsg::CurrentDir => write_framed(out, 1, max_frame, |buf| push_bytes(buf, &[TAG_CURRENT_DIR])),
        ChildMsg::Output { bytes } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(bytes.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_OUTPUT])?;
                push_len_bytes(buf, bytes, max_frame)
            })
        }
        ChildMsg::Panic { message } => {
            let body_len = 1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(message.len()))
                .ok_or(ProtoError::FrameTooLarge { len: u32::MAX, max: max_frame })?;
            write_framed(out, body_len, max_frame, |buf| {
                push_bytes(buf, &[TAG_PANIC])?;
                push_len_bytes(buf, message.as_bytes(), max_frame)
            })
        }
    }
}

struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], ProtoError> {
        let Some((head, tail)) = self.rest.split_at_checked(n) else {
            return Err(ProtoError::Truncated(what));
        };
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, ProtoError> {
        let bytes = self.take(1, what)?;
        bytes.first().copied().ok_or(ProtoError::Truncated(what))
    }

    fn u32_le(&mut self, what: &'static str) -> Result<u32, ProtoError> {
        let bytes = self.take(4, what)?;
        let arr: [u8; 4] = bytes.try_into().map_err(|_| ProtoError::Truncated(what))?;
        Ok(u32::from_le_bytes(arr))
    }

    fn fixed4(&mut self, what: &'static str) -> Result<[u8; 4], ProtoError> {
        let bytes = self.take(4, what)?;
        bytes.try_into().map_err(|_| ProtoError::Truncated(what))
    }

    fn bytes(&mut self, what: &'static str) -> Result<&'a [u8], ProtoError> {
        let len = self.u32_le(what)?;
        let len_usize = usize::try_from(len).map_err(|_| ProtoError::Truncated(what))?;
        self.take(len_usize, what)
    }

    fn str(&mut self, what: &'static str) -> Result<&'a str, ProtoError> {
        let bytes = self.bytes(what)?;
        core::str::from_utf8(bytes).map_err(|_| ProtoError::InvalidUtf8)
    }

    fn finish(self) -> Result<(), ProtoError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(ProtoError::TrailingBytes(self.rest.len()))
        }
    }
}

/// Decodes a single Host -> Child frame body (without the 4-byte length prefix).
pub fn decode_host_body(body: &[u8]) -> Result<HostMsg<'_>, ProtoError> {
    if body.is_empty() {
        return Err(ProtoError::EmptyFrame);
    }
    let mut c = Cursor::new(body);
    let tag = c.u8("message tag")?;
    let msg = match tag {
        TAG_HELLO => {
            let magic = c.fixed4("hello magic")?;
            let version = c.u32_le("hello version")?;
            HostMsg::Hello { magic, version }
        }
        TAG_EXPAND => {
            let index = c.u32_le("macro index")?;
            let kind = kind_from_u8(c.u8("macro kind")?)?;
            let input = c.bytes("expand input")?;
            HostMsg::Expand { index, kind, input }
        }
        TAG_REPLY => {
            let bytes = c.bytes("reply payload")?;
            HostMsg::Reply { bytes }
        }
        TAG_ENV_VAL => match c.u8("env present flag")? {
            0 => HostMsg::EnvVal { value: None },
            1 => HostMsg::EnvVal { value: Some(c.str("env value")?) },
            other => return Err(ProtoError::InvalidBool(other)),
        },
        TAG_CURRENT_DIR_VAL => {
            let dir = c.str("current dir")?;
            HostMsg::CurrentDirVal { dir }
        }
        TAG_SHUTDOWN => HostMsg::Shutdown,
        other => return Err(ProtoError::UnknownTag(other)),
    };
    c.finish()?;
    Ok(msg)
}

/// Decodes a single Child -> Host frame body (without the 4-byte length prefix).
pub fn decode_child_body(body: &[u8]) -> Result<ChildMsg<'_>, ProtoError> {
    if body.is_empty() {
        return Err(ProtoError::EmptyFrame);
    }
    let mut c = Cursor::new(body);
    let tag = c.u8("message tag")?;
    let msg = match tag {
        TAG_HELLO_ACK => {
            let magic = c.fixed4("hello_ack magic")?;
            let version = c.u32_le("hello_ack version")?;
            ChildMsg::HelloAck { magic, version }
        }
        TAG_DISPATCH => {
            let bytes = c.bytes("dispatch request")?;
            ChildMsg::Dispatch { bytes }
        }
        TAG_ENV_VAR => {
            let name = c.str("env var name")?;
            ChildMsg::EnvVar { name }
        }
        TAG_CURRENT_DIR => ChildMsg::CurrentDir,
        TAG_OUTPUT => {
            let bytes = c.bytes("output payload")?;
            ChildMsg::Output { bytes }
        }
        TAG_PANIC => {
            let message = c.str("panic message")?;
            ChildMsg::Panic { message }
        }
        other => return Err(ProtoError::UnknownTag(other)),
    };
    c.finish()?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_messages_round_trip_and_reject_truncation_and_trailing() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let cases = [
            HostMsg::Hello { magic: MAGIC, version: PROTO_VERSION },
            HostMsg::Expand { index: 7, kind: ProcKind::Derive, input: b"abc\x00\xff" },
            HostMsg::Expand { index: 0, kind: ProcKind::Attr, input: b"" },
            HostMsg::Expand { index: 42, kind: ProcKind::FnLike, input: b"hello" },
            HostMsg::Reply { bytes: b"reply-bytes" },
            HostMsg::EnvVal { value: None },
            HostMsg::EnvVal { value: Some("/workspace/app") },
            HostMsg::CurrentDirVal { dir: "/workspace/app" },
            HostMsg::Shutdown,
        ];
        let mut buf = blaze_own::Vec::new_in(alloc);
        for case in cases {
            buf.clear();
            encode_host_frame(&case, &mut buf, MAX_FRAME_BYTES).expect("encode");
            // Partial prefixes before completion must yield Ok(None).
            for cut in 0..buf.len() {
                assert_eq!(split_frame(&buf[..cut], MAX_FRAME_BYTES), Ok(None));
            }
            let (body, total) = split_frame(&buf, MAX_FRAME_BYTES).expect("split").expect("whole");
            assert_eq!(total, buf.len());
            assert_eq!(decode_host_body(body), Ok(case));
            // Every truncated body must fail with EmptyFrame or Truncated.
            for cut in 0..body.len() {
                let err = decode_host_body(&body[..cut]).expect_err("truncated body must fail");
                assert!(matches!(err, ProtoError::EmptyFrame | ProtoError::Truncated(_)));
            }
            // Trailing byte must fail with TrailingBytes(1).
            let mut with_extra = blaze_own::Vec::new_in(alloc);
            with_extra.extend_from_slice(body);
            with_extra.push(0x99);
            assert_eq!(decode_host_body(&with_extra), Err(ProtoError::TrailingBytes(1)));
        }
    }

    #[test]
    fn child_messages_round_trip_and_reject_truncation_and_trailing() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let cases = [
            ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION },
            ChildMsg::Dispatch { bytes: b"\x01\x02\x03" },
            ChildMsg::EnvVar { name: "CARGO_MANIFEST_DIR" },
            ChildMsg::CurrentDir,
            ChildMsg::Output { bytes: b"out" },
            ChildMsg::Panic { message: "macro panicked cleanly" },
        ];
        let mut buf = blaze_own::Vec::new_in(alloc);
        for case in cases {
            buf.clear();
            encode_child_frame(&case, &mut buf, MAX_FRAME_BYTES).expect("encode");
            let (body, total) = split_frame(&buf, MAX_FRAME_BYTES).expect("split").expect("whole");
            assert_eq!(total, buf.len());
            assert_eq!(decode_child_body(body), Ok(case));
            for cut in 0..body.len() {
                let err = decode_child_body(&body[..cut]).expect_err("truncated body must fail");
                assert!(matches!(err, ProtoError::EmptyFrame | ProtoError::Truncated(_)));
            }
            let mut with_extra = blaze_own::Vec::new_in(alloc);
            with_extra.extend_from_slice(body);
            with_extra.push(0x00);
            assert_eq!(decode_child_body(&with_extra), Err(ProtoError::TrailingBytes(1)));
        }
    }

    #[test]
    fn rejects_oversized_empty_unknown_and_invalid_utf8_frames() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut buf = blaze_own::Vec::new_in(alloc);

        // Oversized on encode.
        let err = encode_host_frame(&HostMsg::Reply { bytes: &[1u8; 64] }, &mut buf, 32)
            .expect_err("oversized encode must fail");
        assert!(matches!(err, ProtoError::FrameTooLarge { len: 69, max: 32 }));
        assert!(buf.is_empty());

        // Oversized on split_frame.
        assert_eq!(split_frame(&100u32.to_be_bytes(), 32), Err(ProtoError::FrameTooLarge { len: 100, max: 32 }));
        // Zero-length frame on split_frame.
        assert_eq!(split_frame(&0u32.to_be_bytes(), 32), Err(ProtoError::EmptyFrame));
        // Unknown tags.
        assert_eq!(decode_host_body(&[0xfe]), Err(ProtoError::UnknownTag(0xfe)));
        assert_eq!(decode_child_body(&[0xfe]), Err(ProtoError::UnknownTag(0xfe)));
        // Unknown ProcKind.
        assert_eq!(
            decode_host_body(&[TAG_EXPAND, 0, 0, 0, 0, 99, 0, 0, 0, 0]),
            Err(ProtoError::UnknownKind(99))
        );
        // Invalid bool flag in EnvVal.
        assert_eq!(decode_host_body(&[TAG_ENV_VAL, 2]), Err(ProtoError::InvalidBool(2)));
        // Invalid UTF-8 in EnvVar.
        assert_eq!(
            decode_child_body(&[TAG_ENV_VAR, 2, 0, 0, 0, 0xff, 0xfe]),
            Err(ProtoError::InvalidUtf8)
        );
        // Handshake validation.
        assert_eq!(validate_hello(MAGIC, PROTO_VERSION), Ok(()));
        assert_eq!(validate_hello(*b"NOPE", PROTO_VERSION), Err(ProtoError::BadMagic(*b"NOPE")));
        assert_eq!(
            validate_hello(MAGIC, PROTO_VERSION + 1),
            Err(ProtoError::VersionMismatch { expected: PROTO_VERSION, found: PROTO_VERSION + 1 })
        );
    }

    #[test]
    fn split_frame_handles_back_to_back_frames() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut buf = blaze_own::Vec::new_in(alloc);
        encode_child_frame(&ChildMsg::CurrentDir, &mut buf, MAX_FRAME_BYTES).expect("f1");
        encode_child_frame(&ChildMsg::EnvVar { name: "FOO" }, &mut buf, MAX_FRAME_BYTES).expect("f2");

        let (b1, n1) = split_frame(&buf, MAX_FRAME_BYTES).expect("s1").expect("w1");
        assert_eq!(decode_child_body(b1), Ok(ChildMsg::CurrentDir));
        let (b2, n2) = split_frame(&buf[n1..], MAX_FRAME_BYTES).expect("s2").expect("w2");
        assert_eq!(decode_child_body(b2), Ok(ChildMsg::EnvVar { name: "FOO" }));
        assert_eq!(n1 + n2, buf.len());
    }
}
