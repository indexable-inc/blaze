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
//! - [`RequestSeed`]: the engine's (blaze-cli engine_seeds.rs). A cell the request knows
//!   ([`RequestHeads`]: the head tasks' table, the dependency heads found in the store, what
//!   another worker froze) is imported from those bytes; any other (a dependency's head on its
//!   first read) is frozen here, on the spot, by one worker's [`SeededHeads`] that takes every
//!   need it knows from the request too, and what it froze is published to every other worker.
//!   A freeze is a function of its cell and the names (head.rs `freeze_seeded`), so a body's
//!   result never depends on which worker froze a head first: two workers that freeze one cell
//!   at once make equal bytes, one is kept, and other bytes are a typed refusal.
//!
//! AN ITEM DECLARED IN A BODY is never any seed's ([`of_the_body`], lane RU4): its key is its
//! enclosing body's lowering's (front2 locals.rs: `ItemKey::INNER` set, derived from that body's
//! key), its head's text is a range of that body's text or a copy of an expansion inside it, and
//! only the store that lowered the body holds it (`Store::locals`). No head task, dependency
//! record or other worker's freeze holds it, and a freeze in another store cannot read it: the
//! names records hold no row for it, so `Store::step` met `resolver.detail(key)` as `None` and
//! raised `HeadError::NoText` (heads.rs, the "names library holds no text of an item it needs"
//! of every clap and serde body whose derive writes items inside a fn: serde's `fn deserialize`
//! declares `struct __Visitor` and `impl Visitor for __Visitor`). Each seed answers
//! [`Seeded::Lower`] for it, so the body's own store lowers it from its own rows, as a store with
//! no seed always did.

use core::cell::{Cell, RefCell};

use blaze_own::{AVec, Alloc};

use super::super::heads::{Seeded, Store, Want};
use super::super::keys::Keys;
use super::super::source::NamesSource;
use super::head::{CellFailure, FreezeError, FrozenHead, HeadSource, Known, KnownHeads, NotTheirs, SeededHeads, cell_of, file_frozen, seed, want_bytes};

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
    /// Two freezes of cell `cell` in one request made other bytes: a freeze that is not a
    /// function of its cell and the names (rule 21), never one kept quietly.
    Unequal { cell: Want },
    /// The request's heads frozen now hold `most` cells: cell `cell` is not taken (rule 18).
    Full { cell: Want, most: usize },
    /// Member cell `cell`'s head was frozen by its head task under other units than the
    /// reader's only (blaze-cli engine_heads.rs `head_cell`: one cell a unit): another unit's
    /// bytes name that unit's keys, so none is taken. `said`: the head by path, the units that
    /// froze it and the reader's (rule 19).
    NotInItsUnit { cell: Want, said: &'static str },
    /// Member cell `cell`'s head has no cell of its head task this request for the reader's
    /// unit (the task failed, was not planned, holds no such cell, or no task covers it:
    /// `said` names which): a body never freezes a member's head, so it is refused (rule 3).
    NoMemberCell { cell: Want, said: &'static str },
}

impl SeedError {
    /// A freeze's error as the seed's: a cell refused as not the asker's to freeze keeps its
    /// typed reason ([`NotTheirs`]): the engine's own lack as [`SeedError::NotInItsUnit`] or
    /// [`SeedError::NoMemberCell`] (the body's failure), a head its task could not freeze as the
    /// freeze's error (the project's head: the body not checked, as when it froze the head
    /// itself and failed). Every other is the freeze's.
    pub fn of_freeze(error: FreezeError) -> SeedError {
        match error {
            FreezeError::NotTheirs { want, why: NotTheirs::OtherUnits { said } } => SeedError::NotInItsUnit { cell: want, said },
            FreezeError::NotTheirs { want, why: NotTheirs::NoCell { said } } => SeedError::NoMemberCell { cell: want, said },
            // `NotTheirs::Failed` among them: the project's head, said by its task's reason.
            error => SeedError::Freeze(error),
        }
    }
}

impl core::fmt::Display for SeedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SeedError::Freeze(e) => write!(f, "{e}"),
            SeedError::NotFrozen { need, cell } => write!(f, "{need:?} needs cell {cell:?}, which the request's frozen heads do not hold (front2 tables/seeds.rs)"),
            SeedError::Unequal { cell } => write!(f, "cell {cell:?} was frozen twice in one request to other bytes: its freeze is not a function of its cell (front2 tables/seeds.rs `RequestHeads::publish`)"),
            SeedError::Full { cell, most } => write!(f, "cell {cell:?}: the request's heads frozen now already hold {most} cells (front2 tables/seeds.rs `RequestHeads::publish`)"),
            SeedError::NotInItsUnit { said, .. } | SeedError::NoMemberCell { said, .. } => write!(f, "{said}"),
        }
    }
}

/// THE GATE'S FETCH (D168; typeck.md "The gate fetches, the recorder records"): the head cells
/// of a body's gate set (its last read list's head cells, on an edit) filed into the body's
/// store before the body is entered, together, by `seeds`. One entry for the gate and for the
/// engine's writer.
///
/// A fetch is never a read: nothing here reaches the body's recorder. `seeds` must read its
/// names through a view whose sink is [`FetchReads`] (`Tables::reading_into(&FetchReads)`), so
/// a cell the check then never asks leaves no row in the read list, and one it asks is
/// recorded by the check's own fact (task.rs `fact_read`), as without a fetch. The list a
/// fetched body records is the list it records unfetched (the gate's oracle compares the two).
///
/// A cell already made is skipped; a cell its seed answers [`Seeded::Lower`] (an item of the
/// body, or one its own store makes) is left to the body's store and counted. A seed's refusal
/// is the fetch's, naming the cell (rule 19): never fetched another way (rule 3).
pub fn fetch_cells<'n, 'c>(store: &Store<'n>, keys: &Keys, seeds: &dyn HeadSeed<'n>, cells: &'c [Want], scratch: Alloc<'static>) -> Result<Fetched<'c>, FetchError> {
    let (mut filed, mut left) = (0u32, 0u32);
    for &cell in cells {
        if store.made(cell) {
            continue;
        }
        match seeds.seed(store, keys, cell, scratch) {
            Ok(Seeded::Filed) => filed += 1,
            Ok(Seeded::Lower) => left += 1,
            Err(error) => return Err(FetchError { cell, error }),
        }
    }
    Ok(Fetched { cells, filed, left })
}

/// The proof a gate's cells were fetched (rule 74): made only by [`fetch_cells`], taken by the
/// body's entry, so a body is entered after its fetch, never before.
#[derive(Debug)]
pub struct Fetched<'c> {
    cells: &'c [Want],
    filed: u32,
    left: u32,
}

impl Fetched<'_> {
    /// The cells of the gate set.
    pub fn cells(&self) -> &[Want] {
        self.cells
    }

    /// How many the fetch filed from frozen records.
    pub fn filed(&self) -> u32 {
        self.filed
    }

    /// How many were left to the body's store ([`Seeded::Lower`]).
    pub fn left(&self) -> u32 {
        self.left
    }
}

/// A gate cell whose seed refused (rule 19: the cell and the seed's error).
#[derive(Debug)]
pub struct FetchError {
    pub cell: Want,
    pub error: SeedError,
}

/// The sink a fetch reads names through: it keeps no read, by design ([`fetch_cells`]: a fetch
/// is not a read; what the check asks is recorded by the check).
pub struct FetchReads;

impl super::recorder::ReadSink for FetchReads {
    fn read(&self, _: super::recorder::TableRead) {}

    fn binding(&self, _: blaze_id::ItemKey, _: blaze_surface::Namespace, _: &str) {}
}

/// Whether `need` is of an item declared in a body that `store` lowered (module docs, "AN ITEM
/// DECLARED IN A BODY"): its key says it is inside a body ([`blaze_id::ItemKey::is_inner`]) and
/// the store's locals hold its head's row (front2 locals.rs `Locals::head_of`, an unsupported
/// kind's refusal included: the store's own step raises it, naming the item). The one test every
/// seed takes, before it turns the need into a cell (`cell_of` reads the names records, which
/// hold no row for it).
pub fn of_the_body(store: &Store<'_>, need: Want) -> bool {
    let key = need.key();
    key.is_inner() && store.locals.head_of(key).is_some()
}

/// Where a [`RequestSeed`] makes a body's need of an item declared in that body (rule 23: the
/// plant removes the cause and a test watches the effect come back).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BodyLocals {
    /// In the body's own store ([`of_the_body`]): the rule.
    Lowered,
    /// THE PLANT: asked of the request like any head (the form before lane RU4), which a body's
    /// item has no cell of, so its stretch's freezer freezes it in a store without the body's
    /// rows and fails `HeadError::NoText`.
    AskTheRequest,
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

// SAFETY: the table is read by every worker of a request at once (blaze-cli engine_seeds.rs
// `RequestTable`). What keeps an `Alloc` handle from being `Sync` is that allocating or freeing
// through it from two threads at once races on its header; a `&FrozenTable` reaches the
// handles only through `AVec`'s shared methods (`iter`, `len`, a slice index, `binary_search`:
// this file's `bytes_of`, `iter`, `len`, `view`), none of which allocates or frees. Growing or
// dropping the table takes `&mut` or ownership, so one thread. `Want` and `u8` are plain data.
unsafe impl Sync for FrozenTable<'_> {}

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
    /// A record's bytes do not read as a frozen head.
    Read(super::head::FrozenError),
    /// Two records of one cell hold other bytes (rule 21).
    Twice { cell: Want },
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

    /// The table of frozen heads `each` hands (each the `BZFZ` form, its cell read from its own
    /// header), copied into `alloc`: the request's table from its head tasks' records. A cell
    /// handed twice with equal bytes is kept once; with other bytes it is refused (rule 21).
    pub fn from_frozen<'b>(each: impl Iterator<Item = &'b [u8]>, alloc: Alloc<'a>) -> Result<FrozenTable<'a>, TableError> {
        let mut cells: AVec<'a, (Want, AVec<'a, u8>)> = AVec::new_in(alloc);
        for bytes in each {
            if cells.len() == MOST_CELLS {
                return Err(TableError::Full { cells: MOST_CELLS + 1 });
            }
            let want = FrozenHead::read(bytes).map_err(TableError::Read)?.want();
            let mut copy: AVec<'a, u8> = AVec::with_capacity_in(bytes.len(), alloc);
            copy.extend_from_slice(bytes);
            cells.push((want, copy));
        }
        cells.sort_unstable_by(|a, b| want_bytes(a.0).cmp(&want_bytes(b.0)).then_with(|| a.1[..].cmp(&b.1[..])));
        for two in cells.windows(2) {
            if two[0].0 == two[1].0 && two[0].1[..] != two[1].1[..] {
                return Err(TableError::Twice { cell: two[0].0 });
            }
        }
        cells.dedup_by(|a, b| a.0 == b.0);
        Ok(FrozenTable { cells })
    }

    /// Every cell and its bytes, sorted by the cell's bytes.
    pub fn iter(&self) -> impl Iterator<Item = (Want, &[u8])> + '_ {
        self.cells.iter().map(|(w, bytes)| (*w, &bytes[..]))
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

impl<'n, N: NamesSource<'n>> HeadSeed<'n> for TableView<'_, '_, N> {
    fn seed(&self, store: &Store<'n>, keys: &Keys, need: Want, scratch: Alloc<'static>) -> Result<Seeded, SeedError> {
        if of_the_body(store, need) {
            return Ok(Seeded::Lower);
        }
        let cell = cell_of(self.names, need).map_err(SeedError::Freeze)?;
        let bytes = self.table.bytes_of(cell).ok_or(SeedError::NotFrozen { need, cell })?;
        let frozen = FrozenHead::read(bytes).map_err(|error| SeedError::Freeze(FreezeError::Read { want: cell, error }))?;
        file_frozen(self.names, cell, frozen, store, keys, scratch).map_err(SeedError::Freeze)?;
        Ok(Seeded::Filed)
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
        if of_the_body(store, need) {
            return Ok(Seeded::Lower);
        }
        let mut heads = self.heads.borrow_mut();
        seed(self.names, None, need, &mut *heads, store, keys, scratch).map_err(SeedError::Freeze)
    }
}

/// The request's frozen heads as one worker's stretch sees them ([`RequestSeed`]): what it
/// knows ([`KnownHeads::known`]: read only, or inserted once with equal bytes), and where a
/// worker publishes a head it froze. Not `Sync` itself: blaze-cli's is a stretch's view
/// (engine_seeds.rs `StretchHeads`: its names place a want), over the one shared `Sync` table.
pub trait RequestHeads: KnownHeads {
    /// `bytes`, frozen now for `cell` by one worker, handed to every other. A cell published
    /// before keeps the first bytes; equal bytes again are counted, other bytes refused
    /// ([`SeedError::Unequal`]); past the table's cap, [`SeedError::Full`]. `krate`: the crate
    /// the freezing worker's names hold the cell's item in (`NamesSource::crate_of`, then
    /// `crate_key`), `None` when they hold it in none: what decides whether the head is a
    /// dependency's, kept beyond the request, by the names' own answer, not by a key's bytes.
    fn publish(&self, cell: Want, krate: Option<blaze_id::CrateItemKey>, bytes: &[u8]) -> Result<(), SeedError>;

    /// Cell `cell`'s first freeze failed with `error`, on one worker: handed to the request so
    /// every later ask of it (here [`FreezeError::Again`], which carries no reason; another
    /// stretch's) quotes its own reason, whichever caller met the failure first. Handed by
    /// [`RequestSeed::seed`] at the failure itself, so a caller that leaves the failure to a
    /// later fill (front2 check.rs `make`, world_items.rs `store_want`) loses no reason.
    fn refused(&self, cell: Want, error: &FreezeError);
}

/// One stretch's seed on the engine (module docs): `names` is the stretch's records, `heads`
/// the request's, `freezer` this stretch's own freezer (its known heads `heads`). Not `Sync`:
/// one a worker's stretch.
pub struct RequestSeed<'k, 'n, 'a, N, H> {
    names: N,
    heads: &'k H,
    freezer: RefCell<SeededHeads<'n, 'a, N>>,
    /// Asks answered from the request's heads.
    pub known: Cell<u32>,
    /// Asks whose cell this stretch froze (with what that freeze needed and froze first).
    pub froze: Cell<u32>,
    /// Asks of an item declared in the asking body, lowered in its own store ([`of_the_body`]).
    pub own: Cell<u32>,
    body_locals: BodyLocals,
}

/// What a [`RequestSeed`] is made over. `freezer` is made with `known: Some(heads)`, so a
/// freeze takes every need the request knows.
pub struct RequestSeedOpts<'k, 'n, 'a, N, H> {
    pub names: N,
    pub heads: &'k H,
    pub freezer: SeededHeads<'n, 'a, N>,
    /// Where a need of an item declared in the asking body is made: [`BodyLocals::Lowered`],
    /// or a test's plant.
    pub body_locals: BodyLocals,
}

impl<'k, 'n, 'a, N: NamesSource<'n>, H: RequestHeads> RequestSeed<'k, 'n, 'a, N, H> {
    pub fn new(o: RequestSeedOpts<'k, 'n, 'a, N, H>) -> RequestSeed<'k, 'n, 'a, N, H> {
        RequestSeed { names: o.names, heads: o.heads, freezer: RefCell::new(o.freezer), known: Cell::new(0), froze: Cell::new(0), own: Cell::new(0), body_locals: o.body_locals }
    }

    /// The wants this stretch's freezes lowered, and the freeze attempts they took.
    pub fn freezer_counts(&self) -> (u32, u32) {
        let freezer = self.freezer.borrow();
        (freezer.lowered, freezer.attempts)
    }

    /// Inspect the cell's retained failure while its freezer is borrowed.
    pub fn with_failure<R>(&self, cell: Want, inspect: impl FnOnce(CellFailure<'_>) -> R) -> Option<R> {
        let freezer = self.freezer.borrow();
        let (failure, _) = freezer.failure(cell)?;
        Some(inspect(failure))
    }
}

impl<'n, N: NamesSource<'n>, H: RequestHeads> HeadSeed<'n> for RequestSeed<'_, 'n, '_, N, H> {
    fn seed(&self, store: &Store<'n>, keys: &Keys, need: Want, scratch: Alloc<'static>) -> Result<Seeded, SeedError> {
        // An item declared in the asking body: the body's own store lowers it (module docs),
        // before the need is made a cell or asked of the request (which holds none of it).
        if self.body_locals == BodyLocals::Lowered && of_the_body(store, need) {
            self.own.set(self.own.get().saturating_add(1));
            return Ok(Seeded::Lower);
        }
        let cell = cell_of(self.names, need).map_err(SeedError::Freeze)?;
        match self.heads.known(cell) {
            Known::Bytes(bytes) => {
                self.known.set(self.known.get().saturating_add(1));
                let frozen = FrozenHead::read(bytes).map_err(|error| SeedError::Freeze(FreezeError::Read { want: cell, error }))?;
                file_frozen(self.names, cell, frozen, store, keys, scratch).map_err(SeedError::Freeze)?;
                return Ok(Seeded::Filed);
            }
            // Not this stretch's to freeze (a member crate's head its task did not freeze for
            // this unit): refused, typed, before any freeze (rule 3).
            Known::Refused(why) => return Err(SeedError::of_freeze(FreezeError::NotTheirs { want: cell, why })),
            Known::Freeze => {}
        }
        // Not known and the stretch's to freeze: frozen here (its needs taken from the request
        // where known, a need not the stretch's refused there), then each cell this ask froze
        // published, needs first, before the body reads on.
        let mut freezer = self.freezer.borrow_mut();
        let from = freezer.kept_len();
        let failed_from = freezer.failed_len();
        let seeded = seed(self.names, None, need, &mut *freezer, store, keys, scratch);
        for (w, bytes) in freezer.frozen_from(from) {
            let krate = NamesSource::crate_of(self.names, w.key()).and_then(|k| NamesSource::crate_key(self.names, k));
            self.heads.publish(w, krate, bytes)?;
        }
        // A freeze that failed on this ask: its reason handed to the request with the cell it
        // is about, before any caller can drop it (`RequestHeads::refused`).
        // (Lane HF1: every ask of a failed cell is `Again`, so the reasons are read from the
        // cells the source kept as failed on this ask, each its own.)
        if seeded.is_err() {
            for (failed, failure, _) in freezer.failures_from(failed_from) {
                match failure {
                    CellFailure::Own(error) => self.heads.refused(failed, error),
                    CellFailure::Needs(needed) => self.heads.refused(failed, &FreezeError::Again { want: failed, failed: needed }),
                    CellFailure::Refused { .. } => {}
                }
            }
        }
        self.froze.set(self.froze.get().saturating_add(1));
        seeded.map_err(SeedError::of_freeze)
    }
}
