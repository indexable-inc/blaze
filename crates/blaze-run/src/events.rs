//! A run's event stream as blaze-format records in columns (data.md section 5): the parent
//! gathers the [`RunEvent`]s its machines say and writes them, every flush, as one [`Batch`]
//! buffer appended to the run's stream (`<name>.events`, each buffer at a multiple of
//! `blaze_format::ALIGN`, so a viewer opens each where it lies, mapped, with no copy).
//!
//! One row an event, one column a field: `kinds`, `at` (0 where an event has no time),
//! `values` (bytes for `Memory`, the status for `Ended`, else 0) and `keys` (the function, the
//! program or the test; zeros for none), plus `checkouts`, one a `Began` row in row order. Plain
//! columns are checked by their bounds alone, so opening a batch costs O(1) in its rows, and a
//! viewer that wants only failures reads `kinds` and the `keys` it picks. Keys and numbers only:
//! nothing of the machine but what the entry names.

#![allow(unsafe_code, reason = "blaze-format's `record!` writes `unsafe impl Record`, its layout asserted")]

use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Record, Slice, Writer, WriterOpts, open, record};
use blaze_own::{AVec, Alloc};

use crate::registry::RunEvent;
use crate::{CheckoutKey, InstanceKey, Nanos, TestId};

record! {
    /// One flush of a run's events, by columns of equal length (but `checkouts`).
    #[derive(Clone, Copy, Debug)]
    pub struct Batch: 1 {
        pub kinds: Slice<u8>,
        pub at: Slice<u64>,
        pub values: Slice<u64>,
        pub keys: Slice<[u8; 32]>,
        /// The checkout of each `Began` row, in row order.
        pub checkouts: Slice<[u8; 32]>,
    }
}

/// Why bytes are not a batch of events.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Bad {
    Format(FormatError),
    /// A column of another length than `kinds`.
    Column { column: &'static str, len: usize, rows: usize },
    /// A `checkouts` column of another length than the `Began` rows.
    Checkouts { len: usize, began: usize },
    Kind { row: usize, kind: u8 },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Bad::Format(error) => write!(f, "an event batch: {error}"),
            Bad::Column { column, len, rows } => {
                write!(f, "an event batch's column {column} has {len} rows, not {rows}")
            }
            Bad::Checkouts { len, began } => {
                write!(f, "an event batch has {len} checkouts for {began} runs begun")
            }
            Bad::Kind { row, kind } => write!(f, "an event batch's row {row} is of unknown kind {kind}"),
        }
    }
}

impl core::error::Error for Bad {}

const BEGAN: u8 = 0;
const COMPILING: u8 = 1;
const PUBLISHED: u8 = 2;
const TIERED: u8 = 3;
const FAILED: u8 = 4;
const PASSED: u8 = 5;
const TEST_FAILED: u8 = 6;
const MEMORY: u8 = 7;
const ENDED: u8 = 8;

/// One event's row: kind, time, value, key.
fn row(event: &RunEvent) -> (u8, u64, u64, [u8; 32]) {
    match *event {
        RunEvent::Began { at, program, .. } => (BEGAN, at.0, 0, program.0),
        RunEvent::Compiling { function } => (COMPILING, 0, 0, function.0),
        RunEvent::Published { function } => (PUBLISHED, 0, 0, function.0),
        RunEvent::Tiered { function } => (TIERED, 0, 0, function.0),
        RunEvent::Failed { function } => (FAILED, 0, 0, function.0),
        RunEvent::TestPassed { test } => (PASSED, 0, 0, test.0),
        RunEvent::TestFailed { test } => (TEST_FAILED, 0, 0, test.0),
        RunEvent::Memory { at, bytes } => (MEMORY, at.0, bytes, [0; 32]),
        RunEvent::Ended { at, status } => (ENDED, at.0, u64::from(status as u32), [0; 32]),
    }
}

/// Pushes one column, each value through `pick`: where it starts and how long it is.
fn column<T: Record>(
    writer: &mut Writer<Alloc<'_>>,
    events: &[RunEvent],
    pick: impl Fn(&RunEvent) -> Option<T>,
) -> (u64, usize) {
    let start = writer.next::<T>();
    let mut len = 0;
    for value in events.iter().filter_map(pick) {
        writer.push(&value);
        len += 1;
    }
    (start, len)
}

/// `events` as one batch buffer in `alloc`. Equal events in equal order are equal bytes. The
/// buffer is `std`'s vector because blaze-format's `Writer` still hands one back: blaze-own lacks
/// nothing here, blaze-format has yet to move onto it.
pub fn write<'a>(events: &[RunEvent], alloc: Alloc<'a>) -> std::vec::Vec<u8, Alloc<'a>> {
    let mut writer = Writer::new(WriterOpts { schema: Batch::SCHEMA, alloc: alloc, capacity: None });
    let kinds = column(&mut writer, events, |event| Some(row(event).0));
    let at = column(&mut writer, events, |event| Some(row(event).1));
    let values = column(&mut writer, events, |event| Some(row(event).2));
    let keys = column(&mut writer, events, |event| Some(row(event).3));
    let checkouts = column(&mut writer, events, |event| match event {
        RunEvent::Began { checkout, .. } => Some(checkout.0),
        _ => None,
    });
    let root = writer.next::<Batch>();
    let field = |offset: usize| root + offset as u64;
    let batch = Batch {
        kinds: Slice::between(field(offset_of!(Batch, kinds)), kinds.0, kinds.1),
        at: Slice::between(field(offset_of!(Batch, at)), at.0, at.1),
        values: Slice::between(field(offset_of!(Batch, values)), values.0, values.1),
        keys: Slice::between(field(offset_of!(Batch, keys)), keys.0, keys.1),
        checkouts: Slice::between(field(offset_of!(Batch, checkouts)), checkouts.0, checkouts.1),
    };
    let written = writer.push(&batch);
    writer.finish(written)
}

/// The events of one batch buffer (aligned to `blaze_format::ALIGN`, as a stream lays it out),
/// pushed to `out` in order.
pub fn read(buffer: &[u8], out: &mut AVec<'_, RunEvent>) -> Result<(), Bad> {
    let batch = open::<Batch>(buffer).map_err(Bad::Format)?;
    let kinds = batch.kinds.follow(buffer).map_err(Bad::Format)?;
    let at = batch.at.follow(buffer).map_err(Bad::Format)?;
    let values = batch.values.follow(buffer).map_err(Bad::Format)?;
    let keys = batch.keys.follow(buffer).map_err(Bad::Format)?;
    let checkouts = batch.checkouts.follow(buffer).map_err(Bad::Format)?;
    let rows = kinds.len();
    for (column, len) in [("at", at.len()), ("values", values.len()), ("keys", keys.len())] {
        if len != rows {
            return Err(Bad::Column { column, len, rows });
        }
    }
    let began = kinds.iter().filter(|kind| **kind == BEGAN).count();
    if checkouts.len() != began {
        return Err(Bad::Checkouts { len: checkouts.len(), began });
    }
    let mut next_checkout = checkouts.iter();
    for row in 0..rows {
        let (time, value, key) = (Nanos(at[row]), values[row], keys[row]);
        out.push(match kinds[row] {
            BEGAN => {
                let Some(checkout) = next_checkout.next() else {
                    return Err(Bad::Checkouts { len: checkouts.len(), began });
                };
                RunEvent::Began { at: time, program: InstanceKey(key), checkout: CheckoutKey(*checkout) }
            }
            COMPILING => RunEvent::Compiling { function: InstanceKey(key) },
            PUBLISHED => RunEvent::Published { function: InstanceKey(key) },
            TIERED => RunEvent::Tiered { function: InstanceKey(key) },
            FAILED => RunEvent::Failed { function: InstanceKey(key) },
            PASSED => RunEvent::TestPassed { test: TestId(key) },
            TEST_FAILED => RunEvent::TestFailed { test: TestId(key) },
            MEMORY => RunEvent::Memory { at: time, bytes: value },
            ENDED => RunEvent::Ended { at: time, status: value as u32 as i32 },
            kind => return Err(Bad::Kind { row, kind }),
        });
    }
    Ok(())
}
