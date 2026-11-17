//! The part of CBOR's core deterministic encoding (RFC 8949 section 4.2.1) the patch
//! protocol uses: unsigned integers, byte strings, text, arrays, maps keyed by text, true,
//! false and null, each head in the shortest form that holds it and every length definite.
//!
//! blaze-protocol's codec (`blaze_protocol::cbor`) is the one for any serde type, on `std`
//! and a global heap. The launcher has neither, and its messages are a dozen fixed shapes, so
//! they are written and read here by hand, into and out of the caller's bytes. The bytes are
//! the ones that codec writes for the same shapes (a struct is a map keyed by its field
//! names, in the order of the keys' encodings), and `tests/wire.rs` holds every message to
//! `blaze_protocol::cbor::check`.

use alloc::vec::Vec;
use core::alloc::Allocator;

use super::WireError;

const UNSIGNED: u8 = 0;
const BYTES: u8 = 2;
const TEXT: u8 = 3;
const ARRAY: u8 = 4;
const MAP: u8 = 5;
const SIMPLE: u8 = 7;

const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;
const NULL: u8 = 0xf6;

/// Writes items onto the end of the caller's buffer.
pub struct Writer<'a, A: Allocator> {
    out: &'a mut Vec<u8, A>,
}

impl<'a, A: Allocator> Writer<'a, A> {
    pub fn new(out: &'a mut Vec<u8, A>) -> Writer<'a, A> {
        Writer { out }
    }

    fn head(&mut self, major: u8, value: u64) {
        let major = major << 5;
        if value < 24 {
            self.out.push(major | value as u8);
        } else if value <= 0xff {
            self.out.push(major | 24);
            self.out.push(value as u8);
        } else if value <= 0xffff {
            self.out.push(major | 25);
            self.out.extend_from_slice(&(value as u16).to_be_bytes());
        } else if value <= 0xffff_ffff {
            self.out.push(major | 26);
            self.out.extend_from_slice(&(value as u32).to_be_bytes());
        } else {
            self.out.push(major | 27);
            self.out.extend_from_slice(&value.to_be_bytes());
        }
    }

    pub fn uint(&mut self, value: u64) {
        self.head(UNSIGNED, value);
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.head(BYTES, bytes.len() as u64);
        self.out.extend_from_slice(bytes);
    }

    pub fn text(&mut self, text: &str) {
        self.head(TEXT, text.len() as u64);
        self.out.extend_from_slice(text.as_bytes());
    }

    pub fn array(&mut self, items: usize) {
        self.head(ARRAY, items as u64);
    }

    pub fn map(&mut self, entries: usize) {
        self.head(MAP, entries as u64);
    }

    pub fn bool(&mut self, value: bool) {
        self.out.push(if value { TRUE } else { FALSE });
    }

    pub fn null(&mut self) {
        self.out.push(NULL);
    }
}

/// Reads items from bytes, refusing any that is not in the deterministic encoding, with the
/// offset of the first offence.
#[derive(Clone)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    /// Where the next item begins.
    pub fn at(&self) -> usize {
        self.at
    }

    /// A reader of `bytes[from..to]` whose offsets are those of `bytes`.
    pub fn within(bytes: &'a [u8], from: usize, to: usize) -> Reader<'a> {
        Reader { bytes: &bytes[..to], at: from }
    }

    /// The bytes from `from` (an earlier [`Reader::at`]) to here.
    pub fn since(&self, from: usize) -> &'a [u8] {
        &self.bytes[from..self.at]
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.bytes.len());
        match end {
            Some(end) => {
                let taken = &self.bytes[self.at..end];
                self.at = end;
                Ok(taken)
            }
            None => Err(WireError::Short { at: self.bytes.len() }),
        }
    }

    /// The next head: its major type and its value, which must be in its shortest form.
    fn head(&mut self) -> Result<(u8, u64), WireError> {
        let start = self.at;
        let first = self.take(1)?[0];
        let (major, info) = (first >> 5, first & 0x1f);
        let (value, least) = match info {
            0..=23 => (u64::from(info), 0),
            24 => (u64::from(self.take(1)?[0]), 24),
            25 => {
                let it = self.take(2)?;
                (u64::from(u16::from_be_bytes([it[0], it[1]])), 0x100)
            }
            26 => {
                let it = self.take(4)?;
                (u64::from(u32::from_be_bytes([it[0], it[1], it[2], it[3]])), 0x1_0000)
            }
            27 => {
                let it = self.take(8)?;
                let value =
                    u64::from_be_bytes([it[0], it[1], it[2], it[3], it[4], it[5], it[6], it[7]]);
                (value, 0x1_0000_0000)
            }
            _ => return Err(WireError::Type { at: start, want: "a definite length" }),
        };
        if value < least {
            return Err(WireError::NotShortest { at: start });
        }
        Ok((major, value))
    }

    fn expect(&mut self, major: u8, want: &'static str) -> Result<u64, WireError> {
        let start = self.at;
        match self.head()? {
            (found, value) if found == major => Ok(value),
            _ => Err(WireError::Type { at: start, want }),
        }
    }

    pub fn uint(&mut self) -> Result<u64, WireError> {
        self.expect(UNSIGNED, "an unsigned integer")
    }

    /// An unsigned integer that fits a `u32`; `what` names the field for the refusal.
    pub fn uint32(&mut self, what: &'static str) -> Result<u32, WireError> {
        let start = self.at;
        u32::try_from(self.uint()?).map_err(|_| WireError::Range { at: start, what })
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let start = self.at;
        let len = self.expect(BYTES, "a byte string")?;
        let len = usize::try_from(len).map_err(|_| WireError::Short { at: start })?;
        self.take(len)
    }

    pub fn text(&mut self) -> Result<&'a str, WireError> {
        let start = self.at;
        let len = self.expect(TEXT, "text")?;
        let len = usize::try_from(len).map_err(|_| WireError::Short { at: start })?;
        core::str::from_utf8(self.take(len)?).map_err(|_| WireError::Utf8 { at: start })
    }

    /// An array's head: how many items follow. More items than bytes are left is refused
    /// here, so no count is believed past what the frame can hold.
    pub fn array(&mut self) -> Result<usize, WireError> {
        let start = self.at;
        let items = self.expect(ARRAY, "an array")?;
        match usize::try_from(items) {
            Ok(items) if items <= self.bytes.len() - self.at => Ok(items),
            _ => Err(WireError::Short { at: start }),
        }
    }

    /// A map's head, which must say `entries`.
    pub fn map(&mut self, entries: usize, want: &'static str) -> Result<(), WireError> {
        let start = self.at;
        match self.expect(MAP, want)? {
            said if said == entries as u64 => Ok(()),
            _ => Err(WireError::Type { at: start, want }),
        }
    }

    /// A map's key, which must be `want`: the keys of a deterministic map come in one order,
    /// so the next key is known.
    pub fn key(&mut self, want: &'static str) -> Result<(), WireError> {
        let start = self.at;
        match self.text() {
            Ok(found) if found == want => Ok(()),
            Ok(_) | Err(WireError::Type { .. }) => Err(WireError::Key { at: start, want }),
            Err(other) => Err(other),
        }
    }

    pub fn bool(&mut self) -> Result<bool, WireError> {
        let start = self.at;
        match self.take(1)?[0] {
            TRUE => Ok(true),
            FALSE => Ok(false),
            _ => Err(WireError::Type { at: start, want: "true or false" }),
        }
    }

    /// Takes a null when the next item is one.
    pub fn null(&mut self) -> bool {
        let is = self.bytes.get(self.at) == Some(&NULL);
        if is {
            self.at += 1;
        }
        is
    }

    /// Passes over one item of any shape this encoding has, nested at most `depth` deep.
    pub fn skip(&mut self, depth: u32) -> Result<(), WireError> {
        let start = self.at;
        if self.bytes.get(start).is_some_and(|it| matches!(*it, FALSE | TRUE | NULL)) {
            self.at += 1;
            return Ok(());
        }
        let (major, value) = self.head()?;
        let items = match major {
            UNSIGNED => return Ok(()),
            BYTES | TEXT => {
                let len = usize::try_from(value).map_err(|_| WireError::Short { at: start })?;
                return self.take(len).map(|_| ());
            }
            ARRAY => value,
            MAP => value.saturating_mul(2),
            _ => return Err(WireError::Type { at: start, want: "an item of the schema" }),
        };
        if depth == 0 {
            return Err(WireError::Type { at: start, want: "an item nested less deeply" });
        }
        for _ in 0..items {
            self.skip(depth - 1)?;
        }
        Ok(())
    }

    /// Nothing may follow the item.
    pub fn end(&self) -> Result<(), WireError> {
        match self.at == self.bytes.len() {
            true => Ok(()),
            false => Err(WireError::Left { at: self.at }),
        }
    }
}

// The major type of a simple value is named for the reader of this file: true, false and
// null are written and read as their one byte.
const _: () = assert!(TRUE >> 5 == SIMPLE && FALSE >> 5 == SIMPLE && NULL >> 5 == SIMPLE);
