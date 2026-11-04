//! The `.ixprof` container (docs/profiling.md, "The file"): magic, a canonical JSON
//! manifest, then one Arrow IPC file a table, each 8-aligned and hashed with blake3.

use std::{collections::BTreeMap, io::Cursor, sync::Arc};

use arrow_array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int64Array, RecordBatch,
    StringArray, UInt16Array, UInt32Array, UInt64Array, types::UInt32Type,
};
use arrow_schema::{DataType, Field, Schema};

use crate::{
    json::{self, Json},
    profile::{
        Alloc, AllocEvent, Counter, CpuSample, Frame, FrameKind, Meta, Profile, Span, SpanKind,
        StackNode, Thread,
    },
};

pub const MAGIC: [u8; 8] = *b"IXPROF\0\x01";
pub const FORMAT_VERSION: u32 = 1;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

#[derive(Debug)]
pub enum Error {
    NotIxprof(&'static str),
    Manifest(String),
    Table { table: String, why: String },
    Hash { table: String },
    Version(u64),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotIxprof(why) => write!(f, "not an .ixprof file: {why}"),
            Error::Manifest(why) => write!(f, ".ixprof manifest: {why}"),
            Error::Table { table, why } => write!(f, ".ixprof table `{table}`: {why}"),
            Error::Hash { table } => {
                write!(f, ".ixprof table `{table}`: its bytes do not match the manifest's blake3")
            }
            Error::Version(v) => {
                write!(f, ".ixprof format_version {v} is newer than this reader's {FORMAT_VERSION}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// A column's type in the format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    U16,
    U32,
    U64,
    I64,
    F64,
    Bool,
    Utf8,
    Dict,
}

impl Ty {
    fn arrow(self) -> DataType {
        match self {
            Ty::U16 => DataType::UInt16,
            Ty::U32 => DataType::UInt32,
            Ty::U64 => DataType::UInt64,
            Ty::I64 => DataType::Int64,
            Ty::F64 => DataType::Float64,
            Ty::Bool => DataType::Boolean,
            Ty::Utf8 => DataType::Utf8,
            Ty::Dict => DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
        }
    }
}

/// One table's columns: (name, type, nullable).
pub type Columns = &'static [(&'static str, Ty, bool)];

/// Every table of the format, in name order, with its columns: the code form of the tables
/// in docs/profiling.md.
pub const TABLES: [(&str, Columns); 8] = [
    (
        "alloc_events",
        &[
            ("process", Ty::U32, false),
            ("thread", Ty::U64, false),
            ("t_ns", Ty::I64, false),
            ("stack", Ty::U32, false),
            ("bytes", Ty::I64, false),
            ("span_id", Ty::U64, true),
        ],
    ),
    (
        "allocs",
        &[
            ("stack", Ty::U32, false),
            ("span_name", Ty::Dict, true),
            ("alloc_count", Ty::U64, false),
            ("alloc_bytes", Ty::U64, false),
            ("live_count", Ty::U64, false),
            ("live_bytes", Ty::U64, false),
            ("sampled", Ty::U64, false),
        ],
    ),
    (
        "counters",
        &[
            ("process", Ty::U32, false),
            ("name", Ty::Dict, false),
            ("t_ns", Ty::I64, false),
            ("value", Ty::F64, false),
        ],
    ),
    (
        "cpu_samples",
        &[
            ("process", Ty::U32, false),
            ("thread", Ty::U64, false),
            ("t_ns", Ty::I64, false),
            ("stack", Ty::U32, false),
            ("span_id", Ty::U64, true),
            ("on_cpu", Ty::Bool, false),
            ("weight_ns", Ty::U64, false),
        ],
    ),
    (
        "frames",
        &[
            ("function", Ty::Dict, false),
            ("system_name", Ty::Dict, true),
            ("file", Ty::Dict, true),
            ("line", Ty::U32, true),
            ("column", Ty::U32, true),
            ("start_line", Ty::U32, true),
            ("module", Ty::Dict, true),
            ("build_id", Ty::Dict, true),
            ("address", Ty::U64, true),
            ("inline_depth", Ty::U16, false),
            ("kind", Ty::Dict, false),
        ],
    ),
    (
        "spans",
        &[
            ("process", Ty::U32, false),
            ("thread", Ty::U64, false),
            ("span_id", Ty::U64, false),
            ("parent", Ty::U64, true),
            ("depth", Ty::U16, false),
            ("name", Ty::Dict, false),
            ("target", Ty::Dict, false),
            ("kind", Ty::Dict, false),
            ("task", Ty::U64, true),
            ("file", Ty::Dict, true),
            ("line", Ty::U32, true),
            ("start_ns", Ty::I64, false),
            ("dur_ns", Ty::I64, false),
            ("busy_ns", Ty::I64, true),
            ("self_cpu_ns", Ty::I64, true),
            ("alloc_bytes", Ty::U64, true),
            ("allocs", Ty::U64, true),
            ("frees_bytes", Ty::U64, true),
            ("wait", Ty::Dict, true),
            ("wait_on", Ty::Dict, true),
            ("wait_owner", Ty::Dict, true),
            ("fields", Ty::Utf8, true),
        ],
    ),
    ("stacks", &[("parent", Ty::U32, true), ("frame", Ty::U32, false), ("depth", Ty::U16, false)]),
    (
        "threads",
        &[
            ("process", Ty::U32, false),
            ("process_name", Ty::Dict, false),
            ("thread", Ty::U64, false),
            ("thread_name", Ty::Dict, true),
            ("cpu_ns", Ty::U64, true),
        ],
    ),
];

/// The columns of `table`.
/// The live stream's `open_spans` has the columns of `spans`.
pub fn columns(table: &str) -> Option<Columns> {
    let table = if table == "open_spans" { "spans" } else { table };
    TABLES.iter().find(|it| it.0 == table).map(|it| it.1)
}

/// A column's values, before they are Arrow.
#[derive(Clone, Debug, PartialEq)]
pub enum Col {
    U16(Vec<Option<u16>>),
    U32(Vec<Option<u32>>),
    U64(Vec<Option<u64>>),
    I64(Vec<Option<i64>>),
    F64(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    /// `Utf8` and `Dict` both.
    Str(Vec<Option<String>>),
}

impl Col {
    fn len(&self) -> usize {
        match self {
            Col::U16(it) => it.len(),
            Col::U32(it) => it.len(),
            Col::U64(it) => it.len(),
            Col::I64(it) => it.len(),
            Col::F64(it) => it.len(),
            Col::Bool(it) => it.len(),
            Col::Str(it) => it.len(),
        }
    }

    fn array(&self, ty: Ty) -> ArrayRef {
        match (self, ty) {
            (Col::U16(it), _) => Arc::new(UInt16Array::from(it.clone())),
            (Col::U32(it), _) => Arc::new(UInt32Array::from(it.clone())),
            (Col::U64(it), _) => Arc::new(UInt64Array::from(it.clone())),
            (Col::I64(it), _) => Arc::new(Int64Array::from(it.clone())),
            (Col::F64(it), _) => Arc::new(Float64Array::from(it.clone())),
            (Col::Bool(it), _) => Arc::new(BooleanArray::from(it.clone())),
            (Col::Str(it), Ty::Dict) => {
                Arc::new(it.iter().map(|it| it.as_deref()).collect::<DictionaryArray<UInt32Type>>())
            }
            (Col::Str(it), _) => Arc::new(StringArray::from(it.clone())),
        }
    }

    fn read(array: &dyn Array, ty: Ty) -> Result<Col, String> {
        fn get<T: 'static>(array: &dyn Array) -> Result<&T, String> {
            array
                .as_any()
                .downcast_ref::<T>()
                .ok_or_else(|| format!("is {}, not the type the format gives", array.data_type()))
        }
        let rows = 0..array.len();
        Ok(match ty {
            Ty::U16 => {
                let it = get::<UInt16Array>(array)?;
                Col::U16(rows.map(|i| (!it.is_null(i)).then(|| it.value(i))).collect())
            }
            Ty::U32 => {
                let it = get::<UInt32Array>(array)?;
                Col::U32(rows.map(|i| (!it.is_null(i)).then(|| it.value(i))).collect())
            }
            Ty::U64 => {
                let it = get::<UInt64Array>(array)?;
                Col::U64(rows.map(|i| (!it.is_null(i)).then(|| it.value(i))).collect())
            }
            Ty::I64 => {
                let it = get::<Int64Array>(array)?;
                Col::I64(rows.map(|i| (!it.is_null(i)).then(|| it.value(i))).collect())
            }
            Ty::F64 => {
                let it = get::<Float64Array>(array)?;
                Col::F64(rows.map(|i| (!it.is_null(i)).then(|| it.value(i))).collect())
            }
            Ty::Bool => {
                let it = get::<BooleanArray>(array)?;
                Col::Bool(rows.map(|i| (!it.is_null(i)).then(|| it.value(i))).collect())
            }
            Ty::Utf8 => {
                let it = get::<StringArray>(array)?;
                Col::Str(rows.map(|i| (!it.is_null(i)).then(|| it.value(i).to_owned())).collect())
            }
            Ty::Dict => {
                let dict = get::<DictionaryArray<UInt32Type>>(array)?;
                let keys = dict.keys();
                let values = get::<StringArray>(dict.values().as_ref())?;
                Col::Str(
                    rows.map(|i| {
                        if keys.is_null(i) {
                            return Ok(None);
                        }
                        let key = keys.value(i) as usize;
                        if key >= values.len() {
                            return Err(format!(
                                "dictionary key {key} past its {} values",
                                values.len()
                            ));
                        }
                        Ok((!values.is_null(key)).then(|| values.value(key).to_owned()))
                    })
                    .collect::<Result<_, String>>()?,
                )
            }
        })
    }
}

/// A table's columns, in the format's order.
pub struct Table {
    pub name: &'static str,
    pub cols: Vec<Col>,
}

impl Table {
    pub fn rows(&self) -> usize {
        self.cols.first().map_or(0, Col::len)
    }

    /// The Arrow record batch.
    pub fn batch(&self) -> Result<RecordBatch, Error> {
        let spec = columns(self.name).ok_or_else(|| Error::Table {
            table: self.name.to_owned(),
            why: "is not a table of the format".to_owned(),
        })?;
        let fields: Vec<Field> = spec
            .iter()
            .map(|(name, ty, nullable)| Field::new(*name, ty.arrow(), *nullable))
            .collect();
        let arrays: Vec<ArrayRef> =
            self.cols.iter().zip(spec).map(|(col, (_, ty, _))| col.array(*ty)).collect();
        #[allow(clippy::disallowed_types, reason = "arrow's schema metadata is a std HashMap")]
        let schema = Schema::new(fields).with_metadata(
            [
                ("ixprof.table".to_owned(), self.name.to_owned()),
                ("ixprof.format_version".to_owned(), FORMAT_VERSION.to_string()),
            ]
            .into_iter()
            .collect::<std::collections::HashMap<String, String>>(),
        );
        RecordBatch::try_new(Arc::new(schema), arrays)
            .map_err(|why| Error::Table { table: self.name.to_owned(), why: why.to_string() })
    }

    /// The table as one Arrow IPC file.
    pub fn ipc_file(&self) -> Result<Vec<u8>, Error> {
        let batch = self.batch()?;
        let err = |why: arrow_schema::ArrowError| Error::Table {
            table: self.name.to_owned(),
            why: why.to_string(),
        };
        let mut out = Vec::new();
        let mut writer =
            arrow_ipc::writer::FileWriter::try_new(&mut out, batch.schema_ref()).map_err(err)?;
        writer.write(&batch).map_err(err)?;
        writer.finish().map_err(err)?;
        drop(writer);
        Ok(out)
    }

    /// The table as one Arrow IPC stream (schema, one batch, end of stream): what the live
    /// stream's BATCH frames carry.
    pub fn ipc_stream(&self) -> Result<Vec<u8>, Error> {
        let batch = self.batch()?;
        let err = |why: arrow_schema::ArrowError| Error::Table {
            table: self.name.to_owned(),
            why: why.to_string(),
        };
        let mut out = Vec::new();
        let mut writer =
            arrow_ipc::writer::StreamWriter::try_new(&mut out, batch.schema_ref()).map_err(err)?;
        writer.write(&batch).map_err(err)?;
        writer.finish().map_err(err)?;
        drop(writer);
        Ok(out)
    }

    /// Reads table `name` from an Arrow IPC file.
    pub fn read_ipc_file(name: &'static str, bytes: &[u8]) -> Result<Table, Error> {
        let err = |why: String| Error::Table { table: name.to_owned(), why };
        let spec = columns(name).ok_or_else(|| err("is not a table of the format".to_owned()))?;
        let reader = arrow_ipc::reader::FileReader::try_new(Cursor::new(bytes), None)
            .map_err(|it| err(it.to_string()))?;
        let mut cols: Vec<Option<Col>> = spec.iter().map(|_| None).collect();
        for batch in reader {
            let batch = batch.map_err(|it| err(it.to_string()))?;
            for (i, (column, ty, _)) in spec.iter().enumerate() {
                let Some(array) = batch.column_by_name(column) else {
                    return Err(err(format!("has no column `{column}`")));
                };
                let read = Col::read(array.as_ref(), *ty)
                    .map_err(|why| err(format!("column `{column}` {why}")))?;
                cols[i] = Some(match (cols[i].take(), read) {
                    (None, it) => it,
                    (Some(old), new) => append(old, new),
                });
            }
        }
        let cols = cols
            .into_iter()
            .zip(spec)
            .map(|(col, (_, ty, _))| col.unwrap_or_else(|| empty(*ty)))
            .collect();
        Ok(Table { name, cols })
    }
}

fn empty(ty: Ty) -> Col {
    match ty {
        Ty::U16 => Col::U16(Vec::new()),
        Ty::U32 => Col::U32(Vec::new()),
        Ty::U64 => Col::U64(Vec::new()),
        Ty::I64 => Col::I64(Vec::new()),
        Ty::F64 => Col::F64(Vec::new()),
        Ty::Bool => Col::Bool(Vec::new()),
        Ty::Utf8 | Ty::Dict => Col::Str(Vec::new()),
    }
}

fn append(old: Col, new: Col) -> Col {
    match (old, new) {
        (Col::U16(mut a), Col::U16(b)) => {
            a.extend(b);
            Col::U16(a)
        }
        (Col::U32(mut a), Col::U32(b)) => {
            a.extend(b);
            Col::U32(a)
        }
        (Col::U64(mut a), Col::U64(b)) => {
            a.extend(b);
            Col::U64(a)
        }
        (Col::I64(mut a), Col::I64(b)) => {
            a.extend(b);
            Col::I64(a)
        }
        (Col::F64(mut a), Col::F64(b)) => {
            a.extend(b);
            Col::F64(a)
        }
        (Col::Bool(mut a), Col::Bool(b)) => {
            a.extend(b);
            Col::Bool(a)
        }
        (Col::Str(mut a), Col::Str(b)) => {
            a.extend(b);
            Col::Str(a)
        }
        (old, _) => old,
    }
}

// Rows to columns and back, one pair a table.

fn some<T: Copy>(rows: impl Iterator<Item = T>) -> Vec<Option<T>> {
    rows.map(Some).collect()
}

fn strs<'a>(rows: impl Iterator<Item = &'a str>) -> Col {
    Col::Str(rows.map(|it| Some(it.to_owned())).collect())
}

fn opt_strs<'a>(rows: impl Iterator<Item = Option<&'a str>>) -> Col {
    Col::Str(rows.map(|it| it.map(str::to_owned)).collect())
}

/// Every table of `profile` that has rows, in name order.
pub fn tables(p: &Profile) -> Vec<Table> {
    let mut out = Vec::new();
    let e = &p.alloc_events;
    out.push(Table {
        name: "alloc_events",
        cols: vec![
            Col::U32(some(e.iter().map(|it| it.process))),
            Col::U64(some(e.iter().map(|it| it.thread))),
            Col::I64(some(e.iter().map(|it| it.t_ns))),
            Col::U32(some(e.iter().map(|it| it.stack))),
            Col::I64(some(e.iter().map(|it| it.bytes))),
            Col::U64(e.iter().map(|it| it.span_id).collect()),
        ],
    });
    let a = &p.allocs;
    out.push(Table {
        name: "allocs",
        cols: vec![
            Col::U32(some(a.iter().map(|it| it.stack))),
            opt_strs(a.iter().map(|it| it.span_name.as_deref())),
            Col::U64(some(a.iter().map(|it| it.alloc_count))),
            Col::U64(some(a.iter().map(|it| it.alloc_bytes))),
            Col::U64(some(a.iter().map(|it| it.live_count))),
            Col::U64(some(a.iter().map(|it| it.live_bytes))),
            Col::U64(some(a.iter().map(|it| it.sampled))),
        ],
    });
    let c = &p.counters;
    out.push(Table {
        name: "counters",
        cols: vec![
            Col::U32(some(c.iter().map(|it| it.process))),
            strs(c.iter().map(|it| it.name.as_str())),
            Col::I64(some(c.iter().map(|it| it.t_ns))),
            Col::F64(some(c.iter().map(|it| it.value))),
        ],
    });
    out.push(cpu_samples(&p.cpu_samples));
    out.push(frames(&p.frames));
    out.push(spans("spans", &p.spans));
    out.push(stacks(&p.stacks));
    out.push(threads(&p.threads));
    out.retain(|it| it.rows() > 0);
    out
}

pub(crate) fn cpu_samples(s: &[CpuSample]) -> Table {
    Table {
        name: "cpu_samples",
        cols: vec![
            Col::U32(some(s.iter().map(|it| it.process))),
            Col::U64(some(s.iter().map(|it| it.thread))),
            Col::I64(some(s.iter().map(|it| it.t_ns))),
            Col::U32(some(s.iter().map(|it| it.stack))),
            Col::U64(s.iter().map(|it| it.span_id).collect()),
            Col::Bool(some(s.iter().map(|it| it.on_cpu))),
            Col::U64(some(s.iter().map(|it| it.weight_ns))),
        ],
    }
}

pub(crate) fn frames(f: &[Frame]) -> Table {
    Table {
        name: "frames",
        cols: vec![
            strs(f.iter().map(|it| it.function.as_str())),
            opt_strs(f.iter().map(|it| it.system_name.as_deref())),
            opt_strs(f.iter().map(|it| it.file.as_deref())),
            Col::U32(f.iter().map(|it| it.line).collect()),
            Col::U32(f.iter().map(|it| it.column).collect()),
            Col::U32(f.iter().map(|it| it.start_line).collect()),
            opt_strs(f.iter().map(|it| it.module.as_deref())),
            opt_strs(f.iter().map(|it| it.build_id.as_deref())),
            Col::U64(f.iter().map(|it| it.address).collect()),
            Col::U16(some(f.iter().map(|it| it.inline_depth))),
            strs(f.iter().map(|it| it.kind.as_str())),
        ],
    }
}

/// `spans`, or the live stream's `open_spans` (the same columns).
pub(crate) fn spans(name: &'static str, s: &[Span]) -> Table {
    Table {
        name,
        cols: vec![
            Col::U32(some(s.iter().map(|it| it.process))),
            Col::U64(some(s.iter().map(|it| it.thread))),
            Col::U64(some(s.iter().map(|it| it.span_id))),
            Col::U64(s.iter().map(|it| it.parent).collect()),
            Col::U16(some(s.iter().map(|it| it.depth))),
            strs(s.iter().map(|it| it.name.as_str())),
            strs(s.iter().map(|it| it.target.as_str())),
            strs(s.iter().map(|it| it.kind.as_str())),
            Col::U64(s.iter().map(|it| it.task).collect()),
            opt_strs(s.iter().map(|it| it.file.as_deref())),
            Col::U32(s.iter().map(|it| it.line).collect()),
            Col::I64(some(s.iter().map(|it| it.start_ns))),
            Col::I64(some(s.iter().map(|it| it.dur_ns))),
            Col::I64(s.iter().map(|it| it.busy_ns).collect()),
            Col::I64(s.iter().map(|it| it.self_cpu_ns).collect()),
            Col::U64(s.iter().map(|it| it.alloc_bytes).collect()),
            Col::U64(s.iter().map(|it| it.allocs).collect()),
            Col::U64(s.iter().map(|it| it.frees_bytes).collect()),
            opt_strs(s.iter().map(|it| it.wait.as_deref())),
            opt_strs(s.iter().map(|it| it.wait_on.as_deref())),
            opt_strs(s.iter().map(|it| it.wait_owner.as_deref())),
            Col::Str(s.iter().map(|it| it.fields.clone()).collect()),
        ],
    }
}

pub(crate) fn stacks(s: &[StackNode]) -> Table {
    Table {
        name: "stacks",
        cols: vec![
            Col::U32(s.iter().map(|it| it.parent).collect()),
            Col::U32(some(s.iter().map(|it| it.frame))),
            Col::U16(some(s.iter().map(|it| it.depth))),
        ],
    }
}

pub(crate) fn threads(t: &[Thread]) -> Table {
    Table {
        name: "threads",
        cols: vec![
            Col::U32(some(t.iter().map(|it| it.process))),
            strs(t.iter().map(|it| it.process_name.as_str())),
            Col::U64(some(t.iter().map(|it| it.thread))),
            opt_strs(t.iter().map(|it| it.thread_name.as_deref())),
            Col::U64(t.iter().map(|it| it.cpu_ns).collect()),
        ],
    }
}

/// Column `i` of `table` as the type the caller expects; a mismatch is an error naming both.
struct Reader<'a> {
    table: &'a Table,
    next: usize,
}

impl<'a> Reader<'a> {
    fn err(&self, why: &str) -> Error {
        Error::Table {
            table: self.table.name.to_owned(),
            why: format!("column {}: {why}", self.next),
        }
    }

    fn take(&mut self) -> Result<&'a Col, Error> {
        let col = self.table.cols.get(self.next).ok_or_else(|| self.err("missing"))?;
        self.next += 1;
        Ok(col)
    }

    fn u16(&mut self) -> Result<&'a [Option<u16>], Error> {
        match self.take()? {
            Col::U16(it) => Ok(it),
            _ => Err(self.err("not u16")),
        }
    }
    fn u32(&mut self) -> Result<&'a [Option<u32>], Error> {
        match self.take()? {
            Col::U32(it) => Ok(it),
            _ => Err(self.err("not u32")),
        }
    }
    fn u64(&mut self) -> Result<&'a [Option<u64>], Error> {
        match self.take()? {
            Col::U64(it) => Ok(it),
            _ => Err(self.err("not u64")),
        }
    }
    fn i64(&mut self) -> Result<&'a [Option<i64>], Error> {
        match self.take()? {
            Col::I64(it) => Ok(it),
            _ => Err(self.err("not i64")),
        }
    }
    fn f64(&mut self) -> Result<&'a [Option<f64>], Error> {
        match self.take()? {
            Col::F64(it) => Ok(it),
            _ => Err(self.err("not f64")),
        }
    }
    fn bool(&mut self) -> Result<&'a [Option<bool>], Error> {
        match self.take()? {
            Col::Bool(it) => Ok(it),
            _ => Err(self.err("not bool")),
        }
    }
    fn str(&mut self) -> Result<&'a [Option<String>], Error> {
        match self.take()? {
            Col::Str(it) => Ok(it),
            _ => Err(self.err("not a string")),
        }
    }
}

/// A required value: null in a non-null column is an error naming the table and row.
fn req<T: Clone>(col: &[Option<T>], row: usize, table: &str, what: &str) -> Result<T, Error> {
    col.get(row).cloned().flatten().ok_or_else(|| Error::Table {
        table: table.to_owned(),
        why: format!("row {row}: `{what}` is null or missing"),
    })
}

fn opt<T: Clone>(col: &[Option<T>], row: usize) -> Option<T> {
    col.get(row).cloned().flatten()
}

fn rows_into(p: &mut Profile, t: &Table) -> Result<(), Error> {
    let n = t.rows();
    let name = t.name;
    let mut r = Reader { table: t, next: 0 };
    match name {
        "alloc_events" => {
            let (pr, th, ts, st, by, sp) =
                (r.u32()?, r.u64()?, r.i64()?, r.u32()?, r.i64()?, r.u64()?);
            for i in 0..n {
                p.alloc_events.push(AllocEvent {
                    process: req(pr, i, name, "process")?,
                    thread: req(th, i, name, "thread")?,
                    t_ns: req(ts, i, name, "t_ns")?,
                    stack: req(st, i, name, "stack")?,
                    bytes: req(by, i, name, "bytes")?,
                    span_id: opt(sp, i),
                });
            }
        }
        "allocs" => {
            let (st, sn, ac, ab, lc, lb, sa) =
                (r.u32()?, r.str()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?);
            for i in 0..n {
                p.allocs.push(Alloc {
                    stack: req(st, i, name, "stack")?,
                    span_name: opt(sn, i),
                    alloc_count: req(ac, i, name, "alloc_count")?,
                    alloc_bytes: req(ab, i, name, "alloc_bytes")?,
                    live_count: req(lc, i, name, "live_count")?,
                    live_bytes: req(lb, i, name, "live_bytes")?,
                    sampled: req(sa, i, name, "sampled")?,
                });
            }
        }
        "counters" => {
            let (pr, nm, ts, va) = (r.u32()?, r.str()?, r.i64()?, r.f64()?);
            for i in 0..n {
                p.counters.push(Counter {
                    process: req(pr, i, name, "process")?,
                    name: req(nm, i, name, "name")?,
                    t_ns: req(ts, i, name, "t_ns")?,
                    value: req(va, i, name, "value")?,
                });
            }
        }
        "cpu_samples" => {
            let (pr, th, ts, st, sp, on, w) =
                (r.u32()?, r.u64()?, r.i64()?, r.u32()?, r.u64()?, r.bool()?, r.u64()?);
            for i in 0..n {
                p.cpu_samples.push(CpuSample {
                    process: req(pr, i, name, "process")?,
                    thread: req(th, i, name, "thread")?,
                    t_ns: req(ts, i, name, "t_ns")?,
                    stack: req(st, i, name, "stack")?,
                    span_id: opt(sp, i),
                    on_cpu: req(on, i, name, "on_cpu")?,
                    weight_ns: req(w, i, name, "weight_ns")?,
                });
            }
        }
        "frames" => {
            let (fu, sy, fi, li, co, sl, mo, bi, ad, inl, ki) = (
                r.str()?,
                r.str()?,
                r.str()?,
                r.u32()?,
                r.u32()?,
                r.u32()?,
                r.str()?,
                r.str()?,
                r.u64()?,
                r.u16()?,
                r.str()?,
            );
            for i in 0..n {
                let kind: String = req(ki, i, name, "kind")?;
                p.frames.push(Frame {
                    function: req(fu, i, name, "function")?,
                    system_name: opt(sy, i),
                    file: opt(fi, i),
                    line: opt(li, i),
                    column: opt(co, i),
                    start_line: opt(sl, i),
                    module: opt(mo, i),
                    build_id: opt(bi, i),
                    address: opt(ad, i),
                    inline_depth: req(inl, i, name, "inline_depth")?,
                    kind: FrameKind::parse(&kind).ok_or_else(|| Error::Table {
                        table: name.to_owned(),
                        why: format!("row {i}: kind `{kind}` is not native, span, wait or alloc"),
                    })?,
                });
            }
        }
        "spans" => p.spans.extend(read_spans(t)?),
        "stacks" => {
            let (pa, fr, de) = (r.u32()?, r.u32()?, r.u16()?);
            for i in 0..n {
                p.stacks.push(StackNode {
                    parent: opt(pa, i),
                    frame: req(fr, i, name, "frame")?,
                    depth: req(de, i, name, "depth")?,
                });
            }
        }
        "threads" => {
            let (pr, pn, th, tn, cpu) = (r.u32()?, r.str()?, r.u64()?, r.str()?, r.u64()?);
            for i in 0..n {
                p.threads.push(Thread {
                    process: req(pr, i, name, "process")?,
                    process_name: req(pn, i, name, "process_name")?,
                    thread: req(th, i, name, "thread")?,
                    thread_name: opt(tn, i),
                    cpu_ns: opt(cpu, i),
                });
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn read_spans(t: &Table) -> Result<Vec<Span>, Error> {
    let name = t.name;
    let mut r = Reader { table: t, next: 0 };
    let (pr, th, id, pa, de, nm, tg, ki, tk, fi, li) = (
        r.u32()?,
        r.u64()?,
        r.u64()?,
        r.u64()?,
        r.u16()?,
        r.str()?,
        r.str()?,
        r.str()?,
        r.u64()?,
        r.str()?,
        r.u32()?,
    );
    let (st, du, bu, sc, ab, al, fb, wa, wo, ow, fl) = (
        r.i64()?,
        r.i64()?,
        r.i64()?,
        r.i64()?,
        r.u64()?,
        r.u64()?,
        r.u64()?,
        r.str()?,
        r.str()?,
        r.str()?,
        r.str()?,
    );
    let mut out = Vec::with_capacity(t.rows());
    for i in 0..t.rows() {
        let kind: String = req(ki, i, name, "kind")?;
        out.push(Span {
            process: req(pr, i, name, "process")?,
            thread: req(th, i, name, "thread")?,
            span_id: req(id, i, name, "span_id")?,
            parent: opt(pa, i),
            depth: req(de, i, name, "depth")?,
            name: req(nm, i, name, "name")?,
            target: req(tg, i, name, "target")?,
            kind: SpanKind::parse(&kind).ok_or_else(|| Error::Table {
                table: name.to_owned(),
                why: format!("row {i}: kind `{kind}` is not span, fn, wait or poll"),
            })?,
            task: opt(tk, i),
            file: opt(fi, i),
            line: opt(li, i),
            start_ns: req(st, i, name, "start_ns")?,
            dur_ns: req(du, i, name, "dur_ns")?,
            busy_ns: opt(bu, i),
            self_cpu_ns: opt(sc, i),
            alloc_bytes: opt(ab, i),
            allocs: opt(al, i),
            frees_bytes: opt(fb, i),
            wait: opt(wa, i),
            wait_on: opt(wo, i),
            wait_owner: opt(ow, i),
            fields: opt(fl, i),
        });
    }
    Ok(out)
}

pub(crate) fn meta_json(m: &Meta) -> Json {
    let labels = Json::Obj(m.labels.iter().map(|(k, v)| (k.clone(), json::str(v))).collect());
    json::obj([
        ("program", json::str(&m.program)),
        ("producer", json::str(&m.producer)),
        ("clock", json::str(&m.clock)),
        ("start_ns", Json::Int(i128::from(m.start_ns))),
        ("end_ns", Json::Int(i128::from(m.end_ns))),
        ("start_unix_ns", Json::Int(i128::from(m.start_unix_ns))),
        ("arch", json::str(&m.arch)),
        ("os", json::str(&m.os)),
        ("cpu_period_ns", json::opt_u64(m.cpu_period_ns)),
        ("cpu_sampler", json::str(&m.cpu_sampler)),
        ("alloc_sample_bytes", json::opt_u64(m.alloc_sample_bytes)),
        ("alloc_counter", json::str(&m.alloc_counter)),
        ("symbolized", Json::Bool(m.symbolized)),
        ("labels", labels),
    ])
}

pub(crate) fn meta_from(j: &Json) -> Result<Meta, Error> {
    let s = |k: &str| j.get(k).and_then(Json::as_str).unwrap_or_default().to_owned();
    let i = |k: &str| {
        j.get(k)
            .and_then(Json::as_i64)
            .ok_or_else(|| Error::Manifest(format!("meta.{k} is not an integer")))
    };
    let labels = match j.get("labels") {
        Some(Json::Obj(map)) => map
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
            .collect(),
        _ => BTreeMap::new(),
    };
    Ok(Meta {
        program: s("program"),
        producer: s("producer"),
        clock: s("clock"),
        start_ns: i("start_ns")?,
        end_ns: i("end_ns")?,
        start_unix_ns: i("start_unix_ns")?,
        arch: s("arch"),
        os: s("os"),
        cpu_period_ns: j.get("cpu_period_ns").and_then(Json::as_u64),
        cpu_sampler: s("cpu_sampler"),
        alloc_sample_bytes: j.get("alloc_sample_bytes").and_then(Json::as_u64),
        alloc_counter: s("alloc_counter"),
        symbolized: j.get("symbolized").and_then(Json::as_bool).unwrap_or(false),
        labels,
    })
}

fn pad8(out: &mut Vec<u8>) {
    while out.len() % 8 != 0 {
        out.push(0);
    }
}

impl Profile {
    /// The profile as `.ixprof` bytes, canonical: [`Profile::canonicalize`] first.
    pub fn to_ixprof(&self) -> Result<Vec<u8>, Error> {
        let mut p = self.clone();
        p.canonicalize();
        let tables = tables(&p);
        let sections: Vec<(&'static str, usize, Vec<u8>)> = tables
            .iter()
            .map(|t| Ok((t.name, t.rows(), t.ipc_file()?)))
            .collect::<Result<_, Error>>()?;
        let hashes: Vec<blake3::Hash> = sections.iter().map(|it| blake3::hash(&it.2)).collect();
        let meta = meta_json(&p.meta);
        let mut id = blake3::Hasher::new();
        id.update(meta.to_text().as_bytes());
        for hash in &hashes {
            id.update(hash.as_bytes());
        }
        let id = id.finalize().to_hex().to_string();
        // The offsets depend on the manifest's length, which depends on the offsets' digits:
        // lay out until it settles (twice at most in practice; bounded at 8).
        let mut base = 0usize;
        for _ in 0..8 {
            let manifest = manifest(&id, &meta, &sections, &hashes, base);
            let start = (12 + manifest.len()).next_multiple_of(8);
            if start == base {
                let mut out = Vec::with_capacity(
                    start + sections.iter().map(|it| it.2.len() + 8).sum::<usize>(),
                );
                out.extend_from_slice(&MAGIC);
                out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
                out.extend_from_slice(manifest.as_bytes());
                pad8(&mut out);
                for (_, _, bytes) in &sections {
                    out.extend_from_slice(bytes);
                    pad8(&mut out);
                }
                return Ok(out);
            }
            base = start;
        }
        Err(Error::Manifest("the manifest's layout did not settle in 8 rounds".to_owned()))
    }

    /// Reads `.ixprof` bytes (or `.ixprof.zst`), checking each table's hash.
    pub fn from_ixprof(bytes: &[u8]) -> Result<Profile, Error> {
        if bytes.starts_with(&ZSTD_MAGIC) {
            return Err(Error::NotIxprof(
                "zstd-wrapped; decompress it first (`zstd -d`), this reader has no zstd",
            ));
        }
        if bytes.len() < 12 || bytes[..8] != MAGIC {
            return Err(Error::NotIxprof("no IXPROF magic in its first 8 bytes"));
        }
        let len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let text = bytes
            .get(12..12 + len)
            .ok_or(Error::NotIxprof("the manifest runs past the end of the file"))?;
        let value: serde_json::Value =
            serde_json::from_slice(text).map_err(|it| Error::Manifest(it.to_string()))?;
        let manifest = Json::from_value(&value);
        if manifest.get("format").and_then(Json::as_str) != Some("ixprof") {
            return Err(Error::Manifest("`format` is not \"ixprof\"".to_owned()));
        }
        let version = manifest.get("format_version").and_then(Json::as_u64).unwrap_or(0);
        if version > u64::from(FORMAT_VERSION) {
            return Err(Error::Version(version));
        }
        let meta =
            meta_from(manifest.get("meta").ok_or_else(|| Error::Manifest("no meta".to_owned()))?)?;
        let mut p = Profile { meta, ..Profile::default() };
        let Some(Json::Arr(list)) = manifest.get("tables") else {
            return Err(Error::Manifest("no tables list".to_owned()));
        };
        for entry in list {
            let table = entry.get("name").and_then(Json::as_str).unwrap_or_default();
            let Some(&(name, _)) = TABLES.iter().find(|it| it.0 == table) else {
                // A table this reader does not know: newer, additive; left out.
                continue;
            };
            let err = |why: &str| Error::Table { table: name.to_owned(), why: why.to_owned() };
            let offset =
                entry.get("offset").and_then(Json::as_u64).ok_or_else(|| err("no offset"))?
                    as usize;
            let len =
                entry.get("len").and_then(Json::as_u64).ok_or_else(|| err("no len"))? as usize;
            let section = bytes
                .get(offset..offset.saturating_add(len))
                .ok_or_else(|| err("its section runs past the end of the file"))?;
            let want = entry.get("blake3").and_then(Json::as_str).unwrap_or_default();
            if blake3::hash(section).to_hex().as_str() != want {
                return Err(Error::Hash { table: name.to_owned() });
            }
            rows_into(&mut p, &Table::read_ipc_file(name, section)?)?;
        }
        Ok(p)
    }
}

fn manifest(
    id: &str,
    meta: &Json,
    sections: &[(&'static str, usize, Vec<u8>)],
    hashes: &[blake3::Hash],
    base: usize,
) -> String {
    let mut at = base;
    let tables = sections
        .iter()
        .zip(hashes)
        .map(|((name, rows, bytes), hash)| {
            let entry = json::obj([
                ("name", json::str(name)),
                ("offset", Json::Int(at as i128)),
                ("len", Json::Int(bytes.len() as i128)),
                ("rows", Json::Int(*rows as i128)),
                ("blake3", json::str(hash.to_hex().as_str())),
            ]);
            at = (at + bytes.len()).next_multiple_of(8);
            entry
        })
        .collect();
    json::obj([
        ("format", json::str("ixprof")),
        ("format_version", Json::Int(i128::from(FORMAT_VERSION))),
        ("id", json::str(id)),
        ("meta", meta.clone()),
        ("tables", Json::Arr(tables)),
    ])
    .to_text()
}

/// The manifest of `.ixprof` bytes, as canonical JSON text (`blaze pprof --http` serves it).
pub fn manifest_text(bytes: &[u8]) -> Result<String, Error> {
    if bytes.len() < 12 || bytes[..8] != MAGIC {
        return Err(Error::NotIxprof("no IXPROF magic in its first 8 bytes"));
    }
    let len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let text = bytes.get(12..12 + len).ok_or(Error::NotIxprof("the manifest runs past the end"))?;
    String::from_utf8(text.to_vec()).map_err(|_| Error::Manifest("not UTF-8".to_owned()))
}
