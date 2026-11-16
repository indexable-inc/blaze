//! The reader: TOML 1.0 text to a [`Document`], line by line, one pass, no backtracking.

use core::alloc::Allocator;

use blaze_own::{String, Vec};

use crate::{
    Error, Expected, Limits, MOST_KEY_PARTS, What,
    doc::{
        Date, Datetime, Document, Kind, NONE, Node, Nodes, Offset, Origin, Span, Text, Time, narrow, pack, widen,
    },
};

/// The most characters of a float as written, underscores aside.
const MOST_FLOAT_LEN: usize = 256;

/// One part of a dotted key: its decoded text and where it is written.
#[derive(Clone, Copy)]
struct Part {
    text: Text,
    span: Span,
}

impl Part {
    const EMPTY: Part = Part { text: Text::EMPTY, span: Span { start: 0, end: 0 } };
}

/// A dotted key's parts, in place.
struct Key {
    parts: [Part; MOST_KEY_PARTS],
    len: usize,
}

impl Key {
    fn init(&self) -> &[Part] {
        &self.parts[..self.len - 1]
    }

    fn last(&self) -> Part {
        self.parts[self.len - 1]
    }
}

impl<'t, A: Allocator + Clone> Document<'t, A> {
    /// `text` read as TOML 1.0, within `limits`, the document's columns in `alloc`.
    pub fn parse_in(text: &'t str, limits: Limits, alloc: A) -> Result<Document<'t, A>, Error> {
        let most = limits.most_bytes.min(widen(u32::MAX));
        if text.len() > most {
            return Err(Error { at: 0, what: What::TooLarge { len: text.len(), most } });
        }
        let mut doc = Document {
            text,
            nodes: Nodes::new_in(alloc.clone()),
            arena: String::new_in(alloc.clone()),
            datetimes: Vec::new_in(alloc),
        };
        doc.nodes.push(Node {
            kind: Kind::Table,
            origin: Origin::Header,
            parent: NONE,
            key: Text::EMPTY,
            key_span: Span { start: 0, end: 0 },
            span: Span { start: 0, end: narrow(text.len()) },
            payload: 0,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
        });
        Parser { text, b: text.as_bytes(), at: 0, limits, doc: &mut doc, table: 0, section: 0, depth: 0 }
            .document()?;
        Ok(doc)
    }
}

struct Parser<'t, 'd, A: Allocator> {
    text: &'t str,
    b: &'t [u8],
    at: usize,
    limits: Limits,
    doc: &'d mut Document<'t, A>,
    /// The table key/values go into: the last header's, or the root.
    table: u32,
    /// The table whose section the current line is in, whose body end each entry moves.
    section: u32,
    /// Arrays and inline tables open.
    depth: usize,
}

fn is_control(ch: char) -> bool {
    (ch < ' ' && ch != '\t') || ch == '\u{7f}'
}

fn bare(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

impl<'t, A: Allocator> Parser<'t, '_, A> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.b.get(self.at + n).copied()
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
        while let Some(b' ' | b'\t') = self.peek() {
            self.at += 1;
        }
    }

    /// A newline at the cursor, eaten.
    fn newline(&mut self) -> Result<bool, Error> {
        match self.peek() {
            Some(b'\n') => {
                self.at += 1;
                Ok(true)
            }
            Some(b'\r') if self.peek_at(1) == Some(b'\n') => {
                self.at += 2;
                Ok(true)
            }
            Some(b'\r') => Err(self.err(self.at, What::BareCarriageReturn)),
            _ => Ok(false),
        }
    }

    /// A comment at the cursor, up to its newline (not eaten).
    fn comment(&mut self) -> Result<(), Error> {
        if self.peek() != Some(b'#') {
            return Ok(());
        }
        self.at += 1;
        while let Some(ch) = self.text[self.at..].chars().next() {
            match ch {
                '\n' => break,
                '\r' if self.peek_at(1) == Some(b'\n') => break,
                '\r' => return Err(self.err(self.at, What::BareCarriageReturn)),
                ch if is_control(ch) => return Err(self.err(self.at, What::Control { ch })),
                ch => self.at += ch.len_utf8(),
            }
        }
        Ok(())
    }

    /// Blanks, a comment, and the newline or the end of the text.
    fn end_of_line(&mut self) -> Result<(), Error> {
        self.spaces();
        self.comment()?;
        if self.peek().is_none() || self.newline()? {
            return Ok(());
        }
        Err(self.unexpected(self.at, Expected::EndOfLine))
    }

    /// Blanks, comments and newlines, as an array allows between its parts.
    fn array_blanks(&mut self) -> Result<(), Error> {
        loop {
            self.spaces();
            self.comment()?;
            if !self.newline()? {
                return Ok(());
            }
        }
    }

    fn document(mut self) -> Result<(), Error> {
        loop {
            self.spaces();
            match self.peek() {
                None => return Ok(()),
                Some(b'#' | b'\n' | b'\r') => self.end_of_line()?,
                Some(b'[') => self.header()?,
                Some(_) => {
                    self.keyval(self.table)?;
                    self.end_of_line()?;
                    let end = crate::plant::body_end(self.at);
                    self.doc.nodes.payloads_mut()[widen(self.section)] = u64::from(narrow(end));
                }
            }
        }
    }

    /// `[a.b]` or `[[a.b]]` and the rest of its line.
    fn header(&mut self) -> Result<(), Error> {
        let start = self.at;
        self.at += 1;
        let array = self.peek() == Some(b'[');
        if array {
            self.at += 1;
        }
        self.spaces();
        let key = self.key()?;
        self.spaces();
        let close = match array {
            true => self.peek() == Some(b']') && self.peek_at(1) == Some(b']'),
            false => self.peek() == Some(b']'),
        };
        if !close {
            let expected = if array { Expected::CloseArrayHeader } else { Expected::CloseHeader };
            return Err(self.unexpected(self.at, expected));
        }
        self.at += if array { 2 } else { 1 };
        let span = Span { start: narrow(start), end: narrow(self.at) };
        let mut parent = 0;
        for part in key.init() {
            parent = self.descend_header(parent, *part)?;
        }
        let table = match array {
            true => self.array_table(parent, key.last(), span)?,
            false => self.define_table(parent, key.last(), span)?,
        };
        self.table = table;
        self.section = table;
        self.end_of_line()?;
        self.doc.nodes.payloads_mut()[widen(table)] = u64::from(narrow(self.at));
        Ok(())
    }

    /// A dotted key: simple keys joined by `.`, blanks allowed around each dot.
    fn key(&mut self) -> Result<Key, Error> {
        let mut key = Key { parts: [Part::EMPTY; MOST_KEY_PARTS], len: 0 };
        loop {
            if key.len == MOST_KEY_PARTS {
                return Err(self.err(self.at, What::KeyTooLong { most: MOST_KEY_PARTS }));
            }
            key.parts[key.len] = self.simple_key()?;
            key.len += 1;
            self.spaces();
            if self.peek() != Some(b'.') {
                return Ok(key);
            }
            self.at += 1;
            self.spaces();
        }
    }

    fn simple_key(&mut self) -> Result<Part, Error> {
        let start = self.at;
        let text = match self.peek() {
            Some(b'"') => self.basic(false)?,
            Some(b'\'') => self.literal(false)?,
            Some(b) if bare(b) => {
                while self.peek().is_some_and(bare) {
                    self.at += 1;
                }
                Text { at: narrow(start), len: narrow(self.at - start), arena: false }
            }
            _ => return Err(self.unexpected(start, Expected::Key)),
        };
        Ok(Part { text, span: Span { start: narrow(start), end: narrow(self.at) } })
    }

    /// `key = value` into `table`.
    fn keyval(&mut self, table: u32) -> Result<(), Error> {
        let key = self.key()?;
        self.spaces();
        if self.peek() != Some(b'=') {
            return Err(self.unexpected(self.at, Expected::Equals));
        }
        self.at += 1;
        self.spaces();
        let mut parent = table;
        for part in key.init() {
            parent = self.descend_dotted(parent, *part)?;
        }
        let last = key.last();
        if let Some(existing) = self.doc.child(parent, self.doc.text_of(last.text)) {
            let first = self.first(existing);
            return Err(self.err(widen(last.span.start), What::Duplicate { first }));
        }
        self.value(parent, last).map(|_| ())
    }

    /// Where node `id` was first written: its key, or itself for a keyless one.
    fn first(&self, id: u32) -> usize {
        let key = self.doc.nodes.key_spans()[widen(id)];
        match key.start == key.end {
            true => widen(self.doc.nodes.spans()[widen(id)].start),
            false => widen(key.start),
        }
    }

    fn found(&self, parent: u32, part: Part) -> Option<(u32, Kind, Origin)> {
        let id = self.doc.child(parent, self.doc.text_of(part.text))?;
        Some((id, self.doc.nodes.kinds()[widen(id)], self.doc.nodes.origins()[widen(id)]))
    }

    /// A step of a header's key that is not its last: a table on the way, made if missing.
    fn descend_header(&mut self, parent: u32, part: Part) -> Result<u32, Error> {
        let at = widen(part.span.start);
        match self.found(parent, part) {
            None => self.add(Kind::Table, Origin::Implicit, parent, part, part.span, 0),
            Some((id, Kind::Table, Origin::Inline)) | Some((id, Kind::Array, Origin::Value)) => {
                Err(self.err(at, What::Closed { first: self.first(id) }))
            }
            Some((id, Kind::Table, _)) => Ok(id),
            Some((id, Kind::Array, _)) => Ok(self.doc.nodes.last_children()[widen(id)]),
            Some((id, _, _)) => Err(self.err(at, What::NotATable { first: self.first(id) })),
        }
    }

    /// A key/value's key part that is not its last: a dotted table, made if missing.
    fn descend_dotted(&mut self, parent: u32, part: Part) -> Result<u32, Error> {
        let at = widen(part.span.start);
        match self.found(parent, part) {
            None => self.add(Kind::Table, Origin::Dotted, parent, part, part.span, 0),
            Some((id, Kind::Table, Origin::Dotted)) => Ok(id),
            Some((id, Kind::Table, Origin::Inline)) | Some((id, Kind::Array, Origin::Value)) => {
                Err(self.err(at, What::Closed { first: self.first(id) }))
            }
            Some((id, Kind::Table, _)) => Err(self.err(at, What::Redefined { first: self.first(id) })),
            Some((id, _, _)) => Err(self.err(at, What::NotATable { first: self.first(id) })),
        }
    }

    /// The table `[a.b]` defines.
    fn define_table(&mut self, parent: u32, part: Part, span: Span) -> Result<u32, Error> {
        let at = widen(part.span.start);
        match self.found(parent, part) {
            None => self.add(Kind::Table, Origin::Header, parent, part, span, 0),
            Some((id, Kind::Table, Origin::Implicit)) => {
                self.doc.nodes.origins_mut()[widen(id)] = Origin::Header;
                self.doc.nodes.spans_mut()[widen(id)] = span;
                Ok(id)
            }
            Some((id, Kind::Table | Kind::Array, _)) => {
                Err(self.err(at, What::Redefined { first: self.first(id) }))
            }
            Some((id, _, _)) => Err(self.err(at, What::NotATable { first: self.first(id) })),
        }
    }

    /// A new element of the array of tables `[[a.b]]` names.
    fn array_table(&mut self, parent: u32, part: Part, span: Span) -> Result<u32, Error> {
        let array = match self.found(parent, part) {
            None => self.add(Kind::Array, Origin::Header, parent, part, span, 0)?,
            Some((id, Kind::Array, Origin::Header)) => id,
            Some((id, _, _)) => {
                let first = self.first(id);
                return Err(self.err(widen(part.span.start), What::NotAnArrayOfTables { first }));
            }
        };
        let element = Part { text: Text::EMPTY, span: part.span };
        self.add(Kind::Table, Origin::Header, array, element, span, 0)
    }

    /// A node under `parent`, last among its children.
    fn add(&mut self, kind: Kind, origin: Origin, parent: u32, key: Part, span: Span, payload: u64) -> Result<u32, Error> {
        let nodes = &mut self.doc.nodes;
        if nodes.len() >= self.limits.most_nodes {
            return Err(Error { at: widen(span.start), what: What::TooManyNodes { most: self.limits.most_nodes } });
        }
        let id = narrow(nodes.len());
        nodes.push(Node {
            kind,
            origin,
            parent,
            key: key.text,
            key_span: key.span,
            span,
            payload,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
        });
        let last = nodes.last_children()[widen(parent)];
        match last {
            NONE => nodes.first_children_mut()[widen(parent)] = id,
            last => nodes.next_siblings_mut()[widen(last)] = id,
        }
        nodes.last_children_mut()[widen(parent)] = id;
        Ok(id)
    }

    /// A value under `parent`, keyed by `key` (empty for an array's element).
    fn value(&mut self, parent: u32, key: Part) -> Result<u32, Error> {
        let start = self.at;
        let (kind, payload) = match self.peek() {
            Some(b'"') => {
                let multi = self.b[start..].starts_with(b"\"\"\"");
                let text = self.basic(multi)?;
                (Kind::String, pack(text.at, text.len))
            }
            Some(b'\'') => {
                let multi = self.b[start..].starts_with(b"'''");
                let text = self.literal(multi)?;
                (Kind::String, pack(text.at, text.len))
            }
            Some(b't') => (Kind::Bool, self.word("true", 1)?),
            Some(b'f') => (Kind::Bool, self.word("false", 0)?),
            Some(b'[') => return self.array(parent, key),
            Some(b'{') => return self.inline(parent, key),
            Some(b'0'..=b'9') if self.is_datetime() => (Kind::Datetime, self.datetime()?),
            Some(b'+' | b'-' | b'0'..=b'9' | b'i' | b'n') => self.number()?,
            _ => return Err(self.unexpected(start, Expected::Value)),
        };
        let span = Span { start: narrow(start), end: narrow(self.at) };
        self.add(kind, Origin::Value, parent, key, span, payload)
    }

    fn word(&mut self, word: &'static str, payload: u64) -> Result<u64, Error> {
        for (i, &b) in word.as_bytes().iter().enumerate() {
            if self.peek_at(i) != Some(b) {
                return Err(self.unexpected(self.at + i, Expected::Value));
            }
        }
        self.at += word.len();
        Ok(payload)
    }

    fn open(&mut self) -> Result<(), Error> {
        if self.depth == self.limits.most_depth {
            return Err(self.err(self.at, What::TooDeep { most: self.limits.most_depth }));
        }
        self.depth += 1;
        Ok(())
    }

    fn array(&mut self, parent: u32, key: Part) -> Result<u32, Error> {
        self.open()?;
        let start = narrow(self.at);
        let id = self.add(Kind::Array, Origin::Value, parent, key, Span { start, end: start }, 0)?;
        self.at += 1;
        loop {
            self.array_blanks()?;
            if self.peek() == Some(b']') {
                self.at += 1;
                break;
            }
            self.value(id, Part { text: Text::EMPTY, span: Span { start: narrow(self.at), end: narrow(self.at) } })?;
            self.array_blanks()?;
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(self.unexpected(self.at, Expected::CommaOrEndArray)),
            }
        }
        self.doc.nodes.spans_mut()[widen(id)].end = narrow(self.at);
        self.depth -= 1;
        Ok(id)
    }

    /// `{ k = v, ... }` on one line, no trailing comma (TOML 1.0).
    fn inline(&mut self, parent: u32, key: Part) -> Result<u32, Error> {
        self.open()?;
        let start = narrow(self.at);
        let id = self.add(Kind::Table, Origin::Inline, parent, key, Span { start, end: start }, 0)?;
        self.at += 1;
        self.spaces();
        if self.peek() == Some(b'}') {
            self.at += 1;
        } else {
            loop {
                self.keyval(id)?;
                self.spaces();
                match self.peek() {
                    Some(b',') => {
                        self.at += 1;
                        self.spaces();
                    }
                    Some(b'}') => {
                        self.at += 1;
                        break;
                    }
                    _ => return Err(self.unexpected(self.at, Expected::CommaOrEndInline)),
                }
            }
        }
        self.doc.nodes.spans_mut()[widen(id)].end = narrow(self.at);
        self.depth -= 1;
        Ok(id)
    }

    /// A basic string (`"..."`, or `"""..."""` when `multi`) decoded into the arena.
    fn basic(&mut self, multi: bool) -> Result<Text, Error> {
        let arena_start = self.doc.arena.len();
        self.at += if multi { 3 } else { 1 };
        if multi {
            self.newline()?;
        }
        let mut run = self.at;
        loop {
            match self.peek() {
                None => return Err(self.err(self.at, What::End { expected: Expected::Quote })),
                Some(b'"') => {
                    if !multi {
                        self.doc.arena.push_str(&self.text[run..self.at]);
                        self.at += 1;
                        break;
                    }
                    let quotes = self.b[self.at..].iter().take_while(|&&b| b == b'"').count();
                    if quotes < 3 {
                        self.at += quotes;
                        continue;
                    }
                    if quotes > 5 {
                        return Err(self.unexpected(self.at + 5, Expected::EndOfLine));
                    }
                    self.doc.arena.push_str(&self.text[run..self.at + quotes - 3]);
                    self.at += quotes;
                    break;
                }
                Some(b'\\') => {
                    self.doc.arena.push_str(&self.text[run..self.at]);
                    self.escape(multi)?;
                    run = self.at;
                }
                Some(b'\n') if multi => self.at += 1,
                Some(b'\r') if multi => {
                    if !self.newline()? {
                        return Err(self.err(self.at, What::BareCarriageReturn));
                    }
                }
                Some(_) => self.text_char()?,
            }
        }
        let len = self.doc.arena.len() - arena_start;
        Ok(Text { at: narrow(arena_start), len: narrow(len), arena: true })
    }

    /// One character of a string that is not a quote, escape or newline: refused if control.
    fn text_char(&mut self) -> Result<(), Error> {
        match self.text[self.at..].chars().next() {
            Some(ch) if is_control(ch) => Err(self.err(self.at, What::Control { ch })),
            Some(ch) => {
                self.at += ch.len_utf8();
                Ok(())
            }
            None => unreachable!("the caller saw a byte"),
        }
    }

    /// The escape whose `\` is at the cursor, decoded into the arena.
    fn escape(&mut self, multi: bool) -> Result<(), Error> {
        let at = self.at;
        let ch = match self.peek_at(1) {
            None => return Err(self.err(at + 1, What::End { expected: Expected::Quote })),
            Some(b'b') => '\u{8}',
            Some(b't') => '\t',
            Some(b'n') => '\n',
            Some(b'f') => '\u{c}',
            Some(b'r') => '\r',
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'u') => return self.unicode(4),
            Some(b'U') => return self.unicode(8),
            Some(b' ' | b'\t' | b'\n' | b'\r') if multi => {
                // A line-ending backslash: blanks, a newline, then every blank and newline go.
                self.at += 1;
                self.spaces();
                if !self.newline()? {
                    return Err(self.err(at + 1, What::BadEscape { found: ' ' }));
                }
                loop {
                    self.spaces();
                    if !self.newline()? {
                        return Ok(());
                    }
                }
            }
            Some(_) => {
                let found = self.text[at + 1..].chars().next().unwrap_or_else(|| unreachable!("a byte is there"));
                return Err(self.err(at + 1, What::BadEscape { found }));
            }
        };
        self.doc.arena.push(ch);
        self.at += 2;
        Ok(())
    }

    /// `\u` and four hex digits or `\U` and eight, at the cursor.
    fn unicode(&mut self, digits: usize) -> Result<(), Error> {
        let start = self.at + 2;
        let mut value: u32 = 0;
        for i in 0..digits {
            let d = match self.b.get(start + i).and_then(|&b| char::from(b).to_digit(16)) {
                Some(d) => d,
                None => return Err(self.unexpected(start + i, Expected::Hex)),
            };
            value = value * 16 + d;
        }
        match char::from_u32(value) {
            Some(ch) => self.doc.arena.push(ch),
            None => return Err(self.err(self.at, What::NotAScalar { value })),
        }
        self.at = start + digits;
        Ok(())
    }

    /// A literal string (`'...'`, or `'''...'''` when `multi`) copied into the arena.
    fn literal(&mut self, multi: bool) -> Result<Text, Error> {
        let arena_start = self.doc.arena.len();
        self.at += if multi { 3 } else { 1 };
        if multi {
            self.newline()?;
        }
        let start = self.at;
        loop {
            match self.peek() {
                None => return Err(self.err(self.at, What::End { expected: Expected::Quote })),
                Some(b'\'') => {
                    if !multi {
                        self.doc.arena.push_str(&self.text[start..self.at]);
                        self.at += 1;
                        break;
                    }
                    let quotes = self.b[self.at..].iter().take_while(|&&b| b == b'\'').count();
                    if quotes < 3 {
                        self.at += quotes;
                        continue;
                    }
                    if quotes > 5 {
                        return Err(self.unexpected(self.at + 5, Expected::EndOfLine));
                    }
                    self.doc.arena.push_str(&self.text[start..self.at + quotes - 3]);
                    self.at += quotes;
                    break;
                }
                Some(b'\n') if multi => self.at += 1,
                Some(b'\r') if multi => {
                    if !self.newline()? {
                        return Err(self.err(self.at, What::BareCarriageReturn));
                    }
                }
                Some(_) => self.text_char()?,
            }
        }
        let len = self.doc.arena.len() - arena_start;
        Ok(Text { at: narrow(arena_start), len: narrow(len), arena: true })
    }

    /// Whether the digits at the cursor start a date (`1979-`) or a time (`07:`).
    fn is_datetime(&self) -> bool {
        let digits = self.b[self.at..].iter().take_while(|b| b.is_ascii_digit()).count();
        matches!((digits, self.peek_at(digits)), (4, Some(b'-')) | (2, Some(b':')))
    }

    /// Exactly `n` digits at the cursor.
    fn digits(&mut self, n: usize) -> Result<u32, Error> {
        let mut value = 0;
        for _ in 0..n {
            match self.peek() {
                Some(b @ b'0'..=b'9') => {
                    value = value * 10 + u32::from(b - b'0');
                    self.at += 1;
                }
                _ => return Err(self.unexpected(self.at, Expected::DatetimePart)),
            }
        }
        Ok(value)
    }

    fn expect(&mut self, byte: u8) -> Result<(), Error> {
        match self.peek() == Some(byte) {
            true => {
                self.at += 1;
                Ok(())
            }
            false => Err(self.unexpected(self.at, Expected::DatetimePart)),
        }
    }

    /// A datetime of any of TOML's four forms, checked for a real date and time.
    fn datetime(&mut self) -> Result<u64, Error> {
        let start = self.at;
        let no_such = |p: &Self| p.err(start, What::NoSuchDatetime);
        let narrow8 = |n: u32| u8::try_from(n).unwrap_or_else(|_| unreachable!("two digits fit"));
        let mut dt = Datetime { date: None, time: None, offset: None };
        let mut want_time = true;
        if self.peek_at(4) == Some(b'-') {
            let year = u16::try_from(self.digits(4)?).unwrap_or_else(|_| unreachable!("four digits fit"));
            self.expect(b'-')?;
            let month = narrow8(self.digits(2)?);
            self.expect(b'-')?;
            let day = narrow8(self.digits(2)?);
            if month == 0 || day == 0 || day > days_in_month(year, month) {
                return Err(no_such(self));
            }
            dt.date = Some(Date { year, month, day });
            want_time = match self.peek() {
                Some(b'T' | b't') => {
                    self.at += 1;
                    true
                }
                Some(b' ')
                    if self.peek_at(1).is_some_and(|b| b.is_ascii_digit())
                        && self.peek_at(2).is_some_and(|b| b.is_ascii_digit())
                        && self.peek_at(3) == Some(b':') =>
                {
                    self.at += 1;
                    true
                }
                _ => false,
            };
        }
        if want_time {
            let hour = narrow8(self.digits(2)?);
            self.expect(b':')?;
            let minute = narrow8(self.digits(2)?);
            self.expect(b':')?;
            let second = narrow8(self.digits(2)?);
            let mut nanosecond = 0;
            if self.peek() == Some(b'.') {
                self.at += 1;
                let digits = self.b[self.at..].iter().take_while(|b| b.is_ascii_digit()).count();
                if digits == 0 {
                    return Err(self.unexpected(self.at, Expected::Digit));
                }
                for (i, &d) in self.b[self.at..self.at + digits].iter().enumerate().take(9) {
                    nanosecond += u32::from(d - b'0') * 10u32.pow(8 - narrow(i));
                }
                self.at += digits;
            }
            if hour > 23 || minute > 59 || second > 60 {
                return Err(no_such(self));
            }
            dt.time = Some(Time { hour, minute, second, nanosecond });
            if dt.date.is_some() {
                dt.offset = match self.peek() {
                    Some(b'Z' | b'z') => {
                        self.at += 1;
                        Some(Offset::Z)
                    }
                    Some(sign @ (b'+' | b'-')) => {
                        self.at += 1;
                        let hours = self.digits(2)?;
                        self.expect(b':')?;
                        let minutes = self.digits(2)?;
                        if hours > 23 || minutes > 59 {
                            return Err(no_such(self));
                        }
                        let total = i16::try_from(hours * 60 + minutes).unwrap_or_else(|_| unreachable!("under a day"));
                        Some(Offset::Minutes(if sign == b'-' { -total } else { total }))
                    }
                    _ => None,
                };
            }
        }
        let index = self.doc.datetimes.len();
        self.doc.datetimes.push(dt);
        Ok(u64::try_from(index).unwrap_or_else(|_| unreachable!("an index fits")))
    }

    /// Digits of `radix` with single `_`s between them, from `i`; the end.
    fn digits_underscored(&self, i: usize, radix: u32) -> Result<usize, Error> {
        let digit = |b: u8| char::from(b).is_digit(radix);
        if !self.b.get(i).copied().is_some_and(digit) {
            return Err(self.unexpected(i, Expected::Digit));
        }
        let mut j = i;
        while let Some(&b) = self.b.get(j) {
            if digit(b) {
                j += 1;
            } else if b == b'_' {
                if !self.b.get(j + 1).copied().is_some_and(digit) {
                    return Err(self.err(j, What::Underscore));
                }
                j += 1;
            } else {
                break;
            }
        }
        Ok(j)
    }

    /// An integer (any radix) or a float (with `inf` and `nan`) at the cursor.
    fn number(&mut self) -> Result<(Kind, u64), Error> {
        let start = self.at;
        let mut i = start;
        let sign = match self.b[i] {
            b @ (b'+' | b'-') => {
                i += 1;
                Some(b)
            }
            _ => None,
        };
        let negative = sign == Some(b'-');
        for (word, value) in [("inf", f64::INFINITY), ("nan", f64::NAN)] {
            if self.b[i..].starts_with(word.as_bytes()) {
                self.at = i + 3;
                let value = if negative { -value } else { value };
                return Ok((Kind::Float, value.to_bits()));
            }
        }
        if sign.is_none() && self.b[i] == b'0' {
            let radix = match self.b.get(i + 1) {
                Some(b'x') => 16,
                Some(b'o') => 8,
                Some(b'b') => 2,
                _ => 0,
            };
            if radix != 0 {
                let end = self.digits_underscored(i + 2, radix)?;
                let mut value: u64 = 0;
                for &b in &self.b[i + 2..end] {
                    let Some(d) = char::from(b).to_digit(radix) else { continue };
                    value = match value.checked_mul(u64::from(radix)).and_then(|v| v.checked_add(u64::from(d))) {
                        Some(v) if v <= i64::MAX.cast_unsigned() => v,
                        _ => return Err(self.err(start, What::IntegerOutOfRange)),
                    };
                }
                self.at = end;
                return Ok((Kind::Integer, value));
            }
        }
        let int_start = i;
        let int_end = self.digits_underscored(i, 10)?;
        if self.b[int_start] == b'0' && int_end - int_start > 1 {
            return Err(self.err(int_start, What::LeadingZero));
        }
        let mut end = int_end;
        let mut float = false;
        if self.b.get(end) == Some(&b'.') {
            float = true;
            end = self.digits_underscored(end + 1, 10)?;
        }
        if let Some(b'e' | b'E') = self.b.get(end) {
            float = true;
            end += 1;
            if let Some(b'+' | b'-') = self.b.get(end) {
                end += 1;
            }
            end = self.digits_underscored(end, 10)?;
        }
        self.at = end;
        if float {
            let mut buf = [0u8; MOST_FLOAT_LEN];
            let mut len = 0;
            for &b in self.b[start..end].iter().filter(|&&b| b != b'_') {
                if len == MOST_FLOAT_LEN {
                    return Err(self.err(start, What::NumberTooLong { most: MOST_FLOAT_LEN }));
                }
                buf[len] = b;
                len += 1;
            }
            let written = core::str::from_utf8(&buf[..len]).unwrap_or_else(|_| unreachable!("ASCII"));
            return match written.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok((Kind::Float, f.to_bits())),
                Ok(_) => Err(self.err(start, What::FloatOutOfRange)),
                Err(error) => unreachable!("{written:?} is a TOML float, which Rust reads: {error}"),
            };
        }
        let mut magnitude: u64 = 0;
        for &b in self.b[int_start..int_end].iter().filter(|&&b| b != b'_') {
            magnitude = match magnitude.checked_mul(10).and_then(|m| m.checked_add(u64::from(b - b'0'))) {
                Some(m) => m,
                None => return Err(self.err(start, What::IntegerOutOfRange)),
            };
        }
        let value = match negative {
            true => 0i64.checked_sub_unsigned(magnitude),
            false => i64::try_from(magnitude).ok(),
        };
        match value {
            Some(n) => Ok((Kind::Integer, n.cast_unsigned())),
            None => Err(self.err(start, What::IntegerOutOfRange)),
        }
    }
}
