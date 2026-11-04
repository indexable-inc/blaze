//! Frames of the channel between a run process and the daemon: a fixed 12-byte little-endian
//! header (kind `u16`, a zero `u16`, body length `u32`, sequence `u32`) and the body.
//!
//! Every read is bounds-checked and nothing here panics on any bytes: a short buffer, a body
//! longer than [`MOST_BODY`], nonzero reserved bits, trailing bytes and bad UTF-8 are each a
//! [`FrameError`]. Bodies are written with [`Writer`] and read with [`Reader`], the same
//! primitives for every user of the channel.

use std::fmt;
use std::ops::Range;

pub const HEADER: usize = 12;
/// The longest body a frame may have: what a stub miss answers with is far shorter.
pub const MOST_BODY: u32 = 64 << 20;

/// What a frame is, by number. Each user of the channel has its own range ([`kinds`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Kind(pub u16);

/// The kinds each crate on the channel owns.
pub mod kinds {
    use super::{Kind, Range};

    /// This crate's: the start hook's handshake ([`crate::handshake`]).
    pub const INPROC: Range<u16> = 0x0000..0x0100;
    /// `blaze-lazy`'s: stub misses and their answers.
    pub const LAZY: Range<u16> = 0x0100..0x0200;
    /// `blaze-premap`'s.
    pub const PREMAP: Range<u16> = 0x0200..0x0300;
    /// `blaze-live`'s: patches for a running program.
    pub const LIVE: Range<u16> = 0x0300..0x0400;

    /// The start hook's first frame, to the daemon: where its image is.
    pub const HELLO: Kind = Kind(0x01);
    /// The daemon's answer: the memory objects and the overlays.
    pub const GRANT: Kind = Kind(0x02);
    /// A start hook asking the daemon to map a whole object into it (Windows, where the daemon
    /// maps views into the run process).
    pub const MAP: Kind = Kind(0x03);
    pub const MAPPED: Kind = Kind(0x04);
    /// The start hook is about to return into the program's own start, on this thread.
    pub const STARTED: Kind = Kind(0x05);
    /// The start hook could not attach: why, as UTF-8. The process exits.
    pub const FAILED: Kind = Kind(0x06);
    /// The daemon could not do what a frame asked: why, as UTF-8.
    pub const REFUSED: Kind = Kind(0x07);
    /// A start hook asking the daemon to lay the grant's overlays over its image (Windows).
    pub const OVERLAY: Kind = Kind(0x08);
    pub const OVERLAID: Kind = Kind(0x09);
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FrameError {
    /// Fewer bytes than the read needs.
    Short { need: usize, have: usize },
    /// A body longer than [`MOST_BODY`].
    TooLong { len: u64 },
    /// The header's reserved half-word is not zero.
    Reserved { bits: u16 },
    /// Bytes left after the last field of a body.
    Trailing { left: usize },
    /// Not a kind the reader takes here.
    Kind { kind: Kind },
    /// A string that is not UTF-8.
    Utf8,
    /// A field out of the range its reader allows.
    Field { what: &'static str, value: u64 },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Short { need, have } => write!(f, "a frame cut short: {need} bytes needed, {have} there"),
            FrameError::TooLong { len } => write!(f, "a frame body of {len} bytes, past the most of {MOST_BODY}"),
            FrameError::Reserved { bits } => write!(f, "a frame header with reserved bits {bits:#06x}"),
            FrameError::Trailing { left } => write!(f, "{left} bytes after the end of a frame body"),
            FrameError::Kind { kind } => write!(f, "a frame of kind {:#06x} where it was not expected", kind.0),
            FrameError::Utf8 => f.write_str("a frame string that is not UTF-8"),
            FrameError::Field { what, value } => write!(f, "a frame field {what} of {value}"),
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub kind: Kind,
    pub len: u32,
    pub seq: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER] {
        let mut out = [0; HEADER];
        out[0..2].copy_from_slice(&self.kind.0.to_le_bytes());
        out[4..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..12].copy_from_slice(&self.seq.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; HEADER]) -> Result<Header, FrameError> {
        let kind = u16::from_le_bytes([bytes[0], bytes[1]]);
        let reserved = u16::from_le_bytes([bytes[2], bytes[3]]);
        let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let seq = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if reserved != 0 {
            return Err(FrameError::Reserved { bits: reserved });
        }
        if len > MOST_BODY {
            return Err(FrameError::TooLong { len: u64::from(len) });
        }
        Ok(Header { kind: Kind(kind), len, seq })
    }
}

/// A frame read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame<'a> {
    pub kind: Kind,
    pub seq: u32,
    pub body: &'a [u8],
}

impl<'a> Frame<'a> {
    /// A reader of the body, for a frame of kind `kind` only.
    pub fn reader(&self, kind: Kind) -> Result<Reader<'a>, FrameError> {
        if self.kind != kind {
            return Err(FrameError::Kind { kind: self.kind });
        }
        Ok(Reader::new(self.body))
    }
}

/// Appends the frame of `kind`, `seq` and `body` to `out`.
pub fn encode(kind: Kind, seq: u32, body: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|it| *it <= MOST_BODY)
        .ok_or(FrameError::TooLong { len: body.len() as u64 })?;
    out.reserve(HEADER + body.len());
    out.extend_from_slice(&Header { kind, len, seq }.encode());
    out.extend_from_slice(body);
    Ok(())
}

/// The first frame of `bytes` and its whole length, `None` while `bytes` holds only part of
/// one, or why the bytes are not a frame.
pub fn decode(bytes: &[u8]) -> Result<Option<(Frame<'_>, usize)>, FrameError> {
    let Some(header) = bytes.first_chunk::<HEADER>() else { return Ok(None) };
    let header = Header::decode(header)?;
    let end = HEADER + header.len as usize;
    let Some(body) = bytes.get(HEADER..end) else { return Ok(None) };
    Ok(Some((Frame { kind: header.kind, seq: header.seq, body }, end)))
}

/// Writes the fields of a body, little-endian. Not `blaze_format::cursor`'s `Put`, nor
/// [`Reader`] its `Reader`: the run process links this crate with only `blaze-mem`, and
/// `blaze-format` would bring zstd and serde into every running program.
pub struct Writer<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    /// A writer appending to `out`.
    pub fn new(out: &'a mut Vec<u8>) -> Writer<'a> {
        Writer { out }
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.out.push(value);
        self
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i64(&mut self, value: i64) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Bytes as they are, with no length: for a field of a length the reader knows.
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.out.extend_from_slice(bytes);
        self
    }

    /// A `u32` length, then the bytes. Longer than [`MOST_BODY`] cannot be read back.
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        self.raw(bytes)
    }

    pub fn str(&mut self, text: &str) -> &mut Self {
        self.bytes(text.as_bytes())
    }
}

/// Reads the fields of a body in order, each bounds-checked.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    /// The bytes not yet read.
    pub fn left(&self) -> usize {
        self.bytes.len() - self.at
    }

    pub fn raw(&mut self, len: usize) -> Result<&'a [u8], FrameError> {
        let have = self.left();
        if len > have {
            return Err(FrameError::Short { need: len, have });
        }
        let out = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(out)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], FrameError> {
        let bytes = self.raw(N)?;
        let mut out = [0; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16, FrameError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, FrameError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, FrameError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64, FrameError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// A `u32` length, then that many bytes.
    pub fn bytes(&mut self) -> Result<&'a [u8], FrameError> {
        let len = self.u32()?;
        if len > MOST_BODY {
            return Err(FrameError::TooLong { len: u64::from(len) });
        }
        self.raw(len as usize)
    }

    pub fn str(&mut self) -> Result<&'a str, FrameError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| FrameError::Utf8)
    }

    /// A count of items that each take at least `each` bytes: more than the bytes left could
    /// hold is an error, so a count never makes a reader allocate for bytes that are not there.
    pub fn count(&mut self, each: usize) -> Result<usize, FrameError> {
        let count = self.u32()? as usize;
        let need = count.saturating_mul(each.max(1));
        if need > self.left() {
            return Err(FrameError::Short { need, have: self.left() });
        }
        Ok(count)
    }

    /// Every byte was read.
    pub fn finish(self) -> Result<(), FrameError> {
        match self.left() {
            0 => Ok(()),
            left => Err(FrameError::Trailing { left }),
        }
    }
}
