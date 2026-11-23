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

use crate::engine_tasks::{CELLS, EXPORTS, HEADS, SURFACE, WHOLE};

#[cfg(test)]
thread_local! {
    /// PLANTED CONTROL, tests only (rule 23): a settled crate's read is placed at the graph's
    /// whole cell even where the graph defines its crate cells (the grain before D187), so a
    /// dependency bumped enters every body that reads any settled crate.
    pub(crate) static PLANT_WHOLE_GRAPH: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// What can cover a read of a crate's names or impl tables in this snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadGrain {
    Cell,
    Record,
    /// Settled by the graph: read at this cell of the graph's record, the crate's own
    /// (engine_units.rs `crate_cell`) when the graph defines its crate cells, else its whole.
    Graph(Cell),
}

impl ReadGrain {
    pub(crate) fn place(self, id: CellId, graph: CellId) -> CellId {
        match self {
            Self::Cell => id,
            Self::Record => CellId { of: id.of, cell: WHOLE },
            Self::Graph(cell) => CellId { of: graph.of, cell },
        }
    }
}

/// WHAT [`ReadGrains`] IS, and so what it is a function of: for each crate of the crate list,
/// the grain at which a body's read of that crate's names (its `CrateScope` cells) and of its
/// impl tables (its `ImplBucket` row) is kept, the cell itself, its record whole, or the graph's
/// cell for a crate settled by the graph (its inputs fixed by its pack's tree). It is a function
/// of the graph's record (the crate list and its packs: which crates are pinned, members,
/// refused, scripts, proc macros, their dependencies and the sysroot) and of three facts of the
/// answer a crate, the record's read list ([`Facts`]): whether its names task defines a cell table
/// (its `CrateScope` `CELLS` cell), whether its names row stands unfailed, and whether its impl
/// tables row does. Nothing else: so it is a derived row, kept ([`GrainsIn`]): made whole once,
/// and on a later request taken from the stored record with only the crates whose names or impl
/// tables task this request entered read again (forward from what changed: a fact moves only by
/// its own task's row), never the whole list's 3 x crates answer reads, each a page of the
/// stored answer (the measurement of 2026-11-20 on tempo: 1,240 task pages and all 128 producer
/// header shards for a one-function edit, a third of its CPU).
///
/// At most two entries per crate. An ID outside these records keeps its original grain.
pub(crate) struct ReadGrains<'a> {
    by_name: blaze_hash::HashMap<Name, ReadGrain, blaze_own::SharedAlloc<'a>>,
    graph: CellId,
}

/// One crate's facts of the answer, a byte of the stored record: what its grain is made from
/// beside the graph's record.
const CELL_TABLE: u8 = 1;
/// Its names row stands unfailed (read only for a crate the graph could settle: pinned, not a
/// member, refused, script or proc macro, and with no cell table).
const NAMES_STAND: u8 = 2;
/// Its impl tables row stands unfailed (read only where [`NAMES_STAND`] was).
const TABLES_STAND: u8 = 4;

/// The stored grains record's form: `"BRG2" | graph out [32] | made at serial u64 | crates u32 |
/// crate cells u8 (0 or 1: the graph defines a cell per crate, a function of the graph's record,
/// so of the out above) | one facts byte a crate, in the crate list's order`. A record of the
/// form before ("BRG1": no crate cells byte) is refused as [`WholeWhy::Form`] and made again
/// (rule 26).
pub(crate) const GRAINS_MAGIC: &[u8; 4] = b"BRG2";
const GRAINS_HEAD: usize = 4 + 32 + 8 + 4 + 1;

/// What [`ReadGrains::new`] may take the facts from: the last request's record, and what makes
/// it valid today. The record holds the facts as the answer had them when the request at its
/// serial made it; the last answer is that request's answer exactly when its serial is the last
/// answer's (`last_serial`), and from it to now only the tasks this request entered (`entered`)
/// moved a fact. So `entered` must hold EVERY `CrateScope` and `ImplBucket` task this request
/// entered before the Bodies phase (engine_check.rs feeds it phase by phase,
/// `BodiesStage::note_entered`): one left out is a stale fact.
#[derive(Clone, Copy)]
pub(crate) struct GrainsIn<'g> {
    /// The graph record's out: the crate list and its packs (a moved graph makes the facts whole).
    pub(crate) graph_out: ContentId,
    /// The last request's record, in place; `None`: none is kept (a fresh home, or no answer).
    pub(crate) stored: Option<&'g [u8]>,
    /// The serial of the answer this request starts from (`Answer::serial`); this request's is
    /// one past it (blaze-engine engine.rs `Engine::new_in`), and its record is stamped so.
    pub(crate) last_serial: u64,
    /// The names of the tasks entered this request so far, sorted.
    pub(crate) entered: &'g [Name],
    /// PLANTED CONTROL, tests only: the record is never taken (made whole every command).
    pub(crate) rebuilt: bool,
}

/// Why the facts were read whole, not taken from a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WholeWhy {
    /// No record was kept.
    None,
    /// The record is of another form (an old home's: rebuilt, rule 26).
    Form,
    /// The graph moved since it was made.
    Graph,
    /// It was made at another serial than the last answer's (a request that did not save its
    /// answer, or one that saved without a Bodies phase).
    Serial { made: u64, last: u64 },
    /// It holds another number of crates than the list (a form fault: the graph is in its key).
    Crates { held: u32, now: u32 },
    /// The planted control.
    Planted,
}

impl WholeWhy {
    fn words(self) -> &'static str {
        match self {
            WholeWhy::None => "no record kept",
            WholeWhy::Form => "the record is of an older form",
            WholeWhy::Graph => "the graph moved",
            WholeWhy::Serial { .. } => "the record was made at another serial than the last answer",
            WholeWhy::Crates { .. } => "the record holds another number of crates",
            WholeWhy::Planted => "PLANTED: never taken",
        }
    }
}

/// What [`ReadGrains::new`] made: the grains, the record to put (a store key by its bytes), and
/// how it came to be, for the profile's `read grains` line.
pub(crate) struct MadeGrains<'a> {
    pub(crate) grains: ReadGrains<'a>,
    pub(crate) record: blaze_own::AVec<'a, u8>,
    pub(crate) line: GrainsLine,
}

/// The profile's `bodies: read grains` line: the crates, how many had their facts read from
/// the answer (floor: the crates whose names or tables task this request entered; a cold home
/// or a moved graph: every crate), and the answer reads that took (each a cell or row of the
/// stored answer: at most three a crate).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct GrainsLine {
    pub(crate) crates: u32,
    pub(crate) refreshed: u32,
    pub(crate) answer_reads: u32,
    /// `None`: taken from the record.
    pub(crate) whole: Option<WholeWhy>,
}

impl GrainsLine {
    /// Sums `other` (a later Bodies phase of the request) into this line.
    pub(crate) fn add(&mut self, other: GrainsLine) {
        self.crates = self.crates.max(other.crates);
        self.refreshed = self.refreshed.saturating_add(other.refreshed);
        self.answer_reads = self.answer_reads.saturating_add(other.answer_reads);
        self.whole = self.whole.or(other.whole);
    }
}

impl core::fmt::Display for GrainsLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bodies: read grains of {} crates: ", self.crates)?;
        match self.whole {
            Some(why) => write!(f, "made whole ({})", why.words())?,
            None => write!(f, "taken from the stored record")?,
        }
        write!(f, "; facts read for {} crates in {} answer reads (floor: the crates whose names or tables task ran)", self.refreshed, self.answer_reads)
    }
}

/// The facts of the last record, when it is valid today ([`GrainsIn`]): whether the graph
/// defines a cell per crate, and a facts byte a crate.
fn stored_facts<'g>(from: &GrainsIn<'g>, crates: usize) -> Result<(bool, &'g [u8]), WholeWhy> {
    if from.rebuilt {
        return Err(WholeWhy::Planted);
    }
    let bytes = from.stored.ok_or(WholeWhy::None)?;
    if bytes.len() < GRAINS_HEAD || &bytes[..4] != GRAINS_MAGIC {
        return Err(WholeWhy::Form);
    }
    if bytes[4..36] != from.graph_out.0 {
        return Err(WholeWhy::Graph);
    }
    let made = u64::from_le_bytes(bytes[36..44].try_into().expect("eight bytes"));
    if made != from.last_serial {
        return Err(WholeWhy::Serial { made, last: from.last_serial });
    }
    let held = u32::from_le_bytes(bytes[44..48].try_into().expect("four bytes"));
    let crate_cells = match bytes[48] {
        0 => false,
        1 => true,
        _ => return Err(WholeWhy::Form),
    };
    let facts = &bytes[GRAINS_HEAD..];
    if held as usize != crates || facts.len() != crates {
        return Err(WholeWhy::Crates { held, now: u32::try_from(crates).unwrap_or(u32::MAX) });
    }
    Ok((crate_cells, facts))
}

impl<'a> ReadGrains<'a> {
    /// The grains of `list`'s crates, their facts taken from `from`'s record where it is valid
    /// and read from `view` for every crate it does not cover ([`GrainsIn`]); with the record
    /// this request puts for the next. `view` is today's digests and rows through the engine's
    /// read seam (`blaze_engine::CellDigests`): the phase engine's `View`, or the `Ready` host's
    /// (D187), the same answers either way.
    pub(crate) fn new<V: ?Sized + blaze_engine::CellDigests>(list: blaze_world::crate_list::CrateList<'_>, graph: CellId, view: &V, from: GrainsIn<'_>, alloc: blaze_own::SharedAlloc<'a>) -> MadeGrains<'a> {
        use blaze_drive::key::{ABSENT, crate_scope_name};
        use blaze_world::{PackOrigin, Side};
        use blaze_world::crate_list::CrateGraph;
        let n = CrateGraph::len(&list);
        let stored = stored_facts(&from, n);
        let mut line = GrainsLine { crates: u32::try_from(n).unwrap_or(u32::MAX), whole: stored.err(), ..GrainsLine::default() };
        let mut record = blaze_own::Vec::with_capacity_in(GRAINS_HEAD + n, alloc.local());
        record.extend_from_slice(GRAINS_MAGIC);
        record.extend_from_slice(&from.graph_out.0);
        record.extend_from_slice(&from.last_serial.saturating_add(1).to_le_bytes());
        record.extend_from_slice(&line.crates.to_le_bytes());
        // The crate cells byte, set below once it is known.
        record.push(0);
        let mut settled = blaze_own::Vec::with_capacity_in(n, alloc.local());
        let mut crates = blaze_own::Vec::with_capacity_in(n, alloc.local());
        for at in 0..n {
            let key = list.key(at);
            let scope = crate_scope_name(&key);
            let tables = blaze_ladder::front2::buckets::impl_tables_name(key);
            // A tree is one-based in the checked crate list. The pack's kind distinguishes
            // registry archives from git records (both have PackOrigin::Source); it is the tree
            // row's (crate_list.rs `TreeRow::kind`, version 7), so no pack is mapped here.
            let tree = list.root(at).strip_prefix('/').and_then(|s| s.split_once('/'))
                .and_then(|(tree, _)| tree.parse::<usize>().ok());
            let origin = tree.and_then(|k| list.tree(k)).map(|row| (row.origin, row.kind));
            let pinned = matches!((list.toolchain(at), origin),
                (true, Some((PackOrigin::Shelf { .. }, blaze_fetch::pack::Kind::Tree)))
                | (false, Some((PackOrigin::Source { .. }, blaze_fetch::pack::Kind::Crate))));
            // Of the graph's record alone: whether the graph could settle this crate.
            let candidate = pinned && !list.member(at) && !list.refused(at) && list.script(at).is_none() && !list.proc_macro(at);
            let moved = |name: &Name| from.entered.binary_search(name).is_ok();
            let facts = match stored {
                Ok((_, held)) if !moved(&scope) && !moved(&tables) => held[at],
                _ => {
                    line.refreshed += 1;
                    let mut facts = 0u8;
                    line.answer_reads += 1;
                    if view.cell(&CellId { of: scope, cell: CELLS }) != ABSENT {
                        facts |= CELL_TABLE;
                    }
                    if candidate && facts & CELL_TABLE == 0 {
                        line.answer_reads += 1;
                        if view.row_failed(scope) == Some(false) {
                            facts |= NAMES_STAND;
                            // Read wherever the graph could settle the crate (not only where it
                            // does): a dependency's facts moving settles or unsettles this one
                            // with no task of its own entered.
                            line.answer_reads += 1;
                            if view.row_failed(tables) == Some(false) {
                                facts |= TABLES_STAND;
                            }
                        }
                    }
                    facts
                }
            };
            record.push(facts);
            let cells = facts & CELL_TABLE != 0;
            crates.push((scope, tables, cells, facts & TABLES_STAND != 0));
            settled.push(candidate && !cells && facts & NAMES_STAND != 0);
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
        // Whether the graph defines a cell per crate (engine_units.rs `CRATE_CELLS`): a function
        // of the graph's record alone, so taken from the stored record with the facts (that
        // record is valid only for the same graph out), else one read of the answer (rule 77).
        // Where it does, a settled crate's read is its own cell, so a dependency bumped enters
        // only the bodies that read it (or a crate keyed through it); a graph made before
        // defines its whole cell alone.
        let defined = match stored {
            Ok((held, _)) => held,
            Err(_) => {
                line.answer_reads += 1;
                view.cell(&CellId { of: graph.of, cell: crate::engine_units::CRATE_CELLS }) != ABSENT
            }
        };
        record[GRAINS_HEAD - 1] = u8::from(defined);
        #[cfg(test)]
        let planted = PLANT_WHOLE_GRAPH.with(core::cell::Cell::get);
        #[cfg(not(test))]
        let planted = false;
        let crate_cells = !planted && defined;
        let settled_at = |at: usize| if crate_cells { crate::engine_units::crate_cell(&list.key(at)) } else { graph.cell };
        let mut by_name = blaze_hash::HashMap::with_capacity_in(2 * n, alloc);
        for (at, &(scope, tables, cells, tables_stand)) in crates.iter().enumerate() {
            let grain = if settled[at] { ReadGrain::Graph(settled_at(at)) } else if cells { ReadGrain::Cell } else { ReadGrain::Record };
            by_name.insert(scope, grain);
            // Missing/failed tables may become available independently of a graph edit.
            let tables_grain = if settled[at] && !tables_stand { ReadGrain::Record } else { grain };
            by_name.insert(tables, tables_grain);
        }
        MadeGrains { grains: Self { by_name, graph }, record, line }
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
    /// A crate's exports section ([`EXPORTS`]).
    Exports,
    /// One cell of a crate's names (a `CrateScope` cell but the above).
    NamesCell,
    /// One head task's cell (a `Signature` cell).
    HeadCell,
    /// One impl-bucket cell (an `ImplBucket` cell).
    BucketCell,
    /// One crate's cell of the unit graph (engine_units.rs `crate_cell`): what the graph
    /// settles about that crate alone.
    GraphCrate,
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
            (cell, Kind::CrateScope) if cell == EXPORTS => CellKind::Exports,
            (_, Kind::CrateScope) => CellKind::NamesCell,
            (_, Kind::Signature) => CellKind::HeadCell,
            (_, Kind::ImplBucket) => CellKind::BucketCell,
            (cell, Kind::UnitGraph) if cell != crate::engine_units::CRATE_CELLS => CellKind::GraphCrate,
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
            CellKind::Exports => "exports",
            CellKind::NamesCell => "names cell",
            CellKind::HeadCell => "head cell",
            CellKind::BucketCell => "bucket cell",
            CellKind::GraphCrate => "graph crate cell",
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
    /// The command's default: cell-grain scope reads and impl tables, so an unrelated item
    /// added to a crate does not wake bodies or impl tables that never read it.
    pub(crate) const DEFAULT: ScopeReadsAs = ScopeReadsAs::Cells;
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
    /// each impl header's head read the cell of the head task that covers that head
    /// (`Owners::place`, as a body's head read: a dependency's head, or one a macro wrote, its
    /// crate's record whole), and the task's own crate reads `NameCell::ImplRows` (moving only
    /// when impl rows change, not when private fns are added). `false`: each names read its
    /// crate's record whole, the task's own crate stays whole, and every header read every scope
    /// crate's record whole, so any item added to any of them enters it.
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
