//! One crate's expansions kept as ONE set: the set's form is [`crate::kept_set`] (one root,
//! entries as rows over shared columns, token texts once per set); this module holds what goes
//! into it: the read list that is not in the key ([`Asked`]) and one expansion made this solve
//! as an entry ([`write_entry`], opened by [`KeptEntry`]).
//!
//! A re-solve of a crate after an edit finds every call whose key ([`crate::call_key`]) is unchanged in the set
//! and expands only the calls the edit moved. One record a crate, never a file a call.
//!
//! An entry made this solve, little-endian:
//!
//! ```text
//! asked count (u32)  asked length (u32)  asked  column lengths (u32 x 14)
//! token record length (u32)  the columns  zeros to a multiple of 16  the token record
//! ```
//!
//! the columns being the expansion record's ([`Expansion::write_columns`]) without its root;
//! one asked cfg atom: `answer (u8)  has value (u8)  name length (u16)  value length (u16)
//! name  value`. The token record is the expansion's blaze-tokens record
//! ([`Expansion::token_record`], D122): its own blaze-format buffer, placed at a multiple of 16
//! bytes from the entry's start, so it opens where it is ([`KeptEntry::tokens`]) when the entry
//! itself starts 16-aligned (a store's buffer does). Equal values give equal bytes.

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use blaze_tokens::{OpenError, Tokens};

use crate::{
    expansion::Expansion,
    key::CallKey,
    record::{COLUMNS, RecordError},
    token_record::{OriginMaps, TokenRecordError},
};

pub use crate::kept_set::{KeptSet, SetEntry, SetPlant, SetSource, write_set, write_set_planted};

/// What the store aligns a kept set to: blaze-format's own alignment. The set itself reads at
/// any alignment.
pub const ALIGN: usize = blaze_format::ALIGN;

const ENTRY_HEAD: usize = 8;
const ATOM_HEAD: usize = 6;

/// Why a set or an entry was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeptError {
    /// Not a set of this layout.
    Magic,
    /// A set of an earlier layout ([`crate::kept_set::MAGIC_V1`]): detected, never misread;
    /// the caller rebuilds it.
    OldForm,
    /// A table or column of the set is not as written: `what`, at `index` of it.
    Table { what: &'static str, index: usize },
    /// Bytes past the set's last column.
    Trailing,
    /// The writer's allocator refused to grow.
    OutOfMemory,
    /// One of two expansions under `key` did not read back for comparing.
    Unreadable { key: CallKey },
    /// The set's token atom table ([`crate::atoms`]) refused, or refused a token written to it.
    Atoms { error: crate::atoms::AtomsError },
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
    TwoValues { key: CallKey },
    /// More entries or bytes than the layout counts.
    TooLarge,
    /// The expansion's tokens make no token record ([`Expansion::token_record`]).
    TokenRecord { error: TokenRecordError },
    /// The bytes between an entry's columns and its token record are not the zeros written.
    TokenPad,
}

impl fmt::Display for KeptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeptError::Magic => f.write_str("not a kept expansions set (its magic)"),
            KeptError::OldForm => f.write_str("a kept expansions set of an earlier layout (BZXSET1 or BZXSET2): rebuilt, never read"),
            KeptError::Atoms { error } => write!(f, "a kept expansions set whose token atoms are not as written: {error}"),
            KeptError::Table { what, index } => write!(f, "a kept expansions set with {what} (at {index})"),
            KeptError::Trailing => f.write_str("a kept expansions set with bytes past its last column"),
            KeptError::OutOfMemory => f.write_str("a kept expansions set whose allocator refused to grow"),
            KeptError::Unreadable { key } => write!(f, "an expansion under call key {:02x?} that did not read back to be compared", &key.0.0[..8]),
            KeptError::Short { what } => write!(f, "a kept expansions set cut short in {what}"),
            KeptError::KeyOrder { index } => write!(f, "a kept expansions set whose key {index} is not above the one before"),
            KeptError::EntryEnd { index } => write!(f, "a kept expansions set whose entry {index} ends outside it"),
            KeptError::Asked { what } => write!(f, "a kept expansion whose asked atoms are not as written: {what}"),
            KeptError::AtomTooLong => f.write_str("a cfg atom too long for a kept expansion (65535 bytes a name or a value)"),
            KeptError::TwoValues { key } => write!(f, "two expansions under one call key {:02x?} with different bytes (rule 21: the key misses an input)", &key.0.0[..8]),
            KeptError::TooLarge => f.write_str("a kept expansions set past what its layout counts"),
            KeptError::TokenRecord { error } => write!(f, "a kept expansion whose tokens make no token record: {error}"),
            KeptError::TokenPad => f.write_str("a kept expansion whose bytes before its token record are not the zeros written"),
        }
    }
}

impl core::error::Error for KeptError {}

fn u16_at(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
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
    /// The `left` atoms `bytes` hold, every one checked once here (a kept set's entry,
    /// [`crate::kept_set`]).
    pub(crate) fn checked(bytes: &'e [u8], left: usize) -> Result<AskedIter<'e>, KeptError> {
        let asked = AskedIter { bytes, left };
        let mut walk = asked;
        while walk.step()?.is_some() {}
        Ok(asked)
    }

    /// The atoms' bytes as [`Asked`] wrote them, and how many: what a set copies as it is.
    pub(crate) fn raw(&self) -> (&'e [u8], usize) {
        (self.bytes, self.left)
    }

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

/// One expansion made this solve: [`write_entry`]'s bytes, opened. What [`write_set`] takes
/// ([`SetSource::Entry`]); a kept one is read from the set ([`SetEntry`]).
#[derive(Clone, Copy, Debug)]
pub struct KeptEntry<'e> {
    asked: AskedIter<'e>,
    columns: [&'e [u8]; COLUMNS],
    /// The token record's bytes, at a multiple of 16 from the entry's start.
    tokens: &'e [u8],
}

/// Where an entry's token record is placed: its blaze-format buffer's alignment.
const TOKENS_ALIGN: usize = 16;

impl<'e> KeptEntry<'e> {
    /// The entry `bytes` hold, its asked atoms checked and its columns and token record cut by
    /// their lengths. The token record is not opened here ([`KeptEntry::tokens`]).
    pub fn open(bytes: &'e [u8]) -> Result<KeptEntry<'e>, KeptError> {
        if bytes.len() < ENTRY_HEAD {
            return Err(KeptError::Short { what: "an entry's head" });
        }
        let (count, len) = (u32_at(bytes, 0), u32_at(bytes, 4));
        let lens_at = ENTRY_HEAD.checked_add(len).ok_or(KeptError::TooLarge)?;
        let tokens_len_at = lens_at.checked_add(COLUMNS * 4).ok_or(KeptError::TooLarge)?;
        let mut at = tokens_len_at.checked_add(4).ok_or(KeptError::TooLarge)?;
        if bytes.len() < at {
            return Err(KeptError::Short { what: "an entry's asked atoms, column lengths or token record length" });
        }
        let asked = AskedIter::checked(&bytes[ENTRY_HEAD..lens_at], count)?;
        let mut columns: [&'e [u8]; COLUMNS] = [&[]; COLUMNS];
        for (column, place) in columns.iter_mut().enumerate() {
            let end = at.checked_add(u32_at(bytes, lens_at + column * 4)).ok_or(KeptError::TooLarge)?;
            if end > bytes.len() {
                return Err(KeptError::Short { what: "an entry's columns" });
            }
            *place = &bytes[at..end];
            at = end;
        }
        let tokens_at = at.checked_next_multiple_of(TOKENS_ALIGN).ok_or(KeptError::TooLarge)?;
        let tokens_end = tokens_at.checked_add(u32_at(bytes, tokens_len_at)).ok_or(KeptError::TooLarge)?;
        if tokens_end > bytes.len() {
            return Err(KeptError::Short { what: "an entry's token record" });
        }
        if bytes[at..tokens_at].iter().any(|&b| b != 0) {
            return Err(KeptError::TokenPad);
        }
        if tokens_end != bytes.len() {
            return Err(KeptError::Trailing);
        }
        Ok(KeptEntry { asked, columns, tokens: &bytes[tokens_at..tokens_end] })
    }

    /// The expansion's token record (D122), opened where it is and checked once by
    /// blaze-tokens. It opens in place when the entry's bytes start 16-aligned (a store's
    /// buffer, `blaze_format::Aligned`); blaze-format refuses it otherwise, by name.
    pub fn tokens(&self) -> Result<Tokens<'e>, OpenError> {
        Tokens::open(self.tokens)
    }

    /// The cfg atoms the item walk asked when this was expanded, with their answers then.
    pub fn asked(&self) -> AskedIter<'e> {
        self.asked
    }

    /// The expansion record's columns, in [`crate::RecordColumn`] order.
    pub(crate) fn columns(&self) -> [&'e [u8]; COLUMNS] {
        self.columns
    }

    /// The expansion, read back and checked once.
    pub fn expansion<'a>(&self, alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        Expansion::read_columns(self.columns, alloc)
    }
}

/// `expansion` with the atoms `asked` as one entry's bytes, in `alloc` (every growth
/// fallible), its token record made with the origin maps `maps` of the call's sources
/// ([`crate::SourceIndexes::of`]`(def, call, alloc).maps()`). Equal values give equal bytes. No
/// record root is built: the set keeps columns.
pub fn write_entry<'b>(asked: &Asked<'_>, expansion: &Expansion<'_>, maps: &OriginMaps<'_>, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, KeptError> {
    let columns = expansion.write_columns(alloc);
    let tokens = expansion.token_record(maps, alloc).map_err(|error| KeptError::TokenRecord { error })?;
    let asked_len = u32::try_from(asked.bytes.len()).map_err(|_| KeptError::TooLarge)?;
    let tokens_len = u32::try_from(tokens.len()).map_err(|_| KeptError::TooLarge)?;
    let mut end = ENTRY_HEAD + asked.bytes.len() + COLUMNS * 4 + 4;
    for column in &columns {
        end = end.checked_add(column.len()).ok_or(KeptError::TooLarge)?;
    }
    let tokens_at = end.checked_next_multiple_of(TOKENS_ALIGN).ok_or(KeptError::TooLarge)?;
    let total = tokens_at.checked_add(tokens.len()).ok_or(KeptError::TooLarge)?;
    let mut out: AVec<'b, u8> = Vec::try_with_capacity_in(total, alloc).map_err(|_| KeptError::OutOfMemory)?;
    out.extend_from_slice(&asked.count.to_le_bytes());
    out.extend_from_slice(&asked_len.to_le_bytes());
    out.extend_from_slice(&asked.bytes);
    for column in &columns {
        let len = u32::try_from(column.len()).map_err(|_| KeptError::TooLarge)?;
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&tokens_len.to_le_bytes());
    for column in &columns {
        out.extend_from_slice(column);
    }
    out.resize(tokens_at, 0);
    out.extend_from_slice(&tokens);
    Ok(out)
}

