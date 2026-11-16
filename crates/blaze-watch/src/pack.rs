//! The recording format (`.bwr`): what the flight recorder saves and any reader opens. A
//! prelude (`BWR` and a version byte), then blocks; a block is up to [`MOST_ROWS`] ring entries
//! in columns, one an event field, each a run of LEB128 numbers prefixed by its byte length, so
//! a reader that wants only kinds and times skips the rest without decoding it:
//!
//! ```text
//! block := rows keys key*32  col(kind) col(flags) col(detail) col(lane) col(n) col(at) col(value) col(key)
//! col   := byte_len bytes            (LEB128 throughout)
//! ```
//!
//! Rows are in the order the writer gives (by lane, then entry number, as a save reads the
//! rings), so `n`, `at` and `value` are zigzag deltas from the row before, mostly one byte. A
//! key is an index into the block's table of distinct keys (0 for none, else 1 + its index,
//! the table in first-seen order). Equal rows in equal order are equal bytes.
//!
//! The reader refuses, at its offset, a number longer than ten bytes or not in its shortest
//! form, a count past [`MOST_ROWS`] or [`MOST_KEYS`] or past what the input could hold, a
//! column that ends before its rows or goes past them, and a key index past the table; it
//! sizes nothing by a count before checking it.

// adapted from makepad/relax (MIT), libs/prof_stream/src/lib.rs (`Enc`, `Dec`: LEB128 varints,
// zigzag deltas, counts checked against the bytes left); changed: columns per field in place of
// one payload a record, a key table per block, refusal of non-shortest forms.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{SortedMap, Vec};

use crate::ring::Event;

pub const MAGIC: [u8; 3] = *b"BWR";
pub const VERSION: u8 = 1;
/// The most rows a block holds.
pub const MOST_ROWS: usize = 1 << 16;
/// The most distinct keys a block holds.
pub const MOST_KEYS: usize = 1 << 12;
/// The columns of a block.
pub const COLUMNS: usize = 8;

/// One entry of a recording: the lane it was emitted on, its number there, the event.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Row {
    pub lane: u32,
    pub n: u64,
    pub event: Event,
}

/// Why bytes are not a recording, or rows not a block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    Magic,
    Version { found: u8 },
    Truncated { at: usize },
    /// A number of more than ten bytes, or past 64 bits.
    Overlong { at: usize },
    /// A number with a trailing zero group: not its shortest form.
    NotShortest { at: usize },
    TooMany { at: usize, what: &'static str, found: u64, most: usize },
    /// A column whose bytes end before its rows do, or go on after.
    Column { at: usize, column: &'static str },
    Key { at: usize, index: u64, keys: usize },
    /// A value that does not fit its field.
    Field { at: usize, column: &'static str, found: u64 },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::Magic => write!(f, "not a blaze recording: it does not start with `BWR`"),
            Bad::Version { found } => {
                write!(f, "a recording of format {found}, where this blaze reads {VERSION}")
            }
            Bad::Truncated { at } => write!(f, "a recording ends mid-value at byte {at}"),
            Bad::Overlong { at } => write!(f, "a recording's number at byte {at} is past 64 bits"),
            Bad::NotShortest { at } => {
                write!(f, "a recording's number at byte {at} is not in its shortest form")
            }
            Bad::TooMany { at, what, found, most } => {
                write!(f, "a recording's block at byte {at} has {found} {what}, past {most}")
            }
            Bad::Column { at, column } => {
                write!(f, "a recording's column {column} at byte {at} does not hold its block's rows")
            }
            Bad::Key { at, index, keys } => {
                write!(f, "a recording's key index {index} at byte {at} is past its {keys} keys")
            }
            Bad::Field { at, column, found } => {
                write!(f, "a recording's {column} {found} at byte {at} does not fit its field")
            }
        }
    }
}

impl core::error::Error for Bad {}

/// Appends `n` as LEB128.
pub fn leb<A: Allocator>(out: &mut Vec<u8, A>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

pub const fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub const fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// `value - base` as a zigzag delta.
pub const fn delta(value: u64, base: u64) -> u64 {
    zigzag(value.wrapping_sub(base) as i64)
}

pub const fn undelta(d: u64, base: u64) -> u64 {
    base.wrapping_add(unzigzag(d) as u64)
}

/// A reader of LEB128 numbers over a slice, offsets reported against the whole input.
struct Leb<'a> {
    input: &'a [u8],
    at: usize,
    end: usize,
}

impl Leb<'_> {
    fn number(&mut self) -> Result<u64, Bad> {
        let start = self.at;
        let mut n = 0u64;
        for i in 0..10 {
            // `end` is at most the input's length (each reader is made so).
            if self.at >= self.end || self.at >= self.input.len() {
                return Err(Bad::Truncated { at: self.at });
            }
            let byte = self.input[self.at];
            self.at += 1;
            if i == 9 && byte > 1 {
                return Err(Bad::Overlong { at: start });
            }
            n |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                if byte == 0 && i > 0 {
                    return Err(Bad::NotShortest { at: start });
                }
                return Ok(n);
            }
        }
        Err(Bad::Overlong { at: start })
    }
}

/// The prelude of a recording.
pub fn prelude<A: Allocator>(out: &mut Vec<u8, A>) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
}

/// Checks the prelude; the first block starts at the returned offset.
pub fn open(input: &[u8]) -> Result<usize, Bad> {
    if input.len() < 4 || input[..3] != MAGIC {
        return Err(Bad::Magic);
    }
    if input[3] != VERSION {
        return Err(Bad::Version { found: input[3] });
    }
    Ok(4)
}

const NAMES: [&str; COLUMNS] = ["kind", "flags", "detail", "lane", "n", "at", "value", "key"];

/// Appends `rows` as one block. `scratch` holds one column at a time; the key table is in
/// `alloc`.
pub fn write<A: Allocator, S: Allocator, K: Allocator + Clone>(
    rows: &[Row],
    out: &mut Vec<u8, A>,
    scratch: &mut Vec<u8, S>,
    alloc: K,
) -> Result<(), Bad> {
    if rows.len() > MOST_ROWS {
        return Err(Bad::TooMany { at: out.len(), what: "rows", found: rows.len() as u64, most: MOST_ROWS });
    }
    // The key table, in first-seen order, and each key's index.
    let mut index: SortedMap<[u8; 32], u64, K> = SortedMap::with_capacity_in(16, alloc.clone());
    let mut table: Vec<[u8; 32], K> = Vec::new_in(alloc);
    for row in rows {
        if row.event.key != [0; 32] && !index.contains_key(&row.event.key) {
            if table.len() == MOST_KEYS {
                return Err(Bad::TooMany { at: out.len(), what: "keys", found: MOST_KEYS as u64 + 1, most: MOST_KEYS });
            }
            table.push(row.event.key);
            index.insert(row.event.key, table.len() as u64);
        }
    }
    leb(out, rows.len() as u64);
    leb(out, table.len() as u64);
    for key in table.iter() {
        out.extend_from_slice(key);
    }
    for column in 0..COLUMNS {
        scratch.clear();
        let mut before = Row::default();
        for row in rows {
            let e = &row.event;
            let value = match column {
                0 => u64::from(e.kind),
                1 => u64::from(e.flags),
                2 => u64::from(e.detail),
                3 => u64::from(row.lane),
                4 => delta(row.n, before.n),
                5 => delta(e.at, before.event.at),
                6 => delta(e.value, before.event.value),
                // Every nonzero key was put in the table above.
                _ => match (e.key == [0; 32], index.get(&e.key)) {
                    (true, _) => 0,
                    (false, Some(index)) => *index,
                    (false, None) => unreachable!("the table holds every key of the block"),
                },
            };
            leb(scratch, value);
            before = *row;
        }
        leb(out, scratch.len() as u64);
        out.extend_from_slice(scratch);
    }
    Ok(())
}

/// Reads the block at `at`, pushing its rows to `out`; returns where the next block starts.
pub fn read<A: Allocator>(input: &[u8], at: usize, out: &mut Vec<Row, A>) -> Result<usize, Bad> {
    let mut head = Leb { input, at, end: input.len() };
    let block = at;
    let rows = head.number()?;
    // Each row takes at least a byte in each column.
    if rows > MOST_ROWS as u64 || rows.saturating_mul(COLUMNS as u64) > (input.len() - head.at) as u64 {
        return Err(Bad::TooMany { at: block, what: "rows", found: rows, most: MOST_ROWS });
    }
    let keys = head.number()?;
    if keys > MOST_KEYS as u64 || keys.saturating_mul(32) > (input.len() - head.at) as u64 {
        return Err(Bad::TooMany { at: block, what: "keys", found: keys, most: MOST_KEYS });
    }
    let (rows, keys) = (rows as usize, keys as usize);
    let table = &input[head.at..head.at + keys * 32];
    head.at += keys * 32;
    let first = out.len();
    out.reserve(rows);
    for _ in 0..rows {
        out.push(Row::default());
    }
    let mut at = head.at;
    for (column, name) in NAMES.iter().enumerate() {
        let mut lengths = Leb { input, at, end: input.len() };
        let len = lengths.number()?;
        if len > (input.len() - lengths.at) as u64 {
            return Err(Bad::Column { at, column: name });
        }
        let start = lengths.at;
        let mut values = Leb { input, at: start, end: start + len as usize };
        let mut before = Row::default();
        for i in 0..rows {
            let value_at = values.at;
            let value = values.number().map_err(|bad| match bad {
                Bad::Truncated { .. } => Bad::Column { at: start, column: name },
                other => other,
            })?;
            let fits = |most: u64| match value <= most {
                true => Ok(value),
                false => Err(Bad::Field { at: value_at, column: name, found: value }),
            };
            let row = &mut out[first + i];
            match column {
                0 => row.event.kind = fits(u64::from(u16::MAX))? as u16,
                1 => row.event.flags = fits(u64::from(u16::MAX))? as u16,
                2 => row.event.detail = fits(u64::from(u32::MAX))? as u32,
                3 => row.lane = fits(u64::from(u32::MAX))? as u32,
                4 => row.n = undelta(value, before.n),
                5 => row.event.at = undelta(value, before.event.at),
                6 => row.event.value = undelta(value, before.event.value),
                _ => {
                    if value > keys as u64 {
                        return Err(Bad::Key { at: value_at, index: value, keys });
                    }
                    if value > 0 {
                        let from = (value as usize - 1) * 32;
                        row.event.key.copy_from_slice(&table[from..from + 32]);
                    }
                }
            }
            before = *row;
        }
        if values.at != values.end {
            return Err(Bad::Column { at: start, column: name });
        }
        at = values.end;
    }
    Ok(at)
}
