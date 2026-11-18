//! HOW A BODY'S NAMES READS ARE KEPT (lane BH1, rule 78): one seam, a type parameter of
//! engine_bodies.rs `widen`, with two strategies tested equal in answers
//! (tests/engine_scope_reads_tests.rs).
//!
//! - [`Whole`], today's: the impl-bucket fact of the whole graph (`ImplBucket` of no crate,
//!   blaze-ladder tables/view.rs `incoherent_crates`, asked by every probe of a primitive's
//!   inherent methods: `i64::wrapping_add`, `str::len`) is widened to every scope crate's
//!   `CrateScope` record WHOLE. A member's record moves with any item added to it, so one
//!   private fn added re-entered every body that called a method on a primitive (the tiny
//!   fixture, g200823: 66 of 72).
//! - [`Cells`]: that coarse read is dropped. Every fact behind it has a cell the body read
//!   beside it: which crates are asked (the lang set: the owner and, without `#![no_core]`,
//!   the toolchain crates, fixed by the owner's root, its `Root` cell, whose digest holds its
//!   head (`no_core`) and its dependencies) and each one's answer (its
//!   `NameCell::Incoherent`, blaze-scope names_cells.rs). A dependency defines no cell table,
//!   so its cells are read as its record whole (engine_bodies.rs `widen`), which no edit in
//!   the checkout moves.
//!
//! What stays coarse under both (the census, reported by lane BH1): the sections read whole
//! (`NameCell::Whole`: the files, every impl row, every item, a failed call's rows, the crates
//! linked), an expansion entry with no items (its member's record whole), a head a macro wrote
//! or a dependency's (its crate's record whole), a head task's failure (its member's record
//! whole), every refusal (the cells it is a function of, whole), the `ImplBucket` task's own
//! crate's record (its impl rows: whole; under `Whole` its other names reads and every header's
//! head read are widened to records whole too, [`ScopeReads::TABLES_AT_CELLS`]), and the script
//! run (whole).
//!
//! A DEPENDENCY'S BUCKETS (lane BD3, under both strategies): a body's read of a bucket of a
//! crate that defines no cell table (`CELLS` ABSENT: engine_names.rs `cells_of` defines one only
//! for a crate whose names read a checkout file, a member's or a path dependency's) is its impl
//! tables task's `WHOLE` (engine_bodies.rs `widen`, `dependency_tables`), one cell a crate,
//! where it was one cell a bucket probed: on a 700-crate workspace a body's lookups probe a
//! bucket in most of its scope's crates, and each cell is a read in the row, in the engine's
//! cell index and in its reverse index for the request's whole life.
//! - SOUND: `WHOLE` is the digest of the tables' records (front2 buckets/records.rs: the
//!   directory, which names every bucket's content id, and `NoImplsOf`), so it moves whenever
//!   any bucket cell of that crate moves, a bucket going from absent to filed included: the
//!   coarse read never misses an edit its fine reads would have seen (a superset; rule 21).
//! - WHAT IT COSTS: precision only where a crate's tables move, and a dependency's move only
//!   with its text, which is keyed by the lock and the shelf (registry sources are never
//!   statted): a new graph, which re-enters every body of its readers anyway.
//! - WHERE IT WOULD BE WRONG: an edited PATH dependency (or any crate whose text the checkout
//!   holds). It is not unsound (the digest still moves) but every body that probed any of its
//!   buckets would re-enter on any impl added to it, which is why such a crate keeps its
//!   bucket cells: it defines `CELLS`, and `dependency_tables` reads it as fine. Reading it whole
//!   would be UNSOUND only if `WHOLE` were a key of the tables' inputs that an edit can leave
//!   unmoved (a lock digest, say), not their content's digest; it is the content's.

use blaze_drive::Name;
use blaze_drive::cells::Cell;
use blaze_engine::CellId;
use blaze_ids::ContentId;
use std::alloc::Global;

/// The cells one Heads job's tasks define, each task's (its name, and what it defines; `None`
/// for one that failed), sorted by cell: what a head's read of a sibling head of its crate is
/// recorded at (engine_heads.rs). Bounded by the job's heads, each a row of its crate's outline.
pub(crate) fn job_cells<'d>(tasks: impl Iterator<Item = (Name, Option<&'d [(Cell, ContentId)]>)>) -> std::vec::Vec<(CellId, ContentId), Global> {
    let mut cells: std::vec::Vec<(CellId, ContentId), Global> = std::vec::Vec::new_in(Global);
    for (of, defines) in tasks {
        for (cell, digest) in defines.unwrap_or(&[]).iter() {
            cells.push((CellId { of, cell: *cell }, *digest));
        }
    }
    cells.sort_unstable_by_key(|(id, _)| *id);
    cells
}

/// The digest `id` was made at by this job ([`job_cells`]); `None`: no task of the job made it.
pub(crate) fn today(cells: &[(CellId, ContentId)], id: &CellId) -> Option<ContentId> {
    cells.binary_search_by_key(id, |(cell, _)| *cell).ok().map(|at| cells[at].1)
}

use crate::engine_tasks::{CELLS, HEADS, SURFACE, WHOLE};

/// The kind of a cell that moved, for the profile's "entered because" lines (lane BH1, part E):
/// a coarse read shows as `whole record`, so a cause at cell grain and one widened to a record
/// are told apart at a glance. A names cell's key is a hash of its `NameCell` (blaze-scope
/// names_cells.rs `NameCell::key`), so which variant it was is not read back from it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum CellKind {
    /// A record read whole ([`WHOLE`]).
    WholeRecord,
    /// An outline's surface ([`SURFACE`]).
    Surface,
    /// An outline's heads digest ([`HEADS`]).
    HeadsDigest,
    /// A crate's mark that it defines its cell table ([`CELLS`]).
    CellTable,
    /// One cell of a crate's names (a `CrateScope` cell but the above).
    NamesCell,
    /// One head task's cell (a `Signature` cell).
    HeadCell,
    /// One impl-bucket cell (an `ImplBucket` cell).
    BucketCell,
    /// Any other task's cell.
    Other,
}

impl CellKind {
    /// The kind of `id`.
    pub(crate) fn of(id: &CellId) -> CellKind {
        use blaze_drive::Kind;
        match (id.cell, id.of.kind) {
            (cell, _) if cell == WHOLE => CellKind::WholeRecord,
            (cell, Kind::FileItems) if cell == SURFACE => CellKind::Surface,
            (cell, Kind::FileItems) if cell == HEADS => CellKind::HeadsDigest,
            (cell, Kind::CrateScope) if cell == CELLS => CellKind::CellTable,
            (_, Kind::CrateScope) => CellKind::NamesCell,
            (_, Kind::Signature) => CellKind::HeadCell,
            (_, Kind::ImplBucket) => CellKind::BucketCell,
            _ => CellKind::Other,
        }
    }

    /// Its words in the profile.
    pub(crate) fn words(self) -> &'static str {
        match self {
            CellKind::WholeRecord => "whole record",
            CellKind::Surface => "surface",
            CellKind::HeadsDigest => "heads digest",
            CellKind::CellTable => "cell table mark",
            CellKind::NamesCell => "names cell",
            CellKind::HeadCell => "head cell",
            CellKind::BucketCell => "bucket cell",
            CellKind::Other => "cell",
        }
    }
}

/// Which strategy `engine-check` keeps a body's names reads by: chosen once a command
/// (`engine_check::Opts`), dispatched to a [`ScopeReads`] instantiation at one call
/// (engine_bodies.rs `run_stretch`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScopeReadsAs {
    /// [`Whole`].
    Whole,
    /// [`Cells`].
    Cells,
    /// PLANTED BUG, tests only: [`PlantedCells`].
    #[cfg(test)]
    PlantedCells,
}

impl ScopeReadsAs {
    /// The command's default, the one constant the next piece flips to `Cells` once its
    /// tests, the engine tests and the 500-module crate's answers are equal under it.
    pub(crate) const DEFAULT: ScopeReadsAs = ScopeReadsAs::Whole;
}

/// The strategy of one seam: what a body's coarse reads widen to.
pub(crate) trait ScopeReads {
    /// The read of the whole graph's impl-bucket fact (`ImplBucket` of no crate): each
    /// `CrateScope` cell it stands for is given to `push`. `scope` is the group's scope crates'
    /// `CrateScope` names.
    fn incoherent_fact(scope: &[Name], push: &mut dyn FnMut(Name, Cell));

    /// Whether a names read `id` of a body of the member whose `CrateScope` is `own` is kept.
    /// Every strategy but the planted one keeps all.
    fn keeps(_own: Name, _id: CellId) -> bool {
        true
    }

    /// The `ImplBucket` task's reads (engine_heads.rs `run_phase`, `tables_of`). `true`: each
    /// names read its cell where the crate defines a cell table (engine_heads.rs `names_read`),
    /// and each impl header's head read the cell of the head task that covers that head
    /// (`Owners::place`, as a body's head read: a dependency's head, or one a macro wrote, its
    /// crate's record whole). `false`: each names read its crate's record whole, and every
    /// header read every scope crate's record whole, so any item added to any of them enters it.
    /// The task's own crate's record stays whole under both: its impl rows are read whole
    /// (`NamesSource::each_impl_row`, no cell holds them).
    const TABLES_AT_CELLS: bool;
}

impl ScopeReadsAs {
    /// [`ScopeReads::TABLES_AT_CELLS`] of the chosen strategy.
    pub(crate) fn tables_at_cells(self) -> bool {
        match self {
            ScopeReadsAs::Whole => <Whole as ScopeReads>::TABLES_AT_CELLS,
            ScopeReadsAs::Cells => <Cells as ScopeReads>::TABLES_AT_CELLS,
            #[cfg(test)]
            ScopeReadsAs::PlantedCells => <PlantedCells as ScopeReads>::TABLES_AT_CELLS,
        }
    }
}

/// Today's strategy: the impl-bucket fact read as every scope crate's record, whole.
pub(crate) struct Whole;

impl ScopeReads for Whole {
    const TABLES_AT_CELLS: bool = false;

    fn incoherent_fact(scope: &[Name], push: &mut dyn FnMut(Name, Cell)) {
        for name in scope.iter() {
            push(*name, WHOLE);
        }
    }
}

/// Cell grain: the impl-bucket fact read through the cells the body recorded beside it (each
/// lang crate's `Root` and `Incoherent`), so nothing more is pushed for it.
pub(crate) struct Cells;

impl ScopeReads for Cells {
    const TABLES_AT_CELLS: bool = true;

    fn incoherent_fact(_scope: &[Name], _push: &mut dyn FnMut(Name, Cell)) {}
}

/// PLANTED BUG, tests only: [`Cells`] that also drops every cell read of the member's own
/// names (a glob's member set among them: the `Binding` and `Module` cells a glob import is
/// resolved through). A body whose answer a glob's new member changes then stands on a stale
/// answer, which the oracle (a fresh run) must catch (tests/engine_scope_reads_tests.rs).
#[cfg(test)]
pub(crate) struct PlantedCells;

#[cfg(test)]
impl ScopeReads for PlantedCells {
    const TABLES_AT_CELLS: bool = true;

    fn incoherent_fact(_scope: &[Name], _push: &mut dyn FnMut(Name, Cell)) {}

    fn keeps(own: Name, id: CellId) -> bool {
        !(id.of == own && id.cell != WHOLE)
    }
}
