//! A [`Value`] as TOML text.
//!
//! The key-order rule: keys in the map's insertion order, except that in each table its plain
//! entries come before its sub-tables and arrays of tables (TOML can not go back to a table once
//! a header has left it). A sub-table gets a `[a.b]` header when it has plain entries or is
//! empty; one holding only tables is left implicit. From [`DocumentOptions::inline_from_depth`]
//! on, every table is written inline. TOML has no null and no bytes, and its integers are i64:
//! each is refused with the path to it, never quietly dropped or changed.

use core::{alloc::Allocator, fmt};

use blaze_data::{At, Map, Value};
use blaze_own::String;

/// How [`write_document`] lays a document out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DocumentOptions {
    /// Tables this many keys deep or deeper are written `{ inline }`; the root is depth 0, so
    /// with 2, `[dependencies]` gets a header and `serde = { version = "1" }` stays inline, as
    /// Cargo's manifests are written.
    pub inline_from_depth: usize,
}

impl DocumentOptions {
    pub const CARGO: DocumentOptions = DocumentOptions { inline_from_depth: 2 };
}

/// What TOML cannot say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unwritable {
    Null,
    Bytes,
    /// An integer past `i64::MAX`.
    Integer(u64),
}

/// A value TOML cannot say: the path to it (`a.b[3]`, empty for the root) and what.
pub struct WriteError<A: Allocator> {
    pub path: String<A>,
    pub what: Unwritable,
}

impl<A: Allocator> fmt::Display for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = match self.path.is_empty() {
            true => "the value",
            false => self.path.as_str(),
        };
        match self.what {
            Unwritable::Null => write!(f, "{path}: null has no TOML form"),
            Unwritable::Bytes => write!(f, "{path}: bytes have no TOML form; encode them as text first"),
            Unwritable::Integer(n) => write!(f, "{path}: {n} is past TOML's i64 integers"),
        }
    }
}

impl<A: Allocator> fmt::Debug for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WriteError({self})")
    }
}

impl<A: Allocator> core::error::Error for WriteError<A> {}

pub(crate) fn fail<B: Allocator + Clone>(out: &String<B>, at: &At<'_>, what: Unwritable) -> WriteError<B> {
    let mut path = String::new_in(out.allocator().clone());
    if at.write_into(&mut path).is_err() {
        unreachable!("writing into a `String` does not fail");
    }
    WriteError { path, what }
}

fn bare_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `key` as a TOML key: bare when it can be, else a basic string.
pub fn write_key<B: Allocator>(key: &str, out: &mut String<B>) {
    match bare_key(key) {
        true => out.push_str(key),
        false => basic(key, out),
    }
}

/// `text` as a basic string.
fn basic<B: Allocator>(text: &str, out: &mut String<B>) {
    out.push('"');
    let mut run = 0;
    for (i, &b) in text.as_bytes().iter().enumerate() {
        let short = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0C => "\\f",
            0x00..=0x1F | 0x7F => "",
            _ => continue,
        };
        out.push_str(&text[run..i]);
        match short.is_empty() {
            true => blaze_own::format_into!(out, "\\u{b:04X}"),
            false => out.push_str(short),
        }
        run = i + 1;
    }
    out.push_str(&text[run..]);
    out.push('"');
}

/// `value` as an inline TOML value (`1`, `"x"`, `[1, 2]`, `{ a = 1 }`) appended to `out`.
pub fn write_inline<A: Allocator, B: Allocator + Clone>(value: &Value<A>, out: &mut String<B>) -> Result<(), WriteError<B>> {
    inline(value, out, &At::ROOT)
}

fn inline<A: Allocator, B: Allocator + Clone>(value: &Value<A>, out: &mut String<B>, at: &At<'_>) -> Result<(), WriteError<B>> {
    match value {
        Value::Null => return Err(fail(out, at, Unwritable::Null)),
        Value::Bytes(_) => return Err(fail(out, at, Unwritable::Bytes)),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(n) => match n.as_i64() {
            Some(n) => blaze_own::format_into!(out, "{n}"),
            None => {
                let n = n.as_u64().unwrap_or_else(|| unreachable!("an Int outside i64 is a large u64"));
                return Err(fail(out, at, Unwritable::Integer(n)));
            }
        },
        Value::Float(x) => float(*x, out),
        Value::Text(text) => basic(text.as_str(), out),
        Value::List(list) => {
            out.push('[');
            for (i, item) in list.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                inline(item, out, &at.index(i))?;
            }
            out.push(']');
        }
        Value::Map(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push_str("{ ");
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_key(key, out);
                out.push_str(" = ");
                inline(item, out, &at.key(key))?;
            }
            out.push_str(" }");
        }
    }
    Ok(())
}

fn float<B: Allocator>(x: f64, out: &mut String<B>) {
    if x.is_nan() {
        out.push_str(if x.is_sign_negative() { "-nan" } else { "nan" });
    } else if x.is_infinite() {
        out.push_str(if x < 0.0 { "-inf" } else { "inf" });
    } else {
        // Debug is the shortest text that reads back as `x`, always with a `.` or an `e`.
        blaze_own::format_into!(out, "{x:?}");
    }
}

/// A header's keys, chained on the stack.
struct Keys<'p> {
    parent: Option<&'p Keys<'p>>,
    key: &'p str,
}

impl Keys<'_> {
    fn write<B: Allocator>(&self, out: &mut String<B>) {
        if let Some(parent) = self.parent {
            parent.write(out);
            out.push('.');
        }
        write_key(self.key, out);
    }
}

enum Entry<'v, A: Allocator> {
    Plain,
    Table(&'v Map<A>),
    Tables(&'v [Value<A>]),
}

fn entry<'v, A: Allocator>(value: &'v Value<A>, inline: bool) -> Entry<'v, A> {
    match value {
        _ if inline => Entry::Plain,
        Value::Map(map) => Entry::Table(map),
        Value::List(list) if !list.is_empty() && list.iter().all(|v| matches!(v, Value::Map(_))) => {
            Entry::Tables(list.as_slice())
        }
        _ => Entry::Plain,
    }
}

/// `map` as a TOML document appended to `out`, by the order rule in this module's docs.
pub fn write_document<A: Allocator, B: Allocator + Clone>(
    map: &Map<A>,
    out: &mut String<B>,
    options: DocumentOptions,
) -> Result<(), WriteError<B>> {
    body(map, out, options, None, &At::ROOT, 0)
}

fn header<B: Allocator>(out: &mut String<B>, keys: &Keys<'_>, array: bool) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(if array { "[[" } else { "[" });
    keys.write(out);
    out.push_str(if array { "]]\n" } else { "]\n" });
}

fn body<A: Allocator, B: Allocator + Clone>(
    map: &Map<A>,
    out: &mut String<B>,
    options: DocumentOptions,
    keys: Option<&Keys<'_>>,
    at: &At<'_>,
    depth: usize,
) -> Result<(), WriteError<B>> {
    let inline_below = depth + 1 >= options.inline_from_depth;
    for (key, value) in map.iter() {
        if let Entry::Plain = entry(value, inline_below) {
            write_key(key, out);
            out.push_str(" = ");
            inline(value, out, &at.key(key))?;
            out.push('\n');
        }
    }
    for (key, value) in map.iter() {
        let here = Keys { parent: keys, key };
        let at = at.key(key);
        match entry(value, inline_below) {
            Entry::Plain => {}
            Entry::Table(table) => {
                let plain = table.iter().any(|(_, v)| matches!(entry(v, depth + 2 >= options.inline_from_depth), Entry::Plain));
                if plain || table.is_empty() {
                    header(out, &here, false);
                }
                body(table, out, options, Some(&here), &at, depth + 1)?;
            }
            Entry::Tables(list) => {
                for (i, element) in list.iter().enumerate() {
                    let Value::Map(table) = element else { unreachable!("`entry` saw only maps") };
                    header(out, &here, true);
                    body(table, out, options, Some(&here), &at.index(i), depth + 1)?;
                }
            }
        }
    }
    Ok(())
}
