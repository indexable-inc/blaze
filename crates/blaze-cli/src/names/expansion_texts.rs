//! The expansion texts section of a `CrateScope` payload (piece 16, "expansion text as a
//! record"): the text each of the crate's macro calls made, read in place by the body checker
//! through front_load.rs `StoreLoad::expansion`.
//!
//! The items record holds, for each call that made a text, a row (call key, the text's digest,
//! its length) and the items placed in it; a body's check that reads such an item asks the
//! loader for the call's text. The texts travel IN the crate's record, beside the rows that
//! name them: whoever holds the record (this home, the machine store, the shared cache) holds
//! its texts, and nothing can find one without the other. The items record's reader checks each
//! served text against its row's digest once ([`blaze_scope::items_record`] `Stale`), so a
//! wrong text is never answered.
//!
//! Why not one record per call under `blaze_expand::call_key` (the first plan): (1) a
//! function-like builtin's call and a block's fragment have no call key, so a table by call key
//! cannot serve every text a row names; (2) the body checker runs on the pool, off the thread
//! that holds the store, so a record a call would be one store read per call, made before the
//! check, for calls the check has not yet named; (3) a separate record is placed by its own
//! name (store.rs `ToolchainCrates::place`), apart from the crate's.
//!
//! Form ([`FORMAT`] 3), little-endian, 16-aligned as a section: count u32, the FORMAT u32, 8 zero
//! bytes, then `count` rows of 48 bytes sorted by call key (call key 32; the text's start u32 from
//! the blob's start and its length u32; its origins' start u32 and length u32, length 0 when the
//! text keeps none), then the blob: the texts in row order, then the origins in row order (the
//! form of `blaze_scope::expansion_origins`: drive.md, "A finding inside an expansion is reported
//! where its tokens came from"). A function of the crate's expansions only.

use std::alloc::Global;

use blaze_id::ItemKey;
use blaze_ids::ContentId;
use blaze_scope::resolve::CrateNames;

/// The fixed head: the count and its padding.
const HEAD: usize = 16;
/// One row: the call key, the text's start and length, the origins' start and length.
const ROW: usize = 48;
/// The section's form: 2 since the origins (0, the head's zero word, before); 3 since a kept
/// item's text keeps its origins (blaze-scope expansion_origins.rs `kept`; before, none): a
/// form-2 section is refused by name and its crate's record rebuilt (rule 26), never migrated.
const FORMAT: u32 = 3;

/// Why the section could not be written or read (rule: names/expansion_texts.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExpansionTextsError {
    /// The crate's texts are more than a u32 can place.
    TooBig { bytes: usize },
    /// The section is shorter than its head or its rows say.
    Short { len: usize, want: usize },
    /// A row's text runs past the section's end.
    Range { call: ItemKey, start: u32, len: u32 },
    /// A row's text is not UTF-8 (a writer's bug or a damaged record).
    Utf8 { call: ItemKey },
    /// A section of another form (0: written before the origins): the record is rebuilt.
    OldForm { found: u32 },
}

impl core::fmt::Display for ExpansionTextsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExpansionTextsError::TooBig { bytes } => write!(f, "a crate's expansion texts of {bytes} bytes do not fit the section's u32 places (rule: names/expansion_texts.rs)"),
            ExpansionTextsError::Short { len, want } => write!(f, "an expansion texts section of {len} bytes is shorter than the {want} its head says (rule: names/expansion_texts.rs)"),
            ExpansionTextsError::Range { call, start, len } => {
                write!(f, "the expansion text of call {call:?} at {start}+{len} runs past its section (rule: names/expansion_texts.rs)")
            }
            ExpansionTextsError::Utf8 { call } => write!(f, "the expansion text of call {call:?} is not UTF-8 (rule: names/expansion_texts.rs)"),
            ExpansionTextsError::OldForm { found } => {
                write!(f, "an expansion texts section of form {found}, not {FORMAT}: the crate's record is rebuilt (rules 26 and names/expansion_texts.rs)")
            }
        }
    }
}

/// The section of `names`: every call whose expansion made a text, with that text, sorted by
/// the call's key (the items record's expansion rows are the same set, in the same order).
/// The list and the section are the CrateScope task's, which keeps its record for the command
/// (`Global`, named, as names.rs `scope_record`'s other sections).
pub(crate) fn write(names: &CrateNames<'_>) -> Result<blaze_own::Vec<u8, Global>, ExpansionTextsError> {
    let mut calls: blaze_own::Vec<(ItemKey, &str, &[u8]), Global> = blaze_own::Vec::new_in(Global);
    // A text whose origins are `Err` (a call's statements read as items, a block's module text,
    // a column that could not be made; a kept item's text keeps its own) keeps none: its reader answers "no origins kept", typed.
    calls.extend(names.expansions.iter().filter_map(|(k, e)| e.made.map(|m| (*k, m.text, m.origins.unwrap_or(&[])))));
    calls.sort_unstable_by_key(|c| c.0);
    let texts: usize = calls.iter().map(|c| c.1.len()).sum();
    let blob: usize = texts + calls.iter().map(|c| c.2.len()).sum::<usize>();
    let too_big = || ExpansionTextsError::TooBig { bytes: blob };
    let count = u32::try_from(calls.len()).map_err(|_| too_big())?;
    u32::try_from(blob).map_err(|_| too_big())?;
    let mut out = blaze_own::Vec::with_capacity_in(HEAD + calls.len() * ROW + blob, Global);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.resize(HEAD, 0);
    // Fits: the blob fits a u32, checked above, and every start and length is within it.
    let (mut start, mut origins_at) = (0u32, texts as u32);
    for (call, text, origins) in calls.iter() {
        let (len, origins_len) = (text.len() as u32, origins.len() as u32);
        out.extend_from_slice(&call.0.0);
        for word in [start, len, origins_at, origins_len] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        start += len;
        origins_at += origins_len;
    }
    for (_, text, _) in calls.iter() {
        out.extend_from_slice(text.as_bytes());
    }
    for (_, _, origins) in calls.iter() {
        out.extend_from_slice(origins);
    }
    Ok(out)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// A section read in place: its rows and texts, checked once against its length.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExpansionTexts<'s> {
    rows: &'s [u8],
    texts: &'s [u8],
}

impl<'s> ExpansionTexts<'s> {
    /// The section `bytes`; an empty section (a crate that made no text) has no rows.
    pub(crate) fn open(bytes: &'s [u8]) -> Result<ExpansionTexts<'s>, ExpansionTextsError> {
        if bytes.is_empty() {
            return Ok(ExpansionTexts { rows: &[], texts: &[] });
        }
        if bytes.len() < HEAD {
            return Err(ExpansionTextsError::Short { len: bytes.len(), want: HEAD });
        }
        let found = u32_at(bytes, 4);
        if found != FORMAT {
            return Err(ExpansionTextsError::OldForm { found });
        }
        let count = u32_at(bytes, 0) as usize;
        let want = count.checked_mul(ROW).and_then(|rows| rows.checked_add(HEAD)).ok_or(ExpansionTextsError::Short { len: bytes.len(), want: usize::MAX })?;
        if bytes.len() < want {
            return Err(ExpansionTextsError::Short { len: bytes.len(), want });
        }
        Ok(ExpansionTexts { rows: &bytes[HEAD..want], texts: &bytes[want..] })
    }

    /// How many calls made a text.
    pub(crate) fn len(&self) -> usize {
        self.rows.len() / ROW
    }

    fn call_at(&self, i: usize) -> ItemKey {
        let mut key = [0u8; 32];
        key.copy_from_slice(&self.rows[i * ROW..i * ROW + 32]);
        ItemKey(ContentId(key))
    }

    /// The row of `call`, by binary search over the sorted keys.
    fn row_of(&self, call: ItemKey) -> Option<usize> {
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

    /// The origins (`blaze_scope::expansion_origins` form) of the text the call `call` made:
    /// `None` when the record holds no text for it, `Some(&[])` when the text keeps no origins.
    pub(crate) fn origins(&self, call: ItemKey) -> Result<Option<&'s [u8]>, ExpansionTextsError> {
        let Some(i) = self.row_of(call) else { return Ok(None) };
        let (start, len) = (u32_at(self.rows, i * ROW + 40), u32_at(self.rows, i * ROW + 44));
        let range = (start as usize).checked_add(len as usize).filter(|end| *end <= self.texts.len()).map(|end| start as usize..end);
        range.map(|r| Some(&self.texts[r])).ok_or(ExpansionTextsError::Range { call, start, len })
    }

    /// The text the call `call` made; `None` when the crate's record holds none for it.
    pub(crate) fn text(&self, call: ItemKey) -> Result<Option<&'s str>, ExpansionTextsError> {
        let (mut low, mut high) = (0usize, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.call_at(mid).cmp(&call) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => {
                    let (start, len) = (u32_at(self.rows, mid * ROW + 32), u32_at(self.rows, mid * ROW + 36));
                    let range = (start as usize).checked_add(len as usize).filter(|end| *end <= self.texts.len()).map(|end| start as usize..end);
                    let bytes = range.map(|r| &self.texts[r]).ok_or(ExpansionTextsError::Range { call, start, len })?;
                    return core::str::from_utf8(bytes).map(Some).map_err(|_| ExpansionTextsError::Utf8 { call });
                }
            }
        }
        Ok(None)
    }
}
