//! A body record's payload: the body's steps as the item store encodes them today (inference,
//! the built MIR of each owner, the borrow check's verdict, a constant's value), one section a
//! step, each the step's answer bytes in the item store's own encoding, which names items by
//! `BodyFacts::def_key` and holds no `Gid` and no position.
//!
//! Layout, little-endian, read in place: a header of [`FORMAT`] (u32), the section count (u32)
//! and eight zero bytes; then per section its step (u8), three zero bytes, its length (u32),
//! eight zero bytes, its schema (32 bytes: the step's answer schema, which for MIR names the
//! owner's place in the body), its bytes, and zeros to the next multiple of 16. Every
//! section's bytes so start 16-aligned from the payload's start, which the record places
//! 16-aligned (`blaze_format::Writer::push_buffer`): each answer is a blaze-format buffer a
//! reader opens where the record is mapped, with no copy ([`crate::body::mir`]). Sections are
//! sorted by (step, schema), so the bytes depend only on what the steps answered (rule 25).
//!
//! Floor: one copy of each step's answer at the write; none at a read.

#![blaze::sans_io]

use std::alloc::{Allocator, Global};

use blaze_ids::ContentId;

/// The payload's format: a change to the layout or to what a section holds bumps it, and with
/// it `blaze_keyed::kind::Body`'s version. 2: sections 16-aligned, so read in place; the MIR
/// answer keeps its body's node counts (`hir_ty::body_store`'s MIR schema 2).
pub const FORMAT: u32 = 2;

/// Where a section's bytes are aligned to, from the payload's start: `blaze_format::ALIGN`.
pub const ALIGN: usize = blaze_format::ALIGN;

const HEADER: usize = 16;
const SECTION: usize = 16 + 32;

const _: () = assert!(HEADER.is_multiple_of(ALIGN) && SECTION.is_multiple_of(ALIGN));

/// Writes a payload.
pub struct Writer {
    sections: Vec<(u8, ContentId, Vec<u8, Global>), Global>,
}

impl Default for Writer {
    fn default() -> Writer {
        Writer::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer { sections: Vec::new_in(Global) }
    }

    /// A step's answer: `step` is `hir_ty::body_store::Step as u8`, `schema` its answer schema.
    pub fn push(&mut self, step: u8, schema: ContentId, bytes: &[u8]) {
        let mut owned = Vec::with_capacity_in(bytes.len(), Global);
        owned.extend_from_slice(bytes);
        self.sections.push((step, schema, owned));
    }

    #[expect(
        blaze::narrowed_len,
        reason = "a body's step answer and the count of a body's steps fit in u32; a larger one is refused by the item store's own u32 framing first"
    )]
    pub fn finish<A: Allocator>(mut self, alloc: A) -> Vec<u8, A> {
        self.sections.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        let len = HEADER
            + self.sections.iter().map(|(_, _, bytes)| SECTION + bytes.len().next_multiple_of(ALIGN)).sum::<usize>();
        let mut out = Vec::with_capacity_in(len, alloc);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(self.sections.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        for (step, schema, bytes) in &self.sections {
            out.extend_from_slice(&[*step, 0, 0, 0]);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&schema.0);
            out.extend_from_slice(bytes);
            out.resize(out.len().next_multiple_of(ALIGN), 0);
        }
        out
    }
}

/// One section of a payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Section<'b> {
    pub step: u8,
    pub schema: ContentId,
    pub bytes: &'b [u8],
}

/// Why bytes are not a payload, naming where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayloadError {
    Format { found: u32 },
    Short { at: usize },
    Padding { at: usize },
    Unsorted { at: usize },
}

/// Where the bytes of the section of `step` and `schema` are in `bytes`, a payload: `None` when
/// it has none. A walk of the section heads alone, no allocation; each head read is checked as
/// [`sections`] checks it, and the walk stops past `(step, schema)`, as sections are sorted.
/// Floor: one read of each head before the one found (a body has a handful).
pub fn find(bytes: &[u8], step: u8, schema: ContentId) -> Result<Option<core::ops::Range<usize>>, PayloadError> {
    let word = |at: usize| -> Result<u32, PayloadError> {
        let slice = bytes.get(at..at + 4).ok_or(PayloadError::Short { at })?;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    };
    let format = word(0)?;
    if format != FORMAT {
        return Err(PayloadError::Format { found: format });
    }
    let count = word(4)? as usize;
    let mut at = HEADER;
    for _ in 0..count {
        let head = bytes.get(at..at + SECTION).ok_or(PayloadError::Short { at })?;
        if head[1..4] != [0, 0, 0] || head[8..16] != [0; 8] {
            return Err(PayloadError::Padding { at });
        }
        let len = word(at + 4)? as usize;
        let from = at + SECTION;
        if bytes.len() < from + len {
            return Err(PayloadError::Short { at: from });
        }
        match (head[0], &head[16..48]).cmp(&(step, &schema.0[..])) {
            core::cmp::Ordering::Less => at = from + len.next_multiple_of(ALIGN),
            core::cmp::Ordering::Equal => return Ok(Some(from..from + len)),
            core::cmp::Ordering::Greater => return Ok(None),
        }
    }
    Ok(None)
}

/// Every section of `bytes`, checked once: the format, each length, the padding, the order.
pub fn sections<A: Allocator>(bytes: &[u8], alloc: A) -> Result<Vec<Section<'_>, A>, PayloadError> {
    let word = |at: usize| -> Result<u32, PayloadError> {
        let slice = bytes.get(at..at + 4).ok_or(PayloadError::Short { at })?;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    };
    let format = word(0)?;
    if format != FORMAT {
        return Err(PayloadError::Format { found: format });
    }
    let count = word(4)? as usize;
    if word(8)? != 0 || word(12)? != 0 {
        return Err(PayloadError::Padding { at: 8 });
    }
    let mut out: Vec<Section<'_>, A> = Vec::with_capacity_in(count.min(bytes.len() / SECTION), alloc);
    let mut at = HEADER;
    for _ in 0..count {
        let head = bytes.get(at..at + SECTION).ok_or(PayloadError::Short { at })?;
        if head[1..4] != [0, 0, 0] || head[8..16] != [0; 8] {
            return Err(PayloadError::Padding { at });
        }
        let len = word(at + 4)? as usize;
        let mut schema = [0u8; 32];
        schema.copy_from_slice(&head[16..48]);
        let from = at + SECTION;
        let body = bytes.get(from..from + len).ok_or(PayloadError::Short { at: from })?;
        let end = from + len.next_multiple_of(ALIGN);
        let pad = bytes.get(from + len..end).ok_or(PayloadError::Short { at: from + len })?;
        if pad.iter().any(|&b| b != 0) {
            return Err(PayloadError::Padding { at: from + len });
        }
        let section = Section { step: head[0], schema: ContentId(schema), bytes: body };
        if let Some(last) = out.last()
            && (last.step, last.schema) > (section.step, section.schema)
        {
            return Err(PayloadError::Unsorted { at });
        }
        out.push(section);
        at = end;
    }
    if at != bytes.len() {
        return Err(PayloadError::Short { at });
    }
    Ok(out)
}
