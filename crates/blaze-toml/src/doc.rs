//! The [`Document`]: one node per table, array and value, in columns, each with the byte range
//! of its key and of itself in the text.
//!
//! The node columns (an `Soa`) are what a lookup and a walk read: a node's kind, its parent,
//! its first and last child and next sibling (children in the order the text names them), its
//! key as a range of the text or of the arena (a quoted key with escapes is decoded there), its
//! key's and its own byte range, and one word of payload (an integer's or a float's bits, a
//! bool, a decoded string's range in the arena, a datetime's index, a section's body end).

use core::{alloc::Allocator, fmt, ops::Range};

use blaze_own::{String, Vec};
use blaze_text::{Dbg, DebugForm, Put, Show, Shown, Writes as _};

/// No node: the end of a chain.
pub(crate) const NONE: u32 = u32::MAX;

/// What a node is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A table: `[a]`, one made by `[a.b]` or `a.b = 1`, an inline `{ }`, or an element of
    /// an array of tables.
    Table,
    /// `[1, 2]`, or the array `[[a]]` makes.
    Array,
    String,
    Integer,
    Float,
    Bool,
    Datetime,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Table => "a table",
            Kind::Array => "an array",
            Kind::String => "a string",
            Kind::Integer => "an integer",
            Kind::Float => "a float",
            Kind::Bool => "a bool",
            Kind::Datetime => "a datetime",
        }
    }
}

/// How a table or array came to be, which decides what may add to it later (TOML 1.0's
/// rules, as makepad/relax's reader states them).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Named only on the way to another: `a` in `[a.b]`. A later `[a]` may define it once.
    Implicit,
    /// Defined by its own header `[a]`; for an array, made by `[[a]]` (and each element too).
    Header,
    /// Made by a dotted key `a.b = 1`: more dotted keys may add to it, a header may not
    /// define it, a header may name it on the way to a sub-table.
    Dotted,
    /// `{ }`: closed once written.
    Inline,
    /// A value, or an array written `[ ]`: closed once written.
    Value,
}

blaze_text::debug_variants!(Origin { Implicit, Header, Dotted, Inline, Value });

/// Text that is either a range of the source or of the arena.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Text {
    pub(crate) at: u32,
    pub(crate) len: u32,
    pub(crate) arena: bool,
}

impl Text {
    pub(crate) const EMPTY: Text = Text { at: 0, len: 0, arena: false };
}

/// A byte range of the source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Span {
    pub(crate) start: u32,
    pub(crate) end: u32,
}

impl Span {
    pub(crate) fn range(self) -> Range<usize> {
        widen(self.start)..widen(self.end)
    }
}

pub(crate) fn widen(n: u32) -> usize {
    match usize::try_from(n) {
        Ok(n) => n,
        Err(_) => unreachable!("blaze's targets have a `usize` of 32 bits or more"),
    }
}

/// A source offset as stored; the reader has checked the text fits.
pub(crate) fn narrow(n: usize) -> u32 {
    match u32::try_from(n) {
        Ok(n) => n,
        Err(_) => unreachable!("the reader refuses a text past `u32::MAX` bytes"),
    }
}

blaze_own::soa! {
    pub(crate) struct Node, Nodes, NodeRef {
        pub(crate) kind: Kind => kinds / kinds_mut,
        pub(crate) origin: Origin => origins / origins_mut,
        pub(crate) parent: u32 => parents / parents_mut,
        pub(crate) key: Text => keys / keys_mut,
        pub(crate) key_span: Span => key_spans / key_spans_mut,
        pub(crate) span: Span => spans / spans_mut,
        pub(crate) payload: u64 => payloads / payloads_mut,
        pub(crate) first_child: u32 => first_children / first_children_mut,
        pub(crate) last_child: u32 => last_children / last_children_mut,
        pub(crate) next_sibling: u32 => next_siblings / next_siblings_mut,
    }
}

/// A calendar date.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

/// A time of day; a fraction past nanoseconds is cut, as TOML allows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Time {
    pub hour: u8,
    pub minute: u8,
    /// Up to 60, for a leap second.
    pub second: u8,
    pub nanosecond: u32,
}

/// A UTC offset.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Offset {
    /// `Z`.
    Z,
    /// `+05:30` is 330.
    Minutes(i16),
}

/// TOML's four datetime forms: offset date-time (all three), local date-time (no offset),
/// local date, local time.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Datetime {
    pub date: Option<Date>,
    pub time: Option<Time>,
    pub offset: Option<Offset>,
}

/// RFC 3339's form: `1979-05-27T07:32:00.5-07:00`, the fraction without trailing zeros.
impl Show for Datetime {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        if let Some(Date { year, month, day }) = self.date {
            out.int_zeros(year, 4).byte(b'-').int_zeros(month, 2).byte(b'-').int_zeros(day, 2);
            if self.time.is_some() {
                out.byte(b'T');
            }
        }
        if let Some(Time { hour, minute, second, nanosecond }) = self.time {
            out.int_zeros(hour, 2).byte(b':').int_zeros(minute, 2).byte(b':').int_zeros(second, 2);
            if nanosecond > 0 {
                let mut digits = 9;
                let mut n = nanosecond;
                while n % 10 == 0 {
                    n /= 10;
                    digits -= 1;
                }
                out.byte(b'.').int_zeros(n, digits);
            }
        }
        match self.offset {
            None => {}
            Some(Offset::Z) => _ = out.byte(b'Z'),
            Some(Offset::Minutes(m)) => {
                let sign = if m < 0 { b'-' } else { b'+' };
                let m = m.unsigned_abs();
                out.byte(sign).int_zeros(m / 60, 2).byte(b':').int_zeros(m % 60, 2);
            }
        }
    }
}

/// The bridge for `{}` users: the text is [`Show`]'s.
impl fmt::Display for Datetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

/// A TOML text read: its nodes, the decoded strings, and the datetimes, over the text itself.
pub struct Document<'t, A: Allocator> {
    pub(crate) text: &'t str,
    pub(crate) nodes: Nodes<A>,
    pub(crate) arena: String<A>,
    pub(crate) datetimes: Vec<Datetime, A>,
}

impl<'t, A: Allocator> Document<'t, A> {
    /// The text read.
    pub fn text(&self) -> &'t str {
        self.text
    }

    /// The root table.
    pub fn root(&self) -> Item<'_, 't, A> {
        Item { doc: self, id: 0 }
    }

    /// How many nodes: tables, arrays and values.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub(crate) fn text_of(&self, text: Text) -> &str {
        let range = widen(text.at)..widen(text.at) + widen(text.len);
        match text.arena {
            true => &self.arena.as_str()[range],
            false => &self.text[range],
        }
    }

    pub(crate) fn child(&self, parent: u32, key: &str) -> Option<u32> {
        let mut at = self.nodes.first_children()[widen(parent)];
        while at != NONE {
            if self.text_of(self.nodes.keys()[widen(at)]) == key {
                return Some(at);
            }
            at = self.nodes.next_siblings()[widen(at)];
        }
        None
    }
}

/// One node of a [`Document`], to read.
pub struct Item<'d, 't, A: Allocator> {
    pub(crate) doc: &'d Document<'t, A>,
    pub(crate) id: u32,
}

impl<A: Allocator> Clone for Item<'_, '_, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A: Allocator> Copy for Item<'_, '_, A> {}

impl<'d, 't, A: Allocator> Item<'d, 't, A> {
    fn i(&self) -> usize {
        widen(self.id)
    }

    pub fn kind(&self) -> Kind {
        self.doc.nodes.kinds()[self.i()]
    }

    pub fn origin(&self) -> Origin {
        self.doc.nodes.origins()[self.i()]
    }

    /// The key it is under, decoded; empty for the root and an array's element.
    pub fn key(&self) -> &'d str {
        self.doc.text_of(self.doc.nodes.keys()[self.i()])
    }

    /// The byte range of its key as written (for a header, the header's last key); empty for
    /// the root and an array's element.
    pub fn key_span(&self) -> Range<usize> {
        self.doc.nodes.key_spans()[self.i()].range()
    }

    /// Its byte range: a value's text; a header table's `[a.b]` or `[[a.b]]`; an inline
    /// table's or an array's brackets and all; for a table named only by keys, its first key.
    pub fn span(&self) -> Range<usize> {
        self.doc.nodes.spans()[self.i()].range()
    }

    /// The 1-based line and column of its start.
    pub fn line_col(&self) -> (usize, usize) {
        crate::line_col(self.doc.text, self.span().start)
    }

    pub(crate) fn payload(&self) -> u64 {
        self.doc.nodes.payloads()[self.i()]
    }

    pub fn parent(&self) -> Option<Item<'d, 't, A>> {
        let parent = self.doc.nodes.parents()[self.i()];
        (parent != NONE).then_some(Item { doc: self.doc, id: parent })
    }

    pub fn as_str(&self) -> Option<&'d str> {
        (self.kind() == Kind::String).then(|| {
            let p = self.payload();
            let text = Text { at: high(p), len: low(p), arena: true };
            self.doc.text_of(text)
        })
    }

    pub fn as_integer(&self) -> Option<i64> {
        (self.kind() == Kind::Integer).then(|| self.payload().cast_signed())
    }

    pub fn as_float(&self) -> Option<f64> {
        (self.kind() == Kind::Float).then(|| f64::from_bits(self.payload()))
    }

    pub fn as_bool(&self) -> Option<bool> {
        (self.kind() == Kind::Bool).then(|| self.payload() != 0)
    }

    pub fn as_datetime(&self) -> Option<&'d Datetime> {
        match self.kind() {
            Kind::Datetime => match usize::try_from(self.payload()) {
                Ok(index) => self.doc.datetimes.get(index),
                Err(_) => unreachable!("a datetime's index fits"),
            },
            _ => None,
        }
    }

    pub fn is_table(&self) -> bool {
        self.kind() == Kind::Table
    }

    pub fn is_array(&self) -> bool {
        self.kind() == Kind::Array
    }

    /// The entry `key` of a table.
    pub fn get(&self, key: &str) -> Option<Item<'d, 't, A>> {
        match self.kind() {
            Kind::Table => self.doc.child(self.id, key).map(|id| Item { doc: self.doc, id }),
            _ => None,
        }
    }

    /// A table's entries or an array's elements, in the order the text names them.
    pub fn children(&self) -> Children<'d, 't, A> {
        Children { doc: self.doc, at: self.doc.nodes.first_children()[self.i()] }
    }

    /// How many entries or elements.
    pub fn len(&self) -> usize {
        self.children().count()
    }

    pub fn is_empty(&self) -> bool {
        self.doc.nodes.first_children()[self.i()] == NONE
    }

    /// The element `index` of an array.
    pub fn at(&self, index: usize) -> Option<Item<'d, 't, A>> {
        match self.kind() {
            Kind::Array => self.children().nth(index),
            _ => None,
        }
    }
}

/// `Item(table "key" at 3..9)`: its own debug form, in place of reflection's reading of the
/// document it points into.
impl<A: Allocator> DebugForm for Item<'_, '_, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.str("Item(").str(self.kind().name()).byte(b' ').debug(self.key()).str(" at ").debug(&self.span()).byte(b')');
    }
}

/// The bridge for `{:?}` users (`assert_eq!`, `unwrap`): the debug form above.
impl<A: Allocator> fmt::Debug for Item<'_, '_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(Dbg(self)), f)
    }
}

/// The children of an [`Item`].
pub struct Children<'d, 't, A: Allocator> {
    doc: &'d Document<'t, A>,
    at: u32,
}

impl<'d, 't, A: Allocator> Iterator for Children<'d, 't, A> {
    type Item = Item<'d, 't, A>;

    fn next(&mut self) -> Option<Item<'d, 't, A>> {
        if self.at == NONE {
            return None;
        }
        let item = Item { doc: self.doc, id: self.at };
        self.at = self.doc.nodes.next_siblings()[widen(self.at)];
        Some(item)
    }
}

pub(crate) fn pack(at: u32, len: u32) -> u64 {
    (u64::from(at) << 32) | u64::from(len)
}

pub(crate) fn high(p: u64) -> u32 {
    match u32::try_from(p >> 32) {
        Ok(n) => n,
        Err(_) => unreachable!("the high half of a word fits"),
    }
}

pub(crate) fn low(p: u64) -> u32 {
    match u32::try_from(p & u64::from(u32::MAX)) {
        Ok(n) => n,
        Err(_) => unreachable!("the low half of a word fits"),
    }
}
