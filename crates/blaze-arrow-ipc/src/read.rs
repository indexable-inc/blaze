//! Arrow IPC messages as batches: a stream across calls ([`StreamReader`]), one whole stream
//! ([`read_stream`]), a file ([`read_file`]).

use core::alloc::Allocator;

use blaze_columns::{Batch, Column, Field, Name, Scalar, Texts, Type, Values};
use blaze_int::Int;
use blaze_own::{String, Vec};

use crate::{
    END, IpcError, MAGIC,
    fb::{Tab, i32_of, i64_of},
    write::{header, ty},
};

/// How a leaf's values lie in its buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phys {
    Bool,
    Int { bytes: usize, signed: bool },
    F32,
    F64,
    Utf8,
    /// Keys of dictionary `id`, each an integer of `bytes`.
    Dict { id: i64, bytes: usize, signed: bool },
}

#[derive(Clone, Copy, Debug)]
struct Leaf {
    phys: Phys,
    list: bool,
}

/// A stream's schema.
struct Schema<A: Allocator> {
    fields: Vec<Field<A>, A>,
    leaves: Vec<Leaf, A>,
    metadata: Vec<(String<A>, String<A>), A>,
}

fn int_of(table: Tab<'_>, field: usize) -> Result<(usize, bool), IpcError> {
    let bits = table.i32(0, 0)?;
    let signed = table.bool(1)?;
    match bits {
        8 | 16 | 32 | 64 => Ok((usize::try_from(bits / 8).unwrap_or(0), signed)),
        _ => Err(IpcError::Unsupported { field: Some(field), what: "an integer of a width", code: i64::from(bits) }),
    }
}

/// The scalar and buffers of a field of type `tag`, dictionary-encoded by `dictionary`.
fn leaf_of(field: usize, tag: u8, table: Option<Tab<'_>>, dictionary: Option<Tab<'_>>) -> Result<(Scalar, Phys), IpcError> {
    let unsupported = |what, code| IpcError::Unsupported { field: Some(field), what, code };
    let need = || table.ok_or(IpcError::Corrupt { field: Some(field), what: "a type with no table" });
    if let Some(dictionary) = dictionary {
        if tag != ty::UTF8 {
            return Err(unsupported("a dictionary of values other than Utf8", i64::from(tag)));
        }
        let (bytes, signed) = match dictionary.table(1)? {
            Some(index) => int_of(index, field)?,
            None => (4, true),
        };
        return Ok((Scalar::Dict, Phys::Dict { id: dictionary.i64(0, 0)?, bytes, signed }));
    }
    Ok(match tag {
        ty::BOOL => (Scalar::Bool, Phys::Bool),
        ty::UTF8 => (Scalar::Text, Phys::Utf8),
        ty::INT => {
            let (bytes, signed) = int_of(need()?, field)?;
            let scalar = match (bytes, signed) {
                (1 | 2 | 4, true) => Scalar::I32,
                (8, true) => Scalar::I64,
                (1 | 2, false) => Scalar::U16,
                (4, false) => Scalar::U32,
                _ => Scalar::U64,
            };
            (scalar, Phys::Int { bytes, signed })
        }
        ty::FLOAT => match need()?.i16(0, 0)? {
            1 => (Scalar::F64, Phys::F32),
            2 => (Scalar::F64, Phys::F64),
            other => return Err(unsupported("a float of a precision", i64::from(other))),
        },
        other => return Err(unsupported("a type", i64::from(other))),
    })
}

fn read_schema<A: Allocator + Clone>(schema: Tab<'_>, alloc: &A) -> Result<Schema<A>, IpcError> {
    if schema.i16(0, 0)? != 0 {
        return Err(IpcError::Unsupported { field: None, what: "a big-endian schema", code: 1 });
    }
    let tables = schema.tables(1)?;
    let mut fields = Vec::with_capacity_in(tables.len(), alloc.clone());
    let mut leaves = Vec::with_capacity_in(tables.len(), alloc.clone());
    for at in 0..tables.len() {
        let field = tables.nth(at)?;
        let name = Name::Owned(String::from_str_in(field.string(0)?.unwrap_or(""), alloc.clone()));
        let nullable = field.bool(1)?;
        let tag = field.u8(2, 0)?;
        let (ty, leaf) = if tag == ty::LIST {
            let children = field.tables(5)?;
            if children.len() != 1 {
                return Err(IpcError::Corrupt { field: Some(at), what: "a list of other than one child" });
            }
            let item = children.nth(0)?;
            let (scalar, phys) = leaf_of(at, item.u8(2, 0)?, item.table(3)?, item.table(4)?)?;
            if item.u8(2, 0)? == ty::LIST {
                return Err(IpcError::Unsupported { field: Some(at), what: "a list of lists", code: i64::from(ty::LIST) });
            }
            (Type::List(scalar), Leaf { phys, list: true })
        } else {
            let (scalar, phys) = leaf_of(at, tag, field.table(3)?, field.table(4)?)?;
            (Type::One(scalar), Leaf { phys, list: false })
        };
        fields.push(Field { name, ty, nullable });
        leaves.push(leaf);
    }
    let pairs = schema.tables(2)?;
    let mut metadata = Vec::with_capacity_in(pairs.len(), alloc.clone());
    for at in 0..pairs.len() {
        let pair = pairs.nth(at)?;
        let key = String::from_str_in(pair.string(0)?.unwrap_or(""), alloc.clone());
        let value = String::from_str_in(pair.string(1)?.unwrap_or(""), alloc.clone());
        metadata.push((key, value));
    }
    Ok(Schema { fields, leaves, metadata })
}

/// A record batch's field nodes and buffers, taken in order.
struct Parts<'a> {
    rows: usize,
    nodes: &'a [u8],
    buffers: &'a [u8],
    body: &'a [u8],
    node: usize,
    buffer: usize,
}

impl<'a> Parts<'a> {
    fn of(record: Tab<'a>, body: &'a [u8]) -> Result<Parts<'a>, IpcError> {
        if record.table(3)?.is_some() {
            return Err(IpcError::Unsupported { field: None, what: "a compressed record batch", code: 0 });
        }
        let rows = usize::try_from(record.i64(0, 0)?).map_err(|_| IpcError::Corrupt { field: None, what: "a negative row count" })?;
        Ok(Parts { rows, nodes: record.structs(1, 16)?, buffers: record.structs(2, 16)?, body, node: 0, buffer: 0 })
    }

    /// The next field node: its rows and nulls.
    fn node(&mut self, field: usize) -> Result<(usize, usize), IpcError> {
        let at = 16 * self.node;
        if at >= self.nodes.len() {
            return Err(IpcError::Corrupt { field: Some(field), what: "fewer field nodes than fields" });
        }
        self.node += 1;
        let corrupt = IpcError::Corrupt { field: Some(field), what: "a negative length or null count" };
        let rows = usize::try_from(i64_of(self.nodes, at)?).map_err(|_| corrupt)?;
        let nulls = usize::try_from(i64_of(self.nodes, at + 8)?).map_err(|_| corrupt)?;
        Ok((rows, nulls))
    }

    /// The next buffer's bytes.
    fn buffer(&mut self, field: usize) -> Result<&'a [u8], IpcError> {
        let at = 16 * self.buffer;
        if at >= self.buffers.len() {
            return Err(IpcError::Corrupt { field: Some(field), what: "fewer buffers than its fields have" });
        }
        self.buffer += 1;
        let corrupt = IpcError::Corrupt { field: Some(field), what: "a buffer past its message's body" };
        let offset = usize::try_from(i64_of(self.buffers, at)?).map_err(|_| corrupt)?;
        let len = usize::try_from(i64_of(self.buffers, at + 8)?).map_err(|_| corrupt)?;
        let end = offset.checked_add(len).ok_or(corrupt)?;
        self.body.get(offset..end).ok_or(corrupt)
    }

    /// `rows` bits of a bitmap buffer.
    fn bits<A: Allocator + Clone>(&mut self, rows: usize, field: usize, alloc: &A) -> Result<Vec<bool, A>, IpcError> {
        let bytes = self.buffer(field)?;
        if bytes.len() < rows.div_ceil(8) {
            return Err(IpcError::Corrupt { field: Some(field), what: "a bitmap shorter than its rows" });
        }
        let mut out = Vec::with_capacity_in(rows, alloc.clone());
        for row in 0..rows {
            out.push(bytes[row / 8] >> (row % 8) & 1 == 1);
        }
        Ok(out)
    }

    /// A validity buffer: `None` when no row is null (its bytes may be absent then).
    fn validity<A: Allocator + Clone>(&mut self, rows: usize, nulls: usize, field: usize, alloc: &A) -> Result<Option<Vec<bool, A>>, IpcError> {
        if nulls == 0 {
            self.buffer(field)?;
            return Ok(None);
        }
        self.bits(rows, field, alloc).map(Some)
    }

    /// The next buffer as `count` values of `N` bytes.
    fn fixed<const N: usize>(&mut self, count: usize, field: usize) -> Result<impl Iterator<Item = [u8; N]> + 'a, IpcError> {
        let bytes = self.buffer(field)?;
        if count.checked_mul(N).is_none_or(|it| bytes.len() < it) {
            return Err(IpcError::Corrupt { field: Some(field), what: "a buffer shorter than its values" });
        }
        Ok(bytes[..count * N].chunks_exact(N).map(|it| {
            let mut out = [0; N];
            out.copy_from_slice(it);
            out
        }))
    }

    /// `count + 1` offsets, rebased onto their first: the offsets and where they began.
    fn offsets<A: Allocator + Clone>(&mut self, count: usize, field: usize, alloc: &A) -> Result<(Vec<u32, A>, usize), IpcError> {
        let corrupt = IpcError::Corrupt { field: Some(field), what: "offsets that are negative or go down" };
        let mut out = Vec::with_capacity_in(count + 1, alloc.clone());
        let mut first = None;
        for raw in self.fixed::<4>(count + 1, field)? {
            let offset = usize::try_from(i32::from_le_bytes(raw)).map_err(|_| corrupt)?;
            let base = *first.get_or_insert(offset);
            let rebased = offset.checked_sub(base).and_then(|it| u32::try_from(it).ok()).ok_or(corrupt)?;
            if out.last().is_some_and(|last| *last > rebased) {
                return Err(corrupt);
            }
            out.push(rebased);
        }
        Ok((out, first.unwrap_or(0)))
    }

    /// `count` texts: their offsets and data buffers.
    fn texts<A: Allocator + Clone>(&mut self, count: usize, field: usize, alloc: &A) -> Result<Texts<A>, IpcError> {
        let (offsets, first) = self.offsets(count, field, alloc)?;
        let data = self.buffer(field)?;
        let end = first + offsets.last().map_or(0, |it| it.widen_to::<usize>());
        let text = data.get(first..end).ok_or(IpcError::Corrupt { field: Some(field), what: "offsets past their text" })?;
        let text = core::str::from_utf8(text).map_err(|_| IpcError::Corrupt { field: Some(field), what: "text that is not UTF-8" })?;
        Texts::from_parts(offsets, String::from_str_in(text, alloc.clone())).map_err(|error| IpcError::Column { field, error })
    }

    /// `count` values of `phys`, as `scalar`'s.
    fn values<A: Allocator + Clone>(
        &mut self,
        phys: Phys,
        scalar: Scalar,
        count: usize,
        field: usize,
        dicts: &[(i64, Texts<A>)],
        alloc: &A,
    ) -> Result<Values<A>, IpcError> {
        let int = |raw: &[u8], signed: bool| -> i128 {
            let mut bytes = [0u8; 16];
            bytes[..raw.len()].copy_from_slice(raw);
            if signed && raw.last().is_some_and(|it| it & 0x80 != 0) {
                bytes[raw.len()..].fill(0xFF);
            }
            i128::from_le_bytes(bytes)
        };
        let corrupt = IpcError::Corrupt { field: Some(field), what: "an integer past its column's type" };
        macro_rules! ints {
            ($n:literal, $signed:expr, $t:ty, $v:ident) => {{
                let mut out = Vec::with_capacity_in(count, alloc.clone());
                for raw in self.fixed::<$n>(count, field)? {
                    out.push(<$t>::try_from(int(&raw, $signed)).map_err(|_| corrupt)?);
                }
                Values::$v(out)
            }};
        }
        macro_rules! by_width {
            ($bytes:expr, $signed:expr, $t:ty, $v:ident) => {
                match $bytes {
                    1 => ints!(1, $signed, $t, $v),
                    2 => ints!(2, $signed, $t, $v),
                    4 => ints!(4, $signed, $t, $v),
                    _ => ints!(8, $signed, $t, $v),
                }
            };
        }
        Ok(match phys {
            Phys::Bool => Values::Bool(self.bits(count, field, alloc)?),
            Phys::Int { bytes, signed } => match scalar {
                Scalar::I32 => by_width!(bytes, signed, i32, I32),
                Scalar::I64 => by_width!(bytes, signed, i64, I64),
                Scalar::U16 => by_width!(bytes, signed, u16, U16),
                Scalar::U32 => by_width!(bytes, signed, u32, U32),
                _ => by_width!(bytes, signed, u64, U64),
            },
            Phys::F32 => Values::F64(Vec::from_iter_in(self.fixed::<4>(count, field)?.map(|it| f64::from(f32::from_le_bytes(it))), alloc.clone())),
            Phys::F64 => Values::F64(Vec::from_iter_in(self.fixed::<8>(count, field)?.map(f64::from_le_bytes), alloc.clone())),
            Phys::Utf8 => Values::Text(self.texts(count, field, alloc)?),
            Phys::Dict { id, bytes, signed } => {
                let Values::U32(keys) = by_width!(bytes, signed, u32, U32) else { unreachable!("u32 keys are U32") };
                let values = dicts.iter().find(|it| it.0 == id).ok_or(IpcError::NoDictionary { id })?;
                // clone: a dictionary's values, once a batch that names it
                Values::Dict { keys, values: values.1.clone() }
            }
        })
    }
}

fn record<A: Allocator + Clone>(
    schema: &Schema<A>,
    dicts: &[(i64, Texts<A>)],
    record: Tab<'_>,
    body: &[u8],
    alloc: &A,
) -> Result<Batch<A>, IpcError> {
    let mut parts = Parts::of(record, body)?;
    let mut columns = Vec::with_capacity_in(schema.fields.len(), alloc.clone());
    for (at, (field, leaf)) in schema.fields.iter().zip(schema.leaves.iter()).enumerate() {
        let (rows, nulls) = parts.node(at)?;
        if rows != parts.rows {
            return Err(IpcError::Corrupt { field: Some(at), what: "a column of other rows than its batch" });
        }
        let valid = parts.validity(rows, nulls, at, alloc)?;
        let scalar = field.ty.scalar();
        let column = if leaf.list {
            let (offsets, first) = parts.offsets(rows, at, alloc)?;
            if first != 0 {
                return Err(IpcError::Unsupported { field: Some(at), what: "a list whose offsets do not start at 0", code: 0 });
            }
            let (elements, element_nulls) = parts.node(at)?;
            if element_nulls != 0 {
                return Err(IpcError::Unsupported { field: Some(at), what: "a list with null elements", code: 0 });
            }
            parts.validity(elements, 0, at, alloc)?;
            let values = parts.values(leaf.phys, scalar, elements, at, dicts, alloc)?;
            Column::list(offsets, valid, values)
        } else {
            let values = parts.values(leaf.phys, scalar, rows, at, dicts, alloc)?;
            Column::try_new(values, valid)
        };
        columns.push(column.map_err(|error| IpcError::Column { field: at, error })?);
    }
    let fields = Vec::from_iter_in(schema.fields.iter().cloned(), alloc.clone());
    Batch::try_new(fields, columns).map_err(|error| IpcError::Column { field: 0, error })
}

/// A dictionary batch into `dicts`: its values replace or (a delta) extend dictionary `id`'s.
fn dictionary<A: Allocator + Clone>(
    schema: &Schema<A>,
    dicts: &mut Vec<(i64, Texts<A>), A>,
    batch: Tab<'_>,
    body: &[u8],
    alloc: &A,
) -> Result<(), IpcError> {
    let id = batch.i64(0, 0)?;
    let delta = batch.bool(2)?;
    let field = schema.leaves.iter().position(|it| matches!(it.phys, Phys::Dict { id: of, .. } if of == id));
    let field = field.ok_or(IpcError::NoDictionary { id })?;
    let data = batch.table(1)?.ok_or(IpcError::Corrupt { field: Some(field), what: "a dictionary batch with no data" })?;
    let mut parts = Parts::of(data, body)?;
    let (rows, nulls) = parts.node(field)?;
    if nulls != 0 {
        return Err(IpcError::Unsupported { field: Some(field), what: "a dictionary with null values", code: 0 });
    }
    parts.validity(rows, 0, field, alloc)?;
    let values = parts.texts(rows, field, alloc)?;
    match dicts.iter_mut().find(|it| it.0 == id) {
        Some((_, old)) if delta => {
            for value in values.iter() {
                old.try_push(value).map_err(|error| IpcError::Column { field, error })?;
            }
        }
        Some((_, old)) => *old = values,
        None if delta => return Err(IpcError::NoDictionary { id }),
        None => dicts.push((id, values)),
    }
    Ok(())
}

/// One message: what it is, its flatbuffer's header and its body, and where the next begins.
enum Raw<'a> {
    End,
    Message { tag: u8, header: Option<Tab<'a>>, body: &'a [u8] },
}

fn raw(bytes: &[u8], at: usize) -> Result<(Raw<'_>, usize), IpcError> {
    let word = |at: usize| -> Result<[u8; 4], IpcError> {
        let got = bytes.get(at..at + 4).ok_or(IpcError::Truncated { what: "a message's length", at })?;
        Ok([got[0], got[1], got[2], got[3]])
    };
    let (len, meta) = match word(at)? {
        [0xFF, 0xFF, 0xFF, 0xFF] => (i32::from_le_bytes(word(at + 4)?), at + 8),
        // A stream before the continuation (Arrow before 0.15): the length alone.
        other => (i32::from_le_bytes(other), at + 4),
    };
    let len = usize::try_from(len).map_err(|_| IpcError::Corrupt { field: None, what: "a negative message length" })?;
    if len == 0 {
        return Ok((Raw::End, meta));
    }
    let flat = bytes.get(meta..meta + len).ok_or(IpcError::Truncated { what: "a message's metadata", at: meta })?;
    let message = Tab::root(flat)?;
    let version = message.i16(0, 0)?;
    if version < 3 {
        return Err(IpcError::Unsupported { field: None, what: "a message of a metadata version before V4", code: i64::from(version) });
    }
    let tag = message.u8(1, 0)?;
    let header = message.table(2)?;
    let body_len = usize::try_from(message.i64(3, 0)?).map_err(|_| IpcError::Corrupt { field: None, what: "a negative body length" })?;
    let start = meta + len;
    let end = start.checked_add(body_len).ok_or(IpcError::Truncated { what: "a message's body", at: start })?;
    let body = bytes.get(start..end).ok_or(IpcError::Truncated { what: "a message's body", at: start })?;
    Ok((Raw::Message { tag, header, body }, end))
}

/// What a message is, as [`messages`] lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Schema,
    Dictionary { id: i64, delta: bool, rows: usize },
    Batch { rows: usize },
    End,
}

/// The messages `bytes` hold, in order.
pub fn messages<A: Allocator + Clone>(bytes: &[u8], alloc: A) -> Result<Vec<Message, A>, IpcError> {
    let mut out = Vec::new_in(alloc);
    let mut at = 0;
    while at < bytes.len() {
        let (message, next) = raw(bytes, at)?;
        fn header(header: Option<Tab<'_>>) -> Result<Tab<'_>, IpcError> {
            header.ok_or(IpcError::Corrupt { field: None, what: "a message with no header" })
        }
        out.push(match message {
            Raw::End => Message::End,
            Raw::Message { tag: header::SCHEMA, .. } => Message::Schema,
            Raw::Message { tag: header::DICTIONARY, header: it, body } => {
                let it = header(it)?;
                let data = it.table(1)?.ok_or(IpcError::Corrupt { field: None, what: "a dictionary batch with no data" })?;
                Message::Dictionary { id: it.i64(0, 0)?, delta: it.bool(2)?, rows: Parts::of(data, body)?.rows }
            }
            Raw::Message { tag: header::RECORD, header: it, body } => Message::Batch { rows: Parts::of(header(it)?, body)?.rows },
            Raw::Message { tag, .. } => return Err(IpcError::Unsupported { field: None, what: "a message", code: i64::from(tag) }),
        });
        at = next;
    }
    Ok(out)
}

/// A stream's (or a file's) fields, metadata and batches.
pub struct Read<A: Allocator> {
    pub fields: Vec<Field<A>, A>,
    pub metadata: Vec<(String<A>, String<A>), A>,
    pub batches: Vec<Batch<A>, A>,
}

impl<A: Allocator> Read<A> {
    /// The value of the schema's metadata `key`.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.metadata.iter().find(|it| it.0.as_str() == key).map(|it| it.1.as_str())
    }
}

/// One stream followed across calls, and the next one after its [`END`].
pub struct StreamReader<A: Allocator + Clone> {
    schema: Option<Schema<A>>,
    dicts: Vec<(i64, Texts<A>), A>,
    streams: u64,
    alloc: A,
}

impl<A: Allocator + Clone> StreamReader<A> {
    pub fn new(alloc: A) -> StreamReader<A> {
        StreamReader { schema: None, dicts: Vec::new_in(alloc.clone()), streams: 0, alloc }
    }

    /// The batches of `bytes`, whole messages, appended to `into`. [`END`] ends the stream;
    /// bytes after it begin a new one, its schema first.
    pub fn read(&mut self, bytes: &[u8], into: &mut Vec<Batch<A>, A>) -> Result<(), IpcError> {
        let mut at = 0;
        while at < bytes.len() {
            let (message, next) = raw(bytes, at)?;
            at = next;
            let (tag, header, body) = match message {
                Raw::End => {
                    if self.schema.take().is_none() {
                        return Err(IpcError::Order { what: "an end of stream before any schema" });
                    }
                    self.dicts.clear();
                    continue;
                }
                Raw::Message { tag, header, body } => (tag, header, body),
            };
            let header = header.ok_or(IpcError::Corrupt { field: None, what: "a message with no header" })?;
            match (tag, &self.schema) {
                (header::SCHEMA, None) => {
                    self.schema = Some(read_schema(header, &self.alloc)?);
                    self.streams += 1;
                }
                (header::SCHEMA, Some(_)) => return Err(IpcError::Order { what: "a second schema before the stream's end" }),
                (_, None) => return Err(IpcError::Order { what: "a batch before the stream's schema" }),
                (header::DICTIONARY, Some(schema)) => dictionary(schema, &mut self.dicts, header, body, &self.alloc)?,
                (header::RECORD, Some(schema)) => into.push(record(schema, &self.dicts, header, body, &self.alloc)?),
                (tag, Some(_)) => return Err(IpcError::Unsupported { field: None, what: "a message", code: i64::from(tag) }),
            }
        }
        Ok(())
    }

    /// The fields of the stream under way.
    pub fn fields(&self) -> Option<&[Field<A>]> {
        self.schema.as_ref().map(|it| it.fields.as_slice())
    }

    /// The value of the stream's schema's metadata `key`.
    pub fn value(&self, key: &str) -> Option<&str> {
        let schema = self.schema.as_ref()?;
        schema.metadata.iter().find(|it| it.0.as_str() == key).map(|it| it.1.as_str())
    }

    /// Streams begun so far.
    pub fn streams(&self) -> u64 {
        self.streams
    }
}

/// One whole stream: its schema, batches and [`END`] (which may be absent at the bytes' end).
pub fn read_stream<A: Allocator + Clone>(bytes: &[u8], alloc: A) -> Result<Read<A>, IpcError> {
    let mut reader = StreamReader::new(alloc.clone());
    let mut batches = Vec::new_in(alloc.clone());
    let body = bytes.strip_suffix(&END).unwrap_or(bytes);
    reader.read(body, &mut batches)?;
    let schema = reader.schema.ok_or(IpcError::Order { what: "a stream with no schema" })?;
    Ok(Read { fields: schema.fields, metadata: schema.metadata, batches })
}

/// An Arrow IPC file: its footer's schema, then its dictionaries and record batches where the
/// footer's blocks say.
pub fn read_file<A: Allocator + Clone>(bytes: &[u8], alloc: A) -> Result<Read<A>, IpcError> {
    if bytes.len() < 8 + 10 || !bytes.starts_with(&MAGIC) || !bytes.ends_with(&MAGIC) {
        return Err(IpcError::NotArrow { why: "it does not begin and end with ARROW1" });
    }
    let len_at = bytes.len() - 10;
    let len = usize::try_from(i32_of(bytes, len_at)?).map_err(|_| IpcError::NotArrow { why: "a negative footer length" })?;
    let start = len_at.checked_sub(len).filter(|it| *it >= 8).ok_or(IpcError::NotArrow { why: "a footer longer than the file" })?;
    let footer = Tab::root(&bytes[start..len_at])?;
    let schema = footer.table(1)?.ok_or(IpcError::NotArrow { why: "a footer with no schema" })?;
    let schema = read_schema(schema, &alloc)?;
    let mut dicts = Vec::new_in(alloc.clone());
    let mut batches = Vec::new_in(alloc.clone());
    for (slot, want) in [(2, header::DICTIONARY), (3, header::RECORD)] {
        let blocks = footer.structs(slot, 24)?;
        for block in blocks.chunks_exact(24) {
            let offset = usize::try_from(i64_of(block, 0)?).map_err(|_| IpcError::NotArrow { why: "a block at a negative offset" })?;
            let (message, _) = raw(bytes, offset)?;
            let Raw::Message { tag, header: Some(header), body } = message else {
                return Err(IpcError::Corrupt { field: None, what: "a block that is no message" });
            };
            if tag != want {
                return Err(IpcError::Corrupt { field: None, what: "a block of another message than its list's" });
            }
            match tag {
                header::DICTIONARY => dictionary(&schema, &mut dicts, header, body, &alloc)?,
                _ => batches.push(record(&schema, &dicts, header, body, &alloc)?),
            }
        }
    }
    Ok(Read { fields: schema.fields, metadata: schema.metadata, batches })
}
