//! Edits that keep every byte they do not name: each is a splice of the text at a node's byte
//! range, and [`Edit::finish`] applies them all at once to a copy.
//!
//! The rule for a new entry: in a table with its own header (or the root), it goes on its own
//! line just after the section's last entry; in an inline table, after its last entry. A table
//! made only by keys (`[a.b]`'s `a`, or `a.b = 1`'s `a`) has no one place an entry belongs, so
//! setting a key in it is refused, as is setting a key that holds a table. A new array element
//! follows the array's layout: on its own line, indented like the others, in a multi-line
//! array; after `, ` in a one-line array; a trailing comma stays a trailing comma.

use core::{alloc::Allocator, fmt};

use blaze_data::Value;
use blaze_own::{String, Vec};
use blaze_text::{Dbg, DebugForm, Put, Show, Shown, Writes as _};

use crate::{
    doc::{Document, Item, Kind, NONE, Origin, low, widen},
    write::{WriteError, write_inline, write_key},
};

/// Why an edit was refused.
pub enum EditWhat<B: Allocator> {
    /// The item is from another document.
    OtherDocument,
    NotATable { kind: Kind },
    NotAnArray { kind: Kind },
    /// The key holds a table; set its keys instead.
    IsATable,
    /// The table or array has no one place to edit: one made only by keys, or an array of
    /// tables (whose elements are headers).
    NotEditable { origin: Origin },
    Index { index: usize, len: usize },
    /// Two edits touch one range.
    Overlap { at: usize },
    /// The value has no TOML form.
    Value(WriteError<B>),
}

/// An edit refused: the byte offset of what it named, and why.
pub struct EditError<B: Allocator> {
    pub at: usize,
    pub what: EditWhat<B>,
}

/// `the edit at byte N: why`.
impl<B: Allocator> Show for EditError<B> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.str("the edit at byte ").int(self.at).str(": ");
        match &self.what {
            EditWhat::OtherDocument => out.str("the item is from another document"),
            EditWhat::NotATable { kind } => out.str(kind.name()).str(" is not a table"),
            EditWhat::NotAnArray { kind } => out.str(kind.name()).str(" is not an array"),
            EditWhat::IsATable => out.str("the key holds a table; set the keys inside it"),
            EditWhat::NotEditable { origin } => out.str("a table or array of origin ").debug(origin).str(" has no one place to edit"),
            EditWhat::Index { index, len } => out.str("index ").int(*index).str(" is past the array's ").int(*len).str(" elements"),
            EditWhat::Overlap { at } => out.str("it overlaps another edit at byte ").int(*at),
            EditWhat::Value(error) => out.show(error),
        };
    }
}

/// `EditError(the edit at byte N: why)`.
impl<B: Allocator> DebugForm for EditError<B> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.str("EditError(").show(self).byte(b')');
    }
}

/// The bridges `core::error::Error` needs: the text is [`Show`]'s and [`DebugForm`]'s.
impl<B: Allocator> fmt::Display for EditError<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl<B: Allocator> fmt::Debug for EditError<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(Dbg(self)), f)
    }
}

impl<B: Allocator> core::error::Error for EditError<B> {}

struct Splice<B: Allocator> {
    /// The order it was asked in, which orders two inserts at one place.
    seq: usize,
    at: usize,
    end: usize,
    text: String<B>,
}

/// Edits to one [`Document`], applied together by [`Edit::finish`].
pub struct Edit<'d, 't, A: Allocator, B: Allocator + Clone> {
    doc: &'d Document<'t, A>,
    splices: Vec<Splice<B>, B>,
    alloc: B,
}

fn blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

impl<'d, 't, A: Allocator, B: Allocator + Clone> Edit<'d, 't, A, B> {
    /// No edits yet to `doc`; the edits' texts and the result in `alloc`.
    pub fn new_in(doc: &'d Document<'t, A>, alloc: B) -> Edit<'d, 't, A, B> {
        Edit { doc, splices: Vec::new_in(alloc.clone()), alloc }
    }

    fn mine(&self, item: &Item<'_, '_, A>) -> Result<(), EditError<B>> {
        match core::ptr::eq(item.doc, self.doc) {
            true => Ok(()),
            false => Err(EditError { at: item.span().start, what: EditWhat::OtherDocument }),
        }
    }

    fn text_of<V: Allocator>(&self, value: &Value<V>, at: usize) -> Result<String<B>, EditError<B>> {
        let mut text = String::new_in(self.alloc.clone());
        write_inline(value, &mut text).map_err(|error| EditError { at, what: EditWhat::Value(error) })?;
        Ok(text)
    }

    fn splice(&mut self, at: usize, end: usize, text: String<B>) {
        let seq = self.splices.len();
        self.splices.push(Splice { seq, at, end, text });
    }

    /// The value at `item` becomes `value`; `item` must be a value, an inline table or a
    /// `[ ]` array.
    pub fn replace<V: Allocator>(&mut self, item: Item<'_, '_, A>, value: &Value<V>) -> Result<(), EditError<B>> {
        self.mine(&item)?;
        let span = item.span();
        match item.origin() {
            Origin::Value | Origin::Inline => {}
            origin => return Err(EditError { at: span.start, what: EditWhat::NotEditable { origin } }),
        }
        let text = self.text_of(value, span.start)?;
        self.splice(span.start, span.end, text);
        Ok(())
    }

    /// `key = value` in `table`: the old value replaced if there is one, else a new entry by
    /// the rule in this module's docs.
    pub fn set<V: Allocator>(&mut self, table: Item<'_, '_, A>, key: &str, value: &Value<V>) -> Result<(), EditError<B>> {
        self.mine(&table)?;
        let span = table.span();
        if !table.is_table() {
            return Err(EditError { at: span.start, what: EditWhat::NotATable { kind: table.kind() } });
        }
        if let Some(old) = table.get(key) {
            return match old.origin() {
                Origin::Value | Origin::Inline => self.replace(old, value),
                _ => Err(EditError { at: old.key_span().start, what: EditWhat::IsATable }),
            };
        }
        let mut text = String::new_in(self.alloc.clone());
        match table.origin() {
            Origin::Header => {
                let at = crate::doc::widen(crate::doc::low(table.payload()));
                let source = self.doc.text();
                if at == source.len() && !source.is_empty() && !source.ends_with('\n') {
                    text.push('\n');
                }
                write_key(key, &mut text);
                text.push_str(" = ");
                write_inline(value, &mut text).map_err(|error| EditError { at, what: EditWhat::Value(error) })?;
                text.push('\n');
                self.splice(at, at, text);
            }
            Origin::Inline => {
                let bytes = self.doc.text().as_bytes();
                let mut at = span.end - 1;
                while at > span.start + 1 && blank(bytes[at - 1]) {
                    at -= 1;
                }
                let empty = table.is_empty();
                let start = if empty { span.start } else { at };
                text.push_str(if empty { "{ " } else { ", " });
                write_key(key, &mut text);
                text.push_str(" = ");
                write_inline(value, &mut text).map_err(|error| EditError { at, what: EditWhat::Value(error) })?;
                match empty {
                    true => {
                        text.push_str(" }");
                        self.splice(start, span.end, text);
                    }
                    false => self.splice(at, at, text),
                }
            }
            origin => return Err(EditError { at: span.start, what: EditWhat::NotEditable { origin } }),
        }
        Ok(())
    }

    /// `value` appended to `array`.
    pub fn push<V: Allocator>(&mut self, array: Item<'_, '_, A>, value: &Value<V>) -> Result<(), EditError<B>> {
        let len = array.len();
        self.insert(array, len, value)
    }

    /// `value` put into `array` before element `index` (at `len`, appended).
    pub fn insert<V: Allocator>(&mut self, array: Item<'_, '_, A>, index: usize, value: &Value<V>) -> Result<(), EditError<B>> {
        self.mine(&array)?;
        let span = array.span();
        if !array.is_array() {
            return Err(EditError { at: span.start, what: EditWhat::NotAnArray { kind: array.kind() } });
        }
        if array.origin() != Origin::Value {
            return Err(EditError { at: span.start, what: EditWhat::NotEditable { origin: array.origin() } });
        }
        let len = array.len();
        if index > len {
            return Err(EditError { at: span.start, what: EditWhat::Index { index, len } });
        }
        let item = self.text_of(value, span.start)?;
        let source = self.doc.text();
        let bytes = source.as_bytes();
        let Some(last) = array.at(len.wrapping_sub(1)) else {
            let mut text = String::new_in(self.alloc.clone());
            text.push('[');
            text.push_str(item.as_str());
            text.push(']');
            self.splice(span.start, span.end, text);
            return Ok(());
        };
        // The layout, from the element `index` names or else the last: its line's indent when
        // it starts its line.
        let model = array.at(index).unwrap_or(last).span().start;
        let line_start = bytes[..model].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let multi = bytes[line_start..model].iter().all(|&b| blank(b)) && line_start > span.start;
        let indent = &source[line_start..model];
        let mut text = String::new_in(self.alloc.clone());
        if index < len {
            text.push_str(item.as_str());
            match multi {
                true => {
                    text.push_str(",\n");
                    text.push_str(indent);
                }
                false => text.push_str(", "),
            }
            self.splice(model, model, text);
            return Ok(());
        }
        let end = last.span().end;
        match trailing_comma(bytes, end, span.end) {
            Some(comma) => {
                match multi {
                    true => {
                        text.push('\n');
                        text.push_str(indent);
                    }
                    false => text.push(' '),
                }
                text.push_str(item.as_str());
                text.push(',');
                self.splice(comma + 1, comma + 1, text);
            }
            None => {
                text.push(',');
                match multi {
                    true => {
                        text.push('\n');
                        text.push_str(indent);
                    }
                    false => text.push(' '),
                }
                text.push_str(item.as_str());
                self.splice(end, end, text);
            }
        }
        Ok(())
    }

    /// `item` and everything under it gone, every other byte kept:
    /// - a key/value outside inline tables: its whole line, with a comment after the value;
    /// - a key/value in an inline table: the entry and one comma beside it (`{}` when it was
    ///   the only one);
    /// - a table made by headers or by dotted keys, an array of tables or an element of one:
    ///   the section of every header at or under it (from the header's line through its last
    ///   entry's line) and the line of every key/value under it outside those sections.
    ///
    /// A table made by dotted keys has no text but its keys: removing `b` of `a.b = 1`, the
    /// only key under `a`, removes `a` too. A table with a header stays, empty.
    ///
    /// Refused: the root, an array's element, and a table made by dotted keys inside an inline
    /// table, or a key under one (`x.y` of `{ x.y = 1 }`), whose entries need not be together.
    pub fn remove(&mut self, item: Item<'_, '_, A>) -> Result<(), EditError<B>> {
        self.mine(&item)?;
        let at = item.span().start;
        let Some(parent) = item.parent() else {
            return Err(EditError { at, what: EditWhat::NotEditable { origin: item.origin() } });
        };
        let mut enclosing = parent;
        while enclosing.kind() == Kind::Table && enclosing.origin() == Origin::Dotted {
            match enclosing.parent() {
                Some(up) => enclosing = up,
                None => break,
            }
        }
        match (parent.kind(), enclosing.origin()) {
            // An array's element: arrays written `[ ]` are closed, and an element of an array
            // of tables is a table with a header (`Origin::Header` on the array).
            (Kind::Array, Origin::Value) => Err(EditError { at, what: EditWhat::NotEditable { origin: Origin::Value } }),
            (_, Origin::Inline) => match parent.origin() == Origin::Inline && item.origin() != Origin::Dotted {
                true => {
                    self.remove_inline_entry(item, parent);
                    Ok(())
                }
                false => Err(EditError { at, what: EditWhat::NotEditable { origin: Origin::Dotted } }),
            },
            _ => {
                self.remove_lines(item);
                Ok(())
            }
        }
    }

    /// An inline table's entry and a comma beside it.
    fn remove_inline_entry(&mut self, item: Item<'_, '_, A>, table: Item<'_, '_, A>) {
        let nodes = &self.doc.nodes;
        let bytes = self.doc.text().as_bytes();
        let start = item.key_span().start;
        let next = nodes.next_siblings()[widen(item.id)];
        let empty = String::new_in(self.alloc.clone());
        if next != NONE {
            // Up to the next entry's first key: the comma and the blanks after it.
            let until = nodes.key_spans()[widen(next)].range().start;
            self.splice(start, until, empty);
        } else if nodes.first_children()[widen(table.id)] != item.id {
            // The last of several: from the comma before it.
            let mut comma = start;
            while comma > 0 && blank(bytes[comma - 1]) {
                comma -= 1;
            }
            self.splice(comma - 1, item.span().end, empty);
        } else {
            let mut text = empty;
            text.push_str("{}");
            let span = table.span();
            self.splice(span.start, span.end, text);
        }
    }

    /// The section of every header table at or under `item` and the line of every key/value
    /// at or under it, walked by the node columns without recursion; a line inside a removed
    /// section goes with the section.
    fn remove_lines(&mut self, item: Item<'_, '_, A>) {
        let nodes = &self.doc.nodes;
        let bytes = self.doc.text().as_bytes();
        let top = item.id;
        let mut at = top;
        let mut ranges = Vec::new_in(self.alloc.clone());
        loop {
            let i = widen(at);
            let (kind, origin) = (nodes.kinds()[i], nodes.origins()[i]);
            let span = nodes.spans()[i].range();
            match origin {
                Origin::Header if kind == Kind::Table => {
                    ranges.push((line_start(bytes, span.start), widen(low(nodes.payloads()[i]))));
                }
                // A key/value starts its line (TOML puts each on its own line), after blanks;
                // its key's first part is where the line's text starts.
                Origin::Value | Origin::Inline => {
                    ranges.push((line_start(bytes, nodes.key_spans()[i].range().start), line_end(bytes, span.end)));
                }
                Origin::Header | Origin::Implicit | Origin::Dotted => {}
            }
            // Headers and key/values sit under tables and arrays of tables made by headers or
            // keys, never under a value or an inline table.
            let into = matches!(kind, Kind::Table | Kind::Array) && !matches!(origin, Origin::Value | Origin::Inline);
            let first = nodes.first_children()[i];
            if into && first != NONE {
                at = first;
                continue;
            }
            loop {
                if at == top {
                    ranges.sort_unstable();
                    let mut done = 0;
                    for (start, end) in ranges.iter() {
                        if *start < done {
                            // Inside a section already removed.
                            continue;
                        }
                        self.splice(*start, *end, String::new_in(self.alloc.clone()));
                        done = *end;
                    }
                    return;
                }
                let next = nodes.next_siblings()[widen(at)];
                if next != NONE {
                    at = next;
                    break;
                }
                at = nodes.parents()[widen(at)];
            }
        }
    }

    /// The text with every edit applied, in the edits' allocator.
    pub fn finish(mut self) -> Result<String<B>, EditError<B>> {
        self.splices.sort_unstable_by_key(|s| (s.at, s.seq));
        let source = self.doc.text();
        let grow: usize = self.splices.iter().map(|s| s.text.len()).sum();
        let mut out = String::with_capacity_in(source.len() + grow, self.alloc.clone());
        let mut done = 0;
        for splice in self.splices.iter() {
            if splice.at < done {
                return Err(EditError { at: splice.at, what: EditWhat::Overlap { at: done } });
            }
            out.push_str(&source[done..splice.at]);
            out.push_str(splice.text.as_str());
            done = splice.end;
        }
        out.push_str(&source[done..]);
        Ok(out)
    }
}

/// The start of the line `at` is on.
fn line_start(bytes: &[u8], at: usize) -> usize {
    bytes[..at].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
}

/// Past the rest of the line from `from`: blanks, a comment, and the newline.
fn line_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && blank(bytes[i]) {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'#' {
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
    }
    if i < bytes.len() && bytes[i] == b'\r' {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'\n' {
        i += 1;
    }
    i
}

/// Where the comma after an array's last element is, if it has one: past blanks, comments and
/// newlines from `from`, before `close` (the array's end).
fn trailing_comma(bytes: &[u8], from: usize, close: usize) -> Option<usize> {
    let mut i = from;
    while i < close {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'#' => {
                while i < close && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b',' => return Some(i),
            _ => return None,
        }
    }
    None
}
