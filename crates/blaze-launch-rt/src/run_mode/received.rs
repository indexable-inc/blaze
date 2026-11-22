//! The records the parent sent inline (blaze-launch wire/bytes.rs, `Down::Bytes`): assembled
//! under a hard bound, checked once against the id they were sent under, and held for the run.
//!
//! One message's shape (a part not empty, within its total, a total within `BYTES_RECORD_MOST`)
//! was checked by its decode; this machine checks what spans messages, each breach a typed
//! refusal naming the record ([`SentRefused`], under `RunModeError::Sent`), which stops the
//! program (rule 18; the code review's "Trust" rule: no byte is placed that was not checked):
//!
//! - a record's first part is at 0, and the record is not held already;
//! - its `total` with everything held stays within `BYTES_HELD_MOST`, checked BEFORE the one
//!   allocation of exactly `total` bytes (`try_reserve_exact`: a refused allocation is a
//!   refusal, never an abort);
//! - its parts come consecutively, each at the length received so far, all of one `total`, no
//!   part of another record and no other message between them (`RunMode::down`);
//! - once whole, its BLAKE3 is the id it was sent under (the same check the file path makes,
//!   run_mode.rs `Folder::read_into`).
//!
//! WHERE THE BYTES LIVE AND WHEN THEY ARE FREED: each whole record is one block of exactly its
//! length in the run mode's allocator (`RunMode::alloc`, the launcher's heap), held in [`Received`]
//! until the run mode is dropped at the process's end. Not freed at placement: the parent sends a
//! record's bytes once per run (blaze-cli run_bytes.rs dedupes by id), and one id is placed more
//! than once when two statics or two slots have equal bytes (a record's id is its content), so a
//! later placement of the same id reads the same block. The bound is `BYTES_HELD_MOST` (the code
//! reach and the data area together: more than that could never be placed).
//!
//! One run reads its records one way: once a record was read from the store's files (the file
//! path, today's control), a `Bytes` is refused; once a `Bytes` came, a record that was never
//! sent is refused by name rather than looked for in a file ([`Received::bytes_of`]). Nothing
//! mixes the two, so neither is the other's fallback (rule 3).

use core::cell::Cell;
use core::fmt;

use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::BYTES_HELD_MOST;
use blaze_own::{AVec, Alloc, Vec};

use super::{RecordSource, RunModeError, Short};

/// Why the parent's inline bytes for a record are refused (module doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SentRefused {
    /// The record's first part is at `at`, not 0.
    FirstAt { at: u64 },
    /// A part at `at`, where `want` bytes of the record have come.
    OutOfOrder { at: u64, want: u64 },
    /// A part says the record is `total` bytes; its first part said `was`.
    TotalChanged { total: u64, was: u64 },
    /// A part of this record while record `assembling` is not whole.
    Interleaved { assembling: RecordId },
    /// Another message came while the record had `got` of its `total` bytes.
    Partial { got: u64, total: u64 },
    /// The record is held already: the parent sends each record once a run.
    Again,
    /// The record's `total` with the `held` bytes held is past `BYTES_HELD_MOST`.
    HeldFull { total: u64, held: u64 },
    /// The allocator refused the record's `total` bytes.
    NoRoom { total: u64 },
    /// The whole record's BLAKE3 is `got`, not its id.
    Digest { got: [u8; 32] },
    /// A run whose records come inline named one that was never sent.
    NotSent,
    /// A `Bytes` after this run read a record from the store's files.
    AfterFiles,
}

impl fmt::Display for SentRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SentRefused::FirstAt { at } => write!(f, "its first part is at byte {at}, not 0"),
            SentRefused::OutOfOrder { at, want } => write!(f, "a part at byte {at}, where {want} bytes have come (parts come in order)"),
            SentRefused::TotalChanged { total, was } => write!(f, "a part says it is {total} bytes, its first said {was}"),
            SentRefused::Interleaved { assembling } => {
                write!(f, "a part of it came while record {} is not whole (a record's parts come together)", Short(&assembling.0))
            }
            SentRefused::Partial { got, total } => {
                write!(f, "another message came when {got} of its {total} bytes had (the record was cut short)")
            }
            SentRefused::Again => write!(f, "it was sent a second time (the parent sends a record once a run)"),
            SentRefused::HeldFull { total, held } => write!(
                f,
                "its {total} bytes with the {held} held are past the {BYTES_HELD_MOST} bytes a run holds (blaze-launch wire/bytes.rs BYTES_HELD_MOST)"
            ),
            SentRefused::NoRoom { total } => write!(f, "the launcher's allocator refused its {total} bytes"),
            SentRefused::Digest { got } => write!(f, "its bytes hash to {}, not to its id", Short(&got)),
            SentRefused::NotSent => write!(f, "it is named, and its bytes were never sent (this run's records come inline)"),
            SentRefused::AfterFiles => write!(f, "its bytes came inline after this run read records from the store's files (one run reads one way)"),
        }
    }
}

/// The record being assembled: its id, its total, its bytes so far (capacity exactly `total`).
struct Open<'a> {
    record: RecordId,
    total: u64,
    bytes: AVec<'a, u8>,
}

/// The records sent inline (module doc).
pub(super) struct Received<'a> {
    /// Each whole record, sorted by id: checked against it when it became whole.
    held: AVec<'a, ([u8; 32], AVec<'a, u8>)>,
    open: Option<Open<'a>>,
    /// Bytes held and being assembled: each record counted whole at its first part.
    counted: u64,
    /// Whether this run read a record from the store's files ([`Received::bytes_of`]).
    files: Cell<bool>,
    alloc: Alloc<'a>,
}

impl<'a> Received<'a> {
    pub(super) fn new(alloc: Alloc<'a>) -> Received<'a> {
        Received { held: Vec::new_in(alloc), open: None, counted: 0, files: Cell::new(false), alloc }
    }

    /// Whether this run's records come inline: a `Bytes` came.
    fn inline(&self) -> bool {
        self.open.is_some() || !self.held.is_empty()
    }

    /// The record being assembled and how far, if one is: a message other than its next part
    /// is then refused (`SentRefused::Partial`).
    pub(super) fn assembling(&self) -> Option<RunModeError> {
        self.open.as_ref().map(|open| RunModeError::Sent {
            record: open.record,
            why: SentRefused::Partial { got: open.bytes.len() as u64, total: open.total },
        })
    }

    /// One part (the module doc's rules), its shape already checked by its decode
    /// (`part` not empty, `at + part.len() <= total`).
    pub(super) fn part(&mut self, record: RecordId, at: u64, total: u64, part: &[u8]) -> Result<(), RunModeError> {
        let refused = |why| RunModeError::Sent { record, why };
        if self.files.get() {
            return Err(refused(SentRefused::AfterFiles));
        }
        if self.open.is_none() {
            if at != 0 {
                return Err(refused(SentRefused::FirstAt { at }));
            }
            if self.find(record).is_ok() {
                return Err(refused(SentRefused::Again));
            }
            // `counted` never passes the bound, so the difference cannot wrap.
            if total > BYTES_HELD_MOST - self.counted {
                return Err(refused(SentRefused::HeldFull { total, held: self.counted }));
            }
            let mut bytes: AVec<'a, u8> = Vec::new_in(self.alloc);
            // `total <= BYTES_RECORD_MOST` (the decode), which a usize holds on the 64-bit
            // hosts the launcher runs on.
            bytes.try_reserve_exact(total as usize).map_err(|_| refused(SentRefused::NoRoom { total }))?;
            self.counted += total;
            self.open = Some(Open { record, total, bytes });
        }
        let Some(open) = self.open.as_mut() else { unreachable!("a record was opened just above") };
        if open.record != record {
            return Err(refused(SentRefused::Interleaved { assembling: open.record }));
        }
        if open.total != total {
            return Err(refused(SentRefused::TotalChanged { total, was: open.total }));
        }
        let want = open.bytes.len() as u64;
        if at != want {
            return Err(refused(SentRefused::OutOfOrder { at, want }));
        }
        // Within the capacity reserved: `at + part.len() <= total`, so no reallocation.
        open.bytes.extend_from_slice(part);
        if let Some(whole) = self.open.take_if(|open| open.bytes.len() as u64 == open.total) {
            let got = blaze_blake3::hash(&whole.bytes);
            if got != record.0 {
                return Err(refused(SentRefused::Digest { got }));
            }
            let at = self.find(record).expect_err("a held record was refused at its first part");
            self.held.insert_shift(at, (record.0, whole.bytes));
        }
        Ok(())
    }

    fn find(&self, record: RecordId) -> Result<usize, usize> {
        self.held.binary_search_by(|(id, _)| id.cmp(&record.0))
    }

    /// The bytes of a held record, checked when they became whole.
    fn held(&self, record: RecordId) -> Option<&[u8]> {
        self.find(record).ok().map(|at| self.held[at].1.as_slice())
    }

    /// Record `record`'s bytes, read the one way this run reads: held, in a run whose records
    /// come inline (one never sent is `SentRefused::NotSent`); else from `source`, the store's
    /// files, into `scratch` (and from then on a `Bytes` is refused).
    pub(super) fn bytes_of<'s>(&'s self, record: RecordId, source: &'s impl RecordSource, scratch: &'s mut std::vec::Vec<u8>) -> Result<&'s [u8], RunModeError> {
        match self.inline() {
            true => self.held(record).ok_or(RunModeError::Sent { record, why: SentRefused::NotSent }),
            false => {
                self.files.set(true);
                source.read_into(record, scratch)
            }
        }
    }

    /// Record `record`'s bytes a second time in one placement (`RunMode::place_statics`'s fill
    /// pass): held, or what the first read left in `scratch`, or (a source that lends without
    /// copying left `scratch` empty) the source's again.
    pub(super) fn again<'s>(&'s self, record: RecordId, source: &'s impl RecordSource, scratch: &'s [u8]) -> Result<&'s [u8], RunModeError> {
        match self.inline() {
            true => self.held(record).ok_or(RunModeError::Sent { record, why: SentRefused::NotSent }),
            false if scratch.is_empty() => source.bytes(record),
            false => Ok(scratch),
        }
    }
}

#[cfg(test)]
#[path = "received_tests.rs"]
mod tests;
