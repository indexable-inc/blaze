//! A merge: Parquet files of one schema as one file, each row group's column chunks copied as
//! their bytes are (whatever their pages' encoding) with only the offsets moved in the new
//! footer, or, where a chunk is of another codec than the merge's, its pages recompressed.
//! Page indexes and bloom filters are not carried (this crate writes neither).

use core::{alloc::Allocator, fmt::Write};

use blaze_int::Int;
use blaze_own::{String, Vec};

use crate::{
    Compression, MAGIC, ParquetError, compress,
    meta::{self, ColumnChunk, FileMeta, KeyValue, RowGroup},
    read,
};

/// One input of a merge: its bytes and its footer.
pub struct Input<B: AsRef<[u8]>, A: Allocator + Copy> {
    bytes: B,
    meta: FileMeta<A>,
    /// Where its footer begins.
    data_end: usize,
}

impl<B: AsRef<[u8]>, A: Allocator + Copy> Input<B, A> {
    /// The input of `bytes`, a whole Parquet file: its footer read.
    pub fn open(bytes: B, alloc: &A) -> Result<Input<B, A>, ParquetError> {
        let (meta, data_end) = read::footer(bytes.as_ref(), *alloc)?;
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
pub struct Concat<B: AsRef<[u8]>, A: Allocator + Copy> {
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
    /// The output's pages' codec, and a page's bytes as they are recompressed into it.
    compression: Compression,
    plain: Vec<u8, A>,
    packed: Vec<u8, A>,
}

/// The pages of the column chunk `raw` (of codec `codec`) appended to `out` in `to`, each page
/// decompressed and compressed again with its header's sizes rewritten (its encoding as it
/// was); where its first data page and its dictionary page now start in `out`.
fn recode<A: Allocator>(
    name: &str,
    raw: &[u8],
    codec: i32,
    to: Compression,
    out: &mut Vec<u8, A>,
    (plain, packed): (&mut Vec<u8, A>, &mut Vec<u8, A>),
) -> Result<(usize, Option<usize>), ParquetError> {
    let corrupt = |what| ParquetError::Corrupt { column: crate::column_name(name), what };
    let mut at = 0;
    let (mut data, mut dictionary) = (None, None);
    while at < raw.len() {
        let (mut header, header_len) = meta::read_page_header(&raw[at..])?;
        if header.ty == meta::page::DATA_PAGE_V2 || header.ty == meta::page::INDEX_PAGE {
            return Err(ParquetError::Unsupported {
                column: crate::column_name(name),
                what: "a merge that recompresses a v2 data page or an index page",
                code: header.ty.into(),
            });
        }
        let len = header.compressed_page_size.narrow::<usize>().map_err(|_| corrupt("a page of negative size"))?;
        let stored = raw.get(at + header_len..at + header_len + len).ok_or_else(|| corrupt("a page past its chunk"))?;
        at += header_len + len;
        let size = header.uncompressed_page_size.narrow::<usize>().map_err(|_| corrupt("a page of negative size"))?;
        plain.clear();
        let body: &[u8] = if compress::decompress(name, codec, stored, size, plain)? { plain } else { stored };
        packed.clear();
        let body: &[u8] = if to.compress(body, packed)? { packed } else { body };
        header.compressed_page_size =
            body.len().narrow::<i32>().map_err(|_| ParquetError::TooLarge { column: crate::column_name(name), what: "a page past 2 GiB" })?;
        if header.ty == meta::page::DICTIONARY_PAGE {
            dictionary.get_or_insert(out.len());
        } else {
            data.get_or_insert(out.len());
        }
        meta::write_page_header(out, &header, None);
        out.extend_from_slice(body);
    }
    Ok((data.ok_or_else(|| corrupt("a column chunk of no data page"))?, dictionary))
}

impl<B: AsRef<[u8]>, A: Allocator + Copy> Concat<B, A> {
    /// The merge of `inputs` (at least one, all of the first one's schema). Its metadata is the
    /// first input's, but for `rows_key`, which says the rows the inputs' footers held. The
    /// output is handed out in pieces of at least `piece` bytes. Its pages are compressed as
    /// `compression` says: a chunk already so is copied as its bytes are, another has each page
    /// decompressed and compressed again (a merge of a build's uncompressed files into a cold
    /// one under zstd).
    pub fn new(
        inputs: Vec<Input<B, A>, A>,
        rows_key: &'static str,
        piece: usize,
        compression: Compression,
        alloc: A,
    ) -> Result<Concat<B, A>, Failed> {
        if inputs.is_empty() {
            return Err(Failed { input: None, doing: "starting a merge of no input", error: ParquetError::SchemaDiffers });
        }
        let mut out = Vec::with_capacity_in(piece.saturating_add(1 << 20), alloc);
        out.extend_from_slice(&MAGIC);
        Ok(Concat {
            inputs,
            compression,
            plain: Vec::new_in(alloc),
            packed: Vec::new_in(alloc),
            groups: Vec::new_in(alloc),
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
            let mut columns = Vec::with_capacity_in(group.columns.len(), self.alloc);
            let group_start = self.handed + self.out.len();
            let mut compressed = 0i64;
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
                let mut column = column.clone_in(self.alloc);
                if column.codec == self.compression.codec() {
                    self.out.extend_from_slice(raw);
                    let shift = new_start.widen_to::<u64>().cast_signed() - column.start();
                    column.data_page_offset += shift;
                    column.dictionary_page_offset = column.dictionary_page_offset.map(|it| it + shift);
                    column.index_page_offset = column.index_page_offset.map(|it| it + shift);
                } else {
                    let name = column.path.last().map_or("", |it| it.as_str());
                    let (data, dictionary) = recode(
                        name,
                        raw,
                        column.codec,
                        self.compression,
                        &mut self.out,
                        (&mut self.plain, &mut self.packed),
                    )
                    .map_err(|error| failed("compressing a column chunk of the input", error))?;
                    let offset = |at: usize| (self.handed + at).widen_to::<u64>().cast_signed();
                    column.data_page_offset = offset(data);
                    column.dictionary_page_offset = dictionary.map(offset);
                    column.index_page_offset = None;
                    column.codec = self.compression.codec();
                    column.total_compressed_size = (self.handed + self.out.len() - new_start).widen_to::<u64>().cast_signed();
                }
                compressed += column.total_compressed_size;
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
                total_compressed_size: group.total_compressed_size.map(|_| compressed),
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
        let piece = core::mem::replace(&mut self.out, Vec::with_capacity_in(self.piece.saturating_add(1 << 20), self.alloc));
        self.handed += piece.len();
        piece
    }

    fn footer(&mut self) -> Result<(), Failed> {
        let failed = |doing, error| Failed { input: None, doing, error };
        let first = &self.inputs[0].meta;
        let alloc = self.alloc;
        let mut key_value = Vec::with_capacity_in(first.key_value.len() + 1, alloc);
        for kv in first.key_value.iter().filter(|it| it.key.as_str() != self.rows_key) {
            key_value.push(kv.clone_in(alloc));
        }
        let mut rows = String::new_in(alloc);
        write!(rows, "{}", self.rows).map_err(|_| {
            failed("writing the merged rows' count", ParquetError::TooLarge { column: crate::column_name(""), what: "a count" })
        })?;
        key_value.push(KeyValue { key: String::from_str_in(self.rows_key, alloc), value: Some(rows) });
        let footer = FileMeta {
            version: first.version,
            schema: Vec::from_iter_in(first.schema.iter().cloned(), alloc),
            num_rows: self.rows,
            row_groups: core::mem::replace(&mut self.groups, Vec::new_in(alloc)),
            key_value,
            created_by: first.created_by.as_ref().map(|s| s.clone_in(alloc)),
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
        let mut out = Vec::new_in(self.alloc);
        while let Some(piece) = self.next()? {
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }
}
