//! What a query reads: [`Table`]s of chunks, each chunk a blaze-columns [`Batch`] (a Parquet
//! row group, a record), with optional statistics a scan skips chunks by. The engine owns no
//! IO: a Parquet reader, blaze's records and the tests' [`MemTable`] implement the trait.

use core::alloc::Allocator;

use blaze_columns::{Batch, Field, Scalar, Type};
use blaze_own::Vec;

/// A column's statistics over one chunk, where the table has them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// The least and greatest non-null values of a number column.
    pub min: Option<Bound>,
    pub max: Option<Bound>,
    /// How many rows are null.
    pub nulls: Option<u64>,
}

/// A number a statistic holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bound {
    Int(i128),
    Float(f64),
}

/// A table could not hand over a chunk: which, and why in the reader's words.
#[derive(Clone, Copy, Debug)]
pub struct ReadFailed {
    pub chunk: usize,
    pub why: &'static str,
}

/// A table of chunks of one schema. `B` is the allocator its batches live in.
pub trait Table<B: Allocator> {
    /// The table's columns: names, types and whether they may be null.
    fn fields(&self) -> &[Field<B>];

    fn chunks(&self) -> usize;

    /// Statistics of `column` over chunk `chunk`, if the table keeps them.
    fn stats(&self, chunk: usize, column: usize) -> Option<Stats>;

    /// Hands chunk `chunk` to `read`, its columns in [`Table::fields`]' order. A table may read
    /// only `columns` of it (the scan's), leaving the others empty of rows.
    fn read(
        &self,
        chunk: usize,
        columns: &[usize],
        read: &mut dyn FnMut(&Batch<B>),
    ) -> Result<(), ReadFailed>;
}

/// A table held in memory: batches of one schema.
pub struct MemTable<B: Allocator + Clone> {
    fields: Vec<Field<B>, B>,
    batches: Vec<Batch<B>, B>,
}

impl<B: Allocator + Clone> MemTable<B> {
    /// A table of `batches`, each of `fields` (checked: a batch of other fields is refused).
    pub fn new(fields: Vec<Field<B>, B>, batches: Vec<Batch<B>, B>) -> Result<MemTable<B>, usize> {
        for (at, batch) in batches.iter().enumerate() {
            if batch.fields() != fields.as_slice() {
                return Err(at);
            }
        }
        Ok(MemTable { fields, batches })
    }

    /// The statistics of a number column of a batch: its least and greatest values and nulls.
    fn computed(batch: &Batch<B>, column: usize) -> Option<Stats> {
        let col = batch.columns().get(column)?;
        if !matches!(col.ty(), Type::One(scalar) if !scalar.is_text() && scalar != Scalar::Bool) {
            return None;
        }
        let mut min: Option<Bound> = None;
        let mut max: Option<Bound> = None;
        for row in 0..col.len() {
            let value = if col.is_null(row) {
                continue;
            } else if let Some(n) = col.int(row) {
                Bound::Int(n)
            } else if let Some(f) = col.float(row) {
                if f.is_nan() {
                    return Some(Stats { min: None, max: None, nulls: None });
                }
                Bound::Float(f)
            } else {
                return None;
            };
            min = Some(match min {
                Some(it) if !below(value, it) => it,
                _ => value,
            });
            max = Some(match max {
                Some(it) if !below(it, value) => it,
                _ => value,
            });
        }
        Some(Stats { min, max, nulls: u64::try_from(col.null_count()).ok() })
    }
}

/// Whether `a < b`.
#[expect(blaze::as_cast, reason = "an integer statistic to the nearest double, to compare")]
pub(crate) fn below(a: Bound, b: Bound) -> bool {
    match (a, b) {
        (Bound::Int(x), Bound::Int(y)) => x < y,
        (Bound::Float(x), Bound::Float(y)) => x < y,
        (Bound::Int(x), Bound::Float(y)) => (x as f64) < y,
        (Bound::Float(x), Bound::Int(y)) => x < y as f64,
    }
}

impl<B: Allocator + Clone> Table<B> for MemTable<B> {
    fn fields(&self) -> &[Field<B>] {
        &self.fields
    }

    fn chunks(&self) -> usize {
        self.batches.len()
    }

    fn stats(&self, chunk: usize, column: usize) -> Option<Stats> {
        MemTable::computed(self.batches.get(chunk)?, column)
    }

    fn read(
        &self,
        chunk: usize,
        _columns: &[usize],
        read: &mut dyn FnMut(&Batch<B>),
    ) -> Result<(), ReadFailed> {
        let Some(batch) = self.batches.get(chunk) else {
            return Err(ReadFailed { chunk, why: "no such chunk" });
        };
        read(batch);
        Ok(())
    }
}

/// The base tables a query may name, each by its name (a constant of the profile's schema).
pub struct Catalog<'t, B: Allocator, A: Allocator> {
    tables: Vec<(&'static str, &'t dyn Table<B>), A>,
}

impl<'t, B: Allocator, A: Allocator> Catalog<'t, B, A> {
    pub fn new_in(alloc: A) -> Catalog<'t, B, A> {
        Catalog { tables: Vec::new_in(alloc) }
    }

    /// Adds `table` under `name`; a second table of one name replaces the first.
    pub fn add(&mut self, name: &'static str, table: &'t dyn Table<B>) {
        if let Some(entry) = self.tables.iter_mut().find(|(it, _)| it.eq_ignore_ascii_case(name)) {
            entry.1 = table;
        } else {
            self.tables.push((name, table));
        }
    }

    /// The table named `name`, without regard to ASCII case, and its index.
    pub fn find(&self, name: &str) -> Option<(usize, &'t dyn Table<B>)> {
        self.tables
            .iter()
            .enumerate()
            .find(|(_, (it, _))| it.eq_ignore_ascii_case(name))
            .map(|(at, (_, table))| (at, *table))
    }

    pub fn get(&self, at: usize) -> Option<(&'static str, &'t dyn Table<B>)> {
        self.tables.get(at).copied()
    }
}
