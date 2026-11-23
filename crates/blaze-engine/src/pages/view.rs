//! A page read in place. Its bytes come proved ([`Sealed`]); its entries are found through the
//! start table its writer put in its head, so an open checks the head alone and allocates
//! nothing. Lists and their payloads stay in the record.

use super::*;
use crate::RowFacts;

pub(super) fn id(bytes: &[u8]) -> ContentId {
    ContentId(bytes[..ID].try_into().expect("a checked digest"))
}

pub(super) fn name(bytes: &[u8]) -> Name {
    Name::new(Kind::from_u8(bytes[0]).expect("a checked name"), id(&bytes[1..]))
}

fn cell(bytes: &[u8]) -> Cell {
    Cell(bytes[..CELL].try_into().expect("a checked cell"))
}

fn cell_id(bytes: &[u8]) -> CellId {
    CellId { of: name(bytes), cell: cell(&bytes[NAME..]) }
}

pub(super) fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().expect("a checked word"))
}

fn serial(bytes: &[u8]) -> Gen {
    Gen(u64::from_le_bytes(bytes[..8].try_into().expect("a checked generation")))
}

fn phase(bytes: &[u8]) -> Phase {
    Phase::new(Stage::ALL[bytes[0] as usize], u16::from_le_bytes([bytes[1], bytes[2]]))
}

/// The list of names in a stored file plan or reverse reader list.
#[derive(Clone)]
pub struct StoredNames<'b>(core::slice::ChunksExact<'b, u8>);
impl Iterator for StoredNames<'_> {
    type Item = Name;
    fn next(&mut self) -> Option<Name> { self.0.next().map(name) }
    fn size_hint(&self) -> (usize, Option<usize>) { self.0.size_hint() }
}
impl ExactSizeIterator for StoredNames<'_> {}

#[derive(Clone)]
pub struct StoredReads<'b>(core::slice::ChunksExact<'b, u8>);
impl Iterator for StoredReads<'_> {
    type Item = (CellId, ContentId);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|b| (cell_id(b), id(&b[CELL_ID..])))
    }
    fn size_hint(&self) -> (usize, Option<usize>) { self.0.size_hint() }
}
impl ExactSizeIterator for StoredReads<'_> {}

#[derive(Clone)]
pub struct StoredDefines<'b>(core::slice::ChunksExact<'b, u8>);
impl Iterator for StoredDefines<'_> {
    type Item = (Cell, ContentId);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|b| (cell(b), id(&b[CELL..])))
    }
    fn size_hint(&self) -> (usize, Option<usize>) { self.0.size_hint() }
}
impl ExactSizeIterator for StoredDefines<'_> {}

/// A row's checked fixed fields and its borrowed variable-width columns.
#[derive(Clone, Copy)]
pub struct StoredRow<'b> {
    pub facts: RowFacts,
    defines: &'b [u8],
    reads: &'b [u8],
    by_cell: &'b [u8],
}

pub(super) fn row_facts(b: &[u8]) -> RowFacts {
    const PHASE: usize = NAME + ID;
    const FILE: usize = PHASE + 3;
    const SOURCE: usize = FILE + NAME;
    const OUT: usize = SOURCE + ID;
    const SERIAL: usize = OUT + ID;
    const FINDINGS: usize = SERIAL + 8;
    let flags = b[FINDINGS + 4];
    RowFacts {
        key: Key(id(&b[NAME..])), phase: phase(&b[PHASE..]), file: name(&b[FILE..]),
        source: id(&b[SOURCE..]), out: id(&b[OUT..]), serial: serial(&b[SERIAL..]),
        findings: word(&b[FINDINGS..]), inline: flags & INLINE != 0, failed: flags & FAILED != 0,
    }
}

impl<'b> StoredRow<'b> {
    pub(super) fn from_checked(b: &'b [u8]) -> Self {
        let definitions = word(&b[ROW_FIXED - 8..]) as usize;
        let observations = word(&b[ROW_FIXED - 4..]) as usize;
        let (defines, lists) = b[ROW_FIXED..].split_at(definitions * DEFINE);
        let (reads, by_cell) = lists.split_at(observations * READ);
        Self { facts: row_facts(b), defines, reads, by_cell }
    }
    pub fn defines(&self) -> StoredDefines<'b> { StoredDefines(self.defines.chunks_exact(DEFINE)) }
    pub fn reads(&self) -> StoredReads<'b> { StoredReads(self.reads.chunks_exact(READ)) }

    fn position(&self, at: usize) -> usize {
        if self.by_cell.is_empty() { at } else { word(&self.by_cell[at * 4..]) as usize }
    }

    pub fn read_of(&self, wanted: &CellId) -> (Option<ContentId>, u32) {
        let (found, compared) = crate::search(self.reads.len() / READ, |at| cell_id(&self.reads[self.position(at) * READ..]).cmp(wanted));
        (found.map(|at| id(&self.reads[self.position(at) * READ + CELL_ID..])), compared)
    }

    pub fn to_owned<'a>(&self, alloc: SharedAlloc<'a>) -> Row<'a> {
        let f = self.facts;
        let reads = self.reads();
        let by_cell = Vec::from_iter_in((0..reads.len()).map(|at| self.position(at) as u32), alloc);
        Row {
            key: f.key, phase: f.phase, file: f.file, source: f.source, out: f.out, serial: f.serial,
            findings: f.findings, failed: f.failed, inline: f.inline, by_cell,
            reads: Vec::from_iter_in(reads, alloc), defines: Vec::from_iter_in(self.defines(), alloc),
        }
    }
}

fn list<'b>(r: &mut Rd<'b>, n: usize, width: usize) -> Result<&'b [u8], PagesError> {
    let len = n.checked_mul(width).ok_or(PagesError::Short { page: r.page, at: r.at as u32 })?;
    r.take(len)
}

/// Check the fixed fields and delimit the three lists on admission.
pub(super) fn read_row<'b>(r: &mut Rd<'b>) -> Result<StoredRow<'b>, PagesError> {
    let key = Key(r.id()?);
    let phase = r.phase()?;
    let file = r.name()?;
    let source = r.id()?;
    let out = r.id()?;
    let serial = Gen(r.u64()?);
    let findings = r.u32()?;
    let flags = r.u8()?;
    if flags & !(INLINE | FAILED | INDEXED) != 0 { return Err(r.tag(flags)); }
    let definitions = r.u32()? as usize;
    let observations = r.u32()? as usize;
    if observations > crate::READS_CAP {
        return Err(PagesError::Reads { page: r.page, at: r.at as u32, count: observations as u32 });
    }
    let defines = list(r, definitions, DEFINE)?;
    let reads = list(r, observations, READ)?;
    let by_cell = list(r, if flags & INDEXED == 0 { 0 } else { observations }, 4)?;
    Ok(StoredRow {
        facts: RowFacts { key, phase, file, source, out, serial, findings, failed: flags & FAILED != 0, inline: flags & INLINE != 0 },
        defines, reads, by_cell,
    })
}

pub(super) fn check_row(row: &StoredRow<'_>, page: u32, start: usize) -> Result<(), PagesError> {
    let unsorted = || PagesError::Unsorted { page, section: SECTION_NAMES[ROWS], at: start as u32 };
    let mut previous = None;
    for field in row.defines.chunks_exact(DEFINE) {
        if previous.is_some_and(|last: &[u8]| last > &field[..CELL]) { return Err(unsorted()); }
        previous = Some(&field[..CELL]);
    }
    previous = None;
    for at in 0..row.reads.len() / READ {
        let position = row.position(at);
        if position >= row.reads.len() / READ { return Err(unsorted()); }
        let field = &row.reads[position * READ..][..CELL_ID];
        if Kind::from_u8(field[0]).is_none() { return Err(PagesError::Tag { page, at: start as u32, byte: field[0] }); }
        if previous.is_some_and(|last| last >= field) { return Err(unsorted()); }
        previous = Some(field);
    }
    Ok(())
}

fn read_cause(r: &mut Rd<'_>) -> Result<(Gen, Cause), PagesError> {
    let serial = Gen(r.u64()?);
    let tag = r.u8()?;
    let bad_tag = r.tag(tag);
    let read = r.take(CELL_ID)?;
    let was = r.id()?;
    let now = r.id()?;
    let cause = match tag {
        0 => Cause::New,
        1 => Cause::KeyMoved,
        2 => Cause::ReadMoved { read: Rd { bytes: read, at: 0, page: r.page }.cell_id()?, was, now },
        3 => Cause::Owed,
        4 => Cause::Inline,
        _ => return Err(bad_tag),
    };
    Ok((serial, cause))
}

pub(super) fn key_width(section: usize) -> usize {
    if matches!(section, CELLS | OWED_CELLS | READERS) { CELL_ID } else { NAME }
}

pub(super) fn page_records<'a>(section: &[u8], alloc: SharedAlloc<'a>) -> SVec<'a, (Name, Key)> {
    Vec::from_iter_in(section.chunks_exact(NAME + ID).map(|b| (name(b), Key(id(&b[NAME..])))), alloc)
}

/// A page whose bytes are its key's ([`Sealed`]) and whose head delimits its entries: the
/// pages' one reader. Two words and nine indexes; it borrows the record, allocates nothing.
pub struct VerifiedPage<'b> {
    bytes: &'b [u8],
    /// The first entry of each section, and past the last: section `s` is entries
    /// `first[s]..first[s + 1]` of the start table.
    first: [usize; SECTIONS + 1],
}

impl<'b> VerifiedPage<'b> {
    /// The page's length in bytes.
    pub fn len(&self) -> usize { self.bytes.len() }

    /// Where entry `j` starts (the start table's word `j`; word `n` is the page's length).
    fn start(&self, j: usize) -> usize {
        word(&self.bytes[HEAD + 4 * j..]) as usize
    }

    fn entry(&self, j: usize) -> &'b [u8] {
        &self.bytes[self.start(j)..self.start(j + 1)]
    }

    pub(super) fn restore<'r>(&self, kept: &mut Kept<'r>, alloc: SharedAlloc<'r>) {
        for n in self.row_names() {
            let row = self.row(n).expect("a checked row").to_owned(alloc);
            if row.findings > 0 || row.failed { kept.answer.with_findings.insert(n); }
            kept.answer.rows.insert(n, row);
        }
        for b in self.entries(CELLS) {
            kept.answer.cells.insert(cell_id(b), (id(&b[CELL_ID..]), serial(&b[CELL_ID + ID..])));
        }
        for b in self.entries(OWED_CELLS) { kept.answer.owed_cells.insert(cell_id(b), serial(&b[CELL_ID..])); }
        for n in self.entries(OWED_TASKS).map(name) {
            kept.answer.owed_tasks.insert(n, self.owed_task(n).expect("a checked owed plan"));
        }
        for n in self.entries(FILES).map(name) {
            kept.answer.by_file.insert(n, Vec::from_iter_in(self.file_tasks(n), alloc));
        }
        for n in self.entries(CAUSES).map(name) {
            kept.causes.insert(n, self.cause(n).expect("a checked cause"));
        }
        kept.records.extend(self.records());
        for b in self.entries(READERS) {
            let id = cell_id(b);
            let mut readers = BTreeSet::new_in(alloc);
            readers.extend(self.readers(&id));
            kept.answer.readers.insert(id, readers);
        }
    }
    /// Page `page` of a root paged by `bits`, its bytes proved by `sealed`: the head alone is
    /// checked (its form, bits and index; that its start table fits, begins after itself and
    /// ends at the page's length). Nothing is hashed and no entry walked: the store's proof
    /// covers the bytes the writer put, start table and all (the module docs, "Checked once").
    pub fn open(sealed: Sealed<'b>, page: u32, bits: u8) -> Result<Self, PagesError> {
        let bytes = sealed.bytes();
        let mut r = Rd { bytes, at: 0, page };
        if r.take(4)? != PAGE_MAGIC || r.u8()? != bits || r.u32()? != page { return Err(PagesError::Magic { page }); }
        let mut first = [0usize; SECTIONS + 1];
        for section in 0..SECTIONS {
            let count = r.u32()? as usize;
            first[section + 1] = first[section].checked_add(count).ok_or(PagesError::Short { page, at: r.at as u32 })?;
        }
        let entries = first[SECTIONS];
        // Each entry is at least a name: a count past that is a cut page, refused before the
        // table is sized from it.
        if entries > bytes.len() / NAME {
            return Err(PagesError::Short { page, at: r.at as u32 });
        }
        let body_at = super::starts_end(entries);
        if body_at > bytes.len() {
            return Err(PagesError::Short { page, at: bytes.len() as u32 });
        }
        let view = Self { bytes, first };
        if view.start(0) != body_at {
            return Err(PagesError::Starts { page, entry: 0 });
        }
        if view.start(entries) != bytes.len() {
            return Err(PagesError::Starts { page, entry: entries as u32 });
        }
        Ok(view)
    }

    /// [`VerifiedPage::open`], then every entry walked: each count, kind byte, order, page
    /// membership and row list, and each entry's start against where the walk found it. For an
    /// explicit verify and the exhaustive [`decode`]; the edit path never walks.
    pub fn open_whole(sealed: Sealed<'b>, page: u32, bits: u8) -> Result<Self, PagesError> {
        let view = Self::open(sealed, page, bits)?;
        let bytes = view.bytes;
        let mut r = Rd { bytes, at: view.start(0), page };
        for section in 0..SECTIONS {
            let width = key_width(section);
            let unsorted = |at: usize| PagesError::Unsorted { page, section: SECTION_NAMES[section], at: at as u32 };
            let mut previous: Option<usize> = None;
            for j in view.first[section]..view.first[section + 1] {
                let start = r.at;
                if view.start(j) != start {
                    return Err(PagesError::Starts { page, entry: j as u32 });
                }
                let n = r.name()?;
                let cell_bytes = if width == CELL_ID { Some(r.take(CELL)?) } else { None };
                // A readers entry is placed by its cell's slot, every other by its name (the form).
                let placed = match cell_bytes {
                    Some(bytes) if section == READERS => readers_slot(&CellId { of: n, cell: cell(bytes) }),
                    _ => n,
                };
                if page_of(placed, bits) != page { return Err(PagesError::Misplaced { page, section: SECTION_NAMES[section], name: n }); }
                if previous.is_some_and(|prev| bytes[prev..prev + width] >= bytes[start..r.at]) {
                    return Err(unsorted(start));
                }
                previous = Some(start);
                match section {
                    ROWS => {
                        let row = read_row(&mut r)?;
                        check_row(&row, page, start)?;
                    }
                    CELLS => { r.take(ID + 8)?; }
                    OWED_CELLS => { r.take(8)?; }
                    OWED_TASKS => { r.phase()?; r.take(ID)?; r.name()?; }
                    FILES | READERS => {
                        let width = if section == FILES { FILE_MEMBER } else { NAME };
                        let names = r.count(width)?;
                        let mut previous = None;
                        for _ in 0..names {
                            let at = r.at;
                            let one = r.name()?;
                            if previous.is_some_and(|last| last >= one) { return Err(unsorted(at)); }
                            previous = Some(one);
                            if section == FILES {
                                match r.u8()? {
                                    0 => {
                                        if r.take(ID + 3 + ID)?.iter().any(|&b| b != 0) {
                                            return Err(PagesError::Tag { page, at: at as u32, byte: 0 });
                                        }
                                    }
                                    1 => { r.take(ID)?; r.phase()?; r.take(ID)?; }
                                    tag => return Err(r.tag(tag)),
                                }
                            }
                        }
                    }
                    CAUSES => { read_cause(&mut r)?; }
                    RECORDS => { r.take(ID)?; }
                    _ => unreachable!("a page section"),
                }
            }
        }
        // The last entry ends where the table's last word says the page does.
        if r.at != bytes.len() { return Err(PagesError::Trailing { page, at: r.at as u32 }); }
        Ok(view)
    }

    pub(super) fn entries(&self, section: usize) -> impl Iterator<Item = &'b [u8]> + '_ {
        (self.first[section]..self.first[section + 1]).map(|j| self.entry(j))
    }

    pub(super) fn section_count(&self, section: usize) -> usize {
        let from = self.first.get(section).copied().unwrap_or(0);
        let to = self.first.get(section + 1).copied().unwrap_or(from);
        to.saturating_sub(from)
    }

    pub(super) fn section_start(&self, section: usize) -> usize {
        self.start(self.first.get(section).copied().unwrap_or(0))
    }

    pub(super) fn section_entry_start(&self, section: usize, offset: usize) -> usize {
        self.start(self.first.get(section).copied().unwrap_or(0) + offset)
    }

    pub(super) fn section_bytes(&self, section: usize) -> &'b [u8] {
        let from = self.start(self.first.get(section).copied().unwrap_or(0));
        let to = self.start(self.first.get(section + 1).copied().unwrap_or(0));
        self.bytes.get(from..to).unwrap_or_default()
    }

    pub(super) fn readers_raw(&self, wanted: &CellId) -> Option<&'b [u8]> {
        self.find(READERS, wanted, cell_id)
    }

    fn find<K: Ord>(&self, section: usize, wanted: &K, key: impl Fn(&[u8]) -> K) -> Option<&'b [u8]> {
        let (from, to) = (self.first[section], self.first[section + 1]);
        let (mut lo, mut hi) = (from, to);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match key(&self.bytes[self.start(mid)..]).cmp(wanted) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(self.entry(mid)),
            }
        }
        None
    }

    pub fn row_names(&self) -> impl Iterator<Item = Name> + '_ { self.entries(ROWS).map(name) }

    pub fn row(&self, wanted: Name) -> Option<StoredRow<'b>> {
        self.find(ROWS, &wanted, name).map(StoredRow::from_checked)
    }

    pub fn cell(&self, wanted: &CellId) -> Option<(ContentId, Gen)> {
        let b = self.find(CELLS, wanted, cell_id)?;
        Some((id(&b[CELL_ID..]), serial(&b[CELL_ID + ID..])))
    }

    pub fn owed_cell(&self, wanted: &CellId) -> Option<Gen> {
        self.find(OWED_CELLS, wanted, cell_id).map(|b| serial(&b[CELL_ID..]))
    }

    pub fn owed_task(&self, wanted: Name) -> Option<Planned> {
        let bytes = self.find(OWED_TASKS, &wanted, name)?;
        Some(Planned { name: wanted, phase: phase(&bytes[NAME..]), source: id(&bytes[NAME + 3..]), file: name(&bytes[NAME + 3 + ID..]) })
    }

    pub fn cause(&self, wanted: Name) -> Option<(Gen, Cause)> {
        let bytes = self.find(CAUSES, &wanted, name)?;
        Some(read_cause(&mut Rd { bytes, at: NAME, page: 0 }).expect("a checked cause"))
    }

    pub fn record(&self, wanted: Name) -> Option<Key> {
        self.find(RECORDS, &wanted, name).map(|b| Key(id(&b[NAME..])))
    }

    pub fn records(&self) -> impl Iterator<Item = (Name, Key)> + '_ {
        self.entries(RECORDS).map(|b| (name(b), Key(id(&b[NAME..]))))
    }

    pub fn readers(&self, wanted: &CellId) -> StoredNames<'b> {
        let bytes = self.find(READERS, wanted, cell_id).map_or(&[][..], |b| &b[CELL_ID + 4..]);
        StoredNames(bytes.chunks_exact(NAME))
    }

    pub fn file_tasks(&self, wanted: Name) -> StoredNames<'b> {
        StoredNames(self.file_members(wanted).chunks_exact(FILE_MEMBER))
    }

    pub(super) fn file_members(&self, wanted: Name) -> &'b [u8] {
        self.find(FILES, &wanted, name).map_or(&[][..], |b| &b[NAME + 4..])
    }

    pub(super) fn member<'s>(members: &'s [u8], wanted: Name) -> Option<&'s [u8]> {
        let (at, _) = crate::search(members.len() / FILE_MEMBER, |at| name(&members[at * FILE_MEMBER..]).cmp(&wanted));
        at.map(|at| &members[at * FILE_MEMBER..][..FILE_MEMBER])
    }

    /// A successful lookup certifies a row under exactly this key, present and unowed.
    pub fn planned_key(&self, file: Name, wanted: Name) -> Option<Key> {
        let b = Self::member(self.file_members(file), wanted)?;
        (b[NAME] == 1).then(|| Key(id(&b[NAME + 1..])))
    }
}
