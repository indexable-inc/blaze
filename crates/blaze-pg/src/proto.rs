//! The bytes of the protocol: the backend's message names, a reader of a message's body that
//! names the field it ran out at, and the writers of the frontend's messages.
//!
//! Every message but the startup one is a type byte, a big-endian `int32` length that counts
//! itself and the body but not the type byte, and the body. Strings are NUL-terminated.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::error::What;

/// The most a message's length field may say, counting itself: 1 GiB minus 1, as Postgres caps
/// the messages it reads (`PQ_LARGE_MESSAGE_LIMIT`). A larger one is a protocol error.
pub const MESSAGE_MOST: usize = (1 << 30) - 1;

/// The most a message may say before the connection is ready (authentication, parameters, the
/// startup message itself): Postgres's `MAX_STARTUP_PACKET_LENGTH`.
pub const STARTUP_MESSAGE_MOST: usize = 10_000;

/// Protocol 3.0, as the startup message names it.
pub(crate) const PROTOCOL_3_0: u32 = 3 << 16;

/// The name of a backend message type, for errors.
pub fn backend_name(tag: u8) -> &'static str {
    match tag {
        b'R' => "Authentication",
        b'S' => "ParameterStatus",
        b'K' => "BackendKeyData",
        b'Z' => "ReadyForQuery",
        b'N' => "NoticeResponse",
        b'E' => "ErrorResponse",
        b'1' => "ParseComplete",
        b't' => "ParameterDescription",
        b'T' => "RowDescription",
        b'n' => "NoData",
        b'2' => "BindComplete",
        b'3' => "CloseComplete",
        b'D' => "DataRow",
        b'C' => "CommandComplete",
        b'I' => "EmptyQueryResponse",
        b's' => "PortalSuspended",
        b'A' => "NotificationResponse",
        b'v' => "NegotiateProtocolVersion",
        b'G' => "CopyInResponse",
        b'H' => "CopyOutResponse",
        b'W' => "CopyBothResponse",
        b'd' => "CopyData",
        b'c' => "CopyDone",
        b'V' => "FunctionCallResponse",
        _ => "an unknown message",
    }
}

/// A cursor over a message's body. Each read names the field it is for, so a body that ends
/// early is refused naming that field.
pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    /// How many bytes of the body are read.
    at: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Reader<'b> {
        Reader { bytes, at: 0 }
    }

    /// How many bytes of the body are read: where the next field starts.
    pub(crate) fn at(&self) -> usize {
        self.at
    }

    pub(crate) fn take(&mut self, n: usize, field: &'static str) -> Result<&'b [u8], What> {
        match self.bytes.split_at_checked(n) {
            Some((head, rest)) => {
                self.bytes = rest;
                self.at += n;
                Ok(head)
            }
            None => Err(What::Truncated { field }),
        }
    }

    pub(crate) fn u8(&mut self, field: &'static str) -> Result<u8, What> {
        let [byte] = self.array::<1>(field)?;
        Ok(byte)
    }

    pub(crate) fn u16(&mut self, field: &'static str) -> Result<u16, What> {
        Ok(u16::from_be_bytes(self.array(field)?))
    }

    pub(crate) fn i32(&mut self, field: &'static str) -> Result<i32, What> {
        Ok(i32::from_be_bytes(self.array(field)?))
    }

    pub(crate) fn u32(&mut self, field: &'static str) -> Result<u32, What> {
        Ok(u32::from_be_bytes(self.array(field)?))
    }

    pub(crate) fn array<const N: usize>(&mut self, field: &'static str) -> Result<[u8; N], What> {
        let bytes = self.take(N, field)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// A NUL-terminated string, without its NUL.
    pub(crate) fn cstr(&mut self, field: &'static str) -> Result<&'b [u8], What> {
        let Some(end) = self.bytes.iter().position(|&b| b == 0) else {
            return Err(What::Unterminated { field });
        };
        let text = self.take(end, field)?;
        self.take(1, field)?;
        Ok(text)
    }

    /// The rest of the body.
    pub(crate) fn rest(&mut self) -> &'b [u8] {
        let rest = self.bytes;
        self.bytes = &[];
        self.at += rest.len();
        rest
    }

    /// The body is read to its end: bytes after the last field are a protocol error.
    pub(crate) fn end(&self) -> Result<(), What> {
        match self.bytes.len() {
            0 => Ok(()),
            bytes => Err(What::Trailing { bytes }),
        }
    }
}

/// Starts a message of type `tag` in `out`: the type and a length to patch. Returns where the
/// length is, for [`finish`].
pub(crate) fn begin<A: Allocator>(out: &mut Vec<u8, A>, tag: u8) -> usize {
    out.push(tag);
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    at
}

/// Patches the length of the message [`begin`] started at `at`. A message longer than
/// [`MESSAGE_MOST`] is refused with its length; the caller drops what it wrote.
pub(crate) fn finish<A: Allocator>(out: &mut Vec<u8, A>, at: usize) -> Result<(), usize> {
    let len = out.len().saturating_sub(at);
    if len > MESSAGE_MOST {
        return Err(len);
    }
    // `len` is at most `MESSAGE_MOST`, below `u32::MAX`.
    let field = (len as u32).to_be_bytes();
    if let Some(slot) = out.get_mut(at..at.saturating_add(4)) {
        slot.copy_from_slice(&field);
    }
    Ok(())
}

pub(crate) fn put_u16<A: Allocator>(out: &mut Vec<u8, A>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub(crate) fn put_u32<A: Allocator>(out: &mut Vec<u8, A>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub(crate) fn put_i32<A: Allocator>(out: &mut Vec<u8, A>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// `text` and its NUL. The caller has checked that `text` holds no NUL.
pub(crate) fn put_cstr<A: Allocator>(out: &mut Vec<u8, A>, text: &[u8]) {
    out.extend_from_slice(text);
    out.push(0);
}

/// Standard base64 with padding (RFC 4648), the encoding SCRAM's attributes use. Decoding a
/// server's attribute goes through the `base64` crate, which refuses what is not canonical;
/// encoding our own bytes cannot fail, so it is written here without a `Result`.
pub(crate) fn put_base64<A: Allocator>(out: &mut Vec<u8, A>, bytes: &[u8]) {
    for group in bytes.chunks(3) {
        out.extend_from_slice(&base64_group(group));
    }
}

/// 18 bytes as their 24 base64 characters (no padding: 18 is a whole number of groups).
pub(crate) fn base64_18(bytes: &[u8; 18]) -> [u8; 24] {
    let mut out = [0u8; 24];
    for (group, chars) in bytes.chunks(3).zip(out.chunks_mut(4)) {
        chars.copy_from_slice(&base64_group(group));
    }
    out
}

/// One group of up to 3 bytes as 4 characters, padded with `=`.
fn base64_group(group: &[u8]) -> [u8; 4] {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let char_of = |six: u32| ALPHABET[(six & 63) as usize];
    let byte = |at: usize| group.get(at).copied().map(u32::from);
    let word = (byte(0).unwrap_or(0) << 16) | (byte(1).unwrap_or(0) << 8) | byte(2).unwrap_or(0);
    [
        char_of(word >> 18),
        char_of(word >> 12),
        if group.len() > 1 { char_of(word >> 6) } else { b'=' },
        if group.len() > 2 { char_of(word) } else { b'=' },
    ]
}
