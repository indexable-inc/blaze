//! The writer: batches of one set of fields in, a Parquet file's bytes out.
//!
//! # Encodings
//!
//! Each column chunk's values are encoded the way its first page with values says is smallest
//! ([`Encoding::Auto`]): the page's values are sized as PLAIN, as a dictionary (a PLAIN
//! dictionary page and RLE_DICTIONARY indices in the hybrid), as DELTA_BINARY_PACKED (integers)
//! and as DELTA_BYTE_ARRAY or DELTA_LENGTH_BYTE_ARRAY (text), and the least is the chunk's. A
//! dictionary that grows past [`Options::dictionary_bytes`] stops: the pages after it take the
//! best encoding that is not one. BOOLEAN values are RLE (the hybrid at width 1); DOUBLE values
//! are PLAIN or a dictionary. Levels are the hybrid. Every encoding is one DuckDB reads, and
//! [`crate::File`] reads every one written here.

use core::{alloc::Allocator, ops::Range};

use blaze_columns::{Batch, Column, Field, Scalar, Type, Values};
use blaze_int::Int;
use blaze_kit::Dict;
use blaze_own::{AllocFailure, String, Vec};

use crate::{
    Compression, MAGIC, ParquetError, column_name, delta,
    levels::{self, Refused},
    meta::{
        self, ColumnChunk, ColumnMeta, FileMeta, KeyValue, Logical, PageHeader, RowGroup, SchemaElement,
        Statistics, converted, encoding, page, physical, repetition,
    },
};

/// The most bytes a text column's min or max is written with: past it the chunk says neither
/// (a min or max cut short would need care no query here needs).
pub const STAT_BYTES: usize = 64;

/// How a column's values are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Each chunk's by its first page with values (see the module).
    Auto,
    /// PLAIN (BOOLEAN too).
    Plain,
    /// A dictionary page and RLE_DICTIONARY indices (BOOLEAN: RLE).
    Dictionary,
    /// DELTA_BINARY_PACKED integers and DELTA_BYTE_ARRAY text (BOOLEAN: RLE; DOUBLE: PLAIN).
    Delta,
    /// DELTA_BINARY_PACKED integers and DELTA_LENGTH_BYTE_ARRAY text (BOOLEAN: RLE; DOUBLE:
    /// PLAIN).
    DeltaLength,
}

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
    /// How values are encoded.
    pub encoding: Encoding,
    /// The most bytes a chunk's dictionary page holds (parquet-mr's default, 1 MiB).
    pub dictionary_bytes: usize,
    /// How pages are compressed: none on the hot path.
    pub compression: Compression,
}

impl Options {
    pub const DEFAULT: Options = Options {
        row_group_rows: 122_880,
        page_bytes: 1 << 20,
        page_rows: 20_000,
        created_by: "blaze-parquet",
        encoding: Encoding::Auto,
        dictionary_bytes: 1 << 20,
        compression: Compression::HOT,
    };
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

    fn kind(self) -> Kind {
        match self.scalar {
            Scalar::Bool => Kind::Bool,
            Scalar::I32 | Scalar::U16 | Scalar::U32 | Scalar::I64 | Scalar::U64 => Kind::Int,
            Scalar::F64 => Kind::Float,
            Scalar::Text | Scalar::Dict => Kind::Text,
        }
    }

    /// The bytes of one PLAIN integer or double.
    fn width(self) -> usize {
        if self.physical() == physical::INT32 { 4 } else { 8 }
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

/// What a leaf's values are held as while a page fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Bool,
    /// Integers, as `i64` (an unsigned one's bits).
    Int,
    /// Doubles, as their bits.
    Float,
    Text,
}

/// How a page's values are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    Plain,
    Rle,
    Dict,
    Delta,
    DeltaLength,
}

impl Plan {
    fn code(self, kind: Kind) -> i32 {
        match self {
            Plan::Plain => encoding::PLAIN,
            Plan::Rle => encoding::RLE,
            Plan::Dict => encoding::RLE_DICTIONARY,
            Plan::Delta if kind == Kind::Text => encoding::DELTA_BYTE_ARRAY,
            Plan::Delta => encoding::DELTA_BINARY_PACKED,
            Plan::DeltaLength => encoding::DELTA_LENGTH_BYTE_ARRAY,
        }
    }

    /// The plan `encoding` asks of a `kind` column, unless it is [`Encoding::Auto`].
    fn forced(encoding: Encoding, kind: Kind) -> Option<Plan> {
        Some(match (encoding, kind) {
            (Encoding::Auto, _) => return None,
            (Encoding::Plain, _) => Plan::Plain,
            (_, Kind::Bool) => Plan::Rle,
            (Encoding::Dictionary, _) => Plan::Dict,
            (_, Kind::Float) => Plan::Plain,
            (Encoding::Delta, _) | (Encoding::DeltaLength, Kind::Int) => Plan::Delta,
            (Encoding::DeltaLength, _) => Plan::DeltaLength,
        })
    }
}

/// The schema elements of `fields`, depth first under the root `schema`.
fn schema<A: Allocator + Copy>(fields: &[Field<A>], alloc: A) -> Vec<SchemaElement<A>, A> {
    let element = |name: &str| SchemaElement {
        ty: None,
        type_length: None,
        repetition: None,
        name: String::from_str_in(name, alloc),
        num_children: None,
        converted: None,
        scale: None,
        precision: None,
        field_id: None,
        logical: None,
    };
    let mut out = Vec::with_capacity_in(1 + 3 * fields.len(), alloc);
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

impl<A: Allocator + Copy> Stats<A> {
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

    fn text(&mut self, value: &[u8], alloc: A) {
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
                self.text = Some((Vec::from_slice_in(value, alloc), Vec::from_slice_in(value, alloc)));
            }
        }
    }

    /// The chunk's statistics in the leaf's plain encoding.
    fn finish(&mut self, leaf: Leaf, alloc: A) -> Statistics<A> {
        let bytes = |raw: &[u8]| Some(Vec::from_slice_in(raw, alloc));
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

fn refused(_: Refused) -> ParquetError {
    ParquetError::TooLarge { column: column_name("a page"), what: "a page's room the allocator refused" }
}

fn refused_alloc(_: AllocFailure) -> ParquetError {
    refused(Refused)
}

/// One column's chunk of the row group being written: its finished pages, the page being
/// filled and the chunk's dictionary.
struct Chunk<A: Allocator + Copy> {
    leaf: Leaf,
    /// The chunk's finished data pages, each a header and its body.
    pages: Vec<u8, A>,
    /// The page's values: integers and doubles' bits, booleans a byte each, texts end to end
    /// with where each ends.
    ints: Vec<i64, A>,
    bools: Vec<u8, A>,
    text: Vec<u8, A>,
    ends: Vec<u32, A>,
    defs: Vec<u8, A>,
    reps: Vec<u8, A>,
    /// The page's level entries (its `num_values`: a value, a null or an empty list each).
    page_values: usize,
    page_rows: usize,
    /// The chunk's level entries, and its pages' bytes before compression.
    chunk_values: i64,
    uncompressed: usize,
    stats: Stats<A>,
    /// How the chunk's pages are encoded, once its first page with values has said; and what
    /// takes over from a dictionary grown past its bound.
    plan: Option<Plan>,
    fallback: Plan,
    /// The chunk's dictionary (of integers' or doubles' bits, or of texts), its PLAIN bytes, and
    /// whether a page used it.
    ints_dict: Dict<u32, i64, A>,
    text_dict: Dict<u32, Vec<u8, A>, A>,
    dict_bytes: usize,
    dict_used: bool,
    /// The encodings the chunk's pages use.
    encodings: Vec<i32, A>,
    /// A page's body, and its values sized or compressed.
    body: Vec<u8, A>,
    scratch: Vec<u8, A>,
    alloc: A,
}

impl<A: Allocator + Copy> Chunk<A> {
    fn new(leaf: Leaf, alloc: A) -> Chunk<A> {
        let vec = || Vec::new_in(alloc);
        Chunk {
            leaf,
            pages: vec(),
            ints: Vec::new_in(alloc),
            bools: vec(),
            text: vec(),
            ends: Vec::new_in(alloc),
            defs: vec(),
            reps: vec(),
            page_values: 0,
            page_rows: 0,
            chunk_values: 0,
            uncompressed: 0,
            stats: Stats::new(),
            plan: None,
            fallback: Plan::Plain,
            ints_dict: Dict::new_in(alloc),
            text_dict: Dict::new_in(alloc),
            dict_bytes: 0,
            dict_used: false,
            encodings: Vec::new_in(alloc),
            body: vec(),
            scratch: vec(),
            alloc,
        }
    }

    /// Value `at` of `values` appended to the page, and to the statistics unless a list's.
    fn put(&mut self, values: &Values<A>, at: usize, alloc: A) {
        let stats = !self.leaf.list;
        match values {
            Values::Bool(it) => {
                self.bools.push(u8::from(it[at]));
                if stats {
                    self.stats.boolean(it[at]);
                }
            }
            Values::I32(it) => self.int(it[at].into(), stats),
            Values::U16(it) => self.int(it[at].into(), stats),
            Values::U32(it) => self.int(it[at].into(), stats),
            Values::I64(it) => self.int(it[at].into(), stats),
            Values::U64(it) => {
                self.ints.push(it[at].cast_signed());
                if stats {
                    self.stats.int(it[at].into());
                }
            }
            Values::F64(it) => {
                self.ints.push(it[at].to_bits().cast_signed());
                if stats {
                    self.stats.float(it[at]);
                }
            }
            Values::Text(texts) => self.text(texts.get(at), stats, alloc),
            Values::Dict { keys, values } => self.text(values.get(keys[at].widen_to::<usize>()), stats, alloc),
        }
    }

    fn int(&mut self, value: i64, stats: bool) {
        self.ints.push(value);
        if stats {
            self.stats.int(value.into());
        }
    }

    fn text(&mut self, text: &str, stats: bool, alloc: A) {
        self.text.extend_from_slice(text.as_bytes());
        // A page's text is below `u32::MAX`: a page ends at about `page_bytes`.
        self.ends.push(self.text.len().narrow_or_stop::<u32>());
        if stats {
            self.stats.text(text.as_bytes(), alloc);
        }
    }

    /// Rows `rows` of `column` appended, a page ending at each row that fills one.
    fn push(&mut self, column: &Column<A>, rows: Range<usize>, options: &Options, alloc: A) -> Result<(), ParquetError> {
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
                        for at in elements.start..elements.end {
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
        let bytes = self.ints.len() * 8 + self.text.len() + self.ends.len() * 4 + self.bools.len() / 8;
        if self.page_rows >= options.page_rows || bytes >= options.page_bytes {
            self.end_page(options)?;
        }
        Ok(())
    }

    /// The values the page holds (not its nulls or empty lists).
    fn present(&self) -> usize {
        match self.leaf.kind() {
            Kind::Bool => self.bools.len(),
            Kind::Int | Kind::Float => self.ints.len(),
            Kind::Text => self.ends.len(),
        }
    }

    /// Text `i` of the page.
    fn text_at(_text: &[u8], ends: &[u32], i: usize) -> Range<usize> {
        let start = if i == 0 { 0 } else { ends[i - 1].widen_to::<usize>() };
        start..ends[i].widen_to::<usize>()
    }

    /// The page's values pushed into the chunk's dictionary: the codes from `before` on are
    /// theirs.
    fn push_dict(&mut self) -> Result<usize, ParquetError> {
        let width = self.leaf.width();
        let alloc = self.alloc;
        match self.leaf.kind() {
            Kind::Text => {
                let before = self.text_dict.len();
                for i in 0..self.ends.len() {
                    let range = Chunk::<A>::text_at(&self.text, &self.ends, i);
                    let bytes = &self.text[range];
                    let known = self.text_dict.distinct();
                    self.text_dict
                        .try_push_by(bytes, |it| {
                            let mut own = Vec::try_with_capacity_in(it.len(), alloc)?;
                            own.try_extend_from_slice(it)?;
                            Ok(own)
                        })
                        .map_err(refused_alloc)?;
                    if self.text_dict.distinct() > known {
                        self.dict_bytes += 4 + bytes.len();
                    }
                }
                Ok(before)
            }
            _ => {
                let before = self.ints_dict.len();
                for &value in self.ints.iter() {
                    let known = self.ints_dict.distinct();
                    self.ints_dict.try_push(value).map_err(|it| refused_alloc(it.why))?;
                    if self.ints_dict.distinct() > known {
                        self.dict_bytes += width;
                    }
                }
                Ok(before)
            }
        }
    }

    fn dict_codes(&self) -> &[u32] {
        if self.leaf.kind() == Kind::Text { self.text_dict.codes() } else { self.ints_dict.codes() }
    }

    fn dict_distinct(&self) -> usize {
        if self.leaf.kind() == Kind::Text { self.text_dict.distinct() } else { self.ints_dict.distinct() }
    }

    fn clear_dict(&mut self) {
        self.ints_dict.clear();
        self.text_dict.clear();
        self.dict_bytes = 0;
    }

    /// The chunk's plan, from its first page with `n` values: the smallest of the encodings the
    /// page's values could take (the dictionary's only within its bound).
    fn decide(&mut self, n: usize, options: &Options) -> Result<Plan, ParquetError> {
        let kind = self.leaf.kind();
        if let Some(plan) = Plan::forced(options.encoding, kind) {
            self.fallback = if plan == Plan::Dict { Plan::Plain } else { plan };
            return Ok(plan);
        }
        if kind == Kind::Bool {
            self.fallback = Plan::Rle;
            return Ok(Plan::Rle);
        }
        let mut sizes: [(Plan, usize); 4] = [(Plan::Plain, usize::MAX); 4];
        sizes[0].1 = match kind {
            Kind::Text => self.text.len() + 4 * n,
            _ => self.leaf.width() * n,
        };
        // The dictionary: its page and the indices at their width.
        let codes = self.push_dict()?;
        let distinct = self.dict_distinct();
        let dict = self.dict_bytes + 1 + (n * usize::from(levels::bit_width((distinct.max(1) - 1).narrow_or_stop()))).div_ceil(8);
        if self.dict_bytes <= options.dictionary_bytes {
            sizes[1] = (Plan::Dict, dict);
        }
        let _ = codes;
        self.clear_dict();
        match kind {
            Kind::Int => {
                self.scratch.clear();
                let narrow = self.leaf.width() == 4;
                let ints = &self.ints;
                delta::encode(&mut self.scratch, n, narrow, |i| ints[i]).map_err(refused)?;
                sizes[2] = (Plan::Delta, self.scratch.len());
            }
            Kind::Text => {
                let (text, ends) = (&self.text, &self.ends);
                let at = |i: usize| &text[Chunk::<A>::text_at(text, ends, i)];
                sizes[2] = (Plan::Delta, delta::prefixed_size(n, at));
                sizes[3] = (Plan::DeltaLength, text.len() + n);
            }
            Kind::Float | Kind::Bool => {}
        }
        // The least, PLAIN on a tie; what is not a dictionary takes over from one.
        let best = |dict: bool| {
            sizes.iter().filter(|it| dict || it.0 != Plan::Dict).min_by_key(|it| it.1).map_or(Plan::Plain, |it| it.0)
        };
        self.fallback = best(false);
        Ok(best(true))
    }

    /// The page's values in `plan`, appended to `body`.
    fn values(&mut self, plan: Plan, n: usize) -> Result<(), ParquetError> {
        let mut body = core::mem::replace(&mut self.body, Vec::new_in(self.alloc));
        let written = self.values_into(plan, n, &mut body);
        self.body = body;
        written
    }

    fn values_into(&mut self, plan: Plan, n: usize, body: &mut Vec<u8, A>) -> Result<(), ParquetError> {
        let kind = self.leaf.kind();
        let width = self.leaf.width();
        match (plan, kind) {
            (Plan::Plain, Kind::Bool) => {
                for group in self.bools.chunks(8) {
                    let byte = group.iter().enumerate().fold(0u8, |byte, (bit, value)| byte | (value << bit));
                    body.push(byte);
                }
            }
            (Plan::Plain, Kind::Text) => {
                for i in 0..n {
                    let range = Chunk::<A>::text_at(&self.text, &self.ends, i);
                    body.extend_from_slice(&range.len().narrow_or_stop::<u32>().to_le_bytes());
                    body.extend_from_slice(&self.text[range]);
                }
            }
            (Plan::Plain, _) => {
                for value in self.ints.iter() {
                    body.extend_from_slice(&value.to_le_bytes()[..width]);
                }
            }
            (Plan::Rle, _) => {
                let at = body.len();
                body.extend_from_slice(&[0; 4]);
                let bools = &self.bools;
                levels::encode(body, n, 1, |i| u32::from(bools[i])).map_err(refused)?;
                let len = (body.len() - at - 4).narrow_or_stop::<u32>();
                body[at..at + 4].copy_from_slice(&len.to_le_bytes());
            }
            (Plan::Dict, _) => {
                let before = self.push_dict()?;
                self.dict_used = true;
                let width = levels::bit_width((self.dict_distinct().max(1) - 1).narrow_or_stop());
                body.push(width);
                let codes = &self.dict_codes()[before..];
                levels::encode(body, n, width, |i| codes[i]).map_err(refused)?;
            }
            (Plan::Delta | Plan::DeltaLength, Kind::Int) => {
                let ints = &self.ints;
                delta::encode(body, n, width == 4, |i| ints[i]).map_err(refused)?;
            }
            (Plan::Delta, Kind::Text) => {
                let (text, ends) = (&self.text, &self.ends);
                delta::encode_prefixed(body, n, |i| &text[Chunk::<A>::text_at(text, ends, i)]).map_err(refused)?;
            }
            (Plan::DeltaLength, Kind::Text) => {
                let (text, ends) = (&self.text, &self.ends);
                delta::encode_lengths(body, n, |i| &text[Chunk::<A>::text_at(text, ends, i)]).map_err(refused)?;
            }
            (Plan::Delta | Plan::DeltaLength, Kind::Bool | Kind::Float) => {
                unreachable!("no delta plan is made for a boolean or a double")
            }
        }
        Ok(())
    }

    /// A data page, its `header` filled in with its body's sizes, appended to the chunk's pages,
    /// the body compressed as `options` says.
    fn page(&mut self, mut header: PageHeader, options: &Options) -> Result<(), ParquetError> {
        let size = self.body.len().narrow::<i32>().map_err(|_| ParquetError::TooLarge {
            column: column_name("a page"),
            what: "a page past 2 GiB",
        })?;
        self.scratch.clear();
        let compressed = options.compression.compress(&self.body, &mut self.scratch)?;
        let stored = if compressed { &self.scratch } else { &self.body };
        header.uncompressed_page_size = size;
        header.compressed_page_size = stored.len().narrow_or_stop();
        let at = self.pages.len();
        meta::write_page_header(&mut self.pages, &header, None);
        self.uncompressed += self.pages.len() - at + self.body.len();
        self.pages.extend_from_slice(stored);
        Ok(())
    }

    /// The page being filled, ended: its header and body appended to the chunk's pages.
    fn end_page(&mut self, options: &Options) -> Result<(), ParquetError> {
        if self.page_values == 0 {
            return Ok(());
        }
        let n = self.present();
        let plan = match self.plan {
            Some(plan) => plan,
            // A page of only nulls says nothing of the values: PLAIN (none), still undecided.
            None if n == 0 => Plan::Plain,
            None => {
                let plan = self.decide(n, options)?;
                self.plan = Some(plan);
                plan
            }
        };
        self.body.clear();
        if self.leaf.max_rep > 0 {
            prefixed(&mut self.body, &self.reps, levels::bit_width(self.leaf.max_rep.into()))?;
        }
        if self.leaf.max_def > 0 {
            prefixed(&mut self.body, &self.defs, levels::bit_width(self.leaf.max_def.into()))?;
        }
        self.values(plan, n)?;
        if plan == Plan::Dict && self.dict_bytes > options.dictionary_bytes {
            // Grown past its bound: this page keeps it, the rest take the fallback.
            self.plan = Some(self.fallback);
        }
        let code = plan.code(self.leaf.kind());
        if !self.encodings.contains(&code) {
            self.encodings.push(code);
        }
        let header = PageHeader {
            ty: page::DATA_PAGE,
            uncompressed_page_size: 0,
            compressed_page_size: 0,
            data: Some(meta::DataHeader {
                num_values: self.page_values.narrow_or_stop(),
                encoding: code,
                definition_level_encoding: encoding::RLE,
                repetition_level_encoding: encoding::RLE,
                definition_levels_byte_length: 0,
                repetition_levels_byte_length: 0,
                is_compressed: false,
            }),
            dictionary: None,
        };
        self.page(header, options)?;
        self.chunk_values += self.page_values.widen_to::<u64>().cast_signed();
        self.ints.clear();
        self.bools.clear();
        self.text.clear();
        self.ends.clear();
        self.defs.clear();
        self.reps.clear();
        self.page_values = 0;
        self.page_rows = 0;
        Ok(())
    }

    /// The chunk's dictionary page (PLAIN values), when a page used the dictionary, appended to
    /// `out`; and its bytes before compression.
    fn dictionary_page(&mut self, options: &Options, out: &mut Vec<u8, A>) -> Result<usize, ParquetError> {
        if !self.dict_used {
            return Ok(0);
        }
        self.body.clear();
        let count = self.dict_distinct();
        if self.leaf.kind() == Kind::Text {
            for value in self.text_dict.values() {
                self.body.extend_from_slice(&value.len().narrow_or_stop::<u32>().to_le_bytes());
                self.body.extend_from_slice(value);
            }
        } else {
            let width = self.leaf.width();
            for value in self.ints_dict.values() {
                self.body.extend_from_slice(&value.to_le_bytes()[..width]);
            }
        }
        let size = self.body.len().narrow::<i32>().map_err(|_| ParquetError::TooLarge {
            column: column_name("a dictionary page"),
            what: "a dictionary page past 2 GiB",
        })?;
        self.scratch.clear();
        let compressed = options.compression.compress(&self.body, &mut self.scratch)?;
        let stored = if compressed { &self.scratch } else { &self.body };
        let header = PageHeader {
            ty: page::DICTIONARY_PAGE,
            uncompressed_page_size: size,
            compressed_page_size: stored.len().narrow_or_stop(),
            data: None,
            dictionary: Some((count.narrow_or_stop(), encoding::PLAIN)),
        };
        let at = out.len();
        meta::write_page_header(out, &header, None);
        let head = out.len() - at;
        out.extend_from_slice(stored);
        Ok(head + self.body.len())
    }

    /// Ready for the next row group's chunk.
    fn reset(&mut self) {
        self.chunk_values = 0;
        self.uncompressed = 0;
        self.plan = None;
        self.fallback = Plan::Plain;
        self.clear_dict();
        self.dict_used = false;
        self.encodings.clear();
    }
}

/// `levels` in the hybrid after their length as four little-endian bytes, as data page v1
/// has them.
fn prefixed<A: Allocator>(out: &mut Vec<u8, A>, values: &[u8], width: u8) -> Result<(), ParquetError> {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    levels::encode(out, values.len(), width, |i| u32::from(values[i])).map_err(refused)?;
    let len = (out.len() - at - 4).narrow::<u32>().map_err(|_| ParquetError::TooLarge {
        column: column_name("a page's levels"),
        what: "levels past 4 GiB",
    })?;
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// A Parquet file being written: batches in, row groups cut at [`Options::row_group_rows`].
pub struct Writer<A: Allocator + Copy> {
    alloc: A,
    options: Options,
    fields: Vec<Field<A>, A>,
    chunks: Vec<Chunk<A>, A>,
    out: Vec<u8, A>,
    groups: Vec<RowGroup<A>, A>,
    group_rows: usize,
    rows: i64,
}

impl<A: Allocator + Copy> Writer<A> {
    /// A file of `fields`.
    pub fn new(fields: &[Field<A>], options: Options, alloc: A) -> Writer<A> {
        let mut chunks = Vec::with_capacity_in(fields.len(), alloc);
        for field in fields {
            chunks.push(Chunk::new(Leaf::of(field.ty, field.nullable), alloc));
        }
        let mut out = Vec::new_in(alloc);
        out.extend_from_slice(&MAGIC);
        Writer {
            fields: Vec::from_iter_in(fields.iter().cloned(), alloc),
            chunks,
            out,
            groups: Vec::new_in(alloc),
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
                chunk.push(column, start..start + take, &self.options, self.alloc)?;
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
        let mut columns = Vec::with_capacity_in(self.chunks.len(), self.alloc);
        let group_start = self.out.len();
        let mut group_uncompressed = 0i64;
        for (chunk, field) in self.chunks.iter_mut().zip(self.fields.iter()) {
            chunk.end_page(&self.options)?;
            let offset = self.out.len();
            let dictionary = chunk.dictionary_page(&self.options, &mut self.out)?;
            let data_offset = self.out.len();
            self.out.extend_from_slice(&chunk.pages);
            chunk.pages.clear();
            let size = (self.out.len() - offset).widen_to::<u64>().cast_signed();
            let uncompressed = (dictionary + chunk.uncompressed).widen_to::<u64>().cast_signed();
            group_uncompressed += uncompressed;
            let leaf = chunk.leaf;
            let statistics = chunk.stats.finish(leaf, self.alloc);
            let mut path = Vec::new_in(self.alloc);
            path.push(String::from_str_in(field.name(), self.alloc));
            if leaf.list {
                path.push(String::from_str_in("list", self.alloc));
                path.push(String::from_str_in("element", self.alloc));
            }
            let mut encodings = Vec::new_in(self.alloc);
            encodings.push(encoding::RLE);
            if chunk.dict_used {
                encodings.push(encoding::PLAIN);
            }
            for &code in chunk.encodings.iter() {
                if !encodings.contains(&code) {
                    encodings.push(code);
                }
            }
            columns.push(ColumnChunk {
                file_path: None,
                file_offset: offset.widen_to::<u64>().cast_signed(),
                meta: Some(ColumnMeta {
                    ty: leaf.physical(),
                    encodings,
                    path,
                    codec: self.options.compression.codec(),
                    num_values: chunk.chunk_values,
                    total_uncompressed_size: uncompressed,
                    total_compressed_size: size,
                    data_page_offset: data_offset.widen_to::<u64>().cast_signed(),
                    index_page_offset: None,
                    dictionary_page_offset: chunk.dict_used.then(|| offset.widen_to::<u64>().cast_signed()),
                    statistics: (!leaf.list).then_some(statistics),
                }),
            });
            chunk.reset();
        }
        let size = (self.out.len() - group_start).widen_to::<u64>().cast_signed();
        let ordinal = i16::try_from(self.groups.len()).ok();
        let rows = self.group_rows.widen_to::<u64>().cast_signed();
        self.groups.push(RowGroup {
            columns,
            total_byte_size: group_uncompressed,
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
        let alloc = self.alloc;
        let mut key_value = Vec::with_capacity_in(metadata.len(), alloc);
        for (key, value) in metadata {
            key_value.push(KeyValue {
                key: String::from_str_in(key, alloc),
                value: Some(String::from_str_in(value, alloc)),
            });
        }
        let footer = FileMeta {
            version: 1,
            schema: schema(&self.fields, alloc),
            num_rows: self.rows,
            row_groups: core::mem::replace(&mut self.groups, Vec::new_in(alloc)),
            key_value,
            created_by: Some(String::from_str_in(self.options.created_by, alloc)),
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
pub fn write<A: Allocator + Copy>(
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
