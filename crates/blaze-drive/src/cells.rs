//! Part reads (drive.md, "D114 rulings", "Part reads"): a read is (name, part, digest), the part
//! `Whole` or one CELL of the record, a 128-bit key the record's kind gives meaning to. A kind
//! that has cells gives one pure function over its record's payload, [`CellOf`]: a search in the
//! mapped bytes, answering the cell's digest or [`Seen::Absent`]. A reader's entry is valid when
//! today's record gives the same digest for its cell, so forward from a change only a reader
//! whose cell moved is dirty; `Absent` is a value, so a lookup that found nothing is a read.
//!
//! A `Whole` read stays what every caller wrote before: a `(Name, ContentId)` in a read list. A
//! cell read is a [`CellRead`] beside it, in a list of its own sorted by (name, cell), so a
//! record with no cell reads pays one count.

use blaze_id::ContentId;

use crate::key::{ABSENT, KINDS, Kind, Name};
use crate::wire::{Reader, WireError, Writer};

/// One cell of a record: what a reader looked up in it (a module's (namespace, name), an
/// impl bucket's self shape), as a 128-bit key the kind's [`CellOf`] understands.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Cell(pub [u8; 16]);

pub(crate) const CELL_BYTES: usize = 16;

/// What a cell of a record holds today: its digest, or nothing there.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Seen {
    Is(ContentId),
    Absent,
}

impl Seen {
    /// The digest a read list records: [`ABSENT`] for a cell that is not there.
    pub fn digest(self) -> ContentId {
        match self {
            Seen::Is(digest) => digest,
            Seen::Absent => ABSENT,
        }
    }
}

/// A read of one cell of `name`'s record, and what the reader saw there.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CellRead {
    pub name: Name,
    pub cell: Cell,
    /// [`Seen::digest`] of what the cell held.
    pub saw: ContentId,
}

impl CellRead {
    pub fn new(name: Name, cell: Cell, seen: Seen) -> CellRead {
        CellRead { name, cell, saw: seen.digest() }
    }
}

/// The reverse entry: `reader` read `cell` of the row's name and saw `saw`. Kept in the read
/// name's row, so a change finds which readers moved without loading a reader's row.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CellReader {
    pub reader: Name,
    pub cell: Cell,
    pub saw: ContentId,
}

/// A kind's record refused a cell lookup: its payload is not of the form the kind's
/// [`CellOf`] reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellRefused {
    pub what: &'static str,
}

/// A kind's cell lookup: pure over its record's payload (rule 14), a search in the bytes.
pub type CellOf = fn(payload: &[u8], cell: Cell) -> Result<Seen, CellRefused>;

/// Which kinds have cells, and each one's lookup: given once to the command. A cell read of a
/// kind with none is refused by the caller, naming the record and its reader.
#[derive(Clone, Copy, Debug)]
pub struct Cells {
    of: [Option<CellOf>; KINDS],
}

impl Cells {
    /// No kind has cells: every read is `Whole`.
    pub const NONE: Cells = Cells { of: [None; KINDS] };

    pub fn with(mut self, kind: Kind, of: CellOf) -> Cells {
        self.of[kind.index()] = Some(of);
        self
    }

    pub fn of(&self, kind: Kind) -> Option<CellOf> {
        self.of[kind.index()]
    }
}

pub(crate) fn write_cell(w: &mut Writer<'_>, cell: Cell) {
    w.raw(&cell.0);
}

pub(crate) fn read_cell(r: &mut Reader<'_>) -> Result<Cell, WireError> {
    let bytes = r.take_bytes(CELL_BYTES)?;
    let mut cell = [0; CELL_BYTES];
    cell.copy_from_slice(bytes);
    Ok(Cell(cell))
}
