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

use blaze_id::ContentId;
use blaze_own::{AVec, Alloc};

use crate::cells::{CELL_BYTES, Cell, CellRead, read_cell, write_cell};
use crate::condition::Var;
use crate::key::{Key, Name};
use crate::wire::{NAME_BYTES, Reader, WireError, WireWhy, Writer};

/// 2: a whole record holds its split variable (drive.md, "One check covers every
/// configuration"). 3: and the path of each `Locate::File` part ([`PathOf`]). 4: and its cell
/// reads after its whole reads, a shape its cells after its names (cells.rs). A home of an older
/// form is refused by its magic, reported and rebuilt.
pub(crate) const MAGIC: &[u8; 4] = b"BDR4";
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

/// A [`Locate::File`] byte that is no edition (blaze-surface's are 0 to 3): the file is read as
/// DATA (`include_str!`, `include_bytes!`), not as Rust. Its task reads the bytes and its out is
/// their content id (blaze-cli data_include.rs); listed and not there, its record carries no
/// finding (`absent_module`): the call that reads it reports it at the call.
pub const DATA_FILE: u8 = 0xD0;

/// A name a record lists: what the driver walks to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Part {
    pub name: Name,
    pub locate: Locate,
    /// Its first byte in its file (file-relative: the home is keyed by the file's bytes, so this
    /// moves exactly when the home's key does). 0 for a file.
    pub place: u32,
}

/// Where a `Locate::File` part's file is: the record names the path, so a walk that reaches the
/// part with no file for it (a first build, a record from another checkout) opens it there
/// (`Effect::Open`): files are found forward from what was read (drive.md, "The system side").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PathOf<'a> {
    pub name: Name,
    /// Relative to the checkout, `/`-separated.
    pub path: &'a [u8],
    /// A `mod x;` slot: the path is `x.rs`, and `x/mod.rs` is probed beside it.
    pub module: bool,
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
const CELL_READ_BYTES: usize = NAME_BYTES + CELL_BYTES + 32;
const SHAPE_CELL_BYTES: usize = NAME_BYTES + CELL_BYTES;
const PART_BYTES: usize = NAME_BYTES + 1 + 32 + 4;
const FINDING_LEAST: usize = 2 + 4 + 4 + 4;
const PATH_LEAST: usize = NAME_BYTES + 1 + 4;

/// A whole record's fields, to write.
pub(crate) struct Whole<'a> {
    pub(crate) name: Name,
    pub(crate) source: ContentId,
    pub(crate) out: ContentId,
    pub(crate) reads: &'a [(Name, ContentId)],
    /// Its part reads, sorted by (name, cell), one entry each.
    pub(crate) cells: &'a [CellRead],
    pub(crate) parts: &'a [Part],
    pub(crate) paths: &'a [PathOf<'a>],
    pub(crate) findings: &'a [Finding<'a>],
    pub(crate) payload: &'a [u8],
    /// A split: the task read this cfg variable unfixed, and its parts are its job re-entered
    /// once per value the space allows (`Locate` as its own; each fork's name is
    /// `key::at_condition`). `None` for every other record.
    pub(crate) split: Option<Var>,
}

/// A whole record with no parts, paths, findings or split: the bytes the drive would put for a
/// task that answered `out` with these reads and this payload. For a consumer that reads
/// records at a seam of its own (the cli's sharing choice, D115), so its tests take the drive's
/// envelope instead of a copy of its form.
pub fn write_plain<'a>(
    name: Name,
    source: ContentId,
    out: ContentId,
    reads: &[(Name, ContentId)],
    payload: &[u8],
    alloc: Alloc<'a>,
) -> AVec<'a, u8> {
    write_whole(&Whole { name, source, out, reads, cells: &[], parts: &[], paths: &[], findings: &[], payload, split: None }, alloc)
}

/// What a record lists besides its reads and payload: its parts, the path of each file part,
/// and its findings, read back through [`WholeView::parts`], [`WholeView::paths`] and
/// [`WholeView::findings`].
#[derive(Clone, Copy, Debug)]
pub struct Listed<'a> {
    pub parts: &'a [Part],
    pub paths: &'a [PathOf<'a>],
    pub findings: &'a [Finding<'a>],
}

/// A whole record with the parts, paths and findings of `listed`, and no cell reads or split:
/// the bytes a consumer puts for a task whose later readers walk its parts (the cli's
/// `blaze engine-check` puts each file's outline so, and a later request that finds the file
/// unchanged reads the outline back instead of reading the file).
pub fn write_listed<'a>(
    name: Name,
    source: ContentId,
    out: ContentId,
    reads: &[(Name, ContentId)],
    listed: &Listed<'_>,
    payload: &[u8],
    alloc: Alloc<'a>,
) -> AVec<'a, u8> {
    let Listed { parts, paths, findings } = *listed;
    write_whole(&Whole { name, source, out, reads, cells: &[], parts, paths, findings, payload, split: None }, alloc)
}

/// The head of [`write_listed`]'s record for a payload of `payload_len` bytes: every byte before
/// the payload, so the record is this head followed by the payload's bytes, equal to
/// [`write_listed`]'s (blaze-cli engine_store.rs puts the two as two slices, never joined).
pub fn write_listed_head<'a>(name: Name, source: ContentId, out: ContentId, reads: &[(Name, ContentId)], listed: &Listed<'_>, payload_len: usize, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let Listed { parts, paths, findings } = *listed;
    let whole = Whole { name, source, out, reads, cells: &[], parts, paths, findings, payload: &[], split: None };
    write_head(&whole, payload_len, alloc).finish()
}

/// The head of [`write_plain`]'s record for a payload of `payload_len` bytes: every byte before
/// the payload, so the record is this head followed by the payload's bytes, equal to
/// [`write_plain`]'s (one writer: [`write_whole`] is this head and the payload). For a writer
/// that hands the store the head and the payload as two slices, never joined (blaze-cli
/// engine_store.rs); read back by [`open`], or by [`open_parts`] over the two.
pub fn write_plain_head<'a>(name: Name, source: ContentId, out: ContentId, reads: &[(Name, ContentId)], payload_len: usize, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let whole = Whole { name, source, out, reads, cells: &[], parts: &[], paths: &[], findings: &[], payload: &[], split: None };
    write_head(&whole, payload_len, alloc).finish()
}

pub(crate) fn write_whole<'a>(whole: &Whole<'_>, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut w = write_head(whole, whole.payload.len(), alloc);
    w.raw(whole.payload);
    w.finish()
}

/// Every byte of `whole`'s record before its payload (of `payload_len` bytes; `whole.payload`
/// is not read).
fn write_head<'a>(whole: &Whole<'_>, payload_len: usize, alloc: Alloc<'a>) -> Writer<'a> {
    let mut w = Writer::new_in(MAGIC, alloc);
    w.u8(WHOLE);
    w.name(whole.name);
    w.id(&whole.source);
    w.id(&whole.out);
    w.split(whole.split);
    w.count(whole.reads.len());
    for (name, saw) in whole.reads {
        w.name(*name);
        w.id(saw);
    }
    w.count(whole.cells.len());
    for read in whole.cells {
        w.name(read.name);
        write_cell(&mut w, read.cell);
        w.id(&read.saw);
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
    w.count(whole.paths.len());
    for path in whole.paths {
        w.name(path.name);
        w.u8(u8::from(path.module));
        w.bytes(path.path);
    }
    w.count(whole.findings.len());
    for finding in whole.findings {
        w.u16(finding.rule);
        w.u32(finding.start);
        w.u32(finding.end);
        w.bytes(finding.message);
    }
    w.count(payload_len);
    w.pad_to(PAYLOAD_ALIGN);
    w
}

/// Where a payload starts: a multiple of this from the record's first byte, so a blaze-format
/// payload (aligned to `blaze_format::ALIGN`, 16) is aligned in a store buffer aligned to it.
/// The store hands every record in such a buffer (drive.md, "The system side").
pub const PAYLOAD_ALIGN: usize = 16;

pub(crate) fn write_shape<'a>(name: Name, reads: &[(Name, ContentId)], cells: &[CellRead], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut w = Writer::new_in(MAGIC, alloc);
    w.u8(SHAPE);
    w.name(name);
    w.count(reads.len());
    for (read, _) in reads {
        w.name(*read);
    }
    w.count(cells.len());
    for read in cells {
        w.name(read.name);
        write_cell(&mut w, read.cell);
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
    /// See [`Whole::split`].
    pub split: Option<Var>,
    reads: &'a [u8],
    cells: &'a [u8],
    parts: &'a [u8],
    paths: &'a [u8],
    npaths: usize,
    findings: &'a [u8],
    nfindings: usize,
    pub payload: &'a [u8],
}

/// A shape: the read names of the records under one task key.
#[derive(Clone, Copy, Debug)]
pub struct ShapeView<'a> {
    pub name: Name,
    names: &'a [u8],
    cells: &'a [u8],
}

/// `bytes` as a record of the drive's, checked whole.
pub fn open(bytes: &[u8]) -> Result<Stored<'_>, WireError> {
    open_in(bytes, None)
}

/// A whole record held as two slices, `head` ([`write_plain_head`]: every byte before the
/// payload) and `payload`, checked as [`open`] checks their concatenation, read in place: for a
/// writer that puts the two without joining them (blaze-cli engine_store.rs). A shape has no
/// payload: one handed with it is refused.
pub fn open_parts<'a>(head: &'a [u8], payload: &'a [u8]) -> Result<Stored<'a>, WireError> {
    open_in(head, Some(payload))
}

/// [`open`] over `bytes`, or over `bytes` as a head followed by `payload`.
fn open_in<'a>(bytes: &'a [u8], payload: Option<&'a [u8]>) -> Result<Stored<'a>, WireError> {
    let mut r = Reader::new(bytes, MAGIC, "drive record")?;
    let tag = r.u8()?;
    match tag {
        WHOLE => {
            let name = r.name()?;
            let source = r.id()?;
            let out = r.id()?;
            let split = r.split()?;
            let reads = fixed(&mut r, bytes, READ_BYTES, |r| {
                r.name()?;
                r.id()?;
                Ok(())
            })?;
            let cells = fixed(&mut r, bytes, CELL_READ_BYTES, |r| {
                r.name()?;
                read_cell(r)?;
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
            let npaths = r.count(PATH_LEAST)?;
            let start = r.position();
            for _ in 0..npaths {
                r.name()?;
                let module = r.u8()?;
                if module > 1 {
                    return Err(r.error(WireWhy::Tag(module)));
                }
                r.bytes()?;
            }
            let paths = &bytes[start..r.position()];
            let nfindings = r.count(FINDING_LEAST)?;
            let start = r.position();
            for _ in 0..nfindings {
                r.u16()?;
                r.u32()?;
                r.u32()?;
                r.bytes()?;
            }
            let findings = &bytes[start..r.position()];
            let payload = match payload {
                None => {
                    let len = r.count(1)?;
                    r.pad_to(PAYLOAD_ALIGN)?;
                    r.take_bytes(len)?
                }
                // The head ends where the payload starts: its length said, its padding read.
                Some(payload) => {
                    let len = r.u32()?;
                    if len as usize != payload.len() {
                        return Err(r.error(WireWhy::Count(len)));
                    }
                    r.pad_to(PAYLOAD_ALIGN)?;
                    payload
                }
            };
            r.done()?;
            Ok(Stored::Whole(WholeView { name, source, out, split, reads, cells, parts, paths, npaths, findings, nfindings, payload }))
        }
        SHAPE => {
            let name = r.name()?;
            let names = fixed(&mut r, bytes, NAME_BYTES, |r| r.name().map(|_| ()))?;
            let cells = fixed(&mut r, bytes, SHAPE_CELL_BYTES, |r| {
                r.name()?;
                read_cell(r).map(|_| ())
            })?;
            r.done()?;
            if payload.is_some_and(|payload| !payload.is_empty()) {
                return Err(r.error(WireWhy::Trailing));
            }
            Ok(Stored::Shape(ShapeView { name, names, cells }))
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

    /// The part reads, sorted by (name, cell).
    pub fn cells(&self) -> impl ExactSizeIterator<Item = CellRead> + 'a {
        self.cells.chunks_exact(CELL_READ_BYTES).map(|chunk| {
            let mut r = checked(chunk);
            let name = r.name().expect(CHECKED);
            let cell = read_cell(&mut r).expect(CHECKED);
            CellRead { name, cell, saw: r.id().expect(CHECKED) }
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

    /// The path of each `Locate::File` part, in the order the task gave them.
    pub fn paths(&self) -> impl Iterator<Item = PathOf<'a>> + 'a {
        let mut r = checked(self.paths);
        (0..self.npaths).map(move |_| {
            let name = r.name().expect(CHECKED);
            let module = r.u8().expect(CHECKED) == 1;
            PathOf { name, path: r.bytes().expect(CHECKED), module }
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

    /// The cells its value key reads, sorted by (name, cell).
    pub fn cells(&self) -> impl ExactSizeIterator<Item = (Name, Cell)> + 'a {
        self.cells.chunks_exact(SHAPE_CELL_BYTES).map(|chunk| {
            let mut r = checked(chunk);
            (r.name().expect(CHECKED), read_cell(&mut r).expect(CHECKED))
        })
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

#[cfg(test)]
mod head_tests {
    use super::*;

    /// [`write_plain_head`] and the payload are [`write_plain`]'s bytes, for payloads of each
    /// length around the alignment; [`open_parts`] over the two reads what [`open`] reads of the
    /// whole. CONTROL: a payload of another length than the head says is refused.
    #[test]
    fn a_plain_record_is_its_head_then_its_payload() {
        let heap = blaze_own::testing::heap();
        let reads = [(Name::units(), ContentId([3; 32]))];
        for len in [0usize, 1, 15, 16, 17, 100] {
            let payload: std::vec::Vec<u8> = (0..len).map(|at| at as u8).collect();
            let whole = write_plain(Name::units(), ContentId([1; 32]), ContentId([2; 32]), &reads, &payload, heap.alloc());
            let head = write_plain_head(Name::units(), ContentId([1; 32]), ContentId([2; 32]), &reads, len, heap.alloc());
            let mut joined = head.to_vec();
            joined.extend_from_slice(&payload);
            assert_eq!(&whole[..], &joined[..], "a payload of {len} bytes: head then payload is the record");
            let (Ok(Stored::Whole(a)), Ok(Stored::Whole(b))) = (open(&whole), open_parts(&head, &payload)) else { panic!("both open as whole records ({len} bytes)") };
            assert_eq!((a.name, a.out, a.payload), (b.name, b.out, b.payload), "{len} bytes");
            assert_eq!(a.reads().collect::<std::vec::Vec<_>>(), b.reads().collect::<std::vec::Vec<_>>());
            let longer = [payload.as_slice(), &[9]].concat();
            assert!(open_parts(&head, &longer).is_err(), "control: a payload one byte longer than the head says is refused");
        }
    }
}
