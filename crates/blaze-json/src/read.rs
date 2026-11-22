//! [`Reader`]: JSON text to [`Event`]s, one at a time.
//!
//! The reader checks the whole grammar as it goes (escapes and surrogate pairs included), so
//! a [`Str`] it hands out is known to decode; it does not see duplicate keys, which need
//! memory: [`read_in`](crate::read_in) checks those. It allocates only for nesting past the
//! first thousand levels.

use core::alloc::Allocator;

use blaze_data::Int;
use blaze_own::String;

use crate::{Error, Expected, Limits, What, stack::Stack};

/// A string as written between its quotes, checked; decoded only when asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Str<'t> {
    raw: &'t str,
    escaped: bool,
}

impl<'t> Str<'t> {
    /// Text to be written: no escapes in it yet (a writer escapes it).
    pub const fn plain(text: &'t str) -> Str<'t> {
        Str { raw: text, escaped: false }
    }

    /// The text as written, escapes and all; for a [`Str::plain`], the text itself.
    pub fn raw(&self) -> &'t str {
        self.raw
    }

    pub fn has_escapes(&self) -> bool {
        self.escaped
    }

    /// The text borrowed, when it has no escapes.
    pub fn as_plain(&self) -> Option<&'t str> {
        (!self.escaped).then_some(self.raw)
    }

    /// The decoded text appended to `out`.
    pub fn decode_into<A: Allocator>(&self, out: &mut String<A>) {
        if !self.escaped {
            out.push_str(self.raw);
            return;
        }
        let bytes = self.raw.as_bytes();
        let mut run = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'\\' {
                i += 1;
                continue;
            }
            out.push_str(&self.raw[run..i]);
            let (ch, len) = match bytes[i + 1] {
                b'"' => ('"', 2),
                b'\\' => ('\\', 2),
                b'/' => ('/', 2),
                b'b' => ('\u{8}', 2),
                b'f' => ('\u{c}', 2),
                b'n' => ('\n', 2),
                b'r' => ('\r', 2),
                b't' => ('\t', 2),
                b'u' => {
                    let high = hex4(bytes, i + 2).unwrap_or_else(|| unreachable!("checked when read"));
                    match high {
                        0xD800..=0xDBFF => {
                            let low = hex4(bytes, i + 8).unwrap_or_else(|| unreachable!("checked when read"));
                            let cp = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                            (char::from_u32(cp).unwrap_or_else(|| unreachable!("a pair is a scalar")), 12)
                        }
                        _ => (char::from_u32(high).unwrap_or_else(|| unreachable!("checked when read")), 6),
                    }
                }
                other => unreachable!("escape {other:#x} was checked when read"),
            };
            out.push(ch);
            i += len;
            run = i;
        }
        out.push_str(&self.raw[run..]);
    }

    /// The decoded text in `alloc`.
    pub fn to_string_in<A: Allocator>(&self, alloc: A) -> String<A> {
        let mut out = String::with_capacity_in(self.raw.len(), alloc);
        self.decode_into(&mut out);
        out
    }
}

/// The value of four hex digits at `at`, if there are four.
fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let digits = bytes.get(at..at + 4)?;
    let mut value = 0;
    for &d in digits {
        let v = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' => d - b'a' + 10,
            b'A'..=b'F' => d - b'A' + 10,
            _ => return None,
        };
        value = value * 16 + u32::from(v);
    }
    Some(value)
}

/// One step of a JSON text.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Event<'t> {
    Null,
    Bool(bool),
    Int(Int),
    /// Finite.
    Float(f64),
    Text(Str<'t>),
    /// A map's key; its value's events follow.
    Key(Str<'t>),
    StartList,
    EndList,
    StartMap,
    EndMap,
}

/// An event and the byte offset it starts at.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Item<'t> {
    pub at: usize,
    pub event: Event<'t>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// A value: at the top, after `:`, or after `,` in a list.
    Value,
    /// Just after `[`.
    FirstInList,
    /// Just after `{`.
    FirstInMap,
    /// After `,` in a map.
    Key,
    /// After a whole value.
    AfterValue,
    /// The top value is whole.
    Done,
}

/// JSON text to [`Event`]s; see the module documentation.
pub struct Reader<'t, S: Allocator> {
    text: &'t str,
    at: usize,
    limits: Limits,
    stack: Stack<S>,
    state: State,
}

impl<'t, S: Allocator> Reader<'t, S> {
    /// A reader over `bytes`, refused when they are past the size limit or not UTF-8. Nesting
    /// past the first thousand levels is held in `scratch`, a bit a level, so any depth limit
    /// is accepted and none recurses.
    pub fn new_in(bytes: &'t [u8], limits: Limits, scratch: S) -> Result<Reader<'t, S>, Error> {
        if bytes.len() > limits.most_bytes {
            return Err(Error { at: 0, what: What::TooLarge { len: bytes.len(), most: limits.most_bytes } });
        }
        let text = match core::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => return Err(Error { at: error.valid_up_to(), what: What::Utf8 }),
        };
        Ok(Reader { text, at: 0, limits, stack: Stack::new_in(scratch), state: State::Value })
    }

    /// The text, as UTF-8.
    pub fn text(&self) -> &'t str {
        self.text
    }

    /// How many lists and maps are open.
    pub fn depth(&self) -> usize {
        self.stack.depth()
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn err(&self, at: usize, what: What) -> Error {
        Error { at, what }
    }

    /// The error for what stands at `at`, where `expected` was wanted.
    fn unexpected(&self, at: usize, expected: Expected) -> Error {
        match self.text[at..].chars().next() {
            None => self.err(at, What::End { expected }),
            Some(found) => self.err(at, What::Unexpected { found, expected }),
        }
    }

    fn spaces(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.at += 1;
        }
    }

    /// The next event, or `None` once the top value is whole and only blanks follow.
    pub fn next(&mut self) -> Result<Option<Item<'t>>, Error> {
        loop {
            self.spaces();
            let at = self.at;
            match self.state {
                State::Done => {
                    return match self.peek() {
                        None => Ok(None),
                        Some(_) => Err(self.unexpected(at, Expected::End)),
                    };
                }
                State::AfterValue => match (self.stack.top(), self.peek()) {
                    (None, _) => self.state = State::Done,
                    (Some(true), Some(b',')) => {
                        self.at += 1;
                        self.state = State::Key;
                    }
                    (Some(false), Some(b',')) => {
                        self.at += 1;
                        self.state = State::Value;
                    }
                    (Some(true), Some(b'}')) => return Ok(Some(self.close(Event::EndMap))),
                    (Some(false), Some(b']')) => return Ok(Some(self.close(Event::EndList))),
                    (Some(true), _) => return Err(self.unexpected(at, Expected::CommaOrEndMap)),
                    (Some(false), _) => return Err(self.unexpected(at, Expected::CommaOrEndList)),
                },
                State::FirstInList if self.peek() == Some(b']') => {
                    return Ok(Some(self.close(Event::EndList)));
                }
                State::FirstInMap if self.peek() == Some(b'}') => {
                    return Ok(Some(self.close(Event::EndMap)));
                }
                State::FirstInMap | State::Key => {
                    let expected = match self.state {
                        State::Key => Expected::Key,
                        _ => Expected::KeyOrEndMap,
                    };
                    if self.peek() != Some(b'"') {
                        return Err(self.unexpected(at, expected));
                    }
                    let key = self.string()?;
                    self.spaces();
                    if self.peek() != Some(b':') {
                        return Err(self.unexpected(self.at, Expected::Colon));
                    }
                    self.at += 1;
                    self.state = State::Value;
                    return Ok(Some(Item { at, event: Event::Key(key) }));
                }
                State::Value | State::FirstInList => {
                    let expected = match self.state {
                        State::FirstInList => Expected::ValueOrEndList,
                        _ => Expected::Value,
                    };
                    return self.value(expected).map(Some);
                }
            }
        }
    }

    /// The `]` or `}` at the cursor.
    fn close(&mut self, event: Event<'t>) -> Item<'t> {
        let at = self.at;
        self.at += 1;
        self.stack.pop();
        self.state = State::AfterValue;
        Item { at, event }
    }

    fn open(&mut self, map: bool) -> Result<(), Error> {
        if self.stack.depth() == self.limits.most_depth {
            return Err(self.err(self.at, What::TooDeep { most: self.limits.most_depth }));
        }
        self.stack.push(map);
        self.at += 1;
        self.state = match map {
            true => State::FirstInMap,
            false => State::FirstInList,
        };
        Ok(())
    }

    fn value(&mut self, expected: Expected) -> Result<Item<'t>, Error> {
        let at = self.at;
        let event = match self.peek() {
            Some(b'{') => {
                self.open(true)?;
                return Ok(Item { at, event: Event::StartMap });
            }
            Some(b'[') => {
                self.open(false)?;
                return Ok(Item { at, event: Event::StartList });
            }
            Some(b'"') => Event::Text(self.string()?),
            Some(b't') => self.literal("true", Event::Bool(true))?,
            Some(b'f') => self.literal("false", Event::Bool(false))?,
            Some(b'n') => self.literal("null", Event::Null)?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            _ => return Err(self.unexpected(at, expected)),
        };
        self.state = State::AfterValue;
        Ok(Item { at, event })
    }

    fn literal(&mut self, word: &'static str, event: Event<'t>) -> Result<Event<'t>, Error> {
        let rest = &self.text.as_bytes()[self.at..];
        for (i, &b) in word.as_bytes().iter().enumerate() {
            if rest.get(i) != Some(&b) {
                return Err(self.unexpected(self.at + i, Expected::Literal(word)));
            }
        }
        self.at += word.len();
        Ok(event)
    }

    /// The string whose `"` is at the cursor, checked.
    fn string(&mut self) -> Result<Str<'t>, Error> {
        let bytes = self.text.as_bytes();
        let start = self.at + 1;
        let mut i = start;
        let mut escaped = false;
        loop {
            match bytes.get(i) {
                None => return Err(self.err(i, What::End { expected: Expected::Quote })),
                Some(b'"') => break,
                Some(b'\\') => {
                    escaped = true;
                    i = self.escape(i)?;
                }
                Some(&byte) if byte < 0x20 => return Err(self.err(i, What::Control { byte })),
                Some(_) => i += 1,
            }
        }
        self.at = i + 1;
        Ok(Str { raw: &self.text[start..i], escaped })
    }

    /// The escape whose `\` is at `i`, checked; the offset past it.
    fn escape(&self, i: usize) -> Result<usize, Error> {
        let bytes = self.text.as_bytes();
        match bytes.get(i + 1) {
            None => Err(self.err(i + 1, What::End { expected: Expected::Quote })),
            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => Ok(i + 2),
            Some(b'u') => {
                let Some(unit) = hex4(bytes, i + 2) else {
                    return Err(self.unexpected(i + 2, Expected::Hex));
                };
                match unit {
                    0xD800..=0xDBFF => {
                        let low = match bytes.get(i + 6..i + 8) {
                            Some(b"\\u") => hex4(bytes, i + 8),
                            _ => None,
                        };
                        match low {
                            Some(0xDC00..=0xDFFF) => Ok(i + 12),
                            _ => Err(self.err(i, What::LoneSurrogate)),
                        }
                    }
                    0xDC00..=0xDFFF => Err(self.err(i, What::LoneSurrogate)),
                    _ => Ok(i + 6),
                }
            }
            Some(_) => {
                let found = self.text[i + 1..].chars().next().unwrap_or_else(|| unreachable!("a byte is there"));
                Err(self.err(i + 1, What::BadEscape { found }))
            }
        }
    }

    /// The number at the cursor: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    fn number(&mut self) -> Result<Event<'t>, Error> {
        let bytes = self.text.as_bytes();
        let start = self.at;
        let mut i = start;
        let negative = bytes[i] == b'-';
        if negative {
            i += 1;
        }
        let digits = |i: usize| bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        let int_digits = digits(i);
        if int_digits == 0 {
            return Err(self.unexpected(i, Expected::Digit));
        }
        if bytes[i] == b'0' && int_digits > 1 {
            return Err(self.err(i, What::LeadingZero));
        }
        let int_start = i;
        i += int_digits;
        let int_end = i;
        let mut float = false;
        if bytes.get(i) == Some(&b'.') {
            float = true;
            i += 1;
            let n = digits(i);
            if n == 0 {
                return Err(self.unexpected(i, Expected::Digit));
            }
            i += n;
        }
        if let Some(b'e' | b'E') = bytes.get(i) {
            float = true;
            i += 1;
            if let Some(b'+' | b'-') = bytes.get(i) {
                i += 1;
            }
            let n = digits(i);
            if n == 0 {
                return Err(self.unexpected(i, Expected::Digit));
            }
            i += n;
        }
        if i - start > self.limits.most_number_len {
            return Err(self.err(start, What::NumberTooLong { most: self.limits.most_number_len }));
        }
        self.at = i;
        let written = &self.text[start..i];
        if float {
            return match written.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(Event::Float(f)),
                Ok(_) => Err(self.err(start, What::FloatOutOfRange)),
                Err(error) => unreachable!("{written:?} is a JSON number, which Rust reads: {error}"),
            };
        }
        let mut magnitude: u64 = 0;
        for &d in &bytes[int_start..int_end] {
            magnitude = match magnitude.checked_mul(10).and_then(|m| m.checked_add(u64::from(d - b'0'))) {
                Some(m) => m,
                None => return Err(self.err(start, What::IntegerOutOfRange)),
            };
        }
        match negative {
            false => Ok(Event::Int(Int::from_u64(magnitude))),
            true => match 0i64.checked_sub_unsigned(magnitude) {
                Some(n) => Ok(Event::Int(Int::from_i64(n))),
                None => Err(self.err(start, What::IntegerOutOfRange)),
            },
        }
    }
}
