//! A [`History`] as bytes for the daemon's image: the same history gives the same bytes, and
//! bytes read back predict what the history they came from predicted.
//!
//! Little-endian: [`MAGIC`]; the table's slots, its clock and the records it dropped; the
//! kinds' peaks as [`Estimates::encode`] gives them, after their length; the kinds' times in
//! order of their numbers, [`KIND_BYTES`] each (kind, 4 zero bytes, seen, mean); then the
//! records in slot order, each bucket's from its first way with no gap, [`RECORD_BYTES`] each
//! (slot, kind, seen, 4 zero bytes, stamp, the key's halves, peak, the four runs).

use std::fmt;

use blaze_bytes::le::{u32 as u32_at, u64 as u64_at};
use blaze_format::cursor::{self, Put as _, Reader};
use blaze_graph::admission::{self, Estimates, TaskKind};
use blaze_ids::radix;

use crate::{
    Config, History,
    kinds::{Time, Times},
    table::{self, RUNS, Record, WAYS, Writer},
};

pub const MAGIC: &[u8; 8] = b"blzhis01";
pub const KIND_BYTES: usize = 24;
pub const RECORD_BYTES: usize = 64;
/// The most slots bytes may say a table had: 1 GiB of records. More is not a table this
/// blaze makes.
pub const MOST_SLOTS: u64 = 1 << 24;

/// Why bytes are not a [`History`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not of this version of [`History::encode`].
    Version,
    Short { at: usize },
    /// The kinds' peaks are not [`Estimates`]' bytes.
    Estimates(admission::DecodeError),
    /// Not as many bytes as `count` records take after the kinds.
    Length { count: u64, bytes: usize },
    /// A table of a size no history has.
    Slots(u64),
    /// A kind not after the one before it.
    KindOrder { kind: u32 },
    /// A kind's times no observations make.
    KindUnsound { kind: u32 },
    /// A record's slot not after the one before it, or past the table.
    SlotOrder { slot: u32 },
    /// A record in a slot not of its key's bucket.
    Bucket { slot: u32 },
    /// A record after an empty way of its bucket, which fills from its first.
    Gap { slot: u32 },
    /// A record no observations make: unseen, stamped after the clock or with another's
    /// stamp, or a run where no run reached.
    Unsound { slot: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Version => write!(f, "a task history of another version"),
            DecodeError::Short { at } => write!(f, "a task history cut short at byte {at}"),
            DecodeError::Estimates(error) => write!(f, "a task history's memory estimates: {error}"),
            DecodeError::Length { count, bytes } => {
                write!(f, "a task history of {count} records in {bytes} bytes")
            }
            DecodeError::Slots(slots) => write!(f, "a task history of a table of {slots} slots"),
            DecodeError::KindOrder { kind } => write!(f, "a task history with kind {kind} out of order"),
            DecodeError::KindUnsound { kind } => {
                write!(f, "a task history with times of kind {kind} that no runs make")
            }
            DecodeError::SlotOrder { slot } => write!(f, "a task history with slot {slot} out of order"),
            DecodeError::Bucket { slot } => {
                write!(f, "a task history with the record in slot {slot} out of its bucket")
            }
            DecodeError::Gap { slot } => {
                write!(f, "a task history with the record in slot {slot} after an empty way of its bucket")
            }
            DecodeError::Unsound { slot } => {
                write!(f, "a task history with a record in slot {slot} that no runs make")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

pub(crate) fn encode(history: &History) -> Vec<u8> {
    let estimates = history.estimates.encode();
    let kinds = history.times.sorted();
    // The table's every slot is the records' room: one allocation, never grown, whose pages
    // past the records are never touched.
    let mut out = Vec::with_capacity(
        MAGIC.len() + 32 + estimates.len() + 8 + kinds.len() * KIND_BYTES + 8 + history.keys.slots() * RECORD_BYTES,
    );
    out.put_bytes(MAGIC);
    out.put_u64(history.keys.slots() as u64);
    out.put_u64(history.keys.clock());
    out.put_u64(history.keys.evicted());
    out.put_bytes_u64(&estimates);
    out.put_u64(kinds.len() as u64);
    for (kind, time) in kinds {
        out.put_u32(kind.0);
        out.put_u32(0);
        out.put_u64(time.seen);
        out.put_u64(time.mean_ns);
    }
    let count_at = out.len();
    out.put_u64(0);
    let mut count = 0u64;
    for (slot, record) in history.keys.records() {
        out.put_bytes(&record_bytes(slot, &record));
        count += 1;
    }
    out.patch_u64(count_at, count);
    out
}

/// A record in slot `slot` as its bytes, built in registers and written in one copy.
fn record_bytes(slot: usize, record: &Record) -> [u8; RECORD_BYTES] {
    let mut bytes = [0; RECORD_BYTES];
    for (at, half) in [slot as u32, record.kind.0, record.seen, 0].into_iter().enumerate() {
        bytes[at * 4..at * 4 + 4].copy_from_slice(&half.to_le_bytes());
    }
    for (at, word) in [record.stamp, record.key[0], record.key[1], record.peak].into_iter().enumerate() {
        bytes[16 + at * 8..24 + at * 8].copy_from_slice(&word.to_le_bytes());
    }
    for (at, run) in record.runs.into_iter().enumerate() {
        bytes[48 + at * 4..52 + at * 4].copy_from_slice(&run.to_le_bytes());
    }
    bytes
}

/// The slot, the pad and the record of [`record_bytes`]' bytes.
fn read_record(bytes: &[u8; RECORD_BYTES]) -> (u32, u32, Record) {
    let mut runs = [0; RUNS];
    for (at, run) in runs.iter_mut().enumerate() {
        *run = u32_at(bytes, 48 + at * 4);
    }
    let record = Record {
        kind: TaskKind(u32_at(bytes, 4)),
        seen: u32_at(bytes, 8),
        stamp: u64_at(bytes, 16),
        key: [u64_at(bytes, 24), u64_at(bytes, 32)],
        peak: u64_at(bytes, 40),
        runs,
    };
    (u32_at(bytes, 0), u32_at(bytes, 12), record)
}

impl From<cursor::Error> for DecodeError {
    fn from(error: cursor::Error) -> DecodeError {
        DecodeError::Short { at: error.at }
    }
}

pub(crate) fn decode(bytes: &[u8], config: &Config) -> Result<History, DecodeError> {
    let mut read = Reader::new(bytes);
    if read.array::<8>()? != MAGIC {
        return Err(DecodeError::Version);
    }
    let slots = read.u64()?;
    if !slots.is_power_of_two() || slots < WAYS as u64 || slots > MOST_SLOTS {
        return Err(DecodeError::Slots(slots));
    }
    let slots = slots as usize;
    let (clock, evicted) = (read.u64()?, read.u64()?);
    let mut estimates = Estimates::decode(read.bytes_u64()?).map_err(DecodeError::Estimates)?;
    estimates.unseen = config.unseen_bytes;

    let kind_count = read.count_u64(KIND_BYTES)?;
    let mut kinds = Vec::with_capacity(kind_count);
    let mut last: Option<u32> = None;
    for _ in 0..kind_count {
        let kind = read.u32()?;
        let pad = read.u32()?;
        let time = Time { seen: read.u64()?, mean_ns: read.u64()? };
        if last.is_some_and(|it| it >= kind) {
            return Err(DecodeError::KindOrder { kind });
        }
        last = Some(kind);
        if pad != 0 || time.seen == 0 {
            return Err(DecodeError::KindUnsound { kind });
        }
        kinds.push((TaskKind(kind), time));
    }

    let count = read.u64()?;
    if count.checked_mul(RECORD_BYTES as u64) != Some(read.left() as u64) {
        return Err(DecodeError::Length { count, bytes: bytes.len() });
    }
    let mut records = Vec::with_capacity(count as usize);
    let mut last: Option<u32> = None;
    for _ in 0..count {
        let (slot, pad, record) = read_record(read.array::<RECORD_BYTES>()?);
        if last.is_some_and(|it| it >= slot) || slot as usize >= slots {
            return Err(DecodeError::SlotOrder { slot });
        }
        if slot as usize % WAYS != 0 && last != Some(slot - 1) {
            return Err(DecodeError::Gap { slot });
        }
        last = Some(slot);
        if table::bucket_in(slots, record.kind, record.key) != slot as usize - slot as usize % WAYS {
            return Err(DecodeError::Bucket { slot });
        }
        if pad != 0 || !record.is_sound() || record.stamp > clock {
            return Err(DecodeError::Unsound { slot });
        }
        records.push((slot as usize, record));
    }
    // Each record's stamp and its index in `records`, which is in slot order; the sort is
    // stable, so records of one stamp stay in slot order.
    let mut stamps: Vec<(u64, usize)> = records.iter().enumerate().map(|(at, (_, it))| (it.stamp, at)).collect(); // alloc: once per daemon start
    radix::sort_copy_by_key(&mut stamps, |&(stamp, _)| stamp);
    if let Some(pair) = stamps.windows(2).find(|it| it[0].0 == it[1].0) {
        return Err(DecodeError::Unsound { slot: records[pair[1].1].0 as u32 });
    }

    let mut keys = Writer::new(config.slots);
    keys.set_counts(clock, evicted);
    if config.slots == slots {
        keys.put_all(&records);
    } else {
        // Oldest first, so where a bucket of the new size is full the newest stay.
        keys.place_all(stamps.iter().map(|&(_, at)| records[at].1));
    }
    Ok(History { estimates, times: Times::from_sorted(kinds), keys, unseen_ns: config.unseen_ns })
}
