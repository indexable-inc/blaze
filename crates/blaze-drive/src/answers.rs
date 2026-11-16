//! The answer table (drive.md, "Records and validity"): the checkout's index from each name to
//! what it is now, kept as content-addressed pages under a root, and the `last` pointer that
//! names the root. A row is the forward index (its key, where its record is, its read list) and
//! the reverse one (its readers) at once; nothing of the table is held in memory but the pages a
//! run touches.

use blaze_id::ContentId;
use blaze_own::{ASortedMap, AVec, Alloc, SortedMap, Vec};

use crate::condition::{Condition, Var};
use crate::key::{Key, Name};
use crate::wire::{NAME_BYTES, Reader, WireError, WireWhy, Writer};

/// 2: a row holds its split and its condition.
pub(crate) const PAGE_MAGIC: &[u8; 4] = b"BDP2";
pub(crate) const ROOT_MAGIC: &[u8; 4] = b"BDA1";
/// The `last` file's form: read at a fixed path, so its caller checks this before
/// [`read_last`] and rebuilds a file of another form (crate::FORMATS).
pub const LAST_MAGIC: &[u8; 4] = b"BDL1";

/// The rows a page aims to hold; the root's bit count is the fewest that keep every page at or
/// under it on average.
pub const PAGE_ROWS: u64 = 512;
/// The most bits a root pages by: 65,536 pages, 33 million rows at [`PAGE_ROWS`].
pub const MOST_BITS: u8 = 16;

/// One name's answer.
#[derive(Clone, Debug)]
pub struct Row<'a> {
    /// Its task key: what its record is.
    pub key: Key,
    /// Where its whole record is: the key itself, or the value key of a read-addressed record.
    pub at: Key,
    pub source: ContentId,
    /// What a reader of it sees.
    pub out: ContentId,
    /// The record that lists it (its file's `FileItems`, a `Fetch`, the units); itself for a
    /// root.
    pub home: Name,
    /// Its start in its home's file; for a `FileItems` row, the edition it is read in.
    pub place: u32,
    /// Its read list, sorted by name: also the hints of its next run.
    pub reads: AVec<'a, (Name, ContentId)>,
    /// The names its record lists.
    pub parts: AVec<'a, Name>,
    /// The names whose read lists hold it, sorted: the reverse index.
    pub readers: AVec<'a, Name>,
    /// Its record's split variable: its parts are then its forks, one per allowed value.
    pub split: Option<Var>,
    /// The name it is a fork of and the condition it runs under; `None` under
    /// [`Condition::ANY`] (the name is then its own base). Kept so a fork walked from a hint
    /// next run runs under its condition with no parent walked first.
    pub cond: Option<(Name, Condition)>,
}

impl Row<'_> {
    /// A watch row: no record, only the readers that read the name ABSENT (a read that found
    /// nothing is a read, rule 21): when the name appears, they are found here and rerun.
    /// Its key is the zero key, which no task key is.
    pub fn watching(&self) -> bool {
        self.key == Key::default()
    }
}

/// The rows of one page, by name.
pub type Page<'a> = ASortedMap<'a, Name, Row<'a>>;

/// Which page of a root of `bits` holds `name`: its subject's leading bits.
pub fn page_of(name: Name, bits: u8) -> u32 {
    let lead = u32::from(name.subject.0[0]) << 8 | u32::from(name.subject.0[1]);
    match bits {
        0 => 0,
        bits => lead >> (16 - u32::from(bits.min(MOST_BITS))),
    }
}

/// The bit count for a table of `rows`.
pub fn bits_for(rows: u64) -> u8 {
    let mut bits = 0;
    while bits < MOST_BITS && rows > PAGE_ROWS << bits {
        bits += 1;
    }
    bits
}

pub(crate) fn write_page<'r, 'x: 'r, 'b>(
    rows: impl ExactSizeIterator<Item = (&'r Name, &'r Row<'x>)>,
    alloc: Alloc<'b>,
) -> AVec<'b, u8> {
    let mut w = Writer::new_in(PAGE_MAGIC, alloc);
    w.count(rows.len());
    for (name, row) in rows {
        w.name(*name);
        w.key(row.key);
        w.key(row.at);
        w.id(&row.source);
        w.id(&row.out);
        w.name(row.home);
        w.u32(row.place);
        w.split(row.split);
        match &row.cond {
            None => w.u8(0),
            Some((base, condition)) => {
                w.u8(1);
                w.name(*base);
                w.condition(condition);
            }
        }
        w.count(row.reads.len());
        for (read, saw) in row.reads.iter() {
            w.name(*read);
            w.id(saw);
        }
        w.count(row.parts.len());
        for part in row.parts.iter() {
            w.name(*part);
        }
        w.count(row.readers.len());
        for reader in row.readers.iter() {
            w.name(*reader);
        }
    }
    w.finish()
}

pub(crate) fn read_page<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<Page<'a>, WireError> {
    let mut r = Reader::new(bytes, PAGE_MAGIC, "answer page")?;
    // The least row: its name and home, key, at, source and out, place, split, the condition
    // tag and three counts.
    let n = r.count(NAME_BYTES * 2 + 32 * 4 + 4 + 4 + 1 + 12)?;
    let mut rows = Vec::with_capacity_in(n, alloc.clone());
    for _ in 0..n {
        let name = r.name()?;
        let key = r.key()?;
        let at = r.key()?;
        let source = r.id()?;
        let out = r.id()?;
        let home = r.name()?;
        let place = r.u32()?;
        let split = r.split()?;
        let cond = match r.u8()? {
            0 => None,
            1 => Some((r.name()?, r.condition()?)),
            tag => return Err(r.error(WireWhy::Tag(tag))),
        };
        let nreads = r.count(NAME_BYTES + 32)?;
        let mut reads = Vec::with_capacity_in(nreads, alloc.clone());
        for _ in 0..nreads {
            reads.push((r.name()?, r.id()?));
        }
        let nparts = r.count(NAME_BYTES)?;
        let mut parts = Vec::with_capacity_in(nparts, alloc.clone());
        for _ in 0..nparts {
            parts.push(r.name()?);
        }
        let nreaders = r.count(NAME_BYTES)?;
        let mut readers = Vec::with_capacity_in(nreaders, alloc.clone());
        for _ in 0..nreaders {
            readers.push(r.name()?);
        }
        rows.push((name, Row { key, at, source, out, home, place, reads, parts, readers, split, cond }));
    }
    r.done()?;
    SortedMap::from_sorted(rows).map_err(|_| r.error(crate::wire::WireWhy::Unsorted))
}

/// The table's root: how it is paged and each page's key.
#[derive(Clone, Debug)]
pub struct Root<'a> {
    pub bits: u8,
    pub rows: u64,
    /// One per page; [`Key::default`] for a page with no row.
    pub pages: AVec<'a, Key>,
}

pub(crate) fn write_root<'b>(root: &Root<'_>, alloc: Alloc<'b>) -> AVec<'b, u8> {
    let mut w = Writer::new_in(ROOT_MAGIC, alloc);
    w.u8(root.bits);
    w.u32(u32::try_from(root.rows).unwrap_or(u32::MAX));
    w.count(root.pages.len());
    for key in root.pages.iter() {
        w.key(*key);
    }
    w.finish()
}

pub(crate) fn read_root<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<Root<'a>, WireError> {
    let mut r = Reader::new(bytes, ROOT_MAGIC, "answer root")?;
    let bits = r.u8()?;
    let rows = u64::from(r.u32()?);
    let n = r.count(32)?;
    if bits > MOST_BITS || n != 1 << bits {
        return Err(r.error(crate::wire::WireWhy::Count(u32::try_from(n).unwrap_or(u32::MAX))));
    }
    let mut pages = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        pages.push(r.key()?);
    }
    r.done()?;
    Ok(Root { bits, rows, pages })
}

/// The checkout's pointer to its last answer: a file of the checkout, written whole after the
/// answer's records are in the store.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Last {
    pub tool: ContentId,
    /// The command's shape: what it wants (check, run, test) and its arguments.
    pub shape: ContentId,
    /// The root units' source the answer was made from (the root manifest and the manifests'
    /// paths): another one now reads the root units again.
    pub units: ContentId,
    pub cert: Key,
    pub root: Key,
}

pub(crate) fn write_last<'a>(last: &Last, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut w = Writer::new_in(LAST_MAGIC, alloc);
    w.id(&last.tool);
    w.id(&last.shape);
    w.id(&last.units);
    w.key(last.cert);
    w.key(last.root);
    w.finish()
}

/// The `last` file's bytes, checked.
pub fn read_last(bytes: &[u8]) -> Result<Last, WireError> {
    let mut r = Reader::new(bytes, LAST_MAGIC, "last answer")?;
    let last = Last { tool: r.id()?, shape: r.id()?, units: r.id()?, cert: r.key()?, root: r.key()? };
    r.done()?;
    Ok(last)
}
