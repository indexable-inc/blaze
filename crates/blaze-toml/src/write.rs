//! A [`Value`] as TOML text.
//!
//! The key-order rule: keys in the map's insertion order, except that in each table its plain
//! entries come before its sub-tables and arrays of tables (TOML can not go back to a table once
//! a header has left it). A sub-table gets a `[a.b]` header when it has plain entries or is
//! empty; one holding only tables is left implicit. From [`DocumentOptions::inline_from_depth`]
//! on, every table is written inline. TOML has no null and no bytes, and its integers are i64:
//! each is refused with the path to it, never quietly dropped or changed.

use core::{alloc::Allocator, fmt};

use blaze_data::{Map, Order, Step, Value, Visit, Walk, write_step};
use blaze_own::{String, Vec};
use blaze_text::{Dbg, DebugForm, Put, Show, Shown, Writes as _};

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

/// `a.b[3]: null has no TOML form`.
impl<A: Allocator> Show for WriteError<A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let path = match self.path.is_empty() {
            true => "the value",
            false => self.path.as_str(),
        };
        out.str(path).str(": ");
        match self.what {
            Unwritable::Null => out.str("null has no TOML form"),
            Unwritable::Bytes => out.str("bytes have no TOML form; encode them as text first"),
            Unwritable::Integer(n) => out.int(n).str(" is past TOML's i64 integers"),
        };
    }
}

/// `WriteError(a.b[3]: ...)`.
impl<A: Allocator> DebugForm for WriteError<A> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.str("WriteError(").show(self).byte(b')');
    }
}

/// The bridges `core::error::Error` needs: the text is [`Show`]'s and [`DebugForm`]'s.
impl<A: Allocator> fmt::Display for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl<A: Allocator> fmt::Debug for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(Dbg(self)), f)
    }
}

impl<A: Allocator> core::error::Error for WriteError<A> {}

/// The error for `what` at the steps `prefix` (a document's table path) and then where `walk`
/// stands (inside an inline value).
fn fail<A: Allocator, B: Allocator + Clone>(
    out: &String<B>,
    prefix: &[Step<'_>],
    walk: &Walk<'_, A, B>,
    what: Unwritable,
) -> WriteError<B> {
    let mut path = String::new_in(out.allocator().clone());
    let written = prefix
        .iter()
        .try_fold(false, |before, step| write_step(&mut path, *step, before))
        .and_then(|before| walk.path_after(&mut path, before));
    if written.is_err() {
        unreachable!("writing into a `String` does not fail");
    }
    WriteError { path, what }
}

fn bare_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// Strings and keys are spelled as cargo prints them: `toml_edit`'s default representation
// (`toml_writer`'s `TomlStringBuilder::as_default` and `TomlKeyBuilder::as_default`), so a
// manifest, config value or lockfile blaze writes is cargo's bytes.

/// What a string holds that decides its spelling.
struct Metrics {
    most_single_quotes: usize,
    most_double_quotes: usize,
    /// A control character other than tab and newline, which only an escape can say.
    escape_codes: bool,
    /// A backslash.
    escape: bool,
    newline: bool,
}

impl Metrics {
    fn of(text: &str) -> Metrics {
        let mut m =
            Metrics { most_single_quotes: 0, most_double_quotes: 0, escape_codes: false, escape: false, newline: false };
        let (mut single, mut double) = (0usize, 0usize);
        for &b in text.as_bytes() {
            single = if b == b'\'' { single + 1 } else { 0 };
            double = if b == b'"' { double + 1 } else { 0 };
            m.most_single_quotes = m.most_single_quotes.max(single);
            m.most_double_quotes = m.most_double_quotes.max(double);
            match b {
                b'\\' => m.escape = true,
                b'\t' => {}
                b'\n' => m.newline = true,
                c if c <= 0x1f || c == 0x7f => m.escape_codes = true,
                _ => {}
            }
        }
        m
    }
}

/// A TOML string's four spellings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StringForm {
    /// `"..."`, with escapes where needed.
    Basic,
    /// `'...'`: no escapes, so no `'`, newline or control character.
    Literal,
    /// `"""..."""`, a newline as itself.
    MultiLineBasic,
    /// `'''...'''`: no escapes, at most two `'` in a row.
    MultiLineLiteral,
}

impl StringForm {
    /// The form cargo writes `text` in: basic when it needs no escape, else literal, else a
    /// multi-line form, else an escaped basic string.
    pub fn of(text: &str) -> StringForm {
        let m = Metrics::of(text);
        if !m.escape_codes && !m.escape && m.most_double_quotes == 0 && !m.newline {
            StringForm::Basic
        } else if !m.escape_codes && m.most_single_quotes == 0 && !m.newline {
            StringForm::Literal
        } else if !m.escape_codes && !m.escape && m.most_double_quotes <= 2 {
            StringForm::MultiLineBasic
        } else if !m.escape_codes && m.most_single_quotes <= 2 {
            StringForm::MultiLineLiteral
        } else if m.newline {
            StringForm::MultiLineBasic
        } else {
            StringForm::Basic
        }
    }

    /// Whether `text` can be spelled this way and read back as itself.
    pub fn holds(self, text: &str) -> bool {
        let m = Metrics::of(text);
        match self {
            StringForm::Basic | StringForm::MultiLineBasic => true,
            StringForm::Literal => !m.escape_codes && m.most_single_quotes == 0 && !m.newline,
            StringForm::MultiLineLiteral => !m.escape_codes && m.most_single_quotes <= 2,
        }
    }
}

/// `text` as a TOML string value, in the form cargo writes it ([`StringForm::of`]).
pub fn write_string<B: Allocator>(text: &str, out: &mut String<B>) {
    write_string_as(text, StringForm::of(text), out);
}

/// `text` as a TOML string in `form`; refused, with nothing written, when the form cannot
/// hold it ([`StringForm::holds`]).
pub fn write_string_in_form<B: Allocator>(text: &str, form: StringForm, out: &mut String<B>) -> Result<(), StringForm> {
    match form.holds(text) {
        true => {
            write_string_as(text, form, out);
            Ok(())
        }
        false => Err(form),
    }
}

fn write_string_as<B: Allocator>(text: &str, form: StringForm, out: &mut String<B>) {
    let delimiter = match form {
        StringForm::Literal => "'",
        StringForm::Basic => "\"",
        StringForm::MultiLineLiteral => "'''",
        StringForm::MultiLineBasic => "\"\"\"",
    };
    let multi_line = matches!(form, StringForm::MultiLineLiteral | StringForm::MultiLineBasic);
    out.push_str(delimiter);
    // A newline just after the opening delimiter is trimmed when read, so a text that has
    // newlines starts on the next line, as cargo writes it.
    if multi_line && text.contains('\n') {
        out.push('\n');
    }
    match form {
        StringForm::Literal | StringForm::MultiLineLiteral => out.push_str(text),
        StringForm::Basic | StringForm::MultiLineBasic => escape(text, multi_line, out),
    }
    out.push_str(delimiter);
}

/// `text` in a basic string: `"` escaped past the run a multi-line string allows, the short
/// escapes, `\uXXXX` for the other control characters, a newline as itself in a multi-line one.
fn escape<B: Allocator>(text: &str, multi_line: bool, out: &mut String<B>) {
    let most_quotes = if multi_line { 2 } else { 0 };
    let mut quotes = 0;
    let mut run = 0;
    for (i, &b) in text.as_bytes().iter().enumerate() {
        if b == b'"' {
            quotes += 1;
            if quotes <= most_quotes {
                continue;
            }
            // An escaped quote ends the run, as toml_writer counts it.
            quotes = 0;
        } else {
            quotes = 0;
        }
        let short = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' if multi_line => continue,
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
}

/// `key` as a TOML key, as cargo writes it: bare when it can be, else a basic string when it
/// needs no escape, else a literal one, else a basic string with escapes.
pub fn write_key<B: Allocator>(key: &str, out: &mut String<B>) {
    if bare_key(key) {
        out.push_str(key);
        return;
    }
    let (mut single, mut double, mut backslash, mut codes) = (false, false, false, false);
    for &b in key.as_bytes() {
        match b {
            b'\'' => single = true,
            b'"' => double = true,
            b'\\' => backslash = true,
            b'\t' => {}
            c if c <= 0x1f || c == 0x7f => codes = true,
            _ => {}
        }
    }
    let form = match (codes, backslash, double, single) {
        (false, false, false, _) => StringForm::Basic,
        (false, _, _, false) => StringForm::Literal,
        _ => StringForm::Basic,
    };
    write_string_as(key, form, out);
}

/// A dotted key, `a.b."c d"`: each part as [`write_key`] writes it, joined by `.`.
pub fn write_dotted_key<B: Allocator, K: AsRef<str>>(parts: &[K], out: &mut String<B>) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        write_key(part.as_ref(), out);
    }
}

/// `value` as an inline TOML value (`1`, `"x"`, `[1, 2]`, `{ a = 1 }`) appended to `out`,
/// walked with its stack in `out`'s allocator, so no depth recurses.
pub fn write_inline<A: Allocator, B: Allocator + Clone>(value: &Value<A>, out: &mut String<B>) -> Result<(), WriteError<B>> {
    inline(value, out, &[])
}

/// [`write_inline`] for the value at the steps `prefix`, which an error's path starts with.
fn inline<A: Allocator, B: Allocator + Clone>(
    value: &Value<A>,
    out: &mut String<B>,
    prefix: &[Step<'_>],
) -> Result<(), WriteError<B>> {
    let mut walk = Walk::new_in(value, Order::Insertion, out.allocator().clone());
    while let Some(visit) = walk.next() {
        match visit {
            Visit::Leaf(leaf) => {
                if let Err(what) = scalar(leaf, out) {
                    return Err(fail(out, prefix, &walk, what));
                }
            }
            Visit::StartList(_) => out.push('['),
            Visit::Item(i) => {
                if i > 0 {
                    out.push_str(", ");
                }
            }
            Visit::EndList(_) => out.push(']'),
            Visit::StartMap(map) => out.push_str(if map.is_empty() { "{" } else { "{ " }),
            Visit::Key(i, key) => {
                if i > 0 {
                    out.push_str(", ");
                }
                write_key(key, out);
                out.push_str(" = ");
            }
            Visit::EndMap(len) => out.push_str(if len == 0 { "}" } else { " }" }),
        }
    }
    Ok(())
}

/// A value that is no list or map, or why TOML cannot say it.
fn scalar<A: Allocator, B: Allocator>(value: &Value<A>, out: &mut String<B>) -> Result<(), Unwritable> {
    match value {
        Value::Null => return Err(Unwritable::Null),
        Value::Bytes(_) => return Err(Unwritable::Bytes),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(n) => match n.as_i64() {
            Some(n) => blaze_own::format_into!(out, "{n}"),
            None => {
                let n = n.as_u64().unwrap_or_else(|| unreachable!("an Int outside i64 is a large u64"));
                return Err(Unwritable::Integer(n));
            }
        },
        Value::Float(x) => float(*x, out),
        Value::Text(text) => write_string(text.as_str(), out),
        Value::List(_) | Value::Map(_) => unreachable!("a walk visits containers as starts"),
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

/// A table being written: its map, its depth (the root is 0), the next entry to look at for a
/// sub-table, the next element of an array of tables under that entry, and how many steps of
/// the path lead to it.
struct Table<'v, A: Allocator> {
    map: &'v Map<A>,
    depth: usize,
    next: usize,
    element: usize,
    steps: usize,
}

// By hand: a derive would ask `A: Copy` of the allocator, which only the reference holds.
impl<A: Allocator> Clone for Table<'_, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A: Allocator> Copy for Table<'_, A> {}

/// `map` as a TOML document appended to `out`, by the order rule in this module's docs. Tables
/// are walked with a stack, and the path in steps, in `out`'s allocator, so no depth recurses.
pub fn write_document<A: Allocator, B: Allocator + Clone>(
    map: &Map<A>,
    out: &mut String<B>,
    options: DocumentOptions,
) -> Result<(), WriteError<B>> {
    let scratch = out.allocator().clone();
    let mut steps: Vec<Step<'_>, B> = Vec::new_in(scratch.clone());
    let mut stack = Vec::new_in(scratch);
    plain(map, 0, out, options, &mut steps)?;
    stack.push(Table { map, depth: 0, next: 0, element: 0, steps: 0 });
    while let Some(top) = stack.last_mut() {
        let table = *top;
        let Some((key, value)) = table.map.entry_at(table.next) else {
            stack.pop();
            continue;
        };
        let inline_below = table.depth + 1 >= options.inline_from_depth;
        let (child, array) = match entry(value, inline_below) {
            Entry::Plain => {
                top.next += 1;
                continue;
            }
            Entry::Table(child) => {
                top.next += 1;
                (child, false)
            }
            Entry::Tables(list) => match list.get(table.element) {
                None => {
                    top.next += 1;
                    top.element = 0;
                    continue;
                }
                Some(element) => {
                    top.element += 1;
                    let Value::Map(child) = element else { unreachable!("`entry` saw only maps") };
                    (child, true)
                }
            },
        };
        steps.truncate(table.steps);
        steps.push(Step::Key(key));
        let depth = table.depth + 1;
        let header_due = match array {
            true => true,
            false => {
                let below = depth + 1 >= options.inline_from_depth;
                child.is_empty() || child.iter().any(|(_, v)| matches!(entry(v, below), Entry::Plain))
            }
        };
        if header_due {
            header(out, steps.as_slice(), array);
        }
        if array {
            steps.push(Step::Index(table.element));
        }
        plain(child, depth, out, options, &mut steps)?;
        stack.push(Table { map: child, depth, next: 0, element: 0, steps: steps.len() });
    }
    Ok(())
}

/// `[a.b]` or `[[a.b]]` for the table at `steps` (its keys; indices are not in a header).
fn header<B: Allocator>(out: &mut String<B>, steps: &[Step<'_>], array: bool) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(if array { "[[" } else { "[" });
    let mut first = true;
    for step in steps {
        if let Step::Key(key) = step {
            if !first {
                out.push('.');
            }
            write_key(key, out);
            first = false;
        }
    }
    out.push_str(if array { "]]\n" } else { "]\n" });
}

/// The plain entries of `map`, a table at `depth` whose path is `steps`, as `key = value`
/// lines; `steps` is as it was after.
fn plain<'v, A: Allocator, B: Allocator + Clone>(
    map: &'v Map<A>,
    depth: usize,
    out: &mut String<B>,
    options: DocumentOptions,
    steps: &mut Vec<Step<'v>, B>,
) -> Result<(), WriteError<B>> {
    let inline_below = depth + 1 >= options.inline_from_depth;
    for (key, value) in map.iter() {
        if let Entry::Plain = entry(value, inline_below) {
            write_key(key, out);
            out.push_str(" = ");
            steps.push(Step::Key(key));
            let written = inline(value, out, steps.as_slice());
            steps.pop();
            written?;
            out.push('\n');
        }
    }
    Ok(())
}
