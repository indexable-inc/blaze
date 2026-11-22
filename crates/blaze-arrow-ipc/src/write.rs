//! Batches as Arrow IPC messages: a stream ([`StreamWriter`], [`stream`]) or a file ([`file`]).

use core::alloc::Allocator;

use blaze_columns::{Batch, Column, Field, Scalar, Texts, Type, Values};
use blaze_int::Int;
use blaze_own::Vec;

use crate::{
    END, IpcError, MAGIC,
    fb::{Builder, TableStart},
};

/// `Message.version`: V5.
pub(crate) const VERSION: i16 = 4;

/// `MessageHeader`'s union tags.
pub(crate) mod header {
    pub(crate) const SCHEMA: u8 = 1;
    pub(crate) const DICTIONARY: u8 = 2;
    pub(crate) const RECORD: u8 = 3;
}

/// `Type`'s union tags.
pub(crate) mod ty {
    pub(crate) const INT: u8 = 2;
    pub(crate) const FLOAT: u8 = 3;
    pub(crate) const UTF8: u8 = 5;
    pub(crate) const BOOL: u8 = 6;
    pub(crate) const LIST: u8 = 12;
}

/// The field whose values a dictionary holds: its place in the schema and the dictionary's id.
fn dictionary_fields<A: Allocator>(fields: &[Field<A>]) -> impl Iterator<Item = (usize, i64)> + '_ {
    fields
        .iter()
        .enumerate()
        .filter(|(_, it)| it.ty.scalar() == Scalar::Dict)
        .zip(0i64..)
        .map(|((at, _), id)| (at, id))
}

/// A field's type's union tag and table.
fn type_table<A: Allocator>(b: &mut Builder<A>, scalar: Scalar) -> (u8, usize) {
    let int = |b: &mut Builder<A>, bits: i32, signed: bool| {
        let mut t = b.start();
        b.i32(&mut t, 0, bits);
        b.bool(&mut t, 1, signed);
        (ty::INT, b.end(t))
    };
    match scalar {
        Scalar::Bool => (ty::BOOL, empty(b)),
        Scalar::I32 => int(b, 32, true),
        Scalar::I64 => int(b, 64, true),
        Scalar::U16 => int(b, 16, false),
        Scalar::U32 => int(b, 32, false),
        Scalar::U64 => int(b, 64, false),
        Scalar::F64 => {
            let mut t = b.start();
            // DOUBLE
            b.i16(&mut t, 0, 2);
            (ty::FLOAT, b.end(t))
        }
        Scalar::Text | Scalar::Dict => (ty::UTF8, empty(b)),
    }
}

fn empty<A: Allocator>(b: &mut Builder<A>) -> usize {
    let t = b.start();
    b.end(t)
}

/// A `Field` table: `name`, of `scalar`s (a list of them when `children` is one field), its
/// dictionary `id` when dictionary-encoded.
fn field_table<A: Allocator>(
    b: &mut Builder<A>,
    name: &str,
    nullable: bool,
    leaf: Option<(Scalar, Option<i64>)>,
    child: Option<usize>,
) -> usize {
    let name = b.string(name);
    let (tag, ty) = match leaf {
        Some((scalar, _)) => type_table(b, scalar),
        None => (ty::LIST, empty(b)),
    };
    let dictionary = match leaf {
        Some((_, Some(id))) => {
            let mut index = b.start();
            b.i32(&mut index, 0, 32);
            b.bool(&mut index, 1, false);
            let index = b.end(index);
            let mut t = b.start();
            b.i64(&mut t, 0, id);
            b.offset(&mut t, 1, index);
            b.bool(&mut t, 2, false);
            Some(b.end(t))
        }
        _ => None,
    };
    let children = match child {
        Some(it) => b.offsets(&[it]),
        None => b.offsets(&[]),
    };
    let mut t = b.start();
    b.offset(&mut t, 0, name);
    b.bool(&mut t, 1, nullable);
    b.u8(&mut t, 2, tag);
    b.offset(&mut t, 3, ty);
    if let Some(it) = dictionary {
        b.offset(&mut t, 4, it);
    }
    b.offset(&mut t, 5, children);
    b.end(t)
}

/// A `Schema` table of `fields` and `metadata`.
fn schema_table<A: Allocator + Clone>(b: &mut Builder<A>, fields: &[Field<A>], metadata: &[(&str, &str)], alloc: &A) -> usize {
    let mut ids = dictionary_fields(fields).peekable();
    let mut tables = Vec::with_capacity_in(fields.len(), alloc.clone());
    for (at, field) in fields.iter().enumerate() {
        let id = ids.next_if(|it| it.0 == at).map(|it| it.1);
        let table = match field.ty {
            Type::One(scalar) => field_table(b, field.name(), field.nullable, Some((scalar, id)), None),
            Type::List(scalar) => {
                let item = field_table(b, "item", false, Some((scalar, id)), None);
                field_table(b, field.name(), field.nullable, None, Some(item))
            }
        };
        tables.push(table);
    }
    let fields = b.offsets(&tables);
    let mut pairs = Vec::with_capacity_in(metadata.len(), alloc.clone());
    for (key, value) in metadata {
        let (key, value) = (b.string(key), b.string(value));
        let mut t = b.start();
        b.offset(&mut t, 0, key);
        b.offset(&mut t, 1, value);
        pairs.push(b.end(t));
    }
    let metadata = b.offsets(&pairs);
    let mut t = b.start();
    // Little-endian.
    b.i16(&mut t, 0, 0);
    b.offset(&mut t, 1, fields);
    b.offset(&mut t, 2, metadata);
    b.end(t)
}

/// A message's body: its buffers, each 8-aligned, and the field nodes and buffers that say
/// where they are.
struct Body<A: Allocator> {
    bytes: Vec<u8, A>,
    nodes: Vec<u8, A>,
    buffers: Vec<u8, A>,
    node_count: usize,
    buffer_count: usize,
}

impl<A: Allocator + Clone> Body<A> {
    fn new(alloc: &A) -> Body<A> {
        Body {
            bytes: Vec::new_in(alloc.clone()),
            nodes: Vec::new_in(alloc.clone()),
            buffers: Vec::new_in(alloc.clone()),
            node_count: 0,
            buffer_count: 0,
        }
    }

    fn node(&mut self, rows: usize, nulls: usize) {
        self.nodes.extend_from_slice(&rows.widen_to::<u64>().to_le_bytes());
        self.nodes.extend_from_slice(&nulls.widen_to::<u64>().to_le_bytes());
        self.node_count += 1;
    }

    /// A buffer of what `fill` writes.
    fn buffer(&mut self, fill: impl FnOnce(&mut Vec<u8, A>)) {
        let start = self.bytes.len();
        fill(&mut self.bytes);
        let len = self.bytes.len() - start;
        self.buffers.extend_from_slice(&start.widen_to::<u64>().to_le_bytes());
        self.buffers.extend_from_slice(&len.widen_to::<u64>().to_le_bytes());
        self.buffer_count += 1;
        while self.bytes.len() % 8 != 0 {
            self.bytes.push(0);
        }
    }

    fn bitmap(&mut self, bits: &[bool]) {
        self.buffer(|out| {
            for chunk in bits.chunks(8) {
                let byte = chunk.iter().enumerate().fold(0u8, |byte, (i, bit)| byte | (u8::from(*bit) << i));
                out.push(byte);
            }
        });
    }

    /// `offsets` as Arrow's 32-bit signed ones, less the first.
    fn offsets(&mut self, offsets: &[u32], field: usize) -> Result<(), IpcError> {
        let first = offsets.first().copied().unwrap_or(0);
        if offsets.last().is_some_and(|last| i32::try_from(last - first).is_err()) {
            return Err(IpcError::TooLarge { field, what: "offsets past i32::MAX, the most Arrow's Utf8 and List hold" });
        }
        self.buffer(|out| {
            for offset in offsets {
                out.extend_from_slice(&(offset - first).to_le_bytes());
            }
        });
        Ok(())
    }

    /// Texts `from..` of `texts` as a Utf8 array's two buffers.
    fn texts(&mut self, texts: &Texts<A>, from: usize, field: usize) -> Result<(), IpcError> {
        let offsets = &texts.offsets()[from..];
        self.offsets(offsets, field)?;
        let start = offsets.first().map_or(0, |it| it.widen_to::<usize>());
        self.buffer(|out| out.extend_from_slice(&texts.text().as_bytes()[start..]));
        Ok(())
    }

    /// The buffers of `values` (a column's, or a list's elements), `keys` in place of a
    /// dictionary's own.
    fn values(&mut self, values: &Values<A>, keys: Option<&[u32]>, field: usize) -> Result<(), IpcError> {
        macro_rules! le {
            ($it:expr) => {{
                let slice: &[_] = $it;
                self.buffer(|out| {
                    for value in slice {
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                })
            }};
        }
        match values {
            Values::Bool(it) => self.bitmap(it.as_slice()),
            Values::I32(it) => le!(it.as_slice()),
            Values::I64(it) => le!(it.as_slice()),
            Values::U16(it) => le!(it.as_slice()),
            Values::U32(it) => le!(it.as_slice()),
            Values::U64(it) => le!(it.as_slice()),
            Values::F64(it) => le!(it.as_slice()),
            Values::Text(it) => self.texts(it, 0, field)?,
            Values::Dict { keys: own, .. } => le!(keys.unwrap_or(own.as_slice())),
        }
        Ok(())
    }

    /// Column `field` of a batch, `keys` in place of its dictionary's own.
    fn column(&mut self, column: &Column<A>, keys: Option<&[u32]>, field: usize) -> Result<(), IpcError> {
        self.node(column.len(), column.null_count());
        match column.validity() {
            Some(valid) => self.bitmap(valid),
            None => self.buffer(|_| {}),
        }
        if let Some(offsets) = column.list_offsets() {
            self.offsets(offsets, field)?;
            self.node(column.buffer().len(), 0);
            self.buffer(|_| {});
        }
        self.values(column.buffer(), keys, field)
    }
}

/// A `RecordBatch` table of `rows` and `body`'s nodes and buffers.
fn record_table<A: Allocator>(b: &mut Builder<A>, rows: usize, body: &Body<A>) -> usize {
    let buffers = b.structs(&body.buffers, body.buffer_count);
    let nodes = b.structs(&body.nodes, body.node_count);
    let mut t = b.start();
    b.i64(&mut t, 0, i64::try_from(rows).unwrap_or(i64::MAX));
    b.offset(&mut t, 1, nodes);
    b.offset(&mut t, 2, buffers);
    b.end(t)
}

/// Where a message lies in a file: `Block`'s fields.
#[derive(Clone, Copy)]
struct Block {
    offset: usize,
    metadata: usize,
    body: usize,
}

/// One message, `header` (made by `make`) and `body`, appended to `out`: where it lies.
fn message<A: Allocator + Clone>(
    out: &mut Vec<u8, A>,
    tag: u8,
    body: &[u8],
    alloc: &A,
    make: impl FnOnce(&mut Builder<A>) -> usize,
) -> Block {
    let mut b = Builder::new_in(alloc.clone());
    let header = make(&mut b);
    let mut t: TableStart = b.start();
    b.i16(&mut t, 0, VERSION);
    b.u8(&mut t, 1, tag);
    b.offset(&mut t, 2, header);
    b.i64(&mut t, 3, i64::try_from(body.len()).unwrap_or(i64::MAX));
    let root = b.end(t);
    let mut meta = Vec::new_in(alloc.clone());
    b.finish(root, &mut meta);
    let offset = out.len();
    out.extend_from_slice(&END[..4]);
    out.extend_from_slice(&i32::try_from(meta.len()).unwrap_or(i32::MAX).to_le_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(body);
    Block { offset, metadata: 8 + meta.len(), body: body.len() }
}

/// A dictionary batch of `values` from `from` on.
fn dictionary_message<A: Allocator + Clone>(
    out: &mut Vec<u8, A>,
    id: i64,
    delta: bool,
    values: &Texts<A>,
    from: usize,
    field: usize,
    alloc: &A,
) -> Result<Block, IpcError> {
    let mut body = Body::new(alloc);
    let rows = values.len() - from;
    body.node(rows, 0);
    body.buffer(|_| {});
    body.texts(values, from, field)?;
    Ok(message(out, header::DICTIONARY, &body.bytes, alloc, |b| {
        let data = record_table(b, rows, &body);
        let mut t = b.start();
        b.i64(&mut t, 0, id);
        b.offset(&mut t, 1, data);
        b.bool(&mut t, 2, delta);
        b.end(t)
    }))
}

/// A record batch of `batch`, each dictionary column's keys from `keys` (by its place among
/// them).
fn record_message<A: Allocator + Clone>(
    out: &mut Vec<u8, A>,
    batch: &Batch<A>,
    keys: &[(usize, Vec<u32, A>)],
    alloc: &A,
) -> Result<Block, IpcError> {
    let mut body = Body::new(alloc);
    for (at, column) in batch.columns().iter().enumerate() {
        let own = keys.iter().find(|it| it.0 == at).map(|it| it.1.as_slice());
        body.column(column, own, at)?;
    }
    Ok(message(out, header::RECORD, &body.bytes, alloc, |b| record_table(b, batch.num_rows(), &body)))
}

/// One dictionary of a stream: its values in the order first named, a table of them by text,
/// and how many were sent.
struct Dict<A: Allocator> {
    field: usize,
    id: i64,
    values: Texts<A>,
    /// Open addressing over `values`' places; [`EMPTY`] where none.
    slots: Vec<u32, A>,
    sent: Option<usize>,
}

const EMPTY: u32 = u32::MAX;

/// FNV-1a: a table's hash, deterministic and seedless (keys are a stream's own text).
fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

impl<A: Allocator + Clone> Dict<A> {
    fn new(field: usize, id: i64, alloc: &A) -> Dict<A> {
        Dict { field, id, values: Texts::new_in(alloc.clone()), slots: Vec::from_elem_in(EMPTY, 16, alloc.clone()), sent: None }
    }

    fn place(slots: &[u32], values: &Texts<A>, text: &str) -> (usize, Option<u32>) {
        let mask = slots.len() - 1;
        let mut at = usize::try_from(hash(text) & mask.widen_to::<u64>()).unwrap_or(0);
        loop {
            match slots[at] {
                EMPTY => return (at, None),
                key if values.get(key.widen_to::<usize>()) == text => return (at, Some(key)),
                _ => at = (at + 1) & mask,
            }
        }
    }

    /// `text`'s key, added when new.
    fn key(&mut self, text: &str, field: usize) -> Result<u32, IpcError> {
        let (at, found) = Dict::place(&self.slots, &self.values, text);
        if let Some(key) = found {
            return Ok(key);
        }
        let key = u32::try_from(self.values.len())
            .ok()
            .filter(|it| *it != EMPTY)
            .ok_or(IpcError::TooLarge { field, what: "a dictionary past u32::MAX values" })?;
        self.values.try_push(text).map_err(|error| IpcError::Column { field, error })?;
        self.slots[at] = key;
        if self.values.len() * 2 > self.slots.len() {
            let mut slots = Vec::from_elem_in(EMPTY, self.slots.len() * 2, self.slots.allocator().clone());
            for key in 0..self.values.len() {
                let (at, _) = Dict::place(&slots, &self.values, self.values.get(key));
                slots[at] = u32::try_from(key).unwrap_or(EMPTY);
            }
            self.slots = slots;
        }
        Ok(key)
    }

    /// `column`'s keys onto this dictionary, its null rows' 0.
    fn rekey(&mut self, column: &Column<A>, alloc: &A) -> Result<Vec<u32, A>, IpcError> {
        let field = self.field;
        let Values::Dict { keys, values } = column.buffer() else {
            return Err(IpcError::FieldsDiffer);
        };
        let valid = column.list_offsets().is_none().then(|| column.validity()).flatten();
        let mut ours = Vec::from_elem_in(EMPTY, values.len(), alloc.clone());
        let mut out = Vec::with_capacity_in(keys.len(), alloc.clone());
        for (row, key) in keys.iter().enumerate() {
            if valid.is_some_and(|it| !it[row]) {
                out.push(0);
                continue;
            }
            let key = key.widen_to::<usize>();
            if ours[key] == EMPTY {
                ours[key] = self.key(values.get(key), field)?;
            }
            out.push(ours[key]);
        }
        Ok(out)
    }
}

/// One long-lived stream: its fields, and each dictionary column's values so far.
pub struct StreamWriter<A: Allocator + Clone> {
    fields: Vec<Field<A>, A>,
    metadata: Vec<(blaze_own::String<A>, blaze_own::String<A>), A>,
    dicts: Vec<Dict<A>, A>,
    begun: bool,
    failed: bool,
    alloc: A,
}

impl<A: Allocator + Clone> StreamWriter<A> {
    pub fn new(fields: &[Field<A>], metadata: &[(&str, &str)], alloc: A) -> StreamWriter<A> {
        let fields = Vec::from_iter_in(fields.iter().cloned(), alloc.clone());
        let pairs = metadata.iter().map(|(key, value)| {
            (blaze_own::String::from_str_in(key, alloc.clone()), blaze_own::String::from_str_in(value, alloc.clone()))
        });
        let metadata = Vec::from_iter_in(pairs, alloc.clone());
        let dicts = Vec::from_iter_in(dictionary_fields(&fields).map(|(at, id)| Dict::new(at, id, &alloc)), alloc.clone());
        StreamWriter { fields, metadata, dicts, begun: false, failed: false, alloc }
    }

    /// The schema, when the stream has not begun.
    fn begin(&mut self, out: &mut Vec<u8, A>) {
        if self.begun {
            return;
        }
        self.begun = true;
        let alloc = self.alloc.clone();
        let pairs = Vec::from_iter_in(self.metadata.iter().map(|(k, v)| (k.as_str(), v.as_str())), alloc.clone());
        let fields = &self.fields;
        message(out, header::SCHEMA, &[], &alloc, |b| schema_table(b, fields, &pairs, &alloc));
    }

    /// Appends to `out` the schema (at the stream's start), a dictionary batch for each
    /// dictionary column of the values it names first (a delta after the stream's first), then
    /// the record batch. On an error `out` is as it was and the writer refuses every later batch
    /// ([`IpcError::Failed`]): its stream must end.
    pub fn write(&mut self, batch: &Batch<A>, out: &mut Vec<u8, A>) -> Result<(), IpcError> {
        if self.failed {
            return Err(IpcError::Failed);
        }
        if batch.fields() != self.fields.as_slice() {
            return Err(IpcError::FieldsDiffer);
        }
        let before = out.len();
        let written = self.messages(batch, out);
        if written.is_err() {
            self.failed = true;
            out.truncate(before);
        }
        written
    }

    fn messages(&mut self, batch: &Batch<A>, out: &mut Vec<u8, A>) -> Result<(), IpcError> {
        self.begin(out);
        let alloc = self.alloc.clone();
        let mut keys = Vec::with_capacity_in(self.dicts.len(), alloc.clone());
        for dict in self.dicts.iter_mut() {
            keys.push((dict.field, dict.rekey(batch.column(dict.field), &alloc)?));
            let from = dict.sent.unwrap_or(0);
            if dict.sent.is_none() || dict.values.len() > from {
                dictionary_message(out, dict.id, dict.sent.is_some(), &dict.values, from, dict.field, &alloc)?;
                dict.sent = Some(dict.values.len());
            }
        }
        record_message(out, batch, &keys, &alloc)?;
        Ok(())
    }

    /// The distinct values and the bytes of their text the stream's dictionaries hold.
    pub fn dictionary_size(&self) -> (usize, usize) {
        self.dicts.iter().fold((0, 0), |(values, bytes), it| (values + it.values.len(), bytes + it.values.text().len()))
    }

    /// The stream's end: the schema when nothing was written, then [`END`].
    pub fn finish(mut self, out: &mut Vec<u8, A>) {
        self.begin(out);
        out.extend_from_slice(&END);
    }
}

/// `batches` as one whole stream, [`END`] last.
pub fn stream<A: Allocator + Clone>(
    fields: &[Field<A>],
    metadata: &[(&str, &str)],
    batches: &[Batch<A>],
    alloc: A,
) -> Result<Vec<u8, A>, IpcError> {
    let mut out = Vec::new_in(alloc.clone());
    let mut writer = StreamWriter::new(fields, metadata, alloc);
    for batch in batches {
        writer.write(batch, &mut out)?;
    }
    writer.finish(&mut out);
    Ok(out)
}

fn block_bytes<A: Allocator>(blocks: &[Block], out: &mut Vec<u8, A>) {
    for block in blocks {
        out.extend_from_slice(&block.offset.widen_to::<u64>().to_le_bytes());
        out.extend_from_slice(&u32::try_from(block.metadata).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&block.body.widen_to::<u64>().to_le_bytes());
    }
}

/// `batches` as one Arrow IPC file: each dictionary whole, once, before the record batches.
pub fn file<A: Allocator + Clone>(
    fields: &[Field<A>],
    metadata: &[(&str, &str)],
    batches: &[Batch<A>],
    alloc: A,
) -> Result<Vec<u8, A>, IpcError> {
    let mut writer = StreamWriter::new(fields, metadata, alloc.clone());
    let mut keyed = Vec::with_capacity_in(batches.len(), alloc.clone());
    for batch in batches {
        if batch.fields() != writer.fields.as_slice() {
            return Err(IpcError::FieldsDiffer);
        }
        let mut keys = Vec::with_capacity_in(writer.dicts.len(), alloc.clone());
        for dict in writer.dicts.iter_mut() {
            keys.push((dict.field, dict.rekey(batch.column(dict.field), &alloc)?));
        }
        keyed.push(keys);
    }
    let mut out = Vec::new_in(alloc.clone());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[0; 2]);
    writer.begin(&mut out);
    let mut dictionaries = Vec::with_capacity_in(writer.dicts.len(), alloc.clone());
    for dict in writer.dicts.iter() {
        dictionaries.push(dictionary_message(&mut out, dict.id, false, &dict.values, 0, dict.field, &alloc)?);
    }
    let mut records = Vec::with_capacity_in(batches.len(), alloc.clone());
    for (batch, keys) in batches.iter().zip(keyed.iter()) {
        records.push(record_message(&mut out, batch, keys, &alloc)?);
    }
    out.extend_from_slice(&END);
    let mut b = Builder::new_in(alloc.clone());
    let mut bytes = Vec::new_in(alloc.clone());
    block_bytes(&records, &mut bytes);
    let records = b.structs(&bytes, records.len());
    bytes.clear();
    block_bytes(&dictionaries, &mut bytes);
    let dictionaries = b.structs(&bytes, dictionaries.len());
    let pairs = Vec::from_iter_in(writer.metadata.iter().map(|(k, v)| (k.as_str(), v.as_str())), alloc.clone());
    let schema = schema_table(&mut b, &writer.fields, &pairs, &alloc);
    let mut t = b.start();
    b.i16(&mut t, 0, VERSION);
    b.offset(&mut t, 1, schema);
    b.offset(&mut t, 2, dictionaries);
    b.offset(&mut t, 3, records);
    let root = b.end(t);
    let mut footer = Vec::new_in(alloc);
    b.finish(root, &mut footer);
    out.extend_from_slice(&footer);
    out.extend_from_slice(&i32::try_from(footer.len()).unwrap_or(i32::MAX).to_le_bytes());
    out.extend_from_slice(&MAGIC);
    Ok(out)
}
