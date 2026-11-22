//! The postings record of a suite (docs/v2/plan/design/test.md, "End state"): for each ask (a
//! function a test entered, or an outside read it made), the tests whose kept result asked it.
//! It is how `blaze test` finds the tests an edit can affect FORWARD from what changed: one binary
//! search per changed ask, in place, with no graph held in memory and no scan of the tests.
//!
//! The form (integers little-endian): a 16-byte header (`BZPS`, the version, the row count,
//! zero), then the rows, FIXED 72 bytes, sorted by their bytes and each once: the ask's tag (0 a
//! function, 1 a read), seven zero bytes, the ask's 32-byte name, the test's 32-byte id. Since
//! the tag comes first and names compare as bytes, byte order is `(Ask, TestId)` order, so any
//! writer in any order writes equal bytes (rule 25). A reader checks the header, the length, each
//! tag and the order ONCE, at [`Postings::open`]; a lookup then checks nothing (rule 74).

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::{Hex, TestId};

/// A function instance by its stable name: its item and type arguments, never its code. An edit
/// of the body leaves the name and moves the code identity it is answered by. Becomes the front
/// end's instance name (front2's reach walk names instances so).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FnName(pub [u8; 32]);

/// An outside read by its name: a file by its path from the workspace root, an environment
/// variable, a host fact (a CPU feature, the OS version), hashed with its kind. Answered by the
/// digest of what it reads now.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ReadName(pub [u8; 32]);

/// One thing a test's result rests on. The order (functions first, then reads, each by name) is
/// the record's byte order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Ask {
    Fn(FnName),
    Read(ReadName),
}

impl Ask {
    const fn tag(self) -> u8 {
        match self {
            Ask::Fn(_) => 0,
            Ask::Read(_) => 1,
        }
    }

    const fn name(&self) -> &[u8; 32] {
        match self {
            Ask::Fn(FnName(name)) | Ask::Read(ReadName(name)) => name,
        }
    }
}

/// The record's magic, its own: the tapes record (tapes.rs) is `BZTP`, and a header both
/// shared let one record's bytes pass the other's check (a postings header of three rows or
/// fewer reads as a tapes header of kind 0..3 with zero padding).
pub(crate) const MAGIC: [u8; 4] = *b"BZPS";
const VERSION: u32 = 1;
const HEADER: usize = 16;
const ROW: usize = 72;
const NAME_AT: usize = 8;
const TEST_AT: usize = 40;

/// Why a postings record was not written or not read. Each names the row and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PostingsError {
    /// The bytes do not start with this record's magic and version.
    NotThisRecord,
    /// The length is not the header plus the rows the header counts.
    Length { rows: usize, len: usize },
    /// A row's tag is neither a function (0) nor a read (1), or its padding is not zero.
    Tag { row: usize },
    /// A row is not after the one before it: the rows must be sorted and each once.
    Order { row: usize },
    /// More rows than a u32 counts.
    TooMany { rows: usize },
}

impl fmt::Display for PostingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PostingsError::NotThisRecord => write!(f, "not a postings record (magic `BZPS`, version {VERSION})"),
            PostingsError::Length { rows, len } => {
                write!(f, "postings record of {len} bytes, but its header counts {rows} rows of {ROW} bytes after {HEADER}")
            }
            PostingsError::Tag { row } => write!(f, "postings row {row}: an unknown ask tag or padding that is not zero"),
            PostingsError::Order { row } => {
                write!(f, "postings row {row} is not after row {}: rows are sorted and each once (rule 25)", row.saturating_sub(1))
            }
            PostingsError::TooMany { rows } => write!(f, "{rows} postings rows: more than a u32 counts"),
        }
    }
}

impl core::error::Error for PostingsError {}

/// The record's bytes for `pairs`, in any order and with repeats: sorted and each once, so equal
/// sets give equal bytes. The buffer is in `alloc`.
pub fn encode<'b>(pairs: &[(Ask, TestId)], alloc: Alloc<'b>) -> Result<AVec<'b, u8>, PostingsError> {
    let mut rows: AVec<'b, (Ask, TestId)> = Vec::from_slice_in(pairs, alloc);
    rows.sort_unstable();
    rows.dedup();
    let count = u32::try_from(rows.len()).map_err(|_| PostingsError::TooMany { rows: rows.len() })?;
    let mut out = Vec::with_capacity_in(HEADER + rows.len() * ROW, alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for (ask, test) in rows.iter() {
        out.push(ask.tag());
        out.extend_from_slice(&[0; 7]);
        out.extend_from_slice(ask.name());
        out.extend_from_slice(&test.0);
    }
    Ok(out)
}

/// A postings record, open: checked once, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Postings<'r> {
    rows: &'r [u8],
    count: usize,
}

impl<'r> Postings<'r> {
    /// No postings: a suite with no kept result yet.
    pub const EMPTY: Postings<'static> = Postings { rows: &[], count: 0 };

    /// Checks the header, the length, every row's tag and padding, and that the rows are sorted
    /// and each once. Nothing is checked again on a lookup.
    pub fn open(bytes: &'r [u8]) -> Result<Postings<'r>, PostingsError> {
        if bytes.len() < HEADER || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
            return Err(PostingsError::NotThisRecord);
        }
        let count = le32(bytes, 8) as usize;
        let rows = &bytes[HEADER..];
        if count.checked_mul(ROW) != Some(rows.len()) || le32(bytes, 12) != 0 {
            return Err(PostingsError::Length { rows: count, len: bytes.len() });
        }
        for row in 0..count {
            let at = row * ROW;
            if rows[at] > 1 || rows[at + 1..at + NAME_AT].iter().any(|b| *b != 0) {
                return Err(PostingsError::Tag { row });
            }
            if row > 0 && rows[at - ROW..at] >= rows[at..at + ROW] {
                return Err(PostingsError::Order { row });
            }
        }
        Ok(Postings { rows, count })
    }

    /// How many (ask, test) rows.
    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The `row`th row's ask key (tag and name, 33 bytes compared as the record orders them) and
    /// test.
    fn row(&self, row: usize) -> (u8, &'r [u8], TestId) {
        let rows: &'r [u8] = self.rows;
        let at = row * ROW;
        let mut test = [0; 32];
        test.copy_from_slice(&rows[at + TEST_AT..at + TEST_AT + 32]);
        (rows[at], &rows[at + NAME_AT..at + NAME_AT + 32], TestId(test))
    }

    /// Pushes to `out` every test whose kept result asked `ask`, in id order. One binary search
    /// for the first row of `ask`, then the rows that follow it while they hold `ask`.
    pub fn tests_of(&self, ask: Ask, out: &mut AVec<'_, TestId>) {
        let (tag, name) = (ask.tag(), &ask.name()[..]);
        // The first row whose (tag, name) is not below the ask's.
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            let (row_tag, row_name, _) = self.row(mid);
            if (row_tag, row_name) < (tag, name) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        for row in low..self.count {
            let (row_tag, row_name, test) = self.row(row);
            if (row_tag, row_name) != (tag, name) {
                break;
            }
            out.push(test);
        }
    }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl fmt::Display for Ask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ask::Fn(FnName(name)) => write!(f, "function {}", Hex(name)),
            Ask::Read(ReadName(name)) => write!(f, "read {}", Hex(name)),
        }
    }
}
