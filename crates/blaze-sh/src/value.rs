//! Structured runtime values of `blaze-sh`: every composite value shares structure via
//! [`ARc`], [`PVec`], and [`PMap`] so cloning or passing a large table down a pipeline is O(1).

use core::cmp::Ordering;
use core::fmt::{self, Write as _};

use blaze_own::{ARc, AString, AVec, Alloc, Rc, Share};

use crate::ast::Block;
use crate::error::{ErrorKind, ShellError};
use crate::persist::{PMap, PVec};
use crate::span::Span;

/// Newtype for byte sizes (`10kb`, `3MiB`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Bytes(pub i64);

/// Newtype for durations in nanoseconds (`5sec`, `200ms`, `2hr`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Nanos(pub i64);

/// UTC timestamp in nanoseconds since Unix epoch plus display timezone offset in seconds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct DateTime {
    pub utc_ns: i64,
    pub offset_secs: i32,
}

impl PartialOrd for DateTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DateTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.utc_ns.cmp(&other.utc_ns)
    }
}

/// Numeric range (`1..10`, `1..<10`, `1..2..10`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RangeVal {
    pub start: i64,
    pub step: i64,
    pub end: Option<i64>,
    pub inclusive: bool,
}

impl RangeVal {
    /// Materialize bounded range into at most `cap` integers (rule 18).
    pub fn collect_bounded<'a>(&self, cap: usize, alloc: Alloc<'a>) -> Result<PVec<'a, Value<'a>>, ShellError<'a>> {
        let Some(end) = self.end else {
            return Err(ShellError::new(
                ErrorKind::Budget,
                Span::ZERO,
                "cannot materialize unbounded range without `take`",
                alloc,
            ));
        };
        if self.step == 0 {
            return Err(ShellError::new(ErrorKind::Type, Span::ZERO, "range step cannot be 0", alloc));
        }
        let mut out = PVec::new(alloc);
        let mut cur = self.start;
        let mut count = 0usize;
        loop {
            let in_bounds = if self.step > 0 {
                if self.inclusive { cur <= end } else { cur < end }
            } else if self.inclusive {
                cur >= end
            } else {
                cur > end
            };
            if !in_bounds {
                break;
            }
            if count >= cap {
                return Err(ShellError::new(
                    ErrorKind::Budget,
                    Span::ZERO,
                    "range exceeds element budget",
                    alloc,
                ));
            }
            out.push(Value::Int(cur));
            count += 1;
            match cur.checked_add(self.step) {
                Some(next) => cur = next,
                None => break,
            }
        }
        Ok(out)
    }
}

/// Ordered persistent record (preserves key order, O(1) fork/clone, O(log_32 N) lookup).
#[derive(Clone, Debug)]
pub struct Record<'a> {
    pub keys: PVec<'a, ARc<'a, str>>,
    pub values: PVec<'a, Value<'a>>,
    pub index: PMap<'a, ARc<'a, str>, usize>,
}

impl<'a> Share for Record<'a> {}

impl<'a> Record<'a> {
    pub fn new(alloc: Alloc<'a>) -> Record<'a> {
        Record {
            keys: PVec::new(alloc),
            values: PVec::new(alloc),
            index: PMap::new(alloc),
        }
    }

    #[inline]
    pub fn alloc(&self) -> Alloc<'a> {
        self.keys.alloc()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        let &idx = self.index.get(key)?;
        self.values.get(idx)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    /// Insert or update `key` in place (CoW). Preserves existing column order on update.
    pub fn insert(&mut self, key: ARc<'a, str>, val: Value<'a>) {
        if let Some(&idx) = self.index.get(key.as_ref()) {
            self.values.set(idx, val);
        } else {
            let idx = self.keys.len();
            self.keys.push(key.share());
            self.values.push(val);
            self.index.insert(key, idx);
        }
    }

    pub fn insert_str(&mut self, key: &str, val: Value<'a>) {
        let k = Rc::from_str_in(key, self.alloc());
        self.insert(k, val);
    }

    /// Return a new record with `key` removed (preserves order of remaining keys).
    pub fn without(&self, key: &str) -> Record<'a> {
        if !self.contains_key(key) {
            return self.share();
        }
        let mut out = Record::new(self.alloc());
        for i in 0..self.len() {
            let k = self.keys.get(i).unwrap();
            if k.as_ref() != key {
                out.insert(k.share(), self.values.get(i).unwrap().share());
            }
        }
        out
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value<'a>)> {
        (0..self.len()).map(|i| {
            (
                self.keys.get(i).unwrap().as_ref(),
                self.values.get(i).unwrap(),
            )
        })
    }
}

impl PartialEq for Record<'_> {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        for i in 0..self.len() {
            if self.keys.get(i) != other.keys.get(i) || self.values.get(i) != other.values.get(i) {
                return false;
            }
        }
        true
    }
}

/// Semantic display type for a dataframe [`Table`] column so renderers draw
/// rich custom representations (file/folder icons, proportional size bars, human
/// relative timestamps, process status dots, crate badges) without losing raw values.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ColDisplay {
    #[default]
    Auto,
    /// File or directory entry (renders with file-type / folder icon).
    FileEntry,
    /// Byte size with proportional bar.
    FileSize,
    /// Duration column.
    Duration,
    /// Timestamp rendered as human relative time (`3m ago`, `2h ago`, `Nov 24`).
    HumanTime,
    /// Live status / boolean indicator dot.
    Status,
    /// OS process name.
    Process,
    /// Percentage with proportional bar.
    Percent,
    /// Rust crate / package name.
    Crate,
}

/// Columnar persistent table: a list of records sharing column headers, stored column-wise
/// so `select`, `reject`, `sort-by`, `where`, and rendering touch only needed columns and
/// O(1) cloning shares every column vector.
#[derive(Clone, Debug)]
pub struct Table<'a> {
    pub columns: PVec<'a, ARc<'a, str>>,
    pub cols: PVec<'a, PVec<'a, Value<'a>>>,
    pub col_display: PVec<'a, ColDisplay>,
    pub nrows: usize,
    /// When true (e.g. plain `ls`), renderers default to a dense multi-column `eza`-style
    /// icon grid; `ls -l` or piped transforms (`sort-by`, `where`, `select`) show the full table.
    pub default_grid: bool,
}

impl<'a> Share for Table<'a> {}

impl<'a> Table<'a> {
    pub fn new(columns: PVec<'a, ARc<'a, str>>, alloc: Alloc<'a>) -> Table<'a> {
        let mut cols = PVec::new(alloc);
        let mut col_display = PVec::new(alloc);
        for _ in 0..columns.len() {
            cols.push(PVec::new(alloc));
            col_display.push(ColDisplay::Auto);
        }
        Table { columns, cols, col_display, nrows: 0, default_grid: false }
    }

    /// Create an empty table with identical columns and [`ColDisplay`] metadata.
    pub fn empty_like(&self) -> Table<'a> {
        let alloc = self.alloc();
        let mut cols = PVec::new(alloc);
        for _ in 0..self.columns.len() {
            cols.push(PVec::new(alloc));
        }
        Table {
            columns: self.columns.share(),
            cols,
            col_display: self.col_display.share(),
            nrows: 0,
            default_grid: false,
        }
    }

    /// Tag column `name` with a custom [`ColDisplay`] type.
    pub fn set_col_display(&mut self, name: &str, display: ColDisplay) {
        if let Some(ci) = self.col_index(name) {
            self.col_display.set(ci, display);
        }
    }

    #[inline]
    pub fn col_display(&self, col: usize) -> ColDisplay {
        self.col_display.get(col).copied().unwrap_or(ColDisplay::Auto)
    }

    pub fn from_col_names(names: &[&str], alloc: Alloc<'a>) -> Table<'a> {
        let mut columns = PVec::new(alloc);
        for &n in names {
            columns.push(Rc::from_str_in(n, alloc));
        }
        Table::new(columns, alloc)
    }

    #[inline]
    pub fn alloc(&self) -> Alloc<'a> {
        self.columns.alloc()
    }

    #[inline]
    pub fn ncols(&self) -> usize {
        self.columns.len()
    }

    #[inline]
    pub fn nrows(&self) -> usize {
        self.nrows
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nrows == 0
    }

    pub fn col_index(&self, name: &str) -> Option<usize> {
        for i in 0..self.columns.len() {
            if self.columns.get(i).map(|s| s.as_ref()) == Some(name) {
                return Some(i);
            }
        }
        None
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&Value<'a>> {
        self.cols.get(col)?.get(row)
    }

    pub fn get_by_name(&self, row: usize, col_name: &str) -> Option<&Value<'a>> {
        let ci = self.col_index(col_name)?;
        self.cell(row, ci)
    }

    /// Push a row of values aligned with `self.columns` (missing cells padded with `Nothing`).
    pub fn push_row_values(&mut self, row: &[Value<'a>]) {
        let ncols = self.columns.len();
        for c in 0..ncols {
            let val = row.get(c).map(Share::share).unwrap_or(Value::Nothing);
            let mut col = self.cols.get(c).unwrap().share();
            col.push(val);
            self.cols.set(c, col);
        }
        self.nrows += 1;
    }

    /// Push a record as a row, extending columns if the record introduces new keys.
    pub fn push_record(&mut self, rec: &Record<'a>) {
        let alloc = self.alloc();
        // Ensure any new keys from `rec` get a column padded with `Nothing` for prior rows.
        for (k, _) in rec.iter() {
            if self.col_index(k).is_none() {
                let mut new_col = PVec::new(alloc);
                for _ in 0..self.nrows {
                    new_col.push(Value::Nothing);
                }
                self.columns.push(Rc::from_str_in(k, alloc));
                self.cols.push(new_col);
                self.col_display.push(ColDisplay::Auto);
            }
        }
        let ncols = self.columns.len();
        for c in 0..ncols {
            let col_name = self.columns.get(c).unwrap().as_ref();
            let val = rec.get(col_name).map(Share::share).unwrap_or(Value::Nothing);
            let mut col = self.cols.get(c).unwrap().share();
            col.push(val);
            self.cols.set(c, col);
        }
        self.nrows += 1;
    }

    /// Materialize row `row` as a [`Record`].
    pub fn row_record(&self, row: usize) -> Option<Record<'a>> {
        if row >= self.nrows {
            return None;
        }
        let mut rec = Record::new(self.alloc());
        for c in 0..self.columns.len() {
            let k = self.columns.get(c).unwrap().share();
            let v = self.cell(row, c).map(Share::share).unwrap_or(Value::Nothing);
            rec.insert(k, v);
        }
        Some(rec)
    }

    /// Project a subset of columns in O(k) (shares each column's persistent vector!).
    pub fn select_columns(&self, names: &[&str]) -> Result<Table<'a>, ShellError<'a>> {
        let alloc = self.alloc();
        let mut columns = PVec::new(alloc);
        let mut cols = PVec::new(alloc);
        let mut col_display = PVec::new(alloc);
        for &n in names {
            let Some(ci) = self.col_index(n) else {
                let mut msg = AString::from_str_in("table has no column `", alloc);
                msg.push_str(n);
                msg.push('`');
                return Err(ShellError::new(ErrorKind::MissingColumn, Span::ZERO, msg.as_str(), alloc));
            };
            columns.push(self.columns.get(ci).unwrap().share());
            cols.push(self.cols.get(ci).unwrap().share());
            col_display.push(self.col_display(ci));
        }
        Ok(Table { columns, cols, col_display, nrows: self.nrows, default_grid: false })
    }

    /// Drop named columns in O(ncols), sharing every surviving column's persistent vector.
    pub fn reject_columns(&self, names: &[&str]) -> Table<'a> {
        let alloc = self.alloc();
        let mut columns = PVec::new(alloc);
        let mut cols = PVec::new(alloc);
        let mut col_display = PVec::new(alloc);
        for c in 0..self.columns.len() {
            let cname = self.columns.get(c).unwrap();
            if !names.iter().any(|&drop| drop == cname.as_ref()) {
                columns.push(cname.share());
                cols.push(self.cols.get(c).unwrap().share());
                col_display.push(self.col_display(c));
            }
        }
        Table { columns, cols, col_display, nrows: self.nrows, default_grid: false }
    }

    /// Build a table from a list of records if all elements are records (or empty).
    pub fn try_from_list(list: &PVec<'a, Value<'a>>, alloc: Alloc<'a>) -> Option<Table<'a>> {
        if list.is_empty() {
            return None;
        }
        for i in 0..list.len() {
            if !matches!(list.get(i), Some(Value::Record(_))) {
                return None;
            }
        }
        let mut table = Table::new(PVec::new(alloc), alloc);
        for i in 0..list.len() {
            if let Some(Value::Record(r)) = list.get(i) {
                table.push_record(r);
            }
        }
        Some(table)
    }
}

impl PartialEq for Table<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.nrows == other.nrows && self.columns == other.columns && self.cols == other.cols
    }
}

/// One step in a runtime cell-path value (`$.a.0?.b`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathMember<'a> {
    Field { name: ARc<'a, str>, optional: bool },
    Index { index: usize, optional: bool },
}

/// Captured closure: holds parameter names, AST body, and an **O(1) forked snapshot**
/// of the lexical bindings in scope where the closure was created.
#[derive(Clone, Debug)]
pub struct ClosureData<'a> {
    pub params: AVec<'a, ARc<'a, str>>,
    pub body: ARc<'a, Block<'a>>,
    pub captured: PMap<'a, ARc<'a, str>, Value<'a>>,
    pub span: Span,
}

/// A first-class structured value flowing through `blaze-sh` pipelines.
/// Filesystem entry classification carried by a first-class [`Value::Path`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum PathKind {
    #[default]
    Unknown,
    Dir,
    File,
    Symlink,
}

#[derive(Clone, Debug)]
pub enum Value<'a> {
    Nothing,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(ARc<'a, str>),
    Path { path: ARc<'a, str>, kind: PathKind },
    Size(Bytes),
    Duration(Nanos),
    Date(DateTime),
    Range(RangeVal),
    Binary(ARc<'a, [u8]>),
    List(PVec<'a, Value<'a>>),
    Record(Record<'a>),
    Table(Table<'a>),
    Closure(ARc<'a, ClosureData<'a>>),
    CellPath(ARc<'a, [PathMember<'a>]>),
    Error(ARc<'a, ShellError<'a>>),
}

impl<'a> Share for Value<'a> {}

impl<'a> Value<'a> {
    pub fn string(s: &str, alloc: Alloc<'a>) -> Value<'a> {
        Value::String(Rc::from_str_in(s, alloc))
    }

    pub fn path(s: &str, kind: PathKind, alloc: Alloc<'a>) -> Value<'a> {
        Value::Path { path: Rc::from_str_in(s, alloc), kind }
    }

    pub fn binary(b: &[u8], alloc: Alloc<'a>) -> Value<'a> {
        Value::Binary(Rc::from_slice_in(b, alloc))
    }

    pub fn error(err: ShellError<'a>, alloc: Alloc<'a>) -> Value<'a> {
        Value::Error(Rc::new_in(err, alloc))
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nothing => "nothing",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Path { .. } => "path",
            Value::Size(_) => "size",
            Value::Duration(_) => "duration",
            Value::Date(_) => "date",
            Value::Range(_) => "range",
            Value::Binary(_) => "binary",
            Value::List(_) => "list",
            Value::Record(_) => "record",
            Value::Table(_) => "table",
            Value::Closure(_) => "closure",
            Value::CellPath(_) => "cell-path",
            Value::Error(_) => "error",
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Nothing | Value::Bool(false) | Value::Error(_) => false,
            Value::Bool(true) => true,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0 && !f.is_nan(),
            Value::String(s) => !s.is_empty(),
            Value::Path { path, .. } => !path.is_empty(),
            Value::Size(Bytes(n)) => *n != 0,
            Value::Duration(Nanos(n)) => *n != 0,
            Value::List(v) => !v.is_empty(),
            Value::Record(r) => !r.is_empty(),
            Value::Table(t) => !t.is_empty(),
            Value::Binary(b) => !b.is_empty(),
            Value::Date(_) | Value::Range(_) | Value::Closure(_) | Value::CellPath(_) => true,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_ref()),
            Value::Path { path, .. } => Some(path.as_ref()),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(n) => Some(*n as f64),
            _ => None,
        }
    }

    /// Follow a single path member (`.name` or `.0`), returning `Nothing` when optional.
    pub fn follow_member(&self, member: &PathMember<'a>, span: Span, alloc: Alloc<'a>) -> Result<Value<'a>, ShellError<'a>> {
        match member {
            PathMember::Field { name, optional } => match self {
                Value::Record(r) => match r.get(name.as_ref()) {
                    Some(v) => Ok(v.share()),
                    None if *optional => Ok(Value::Nothing),
                    None => {
                        let mut msg = AString::from_str_in("record has no field `", alloc);
                        msg.push_str(name.as_ref());
                        msg.push('`');
                        Err(ShellError::new(ErrorKind::MissingColumn, span, msg.as_str(), alloc))
                    }
                },
                Value::Table(t) => match t.col_index(name.as_ref()) {
                    Some(ci) => Ok(Value::List(t.cols.get(ci).unwrap().share())),
                    None if *optional => Ok(Value::Nothing),
                    None => {
                        let mut msg = AString::from_str_in("table has no column `", alloc);
                        msg.push_str(name.as_ref());
                        msg.push('`');
                        Err(ShellError::new(ErrorKind::MissingColumn, span, msg.as_str(), alloc))
                    }
                },
                Value::List(items) => {
                    // Column extraction across a list of records.
                    let mut out = PVec::new(alloc);
                    for i in 0..items.len() {
                        match items.get(i).unwrap() {
                            Value::Record(r) => match r.get(name.as_ref()) {
                                Some(v) => out.push(v.share()),
                                None if *optional => out.push(Value::Nothing),
                                None => {
                                    let mut msg = AString::from_str_in("record has no field `", alloc);
                                    msg.push_str(name.as_ref());
                                    msg.push('`');
                                    return Err(ShellError::new(ErrorKind::MissingColumn, span, msg.as_str(), alloc));
                                }
                            },
                            _ if *optional => out.push(Value::Nothing),
                            other => {
                                let mut msg = AString::from_str_in("cannot access field `", alloc);
                                msg.push_str(name.as_ref());
                                msg.push_str("` on ");
                                msg.push_str(other.type_name());
                                return Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc));
                            }
                        }
                    }
                    Ok(Value::List(out))
                }
                Value::Nothing if *optional => Ok(Value::Nothing),
                other => {
                    let mut msg = AString::from_str_in("cannot access field `", alloc);
                    msg.push_str(name.as_ref());
                    msg.push_str("` on ");
                    msg.push_str(other.type_name());
                    Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
                }
            },
            PathMember::Index { index, optional } => match self {
                Value::List(items) => match items.get(*index) {
                    Some(v) => Ok(v.share()),
                    None if *optional => Ok(Value::Nothing),
                    None => {
                        let mut msg = AString::new_in(alloc);
                        let _ = write!(msg, "index {} out of bounds for list of length {}", index, items.len());
                        Err(ShellError::new(ErrorKind::IndexOutOfBounds, span, msg.as_str(), alloc))
                    }
                },
                Value::Table(t) => match t.row_record(*index) {
                    Some(r) => Ok(Value::Record(r)),
                    None if *optional => Ok(Value::Nothing),
                    None => {
                        let mut msg = AString::new_in(alloc);
                        let _ = write!(msg, "row index {} out of bounds for table with {} rows", index, t.nrows());
                        Err(ShellError::new(ErrorKind::IndexOutOfBounds, span, msg.as_str(), alloc))
                    }
                },
                Value::Nothing if *optional => Ok(Value::Nothing),
                other => {
                    let mut msg = AString::new_in(alloc);
                    let _ = write!(msg, "cannot index {} by {}", other.type_name(), index);
                    Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
                }
            },
        }
    }

    /// Total ordering suitable for `sort-by` and comparison operators.
    pub fn compare(&self, other: &Value<'a>) -> Option<Ordering> {
        match (self, other) {
            (Value::Nothing, Value::Nothing) => Some(Ordering::Equal),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::String(a), Value::String(b)) => Some(a.as_ref().cmp(b.as_ref())),
            (Value::Path { path: a, .. }, Value::Path { path: b, .. }) => Some(a.as_ref().cmp(b.as_ref())),
            (Value::Path { path: a, .. }, Value::String(b)) => Some(a.as_ref().cmp(b.as_ref())),
            (Value::String(a), Value::Path { path: b, .. }) => Some(a.as_ref().cmp(b.as_ref())),
            (Value::Size(a), Value::Size(b)) => Some(a.cmp(b)),
            (Value::Duration(a), Value::Duration(b)) => Some(a.cmp(b)),
            (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
            (Value::Binary(a), Value::Binary(b)) => Some(a.as_ref().cmp(b.as_ref())),
            _ => None,
        }
    }

    /// Compact single-cell display string (for table cells, string interpolation, `to text`).
    pub fn format_cell(&self, out: &mut impl fmt::Write) -> fmt::Result {
        match self {
            Value::Nothing => Ok(()),
            Value::Bool(b) => write!(out, "{b}"),
            Value::Int(n) => write!(out, "{n}"),
            Value::Float(f) => write!(out, "{f}"),
            Value::String(s) => out.write_str(s.as_ref()),
            Value::Path { path, .. } => out.write_str(path.as_ref()),
            Value::Size(b) => format_size(*b, out),
            Value::Duration(d) => format_duration(*d, out),
            Value::Date(dt) => format_date(*dt, out),
            Value::Range(r) => {
                write!(out, "{}", r.start)?;
                if r.step != 1 {
                    write!(out, "..{}", r.start.saturating_add(r.step))?;
                }
                if r.inclusive {
                    out.write_str("..")?;
                } else {
                    out.write_str("..<")?;
                }
                if let Some(end) = r.end {
                    write!(out, "{end}")?;
                }
                Ok(())
            }
            Value::Binary(b) => write!(out, "<binary {} B>", b.len()),
            Value::List(l) => write!(out, "[list {} items]", l.len()),
            Value::Record(r) => write!(out, "{{record {} fields}}", r.len()),
            Value::Table(t) => write!(out, "[table {}x{}]", t.nrows(), t.ncols()),
            Value::Closure(_) => out.write_str("<closure>"),
            Value::CellPath(p) => {
                out.write_str("$")?;
                for m in p.iter() {
                    match m {
                        PathMember::Field { name, optional } => {
                            out.write_str(if *optional { "?." } else { "." })?;
                            out.write_str(name.as_ref())?;
                        }
                        PathMember::Index { index, optional } => {
                            out.write_str(if *optional { "?." } else { "." })?;
                            write!(out, "{index}")?;
                        }
                    }
                }
                Ok(())
            }
            Value::Error(e) => write!(out, "error[{}]: {}", e.kind.as_str(), e.message.as_str()),
        }
    }

    pub fn to_display_string(&self, alloc: Alloc<'a>) -> AString<'a> {
        let mut s = AString::new_in(alloc);
        let _ = self.format_cell(&mut s);
        s
    }

    /// Convert to `blaze_data::Value` for JSON/TOML serialization and Rust cell marshalling.
    pub fn to_blaze_data(&self, alloc: Alloc<'a>) -> blaze_data::Value<Alloc<'a>> {
        match self {
            Value::Nothing => blaze_data::Value::Null,
            Value::Bool(b) => blaze_data::Value::Bool(*b),
            Value::Int(n) => blaze_data::Value::Int(blaze_data::Int::from_i64(*n)),
            Value::Float(f) => blaze_data::Value::Float(*f),
            Value::String(s) => blaze_data::Value::Text(AString::from_str_in(s.as_ref(), alloc)),
            Value::Path { path, .. } => blaze_data::Value::Text(AString::from_str_in(path.as_ref(), alloc)),
            Value::Size(Bytes(n)) => blaze_data::Value::Int(blaze_data::Int::from_i64(*n)),
            Value::Duration(Nanos(n)) => blaze_data::Value::Int(blaze_data::Int::from_i64(*n)),
            Value::Date(dt) => {
                let mut s = AString::new_in(alloc);
                let _ = format_date(*dt, &mut s);
                blaze_data::Value::Text(s)
            }
            Value::Range(_) | Value::Closure(_) | Value::CellPath(_) => {
                blaze_data::Value::Text(self.to_display_string(alloc))
            }
            Value::Binary(b) => {
                let mut v = AVec::with_capacity_in(b.len(), alloc);
                v.extend_from_slice(b.as_ref());
                blaze_data::Value::Bytes(v)
            }
            Value::List(items) => {
                let mut out = AVec::with_capacity_in(items.len(), alloc);
                for i in 0..items.len() {
                    out.push(items.get(i).unwrap().to_blaze_data(alloc));
                }
                blaze_data::Value::List(out)
            }
            Value::Record(rec) => {
                let mut map = blaze_data::Map::new_in(alloc);
                for (k, v) in rec.iter() {
                    map.insert(AString::from_str_in(k, alloc), v.to_blaze_data(alloc));
                }
                blaze_data::Value::Map(map)
            }
            Value::Table(t) => {
                let mut out = AVec::with_capacity_in(t.nrows(), alloc);
                for r in 0..t.nrows() {
                    let mut map = blaze_data::Map::new_in(alloc);
                    for c in 0..t.ncols() {
                        let k = t.columns.get(c).unwrap().as_ref();
                        let v = t.cell(r, c).unwrap();
                        map.insert(AString::from_str_in(k, alloc), v.to_blaze_data(alloc));
                    }
                    out.push(blaze_data::Value::Map(map));
                }
                blaze_data::Value::List(out)
            }
            Value::Error(e) => {
                let mut map = blaze_data::Map::new_in(alloc);
                map.insert(
                    AString::from_str_in("kind", alloc),
                    blaze_data::Value::Text(AString::from_str_in(e.kind.as_str(), alloc)),
                );
                map.insert(
                    AString::from_str_in("message", alloc),
                    blaze_data::Value::Text(AString::from_str_in(e.message.as_str(), alloc)),
                );
                blaze_data::Value::Map(map)
            }
        }
    }

    /// Convert a `blaze_data::Value` (from `blaze-json` or `blaze-toml`) into a `blaze-sh` Value,
    /// automatically lifting uniform lists of maps into columnar [`Table`]s!
    pub fn from_blaze_data(v: &blaze_data::Value<Alloc<'a>>, alloc: Alloc<'a>) -> Value<'a> {
        match v {
            blaze_data::Value::Null => Value::Nothing,
            blaze_data::Value::Bool(b) => Value::Bool(*b),
            blaze_data::Value::Int(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_i128() as f64)
                }
            }
            blaze_data::Value::Float(f) => Value::Float(*f),
            blaze_data::Value::Text(s) => Value::string(s.as_str(), alloc),
            blaze_data::Value::Bytes(b) => Value::binary(b.as_slice(), alloc),
            blaze_data::Value::List(items) => {
                let mut list = PVec::new(alloc);
                let mut all_maps = !items.is_empty();
                for item in items.iter() {
                    let converted = Value::from_blaze_data(item, alloc);
                    if !matches!(converted, Value::Record(_)) {
                        all_maps = false;
                    }
                    list.push(converted);
                }
                if all_maps && let Some(table) = Table::try_from_list(&list, alloc) {
                    Value::Table(table)
                } else {
                    Value::List(list)
                }
            }
            blaze_data::Value::Map(map) => {
                let mut rec = Record::new(alloc);
                for (k, val) in map.iter() {
                    rec.insert_str(k, Value::from_blaze_data(val, alloc));
                }
                Value::Record(rec)
            }
        }
    }
}

impl PartialEq for Value<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Nothing, Value::Nothing) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
            (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
            (Value::String(a), Value::String(b)) => a.as_ref() == b.as_ref(),
            (Value::Path { path: a, .. }, Value::Path { path: b, .. }) => a.as_ref() == b.as_ref(),
            (Value::Path { path: a, .. }, Value::String(b)) => a.as_ref() == b.as_ref(),
            (Value::String(a), Value::Path { path: b, .. }) => a.as_ref() == b.as_ref(),
            (Value::Size(a), Value::Size(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Range(a), Value::Range(b)) => a == b,
            (Value::Binary(a), Value::Binary(b)) => a.as_ref() == b.as_ref(),
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Record(a), Value::Record(b)) => a == b,
            (Value::Table(a), Value::Table(b)) => a == b,
            (Value::CellPath(a), Value::CellPath(b)) => a.as_ref() == b.as_ref(),
            (Value::Error(a), Value::Error(b)) => a.as_ref() == b.as_ref(),
            _ => false,
        }
    }
}

/// Human-friendly byte size formatting (`512 B`, `14.2 KB`, `3.1 MB`, `1.4 GB`).
pub fn format_size(Bytes(b): Bytes, out: &mut impl fmt::Write) -> fmt::Result {
    let neg = b < 0;
    let abs = b.unsigned_abs();
    if neg {
        out.write_char('-')?;
    }
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000_000_000_000;
    if abs < KB {
        write!(out, "{abs} B")
    } else if abs < MB {
        write_scaled(abs, KB, "KB", out)
    } else if abs < GB {
        write_scaled(abs, MB, "MB", out)
    } else if abs < TB {
        write_scaled(abs, GB, "GB", out)
    } else {
        write_scaled(abs, TB, "TB", out)
    }
}

fn write_scaled(val: u64, unit: u64, suffix: &str, out: &mut impl fmt::Write) -> fmt::Result {
    let whole = val / unit;
    let frac = ((val % unit) * 10) / unit;
    if frac == 0 {
        write!(out, "{whole} {suffix}")
    } else {
        write!(out, "{whole}.{frac} {suffix}")
    }
}

/// Human-friendly duration formatting (`420ns`, `12.4ms`, `1.8sec`, `4min 12sec`).
pub fn format_duration(Nanos(ns): Nanos, out: &mut impl fmt::Write) -> fmt::Result {
    let neg = ns < 0;
    let abs = ns.unsigned_abs();
    if neg {
        out.write_char('-')?;
    }
    const US: u64 = 1_000;
    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000_000_000;
    const MIN: u64 = 60 * SEC;
    const HR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HR;
    if abs < US {
        write!(out, "{abs}ns")
    } else if abs < MS {
        write_scaled(abs, US, "us", out)
    } else if abs < SEC {
        write_scaled(abs, MS, "ms", out)
    } else if abs < MIN {
        write_scaled(abs, SEC, "sec", out)
    } else if abs < HR {
        let m = abs / MIN;
        let s = (abs % MIN) / SEC;
        if s == 0 { write!(out, "{m}min") } else { write!(out, "{m}min {s}sec") }
    } else if abs < DAY {
        let h = abs / HR;
        let m = (abs % HR) / MIN;
        if m == 0 { write!(out, "{h}hr") } else { write!(out, "{h}hr {m}min") }
    } else {
        let d = abs / DAY;
        let h = (abs % DAY) / HR;
        if h == 0 { write!(out, "{d}day") } else { write!(out, "{d}day {h}hr") }
    }
}

/// Format UTC nanoseconds + offset as ISO 8601 (`YYYY-MM-DD HH:MM:SS±HH:MM`, civil calendar).
pub fn format_date(dt: DateTime, out: &mut impl fmt::Write) -> fmt::Result {
    let local_secs = dt.utc_ns.div_euclid(1_000_000_000) + i64::from(dt.offset_secs);
    let days = local_secs.div_euclid(86_400);
    let sec_of_day = local_secs.rem_euclid(86_400);
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day % 3600) / 60;
    let second = sec_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    write!(out, "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")?;
    if dt.offset_secs == 0 {
        out.write_char('Z')
    } else {
        let sign = if dt.offset_secs < 0 { '-' } else { '+' };
        let abs = dt.offset_secs.unsigned_abs();
        write!(out, "{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
    }
}

/// Format a [`DateTime`] as concise human relative time against `now_ns` (UTC nanoseconds,
/// passed in explicitly — never reads an ambient clock).
pub fn format_relative_time(dt: DateTime, now_ns: i64, out: &mut impl fmt::Write) -> fmt::Result {
    let diff_secs = (now_ns.saturating_sub(dt.utc_ns)) / 1_000_000_000;
    if diff_secs < 0 {
        let ahead = diff_secs.unsigned_abs();
        return if ahead < 60 {
            write!(out, "in {ahead}s")
        } else if ahead < 3_600 {
            write!(out, "in {}m", ahead / 60)
        } else if ahead < 86_400 {
            write!(out, "in {}h", ahead / 3_600)
        } else {
            write!(out, "in {}d", ahead / 86_400)
        };
    }
    let s = diff_secs as u64;
    if s < 10 {
        out.write_str("just now")
    } else if s < 60 {
        write!(out, "{s}s ago")
    } else if s < 3_600 {
        write!(out, "{}m ago", s / 60)
    } else if s < 86_400 {
        write!(out, "{}h ago", s / 3_600)
    } else if s < 172_800 {
        out.write_str("yesterday")
    } else if s < 14 * 86_400 {
        write!(out, "{}d ago", s / 86_400)
    } else {
        let local_secs = dt.utc_ns.div_euclid(1_000_000_000) + i64::from(dt.offset_secs);
        let days = local_secs.div_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let now_local = now_ns.div_euclid(1_000_000_000) + i64::from(dt.offset_secs);
        let (now_year, _, _) = civil_from_days(now_local.div_euclid(86_400));
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let mon = MONTHS[(month.saturating_sub(1) as usize).min(11)];
        if year == now_year {
            write!(out, "{mon} {day}")
        } else {
            write!(out, "{mon} {day}, {year}")
        }
    }
}

/// Howard Hinnant's civil-from-days algorithm (exact for all proleptic Gregorian dates).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}
