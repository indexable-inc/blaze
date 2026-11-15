//! A schema: tables and their columns. The typed surface builds one from Rust structs by
//! reflection (a table is a struct, a column is a field); a test writes one out.
//!
//! Each table is its own value behind a reference, so a statement's check reads only the tables
//! it names and an edit to one table's struct rechecks only the statements naming it.

#![blaze::sans_io]

use crate::desc::{RustTy, Storage};

#[derive(Clone, Copy, Debug)]
pub struct Schema<'s> {
    pub tables: &'s [&'s Table<'s>],
}

#[derive(Clone, Copy, Debug)]
pub struct Table<'s> {
    /// The table's name in SQL. Matched without regard to ASCII case.
    pub name: &'s str,
    pub columns: &'s [Column<'s>],
}

#[derive(Clone, Copy, Debug)]
pub struct Column<'s> {
    /// The column's name in SQL. Matched without regard to ASCII case.
    pub name: &'s str,
    pub storage: Storage,
    /// The field's type, without its `Option`.
    pub rust: RustTy<'s>,
    /// The field is an `Option`.
    pub nullable: bool,
    pub primary_key: bool,
}

impl<'s> Schema<'s> {
    /// The table named `name`, without regard to ASCII case.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn table(&self, name: &str) -> Option<&'s Table<'s>> {
        self.tables.iter().copied().find(|table| table.name.eq_ignore_ascii_case(name))
    }
}

impl<'s> Table<'s> {
    /// The column named `name`, without regard to ASCII case, and its index.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn column(&self, name: &str) -> Option<(u32, &'s Column<'s>)> {
        // Counted in `u32` as it is walked, so no index narrows.
        self.columns
            .iter()
            .zip(0u32..)
            .find(|(column, _)| column.name.eq_ignore_ascii_case(name))
            .map(|(column, index)| (index, column))
    }
}
