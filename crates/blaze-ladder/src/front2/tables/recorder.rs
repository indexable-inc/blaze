//! The per-task recorder of every read (engine.md, E2): ONE list a task fills while it runs,
//! of the names cells its walks and rows consulted (lane CELL's [`NameCell`], put through
//! [`CellSink`]; a crate's lang rows are one, [`NameCell::Lang`]), the heads it read and the impl buckets it read. The
//! list is sorted and deduplicated ONCE, when the task ends ([`Recorder::finish`], rule 25):
//! what a task read is then a function of its questions, never of the order it asked them.
//!
//! A recorder is the task's own (made in its frame, dropped with it); the tables it records
//! reads of hold no per-task cache, so a read is listed for EVERY task that asks it, not only
//! for the first (front2 reads.rs, "What is not listed": the old world's cause).
//!
//! A read is not given its digest here: the merge decides each cell's digest from the same
//! tables a later run checks it against (names_cells.rs `seen_in`), so the two cannot disagree.

use core::cell::{Cell, RefCell};

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_scope::names_cells::{CellSink, NameCell, Section};

/// The most reads one task keeps before it is refused (rule 18): a body that reads more is
/// [`ReadsFull`], never a list cut short.
pub const MOST_READS: usize = 1 << 20;

/// One read of a task. Ordered (and so sorted) by its derived `Ord`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TableRead {
    /// A cell of a crate's names, as lane CELL defines it (its `CrateScope` record's cell table).
    Names(NameCell),
    /// A head read whole: the item by its checker key's 16 bytes (front2 reads.rs `Fact::key`,
    /// big-endian) and which part (the `FactKind` byte: signature, predicates, ADT, ..).
    Head { item: [u8; 16], part: u8 },
    /// One crate's impl bucket (crate: its key's first 16 bytes, all zero for a fact of the
    /// whole graph; bucket: [`bucket_cell`], or [`UNFILED`], or [`INCOHERENT`]).
    Impls { krate: [u8; 16], bucket: [u8; 16] },
}

/// The bucket cell of a crate's list of impls filed under no bucket.
pub const UNFILED: [u8; 16] = [0xff; 16];
/// The bucket cell of the one fact "which crates may write incoherent impls".
pub const INCOHERENT: [u8; 16] = [0xfe; 16];

const BUCKET_DOMAIN: &str = "blaze-ladder front2 tables bucket cell 1";

/// The cell of an impl bucket by its key's bytes (blaze-scope items_record `BucketKey::bytes`).
pub fn bucket_cell(key: &[u8]) -> [u8; 16] {
    let id = blaze_id::ContentId::of(BUCKET_DOMAIN, key);
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&id.0[..16]);
    cell
}

/// Which record a read is of: what the engine names the read by (its `CellId`'s `Name`), with
/// [`TableRead::cell`] as the cell.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ReadOf {
    /// The `CrateScope` record of the crate whose keys begin with these 8 bytes.
    Names { krate: [u8; 8] },
    /// The head record of the item (its checker key's 16 bytes).
    Head { item: [u8; 16] },
    /// The impl table of the crate (its key's first 16 bytes; zero: the whole graph's fact).
    Impls { krate: [u8; 16] },
}

impl TableRead {
    /// The record the read is of.
    pub fn of(self) -> ReadOf {
        match self {
            TableRead::Names(cell) => ReadOf::Names { krate: cell.crate_prefix() },
            TableRead::Head { item, .. } => ReadOf::Head { item },
            TableRead::Impls { krate, .. } => ReadOf::Impls { krate },
        }
    }

    /// The read's cell in its record ([`TableRead::of`]): a names cell's key, a head's part, an
    /// impl table's bucket.
    pub fn cell(self) -> [u8; 16] {
        match self {
            TableRead::Names(cell) => cell.key(),
            TableRead::Head { part, .. } => {
                let mut cell = [0u8; 16];
                cell[0] = part;
                cell
            }
            TableRead::Impls { bucket, .. } => bucket,
        }
    }
}

/// Where the view puts each read: the recorder, or a test's sink.
pub trait ReadSink {
    fn read(&self, read: TableRead);
}

/// A [`ReadSink`] as the names walk's [`CellSink`]: each cell a [`TableRead::Names`].
pub struct NamesInto<'s>(pub &'s dyn ReadSink);

impl CellSink for NamesInto<'_> {
    fn put(&self, cell: NameCell) {
        self.0.read(TableRead::Names(cell));
    }
}

/// A task's reads were more than [`MOST_READS`] (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadsFull {
    pub most: usize,
}

impl core::fmt::Display for ReadsFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the task read more than {} cells (front2 tables/recorder.rs `MOST_READS`)", self.most)
    }
}

/// What a recorder is made from.
pub struct RecorderOpts<'a> {
    /// The task's frame: the list lives there and leaves with the task's result.
    pub alloc: Alloc<'a>,
}

/// One task's reads (module docs).
pub struct Recorder<'a> {
    reads: RefCell<AVec<'a, TableRead>>,
    full: Cell<bool>,
    /// A test's planted bug: reads of this cell are dropped ([`Recorder::plant_drop`]).
    dropped: Cell<Option<NameCell>>,
}

impl<'a> Recorder<'a> {
    pub fn new(opts: RecorderOpts<'a>) -> Recorder<'a> {
        Recorder { reads: RefCell::new(AVec::new_in(opts.alloc)), full: Cell::new(false), dropped: Cell::new(None) }
    }

    /// PLANTED BUG, for tests only: every read of `cell` is dropped from now on, as a view
    /// that forgot to record one question would leave it out.
    #[doc(hidden)]
    pub fn plant_drop(&self, cell: NameCell) {
        self.dropped.set(Some(cell));
    }

    /// A head read whole ([`TableRead::Head`]).
    pub fn head(&self, item: [u8; 16], part: u8) {
        self.read(TableRead::Head { item, part });
    }

    /// An impl bucket read ([`TableRead::Impls`]).
    pub fn impls(&self, krate: [u8; 16], bucket: [u8; 16]) {
        self.read(TableRead::Impls { krate, bucket });
    }

    /// A crate's lang rows ([`NameCell::Lang`]: a names cell, absent where it declares none).
    pub fn lang(&self, krate: [u8; 8]) {
        self.read(TableRead::Names(NameCell::Lang { krate }));
    }

    /// A section of a crate's names read whole (a question no cell answers).
    pub fn whole(&self, krate: [u8; 8], section: Section) {
        self.read(TableRead::Names(NameCell::Whole { krate, section }));
    }

    /// An items row's cell.
    pub fn item(&self, key: ItemKey) {
        self.read(TableRead::Names(NameCell::Item { key }));
    }

    /// The task's reads, sorted and each once: the one sort of the task (rule 25).
    pub fn finish(self) -> Result<AVec<'a, TableRead>, ReadsFull> {
        if self.full.get() {
            return Err(ReadsFull { most: MOST_READS });
        }
        let mut reads = self.reads.into_inner();
        reads.sort_unstable();
        reads.dedup();
        Ok(reads)
    }
}

impl ReadSink for Recorder<'_> {
    fn read(&self, read: TableRead) {
        if let (TableRead::Names(cell), Some(dropped)) = (read, self.dropped.get())
            && cell == dropped
        {
            return;
        }
        let mut reads = self.reads.borrow_mut();
        if reads.len() >= MOST_READS {
            self.full.set(true);
            return;
        }
        reads.push(read);
    }
}

impl CellSink for Recorder<'_> {
    fn put(&self, cell: NameCell) {
        self.read(TableRead::Names(cell));
    }
}

#[cfg(test)]
mod tests {
    use blaze_id::{ContentId, ItemKey};
    use blaze_scope::names_cells::NameCell;

    use super::{ReadSink, Recorder, RecorderOpts, TableRead};

    fn key(n: u8) -> ItemKey {
        ItemKey(ContentId([n; 32]))
    }

    /// The same reads put in two orders, with repeats, finish as one list: sorted, each once.
    #[test]
    fn reads_finish_sorted_and_once_whatever_the_order() {
        let alloc = blaze_own::testing::heap().alloc();
        let reads = [
            TableRead::Names(NameCell::Item { key: key(3) }),
            TableRead::Head { item: [1; 16], part: 3 },
            TableRead::Names(NameCell::Module { module: key(1) }),
            TableRead::Impls { krate: [2; 16], bucket: [9; 16] },
            TableRead::Names(NameCell::Lang { krate: [4; 8] }),
        ];
        let forward = Recorder::new(RecorderOpts { alloc });
        for r in reads.iter().chain(reads.iter()) {
            forward.read(*r);
        }
        let backward = Recorder::new(RecorderOpts { alloc });
        for r in reads.iter().rev() {
            backward.read(*r);
        }
        let (forward, backward) = (forward.finish().expect("within the cap"), backward.finish().expect("within the cap"));
        assert_eq!(&forward[..], &backward[..]);
        assert_eq!(forward.len(), reads.len(), "each read once");
        assert!(forward.windows(2).all(|w| w[0] < w[1]), "sorted: {:?}", &forward[..]);
    }

    /// Control: the planted drop removes exactly that cell.
    #[test]
    fn a_planted_drop_removes_its_cell() {
        let alloc = blaze_own::testing::heap().alloc();
        let r = Recorder::new(RecorderOpts { alloc });
        r.plant_drop(NameCell::Item { key: key(3) });
        r.item(key(3));
        r.item(key(4));
        let list = r.finish().expect("within the cap");
        assert_eq!(&list[..], &[TableRead::Names(NameCell::Item { key: key(4) })]);
    }
}
