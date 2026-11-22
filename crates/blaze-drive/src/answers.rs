//! The answer table (drive.md, "Records and validity"): the checkout's index from each name to
//! what it is now, kept as content-addressed pages under a root, and the `last` pointer that
//! names the root. A row is the forward index (its key, where its record is, its read list) and
//! the reverse one (its readers) at once; nothing of the table is held in memory but the pages a
//! run touches, and those are read in place ([`PageView`]): a row and its readers are slices of
//! the page's bytes, never a decoded copy.

use core::fmt;
use core::marker::PhantomData;

use blaze_id::ContentId;
use blaze_own::{ARc, AVec, Alloc, Rc, Vec};

use crate::cells::{CELL_BYTES, Cell, CellRead, CellReader};
use crate::condition::{Condition, MOST_ATOMS, Var};
use crate::key::{ABSENT, Key, Kind, Name};
use crate::wire::{NAME_BYTES, Reader, WireError, Writer};

/// 2: a row holds its split and its condition. 3: and its cell reads and cell readers
/// (cells.rs). 4: read in place ([`PageView`]): fixed-width rows sorted by name, each list's
/// entries in a tail of fixed-width entries. A page of an older form is never fetched: the form
/// is in the tool's key (crate::FORMATS), so the table is made again.
pub(crate) const PAGE_MAGIC: &[u8; 4] = b"BDP4";
/// 2: the root holds the table's page bytes, which size its pages ([`bits_for`]).
pub(crate) const ROOT_MAGIC: &[u8; 4] = b"BDA2";
/// The `last` file's form: read at a fixed path, so its caller checks this before
/// [`read_last`] and rebuilds a file of another form (crate::FORMATS).
pub const LAST_MAGIC: &[u8; 4] = b"BDL1";

/// The bytes a page aims to hold; the root's bit count is the fewest that keep every page at or
/// under it on average. Sized by bytes, not rows: a command reads whole pages to reach the few
/// rows it needs, and a row's lists (its reads and the reverse index) vary from none to
/// thousands. At 512 rows a page, tempo's table was 128 pages of about 20 MB each (2.6 GB in
/// all), and a one-statement edit read all 128: 2.7 GB of footprint and about 3.7 ms of step a
/// page (check-g044908-body, 2026-11-17). At this size a page is tens of KB.
pub const PAGE_BYTES: u64 = 64 << 10;
/// The most bits a root pages by: 65,536 pages (a 2 MiB root), 4 GiB at [`PAGE_BYTES`].
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
    /// Its part reads, sorted by (name, cell): a reader of one cell of a record.
    pub cells: AVec<'a, CellRead>,
    /// The names its record lists.
    pub parts: AVec<'a, Name>,
    /// The names whose read lists hold it whole, sorted: the reverse index.
    pub readers: AVec<'a, Name>,
    /// The readers of one of its cells, each with what it saw there, sorted: the reverse index
    /// of part reads, so a change dirties only the readers whose cell moved.
    pub cell_readers: AVec<'a, CellReader>,
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

/// Which page of a root of `bits` holds `name`: its subject's leading bits.
pub fn page_of(name: Name, bits: u8) -> u32 {
    let lead = u32::from(name.subject.0[0]) << 8 | u32::from(name.subject.0[1]);
    match bits {
        0 => 0,
        bits => lead >> (16 - u32::from(bits.min(MOST_BITS))),
    }
}

/// The bit count for a table of `bytes` of pages.
pub fn bits_for(bytes: u64) -> u8 {
    let mut bits = 0;
    while bits < MOST_BITS && bytes > PAGE_BYTES << bits {
        bits += 1;
    }
    bits
}

/// The bits a table of about `bytes` is paged by, given the bits it was paged by: kept while
/// they are within two of what its size asks (each page between a quarter of and
/// [`PAGE_BYTES`] on average), so a table near a boundary is not paged again whole each command.
pub fn bits_kept(bytes: u64, old: Option<u8>) -> u8 {
    let need = bits_for(bytes);
    match old {
        Some(old) if old >= need && old <= need.saturating_add(2) => old,
        _ => need,
    }
}

/// The bytes `row` takes in a page with no readers yet: its table row and its own lists. A
/// table's size before its pages are written is this over its rows, plus each reader entry.
pub(crate) fn row_bytes(row: &Row<'_>) -> u64 {
    let entries = [row.reads.len(), row.cells.len(), row.parts.len(), row.readers.len(), row.cell_readers.len(), usize::from(row.cond.is_some())];
    let lists: usize = entries.iter().zip(WIDTHS).map(|(n, width)| n * width).sum();
    (ROW_BYTES + lists) as u64
}

// --- The page form, read in place ----------------------------------------------------------
//
// A page is three parts, every field little-endian and fixed-width:
//
// - the header (`HEAD` bytes): the magic, the row count, then each tail's entry count;
// - the row table: one `ROW_BYTES` row a name, sorted by name (kind first): the name, key, at,
//   source, out, home, place and split, then for each list (reads, cell reads, parts, readers,
//   cell readers, condition) its first entry and its count in that list's tail;
// - the tails, one a list in that order: each row's entries, rows in table order, each entry
//   `WIDTHS[list]` bytes, sorted within the row as its writer keeps them.
//
// `PageView::open` checks it all once (sizes, the table's order, every range in its tail, every
// kind byte, each list's order); after it a row is a binary search of the table and a list a
// slice of its tail, read in place.

/// The lists a row has, in their order in a row and in the page's tails.
const LISTS: usize = 6;
const READS: usize = 0;
const CELLS: usize = 1;
const PARTS: usize = 2;
const READERS: usize = 3;
const CELL_READERS: usize = 4;
/// Zero or one entry: the name a fork is of and the condition it runs under.
const CONDS: usize = 5;
const LIST_NAMES: [&str; LISTS] = ["reads", "cell reads", "parts", "readers", "cell readers", "condition"];
/// A condition in place: its atom count, then [`MOST_ATOMS`] (variable, value) pairs, the
/// unused ones zero (one encoding, rule 25).
const COND_BYTES: usize = 1 + 4 * MOST_ATOMS;
const CELL_ENTRY: usize = NAME_BYTES + CELL_BYTES + 32;
const WIDTHS: [usize; LISTS] = [NAME_BYTES + 32, CELL_ENTRY, NAME_BYTES, NAME_BYTES, CELL_ENTRY, NAME_BYTES + COND_BYTES];
/// The header: the magic, the row count, each tail's entry count.
const HEAD: usize = 4 + 4 + 4 * LISTS;
const O_KEY: usize = NAME_BYTES;
const O_AT: usize = O_KEY + 32;
const O_SOURCE: usize = O_AT + 32;
const O_OUT: usize = O_SOURCE + 32;
const O_HOME: usize = O_OUT + 32;
const O_PLACE: usize = O_HOME + NAME_BYTES;
const O_SPLIT: usize = O_PLACE + 4;
const O_LISTS: usize = O_SPLIT + 4;
/// One row of the table: 250 bytes.
const ROW_BYTES: usize = O_LISTS + 8 * LISTS;
/// The split field of a row with none.
const NO_SPLIT: u32 = u32::MAX;

/// Why bytes are not an answer page of this form. Each names the row and the list it is about
/// (rule 19); a page refused is never read past (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageError {
    /// Fewer bytes than the header, or than its counts need.
    Short { len: u64, need: u64 },
    /// The first four bytes are not this form's.
    Magic,
    /// Bytes past the end the counts give.
    Trailing { len: u64, need: u64 },
    /// A name of row `row` (its own or its home) has a byte that is no kind.
    Kind { row: u32, byte: u8 },
    /// Row `row`'s name is not after row `row - 1`'s: the table is not sorted, or has a name
    /// twice.
    Unsorted { row: u32 },
    /// Row `row`'s split is no variable.
    Split { row: u32, split: u32 },
    /// Row `row`'s `list` ends past its tail.
    PastEnd { row: u32, list: &'static str },
    /// Row `row`'s `list` does not start where the row before it ended (one encoding).
    Misplaced { row: u32, list: &'static str },
    /// Entries of `list`'s tail no row claims.
    Unclaimed { list: &'static str },
    /// Row `row` has more than one condition.
    Conditions { row: u32 },
    /// An entry of row `row`'s `list` is not one (a kind byte, a condition).
    Entry { row: u32, list: &'static str },
    /// Row `row`'s `list` is not sorted.
    UnsortedList { row: u32, list: &'static str },
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a stored answer page is not one: ")?;
        match self {
            PageError::Short { len, need } => write!(f, "{len} bytes, its counts need {need}")?,
            PageError::Magic => f.write_str("its magic is not BDP4")?,
            PageError::Trailing { len, need } => write!(f, "{len} bytes, its counts give {need}")?,
            PageError::Kind { row, byte } => write!(f, "row {row} names kind {byte}, which is none")?,
            PageError::Unsorted { row } => write!(f, "row {row}'s name is not after row {}'s", row.saturating_sub(1))?,
            PageError::Split { row, split } => write!(f, "row {row}'s split {split} is no variable")?,
            PageError::PastEnd { row, list } => write!(f, "row {row}'s {list} end past their tail")?,
            PageError::Misplaced { row, list } => write!(f, "row {row}'s {list} do not start where the row before ended")?,
            PageError::Unclaimed { list } => write!(f, "the {list} tail holds entries no row claims")?,
            PageError::Conditions { row } => write!(f, "row {row} has more than one condition")?,
            PageError::Entry { row, list } => write!(f, "an entry of row {row}'s {list} is not one")?,
            PageError::UnsortedList { row, list } => write!(f, "row {row}'s {list} are not sorted")?,
        }
        f.write_str(" (rule 3: refused, not read past)")
    }
}

impl core::error::Error for PageError {}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    ContentId(id)
}

fn kind_ok(bytes: &[u8], at: usize) -> bool {
    Kind::from_u8(bytes[at]).is_some()
}

/// A name of a page [`PageView::open`] checked: its kind byte is a kind.
fn name_at(bytes: &[u8], at: usize) -> Name {
    let kind = Kind::from_u8(bytes[at]).expect("PageView::open checked every name's kind byte");
    Name { kind, subject: id_at(bytes, at + 1) }
}

fn cell_at(bytes: &[u8], at: usize) -> Cell {
    let mut cell = [0; CELL_BYTES];
    cell.copy_from_slice(&bytes[at..at + CELL_BYTES]);
    Cell(cell)
}

/// A condition in place ([`COND_BYTES`]): sorted, no variable twice, at most [`MOST_ATOMS`]
/// atoms, the unused pairs zero; `None` for bytes that are not one.
fn condition_at(bytes: &[u8]) -> Option<Condition> {
    let n = usize::from(bytes[0]);
    if n > MOST_ATOMS {
        return None;
    }
    let mut condition = Condition::ANY;
    let mut last: Option<Var> = None;
    for i in 0..n {
        let at = 1 + 4 * i;
        let var = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let value = u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]);
        if last.is_some_and(|last| last >= var) {
            return None;
        }
        last = Some(var);
        condition = match condition.with(var, value) {
            Ok(Some(next)) => next,
            _ => return None,
        };
    }
    bytes[1 + 4 * n..COND_BYTES].iter().all(|byte| *byte == 0).then_some(condition)
}

fn put32(page: &mut AVec<'_, u8>, value: usize) {
    let value = u32::try_from(value).expect("a page's counts fit u32 (a page holds about PAGE_BYTES)");
    page.extend_from_slice(&value.to_le_bytes());
}

fn put_id(page: &mut AVec<'_, u8>, id: &ContentId) {
    page.extend_from_slice(&id.0);
}

fn put_name(page: &mut AVec<'_, u8>, name: Name) {
    page.push(name.kind as u8);
    put_id(page, &name.subject);
}

fn put_cell(page: &mut AVec<'_, u8>, cell: Cell) {
    page.extend_from_slice(&cell.0);
}

fn put_condition(page: &mut AVec<'_, u8>, condition: &Condition) {
    page.push(condition.atoms().len() as u8);
    for (var, value) in condition.atoms() {
        page.extend_from_slice(&var.to_le_bytes());
        page.extend_from_slice(&value.to_le_bytes());
    }
    for _ in condition.atoms().len()..MOST_ATOMS {
        page.extend_from_slice(&[0; 4]);
    }
}

/// An entry of a row's list in a page: fixed width, and ordered as the value it decodes to, which
/// is the order its writer keeps the list in.
pub trait Entry: Copy + Ord + 'static {
    const BYTES: usize;
    /// Whether raw wire-byte lexicographical order equals [`Ord`], allowing [`PageView::open`] to
    /// validate list sortedness directly on byte slices without per-entry struct decoding.
    const RAW_ORD: bool = false;
    /// The entry `bytes` hold ([`Entry::BYTES`] of them), which [`PageView::open`] checked.
    fn read(bytes: &[u8]) -> Self;
    fn write(&self, page: &mut AVec<'_, u8>);
    /// Whether `bytes` are an entry: [`Entry::read`] of them decodes.
    fn check(bytes: &[u8]) -> bool;
}

impl Entry for Name {
    const BYTES: usize = NAME_BYTES;
    const RAW_ORD: bool = true;
    fn read(bytes: &[u8]) -> Name {
        name_at(bytes, 0)
    }
    fn write(&self, page: &mut AVec<'_, u8>) {
        put_name(page, *self);
    }
    fn check(bytes: &[u8]) -> bool {
        kind_ok(bytes, 0)
    }
}

impl Entry for (Name, ContentId) {
    const BYTES: usize = NAME_BYTES + 32;
    const RAW_ORD: bool = true;
    fn read(bytes: &[u8]) -> (Name, ContentId) {
        (name_at(bytes, 0), id_at(bytes, NAME_BYTES))
    }
    fn write(&self, page: &mut AVec<'_, u8>) {
        put_name(page, self.0);
        put_id(page, &self.1);
    }
    fn check(bytes: &[u8]) -> bool {
        kind_ok(bytes, 0)
    }
}

impl Entry for CellRead {
    const BYTES: usize = CELL_ENTRY;
    const RAW_ORD: bool = true;
    fn read(bytes: &[u8]) -> CellRead {
        CellRead { name: name_at(bytes, 0), cell: cell_at(bytes, NAME_BYTES), saw: id_at(bytes, NAME_BYTES + CELL_BYTES) }
    }
    fn write(&self, page: &mut AVec<'_, u8>) {
        put_name(page, self.name);
        put_cell(page, self.cell);
        put_id(page, &self.saw);
    }
    fn check(bytes: &[u8]) -> bool {
        kind_ok(bytes, 0)
    }
}

impl Entry for CellReader {
    const BYTES: usize = CELL_ENTRY;
    const RAW_ORD: bool = true;
    fn read(bytes: &[u8]) -> CellReader {
        CellReader { reader: name_at(bytes, 0), cell: cell_at(bytes, NAME_BYTES), saw: id_at(bytes, NAME_BYTES + CELL_BYTES) }
    }
    fn write(&self, page: &mut AVec<'_, u8>) {
        put_name(page, self.reader);
        put_cell(page, self.cell);
        put_id(page, &self.saw);
    }
    fn check(bytes: &[u8]) -> bool {
        kind_ok(bytes, 0)
    }
}

impl Entry for (Name, Condition) {
    const BYTES: usize = NAME_BYTES + COND_BYTES;
    fn read(bytes: &[u8]) -> (Name, Condition) {
        (name_at(bytes, 0), condition_at(&bytes[NAME_BYTES..]).expect("PageView::open checked every condition"))
    }
    fn write(&self, page: &mut AVec<'_, u8>) {
        put_name(page, self.0);
        put_condition(page, &self.1);
    }
    fn check(bytes: &[u8]) -> bool {
        kind_ok(bytes, 0) && condition_at(&bytes[NAME_BYTES..]).is_some()
    }
}

/// A row's list in place: a slice of the page's tail, each entry decoded as it is read. No
/// vector and no copy of the list.
pub struct Entries<'p, E> {
    raw: &'p [u8],
    of: PhantomData<fn() -> E>,
}

impl<E> Clone for Entries<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for Entries<'_, E> {}

impl<'p, E: Entry> Entries<'p, E> {
    pub fn len(&self) -> usize {
        self.raw.len() / E::BYTES
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// The list's bytes in the page.
    pub fn raw(&self) -> &'p [u8] {
        self.raw
    }

    pub fn get(&self, i: usize) -> E {
        E::read(&self.raw[i * E::BYTES..(i + 1) * E::BYTES])
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = E> + Clone + 'p {
        self.raw.chunks_exact(E::BYTES).map(E::read)
    }

    pub fn binary_search_by_key<K: Ord>(&self, key: &K, f: impl Fn(E) -> K) -> Result<usize, usize> {
        search(self.len(), |i| f(self.get(i)).cmp(key))
    }

    pub fn binary_search(&self, entry: &E) -> Result<usize, usize> {
        self.binary_search_by_key(entry, |e| e)
    }
}

/// The index `probe` says is equal, or where it would go: `probe(i)` orders entry `i` against
/// the one sought.
fn search(len: usize, probe: impl Fn(usize) -> core::cmp::Ordering) -> Result<usize, usize> {
    let (mut low, mut high) = (0, len);
    while low < high {
        let mid = low + (high - low) / 2;
        match probe(mid) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(low)
}

/// A row's list as the request reads it: a made row's vector, or an old row's list in place.
pub enum List<'r, E> {
    Held(&'r [E]),
    Page(Entries<'r, E>),
}

impl<E> Clone for List<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for List<'_, E> {}

impl<'r, E: Entry> List<'r, E> {
    pub fn len(&self) -> usize {
        match self {
            List::Held(held) => held.len(),
            List::Page(page) => page.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> E {
        match self {
            List::Held(held) => held[i],
            List::Page(page) => page.get(i),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = E> + 'r {
        let list = *self;
        (0..list.len()).map(move |i| list.get(i))
    }

    pub fn binary_search_by_key<K: Ord>(&self, key: &K, f: impl Fn(E) -> K) -> Result<usize, usize> {
        search(self.len(), |i| f(self.get(i)).cmp(key))
    }

    pub fn binary_search(&self, entry: &E) -> Result<usize, usize> {
        self.binary_search_by_key(entry, |e| e)
    }

    /// The list copied into `alloc`: for a row this run makes from it.
    pub fn to_vec_in<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, E> {
        let mut list = Vec::with_capacity_in(self.len(), alloc);
        list.extend(self.iter());
        list
    }
}

/// Where a page's row table and tails are, from its header: what [`PageView::open`] checked.
#[derive(Clone, Copy, Debug)]
struct Shape {
    rows: usize,
    /// Each tail's first byte.
    tails: [usize; LISTS],
}

impl Shape {
    const EMPTY: Shape = Shape { rows: 0, tails: [0; LISTS] };
}

/// An answer page read in place, checked once by [`PageView::open`].
#[derive(Clone, Copy)]
pub struct PageView<'p> {
    bytes: &'p [u8],
    shape: Shape,
}

impl<'p> PageView<'p> {
    /// The page `bytes` hold, checked whole once: after this no read of it fails or copies.
    pub fn open(bytes: &'p [u8]) -> Result<PageView<'p>, PageError> {
        let len = bytes.len() as u64;
        if bytes.len() < HEAD {
            return Err(PageError::Short { len, need: HEAD as u64 });
        }
        if bytes[..4] != PAGE_MAGIC[..] {
            return Err(PageError::Magic);
        }
        let rows = le32(bytes, 4) as usize;
        let mut counts = [0usize; LISTS];
        let mut need = HEAD as u64 + rows as u64 * ROW_BYTES as u64;
        for (list, count) in counts.iter_mut().enumerate() {
            *count = le32(bytes, 8 + 4 * list) as usize;
            need += *count as u64 * WIDTHS[list] as u64;
        }
        if len < need {
            return Err(PageError::Short { len, need });
        }
        if len > need {
            return Err(PageError::Trailing { len, need });
        }
        // Every size below fits: `need` is at most the slice's length.
        let mut tails = [0usize; LISTS];
        let mut at = HEAD + rows * ROW_BYTES;
        for list in 0..LISTS {
            tails[list] = at;
            at += counts[list] * WIDTHS[list];
        }
        let page = PageView { bytes, shape: Shape { rows, tails } };
        let mut claimed = [0usize; LISTS];
        let mut last: Option<&[u8]> = None;
        for row in 0..rows {
            let r = page.row_bytes(row);
            let at_row = row as u32;
            for at in [0, O_HOME] {
                if !kind_ok(r, at) {
                    return Err(PageError::Kind { row: at_row, byte: r[at] });
                }
            }
            // `Name` wire order (`[kind as u8, subject[0..32]]`) equals `Name::Ord`.
            let name = &r[..NAME_BYTES];
            if last.is_some_and(|prev| prev >= name) {
                return Err(PageError::Unsorted { row: at_row });
            }
            last = Some(name);
            let split = le32(r, O_SPLIT);
            if split != NO_SPLIT && Var::try_from(split).is_err() {
                return Err(PageError::Split { row: at_row, split });
            }
            for list in 0..LISTS {
                let start = le32(r, O_LISTS + 8 * list) as usize;
                let count = le32(r, O_LISTS + 8 * list + 4) as usize;
                if start.checked_add(count).is_none_or(|end| end > counts[list]) {
                    return Err(PageError::PastEnd { row: at_row, list: LIST_NAMES[list] });
                }
                if start != claimed[list] {
                    return Err(PageError::Misplaced { row: at_row, list: LIST_NAMES[list] });
                }
                claimed[list] += count;
            }
            if le32(r, O_LISTS + 8 * CONDS + 4) > 1 {
                return Err(PageError::Conditions { row: at_row });
            }
            let view = RowView { page, row };
            let checks = [
                check_list(view.reads()),
                check_list(view.cells()),
                check_list(view.parts()),
                check_list(view.readers()),
                check_list(view.cell_readers()),
                check_list(view.list::<(Name, Condition)>(CONDS)),
            ];
            for (list, check) in checks.iter().enumerate() {
                match check {
                    Ok(()) => {}
                    Err(ListWhy::Entry) => return Err(PageError::Entry { row: at_row, list: LIST_NAMES[list] }),
                    Err(ListWhy::Unsorted) => return Err(PageError::UnsortedList { row: at_row, list: LIST_NAMES[list] }),
                }
            }
        }
        for list in 0..LISTS {
            if claimed[list] != counts[list] {
                return Err(PageError::Unclaimed { list: LIST_NAMES[list] });
            }
        }
        Ok(page)
    }

    /// The page with no row: an empty page's key is the zero key, and nothing is stored for it.
    pub fn empty() -> PageView<'static> {
        PageView { bytes: &[], shape: Shape::EMPTY }
    }

    pub fn len(&self) -> usize {
        self.shape.rows
    }

    pub fn is_empty(&self) -> bool {
        self.shape.rows == 0
    }

    fn row_bytes(&self, row: usize) -> &'p [u8] {
        let at = HEAD + row * ROW_BYTES;
        &self.bytes[at..at + ROW_BYTES]
    }

    #[inline]
    fn row_name_bytes(&self, row: usize) -> &'p [u8] {
        let at = HEAD + row * ROW_BYTES;
        &self.bytes[at..at + NAME_BYTES]
    }

    /// The `row`-th row of the table, in name order.
    pub fn row_at(&self, row: usize) -> RowView<'p> {
        assert!(row < self.shape.rows, "row {row} of a page of {}", self.shape.rows);
        RowView { page: *self, row }
    }

    /// Every row, in name order.
    pub fn rows(&self) -> impl Iterator<Item = RowView<'p>> + 'p {
        let page = *self;
        (0..page.shape.rows).map(move |row| RowView { page, row })
    }

    /// `name`'s row: interpolation search over the uniform BLAKE3 subject prefix within the
    /// matching kind (O(log log N)), falling back cleanly to binary search when narrow, across
    /// kind boundaries, or on uneven distributions (O(log N) worst case).
    pub fn row(&self, name: Name) -> Option<RowView<'p>> {
        let mut target = [0u8; NAME_BYTES];
        target[0] = name.kind as u8;
        target[1..].copy_from_slice(&name.subject.0);
        self.find_row_counted(&target, None).map(|row| RowView { page: *self, row })
    }

    /// Finds `target` in the sorted row table; if `probes` is `Some`, counts row comparisons.
    fn find_row_counted(&self, target: &[u8; NAME_BYTES], mut probes: Option<&mut usize>) -> Option<usize> {
        #[inline]
        fn prefix64(bytes: &[u8]) -> u64 {
            u64::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8]])
        }

        let n = self.shape.rows;
        if n == 0 {
            return None;
        }
        let mut bump = || {
            if let Some(p) = probes.as_deref_mut() {
                *p += 1;
            }
        };

        let mut lo = 0usize;
        let mut hi = n - 1;
        let mut lo_b = self.row_name_bytes(lo);
        bump();
        match lo_b.cmp(target.as_slice()) {
            core::cmp::Ordering::Equal => return Some(lo),
            core::cmp::Ordering::Greater => return None,
            core::cmp::Ordering::Less => {}
        }
        if lo == hi {
            return None;
        }
        let mut hi_b = self.row_name_bytes(hi);
        bump();
        match hi_b.cmp(target.as_slice()) {
            core::cmp::Ordering::Equal => return Some(hi),
            core::cmp::Ordering::Less => return None,
            core::cmp::Ordering::Greater => {}
        }

        let target_p = prefix64(target);
        let mut stall = 0u8;
        while hi - lo > 1 {
            let span = hi - lo;
            if span <= 4 {
                // Tight binary search on the exclusive interior `(lo + 1)..hi`.
                let mut left = lo + 1;
                let mut right = hi;
                while left < right {
                    let mid = left + (right - left) / 2;
                    bump();
                    match self.row_name_bytes(mid).cmp(target.as_slice()) {
                        core::cmp::Ordering::Less => left = mid + 1,
                        core::cmp::Ordering::Greater => right = mid,
                        core::cmp::Ordering::Equal => return Some(mid),
                    }
                }
                return None;
            }

            let probe = if stall < 3 && lo_b[0] == hi_b[0] {
                let lo_p = prefix64(lo_b);
                let hi_p = prefix64(hi_b);
                if hi_p > lo_p {
                    // `lo_b < target < hi_b` with equal kind guarantees `lo_p <= target_p <= hi_p`.
                    let den = u128::from(hi_p - lo_p);
                    let offset = ((u128::from(target_p - lo_p) * span as u128 + den / 2) / den) as usize;
                    (lo + offset).clamp(lo + 1, hi - 1)
                } else {
                    lo + span / 2
                }
            } else {
                stall = 0;
                lo + span / 2
            };

            let p_b = self.row_name_bytes(probe);
            bump();
            match p_b.cmp(target.as_slice()) {
                core::cmp::Ordering::Equal => return Some(probe),
                core::cmp::Ordering::Less => {
                    lo = probe;
                    lo_b = p_b;
                }
                core::cmp::Ordering::Greater => {
                    hi = probe;
                    hi_b = p_b;
                }
            }

            let new_span = hi - lo;
            if new_span > span / 2 {
                stall = stall.saturating_add(1);
            } else {
                stall = 0;
            }
        }
        None
    }
}

/// Why one list of a row is refused.
#[derive(Clone, Copy)]
enum ListWhy {
    Entry,
    Unsorted,
}

/// Each entry of `list` checked ([`Entry::check`]) before it is decoded, and the list in order.
/// When `E::RAW_ORD` holds, compares raw wire byte slices directly without decoding each entry.
fn check_list<E: Entry>(list: Entries<'_, E>) -> Result<(), ListWhy> {
    if E::RAW_ORD {
        let mut last: Option<&[u8]> = None;
        for bytes in list.raw.chunks_exact(E::BYTES) {
            if !E::check(bytes) {
                return Err(ListWhy::Entry);
            }
            if last.is_some_and(|prev| prev > bytes) {
                return Err(ListWhy::Unsorted);
            }
            last = Some(bytes);
        }
        return Ok(());
    }
    let mut last: Option<E> = None;
    for bytes in list.raw.chunks_exact(E::BYTES) {
        if !E::check(bytes) {
            return Err(ListWhy::Entry);
        }
        let entry = E::read(bytes);
        if last.is_some_and(|last| last > entry) {
            return Err(ListWhy::Unsorted);
        }
        last = Some(entry);
    }
    Ok(())
}

/// One row of a page, in place: each field read from the row table, each list a slice of its
/// tail.
#[derive(Clone, Copy)]
pub struct RowView<'p> {
    page: PageView<'p>,
    row: usize,
}

impl<'p> RowView<'p> {
    fn bytes(&self) -> &'p [u8] {
        self.page.row_bytes(self.row)
    }

    pub fn name(&self) -> Name {
        name_at(self.bytes(), 0)
    }

    pub fn key(&self) -> Key {
        Key(id_at(self.bytes(), O_KEY))
    }

    pub fn at(&self) -> Key {
        Key(id_at(self.bytes(), O_AT))
    }

    pub fn source(&self) -> ContentId {
        id_at(self.bytes(), O_SOURCE)
    }

    pub fn out(&self) -> ContentId {
        id_at(self.bytes(), O_OUT)
    }

    pub fn home(&self) -> Name {
        name_at(self.bytes(), O_HOME)
    }

    pub fn place(&self) -> u32 {
        le32(self.bytes(), O_PLACE)
    }

    pub fn split(&self) -> Option<Var> {
        match le32(self.bytes(), O_SPLIT) {
            NO_SPLIT => None,
            // Checked at `open` to fit a variable.
            var => Var::try_from(var).ok(),
        }
    }

    pub fn cond(&self) -> Option<(Name, Condition)> {
        self.list::<(Name, Condition)>(CONDS).iter().next()
    }

    /// [`Row::watching`]: no record, its readers only.
    pub fn watching(&self) -> bool {
        self.key() == Key::default()
    }

    fn list<E: Entry>(&self, list: usize) -> Entries<'p, E> {
        debug_assert_eq!(E::BYTES, WIDTHS[list], "list {list}'s entry width");
        let bytes = self.bytes();
        let start = le32(bytes, O_LISTS + 8 * list) as usize;
        let count = le32(bytes, O_LISTS + 8 * list + 4) as usize;
        let at = self.page.shape.tails[list] + start * E::BYTES;
        Entries { raw: &self.page.bytes[at..at + count * E::BYTES], of: PhantomData }
    }

    pub fn reads(&self) -> Entries<'p, (Name, ContentId)> {
        self.list(READS)
    }

    pub fn cells(&self) -> Entries<'p, CellRead> {
        self.list(CELLS)
    }

    pub fn parts(&self) -> Entries<'p, Name> {
        self.list(PARTS)
    }

    /// The names whose read lists hold this one whole, sorted: the reverse index, in place.
    pub fn readers(&self) -> Entries<'p, Name> {
        self.list(READERS)
    }

    /// The readers of one of its cells, each with what it saw, sorted, in place.
    pub fn cell_readers(&self) -> Entries<'p, CellReader> {
        self.list(CELL_READERS)
    }

    /// The row copied into `alloc`: for one this run changes (a `Given` watch row's `out`).
    pub fn to_row<'b>(&self, alloc: Alloc<'b>) -> Row<'b> {
        Row {
            key: self.key(),
            at: self.at(),
            source: self.source(),
            out: self.out(),
            home: self.home(),
            place: self.place(),
            reads: List::Page(self.reads()).to_vec_in(alloc.clone()),
            cells: List::Page(self.cells()).to_vec_in(alloc.clone()),
            parts: List::Page(self.parts()).to_vec_in(alloc.clone()),
            readers: List::Page(self.readers()).to_vec_in(alloc.clone()),
            cell_readers: List::Page(self.cell_readers()).to_vec_in(alloc),
            split: self.split(),
            cond: self.cond(),
        }
    }
}

/// A row the request reads: one made this run, or the last answer's, in place.
#[derive(Clone, Copy)]
pub enum RowRef<'r, 'a> {
    Made(&'r Row<'a>),
    Old(RowView<'r>),
}

impl<'r> RowRef<'r, '_> {
    pub fn key(self) -> Key {
        match self {
            RowRef::Made(row) => row.key,
            RowRef::Old(row) => row.key(),
        }
    }

    pub fn at(self) -> Key {
        match self {
            RowRef::Made(row) => row.at,
            RowRef::Old(row) => row.at(),
        }
    }

    pub fn source(self) -> ContentId {
        match self {
            RowRef::Made(row) => row.source,
            RowRef::Old(row) => row.source(),
        }
    }

    pub fn out(self) -> ContentId {
        match self {
            RowRef::Made(row) => row.out,
            RowRef::Old(row) => row.out(),
        }
    }

    pub fn home(self) -> Name {
        match self {
            RowRef::Made(row) => row.home,
            RowRef::Old(row) => row.home(),
        }
    }

    pub fn place(self) -> u32 {
        match self {
            RowRef::Made(row) => row.place,
            RowRef::Old(row) => row.place(),
        }
    }

    pub fn split(self) -> Option<Var> {
        match self {
            RowRef::Made(row) => row.split,
            RowRef::Old(row) => row.split(),
        }
    }

    pub fn cond(self) -> Option<(Name, Condition)> {
        match self {
            RowRef::Made(row) => row.cond,
            RowRef::Old(row) => row.cond(),
        }
    }

    pub fn watching(self) -> bool {
        self.key() == Key::default()
    }

    pub fn reads(self) -> List<'r, (Name, ContentId)> {
        match self {
            RowRef::Made(row) => List::Held(row.reads.as_slice()),
            RowRef::Old(row) => List::Page(row.reads()),
        }
    }

    pub fn cells(self) -> List<'r, CellRead> {
        match self {
            RowRef::Made(row) => List::Held(row.cells.as_slice()),
            RowRef::Old(row) => List::Page(row.cells()),
        }
    }

    pub fn parts(self) -> List<'r, Name> {
        match self {
            RowRef::Made(row) => List::Held(row.parts.as_slice()),
            RowRef::Old(row) => List::Page(row.parts()),
        }
    }

    pub fn readers(self) -> List<'r, Name> {
        match self {
            RowRef::Made(row) => List::Held(row.readers.as_slice()),
            RowRef::Old(row) => List::Page(row.readers()),
        }
    }

    pub fn cell_readers(self) -> List<'r, CellReader> {
        match self {
            RowRef::Made(row) => List::Held(row.cell_readers.as_slice()),
            RowRef::Old(row) => List::Page(row.cell_readers()),
        }
    }
}

/// A page the request holds: its bytes, checked once by [`PageView::open`], counted so a walk
/// over one row's readers keeps the page while it changes the request (`dirty_readers`).
#[derive(Clone)]
pub struct HeldPage<'a> {
    bytes: Option<ARc<'a, [u8]>>,
    shape: Shape,
}

impl<'a> HeldPage<'a> {
    /// A page with no row (its key the zero key, or lost by the store).
    pub fn empty() -> HeldPage<'a> {
        HeldPage { bytes: None, shape: Shape::EMPTY }
    }

    /// `bytes` checked, then held in `alloc`: one copy of the page, never a decode.
    pub fn open_in(bytes: &[u8], alloc: Alloc<'a>) -> Result<HeldPage<'a>, PageError> {
        let shape = PageView::open(bytes)?.shape;
        Ok(HeldPage { bytes: Some(Rc::from_slice_in(bytes, alloc)), shape })
    }

    /// Its bytes: 0 for the empty page.
    pub fn byte_len(&self) -> usize {
        self.bytes.as_ref().map_or(0, |bytes| bytes.len())
    }

    /// The page in place, with no check again: these bytes are the ones `open_in` checked.
    pub fn view(&self) -> PageView<'_> {
        match &self.bytes {
            Some(bytes) => PageView { bytes: &bytes[..], shape: self.shape },
            None => PageView::empty(),
        }
    }
}

/// A row of a page being written: its base (a row made this run, an old row in place, or none:
/// a watch row of `name`) and this command's reader updates, merged into it as it is written.
pub(crate) struct PageRow<'r, 'x> {
    pub(crate) name: Name,
    pub(crate) base: Option<RowRef<'r, 'x>>,
    /// (target, reader, add), sorted; every target is `name`.
    pub(crate) readers: &'r [(Name, Name, bool)],
    /// (target, entry, add), sorted; every target is `name`.
    pub(crate) cell_readers: &'r [(Name, CellReader, bool)],
}

impl<'r> PageRow<'r, '_> {
    fn each_reader(&self, each: impl FnMut(Name)) {
        let held = self.base.map_or(List::Held(&[]), |base| base.readers());
        merge_each(held, self.readers.iter().map(|(_, reader, add)| (*reader, *add)), each);
    }

    fn each_cell_reader(&self, each: impl FnMut(CellReader)) {
        let held = self.base.map_or(List::Held(&[]), |base| base.cell_readers());
        merge_each(held, self.cell_readers.iter().map(|(_, entry, add)| (*entry, *add)), each);
    }
}

/// `held` (sorted, no repeats) with each sorted update applied, handed to `each` in order: an add
/// keeps an entry once, a removal of an entry not held is nothing. Two sorted sequences walked
/// once; nothing is collected.
fn merge_each<E: Entry>(held: List<'_, E>, updates: impl Iterator<Item = (E, bool)>, mut each: impl FnMut(E)) {
    let mut i = 0;
    let mut last: Option<E> = None;
    for (entry, add) in updates {
        while i < held.len() && held.get(i) < entry {
            let kept = held.get(i);
            each(kept);
            last = Some(kept);
            i += 1;
        }
        if i < held.len() && held.get(i) == entry {
            i += 1;
        }
        if add && last != Some(entry) {
            each(entry);
            last = Some(entry);
        }
    }
    while i < held.len() {
        each(held.get(i));
        i += 1;
    }
}

/// Appends `list`'s wire bytes to `page`: copies the raw slice directly when the list is already
/// in page format, otherwise encodes each entry.
fn write_list<E: Entry>(list: List<'_, E>, page: &mut AVec<'_, u8>) {
    match list {
        List::Page(entries) => page.extend_from_slice(entries.raw()),
        List::Held(held) => {
            for entry in held {
                entry.write(page);
            }
        }
    }
}

/// The page of `rows` (sorted by name, one a name), its readers merged with their updates in a
/// single pass into scratch buffers in `alloc`; `None` when no row is kept (an empty page is
/// stored as the zero key). Sized exactly before the first byte of the page is written.
pub(crate) fn write_page<'b>(rows: &[PageRow<'_, '_>], alloc: Alloc<'b>) -> Option<AVec<'b, u8>> {
    struct KeptRow {
        index: usize,
        counts: [usize; LISTS],
        readers_start: usize,
        cell_readers_start: usize,
    }

    let mut kept_rows: AVec<'b, KeptRow> = Vec::with_capacity_in(rows.len(), alloc.clone());
    let mut merged_readers: AVec<'b, Name> = Vec::new_in(alloc.clone());
    let mut merged_cell_readers: AVec<'b, CellReader> = Vec::new_in(alloc.clone());
    let mut totals = [0usize; LISTS];

    for (index, row) in rows.iter().enumerate() {
        let mut counts = [0usize; LISTS];
        if let Some(base) = row.base {
            counts[READS] = base.reads().len();
            counts[CELLS] = base.cells().len();
            counts[PARTS] = base.parts().len();
            counts[CONDS] = usize::from(base.cond().is_some());
        }

        let readers_start = merged_readers.len();
        if row.readers.is_empty() {
            counts[READERS] = row.base.map_or(0, |base| base.readers().len());
        } else {
            row.each_reader(|entry| merged_readers.push(entry));
            counts[READERS] = merged_readers.len() - readers_start;
        }

        let cell_readers_start = merged_cell_readers.len();
        if row.cell_readers.is_empty() {
            counts[CELL_READERS] = row.base.map_or(0, |base| base.cell_readers().len());
        } else {
            row.each_cell_reader(|entry| merged_cell_readers.push(entry));
            counts[CELL_READERS] = merged_cell_readers.len() - cell_readers_start;
        }

        let is_kept = row.base.is_some_and(|base| !base.watching()) || counts[READERS] > 0 || counts[CELL_READERS] > 0;
        if !is_kept {
            continue;
        }
        for (total, count) in totals.iter_mut().zip(counts) {
            *total += count;
        }
        kept_rows.push(KeptRow { index, counts, readers_start, cell_readers_start });
    }

    let kept = kept_rows.len();
    if kept == 0 {
        return None;
    }
    let size = HEAD + kept * ROW_BYTES + totals.iter().zip(WIDTHS).map(|(total, width)| total * width).sum::<usize>();
    let mut page = Vec::with_capacity_in(size, alloc);
    page.extend_from_slice(PAGE_MAGIC);
    put32(&mut page, kept);
    for total in totals {
        put32(&mut page, total);
    }

    let mut starts = [0usize; LISTS];
    for meta in kept_rows.iter() {
        let row = &rows[meta.index];
        let (key, at, source, out, home, place, split) = match row.base {
            Some(base) => (base.key(), base.at(), base.source(), base.out(), base.home(), base.place(), base.split()),
            None => (Key::default(), Key::default(), ABSENT, ABSENT, row.name, 0, None),
        };
        put_name(&mut page, row.name);
        put_id(&mut page, &key.0);
        put_id(&mut page, &at.0);
        put_id(&mut page, &source);
        put_id(&mut page, &out);
        put_name(&mut page, home);
        page.extend_from_slice(&place.to_le_bytes());
        page.extend_from_slice(&split.map_or(NO_SPLIT, u32::from).to_le_bytes());
        for (start, count) in starts.iter_mut().zip(meta.counts) {
            put32(&mut page, *start);
            put32(&mut page, count);
            *start += count;
        }
    }
    for meta in kept_rows.iter() {
        if let Some(base) = rows[meta.index].base {
            write_list(base.reads(), &mut page);
        }
    }
    for meta in kept_rows.iter() {
        if let Some(base) = rows[meta.index].base {
            write_list(base.cells(), &mut page);
        }
    }
    for meta in kept_rows.iter() {
        if let Some(base) = rows[meta.index].base {
            write_list(base.parts(), &mut page);
        }
    }
    for meta in kept_rows.iter() {
        let row = &rows[meta.index];
        if row.readers.is_empty() {
            if let Some(base) = row.base {
                write_list(base.readers(), &mut page);
            }
        } else {
            let end = meta.readers_start + meta.counts[READERS];
            for entry in &merged_readers[meta.readers_start..end] {
                entry.write(&mut page);
            }
        }
    }
    for meta in kept_rows.iter() {
        let row = &rows[meta.index];
        if row.cell_readers.is_empty() {
            if let Some(base) = row.base {
                write_list(base.cell_readers(), &mut page);
            }
        } else {
            let end = meta.cell_readers_start + meta.counts[CELL_READERS];
            for entry in &merged_cell_readers[meta.cell_readers_start..end] {
                entry.write(&mut page);
            }
        }
    }
    for meta in kept_rows.iter() {
        match rows[meta.index].base {
            Some(RowRef::Old(view)) => page.extend_from_slice(view.list::<(Name, Condition)>(CONDS).raw()),
            Some(RowRef::Made(row)) => {
                if let Some(cond) = &row.cond {
                    cond.write(&mut page);
                }
            }
            None => {}
        }
    }
    debug_assert_eq!(page.len(), size, "the page's size counted before it was written");
    Some(page)
}

/// The table's root: how it is paged and each page's key.
#[derive(Clone, Debug)]
pub struct Root<'a> {
    pub bits: u8,
    pub rows: u64,
    /// The pages' bytes in all: with `rows`, the mean row the next answer's paging is sized by.
    pub bytes: u64,
    /// One per page; [`Key::default`] for a page with no row.
    pub pages: AVec<'a, Key>,
}

pub(crate) fn write_root<'b>(root: &Root<'_>, alloc: Alloc<'b>) -> AVec<'b, u8> {
    let mut w = Writer::new_in(ROOT_MAGIC, alloc);
    w.u8(root.bits);
    w.u32(u32::try_from(root.rows).unwrap_or(u32::MAX));
    w.u32(root.bytes as u32);
    w.u32((root.bytes >> 32) as u32);
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
    let bytes = u64::from(r.u32()?) | u64::from(r.u32()?) << 32;
    let n = r.count(32)?;
    if bits > MOST_BITS || n != 1 << bits {
        return Err(r.error(crate::wire::WireWhy::Count(u32::try_from(n).unwrap_or(u32::MAX))));
    }
    let mut pages = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        pages.push(r.key()?);
    }
    r.done()?;
    Ok(Root { bits, rows, bytes, pages })
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

#[cfg(test)]
mod tests {
    use std::vec::Vec as StdVec;

    use blaze_id::ContentId;
    use blaze_own::testing::Counting;
    use blaze_own::{AVec, Alloc, Erased, Vec};

    use super::{HEAD, HeldPage, O_LISTS, PageError, PageRow, PageView, READERS, ROW_BYTES, Row, RowRef, RowView, write_page};
    use crate::cells::{Cell, CellRead, CellReader};
    use crate::condition::Condition;
    use crate::key::{Key, Kind, Name};
    use crate::wire::NAME_BYTES;

    fn name(kind: Kind, n: u8) -> Name {
        Name::new(kind, ContentId([n; 32]))
    }

    fn id(n: u8) -> ContentId {
        ContentId([n; 32])
    }

    fn list<'a, T: Copy>(items: impl Iterator<Item = T>, alloc: Alloc<'a>) -> AVec<'a, T> {
        let mut list = Vec::new_in(alloc);
        list.extend(items);
        list
    }

    /// Twelve rows over three kinds, sorted by name: lists of each length up to three, a fork
    /// (row 4), a split (row 5) and a watch row (row 11, three readers and no record).
    fn rows<'a>(alloc: Alloc<'a>) -> StdVec<(Name, Row<'a>)> {
        let mut rows = StdVec::new();
        for i in 0..12u8 {
            let kind = [Kind::FileItems, Kind::Signature, Kind::BodyCheck][usize::from(i % 3)];
            let me = name(kind, 10 + i);
            let n = i % 4;
            let watch = i == 11;
            let fork = Condition::ANY.with(3, 1).ok().flatten().expect("one atom is a condition");
            rows.push((
                me,
                Row {
                    key: if watch { Key::default() } else { Key(id(200 + i)) },
                    at: if watch { Key::default() } else { Key(id(220 + i)) },
                    source: id(i),
                    out: id(40 + i),
                    home: name(Kind::FileItems, 90 + i),
                    place: u32::from(i) * 7,
                    reads: list((0..n).filter(|_| !watch).map(|k| (name(Kind::FileItems, 30 + k), id(k))), alloc.clone()),
                    cells: list((0..n).filter(|_| !watch).map(|k| CellRead { name: name(Kind::Signature, 50 + k), cell: Cell([k; 16]), saw: id(k) }), alloc.clone()),
                    parts: list((0..n).filter(|_| !watch).map(|k| name(Kind::BodyCheck, 70 + k)), alloc.clone()),
                    readers: list((0..n).map(|k| name(Kind::BodyCheck, 100 + k)), alloc.clone()),
                    cell_readers: list((0..n).map(|k| CellReader { reader: name(Kind::BodyCheck, 100 + k), cell: Cell([k; 16]), saw: id(k) }), alloc.clone()),
                    split: (i == 5).then_some(2),
                    cond: (i == 4).then_some((name(kind, 3), fork)),
                },
            ));
        }
        rows.sort_by_key(|(name, _)| *name);
        rows
    }

    fn page<'b>(rows: &[(Name, Row<'_>)], alloc: Alloc<'b>) -> AVec<'b, u8> {
        let page_rows: StdVec<PageRow<'_, '_>> =
            rows.iter().map(|(name, row)| PageRow { name: *name, base: Some(RowRef::Made(row)), readers: &[], cell_readers: &[] }).collect();
        write_page(&page_rows, alloc).expect("twelve rows are kept")
    }

    /// Every field and list of `row` as `view` reads it in place.
    fn same(row: &Row<'_>, view: RowView<'_>) -> bool {
        row.key == view.key()
            && row.at == view.at()
            && row.source == view.source()
            && row.out == view.out()
            && row.home == view.home()
            && row.place == view.place()
            && row.split == view.split()
            && row.cond == view.cond()
            && view.reads().iter().eq(row.reads.iter().copied())
            && view.cells().iter().eq(row.cells.iter().copied())
            && view.parts().iter().eq(row.parts.iter().copied())
            && view.readers().iter().eq(row.readers.iter().copied())
            && view.cell_readers().iter().eq(row.cell_readers.iter().copied())
    }

    /// Expected: "test answers::tests::a_page_round_trips_every_row_and_list ... ok".
    #[test]
    fn a_page_round_trips_every_row_and_list() {
        let g = blaze_own::testing::heap().alloc();
        let rows = rows(g.clone());
        let bytes = page(&rows, g.clone());
        let view = PageView::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(view.len(), rows.len());
        for (i, (name, row)) in rows.iter().enumerate() {
            let found = view.row(*name).unwrap_or_else(|| panic!("row {i} is not found by name"));
            assert_eq!(found.name(), *name, "row {i}'s name");
            assert!(same(row, found), "row {i} read in place differs from the row written");
            assert!(same(row, view.row_at(i)), "row {i} by position differs");
        }
        assert!(view.row(name(Kind::Units, 1)).is_none(), "a name with no row is found");
        // Written again from its own rows in place, the page is the same bytes (rule 25).
        let again: StdVec<PageRow<'_, '_>> =
            view.rows().map(|row| PageRow { name: row.name(), base: Some(RowRef::Old(row)), readers: &[], cell_readers: &[] }).collect();
        assert_eq!(write_page(&again, g.clone()).expect("rows are kept").as_slice(), bytes.as_slice(), "a page written from its view moved");
        // And a row copied out of the page is the row written.
        for (name, row) in rows.iter() {
            let copied = view.row(*name).expect("found above").to_row(g.clone());
            assert!(same(&copied, view.row(*name).expect("found above")) && same(row, view.row(*name).expect("found above")));
        }
    }

    /// Expected: "test answers::tests::reader_updates_merge_into_the_written_page ... ok".
    #[test]
    fn reader_updates_merge_into_the_written_page() {
        let g = blaze_own::testing::heap().alloc();
        let rows = rows(g.clone());
        let bytes = page(&rows, g.clone());
        let old = PageView::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
        // A record row with three readers: one removed, one added before them all, one added
        // that it holds (kept once). The watch row loses its three readers and its cell readers:
        // it is not written. A name with no row gains a reader: a watch row of it is written.
        let (target, held) = rows.iter().find(|(_, row)| row.readers.len() == 3 && !row.watching()).map(|(n, row)| (*n, row)).expect("a row with three readers");
        let (watch, watched) = rows.iter().find(|(_, row)| row.watching()).map(|(n, row)| (*n, row)).expect("the watch row");
        let fresh = name(Kind::Signature, 250);
        let mut updates = StdVec::from([(target, held.readers[1], false), (target, name(Kind::BodyCheck, 99), true), (target, held.readers[2], true), (fresh, name(Kind::BodyCheck, 98), true)]);
        updates.extend(watched.readers.iter().map(|reader| (watch, *reader, false)));
        updates.sort();
        let cell_updates: StdVec<(Name, CellReader, bool)> = watched.cell_readers.iter().map(|entry| (watch, *entry, false)).collect();
        let group = |of: Name| {
            let start = updates.partition_point(|(t, _, _)| *t < of);
            let end = updates.partition_point(|(t, _, _)| *t <= of);
            &updates[start..end]
        };
        let mut page_rows: StdVec<PageRow<'_, '_>> = old
            .rows()
            .map(|row| PageRow {
                name: row.name(),
                base: Some(RowRef::Old(row)),
                readers: group(row.name()),
                cell_readers: if row.name() == watch { &cell_updates[..] } else { &[] },
            })
            .collect();
        page_rows.push(PageRow { name: fresh, base: None, readers: group(fresh), cell_readers: &[] });
        page_rows.sort_by_key(|row| row.name);
        let bytes = write_page(&page_rows, g.clone()).expect("rows are kept");
        let new = PageView::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
        let readers: StdVec<Name> = new.row(target).expect("the target is written").readers().iter().collect();
        assert_eq!(readers, [name(Kind::BodyCheck, 99), held.readers[0], held.readers[2]], "the target's readers after the merge");
        assert!(new.row(watch).is_none(), "a watch row no one reads was written");
        let fresh_row = new.row(fresh).expect("the new watch row is written");
        assert!(fresh_row.watching() && fresh_row.home() == fresh && fresh_row.readers().iter().eq([name(Kind::BodyCheck, 98)]));
        assert_eq!(new.len(), rows.len(), "one row went and one came");
    }

    /// Expected: "test answers::tests::open_refuses_a_short_page_an_unsorted_table_and_a_list_past_the_end ... ok".
    #[test]
    fn open_refuses_a_short_page_an_unsorted_table_and_a_list_past_the_end() {
        let g = blaze_own::testing::heap().alloc();
        let rows = rows(g.clone());
        let bytes = page(&rows, g.clone());
        assert!(matches!(PageView::open(&bytes[..HEAD - 1]), Err(PageError::Short { .. })), "a page shorter than its header");
        assert!(matches!(PageView::open(&bytes[..bytes.len() - 1]), Err(PageError::Short { .. })), "a page shorter than its counts");
        let mut magic = bytes.as_slice().to_vec();
        magic[3] = b'3';
        assert_eq!(PageView::open(&magic).err(), Some(PageError::Magic), "a page of the old form");
        // Row 1 named as row 0: not after it.
        let mut unsorted = bytes.as_slice().to_vec();
        unsorted.copy_within(HEAD..HEAD + NAME_BYTES, HEAD + ROW_BYTES);
        assert_eq!(PageView::open(&unsorted).err(), Some(PageError::Unsorted { row: 1 }));
        // The last row's readers counted past their tail.
        let last = rows.len() - 1;
        let count = HEAD + last * ROW_BYTES + O_LISTS + 8 * READERS + 4;
        let mut past = bytes.as_slice().to_vec();
        past[count..count + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(PageView::open(&past).err(), Some(PageError::PastEnd { row: last as u32, list: "readers" }));
    }

    /// Opening a page and reading rows' readers takes no allocator and allocates nothing (the
    /// crate links none outside its tests); the control shows the counter counts.
    /// Expected: "test answers::tests::opening_a_page_and_reading_ten_rows_readers_allocates_nothing ... ok".
    #[test]
    fn opening_a_page_and_reading_ten_rows_readers_allocates_nothing() {
        let g = blaze_own::testing::heap().alloc();
        let rows = rows(g.clone());
        let bytes = page(&rows, g.clone());
        let counting = Erased::new(Counting::new(g));
        let c = counting.handle();
        let before = counting.inner().allocations();
        let view = PageView::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
        let mut seen = 0;
        for (name, _) in rows.iter().take(10) {
            let row = view.row(*name).expect("written");
            seen += row.readers().iter().filter(|reader| reader.kind == Kind::BodyCheck).count();
            seen += row.cell_readers().iter().count();
        }
        assert_eq!(counting.inner().allocations(), before, "opening a page and reading ten rows' readers allocated");
        assert!(seen > 0, "no reader was read");
        // Control: holding a page copies it once, in the counted allocator.
        let held = HeldPage::open_in(&bytes, c).unwrap_or_else(|e| panic!("{e}"));
        assert!(counting.inner().allocations() > before, "control: holding a page allocated nothing, so the counter is not the page's");
        assert_eq!(held.view().len(), rows.len());
    }

    /// Pages are sized by bytes: tempo's table of 2026-11-17 (65,536 rows, 2.6 GB of pages),
    /// paged by bytes, has pages of at most `PAGE_BYTES` on average (65,536 pages of ~40 KB);
    /// paged as before by 512 rows it had 128 pages of ~20 MB, of which a one-statement edit
    /// read all (the control). A table near a boundary keeps its paging (`bits_kept`).
    /// Expected: "test answers::tests::pages_are_sized_by_bytes ... ok".
    #[test]
    fn pages_are_sized_by_bytes() {
        use super::{MOST_BITS, PAGE_BYTES, bits_for, bits_kept};
        assert_eq!((bits_for(0), bits_for(PAGE_BYTES), bits_for(PAGE_BYTES + 1)), (0, 0, 1));
        let tempo: u64 = 2_600_000_000;
        let bits = bits_for(tempo);
        assert_eq!(bits, MOST_BITS);
        assert!(tempo >> bits <= PAGE_BYTES, "tempo's pages average {} bytes", tempo >> bits);
        // Control: the old rule (fewest bits keeping 512 rows a page) on its 65,536 rows.
        let mut by_rows = 0u8;
        while 65_536u64 > 512u64 << by_rows {
            by_rows += 1;
        }
        assert!(tempo >> by_rows > 100 * PAGE_BYTES, "control: by rows, a page averages {} bytes", tempo >> by_rows);
        // Kept within two of what the size asks, else paged again.
        assert_eq!(bits_kept(PAGE_BYTES << 4, Some(5)), 5);
        assert_eq!(bits_kept(PAGE_BYTES << 4, Some(6)), 6);
        assert_eq!(bits_kept(PAGE_BYTES << 4, Some(7)), 4);
        assert_eq!(bits_kept(PAGE_BYTES << 4, Some(3)), 4);
        assert_eq!(bits_kept(PAGE_BYTES << 4, None), 4);
    }

    /// A root keeps its page bytes past 4 GiB through its bytes.
    /// Expected: "test answers::tests::a_root_round_trips_its_bytes ... ok".
    #[test]
    fn a_root_round_trips_its_bytes() {
        let g = blaze_own::testing::heap().alloc();
        let mut pages = Vec::new_in(g.clone());
        pages.extend([Key(id(1)), Key::default()]);
        let root = super::Root { bits: 1, rows: 7, bytes: (5u64 << 32) + 9, pages };
        let bytes = super::write_root(&root, g.clone());
        let back = super::read_root(&bytes, g).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((back.bits, back.rows, back.bytes, back.pages.as_slice()), (1, 7, (5u64 << 32) + 9, root.pages.as_slice()));
    }

    /// Interpolation search finds every row (and refuses absent names) across uniform BLAKE3
    /// digests, multiple kinds, and skewed prefixes, while averaging far fewer probes than binary
    /// search on large uniform pages.
    #[test]
    fn interpolation_search_matches_binary_search_and_beats_log_n() {
        let g = blaze_own::testing::heap().alloc();
        fn make_row<'a>(i: u32, alloc: Alloc<'a>) -> Row<'a> {
            Row {
                key: Key(ContentId::of("interp-key", &i.to_le_bytes())),
                at: Key(ContentId::of("interp-at", &i.to_le_bytes())),
                source: ContentId::of("interp-src", &i.to_le_bytes()),
                out: ContentId::of("interp-out", &i.to_le_bytes()),
                home: name(Kind::FileItems, 1),
                place: i,
                reads: Vec::new_in(alloc.clone()),
                cells: Vec::new_in(alloc.clone()),
                parts: Vec::new_in(alloc.clone()),
                readers: Vec::new_in(alloc.clone()),
                cell_readers: Vec::new_in(alloc),
                split: None,
                cond: None,
            }
        }

        // 1. Large homogeneous page of 2048 uniform BLAKE3 subjects.
        let n = 2048u32;
        let mut uniform: StdVec<(Name, Row<'_>)> = (0..n)
            .map(|i| (Name::new(Kind::BodyCheck, ContentId::of("interp-subject", &i.to_le_bytes())), make_row(i, g.clone())))
            .collect();
        uniform.sort_by_key(|(name, _)| *name);
        let bytes = page(&uniform, g.clone());
        let view = PageView::open(&bytes).unwrap_or_else(|e| panic!("{e}"));

        let mut total_probes = 0usize;
        for (idx, (nm, _)) in uniform.iter().enumerate() {
            let mut target = [0u8; NAME_BYTES];
            target[0] = nm.kind as u8;
            target[1..].copy_from_slice(&nm.subject.0);
            let mut probes = 0usize;
            assert_eq!(view.find_row_counted(&target, Some(&mut probes)), Some(idx));
            total_probes += probes;
            assert_eq!(view.row(*nm).expect("found").name(), *nm);
        }
        // Binary search on 2048 items averages ~10.0 probes; interpolation should average < 6.5.
        let avg_probes = total_probes as f64 / f64::from(n);
        assert!(avg_probes < 6.5, "interpolation search averaged {avg_probes:.2} probes on {n} uniform BLAKE3 rows");

        // Absent probes (below min, above max, and interior misses).
        assert!(view.row(Name::new(Kind::Units, ContentId([0; 32]))).is_none());
        assert!(view.row(Name::new(Kind::Lint, ContentId([0xff; 32]))).is_none());
        for miss in 0..256u32 {
            let nm = Name::new(Kind::BodyCheck, ContentId::of("interp-miss", &miss.to_le_bytes()));
            assert!(view.row(nm).is_none(), "absent uniform name {miss} was found");
        }

        // 2. Multi-kind + adversarial skewed prefixes (same leading 64 bits, differing later bytes,
        //    plus exponential gaps): must stay exact and bounded by O(log N).
        let mut skewed: StdVec<(Name, Row<'_>)> = StdVec::new();
        for i in 0..128u32 {
            let mut sub = [0u8; 32];
            if i < 64 {
                // Clustered identical 64-bit prefix, differing only in byte 31.
                sub[0..8].copy_from_slice(&0x1000_0000_0000_0000u64.to_be_bytes());
                sub[31] = i as u8;
            } else {
                let shift = (i - 64).min(63);
                sub[0..8].copy_from_slice(&(1u64 << shift).to_be_bytes());
                sub[31] = i as u8;
            }
            let kind = if i % 5 == 0 { Kind::Signature } else { Kind::BodyCheck };
            skewed.push((Name::new(kind, ContentId(sub)), make_row(i, g.clone())));
        }
        skewed.sort_by_key(|(name, _)| *name);
        skewed.dedup_by_key(|(name, _)| *name);
        let bytes = page(&skewed, g.clone());
        let view = PageView::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
        for (idx, (nm, _)) in skewed.iter().enumerate() {
            let mut target = [0u8; NAME_BYTES];
            target[0] = nm.kind as u8;
            target[1..].copy_from_slice(&nm.subject.0);
            let mut probes = 0usize;
            assert_eq!(view.find_row_counted(&target, Some(&mut probes)), Some(idx));
            assert!(probes <= 20, "skewed lookup took {probes} probes");
        }
    }

    /// Raw wire byte ordering equals Rust `Ord` for every `RAW_ORD = true` entry type.
    #[test]
    fn raw_ord_matches_struct_ord_for_all_entries() {
        use super::Entry;
        let g = blaze_own::testing::heap().alloc();
        let encode = |write: &dyn Fn(&mut AVec<'_, u8>)| {
            let mut buf = Vec::new_in(g.clone());
            write(&mut buf);
            buf
        };
        let names = [
            name(Kind::Units, 0),
            name(Kind::Units, 255),
            name(Kind::FileItems, 1),
            name(Kind::BodyCheck, 2),
            name(Kind::Lint, 3),
        ];
        for &a in &names {
            for &b in &names {
                assert_eq!(a.cmp(&b), encode(&|p| a.write(p)).as_slice().cmp(encode(&|p| b.write(p)).as_slice()));
                let pa = (a, id(1));
                let pb = (b, id(2));
                assert_eq!(pa.cmp(&pb), encode(&|p| pa.write(p)).as_slice().cmp(encode(&|p| pb.write(p)).as_slice()));
                let ca = CellRead { name: a, cell: Cell([1; 16]), saw: id(3) };
                let cb = CellRead { name: b, cell: Cell([2; 16]), saw: id(4) };
                assert_eq!(ca.cmp(&cb), encode(&|p| ca.write(p)).as_slice().cmp(encode(&|p| cb.write(p)).as_slice()));
                let ra = CellReader { reader: a, cell: Cell([1; 16]), saw: id(3) };
                let rb = CellReader { reader: b, cell: Cell([2; 16]), saw: id(4) };
                assert_eq!(ra.cmp(&rb), encode(&|p| ra.write(p)).as_slice().cmp(encode(&|p| rb.write(p)).as_slice()));
            }
        }
    }
}
