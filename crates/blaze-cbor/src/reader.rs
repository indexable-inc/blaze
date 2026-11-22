//! The field reader: records are arrays in a fixed field order, read one call a field, each
//! length capped by its caller (moved from blaze-watch's `cbor`, blaze-pack's before it).
//!
//! It takes nothing on trust: a head that is not the shortest, a length past what is left of
//! the input or past the field's own cap, a type other than the one asked for and bytes after
//! the value's end are each an error at their offset, and every length is checked before
//! anything is sized by it. It allocates nothing: strings are borrowed from the input.

use crate::{Error, wide, why, write};

/// A reader over one value's bytes.
pub struct Reader<'a> {
    pub(crate) input: &'a [u8],
    pub(crate) at: usize,
}

fn narrow(n: u64) -> usize {
    match usize::try_from(n) {
        Ok(n) => n,
        Err(_) => unreachable!("checked against what is left of the input first"),
    }
}

impl<'a> Reader<'a> {
    pub fn new(input: &'a [u8]) -> Reader<'a> {
        Reader { input, at: 0 }
    }

    /// Where the next read starts.
    pub fn at(&self) -> usize {
        self.at
    }

    fn fail<T>(&self, why: &'static str) -> Result<T, Error> {
        Err(Error { at: self.at, why })
    }

    fn left(&self) -> usize {
        self.input.len() - self.at
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        if len > self.left() {
            return self.fail(why::END_IN_VALUE);
        }
        let out = &self.input[self.at..self.at + len];
        self.at += len;
        Ok(out)
    }

    /// The number of a head of `major`, the shortest form only.
    pub(crate) fn number(&mut self, major: u8, not: &'static str) -> Result<u64, Error> {
        let start = self.at;
        let Some(&first) = self.input.get(self.at) else {
            return self.fail(why::END_AT_VALUE);
        };
        if first >> 5 != major {
            return self.fail(not);
        }
        self.at += 1;
        let (len, least) = match first & 0x1f {
            small @ 0..=23 => return Ok(u64::from(small)),
            24 => (1, 24),
            25 => (2, 0x100),
            26 => (4, 0x1_0000),
            27 => (8, 0x1_0000_0000),
            _ => {
                self.at = start;
                return self.fail(why::INDEFINITE);
            }
        };
        let raw = match self.take(len) {
            Ok(raw) => raw,
            Err(error) => {
                self.at = start;
                return Err(error);
            }
        };
        let n = raw.iter().fold(0u64, |n, byte| (n << 8) | u64::from(*byte));
        if n < least {
            self.at = start;
            return self.fail(why::NOT_SHORTEST);
        }
        Ok(n)
    }

    pub fn uint(&mut self) -> Result<u64, Error> {
        self.number(crate::major::UINT, why::NOT_UINT)
    }

    /// An unsigned integer of at most `most`.
    pub fn uint_most(&mut self, most: u64, what: &'static str) -> Result<u64, Error> {
        let start = self.at;
        let n = self.uint()?;
        if n > most {
            self.at = start;
            return self.fail(what);
        }
        Ok(n)
    }

    pub fn boolean(&mut self) -> Result<bool, Error> {
        match self.input.get(self.at) {
            Some(&write::TRUE) => {
                self.at += 1;
                Ok(true)
            }
            Some(&write::FALSE) => {
                self.at += 1;
                Ok(false)
            }
            Some(_) => self.fail(why::NOT_BOOL),
            None => self.fail(why::END_AT_VALUE),
        }
    }

    /// A length of `major` at most `most` and at most what is left.
    fn length(&mut self, major: u8, not: &'static str, most: usize, what: &'static str, past: &'static str) -> Result<usize, Error> {
        let start = self.at;
        let len = self.number(major, not)?;
        if len > wide(most) {
            self.at = start;
            return self.fail(what);
        }
        if len > wide(self.left()) {
            self.at = start;
            return self.fail(past);
        }
        Ok(narrow(len))
    }

    /// A byte string of at most `most` bytes, borrowed; `what` says which field's cap it passed.
    pub fn bytes(&mut self, most: usize, what: &'static str) -> Result<&'a [u8], Error> {
        let len = self.length(crate::major::BYTES, why::NOT_BYTES, most, what, why::BYTES_PAST_INPUT)?;
        self.take(len)
    }

    /// A byte string of exactly `N` bytes.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let start = self.at;
        let raw = self.bytes(N, why::FIXED_LONGER)?;
        if raw.len() != N {
            self.at = start;
            return self.fail(why::FIXED_LENGTH);
        }
        let mut out = [0u8; N];
        out.copy_from_slice(raw);
        Ok(out)
    }

    /// A text string of at most `most` bytes, borrowed; UTF-8 checked.
    pub fn text(&mut self, most: usize, what: &'static str) -> Result<&'a str, Error> {
        let start = self.at;
        let len = self.length(crate::major::TEXT, why::NOT_TEXT, most, what, why::TEXT_PAST_INPUT)?;
        let raw = self.take(len)?;
        match core::str::from_utf8(raw) {
            Ok(text) => Ok(text),
            Err(_) => {
                self.at = start;
                self.fail(why::NOT_UTF8)
            }
        }
    }

    /// The length of an array whose items follow, at most `most`. Each item is at least a byte,
    /// so a length past what is left of the input is refused too, before a caller sizes
    /// anything by it.
    pub fn array(&mut self, most: usize, what: &'static str) -> Result<usize, Error> {
        self.length(crate::major::ARRAY, why::NOT_ARRAY, most, what, why::ARRAY_PAST_INPUT)
    }

    /// The number of entries of a map whose keys and values follow, at most `most`. As for
    /// [`Reader::array`], a count past what is left of the input is refused before a caller
    /// sizes anything by it. The keys are the caller's to read and to hold to the order of
    /// their encodings (a record keyed by small integers reads them in ascending order).
    pub fn map(&mut self, most: usize, what: &'static str) -> Result<usize, Error> {
        self.length(crate::major::MAP, why::NOT_MAP, most, what, why::MAP_PAST_INPUT)
    }

    /// An array of exactly `len` items.
    pub fn array_of(&mut self, len: usize) -> Result<(), Error> {
        let start = self.at;
        if self.array(len, why::RECORD_LENGTH)? != len {
            self.at = start;
            return self.fail(why::RECORD_LENGTH);
        }
        Ok(())
    }

    /// The value ended where the input does.
    pub fn end(&self) -> Result<(), Error> {
        if self.at != self.input.len() {
            return self.fail(why::BYTES_AFTER);
        }
        Ok(())
    }
}
