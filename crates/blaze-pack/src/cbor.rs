//! The deterministic CBOR a pack's manifest, head and peer answers are: blaze-cbor's (RFC 8949,
//! section 4.2.1), under the path this crate's callers have always named. Unsigned integers,
//! byte strings, text strings and arrays, every length definite and every head the shortest
//! that holds its number. Arrays in a fixed field order stand for records, so no map is
//! written and no key order is anyone's to get wrong.
//!
//! The one home of the encoding and of the reader's refusals is blaze-cbor (rule 30); what is
//! here only fits it to this crate's `alloc` `Vec` and its uncapped reads (a pack's fields are
//! bounded by the input's length and by the record bound, `manifest::RECORD_MOST`).

use alloc::vec::Vec;

pub use blaze_cbor::Error;
use blaze_cbor::{Head, major, why};

pub fn uint(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(Head::new(major::UINT, n).as_bytes());
}

pub fn bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(Head::new(major::BYTES, bytes.len() as u64).as_bytes());
    out.extend_from_slice(bytes);
}

pub fn text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(Head::new(major::TEXT, text.len() as u64).as_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// The head of an array of `len` items, which follow it.
pub fn array(out: &mut Vec<u8>, len: u64) {
    out.extend_from_slice(Head::new(major::ARRAY, len).as_bytes());
}

/// A reader over one value's bytes: blaze-cbor's, each length bounded by the input alone.
pub struct Reader<'a>(blaze_cbor::Reader<'a>);

impl<'a> Reader<'a> {
    pub fn new(input: &'a [u8]) -> Reader<'a> {
        Reader(blaze_cbor::Reader::new(input))
    }

    /// Where the next read starts.
    pub fn at(&self) -> usize {
        self.0.at()
    }

    pub fn uint(&mut self) -> Result<u64, Error> {
        self.0.uint()
    }

    /// A byte string, borrowed.
    pub fn bytes(&mut self) -> Result<&'a [u8], Error> {
        self.0.bytes(usize::MAX, why::BYTES_PAST_INPUT)
    }

    /// A byte string of exactly `N` bytes.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.0.fixed()
    }

    /// A text string, borrowed; UTF-8 checked.
    pub fn text(&mut self) -> Result<&'a str, Error> {
        self.0.text(usize::MAX, why::TEXT_PAST_INPUT)
    }

    /// The length of an array whose items follow, refused past what is left of the input.
    pub fn array(&mut self) -> Result<u64, Error> {
        self.0.array(usize::MAX, why::ARRAY_PAST_INPUT).map(|len| len as u64)
    }

    /// An array of exactly `len` items.
    pub fn array_of(&mut self, len: u64) -> Result<(), Error> {
        self.0.array_of(usize::try_from(len).unwrap_or(usize::MAX))
    }

    /// The value ended where the input does.
    pub fn end(&self) -> Result<(), Error> {
        self.0.end()
    }
}
