//! The line part of a file: where each item and each of its own tokens is, and where the
//! file's lines start (end-state.md, review point 7: positions are placed, not baked).
//!
//! An entry of `items(file)` holds no byte position; a position in an item is a token's index
//! among the item's own tokens. This part turns (item, token index) into a byte offset, and a
//! byte offset into a line and a column. A comment above an item moves this part and nothing
//! else: what holds a position holds a slot filled from here.
//!
//! A row an item, under its id: the item's start in the file (`u32`), its length (`u32`), then
//! each own token's start as a LEB128 distance from the own token before it (the first from
//! the item's start). Under [`LINE_STARTS`]: the offset of each line's first byte (`u32` each;
//! the first is 0).
//!
//! Cost against the floor: made in the same walk as the entries (`O(file)`, about a byte a
//! token); a lookup is one table probe and a scan of one item's row.

#![blaze::sans_io]

use alloc::{format, string::String, vec::Vec};

use crate::table::{Family, Key};

/// The line part's table, as an error names it.
pub const LINES: Family = Family { name: "lines" };

/// Bytes of a row before its token offsets.
const FIXED: usize = 16;

/// The key of the file's line starts.
pub const LINE_STARTS: Key = {
    let mut key = [0xff; 16];
    key[15] = 4;
    key
};

/// One item's row: its start, its length, its body's first token and count of tokens (in the
/// item's own tokens; `(0, 0)` for no body), and `deltas` as the walk wrote them.
pub(crate) fn row(start: u32, len: u32, body_span: (u32, u32), deltas: &[u8]) -> Vec<u8> {
    // alloc: one item's line row, kept in the step's value
    let mut out = Vec::with_capacity(FIXED + deltas.len());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body_span.0.to_le_bytes());
    out.extend_from_slice(&body_span.1.to_le_bytes());
    out.extend_from_slice(deltas);
    out
}

/// The entry under [`LINE_STARTS`] for `text`.
pub(crate) fn line_starts(text: &str) -> Vec<u8> {
    // alloc: the file's line starts, kept in the step's value
    let mut out = Vec::new();
    out.extend_from_slice(&0u32.to_le_bytes());
    for (at, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            out.extend_from_slice(&(at as u32 + 1).to_le_bytes());
        }
    }
    out
}

/// One item's row, read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemLines<'a> {
    /// Where the item starts in its file, its attributes and doc comments included.
    pub start: u32,
    /// How many bytes it is.
    pub len: u32,
    /// The body's first token and its count of tokens, among the item's own tokens
    /// ([`ItemLines::tokens`]): where the body is, which moves with the header and so is no
    /// part of `items(file)`.
    pub body_span: (u32, u32),
    deltas: &'a [u8],
}

impl<'a> ItemLines<'a> {
    /// The row `bytes` are; bytes that are no row are refused saying why.
    pub fn read(bytes: &'a [u8]) -> Result<ItemLines<'a>, String> {
        if bytes.len() < FIXED {
            // alloc: error path
            return Err(format!("an item's line row of {} bytes, under its fixed {FIXED}", bytes.len()));
        }
        let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        if bytes.last().is_some_and(|last| bytes.len() > FIXED && last & 0x80 != 0) {
            // alloc: error path
            return Err(String::from("an item's line row whose last token offset is cut short"));
        }
        Ok(ItemLines {
            start: word(0),
            len: word(4),
            body_span: (word(8), word(12)),
            deltas: &bytes[FIXED..],
        })
    }

    /// Where each of the item's own tokens starts in the file, in order.
    pub fn tokens(&self) -> Tokens<'a> {
        Tokens { at: self.start, rest: self.deltas }
    }
}

/// The starts of an item's own tokens.
#[derive(Clone, Debug)]
pub struct Tokens<'a> {
    at: u32,
    rest: &'a [u8],
}

impl Iterator for Tokens<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.rest.is_empty() {
            return None;
        }
        let (mut delta, mut shift, mut used) = (0u32, 0u32, 0usize);
        for byte in self.rest {
            used += 1;
            delta |= u32::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        self.rest = &self.rest[used..];
        self.at = self.at.wrapping_add(delta);
        Some(self.at)
    }
}

/// The line (from 0) and the byte column of `offset`, from the entry under [`LINE_STARTS`].
/// Bytes that are no line starts are refused saying why.
pub fn line_col(starts: &[u8], offset: u32) -> Result<(u32, u32), String> {
    if starts.len() < 4 || starts.len() % 4 != 0 {
        // alloc: error path
        return Err(format!("line starts in {} bytes, which is no count of u32s", starts.len()));
    }
    let start = |line: usize| {
        u32::from_le_bytes([
            starts[line * 4],
            starts[line * 4 + 1],
            starts[line * 4 + 2],
            starts[line * 4 + 3],
        ])
    };
    // The last line that starts at or before `offset`.
    let (mut low, mut high) = (0, starts.len() / 4);
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        if start(mid) <= offset {
            low = mid;
        } else {
            high = mid;
        }
    }
    Ok((low as u32, offset - start(low)))
}
