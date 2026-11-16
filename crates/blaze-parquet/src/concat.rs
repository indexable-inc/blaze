//! A merge: Parquet files of one schema as one file, each row group's column chunks copied as
//! their bytes are (whatever their pages' encoding or codec), with only the offsets moved in
//! the new footer. Page indexes and bloom filters are not carried (this crate writes neither).

use core::{alloc::Allocator, fmt::Write};

use blaze_int::Int;
use blaze_own::{String, Vec};

use crate::{
    MAGIC, ParquetError,
    meta::{self, ColumnChunk, FileMeta, KeyValue, RowGroup},
    read,
};

/// One input of a merge: its bytes and its footer.
pub struct Input<B: AsRef<[u8]>, A: Allocator + Clone> {
    bytes: B,
    meta: FileMeta<A>,
    /// Where its footer begins.
    data_end: usize,
}

impl<B: AsRef<[u8]>, A: Allocator + Clone> Input<B, A> {
    /// The input of `bytes`, a whole Parquet file: its footer read.
    pub fn open(bytes: B, alloc: &A) -> Result<Input<B, A>, ParquetError> {
        let (meta, data_end) = read::footer(bytes.as_ref(), alloc)?;
        Ok(Input { bytes, meta, data_end })
    }

    pub fn meta(&self) -> &FileMeta<A> {
        &self.meta
    }
}

/// Where a merge failed: the input at `input` (its place in the merge's inputs), or the
/// output when `None`, and the step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub input: Option<usize>,
    pub doing: &'static str,
    pub error: ParquetError,
}

/// A merge under way.
pub struct Concat<B: AsRef<[u8]>, A: Allocator + Clone> {
    inputs: Vec<Input<B, A>, A>,
    alloc: A,
    /// The key of the merged rows' count: dropped from the first input's metadata and written
    /// with the merge's.
    rows_key: &'static str,
    /// How many bytes [`Concat::next`] gathers before handing them out.
    piece: usize,
    /// The output not yet handed out, and how many bytes were.
    out: Vec<u8, A>,
    handed: usize,
    groups: Vec<RowGroup<A>, A>,
    /// The input and its row group copied next.
    at: (usize, usize),
    /// The rows the inputs' footers said, so far.
    rows: i64,
    done: bool,
}

impl<B: AsRef<[u8]>, A: Allocator + Clone> Concat<B, A> {
    /// The merge of `inputs` (at least one, all of the first one's schema). Its metadata is the
    /// first input's, but for `rows_key`, which says the rows the inputs' footers held. The
    /// output is handed out in pieces of at least `piece` bytes.
    pub fn new(inputs: Vec<Input<B, A>, A>, rows_key: &'static str, piece: usize, alloc: A) -> Result<Concat<B, A>, Failed> {
        if inputs.is_empty() {
            return Err(Failed { input: None, doing: "starting a merge of no input", error: ParquetError::SchemaDiffers });
        }
        let mut out = Vec::with_capacity_in(piece.saturating_add(1 << 20), alloc.clone());
        out.extend_from_slice(&MAGIC);
        Ok(Concat {
            inputs,
            groups: Vec::new_in(alloc.clone()),
            alloc,
            rows_key,
            piece,
            out,
            handed: 0,
            at: (0, 0),
            rows: 0,
            done: false,
        })
    }

    /// The next piece of the output's bytes, `None` once it was all handed out: at least the
    /// piece's bytes of row groups when there are that many left, and the footer with the last.
    pub fn next(&mut self) -> Result<Option<Vec<u8, A>>, Failed> {
        if self.done {
            return Ok(None);
        }
        while self.at.0 < self.inputs.len() {
            let (input_at, group_at) = self.at;
            let failed = |doing, error| Failed { input: Some(input_at), doing, error };
            let input = &self.inputs[input_at];
            if group_at == 0 {
                if input.meta.schema.as_slice() != self.inputs[0].meta.schema.as_slice() {
                    return Err(failed("checking the schema of the input", ParquetError::SchemaDiffers));
                }
                self.rows += input.meta.num_rows;
            }
            if group_at == input.meta.row_groups.len() {
                self.at = (input_at + 1, 0);
                continue;
            }
            let group = &input.meta.row_groups[group_at];
            let bytes = &input.bytes.as_ref()[..input.data_end];
            let mut columns = Vec::with_capacity_in(group.columns.len(), self.alloc.clone());
            let group_start = self.handed + self.out.len();
            for chunk in group.columns.iter() {
                let corrupt = |what| {
                    failed("copying a column chunk of the input", ParquetError::Corrupt { column: crate::column_name(""), what })
                };
                let Some(column) = &chunk.meta else {
                    return Err(corrupt("a column chunk in another file"));
                };
                let start = column.start().narrow::<usize>().map_err(|_| corrupt("a chunk at a negative offset"))?;
                let len = column.total_compressed_size.narrow::<usize>().map_err(|_| corrupt("a chunk of negative size"))?;
                let raw = start
                    .checked_add(len)
                    .and_then(|end| bytes.get(start..end))
                    .filter(|_| start >= MAGIC.len())
                    .ok_or_else(|| corrupt("a chunk past the input's data"))?;
                let new_start = self.handed + self.out.len();
                self.out.extend_from_slice(raw);
                let shift = new_start.widen_to::<u64>().cast_signed() - column.start();
                let mut column = column.clone();
                column.data_page_offset += shift;
                column.dictionary_page_offset = column.dictionary_page_offset.map(|it| it + shift);
                column.index_page_offset = column.index_page_offset.map(|it| it + shift);
                columns.push(ColumnChunk {
                    file_path: None,
                    file_offset: new_start.widen_to::<u64>().cast_signed(),
                    meta: Some(column),
                });
            }
            let ordinal = i16::try_from(self.groups.len()).ok();
            self.groups.push(RowGroup {
                columns,
                total_byte_size: group.total_byte_size,
                num_rows: group.num_rows,
                file_offset: Some(group_start.widen_to::<u64>().cast_signed()),
                total_compressed_size: group.total_compressed_size,
                ordinal,
            });
            self.at = (input_at, group_at + 1);
            if self.out.len() >= self.piece {
                return Ok(Some(self.hand_out()));
            }
        }
        self.footer()?;
        self.done = true;
        Ok(Some(self.hand_out()))
    }

    fn hand_out(&mut self) -> Vec<u8, A> {
        let piece = core::mem::replace(&mut self.out, Vec::with_capacity_in(self.piece.saturating_add(1 << 20), self.alloc.clone()));
        self.handed += piece.len();
        piece
    }

    fn footer(&mut self) -> Result<(), Failed> {
        let failed = |doing, error| Failed { input: None, doing, error };
        let first = &self.inputs[0].meta;
        let alloc = self.alloc.clone();
        let mut key_value = Vec::with_capacity_in(first.key_value.len() + 1, alloc.clone());
        for kv in first.key_value.iter().filter(|it| it.key.as_str() != self.rows_key) {
            key_value.push(kv.clone());
        }
        let mut rows = String::new_in(alloc.clone());
        write!(rows, "{}", self.rows).map_err(|_| {
            failed("writing the merged rows' count", ParquetError::TooLarge { column: crate::column_name(""), what: "a count" })
        })?;
        key_value.push(KeyValue { key: String::from_str_in(self.rows_key, alloc.clone()), value: Some(rows) });
        let footer = FileMeta {
            version: first.version,
            schema: Vec::from_iter_in(first.schema.iter().cloned(), alloc.clone()),
            num_rows: self.rows,
            row_groups: core::mem::replace(&mut self.groups, Vec::new_in(alloc.clone())),
            key_value,
            created_by: first.created_by.clone(),
            column_orders: first.column_orders,
        };
        let start = self.out.len();
        meta::write_file_meta(&mut self.out, &footer);
        let len = (self.out.len() - start)
            .narrow::<u32>()
            .map_err(|_| failed("ending", ParquetError::TooLarge { column: crate::column_name(""), what: "a footer past 4 GiB" }))?;
        self.out.extend_from_slice(&len.to_le_bytes());
        self.out.extend_from_slice(&MAGIC);
        Ok(())
    }

    /// The whole output at once: for a caller that writes it in one write.
    pub fn all(mut self) -> Result<Vec<u8, A>, Failed> {
        let mut out = Vec::new_in(self.alloc.clone());
        while let Some(piece) = self.next()? {
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }
}
