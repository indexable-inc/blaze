//! The stored answer (engine.md E6, "the incremental half"): the engine's [`Answer`] and why each
//! of its rows was last entered ([`Kept`]), as content-keyed pages under a root, in the form of
//! blaze-drive's answer table (answers.rs: pages sized by bytes, [`PAGE_BYTES`] on average, a
//! name's page its subject's leading bits, [`page_of`]).
//!
//! # The form
//!
//! Every field little-endian and fixed-width; a name is its kind byte and its 32-byte subject.
//!
//! ```text
//! root = "BER1" | serial u64 | bits u8 | bytes u64 | pages u32 (= 1 << bits) | pages x key [32]
//!        (a page with nothing on it: the zero key, no record)
//! page = "BEP2" | bits u8 | index u32 | 7 x count u32 | the seven sections, in this order:
//!   rows        name | key | stage u8 | wave u16 | file | source | out | serial u64 |
//!               findings u32 | flags u8 (1 inline, 2 failed) | defines u32 | reads u32 |
//!               defines x (cell [16], digest) | reads x (cell id, digest)
//!   cells       cell id | digest | gen u64
//!   owed cells  cell id | gen u64
//!   owed tasks  name | stage u8 | wave u16 | source | file
//!   files       file | names u32 | names x name
//!   causes      name | gen u64 | tag u8 | cell id | was | now   (tag: new, key moved,
//!               read moved, owed, inline; the cell and digests zero but for a moved read)
//!   records     name | key   (the store key of the record the row's last entry made: the
//!               driver's, which a later request reads a row that stands by)
//! cell id = name (its task) | cell [16]
//! ```
//!
//! Each section sorted by its first field, strictly; an entry is on the page of its first
//! field's name (a cell's: its task's). So everything about one task is on one page, and an edit
//! rewrites the pages of the tasks it entered and nothing else ([`encode`] puts only pages whose
//! key is not the last root's). The reverse index ([`Answer::readers`]) and the rows with
//! findings are the rows' own facts and are made again on [`decode`], never stored twice.
//!
//! # Checked once (rule 74)
//!
//! [`decode`] checks each page's bytes against the key that named it (a flipped bit or a cut
//! write is [`PagesError::Digest`]), then every count, kind byte, order and page membership. A
//! refusal names the page and the section (rule 19); the caller reports it and answers fresh
//! (rule 3: a stated path, never a quiet one).
//!
//! # Durable order (rule 43; simulated in pages_sim.rs)
//!
//! [`Encoded::puts`] lists the records to put, every new page before the root; [`Encoded::pins`]
//! the keys the checkout's root names, the root first then every page, pinned after the puts and
//! made visible by the store's one commit (its generation flip, blaze-cas plain). A crash before
//! that commit leaves the last root naming the last pages; after it, the new root naming the new.

use core::fmt;

use blaze_drive::answers::{bits_kept, page_of};
use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name, content_key};
use blaze_id::ContentId;
use blaze_own::{BTreeMap, SharedAlloc, Vec};

use crate::engine::{Answer, Cause, CellId, Entered, Gen, Phase, Planned, Row, SMap, SVec, Stage};

/// A page's form. A page of another form is never read: it is a [`PagesError::Magic`].
pub const PAGE_MAGIC: &[u8; 4] = b"BEP2";
/// The root's form.
pub const ROOT_MAGIC: &[u8; 4] = b"BER1";

const NAME: usize = 1 + 32;
const ID: usize = 32;
const CELL: usize = 16;
const CELL_ID: usize = NAME + CELL;
const SECTIONS: usize = 7;
const ROWS: usize = 0;
const CELLS: usize = 1;
const OWED_CELLS: usize = 2;
const OWED_TASKS: usize = 3;
const FILES: usize = 4;
const CAUSES: usize = 5;
const RECORDS: usize = 6;
const SECTION_NAMES: [&str; SECTIONS] = ["rows", "cells", "owed cells", "owed tasks", "files", "causes", "records"];
const HEAD: usize = 4 + 1 + 4 + 4 * SECTIONS;
const ROW_FIXED: usize = NAME + ID + 1 + 2 + NAME + ID + ID + 8 + 4 + 1 + 4 + 4;
const DEFINE: usize = CELL + ID;
const READ: usize = CELL_ID + ID;
const CELL_ENTRY: usize = CELL_ID + ID + 8;
const OWED_CELL: usize = CELL_ID + 8;
const OWED_TASK: usize = NAME + 1 + 2 + ID + NAME;
const CAUSE: usize = NAME + 8 + 1 + CELL_ID + ID + ID;
const ROOT_HEAD: usize = 4 + 8 + 1 + 8 + 4;
/// The page index a root's own errors name.
pub const ROOT_PAGE: u32 = u32::MAX;

const INLINE: u8 = 1;
const FAILED: u8 = 2;

/// What a request keeps: the engine's answer, and why each row's task was last entered and in
/// which request (`blaze why` walks these back to the edit).
pub struct Kept<'a> {
    pub answer: Answer<'a>,
    pub causes: SMap<'a, Name, (Gen, Cause)>,
    /// Each row's record in the store, by the key the driver put it under (one a row that
    /// stands is read by); the engine never reads it. One whose row went is not stored.
    pub records: SMap<'a, Name, Key>,
}

impl<'a> Kept<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Kept<'a> {
        Kept { answer: Answer::new_in(alloc), causes: BTreeMap::new_in(alloc), records: BTreeMap::new_in(alloc) }
    }

    /// The request's answer kept with its causes over the last ones: each entered task's cause
    /// at the answer's serial. A cause whose row went is not stored ([`encode`] skips it).
    pub fn after(mut causes: SMap<'a, Name, (Gen, Cause)>, answer: Answer<'a>, entered: &[Entered], records: SMap<'a, Name, Key>) -> Kept<'a> {
        for one in entered.iter() {
            causes.insert(one.name, (answer.serial, one.cause));
        }
        Kept { answer, causes, records }
    }
}

/// Why stored pages are not an answer of this form. Each names the page (its index;
/// [`ROOT_PAGE`] for the root) and what in it (rule 19); nothing past a refusal is read (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PagesError {
    /// The store has no record of the key the root names.
    Missing { page: u32, key: Key },
    /// The bytes are not the key's: a flipped bit, or a write a crash cut short.
    Digest { page: u32, want: Key, got: Key },
    /// Another form's magic, or a page another root's (its bits or index not the root's).
    Magic { page: u32 },
    /// Fewer bytes than its counts need.
    Short { page: u32, at: u32 },
    /// Bytes past its last section.
    Trailing { page: u32, at: u32 },
    /// A kind, stage, flag or cause byte no value has.
    Tag { page: u32, at: u32, byte: u8 },
    /// A section not strictly sorted by its first field.
    Unsorted { page: u32, section: &'static str, at: u32 },
    /// An entry on a page its name's bits do not choose.
    Misplaced { page: u32, section: &'static str, name: Name },
    /// A root whose page count is not `1 << bits`, or whose bits pass the most.
    Count { page: u32, count: u32 },
}

impl fmt::Display for PagesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let page = |f: &mut fmt::Formatter<'_>, page: u32| match page {
            ROOT_PAGE => write!(f, "the stored answer's root"),
            page => write!(f, "the stored answer's page {page}"),
        };
        match self {
            PagesError::Missing { page: p, key } => {
                page(f, *p)?;
                write!(f, " ({key:?}) is not in the store (blaze-engine pages.rs `decode`)")
            }
            PagesError::Digest { page: p, want, got } => {
                page(f, *p)?;
                write!(f, " is not the bytes its key names: want {want:?}, got {got:?}: a flipped bit or a cut write (pages.rs `decode`)")
            }
            PagesError::Magic { page: p } => {
                page(f, *p)?;
                write!(f, " is of another form or another root (pages.rs `decode`)")
            }
            PagesError::Short { page: p, at } => {
                page(f, *p)?;
                write!(f, " ends at byte {at} before its counts do (pages.rs `decode`)")
            }
            PagesError::Trailing { page: p, at } => {
                page(f, *p)?;
                write!(f, " has bytes past its last section, from {at} (pages.rs `decode`)")
            }
            PagesError::Tag { page: p, at, byte } => {
                page(f, *p)?;
                write!(f, ": byte {at} is {byte}, which no value has (pages.rs `decode`)")
            }
            PagesError::Unsorted { page: p, section, at } => {
                page(f, *p)?;
                write!(f, ": its {section} are not sorted at byte {at} (pages.rs `decode`)")
            }
            PagesError::Misplaced { page: p, section, name } => {
                page(f, *p)?;
                write!(f, ": {name:?} in its {section} belongs on another page (pages.rs `decode`)")
            }
            PagesError::Count { page: p, count } => {
                page(f, *p)?;
                write!(f, " names {count} pages, not 1 << its bits (pages.rs `decode`)")
            }
        }
    }
}

/// A [`Kept`] as records: what to put, in order, and what the checkout's root names.
pub struct Encoded<'a> {
    pub serial: Gen,
    pub bits: u8,
    /// The pages' bytes in all.
    pub bytes: u64,
    pub root: Key,
    /// Each page's key by index; the zero key for a page with nothing on it.
    pub pages: SVec<'a, Key>,
    /// The records not under the last root, every page before the root (the durable order).
    pub puts: SVec<'a, (Key, SVec<'a, u8>)>,
    plant: PagesPlant,
}

impl<'a> Encoded<'a> {
    /// The keys the checkout's root names after the puts: the root, then every page with
    /// anything on it, so compaction keeps the whole answer the root reaches.
    pub fn pins(&self, alloc: SharedAlloc<'a>) -> SVec<'a, Key> {
        let mut pins = Vec::with_capacity_in(1 + self.pages.len(), alloc);
        pins.push(self.root);
        if self.plant == PagesPlant::PagesNotPinned {
            return pins;
        }
        pins.extend(self.pages.iter().copied().filter(|key| *key != Key::default()));
        pins
    }
}

/// The root as stored: its serial, its paging and each page's key.
pub struct Root<'a> {
    pub serial: Gen,
    pub bits: u8,
    pub bytes: u64,
    pub pages: SVec<'a, Key>,
}

fn put_id(out: &mut SVec<'_, u8>, id: &ContentId) {
    out.extend_from_slice(&id.0);
}

fn put_name(out: &mut SVec<'_, u8>, name: Name) {
    out.push(name.kind as u8);
    put_id(out, &name.subject);
}

fn put_cell_id(out: &mut SVec<'_, u8>, id: &CellId) {
    put_name(out, id.of);
    out.extend_from_slice(&id.cell.0);
}

fn put_phase(out: &mut SVec<'_, u8>, phase: Phase) {
    out.push(phase.stage as u8);
    out.extend_from_slice(&phase.wave.to_le_bytes());
}

fn put_u32(out: &mut SVec<'_, u8>, value: usize) {
    out.extend_from_slice(&u32::try_from(value).expect("a page's counts fit u32 (a page holds about PAGE_BYTES)").to_le_bytes());
}

/// A page being written: each section's bytes and count.
struct PageBuf<'a> {
    counts: [usize; SECTIONS],
    parts: [SVec<'a, u8>; SECTIONS],
}

/// A bug the simulations plant to show their oracles catch it. Always `None` outside the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PagesPlant {
    None,
    /// Each file's planned tasks are not stored: a task an edit removes from its file keeps its
    /// row after a restore.
    FilesDropped,
    /// The checkout's root names the answer's root and not its pages: compaction takes them.
    PagesNotPinned,
}

/// `kept` as pages of about [`PAGE_BYTES`](blaze_drive::answers::PAGE_BYTES), paged as the last
/// root was while its bits still fit (answers.rs `bits_kept`); `last` is that root, whose pages
/// are not put again.
pub fn encode<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_planted(kept, last, PagesPlant::None, alloc)
}

/// [`encode`] with a planted bug ([`PagesPlant`]).
pub fn encode_planted<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, plant: PagesPlant, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    let answer = &kept.answer;
    let files_kept = plant != PagesPlant::FilesDropped;
    let has_row = |name: &Name| answer.rows.contains_key(name);
    // The bytes in all, which size the paging.
    let mut total = 0u64;
    for row in answer.rows.values() {
        total += (ROW_FIXED + DEFINE * row.defines.len() + READ * row.reads.len()) as u64;
    }
    total += (CELL_ENTRY * answer.cells.len() + OWED_CELL * answer.owed_cells.len() + OWED_TASK * answer.owed_tasks.len()) as u64;
    for names in answer.by_file.values().filter(|_| files_kept) {
        total += (NAME + 4 + NAME * names.len()) as u64;
    }
    total += (CAUSE * kept.causes.keys().filter(|name| has_row(*name)).count()) as u64;
    total += ((NAME + ID) * kept.records.keys().filter(|name| has_row(*name)).count()) as u64;
    let bits = bits_kept(total, last.map(|root| root.bits));
    let count = 1usize << bits;
    let mut bufs: SVec<'a, PageBuf<'a>> = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        bufs.push(PageBuf { counts: [0; SECTIONS], parts: core::array::from_fn(|_| Vec::new_in(alloc)) });
    }
    fn at<'b, 'a>(bufs: &'b mut [PageBuf<'a>], bits: u8, name: Name, section: usize) -> &'b mut SVec<'a, u8> {
        let buf = &mut bufs[page_of(name, bits) as usize];
        buf.counts[section] += 1;
        &mut buf.parts[section]
    }
    for (name, row) in answer.rows.iter() {
        let out = at(&mut bufs, bits, *name, ROWS);
        put_name(out, *name);
        put_id(out, &row.key.0);
        put_phase(out, row.phase);
        put_name(out, row.file);
        put_id(out, &row.source);
        put_id(out, &row.out);
        out.extend_from_slice(&row.serial.0.to_le_bytes());
        out.extend_from_slice(&row.findings.to_le_bytes());
        out.push(if row.inline { INLINE } else { 0 } | if row.failed { FAILED } else { 0 });
        put_u32(out, row.defines.len());
        put_u32(out, row.reads.len());
        for (cell, digest) in row.defines.iter() {
            out.extend_from_slice(&cell.0);
            put_id(out, digest);
        }
        for (id, digest) in row.reads.iter() {
            put_cell_id(out, id);
            put_id(out, digest);
        }
    }
    for (id, (digest, moved)) in answer.cells.iter() {
        let out = at(&mut bufs, bits, id.of, CELLS);
        put_cell_id(out, id);
        put_id(out, digest);
        out.extend_from_slice(&moved.0.to_le_bytes());
    }
    for (id, moved) in answer.owed_cells.iter() {
        let out = at(&mut bufs, bits, id.of, OWED_CELLS);
        put_cell_id(out, id);
        out.extend_from_slice(&moved.0.to_le_bytes());
    }
    for (name, planned) in answer.owed_tasks.iter() {
        let out = at(&mut bufs, bits, *name, OWED_TASKS);
        put_name(out, *name);
        put_phase(out, planned.phase);
        put_id(out, &planned.source);
        put_name(out, planned.file);
    }
    for (file, names) in answer.by_file.iter().filter(|_| files_kept) {
        let out = at(&mut bufs, bits, *file, FILES);
        put_name(out, *file);
        put_u32(out, names.len());
        for name in names.iter() {
            put_name(out, *name);
        }
    }
    for (name, (serial, cause)) in kept.causes.iter() {
        if !has_row(name) {
            continue;
        }
        let out = at(&mut bufs, bits, *name, CAUSES);
        put_name(out, *name);
        out.extend_from_slice(&serial.0.to_le_bytes());
        let (tag, read, was, now) = match cause {
            Cause::New => (0, None, ContentId([0; 32]), ContentId([0; 32])),
            Cause::KeyMoved => (1, None, ContentId([0; 32]), ContentId([0; 32])),
            Cause::ReadMoved { read, was, now } => (2, Some(*read), *was, *now),
            Cause::Owed => (3, None, ContentId([0; 32]), ContentId([0; 32])),
            Cause::Inline => (4, None, ContentId([0; 32]), ContentId([0; 32])),
        };
        out.push(tag);
        match read {
            Some(id) => put_cell_id(out, &id),
            None => out.extend_from_slice(&[0; CELL_ID]),
        }
        put_id(out, &was);
        put_id(out, &now);
    }
    for (name, key) in kept.records.iter() {
        if !has_row(name) {
            continue;
        }
        let out = at(&mut bufs, bits, *name, RECORDS);
        put_name(out, *name);
        put_id(out, &key.0);
    }
    // Each page whole, keyed by its bytes; put when the last root did not name it.
    let same_paging = last.filter(|root| root.bits == bits);
    let mut pages: SVec<'a, Key> = Vec::with_capacity_in(count, alloc);
    let mut puts: SVec<'a, (Key, SVec<'a, u8>)> = Vec::new_in(alloc);
    let mut bytes = 0u64;
    for (index, buf) in bufs.into_iter().enumerate() {
        if buf.counts.iter().all(|count| *count == 0) {
            pages.push(Key::default());
            continue;
        }
        let len = HEAD + buf.parts.iter().map(|part| part.len()).sum::<usize>();
        let mut page: SVec<'a, u8> = Vec::with_capacity_in(len, alloc);
        page.extend_from_slice(PAGE_MAGIC);
        page.push(bits);
        put_u32(&mut page, index);
        for count in buf.counts.iter() {
            put_u32(&mut page, *count);
        }
        for part in buf.parts.iter() {
            page.extend_from_slice(part);
        }
        bytes += page.len() as u64;
        let key = content_key(&page);
        pages.push(key);
        if same_paging.is_none_or(|root| root.pages.get(index) != Some(&key)) {
            puts.push((key, page));
        }
    }
    let mut root_bytes: SVec<'a, u8> = Vec::with_capacity_in(ROOT_HEAD + ID * count, alloc);
    root_bytes.extend_from_slice(ROOT_MAGIC);
    root_bytes.extend_from_slice(&answer.serial.0.to_le_bytes());
    root_bytes.push(bits);
    root_bytes.extend_from_slice(&bytes.to_le_bytes());
    put_u32(&mut root_bytes, count);
    for key in pages.iter() {
        put_id(&mut root_bytes, &key.0);
    }
    let root = content_key(&root_bytes);
    puts.push((root, root_bytes));
    Encoded { serial: answer.serial, bits, bytes, root, pages, puts, plant }
}

/// Bytes read in order, each refusal naming its page and offset.
struct Rd<'b> {
    bytes: &'b [u8],
    at: usize,
    page: u32,
}

impl<'b> Rd<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], PagesError> {
        if self.bytes.len() - self.at < n {
            return Err(PagesError::Short { page: self.page, at: self.bytes.len() as u32 });
        }
        let taken = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, PagesError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, PagesError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, PagesError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, PagesError> {
        let b = self.take(8)?;
        let mut word = [0; 8];
        word.copy_from_slice(b);
        Ok(u64::from_le_bytes(word))
    }

    fn id(&mut self) -> Result<ContentId, PagesError> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(ID)?);
        Ok(ContentId(id))
    }

    fn tag(&self, byte: u8) -> PagesError {
        PagesError::Tag { page: self.page, at: self.at.saturating_sub(1) as u32, byte }
    }

    fn name(&mut self) -> Result<Name, PagesError> {
        let byte = self.u8()?;
        let kind = Kind::from_u8(byte).ok_or(self.tag(byte))?;
        Ok(Name::new(kind, self.id()?))
    }

    fn cell(&mut self) -> Result<Cell, PagesError> {
        let mut cell = [0; CELL];
        cell.copy_from_slice(self.take(CELL)?);
        Ok(Cell(cell))
    }

    fn cell_id(&mut self) -> Result<CellId, PagesError> {
        Ok(CellId { of: self.name()?, cell: self.cell()? })
    }

    fn phase(&mut self) -> Result<Phase, PagesError> {
        let byte = self.u8()?;
        let stage = *Stage::ALL.get(usize::from(byte)).ok_or(self.tag(byte))?;
        Ok(Phase::new(stage, self.u16()?))
    }

    /// A count of entries of `width` bytes each that the page still holds.
    fn count(&mut self, width: usize) -> Result<usize, PagesError> {
        let n = self.u32()? as usize;
        match n.checked_mul(width) {
            Some(need) if need <= self.bytes.len() - self.at => Ok(n),
            _ => Err(PagesError::Short { page: self.page, at: self.bytes.len() as u32 }),
        }
    }
}

/// The root's bytes, checked against the key that named it.
pub fn decode_root<'a>(key: Key, bytes: &[u8], alloc: SharedAlloc<'a>) -> Result<Root<'a>, PagesError> {
    let got = content_key(bytes);
    if got != key {
        return Err(PagesError::Digest { page: ROOT_PAGE, want: key, got });
    }
    let mut r = Rd { bytes, at: 0, page: ROOT_PAGE };
    if r.take(4)? != ROOT_MAGIC {
        return Err(PagesError::Magic { page: ROOT_PAGE });
    }
    let serial = Gen(r.u64()?);
    let bits = r.u8()?;
    let total = r.u64()?;
    let count = r.count(ID)?;
    if bits > blaze_drive::answers::MOST_BITS || count != 1 << bits {
        return Err(PagesError::Count { page: ROOT_PAGE, count: count as u32 });
    }
    let mut pages = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        pages.push(Key(r.id()?));
    }
    if r.at != bytes.len() {
        return Err(PagesError::Trailing { page: ROOT_PAGE, at: r.at as u32 });
    }
    Ok(Root { serial, bits, bytes: total, pages })
}

/// The answer the root `key` names, every record through `get`; each checked once against its
/// key, then whole (the module docs, "Checked once").
pub fn decode<'a, 'g>(key: Key, mut get: impl FnMut(Key) -> Option<&'g [u8]>, alloc: SharedAlloc<'a>) -> Result<Kept<'a>, PagesError> {
    let root_bytes = get(key).ok_or(PagesError::Missing { page: ROOT_PAGE, key })?;
    let root = decode_root(key, root_bytes, alloc)?;
    let mut kept = Kept::new_in(alloc);
    kept.answer.serial = root.serial;
    for (index, key) in root.pages.iter().enumerate() {
        if *key == Key::default() {
            continue;
        }
        let page = index as u32;
        let bytes = get(*key).ok_or(PagesError::Missing { page, key: *key })?;
        let got = content_key(bytes);
        if got != *key {
            return Err(PagesError::Digest { page, want: *key, got });
        }
        decode_page(bytes, page, root.bits, &mut kept, alloc)?;
    }
    // The reverse index, made again from the rows' reads: each list sorted (rows came page by
    // page, and a name's page is its subject's bits, not its order).
    for list in kept.answer.readers.values_mut() {
        list.sort_unstable();
    }
    Ok(kept)
}

/// One page decoded alone, for the tests of its form's own checks.
#[cfg(test)]
pub(crate) fn decode_page_for_test(bytes: &[u8], page: u32, bits: u8) -> Result<(), PagesError> {
    let alloc = blaze_own::testing::heap().shared();
    let mut kept = Kept::new_in(alloc);
    decode_page(bytes, page, bits, &mut kept, alloc)
}

fn decode_page<'a>(bytes: &[u8], page: u32, bits: u8, kept: &mut Kept<'a>, alloc: SharedAlloc<'a>) -> Result<(), PagesError> {
    let mut r = Rd { bytes, at: 0, page };
    if r.take(4)? != PAGE_MAGIC || r.u8()? != bits || r.u32()? != page {
        return Err(PagesError::Magic { page });
    }
    let mut counts = [0usize; SECTIONS];
    for count in counts.iter_mut() {
        *count = r.u32()? as usize;
    }
    let placed = |section: usize, name: Name| match page_of(name, bits) == page {
        true => Ok(()),
        false => Err(PagesError::Misplaced { page, section: SECTION_NAMES[section], name }),
    };
    let unsorted = |section: usize, at: usize| PagesError::Unsorted { page, section: SECTION_NAMES[section], at: at as u32 };
    let answer = &mut kept.answer;
    let mut last: Option<Name> = None;
    for _ in 0..counts[ROWS] {
        let start = r.at;
        let name = r.name()?;
        if last.is_some_and(|last| last >= name) {
            return Err(unsorted(ROWS, start));
        }
        last = Some(name);
        placed(ROWS, name)?;
        let key = Key(r.id()?);
        let phase = r.phase()?;
        let file = r.name()?;
        let source = r.id()?;
        let out = r.id()?;
        let serial = Gen(r.u64()?);
        let findings = r.u32()?;
        let flags = r.u8()?;
        if flags & !(INLINE | FAILED) != 0 {
            return Err(r.tag(flags));
        }
        let n_defines = r.u32()? as usize;
        let n_reads = r.count(0)?;
        if n_defines.checked_mul(DEFINE).zip(n_reads.checked_mul(READ)).is_none_or(|(d, e)| d.saturating_add(e) > bytes.len() - r.at) {
            return Err(PagesError::Short { page, at: bytes.len() as u32 });
        }
        let mut defines: SVec<'a, (Cell, ContentId)> = Vec::with_capacity_in(n_defines, alloc);
        for _ in 0..n_defines {
            let at = r.at;
            let one = (r.cell()?, r.id()?);
            if defines.last().is_some_and(|prev| prev.0 > one.0) {
                return Err(unsorted(ROWS, at));
            }
            defines.push(one);
        }
        let mut reads: SVec<'a, (CellId, ContentId)> = Vec::with_capacity_in(n_reads, alloc);
        for _ in 0..n_reads {
            let at = r.at;
            let one = (r.cell_id()?, r.id()?);
            if reads.last().is_some_and(|prev| prev.0 > one.0) {
                return Err(unsorted(ROWS, at));
            }
            reads.push(one);
        }
        for (id, _) in reads.iter() {
            let readers = answer.readers.entry(*id).or_insert_with(|| Vec::new_in(alloc));
            if readers.last() != Some(&name) {
                readers.push(name);
            }
        }
        let failed = flags & FAILED != 0;
        if findings > 0 || failed {
            answer.with_findings.insert(name);
        }
        answer.rows.insert(name, Row { key, phase, file, source, out, serial, defines, reads, findings, inline: flags & INLINE != 0, failed });
    }
    let mut last: Option<CellId> = None;
    for _ in 0..counts[CELLS] {
        let start = r.at;
        let id = r.cell_id()?;
        if last.is_some_and(|last| last >= id) {
            return Err(unsorted(CELLS, start));
        }
        last = Some(id);
        placed(CELLS, id.of)?;
        let digest = r.id()?;
        answer.cells.insert(id, (digest, Gen(r.u64()?)));
    }
    let mut last: Option<CellId> = None;
    for _ in 0..counts[OWED_CELLS] {
        let start = r.at;
        let id = r.cell_id()?;
        if last.is_some_and(|last| last >= id) {
            return Err(unsorted(OWED_CELLS, start));
        }
        last = Some(id);
        placed(OWED_CELLS, id.of)?;
        answer.owed_cells.insert(id, Gen(r.u64()?));
    }
    let mut last: Option<Name> = None;
    for _ in 0..counts[OWED_TASKS] {
        let start = r.at;
        let name = r.name()?;
        if last.is_some_and(|last| last >= name) {
            return Err(unsorted(OWED_TASKS, start));
        }
        last = Some(name);
        placed(OWED_TASKS, name)?;
        let phase = r.phase()?;
        let source = r.id()?;
        let file = r.name()?;
        answer.owed_tasks.insert(name, Planned { name, phase, source, file });
    }
    let mut last: Option<Name> = None;
    for _ in 0..counts[FILES] {
        let start = r.at;
        let file = r.name()?;
        if last.is_some_and(|last| last >= file) {
            return Err(unsorted(FILES, start));
        }
        last = Some(file);
        placed(FILES, file)?;
        let n = r.count(NAME)?;
        let mut names: SVec<'a, Name> = Vec::with_capacity_in(n, alloc);
        for _ in 0..n {
            let at = r.at;
            let name = r.name()?;
            if names.last().is_some_and(|prev| *prev >= name) {
                return Err(unsorted(FILES, at));
            }
            names.push(name);
        }
        answer.by_file.insert(file, names);
    }
    let mut last: Option<Name> = None;
    for _ in 0..counts[CAUSES] {
        let start = r.at;
        let name = r.name()?;
        if last.is_some_and(|last| last >= name) {
            return Err(unsorted(CAUSES, start));
        }
        last = Some(name);
        placed(CAUSES, name)?;
        let serial = Gen(r.u64()?);
        let tag = r.u8()?;
        let tag_error = r.tag(tag);
        let read = r.take(CELL_ID)?;
        let was = r.id()?;
        let now = r.id()?;
        let cause = match tag {
            0 => Cause::New,
            1 => Cause::KeyMoved,
            2 => {
                let mut cell = Rd { bytes: read, at: 0, page };
                Cause::ReadMoved { read: cell.cell_id()?, was, now }
            }
            3 => Cause::Owed,
            4 => Cause::Inline,
            _ => return Err(tag_error),
        };
        kept.causes.insert(name, (serial, cause));
    }
    let mut last: Option<Name> = None;
    for _ in 0..counts[RECORDS] {
        let start = r.at;
        let name = r.name()?;
        if last.is_some_and(|last| last >= name) {
            return Err(unsorted(RECORDS, start));
        }
        last = Some(name);
        placed(RECORDS, name)?;
        kept.records.insert(name, Key(r.id()?));
    }
    if r.at != bytes.len() {
        return Err(PagesError::Trailing { page, at: r.at as u32 });
    }
    Ok(())
}
