//! A read-list's bytes. Every number is little-endian:
//!
//! ```text
//!  0  u64       the layout word, [`LAYOUT`]
//!  8  [u8; 32]  the base key the list is filed under
//! 40  u32       the note's length
//! 44  u32       how many reads
//! 48  the note, then each read: u32 length, its name
//! ```
//!
//! The **note** is bytes the filer keeps beside the reads (a goal's stable form): part of the
//! list's bytes, so of its answers' keys, and never read here. A read's **name** is its
//! filer's encoding of what was read; this crate compares and hashes it and no more.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::fmt;

use blaze_ids::ContentId;

/// The layout word of a read-list: `bzrecl`, then the layout's number in two digits. A change
/// to the bytes above takes the next number; [`ListRef::open`] refuses every other word.
pub const LAYOUT: u64 = u64::from_le_bytes(*b"bzrecl01");

const HEAD: usize = 8 + 32 + 4 + 4;
const NOTE_LEN_AT: usize = 8 + 32;
const COUNT_AT: usize = 8 + 32 + 4;

fn len32(len: usize, what: &str) -> [u8; 4] {
    match u32::try_from(len) {
        Ok(len) => len.to_le_bytes(),
        Err(_) => panic!("{what} of {len} bytes in a read-list: its length is kept in 32 bits"),
    }
}

/// Appends to `out` the read-list of `base` whose reads are named `reads`, in that order.
pub fn write_list<R: AsRef<[u8]>>(
    out: &mut Vec<u8>,
    base: &ContentId,
    note: &[u8],
    reads: impl IntoIterator<Item = R>,
) {
    // alloc: the caller's buffer, which it reuses or keeps as the list
    write_list_with(out, base, |out| out.extend_from_slice(note), reads);
}

/// As [`write_list`], for a note that is not bytes yet: `note` appends it to `out` (and does
/// nothing else to `out`), so it is encoded once, in place.
pub fn write_list_with<R: AsRef<[u8]>>(
    out: &mut Vec<u8>,
    base: &ContentId,
    note: impl FnOnce(&mut Vec<u8>),
    reads: impl IntoIterator<Item = R>,
) {
    let start = out.len();
    // alloc: the caller's buffer, which it reuses or keeps as the list
    out.reserve(HEAD);
    out.extend_from_slice(&LAYOUT.to_le_bytes());
    out.extend_from_slice(&base.0);
    out.extend_from_slice(&[0; 8]);
    note(out);
    let note_len = match out.len().checked_sub(start + HEAD) {
        Some(len) => len,
        None => panic!("the note of a read-list was written by cutting its buffer short"),
    };
    out[start + NOTE_LEN_AT..start + COUNT_AT].copy_from_slice(&len32(note_len, "a note"));
    let mut count = 0_usize;
    for read in reads {
        let read = read.as_ref();
        // alloc: as above
        out.extend_from_slice(&len32(read.len(), "a read's name"));
        out.extend_from_slice(read);
        count += 1;
    }
    out[start + COUNT_AT..start + HEAD].copy_from_slice(&len32(count, "a count of reads"));
}

/// The read-list of `base` with no note and no reads, as [`write_list`] writes it. Its bytes
/// are known from the base alone, so a filer whose work reads nothing keys its answer by them
/// ([`crate::AnswerKey`]) without filing a list or allocating one: one probe, and the answer a
/// lookup of the same list filed would find.
pub fn empty_list(base: &ContentId) -> [u8; HEAD] {
    let mut out = [0; HEAD];
    out[..8].copy_from_slice(&LAYOUT.to_le_bytes());
    out[8..NOTE_LEN_AT].copy_from_slice(&base.0);
    out
}

/// Why bytes are not a read-list of the base asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    /// The layout word is this one, not [`LAYOUT`]: a previous or later layout, or bytes that
    /// are no read-list at all.
    Layout(u64),
    /// The bytes end inside the list.
    Short,
    /// A read-list filed under another base.
    OtherBase,
    /// Bytes past the last read.
    Trailing,
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ListError::Layout(found) => {
                let word = found.to_le_bytes();
                let ours = LAYOUT.to_le_bytes();
                match word[..6] == ours[..6] && word[6].is_ascii_digit() && word[7].is_ascii_digit()
                {
                    true => write!(
                        f,
                        "a read-list of layout {}{}, where this blaze reads layout {}{}",
                        word[6] as char, word[7] as char, ours[6] as char, ours[7] as char,
                    ),
                    false => write!(f, "not a read-list: its layout word is {found:#018x}"),
                }
            }
            ListError::Short => f.write_str("a read-list cut short"),
            ListError::OtherBase => f.write_str("the read-list of another base key"),
            ListError::Trailing => f.write_str("a read-list with bytes past its last read"),
        }
    }
}

fn take(bytes: &[u8], len: usize) -> Result<(&[u8], &[u8]), ListError> {
    bytes.split_at_checked(len).ok_or(ListError::Short)
}

fn take_u32(bytes: &[u8]) -> Result<(u32, &[u8]), ListError> {
    let (word, rest) = bytes.split_first_chunk::<4>().ok_or(ListError::Short)?;
    Ok((u32::from_le_bytes(*word), rest))
}

/// A read-list, read in place.
#[derive(Clone, Copy, Debug)]
pub struct ListRef<'a> {
    note: &'a [u8],
    reads: &'a [u8],
    count: u32,
}

impl<'a> ListRef<'a> {
    /// The read-list in `bytes`, checked to be of this layout, of `base` and whole. Nothing
    /// is allocated.
    pub fn open(bytes: &'a [u8], base: &ContentId) -> Result<ListRef<'a>, ListError> {
        let (word, rest) = bytes.split_first_chunk::<8>().ok_or(ListError::Short)?;
        let word = u64::from_le_bytes(*word);
        if word != LAYOUT {
            return Err(ListError::Layout(word));
        }
        let (kept_base, rest) = take(rest, 32)?;
        if kept_base != base.0 {
            return Err(ListError::OtherBase);
        }
        let (note_len, rest) = take_u32(rest)?;
        let (count, rest) = take_u32(rest)?;
        let (note, reads) = take(rest, note_len as usize)?;
        let mut rest = reads;
        for _ in 0..count {
            let (len, after) = take_u32(rest)?;
            (_, rest) = take(after, len as usize)?;
        }
        if !rest.is_empty() {
            return Err(ListError::Trailing);
        }
        Ok(ListRef { note, reads, count })
    }

    pub fn note(self) -> &'a [u8] {
        self.note
    }

    /// How many reads the list names.
    pub fn len(self) -> usize {
        self.count as usize
    }

    pub fn is_empty(self) -> bool {
        self.count == 0
    }

    /// The name of each read, in the list's order.
    pub fn reads(self) -> Reads<'a> {
        Reads { rest: self.reads, left: self.count }
    }
}

/// The names of a list's reads.
#[derive(Clone, Debug)]
pub struct Reads<'a> {
    rest: &'a [u8],
    left: u32,
}

impl<'a> Iterator for Reads<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let whole = "`ListRef::open` checked every read";
        let (len, after) = take_u32(self.rest).expect(whole);
        let (name, rest) = take(after, len as usize).expect(whole);
        self.rest = rest;
        Some(name)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}

impl ExactSizeIterator for Reads<'_> {}
