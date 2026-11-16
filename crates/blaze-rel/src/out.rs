//! A query's answer: named columns of blaze-data values, and the two shapes `blaze debug sql`
//! prints (crates/blaze-cargo/src/cli/observe.rs): an aligned table, and JSON lines.

use core::alloc::Allocator;
use core::fmt::{self, Write};

use blaze_data::{Int, Value};
use blaze_own::{String, Vec};

use crate::datum::{Datum, Store};
use crate::eval::write_float;
use crate::exec::Rel;

/// The rows of an answer, each as wide as `names`.
#[derive(Debug)]
pub struct Rows<A: Allocator> {
    pub names: Vec<String<A>, A>,
    pub rows: Vec<Vec<Value<A>, A>, A>,
}

fn value<A: Allocator + Copy>(store: &Store<A>, d: Datum, alloc: A) -> Value<A> {
    match d {
        Datum::Null => Value::Null,
        Datum::Bool(b) => Value::Bool(b),
        Datum::Int(n) => Value::Int(Int::from_i64(n)),
        Datum::UInt(n) => Value::Int(Int::from_u64(n)),
        Datum::Float(f) => Value::Float(f),
        Datum::Text(sym) => Value::Text(String::from_str_in(store.text(sym), alloc)),
        Datum::List(list) => {
            let items = store.items(list);
            let mut out = Vec::with_capacity_in(items.len(), alloc);
            for item in items {
                out.push(value(store, *item, alloc));
            }
            Value::List(out)
        }
    }
}

impl<A: Allocator + Copy> Rows<A> {
    pub(crate) fn of(names: &[&str], rel: &Rel<A>, store: &Store<A>, alloc: A) -> Rows<A> {
        let mut out_names = Vec::with_capacity_in(names.len(), alloc);
        for name in names {
            out_names.push(String::from_str_in(name, alloc));
        }
        let mut rows = Vec::with_capacity_in(rel.rows, alloc);
        for row in 0..rel.rows {
            let mut cells = Vec::with_capacity_in(names.len(), alloc);
            for col in 0..names.len() {
                cells.push(value(store, rel.get(col, row), alloc));
            }
            rows.push(cells);
        }
        Rows { names: out_names, rows }
    }

    /// The aligned table: the names, a rule of dashes as wide as each column, the rows (null
    /// empty), two spaces between columns, no trailing spaces, then `{n} rows`. With no
    /// columns, `0 rows`.
    pub fn write_table(&self, out: &mut impl Write) -> fmt::Result {
        if self.names.is_empty() {
            return out.write_str("0 rows\n");
        }
        let alloc = *self.names.allocator();
        let mut cells: Vec<String<A>, A> = Vec::with_capacity_in(self.rows.len() * self.names.len(), alloc);
        let mut widths = Vec::with_capacity_in(self.names.len(), alloc);
        for name in self.names.iter() {
            widths.push(name.as_str().chars().count());
        }
        for row in self.rows.iter() {
            for (at, cell) in row.iter().enumerate() {
                let mut text = String::new_in(alloc);
                write_cell(cell, &mut text)?;
                if let Some(width) = widths.get_mut(at) {
                    *width = (*width).max(text.as_str().chars().count());
                }
                cells.push(text);
            }
        }
        let mut line = String::new_in(alloc);
        let mut emit = |out: &mut dyn Write, texts: &mut dyn Iterator<Item = &str>| -> fmt::Result {
            line.clear();
            for (at, (text, width)) in texts.zip(widths.iter()).enumerate() {
                if at > 0 {
                    line.push_str("  ");
                }
                line.push_str(text);
                for _ in text.chars().count()..*width {
                    line.push(' ');
                }
            }
            out.write_str(line.as_str().trim_end_matches(' '))?;
            out.write_char('\n')
        };
        emit(out, &mut self.names.iter().map(String::as_str))?;
        let mut rule = Vec::with_capacity_in(widths.len(), alloc);
        for width in widths.iter() {
            let mut dashes = String::new_in(alloc);
            for _ in 0..*width {
                dashes.push('-');
            }
            rule.push(dashes);
        }
        emit(out, &mut rule.iter().map(String::as_str))?;
        for row in cells.chunks(self.names.len().max(1)) {
            emit(out, &mut row.iter().map(String::as_str))?;
        }
        writeln!(out, "{} rows", self.rows.len())
    }

    /// One JSON object a row, its keys the column names in order.
    pub fn write_json(&self, out: &mut impl Write) -> fmt::Result {
        for row in self.rows.iter() {
            out.write_char('{')?;
            for (at, (name, cell)) in self.names.iter().zip(row.iter()).enumerate() {
                if at > 0 {
                    out.write_char(',')?;
                }
                write_json_text(name.as_str(), out)?;
                out.write_char(':')?;
                write_json_value(cell, out)?;
            }
            out.write_str("}\n")?;
        }
        Ok(())
    }
}

/// A cell of the table: null empty, text as it is, the rest as JSON writes it.
fn write_cell<A: Allocator>(cell: &Value<A>, out: &mut impl Write) -> fmt::Result {
    match cell {
        Value::Null => Ok(()),
        Value::Text(text) => out.write_str(text.as_str()),
        other => write_json_value(other, out),
    }
}

fn write_json_value<A: Allocator>(cell: &Value<A>, out: &mut impl Write) -> fmt::Result {
    match cell {
        Value::Null => out.write_str("null"),
        Value::Bool(b) => out.write_str(if *b { "true" } else { "false" }),
        Value::Int(n) => write!(out, "{n}"),
        // JSON has no NaN or infinity.
        Value::Float(f) if !f.is_finite() => out.write_str("null"),
        Value::Float(f) => write_float(*f, out),
        Value::Text(text) => write_json_text(text.as_str(), out),
        Value::List(items) => {
            out.write_char('[')?;
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.write_char(',')?;
                }
                write_json_value(item, out)?;
            }
            out.write_char(']')
        }
        Value::Bytes(_) | Value::Map(_) => out.write_str("null"),
    }
}

/// A JSON string, escaped as serde_json escapes it.
fn write_json_text(text: &str, out: &mut impl Write) -> fmt::Result {
    out.write_char('"')?;
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            '\u{8}' => out.write_str("\\b")?,
            '\u{c}' => out.write_str("\\f")?,
            c if u32::from(c) < 0x20 => write!(out, "\\u{:04x}", u32::from(c))?,
            c => out.write_char(c)?,
        }
    }
    out.write_char('"')
}
