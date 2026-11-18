//! The `Heads` stage of `blaze engine-check` (engine.md E6): each crate's impl tables made ONCE
//! a request as its own task, and shared read-only by every body.
//!
//! THE PLAN, from text: one `ImplBucket` task a crate of the names world, named
//! [`impl_tables_name`] of its key, in `Phase { stage: Heads, wave: 0 }` (after every crate's
//! names), its source and its file the crate's names task's, so its key is known before any
//! phase runs and it enters with its crate's names ([`plan`]).
//!
//! THE TASK, on the pool: the crate's check scope (engine_bodies.rs `scope_of`, the rule the
//! Bodies stage takes too), its records opened in the task's frame, its impl rows collected
//! (`NamesSource::each_impl_row` of the crate at 0), and front2 buckets/records.rs
//! `impl_tables_task` run over the read-only view with its own recorder. Its row:
//! - defines: the tables' own cells (a bucket's, a trait's) and `WHOLE`, the three records' digest;
//! - reads: the crate's names record whole (its impl rows are its items record's), each names
//!   read of the filing as its crate's names record whole (the Names stage defines that cell),
//!   and a header text the filing lowered as every scope crate's names record whole (no task
//!   defines a head's cell yet: the header's crate is one of the scope's, whose records hold its
//!   text); at cell grain (engine_scope_reads.rs `ScopeReads::TABLES_AT_CELLS`) each names read
//!   is its cell instead and each header read the cell of the head task covering it;
//! - its record in the store: the three records as one framed payload (buckets/records.rs
//!   `framed`), which a later request puts on its shelf while the task stands ([`complete`]).
//!
//! THE SHELF: each crate's records copied once onto engine_bodies.rs `BodiesStage::shelf` on the
//! coordinator after the phase; every body run reads it through its own `TableSet::lent`.
//! Nothing on it changes after the Heads phase, so nothing shared is mutable.
//!
//! THE HEAD TASKS (lane EY): one `Signature` task an outline entry that has a head, named and
//! keyed from text as the outline lists it ([`plan_items`]), in the same phase. Its record is the
//! frozen heads of every head cell the entry covers (front2 tables/head.rs `head_task`: a fn's
//! signature, an ADT, a trait with its members, an impl's header, an impl fn), each the cell
//! [`head_cell`] of the task with the frozen head's meaning, so a head re-made to equal bytes
//! moves no reader (early cutoff by digest, blaze-engine engine.rs). Its reads: each names cell its
//! freeze consulted, and for each head the freeze read (followed through the heads those read),
//! that head's head task's cell (a same-stage read, entered after its definer; before lane BH1
//! its file's `HEADS` cell, which every sibling head added moved)
//! or, for a dependency's head or one a macro wrote, its crate's names record whole. A body's
//! read of a member's head is the head's cell ([`Owners::place`], engine_bodies.rs `widen`). A
//! head that cannot be frozen fails ALONE (A8): the task defines the heads it froze, and the
//! failed head's cell is defined by its reason's digest ([`HEAD_FAILED_DOMAIN`]), so a reader
//! moves when the reason does; the row counts one finding a failed head, and its reason is a
//! head note (engine_head_notes.rs) a body that read it quotes. A task that cannot run its heads
//! at all (its crate's records, a cap) is a failed row, named by its file. A dependency's head is
//! read as its crate's names record whole (engine.md point 3). The freeze has a constant
//! evaluator, and a head that evaluated reads what the evaluation read (engine_head_consts.rs).
//!
//! ONE FREEZE A UNIT (lane RU1): a member crate is checked as several units (its lib, its test
//! harness, a bin that `mod`s its files), each with its own crate key, and an item's key holds
//! its unit's prefix. A head task (one an entry, by text) freezes its heads once for EACH member
//! unit whose items record lists its file, under that unit's records, and defines one cell a
//! (unit, head) ([`head_cell`] holds the unit); its payload is each unit's runs, its reads the
//! union of each unit's, and a unit that stops holding an item (a `cfg`) drops its cell. Which
//! units hold the file is read too (lane RU2): each unit whose module walk reaches the file but
//! whose items record does not list it is read by its not-held cell (engine_body_plan.rs
//! `not_held_cell`, the one home the body plan reads), so a `cfg` taken off a `mod` enters the
//! task again and the newly holding unit gets its cell (rule 21). A
//! reader finds the cell by its own key ([`Owners::place`]), so it is never handed bytes frozen
//! under another unit's keys. The profile counts it: "heads frozen: N for M items in U units
//! (K items held by 2+ units)" ([`UnitCounts`]).

use core::convert::Infallible;

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Locate, Name};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::buckets::{ImplShelf, framed, impl_tables_name, impl_tables_task};
use blaze_ladder::front2::by_origin;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{ConstValues, HeadCells, SeededHeads, SeededHeadsOpts, cell_id as head_cell_id, head_task, want_bytes};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::cell_id;
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{Alloc, SharedAlloc, Vec};
use blaze_scope::items_record::ItemsRecord;
use blaze_scope::resolve::At;
use blaze_surface::ItemIndex;
use blaze_world::crate_list::CrateGraph;
use bstd::cx::Cx;
use bstd::mem::Frame;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::engine_bodies::Handles;
use crate::engine_check::EngineCheckError;
use crate::engine_names::NamesStage;
use crate::engine_records::EngineRecords;
use crate::engine_tasks::{CELLS, Outlined, WHOLE};
use crate::run_names::Crate;
use crate::work;

/// A task's frame: its scope's records opened, its filing's scratch (blocks taken as used).
const HEADS_FRAME: usize = 1 << 30;

/// One `ImplBucket` task a crate of the names world (sorted by name).
pub(crate) fn plan<'h>(names: &NamesStage<'_>, alloc: SharedAlloc<'h>) -> Result<SVec<'h, Planned>, CliError> {
    let mut planned = SVec::new_in(alloc);
    let Some(world) = names.names.as_ref() else { return Ok(planned) };
    for (name, source) in crate::names::parts(world).iter() {
        let at = world.crate_at(name.subject).ok_or(EngineCheckError::Unseen { task: *name, read: *name })?;
        // A crate with no check scope (refused by the list, or a dependency's proc macro kept for
        // its macros only) has no tables task: it could only fail, and no body asks a proc
        // macro's or a refused crate's impls (blaze-world check_scope.rs `has_check_scope`).
        if !blaze_world::check_scope::has_check_scope(world.world(), at) {
            continue;
        }
        let tables = impl_tables_name(world.world().crates[at].input.key);
        planned.push(Planned { name: tables, phase: Phase::new(Stage::Heads, 0), source: *source, file: *name });
    }
    planned.sort_unstable_by_key(|one| one.name);
    Ok(planned)
}

/// One crate's tables as its task made them, all in `Global` (a worker's result crosses to the
/// coordinator, and the store's put reads it after the phase).
pub(crate) struct Made {
    pub(crate) name: Name,
    pub(crate) out: ContentId,
    /// Its read list as the store's record holds it: each read's record and digest, sorted.
    pub(crate) reads: std::vec::Vec<(Name, ContentId), Global>,
    /// The three records, [`framed`].
    pub(crate) payload: std::vec::Vec<u8, Global>,
}

/// What a worker hands back for one task.
struct Worked {
    out: ContentId,
    defines: std::vec::Vec<(Cell, ContentId), Global>,
    reads: std::vec::Vec<CellId, Global>,
    /// At cell grain: each header's head read where `Owners::place` put it (a head task's cell,
    /// or a dependency's record whole), read as is ([`HeadsStage::run_phase`]).
    heads: std::vec::Vec<CellId, Global>,
    payload: std::vec::Vec<u8, Global>,
}

/// One task's inputs, made on the coordinator.
struct Job<'r> {
    task: Task,
    krate: blaze_id::CrateItemKey,
    /// The crate's own `CrateScope` name: read whole.
    own: Name,
    /// Its check scope's crates, the crate at 0, their records in place in the store's mapping
    /// (engine_records.rs).
    crates: Vec<Crate<'r>, Global>,
    /// Each scope crate's `CrateScope` name, sorted: what a header read widens to.
    scope: Vec<Name, Global>,
    /// The cells a refusal of its tables is a function of ([`scope_cells`]).
    cells: std::vec::Vec<CellId, Global>,
}

/// The cells a refusal over crate `at`'s check scope is a function of (plan row A8): the
/// graph's (the crate list, the check scope and the packs are its record's), the crate's own
/// names cell, and with its `scope` each scope crate's names cell and the crate it was cut at
/// ([`crate::engine_bodies::Scope::cut`]), each `WHOLE`. A crate whose names task failed or did
/// not run defines no cell: read ABSENT, so its task that comes, or its fix, moves the row.
/// Unsorted: `failed_with` sorts and dedups.
fn scope_cells(graph: Name, list: &blaze_world::crate_list::CrateList<'static>, at: usize, scope: Option<&crate::engine_bodies::Scope<'_>>) -> std::vec::Vec<CellId, Global> {
    let mut cells: std::vec::Vec<CellId, Global> = std::vec::Vec::with_capacity_in(2 + scope.map_or(1, |scope| scope.scope.len() + 1), Global);
    cells.push(CellId { of: graph, cell: WHOLE });
    cells.push(CellId { of: blaze_drive::key::crate_scope_name(&list.key(at)), cell: WHOLE });
    if let Some(scope) = scope {
        cells.extend(scope.scope.iter().map(|one| CellId { of: *one, cell: WHOLE }));
        cells.extend(scope.cut_name(list).map(|one| CellId { of: one, cell: WHOLE }));
    }
    cells
}

/// The stage: each task's result and refusal.
pub(crate) struct HeadsStage {
    /// Each task entered and made, sorted by name after its phase.
    made: std::vec::Vec<Made, Global>,
    /// Each task refused, and each head a task could not freeze: which and why (rule 19).
    pub(crate) refused: std::vec::Vec<crate::engine_head_notes::Refused, Global>,
    /// Crates whose standing tables were read back from the store this request.
    pub(crate) read_back: u32,
    /// Under which units a head task freezes (the command's: [`HeadUnits::Each`]).
    pub(crate) freeze: HeadUnits,
    /// What the head tasks entered this request froze, by unit.
    pub(crate) units: UnitCounts,
    /// How the `ImplBucket` tasks' reads are kept (engine_scope_reads.rs
    /// `ScopeReads::TABLES_AT_CELLS`), the command's choice, as the bodies'.
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
}

impl HeadsStage {
    pub(crate) fn new(scope_reads: crate::engine_scope_reads::ScopeReadsAs) -> HeadsStage {
        HeadsStage { made: std::vec::Vec::new_in(Global), refused: std::vec::Vec::new_in(Global), read_back: 0, freeze: HeadUnits::Each, units: UnitCounts::default(), scope_reads }
    }

    /// Task `name`'s record, when it was entered and made this request.
    pub(crate) fn result(&self, name: Name) -> Option<&Made> {
        self.made.binary_search_by_key(&name, |one| one.name).ok().map(|at| &self.made[at])
    }

    /// The Heads phase: every task entered once, its result in `buffer`, its tables on `shelf`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        kept: &mut Kept,
        shelf: &mut ImplShelf,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &Frame,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: the heads phase");
        // The per-item head tasks first ([`HeadsStage::run_items`]), then the crates' tables.
        let (items, tasks): (std::vec::Vec<Task, Global>, std::vec::Vec<Task, Global>) = tasks.iter().copied().partition(|task| task.planned.name.kind == Kind::Signature);
        let tasks = &tasks[..];
        self.run_items(&cx, store, read, handles, kept, &items, view, buffer, times, alloc, frame)?;
        let heap = handles.env.heap;
        let at_cells = self.scope_reads.tables_at_cells();
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each read as the view holds it today
        // (ABSENT where no task defines it: a crate whose names task failed or did not run), so
        // its row stands until one moves (plan row A8). None: owed, entered again each request,
        // said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            refused.push(crate::engine_head_notes::Refused { name: task.planned.name, cell: None, head: "its crate's impl tables", why });
        };
        // 1. Each task's scope, on this thread.
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            // No `UnitGraph` task planned, or no names stage: no cell of this command says so,
            // so owed until the plan's inputs are cells (plan row D2).
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no crate's impl tables", &[], buffer);
            }
            return Ok(());
        };
        // The crate list, each crate's check scope and the packs are the graph record's.
        let graph_cell = CellId { of: graph.name, cell: WHOLE };
        let Some(list) = kept.list(&cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", &[graph_cell], buffer);
            }
            return Ok(());
        };
        let (packs, corrected) = match kept.trees(&cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => {
                // A pack missing from the shelf or the sources store, or not the one the list's
                // world read (body_types.rs `open_trees`): the machine's files, not a cell, so
                // owed (rule 3: no invented read).
                for task in tasks.iter() {
                    refuse(task, why, &[], buffer);
                }
                return Ok(());
            }
        };
        // Each crate of the list by its tables' name.
        let mut by_name: std::vec::Vec<(Name, usize), Global> = std::vec::Vec::with_capacity_in(CrateGraph::len(&list), Global);
        by_name.extend((0..CrateGraph::len(&list)).map(|at| (impl_tables_name(list.key(at)), at)));
        by_name.sort_unstable();
        let mut jobs: std::vec::Vec<Job<'r>, Global> =std::vec::Vec::with_capacity_in(tasks.len(), Global);
        for task in tasks.iter() {
            let Ok(found) = by_name.binary_search_by_key(&task.planned.name, |(name, _)| *name) else {
                // The list is the graph's: a new graph record moves it.
                refuse(task, "an `ImplBucket` task of a crate the crate list does not hold", &[graph_cell], buffer);
                continue;
            };
            let at = by_name[found].1;
            match crate::engine_bodies::scope_of(read, &cx, store, names, &list, at, heap) {
                Ok((scope, true)) => {
                    let cells = scope_cells(graph.name, &list, at, Some(&scope));
                    let own = names.input_of(blaze_drive::key::crate_scope_name(&list.key(at))).map(|input| input.name);
                    let Some(own) = own else {
                        refuse(task, "its crate has no names record this command (its `Names` task failed)", &cells, buffer);
                        continue;
                    };
                    jobs.push(Job { task: *task, krate: list.key(at), own, crates: scope.crates, scope: scope.scope, cells });
                }
                // The crate's names cell and each scope crate's that had records, and the first
                // that had none read ABSENT (`Scope::cut`): its task that comes, or its fix,
                // moves the row (engine_bodies.rs `scope_of`).
                // The cut's cause printed, typed, the crate named (rule 19).
                Ok((scope, false)) => {
                    let why = match scope.cut_cause(&list) {
                        Some(cause) => crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's {cause}")),
                        None => "its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)",
                    };
                    refuse(task, why, &scope_cells(graph.name, &list, at, Some(&scope)), buffer);
                }
                // `scope_of` reads the crate's own names record (ABSENT when its task failed)
                // and the check scope the graph makes: both cells. Its cause printed, typed.
                Err(cause) => refuse(task, crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), &scope_cells(graph.name, &list, at, None), buffer),
            }
        }
        // At cell grain, the members' files a header's head read is placed by (`Owners::place`).
        let subjects = match at_cells && !jobs.is_empty() {
            true => member_subjects(read, &cx, store, names, &list, handles.outlined, heap),
            false => std::vec::Vec::new_in(Global),
        };
        let subjects: Option<&Subjects> = at_cells.then_some(&subjects[..]);
        // 2. The tables, at once, on the workers.
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let clock = handles.clock;
        let mut worked: std::vec::Vec<(Result<Result<Worked, &'static str>, work::Panicked>, u64), Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = std::vec::Vec::with_capacity_in(jobs.len(), Global);
            for job in jobs.iter() {
                let clock = clock.clone();
                running.push(scope.may_overlap("engine impl tables", HEADS_FRAME, move |cx: &Cx| {
                    let began = clock.now();
                    let made = work::unwinding(|| tables_of(cx, root, packs, corrected, procs, job, subjects, heap));
                    Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                }));
            }
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(&cx) {
                    Ok(one) => worked.push(one),
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
        }
        // 3. Each result as the engine's row, its tables on the shelf, in task order.
        for (job, (made, ns)) in jobs.iter().zip(worked) {
            let task = &job.task;
            let name = task.planned.name;
            times.push(name, ns);
            let one = match made {
                Ok(Ok(one)) => one,
                // `tables_of` over the scope's records: a refusal of them is a function of the
                // graph's cell (the packs) and each scope crate's names cell.
                Ok(Err(why)) => {
                    refuse(task, why, &job.cells, buffer);
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    refuse(task, "its impl tables' task panicked; that crate's tables alone failed", &[], buffer);
                    continue;
                }
            };
            if shelf.put_framed(job.krate, &one.payload).is_err() {
                // The request's shelf, filled this request (its cap, its order): no cell, so owed.
                refuse(task, "its tables could not be put on the request's shelf (front2 buckets/records.rs `ImplShelf::put_framed`)", &[], buffer);
                continue;
            }
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len() + 1, alloc);
            defines.push((WHOLE, one.out));
            defines.extend(one.defines.iter().copied());
            defines.sort_unstable_by_key(|(cell, _)| *cell);
            defines.dedup_by_key(|(cell, _)| *cell);
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(one.reads.len() + 1, alloc);
            let own = CellId { of: job.own, cell: WHOLE };
            reads.push((own, view.cell(&own)));
            for id in one.reads.iter() {
                if id.of.kind == Kind::Signature {
                    for crate_scope in job.scope.iter() {
                        let whole = CellId { of: *crate_scope, cell: WHOLE };
                        reads.push((whole, view.cell(&whole)));
                    }
                } else {
                    // A names cell kept by `tables_of` (at cell grain) as its crate defines it.
                    let id = names_read(view, *id);
                    reads.push((id, view.cell(&id)));
                }
            }
            // Each header's head placed at its head task's cell (at cell grain, `tables_of`): a
            // head task of this phase, entered before these tables (`run_items`), is read at the
            // digest its row holds now, so the merge sends this task to no second wave for a
            // head it saw (as a head's read of a sibling head, `job_cells`).
            for id in one.heads.iter() {
                let made = buffer.done.iter().find(|done| done.name == id.of).and_then(|done| done.defines.binary_search_by_key(&id.cell, |(cell, _)| *cell).ok().map(|at| done.defines[at].1));
                reads.push((*id, made.unwrap_or_else(|| view.cell(id))));
            }
            reads.sort_unstable_by_key(|(id, _)| *id);
            reads.dedup_by_key(|(id, _)| *id);
            let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(reads.len(), Global);
            record_reads.extend(reads.iter().map(|(id, digest)| (id.of, *digest)));
            record_reads.dedup_by_key(|(of, _)| *of);
            buffer.done.push(Done { name, key: task.key, out: one.out, defines, reads, findings: 0, failed: false });
            self.made.push(Made { name, out: one.out, reads: record_reads, payload: one.payload });
        }
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }
}

/// Before a Bodies phase: the tables of each crate whose Heads task stands from the stored
/// answer (not entered this request), read back from its record in the store by the key the
/// answer keeps (`record`), so every body reads one shelf holding every crate. A crate whose
/// task failed, or that has no row, is left off: a body asking it fails naming the crate
/// (buckets/records.rs `NoTables`), never answered from less (rule 3).
pub(crate) fn complete(cx: &Cx, store: &mut dyn crate::store::Store<'_>, names: &NamesStage<'_>, shelf: &mut ImplShelf, record: &dyn Fn(Name) -> Option<Key>, view: View<'_, '_>, frame: &Frame) -> Result<u32, CliError> {
    let Some(world) = names.names.as_ref() else { return Ok(0) };
    let mut read_back = 0u32;
    for krate in world.world().crates.iter() {
        let key = krate.input.key;
        if shelf.holds(key) {
            continue;
        }
        let name = impl_tables_name(key);
        let Some(row) = view.row(name) else { continue };
        if row.failed {
            continue;
        }
        let Some(at) = record(name) else {
            return Err(CliError::Text { what: "a standing `ImplBucket` row has no record key (pages.rs `records`; engine_heads.rs `complete`)" });
        };
        let read = work::payload(cx, store, &Input { name, at, out: row.out }, frame)?;
        shelf.put_framed(key, read.payload()).map_err(|_| CliError::Text { what: "a standing crate's impl tables read back malformed from the store (engine_heads.rs `complete`)" })?;
        read_back += 1;
    }
    Ok(read_back)
}

/// One crate's tables on a worker: its scope's records opened in the task's frame, its rows filed.
/// `subjects`: `Some` at cell grain (engine_scope_reads.rs `ScopeReads::TABLES_AT_CELLS`): its
/// names reads kept as cells and each header's head read placed (`Owners::place`); `None`: each
/// names read its crate's record whole, each header read left for the coordinator to widen.
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn tables_of(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &Job<'_>,
    subjects: Option<&Subjects>,
    heap: &'static blaze_own::Heap,
) -> Result<Worked, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let opened = crate::run_names::names_records(cx, root, &job.crates, packs, corrected, procs, std::vec::Vec::new_in(Global), None, frame, g).map_err(|_| "its crate's records could not be opened (run_names.rs `names_records`)")?;
    let records = opened.records;
    let mut rows = Vec::new_in(g);
    if !NamesSource::each_impl_row(records, 0, &mut |row| rows.push(row)) {
        return Err("its crate's records hold no impl rows (front2 source.rs `each_impl_row`)");
    }
    let done = impl_tables_task(records, 0, &rows, g);
    if done.failed.is_some() {
        return Err("its impl tables were not written (front2 buckets/records.rs `impl_tables_task`: a row its writer refuses)");
    }
    if done.name != job.task.planned.name {
        return Err("its tables' crate key is not its task's (front2 buckets/records.rs `impl_tables_name`)");
    }
    let tables = done.records.as_ref().ok_or("its impl tables' task made no records")?;
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let view = Tables::new(TablesOpts { records, impls: None, lang_crates: &[], reads: &unused });
    let mut reads: std::vec::Vec<CellId, Global> = std::vec::Vec::with_capacity_in(done.reads.len(), Global);
    let mut heads: std::vec::Vec<CellId, Global> = std::vec::Vec::new_in(Global);
    let mut owners = subjects.map(Owners::new);
    for read in done.reads.iter() {
        match cell_id(view, *read) {
            // A names read: at cell grain its cell (the coordinator keeps it where its crate
            // defines a cell table, `names_read`); else its crate's names record whole, the cell
            // the Names stage defines.
            Ok(Some(id)) if id.of.kind == Kind::CrateScope => reads.push(match subjects {
                Some(_) => id,
                None => CellId { of: id.of, cell: WHOLE },
            }),
            // A header's head read: at cell grain placed as a body's is; a head of no crate the
            // records hold (or `Whole`) is left for the coordinator, which widens it.
            Ok(Some(id)) if id.of.kind == Kind::Signature => match owners.as_mut().and_then(|owners| owners.place(records, &id, g)) {
                Some(HeadAt::Cell { cell, .. }) => heads.push(cell),
                Some(HeadAt::Dependency(scope) | HeadAt::Unplaced(scope)) => heads.push(CellId { of: scope, cell: WHOLE }),
                None => reads.push(id),
            },
            Ok(Some(id)) => reads.push(id),
            Ok(None) => {}
            Err(_) => return Err("a read of its filing names a crate its scope does not hold (front2 tables/task.rs `cell_id`)"),
        }
    }
    let mut defines = std::vec::Vec::with_capacity_in(done.defines.len(), Global);
    defines.extend(done.defines.iter().copied());
    Ok(Worked { out: done.out, defines, reads, heads, payload: framed(tables, Global) })
}

// ---------------------------------------------------------------------------------------------
// The head tasks: one an outline entry that has a head (module docs, "THE HEAD TASKS").

/// Each member crate's files as its items record lists them, matched to the planner's outlines:
/// (its crate key's prefix, the record's file index, the outline's `FileItems` name), sorted.
pub(crate) type Subjects = [([u8; 8], u32, Name)];

/// The domain of a head cell ([`head_cell`]). 3: the cell holds its unit's crate prefix.
const HEAD_CELL_DOMAIN: &str = "blaze-cli engine head cell 3";
/// The domain of a head's item, of no unit ([`head_item`]).
const HEAD_ITEM_DOMAIN: &str = "blaze-cli engine head item 1";
/// The domain of a head task's source ([`plan_items`]). 2: its payload holds each run's cell
/// ([`WorkedHead::payload`]), so no record of the old form is read by its key. 3: one run a
/// (unit, cell), each frozen under its unit's records ([`HeadsStage::run_items`]).
const HEAD_SOURCE_DOMAIN: &str = "blaze-cli engine head source 3";
/// The domain of one unit's freeze of a head task: its cells and their digests, in order.
const HEAD_OUT_DOMAIN: &str = "blaze-cli engine head entry out 1";
/// The domain of a head task's out: each unit's freeze's out, in unit order.
const HEAD_UNITS_OUT_DOMAIN: &str = "blaze-cli engine head entry units out 1";

/// The out of a head task whose file no unit holds ([`HeadsStage::run_items`]): its name's alone.
const HEAD_NO_UNIT_DOMAIN: &str = "blaze-cli engine head no unit's 1";

/// The out of the head task `name` when no unit holds its file: a function of its name alone,
/// so the heads table knows such a row without a record (engine_seeds.rs `RequestTable::build`).
pub(crate) fn no_units_out(name: Name) -> ContentId {
    ContentId::of(HEAD_NO_UNIT_DOMAIN, &name.subject.0)
}
/// The tool `head_task` folds into a key the engine does not use (the engine keys the task by
/// its name and source, `Tools::key`).
const HEAD_TOOL_DOMAIN: &str = "blaze-cli engine head task";

/// The cell of a head in the task of the outline entry that covers it: by what the entry's text
/// names it (the want's kind, the item's name, an impl fn's name), never by its checker key. A
/// crate's item keys are of the unit that holds it: `b`'s view of `a` (a dependency's unit) keys
/// `a::one` apart from `a`'s own check scope, where its head task runs, so a cell by key was one
/// no task defines (lane EY, the callers test). `part`: `WHOLE`, the whole head, the only part
/// today, hashed in so a head can be split later (engine.md "ASPECTS"). Two heads of one entry
/// under one cell are refused, both named ([`heads_of`]). The cell is injective over one
/// entry's heads because an entry is ONE item of its text: its own head, or an impl fn's, or
/// an `extern` block's foreign items (one namespace's names, distinct). That rests on each
/// entry having a task name of its own (signature.rs `signature_name`, lane RU3: before it, two
/// unnamed impls of one file were one entry, their two headers one cell).
///
/// ONE CELL A UNIT (lane RU1): a member's item is held by each unit whose records list its file
/// (the lib, the test harness, a bin that `mod`s it), and its keys there hold that unit's crate
/// prefix, so a head frozen under one unit's records names that unit's keys and cannot be read
/// through another's names. The cell holds the unit: its crate prefix, the first 8 bytes of
/// the want's key (front2 tables/head.rs `want_bytes`, bytes 1 to 8; front2 keys.rs
/// `checker_of`), which a head task's freeze under a unit and a reader in that unit both take
/// from their own records' want, so a reader's want places the cell of ITS unit, by
/// construction. Nothing of the machine (rule 25): a crate prefix is a hash of the crate's text.
pub(crate) fn head_cell(want: Want, item: &str, member: Option<&str>, part: Cell) -> Cell {
    let bytes = want_bytes(want);
    let mut h = blaze_ids::Hasher::new(HEAD_CELL_DOMAIN);
    h.update(&bytes[1..9]).update(&head_item(want, item, member, part).0);
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// A head's item as an entry's text names it, of no unit: the want's kind, the item's name, an
/// impl fn's name and the part. Two units' cells of one item ([`head_cell`]) share it; the
/// profile counts the items held by two units and more by it ([`UnitCounts`]).
fn head_item(want: Want, item: &str, member: Option<&str>, part: Cell) -> Cell {
    let mut h = blaze_ids::Hasher::new(HEAD_ITEM_DOMAIN);
    h.update(&want_bytes(want)[..1]).part(item.as_bytes()).opt_part(member).update(&part.0);
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// Under which units a head task freezes its heads: the rule, or a test's plant (rule 23:
/// the plant removes the cause and the tests watch the effect come back).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeadUnits {
    /// Once for each member unit whose records hold its file (the rule, [`head_cell`]).
    Each,
    /// THE PLANT: under the first holding unit by crate prefix only (the form before lane RU1).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    OnlyFirst,
    /// THE PLANT: under the last holding unit by crate prefix only.
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    OnlyLast,
}

/// What the head tasks froze this request, by unit (the profile's "heads frozen" line, rule 53).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct UnitCounts {
    /// Cells defined: one a (unit, head), a head that failed too.
    pub(crate) frozen: u32,
    /// Heads by item, of no unit ([`head_item`]): each head task's items, summed.
    pub(crate) items: u32,
    /// Distinct units that froze a head.
    pub(crate) units: u32,
    /// Items frozen under two units or more (each a freeze a head form relative to "this crate"
    /// would share).
    pub(crate) shared: u32,
}

/// The profile's line of the head tasks' freezes by unit.
pub(crate) fn unit_profile(counts: &UnitCounts, into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let _ = writeln!(into, "heads frozen: {} for {} items in {} units ({} items held by 2+ units)", counts.frozen, counts.items, counts.units, counts.shared);
}

/// One `Signature` task an outline entry that has a head (signature.rs `is_signature_entry`), in
/// `Phase { stage: Heads, wave: 0 }`, named as the outline lists it (signature.rs
/// `signature_name`: the file and the entry's chain), so its name and key are known from text
/// before any phase runs. Its source (rule 21): the outline's head source (the entry's head bytes,
/// its containers' heads, nothing of its siblings, signature.rs `signature_source`), the item's
/// whole text when it has a body part (a frozen fn head holds its text's digest), and the edition.
pub(crate) fn plan_items<'h>(outlined: &[Outlined<'_>], alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned = SVec::new_in(alloc);
    for one in outlined.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            let (Kind::Signature, Locate::Source(head)) = (part.name.kind, part.locate) else { continue };
            let body = record.parts.iter().find_map(|other| match (other.name.kind, other.locate) {
                (Kind::BodyCheck, Locate::Source(text)) if other.place == part.place => Some(text),
                _ => None,
            });
            let mut h = blaze_ids::Hasher::new(HEAD_SOURCE_DOMAIN);
            h.update(&head.0).update(&body.unwrap_or(blaze_drive::key::ABSENT).0);
            let source = crate::engine_tasks::edition_source(h.finish(), one.edition);
            planned.push(Planned { name: part.name, phase: Phase::new(Stage::Heads, 0), source, file: one.name });
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// Where a read of a head (a `Signature` cell named by its item, front2 tables/head.rs
/// `cell_id`) is read from in this engine.
#[derive(Clone, Copy)]
pub(crate) enum HeadAt {
    /// A member crate's head: the cell of the head task of the entry that covers it, and that
    /// entry's file (its outline's `FileItems` name).
    Cell { cell: CellId, file: Name },
    /// A dependency's head (its crate defines no cell table): its crate's names record, whole
    /// (nothing of a dependency moves unless its record does).
    Dependency(Name),
    /// A member crate's head no entry of its files covers (a macro wrote it): its crate's names
    /// record whole, which holds every expansion's text.
    Unplaced(Name),
}

/// Each head of the member crates a task reads, by its item's head name, filled one crate at a
/// time on first ask, over a task's own records.
pub(crate) struct Owners<'s> {
    subjects: &'s Subjects,
    /// The crates (by key prefix) filled so far.
    crates: std::vec::Vec<[u8; 8], Global>,
    /// (head name, its entry's task, its file, its cell in that task), sorted by head name.
    rows: std::vec::Vec<(Name, Name, Name, Cell), Global>,
}

impl<'s> Owners<'s> {
    pub(crate) fn new(subjects: &'s Subjects) -> Owners<'s> {
        Owners { subjects, crates: std::vec::Vec::new_in(Global), rows: std::vec::Vec::new_in(Global) }
    }

    /// Where the head read `id` is read from; `None`: its crate is none of `records`'. By the
    /// reader's OWN key: `id`'s crate prefix is the reader's unit's view of the item's crate,
    /// so a member's head is the cell its head task froze under THAT unit ([`head_cell`]).
    pub(crate) fn place<'n>(&mut self, records: &'n Records<'n>, id: &CellId, g: Alloc<'static>) -> Option<HeadAt> {
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&id.of.subject.0[..8]);
        let (k, key) = (0..NamesSource::crate_count(records)).find_map(|k| NamesSource::crate_key(records, k).filter(|key| key.prefix() == prefix).map(|key| (k, key)))?;
        let scope = blaze_drive::key::crate_scope_name(&key);
        // A crate with no file among the members' outlines (a dependency) has no head tasks.
        // Judged by the files, not by the `CELLS` mark of the scope named here: by that mark,
        // `b`'s caller of `a::one` read `a` whole and missed `one`'s new head (lane EY,
        // engine_check_rechecks_the_callers_of_an_edited_signature).
        let from = self.subjects.partition_point(|(p, _, _)| *p < prefix);
        if self.subjects.get(from).is_none_or(|(p, _, _)| *p != prefix) {
            return Some(HeadAt::Dependency(scope));
        }
        if !self.crates.contains(&prefix) {
            self.crates.push(prefix);
            let rows = &mut self.rows;
            heads_of_crate(records, k, prefix, self.subjects, g, &mut |one| rows.push((one.sig, one.entry, one.file, one.cell)));
            rows.sort_unstable();
            rows.dedup_by_key(|(sig, _, _, _)| *sig);
        }
        Some(match self.rows.binary_search_by_key(&id.of, |(sig, _, _, _)| *sig) {
            // The only part is the whole head (`WHOLE`, front2 head.rs `cell_id`).
            Ok(at) => HeadAt::Cell { cell: CellId { of: self.rows[at].1, cell: self.rows[at].3 }, file: self.rows[at].2 },
            Err(_) => HeadAt::Unplaced(scope),
        })
    }
}

/// One head cell [`heads_of_crate`] lists.
struct HeadRow<'n> {
    /// Its item's head name in the records read (front2 head.rs `cell_id`; of their unit).
    sig: Name,
    /// The task of the outline entry that covers it.
    entry: Name,
    /// Its file's outline.
    file: Name,
    /// Its cell in `entry`'s task ([`head_cell`]; of the unit of `records`' crate).
    cell: Cell,
    /// Its item, of no unit ([`head_item`]).
    item: Cell,
    want: Want,
    /// Its item's name and an impl fn's name, as its entry's text names them (what
    /// [`head_item`] hashed): the words a refusal of two heads under one cell prints.
    name: &'n str,
    member: Option<&'n str>,
}

/// What a head's want is, in words, for a refusal that names it ([`heads_of`]).
fn want_words(want: Want) -> &'static str {
    match want {
        Want::Head(_) => "the alias",
        Want::Adt(_) => "the type",
        Want::Impl(_) => "the impl header",
        Want::Fn(_) => "the fn, const or static",
        Want::Members(_) => "the trait",
        Want::MemberFn(..) => "the impl fn",
    }
}

/// A head as its entry's text names it, for a refusal ([`heads_of`]): its want's kind in words,
/// its item's name and an impl fn's name. `want: None`: no row of the head (never, as
/// `heads_of` asks only of its own rows; said, not hidden).
struct HeadLabel<'n> {
    want: Option<Want>,
    name: &'n str,
    member: Option<&'n str>,
}

impl core::fmt::Display for HeadLabel<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let words = self.want.map_or("a head with no row", want_words);
        match self.member {
            Some(member) => write!(f, "{words} `{}::{member}`", self.name),
            None => write!(f, "{words} `{}`", self.name),
        }
    }
}

/// Every head cell of crate `k` of `records` whose item is written in a file of `subjects`, in
/// file order. An item a macro wrote (its text an expansion's) or one no entry covers is not
/// listed ([`HeadAt::Unplaced`]).
fn heads_of_crate<'n>(records: &'n Records<'n>, k: usize, prefix: [u8; 8], subjects: &Subjects, g: Alloc<'static>, each: &mut dyn FnMut(HeadRow<'n>)) {
    let mut keys: Vec<blaze_id::ItemKey, Alloc<'static>> = Vec::new_in(g);
    NamesSource::each_item(records, k, &mut |key, _| keys.push(key));
    // (file, item index, want, the item's name, an impl fn's name).
    let mut placed: std::vec::Vec<(u32, u32, Want, &'n str, Option<&'n str>), Global> = std::vec::Vec::new_in(Global);
    for key in keys.iter().copied() {
        let Ok(want) = by_origin::kind_of_lib(records, key).and_then(|kind| by_origin::item_want(records, kind)) else { continue };
        let Some(place) = NamesSource::place_of(records, key) else { continue };
        let At::File(file) = place.at else { continue };
        let item = NamesSource::name_of(records, key).unwrap_or("");
        placed.push((file, place.item.0, want, item, None));
        if let Want::Impl(lib) = want {
            NamesSource::each_member(records, lib, &mut |m| {
                if let (blaze_surface::ItemKind::Function, None, Some(name)) = (m.kind, m.written_by, m.name) {
                    placed.push((file, m.item.0, Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, name)), item, Some(name)));
                }
            });
        }
    }
    placed.sort_unstable_by_key(|(file, item, want, _, _)| (*file, *item, want_bytes(*want)));
    let edition = NamesSource::edition_of(records, k);
    for run in placed.chunk_by(|a, b| a.0 == b.0) {
        let file = run[0].0;
        let Ok(at) = subjects.binary_search_by(|(p, f, _)| (*p, *f).cmp(&(prefix, file))) else { continue };
        let outline_name = subjects[at].2;
        let (Some(text), Some(edition)) = (NamesSource::file_text(records, k, file), edition) else { continue };
        let outline = blaze_file_items::outline(text, edition, g);
        for (_, item, want, name, member) in run.iter() {
            let Some(cover) = crate::signature::covering_entry(&outline, ItemIndex(*item)) else { continue };
            let entry = crate::signature::signature_name(outline_name.subject, &outline, outline.item(cover), text);
            each(HeadRow {
                sig: head_cell_id(*want).of,
                entry,
                file: outline_name,
                cell: head_cell(*want, name, *member, WHOLE),
                item: head_item(*want, name, *member, WHOLE),
                want: *want,
                name: *name,
                member: *member,
            });
        }
    }
}

/// A names read as its crate's task defines it (engine_bodies.rs `widen`'s rule): a cell of a
/// crate with a cell table stays its cell (its lang rows one of them); any other its record whole.
fn names_read(view: View<'_, '_>, id: CellId) -> CellId {
    match id.of.kind {
        Kind::CrateScope if id.cell != WHOLE && view.cell(&CellId { of: id.of, cell: CELLS }) != blaze_drive::key::ABSENT => id,
        Kind::CrateScope => CellId { of: id.of, cell: WHOLE },
        _ => id,
    }
}

/// Every member crate's files as its items record lists them, matched to the planner's outlines
/// ([`Subjects`], sorted): what [`Owners::place`] places an `ImplBucket` task's header read by,
/// at cell grain ([`HeadsStage::run_phase`]). The same match `run_items` and engine_bodies.rs
/// `groups` make in their member loops (to be this one home once lane RU1's per-unit heads land
/// there). A member whose records do not open lists no file: its heads are then placed as a
/// dependency's (its record whole), never missed.
fn member_subjects<'r>(
    read: &mut EngineRecords<'r>,
    cx: &Cx,
    store: &mut dyn crate::store::Store<'r>,
    names: &NamesStage<'_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    outlined: &[Outlined<'_>],
    heap: &'static blaze_own::Heap,
) -> std::vec::Vec<([u8; 8], u32, Name), Global> {
    let g = heap.alloc();
    let mut subjects: std::vec::Vec<([u8; 8], u32, Name), Global> = std::vec::Vec::new_in(Global);
    for member in (0..CrateGraph::len(list)).filter(|at| list.member(*at)) {
        // A member whose scope is refused lists no file here (its heads placed as a
        // dependency's, coarser, never missed); its typed cause is printed by the refusal of
        // that member's own head tasks and bodies ([`HeadsStage::run_items`], engine_bodies.rs
        // `groups`), which call `scope_of` for it the same way: nothing is dropped unsaid.
        let Ok((scope, _)) = crate::engine_bodies::scope_of(read, cx, store, names, list, member, heap) else { continue };
        let Ok(items) = ItemsRecord::open(scope.own_records.items, g) else { continue };
        let prefix = list.key(member).prefix();
        for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
            let Ok(Some(row)) = items.file(file) else { continue };
            let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
            let Some(path) = path.strip_prefix("/0/") else { continue };
            if let Some(one) = outlined.iter().find(|one| one.path.as_slice() == path.as_bytes()) {
                subjects.push((prefix, file, one.name));
            }
        }
    }
    subjects.sort_unstable();
    subjects
}

/// One member crate's head tasks, made on one worker over its check scope's records (in place
/// in the store's mapping, engine_records.rs).
struct ItemJob<'r> {
    prefix: [u8; 8],
    crates: Vec<Crate<'r>, Global>,
    lang: Vec<usize, Global>,
    /// Every crate of its scope had records (else its tasks are refused by name).
    whole: bool,
    /// When not `whole`: the cut's cause in words, the crate named (engine_bodies.rs
    /// `Scope::cut_cause`), what its tasks' refusal prints.
    cut_words: Option<&'static str>,
    /// The cells a refusal of its tasks is a function of ([`scope_cells`]): at 0 the graph's,
    /// at 1 the member's own names cell.
    cells: std::vec::Vec<CellId, Global>,
    /// Its tasks, sorted by name.
    tasks: std::vec::Vec<Task, Global>,
}

/// One head task as a worker made it.
struct WorkedHead {
    out: ContentId,
    defines: std::vec::Vec<(Cell, ContentId), Global>,
    /// Each cell's item ([`head_item`]) and its unit's crate prefix, as `defines` lists them:
    /// what the profile counts by. Over the handed heap's `Sync` handle: it crosses back from
    /// the worker.
    items: SVec<'static, (Cell, [u8; 8])>,
    reads: std::vec::Vec<CellId, Global>,
    /// Each cell's want (front2 head.rs `want_bytes`), its cell ([`head_cell`], 16 bytes), its
    /// frozen bytes' length (u32 LE) and the frozen bytes, in cell order.
    payload: std::vec::Vec<u8, Global>,
    /// Each head it could not freeze: its cell, the head, why (engine_head_notes.rs).
    failed: std::vec::Vec<(Cell, &'static str, &'static str), Global>,
}

/// The domain of a head's cell digest when it could not be frozen: of its reason, so a reader
/// moves when the reason does, and when the head is frozen at last.
const HEAD_FAILED_DOMAIN: &str = "blaze-cli engine heads: a head not frozen";

/// A head task that could not freeze a head: why (rule 19), and what it read until then.
struct HeadFailed {
    why: &'static str,
    reads: std::vec::Vec<CellId, Global>,
}

/// One head task's freezes under each unit that holds its file, gathered in unit order (lane
/// RU1, [`head_cell`]): its row is their union, each unit's cells its own. Allocates through
/// the phase's handed `alloc` only; the first unit's [`WorkedHead`] holds the union of the
/// rest (its vectors are the worker's).
struct UnitsMade<'a> {
    alloc: SharedAlloc<'a>,
    /// Its share of each unit's worker time.
    ns: u64,
    /// The first unit's failure, the cells every failing unit's refusal is a function of, and
    /// whether one is owed (a panic): the task is refused whole.
    failed: Option<(&'static str, SVec<'a, CellId>, bool)>,
    /// Each unit's crate prefix and out, in unit order ([`HEAD_UNITS_OUT_DOMAIN`]).
    out: blaze_ids::Hasher,
    /// What every unit read, and the refusal cells of a holding unit that was not whole.
    reads: SVec<'a, CellId>,
    /// The first unit's freeze, the later units' defines, items, runs and failed heads
    /// appended in unit order ([`WorkedHead::payload`]: one run a (unit, cell)).
    first: Option<WorkedHead>,
}

impl<'a> UnitsMade<'a> {
    fn new(alloc: SharedAlloc<'a>) -> UnitsMade<'a> {
        UnitsMade { alloc, ns: 0, failed: None, out: blaze_ids::Hasher::new(HEAD_UNITS_OUT_DOMAIN), reads: SVec::new_in(alloc), first: None }
    }

    /// Unit `prefix`'s freeze of the task (its items carry `prefix` already, `heads_of`).
    fn take(&mut self, prefix: [u8; 8], one: WorkedHead) {
        self.out.update(&prefix).update(&one.out.0);
        self.reads.extend(one.reads.iter().copied());
        match &mut self.first {
            None => self.first = Some(one),
            Some(all) => {
                all.defines.extend(one.defines.iter().copied());
                all.items.extend(one.items.iter().copied());
                all.payload.extend_from_slice(&one.payload);
                all.failed.extend(one.failed.iter().copied());
            }
        }
    }

    /// A unit could not freeze the task: why, and the cells that is a function of.
    fn fail(&mut self, why: &'static str, cells: &[CellId], owed: bool) {
        match &mut self.failed {
            Some((_, all, was_owed)) => {
                all.extend_from_slice(cells);
                *was_owed |= owed;
            }
            None => {
                let mut all = SVec::with_capacity_in(cells.len(), self.alloc);
                all.extend_from_slice(cells);
                self.failed = Some((why, all, owed));
            }
        }
    }
}

impl HeadsStage {
    /// The head tasks of one Heads phase (module docs, "THE HEAD TASKS"): each member crate's
    /// tasks on one worker, each result its row in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    fn run_items<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        kept: &mut Kept,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &Frame,
    ) -> Result<(), CliError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let heap = handles.env.heap;
        let g = heap.alloc();
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each as the view holds it today (ABSENT
        // where no task defines it), through `failed_with` (sorted, once each): its row stands
        // until one moves (plan row A8). None: owed, said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            // The whole task failed: named by the file its entry is in.
            let file = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).ok().and_then(|at| core::str::from_utf8(&handles.outlined[at].path).ok());
            let head = match file {
                Some(path) => crate::engine_head_notes::leak(g, format_args!("a head of `{path}`")),
                None => "a head of a file the outlines do not hold",
            };
            refused.push(crate::engine_head_notes::Refused { name: task.planned.name, cell: None, head, why });
        };
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            // No `UnitGraph` task planned, or no names stage: no cell of this command says so,
            // so owed until the plan's inputs are cells (plan row D2).
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no head", &[], buffer);
            }
            return Ok(());
        };
        // The crate list, each member's check scope and the packs are the graph record's.
        let graph_cell = CellId { of: graph.name, cell: WHOLE };
        let Some(list) = kept.list(cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", &[graph_cell], buffer);
            }
            return Ok(());
        };
        let (packs, corrected) = match kept.trees(cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => {
                // The machine's packs (body_types.rs `open_trees`), not a cell: owed (rule 3).
                for task in tasks.iter() {
                    refuse(task, why, &[], buffer);
                }
                return Ok(());
            }
        };
        // 1. Every member's files and check scope, on this thread: each scope crate's records
        // read in place through `read`, opened and checked once a request however many members'
        // scopes hold it (engine_records.rs), and the workers below handed shared borrows of them.
        let mut subjects: std::vec::Vec<([u8; 8], u32, Name), Global> = std::vec::Vec::new_in(Global);
        let mut jobs: std::vec::Vec<ItemJob<'r>, Global> = std::vec::Vec::new_in(Global);
        // What "its file is in no member crate's items record" is a function of: the graph's
        // cell (which crates are members, their roots) and every member's names cell (its items
        // record; ABSENT for a member whose names task failed, so its fix moves the row).
        let mut no_member: std::vec::Vec<CellId, Global> = std::vec::Vec::new_in(Global);
        no_member.push(graph_cell);
        // Every member's items record was read: then a file none lists is no unit's (a `cfg` on
        // its `mod` cut it in every unit, engine_body_plan.rs), not a refusal.
        let mut every_member_read = true;
        // Why the first member whose records could not be read was not (its typed cause, the
        // crate named) and how many were not: what a task no unit holds then prints (rule 19).
        let mut unread: Option<&'static str> = None;
        let mut unread_count = 0u32;
        // Each member unit's cfg-blind walk of its module tree (engine_body_plan.rs `reached`,
        // the planner's walk): with its names task and crate prefix. A unit whose walk reaches a
        // task's file but whose items record does not list it (a `#[cfg(test)]` on its `mod`)
        // is a unit the task's heads are NOT frozen under, and that is a fact of the unit's
        // records: the row reads the unit's not-held cell (engine_body_plan.rs `not_held_cell`,
        // the one home Bodies reads too), so a cfg taken off enters the task again (rule 21).
        let mut walks: Vec<(Name, [u8; 8], Vec<u32, Alloc<'static>>), Alloc<'static>> = Vec::new_in(g);
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            let root = list.root(member);
            let reach = crate::engine_body_plan::reached(handles.outlined, Name::file(root.strip_prefix("/0/").unwrap_or(root).as_bytes()), g);
            walks.push((blaze_drive::key::crate_scope_name(&list.key(member)), list.key(member).prefix(), reach));
            no_member.push(CellId { of: blaze_drive::key::crate_scope_name(&list.key(member)), cell: WHOLE });
            let (scope, whole) = match crate::engine_bodies::scope_of(read, cx, store, names, &list, member, heap) {
                Ok(made) => made,
                Err(cause) => {
                    every_member_read = false;
                    unread_count = unread_count.saturating_add(1);
                    unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("its check scope was not read: {cause}")));
                    continue;
                }
            };
            let cells = scope_cells(graph.name, &list, member, Some(&scope));
            let cut_words = scope.cut_cause(&list).map(|cause| crate::engine_head_notes::leak(g, format_args!("its crate's {cause}")));
            let Ok(items) = ItemsRecord::open(scope.own_records.items, g) else {
                every_member_read = false;
                unread_count = unread_count.saturating_add(1);
                unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("the items record of crate `{}` does not open (blaze-scope items_record.rs `ItemsRecord::open`)", CrateGraph::name(&list, member))));
                continue;
            };
            let prefix = list.key(member).prefix();
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(one) = handles.outlined.iter().find(|one| one.path.as_slice() == path.as_bytes()) {
                    subjects.push((prefix, file, one.name));
                }
            }
            jobs.push(ItemJob { prefix, crates: scope.crates, lang: scope.lang, whole, cut_words, cells, tasks: std::vec::Vec::new_in(Global) });
        }
        subjects.sort_unstable();
        jobs.sort_unstable_by_key(|job| job.prefix);
        // Each task to EVERY member unit (by prefix) whose files hold its file (lane RU1,
        // `head_cell`): each freezes the task's heads under its own records, one cell a unit.
        let mut by_file: std::vec::Vec<(Name, [u8; 8]), Global> = std::vec::Vec::with_capacity_in(subjects.len(), Global);
        by_file.extend(subjects.iter().map(|(prefix, _, file)| (*file, *prefix)));
        by_file.sort_unstable();
        by_file.dedup();
        // Each task that some unit runs, sorted by name, and what its units made (step 3).
        let mut accs: SVec<'a, (Task, UnitsMade<'a>)> = SVec::with_capacity_in(tasks.len(), alloc);
        let mut holding: SVec<'a, usize> = SVec::new_in(alloc);
        for task in tasks.iter() {
            let at = by_file.partition_point(|(file, _)| *file < task.planned.file);
            holding.clear();
            holding.extend(by_file[at..].iter().take_while(|(file, _)| *file == task.planned.file).filter_map(|(_, prefix)| jobs.binary_search_by_key(prefix, |job| job.prefix).ok()));
            // The plant (`HeadUnits`): one unit alone, as before the rule.
            match self.freeze {
                HeadUnits::Each => {}
                HeadUnits::OnlyFirst => holding.truncate(1),
                HeadUnits::OnlyLast => {
                    if let Some(last) = holding.last().copied() {
                        holding.clear();
                        holding.push(last);
                    }
                }
            }
            let whole = holding.iter().copied().find(|at| jobs[*at].whole);
            match (holding.first().copied(), whole) {
                // No unit holds its file under its cfg: no unit's head, a row with no finding on
                // the cells that say so (each member's names cell, the graph's), so a unit that
                // comes to hold it enters it again (engine_body_plan.rs, as its bodies).
                (None, _) if every_member_read => {
                    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(no_member.len(), alloc);
                    reads.extend(no_member.iter().map(|id| (*id, view.cell(id))));
                    reads.sort_unstable_by_key(|(id, _)| *id);
                    reads.dedup_by_key(|(id, _)| *id);
                    let out = no_units_out(task.planned.name);
                    buffer.done.push(Done { name: task.planned.name, key: task.key, out, defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                }
                (None, _) => {
                    let why = crate::engine_head_notes::leak(
                        g,
                        format_args!(
                            "its file is in no member crate's items record, and {unread_count} member(s)' records could not be read, the first because {} (engine_heads.rs `run_items`)",
                            unread.unwrap_or("of no cause kept (engine_heads.rs `run_items` keeps the first)")
                        ),
                    );
                    refuse(task, why, &no_member, buffer);
                }
                // Its member's names cell, each scope crate's that had records, and the one the
                // scope was cut at, read ABSENT: its task that comes, or its fix, moves the row.
                // The cut's cause printed, the crate named (rule 19).
                (Some(at), None) => {
                    let why = jobs[at].cut_words.unwrap_or("its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)");
                    refuse(task, why, &jobs[at].cells, buffer);
                }
                // Frozen under each whole unit; a unit whose scope lacks a names record defines
                // no cell (its readers read it ABSENT), and the row reads that unit's refusal
                // cells, so the fix that makes it whole enters the task again (rule 21).
                (Some(_), Some(_)) => {
                    let mut made = UnitsMade::new(alloc);
                    for at in holding.iter().copied() {
                        match jobs[at].whole {
                            true => jobs[at].tasks.push(*task),
                            false => made.reads.extend(jobs[at].cells.iter().copied()),
                        }
                    }
                    // The units its file is NOT frozen under though their walk reaches it: their
                    // not-held cells, so one that comes to hold the file moves the row.
                    let held = &by_file[at..];
                    let held = &held[..held.partition_point(|(file, _)| *file == task.planned.file)];
                    if let Ok(file_at) = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)) {
                        let file_at = u32::try_from(file_at).unwrap_or(u32::MAX);
                        for (unit, prefix, reach) in walks.iter() {
                            if reach.binary_search(&file_at).is_ok() && !held.iter().any(|(_, one)| one == prefix) {
                                made.reads.push(crate::engine_body_plan::not_held_cell(view, *unit, *prefix));
                            }
                        }
                    }
                    accs.push((*task, made));
                }
            }
        }
        accs.sort_unstable_by_key(|(task, _)| task.planned.name);
        accs.dedup_by_key(|(task, _)| task.planned.name);
        jobs.retain(|job| !job.tasks.is_empty());
        // 2. Each member's heads, at once, on the workers.
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let clock = handles.clock;
        let subjects = &subjects[..];
        type Ran = Result<Result<std::vec::Vec<Result<WorkedHead, HeadFailed>, Global>, &'static str>, work::Panicked>;
        let mut worked: std::vec::Vec<(Ran, u64), Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = std::vec::Vec::with_capacity_in(jobs.len(), Global);
            for job in jobs.iter() {
                let clock = clock.clone();
                running.push(scope.may_overlap("engine heads", HEADS_FRAME, move |cx: &Cx| {
                    let began = clock.now();
                    let made = work::unwinding(|| heads_of(cx, root, packs, corrected, procs, job, subjects, view, heap));
                    Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                }));
            }
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(cx) {
                    Ok(one) => worked.push(one),
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
        }
        // 3. Each unit's results gathered by task (units in prefix order), then each task as the
        // engine's row, in task order.
        let acc = |accs: &[(Task, UnitsMade<'a>)], task: &Task| -> Option<usize> { accs.binary_search_by_key(&task.planned.name, |(one, _)| one.planned.name).ok() };
        for (job, (ran, ns)) in jobs.iter().zip(worked) {
            let each = u64::try_from(job.tasks.len()).unwrap_or(1).max(1);
            for task in job.tasks.iter() {
                if let Some(at) = acc(&accs, task) {
                    accs[at].1.ns = accs[at].1.ns.saturating_add(ns / each);
                }
            }
            let made = match ran {
                Ok(Ok(made)) => made,
                // The scope's records not opened (`heads_of`): a function of the graph's cell
                // (the packs) and each scope crate's names cell.
                Ok(Err(why)) => {
                    for task in job.tasks.iter() {
                        if let Some(at) = acc(&accs, task) {
                            accs[at].1.fail(why, &job.cells, false);
                        }
                    }
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    for task in job.tasks.iter() {
                        if let Some(at) = acc(&accs, task) {
                            accs[at].1.fail("its crate's head tasks panicked; that crate's heads alone failed", &[], true);
                        }
                    }
                    continue;
                }
            };
            for (task, one) in job.tasks.iter().zip(made) {
                let Some(at) = acc(&accs, task) else { continue };
                match one {
                    Ok(one) => accs[at].1.take(job.prefix, one),
                    // What its freeze read until it failed, and its member's names cell: its
                    // heads' rows are that crate's records' (`heads_of_crate`), so a refusal
                    // that read nothing else (two heads under one cell) stands on it too.
                    Err(mut failed) => {
                        failed.reads.push(job.cells[1]);
                        accs[at].1.fail(failed.why, &failed.reads, false);
                    }
                }
            }
        }
        let mut heads_failed: std::vec::Vec<crate::engine_head_notes::Refused, Global> = std::vec::Vec::new_in(Global);
        let mut counts = UnitCounts::default();
        let mut units: SVec<'a, [u8; 8]> = SVec::new_in(alloc);
        // The cells this phase's head tasks define, every unit's (a refused task defines none): a
        // head's read of another head of this phase is recorded at today's digest (lane BH1),
        // so the merge never sends it to a second wave for a cell it saw (blaze-engine
        // engine.rs `readers_of`). Since heads are frozen per unit (lane RU1) a task's cells
        // come from several jobs, so this is one table for the phase, not one a job.
        let today = crate::engine_scope_reads::job_cells(accs.iter().map(|(task, made)| (task.planned.name, made.first.as_ref().filter(|_| made.failed.is_none()).map(|one| &one.defines[..]))));
        for (task, made) in accs.into_iter() {
            times.push(task.planned.name, made.ns);
            // One unit's failure fails the task whole: its row is refused on the union of what
            // each failing unit read (owed when one panicked).
            if let Some((why, cells, owed)) = made.failed {
                let cells: &[CellId] = if owed { &[] } else { &cells };
                refuse(&task, why, cells, buffer);
                continue;
            }
            // Each whole holding unit ran it and none failed, so one froze it (`heads_of` makes
            // one result a task); none is a bug of this function, said by name (rule 3).
            let Some(one) = made.first else {
                refuse(&task, "no unit that holds its file froze it, and none failed (engine_heads.rs `run_items`)", &made.reads, buffer);
                continue;
            };
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len(), alloc);
            defines.extend(one.defines.iter().copied());
            defines.sort_unstable_by_key(|(cell, _)| *cell);
            defines.dedup_by_key(|(cell, _)| *cell);
            // Each unit's reads, each cell once (rule 21: the row moves when any unit's do).
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(made.reads.len(), alloc);
            reads.extend(made.reads.iter().map(|id| (*id, crate::engine_scope_reads::today(&today, id).unwrap_or_else(|| view.cell(id)))));
            reads.sort_unstable_by_key(|(id, _)| *id);
            reads.dedup_by_key(|(id, _)| *id);
            let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(reads.len(), Global);
            record_reads.extend(reads.iter().map(|(id, digest)| (id.of, *digest)));
            record_reads.dedup_by_key(|(of, _)| *of);
            let name = task.planned.name;
            // Each head it could not freeze, under each unit: a head note a cell (a reader of
            // either unit's cell quotes its own), one finding a head by path and reason.
            heads_failed.extend(one.failed.iter().map(|(cell, head, why)| crate::engine_head_notes::Refused { name, cell: Some(*cell), head: *head, why: *why }));
            let mut said: SVec<'a, (&'static str, &'static str)> = SVec::with_capacity_in(one.failed.len(), alloc);
            said.extend(one.failed.iter().map(|(_, head, why)| (*head, *why)));
            said.sort_unstable();
            said.dedup();
            let findings = u32::try_from(said.len()).unwrap_or(u32::MAX);
            // The counts: each (item, unit) once; an item frozen under 2+ units counted shared.
            let mut items = one.items;
            items.sort_unstable();
            items.dedup();
            counts.frozen = counts.frozen.saturating_add(u32::try_from(items.len()).unwrap_or(u32::MAX));
            for run in items.chunk_by(|a, b| a.0 == b.0) {
                counts.items = counts.items.saturating_add(1);
                if run.len() > 1 {
                    counts.shared = counts.shared.saturating_add(1);
                }
            }
            units.extend(items.iter().map(|(_, unit)| *unit));
            let out = made.out.finish();
            buffer.done.push(Done { name, key: task.key, out, defines, reads, findings, failed: false });
            self.made.push(Made { name, out, reads: record_reads, payload: one.payload });
        }
        units.sort_unstable();
        units.dedup();
        counts.units = u32::try_from(units.len()).unwrap_or(u32::MAX);
        // Summed over the request's Heads phases (the units: the most one phase froze under).
        self.units = UnitCounts {
            frozen: self.units.frozen.saturating_add(counts.frozen),
            items: self.units.items.saturating_add(counts.items),
            units: self.units.units.max(counts.units),
            shared: self.units.shared.saturating_add(counts.shared),
        };
        self.refused.extend(heads_failed);
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }
}

/// One member crate's head tasks on a worker: its scope's records opened in the task's frame,
/// each task's heads frozen (front2 tables/head.rs `head_task`), each over a source of its own
/// so its read list is its own: every names cell its freeze consulted (the recorder), and for
/// each head its freeze read, the cell of the head task that covers it (a same-stage read: the
/// engine enters the reader at the wave after its definer, engine.rs), or its crate's names
/// record.
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn heads_of(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &ItemJob<'_>,
    subjects: &Subjects,
    view: View<'_, '_>,
    heap: &'static blaze_own::Heap,
) -> Result<std::vec::Vec<Result<WorkedHead, HeadFailed>, Global>, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let opened = crate::run_names::names_records(cx, root, &job.crates, packs, corrected, procs, std::vec::Vec::new_in(Global), None, frame, g).map_err(|_| "its crate's records could not be opened (run_names.rs `names_records`)")?;
    let records = opened.records;
    let mut lang: Vec<usize, Alloc<'static>> = Vec::with_capacity_in(job.lang.len(), g);
    lang.extend_from_slice(&job.lang);
    let lang: &'static [usize] = lang.leak();
    let mut sized = None;
    for at in lang.iter() {
        NamesSource::each_lang(records, *at, &mut |name, key| {
            if name == "sized" {
                sized = Some(key);
            }
        });
    }
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let base = Tables::new(TablesOpts { records, impls: None, lang_crates: lang, reads: &unused });
    // The constant evaluator's lang index, once a job; what it read is read by every head that
    // evaluates (engine_head_consts.rs).
    let lang_recorder = Recorder::new(RecorderOpts { alloc: g });
    let index = crate::engine_head_consts::lang_index(base, &lang_recorder, lang).map_err(|_| "two crates of its scope declare one lang item, so its constants have no index (front2 index.rs `LangTwice`)")?;
    let mut lang_reads: std::vec::Vec<CellId, Global> = std::vec::Vec::new_in(Global);
    for one in lang_recorder.finish().map_err(|_| "its lang index read more names cells than the recorder holds (front2 tables/recorder.rs)")?.iter() {
        if let Some(id) = cell_id(base, *one).map_err(|_| "a names read of its lang index names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")? {
            lang_reads.push(names_read(view, id));
        }
    }
    // (entry, its cell, head name, want, its item) of the crate at 0 (this job's unit), sorted.
    let mut rows: std::vec::Vec<(Name, Cell, Name, Want, Cell), Global> = std::vec::Vec::new_in(Global);
    // Each row's words (want, item name, impl fn name) by the same (entry, cell, head name):
    // what a refusal of two heads under one cell names them by.
    let mut labels: Vec<(Name, Cell, Name, Want, &str, Option<&str>), Alloc<'static>> = Vec::new_in(g);
    heads_of_crate(records, 0, job.prefix, subjects, g, &mut |one| {
        rows.push((one.entry, one.cell, one.sig, one.want, one.item));
        labels.push((one.entry, one.cell, one.sig, one.want, one.name, one.member));
    });
    rows.sort_unstable_by_key(|(entry, cell, sig, _, _)| (*entry, *cell, *sig));
    rows.dedup_by_key(|(entry, cell, sig, _, _)| (*entry, *cell, *sig));
    labels.sort_unstable_by_key(|(entry, cell, sig, _, _, _)| (*entry, *cell, *sig));
    labels.dedup_by_key(|(entry, cell, sig, _, _, _)| (*entry, *cell, *sig));
    let label = |entry: Name, cell: Cell, sig: Name| match labels.binary_search_by_key(&(entry, cell, sig), |(e, c, s, _, _, _)| (*e, *c, *s)) {
        Ok(at) => HeadLabel { want: Some(labels[at].3), name: labels[at].4, member: labels[at].5 },
        Err(_) => HeadLabel { want: None, name: "", member: None },
    };
    let mut owners = Owners::new(subjects);
    let tool = blaze_drive::key::Tool(ContentId::of(HEAD_TOOL_DOMAIN, b""));
    let mut done = std::vec::Vec::with_capacity_in(job.tasks.len(), Global);
    for task in job.tasks.iter() {
        let name = task.planned.name;
        let from = rows.partition_point(|(entry, _, _, _, _)| *entry < name);
        let mut out = blaze_ids::Hasher::new(HEAD_OUT_DOMAIN);
        let mut worked = WorkedHead { out: ContentId([0; 32]), defines: std::vec::Vec::new_in(Global), items: SVec::new_in(heap.shared()), reads: std::vec::Vec::new_in(Global), payload: std::vec::Vec::new_in(Global), failed: std::vec::Vec::new_in(Global) };
        let mut failed: Option<&'static str> = None;
        let mine = &rows[from..from + rows[from..].iter().take_while(|(entry, _, _, _, _)| *entry == name).count()];
        // Two heads under one cell would be two values under one key (rule 21): the whole
        // entry is refused, the two heads named as its text names them.
        if let Some(two) = mine.windows(2).find(|two| two[0].1 == two[1].1) {
            let (one, other) = (label(two[0].0, two[0].1, two[0].2), label(two[1].0, two[1].1, two[1].2));
            failed = Some(crate::engine_head_notes::leak(g, format_args!("two heads of its entry fall under one cell: {one} and {other}, the same kind and name (engine_heads.rs `head_cell`)")));
        }
        let mine = if failed.is_some() { &mine[..0] } else { mine };
        for (_, cell, _, want, item) in mine.iter() {
            worked.items.push((*item, job.prefix));
            let recorder = Recorder::new(RecorderOpts { alloc: g });
            let names = base.reading_into(&recorder);
            // The evaluator reads through a recorder of its own (engine_head_consts.rs).
            let evaluated = Recorder::new(RecorderOpts { alloc: g });
            let mut consts = crate::engine_head_consts::HeadConsts::new(base.reading_into(&evaluated), &index, g);
            let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, frame, scratch: g, alloc: g, consts: Some(&mut consts as &mut dyn ConstValues<'_>), known: None });
            let made = head_task(names, &mut source, *want, tool, g);
            // Every head its freeze read, through the heads they read in turn: a head's bytes
            // can hold what a head it read was made from (an alias's target), so each is read;
            // and each constant whose value a cell it read holds.
            let mut seen: std::vec::Vec<Want, Global> = std::vec::Vec::new_in(Global);
            let mut next: std::vec::Vec<Want, Global> = std::vec::Vec::new_in(Global);
            let mut values: std::vec::Vec<blaze_id::ItemKey, Global> = std::vec::Vec::new_in(Global);
            next.push(*want);
            while let Some(w) = next.pop() {
                if seen.contains(&w) {
                    continue;
                }
                if seen.len() >= MOST_HEAD_READS {
                    failed.get_or_insert("its head's freeze read more heads than a head task follows (engine_heads.rs `MOST_HEAD_READS`)");
                    break;
                }
                seen.push(w);
                if let Ok((_, read)) = source.cell(w) {
                    next.extend(read.heads.iter().copied());
                    values.extend(read.consts.iter().map(|(c, _)| *c));
                }
            }
            drop(source);
            // What evaluating its constants read (engine_head_consts.rs, module docs).
            if consts.asked > 0 {
                worked.reads.extend(lang_reads.iter().copied());
                match evaluated.finish() {
                    Ok(read) => {
                        for one in read.iter() {
                            match cell_id(base, *one) {
                                Ok(Some(id)) => worked.reads.push(names_read(view, id)),
                                Ok(None) => {}
                                Err(_) => {
                                    failed.get_or_insert("a names read of its constants' evaluation names a crate its scope does not hold (front2 tables/task.rs `cell_id`)");
                                }
                            }
                        }
                        if let Err(unplaced) = crate::engine_head_consts::eval_reads(records, subjects, &read, &mut |id| worked.reads.push(id)) {
                            let shown = crate::engine_head_consts::UnplacedIn { unplaced: &unplaced, crates: &job.crates[..] };
                            failed.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("its constants' evaluation read a text no crate of its scope holds: {shown}")));
                        }
                    }
                    Err(_) => {
                        failed.get_or_insert("its constants' evaluation read more names cells than the recorder holds (front2 tables/recorder.rs)");
                    }
                }
            }
            values.sort_unstable();
            values.dedup();
            for c in values.iter() {
                match crate::engine_head_consts::text_cell(records, subjects, *c) {
                    Ok(Some(id)) => worked.reads.push(id),
                    // A builtin type: no text, no cell (`text_cell`).
                    Ok(None) => {}
                    Err(unplaced) => {
                        let shown = crate::engine_head_consts::UnplacedIn { unplaced: &unplaced, crates: &job.crates[..] };
                        failed.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("a constant its head holds is of no crate of its scope: {shown}")));
                    }
                }
            }
            for w in seen.iter().filter(|w| **w != *want) {
                let id = head_cell_id(*w);
                // Another entry's head of this crate (`rows`): its head task's cell, never its
                // file's `HEADS` digest, which every sibling head added moves (lane BH1). Made in
                // this job, it is recorded at the digest the job made (`job_cells`), so the merge
                // never sends it to a second wave; standing, at the view's, today's unless its own
                // reads move, and then the wave rule enters this one after it. A head of this task's own entry is
                // its own work, not a read. Another member's head is made by another job of the
                // same phase: still its file's `HEADS` cell (an earlier phase's), so a fresh run
                // enters no head twice (COARSE, lane BH1's census).
                worked.reads.push(match owners.place(records, &id, g) {
                    Some(HeadAt::Cell { cell, .. }) if cell.of == name => continue,
                    Some(HeadAt::Cell { cell, .. }) if rows.binary_search_by(|(entry, _, _, _, _)| entry.cmp(&cell.of)).is_ok() => cell,
                    Some(HeadAt::Cell { file, .. }) => CellId { of: file, cell: crate::engine_tasks::HEADS },
                    Some(HeadAt::Dependency(scope) | HeadAt::Unplaced(scope)) => CellId { of: scope, cell: WHOLE },
                    None => id,
                });
            }
            match recorder.finish() {
                Ok(read) => {
                    for one in read.iter() {
                        match cell_id(base, *one) {
                            Ok(Some(id)) => worked.reads.push(names_read(view, id)),
                            Ok(None) => {}
                            Err(_) => {
                                failed.get_or_insert("a names read of its head names a crate its scope does not hold (front2 tables/task.rs `cell_id`)");
                            }
                        }
                    }
                }
                Err(_) => {
                    failed.get_or_insert("its head read more names cells than the recorder holds (front2 tables/recorder.rs)");
                }
            }
            let cell = *cell;
            // A head not frozen fails alone (module docs): its cell the digest of its reason.
            if let Some(error) = &made.failed {
                let head = crate::engine_head_consts::head_label(records, *want, g);
                let why = crate::engine_head_notes::leak(g, format_args!("{}", crate::engine_head_notes::Reason(error)));
                let digest = ContentId::of(HEAD_FAILED_DOMAIN, why.as_bytes());
                worked.defines.push((cell, digest));
                out.update(&cell.0).update(&digest.0);
                worked.failed.push((cell, head, why));
                continue;
            }
            worked.defines.push((cell, made.out));
            out.update(&cell.0).update(&made.out.0);
            // The cell too: a body finds a member's head by it (engine_seeds.rs `StretchHeads`).
            worked.payload.extend_from_slice(&want_bytes(*want));
            worked.payload.extend_from_slice(&cell.0);
            worked.payload.extend_from_slice(&u32::try_from(made.bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
            worked.payload.extend_from_slice(&made.bytes);
        }
        done.push(match failed {
            Some(why) => Err(HeadFailed { why, reads: worked.reads }),
            None => {
                worked.out = out.finish();
                Ok(worked)
            }
        });
    }
    Ok(done)
}

/// The most heads one head's freeze is followed through ([`heads_of`], rule 18): past it the
/// task fails by name.
const MOST_HEAD_READS: usize = 1 << 12;
