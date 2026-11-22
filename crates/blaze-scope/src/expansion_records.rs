//! Macro expansions as their own records, keyed by content (runtime.md, plan row B3; "The end
//! state" 4: "expansions as their own records by content").
//!
//! A crate's `CrateScope` record held every text its macro calls made, with their origins, in
//! one section: 48 MB of the core library's 83 MB. Resolving names does not read that text once
//! the expansion is outlined and its items declared; only a body a macro wrote, or a read of
//! such an item's text, does, and for a dependency whose bodies are never checked almost none
//! is. So the text leaves the crate's record:
//!
//! - An EXPANSION RECORD ([`ExpansionRecord`]) holds one expansion's text and the COLUMN of its
//!   origins (each rendered token's place in the text and where it came from, input tokens by
//!   index: blaze-expand origin_column.rs), under the digest of its own bytes ([`record_key`]).
//!   Both are functions of the expansion alone (the macro's definition, the call's input tokens,
//!   their marks and edition), never of where the call stands (rules 21, 25, 71), so two calls of
//!   equal input (`impl_int!` for one type in two places, `debug_assert!` with equal tokens)
//!   make one record. The key is the record's content, not its inputs: the record is never
//!   looked up before the expansion is made (the names task expands, then files it), and the
//!   cell a reader of the text records is `NameCell::Expansion`, whose digest covers the items
//!   row's text id and the directory row's record key (names_cells.rs `expansion_digest`): the
//!   text and the column the record holds, so a reader is entered when either moves.
//! - The crate's DIRECTORY ([`Directory`]), the section the texts were in, holds a row a call:
//!   its key, its record's key, the text's length, the column's length and the CALL PART of its
//!   origins (the calling text, the entry, where the input starts, the macro's path: blaze-scope
//!   expansion_origins.rs, everything before the column). The items the expansion declared are
//!   the items record's rows already; the directory adds none.
//!
//! Form of a record ([`RECORD_FORM`] 1), little-endian: the form u32, the text's length u32, the
//! column's length u32, a zero u32, the text, zeros to a multiple of 4, the column. Positions in
//! the column are relative to the text (rule 71).
//!
//! Form of the directory ([`DIRECTORY_FORM`] 4, after the texts section's 3: blaze-cli
//! names/expansion_texts.rs), little-endian, 16-aligned as a section: the count u32, the form
//! u32, the count of distinct records u32, a zero u32; then `count` rows of [`DIRECTORY_ROW`]
//! bytes sorted by call key (call key 32, record key 32, text length u32, column length u32, the
//! call part's start u32 from the blob's start and its length u32: 0 when the text keeps no
//! origins); then the blob, the call parts in row order. A function of the crate's expansions
//! only. Sans-IO: the caller puts the records and reads them back.
#![blaze::sans_io]

use blaze_id::{ContentId, ItemKey};
use blaze_ids::{Domain, Hasher};
use blaze_own::{AVec, Alloc};

use crate::expansion_origins::{OriginsError, OriginsView, split};
use crate::resolve::CrateNames;

/// The domain an expansion record's key is the digest of its bytes in. Its number moves with
/// [`RECORD_FORM`].
pub const RECORD_DOMAIN: &str = "blaze-scope expansion record 1";
static RECORDS: Domain = Domain::new(RECORD_DOMAIN);

/// An expansion record's form.
pub const RECORD_FORM: u32 = 1;
/// An expansion record's head: form, text length, column length, zero.
pub const RECORD_HEAD: usize = 16;

/// The directory's form: 4 follows the expansion texts section's 3 (which held the texts), so a
/// record of the old form is refused by name and its crate's record rebuilt (rule 26).
pub const DIRECTORY_FORM: u32 = 4;
const DIRECTORY_HEAD: usize = 16;
/// One directory row: call key, record key, text length, column length, call part's start and
/// length.
pub const DIRECTORY_ROW: usize = 32 + 32 + 4 * 4;

/// Why expansion records or a directory could not be written or read (rule: blaze-scope
/// expansion_records.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpansionRecordsError {
    /// More than a u32 can count or place (`what`: the calls, a text, a column, the blob).
    TooBig { what: &'static str, bytes: usize },
    /// A call's origins do not cut into a call part and a column: a writer's bug.
    Origins { call: ItemKey, error: OriginsError },
    /// Two expansions of one record key with other bytes (rule 21: the fix is the key).
    TwoValues { key: ContentId, first: ItemKey, second: ItemKey },
    /// Bytes shorter than their head or rows say.
    Short { what: &'static str, len: usize, want: usize },
    /// Another form than this reader's: the record is rebuilt (rule 26), never migrated.
    OldForm { what: &'static str, found: u32 },
    /// A row's call part runs past the blob.
    Range { call: ItemKey, start: u32, len: u32 },
    /// A record's text is not UTF-8 (a writer's bug or a damaged record).
    Utf8,
    /// A record whose text or column is not as long as the directory's row says: the record
    /// served for `call` is not the one its row names.
    NotItsRecord { call: ItemKey, key: ContentId },
    /// No record for the call's row: what a reader names instead of expanding again (rule 3).
    Missing { call: ItemKey, key: ContentId },
}

impl core::fmt::Display for ExpansionRecordsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const RULE: &str = "(rule: blaze-scope expansion_records.rs)";
        match self {
            ExpansionRecordsError::TooBig { what, bytes } => write!(f, "{what} of {bytes} do not fit a u32 {RULE}"),
            ExpansionRecordsError::Origins { call, error } => write!(f, "the origins of macro call {call:?} do not cut into a call part and a column: {error} {RULE}"),
            ExpansionRecordsError::TwoValues { key, first, second } => {
                write!(f, "macro calls {first:?} and {second:?} made other expansions under one record key {key:?} {RULE}")
            }
            ExpansionRecordsError::Short { what, len, want } => write!(f, "{what} of {len} bytes is shorter than the {want} its head says {RULE}"),
            ExpansionRecordsError::OldForm { what, found } => write!(f, "{what} of form {found}: the crate's record is rebuilt (rule 26) {RULE}"),
            ExpansionRecordsError::Range { call, start, len } => write!(f, "the call part of macro call {call:?} at {start}+{len} runs past its directory {RULE}"),
            ExpansionRecordsError::Utf8 => write!(f, "an expansion record's text is not UTF-8 {RULE}"),
            ExpansionRecordsError::NotItsRecord { call, key } => write!(f, "the expansion record {key:?} served for macro call {call:?} is not the one its directory row names {RULE}"),
            ExpansionRecordsError::Missing { call, key } => write!(f, "the store holds no expansion record {key:?} for macro call {call:?} {RULE}"),
        }
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

fn fits(n: usize, what: &'static str) -> Result<u32, ExpansionRecordsError> {
    u32::try_from(n).map_err(|_| ExpansionRecordsError::TooBig { what, bytes: n })
}

/// The head of the record of `text` with `column`.
fn head(text: &str, column: &[u8]) -> Result<[u8; RECORD_HEAD], ExpansionRecordsError> {
    let mut head = [0u8; RECORD_HEAD];
    head[..4].copy_from_slice(&RECORD_FORM.to_le_bytes());
    head[4..8].copy_from_slice(&fits(text.len(), "an expansion's text")?.to_le_bytes());
    head[8..12].copy_from_slice(&fits(column.len(), "an expansion's column")?.to_le_bytes());
    Ok(head)
}

const ZEROS: [u8; 3] = [0; 3];

/// The key of the record of `text` with `column`: the digest of its bytes in [`RECORD_DOMAIN`],
/// hashed from its parts, never joined; equal to [`record_key`] of [`ExpansionRecord::write`]'s
/// bytes.
pub fn key_of(text: &str, column: &[u8]) -> Result<ContentId, ExpansionRecordsError> {
    let head = head(text, column)?;
    let mut h = Hasher::of(&RECORDS);
    h.update(&head).update(text.as_bytes()).update(&ZEROS[..pad4(text.len()) - text.len()]).update(column);
    Ok(h.finish())
}

/// The key of the record `bytes`: the digest of its bytes.
pub fn record_key(bytes: &[u8]) -> ContentId {
    ContentId::of_domain(&RECORDS, bytes)
}

/// One expansion record, read where it lies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExpansionRecord<'s> {
    pub text: &'s str,
    /// The origin column (blaze-expand `OriginsRef::open`'s form); empty when the text keeps no
    /// origins.
    pub column: &'s [u8],
}

impl<'s> ExpansionRecord<'s> {
    /// The record of `text` with `column`, appended to `out`.
    pub fn write(text: &str, column: &[u8], out: &mut AVec<'_, u8>) -> Result<(), ExpansionRecordsError> {
        let head = head(text, column)?;
        out.reserve(RECORD_HEAD + pad4(text.len()) + column.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(&ZEROS[..pad4(text.len()) - text.len()]);
        out.extend_from_slice(column);
        Ok(())
    }

    /// The record `bytes`, its head checked against its length. Its digest is not checked here:
    /// the store proved its bytes are the ones filed under its key (rule 74), and the items
    /// record checks the text against its row's digest at its first read.
    pub fn open(bytes: &'s [u8]) -> Result<ExpansionRecord<'s>, ExpansionRecordsError> {
        if bytes.len() < RECORD_HEAD {
            return Err(ExpansionRecordsError::Short { what: "an expansion record", len: bytes.len(), want: RECORD_HEAD });
        }
        let found = u32_at(bytes, 0);
        if found != RECORD_FORM {
            return Err(ExpansionRecordsError::OldForm { what: "an expansion record", found });
        }
        let (text_len, column_len) = (u32_at(bytes, 4) as usize, u32_at(bytes, 8) as usize);
        let column_at = RECORD_HEAD + pad4(text_len);
        let want = column_at + column_len;
        if bytes.len() != want {
            return Err(ExpansionRecordsError::Short { what: "an expansion record", len: bytes.len(), want });
        }
        let text = core::str::from_utf8(&bytes[RECORD_HEAD..RECORD_HEAD + text_len]).map_err(|_| ExpansionRecordsError::Utf8)?;
        Ok(ExpansionRecord { text, column: &bytes[column_at..] })
    }
}

/// One record a crate's calls name, to put under its key.
#[derive(Debug)]
pub struct Record<'a> {
    pub key: ContentId,
    pub bytes: AVec<'a, u8>,
}

/// A crate's expansions split: its directory (the section of its record) and each distinct
/// record its calls name, sorted by key, each once.
#[derive(Debug)]
pub struct Split<'a> {
    pub directory: AVec<'a, u8>,
    pub records: AVec<'a, Record<'a>>,
}

/// One call that made a text, as the writer takes it.
struct Call<'n> {
    call: ItemKey,
    text: &'n str,
    call_part: &'n [u8],
    column: &'n [u8],
    key: ContentId,
}

/// The directory and the records of `names`' expansions, in `alloc`: every call whose expansion
/// made a text (the items record's expansion rows are the same set, in the same order). A text
/// whose origins are `Err` (a call's statements read as items, a block's module text, a column
/// that could not be made) keeps none: its row's call part and its record's column are empty,
/// and its reader answers "no origins kept", typed, as before.
pub fn write<'a>(names: &CrateNames<'_>, alloc: Alloc<'a>) -> Result<Split<'a>, ExpansionRecordsError> {
    let mut calls: AVec<'_, Call<'_>> = AVec::new_in(alloc);
    for (call, expansion) in names.expansions.iter() {
        let Some(made) = expansion.made else { continue };
        let (call_part, column) = match made.origins {
            Ok(origins) => split(origins).map_err(|error| ExpansionRecordsError::Origins { call: *call, error })?,
            Err(_) => (&[][..], &[][..]),
        };
        // The key is set by `fill_keys` below, for every call, before any is read.
        calls.push(Call { call: *call, text: made.text, call_part, column, key: ContentId::default() });
    }
    fill_keys(&mut calls, alloc)?;
    // Sorted by call key: the map's order is the machine's (rule 25).
    calls.sort_unstable_by_key(|c| c.call);
    let count = fits(calls.len(), "a crate's macro calls")?;

    // Each distinct record once, sorted by key; two calls under one key must have made one
    // expansion (rule 21).
    let mut by_key: AVec<'_, (ContentId, usize)> = AVec::with_capacity_in(calls.len(), alloc);
    by_key.extend(calls.iter().enumerate().map(|(i, c)| (c.key, i)));
    by_key.sort_unstable();
    let mut records: AVec<'a, Record<'a>> = AVec::new_in(alloc);
    let mut last: Option<(ContentId, usize)> = None;
    for &(key, i) in by_key.iter() {
        if let Some((_, first)) = last.filter(|(held, _)| *held == key) {
            let (a, b) = (&calls[first], &calls[i]);
            if a.text != b.text || a.column != b.column {
                return Err(ExpansionRecordsError::TwoValues { key, first: a.call, second: b.call });
            }
            continue;
        }
        let mut bytes = AVec::new_in(alloc);
        ExpansionRecord::write(calls[i].text, calls[i].column, &mut bytes)?;
        records.push(Record { key, bytes });
        last = Some((key, i));
    }

    let blob: usize = calls.iter().map(|c| c.call_part.len()).sum();
    fits(blob, "a crate's call parts")?;
    let mut directory = AVec::with_capacity_in(DIRECTORY_HEAD + calls.len() * DIRECTORY_ROW + blob, alloc);
    directory.extend_from_slice(&count.to_le_bytes());
    directory.extend_from_slice(&DIRECTORY_FORM.to_le_bytes());
    directory.extend_from_slice(&fits(records.len(), "a crate's expansion records")?.to_le_bytes());
    directory.resize(DIRECTORY_HEAD, 0);
    // Fits: the blob fits a u32, checked above, and every start is within it; each text's and
    // column's length was checked by `key_of`.
    let mut at = 0u32;
    for c in calls.iter() {
        directory.extend_from_slice(&c.call.0.0);
        directory.extend_from_slice(&c.key.0);
        for word in [c.text.len() as u32, c.column.len() as u32, at, c.call_part.len() as u32] {
            directory.extend_from_slice(&word.to_le_bytes());
        }
        at += c.call_part.len() as u32;
    }
    for c in calls.iter() {
        directory.extend_from_slice(c.call_part);
    }
    Ok(Split { directory, records })
}

/// The records [`fill_keys`] hashes side by side at once: blaze-blake3's batch.
const KEY_BATCH: usize = 64;

/// The longest record whose key is hashed beside others' (a BLAKE3 chunk, one SIMD lane); a
/// longer one is hashed alone either way, by [`key_of`] from its parts.
const SHORT_RECORD: usize = blaze_ids::b3::CHUNK_LEN;

/// Each call's key, the [`key_of`] its text and column. A record of at most [`SHORT_RECORD`]
/// bytes is written into one reused buffer as [`ExpansionRecord::write`] writes it, and
/// [`KEY_BATCH`] of them are hashed side by side ([`ContentId::of_each_domain`]); a longer one
/// goes through [`key_of`], streamed from its parts with no copy. Both digest the record's bytes
/// in [`RECORDS`] (`key_of`'s parts are those bytes in order, and BLAKE3 hashes a stream by its
/// bytes, not by its updates), so each key is the one `key_of` gives
/// (`batched_keys_are_key_of`).
fn fill_keys(calls: &mut [Call<'_>], alloc: Alloc<'_>) -> Result<(), ExpansionRecordsError> {
    let mut scratch: AVec<'_, u8> = AVec::new_in(alloc);
    let mut members = [0usize; KEY_BATCH];
    // Where each waiting record ends in `scratch`; the first starts at 0.
    let mut ends = [0usize; KEY_BATCH + 1];
    let mut count = 0;
    for at in 0..calls.len() {
        let (text, column) = (calls[at].text, calls[at].column);
        if RECORD_HEAD + pad4(text.len()) + column.len() > SHORT_RECORD {
            calls[at].key = key_of(text, column)?;
            continue;
        }
        ExpansionRecord::write(text, column, &mut scratch)?;
        members[count] = at;
        count += 1;
        ends[count] = scratch.len();
        if count == KEY_BATCH {
            hash_keys(calls, &members[..count], &ends[..=count], &scratch);
            scratch.clear();
            count = 0;
        }
    }
    hash_keys(calls, &members[..count], &ends[..=count], &scratch);
    Ok(())
}

/// The keys of the records `scratch[ends[i]..ends[i + 1]]`, side by side, into the calls
/// `members[i]`.
fn hash_keys(calls: &mut [Call<'_>], members: &[usize], ends: &[usize], scratch: &[u8]) {
    let count = members.len();
    let mut inputs: [&[u8]; KEY_BATCH] = [&[]; KEY_BATCH];
    for (at, input) in inputs[..count].iter_mut().enumerate() {
        *input = &scratch[ends[at]..ends[at + 1]];
    }
    let mut keys = [ContentId::default(); KEY_BATCH];
    ContentId::of_each_domain(&RECORDS, &inputs[..count], &mut keys[..count])
        .expect("as many key slots as records: both are `members.len()`");
    for (&member, key) in members.iter().zip(keys) {
        calls[member].key = key;
    }
}

/// One call's row of a directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'s> {
    pub call: ItemKey,
    /// The key of the record that holds its text and column.
    pub record: ContentId,
    pub text_len: u32,
    pub column_len: u32,
    /// Its origins' call part ([`split`]); empty when the text keeps no origins.
    pub call_part: &'s [u8],
}

impl<'s> Entry<'s> {
    /// The text of `record`, the record served for this row: refused when it is not the one
    /// the row names by its lengths ([`ExpansionRecordsError::NotItsRecord`]; the items record
    /// checks the text's digest at its first read).
    pub fn text<'r>(&self, record: &ExpansionRecord<'r>) -> Result<&'r str, ExpansionRecordsError> {
        self.check(record)?;
        Ok(record.text)
    }

    /// The origins of this call's text, read where they lie (the call part in the directory,
    /// the column in the record): `None` when the text keeps none.
    pub fn origins<'r>(&self, record: &ExpansionRecord<'r>) -> Result<Option<OriginsView<'r>>, ExpansionRecordsError>
    where
        's: 'r,
    {
        self.check(record)?;
        if self.call_part.is_empty() {
            return Ok(None);
        }
        OriginsView::open_parts(self.call_part, record.column).map(Some).map_err(|error| ExpansionRecordsError::Origins { call: self.call, error })
    }

    fn check(&self, record: &ExpansionRecord<'_>) -> Result<(), ExpansionRecordsError> {
        if record.text.len() != self.text_len as usize || record.column.len() != self.column_len as usize {
            return Err(ExpansionRecordsError::NotItsRecord { call: self.call, key: self.record });
        }
        Ok(())
    }
}

/// A crate's directory read in place, its head checked once against its length.
#[derive(Clone, Copy, Debug)]
pub struct Directory<'s> {
    rows: &'s [u8],
    blob: &'s [u8],
    records: u32,
}

impl<'s> Directory<'s> {
    /// The directory `bytes`; an empty section (a crate that made no text) has no rows.
    pub fn open(bytes: &'s [u8]) -> Result<Directory<'s>, ExpansionRecordsError> {
        if bytes.is_empty() {
            return Ok(Directory { rows: &[], blob: &[], records: 0 });
        }
        if bytes.len() < DIRECTORY_HEAD {
            return Err(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want: DIRECTORY_HEAD });
        }
        let found = u32_at(bytes, 4);
        if found != DIRECTORY_FORM {
            return Err(ExpansionRecordsError::OldForm { what: "an expansions directory", found });
        }
        let count = u32_at(bytes, 0) as usize;
        let want = count
            .checked_mul(DIRECTORY_ROW)
            .and_then(|rows| rows.checked_add(DIRECTORY_HEAD))
            .ok_or(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want: usize::MAX })?;
        if bytes.len() < want {
            return Err(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want });
        }
        Ok(Directory { rows: &bytes[DIRECTORY_HEAD..want], blob: &bytes[want..], records: u32_at(bytes, 8) })
    }

    /// How many calls made a text.
    pub fn len(&self) -> usize {
        self.rows.len() / DIRECTORY_ROW
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// How many distinct records the calls name.
    pub fn records(&self) -> u32 {
        self.records
    }

    fn call_at(&self, i: usize) -> ItemKey {
        let mut key = [0u8; 32];
        key.copy_from_slice(&self.rows[i * DIRECTORY_ROW..i * DIRECTORY_ROW + 32]);
        ItemKey(ContentId(key))
    }

    /// Row `i`, in call-key order.
    pub fn entry_at(&self, i: usize) -> Result<Entry<'s>, ExpansionRecordsError> {
        let r = i * DIRECTORY_ROW;
        let call = self.call_at(i);
        let mut record = [0u8; 32];
        record.copy_from_slice(&self.rows[r + 32..r + 64]);
        let (text_len, column_len) = (u32_at(self.rows, r + 64), u32_at(self.rows, r + 68));
        let (start, len) = (u32_at(self.rows, r + 72), u32_at(self.rows, r + 76));
        let range = (start as usize).checked_add(len as usize).filter(|end| *end <= self.blob.len()).map(|end| start as usize..end);
        let call_part = range.map(|range| &self.blob[range]).ok_or(ExpansionRecordsError::Range { call, start, len })?;
        Ok(Entry { call, record: ContentId(record), text_len, column_len, call_part })
    }

    /// The row of `call`, by binary search over the sorted keys; `None` when the call made no
    /// text.
    pub fn entry(&self, call: ItemKey) -> Result<Option<Entry<'s>>, ExpansionRecordsError> {
        match self.position(call) {
            Some(at) => self.entry_at(at).map(Some),
            None => Ok(None),
        }
    }

    /// The row index of `call`, by binary search over the sorted keys: what a reader that keeps
    /// one served record per row indexes by (blaze-cli names/expansion_texts.rs); `None` when
    /// the call made no text.
    pub fn position(&self, call: ItemKey) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.call_at(mid).cmp(&call) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{string::String, vec::Vec};

    use blaze_sim::{Config, Tape};

    use super::*;

    /// A length near where a record stops being short, or any up to past a chunk.
    fn length(tape: &Tape, label: &'static str) -> usize {
        match tape.coin(label) {
            true => tape.index(1100, label),
            false => SHORT_RECORD - RECORD_HEAD - 8 + tape.index(16, label),
        }
    }

    /// Up to 200 drawn texts (ASCII) and columns.
    fn drawn(tape: &Tape) -> Vec<(String, Vec<u8>)> {
        let count = tape.index(if cfg!(miri) { 20 } else { 201 }, "calls");
        (0..count)
            .map(|_| {
                let mut text = std::vec![0u8; length(tape, "text")];
                tape.fill(&mut text, "text bytes");
                let text: Vec<u8> = text.iter().map(|b| b'a' + b % 26).collect();
                let mut column = std::vec![0u8; length(tape, "column") % 600];
                tape.fill(&mut column, "column bytes");
                (String::from_utf8(text).expect("ASCII"), column)
            })
            .collect()
    }

    /// Every key [`fill_keys`] sets, side by side or alone, is [`key_of`] of the call's text and
    /// column and [`record_key`] of the record [`ExpansionRecord::write`] makes of them.
    #[test]
    fn batched_keys_are_key_of() {
        blaze_sim::check(Config::cases(if cfg!(miri) { 4 } else { 128 }), |tape| {
            let alloc = blaze_own::testing::heap().alloc();
            let datas = drawn(tape);
            let mut calls: Vec<Call<'_>> = datas
                .iter()
                .enumerate()
                .map(|(at, (text, column))| {
                    let mut call = [0u8; 32];
                    call[..8].copy_from_slice(&u64::try_from(at).expect("a count fits u64").to_le_bytes());
                    Call { call: ItemKey(ContentId(call)), text, call_part: &[], column, key: ContentId::default() }
                })
                .collect();
            fill_keys(&mut calls, alloc).expect("drawn records fit a u32");
            for (at, call) in calls.iter().enumerate() {
                let alone = key_of(call.text, call.column).expect("drawn records fit a u32");
                let mut bytes = AVec::new_in(alloc);
                ExpansionRecord::write(call.text, call.column, &mut bytes).expect("drawn records fit a u32");
                assert_eq!(call.key, alone, "call {at}: {} B of text, {} B of column", call.text.len(), call.column.len());
                assert_eq!(call.key, record_key(&bytes), "call {at}: the record's own digest");
            }
        });
    }

    /// Records whose bytes end a byte either side of 1, 2, 3, 4, 8 and 16 chunks (and on them),
    /// in a drawn order among short ones, so the batch, the streamed [`key_of`] (four updates
    /// over a lazily merged stack) and the one-shot [`record_key`] each meet every tree shape
    /// up to 16 chunks: all three keys equal.
    #[test]
    fn keys_across_chunk_edges_are_key_of() {
        let mut shapes: Vec<(usize, usize)> = Vec::new();
        for chunks in [1usize, 2, 3, 4, 8, 16] {
            for delta in -5i64..=5 {
                for column in [0usize, 1, 3, 70, 600] {
                    let target = usize::try_from(i64::try_from(chunks * 1024).expect("small") + delta).expect("positive");
                    let room = target - RECORD_HEAD - column;
                    // A text of a multiple of four bytes, the column taking the rest, so the
                    // record is `target` bytes exactly.
                    let text = room / 4 * 4;
                    shapes.push((text, column + room - text));
                }
            }
        }
        for short in [0usize, 5, 64, 400] {
            shapes.push((short, short % 9));
        }
        blaze_sim::check(Config::cases(if cfg!(miri) { 2 } else { 16 }), |tape| {
            let alloc = blaze_own::testing::heap().alloc();
            let mut order = shapes.clone();
            if cfg!(miri) {
                order.retain(|&(text, column)| RECORD_HEAD + text + column <= 3 * 1024);
            }
            tape.shuffle(&mut order, "order");
            let datas: Vec<(String, Vec<u8>)> = order
                .iter()
                .map(|&(text, column)| {
                    let text: String = (0..text).map(|at| char::from(b'a' + u8::try_from(at % 26).expect("small"))).collect();
                    let column: Vec<u8> = (0..column).map(|at| u8::try_from(at % 251).expect("small")).collect();
                    (text, column)
                })
                .collect();
            let mut calls: Vec<Call<'_>> = datas
                .iter()
                .enumerate()
                .map(|(at, (text, column))| {
                    let mut call = [0u8; 32];
                    call[..8].copy_from_slice(&u64::try_from(at).expect("a count fits u64").to_le_bytes());
                    Call { call: ItemKey(ContentId(call)), text, call_part: &[], column, key: ContentId::default() }
                })
                .collect();
            fill_keys(&mut calls, alloc).expect("records fit a u32");
            for call in &calls {
                let alone = key_of(call.text, call.column).expect("records fit a u32");
                let mut bytes = AVec::new_in(alloc);
                ExpansionRecord::write(call.text, call.column, &mut bytes).expect("records fit a u32");
                let what = std::format!("{} B record, {} B of text, {} B of column", bytes.len(), call.text.len(), call.column.len());
                assert_eq!(call.key, alone, "{what}: batched against key_of");
                assert_eq!(alone, record_key(&bytes), "{what}: key_of against the record's one-shot digest");
            }
        });
    }
}
