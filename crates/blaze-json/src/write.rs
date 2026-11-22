//! Writing JSON: a [`Value`] by [`write_into`], or events by [`Writer`].
//!
//! Both are deterministic: equal inputs give equal bytes. A float is written in the shortest
//! form that reads back as the same `f64` (Rust's `{:?}`: `0.1`, `1.0`, `1e100`); a string
//! escapes `"`, `\` and the control characters (`\n`, `\t`, ... and `\u00XX` for the rest) and
//! writes everything else as UTF-8.

use core::{alloc::Allocator, fmt};

use blaze_data::{Order, Value, Visit, Walk};
use blaze_own::{
    String,
    planted::{NoPlant, Planted},
};

use crate::{Event, JsonPlant, Str, stack::Stack};

/// Blanks between tokens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    /// None: `{"a":[1,2]}`.
    Compact,
    /// A newline before each entry, `indent` spaces per level, and `": "` after keys; an empty
    /// list or map stays `[]` or `{}`.
    Pretty { indent: u8 },
}

/// The order a map's keys are written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyOrder {
    /// By key bytes (code-point order): the same map gives the same text however it was built.
    Sorted,
    /// As inserted: the text's order for a map read, the builder's for one built.
    Insertion,
}

/// What `Value::Bytes` is written as; JSON has no bytes, so there is no silent choice.
#[derive(Clone, Copy, Debug)]
pub enum BytesAs {
    /// A typed error naming the path.
    Refuse,
    /// A string in this base64 encoding.
    Base64(blaze_base64::Engine),
}

#[derive(Clone, Copy, Debug)]
pub struct WriteOptions {
    pub style: Style,
    pub keys: KeyOrder,
    pub bytes: BytesAs,
}

impl WriteOptions {
    /// Compact, keys sorted, bytes refused.
    pub const COMPACT: WriteOptions =
        WriteOptions { style: Style::Compact, keys: KeyOrder::Sorted, bytes: BytesAs::Refuse };
    /// Two-space indent, keys sorted, bytes refused.
    pub const PRETTY: WriteOptions =
        WriteOptions { style: Style::Pretty { indent: 2 }, keys: KeyOrder::Sorted, bytes: BytesAs::Refuse };
}

/// What JSON cannot say.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Unwritable {
    /// Bytes, under [`BytesAs::Refuse`].
    Bytes,
    /// An infinite or NaN float.
    NonFinite(f64),
}

/// A value JSON cannot say: the path to it (`a.b[3]`, empty for the root) and what.
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
            Unwritable::Bytes => write!(f, "{path}: bytes have no JSON form; choose an encoding"),
            Unwritable::NonFinite(x) => write!(f, "{path}: {x} has no JSON form"),
        }
    }
}

impl<A: Allocator> fmt::Debug for WriteError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WriteError({self})")
    }
}

impl<A: Allocator> core::error::Error for WriteError<A> {}

/// `value` as JSON text in `alloc`.
pub fn to_string_in<A: Allocator, B: Allocator + Clone>(
    value: &Value<A>,
    options: WriteOptions,
    alloc: B,
) -> Result<String<B>, WriteError<B>> {
    let mut out = String::new_in(alloc);
    write_into(value, &mut out, options)?;
    Ok(out)
}

/// `value` as JSON text appended to `out`; an error's path is made in `out`'s allocator. On an
/// error, `out` holds what was written before it.
///
/// The value is walked with its open containers on a stack in `out`'s allocator, so no depth
/// recurses.
pub fn write_into<A: Allocator, B: Allocator + Clone>(
    value: &Value<A>,
    out: &mut String<B>,
    options: WriteOptions,
) -> Result<(), WriteError<B>> {
    write_into_planted(value, out, options, NoPlant)
}

/// [`write_into`] with a planted bug from `plant`: the seam the round trip's control drives.
/// Shipped callers pass `NoPlant` (through [`write_into`]), and the writer is the code as it is.
pub fn write_into_planted<A: Allocator, B: Allocator + Clone>(
    value: &Value<A>,
    out: &mut String<B>,
    options: WriteOptions,
    plant: impl Planted<JsonPlant>,
) -> Result<(), WriteError<B>> {
    let quote = match plant.has(JsonPlant::QuoteUnescaped) {
        true => Quote::Bare,
        false => Quote::Escaped,
    };
    let order = match options.keys {
        KeyOrder::Sorted => Order::Sorted,
        KeyOrder::Insertion => Order::Insertion,
    };
    let pretty = matches!(options.style, Style::Pretty { .. });
    let mut walk = Walk::new_in(value, order, out.allocator().clone());
    while let Some(visit) = walk.next() {
        match visit {
            Visit::Leaf(leaf) => leaf_into(leaf, out, options.bytes, quote).map_err(|what| fail(out, &walk, what))?,
            Visit::StartList(_) => out.push('['),
            Visit::StartMap(_) => out.push('{'),
            Visit::Item(i) => {
                if i > 0 {
                    out.push(',');
                }
                newline(out, options.style, walk.depth());
            }
            Visit::Key(i, key) => {
                if i > 0 {
                    out.push(',');
                }
                newline(out, options.style, walk.depth());
                escape(key, out, quote);
                out.push(':');
                if pretty {
                    out.push(' ');
                }
            }
            Visit::EndList(len) | Visit::EndMap(len) => {
                if len > 0 {
                    newline(out, options.style, walk.depth());
                }
                out.push(if matches!(visit, Visit::EndMap(_)) { '}' } else { ']' });
            }
        }
    }
    Ok(())
}

fn fail<A: Allocator, B: Allocator + Clone>(out: &String<B>, walk: &Walk<'_, A, B>, what: Unwritable) -> WriteError<B> {
    let mut path = String::new_in(out.allocator().clone());
    if walk.path_into(&mut path).is_err() {
        unreachable!("writing into a `String` does not fail");
    }
    WriteError { path, what }
}

fn newline<B: Allocator>(out: &mut String<B>, style: Style, depth: usize) {
    if let Style::Pretty { indent } = style {
        out.push('\n');
        for _ in 0..depth * usize::from(indent) {
            out.push(' ');
        }
    }
}

/// A value that is no list or map, as JSON.
fn leaf_into<A: Allocator, B: Allocator>(
    value: &Value<A>,
    out: &mut String<B>,
    bytes_as: BytesAs,
    quote: Quote,
) -> Result<(), Unwritable> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(n) => blaze_own::format_into!(out, "{n}"),
        Value::Float(x) => float(*x, out)?,
        Value::Text(text) => escape(text.as_str(), out, quote),
        Value::Bytes(bytes) => match bytes_as {
            BytesAs::Refuse => return Err(Unwritable::Bytes),
            BytesAs::Base64(engine) => {
                out.push('"');
                engine.encode_append(bytes.as_slice(), out);
                out.push('"');
            }
        },
        Value::List(_) | Value::Map(_) => unreachable!("a walk visits containers as starts"),
    }
    Ok(())
}

/// The shortest text that reads back as `x`; non-finite floats have none.
fn float<B: Allocator>(x: f64, out: &mut String<B>) -> Result<(), Unwritable> {
    if !x.is_finite() {
        return Err(Unwritable::NonFinite(x));
    }
    blaze_own::format_into!(out, "{x:?}");
    Ok(())
}

/// How a `"` inside a string is written: escaped, or bare under [`JsonPlant::QuoteUnescaped`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Quote {
    Escaped,
    Bare,
}

/// `text` as a JSON string.
fn escape<B: Allocator>(text: &str, out: &mut String<B>, quote: Quote) {
    out.push('"');
    let bytes = text.as_bytes();
    let mut run = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let short = match b {
            b'"' if quote == Quote::Escaped => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0C => "\\f",
            0x00..=0x1F => "",
            _ => continue,
        };
        out.push_str(&text[run..i]);
        match short.is_empty() {
            true => blaze_own::format_into!(out, "\\u{b:04x}"),
            false => out.push_str(short),
        }
        run = i + 1;
    }
    out.push_str(&text[run..]);
    out.push('"');
}

/// A misuse of the [`Writer`], or what JSON cannot say: the event's index and what.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StreamError {
    /// The 0-based index of the event refused.
    pub event: usize,
    pub what: StreamWhat,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum StreamWhat {
    /// A key where a value was wanted, or outside a map.
    KeyHere,
    /// A value where a map wanted a key.
    ValueWithoutKey,
    /// An end that closes nothing open, or the other kind.
    Unbalanced,
    /// An event after the top value was whole.
    AfterEnd,
    /// [`Writer::finish`] with a value open or none written.
    Unfinished,
    NonFinite(f64),
}

/// Events to compact JSON text, in the caller's key order, checked to make one well-formed
/// value. A [`Str`] read with escapes is copied as written (its escapes are valid JSON); a
/// [`Str::plain`] is escaped. Nesting past the first thousand levels is held in the scratch
/// allocator, so any depth is written without recursion.
pub struct Writer<'o, B: Allocator, S: Allocator> {
    out: &'o mut String<B>,
    stack: Stack<S>,
    due: Due,
    events: usize,
}

/// What the writer's place wants next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Due {
    /// The top value, a list's first element or a map's first key: no `,` before it.
    First,
    /// A key was written and its value is wanted.
    AfterKey,
    /// Another element or key, after a `,`, or the open container's end.
    Next,
    /// The top value is whole: nothing more may come.
    Whole,
}

impl<'o, B: Allocator, S: Allocator> Writer<'o, B, S> {
    pub fn new_in(out: &'o mut String<B>, scratch: S) -> Writer<'o, B, S> {
        Writer { out, stack: Stack::new_in(scratch), due: Due::First, events: 0 }
    }

    fn refuse(&self, what: StreamWhat) -> StreamError {
        StreamError { event: self.events, what }
    }

    /// The separator before a value or key at this place, after checking it may stand here.
    fn before(&mut self, key: bool) -> Result<(), StreamError> {
        if self.due == Due::Whole {
            return Err(self.refuse(StreamWhat::AfterEnd));
        }
        let in_map = self.stack.top() == Some(true);
        let after_key = self.due == Due::AfterKey;
        match (key, in_map, after_key) {
            (true, true, false) => {}
            (true, _, _) => return Err(self.refuse(StreamWhat::KeyHere)),
            (false, true, false) => return Err(self.refuse(StreamWhat::ValueWithoutKey)),
            (false, _, _) => {}
        }
        if self.due == Due::Next {
            self.out.push(',');
        }
        self.due = Due::Next;
        Ok(())
    }

    /// After a whole value: the top one ends the text.
    fn after_value(&mut self) {
        if self.stack.depth() == 0 {
            self.due = Due::Whole;
        }
    }

    pub fn event(&mut self, event: &Event<'_>) -> Result<(), StreamError> {
        match *event {
            Event::Key(key) => {
                self.before(true)?;
                string(key, self.out);
                self.out.push(':');
                self.due = Due::AfterKey;
            }
            Event::EndList | Event::EndMap => {
                let map = matches!(event, Event::EndMap);
                if matches!(self.due, Due::Whole | Due::AfterKey) || self.stack.top() != Some(map) {
                    return Err(self.refuse(StreamWhat::Unbalanced));
                }
                self.stack.pop();
                self.out.push(if map { '}' } else { ']' });
                self.due = Due::Next;
                self.after_value();
            }
            Event::StartList | Event::StartMap => {
                self.before(false)?;
                let map = matches!(event, Event::StartMap);
                self.stack.push(map);
                self.out.push(if map { '{' } else { '[' });
                self.due = Due::First;
            }
            Event::Float(x) if !x.is_finite() => return Err(self.refuse(StreamWhat::NonFinite(x))),
            scalar => {
                self.before(false)?;
                match scalar {
                    Event::Null => self.out.push_str("null"),
                    Event::Bool(true) => self.out.push_str("true"),
                    Event::Bool(false) => self.out.push_str("false"),
                    Event::Int(n) => blaze_own::format_into!(self.out, "{n}"),
                    Event::Float(x) => blaze_own::format_into!(self.out, "{x:?}"),
                    Event::Text(text) => string(text, self.out),
                    _ => unreachable!("the structural events are matched above"),
                }
                self.after_value();
            }
        }
        self.events += 1;
        Ok(())
    }

    /// Checks that one whole value was written.
    pub fn finish(self) -> Result<(), StreamError> {
        match self.due {
            Due::Whole => Ok(()),
            Due::First | Due::AfterKey | Due::Next => Err(self.refuse(StreamWhat::Unfinished)),
        }
    }
}

fn string<B: Allocator>(text: Str<'_>, out: &mut String<B>) {
    match text.has_escapes() {
        true => {
            out.push('"');
            out.push_str(text.raw());
            out.push('"');
        }
        false => escape(text.raw(), out, Quote::Escaped),
    }
}
