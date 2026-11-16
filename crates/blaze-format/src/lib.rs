//! blaze v2's data format (docs/v2/plan/design/data.md section 5): **the bytes in memory are the
//! bytes on disk**.
//!
//! The format itself (records, their views, [`Writer`], [`record!`]) is blaze-format-core's,
//! on `core` alone so a `#![no_std]` crate defines and reads records; this crate re-exports all
//! of it at the paths it always had, and adds what needs `std`: the [`cursor`], [`hex`] and
//! [`leb`] codecs, and the span side table ([`Spans`]). It brings no third-party package:
//! compression by tier, which needs zstd, is blaze-format-tiers (it was `tiers` here until
//! 2026-11-15). See blaze-format-core's crate docs for the format.
//!
//! Spans are never in a record that is hashed: they sit in a side table ([`Spans`]), and a
//! record names one by [`SpanRef`].

pub mod cursor;
pub mod hex;
pub mod leb;

use std::mem::offset_of;

use blaze_ids::ContentId;

// Every public item of blaze-format-core, named one by one: a path of this crate is a promise
// to the hundred crates that use it, so none appears or goes by a glob.
pub use blaze_format_core::{
    ALIGN, Aligned, BUFFER_DOMAIN, FormatError, HEADER_LEN, Header, Items, MAGIC, Offset, Own,
    Part, Persist, Placed, Record, Rel, RelSpan, SCHEMA_DOMAIN, SchemaHash, Slice, SpanRef, Str,
    Text, Unfit, Verified, WordTable, WordTables, WriteBuffer, Writer, WriterIn, buffer_id,
    bytes_of_slice, check_field, check_field_shallow, declared, field, field_columns, header,
    open, open_as, open_verified, read_verified, record, record_at, row_words, schema, schema_of,
    to_bytes,
};

record! {
    /// A [`Spans`] buffer's root; public for blaze-format-tiers' `spans_columns`.
    #[derive(Clone, Copy)]
    pub struct SpansRoot: 1 {
        anchor: ContentId,
        spans: Slice<RelSpan>,
    }
}

/// The span side table of one anchor: what a [`SpanRef`] of that anchor indexes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Spans {
    pub anchor: ContentId,
    pub spans: Vec<RelSpan>,
}

impl Spans {
    pub fn new(anchor: ContentId) -> Spans {
        // alloc: `Vec::new` allocates nothing until a span is pushed
        Spans { anchor, spans: Vec::new() }
    }

    /// Adds a span `len` bytes long, `start` bytes from the anchor's start.
    pub fn push(&mut self, start: u32, len: u32) -> SpanRef {
        let index = u32::try_from(self.spans.len()).expect("an anchor of 2^32 spans");
        self.spans.push(RelSpan { start, len });
        SpanRef { anchor: self.anchor, index }
    }

    /// The span `span` names, `None` for a span of another anchor or past this table.
    pub fn get(&self, span: SpanRef) -> Option<RelSpan> {
        match span.anchor == self.anchor {
            true => self.spans.get(span.index as usize).copied(),
            false => None,
        }
    }
}

/// [`Spans`] read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpansRef<'a> {
    pub anchor: ContentId,
    pub spans: &'a [RelSpan],
}

impl SpansRef<'_> {
    pub fn get(&self, span: SpanRef) -> Option<RelSpan> {
        match span.anchor == self.anchor {
            true => self.spans.get(span.index as usize).copied(),
            false => None,
        }
    }

    pub fn to_spans(&self) -> Spans {
        // alloc: an owned copy, asked for by name
        Spans { anchor: self.anchor, spans: self.spans.to_vec() }
    }
}

impl Persist for Spans {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("Spans"), Part::U64(1), Part::Schema(SpansRoot::SCHEMA)]);
    type Ref<'a> = SpansRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let spans = out.push_slice(&self.spans);
        let at = out.next::<SpansRoot>();
        let field = at + offset_of!(SpansRoot, spans) as u64;
        let root = SpansRoot {
            anchor: self.anchor,
            spans: Slice::between(field, spans, self.spans.len()),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<SpansRef<'_>, FormatError> {
        let root = open_as::<SpansRoot>(buffer, Self::SCHEMA)?;
        Ok(SpansRef { anchor: root.anchor, spans: root.spans.follow(buffer)? })
    }
}

#[cfg(test)]
mod placed_tests;
#[cfg(test)]
mod tests;
