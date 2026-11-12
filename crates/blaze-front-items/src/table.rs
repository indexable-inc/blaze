//! A step's value as a table: sorted (key → entry bytes), read in place.
//!
//! The value of [`crate::items`] and of [`crate::lines`] is one of these, so that a reader names
//! the key it read and its read's digest is that key's entry or its absence (end-state.md,
//! "Read-by-part is the primitive"). The kernel's part read is another lane's; this is the
//! content it will index: keys of 16 bytes in ascending order, each once, with its entry's
//! bytes. Nothing here depends on the order rows were pushed in (rule 25): [`write`] sorts.
//!
//! Layout, little-endian: [`MAGIC`], the family (`u32`), the family's layout (`u32`), the
//! count of rows (`u32`); then a row an entry, `key` (16 bytes) and the offset its bytes end at
//! in the data (`u32`; an entry starts where the one before ends); then the data.
//!
//! Cost against the floor: [`Table::get`] is `O(log rows)` compares of 16 bytes and no
//! allocation; [`write`] is one sort of the rows and one copy of the bytes.

#![blaze::sans_io]

use alloc::{format, string::String, vec::Vec};
use core::fmt;

/// A key of a table: an item's id, or one of a family's named keys.
pub type Key = [u8; 16];

/// What a table starts with.
pub const MAGIC: [u8; 8] = *b"blzftab\0";
/// Bytes before the rows.
const HEAD: usize = 20;
/// Bytes of a row: a key and where its entry ends.
const ROW: usize = 20;

/// Why bytes are not a table of the family and layout asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableError {
    /// The family asked for, as its reader names it (`items`, `lines`, `modules`).
    pub family: &'static str,
    pub what: String,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a table of {}: {}", self.family, self.what)
    }
}

/// Which table: its number in the head, its layout, and its name in an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    pub number: u32,
    pub layout: u32,
    pub name: &'static str,
}

/// The table of `rows` (any order) as its bytes. Two rows of one key are refused, naming the
/// key: a key covers what its entry depends on (rule 21), so two entries under one is a bug of
/// the caller's keys, never something to pick from.
pub fn write(family: Family, mut rows: Vec<(Key, Vec<u8>)>) -> Result<Vec<u8>, TableError> {
    let wrong = |what: String| TableError { family: family.name, what };
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    if let Some(pair) = rows.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        // alloc: error path
        return Err(wrong(format!("two entries under the key {}", hex(&pair[0].0))));
    }
    let data: usize = rows.iter().map(|(_, bytes)| bytes.len()).sum();
    if rows.len() > u32::MAX as usize || data > u32::MAX as usize {
        // alloc: error path
        return Err(wrong(format!("{} rows and {data} bytes, past what a u32 counts", rows.len())));
    }
    // alloc: the step's value, kept by the kernel
    let mut out = Vec::with_capacity(HEAD + rows.len() * ROW + data);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&family.number.to_le_bytes());
    out.extend_from_slice(&family.layout.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    let mut end = 0u32;
    for (key, bytes) in &rows {
        end += bytes.len() as u32;
        out.extend_from_slice(key);
        out.extend_from_slice(&end.to_le_bytes());
    }
    for (_, bytes) in &rows {
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// A table read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table<'a> {
    rows: &'a [u8],
    data: &'a [u8],
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A key as an error prints it.
pub fn hex(key: &Key) -> String {
    // alloc: an error's or a label's text
    let mut out = String::with_capacity(32);
    for byte in key {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        out.push(char::from_digit(u32::from(byte & 15), 16).unwrap_or('?'));
    }
    out
}

impl<'a> Table<'a> {
    /// The table `bytes` are, when [`write`] made them for `family`: every key in order, every
    /// entry inside the data. Other bytes are refused saying what is wrong, a previous layout
    /// by its number.
    pub fn read(family: Family, bytes: &'a [u8]) -> Result<Table<'a>, TableError> {
        let wrong = |what: String| TableError { family: family.name, what };
        if bytes.len() < HEAD || bytes[..8] != MAGIC {
            // alloc: error path
            return Err(wrong(format!("{} bytes that do not start as a table", bytes.len())));
        }
        let (number, layout, count) = (u32_at(bytes, 8), u32_at(bytes, 12), u32_at(bytes, 16));
        if number != family.number {
            // alloc: error path
            return Err(wrong(format!("a table of family {number}, where {} was asked", family.number)));
        }
        if layout != family.layout {
            // alloc: error path
            return Err(wrong(format!("layout {layout}, and this reads layout {}", family.layout)));
        }
        let rows_len = count as usize * ROW;
        if bytes.len() < HEAD + rows_len {
            // alloc: error path
            return Err(wrong(format!("{count} rows in {} bytes", bytes.len())));
        }
        let table = Table { rows: &bytes[HEAD..HEAD + rows_len], data: &bytes[HEAD + rows_len..] };
        let mut last_end = 0u32;
        for at in 0..table.len() {
            let end = table.end(at);
            if end < last_end || (at > 0 && table.key(at - 1) >= table.key(at)) {
                // alloc: error path
                return Err(wrong(format!("row {at} (key {}) is out of order", hex(table.key(at)))));
            }
            last_end = end;
        }
        if last_end as usize != table.data.len() {
            // alloc: error path
            return Err(wrong(format!(
                "entries of {last_end} bytes in data of {}",
                table.data.len()
            )));
        }
        Ok(table)
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.rows.len() / ROW
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn key(&self, at: usize) -> &'a Key {
        let row: &'a [u8] = &self.rows[at * ROW..at * ROW + 16];
        row.try_into().expect("a row's key is 16 bytes")
    }

    fn end(&self, at: usize) -> u32 {
        u32_at(self.rows, at * ROW + 16)
    }

    fn entry(&self, at: usize) -> &'a [u8] {
        let start = if at == 0 { 0 } else { self.end(at - 1) };
        &self.data[start as usize..self.end(at) as usize]
    }

    /// The entry under `key`; `None` is the key's absence, which is what a reader of the part
    /// saw.
    pub fn get(&self, key: &Key) -> Option<&'a [u8]> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.key(mid).cmp(key) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(self.entry(mid)),
            }
        }
        None
    }

    /// Every key with its entry, in key order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&'a Key, &'a [u8])> + Clone + 'a {
        let table = *self;
        (0..self.len()).map(move |at| (table.key(at), table.entry(at)))
    }
}
