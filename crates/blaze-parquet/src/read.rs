//! The reader: a Parquet file's bytes in place, its columns read into batches.
//!
//! It reads what blaze reads back: its own files (every encoding and codec the writer writes)
//! and DuckDB's answers. Every
//! offset and length the file gives is held to the bytes it points into, and every count to
//! what those bytes can hold, so a hostile file is a typed refusal, never a large allocation.

use core::alloc::Allocator;

use blaze_columns::{Batch, Column, Field, Name, Scalar, Texts, Type, Values};
use blaze_int::Int;
use blaze_own::{String, Vec};

use crate::{
    MAGIC, ParquetError, column_name, delta, levels,
    meta::{
        self, ColumnMeta, FileMeta, Logical, SchemaElement, codec, converted, encoding, page, physical,
        repetition,
    },
};

/// How a top-level column is read: its leaf, its levels and the type it is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Leaf {
    physical: i32,
    scalar: Scalar,
    /// A DECIMAL's scale: its values are read as `F64`s, divided by ten to it.
    scale: Option<i32>,
    list: bool,
    /// Whether the column (the list, for a list) may be null.
    nullable: bool,
    /// A list's elements may be null by the schema (DuckDB's lists say so): one that is, is
    /// refused.
    element_optional: bool,
    max_def: u8,
    max_rep: u8,
}

impl Leaf {
    /// The definition level of an empty list (and one below it, a null list).
    fn list_def(self) -> u8 {
        u8::from(self.nullable)
    }
}

fn unsupported(name: &str, what: &'static str, code: i64) -> ParquetError {
    ParquetError::Unsupported { column: column_name(name), what, code }
}

fn corrupt(name: &str, what: &'static str) -> ParquetError {
    ParquetError::Corrupt { column: column_name(name), what }
}

/// What a leaf of `element` is read as.
fn scalar_of<A: Allocator>(element: &SchemaElement<A>) -> Result<(i32, Scalar, Option<i32>), ParquetError> {
    let name = element.name.as_str();
    let ty = element.ty.ok_or_else(|| corrupt(name, "a leaf of no physical type"))?;
    let decimal = matches!(element.logical, Some(Logical::Decimal { .. })) || element.converted == Some(converted::DECIMAL);
    let scale = || match element.logical {
        Some(Logical::Decimal { scale, .. }) => scale,
        _ => element.scale.unwrap_or(0),
    };
    let unsigned = match element.logical {
        Some(Logical::Integer { signed, bits }) => (!signed).then_some(bits),
        _ => match element.converted {
            Some(converted::UINT_8) => Some(8),
            Some(converted::UINT_16) => Some(16),
            Some(converted::UINT_32) => Some(32),
            Some(converted::UINT_64) => Some(64),
            _ => None,
        },
    };
    let scalar = match ty {
        physical::BOOLEAN => Scalar::Bool,
        physical::INT32 if decimal => return Ok((ty, Scalar::F64, Some(scale()))),
        physical::INT32 => match unsigned {
            Some(8 | 16) => Scalar::U16,
            Some(_) => Scalar::U32,
            None => Scalar::I32,
        },
        physical::INT64 if decimal => return Ok((ty, Scalar::F64, Some(scale()))),
        physical::INT64 => match unsigned {
            Some(_) => Scalar::U64,
            None => Scalar::I64,
        },
        physical::FLOAT | physical::DOUBLE => Scalar::F64,
        physical::BYTE_ARRAY if decimal => return Err(unsupported(name, "a DECIMAL in bytes", ty.into())),
        physical::BYTE_ARRAY => Scalar::Text,
        physical::INT96 => return Err(unsupported(name, "an INT96 column", ty.into())),
        physical::FIXED_LEN_BYTE_ARRAY => {
            return Err(unsupported(name, "a FIXED_LEN_BYTE_ARRAY column (a DECIMAL past 18 digits, a UUID)", ty.into()));
        }
        other => return Err(unsupported(name, "a column of an unknown physical type", other.into())),
    };
    Ok((ty, scalar, None))
}

/// The top-level columns of `schema` (its root first), each a leaf or a 3-level list.
fn leaves<A: Allocator + Copy>(
    schema: &[SchemaElement<A>],
    alloc: A,
) -> Result<(Vec<Leaf, A>, Vec<Field<A>, A>), ParquetError> {
    let root = &schema[0];
    let children = root.num_children.unwrap_or(0).narrow::<usize>().map_err(|_| corrupt("", "a root of negative children"))?;
    let mut out = Vec::with_capacity_in(children.min(schema.len()), alloc);
    let mut fields = Vec::with_capacity_in(children.min(schema.len()), alloc);
    let mut at = 1;
    for _ in 0..children {
        let element = schema.get(at).ok_or_else(|| corrupt("", "a schema of fewer elements than its root's children"))?;
        let name = element.name.as_str();
        let optional = match element.repetition {
            Some(repetition::REQUIRED) => false,
            Some(repetition::OPTIONAL) => true,
            _ => return Err(unsupported(name, "a repeated top-level column", 2)),
        };
        let leaf = match element.num_children {
            None | Some(0) if element.ty.is_some() => {
                at += 1;
                let (physical, scalar, scale) = scalar_of(element)?;
                Leaf {
                    physical,
                    scalar,
                    scale,
                    list: false,
                    nullable: optional,
                    element_optional: false,
                    max_def: u8::from(optional),
                    max_rep: 0,
                }
            }
            Some(1) if element.converted == Some(converted::LIST) || element.logical == Some(Logical::List) => {
                let repeated = schema.get(at + 1).ok_or_else(|| corrupt(name, "a list of no repeated group"))?;
                let value = schema.get(at + 2).ok_or_else(|| corrupt(name, "a list of no element"))?;
                if repeated.repetition != Some(repetition::REPEATED)
                    || repeated.num_children != Some(1)
                    || value.ty.is_none()
                    || value.num_children.is_some_and(|it| it > 0)
                {
                    return Err(unsupported(name, "a list not of three levels over a leaf", 0));
                }
                let element_optional = match value.repetition {
                    Some(repetition::REQUIRED) => false,
                    Some(repetition::OPTIONAL) => true,
                    _ => return Err(unsupported(name, "a list of repeated elements", 2)),
                };
                at += 3;
                let (physical, scalar, scale) = scalar_of(value)?;
                Leaf {
                    physical,
                    scalar,
                    scale,
                    list: true,
                    nullable: optional,
                    element_optional,
                    max_def: u8::from(optional) + 1 + u8::from(element_optional),
                    max_rep: 1,
                }
            }
            _ => return Err(unsupported(name, "a nested column (a struct or a map)", 0)),
        };
        let ty = if leaf.list { Type::List(leaf.scalar) } else { Type::One(leaf.scalar) };
        fields.push(Field { name: Name::Owned(String::from_str_in(name, alloc)), ty, nullable: leaf.nullable });
        out.push(leaf);
    }
    if at != schema.len() {
        return Err(corrupt("", "a schema of more elements than its root's children hold"));
    }
    Ok((out, fields))
}

/// The integer statistics of a column chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntStats {
    pub nulls: Option<i64>,
    /// The least and greatest value, in the column's own order (unsigned for an unsigned one).
    pub range: Option<(i128, i128)>,
}

/// A Parquet file, its footer read.
pub struct File<'a, A: Allocator + Copy> {
    bytes: &'a [u8],
    /// Where the footer begins: no column chunk reaches past it.
    data_end: usize,
    meta: FileMeta<A>,
    leaves: Vec<Leaf, A>,
    fields: Vec<Field<A>, A>,
    alloc: A,
}

impl<'a, A: Allocator + Copy> File<'a, A> {
    /// `bytes` as a Parquet file: its framing checked and its footer read.
    pub fn open(bytes: &'a [u8], alloc: A) -> Result<File<'a, A>, ParquetError> {
        let (meta, data_end) = footer(bytes, alloc)?;
        let (leaves, fields) = leaves(&meta.schema, alloc)?;
        for group in meta.row_groups.iter() {
            if group.columns.len() != leaves.len() {
                return Err(corrupt("", "a row group of another number of columns than the schema's leaves"));
            }
        }
        Ok(File { bytes, data_end, meta, leaves, fields, alloc })
    }

    pub fn meta(&self) -> &FileMeta<A> {
        &self.meta
    }

    /// The file's columns.
    pub fn fields(&self) -> &[Field<A>] {
        &self.fields
    }

    pub fn num_rows(&self) -> i64 {
        self.meta.num_rows
    }

    pub fn row_groups(&self) -> usize {
        self.meta.row_groups.len()
    }

    /// The rows of row group `group`.
    pub fn group_rows(&self, group: usize) -> i64 {
        self.meta.row_groups[group].num_rows
    }

    /// The column named `name`.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|it| it.name() == name)
    }

    /// The value of the footer's key-value metadata `key`.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.meta.value(key)
    }

    /// The statistics of column `column` in row group `group`, for an integer column.
    pub fn int_stats(&self, group: usize, column: usize) -> Option<IntStats> {
        let leaf = self.leaves[column];
        let meta = self.meta.row_groups[group].columns[column].meta.as_ref()?;
        let stats = meta.statistics.as_ref()?;
        let unsigned = matches!(leaf.scalar, Scalar::U16 | Scalar::U32 | Scalar::U64);
        let decode = |raw: &[u8]| -> Option<i128> {
            match (leaf.physical, leaf.scale) {
                (physical::INT32, None) => {
                    let raw = i32::from_le_bytes(raw.try_into().ok()?);
                    Some(if unsigned { raw.cast_unsigned().into() } else { raw.into() })
                }
                (physical::INT64, None) => {
                    let raw = i64::from_le_bytes(raw.try_into().ok()?);
                    Some(if unsigned { raw.cast_unsigned().into() } else { raw.into() })
                }
                _ => None,
            }
        };
        // The deprecated `min`/`max` are in signed order: read only for a signed column.
        let low = stats.min_value.as_deref().or(if unsigned { None } else { stats.min.as_deref() });
        let high = stats.max_value.as_deref().or(if unsigned { None } else { stats.max.as_deref() });
        let range = match (low.and_then(decode), high.and_then(decode)) {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
        };
        Some(IntStats { nulls: stats.null_count, range })
    }

    /// Row group `group`'s columns `columns` (by index) as a batch.
    pub fn read_group(&self, group: usize, columns: &[usize]) -> Result<Batch<A>, ParquetError> {
        let rows = self.meta.row_groups[group].num_rows.narrow::<usize>().map_err(|_| corrupt("", "a row group of negative rows"))?;
        let mut fields = Vec::with_capacity_in(columns.len(), self.alloc);
        let mut out = Vec::with_capacity_in(columns.len(), self.alloc);
        for &column in columns {
            let field = self.fields.get(column).ok_or(ParquetError::Field { column: column_name(""), why: "a column past the file's" })?;
            let meta = self.meta.row_groups[group].columns[column]
                .meta
                .as_ref()
                .ok_or_else(|| unsupported(field.name(), "a column chunk in another file", 0))?;
            let read = self.chunk(field.name(), self.leaves[column], meta, rows)?;
            let name = match &field.name {
                Name::Static(s) => Name::Static(s),
                Name::Owned(s) => Name::Owned(s.clone_in(self.alloc)),
            };
            fields.push(Field { name, ty: field.ty, nullable: field.nullable });
            out.push(read);
        }
        Batch::try_new(fields, out).map_err(|error| ParquetError::Column { column: column_name(""), error })
    }

    /// Every row group's columns `columns`, a batch each.
    pub fn read(&self, columns: &[usize]) -> Result<Vec<Batch<A>, A>, ParquetError> {
        let mut out = Vec::with_capacity_in(self.row_groups(), self.alloc);
        for group in 0..self.row_groups() {
            out.push(self.read_group(group, columns)?);
        }
        Ok(out)
    }

    /// Every column the file has, a batch a row group.
    pub fn read_all(&self) -> Result<Vec<Batch<A>, A>, ParquetError> {
        let all: Vec<usize, A> = Vec::from_iter_in(0..self.fields.len(), self.alloc);
        self.read(&all)
    }

    /// The columns named `names`, a batch a row group; a name the file lacks is refused.
    pub fn read_named(&self, names: &[&str]) -> Result<Vec<Batch<A>, A>, ParquetError> {
        let mut columns = Vec::with_capacity_in(names.len(), self.alloc);
        for name in names {
            columns.push(
                self.column(name)
                    .ok_or(ParquetError::Field { column: column_name(name), why: "a column the file does not have" })?,
            );
        }
        self.read(&columns)
    }

    /// One column chunk, read.
    fn chunk(&self, name: &str, leaf: Leaf, meta: &ColumnMeta<A>, rows: usize) -> Result<Column<A>, ParquetError> {
        if meta.ty != leaf.physical {
            return Err(corrupt(name, "a column chunk of another type than its schema's"));
        }
        let start = meta.start().narrow::<usize>().map_err(|_| corrupt(name, "a chunk at a negative offset"))?;
        let len = meta.total_compressed_size.narrow::<usize>().map_err(|_| corrupt(name, "a chunk of negative size"))?;
        let end = start.checked_add(len).filter(|&it| it <= self.data_end && start >= MAGIC.len());
        let end = end.ok_or_else(|| corrupt(name, "a chunk past the file's data"))?;
        let bytes = &self.bytes[start..end];
        let entries = meta.num_values.narrow::<usize>().map_err(|_| corrupt(name, "a chunk of negative values"))?;
        let mut state = State::new(name, leaf, rows, self.alloc);
        let mut at = 0;
        let mut dict: Option<Sink<A>> = None;
        let mut inflated = Vec::new_in(self.alloc);
        while state.entries < entries {
            let (header, header_len) = meta::read_page_header(&bytes[at..])?;
            let body_len = header.compressed_page_size.narrow::<usize>().map_err(|_| corrupt(name, "a page of negative size"))?;
            let body_start = at + header_len;
            let stored = bytes
                .get(body_start..body_start.checked_add(body_len).ok_or_else(|| corrupt(name, "a page past memory"))?)
                .ok_or(ParquetError::Truncated { what: "a page", at: start + body_start })?;
            at = body_start + body_len;
            let body = self.inflate(name, meta.codec, &header, stored, &mut inflated)?;
            match header.ty {
                page::DICTIONARY_PAGE => {
                    let (count, how) = header.dictionary.ok_or_else(|| corrupt(name, "a dictionary page of no header"))?;
                    if how != encoding::PLAIN && how != encoding::PLAIN_DICTIONARY {
                        return Err(unsupported(name, "a dictionary of another encoding than PLAIN", how.into()));
                    }
                    let count = count.narrow::<usize>().map_err(|_| corrupt(name, "a dictionary of negative values"))?;
                    if count > body.len().saturating_mul(8) {
                        return Err(corrupt(name, "a dictionary of more values than its bytes hold"));
                    }
                    let mut sink = Sink::new(leaf.scalar, count, self.alloc);
                    let mut cursor = Plain { bytes: body, at: 0, bit: 0 };
                    for _ in 0..count {
                        cursor.next(&mut sink, leaf, name)?;
                    }
                    dict = Some(sink);
                }
                page::DATA_PAGE | page::DATA_PAGE_V2 => {
                    let data = header.data.ok_or_else(|| corrupt(name, "a data page of no header"))?;
                    state.page(body, &data, header.ty == page::DATA_PAGE_V2, dict.as_ref())?;
                }
                page::INDEX_PAGE => {}
                other => return Err(unsupported(name, "a page of an unknown type", other.into())),
            }
            if state.entries < entries && at >= bytes.len() {
                return Err(corrupt(name, "a chunk whose pages hold fewer values than it says"));
            }
        }
        state.finish()
    }

    /// A page's body as it was before `codec` compressed it: `stored` itself when it was not,
    /// else decompressed into `inflated` (a v2 page's levels stay as they are, before its
    /// values).
    fn inflate<'p>(
        &self,
        name: &str,
        codec: i32,
        header: &meta::PageHeader,
        stored: &'p [u8],
        inflated: &'p mut Vec<u8, A>,
    ) -> Result<&'p [u8], ParquetError> {
        if codec == codec::UNCOMPRESSED {
            return Ok(stored);
        }
        let size = header.uncompressed_page_size.narrow::<usize>().map_err(|_| corrupt(name, "a page of negative size"))?;
        let levels = match &header.data {
            Some(data) if header.ty == page::DATA_PAGE_V2 => {
                if !data.is_compressed {
                    return Ok(stored);
                }
                let rep = data.repetition_levels_byte_length.narrow::<usize>();
                let def = data.definition_levels_byte_length.narrow::<usize>();
                match (rep, def) {
                    (Ok(rep), Ok(def)) => rep.saturating_add(def),
                    _ => return Err(corrupt(name, "levels of negative length")),
                }
            }
            _ => 0,
        };
        if levels > stored.len() || levels > size {
            return Err(corrupt(name, "a page's levels past its bytes"));
        }
        inflated.clear();
        inflated.extend_from_slice(&stored[..levels]);
        crate::compress::decompress(name, codec, &stored[levels..], size - levels, inflated)?;
        Ok(inflated)
    }
}

/// The footer of `bytes`, and where it begins.
pub(crate) fn footer<A: Allocator + Copy>(bytes: &[u8], alloc: A) -> Result<(FileMeta<A>, usize), ParquetError> {
    if bytes.len() < 12 {
        return Err(ParquetError::NotParquet { why: "fewer than 12 bytes" });
    }
    if bytes[..4] != MAGIC || bytes[bytes.len() - 4..] != MAGIC {
        return Err(ParquetError::NotParquet { why: "no PAR1 at its start and end (an encrypted footer is PARE)" });
    }
    let len_at = bytes.len() - 8;
    let mut len = [0u8; 4];
    len.copy_from_slice(&bytes[len_at..len_at + 4]);
    let len = u32::from_le_bytes(len).widen_to::<usize>();
    if len > len_at - 4 {
        return Err(ParquetError::NotParquet { why: "a footer longer than the file" });
    }
    let start = len_at - len;
    Ok((meta::read_file_meta(&bytes[start..len_at], alloc)?, start))
}

/// A column's values as they are read: one buffer of the type the column is read as.
enum Sink<A: Allocator> {
    Bool(Vec<bool, A>),
    I32(Vec<i32, A>),
    U16(Vec<u16, A>),
    U32(Vec<u32, A>),
    I64(Vec<i64, A>),
    U64(Vec<u64, A>),
    F64(Vec<f64, A>),
    Text(Texts<A>),
}

impl<A: Allocator + Copy> Sink<A> {
    fn new(scalar: Scalar, capacity: usize, alloc: A) -> Sink<A> {
        match scalar {
            Scalar::Bool => Sink::Bool(Vec::with_capacity_in(capacity, alloc)),
            Scalar::I32 => Sink::I32(Vec::with_capacity_in(capacity, alloc)),
            Scalar::U16 => Sink::U16(Vec::with_capacity_in(capacity, alloc)),
            Scalar::U32 => Sink::U32(Vec::with_capacity_in(capacity, alloc)),
            Scalar::I64 => Sink::I64(Vec::with_capacity_in(capacity, alloc)),
            Scalar::U64 => Sink::U64(Vec::with_capacity_in(capacity, alloc)),
            Scalar::F64 => Sink::F64(Vec::with_capacity_in(capacity, alloc)),
            Scalar::Text | Scalar::Dict => Sink::Text(Texts::with_capacity_in(capacity, 0, alloc)),
        }
    }

    fn len(&self) -> usize {
        match self {
            Sink::Bool(it) => it.len(),
            Sink::I32(it) => it.len(),
            Sink::U16(it) => it.len(),
            Sink::U32(it) => it.len(),
            Sink::I64(it) => it.len(),
            Sink::U64(it) => it.len(),
            Sink::F64(it) => it.len(),
            Sink::Text(it) => it.len(),
        }
    }

    /// A null row's place.
    fn zero(&mut self, name: &str) -> Result<(), ParquetError> {
        match self {
            Sink::Bool(it) => it.push(false),
            Sink::I32(it) => it.push(0),
            Sink::U16(it) => it.push(0),
            Sink::U32(it) => it.push(0),
            Sink::I64(it) => it.push(0),
            Sink::U64(it) => it.push(0),
            Sink::F64(it) => it.push(0.0),
            Sink::Text(it) => it.try_push("").map_err(|error| ParquetError::Column { column: column_name(name), error })?,
        }
        Ok(())
    }

    /// Value `at` of the dictionary `from` (of the same type).
    fn copy(&mut self, from: &Sink<A>, at: usize, name: &str) -> Result<(), ParquetError> {
        if at >= from.len() {
            return Err(corrupt(name, "a dictionary index past its dictionary"));
        }
        match (self, from) {
            (Sink::Bool(it), Sink::Bool(from)) => it.push(from[at]),
            (Sink::I32(it), Sink::I32(from)) => it.push(from[at]),
            (Sink::U16(it), Sink::U16(from)) => it.push(from[at]),
            (Sink::U32(it), Sink::U32(from)) => it.push(from[at]),
            (Sink::I64(it), Sink::I64(from)) => it.push(from[at]),
            (Sink::U64(it), Sink::U64(from)) => it.push(from[at]),
            (Sink::F64(it), Sink::F64(from)) => it.push(from[at]),
            (Sink::Text(it), Sink::Text(from)) => {
                it.try_push(from.get(at)).map_err(|error| ParquetError::Column { column: column_name(name), error })?;
            }
            _ => return Err(corrupt(name, "a dictionary of another type than its column")),
        }
        Ok(())
    }

    fn into_values(self) -> Values<A> {
        match self {
            Sink::Bool(it) => Values::Bool(it),
            Sink::I32(it) => Values::I32(it),
            Sink::U16(it) => Values::U16(it),
            Sink::U32(it) => Values::U32(it),
            Sink::I64(it) => Values::I64(it),
            Sink::U64(it) => Values::U64(it),
            Sink::F64(it) => Values::F64(it),
            Sink::Text(it) => Values::Text(it),
        }
    }
}

/// Ten to `scale`, a DECIMAL's divisor (by multiplying: `no_std` has no `powi`).
fn ten_to(scale: i32) -> f64 {
    let mut out = 1.0f64;
    for _ in 0..scale.unsigned_abs().min(400) {
        out *= 10.0;
    }
    if scale < 0 { 1.0 / out } else { out }
}

#[expect(blaze::as_cast, reason = "a DECIMAL's unscaled integer to its nearest double, the one conversion `as` names")]
fn decimal(raw: i64, scale: i32) -> f64 {
    raw as f64 / ten_to(scale)
}

/// An INT32 value read into `sink` as the leaf says.
fn int32<A: Allocator>(sink: &mut Sink<A>, raw: i32, leaf: Leaf, name: &str) -> Result<(), ParquetError> {
    match sink {
        Sink::I32(it) => it.push(raw),
        Sink::U16(it) => {
            it.push(u16::try_from(raw.cast_unsigned()).map_err(|_| corrupt(name, "a 16-bit value past 16 bits"))?);
        }
        Sink::U32(it) => it.push(raw.cast_unsigned()),
        Sink::F64(it) => it.push(decimal(raw.into(), leaf.scale.unwrap_or(0))),
        _ => return Err(corrupt(name, "an INT32 read as another type")),
    }
    Ok(())
}

/// An INT64 value read into `sink` as the leaf says.
fn int64<A: Allocator>(sink: &mut Sink<A>, raw: i64, leaf: Leaf, name: &str) -> Result<(), ParquetError> {
    match sink {
        Sink::I64(it) => it.push(raw),
        Sink::U64(it) => it.push(raw.cast_unsigned()),
        Sink::F64(it) => it.push(decimal(raw, leaf.scale.unwrap_or(0))),
        _ => return Err(corrupt(name, "an INT64 read as another type")),
    }
    Ok(())
}

/// A text read into `sink`.
fn text<A: Allocator>(sink: &mut Sink<A>, raw: &[u8], name: &str) -> Result<(), ParquetError> {
    let Sink::Text(it) = sink else {
        return Err(corrupt(name, "a text in a column of another type"));
    };
    let text = core::str::from_utf8(raw).map_err(|_| corrupt(name, "a text not UTF-8"))?;
    it.try_push(text).map_err(|error| ParquetError::Column { column: column_name(name), error })
}

/// A length of DELTA_LENGTH_BYTE_ARRAY or DELTA_BYTE_ARRAY: not negative, and within `room`.
fn length(raw: i64, room: usize, name: &str) -> Result<usize, ParquetError> {
    usize::try_from(raw).ok().filter(|&it| it <= room).ok_or_else(|| corrupt(name, "a delta-encoded length that is negative or past its page"))
}

/// `present` values of a page in a delta encoding or RLE, decoded into `sink`.
fn decode<A: Allocator + Copy>(
    sink: &mut Sink<A>,
    how: i32,
    values: &[u8],
    present: usize,
    leaf: Leaf,
    name: &str,
    alloc: A,
) -> Result<(), ParquetError> {
    let bad = |_| corrupt(name, "delta-encoded values that end early or say something impossible");
    let mut failed = None;
    match (how, leaf.physical) {
        (encoding::RLE, physical::BOOLEAN) => {
            let len = values.get(..4).ok_or_else(|| corrupt(name, "a page ends inside its booleans' length"))?;
            let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]).widen_to::<usize>();
            let bytes = values.get(4..4usize.saturating_add(len)).ok_or_else(|| corrupt(name, "a page ends inside its booleans"))?;
            let Sink::Bool(out) = sink else {
                return Err(corrupt(name, "booleans in a column of another type"));
            };
            let before = out.len();
            levels::decode(bytes, 1, present, |it| out.push(it == 1)).map_err(|_| corrupt(name, "booleans that end early"))?;
            if out.len() - before != present {
                return Err(corrupt(name, "fewer booleans than the page's values"));
            }
        }
        (encoding::DELTA_BINARY_PACKED, physical::INT32 | physical::INT64) => {
            let (total, _) = delta::decode(values, present, |raw| {
                if failed.is_none() {
                    let pushed = if leaf.physical == physical::INT32 {
                        int32(sink, raw.truncate_to::<i32>(), leaf, name)
                    } else {
                        int64(sink, raw, leaf, name)
                    };
                    failed = pushed.err();
                }
            })
            .map_err(bad)?;
            if total != present {
                return Err(corrupt(name, "delta-encoded values of another number than the page's"));
            }
        }
        (encoding::DELTA_LENGTH_BYTE_ARRAY, physical::BYTE_ARRAY) => {
            let mut lengths = Vec::with_capacity_in(present, alloc);
            let (total, used) = delta::decode(values, present, |it| lengths.push(it)).map_err(bad)?;
            if total != present {
                return Err(corrupt(name, "delta-encoded lengths of another number than the page's values"));
            }
            let mut at = used;
            for &raw in lengths.iter() {
                let len = length(raw, values.len() - at, name)?;
                text(sink, &values[at..at + len], name)?;
                at += len;
            }
        }
        (encoding::DELTA_BYTE_ARRAY, physical::BYTE_ARRAY) => {
            let mut prefixes = Vec::with_capacity_in(present, alloc);
            let (total, used) = delta::decode(values, present, |it| prefixes.push(it)).map_err(bad)?;
            let mut suffixes = Vec::with_capacity_in(present, alloc);
            let (rest, more) = delta::decode(&values[used..], present, |it| suffixes.push(it)).map_err(bad)?;
            if total != present || rest != present {
                return Err(corrupt(name, "delta-encoded lengths of another number than the page's values"));
            }
            let mut at = used + more;
            let mut last: Vec<u8, A> = Vec::new_in(alloc);
            for (&prefix, &suffix) in prefixes.iter().zip(suffixes.iter()) {
                let prefix = length(prefix, last.len(), name)?;
                let suffix = length(suffix, values.len() - at, name)?;
                last.truncate(prefix);
                last.extend_from_slice(&values[at..at + suffix]);
                at += suffix;
                text(sink, &last, name)?;
            }
        }
        (other, _) => return Err(unsupported(name, "values of an encoding this reader does not read for their type", other.into())),
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// PLAIN values being read from a page.
struct Plain<'b> {
    bytes: &'b [u8],
    at: usize,
    /// The next bit of a boolean's byte.
    bit: u8,
}

impl<'b> Plain<'b> {
    fn take<const N: usize>(&mut self, name: &str) -> Result<[u8; N], ParquetError> {
        let raw = self.bytes.get(self.at..self.at + N).ok_or_else(|| corrupt(name, "a page ends inside a value"))?;
        self.at += N;
        let mut out = [0u8; N];
        out.copy_from_slice(raw);
        Ok(out)
    }

    /// The next value, read into `sink` as the leaf says.
    fn next<A: Allocator>(&mut self, sink: &mut Sink<A>, leaf: Leaf, name: &str) -> Result<(), ParquetError> {
        match (leaf.physical, sink) {
            (physical::BOOLEAN, Sink::Bool(it)) => {
                let byte = *self.bytes.get(self.at).ok_or_else(|| corrupt(name, "a page ends inside a value"))?;
                it.push(byte >> self.bit & 1 == 1);
                self.bit += 1;
                if self.bit == 8 {
                    self.bit = 0;
                    self.at += 1;
                }
            }
            (physical::INT32, sink) => int32(sink, i32::from_le_bytes(self.take::<4>(name)?), leaf, name)?,
            (physical::INT64, sink) => int64(sink, i64::from_le_bytes(self.take::<8>(name)?), leaf, name)?,
            (physical::FLOAT, Sink::F64(it)) => it.push(f64::from(f32::from_le_bytes(self.take::<4>(name)?))),
            (physical::DOUBLE, Sink::F64(it)) => it.push(f64::from_le_bytes(self.take::<8>(name)?)),
            (physical::BYTE_ARRAY, sink @ Sink::Text(_)) => {
                let len = u32::from_le_bytes(self.take::<4>(name)?).widen_to::<usize>();
                let end = self.at.checked_add(len).filter(|&it| it <= self.bytes.len());
                let end = end.ok_or_else(|| corrupt(name, "a page ends inside a text"))?;
                let raw = &self.bytes[self.at..end];
                self.at = end;
                text(sink, raw, name)?;
            }
            _ => return Err(corrupt(name, "a value of another type than its column")),
        }
        Ok(())
    }
}

/// How a data page's values are encoded.
enum Cursor<'b, 'd, A: Allocator> {
    Plain(Plain<'b>),
    Dict { indices: Vec<u32, A>, at: usize, dict: &'d Sink<A> },
    /// Values decoded whole (a delta encoding, RLE booleans), taken in order.
    Decoded { values: Sink<A>, at: usize },
}

/// A column chunk being read, page by page.
struct State<'n, A: Allocator + Copy> {
    name: &'n str,
    leaf: Leaf,
    rows: usize,
    alloc: A,
    values: Sink<A>,
    valid: Vec<bool, A>,
    offsets: Vec<u32, A>,
    /// The rows begun (a list's), and the level entries read.
    begun: usize,
    entries: usize,
    defs: Vec<u8, A>,
    reps: Vec<u8, A>,
}

impl<'n, A: Allocator + Copy> State<'n, A> {
    fn new(name: &'n str, leaf: Leaf, rows: usize, alloc: A) -> State<'n, A> {
        let mut offsets = Vec::new_in(alloc);
        if leaf.list {
            offsets.push(0);
        }
        State {
            name,
            leaf,
            rows,
            alloc,
            values: Sink::new(leaf.scalar, if leaf.list { 0 } else { rows }, alloc),
            valid: Vec::with_capacity_in(rows, alloc),
            offsets,
            begun: 0,
            entries: 0,
            defs: Vec::new_in(alloc),
            reps: Vec::new_in(alloc),
        }
    }

    /// `count` levels of at most `max` into `out`, from the hybrid in `bytes`.
    fn levels(&self, bytes: &[u8], max: u8, count: usize, out: &mut Vec<u8, A>) -> Result<(), ParquetError> {
        out.clear();
        let mut over = false;
        levels::decode(bytes, levels::bit_width(max.into()), count, |level| {
            over |= level > u32::from(max);
            out.push(level.truncate_to::<u8>());
        })
        .map_err(|_| corrupt(self.name, "levels that end early or say too much"))?;
        if over || out.len() != count {
            return Err(corrupt(self.name, "levels past their maximum, or fewer than the page's values"));
        }
        Ok(())
    }

    /// One data page.
    fn page(
        &mut self,
        body: &[u8],
        data: &meta::DataHeader,
        v2: bool,
        dict: Option<&Sink<A>>,
    ) -> Result<(), ParquetError> {
        let name = self.name;
        let leaf = self.leaf;
        let count = data.num_values.narrow::<usize>().map_err(|_| corrupt(name, "a page of negative values"))?;
        // An entry is a row (a null, an empty list) or a value of at least one bit.
        if count > self.rows.saturating_add(body.len().saturating_mul(8)) {
            return Err(ParquetError::TooLarge { column: column_name(name), what: "a page of more values than it can hold" });
        }
        let mut at = 0;
        let mut defs = core::mem::replace(&mut self.defs, Vec::new_in(self.alloc));
        let mut reps = core::mem::replace(&mut self.reps, Vec::new_in(self.alloc));
        let mut section = |len: Option<usize>, max: u8, out: &mut Vec<u8, A>, state: &Self| -> Result<(), ParquetError> {
            let len = match len {
                Some(it) => it,
                None => {
                    let raw = body.get(at..at + 4).ok_or_else(|| corrupt(name, "a page ends inside its levels' length"))?;
                    at += 4;
                    u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]).widen_to::<usize>()
                }
            };
            let bytes = body.get(at..at.checked_add(len).unwrap_or(usize::MAX)).ok_or_else(|| corrupt(name, "a page ends inside its levels"))?;
            at += len;
            state.levels(bytes, max, count, out)
        };
        if !v2 {
            for level in [data.repetition_level_encoding, data.definition_level_encoding] {
                if level != encoding::RLE {
                    return Err(unsupported(name, "levels not in the RLE hybrid", level.into()));
                }
            }
        }
        let lens = |len: i32| -> Result<Option<usize>, ParquetError> {
            if v2 { len.narrow::<usize>().map(Some).map_err(|_| corrupt(name, "levels of negative length")) } else { Ok(None) }
        };
        if leaf.max_rep > 0 {
            section(lens(data.repetition_levels_byte_length)?, leaf.max_rep, &mut reps, self)?;
        } else if v2 && data.repetition_levels_byte_length > 0 {
            return Err(corrupt(name, "repetition levels in a column that has none"));
        }
        if leaf.max_def > 0 {
            section(lens(data.definition_levels_byte_length)?, leaf.max_def, &mut defs, self)?;
        } else if v2 && data.definition_levels_byte_length > 0 {
            return Err(corrupt(name, "definition levels in a column that has none"));
        }
        let values = &body[at..];
        let present = if leaf.max_def == 0 { count } else { defs.iter().filter(|&&it| it == leaf.max_def).count() };
        let mut cursor = match data.encoding {
            encoding::PLAIN => Cursor::Plain(Plain { bytes: values, at: 0, bit: 0 }),
            encoding::RLE_DICTIONARY | encoding::PLAIN_DICTIONARY => {
                let dict = dict.ok_or_else(|| corrupt(name, "dictionary indices before a dictionary page"))?;
                let (&width, packed) = values.split_first().ok_or_else(|| corrupt(name, "a page of no indices' width"))?;
                let mut indices = Vec::with_capacity_in(present, self.alloc);
                levels::decode(packed, width, present, |it| indices.push(it))
                    .map_err(|_| corrupt(name, "dictionary indices that end early or say too much"))?;
                if indices.len() != present {
                    return Err(corrupt(name, "fewer dictionary indices than the page's values"));
                }
                Cursor::Dict { indices, at: 0, dict }
            }
            how => {
                let mut decoded = Sink::new(leaf.scalar, present, self.alloc);
                decode(&mut decoded, how, values, present, leaf, name, self.alloc)?;
                Cursor::Decoded { values: decoded, at: 0 }
            }
        };
        let mut next = |sink: &mut Sink<A>| -> Result<(), ParquetError> {
            match &mut cursor {
                Cursor::Plain(plain) => plain.next(sink, leaf, name),
                Cursor::Dict { indices, at, dict } => {
                    let index = indices.get(*at).ok_or_else(|| corrupt(name, "fewer dictionary indices than values"))?.widen_to::<usize>();
                    *at += 1;
                    sink.copy(*dict, index, name)
                }
                Cursor::Decoded { values, at } => {
                    *at += 1;
                    sink.copy(values, *at - 1, name)
                }
            }
        };
        for entry in 0..count {
            let def = if leaf.max_def > 0 { defs[entry] } else { 0 };
            if !leaf.list {
                if def == leaf.max_def {
                    next(&mut self.values)?;
                    self.valid.push(true);
                } else {
                    self.values.zero(name)?;
                    self.valid.push(false);
                }
                self.begun += 1;
                continue;
            }
            let rep = reps[entry];
            if rep == 0 {
                if self.begun > 0 {
                    self.offsets.push(self.values.len().narrow::<u32>().map_err(|_| corrupt(name, "a list column past 2^32 elements"))?);
                }
                self.begun += 1;
                if self.begun > self.rows {
                    return Err(corrupt(name, "a list column of more rows than its row group"));
                }
                self.valid.push(def >= leaf.list_def());
            }
            if def < leaf.list_def() || def == leaf.list_def() {
                // A null or an empty list: nothing more of the row.
                if rep != 0 {
                    return Err(corrupt(name, "a list's later entry that is not an element"));
                }
            } else if def == leaf.max_def {
                next(&mut self.values)?;
            } else {
                return Err(ParquetError::Unsupported { column: column_name(name), what: "a list with a null element", code: 0 });
            }
        }
        self.entries += count;
        if !leaf.list && self.begun > self.rows {
            return Err(corrupt(name, "a column of more rows than its row group"));
        }
        self.defs = defs;
        self.reps = reps;
        Ok(())
    }

    fn finish(mut self) -> Result<Column<A>, ParquetError> {
        let name = self.name;
        if self.begun != self.rows {
            return Err(corrupt(name, "a column of another number of rows than its row group"));
        }
        let wrap = |error| ParquetError::Column { column: column_name(name), error };
        let valid = (self.leaf.nullable && self.valid.contains(&false)).then_some(self.valid);
        if self.leaf.list {
            let end = self.values.len().narrow::<u32>().map_err(|_| corrupt(name, "a list column past 2^32 elements"))?;
            if self.begun > 0 {
                self.offsets.push(end);
            }
            Column::list(self.offsets, valid, self.values.into_values()).map_err(wrap)
        } else {
            Column::try_new(self.values.into_values(), valid).map_err(wrap)
        }
    }
}
