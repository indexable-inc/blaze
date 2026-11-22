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
//!
//! # `core` only, and the `std` feature
//!
//! The crate is `#![no_std]`. What it writes it writes into a buffer in the caller's allocator
//! (`blaze_own::Vec<u8, A>`: [`Writer`]`<`[`Own`]`<A>>`, `leb::write_u64_in`, `hex::encode_in`,
//! [`cursor::Put`] on `blaze_own::Vec`), and [`Persist`] takes its writer explicitly. The `std`
//! feature, on by default until every caller is ported (D170), keeps the deprecated forms on
//! the global heap: [`Spans`] with `SpansRef::to_spans`, `Aligned`,
//! `Writer::on_the_global_heap_until_ported`, `hex::encode`/`encode_into`,
//! `leb::write_u64`/`write_i64`, `Put` for `std`'s `Vec<u8>` and `From<cursor::Error>` for
//! `String`.
// Tests use std's collections and printing; the crate itself sees `std` only under its feature.
#![cfg_attr(not(test), no_std)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

#[cfg(all(feature = "std", not(test)))]
extern crate std;

pub mod cursor;
pub mod hex;
pub mod leb;

#[cfg(feature = "std")]
use core::mem::offset_of;

use blaze_ids::ContentId;

// Every public item of blaze-format-core, named one by one: a path of this crate is a promise
// to the hundred crates that use it, so none appears or goes by a glob.
pub use blaze_format_core::{
    ALIGN, AlignedIn, BUFFER_DOMAIN, FormatError, HEADER_LEN, Header, Items, MAGIC, Offset, Own,
    Part, Persist, Placed, Record, Rel, RelSpan, SCHEMA_DOMAIN, SchemaHash, Slice, SpanRef, Str,
    Text, Unfit, Verified, WRITER_CAPACITY, WordTable, WordTables, WriteBuffer, Writer, WriterIn,
    WriterOpts, buffer_id, bytes_of_slice, check_field, check_field_shallow, declared, field,
    field_columns, header, open, open_as, open_verified, read_verified, record, record_at, reopen,
    row_words, rows, schema, schema_of, to_bytes,
};
/// DEPRECATED (the `std` feature): a copy on the global heap; `AlignedIn` takes the allocator.
#[cfg(feature = "std")]
pub use blaze_format_core::Aligned;

record! {
    /// A [`Spans`] buffer's root; public for blaze-format-tiers' `spans_columns`.
    #[derive(Clone, Copy)]
    pub struct SpansRoot: 1 {
        anchor: ContentId,
        spans: Slice<RelSpan>,
    }
}

/// The span side table of one anchor: what a [`SpanRef`] of that anchor indexes.
///
/// DEPRECATED (the `std` feature): its spans are on the global heap. To remove: its writers hold
/// a `blaze_own::Vec<RelSpan, A>` and write a [`SpansRoot`] themselves (bstd move); reading needs
/// none of it ([`SpansRef`]).
#[cfg(feature = "std")]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Spans {
    pub anchor: ContentId,
    pub spans: std::vec::Vec<RelSpan>,
}

#[cfg(feature = "std")]
impl Spans {
    pub fn new(anchor: ContentId) -> Spans {
        // alloc: `Vec::new` allocates nothing until a span is pushed
        Spans { anchor, spans: std::vec::Vec::new() }
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

    /// DEPRECATED (the `std` feature): an owned copy on the global heap, asked for by name.
    #[cfg(feature = "std")]
    pub fn to_spans(&self) -> Spans {
        // alloc: an owned copy, asked for by name
        Spans { anchor: self.anchor, spans: self.spans.to_vec() }
    }
}

/// The schema a [`Spans`] buffer is written under: the same with or without the `std` feature.
pub const SPANS_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("Spans"), Part::U64(1), Part::Schema(SpansRoot::SCHEMA)]);

/// A [`Spans`] buffer read in place: with no `std` feature too.
pub fn read_spans(buffer: &[u8]) -> Result<SpansRef<'_>, FormatError> {
    let root = open_as::<SpansRoot>(buffer, SPANS_SCHEMA)?;
    Ok(SpansRef { anchor: root.anchor, spans: root.spans.follow(buffer)? })
}

#[cfg(feature = "std")]
impl Persist for Spans {
    const SCHEMA: SchemaHash = SPANS_SCHEMA;
    type Ref<'a> = SpansRef<'a>;

    fn write<P: WriterIn>(&self, out: &mut Writer<P>) -> u64 {
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
        read_spans(buffer)
    }
}

#[cfg(test)]
mod own_tests;
#[cfg(test)]
mod placed_tests;
#[cfg(test)]
mod tests;
