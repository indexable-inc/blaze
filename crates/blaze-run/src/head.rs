//! The head record of a suite (docs/v2/plan/design/test.md, "End state", piece 3): one row per
//! test with its last outcome, the address its kept pass is filed at, how long its last run took
//! and the range of its asks in one shared asks table; and the read table, each outside read any
//! test asked with the digest it answered when the head was written. A `blaze test` request
//! starts from it: [`Head::kept_into`] gives [`Plan::decide`](crate::plan::Plan::decide) its
//! `kept` rows (already sorted by test), [`Head::moved_reads`] answers the read table now and
//! gives the reads that moved (R), and [`Head::asks`] gives a candidate's last asks, which the
//! driver answers now to compute the address it looks up.
//!
//! The form (integers little-endian), lengths before every variable part:
//!
//! - a 24-byte header: `BZTH`, the version, the row count, the ask count, the read count, zero;
//! - the rows, FIXED 88 bytes, sorted by test id and each once (so [`Head::row`] is one binary
//!   search over the mapped bytes): the test's 32-byte id; the outcome's tag (0 a pass, 1 an
//!   unsettled pass, 2 a failure, 3 not kept); the not-kept reason (zero unless not kept); six
//!   zero bytes; the 32-byte address (zero unless a pass); the last run's micros (u64); the
//!   first ask's index (u32); the ask count (u32). Rows' ask ranges follow one another in row
//!   order, so the asks table has one form for one head;
//! - the asks, FIXED 40 bytes, as postings.rs writes an ask: the tag (0 a function, 1 a read),
//!   seven zero bytes, the 32-byte name; within one test's range sorted by their bytes and each
//!   once, which is [`Ask`] order;
//! - the read table, FIXED 64 bytes, sorted by name and each once: the read's 32-byte name, the
//!   32-byte digest it answered. Every read a test asks has its row here, else an edit of that
//!   read could never be seen to move it.
//!
//! [`encode`] takes rows and reads in any order and sorts them at this boundary, so equal input
//! gives equal bytes on any machine (rule 25). Nothing of the machine is in the bytes: no path,
//! no time of day, no memory address. [`Head::open`] checks the header, every length, every tag,
//! every order and every range ONCE and refuses bad bytes with a typed [`HeadError`] naming the
//! row, ask or read; a lookup then checks nothing (rule 74) and reads in place, copying nothing.
//!
//! Why `micros` does not move a result's address: the address is H(test id, asks, answers), what
//! the run is a function of. How long a run took is a measurement of the machine and its load,
//! different on every run of the same code; in the address it would make no two runs, and no
//! developer's run and CI's, ever meet at one address, so nothing would be kept or shared. It is
//! data: the plan starts the longest last run first and the report prints it. It sits in the
//! head's row beside the address, never in it.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::plan::{Address, Kept, Last, NotKept};
use crate::postings::{Ask, FnName, ReadName};
use crate::{Hex, TestId};

/// What an outside read answered when the head was written: the digest of what it read.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ReadDigest(pub [u8; 32]);

/// One test's row, as [`encode`] takes it: its asks in any order and with repeats.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeadRow<'i> {
    pub test: TestId,
    pub last: Last,
    pub micros: u64,
    pub asks: &'i [Ask],
}

const MAGIC: [u8; 4] = *b"BZTH";
const VERSION: u32 = 1;
const HEADER: usize = 24;
const ROW: usize = 88;
const ASK: usize = 40;
const READ: usize = 64;
const TAG_AT: usize = 32;
const REASON_AT: usize = 33;
const ADDRESS_AT: usize = 40;
const MICROS_AT: usize = 72;
const START_AT: usize = 80;
const COUNT_AT: usize = 84;
const PASSED: u8 = 0;
const UNSETTLED: u8 = 1;
const FAILED: u8 = 2;
const NOT_KEPT: u8 = 3;

/// Why a head record was not written or not read. Each names the row, ask or read and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadError {
    /// The bytes do not start with this record's magic and version (or are shorter than its
    /// header).
    NotThisRecord,
    /// The length is not the header plus the rows, asks and reads it counts, or the header's
    /// last word is not zero.
    Length { tests: usize, asks: usize, reads: usize, len: usize },
    /// A row's outcome tag or not-kept reason is unknown, its padding is not zero, or a failure
    /// or a not-kept row has an address.
    Outcome { row: usize },
    /// A row is not after the one before it: rows are sorted by test id and each once.
    Order { row: usize },
    /// A row's asks do not start where the row before it ended, or run past the asks table
    /// (`row` equal to the row count: the rows do not cover the table).
    AsksRange { row: usize },
    /// An ask's tag is neither a function (0) nor a read (1), or its padding is not zero.
    AskTag { ask: usize },
    /// An ask is not after the one before it within its test's range: sorted and each once.
    AskOrder { ask: usize },
    /// An ask names a read the read table does not hold.
    ReadMissing { ask: usize },
    /// A read is not after the one before it: the read table is sorted by name and each once.
    ReadOrder { read: usize },
    /// Two rows given for one test (rule 21: two values under one key are refused).
    TwoRows { test: TestId },
    /// Two digests given for one read (rule 21).
    TwoDigests { read: ReadName },
    /// A test asks a read that the reads given do not answer.
    Unanswered { test: TestId, read: ReadName },
    /// More rows, asks or reads than a u32 counts.
    TooMany { what: &'static str, count: usize },
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HeadError::NotThisRecord => write!(f, "not a suite head record (magic `BZTH`, version {VERSION})"),
            HeadError::Length { tests, asks, reads, len } => write!(
                f,
                "suite head of {len} bytes, but its header counts {tests} rows of {ROW}, {asks} asks of {ASK} and \
                 {reads} reads of {READ} bytes after {HEADER}"
            ),
            HeadError::Outcome { row } => {
                write!(f, "suite head row {row}: an unknown outcome or reason, padding not zero, or an address on a row with no pass")
            }
            HeadError::Order { row } => write!(
                f,
                "suite head row {row} is not after row {}: rows are sorted by test and each once (rule 25)",
                row.saturating_sub(1)
            ),
            HeadError::AsksRange { row } => {
                write!(f, "suite head row {row}: its asks do not start where the row before ended, or run past the table")
            }
            HeadError::AskTag { ask } => write!(f, "suite head ask {ask}: an unknown ask tag or padding that is not zero"),
            HeadError::AskOrder { ask } => {
                write!(f, "suite head ask {ask} is not after the ask before it in its test's range: sorted and each once (rule 25)")
            }
            HeadError::ReadMissing { ask } => write!(f, "suite head ask {ask} names a read the head's read table does not hold"),
            HeadError::ReadOrder { read } => {
                write!(f, "suite head read {read} is not after the read before it: sorted by name and each once (rule 25)")
            }
            HeadError::TwoRows { test } => {
                write!(f, "two suite head rows for test {} (rule 21: two values under one key)", Hex(&test.0))
            }
            HeadError::TwoDigests { read } => {
                write!(f, "two digests for read {} in one suite head (rule 21: two values under one key)", Hex(&read.0))
            }
            HeadError::Unanswered { test, read } => write!(
                f,
                "test {} asks read {}, which the suite head's reads do not answer",
                Hex(&test.0),
                Hex(&read.0)
            ),
            HeadError::TooMany { what, count } => write!(f, "{count} suite head {what}: more than a u32 counts"),
        }
    }
}

impl core::error::Error for HeadError {}

/// The record's bytes for `rows` and `reads`, each in any order: rows sorted by test, each
/// test's asks sorted and each once, reads sorted by name and each once, so equal input gives
/// equal bytes. A test given twice, a read given two digests or a read asked and not given is
/// refused. The buffers are in `alloc`.
pub fn encode<'b>(rows: &[HeadRow<'_>], reads: &[(ReadName, ReadDigest)], alloc: Alloc<'b>) -> Result<AVec<'b, u8>, HeadError> {
    let mut order: AVec<'b, (TestId, usize)> = Vec::with_capacity_in(rows.len(), alloc);
    for (at, row) in rows.iter().enumerate() {
        order.push((row.test, at));
    }
    order.sort_unstable();
    for pair in order.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(HeadError::TwoRows { test: pair[0].0 });
        }
    }
    let mut table: AVec<'b, (ReadName, ReadDigest)> = Vec::from_slice_in(reads, alloc);
    table.sort_unstable();
    table.dedup();
    for pair in table.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(HeadError::TwoDigests { read: pair[0].0 });
        }
    }
    let mut asks: AVec<'b, Ask> = Vec::new_in(alloc);
    let mut ranges: AVec<'b, (u32, u32)> = Vec::with_capacity_in(order.len(), alloc);
    let mut scratch: AVec<'b, Ask> = Vec::new_in(alloc);
    for (test, at) in order.iter() {
        scratch.clear();
        scratch.extend_from_slice(rows[*at].asks);
        scratch.sort_unstable();
        scratch.dedup();
        for ask in scratch.iter() {
            if let Ask::Read(read) = ask
                && table.binary_search_by(|(name, _)| name.cmp(read)).is_err()
            {
                return Err(HeadError::Unanswered { test: *test, read: *read });
            }
        }
        let start = count(asks.len(), "asks")?;
        ranges.push((start, count(scratch.len(), "asks")?));
        asks.extend_from_slice(&scratch);
    }
    let (tests, ask_count, read_count) = (count(order.len(), "rows")?, count(asks.len(), "asks")?, count(table.len(), "reads")?);
    let mut out = Vec::with_capacity_in(HEADER + order.len() * ROW + asks.len() * ASK + table.len() * READ, alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&tests.to_le_bytes());
    out.extend_from_slice(&ask_count.to_le_bytes());
    out.extend_from_slice(&read_count.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for ((test, at), (start, n)) in order.iter().zip(ranges.iter()) {
        let (tag, reason, address) = outcome_bytes(rows[*at].last);
        out.extend_from_slice(&test.0);
        out.push(tag);
        out.push(reason);
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&address);
        out.extend_from_slice(&rows[*at].micros.to_le_bytes());
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
    }
    for ask in asks.iter() {
        let (tag, name) = ask_bytes(ask);
        out.push(tag);
        out.extend_from_slice(&[0; 7]);
        out.extend_from_slice(name);
    }
    for (name, digest) in table.iter() {
        out.extend_from_slice(&name.0);
        out.extend_from_slice(&digest.0);
    }
    Ok(out)
}

/// A test's row as [`Head::row`] found it: its last outcome and micros, and where its asks are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowRef {
    pub kept: Kept,
    start: usize,
    count: usize,
}

/// A suite head record, open: checked once, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Head<'r> {
    rows: &'r [u8],
    asks: &'r [u8],
    reads: &'r [u8],
    tests: usize,
}

impl<'r> Head<'r> {
    /// No head: a suite with no test run yet.
    pub const EMPTY: Head<'static> = Head { rows: &[], asks: &[], reads: &[], tests: 0 };

    /// Checks the header and the length; then the read table's order; then every ask's tag and
    /// padding, and that a read it names is in the read table; then every row's outcome, its
    /// order and its ask range, and each range's order. Nothing is checked again on a lookup.
    pub fn open(bytes: &'r [u8]) -> Result<Head<'r>, HeadError> {
        if bytes.len() < HEADER || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
            return Err(HeadError::NotThisRecord);
        }
        let (tests, asks, reads) = (le32(bytes, 8) as usize, le32(bytes, 12) as usize, le32(bytes, 16) as usize);
        let rows_end = tests.checked_mul(ROW).and_then(|n| n.checked_add(HEADER));
        let asks_end = asks.checked_mul(ASK).and_then(|n| rows_end.and_then(|end| end.checked_add(n)));
        let reads_end = reads.checked_mul(READ).and_then(|n| asks_end.and_then(|end| end.checked_add(n)));
        let (Some(rows_end), Some(asks_end), Some(len)) = (rows_end, asks_end, reads_end) else {
            return Err(HeadError::Length { tests, asks, reads, len: bytes.len() });
        };
        if len != bytes.len() || le32(bytes, 20) != 0 {
            return Err(HeadError::Length { tests, asks, reads, len: bytes.len() });
        }
        let head = Head {
            rows: &bytes[HEADER..rows_end],
            asks: &bytes[rows_end..asks_end],
            reads: &bytes[asks_end..],
            tests,
        };
        for read in 1..reads {
            if head.reads[(read - 1) * READ..(read - 1) * READ + 32] >= head.reads[read * READ..read * READ + 32] {
                return Err(HeadError::ReadOrder { read });
            }
        }
        for ask in 0..asks {
            let entry = &head.asks[ask * ASK..(ask + 1) * ASK];
            if entry[0] > 1 || entry[1..8].iter().any(|b| *b != 0) {
                return Err(HeadError::AskTag { ask });
            }
            if entry[0] == 1 && head.find_read(&entry[8..ASK]).is_none() {
                return Err(HeadError::ReadMissing { ask });
            }
        }
        let mut next = 0;
        for row in 0..tests {
            let bytes = &head.rows[row * ROW..(row + 1) * ROW];
            if last_of(bytes).is_none() {
                return Err(HeadError::Outcome { row });
            }
            if row > 0 && head.rows[(row - 1) * ROW..(row - 1) * ROW + 32] >= bytes[..32] {
                return Err(HeadError::Order { row });
            }
            let (start, n) = (le32(bytes, START_AT) as usize, le32(bytes, COUNT_AT) as usize);
            if start != next || n > asks - next {
                return Err(HeadError::AsksRange { row });
            }
            for ask in start + 1..start + n {
                if head.asks[(ask - 1) * ASK..ask * ASK] >= head.asks[ask * ASK..(ask + 1) * ASK] {
                    return Err(HeadError::AskOrder { ask });
                }
            }
            next = start + n;
        }
        if next != asks {
            return Err(HeadError::AsksRange { row: tests });
        }
        Ok(head)
    }

    /// How many tests have a row.
    pub const fn len(&self) -> usize {
        self.tests
    }

    pub const fn is_empty(&self) -> bool {
        self.tests == 0
    }

    /// The row of `test`, if the head has one: one binary search over the rows' ids, in place.
    pub fn row(&self, test: TestId) -> Option<RowRef> {
        let (mut low, mut high) = (0, self.tests);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.rows[mid * ROW..mid * ROW + 32].cmp(&test.0[..]) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(self.row_at(mid)),
            }
        }
        None
    }

    /// Every row, in test id order.
    pub fn rows(&self) -> impl Iterator<Item = RowRef> + use<'r> {
        let head = *self;
        (0..head.tests).map(move |at| head.row_at(at))
    }

    /// The asks of `row`'s last run, in [`Ask`] order, each once.
    pub fn asks(&self, row: RowRef) -> Asks<'r> {
        let asks: &'r [u8] = self.asks;
        Asks { bytes: &asks[row.start * ASK..(row.start + row.count) * ASK] }
    }

    /// Pushes every row's [`Kept`] to `out` in test id order: the `kept` that
    /// [`Plan::decide`](crate::plan::Plan::decide) takes, sorted as it requires.
    pub fn kept_into(&self, out: &mut AVec<'_, Kept>) {
        for row in self.rows() {
            out.push(row.kept);
        }
    }

    /// The read table: each read any test asked and the digest it answered, by name.
    pub fn reads(&self) -> impl Iterator<Item = (ReadName, ReadDigest)> + use<'r> {
        let reads: &'r [u8] = self.reads;
        reads.chunks_exact(READ).map(|row| (ReadName(word(row, 0)), ReadDigest(word(row, 32))))
    }

    /// Answers every read of the table now through `answer` (the driver's: it reads the file,
    /// the variable or the host fact and digests it) and pushes to `out`, in name order, each read
    /// whose digest moved: the R of a request. The first error `answer` gives stops it.
    pub fn moved_reads<E>(
        &self,
        mut answer: impl FnMut(ReadName) -> Result<ReadDigest, E>,
        out: &mut AVec<'_, Ask>,
    ) -> Result<(), E> {
        for (name, then) in self.reads() {
            if answer(name)? != then {
                out.push(Ask::Read(name));
            }
        }
        Ok(())
    }

    fn row_at(&self, at: usize) -> RowRef {
        let bytes = &self.rows[at * ROW..(at + 1) * ROW];
        let Some(last) = last_of(bytes) else {
            unreachable!("suite head row {at}: `Head::open` refused every row whose outcome does not decode (rule 74)")
        };
        let kept = Kept { test: TestId(word(bytes, 0)), last, micros: le64(bytes, MICROS_AT) };
        RowRef { kept, start: le32(bytes, START_AT) as usize, count: le32(bytes, COUNT_AT) as usize }
    }

    /// The read table's row of `name`, by one binary search.
    fn find_read(&self, name: &[u8]) -> Option<usize> {
        let (mut low, mut high) = (0, self.reads.len() / READ);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.reads[mid * READ..mid * READ + 32].cmp(name) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

/// A test's asks, read in place from the head's asks table.
#[derive(Clone, Copy, Debug)]
pub struct Asks<'r> {
    bytes: &'r [u8],
}

impl Iterator for Asks<'_> {
    type Item = Ask;

    fn next(&mut self) -> Option<Ask> {
        if self.bytes.len() < ASK {
            return None;
        }
        let (entry, rest) = self.bytes.split_at(ASK);
        self.bytes = rest;
        let name = word(entry, 8);
        // `Head::open` refused every tag but 0 and 1 (rule 74).
        Some(match entry[0] {
            0 => Ask::Fn(FnName(name)),
            _ => Ask::Read(ReadName(name)),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.bytes.len() / ASK;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Asks<'_> {}

/// A row whose last kept pass one of whose asks moved in a request that did not ask the test:
/// what the driver writes for [`Effect::Unsettle`](crate::plan::Effect::Unsettle). Any other
/// outcome already runs again and stays as it is.
pub const fn unsettle(last: Last) -> Last {
    match last {
        Last::Passed { address } => Last::Unsettled { address },
        other => other,
    }
}

fn outcome_bytes(last: Last) -> (u8, u8, [u8; 32]) {
    match last {
        Last::Passed { address } => (PASSED, 0, address.0),
        Last::Unsettled { address } => (UNSETTLED, 0, address.0),
        Last::Failed => (FAILED, 0, [0; 32]),
        Last::NotKept(why) => (NOT_KEPT, reason_byte(why), [0; 32]),
    }
}

const fn reason_byte(why: NotKept) -> u8 {
    match why {
        NotKept::Clock => 0,
        NotKept::Network => 1,
        NotKept::WroteOutside => 2,
        NotKept::Foreign => 3,
        NotKept::Unsandboxed => 4,
        NotKept::Descriptor => 5,
        NotKept::Threads => 6,
        NotKept::Untraced => 7,
    }
}

const fn reason_of(byte: u8) -> Option<NotKept> {
    match byte {
        0 => Some(NotKept::Clock),
        1 => Some(NotKept::Network),
        2 => Some(NotKept::WroteOutside),
        3 => Some(NotKept::Foreign),
        4 => Some(NotKept::Unsandboxed),
        5 => Some(NotKept::Descriptor),
        6 => Some(NotKept::Threads),
        7 => Some(NotKept::Untraced),
        _ => None,
    }
}

/// A row's outcome, or `None` when its tag, reason, padding or address is not this form's.
fn last_of(row: &[u8]) -> Option<Last> {
    if row[REASON_AT + 1..ADDRESS_AT].iter().any(|b| *b != 0) {
        return None;
    }
    let address = Address(word(row, ADDRESS_AT));
    let no_address = address.0 == [0; 32];
    match (row[TAG_AT], row[REASON_AT]) {
        (PASSED, 0) => Some(Last::Passed { address }),
        (UNSETTLED, 0) => Some(Last::Unsettled { address }),
        (FAILED, 0) if no_address => Some(Last::Failed),
        (NOT_KEPT, reason) if no_address => reason_of(reason).map(Last::NotKept),
        _ => None,
    }
}

// The ask's tag and name as postings.rs writes them (its `Ask::tag` and `Ask::name` are private
// to it; one form, so an ask's bytes are equal in both records).
const fn ask_bytes(ask: &Ask) -> (u8, &[u8; 32]) {
    match ask {
        Ask::Fn(FnName(name)) => (0, name),
        Ask::Read(ReadName(name)) => (1, name),
    }
}

fn count(n: usize, what: &'static str) -> Result<u32, HeadError> {
    u32::try_from(n).map_err(|_| HeadError::TooMany { what, count: n })
}

// `word`, `le32` and `le64` are the crate's readers of fixed-width little-endian fields (wire.rs
// reads with them too); each takes a range its caller checked.
pub(crate) fn word(bytes: &[u8], at: usize) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(&bytes[at..at + 32]);
    out
}

pub(crate) fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

pub(crate) fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut out = [0; 8];
    out.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(out)
}

#[cfg(test)]
mod tests {
    use blaze_own::{Alloc, Vec as Own};

    use super::{Head, HeadError, HeadRow, ReadDigest, encode};
    use crate::plan::{Address, Asked, Basis, Config, Effect, Filter, Ignored, Kept, Last, NotKept, Outcome, Plan, ShouldPanic};
    use crate::postings::{Ask, FnName, Postings, ReadName};
    use crate::{Nanos, TestId};

    fn key(n: u64) -> [u8; 32] {
        let mut out = [0; 32];
        out[..8].copy_from_slice(&n.to_le_bytes());
        out
    }

    /// A suite head's bytes are a function of its rows and reads alone, read back exactly, and
    /// refused by name when short; an unsettled pass survives the round trip and makes the plan
    /// look the test up rather than keep its pass (test.md, "End state": "A request that filters
    /// a test out leaves that test's kept pass unchecked ... the head-record piece (3) must carry
    /// that state").
    ///
    /// Expected line (`lanecheck.sh blaze-run test head_round_trips_in_any_order`):
    /// `test head::tests::head_round_trips_in_any_order_and_refuses_every_truncation ... ok`
    #[test]
    fn head_round_trips_in_any_order_and_refuses_every_truncation() {
        let (f0, f1, f2) = (Ask::Fn(FnName(key(1))), Ask::Fn(FnName(key(2))), Ask::Fn(FnName(key(3))));
        let (r0, r1) = (ReadName(key(100)), ReadName(key(101)));
        let (t0, t1, t2, t3) = (TestId(key(200)), TestId(key(201)), TestId(key(202)), TestId(key(203)));
        let reads = [(r1, ReadDigest(key(51))), (r0, ReadDigest(key(50)))];
        let a = HeadRow { test: t0, last: Last::Passed { address: Address(key(300)) }, micros: 40, asks: &[f2, f0, Ask::Read(r1)] };
        let b = HeadRow { test: t1, last: Last::Unsettled { address: Address(key(301)) }, micros: 7, asks: &[f1] };
        let c = HeadRow { test: t2, last: Last::Failed, micros: 9, asks: &[Ask::Read(r0), f1, f1] };
        let d = HeadRow { test: t3, last: Last::NotKept(NotKept::Clock), micros: 0, asks: &[] };
        let bytes = encode(&[a, b, c, d], &reads, Alloc::global()).expect("four rows encode");
        // Another insertion order, asks reordered and repeated, reads reordered and repeated.
        let c2 = HeadRow { asks: &[f1, Ask::Read(r0)], ..c };
        let a2 = HeadRow { asks: &[Ask::Read(r1), f0, f2, f0], ..a };
        let reads2 = [(r0, ReadDigest(key(50))), (r1, ReadDigest(key(51))), (r0, ReadDigest(key(50)))];
        let again = encode(&[d, c2, a2, b], &reads2, Alloc::global()).expect("the same rows encode");
        assert_eq!(bytes.as_slice(), again.as_slice(), "equal rows in another order give equal bytes");

        // Open, then read back equals the input.
        let head = Head::open(bytes.as_slice()).expect("written bytes open");
        assert_eq!(head.len(), 4);
        for (row, asks) in [(a, &[f0, f2, Ask::Read(r1)][..]), (b, &[f1][..]), (c, &[f1, Ask::Read(r0)][..]), (d, &[][..])] {
            let found = head.row(row.test).expect("every written test has a row");
            assert_eq!(found.kept, Kept { test: row.test, last: row.last, micros: row.micros });
            let mut back = Own::new_in(Alloc::global());
            for ask in head.asks(found) {
                back.push(ask);
            }
            assert_eq!(back.as_slice(), asks, "asks of {:?} sorted and each once", row.test);
        }
        assert!(head.row(TestId(key(250))).is_none());
        let mut table = Own::new_in(Alloc::global());
        for read in head.reads() {
            table.push(read);
        }
        assert_eq!(table.as_slice(), &[(r0, ReadDigest(key(50))), (r1, ReadDigest(key(51)))]);
        let mut moved = Own::new_in(Alloc::global());
        let now = |name: ReadName| -> Result<ReadDigest, HeadError> {
            Ok(if name == r1 { ReadDigest(key(52)) } else { ReadDigest(key(50)) })
        };
        head.moved_reads(now, &mut moved).expect("every read answers");
        assert_eq!(moved.as_slice(), &[Ask::Read(r1)], "only the read whose digest moved");

        // Every truncation is refused by name, never a panic.
        for len in 0..bytes.len() {
            let want = match len < 24 {
                true => HeadError::NotThisRecord,
                false => HeadError::Length { tests: 4, asks: 6, reads: 2, len },
            };
            assert_eq!(Head::open(&bytes.as_slice()[..len]).map(|h| h.len()), Err(want), "truncated at {len}");
        }
        // A failure carrying an address is not this form.
        let mut bad = bytes.as_slice().to_vec();
        let failed_row = 24 + 2 * 88;
        bad[failed_row + 40] = 1;
        assert_eq!(Head::open(&bad).map(|h| h.len()), Err(HeadError::Outcome { row: 2 }));
        // Two values under one key are refused at the boundary.
        assert_eq!(encode(&[a, a2], &reads, Alloc::global()).map(|v| v.len()), Err(HeadError::TwoRows { test: t0 }));
        assert_eq!(
            encode(&[a], &[(r1, ReadDigest(key(51))), (r1, ReadDigest(key(9)))], Alloc::global()).map(|v| v.len()),
            Err(HeadError::TwoDigests { read: r1 }),
        );
        assert_eq!(encode(&[c], &[], Alloc::global()).map(|v| v.len()), Err(HeadError::Unanswered { test: t2, read: r0 }));

        // The plan, started from the head with nothing moved: the pass stands, the unsettled pass
        // is looked up, the failure and the not-kept run again.
        let config = Config {
            width: 4,
            deadline: Nanos(1_000),
            run_all: false,
            ignored: Ignored::Skip,
            benches: false,
            filter: Filter::All,
            skip: &[],
        };
        let mut kept = Own::new_in(Alloc::global());
        head.kept_into(&mut kept);
        let asked = [t0, t1, t2, t3].map(|test| Asked { test, path: "m::t", ignore: None, should_panic: ShouldPanic::No, bench: false });
        let mut plan = Plan::new(config, Alloc::global()).expect("width 4");
        let mut out = Own::new_in(Alloc::global());
        plan.decide(&asked, kept.as_slice(), &[], &Postings::EMPTY, Nanos(0), &mut out).expect("the head's rows are sorted");
        let effects = out.as_slice();
        assert!(effects.contains(&Effect::Report { test: t0, outcome: Outcome::Cached(Basis::Unchanged) }), "{effects:?}");
        assert!(effects.contains(&Effect::Look { test: t1 }), "an unsettled pass is looked up: {effects:?}");
        assert!(!effects.contains(&Effect::Look { test: t0 }), "a settled pass with nothing moved is not: {effects:?}");
        assert!(effects.iter().any(|e| matches!(e, Effect::Start { test, .. } if *test == t2)), "a failure runs: {effects:?}");
        assert!(effects.iter().any(|e| matches!(e, Effect::Start { test, .. } if *test == t3)), "not kept runs: {effects:?}");
    }
}
