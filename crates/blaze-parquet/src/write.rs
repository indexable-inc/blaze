//! The writer: batches of one set of fields in, a Parquet file's bytes out.

use core::{alloc::Allocator, ops::Range};

use blaze_columns::{Batch, Column, Field, Scalar, Type, Values};
use blaze_int::Int;
use blaze_own::{String, Vec};

use crate::{
    MAGIC, ParquetError, column_name, levels,
    meta::{
        self, ColumnChunk, ColumnMeta, FileMeta, KeyValue, Logical, PageHeader, RowGroup, SchemaElement,
        Statistics, codec, converted, encoding, page, physical, repetition,
    },
};

/// The most bytes a text column's min or max is written with: past it the chunk says neither
/// (a min or max cut short would need care no query here needs).
pub const STAT_BYTES: usize = 64;

/// How a file is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The most rows of a row group (DuckDB's own default, 122 880, is what blaze-profile
    /// gives).
    pub row_group_rows: usize,
    /// About the most bytes of values a page holds: a page ends at the first row that reaches
    /// it.
    pub page_bytes: usize,
    /// The most rows of a page.
    pub page_rows: usize,
    /// The footer's `created_by`.
    pub created_by: &'static str,
}

impl Options {
    pub const DEFAULT: Options =
        Options { row_group_rows: 122_880, page_bytes: 1 << 20, page_rows: 20_000, created_by: "blaze-parquet" };
}

/// How a field is stored: its leaf's physical type and annotations, and its levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Leaf {
    pub(crate) scalar: Scalar,
    pub(crate) list: bool,
    pub(crate) nullable: bool,
    pub(crate) max_def: u8,
    pub(crate) max_rep: u8,
}

impl Leaf {
    pub(crate) fn of(ty: Type, nullable: bool) -> Leaf {
        let (scalar, list) = match ty {
            Type::One(it) => (it, false),
            Type::List(it) => (it, true),
        };
        let max_def = u8::from(nullable) + u8::from(list);
        Leaf { scalar, list, nullable, max_def, max_rep: u8::from(list) }
    }

    pub(crate) fn physical(self) -> i32 {
        match self.scalar {
            Scalar::Bool => physical::BOOLEAN,
            Scalar::I32 | Scalar::U16 | Scalar::U32 => physical::INT32,
            Scalar::I64 | Scalar::U64 => physical::INT64,
            Scalar::F64 => physical::DOUBLE,
            Scalar::Text | Scalar::Dict => physical::BYTE_ARRAY,
        }
    }

    /// The leaf's converted type and logical type.
    fn annotations(self) -> (Option<i32>, Option<Logical>) {
        match self.scalar {
            Scalar::U16 => (Some(converted::UINT_16), Some(Logical::Integer { bits: 16, signed: false })),
            Scalar::U32 => (Some(converted::UINT_32), Some(Logical::Integer { bits: 32, signed: false })),
            Scalar::U64 => (Some(converted::UINT_64), Some(Logical::Integer { bits: 64, signed: false })),
            Scalar::Text | Scalar::Dict => (Some(converted::UTF8), Some(Logical::String)),
            Scalar::Bool | Scalar::I32 | Scalar::I64 | Scalar::F64 => (None, None),
        }
    }
}

/// The schema elements of `fields`, depth first under the root `schema`.
fn schema<A: Allocator + Clone>(fields: &[Field<A>], alloc: &A) -> Vec<SchemaElement<A>, A> {
    let element = |name: &str| SchemaElement {
        ty: None,
        type_length: None,
        repetition: None,
        name: String::from_str_in(name, alloc.clone()),
        num_children: None,
        converted: None,
        scale: None,
        precision: None,
        field_id: None,
        logical: None,
    };
    let mut out = Vec::with_capacity_in(1 + 3 * fields.len(), alloc.clone());
    let mut root = element("schema");
    root.num_children = Some(fields.len().narrow_or_stop());
    out.push(root);
    for field in fields {
        let leaf = Leaf::of(field.ty, field.nullable);
        let rep = if field.nullable { repetition::OPTIONAL } else { repetition::REQUIRED };
        let (converted, logical) = leaf.annotations();
        let mut value = element(if leaf.list { "element" } else { field.name() });
        value.ty = Some(leaf.physical());
        value.converted = converted;
        value.logical = logical;
        if leaf.list {
            let mut list = element(field.name());
            list.repetition = Some(rep);
            list.num_children = Some(1);
            list.converted = Some(converted::LIST);
            list.logical = Some(Logical::List);
            out.push(list);
            let mut repeated = element("list");
            repeated.repetition = Some(repetition::REPEATED);
            repeated.num_children = Some(1);
            out.push(repeated);
            value.repetition = Some(repetition::REQUIRED);
        } else {
            value.repetition = Some(rep);
        }
        out.push(value);
    }
    out
}

/// A column chunk's statistics as they are gathered.
struct Stats<A: Allocator> {
    nulls: i64,
    int: Option<(i128, i128)>,
    float: Option<(f64, f64)>,
    boolean: Option<(bool, bool)>,
    text: Option<(Vec<u8, A>, Vec<u8, A>)>,
}

impl<A: Allocator + Clone> Stats<A> {
    fn new() -> Stats<A> {
        Stats { nulls: 0, int: None, float: None, boolean: None, text: None }
    }

    fn int(&mut self, value: i128) {
        self.int = Some(match self.int {
            Some((low, high)) => (low.min(value), high.max(value)),
            None => (value, value),
        });
    }

    fn float(&mut self, value: f64) {
        if value.is_nan() {
            return;
        }
        self.float = Some(match self.float {
            Some((low, high)) => (if value < low { value } else { low }, if value > high { value } else { high }),
            None => (value, value),
        });
    }

    fn boolean(&mut self, value: bool) {
        self.boolean = Some(match self.boolean {
            Some((low, high)) => (low & value, high | value),
            None => (value, value),
        });
    }

    fn text(&mut self, value: &[u8], alloc: &A) {
        match &mut self.text {
            Some((low, high)) => {
                if value < low.as_slice() {
                    low.clear();
                    low.extend_from_slice(value);
                }
                if value > high.as_slice() {
                    high.clear();
                    high.extend_from_slice(value);
                }
            }
            None => {
                self.text =
                    Some((Vec::from_slice_in(value, alloc.clone()), Vec::from_slice_in(value, alloc.clone())));
            }
        }
    }

    /// The chunk's statistics in the leaf's plain encoding.
    fn finish(&mut self, leaf: Leaf, alloc: &A) -> Statistics<A> {
        let bytes = |raw: &[u8]| Some(Vec::from_slice_in(raw, alloc.clone()));
        let (min, max) = match leaf.physical() {
            physical::INT32 => match self.int {
                Some((low, high)) => {
                    (bytes(&low.truncate_to::<i32>().to_le_bytes()), bytes(&high.truncate_to::<i32>().to_le_bytes()))
                }
                None => (None, None),
            },
            physical::INT64 => match self.int {
                Some((low, high)) => {
                    (bytes(&low.truncate_to::<i64>().to_le_bytes()), bytes(&high.truncate_to::<i64>().to_le_bytes()))
                }
                None => (None, None),
            },
            physical::DOUBLE => match self.float {
                // A zero bound is written as the zero that holds both: -0.0 below, +0.0 above,
                // as the format asks.
                Some((low, high)) => {
                    let low = if low == 0.0 { -0.0f64 } else { low };
                    let high = if high == 0.0 { 0.0f64 } else { high };
                    (bytes(&low.to_le_bytes()), bytes(&high.to_le_bytes()))
                }
                None => (None, None),
            },
            physical::BOOLEAN => match self.boolean {
                Some((low, high)) => (bytes(&[u8::from(low)]), bytes(&[u8::from(high)])),
                None => (None, None),
            },
            _ => match self.text.take() {
                Some((low, high)) if low.len() <= STAT_BYTES && high.len() <= STAT_BYTES => (Some(low), Some(high)),
                _ => (None, None),
            },
        };
        let out = Statistics {
            max: None,
            min: None,
            null_count: Some(self.nulls),
            distinct_count: None,
            max_value: max,
            min_value: min,
        };
        *self = Stats::new();
        out
    }
}

/// One column's chunk of the row group being written: its finished pages and the page being
/// filled.
struct Chunk<A: Allocator> {
    leaf: Leaf,
    /// The chunk's finished pages, each a header and its body.
    pages: Vec<u8, A>,
    /// The page's values, PLAIN-encoded (a boolean column's in `bools` until the page ends).
    values: Vec<u8, A>,
    bools: Vec<u8, A>,
    defs: Vec<u8, A>,
    reps: Vec<u8, A>,
    /// The page's level entries (its `num_values`: a value, a null or an empty list each).
    page_values: usize,
    page_rows: usize,
    /// The chunk's level entries.
    chunk_values: i64,
    stats: Stats<A>,
    /// A page's body, as it is put together.
    body: Vec<u8, A>,
}

impl<A: Allocator + Clone> Chunk<A> {
    fn new(leaf: Leaf, alloc: &A) -> Chunk<A> {
        Chunk {
            leaf,
            pages: Vec::new_in(alloc.clone()),
            values: Vec::new_in(alloc.clone()),
            bools: Vec::new_in(alloc.clone()),
            defs: Vec::new_in(alloc.clone()),
            reps: Vec::new_in(alloc.clone()),
            page_values: 0,
            page_rows: 0,
            chunk_values: 0,
            stats: Stats::new(),
            body: Vec::new_in(alloc.clone()),
        }
    }

    /// Value `at` of `values` appended to the page, and to the statistics unless a list's.
    fn put(&mut self, values: &Values<A>, at: usize, alloc: &A) {
        let stats = !self.leaf.list;
        match values {
            Values::Bool(it) => {
                self.bools.push(u8::from(it[at]));
                if stats {
                    self.stats.boolean(it[at]);
                }
            }
            Values::I32(it) => {
                self.values.extend_from_slice(&it[at].to_le_bytes());
                if stats {
                    self.stats.int(it[at].into());
                }
            }
            Values::U16(it) => {
                self.values.extend_from_slice(&u32::from(it[at]).to_le_bytes());
                if stats {
                    self.stats.int(it[at].into());
                }
            }
            Values::U32(it) => {
                self.values.extend_from_slice(&it[at].to_le_bytes());
                if stats {
                    self.stats.int(it[at].into());
                }
            }
            Values::I64(it) => {
                self.values.extend_from_slice(&it[at].to_le_bytes());
                if stats {
                    self.stats.int(it[at].into());
                }
            }
            Values::U64(it) => {
                self.values.extend_from_slice(&it[at].to_le_bytes());
                if stats {
                    self.stats.int(it[at].into());
                }
            }
            Values::F64(it) => {
                self.values.extend_from_slice(&it[at].to_le_bytes());
                if stats {
                    self.stats.float(it[at]);
                }
            }
            Values::Text(texts) => self.text(texts.get(at), stats, alloc),
            Values::Dict { keys, values } => self.text(values.get(keys[at].widen_to::<usize>()), stats, alloc),
        }
    }

    fn text(&mut self, text: &str, stats: bool, alloc: &A) {
        // A text's length is below `u32::MAX`: `Texts` holds its offsets as `u32`s.
        self.values.extend_from_slice(&text.len().narrow_or_stop::<u32>().to_le_bytes());
        self.values.extend_from_slice(text.as_bytes());
        if stats {
            self.stats.text(text.as_bytes(), alloc);
        }
    }

    /// Rows `rows` of `column` appended, a page ending at each row that fills one.
    fn push(&mut self, column: &Column<A>, rows: Range<usize>, options: &Options, alloc: &A) -> Result<(), ParquetError> {
        let values = column.buffer();
        let valid = column.validity();
        let max_def = self.leaf.max_def;
        match column.list_offsets() {
            None => {
                for row in rows {
                    let present = valid.is_none_or(|it| it[row]);
                    if self.leaf.nullable {
                        self.defs.push(u8::from(present));
                    }
                    if present {
                        self.put(values, row, alloc);
                    } else {
                        self.stats.nulls += 1;
                    }
                    self.page_values += 1;
                    self.end_row(options)?;
                }
            }
            Some(offsets) => {
                for row in rows {
                    if valid.is_some_and(|it| !it[row]) {
                        self.defs.push(0);
                        self.reps.push(0);
                        self.page_values += 1;
                        self.stats.nulls += 1;
                    } else {
                        let elements = offsets[row].widen_to::<usize>()..offsets[row + 1].widen_to::<usize>();
                        if elements.is_empty() {
                            self.defs.push(max_def - 1);
                            self.reps.push(0);
                            self.page_values += 1;
                        }
                        for at in elements.clone() {
                            self.defs.push(max_def);
                            self.reps.push(u8::from(at != elements.start));
                            self.put(values, at, alloc);
                            self.page_values += 1;
                        }
                    }
                    self.end_row(options)?;
                }
            }
        }
        Ok(())
    }

    fn end_row(&mut self, options: &Options) -> Result<(), ParquetError> {
        self.page_rows += 1;
        if self.page_rows >= options.page_rows || self.values.len() + self.bools.len() / 8 >= options.page_bytes {
            self.end_page()?;
        }
        Ok(())
    }

    /// The page being filled, ended: its header and body appended to the chunk's pages.
    fn end_page(&mut self) -> Result<(), ParquetError> {
        if self.page_values == 0 {
            return Ok(());
        }
        self.body.clear();
        if self.leaf.max_rep > 0 {
            prefixed(&mut self.body, &self.reps, levels::bit_width(self.leaf.max_rep.into()))?;
        }
        if self.leaf.max_def > 0 {
            prefixed(&mut self.body, &self.defs, levels::bit_width(self.leaf.max_def.into()))?;
        }
        if self.leaf.scalar == Scalar::Bool {
            for group in self.bools.chunks(8) {
                let mut byte = 0u8;
                for (bit, value) in group.iter().enumerate() {
                    byte |= value << bit;
                }
                self.body.push(byte);
            }
        } else {
            self.body.extend_from_slice(&self.values);
        }
        let size = self.body.len().narrow::<i32>().map_err(|_| ParquetError::TooLarge {
            column: column_name("a page"),
            what: "a page past 2 GiB",
        })?;
        let header = PageHeader {
            ty: page::DATA_PAGE,
            uncompressed_page_size: size,
            compressed_page_size: size,
            data: Some(meta::DataHeader {
                num_values: self.page_values.narrow_or_stop(),
                encoding: encoding::PLAIN,
                definition_level_encoding: encoding::RLE,
                repetition_level_encoding: encoding::RLE,
                definition_levels_byte_length: 0,
                repetition_levels_byte_length: 0,
                is_compressed: false,
            }),
            dictionary: None,
        };
        meta::write_page_header(&mut self.pages, &header, None);
        self.pages.extend_from_slice(&self.body);
        self.chunk_values += self.page_values.widen_to::<u64>().cast_signed();
        self.values.clear();
        self.bools.clear();
        self.defs.clear();
        self.reps.clear();
        self.page_values = 0;
        self.page_rows = 0;
        Ok(())
    }
}

/// `levels` in the hybrid after their length as four little-endian bytes, as data page v1
/// has them.
fn prefixed<A: Allocator>(out: &mut Vec<u8, A>, values: &[u8], width: u8) -> Result<(), ParquetError> {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    levels::encode(out, values, width);
    let len = (out.len() - at - 4).narrow::<u32>().map_err(|_| ParquetError::TooLarge {
        column: column_name("a page's levels"),
        what: "levels past 4 GiB",
    })?;
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// A Parquet file being written: batches in, row groups cut at [`Options::row_group_rows`].
pub struct Writer<A: Allocator + Clone> {
    alloc: A,
    options: Options,
    fields: Vec<Field<A>, A>,
    chunks: Vec<Chunk<A>, A>,
    out: Vec<u8, A>,
    groups: Vec<RowGroup<A>, A>,
    group_rows: usize,
    rows: i64,
}

impl<A: Allocator + Clone> Writer<A> {
    /// A file of `fields`.
    pub fn new(fields: &[Field<A>], options: Options, alloc: A) -> Writer<A> {
        let mut chunks = Vec::with_capacity_in(fields.len(), alloc.clone());
        for field in fields {
            chunks.push(Chunk::new(Leaf::of(field.ty, field.nullable), &alloc));
        }
        let mut out = Vec::new_in(alloc.clone());
        out.extend_from_slice(&MAGIC);
        Writer {
            fields: Vec::from_iter_in(fields.iter().cloned(), alloc.clone()),
            chunks,
            out,
            groups: Vec::new_in(alloc.clone()),
            group_rows: 0,
            rows: 0,
            options,
            alloc,
        }
    }

    /// `batch`'s rows appended: refused unless its fields are the file's.
    pub fn write(&mut self, batch: &Batch<A>) -> Result<(), ParquetError> {
        if batch.fields().len() != self.fields.len() {
            return Err(ParquetError::Field { column: column_name(""), why: "a batch of another number of columns" });
        }
        for (have, want) in batch.fields().iter().zip(self.fields.iter()) {
            if have.name() != want.name() || have.ty != want.ty || (have.nullable && !want.nullable) {
                return Err(ParquetError::Field {
                    column: column_name(have.name()),
                    why: "a batch's column of another name, type or nullability than the file's",
                });
            }
        }
        let rows = batch.num_rows();
        let mut start = 0;
        while start < rows {
            let take = (rows - start).min(self.options.row_group_rows.max(1) - self.group_rows);
            for (chunk, column) in self.chunks.iter_mut().zip(batch.columns()) {
                chunk.push(column, start..start + take, &self.options, &self.alloc)?;
            }
            self.group_rows += take;
            start += take;
            if self.group_rows >= self.options.row_group_rows.max(1) {
                self.end_group()?;
            }
        }
        Ok(())
    }

    fn end_group(&mut self) -> Result<(), ParquetError> {
        if self.group_rows == 0 {
            return Ok(());
        }
        let mut columns = Vec::with_capacity_in(self.chunks.len(), self.alloc.clone());
        let group_start = self.out.len();
        for (chunk, field) in self.chunks.iter_mut().zip(self.fields.iter()) {
            chunk.end_page()?;
            let offset = self.out.len().widen_to::<u64>().cast_signed();
            let size = chunk.pages.len().widen_to::<u64>().cast_signed();
            self.out.extend_from_slice(&chunk.pages);
            chunk.pages.clear();
            let leaf = chunk.leaf;
            let statistics = chunk.stats.finish(leaf, &self.alloc);
            let mut path = Vec::new_in(self.alloc.clone());
            path.push(String::from_str_in(field.name(), self.alloc.clone()));
            if leaf.list {
                path.push(String::from_str_in("list", self.alloc.clone()));
                path.push(String::from_str_in("element", self.alloc.clone()));
            }
            let mut encodings = Vec::new_in(self.alloc.clone());
            encodings.push(encoding::PLAIN);
            encodings.push(encoding::RLE);
            columns.push(ColumnChunk {
                file_path: None,
                file_offset: offset,
                meta: Some(ColumnMeta {
                    ty: leaf.physical(),
                    encodings,
                    path,
                    codec: codec::UNCOMPRESSED,
                    num_values: chunk.chunk_values,
                    total_uncompressed_size: size,
                    total_compressed_size: size,
                    data_page_offset: offset,
                    index_page_offset: None,
                    dictionary_page_offset: None,
                    statistics: (!leaf.list).then_some(statistics),
                }),
            });
            chunk.chunk_values = 0;
        }
        let size = (self.out.len() - group_start).widen_to::<u64>().cast_signed();
        let ordinal = i16::try_from(self.groups.len()).ok();
        let rows = self.group_rows.widen_to::<u64>().cast_signed();
        self.groups.push(RowGroup {
            columns,
            total_byte_size: size,
            num_rows: rows,
            file_offset: Some(group_start.widen_to::<u64>().cast_signed()),
            total_compressed_size: Some(size),
            ordinal,
        });
        self.rows += rows;
        self.group_rows = 0;
        Ok(())
    }

    /// The file's bytes: its last row group, the footer with `metadata` as its key-value
    /// metadata, the footer's length and `PAR1`.
    pub fn finish(mut self, metadata: &[(&str, &str)]) -> Result<Vec<u8, A>, ParquetError> {
        self.end_group()?;
        let alloc = self.alloc.clone();
        let mut key_value = Vec::with_capacity_in(metadata.len(), alloc.clone());
        for (key, value) in metadata {
            key_value.push(KeyValue {
                key: String::from_str_in(key, alloc.clone()),
                value: Some(String::from_str_in(value, alloc.clone())),
            });
        }
        let footer = FileMeta {
            version: 1,
            schema: schema(&self.fields, &alloc),
            num_rows: self.rows,
            row_groups: core::mem::replace(&mut self.groups, Vec::new_in(alloc.clone())),
            key_value,
            created_by: Some(String::from_str_in(self.options.created_by, alloc.clone())),
            column_orders: self.fields.len(),
        };
        let start = self.out.len();
        meta::write_file_meta(&mut self.out, &footer);
        let len = (self.out.len() - start).narrow::<u32>().map_err(|_| ParquetError::TooLarge {
            column: column_name(""),
            what: "a footer past 4 GiB",
        })?;
        self.out.extend_from_slice(&len.to_le_bytes());
        self.out.extend_from_slice(&MAGIC);
        Ok(self.out)
    }
}

/// A file of `fields` holding `batches`, with `metadata` as its key-value metadata.
pub fn write<A: Allocator + Clone>(
    fields: &[Field<A>],
    batches: &[Batch<A>],
    metadata: &[(&str, &str)],
    options: Options,
    alloc: A,
) -> Result<Vec<u8, A>, ParquetError> {
    let mut writer = Writer::new(fields, options, alloc);
    for batch in batches {
        writer.write(batch)?;
    }
    writer.finish(metadata)
}
