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

/// The bytes of an `int32` field: a message's length, a value's length, a word of an array's
/// header.
pub(crate) const INT32_BYTES: usize = size_of::<i32>();

/// The bytes base64 encodes as one group: 24 bits.
const GROUP_BYTES: usize = 3;

/// The characters of one base64 group: its 24 bits as four of [`CHAR_BITS`].
const GROUP_CHARS: usize = 4;

/// The bits of a group, [`GROUP_BYTES`] bytes of 8.
const GROUP_BITS: u32 = 24;

/// The bits one base64 character carries: its alphabet has 64.
const CHAR_BITS: u32 = 6;

/// The mask of one character's bits.
const CHAR_MASK: u32 = (1 << CHAR_BITS) - 1;

/// The bytes of SCRAM's raw client nonce, libpq's `SCRAM_RAW_NONCE_LEN`: a whole number of
/// base64 groups, so its encoding has no padding.
pub(crate) const NONCE_BYTES: usize = 18;

/// The characters of the client nonce as base64.
pub(crate) const NONCE_CHARS: usize = NONCE_BYTES / GROUP_BYTES * GROUP_CHARS;

/// The base 10 that counts are written and read in.
pub(crate) const DECIMAL: u64 = 10;

/// `ReadyForQuery`'s name, written once: a variant of the connection's stages has it too.
const READY_FOR_QUERY: &str = "ReadyForQuery";

/// `ParseComplete`'s name, written once: a variant of the connection's stages has it too.
const PARSE_COMPLETE: &str = "ParseComplete";

/// `ParameterDescription`'s name, written once: a variant of the connection's stages has it too.
const PARAMETER_DESCRIPTION: &str = "ParameterDescription";

/// `RowDescription`'s name, written once: a variant of the connection's stages has it too.
const ROW_DESCRIPTION: &str = "RowDescription";

/// `BindComplete`'s name, written once: a variant of the connection's stages has it too.
const BIND_COMPLETE: &str = "BindComplete";

/// `CloseComplete`'s name, written once: a variant of the connection's stages has it too.
const CLOSE_COMPLETE: &str = "CloseComplete";

/// The name of a backend message type, for errors.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn backend_name(tag: u8) -> &'static str {
    match tag {
        b'R' => "Authentication",
        b'S' => "ParameterStatus",
        b'K' => "BackendKeyData",
        b'Z' => READY_FOR_QUERY,
        b'N' => "NoticeResponse",
        b'E' => "ErrorResponse",
        b'1' => PARSE_COMPLETE,
        b't' => PARAMETER_DESCRIPTION,
        b'T' => ROW_DESCRIPTION,
        b'n' => "NoData",
        b'2' => BIND_COMPLETE,
        b'3' => CLOSE_COMPLETE,
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
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(bytes: &'b [u8]) -> Reader<'b> {
        Reader { bytes, at: 0 }
    }

    /// How many bytes of the body are read: where the next field starts.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn at(&self) -> usize {
        self.at
    }

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
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

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn u8(&mut self, field: &'static str) -> Result<u8, What> {
        let [byte] = self.array::<1>(field)?;
        Ok(byte)
    }

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn u16(&mut self, field: &'static str) -> Result<u16, What> {
        Ok(u16::from_be_bytes(self.array(field)?))
    }

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn i32(&mut self, field: &'static str) -> Result<i32, What> {
        Ok(i32::from_be_bytes(self.array(field)?))
    }

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn u32(&mut self, field: &'static str) -> Result<u32, What> {
        Ok(u32::from_be_bytes(self.array(field)?))
    }

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn array<const N: usize>(&mut self, field: &'static str) -> Result<[u8; N], What> {
        let bytes = self.take(N, field)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// A NUL-terminated string, without its NUL.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn cstr(&mut self, field: &'static str) -> Result<&'b [u8], What> {
        let Some(end) = self.bytes.iter().position(|&b| b == 0) else {
            return Err(What::Unterminated { field });
        };
        let text = self.take(end, field)?;
        self.take(1, field)?;
        Ok(text)
    }

    /// The rest of the body.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn rest(&mut self) -> &'b [u8] {
        let rest = self.bytes;
        self.bytes = &[];
        self.at += rest.len();
        rest
    }

    /// The body is read to its end: bytes after the last field are a protocol error.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn end(&self) -> Result<(), What> {
        match self.bytes.len() {
            0 => Ok(()),
            bytes => Err(What::Trailing { bytes }),
        }
    }
}

/// Starts a message of type `tag` in `out`: the type and a length to patch. Returns where the
/// length is, for [`finish`].
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn begin<A: Allocator>(out: &mut Vec<u8, A>, tag: u8) -> usize {
    out.push(tag);
    let at = out.len();
    out.extend_from_slice(&[0; INT32_BYTES]);
    at
}

/// Patches the length of the message [`begin`] started at `at`. A message longer than
/// [`MESSAGE_MOST`] is refused with its length; the caller drops what it wrote.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn finish<A: Allocator>(out: &mut Vec<u8, A>, at: usize) -> Result<(), usize> {
    let len = out.len().saturating_sub(at);
    if len > MESSAGE_MOST {
        return Err(len);
    }
    // `len` is at most `MESSAGE_MOST`, below `u32::MAX`, so this refuses nothing more.
    let Ok(field) = u32::try_from(len) else {
        return Err(len);
    };
    if let Some(slot) = out.get_mut(at..at.saturating_add(INT32_BYTES)) {
        slot.copy_from_slice(&field.to_be_bytes());
    }
    Ok(())
}

#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn put_u16<A: Allocator>(out: &mut Vec<u8, A>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn put_u32<A: Allocator>(out: &mut Vec<u8, A>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn put_i32<A: Allocator>(out: &mut Vec<u8, A>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// `text` and its NUL. The caller has checked that `text` holds no NUL.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn put_cstr<A: Allocator>(out: &mut Vec<u8, A>, text: &[u8]) {
    out.extend_from_slice(text);
    out.push(0);
}

/// Standard base64 with padding (RFC 4648), the encoding SCRAM's attributes use. Decoding a
/// server's attribute goes through the `base64` crate, which refuses what is not canonical;
/// encoding our own bytes cannot fail, so it is written here without a `Result`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn put_base64<A: Allocator>(out: &mut Vec<u8, A>, bytes: &[u8]) {
    for group in bytes.chunks(GROUP_BYTES) {
        out.extend_from_slice(&base64_group(group));
    }
}

/// The client nonce's [`NONCE_BYTES`] as their [`NONCE_CHARS`] base64 characters (no padding:
/// they are a whole number of groups).
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn base64_18(bytes: &[u8; NONCE_BYTES]) -> [u8; NONCE_CHARS] {
    let mut out = [0u8; NONCE_CHARS];
    for (group, chars) in bytes.chunks(GROUP_BYTES).zip(out.chunks_mut(GROUP_CHARS)) {
        chars.copy_from_slice(&base64_group(group));
    }
    out
}

/// One group of up to [`GROUP_BYTES`] bytes as [`GROUP_CHARS`] characters, padded with `=`:
/// a group of `n` bytes is `n + 1` characters, then padding.
fn base64_group(group: &[u8]) -> [u8; GROUP_CHARS] {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let char_of = |six: u32| ALPHABET[blaze_cast::usize_from_u32(six & CHAR_MASK)];
    // The group's bytes, big-endian, in the low `GROUP_BITS` of a word; a missing byte is 0.
    let word = (0..GROUP_BYTES)
        .fold(0u32, |word, at| (word << u8::BITS) | group.get(at).copied().map_or(0, u32::from));
    let mut chars = [b'='; GROUP_CHARS];
    let written = group.len().max(1).saturating_add(1);
    let mut shift = GROUP_BITS;
    for slot in chars.iter_mut().take(written) {
        shift = shift.saturating_sub(CHAR_BITS);
        *slot = char_of(word >> shift);
    }
    chars
}
