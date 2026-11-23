//! Parquet's footer and page headers (parquet-format's `parquet.thrift`), the fields this crate
//! writes and reads: each struct encoded and decoded by hand in the compact protocol. A field
//! a reader has no use for is skipped; a field of a known id and another type is refused.

use core::alloc::Allocator;

use blaze_own::{String, Vec};

use crate::{
    ParquetError,
    thrift::{BINARY, BYTE, I16, I32, I64, In, LIST, STRUCT, Struct},
};

/// `Type`: a leaf's physical type.
pub mod physical {
    pub const BOOLEAN: i32 = 0;
    pub const INT32: i32 = 1;
    pub const INT64: i32 = 2;
    pub const INT96: i32 = 3;
    pub const FLOAT: i32 = 4;
    pub const DOUBLE: i32 = 5;
    pub const BYTE_ARRAY: i32 = 6;
    pub const FIXED_LEN_BYTE_ARRAY: i32 = 7;
}

/// `FieldRepetitionType`.
pub mod repetition {
    pub const REQUIRED: i32 = 0;
    pub const OPTIONAL: i32 = 1;
    pub const REPEATED: i32 = 2;
}

/// `ConvertedType`, the annotations older readers read.
pub mod converted {
    pub const UTF8: i32 = 0;
    pub const MAP: i32 = 1;
    pub const MAP_KEY_VALUE: i32 = 2;
    pub const LIST: i32 = 3;
    pub const DECIMAL: i32 = 5;
    pub const DATE: i32 = 6;
    pub const TIME_MILLIS: i32 = 7;
    pub const TIMESTAMP_MILLIS: i32 = 9;
    pub const TIMESTAMP_MICROS: i32 = 10;
    pub const UINT_8: i32 = 11;
    pub const UINT_16: i32 = 12;
    pub const UINT_32: i32 = 13;
    pub const UINT_64: i32 = 14;
    pub const INT_8: i32 = 15;
    pub const INT_16: i32 = 16;
    pub const INT_32: i32 = 17;
    pub const INT_64: i32 = 18;
}

/// `Encoding`.
pub mod encoding {
    pub const PLAIN: i32 = 0;
    pub const PLAIN_DICTIONARY: i32 = 2;
    pub const RLE: i32 = 3;
    pub const BIT_PACKED: i32 = 4;
    pub const DELTA_BINARY_PACKED: i32 = 5;
    pub const DELTA_LENGTH_BYTE_ARRAY: i32 = 6;
    pub const DELTA_BYTE_ARRAY: i32 = 7;
    pub const RLE_DICTIONARY: i32 = 8;
}

/// `CompressionCodec`.
pub mod codec {
    pub const UNCOMPRESSED: i32 = 0;
    pub const ZSTD: i32 = 6;
    pub const LZ4_RAW: i32 = 7;
}

/// `PageType`.
pub mod page {
    pub const DATA_PAGE: i32 = 0;
    pub const INDEX_PAGE: i32 = 1;
    pub const DICTIONARY_PAGE: i32 = 2;
    pub const DATA_PAGE_V2: i32 = 3;
}

/// `TimeUnit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeUnit {
    Millis,
    Micros,
    Nanos,
}

/// `LogicalType`, the union, its arms this crate names; another arm is `Other` by its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logical {
    String,
    Map,
    List,
    Enum,
    Decimal { scale: i32, precision: i32 },
    Date,
    Time { utc: bool, unit: TimeUnit },
    Timestamp { utc: bool, unit: TimeUnit },
    Integer { bits: i8, signed: bool },
    Unknown,
    Json,
    Bson,
    Uuid,
    Float16,
    Other(i16),
}

/// `SchemaElement`: a node of the schema's tree, depth first.
pub struct SchemaElement<A: Allocator> {
    pub ty: Option<i32>,
    pub type_length: Option<i32>,
    pub repetition: Option<i32>,
    pub name: String<A>,
    pub num_children: Option<i32>,
    pub converted: Option<i32>,
    pub scale: Option<i32>,
    pub precision: Option<i32>,
    pub field_id: Option<i32>,
    pub logical: Option<Logical>,
}

impl<A: Allocator> PartialEq for SchemaElement<A> {
    fn eq(&self, other: &SchemaElement<A>) -> bool {
        self.ty == other.ty
            && self.type_length == other.type_length
            && self.repetition == other.repetition
            && self.name.as_str() == other.name.as_str()
            && self.num_children == other.num_children
            && self.converted == other.converted
            && self.scale == other.scale
            && self.precision == other.precision
            && self.field_id == other.field_id
            && self.logical == other.logical
    }
}

/// `Statistics`.
pub struct Statistics<A: Allocator> {
    pub max: Option<Vec<u8, A>>,
    pub min: Option<Vec<u8, A>>,
    pub null_count: Option<i64>,
    pub distinct_count: Option<i64>,
    pub max_value: Option<Vec<u8, A>>,
    pub min_value: Option<Vec<u8, A>>,
}

/// `ColumnMetaData`.
pub struct ColumnMeta<A: Allocator> {
    pub ty: i32,
    pub encodings: Vec<i32, A>,
    pub path: Vec<String<A>, A>,
    pub codec: i32,
    pub num_values: i64,
    pub total_uncompressed_size: i64,
    pub total_compressed_size: i64,
    pub data_page_offset: i64,
    pub index_page_offset: Option<i64>,
    pub dictionary_page_offset: Option<i64>,
    pub statistics: Option<Statistics<A>>,
}

impl<A: Allocator> ColumnMeta<A> {
    /// Where the chunk's bytes begin: its dictionary page, when it has one, else its first data
    /// page.
    pub fn start(&self) -> i64 {
        match self.dictionary_page_offset {
            Some(it) if it > 0 && it < self.data_page_offset => it,
            _ => self.data_page_offset,
        }
    }
}

/// `ColumnChunk`.
pub struct ColumnChunk<A: Allocator> {
    pub file_path: Option<String<A>>,
    pub file_offset: i64,
    pub meta: Option<ColumnMeta<A>>,
}

/// `RowGroup`.
pub struct RowGroup<A: Allocator> {
    pub columns: Vec<ColumnChunk<A>, A>,
    pub total_byte_size: i64,
    pub num_rows: i64,
    pub file_offset: Option<i64>,
    pub total_compressed_size: Option<i64>,
    pub ordinal: Option<i16>,
}

/// `KeyValue`.
pub struct KeyValue<A: Allocator> {
    pub key: String<A>,
    pub value: Option<String<A>>,
}

/// `FileMetaData`: the footer.
pub struct FileMeta<A: Allocator> {
    pub version: i32,
    pub schema: Vec<SchemaElement<A>, A>,
    pub num_rows: i64,
    pub row_groups: Vec<RowGroup<A>, A>,
    pub key_value: Vec<KeyValue<A>, A>,
    pub created_by: Option<String<A>>,
    /// How many `TypeDefinedOrder` column orders it gives (one a leaf when written): min and
    /// max are by the logical type's order.
    pub column_orders: usize,
}

impl<A: Allocator> FileMeta<A> {
    /// The value of key `key`, when it has one.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.key_value.iter().find(|it| it.key.as_str() == key)?.value.as_ref().map(|it| it.as_str())
    }
}

impl<A: Allocator + Copy> Statistics<A> {
    pub(crate) fn clone_in(&self, alloc: A) -> Statistics<A> {
        Statistics {
            max: self.max.as_ref().map(|v| v.clone_in(alloc)),
            min: self.min.as_ref().map(|v| v.clone_in(alloc)),
            null_count: self.null_count,
            distinct_count: self.distinct_count,
            max_value: self.max_value.as_ref().map(|v| v.clone_in(alloc)),
            min_value: self.min_value.as_ref().map(|v| v.clone_in(alloc)),
        }
    }
}

impl<A: Allocator + Copy> ColumnMeta<A> {
    pub(crate) fn clone_in(&self, alloc: A) -> ColumnMeta<A> {
        ColumnMeta {
            ty: self.ty,
            encodings: self.encodings.clone_in(alloc),
            path: self.path.clone_in(alloc),
            codec: self.codec,
            num_values: self.num_values,
            total_uncompressed_size: self.total_uncompressed_size,
            total_compressed_size: self.total_compressed_size,
            data_page_offset: self.data_page_offset,
            index_page_offset: self.index_page_offset,
            dictionary_page_offset: self.dictionary_page_offset,
            statistics: self.statistics.as_ref().map(|s| s.clone_in(alloc)),
        }
    }
}

impl<A: Allocator + Copy> KeyValue<A> {
    pub(crate) fn clone_in(&self, alloc: A) -> KeyValue<A> {
        KeyValue { key: self.key.clone_in(alloc), value: self.value.as_ref().map(|v| v.clone_in(alloc)) }
    }
}

impl<A: Allocator + Copy> Clone for SchemaElement<A> {
    fn clone(&self) -> SchemaElement<A> {
        let alloc = *self.name.allocator();
        SchemaElement {
            ty: self.ty,
            type_length: self.type_length,
            repetition: self.repetition,
            name: self.name.clone_in(alloc),
            num_children: self.num_children,
            converted: self.converted,
            scale: self.scale,
            precision: self.precision,
            field_id: self.field_id,
            logical: self.logical,
        }
    }
}

impl<A: Allocator + Copy> Clone for Statistics<A> {
    fn clone(&self) -> Statistics<A> {
        Statistics {
            max: self.max.as_ref().map(|v| v.clone_in(*v.allocator())),
            min: self.min.as_ref().map(|v| v.clone_in(*v.allocator())),
            null_count: self.null_count,
            distinct_count: self.distinct_count,
            max_value: self.max_value.as_ref().map(|v| v.clone_in(*v.allocator())),
            min_value: self.min_value.as_ref().map(|v| v.clone_in(*v.allocator())),
        }
    }
}

impl<A: Allocator + Copy> Clone for ColumnMeta<A> {
    fn clone(&self) -> ColumnMeta<A> {
        self.clone_in(*self.encodings.allocator())
    }
}

impl<A: Allocator + Copy> Clone for KeyValue<A> {
    fn clone(&self) -> KeyValue<A> {
        self.clone_in(*self.key.allocator())
    }
}

/// `DataPageHeader` and `DataPageHeaderV2`, as one: what a reader needs of either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataHeader {
    pub num_values: i32,
    pub encoding: i32,
    pub definition_level_encoding: i32,
    pub repetition_level_encoding: i32,
    /// v2 only: the levels' lengths, before the values.
    pub definition_levels_byte_length: i32,
    pub repetition_levels_byte_length: i32,
    /// v2 only: whether the values (not the levels) are compressed; true when unsaid.
    pub is_compressed: bool,
}

/// `PageHeader`, what a reader needs of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageHeader {
    pub ty: i32,
    pub uncompressed_page_size: i32,
    pub compressed_page_size: i32,
    pub data: Option<DataHeader>,
    /// A dictionary page's value count and encoding.
    pub dictionary: Option<(i32, i32)>,
}

// --- writing ---

pub(crate) fn write_logical<A: Allocator>(s: &mut Struct<'_, A>, logical: Logical) {
    let unit = |s: &mut Struct<'_, A>, unit: TimeUnit| {
        let id = match unit {
            TimeUnit::Millis => 1,
            TimeUnit::Micros => 2,
            TimeUnit::Nanos => 3,
        };
        s.empty(id);
    };
    match logical {
        Logical::String => s.empty(1),
        Logical::Map => s.empty(2),
        Logical::List => s.empty(3),
        Logical::Enum => s.empty(4),
        Logical::Decimal { scale, precision } => s.strukt(5, |s| {
            s.i32(1, scale);
            s.i32(2, precision);
        }),
        Logical::Date => s.empty(6),
        Logical::Time { utc, unit: u } => s.strukt(7, |s| {
            s.bool(1, utc);
            s.strukt(2, |s| unit(s, u));
        }),
        Logical::Timestamp { utc, unit: u } => s.strukt(8, |s| {
            s.bool(1, utc);
            s.strukt(2, |s| unit(s, u));
        }),
        Logical::Integer { bits, signed } => s.strukt(10, |s| {
            s.i8(1, bits);
            s.bool(2, signed);
        }),
        Logical::Unknown => s.empty(11),
        Logical::Json => s.empty(12),
        Logical::Bson => s.empty(13),
        Logical::Uuid => s.empty(14),
        Logical::Float16 => s.empty(15),
        Logical::Other(id) => s.empty(id),
    }
}

pub(crate) fn write_schema_element<A: Allocator>(s: &mut Struct<'_, A>, it: &SchemaElement<A>) {
    if let Some(v) = it.ty {
        s.i32(1, v);
    }
    if let Some(v) = it.type_length {
        s.i32(2, v);
    }
    if let Some(v) = it.repetition {
        s.i32(3, v);
    }
    s.binary(4, it.name.as_bytes());
    if let Some(v) = it.num_children {
        s.i32(5, v);
    }
    if let Some(v) = it.converted {
        s.i32(6, v);
    }
    if let Some(v) = it.scale {
        s.i32(7, v);
    }
    if let Some(v) = it.precision {
        s.i32(8, v);
    }
    if let Some(v) = it.field_id {
        s.i32(9, v);
    }
    if let Some(logical) = it.logical {
        s.strukt(10, |s| write_logical(s, logical));
    }
}

pub(crate) fn write_statistics<A: Allocator>(s: &mut Struct<'_, A>, it: &Statistics<A>) {
    if let Some(v) = &it.max {
        s.binary(1, v);
    }
    if let Some(v) = &it.min {
        s.binary(2, v);
    }
    if let Some(v) = it.null_count {
        s.i64(3, v);
    }
    if let Some(v) = it.distinct_count {
        s.i64(4, v);
    }
    if let Some(v) = &it.max_value {
        s.binary(5, v);
    }
    if let Some(v) = &it.min_value {
        s.binary(6, v);
    }
}

pub(crate) fn write_column_meta<A: Allocator>(s: &mut Struct<'_, A>, it: &ColumnMeta<A>) {
    s.i32(1, it.ty);
    s.i32s(2, it.encodings.iter().copied());
    s.binaries(3, it.path.iter().map(|it| it.as_bytes()));
    s.i32(4, it.codec);
    s.i64(5, it.num_values);
    s.i64(6, it.total_uncompressed_size);
    s.i64(7, it.total_compressed_size);
    s.i64(9, it.data_page_offset);
    if let Some(v) = it.index_page_offset {
        s.i64(10, v);
    }
    if let Some(v) = it.dictionary_page_offset {
        s.i64(11, v);
    }
    if let Some(stats) = &it.statistics {
        s.strukt(12, |s| write_statistics(s, stats));
    }
}

pub(crate) fn write_file_meta<A: Allocator>(out: &mut Vec<u8, A>, it: &FileMeta<A>) {
    let mut s = Struct::new(out);
    s.i32(1, it.version);
    s.structs(2, it.schema.as_slice(), write_schema_element);
    s.i64(3, it.num_rows);
    s.structs(4, it.row_groups.as_slice(), |s, group| {
        s.structs(1, group.columns.as_slice(), |s, chunk| {
            if let Some(path) = &chunk.file_path {
                s.binary(1, path.as_bytes());
            }
            s.i64(2, chunk.file_offset);
            if let Some(meta) = &chunk.meta {
                s.strukt(3, |s| write_column_meta(s, meta));
            }
        });
        s.i64(2, group.total_byte_size);
        s.i64(3, group.num_rows);
        if let Some(v) = group.file_offset {
            s.i64(5, v);
        }
        if let Some(v) = group.total_compressed_size {
            s.i64(6, v);
        }
        if let Some(v) = group.ordinal {
            s.i16(7, v);
        }
    });
    if !it.key_value.is_empty() {
        s.structs(5, it.key_value.as_slice(), |s, kv| {
            s.binary(1, kv.key.as_bytes());
            if let Some(v) = &kv.value {
                s.binary(2, v.as_bytes());
            }
        });
    }
    if let Some(v) = &it.created_by {
        s.binary(6, v.as_bytes());
    }
    if it.column_orders > 0 {
        // Each leaf's order is the union's one arm, `TYPE_ORDER` (an empty `TypeDefinedOrder`).
        s.repeated(7, it.column_orders, |s| s.empty(1));
    }
    s.end();
}

/// The page header `header` describes, encoded.
pub(crate) fn write_page_header<A: Allocator>(out: &mut Vec<u8, A>, header: &PageHeader, statistics: Option<&Statistics<A>>) {
    let mut s = Struct::new(out);
    s.i32(1, header.ty);
    s.i32(2, header.uncompressed_page_size);
    s.i32(3, header.compressed_page_size);
    if let Some(data) = &header.data {
        s.strukt(5, |s| {
            s.i32(1, data.num_values);
            s.i32(2, data.encoding);
            s.i32(3, data.definition_level_encoding);
            s.i32(4, data.repetition_level_encoding);
            if let Some(stats) = statistics {
                s.strukt(5, |s| write_statistics(s, stats));
            }
        });
    }
    if let Some((num_values, encoding)) = header.dictionary {
        s.strukt(7, |s| {
            s.i32(1, num_values);
            s.i32(2, encoding);
        });
    }
    s.end();
}

// --- reading ---

fn owned<A: Allocator>(text: &str, alloc: A) -> String<A> {
    String::from_str_in(text, alloc)
}

fn read_unit(input: &mut In<'_>) -> Result<TimeUnit, ParquetError> {
    let mut unit = None;
    input.strukt(|input, id, ty| {
        input.expect(ty, STRUCT, "a time unit's arm is not a struct")?;
        unit = Some(match id {
            1 => TimeUnit::Millis,
            2 => TimeUnit::Micros,
            3 => TimeUnit::Nanos,
            _ => return Ok(false),
        });
        input.skip(ty)?;
        Ok(true)
    })?;
    unit.ok_or(ParquetError::Thrift { what: "a time unit of no known arm", at: input.read() })
}

fn read_logical(input: &mut In<'_>) -> Result<Option<Logical>, ParquetError> {
    let mut out = None;
    input.strukt(|input, id, ty| {
        input.expect(ty, STRUCT, "a logical type's arm is not a struct")?;
        let logical = match id {
            5 => {
                let (mut scale, mut precision) = (0, 0);
                input.strukt(|input, field, ty| {
                    match field {
                        1 => scale = i32_field(input, ty, "a decimal's scale is not an i32")?,
                        2 => precision = i32_field(input, ty, "a decimal's precision is not an i32")?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                Logical::Decimal { scale, precision }
            }
            7 | 8 => {
                let (mut utc, mut unit) = (false, TimeUnit::Millis);
                input.strukt(|input, field, ty| {
                    match field {
                        1 => utc = input.bool_of(ty)?,
                        2 => {
                            input.expect(ty, STRUCT, "a time's unit is not a struct")?;
                            unit = read_unit(input)?;
                        }
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                if id == 7 { Logical::Time { utc, unit } } else { Logical::Timestamp { utc, unit } }
            }
            10 => {
                let (mut bits, mut signed) = (0, true);
                input.strukt(|input, field, ty| {
                    match field {
                        1 => {
                            input.expect(ty, BYTE, "an integer's width is not a byte")?;
                            bits = input.i8()?;
                        }
                        2 => signed = input.bool_of(ty)?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                Logical::Integer { bits, signed }
            }
            other => {
                // An arm of no value: an empty struct.
                input.skip(ty)?;
                match other {
                    1 => Logical::String,
                    2 => Logical::Map,
                    3 => Logical::List,
                    4 => Logical::Enum,
                    6 => Logical::Date,
                    11 => Logical::Unknown,
                    12 => Logical::Json,
                    13 => Logical::Bson,
                    14 => Logical::Uuid,
                    15 => Logical::Float16,
                    other => Logical::Other(other),
                }
            }
        };
        out = Some(logical);
        Ok(true)
    })?;
    Ok(out)
}

/// Reads an optional i32 field of a known id: refused when of another type.
fn i32_field(input: &mut In<'_>, ty: u8, what: &'static str) -> Result<i32, ParquetError> {
    input.expect(ty, I32, what)?;
    input.i32()
}

fn i64_field(input: &mut In<'_>, ty: u8, what: &'static str) -> Result<i64, ParquetError> {
    input.expect(ty, I64, what)?;
    input.i64()
}

fn read_schema_element<A: Allocator + Copy>(input: &mut In<'_>, alloc: A) -> Result<SchemaElement<A>, ParquetError> {
    let mut it = SchemaElement {
        ty: None,
        type_length: None,
        repetition: None,
        name: String::new_in(alloc),
        num_children: None,
        converted: None,
        scale: None,
        precision: None,
        field_id: None,
        logical: None,
    };
    let mut named = false;
    input.strukt(|input, id, ty| {
        match id {
            1 => it.ty = Some(i32_field(input, ty, "a schema element's type is not an i32")?),
            2 => it.type_length = Some(i32_field(input, ty, "a schema element's length is not an i32")?),
            3 => it.repetition = Some(i32_field(input, ty, "a schema element's repetition is not an i32")?),
            4 => {
                input.expect(ty, BINARY, "a schema element's name is not a string")?;
                it.name = owned(input.string()?, alloc);
                named = true;
            }
            5 => it.num_children = Some(i32_field(input, ty, "a schema element's children is not an i32")?),
            6 => it.converted = Some(i32_field(input, ty, "a schema element's converted type is not an i32")?),
            7 => it.scale = Some(i32_field(input, ty, "a schema element's scale is not an i32")?),
            8 => it.precision = Some(i32_field(input, ty, "a schema element's precision is not an i32")?),
            9 => it.field_id = Some(i32_field(input, ty, "a schema element's field id is not an i32")?),
            10 => {
                input.expect(ty, STRUCT, "a schema element's logical type is not a struct")?;
                it.logical = read_logical(input)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if !named {
        return Err(ParquetError::Thrift { what: "a schema element with no name", at: input.read() });
    }
    Ok(it)
}

fn read_bytes<A: Allocator + Copy>(input: &mut In<'_>, ty: u8, alloc: A) -> Result<Vec<u8, A>, ParquetError> {
    input.expect(ty, BINARY, "a statistic is not binary")?;
    Ok(Vec::from_slice_in(input.binary()?, alloc))
}

fn read_statistics<A: Allocator + Copy>(input: &mut In<'_>, alloc: A) -> Result<Statistics<A>, ParquetError> {
    let mut it = Statistics { max: None, min: None, null_count: None, distinct_count: None, max_value: None, min_value: None };
    input.strukt(|input, id, ty| {
        match id {
            1 => it.max = Some(read_bytes(input, ty, alloc)?),
            2 => it.min = Some(read_bytes(input, ty, alloc)?),
            3 => it.null_count = Some(i64_field(input, ty, "a null count is not an i64")?),
            4 => it.distinct_count = Some(i64_field(input, ty, "a distinct count is not an i64")?),
            5 => it.max_value = Some(read_bytes(input, ty, alloc)?),
            6 => it.min_value = Some(read_bytes(input, ty, alloc)?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(it)
}

fn read_column_meta<A: Allocator + Copy>(input: &mut In<'_>, alloc: A) -> Result<ColumnMeta<A>, ParquetError> {
    let mut it = ColumnMeta {
        ty: -1,
        encodings: Vec::new_in(alloc),
        path: Vec::new_in(alloc),
        codec: -1,
        num_values: -1,
        total_uncompressed_size: -1,
        total_compressed_size: -1,
        data_page_offset: -1,
        index_page_offset: None,
        dictionary_page_offset: None,
        statistics: None,
    };
    input.strukt(|input, id, ty| {
        match id {
            1 => it.ty = i32_field(input, ty, "a column's type is not an i32")?,
            2 => {
                input.expect(ty, LIST, "a column's encodings are not a list")?;
                input.list(I32, "a column's encodings are not i32s", |input| {
                    it.encodings.push(input.i32()?);
                    Ok(())
                })?;
            }
            3 => {
                input.expect(ty, LIST, "a column's path is not a list")?;
                input.list(BINARY, "a column's path is not strings", |input| {
                    it.path.push(owned(input.string()?, alloc));
                    Ok(())
                })?;
            }
            4 => it.codec = i32_field(input, ty, "a column's codec is not an i32")?,
            5 => it.num_values = i64_field(input, ty, "a column's value count is not an i64")?,
            6 => it.total_uncompressed_size = i64_field(input, ty, "a column's size is not an i64")?,
            7 => it.total_compressed_size = i64_field(input, ty, "a column's size is not an i64")?,
            9 => it.data_page_offset = i64_field(input, ty, "a column's data page offset is not an i64")?,
            10 => it.index_page_offset = Some(i64_field(input, ty, "a column's index page offset is not an i64")?),
            11 => {
                it.dictionary_page_offset =
                    Some(i64_field(input, ty, "a column's dictionary page offset is not an i64")?);
            }
            12 => {
                input.expect(ty, STRUCT, "a column's statistics are not a struct")?;
                it.statistics = Some(read_statistics(input, alloc)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if it.ty < 0 || it.codec < 0 || it.num_values < 0 || it.total_compressed_size < 0 || it.data_page_offset < 0 {
        return Err(ParquetError::Thrift { what: "a column chunk missing a required field", at: input.read() });
    }
    Ok(it)
}

fn read_row_group<A: Allocator + Copy>(input: &mut In<'_>, alloc: A) -> Result<RowGroup<A>, ParquetError> {
    let mut it = RowGroup {
        columns: Vec::new_in(alloc),
        total_byte_size: 0,
        num_rows: -1,
        file_offset: None,
        total_compressed_size: None,
        ordinal: None,
    };
    input.strukt(|input, id, ty| {
        match id {
            1 => {
                input.expect(ty, LIST, "a row group's columns are not a list")?;
                input.list(STRUCT, "a row group's columns are not structs", |input| {
                    let mut chunk = ColumnChunk { file_path: None, file_offset: 0, meta: None };
                    input.strukt(|input, id, ty| {
                        match id {
                            1 => {
                                input.expect(ty, BINARY, "a column chunk's path is not a string")?;
                                chunk.file_path = Some(owned(input.string()?, alloc));
                            }
                            2 => chunk.file_offset = i64_field(input, ty, "a column chunk's offset is not an i64")?,
                            3 => {
                                input.expect(ty, STRUCT, "a column chunk's metadata is not a struct")?;
                                chunk.meta = Some(read_column_meta(input, alloc)?);
                            }
                            _ => return Ok(false),
                        }
                        Ok(true)
                    })?;
                    it.columns.push(chunk);
                    Ok(())
                })?;
            }
            2 => it.total_byte_size = i64_field(input, ty, "a row group's size is not an i64")?,
            3 => it.num_rows = i64_field(input, ty, "a row group's rows are not an i64")?,
            5 => it.file_offset = Some(i64_field(input, ty, "a row group's offset is not an i64")?),
            6 => it.total_compressed_size = Some(i64_field(input, ty, "a row group's size is not an i64")?),
            7 => {
                input.expect(ty, I16, "a row group's ordinal is not an i16")?;
                it.ordinal = Some(input.i16()?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if it.num_rows < 0 {
        return Err(ParquetError::Thrift { what: "a row group with no row count", at: input.read() });
    }
    Ok(it)
}

/// The footer of `bytes` (a `FileMetaData`, the bytes between the file's last data and its
/// length word).
pub(crate) fn read_file_meta<A: Allocator + Copy>(bytes: &[u8], alloc: A) -> Result<FileMeta<A>, ParquetError> {
    let mut input = In::new(bytes);
    let mut it = FileMeta {
        version: 0,
        schema: Vec::new_in(alloc),
        num_rows: -1,
        row_groups: Vec::new_in(alloc),
        key_value: Vec::new_in(alloc),
        created_by: None,
        column_orders: 0,
    };
    input.strukt(|input, id, ty| {
        match id {
            1 => it.version = i32_field(input, ty, "the footer's version is not an i32")?,
            2 => {
                input.expect(ty, LIST, "the footer's schema is not a list")?;
                input.list(STRUCT, "the footer's schema is not structs", |input| {
                    let element = read_schema_element(input, alloc)?;
                    it.schema.push(element);
                    Ok(())
                })?;
            }
            3 => it.num_rows = i64_field(input, ty, "the footer's rows are not an i64")?,
            4 => {
                input.expect(ty, LIST, "the footer's row groups are not a list")?;
                input.list(STRUCT, "the footer's row groups are not structs", |input| {
                    let group = read_row_group(input, alloc)?;
                    it.row_groups.push(group);
                    Ok(())
                })?;
            }
            5 => {
                input.expect(ty, LIST, "the footer's key-value metadata is not a list")?;
                input.list(STRUCT, "the footer's key-value metadata is not structs", |input| {
                    let mut kv = KeyValue { key: String::new_in(alloc), value: None };
                    input.strukt(|input, id, ty| {
                        match id {
                            1 => {
                                input.expect(ty, BINARY, "a key is not a string")?;
                                kv.key = owned(input.string()?, alloc);
                            }
                            2 => {
                                input.expect(ty, BINARY, "a value is not a string")?;
                                kv.value = Some(owned(input.string()?, alloc));
                            }
                            _ => return Ok(false),
                        }
                        Ok(true)
                    })?;
                    it.key_value.push(kv);
                    Ok(())
                })?;
            }
            6 => {
                input.expect(ty, BINARY, "the footer's writer is not a string")?;
                it.created_by = Some(owned(input.string()?, alloc));
            }
            7 => {
                input.expect(ty, LIST, "the footer's column orders are not a list")?;
                input.list(STRUCT, "the footer's column orders are not structs", |input| {
                    input.skip(STRUCT)?;
                    it.column_orders += 1;
                    Ok(())
                })?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if it.num_rows < 0 || it.schema.is_empty() {
        return Err(ParquetError::Thrift { what: "a footer with no rows or no schema", at: input.read() });
    }
    Ok(it)
}

/// A page header at the start of `bytes`, and its encoded length.
pub(crate) fn read_page_header(bytes: &[u8]) -> Result<(PageHeader, usize), ParquetError> {
    let mut input = In::new(bytes);
    let mut it = PageHeader::default();
    input.strukt(|input, id, ty| {
        match id {
            1 => it.ty = i32_field(input, ty, "a page's type is not an i32")?,
            2 => it.uncompressed_page_size = i32_field(input, ty, "a page's size is not an i32")?,
            3 => it.compressed_page_size = i32_field(input, ty, "a page's size is not an i32")?,
            5 | 8 => {
                input.expect(ty, STRUCT, "a data page header is not a struct")?;
                let mut data = DataHeader { is_compressed: true, ..DataHeader::default() };
                let v2 = id == 8;
                input.strukt(|input, field, ty| {
                    match (v2, field) {
                        (_, 1) => data.num_values = i32_field(input, ty, "a page's value count is not an i32")?,
                        (false, 2) | (true, 4) => data.encoding = i32_field(input, ty, "a page's encoding is not an i32")?,
                        (false, 3) => {
                            data.definition_level_encoding = i32_field(input, ty, "a page's level encoding is not an i32")?;
                        }
                        (false, 4) => {
                            data.repetition_level_encoding = i32_field(input, ty, "a page's level encoding is not an i32")?;
                        }
                        (true, 5) => {
                            data.definition_levels_byte_length = i32_field(input, ty, "a page's level length is not an i32")?;
                        }
                        (true, 6) => {
                            data.repetition_levels_byte_length = i32_field(input, ty, "a page's level length is not an i32")?;
                        }
                        (true, 7) => data.is_compressed = input.bool_of(ty)?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                it.data = Some(data);
            }
            7 => {
                input.expect(ty, STRUCT, "a dictionary page header is not a struct")?;
                let (mut values, mut encoding) = (0, 0);
                input.strukt(|input, field, ty| {
                    match field {
                        1 => values = i32_field(input, ty, "a dictionary's value count is not an i32")?,
                        2 => encoding = i32_field(input, ty, "a dictionary's encoding is not an i32")?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                it.dictionary = Some((values, encoding));
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok((it, input.read()))
}

