//! The flat byte layout of this crate's payloads, inside a core record (`blaze_keyed::record`,
//! whose head checks the kind, its version and the full key): a layout word, then fields written
//! in order, each fixed-width little-endian or length-prefixed. No offset or pointer is written,
//! so the bytes are position-independent and read in place: a [`Reader`] lends slices of the
//! bytes it reads and allocates nothing.
//!
//! Hand-written until the generic `Persist` over `for const` lands (memory-and-data skill,
//! `outputs-are-records`); a payload of another layout is refused by name ([`Corrupt`]), never
//! read as this one.

use std::{alloc::Global, fmt};

use blaze_keyed::kind::Kind;

/// The version of every payload this crate writes: a change to any payload's fields bumps it.
pub const PAYLOAD_LAYOUT: u32 = 1;

/// Why bytes did not read as a payload of a kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Corrupt {
    /// The kind it was read as.
    pub kind: &'static str,
    /// The byte the reader stopped at.
    pub at: usize,
    pub why: Why,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// A payload of another layout (an older or newer blaze wrote it).
    Layout { found: u32, expected: u32 },
    /// It ends before a field does.
    Short,
    /// A tag no layout of this kind has.
    Tag(u8),
    /// A text field that is not UTF-8.
    Utf8,
    /// Bytes left after the last field.
    Trailing,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a {} payload that does not read at byte {}: ", self.kind, self.at)?;
        match self.why {
            Why::Layout { found, expected } => write!(
                f,
                "it is of layout {found}, and this blaze reads layout {expected}: it is not read"
            ),
            Why::Short => f.write_str("it ends inside a field"),
            Why::Tag(tag) => write!(f, "it has the tag {tag}, which no field of its layout has"),
            Why::Utf8 => f.write_str("a text field is not UTF-8"),
            Why::Trailing => f.write_str("bytes follow its last field"),
        }
    }
}

/// Writes a payload of kind `K`.
pub struct Writer {
    bytes: Vec<u8, Global>,
}

impl Writer {
    pub fn new<K: Kind>() -> Writer {
        let mut bytes = Vec::new_in(Global);
        bytes.extend_from_slice(&PAYLOAD_LAYOUT.to_le_bytes());
        Writer { bytes }
    }

    pub fn u8(&mut self, value: u8) -> &mut Writer {
        self.bytes.push(value);
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i64(&mut self, value: i64) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn digest(&mut self, digest: &crate::Digest) -> &mut Writer {
        self.bytes.extend_from_slice(&digest.0);
        self
    }

    /// Length-prefixed bytes.
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Writer {
        self.u32(u32::try_from(bytes.len()).expect("a field of a record is under 4 GiB"));
        self.bytes.extend_from_slice(bytes);
        self
    }

    pub fn str(&mut self, text: &str) -> &mut Writer {
        self.bytes(text.as_bytes())
    }

    pub fn finish(self) -> Vec<u8, Global> {
        self.bytes
    }
}

/// Reads a payload of kind `K` in place.
#[derive(Clone)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    kind: &'static str,
}

impl<'a> Reader<'a> {
    /// The fields of `bytes`, after checking they are of this crate's payload layout.
    pub fn open<K: Kind>(bytes: &'a [u8]) -> Result<Reader<'a>, Corrupt> {
        let mut reader = Reader { bytes, at: 0, kind: K::NAME };
        let found = reader.u32()?;
        if found != PAYLOAD_LAYOUT {
            return Err(reader.corrupt(Why::Layout { found, expected: PAYLOAD_LAYOUT }));
        }
        Ok(reader)
    }

    pub fn corrupt(&self, why: Why) -> Corrupt {
        Corrupt { kind: self.kind, at: self.at, why }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Corrupt> {
        let Some(part) = self.bytes.get(self.at..self.at + len) else {
            return Err(self.corrupt(Why::Short));
        };
        self.at += len;
        Ok(part)
    }

    pub fn u8(&mut self) -> Result<u8, Corrupt> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, Corrupt> {
        let part = self.take(4)?;
        Ok(u32::from_le_bytes(part.try_into().expect("four bytes were taken")))
    }

    pub fn u64(&mut self) -> Result<u64, Corrupt> {
        let part = self.take(8)?;
        Ok(u64::from_le_bytes(part.try_into().expect("eight bytes were taken")))
    }

    pub fn i64(&mut self) -> Result<i64, Corrupt> {
        let part = self.take(8)?;
        Ok(i64::from_le_bytes(part.try_into().expect("eight bytes were taken")))
    }

    pub fn digest(&mut self) -> Result<crate::Digest, Corrupt> {
        let part = self.take(32)?;
        Ok(crate::Digest(part.try_into().expect("32 bytes were taken")))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], Corrupt> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn str(&mut self) -> Result<&'a str, Corrupt> {
        let bytes = self.bytes()?;
        std::str::from_utf8(bytes).map_err(|_| self.corrupt(Why::Utf8))
    }

    /// The bytes from where this reader is to where `later`, a clone of it read further, is.
    pub(crate) fn between(&self, later: &Reader<'a>) -> &'a [u8] {
        &self.bytes[self.at..later.at]
    }

    /// The rest of the bytes, unread.
    pub fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.at..];
        self.at = self.bytes.len();
        rest
    }

    /// Checks nothing is left.
    pub fn end(&self) -> Result<(), Corrupt> {
        match self.at == self.bytes.len() {
            true => Ok(()),
            false => Err(self.corrupt(Why::Trailing)),
        }
    }
}
