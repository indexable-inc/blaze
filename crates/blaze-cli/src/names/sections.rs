//! The form of a `CrateScope` record's payload (drive.md, "End state", piece 3, NAMES AND ITEMS
//! FILED): one head, then four sections, each found by its place in the head.
//!
//! - `exports`: the exports record, what a dependent reads and keys on (the task's out is its
//!   digest alone);
//! - `inner`: the inner record, the crate's own;
//! - `names`: the names record ([`blaze_scope::names_record::encode`]), read through
//!   `RecordNames`: a path in the crate answered without solving it;
//! - `items`: the items record ([`blaze_scope::items_record::encode`]), read through
//!   `RecordItems`: an item's row (kind, module, text place) without its crate's tables.
//!
//! The head, [`HEAD`] bytes, little-endian: [`MAGIC`] (4), [`FORM`] (u32), the crate's
//! unresolved count (u32), 4 zero bytes, then per section in the order above its place (u64)
//! and its length (u64). Each section starts at the end of the one before (the head for the
//! first) rounded up to 16, so a reader may map it 16-aligned; the padding is zero; the payload
//! ends where the items end. [`sections`] checks all of it and copies nothing.
//!
//! An older form (the 16-byte head of exports then inner, before 2026-11-16) is a record under
//! another name: names.rs's subject and source domains moved with this form, so an old home's
//! records are never found and are made again; one read anyway is [`SectionsError::Form`].

use super::Global;

/// The payload's first bytes.
pub(crate) const MAGIC: [u8; 4] = *b"BZCS";
/// This form's number. Bump it, and names.rs's domains, at any change of the layout.
pub(crate) const FORM: u32 = 2;
/// Where the four (place, length) pairs start.
const PLACES: usize = 16;
/// The head's length: the fixed words, then four pairs of u64.
pub(crate) const HEAD: usize = PLACES + 4 * 16;
/// What each section's place is rounded up to.
const ALIGN: usize = 16;

/// One section of the payload, in its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Section {
    Exports,
    Inner,
    Names,
    Items,
}

impl Section {
    pub(crate) const ALL: [Section; 4] = [Section::Exports, Section::Inner, Section::Names, Section::Items];

    fn says(self) -> &'static str {
        match self {
            Section::Exports => "the exports record",
            Section::Inner => "the inner record",
            Section::Names => "the names record",
            Section::Items => "the items record",
        }
    }
}

/// The four parts of a `CrateScope` payload, slices of the bytes given (no copy).
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "the command reads `exports` today; the names and items sections' readers are drive.md's pieces 5 and 9"))]
pub(crate) struct Sections<'p> {
    pub exports: &'p [u8],
    pub inner: &'p [u8],
    pub names: &'p [u8],
    pub items: &'p [u8],
    /// The crate's unresolved paths, counted when it was made.
    pub unresolved: u32,
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
    /// Bytes after the items record.
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
                write!(f, "a `CrateScope` payload of {len} bytes goes on past its items record's end at byte {end} (rule: names.rs)")
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
/// items) each at a 16-aligned place. A function of its arguments only: the same crate gives
/// the same bytes on any machine.
pub(crate) fn write(unresolved: u32, parts: [&[u8]; 4]) -> blaze_own::Vec<u8, Global> {
    let mut places = [(0usize, 0usize); 4];
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

/// The four sections of a `CrateScope` payload, every place and length checked against the
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
    let mut parts: [&[u8]; 4] = [&[]; 4];
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
    let [exports, inner, names, items] = parts;
    Ok(Sections { exports, inner, names, items, unresolved })
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

/// A dependency's payload as read from the store: kept when its crate is the one watched.
#[cfg(test)]
pub(crate) fn watch(krate: &str, payload: &[u8]) {
    let mut watched = WATCHED.lock();
    if !watched.krate.is_empty() && watched.krate == krate && watched.payload.is_empty() {
        watched.payload.extend_from_slice(payload);
    }
}
