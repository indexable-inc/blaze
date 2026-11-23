//! Zero-recursion iterative writer for `blaze-tab`.
//!
//! Serializes any [`blaze_data::Value`] losslessly into `blaze-tab` text using an explicit
//! allocator-backed stack (`Vec<WriteFrame, Alloc<'a>>`). Lists of maps automatically compress
//! into homogeneous or sparse union-schema tables (`[[col1 col2]; [v1 v2] [v3 _]]`) according
//! to [`TableMode`].

use core::alloc::Allocator;
use core::fmt;

use blaze_data::{Map, Step, Value, write_step};
use blaze_own::{Alloc, BTreeSet, String, Vec};

use crate::{Limits, is_bare_key, is_bare_value};

/// Whitespace layout for written `blaze-tab` output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    /// Minimal whitespace for maximum LLM token density: `{a:1 b:2}`, `[[k v];[a 1] [b 2]]`.
    Compact,
    /// Human-readable indented layout with one row per line in tables and `indent` spaces per level.
    Pretty { indent: u8 },
}

/// Deterministic ordering for map keys and table columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyOrder {
    /// Lexicographic by key UTF-8 bytes (canonical deterministic output regardless of build order).
    Sorted,
    /// Map insertion order (and first-seen order across rows for union-schema tables).
    Insertion,
}

/// Policy for automatic tabular compression of lists of maps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableMode {
    /// Emit a table for homogeneous lists of `>= 1` non-empty maps, AND for sparse-schema lists
    /// of `>= 2` maps with at least one shared key whose average key coverage across rows is at
    /// least `min_coverage_pct` (0..=100, default 50), emitting `_` for absent keys.
    Auto { min_coverage_pct: u8 },
    /// Emit a table only when every row map has the exact same set of keys (NUON-compatible).
    HomogeneousOnly,
    /// Never emit tables; always write lists of maps as `[{...} {...}]`.
    Off,
}

/// Configuration for [`write_with`] and [`write_into`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriteOptions {
    pub style: Style,
    pub keys: KeyOrder,
    pub tables: TableMode,
    /// Maximum container nesting depth permitted while writing ([`WriteWhat::TooDeep`] past it).
    pub most_depth: usize,
}

impl WriteOptions {
    /// Compact output, sorted keys, automatic homogeneous + sparse tables (>= 50% coverage).
    pub const COMPACT: WriteOptions = WriteOptions {
        style: Style::Compact,
        keys: KeyOrder::Sorted,
        tables: TableMode::Auto { min_coverage_pct: 50 },
        most_depth: Limits::MOST_DEPTH,
    };

    /// Pretty 2-space indented output, sorted keys, automatic homogeneous + sparse tables (>= 50%).
    pub const PRETTY: WriteOptions = WriteOptions {
        style: Style::Pretty { indent: 2 },
        keys: KeyOrder::Sorted,
        tables: TableMode::Auto { min_coverage_pct: 50 },
        most_depth: Limits::MOST_DEPTH,
    };
}

/// Why a write failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteWhat {
    /// Value nesting exceeded [`WriteOptions::most_depth`].
    TooDeep { most: usize },
}

/// Error returned while writing a [`Value`] to `blaze-tab`, naming the structural path and reason.
pub struct WriteError<A: Allocator> {
    pub path: String<A>,
    pub what: WriteWhat,
}

impl<A: Allocator> PartialEq for WriteError<A> {
    fn eq(&self, other: &Self) -> bool {
        self.path.as_str() == other.path.as_str() && self.what == other.what
    }
}

impl<A: Allocator> Eq for WriteError<A> {}

impl<A: Allocator> fmt::Display for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = match self.path.is_empty() {
            true => "the value",
            false => self.path.as_str(),
        };
        match self.what {
            WriteWhat::TooDeep { most } => write!(f, "{path}: nesting exceeds write limit of {most} containers"),
        }
    }
}

impl<A: Allocator> fmt::Debug for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WriteError({self})")
    }
}

impl<A: Allocator> core::error::Error for WriteError<A> {}

/// Writes `value` as compact `blaze-tab` text in `alloc` ([`WriteOptions::COMPACT`]).
pub fn write<'a, A: Allocator>(value: &Value<A>, alloc: Alloc<'a>) -> Result<String<Alloc<'a>>, WriteError<Alloc<'a>>> {
    write_with(value, WriteOptions::COMPACT, alloc)
}

/// Writes `value` as pretty-indented `blaze-tab` text in `alloc` ([`WriteOptions::PRETTY`]).
pub fn write_pretty<'a, A: Allocator>(
    value: &Value<A>,
    alloc: Alloc<'a>,
) -> Result<String<Alloc<'a>>, WriteError<Alloc<'a>>> {
    write_with(value, WriteOptions::PRETTY, alloc)
}

/// Writes `value` as `blaze-tab` text in `alloc` with explicit `options`.
pub fn write_with<'a, A: Allocator>(
    value: &Value<A>,
    options: WriteOptions,
    alloc: Alloc<'a>,
) -> Result<String<Alloc<'a>>, WriteError<Alloc<'a>>> {
    let mut out = String::new_in(alloc);
    write_into(value, &mut out, options)?;
    Ok(out)
}

enum WriteFrame<'v, 'a, A: Allocator> {
    List {
        items: &'v [Value<A>],
        next: usize,
    },
    Map {
        map: &'v Map<A>,
        next: usize,
    },
    Table {
        items: &'v [Value<A>],
        cols: Vec<&'v str, Alloc<'a>>,
        row: usize,
        col: usize,
        in_row: bool,
    },
}

/// Appends `value` formatted as `blaze-tab` text to `out` without recursion.
pub fn write_into<'a, A: Allocator>(
    value: &Value<A>,
    out: &mut String<Alloc<'a>>,
    options: WriteOptions,
) -> Result<(), WriteError<Alloc<'a>>> {
    let alloc = *out.allocator();
    let mut stack: Vec<WriteFrame<'_, 'a, A>, Alloc<'a>> = Vec::new_in(alloc);
    let mut pending: Option<&Value<A>> = Some(value);
    let mut depth: usize = 0;

    loop {
        if let Some(current) = pending.take() {
            match current {
                Value::Null => out.push_str("null"),
                Value::Bool(true) => out.push_str("true"),
                Value::Bool(false) => out.push_str("false"),
                Value::Int(n) => blaze_own::format_into!(out, "{n}"),
                Value::Float(x) => write_float(*x, out),
                Value::Text(text) => write_text_value(text.as_str(), out),
                Value::Bytes(bytes) => write_bytes(bytes.as_slice(), out),
                Value::List(list) => {
                    let items = list.as_slice();
                    if items.is_empty() {
                        out.push_str("[]");
                    } else if let Some(cols) = detect_table(items, options, alloc) {
                        if depth.saturating_add(2) > options.most_depth {
                            return Err(WriteError {
                                path: format_write_path(&stack, options.keys, alloc),
                                what: WriteWhat::TooDeep { most: options.most_depth },
                            });
                        }
                        out.push_str("[[");
                        for (i, &col) in cols.iter().enumerate() {
                            if i > 0 {
                                out.push(' ');
                            }
                            write_key(col, out);
                        }
                        out.push_str("];");
                        stack.push(WriteFrame::Table { items, cols, row: 0, col: 0, in_row: false });
                        depth += 1;
                    } else {
                        if depth.saturating_add(1) > options.most_depth {
                            return Err(WriteError {
                                path: format_write_path(&stack, options.keys, alloc),
                                what: WriteWhat::TooDeep { most: options.most_depth },
                            });
                        }
                        out.push('[');
                        stack.push(WriteFrame::List { items, next: 0 });
                        depth += 1;
                    }
                }
                Value::Map(map) => {
                    if map.is_empty() {
                        out.push_str("{}");
                    } else {
                        if depth.saturating_add(1) > options.most_depth {
                            return Err(WriteError {
                                path: format_write_path(&stack, options.keys, alloc),
                                what: WriteWhat::TooDeep { most: options.most_depth },
                            });
                        }
                        out.push('{');
                        stack.push(WriteFrame::Map { map, next: 0 });
                        depth += 1;
                    }
                }
            }
            continue;
        }

        let Some(frame) = stack.last_mut() else {
            return Ok(());
        };

        match frame {
            WriteFrame::List { items, next } => {
                let idx = *next;
                if let Some(item) = items.get(idx) {
                    *next += 1;
                    match options.style {
                        Style::Compact => {
                            if idx > 0 {
                                out.push(' ');
                            }
                        }
                        Style::Pretty { indent } => write_indent(out, indent, depth),
                    }
                    pending = Some(item);
                } else {
                    depth = depth.saturating_sub(1);
                    let _ = stack.pop();
                    if let Style::Pretty { indent } = options.style {
                        write_indent(out, indent, depth);
                    }
                    out.push(']');
                }
            }
            WriteFrame::Map { map, next } => {
                let idx = *next;
                let entry = match options.keys {
                    KeyOrder::Sorted => map.sorted_at(idx),
                    KeyOrder::Insertion => map.entry_at(idx),
                };
                if let Some((key, val)) = entry {
                    *next += 1;
                    match options.style {
                        Style::Compact => {
                            if idx > 0 {
                                out.push(' ');
                            }
                        }
                        Style::Pretty { indent } => write_indent(out, indent, depth),
                    }
                    write_key(key, out);
                    out.push(':');
                    if matches!(options.style, Style::Pretty { .. }) {
                        out.push(' ');
                    }
                    pending = Some(val);
                } else {
                    depth = depth.saturating_sub(1);
                    let _ = stack.pop();
                    if let Style::Pretty { indent } = options.style {
                        write_indent(out, indent, depth);
                    }
                    out.push('}');
                }
            }
            WriteFrame::Table { items, cols, row, col, in_row } => {
                if !*in_row {
                    let row_idx = *row;
                    if row_idx < items.len() {
                        match options.style {
                            Style::Compact => {
                                if row_idx > 0 {
                                    out.push(' ');
                                }
                            }
                            Style::Pretty { indent } => write_indent(out, indent, depth),
                        }
                        out.push('[');
                        *in_row = true;
                        *col = 0;
                        depth += 1;
                    } else {
                        depth = depth.saturating_sub(1);
                        let _ = stack.pop();
                        out.push(']');
                    }
                } else {
                    let col_idx = *col;
                    if let Some(&col_name) = cols.get(col_idx) {
                        *col += 1;
                        if col_idx > 0 {
                            out.push(' ');
                        }
                        match items.get(*row).and_then(Value::as_map).and_then(|m| m.get(col_name)) {
                            Some(cell) => pending = Some(cell),
                            None => out.push('_'),
                        }
                    } else {
                        out.push(']');
                        *in_row = false;
                        *row += 1;
                        depth = depth.saturating_sub(1);
                    }
                }
            }
        }
    }
}

fn detect_table<'v, 'a, A: Allocator>(
    items: &'v [Value<A>],
    options: WriteOptions,
    alloc: Alloc<'a>,
) -> Option<Vec<&'v str, Alloc<'a>>> {
    let min_coverage_pct = match options.tables {
        TableMode::Off => return None,
        TableMode::HomogeneousOnly => 100u8,
        TableMode::Auto { min_coverage_pct } => min_coverage_pct.min(100),
    };
    if items.is_empty() {
        return None;
    }

    let mut seen: BTreeSet<&'v str, Alloc<'a>> = BTreeSet::new_in(alloc);
    let mut cols_insertion: Vec<&'v str, Alloc<'a>> = Vec::new_in(alloc);
    let mut total_present: usize = 0;

    for item in items {
        let map = item.as_map()?;
        total_present = total_present.checked_add(map.len())?;
        match options.keys {
            KeyOrder::Sorted => {
                for (k, _) in map.iter() {
                    let _ = seen.insert(k);
                }
            }
            KeyOrder::Insertion => {
                for (k, _) in map.iter() {
                    if seen.insert(k) {
                        cols_insertion.push(k);
                    }
                }
            }
        }
    }

    let col_count = seen.len();
    if col_count == 0 {
        return None;
    }

    let total_cells = items.len().checked_mul(col_count)?;
    let is_homogeneous = total_present == total_cells;

    let qualify = match options.tables {
        TableMode::Off => false,
        TableMode::HomogeneousOnly => is_homogeneous,
        TableMode::Auto { .. } => {
            if items.len() == 1 {
                true
            } else {
                // Require at least one shared key across rows (`total_present > col_count` by
                // pigeonhole) and coverage >= `min_coverage_pct`.
                total_present > col_count && meets_coverage(total_present, total_cells, min_coverage_pct)
            }
        }
    };

    if !qualify {
        return None;
    }

    match options.keys {
        KeyOrder::Sorted => {
            let mut cols = Vec::with_capacity_in(col_count, alloc);
            for &k in seen.iter() {
                cols.push(k);
            }
            Some(cols)
        }
        KeyOrder::Insertion => Some(cols_insertion),
    }
}

fn meets_coverage(total_present: usize, total_cells: usize, min_coverage_pct: u8) -> bool {
    let pct = usize::from(min_coverage_pct);
    if pct == 0 {
        return true;
    }
    if pct >= 100 {
        return total_present == total_cells;
    }
    match (total_present.checked_mul(100), total_cells.checked_mul(pct)) {
        (Some(lhs), Some(rhs)) => lhs >= rhs,
        _ => {
            // Overflow-safe fallback for gigantic counts: compare after scaling down by 100.
            let lhs = total_present / 100;
            let rhs = (total_cells / 100).saturating_mul(pct);
            lhs >= rhs
        }
    }
}

fn format_write_path<'v, 'a, A: Allocator>(
    stack: &Vec<WriteFrame<'v, 'a, A>, Alloc<'a>>,
    keys: KeyOrder,
    alloc: Alloc<'a>,
) -> String<Alloc<'a>> {
    let mut out = String::new_in(alloc);
    let mut before = false;
    for frame in stack.iter() {
        match frame {
            WriteFrame::List { next: 0, .. } | WriteFrame::Map { next: 0, .. } => {}
            WriteFrame::List { next, .. } => {
                if let Ok(next_before) = write_step(&mut out, Step::Index(*next - 1), before) {
                    before = next_before;
                }
            }
            WriteFrame::Map { map, next } => {
                let entry = match keys {
                    KeyOrder::Sorted => map.sorted_at(*next - 1),
                    KeyOrder::Insertion => map.entry_at(*next - 1),
                };
                if let Some((key, _)) = entry
                    && let Ok(next_before) = write_step(&mut out, Step::Key(key), before)
                {
                    before = next_before;
                }
            }
            WriteFrame::Table { cols, row, col, in_row, .. } => {
                if *in_row {
                    if let Ok(next_before) = write_step(&mut out, Step::Index(*row), before) {
                        before = next_before;
                    }
                    if *col > 0
                        && let Some(&col_name) = cols.get(*col - 1)
                        && let Ok(next_before) = write_step(&mut out, Step::Key(col_name), before)
                    {
                        before = next_before;
                    }
                }
            }
        }
    }
    out
}

fn write_indent(out: &mut String<Alloc<'_>>, indent: u8, depth: usize) {
    out.push('\n');
    let spaces = depth.saturating_mul(usize::from(indent));
    for _ in 0..spaces {
        out.push(' ');
    }
}

fn write_float(x: f64, out: &mut String<Alloc<'_>>) {
    if x.is_nan() {
        out.push_str("nan");
    } else if x.is_infinite() {
        match x.is_sign_negative() {
            true => out.push_str("-inf"),
            false => out.push_str("inf"),
        }
    } else {
        blaze_own::format_into!(out, "{x:?}");
    }
}

fn write_bytes(bytes: &[u8], out: &mut String<Alloc<'_>>) {
    out.push_str("0x[");
    for &b in bytes {
        blaze_own::format_into!(out, "{b:02x}");
    }
    out.push(']');
}

fn write_text_value(text: &str, out: &mut String<Alloc<'_>>) {
    if is_bare_value(text) {
        out.push_str(text);
    } else {
        write_quoted(text, out);
    }
}

fn write_key(key: &str, out: &mut String<Alloc<'_>>) {
    if is_bare_key(key) {
        out.push_str(key);
    } else {
        write_quoted(key, out);
    }
}

fn write_quoted(text: &str, out: &mut String<Alloc<'_>>) {
    out.push('"');
    let bytes = text.as_bytes();
    let mut run = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let short = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            b'\0' => "\\0",
            0x08 => "\\b",
            0x0C => "\\f",
            0x00..=0x1F => "",
            _ => continue,
        };
        if let Some(chunk) = text.get(run..i) {
            out.push_str(chunk);
        }
        match short.is_empty() {
            true => blaze_own::format_into!(out, "\\u{{{b:x}}}"),
            false => out.push_str(short),
        }
        run = i + 1;
    }
    if let Some(tail) = text.get(run..) {
        out.push_str(tail);
    }
    out.push('"');
}
