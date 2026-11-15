//! The deterministic CBOR a pack's manifest, head and peer answers are (RFC 8949, section
//! 4.2.1): unsigned integers, byte strings, text strings and arrays, every length definite
//! and every head the shortest that holds its number. Arrays in a fixed field order stand
//! for records, so no map is written and no key order is anyone's to get wrong.
//!
//! The reader takes nothing on trust: a head that is not the shortest, a length past what is
//! left of the input, a type other than the one asked for and bytes after the value's end
//! are each an error at their offset, and a length is checked before anything is allocated
//! for it (the reader allocates nothing: strings are borrowed from the input).

use alloc::vec::Vec;

const UINT: u8 = 0;
const BYTES: u8 = 2;
const TEXT: u8 = 3;
const ARRAY: u8 = 4;

/// Why a read stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub why: &'static str,
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let major = major << 5;
    match n {
        0..=23 => out.push(major | n as u8),
        24..=0xff => {
            out.push(major | 24);
            out.push(n as u8);
        }
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
}

pub fn uint(out: &mut Vec<u8>, n: u64) {
    head(out, UINT, n);
}

pub fn bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    head(out, BYTES, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub fn text(out: &mut Vec<u8>, text: &str) {
    head(out, TEXT, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// The head of an array of `len` items, which follow it.
pub fn array(out: &mut Vec<u8>, len: u64) {
    head(out, ARRAY, len);
}

/// A reader over one value's bytes.
pub struct Reader<'a> {
    input: &'a [u8],
    at: usize,
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let left = self.input.len() - self.at;
        if len > left {
            return self.fail("the input ends before the value does");
        }
        let out = &self.input[self.at..self.at + len];
        self.at += len;
        Ok(out)
    }

    /// The number of a head of `major`, the shortest form only.
    fn number(&mut self, major: u8, not: &'static str) -> Result<u64, Error> {
        let start = self.at;
        let Some(&first) = self.input.get(self.at) else {
            return self.fail("the input ends where a value starts");
        };
        if first >> 5 != major {
            return self.fail(not);
        }
        self.at += 1;
        let (n, least) = match first & 0x1f {
            small @ 0..=23 => (u64::from(small), 0),
            24 => (u64::from(self.take(1)?[0]), 24),
            25 => {
                let raw = self.take(2)?;
                (u64::from(u16::from_be_bytes([raw[0], raw[1]])), 0x100)
            }
            26 => {
                let raw = self.take(4)?;
                (u64::from(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])), 0x1_0000)
            }
            27 => {
                let raw = self.take(8)?;
                let mut wide = [0u8; 8];
                wide.copy_from_slice(raw);
                (u64::from_be_bytes(wide), 0x1_0000_0000)
            }
            _ => {
                self.at = start;
                return self.fail("a length that is indefinite or reserved");
            }
        };
        if n < least {
            self.at = start;
            return self.fail("a number not in its shortest form");
        }
        Ok(n)
    }

    pub fn uint(&mut self) -> Result<u64, Error> {
        self.number(UINT, "not an unsigned integer")
    }

    /// A byte string, borrowed.
    pub fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let start = self.at;
        let len = self.number(BYTES, "not a byte string")?;
        let left = (self.input.len() - self.at) as u64;
        if len > left {
            self.at = start;
            return self.fail("a byte string longer than what is left of the input");
        }
        self.take(len as usize)
    }

    /// A byte string of exactly `N` bytes.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let start = self.at;
        let raw = self.bytes()?;
        match <[u8; N]>::try_from(raw) {
            Ok(out) => Ok(out),
            Err(_) => {
                self.at = start;
                self.fail("a byte string of another length than its field's")
            }
        }
    }

    /// A text string, borrowed; UTF-8 checked.
    pub fn text(&mut self) -> Result<&'a str, Error> {
        let start = self.at;
        let len = self.number(TEXT, "not a text string")?;
        let left = (self.input.len() - self.at) as u64;
        if len > left {
            self.at = start;
            return self.fail("a text string longer than what is left of the input");
        }
        let raw = self.take(len as usize)?;
        match core::str::from_utf8(raw) {
            Ok(text) => Ok(text),
            Err(_) => {
                self.at = start;
                self.fail("a text string that is not UTF-8")
            }
        }
    }

    /// The length of an array whose items follow. Each item is at least a byte, so a length
    /// past what is left of the input is refused here, before a caller sizes anything by it.
    pub fn array(&mut self) -> Result<u64, Error> {
        let start = self.at;
        let len = self.number(ARRAY, "not an array")?;
        let left = (self.input.len() - self.at) as u64;
        if len > left {
            self.at = start;
            return self.fail("an array of more items than bytes are left of the input");
        }
        Ok(len)
    }

    /// An array of exactly `len` items.
    pub fn array_of(&mut self, len: u64) -> Result<(), Error> {
        let start = self.at;
        if self.array()? != len {
            self.at = start;
            return self.fail("an array of another length than its record's");
        }
        Ok(())
    }

    /// The value ended where the input does.
    pub fn end(&self) -> Result<(), Error> {
        if self.at != self.input.len() {
            return self.fail("bytes after the value's end");
        }
        Ok(())
    }
}
