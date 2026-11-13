//! This crate's tables are the kernel's ([`blaze_kernel::table`]): keyed parts with a digest
//! each and an index, so a step reads one key of `items(file)`, `lines(file)` or
//! `modules(crate)` (`Reads::part`) and a carry compares two values part by part. There is
//! no second layout here: this module only names which table an error is about (rule 19) and
//! gives a part's bytes in place.
//!
//! Cost: [`Table::get`] is the kernel's binary search of the directory, no allocation;
//! [`finish`] is the kernel's one sort and one pass of digests.

#![blaze::sans_io]

use alloc::string::String;
use core::fmt;

use blaze_kernel::table as kernel;

/// An item's id, and the length of every key that is not of a body's part.
pub type Key = [u8; 16];

/// Why bytes are not a table, or a table could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableError {
    /// The table, as its reader names it (`items`, `lines`, `modules`).
    pub family: &'static str,
    pub what: String,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the table of {}: {}", self.family, self.what)
    }
}

/// Which table, as an error names it. A table's layout is not in its bytes: it is the code of
/// the step that makes it, which the kernel's key of the value covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    pub name: &'static str,
}

/// The bytes of the table `builder` holds. Two parts under one key are refused, naming the
/// key: a key covers what its part depends on (rule 21), so two under one is a bug of the
/// caller's keys, never something to pick from.
pub fn finish(family: Family, builder: kernel::Builder) -> Result<alloc::vec::Vec<u8>, TableError> {
    builder.finish().map_err(|what| TableError { family: family.name, what })
}

/// A key as an error prints it.
pub fn hex(key: &[u8]) -> String {
    // alloc: an error's or a label's text
    let mut out = String::with_capacity(key.len() * 2);
    for byte in key {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        out.push(char::from_digit(u32::from(byte & 15), 16).unwrap_or('?'));
    }
    out
}

/// A table read in place.
#[derive(Clone, Copy, Debug)]
pub struct Table<'a> {
    table: kernel::Table<'a>,
    family: Family,
}

impl<'a> Table<'a> {
    /// The table `bytes` are: the header is checked and nothing else is read. Other bytes are
    /// refused saying what is wrong.
    pub fn read(family: Family, bytes: &'a [u8]) -> Result<Table<'a>, TableError> {
        match kernel::Table::read(bytes) {
            Ok(table) => Ok(Table { table, family }),
            Err(what) => Err(TableError { family: family.name, what }),
        }
    }

    /// How many parts.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// The part under `key`; `None` is the key's absence, which is what a reader of the part
    /// saw. Torn bytes are refused.
    pub fn get(&self, key: &[u8]) -> Result<Option<&'a [u8]>, TableError> {
        match self.table.part(key) {
            Ok(part) => Ok(part.map(|part| part.bytes)),
            Err(what) => Err(TableError { family: self.family.name, what }),
        }
    }

    /// Every key with its part, in the order of the keys' bytes. Torn bytes are refused.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Result<(&'a [u8], &'a [u8]), TableError>> + Clone + 'a {
        let Table { table, family } = *self;
        (0..table.len()).map(move |at| match table.entry(at) {
            Ok((key, part)) => Ok((key, part.bytes)),
            Err(what) => Err(TableError { family: family.name, what }),
        })
    }
}
