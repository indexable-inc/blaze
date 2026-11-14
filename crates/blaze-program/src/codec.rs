//! The payload codec: how this crate's values are laid out inside the core's records
//! (`blaze_keyed`). The record framing (head, kind tag, version, full key) is the core's, filed
//! through [`crate::keyed`]; what is here is only the bytes of a payload: little-endian, a count
//! before what it counts. The layout of every payload is a function of its value alone: two
//! equal values encode to equal bytes, and `decode(encode(v)) == v`.
//!
//! A payload here is decoded into owned values, a copy; reading it in place as a blaze-format
//! record is the next step once the core's roots for these kinds exist (the report's list).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

pub use blaze_ids::{ContentId as Digest, Hasher};

/// A value carried as a payload of a core record: its name for errors, and its bytes. The
/// payload's version is the version of the core kind it is filed under ([`crate::keyed`]).
pub trait Payload: Sized {
    const NAME: &'static str;

    fn encode_body(&self, out: &mut Out);

    fn decode_body(input: &mut In<'_>) -> Result<Self, CodecError>;

    /// The payload's bytes.
    fn encode(&self) -> Vec<u8> {
        let mut out = Out::new();
        self.encode_body(&mut out);
        out.finish()
    }

    /// The value `bytes` hold, exactly: bytes after it are refused.
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut input = In::new(Self::NAME, bytes);
        let value = Self::decode_body(&mut input)?;
        input.end()?;
        Ok(value)
    }
}

/// Why bytes are not the payload they were read as: the payload's name, the byte offset and
/// what was wrong there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CodecError {
    pub kind: &'static str,
    pub at: usize,
    pub what: &'static str,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a {} payload, at byte {}: {}", self.kind, self.at, self.what)
    }
}

/// A payload being written: little-endian, a count before what it counts.
pub struct Out(Vec<u8, Global>);

impl Default for Out {
    fn default() -> Out {
        Out::new()
    }
}

impl Out {
    pub fn new() -> Out {
        Out(Vec::new_in(Global))
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    pub fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub fn bool(&mut self, value: bool) {
        self.0.push(u8::from(value));
    }

    pub fn u16(&mut self, value: u16) {
        self.raw(&value.to_le_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.raw(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.raw(&value.to_le_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.raw(&value.to_le_bytes());
    }

    pub fn digest(&mut self, value: &Digest) {
        self.raw(&value.0);
    }

    /// A count of what follows. A count over `u32::MAX` is a payload no machine of this crate
    /// makes: refused loudly.
    pub fn count(&mut self, count: usize) {
        let Ok(count) = u32::try_from(count) else {
            panic!("a blaze-program payload holds a list of {count} entries, over u32::MAX");
        };
        self.u32(count);
    }

    /// Length-prefixed bytes.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.count(bytes.len());
        self.raw(bytes);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// A payload being read, with its name in every error.
pub struct In<'a> {
    kind: &'static str,
    bytes: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    pub fn new(kind: &'static str, bytes: &'a [u8]) -> In<'a> {
        In { kind, bytes, at: 0 }
    }

    pub fn error(&self, what: &'static str) -> CodecError {
        CodecError { kind: self.kind, at: self.at, what }
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        let end = self.at.checked_add(len).ok_or(self.error("a length past the end"))?;
        let bytes: &'a [u8] = self.bytes;
        let Some(part) = bytes.get(self.at..end) else {
            return Err(self.error("the payload ends early"));
        };
        self.at = end;
        Ok(part)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let part = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(part);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }

    pub fn bool(&mut self) -> Result<bool, CodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.error("a flag that is neither 0 nor 1")),
        }
    }

    pub fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64, CodecError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    pub fn digest(&mut self) -> Result<Digest, CodecError> {
        Ok(Digest(self.array()?))
    }

    /// A count of entries of at least `least` bytes each: refused when the payload cannot hold
    /// that many, so a torn or hostile count allocates nothing.
    pub fn count(&mut self, least: usize) -> Result<usize, CodecError> {
        let count = self.u32()? as usize;
        let left = self.bytes.len() - self.at;
        if count.saturating_mul(least.max(1)) > left {
            return Err(self.error("a count of more entries than the payload holds"));
        }
        Ok(count)
    }

    /// Length-prefixed bytes, borrowed from the payload.
    pub fn bytes(&mut self) -> Result<&'a [u8], CodecError> {
        let len = self.count(1)?;
        self.take(len)
    }

    /// Length-prefixed bytes, copied.
    pub fn bytes_owned(&mut self) -> Result<Vec<u8>, CodecError> {
        let part = self.bytes()?;
        let mut out = Vec::with_capacity_in(part.len(), Global);
        out.extend_from_slice(part);
        Ok(out)
    }

    /// Whether every byte is read.
    pub fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }

    /// Refuses bytes after the value: a payload is exactly its value.
    pub fn end(&self) -> Result<(), CodecError> {
        match self.at == self.bytes.len() {
            true => Ok(()),
            false => Err(self.error("bytes after the payload's value")),
        }
    }
}
