//! A writer's bytes are its records', never its allocator's (rule 25, rule 26): one value
//! written through writers in two different allocators, at two capacities, and through
//! [`to_bytes`], is byte for byte the same buffer, and that buffer is pinned here byte by byte.

use core::mem::offset_of;
use std::vec::Vec;

use blaze_own::Inline;

use crate::{
    AlignedIn, FormatError, Own, Persist, RelSpan, SchemaHash, Slice, Str, Writer, WriterIn,
    WriterOpts, open_as, record, schema, to_bytes,
};

record! {
    #[derive(Clone, Copy)]
    struct NoteRoot: 1 {
        spans: Slice<RelSpan>,
        text: Str,
        id: u64,
    }
}

struct Note {
    spans: Vec<RelSpan>,
    text: &'static str,
    id: u64,
}

impl Persist for Note {
    const SCHEMA: SchemaHash = schema("blaze-format-core writer test Note");
    type Ref<'a> = (&'a [RelSpan], &'a str, u64);

    fn write<P: WriterIn>(&self, out: &mut Writer<P>) -> u64 {
        let spans = out.push_slice(&self.spans);
        let text = out.push_str(self.text);
        let at = out.next::<NoteRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = NoteRoot {
            spans: Slice::between(field(offset_of!(NoteRoot, spans)), spans, self.spans.len()),
            text: Slice::between(field(offset_of!(NoteRoot, text)), text, self.text.len()),
            id: self.id,
        };
        let written = out.push(&root);
        assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<Self::Ref<'_>, FormatError> {
        let root = open_as::<NoteRoot>(buffer, Self::SCHEMA)?;
        Ok((root.spans.follow(buffer)?, root.text.as_str(buffer)?, root.id))
    }
}

fn note() -> Note {
    Note {
        spans: std::vec![RelSpan { start: 1, len: 2 }, RelSpan { start: 3, len: 4 }],
        text: "ab",
        id: 0x0102_0304_0506_0708,
    }
}

/// The buffer [`note`] is, written out by hand from the format's rules: the header, the spans
/// at 56, the text at 72, the root at 80 (aligned to 8), the end padded to 112.
fn pinned() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"BLAZEV2\0");
    bytes.extend_from_slice(&Note::SCHEMA.0.0);
    bytes.extend_from_slice(&112u64.to_le_bytes());
    bytes.extend_from_slice(&80u64.to_le_bytes());
    for word in [1u32, 2, 3, 4] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.extend_from_slice(b"ab");
    bytes.extend_from_slice(&[0; 6]);
    bytes.extend_from_slice(&(56i32 - 80).to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&(72i32 - 88).to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

#[test]
fn a_record_is_the_same_bytes_in_any_allocator() {
    let note = note();
    let heap = blaze_own::testing::heap().alloc();
    let inline = Inline::<8192>::new();

    let on_heap = to_bytes(&note, Own(heap));
    let in_inline = to_bytes(&note, Own(&inline));
    assert_eq!(&on_heap[..], &pinned()[..], "the bytes the format's rules give");
    assert_eq!(&in_inline[..], &on_heap[..], "two allocators, one buffer");
    assert!(inline.used() > 0, "the second buffer was in the inline allocator");

    let mut small = Writer::new(WriterOpts { schema: Note::SCHEMA, alloc: Own(heap), capacity: Some(1) });
    let root = note.write(&mut small);
    assert_eq!(&small.finish(root)[..], &on_heap[..], "a writer that grew writes the same bytes");

    let aligned = AlignedIn::copy_from(&on_heap, heap);
    assert_eq!(Note::read(&aligned), Ok((&note.spans[..], note.text, note.id)), "read back in place");
}

/// The deprecated global-heap writer the unported callers use writes the same bytes too.
#[cfg(feature = "alloc")]
#[test]
fn the_global_heap_writer_writes_the_same_bytes() {
    let note = note();
    let mut out = Writer::on_the_global_heap_until_ported(Note::SCHEMA);
    let root = note.write(&mut out);
    assert_eq!(&out.finish(root)[..], &pinned()[..]);
}
