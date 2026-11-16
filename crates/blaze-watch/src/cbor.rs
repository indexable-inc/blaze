//! The deterministic CBOR every message of the monitor is (rule 33; RFC 8949, section 4.2.1):
//! unsigned integers, byte strings, text strings, arrays and the two booleans, every length
//! definite and every head the shortest that holds its number. Arrays in a fixed field order
//! stand for records, so no map is written and no key order is anyone's to get wrong.
//!
//! The reader takes nothing on trust (Code Review Rules, "Trust": network CBOR decoded with
//! limits): a head that is not the shortest, a length past what is left of the input or past
//! the field's own cap, a type other than the one asked for and bytes after the value's end
//! are each an error at their offset, and every length is checked before anything is sized by
//! it. The reader allocates nothing: strings are borrowed from the input.
//!
//! This is blaze-pack's `cbor` (the same forms, the same refusals), written into a blaze-own
//! `Vec` in the caller's allocator and with a cap on every length. It is a copy because
//! blaze-pack links blaze-report and through it serde, and this crate links nothing third-party;
//! watch.md names the one home both should take it from (rule 30).

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

const UINT: u8 = 0;
const BYTES: u8 = 2;
const TEXT: u8 = 3;
const ARRAY: u8 = 4;
const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;

/// Why a read stopped: where, and what was wrong there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub why: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CBOR at byte {}: {}", self.at, self.why)
    }
}

impl core::error::Error for Error {}

fn head<A: Allocator>(out: &mut Vec<u8, A>, major: u8, n: u64) {
    let major = major << 5;
    match n {
        0..=23 => out.push(major | n as u8),
        24..=0xff => out.extend_from_slice(&[major | 24, n as u8]),
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

pub fn uint<A: Allocator>(out: &mut Vec<u8, A>, n: u64) {
    head(out, UINT, n);
}

pub fn bytes<A: Allocator>(out: &mut Vec<u8, A>, bytes: &[u8]) {
    head(out, BYTES, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// The head of a byte string of `len` bytes, which the caller appends (from parts).
pub fn bytes_head<A: Allocator>(out: &mut Vec<u8, A>, len: usize) {
    head(out, BYTES, len as u64);
}

pub fn text<A: Allocator>(out: &mut Vec<u8, A>, text: &str) {
    head(out, TEXT, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

pub fn boolean<A: Allocator>(out: &mut Vec<u8, A>, value: bool) {
    out.push(if value { TRUE } else { FALSE });
}

/// The head of an array of `len` items, which follow it.
pub fn array<A: Allocator>(out: &mut Vec<u8, A>, len: u64) {
    head(out, ARRAY, len);
}

/// The bytes a head of `n` takes: what a writer that sizes a frame before it writes adds up.
pub const fn head_len(n: u64) -> usize {
    match n {
        0..=23 => 1,
        24..=0xff => 2,
        0x100..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
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

    fn left(&self) -> usize {
        self.input.len() - self.at
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        if len > self.left() {
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
        let (len, least) = match first & 0x1f {
            small @ 0..=23 => return Ok(u64::from(small)),
            24 => (1, 24),
            25 => (2, 0x100),
            26 => (4, 0x1_0000),
            27 => (8, 0x1_0000_0000),
            _ => {
                self.at = start;
                return self.fail("a length that is indefinite or reserved");
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
            return self.fail("a number not in its shortest form");
        }
        Ok(n)
    }

    pub fn uint(&mut self) -> Result<u64, Error> {
        self.number(UINT, "not an unsigned integer")
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
            Some(&TRUE) => {
                self.at += 1;
                Ok(true)
            }
            Some(&FALSE) => {
                self.at += 1;
                Ok(false)
            }
            Some(_) => self.fail("not a boolean"),
            None => self.fail("the input ends where a value starts"),
        }
    }

    /// A byte string of at most `most` bytes, borrowed; `what` says which field's cap it passed.
    pub fn bytes(&mut self, most: usize, what: &'static str) -> Result<&'a [u8], Error> {
        let start = self.at;
        let len = self.number(BYTES, "not a byte string")?;
        if len > most as u64 {
            self.at = start;
            return self.fail(what);
        }
        if len > self.left() as u64 {
            self.at = start;
            return self.fail("a byte string longer than what is left of the input");
        }
        self.take(len as usize)
    }

    /// A byte string of exactly `N` bytes.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let start = self.at;
        let raw = self.bytes(N, "a byte string longer than its field's")?;
        if raw.len() != N {
            self.at = start;
            return self.fail("a byte string of another length than its field's");
        }
        let mut out = [0u8; N];
        out.copy_from_slice(raw);
        Ok(out)
    }

    /// A text string of at most `most` bytes, borrowed; UTF-8 checked.
    pub fn text(&mut self, most: usize, what: &'static str) -> Result<&'a str, Error> {
        let start = self.at;
        let len = self.number(TEXT, "not a text string")?;
        if len > most as u64 {
            self.at = start;
            return self.fail(what);
        }
        if len > self.left() as u64 {
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

    /// The length of an array whose items follow, at most `most`. Each item is at least a byte,
    /// so a length past what is left of the input is refused too, before a caller sizes
    /// anything by it.
    pub fn array(&mut self, most: usize, what: &'static str) -> Result<usize, Error> {
        let start = self.at;
        let len = self.number(ARRAY, "not an array")?;
        if len > most as u64 {
            self.at = start;
            return self.fail(what);
        }
        if len > self.left() as u64 {
            self.at = start;
            return self.fail("an array of more items than bytes are left of the input");
        }
        Ok(len as usize)
    }

    /// An array of exactly `len` items.
    pub fn array_of(&mut self, len: usize) -> Result<(), Error> {
        let start = self.at;
        if self.array(len, "an array of another length than its record's")? != len {
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
