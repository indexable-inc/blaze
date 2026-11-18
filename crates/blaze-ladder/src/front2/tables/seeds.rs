//! A BODY'S HEADS FROM FROZEN RECORDS: what a body task's store is seeded with, so a body
//! lowers no head (engine.md "nothing starts over"; lane FJ's `seed`, head.rs).
//!
//! A body's store asks its seed for each head it needs ([`HeadSeed`], heads.rs
//! `Store::ensure_noting`): the need's cell is imported from its frozen record and filed, never
//! lowered. Two seeds:
//! - [`FrozenTable`]: the request's heads, frozen once and then READ ONLY, shared by every
//!   body of the request and every worker (nothing in it changes after it is made). A cell it
//!   does not hold is a typed refusal ([`SeedError::NotFrozen`]), never lowered instead (rule 3).
//! - [`Freezing`]: a [`SeededHeads`] behind a `RefCell`, freezing each cell on its first ask:
//!   one worker's, for making a table (the cost test freezes the fixture's heads this way, once).

use core::cell::RefCell;

use blaze_own::{AVec, Alloc};

use super::super::heads::{Seeded, Store, Want};
use super::super::keys::Keys;
use super::super::source::NamesSource;
use super::head::{FreezeError, FrozenHead, HeadSource, SeededHeads, cell_of, seed, want_bytes};

/// What a body's store asks for each head it needs (heads.rs `Store::seeds`).
pub trait HeadSeed<'n> {
    /// Need `need`'s cell filed into `store` from its frozen record ([`Seeded::Filed`]);
    /// `scratch` holds the import's work.
    fn seed(&self, store: &Store<'n>, keys: &Keys, need: Want, scratch: Alloc<'static>) -> Result<Seeded, SeedError>;
}

/// Why a head could not be seeded.
#[derive(Debug)]
pub enum SeedError {
    /// The cell's record could not be made, read or imported (head.rs).
    Freeze(FreezeError),
    /// The table holds no frozen record of `cell`, the cell of need `need`: the request froze
    /// too few heads (a fault of the plan, never lowered here).
    NotFrozen { need: Want, cell: Want },
}

impl core::fmt::Display for SeedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SeedError::Freeze(e) => write!(f, "{e}"),
            SeedError::NotFrozen { need, cell } => write!(f, "{need:?} needs cell {cell:?}, which the request's frozen heads do not hold (front2 tables/seeds.rs)"),
        }
    }
}

/// How many cells one table holds at most.
pub const MOST_CELLS: usize = 1 << 20;

/// The request's frozen heads, by cell, sorted by the cell's bytes (head.rs `want_bytes`).
///
/// It holds no names: a frozen head is a record keyed by its item, its bytes the same whoever
/// reads them, so one table serves every member group and every worker of the request. What
/// differs per group is only how a body's NEED becomes a cell (head.rs `cell_of` through that
/// group's names): a body reads the table through a [`TableView`] of its own names.
pub struct FrozenTable<'a> {
    cells: AVec<'a, (Want, AVec<'a, u8>)>,
}

/// One group's view of the request's [`FrozenTable`]: its names turn a need into a cell, the
/// table gives the cell's bytes. Copied freely; nothing in it changes.
pub struct TableView<'t, 'a, N> {
    pub names: N,
    pub table: &'t FrozenTable<'a>,
}

/// Why a table could not be made.
#[derive(Debug)]
pub enum TableError {
    /// More than [`MOST_CELLS`] cells.
    Full { cells: usize },
    Freeze(FreezeError),
}

impl<'a> FrozenTable<'a> {
    /// Every cell `freezing` froze, copied into `alloc`.
    pub fn of<'n, N: NamesSource<'n>>(freezing: &Freezing<'n, '_, N>, alloc: Alloc<'a>) -> Result<FrozenTable<'a>, TableError> {
        let mut heads = freezing.heads.borrow_mut();
        let mut wants: AVec<'a, Want> = AVec::new_in(alloc);
        wants.extend(heads.cells());
        if wants.len() > MOST_CELLS {
            return Err(TableError::Full { cells: wants.len() });
        }
        let mut cells: AVec<'a, (Want, AVec<'a, u8>)> = AVec::with_capacity_in(wants.len(), alloc);
        for w in wants.iter().copied() {
            let frozen = heads.frozen(w).map_err(TableError::Freeze)?;
            let mut bytes: AVec<'a, u8> = AVec::with_capacity_in(frozen.bytes().len(), alloc);
            bytes.extend_from_slice(frozen.bytes());
            cells.push((w, bytes));
        }
        cells.sort_unstable_by(|a, b| want_bytes(a.0).cmp(&want_bytes(b.0)));
        Ok(FrozenTable { cells })
    }

    /// This table read through `names`: what a body of that names' group is handed.
    pub fn view<N>(&self, names: N) -> TableView<'_, 'a, N> {
        TableView { names, table: self }
    }

    /// How many cells it holds.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Cell `cell`'s frozen bytes (the `BZFZ` form), if the request froze it.
    pub fn bytes_of(&self, cell: Want) -> Option<&[u8]> {
        let at = self.cells.binary_search_by(|(w, _)| want_bytes(*w).cmp(&want_bytes(cell))).ok()?;
        Some(&self.cells[at].1)
    }
}

/// One cell's record, as [`seed`] reads it: it asks only the cell of the need it was given.
struct OneCell<'t> {
    cell: Want,
    bytes: &'t [u8],
}

impl HeadSource for OneCell<'_> {
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError> {
        if want != self.cell {
            // `seed` asks the cell of the need it is given, found before it is called.
            return Err(FreezeError::NotACell { want, cell: self.cell });
        }
        FrozenHead::read(self.bytes).map_err(|error| FreezeError::Read { want, error })
    }

    fn open(&mut self, _want: Want) -> bool {
        false
    }
}

impl<'n, N: NamesSource<'n>> HeadSeed<'n> for TableView<'_, '_, N> {
    fn seed(&self, store: &Store<'n>, keys: &Keys, need: Want, scratch: Alloc<'static>) -> Result<Seeded, SeedError> {
        let cell = cell_of(self.names, need).map_err(SeedError::Freeze)?;
        let bytes = self.table.bytes_of(cell).ok_or(SeedError::NotFrozen { need, cell })?;
        seed(self.names, None, need, &mut OneCell { cell, bytes }, store, keys, scratch).map_err(SeedError::Freeze)
    }
}

/// A [`SeededHeads`] freezing each cell on its first ask (one worker's: not `Sync`).
pub struct Freezing<'n, 'a, N> {
    names: N,
    pub heads: RefCell<SeededHeads<'n, 'a, N>>,
}

impl<'n, 'a, N: NamesSource<'n>> Freezing<'n, 'a, N> {
    pub fn new(names: N, heads: SeededHeads<'n, 'a, N>) -> Freezing<'n, 'a, N> {
        Freezing { names, heads: RefCell::new(heads) }
    }
}

impl<'n, N: NamesSource<'n>> HeadSeed<'n> for Freezing<'n, '_, N> {
    fn seed(&self, store: &Store<'n>, keys: &Keys, need: Want, scratch: Alloc<'static>) -> Result<Seeded, SeedError> {
        let mut heads = self.heads.borrow_mut();
        seed(self.names, None, need, &mut *heads, store, keys, scratch).map_err(SeedError::Freeze)
    }
}
