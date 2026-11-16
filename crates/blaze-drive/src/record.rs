//! The drive's record envelope (drive.md, "Records and validity").
//!
//! One buffer per record, in one of two forms under one magic:
//!
//! - **whole**: the name, the source, the out digest (what a reader of it sees), the read list,
//!   the parts (names this record lists, how each is found, its start in the file), the findings
//!   (rule, item-relative range, message), and the kind's own payload (blaze-surface's record
//!   bytes, a checked body, code). Stored at the task key when the read list is empty, else at
//!   the value key.
//! - **shape**: the read names of a read-addressed record, stored at its task key, so a lookup
//!   that knows only the key can compute the value key from today's digests.
//!
//! A view checks its buffer once when opened; its iterators then decode without checking again
//! (rule 74).

use core::alloc::Allocator;

use blaze_id::ContentId;
use blaze_own::Vec;

use crate::key::{Key, Name};
use crate::wire::{NAME_BYTES, Reader, WireError, WireWhy, Writer};

const MAGIC: &[u8; 4] = b"BDR1";
const WHOLE: u8 = 1;
const SHAPE: u8 = 2;

/// How the drive finds a part's source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Locate {
    /// Its source digest, in its home's record (an item's signature or body text).
    Source(ContentId),
    /// A file of the checkout read in `edition` (a crate root, a `mod x;`): the source is the
    /// file's content id from the sweep and the edition.
    File { edition: u8 },
    /// A record of a crate with no text here, by its key: fetched, never made.
    Key(Key),
}

/// A name a record lists: what the driver walks to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Part {
    pub name: Name,
    pub locate: Locate,
    /// Its first byte in its file (file-relative: the home is keyed by the file's bytes, so this
    /// moves exactly when the home's key does). 0 for a file.
    pub place: u32,
}

/// One finding: a lint or an error, no level, its range relative to its item's start (rule 81).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Finding<'a> {
    /// The check's number (a rule id, an error code).
    pub rule: u16,
    pub start: u32,
    pub end: u32,
    pub message: &'a [u8],
}

const READ_BYTES: usize = NAME_BYTES + 32;
const PART_BYTES: usize = NAME_BYTES + 1 + 32 + 4;
const FINDING_LEAST: usize = 2 + 4 + 4 + 4;

/// A whole record's fields, to write.
pub(crate) struct Whole<'a> {
    pub(crate) name: Name,
    pub(crate) source: ContentId,
    pub(crate) out: ContentId,
    pub(crate) reads: &'a [(Name, ContentId)],
    pub(crate) parts: &'a [Part],
    pub(crate) findings: &'a [Finding<'a>],
    pub(crate) payload: &'a [u8],
}

pub(crate) fn write_whole<A: Allocator>(whole: &Whole<'_>, alloc: A) -> Vec<u8, A> {
    let mut w = Writer::new_in(MAGIC, alloc);
    w.u8(WHOLE);
    w.name(whole.name);
    w.id(&whole.source);
    w.id(&whole.out);
    w.count(whole.reads.len());
    for (name, saw) in whole.reads {
        w.name(*name);
        w.id(saw);
    }
    w.count(whole.parts.len());
    for part in whole.parts {
        w.name(part.name);
        match part.locate {
            Locate::Source(id) => {
                w.u8(0);
                w.id(&id);
            }
            Locate::File { edition } => {
                w.u8(1);
                let mut id = [0; 32];
                id[0] = edition;
                w.id(&ContentId(id));
            }
            Locate::Key(key) => {
                w.u8(2);
                w.key(key);
            }
        }
        w.u32(part.place);
    }
    w.count(whole.findings.len());
    for finding in whole.findings {
        w.u16(finding.rule);
        w.u32(finding.start);
        w.u32(finding.end);
        w.bytes(finding.message);
    }
    w.count(whole.payload.len());
    w.pad_to(PAYLOAD_ALIGN);
    w.raw(whole.payload);
    w.finish()
}

/// Where a payload starts: a multiple of this from the record's first byte, so a blaze-format
/// payload (aligned to `blaze_format::ALIGN`, 16) is aligned in a store buffer aligned to it.
/// The store hands every record in such a buffer (drive.md, "The system side").
pub const PAYLOAD_ALIGN: usize = 16;

pub(crate) fn write_shape<A: Allocator>(name: Name, reads: &[(Name, ContentId)], alloc: A) -> Vec<u8, A> {
    let mut w = Writer::new_in(MAGIC, alloc);
    w.u8(SHAPE);
    w.name(name);
    w.count(reads.len());
    for (read, _) in reads {
        w.name(*read);
    }
    w.finish()
}

/// A stored record, checked.
pub enum Stored<'a> {
    Whole(WholeView<'a>),
    Shape(ShapeView<'a>),
}

/// A whole record, read in place.
#[derive(Clone, Copy, Debug)]
pub struct WholeView<'a> {
    pub name: Name,
    pub source: ContentId,
    pub out: ContentId,
    reads: &'a [u8],
    parts: &'a [u8],
    findings: &'a [u8],
    nfindings: usize,
    pub payload: &'a [u8],
}

/// A shape: the read names of the records under one task key.
#[derive(Clone, Copy, Debug)]
pub struct ShapeView<'a> {
    pub name: Name,
    names: &'a [u8],
}

/// `bytes` as a record of the drive's, checked whole.
pub fn open(bytes: &[u8]) -> Result<Stored<'_>, WireError> {
    let mut r = Reader::new(bytes, MAGIC, "drive record")?;
    let tag = r.u8()?;
    match tag {
        WHOLE => {
            let name = r.name()?;
            let source = r.id()?;
            let out = r.id()?;
            let reads = fixed(&mut r, bytes, READ_BYTES, |r| {
                r.name()?;
                r.id()?;
                Ok(())
            })?;
            let parts = fixed(&mut r, bytes, PART_BYTES, |r| {
                r.name()?;
                let tag = r.u8()?;
                if tag > 2 {
                    return Err(r.error(WireWhy::Tag(tag)));
                }
                r.id()?;
                r.u32().map(|_| ())
            })?;
            let nfindings = r.count(FINDING_LEAST)?;
            let start = r.position();
            for _ in 0..nfindings {
                r.u16()?;
                r.u32()?;
                r.u32()?;
                r.bytes()?;
            }
            let findings = &bytes[start..r.position()];
            let len = r.count(1)?;
            r.pad_to(PAYLOAD_ALIGN)?;
            let payload = r.take_bytes(len)?;
            r.done()?;
            Ok(Stored::Whole(WholeView { name, source, out, reads, parts, findings, nfindings, payload }))
        }
        SHAPE => {
            let name = r.name()?;
            let names = fixed(&mut r, bytes, NAME_BYTES, |r| r.name().map(|_| ()))?;
            r.done()?;
            Ok(Stored::Shape(ShapeView { name, names }))
        }
        tag => Err(r.error(WireWhy::Tag(tag))),
    }
}

/// A list of fixed-size entries: its count, each entry checked by `check`, and its bytes.
fn fixed<'a>(
    r: &mut Reader<'a>,
    bytes: &'a [u8],
    size: usize,
    check: impl Fn(&mut Reader<'a>) -> Result<(), WireError>,
) -> Result<&'a [u8], WireError> {
    let n = r.count(size)?;
    let start = r.position();
    for _ in 0..n {
        check(r)?;
    }
    Ok(&bytes[start..r.position()])
}

/// A reader over bytes [`open`] already checked.
fn checked(bytes: &[u8]) -> Reader<'_> {
    Reader::raw(bytes, "drive record")
}

const CHECKED: &str = "a drive record checked when it was opened";

impl<'a> WholeView<'a> {
    /// The read list, sorted by name.
    pub fn reads(&self) -> impl ExactSizeIterator<Item = (Name, ContentId)> + 'a {
        self.reads.chunks_exact(READ_BYTES).map(|chunk| {
            let mut r = checked(chunk);
            (r.name().expect(CHECKED), r.id().expect(CHECKED))
        })
    }

    pub fn parts(&self) -> impl ExactSizeIterator<Item = Part> + 'a {
        self.parts.chunks_exact(PART_BYTES).map(|chunk| {
            let mut r = checked(chunk);
            let name = r.name().expect(CHECKED);
            let tag = r.u8().expect(CHECKED);
            let id = r.id().expect(CHECKED);
            let locate = match tag {
                0 => Locate::Source(id),
                1 => Locate::File { edition: id.0[0] },
                _ => Locate::Key(Key(id)),
            };
            Part { name, locate, place: r.u32().expect(CHECKED) }
        })
    }

    pub fn findings(&self) -> Findings<'a> {
        Findings { r: checked(self.findings), left: self.nfindings }
    }

    pub fn finding_count(&self) -> usize {
        self.nfindings
    }
}

impl<'a> ShapeView<'a> {
    pub fn names(&self) -> impl ExactSizeIterator<Item = Name> + 'a {
        self.names.chunks_exact(NAME_BYTES).map(|chunk| checked(chunk).name().expect(CHECKED))
    }
}

/// A record's findings, in its order.
pub struct Findings<'a> {
    r: Reader<'a>,
    left: usize,
}

impl<'a> Iterator for Findings<'a> {
    type Item = Finding<'a>;

    fn next(&mut self) -> Option<Finding<'a>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let rule = self.r.u16().expect(CHECKED);
        let start = self.r.u32().expect(CHECKED);
        let end = self.r.u32().expect(CHECKED);
        let message = self.r.bytes().expect(CHECKED);
        Some(Finding { rule, start, end, message })
    }
}
