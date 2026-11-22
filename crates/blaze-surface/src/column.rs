//! What every record of this crate is written and read with: a column's place, a record's
//! string table, offsets in a buffer, and the checks a view makes once when it is read.

use core::ops::Range;

use blaze_format::{FormatError, Record, Slice, Writer};
use blaze_own::{AVec, Alloc};

use crate::error::BuildError;

/// A finished record's bytes, as blaze-format's [`Writer::finish`] hands them back: its
/// `std` vector until blaze-format moves onto blaze-own, the one place this crate names `std`'s
/// collections outside a `record!` expansion.
pub type RecordBytes<'a> = std::vec::Vec<u8, Alloc<'a>>;

/// No row: a parent at the top, an item index not given.
pub(crate) const NONE: u32 = u32::MAX;

blaze_format::record! {
    /// Text in a record's string table: `len` bytes from byte `at`, a whole `str` that was
    /// pushed, so it starts and ends on a character's boundary.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct Text: 1 {
        pub(crate) at: u32,
        pub(crate) len: u32,
    }
}

impl Text {
    pub(crate) fn range(self) -> Range<usize> {
        wide(self.at)..wide(self.at) + wide(self.len)
    }
}

/// `value` as a `usize`: every target blaze runs on has a `usize` of 64 bits.
pub(crate) fn wide(value: u32) -> usize {
    usize::try_from(value).expect("a u32 fits a usize on every target blaze runs on")
}

/// How far `bytes`, a part of `buffer`, are from its start: what a [`FormatError`] names.
pub(crate) fn at_of(bytes: &[u8], buffer: &[u8]) -> u64 {
    let at = bytes.as_ptr().addr().wrapping_sub(buffer.as_ptr().addr());
    u64::try_from(at).expect("a byte offset fits a u64 on every target blaze runs on")
}

/// [`at_of`] of a record read from `buffer`.
pub(crate) fn at_of_record<T: Record>(record: &T, buffer: &[u8]) -> u64 {
    at_of(blaze_format::bytes_of_slice(core::slice::from_ref(record)), buffer)
}

/// Where the field `offset` bytes into a record written at `at` is.
pub(crate) fn field_at(at: u64, offset: usize) -> u64 {
    at + u64::try_from(offset).expect("a field's offset fits a u64")
}

/// A count of `table`'s rows as the `u32` a record holds, refused past `u32::MAX`.
pub(crate) fn count(len: usize, table: &'static str) -> Result<u32, BuildError> {
    u32::try_from(len).map_err(|_| BuildError::TooLarge { table })
}

/// Where a column was written: nowhere when it is empty. Over the writer's allocator, which
/// blaze-format's `Placed` is not.
#[derive(Clone, Copy)]
pub(crate) struct Column {
    at: u64,
    len: usize,
}

impl Column {
    /// Writes `values` as a column; an empty one writes nothing.
    pub(crate) fn push<T: Record>(out: &mut Writer<Alloc<'_>>, values: &[T]) -> Column {
        match values.is_empty() {
            true => Column { at: 0, len: 0 },
            false => Column { at: out.push_slice(values), len: values.len() },
        }
    }

    /// The slice of this column for the field `offset` bytes into a record written at `at`.
    pub(crate) fn slice<T>(self, at: u64, offset: usize) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(field_at(at, offset), self.at, len),
        }
    }
}

/// Pushes `root` as the last record of `out` and finishes the buffer with it as its root.
/// `make` builds the root from where it will be written, so its slices can be made.
pub(crate) fn finish_with<'a, R: Record>(
    mut out: Writer<Alloc<'a>>,
    make: impl FnOnce(u64) -> R,
) -> RecordBytes<'a> {
    let at = out.next::<R>();
    let root = make(at);
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    out.finish(at)
}

/// A string table while it is built: each pushed text is appended whole, so the table is UTF-8
/// and every [`Text`] of it is on character boundaries.
///
/// A builder pushes each name into a staging table as it is given, and at its end into the
/// record's table in the record's own order ([`Strings::moved`]): two builders given the same
/// rows in another order write equal bytes (rule 25).
pub(crate) struct Strings<'a> {
    bytes: AVec<'a, u8>,
}

impl<'a> Strings<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Strings<'a> {
        Strings { bytes: AVec::new_in(alloc) }
    }

    /// Appends `text`, refused when the table would pass `u32::MAX` bytes.
    pub(crate) fn push(&mut self, text: &str, table: &'static str) -> Result<Text, BuildError> {
        let at = count(self.bytes.len(), table)?;
        let len = count(text.len(), table)?;
        at.checked_add(len).ok_or(BuildError::TooLarge { table })?;
        self.bytes.extend_from_slice(text.as_bytes());
        Ok(Text { at, len })
    }

    /// The text `text` names, a [`Text`] this table gave.
    pub(crate) fn of(&self, text: Text) -> &str {
        core::str::from_utf8(&self.bytes[text.range()])
            .expect("a table of whole pushed strs cut at their own bounds")
    }

    /// Appends the text `text` names in `staging` to this table.
    pub(crate) fn moved(
        &mut self,
        staging: &Strings<'_>,
        text: Text,
        table: &'static str,
    ) -> Result<Text, BuildError> {
        self.push(staging.of(text), table)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// The text `text` names in `strings`, a string table read from a buffer, or `None` when it
/// runs past the table or cuts a character.
pub(crate) fn text_in(strings: &str, text: Text) -> Option<&str> {
    let start = usize::try_from(text.at).ok()?;
    let end = start.checked_add(usize::try_from(text.len).ok()?)?;
    strings.get(start..end)
}

/// The text `text` names in `strings`, which the view's `read` checked with [`check_text`].
pub(crate) fn text_of(strings: &str, text: Text) -> &str {
    text_in(strings, text).expect("every name of a record is checked when its view is read")
}

/// [`text_in`], refused as a [`FormatError`] naming the row it is in.
pub(crate) fn check_text<T: Record>(
    strings: &str,
    text: Text,
    row: &T,
    buffer: &[u8],
) -> Result<(), FormatError> {
    match text_in(strings, text) {
        Some(_) => Ok(()),
        None => Err(FormatError::Invalid {
            at: at_of_record(row, buffer),
            what: "a name past its record's string table or inside a character",
        }),
    }
}

/// Whether `rows` are in strictly increasing order of `key`: what a view that probes them by
/// binary search checks once when it is read.
pub(crate) fn check_increasing<T: Record, K: Ord>(
    rows: &[T],
    buffer: &[u8],
    what: &'static str,
    key: impl Fn(&T) -> K,
) -> Result<(), FormatError> {
    match rows.windows(2).find(|pair| key(&pair[0]) >= key(&pair[1])) {
        None => Ok(()),
        Some(pair) => Err(FormatError::Invalid { at: at_of_record(&pair[1], buffer), what }),
    }
}

/// `index` as an option: [`NONE`] is none.
pub(crate) fn index_of(index: u32) -> Option<crate::ItemIndex> {
    match index {
        NONE => None,
        index => Some(crate::ItemIndex(index)),
    }
}

/// An optional index as a row holds it.
pub(crate) fn index_word(index: Option<crate::ItemIndex>) -> u32 {
    index.map_or(NONE, |index| index.0)
}

/// A key that may be absent, as a row holds it: all zeros is none. No item key is zero: its
/// first 8 bytes are its crate's key's, a BLAKE3 hash.
pub(crate) fn key_word(key: Option<blaze_id::ItemKey>) -> blaze_id::ContentId {
    key.map_or(blaze_id::ContentId([0; 32]), |key| key.0)
}

/// What [`key_word`] wrote.
pub(crate) fn key_of_word(word: blaze_id::ContentId) -> Option<blaze_id::ItemKey> {
    match word.0 == [0; 32] {
        true => None,
        false => Some(blaze_id::ItemKey(word)),
    }
}
