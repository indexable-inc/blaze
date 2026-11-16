//! One crate's expansions kept as ONE record: a sorted table of [`TextKey`]s read in place,
//! each with its entry: the read list that is not in the key and the [`Expansion`]'s record.
//!
//! A re-solve of a crate after an edit finds every call whose text key is unchanged here and
//! expands only the calls the edit moved. One record a crate, never a file a call.
//!
//! The set, little-endian, every entry at a multiple of [`ALIGN`] bytes (so an expansion's
//! record is aligned as [`Expansion::read_record`] takes it when the set is):
//!
//! ```text
//! magic (8)  count (u64)  keys (32 each, ascending, no two equal)  ends (u64 each)  entries
//! ```
//!
//! `ends[i]` is where entry `i` ends, counted from the first entry's start; entry `i` starts at
//! `ends[i - 1]` rounded up to [`ALIGN`]. An entry:
//!
//! ```text
//! asked count (u32)  asked length (u32)  asked  (padding to ALIGN)  the expansion's record
//! ```
//!
//! and one asked cfg atom: `answer (u8)  has value (u8)  name length (u16)  value length (u16)
//! name  value`. The bytes are a function of the entries (rule 25): the writer sorts by key,
//! and two entries under one key with different bytes are a typed refusal (rule 21), never one
//! of them picked.

use core::fmt;

use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};

use crate::{
    expansion::Expansion,
    key::TextKey,
    record::RecordError,
};

/// What every entry, and the set, is aligned to: blaze-format's own alignment.
pub const ALIGN: usize = blaze_format::ALIGN;

const MAGIC: [u8; 8] = *b"BZXSET1\0";
const HEAD: usize = 16;
const KEY: usize = 32;
const ENTRY_HEAD: usize = 8;
const ATOM_HEAD: usize = 6;

/// Why a set or an entry was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeptError {
    /// The buffer does not start at a multiple of [`ALIGN`].
    Unaligned,
    /// Not a set of this layout.
    Magic,
    /// The buffer ends before `what` does.
    Short { what: &'static str },
    /// The keys are not ascending at `index`.
    KeyOrder { index: usize },
    /// Entry `index` ends before it starts or past the buffer.
    EntryEnd { index: usize },
    /// An entry's asked atoms are not as written: `what`.
    Asked { what: &'static str },
    /// A cfg atom's name or value is longer than an entry holds.
    AtomTooLong,
    /// Two entries under `key` with different bytes: the key misses an input (rule 21).
    TwoValues { key: TextKey },
    /// More entries or bytes than the layout counts.
    TooLarge,
}

impl fmt::Display for KeptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeptError::Unaligned => write!(f, "a kept expansions set that is not aligned to {ALIGN} bytes"),
            KeptError::Magic => f.write_str("not a kept expansions set (its magic)"),
            KeptError::Short { what } => write!(f, "a kept expansions set cut short in {what}"),
            KeptError::KeyOrder { index } => write!(f, "a kept expansions set whose key {index} is not above the one before"),
            KeptError::EntryEnd { index } => write!(f, "a kept expansions set whose entry {index} ends outside it"),
            KeptError::Asked { what } => write!(f, "a kept expansion whose asked atoms are not as written: {what}"),
            KeptError::AtomTooLong => f.write_str("a cfg atom too long for a kept expansion (65535 bytes a name or a value)"),
            KeptError::TwoValues { key } => write!(f, "two expansions under one text key {:02x?} with different bytes (rule 21: the key misses an input)", &key.0.0[..8]),
            KeptError::TooLarge => f.write_str("a kept expansions set past what its layout counts"),
        }
    }
}

impl core::error::Error for KeptError {}

fn up(at: usize) -> usize {
    at.next_multiple_of(ALIGN)
}

fn u16_at(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

fn u64_at(bytes: &[u8], at: usize) -> Option<usize> {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    usize::try_from(u64::from_le_bytes(word)).ok()
}

/// The cfg atoms an item walk asked ([`crate::ItemAttrs`]) with what each was answered, in the
/// order asked: the part of a kept expansion's read list its key does not hold. Checked again
/// on a hit ([`KeptEntry::asked`]).
#[derive(Debug)]
pub struct Asked<'a> {
    bytes: AVec<'a, u8>,
    count: u32,
}

impl<'a> Asked<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Asked<'a> {
        Asked { bytes: Vec::new_in(alloc), count: 0 }
    }

    /// The atom `name` (with `value`) was asked and answered `answer`.
    pub fn push(&mut self, name: &str, value: Option<&str>, answer: bool) -> Result<(), KeptError> {
        let name_len = u16::try_from(name.len()).map_err(|_| KeptError::AtomTooLong)?;
        let value_len = u16::try_from(value.map_or(0, str::len)).map_err(|_| KeptError::AtomTooLong)?;
        self.count = self.count.checked_add(1).ok_or(KeptError::TooLarge)?;
        self.bytes.extend_from_slice(&[u8::from(answer), u8::from(value.is_some())]);
        self.bytes.extend_from_slice(&name_len.to_le_bytes());
        self.bytes.extend_from_slice(&value_len.to_le_bytes());
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.extend_from_slice(value.unwrap_or("").as_bytes());
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// One asked atom read back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AskedAtom<'e> {
    pub name: &'e str,
    pub value: Option<&'e str>,
    pub answer: bool,
}

/// An entry's asked atoms, in the order asked; checked when the entry was opened.
#[derive(Clone, Copy, Debug)]
pub struct AskedIter<'e> {
    bytes: &'e [u8],
    left: usize,
}

impl<'e> AskedIter<'e> {
    fn step(&mut self) -> Result<Option<AskedAtom<'e>>, KeptError> {
        if self.left == 0 {
            return match self.bytes.is_empty() {
                true => Ok(None),
                false => Err(KeptError::Asked { what: "bytes after the last atom" }),
            };
        }
        let bytes = self.bytes;
        if bytes.len() < ATOM_HEAD {
            return Err(KeptError::Asked { what: "an atom cut short" });
        }
        let (name_len, value_len) = (u16_at(bytes, 2), u16_at(bytes, 4));
        let end = ATOM_HEAD + name_len + value_len;
        if bytes.len() < end || bytes[0] > 1 || bytes[1] > 1 || (bytes[1] == 0 && value_len != 0) {
            return Err(KeptError::Asked { what: "an atom's lengths or flags" });
        }
        let text = |part: &'e [u8]| core::str::from_utf8(part).map_err(|_| KeptError::Asked { what: "an atom that is not UTF-8" });
        let name = text(&bytes[ATOM_HEAD..ATOM_HEAD + name_len])?;
        let value = text(&bytes[ATOM_HEAD + name_len..end])?;
        self.bytes = &bytes[end..];
        self.left -= 1;
        Ok(Some(AskedAtom { name, value: (bytes[1] == 1).then_some(value), answer: bytes[0] == 1 }))
    }
}

impl<'e> Iterator for AskedIter<'e> {
    type Item = AskedAtom<'e>;

    fn next(&mut self) -> Option<AskedAtom<'e>> {
        // Checked whole by `KeptEntry::open`: nothing is refused here.
        self.step().ok().flatten()
    }
}

/// One kept expansion as the set holds it: [`write_entry`]'s bytes, opened.
#[derive(Clone, Copy, Debug)]
pub struct KeptEntry<'e> {
    asked: AskedIter<'e>,
    record: &'e [u8],
}

impl<'e> KeptEntry<'e> {
    /// The entry `bytes` hold (a slice [`KeptSet::find`] gave), its asked atoms checked.
    pub fn open(bytes: &'e [u8]) -> Result<KeptEntry<'e>, KeptError> {
        if bytes.len() < ENTRY_HEAD {
            return Err(KeptError::Short { what: "an entry's head" });
        }
        let (count, len) = (u32_at(bytes, 0), u32_at(bytes, 4));
        let asked_end = ENTRY_HEAD.checked_add(len).ok_or(KeptError::TooLarge)?;
        let record_at = up(asked_end);
        if bytes.len() < record_at {
            return Err(KeptError::Short { what: "an entry's asked atoms" });
        }
        let asked = AskedIter { bytes: &bytes[ENTRY_HEAD..asked_end], left: count };
        let mut walk = asked;
        while walk.step()?.is_some() {}
        Ok(KeptEntry { asked, record: &bytes[record_at..] })
    }

    /// The cfg atoms the item walk asked when this was expanded, with their answers then.
    pub fn asked(&self) -> AskedIter<'e> {
        self.asked
    }

    /// The expansion's record, as [`Expansion::read_record`] takes it.
    pub fn record(&self) -> &'e [u8] {
        self.record
    }

    /// The expansion, read back and checked once.
    pub fn expansion<'a>(&self, alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        Expansion::read_record(self.record, alloc)
    }
}

/// `expansion` with the atoms `asked` as one entry's bytes, in `alloc`. Equal values give
/// equal bytes.
pub fn write_entry<'b>(asked: &Asked<'_>, expansion: &Expansion<'_>, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, KeptError> {
    let record = expansion.write_record(alloc);
    let asked_len = u32::try_from(asked.bytes.len()).map_err(|_| KeptError::TooLarge)?;
    let record_at = up(ENTRY_HEAD + asked.bytes.len());
    let mut out: AVec<'b, u8> = Vec::with_capacity_in(record_at + record.len(), alloc);
    out.extend_from_slice(&asked.count.to_le_bytes());
    out.extend_from_slice(&asked_len.to_le_bytes());
    out.extend_from_slice(&asked.bytes);
    out.resize(record_at, 0);
    out.extend_from_slice(&record);
    Ok(out)
}

/// The set of `entries` (each a key and [`write_entry`]'s bytes), in `alloc`: sorted here, so
/// the bytes do not depend on the order the entries were made in. Two entries of one key with
/// equal bytes are one; with different bytes, [`KeptError::TwoValues`].
pub fn write_set<'b>(entries: &mut [(TextKey, &[u8])], alloc: Alloc<'b>) -> Result<AVec<'b, u8>, KeptError> {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (at, entry) in entries.iter().enumerate() {
        if at > 0 && entries[at - 1].0 == entry.0 {
            if entries[at - 1].1 != entry.1 {
                return Err(KeptError::TwoValues { key: entry.0 });
            }
            continue;
        }
        count += 1;
        bytes = up(bytes) + entry.1.len();
    }
    let first = up(HEAD + count * (KEY + 8));
    let mut out: AVec<'b, u8> = Vec::with_capacity_in(first + bytes, alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(count as u64).to_le_bytes());
    let distinct = |at: usize| at == 0 || entries[at - 1].0 != entries[at].0;
    for (at, entry) in entries.iter().enumerate() {
        if distinct(at) {
            out.extend_from_slice(&entry.0.0.0);
        }
    }
    let mut end = 0usize;
    for (at, entry) in entries.iter().enumerate() {
        if distinct(at) {
            end = up(end) + entry.1.len();
            out.extend_from_slice(&(end as u64).to_le_bytes());
        }
    }
    out.resize(first, 0);
    for (at, entry) in entries.iter().enumerate() {
        if distinct(at) {
            let start = up(out.len());
            out.resize(start, 0);
            out.extend_from_slice(entry.1);
        }
    }
    Ok(out)
}

/// A set read in place: nothing is copied or decoded until an entry is asked for.
#[derive(Clone, Copy, Debug)]
pub struct KeptSet<'k> {
    keys: &'k [u8],
    ends: &'k [u8],
    entries: &'k [u8],
}

impl<'k> KeptSet<'k> {
    /// The set `bytes` hold ([`write_set`]'s), its table checked once: the keys ascend and
    /// every entry lies inside the buffer. `bytes` start at a multiple of [`ALIGN`].
    pub fn open(bytes: &'k [u8]) -> Result<KeptSet<'k>, KeptError> {
        if bytes.as_ptr().align_offset(ALIGN) != 0 {
            return Err(KeptError::Unaligned);
        }
        if bytes.len() < HEAD {
            return Err(KeptError::Short { what: "its head" });
        }
        if bytes[..8] != MAGIC {
            return Err(KeptError::Magic);
        }
        let count = u64_at(bytes, 8).ok_or(KeptError::TooLarge)?;
        let table = count.checked_mul(KEY + 8).and_then(|t| t.checked_add(HEAD)).ok_or(KeptError::TooLarge)?;
        let first = up(table);
        if bytes.len() < first {
            return Err(KeptError::Short { what: "its key table" });
        }
        let keys = &bytes[HEAD..HEAD + count * KEY];
        let ends = &bytes[HEAD + count * KEY..table];
        let entries = &bytes[first..];
        let mut before = 0usize;
        for index in 0..count {
            if index > 0 && keys[(index - 1) * KEY..index * KEY] >= keys[index * KEY..(index + 1) * KEY] {
                return Err(KeptError::KeyOrder { index });
            }
            let end = u64_at(ends, index * 8).ok_or(KeptError::TooLarge)?;
            if end < up(before) || end > entries.len() {
                return Err(KeptError::EntryEnd { index });
            }
            before = end;
        }
        Ok(KeptSet { keys, ends, entries })
    }

    /// How many expansions the set holds.
    pub fn len(&self) -> usize {
        self.keys.len() / KEY
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The key at `index`, in ascending order.
    pub fn key(&self, index: usize) -> TextKey {
        let mut key = [0u8; KEY];
        key.copy_from_slice(&self.keys[index * KEY..(index + 1) * KEY]);
        TextKey(ContentId(key))
    }

    /// The entry at `index`: its bytes as [`write_entry`] wrote them.
    pub fn entry(&self, index: usize) -> &'k [u8] {
        // Checked by `open`: every end is a `usize` inside `entries`, at or past its start.
        let end = |at: usize| u64_at(self.ends, at * 8).unwrap_or(0);
        let start = if index == 0 { 0 } else { up(end(index - 1)) };
        &self.entries[start..end(index)]
    }

    /// Where `key` is in the key table: a binary search.
    pub fn position(&self, key: &TextKey) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.keys[mid * KEY..(mid + 1) * KEY].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The entry kept under `key`.
    pub fn find(&self, key: &TextKey) -> Option<&'k [u8]> {
        self.position(key).map(|index| self.entry(index))
    }
}
