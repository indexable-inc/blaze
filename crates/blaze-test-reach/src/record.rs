//! A test's result record (tests-stage.md, "Its record"). All integers little-endian; equal values
//! encode to equal bytes (rule 25).
//!
//! | at | bytes | what |
//! |---|---|---|
//! | 0 | 4 | `BZTK` |
//! | 4 | 4 | version, [`VERSION`] |
//! | 8 | 1 | out: 0 passed, 1 failed, 2 ignored |
//! | 9 | 1 | reusable: 0 yes, else the [`NotReusable`] number |
//! | 10 | 6 | zero |
//! | 16 | 32 | the test item's key |
//! | 48 | 32 | the reach's digest |
//! | 80 | 32 | a failure's captured output record key; zero for any other out |
//! | 112 | 4 | the row count, at most [`MAX_READS`] |
//! | 116 | 4 | zero |
//! | 120 | 72 a row | kind, seven zero bytes, name (32), value (32); strictly ascending |
//!
//! [`TestRecord::open`] checks every field once, before any row is trusted, each refusal naming
//! its field ([`RecordError`]); the view it gives is the proof later code takes (rule 74): its
//! rows are read in place and never checked again.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::reach::{REACH_DOMAIN, ROW, Reached, row_bytes};
use crate::{CellKind, Digest, MAX_READS, NotReusable, Read, Reusable};

pub const MAGIC: [u8; 4] = *b"BZTK";
pub const VERSION: u32 = 1;
/// The fixed part before the rows.
pub const HEADER: usize = 120;

const OUT_AT: usize = 8;
const REUSABLE_AT: usize = 9;
const TEST_AT: usize = 16;
const DIGEST_AT: usize = 48;
const OUTPUT_AT: usize = 80;
const COUNT_AT: usize = 112;

/// How the test's run ended, as the runner judged it (blaze-run plan.rs judges `should_panic`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Out {
    Passed,
    /// Failed: its captured output is the record under this key.
    Failed { output: Digest },
    Ignored,
}

impl Out {
    const fn tag(self) -> u8 {
        match self {
            Out::Passed => 0,
            Out::Failed { .. } => 1,
            Out::Ignored => 2,
        }
    }
}

/// Why bytes are not a test record, naming the field (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// Shorter than the fixed part.
    Short { len: usize },
    Magic,
    Version { found: u32 },
    OutTag { found: u8 },
    ReusableTag { found: u8 },
    /// A field that must be zero is not, at this byte.
    NotZero { at: usize },
    /// More rows than [`MAX_READS`].
    TooManyReads { count: u32 },
    /// The length is not the fixed part and the rows the count says.
    Length { len: usize, want: usize },
    RowKind { row: usize, found: u8 },
    /// A row's seven bytes after its kind are not zero.
    RowPadding { row: usize },
    /// A row is not after the one before it.
    Order { row: usize },
    /// A row names the cell the row before it names, with another value.
    TwoValues { row: usize },
    /// The rows' digest is not the stored one.
    Digest,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a test record is refused: ")?;
        match self {
            RecordError::Short { len } => write!(f, "{len} bytes, shorter than its {HEADER}-byte fixed part"),
            RecordError::Magic => f.write_str("it does not start `BZTK`"),
            RecordError::Version { found } => write!(f, "version {found}, not {VERSION}"),
            RecordError::OutTag { found } => write!(f, "out tag {found} is none of passed (0), failed (1), ignored (2)"),
            RecordError::ReusableTag { found } => write!(f, "reusable tag {found} is no reason"),
            RecordError::NotZero { at } => write!(f, "byte {at} must be zero"),
            RecordError::TooManyReads { count } => write!(f, "{count} rows, more than the {MAX_READS} a record holds"),
            RecordError::Length { len, want } => write!(f, "{len} bytes where its row count makes {want}"),
            RecordError::RowKind { row, found } => write!(f, "row {row}'s kind {found} is no cell kind"),
            RecordError::RowPadding { row } => write!(f, "row {row}'s bytes after its kind are not zero"),
            RecordError::Order { row } => write!(f, "row {row} is not after row {}", row.saturating_sub(1)),
            RecordError::TwoValues { row } => write!(f, "row {row} names row {}'s cell with another value", row.saturating_sub(1)),
            RecordError::Digest => f.write_str("its rows are not the reach its digest names"),
        }?;
        f.write_str(" (blaze-test-reach record.rs `TestRecord::open`)")
    }
}

/// An opened test record: every field checked, its rows read in place.
#[derive(Clone, Copy, Debug)]
pub struct TestRecord<'r> {
    test: Digest,
    out: Out,
    reusable: Reusable,
    digest: Digest,
    rows: &'r [u8],
}

impl<'r> TestRecord<'r> {
    /// The record of test item `test`'s run: its out, whether it may be reused, its reach.
    pub fn encode<A: Allocator, B: Allocator>(test: Digest, out: Out, reusable: Reusable, reach: &Reached<B>, into: &mut Vec<u8, A>) {
        let reads = reach.reads();
        // A `Reached` holds at most `MAX_READS` rows (`ReachOpts::cap` is checked against it).
        let count = u32::try_from(reads.len()).unwrap_or(u32::MAX);
        into.clear();
        into.extend_from_slice(&MAGIC);
        into.extend_from_slice(&VERSION.to_le_bytes());
        into.extend_from_slice(&[out.tag(), reusable.byte(), 0, 0, 0, 0, 0, 0]);
        into.extend_from_slice(&test.0);
        into.extend_from_slice(&reach.digest().0);
        let output = match out {
            Out::Failed { output } => output,
            Out::Passed | Out::Ignored => Digest::ZERO,
        };
        into.extend_from_slice(&output.0);
        into.extend_from_slice(&count.to_le_bytes());
        into.extend_from_slice(&[0; 4]);
        for read in reads {
            into.extend_from_slice(&row_bytes(read));
        }
    }

    /// `bytes` as a test record, every limit checked before a row is trusted.
    pub fn open(bytes: &'r [u8]) -> Result<TestRecord<'r>, RecordError> {
        if bytes.len() < HEADER {
            return Err(RecordError::Short { len: bytes.len() });
        }
        if bytes[..4] != MAGIC {
            return Err(RecordError::Magic);
        }
        let version = le32(bytes, 4);
        if version != VERSION {
            return Err(RecordError::Version { found: version });
        }
        let output = digest_at(bytes, OUTPUT_AT);
        let out = match bytes[OUT_AT] {
            0 => Out::Passed,
            1 => Out::Failed { output },
            2 => Out::Ignored,
            found => return Err(RecordError::OutTag { found }),
        };
        let reusable = match bytes[REUSABLE_AT] {
            0 => Reusable::Yes,
            found => Reusable::No(NotReusable::from_byte(found).ok_or(RecordError::ReusableTag { found })?),
        };
        let mut zeros = (10..16).chain(116..120);
        if let Some(at) = zeros.find(|at| bytes[*at] != 0) {
            return Err(RecordError::NotZero { at });
        }
        if !matches!(out, Out::Failed { .. })
            && let Some(at) = (OUTPUT_AT..OUTPUT_AT + 32).find(|at| bytes[*at] != 0)
        {
            return Err(RecordError::NotZero { at });
        }
        let count = le32(bytes, COUNT_AT);
        let rows_len = usize::try_from(count).ok().filter(|count| *count <= MAX_READS).ok_or(RecordError::TooManyReads { count })?;
        let want = HEADER + rows_len * ROW;
        if bytes.len() != want {
            return Err(RecordError::Length { len: bytes.len(), want });
        }
        let rows = &bytes[HEADER..];
        let mut before: Option<Read> = None;
        for (row, at) in rows.chunks_exact(ROW).enumerate() {
            let kind = CellKind::from_byte(at[0]).ok_or(RecordError::RowKind { row, found: at[0] })?;
            if at[1..8].iter().any(|byte| *byte != 0) {
                return Err(RecordError::RowPadding { row });
            }
            let read = Read { kind, name: digest_at(at, 8), value: digest_at(at, 40) };
            if let Some(prev) = before {
                if prev.kind == read.kind && prev.name == read.name && prev.value != read.value {
                    return Err(RecordError::TwoValues { row });
                }
                if read <= prev {
                    return Err(RecordError::Order { row });
                }
            }
            before = Some(read);
        }
        let record = TestRecord { test: digest_at(bytes, TEST_AT), out, reusable, digest: digest_at(bytes, DIGEST_AT), rows };
        if digest_of_rows(rows) != record.digest {
            return Err(RecordError::Digest);
        }
        Ok(record)
    }

    /// The test item's key.
    pub fn test(&self) -> Digest {
        self.test
    }

    pub fn out(&self) -> Out {
        self.out
    }

    pub fn reusable(&self) -> Reusable {
        self.reusable
    }

    /// The reach's digest: equal digests, equal reach.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// How many cells the reach holds.
    pub fn len(&self) -> usize {
        self.rows.len() / ROW
    }

    /// The reach's cells, in the record's order.
    pub fn reads(&self) -> Reads<'r> {
        Reads { rows: self.rows.chunks_exact(ROW) }
    }
}

/// An opened record's cells, read in place.
#[derive(Clone)]
pub struct Reads<'r> {
    rows: core::slice::ChunksExact<'r, u8>,
}

impl Iterator for Reads<'_> {
    type Item = Read;

    fn next(&mut self) -> Option<Read> {
        let at = self.rows.next()?;
        // `open` refused any kind byte past the last kind: indexing by it is in bounds.
        let kind = CellKind::ALL[usize::from(at[0])];
        Some(Read { kind, name: digest_at(at, 8), value: digest_at(at, 40) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

/// The digest of a record's row bytes: reach.rs `digest_of`'s, which hashes each row's bytes in
/// order, over rows already in their bytes.
fn digest_of_rows(rows: &[u8]) -> Digest {
    let mut hasher = blaze_blake3::Hasher::new_derive_key(REACH_DOMAIN);
    for row in rows.chunks_exact(ROW) {
        hasher.update(row);
    }
    Digest(hasher.finalize())
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn digest_at(bytes: &[u8], at: usize) -> Digest {
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[at..at + 32]);
    Digest(out)
}
