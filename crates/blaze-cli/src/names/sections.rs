//! The form of a `CrateScope` record's payload (drive.md, "End state", piece 3, NAMES AND ITEMS
//! FILED): one head, then eight sections, each found by its place in the head.
//!
//! - `exports`: the exports record, what a dependent reads and keys on (the task's out is its
//!   digest alone);
//! - `inner`: the inner record, the crate's own;
//! - `names`: the names record ([`blaze_scope::names_record::encode`]), read through
//!   `RecordNames`: a path in the crate answered without solving it;
//! - `items`: the items record ([`blaze_scope::items_record::encode`]), read through
//!   `RecordItems`: an item's row (kind, module, text place) without its crate's tables;
//! - `tests`: a test unit's tests record ([`blaze_scope::tests_record::encode`], test.md "The
//!   tests record", piece 6), read through `TestsRecord::open`: one row per `#[test]` or
//!   `#[bench]` function, from attributes alone. EMPTY for a unit without `test` in its cfg
//!   (`NotATestUnit`): a plain library or binary has no tests record.
//! - `expansions`: the crate's expansions directory (blaze-scope expansion_records.rs): a row
//!   each macro call that made a text, naming the expansion record (its own record in the
//!   store) that holds the text ([`super::expansion_texts`]), served to a body's check by
//!   front_load.rs `StoreLoad::expansion`. EMPTY for a crate whose calls made no text.
//! - `links`: the crate's link table (`#[link]` lines, `extern` names, exported C names:
//!   [`blaze_ladder::native_links::scope_table`], run.md "the native image as a task"), made
//!   from the names this job already holds (rule 77). The record lists it as the crate's
//!   `Links` part, located by the table's digest, so the program's native image reads a table,
//!   not the whole scope. EMPTY only in a failed crate's record.
//! - `cells`: the crate's cell table ([`blaze_scope::names_cells::crate_table`], drive.md
//!   "Names at module grain" point 3): sorted (cell, digest) rows over the names and items
//!   records, what a body or head that read the crate BY CELL is checked against, by
//!   [`cell_of`], the kind's part lookup the drive is given. EMPTY in a failed crate's record
//!   (every cell then reads absent).
//!
//! The head, [`HEAD`] bytes, little-endian: [`MAGIC`] (4), [`FORM`] (u32), the crate's
//! unresolved count (u32), 4 zero bytes, then per section in the order above its place (u64)
//! and its length (u64). Each section starts at the end of the one before (the head for the
//! first) rounded up to 16, so a reader may map it 16-aligned; the padding is zero; the payload
//! ends where the last section ends (an empty one ends at its place). [`sections`] checks all
//! of it and copies nothing.
//!
//! Older forms (the 16-byte head of exports then inner, before 2026-11-16; form 2, four
//! sections without the tests; form 3, five without the expansions; form 4, six without the
//! link table; form 5, seven without the cell table) are records under other
//! names: names.rs's subject and source
//! domains moved with each form, so an old home's records are never found and are made again;
//! one read anyway is [`SectionsError::Form`].

use super::Global;

/// The payload's first bytes.
pub(crate) const MAGIC: [u8; 4] = *b"BZCS";
/// This form's number. Bump it, and names.rs's domains, at any change of the layout.
pub(crate) const FORM: u32 = 6;
/// How many sections a payload of this form holds.
pub(crate) const SECTIONS: usize = 8;
/// Where the eight (place, length) pairs start.
const PLACES: usize = 16;
/// The head's length: the fixed words, then eight pairs of u64.
pub(crate) const HEAD: usize = PLACES + SECTIONS * 16;
/// What each section's place is rounded up to.
const ALIGN: usize = 16;

/// One section of the payload, in its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Section {
    Exports,
    Inner,
    Names,
    Items,
    Tests,
    Expansions,
    Links,
    Cells,
}

impl Section {
    pub(crate) const ALL: [Section; SECTIONS] =
        [Section::Exports, Section::Inner, Section::Names, Section::Items, Section::Tests, Section::Expansions, Section::Links, Section::Cells];

    fn says(self) -> &'static str {
        match self {
            Section::Exports => "the exports record",
            Section::Inner => "the inner record",
            Section::Names => "the names record",
            Section::Items => "the items record",
            Section::Tests => "the tests record",
            Section::Expansions => "the expansions directory",
            Section::Links => "the link table",
            Section::Cells => "the cell table",
        }
    }

    /// The section's name as the profile's `scope section bytes` rows print it
    /// (phases/byte_rows.rs).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Section::Exports => "exports",
            Section::Inner => "inner",
            Section::Names => "names",
            Section::Items => "items",
            Section::Tests => "tests",
            Section::Expansions => "expansions",
            Section::Links => "links",
            Section::Cells => "cells",
        }
    }
}

/// The seven parts of a `CrateScope` payload, slices of the bytes given (no copy).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sections<'p> {
    pub exports: &'p [u8],
    pub inner: &'p [u8],
    pub names: &'p [u8],
    pub items: &'p [u8],
    /// The unit's tests record; empty when the unit is not compiled with `test`.
    pub tests: &'p [u8],
    /// Its expansions directory ([`super::expansion_texts`]); empty when no call made a text.
    pub expansions: &'p [u8],
    /// The crate's link table ([`blaze_ladder::native_links::open_table`] reads it); empty only
    /// in a failed crate's record.
    pub links: &'p [u8],
    /// The crate's cell table; empty only in a failed crate's record.
    pub cells: &'p [u8],
    /// The crate's unresolved paths, counted when it was made.
    #[cfg_attr(not(test), expect(dead_code, reason = "read back by the payload tests; the command counts a crate's unresolved paths when it makes it (scope.rs)"))]
    pub unresolved: u32,
}

impl<'p> Sections<'p> {
    /// The bytes of `section`: with [`Section::ALL`], every section by the table, never by a
    /// count written out (the profile's `scope section bytes` rows, phases/byte_rows.rs).
    pub(crate) fn part(&self, section: Section) -> &'p [u8] {
        match section {
            Section::Exports => self.exports,
            Section::Inner => self.inner,
            Section::Names => self.names,
            Section::Items => self.items,
            Section::Tests => self.tests,
            Section::Expansions => self.expansions,
            Section::Links => self.links,
            Section::Cells => self.cells,
        }
    }
}

/// Why a payload is not a `CrateScope` payload of this form (rule: names.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SectionsError {
    /// Shorter than the head.
    Head { len: usize },
    /// The head is not this form's: another record, or an older blaze's.
    Form { magic: [u8; 4], form: u32 },
    /// A section does not start where the one before ends, rounded up to 16.
    Misplaced { section: Section, at: u64, want: u64 },
    /// A section runs past the payload's end: the payload was cut short.
    Short { section: Section, end: u64, len: usize },
    /// Bytes after the cell table (the last section).
    Trailing { end: u64, len: usize },
}

impl core::fmt::Display for SectionsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SectionsError::Head { len } => write!(f, "a `CrateScope` payload of {len} bytes is shorter than its {HEAD}-byte head (rule: names.rs)"),
            SectionsError::Form { magic, form } => {
                write!(f, "a `CrateScope` payload's head reads {magic:?} form {form}, not {MAGIC:?} form {FORM}: a record of another form (rule: names.rs)")
            }
            SectionsError::Misplaced { section, at, want } => {
                write!(f, "a `CrateScope` payload places {} at byte {at}, not at byte {want} (rule: names.rs)", section.says())
            }
            SectionsError::Short { section, end, len } => {
                write!(f, "a `CrateScope` payload of {len} bytes is cut short: {} ends at byte {end} (rule: names.rs)", section.says())
            }
            SectionsError::Trailing { end, len } => {
                write!(f, "a `CrateScope` payload of {len} bytes goes on past its last section's end at byte {end} (rule: names.rs)")
            }
        }
    }
}

fn word(head: &[u8; HEAD], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&head[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// The payload of a crate's `CrateScope` record: the head, then `parts` (exports, inner, names,
/// items, tests, expansions, links) each at a 16-aligned place. A function of its arguments only: the same crate
/// gives the same bytes on any machine.
pub(crate) fn write(unresolved: u32, parts: [&[u8]; SECTIONS]) -> blaze_own::Vec<u8, Global> {
    let mut places = [(0usize, 0usize); SECTIONS];
    let mut end = HEAD;
    for (place, part) in places.iter_mut().zip(parts) {
        let at = end.next_multiple_of(ALIGN);
        *place = (at, part.len());
        end = at + part.len();
    }
    let mut payload = blaze_own::Vec::with_capacity_in(end, Global);
    payload.extend_from_slice(&MAGIC);
    payload.extend_from_slice(&FORM.to_le_bytes());
    payload.extend_from_slice(&unresolved.to_le_bytes());
    payload.resize(PLACES, 0);
    for (at, len) in places {
        payload.extend_from_slice(&(at as u64).to_le_bytes());
        payload.extend_from_slice(&(len as u64).to_le_bytes());
    }
    for ((at, _), part) in places.iter().zip(parts) {
        payload.resize(*at, 0);
        payload.extend_from_slice(part);
    }
    payload
}

/// The seven sections of a `CrateScope` payload, every place and length checked against the
/// payload's length; a payload cut short anywhere is refused naming the first section it cuts.
pub(crate) fn sections(payload: &[u8]) -> Result<Sections<'_>, SectionsError> {
    let Some(head) = payload.first_chunk::<HEAD>() else {
        return Err(SectionsError::Head { len: payload.len() });
    };
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&head[..4]);
    let mut words = [0u8; 4];
    words.copy_from_slice(&head[4..8]);
    let form = u32::from_le_bytes(words);
    if magic != MAGIC || form != FORM {
        return Err(SectionsError::Form { magic, form });
    }
    words.copy_from_slice(&head[8..12]);
    let unresolved = u32::from_le_bytes(words);
    let mut parts: [&[u8]; SECTIONS] = [&[]; SECTIONS];
    let mut end = HEAD as u64;
    for (i, section) in Section::ALL.into_iter().enumerate() {
        let at = word(head, PLACES + 16 * i);
        let len = word(head, PLACES + 16 * i + 8);
        let want = end.next_multiple_of(ALIGN as u64);
        if at != want {
            return Err(SectionsError::Misplaced { section, at, want });
        }
        let stop = at.saturating_add(len);
        let range = usize::try_from(at).ok().zip(usize::try_from(stop).ok());
        let Some(bytes) = range.and_then(|(from, to)| payload.get(from..to)) else {
            return Err(SectionsError::Short { section, end: stop, len: payload.len() });
        };
        parts[i] = bytes;
        end = stop;
    }
    if end != payload.len() as u64 {
        return Err(SectionsError::Trailing { end, len: payload.len() });
    }
    let [exports, inner, names, items, tests, expansions, links, cells] = parts;
    Ok(Sections { exports, inner, names, items, tests, expansions, links, cells, unresolved })
}

/// The `CrateScope` kind's part lookup ([`blaze_drive::CellOf`]): the cell's digest in the
/// payload's cell table, one binary search, or absent.
pub(crate) fn cell_of(payload: &[u8], cell: blaze_drive::Cell) -> Result<blaze_drive::Seen, blaze_drive::CellRefused> {
    let parts = sections(payload).map_err(|_| blaze_drive::CellRefused { what: "a `CrateScope` payload whose sections do not read (names/sections.rs)" })?;
    match blaze_scope::names_cells::seen_in(parts.cells, cell.0) {
        Ok(Some(digest)) => Ok(blaze_drive::Seen::Is(digest)),
        Ok(None) => Ok(blaze_drive::Seen::Absent),
        Err(_) => Err(blaze_drive::CellRefused { what: "a `CrateScope` cell table that is not whole rows (names/sections.rs)" }),
    }
}

/// Tests: the one crate whose dependency payload, as a dependent read it from the store, is
/// kept for the test to look at ([`watch`]). Set by the one test that reads it.
#[cfg(test)]
pub(crate) static WATCHED: blaze_core::lock::Lock<Watched> =
    blaze_core::lock::Lock::new("names.watched (tests)", Watched { krate: "", payload: blaze_own::Vec::new_in(Global) });

/// The crate a test watches and its payload once read.
#[cfg(test)]
pub(crate) struct Watched {
    pub krate: &'static str,
    pub payload: blaze_own::Vec<u8, Global>,
}

/// Tests: the first payload WRITTEN with a non-empty tests section, kept for the test to look at
/// ([`watch_tests`]). A test unit is no dependency of anything, so [`WATCHED`] never sees it.
#[cfg(test)]
pub(crate) static WATCHED_TESTS: blaze_core::lock::Lock<Watched> =
    blaze_core::lock::Lock::new("names.watched_tests (tests)", Watched { krate: "", payload: blaze_own::Vec::new_in(Global) });

/// A payload as the names step wrote it: kept when its crate is the one watched and its tests
/// section is not empty (the crate's test unit, not its plain library).
#[cfg(test)]
pub(crate) fn watch_tests(krate: &str, payload: &[u8], tests: &[u8]) {
    let mut watched = WATCHED_TESTS.lock();
    if !watched.krate.is_empty() && watched.krate == krate && !tests.is_empty() && watched.payload.is_empty() {
        watched.payload.extend_from_slice(payload);
    }
}

/// A dependency's payload as read from the store: kept when its crate is the one watched.
#[cfg(test)]
pub(crate) fn watch(krate: &str, payload: &[u8]) {
    let mut watched = WATCHED.lock();
    if !watched.krate.is_empty() && watched.krate == krate && watched.payload.is_empty() {
        watched.payload.extend_from_slice(payload);
    }
}
