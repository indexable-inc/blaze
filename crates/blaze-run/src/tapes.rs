//! The tape records of a property test (docs/v2/plan/design/test.md, "End state: property,
//! differential and end-to-end tests", A "Records", and G. Pieces item 4): the FAILING tape (the
//! least tape of a failure, replayed first in every later run of that test), the REGRESSION tapes
//! (failing tapes that later passed, replayed in every foreground run), the CORPUS (tapes the
//! background search kept for new coverage) and the SEARCHED record (the seed range already
//! searched at one address, so no seed is searched twice there: rule 77). One form for all four,
//! told apart by a kind byte; [`encode`] writes it, [`open`] checks every bound ONCE and the
//! [`TapesView`] then reads in place (rule 74), copying nothing.
//!
//! A tape is held in blaze-sim's ONE text form (crates/blaze-sim/src/text.rs `print`/`parse`: values
//! in decimal, one space between, `0*n` for a run of zeros, trailing zeros cut), as its bytes. No
//! second form of a tape exists: what a failure prints, what `BLAZE_SIM_TAPE` takes, what a kept
//! line in a crate's `sim-kept.tapes` holds (D95, S3, crates/blaze-sim/src/kept.rs) and what this
//! record holds are the same bytes. This crate does not parse them (blaze-sim is not a dependency
//! of the library; the child that replays a tape parses it with `text::parse`); [`open`] checks
//! only that the bytes are in that form's alphabet (`0`-`9`, space, `*`), so a binary blob is
//! refused by name and never handed to a replay.
//!
//! Each tape carries the PRIMITIVES VERSION it was drawn under (the `v<n>` of a kept line), since
//! one tape read by another version of the draw primitives is another case. So a kept line and an
//! entry hold the same three things, the test's path, the primitives version and the tape text,
//! and convert both ways without loss:
//!
//! - a kept line `<test path> v<version> <tape text>  # <date> <why>` (kept.rs parses it) becomes
//!   an [`Entry`] `{ primitives: version, tape: <tape text as bytes> }` in the record whose `path`
//!   is `<test path>`; all of one test's lines go in ONE regression record, sorted by tape bytes;
//!   the date and the why are the file's comment and are not kept here (rule 25: no time of day in
//!   a record);
//! - an entry becomes the line again from [`TapesView::path`], [`Entry::primitives`] and
//!   [`Entry::text`], kept.rs printing it with the date and why its caller gives.
//!
//! The test's 32-byte id ([`TestId`], the key of `TestInputs`) is the record's key and is NOT
//! derived from the path here: the driver finds it by the path in the tests record. The path is
//! kept beside it so that a record names its test in a report and becomes a kept line.
//!
//! The form (integers little-endian), lengths before every variable part:
//!
//! - an 88-byte header: 0 `BZTP`; 4 the version (u32, [`VERSION`]); 8 the kind (0 failing, 1
//!   regression, 2 corpus, 3 searched); 9 three zero bytes; 12 the entry count (u32); 16 the test
//!   id (32 bytes); 48 the D81 address the tapes were found, passed or searched at (32 bytes); 80
//!   the path's length (u32); 84 four zero bytes;
//! - the test's path, UTF-8, 1 to [`MOST_PATH`] bytes, no ASCII space or control, no `#` (so it
//!   is one token of a kept line);
//! - the entries, each: the primitives version (u32), the tape's length (u32), the tape's bytes.
//!   Entries are sorted by their tape bytes and each once: two entries with one tape are two
//!   values under one key (rule 21), whatever their versions.
//!
//! Caps, each checked at [`open`] and refused by name: a tape at most [`MOST_TAPE_BYTES`]; a
//! failing record exactly one entry; a regression record at most [`MOST_REGRESSION`]; a corpus at
//! most [`MOST_CORPUS`]; a searched record at most one entry, whose 16 bytes are the searched
//! seed range (`first`, `end`: two u64, `first < end`) rather than tape text.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::TestId;
use crate::head::le32;
use crate::plan::Address;

/// The record's magic.
const MAGIC: [u8; 4] = *b"BZTP";
/// The form's version: another one is refused by name ([`TapesError::Version`]), never misread.
pub const VERSION: u32 = 1;
/// The header's length.
const HEADER: usize = 88;
/// An entry's fixed part: the primitives version and the tape's length.
const ENTRY_HEAD: usize = 8;
const KIND_AT: usize = 8;
const COUNT_AT: usize = 12;
const TEST_AT: usize = 16;
const ADDRESS_AT: usize = 48;
const PATH_LEN_AT: usize = 80;

/// The longest tape, in bytes of its text.
pub const MOST_TAPE_BYTES: usize = 1 << 20;
/// The most regression tapes a test keeps (test.md A: "at most 64 per test, oldest dropped with
/// a printed line": the dropping is the caller's, which knows the age; this record refuses 65).
pub const MOST_REGRESSION: usize = 64;
/// The most corpus tapes a test keeps.
pub const MOST_CORPUS: usize = 4_096;
/// The longest test path, in bytes.
pub const MOST_PATH: usize = 1_024;
/// A searched entry's length: two u64 seeds.
const SEARCHED_BYTES: usize = 16;

/// Which of the four records the bytes are.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TapeKind {
    /// The least tape of a live failure: exactly one.
    Failing,
    /// Failing tapes that passed later, replayed in every foreground run.
    Regression,
    /// Tapes the background search kept because they reached new coverage.
    Corpus,
    /// The seed range searched at the record's address.
    Searched,
}

impl TapeKind {
    const fn byte(self) -> u8 {
        match self {
            TapeKind::Failing => 0,
            TapeKind::Regression => 1,
            TapeKind::Corpus => 2,
            TapeKind::Searched => 3,
        }
    }

    const fn of_byte(byte: u8) -> Option<TapeKind> {
        match byte {
            0 => Some(TapeKind::Failing),
            1 => Some(TapeKind::Regression),
            2 => Some(TapeKind::Corpus),
            3 => Some(TapeKind::Searched),
            _ => None,
        }
    }

    /// The fewest and the most entries a record of this kind holds.
    const fn counts(self) -> (usize, usize) {
        match self {
            TapeKind::Failing => (1, 1),
            TapeKind::Regression => (0, MOST_REGRESSION),
            TapeKind::Corpus => (0, MOST_CORPUS),
            TapeKind::Searched => (0, 1),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            TapeKind::Failing => "failing",
            TapeKind::Regression => "regression",
            TapeKind::Corpus => "corpus",
            TapeKind::Searched => "searched",
        }
    }
}

/// One entry: a tape (or, in a searched record, a seed range) and the primitives version it was
/// drawn under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'a> {
    /// The draw primitives' version (a kept line's `v<n>`).
    pub primitives: u32,
    /// The tape's text.rs bytes; in a searched record, [`SeedRange::to_bytes`].
    pub tape: &'a [u8],
}

impl<'a> Entry<'a> {
    /// The tape as text, exactly as text.rs printed it and a kept line holds it. `None` only for
    /// bytes outside that form's alphabet, which [`open`] has refused in every tape kind (so on
    /// an entry read from a tape record it is always `Some`), and for a searched entry's seeds.
    pub fn text(&self) -> Option<&'a str> {
        match tape_text(self.tape) {
            Ok(()) => core::str::from_utf8(self.tape).ok(),
            Err(_) => None,
        }
    }
}

/// The seeds `first..end` a background search has run at an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SeedRange {
    pub first: u64,
    pub end: u64,
}

impl SeedRange {
    /// The entry bytes of the range: `first`, then `end`, each a little-endian u64.
    pub fn to_bytes(self) -> [u8; SEARCHED_BYTES] {
        let mut out = [0; SEARCHED_BYTES];
        out[..8].copy_from_slice(&self.first.to_le_bytes());
        out[8..].copy_from_slice(&self.end.to_le_bytes());
        out
    }

    fn of_bytes(bytes: &[u8]) -> Option<SeedRange> {
        if bytes.len() != SEARCHED_BYTES {
            return None;
        }
        let mut first = [0; 8];
        let mut end = [0; 8];
        first.copy_from_slice(&bytes[..8]);
        end.copy_from_slice(&bytes[8..]);
        let range = SeedRange { first: u64::from_le_bytes(first), end: u64::from_le_bytes(end) };
        (range.first < range.end).then_some(range)
    }
}

/// A record as [`encode`] takes it. `tapes` must be sorted by their tape bytes and each once:
/// [`encode`] refuses any other order rather than sorting (the caller sorts its own slice:
/// `entries.sort_unstable_by(|a, b| a.tape.cmp(b.tape))`), so one record has one byte form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TapesRecord<'a> {
    pub kind: TapeKind,
    pub test: TestId,
    pub address: Address,
    pub path: &'a str,
    pub tapes: &'a [Entry<'a>],
}

/// Which part of a record was cut short.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// The 88-byte header.
    Header,
    /// The test's path.
    Path,
    /// Entry `tape`'s version and length.
    EntryHead { tape: usize },
    /// Entry `tape`'s bytes.
    EntryBytes { tape: usize },
}

/// What is wrong with a test path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathProblem {
    Empty,
    TooLong { len: usize },
    NotUtf8,
    /// A space, a control character or `#` at byte `at`: the path would not be one token of a
    /// kept line.
    Separator { at: usize },
}

/// Why a tapes record was not written or not read. Each names the field, the entry and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TapesError {
    /// The bytes do not start with `BZTP`.
    NotThisRecord,
    /// A `BZTP` record of another version.
    Version { found: u32 },
    /// The kind byte is none of the four.
    Kind { byte: u8 },
    /// A reserved header byte is not zero.
    Padding { at: usize },
    /// The record ends inside `field`; `len` is its length.
    Cut { field: Field, len: usize },
    /// More bytes after the last entry.
    Trailing { end: usize, len: usize },
    /// The entry count is outside what the kind allows.
    Count { kind: TapeKind, count: usize },
    /// The test's path.
    Path { problem: PathProblem },
    /// A tape longer than [`MOST_TAPE_BYTES`].
    TapeTooLong { tape: usize, len: usize },
    /// A byte outside text.rs's alphabet in a tape.
    NotTapeText { tape: usize, at: usize, byte: u8 },
    /// A searched entry that is not 16 bytes of `first < end`.
    SeedRange { tape: usize },
    /// An entry not after the one before it: sorted by tape bytes and each once (rules 21, 25).
    Order { tape: usize },
}

impl fmt::Display for TapesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TapesError::NotThisRecord => write!(f, "not a tapes record (magic `BZTP`)"),
            TapesError::Version { found } => {
                write!(f, "tapes record of version {found}, where this blaze reads version {VERSION} alone")
            }
            TapesError::Kind { byte } => {
                write!(f, "tapes record kind {byte}: none of failing (0), regression (1), corpus (2), searched (3)")
            }
            TapesError::Padding { at } => write!(f, "tapes record header byte {at} is reserved and not zero"),
            TapesError::Cut { field, len } => write!(f, "tapes record of {len} bytes ends inside {field:?}"),
            TapesError::Trailing { end, len } => {
                write!(f, "tapes record's last entry ends at byte {end} but the record has {len} bytes")
            }
            TapesError::Count { kind, count } => {
                let (least, most) = kind.counts();
                write!(f, "{} tapes record of {count} entries, where it holds {least} to {most}", kind.name())
            }
            TapesError::Path { problem } => write!(f, "tapes record's test path: {problem:?} (1 to {MOST_PATH} bytes of UTF-8, one token: no space, control or `#`)"),
            TapesError::TapeTooLong { tape, len } => {
                write!(f, "tapes record entry {tape}: a tape of {len} bytes, over the cap of {MOST_TAPE_BYTES}")
            }
            TapesError::NotTapeText { tape, at, byte } => write!(
                f,
                "tapes record entry {tape}, byte {at} ({byte:#04x}): not in blaze-sim text.rs's tape form (digits, space, `*`)"
            ),
            TapesError::SeedRange { tape } => {
                write!(f, "searched record entry {tape}: not 16 bytes of a seed range `first < end`")
            }
            TapesError::Order { tape } => write!(
                f,
                "tapes record entry {tape} is not after the entry before it: sorted by tape and each once (rules 21, 25)"
            ),
        }
    }
}

impl core::error::Error for TapesError {}

/// Appends the bytes of `rec` to `out`. The same checks as [`open`] are made, by [`open`] itself
/// on what was written (one checker for both sides); on an error `out` is left as it was given.
/// Counts and lengths are checked before anything is written, so an over-cap record never
/// allocates its size.
pub fn encode<A: Allocator>(rec: &TapesRecord<'_>, out: &mut Vec<u8, A>) -> Result<(), TapesError> {
    let (least, most) = rec.kind.counts();
    if !(least..=most).contains(&rec.tapes.len()) {
        return Err(TapesError::Count { kind: rec.kind, count: rec.tapes.len() });
    }
    let path = rec.path.as_bytes();
    if path.len() > MOST_PATH {
        return Err(TapesError::Path { problem: PathProblem::TooLong { len: path.len() } });
    }
    let mut size = HEADER + path.len();
    for (tape, entry) in rec.tapes.iter().enumerate() {
        if entry.tape.len() > MOST_TAPE_BYTES {
            return Err(TapesError::TapeTooLong { tape, len: entry.tape.len() });
        }
        size += ENTRY_HEAD + entry.tape.len();
    }
    let start = out.len();
    out.reserve(size);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.push(rec.kind.byte());
    out.extend_from_slice(&[0; 3]);
    // Both conversions are in range: the count is at most `MOST_CORPUS`, the path at most
    // `MOST_PATH`, a tape at most `MOST_TAPE_BYTES`, each checked above.
    out.extend_from_slice(&(rec.tapes.len() as u32).to_le_bytes());
    out.extend_from_slice(&rec.test.0);
    out.extend_from_slice(&rec.address.0);
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(path);
    for entry in rec.tapes {
        out.extend_from_slice(&entry.primitives.to_le_bytes());
        out.extend_from_slice(&(entry.tape.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.tape);
    }
    if let Err(error) = open(&out[start..]) {
        out.truncate(start);
        return Err(error);
    }
    Ok(())
}

/// A tapes record, checked whole by [`open`], read in place.
#[derive(Clone, Copy, Debug)]
pub struct TapesView<'r> {
    kind: TapeKind,
    test: TestId,
    address: Address,
    path: &'r str,
    count: usize,
    /// The entries' bytes, from the first entry's head to the record's end.
    entries: &'r [u8],
}

/// Checks the header, the path, every entry's length, its bytes against its kind and the order
/// of the entries, ONCE; every read of the view then checks nothing (rule 74).
pub fn open(bytes: &[u8]) -> Result<TapesView<'_>, TapesError> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        return Err(TapesError::NotThisRecord);
    }
    if bytes.len() < HEADER {
        return Err(TapesError::Cut { field: Field::Header, len: bytes.len() });
    }
    let version = le32(bytes, 4);
    if version != VERSION {
        return Err(TapesError::Version { found: version });
    }
    let Some(kind) = TapeKind::of_byte(bytes[KIND_AT]) else {
        return Err(TapesError::Kind { byte: bytes[KIND_AT] });
    };
    for at in (KIND_AT + 1..COUNT_AT).chain(PATH_LEN_AT + 4..HEADER) {
        if bytes[at] != 0 {
            return Err(TapesError::Padding { at });
        }
    }
    let count = le32(bytes, COUNT_AT) as usize;
    let (least, most) = kind.counts();
    if !(least..=most).contains(&count) {
        return Err(TapesError::Count { kind, count });
    }
    let mut test = [0; 32];
    test.copy_from_slice(&bytes[TEST_AT..TEST_AT + 32]);
    let mut address = [0; 32];
    address.copy_from_slice(&bytes[ADDRESS_AT..ADDRESS_AT + 32]);
    let path_len = le32(bytes, PATH_LEN_AT) as usize;
    if path_len > MOST_PATH {
        return Err(TapesError::Path { problem: PathProblem::TooLong { len: path_len } });
    }
    if bytes.len() - HEADER < path_len {
        return Err(TapesError::Cut { field: Field::Path, len: bytes.len() });
    }
    let path = test_path(&bytes[HEADER..HEADER + path_len])?;
    let entries_at = HEADER + path_len;
    let mut at = entries_at;
    let mut before: Option<&[u8]> = None;
    for tape in 0..count {
        if bytes.len() - at < ENTRY_HEAD {
            return Err(TapesError::Cut { field: Field::EntryHead { tape }, len: bytes.len() });
        }
        let len = le32(bytes, at + 4) as usize;
        if len > MOST_TAPE_BYTES {
            return Err(TapesError::TapeTooLong { tape, len });
        }
        at += ENTRY_HEAD;
        if bytes.len() - at < len {
            return Err(TapesError::Cut { field: Field::EntryBytes { tape }, len: bytes.len() });
        }
        let body = &bytes[at..at + len];
        match kind {
            TapeKind::Searched => {
                if SeedRange::of_bytes(body).is_none() {
                    return Err(TapesError::SeedRange { tape });
                }
            }
            TapeKind::Failing | TapeKind::Regression | TapeKind::Corpus => {
                tape_text(body).map_err(|(at, byte)| TapesError::NotTapeText { tape, at, byte })?;
            }
        }
        if let Some(before) = before
            && before >= body
        {
            return Err(TapesError::Order { tape });
        }
        before = Some(body);
        at += len;
    }
    if at != bytes.len() {
        return Err(TapesError::Trailing { end: at, len: bytes.len() });
    }
    Ok(TapesView { kind, test: TestId(test), address: Address(address), path, count, entries: &bytes[entries_at..] })
}

impl<'r> TapesView<'r> {
    pub fn kind(&self) -> TapeKind {
        self.kind
    }

    pub fn test(&self) -> TestId {
        self.test
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// The test's path: a kept line's first token.
    pub fn path(&self) -> &'r str {
        self.path
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Entry `i`, or `None` past the end. Walks the entries before it (the form has no index:
    /// readers take the entries in order through [`TapesView::entries`], one step each).
    pub fn tape(&self, i: usize) -> Option<Entry<'r>> {
        self.entries().nth(i)
    }

    /// The entries in their order (sorted by tape bytes).
    pub fn entries(&self) -> Entries<'r> {
        Entries { bytes: self.entries, left: self.count }
    }

    /// A searched record's range, `None` for an empty searched record or another kind.
    pub fn searched(&self) -> Option<SeedRange> {
        match self.kind {
            TapeKind::Searched => self.tape(0).and_then(|entry| SeedRange::of_bytes(entry.tape)),
            TapeKind::Failing | TapeKind::Regression | TapeKind::Corpus => None,
        }
    }

    /// The view as an [`encode`] input, with `buf` holding its entries: encoding it gives the
    /// opened bytes again.
    pub fn record<'b>(&self, buf: &'b mut [Entry<'r>]) -> Option<TapesRecord<'b>>
    where
        'r: 'b,
    {
        let buf = buf.get_mut(..self.count)?;
        for (slot, entry) in buf.iter_mut().zip(self.entries()) {
            *slot = entry;
        }
        Some(TapesRecord { kind: self.kind, test: self.test, address: self.address, path: self.path, tapes: buf })
    }
}

/// The entries of an opened record. Every length was checked by [`open`].
#[derive(Clone, Debug)]
pub struct Entries<'r> {
    bytes: &'r [u8],
    left: usize,
}

impl<'r> Iterator for Entries<'r> {
    type Item = Entry<'r>;

    fn next(&mut self) -> Option<Entry<'r>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let primitives = le32(self.bytes, 0);
        let len = le32(self.bytes, 4) as usize;
        let tape = &self.bytes[ENTRY_HEAD..ENTRY_HEAD + len];
        self.bytes = &self.bytes[ENTRY_HEAD + len..];
        Some(Entry { primitives, tape })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for Entries<'_> {}

/// `Ok` when every byte is in text.rs's alphabet; else the first other byte and where it is.
fn tape_text(bytes: &[u8]) -> Result<(), (usize, u8)> {
    match bytes.iter().position(|b| !(b.is_ascii_digit() || *b == b' ' || *b == b'*')) {
        Some(at) => Err((at, bytes[at])),
        None => Ok(()),
    }
}

/// The path, when it is one token of a kept line.
fn test_path(bytes: &[u8]) -> Result<&str, TapesError> {
    if bytes.is_empty() {
        return Err(TapesError::Path { problem: PathProblem::Empty });
    }
    let Ok(path) = core::str::from_utf8(bytes) else {
        return Err(TapesError::Path { problem: PathProblem::NotUtf8 });
    };
    match bytes.iter().position(|b| b.is_ascii_whitespace() || b.is_ascii_control() || *b == b'#') {
        Some(at) => Err(TapesError::Path { problem: PathProblem::Separator { at } }),
        None => Ok(path),
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{Alloc, Vec as Own};

    use super::{Entry, Field, PathProblem, SeedRange, TapeKind, TapesError, TapesRecord, encode, open};
    use crate::TestId;
    use crate::plan::Address;

    fn key(n: u64) -> [u8; 32] {
        let mut out = [0; 32];
        out[..8].copy_from_slice(&n.to_le_bytes());
        out
    }

    /// Encodes `rec`, opens it, checks every field and entry, encodes the view again and checks
    /// the bytes are equal; returns the bytes.
    fn round_trip(rec: &TapesRecord<'_>) -> Own<u8, Alloc<'static>> {
        let mut bytes = Own::new_in(Alloc::global());
        encode(rec, &mut bytes).expect("a well-formed record encodes");
        let view = open(bytes.as_slice()).expect("written bytes open");
        assert_eq!((view.kind(), view.test(), view.address(), view.path(), view.len()), (rec.kind, rec.test, rec.address, rec.path, rec.tapes.len()));
        for (i, (got, want)) in view.entries().zip(rec.tapes.iter()).enumerate() {
            assert_eq!(got, *want, "entry {i} of the {:?} record", rec.kind);
            assert_eq!(view.tape(i), Some(*want));
        }
        assert_eq!(view.tape(rec.tapes.len()), None);
        let mut buf = [Entry { primitives: 0, tape: &[] }; 8];
        let again_rec = view.record(&mut buf).expect("at most 8 entries in this test");
        let mut again = Own::new_in(Alloc::global());
        encode(&again_rec, &mut again).expect("the view encodes");
        assert_eq!(again.as_slice(), bytes.as_slice(), "the {:?} record round-trips byte-equal", rec.kind);
        // Cut at every length: refused by name, never a panic and never a shorter record.
        for len in 0..bytes.len() {
            let cut = open(&bytes.as_slice()[..len]).map(|v| v.len());
            assert!(cut.is_err(), "the {:?} record cut at {len} of {} opened: {cut:?}", rec.kind, bytes.len());
            if (4..88).contains(&len) {
                assert_eq!(cut, Err(TapesError::Cut { field: Field::Header, len }));
            }
        }
        bytes
    }

    /// Two records, two magics: the tapes reader refuses the postings record's bytes as not its
    /// record, and the postings reader refuses the tapes record's. The cause it guards: both were
    /// `BZTP` at version 1, so postings bytes passed the tapes magic check and were refused, if
    /// at all, by a later field (a one-row postings header reads as a tapes header of kind 1 with
    /// zero padding). Control: each reader opens its own bytes.
    ///
    /// Expected line: `test tapes::tests::each_reader_refuses_the_other_records_bytes_by_magic ... ok`
    #[test]
    fn each_reader_refuses_the_other_records_bytes_by_magic() {
        use crate::postings::{self, Ask, FnName, Postings, PostingsError};
        assert_ne!(super::MAGIC, postings::MAGIC, "one magic per record kind");
        let (test, address, path) = (TestId(key(7)), Address(key(9)), "blaze_run::tapes::tests::magic");
        let failing = [Entry { primitives: 1, tape: b"1" }];
        let mut tapes = Own::new_in(Alloc::global());
        encode(&TapesRecord { kind: TapeKind::Failing, test, address, path, tapes: &failing }, &mut tapes).expect("a well-formed record encodes");
        let posted = postings::encode(&[(Ask::Fn(FnName(key(1))), test)], Alloc::global()).expect("one row encodes");
        // Controls: each reader opens its own record.
        assert_eq!(open(tapes.as_slice()).map(|v| v.len()), Ok(1));
        assert_eq!(Postings::open(posted.as_slice()).map(|p| p.len()), Ok(1));
        // Each refuses the other's by its magic, before any later field is read.
        assert_eq!(open(posted.as_slice()).map(|v| v.len()), Err(TapesError::NotThisRecord));
        assert_eq!(Postings::open(tapes.as_slice()).map(|p| p.len()), Err(PostingsError::NotThisRecord));
    }

    /// Every kind of tapes record round-trips byte-equal; the record cut at each length is
    /// refused; an unsorted pair is refused on both sides; caps, the kind, the version, the path
    /// and a tape that is not in text.rs's form are refused by name. And a kept line's three
    /// parts (test path, primitives version, tape text: D95 S3) go into an entry and come back
    /// unchanged, the tape text parsing with blaze-sim's one parser to the values printed.
    ///
    /// Expected line (`lanecheck.sh blaze-run test tapes::`):
    /// `test tapes::tests::tapes_round_trip_and_a_cut_record_is_refused ... ok`
    #[test]
    fn tapes_round_trip_and_a_cut_record_is_refused() {
        let (test, address, path) = (TestId(key(7)), Address(key(9)), "blaze_run::tapes::tests::planted");
        // The least tape of W1-3's planted failure, in text.rs's one form.
        let least = blaze_sim::text::print(&[2, 0, 0, 0, 255]);
        assert_eq!(least, "2 0*3 255");
        let failing = [Entry { primitives: 1, tape: least.as_bytes() }];
        let rec = TapesRecord { kind: TapeKind::Failing, test, address, path, tapes: &failing };
        let bytes = round_trip(&rec);

        // The kept line's three parts out of the record, and the tape back to its values.
        let view = open(bytes.as_slice()).expect("opens");
        let entry = view.tape(0).expect("one entry");
        assert_eq!((view.path(), entry.primitives, entry.text()), (path, 1, Some("2 0*3 255")));
        let values = blaze_sim::text::parse(entry.text().expect("tape text")).expect("text.rs parses its own form");
        assert_eq!(values.as_slice(), &[2, 0, 0, 0, 255]);

        // Regression: three tapes, two primitives versions, sorted by bytes; the empty (calm)
        // tape is a tape.
        let regression = [
            Entry { primitives: 1, tape: b"" },
            Entry { primitives: 1, tape: b"1 0*4 9" },
            Entry { primitives: 2, tape: b"17" },
        ];
        round_trip(&TapesRecord { kind: TapeKind::Regression, test, address, path, tapes: &regression });
        round_trip(&TapesRecord { kind: TapeKind::Regression, test, address, path, tapes: &[] });
        let corpus = [Entry { primitives: 1, tape: b"3" }, Entry { primitives: 1, tape: b"3 4" }, Entry { primitives: 1, tape: b"4" }];
        round_trip(&TapesRecord { kind: TapeKind::Corpus, test, address, path, tapes: &corpus });
        let seeds = SeedRange { first: 0, end: 256 }.to_bytes();
        let searched = [Entry { primitives: 1, tape: &seeds }];
        let bytes = round_trip(&TapesRecord { kind: TapeKind::Searched, test, address, path, tapes: &searched });
        assert_eq!(open(bytes.as_slice()).expect("opens").searched(), Some(SeedRange { first: 0, end: 256 }));

        // An unsorted pair: refused by encode, and by open when the bytes are swapped by hand.
        let mut out = Own::new_in(Alloc::global());
        let unsorted = [Entry { primitives: 1, tape: b"1 3" }, Entry { primitives: 1, tape: b"1 2" }];
        let refused = encode(&TapesRecord { kind: TapeKind::Regression, test, address, path, tapes: &unsorted }, &mut out);
        assert_eq!(refused, Err(TapesError::Order { tape: 1 }));
        assert!(out.is_empty(), "a refused record leaves nothing written");
        let sorted = [unsorted[1], unsorted[0]];
        encode(&TapesRecord { kind: TapeKind::Regression, test, address, path, tapes: &sorted }, &mut out).expect("sorted encodes");
        let mut swapped = out.as_slice().to_vec();
        let first = 88 + path.len() + 8;
        let second = first + 3 + 8;
        swapped[first..first + 3].copy_from_slice(b"1 3");
        swapped[second..second + 3].copy_from_slice(b"1 2");
        assert_eq!(open(&swapped).map(|v| v.len()), Err(TapesError::Order { tape: 1 }));
        // One tape twice, even under two versions, is two values under one key (rule 21).
        let twice = [Entry { primitives: 1, tape: b"5" }, Entry { primitives: 2, tape: b"5" }];
        let mut out = Own::new_in(Alloc::global());
        assert_eq!(
            encode(&TapesRecord { kind: TapeKind::Corpus, test, address, path, tapes: &twice }, &mut out),
            Err(TapesError::Order { tape: 1 })
        );

        // Caps and forms, each by name.
        let mut out = Own::new_in(Alloc::global());
        assert_eq!(
            encode(&TapesRecord { kind: TapeKind::Failing, test, address, path, tapes: &[] }, &mut out),
            Err(TapesError::Count { kind: TapeKind::Failing, count: 0 })
        );
        // 65 tapes, sorted (four-digit numbers sort as text in numeric order).
        let mut texts = Own::new_in(Alloc::global());
        for n in 0..65u64 {
            texts.push(blaze_sim::text::print(&[1_000 + n]));
        }
        let mut many = Own::new_in(Alloc::global());
        for text in texts.iter() {
            many.push(Entry { primitives: 1, tape: text.as_bytes() });
        }
        assert_eq!(
            encode(&TapesRecord { kind: TapeKind::Regression, test, address, path, tapes: many.as_slice() }, &mut out),
            Err(TapesError::Count { kind: TapeKind::Regression, count: 65 })
        );
        let binary = [Entry { primitives: 1, tape: &[1, 2] }];
        assert_eq!(
            encode(&TapesRecord { kind: TapeKind::Corpus, test, address, path, tapes: &binary }, &mut out),
            Err(TapesError::NotTapeText { tape: 0, at: 0, byte: 1 })
        );
        let empty_range = SeedRange { first: 9, end: 9 }.to_bytes();
        let backwards = [Entry { primitives: 1, tape: &empty_range }];
        assert_eq!(
            encode(&TapesRecord { kind: TapeKind::Searched, test, address, path, tapes: &backwards }, &mut out),
            Err(TapesError::SeedRange { tape: 0 })
        );
        assert_eq!(
            encode(&TapesRecord { kind: TapeKind::Failing, test, address, path: "a b", tapes: &failing }, &mut out),
            Err(TapesError::Path { problem: PathProblem::Separator { at: 1 } })
        );
        assert!(out.is_empty(), "no refused record left bytes behind");

        // Another version, kind or trailing bytes are not this record.
        let mut good = Own::new_in(Alloc::global());
        encode(&rec, &mut good).expect("encodes");
        let mut other = good.as_slice().to_vec();
        other[4] = 2;
        assert_eq!(open(&other).map(|v| v.len()), Err(TapesError::Version { found: 2 }));
        let mut other = good.as_slice().to_vec();
        other[8] = 4;
        assert_eq!(open(&other).map(|v| v.len()), Err(TapesError::Kind { byte: 4 }));
        let mut other = good.as_slice().to_vec();
        other.push(b'0');
        assert_eq!(open(&other).map(|v| v.len()), Err(TapesError::Trailing { end: good.len(), len: good.len() + 1 }));
        assert_eq!(open(b"BZTH").map(|v| v.len()), Err(TapesError::NotThisRecord));
    }
}
