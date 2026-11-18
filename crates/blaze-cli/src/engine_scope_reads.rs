//! HOW A BODY'S NAMES READS ARE KEPT (lane BH1, rule 78): one seam, a type parameter of
//! engine_bodies.rs `widen`, with two strategies tested equal in answers
//! (tests/engine_scope_reads_tests.rs).
//!
//! - [`Whole`], today's: the impl-bucket fact of the whole graph (`ImplBucket` of no crate,
//!   blaze-ladder tables/view.rs `incoherent_crates`, asked by every probe of a primitive's
//!   inherent methods: `i64::wrapping_add`, `str::len`) reads the whole scope, including the
//!   graph proof for its settled crates. A member's record moves with any item added to it, so one
//!   private fn added re-entered every body that called a method on a primitive (the tiny
//!   fixture, g200823: 66 of 72).
//! - [`Cells`]: that coarse read is dropped. Every fact behind it has a cell the body read
//!   beside it: which crates are asked (the lang set: the owner and, without `#![no_core]`,
//!   the toolchain crates, fixed by the owner's root, its `Root` cell, whose digest holds its
//!   head (`no_core`) and its dependencies) and each one's answer (its
//!   `NameCell::Incoherent`, blaze-scope names_cells.rs).
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
//! Both strategies use the same proof for settled crates. The graph record contains the
//! source key and tree digest of each package or toolchain tree. A successful names record
//! whose inputs are fixed by those trees, and its successful impl tables, read the graph
//! cell. A checkout keeps its published cells. Other dependencies keep their own records.

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

/// What can cover a read of a crate's names or impl tables in this snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadGrain {
    Cell,
    Record,
    Graph,
}

impl ReadGrain {
    pub(crate) fn place(self, id: CellId, graph: CellId) -> CellId {
        match self {
            Self::Cell => id,
            Self::Record => CellId { of: id.of, cell: WHOLE },
            Self::Graph => graph,
        }
    }
}

/// At most two entries per unit. The CELLS mark and the state of the tables are read once
/// per crate before workers start. An ID outside these records keeps its original grain.
pub(crate) struct ReadGrains<'a> {
    by_name: blaze_hash::HashMap<Name, ReadGrain, blaze_own::SharedAlloc<'a>>,
    graph: CellId,
}

impl<'a> ReadGrains<'a> {
    pub(crate) fn new(list: blaze_world::crate_list::CrateList<'_>, packs: &[blaze_fetch::PackView<'_>], graph: CellId, view: blaze_engine::View<'_, '_>, alloc: blaze_own::SharedAlloc<'a>) -> Self {
        use blaze_drive::key::{ABSENT, crate_scope_name};
        use blaze_world::{PackOrigin, Side};
        use blaze_world::crate_list::CrateGraph;
        let n = CrateGraph::len(&list);
        let mut settled = blaze_own::Vec::with_capacity_in(n, alloc.local());
        let mut crates = blaze_own::Vec::with_capacity_in(n, alloc.local());
        for at in 0..n {
            let key = list.key(at);
            let scope = crate_scope_name(&key);
            let cells = view.cell(&CellId { of: scope, cell: CELLS }) != ABSENT;
            let tables = blaze_ladder::front2::buckets::impl_tables_name(key);
            crates.push((scope, tables, cells));
            // A tree is one-based in the checked crate list. The pack's kind distinguishes
            // registry archives from git records (both have PackOrigin::Source).
            let tree = list.root(at).strip_prefix('/').and_then(|s| s.split_once('/'))
                .and_then(|(tree, _)| tree.parse::<usize>().ok());
            let origin = tree.and_then(|k| Some((list.tree(k)?.origin, packs.get(k.checked_sub(1)?)?.kind())));
            let pinned = matches!((list.toolchain(at), origin),
                (true, Some((PackOrigin::Shelf { .. }, blaze_fetch::pack::Kind::Tree)))
                | (false, Some((PackOrigin::Source { .. }, blaze_fetch::pack::Kind::Crate))));
            settled.push(pinned && !list.member(at) && !cells && !list.refused(at)
                && list.script(at).is_none() && !list.proc_macro(at)
                && view.row(scope).is_some_and(|row| !row.failed));
        }
        // An immutable archive can resolve a path through a patched dependency or import
        // names generated by its build script. Its derived records then have a mutable input.
        // Each changing pass removes at least one of the n candidates.
        for _ in 0..n {
            let mut changed = false;
            let sysroot_settled = list.sysroot().into_iter().flatten().all(|dep| settled[dep]);
            for at in 0..n {
                if settled[at] && (list.deps(at).any(|dep| !settled[dep])
                    || (list.side(at) == Side::Target && !sysroot_settled)) {
                    settled[at] = false;
                    changed = true;
                }
            }
            if !changed { break }
        }
        let mut by_name = blaze_hash::HashMap::with_capacity_in(2 * n, alloc);
        for (at, &(scope, tables, cells)) in crates.iter().enumerate() {
            let grain = if settled[at] { ReadGrain::Graph } else if cells { ReadGrain::Cell } else { ReadGrain::Record };
            by_name.insert(scope, grain);
            // Missing/failed tables may become available independently of a graph edit.
            let tables_grain = if settled[at] && !view.row(tables).is_some_and(|row| !row.failed) { ReadGrain::Record } else { grain };
            by_name.insert(tables, tables_grain);
        }
        Self { by_name, graph }
    }

    pub(crate) fn place(&self, id: CellId, fine_buckets: bool) -> CellId {
        if fine_buckets && id.of.kind == blaze_drive::Kind::ImplBucket {
            return id;
        }
        self.by_name.get(&id.of).copied().unwrap_or(ReadGrain::Cell).place(id, self.graph)
    }
}

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
